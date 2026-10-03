//! Verification spine for worker-produced diffs.
//!
//! Implements the layered verification pipeline described in
//! `01-architecture/failure-modes.md` Risk 1. Each layer is strictly more
//! expensive than the last; the orchestrator pays only for the cheapest
//! layer that catches the error.
//!
//! Layers:
//! * Layer 1 — `Syntax`: tree-sitter parse of the post-diff content.
//! * Layer 2 — `CrateCheck`: `cargo check --package <crate>` per touched crate.
//! * Layer 3 — `WorkspaceCheck`: `cargo check --workspace`.
//! * Layer 4 — `Test`: `cargo test --package <crate>`, 120s timeout.

pub mod browser;
mod executor;
pub use browser::verify_browser_check;

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use phonton_memory::MemoryStore;
use phonton_types::verification::VerificationExecution;
use phonton_types::{DiffHunk, DiffLine, MemoryRecord, VerifyLayer, VerifyResult};
use tree_sitter::{Node, Parser};

/// Run layered verification against `hunks`, with cargo commands executed
/// in `working_dir`.
///
/// Escalation order: [`verify_syntax`] → [`verify_crate_check`] →
/// [`verify_workspace_check`] → [`verify_test`]. A `Fail` from any layer
/// short-circuits the pipeline; subsequent (more expensive) layers only
/// run when every earlier layer passed.
///
/// This entry point is memory-unaware. To engage Layer 1.5
/// ([`verify_decisions`]) — which fails the diff if it violates a
/// recorded memory decision — call [`verify_diff_with_memory`] instead.
pub async fn verify_diff(hunks: &[DiffHunk], working_dir: &Path) -> Result<VerifyResult> {
    verify_diff_with_memory(hunks, working_dir, None).await
}

/// Memory-aware verification.
///
/// Identical to [`verify_diff`] except that when `memory` is `Some`, a
/// new **Layer 1.5 — Decision Check** runs between syntax and the cargo
/// layers. The check queries memory for `Decision`, `Constraint`,
/// `RejectedApproach`, and `Convention` records relevant to the touched
/// files, and rejects diffs that violate them — surfacing the offending
/// record's text verbatim as the error context. See [`verify_decisions`].
pub async fn verify_diff_with_memory(
    hunks: &[DiffHunk],
    working_dir: &Path,
    memory: Option<&MemoryStore>,
) -> Result<VerifyResult> {
    verify_diff_with_execution(
        hunks,
        working_dir,
        memory,
        VerificationExecution::RequireIsolation,
    )
    .await
}

/// Verify with explicit caller-owned execution authority. Host approval grants
/// no filesystem/network containment; default entry points never infer it.
pub async fn verify_diff_with_execution(
    hunks: &[DiffHunk],
    working_dir: &Path,
    memory: Option<&MemoryStore>,
    policy: VerificationExecution,
) -> Result<VerifyResult> {
    if let Some(fail) = verify_patch_applies(hunks, working_dir) {
        return Ok(fail);
    }
    let mut pass_layer = None;

    if let Some(fail) = verify_syntax_with_worktree(hunks, Some(working_dir)) {
        return Ok(fail);
    }

    if let Some(mem) = memory {
        if let Some(fail) = verify_decisions(hunks, mem).await? {
            return Ok(fail);
        }
    }

    if policy == VerificationExecution::RequireIsolation {
        return Ok(VerifyResult::Unavailable { reason: "Executable verification unavailable: no isolation backend is configured and host execution was not approved. Static checks alone cannot verify a candidate.".into() });
    }
    if hunks
        .iter()
        .any(|h| is_verification_definition_path(&h.file_path))
    {
        return Ok(VerifyResult::Unavailable { reason: "Verification definition changed in the candidate. Capture independent checks before executing this diff; candidate-authored tests or package scripts cannot authorize or certify themselves.".into() });
    }

    let packages = touched_packages(hunks, working_dir);
    let patched_worktree = if command_verification_relevant(hunks, working_dir) {
        Some(patched_verification_worktree(hunks, working_dir)?)
    } else {
        None
    };
    let command_dir = patched_worktree
        .as_ref()
        .map(|temp| temp.path())
        .unwrap_or(working_dir);

    if let Some(fail) = verify_crate_check_with_execution(&packages, command_dir, policy).await? {
        return Ok(fail);
    }

    if let Some(fail) = verify_workspace_check_with_execution(command_dir, policy).await? {
        return Ok(fail);
    }
    if find_cargo_workspace(command_dir).is_some() {
        pass_layer = Some(VerifyLayer::WorkspaceCheck);
    }

    if let Some(test_result) = verify_test_with_execution(&packages, command_dir, policy).await? {
        match test_result {
            VerifyResult::Pass { layer } => pass_layer = Some(layer),
            other => return Ok(other),
        }
    }

    if let Some(node_result) = verify_node_test_with_execution(command_dir, policy).await? {
        match node_result {
            VerifyResult::Fail { .. }
            | VerifyResult::Unavailable { .. }
            | VerifyResult::NotRun { .. }
            | VerifyResult::Escalate { .. } => return Ok(node_result),
            VerifyResult::Pass { layer } => pass_layer = Some(layer),
        }
    }

    if let Some(browser_result) =
        browser::verify_browser_check_with_execution(command_dir, policy).await?
    {
        match browser_result {
            VerifyResult::Fail { .. }
            | VerifyResult::Unavailable { .. }
            | VerifyResult::NotRun { .. }
            | VerifyResult::Escalate { .. } => {
                return Ok(browser_result);
            }
            VerifyResult::Pass { layer } => pass_layer = Some(layer),
        }
    }

    Ok(match pass_layer {
        Some(layer) => VerifyResult::Pass { layer },
        None => VerifyResult::NotRun { reason: "Verification not run: static patch/syntax checks found no error, but no executable verification was available. Select explicit checks before accepting the candidate.".into() },
    })
}

fn is_verification_definition_path(file_path: &Path) -> bool {
    let path = file_path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    let file = path.rsplit('/').next().unwrap_or_default();
    path.split('/')
        .any(|part| matches!(part, "tests" | "test" | "__tests__" | "spec" | "specs"))
        || file.starts_with("test_")
        || file.starts_with("test-")
        || file.starts_with("test.")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file.contains("_test.")
        || file.contains("-test.")
        || matches!(
            file,
            "package.json" | "cargo.toml" | "cargo.lock" | "package-lock.json"
        )
}

fn command_verification_relevant(hunks: &[DiffHunk], working_dir: &Path) -> bool {
    find_cargo_workspace(working_dir).is_some()
        || working_dir.join("package.json").is_file()
        || working_dir.join("index.html").is_file()
        || hunks.iter().any(|hunk| {
            let path = hunk.file_path.as_path();
            let name = path.file_name().and_then(|name| name.to_str());
            matches!(name, Some("Cargo.toml" | "package.json" | "index.html"))
                || matches!(
                    path.extension().and_then(|ext| ext.to_str()),
                    Some("rs" | "js" | "jsx" | "ts" | "tsx" | "html" | "css")
                )
        })
}

fn patched_verification_worktree(
    hunks: &[DiffHunk],
    working_dir: &Path,
) -> Result<tempfile::TempDir> {
    let temp = tempfile::tempdir()?;
    copy_workspace_contents(working_dir, temp.path())?;
    apply_hunks_to_worktree(hunks, working_dir, temp.path())?;
    Ok(temp)
}

fn copy_workspace_contents(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let src_path = entry.path();
        let dst_path = dest.join(&file_name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if should_skip_verification_copy_dir(&file_name) {
                continue;
            }
            copy_workspace_contents(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn should_skip_verification_copy_dir(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    matches!(
        name,
        ".git"
            | "target"
            | "node_modules"
            | ".next"
            | ".nuxt"
            | ".svelte-kit"
            | ".turbo"
            | ".vite"
            | "dist"
            | "coverage"
    )
}

fn apply_hunks_to_worktree(hunks: &[DiffHunk], source_root: &Path, dest_root: &Path) -> Result<()> {
    for (path, file_hunks) in group_hunks_by_file(hunks) {
        let Some(dest_path) = safe_join(dest_root, path) else {
            anyhow::bail!(
                "diff target path `{}` is absolute or escapes the workspace",
                path.display()
            );
        };
        let content = post_diff_source(source_root, path, &file_hunks)?;
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(dest_path, content)?;
    }
    Ok(())
}

fn safe_join(root: &Path, rel: &Path) -> Option<PathBuf> {
    if rel.is_absolute() {
        return None;
    }
    let mut out = root.to_path_buf();
    for component in rel.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => return None,
        }
    }
    Some(out)
}

/// Layer 0: prove each modified-file hunk can be located in the current
/// worktree before more expensive verification runs.
pub fn verify_patch_applies(hunks: &[DiffHunk], working_dir: &Path) -> Option<VerifyResult> {
    let allowed: Vec<_> = hunks.iter().map(|h| h.file_path.clone()).collect();
    phonton_local::edit::materialize_hunks_with_new_files(working_dir, &allowed, hunks)
        .err()
        .map(|error| VerifyResult::Fail {
            layer: VerifyLayer::PatchApply,
            errors: vec![error.to_string()],
            attempt: 1,
        })
}

/// Run `npm test` when the workspace is a Node package with an explicit
/// test script.
pub async fn verify_node_test(working_dir: &Path) -> Result<Option<VerifyResult>> {
    verify_node_test_with_execution(working_dir, VerificationExecution::RequireIsolation).await
}

/// Execute the package's test script only with explicit execution authority.
pub async fn verify_node_test_with_execution(
    working_dir: &Path,
    policy: VerificationExecution,
) -> Result<Option<VerifyResult>> {
    let package_json = working_dir.join("package.json");
    if !package_json_has_test_script(&package_json) {
        return Ok(None);
    }

    let output = executor::run(
        working_dir,
        npm_command(),
        vec!["test".into()],
        Duration::from_secs(120),
        policy,
    )
    .await;

    match output {
        Ok(out) if out.status.success() => Ok(Some(VerifyResult::Pass {
            layer: VerifyLayer::Test,
        })),
        Ok(out) => {
            let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
            combined.push_str(&String::from_utf8_lossy(&out.stderr));
            Ok(Some(VerifyResult::Fail {
                layer: VerifyLayer::Test,
                errors: vec![last_lines(&combined, 30)],
                attempt: 1,
            }))
        }
        Err(unavailable) => Ok(Some(unavailable)),
    }
}

fn npm_command() -> &'static str {
    if cfg!(windows) {
        "npm.cmd"
    } else {
        "npm"
    }
}

/// Layer 1: tree-sitter parse of each hunk's post-diff view.
pub fn verify_syntax(hunks: &[DiffHunk]) -> Option<VerifyResult> {
    verify_syntax_with_worktree(hunks, None)
}

fn verify_syntax_with_worktree(
    hunks: &[DiffHunk],
    working_dir: Option<&Path>,
) -> Option<VerifyResult> {
    let mut errors = Vec::new();
    for (path, file_hunks) in group_hunks_by_file(hunks) {
        let Some(language) = syntax_language_for_path(path) else {
            continue;
        };
        let snippet = match working_dir {
            Some(root) => match post_diff_source(root, path, &file_hunks) {
                Ok(source) => source,
                Err(e) => {
                    errors.push(format!(
                        "could not reconstruct post-diff file {} for syntax check: {e}",
                        path.display()
                    ));
                    continue;
                }
            },
            None => file_hunks
                .iter()
                .map(|hunk| reconstruct_new_side(hunk))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        let mut parser = Parser::new();
        if set_parser_language(&mut parser, language).is_err() {
            return Some(VerifyResult::Unavailable {
                reason: format!("failed to load {} grammar", language.label()),
            });
        }
        let Some(tree) = parser.parse(&snippet, None) else {
            errors.push(format!("tree-sitter could not parse {}", path.display()));
            continue;
        };
        if tree.root_node().has_error() || contains_error_node(tree.root_node()) {
            let detail = first_syntax_error_detail(&tree, &snippet);
            errors.push(format!(
                "syntax error in post-diff content targeting {}: {}",
                path.display(),
                detail
            ));
        } else if matches!(language, SyntaxLanguage::Python)
            && has_top_level_python_return(&snippet)
        {
            errors.push(format!(
                "syntax error in post-diff content targeting {}: top-level return statement",
                path.display()
            ));
        }
    }

    if errors.is_empty() {
        None
    } else {
        Some(VerifyResult::Fail {
            layer: VerifyLayer::Syntax,
            errors,
            attempt: 1,
        })
    }
}

#[derive(Clone, Copy)]
enum SyntaxLanguage {
    Rust,
    Python,
    TypeScript,
    Tsx,
}

impl SyntaxLanguage {
    fn label(self) -> &'static str {
        match self {
            Self::Rust => "tree-sitter-rust",
            Self::Python => "tree-sitter-python",
            Self::TypeScript => "tree-sitter-typescript",
            Self::Tsx => "tree-sitter-tsx",
        }
    }
}

fn set_parser_language(
    parser: &mut Parser,
    language: SyntaxLanguage,
) -> std::result::Result<(), tree_sitter::LanguageError> {
    match language {
        SyntaxLanguage::Rust => parser.set_language(&tree_sitter_rust::language()),
        SyntaxLanguage::Python => parser.set_language(&tree_sitter_python::language()),
        SyntaxLanguage::TypeScript => {
            parser.set_language(&tree_sitter_typescript::language_typescript())
        }
        SyntaxLanguage::Tsx => parser.set_language(&tree_sitter_typescript::language_tsx()),
    }
}

fn syntax_language_for_path(path: &Path) -> Option<SyntaxLanguage> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("rs") => Some(SyntaxLanguage::Rust),
        Some("py") => Some(SyntaxLanguage::Python),
        Some("ts") | Some("js") | Some("mjs") | Some("cjs") => Some(SyntaxLanguage::TypeScript),
        Some("tsx") | Some("jsx") => Some(SyntaxLanguage::Tsx),
        _ => None,
    }
}

fn has_top_level_python_return(source: &str) -> bool {
    source.lines().any(|line| {
        let trimmed = line.trim_start();
        !line.starts_with(char::is_whitespace)
            && (trimmed == "return" || trimmed.starts_with("return "))
    })
}

fn group_hunks_by_file(hunks: &[DiffHunk]) -> Vec<(&Path, Vec<&DiffHunk>)> {
    let mut by_file = std::collections::BTreeMap::<&Path, Vec<&DiffHunk>>::new();
    for hunk in hunks {
        by_file
            .entry(hunk.file_path.as_path())
            .or_default()
            .push(hunk);
    }
    by_file.into_iter().collect()
}

fn post_diff_source(root: &Path, path: &Path, hunks: &[&DiffHunk]) -> Result<String> {
    let owned: Vec<_> = hunks.iter().map(|h| (*h).clone()).collect();
    let mut changes =
        phonton_local::edit::materialize_hunks_with_new_files(root, &[path.to_path_buf()], &owned)?;
    changes
        .remove(path)
        .ok_or_else(|| anyhow::anyhow!("No materialized content for {}", path.display()))
}

/// Layer 1.5 — Decision Check.
///
/// Pulls candidate `MemoryRecord`s from `memory` (top-N by keyword
/// overlap with the touched file paths plus the added-line text), then
/// runs each record through [`record_violations`] to look for concrete
/// transgressions in the diff. If any are found, returns
/// [`VerifyResult::Fail`] at [`VerifyLayer::DecisionCheck`] with each
/// error string formatted as `"<record-kind>: <record-text> — violated
/// by: <evidence>"` so the worker (and the user) see exactly which
/// recorded decision was tripped.
///
/// Pure read against memory: this layer never writes back. A query
/// failure surfaces as `Escalate` (rather than `Fail`) so a flaky store
/// can't ground the worker — the orchestrator's escalation policy then
/// decides whether to retry or surface to the user.
///
/// The current rule set is intentionally narrow: matching well-known
/// "no panics", "no unwrap", "no `expect`", and "no blocking-in-async"
/// conventions, plus a generic "rejected approach summary appears as a
/// substring in the added lines" check. New rules belong here as the
/// memory schema gains structure; today the rule set is tuned to catch
/// the highest-frequency violations seen in practice.
pub async fn verify_decisions(
    hunks: &[DiffHunk],
    memory: &MemoryStore,
) -> Result<Option<VerifyResult>> {
    // Build the query from file paths + added lines so records pinned
    // to a specific crate or symbol surface first.
    let mut query = String::new();
    for hunk in hunks {
        query.push_str(&hunk.file_path.to_string_lossy());
        query.push(' ');
        for line in &hunk.lines {
            if let DiffLine::Added(s) = line {
                query.push_str(s);
                query.push(' ');
            }
        }
    }
    if query.trim().is_empty() {
        return Ok(None);
    }

    let records = match memory.query(&query, 16).await {
        Ok(r) => r,
        Err(e) => {
            return Ok(Some(VerifyResult::Unavailable {
                reason: format!("memory query failed: {e}"),
            }));
        }
    };

    let added_text = collected_added_text(hunks);
    let mut errors: Vec<String> = Vec::new();
    for rec in &records {
        for evidence in record_violations(rec, &added_text) {
            let provenance = match rec {
                MemoryRecord::Decision { task_id, body, .. } => {
                    let tid_str = task_id
                        .map(|id| format!(" (from task ID: {id})"))
                        .unwrap_or_default();
                    format!("Decision Detail: {body}{tid_str}")
                }
                MemoryRecord::Constraint { rationale, .. } => {
                    format!("Rationale: {rationale}")
                }
                MemoryRecord::Convention { scope, .. } => {
                    let scope_str = scope
                        .as_ref()
                        .map(|s| format!(" [scope: {s}]"))
                        .unwrap_or_default();
                    format!("Convention Rule{scope_str}")
                }
                MemoryRecord::RejectedApproach { reason, .. } => {
                    format!("Rejection Reason: {reason}")
                }
            };
            errors.push(format!(
                "{}: \"{}\" — violated by: {}. Provenance: {}",
                kind_label(rec),
                record_quote(rec),
                evidence,
                provenance
            ));
        }
    }

    if errors.is_empty() {
        Ok(None)
    } else {
        Ok(Some(VerifyResult::Fail {
            layer: VerifyLayer::DecisionCheck,
            errors,
            attempt: 1,
        }))
    }
}

/// Walk up from `start` looking for a `Cargo.toml`. Returns the directory
/// containing it (the workspace root) or `None` if none exists between
/// `start` and the filesystem root. Used to short-circuit the cargo
/// verification layers when the working directory isn't part of any Rust
/// project — without this, every goal in (say) a fresh empty folder
/// fails with "could not find Cargo.toml" before the worker can even
/// scaffold a project.
pub fn find_cargo_workspace(start: &Path) -> Option<std::path::PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join("Cargo.toml").is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Layer 2: `cargo check --package <crate> --message-format json` per
/// affected crate.
///
/// Parses compiler-message JSON lines and collects any whose `level` is
/// `"error"`. Warnings do not fail this layer. Returns `Ok(None)` when
/// every package check comes back clean.
pub async fn verify_crate_check(
    packages: &[String],
    working_dir: &Path,
) -> Result<Option<VerifyResult>> {
    verify_crate_check_with_execution(
        packages,
        working_dir,
        VerificationExecution::RequireIsolation,
    )
    .await
}

/// Check packages through the shared executor under explicit authority.
pub async fn verify_crate_check_with_execution(
    packages: &[String],
    working_dir: &Path,
    policy: VerificationExecution,
) -> Result<Option<VerifyResult>> {
    // Skip when we're not in a Rust workspace — `cargo check` would just
    // error with "could not find Cargo.toml" and turn every legitimate
    // create-a-new-project goal into a failure. Syntax (Layer 1) still
    // catches malformed Rust regardless of project shape.
    if find_cargo_workspace(working_dir).is_none() {
        return Ok(None);
    }
    let mut errors = Vec::new();
    for pkg in packages {
        let output = executor::run(
            working_dir,
            "cargo",
            vec![
                "check".into(),
                "--package".into(),
                pkg.clone(),
                "--message-format".into(),
                "json".into(),
            ],
            Duration::from_secs(120),
            policy,
        )
        .await;

        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                errors.extend(parse_cargo_errors(&stdout, pkg));
                if !out.status.success() && errors.is_empty() {
                    errors.push(format!(
                        "cargo check for {pkg} exited unsuccessfully: {}",
                        last_lines(&String::from_utf8_lossy(&out.stderr), 10)
                    ));
                }
            }
            Err(unavailable) => return Ok(Some(unavailable)),
        }
    }

    if errors.is_empty() {
        Ok(None)
    } else {
        Ok(Some(VerifyResult::Fail {
            layer: VerifyLayer::CrateCheck,
            errors,
            attempt: 1,
        }))
    }
}

/// Layer 3: `cargo check --workspace --message-format json`.
///
/// Only run by [`verify_diff`] after [`verify_crate_check`] passes, since
/// this scans every crate in the workspace and is the expensive cousin of
/// Layer 2.
pub async fn verify_workspace_check(working_dir: &Path) -> Result<Option<VerifyResult>> {
    verify_workspace_check_with_execution(working_dir, VerificationExecution::RequireIsolation)
        .await
}

/// Check a Cargo workspace through the shared executor under explicit authority.
pub async fn verify_workspace_check_with_execution(
    working_dir: &Path,
    policy: VerificationExecution,
) -> Result<Option<VerifyResult>> {
    // Same short-circuit as the crate check — no Cargo.toml means there's
    // nothing for cargo to verify. The previous behaviour was to surface
    // "cargo check --workspace failed: could not find `Cargo.toml`" as a
    // hard failure, which broke every project-bootstrap goal.
    if find_cargo_workspace(working_dir).is_none() {
        return Ok(None);
    }
    let output = executor::run(
        working_dir,
        "cargo",
        vec![
            "check".into(),
            "--workspace".into(),
            "--message-format".into(),
            "json".into(),
        ],
        Duration::from_secs(120),
        policy,
    )
    .await;

    let errors = match output {
        Ok(out) => {
            // Check both JSON compiler errors in stdout and the exit code.
            // A non-zero exit with no JSON (e.g. "no Cargo.toml found") would
            // previously pass silently; now we surface stderr as the error.
            let mut errs = parse_cargo_errors(&String::from_utf8_lossy(&out.stdout), "workspace");
            if !out.status.success() && errs.is_empty() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let msg = stderr.trim();
                if !msg.is_empty() {
                    errs.push(format!(
                        "cargo check --workspace failed: {}",
                        last_lines(msg, 5)
                    ));
                } else {
                    errs.push("cargo check --workspace exited unsuccessfully without compiler diagnostics".into());
                }
            }
            errs
        }
        Err(unavailable) => return Ok(Some(unavailable)),
    };

    if errors.is_empty() {
        Ok(None)
    } else {
        Ok(Some(VerifyResult::Fail {
            layer: VerifyLayer::WorkspaceCheck,
            errors,
            attempt: 1,
        }))
    }
}

/// Layer 4: `cargo test --package <crate>` per affected crate, capped at
/// 120s per invocation.
///
/// A non-zero exit surfaces the last 20 lines of combined stdout+stderr.
/// A successful process reports Test only when its output records a completed
/// passing test; empty or missing summaries remain NotRun.
pub async fn verify_test(packages: &[String], working_dir: &Path) -> Result<Option<VerifyResult>> {
    verify_test_with_execution(
        packages,
        working_dir,
        VerificationExecution::RequireIsolation,
    )
    .await
}

/// Run Cargo tests through the shared executor under explicit authority.
pub async fn verify_test_with_execution(
    packages: &[String],
    working_dir: &Path,
    policy: VerificationExecution,
) -> Result<Option<VerifyResult>> {
    // Skip for the same reason the cargo check layers do.
    if find_cargo_workspace(working_dir).is_none() || packages.is_empty() {
        return Ok(None);
    }
    let mut errors = Vec::new();
    let mut unavailable_packages = Vec::new();
    let mut no_test_packages = Vec::new();
    for pkg in packages {
        let output = executor::run(
            working_dir,
            "cargo",
            vec![
                "test".into(),
                "--package".into(),
                pkg.clone(),
                "--".into(),
                "--nocapture".into(),
            ],
            Duration::from_secs(120),
            policy,
        )
        .await;
        match output {
            Ok(out) if out.status.success() => {
                if !cargo_tests_completed(&String::from_utf8_lossy(&out.stdout)) {
                    no_test_packages.push(pkg.as_str());
                }
            }
            Ok(out) => {
                let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
                combined.push_str(&String::from_utf8_lossy(&out.stderr));
                errors.push(last_lines(&combined, 20));
            }
            Err(VerifyResult::Unavailable { reason }) => {
                unavailable_packages.push(format!("{pkg}: {reason}"));
            }
            Err(other) => return Ok(Some(other)),
        }
    }

    Ok(Some(cargo_test_verdict(
        errors,
        unavailable_packages,
        no_test_packages,
    )))
}

fn cargo_test_verdict(
    errors: Vec<String>,
    unavailable_packages: Vec<String>,
    no_test_packages: Vec<&str>,
) -> VerifyResult {
    if !errors.is_empty() {
        VerifyResult::Fail {
            layer: VerifyLayer::Test,
            errors,
            attempt: 1,
        }
    } else if !unavailable_packages.is_empty() {
        VerifyResult::Unavailable {
            reason: format!(
                "cargo test unavailable in: {}",
                unavailable_packages.join("; ")
            ),
        }
    } else if !no_test_packages.is_empty() {
        VerifyResult::NotRun {
            reason: format!(
                "cargo test exited successfully without a completed passing test in: {}",
                no_test_packages.join(", ")
            ),
        }
    } else {
        VerifyResult::Pass {
            layer: VerifyLayer::Test,
        }
    }
}

fn cargo_tests_completed(stdout: &str) -> bool {
    let mut saw_completed_test = false;
    for line in stdout.lines() {
        let Some(summary) = line.trim_start().strip_prefix("test result: ") else {
            continue;
        };
        let Some(counts) = summary.strip_prefix("ok. ") else {
            return false;
        };
        let Some((passed, remainder)) = counts.split_once(" passed; ") else {
            return false;
        };
        let Ok(passed) = passed.parse::<u64>() else {
            return false;
        };
        if !remainder.starts_with("0 failed;") {
            return false;
        }
        saw_completed_test |= passed > 0;
    }
    saw_completed_test
}

// ---------------------------------------------------------------------------
// Decision-check rule set
// ---------------------------------------------------------------------------

/// Concatenate every `Added` line across `hunks` into a single buffer
/// for substring scanning. Removed and context lines are excluded —
/// only what's *new in this diff* can violate a decision.
fn collected_added_text(hunks: &[DiffHunk]) -> String {
    let mut out = String::new();
    for hunk in hunks {
        for line in &hunk.lines {
            if let DiffLine::Added(s) = line {
                out.push_str(s);
                out.push('\n');
            }
        }
    }
    out
}

/// Apply the decision-check rule set against a single memory record and
/// return one evidence string per violation found in `added_text`.
///
/// The rules are partitioned by record kind:
///
/// * `Decision`/`Convention` — keyword-driven. We look for canonical
///   anti-patterns the record's text alludes to ("no panics", "no
///   unwrap", "thiserror not anyhow", "no blocking in async").
/// * `Constraint` — same keyword set, since constraints often phrase
///   the same rule from a different angle ("phonton-types stays
///   tokio-free").
/// * `RejectedApproach` — straight substring match against the
///   approach's `summary` (typed by humans, often verbatim quotable).
fn record_violations(rec: &MemoryRecord, added_text: &str) -> Vec<String> {
    let lower = added_text.to_ascii_lowercase();
    let mut hits: Vec<String> = Vec::new();

    let text = match rec {
        MemoryRecord::Decision { title, body, .. } => format!("{title} {body}"),
        MemoryRecord::Constraint {
            statement,
            rationale,
        } => format!("{statement} {rationale}"),
        MemoryRecord::Convention { rule, scope } => {
            format!("{} {}", rule, scope.as_deref().unwrap_or(""))
        }
        MemoryRecord::RejectedApproach { summary, reason } => format!("{summary} {reason}"),
    };
    let lc = text.to_ascii_lowercase();

    // Rule 1: "no panics" / "no unwrap" / "no expect".
    let bans_panic = lc.contains("no panic") || lc.contains("never panic");
    let bans_unwrap = lc.contains("no unwrap")
        || lc.contains("avoid unwrap")
        || lc.contains("never unwrap")
        || bans_panic;
    let bans_expect = lc.contains("no expect")
        || lc.contains("avoid expect")
        || lc.contains("never expect")
        || bans_panic;

    if bans_unwrap {
        for needle in [".unwrap()", ".unwrap("] {
            if lower.contains(needle) {
                hits.push(format!(
                    "added code contains `{}`",
                    needle.trim_end_matches('(')
                ));
            }
        }
    }
    if bans_expect && lower.contains(".expect(") {
        hits.push("added code contains `.expect(`".into());
    }
    if bans_panic && (lower.contains("panic!(") || lower.contains("panic !(")) {
        hits.push("added code contains `panic!`".into());
    }

    // Rule 2: "use thiserror in libraries / no anyhow in libraries".
    if ((lc.contains("thiserror") && lc.contains("anyhow"))
        || lc.contains("no anyhow in lib")
        || lc.contains("avoid anyhow in lib"))
        && (lower.contains("anyhow::") || lower.contains("use anyhow"))
    {
        hits.push("added code uses `anyhow` where the convention is `thiserror`".into());
    }

    // Rule 3: "no blocking in async".
    if lc.contains("no blocking") || lc.contains("avoid blocking") || lc.contains("blocking call") {
        for needle in ["std::thread::sleep", "std::fs::read", "std::fs::write"] {
            if lower.contains(needle) {
                hits.push(format!(
                    "added code calls blocking `{needle}` (convention forbids)"
                ));
            }
        }
    }

    // Rule 4: rejected-approach substring match.
    if let MemoryRecord::RejectedApproach { summary, .. } = rec {
        let needle = summary.to_ascii_lowercase();
        // Only fire on summaries with enough signal to be meaningful —
        // a 2-char summary would substring-match nearly any diff.
        if needle.trim().len() >= 6 && lower.contains(needle.trim()) {
            hits.push(format!(
                "added code contains the rejected-approach phrase `{}`",
                summary
            ));
        }
    }

    hits
}

fn kind_label(r: &MemoryRecord) -> &'static str {
    match r {
        MemoryRecord::Decision { .. } => "decision",
        MemoryRecord::Constraint { .. } => "constraint",
        MemoryRecord::Convention { .. } => "convention",
        MemoryRecord::RejectedApproach { .. } => "rejected-approach",
    }
}

fn record_quote(r: &MemoryRecord) -> String {
    match r {
        MemoryRecord::Decision { title, .. } => title.clone(),
        MemoryRecord::Constraint { statement, .. } => statement.clone(),
        MemoryRecord::Convention { rule, .. } => rule.clone(),
        MemoryRecord::RejectedApproach { summary, .. } => summary.clone(),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn package_json_has_test_script(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    value
        .get("scripts")
        .and_then(|scripts| scripts.get("test"))
        .and_then(|script| script.as_str())
        .map(|script| !script.trim().is_empty())
        .unwrap_or(false)
}

fn reconstruct_new_side(hunk: &DiffHunk) -> String {
    let mut out = String::new();
    for line in &hunk.lines {
        match line {
            DiffLine::Context(s) | DiffLine::Added(s) => {
                out.push_str(s);
                if !s.ends_with('\n') {
                    out.push('\n');
                }
            }
            DiffLine::Removed(_) => {}
        }
    }
    out
}

fn contains_error_node(node: Node<'_>) -> bool {
    if node.is_error() || node.is_missing() {
        return true;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if contains_error_node(child) {
            return true;
        }
    }
    false
}

fn first_syntax_error_detail(tree: &tree_sitter::Tree, source: &str) -> String {
    fn walk(node: Node<'_>, source: &str) -> Option<String> {
        if node.is_error() || node.is_missing() {
            let row = node.start_position().row + 1;
            let col = node.start_position().column + 1;
            let line = source
                .lines()
                .nth(node.start_position().row)
                .unwrap_or("")
                .trim();
            if line.is_empty() {
                return Some(format!("line {row} col {col}"));
            }
            return Some(format!("line {row} col {col}: {line}"));
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(detail) = walk(child, source) {
                return Some(detail);
            }
        }
        None
    }
    walk(tree.root_node(), source).unwrap_or_else(|| "parse tree contains errors".into())
}

/// Resolve the nearest Cargo package from the target workspace, not the
/// Phonton process's current directory.
fn crate_name_for(path: &Path, working_dir: &Path) -> Option<String> {
    let relative = if path.is_absolute() {
        path.strip_prefix(working_dir).ok()?
    } else {
        path
    };
    if !relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return None;
    }

    let mut dir = working_dir.join(relative).parent()?.to_path_buf();
    loop {
        if let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) {
            if let Ok(manifest) = toml::from_str::<toml::Value>(&text) {
                if let Some(name) = manifest
                    .get("package")
                    .and_then(|package| package.get("name"))
                    .and_then(toml::Value::as_str)
                    .filter(|name| !name.is_empty())
                {
                    return Some(name.to_string());
                }
            }
        }
        if dir == working_dir {
            return None;
        }
        dir = dir.parent()?.to_path_buf();
        if !dir.starts_with(working_dir) {
            return None;
        }
    }
}

fn touched_packages(hunks: &[DiffHunk], working_dir: &Path) -> Vec<String> {
    let mut packages: Vec<String> = Vec::new();
    for hunk in hunks {
        if let Some(pkg) = crate_name_for(&hunk.file_path, working_dir) {
            if !packages.iter().any(|p| p == &pkg) {
                packages.push(pkg);
            }
        }
    }
    packages
}

/// Parse `cargo --message-format json` stdout and collect compiler errors.
///
/// Each non-empty line is expected to be a JSON object. Lines that don't
/// parse are ignored (cargo also emits non-JSON lines on stderr; we read
/// stdout where the contract holds). We collect entries whose
/// `reason == "compiler-message"` and whose `message.level == "error"`,
/// returning the `rendered` field when present, else `message.message`.
fn parse_cargo_errors(stdout: &str, label: &str) -> Vec<String> {
    let mut errors = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if val.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = val.get("message") else {
            continue;
        };
        if msg.get("level").and_then(|l| l.as_str()) != Some("error") {
            continue;
        }
        let text = msg
            .get("rendered")
            .and_then(|r| r.as_str())
            .or_else(|| msg.get("message").and_then(|m| m.as_str()))
            .unwrap_or("<unrendered compiler error>");
        errors.push(format!("[{label}] {text}"));
    }
    errors
}

fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_test_evidence_requires_completed_nonempty_suite() {
        assert!(!cargo_tests_completed(""));
        assert!(!cargo_tests_completed("test result: ok.\n"));
        assert!(!cargo_tests_completed(
            "test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured\n"
        ));
        assert!(cargo_tests_completed(
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured\n"
        ));
        assert!(cargo_tests_completed(
            "test result: ok. 0 passed; 0 failed; 0 ignored\ntest result: ok. 2 passed; 0 failed; 0 ignored\n"
        ));
    }

    #[test]
    fn an_observed_test_failure_takes_priority_over_later_unavailability() {
        assert!(matches!(
            cargo_test_verdict(
                vec!["assertion failed".into()],
                vec!["later package timed out".into()],
                vec!["empty"],
            ),
            VerifyResult::Fail {
                layer: VerifyLayer::Test,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn default_verification_never_runs_project_scripts_or_writes_browser_helpers() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"scripts":{"test":"node -e \"require('fs').writeFileSync('executed','bad')\""}}"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join("index.html"), "<h1>fixture</h1>").unwrap();
        std::fs::write(tmp.path().join("phonton-server.js"), "user-owned content").unwrap();
        assert!(matches!(
            verify_node_test(tmp.path()).await.unwrap(),
            Some(VerifyResult::Unavailable { .. })
        ));
        assert!(matches!(
            verify_browser_check(tmp.path()).await.unwrap(),
            Some(VerifyResult::Unavailable { .. })
        ));
        assert!(!tmp.path().join("executed").exists());
        assert!(!tmp.path().join("phonton-playwright.js").exists());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("phonton-server.js")).unwrap(),
            "user-owned content"
        );
    }

    #[tokio::test]
    async fn static_checks_without_executable_evidence_never_pass_a_diff() {
        let tmp = tempfile::tempdir().unwrap();
        for policy in [
            VerificationExecution::RequireIsolation,
            VerificationExecution::HostApproved,
        ] {
            let result = verify_diff_with_execution(
                &[hunk("note.txt", vec![DiffLine::Added("plain text".into())])],
                tmp.path(),
                None,
                policy,
            )
            .await
            .unwrap();
            match policy {
                VerificationExecution::RequireIsolation => {
                    assert!(matches!(result, VerifyResult::Unavailable { .. }))
                }
                VerificationExecution::HostApproved => {
                    assert!(matches!(result, VerifyResult::NotRun { .. }))
                }
            }
        }
    }

    #[tokio::test]
    async fn candidate_cannot_replace_the_script_that_certifies_it() {
        let tmp = tempfile::tempdir().unwrap();
        let old = r#"{"scripts":{"test":"node tests.js"}}"#;
        std::fs::write(tmp.path().join("package.json"), format!("{old}\n")).unwrap();
        let change = DiffHunk {
            file_path: "package.json".into(),
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![
                DiffLine::Removed(old.into()),
                DiffLine::Added(r#"{"scripts":{"test":"node -e process.exit(0)"}}"#.into()),
            ],
        };
        let result = verify_diff_with_execution(
            &[change],
            tmp.path(),
            None,
            VerificationExecution::HostApproved,
        )
        .await
        .unwrap();
        assert!(
            matches!(result, VerifyResult::Unavailable { reason } if reason.contains("definition changed"))
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("package.json")).unwrap(),
            format!("{old}\n")
        );
    }

    #[test]
    fn verification_definitions_cover_case_aliases_and_node_test_names() {
        for path in [
            "Package.json",
            "CARGO.TOML",
            "package-lock.JSON",
            "foo_test.js",
            "foo-test.js",
            "test-foo.js",
            "test.js",
            "src\\Specs\\math.ts",
        ] {
            assert!(
                is_verification_definition_path(Path::new(path)),
                "missed {path}"
            );
        }
        assert!(!is_verification_definition_path(Path::new("src/math.js")));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn mixed_case_manifest_alias_cannot_certify_its_own_script() {
        let tmp = tempfile::tempdir().unwrap();
        let old = r#"{"scripts":{"test":"node --test"}}"#;
        std::fs::write(tmp.path().join("package.json"), format!("{old}\n")).unwrap();
        let change = DiffHunk {
            file_path: "Package.json".into(),
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![
                DiffLine::Removed(old.into()),
                DiffLine::Added(r#"{"scripts":{"test":"node -e process.exit(0)"}}"#.into()),
            ],
        };
        let result = verify_diff_with_execution(
            &[change],
            tmp.path(),
            None,
            VerificationExecution::HostApproved,
        )
        .await
        .unwrap();
        assert!(
            matches!(result, VerifyResult::Unavailable { reason } if reason.contains("definition changed"))
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("package.json")).unwrap(),
            format!("{old}\n")
        );
    }

    #[tokio::test]
    async fn cargo_default_fails_closed_and_nonzero_without_json_is_not_success() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        let packages = vec!["missing-package".into()];
        assert!(matches!(
            verify_crate_check(&packages, tmp.path()).await.unwrap(),
            Some(VerifyResult::Unavailable { .. })
        ));
        assert!(matches!(
            verify_workspace_check(tmp.path()).await.unwrap(),
            Some(VerifyResult::Unavailable { .. })
        ));
        assert!(matches!(
            verify_test(&packages, tmp.path()).await.unwrap(),
            Some(VerifyResult::Unavailable { .. })
        ));
        assert!(matches!(
            verify_crate_check_with_execution(
                &packages,
                tmp.path(),
                VerificationExecution::HostApproved
            )
            .await
            .unwrap(),
            Some(VerifyResult::Fail { .. })
        ));
    }
    use std::path::PathBuf;

    fn hunk(path: &str, lines: Vec<DiffLine>) -> DiffHunk {
        let old_count = lines
            .iter()
            .filter(|line| !matches!(line, DiffLine::Added(_)))
            .count() as u32;
        let new_count = lines
            .iter()
            .filter(|line| !matches!(line, DiffLine::Removed(_)))
            .count() as u32;
        DiffHunk {
            file_path: PathBuf::from(path),
            old_start: if old_count == 0 { 0 } else { 1 },
            old_count,
            new_start: 1,
            new_count,
            lines,
        }
    }

    #[test]
    fn syntax_pass_on_valid_rust() {
        let h = hunk(
            "phonton-types/src/foo.rs",
            vec![DiffLine::Added("fn ok() -> u32 { 42 }".into())],
        );
        assert!(verify_syntax(&[h]).is_none());
    }

    #[test]
    fn syntax_fail_on_broken_rust() {
        let h = hunk(
            "phonton-types/src/foo.rs",
            vec![DiffLine::Added("fn broken( -> {".into())],
        );
        match verify_syntax(&[h]) {
            Some(VerifyResult::Fail {
                layer: VerifyLayer::Syntax,
                ..
            }) => {}
            other => panic!("expected syntax fail, got {other:?}"),
        }
    }

    #[test]
    fn syntax_fail_on_broken_python() {
        let h = hunk(
            "broken_code.py",
            vec![
                DiffLine::Added("def calculate_sum(a, b):".into()),
                DiffLine::Added("    result = a + b".into()),
                DiffLine::Added("return result".into()),
            ],
        );

        match verify_syntax(&[h]) {
            Some(VerifyResult::Fail {
                layer: VerifyLayer::Syntax,
                ..
            }) => {}
            other => panic!("expected Python syntax fail, got {other:?}"),
        }
    }

    #[test]
    fn syntax_fail_on_broken_typescript() {
        let h = hunk(
            "broken_code.ts",
            vec![
                DiffLine::Added("function getUserInfo(userId: string) {".into()),
                DiffLine::Added("  return {".into()),
                DiffLine::Added("    id: userId,".into()),
                DiffLine::Added("    name: \"Test User\"".into()),
            ],
        );

        match verify_syntax(&[h]) {
            Some(VerifyResult::Fail {
                layer: VerifyLayer::Syntax,
                ..
            }) => {}
            other => panic!("expected TypeScript syntax fail, got {other:?}"),
        }
    }

    #[test]
    fn patch_apply_fails_when_context_does_not_match_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/config.js"),
            "export function loadConfig() {\n  return {};\n}\n",
        )
        .unwrap();
        let h = hunk(
            "src/config.js",
            vec![
                DiffLine::Context("function loadSettings() {".into()),
                DiffLine::Context("  return {};".into()),
                DiffLine::Context("}".into()),
            ],
        );

        match verify_patch_applies(&[h], tmp.path()) {
            Some(VerifyResult::Fail {
                layer: VerifyLayer::PatchApply,
                errors,
                ..
            }) => {
                assert!(errors.join("\n").contains("Exact hunk verification failed"));
                assert!(errors.join("\n").contains("config.js"));
            }
            other => panic!("expected PatchApply failure, got {other:?}"),
        }
    }

    #[test]
    fn patch_apply_passes_when_context_matches_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/config.js"),
            "export function loadConfig() {\n  return {};\n}\n",
        )
        .unwrap();
        let h = hunk(
            "src/config.js",
            vec![
                DiffLine::Context("export function loadConfig() {".into()),
                DiffLine::Removed("  return {};".into()),
                DiffLine::Added("  return { ok: true };".into()),
                DiffLine::Context("}".into()),
            ],
        );

        assert!(verify_patch_applies(&[h], tmp.path()).is_none());
    }

    #[test]
    fn patch_apply_rejects_export_prefix_context_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/config.js"),
            "export function loadConfig() {\n  return {};\n}\n",
        )
        .unwrap();
        let h = hunk(
            "src/config.js",
            vec![
                DiffLine::Context("function loadConfig() {".into()),
                DiffLine::Removed("  return {};".into()),
                DiffLine::Added("  return { ok: true };".into()),
                DiffLine::Context("}".into()),
            ],
        );

        assert!(matches!(
            verify_patch_applies(&[h], tmp.path()),
            Some(VerifyResult::Fail {
                layer: VerifyLayer::PatchApply,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn node_test_fails_plain_node_package_when_npm_test_fails() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"type":"module","scripts":{"test":"node --test --test-reporter=tap"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("test")).unwrap();
        std::fs::write(
            tmp.path().join("test").join("fail.test.js"),
            "import test from 'node:test';\nimport assert from 'node:assert/strict';\ntest('fails', () => assert.equal(1, 2));\n",
        )
        .unwrap();

        match verify_node_test_with_execution(tmp.path(), VerificationExecution::HostApproved)
            .await
            .unwrap()
        {
            Some(VerifyResult::Fail {
                layer: VerifyLayer::Test,
                errors,
                ..
            }) => {
                // Reporter defaults vary by Node version. Verify the retained
                // TAP failure evidence, not a test name outside the output tail.
                let diagnostic = errors.join("\n");
                for evidence in ["ERR_ASSERTION", "expected: 2", "actual: 1", "# fail 1"] {
                    assert!(
                        diagnostic.contains(evidence),
                        "missing {evidence:?} in npm test failure: {diagnostic}"
                    );
                }
            }
            other => panic!("expected npm test failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn node_test_passes_plain_node_package_when_npm_test_passes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"type":"module","scripts":{"test":"node --test"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("test")).unwrap();
        std::fs::write(
            tmp.path().join("test").join("pass.test.js"),
            "import test from 'node:test';\nimport assert from 'node:assert/strict';\ntest('passes', () => assert.equal(1, 1));\n",
        )
        .unwrap();

        assert_eq!(
            verify_node_test_with_execution(tmp.path(), VerificationExecution::HostApproved)
                .await
                .unwrap(),
            Some(VerifyResult::Pass {
                layer: VerifyLayer::Test
            })
        );
    }

    #[tokio::test]
    async fn verify_diff_runs_node_tests_against_candidate_diff() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"type":"module","scripts":{"test":"node --test"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::create_dir_all(tmp.path().join("test")).unwrap();
        std::fs::write(
            tmp.path().join("src").join("config.js"),
            "export const value = 1;\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("test").join("config.test.js"),
            "import test from 'node:test';\nimport assert from 'node:assert/strict';\nimport { value } from '../src/config.js';\ntest('uses patched value', () => assert.equal(value, 2));\n",
        )
        .unwrap();

        let h = DiffHunk {
            file_path: PathBuf::from("src/config.js"),
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![
                DiffLine::Removed("export const value = 1;".into()),
                DiffLine::Added("export const value = 2;".into()),
            ],
        };

        assert_eq!(
            verify_diff_with_execution(&[h], tmp.path(), None, VerificationExecution::HostApproved)
                .await
                .unwrap(),
            VerifyResult::Pass {
                layer: VerifyLayer::Test
            }
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("src").join("config.js")).unwrap(),
            "export const value = 1;\n"
        );
    }

    #[test]
    fn parse_cargo_errors_extracts_error_level_messages() {
        let stdout = concat!(
            r#"{"reason":"compiler-message","message":{"level":"warning","message":"unused","rendered":"warn: unused"}}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","rendered":"error: mismatched types"}}"#,
            "\n",
            r#"{"reason":"compiler-artifact"}"#,
            "\n",
        );
        let errs = parse_cargo_errors(stdout, "pkg");
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("mismatched types"));
    }

    // -------------------------------------------------------------
    // Decision Check (Layer 1.5)
    // -------------------------------------------------------------

    use phonton_memory::MemoryStore;
    use phonton_store::Store;
    use phonton_types::MemoryRecord;
    use std::sync::{Arc, Mutex};

    async fn fresh_memory() -> MemoryStore {
        let store = Store::in_memory().expect("open in-memory store");
        // MemoryStore::new takes Arc<Mutex<Store>>; mirror what the
        // memory crate's own tests do.
        let s = Arc::new(Mutex::new(store));
        MemoryStore::new(s).await
    }

    #[tokio::test]
    async fn decision_check_flags_unwrap_under_no_panics_decision() {
        let mem = fresh_memory().await;
        mem.record(MemoryRecord::Decision {
            title: "No panics in library code".into(),
            body: "Never use unwrap or expect in phonton-* libraries; \
                   propagate errors with `?`."
                .into(),
            task_id: None,
        })
        .await
        .unwrap();

        let h = hunk(
            "phonton-types/src/foo.rs",
            vec![DiffLine::Added("let v = some_call().unwrap();".into())],
        );
        let res = verify_decisions(&[h], &mem).await.unwrap();
        match res {
            Some(VerifyResult::Fail {
                layer: VerifyLayer::DecisionCheck,
                errors,
                ..
            }) => {
                assert!(!errors.is_empty(), "expected at least one violation");
                let joined = errors.join(" | ");
                assert!(
                    joined.contains("No panics") || joined.contains("decision:"),
                    "error must quote the decision text: {joined}"
                );
                assert!(
                    joined.contains(".unwrap"),
                    "error must cite the offending construct: {joined}"
                );
            }
            other => panic!("expected DecisionCheck Fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn decision_check_passes_when_diff_respects_rule() {
        let mem = fresh_memory().await;
        mem.record(MemoryRecord::Convention {
            rule: "no unwrap in libraries".into(),
            scope: Some("phonton-*".into()),
        })
        .await
        .unwrap();

        let h = hunk(
            "phonton-types/src/foo.rs",
            vec![DiffLine::Added("let v = some_call()?;".into())],
        );
        let res = verify_decisions(&[h], &mem).await.unwrap();
        assert!(
            res.is_none(),
            "diff with `?` propagation should pass; got {res:?}"
        );
    }

    #[tokio::test]
    async fn decision_check_flags_rejected_approach_substring() {
        let mem = fresh_memory().await;
        mem.record(MemoryRecord::RejectedApproach {
            summary: "global Arc<RwLock> context manager".into(),
            reason: "lock contention under parallel workers".into(),
        })
        .await
        .unwrap();

        let h = hunk(
            "phonton-context/src/lib.rs",
            vec![DiffLine::Added(
                "static CTX: Lazy<global Arc<RwLock> context manager> = ...;".into(),
            )],
        );
        let res = verify_decisions(&[h], &mem).await.unwrap();
        match res {
            Some(VerifyResult::Fail {
                layer: VerifyLayer::DecisionCheck,
                errors,
                ..
            }) => {
                assert!(errors.iter().any(|e| e.contains("rejected-approach")));
            }
            other => panic!("expected DecisionCheck Fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn decision_check_skipped_when_memory_empty() {
        let mem = fresh_memory().await;
        let h = hunk(
            "phonton-types/src/foo.rs",
            vec![DiffLine::Added("let v = bar.unwrap();".into())],
        );
        let res = verify_decisions(&[h], &mem).await.unwrap();
        assert!(res.is_none(), "no records → no violations");
    }

    #[test]
    fn last_lines_returns_tail() {
        let s = "a\nb\nc\nd\ne";
        assert_eq!(last_lines(s, 2), "d\ne");
        assert_eq!(last_lines(s, 100), "a\nb\nc\nd\ne");
    }

    /// Regression: every cargo-based verify layer must be a no-op when
    /// the working directory has no `Cargo.toml`. Without this guard,
    /// running phonton in a fresh empty folder ("make chess") fails on
    /// the first verify pass with `could not find Cargo.toml in <dir>`,
    /// rolling back the diff that was about to scaffold the project.
    #[tokio::test]
    async fn cargo_layers_skip_when_no_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        assert!(
            super::find_cargo_workspace(dir).is_none(),
            "tempdir must not contain a Cargo.toml"
        );
        // All three cargo layers must short-circuit to Ok(None) rather
        // than invoking cargo and surfacing "could not find Cargo.toml".
        let crate_check = super::verify_crate_check(&["any".into()], dir)
            .await
            .unwrap();
        assert!(crate_check.is_none(), "crate_check must skip");
        let workspace_check = super::verify_workspace_check(dir).await.unwrap();
        assert!(workspace_check.is_none(), "workspace_check must skip");
        let test = super::verify_test(&["any".into()], dir).await.unwrap();
        assert!(test.is_none(), "test layer must skip");
    }

    /// Counterpart: when a Cargo.toml *is* present, find_cargo_workspace
    /// returns the directory containing it (so the cargo layers will
    /// actually run as before).
    #[test]
    fn finds_workspace_walking_up() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        let nested = root.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        let found = super::find_cargo_workspace(&nested).expect("should find Cargo.toml");
        assert_eq!(found, root);
    }

    #[tokio::test]
    async fn browser_check_passes_on_valid_html() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let html_content = "<html><head><title>Test</title></head><body><h1>Hello</h1><button>Click me</button></body></html>";
        std::fs::write(dir.join("index.html"), html_content).unwrap();

        let res = super::browser::verify_browser_check_with_execution(
            dir,
            VerificationExecution::HostApproved,
        )
        .await
        .unwrap();
        match res {
            Some(VerifyResult::Pass {
                layer: VerifyLayer::BrowserCheck,
            }) => {}
            other => panic!("expected browser check pass, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn browser_check_skips_plain_node_packages() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join("package.json"),
            r#"{"type":"module","scripts":{"test":"node --test"}}"#,
        )
        .unwrap();

        let res = super::verify_browser_check(dir).await.unwrap();

        assert!(
            res.is_none(),
            "plain Node packages should use test/syntax layers, not browser checks"
        );
    }
}
