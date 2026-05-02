//! Tokio async DAG executor and global ADE state machine.
//!
//! Given a [`PlannerOutput`], the orchestrator:
//!
//! 1. Builds a `petgraph` DAG from the subtask list, with one edge per
//!    `dependencies` entry.
//! 2. Walks the DAG topologically — a subtask becomes `Ready` only when
//!    every dependency reaches `SubtaskStatus::Done`.
//! 3. Dispatches workers through a caller-supplied [`WorkerDispatcher`]
//!    and manages their lifetimes with a [`tokio::task::JoinSet`].
//! 4. Routes every worker-produced diff through [`phonton_verify::verify_diff`]
//!    before marking the subtask `Done`. Workers cannot bypass verify.
//! 5. Re-dispatches on `VerifyResult::Fail` with an incremented attempt
//!    counter and the error set threaded back into the retry.
//! 6. Bumps the [`ModelTier`] on `VerifyResult::Escalate` and re-dispatches
//!    at the new tier; when the tier ceiling is hit the subtask fails.
//! 7. Publishes a fresh [`GlobalState`] snapshot on every transition via a
//!    `tokio::sync::watch::Sender<GlobalState>` the caller owns.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use petgraph::graph::{DiGraph, NodeIndex};
use phonton_types::{
    classify_task, effective_tier, BudgetDecision, BudgetLimits, Checkpoint, CostSummary, DiffHunk,
    EventRecord, GlobalState, ModelPricing, ModelTier, OrchestratorEvent, OrchestratorMessage,
    PlannerOutput, ProviderKind, Subtask, SubtaskId, SubtaskResult, SubtaskStatus, TaskId,
    TaskStatus, TokenUsage, VerifyResult, WorkerState, TOKEN_MILESTONE_INTERVAL,
};
use std::collections::HashSet;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinSet;
use tracing::{debug, warn};

/// Retries permitted at the same tier before the orchestrator escalates.
///
/// One retry (i.e. two total attempts at a tier) catches the common
/// "model produced a bad first cut, fed the verify error back and the
/// second cut compiles" pattern without burning a long tail of attempts
/// when the tier simply cannot solve the subtask. The previous value of
/// `2` (three attempts) routinely cost users ~60s before failing on
/// free-tier keys; shorter tails make failure cheap and escalation
/// arrive sooner.
pub const MAX_RETRIES_PER_TIER: u8 = 1;

/// Escalations permitted before the orchestrator surfaces a hard failure.
pub const MAX_ESCALATIONS: u8 = 3;

/// Deprecated — budget pauses now produce `TaskStatus::Paused` directly.
/// Kept for any downstream code that may still substring-match the old sentinel.
#[deprecated(note = "budget pauses now emit TaskStatus::Paused; remove this check")]
#[allow(dead_code)]
pub const BUDGET_PAUSE_PREFIX: &str = "BUDGET_PAUSE: ";

// ---------------------------------------------------------------------------
// Budget guard
// ---------------------------------------------------------------------------

/// Per-goal budget enforcement.
///
/// The orchestrator feeds every worker-billed call into [`charge`]; the
/// guard keeps running totals for tokens and USD (in micro-dollars), and
/// returns [`BudgetDecision::Pause`] the moment either configured ceiling
/// is crossed. The orchestrator then aborts the in-flight DAG and surfaces
/// a [`TaskStatus::Failed`] whose `reason` begins with
/// [`TaskStatus::Paused`] so the UI can present it as a pause rather
/// than a terminal error.
///
/// Pricing is keyed by `(ProviderKind, model_name)`; unknown models are
/// treated as free for USD accounting (tokens still count). A future
/// iteration will pull pricing from a shipped table; today callers wire
/// in whatever they know.
#[derive(Debug, Clone, Default)]
pub struct BudgetGuard {
    limits: BudgetLimits,
    pricing: HashMap<(ProviderKind, String), ModelPricing>,
    tokens_used: u64,
    usd_micros_spent: u64,
}

impl BudgetGuard {
    /// Fresh guard with the given ceilings. A default-constructed
    /// [`BudgetLimits`] imposes no cap.
    pub fn new(limits: BudgetLimits) -> Self {
        Self {
            limits,
            pricing: HashMap::new(),
            tokens_used: 0,
            usd_micros_spent: 0,
        }
    }

    /// Register the price of `model` under `provider`. Without an entry
    /// the guard only enforces the token ceiling for that model.
    pub fn with_price(
        mut self,
        provider: ProviderKind,
        model: &str,
        pricing: ModelPricing,
    ) -> Self {
        self.pricing.insert((provider, model.to_string()), pricing);
        self
    }

    /// Charge one worker call against the budget.
    ///
    /// Token totals are always updated. USD is only charged when a
    /// matching `(provider, model)` price was registered via
    /// [`with_price`]. Returns `BudgetDecision::Pause` the first time
    /// either ceiling is crossed.
    pub fn charge(
        &mut self,
        provider: ProviderKind,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> BudgetDecision {
        self.tokens_used = self
            .tokens_used
            .saturating_add(input_tokens.saturating_add(output_tokens));
        if let Some(p) = self.pricing.get(&(provider, model.to_string())) {
            self.usd_micros_spent = self
                .usd_micros_spent
                .saturating_add(p.cost_micros(input_tokens, output_tokens));
        }
        self.decision()
    }

    /// Estimate cost for a usage bucket without mutating running totals.
    pub fn estimate(&self, provider: ProviderKind, model: &str, usage: TokenUsage) -> CostSummary {
        let Some(p) = self.pricing.get(&(provider, model.to_string())) else {
            return CostSummary {
                pricing_known: false,
                ..CostSummary::default()
            };
        };
        let input_usd_micros =
            ((usage.input_tokens as u128 * p.input_usd_micros_per_mtok as u128) / 1_000_000) as u64;
        let output_usd_micros = ((usage.output_tokens as u128
            * p.output_usd_micros_per_mtok as u128)
            / 1_000_000) as u64;
        CostSummary {
            pricing_known: true,
            input_usd_micros,
            output_usd_micros,
            total_usd_micros: input_usd_micros.saturating_add(output_usd_micros),
        }
    }

    /// Decision for the current running totals without charging anything.
    pub fn decision(&self) -> BudgetDecision {
        if let Some(ceiling) = self.limits.max_tokens {
            if self.tokens_used >= ceiling {
                return BudgetDecision::Pause {
                    limit: "tokens".into(),
                    observed: self.tokens_used,
                    ceiling,
                };
            }
        }
        if let Some(ceiling) = self.limits.max_usd_micros {
            if self.usd_micros_spent >= ceiling {
                return BudgetDecision::Pause {
                    limit: "usd".into(),
                    observed: self.usd_micros_spent,
                    ceiling,
                };
            }
        }
        BudgetDecision::Ok
    }

    /// Running token total.
    pub fn tokens_used(&self) -> u64 {
        self.tokens_used
    }

    /// Running micro-dollar total.
    pub fn usd_micros_spent(&self) -> u64 {
        self.usd_micros_spent
    }
}

// ---------------------------------------------------------------------------
// Dispatcher contract
// ---------------------------------------------------------------------------

/// Pluggable worker dispatch contract.
///
/// The orchestrator never constructs a `phonton-worker::Worker` directly —
/// doing so would drag provider configuration and tool-execution policy
/// into this crate. Instead, the caller wires up workers behind this trait
/// and hands the orchestrator an `Arc<dyn WorkerDispatcher>`.
///
/// Each call corresponds to one worker attempt. `prior_errors` is the error
/// set from the previous failing `VerifyResult::Fail`, to be threaded into
/// the worker's prompt as additional context; `attempt` is 1-indexed and
/// resets to 1 when the orchestrator escalates the tier.
#[async_trait]
pub trait WorkerDispatcher: Send + Sync + 'static {
    /// Dispatch a single worker for `subtask` at its currently assigned
    /// [`ModelTier`]. The returned [`SubtaskResult`] carries the produced
    /// diff hunks; the orchestrator then runs them through
    /// [`phonton_verify::verify_diff`] independently of any worker-side
    /// verdict, per the "no worker diff bypasses verify" rule.
    async fn dispatch(
        &self,
        subtask: Subtask,
        prior_errors: Vec<String>,
        attempt: u8,
        msg_tx: Option<tokio::sync::mpsc::Sender<OrchestratorMessage>>,
    ) -> Result<SubtaskResult>;
}

// ---------------------------------------------------------------------------
// Per-subtask runtime bookkeeping
// ---------------------------------------------------------------------------

/// Mutable bookkeeping for one subtask for the duration of a task run.
struct SubtaskRuntime {
    subtask: Subtask,
    node: NodeIndex,
    status: SubtaskStatus,
    attempts_at_tier: u8,
    escalations: u8,
    prior_errors: Vec<String>,
    tokens_used: u64,
    token_usage: TokenUsage,
    diff_hunks: Vec<DiffHunk>,
    /// Provider that served the most recent successful LLM call. Used by
    /// `BudgetGuard` to look up per-model pricing.
    provider: ProviderKind,
    /// Model name from the most recent LLM call. Empty until the first
    /// worker result is received.
    model_name: String,
    /// True if the worker is actively waiting for an LLM response.
    is_thinking: bool,
}

impl SubtaskRuntime {
    fn new(subtask: Subtask, node: NodeIndex) -> Self {
        Self {
            status: SubtaskStatus::Queued,
            subtask,
            node,
            attempts_at_tier: 0,
            escalations: 0,
            prior_errors: Vec::new(),
            tokens_used: 0,
            token_usage: TokenUsage::default(),
            diff_hunks: Vec::new(),
            provider: ProviderKind::Anthropic,
            model_name: String::new(),
            is_thinking: false,
        }
    }

    fn is_done(&self) -> bool {
        matches!(self.status, SubtaskStatus::Done { .. })
    }

    fn is_failed(&self) -> bool {
        matches!(self.status, SubtaskStatus::Failed { .. })
    }

    fn is_terminal(&self) -> bool {
        self.is_done() || self.is_failed()
    }

    fn is_active(&self) -> bool {
        matches!(
            self.status,
            SubtaskStatus::Dispatched | SubtaskStatus::Running { .. }
        )
    }
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

/// Async DAG executor coordinating workers and the verify spine.
///
/// Construct one with a [`WorkerDispatcher`] and drive it with
/// [`Orchestrator::run_task`]. The orchestrator is `!Clone` on purpose —
/// the `tokens_used` / `GlobalState` accounting is exclusive to a single
/// task run.
pub struct Orchestrator<D: WorkerDispatcher + ?Sized> {
    dispatcher: Arc<D>,
    estimated_naive_tokens: u64,
    tokens_budget: Option<u64>,
    budget_guard: Option<Arc<Mutex<BudgetGuard>>>,
    memory: Option<phonton_memory::MemoryStore>,
    diff_applier: Option<Arc<Mutex<phonton_diff::DiffApplier>>>,
    control_rx: Arc<Mutex<Option<tokio::sync::mpsc::Receiver<OrchestratorMessage>>>>,
    working_dir: std::path::PathBuf,
    task_id: TaskId,
    goal_text: String,
    event_sink: Option<broadcast::Sender<EventRecord>>,
}

impl<D: WorkerDispatcher + ?Sized> Orchestrator<D> {
    /// Construct an orchestrator bound to a dispatcher.
    pub fn new(dispatcher: Arc<D>) -> Self {
        Self {
            dispatcher,
            estimated_naive_tokens: 0,
            tokens_budget: None,
            budget_guard: None,
            memory: None,
            diff_applier: None,
            control_rx: Arc::new(Mutex::new(None)),
            working_dir: std::path::PathBuf::from("."),
            task_id: TaskId::new(),
            goal_text: String::new(),
            event_sink: None,
        }
    }

    /// Attach structured telemetry: every DAG state change will be
    /// published as an [`EventRecord`] on `sender` (late subscribers
    /// simply miss earlier events — persistence is a separate concern).
    /// `task_id` and `goal_text` are embedded in emitted events so the
    /// Flight Log can group events by run.
    pub fn with_event_sink(
        mut self,
        task_id: TaskId,
        goal_text: impl Into<String>,
        sender: broadcast::Sender<EventRecord>,
    ) -> Self {
        self.task_id = task_id;
        self.goal_text = goal_text.into();
        self.event_sink = Some(sender);
        self
    }

    /// Directory in which phonton-verify runs cargo commands. Defaults to
    /// `"."`. Typically set to the repo root or a scratch worktree path.
    pub fn with_working_dir(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.working_dir = path.into();
        self
    }

    /// Baseline token estimate surfaced on the UI savings meter.
    pub fn with_naive_baseline(mut self, naive_tokens: u64) -> Self {
        self.estimated_naive_tokens = naive_tokens;
        self
    }

    /// Optional hard token budget. `None` leaves the task unbounded.
    pub fn with_budget(mut self, budget: Option<u64>) -> Self {
        self.tokens_budget = budget;
        self
    }

    /// Attach a [`BudgetGuard`] tracking USD/token ceilings. When the
    /// guard returns [`BudgetDecision::Pause`], the orchestrator aborts
    /// in-flight work and surfaces a [`TaskStatus::Failed`] whose
    /// [`TaskStatus::Paused`] is emitted instead of `Failed`.
    pub fn with_budget_guard(mut self, guard: BudgetGuard) -> Self {
        self.budget_guard = Some(Arc::new(Mutex::new(guard)));
        self
    }

    /// Attach a [`phonton_memory::MemoryStore`] for the verify spine to
    /// consult on every diff. With memory wired in, `phonton-verify`
    /// runs Layer 1.5 (Decision Check) between Syntax and CrateCheck:
    /// diffs that violate a recorded decision/constraint/convention or
    /// reproduce a rejected approach fail immediately, with the
    /// offending record's text as the error context. Without memory,
    /// the layer is skipped and the pipeline behaves as before.
    pub fn with_memory(mut self, memory: phonton_memory::MemoryStore) -> Self {
        self.memory = Some(memory);
        self
    }

    fn cost_summary(&self, provider: ProviderKind, model: &str, usage: TokenUsage) -> CostSummary {
        let Some(guard) = &self.budget_guard else {
            return CostSummary::default();
        };
        guard
            .lock()
            .map(|g| g.estimate(provider, model, usage))
            .unwrap_or_default()
    }

    /// Attach a `phonton_diff::DiffApplier` so the orchestrator can take
    /// a point-in-time checkpoint commit after every subtask passes
    /// verify. The applier is shared (`Arc<Mutex<...>>`) so the same
    /// instance can also be used to apply hunks elsewhere.
    pub fn with_diff_applier(mut self, diff: Arc<Mutex<phonton_diff::DiffApplier>>) -> Self {
        self.diff_applier = Some(diff);
        self
    }

    /// Provide a control-message channel the orchestrator polls between
    /// scheduler iterations. Today this is the rollback path: the UI
    /// sends `OrchestratorMessage::RollbackRequest { to_seq }` and the
    /// orchestrator aborts in-flight workers, asks `phonton-diff` to
    /// reset to the named checkpoint, requeues every subtask after it,
    /// and resumes the scheduler.
    pub fn with_control_channel(
        self,
        rx: tokio::sync::mpsc::Receiver<OrchestratorMessage>,
    ) -> Self {
        if let Ok(mut slot) = self.control_rx.lock() {
            *slot = Some(rx);
        }
        self
    }

    /// Run a full task to completion.
    ///
    /// Walks `plan` as a DAG, dispatches workers as dependencies clear,
    /// verifies every diff, and returns the terminal [`TaskStatus`] —
    /// `Reviewing` on a fully successful walk, `Failed` when a subtask
    /// exhausts its escalations.
    ///
    /// `state_tx` receives a fresh [`GlobalState`] on every transition.
    pub async fn run_task(
        &self,
        plan: PlannerOutput,
        state_tx: watch::Sender<GlobalState>,
    ) -> Result<TaskStatus> {
        // 1. Build the DAG and the SubtaskId → NodeIndex lookup.
        let (graph, mut runtimes) = build_graph(&plan)?;

        self.emit(OrchestratorEvent::TaskStarted {
            task_id: self.task_id,
            goal: self.goal_text.clone(),
            subtask_count: plan.subtasks.len(),
        });
        let mut last_milestone: u64 = 0;

        // 2. Mark subtasks with no deps as Ready so the first scheduler
        //    sweep can pick them up.
        for rt in runtimes.values_mut() {
            if graph
                .neighbors_directed(rt.node, petgraph::Direction::Incoming)
                .next()
                .is_none()
            {
                rt.status = SubtaskStatus::Ready;
            }
        }

        let mut joinset: JoinSet<(SubtaskId, Result<SubtaskResult>)> = JoinSet::new();
        let mut tokens_used: u64 = 0;
        let mut task_status = TaskStatus::Planning;
        let mut failure: Option<(SubtaskId, String)> = None;
        // Budget-pause: (triggering_subtask_id, limit_name, observed, ceiling).
        // Set when BudgetGuard fires; produces TaskStatus::Paused at the end.
        let mut paused: Option<(SubtaskId, String, u64, u64)> = None;
        let mut checkpoints: Vec<Checkpoint> = Vec::new();
        let mut checkpointed: HashSet<SubtaskId> = HashSet::new();
        let mut next_seq: u32 = 1;

        // Channel for worker-to-orchestrator intermediate messages.
        let (worker_msg_tx, mut worker_msg_rx) = mpsc::channel::<OrchestratorMessage>(32);

        // Take ownership of the control channel for the duration of this run.
        let mut control_rx = self.control_rx.lock().ok().and_then(|mut g| g.take());

        // Initial broadcast so UIs see the freshly planned task.
        broadcast(
            &state_tx,
            &task_status,
            &runtimes,
            tokens_used,
            self.tokens_budget,
            self.estimated_naive_tokens,
            &checkpoints,
        );

        // 3. Main scheduler loop. Each iteration either dispatches newly
        //    ready subtasks or waits for an in-flight one to finish.
        loop {
            if failure.is_none() {
                self.schedule_ready(&graph, &mut runtimes, &mut joinset, worker_msg_tx.clone());
            }

            let any_active = runtimes.values().any(|r| r.is_active());
            if !any_active && joinset.is_empty() {
                // Nothing in flight and nothing scheduled — terminal.
                break;
            }

            task_status = task_status_from(&runtimes, tokens_used, self.estimated_naive_tokens);
            broadcast(
                &state_tx,
                &task_status,
                &runtimes,
                tokens_used,
                self.tokens_budget,
                self.estimated_naive_tokens,
                &checkpoints,
            );

            // Race a worker completion against an inbound control
            // message (rollback request, etc.) or an intermediate worker
            // message (progress, thinking).
            let joined = tokio::select! {
                biased;
                msg = worker_msg_rx.recv() => {
                    match msg {
                        Some(OrchestratorMessage::SubtaskThinking { id, model_name }) => {
                            if let Some(rt) = runtimes.get_mut(&id) {
                                rt.is_thinking = true;
                                rt.model_name = model_name.clone();
                                self.emit(OrchestratorEvent::Thinking {
                                    subtask_id: id,
                                    model_name,
                                });
                            }
                            continue;
                        }
                        Some(OrchestratorMessage::ContextSelected {
                            id,
                            slices,
                            total_token_count,
                        }) => {
                            self.emit(OrchestratorEvent::ContextSelected {
                                subtask_id: id,
                                slices,
                                total_token_count,
                            });
                            continue;
                        }
                        Some(OrchestratorMessage::SubtaskProgress { id, tokens_so_far }) => {
                            if let Some(rt) = runtimes.get_mut(&id) {
                                rt.tokens_used = tokens_so_far;
                            }
                            continue;
                        }
                        _ => continue,
                    }
                }
                msg = async {
                    match control_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(OrchestratorMessage::RollbackRequest { to_seq }) = msg {
                        joinset.abort_all();
                        let requeued = self.handle_rollback(
                            to_seq,
                            &mut runtimes,
                            &mut checkpoints,
                            &mut checkpointed,
                            &mut next_seq,
                        );
                        self.emit(OrchestratorEvent::RollbackPerformed {
                            task_id: self.task_id,
                            to_seq,
                            requeued_subtasks: requeued,
                        });
                        continue;
                    }
                    joinset.join_next().await
                }
                j = joinset.join_next() => j,
            };

            let Some(joined) = joined else {
                break;
            };
            let (id, dispatch_outcome) = match joined {
                Ok(pair) => pair,
                Err(join_err) => {
                    warn!(error = %join_err, "worker task panicked or was cancelled");
                    continue;
                }
            };

            // 4. Route through verify, possibly re-dispatch.
            let dispatch_result = match dispatch_outcome {
                Ok(sr) => {
                    self.handle_completion(
                        &mut runtimes,
                        id,
                        sr,
                        &mut joinset,
                        worker_msg_tx.clone(),
                    )
                    .await
                }
                Err(e) => {
                    warn!(subtask = %id, error = %e, "worker dispatch returned Err");
                    fail_subtask(&mut runtimes, id, format!("dispatch error: {e}"));
                    Ok(())
                }
            };
            if let Err(e) = dispatch_result {
                warn!(subtask = %id, error = %e, "verify pipeline error");
                fail_subtask(&mut runtimes, id, format!("verify error: {e}"));
            }

            tokens_used = runtimes.values().map(|r| r.tokens_used).sum();

            // Take a checkpoint for any newly-Done subtask. Order is
            // insertion order into `runtimes`; for the rollback UX we
            // need a stable seq within a task, so we assign on first
            // observation of `Done` rather than at planner time.
            {
                let newly_done: Vec<(SubtaskId, String, Vec<phonton_types::DiffHunk>)> = runtimes
                    .values()
                    .filter(|r| r.is_done() && !checkpointed.contains(&r.subtask.id))
                    .map(|r| {
                        (
                            r.subtask.id,
                            r.subtask.description.clone(),
                            r.diff_hunks.clone(),
                        )
                    })
                    .collect();
                for (sid, desc, hunks) in newly_done {
                    let seq = next_seq;
                    next_seq = next_seq.saturating_add(1);

                    if let Some(diff) = &self.diff_applier {
                        // Git-backed path: apply hunks → stage → checkpoint commit.
                        let checkpoint = match diff.lock() {
                            Ok(mut d) => {
                                if let Err(e) = d.apply_verified_hunks(&hunks) {
                                    warn!(error = %e, subtask = %sid, "apply_verified_hunks failed");
                                }
                                match d.commit_checkpoint(self.task_id, sid, seq, &desc) {
                                    Ok(c) => Some(c),
                                    Err(e) => {
                                        warn!(error = %e, subtask = %sid, "checkpoint commit failed");
                                        None
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, "diff applier mutex poisoned");
                                None
                            }
                        };
                        checkpointed.insert(sid);
                        if let Some(c) = checkpoint {
                            self.emit(OrchestratorEvent::CheckpointCreated {
                                task_id: self.task_id,
                                subtask_id: sid,
                                seq: c.seq,
                                commit_oid: c.commit_oid.clone(),
                            });
                            checkpoints.push(c);
                        }
                    } else {
                        // No git repo — write files directly to disk so the
                        // user sees output even without a git repository.
                        apply_hunks_direct(&hunks, &self.working_dir);
                        checkpointed.insert(sid);
                    }
                }
            }

            // Emit token-milestone events for each crossed boundary.
            while tokens_used / TOKEN_MILESTONE_INTERVAL > last_milestone {
                last_milestone += 1;
                self.emit(OrchestratorEvent::TokenMilestone {
                    task_id: self.task_id,
                    tokens_used,
                    milestone: last_milestone * TOKEN_MILESTONE_INTERVAL,
                });
            }

            // Budget check (legacy single-token cap).
            if let Some(limit) = self.tokens_budget {
                if tokens_used >= limit && failure.is_none() && paused.is_none() {
                    paused = Some((id, "tokens".into(), tokens_used, limit));
                    joinset.abort_all();
                }
            }

            // BudgetGuard: USD/token ceiling enforcement. Charge the delta
            // tokens observed since the last iteration, tagged with the
            // provider/model from the subtask that just completed so the
            // pricing table lookup succeeds when the caller registered
            // a price for that model.
            if let Some(guard) = &self.budget_guard {
                if failure.is_none() && paused.is_none() {
                    if let Ok(mut g) = guard.lock() {
                        let already = g.tokens_used();
                        let delta = tokens_used.saturating_sub(already);
                        if delta > 0 {
                            let (charge_provider, charge_model, usage) = runtimes
                                .get(&id)
                                .map(|r| (r.provider, r.model_name.clone(), r.token_usage))
                                .unwrap_or((
                                    ProviderKind::Anthropic,
                                    String::new(),
                                    TokenUsage::estimated(delta),
                                ));
                            let input = if usage.budget_tokens() > 0 {
                                usage
                                    .input_tokens
                                    .saturating_add(usage.cache_creation_tokens)
                            } else {
                                delta
                            };
                            let output = usage.output_tokens;
                            let _ = g.charge(charge_provider, &charge_model, input, output);
                        }
                        if let BudgetDecision::Pause {
                            limit,
                            observed,
                            ceiling,
                        } = g.decision()
                        {
                            paused = Some((id, limit, observed, ceiling));
                            joinset.abort_all();
                        }
                    }
                }
            }

            // Hard-fail propagation: any Failed subtask aborts the task.
            if failure.is_none() {
                if let Some((fid, reason)) = runtimes
                    .values()
                    .find(|r| r.is_failed())
                    .map(|r| (r.subtask.id, failure_reason(&r.status)))
                {
                    failure = Some((fid, reason));
                    joinset.abort_all();
                }
            }
        }

        // 5. Final task status. Paused takes priority over a simultaneous
        //    failure (the pause aborted the joinset so any failure that
        //    raced in is noise). Real failures take priority over nothing.
        let terminal = if let Some((_, limit, observed, ceiling)) = paused {
            TaskStatus::Paused {
                limit,
                observed,
                ceiling,
            }
        } else if let Some((fid, reason)) = failure {
            self.emit(OrchestratorEvent::TaskFailed {
                task_id: self.task_id,
                reason: reason.clone(),
                failed_subtask: Some(fid),
            });
            TaskStatus::Failed {
                reason,
                failed_subtask: Some(fid),
            }
        } else {
            self.emit(OrchestratorEvent::TaskCompleted {
                task_id: self.task_id,
                tokens_used,
            });
            TaskStatus::Reviewing {
                tokens_used,
                estimated_savings_tokens: self.estimated_naive_tokens.saturating_sub(tokens_used),
            }
        };
        broadcast(
            &state_tx,
            &terminal,
            &runtimes,
            tokens_used,
            self.tokens_budget,
            self.estimated_naive_tokens,
            &checkpoints,
        );
        Ok(terminal)
    }

    /// Apply an inbound `RollbackRequest`.
    ///
    /// Hard-resets the worktree to the checkpoint with seq = `to_seq`
    /// (via `phonton-diff`), then walks the runtime map and re-marks
    /// every subtask whose checkpoint seq is *greater than* `to_seq`
    /// as `Queued`, dropping its diff hunks and prior errors so the
    /// scheduler will re-dispatch fresh. Subtasks at or below the
    /// target seq are left in `Done`.
    ///
    /// Crucially, subtasks that *depend on* any rolled-back subtask are
    /// also requeued, even if they were never checkpointed themselves.
    /// This transitive invalidation ensures the scheduler re-evaluates
    /// the full DAG tail after a rollback.
    ///
    /// Returns the count of subtasks that were requeued so the
    /// `RollbackPerformed` event can carry an accurate number.
    fn handle_rollback(
        &self,
        to_seq: u32,
        runtimes: &mut HashMap<SubtaskId, SubtaskRuntime>,
        checkpoints: &mut Vec<Checkpoint>,
        checkpointed: &mut HashSet<SubtaskId>,
        next_seq: &mut u32,
    ) -> usize {
        // Find the target checkpoint commit.
        let target = checkpoints.iter().find(|c| c.seq == to_seq).cloned();
        let target_oid = match target {
            Some(c) => c.commit_oid,
            None => {
                warn!(to_seq, "rollback target seq not found; ignoring");
                return 0;
            }
        };
        if let Some(diff) = &self.diff_applier {
            if let Ok(mut d) = diff.lock() {
                if let Err(e) = d.rollback_to_checkpoint(&target_oid) {
                    warn!(error = %e, "rollback_to_checkpoint failed");
                    return 0;
                }
            }
        }

        // Seed set: every checkpoint with seq > to_seq.
        let mut invalidated: HashSet<SubtaskId> = checkpoints
            .iter()
            .filter(|c| c.seq > to_seq)
            .map(|c| c.subtask_id)
            .collect();

        // Trim the checkpoint list and bookkeeping in lockstep.
        checkpoints.retain(|c| c.seq <= to_seq);
        for id in &invalidated {
            checkpointed.remove(id);
        }
        *next_seq = to_seq.saturating_add(1);

        // Expand the invalidated set to include every subtask that
        // transitively depends on one of the rolled-back subtasks.
        // Fixed-point loop: keep growing until no new subtask is added.
        loop {
            let mut grew = false;
            for rt in runtimes.values() {
                if invalidated.contains(&rt.subtask.id) {
                    continue;
                }
                // If any of this subtask's deps is invalidated, it must
                // also be invalidated — regardless of its current status.
                let depends_on_invalid = rt
                    .subtask
                    .dependencies
                    .iter()
                    .any(|dep| invalidated.contains(dep));
                if depends_on_invalid {
                    invalidated.insert(rt.subtask.id);
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }

        let mut requeued = 0usize;
        for rt in runtimes.values_mut() {
            if invalidated.contains(&rt.subtask.id) {
                rt.status = SubtaskStatus::Queued;
                rt.attempts_at_tier = 0;
                rt.escalations = 0;
                rt.prior_errors.clear();
                rt.tokens_used = 0;
                rt.diff_hunks.clear();
                requeued += 1;
            }
        }
        requeued
    }

    /// Publish an event on the attached sink, if any. Never fails — a
    /// closed receiver or missing sink is a no-op so telemetry can never
    /// break the orchestrator's control flow.
    fn emit(&self, event: OrchestratorEvent) {
        let Some(tx) = &self.event_sink else { return };
        let record = EventRecord {
            task_id: self.task_id,
            timestamp_ms: now_ms(),
            event,
        };
        let _ = tx.send(record);
    }

    /// Dispatch every subtask currently in `SubtaskStatus::Ready`.
    ///
    /// Parallelism contract: each Ready subtask becomes its own
    /// `tokio::task` via `joinset.spawn`. The scheduler does not wait
    /// between spawns — a DAG of `N` independent leaves all start
    /// dispatching in the same iteration of the outer loop. The outer
    /// `joinset.join_next().await` then races all in-flight workers and
    /// resumes scheduling the moment any one of them finishes, so newly
    /// satisfied dependencies become Ready while their siblings are
    /// still running. This is the "Environment is concurrent" property:
    /// agents are sequential, the orchestrator is not.
    ///
    /// Same-crate `cargo` contention (which would otherwise serialise
    /// in-process work behind `target/debug/.cargo-lock`) is mitigated
    /// at the *worker* layer via `phonton_sandbox::CrateLock`, not here
    /// — the orchestrator never reasons about file paths.
    fn schedule_ready(
        &self,
        graph: &DiGraph<SubtaskId, ()>,
        runtimes: &mut HashMap<SubtaskId, SubtaskRuntime>,
        joinset: &mut JoinSet<(SubtaskId, Result<SubtaskResult>)>,
        worker_msg_tx: tokio::sync::mpsc::Sender<OrchestratorMessage>,
    ) {
        // First: promote Queued → Ready for any subtask whose dependencies
        // have all reached Done.
        let ids: Vec<SubtaskId> = runtimes.keys().copied().collect();
        for id in &ids {
            let Some(rt) = runtimes.get(id) else { continue };
            if !matches!(rt.status, SubtaskStatus::Queued) {
                continue;
            }
            let node = rt.node;
            let deps_done = graph
                .neighbors_directed(node, petgraph::Direction::Incoming)
                .all(|dep_node| {
                    let dep_id = graph[dep_node];
                    runtimes.get(&dep_id).map(|r| r.is_done()).unwrap_or(false)
                });
            if deps_done {
                if let Some(rt) = runtimes.get_mut(id) {
                    rt.status = SubtaskStatus::Ready;
                }
            }
        }

        // Then: dispatch every Ready one.
        for id in &ids {
            let should_dispatch = runtimes
                .get(id)
                .map(|r| matches!(r.status, SubtaskStatus::Ready))
                .unwrap_or(false);
            if !should_dispatch {
                continue;
            }
            self.spawn_worker(runtimes, *id, joinset, worker_msg_tx.clone());
        }
    }

    fn spawn_worker(
        &self,
        runtimes: &mut HashMap<SubtaskId, SubtaskRuntime>,
        id: SubtaskId,
        joinset: &mut JoinSet<(SubtaskId, Result<SubtaskResult>)>,
        worker_msg_tx: tokio::sync::mpsc::Sender<OrchestratorMessage>,
    ) {
        let Some(rt) = runtimes.get_mut(&id) else {
            return;
        };
        rt.status = SubtaskStatus::Dispatched;
        rt.is_thinking = false;
        let subtask = rt.subtask.clone();
        let prior_errors = rt.prior_errors.clone();
        let attempt = rt.attempts_at_tier.saturating_add(1);
        let dispatcher = Arc::clone(&self.dispatcher);
        debug!(subtask = %id, tier = %subtask.model_tier, attempt, "dispatching worker");
        self.emit(OrchestratorEvent::SubtaskDispatched {
            subtask_id: id,
            tier: subtask.model_tier,
            attempt,
        });

        let tx = Some(worker_msg_tx);
        joinset.spawn(async move {
            let r = dispatcher
                .dispatch(subtask, prior_errors, attempt, tx)
                .await;
            (id, r)
        });
    }

    async fn handle_completion(
        &self,
        runtimes: &mut HashMap<SubtaskId, SubtaskRuntime>,
        id: SubtaskId,
        sr: SubtaskResult,
        joinset: &mut JoinSet<(SubtaskId, Result<SubtaskResult>)>,
        worker_msg_tx: tokio::sync::mpsc::Sender<OrchestratorMessage>,
    ) -> Result<()> {
        // Scoped borrow: update bookkeeping from the returned SubtaskResult
        // before calling verify (verify is async and takes no &mut self).
        let diff_hunks = {
            let rt = runtimes
                .get_mut(&id)
                .ok_or_else(|| anyhow!("unknown subtask id {id}"))?;
            let worker_tokens = match &sr.status {
                SubtaskStatus::Done { tokens_used, .. }
                | SubtaskStatus::Running {
                    tokens_so_far: tokens_used,
                    ..
                } => *tokens_used,
                _ => 0,
            };
            rt.tokens_used = rt.tokens_used.saturating_add(worker_tokens);
            rt.token_usage = sr.token_usage;
            rt.diff_hunks = sr.diff_hunks.clone();
            rt.provider = sr.provider;
            rt.model_name = sr.model_name.clone();

            // If the worker itself already surfaced a hard failure (no diff
            // to verify), don't re-verify an empty hunk set and mask it.
            if matches!(sr.status, SubtaskStatus::Failed { .. }) {
                let reason = failure_reason(&sr.status);
                let attempt = rt.attempts_at_tier.saturating_add(1);
                rt.status = SubtaskStatus::Failed {
                    reason: reason.clone(),
                    attempt,
                };
                self.emit(OrchestratorEvent::SubtaskFailed {
                    subtask_id: id,
                    reason,
                    attempt,
                });
                return Ok(());
            }

            sr.diff_hunks.clone()
        };

        // Hard rule: every worker diff passes through phonton-verify before
        // the orchestrator marks it Done. No bypass flags. When a memory
        // store is attached, Layer 1.5 (Decision Check) runs between
        // Syntax and CrateCheck — see `with_memory`.
        let verdict = phonton_verify::verify_diff_with_memory(
            &diff_hunks,
            &self.working_dir,
            self.memory.as_ref(),
        )
        .await?;
        self.apply_verdict(runtimes, id, verdict, joinset, worker_msg_tx)
    }

    fn apply_verdict(
        &self,
        runtimes: &mut HashMap<SubtaskId, SubtaskRuntime>,
        id: SubtaskId,
        verdict: VerifyResult,
        joinset: &mut JoinSet<(SubtaskId, Result<SubtaskResult>)>,
        worker_msg_tx: tokio::sync::mpsc::Sender<OrchestratorMessage>,
    ) -> Result<()> {
        let mut events: Vec<OrchestratorEvent> = Vec::new();
        let (redispatch, redispatch_fresh_tier) = {
            let rt = runtimes
                .get_mut(&id)
                .ok_or_else(|| anyhow!("unknown subtask id {id}"))?;
            match verdict {
                VerifyResult::Pass { layer } => {
                    events.push(OrchestratorEvent::VerifyPass {
                        subtask_id: id,
                        layer,
                    });
                    rt.status = SubtaskStatus::Done {
                        tokens_used: rt.tokens_used,
                        diff_hunk_count: rt.diff_hunks.len(),
                    };
                    events.push(OrchestratorEvent::SubtaskCompleted {
                        subtask_id: id,
                        tokens_used: rt.tokens_used,
                    });
                    events.push(OrchestratorEvent::SubtaskReviewReady {
                        subtask_id: id,
                        description: rt.subtask.description.clone(),
                        tier: rt.subtask.model_tier,
                        tokens_used: rt.tokens_used,
                        token_usage: rt.token_usage,
                        cost: self.cost_summary(rt.provider, &rt.model_name, rt.token_usage),
                        diff_hunks: rt.diff_hunks.clone(),
                        verify_result: VerifyResult::Pass { layer },
                        provider: rt.provider,
                        model_name: rt.model_name.clone(),
                    });
                    rt.prior_errors.clear();
                    (false, false)
                }
                VerifyResult::Fail { errors, layer, .. } => {
                    let attempt_for_event = rt.attempts_at_tier.saturating_add(1);
                    events.push(OrchestratorEvent::VerifyFail {
                        subtask_id: id,
                        layer,
                        errors: errors.clone(),
                        attempt: attempt_for_event,
                    });
                    rt.prior_errors = errors;
                    rt.attempts_at_tier = rt.attempts_at_tier.saturating_add(1);
                    if rt.attempts_at_tier >= MAX_RETRIES_PER_TIER {
                        let from_tier = rt.subtask.model_tier;
                        if !escalate(rt) {
                            // Already at max tier: terminal failure.
                            let reason = format!(
                                "verify failed at {}: {}",
                                rt.subtask.model_tier,
                                rt.prior_errors.join("; ")
                            );
                            rt.status = SubtaskStatus::Failed {
                                reason: reason.clone(),
                                attempt: rt.attempts_at_tier,
                            };
                            events.push(OrchestratorEvent::SubtaskFailed {
                                subtask_id: id,
                                reason,
                                attempt: rt.attempts_at_tier,
                            });
                            (false, false)
                        } else {
                            events.push(OrchestratorEvent::VerifyEscalated {
                                subtask_id: id,
                                from: from_tier,
                                to: rt.subtask.model_tier,
                                reason: "retry budget exhausted".into(),
                            });
                            (true, true)
                        }
                    } else {
                        // Re-dispatch at same tier with error context.
                        (true, false)
                    }
                }
                VerifyResult::Escalate { reason } => {
                    rt.prior_errors.push(reason.clone());
                    let from_tier = rt.subtask.model_tier;
                    if !escalate(rt) {
                        let msg = format!("escalation exhausted at max tier: {reason}");
                        rt.status = SubtaskStatus::Failed {
                            reason: msg.clone(),
                            attempt: rt.attempts_at_tier,
                        };
                        events.push(OrchestratorEvent::SubtaskFailed {
                            subtask_id: id,
                            reason: msg,
                            attempt: rt.attempts_at_tier,
                        });
                        (false, false)
                    } else {
                        events.push(OrchestratorEvent::VerifyEscalated {
                            subtask_id: id,
                            from: from_tier,
                            to: rt.subtask.model_tier,
                            reason,
                        });
                        (true, true)
                    }
                }
            }
        };
        for ev in events {
            self.emit(ev);
        }

        if redispatch {
            if redispatch_fresh_tier {
                debug!(subtask = %id, "re-dispatching at bumped tier");
            } else {
                debug!(subtask = %id, "re-dispatching at same tier with error context");
            }
            self.spawn_worker(runtimes, id, joinset, worker_msg_tx);
        }
        Ok(())
    }
}

/// Try to bump `rt`'s tier by one step. Returns `false` if already at the
/// ceiling. On success, resets the per-tier attempt counter.
fn escalate(rt: &mut SubtaskRuntime) -> bool {
    let next = next_tier(rt.subtask.model_tier);
    if next == rt.subtask.model_tier {
        return false;
    }
    if rt.escalations >= MAX_ESCALATIONS {
        return false;
    }
    debug!(
        subtask = %rt.subtask.id,
        from = %rt.subtask.model_tier,
        to = %next,
        "escalating tier"
    );
    rt.subtask.model_tier = next;
    rt.attempts_at_tier = 0;
    rt.escalations = rt.escalations.saturating_add(1);
    true
}

fn next_tier(t: ModelTier) -> ModelTier {
    match t {
        ModelTier::Local => ModelTier::Cheap,
        ModelTier::Cheap => ModelTier::Standard,
        ModelTier::Standard => ModelTier::Frontier,
        ModelTier::Frontier => ModelTier::Frontier,
    }
}

fn fail_subtask(runtimes: &mut HashMap<SubtaskId, SubtaskRuntime>, id: SubtaskId, reason: String) {
    if let Some(rt) = runtimes.get_mut(&id) {
        rt.status = SubtaskStatus::Failed {
            reason,
            attempt: rt.attempts_at_tier.saturating_add(1),
        };
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn failure_reason(status: &SubtaskStatus) -> String {
    match status {
        SubtaskStatus::Failed { reason, .. } => reason.clone(),
        _ => "unknown failure".into(),
    }
}

/// Fallback diff application for projects without a git repository.
/// Writes new files and applies simple line-based patches directly to disk.
/// Silently skips hunks whose parent directory can't be created.
fn apply_hunks_direct(hunks: &[phonton_types::DiffHunk], working_dir: &std::path::Path) {
    use phonton_types::DiffLine;
    use std::collections::BTreeMap;

    let mut by_file: BTreeMap<&std::path::Path, Vec<&phonton_types::DiffHunk>> = BTreeMap::new();
    for h in hunks {
        by_file.entry(&h.file_path).or_default().push(h);
    }

    for (rel_path, file_hunks) in by_file {
        let full = working_dir.join(rel_path);
        // Create parent dirs if needed.
        if let Some(parent) = full.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let is_new = file_hunks
            .iter()
            .all(|h| h.old_count == 0 && h.old_start == 0)
            || !full.exists();

        if is_new {
            // New file — reconstruct from Added lines.
            let content: String = file_hunks
                .iter()
                .flat_map(|h| h.lines.iter())
                .filter_map(|l| {
                    if let DiffLine::Added(s) = l {
                        Some(s.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let _ = std::fs::write(&full, content);
        } else {
            // Existing file — apply line patches naively.
            let Ok(original) = std::fs::read_to_string(&full) else {
                continue;
            };
            let mut out_lines: Vec<String> = original.lines().map(String::from).collect();
            // Apply hunks in reverse order so line offsets don't shift.
            let mut sorted = file_hunks.clone();
            sorted.sort_by_key(|h| std::cmp::Reverse(h.new_start));
            for hunk in sorted {
                let start = hunk.new_start.saturating_sub(1) as usize;
                let remove = hunk.old_count as usize;
                let added: Vec<String> = hunk
                    .lines
                    .iter()
                    .filter_map(|l| {
                        if let DiffLine::Added(s) = l {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                let end = (start + remove).min(out_lines.len());
                out_lines.splice(start..end, added);
            }
            let _ = std::fs::write(&full, out_lines.join("\n"));
        }
    }
}

// ---------------------------------------------------------------------------
// Graph construction
// ---------------------------------------------------------------------------

/// Build the subtask DAG and runtime map from a planner output.
///
/// Returns `Err` if a `dependencies` entry references an unknown subtask.
fn build_graph(
    plan: &PlannerOutput,
) -> Result<(DiGraph<SubtaskId, ()>, HashMap<SubtaskId, SubtaskRuntime>)> {
    let mut graph: DiGraph<SubtaskId, ()> = DiGraph::new();
    let mut runtimes: HashMap<SubtaskId, SubtaskRuntime> = HashMap::new();
    let mut index_of: HashMap<SubtaskId, NodeIndex> = HashMap::new();

    for subtask in &plan.subtasks {
        let node = graph.add_node(subtask.id);
        index_of.insert(subtask.id, node);
        // Auto-downgrade: classify the subtask once, and replace the
        // planner's tier with the cost-aware effective tier. The original
        // tier is logged for observability via the SubtaskDispatched
        // event but otherwise discarded.
        let class = classify_task(&subtask.description);
        let mut subtask = subtask.clone();
        let downgraded = effective_tier(subtask.model_tier, class);
        if downgraded != subtask.model_tier {
            debug!(
                subtask = %subtask.id,
                from = %subtask.model_tier,
                to = %downgraded,
                class = %class,
                "auto-downgrading subtask tier"
            );
        }
        subtask.model_tier = downgraded;
        runtimes.insert(subtask.id, SubtaskRuntime::new(subtask, node));
    }

    for subtask in &plan.subtasks {
        let child = *index_of
            .get(&subtask.id)
            .ok_or_else(|| anyhow!("missing index for {}", subtask.id))?;
        for dep in &subtask.dependencies {
            let parent = *index_of
                .get(dep)
                .ok_or_else(|| anyhow!("subtask {} depends on unknown {}", subtask.id, dep))?;
            graph.add_edge(parent, child, ());
        }
    }

    if petgraph::algo::is_cyclic_directed(&graph) {
        return Err(anyhow!("planner DAG contains a cycle"));
    }

    Ok((graph, runtimes))
}

// ---------------------------------------------------------------------------
// State broadcast
// ---------------------------------------------------------------------------

fn broadcast(
    tx: &watch::Sender<GlobalState>,
    task_status: &TaskStatus,
    runtimes: &HashMap<SubtaskId, SubtaskRuntime>,
    tokens_used: u64,
    tokens_budget: Option<u64>,
    estimated_naive_tokens: u64,
    checkpoints: &[Checkpoint],
) {
    let active_workers = runtimes
        .values()
        .filter(|r| r.is_active())
        .map(|r| WorkerState {
            subtask_id: r.subtask.id,
            subtask_description: r.subtask.description.clone(),
            model_tier: r.subtask.model_tier,
            tokens_used: r.tokens_used,
            status: r.status.clone(),
            is_thinking: r.is_thinking,
        })
        .collect();

    // send_replace never fails; if no receivers exist the update is still
    // recorded as the latest value, so a late-subscribing UI sees it.
    let _ = tx.send(GlobalState {
        task_status: task_status.clone(),
        active_workers,
        tokens_used,
        tokens_budget,
        estimated_naive_tokens,
        checkpoints: checkpoints.to_vec(),
    });
}

fn task_status_from(
    runtimes: &HashMap<SubtaskId, SubtaskRuntime>,
    _tokens_used: u64,
    _naive: u64,
) -> TaskStatus {
    let total = runtimes.len();
    let completed = runtimes.values().filter(|r| r.is_terminal()).count();
    let active_subtasks: Vec<SubtaskId> = runtimes
        .values()
        .filter(|r| r.is_active())
        .map(|r| r.subtask.id)
        .collect();
    TaskStatus::Running {
        active_subtasks,
        completed,
        total,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::{
        CoverageSummary, DiffHunk, DiffLine, Subtask, SubtaskId, SubtaskStatus, VerifyLayer,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Test dispatcher: produces a Pass-shaped SubtaskResult with a
    /// trivially valid Rust hunk. Records every dispatch.
    struct TrivialDispatcher {
        calls: Mutex<Vec<(SubtaskId, u8, ModelTier)>>,
    }

    impl TrivialDispatcher {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl WorkerDispatcher for TrivialDispatcher {
        async fn dispatch(
            &self,
            subtask: Subtask,
            _prior_errors: Vec<String>,
            attempt: u8,
            _msg_tx: Option<tokio::sync::mpsc::Sender<OrchestratorMessage>>,
        ) -> Result<SubtaskResult> {
            self.calls
                .lock()
                .map_err(|e| anyhow!("lock poisoned: {e}"))?
                .push((subtask.id, attempt, subtask.model_tier));
            let hunks = vec![DiffHunk {
                file_path: PathBuf::from("phonton-types/src/stub.rs"),
                old_start: 1,
                old_count: 0,
                new_start: 1,
                new_count: 1,
                lines: vec![DiffLine::Added(format!(
                    "fn ok_{}() -> u32 {{ 42 }}",
                    subtask.id.to_string().replace('-', "_")
                ))],
            }];
            Ok(SubtaskResult {
                id: subtask.id,
                status: SubtaskStatus::Done {
                    tokens_used: 100,
                    diff_hunk_count: hunks.len(),
                },
                diff_hunks: hunks,
                model_tier: subtask.model_tier,
                verify_result: VerifyResult::Pass {
                    layer: VerifyLayer::Syntax,
                },
                provider: ProviderKind::Anthropic,
                model_name: "test-model".into(),
                token_usage: TokenUsage {
                    input_tokens: 60,
                    output_tokens: 40,
                    ..TokenUsage::default()
                },
            })
        }
    }

    fn subtask(desc: &str, deps: Vec<SubtaskId>) -> Subtask {
        Subtask {
            id: SubtaskId::new(),
            description: desc.into(),
            model_tier: ModelTier::Cheap,
            dependencies: deps,
            status: SubtaskStatus::Queued,
        }
    }

    fn empty_state() -> watch::Sender<GlobalState> {
        let (tx, _rx) = watch::channel(GlobalState {
            task_status: TaskStatus::Queued,
            active_workers: Vec::new(),
            tokens_used: 0,
            tokens_budget: None,
            estimated_naive_tokens: 0,
            checkpoints: Vec::new(),
        });
        tx
    }

    fn temp_workspace() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("temp workspace");
        fs::create_dir_all(tmp.path().join("phonton-types/src")).expect("fixture dirs");
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"phonton-types\"]\n",
        )
        .expect("workspace manifest");
        fs::write(
            tmp.path().join("phonton-types/Cargo.toml"),
            "[package]\nname = \"phonton-types\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .expect("crate manifest");
        fs::write(
            tmp.path().join("phonton-types/src/lib.rs"),
            "pub mod stub;\n",
        )
        .expect("lib");
        fs::write(tmp.path().join("phonton-types/src/stub.rs"), "").expect("stub");
        tmp
    }

    #[tokio::test]
    async fn runs_linear_chain_in_order() {
        let a = subtask("first", vec![]);
        let b = subtask("second", vec![a.id]);
        let c = subtask("third", vec![b.id]);
        let plan = PlannerOutput {
            subtasks: vec![a.clone(), b.clone(), c.clone()],
            estimated_total_tokens: 0,
            naive_baseline_tokens: 0,
            coverage_summary: CoverageSummary::default(),
        };
        let dispatcher = Arc::new(TrivialDispatcher::new());
        let tmp = temp_workspace();
        let orch = Orchestrator::new(Arc::clone(&dispatcher)).with_working_dir(tmp.path());
        let status = orch.run_task(plan, empty_state()).await.unwrap();
        assert!(matches!(status, TaskStatus::Reviewing { .. }));
        let calls = dispatcher.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        // Linear: first dispatch must be a, then b, then c.
        assert_eq!(calls[0].0, a.id);
        assert_eq!(calls[1].0, b.id);
        assert_eq!(calls[2].0, c.id);
    }

    #[tokio::test]
    async fn independent_subtasks_both_dispatch() {
        let a = subtask("one", vec![]);
        let b = subtask("two", vec![]);
        let plan = PlannerOutput {
            subtasks: vec![a.clone(), b.clone()],
            estimated_total_tokens: 0,
            naive_baseline_tokens: 0,
            coverage_summary: CoverageSummary::default(),
        };
        let dispatcher = Arc::new(TrivialDispatcher::new());
        let tmp = temp_workspace();
        let orch = Orchestrator::new(Arc::clone(&dispatcher)).with_working_dir(tmp.path());
        let status = orch.run_task(plan, empty_state()).await.unwrap();
        assert!(matches!(status, TaskStatus::Reviewing { .. }));
        assert_eq!(dispatcher.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn escalates_on_persistent_verify_fail() {
        /// Dispatcher that always returns a syntactically broken hunk.
        struct BrokenDispatcher {
            calls: Mutex<Vec<ModelTier>>,
        }
        #[async_trait]
        impl WorkerDispatcher for BrokenDispatcher {
            async fn dispatch(
                &self,
                subtask: Subtask,
                _prior_errors: Vec<String>,
                _attempt: u8,
                _msg_tx: Option<tokio::sync::mpsc::Sender<OrchestratorMessage>>,
            ) -> Result<SubtaskResult> {
                self.calls.lock().unwrap().push(subtask.model_tier);
                let hunks = vec![DiffHunk {
                    file_path: PathBuf::from("phonton-types/src/stub.rs"),
                    old_start: 1,
                    old_count: 0,
                    new_start: 1,
                    new_count: 1,
                    lines: vec![DiffLine::Added("fn broken( -> {".into())],
                }];
                Ok(SubtaskResult {
                    id: subtask.id,
                    status: SubtaskStatus::Done {
                        tokens_used: 10,
                        diff_hunk_count: hunks.len(),
                    },
                    diff_hunks: hunks,
                    model_tier: subtask.model_tier,
                    verify_result: VerifyResult::Pass {
                        layer: VerifyLayer::Syntax,
                    },
                    provider: ProviderKind::Anthropic,
                    model_name: "test-broken".into(),
                    token_usage: TokenUsage {
                        input_tokens: 10,
                        ..TokenUsage::default()
                    },
                })
            }
        }

        let a = subtask("busted", vec![]);
        let plan = PlannerOutput {
            subtasks: vec![a.clone()],
            estimated_total_tokens: 0,
            naive_baseline_tokens: 0,
            coverage_summary: CoverageSummary::default(),
        };
        let dispatcher = Arc::new(BrokenDispatcher {
            calls: Mutex::new(Vec::new()),
        });
        let tmp = temp_workspace();
        let orch = Orchestrator::new(Arc::clone(&dispatcher)).with_working_dir(tmp.path());
        let status = orch.run_task(plan, empty_state()).await.unwrap();
        assert!(matches!(status, TaskStatus::Failed { .. }));
        let calls = dispatcher.calls.lock().unwrap();
        // At minimum: retries at initial tier plus at least one escalation.
        assert!(calls.len() >= 2);
        assert!(calls.iter().any(|t| *t != ModelTier::Cheap));
    }

    #[tokio::test]
    async fn independent_subtasks_overlap_in_time() {
        // Concurrency proof: two independent subtasks must be in flight
        // simultaneously. The dispatcher gates each call on a barrier
        // that only releases once both have entered — if dispatch were
        // sequential, the second call would never arrive and the test
        // would deadlock under the timeout.
        use tokio::time::{timeout, Duration};

        struct BarrierDispatcher {
            barrier: Arc<tokio::sync::Barrier>,
            calls: Mutex<Vec<SubtaskId>>,
        }

        #[async_trait]
        impl WorkerDispatcher for BarrierDispatcher {
            async fn dispatch(
                &self,
                subtask: Subtask,
                _prior_errors: Vec<String>,
                _attempt: u8,
                _msg_tx: Option<tokio::sync::mpsc::Sender<OrchestratorMessage>>,
            ) -> Result<SubtaskResult> {
                self.barrier.wait().await;
                self.calls.lock().unwrap().push(subtask.id);
                let hunks = vec![DiffHunk {
                    file_path: PathBuf::from("phonton-types/src/stub.rs"),
                    old_start: 1,
                    old_count: 0,
                    new_start: 1,
                    new_count: 1,
                    lines: vec![DiffLine::Added(format!(
                        "fn ok_{}() -> u32 {{ 42 }}",
                        subtask.id.to_string().replace('-', "_")
                    ))],
                }];
                Ok(SubtaskResult {
                    id: subtask.id,
                    status: SubtaskStatus::Done {
                        tokens_used: 10,
                        diff_hunk_count: hunks.len(),
                    },
                    diff_hunks: hunks,
                    model_tier: subtask.model_tier,
                    verify_result: VerifyResult::Pass {
                        layer: VerifyLayer::Syntax,
                    },
                    provider: ProviderKind::Anthropic,
                    model_name: "test-barrier".into(),
                    token_usage: TokenUsage {
                        input_tokens: 10,
                        ..TokenUsage::default()
                    },
                })
            }
        }

        let a = subtask("first", vec![]);
        let b = subtask("second", vec![]);
        let plan = PlannerOutput {
            subtasks: vec![a, b],
            estimated_total_tokens: 0,
            naive_baseline_tokens: 0,
            coverage_summary: CoverageSummary::default(),
        };
        let dispatcher = Arc::new(BarrierDispatcher {
            barrier: Arc::new(tokio::sync::Barrier::new(2)),
            calls: Mutex::new(Vec::new()),
        });
        let tmp = temp_workspace();
        let orch = Orchestrator::new(Arc::clone(&dispatcher)).with_working_dir(tmp.path());
        let status = timeout(Duration::from_secs(600), orch.run_task(plan, empty_state()))
            .await
            .expect("sequential dispatch would deadlock the barrier")
            .expect("orchestrator returned Err");
        assert!(matches!(status, TaskStatus::Reviewing { .. }));
        assert_eq!(dispatcher.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn budget_guard_pauses_when_token_ceiling_crossed() {
        let a = subtask("first", vec![]);
        let b = subtask("second", vec![a.id]);
        let plan = PlannerOutput {
            subtasks: vec![a.clone(), b.clone()],
            estimated_total_tokens: 0,
            naive_baseline_tokens: 0,
            coverage_summary: CoverageSummary::default(),
        };
        // TrivialDispatcher reports 100 tokens per call; ceiling at 50
        // must trip after the first completion.
        let dispatcher = Arc::new(TrivialDispatcher::new());
        let guard = BudgetGuard::new(BudgetLimits {
            max_tokens: Some(50),
            max_usd_micros: None,
        });
        let tmp = temp_workspace();
        let orch = Orchestrator::new(Arc::clone(&dispatcher))
            .with_working_dir(tmp.path())
            .with_budget_guard(guard);
        let status = orch.run_task(plan, empty_state()).await.unwrap();
        match status {
            TaskStatus::Paused { limit, .. } => {
                assert_eq!(limit, "tokens", "expected token ceiling to trip");
            }
            other => panic!("expected Paused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn auto_downgrades_test_subtask_to_cheap() {
        // Frontier-tier subtask whose description is clearly a test —
        // the orchestrator must downgrade it to Cheap before dispatch.
        let st = Subtask {
            id: SubtaskId::new(),
            description: "Write integration tests for FooBar".into(),
            model_tier: ModelTier::Frontier,
            dependencies: vec![],
            status: SubtaskStatus::Queued,
        };
        let plan = PlannerOutput {
            subtasks: vec![st.clone()],
            estimated_total_tokens: 0,
            naive_baseline_tokens: 0,
            coverage_summary: CoverageSummary::default(),
        };
        let dispatcher = Arc::new(TrivialDispatcher::new());
        let tmp = temp_workspace();
        let orch = Orchestrator::new(Arc::clone(&dispatcher)).with_working_dir(tmp.path());
        let _ = orch.run_task(plan, empty_state()).await.unwrap();
        let calls = dispatcher.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        // Recorded tier on dispatch must be the downgraded one.
        assert_eq!(calls[0].2, ModelTier::Cheap);
    }

    #[test]
    fn budget_guard_charge_trips_usd_ceiling() {
        let mut g = BudgetGuard::new(BudgetLimits {
            max_tokens: None,
            max_usd_micros: Some(1_000_000), // $1.00
        })
        .with_price(
            ProviderKind::Anthropic,
            "claude-sonnet-4-6",
            ModelPricing {
                input_usd_micros_per_mtok: 3_000_000, // $3 / Mtok
                output_usd_micros_per_mtok: 15_000_000,
            },
        );
        // 100k input tokens at $3/Mtok = $0.30 — under cap.
        let d = g.charge(ProviderKind::Anthropic, "claude-sonnet-4-6", 100_000, 0);
        assert!(matches!(d, BudgetDecision::Ok));
        // Add 50k output at $15/Mtok = $0.75 → $1.05 total → trips.
        let d = g.charge(ProviderKind::Anthropic, "claude-sonnet-4-6", 0, 50_000);
        assert!(matches!(d, BudgetDecision::Pause { .. }));
    }

    #[tokio::test]
    async fn rejects_cyclic_plan() {
        let id_a = SubtaskId::new();
        let id_b = SubtaskId::new();
        let a = Subtask {
            id: id_a,
            description: "a".into(),
            model_tier: ModelTier::Cheap,
            dependencies: vec![id_b],
            status: SubtaskStatus::Queued,
        };
        let b = Subtask {
            id: id_b,
            description: "b".into(),
            model_tier: ModelTier::Cheap,
            dependencies: vec![id_a],
            status: SubtaskStatus::Queued,
        };
        let plan = PlannerOutput {
            subtasks: vec![a, b],
            estimated_total_tokens: 0,
            naive_baseline_tokens: 0,
            coverage_summary: CoverageSummary::default(),
        };
        let dispatcher = Arc::new(TrivialDispatcher::new());
        let tmp = temp_workspace();
        let orch = Orchestrator::new(Arc::clone(&dispatcher)).with_working_dir(tmp.path());
        let r = orch.run_task(plan, empty_state()).await;
        assert!(r.is_err());
    }
}
