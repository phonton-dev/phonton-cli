//! TUI goals on a calibrated local model.
//!
//! Small local models are reliable with the search/replace protocol their
//! calibration measured and unreliable with exact unified diffs, so when the
//! TUI runs on this machine with a calibrated model, goals go through the
//! local harness: edits land in copies, the project's checks run there, and
//! the working tree changes only when the user applies the candidate.

use phonton_types::local::CheckStatus;
use phonton_types::local_run::LocalRunReceipt;
use phonton_types::{
    ChangedFileSummary, CostReceipt, DiffStats, GlobalState, HandoffPacket, ModelTier, RunCommand,
    SubtaskId, SubtaskStatus, TaskId, TaskStatus, TokenUsage, VerifyReport, WorkerState,
};

/// Loop stage for a receipt state (see `art::STAGES`).
pub fn stage(receipt: &LocalRunReceipt) -> usize {
    let s = receipt.state.as_str();
    if s.starts_with("verifying") || s == "finalizing" {
        3
    } else if s.starts_with("review") {
        4
    } else {
        2
    }
}

/// True once the harness has stopped (with or without a candidate).
pub fn settled(receipt: &LocalRunReceipt) -> bool {
    let s = receipt.state.as_str();
    !(s == "baseline"
        || s == "finalizing"
        || s.starts_with("generating")
        || s.starts_with("verifying")
        || s.starts_with("hypothesizing")
        || s == "awaiting_existing_edit")
}

/// Per-file `+/-` counts from a unified diff.
pub fn diff_stats(diff: &str) -> (Vec<ChangedFileSummary>, DiffStats) {
    let mut files: Vec<ChangedFileSummary> = Vec::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            let path = path.trim_start_matches("b/");
            if path != "/dev/null" {
                files.push(ChangedFileSummary {
                    path: path.into(),
                    added_lines: 0,
                    removed_lines: 0,
                    summary: String::new(),
                });
            }
        } else if line.starts_with("---") {
        } else if let Some(file) = files.last_mut() {
            if line.starts_with('+') {
                file.added_lines += 1;
            } else if line.starts_with('-') {
                file.removed_lines += 1;
            }
        }
    }
    let stats = DiffStats {
        files_changed: files.len() as u32,
        added_lines: files.iter().map(|f| f.added_lines).sum(),
        removed_lines: files.iter().map(|f| f.removed_lines).sum(),
    };
    (files, stats)
}

fn check_label(check: &phonton_types::local_run::CheckEvidence) -> String {
    check
        .check
        .as_ref()
        .map(|c| {
            std::iter::once(c.program.as_str())
                .chain(c.args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| "check".into())
}

/// Tokens the runtime reported across every candidate.
pub fn tokens(receipt: &LocalRunReceipt) -> (u64, u64) {
    receipt.candidates.iter().fold((0, 0), |(i, o), c| {
        (
            i + c.input_tokens.unwrap_or(0),
            o + c.output_tokens.unwrap_or(0),
        )
    })
}

/// Translate a (possibly intermediate) receipt into the TUI's state model.
pub fn global_state(receipt: &LocalRunReceipt, task_id: TaskId, worker: SubtaskId) -> GlobalState {
    let (tin, tout) = tokens(receipt);
    let selected = receipt
        .selected_candidate
        .and_then(|n| receipt.candidates.iter().find(|c| c.number == n));
    let mut state = GlobalState {
        task_status: TaskStatus::Running {
            active_subtasks: vec![worker],
            completed: receipt.candidates.len(),
            total: receipt.candidates.len() + 1,
        },
        goal_contract: None,
        plan_graph: None,
        index_backend: None,
        handoff_packet: None,
        active_workers: Vec::new(),
        tokens_used: tin + tout,
        tokens_budget: None,
        estimated_naive_tokens: 0,
        checkpoints: Vec::new(),
        resume_checkpoint: None,
        cost_receipt: CostReceipt::default(),
    };
    if !settled(receipt) {
        let s = receipt.state.as_str();
        let number = s.rsplit('_').next().unwrap_or("1");
        let files = receipt
            .request
            .files
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let doing = if s.starts_with("verifying") {
            let checks = receipt
                .request
                .checks
                .first()
                .map(|c| c.program.clone())
                .unwrap_or_default();
            format!("checking candidate {number} with {checks}")
        } else if s.starts_with("generating") {
            format!("writing candidate {number} · {files}")
        } else if s.starts_with("hypothesizing") {
            format!("rethinking approach {number}")
        } else if s == "baseline" {
            "running checks on the unchanged source".to_string()
        } else {
            "saving the review".to_string()
        };
        state.active_workers.push(WorkerState {
            subtask_id: worker,
            subtask_description: doing,
            model_tier: ModelTier::Local,
            tokens_used: tin + tout,
            status: SubtaskStatus::Running {
                model_tier: ModelTier::Local,
                tokens_so_far: tin + tout,
            },
            is_thinking: s.starts_with("generating") || s.starts_with("hypothesizing"),
            model_name: receipt.profile.model.clone(),
        });
        return state;
    }
    let failed = selected.is_none();
    let Some(candidate) = selected.or(receipt.candidates.last()) else {
        state.task_status = TaskStatus::Failed {
            reason: failure_reason(receipt),
            failed_subtask: None,
        };
        return state;
    };
    let (changed_files, diff_stats) = diff_stats(&candidate.diff);
    let passed: Vec<String> = candidate
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Passed)
        .map(|c| format!("{} passed", check_label(c)))
        .collect();
    let failing: Vec<String> = candidate
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Failed)
        .map(|c| format!("{} failed", check_label(c)))
        .collect();
    let skipped: Vec<String> = if candidate.checks.is_empty() {
        vec!["No check ran on this candidate.".into()]
    } else {
        candidate
            .checks
            .iter()
            .filter(|c| matches!(c.status, CheckStatus::NotRun | CheckStatus::Unavailable))
            .map(|c| format!("{} did not run: {}", check_label(c), c.detail))
            .collect()
    };
    let verified = receipt.state == "review_ready";
    state.task_status = if failed {
        TaskStatus::Failed {
            reason: failure_reason(receipt),
            failed_subtask: None,
        }
    } else {
        TaskStatus::Reviewing {
            tokens_used: tin + tout,
            estimated_savings_tokens: 0,
        }
    };
    state.handoff_packet = Some(HandoffPacket {
        schema_version: phonton_types::HANDOFF_PACKET_SCHEMA_VERSION.to_string(),
        task_id,
        goal: receipt.request.goal.clone(),
        headline: if failed {
            format!(
                "{} candidate(s) tried; none passed the checks",
                receipt.candidates.len()
            )
        } else if verified {
            format!(
                "Candidate {} passed {} check{} on {}",
                candidate.number,
                passed.len(),
                if passed.len() == 1 { "" } else { "s" },
                receipt.profile.model
            )
        } else {
            format!(
                "Candidate {} is ready, but its checks did not all pass",
                candidate.number
            )
        },
        changed_files,
        generated_artifacts: Vec::new(),
        diff_stats,
        verification: VerifyReport {
            passed,
            findings: failing,
            skipped,
        },
        run_commands: receipt
            .request
            .checks
            .iter()
            .map(|c| RunCommand {
                label: "check".into(),
                command: std::iter::once(c.program.clone())
                    .chain(c.args.iter().cloned())
                    .collect(),
                cwd: None,
            })
            .collect(),
        known_gaps: receipt.known_gaps.clone(),
        review_actions: Vec::new(),
        rollback_points: Vec::new(),
        token_usage: TokenUsage {
            input_tokens: tin,
            output_tokens: tout,
            ..Default::default()
        },
        influence: Default::default(),
        screenshot_path: None,
        rendering_summary: None,
        cost_receipt: CostReceipt::default(),
    });
    state
}

/// The lines of a failed check's output that say what failed (TAP `not ok`
/// lines and the pass/fail summary), at most `limit`.
pub fn check_failures(
    candidate: &phonton_types::local_run::CandidateEvidence,
    limit: usize,
) -> Vec<String> {
    candidate
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Failed)
        .flat_map(|c| c.stdout.lines().chain(c.stderr.lines()))
        .map(str::trim)
        .filter(|l| {
            l.starts_with("not ok")
                || l.starts_with("# pass")
                || l.starts_with("# fail")
                || l.starts_with("error:")
        })
        .take(limit)
        .map(str::to_string)
        .collect()
}

fn failure_reason(receipt: &LocalRunReceipt) -> String {
    let last = receipt
        .candidates
        .last()
        .and_then(|c| c.rejection.clone())
        .unwrap_or_default();
    let why = match receipt.state.as_str() {
        "no_verified_candidate" => "No candidate passed the checks",
        "budget_exhausted" => "The run budget ran out before a candidate passed",
        "resource_pressure" => "Not enough free memory to keep the model loaded",
        "search_stopped" => "The search stopped without a reviewable candidate",
        "dependency_unavailable" => "Check dependencies could not be prepared offline",
        other => other,
    };
    if last.is_empty() {
        format!(
            "{why}. Evidence saved: phonton goal --local show {}",
            receipt.id
        )
    } else {
        format!("{why}: {last}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_stats_count_per_file() {
        let diff = "--- a/src/port.js\n+++ b/src/port.js\n@@ -1,3 +1,5 @@\n x\n+a\n+b\n-c\n";
        let (files, stats) = diff_stats(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path.display().to_string(), "src/port.js");
        assert_eq!((stats.added_lines, stats.removed_lines), (2, 1));
    }
}
