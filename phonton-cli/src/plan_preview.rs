//! Non-mutating plan preview for the CLI.
//!
//! `phonton plan <goal>` is the release-path bridge between "typed goal" and
//! "workers edit files": it shows the DAG, tiers, memory influence, and token
//! estimate before execution.

use std::collections::HashMap;

use anyhow::Result;
use phonton_planner::{decompose, decompose_with_memory, Goal};
use phonton_types::{PlannerOutput, SubtaskId};
use serde::Serialize;

use crate::open_persistent_store;

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
    memory_enabled: bool,
    plan: PlannerOutput,
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
            Ok(store) => decompose_with_memory(&goal, &store, None).await?,
            Err(e) => {
                eprintln!("phonton plan: memory unavailable ({e}); planning without memory");
                decompose(&goal)
            }
        }
    } else {
        decompose(&goal)
    };

    let report = PlanReport {
        goal: request.goal,
        memory_enabled: request.options.use_memory,
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
    }

    println!();
    println!("Preview only: no files were changed and no worker was dispatched.");
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
