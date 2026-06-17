//! Non-mutating plan preview for the CLI.
//!
//! `phonton plan <goal>` is the release-path bridge between "typed goal" and
//! "workers edit files": it shows the DAG, tiers, memory influence, and token
//! estimate before execution.

use std::collections::HashMap;

use anyhow::Result;
use phonton_planner::{decompose, decompose_with_memory, Goal};
use phonton_types::{
    ConflictGroup, PlanGraph, PlannerOutput, SubtaskAssignment, SubtaskId, SwarmMode,
};
use serde::Serialize;

use crate::{config, store_util::open_persistent_store};

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
pub struct PlanReport {
    goal: String,
    memory_enabled: bool,
    memory_influence_count: usize,
    coverage_warnings: Vec<String>,
    index_backend: String,
    swarm_mode: SwarmMode,
    swarm_reason: String,
    assignments: Vec<SubtaskAssignment>,
    conflict_groups: Vec<ConflictGroup>,
    plan_graph: PlanGraph,
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

    let plan = if request.options.use_memory {
        match open_persistent_store() {
            Ok(store) => {
                let cfg = config::load().unwrap_or_default();
                decompose_with_memory(&goal, &store, crate::load_ask_provider(&cfg)).await?
            }
            Err(e) => {
                eprintln!("phonton plan: memory unavailable ({e}); planning without memory");
                decompose(&goal)
            }
        }
    } else {
        decompose(&goal)
    };

    let report = build_plan_report(&request, plan).await?;

    if request.options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text_report(&report);
    }

    Ok(0)
}

pub async fn build_plan_report(request: &PlanRequest, plan: PlannerOutput) -> Result<PlanReport> {
    Ok(PlanReport {
        goal: request.goal.clone(),
        memory_enabled: request.options.use_memory,
        memory_influence_count: memory_influence_count(&plan),
        coverage_warnings: coverage_warnings(&plan),
        index_backend: config::load()
            .map(|cfg| cfg.index.backend)
            .unwrap_or_else(|_| "local-hnsw".into()),
        swarm_mode: plan.plan_graph.swarm_mode,
        swarm_reason: plan.plan_graph.swarm_reason.clone(),
        assignments: plan.plan_graph.assignments.clone(),
        conflict_groups: plan.plan_graph.conflict_groups.clone(),
        plan_graph: plan.plan_graph.clone(),
        subtasks: subtask_reports(&plan),
        plan,
    })
}

pub async fn build_plan_for_goal(
    goal: &str,
    use_memory: bool,
    no_tests: bool,
) -> Result<PlanReport> {
    let request = PlanRequest {
        goal: goal.to_string(),
        options: PlanOptions {
            json: true,
            use_memory,
            no_tests,
        },
    };
    let mut goal_obj = Goal::new(request.goal.clone());
    goal_obj.no_tests = request.options.no_tests;
    let plan = if request.options.use_memory {
        match open_persistent_store() {
            Ok(store) => {
                let cfg = config::load().unwrap_or_default();
                decompose_with_memory(&goal_obj, &store, crate::load_ask_provider(&cfg)).await?
            }
            Err(_) => decompose(&goal_obj),
        }
    } else {
        decompose(&goal_obj)
    };
    build_plan_report(&request, plan).await
}

fn print_text_report(report: &PlanReport) {
    println!("Phonton plan preview");
    println!("goal:   {}", report.goal);
    println!(
        "memory: {}",
        if report.memory_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "tokens: estimated {} / naive baseline {}",
        report.plan.estimated_total_tokens, report.plan.naive_baseline_tokens
    );
    println!(
        "coverage: {} new functions, {} tests planned",
        report.plan.coverage_summary.new_functions, report.plan.coverage_summary.tests_planned
    );
    println!("index: {}", report.index_backend);
    println!("swarm: {:?} ({})", report.swarm_mode, report.swarm_reason);
    for warning in &report.coverage_warnings {
        println!("warning: {warning}");
    }
    println!();

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
        println!(
            "{}. [{:?}] {}",
            idx + 1,
            subtask.model_tier,
            first_line(&subtask.description)
        );
        println!("   id: {}", subtask.id);
        println!("   depends_on: {deps}");
        if subtask.description.contains("# Prior context") {
            println!("   memory: applied to this subtask");
        }
        let areas = expected_touched_areas(&subtask.description);
        if !areas.is_empty() {
            println!("   expected areas: {}", areas.join(", "));
        }
    }

    println!();
    println!("Preview only: no files were changed and no worker was dispatched.");
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
}
