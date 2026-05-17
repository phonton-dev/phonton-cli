use anyhow::{anyhow, Result};
use phonton_types::{ContextBucketSummary, EventRecord, OrchestratorEvent, TokenUsage};

use crate::open_persistent_store;

pub async fn run(args: &[String]) -> Result<i32> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
    {
        print_help();
        return Ok(0);
    }
    let by_source = args.is_empty() || args.iter().any(|arg| arg == "--by-source");
    if !by_source {
        return Err(anyhow!("usage: phonton why-tokens --by-source"));
    }
    let store = open_persistent_store()?;
    let task = store
        .list_tasks(20)
        .await?
        .into_iter()
        .find(|task| task.outcome_ledger.is_some())
        .ok_or_else(|| anyhow!("no task with token evidence found"))?;
    let events = store.list_events(task.id, 1_000)?;
    let usage = task
        .outcome_ledger
        .as_ref()
        .and_then(|ledger| ledger.handoff.as_ref())
        .map(|handoff| handoff.token_usage)
        .unwrap_or_default();
    let buckets = aggregate_context_buckets(&events, usage);
    let prompt_estimate = aggregate_prompt_estimate(&events);

    print!(
        "{}",
        render_why_tokens_by_source(&task.goal_text, &buckets, usage, prompt_estimate)
    );
    Ok(0)
}

fn print_help() {
    println!(
        "Usage:\n  phonton why-tokens --by-source\n\nShows source-attributed prompt/context token buckets for the latest run."
    );
}

fn aggregate_context_buckets(events: &[EventRecord], usage: TokenUsage) -> ContextBucketSummary {
    let mut buckets = ContextBucketSummary::default();
    for record in events {
        if let OrchestratorEvent::PromptManifest { manifest, .. } = &record.event {
            buckets.add_prompt_manifest(manifest);
        }
    }
    buckets.cached_tokens = buckets.cached_tokens.saturating_add(usage.cached_tokens);
    buckets
}

fn aggregate_prompt_estimate(events: &[EventRecord]) -> u64 {
    events
        .iter()
        .filter_map(|record| match &record.event {
            OrchestratorEvent::PromptManifest { manifest, .. } => {
                Some(manifest.total_estimated_tokens)
            }
            _ => None,
        })
        .sum()
}

fn render_why_tokens_by_source(
    goal_text: &str,
    buckets: &ContextBucketSummary,
    usage: TokenUsage,
    local_prompt_estimate_tokens: u64,
) -> String {
    let mut out = String::new();
    out.push_str("Why tokens by source\n");
    out.push_str(&format!("task: {goal_text}\n"));
    out.push_str(&format!(
        "local_prompt_estimate_tokens: {local_prompt_estimate_tokens}\n"
    ));
    out.push_str("estimate_source: local tokenizer estimate\n");
    out.push_str(&format!(
        "provider_usage_source: {}\n",
        provider_usage_source(usage)
    ));
    out.push_str(&format!(
        "provider_reported_input_tokens: {}\n",
        provider_value(usage, usage.input_tokens)
    ));
    out.push_str(&format!(
        "provider_reported_output_tokens: {}\n",
        provider_value(usage, usage.output_tokens)
    ));
    out.push_str(&format!(
        "provider_cached_tokens: {}\n",
        provider_value(usage, usage.cached_tokens)
    ));
    out.push_str(&format!(
        "provider_cache_creation_tokens: {}\n",
        provider_value(usage, usage.cache_creation_tokens)
    ));
    out.push_str(&format!(
        "selected_code_tokens: {}\n",
        buckets.selected_code_tokens
    ));
    out.push_str(&format!(
        "omitted_candidate_tokens: {}\n",
        buckets.omitted_candidate_tokens
    ));
    out.push_str(&format!("memory_tokens: {}\n", buckets.memory_tokens));
    out.push_str(&format!("skill_tokens: {}\n", buckets.skill_tokens));
    out.push_str(&format!("artifact_tokens: {}\n", buckets.artifact_tokens));
    out.push_str(&format!(
        "retry_diagnostic_tokens: {}\n",
        buckets.retry_diagnostic_tokens
    ));
    out.push_str(&format!(
        "tool_output_tokens: {}\n",
        buckets.tool_output_tokens
    ));
    out.push_str(&format!(
        "context_mention_tokens: {} (attribution only; already counted in concrete buckets)\n",
        buckets.context_mention_tokens
    ));
    out.push_str(&format!("deduped_tokens: {}\n", buckets.deduped_tokens));
    out.push_str(&format!("cached_tokens: {}\n", buckets.cached_tokens));
    out.push_str("provider_reported_tokens_are_billing_source: true\n");
    out
}

fn provider_usage_source(usage: TokenUsage) -> &'static str {
    if usage.input_tokens == 0
        && usage.output_tokens == 0
        && usage.cached_tokens == 0
        && usage.cache_creation_tokens == 0
        && !usage.estimated
    {
        "no provider call"
    } else if usage.estimated {
        "estimated legacy aggregate"
    } else {
        "provider reported"
    }
}

fn provider_value(usage: TokenUsage, value: u64) -> String {
    if provider_usage_source(usage) == "no provider call" {
        "no provider call".into()
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use phonton_types::{PromptContextManifest, SubtaskId, TaskId};

    use super::*;

    #[test]
    fn aggregate_context_buckets_adds_prompt_sources_and_cached_tokens() {
        let task_id = TaskId::new();
        let events = vec![EventRecord {
            task_id,
            timestamp_ms: 1,
            event: OrchestratorEvent::PromptManifest {
                subtask_id: SubtaskId::new(),
                manifest: PromptContextManifest {
                    code_context_tokens: 700,
                    repo_map_tokens: 200,
                    omitted_code_tokens: 4000,
                    memory_tokens: 120,
                    attachment_tokens: 30,
                    retry_error_tokens: 40,
                    mcp_tool_tokens: 50,
                    context_mention_tokens: 30,
                    deduped_tokens: 60,
                    ..Default::default()
                },
            },
        }];

        let buckets = aggregate_context_buckets(
            &events,
            TokenUsage {
                cached_tokens: 300,
                ..Default::default()
            },
        );

        assert_eq!(buckets.selected_code_tokens, 900);
        assert_eq!(buckets.omitted_candidate_tokens, 4000);
        assert_eq!(buckets.memory_tokens, 120);
        assert_eq!(buckets.artifact_tokens, 30);
        assert_eq!(buckets.retry_diagnostic_tokens, 40);
        assert_eq!(buckets.tool_output_tokens, 50);
        assert_eq!(buckets.context_mention_tokens, 30);
        assert_eq!(buckets.deduped_tokens, 60);
        assert_eq!(buckets.cached_tokens, 300);
    }

    #[test]
    fn render_labels_estimates_separately_from_provider_usage() {
        let buckets = ContextBucketSummary {
            selected_code_tokens: 900,
            artifact_tokens: 30,
            cached_tokens: 300,
            ..Default::default()
        };

        let rendered = render_why_tokens_by_source(
            "fix parser",
            &buckets,
            TokenUsage {
                input_tokens: 1200,
                output_tokens: 400,
                cached_tokens: 300,
                cache_creation_tokens: 20,
                estimated: false,
            },
            1800,
        );

        assert!(rendered.contains("local_prompt_estimate_tokens: 1800"));
        assert!(rendered.contains("estimate_source: local tokenizer estimate"));
        assert!(rendered.contains("provider_reported_input_tokens: 1200"));
        assert!(rendered.contains("provider_reported_output_tokens: 400"));
        assert!(rendered.contains("provider_usage_source: provider reported"));
        assert!(rendered.contains("context_mention_tokens: 0 (attribution only"));
        assert!(rendered.contains("provider_reported_tokens_are_billing_source: true"));
    }

    #[test]
    fn render_zero_provider_usage_as_no_provider_call() {
        let rendered = render_why_tokens_by_source(
            "seed local demo",
            &ContextBucketSummary::default(),
            TokenUsage::default(),
            0,
        );

        assert!(rendered.contains("provider_usage_source: no provider call"));
        assert!(rendered.contains("provider_reported_input_tokens: no provider call"));
        assert!(!rendered.contains("provider_reported_input_tokens: 0"));
    }
}
