use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use phonton_store::TaskRecord;
use phonton_types::{
    BenchmarkFinalStatus, BenchmarkRunExport, EventRecord, OrchestratorEvent, TaskStatus,
    TokenUsage, VerifyReport,
};

use crate::open_persistent_store;

pub async fn run(args: &[String]) -> Result<i32> {
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print_help();
        return Ok(0);
    }
    if args.first().map(String::as_str) != Some("export") {
        eprintln!("phonton benchmark: unknown command `{}`", args[0]);
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
            other => return Err(anyhow!("unexpected benchmark export argument `{other}`")),
        }
        i += 1;
    }
    if !latest {
        return Err(anyhow!(
            "benchmark export currently requires --latest so it cannot accidentally export the wrong run"
        ));
    }
    if format != "json" {
        return Err(anyhow!("unsupported benchmark export format `{format}`"));
    }

    let store = open_persistent_store()?;
    let task = store
        .list_tasks(50)
        .await?
        .into_iter()
        .find(|task| task.outcome_ledger.is_some())
        .ok_or_else(|| anyhow!("no task with an outcome ledger found"))?;
    let events = store.list_events(task.id, 1_000)?;
    let export = build_export(&task, &events, current_repo_commit())?;
    println!("{}", serde_json::to_string_pretty(&export)?);
    Ok(0)
}

fn print_help() {
    println!(
        "Usage:\n  phonton benchmark export --latest --format json\n\nExports benchmark evidence from the latest real OutcomeLedger run."
    );
}

fn current_repo_commit() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
}

fn build_export(
    task: &TaskRecord,
    events: &[EventRecord],
    repo_commit: String,
) -> Result<BenchmarkRunExport> {
    let ledger = task
        .outcome_ledger
        .as_ref()
        .ok_or_else(|| anyhow!("task has no OutcomeLedger"))?;
    let handoff = ledger
        .handoff
        .as_ref()
        .ok_or_else(|| anyhow!("OutcomeLedger has no HandoffPacket"))?;
    let usage = handoff.token_usage;
    if usage.estimated {
        return Err(anyhow!(
            "benchmark export requires provider-reported tokens; this ledger only has estimated tokens"
        ));
    }

    let mut context_buckets = ledger.context_manifest.buckets;
    for record in events {
        if let OrchestratorEvent::PromptManifest { manifest, .. } = &record.event {
            context_buckets.add_prompt_manifest(manifest);
        }
    }
    context_buckets.cached_tokens = context_buckets
        .cached_tokens
        .saturating_add(usage.cached_tokens);

    let review_event = events.iter().rev().find_map(|record| match &record.event {
        OrchestratorEvent::SubtaskReviewReady {
            provider,
            model_name,
            cost,
            ..
        } => Some((
            provider.to_string(),
            model_name.clone(),
            cost.total_usd_micros,
        )),
        _ => None,
    });
    let (provider, model, cost_micros) =
        review_event.unwrap_or_else(|| (String::new(), String::new(), 0));

    let task_class = ledger
        .goal_contract
        .as_ref()
        .map(|contract| contract.task_class)
        .unwrap_or_else(|| phonton_types::classify_task(&task.goal_text));

    Ok(BenchmarkRunExport {
        task_class,
        goal: ledger
            .goal_contract
            .as_ref()
            .map(|contract| contract.goal.clone())
            .unwrap_or_else(|| task.goal_text.clone()),
        repo_commit,
        provider,
        model,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_tokens: usage.cached_tokens,
        cost_usd: cost_micros as f64 / 1_000_000.0,
        context_buckets,
        verification: verification_map(&ledger.verify_report),
        quality_gates: quality_gate_map(ledger),
        handoff_packet_id: handoff.task_id.to_string(),
        final_status: final_status(task, &ledger.verify_report, usage),
    })
}

fn verification_map(report: &VerifyReport) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if !report.passed.is_empty() {
        map.insert("passed".into(), report.passed.join("; "));
    }
    if !report.findings.is_empty() {
        map.insert("findings".into(), report.findings.join("; "));
    }
    if !report.skipped.is_empty() {
        map.insert("skipped".into(), report.skipped.join("; "));
    }
    map
}

fn quality_gate_map(ledger: &phonton_types::OutcomeLedger) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Some(contract) = &ledger.goal_contract {
        for slice in &contract.acceptance_slices {
            map.insert(slice.id.clone(), slice.criterion.clone());
        }
        if map.is_empty() {
            for (idx, criterion) in contract.quality_floor.criteria.iter().enumerate() {
                map.insert(format!("quality_floor_{}", idx + 1), criterion.clone());
            }
        }
    }
    map
}

fn final_status(
    task: &TaskRecord,
    report: &VerifyReport,
    usage: TokenUsage,
) -> BenchmarkFinalStatus {
    if serde_json::from_value::<TaskStatus>(task.status.clone())
        .is_ok_and(|status| matches!(status, TaskStatus::Failed { .. } | TaskStatus::Rejected))
    {
        return BenchmarkFinalStatus::Failed;
    }
    if usage.estimated {
        return BenchmarkFinalStatus::Unverified;
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
        BenchmarkFinalStatus, ContextManifest, CostSummary, EventRecord, GoalContract,
        HandoffPacket, OrchestratorEvent, OutcomeLedger, PermissionLedger, ProviderKind, SubtaskId,
        TaskClass, TaskId, TaskStatus, TokenUsage, VerifyLayer, VerifyReport, VerifyResult,
    };

    use super::*;

    fn task_with_ledger(token_usage: TokenUsage) -> TaskRecord {
        let task_id = TaskId::new();
        let handoff = HandoffPacket {
            task_id,
            goal: "fix config panic".into(),
            headline: "verified".into(),
            changed_files: Vec::new(),
            generated_artifacts: Vec::new(),
            diff_stats: Default::default(),
            verification: VerifyReport {
                passed: vec!["cargo test".into()],
                findings: Vec::new(),
                skipped: Vec::new(),
            },
            run_commands: Vec::new(),
            known_gaps: Vec::new(),
            review_actions: Vec::new(),
            rollback_points: Vec::new(),
            token_usage,
            influence: Default::default(),
        };
        TaskRecord {
            id: task_id,
            goal_text: "fix config panic".into(),
            status: serde_json::to_value(TaskStatus::Reviewing {
                tokens_used: token_usage.budget_tokens(),
                estimated_savings_tokens: 100,
            })
            .unwrap(),
            created_at: 1,
            total_tokens: token_usage.budget_tokens(),
            outcome_ledger: Some(OutcomeLedger {
                task_id,
                goal_contract: Some(GoalContract {
                    goal: "fix config panic".into(),
                    task_class: TaskClass::BugFix,
                    intent: None,
                    confidence_percent: 80,
                    acceptance_criteria: vec!["panic is fixed".into()],
                    acceptance_slices: Vec::new(),
                    expected_artifacts: Vec::new(),
                    likely_files: Vec::new(),
                    verify_plan: Vec::new(),
                    run_plan: Vec::new(),
                    quality_floor: phonton_types::QualityFloor {
                        criteria: vec!["tests pass".into()],
                    },
                    clarification_questions: Vec::new(),
                    assumptions: Vec::new(),
                    token_policy: Default::default(),
                }),
                context_manifest: ContextManifest::default(),
                permission_ledger: PermissionLedger::default(),
                verify_report: handoff.verification.clone(),
                handoff: Some(handoff),
            }),
        }
    }

    #[test]
    fn benchmark_export_uses_provider_tokens_and_prompt_buckets() {
        let task = task_with_ledger(TokenUsage {
            input_tokens: 2000,
            output_tokens: 500,
            cached_tokens: 300,
            cache_creation_tokens: 0,
            estimated: false,
        });
        let subtask_id = SubtaskId::new();
        let events = vec![
            EventRecord {
                task_id: task.id,
                timestamp_ms: 1,
                event: OrchestratorEvent::PromptManifest {
                    subtask_id,
                    manifest: phonton_types::PromptContextManifest {
                        code_context_tokens: 900,
                        omitted_code_tokens: 4000,
                        memory_tokens: 100,
                        retry_error_tokens: 20,
                        ..Default::default()
                    },
                },
            },
            EventRecord {
                task_id: task.id,
                timestamp_ms: 2,
                event: OrchestratorEvent::SubtaskReviewReady {
                    subtask_id,
                    description: "fix config panic".into(),
                    tier: phonton_types::ModelTier::Standard,
                    tokens_used: 2500,
                    token_usage: task
                        .outcome_ledger
                        .as_ref()
                        .unwrap()
                        .handoff
                        .as_ref()
                        .unwrap()
                        .token_usage,
                    cost: CostSummary {
                        pricing_known: true,
                        input_usd_micros: 1000,
                        output_usd_micros: 2000,
                        total_usd_micros: 3000,
                    },
                    diff_hunks: Vec::new(),
                    verify_result: VerifyResult::Pass {
                        layer: VerifyLayer::Test,
                    },
                    provider: ProviderKind::OpenAI,
                    model_name: "gpt-test".into(),
                },
            },
        ];

        let export = build_export(&task, &events, "abc123".into()).unwrap();

        assert_eq!(export.task_class, TaskClass::BugFix);
        assert_eq!(export.provider, "openai");
        assert_eq!(export.model, "gpt-test");
        assert_eq!(export.input_tokens, 2000);
        assert_eq!(export.output_tokens, 500);
        assert_eq!(export.cached_tokens, 300);
        assert_eq!(export.context_buckets.selected_code_tokens, 900);
        assert_eq!(export.context_buckets.omitted_candidate_tokens, 4000);
        assert_eq!(export.context_buckets.cached_tokens, 300);
        assert_eq!(export.final_status, BenchmarkFinalStatus::VerifiedSuccess);
    }

    #[test]
    fn benchmark_export_rejects_estimated_tokens() {
        let task = task_with_ledger(TokenUsage {
            estimated: true,
            input_tokens: 100,
            ..Default::default()
        });

        let err = build_export(&task, &[], "abc123".into()).unwrap_err();

        assert!(err.to_string().contains("provider-reported tokens"));
    }
}
