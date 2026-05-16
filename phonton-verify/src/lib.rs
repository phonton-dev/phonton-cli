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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use phonton_memory::MemoryStore;
use phonton_types::{DiffHunk, DiffLine, MemoryRecord, VerifyLayer, VerifyResult};
use tokio::process::Command;
use tree_sitter::{Language, Node, Parser};

/// Browser/runtime verification request for a generated web artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserRuntimeSpec {
    /// HTML artifact path, relative to the verified workspace.
    pub artifact_path: PathBuf,
    /// CSS selectors that must exist after the page loads.
    pub required_selectors: Vec<String>,
}

/// Run optional Playwright-based browser verification for a generated web artifact.
///
/// If Playwright is not installed in the local Node environment, this returns
/// `Escalate` so generated web work remains explicitly unverified.
pub async fn verify_browser_runtime(
    working_dir: &Path,
    spec: &BrowserRuntimeSpec,
) -> Result<VerifyResult> {
    let artifact = working_dir.join(&spec.artifact_path);
    if !artifact.is_file() {
        return Ok(VerifyResult::Fail {
            layer: VerifyLayer::RuntimeSmoke,
            errors: vec![format!(
                "runtime artifact not found: {}",
                spec.artifact_path.display()
            )],
            attempt: 1,
        });
    }

    if !playwright_available(working_dir).await {
        return Ok(VerifyResult::Escalate {
            reason: "browser runtime verification unavailable: install Playwright for this workspace to verify generated web artifacts".into(),
        });
    }

    let script = browser_verify_script(&artifact, &spec.required_selectors)?;
    let output = Command::new("node")
        .arg("-e")
        .arg(script)
        .current_dir(working_dir)
        .output()
        .await?;
    if output.status.success() {
        return Ok(VerifyResult::Pass {
            layer: VerifyLayer::InteractionCheck,
        });
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if stderr.is_empty() { stdout } else { stderr };
    Ok(VerifyResult::Fail {
        layer: VerifyLayer::BrowserDomCheck,
        errors: vec![detail],
        attempt: 1,
    })
}

async fn playwright_available(working_dir: &Path) -> bool {
    Command::new("node")
        .arg("-e")
        .arg("require.resolve('playwright')")
        .current_dir(working_dir)
        .output()
        .await
        .is_ok_and(|output| output.status.success())
}

fn browser_verify_script(artifact: &Path, selectors: &[String]) -> Result<String> {
    let artifact = artifact.canonicalize()?;
    let artifact_url = format!("file:///{}", artifact.to_string_lossy().replace('\\', "/"));
    let artifact_json = serde_json::to_string(&artifact_url)?;
    let selectors_json = serde_json::to_string(selectors)?;
    Ok(format!(
        r#"
const {{ chromium }} = require('playwright');
const artifactUrl = {artifact_json};
const selectors = {selectors_json};
(async () => {{
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const consoleErrors = [];
  page.on('console', msg => {{
    if (msg.type() === 'error') consoleErrors.push(msg.text());
  }});
  await page.goto(artifactUrl);
  for (const selector of selectors) {{
    await page.waitForSelector(selector, {{ timeout: 1500 }});
  }}
  if (consoleErrors.length) {{
    throw new Error('console errors: ' + consoleErrors.join('; '));
  }}
  await browser.close();
}})().catch(async error => {{
  console.error(error.message || String(error));
  process.exit(1);
}});
"#
    ))
}

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
    if let Some(fail) = verify_syntax_in_workspace(hunks, working_dir) {
        return Ok(fail);
    }

    if let Some(mem) = memory {
        if let Some(fail) = verify_decisions(hunks, mem).await? {
            return Ok(fail);
        }
    }

    if let Some(fail) = verify_node_project(hunks, working_dir).await? {
        return Ok(fail);
    }

    let packages = touched_packages(hunks);

    if let Some(fail) = verify_crate_check(&packages, working_dir).await? {
        return Ok(fail);
    }

    if let Some(fail) = verify_workspace_check(working_dir).await? {
        return Ok(fail);
    }

    if let Some(fail) = verify_test(&packages, working_dir).await? {
        return Ok(fail);
    }

    Ok(VerifyResult::Pass {
        layer: VerifyLayer::Test,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyntaxLanguage {
    Rust,
    Python,
    JavaScript,
    Jsx,
    TypeScript,
    Tsx,
    Json,
    Toml,
    Yaml,
    Html,
    Css,
}

impl SyntaxLanguage {
    fn label(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript | Self::Jsx => "javascript",
            Self::TypeScript | Self::Tsx => "typescript",
            Self::Json => "json",
            Self::Toml => "toml",
            Self::Yaml => "yaml",
            Self::Html => "html",
            Self::Css => "css",
        }
    }

    fn tree_sitter_language(self) -> Option<Language> {
        match self {
            Self::Rust => Some(tree_sitter_rust::language()),
            Self::Python => Some(tree_sitter_python::language()),
            Self::JavaScript => Some(tree_sitter_typescript::language_typescript()),
            Self::Jsx => Some(tree_sitter_typescript::language_tsx()),
            Self::TypeScript => Some(tree_sitter_typescript::language_typescript()),
            Self::Tsx => Some(tree_sitter_typescript::language_tsx()),
            Self::Html => Some(tree_sitter_html::language()),
            Self::Json | Self::Toml | Self::Yaml | Self::Css => None,
        }
    }
}

/// Layer 1: syntax parse of each supported hunk's post-diff view.
pub fn verify_syntax(hunks: &[DiffHunk]) -> Option<VerifyResult> {
    verify_syntax_for_sources(hunks, None, None)
}

/// Layer 1 with workspace-aware post-diff reconstruction.
///
/// Generated files are parsed as full files. Existing files are rebuilt from
/// the current workspace content plus the proposed hunks before parsing.
pub fn verify_syntax_in_workspace(hunks: &[DiffHunk], working_dir: &Path) -> Option<VerifyResult> {
    verify_syntax_for_sources(hunks, Some(working_dir), None)
}

/// Layer 1b: tree-sitter parse for whole-file generated Python hunks.
///
/// The Rust syntax verifier above intentionally ignores non-Rust files, and
/// cargo layers are skipped outside a Rust workspace. That combination let a
/// generated `chess.py` with an unterminated string reach review-ready status.
///
/// We only parse whole-file Python hunks here. Partial hunks usually do not
/// contain enough context to parse as a complete module, so failing them would
/// create false negatives for normal edits.
pub fn verify_python_syntax(hunks: &[DiffHunk]) -> Option<VerifyResult> {
    verify_syntax_for_sources(hunks, None, Some(SyntaxLanguage::Python))
}

fn verify_syntax_for_sources(
    hunks: &[DiffHunk],
    working_dir: Option<&Path>,
    only_language: Option<SyntaxLanguage>,
) -> Option<VerifyResult> {
    let mut errors = Vec::new();
    for (path, grouped_hunks) in group_supported_hunks(hunks, only_language) {
        let Some(language) = syntax_language_for_path(&path) else {
            continue;
        };
        if only_language.is_some_and(|only| only != language) {
            continue;
        }
        if working_dir.is_none() && !grouped_hunks.iter().any(is_new_file_hunk) {
            continue;
        }
        let source = match reconstruct_post_diff_source(&path, &grouped_hunks, working_dir) {
            Ok(source) => source,
            Err(reason) => {
                errors.push(format!(
                    "[{} syntax] {}: could not reconstruct post-diff file: {}",
                    language.label(),
                    path.display(),
                    reason
                ));
                continue;
            }
        };
        if let Some(error) = parse_source(language, &path, &source) {
            errors.push(error);
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

fn group_supported_hunks(
    hunks: &[DiffHunk],
    only_language: Option<SyntaxLanguage>,
) -> BTreeMap<PathBuf, Vec<DiffHunk>> {
    let mut grouped = BTreeMap::new();
    for hunk in hunks {
        let Some(language) = syntax_language_for_path(&hunk.file_path) else {
            continue;
        };
        if only_language.is_some_and(|only| only != language) {
            continue;
        }
        grouped
            .entry(hunk.file_path.clone())
            .or_insert_with(Vec::new)
            .push(hunk.clone());
    }
    grouped
}

fn parse_source(language: SyntaxLanguage, path: &Path, source: &str) -> Option<String> {
    match language {
        SyntaxLanguage::Json => parse_json(path, source),
        SyntaxLanguage::Toml => parse_toml(path, source),
        SyntaxLanguage::Yaml => parse_yaml(path, source),
        SyntaxLanguage::Css => parse_css(path, source),
        _ => parse_tree_sitter(language, path, source),
    }
}

fn parse_tree_sitter(language: SyntaxLanguage, path: &Path, source: &str) -> Option<String> {
    let mut parser = Parser::new();
    let grammar = language.tree_sitter_language()?;
    if parser.set_language(&grammar).is_err() {
        return Some(format!(
            "[{} syntax] {}: failed to load parser grammar",
            language.label(),
            path.display()
        ));
    }
    let Some(tree) = parser.parse(source, None) else {
        return Some(format!(
            "[{} syntax] {}: parser could not parse source",
            language.label(),
            path.display()
        ));
    };
    let root = tree.root_node();
    if root.has_error() || contains_error_node(root) {
        let location = first_error_position(root)
            .map(|(line, col)| format!(":{line}:{col}"))
            .unwrap_or_default();
        return Some(format!(
            "[{} syntax] {}{}: invalid syntax",
            language.label(),
            path.display(),
            location
        ));
    }
    None
}

fn parse_json(path: &Path, source: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(source)
        .err()
        .map(|e| {
            format!(
                "[json syntax] {}:{}:{}: {}",
                path.display(),
                e.line(),
                e.column(),
                e
            )
        })
}

fn parse_toml(path: &Path, source: &str) -> Option<String> {
    source.parse::<toml::Value>().err().map(|e| {
        let location = e
            .span()
            .map(|span| offset_to_line_col(source, span.start))
            .map(|(line, col)| format!(":{line}:{col}"))
            .unwrap_or_default();
        format!("[toml syntax] {}{}: {}", path.display(), location, e)
    })
}

fn parse_yaml(path: &Path, source: &str) -> Option<String> {
    serde_yaml::from_str::<serde_yaml::Value>(source)
        .err()
        .map(|e| {
            if let Some(location) = e.location() {
                format!(
                    "[yaml syntax] {}:{}:{}: {}",
                    path.display(),
                    location.line(),
                    location.column(),
                    e
                )
            } else {
                format!("[yaml syntax] {}: {}", path.display(), e)
            }
        })
}

fn parse_css(path: &Path, source: &str) -> Option<String> {
    let mut depth = 0usize;
    for (idx, ch) in source.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                if depth == 0 {
                    let (line, col) = offset_to_line_col(source, idx);
                    return Some(format!(
                        "[css syntax] {}:{line}:{col}: unmatched closing brace",
                        path.display()
                    ));
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Some(format!(
            "[css syntax] {}: unmatched opening brace",
            path.display()
        ));
    }

    for block in source.split('{').skip(1) {
        let body = block.split('}').next().unwrap_or(block);
        for declaration in body.split(';') {
            let Some((name, value)) = declaration.split_once(':') else {
                continue;
            };
            if !name.trim().is_empty() && value.trim().is_empty() {
                return Some(format!(
                    "[css syntax] {}: empty value for `{}`",
                    path.display(),
                    name.trim()
                ));
            }
        }
    }
    None
}

fn reconstruct_post_diff_source(
    path: &Path,
    hunks: &[DiffHunk],
    working_dir: Option<&Path>,
) -> std::result::Result<String, String> {
    if working_dir.is_none() || hunks.iter().all(is_new_file_hunk) {
        return Ok(reconstruct_new_side_from_hunks(hunks));
    }

    let root = working_dir.expect("checked above");
    let full_path = root.join(path);
    let current = std::fs::read_to_string(&full_path)
        .map_err(|e| format!("could not read {}: {e}", full_path.display()))?;
    let mut original_lines = split_source_lines(&current);
    let mut output = Vec::new();
    let mut cursor = 0usize;
    let mut ordered = hunks.to_vec();
    ordered.sort_by_key(|hunk| hunk.old_start);

    for hunk in ordered {
        let start = hunk.old_start.saturating_sub(1) as usize;
        if start < cursor {
            return Err(format!(
                "overlapping hunk at old line {} after cursor {}",
                hunk.old_start,
                cursor + 1
            ));
        }
        if start > original_lines.len() {
            return Err(format!(
                "hunk starts at old line {}, beyond {} line(s)",
                hunk.old_start,
                original_lines.len()
            ));
        }
        output.extend(original_lines[cursor..start].iter().cloned());
        cursor = start;

        for line in hunk.lines {
            match line {
                DiffLine::Context(text) => {
                    let Some(existing) = original_lines.get(cursor) else {
                        return Err(format!(
                            "context line `{}` expected after end of file",
                            trim_for_error(&text)
                        ));
                    };
                    if normalize_line(existing) != normalize_line(&text) {
                        return Err(format!(
                            "context mismatch at line {}: expected `{}`, got `{}`",
                            cursor + 1,
                            trim_for_error(&text),
                            trim_for_error(existing)
                        ));
                    }
                    output.push(existing.clone());
                    cursor += 1;
                }
                DiffLine::Removed(text) => {
                    let Some(existing) = original_lines.get(cursor) else {
                        return Err(format!(
                            "removed line `{}` expected after end of file",
                            trim_for_error(&text)
                        ));
                    };
                    if normalize_line(existing) != normalize_line(&text) {
                        return Err(format!(
                            "removed-line mismatch at line {}: expected `{}`, got `{}`",
                            cursor + 1,
                            trim_for_error(&text),
                            trim_for_error(existing)
                        ));
                    }
                    cursor += 1;
                }
                DiffLine::Added(text) => output.push(text),
            }
        }
    }

    output.extend(original_lines.drain(cursor..));
    Ok(join_source_lines(&output))
}

fn reconstruct_new_side_from_hunks(hunks: &[DiffHunk]) -> String {
    let mut ordered = hunks.to_vec();
    ordered.sort_by_key(|hunk| hunk.new_start);
    let mut out = String::new();
    for hunk in &ordered {
        out.push_str(&reconstruct_new_side(hunk));
    }
    out
}

fn split_source_lines(source: &str) -> Vec<String> {
    let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = normalized
        .split('\n')
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if normalized.ends_with('\n') && lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

fn join_source_lines(lines: &[String]) -> String {
    let mut source = lines.join("\n");
    source.push('\n');
    source
}

fn normalize_line(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

fn trim_for_error(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() > 80 {
        format!("{}...", text.chars().take(77).collect::<String>())
    } else {
        text.to_string()
    }
}

fn offset_to_line_col(source: &str, offset: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    for (idx, ch) in source.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
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
            return Ok(Some(VerifyResult::Escalate {
                reason: format!("memory query failed: {e}"),
            }));
        }
    };

    let added_text = collected_added_text(hunks);
    let mut errors: Vec<String> = Vec::new();
    for rec in &records {
        for evidence in record_violations(rec, &added_text) {
            errors.push(format!(
                "{}: \"{}\" — violated by: {}",
                kind_label(rec),
                record_quote(rec),
                evidence,
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

async fn verify_node_project(
    hunks: &[DiffHunk],
    working_dir: &Path,
) -> Result<Option<VerifyResult>> {
    let touches_package_json = hunks
        .iter()
        .any(|hunk| hunk.file_path == Path::new("package.json"));
    let touches_web_source = hunks.iter().any(|hunk| {
        matches!(
            hunk.file_path.extension().and_then(|ext| ext.to_str()),
            Some("js" | "jsx" | "ts" | "tsx" | "html" | "css")
        )
    });
    if !touches_package_json && !touches_web_source {
        return Ok(None);
    }

    let temp = materialize_post_diff_workspace(hunks, working_dir)?;
    let package_path = temp.path().join("package.json");
    if !package_path.is_file() {
        return Ok(None);
    }
    let package_text = std::fs::read_to_string(&package_path)?;
    let package: serde_json::Value = match serde_json::from_str(&package_text) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    if !touches_package_json && !is_vite_or_react_package(&package) {
        return Ok(None);
    }

    let scripts_value = package.get("scripts");
    let scripts = scripts_value.and_then(|s| s.as_object());
    let mut steps: Vec<NodeVerifyStep> = vec![NodeVerifyStep::new(
        "npm install --no-audit --no-fund",
        ["install", "--no-audit", "--no-fund"],
    )];
    let raw_test_script = scripts
        .and_then(|map| map.get("test"))
        .and_then(|v| v.as_str());
    let run_tests = match raw_test_script {
        Some(script) => should_run_npm_test(temp.path(), script),
        None => false,
    };
    if run_tests {
        if let Some(step) = select_node_test_command(scripts_value) {
            steps.push(step);
        }
    }
    let has_build = scripts.and_then(|map| map.get("build")).is_some();
    if has_build {
        steps.push(NodeVerifyStep::new("npm run build", ["run", "build"]));
    }
    if steps.len() == 1 && !touches_package_json {
        return Ok(None);
    }

    let mut errors = Vec::new();
    for step in &steps {
        if let Some(error) = run_npm_command(temp.path(), step).await? {
            errors.push(error);
            break;
        }
    }

    if errors.is_empty() {
        Ok(None)
    } else {
        Ok(Some(VerifyResult::Fail {
            layer: VerifyLayer::Test,
            errors,
            attempt: 1,
        }))
    }
}

fn is_vite_or_react_package(package: &serde_json::Value) -> bool {
    let mut names = Vec::new();
    for section in ["dependencies", "devDependencies"] {
        if let Some(map) = package.get(section).and_then(|value| value.as_object()) {
            names.extend(map.keys().map(|key| key.as_str()));
        }
    }
    names.iter().any(|name| {
        matches!(
            *name,
            "vite" | "react" | "react-dom" | "vitest" | "chess.js"
        )
    })
}

fn should_run_npm_test(working_dir: &Path, test_script: &str) -> bool {
    if has_node_test_file(working_dir) {
        return true;
    }

    !is_file_discovery_test_script(test_script)
}

fn is_file_discovery_test_script(test_script: &str) -> bool {
    let lower = test_script.to_ascii_lowercase();
    lower.contains("vitest") || lower.contains("jest")
}

fn has_node_test_file(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if matches!(
                name.as_str(),
                ".git" | ".phonton" | "coverage" | "dist" | "node_modules" | "target"
            ) {
                continue;
            }
            if has_node_test_file(&path) {
                return true;
            }
        } else if is_node_test_file(&path) {
            return true;
        }
    }

    false
}

fn is_node_test_file(path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    matches!(ext, "js" | "jsx" | "ts" | "tsx")
        && (file_name.contains(".test.") || file_name.contains(".spec."))
}

fn materialize_post_diff_workspace(
    hunks: &[DiffHunk],
    working_dir: &Path,
) -> Result<tempfile::TempDir> {
    let temp = tempfile::tempdir()?;
    copy_workspace_for_verification(working_dir, temp.path())?;

    let mut grouped: BTreeMap<PathBuf, Vec<DiffHunk>> = BTreeMap::new();
    for hunk in hunks {
        grouped
            .entry(hunk.file_path.clone())
            .or_default()
            .push(hunk.clone());
    }

    for (path, file_hunks) in grouped {
        if path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            continue;
        }
        let source = reconstruct_post_diff_source(&path, &file_hunks, Some(working_dir)).map_err(
            |reason| anyhow::anyhow!("could not materialize {}: {reason}", path.display()),
        )?;
        let full = temp.path().join(&path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(full, source)?;
    }

    Ok(temp)
}

fn copy_workspace_for_verification(source: &Path, target: &Path) -> Result<()> {
    if !source.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(
            name.as_ref(),
            ".git" | "target" | "node_modules" | "dist" | ".phonton"
        ) || name.ends_with(".sqlite3")
            || name.ends_with(".log")
        {
            continue;
        }
        let src = entry.path();
        let dst = target.join(entry.file_name());
        if src.is_dir() {
            std::fs::create_dir_all(&dst)?;
            copy_workspace_for_verification(&src, &dst)?;
        } else if src.is_file() {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let _ = std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// One npm subcommand invocation in the Node verification pipeline
/// (e.g. `npm install`, `npm test -- --run`, `npm run build`).
///
/// The `label` is shown verbatim in failure receipts so users see the
/// exact command Phonton tried; `args` is what we pass to the spawned
/// `npm` process. CI environment variables are injected by
/// [`run_npm_command`] and are not part of this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeVerifyStep {
    /// Human-readable command shown in receipts and error output.
    pub label: String,
    /// Argument vector passed to the `npm` executable.
    pub args: Vec<String>,
}

impl NodeVerifyStep {
    fn new<L, I, S>(label: L, args: I) -> Self
    where
        L: Into<String>,
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            label: label.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

/// Select a deterministic, non-interactive `npm` test command from the
/// project's `scripts` object.
///
/// Returns `None` when there is no usable `test` script (or the project
/// has no `scripts` map at all). The selection rules, in order:
///
/// 1. `test:ci` → `npm run test:ci`
/// 2. `test:run` → `npm run test:run`
/// 3. `test`:
///    * Vitest invocation without explicit `--run`/`run` → `npm test -- --run`
///    * Jest invocation without `--watchAll=false`/`--ci` → `npm test -- --watchAll=false`
///    * Otherwise → `npm test`
///
/// This addresses the v0.13.x failure mode where `npm test` against a
/// stock Vite scaffold dropped Vitest into watch mode and hung until the
/// verifier's 180s timeout fired. Callers must still inject CI/non-
/// interactive env vars on the spawned process — see
/// [`npm_verification_env`].
pub fn select_node_test_command(scripts: Option<&serde_json::Value>) -> Option<NodeVerifyStep> {
    let map = scripts.and_then(|value| value.as_object())?;
    for ci_script in ["test:ci", "test:run"] {
        if let Some(s) = map.get(ci_script).and_then(|v| v.as_str()) {
            if !s.trim().is_empty() {
                return Some(NodeVerifyStep::new(
                    format!("npm run {ci_script}"),
                    ["run", ci_script],
                ));
            }
        }
    }
    let test_script = map.get("test").and_then(|v| v.as_str())?;
    let trimmed = test_script.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("vitest") {
        let already_non_watch = lower
            .split_whitespace()
            .any(|tok| tok == "run" || tok == "--run")
            || lower.contains(" --run");
        if already_non_watch {
            return Some(NodeVerifyStep::new("npm test", ["test"]));
        }
        return Some(NodeVerifyStep::new(
            "npm test -- --run",
            ["test", "--", "--run"],
        ));
    }
    if lower.contains("jest") {
        let already_non_watch = lower.contains("--watchall=false") || lower.contains("--ci");
        if already_non_watch {
            return Some(NodeVerifyStep::new("npm test", ["test"]));
        }
        return Some(NodeVerifyStep::new(
            "npm test -- --watchAll=false",
            ["test", "--", "--watchAll=false"],
        ));
    }
    Some(NodeVerifyStep::new("npm test", ["test"]))
}

/// Environment variables Phonton always sets on verification subprocesses
/// to force non-interactive, CI-style behavior.
///
/// Vitest and Jest both default to watch mode when stdout looks like a
/// TTY, and `npm` itself can prompt for confirmation on certain
/// operations. Setting `CI=1` plus the npm-config equivalents makes the
/// underlying tools behave deterministically inside the 180s verifier
/// budget instead of dropping into an interactive loop and timing out.
///
/// Exposed for tests and for any future verifier that shells out to npm.
pub fn npm_verification_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("CI", "1"),
        ("NPM_CONFIG_YES", "true"),
        ("NPM_CONFIG_FUND", "false"),
        ("NPM_CONFIG_AUDIT", "false"),
        ("NPM_CONFIG_LOGLEVEL", "error"),
        ("NPM_CONFIG_UPDATE_NOTIFIER", "false"),
        ("NO_COLOR", "1"),
        ("FORCE_COLOR", "0"),
        ("NODE_NO_WARNINGS", "1"),
    ]
}

const NPM_STEP_TIMEOUT_SECS: u64 = 180;

/// Format the timeout failure shown to the user when an `npm` step
/// exceeds its budget.
///
/// Test-only steps that are not themselves the project test runner
/// (currently `npm install` and `npm run build`) get a generic timeout
/// message; the test runner gets a harness-aware message that names the
/// likely cause (watch mode) and the concrete repairs Phonton already
/// understands. Centralising the format here keeps the failure-receipt
/// language consistent and lets the helper be unit-tested without
/// actually waiting `secs` seconds.
fn format_npm_timeout_message(label: &str, secs: u64) -> String {
    let is_test_step = label.starts_with("npm test") || label.starts_with("npm run test:");
    if is_test_step {
        format!(
            "{label} timed out after {secs}s (test harness timeout — likely interactive/watch mode). \
             Repair: add a `test:ci` or `test:run` script in package.json that exits, or invoke the runner non-interactively \
             (vitest with `--run`, jest with `--watchAll=false`)."
        )
    } else {
        format!("{label} timed out after {secs}s")
    }
}

async fn run_npm_command(working_dir: &Path, step: &NodeVerifyStep) -> Result<Option<String>> {
    let mut cmd = Command::new(npm_bin());
    cmd.current_dir(working_dir).args(&step.args);
    for (key, value) in npm_verification_env() {
        cmd.env(key, value);
    }
    let fut = cmd.output();
    match tokio::time::timeout(Duration::from_secs(NPM_STEP_TIMEOUT_SECS), fut).await {
        Ok(Ok(out)) if out.status.success() => Ok(None),
        Ok(Ok(out)) => {
            let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
            combined.push_str(&String::from_utf8_lossy(&out.stderr));
            Ok(Some(format!(
                "{} failed: {}",
                step.label,
                last_lines(&combined, 20)
            )))
        }
        Ok(Err(e)) => Ok(Some(format!("could not invoke {}: {e}", step.label))),
        Err(_) => Ok(Some(format_npm_timeout_message(
            &step.label,
            NPM_STEP_TIMEOUT_SECS,
        ))),
    }
}

fn npm_bin() -> &'static str {
    if cfg!(windows) {
        "npm.cmd"
    } else {
        "npm"
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
    // Skip when we're not in a Rust workspace — `cargo check` would just
    // error with "could not find Cargo.toml" and turn every legitimate
    // create-a-new-project goal into a failure. Syntax (Layer 1) still
    // catches malformed Rust regardless of project shape.
    if find_cargo_workspace(working_dir).is_none() {
        return Ok(None);
    }
    let mut errors = Vec::new();
    for pkg in packages {
        let output = Command::new("cargo")
            .current_dir(working_dir)
            .args(["check", "--package", pkg, "--message-format", "json"])
            .output()
            .await;

        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                errors.extend(parse_cargo_errors(&stdout, pkg));
            }
            Err(e) => {
                errors.push(format!("could not invoke cargo check for {pkg}: {e}"));
            }
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
    // Same short-circuit as the crate check — no Cargo.toml means there's
    // nothing for cargo to verify. The previous behaviour was to surface
    // "cargo check --workspace failed: could not find `Cargo.toml`" as a
    // hard failure, which broke every project-bootstrap goal.
    if find_cargo_workspace(working_dir).is_none() {
        return Ok(None);
    }
    let output = Command::new("cargo")
        .current_dir(working_dir)
        .args(["check", "--workspace", "--message-format", "json"])
        .output()
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
                }
            }
            errs
        }
        Err(e) => vec![format!("could not invoke cargo check --workspace: {e}")],
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
/// A non-zero exit or timeout surfaces the last 20 lines of combined
/// stdout+stderr as the failure message — enough to point at a failing
/// assertion without flooding the UI.
pub async fn verify_test(packages: &[String], working_dir: &Path) -> Result<Option<VerifyResult>> {
    // Skip for the same reason the cargo check layers do.
    if find_cargo_workspace(working_dir).is_none() {
        return Ok(None);
    }
    let mut errors = Vec::new();
    for pkg in packages {
        let fut = Command::new("cargo")
            .current_dir(working_dir)
            .args(["test", "--package", pkg, "--", "--nocapture"])
            .output();

        match tokio::time::timeout(Duration::from_secs(600), fut).await {
            Ok(Ok(out)) if out.status.success() => {}
            Ok(Ok(out)) => {
                let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
                combined.push_str(&String::from_utf8_lossy(&out.stderr));
                errors.push(last_lines(&combined, 20));
            }
            Ok(Err(e)) => {
                errors.push(format!("could not invoke cargo test for {pkg}: {e}"));
            }
            Err(_) => {
                errors.push(format!("cargo test for {pkg} timed out after 600s"));
            }
        }
    }

    if errors.is_empty() {
        Ok(None)
    } else {
        Ok(Some(VerifyResult::Fail {
            layer: VerifyLayer::Test,
            errors,
            attempt: 1,
        }))
    }
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

fn syntax_language_for_path(path: &Path) -> Option<SyntaxLanguage> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "rs" => Some(SyntaxLanguage::Rust),
        "py" | "pyw" => Some(SyntaxLanguage::Python),
        "js" => Some(SyntaxLanguage::JavaScript),
        "jsx" => Some(SyntaxLanguage::Jsx),
        "ts" => Some(SyntaxLanguage::TypeScript),
        "tsx" => Some(SyntaxLanguage::Tsx),
        "json" => Some(SyntaxLanguage::Json),
        "toml" => Some(SyntaxLanguage::Toml),
        "yml" | "yaml" => Some(SyntaxLanguage::Yaml),
        "html" | "htm" => Some(SyntaxLanguage::Html),
        "css" => Some(SyntaxLanguage::Css),
        _ => None,
    }
}

fn is_new_file_hunk(hunk: &DiffHunk) -> bool {
    hunk.old_start <= 1 && hunk.old_count == 0 && hunk.new_start <= 1
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

fn first_error_position(node: Node<'_>) -> Option<(usize, usize)> {
    if node.is_error() || node.is_missing() {
        let pos = node.start_position();
        return Some((pos.row + 1, pos.column + 1));
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(pos) = first_error_position(child) {
            return Some(pos);
        }
    }
    None
}

/// Infer the cargo package name for a path inside the workspace.
/// Extract a crate name from a relative path component.
///
/// Tries two strategies in order:
/// 1. Any path component starting with `phonton-` (workspace convention).
/// 2. Walk up the path looking for a `Cargo.toml` and read its `[package] name`.
fn crate_name_for(path: &Path) -> Option<String> {
    // Fast path: phonton workspace layout.
    for component in path.components() {
        let s = component.as_os_str().to_string_lossy();
        if s.starts_with("phonton-") {
            return Some(s.into_owned());
        }
    }
    // Slow path: walk up looking for Cargo.toml.
    let mut dir = path.parent()?;
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.exists() {
            if let Ok(text) = std::fs::read_to_string(&manifest) {
                // Simple line scan — avoids pulling in toml just for this.
                for line in text.lines() {
                    let line = line.trim();
                    if let Some(rest) = line.strip_prefix("name") {
                        if let Some(val) = rest
                            .trim_start_matches([' ', '=', '"', '\''].as_ref())
                            .split('"')
                            .next()
                        {
                            let name = val.trim_matches(['"', '\'', ' '].as_ref()).to_string();
                            if !name.is_empty() {
                                return Some(name);
                            }
                        }
                    }
                }
            }
        }
        match dir.parent() {
            Some(p) if p != dir => dir = p,
            _ => break,
        }
    }
    None
}

fn touched_packages(hunks: &[DiffHunk]) -> Vec<String> {
    let mut packages: Vec<String> = Vec::new();
    for hunk in hunks {
        if let Some(pkg) = crate_name_for(&hunk.file_path) {
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
    use std::path::PathBuf;

    fn hunk(path: &str, lines: Vec<DiffLine>) -> DiffHunk {
        DiffHunk {
            file_path: PathBuf::from(path),
            old_start: 1,
            old_count: 0,
            new_start: 1,
            new_count: lines.len() as u32,
            lines,
        }
    }

    fn generated_file(path: &str, source: &str) -> DiffHunk {
        let lines = source
            .lines()
            .map(|line| DiffLine::Added(line.to_string()))
            .collect::<Vec<_>>();
        hunk(path, lines)
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
    fn python_syntax_fail_on_broken_generated_file() {
        let h = hunk(
            "chess.py",
            vec![
                DiffLine::Added("def broken():".into()),
                DiffLine::Added("    return \"unterminated".into()),
            ],
        );
        match verify_python_syntax(&[h]) {
            Some(VerifyResult::Fail {
                layer: VerifyLayer::Syntax,
                errors,
                ..
            }) => {
                let joined = errors.join("\n");
                assert!(
                    joined.contains("chess.py"),
                    "error should identify the Python file: {joined}"
                );
            }
            other => panic!("expected python syntax fail, got {other:?}"),
        }
    }

    #[test]
    fn python_syntax_pass_on_valid_generated_file() {
        let h = hunk(
            "chess.py",
            vec![
                DiffLine::Added("def ok():".into()),
                DiffLine::Added("    return \"chess\"".into()),
            ],
        );
        assert!(verify_python_syntax(&[h]).is_none());
    }

    #[test]
    fn python_syntax_skips_partial_hunks() {
        let h = DiffHunk {
            file_path: PathBuf::from("chess.py"),
            old_start: 20,
            old_count: 2,
            new_start: 20,
            new_count: 2,
            lines: vec![
                DiffLine::Removed("    return old".into()),
                DiffLine::Added("    return \"unterminated".into()),
            ],
        };
        assert!(verify_python_syntax(&[h]).is_none());
    }

    #[tokio::test]
    async fn verify_diff_rejects_broken_generated_python() {
        let tmp = tempfile::tempdir().unwrap();
        let h = hunk(
            "chess.py",
            vec![
                DiffLine::Added("def broken():".into()),
                DiffLine::Added("    return \"unterminated".into()),
            ],
        );

        match verify_diff(&[h], tmp.path()).await.unwrap() {
            VerifyResult::Fail {
                layer: VerifyLayer::Syntax,
                errors,
                ..
            } => {
                assert!(
                    errors.iter().any(|e| e.contains("chess.py")),
                    "syntax failure should name the generated file: {errors:?}"
                );
            }
            other => panic!("expected syntax failure, got {other:?}"),
        }
    }

    #[test]
    fn syntax_registry_fails_broken_generated_supported_languages() {
        let tmp = tempfile::tempdir().unwrap();
        let cases = [
            ("app.js", "function broken( {"),
            ("app.jsx", "export default function App(){ return <div>; }"),
            ("app.ts", "const value: = 1;"),
            ("app.tsx", "export function App(){ return <div>; }"),
            ("package.json", r#"{ "scripts": }"#),
            ("config.toml", "name ="),
            ("workflow.yaml", "jobs: ["),
            ("index.html", "<div <span>broken</span>"),
            ("style.css", ".board { color: ; }"),
        ];

        for (path, source) in cases {
            let h = generated_file(path, source);
            match verify_syntax_in_workspace(&[h], tmp.path()) {
                Some(VerifyResult::Fail {
                    layer: VerifyLayer::Syntax,
                    errors,
                    ..
                }) => {
                    let joined = errors.join("\n");
                    assert!(
                        joined.contains(path),
                        "syntax error should mention {path}: {joined}"
                    );
                }
                other => panic!("expected syntax failure for {path}, got {other:?}"),
            }
        }
    }

    #[test]
    fn syntax_registry_passes_valid_generated_supported_languages() {
        let tmp = tempfile::tempdir().unwrap();
        let cases = [
            ("app.js", "function ok() { return 1; }\n"),
            (
                "app.jsx",
                "export default function App(){ return <div>ok</div>; }\n",
            ),
            ("app.ts", "const value: number = 1;\n"),
            (
                "app.tsx",
                "export function App(){ return <div>ok</div>; }\n",
            ),
            ("package.json", r#"{ "scripts": { "test": "echo ok" } }"#),
            ("config.toml", "name = \"phonton\"\n"),
            (
                "workflow.yaml",
                "jobs:\n  test:\n    runs-on: ubuntu-latest\n",
            ),
            (
                "index.html",
                "<!doctype html><html><body>ok</body></html>\n",
            ),
            ("style.css", ".board { color: white; }\n"),
        ];

        for (path, source) in cases {
            let h = generated_file(path, source);
            assert!(
                verify_syntax_in_workspace(&[h], tmp.path()).is_none(),
                "valid generated file should pass syntax: {path}"
            );
        }
    }

    #[test]
    fn syntax_registry_reconstructs_existing_file_before_parsing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("app.js"),
            "function ok() {\n  return 1;\n}\n",
        )
        .unwrap();
        let h = DiffHunk {
            file_path: PathBuf::from("app.js"),
            old_start: 2,
            old_count: 1,
            new_start: 2,
            new_count: 1,
            lines: vec![
                DiffLine::Removed("  return 1;".into()),
                DiffLine::Added("  return ; }".into()),
            ],
        };

        match verify_syntax_in_workspace(&[h], tmp.path()) {
            Some(VerifyResult::Fail {
                layer: VerifyLayer::Syntax,
                errors,
                ..
            }) => assert!(
                errors.iter().any(|error| error.contains("app.js")),
                "reconstructed-file syntax error should mention app.js: {errors:?}"
            ),
            other => panic!("expected reconstructed syntax failure, got {other:?}"),
        }
    }

    #[test]
    fn syntax_registry_fails_when_existing_file_reconstruction_fails() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("app.js"), "function ok() { return 1; }\n").unwrap();
        let h = DiffHunk {
            file_path: PathBuf::from("app.js"),
            old_start: 20,
            old_count: 1,
            new_start: 20,
            new_count: 1,
            lines: vec![DiffLine::Added("function later() {}".into())],
        };

        match verify_syntax_in_workspace(&[h], tmp.path()) {
            Some(VerifyResult::Fail {
                layer: VerifyLayer::Syntax,
                errors,
                ..
            }) => assert!(
                errors
                    .iter()
                    .any(|error| error.contains("could not reconstruct")),
                "reconstruction failure should be explicit: {errors:?}"
            ),
            other => panic!("expected reconstruction failure, got {other:?}"),
        }
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

    #[tokio::test]
    async fn verify_diff_fails_when_node_build_script_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let package = DiffHunk {
            file_path: PathBuf::from("package.json"),
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: 9,
            lines: r#"{
  "type": "module",
  "scripts": {
    "test": "node -e \"process.exit(0)\"",
    "build": "node -e \"process.exit(7)\""
  },
  "dependencies": {},
  "devDependencies": {}
}"#
            .lines()
            .map(|line| DiffLine::Added(line.into()))
            .collect(),
        };
        let source = DiffHunk {
            file_path: PathBuf::from("src/main.ts"),
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine::Added("export const ok: boolean = true;".into())],
        };

        let result = verify_diff(&[package, source], tmp.path()).await.unwrap();

        match result {
            VerifyResult::Fail {
                layer: VerifyLayer::Test,
                errors,
                ..
            } => {
                assert!(errors.iter().any(|error| error.contains("npm run build")));
            }
            other => panic!("expected failing npm build to fail verification, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn verify_diff_skips_vitest_until_test_files_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let package = DiffHunk {
            file_path: PathBuf::from("package.json"),
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: 9,
            lines: r#"{
  "type": "module",
  "scripts": {
    "test": "vitest run",
    "build": "node -e \"process.exit(0)\""
  },
  "dependencies": {},
  "devDependencies": {}
}"#
            .lines()
            .map(|line| DiffLine::Added(line.into()))
            .collect(),
        };
        let source = DiffHunk {
            file_path: PathBuf::from("src/main.tsx"),
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine::Added("export const ok: boolean = true;".into())],
        };

        let result = verify_diff(&[package, source], tmp.path()).await;

        match result {
            Ok(VerifyResult::Pass { .. }) => {}
            other => panic!(
                "vitest should not fail scaffold verification before test files exist: {other:?}"
            ),
        }
    }

    // -------------------------------------------------------------
    // Node test command selection (v0.14.0)
    // -------------------------------------------------------------

    fn scripts_value(json: &str) -> serde_json::Value {
        let v: serde_json::Value = serde_json::from_str(json).expect("valid json");
        v.get("scripts").cloned().expect("scripts key present")
    }

    #[test]
    fn select_node_test_command_prefers_test_ci_script() {
        let scripts = scripts_value(
            r#"{"scripts":{"test":"vitest","test:ci":"vitest run --reporter=verbose"}}"#,
        );
        let step = super::select_node_test_command(Some(&scripts)).expect("step");
        assert_eq!(step.label, "npm run test:ci");
        assert_eq!(step.args, vec!["run", "test:ci"]);
    }

    #[test]
    fn select_node_test_command_prefers_test_run_when_ci_absent() {
        let scripts = scripts_value(r#"{"scripts":{"test":"jest","test:run":"jest --ci"}}"#);
        let step = super::select_node_test_command(Some(&scripts)).expect("step");
        assert_eq!(step.label, "npm run test:run");
        assert_eq!(step.args, vec!["run", "test:run"]);
    }

    #[test]
    fn select_node_test_command_rewrites_vitest_to_non_watch() {
        // Stock Vite scaffold: `"test": "vitest"` enters watch mode by
        // default and was the source of the v0.13.x 180s hang.
        let scripts = scripts_value(r#"{"scripts":{"test":"vitest"}}"#);
        let step = super::select_node_test_command(Some(&scripts)).expect("step");
        assert_eq!(step.label, "npm test -- --run");
        assert_eq!(step.args, vec!["test", "--", "--run"]);
    }

    #[test]
    fn select_node_test_command_keeps_explicit_vitest_run() {
        let scripts = scripts_value(r#"{"scripts":{"test":"vitest run"}}"#);
        let step = super::select_node_test_command(Some(&scripts)).expect("step");
        assert_eq!(step.label, "npm test");
        assert_eq!(step.args, vec!["test"]);
    }

    #[test]
    fn select_node_test_command_rewrites_jest_to_no_watch() {
        let scripts = scripts_value(r#"{"scripts":{"test":"jest"}}"#);
        let step = super::select_node_test_command(Some(&scripts)).expect("step");
        assert_eq!(step.label, "npm test -- --watchAll=false");
        assert_eq!(step.args, vec!["test", "--", "--watchAll=false"]);
    }

    #[test]
    fn select_node_test_command_keeps_jest_when_already_ci() {
        let scripts = scripts_value(r#"{"scripts":{"test":"jest --watchAll=false"}}"#);
        let step = super::select_node_test_command(Some(&scripts)).expect("step");
        assert_eq!(step.args, vec!["test"]);
    }

    #[test]
    fn select_node_test_command_passes_through_custom_scripts() {
        let scripts = scripts_value(r#"{"scripts":{"test":"node scripts/test.js"}}"#);
        let step = super::select_node_test_command(Some(&scripts)).expect("step");
        assert_eq!(step.label, "npm test");
        assert_eq!(step.args, vec!["test"]);
    }

    #[test]
    fn select_node_test_command_returns_none_for_missing_or_empty() {
        let no_scripts: serde_json::Value = serde_json::from_str("{}").unwrap();
        assert!(super::select_node_test_command(no_scripts.get("scripts")).is_none());
        let empty_script = scripts_value(r#"{"scripts":{"test":""}}"#);
        assert!(super::select_node_test_command(Some(&empty_script)).is_none());
    }

    #[test]
    fn npm_timeout_message_classifies_test_harness_timeouts() {
        // The receipt must (a) name the exact attempted command verbatim
        // and (b) point at the actual repair path. Both behaviours
        // regress silently if the format string drifts, so pin them.
        let test_msg = super::format_npm_timeout_message("npm test -- --run", 180);
        assert!(test_msg.contains("npm test -- --run"));
        assert!(test_msg.contains("test harness timeout"));
        assert!(test_msg.contains("test:ci") && test_msg.contains("test:run"));
        assert!(test_msg.contains("--run") && test_msg.contains("--watchAll=false"));

        let script_msg = super::format_npm_timeout_message("npm run test:ci", 180);
        assert!(script_msg.contains("npm run test:ci"));
        assert!(script_msg.contains("test harness timeout"));

        // Non-test steps get the plain timeout message — no test-runner
        // repair guidance, since that would be misleading.
        let build_msg = super::format_npm_timeout_message("npm run build", 180);
        assert!(build_msg.starts_with("npm run build timed out after 180s"));
        assert!(!build_msg.contains("test harness"));
        let install_msg =
            super::format_npm_timeout_message("npm install --no-audit --no-fund", 180);
        assert!(install_msg.starts_with("npm install --no-audit --no-fund timed out after 180s"));
        assert!(!install_msg.contains("test harness"));
    }

    #[test]
    fn npm_verification_env_forces_non_interactive_mode() {
        // The CI variables here are the contract that keeps Vitest/Jest
        // out of watch mode and npm from prompting; regressing this set
        // is what makes a v0.13.x-style 180s timeout possible again.
        let env: std::collections::HashMap<_, _> =
            super::npm_verification_env().into_iter().collect();
        assert_eq!(env.get("CI"), Some(&"1"));
        assert_eq!(env.get("NPM_CONFIG_YES"), Some(&"true"));
        assert_eq!(env.get("NPM_CONFIG_FUND"), Some(&"false"));
        assert_eq!(env.get("NPM_CONFIG_AUDIT"), Some(&"false"));
        assert_eq!(env.get("NO_COLOR"), Some(&"1"));
    }

    /// Regression for the v0.13.5 chess-seed timeout: a stock Vite/React
    /// project with `"test": "vitest"` and an existing `*.test.ts` file
    /// previously enqueued `npm test` (watch mode) and hit the 180s
    /// verifier timeout. The selector must rewrite it to `npm test -- --run`.
    #[test]
    fn vitest_watch_mode_scaffold_is_rewritten_to_non_watch() {
        let scripts =
            scripts_value(r#"{"scripts":{"test":"vitest","build":"vite build","dev":"vite"}}"#);
        let step = super::select_node_test_command(Some(&scripts)).expect("step");
        assert!(
            step.args == vec!["test", "--", "--run"],
            "v0.13.x regression: vitest scaffold must run in non-watch mode, got {:?}",
            step.args
        );
    }

    #[test]
    fn npm_test_discovery_scripts_wait_for_test_files() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            !super::should_run_npm_test(tmp.path(), "vitest run"),
            "vitest should wait until a test file exists"
        );
        assert!(
            super::should_run_npm_test(tmp.path(), "node scripts/test.js"),
            "custom test scripts should still run even without test-like filenames"
        );

        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("chessRules.test.ts"), "test('ok', () => {})").unwrap();

        assert!(
            super::should_run_npm_test(tmp.path(), "vitest run"),
            "vitest should run once generated tests exist"
        );
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
    async fn browser_runtime_fails_when_artifact_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let result = verify_browser_runtime(
            tmp.path(),
            &BrowserRuntimeSpec {
                artifact_path: PathBuf::from("index.html"),
                required_selectors: vec!["#board".into()],
            },
        )
        .await
        .unwrap();

        match result {
            VerifyResult::Fail {
                layer: VerifyLayer::RuntimeSmoke,
                errors,
                ..
            } => assert!(errors[0].contains("index.html")),
            other => panic!("expected runtime smoke failure, got {other:?}"),
        }
    }

    #[test]
    fn browser_runtime_script_contains_required_selectors() {
        let tmp = tempfile::tempdir().unwrap();
        let html = tmp.path().join("index.html");
        std::fs::write(&html, "<!doctype html><div id='board'></div>").unwrap();

        let script = browser_verify_script(&html, &["#board".into()]).unwrap();

        assert!(script.contains("file:///"));
        assert!(script.contains("#board"));
        assert!(script.contains("waitForSelector"));
    }
}
