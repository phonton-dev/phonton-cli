use std::path::{Path, PathBuf};

use phonton_types::{
    PlannerOutput, PromptAttachment, PromptAttachmentKind, RunCommand, Subtask, VerifyStepSpec,
};

const PREFLIGHT_TEXT_ATTACHMENT_LIMIT: usize = 40_000;
const PREFLIGHT_TEST_ATTACHMENT_LIMIT: usize = 8;

/// Apply local workspace signals to a visible goal contract.
///
/// This is intentionally deterministic and cheap: it inspects only common
/// stack marker files so `phonton plan` and the TUI can show the same run and
/// verification contract before any worker is dispatched.
pub fn apply_workspace_preflight(plan: &mut PlannerOutput, working_dir: &Path) {
    let Some(contract) = plan.goal_contract.as_mut() else {
        return;
    };
    let mut stack_detected = false;

    let package_json = working_dir.join("package.json");
    if package_json.is_file() {
        stack_detected = true;
        attach_preflight_context(
            &mut plan.subtasks,
            collect_node_preflight_attachments(working_dir),
        );
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
    }

    if working_dir.join("Cargo.toml").is_file() {
        stack_detected = true;
        push_verify_step(
            contract,
            "cargo test",
            vec!["cargo".into(), "test".into(), "--locked".into()],
        );
        if cargo_has_default_binary(working_dir) {
            push_run_command(contract, "Run binary", vec!["cargo".into(), "run".into()]);
        }
    }

    if working_dir.join("Makefile").is_file() || working_dir.join("makefile").is_file() {
        stack_detected = true;
        push_verify_step(contract, "make", vec!["make".into()]);
    }

    if !stack_detected {
        contract
            .assumptions
            .push("No package.json, Cargo.toml, or Makefile was detected before planning.".into());
    }
}

/// Suggest plain `cargo run` only for the simple single-binary layout.
/// Other Cargo layouts need an explicit binary name or have no binary.
fn cargo_has_default_binary(root: &Path) -> bool {
    if !root.join("src/main.rs").is_file() {
        return false;
    }
    let Ok(raw_manifest) = std::fs::read_to_string(root.join("Cargo.toml")) else {
        return false;
    };
    let Ok(manifest) = toml::from_str::<toml::Value>(&raw_manifest) else {
        return false;
    };
    let Some(package) = manifest.get("package") else {
        return false;
    };
    let autobins = package.get("autobins").and_then(toml::Value::as_bool);
    let edition_2015 = package
        .get("edition")
        .and_then(toml::Value::as_str)
        .is_none_or(|edition| edition == "2015");
    let has_explicit_target = ["lib", "bin", "example", "test", "bench"]
        .iter()
        .any(|target| manifest.get(*target).is_some());
    if autobins == Some(false)
        || (autobins != Some(true) && edition_2015 && has_explicit_target)
        || manifest
            .get("bin")
            .and_then(toml::Value::as_array)
            .is_some_and(|bins| !bins.is_empty())
    {
        return false;
    }
    match std::fs::read_dir(root.join("src/bin")) {
        Ok(mut entries) => entries.next().is_none(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

fn collect_node_preflight_attachments(working_dir: &Path) -> Vec<PromptAttachment> {
    let mut attachments = Vec::new();
    push_text_attachment(
        &mut attachments,
        working_dir,
        &working_dir.join("package.json"),
    );

    let mut test_files = Vec::new();
    for dir_name in ["test", "tests"] {
        collect_test_files(&working_dir.join(dir_name), &mut test_files);
    }
    test_files.sort();
    for path in test_files.into_iter().take(PREFLIGHT_TEST_ATTACHMENT_LIMIT) {
        push_text_attachment(&mut attachments, working_dir, &path);
    }
    attachments
}

fn collect_test_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_test_files(&path, out);
        } else if file_type.is_file()
            && matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs")
            )
        {
            out.push(path);
        }
    }
}

fn push_text_attachment(attachments: &mut Vec<PromptAttachment>, root: &Path, path: &Path) {
    if attachments
        .iter()
        .any(|attachment| root.join(&attachment.path) == path)
    {
        return;
    }
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    let truncated = bytes.len() > PREFLIGHT_TEXT_ATTACHMENT_LIMIT;
    let text_bytes = if truncated {
        &bytes[..PREFLIGHT_TEXT_ATTACHMENT_LIMIT]
    } else {
        bytes.as_slice()
    };
    let text = String::from_utf8_lossy(text_bytes).into_owned();
    let display_path = path
        .strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf());
    attachments.push(PromptAttachment {
        path: display_path,
        kind: PromptAttachmentKind::Text,
        mime_type: Some("text/plain".into()),
        size_bytes: bytes.len() as u64,
        text: Some(text),
        data_base64: None,
        truncated,
        note: truncated.then(|| "preflight context truncated".into()),
    });
}

fn attach_preflight_context(subtasks: &mut [Subtask], attachments: Vec<PromptAttachment>) {
    if attachments.is_empty() {
        return;
    }
    for subtask in subtasks {
        for attachment in &attachments {
            if !subtask
                .attachments
                .iter()
                .any(|existing| existing.path == attachment.path)
            {
                subtask.attachments.push(attachment.clone());
            }
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
        apply_workspace_preflight(&mut plan, temp.path());

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
    fn npm_workspace_attaches_package_and_tests_as_preflight_context() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            r#"{"type":"module","scripts":{"test":"node --test"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(temp.path().join("test")).unwrap();
        std::fs::write(
            temp.path().join("test").join("receipt.test.js"),
            "import test from 'node:test';\n",
        )
        .unwrap();

        let mut plan = plan_for("refactor `src/receipt.js` and add tests");
        apply_workspace_preflight(&mut plan, temp.path());

        let subtask = plan.subtasks.first().expect("subtask");
        assert!(subtask
            .attachments
            .iter()
            .any(|attachment| attachment.path == Path::new("package.json")));
        assert!(subtask
            .attachments
            .iter()
            .any(|attachment| attachment.path == Path::new("test/receipt.test.js")));
    }

    #[test]
    fn receipt_wording_does_not_replace_a_rust_workspace_plan() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"receipt\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let goal = "refactor the receipt formatter";
        let mut plan = plan_for(goal);
        let original: Vec<_> = plan
            .subtasks
            .iter()
            .map(|task| task.description.clone())
            .collect();

        apply_workspace_preflight(&mut plan, temp.path());

        let updated: Vec<_> = plan
            .subtasks
            .iter()
            .map(|task| task.description.clone())
            .collect();
        assert_eq!(updated, original);
        let contract = plan.goal_contract.as_ref().unwrap();
        assert!(contract.verify_plan.iter().any(|step| step
            .command
            .as_ref()
            .is_some_and(|command| command.command == ["cargo", "test", "--locked"])));
        assert!(!contract
            .acceptance_criteria
            .iter()
            .any(|criterion| criterion.contains("src/receipt.js")));
    }

    #[test]
    fn library_only_cargo_project_does_not_suggest_cargo_run() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"add-one\"\nversion = \"0.1.0\"\n[lib]\npath = \"src/lib.rs\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(
            temp.path().join("src/lib.rs"),
            "pub fn add_one(n: i32) -> i32 { n + 1 }\n",
        )
        .unwrap();
        let mut plan = plan_for("fix add_one");

        apply_workspace_preflight(&mut plan, temp.path());

        let contract = plan.goal_contract.as_ref().unwrap();
        assert!(contract
            .verify_plan
            .iter()
            .any(|step| step.name == "cargo test"));
        assert!(!contract
            .run_plan
            .iter()
            .any(|command| command.command == ["cargo", "run"]));
    }

    #[test]
    fn single_binary_cargo_project_suggests_cargo_run() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"hello\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        let mut plan = plan_for("fix hello");

        apply_workspace_preflight(&mut plan, temp.path());

        let contract = plan.goal_contract.as_ref().unwrap();
        assert!(contract
            .run_plan
            .iter()
            .any(|command| command.command == ["cargo", "run"]));

        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nautobins= false\n",
        )
        .unwrap();
        let mut disabled = plan_for("fix hello");
        apply_workspace_preflight(&mut disabled, temp.path());
        assert!(!disabled
            .goal_contract
            .as_ref()
            .unwrap()
            .run_plan
            .iter()
            .any(|command| command.command == ["cargo", "run"]));

        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nedition = \"2015\"\n[lib]\npath = \"src/lib.rs\"\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("src/lib.rs"), "pub fn library() {}\n").unwrap();
        let mut edition_2015 = plan_for("fix hello");
        apply_workspace_preflight(&mut edition_2015, temp.path());
        assert!(!edition_2015
            .goal_contract
            .as_ref()
            .unwrap()
            .run_plan
            .iter()
            .any(|command| command.command == ["cargo", "run"]));
    }
}
