use std::path::{Path, PathBuf};

use phonton_types::{ExpectedArtifact, PlannerOutput, RunCommand, VerifyStepSpec};

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
    let wants_static_html = is_chess && (lower_goal.contains("html") || lower_goal.contains("web"));
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
            contract.clarification_questions.retain(|question| {
                !question.contains("No project stack was detected")
                    && !question.contains("What exact behavior or artifact")
            });
            if wants_static_html {
                contract.assumptions.push(
                    "No project stack was detected; defaulting to a self-contained static HTML chess page."
                        .into(),
                );
                contract.acceptance_criteria.push(
                    "In an empty workspace, create a self-contained playable chess page in index.html."
                        .into(),
                );
                push_expected_artifact(
                    contract,
                    "Static browser chess page",
                    Some(PathBuf::from("index.html")),
                );
                push_likely_file(contract, PathBuf::from("index.html"));
                push_verify_step(
                    contract,
                    "index.html exists",
                    vec![
                        "python".into(),
                        "-c".into(),
                        "from pathlib import Path; p=Path('index.html'); assert p.is_file() and p.read_text(encoding='utf-8').strip()".into(),
                    ],
                );
                push_run_command(
                    contract,
                    "Serve static chess page",
                    vec![
                        "python".into(),
                        "-m".into(),
                        "http.server".into(),
                        "8000".into(),
                    ],
                );
                for subtask in &mut plan.subtasks {
                    if subtask_matches_goal(subtask, goal_text) {
                        subtask.description = format!(
                            "{}\n\nDefault empty-workspace target: create a self-contained playable chess page in index.html with embedded CSS and JavaScript. Include an 8x8 board, named pieces, turn handling, legal/valid move checks, reset or new-game behavior, and a clear way to run it with `python -m http.server 8000`.",
                            subtask.description
                        );
                    }
                }
            } else {
                contract.assumptions.push(
                    "No project stack was detected; defaulting to a self-contained Python terminal chess game."
                        .into(),
                );
                contract.acceptance_criteria.push(
                    "In an empty workspace, create a self-contained terminal chess game in chess.py."
                        .into(),
                );
                push_expected_artifact(
                    contract,
                    "Terminal chess game",
                    Some(PathBuf::from("chess.py")),
                );
                push_likely_file(contract, PathBuf::from("chess.py"));
                push_verify_step(
                    contract,
                    "python syntax check",
                    vec![
                        "python".into(),
                        "-m".into(),
                        "py_compile".into(),
                        "chess.py".into(),
                    ],
                );
                push_run_command(
                    contract,
                    "Run terminal chess",
                    vec!["python".into(), "chess.py".into()],
                );
                for subtask in &mut plan.subtasks {
                    if subtask_matches_goal(subtask, goal_text) {
                        subtask.description = format!(
                            "{}\n\nDefault empty-workspace target: create a self-contained Python terminal chess game in chess.py. Include an 8x8 board, named pieces, turn handling, legal/valid move checks, reset or new-game behavior, and a clear way to run it with `python chess.py`.",
                            subtask.description
                        );
                    }
                }
            }
        }
    }
}

fn subtask_matches_goal(subtask: &phonton_types::Subtask, goal_text: &str) -> bool {
    subtask
        .description
        .trim()
        .eq_ignore_ascii_case(goal_text.trim())
        || subtask
            .description
            .contains(&format!("for `{}`", goal_text.trim()))
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

fn push_expected_artifact(
    contract: &mut phonton_types::GoalContract,
    description: &str,
    path: Option<PathBuf>,
) {
    if contract
        .expected_artifacts
        .iter()
        .any(|artifact| artifact.path == path)
    {
        return;
    }
    contract.expected_artifacts.push(ExpectedArtifact {
        description: description.into(),
        path,
    });
}

fn push_likely_file(contract: &mut phonton_types::GoalContract, path: PathBuf) {
    if contract
        .likely_files
        .iter()
        .any(|existing| existing == &path)
    {
        return;
    }
    contract.likely_files.push(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_planner::Goal;

    fn plan_for(goal: &str) -> PlannerOutput {
        phonton_planner::decompose(&Goal::new(goal))
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
    fn empty_chess_workspace_defaults_to_terminal_python_game() {
        let temp = tempfile::tempdir().unwrap();
        let mut plan = plan_for("make chess");

        apply_workspace_preflight(&mut plan, temp.path(), "make chess");

        let contract = plan.goal_contract.as_ref().unwrap();
        assert!(contract
            .assumptions
            .iter()
            .any(|assumption| assumption.contains("No package.json")));
        assert!(contract.clarification_questions.is_empty());
        assert!(contract
            .likely_files
            .iter()
            .any(|path| path == &PathBuf::from("chess.py")));
        assert!(contract.verify_plan.iter().any(|step| {
            step.command.as_ref().is_some_and(|cmd| {
                cmd.command
                    == vec![
                        "python".to_string(),
                        "-m".to_string(),
                        "py_compile".to_string(),
                        "chess.py".to_string(),
                    ]
            })
        }));
        assert!(contract
            .run_plan
            .iter()
            .any(|cmd| cmd.command == vec!["python".to_string(), "chess.py".to_string()]));
        assert!(plan
            .subtasks
            .iter()
            .any(|subtask| subtask.description.contains("python chess.py")));
    }

    #[test]
    fn empty_chess_html_goal_defaults_to_static_index_html() {
        let temp = tempfile::tempdir().unwrap();
        let mut plan = plan_for("make chess in html");

        apply_workspace_preflight(&mut plan, temp.path(), "make chess in html");

        let contract = plan.goal_contract.as_ref().unwrap();
        assert!(contract
            .likely_files
            .iter()
            .any(|path| path == &PathBuf::from("index.html")));
        assert!(!contract
            .likely_files
            .iter()
            .any(|path| path == &PathBuf::from("chess.py")));
        assert!(contract.run_plan.iter().any(|cmd| cmd.command
            == vec![
                "python".to_string(),
                "-m".to_string(),
                "http.server".to_string(),
                "8000".to_string()
            ]));
        assert!(plan
            .subtasks
            .iter()
            .any(|subtask| subtask.description.contains("index.html")));
    }
}
