//! Review command for verified Phonton task output.
//!
//! The review surface is reconstructed from persisted orchestrator events.
//! In particular, `SubtaskReviewReady` is emitted only after verification
//! passes, so this command never presents an unverified worker diff as ready.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use phonton_diff::DiffApplier;
use phonton_store::TaskRecord;
use phonton_types::{
    reference_pricing_for_tier, ContextAttribution, CostReceipt, CostSummary, DiffHunk, DiffLine,
    EventRecord, HandoffPacket, ModelTier, OrchestratorEvent, RollbackPoint, RouteOutcome,
    RouteStep, TaskId, TaskStatus, TokenUsage,
};
use serde::Serialize;

use crate::store_util::open_persistent_store;

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
pub struct ReviewReport {
    task_id: String,
    goal: String,
    status: serde_json::Value,
    total_tokens: u64,
    cost_receipt: CostReceipt,
    handoff: Option<HandoffPacket>,
    checkpoints: Vec<CheckpointItem>,
    review_items: Vec<ReviewItem>,
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
            "-h" | "--help" => {
                return Err(anyhow::anyhow!(
                    "usage: phonton review [--json] [latest|<task-id>]\n       phonton review approve [--json] [latest|<task-id>]\n       phonton review reject [--json] [latest|<task-id>]\n       phonton review rollback [--json] [latest|<task-id>] <seq>  (disabled: unsafe legacy reset)"
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
            let refusal = approval_refusal(&task, &report)
                .map(str::to_owned)
                .or_else(|| {
                    std::env::current_dir()
                        .map_err(anyhow::Error::from)
                        .and_then(|cwd| reviewed_checkpoint_identity(&task, &report, &cwd))
                        .err()
                        .map(|error| error.to_string())
                });
            if let Some(reason) = refusal {
                print_action_report(
                    &ActionReport {
                        task_id: task.id.to_string(),
                        action: "approve-refused".into(),
                        status: task.status.clone(),
                        detail: reason,
                    },
                    request.options.json,
                )?;
                return Ok(1);
            }
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

fn approval_refusal(task: &TaskRecord, report: &ReviewReport) -> Option<&'static str> {
    if !matches!(
        serde_json::from_value::<TaskStatus>(task.status.clone()),
        Ok(TaskStatus::Reviewing { .. })
    ) {
        return Some("Only a task in Reviewing can be approved.");
    }
    if report.review_items.is_empty()
        || report
            .review_items
            .iter()
            .any(|item| item.diff_hunks.is_empty())
    {
        return Some("No verified change payload is ready to approve.");
    }
    None
}

fn reviewed_checkpoint_identity(
    task: &TaskRecord,
    report: &ReviewReport,
    cwd: &Path,
) -> Result<()> {
    let checkpoint = report
        .checkpoints
        .iter()
        .max_by_key(|checkpoint| checkpoint.seq)
        .ok_or_else(|| anyhow::anyhow!("No Git checkpoint proves the reviewed files"))?;
    if report.review_items.iter().any(|item| {
        !report
            .checkpoints
            .iter()
            .any(|checkpoint| checkpoint.subtask_id == item.subtask_id)
    }) {
        return Err(anyhow::anyhow!(
            "A reviewed change has no matching checkpoint"
        ));
    }
    let diff = DiffApplier::open(cwd)?;
    let repo = diff.repo();
    let ref_name = format!("refs/phonton/checkpoints/{}/{}", task.id, checkpoint.seq);
    let current_oid = repo
        .find_reference(&ref_name)?
        .target()
        .ok_or_else(|| anyhow::anyhow!("Reviewed checkpoint has no commit"))?;
    if current_oid.to_string() != checkpoint.commit_oid {
        return Err(anyhow::anyhow!(
            "Reviewed checkpoint changed since the task finished"
        ));
    }
    let tree = repo.find_commit(current_oid)?.tree()?;
    let mut index = repo.index()?;
    index.read(true)?;
    let root = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("Reviewed repository has no worktree"))?;
    let mut paths = BTreeSet::new();
    for item in &report.review_items {
        for hunk in &item.diff_hunks {
            paths.insert(diff.repository_relative_path(&hunk.file_path)?);
        }
    }
    for path in paths {
        let saved = tree.get_path(&path)?;
        let staged = index.get_path(&path, 0).ok_or_else(|| {
            anyhow::anyhow!("Reviewed file {} is no longer staged", path.display())
        })?;
        if saved.kind() != Some(git2::ObjectType::Blob)
            || saved.id() != staged.id
            || saved.filemode() as u32 != staged.mode
        {
            return Err(anyhow::anyhow!(
                "Reviewed file {} differs from its checkpoint in the Git index",
                path.display()
            ));
        }
        if !std::fs::symlink_metadata(root.join(&path))?
            .file_type()
            .is_file()
        {
            return Err(anyhow::anyhow!(
                "Reviewed file {} is no longer a regular file",
                path.display()
            ));
        }
        let status = repo.status_file(&path)?;
        if status.intersects(
            git2::Status::WT_NEW
                | git2::Status::WT_MODIFIED
                | git2::Status::WT_DELETED
                | git2::Status::WT_TYPECHANGE
                | git2::Status::WT_RENAMED
                | git2::Status::CONFLICTED,
        ) {
            return Err(anyhow::anyhow!(
                "Reviewed file {} changed in the worktree since verification",
                path.display()
            ));
        }
    }
    Ok(())
}

pub async fn fetch_report(task_ref: Option<&str>) -> Result<Option<ReviewReport>> {
    let store = open_persistent_store()?;
    let task = resolve_task(&store, task_ref).await?;
    let Some(task) = task else {
        return Ok(None);
    };
    let events = store.list_events(task.id, 10_000)?;
    Ok(Some(build_report(task, events)))
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
            "approve" => "Task marked Done. Hosted edits were already staged before review.",
            "reject" => "Task marked Rejected. Hosted edits remain staged for manual review.",
            _ => "Task updated.",
        },
    )?;
    let status_json = serde_json::to_value(&status)?;
    let report = ActionReport {
        task_id: task.id.to_string(),
        action: action.into(),
        status: status_json,
        detail: match action {
            "approve" => "Task marked Done. Hosted edits were already staged before review.".into(),
            "reject" => {
                "Task marked Rejected. Hosted edits remain staged for manual review.".into()
            }
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

pub fn build_report(task: TaskRecord, events: Vec<EventRecord>) -> ReviewReport {
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
                    description: phonton_types::task_description_without_prior_context(
                        &description,
                    )
                    .to_string(),
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
            _ => {}
        }
    }

    let mut handoff = task.outcome_ledger.and_then(|ledger| ledger.handoff);
    if let Some(packet) = &mut handoff {
        sanitize_rollback_labels(&mut packet.rollback_points, &checkpoints, &review_items);
    }
    let from_items = cost_receipt_from_items(&review_items);
    let cost_receipt = match &handoff {
        Some(packet)
            if !packet.cost_receipt.route.is_empty()
                || packet.cost_receipt.frontier_equivalent_usd_micros > 0 =>
        {
            packet.cost_receipt.clone()
        }
        _ => from_items,
    };

    ReviewReport {
        task_id: task.id.to_string(),
        goal: task.goal_text,
        status: task.status,
        total_tokens: task.total_tokens,
        cost_receipt,
        handoff,
        checkpoints,
        review_items,
    }
}

fn sanitize_rollback_labels(
    points: &mut [RollbackPoint],
    checkpoints: &[CheckpointItem],
    review_items: &[ReviewItem],
) {
    for point in points {
        if !point.label.trim_start().starts_with("# Prior context") {
            continue;
        }
        let task = checkpoints
            .iter()
            .find(|checkpoint| checkpoint.seq == point.seq)
            .and_then(|checkpoint| {
                review_items
                    .iter()
                    .find(|item| item.subtask_id == checkpoint.subtask_id)
            })
            .map(|item| item.description.as_str());
        point.label = task
            .map(|task| task.chars().take(120).collect())
            .unwrap_or_else(|| format!("Verified checkpoint #{}", point.seq));
    }
}

fn parse_tier(raw: &str) -> ModelTier {
    match raw {
        "local" => ModelTier::Local,
        "cheap" => ModelTier::Cheap,
        "standard" => ModelTier::Standard,
        "frontier" => ModelTier::Frontier,
        _ => ModelTier::Cheap,
    }
}

fn cost_receipt_from_items(items: &[ReviewItem]) -> CostReceipt {
    let mut usage = TokenUsage::default();
    let mut actual = 0u64;
    let mut known = !items.is_empty();
    let mut route = Vec::new();
    for item in items {
        usage.input_tokens = usage
            .input_tokens
            .saturating_add(item.token_usage.input_tokens);
        usage.output_tokens = usage
            .output_tokens
            .saturating_add(item.token_usage.output_tokens);
        usage.cached_tokens = usage
            .cached_tokens
            .saturating_add(item.token_usage.cached_tokens);
        let tier = parse_tier(&item.tier);
        if item.cost.pricing_known {
            actual = actual.saturating_add(item.cost.total_usd_micros);
        } else {
            known = false;
            actual = actual.saturating_add(reference_pricing_for_tier(tier).cost_micros(
                item.token_usage.input_tokens,
                item.token_usage.output_tokens,
            ));
        }
        route.push(RouteStep::new(
            item.model_name.clone(),
            tier,
            RouteOutcome::Passed,
        ));
    }
    CostReceipt::from_usage(actual, &usage, known, route)
}

fn print_text_report(report: &ReviewReport) {
    println!("Phonton review");
    println!("task:   {}", report.task_id);
    println!("goal:   {}", report.goal);
    println!("tokens: {}", report.total_tokens);
    if report.cost_receipt.frontier_equivalent_usd_micros > 0 {
        let pct = report
            .cost_receipt
            .saved_percent()
            .map(|p| format!("{p}%"))
            .unwrap_or_else(|| "n/a".into());
        println!(
            "cost est.: ${:.4}  frontier est.: ${:.4}  saved est.: {}",
            report.cost_receipt.actual_usd_micros as f64 / 1_000_000.0,
            report.cost_receipt.frontier_equivalent_usd_micros as f64 / 1_000_000.0,
            pct
        );
        if !report.cost_receipt.route.is_empty() {
            let hops: Vec<String> = report
                .cost_receipt
                .route
                .iter()
                .map(|step| {
                    let model = if step.model.is_empty() {
                        step.tier.to_string()
                    } else {
                        step.model.clone()
                    };
                    format!("{model} ({})", step.outcome)
                })
                .collect();
            println!("route:  {}", hops.join(" -> "));
        }
    }
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
        let price = if item.cost.pricing_known {
            format!("{} micros estimated", item.cost.total_usd_micros)
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
    }

    #[test]
    fn parse_request_accepts_approve_action() {
        let request = parse_request(&["approve".into(), "latest".into()]).unwrap();
        assert_eq!(request.action, ReviewAction::Approve);
        assert_eq!(request.task_ref.as_deref(), Some("latest"));
    }

    #[test]
    fn approval_requires_reviewing_and_a_verified_change_payload() {
        let id = TaskId::new();
        let make_task = |status: TaskStatus| TaskRecord {
            id,
            goal_text: "edit code".into(),
            status: serde_json::to_value(status).unwrap(),
            created_at: 1,
            total_tokens: 1,
            outcome_ledger: None,
        };
        let mut report = ReviewReport {
            task_id: id.to_string(),
            goal: "edit code".into(),
            status: serde_json::Value::Null,
            total_tokens: 1,
            cost_receipt: CostReceipt::default(),
            handoff: None,
            checkpoints: vec![],
            review_items: vec![],
        };
        let reviewing = || TaskStatus::Reviewing {
            tokens_used: 1,
            estimated_savings_tokens: 0,
        };
        assert!(approval_refusal(&make_task(reviewing()), &report).is_some());
        report.review_items.push(ReviewItem {
            subtask_id: SubtaskId::new().to_string(),
            description: "edit code".into(),
            tier: "standard".into(),
            tokens_used: 1,
            token_usage: TokenUsage::default(),
            cost: CostSummary::default(),
            provider: "test".into(),
            model_name: "test".into(),
            verify: "Pass".into(),
            context: vec![],
            context_token_count: 0,
            diff_hunks: vec![],
        });
        assert!(approval_refusal(&make_task(reviewing()), &report).is_some());
        report.review_items[0].diff_hunks.push(DiffHunk {
            file_path: "code.txt".into(),
            old_start: 1,
            old_count: 0,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine::Added("fixed".into())],
        });
        assert_eq!(approval_refusal(&make_task(reviewing()), &report), None);
        assert!(approval_refusal(&make_task(TaskStatus::Queued), &report).is_some());
        assert!(approval_refusal(
            &make_task(TaskStatus::Running {
                active_subtasks: vec![],
                completed: 0,
                total: 1,
            }),
            &report,
        )
        .is_some());
        assert!(approval_refusal(
            &make_task(TaskStatus::Failed {
                reason: "check failed".into(),
                failed_subtask: None,
            }),
            &report,
        )
        .is_some());
    }

    #[test]
    fn approval_checks_the_reviewed_repository_index_and_worktree() {
        let root = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(root.path()).unwrap();
        let scope = root.path().join("project");
        std::fs::create_dir(&scope).unwrap();
        let path = Path::new("code.txt");
        std::fs::write(scope.join(path), "before\n").unwrap();
        std::fs::write(root.path().join(path), "outer\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("project/code.txt")).unwrap();
        index.add_path(path).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("phonton-test", "test@phonton").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "seed", &tree, &[])
            .unwrap();
        drop(tree);

        let task_id = TaskId::new();
        let subtask_id = SubtaskId::new();
        let hunk = DiffHunk {
            file_path: path.into(),
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![
                DiffLine::Removed("before".into()),
                DiffLine::Added("verified".into()),
            ],
        };
        let mut applier = DiffApplier::open(&scope).unwrap();
        applier
            .apply_verified_hunks(std::slice::from_ref(&hunk))
            .unwrap();
        let checkpoint = applier
            .commit_checkpoint(task_id, subtask_id, 1, "verified", &[path.into()])
            .unwrap();
        let task = TaskRecord {
            id: task_id,
            goal_text: "fix code".into(),
            status: serde_json::to_value(TaskStatus::Reviewing {
                tokens_used: 1,
                estimated_savings_tokens: 0,
            })
            .unwrap(),
            created_at: 1,
            total_tokens: 1,
            outcome_ledger: None,
        };
        let report = ReviewReport {
            task_id: task_id.to_string(),
            goal: "fix code".into(),
            status: task.status.clone(),
            total_tokens: 1,
            cost_receipt: CostReceipt::default(),
            handoff: None,
            checkpoints: vec![CheckpointItem {
                seq: 1,
                subtask_id: subtask_id.to_string(),
                commit_oid: checkpoint.commit_oid,
            }],
            review_items: vec![ReviewItem {
                subtask_id: subtask_id.to_string(),
                description: "fix code".into(),
                tier: "cheap".into(),
                tokens_used: 1,
                token_usage: TokenUsage::default(),
                cost: CostSummary::default(),
                provider: "test".into(),
                model_name: "test".into(),
                verify: "Pass".into(),
                context: vec![],
                context_token_count: 0,
                diff_hunks: vec![hunk],
            }],
        };
        reviewed_checkpoint_identity(&task, &report, &scope).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join(path)).unwrap(),
            "outer\n"
        );
        let mut missing = report.clone();
        missing.checkpoints.clear();
        assert!(reviewed_checkpoint_identity(&task, &missing, &scope).is_err());

        std::fs::write(root.path().join("unrelated.txt"), "user work\n").unwrap();
        let mut index = repo.index().unwrap();
        index.read(true).unwrap();
        index.add_path(Path::new("unrelated.txt")).unwrap();
        index.write().unwrap();
        reviewed_checkpoint_identity(&task, &report, &scope).unwrap();

        std::fs::write(scope.join(path), "changed after review\n").unwrap();
        assert!(reviewed_checkpoint_identity(&task, &report, &scope).is_err());
        let mut index = repo.index().unwrap();
        index.read(true).unwrap();
        index.add_path(Path::new("project/code.txt")).unwrap();
        index.write().unwrap();
        assert!(reviewed_checkpoint_identity(&task, &report, &scope).is_err());
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
                    description: format!(
                        "# Prior context from memory\n- unrelated task{}Implement function `foo`",
                        phonton_types::PRIOR_CONTEXT_TASK_SEPARATOR
                    ),
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
        assert_eq!(
            report.review_items[0].description,
            "Implement function `foo`"
        );
        assert_eq!(report.review_items[0].diff_hunks.len(), 1);
        assert_eq!(report.review_items[0].context_token_count, 11);
        assert_eq!(report.review_items[0].context[0].symbol_name, "foo");
        assert_eq!(report.review_items[0].token_usage.input_tokens, 80);
        assert_eq!(report.review_items[0].cost.total_usd_micros, 120);

        let mut mixed = report.review_items.clone();
        let mut unpriced = mixed[0].clone();
        unpriced.cost.pricing_known = false;
        mixed.push(unpriced);
        assert!(!cost_receipt_from_items(&mixed).pricing_known);
    }

    #[test]
    fn old_memory_checkpoint_label_uses_reviewed_task_description() {
        let subtask_id = SubtaskId::new().to_string();
        let mut points = vec![RollbackPoint {
            seq: 1,
            label: "# Prior context from memory\n- unrelated task".into(),
        }];
        let checkpoints = vec![CheckpointItem {
            seq: 1,
            subtask_id: subtask_id.clone(),
            commit_oid: "abc123".into(),
        }];
        let review_items = vec![ReviewItem {
            subtask_id,
            description: "Implement function `foo`".into(),
            tier: "cheap".into(),
            tokens_used: 1,
            token_usage: TokenUsage::default(),
            cost: CostSummary::default(),
            provider: "test".into(),
            model_name: "test".into(),
            verify: "Pass".into(),
            context: vec![],
            context_token_count: 0,
            diff_hunks: vec![],
        }];
        sanitize_rollback_labels(&mut points, &checkpoints, &review_items);
        assert_eq!(points[0].label, "Implement function `foo`");
    }
}
