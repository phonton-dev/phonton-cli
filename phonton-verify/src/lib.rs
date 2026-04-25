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

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use phonton_memory::MemoryStore;
use phonton_types::{DiffHunk, DiffLine, MemoryRecord, VerifyLayer, VerifyResult};
use tokio::process::Command;
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
    if let Some(fail) = verify_syntax(hunks) {
        return Ok(fail);
    }

    if let Some(mem) = memory {
        if let Some(fail) = verify_decisions(hunks, mem).await? {
            return Ok(fail);
        }
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

/// Layer 1: tree-sitter parse of each hunk's post-diff view.
pub fn verify_syntax(hunks: &[DiffHunk]) -> Option<VerifyResult> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_rust::language())
        .is_err()
    {
        return Some(VerifyResult::Escalate {
            reason: "failed to load tree-sitter-rust grammar".into(),
        });
    }

    let mut errors = Vec::new();
    for hunk in hunks {
        if !is_rust_path(&hunk.file_path) {
            continue;
        }
        let snippet = reconstruct_new_side(hunk);
        let Some(tree) = parser.parse(&snippet, None) else {
            errors.push(format!(
                "tree-sitter could not parse hunk in {}",
                hunk.file_path.display()
            ));
            continue;
        };
        if tree.root_node().has_error() || contains_error_node(tree.root_node()) {
            errors.push(format!(
                "syntax error in hunk targeting {} (lines {}..{})",
                hunk.file_path.display(),
                hunk.new_start,
                hunk.new_start + hunk.new_count
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
    let mut errors = Vec::new();
    for pkg in packages {
        let output = Command::new("cargo")
            .current_dir(working_dir)
            .args([
                "check",
                "--package",
                pkg,
                "--message-format",
                "json",
            ])
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
                    errs.push(format!("cargo check --workspace failed: {}", last_lines(msg, 5)));
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
pub async fn verify_test(
    packages: &[String],
    working_dir: &Path,
) -> Result<Option<VerifyResult>> {
    let mut errors = Vec::new();
    for pkg in packages {
        let fut = Command::new("cargo")
            .current_dir(working_dir)
            .args(["test", "--package", pkg, "--", "--nocapture"])
            .output();

        match tokio::time::timeout(Duration::from_secs(120), fut).await {
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
                errors.push(format!("cargo test for {pkg} timed out after 120s"));
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
                hits.push(format!("added code contains `{}`", needle.trim_end_matches('(')));
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
    if (lc.contains("thiserror") && lc.contains("anyhow"))
        || lc.contains("no anyhow in lib")
        || lc.contains("avoid anyhow in lib")
    {
        if lower.contains("anyhow::") || lower.contains("use anyhow") {
            hits.push("added code uses `anyhow` where the convention is `thiserror`".into());
        }
    }

    // Rule 3: "no blocking in async".
    if lc.contains("no blocking") || lc.contains("avoid blocking") || lc.contains("blocking call")
    {
        for needle in ["std::thread::sleep", "std::fs::read", "std::fs::write"] {
            if lower.contains(needle) {
                hits.push(format!("added code calls blocking `{needle}` (convention forbids)"));
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

fn is_rust_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("rs"))
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
                        if let Some(val) = rest.trim_start_matches([' ', '=', '"', '\''].as_ref())
                            .split('"').next()
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
        let Some(msg) = val.get("message") else { continue };
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
            vec![DiffLine::Added(
                "let v = some_call().unwrap();".into(),
            )],
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
            vec![DiffLine::Added(
                "let v = some_call()?;".into(),
            )],
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
}
