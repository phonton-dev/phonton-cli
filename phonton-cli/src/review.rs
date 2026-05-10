//! Review command for verified Phonton task output.
//!
//! The review surface is reconstructed from persisted orchestrator events.
//! In particular, `SubtaskReviewReady` is emitted only after verification
//! passes, so this command never presents an unverified worker diff as ready.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use phonton_diff::DiffApplier;
use phonton_store::TaskRecord;
use phonton_types::{
    ContextAttribution, CostSummary, DiffHunk, DiffLine, EventRecord, HandoffPacket,
    OrchestratorEvent, TaskId, TaskStatus, TokenUsage,
};
use serde::Serialize;

use crate::open_persistent_store;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewAction {
    Show,
    Approve,
    Reject,
    Rollback { seq: u32 },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReviewOptions {
    pub json: bool,
    pub markdown: bool,
}

#[derive(Debug, Clone)]
pub struct ReviewRequest {
    pub action: ReviewAction,
    pub task_ref: Option<String>,
    pub options: ReviewOptions,
}

#[derive(Debug, Clone, Serialize)]
struct ReviewReport {
    task_id: String,
    goal: String,
    status: serde_json::Value,
    total_tokens: u64,
    handoff: Option<HandoffPacket>,
    checkpoints: Vec<CheckpointItem>,
    review_items: Vec<ReviewItem>,
    diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ReviewItem {
    subtask_id: String,
    description: String,
    tier: String,
    tokens_used: u64,
    token_usage: TokenUsage,
    cost: CostSummary,
    provider: String,
    model_name: String,
    verify: String,
    context: Vec<ContextAttribution>,
    context_token_count: usize,
    diff_hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone, Serialize)]
struct CheckpointItem {
    seq: u32,
    subtask_id: String,
    commit_oid: String,
}

#[derive(Debug, Clone, Serialize)]
struct ActionReport {
    task_id: String,
    action: String,
    status: serde_json::Value,
    detail: String,
}

pub fn parse_request(args: &[String]) -> Result<ReviewRequest> {
    let mut options = ReviewOptions::default();
    let mut action = ReviewAction::Show;
    let mut task_ref = None;
    let mut positionals = Vec::new();

    for arg in args {
        match arg.as_str() {
            "--json" => options.json = true,
            "--markdown" | "--md" => options.markdown = true,
            "-h" | "--help" => {
                return Err(anyhow::anyhow!(
                    "usage: phonton review [--json|--markdown] [latest|<task-id>]\n       phonton review approve [--json] [latest|<task-id>]\n       phonton review reject [--json] [latest|<task-id>]\n       phonton review rollback [--json] [latest|<task-id>] <seq>"
                ));
            }
            other if other.starts_with('-') => {
                return Err(anyhow::anyhow!("unknown review option `{other}`"));
            }
            other => positionals.push(other.to_string()),
        }
    }

    if let Some(first) = positionals.first().map(String::as_str) {
        match first {
            "approve" => {
                action = ReviewAction::Approve;
                positionals.remove(0);
            }
            "reject" => {
                action = ReviewAction::Reject;
                positionals.remove(0);
            }
            "rollback" => {
                positionals.remove(0);
                let seq_raw = match positionals.len() {
                    1 => positionals.remove(0),
                    2 => {
                        task_ref = Some(positionals.remove(0));
                        positionals.remove(0)
                    }
                    _ => {
                        return Err(anyhow::anyhow!(
                            "rollback expects `<seq>` or `<task-id> <seq>`"
                        ))
                    }
                };
                let seq = seq_raw
                    .parse::<u32>()
                    .map_err(|_| anyhow::anyhow!("rollback seq must be a positive integer"))?;
                if seq == 0 {
                    return Err(anyhow::anyhow!("rollback seq must be greater than zero"));
                }
                action = ReviewAction::Rollback { seq };
            }
            _ => {}
        }
    }

    if !positionals.is_empty() {
        if positionals.len() > 1 || task_ref.is_some() {
            return Err(anyhow::anyhow!("review accepts at most one task id"));
        }
        task_ref = Some(positionals.remove(0));
    }

    if options.json && options.markdown {
        return Err(anyhow::anyhow!(
            "choose either --json or --markdown, not both"
        ));
    }

    Ok(ReviewRequest {
        action,
        task_ref,
        options,
    })
}

pub async fn run(args: &[String]) -> Result<i32> {
    let request = match parse_request(args) {
        Ok(request) => request,
        Err(e) => {
            let msg = e.to_string();
            if msg.starts_with("usage:") {
                println!("{msg}");
                return Ok(0);
            }
            eprintln!("phonton review: {msg}");
            eprintln!("Run `phonton review --help` for usage.");
            return Ok(2);
        }
    };

    let store = match open_persistent_store() {
        Ok(store) => store,
        Err(e) => {
            eprintln!("phonton review: persistent store unavailable: {e}");
            return Ok(1);
        }
    };

    let task = match resolve_task(&store, request.task_ref.as_deref()).await? {
        Some(task) => task,
        None => {
            eprintln!("phonton review: no matching task found");
            return Ok(1);
        }
    };

    let events = store.list_events(task.id, 10_000)?;
    let report = build_report(task.clone(), events.clone());

    match request.action {
        ReviewAction::Show => {}
        ReviewAction::Approve => {
            return finish_task(
                &store,
                task,
                TaskStatus::Done {
                    tokens_used: report.total_tokens,
                    wall_time_ms: 0,
                },
                "approve",
                request.options.json,
            )
            .await;
        }
        ReviewAction::Reject => {
            return finish_task(
                &store,
                task,
                TaskStatus::Rejected,
                "reject",
                request.options.json,
            )
            .await;
        }
        ReviewAction::Rollback { seq } => {
            return rollback_task(&store, task, events, seq, request.options.json).await;
        }
    }

    if request.options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if request.options.markdown {
        print!("{}", format_markdown_report(&report));
    } else {
        print_text_report(&report);
    }

    Ok(
        if report.review_items.is_empty() && report.diagnostics.is_empty() {
            1
        } else {
            0
        },
    )
}

async fn resolve_task(
    store: &phonton_store::Store,
    task_ref: Option<&str>,
) -> Result<Option<TaskRecord>> {
    match task_ref {
        None | Some("latest") => Ok(store.list_tasks(1).await?.into_iter().next()),
        Some(raw) => {
            let id = parse_task_id(raw)?;
            store.get_task(id).await
        }
    }
}

async fn finish_task(
    store: &phonton_store::Store,
    task: TaskRecord,
    status: TaskStatus,
    action: &str,
    json: bool,
) -> Result<i32> {
    store.upsert_task(task.id, &task.goal_text, &status, task.total_tokens)?;
    append_review_decision(
        store,
        task.id,
        action,
        match action {
            "approve" => "Task marked Done.",
            "reject" => "Task marked Rejected.",
            _ => "Task updated.",
        },
    )?;
    let status_json = serde_json::to_value(&status)?;
    let report = ActionReport {
        task_id: task.id.to_string(),
        action: action.into(),
        status: status_json,
        detail: match action {
            "approve" => "Task marked Done.".into(),
            "reject" => "Task marked Rejected.".into(),
            _ => "Task updated.".into(),
        },
    };
    print_action_report(&report, json)?;
    Ok(0)
}

async fn rollback_task(
    store: &phonton_store::Store,
    task: TaskRecord,
    events: Vec<EventRecord>,
    seq: u32,
    json: bool,
) -> Result<i32> {
    let Some(commit_oid) = checkpoint_oid(&events, seq) else {
        eprintln!(
            "phonton review rollback: checkpoint #{seq} not found for task {}",
            task.id
        );
        return Ok(1);
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let mut diff = match DiffApplier::open(&cwd) {
        Ok(diff) => diff,
        Err(e) => {
            eprintln!("phonton review rollback: {e}");
            return Ok(1);
        }
    };
    if let Err(e) = diff.rollback_to_checkpoint(&commit_oid) {
        eprintln!("phonton review rollback: {e}");
        return Ok(1);
    }

    let status = TaskStatus::Reviewing {
        tokens_used: task.total_tokens,
        estimated_savings_tokens: 0,
    };
    store.upsert_task(task.id, &task.goal_text, &status, task.total_tokens)?;
    let detail = format!(
        "Rolled worktree back to checkpoint #{seq} ({commit_oid}). Review remaining work and rerun planning for a revised path."
    );
    store.append_event(&EventRecord {
        task_id: task.id,
        timestamp_ms: now_ms(),
        event: OrchestratorEvent::RollbackPerformed {
            task_id: task.id,
            to_seq: seq,
            requeued_subtasks: 0,
        },
    })?;
    append_review_decision(store, task.id, "rollback", &detail)?;
    let report = ActionReport {
        task_id: task.id.to_string(),
        action: "rollback".into(),
        status: serde_json::to_value(&status)?,
        detail,
    };
    print_action_report(&report, json)?;
    Ok(0)
}

fn checkpoint_oid(events: &[EventRecord], seq: u32) -> Option<String> {
    events.iter().find_map(|event| {
        if let OrchestratorEvent::CheckpointCreated {
            seq: event_seq,
            commit_oid,
            ..
        } = &event.event
        {
            if *event_seq == seq {
                return Some(commit_oid.clone());
            }
        }
        None
    })
}

fn append_review_decision(
    store: &phonton_store::Store,
    task_id: TaskId,
    decision: &str,
    detail: &str,
) -> Result<()> {
    store.append_event(&EventRecord {
        task_id,
        timestamp_ms: now_ms(),
        event: OrchestratorEvent::ReviewDecision {
            task_id,
            decision: decision.to_string(),
            detail: detail.to_string(),
        },
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn parse_task_id(raw: &str) -> Result<TaskId> {
    let json = serde_json::Value::String(raw.to_string());
    serde_json::from_value(json).map_err(Into::into)
}

fn build_report(task: TaskRecord, events: Vec<EventRecord>) -> ReviewReport {
    let mut context_by_subtask: std::collections::HashMap<
        String,
        (Vec<ContextAttribution>, usize),
    > = std::collections::HashMap::new();
    for event in &events {
        if let OrchestratorEvent::ContextSelected {
            subtask_id,
            slices,
            total_token_count,
        } = &event.event
        {
            context_by_subtask.insert(subtask_id.to_string(), (slices.clone(), *total_token_count));
        }
    }

    let mut checkpoints = Vec::new();
    let mut review_items = Vec::new();
    let mut diagnostics = Vec::new();
    for event in events {
        match event.event {
            OrchestratorEvent::CheckpointCreated {
                subtask_id,
                seq,
                commit_oid,
                ..
            } => checkpoints.push(CheckpointItem {
                seq,
                subtask_id: subtask_id.to_string(),
                commit_oid,
            }),
            OrchestratorEvent::SubtaskReviewReady {
                subtask_id,
                description,
                tier,
                tokens_used,
                token_usage,
                cost,
                diff_hunks,
                verify_result,
                provider,
                model_name,
            } => {
                let (context, context_token_count) = context_by_subtask
                    .remove(&subtask_id.to_string())
                    .unwrap_or_default();
                review_items.push(ReviewItem {
                    subtask_id: subtask_id.to_string(),
                    description,
                    tier: tier.to_string(),
                    tokens_used,
                    token_usage,
                    cost,
                    provider: provider.to_string(),
                    model_name,
                    verify: format!("{verify_result:?}"),
                    context,
                    context_token_count,
                    diff_hunks,
                });
            }
            OrchestratorEvent::VerifyFail {
                layer,
                errors,
                attempt,
                ..
            } => {
                if errors.is_empty() {
                    diagnostics.push(format!("verify {layer:?} failed on attempt {attempt}"));
                } else {
                    for error in errors {
                        diagnostics.push(format!("verify {layer:?} attempt {attempt}: {error}"));
                    }
                }
            }
            OrchestratorEvent::SubtaskFailed {
                reason, attempt, ..
            } => diagnostics.push(format!("subtask failed on attempt {attempt}: {reason}")),
            _ => {}
        }
    }

    ReviewReport {
        task_id: task.id.to_string(),
        goal: task.goal_text,
        status: task.status,
        total_tokens: task.total_tokens,
        handoff: task.outcome_ledger.and_then(|ledger| ledger.handoff),
        checkpoints,
        review_items,
        diagnostics,
    }
}

fn print_text_report(report: &ReviewReport) {
    println!("Phonton review");
    println!("task:   {}", report.task_id);
    println!("goal:   {}", report.goal);
    println!("tokens: {}", report.total_tokens);
    println!("status: {}", compact_json(&report.status));
    println!("checkpoints: {}", report.checkpoints.len());
    if let Some(handoff) = &report.handoff {
        println!(
            "result: {} files, +{} -{}",
            handoff.diff_stats.files_changed,
            handoff.diff_stats.added_lines,
            handoff.diff_stats.removed_lines
        );
        println!("summary: {}", handoff.headline);
        if !handoff.known_gaps.is_empty() {
            println!("known gaps:");
            for gap in handoff.known_gaps.iter().take(5) {
                println!("  - {gap}");
            }
        }
    }
    println!();

    if report.review_items.is_empty() {
        if report.diagnostics.is_empty() {
            println!("No verified review payloads found for this task.");
            println!("Run a task to Reviewing/Done first; failed or pre-verification output is not review-ready.");
        } else {
            println!("No verified review payloads found; this task is failed/unverified.");
            println!("Diagnostics:");
            for diagnostic in report.diagnostics.iter().take(10) {
                println!("  - {diagnostic}");
            }
        }
        return;
    }

    for (idx, item) in report.review_items.iter().enumerate() {
        println!(
            "{}. {} [{}] verify={} tokens={} context={} slices/{} tokens",
            idx + 1,
            item.description.lines().next().unwrap_or(&item.description),
            item.tier,
            item.verify,
            item.tokens_used,
            item.context.len(),
            item.context_token_count
        );
        let price = if item.cost.pricing_known {
            format!("{} micros", item.cost.total_usd_micros)
        } else {
            "unknown pricing".into()
        };
        println!(
            "   subtask: {}  provider: {}  model: {}  cost: {}",
            item.subtask_id,
            item.provider,
            if item.model_name.is_empty() {
                "(unknown)"
            } else {
                &item.model_name
            },
            price
        );
        println!(
            "   usage: input={} output={} cached={} cache_creation={}{}",
            item.token_usage.input_tokens,
            item.token_usage.output_tokens,
            item.token_usage.cached_tokens,
            item.token_usage.cache_creation_tokens,
            if item.token_usage.estimated {
                " estimated"
            } else {
                ""
            }
        );
        render_context(&item.context);
        render_hunks(&item.diff_hunks);
        println!();
    }

    if !report.checkpoints.is_empty() {
        println!("Checkpoints:");
        for checkpoint in &report.checkpoints {
            println!(
                "  #{} {} {}",
                checkpoint.seq, checkpoint.subtask_id, checkpoint.commit_oid
            );
        }
    }
}

fn format_markdown_report(report: &ReviewReport) -> String {
    let mut out = String::new();
    writeln!(out, "# Phonton Review Receipt").ok();
    writeln!(out).ok();
    writeln!(out, "- Task: `{}`", report.task_id).ok();
    writeln!(out, "- Goal: {}", report.goal).ok();
    writeln!(out, "- Status: `{}`", compact_json(&report.status)).ok();
    writeln!(out, "- Tokens: {}", report.total_tokens).ok();
    writeln!(out, "- Checkpoints: {}", report.checkpoints.len()).ok();

    writeln!(out).ok();
    writeln!(out, "## Result").ok();
    if let Some(handoff) = &report.handoff {
        writeln!(out, "{}", handoff.headline).ok();
        writeln!(
            out,
            "Changed {} file(s), +{} -{}.",
            handoff.diff_stats.files_changed,
            handoff.diff_stats.added_lines,
            handoff.diff_stats.removed_lines
        )
        .ok();
    } else {
        writeln!(out, "No handoff packet was recorded for this task.").ok();
    }

    writeln!(out).ok();
    writeln!(out, "## Changed Files").ok();
    if let Some(handoff) = &report.handoff {
        if handoff.changed_files.is_empty() {
            writeln!(out, "- None recorded.").ok();
        } else {
            for file in &handoff.changed_files {
                writeln!(
                    out,
                    "- `{}` (+{} -{}): {}",
                    file.path.display(),
                    file.added_lines,
                    file.removed_lines,
                    file.summary
                )
                .ok();
            }
        }
    } else {
        append_review_item_files(&mut out, report);
    }

    writeln!(out).ok();
    writeln!(out, "## Verification").ok();
    if let Some(handoff) = &report.handoff {
        append_markdown_list(&mut out, "Passed", &handoff.verification.passed);
        append_markdown_list(&mut out, "Findings", &handoff.verification.findings);
        append_markdown_list(&mut out, "Skipped", &handoff.verification.skipped);
    } else if report.review_items.is_empty() {
        writeln!(
            out,
            "- Failed/unverified: no verified review payloads found."
        )
        .ok();
    } else {
        for item in &report.review_items {
            writeln!(
                out,
                "- `{}`: {} using `{}`",
                item.subtask_id, item.verify, item.model_name
            )
            .ok();
        }
    }

    writeln!(out).ok();
    writeln!(out, "## Diagnostics").ok();
    if report.diagnostics.is_empty() {
        writeln!(out, "- None recorded.").ok();
    } else {
        append_markdown_items(&mut out, &report.diagnostics);
    }

    writeln!(out).ok();
    writeln!(out, "## Run Commands").ok();
    if let Some(handoff) = &report.handoff {
        if handoff.run_commands.is_empty() {
            writeln!(out, "- None inferred.").ok();
        } else {
            for command in &handoff.run_commands {
                writeln!(out, "- {}: `{}`", command.label, command.command.join(" ")).ok();
            }
        }
    } else {
        writeln!(out, "- No handoff packet was recorded.").ok();
    }

    writeln!(out).ok();
    writeln!(out, "## Known Gaps").ok();
    if let Some(handoff) = &report.handoff {
        append_markdown_items(&mut out, &handoff.known_gaps);
    } else {
        writeln!(out, "- Unknown; no handoff packet was recorded.").ok();
    }

    writeln!(out).ok();
    writeln!(out, "## Rollback").ok();
    if let Some(handoff) = &report.handoff {
        if handoff.rollback_points.is_empty() {
            writeln!(out, "- No rollback checkpoints were recorded.").ok();
        } else {
            for rollback in &handoff.rollback_points {
                writeln!(out, "- #{} {}", rollback.seq, rollback.label).ok();
            }
        }
    } else if report.checkpoints.is_empty() {
        writeln!(out, "- No checkpoints were recorded.").ok();
    } else {
        for checkpoint in &report.checkpoints {
            writeln!(
                out,
                "- #{} `{}` ({})",
                checkpoint.seq, checkpoint.commit_oid, checkpoint.subtask_id
            )
            .ok();
        }
    }

    writeln!(out).ok();
    writeln!(out, "## Cost And Tokens").ok();
    writeln!(out, "- Total tokens: {}", report.total_tokens).ok();
    for item in &report.review_items {
        let cost = if item.cost.pricing_known {
            format!("{} micros", item.cost.total_usd_micros)
        } else {
            "unknown pricing".into()
        };
        writeln!(
            out,
            "- `{}`: {} tokens, {}",
            item.subtask_id, item.tokens_used, cost
        )
        .ok();
    }

    writeln!(out).ok();
    writeln!(out, "## Influence And Memory").ok();
    if let Some(handoff) = &report.handoff {
        append_markdown_list(&mut out, "Memories", &handoff.influence.memories);
        append_markdown_list(&mut out, "Index slices", &handoff.influence.index_slices);
        append_markdown_list(&mut out, "Skills", &handoff.influence.skills);
        append_markdown_list(&mut out, "Extensions", &handoff.influence.extensions);
    } else {
        writeln!(out, "- No influence summary was recorded.").ok();
    }

    out
}

fn append_review_item_files(out: &mut String, report: &ReviewReport) {
    let mut wrote = false;
    for item in &report.review_items {
        for hunk in &item.diff_hunks {
            writeln!(out, "- `{}`", hunk.file_path.display()).ok();
            wrote = true;
        }
    }
    if !wrote {
        writeln!(out, "- None recorded.").ok();
    }
}

fn append_markdown_list(out: &mut String, label: &str, items: &[String]) {
    if items.is_empty() {
        writeln!(out, "- {label}: none").ok();
        return;
    }
    writeln!(out, "{label}:").ok();
    append_markdown_items(out, items);
}

fn append_markdown_items(out: &mut String, items: &[String]) {
    if items.is_empty() {
        writeln!(out, "- None.").ok();
        return;
    }
    for item in items {
        writeln!(out, "- {item}").ok();
    }
}

fn render_context(context: &[ContextAttribution]) {
    if context.is_empty() {
        println!("   context: (none selected)");
        return;
    }
    println!("   context:");
    for slice in context {
        println!(
            "     - {} :: {} ({:?}, {} tokens)",
            slice.file_path.display(),
            slice.symbol_name,
            slice.origin,
            slice.token_count
        );
    }
}

fn render_hunks(hunks: &[DiffHunk]) {
    if hunks.is_empty() {
        println!("   diff: (no hunks)");
        return;
    }
    for hunk in hunks {
        println!("   file: {}", hunk.file_path.display());
        println!(
            "   @@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
        );
        for line in &hunk.lines {
            match line {
                DiffLine::Context(text) => println!("     {text}"),
                DiffLine::Added(text) => println!("   + {text}"),
                DiffLine::Removed(text) => println!("   - {text}"),
            }
        }
    }
}

fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn print_action_report(report: &ActionReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        println!("Phonton review {}", report.action);
        println!("task:   {}", report.task_id);
        println!("status: {}", compact_json(&report.status));
        println!("{}", report.detail);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::{
        CostSummary, DiffHunk, ModelTier, ProviderKind, SliceOrigin, SubtaskId, TokenUsage,
        VerifyLayer, VerifyResult,
    };

    #[test]
    fn parse_request_defaults_to_latest() {
        let request = parse_request(&[]).unwrap();
        assert_eq!(request.action, ReviewAction::Show);
        assert!(request.task_ref.is_none());
        assert!(!request.options.json);
    }

    #[test]
    fn parse_request_accepts_json_and_task_id() {
        let request = parse_request(&["--json".into(), "latest".into()]).unwrap();
        assert_eq!(request.task_ref.as_deref(), Some("latest"));
        assert!(request.options.json);
        assert!(!request.options.markdown);
    }

    #[test]
    fn parse_request_accepts_markdown() {
        let request = parse_request(&["--markdown".into(), "latest".into()]).unwrap();

        assert_eq!(request.task_ref.as_deref(), Some("latest"));
        assert!(request.options.markdown);
    }

    #[test]
    fn parse_request_accepts_approve_action() {
        let request = parse_request(&["approve".into(), "latest".into()]).unwrap();
        assert_eq!(request.action, ReviewAction::Approve);
        assert_eq!(request.task_ref.as_deref(), Some("latest"));
    }

    #[test]
    fn parse_request_accepts_rollback_latest_short_form() {
        let request = parse_request(&["rollback".into(), "3".into()]).unwrap();
        assert_eq!(request.action, ReviewAction::Rollback { seq: 3 });
        assert!(request.task_ref.is_none());
    }

    #[test]
    fn checkpoint_oid_finds_matching_checkpoint() {
        let task_id = TaskId::new();
        let subtask_id = SubtaskId::new();
        let events = vec![EventRecord {
            task_id,
            timestamp_ms: 1,
            event: OrchestratorEvent::CheckpointCreated {
                task_id,
                subtask_id,
                seq: 2,
                commit_oid: "abc123".into(),
            },
        }];
        assert_eq!(checkpoint_oid(&events, 2).as_deref(), Some("abc123"));
        assert!(checkpoint_oid(&events, 3).is_none());
    }

    #[test]
    fn build_report_extracts_verified_review_events() {
        let task_id = TaskId::new();
        let subtask_id = SubtaskId::new();
        let task = TaskRecord {
            id: task_id,
            goal_text: "add function foo".into(),
            status: serde_json::json!({"Reviewing": {"tokens_used": 120}}),
            created_at: 1,
            total_tokens: 120,
            outcome_ledger: None,
        };
        let events = vec![
            EventRecord {
                task_id,
                timestamp_ms: 2,
                event: OrchestratorEvent::ContextSelected {
                    subtask_id,
                    slices: vec![ContextAttribution {
                        file_path: "src/lib.rs".into(),
                        symbol_name: "foo".into(),
                        origin: SliceOrigin::Semantic,
                        token_count: 11,
                    }],
                    total_token_count: 11,
                },
            },
            EventRecord {
                task_id,
                timestamp_ms: 3,
                event: OrchestratorEvent::SubtaskReviewReady {
                    subtask_id,
                    description: "Implement function `foo`".into(),
                    tier: ModelTier::Standard,
                    tokens_used: 120,
                    token_usage: TokenUsage {
                        input_tokens: 80,
                        output_tokens: 40,
                        ..TokenUsage::default()
                    },
                    cost: CostSummary {
                        pricing_known: true,
                        input_usd_micros: 80,
                        output_usd_micros: 40,
                        total_usd_micros: 120,
                    },
                    diff_hunks: vec![DiffHunk {
                        file_path: "src/lib.rs".into(),
                        old_start: 1,
                        old_count: 0,
                        new_start: 1,
                        new_count: 1,
                        lines: vec![DiffLine::Added("pub fn foo() {}".into())],
                    }],
                    verify_result: VerifyResult::Pass {
                        layer: VerifyLayer::Syntax,
                    },
                    provider: ProviderKind::Anthropic,
                    model_name: "test-model".into(),
                },
            },
        ];

        let report = build_report(task, events);
        assert_eq!(report.review_items.len(), 1);
        assert_eq!(report.review_items[0].subtask_id, subtask_id.to_string());
        assert_eq!(report.review_items[0].diff_hunks.len(), 1);
        assert_eq!(report.review_items[0].context_token_count, 11);
        assert_eq!(report.review_items[0].context[0].symbol_name, "foo");
        assert_eq!(report.review_items[0].token_usage.input_tokens, 80);
        assert_eq!(report.review_items[0].cost.total_usd_micros, 120);
    }

    #[test]
    fn markdown_report_includes_receipt_sections() {
        let task_id = TaskId::new();
        let task = TaskRecord {
            id: task_id,
            goal_text: "add validation".into(),
            status: serde_json::json!({"Reviewing": {"tokens_used": 12}}),
            created_at: 1,
            total_tokens: 12,
            outcome_ledger: Some(phonton_types::OutcomeLedger {
                task_id,
                goal_contract: None,
                context_manifest: phonton_types::ContextManifest::default(),
                permission_ledger: phonton_types::PermissionLedger::default(),
                verify_report: phonton_types::VerifyReport {
                    passed: vec!["cargo test config_validation".into()],
                    findings: Vec::new(),
                    skipped: Vec::new(),
                },
                handoff: Some(HandoffPacket {
                    task_id,
                    goal: "add validation".into(),
                    headline: "Review ready: validation added".into(),
                    changed_files: vec![phonton_types::ChangedFileSummary {
                        path: "src/config.rs".into(),
                        added_lines: 4,
                        removed_lines: 0,
                        summary: "reject empty providers".into(),
                    }],
                    generated_artifacts: Vec::new(),
                    diff_stats: phonton_types::DiffStats {
                        files_changed: 1,
                        added_lines: 4,
                        removed_lines: 0,
                    },
                    verification: phonton_types::VerifyReport {
                        passed: vec!["cargo test config_validation".into()],
                        findings: Vec::new(),
                        skipped: Vec::new(),
                    },
                    run_commands: vec![phonton_types::RunCommand {
                        label: "Run tests".into(),
                        command: vec!["cargo".into(), "test".into(), "config_validation".into()],
                        cwd: None,
                    }],
                    known_gaps: vec!["No provider network check needed.".into()],
                    review_actions: Vec::new(),
                    rollback_points: vec![phonton_types::RollbackPoint {
                        seq: 1,
                        label: "before verified diff".into(),
                    }],
                    token_usage: TokenUsage::estimated(12),
                    influence: phonton_types::InfluenceSummary {
                        memories: vec!["empty providers are invalid".into()],
                        ..phonton_types::InfluenceSummary::default()
                    },
                }),
            }),
        };
        let report = build_report(task, Vec::new());

        let markdown = format_markdown_report(&report);

        assert!(markdown.contains("# Phonton Review Receipt"));
        assert!(markdown.contains("## Changed Files"));
        assert!(markdown.contains("`src/config.rs`"));
        assert!(markdown.contains("## Verification"));
        assert!(markdown.contains("cargo test config_validation"));
        assert!(markdown.contains("## Run Commands"));
        assert!(markdown.contains("## Known Gaps"));
        assert!(markdown.contains("## Influence And Memory"));
    }
}
