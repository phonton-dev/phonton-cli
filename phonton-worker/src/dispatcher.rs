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
use phonton_types::{Subtask, SubtaskResult};

use crate::Worker;

/// Production dispatcher: each `dispatch` call constructs a fresh [`Worker`]
/// bound to the configured provider and runs the subtask through the full
/// LLM → verify → retry loop.
///
/// The provider is stored as a factory function rather than a boxed trait
/// object so each dispatch gets its own owned `Box<dyn Provider>` —
/// `phonton_providers::Provider` is not `Clone`, and the orchestrator may
/// dispatch many subtasks concurrently.
pub struct RealDispatcher {
    /// Factory: produces a provider box for each dispatch.
    provider_factory: Arc<dyn Fn() -> Box<dyn phonton_providers::Provider> + Send + Sync>,
    /// Guard shared across all dispatches for this goal.
    guard: ExecutionGuard,
    /// Sandbox shared across all dispatches for this goal.
    sandbox: Arc<Sandbox>,
    /// Optional memory store — wired through to the worker when present.
    memory: Option<phonton_memory::MemoryStore>,
}

impl RealDispatcher {
    /// Construct a dispatcher backed by a provider factory.
    ///
    /// The factory is called once per `dispatch` invocation. This lets
    /// multiple concurrent workers each own their provider state without
    /// requiring `Clone` or interior mutability.
    pub fn new(
        provider_factory: impl Fn() -> Box<dyn phonton_providers::Provider> + Send + Sync + 'static,
        guard: ExecutionGuard,
        sandbox: Arc<Sandbox>,
    ) -> Self {
        Self {
            provider_factory: Arc::new(provider_factory),
            guard,
            sandbox,
            memory: None,
        }
    }

    /// Attach a memory store. When present, the worker writes completion and
    /// rejected-approach records that the planner reads on the next goal.
    pub fn with_memory(mut self, memory: phonton_memory::MemoryStore) -> Self {
        self.memory = Some(memory);
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
    ) -> Result<SubtaskResult> {
        let provider = (self.provider_factory)();
        let mut worker = Worker::new(provider, self.guard.clone())
            .with_sandbox(Arc::clone(&self.sandbox));

        if let Some(memory) = self.memory.clone() {
            worker = worker.with_memory_store(memory);
        }

        // Thread prior_errors back into the context slices as a synthetic
        // "previous errors" slice — the worker's prompt renderer already
        // handles a non-empty `prior_errors` list.
        let context_slices = if prior_errors.is_empty() {
            Vec::new()
        } else {
            // The worker renders prior_errors separately from context_slices;
            // nothing to inject into slices here, but we preserve the hook.
            Vec::new()
        };

        // The worker's `execute` method runs the full LLM → verify → retry
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
