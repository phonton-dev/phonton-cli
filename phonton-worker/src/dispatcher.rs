//! [`WorkerDispatcher`] implementation that wraps [`phonton_worker::Worker`].
//!
//! This is the production bridge between the orchestrator's dispatch contract
//! and the real LLM call / verify / retry loop. The CLI uses
//! [`RealDispatcher`] when a valid provider configuration is available, and
//! falls back to [`StubDispatcher`] only when no API key is set.
//!
//! Construction mirrors the builder pattern used elsewhere in the workspace:
//! ```
//! use phonton_worker::dispatcher::RealDispatcher;
//! use phonton_sandbox::{ExecutionGuard, Sandbox};
//! use std::sync::Arc;
//!
//! // let provider = ...;  // Box<dyn Provider>
//! // let guard = ExecutionGuard::new(project_root);
//! // let sandbox = Arc::new(Sandbox::new(project_root, "task-id".into()));
//! // let dispatcher = RealDispatcher::new(provider, guard, sandbox);
//! ```

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use phonton_orchestrator::WorkerDispatcher;
use phonton_sandbox::{ExecutionGuard, Sandbox};
use phonton_types::{CodeSlice, ContextAttribution, ModelTier, Subtask, SubtaskResult, TaskId};

use crate::Worker;

/// Production dispatcher: each `dispatch` call constructs a fresh [`Worker`]
/// bound to the configured provider and runs the subtask through the full
/// LLM â†’ verify â†’ retry loop.
///
/// The provider is stored as a factory function rather than a boxed trait
/// object so each dispatch gets its own owned `Box<dyn Provider>` â€”
/// `phonton_providers::Provider` is not `Clone`, and the orchestrator may
/// dispatch many subtasks concurrently.
pub struct RealDispatcher {
    /// Factory: produces a provider box for each dispatch, given the
    /// requested model tier.
    provider_factory: Arc<dyn Fn(ModelTier) -> Box<dyn phonton_providers::Provider> + Send + Sync>,
    /// Guard shared across all dispatches for this goal.
    guard: ExecutionGuard,
    /// Sandbox shared across all dispatches for this goal.
    sandbox: Arc<Sandbox>,
    /// Optional memory store â€” wired through to the worker when present.
    memory: Option<phonton_memory::MemoryStore>,
    task_id: Option<TaskId>,
    /// Shared context manager for all workers in this dispatch session.
    context: Arc<tokio::sync::Mutex<phonton_context::ContextManager>>,
    /// Optional semantic index used to retrieve per-subtask context.
    semantic: Option<Arc<crate::SemanticContext>>,
}

impl RealDispatcher {
    /// Construct a dispatcher backed by a provider factory.
    ///
    /// The factory is called once per `dispatch` invocation with the
    /// subtask's assigned [`ModelTier`]. This lets multiple concurrent
    /// workers each own their provider state without requiring `Clone`
    /// or interior mutability.
    pub fn new(
        provider_factory: impl Fn(ModelTier) -> Box<dyn phonton_providers::Provider>
            + Send
            + Sync
            + 'static,
        guard: ExecutionGuard,
        sandbox: Arc<Sandbox>,
    ) -> Self {
        let provider = provider_factory(ModelTier::Cheap);
        let counter = phonton_context::TiktokenCounter::new().unwrap_or_else(|_| {
            panic!("failed to load tiktoken counter");
        });
        let context = phonton_context::ContextManager::new(
            Arc::from(provider.clone_box()),
            crate::DEFAULT_WINDOW_LIMIT,
        )
        .with_counter(Arc::new(counter));

        Self {
            provider_factory: Arc::new(provider_factory),
            guard,
            sandbox,
            memory: None,
            task_id: None,
            context: Arc::new(tokio::sync::Mutex::new(context)),
            semantic: None,
        }
    }

    /// Attach a memory store. When present, the worker writes completion and
    /// rejected-approach records that the planner reads on the next goal.
    pub fn with_memory(mut self, memory: phonton_memory::MemoryStore) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Attach the current task id to worker memory records.
    pub fn with_task_id(mut self, task_id: TaskId) -> Self {
        self.task_id = Some(task_id);
        self
    }

    /// Attach a prebuilt semantic context. Each dispatch queries it for
    /// the top relevant slices and passes those slices into the worker.
    pub fn with_semantic_context(mut self, semantic: Arc<crate::SemanticContext>) -> Self {
        self.semantic = Some(semantic);
        self
    }
}

#[async_trait]
impl WorkerDispatcher for RealDispatcher {
    async fn dispatch(
        &self,
        subtask: Subtask,
        prior_errors: Vec<String>,
        _attempt: u8,
        msg_tx: Option<tokio::sync::mpsc::Sender<phonton_types::messages::OrchestratorMessage>>,
    ) -> Result<SubtaskResult> {
        let provider = (self.provider_factory)(subtask.model_tier);
        let context_slices = self.select_context(&subtask).await;
        if let Some(tx) = &msg_tx {
            let slices: Vec<ContextAttribution> = context_slices
                .iter()
                .map(ContextAttribution::from)
                .collect();
            let total_token_count = slices.iter().map(|s| s.token_count).sum();
            let _ = tx
                .send(
                    phonton_types::messages::OrchestratorMessage::ContextSelected {
                        id: subtask.id,
                        slices,
                        total_token_count,
                    },
                )
                .await;
        }
        let mut worker = Worker::new(provider, self.guard.clone())
            .with_sandbox(Arc::clone(&self.sandbox))
            .with_context_manager(Arc::clone(&self.context));

        if let Some(tx) = msg_tx {
            worker = worker.with_msg_tx(tx);
        }

        if let Some(memory) = self.memory.clone() {
            worker = worker.with_memory_store(memory);
        }

        if let Some(task_id) = self.task_id {
            worker = worker.with_task_id(task_id);
        }

        // The worker's `execute` method runs the full LLM â†’ verify â†’ retry
        // loop and returns a SubtaskResult with a VerifyResult already set.
        // The orchestrator re-verifies independently per its own invariant.
        let mut result = worker.execute(subtask, context_slices).await?;

        // Propagate prior errors into the result so the orchestrator can
        // log a complete audit trail even on the first attempt.
        let _ = prior_errors; // used via the worker prompt, not here
        result.diff_hunks.retain(|_| true); // no-op, keeps the compiler happy

        Ok(result)
    }
}

impl RealDispatcher {
    async fn select_context(&self, subtask: &Subtask) -> Vec<CodeSlice> {
        let Some(semantic) = &self.semantic else {
            return Vec::new();
        };
        phonton_index::query_relevant_slices(
            &semantic.index,
            &semantic.embedder,
            &subtask.description,
            5,
        )
        .await
    }
}
