//! Deterministic first-run demos for Phonton.
//!
//! These demos intentionally avoid provider calls. They are meant to prove the
//! ADE loop shape quickly: contract, plan, verification, receipt, memory.

use anyhow::Result;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DemoOptions {
    pub json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemoRequest {
    pub name: String,
    pub options: DemoOptions,
}

#[derive(Debug, Clone, Serialize)]
struct TrustDemoReport {
    title: &'static str,
    promise: &'static str,
    fixture: DemoFixture,
    goal_contract: DemoGoalContract,
    plan_preview: Vec<&'static str>,
    verification_story: Vec<DemoVerificationStep>,
    review_receipt: DemoReceipt,
    memory_prompt: &'static str,
    next_commands: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
struct DemoFixture {
    workspace: String,
    files: Vec<&'static str>,
    goal: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct DemoGoalContract {
    acceptance: Vec<&'static str>,
    likely_files: Vec<&'static str>,
    assumptions: Vec<&'static str>,
    verify_plan: Vec<&'static str>,
    run_plan: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
struct DemoVerificationStep {
    status: &'static str,
    detail: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct DemoReceipt {
    changed_files: Vec<&'static str>,
    checks: Vec<&'static str>,
    known_gaps: Vec<&'static str>,
    rollback: &'static str,
    tokens: &'static str,
}

pub fn parse_request(args: &[String]) -> Result<DemoRequest> {
    let mut options = DemoOptions::default();
    let mut positionals = Vec::new();

    for arg in args {
        match arg.as_str() {
            "--json" => options.json = true,
            "-h" | "--help" => {
                return Err(anyhow::anyhow!("usage: phonton demo trust-loop [--json]"));
            }
            other if other.starts_with('-') => {
                return Err(anyhow::anyhow!("unknown demo option `{other}`"));
            }
            other => positionals.push(other.to_string()),
        }
    }

    if positionals.len() != 1 {
        return Err(anyhow::anyhow!(
            "`phonton demo` requires exactly one demo name, e.g. `phonton demo trust-loop`"
        ));
    }

    Ok(DemoRequest {
        name: positionals.remove(0),
        options,
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
            eprintln!("phonton demo: {msg}");
            eprintln!("Run `phonton demo --help` for usage.");
            return Ok(2);
        }
    };

    if request.name != "trust-loop" {
        eprintln!("phonton demo: unknown demo `{}`", request.name);
        eprintln!("available demos: trust-loop");
        return Ok(2);
    }

    let fixture_path = prepare_trust_demo_fixture()?;
    let report = trust_demo_report(Some(&fixture_path));
    if request.options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", render_trust_demo_with_fixture(&fixture_path));
    }
    Ok(0)
}

pub fn render_trust_demo() -> String {
    render_trust_demo_report(&trust_demo_report(None))
}

fn render_trust_demo_with_fixture(fixture_path: &Path) -> String {
    render_trust_demo_report(&trust_demo_report(Some(fixture_path)))
}

fn render_trust_demo_report(report: &TrustDemoReport) -> String {
    let mut text = String::new();
    text.push_str(report.title);
    text.push_str("\n\nPromise\n");
    text.push_str(report.promise);
    text.push_str("\n\nFixture\n");
    text.push_str(&format!(
        "workspace: {}\ngoal: {}\nfiles: {}\n",
        report.fixture.workspace,
        report.fixture.goal,
        report.fixture.files.join(", ")
    ));

    text.push_str("\n1. GoalContract\n");
    append_lines(&mut text, "acceptance", &report.goal_contract.acceptance);
    append_lines(
        &mut text,
        "likely files",
        &report.goal_contract.likely_files,
    );
    append_lines(&mut text, "assumptions", &report.goal_contract.assumptions);
    append_lines(&mut text, "verify plan", &report.goal_contract.verify_plan);
    append_lines(&mut text, "run plan", &report.goal_contract.run_plan);

    text.push_str("\n2. Plan Preview\n");
    for item in &report.plan_preview {
        text.push_str(&format!("- {item}\n"));
    }

    text.push_str("\n3. Verification Caught A Weak Attempt\n");
    for step in &report.verification_story {
        text.push_str(&format!("- {}: {}\n", step.status, step.detail));
    }

    text.push_str("\n4. Review Receipt\n");
    append_lines(
        &mut text,
        "changed files",
        &report.review_receipt.changed_files,
    );
    append_lines(&mut text, "checks", &report.review_receipt.checks);
    append_lines(&mut text, "known gaps", &report.review_receipt.known_gaps);
    text.push_str(&format!("- rollback: {}\n", report.review_receipt.rollback));
    text.push_str(&format!("- tokens: {}\n", report.review_receipt.tokens));

    text.push_str("\n5. Memory Prompt\n");
    text.push_str(report.memory_prompt);
    text.push_str("\n\nNext commands\n");
    for command in &report.next_commands {
        text.push_str(command);
        text.push('\n');
    }
    text
}

fn append_lines(text: &mut String, label: &str, lines: &[&str]) {
    for line in lines {
        text.push_str(&format!("- {label}: {line}\n"));
    }
}

fn prepare_trust_demo_fixture() -> Result<std::path::PathBuf> {
    let root = std::env::temp_dir().join("phonton-trust-loop-fixture");
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::create_dir_all(root.join("tests"))?;
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"phonton-trust-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )?;
    std::fs::write(
        root.join("src").join("config.rs"),
        "pub fn validate_provider(name: &str) -> Result<(), &'static str> {\n    if name.trim().is_empty() {\n        return Err(\"provider name cannot be empty\");\n    }\n    Ok(())\n}\n",
    )?;
    std::fs::write(root.join("src").join("lib.rs"), "pub mod config;\n")?;
    std::fs::write(
        root.join("tests").join("config_validation.rs"),
        "use phonton_trust_fixture::config::validate_provider;\n\n#[test]\nfn rejects_empty_provider_name() {\n    assert!(validate_provider(\"\").is_err());\n}\n",
    )?;
    Ok(root)
}

fn trust_demo_report(fixture_path: Option<&Path>) -> TrustDemoReport {
    TrustDemoReport {
        title: "Phonton Trust Demo Loop",
        promise: "Give Phonton a goal. It shows the contract, makes the change, verifies it, hands you a receipt, and remembers what mattered.",
        fixture: DemoFixture {
            workspace: fixture_path
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "tiny config loader fixture".into()),
            files: vec![
                "Cargo.toml",
                "src/lib.rs",
                "src/config.rs",
                "tests/config_validation.rs",
            ],
            goal: "add validation to config loading",
        },
        goal_contract: DemoGoalContract {
            acceptance: vec![
                "reject empty provider names",
                "keep valid config behavior unchanged",
                "surface a specific validation error",
            ],
            likely_files: vec!["src/config.rs", "tests/config_validation.rs"],
            assumptions: vec!["existing TOML parsing remains the source of truth"],
            verify_plan: vec!["cargo test config_validation"],
            run_plan: vec!["cargo test config_validation"],
        },
        plan_preview: vec![
            "edit the validation boundary",
            "add focused regression tests",
            "run verifier before review",
        ],
        verification_story: vec![
            DemoVerificationStep {
                status: "fail",
                detail: "first edit only checked for a missing file",
            },
            DemoVerificationStep {
                status: "caught",
                detail: "empty provider name still passed",
            },
            DemoVerificationStep {
                status: "retry",
                detail: "final diff adds real validation and a regression test",
            },
            DemoVerificationStep {
                status: "pass",
                detail: "cargo test config_validation passed",
            },
        ],
        review_receipt: DemoReceipt {
            changed_files: vec!["src/config.rs", "tests/config_validation.rs"],
            checks: vec!["cargo test config_validation"],
            known_gaps: vec!["no provider network check needed for this local validation task"],
            rollback: "git checkpoint before applying verified diff",
            tokens: "provider-reported tokens only; no benchmark claim made",
        },
        memory_prompt: "Remember: config loading treats empty provider names as invalid input.",
        next_commands: vec![
            "phonton init",
            "phonton doctor",
            "phonton plan \"add validation to config loading\"",
            "phonton review latest --markdown",
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_accepts_trust_loop_json() {
        let request = parse_request(&["trust-loop".into(), "--json".into()]).unwrap();

        assert_eq!(request.name, "trust-loop");
        assert!(request.options.json);
    }

    #[test]
    fn trust_demo_text_contains_contract_receipt_and_memory() {
        let text = render_trust_demo();

        assert!(text.contains("GoalContract"));
        assert!(text.contains("Verification Caught A Weak Attempt"));
        assert!(text.contains("Review Receipt"));
        assert!(text.contains("Memory Prompt"));
    }

    #[test]
    fn trust_demo_json_has_fixture_and_receipt() {
        let json = serde_json::to_value(trust_demo_report(None)).unwrap();

        assert_eq!(json["fixture"]["goal"], "add validation to config loading");
        assert!(json["review_receipt"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value.as_str() == Some("cargo test config_validation")));
    }
}
