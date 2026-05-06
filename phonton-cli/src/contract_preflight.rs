use std::path::Path;

use phonton_types::{PlannerOutput, RunCommand, VerifyStepSpec};

/// Apply local workspace signals to a visible goal contract.
///
/// This is intentionally deterministic and cheap: it inspects only common
/// stack marker files so `phonton plan` and the TUI can show the same run and
/// verification contract before any worker is dispatched.
pub fn apply_workspace_preflight(plan: &mut PlannerOutput, working_dir: &Path, goal_text: &str) {
    let Some(contract) = plan.goal_contract.as_mut() else {
        return;
    };
    let lower_goal = goal_text.to_ascii_lowercase();
    let is_chess = lower_goal.contains("chess")
        && (lower_goal.contains("make")
            || lower_goal.contains("build")
            || lower_goal.contains("create"));
    let mut stack_detected = false;

    let package_json = working_dir.join("package.json");
    if package_json.is_file() {
        stack_detected = true;
        if let Ok(text) = std::fs::read_to_string(&package_json) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                let scripts = value.get("scripts").and_then(|scripts| scripts.as_object());
                if scripts.and_then(|scripts| scripts.get("build")).is_some() {
                    push_verify_step(
                        contract,
                        "npm build",
                        vec!["npm".into(), "run".into(), "build".into()],
                    );
                }
                if scripts.and_then(|scripts| scripts.get("test")).is_some() {
                    push_verify_step(contract, "npm test", vec!["npm".into(), "test".into()]);
                }
                if scripts.and_then(|scripts| scripts.get("dev")).is_some() {
                    push_run_command(
                        contract,
                        "Run dev server",
                        vec!["npm".into(), "run".into(), "dev".into()],
                    );
                } else if scripts.and_then(|scripts| scripts.get("start")).is_some() {
                    push_run_command(contract, "Run app", vec!["npm".into(), "start".into()]);
                }
            }
        }
        if is_chess {
            contract.acceptance_criteria.push(
                "In this web/npm workspace, the chess result must run in the app, not just compile as a toy script."
                    .into(),
            );
        }
    }

    if working_dir.join("Cargo.toml").is_file() {
        stack_detected = true;
        push_verify_step(
            contract,
            "cargo test",
            vec!["cargo".into(), "test".into(), "--locked".into()],
        );
        push_run_command(contract, "Run binary", vec!["cargo".into(), "run".into()]);
    }

    if working_dir.join("Makefile").is_file() || working_dir.join("makefile").is_file() {
        stack_detected = true;
        push_verify_step(contract, "make", vec!["make".into()]);
        if is_chess {
            push_run_command(contract, "Run chess binary", vec![".\\chess.exe".into()]);
        }
    }

    if !stack_detected {
        contract
            .assumptions
            .push("No package.json, Cargo.toml, or Makefile was detected before planning.".into());
        if is_chess {
            contract.clarification_questions.push(
                "No project stack was detected. Should Phonton create chess as a web app, terminal game, or native binary?"
                    .into(),
            );
        }
    }
}

fn push_verify_step(contract: &mut phonton_types::GoalContract, label: &str, command: Vec<String>) {
    if contract
        .verify_plan
        .iter()
        .any(|step| step.command.as_ref().map(|cmd| &cmd.command) == Some(&command))
    {
        return;
    }
    contract.verify_plan.push(VerifyStepSpec {
        name: label.into(),
        layer: None,
        command: Some(RunCommand {
            label: label.into(),
            command,
            cwd: None,
        }),
    });
}

fn push_run_command(contract: &mut phonton_types::GoalContract, label: &str, command: Vec<String>) {
    if contract
        .run_plan
        .iter()
        .any(|existing| existing.command == command)
    {
        return;
    }
    contract.run_plan.push(RunCommand {
        label: label.into(),
        command,
        cwd: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_planner::Goal;

    fn plan_for(goal: &str) -> PlannerOutput {
        PlannerOutput {
            subtasks: Vec::new(),
            estimated_total_tokens: 0,
            naive_baseline_tokens: 0,
            coverage_summary: phonton_types::CoverageSummary::default(),
            goal_contract: Some(Goal::new(goal).contract()),
        }
    }

    #[test]
    fn npm_workspace_adds_build_test_and_dev_run_to_contract() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"scripts":{"build":"vite build","test":"vitest","dev":"vite"}}"#,
        )
        .unwrap();

        let mut plan = plan_for("add validation to config loading");
        apply_workspace_preflight(&mut plan, temp.path(), "add validation to config loading");

        let contract = plan.goal_contract.unwrap();
        assert!(contract.verify_plan.iter().any(|step| {
            step.command.as_ref().is_some_and(|cmd| {
                cmd.command == vec!["npm".to_string(), "run".to_string(), "build".to_string()]
            })
        }));
        assert!(contract.verify_plan.iter().any(|step| {
            step.command
                .as_ref()
                .is_some_and(|cmd| cmd.command == vec!["npm".to_string(), "test".to_string()])
        }));
        assert!(contract.run_plan.iter().any(|cmd| {
            cmd.command == vec!["npm".to_string(), "run".to_string(), "dev".to_string()]
        }));
    }

    #[test]
    fn empty_chess_workspace_requires_clarification() {
        let temp = tempfile::tempdir().unwrap();
        let mut plan = plan_for("make chess");

        apply_workspace_preflight(&mut plan, temp.path(), "make chess");

        let contract = plan.goal_contract.unwrap();
        assert!(contract
            .assumptions
            .iter()
            .any(|assumption| assumption.contains("No package.json")));
        assert!(contract
            .clarification_questions
            .iter()
            .any(|question| question.contains("web app, terminal game, or native binary")));
    }
}
