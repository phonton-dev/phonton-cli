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

use crate::{
    ContextAttribution, CostSummary, DiffHunk, ExtensionId, ExtensionKind, ExtensionSource,
    ModelTier, Permission, PromptContextManifest, ProviderKind, SteeringSeverity, SubtaskId,
    TaskId, TokenUsage, VerifyResult,
};

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
    TaskCompleted { task_id: TaskId, tokens_used: u64 },
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
    /// Review payload for a verified subtask. This is emitted only after
    /// `phonton-verify` passes, so downstream UIs can treat it as the
    /// durable "ready for human review" handoff.
    SubtaskReviewReady {
        subtask_id: SubtaskId,
        description: String,
        tier: ModelTier,
        tokens_used: u64,
        #[serde(default)]
        token_usage: TokenUsage,
        #[serde(default)]
        cost: CostSummary,
        diff_hunks: Vec<DiffHunk>,
        verify_result: VerifyResult,
        provider: ProviderKind,
        model_name: String,
    },
    /// Semantic-index context selected for a subtask before the worker
    /// prompt was built. This makes the token/context claim inspectable.
    ContextSelected {
        subtask_id: SubtaskId,
        slices: Vec<ContextAttribution>,
        total_token_count: usize,
    },
    /// Approximate prompt section token costs for one provider call.
    PromptManifest {
        subtask_id: SubtaskId,
        manifest: PromptContextManifest,
    },
    /// A user requested explicit context compaction for the active task.
    ContextCompacted { task_id: TaskId, compacted: bool },
    /// An extension manifest was loaded by the resolver.
    ExtensionLoaded {
        extension_id: ExtensionId,
        kind: ExtensionKind,
        source: ExtensionSource,
        enabled: bool,
    },
    /// An extension manifest was skipped or rejected.
    ExtensionSkipped {
        extension_id: Option<ExtensionId>,
        kind: Option<ExtensionKind>,
        source: ExtensionSource,
        reason: String,
    },
    /// Two extension records conflicted during resolution.
    ExtensionConflict {
        extension_id: ExtensionId,
        lower_source: ExtensionSource,
        higher_source: ExtensionSource,
        detail: String,
    },
    /// A steering rule influenced planning, worker context, or verification.
    SteeringApplied {
        rule_id: ExtensionId,
        severity: SteeringSeverity,
        target: String,
    },
    /// A skill was injected into planner or worker context.
    SkillApplied {
        skill_id: ExtensionId,
        version: String,
        target: String,
    },
    /// An MCP server is configured and visible to the run.
    McpServerAvailable {
        server_id: ExtensionId,
        permissions: Vec<Permission>,
    },
    /// A worker or planner requested an MCP tool call.
    McpToolRequested {
        server_id: ExtensionId,
        tool_name: String,
        permissions: Vec<Permission>,
    },
    /// A requested MCP tool call was approved.
    McpToolApproved {
        server_id: ExtensionId,
        tool_name: String,
    },
    /// A requested MCP tool call was denied by trust or guard policy.
    McpToolDenied {
        server_id: ExtensionId,
        tool_name: String,
        reason: String,
    },
    /// An MCP tool call completed.
    McpToolCompleted {
        server_id: ExtensionId,
        tool_name: String,
        success: bool,
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
        layer: crate::VerifyLayer,
    },
    /// `phonton-verify` returned `Fail`. The orchestrator may still retry.
    VerifyFail {
        subtask_id: SubtaskId,
        layer: crate::VerifyLayer,
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
    /// a worker is waiting for the LLM to reply. Surfaces as "thinking"
    /// in the UI so the user knows why the task is hanging.
    Thinking {
        subtask_id: SubtaskId,
        model_name: String,
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
    /// Human review action persisted as an immutable audit event.
    ReviewDecision {
        task_id: TaskId,
        decision: String,
        detail: String,
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
            OrchestratorEvent::ContextSelected { .. } => "context",
            OrchestratorEvent::PromptManifest { .. } => "prompt",
            OrchestratorEvent::ContextCompacted { .. } => "compact",
            OrchestratorEvent::ExtensionLoaded { .. } => "extension-loaded",
            OrchestratorEvent::ExtensionSkipped { .. } => "extension-skipped",
            OrchestratorEvent::ExtensionConflict { .. } => "extension-conflict",
            OrchestratorEvent::SteeringApplied { .. } => "steering",
            OrchestratorEvent::SkillApplied { .. } => "skill",
            OrchestratorEvent::McpServerAvailable { .. } => "mcp-server",
            OrchestratorEvent::McpToolRequested { .. } => "mcp-request",
            OrchestratorEvent::McpToolApproved { .. } => "mcp-approved",
            OrchestratorEvent::McpToolDenied { .. } => "mcp-denied",
            OrchestratorEvent::McpToolCompleted { .. } => "mcp-completed",
            OrchestratorEvent::SubtaskCompleted { .. } => "subtask-done",
            OrchestratorEvent::SubtaskReviewReady { .. } => "review-ready",
            OrchestratorEvent::SubtaskFailed { .. } => "subtask-failed",
            OrchestratorEvent::VerifyPass { .. } => "verify-pass",
            OrchestratorEvent::VerifyFail { .. } => "verify-fail",
            OrchestratorEvent::VerifyEscalated { .. } => "escalate",
            OrchestratorEvent::TokenMilestone { .. } => "tokens",
            OrchestratorEvent::Thinking { .. } => "thinking",
            OrchestratorEvent::CheckpointCreated { .. } => "checkpoint",
            OrchestratorEvent::RollbackPerformed { .. } => "rollback",
            OrchestratorEvent::ReviewDecision { .. } => "review-decision",
        }
    }

    /// One-line human-readable rendering used by the TUI Flight Log panel.
    pub fn render_line(&self) -> String {
        match &self.event {
            OrchestratorEvent::TaskStarted {
                goal,
                subtask_count,
                ..
            } => {
                format!("task started — {subtask_count} subtasks — {goal}")
            }
            OrchestratorEvent::TaskFailed {
                reason,
                failed_subtask,
                ..
            } => match failed_subtask {
                Some(id) => format!("task failed at {id}: {reason}"),
                None => format!("task failed: {reason}"),
            },
            OrchestratorEvent::TaskCompleted { tokens_used, .. } => {
                format!("task completed — {tokens_used} tokens")
            }
            OrchestratorEvent::SubtaskDispatched {
                subtask_id,
                tier,
                attempt,
            } => {
                format!("dispatch {subtask_id} tier={tier} attempt={attempt}")
            }
            OrchestratorEvent::SubtaskCompleted {
                subtask_id,
                tokens_used,
            } => {
                format!("done {subtask_id} tokens={tokens_used}")
            }
            OrchestratorEvent::ContextSelected {
                subtask_id,
                slices,
                total_token_count,
            } => {
                format!(
                    "context {subtask_id}: {} slices, {} indexed tokens",
                    slices.len(),
                    total_token_count
                )
            }
            OrchestratorEvent::PromptManifest {
                subtask_id,
                manifest,
            } => {
                format!(
                    "prompt {subtask_id}: system={} goal={} memory={} code={} attachments={} mcp={} retry={} compacted={} deduped={} total~{}",
                    manifest.system_tokens,
                    manifest.user_goal_tokens,
                    manifest.memory_tokens,
                    manifest.code_context_tokens,
                    manifest.attachment_tokens,
                    manifest.mcp_tool_tokens,
                    manifest.retry_error_tokens,
                    manifest.compacted_tokens,
                    manifest.deduped_tokens,
                    manifest.total_estimated_tokens
                )
            }
            OrchestratorEvent::ContextCompacted { compacted, .. } => {
                if *compacted {
                    "context compaction completed".to_string()
                } else {
                    "context compaction skipped: no compressible frames".to_string()
                }
            }
            OrchestratorEvent::ExtensionLoaded {
                extension_id,
                kind,
                source,
                enabled,
            } => {
                format!(
                    "extension loaded {extension_id} kind={kind} source={source} enabled={enabled}"
                )
            }
            OrchestratorEvent::ExtensionSkipped {
                extension_id,
                kind,
                source,
                reason,
            } => {
                let id = extension_id
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "(unknown)".into());
                let kind = kind
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "(unknown)".into());
                format!("extension skipped {id} kind={kind} source={source}: {reason}")
            }
            OrchestratorEvent::ExtensionConflict {
                extension_id,
                lower_source,
                higher_source,
                detail,
            } => {
                format!(
                    "extension conflict {extension_id}: {lower_source} overridden by {higher_source}: {detail}"
                )
            }
            OrchestratorEvent::SteeringApplied {
                rule_id,
                severity,
                target,
            } => {
                format!("steering applied {rule_id} severity={severity} target={target}")
            }
            OrchestratorEvent::SkillApplied {
                skill_id,
                version,
                target,
            } => {
                format!("skill applied {skill_id}@{version} target={target}")
            }
            OrchestratorEvent::McpServerAvailable {
                server_id,
                permissions,
            } => {
                let permissions = permissions
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                format!("mcp server available {server_id} permissions=[{permissions}]")
            }
            OrchestratorEvent::McpToolRequested {
                server_id,
                tool_name,
                permissions,
            } => {
                let permissions = permissions
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                format!("mcp tool requested {server_id}.{tool_name} permissions=[{permissions}]")
            }
            OrchestratorEvent::McpToolApproved {
                server_id,
                tool_name,
            } => {
                format!("mcp tool approved {server_id}.{tool_name}")
            }
            OrchestratorEvent::McpToolDenied {
                server_id,
                tool_name,
                reason,
            } => {
                format!("mcp tool denied {server_id}.{tool_name}: {reason}")
            }
            OrchestratorEvent::McpToolCompleted {
                server_id,
                tool_name,
                success,
            } => {
                format!("mcp tool completed {server_id}.{tool_name} success={success}")
            }
            OrchestratorEvent::SubtaskReviewReady {
                subtask_id,
                diff_hunks,
                verify_result,
                ..
            } => {
                format!(
                    "review ready {subtask_id}: {} hunks, verify={verify_result:?}",
                    diff_hunks.len()
                )
            }
            OrchestratorEvent::SubtaskFailed {
                subtask_id,
                reason,
                attempt,
            } => {
                format!("fail {subtask_id} attempt={attempt}: {reason}")
            }
            OrchestratorEvent::VerifyPass { subtask_id, layer } => {
                format!("verify pass {subtask_id} layer={layer:?}")
            }
            OrchestratorEvent::VerifyFail {
                subtask_id,
                layer,
                errors,
                attempt,
            } => {
                format!(
                    "verify fail {subtask_id} layer={layer:?} attempt={attempt}: {}",
                    errors.join("; ")
                )
            }
            OrchestratorEvent::VerifyEscalated {
                subtask_id,
                from,
                to,
                reason,
            } => {
                format!("escalate {subtask_id} {from} → {to}: {reason}")
            }
            OrchestratorEvent::TokenMilestone {
                tokens_used,
                milestone,
                ..
            } => {
                format!("tokens crossed {milestone} — now at {tokens_used}")
            }
            OrchestratorEvent::Thinking {
                subtask_id,
                model_name,
            } => {
                format!("thinking {subtask_id} model={model_name}")
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
                format!("rollback to checkpoint #{to_seq} — {requeued_subtasks} subtasks requeued")
            }
            OrchestratorEvent::ReviewDecision {
                decision, detail, ..
            } => {
                format!("review {decision}: {detail}")
            }
        }
    }
}
