use anyhow::{anyhow, Result};
use phonton_store::TaskRecord;
use phonton_types::{
    CostSummary, EventRecord, OrchestratorEvent, OutcomeLedger, ProviderKind, TaskStatus,
    TokenUsage,
};
use serde::Serialize;

use crate::{config, open_persistent_store};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BenchmarkFinalStatus {
    VerifiedSuccess,
    Partial,
    Failed,
    Unverified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BenchmarkExecutionMode {
    Provider,
    LocalTemplate,
    Mixed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BenchmarkTokenUsageSource {
    ProviderReported,
    Estimated,
    NoProviderCall,
    Unavailable,
}

#[derive(Debug, Clone, Serialize)]
struct BenchmarkRunExport {
    task_id: String,
    repo_commit: String,
    goal: String,
    task_class: String,
    final_status: BenchmarkFinalStatus,
    provider: String,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    cache_creation_tokens: u64,
    cost_usd: f64,
    token_usage_source: BenchmarkTokenUsageSource,
    execution_mode: BenchmarkExecutionMode,
    provider_call_count: u64,
    token_claim_eligible: bool,
    benchmark_warnings: Vec<String>,
}

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
    let status = task_status(task)?;
    let final_status = classify_final_status(&status, ledger);
    let review_events = review_ready_events(events);
    let usage = choose_usage(handoff.token_usage, &review_events);
    let token_usage_source = token_usage_source(&usage, review_events.is_empty());
    let execution_mode = execution_mode(&usage, &review_events);
    let provider_call_count = provider_call_count(&usage, &review_events);
    let (provider, model) = provider_identity(&review_events);
    let (provider, model) = fill_provider_identity_from_config(provider, model);
    let cost_usd = review_events
        .iter()
        .map(|event| cost_summary_usd(event.cost))
        .sum();
    let token_claim_eligible = matches!(final_status, BenchmarkFinalStatus::VerifiedSuccess)
        && matches!(execution_mode, BenchmarkExecutionMode::Provider)
        && matches!(
            token_usage_source,
            BenchmarkTokenUsageSource::ProviderReported
        )
        && provider_call_count > 0;
    let benchmark_warnings = benchmark_warnings(
        &final_status,
        &token_usage_source,
        &execution_mode,
        token_claim_eligible,
        &provider,
        &model,
    );

    Ok(BenchmarkRunExport {
        task_id: task.id.to_string(),
        repo_commit,
        goal: handoff.goal.clone(),
        task_class: ledger
            .goal_contract
            .as_ref()
            .map(|contract| contract.task_class.to_string())
            .unwrap_or_else(|| "unknown".into()),
        final_status,
        provider,
        model,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_tokens: usage.cached_tokens,
        cache_creation_tokens: usage.cache_creation_tokens,
        cost_usd,
        token_usage_source,
        execution_mode,
        provider_call_count,
        token_claim_eligible,
        benchmark_warnings,
    })
}

fn task_status(task: &TaskRecord) -> Result<TaskStatus> {
    serde_json::from_value(task.status.clone()).map_err(Into::into)
}

#[derive(Debug, Clone, Copy)]
struct ReviewEvent<'a> {
    token_usage: TokenUsage,
    cost: CostSummary,
    provider: ProviderKind,
    model_name: &'a str,
}

fn review_ready_events(events: &[EventRecord]) -> Vec<ReviewEvent<'_>> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            OrchestratorEvent::SubtaskReviewReady {
                token_usage,
                cost,
                provider,
                model_name,
                ..
            } => Some(ReviewEvent {
                token_usage: *token_usage,
                cost: *cost,
                provider: *provider,
                model_name,
            }),
            _ => None,
        })
        .collect()
}

fn choose_usage(handoff_usage: TokenUsage, review_events: &[ReviewEvent<'_>]) -> TokenUsage {
    if handoff_usage.budget_tokens() > 0 || handoff_usage.estimated {
        return handoff_usage;
    }

    let mut usage = TokenUsage::default();
    for event in review_events {
        usage.input_tokens = usage
            .input_tokens
            .saturating_add(event.token_usage.input_tokens);
        usage.output_tokens = usage
            .output_tokens
            .saturating_add(event.token_usage.output_tokens);
        usage.cached_tokens = usage
            .cached_tokens
            .saturating_add(event.token_usage.cached_tokens);
        usage.cache_creation_tokens = usage
            .cache_creation_tokens
            .saturating_add(event.token_usage.cache_creation_tokens);
        usage.estimated |= event.token_usage.estimated;
    }
    usage
}

fn classify_final_status(status: &TaskStatus, ledger: &OutcomeLedger) -> BenchmarkFinalStatus {
    match status {
        TaskStatus::Failed { .. } => BenchmarkFinalStatus::Failed,
        TaskStatus::Reviewing { .. } | TaskStatus::Done { .. } => {
            let handoff = ledger.handoff.as_ref();
            let has_changes = handoff
                .map(|handoff| !handoff.changed_files.is_empty())
                .unwrap_or(false);
            let verify_clean = ledger.verify_report.findings.is_empty()
                && (!ledger.verify_report.passed.is_empty() || has_changes);
            if has_changes && verify_clean {
                BenchmarkFinalStatus::VerifiedSuccess
            } else if has_changes {
                BenchmarkFinalStatus::Partial
            } else {
                BenchmarkFinalStatus::Unverified
            }
        }
        TaskStatus::Rejected | TaskStatus::Paused { .. } => BenchmarkFinalStatus::Partial,
        TaskStatus::Queued | TaskStatus::Planning | TaskStatus::Running { .. } => {
            BenchmarkFinalStatus::Unverified
        }
    }
}

fn token_usage_source(usage: &TokenUsage, no_review_events: bool) -> BenchmarkTokenUsageSource {
    if usage.estimated {
        BenchmarkTokenUsageSource::Estimated
    } else if usage.budget_tokens() > 0 {
        BenchmarkTokenUsageSource::ProviderReported
    } else if no_review_events {
        BenchmarkTokenUsageSource::Unavailable
    } else {
        BenchmarkTokenUsageSource::NoProviderCall
    }
}

fn execution_mode(usage: &TokenUsage, review_events: &[ReviewEvent<'_>]) -> BenchmarkExecutionMode {
    if review_events.is_empty() {
        return if usage.budget_tokens() > 0 && !usage.estimated {
            BenchmarkExecutionMode::Provider
        } else {
            BenchmarkExecutionMode::Unknown
        };
    }

    let provider_events = review_events
        .iter()
        .filter(|event| !is_local_template(event.model_name))
        .count();
    let local_events = review_events.len().saturating_sub(provider_events);
    match (provider_events > 0, local_events > 0) {
        (true, true) => BenchmarkExecutionMode::Mixed,
        (true, false) => BenchmarkExecutionMode::Provider,
        (false, true) => BenchmarkExecutionMode::LocalTemplate,
        (false, false) => BenchmarkExecutionMode::Unknown,
    }
}

fn provider_call_count(usage: &TokenUsage, review_events: &[ReviewEvent<'_>]) -> u64 {
    let review_count = review_events
        .iter()
        .filter(|event| !is_local_template(event.model_name))
        .count() as u64;
    if review_count > 0 {
        review_count
    } else if usage.budget_tokens() > 0 && !usage.estimated {
        1
    } else {
        0
    }
}

fn provider_identity(review_events: &[ReviewEvent<'_>]) -> (String, String) {
    review_events
        .iter()
        .rev()
        .find(|event| !is_local_template(event.model_name))
        .map(|event| (event.provider.to_string(), event.model_name.to_string()))
        .unwrap_or_else(|| (String::new(), String::new()))
}

fn fill_provider_identity_from_config(provider: String, model: String) -> (String, String) {
    if !provider.is_empty() && !model.is_empty() {
        return (provider, model);
    }
    let cfg = config::load().unwrap_or_default();
    let provider = if provider.is_empty() {
        cfg.provider.name
    } else {
        provider
    };
    let model = if model.is_empty() {
        cfg.provider.model.unwrap_or_default()
    } else {
        model
    };
    (provider, model)
}

fn is_local_template(model_name: &str) -> bool {
    let model = model_name.to_ascii_lowercase();
    model.contains("stub") || model.contains("local-template")
}

fn cost_summary_usd(cost: CostSummary) -> f64 {
    cost.total_usd_micros as f64 / 1_000_000.0
}

fn benchmark_warnings(
    final_status: &BenchmarkFinalStatus,
    token_usage_source: &BenchmarkTokenUsageSource,
    execution_mode: &BenchmarkExecutionMode,
    token_claim_eligible: bool,
    provider: &str,
    model: &str,
) -> Vec<String> {
    let mut warnings = Vec::new();
    if !matches!(final_status, BenchmarkFinalStatus::VerifiedSuccess) {
        warnings.push(
            "final status is not verified_success; headline token claims are disabled".into(),
        );
    }
    if matches!(
        token_usage_source,
        BenchmarkTokenUsageSource::Estimated
            | BenchmarkTokenUsageSource::NoProviderCall
            | BenchmarkTokenUsageSource::Unavailable
    ) {
        warnings.push(format!(
            "token usage source `{}` is not eligible for headline token claims",
            serde_json::to_value(token_usage_source)
                .ok()
                .and_then(|v| v.as_str().map(ToString::to_string))
                .unwrap_or_else(|| "unknown".into())
        ));
    }
    if matches!(
        execution_mode,
        BenchmarkExecutionMode::LocalTemplate | BenchmarkExecutionMode::Mixed
    ) {
        warnings.push("execution mode includes local-template output".into());
    }
    if provider.trim().is_empty() || model.trim().is_empty() {
        warnings.push("provider or model identity is missing from benchmark evidence".into());
    }
    if !token_claim_eligible {
        warnings.push("token_claim_eligible is false".into());
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_store::TaskRecord;
    use phonton_types::{
        ContextManifest, DiffStats, HandoffPacket, InfluenceSummary, OutcomeLedger,
        PermissionLedger, VerifyReport,
    };

    fn task(status: TaskStatus, ledger: OutcomeLedger) -> TaskRecord {
        TaskRecord {
            id: phonton_types::TaskId::new(),
            goal_text: "fix config".into(),
            status: serde_json::to_value(status).unwrap(),
            created_at: 1,
            total_tokens: 42,
            outcome_ledger: Some(ledger),
        }
    }

    fn ledger(handoff: HandoffPacket) -> OutcomeLedger {
        OutcomeLedger {
            task_id: handoff.task_id,
            goal_contract: None,
            context_manifest: ContextManifest::default(),
            permission_ledger: PermissionLedger::default(),
            verify_report: handoff.verification.clone(),
            handoff: Some(handoff),
        }
    }

    fn handoff(
        token_usage: TokenUsage,
        changed_files: usize,
        findings: Vec<String>,
    ) -> HandoffPacket {
        let task_id = phonton_types::TaskId::new();
        HandoffPacket {
            task_id,
            goal: "fix config".into(),
            headline: "ready".into(),
            changed_files: (0..changed_files)
                .map(|idx| phonton_types::ChangedFileSummary {
                    path: format!("src/{idx}.js").into(),
                    added_lines: 1,
                    removed_lines: 0,
                    summary: "changed".into(),
                })
                .collect(),
            generated_artifacts: Vec::new(),
            diff_stats: DiffStats {
                files_changed: changed_files as u32,
                added_lines: changed_files as u32,
                removed_lines: 0,
            },
            verification: VerifyReport {
                passed: vec!["npm test passed".into()],
                findings,
                skipped: Vec::new(),
            },
            run_commands: Vec::new(),
            known_gaps: Vec::new(),
            review_actions: Vec::new(),
            rollback_points: Vec::new(),
            token_usage,
            influence: InfluenceSummary::default(),
            screenshot_path: None,
            rendering_summary: None,
        }
    }

    #[test]
    fn failed_export_is_not_claim_eligible_even_with_provider_tokens() {
        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 20,
            cached_tokens: 0,
            cache_creation_tokens: 0,
            estimated: false,
        };
        let handoff = handoff(usage, 0, vec!["worker failed".into()]);
        let task = task(
            TaskStatus::Failed {
                reason: "worker failed".into(),
                failed_subtask: None,
            },
            ledger(handoff),
        );

        let export = build_export(&task, &[], "abc".into()).unwrap();

        assert_eq!(export.final_status, BenchmarkFinalStatus::Failed);
        assert_eq!(
            export.token_usage_source,
            BenchmarkTokenUsageSource::ProviderReported
        );
        assert!(!export.token_claim_eligible);
    }

    #[test]
    fn review_event_sets_provider_identity_and_claim_eligibility() {
        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 20,
            cached_tokens: 0,
            cache_creation_tokens: 2,
            estimated: false,
        };
        let handoff = handoff(usage, 1, Vec::new());
        let task_id = handoff.task_id;
        let task = task(
            TaskStatus::Reviewing {
                tokens_used: 32,
                estimated_savings_tokens: 100,
            },
            ledger(handoff),
        );
        let events = vec![EventRecord {
            task_id,
            timestamp_ms: 1,
            event: OrchestratorEvent::SubtaskReviewReady {
                subtask_id: phonton_types::SubtaskId::new(),
                description: "fix".into(),
                tier: phonton_types::ModelTier::Standard,
                tokens_used: 32,
                token_usage: usage,
                cost: CostSummary {
                    pricing_known: true,
                    input_usd_micros: 1,
                    output_usd_micros: 2,
                    total_usd_micros: 3,
                },
                diff_hunks: Vec::new(),
                verify_result: phonton_types::VerifyResult::Pass {
                    layer: phonton_types::VerifyLayer::Syntax,
                },
                provider: ProviderKind::OpenAiCompatible,
                model_name: "deepseek-chat".into(),
            },
        }];

        let export = build_export(&task, &events, "abc".into()).unwrap();

        assert_eq!(export.final_status, BenchmarkFinalStatus::VerifiedSuccess);
        assert_eq!(export.provider, "openai-compatible");
        assert_eq!(export.model, "deepseek-chat");
        assert_eq!(export.provider_call_count, 1);
        assert!(export.token_claim_eligible);
    }
}
