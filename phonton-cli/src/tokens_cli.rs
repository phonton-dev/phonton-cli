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

    println!("Why tokens by source");
    println!("task: {}", task.goal_text);
    println!("selected_code_tokens: {}", buckets.selected_code_tokens);
    println!(
        "omitted_candidate_tokens: {}",
        buckets.omitted_candidate_tokens
    );
    println!("memory_tokens: {}", buckets.memory_tokens);
    println!("skill_tokens: {}", buckets.skill_tokens);
    println!("artifact_tokens: {}", buckets.artifact_tokens);
    println!(
        "retry_diagnostic_tokens: {}",
        buckets.retry_diagnostic_tokens
    );
    println!("tool_output_tokens: {}", buckets.tool_output_tokens);
    println!("deduped_tokens: {}", buckets.deduped_tokens);
    println!("cached_tokens: {}", buckets.cached_tokens);
    println!("provider_reported_tokens_are_billing_source: true");
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
        assert_eq!(buckets.deduped_tokens, 60);
        assert_eq!(buckets.cached_tokens, 300);
    }
}
