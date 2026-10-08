use std::process::Command;

#[test]
fn packaged_local_plan_starts_without_a_selected_model() {
    let fixture = tempfile::tempdir().unwrap();
    let repository = fixture.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .arg(&repository)
        .status()
        .unwrap()
        .success());
    std::fs::create_dir(repository.join("src")).unwrap();
    std::fs::write(
        repository.join("src/add.py"),
        "def add(a, b):\n    return a + b\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_phonton"))
        .args([
            "goal",
            "--local",
            "--plan",
            "Fix add",
            "--repo",
            repository.to_str().unwrap(),
            "--files",
            "src/add.py",
        ])
        .env("PHONTON_LOCAL_STATE", fixture.path().join("state.json"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "local plan crashed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["request"]["files"][0], "src/add.py");
    assert!(plan["model_selection"].is_null());
}

#[test]
fn local_plan_proposes_pytest_for_a_pytest_repository_without_running_it() {
    let fixture = tempfile::tempdir().unwrap();
    let repository = fixture.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .arg(&repository)
        .status()
        .unwrap()
        .success());
    std::fs::write(repository.join("app.py"), "def value():\n    return 1\n").unwrap();
    std::fs::write(
        repository.join("test_app.py"),
        "class TestValue:\n    def test_value(self):\n        assert False\n",
    )
    .unwrap();
    std::fs::write(repository.join("pytest.ini"), "[pytest]\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_phonton"))
        .args([
            "goal",
            "--local",
            "--plan",
            "Fix value",
            "--repo",
            repository.to_str().unwrap(),
            "--files",
            "app.py",
        ])
        .env("PHONTON_LOCAL_STATE", fixture.path().join("state.json"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["request"]["checks"][0]["program"], "python");
    assert_eq!(
        plan["request"]["checks"][0]["args"],
        serde_json::json!(["-m", "pytest", "--color=no"])
    );
    assert_eq!(
        plan["request"]["checks"][1]["args"],
        serde_json::json!([
            "-m",
            "pytest",
            "--color=no",
            "-o",
            "addopts=",
            "test_app.py"
        ])
    );
    assert_eq!(plan["request"]["approve_host_execution"], false);
    assert!(plan["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning
            .as_str()
            .unwrap_or("")
            .contains("explicit test paths")));
}

#[test]
fn local_goal_help_exits_cleanly() {
    let output = Command::new(env!("CARGO_BIN_EXE_phonton"))
        .args(["goal", "--local", "--help"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "local help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("phonton goal --local"));
}

#[test]
fn incomplete_local_plan_reports_usage_without_crashing() {
    let output = Command::new(env!("CARGO_BIN_EXE_phonton"))
        .args(["goal", "--local", "--plan"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("Usage: phonton goal --local"), "{error}");
    assert!(!error.contains("overflowed its stack"), "{error}");
}

#[test]
fn saved_local_attempt_can_be_listed_and_shown_without_a_model() {
    let fixture = tempfile::tempdir().unwrap();
    let state_path = fixture.path().join("models.json");
    let runs = fixture.path().join("runs");
    std::fs::create_dir(&runs).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let marker = runs.join(format!("{id}.attempt.json"));
    std::fs::write(
        &marker,
        serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "id": id.clone(),
            "goal": "Review saved arithmetic",
            "model": "fixture-local",
            "state": "started",
            "error": null
        }))
        .unwrap(),
    )
    .unwrap();
    let cli = env!("CARGO_BIN_EXE_phonton");
    let listed = Command::new(cli)
        .args(["goal", "--local", "list"])
        .env("PHONTON_LOCAL_STATE", &state_path)
        .output()
        .unwrap();
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["runs"][0]["id"], id);
    assert_eq!(listed["runs"][0]["goal"], "Review saved arithmetic");

    let shown = Command::new(cli)
        .args(["goal", "--local", "show", &id])
        .env("PHONTON_LOCAL_STATE", &state_path)
        .output()
        .unwrap();
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    let shown: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown["id"], id);
    assert_eq!(shown["evidence"]["id"], id);
    assert_eq!(shown["evidence"]["goal"], "Review saved arithmetic");
    assert_eq!(shown["evidence"]["state"], "started");
    assert_eq!(shown["terminal_recorded"], false);
    assert!(shown["note"]
        .as_str()
        .unwrap()
        .contains("may still be active"));
    assert!(shown["apply_status"].is_null());

    let run_directory = runs.join(&id);
    std::fs::create_dir(&run_directory).unwrap();
    let journal = serde_json::json!({
        "schema": 2,
        "run_id": id.clone(),
        "candidate_number": 1,
        "state": "applied",
        "detail": "fixture journal"
    });
    std::fs::write(
        run_directory.join("apply.json"),
        serde_json::to_vec(&journal).unwrap(),
    )
    .unwrap();
    let with_journal = Command::new(cli)
        .args(["goal", "--local", "show", &id])
        .env("PHONTON_LOCAL_STATE", &state_path)
        .output()
        .unwrap();
    assert!(with_journal.status.success());
    let with_journal: serde_json::Value = serde_json::from_slice(&with_journal.stdout).unwrap();
    assert_eq!(with_journal["apply_status"]["state"], "applied");
    assert!(with_journal["apply_status_error"].is_null());

    let mut forged_journal = journal.clone();
    forged_journal["run_id"] = serde_json::Value::String(uuid::Uuid::new_v4().to_string());
    std::fs::write(
        run_directory.join("apply.json"),
        serde_json::to_vec(&forged_journal).unwrap(),
    )
    .unwrap();
    let with_forged_journal = Command::new(cli)
        .args(["goal", "--local", "show", &id])
        .env("PHONTON_LOCAL_STATE", &state_path)
        .output()
        .unwrap();
    assert!(with_forged_journal.status.success());
    let with_forged_journal: serde_json::Value =
        serde_json::from_slice(&with_forged_journal.stdout).unwrap();
    assert!(with_forged_journal["apply_status"].is_null());
    assert!(with_forged_journal["apply_status_error"]
        .as_str()
        .unwrap()
        .contains("identity"));

    let invalid = Command::new(cli)
        .args(["goal", "--local", "show", "not-a-uuid"])
        .env("PHONTON_LOCAL_STATE", &state_path)
        .output()
        .unwrap();
    assert!(!invalid.status.success());

    let other = uuid::Uuid::new_v4().to_string();
    std::fs::write(
        runs.join(format!("{other}.attempt.json")),
        std::fs::read(marker).unwrap(),
    )
    .unwrap();
    let mismatched = Command::new(cli)
        .args(["goal", "--local", "show", &other])
        .env("PHONTON_LOCAL_STATE", &state_path)
        .output()
        .unwrap();
    assert!(!mismatched.status.success());
    assert!(String::from_utf8_lossy(&mismatched.stderr).contains("identity"));
}
