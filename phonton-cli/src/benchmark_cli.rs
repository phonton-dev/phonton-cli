use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use phonton_store::TaskRecord;
use phonton_types::{
    BenchmarkExecutionMode, BenchmarkFinalStatus, BenchmarkRunExport, BenchmarkTokenUsageSource,
    EventRecord, OrchestratorEvent, TaskStatus, TokenUsage, VerifyReport,
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
    let model_event = latest_benchmark_model_event(events);
    let usage = if handoff.token_usage.budget_tokens() > 0 || handoff.token_usage.estimated {
        handoff.token_usage
    } else {
        model_event
            .as_ref()
            .map(|event| event.token_usage)
            .unwrap_or_default()
    };

    let mut context_buckets = ledger.context_manifest.buckets;
    for record in events {
        if let OrchestratorEvent::PromptManifest { manifest, .. } = &record.event {
            context_buckets.add_prompt_manifest(manifest);
        }
    }
    context_buckets.cached_tokens = context_buckets
        .cached_tokens
        .saturating_add(usage.cached_tokens);
    let mut summary_context = ledger.context_manifest.clone();
    summary_context.buckets = context_buckets;
    let summaries = phonton_types::OutcomeSummaries::from_evidence(
        ledger.goal_contract.as_ref(),
        &summary_context,
        &ledger.permission_ledger,
        &ledger.verify_report,
        Some(handoff),
    );

    let (provider, model, cost_micros) = model_event
        .as_ref()
        .map(|event| {
            (
                event.provider.clone(),
                event.model.clone(),
                event.cost_micros,
            )
        })
        .unwrap_or_else(|| (String::new(), String::new(), 0));
    let provider_call_count = provider_call_count(events, usage, &model);
    let execution_mode = execution_mode(events, &model, provider_call_count);
    let token_usage_source = token_usage_source(usage, execution_mode, provider_call_count);
    let final_status = final_status(task, &ledger.verify_report, usage);
    let token_claim_eligible = matches!(final_status, BenchmarkFinalStatus::VerifiedSuccess)
        && matches!(execution_mode, BenchmarkExecutionMode::Provider)
        && matches!(
            token_usage_source,
            BenchmarkTokenUsageSource::ProviderReported
        )
        && provider_call_count > 0
        && !provider.trim().is_empty()
        && !model.trim().is_empty();
    let benchmark_warnings = benchmark_warnings(
        final_status,
        execution_mode,
        token_usage_source,
        provider_call_count,
        &provider,
        &model,
    );

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
        execution_mode,
        token_usage_source,
        token_claim_eligible,
        provider_call_count,
        benchmark_warnings,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_tokens: usage.cached_tokens,
        cache_creation_tokens: usage.cache_creation_tokens,
        cost_usd: cost_micros as f64 / 1_000_000.0,
        context_buckets,
        summaries,
        verification: verification_map(&ledger.verify_report),
        quality_gates: quality_gate_map(ledger),
        handoff_packet_id: handoff.task_id.to_string(),
        final_status,
    })
}

#[derive(Debug, Clone)]
struct BenchmarkModelEvent {
    provider: String,
    model: String,
    token_usage: TokenUsage,
    cost_micros: u64,
}

fn latest_benchmark_model_event(events: &[EventRecord]) -> Option<BenchmarkModelEvent> {
    events.iter().rev().find_map(|record| match &record.event {
        OrchestratorEvent::SubtaskReviewReady {
            provider,
            model_name,
            token_usage,
            cost,
            ..
        } => Some(BenchmarkModelEvent {
            provider: provider.to_string(),
            model: model_name.clone(),
            token_usage: *token_usage,
            cost_micros: cost.total_usd_micros,
        }),
        OrchestratorEvent::SubtaskFailed {
            provider,
            model_name,
            token_usage,
            ..
        } if provider.is_some()
            || !model_name.trim().is_empty()
            || token_usage.budget_tokens() > 0 =>
        {
            Some(BenchmarkModelEvent {
                provider: provider
                    .as_ref()
                    .map(|provider| provider.to_string())
                    .unwrap_or_default(),
                model: model_name.clone(),
                token_usage: *token_usage,
                cost_micros: 0,
            })
        }
        _ => None,
    })
}

fn provider_call_count(events: &[EventRecord], usage: TokenUsage, model: &str) -> u64 {
    let count = events
        .iter()
        .filter(|record| match &record.event {
            OrchestratorEvent::SubtaskReviewReady {
                model_name,
                token_usage,
                ..
            }
            | OrchestratorEvent::SubtaskFailed {
                model_name,
                token_usage,
                ..
            } => is_provider_call(*token_usage, model_name),
            _ => false,
        })
        .count() as u64;

    if count == 0 && is_provider_call(usage, model) {
        1
    } else {
        count
    }
}

fn execution_mode(
    events: &[EventRecord],
    model: &str,
    provider_call_count: u64,
) -> BenchmarkExecutionMode {
    let saw_local_template = is_local_template(model)
        || events.iter().any(|record| match &record.event {
            OrchestratorEvent::SubtaskReviewReady { model_name, .. }
            | OrchestratorEvent::SubtaskFailed { model_name, .. } => is_local_template(model_name),
            _ => false,
        });

    match (provider_call_count > 0, saw_local_template) {
        (true, true) => BenchmarkExecutionMode::Mixed,
        (true, false) => BenchmarkExecutionMode::Provider,
        (false, true) => BenchmarkExecutionMode::LocalTemplate,
        (false, false) => BenchmarkExecutionMode::Unknown,
    }
}

fn token_usage_source(
    usage: TokenUsage,
    execution_mode: BenchmarkExecutionMode,
    provider_call_count: u64,
) -> BenchmarkTokenUsageSource {
    if usage.estimated {
        return BenchmarkTokenUsageSource::Estimated;
    }
    match execution_mode {
        BenchmarkExecutionMode::LocalTemplate => BenchmarkTokenUsageSource::NoProviderCall,
        BenchmarkExecutionMode::Provider | BenchmarkExecutionMode::Mixed
            if usage.budget_tokens() > 0 && provider_call_count > 0 =>
        {
            BenchmarkTokenUsageSource::ProviderReported
        }
        _ => BenchmarkTokenUsageSource::Unavailable,
    }
}

fn benchmark_warnings(
    final_status: BenchmarkFinalStatus,
    execution_mode: BenchmarkExecutionMode,
    token_usage_source: BenchmarkTokenUsageSource,
    provider_call_count: u64,
    provider: &str,
    model: &str,
) -> Vec<String> {
    let mut warnings: Vec<String> = Vec::new();

    match token_usage_source {
        BenchmarkTokenUsageSource::Estimated => warnings.push(
            "token usage is estimated; exclude this run from public efficiency claims".into(),
        ),
        BenchmarkTokenUsageSource::NoProviderCall => warnings.push(
            "run used local-template/no-provider execution; treat it as product evidence, not provider-token evidence".into(),
        ),
        BenchmarkTokenUsageSource::Unavailable => warnings.push(
            "token source is unavailable; exclude this run from public efficiency claims".into(),
        ),
        BenchmarkTokenUsageSource::ProviderReported => {}
    }

    if matches!(
        execution_mode,
        BenchmarkExecutionMode::LocalTemplate | BenchmarkExecutionMode::Mixed
    ) && !warnings
        .iter()
        .any(|warning| warning.contains("local-template"))
    {
        warnings.push(
            "run includes local-template execution; do not compare it against provider-token runs"
                .into(),
        );
    }

    if matches!(execution_mode, BenchmarkExecutionMode::Mixed)
        && !warnings.iter().any(|warning| warning.contains("mixed"))
    {
        warnings.push(
            "run mixed provider and local-template execution; exclude it from provider-token efficiency claims"
                .into(),
        );
    }

    if provider_call_count == 0
        && matches!(
            token_usage_source,
            BenchmarkTokenUsageSource::ProviderReported
        )
    {
        warnings.push(
            "provider-reported token source has no matching provider-call event evidence".into(),
        );
    }

    if matches!(
        token_usage_source,
        BenchmarkTokenUsageSource::ProviderReported
    ) && (provider.trim().is_empty() || model.trim().is_empty())
    {
        warnings.push("provider or model identity is missing from benchmark evidence".into());
    }

    if !matches!(final_status, BenchmarkFinalStatus::VerifiedSuccess) {
        warnings.push(
            "final status is not verified_success; exclude this run from headline wins".into(),
        );
    }

    warnings
}

fn is_provider_call(usage: TokenUsage, model: &str) -> bool {
    usage.budget_tokens() > 0 && !is_local_template(model)
}

fn is_local_template(model: &str) -> bool {
    model.trim().eq_ignore_ascii_case("local-template")
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
                summaries: phonton_types::OutcomeSummaries::default(),
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
        assert_eq!(export.summaries.context.selected_code_tokens, 900);
        assert_eq!(export.summaries.token.provider_input_tokens, 2000);
        assert_eq!(export.summaries.verification.passed, 1);
        assert_eq!(export.final_status, BenchmarkFinalStatus::VerifiedSuccess);
    }

    #[test]
    fn benchmark_export_marks_estimated_tokens_ineligible() {
        let task = task_with_ledger(TokenUsage {
            estimated: true,
            input_tokens: 100,
            ..Default::default()
        });

        let export = build_export(&task, &[], "abc123".into()).unwrap();

        assert_eq!(
            export.token_usage_source,
            phonton_types::BenchmarkTokenUsageSource::Estimated
        );
        assert!(!export.token_claim_eligible);
        assert!(export
            .benchmark_warnings
            .iter()
            .any(|warning| warning.contains("estimated")));
        assert_eq!(export.final_status, BenchmarkFinalStatus::Unverified);
    }

    #[test]
    fn benchmark_export_marks_local_template_runs_not_token_comparable() {
        let task = task_with_ledger(TokenUsage::default());
        let subtask_id = SubtaskId::new();
        let events = vec![EventRecord {
            task_id: task.id,
            timestamp_ms: 1,
            event: OrchestratorEvent::SubtaskReviewReady {
                subtask_id,
                description: "refactor receipt renderer".into(),
                tier: phonton_types::ModelTier::Standard,
                tokens_used: 0,
                token_usage: TokenUsage::default(),
                cost: CostSummary::default(),
                diff_hunks: Vec::new(),
                verify_result: VerifyResult::Pass {
                    layer: VerifyLayer::Test,
                },
                provider: ProviderKind::DeepSeek,
                model_name: "local-template".into(),
            },
        }];

        let export = build_export(&task, &events, "abc123".into()).unwrap();

        assert_eq!(
            export.execution_mode,
            phonton_types::BenchmarkExecutionMode::LocalTemplate
        );
        assert_eq!(
            export.token_usage_source,
            phonton_types::BenchmarkTokenUsageSource::NoProviderCall
        );
        assert_eq!(export.provider_call_count, 0);
        assert!(!export.token_claim_eligible);
        assert!(export
            .benchmark_warnings
            .iter()
            .any(|warning| warning.contains("local-template")));
    }

    #[test]
    fn benchmark_export_marks_mixed_runs_ineligible_for_token_claims() {
        let task = task_with_ledger(TokenUsage {
            input_tokens: 1000,
            output_tokens: 200,
            ..Default::default()
        });
        let subtask_id = SubtaskId::new();
        let events = vec![
            EventRecord {
                task_id: task.id,
                timestamp_ms: 1,
                event: OrchestratorEvent::SubtaskReviewReady {
                    subtask_id,
                    description: "seed from local template".into(),
                    tier: phonton_types::ModelTier::Cheap,
                    tokens_used: 0,
                    token_usage: TokenUsage::default(),
                    cost: CostSummary::default(),
                    diff_hunks: Vec::new(),
                    verify_result: VerifyResult::Pass {
                        layer: VerifyLayer::Syntax,
                    },
                    provider: ProviderKind::DeepSeek,
                    model_name: "local-template".into(),
                },
            },
            EventRecord {
                task_id: task.id,
                timestamp_ms: 2,
                event: OrchestratorEvent::SubtaskReviewReady {
                    subtask_id,
                    description: "provider repair".into(),
                    tier: phonton_types::ModelTier::Standard,
                    tokens_used: 1200,
                    token_usage: TokenUsage {
                        input_tokens: 1000,
                        output_tokens: 200,
                        ..Default::default()
                    },
                    cost: CostSummary {
                        pricing_known: true,
                        input_usd_micros: 100,
                        output_usd_micros: 200,
                        total_usd_micros: 300,
                    },
                    diff_hunks: Vec::new(),
                    verify_result: VerifyResult::Pass {
                        layer: VerifyLayer::Test,
                    },
                    provider: ProviderKind::DeepSeek,
                    model_name: "deepseek-v4-flash".into(),
                },
            },
        ];

        let export = build_export(&task, &events, "abc123".into()).unwrap();

        assert_eq!(export.execution_mode, BenchmarkExecutionMode::Mixed);
        assert_eq!(
            export.token_usage_source,
            BenchmarkTokenUsageSource::ProviderReported
        );
        assert_eq!(export.final_status, BenchmarkFinalStatus::VerifiedSuccess);
        assert!(!export.token_claim_eligible);
        assert!(export
            .benchmark_warnings
            .iter()
            .any(|warning| warning.contains("mixed") || warning.contains("local-template")));
    }

    #[test]
    fn benchmark_export_uses_failed_event_provider_identity() {
        let mut task = task_with_ledger(TokenUsage {
            input_tokens: 1000,
            output_tokens: 200,
            ..Default::default()
        });
        task.status = serde_json::to_value(TaskStatus::Failed {
            reason: "syntax failure".into(),
            failed_subtask: None,
        })
        .unwrap();
        let subtask_id = SubtaskId::new();
        let events = vec![EventRecord {
            task_id: task.id,
            timestamp_ms: 1,
            event: OrchestratorEvent::SubtaskFailed {
                subtask_id,
                reason: "syntax failure".into(),
                attempt: 4,
                token_usage: TokenUsage {
                    input_tokens: 1000,
                    output_tokens: 200,
                    ..Default::default()
                },
                provider: Some(ProviderKind::DeepSeek),
                model_name: "deepseek-v4-flash".into(),
            },
        }];

        let export = build_export(&task, &events, "abc123".into()).unwrap();

        assert_eq!(export.provider, "deepseek");
        assert_eq!(export.model, "deepseek-v4-flash");
        assert_eq!(export.provider_call_count, 1);
        assert_eq!(
            export.token_usage_source,
            phonton_types::BenchmarkTokenUsageSource::ProviderReported
        );
        assert!(!export.token_claim_eligible);
        assert_eq!(export.final_status, BenchmarkFinalStatus::Failed);
    }
}
