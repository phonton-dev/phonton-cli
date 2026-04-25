//! Structured telemetry for the orchestrator DAG.
//!
//! Every state transition the orchestrator performs is emitted as an
//! [`OrchestratorEvent`] wrapped in an [`EventRecord`] with a monotonic
//! timestamp. The CLI's Flight Log panel streams these raw events so a
//! failed run can be diagnosed without re-reading the chat transcript.
//!
//! Events are append-only and serialisable — `phonton-store` persists them
//! and any UI (TUI, desktop) can subscribe via a `tokio::sync::broadcast`
//! channel owned by the caller.

use serde::{Deserialize, Serialize};

use crate::{ModelTier, SubtaskId, TaskId, VerifyLayer};

/// Token threshold between successive [`OrchestratorEvent::TokenMilestone`]
/// events. Chosen to be coarse enough to keep the flight log readable on
/// long runs but fine enough to show steady progress.
pub const TOKEN_MILESTONE_INTERVAL: u64 = 1_000;

/// One discrete telemetry event describing a state change inside the
/// orchestrator DAG.
///
/// Variants beyond the four called out in the positioning doc
/// (`TaskStarted`, `TaskFailed`, `VerifyEscalated`, `TokenMilestone`) cover
/// the intermediate DAG transitions that the Flight Log needs to render a
/// coherent narrative of *why* a task reached its terminal state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrchestratorEvent {
    /// A task has been accepted and the DAG is about to start executing.
    TaskStarted {
        task_id: TaskId,
        goal: String,
        subtask_count: usize,
    },
    /// Task reached its terminal failure state.
    TaskFailed {
        task_id: TaskId,
        reason: String,
        failed_subtask: Option<SubtaskId>,
    },
    /// Task walked the DAG to completion and is awaiting review.
    TaskCompleted {
        task_id: TaskId,
        tokens_used: u64,
    },
    /// A worker was dispatched for a subtask at the given tier/attempt.
    SubtaskDispatched {
        subtask_id: SubtaskId,
        tier: ModelTier,
        attempt: u8,
    },
    /// A subtask passed verify and is now `Done`.
    SubtaskCompleted {
        subtask_id: SubtaskId,
        tokens_used: u64,
    },
    /// A subtask hit terminal failure (retry + escalation budget exhausted).
    SubtaskFailed {
        subtask_id: SubtaskId,
        reason: String,
        attempt: u8,
    },
    /// `phonton-verify` returned `Pass` for a produced diff.
    VerifyPass {
        subtask_id: SubtaskId,
        layer: VerifyLayer,
    },
    /// `phonton-verify` returned `Fail`. The orchestrator may still retry.
    VerifyFail {
        subtask_id: SubtaskId,
        layer: VerifyLayer,
        errors: Vec<String>,
        attempt: u8,
    },
    /// The orchestrator bumped a subtask to a higher model tier.
    VerifyEscalated {
        subtask_id: SubtaskId,
        from: ModelTier,
        to: ModelTier,
        reason: String,
    },
    /// Token usage crossed a [`TOKEN_MILESTONE_INTERVAL`] boundary.
    TokenMilestone {
        task_id: TaskId,
        tokens_used: u64,
        milestone: u64,
    },
    /// A subtask landed and the orchestrator created a point-in-time
    /// checkpoint via `phonton-diff`. Surfaces the user-visible `seq`
    /// alongside the underlying git OID.
    CheckpointCreated {
        task_id: TaskId,
        subtask_id: SubtaskId,
        seq: u32,
        commit_oid: String,
    },
    /// The user requested (and the orchestrator performed) a rollback
    /// to a prior checkpoint. The remaining subtasks are now requeued.
    RollbackPerformed {
        task_id: TaskId,
        to_seq: u32,
        requeued_subtasks: usize,
    },
}

/// An [`OrchestratorEvent`] paired with the wall-clock instant it was
/// emitted at and the task it belongs to.
///
/// This is the unit both `phonton-store` persists and the Flight Log
/// renders. Timestamp is unix-epoch milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRecord {
    pub task_id: TaskId,
    pub timestamp_ms: u64,
    pub event: OrchestratorEvent,
}

impl EventRecord {
    /// Short string label used by the flight-log renderer.
    pub fn kind(&self) -> &'static str {
        match &self.event {
            OrchestratorEvent::TaskStarted { .. } => "task-started",
            OrchestratorEvent::TaskFailed { .. } => "task-failed",
            OrchestratorEvent::TaskCompleted { .. } => "task-done",
            OrchestratorEvent::SubtaskDispatched { .. } => "dispatch",
            OrchestratorEvent::SubtaskCompleted { .. } => "subtask-done",
            OrchestratorEvent::SubtaskFailed { .. } => "subtask-failed",
            OrchestratorEvent::VerifyPass { .. } => "verify-pass",
            OrchestratorEvent::VerifyFail { .. } => "verify-fail",
            OrchestratorEvent::VerifyEscalated { .. } => "escalate",
            OrchestratorEvent::TokenMilestone { .. } => "tokens",
            OrchestratorEvent::CheckpointCreated { .. } => "checkpoint",
            OrchestratorEvent::RollbackPerformed { .. } => "rollback",
        }
    }

    /// One-line human-readable rendering used by the TUI Flight Log panel.
    pub fn render_line(&self) -> String {
        match &self.event {
            OrchestratorEvent::TaskStarted { goal, subtask_count, .. } => {
                format!("task started — {subtask_count} subtasks — {goal}")
            }
            OrchestratorEvent::TaskFailed { reason, failed_subtask, .. } => match failed_subtask {
                Some(id) => format!("task failed at {id}: {reason}"),
                None => format!("task failed: {reason}"),
            },
            OrchestratorEvent::TaskCompleted { tokens_used, .. } => {
                format!("task completed — {tokens_used} tokens")
            }
            OrchestratorEvent::SubtaskDispatched { subtask_id, tier, attempt } => {
                format!("dispatch {subtask_id} tier={tier} attempt={attempt}")
            }
            OrchestratorEvent::SubtaskCompleted { subtask_id, tokens_used } => {
                format!("done {subtask_id} tokens={tokens_used}")
            }
            OrchestratorEvent::SubtaskFailed { subtask_id, reason, attempt } => {
                format!("fail {subtask_id} attempt={attempt}: {reason}")
            }
            OrchestratorEvent::VerifyPass { subtask_id, layer } => {
                format!("verify pass {subtask_id} layer={layer:?}")
            }
            OrchestratorEvent::VerifyFail { subtask_id, layer, errors, attempt } => {
                format!(
                    "verify fail {subtask_id} layer={layer:?} attempt={attempt}: {}",
                    errors.join("; ")
                )
            }
            OrchestratorEvent::VerifyEscalated { subtask_id, from, to, reason } => {
                format!("escalate {subtask_id} {from} → {to}: {reason}")
            }
            OrchestratorEvent::TokenMilestone { tokens_used, milestone, .. } => {
                format!("tokens crossed {milestone} — now at {tokens_used}")
            }
            OrchestratorEvent::CheckpointCreated {
                subtask_id,
                seq,
                commit_oid,
                ..
            } => {
                let short = commit_oid.chars().take(8).collect::<String>();
                format!("checkpoint #{seq} ({short}) for {subtask_id}")
            }
            OrchestratorEvent::RollbackPerformed {
                to_seq,
                requeued_subtasks,
                ..
            } => {
                format!(
                    "rollback to checkpoint #{to_seq} — {requeued_subtasks} subtasks requeued"
                )
            }
        }
    }
}
