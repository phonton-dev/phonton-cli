//! Shared domain types — imported by all phonton crates. No business logic.
//!
//! Rule: if a type crosses a crate boundary, it lives here. Nothing else does.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod events;
pub mod messages;
pub mod providers;

pub use events::{EventRecord, OrchestratorEvent, TOKEN_MILESTONE_INTERVAL};
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
pub enum TaskClass {
    /// Generated docs, stubs, type aliases, trivial wiring.
    Boilerplate,
    /// Unit/integration tests. Routine output, cheap models suffice.
    Tests,
    /// Documentation prose.
    Docs,
    /// Novel algorithmic or architectural work — the one tier that still
    /// justifies a frontier model.
    CoreLogic,
}

impl fmt::Display for TaskClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            TaskClass::Boilerplate => "boilerplate",
            TaskClass::Tests => "tests",
            TaskClass::Docs => "docs",
            TaskClass::CoreLogic => "core-logic",
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
    let d = description.to_ascii_lowercase();

    if d.contains("test") || d.contains("unit-test") || d.contains("integration test") {
        return TaskClass::Tests;
    }
    if d.contains("docstring")
        || d.contains("doc comment")
        || d.contains("readme")
        || d.contains("markdown")
        || d.contains("changelog")
    {
        return TaskClass::Docs;
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
        return TaskClass::Boilerplate;
    }
    TaskClass::CoreLogic
}

/// The effective tier to dispatch a subtask at, given its planner-assigned
/// tier and its classified workload. Core logic keeps its tier; boilerplate,
/// tests, and docs are floored at `Cheap`.
///
/// The cost-aware half of the orchestrator's routing decision. The
/// latency-aware half (driven by `phonton_providers::ModelMetrics`) lives
/// alongside and can still escalate on repeated verify failures via the
/// existing `escalate` path.
pub fn effective_tier(planned: ModelTier, class: TaskClass) -> ModelTier {
    match class {
        TaskClass::CoreLogic => planned,
        TaskClass::Boilerplate | TaskClass::Tests | TaskClass::Docs => match planned {
            ModelTier::Local | ModelTier::Cheap => planned,
            ModelTier::Standard | ModelTier::Frontier => ModelTier::Cheap,
        },
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
    /// Run halted because a `BudgetGuard` ceiling was crossed. Not terminal —
    /// the UI presents a "Approve to continue?" prompt; the user can raise
    /// the limit and resubmit. Distinct from `Failed` so the UI renders it
    /// in amber rather than red.
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
