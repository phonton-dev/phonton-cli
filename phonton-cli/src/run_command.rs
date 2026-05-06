//! Run the latest command captured in a Phonton handoff packet.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use phonton_sandbox::{Sandbox, ToolCall};
use phonton_store::TaskRecord;
use phonton_types::{RunCommand, TaskId};
use serde::Serialize;

use crate::{command_runner::summarize_output, open_persistent_store};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    pub task_ref: Option<String>,
    pub index: usize,
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
struct RunReceipt {
    task_id: String,
    goal: String,
    command_index: usize,
    command: String,
    cwd: String,
    exit_code: Option<i32>,
    duration_ms: u128,
    stdout_preview: String,
    stderr_preview: String,
}

pub fn parse_request(args: &[String]) -> Result<RunRequest> {
    let mut task_ref = None;
    let mut index = 0usize;
    let mut json = false;
    let mut positionals = Vec::new();
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--index" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--index requires a 1-based command number"))?;
                let parsed = raw
                    .parse::<usize>()
                    .map_err(|_| anyhow::anyhow!("--index requires a positive integer"))?;
                if parsed == 0 {
                    return Err(anyhow::anyhow!("--index must be greater than zero"));
                }
                index = parsed - 1;
            }
            "-h" | "--help" => {
                return Err(anyhow::anyhow!(
                    "usage: phonton run [--json] [--index <n>] [latest|<task-id>]"
                ));
            }
            other if other.starts_with('-') => {
                return Err(anyhow::anyhow!("unknown run option `{other}`"));
            }
            other => positionals.push(other.to_string()),
        }
    }

    if positionals.len() > 1 {
        return Err(anyhow::anyhow!("run accepts at most one task id"));
    }
    if let Some(positional) = positionals.pop() {
        task_ref = Some(positional);
    }

    Ok(RunRequest {
        task_ref,
        index,
        json,
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
            eprintln!("phonton run: {msg}");
            eprintln!("Run `phonton run --help` for usage.");
            return Ok(2);
        }
    };

    let store = match open_persistent_store() {
        Ok(store) => store,
        Err(e) => {
            eprintln!("phonton run: persistent store unavailable: {e}");
            return Ok(1);
        }
    };

    let task = match resolve_task(&store, request.task_ref.as_deref()).await? {
        Some(task) => task,
        None => {
            eprintln!("phonton run: no matching task found");
            return Ok(1);
        }
    };
    let commands = commands_for_task(&task);
    let Some(command) = commands.get(request.index) else {
        eprintln!(
            "phonton run: command #{} not found for task {} ({} command(s) available)",
            request.index + 1,
            task.id,
            commands.len()
        );
        return Ok(1);
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let root = match command_working_dir(&cwd, command) {
        Ok(root) => root,
        Err(e) => {
            eprintln!("phonton run: {e}");
            return Ok(1);
        }
    };
    let Some((program, args)) = split_run_command(command) else {
        eprintln!("phonton run: selected command is empty");
        return Ok(1);
    };

    let sandbox = Sandbox::new(root.clone(), format!("phonton-run-{}", task.id));
    let started = Instant::now();
    let output = match sandbox.run_tool(ToolCall::Run { program, args }).await {
        Ok(output) => output,
        Err(e) => {
            eprintln!("phonton run: {e}");
            return Ok(1);
        }
    };
    let duration_ms = started.elapsed().as_millis();
    let summary = summarize_output(
        &command.command.join(" "),
        output.status.code(),
        &output.stdout,
        &output.stderr,
    );
    let receipt = RunReceipt {
        task_id: task.id.to_string(),
        goal: task.goal_text,
        command_index: request.index + 1,
        command: summary.command,
        cwd: root.display().to_string(),
        exit_code: summary.exit_code,
        duration_ms,
        stdout_preview: summary.stdout_preview,
        stderr_preview: summary.stderr_preview,
    };

    if request.json {
        println!("{}", serde_json::to_string_pretty(&receipt)?);
    } else {
        print_receipt(&receipt);
    }

    Ok(if output.status.success() { 0 } else { 1 })
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
    serde_json::from_value(json).map_err(Into::into)
}

fn commands_for_task(task: &TaskRecord) -> Vec<RunCommand> {
    let Some(ledger) = &task.outcome_ledger else {
        return Vec::new();
    };
    if let Some(handoff) = &ledger.handoff {
        if !handoff.run_commands.is_empty() {
            return handoff.run_commands.clone();
        }
    }
    ledger
        .goal_contract
        .as_ref()
        .map(|contract| contract.run_plan.clone())
        .unwrap_or_default()
}

fn split_run_command(command: &RunCommand) -> Option<(String, Vec<String>)> {
    let mut parts = command.command.clone();
    if parts.is_empty() {
        return None;
    }
    let program = parts.remove(0);
    Some((program, parts))
}

fn command_working_dir(base: &Path, command: &RunCommand) -> Result<PathBuf> {
    let base = lexical_normalize(base);
    let Some(cwd) = &command.cwd else {
        return Ok(base);
    };
    let candidate = if cwd.is_absolute() {
        cwd.clone()
    } else {
        base.join(cwd)
    };
    let candidate = lexical_normalize(&candidate);
    if !candidate.starts_with(&base) {
        return Err(anyhow::anyhow!(
            "command cwd `{}` is outside the current workspace",
            cwd.display()
        ));
    }
    Ok(candidate)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn print_receipt(receipt: &RunReceipt) {
    println!("Phonton run");
    println!("task:     {}", receipt.task_id);
    println!("goal:     {}", receipt.goal);
    println!("command:  {}", receipt.command);
    println!("cwd:      {}", receipt.cwd);
    println!("exit:     {}", exit_text(receipt.exit_code));
    println!("duration: {}ms", receipt.duration_ms);
    if !receipt.stdout_preview.trim().is_empty() {
        println!();
        println!("stdout:");
        println!("{}", receipt.stdout_preview.trim_end());
    }
    if !receipt.stderr_preview.trim().is_empty() {
        println!();
        println!("stderr:");
        println!("{}", receipt.stderr_preview.trim_end());
    }
}

fn exit_text(exit_code: Option<i32>) -> String {
    exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "terminated".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::{
        ContextManifest, GoalContract, OutcomeLedger, PermissionLedger, VerifyReport,
    };

    #[test]
    fn parse_request_defaults_to_latest_first_command() {
        let request = parse_request(&[]).unwrap();

        assert!(request.task_ref.is_none());
        assert_eq!(request.index, 0);
        assert!(!request.json);
    }

    #[test]
    fn parse_request_accepts_task_json_and_index() {
        let request = parse_request(&[
            "--json".into(),
            "--index".into(),
            "2".into(),
            "latest".into(),
        ])
        .unwrap();

        assert_eq!(request.task_ref.as_deref(), Some("latest"));
        assert_eq!(request.index, 1);
        assert!(request.json);
    }

    #[test]
    fn commands_prefer_handoff_then_fallback_to_contract() {
        let task_id = TaskId::new();
        let contract_command = RunCommand {
            label: "Contract".into(),
            command: vec!["cargo".into(), "run".into()],
            cwd: None,
        };
        let handoff_command = RunCommand {
            label: "Receipt".into(),
            command: vec!["cargo".into(), "test".into()],
            cwd: None,
        };
        let contract = GoalContract {
            goal: "g".into(),
            task_class: phonton_types::TaskClass::CoreLogic,
            confidence_percent: 80,
            acceptance_criteria: Vec::new(),
            expected_artifacts: Vec::new(),
            likely_files: Vec::new(),
            verify_plan: Vec::new(),
            run_plan: vec![contract_command.clone()],
            quality_floor: phonton_types::QualityFloor {
                criteria: Vec::new(),
            },
            clarification_questions: Vec::new(),
            assumptions: Vec::new(),
        };
        let mut ledger = OutcomeLedger {
            task_id,
            goal_contract: Some(contract),
            context_manifest: ContextManifest::default(),
            permission_ledger: PermissionLedger::default(),
            verify_report: VerifyReport::default(),
            handoff: None,
        };
        let task = TaskRecord {
            id: task_id,
            goal_text: "g".into(),
            status: serde_json::json!({}),
            created_at: 1,
            total_tokens: 0,
            outcome_ledger: Some(ledger.clone()),
        };
        assert_eq!(commands_for_task(&task), vec![contract_command]);

        ledger.handoff = Some(phonton_types::HandoffPacket {
            task_id,
            goal: "g".into(),
            headline: "ready".into(),
            changed_files: Vec::new(),
            generated_artifacts: Vec::new(),
            diff_stats: phonton_types::DiffStats::default(),
            verification: VerifyReport::default(),
            run_commands: vec![handoff_command.clone()],
            known_gaps: Vec::new(),
            review_actions: Vec::new(),
            rollback_points: Vec::new(),
            token_usage: phonton_types::TokenUsage::default(),
            influence: phonton_types::InfluenceSummary::default(),
        });
        let task = TaskRecord {
            outcome_ledger: Some(ledger),
            ..task
        };

        assert_eq!(commands_for_task(&task), vec![handoff_command]);
    }

    #[test]
    fn command_working_dir_rejects_parent_escape() {
        let command = RunCommand {
            label: "x".into(),
            command: vec!["cargo".into(), "test".into()],
            cwd: Some(PathBuf::from("..")),
        };

        assert!(command_working_dir(Path::new("C:/work/repo"), &command).is_err());
    }
}
