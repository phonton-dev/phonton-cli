use anyhow::{anyhow, Result};
use phonton_store::TaskRecord;
use phonton_types::{BenchmarkFinalStatus, ProofBundleExport, TaskStatus};

use crate::open_persistent_store;

pub async fn run(args: &[String]) -> Result<i32> {
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print_help();
        return Ok(0);
    }
    if args.first().map(String::as_str) != Some("export") {
        eprintln!("phonton proof: unknown command `{}`", args[0]);
        print_help();
        return Ok(2);
    }

    let mut latest = false;
    let mut format = "json";
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--latest" => latest = true,
            "--format" => {
                i += 1;
                format = args
                    .get(i)
                    .map(String::as_str)
                    .ok_or_else(|| anyhow!("--format requires a value"))?;
            }
            other => return Err(anyhow!("unexpected proof export argument `{other}`")),
        }
        i += 1;
    }
    if !latest {
        return Err(anyhow!(
            "proof export currently requires --latest so it cannot accidentally export the wrong run"
        ));
    }
    if format != "json" {
        return Err(anyhow!("unsupported proof export format `{format}`"));
    }

    let store = open_persistent_store()?;
    let task = store
        .list_tasks(50)
        .await?
        .into_iter()
        .find(|task| task.outcome_ledger.is_some())
        .ok_or_else(|| anyhow!("no task with an outcome ledger found"))?;
    let export = build_export(&task)?;
    println!("{}", serde_json::to_string_pretty(&export)?);
    Ok(0)
}

fn print_help() {
    println!(
        "Usage:\n  phonton proof export --latest --format json\n\nExports the latest proof bundle from OutcomeLedger."
    );
}

fn build_export(task: &TaskRecord) -> Result<ProofBundleExport> {
    let ledger = task
        .outcome_ledger
        .as_ref()
        .ok_or_else(|| anyhow!("task has no OutcomeLedger"))?;
    let handoff = ledger
        .handoff
        .clone()
        .ok_or_else(|| anyhow!("OutcomeLedger has no HandoffPacket"))?;
    let goal = ledger
        .goal_contract
        .as_ref()
        .map(|contract| contract.goal.clone())
        .unwrap_or_else(|| task.goal_text.clone());

    Ok(ProofBundleExport {
        task_id: ledger.task_id,
        goal,
        goal_contract: ledger.goal_contract.clone(),
        context_manifest: ledger.context_manifest.clone(),
        permission_ledger: ledger.permission_ledger.clone(),
        verify_report: ledger.verify_report.clone(),
        handoff_packet: handoff,
        summaries: if ledger.summaries == phonton_types::OutcomeSummaries::default() {
            phonton_types::OutcomeSummaries::from_evidence(
                ledger.goal_contract.as_ref(),
                &ledger.context_manifest,
                &ledger.permission_ledger,
                &ledger.verify_report,
                ledger.handoff.as_ref(),
            )
        } else {
            ledger.summaries.clone()
        },
        final_status: proof_final_status(task, &ledger.verify_report),
    })
}

fn proof_final_status(
    task: &TaskRecord,
    report: &phonton_types::VerifyReport,
) -> BenchmarkFinalStatus {
    if serde_json::from_value::<TaskStatus>(task.status.clone())
        .is_ok_and(|status| matches!(status, TaskStatus::Failed { .. } | TaskStatus::Rejected))
    {
        return BenchmarkFinalStatus::Failed;
    }
    if !report.passed.is_empty() && report.findings.is_empty() {
        BenchmarkFinalStatus::VerifiedSuccess
    } else if !report.passed.is_empty() {
        BenchmarkFinalStatus::Partial
    } else {
        BenchmarkFinalStatus::Unverified
    }
}

#[cfg(test)]
mod tests {
    use phonton_store::TaskRecord;
    use phonton_types::{
        ContextManifest, DiffStats, HandoffPacket, OutcomeLedger, PermissionLedger, TaskId,
        TaskStatus, TokenUsage, VerifyReport,
    };

    use super::*;

    fn task_with_ledger(report: VerifyReport) -> TaskRecord {
        let task_id = TaskId::new();
        let handoff = HandoffPacket {
            task_id,
            goal: "fix config panic".into(),
            headline: "verified".into(),
            changed_files: Vec::new(),
            generated_artifacts: Vec::new(),
            diff_stats: DiffStats::default(),
            verification: report.clone(),
            run_commands: Vec::new(),
            known_gaps: Vec::new(),
            review_actions: Vec::new(),
            rollback_points: Vec::new(),
            token_usage: TokenUsage::estimated(42),
            influence: Default::default(),
        };
        TaskRecord {
            id: task_id,
            goal_text: "fix config panic".into(),
            status: serde_json::to_value(TaskStatus::Reviewing {
                tokens_used: 42,
                estimated_savings_tokens: 0,
            })
            .unwrap(),
            created_at: 1,
            total_tokens: 42,
            outcome_ledger: Some(OutcomeLedger {
                task_id,
                goal_contract: None,
                context_manifest: ContextManifest::default(),
                permission_ledger: PermissionLedger::default(),
                verify_report: report,
                summaries: phonton_types::OutcomeSummaries::default(),
                handoff: Some(handoff),
            }),
        }
    }

    #[test]
    fn proof_export_includes_handoff_even_with_estimated_tokens() {
        let task = task_with_ledger(VerifyReport {
            passed: vec!["cargo test".into()],
            findings: Vec::new(),
            skipped: Vec::new(),
        });

        let export = build_export(&task).unwrap();

        assert_eq!(export.goal, "fix config panic");
        assert_eq!(export.handoff_packet.headline, "verified");
        assert_eq!(export.final_status, BenchmarkFinalStatus::VerifiedSuccess);
    }

    #[test]
    fn proof_export_includes_deterministic_summary_bundle() {
        let task = task_with_ledger(VerifyReport {
            passed: vec!["cargo test".into()],
            findings: Vec::new(),
            skipped: Vec::new(),
        });

        let export = build_export(&task).unwrap();

        assert_eq!(export.summaries.verification.passed, 1);
        assert_eq!(
            export.summaries.handoff.as_ref().unwrap().headline,
            "verified"
        );
        assert_eq!(export.summaries.token.total_tokens, 42);
    }
}
