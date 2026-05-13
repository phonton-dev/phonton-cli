//! Non-mutating plan preview for the CLI.
//!
//! `phonton plan <goal>` is the release-path bridge between "typed goal" and
//! "workers edit files": it shows the DAG, tiers, memory influence, and token
//! estimate before execution.

use std::collections::HashMap;
use std::fmt::Write as _;

use anyhow::Result;
use phonton_planner::{decompose, decompose_with_memory, Goal};
use phonton_types::{GoalContract, PlannerOutput, RunCommand, SubtaskId};
use serde::Serialize;

use crate::{contract_preflight::apply_workspace_preflight, open_persistent_store};

#[derive(Debug, Clone, Copy, Default)]
pub struct PlanOptions {
    pub json: bool,
    pub use_memory: bool,
    pub no_tests: bool,
}

impl PlanOptions {
    fn with_defaults() -> Self {
        Self {
            json: false,
            use_memory: true,
            no_tests: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlanRequest {
    pub goal: String,
    pub options: PlanOptions,
}

#[derive(Debug, Clone, Serialize)]
struct PlanReport {
    goal: String,
    goal_contract: Option<GoalContract>,
    memory_enabled: bool,
    memory_influence_count: usize,
    coverage_warnings: Vec<String>,
    subtasks: Vec<PlanSubtaskReport>,
    plan: PlannerOutput,
}

#[derive(Debug, Clone, Serialize)]
struct PlanSubtaskReport {
    id: String,
    description: String,
    tier: String,
    dependencies: Vec<String>,
    expected_touched_areas: Vec<String>,
    memory_influenced: bool,
}

pub fn parse_request(args: &[String]) -> Result<PlanRequest> {
    let mut options = PlanOptions::with_defaults();
    let mut goal_parts = Vec::new();

    for arg in args {
        match arg.as_str() {
            "--json" => options.json = true,
            "--no-memory" => options.use_memory = false,
            "--no-tests" => options.no_tests = true,
            "-h" | "--help" => {
                return Err(anyhow::anyhow!(
                    "usage: phonton plan [--json] [--no-memory] [--no-tests] <goal>"
                ));
            }
            other if other.starts_with('-') => {
                return Err(anyhow::anyhow!("unknown plan option `{other}`"));
            }
            other => goal_parts.push(other.to_string()),
        }
    }

    let goal = goal_parts.join(" ");
    if goal.trim().is_empty() {
        return Err(anyhow::anyhow!(
            "`phonton plan` requires a goal, e.g. `phonton plan add a function parse_callsites`"
        ));
    }

    Ok(PlanRequest { goal, options })
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
            eprintln!("phonton plan: {msg}");
            eprintln!("Run `phonton plan --help` for usage.");
            return Ok(2);
        }
    };

    let mut goal = Goal::new(request.goal.clone());
    goal.no_tests = request.options.no_tests;

    let mut plan = if request.options.use_memory {
        match open_persistent_store() {
            Ok(store) => decompose_with_memory(&goal, &store, None).await?,
            Err(e) => {
                eprintln!("phonton plan: memory unavailable ({e}); planning without memory");
                decompose(&goal)
            }
        }
    } else {
        decompose(&goal)
    };
    let working_dir = std::env::current_dir().unwrap_or_else(|_| ".".into());
    apply_workspace_preflight(&mut plan, &working_dir, &request.goal);

    let report = PlanReport {
        goal: request.goal,
        goal_contract: plan.goal_contract.clone(),
        memory_enabled: request.options.use_memory,
        memory_influence_count: memory_influence_count(&plan),
        coverage_warnings: coverage_warnings(&plan),
        subtasks: subtask_reports(&plan),
        plan,
    };

    if request.options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text_report(&report);
    }

    Ok(0)
}

fn print_text_report(report: &PlanReport) {
    print!("{}", format_text_report(report));
}

fn format_text_report(report: &PlanReport) -> String {
    let mut out = String::new();
    writeln!(out, "Phonton plan preview").ok();
    writeln!(out, "goal:   {}", report.goal).ok();
    writeln!(
        out,
        "memory: {}",
        if report.memory_enabled {
            "enabled"
        } else {
            "disabled"
        }
    )
    .ok();
    writeln!(
        out,
        "tokens: estimated {} / naive baseline {}",
        report.plan.estimated_total_tokens, report.plan.naive_baseline_tokens
    )
    .ok();
    writeln!(
        out,
        "coverage: {} new functions, {} tests planned",
        report.plan.coverage_summary.new_functions, report.plan.coverage_summary.tests_planned
    )
    .ok();
    for warning in &report.coverage_warnings {
        writeln!(out, "warning: {warning}").ok();
    }
    writeln!(out).ok();

    if let Some(contract) = &report.goal_contract {
        append_goal_contract(&mut out, contract);
        writeln!(out).ok();
    }

    let id_to_index: HashMap<SubtaskId, usize> = report
        .plan
        .subtasks
        .iter()
        .enumerate()
        .map(|(idx, subtask)| (subtask.id, idx + 1))
        .collect();

    for (idx, subtask) in report.plan.subtasks.iter().enumerate() {
        let deps = if subtask.dependencies.is_empty() {
            "none".to_string()
        } else {
            subtask
                .dependencies
                .iter()
                .filter_map(|id| id_to_index.get(id))
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        writeln!(
            out,
            "{}. [{:?}] {}",
            idx + 1,
            subtask.model_tier,
            first_line(&subtask.description)
        )
        .ok();
        writeln!(out, "   id: {}", subtask.id).ok();
        writeln!(out, "   depends_on: {deps}").ok();
        if subtask.description.contains("# Prior context") {
            writeln!(out, "   memory: applied to this subtask").ok();
        }
        let areas = expected_touched_areas(&subtask.description);
        if !areas.is_empty() {
            writeln!(out, "   expected areas: {}", areas.join(", ")).ok();
        }
    }

    writeln!(out).ok();
    writeln!(
        out,
        "Preview only: no files were changed and no worker was dispatched."
    )
    .ok();
    out
}

fn append_goal_contract(out: &mut String, contract: &GoalContract) {
    writeln!(out, "GoalContract").ok();
    writeln!(
        out,
        "  class: {} ({}% confidence)",
        contract.task_class, contract.confidence_percent
    )
    .ok();
    if let Some(intent) = &contract.intent {
        writeln!(
            out,
            "  intent: {:?}; ambiguity {:?}; blast {:?}; runtime {:?}; token {:?}",
            intent.recommended_action,
            intent.ambiguity,
            intent.blast_radius,
            intent.runtime_risk,
            intent.token_risk
        )
        .ok();
    }
    append_string_list(out, "  acceptance:", &contract.acceptance_criteria);
    if !contract.acceptance_slices.is_empty() {
        writeln!(out, "  acceptance slices:").ok();
        for slice in &contract.acceptance_slices {
            match &slice.artifact_path {
                Some(path) => writeln!(
                    out,
                    "    - {}: {} ({})",
                    slice.id,
                    slice.criterion,
                    path.display()
                )
                .ok(),
                None => writeln!(out, "    - {}: {}", slice.id, slice.criterion).ok(),
            };
        }
    }
    if contract.expected_artifacts.is_empty() {
        writeln!(out, "  expected artifacts: none inferred").ok();
    } else {
        writeln!(out, "  expected artifacts:").ok();
        for artifact in &contract.expected_artifacts {
            match &artifact.path {
                Some(path) => {
                    writeln!(out, "    - {} ({})", artifact.description, path.display()).ok()
                }
                None => writeln!(out, "    - {}", artifact.description).ok(),
            };
        }
    }
    if contract.likely_files.is_empty() {
        writeln!(out, "  likely files: none inferred").ok();
    } else {
        writeln!(
            out,
            "  likely files: {}",
            contract
                .likely_files
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
        .ok();
    }
    if contract.verify_plan.is_empty() {
        writeln!(out, "  verify plan: no verifier inferred yet").ok();
    } else {
        writeln!(out, "  verify plan:").ok();
        for step in &contract.verify_plan {
            let command = step
                .command
                .as_ref()
                .map(format_command)
                .unwrap_or_else(|| "manual check".into());
            writeln!(out, "    - {}: {}", step.name, command).ok();
        }
    }
    if contract.run_plan.is_empty() {
        writeln!(out, "  run plan: no run command inferred yet").ok();
    } else {
        writeln!(out, "  run plan:").ok();
        for command in &contract.run_plan {
            writeln!(out, "    - {}: {}", command.label, format_command(command)).ok();
        }
    }
    append_string_list(out, "  quality floor:", &contract.quality_floor.criteria);
    writeln!(
        out,
        "  token policy: first_attempt_cap={}, broad_repair={}, surgical_repair={}",
        contract
            .token_policy
            .first_attempt_cap_tokens
            .map(|tokens| tokens.to_string())
            .unwrap_or_else(|| "none".into()),
        contract.token_policy.allow_broad_repair,
        contract.token_policy.repair_only_missing_criteria
    )
    .ok();
    append_string_list(out, "  token notes:", &contract.token_policy.notes);
    append_string_list(out, "  assumptions:", &contract.assumptions);
    append_string_list(out, "  clarifications:", &contract.clarification_questions);
}

fn append_string_list(out: &mut String, label: &str, items: &[String]) {
    if items.is_empty() {
        writeln!(out, "{label} none").ok();
        return;
    }
    writeln!(out, "{label}").ok();
    for item in items {
        writeln!(out, "    - {item}").ok();
    }
}

fn format_command(command: &RunCommand) -> String {
    let mut text = command.command.join(" ");
    if let Some(cwd) = &command.cwd {
        text.push_str(&format!("  (cwd: {})", cwd.display()));
    }
    text
}

fn subtask_reports(plan: &PlannerOutput) -> Vec<PlanSubtaskReport> {
    plan.subtasks
        .iter()
        .map(|subtask| PlanSubtaskReport {
            id: subtask.id.to_string(),
            description: first_line(&subtask.description).to_string(),
            tier: format!("{:?}", subtask.model_tier),
            dependencies: subtask
                .dependencies
                .iter()
                .map(ToString::to_string)
                .collect(),
            expected_touched_areas: expected_touched_areas(&subtask.description),
            memory_influenced: subtask.description.contains("# Prior context"),
        })
        .collect()
}

fn memory_influence_count(plan: &PlannerOutput) -> usize {
    plan.subtasks
        .iter()
        .filter(|s| s.description.contains("# Prior context"))
        .count()
}

fn coverage_warnings(plan: &PlannerOutput) -> Vec<String> {
    let mut warnings = Vec::new();
    if plan.coverage_summary.new_functions == 0 && plan.coverage_summary.tests_planned == 0 {
        warnings.push(
            "No concrete symbols detected; review the generated subtask before running.".into(),
        );
    }
    if plan.coverage_summary.new_functions > 0 && plan.coverage_summary.tests_planned == 0 {
        warnings.push(
            "Tests are disabled or were not planned for detected implementation work.".into(),
        );
    }
    warnings
}

fn expected_touched_areas(description: &str) -> Vec<String> {
    let lower = description.to_ascii_lowercase();
    let mut areas = Vec::new();
    for (needle, area) in [
        ("config", "configuration"),
        ("memory", "memory store"),
        ("review", "review surface"),
        ("plan", "planner"),
        ("test", "tests"),
        ("cli", "cli"),
        ("provider", "providers"),
        ("verify", "verification"),
        ("diff", "diff/checkpointing"),
    ] {
        if lower.contains(needle) && !areas.iter().any(|a| a == area) {
            areas.push(area.to_string());
        }
    }
    areas
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_collects_goal_and_flags() {
        let args = vec![
            "--json".into(),
            "--no-memory".into(),
            "--no-tests".into(),
            "add".into(),
            "function".into(),
            "parse_callsites".into(),
        ];
        let request = parse_request(&args).unwrap();
        assert_eq!(request.goal, "add function parse_callsites");
        assert!(request.options.json);
        assert!(!request.options.use_memory);
        assert!(request.options.no_tests);
    }

    #[test]
    fn first_line_ignores_memory_preamble_body() {
        assert_eq!(
            first_line("# Prior context\n- x\n\nImplement thing"),
            "# Prior context"
        );
    }

    #[test]
    fn text_report_includes_visible_goal_contract_sections() {
        let plan = PlannerOutput {
            subtasks: Vec::new(),
            estimated_total_tokens: 1200,
            naive_baseline_tokens: 4000,
            coverage_summary: phonton_types::CoverageSummary::default(),
            goal_contract: Some(Goal::new("make chess").contract()),
        };
        let report = PlanReport {
            goal: "make chess".into(),
            goal_contract: plan.goal_contract.clone(),
            memory_enabled: false,
            memory_influence_count: 0,
            coverage_warnings: Vec::new(),
            subtasks: subtask_reports(&plan),
            plan,
        };

        let text = format_text_report(&report);

        assert!(text.contains("GoalContract"));
        assert!(text.contains("acceptance:"));
        assert!(text.contains("verify plan:"));
        assert!(text.contains("run plan:"));
        assert!(text.contains("quality floor:"));
        assert!(text.contains("Preview only"));
    }
}
