//! Shared domain types — imported by all phonton crates. No business logic.
//!
//! Rule: if a type crosses a crate boundary, it lives here. Nothing else does.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod events;
pub mod extensions;
pub mod messages;
pub mod providers;

pub use events::{EventRecord, OrchestratorEvent, TOKEN_MILESTONE_INTERVAL};
pub use extensions::{
    AppliesTo, ExtensionAction, ExtensionConflict, ExtensionId, ExtensionInfluence, ExtensionKind,
    ExtensionManifest, ExtensionScope, ExtensionSource, McpServerDefinition, McpTransport,
    Permission, ProfileDefinition, SkillDefinition, SteeringRule, SteeringSeverity, TrustLevel,
};
pub use messages::{GlobalState, OrchestratorMessage, WorkerMessage, WorkerState};
pub use providers::{
    BudgetDecision, BudgetLimits, CostSummary, LLMResponse, ModelMetricsSnapshot, ModelPricing,
    ProviderConfig, ProviderError, ProviderKind, TokenUsage,
};

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Unique identifier for a top-level user task.
///
/// A task is the unit the user creates from the UI; it decomposes into one or
/// more [`SubtaskId`]s via the planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub Uuid);

impl TaskId {
    /// Generate a fresh random `TaskId` (UUIDv4).
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Unique identifier for a subtask within a task's DAG.
///
/// Subtasks are produced by the planner and consumed by the orchestrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubtaskId(pub Uuid);

impl SubtaskId {
    /// Generate a fresh random `SubtaskId` (UUIDv4).
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SubtaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SubtaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// ---------------------------------------------------------------------------
// Model tiers
// ---------------------------------------------------------------------------

/// Cost/capability tier assigned to each subtask by the planner.
///
/// The planner's job is to route trivial work to cheap models and reserve
/// frontier models for genuinely complex subtasks. This enum is the vocabulary
/// of that routing decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelTier {
    /// Local model via Ollama — zero API cost. Rename, format, derive.
    Local,
    /// Cheap API model (e.g. Haiku). Add a field, single-function edit.
    Cheap,
    /// Standard API model (e.g. Sonnet). New function, refactor a module.
    Standard,
    /// Frontier API model (e.g. Opus). Cross-crate refactor, architecture.
    Frontier,
}

/// Workload class assigned to a subtask, used by the orchestrator to decide
/// whether a planner-chosen tier should be auto-downgraded.
///
/// Classification is cheap and happens at dispatch time by running a small
/// keyword sweep over the subtask description (see
/// `phonton_planner::classify_task`). This is the pivot from static tiers
/// to dynamic, cost-aware routing: `Boilerplate` and `Tests` are pushed
/// down to `Cheap`; `CoreLogic` is left alone so frontier models stay
/// reserved for the work that actually needs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskClass {
    /// Generated docs, stubs, type aliases, trivial wiring.
    #[serde(alias = "Boilerplate")]
    Boilerplate,
    /// Unit/integration tests. Routine output, cheap models suffice.
    #[serde(alias = "Tests")]
    Tests,
    /// Documentation prose.
    #[serde(alias = "Docs")]
    Docs,
    /// Novel algorithmic or architectural work — the one tier that still
    /// justifies a frontier model.
    #[serde(alias = "CoreLogic")]
    CoreLogic,
    /// A defect fix with an existing expected behavior.
    #[serde(alias = "BugFix")]
    BugFix,
    /// A feature added to an existing project surface.
    #[serde(alias = "ExistingProjectFeature")]
    ExistingProjectFeature,
    /// New tests or test harness work.
    #[serde(alias = "TestGeneration")]
    TestGeneration,
    /// Behavior-preserving code restructuring.
    #[serde(alias = "Refactor")]
    Refactor,
    /// A generated application, game, or broad greenfield artifact.
    #[serde(alias = "GeneratedAppGame")]
    GeneratedAppGame,
    /// Release validation, packaging, tagging, or publishing checks.
    #[serde(alias = "ReleaseCheck")]
    ReleaseCheck,
}

impl fmt::Display for TaskClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            TaskClass::Boilerplate => "boilerplate",
            TaskClass::Tests => "tests",
            TaskClass::Docs => "docs",
            TaskClass::CoreLogic => "core-logic",
            TaskClass::BugFix => "bug_fix",
            TaskClass::ExistingProjectFeature => "existing_project_feature",
            TaskClass::TestGeneration => "test_generation",
            TaskClass::Refactor => "refactor",
            TaskClass::GeneratedAppGame => "generated_app_game",
            TaskClass::ReleaseCheck => "release_check",
        };
        f.write_str(s)
    }
}

/// Classify a subtask description into a [`TaskClass`].
///
/// The orchestrator consults this at dispatch time to decide whether to
/// auto-downgrade the planner-chosen tier — `Boilerplate` and `Tests`
/// descriptions get pushed to `ModelTier::Cheap` regardless of what the
/// planner assigned, while `CoreLogic` stays at the planner's tier.
///
/// Heuristic-only, keyword-based. Lives here (not in `phonton-planner`)
/// so the orchestrator can call it without pulling in the planner crate
/// and its provider/memory dependencies.
pub fn classify_task(description: &str) -> TaskClass {
    classify_intent(description).task_class
}

/// Coarse ambiguity estimate for a user goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AmbiguityLevel {
    /// The goal names a clear target and expected behavior.
    Low,
    /// The goal is actionable but leaves meaningful choices.
    Medium,
    /// The goal is too vague to execute safely without a question.
    High,
}

/// Estimated blast radius of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BlastRadius {
    /// One file or artifact.
    File,
    /// One crate/package/module.
    Crate,
    /// Multiple crates/packages or workspace-level behavior.
    Workspace,
    /// Release, publishing, or repository state outside normal edits.
    Release,
}

/// Runtime proof required by the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RuntimeRisk {
    /// No runtime proof expected.
    None,
    /// Basic command/build execution is enough.
    Low,
    /// Runtime behavior should be exercised.
    Medium,
    /// Browser/game/app behavior must be checked before verified success.
    High,
}

/// Token waste risk before execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TokenRisk {
    /// Small prompt and repair surface.
    Low,
    /// Bounded but non-trivial context or repair risk.
    Medium,
    /// Broad or ambiguous work that can burn tokens without verified value.
    High,
}

/// Planner action selected by intent classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IntentAction {
    /// Ask the user a question before planning execution.
    AskClarifyingQuestion,
    /// Show a plan/contract before dispatch.
    PreviewPlan,
    /// Safe to dispatch directly when the user asks to run.
    ExecuteDirectTask,
    /// Refuse or block because the request is unsafe or too underspecified.
    RefuseUnsafeOrUnderspecified,
}

/// Structured preflight classification for a top-level goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentClassification {
    /// Benchmark/workload class.
    pub task_class: TaskClass,
    /// Confidence as 0-100 to keep ledgers stable across platforms.
    pub confidence_percent: u8,
    /// How much clarification the request still needs.
    pub ambiguity: AmbiguityLevel,
    /// Estimated code/release surface touched.
    pub blast_radius: BlastRadius,
    /// Runtime proof risk.
    pub runtime_risk: RuntimeRisk,
    /// Token waste risk.
    pub token_risk: TokenRisk,
    /// Recommended next action before worker dispatch.
    pub recommended_action: IntentAction,
}

/// Build a structured intent classification for a goal or subtask.
pub fn classify_intent(description: &str) -> IntentClassification {
    let d = description.to_ascii_lowercase();
    let word_count = d.split_whitespace().count();

    if word_count <= 2 && !d.contains("chess") {
        return IntentClassification {
            task_class: TaskClass::CoreLogic,
            confidence_percent: 45,
            ambiguity: AmbiguityLevel::High,
            blast_radius: BlastRadius::Workspace,
            runtime_risk: RuntimeRisk::Medium,
            token_risk: TokenRisk::High,
            recommended_action: IntentAction::AskClarifyingQuestion,
        };
    }

    if contains_any(&d, &["delete all", "wipe", "exfiltrate", "steal token"]) {
        return IntentClassification {
            task_class: TaskClass::CoreLogic,
            confidence_percent: 70,
            ambiguity: AmbiguityLevel::High,
            blast_radius: BlastRadius::Workspace,
            runtime_risk: RuntimeRisk::High,
            token_risk: TokenRisk::High,
            recommended_action: IntentAction::RefuseUnsafeOrUnderspecified,
        };
    }

    if is_generated_app_goal(&d) {
        return IntentClassification {
            task_class: TaskClass::GeneratedAppGame,
            confidence_percent: 85,
            ambiguity: AmbiguityLevel::Medium,
            blast_radius: BlastRadius::File,
            runtime_risk: RuntimeRisk::High,
            token_risk: TokenRisk::High,
            recommended_action: IntentAction::PreviewPlan,
        };
    }

    if contains_any(
        &d,
        &[
            "release",
            "publish",
            "tag",
            "npm pack",
            "cargo publish",
            "changelog",
            "release notes",
        ],
    ) {
        return IntentClassification {
            task_class: TaskClass::ReleaseCheck,
            confidence_percent: 82,
            ambiguity: AmbiguityLevel::Medium,
            blast_radius: BlastRadius::Release,
            runtime_risk: RuntimeRisk::Medium,
            token_risk: TokenRisk::Medium,
            recommended_action: IntentAction::PreviewPlan,
        };
    }

    if contains_any(
        &d,
        &["fix", "bug", "failing", "failure", "regression", "panic"],
    ) {
        return IntentClassification {
            task_class: TaskClass::BugFix,
            confidence_percent: 80,
            ambiguity: AmbiguityLevel::Medium,
            blast_radius: BlastRadius::Crate,
            runtime_risk: RuntimeRisk::Medium,
            token_risk: TokenRisk::Medium,
            recommended_action: IntentAction::PreviewPlan,
        };
    }

    if d.contains("test") || d.contains("unit-test") || d.contains("integration test") {
        return IntentClassification {
            task_class: TaskClass::TestGeneration,
            confidence_percent: 86,
            ambiguity: AmbiguityLevel::Low,
            blast_radius: BlastRadius::Crate,
            runtime_risk: RuntimeRisk::Low,
            token_risk: TokenRisk::Low,
            recommended_action: IntentAction::ExecuteDirectTask,
        };
    }

    if d.contains("docstring")
        || d.contains("doc comment")
        || d.contains("readme")
        || d.contains("markdown")
        || d.contains("changelog")
    {
        return IntentClassification {
            task_class: TaskClass::Docs,
            confidence_percent: 88,
            ambiguity: AmbiguityLevel::Low,
            blast_radius: BlastRadius::File,
            runtime_risk: RuntimeRisk::None,
            token_risk: TokenRisk::Low,
            recommended_action: IntentAction::ExecuteDirectTask,
        };
    }

    if d.contains("rename")
        || d.contains("format")
        || d.contains("derive ")
        || d.contains("getter")
        || d.contains("setter")
        || d.contains("re-export")
        || d.contains("reexport")
        || d.contains("add field")
        || d.contains("add a field")
    {
        return IntentClassification {
            task_class: TaskClass::Boilerplate,
            confidence_percent: 84,
            ambiguity: AmbiguityLevel::Low,
            blast_radius: BlastRadius::File,
            runtime_risk: RuntimeRisk::Low,
            token_risk: TokenRisk::Low,
            recommended_action: IntentAction::ExecuteDirectTask,
        };
    }

    if contains_any(&d, &["refactor", "restructure", "rename module", "extract"]) {
        return IntentClassification {
            task_class: TaskClass::Refactor,
            confidence_percent: 78,
            ambiguity: AmbiguityLevel::Medium,
            blast_radius: BlastRadius::Workspace,
            runtime_risk: RuntimeRisk::Medium,
            token_risk: TokenRisk::Medium,
            recommended_action: IntentAction::PreviewPlan,
        };
    }

    if contains_any(&d, &["dag", "executor", "backpressure", "algorithm"]) {
        return IntentClassification {
            task_class: TaskClass::CoreLogic,
            confidence_percent: 74,
            ambiguity: AmbiguityLevel::Medium,
            blast_radius: BlastRadius::Workspace,
            runtime_risk: RuntimeRisk::Medium,
            token_risk: TokenRisk::Medium,
            recommended_action: IntentAction::PreviewPlan,
        };
    }

    if contains_any(&d, &["add", "implement", "support", "feature", "create"]) {
        return IntentClassification {
            task_class: TaskClass::ExistingProjectFeature,
            confidence_percent: 76,
            ambiguity: AmbiguityLevel::Medium,
            blast_radius: BlastRadius::Crate,
            runtime_risk: RuntimeRisk::Medium,
            token_risk: TokenRisk::Medium,
            recommended_action: IntentAction::PreviewPlan,
        };
    }

    IntentClassification {
        task_class: TaskClass::CoreLogic,
        confidence_percent: 70,
        ambiguity: AmbiguityLevel::Medium,
        blast_radius: BlastRadius::Workspace,
        runtime_risk: RuntimeRisk::Medium,
        token_risk: TokenRisk::Medium,
        recommended_action: IntentAction::PreviewPlan,
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn is_generated_app_goal(description: &str) -> bool {
    let has_build_verb = contains_any(description, &["make", "build", "create", "generate"]);
    let has_generated_target = contains_any(
        description,
        &[
            " app",
            "game",
            "chess",
            "html",
            "website",
            "web page",
            "single page",
        ],
    );
    let generated_acceptance_slice =
        description.contains("acceptance slice") && has_generated_target;
    (has_build_verb && has_generated_target) || generated_acceptance_slice
}

/// The effective tier to dispatch a subtask at, given its planner-assigned
/// tier and its classified workload. Core logic keeps its tier; boilerplate,
/// tests, and docs are floored at `Cheap`. Generated app/game work is capped
/// at `Standard` because it should be decomposed into acceptance slices before
/// dispatch instead of spending frontier tokens on one broad attempt.
///
/// The cost-aware half of the orchestrator's routing decision. The
/// latency-aware half (driven by `phonton_providers::ModelMetrics`) lives
/// alongside and can still escalate on repeated verify failures via the
/// existing `escalate` path.
pub fn effective_tier(planned: ModelTier, class: TaskClass) -> ModelTier {
    match class {
        TaskClass::CoreLogic
        | TaskClass::BugFix
        | TaskClass::ExistingProjectFeature
        | TaskClass::Refactor
        | TaskClass::ReleaseCheck => planned,
        TaskClass::GeneratedAppGame => match planned {
            ModelTier::Frontier => ModelTier::Standard,
            other => other,
        },
        TaskClass::Boilerplate | TaskClass::Tests | TaskClass::Docs | TaskClass::TestGeneration => {
            match planned {
                ModelTier::Local | ModelTier::Cheap => planned,
                ModelTier::Standard | ModelTier::Frontier => ModelTier::Cheap,
            }
        }
    }
}

impl fmt::Display for ModelTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ModelTier::Local => "local",
            ModelTier::Cheap => "cheap",
            ModelTier::Standard => "standard",
            ModelTier::Frontier => "frontier",
        };
        f.write_str(s)
    }
}

// ---------------------------------------------------------------------------
// Status machines
// ---------------------------------------------------------------------------

/// Lifecycle state of a top-level task.
///
/// Transitions:
/// `Queued → Planning → Running → Reviewing → Done`, with `Failed` and
/// `Rejected` as terminal escapes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskStatus {
    /// Waiting for the orchestrator to pick it up.
    Queued,
    /// Planner is decomposing the task into a subtask DAG.
    Planning,
    /// Workers are executing subtasks.
    Running {
        /// IDs of subtasks currently in flight.
        active_subtasks: Vec<SubtaskId>,
        /// Count of subtasks that have finished (Done or Failed).
        completed: usize,
        /// Total subtasks in the DAG.
        total: usize,
    },
    /// All subtasks finished; diffs assembled and awaiting user review.
    Reviewing {
        /// Actual tokens consumed end-to-end for this task.
        tokens_used: u64,
        /// Estimated tokens a naive (non-Phonton) run would have burned,
        /// used to display the savings claim in the UI.
        estimated_savings_tokens: u64,
    },
    /// Task completed and the diff was accepted.
    Done {
        /// Actual tokens consumed end-to-end.
        tokens_used: u64,
        /// Wall-clock duration from Planning through Reviewing in milliseconds.
        wall_time_ms: u64,
    },
    /// Task aborted due to an unrecoverable error.
    Failed {
        /// Human-readable failure reason.
        reason: String,
        /// Specific subtask that caused the failure, if localised.
        failed_subtask: Option<SubtaskId>,
    },
    /// Planning found unanswered contract questions that should be resolved
    /// before workers spend provider tokens or edit files.
    NeedsClarification {
        /// Questions the user should answer before resubmitting the goal.
        questions: Vec<String>,
    },
    /// Run halted because a `BudgetGuard` ceiling was crossed. Not terminal —
    Paused {
        /// Which limit tripped — `"tokens"` or `"usd"`.
        limit: String,
        /// Observed value at the time of the pause.
        observed: u64,
        /// Configured ceiling that was crossed.
        ceiling: u64,
    },
    /// User rejected the produced diff. Task is terminal.
    Rejected,
}

// ---------------------------------------------------------------------------
// Session snapshots
// ---------------------------------------------------------------------------

/// Persisted snapshot of one interactive CLI session for a workspace.
///
/// This is the durable "remember" surface for resuming the local ADE loop.
/// It contains only review-safe UI state and typed task evidence, not private
/// terminal handles or provider credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// Canonical workspace key this snapshot belongs to.
    pub workspace: String,
    /// Unix timestamp in seconds when the snapshot was saved.
    pub saved_at: u64,
    /// Selected goal index when the session ended.
    pub selected_goal: usize,
    /// Draft goal text that had not been submitted yet.
    pub goal_input: String,
    /// Draft Ask-mode text.
    pub ask_input: String,
    /// Last Ask-mode answer shown in the side panel.
    pub ask_answer: Option<String>,
    /// Recent submitted prompt history, newest last, for Up/Down recall after resume.
    #[serde(default)]
    pub prompt_history: Vec<String>,
    /// Highest observed savings percentage for this session.
    pub best_savings_pct: Option<i64>,
    /// Top-level goals visible in the TUI.
    pub goals: Vec<SessionGoalSnapshot>,
    /// Precomputed receipt totals for fast exit display and later inspection.
    pub totals: SessionTotals,
}

/// Persisted view of one top-level goal in a [`SessionSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGoalSnapshot {
    /// Original goal text.
    pub description: String,
    /// Last known lifecycle status.
    pub status: TaskStatus,
    /// Last broadcast task state when available.
    pub state: Option<GlobalState>,
    /// Stable task id used to correlate history and Flight Log events.
    pub task_id: TaskId,
    /// Flight Log events observed for the goal.
    pub flight_log: Vec<EventRecord>,
}

/// Token and lifecycle totals shown when a saved session exits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTotals {
    /// Number of top-level goals in the session snapshot.
    pub goals: usize,
    /// Number of goals that reached `Done`.
    pub completed: usize,
    /// Number of goals that reached `Failed`.
    pub failed: usize,
    /// Number of goals awaiting review.
    pub reviewing: usize,
    /// Total actual tokens used across visible goals.
    pub tokens_used: u64,
    /// Total estimated naive baseline tokens across visible goals.
    pub naive_baseline_tokens: u64,
    /// Estimated token delta versus the naive baseline.
    ///
    /// Positive values mean estimated tokens saved. Negative values mean the
    /// session used more tokens than the baseline estimate.
    pub estimated_tokens_saved: i64,
    /// Best observed savings percentage in the session.
    pub best_savings_pct: Option<i64>,
}

/// Lifecycle state of a single subtask inside a task's DAG.
///
/// Transitions:
/// `Queued → Ready → Dispatched → Running → Done`, with `Failed` as escape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubtaskStatus {
    /// Created by the planner but dependencies are not yet satisfied.
    Queued,
    /// All dependencies satisfied; eligible to be dispatched to a worker.
    Ready,
    /// Assigned to a worker but the worker has not yet made its first call.
    Dispatched,
    /// Worker is executing the LLM call loop.
    Running {
        /// Tier actually assigned — recorded in case the planner reroutes.
        model_tier: ModelTier,
        /// Running token count for this subtask.
        tokens_so_far: u64,
    },
    /// Subtask finished successfully and produced diff hunks.
    Done {
        /// Total tokens consumed by this subtask (input + output).
        tokens_used: u64,
        /// Number of diff hunks the worker produced.
        diff_hunk_count: usize,
    },
    /// Subtask failed. May be retried depending on orchestrator policy.
    Failed {
        /// Human-readable failure reason.
        reason: String,
        /// Retry attempt number, starting at 1 for the first failure.
        attempt: u8,
    },
}

// ---------------------------------------------------------------------------
// Diff primitives
// ---------------------------------------------------------------------------

/// A single line inside a [`DiffHunk`].
///
/// Workers emit these directly; Phonton never round-trips through full-file
/// content, so this is the narrowest representation of a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffLine {
    /// Unchanged context line, preserved for patch application.
    Context(String),
    /// Line added by the worker.
    Added(String),
    /// Line removed by the worker.
    Removed(String),
}

/// A contiguous change region inside a single file, in unified-diff form.
///
/// Corresponds 1:1 to a `@@` hunk header in a `git diff` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    /// Path relative to the workspace root.
    pub file_path: PathBuf,
    /// Starting line number in the original file (1-indexed).
    pub old_start: u32,
    /// Number of lines from the original file covered by this hunk.
    pub old_count: u32,
    /// Starting line number in the new file (1-indexed).
    pub new_start: u32,
    /// Number of lines in the new file produced by this hunk.
    pub new_count: u32,
    /// The ordered sequence of context/added/removed lines.
    pub lines: Vec<DiffLine>,
}

// ---------------------------------------------------------------------------
// Semantic retrieval
// ---------------------------------------------------------------------------

/// A single symbol-level slice of source code returned by the index.
///
/// Slices are the unit of context that workers actually see — never whole
/// files. Every field here is chosen to be useful to an LLM and cheap to
/// tokenise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeSlice {
    /// Path of the source file this symbol lives in.
    pub file_path: PathBuf,
    /// Fully qualified symbol name (e.g. `module::Type::method`).
    pub symbol_name: String,
    /// Signature line as written in source, without the body.
    pub signature: String,
    /// Docstring/comment block immediately preceding the symbol, if any.
    pub docstring: Option<String>,
    /// Fully qualified names of call sites that reference this symbol.
    pub callsites: Vec<String>,
    /// Pre-computed token count for budget accounting. `0` for fallback
    /// slices where the parser couldn't establish symbol boundaries.
    pub token_count: usize,
    /// How this slice was produced — semantic parse vs heuristic fallback.
    /// The planner uses this to widen context budget when slices are fallback.
    pub origin: SliceOrigin,
}

/// Small, review-safe summary of a context slice selected for a subtask.
///
/// Unlike [`CodeSlice`], this is meant for telemetry and review output:
/// it records *what* influenced the worker without persisting full source
/// snippets into every event row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextAttribution {
    /// Source file that provided context.
    pub file_path: PathBuf,
    /// Symbol chosen from that file.
    pub symbol_name: String,
    /// How the slice was produced.
    pub origin: SliceOrigin,
    /// Token count reported by the index, or `0` when unknown.
    pub token_count: usize,
}

impl From<&CodeSlice> for ContextAttribution {
    fn from(slice: &CodeSlice) -> Self {
        Self {
            file_path: slice.file_path.clone(),
            symbol_name: slice.symbol_name.clone(),
            origin: slice.origin,
            token_count: slice.token_count,
        }
    }
}

/// A planned context source category for one provider call.
///
/// This is different from [`ContextSource`]: `ContextPlanItem` is built before
/// the model call and records what Phonton intended to include, omit, or cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContextPlanKind {
    /// The current user goal or subtask.
    Goal,
    /// Contract or quality-floor text.
    Contract,
    /// Compact file/symbol orientation for the repository.
    RepoMap,
    /// Concrete symbol or file slice selected for the worker.
    CodeSlice,
    /// Persistent memory selected for this task.
    Memory,
    /// User-pasted text, images, or mentioned files.
    Attachment,
    /// Compact verifier/provider diagnostics for a repair attempt.
    RetryDiagnostic,
    /// MCP/tool instructions or tool result context.
    Tool,
}

/// One planned context item, included or omitted, for a provider call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPlanItem {
    /// Source category.
    pub kind: ContextPlanKind,
    /// Stable source id, usually a path, symbol name, or synthetic bucket id.
    pub id: String,
    /// Review-safe summary of why this item matters.
    pub summary: String,
    /// Estimated token cost for this item.
    pub estimated_tokens: u64,
    /// True when the item is included in the provider prompt.
    pub included: bool,
    /// Short reason for inclusion or omission.
    pub reason: String,
}

/// Bounded context plan for a single worker/provider call.
///
/// `PromptContextManifest` records the final token buckets after rendering.
/// `ContextPlan` records the budget decision that led to those buckets. This
/// makes token efficiency inspectable instead of relying on hidden heuristics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPlan {
    /// Hard context limit when known.
    #[serde(default)]
    pub budget_limit: Option<u64>,
    /// Target prompt budget chosen by Phonton's context compiler.
    #[serde(default)]
    pub target_tokens: u64,
    /// Estimated tokens from fixed prompt sections before code context.
    #[serde(default)]
    pub fixed_tokens: u64,
    /// Estimated tokens spent on compact repository map lines.
    #[serde(default)]
    pub repo_map_tokens: u64,
    /// Estimated tokens spent on selected code context.
    #[serde(default)]
    pub selected_code_tokens: u64,
    /// Estimated candidate code tokens intentionally omitted.
    #[serde(default)]
    pub omitted_code_tokens: u64,
    /// Total prompt estimate after the context decision.
    #[serde(default)]
    pub estimated_total_tokens: u64,
    /// True when required context forced the estimate above the selected target.
    #[serde(default)]
    pub target_exceeded: bool,
    /// Estimated tokens above the selected target when [`Self::target_exceeded`] is true.
    #[serde(default)]
    pub over_target_tokens: u64,
    /// Included and omitted context items.
    #[serde(default)]
    pub items: Vec<ContextPlanItem>,
}

/// Provenance of a [`CodeSlice`].
///
/// Recorded so downstream consumers (planner, worker) can reason about
/// confidence and budget. See `01-architecture/failure-modes.md` Risk 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SliceOrigin {
    /// Produced by a successful tree-sitter parse — precise boundaries.
    Semantic,
    /// Produced by heuristic line-based extraction because the parser
    /// failed or the language is not in the supported tier.
    Fallback,
}

/// A natural-language query issued against the semantic index.
///
/// Returned result is a ranked `Vec<CodeSlice>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SliceQuery {
    /// Free-form natural language description of what the caller needs.
    pub description: String,
    /// Maximum number of slices to return.
    pub top_k: usize,
    /// Optional language restriction (e.g. `"rust"`, `"python"`).
    pub language_filter: Option<String>,
}

// ---------------------------------------------------------------------------
// Context window
// ---------------------------------------------------------------------------

/// A single frame inside a worker's context window.
///
/// The context compressor evicts and summarises `Summarizable` frames with
/// the lowest priority first; `Verbatim` frames are pinned forever.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextFrame {
    /// Never compressed, never evicted — e.g. system prompt, task spec.
    Verbatim(String),
    /// Eligible for compression / eviction when the window fills.
    Summarizable {
        /// The raw frame content.
        content: String,
        /// Priority from 1 (evict first) to 10 (evict last).
        priority: u8,
    },
}

// ---------------------------------------------------------------------------
// Prompt attachments
// ---------------------------------------------------------------------------

/// Attachment kind parsed from a goal prompt mention such as `@README.md` or
/// `@screenshots/failure.png`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PromptAttachmentKind {
    /// UTF-8-ish text content that can be inlined into the prompt.
    Text,
    /// Raster/vector image content. Providers with vision support may receive
    /// the base64 payload; text-only providers receive metadata only.
    Image,
    /// Mention was recognized but could not be safely attached.
    Unsupported,
}

/// A user-mentioned file carried alongside a goal or subtask.
///
/// Attachments are local-first: paths are resolved by the CLI and constrained
/// to the workspace before they enter planner/worker context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptAttachment {
    /// Path as displayed to the model/user, usually relative to the workspace.
    pub path: PathBuf,
    /// Attachment category.
    pub kind: PromptAttachmentKind,
    /// Best-effort MIME type inferred by the CLI.
    pub mime_type: Option<String>,
    /// File size in bytes when known.
    pub size_bytes: u64,
    /// UTF-8 text payload for text attachments.
    pub text: Option<String>,
    /// Base64 payload for image attachments small enough to carry.
    pub data_base64: Option<String>,
    /// True when text or image bytes were truncated or omitted due to size.
    pub truncated: bool,
    /// User-visible note for skipped, truncated, or metadata-only attachments.
    pub note: Option<String>,
}

impl PromptAttachment {
    /// True when this attachment can be sent as image input to a vision-capable
    /// provider.
    pub fn has_image_payload(&self) -> bool {
        self.kind == PromptAttachmentKind::Image
            && self.mime_type.is_some()
            && self.data_base64.is_some()
    }
}

/// Render prompt attachments as deterministic text context.
///
/// Text files are inlined. Images are described by path/MIME/size and may also
/// be sent as provider-native image parts by `phonton-providers`.
pub fn render_prompt_attachments(attachments: &[PromptAttachment]) -> String {
    if attachments.is_empty() {
        return String::new();
    }
    let mut out = String::from("# Mentioned files\n");
    for attachment in attachments {
        let kind = match attachment.kind {
            PromptAttachmentKind::Text => "text",
            PromptAttachmentKind::Image => "image",
            PromptAttachmentKind::Unsupported => "unsupported",
        };
        let mime = attachment.mime_type.as_deref().unwrap_or("unknown");
        out.push_str(&format!(
            "## {} ({kind}, {mime}, {} bytes)\n",
            attachment.path.display(),
            attachment.size_bytes
        ));
        if let Some(note) = &attachment.note {
            out.push_str("note: ");
            out.push_str(note);
            out.push('\n');
        }
        if let Some(text) = &attachment.text {
            out.push_str("<file-content>\n");
            out.push_str(text);
            if !text.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("</file-content>\n");
        } else if attachment.kind == PromptAttachmentKind::Image {
            out.push_str(
                "image attachment: use the provider-native image payload when available; otherwise treat this as image metadata only.\n",
            );
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// A single node in the subtask DAG produced by `phonton-planner`.
///
/// `dependencies` must reference earlier `Subtask::id`s in the same
/// [`PlannerOutput::subtasks`] list; the orchestrator walks the DAG
/// topologically and dispatches subtasks whose dependencies are all `Done`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subtask {
    /// Stable ID for this subtask.
    pub id: SubtaskId,
    /// Natural-language description of what the worker must do.
    pub description: String,
    /// Tier assigned by the planner. May be escalated on retry.
    pub model_tier: ModelTier,
    /// IDs of subtasks that must reach `Done` before this one is `Ready`.
    pub dependencies: Vec<SubtaskId>,
    /// User-mentioned files/images inherited from the top-level goal.
    #[serde(default)]
    pub attachments: Vec<PromptAttachment>,
    /// Current lifecycle state.
    pub status: SubtaskStatus,
}

/// Output of a single worker run, returned to the orchestrator via
/// [`OrchestratorMessage::SubtaskDone`].
///
/// Read token count via the `status` field:
/// `SubtaskStatus::Done { tokens_used, .. }`. It is not duplicated here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtaskResult {
    /// Subtask this result belongs to.
    pub id: SubtaskId,
    /// Terminal status — `Done` on success, `Failed` otherwise.
    /// Token accounting lives inside this variant's payload.
    pub status: SubtaskStatus,
    /// Diff hunks produced by the worker. May be empty on `Failed`.
    pub diff_hunks: Vec<DiffHunk>,
    /// Tier actually used — recorded in case the planner's choice was
    /// overridden by retry-time escalation.
    pub model_tier: ModelTier,
    /// Verification verdict for the produced diff.
    pub verify_result: VerifyResult,
    /// Provider that served the final LLM call. Used by `BudgetGuard` to
    /// price the call against the registered pricing table.
    pub provider: ProviderKind,
    /// Model name as reported by the provider (e.g. `claude-haiku-4-5-20251001`).
    /// Empty string when unknown (e.g. stub dispatcher).
    pub model_name: String,
    /// Provider-reported token usage split by input/output/cache buckets.
    pub token_usage: TokenUsage,
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// Which layer of the layered verification pipeline ran.
///
/// Layers escalate in cost: `Syntax` is ~50ms tree-sitter parsing,
/// `CrateCheck` is a single-package `cargo check`, `WorkspaceCheck` is a
/// full-workspace check, and `Test` runs `cargo test`. See
/// `01-architecture/failure-modes.md` Risk 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VerifyLayer {
    /// Tree-sitter parse of the diff. Cheapest, runs first.
    Syntax,
    /// Layer 1.5 — diff is checked against `phonton-memory` decisions,
    /// constraints, and rejected approaches. No subprocess; runs against
    /// the in-memory record set. A failure here surfaces the offending
    /// record's text as the error context so the worker (and the user)
    /// see *why* the diff was rejected, not just that it was. The
    /// environment doesn't only remember — it enforces.
    DecisionCheck,
    /// `cargo check --package <crate>` on the affected crate only.
    CrateCheck,
    /// `cargo check --workspace` — only when public types/APIs change.
    WorkspaceCheck,
    /// `cargo test` — never automatic; user-triggered.
    Test,
    /// Launch or execute the generated artifact enough to catch runtime errors.
    RuntimeSmoke,
    /// Browser DOM assertion for generated web artifacts.
    BrowserDomCheck,
    /// Browser/user interaction assertion for generated web artifacts.
    InteractionCheck,
}

/// Outcome of running `phonton-verify` over a worker's diff hunks.
///
/// `Pass` lets the orchestrator advance the subtask to `Done`. `Fail` is
/// retryable: the orchestrator may re-dispatch with a stronger model tier
/// while `attempt` is below the policy ceiling. `Escalate` is terminal for
/// the verification loop and surfaces to the user as a hard stop.
///
/// Both `Pass` and `Fail` carry the [`VerifyLayer`] that produced them so
/// the UI can attribute the verdict ("syntax error" vs "type error") with
/// precision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerifyResult {
    /// Diff passed every configured layer up to and including `layer`.
    Pass {
        /// The deepest layer that ran successfully.
        layer: VerifyLayer,
    },
    /// Diff failed verification at `layer`. May be retried.
    Fail {
        /// Layer that produced the failure.
        layer: VerifyLayer,
        /// One human-readable error per failed check.
        errors: Vec<String>,
        /// 1-indexed retry attempt that produced this failure.
        attempt: u8,
    },
    /// Verification cannot proceed and human attention is required.
    Escalate {
        /// Why the loop is bailing out (e.g. retry budget exhausted).
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Checkpoints (point-in-time recovery)
// ---------------------------------------------------------------------------

/// A point-in-time recovery marker created by the orchestrator after each
/// subtask passes verify and its diff is applied.
///
/// Each checkpoint corresponds to a real `git` commit (created by
/// `phonton-diff`) on a side ref under `refs/phonton/checkpoints/<task>/<seq>`,
/// so HEAD's user-visible history isn't polluted, but the worktree state
/// at the moment the subtask landed is reproducible.
///
/// The `seq` field is monotonically increasing within a task and is the
/// stable handle the UI uses for "Rollback to subtask N" — we don't ask
/// the user to copy git OIDs around.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Task this checkpoint belongs to.
    pub task_id: TaskId,
    /// Subtask whose successful completion produced the checkpoint.
    pub subtask_id: SubtaskId,
    /// Monotonic sequence within the task — `1` for the first
    /// subtask to land, `2` for the next, and so on. The sequence is
    /// the user-facing handle for rollback ("Rollback to step 3").
    pub seq: u32,
    /// Git commit OID as a hex string.
    pub commit_oid: String,
    /// Short human-readable description (typically the subtask
    /// description, truncated). Used in the CLI rollback picker.
    pub message: String,
    /// Wall-clock instant the checkpoint was taken, unix-epoch ms.
    pub timestamp_ms: u64,
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// A single record in Phonton's local decision/convention memory.
///
/// Memory is consulted by the planner and workers to keep behaviour coherent
/// across sessions without re-deriving project conventions from scratch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryRecord {
    /// A concrete decision that was made and should be honoured going forward.
    Decision {
        /// Short title used for retrieval ranking.
        title: String,
        /// Full rationale and resolution.
        body: String,
        /// Originating task, if known.
        task_id: Option<TaskId>,
    },
    /// A hard constraint the codebase or environment imposes.
    Constraint {
        /// What is constrained (e.g. `"phonton-types must not depend on tokio"`).
        statement: String,
        /// Why this constraint exists.
        rationale: String,
    },
    /// An approach that was tried and rejected — recorded so it isn't retried.
    RejectedApproach {
        /// One-line summary of the approach.
        summary: String,
        /// Why it was rejected.
        reason: String,
    },
    /// A coding/architectural convention to apply by default.
    Convention {
        /// The convention itself (e.g. `"prefer thiserror over anyhow in libs"`).
        rule: String,
        /// Optional scope hint (crate name, language, subsystem).
        scope: Option<String>,
    },
}

/// A natural-language query against the local memory store.
///
/// Returned result is a ranked `Vec<MemoryRecord>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryQuery {
    /// Free-form description of what the caller is looking for.
    pub description: String,
    /// Maximum number of records to return.
    pub top_k: usize,
    /// If set, restrict results to this task's records.
    pub task_filter: Option<TaskId>,
}

/// The full plan produced by `phonton-planner` for a single task.
///
/// `subtasks` is topologically consistent: every `SubtaskId` referenced
/// by `dependencies` appears earlier in the vector than any subtask that
/// depends on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannerOutput {
    /// Subtasks in topological order.
    pub subtasks: Vec<Subtask>,
    /// Planner's pre-execution token estimate for the full task. Compared
    /// against `GlobalState::tokens_used` to report savings to the user.
    pub estimated_total_tokens: u64,
    /// Estimated tokens a naive (non-Phonton) baseline would have spent.
    /// Used by the UI to show the "X% saved" headline.
    pub naive_baseline_tokens: u64,
    /// Honest-signal coverage summary surfaced to the UI alongside the plan.
    /// See `01-architecture/failure-modes.md` Risk 2.
    pub coverage_summary: CoverageSummary,
    /// v0.4.0 first-slice definition of done for this goal.
    #[serde(default)]
    pub goal_contract: Option<GoalContract>,
}

/// Pre-execution coverage signal: how many new symbols the plan creates,
/// and how many test subtasks the planner queued to exercise them.
///
/// The UI renders this verbatim — never as "✓ verified". A non-zero gap
/// (`new_functions > tests_planned`) is shown as a warning the user can
/// choose to ignore, not a block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageSummary {
    /// New `fn`/`struct`/`trait`/etc. items the plan introduces.
    pub new_functions: usize,
    /// Test subtasks the planner queued to cover those items.
    pub tests_planned: usize,
}

impl CoverageSummary {
    /// Render the honest-signal line shown next to the plan in the UI.
    pub fn render(&self) -> String {
        format!(
            "Estimated coverage: {} new functions, {} tests planned.",
            self.new_functions, self.tests_planned
        )
    }
}

// ---------------------------------------------------------------------------
// Accountability spine (v0.4.0 first slice)
// ---------------------------------------------------------------------------

/// Expected artifact described by a [`GoalContract`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedArtifact {
    /// Human-readable artifact description.
    pub description: String,
    /// Optional path where the artifact is expected.
    pub path: Option<PathBuf>,
}

/// A command Phonton believes the user can run to inspect or verify a result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCommand {
    /// Display label, e.g. `"Run chess demo"`.
    pub label: String,
    /// Command tokens in execution order.
    pub command: Vec<String>,
    /// Optional working directory for the command.
    pub cwd: Option<PathBuf>,
}

/// A planned verification step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyStepSpec {
    /// Human-readable check name.
    pub name: String,
    /// Verification layer this step maps to when known.
    pub layer: Option<VerifyLayer>,
    /// Optional concrete command.
    pub command: Option<RunCommand>,
}

/// Minimum bar Phonton should satisfy before claiming a task is review-ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityFloor {
    /// Task-class-specific minimum expectations.
    pub criteria: Vec<String>,
}

/// One independently verifiable slice of a broad goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceSlice {
    /// Stable short id such as `board` or `reset`.
    pub id: String,
    /// User-facing criterion this slice must satisfy.
    pub criterion: String,
    /// Artifact this slice primarily applies to.
    #[serde(default)]
    pub artifact_path: Option<PathBuf>,
    /// Verification steps that can prove this slice.
    #[serde(default)]
    pub verify_plan: Vec<VerifyStepSpec>,
}

/// Token and repair policy selected before dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenPolicy {
    /// First-attempt provider token cap when Phonton should avoid broad waste.
    #[serde(default)]
    pub first_attempt_cap_tokens: Option<u64>,
    /// Whether a broad semantic repair is allowed after a large failure.
    #[serde(default = "default_allow_broad_repair")]
    pub allow_broad_repair: bool,
    /// Whether repair workers should receive only missing acceptance criteria.
    #[serde(default)]
    pub repair_only_missing_criteria: bool,
    /// Human-readable policy notes surfaced in plan/review views.
    #[serde(default)]
    pub notes: Vec<String>,
}

impl Default for TokenPolicy {
    fn default() -> Self {
        Self {
            first_attempt_cap_tokens: None,
            allow_broad_repair: true,
            repair_only_missing_criteria: false,
            notes: Vec::new(),
        }
    }
}

fn default_allow_broad_repair() -> bool {
    true
}

/// Visible definition of done for a top-level goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalContract {
    /// Original user goal.
    pub goal: String,
    /// Inferred task class.
    pub task_class: TaskClass,
    /// Structured preflight classification.
    #[serde(default)]
    pub intent: Option<IntentClassification>,
    /// Confidence as 0-100 to avoid float drift across serialized records.
    pub confidence_percent: u8,
    /// Concrete acceptance criteria.
    pub acceptance_criteria: Vec<String>,
    /// Acceptance criteria split into bounded verification slices.
    #[serde(default)]
    pub acceptance_slices: Vec<AcceptanceSlice>,
    /// Expected files, commands, docs, or generated artifacts.
    pub expected_artifacts: Vec<ExpectedArtifact>,
    /// Paths the planner expects to touch.
    pub likely_files: Vec<PathBuf>,
    /// Planned verification.
    pub verify_plan: Vec<VerifyStepSpec>,
    /// Expected run commands.
    pub run_plan: Vec<RunCommand>,
    /// Minimum bar for the task class.
    pub quality_floor: QualityFloor,
    /// Questions that should be asked before execution if confidence is low.
    pub clarification_questions: Vec<String>,
    /// Assumptions Phonton is making if it proceeds.
    pub assumptions: Vec<String>,
    /// Token and repair policy for this goal.
    #[serde(default)]
    pub token_policy: TokenPolicy,
}

/// Summary of a context source that influenced a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSource {
    /// Source kind, e.g. `"index"`, `"memory"`, `"skill"`, `"mcp"`.
    pub kind: String,
    /// Stable id or path for the source.
    pub id: String,
    /// Review-safe summary.
    pub summary: String,
    /// Tokens attributed to this source when known.
    pub token_count: Option<u64>,
}

/// Prompt/context token buckets used by `/why-tokens`, review, and benchmarks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBucketSummary {
    /// Tokens from selected repository code slices.
    #[serde(default)]
    pub selected_code_tokens: u64,
    /// Candidate repository tokens intentionally omitted.
    #[serde(default)]
    pub omitted_candidate_tokens: u64,
    /// Tokens from persistent memory.
    #[serde(default)]
    pub memory_tokens: u64,
    /// Tokens from skills or steering quality modules.
    #[serde(default)]
    pub skill_tokens: u64,
    /// Tokens from user prompt artifacts or attachments.
    #[serde(default)]
    pub artifact_tokens: u64,
    /// Tokens from verifier or retry diagnostics.
    #[serde(default)]
    pub retry_diagnostic_tokens: u64,
    /// Tokens from MCP/tool output.
    #[serde(default)]
    pub tool_output_tokens: u64,
    /// Tokens removed by context deduplication.
    #[serde(default)]
    pub deduped_tokens: u64,
    /// Provider-side cached input tokens.
    #[serde(default)]
    pub cached_tokens: u64,
}

impl ContextBucketSummary {
    /// Fold one prompt manifest into the durable context bucket summary.
    pub fn add_prompt_manifest(&mut self, manifest: &PromptContextManifest) {
        self.selected_code_tokens = self.selected_code_tokens.saturating_add(
            manifest
                .code_context_tokens
                .saturating_add(manifest.repo_map_tokens),
        );
        self.omitted_candidate_tokens = self
            .omitted_candidate_tokens
            .saturating_add(manifest.omitted_code_tokens);
        self.memory_tokens = self.memory_tokens.saturating_add(manifest.memory_tokens);
        self.artifact_tokens = self
            .artifact_tokens
            .saturating_add(manifest.attachment_tokens);
        self.retry_diagnostic_tokens = self
            .retry_diagnostic_tokens
            .saturating_add(manifest.retry_error_tokens);
        self.tool_output_tokens = self
            .tool_output_tokens
            .saturating_add(manifest.mcp_tool_tokens);
        self.deduped_tokens = self.deduped_tokens.saturating_add(manifest.deduped_tokens);
    }
}

/// Manifest of what influenced the model during a task.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextManifest {
    /// Review-safe source list.
    pub sources: Vec<ContextSource>,
    /// Token buckets by source.
    #[serde(default)]
    pub buckets: ContextBucketSummary,
}

/// Estimated token shape of one prompt sent to a provider.
///
/// These values are deliberately approximate. Provider-reported usage is
/// still the billing source of truth; this manifest exists to make prompt
/// composition and avoidable context waste visible in the Flight Log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptContextManifest {
    /// Tokens attributed to the provider system prompt.
    #[serde(default)]
    pub system_tokens: u64,
    /// Tokens attributed to the current user goal/subtask.
    #[serde(default)]
    pub user_goal_tokens: u64,
    /// Tokens attributed to prior context or memory.
    #[serde(default)]
    pub memory_tokens: u64,
    /// Tokens attributed to user-mentioned attachments.
    #[serde(default)]
    pub attachment_tokens: u64,
    /// Tokens attributed to selected repository code context.
    #[serde(default)]
    pub code_context_tokens: u64,
    /// Tokens attributed to the compact repository map.
    #[serde(default)]
    pub repo_map_tokens: u64,
    /// Candidate code-context tokens intentionally omitted by the compiler.
    #[serde(default)]
    pub omitted_code_tokens: u64,
    /// Target prompt budget selected by the context compiler.
    #[serde(default)]
    pub context_target_tokens: u64,
    /// One-based provider attempt index for this prompt.
    #[serde(default = "default_prompt_attempt")]
    pub attempt: u8,
    /// True when this prompt is a repair/retry attempt.
    #[serde(default)]
    pub repair_attempt: bool,
    /// True when required context forced the prompt estimate above the target.
    #[serde(default)]
    pub target_exceeded: bool,
    /// Estimated tokens above the selected context target.
    #[serde(default)]
    pub over_target_tokens: u64,
    /// Tokens attributed to MCP/tool instructions and results.
    #[serde(default)]
    pub mcp_tool_tokens: u64,
    /// Tokens attributed to retry/verification error context.
    #[serde(default)]
    pub retry_error_tokens: u64,
    /// Sum of the approximate section buckets above.
    #[serde(default)]
    pub total_estimated_tokens: u64,
    /// Configured prompt/context budget limit when known.
    #[serde(default)]
    pub budget_limit: Option<u64>,
    /// Approximate tokens removed by context compaction before this prompt.
    #[serde(default)]
    pub compacted_tokens: u64,
    /// Approximate tokens removed by context deduplication before this prompt.
    #[serde(default)]
    pub deduped_tokens: u64,
}

fn default_prompt_attempt() -> u8 {
    1
}

/// Origin for a workspace trust record.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceTrustSource {
    /// Loaded from the legacy `trusted_workspaces.json` path list.
    LegacyJson,
    /// Loaded from the structured trust JSON record list.
    #[default]
    JsonRecord,
    /// Loaded from the SQLite store mirror.
    Store,
    /// Granted by `PHONTON_TRUST_ALL`.
    EnvOverride,
}

impl std::fmt::Display for WorkspaceTrustSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::LegacyJson => "legacy-json",
            Self::JsonRecord => "json-record",
            Self::Store => "store",
            Self::EnvOverride => "env-override",
        };
        f.write_str(value)
    }
}

/// Persistent trust metadata for one workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceTrustRecord {
    /// Canonical absolute workspace path.
    pub canonical_path: String,
    /// Human-readable workspace label.
    pub display_name: String,
    /// Unix timestamp when trust was first granted.
    pub trusted_at: u64,
    /// Unix timestamp when this workspace was last opened.
    pub last_seen_at: u64,
    /// Permission mode active when trust was recorded or last mirrored.
    pub permission_mode: PermissionMode,
    /// Where this record came from.
    pub source: WorkspaceTrustSource,
}

/// Local execution permission posture for shell, filesystem, and network work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// Keep the default guard behavior: safe workspace actions run, risky
    /// actions require approval or are blocked.
    #[default]
    Ask,
    /// Allow reads but block writes and require approval for command/network
    /// activity.
    ReadOnly,
    /// Allow normal workspace writes and allowlisted commands; keep risky
    /// shell/network activity approval-gated.
    WorkspaceWrite,
    /// Allow all non-sensitive actions inside the local sandbox guard.
    FullAccess,
}

impl PermissionMode {
    /// Stable user/config spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionMode::Ask => "ask",
            PermissionMode::ReadOnly => "read-only",
            PermissionMode::WorkspaceWrite => "workspace-write",
            PermissionMode::FullAccess => "full-access",
        }
    }

    /// Parse a user-facing permission mode.
    pub fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "ask" | "default" => Some(PermissionMode::Ask),
            "read-only" | "readonly" | "read" => Some(PermissionMode::ReadOnly),
            "workspace-write" | "workspace" | "write" => Some(PermissionMode::WorkspaceWrite),
            "full-access" | "full" | "danger" => Some(PermissionMode::FullAccess),
            _ => None,
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Record of one privileged action request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRecord {
    /// Action category, e.g. `"shell"`, `"mcp"`, `"network"`.
    pub action: String,
    /// Scope requested by the action.
    pub scope: String,
    /// Whether it was approved.
    pub approved: bool,
    /// Human-readable approval source or denial reason.
    pub decision: String,
}

/// Ledger of privileged actions involved in a task.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionLedger {
    /// Ordered permission records.
    pub records: Vec<PermissionRecord>,
}

/// Summary of verification work performed for a task.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyReport {
    /// Verification steps that passed.
    pub passed: Vec<String>,
    /// Verification failures or warnings.
    pub findings: Vec<String>,
    /// Checks skipped and why.
    pub skipped: Vec<String>,
}

/// Per-file summary for a handoff packet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFileSummary {
    /// Changed file path.
    pub path: PathBuf,
    /// Added lines when known.
    pub added_lines: u32,
    /// Removed lines when known.
    pub removed_lines: u32,
    /// Short explanation.
    pub summary: String,
}

/// Diff statistics for the full task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffStats {
    /// Files touched.
    pub files_changed: u32,
    /// Added lines.
    pub added_lines: u32,
    /// Removed lines.
    pub removed_lines: u32,
}

/// Artifact generated by the task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedArtifact {
    /// Artifact path.
    pub path: PathBuf,
    /// Artifact description.
    pub description: String,
}

/// User-facing review action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAction {
    /// Short command/action label.
    pub label: String,
    /// Details shown in review UI.
    pub description: String,
}

/// Rollback checkpoint surfaced in a handoff packet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackPoint {
    /// Checkpoint sequence number.
    pub seq: u32,
    /// Human-readable checkpoint label.
    pub label: String,
}

/// Summary of influences that shaped the result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InfluenceSummary {
    /// Memory records used.
    pub memories: Vec<String>,
    /// Index slices used.
    pub index_slices: Vec<String>,
    /// Skills or steering rules used.
    pub skills: Vec<String>,
    /// Extensions or MCP servers/tools used.
    pub extensions: Vec<String>,
}

/// User-facing receipt for a completed or failed task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffPacket {
    /// Task id.
    pub task_id: TaskId,
    /// Original goal.
    pub goal: String,
    /// One-line result.
    pub headline: String,
    /// Files changed.
    pub changed_files: Vec<ChangedFileSummary>,
    /// Generated artifacts.
    pub generated_artifacts: Vec<GeneratedArtifact>,
    /// Diff stats.
    pub diff_stats: DiffStats,
    /// Verification report.
    pub verification: VerifyReport,
    /// Commands users can run.
    pub run_commands: Vec<RunCommand>,
    /// Known limitations or incomplete parts.
    pub known_gaps: Vec<String>,
    /// Review actions.
    pub review_actions: Vec<ReviewAction>,
    /// Rollback points.
    pub rollback_points: Vec<RollbackPoint>,
    /// Token usage for the task.
    pub token_usage: TokenUsage,
    /// Influence summary.
    pub influence: InfluenceSummary,
}

/// Final benchmarkable run status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkFinalStatus {
    /// Task completed and all required verification passed.
    VerifiedSuccess,
    /// Task produced partial or reviewable output but not full verification.
    Partial,
    /// Task failed.
    Failed,
    /// Task cannot be claimed as verified.
    Unverified,
}

/// Exportable benchmark evidence for one completed Phonton run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkRunExport {
    /// Benchmark task class.
    pub task_class: TaskClass,
    /// Original user goal.
    pub goal: String,
    /// Git commit of the fixture repository when known.
    pub repo_commit: String,
    /// Provider that served the final model call.
    pub provider: String,
    /// Model that served the final model call.
    pub model: String,
    /// Provider-reported input tokens.
    pub input_tokens: u64,
    /// Provider-reported output tokens.
    pub output_tokens: u64,
    /// Provider-reported cached input tokens.
    pub cached_tokens: u64,
    /// Estimated USD cost.
    pub cost_usd: f64,
    /// Context contribution by source.
    pub context_buckets: ContextBucketSummary,
    /// Verification summary keyed by check name.
    pub verification: BTreeMap<String, String>,
    /// Quality gate summary keyed by gate name.
    pub quality_gates: BTreeMap<String, String>,
    /// Stable id of the handoff packet for this run.
    pub handoff_packet_id: String,
    /// Benchmarkable final status.
    pub final_status: BenchmarkFinalStatus,
}

/// Exportable proof bundle for one completed Phonton run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofBundleExport {
    /// Task id.
    pub task_id: TaskId,
    /// Original or contracted goal text.
    pub goal: String,
    /// Goal contract that governed the run.
    pub goal_contract: Option<GoalContract>,
    /// Context that influenced the run.
    pub context_manifest: ContextManifest,
    /// Permission/audit evidence for privileged actions.
    pub permission_ledger: PermissionLedger,
    /// Verification report attached to the run.
    pub verify_report: VerifyReport,
    /// Human-review handoff packet.
    pub handoff_packet: HandoffPacket,
    /// Final status suitable for proof/benchmark consumers.
    pub final_status: BenchmarkFinalStatus,
}

/// Durable evidence record for one task run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeLedger {
    /// Task id.
    pub task_id: TaskId,
    /// Goal contract when available.
    pub goal_contract: Option<GoalContract>,
    /// Context manifest.
    pub context_manifest: ContextManifest,
    /// Permission ledger.
    pub permission_ledger: PermissionLedger,
    /// Verification report.
    pub verify_report: VerifyReport,
    /// Handoff packet when available.
    pub handoff: Option<HandoffPacket>,
}

/// Structured memory writes proposed after a task completes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryUpdate {
    /// Records that should be written if accepted.
    pub records: Vec<MemoryRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_context_manifest_backfills_new_budget_fields() {
        let raw = r#"{
            "system_tokens": 10,
            "user_goal_tokens": 5,
            "memory_tokens": 3,
            "attachment_tokens": 2,
            "mcp_tool_tokens": 1,
            "retry_error_tokens": 0,
            "total_estimated_tokens": 21
        }"#;

        let manifest: PromptContextManifest = serde_json::from_str(raw).unwrap();

        assert_eq!(manifest.system_tokens, 10);
        assert_eq!(manifest.code_context_tokens, 0);
        assert_eq!(manifest.compacted_tokens, 0);
        assert_eq!(manifest.deduped_tokens, 0);
        assert_eq!(manifest.budget_limit, None);
        assert_eq!(manifest.attempt, 1);
        assert!(!manifest.repair_attempt);
        assert!(!manifest.target_exceeded);
        assert_eq!(manifest.over_target_tokens, 0);
    }

    #[test]
    fn workspace_trust_record_serializes_permission_mode() {
        let record = WorkspaceTrustRecord {
            canonical_path: "/repo/phonton".into(),
            display_name: "phonton".into(),
            trusted_at: 1,
            last_seen_at: 2,
            permission_mode: PermissionMode::WorkspaceWrite,
            source: WorkspaceTrustSource::JsonRecord,
        };

        let value = serde_json::to_value(record).unwrap();

        assert_eq!(value["permission_mode"], "workspace-write");
        assert_eq!(value["source"], "json-record");
    }

    #[test]
    fn intent_classifier_marks_generated_web_games_as_high_risk_previews() {
        let intent = classify_intent("make chess in html");

        assert_eq!(intent.task_class, TaskClass::GeneratedAppGame);
        assert_eq!(intent.runtime_risk, RuntimeRisk::High);
        assert_eq!(intent.token_risk, TokenRisk::High);
        assert_eq!(intent.recommended_action, IntentAction::PreviewPlan);
        assert!(intent.confidence_percent >= 80);
    }

    #[test]
    fn intent_classifier_keeps_acceptance_slices_as_generated_app_work() {
        let intent = classify_intent(
            "Vite React chess app acceptance slice 1/7: scaffold package.json and src/App.tsx",
        );

        assert_eq!(intent.task_class, TaskClass::GeneratedAppGame);
        assert_eq!(intent.token_risk, TokenRisk::High);
    }

    #[test]
    fn context_buckets_are_serialized_for_benchmark_exports() {
        let buckets = ContextBucketSummary {
            selected_code_tokens: 1200,
            omitted_candidate_tokens: 4300,
            memory_tokens: 100,
            skill_tokens: 20,
            artifact_tokens: 30,
            retry_diagnostic_tokens: 40,
            tool_output_tokens: 50,
            deduped_tokens: 60,
            cached_tokens: 70,
        };
        let manifest = ContextManifest {
            sources: Vec::new(),
            buckets,
        };

        let value = serde_json::to_value(manifest).unwrap();

        assert_eq!(value["buckets"]["selected_code_tokens"], 1200);
        assert_eq!(value["buckets"]["omitted_candidate_tokens"], 4300);
        assert_eq!(value["buckets"]["cached_tokens"], 70);
    }

    #[test]
    fn task_class_serializes_benchmark_names_and_reads_legacy_names() {
        assert_eq!(
            serde_json::to_value(TaskClass::BugFix).unwrap(),
            serde_json::json!("bug_fix")
        );
        assert_eq!(
            serde_json::from_str::<TaskClass>("\"CoreLogic\"").unwrap(),
            TaskClass::CoreLogic
        );
    }
}
