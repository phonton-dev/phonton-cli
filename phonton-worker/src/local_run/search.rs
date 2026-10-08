//! Deterministic search decisions driven by immutable candidate evidence.
use phonton_types::code_context::SourceExcerpt;
use phonton_types::{
    local::{CheckStatus, EditProtocol},
    local_run::*,
};
use phonton_types::{DiffHunk, DiffLine};
use serde_json::{json, Value};
use std::path::Path;

#[derive(Debug)]
pub(super) struct GroundedHypothesis {
    pub path: String,
    pub search: Option<String>,
    pub mechanism: String,
    pub difference: String,
}

pub(super) fn inference_budget_allows(
    action: &SearchAction,
    used: u32,
    limit: u32,
    output_tokens_left: u64,
) -> bool {
    let (calls, minimum_tokens) = if *action == SearchAction::Restart {
        (2, 256)
    } else {
        (1, 128)
    };
    used.saturating_add(calls) <= limit && output_tokens_left >= minimum_tokens
}

/// A malformed baseline edit has no candidate to repair. If a fresh
/// hypothesis and edit cannot both fit, spend the remaining call on one
/// direct baseline retry instead of stopping with an unused edit allowance.
pub(super) fn next_with_budget(
    candidates: &[CandidateEvidence],
    calls_used: u32,
    call_limit: u32,
    output_tokens_left: u64,
    mixed_creation: bool,
) -> SearchDecision {
    let mut decision = next(candidates);
    if decision.action != SearchAction::Restart
        || mixed_creation
        || inference_budget_allows(
            &SearchAction::Restart,
            calls_used,
            call_limit,
            output_tokens_left,
        )
        || !inference_budget_allows(
            &SearchAction::Initial,
            calls_used,
            call_limit,
            output_tokens_left,
        )
        || !candidates.last().is_some_and(|last| {
            last.content_sha256.is_none()
                && last
                    .decision
                    .as_ref()
                    .is_some_and(|previous| previous.parent_candidate.is_none())
        })
    {
        return decision;
    }
    decision.action = SearchAction::Initial;
    decision.reason =
        "Previous baseline edit did not produce a candidate; retry directly within the remaining single-call budget"
            .into();
    decision
}

pub(super) fn bound_edit_choices(
    path: &str,
    search: Option<&str>,
    creating: bool,
    protocol: EditProtocol,
    paths: &[String],
    searches: &[String],
) -> std::result::Result<(Vec<String>, Vec<String>), String> {
    if !paths.iter().any(|allowed| allowed == path) {
        return Err("Strategy path was omitted from the edit prompt".into());
    }
    if creating {
        return Ok((vec![path.into()], Vec::new()));
    }
    let search = search.ok_or("Strategy has no source anchor")?;
    if !searches.iter().any(|allowed| allowed == search) {
        return Err("Strategy source anchor was omitted from the edit choices".into());
    }
    let constrained = if protocol == EditProtocol::SearchReplace {
        vec![search.into()]
    } else {
        Vec::new()
    };
    Ok((vec![path.into()], constrained))
}

/// Check the returned bytes, not just the runtime's requested JSON schema.
/// A restarted candidate may edit only its proposed source anchor.
pub(super) fn edit_matches_anchor(
    protocol: EditProtocol,
    raw: &str,
    hunks: &[DiffHunk],
    root: &Path,
    path: &str,
    search: Option<&str>,
    creating: bool,
) -> std::result::Result<(), String> {
    if creating {
        return Ok(());
    }
    let search = search.ok_or("Restart strategy has no source anchor")?;
    if protocol == EditProtocol::SearchReplace {
        let value: Value = serde_json::from_str(raw)
            .map_err(|_| "Restart edit was not structured JSON".to_owned())?;
        if value["path"].as_str() != Some(path) || value["search"].as_str() != Some(search) {
            return Err("Restart edit did not use its proposed exact path and search span".into());
        }
        return Ok(());
    }
    let source = std::fs::read_to_string(root.join(path))
        .map_err(|error| format!("Restart source anchor could not be read: {error}"))?;
    if search.is_empty() || source.matches(search).count() != 1 {
        return Err("Restart source anchor is no longer unique in captured source".into());
    }
    let start_byte = source
        .find(search)
        .ok_or("Restart source anchor disappeared")?;
    let first_line = source[..start_byte]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1;
    let last_line = first_line + search.bytes().filter(|byte| *byte == b'\n').count()
        - usize::from(search.ends_with('\n'));
    let mut removed_anchor_line = false;
    let mut insertion_points = Vec::new();
    for hunk in hunks {
        if hunk.file_path.to_string_lossy().replace('\\', "/") != path {
            return Err("Restart diff changed a path outside its proposed anchor".into());
        }
        let mut old_line = hunk.old_start as usize;
        for line in &hunk.lines {
            match line {
                DiffLine::Context(_) => old_line += 1,
                DiffLine::Removed(_) => {
                    if old_line < first_line || old_line > last_line {
                        return Err(
                            "Restart diff removed source outside its proposed anchor".into()
                        );
                    }
                    removed_anchor_line = true;
                    old_line += 1;
                }
                DiffLine::Added(_) => {
                    insertion_points.push(old_line);
                }
            }
        }
    }
    if insertion_points.iter().any(|point| {
        if removed_anchor_line {
            *point < first_line || *point > last_line + 1
        } else {
            *point <= first_line || *point > last_line
        }
    }) {
        return Err("Restart diff added source outside its proposed anchor".into());
    }
    if !removed_anchor_line && insertion_points.is_empty() {
        return Err("Restart diff did not edit its proposed source anchor".into());
    }
    Ok(())
}

pub(super) fn hypothesis_schema(paths: &[String], searches: &[String], creating: bool) -> Value {
    let mut properties = json!({
        "path": {"type":"string", "enum":paths},
        "mechanism": {"type":"string", "minLength":12, "maxLength":96,
            "description":"One future source edit action, not a summary of rejected candidates"},
        "difference": {"type":"string", "minLength":12, "maxLength":96,
            "description":"How this action differs from the rejected edit, in one short complete phrase"}
    });
    let mut required = vec!["path", "mechanism", "difference"];
    if !creating {
        properties["search"] = json!({"type":"string", "enum":searches});
        required.push("search");
    }
    json!({"type":"object", "properties":properties, "required":required, "additionalProperties":false})
}

/// Model text remains untrusted: exact scope and excerpt anchoring are checked
/// here, while only the eventual canonical candidate and checks judge the edit.
pub(super) fn ground_hypothesis(
    raw: &str,
    paths: &[String],
    searches: &[String],
    excerpts: &[SourceExcerpt],
    creating: bool,
    prior: &[HypothesisEvidence],
) -> std::result::Result<GroundedHypothesis, String> {
    let reply: Value = serde_json::from_str(raw)
        .map_err(|_| "Strategy reply was not the required JSON object".to_owned())?;
    let object = reply
        .as_object()
        .ok_or("Strategy reply was not a JSON object")?;
    let expected = if creating { 3 } else { 4 };
    if object.len() != expected
        || object
            .keys()
            .any(|key| !matches!(key.as_str(), "path" | "search" | "mechanism" | "difference"))
    {
        return Err("Strategy reply did not match the exact required fields".into());
    }
    let path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or("Strategy omitted a path")?
        .to_owned();
    let search = object
        .get("search")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mechanism_raw = object
        .get("mechanism")
        .and_then(Value::as_str)
        .ok_or("Strategy omitted a mechanism")?;
    let difference_raw = object
        .get("difference")
        .and_then(Value::as_str)
        .ok_or("Strategy omitted a difference")?;
    if !paths.contains(&path) {
        return Err("Strategy path is outside the captured edit scope".into());
    }
    if creating {
        if search.is_some() {
            return Err("Creation strategy supplied an unreviewed search span".into());
        }
    } else {
        let search = search
            .as_deref()
            .ok_or("Strategy omitted a source anchor")?;
        if !searches.iter().any(|allowed| allowed == search)
            || !excerpts.iter().any(|excerpt| {
                excerpt.path.to_string_lossy().replace('\\', "/") == path
                    && excerpt.text.contains(search)
            })
        {
            return Err("Strategy anchor is not in an exact captured excerpt for that path".into());
        }
    }
    let clean = |value: &str| -> std::result::Result<String, String> {
        if value.contains(['\r', '\n', '\0']) || value.chars().any(char::is_control) {
            return Err("Strategy contains control characters".into());
        }
        let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
        if !(12..=96).contains(&value.chars().count()) {
            return Err("Strategy must be a short, specific sentence".into());
        }
        let last = value
            .split_whitespace()
            .last()
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(
            last.as_str(),
            "the" | "a" | "an" | "to" | "of" | "and" | "or" | "also" | "with"
        ) || value.ends_with(['`', ':', ','])
        {
            return Err("Strategy phrase appears incomplete".into());
        }
        Ok(value)
    };
    let mechanism = clean(mechanism_raw)?;
    let difference = clean(difference_raw)?;
    let first = mechanism
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    // A closed verb list would stop valid branches for ordinary actions such
    // as "Cache", "Move", or "Update". Reject clear retrospective openings;
    // the returned edit and checks, not this wording, determine success.
    if matches!(
        first.as_str(),
        "the"
            | "a"
            | "an"
            | "this"
            | "that"
            | "previous"
            | "prior"
            | "earlier"
            | "candidate"
            | "rejected"
            | "failed"
            | "failure"
    ) {
        return Err(
            "Strategy mechanism describes a past attempt instead of a proposed edit".into(),
        );
    }
    if prior.iter().any(|item| {
        item.status == HypothesisStatus::Accepted
            && item
                .mechanism
                .as_ref()
                .is_some_and(|old| old.eq_ignore_ascii_case(&mechanism))
            && item.path.as_deref().is_none_or(|old_path| {
                old_path == path
                    && (item.search.is_none() || search.is_none() || item.search == search)
            })
    }) {
        return Err("Strategy repeats a prior accepted hypothesis".into());
    }
    Ok(GroundedHypothesis {
        path,
        search,
        mechanism,
        difference,
    })
}

pub(super) fn next(candidates: &[CandidateEvidence]) -> SearchDecision {
    let choice = |action, parent_candidate, reason: &str| SearchDecision {
        action,
        parent_candidate,
        reason: reason.into(),
    };
    let Some(last) = candidates.last() else {
        return choice(
            SearchAction::Initial,
            None,
            "No candidate has been generated",
        );
    };
    let rejection = last.rejection.as_deref().unwrap_or("");
    if last.stage == CandidateStage::CreationPendingEdit
        && rejection.is_empty()
        && last.content_sha256.is_some()
    {
        return choice(
            SearchAction::Continue,
            Some(last.number),
            "Created bytes are staged; edit the reviewed existing source before verification",
        );
    }
    if rejection.is_empty() {
        return choice(
            SearchAction::Stop,
            None,
            "A candidate is already available for review",
        );
    }
    if rejection.contains("modified the candidate source") {
        return choice(
            SearchAction::Stop,
            None,
            "Verification changed candidate identity; further execution is unsafe",
        );
    }
    if last
        .checks
        .iter()
        .any(|c| c.status == CheckStatus::Unavailable)
    {
        return choice(
            SearchAction::Stop,
            None,
            "Selected verification is unavailable; more generation cannot establish its result",
        );
    }
    if last.content_sha256.is_none()
        && !last.raw_output.trim().is_empty()
        && candidates[..candidates.len() - 1].iter().any(|prior| {
            prior.content_sha256.is_none()
                && prior.raw_output == last.raw_output
                && prior.rejection == last.rejection
        })
    {
        return choice(
            SearchAction::Stop,
            None,
            "The same rejected model output appeared twice without a checkable candidate",
        );
    }
    let repeated = candidates
        .iter()
        .rev()
        .take_while(|c| {
            c.rejection
                .as_deref()
                .is_some_and(|r| r.starts_with("Repeated candidate"))
        })
        .count();
    if repeated >= 2 {
        return choice(
            SearchAction::Stop,
            None,
            "Two successive proposals repeated rejected candidate identities",
        );
    }
    if repeated == 1 {
        return choice(
            SearchAction::Restart,
            None,
            "Candidate identity repeated; abandon this branch and use original source",
        );
    }
    let failed_checks = last.checks.iter().any(|c| c.status == CheckStatus::Failed);
    if failed_checks && last.content_sha256.is_some() {
        if let Some(parent) = last
            .decision
            .as_ref()
            .and_then(|d| d.parent_candidate)
            .and_then(|n| candidates.iter().find(|c| c.number == n))
        {
            if failed_check_signature(parent) == failed_check_signature(last)
                && !gained_passing_check(parent, last)
            {
                return choice(SearchAction::Restart, None, "Repair did not gain a passing check or change failed-check evidence; try a different baseline approach");
            }
        }
        return choice(
            SearchAction::Repair,
            Some(last.number),
            "A stable candidate failed checks; repair its exact bytes using that evidence",
        );
    }
    let parent = last.decision.as_ref().and_then(|d| d.parent_candidate);
    if parent.is_some() && rejection == "Edit makes no change" {
        // A repair that reproduces its parent will reproduce it again; small
        // models need a different approach, not the same prompt twice.
        return choice(
            SearchAction::Restart,
            None,
            "The repair reproduced its parent unchanged; try a different approach from the original source",
        );
    }
    choice(if parent.is_some() { SearchAction::Repair } else { SearchAction::Restart }, parent,
        "Rejected edit did not produce a checkable change; revise it without altering the source scope")
}

fn failures(candidate: &CandidateEvidence) -> String {
    failed_checks_feedback(&candidate.checks)
}

fn feedback_output_excerpt(text: &str, max: usize) -> String {
    const OMITTED: &str = "[...]";
    if text.len() <= max {
        return text.into();
    }
    if max <= OMITTED.len() {
        return super::truncate(text, max);
    }
    // Prompt feedback is far smaller than the saved receipt. A compact marker
    // leaves room for each stream's trailing assertion even with four checks.
    let available = max - OMITTED.len();
    let mut head_end = available / 8;
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len() - (available - head_end);
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!("{}{}{}", &text[..head_end], OMITTED, &text[tail_start..])
}

pub(super) fn failed_checks_feedback(checks: &[CheckEvidence]) -> String {
    let failed: Vec<_> = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Failed)
        .collect();
    if failed.is_empty() {
        return String::new();
    }
    // Retain both output streams independently. Concatenating first can hide
    // a useful stdout tail inside the omitted middle of verbose stderr.
    let available = 450usize.saturating_sub(failed.len().saturating_sub(1));
    let per_check = available / failed.len();
    let remainder = available % failed.len();
    failed
        .into_iter()
        .enumerate()
        .map(|(index, check)| {
            let budget = per_check + usize::from(index < remainder);
            let prefix = format!("{:?}:", check.exit_code);
            if budget <= prefix.len() + 1 {
                return super::truncate(&prefix, budget);
            }
            let stream_budget = budget - prefix.len() - 1;
            let (stdout_budget, stderr_budget) = if check.stdout.is_empty() {
                (0, stream_budget)
            } else if check.stderr.is_empty() {
                (stream_budget, 0)
            } else {
                (stream_budget / 2, stream_budget - stream_budget / 2)
            };
            // TAP keeps the failing assertion far from both ends of the
            // output; a digest of it beats a head/tail slice.
            let stdout = tap_failure_digest(&check.stdout).unwrap_or_else(|| check.stdout.clone());
            format!(
                "{}{}:{}",
                prefix,
                feedback_output_excerpt(&stdout, stdout_budget),
                feedback_output_excerpt(&check.stderr, stderr_budget)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// For TAP output with failures: each `not ok` line with its error fields
/// and the failing test's opening source lines, then the pass/fail counts.
/// Passing cases, timings and stacks are dropped. `None` for other output.
fn tap_failure_digest(stdout: &str) -> Option<String> {
    if !stdout
        .lines()
        .any(|l| l.trim_start().starts_with("not ok "))
    {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    let mut lines = stdout.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.starts_with("# pass ") || trimmed.starts_with("# fail ") {
            out.push(trimmed.to_string());
        }
        if !trimmed.starts_with("not ok ") {
            continue;
        }
        out.push(trimmed.to_string());
        let mut location = None;
        while let Some(next) = lines.peek() {
            let t = next.trim();
            if t.starts_with("ok ") || t.starts_with("not ok ") || t.starts_with("# ") {
                break;
            }
            let next = lines.next().unwrap_or_default().trim();
            for key in ["error:", "expected:", "actual:", "operator:"] {
                if next.starts_with(key) {
                    out.push(format!("  {}", super::truncate(next, 120)));
                }
            }
            if next.contains("Missing expected exception") {
                out.push("  meaning: the asserted call returned normally; it must throw".into());
            }
            if let Some(raw) = next.strip_prefix("location:") {
                location = Some(raw.trim().trim_matches('\'').replace("\\\\", "\\"));
            }
        }
        if let Some(source) = location.as_deref().and_then(failing_test_source) {
            out.push(source);
        }
    }
    Some(out.join("\n"))
}

/// `file.js:14` and the three source lines from that location.
fn failing_test_source(location: &str) -> Option<String> {
    let mut parts = location.rsplitn(3, ':');
    let _column = parts.next()?;
    let line: usize = parts.next()?.parse().ok()?;
    let path = std::path::Path::new(parts.next()?);
    if std::fs::metadata(path).ok()?.len() > 256 * 1024 {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let body: Vec<String> = text
        .lines()
        .skip(line.saturating_sub(1))
        .take(3)
        .map(|l| format!("  | {}", super::truncate(l.trim_end(), 100)))
        .collect();
    let name = path.file_name()?.to_string_lossy();
    Some(format!("  {name}:{line}\n{}", body.join("\n")))
}

// This is a search heuristic only. Bounded output excerpts remain in the
// receipt; normalized diagnostics never turn a failed check into a pass.
#[derive(PartialEq, Eq)]
struct FailedCheckEvidence<'a> {
    index: usize,
    purpose: CheckPurpose,
    check: Option<(&'a str, &'a [String])>,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn failed_check_signature(candidate: &CandidateEvidence) -> Vec<FailedCheckEvidence<'_>> {
    candidate
        .checks
        .iter()
        .enumerate()
        .filter(|(_, check)| check.status == CheckStatus::Failed)
        .map(|(index, check)| FailedCheckEvidence {
            index,
            purpose: check.purpose,
            check: check
                .check
                .as_ref()
                .map(|command| (command.program.as_str(), command.args.as_slice())),
            exit_code: check.exit_code,
            stdout: comparison_output(&check.stdout, &candidate.directory),
            stderr: comparison_output(&check.stderr, &candidate.directory),
        })
        .collect()
}

fn gained_passing_check(parent: &CandidateEvidence, child: &CandidateEvidence) -> bool {
    parent
        .checks
        .iter()
        .zip(&child.checks)
        .any(|(before, after)| {
            before.purpose == after.purpose
                && before
                    .check
                    .as_ref()
                    .map(|check| (&check.program, &check.args))
                    == after
                        .check
                        .as_ref()
                        .map(|check| (&check.program, &check.args))
                && before.status != CheckStatus::Passed
                && after.status == CheckStatus::Passed
        })
}

fn comparison_output(output: &str, directory: &std::path::Path) -> String {
    let root = directory.to_string_lossy();
    let output = if root.is_empty() {
        output.to_owned()
    } else {
        let json_escaped = root.replace('\\', "\\\\");
        output
            .replace(&json_escaped, "<candidate>")
            .replace(root.as_ref(), "<candidate>")
            .replace(&root.replace('\\', "/"), "<candidate>")
    };
    let duration = |s: &str| s.parse::<f64>().is_ok_and(|n| n.is_finite() && n >= 0.0);
    output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            for prefix in ["ℹ duration_ms ", "# duration_ms ", "duration_ms: "] {
                if trimmed.strip_prefix(prefix).is_some_and(duration) {
                    return None;
                }
            }
            if trimmed.starts_with(['✔', '✖'])
                || trimmed.starts_with("ok ")
                || trimmed.starts_with("not ok ")
            {
                if let Some((prefix, elapsed)) =
                    line.strip_suffix("ms)").and_then(|s| s.rsplit_once('('))
                {
                    if prefix.ends_with(' ') && duration(elapsed) {
                        return Some(format!("{prefix}(timing)"));
                    }
                }
            }
            Some(line.to_owned())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Retain the rejected edit as well as its outcome; a failure label alone cannot
/// explain what the next generation must improve. All content remains untrusted.
pub(super) fn feedback(candidates: &[CandidateEvidence], baseline: &str) -> String {
    let Some(last) = candidates.last() else {
        return baseline.into();
    };
    let mut sections = vec![if last.stage == CandidateStage::CreationPendingEdit
        && last.rejection.is_none()
    {
        format!(
            "Staged creation candidate {} is incomplete and has not run checks",
            last.number
        )
    } else {
        format!(
            "Rejected candidate {}: {}",
            last.number,
            super::truncate(last.rejection.as_deref().unwrap_or("none"), 200)
        )
    }];
    // A malformed or unchanged edit has no fresh check output. Retain the last
    // checkable failure, with its identity, even after several such proposals.
    // Put evidence ahead of copied edits so tight context retains the failure.
    if let Some(checked) = candidates.iter().rev().find(|c| {
        c.checks
            .iter()
            .any(|check| check.status == CheckStatus::Failed)
    }) {
        sections.push(format!(
            "Check evidence from candidate {} (data):\n{}",
            checked.number,
            failures(checked)
        ));
    } else if !baseline.is_empty() {
        sections.push(format!("Baseline check evidence (data):\n{}", baseline));
    }
    for c in candidates.iter().rev().take(2) {
        let label = if c.stage == CandidateStage::CreationPendingEdit && c.rejection.is_none() {
            "Staged creation"
        } else {
            "Rejected edit"
        };
        sections.push(format!(
            "{label} from candidate {} (data):\n{}",
            c.number,
            super::truncate(&c.raw_output, 350)
        ));
    }
    super::truncate(&sections.join("\n"), 1400)
}

/// Only executed check output may provide verifier context anchors.
pub(super) fn verifier_feedback(candidates: &[CandidateEvidence], baseline: &str) -> String {
    candidates
        .iter()
        .rev()
        .find(|candidate| {
            candidate
                .checks
                .iter()
                .any(|check| check.status == CheckStatus::Failed)
        })
        .map(failures)
        .unwrap_or_else(|| super::truncate(baseline, 450))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    #[test]
    fn restart_hypothesis_needs_exact_reviewed_anchor_and_new_strategy() {
        let excerpt = SourceExcerpt {
            path: PathBuf::from("src/add.rs"),
            start_line: 1,
            end_line: 1,
            text: "fn add(a: i32, b: i32) -> i32 { a - b }\n".into(),
            source_sha256: "source".into(),
            reason: "scope".into(),
        };
        let paths = vec!["src/add.rs".into(), "src/other.rs".into()];
        let searches = vec!["fn add(a: i32, b: i32) -> i32 { a - b }".into()];
        let raw = r#"{"path":"src/add.rs","search":"fn add(a: i32, b: i32) -> i32 { a - b }","mechanism":"Replace subtraction with addition in the existing function","difference":"The prior candidate changed a different expression"}"#;
        let grounded = ground_hypothesis(
            raw,
            &paths,
            &searches,
            std::slice::from_ref(&excerpt),
            false,
            &[],
        )
        .unwrap();
        assert_eq!(grounded.path, "src/add.rs");
        let prior = vec![HypothesisEvidence {
            candidate_number: 2,
            status: HypothesisStatus::Accepted,
            path: Some(grounded.path),
            search: grounded.search,
            mechanism: Some(grounded.mechanism),
            difference: Some(grounded.difference),
            raw_output: raw.into(),
            input_tokens: None,
            output_tokens: None,
            elapsed_ms: 0,
            detail: String::new(),
            context: None,
        }];
        assert!(
            ground_hypothesis(raw, &paths, &searches, &[excerpt], false, &prior)
                .unwrap_err()
                .contains("repeats")
        );
        let wrong_path = raw.replace("src/add.rs", "src/other.rs");
        assert!(
            ground_hypothesis(&wrong_path, &paths, &searches, &[], false, &[])
                .unwrap_err()
                .contains("anchor")
        );
        let outside = raw.replace("src/add.rs", "test/add.rs");
        assert!(
            ground_hypothesis(&outside, &paths, &searches, &[], false, &[])
                .unwrap_err()
                .contains("scope")
        );
    }

    #[test]
    fn restart_hypothesis_reuses_mechanism_at_independent_anchor() {
        let excerpts = [
            SourceExcerpt {
                path: PathBuf::from("src/a.rs"),
                start_line: 1,
                end_line: 2,
                text: "fn parse_a(input: &str) {}\nfn parse_other(input: &str) {}\n".into(),
                source_sha256: "a".into(),
                reason: "scope".into(),
            },
            SourceExcerpt {
                path: PathBuf::from("src/b.rs"),
                start_line: 1,
                end_line: 1,
                text: "fn parse_b(input: &str) {}\n".into(),
                source_sha256: "b".into(),
                reason: "scope".into(),
            },
        ];
        let paths = vec!["src/a.rs".into(), "src/b.rs".into()];
        let searches = vec![
            "fn parse_a(input: &str) {}".into(),
            "fn parse_other(input: &str) {}".into(),
            "fn parse_b(input: &str) {}".into(),
        ];
        let mechanism = "Guard empty input before parsing";
        let prior = [HypothesisEvidence {
            candidate_number: 2,
            status: HypothesisStatus::Accepted,
            path: Some("src/a.rs".into()),
            search: Some("fn parse_a(input: &str) {}".into()),
            mechanism: Some(mechanism.into()),
            difference: Some("The previous edit changed another expression".into()),
            raw_output: String::new(),
            input_tokens: None,
            output_tokens: None,
            elapsed_ms: 0,
            detail: String::new(),
            context: None,
        }];
        let reply = |path: &str, search: &str| {
            json!({
                "path": path,
                "search": search,
                "mechanism": mechanism,
                "difference": "This proposal targets a separate source location",
            })
            .to_string()
        };

        let other_file = reply("src/b.rs", "fn parse_b(input: &str) {}");
        assert!(
            ground_hypothesis(&other_file, &paths, &searches, &excerpts, false, &prior).is_ok()
        );
        let other_anchor = reply("src/a.rs", "fn parse_other(input: &str) {}");
        assert!(
            ground_hypothesis(&other_anchor, &paths, &searches, &excerpts, false, &prior).is_ok()
        );
        let same_anchor = reply("src/a.rs", "fn parse_a(input: &str) {}");
        assert!(
            ground_hypothesis(&same_anchor, &paths, &searches, &excerpts, false, &prior)
                .unwrap_err()
                .contains("repeats")
        );
    }
    #[test]
    fn failure_summary_is_not_mistaken_for_a_new_mechanism() {
        let reply = r#"{"path":"new.py","mechanism":"The rejected candidate 2 attempted parseFloat and also","difference":"Candidate 1 attempted trim and also"}"#;
        assert!(ground_hypothesis(reply, &["new.py".into()], &[], &[], true, &[]).is_err());
        let valid = r#"{"path":"new.py","mechanism":"Validate only decimal digits before number conversion","difference":"Earlier edits parsed a numeric prefix without full-string validation"}"#;
        assert!(ground_hypothesis(valid, &["new.py".into()], &[], &[], true, &[]).is_ok());
        let domain_terms = r#"{"path":"new.py","mechanism":"Validate candidate state before applying changes","difference":"Earlier edits skipped state validation"}"#;
        assert!(ground_hypothesis(domain_terms, &["new.py".into()], &[], &[], true, &[]).is_ok());
        for mechanism in [
            "Cache parsed results before repeated lookup",
            "Update the previous-state field on transition",
            "Move normalization ahead of validation",
        ] {
            let reply = serde_json::json!({
                "path": "new.py",
                "mechanism": mechanism,
                "difference": "Earlier edits skipped this operation"
            });
            assert!(
                ground_hypothesis(&reply.to_string(), &["new.py".into()], &[], &[], true, &[])
                    .is_ok()
            );
        }
        let retrospective = r#"{"path":"new.py","mechanism":"The failed edit parsed only a numeric prefix","difference":"Earlier edits skipped full validation"}"#;
        assert!(
            ground_hypothesis(retrospective, &["new.py".into()], &[], &[], true, &[])
                .unwrap_err()
                .contains("past attempt")
        );
        let earlier = r#"{"path":"new.py","mechanism":"Earlier edit parsed only a numeric prefix","difference":"Another edit skipped full validation"}"#;
        assert!(ground_hypothesis(earlier, &["new.py".into()], &[], &[], true, &[]).is_err());
    }
    #[test]
    fn restart_reserves_both_model_calls_from_one_ledger() {
        assert!(inference_budget_allows(&SearchAction::Restart, 1, 3, 256));
        assert!(!inference_budget_allows(&SearchAction::Restart, 2, 3, 4096));
        assert!(!inference_budget_allows(&SearchAction::Restart, 1, 3, 255));
        assert!(inference_budget_allows(&SearchAction::Repair, 2, 3, 128));
        assert!(!inference_budget_allows(&SearchAction::Repair, 3, 3, 4096));
    }
    #[test]
    fn unmaterialized_edit_uses_one_remaining_call_without_weakening_restart_or_stop() {
        let mut malformed = failed(1, None, "");
        malformed.content_sha256 = None;
        malformed.checks.clear();
        malformed.raw_output = "not-json".into();
        malformed.rejection = Some("Invalid edit JSON".into());
        let prior = std::slice::from_ref(&malformed);
        assert_eq!(next(prior).action, SearchAction::Restart);
        assert_eq!(
            next_with_budget(prior, 1, 2, 512, false).action,
            SearchAction::Initial
        );
        assert_eq!(
            next_with_budget(prior, 1, 3, 512, false).action,
            SearchAction::Restart
        );
        assert_eq!(
            next_with_budget(prior, 1, 2, 512, true).action,
            SearchAction::Restart
        );
        assert_eq!(
            next_with_budget(prior, 1, 2, 127, false).action,
            SearchAction::Restart
        );

        let mut hashed = malformed.clone();
        hashed.content_sha256 = Some("candidate bytes".into());
        assert_eq!(
            next_with_budget(&[hashed], 1, 2, 512, false).action,
            SearchAction::Restart
        );
        let duplicate = vec![
            malformed.clone(),
            CandidateEvidence {
                number: 2,
                ..malformed
            },
        ];
        assert_eq!(
            next_with_budget(&duplicate, 2, 3, 512, false).action,
            SearchAction::Stop
        );
    }
    #[test]
    fn restart_edit_choices_bind_the_proposed_path_and_exact_search() {
        let paths = vec!["src/add.rs".into(), "src/other.rs".into()];
        let searches = vec!["wrong line".into(), "a - b".into()];
        let (paths, searches) = bound_edit_choices(
            "src/add.rs",
            Some("a - b"),
            false,
            EditProtocol::SearchReplace,
            &paths,
            &searches,
        )
        .unwrap();
        assert_eq!(paths, ["src/add.rs"]);
        assert_eq!(searches, ["a - b"]);
        assert!(bound_edit_choices(
            "src/add.rs",
            Some("omitted"),
            false,
            EditProtocol::SearchReplace,
            &paths,
            &searches,
        )
        .is_err());
    }
    #[test]
    fn restart_checks_actual_edit_even_if_runtime_ignores_schema() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("add.rs"), "first\nsecond\nthird\n").unwrap();
        let wrong_json = r#"{"path":"add.rs","search":"second","text":"replacement"}"#;
        assert!(edit_matches_anchor(
            EditProtocol::SearchReplace,
            wrong_json,
            &[],
            root.path(),
            "add.rs",
            Some("first"),
            false,
        )
        .unwrap_err()
        .contains("exact path and search"));
        let wrong_hunk = DiffHunk {
            file_path: "add.rs".into(),
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 2,
            lines: vec![
                DiffLine::Context("first".into()),
                DiffLine::Removed("second".into()),
                DiffLine::Added("changed".into()),
            ],
        };
        assert!(edit_matches_anchor(
            EditProtocol::UnifiedDiff,
            "",
            &[wrong_hunk],
            root.path(),
            "add.rs",
            Some("first"),
            false,
        )
        .unwrap_err()
        .contains("outside its proposed anchor"));
        let adjacent_insertion = DiffHunk {
            file_path: "add.rs".into(),
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 2,
            lines: vec![
                DiffLine::Context("first".into()),
                DiffLine::Added("injected".into()),
            ],
        };
        assert!(edit_matches_anchor(
            EditProtocol::UnifiedDiff,
            "",
            &[adjacent_insertion],
            root.path(),
            "add.rs",
            Some("first"),
            false,
        )
        .unwrap_err()
        .contains("outside its proposed anchor"));
    }
    fn failed(number: u32, parent: Option<u32>, output: &str) -> CandidateEvidence {
        CandidateEvidence {
            number,
            stage: CandidateStage::Complete,
            approach: String::new(),
            directory: "candidate".into(),
            content_sha256: Some(format!("hash-{number}")),
            raw_output: "rejected edit".into(),
            input_tokens: None,
            output_tokens: None,
            elapsed_ms: 0,
            checks: vec![CheckEvidence {
                check: None,
                purpose: CheckPurpose::Verification,
                status: CheckStatus::Failed,
                exit_code: Some(1),
                stdout: output.into(),
                stderr: String::new(),
                detail: String::new(),
                elapsed_ms: 0,
            }],
            rejection: Some("Candidate failed the selected verification checks".into()),
            diff: String::new(),
            context: None,
            decision: Some(SearchDecision {
                action: if parent.is_some() {
                    SearchAction::Repair
                } else {
                    SearchAction::Initial
                },
                parent_candidate: parent,
                reason: String::new(),
            }),
        }
    }
    #[test]
    fn noop_repair_restarts_instead_of_repeating() {
        let mut noop = failed(2, Some(1), "");
        noop.checks.clear();
        noop.content_sha256 = None;
        noop.rejection = Some("Edit makes no change".into());
        let decision = next(&[failed(1, None, "not ok 3 - rejects trailing garbage"), noop]);
        assert_eq!(decision.action, SearchAction::Restart);
        assert_eq!(decision.parent_candidate, None);
    }
    #[test]
    fn repair_uses_parent_and_unchanged_evidence_restarts() {
        assert_eq!(next(&[]).action, SearchAction::Initial);
        let first = failed(1, None, "test A failed");
        assert_eq!(next(std::slice::from_ref(&first)).parent_candidate, Some(1));
        let repaired = failed(2, Some(1), "test A failed");
        assert_eq!(
            next(&[first.clone(), repaired]).action,
            SearchAction::Restart
        );
        let progress = failed(2, Some(1), "test B failed");
        assert_eq!(next(&[first, progress]).parent_candidate, Some(2));
    }

    #[test]
    fn repair_that_moves_a_failure_to_another_check_is_progress() {
        let named = |program: &str, status: CheckStatus| {
            let failed = status == CheckStatus::Failed;
            CheckEvidence {
                check: Some(LocalCheck {
                    program: program.into(),
                    args: Vec::new(),
                }),
                purpose: CheckPurpose::Verification,
                status,
                exit_code: Some(if failed { 1 } else { 0 }),
                stdout: if failed {
                    "generic failure".into()
                } else {
                    "ok".into()
                },
                stderr: String::new(),
                detail: String::new(),
                elapsed_ms: 0,
            }
        };
        let mut first = failed(1, None, "generic failure");
        first.checks = vec![
            named("check-a", CheckStatus::Failed),
            named("check-b", CheckStatus::Passed),
        ];
        let mut repaired = failed(2, Some(1), "generic failure");
        repaired.checks = vec![
            named("check-a", CheckStatus::Passed),
            named("check-b", CheckStatus::Failed),
        ];
        let decision = next(&[first, repaired]);
        assert_eq!(decision.action, SearchAction::Repair);
        assert_eq!(decision.parent_candidate, Some(2));
    }

    #[test]
    fn repair_that_turns_another_check_into_a_pass_is_progress() {
        let mut first = failed(1, None, "generic failure");
        first.checks[0].check = Some(LocalCheck {
            program: "check-a".into(),
            args: Vec::new(),
        });
        let mut second_check = first.checks[0].clone();
        second_check.check = Some(LocalCheck {
            program: "check-b".into(),
            args: Vec::new(),
        });
        second_check.status = CheckStatus::NotRun;
        second_check.exit_code = Some(0);
        second_check.stdout.clear();
        first.checks.push(second_check.clone());

        let mut repaired = failed(2, Some(1), "generic failure");
        repaired.checks[0].check = first.checks[0].check.clone();
        second_check.status = CheckStatus::Passed;
        repaired.checks.push(second_check);

        let decision = next(&[first, repaired]);
        assert_eq!(decision.action, SearchAction::Repair);
        assert_eq!(decision.parent_candidate, Some(2));
    }

    #[test]
    fn repair_that_loses_a_pass_without_changing_the_failure_restarts() {
        let mut first = failed(1, None, "generic failure");
        first.checks[0].check = Some(LocalCheck {
            program: "check-a".into(),
            args: Vec::new(),
        });
        let mut second_check = first.checks[0].clone();
        second_check.check = Some(LocalCheck {
            program: "check-b".into(),
            args: Vec::new(),
        });
        second_check.status = CheckStatus::Passed;
        second_check.exit_code = Some(0);
        second_check.stdout.clear();
        first.checks.push(second_check.clone());

        let mut repaired = failed(2, Some(1), "generic failure");
        repaired.checks[0].check = first.checks[0].check.clone();
        second_check.status = CheckStatus::NotRun;
        repaired.checks.push(second_check);

        assert_eq!(next(&[first, repaired]).action, SearchAction::Restart);
    }

    #[test]
    fn distinct_stdout_and_stderr_remain_distinct_search_evidence() {
        let mut first = failed(1, None, "a:b");
        first.checks[0].stderr = "c".into();
        let mut repaired = failed(2, Some(1), "a");
        repaired.checks[0].stderr = "b:c".into();

        let decision = next(&[first, repaired]);
        assert_eq!(decision.action, SearchAction::Repair);
        assert_eq!(decision.parent_candidate, Some(2));
    }

    #[test]
    fn staged_creation_requires_a_child_before_review() {
        let mut staged = failed(1, None, "");
        staged.stage = CandidateStage::CreationPendingEdit;
        staged.rejection = None;
        staged.checks.clear();
        let decision = next(&[staged]);
        assert_eq!(decision.action, SearchAction::Continue);
        assert_eq!(decision.parent_candidate, Some(1));
        assert!(inference_budget_allows(&decision.action, 1, 2, 128));
        assert!(!inference_budget_allows(&decision.action, 1, 1, 128));
    }

    #[test]
    fn repeated_unmaterialized_output_stops_inference() {
        let mut first = failed(1, None, "");
        first.content_sha256 = None;
        first.checks.clear();
        first.raw_output = r#"{"path":"src/new.mjs","text":"value"}"#.into();
        first.rejection = Some("Invalid new-file output".into());
        assert_eq!(
            next(std::slice::from_ref(&first)).action,
            SearchAction::Restart
        );
        let mut second = first.clone();
        second.number = 2;
        assert_eq!(
            next(&[first.clone(), second.clone()]).action,
            SearchAction::Stop
        );
        second.content_sha256 = Some("checkable bytes".into());
        assert_ne!(next(&[first, second]).action, SearchAction::Stop);
    }
    #[test]
    fn unchanged_node_failure_ignores_candidate_location_and_timing() {
        let mut first = failed(
            1,
            None,
            "✖ C:\\runs\\candidate-1\\test_sum.js (31.123ms)\n3 !== 6\nℹ duration_ms 40.5\n",
        );
        first.directory = "C:\\runs\\candidate-1".into();
        let mut repaired = failed(
            2,
            Some(1),
            "✖ C:\\runs\\candidate-2\\test_sum.js (28.321ms)\n3 !== 6\nℹ duration_ms 37.2\n",
        );
        repaired.directory = "C:\\runs\\candidate-2".into();
        assert_eq!(
            next(&[first.clone(), repaired.clone()]).action,
            SearchAction::Restart
        );
        repaired.checks[0].stdout = repaired.checks[0].stdout.replace("3 !== 6", "4 !== 6");
        assert_eq!(next(&[first, repaired]).action, SearchAction::Repair);
    }
    #[test]
    fn unchanged_json_escaped_windows_path_restarts_stagnant_repair() {
        let first_path = r"C:\runs\candidate-1";
        let second_path = r"C:\runs\candidate-2";
        let mut first = failed(
            1,
            None,
            &serde_json::json!({"cwd": first_path, "error": "same assertion"}).to_string(),
        );
        first.directory = first_path.into();
        let mut repaired = failed(
            2,
            Some(1),
            &serde_json::json!({"cwd": second_path, "error": "same assertion"}).to_string(),
        );
        repaired.directory = second_path.into();
        assert_eq!(
            next(&[first.clone(), repaired.clone()]).action,
            SearchAction::Restart
        );
        repaired.checks[0].stdout =
            serde_json::json!({"cwd": second_path, "error": "different assertion"}).to_string();
        assert_eq!(next(&[first, repaired]).action, SearchAction::Repair);
    }
    #[test]
    fn verbose_failure_tail_survives_capture_and_drives_repair() {
        let prefix = "building fixture dependency\n".repeat(900);
        let first_log = format!("{prefix}FAIL test_invalid_port: expected rejection, got 2\n");
        let repaired_log = format!("{prefix}FAIL test_invalid_port: expected rejection, got 1\n");
        let first = failed(
            1,
            None,
            &super::super::check_output_excerpt(&first_log, 16_384),
        );
        let repaired = failed(
            2,
            Some(1),
            &super::super::check_output_excerpt(&repaired_log, 16_384),
        );
        assert!(first.checks[0].stdout.len() <= 16_384);
        assert!(repaired.checks[0].stdout.len() <= 16_384);
        assert_eq!(
            next(&[first.clone(), repaired.clone()]).action,
            SearchAction::Repair
        );
        assert!(feedback(std::slice::from_ref(&repaired), "")
            .contains("test_invalid_port: expected rejection, got 1"));
        assert!(verifier_feedback(&[repaired], "")
            .contains("test_invalid_port: expected rejection, got 1"));
    }
    #[test]
    fn bounded_feedback_keeps_each_failing_check_tail() {
        let prefix = "setup noise\n".repeat(500);
        let mut candidate = failed(1, None, &format!("{prefix}FAIL parse_port assertion A\n"));
        let mut second = candidate.checks[0].clone();
        second.stdout = format!("{prefix}FAIL validate_host assertion B\n");
        candidate.checks.push(second);
        let excerpt = verifier_feedback(std::slice::from_ref(&candidate), "");
        assert!(excerpt.len() <= 450);
        assert!(excerpt.contains("FAIL parse_port assertion A"));
        assert!(excerpt.contains("FAIL validate_host assertion B"));
        let prompt = feedback(&[candidate], "");
        assert!(prompt.contains("FAIL parse_port assertion A"));
        assert!(prompt.contains("FAIL validate_host assertion B"));
    }
    #[test]
    fn feedback_keeps_stdout_failure_when_stderr_is_also_verbose() {
        let mut candidate = failed(1, None, "");
        candidate.checks[0].stdout = super::super::check_output_excerpt(
            &format!(
                "{}FAIL invalid_port expected rejection\n",
                "setup stdout\n".repeat(1700)
            ),
            16_384,
        );
        candidate.checks[0].stderr = super::super::check_output_excerpt(
            &format!(
                "{}warning: package cache unavailable\n",
                "warning stderr\n".repeat(1700)
            ),
            16_384,
        );
        let prompt = feedback(std::slice::from_ref(&candidate), "");
        let verifier = verifier_feedback(&[candidate], "");
        for excerpt in [&prompt, &verifier] {
            assert!(excerpt.contains("FAIL invalid_port expected rejection"));
            assert!(excerpt.contains("warning: package cache unavailable"));
        }
        assert!(verifier.len() <= 450);
    }
    #[test]
    fn four_verbose_failures_keep_each_stream_tail() {
        let mut candidate = failed(1, None, "");
        candidate.checks.clear();
        for number in 1..=4 {
            let mut check = failed(number, None, "").checks.remove(0);
            check.stdout = super::super::check_output_excerpt(
                &format!(
                    "{}FAIL case_{number} assertion\n",
                    "setup stdout\n".repeat(1700)
                ),
                16_384,
            );
            check.stderr = super::super::check_output_excerpt(
                &format!(
                    "{}WARN stream_{number} end\n",
                    "warning stderr\n".repeat(1700)
                ),
                16_384,
            );
            candidate.checks.push(check);
        }
        let prompt = feedback(std::slice::from_ref(&candidate), "");
        let verifier = verifier_feedback(&[candidate], "");
        assert!(verifier.len() <= 450);
        for number in 1..=4 {
            for excerpt in [&prompt, &verifier] {
                assert!(excerpt.contains(&format!("FAIL case_{number} assertion")));
                assert!(excerpt.contains(&format!("WARN stream_{number} end")));
            }
        }
    }
    #[test]
    fn tap_digest_keeps_the_failing_assertion_and_its_source() {
        let dir = tempfile::tempdir().unwrap();
        let test_file = dir.path().join("port.test.js");
        std::fs::write(
            &test_file,
            "import test from 'node:test';\n\ntest('rejects trailing garbage', () => {\n  assert.throws(() => parsePort('80abc'));\n});\n",
        )
        .unwrap();
        let location = format!("{}:3:1", test_file.display()).replace('\\', "\\\\");
        let tap = format!(
            "TAP version 13\nok 1 - parses\n  ---\n  duration_ms: 1.6\n  ...\nnot ok 2 - rejects trailing garbage\n  ---\n  duration_ms: 0.9\n  location: '{location}'\n  error: 'Missing expected exception.'\n  operator: 'throws'\n  stack: |-\n    at x\n  ...\n1..2\n# pass 1\n# fail 1\n# duration_ms 161.8\n"
        );
        let digest = tap_failure_digest(&tap).unwrap();
        assert!(
            digest.contains("not ok 2 - rejects trailing garbage"),
            "{digest}"
        );
        assert!(
            digest.contains("error: 'Missing expected exception.'"),
            "{digest}"
        );
        assert!(digest.contains("must throw"), "{digest}");
        assert!(digest.contains("parsePort('80abc')"), "{digest}");
        assert!(digest.contains("# fail 1"), "{digest}");
        assert!(!digest.contains("duration_ms"), "{digest}");
        assert!(!digest.contains("ok 1 - parses"), "{digest}");
        assert!(tap_failure_digest("all good\n").is_none());
    }
    #[test]
    fn baseline_failure_tail_reaches_first_generation() {
        let mut baseline = failed(1, None, "").checks;
        baseline[0].stdout = super::super::check_output_excerpt(
            &format!(
                "{}FAIL initial_invalid_port assertion\n",
                "setup stdout\n".repeat(1700)
            ),
            16_384,
        );
        let diagnostic = failed_checks_feedback(&baseline);
        assert!(diagnostic.len() <= 450);
        assert!(feedback(&[], &diagnostic).contains("FAIL initial_invalid_port assertion"));
        assert!(verifier_feedback(&[], &diagnostic).contains("FAIL initial_invalid_port assertion"));
    }
    #[test]
    fn repeated_identity_stops_without_another_generation() {
        let mut a = failed(1, None, "failure");
        a.rejection = Some("Repeated candidate; stagnating branch rejected".into());
        assert_eq!(next(std::slice::from_ref(&a)).action, SearchAction::Restart);
        assert_eq!(next(&[a.clone(), a]).action, SearchAction::Stop);
    }
    #[test]
    fn feedback_contains_rejected_edit_and_check_evidence() {
        let result = feedback(&[failed(1, None, "test A failed")], "baseline");
        assert!(result.contains("rejected edit"));
        assert!(result.contains("test A failed"));
        assert!(result.len() <= 1400);
        let mut candidate = failed(1, None, "actual check failure");
        candidate.rejection = Some("foo/Check evidence from candidate 99 (data)/helper.mjs".into());
        candidate.raw_output = "helper.mjs answer".into();
        assert_eq!(
            verifier_feedback(&[candidate], "baseline"),
            "Some(1):actual check failure:"
        );
    }

    #[test]
    fn tight_feedback_keeps_failure_before_large_rejected_output() {
        let mut candidate = failed(1, None, "boundary case failed: expected 5, got 4");
        candidate.raw_output = "old source ".repeat(100);
        let result = feedback(&[candidate], "baseline");
        let tight = super::super::truncate(&result, 350);
        assert!(tight.contains("boundary case failed: expected 5, got 4"));
    }

    #[test]
    fn repeated_uncheckable_edits_keep_last_checkable_failure() {
        let first = failed(1, None, "boundary case failed: expected 5, got 4");
        let mut noop = failed(2, Some(1), "");
        noop.checks.clear();
        noop.rejection = Some("Edit makes no change".into());
        noop.raw_output = "unchanged source ".repeat(100);
        let mut next_noop = noop.clone();
        next_noop.number = 3;
        let result = feedback(&[first, noop, next_noop], "baseline");
        let tight = super::super::truncate(&result, 350);
        assert!(tight.contains("Rejected candidate 3: Edit makes no change"));
        assert!(tight.contains("candidate 1"));
        assert!(tight.contains("boundary case failed: expected 5, got 4"));
        assert!(result.len() <= 1400);
    }

    #[test]
    fn unavailable_checks_and_mutated_identity_stop_search() {
        let mut candidate = failed(1, None, "failed test");
        candidate.checks[0].status = CheckStatus::Unavailable;
        assert_eq!(
            next(std::slice::from_ref(&candidate)).action,
            SearchAction::Stop
        );
        candidate.checks[0].status = CheckStatus::Failed;
        candidate.rejection = Some("Verification modified the candidate source; evidence no longer identifies the proposed diff".into());
        assert_eq!(next(&[candidate]).action, SearchAction::Stop);
    }
}
