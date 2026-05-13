//! Task decomposition and model-tier routing.
//!
//! The planner turns a natural-language [`Goal`] into a topologically-ordered
//! [`PlannerOutput`]. The shipping implementation calls a cheap model;
//! this in-crate decomposer uses a regex sweep over the goal text and
//! exists so:
//!
//! 1. The orchestrator can be wired against a real `decompose` signature
//!    today.
//! 2. The Risk 2 "testing trap" mitigation
//!    (`01-architecture/failure-modes.md`) is implemented end-to-end:
//!    every detected new symbol gets a paired test subtask that depends on
//!    its implementation.
//!
//! When the LLM-backed decomposer lands it must produce the same
//! [`PlannerOutput`] shape, including a populated [`CoverageSummary`].

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use phonton_memory::MemoryStore;
use phonton_providers::Provider;
use phonton_store::{MemoryKind, Store};
use phonton_types::{
    AcceptanceSlice, CoverageSummary, ExpectedArtifact, GoalContract, MemoryRecord, ModelTier,
    PlannerOutput, PromptAttachment, QualityFloor, RunCommand, Subtask, SubtaskId, SubtaskStatus,
    TaskClass, TokenPolicy, VerifyLayer, VerifyStepSpec,
};
use regex::Regex;
use serde::Deserialize;
use tracing::{debug, warn};

/// Top-k rejected approaches pulled from memory per goal.
pub const MEMORY_TOP_K: usize = 5;

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// A user-issued goal awaiting decomposition.
#[derive(Debug, Clone)]
pub struct Goal {
    /// Free-form natural-language description, as typed into the task board.
    pub description: String,
    /// Default tier for implementation subtasks. Test subtasks always run
    /// at one tier below (`Cheap` floor) — they're cheap by nature.
    pub default_tier: ModelTier,
    /// If `true`, suppress automatic test-subtask generation. Mirrors the
    /// `--no-tests` user flag.
    pub no_tests: bool,
    /// User-mentioned files/images attached to this goal.
    pub attachments: Vec<PromptAttachment>,
}

impl Goal {
    /// Construct a goal with sensible defaults: `Standard` tier, tests on.
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            default_tier: ModelTier::Standard,
            no_tests: false,
            attachments: Vec::new(),
        }
    }

    /// Attach user-mentioned files/images to this goal.
    pub fn with_attachments(mut self, attachments: Vec<PromptAttachment>) -> Self {
        self.attachments = attachments;
        self
    }

    /// Render the goal and attachment context for planner prompts.
    pub fn prompt_text(&self) -> String {
        let attachment_context = phonton_types::render_prompt_attachments(&self.attachments);
        if attachment_context.is_empty() {
            self.description.clone()
        } else {
            format!("{}\n\n{}", self.description, attachment_context)
        }
    }

    /// Build a conservative first-pass contract for the goal.
    pub fn contract(&self) -> GoalContract {
        let intent = phonton_types::classify_intent(&self.description);
        let task_class = intent.task_class;
        let chess_goal = is_chess_goal(&self.description);
        let html_chess_goal = is_html_chess_goal(&self.description);
        let generated_app_goal = matches!(task_class, TaskClass::GeneratedAppGame);
        let web_artifact_goal = is_web_artifact_goal(&self.description);
        let mut assumptions = Vec::new();
        if !self.attachments.is_empty() {
            assumptions.push(format!(
                "{} mentioned file/image attachment(s) should influence the plan.",
                self.attachments.len()
            ));
        }
        let mut acceptance_criteria = vec![
            "Produce a focused, reviewable diff for the requested change.".into(),
            "Respect mentioned file/image attachments when planning and editing.".into(),
            "Do not claim correctness beyond checks that actually ran.".into(),
        ];
        let mut quality_criteria = vec![
            "Diff is parseable and reviewable.".into(),
            "Verification outcome is surfaced honestly.".into(),
        ];
        let mut acceptance_slices = Vec::new();
        let mut likely_files: Vec<PathBuf> =
            self.attachments.iter().map(|a| a.path.clone()).collect();
        let mut verify_plan = vec![VerifyStepSpec {
            name: "Run configured Phonton verification layers".into(),
            layer: None,
            command: None,
        }];
        let mut run_plan = Vec::<RunCommand>::new();
        let mut token_policy = TokenPolicy::default();
        let mut expected_artifacts: Vec<ExpectedArtifact> = self
            .attachments
            .iter()
            .map(|a| ExpectedArtifact {
                description: format!("Use mentioned attachment {}", a.path.display()),
                path: Some(a.path.clone()),
            })
            .collect();
        if chess_goal {
            acceptance_criteria.extend([
                "Produce a playable chess artifact, not a placeholder or greeting.".into(),
                "Represent an 8x8 board, named chess pieces, turns, legal move handling, and reset/new-game behavior.".into(),
                "Provide a concrete run command and at least one build/test/verification command.".into(),
            ]);
            quality_criteria.extend([
                "Trivial output such as printing \"Chess\" is below the quality floor.".into(),
                "The result must include enough game logic for a user to interact with chess moves.".into(),
            ]);
            expected_artifacts.push(ExpectedArtifact {
                description: "Playable chess implementation".into(),
                path: if html_chess_goal {
                    Some(PathBuf::from("index.html"))
                } else {
                    None
                },
            });
            if html_chess_goal {
                likely_files.push(PathBuf::from("index.html"));
                verify_plan.extend(runtime_web_verify_plan());
                run_plan.push(RunCommand {
                    label: "Serve static chess page".into(),
                    command: vec![
                        "python".into(),
                        "-m".into(),
                        "http.server".into(),
                        "8000".into(),
                    ],
                    cwd: None,
                });
            }
            let artifact_path = html_chess_goal.then(|| PathBuf::from("index.html"));
            let slice_verify_plan = if html_chess_goal {
                runtime_web_verify_plan()
            } else {
                verify_plan.clone()
            };
            acceptance_slices = chess_acceptance_slices(artifact_path, slice_verify_plan);
            token_policy = TokenPolicy {
                first_attempt_cap_tokens: Some(8_000),
                allow_broad_repair: false,
                repair_only_missing_criteria: true,
                notes: vec![
                    "Use contract preflight before implementation.".into(),
                    "Repair only missing acceptance criteria after verifier evidence.".into(),
                ],
            };
        } else if generated_app_goal {
            acceptance_criteria.extend([
                "Produce the requested app/game artifact in bounded acceptance slices.".into(),
                "Each slice must preserve previously accepted behavior and avoid broad rewrites."
                    .into(),
                "Provide concrete runtime instructions when the artifact is runnable.".into(),
            ]);
            quality_criteria.extend([
                "Placeholder-only output is below the quality floor.".into(),
                "Primary user interaction must mutate visible state or behavior.".into(),
            ]);
            let artifact_path = web_artifact_goal.then(|| PathBuf::from("index.html"));
            if let Some(path) = &artifact_path {
                likely_files.push(path.clone());
                expected_artifacts.push(ExpectedArtifact {
                    description: "Runnable generated web artifact".into(),
                    path: Some(path.clone()),
                });
                verify_plan.extend(runtime_web_verify_plan());
                run_plan.push(RunCommand {
                    label: "Serve generated web artifact".into(),
                    command: vec![
                        "python".into(),
                        "-m".into(),
                        "http.server".into(),
                        "8000".into(),
                    ],
                    cwd: None,
                });
            }
            let slice_verify_plan = if web_artifact_goal {
                runtime_web_verify_plan()
            } else {
                verify_plan.clone()
            };
            acceptance_slices =
                generated_app_acceptance_slices(artifact_path.clone(), slice_verify_plan);
            token_policy = TokenPolicy {
                first_attempt_cap_tokens: Some(6_000),
                allow_broad_repair: false,
                repair_only_missing_criteria: true,
                notes: vec![
                    "Dispatch generated-app work as bounded acceptance slices.".into(),
                    "Fail with replan instead of broad auto-repair when multiple criteria are missing.".into(),
                ],
            };
        }
        GoalContract {
            goal: self.description.clone(),
            task_class,
            intent: Some(intent.clone()),
            confidence_percent: intent.confidence_percent,
            acceptance_criteria,
            acceptance_slices,
            expected_artifacts,
            likely_files,
            verify_plan,
            run_plan,
            quality_floor: QualityFloor {
                criteria: quality_criteria,
            },
            clarification_questions: if !chess_goal
                && self.description.split_whitespace().count() <= 2
            {
                vec!["What exact behavior or artifact should Phonton produce?".into()]
            } else {
                Vec::new()
            },
            assumptions,
            token_policy,
        }
    }
}

fn is_chess_goal(description: &str) -> bool {
    let lower = description.to_ascii_lowercase();
    (lower.contains("make") || lower.contains("build") || lower.contains("create"))
        && lower.contains("chess")
}

fn is_html_chess_goal(description: &str) -> bool {
    let lower = description.to_ascii_lowercase();
    is_chess_goal(description) && (lower.contains("html") || lower.contains("web"))
}

fn is_web_artifact_goal(description: &str) -> bool {
    let lower = description.to_ascii_lowercase();
    lower.contains("html")
        || lower.contains("web")
        || lower.contains("website")
        || lower.contains("single page")
}

fn runtime_web_verify_plan() -> Vec<VerifyStepSpec> {
    vec![
        VerifyStepSpec {
            name: "browser runtime smoke".into(),
            layer: Some(VerifyLayer::RuntimeSmoke),
            command: None,
        },
        VerifyStepSpec {
            name: "browser DOM check".into(),
            layer: Some(VerifyLayer::BrowserDomCheck),
            command: None,
        },
        VerifyStepSpec {
            name: "browser interaction check".into(),
            layer: Some(VerifyLayer::InteractionCheck),
            command: None,
        },
    ]
}

fn chess_acceptance_slices(
    path: Option<PathBuf>,
    verify_plan: Vec<VerifyStepSpec>,
) -> Vec<AcceptanceSlice> {
    [
        ("board", "render an 8x8 board"),
        ("pieces", "show named or symbolic chess pieces"),
        ("select_move", "select and move pieces"),
        ("turns", "enforce turn handling"),
        ("illegal_moves", "reject illegal or invalid moves"),
        ("reset", "expose reset or new game"),
        ("run", "provide a concrete run command"),
    ]
    .into_iter()
    .map(|(id, criterion)| AcceptanceSlice {
        id: id.into(),
        criterion: criterion.into(),
        artifact_path: path.clone(),
        verify_plan: verify_plan.clone(),
    })
    .collect()
}

fn generated_app_acceptance_slices(
    path: Option<PathBuf>,
    verify_plan: Vec<VerifyStepSpec>,
) -> Vec<AcceptanceSlice> {
    [
        (
            "artifact_shell",
            "create the smallest runnable artifact for the requested app or game",
        ),
        (
            "domain_elements",
            "render the domain-specific elements named in the goal",
        ),
        (
            "primary_interaction",
            "wire the primary user interaction so it changes visible state",
        ),
        (
            "invalid_state",
            "handle invalid or no-op actions without crashing",
        ),
        (
            "reset_or_restart",
            "provide reset, restart, clear, or equivalent recovery behavior when applicable",
        ),
        ("run", "provide a concrete run command"),
    ]
    .into_iter()
    .map(|(id, criterion)| AcceptanceSlice {
        id: id.into(),
        criterion: criterion.into(),
        artifact_path: path.clone(),
        verify_plan: verify_plan.clone(),
    })
    .collect()
}

// ---------------------------------------------------------------------------
// Decomposition
// ---------------------------------------------------------------------------

/// Decompose `goal` into a [`PlannerOutput`].
///
/// Behaviour:
///
/// * Scans the goal for new-symbol verbs (`add`, `create`, `implement`,
///   `introduce`, `write`, `define`) paired with kinds (`function`/`fn`,
///   `struct`, `enum`, `trait`, `method`, `module`, `type`).
/// * Emits one implementation subtask per detected symbol.
/// * Unless `goal.no_tests` is set, emits a `"Write integration tests for
///   {name}"` subtask **per implementation**, with a dependency on the
///   implementation subtask. Test subtasks run at `Cheap` (or `Local` if
///   the default tier is already `Local`).
/// * If no symbols are detected, falls back to a single generic
///   implementation subtask using the goal text verbatim.
/// * Populates [`CoverageSummary`] with the count of detected symbols and
///   the number of paired test subtasks.
pub fn decompose(goal: &Goal) -> PlannerOutput {
    let contract = goal.contract();
    if !contract.acceptance_slices.is_empty() {
        let subtasks = acceptance_slice_subtasks(goal, &contract);
        return PlannerOutput {
            estimated_total_tokens: estimate_acceptance_slice_tokens(subtasks.len()),
            naive_baseline_tokens: estimate_acceptance_slice_naive_tokens(subtasks.len()),
            coverage_summary: CoverageSummary {
                new_functions: 0,
                tests_planned: 0,
            },
            goal_contract: Some(contract),
            subtasks,
        };
    }

    let detections = detect_new_symbols(&goal.description);

    let mut subtasks: Vec<Subtask> = Vec::new();
    let mut new_functions = 0usize;
    let mut tests_planned = 0usize;

    if detections.is_empty() {
        // Fallback: one catch-all implementation subtask.
        subtasks.push(Subtask {
            id: SubtaskId::new(),
            description: goal.description.clone(),
            model_tier: goal.default_tier,
            dependencies: Vec::new(),
            attachments: goal.attachments.clone(),
            status: SubtaskStatus::Queued,
        });
    } else {
        for d in &detections {
            new_functions += 1;
            let impl_id = SubtaskId::new();
            subtasks.push(Subtask {
                id: impl_id,
                description: format!("Implement {} `{}`", d.kind, d.name),
                model_tier: goal.default_tier,
                dependencies: Vec::new(),
                attachments: goal.attachments.clone(),
                status: SubtaskStatus::Queued,
            });

            if !goal.no_tests {
                tests_planned += 1;
                subtasks.push(Subtask {
                    id: SubtaskId::new(),
                    description: format!("Write integration tests for {}", d.name),
                    model_tier: test_tier(goal.default_tier),
                    dependencies: vec![impl_id],
                    attachments: goal.attachments.clone(),
                    status: SubtaskStatus::Queued,
                });
            }
        }
    }

    PlannerOutput {
        subtasks,
        estimated_total_tokens: estimate_tokens(&detections, goal),
        naive_baseline_tokens: estimate_naive_tokens(&detections, goal),
        coverage_summary: CoverageSummary {
            new_functions,
            tests_planned,
        },
        goal_contract: Some(contract),
    }
}

fn acceptance_slice_subtasks(goal: &Goal, contract: &GoalContract) -> Vec<Subtask> {
    let total = contract.acceptance_slices.len();
    let mut subtasks = Vec::with_capacity(total);
    let mut previous = None;

    for (idx, slice) in contract.acceptance_slices.iter().enumerate() {
        let id = SubtaskId::new();
        let artifact = slice
            .artifact_path
            .as_ref()
            .map(|path| format!(" Artifact: {}.", path.display()))
            .unwrap_or_default();
        let description = format!(
            "Acceptance slice {}/{} for `{}`: {}.{} Keep the diff minimal; satisfy only this slice and preserve earlier slices.",
            idx + 1,
            total,
            goal.description,
            slice.criterion,
            artifact
        );
        subtasks.push(Subtask {
            id,
            description,
            model_tier: generated_slice_tier(goal.default_tier),
            dependencies: previous.into_iter().collect(),
            attachments: goal.attachments.clone(),
            status: SubtaskStatus::Queued,
        });
        previous = Some(id);
    }

    subtasks
}

fn generated_slice_tier(default_tier: ModelTier) -> ModelTier {
    match default_tier {
        ModelTier::Frontier => ModelTier::Standard,
        other => other,
    }
}

/// Alias for [`decompose`] — the regex-based fallback used when no LLM
/// provider is wired in or when the model's response fails to parse.
pub fn decompose_regex(goal: &Goal) -> PlannerOutput {
    decompose(goal)
}

/// One subtask as produced by the LLM decomposer.
///
/// The LLM returns indices into its own array rather than UUIDs; the
/// planner maps them onto freshly-minted [`SubtaskId`]s when assembling
/// the final [`Subtask`] DAG.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SubtaskSpec {
    pub description: String,
    pub model_tier: ModelTier,
    #[serde(default)]
    pub depends_on: Vec<usize>,
}

/// Structured output of the LLM-backed decomposer. Converted into a
/// `PlannerOutput` by the entry-point shims below.
#[derive(Debug, Clone)]
pub struct DecomposedPlan {
    pub subtasks: Vec<Subtask>,
    pub coverage_summary: CoverageSummary,
    pub estimated_total_tokens: u64,
    pub naive_baseline_tokens: u64,
}

/// Decompose `goal` via an LLM provider, falling back to [`decompose_regex`]
/// if the model's response can't be parsed as the structured JSON array.
///
/// `memory_context` is injected into the "Prior context from memory"
/// block of the prompt; empty string is fine. Cycle-containing
/// dependency arrays from the model are rejected (causing the regex
/// fallback) to preserve the DAG invariant the orchestrator relies on.
pub async fn decompose_with_llm(
    goal: &str,
    provider: Arc<dyn Provider>,
    memory_context: &str,
) -> Result<DecomposedPlan> {
    let system = "You are a software task decomposer.";
    let user = build_decomposer_prompt(goal, memory_context);

    let response = provider.call(system, &user, &[]).await?;
    match parse_subtask_specs(&response.content) {
        Ok(specs) if !specs.is_empty() && is_dag(&specs) => {
            let plan = specs_to_plan(specs);
            let dets = detect_new_symbols(goal);
            Ok(DecomposedPlan {
                subtasks: plan.subtasks,
                coverage_summary: plan.coverage_summary,
                estimated_total_tokens: estimate_tokens(&dets, &Goal::new(goal)),
                naive_baseline_tokens: estimate_naive_tokens(&dets, &Goal::new(goal)),
            })
        }
        Ok(_) => {
            warn!("LLM decomposer returned empty or cyclic plan; falling back to regex");
            let fallback = decompose_regex(&Goal::new(goal.to_string()));
            Ok(DecomposedPlan {
                subtasks: fallback.subtasks,
                coverage_summary: fallback.coverage_summary,
                estimated_total_tokens: fallback.estimated_total_tokens,
                naive_baseline_tokens: fallback.naive_baseline_tokens,
            })
        }
        Err(e) => {
            warn!(error = %e, "LLM decomposer JSON parse failed; falling back to regex");
            let fallback = decompose_regex(&Goal::new(goal.to_string()));
            Ok(DecomposedPlan {
                subtasks: fallback.subtasks,
                coverage_summary: fallback.coverage_summary,
                estimated_total_tokens: fallback.estimated_total_tokens,
                naive_baseline_tokens: fallback.naive_baseline_tokens,
            })
        }
    }
}

fn build_decomposer_prompt(goal: &str, memory_context: &str) -> String {
    let ctx = if memory_context.is_empty() {
        "(none)".to_string()
    } else {
        memory_context.to_string()
    };
    format!(
        "You are a software task decomposer. Break the following goal into 2–6 concrete subtasks.\n\
\n\
Prior context from memory:\n\
{ctx}\n\
\n\
Goal: {goal}\n\
\n\
Respond ONLY with a JSON array, no prose:\n\
[\n\
  {{\n\
    \"description\": \"...\",\n\
    \"model_tier\": \"Cheap|Standard|Frontier\",\n\
    \"depends_on\": []\n\
  }}\n\
]\n\
\n\
Rules:\n\
- Each subtask must be a single focused code change\n\
- Assign Cheap tier for tests and documentation, Standard for implementation, Frontier only for complex architecture\n\
- depends_on must form a DAG (no cycles)\n"
    )
}

/// Extract the JSON array from `content`. Accepts either a bare JSON
/// array or one wrapped in ```json ``` fences — both shapes appear in
/// practice depending on the model.
fn parse_subtask_specs(content: &str) -> Result<Vec<SubtaskSpec>> {
    let trimmed = content.trim();
    let json = if let (Some(start), Some(end)) = (trimmed.find('['), trimmed.rfind(']')) {
        if end >= start {
            &trimmed[start..=end]
        } else {
            trimmed
        }
    } else {
        trimmed
    };
    let specs: Vec<SubtaskSpec> = serde_json::from_str(json)?;
    for (i, s) in specs.iter().enumerate() {
        for &dep in &s.depends_on {
            if dep >= specs.len() {
                anyhow::bail!("subtask {i} depends on out-of-range index {dep}");
            }
            if dep == i {
                anyhow::bail!("subtask {i} depends on itself");
            }
        }
    }
    Ok(specs)
}

/// Cycle check via Kahn-style in-degree reduction. Returns true iff
/// every node can be removed, i.e. the graph is a DAG.
fn is_dag(specs: &[SubtaskSpec]) -> bool {
    let n = specs.len();
    let mut in_deg: Vec<usize> = vec![0; n];
    for s in specs {
        for _ in &s.depends_on {
            // in_deg counts incoming edges, but our `depends_on` lists
            // incoming edges directly — so increment the current node.
        }
    }
    for (i, s) in specs.iter().enumerate() {
        in_deg[i] = s.depends_on.len();
    }
    let mut ready: Vec<usize> = (0..n).filter(|i| in_deg[*i] == 0).collect();
    let mut removed = 0usize;
    while let Some(i) = ready.pop() {
        removed += 1;
        for (j, s) in specs.iter().enumerate() {
            if s.depends_on.contains(&i) {
                in_deg[j] = in_deg[j].saturating_sub(1);
                if in_deg[j] == 0 {
                    ready.push(j);
                }
            }
        }
    }
    removed == n
}

fn specs_to_plan(specs: Vec<SubtaskSpec>) -> DecomposedPlan {
    let ids: Vec<SubtaskId> = specs.iter().map(|_| SubtaskId::new()).collect();
    let mut subtasks = Vec::with_capacity(specs.len());
    let mut tests_planned = 0usize;
    let mut new_functions = 0usize;
    for (i, spec) in specs.into_iter().enumerate() {
        let dependencies: Vec<SubtaskId> = spec.depends_on.iter().map(|&j| ids[j]).collect();
        let is_test = matches!(spec.model_tier, ModelTier::Cheap)
            && spec.description.to_ascii_lowercase().contains("test");
        if is_test {
            tests_planned += 1;
        } else {
            new_functions += 1;
        }
        subtasks.push(Subtask {
            id: ids[i],
            description: spec.description,
            model_tier: spec.model_tier,
            dependencies,
            attachments: Vec::new(),
            status: SubtaskStatus::Queued,
        });
    }
    DecomposedPlan {
        subtasks,
        coverage_summary: CoverageSummary {
            new_functions,
            tests_planned,
        },
        estimated_total_tokens: 0, // Filled in by caller
        naive_baseline_tokens: 0,  // Filled in by caller
    }
}

/// Decompose `goal` with memory consultation.
///
/// Before running [`decompose`], queries `store` for prior `RejectedApproach`
/// and `Decision` records whose denormalised topic substring-matches the
/// goal description. The resulting plan:
///
/// 1. **Skips** any detected symbol whose name appears in a
///    `RejectedApproach` summary — repeating a known dead-end is the
///    specific failure mode memory exists to prevent.
/// 2. **Prefixes** the first subtask's description with a short
///    "Prior context" block carrying the matched records, so workers see
///    prior decisions as they execute.
///
/// Memory is now a **warm** input to planning: queried on every goal, not
/// just for retries. The query hits SQLite and is read-only; it does not
/// write back.
pub async fn decompose_with_memory(
    goal: &Goal,
    store: &Store,
    provider: Option<Arc<dyn Provider>>,
) -> Result<PlannerOutput> {
    // `query_memory` matches topic substrings. A whole-goal string rarely
    // appears verbatim in a stored topic, so we fan out per keyword — the
    // detected symbol names plus any 5+ char identifier token in the goal
    // — and de-duplicate.
    let keywords = memory_keywords(&goal.description);
    let rejected = query_unique(store, &keywords, MemoryKind::RejectedApproach)?;
    let decisions = query_unique(store, &keywords, MemoryKind::Decision)?;

    debug!(
        rejected = rejected.len(),
        decisions = decisions.len(),
        "planner consulted memory"
    );

    // LLM path: when a provider is wired in, let the model do the
    // decomposition with the prior-context preamble as its memory input.
    // On any failure the helper itself falls back to the regex path, so
    // there's no need to double-guard here.
    if let Some(p) = provider {
        let memory_context = render_memory_preamble(&rejected, &decisions);
        let mut llm_plan = decompose_with_llm(&goal.prompt_text(), p, &memory_context).await?;
        for subtask in &mut llm_plan.subtasks {
            subtask.attachments = goal.attachments.clone();
        }
        return Ok(PlannerOutput {
            subtasks: llm_plan.subtasks,
            estimated_total_tokens: llm_plan.estimated_total_tokens,
            naive_baseline_tokens: llm_plan.naive_baseline_tokens,
            coverage_summary: llm_plan.coverage_summary,
            goal_contract: Some(goal.contract()),
        });
    }

    let mut plan = decompose(goal);

    // Filter out detections whose symbol name is recorded as a rejected
    // approach. We match on the rejection `summary` substring because that
    // is the column indexed in `memory_records.topic`.
    if !rejected.is_empty() {
        let rejected_summaries: Vec<String> = rejected
            .iter()
            .filter_map(|r| match r {
                MemoryRecord::RejectedApproach { summary, .. } => Some(summary.to_lowercase()),
                _ => None,
            })
            .collect();
        let before = plan.subtasks.len();
        plan.subtasks.retain(|st| {
            !rejected_summaries
                .iter()
                .any(|s| s.contains(&extract_symbol_name(&st.description).to_lowercase()))
        });
        let removed = before - plan.subtasks.len();
        if removed > 0 {
            debug!(
                removed,
                "planner skipped subtasks matching rejected approaches"
            );
            // Keep coverage honest: dropped implementations no longer count.
            plan.coverage_summary.new_functions =
                plan.coverage_summary.new_functions.saturating_sub(removed);
        }
    }

    // Inject a prior-context preamble into the first subtask description.
    // An LLM-backed planner would put this into its own prompt; in the
    // stub this surfaces the same data to the downstream worker.
    let preamble = render_memory_preamble(&rejected, &decisions);
    if !preamble.is_empty() {
        if let Some(first) = plan.subtasks.first_mut() {
            first.description = format!("{preamble}\n\n{}", first.description);
        }
    }

    Ok(plan)
}

/// Decompose `goal` using the async [`MemoryStore`] facade.
///
/// Queries for the top 5 records with keyword overlap against the goal
/// description and prepends a `# Prior context` block — listing each
/// record's searchable fields — to the first subtask's description, so
/// downstream workers inherit the context their planner saw.
///
/// Pure read: never writes back. Returns the plan unchanged if no
/// records overlap.
pub async fn decompose_with_memory_store(
    goal: &Goal,
    memory: &MemoryStore,
) -> Result<PlannerOutput> {
    let records = memory.query(&goal.description, 5).await?;
    let mut plan = decompose(goal);

    let records: Vec<MemoryRecord> = records
        .into_iter()
        .filter(|record| !is_generic_completion_memory(record))
        .take(5)
        .collect();

    if records.is_empty() {
        return Ok(plan);
    }

    let mut preamble = String::from("# Prior context\n");
    for rec in &records {
        preamble.push_str(&format!("- {}\n", render_record_line(rec)));
    }

    if let Some(first) = plan.subtasks.first_mut() {
        first.description = format!("{}\n{}", cap_memory_preamble(preamble), first.description);
    }
    Ok(plan)
}

fn render_record_line(rec: &MemoryRecord) -> String {
    match rec {
        MemoryRecord::Decision { title, body, .. } => format!("decision: {title} — {body}"),
        MemoryRecord::Constraint {
            statement,
            rationale,
        } => {
            format!("constraint: {statement} (because {rationale})")
        }
        MemoryRecord::RejectedApproach { summary, reason } => {
            format!("rejected: {summary} (reason: {reason})")
        }
        MemoryRecord::Convention { rule, scope } => match scope {
            Some(s) => format!("convention ({s}): {rule}"),
            None => format!("convention: {rule}"),
        },
    }
}

fn is_generic_completion_memory(rec: &MemoryRecord) -> bool {
    matches!(
        rec,
        MemoryRecord::Decision { body, .. }
            if body.trim_start().to_ascii_lowercase().starts_with("completed:")
    )
}

/// Collect distinct keyword queries to fan out against memory.
fn memory_keywords(goal: &str) -> Vec<String> {
    let ident = Regex::new(r"[A-Za-z_][A-Za-z0-9_]{4,}").expect("keyword regex is well-formed");
    let mut out: Vec<String> = Vec::new();
    for m in ident.find_iter(goal) {
        let w = m.as_str();
        if is_kind_word(w) || is_stopword(w) {
            continue;
        }
        if !out.iter().any(|x| x == w) {
            out.push(w.to_string());
        }
    }
    out
}

fn is_stopword(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "a" | "an"
            | "the"
            | "and"
            | "with"
            | "for"
            | "into"
            | "that"
            | "this"
            | "from"
            | "when"
            | "then"
            | "also"
            | "base"
            | "basic"
            | "skeletal"
            | "initial"
            | "simple"
            | "project"
            | "everything"
            | "anything"
    )
}

/// Run `query_memory` for every keyword and concatenate unique records
/// (dedup on JSON-serialised form since `MemoryRecord` is not `Hash`).
fn query_unique(store: &Store, keywords: &[String], kind: MemoryKind) -> Result<Vec<MemoryRecord>> {
    let mut out: Vec<MemoryRecord> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for k in keywords {
        for rec in store.search_memory(k, Some(kind), MEMORY_TOP_K)? {
            let key = format!("{rec:?}");
            if seen.iter().any(|s| s == &key) {
                continue;
            }
            seen.push(key);
            out.push(rec);
        }
    }
    Ok(out)
}

/// Pull an identifier-looking token out of a subtask description. Falls
/// back to the whole description when no clear token is present.
fn extract_symbol_name(desc: &str) -> String {
    // Subtask descriptions produced by `decompose` are of the form
    // `Implement function `name`` or `Write integration tests for name`.
    let ident = Regex::new(r"[`']?(?P<name>[A-Za-z_][A-Za-z0-9_]{2,})[`']?")
        .expect("ident regex is well-formed");
    let hit = ident
        .captures_iter(desc)
        .map(|c| c["name"].to_string())
        .find(|n| !is_kind_word(n) && !is_noise_word(n));
    hit.unwrap_or_else(|| desc.to_string())
}

fn is_noise_word(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "implement" | "write" | "integration" | "tests" | "for" | "the"
    )
}

/// Render the "Prior context" preamble from matched memory records. Empty
/// when no records matched — caller should skip injection in that case.
fn render_memory_preamble(rejected: &[MemoryRecord], decisions: &[MemoryRecord]) -> String {
    let rejected: Vec<&MemoryRecord> = rejected
        .iter()
        .filter(|record| !is_generic_completion_memory(record))
        .take(3)
        .collect();
    let decisions: Vec<&MemoryRecord> = decisions
        .iter()
        .filter(|record| !is_generic_completion_memory(record))
        .take(5)
        .collect();
    if rejected.is_empty() && decisions.is_empty() {
        return String::new();
    }
    let mut out = String::from("# Prior context from memory\n");
    if !rejected.is_empty() {
        out.push_str("\n## Do NOT repeat these rejected approaches\n");
        for r in rejected {
            if let MemoryRecord::RejectedApproach { summary, reason } = r {
                out.push_str(&format!("- {summary} (rejected: {reason})\n"));
            }
        }
    }
    if !decisions.is_empty() {
        out.push_str("\n## Honour these prior decisions\n");
        for r in decisions {
            if let MemoryRecord::Decision { title, body, .. } = r {
                out.push_str(&format!("- {title}: {body}\n"));
            }
        }
    }
    cap_memory_preamble(out)
}

fn cap_memory_preamble(mut preamble: String) -> String {
    const MAX_MEMORY_PREAMBLE_CHARS: usize = 1_200;
    if preamble.chars().count() <= MAX_MEMORY_PREAMBLE_CHARS {
        return preamble;
    }
    preamble = preamble.chars().take(MAX_MEMORY_PREAMBLE_CHARS).collect();
    preamble.push_str("\n- [memory context truncated]\n");
    preamble
}

/// Tier used for auto-generated test subtasks. Tests are routine output
/// and rarely need a frontier model; route them one tier below the
/// implementation, with `Local` as a floor.
fn test_tier(impl_tier: ModelTier) -> ModelTier {
    match impl_tier {
        ModelTier::Frontier => ModelTier::Standard,
        ModelTier::Standard => ModelTier::Cheap,
        ModelTier::Cheap | ModelTier::Local => ModelTier::Local,
    }
}

/// Crude token estimate for the planner's headline number. Real
/// estimation lives in the LLM-backed planner; this is a placeholder
/// the orchestrator can pass through to the UI without crashing.
fn estimate_tokens(detections: &[Detection], goal: &Goal) -> u64 {
    let base = 2_000u64;
    let per_impl = 1_500u64;
    let per_test = 800u64;
    let impls = detections.len().max(1) as u64;
    let tests = if goal.no_tests {
        0
    } else {
        detections.len() as u64
    };
    base + per_impl * impls + per_test * tests
}

fn estimate_acceptance_slice_tokens(slice_count: usize) -> u64 {
    900 + (slice_count as u64).saturating_mul(1_350)
}

/// Crude naive baseline estimate. Assumes a stateless agent would load
/// the entire relevant file set for every turn.
fn estimate_naive_tokens(detections: &[Detection], goal: &Goal) -> u64 {
    // 30k input tokens is a typical "whole file load" payload for a medium
    // repo turn. 5k output for the inevitable narration and whole-file
    // rewrites.
    let per_turn = 35_000u64;
    let turns = detections.len().max(1) as u64;
    let tests = if goal.no_tests {
        0
    } else {
        detections.len() as u64
    };
    (turns + tests) * per_turn
}

fn estimate_acceptance_slice_naive_tokens(slice_count: usize) -> u64 {
    // A broad generated app/game attempt tends to reload the whole artifact
    // and verifier feedback per turn; keep the baseline per acceptance slice.
    (slice_count.max(1) as u64).saturating_mul(35_000)
}

// ---------------------------------------------------------------------------
// Task classification (for dynamic routing)
// ---------------------------------------------------------------------------

pub use phonton_types::{classify_task, effective_tier};

// ---------------------------------------------------------------------------
// Symbol detection
// ---------------------------------------------------------------------------

/// One symbol the planner inferred from the goal text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// Kind word that triggered the match (`function`, `struct`, ...).
    pub kind: String,
    /// Identifier extracted immediately after the kind word.
    pub name: String,
}

/// Scan `text` for "<verb> <kind> <name>" phrases and return one
/// [`Detection`] per match. The patterns are intentionally narrow — we
/// would rather miss a symbol and emit one generic subtask than
/// hallucinate a symbol the user didn't ask for.
pub fn detect_new_symbols(text: &str) -> Vec<Detection> {
    // Single regex with kind + name capture. The verb prefix narrows the
    // match so we don't pick up phrases like "the function foo is broken".
    //
    // Examples that match:
    //   "add a function `parse_callsites`"
    //   "create struct ExecutionGuard"
    //   "implement the verify_diff function"
    //   "introduce a trait Provider"
    //   "define enum VerifyLayer"
    let re = Regex::new(
        r"(?ix)
        \b(?P<verb>add|create|implement|introduce|write|define|build|make)\b
        [^\.\n]{0,40}?
        \b(?P<kind>function|fn|struct|enum|trait|method|module|type)\b
        [\s:`'(]*
        (?P<name>[A-Za-z_][A-Za-z0-9_]*)
        ",
    )
    .expect("planner regex is well-formed");

    let mut out: Vec<Detection> = Vec::new();
    for caps in re.captures_iter(text) {
        let kind = caps
            .name("kind")
            .map(|m| normalise_kind(m.as_str()))
            .unwrap_or_else(|| "feature".to_string());

        let Some(name_match) = caps.name("name") else {
            continue;
        };
        let name = name_match.as_str().to_string();

        if name.is_empty() || is_kind_word(&name) {
            continue;
        }
        let det = Detection { kind, name };
        if !out.contains(&det) {
            out.push(det);
        }
    }
    out
}

fn normalise_kind(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    match lower.as_str() {
        "fn" => "function".into(),
        other => other.into(),
    }
}

fn is_kind_word(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "function" | "fn" | "struct" | "enum" | "trait" | "method" | "module" | "type"
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::TaskClass;

    #[test]
    fn detects_function_and_struct() {
        let dets =
            detect_new_symbols("Add a function parse_callsites and create struct ExecutionGuard.");
        assert_eq!(dets.len(), 2);
        assert_eq!(dets[0].name, "parse_callsites");
        assert_eq!(dets[0].kind, "function");
        assert_eq!(dets[1].name, "ExecutionGuard");
        assert_eq!(dets[1].kind, "struct");
    }

    #[test]
    fn ignores_existing_symbol_mentions() {
        // No verb prefix → no match.
        let dets = detect_new_symbols("the function foo is broken");
        assert!(dets.is_empty(), "got {dets:?}");
    }

    #[test]
    fn pairs_each_impl_with_a_test_subtask() {
        let plan = decompose(&Goal::new("add a function parse_callsites"));
        assert_eq!(plan.subtasks.len(), 2);
        assert_eq!(plan.coverage_summary.new_functions, 1);
        assert_eq!(plan.coverage_summary.tests_planned, 1);

        let impl_id = plan.subtasks[0].id;
        let test = &plan.subtasks[1];
        assert!(test.description.contains("Write integration tests"));
        assert_eq!(test.dependencies, vec![impl_id]);
    }

    #[test]
    fn no_tests_flag_suppresses_test_subtasks() {
        let mut g = Goal::new("create struct Foo");
        g.no_tests = true;
        let plan = decompose(&g);
        assert_eq!(plan.subtasks.len(), 1);
        assert_eq!(plan.coverage_summary.tests_planned, 0);
        assert_eq!(plan.coverage_summary.new_functions, 1);
    }

    #[test]
    fn fallback_when_no_symbols_detected() {
        let plan = decompose(&Goal::new("clean up the readme typos"));
        assert_eq!(plan.subtasks.len(), 1);
        assert_eq!(plan.coverage_summary.new_functions, 0);
        assert_eq!(plan.coverage_summary.tests_planned, 0);
    }

    #[test]
    fn generic_feature_goal_preserves_original_terms() {
        let goal = "add input validation to config loading";
        let dets = detect_new_symbols(goal);
        assert!(
            dets.is_empty(),
            "generic detector should not invent a symbol: {dets:?}"
        );

        let plan = decompose(&Goal::new(goal));
        assert_eq!(plan.subtasks.len(), 1);
        assert_eq!(plan.subtasks[0].description, goal);
        assert!(!plan.subtasks[0].description.contains("feature input"));
    }

    #[test]
    fn chess_goal_contract_requires_playable_artifact() {
        let contract = Goal::new("make chess").contract();
        assert!(contract
            .acceptance_criteria
            .iter()
            .any(|criterion| criterion.contains("playable chess")));
        assert!(contract
            .quality_floor
            .criteria
            .iter()
            .any(|criterion| criterion.contains("Trivial output")));
        assert!(contract
            .expected_artifacts
            .iter()
            .any(|artifact| artifact.description.contains("Playable chess")));
        assert!(contract.clarification_questions.is_empty());
    }

    #[test]
    fn html_chess_contract_is_sliced_and_runtime_verified() {
        let contract = Goal::new("make chess in html").contract();

        assert_eq!(contract.task_class, TaskClass::GeneratedAppGame);
        assert!(contract
            .expected_artifacts
            .iter()
            .any(|artifact| artifact.path.as_deref() == Some(std::path::Path::new("index.html"))));
        assert!(contract.acceptance_slices.len() >= 7);
        assert!(contract
            .acceptance_slices
            .iter()
            .any(|slice| slice.criterion.contains("render an 8x8 board")));
        assert!(contract
            .verify_plan
            .iter()
            .any(|step| step.layer == Some(phonton_types::VerifyLayer::RuntimeSmoke)));
        assert!(!contract.token_policy.allow_broad_repair);
        assert!(contract.token_policy.repair_only_missing_criteria);
    }

    #[test]
    fn generated_app_decomposes_into_sequential_acceptance_slices() {
        let plan = decompose(&Goal::new("make chess in html"));
        let contract = plan.goal_contract.as_ref().unwrap();

        assert_eq!(plan.subtasks.len(), contract.acceptance_slices.len());
        assert_eq!(plan.subtasks[0].dependencies.len(), 0);
        for pair in plan.subtasks.windows(2) {
            assert_eq!(pair[1].dependencies, vec![pair[0].id]);
        }
        assert!(plan
            .subtasks
            .iter()
            .all(|subtask| subtask.description.contains("Keep the diff minimal")));
        assert!(plan.estimated_total_tokens < plan.naive_baseline_tokens);
    }

    #[test]
    fn generic_web_app_contract_uses_sliced_runtime_plan() {
        let contract = Goal::new("create a todo web app").contract();

        assert_eq!(contract.task_class, TaskClass::GeneratedAppGame);
        assert!(contract.acceptance_slices.len() >= 4);
        assert!(contract.acceptance_slices.iter().all(
            |slice| slice.artifact_path.as_deref() == Some(std::path::Path::new("index.html"))
        ));
        assert!(!contract.token_policy.allow_broad_repair);
    }

    #[test]
    fn coverage_summary_renders_honest_signal() {
        let plan = decompose(&Goal::new("add function a and add function b"));
        assert_eq!(
            plan.coverage_summary.render(),
            "Estimated coverage: 2 new functions, 2 tests planned."
        );
    }

    #[tokio::test]
    async fn memory_preamble_injected_when_records_exist() {
        let store = Store::in_memory().unwrap();
        store
            .append_memory(&MemoryRecord::Decision {
                title: "use mpsc for parse_callsites".into(),
                body: "channels avoided lock contention".into(),
                task_id: None,
            })
            .unwrap();
        let plan =
            decompose_with_memory(&Goal::new("add a function parse_callsites"), &store, None)
                .await
                .unwrap();
        let first = &plan.subtasks[0];
        assert!(first.description.contains("Prior context from memory"));
        assert!(first.description.contains("parse_callsites"));
    }

    #[tokio::test]
    async fn generic_completion_memory_is_filtered_from_preamble() {
        let store = Store::in_memory().unwrap();
        store
            .append_memory(&MemoryRecord::Decision {
                title: "make chess".into(),
                body: "completed: make chess".into(),
                task_id: None,
            })
            .unwrap();
        let plan = decompose_with_memory(&Goal::new("make chess"), &store, None)
            .await
            .unwrap();
        assert!(!plan.subtasks[0]
            .description
            .contains("Prior context from memory"));
        assert!(!plan.subtasks[0].description.contains("completed:"));
    }

    #[tokio::test]
    async fn rejected_approaches_skip_matching_subtasks() {
        let store = Store::in_memory().unwrap();
        store
            .append_memory(&MemoryRecord::RejectedApproach {
                summary: "parse_callsites via regex scan".into(),
                reason: "too many false positives".into(),
            })
            .unwrap();
        let plan = decompose_with_memory(
            &Goal::new("add a function parse_callsites via regex scan"),
            &store,
            None,
        )
        .await
        .unwrap();
        // The impl subtask for parse_callsites (whose name appears in the
        // rejected summary) should be dropped; its paired test subtask is
        // also dropped since we retain on the same predicate.
        assert!(!plan
            .subtasks
            .iter()
            .any(|s| s.description.contains("parse_callsites")
                && s.description.contains("Implement")));
    }

    #[tokio::test]
    async fn no_memory_records_leaves_plan_unchanged() {
        let store = Store::in_memory().unwrap();
        let base = decompose(&Goal::new("add a function foo"));
        let with_mem = decompose_with_memory(&Goal::new("add a function foo"), &store, None)
            .await
            .unwrap();
        assert_eq!(base.subtasks.len(), with_mem.subtasks.len());
        // First description unchanged (no preamble).
        assert!(!with_mem.subtasks[0].description.contains("Prior context"));
    }

    #[test]
    fn classify_task_identifies_tests() {
        assert_eq!(
            classify_task("Write integration tests for parse_callsites"),
            TaskClass::TestGeneration
        );
        assert_eq!(
            classify_task("add unit-test for Provider"),
            TaskClass::TestGeneration
        );
    }

    #[test]
    fn classify_task_identifies_boilerplate_and_docs() {
        assert_eq!(
            classify_task("Rename getCwd to getCurrentWorkingDirectory"),
            TaskClass::Boilerplate
        );
        assert_eq!(
            classify_task("Update the README with the new flag"),
            TaskClass::Docs
        );
    }

    #[test]
    fn classify_task_defaults_to_core_logic() {
        assert_eq!(
            classify_task("Implement the DAG executor with backpressure"),
            TaskClass::CoreLogic
        );
    }

    #[test]
    fn effective_tier_downgrades_tests_and_boilerplate() {
        assert!(matches!(
            effective_tier(ModelTier::Frontier, TaskClass::Tests),
            ModelTier::Cheap
        ));
        assert!(matches!(
            effective_tier(ModelTier::Standard, TaskClass::Boilerplate),
            ModelTier::Cheap
        ));
        assert!(matches!(
            effective_tier(ModelTier::Frontier, TaskClass::CoreLogic),
            ModelTier::Frontier
        ));
        assert!(matches!(
            effective_tier(ModelTier::Frontier, TaskClass::GeneratedAppGame),
            ModelTier::Standard
        ));
        // Never upgrades past the planner's own floor.
        assert!(matches!(
            effective_tier(ModelTier::Local, TaskClass::Tests),
            ModelTier::Local
        ));
    }

    #[test]
    fn test_tier_steps_down_one_notch() {
        assert!(matches!(
            test_tier(ModelTier::Frontier),
            ModelTier::Standard
        ));
        assert!(matches!(test_tier(ModelTier::Standard), ModelTier::Cheap));
        assert!(matches!(test_tier(ModelTier::Cheap), ModelTier::Local));
        assert!(matches!(test_tier(ModelTier::Local), ModelTier::Local));
    }

    // -----------------------------------------------------------------
    // LLM decomposer tests
    // -----------------------------------------------------------------

    #[derive(Clone)]
    struct MockProvider {
        response: String,
    }

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        async fn call(
            &self,
            _system: &str,
            _user: &str,
            _slice_origins: &[phonton_types::SliceOrigin],
        ) -> anyhow::Result<phonton_types::LLMResponse> {
            Ok(phonton_types::LLMResponse {
                content: self.response.clone(),
                input_tokens: 0,
                output_tokens: 0,
                cached_tokens: 0,
                cache_creation_tokens: 0,
                provider: phonton_types::ProviderKind::Anthropic,
                model_name: "mock".into(),
            })
        }
        fn kind(&self) -> phonton_types::ProviderKind {
            phonton_types::ProviderKind::OpenAI
        }

        fn model(&self) -> String {
            "mock".into()
        }

        fn clone_box(&self) -> Box<dyn Provider> {
            Box::new(self.clone())
        }
    }

    #[tokio::test]
    async fn decompose_with_llm_builds_dag_from_mock_response() {
        let json = r#"[
            {"description": "implement parser", "model_tier": "Standard", "depends_on": []},
            {"description": "implement executor", "model_tier": "Standard", "depends_on": [0]},
            {"description": "write tests for executor", "model_tier": "Cheap", "depends_on": [1]}
        ]"#;
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            response: json.into(),
        });

        let plan = decompose_with_llm("build a thing", provider, "")
            .await
            .unwrap();
        assert_eq!(plan.subtasks.len(), 3);

        // Sanity on tiers.
        assert!(matches!(plan.subtasks[0].model_tier, ModelTier::Standard));
        assert!(matches!(plan.subtasks[2].model_tier, ModelTier::Cheap));

        // Root has no deps.
        assert!(plan.subtasks[0].dependencies.is_empty());
        // Subtask 1 depends on subtask 0 by id.
        assert_eq!(plan.subtasks[1].dependencies, vec![plan.subtasks[0].id]);
        // Subtask 2 depends on subtask 1 by id.
        assert_eq!(plan.subtasks[2].dependencies, vec![plan.subtasks[1].id]);

        // Coverage: one test subtask identified by Cheap tier + "test" word.
        assert_eq!(plan.coverage_summary.tests_planned, 1);
        assert_eq!(plan.coverage_summary.new_functions, 2);
    }

    #[tokio::test]
    async fn decompose_with_llm_falls_back_on_garbage() {
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            response: "not json at all".into(),
        });
        let plan = decompose_with_llm("add a function foo", provider, "")
            .await
            .unwrap();
        // Regex fallback emits impl + paired test for a single detection.
        assert_eq!(plan.subtasks.len(), 2);
    }

    #[tokio::test]
    async fn decompose_with_llm_falls_back_on_cycle() {
        let cyclic = r#"[
            {"description": "a", "model_tier": "Standard", "depends_on": [1]},
            {"description": "b", "model_tier": "Standard", "depends_on": [0]}
        ]"#;
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            response: cyclic.into(),
        });
        let plan = decompose_with_llm("add a function foo", provider, "")
            .await
            .unwrap();
        // Should have fallen back to regex — which emits 2 subtasks for
        // "add a function foo" (impl + test).
        assert_eq!(plan.subtasks.len(), 2);
    }
}
