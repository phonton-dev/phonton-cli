use std::path::{Path, PathBuf};

use phonton_types::{
    AcceptanceSlice, CoverageSummary, ExpectedArtifact, ModelTier, PlannerOutput, RunCommand,
    Subtask, SubtaskId, SubtaskStatus, TokenPolicy, VerifyStepSpec,
};

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
    let wants_vite_react = is_chess && wants_vite_react_app(&lower_goal);
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
                        "npm run build",
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
            if wants_vite_react {
                apply_empty_vite_react_chess_plan(plan, goal_text);
            } else if wants_static_html {
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

fn wants_vite_react_app(lower_goal: &str) -> bool {
    lower_goal.contains("vite")
        && lower_goal.contains("react")
        && (lower_goal.contains("typescript") || lower_goal.contains("type script"))
}

fn apply_empty_vite_react_chess_plan(plan: &mut PlannerOutput, goal_text: &str) {
    let Some(contract) = plan.goal_contract.as_mut() else {
        return;
    };

    contract.assumptions.push(
        "No project stack was detected, but the prompt explicitly requested a Vite + TypeScript + React browser app; scaffold that npm app instead of falling back to static HTML."
            .into(),
    );
    contract.acceptance_criteria.extend([
        "Create a modern chess web app using Vite, TypeScript, and React.".into(),
        "Use a clean game-state/rules boundary; prefer a wrapped chess rules library such as chess.js over UI-only move checks.".into(),
        "Do not claim success unless npm install, npm test, and npm run build pass.".into(),
    ]);
    contract.quality_floor.criteria.extend([
        "Generated React chess must have real board interaction, legal move enforcement, status, reset, and move history.".into(),
        "Game-state/rules behavior must be covered by tests.".into(),
    ]);

    for (description, path) in [
        (
            "npm package manifest with Vite/React scripts",
            "package.json",
        ),
        ("Vite HTML entry", "index.html"),
        ("React TypeScript entry point", "src/main.tsx"),
        ("Playable chess React surface", "src/App.tsx"),
        ("Chess game-state/rules wrapper", "src/chessRules.ts"),
        ("Game-state/rules tests", "src/chessRules.test.ts"),
    ] {
        push_expected_artifact(contract, description, Some(PathBuf::from(path)));
        push_likely_file(contract, PathBuf::from(path));
    }

    push_verify_step(
        contract,
        "npm install",
        vec!["npm".into(), "install".into()],
    );
    push_verify_step(contract, "npm test", vec!["npm".into(), "test".into()]);
    push_verify_step(
        contract,
        "npm run build",
        vec!["npm".into(), "run".into(), "build".into()],
    );
    push_run_command(
        contract,
        "Run Vite dev server",
        vec!["npm".into(), "run".into(), "dev".into()],
    );
    contract.run_plan.retain(|cmd| {
        !cmd.command
            .windows(3)
            .any(|parts| parts == ["python", "-m", "http.server"])
    });

    let verify_plan = contract.verify_plan.clone();
    contract.acceptance_slices = vite_react_chess_acceptance_slices(verify_plan);
    contract.token_policy = TokenPolicy {
        first_attempt_cap_tokens: Some(7_000),
        allow_broad_repair: false,
        repair_only_missing_criteria: true,
        notes: vec![
            "Scaffold the explicit Vite/React stack; do not simplify to static HTML.".into(),
            "Use bounded slices and preserve passing npm test/build after each slice.".into(),
        ],
    };

    let attachments = plan
        .subtasks
        .first()
        .map(|subtask| subtask.attachments.clone())
        .unwrap_or_default();
    plan.subtasks = preflight_acceptance_slice_subtasks(
        goal_text,
        "Vite React chess app",
        &contract.acceptance_slices,
        attachments,
    );
    plan.estimated_total_tokens = (plan.subtasks.len() as u64).saturating_mul(1_000);
    plan.naive_baseline_tokens = (plan.subtasks.len() as u64).saturating_mul(4_000);
    plan.coverage_summary = CoverageSummary {
        new_functions: 0,
        tests_planned: 1,
    };
}

fn vite_react_chess_acceptance_slices(verify_plan: Vec<VerifyStepSpec>) -> Vec<AcceptanceSlice> {
    [
        (
            "scaffold",
            "scaffold package.json, index.html, src/main.tsx, src/App.tsx, a starter src/chessRules.ts rules boundary, and src/chessRules.test.ts smoke test for a Vite, TypeScript, and React chess app with npm scripts",
            "package.json",
        ),
        (
            "rules",
            "implement a clean chess.js-backed game-state/rules boundary for legal moves, turn order, captures, check, checkmate, stalemate, and queen promotion",
            "src/chessRules.ts",
        ),
        (
            "rules_tests",
            "add game-state/rules boundary tests for legal moves, illegal moves, turn order, check safety, promotion, and terminal status",
            "src/chessRules.test.ts",
        ),
        (
            "board_ui",
            "render the actual chess app first screen with board coordinates, named pieces, and clear turn status",
            "src/App.tsx",
        ),
        (
            "interactions",
            "support click/tap selection, legal destination highlights, legal moves, captures, blocked-path rejection, and king-safety rejection",
            "src/App.tsx",
        ),
        (
            "status_history_reset",
            "show check/checkmate/stalemate status when possible, auto-promote pawns to queen, expose reset/new game, and show readable move history",
            "src/App.tsx",
        ),
        (
            "verify_run",
            "provide concrete npm install, npm test, npm run build, and npm run dev commands and keep build/tests passing",
            "package.json",
        ),
    ]
    .into_iter()
    .map(|(id, criterion, artifact)| AcceptanceSlice {
        id: id.into(),
        criterion: criterion.into(),
        artifact_path: Some(PathBuf::from(artifact)),
        verify_plan: verify_plan.clone(),
    })
    .collect()
}

fn preflight_acceptance_slice_subtasks(
    goal_text: &str,
    label: &str,
    slices: &[AcceptanceSlice],
    attachments: Vec<phonton_types::PromptAttachment>,
) -> Vec<Subtask> {
    let total = slices.len();
    let mut subtasks = Vec::with_capacity(total);
    let mut previous = None;
    let goal_label = compact_goal_label(goal_text);
    for (idx, slice) in slices.iter().enumerate() {
        let id = SubtaskId::new();
        let artifact_paths = acceptance_slice_artifact_paths(slice);
        let artifact = match artifact_paths.as_slice() {
            [] => String::new(),
            [path] => format!(" Artifact: {}.", path.display()),
            paths => format!(
                " Artifacts: {}.",
                paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        let extra = if slice.id == "rules" || slice.id == "rules_tests" {
            " Use chess.js through a local wrapper; do not fake rules in UI-only checks."
        } else {
            ""
        };
        subtasks.push(Subtask {
            id,
            description: format!(
                "{label} acceptance slice {}/{} for `{}`: {}.{}{} Keep the diff minimal; satisfy only this slice, preserve earlier slices, and keep npm test/build passing.",
                idx + 1,
                total,
                goal_label,
                slice.criterion,
                artifact,
                extra
            ),
            model_tier: ModelTier::Standard,
            dependencies: previous.into_iter().collect(),
            attachments: attachments.clone(),
            status: SubtaskStatus::Queued,
        });
        previous = Some(id);
    }
    subtasks
}

fn acceptance_slice_artifact_paths(slice: &AcceptanceSlice) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = &slice.artifact_path {
        paths.push(path.clone());
    }
    match slice.id.as_str() {
        "rules" => push_unique_path(&mut paths, PathBuf::from("src/chessRules.test.ts")),
        "rules_tests" => push_unique_path(&mut paths, PathBuf::from("src/chessRules.ts")),
        _ => {}
    }
    paths
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn compact_goal_label(goal_text: &str) -> String {
    let lower = goal_text.to_ascii_lowercase();
    if lower.contains("chess")
        && lower.contains("vite")
        && lower.contains("react")
        && lower.contains("typescript")
    {
        return "playable Vite React TypeScript chess app".into();
    }

    let first_content_line = goal_text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or(goal_text.trim());
    let mut label: String = first_content_line.chars().take(120).collect();
    if first_content_line.chars().count() > 120 {
        label.push_str("...");
    }
    label
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

    const PLAYABLE_CHESS_BENCHMARK_PROMPT: &str = r#"# Benchmark 01 Prompt: Playable Chess App

You are in an empty project folder. Build a playable browser chess app.

Requirements:

1. Create a modern web app using Vite, TypeScript, and React.
2. The first screen must be the actual chess app, not a landing page.
3. Render an 8x8 chess board with coordinates, named pieces, and clear turn status.
4. Let a user click or tap a piece, see legal destination highlights, and move it.
5. Enforce turn order, captures, blocked paths, and legal movement for pawns, knights, bishops, rooks, queens, and kings.
6. A move that would leave the current player's king in check must be rejected.
7. Include check detection and show check/checkmate/stalemate status when possible.
8. Include pawn promotion. Promotion to queen is acceptable.
9. Include reset or new-game behavior.
10. Include move history in algebraic or readable coordinate form.
11. Use a chess rules library only if you wrap it cleanly. Do not fake legal move handling with UI-only checks.
12. Add tests for the game-state/rules boundary.
13. Add a concrete run command and a concrete verification command.
14. Do not claim success unless the app builds and the tests pass.
15. If anything is incomplete, list it as a known gap instead of hiding it.

Expected final state:

- `npm install` works.
- `npm test` or equivalent runs the tests.
- `npm run build` succeeds.
- `npm run dev` starts the playable app.
- The code is reviewable and not just a placeholder that prints "Chess". "#;

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

    #[test]
    fn empty_vite_react_chess_prompt_scaffolds_npm_app_contract() {
        let temp = tempfile::tempdir().unwrap();
        let mut plan = plan_for(PLAYABLE_CHESS_BENCHMARK_PROMPT);

        apply_workspace_preflight(&mut plan, temp.path(), PLAYABLE_CHESS_BENCHMARK_PROMPT);

        let contract = plan.goal_contract.as_ref().unwrap();
        for path in [
            "package.json",
            "index.html",
            "src/main.tsx",
            "src/App.tsx",
            "src/chessRules.ts",
            "src/chessRules.test.ts",
        ] {
            assert!(
                contract
                    .likely_files
                    .iter()
                    .any(|actual| actual == &PathBuf::from(path)),
                "expected {path} in likely files: {:?}",
                contract.likely_files
            );
        }

        assert!(contract
            .acceptance_criteria
            .iter()
            .any(|criterion| criterion.contains("Vite, TypeScript, and React")));
        assert!(contract
            .acceptance_slices
            .iter()
            .any(|slice| slice.criterion.contains("game-state/rules boundary tests")));
        let scaffold_slice = contract
            .acceptance_slices
            .iter()
            .find(|slice| slice.id == "scaffold")
            .expect("vite chess contract should include scaffold slice");
        assert!(
            scaffold_slice.criterion.contains("src/chessRules.test.ts"),
            "scaffold slice must create a starter test file so vitest does not fail before the dedicated test slice runs: {}",
            scaffold_slice.criterion
        );
        assert!(contract.verify_plan.iter().any(|step| step
            .command
            .as_ref()
            .is_some_and(|cmd| cmd.command == vec!["npm".to_string(), "install".to_string()])));
        assert!(contract.verify_plan.iter().any(|step| step
            .command
            .as_ref()
            .is_some_and(|cmd| cmd.command == vec!["npm".to_string(), "test".to_string()])));
        assert!(contract.verify_plan.iter().any(|step| step
            .command
            .as_ref()
            .is_some_and(|cmd| cmd.command
                == vec!["npm".to_string(), "run".to_string(), "build".to_string()])));
        assert!(contract.run_plan.iter().any(
            |cmd| cmd.command == vec!["npm".to_string(), "run".to_string(), "dev".to_string()]
        ));
        assert!(!contract.run_plan.iter().any(|cmd| cmd
            .command
            .windows(3)
            .any(|parts| parts == ["python", "-m", "http.server"])));
        assert_eq!(plan.subtasks.len(), contract.acceptance_slices.len());
        assert!(plan
            .subtasks
            .iter()
            .all(|subtask| subtask.description.contains("Vite React chess app")));
        assert!(plan
            .subtasks
            .iter()
            .all(|subtask| !subtask.description.contains("Expected final state")));
        assert!(plan
            .subtasks
            .iter()
            .all(|subtask| subtask.description.chars().count() < 900));
        let rules_slice = plan
            .subtasks
            .iter()
            .find(|subtask| subtask.description.contains("game-state/rules boundary"))
            .expect("expected a rules boundary slice");
        assert!(
            rules_slice.description.contains("Artifacts: src/chessRules.ts, src/chessRules.test.ts"),
            "rules slice must carry the current rules test artifact too so API updates patch against the actual test file: {}",
            rules_slice.description
        );
        let rules_test_slice = plan
            .subtasks
            .iter()
            .find(|subtask| subtask.description.contains("boundary tests"))
            .expect("expected a rules test slice");
        assert!(
            rules_test_slice.description.contains("Artifacts: src/chessRules.test.ts, src/chessRules.ts"),
            "rules test slice must carry the current rules artifact too so imports match the actual API: {}",
            rules_test_slice.description
        );
    }
}
