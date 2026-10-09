use anyhow::{anyhow, Result};
use phonton_types::{EventRecord, OrchestratorEvent, TokenUsage};

use crate::open_persistent_store;

pub async fn run(args: &[String]) -> Result<i32> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
    {
        print_help();
        return Ok(0);
    }
    let store = open_persistent_store()?;
    let task = store
        .list_tasks(20)
        .await?
        .into_iter()
        .find(|task| task.outcome_ledger.is_some())
        .ok_or_else(|| anyhow!("no finished goal with token evidence yet; run a goal first"))?;
    let events = store.list_events(task.id, 1_000)?;
    let report = TokenReport::from_events(&events);
    print!("{}", render(&task.goal_text, &report));
    Ok(0)
}

fn print_help() {
    println!(
        "Usage:\n  phonton why-tokens\n\nShows where the latest goal spent tokens: provider-reported usage per\nverified subtask and the local estimate of code context selected for it."
    );
}

/// Per-goal token evidence rebuilt from stored events.
#[derive(Debug, Default, PartialEq)]
struct TokenReport {
    rows: Vec<Row>,
    context_tokens: u64,
    context_slices: usize,
}

#[derive(Debug, PartialEq)]
struct Row {
    description: String,
    model: String,
    usage: TokenUsage,
}

impl TokenReport {
    fn from_events(events: &[EventRecord]) -> Self {
        let mut report = Self::default();
        for record in events {
            match &record.event {
                OrchestratorEvent::ContextSelected {
                    slices,
                    total_token_count,
                    ..
                } => {
                    report.context_tokens += *total_token_count as u64;
                    report.context_slices += slices.len();
                }
                OrchestratorEvent::SubtaskReviewReady {
                    description,
                    token_usage,
                    model_name,
                    ..
                } => report.rows.push(Row {
                    description: description.clone(),
                    model: model_name.clone(),
                    usage: *token_usage,
                }),
                _ => {}
            }
        }
        report
    }

    fn total(&self) -> TokenUsage {
        self.rows
            .iter()
            .fold(TokenUsage::default(), |mut acc, row| {
                acc.input_tokens += row.usage.input_tokens;
                acc.output_tokens += row.usage.output_tokens;
                acc.cached_tokens += row.usage.cached_tokens;
                acc.cache_creation_tokens += row.usage.cache_creation_tokens;
                acc.estimated |= row.usage.estimated;
                acc
            })
    }
}

fn render(goal_text: &str, report: &TokenReport) -> String {
    let goal = crate::short(goal_text, 90);
    let mut out = format!("Why tokens: {goal}\n\n");
    if report.rows.is_empty() {
        out.push_str("No verified subtasks recorded provider usage for this goal.\n");
    }
    for row in &report.rows {
        let summary = crate::short(crate::subtask_label(&row.description), 42);
        out.push_str(&format!(
            "  {:>7} in  {:>6} out  {:>7} cached  {}  [{}]{}\n",
            row.usage.input_tokens,
            row.usage.output_tokens,
            row.usage.cached_tokens,
            summary,
            row.model,
            if row.usage.estimated {
                "  (estimated)"
            } else {
                ""
            }
        ));
    }
    let total = report.total();
    out.push_str(&format!(
        "\ntotal: {} input, {} output, {} cached{}\n",
        total.input_tokens,
        total.output_tokens,
        total.cached_tokens,
        if total.estimated {
            " (includes estimates)"
        } else {
            " (provider reported)"
        }
    ));
    out.push_str(&format!(
        "code context selected: {} slices, ~{} tokens (local estimate)\n",
        report.context_slices, report.context_tokens
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::{
        CostSummary, ModelTier, ProviderKind, SubtaskId, TaskId, VerifyLayer, VerifyResult,
    };

    fn record(event: OrchestratorEvent) -> EventRecord {
        EventRecord {
            task_id: TaskId::new(),
            timestamp_ms: 1,
            event,
        }
    }

    #[test]
    fn report_sums_provider_usage_and_context() {
        let usage = TokenUsage {
            input_tokens: 1200,
            output_tokens: 80,
            cached_tokens: 400,
            ..Default::default()
        };
        let events = vec![
            record(OrchestratorEvent::ContextSelected {
                subtask_id: SubtaskId::new(),
                slices: Vec::new(),
                total_token_count: 300,
            }),
            record(OrchestratorEvent::SubtaskReviewReady {
                subtask_id: SubtaskId::new(),
                description: "Fix add\nmore".into(),
                tier: ModelTier::Cheap,
                tokens_used: 1280,
                token_usage: usage,
                cost: CostSummary::default(),
                diff_hunks: Vec::new(),
                verify_result: VerifyResult::Pass {
                    layer: VerifyLayer::Test,
                },
                provider: ProviderKind::OpenAiCompatible,
                model_name: "fixture".into(),
            }),
        ];
        let report = TokenReport::from_events(&events);
        assert_eq!(report.context_tokens, 300);
        assert_eq!(report.total().input_tokens, 1200);
        let text = render("goal", &report);
        assert!(text.contains("Fix add"));
        assert!(text.contains("total: 1200 input, 80 output, 400 cached (provider reported)"));
    }
}
