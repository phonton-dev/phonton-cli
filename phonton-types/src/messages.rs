//! Typed messages exchanged over Tokio `mpsc` channels between the
//! orchestrator, workers, and the UI — plus the `GlobalState` broadcast
//! over a `watch` channel to the TUI.
//!
//! All inter-component communication goes through these enums. Workers
//! never share memory; state changes are expressed as messages.

use serde::{Deserialize, Serialize};

use crate::{
    Checkpoint, CodeSlice, ModelTier, Subtask, SubtaskId, SubtaskResult, SubtaskStatus, TaskStatus,
};

// ---------------------------------------------------------------------------
// Orchestrator inbound
// ---------------------------------------------------------------------------

/// Messages received by the orchestrator from workers and the UI.
///
/// The orchestrator owns a single `mpsc::Receiver<OrchestratorMessage>` and
/// routes these to the task state machine and the `GlobalState` broadcaster.
#[derive(Debug)]
pub enum OrchestratorMessage {
    /// Worker has begun executing a subtask.
    SubtaskStarted {
        /// Subtask now running.
        id: SubtaskId,
        /// Tier actually in use — may differ from the planner's choice on retry.
        model_tier: ModelTier,
    },
    /// Periodic token-usage heartbeat from a running worker.
    SubtaskProgress {
        /// Subtask reporting progress.
        id: SubtaskId,
        /// Running total of tokens this subtask has consumed.
        tokens_so_far: u64,
    },
    /// Worker finished a subtask successfully.
    SubtaskDone {
        /// Subtask that completed.
        id: SubtaskId,
        /// Full result payload (diffs + token accounting).
        result: SubtaskResult,
    },
    /// Worker failed on a subtask. Orchestrator decides whether to retry.
    SubtaskFailed {
        /// Subtask that failed.
        id: SubtaskId,
        /// Human-readable failure reason from the worker.
        reason: String,
        /// Attempt counter — first failure is `1`.
        attempt: u8,
    },
    /// User accepted the assembled diffs from the `Reviewing` state.
    UserApproved,
    /// User rejected the assembled diffs, optionally with written feedback.
    UserRejected {
        /// Free-form user feedback to feed into a retry, if any.
        feedback: Option<String>,
    },
    /// User aborted the task from the UI.
    UserCancelled,
    /// Token budget exhausted — orchestrator must cancel all workers.
    BudgetExceeded {
        /// Configured token limit.
        limit: u64,
        /// Actual tokens consumed at the moment of the trip.
        actual: u64,
    },
    /// Roll the worktree back to the named checkpoint and re-plan the
    /// remainder of the DAG. Sent by the UI when the user picks
    /// "Rollback to step N" from the checkpoint list. The orchestrator
    /// aborts in-flight workers, asks `phonton-diff` to reset to the
    /// checkpoint's commit, marks every subtask whose `seq` is greater
    /// than `to_seq` as `Queued`, and resumes the scheduler.
    RollbackRequest {
        /// Checkpoint sequence number to roll back to. The post-rollback
        /// state is "everything up through `to_seq` is `Done`; subtasks
        /// after it are queued".
        to_seq: u32,
    },
}

// ---------------------------------------------------------------------------
// Worker inbound
// ---------------------------------------------------------------------------

/// Messages sent by the orchestrator to a single worker.
///
/// Each worker owns its own `mpsc::Receiver<WorkerMessage>`.
#[derive(Debug)]
pub enum WorkerMessage {
    /// Begin executing the given subtask with the supplied code slices.
    Execute {
        /// The subtask definition, as produced by the planner.
        subtask: Subtask,
        /// Pre-retrieved code context the worker should use.
        context_slices: Vec<CodeSlice>,
    },
    /// Abort the current subtask as soon as possible.
    Cancel,
    /// Soft warning that the task's token budget is nearly exhausted.
    BudgetWarning {
        /// Tokens remaining under the budget.
        remaining: u64,
    },
}

// ---------------------------------------------------------------------------
// Broadcast state
// ---------------------------------------------------------------------------

/// Snapshot of the whole task, published on every state change via
/// `tokio::sync::watch::Sender<GlobalState>`.
///
/// The TUI, desktop UI, and any external observer consume this instead of
/// subscribing to the mpsc message stream directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalState {
    /// High-level lifecycle state of the task.
    pub task_status: TaskStatus,
    /// One entry per worker currently executing a subtask.
    pub active_workers: Vec<WorkerState>,
    /// Total tokens consumed across all subtasks so far.
    pub tokens_used: u64,
    /// Optional user-supplied token budget. `None` means unlimited.
    pub tokens_budget: Option<u64>,
    /// Estimated tokens a naive (non-Phonton) baseline would have spent,
    /// surfaced to the UI as the headline savings figure.
    pub estimated_naive_tokens: u64,
    /// Point-in-time checkpoints accumulated as subtasks land. The CLI
    /// renders these as the "Rollback to step N" picker. Empty until
    /// the first subtask passes verify and is committed.
    #[serde(default)]
    pub checkpoints: Vec<Checkpoint>,
}

/// Live snapshot of a single worker, included in [`GlobalState`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerState {
    /// Subtask this worker is currently executing.
    pub subtask_id: SubtaskId,
    /// Human-readable description, copied from the subtask for UI rendering.
    pub subtask_description: String,
    /// Tier the worker is using.
    pub model_tier: ModelTier,
    /// Tokens spent by this worker on the current subtask.
    pub tokens_used: u64,
    /// Current lifecycle state of the subtask.
    pub status: SubtaskStatus,
}
