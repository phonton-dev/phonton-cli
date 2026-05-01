//! Review command for verified Phonton task output.
//!
//! The review surface is reconstructed from persisted orchestrator events.
//! In particular, `SubtaskReviewReady` is emitted only after verification
//! passes, so this command never presents an unverified worker diff as ready.

use anyhow::Result;
use phonton_diff::DiffApplier;
use phonton_store::TaskRecord;
use phonton_types::{
    ContextAttribution, DiffHunk, DiffLine, EventRecord, OrchestratorEvent, TaskId, TaskStatus,
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
    review_items: Vec<ReviewItem>,
}

#[derive(Debug, Clone, Serialize)]
struct ReviewItem {
    subtask_id: String,
    description: String,
    tier: String,
    tokens_used: u64,
    provider: String,
    model_name: String,
    verify: String,
    context: Vec<ContextAttribution>,
    context_token_count: usize,
    diff_hunks: Vec<DiffHunk>,
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
            "-h" | "--help" => {
                return Err(anyhow::anyhow!(
                    "usage: phonton review [--json] [latest|<task-id>]\n       phonton review approve [--json] [latest|<task-id>]\n       phonton review reject [--json] [latest|<task-id>]\n       phonton review rollback [--json] [latest|<task-id>] <seq>"
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
    } else {
        print_text_report(&report);
    }

    Ok(if report.review_items.is_empty() { 1 } else { 0 })
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
    let report = ActionReport {
        task_id: task.id.to_string(),
        action: "rollback".into(),
        status: serde_json::to_value(&status)?,
        detail: format!("Rolled worktree back to checkpoint #{seq} ({commit_oid})."),
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

    let mut review_items = Vec::new();
    for event in events {
        if let OrchestratorEvent::SubtaskReviewReady {
            subtask_id,
            description,
            tier,
            tokens_used,
            diff_hunks,
            verify_result,
            provider,
            model_name,
        } = event.event
        {
            let (context, context_token_count) = context_by_subtask
                .remove(&subtask_id.to_string())
                .unwrap_or_default();
            review_items.push(ReviewItem {
                subtask_id: subtask_id.to_string(),
                description,
                tier: tier.to_string(),
                tokens_used,
                provider: provider.to_string(),
                model_name,
                verify: format!("{verify_result:?}"),
                context,
                context_token_count,
                diff_hunks,
            });
        }
    }

    ReviewReport {
        task_id: task.id.to_string(),
        goal: task.goal_text,
        status: task.status,
        total_tokens: task.total_tokens,
        review_items,
    }
}

fn print_text_report(report: &ReviewReport) {
    println!("Phonton review");
    println!("task:   {}", report.task_id);
    println!("goal:   {}", report.goal);
    println!("tokens: {}", report.total_tokens);
    println!("status: {}", compact_json(&report.status));
    println!();

    if report.review_items.is_empty() {
        println!("No verified review payloads found for this task.");
        println!("Run a task to Reviewing/Done first; failed or pre-verification output is not review-ready.");
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
        println!(
            "   subtask: {}  provider: {}  model: {}",
            item.subtask_id,
            item.provider,
            if item.model_name.is_empty() {
                "(unknown)"
            } else {
                &item.model_name
            }
        );
        render_context(&item.context);
        render_hunks(&item.diff_hunks);
        println!();
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
        DiffHunk, ModelTier, ProviderKind, SliceOrigin, SubtaskId, VerifyLayer, VerifyResult,
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
    }
}
