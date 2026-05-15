//! Verified diff export for completed/reviewable Phonton tasks.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{anyhow, bail, Result};
use phonton_store::TaskRecord;
use phonton_types::{DiffHunk, DiffLine, EventRecord, OrchestratorEvent, TaskId};
use serde::Serialize;

use crate::open_persistent_store;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffRenderMode {
    Unified,
    Stat,
    NameOnly,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DiffOptions {
    pub json: bool,
    pub stat: bool,
    pub name_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffRequest {
    pub task_ref: Option<String>,
    pub options: DiffOptions,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct DiffReport {
    pub task_id: String,
    pub goal: String,
    pub total_tokens: u64,
    pub status: serde_json::Value,
    pub files_changed: usize,
    pub total_added: usize,
    pub total_removed: usize,
    pub files: Vec<DiffFile>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct DiffFile {
    pub path: String,
    pub added_lines: usize,
    pub removed_lines: usize,
    pub hunks: Vec<DiffHunk>,
}

pub async fn run(args: &[String]) -> Result<i32> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{}", diff_help());
        return Ok(0);
    }
    let request = parse_args(args)?;
    let store = open_persistent_store()?;
    let Some(task) = resolve_task(&store, request.task_ref.as_deref()).await? else {
        eprintln!("phonton diff: no task found");
        return Ok(1);
    };
    let events = store.list_events(task.id, 10_000)?;
    let report = build_diff_report(task, events);

    if report.files.is_empty() {
        eprintln!(
            "phonton diff: no verified diff hunks found for task {}",
            report.task_id
        );
        eprintln!("Only review-ready SubtaskReviewReady events are exported as diffs.");
        return Ok(1);
    }

    if request.options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let mode = if request.options.name_only {
            DiffRenderMode::NameOnly
        } else if request.options.stat {
            DiffRenderMode::Stat
        } else {
            DiffRenderMode::Unified
        };
        print!("{}", render_text_diff(&report, mode));
    }

    Ok(0)
}

pub(crate) fn parse_args(args: &[String]) -> Result<DiffRequest> {
    let mut request = DiffRequest {
        task_ref: None,
        options: DiffOptions::default(),
    };

    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--json" => request.options.json = true,
            "--stat" => request.options.stat = true,
            "--name-only" => request.options.name_only = true,
            value if value.starts_with('-') => bail!("phonton diff: unknown flag `{value}`"),
            value => {
                if request.task_ref.is_some() {
                    bail!("phonton diff: expected at most one task id or `latest`");
                }
                request.task_ref = Some(value.to_string());
            }
        }
        idx += 1;
    }

    let mode_count = usize::from(request.options.json)
        + usize::from(request.options.stat)
        + usize::from(request.options.name_only);
    if mode_count > 1 {
        bail!("phonton diff: choose only one of --json, --stat, or --name-only");
    }

    Ok(request)
}

pub(crate) fn build_diff_report(task: TaskRecord, events: Vec<EventRecord>) -> DiffReport {
    let mut grouped: BTreeMap<String, Vec<DiffHunk>> = BTreeMap::new();
    for event in events {
        if let OrchestratorEvent::SubtaskReviewReady { diff_hunks, .. } = event.event {
            for hunk in diff_hunks {
                grouped
                    .entry(diff_path(&hunk.file_path))
                    .or_default()
                    .push(hunk);
            }
        }
    }

    let mut total_added = 0;
    let mut total_removed = 0;
    let files: Vec<DiffFile> = grouped
        .into_iter()
        .map(|(path, hunks)| {
            let (added_lines, removed_lines) = count_hunks(&hunks);
            total_added += added_lines;
            total_removed += removed_lines;
            DiffFile {
                path,
                added_lines,
                removed_lines,
                hunks,
            }
        })
        .collect();

    DiffReport {
        task_id: task.id.to_string(),
        goal: task.goal_text,
        total_tokens: task.total_tokens,
        status: task.status,
        files_changed: files.len(),
        total_added,
        total_removed,
        files,
    }
}

pub(crate) fn render_text_diff(report: &DiffReport, mode: DiffRenderMode) -> String {
    match mode {
        DiffRenderMode::NameOnly => {
            let mut out = String::new();
            for file in &report.files {
                out.push_str(&file.path);
                out.push('\n');
            }
            out
        }
        DiffRenderMode::Stat => render_stat(report),
        DiffRenderMode::Unified => render_unified(report),
    }
}

fn render_stat(report: &DiffReport) -> String {
    let mut out = String::from("Phonton diff stat\n");
    out.push_str(&format!("task: {}\n", report.task_id));
    for file in &report.files {
        out.push_str(&format!(
            "{} +{} -{} ({} {})\n",
            file.path,
            file.added_lines,
            file.removed_lines,
            file.hunks.len(),
            plural(file.hunks.len(), "hunk", "hunks")
        ));
    }
    out.push_str(&format!(
        "total +{} -{} across {} {}\n",
        report.total_added,
        report.total_removed,
        report.files_changed,
        plural(report.files_changed, "file", "files")
    ));
    out
}

fn render_unified(report: &DiffReport) -> String {
    let mut out = String::from("Phonton diff\n");
    out.push_str(&format!("task: {}\n", report.task_id));
    out.push_str(&format!("goal: {}\n", report.goal));
    out.push_str(&format!(
        "files: {}  +{} -{}\n\n",
        report.files_changed, report.total_added, report.total_removed
    ));

    for file in &report.files {
        out.push_str(&format!("diff --git a/{} b/{}\n", file.path, file.path));
        out.push_str(&format!("--- a/{}\n", file.path));
        out.push_str(&format!("+++ b/{}\n", file.path));
        for hunk in &file.hunks {
            out.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            ));
            for line in &hunk.lines {
                match line {
                    DiffLine::Context(text) => {
                        out.push(' ');
                        out.push_str(text);
                    }
                    DiffLine::Added(text) => {
                        out.push('+');
                        out.push_str(text);
                    }
                    DiffLine::Removed(text) => {
                        out.push('-');
                        out.push_str(text);
                    }
                }
                out.push('\n');
            }
        }
        out.push('\n');
    }
    out
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

fn parse_task_id(raw: &str) -> Result<TaskId> {
    let json = serde_json::Value::String(raw.to_string());
    serde_json::from_value(json).map_err(|e| anyhow!("invalid task id `{raw}`: {e}"))
}

fn count_hunks(hunks: &[DiffHunk]) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for hunk in hunks {
        for line in &hunk.lines {
            match line {
                DiffLine::Added(_) => added += 1,
                DiffLine::Removed(_) => removed += 1,
                DiffLine::Context(_) => {}
            }
        }
    }
    (added, removed)
}

fn diff_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 {
        singular
    } else {
        plural
    }
}

fn diff_help() -> &'static str {
    "phonton diff [--json|--stat|--name-only] [latest|<task-id>]\n\
     \n\
     Prints only verified SubtaskReviewReady diff hunks. Failed or pre-verification\n\
     worker output is intentionally excluded from this review surface."
}

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_store::TaskRecord;
    use phonton_types::{
        DiffHunk, DiffLine, EventRecord, OrchestratorEvent, SubtaskId, TaskId, VerifyLayer,
        VerifyResult,
    };

    #[test]
    fn parse_args_supports_output_modes_and_task_refs() {
        let request = parse_args(&["--stat".into(), "latest".into()]).unwrap();

        assert!(request.options.stat);
        assert_eq!(request.task_ref.as_deref(), Some("latest"));

        assert!(parse_args(&["--stat".into(), "--name-only".into()]).is_err());
    }

    #[test]
    fn report_groups_verified_review_hunks_and_counts_lines() {
        let task_id = TaskId::new();
        let report = build_diff_report(task(task_id), vec![review_ready_event(task_id)]);

        assert_eq!(report.task_id, task_id.to_string());
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].path, "src/chess.ts");
        assert_eq!(report.files[0].added_lines, 2);
        assert_eq!(report.files[0].removed_lines, 1);
        assert_eq!(report.total_added, 2);
        assert_eq!(report.total_removed, 1);
    }

    #[test]
    fn text_renderer_outputs_reviewable_unified_diff() {
        let task_id = TaskId::new();
        let report = build_diff_report(task(task_id), vec![review_ready_event(task_id)]);
        let rendered = render_text_diff(&report, DiffRenderMode::Unified);

        assert!(rendered.contains("Phonton diff"));
        assert!(rendered.contains("diff --git a/src/chess.ts b/src/chess.ts"));
        assert!(rendered.contains("@@ -1,2 +1,3 @@"));
        assert!(rendered.contains("-console.log('placeholder')"));
        assert!(rendered.contains("+export const board = createInitialBoard();"));
    }

    #[test]
    fn stat_and_name_only_render_compact_review_surfaces() {
        let task_id = TaskId::new();
        let report = build_diff_report(task(task_id), vec![review_ready_event(task_id)]);

        assert_eq!(
            render_text_diff(&report, DiffRenderMode::NameOnly).trim(),
            "src/chess.ts"
        );

        let stat = render_text_diff(&report, DiffRenderMode::Stat);
        assert!(stat.contains("src/chess.ts +2 -1 (1 hunk)"));
        assert!(stat.contains("total +2 -1 across 1 file"));
    }

    fn task(id: TaskId) -> TaskRecord {
        TaskRecord {
            id,
            goal_text: "build chess".into(),
            status: serde_json::json!({"Reviewing":{"tokens_used":42,"estimated_savings_tokens":100}}),
            created_at: 1,
            total_tokens: 42,
            outcome_ledger: None,
        }
    }

    fn review_ready_event(task_id: TaskId) -> EventRecord {
        EventRecord {
            task_id,
            timestamp_ms: 1,
            event: OrchestratorEvent::SubtaskReviewReady {
                subtask_id: SubtaskId::new(),
                description: "add board".into(),
                tier: phonton_types::ModelTier::Standard,
                tokens_used: 42,
                token_usage: phonton_types::TokenUsage::default(),
                cost: phonton_types::CostSummary::default(),
                provider: phonton_types::ProviderKind::OpenAI,
                model_name: "gpt-test".into(),
                verify_result: VerifyResult::Pass {
                    layer: VerifyLayer::Test,
                },
                diff_hunks: vec![DiffHunk {
                    file_path: "src/chess.ts".into(),
                    old_start: 1,
                    old_count: 2,
                    new_start: 1,
                    new_count: 3,
                    lines: vec![
                        DiffLine::Removed("console.log('placeholder')".into()),
                        DiffLine::Added("import { createInitialBoard } from './rules';".into()),
                        DiffLine::Added("export const board = createInitialBoard();".into()),
                        DiffLine::Context("render(board);".into()),
                    ],
                }],
            },
        }
    }
}
