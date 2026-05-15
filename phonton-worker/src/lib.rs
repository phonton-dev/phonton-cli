//! Single-subtask execution loop: LLM call + tool execution + diff output.
//!
//! The worker is the smallest functional unit that produces a verified
//! changeset. Its contract is narrow on purpose:
//!
//! 1. Build a prompt from the subtask + retrieved code slices.
//! 2. Call the assigned [`Provider`].
//! 3. Parse a unified-diff response into [`DiffHunk`]s.
//! 4. Hand them to [`phonton_verify::verify_diff`] **before doing anything
//!    with the result**.
//! 5. On `VerifyResult::Fail`, retry up to [`MAX_ATTEMPTS`] with the
//!    verifier errors threaded back into the prompt as additional context.
//!    On the final failure, escalate the [`ModelTier`] one notch and
//!    surface a `VerifyResult::Escalate` to the orchestrator.
//!
//! Two non-negotiable invariants:
//!
//! * **No unverified diff ever leaves this crate.** [`SubtaskResult`] always
//!   carries a [`VerifyResult`] that came from `phonton-verify`. The only
//!   path to `VerifyResult::Pass` is through `verify_diff` returning Pass.
//! * **No blocked tool call ever runs.** [`ExecutionGuard`] enforces the
//!   permission tiers from `01-architecture/failure-modes.md` Risk 4
//!   before any [`ToolCall`] is dispatched. `Block` is terminal — there is
//!   no override flag and no debug bypass.

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use phonton_context::{ContextCompiler, ContextManager, ContextPlanRequest, TiktokenCounter};
use phonton_mcp::{McpCallResult, McpRuntime, McpTool};
use phonton_providers::Provider;
use phonton_sandbox::Sandbox;
use phonton_store::Store;
use phonton_types::{
    classify_intent, CodeSlice, ContextFrame, ContextPlan, ContextPlanKind, DiffHunk, DiffLine,
    ExtensionId, MemoryRecord, ModelTier, Permission, PromptContextManifest, SliceOrigin, Subtask,
    SubtaskId, SubtaskResult, SubtaskStatus, TaskClass, TaskId, TokenUsage, VerifyLayer,
    VerifyResult,
};
use regex::Regex;
use serde_json::{json, Value};
use tracing::{debug, warn};

/// Re-exports of the guard / tool-call types. Canonical definitions now
/// live in `phonton-sandbox`; these aliases keep downstream `use
/// phonton_worker::ExecutionGuard` sites working.
pub use phonton_sandbox::{ExecutionGuard, GuardDecision, ToolCall};

/// Production [`WorkerDispatcher`] bridge from orchestrator to worker.
pub mod dispatcher;

/// Maximum verification attempts before stopping a worker at this tier.
pub const MAX_ATTEMPTS: u8 = 2;

/// Number of identical diagnostics that proves a blind retry is wasteful.
pub const SAME_DIAGNOSTIC_LIMIT: u8 = 2;

/// Maximum MCP tool calls a worker may make for one subtask.
pub const MAX_MCP_CALLS_PER_SUBTASK: usize = 3;

/// Maximum MCP result characters fed back into the model.
pub const MCP_RESULT_MAX_CHARS: usize = 1_500;

/// Default token limit for the context window.
pub const DEFAULT_WINDOW_LIMIT: usize = 120_000;

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

/// Single-subtask executor.
///
/// Owns one [`Provider`] handle and one [`ExecutionGuard`]. The provider
/// can be swapped by the caller between subtasks to implement model-tier
/// escalation; the guard is fixed for the lifetime of the worker.
pub struct Worker {
    provider: Box<dyn Provider>,
    guard: ExecutionGuard,
    /// Owns the isolated-execution path for [`ToolCall::Run`] and
    /// [`ToolCall::Bash`]. Construction from an existing `guard` reuses its
    /// project root verbatim, so guard and sandbox cannot disagree on
    /// filesystem scope.
    sandbox: Arc<Sandbox>,
    store: Option<Arc<Mutex<Store>>>,
    memory: Option<phonton_memory::MemoryStore>,
    task_id: Option<TaskId>,
    semantic: Option<Arc<SemanticContext>>,
    msg_tx: Option<tokio::sync::mpsc::Sender<phonton_types::messages::OrchestratorMessage>>,
    /// Context window manager.
    context: Arc<tokio::sync::Mutex<ContextManager>>,
    /// Optional MCP runtime used for attributed tool calls.
    mcp: Option<Arc<McpRuntime>>,
}

/// Bundle of the embedder + prebuilt index used to surface relevant
/// code slices in worker prompts.
pub struct SemanticContext {
    /// Embedding model shared across workers.
    pub embedder: phonton_index::Embedder,
    /// HNSW index of the current workspace.
    pub index: phonton_index::SemanticIndex,
}

impl Worker {
    /// Construct a worker bound to a provider and a permission guard. A
    /// sandbox scoped to the guard's project root is derived automatically;
    /// callers that need to share one across workers should use
    /// [`Worker::with_sandbox`] to override it after construction.
    pub fn new(provider: Box<dyn Provider>, guard: ExecutionGuard) -> Self {
        let sandbox = Arc::new(Sandbox::new(
            guard.project_root().to_path_buf(),
            "worker".to_string(),
        ));

        let counter = TiktokenCounter::new().unwrap_or_else(|_| {
            // Fallback to char heuristic if tiktoken fails to load.
            // In a real build this should be unwrapped.
            panic!("failed to load tiktoken counter");
        });

        let context = ContextManager::new(Arc::from(provider.clone_box()), DEFAULT_WINDOW_LIMIT)
            .with_counter(Arc::new(counter));

        Self {
            provider,
            guard,
            sandbox,
            store: None,
            memory: None,
            task_id: None,
            semantic: None,
            msg_tx: None,
            context: Arc::new(tokio::sync::Mutex::new(context)),
            mcp: None,
        }
    }

    /// Attach a message sender to the worker so it can emit intermediate
    /// telemetry (like "Thinking...") to the orchestrator.
    pub fn with_msg_tx(
        mut self,
        tx: tokio::sync::mpsc::Sender<phonton_types::messages::OrchestratorMessage>,
    ) -> Self {
        self.msg_tx = Some(tx);
        self
    }

    /// Replace the worker's sandbox with a caller-supplied one. Typically
    /// the orchestrator shares a single [`Sandbox`] across all workers so
    /// task-id-scoped state (temp dirs, job objects) is consistent.
    pub fn with_sandbox(mut self, sandbox: Arc<Sandbox>) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Attach a shared semantic context. When present, the worker
    /// prepends a task-budgeted set of HNSW-retrieved slices to the user
    /// prompt under a `# Relevant code` section.
    pub fn with_semantic_context(mut self, ctx: Arc<SemanticContext>) -> Self {
        self.semantic = Some(ctx);
        self
    }

    /// Attach the async memory facade. The worker keeps this available for
    /// future typed memory writes, but it does not record generic
    /// "completed task" memories because those pollute later prompts.
    pub fn with_memory_store(mut self, memory: phonton_memory::MemoryStore) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Attach a context manager to the worker.
    pub fn with_context_manager(
        mut self,
        context: Arc<tokio::sync::Mutex<ContextManager>>,
    ) -> Self {
        self.context = context;
        self
    }

    /// Attach the shared MCP runtime. Workers may request MCP calls only by
    /// emitting the explicit `MCP_TOOL_CALL` marker; the runtime handles
    /// permission checks, approval policy, execution, and audit events.
    pub fn with_mcp_runtime(mut self, runtime: Arc<McpRuntime>) -> Self {
        self.mcp = Some(runtime);
        self
    }

    /// Attach a memory store. When present, the worker writes a
    /// [`MemoryRecord::Decision`] for every new architectural symbol
    /// introduced by a subtask that reaches `VerifyResult::Pass`.
    pub fn with_store(mut self, store: Arc<Mutex<Store>>) -> Self {
        self.store = Some(store);
        self
    }

    /// Tag persisted decisions with the owning task id. Purely for
    /// traceability — memory without a task id is still valid.
    pub fn with_task_id(mut self, task_id: TaskId) -> Self {
        self.task_id = Some(task_id);
        self
    }

    /// Read-only view of the guard. Useful for orchestrator-side approval
    /// flows that want to double-check a decision before forwarding it.
    pub fn guard(&self) -> &ExecutionGuard {
        &self.guard
    }

    /// Execute a single tool call and return its textual output.
    ///
    /// The guard is consulted before dispatch; hard-blocked calls never run.
    /// Approval decisions are surfaced as tool output here; this low-level
    /// executor has no interactive approval channel of its own.
    pub async fn execute_tool(&self, call: ToolCall) -> Result<String> {
        match self.guard.evaluate(&call) {
            GuardDecision::Allow => {}
            GuardDecision::Approve { reason } => {
                return Ok(format!("[requires approval: {reason}]"));
            }
            GuardDecision::Block { reason } => {
                return Ok(format!("[blocked by sandbox policy: {reason}]"));
            }
        }

        match call {
            ToolCall::Read { path } => read_tool(self.guard.project_root(), &path).await,
            ToolCall::Write { path, content } => {
                write_tool(self.guard.project_root(), &path, &content).await
            }
            ToolCall::Run { .. } | ToolCall::Bash { .. } => {
                run_sandboxed(&self.sandbox, call).await
            }
            ToolCall::Network { url } => network_tool(&self.guard, &url).await,
        }
    }

    /// Execute one subtask end-to-end.
    ///
    /// Walks the verify-retry loop up to [`MAX_ATTEMPTS`]. The returned
    /// [`SubtaskResult::verify_result`] is whatever `phonton-verify`
    /// returned on the final attempt; if every attempt failed, that is a
    /// [`VerifyResult::Escalate`] carrying the last error set.
    pub async fn execute(
        &self,
        subtask: Subtask,
        context_slices: Vec<CodeSlice>,
    ) -> Result<SubtaskResult> {
        self.execute_with_prior_errors(subtask, context_slices, Vec::new())
            .await
    }

    /// Execute a subtask with verifier diagnostics from an earlier
    /// orchestrator attempt already available to the first worker prompt.
    pub async fn execute_with_prior_errors(
        &self,
        subtask: Subtask,
        context_slices: Vec<CodeSlice>,
        prior_errors: Vec<String>,
    ) -> Result<SubtaskResult> {
        let model_tier = subtask.model_tier;

        let relevant_slices: Vec<CodeSlice> = match &self.semantic {
            Some(ctx) => {
                phonton_index::query_relevant_slices(
                    &ctx.index,
                    &ctx.embedder,
                    &subtask.description,
                    semantic_slice_limit(&subtask),
                )
                .await
            }
            None => Vec::new(),
        };
        let mut last_errors: Vec<String> = prior_errors;
        let mut last_layer = VerifyLayer::Syntax;
        let mut last_signature: Option<String> = None;
        let mut total_tokens: u64 = 0;
        let mut token_usage = TokenUsage::default();
        let mut last_provider = phonton_types::ProviderKind::Anthropic;
        let mut last_model_name = String::new();
        let mut mcp_results: Vec<McpResultContext> = Vec::new();
        let mut mcp_calls = 0usize;
        let context_compiler = ContextCompiler::default();
        let started_with_prior_errors = !last_errors.is_empty();

        if let Some(hunks) =
            local_generated_artifact_seed_hunks(self.guard.project_root(), &subtask)
        {
            let verdict = phonton_verify::verify_diff(&hunks, self.guard.project_root()).await?;
            return match verdict {
                VerifyResult::Pass { layer } => Ok(SubtaskResult {
                    id: subtask.id,
                    status: SubtaskStatus::Done {
                        tokens_used: 0,
                        diff_hunk_count: hunks.len(),
                    },
                    diff_hunks: hunks,
                    model_tier,
                    verify_result: VerifyResult::Pass { layer },
                    provider: self.provider.kind(),
                    model_name: "local-template".into(),
                    token_usage,
                }),
                VerifyResult::Fail {
                    layer,
                    errors,
                    attempt,
                } => Ok(failed_result(
                    subtask.id,
                    model_tier,
                    VerifyResult::Fail {
                        layer,
                        errors: vec![format!(
                            "local generated-artifact seed failed verification: {}",
                            errors.join("; ")
                        )],
                        attempt,
                    },
                    token_usage,
                    attempt,
                    self.provider.kind(),
                    "local-template".into(),
                )),
                VerifyResult::Escalate { reason } => Ok(failed_result(
                    subtask.id,
                    model_tier,
                    VerifyResult::Escalate {
                        reason: format!("local generated-artifact seed failed: {reason}"),
                    },
                    token_usage,
                    1,
                    self.provider.kind(),
                    "local-template".into(),
                )),
            };
        }

        for attempt in 1..=MAX_ATTEMPTS {
            let prompt_policy = worker_prompt_policy(&subtask, !last_errors.is_empty());
            let system_prompt = system_prompt_for_attempt(&last_errors, self.mcp.is_some());
            let artifact_slices =
                current_artifact_context_slices(self.guard.project_root(), &subtask, &last_errors);
            let mut primary_slices =
                Vec::with_capacity(artifact_slices.len().saturating_add(context_slices.len()));
            primary_slices.extend(artifact_slices);
            primary_slices.extend(context_slices.clone());
            let repo_context = dedupe_code_slices(&primary_slices, &relevant_slices);

            if let Some(tx) = &self.msg_tx {
                let _ = tx.try_send(
                    phonton_types::messages::OrchestratorMessage::SubtaskThinking {
                        id: subtask.id,
                        model_name: self.provider.model(),
                    },
                );
            }

            // Render current context + new user prompt. We don't push the
            // user prompt into the manager until we get a successful
            // response, to avoid polluting the history with failed attempts
            // that will be superseded by the error-retry prompt.
            let (rendered_context, compacted_tokens, budget_limit) = {
                let mut ctx = self.context.lock().await;
                let budget_limit = Some(ctx.limit_tokens() as u64);
                let before_tokens = ctx.total_tokens();
                let compacted_tokens = if before_tokens >= ctx.compress_threshold() {
                    match ctx.compress_frames().await {
                        Ok(true) => before_tokens.saturating_sub(ctx.total_tokens()) as u64,
                        Ok(false) => 0,
                        Err(err) => {
                            warn!(error = %err, "context compaction failed before worker prompt");
                            0
                        }
                    }
                } else {
                    0
                };
                let rendered_context = ctx.render();
                (rendered_context, compacted_tokens, budget_limit)
            };
            let fixed_tokens = PromptFixedTokens {
                system: estimate_prompt_tokens(&system_prompt),
                memory: estimate_prompt_tokens(&rendered_context),
                attachments: estimate_prompt_tokens(&phonton_types::render_prompt_attachments(
                    &subtask.attachments,
                )),
                retry: estimate_prompt_tokens(&last_errors.join("\n")),
                mcp: estimate_prompt_tokens(&render_mcp_budget_text(
                    self.mcp.as_deref(),
                    &mcp_results,
                )),
            };
            let compiled_context = context_compiler.compile(ContextPlanRequest {
                goal: &subtask.description,
                candidate_slices: &repo_context.slices,
                system_tokens: fixed_tokens.system,
                memory_tokens: fixed_tokens.memory,
                attachment_tokens: fixed_tokens.attachments,
                retry_error_tokens: fixed_tokens.retry,
                mcp_tool_tokens: fixed_tokens.mcp,
                budget_limit,
                target_tokens: Some(prompt_policy.context_target_tokens),
                max_repo_map_items: prompt_policy.max_repo_map_items,
            });
            let user_prompt = render_user_prompt(
                &subtask,
                &compiled_context.selected_slices,
                Some(&compiled_context.plan),
                &last_errors,
                self.mcp.as_deref(),
                &mcp_results,
            );
            let full_prompt = render_full_prompt(&rendered_context, &user_prompt);
            let origins: Vec<SliceOrigin> = compiled_context
                .selected_slices
                .iter()
                .map(|s| s.origin)
                .collect();
            if let Some(tx) = &self.msg_tx {
                let manifest = prompt_context_manifest(PromptManifestInput {
                    system_prompt: &system_prompt,
                    subtask: &subtask,
                    rendered_context: &rendered_context,
                    repo_context: &compiled_context.selected_slices,
                    context_plan: Some(&compiled_context.plan),
                    prior_errors: last_errors.as_slice(),
                    mcp_results: mcp_results.as_slice(),
                    mcp: self.mcp.as_deref(),
                    deduped_tokens: repo_context.deduped_tokens,
                    compacted_tokens,
                    budget_limit,
                    attempt,
                    repair_attempt: !last_errors.is_empty(),
                });
                let _ = tx.try_send(
                    phonton_types::messages::OrchestratorMessage::PromptManifest {
                        id: subtask.id,
                        manifest,
                    },
                );
            }

            let response = match self
                .provider
                .call_with_attachments(&system_prompt, &full_prompt, &origins, &subtask.attachments)
                .await
            {
                Ok(response) => response,
                Err(err) if provider_contract_error(&err) => {
                    return Ok(failed_result(
                        subtask.id,
                        model_tier,
                        VerifyResult::Fail {
                            layer: VerifyLayer::Syntax,
                            errors: vec![format!(
                                "provider/model failed Phonton diff contract before repair: {err}"
                            )],
                            attempt,
                        },
                        token_usage,
                        attempt,
                        last_provider,
                        last_model_name,
                    ));
                }
                Err(err) => return Err(err),
            };
            last_provider = response.provider;
            last_model_name = response.model_name.clone();
            token_usage.add_response(&response);
            total_tokens = total_tokens
                .saturating_add(response.input_tokens)
                .saturating_add(response.output_tokens);

            if let Some(tx) = &self.msg_tx {
                let _ = tx.try_send(
                    phonton_types::messages::OrchestratorMessage::SubtaskProgress {
                        id: subtask.id,
                        tokens_so_far: total_tokens,
                    },
                );
            }

            match parse_mcp_tool_request(&response.content) {
                Ok(Some(request)) => {
                    let Some(runtime) = &self.mcp else {
                        last_errors = vec![
                            "MCP_TOOL_CALL was requested, but no MCP runtime is available for this run. \
                             Produce the unified diff directly or omit MCP usage."
                                .into(),
                        ];
                        continue;
                    };
                    if mcp_calls >= MAX_MCP_CALLS_PER_SUBTASK {
                        last_errors = vec![format!(
                            "MCP tool-call budget exhausted for this subtask ({MAX_MCP_CALLS_PER_SUBTASK} calls). \
                             Produce the unified diff using the gathered context."
                        )];
                        continue;
                    }
                    mcp_calls += 1;
                    let server_id = request.server_id.clone();
                    let tool_name = request.tool_name.clone();
                    let result = if is_mcp_tool_list_request(&tool_name) {
                        runtime
                            .list_tools(&server_id)
                            .await
                            .map(|tools| (true, render_mcp_tools(&tools)))
                    } else {
                        runtime
                            .call_tool(&server_id, &tool_name, request.arguments)
                            .await
                            .map(|result| (!result.is_error, render_mcp_result(&result)))
                    };
                    match result {
                        Ok((success, rendered)) => {
                            mcp_results.push(McpResultContext {
                                server_id,
                                tool_name,
                                success,
                                content: truncate_chars(
                                    &rendered,
                                    prompt_policy.mcp_result_max_chars,
                                ),
                            });
                            last_errors.clear();
                        }
                        Err(e) => {
                            mcp_results.push(McpResultContext {
                                server_id,
                                tool_name,
                                success: false,
                                content: truncate_chars(
                                    e.to_string(),
                                    prompt_policy.mcp_result_max_chars,
                                ),
                            });
                            last_errors = vec![format!(
                                "The requested MCP tool call failed or was denied: {e}. \
                                 Do not repeat the same request unless you can change it meaningfully."
                            )];
                        }
                    }
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    last_errors = vec![format!(
                        "Your MCP_TOOL_CALL request was malformed: {e}. \
                         Use exactly: MCP_TOOL_CALL {{\"server\":\"<server-id>\",\"tool\":\"<tool-name>\",\"arguments\":{{...}}}}"
                    )];
                    continue;
                }
            }

            let hunks = match parse_unified_diff(&response.content) {
                Ok(h) => h,
                Err(e) => {
                    warn!(attempt, error = %e, "worker could not parse diff; will retry with feedback");
                    let errors = vec![format!(
                        "Your previous response could not be parsed as a unified diff. \
                         Reply with ONLY a unified diff (no prose, no code fences). \
                         Each file starts with `--- a/<path>` then `+++ b/<path>`, \
                         followed by `@@ -old,n +new,n @@` hunk headers and \
                         ` `/`+`/`-` line prefixes. Parser said: {e}"
                    )];
                    let signature = diagnostic_signature(VerifyLayer::Syntax, &errors);
                    let repeated_signature_count =
                        update_repeated_signature_count(&mut last_signature, &signature);
                    last_errors = errors;
                    if attempt >= MAX_ATTEMPTS {
                        return Ok(SubtaskResult {
                            id: subtask.id,
                            status: SubtaskStatus::Failed {
                                reason: format!(
                                    "model returned unparseable output after {MAX_ATTEMPTS} attempts: {e}"
                                ),
                                attempt,
                            },
                            diff_hunks: Vec::new(),
                            model_tier,
                            verify_result: VerifyResult::Fail {
                                layer: VerifyLayer::Syntax,
                                errors: vec![e.to_string()],
                                attempt,
                            },
                            provider: last_provider,
                            model_name: last_model_name,
                            token_usage,
                        });
                    }
                    if repeated_signature_count >= SAME_DIAGNOSTIC_LIMIT {
                        return Ok(failed_result(
                            subtask.id,
                            model_tier,
                            VerifyResult::Fail {
                                layer: VerifyLayer::Syntax,
                                errors: vec![format!(
                                    "same parser diagnostic repeated {SAME_DIAGNOSTIC_LIMIT} times; stopped before blind retry: {signature}"
                                )],
                                attempt,
                            },
                            token_usage,
                            attempt,
                            last_provider,
                            last_model_name,
                        ));
                    }
                    continue;
                }
            };
            debug!(attempt, hunks = hunks.len(), "worker received diff");

            let verdict = phonton_verify::verify_diff(&hunks, self.guard.project_root()).await?;
            match verdict {
                VerifyResult::Pass { layer } => {
                    // Success! Record this exchange in the shared context manager.
                    // This is what allows subsequent subtasks to "remember"
                    // what this subtask did.
                    {
                        let mut ctx = self.context.lock().await;
                        ctx.push(ContextFrame::Summarizable {
                            content: compact_success_context(&subtask, &response.content, &hunks),
                            priority: 5, // SUMMARY_PRIORITY equivalent
                        })
                        .await?;
                    }

                    if let Err(e) = self.persist_decisions(&subtask) {
                        warn!(error = %e, "failed to persist subtask decisions");
                    }
                    if self.memory.is_some() {
                        debug!(
                            subtask = %subtask.id,
                            "skipping generic completion memory record"
                        );
                    }
                    return Ok(SubtaskResult {
                        id: subtask.id,
                        status: SubtaskStatus::Done {
                            tokens_used: total_tokens,
                            diff_hunk_count: hunks.len(),
                        },
                        diff_hunks: hunks,
                        model_tier,
                        verify_result: VerifyResult::Pass { layer },
                        provider: last_provider,
                        model_name: last_model_name,
                        token_usage,
                    });
                }
                VerifyResult::Fail {
                    layer,
                    errors,
                    attempt: _,
                } => {
                    last_layer = layer;
                    if should_stop_before_generated_app_syntax_repair(
                        &subtask,
                        layer,
                        &errors,
                        attempt,
                        started_with_prior_errors,
                    ) {
                        let mut fast_fail_errors = vec![format!(
                            "stopped before generated-app syntax repair to avoid token waste: {}",
                            diagnostic_signature(layer, &errors)
                        )];
                        fast_fail_errors.extend(compact_verify_retry_errors(layer, &errors));
                        return Ok(failed_result(
                            subtask.id,
                            model_tier,
                            VerifyResult::Fail {
                                layer,
                                errors: fast_fail_errors,
                                attempt,
                            },
                            token_usage,
                            attempt,
                            last_provider,
                            last_model_name,
                        ));
                    }
                    warn!(
                        attempt,
                        ?layer,
                        n = errors.len(),
                        "verify failed; will retry"
                    );
                    let signature = diagnostic_signature(layer, &errors);
                    let repeated_signature_count =
                        update_repeated_signature_count(&mut last_signature, &signature);
                    last_errors = compact_verify_retry_errors(layer, &errors);
                    last_errors.extend(repair_guidance_for_errors(&errors));
                    if repeated_signature_count >= SAME_DIAGNOSTIC_LIMIT {
                        return Ok(failed_result(
                            subtask.id,
                            model_tier,
                            VerifyResult::Fail {
                                layer,
                                errors: vec![format!(
                                    "same verifier diagnostic repeated {SAME_DIAGNOSTIC_LIMIT} times; stopped before blind retry: {signature}"
                                )],
                                attempt,
                            },
                            token_usage,
                            attempt,
                            last_provider,
                            last_model_name,
                        ));
                    }
                }
                VerifyResult::Escalate { reason } => {
                    return Ok(failed_result(
                        subtask.id,
                        model_tier,
                        VerifyResult::Escalate { reason },
                        token_usage,
                        attempt,
                        last_provider,
                        last_model_name,
                    ));
                }
            }
        }

        // Exhausted MAX_ATTEMPTS retries at the current tier. Stop with
        // evidence instead of spending another blind repair attempt.
        //
        // Record this as a RejectedApproach so the planner never re-proposes
        // the same subtask in a future goal decomposition.
        if let Some(memory) = &self.memory {
            let rec = MemoryRecord::RejectedApproach {
                summary: subtask.description.clone(),
                reason: format!(
                    "verify failed {} attempts: {}",
                    MAX_ATTEMPTS,
                    last_errors.join("; ")
                ),
            };
            if let Err(e) = memory.record(rec).await {
                warn!(error = %e, "failed to record rejected approach to memory");
            }
        }
        let reason = format!(
            "verify failed {MAX_ATTEMPTS} attempts at {model_tier}; stopped before blind retry: {}",
            last_errors.join("; ")
        );
        Ok(failed_result(
            subtask.id,
            model_tier,
            VerifyResult::Fail {
                layer: last_layer,
                errors: vec![reason],
                attempt: MAX_ATTEMPTS,
            },
            token_usage,
            MAX_ATTEMPTS,
            last_provider,
            last_model_name,
        ))
    }
}

async fn read_tool(root: &Path, path: &Path) -> Result<String> {
    let path = match canonicalize_existing(root, path).await {
        Ok(path) => path,
        Err(e) => return Ok(describe_io_error("read", path, &e)),
    };
    let root = match tokio::fs::canonicalize(root).await {
        Ok(root) => root,
        Err(e) => return Ok(describe_io_error("read", root, &e)),
    };
    if !path.starts_with(&root) {
        return Ok(format!(
            "refusing to read outside project root: {}",
            path.display()
        ));
    }
    match tokio::fs::read_to_string(&path).await {
        Ok(text) => Ok(truncate_with_notice(text, 8000)),
        Err(e) => Ok(describe_io_error("read", &path, &e)),
    }
}

async fn write_tool(root: &Path, path: &Path, content: &str) -> Result<String> {
    if path_has_parent_component(path) {
        return Ok(format!(
            "refusing to write path with `..`: {}",
            path.display()
        ));
    }

    let path = match canonicalize_for_write(root, path).await {
        Ok(path) => path,
        Err(e) => return Ok(describe_io_error("write", path, &e)),
    };
    let root = match tokio::fs::canonicalize(root).await {
        Ok(root) => root,
        Err(e) => return Ok(describe_io_error("write", root, &e)),
    };
    if !path.starts_with(&root) {
        return Ok(format!(
            "refusing to write outside project root: {}",
            path.display()
        ));
    }
    let tmp_path = tmp_path_for(&path);

    if let Err(e) = tokio::fs::write(&tmp_path, content).await {
        return Ok(describe_io_error("write", &tmp_path, &e));
    }
    if let Err(e) = tokio::fs::rename(&tmp_path, &path).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Ok(describe_io_error("rename", &path, &e));
    }

    Ok(format!(
        "wrote {} bytes to {}",
        content.len(),
        path.display()
    ))
}

/// Route a [`ToolCall::Run`] or [`ToolCall::Bash`] through the sandbox and
/// fold the captured [`std::process::Output`] into the worker's textual
/// contract: stdout ++ stderr, truncated at 4000 chars. Guard failures
/// surface as human-readable strings rather than errors so the worker can
/// keep iterating.
async fn run_sandboxed(sandbox: &Sandbox, call: ToolCall) -> Result<String> {
    match sandbox.run_tool(call).await {
        Ok(output) => {
            let mut combined = String::new();
            combined.push_str(&String::from_utf8_lossy(&output.stdout));
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
            Ok(truncate_chars(combined, 4000))
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.starts_with("BLOCKED") {
                Ok(format!("[blocked by sandbox policy: {msg}]"))
            } else if msg.starts_with("Approval required") {
                Ok(format!("[requires approval: {msg}]"))
            } else if msg.contains("timed out") {
                Ok("[timed out after 30s]".into())
            } else {
                Ok(format!("sandbox execution failed: {msg}"))
            }
        }
    }
}

/// HTTP GET a URL and return the response body. POST/PUT/DELETE are refused
/// in v1 — the tool is read-only to prevent unintended side effects.
async fn network_tool(guard: &ExecutionGuard, url: &str) -> Result<String> {
    let call = ToolCall::Network {
        url: url.to_string(),
    };
    if let GuardDecision::Block { reason } = guard.evaluate(&call) {
        return Ok(format!("[blocked by sandbox policy: {reason}]"));
    }
    // Approval-gated calls also block here — the v1 sandbox has no
    // interactive approval channel, so we err on the side of caution.
    if let GuardDecision::Approve { reason } = guard.evaluate(&call) {
        return Ok(format!("[requires approval: {reason}]"));
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| anyhow!("failed to build HTTP client: {e}"))?;

    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => return Ok(format!("[HTTP request failed: {e}]")),
    };

    let status = resp.status();
    let body = match resp.text().await {
        Ok(t) => truncate_with_notice(t, 8000),
        Err(e) => format!("[failed to read response body: {e}]"),
    };
    Ok(format!("[HTTP {status}]\n{body}"))
}

async fn canonicalize_existing(root: &Path, path: &Path) -> std::io::Result<PathBuf> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    tokio::fs::canonicalize(candidate).await
}

async fn canonicalize_for_write(root: &Path, path: &Path) -> std::io::Result<PathBuf> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };

    if tokio::fs::metadata(&candidate).await.is_ok() {
        return tokio::fs::canonicalize(candidate).await;
    }

    let parent = candidate.parent().unwrap_or(root);
    let parent = tokio::fs::canonicalize(parent).await?;
    match candidate.file_name() {
        Some(file_name) => Ok(parent.join(file_name)),
        None => Ok(parent),
    }
}

fn path_has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".phonton_tmp");
    PathBuf::from(tmp)
}

fn truncate_with_notice(text: String, limit: usize) -> String {
    let total = text.chars().count();
    if total <= limit {
        return text;
    }
    let mut truncated: String = text.chars().take(limit).collect();
    truncated.push_str(&format!("\n[truncated — {total} chars total]"));
    truncated
}

fn truncate_chars(text: impl AsRef<str>, limit: usize) -> String {
    let text = text.as_ref();
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect()
}

fn describe_io_error(op: &str, path: &Path, err: &std::io::Error) -> String {
    match err.kind() {
        std::io::ErrorKind::NotFound => {
            format!("{op} failed: file not found: {}", path.display())
        }
        std::io::ErrorKind::PermissionDenied => {
            format!("{op} failed: permission denied: {}", path.display())
        }
        _ => format!("{op} failed for {}: {err}", path.display()),
    }
}

impl Worker {
    /// Inspect `subtask` for new architectural symbols and append a
    /// [`MemoryRecord::Decision`] per match to the attached store.
    ///
    /// Noop when no store is attached or when the subtask description
    /// contains no recognisable new symbols. The current detector is the
    /// same conservative regex the planner uses — it fires on verbs like
    /// `add`/`create`/`implement` immediately followed by a kind word
    /// (`trait`, `struct`, `module`, `type`). Renames, fixes, and
    /// documentation edits deliberately do not produce memory entries.
    fn persist_decisions(&self, subtask: &Subtask) -> Result<()> {
        let Some(store) = self.store.as_ref() else {
            return Ok(());
        };
        let decisions = detect_decisions(subtask, self.task_id);
        if decisions.is_empty() {
            return Ok(());
        }
        let guard = store
            .lock()
            .map_err(|e| anyhow!("memory store mutex poisoned: {e}"))?;
        for rec in &decisions {
            guard.append_memory(rec)?;
        }
        debug!(
            count = decisions.len(),
            subtask = %subtask.id,
            "persisted architectural decisions to memory"
        );
        Ok(())
    }
}

/// Extract zero or more [`MemoryRecord::Decision`]s from `subtask`'s
/// description. Public so external tooling (e.g. a future LLM-reflection
/// pass) can compose with the default heuristic.
///
/// Only architectural kinds trigger records: `trait`, `struct`, `enum`,
/// `module`, `type`. A `function` addition is not, by itself, an
/// architectural decision — workers add functions routinely.
pub fn detect_decisions(subtask: &Subtask, task_id: Option<TaskId>) -> Vec<MemoryRecord> {
    let re = match Regex::new(
        r"(?ix)
        \b(?:add|create|implement|introduce|define|build)\b
        [^\.\n]{0,40}?
        \b(?P<kind>trait|struct|enum|module|type)\b
        [\s:`'(]*
        (?P<name>[A-Za-z_][A-Za-z0-9_]*)
        ",
    ) {
        Ok(re) => re,
        Err(_) => return Vec::new(),
    };

    let mut seen: Vec<(String, String)> = Vec::new();
    for caps in re.captures_iter(&subtask.description) {
        let kind = caps["kind"].to_ascii_lowercase();
        let name = caps["name"].to_string();
        if seen.iter().any(|(k, n)| k == &kind && n == &name) {
            continue;
        }
        seen.push((kind, name));
    }

    seen.into_iter()
        .map(|(kind, name)| MemoryRecord::Decision {
            title: format!("introduced {kind} {name}"),
            body: subtask.description.clone(),
            task_id,
        })
        .collect()
}

fn provider_contract_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("empty output")
        || msg.contains("empty content")
        || msg.contains("missing model text content")
        || msg.contains("diff contract")
}

/// Shape a `Failed` [`SubtaskResult`] with no diffs and the supplied verdict.
fn failed_result(
    id: SubtaskId,
    model_tier: ModelTier,
    verify_result: VerifyResult,
    token_usage: TokenUsage,
    attempt: u8,
    provider: phonton_types::ProviderKind,
    model_name: String,
) -> SubtaskResult {
    let reason = match &verify_result {
        VerifyResult::Escalate { reason } => reason.clone(),
        VerifyResult::Fail { errors, .. } => errors.join("; "),
        VerifyResult::Pass { .. } => "unexpected Pass in failed_result".into(),
    };
    SubtaskResult {
        id,
        status: SubtaskStatus::Failed { reason, attempt },
        diff_hunks: Vec::new(),
        model_tier,
        verify_result,
        provider,
        model_name,
        token_usage,
    }
}

/// Normalize verifier output so retry policy can detect repeated failures.
fn diagnostic_signature(layer: VerifyLayer, errors: &[String]) -> String {
    let joined = errors.join(" ").to_ascii_lowercase();
    let without_attempts = match Regex::new(r"\b\d+\b") {
        Ok(re) => re.replace_all(&joined, "#").into_owned(),
        Err(_) => joined,
    };
    let collapsed = without_attempts
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut signature = format!("{layer:?}:{collapsed}").to_ascii_lowercase();
    if signature.chars().count() > 240 {
        signature = signature.chars().take(240).collect();
    }
    signature
}

fn update_repeated_signature_count(last_signature: &mut Option<String>, signature: &str) -> u8 {
    if last_signature.as_deref() == Some(signature) {
        2
    } else {
        *last_signature = Some(signature.to_string());
        1
    }
}

fn should_stop_before_generated_app_syntax_repair(
    subtask: &Subtask,
    layer: VerifyLayer,
    errors: &[String],
    attempt: u8,
    started_with_prior_errors: bool,
) -> bool {
    if attempt != 1 || started_with_prior_errors || !matches!(layer, VerifyLayer::Syntax) {
        return false;
    }
    if !matches!(
        classify_intent(&subtask.description).task_class,
        TaskClass::GeneratedAppGame
    ) {
        return false;
    }
    let diagnostics = errors.join("\n").to_ascii_lowercase();
    diagnostics.contains("typescript")
        || diagnostics.contains("tsx")
        || diagnostics.contains("jsx")
        || diagnostics.contains("javascript")
        || diagnostics.contains("html")
        || diagnostics.contains("src/app")
}

fn repair_guidance_for_errors(errors: &[String]) -> Vec<String> {
    if !looks_like_stale_hunk_error(errors) {
        return Vec::new();
    }
    vec![
        "Repair policy: the previous patch appears stale. Do not retry the same hunk. \
         For a small generated artifact, replace the whole file with one unified-diff hunk; \
         otherwise use only exact current surrounding lines from the failing file."
            .into(),
    ]
}

fn looks_like_stale_hunk_error(errors: &[String]) -> bool {
    let joined = errors.join(" ").to_ascii_lowercase();
    joined.contains("removed-line mismatch")
        || joined.contains("could not reconstruct post-diff file")
        || joined.contains("hunk starts at old line")
        || joined.contains("git apply failed")
}

/// One-step model-tier escalation table. `Frontier` already at the top
/// returns itself — orchestrator inspects this and decides whether to
/// surface the failure to the user.
#[cfg(test)]
fn next_tier(t: ModelTier) -> ModelTier {
    match t {
        ModelTier::Local => ModelTier::Cheap,
        ModelTier::Cheap => ModelTier::Standard,
        ModelTier::Standard => ModelTier::Frontier,
        ModelTier::Frontier => ModelTier::Frontier,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DedupeCodeContext {
    slices: Vec<CodeSlice>,
    deduped_tokens: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PromptFixedTokens {
    system: u64,
    memory: u64,
    attachments: u64,
    retry: u64,
    mcp: u64,
}

fn dedupe_code_slices(primary: &[CodeSlice], secondary: &[CodeSlice]) -> DedupeCodeContext {
    let mut seen = HashSet::new();
    let mut slices = Vec::with_capacity(primary.len().saturating_add(secondary.len()));
    let mut deduped_tokens = 0u64;

    for slice in primary.iter().chain(secondary.iter()) {
        let key = (
            slice.file_path.clone(),
            slice.symbol_name.clone(),
            slice.signature.clone(),
        );
        if seen.insert(key) {
            slices.push(slice.clone());
        } else {
            deduped_tokens = deduped_tokens.saturating_add(slice.token_count as u64);
        }
    }

    DedupeCodeContext {
        slices,
        deduped_tokens,
    }
}

const CURRENT_ARTIFACT_CONTEXT_MAX_CHARS: usize = 3_600;

fn current_artifact_context_slices(
    root: &Path,
    subtask: &Subtask,
    prior_errors: &[String],
) -> Vec<CodeSlice> {
    let mut paths = artifact_paths_from_subtask(&subtask.description);
    for path in artifact_paths_from_errors(prior_errors) {
        if !paths.iter().any(|existing| existing == &path) {
            paths.push(path);
        }
    }
    paths
        .into_iter()
        .filter_map(|path| current_artifact_context_slice(root, path))
        .collect()
}

fn local_generated_artifact_seed_hunks(root: &Path, subtask: &Subtask) -> Option<Vec<DiffHunk>> {
    if !is_existing_vite_chess_rules_seed(&subtask.description) {
        return None;
    }
    let rules = PathBuf::from("src/chessRules.ts");
    let tests = PathBuf::from("src/chessRules.test.ts");
    Some(vec![
        template_file_hunk(root, rules, include_str!("templates/chessRules.ts")),
        template_file_hunk(root, tests, include_str!("templates/chessRules.test.ts")),
    ])
}

fn is_existing_vite_chess_rules_seed(description: &str) -> bool {
    let lower = description.to_ascii_lowercase();
    lower.contains("existing vite react chess app")
        && lower.contains("src/chessrules.ts")
        && lower.contains("src/chessrules.test.ts")
        && (lower.contains("rules seed")
            || lower.contains("compile-safe local chess")
            || lower.contains("local game-state/rules boundary"))
}

fn template_file_hunk(root: &Path, path: PathBuf, content: &str) -> DiffHunk {
    let old_lines = std::fs::read_to_string(root.join(&path))
        .ok()
        .map(|content| {
            content
                .trim_end()
                .lines()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let new_lines = content
        .trim_end()
        .lines()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    let lines = old_lines
        .iter()
        .map(|line| DiffLine::Removed(line.clone()))
        .chain(new_lines.iter().map(|line| DiffLine::Added(line.clone())))
        .collect::<Vec<_>>();
    DiffHunk {
        file_path: path,
        old_start: if old_lines.is_empty() { 0 } else { 1 },
        old_count: old_lines.len() as u32,
        new_start: 1,
        new_count: new_lines.len() as u32,
        lines,
    }
}

fn current_artifact_context_slice(root: &Path, path: PathBuf) -> Option<CodeSlice> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return None;
    }
    let full = root.join(&path);
    let content = std::fs::read_to_string(&full).ok()?;
    let compact = compact_current_artifact(&content, CURRENT_ARTIFACT_CONTEXT_MAX_CHARS);
    let signature = format!(
        "Patch against this exact current file. If replacing broadly, use a hunk that matches these current lines.\n{}",
        compact
    );
    Some(CodeSlice {
        file_path: path,
        symbol_name: "current artifact snapshot".into(),
        signature,
        docstring: None,
        callsites: Vec::new(),
        token_count: estimate_prompt_tokens(&content).min(900) as usize,
        origin: SliceOrigin::Fallback,
    })
}

fn artifact_paths_from_subtask(description: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for marker in ["Artifact:", "Artifacts:"] {
        let mut rest = description;
        while let Some(idx) = rest.find(marker) {
            rest = &rest[idx + marker.len()..];
            let sentence_end = rest.find(". ").map(|idx| idx + 1);
            let newline_end = rest.find('\n');
            let end = match (sentence_end, newline_end) {
                (Some(a), Some(b)) => a.min(b),
                (Some(a), None) | (None, Some(a)) => a,
                (None, None) => rest.len(),
            };
            let raw = &rest[..end];
            for part in raw.split([',', ';']) {
                let cleaned = part
                    .trim()
                    .trim_end_matches('.')
                    .trim_matches('`')
                    .trim_matches('"')
                    .trim_matches('\'');
                if cleaned.is_empty() || cleaned.contains(' ') {
                    continue;
                }
                let path = PathBuf::from(cleaned);
                if !paths.iter().any(|existing| existing == &path) {
                    paths.push(path);
                }
            }
            rest = &rest[end..];
        }
    }
    paths
}

fn artifact_paths_from_errors(errors: &[String]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for error in errors {
        for token in error.split_whitespace() {
            let cleaned = token
                .trim_matches('`')
                .trim_matches('"')
                .trim_matches('\'')
                .trim_matches('[')
                .trim_matches(']')
                .trim_matches('(')
                .trim_matches(')')
                .trim_end_matches(':')
                .trim_end_matches(',')
                .trim_end_matches(';');
            if !looks_like_relative_artifact_path(cleaned) {
                continue;
            }
            let path = PathBuf::from(cleaned.replace('\\', "/"));
            if !paths.iter().any(|existing| existing == &path) {
                paths.push(path);
            }
        }
    }
    paths
}

fn looks_like_relative_artifact_path(value: &str) -> bool {
    if value.is_empty() || value.contains("://") {
        return false;
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return false;
    }
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some(
            "css"
                | "html"
                | "js"
                | "jsx"
                | "json"
                | "py"
                | "rs"
                | "toml"
                | "ts"
                | "tsx"
                | "yaml"
                | "yml"
        )
    )
}

fn compact_current_artifact(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        return content.to_string();
    }
    let head_chars = max_chars.saturating_mul(3) / 4;
    let tail_chars = max_chars.saturating_sub(head_chars);
    let head: String = content.chars().take(head_chars).collect();
    let tail: String = content
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!(
        "{head}\n/* ... middle omitted for token budget; prefer small local hunks ... */\n{tail}"
    )
}

fn compact_success_context(
    subtask: &Subtask,
    response_content: &str,
    hunks: &[DiffHunk],
) -> String {
    let mut by_file: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    for hunk in hunks {
        let entry = by_file
            .entry(hunk.file_path.display().to_string())
            .or_default();
        entry.0 = entry.0.saturating_add(1);
        for line in &hunk.lines {
            match line {
                DiffLine::Added(_) => entry.1 = entry.1.saturating_add(1),
                DiffLine::Removed(_) => entry.2 = entry.2.saturating_add(1),
                DiffLine::Context(_) => {}
            }
        }
    }
    let mut out = format!(
        "Completed subtask: {}\nWorker diff was verified and applied; future slices must patch current workspace files, not replay this diff.\nModel output chars: {}\nChanged files:",
        truncate_chars(&subtask.description, 240),
        response_content.chars().count()
    );
    if by_file.is_empty() {
        out.push_str("\n- none");
    } else {
        for (path, (hunks, added, removed)) in by_file {
            out.push_str(&format!("\n- {path}: {hunks} hunk(s), +{added}/-{removed}"));
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerPromptPolicy {
    context_target_tokens: u64,
    semantic_top_k: usize,
    max_repo_map_items: usize,
    mcp_result_max_chars: usize,
}

fn worker_prompt_policy(subtask: &Subtask, repair_attempt: bool) -> WorkerPromptPolicy {
    let class = classify_intent(&subtask.description).task_class;
    let (context_target_tokens, semantic_top_k, max_repo_map_items, mcp_result_max_chars) =
        match (class, repair_attempt) {
            (TaskClass::GeneratedAppGame, true) => (900, 1, 2, 900),
            (TaskClass::GeneratedAppGame, false) => (1_200, 1, 2, 1_000),
            (
                TaskClass::Boilerplate
                | TaskClass::Tests
                | TaskClass::Docs
                | TaskClass::TestGeneration,
                true,
            ) => (650, 1, 1, 800),
            (
                TaskClass::Boilerplate
                | TaskClass::Tests
                | TaskClass::Docs
                | TaskClass::TestGeneration,
                false,
            ) => (800, 2, 2, 1_000),
            (TaskClass::BugFix, true) => (900, 2, 2, 900),
            (TaskClass::BugFix, false) => (1_800, 3, 3, 1_200),
            (TaskClass::ExistingProjectFeature, true) => (1_200, 3, 3, 1_200),
            (TaskClass::ExistingProjectFeature, false) => (2_200, 4, 4, 1_500),
            (TaskClass::Refactor, true) => (1_800, 4, 4, 1_200),
            (TaskClass::Refactor, false) => (3_200, 5, 5, 1_500),
            (TaskClass::ReleaseCheck, true) => (800, 2, 2, 900),
            (TaskClass::ReleaseCheck, false) => (1_200, 3, 3, 1_200),
            (TaskClass::CoreLogic, true) => (1_200, 3, 3, 1_200),
            (TaskClass::CoreLogic, false) => (2_200, 5, 5, 1_500),
        };

    WorkerPromptPolicy {
        context_target_tokens,
        semantic_top_k,
        max_repo_map_items,
        mcp_result_max_chars: mcp_result_max_chars.min(MCP_RESULT_MAX_CHARS),
    }
}

pub(crate) fn semantic_slice_limit(subtask: &Subtask) -> usize {
    worker_prompt_policy(subtask, false).semantic_top_k
}

#[cfg(test)]
fn dynamic_worker_context_target(subtask: &Subtask, prior_errors: &[String]) -> u64 {
    worker_prompt_policy(subtask, !prior_errors.is_empty()).context_target_tokens
}

// ---------------------------------------------------------------------------
// Prompt rendering and diff parsing
// ---------------------------------------------------------------------------

/// Base system prompt. Pinned `Verbatim` in the worker's context window —
/// Sent only in the provider system slot so it is not duplicated inside the
/// user-context render. The diff-only constraint is hard-coded here because
/// review and verification expect parseable unified diffs.
#[cfg(test)]
fn base_system_prompt() -> String {
    system_prompt_for_attempt(&[], false)
}

fn system_prompt_for_attempt(prior_errors: &[String], mcp_enabled: bool) -> String {
    let lower_errors = prior_errors.join("\n").to_ascii_lowercase();
    let include_diff_example = lower_errors.contains("diff")
        || lower_errors.contains("parse")
        || lower_errors.contains("unified")
        || lower_errors.contains("no hunks");
    render_system_prompt(include_diff_example, mcp_enabled)
}

fn render_system_prompt(include_diff_example: bool, mcp_enabled: bool) -> String {
    let mut out = String::from(
        "You are a Phonton worker. Produce a single parseable unified diff only.\n\
         No prose, no markdown fences, no commentary, no explanations.\n\
         Start with `--- a/` or `--- /dev/null`; output empty text only when there is nothing to change.\n",
    );
    out.push_str(
        "Minimize tokens: produce the smallest runnable diff that satisfies the acceptance criteria. \
         Avoid decorative comments, large rewrites, duplicated helpers, and unrelated files. \
         For generated examples, prefer concise implementations over exhaustive frameworks.\n",
    );
    if mcp_enabled {
        out.push_str(
            "If the user prompt lists MCP servers, you may output exactly one MCP_TOOL_CALL JSON marker instead of a diff. After MCP results are provided, return to unified-diff output.\n",
        );
    }
    if include_diff_example {
        out.push_str(
            "\nEXAMPLE OF CREATING A NEW FILE `main.c`:\n\
             --- /dev/null\n\
             +++ b/main.c\n\
             @@ -0,0 +1,1 @@\n\
             +int main() { return 0; }\n",
        );
    }
    out.push_str("\nANY PROSE, COMMENTARY, OR NARRATION WILL CAUSE THE TASK TO FAIL.\n");
    out
}

fn render_user_prompt(
    subtask: &Subtask,
    repo_context: &[CodeSlice],
    context_plan: Option<&ContextPlan>,
    prior_errors: &[String],
    mcp: Option<&McpRuntime>,
    mcp_results: &[McpResultContext],
) -> String {
    let mut out = String::new();
    let attachment_context = phonton_types::render_prompt_attachments(&subtask.attachments);
    if !attachment_context.is_empty() {
        out.push_str(&attachment_context);
        out.push('\n');
    }
    out.push_str("# Subtask\n");
    out.push_str(&subtask.description);
    if let Some(plan) = context_plan {
        let repo_map: Vec<&str> = plan
            .items
            .iter()
            .filter(|item| item.included && item.kind == ContextPlanKind::RepoMap)
            .map(|item| item.summary.as_str())
            .collect();
        if !repo_map.is_empty() {
            out.push_str("\n\n# Repo map (compact)");
            for item in repo_map.iter().take(12) {
                out.push_str("\n- ");
                out.push_str(item);
            }
        }
        if plan.omitted_code_tokens > 0 {
            out.push_str(&format!(
                "\n\n# Context budget\nOmitted ~{} candidate code token(s); use the selected context and keep the diff minimal.",
                plan.omitted_code_tokens
            ));
        }
    }
    if !repo_context.is_empty() {
        out.push_str("\n\n# Repo context\n");
        for s in repo_context {
            out.push_str(&format!(
                "- {} ({}): {}\n",
                s.symbol_name,
                s.file_path.display(),
                s.signature
            ));
        }
    }
    if !prior_errors.is_empty() {
        out.push_str("\n# Previous verification failed; address these errors\n");
        for e in prior_errors {
            out.push_str("- ");
            out.push_str(e);
            out.push('\n');
        }
    }

    if let Some(mcp) = mcp {
        let mut servers = mcp.servers();
        servers.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        if !servers.is_empty() {
            out.push_str("\n# MCP servers\n");
            out.push_str(
                "You may request one MCP operation instead of a diff when external context is necessary. \
                 Output exactly one line in this form and no prose:\n",
            );
            out.push_str(
                "MCP_TOOL_CALL {\"server\":\"<server-id>\",\"tool\":\"tools/list\",\"arguments\":{}}\n",
            );
            out.push_str(
                "After tools are listed, request one concrete tool with the same marker. \
                 Approval-required or blocked operations may be denied.\n",
            );
            out.push_str("Available servers:\n");
            for server in servers {
                let permissions = render_permissions(&server.permissions);
                out.push_str(&format!(
                    "- {} ({}) trust={} permissions={}\n",
                    server.id, server.name, server.trust, permissions
                ));
            }
        }
    }

    if !mcp_results.is_empty() {
        out.push_str("\n# MCP results\n");
        for result in mcp_results {
            let status = if result.success { "success" } else { "failed" };
            out.push_str(&format!(
                "- {}/{}: {status}\n",
                result.server_id, result.tool_name
            ));
            out.push_str("<mcp-result>\n");
            out.push_str(&result.content);
            if !result.content.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("</mcp-result>\n");
        }
    }

    out.push_str("\n# CRITICAL\n");
    out.push_str("Output the UNIFIED DIFF for the above subtask. START with `--- a/` or `--- /dev/null`. NO PREAMBLE. NO PROSE.\n");

    out
}

fn render_full_prompt(rendered_context: &str, user_prompt: &str) -> String {
    let context = rendered_context.trim();
    if context.is_empty() {
        user_prompt.to_string()
    } else {
        format!("{context}\n\n{user_prompt}")
    }
}

struct PromptManifestInput<'a> {
    system_prompt: &'a str,
    subtask: &'a Subtask,
    rendered_context: &'a str,
    repo_context: &'a [CodeSlice],
    context_plan: Option<&'a ContextPlan>,
    prior_errors: &'a [String],
    mcp_results: &'a [McpResultContext],
    mcp: Option<&'a McpRuntime>,
    deduped_tokens: u64,
    compacted_tokens: u64,
    budget_limit: Option<u64>,
    attempt: u8,
    repair_attempt: bool,
}

fn prompt_context_manifest(input: PromptManifestInput<'_>) -> PromptContextManifest {
    let PromptManifestInput {
        system_prompt,
        subtask,
        rendered_context,
        repo_context,
        context_plan,
        prior_errors,
        mcp_results,
        mcp,
        deduped_tokens,
        compacted_tokens,
        budget_limit,
        attempt,
        repair_attempt,
    } = input;
    let system_tokens = estimate_prompt_tokens(system_prompt);
    let user_goal_tokens = estimate_prompt_tokens(&subtask.description);
    let memory_tokens = estimate_prompt_tokens(rendered_context);
    let attachment_tokens = estimate_prompt_tokens(&phonton_types::render_prompt_attachments(
        &subtask.attachments,
    ));
    let repo_map_tokens = context_plan.map(|plan| plan.repo_map_tokens).unwrap_or(0);
    let code_context_tokens = context_plan
        .map(|plan| plan.selected_code_tokens)
        .unwrap_or_else(|| {
            repo_context
                .iter()
                .map(|slice| slice.token_count as u64)
                .sum()
        });
    let omitted_code_tokens = context_plan
        .map(|plan| plan.omitted_code_tokens)
        .unwrap_or(0);
    let context_target_tokens = context_plan
        .map(|plan| plan.target_tokens)
        .unwrap_or_default();
    let target_exceeded = context_plan
        .map(|plan| plan.target_exceeded)
        .unwrap_or_default();
    let over_target_tokens = context_plan
        .map(|plan| plan.over_target_tokens)
        .unwrap_or_default();
    let retry_error_tokens = estimate_prompt_tokens(&prior_errors.join("\n"));
    let mcp_text = render_mcp_budget_text(mcp, mcp_results);
    let mcp_tool_tokens = estimate_prompt_tokens(&mcp_text);
    let total_estimated_tokens = system_tokens
        .saturating_add(user_goal_tokens)
        .saturating_add(memory_tokens)
        .saturating_add(attachment_tokens)
        .saturating_add(repo_map_tokens)
        .saturating_add(code_context_tokens)
        .saturating_add(mcp_tool_tokens)
        .saturating_add(retry_error_tokens);

    PromptContextManifest {
        system_tokens,
        user_goal_tokens,
        memory_tokens,
        attachment_tokens,
        code_context_tokens,
        repo_map_tokens,
        omitted_code_tokens,
        context_target_tokens,
        attempt,
        repair_attempt,
        target_exceeded,
        over_target_tokens,
        mcp_tool_tokens,
        retry_error_tokens,
        total_estimated_tokens,
        budget_limit,
        compacted_tokens,
        deduped_tokens,
    }
}

fn estimate_prompt_tokens(text: &str) -> u64 {
    if text.trim().is_empty() {
        return 0;
    }
    ((text.chars().count() as u64).saturating_add(3)) / 4
}

fn render_mcp_budget_text(mcp: Option<&McpRuntime>, mcp_results: &[McpResultContext]) -> String {
    let mut text = String::new();
    if let Some(runtime) = mcp {
        for server in runtime.servers() {
            text.push_str(server.id.as_str());
            text.push(' ');
            text.push_str(&server.name);
            text.push('\n');
        }
    }
    for result in mcp_results {
        text.push_str(&result.server_id.to_string());
        text.push(' ');
        text.push_str(&result.tool_name);
        text.push(' ');
        text.push_str(&result.content);
        text.push('\n');
    }
    text
}

fn compact_verify_retry_errors(layer: VerifyLayer, errors: &[String]) -> Vec<String> {
    if errors.is_empty() {
        return vec![format!(
            "Verifier {layer:?} failed without a detailed diagnostic. Repair the diff and keep the next response as a unified diff only."
        )];
    }
    let mut compact: Vec<String> = errors
        .iter()
        .take(6)
        .map(|error| format!("Verifier {layer:?}: {}", truncate_chars(error.trim(), 500)))
        .collect();
    if errors.len() > compact.len() {
        compact.push(format!(
            "{} additional verifier error(s) omitted.",
            errors.len() - compact.len()
        ));
    }
    compact
}

#[derive(Debug, Clone, PartialEq)]
struct McpToolRequest {
    server_id: ExtensionId,
    tool_name: String,
    arguments: Value,
}

#[derive(Debug, Clone, PartialEq)]
struct McpResultContext {
    server_id: ExtensionId,
    tool_name: String,
    success: bool,
    content: String,
}

fn parse_mcp_tool_request(content: &str) -> Result<Option<McpToolRequest>> {
    let body = unfence_diff(content);
    let trimmed = body.trim();
    if trimmed.is_empty() || trimmed.starts_with("--- ") || trimmed.starts_with("diff --git") {
        return Ok(None);
    }

    let value = if let Some(rest) = trimmed.strip_prefix("MCP_TOOL_CALL") {
        let json_text = extract_json_object(rest)
            .ok_or_else(|| anyhow!("missing JSON object after MCP_TOOL_CALL marker"))?;
        serde_json::from_str::<Value>(json_text)
            .map_err(|e| anyhow!("invalid MCP_TOOL_CALL JSON: {e}"))?
    } else if trimmed.starts_with('{') {
        let json_text =
            extract_json_object(trimmed).ok_or_else(|| anyhow!("missing JSON object"))?;
        let value = serde_json::from_str::<Value>(json_text)
            .map_err(|e| anyhow!("invalid MCP tool-call JSON: {e}"))?;
        match value.get("mcp_tool_call") {
            Some(inner) => inner.clone(),
            None if value.get("server").is_some() || value.get("server_id").is_some() => value,
            None => return Ok(None),
        }
    } else {
        return Ok(None);
    };

    parse_mcp_tool_request_value(value).map(Some)
}

fn parse_mcp_tool_request_value(value: Value) -> Result<McpToolRequest> {
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow!("MCP tool call must be a JSON object"))?;
    let server_id = obj
        .get("server")
        .or_else(|| obj.get("server_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("missing non-empty `server`"))?;
    let tool_name = obj
        .get("tool")
        .or_else(|| obj.get("tool_name"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("missing non-empty `tool`"))?;
    let arguments = obj
        .get("arguments")
        .or_else(|| obj.get("args"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return Err(anyhow!("`arguments` must be a JSON object"));
    }
    Ok(McpToolRequest {
        server_id: ExtensionId::new(server_id.trim()),
        tool_name: tool_name.trim().to_string(),
        arguments,
    })
}

fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => depth = depth.saturating_add(1),
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = start + offset + ch.len_utf8();
                    return Some(&text[start..end]);
                }
            }
            _ => {}
        }
    }
    None
}

fn is_mcp_tool_list_request(tool_name: &str) -> bool {
    matches!(tool_name, "tools/list" | "__list_tools" | "list_tools")
}

fn render_mcp_tools(tools: &[McpTool]) -> String {
    if tools.is_empty() {
        return "No tools returned by server.".into();
    }
    let mut out = String::new();
    for tool in tools {
        out.push_str(&format!("- {}", tool.name));
        if let Some(title) = tool.title.as_deref().filter(|s| !s.trim().is_empty()) {
            out.push_str(&format!(" ({title})"));
        }
        if let Some(description) = tool.description.as_deref().filter(|s| !s.trim().is_empty()) {
            out.push_str(": ");
            out.push_str(description.trim());
        }
        if !tool.input_schema.is_null() {
            out.push_str("\n  input_schema: ");
            out.push_str(&compact_json(&tool.input_schema));
        }
        out.push('\n');
    }
    out
}

fn render_mcp_result(result: &McpCallResult) -> String {
    let mut out = String::new();
    if result.is_error {
        out.push_str("[tool reported error]\n");
    }
    for block in &result.content {
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            out.push_str(text);
            if !text.ends_with('\n') {
                out.push('\n');
            }
        } else if let Some(text) = block.as_str() {
            out.push_str(text);
            if !text.ends_with('\n') {
                out.push('\n');
            }
        } else {
            out.push_str(&compact_json(block));
            out.push('\n');
        }
    }
    if let Some(structured) = &result.structured_content {
        out.push_str("structured_content: ");
        out.push_str(&compact_json(structured));
        out.push('\n');
    }
    if out.trim().is_empty() {
        "Tool returned no content.".into()
    } else {
        out
    }
}

fn render_permissions(permissions: &[Permission]) -> String {
    if permissions.is_empty() {
        return "none".into();
    }
    permissions
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unserializable-json>".into())
}

/// Minimal unified-diff parser. Sufficient for the model output the worker
/// expects (one `--- a/ +++ b/` header per file, `@@` hunk headers, then
/// ` `/`+`/`-` lines).
///
/// Tolerant of common LLM idioms:
/// * Fenced code blocks (```diff … ```, ```patch …, plain ```) are
///   un-fenced before parsing — many models wrap the diff regardless of
///   prompting.
/// * Conversational prose before/after the diff is silently skipped.
/// * A malformed `@@` header inside an otherwise-valid stream marks
///   *just that hunk* invalid; the rest of the stream still parses. The
///   first malformed-hunk reason is surfaced if zero hunks survive, so
///   the caller can feed it back to the model on retry.
///
/// Returns `Err` only when there is **no** valid diff content at all.
/// Real-world edge cases — rename headers, binary markers — are deferred
/// to `phonton-diff`.
pub fn parse_unified_diff(text: &str) -> Result<Vec<DiffHunk>> {
    // Step 1: extract the diff body. If the response is wrapped in a
    // fenced code block we want only what's between the fences. If it's
    // plain text we use it as-is. Everything before the first diff
    // marker (`---`, `+++ b/`, `@@`) is treated as preamble and dropped.
    let body = unfence_diff(text);

    // Explicit preamble stripping: find the first line that looks like
    // a diff header and start from there.
    let markers = ["--- ", "+++ ", "@@ "];
    let mut start_idx = 0;
    let lines_vec: Vec<&str> = body.lines().collect();
    for (i, line) in lines_vec.iter().enumerate() {
        if markers.iter().any(|m| line.starts_with(m)) {
            start_idx = i;
            break;
        }
    }
    let body_trimmed = lines_vec[start_idx..].join("\n");

    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut header: Option<(u32, u32, u32, u32)> = None;
    let mut lines: Vec<DiffLine> = Vec::new();
    let mut last_parse_err: Option<String> = None;

    fn flush(
        hunks: &mut Vec<DiffHunk>,
        path: &Option<PathBuf>,
        header: &Option<(u32, u32, u32, u32)>,
        lines: &mut Vec<DiffLine>,
    ) {
        if let (Some(p), Some(h)) = (path, header) {
            if !lines.is_empty() {
                hunks.push(DiffHunk {
                    file_path: p.clone(),
                    old_start: h.0,
                    old_count: h.1,
                    new_start: h.2,
                    new_count: h.3,
                    lines: std::mem::take(lines),
                });
            }
        }
    }

    for raw in body_trimmed.lines() {
        if let Some(rest) = raw.strip_prefix("+++ b/") {
            flush(&mut hunks, &current_path, &header, &mut lines);
            current_path = Some(PathBuf::from(rest.trim()));
            header = None;
        } else if let Some(rest) = raw.strip_prefix("+++ ") {
            // Some models drop the `b/` prefix.
            flush(&mut hunks, &current_path, &header, &mut lines);
            current_path = Some(PathBuf::from(rest.trim()));
            header = None;
        } else if raw.starts_with("--- ") {
            // Old-side header — handled together with +++; ignore alone.
            continue;
        } else if let Some(rest) = raw.strip_prefix("@@") {
            flush(&mut hunks, &current_path, &header, &mut lines);
            // A bad header invalidates *only* this hunk — record the
            // reason and keep scanning for the next valid one. This
            // turns "model returned slightly wrong @@ counts" from a
            // hard task failure into a recoverable retry-with-error.
            match parse_hunk_header(rest) {
                Ok(h) => header = Some(h),
                Err(e) => {
                    last_parse_err = Some(format!("malformed hunk header `{}`: {e}", raw.trim()));
                    header = None;
                }
            }
        } else if header.is_some() {
            if let Some(s) = raw.strip_prefix('+') {
                lines.push(DiffLine::Added(s.to_string()));
            } else if let Some(s) = raw.strip_prefix('-') {
                lines.push(DiffLine::Removed(s.to_string()));
            } else if let Some(s) = raw.strip_prefix(' ') {
                lines.push(DiffLine::Context(s.to_string()));
            }
            // Lines outside the +/-/space alphabet inside an open hunk
            // are silently dropped (often a stray blank from the model).
        }
    }
    flush(&mut hunks, &current_path, &header, &mut lines);

    if hunks.is_empty() {
        // Nothing usable. Build a diagnostic that the worker can feed
        // back to the model on retry — preview enough of the response
        // for the user to see what went wrong without flooding the UI.
        let preview: String = text.chars().take(200).collect();
        let detail = match last_parse_err {
            Some(reason) => reason,
            None => "no `+++ b/<path>` or `@@` markers found".into(),
        };
        return Err(anyhow!(
            "model output did not contain a parseable unified diff ({detail}). \
             Got (first 200 chars): {preview:?}"
        ));
    }

    Ok(hunks)
}

/// Strip a single ```/```diff/```patch fenced code block out of `text`,
/// returning its body. If no fence is present the original text is
/// returned. If multiple fences are present the *first* one wins —
/// in practice the model puts the diff in the first block and the rest
/// is commentary.
fn unfence_diff(text: &str) -> String {
    let mut in_fence = false;
    let mut captured = String::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if !in_fence {
            if trimmed.starts_with("```") {
                in_fence = true;
                continue;
            }
        } else {
            if trimmed.starts_with("```") {
                // End of the first fenced block — stop and return what
                // we captured so trailing prose doesn't pollute the parse.
                return captured;
            }
            captured.push_str(line);
            captured.push('\n');
        }
    }
    // No closing fence (or no fences at all) — fall back to the full text.
    if captured.is_empty() {
        text.to_string()
    } else {
        captured
    }
}

/// Parse the `-old_start,old_count +new_start,new_count @@` portion of a
/// hunk header. Counts default to 1 when omitted, per unified-diff spec.
fn parse_hunk_header(rest: &str) -> Result<(u32, u32, u32, u32)> {
    let trimmed = rest.trim().trim_end_matches("@@").trim();
    let mut parts = trimmed.split_whitespace();
    let old = parts
        .next()
        .ok_or_else(|| anyhow!("hunk header missing old range"))?;
    let new = parts
        .next()
        .ok_or_else(|| anyhow!("hunk header missing new range"))?;
    let (old_start, old_count) = parse_range(old.trim_start_matches('-'))?;
    let (new_start, new_count) = parse_range(new.trim_start_matches('+'))?;
    Ok((old_start, old_count, new_start, new_count))
}

fn parse_range(s: &str) -> Result<(u32, u32)> {
    let mut it = s.splitn(2, ',');
    let start: u32 = it
        .next()
        .ok_or_else(|| anyhow!("empty range"))?
        .parse()
        .map_err(|e| anyhow!("bad range start: {e}"))?;
    let count: u32 = match it.next() {
        Some(c) => c.parse().map_err(|e| anyhow!("bad range count: {e}"))?,
        None => 1,
    };
    Ok((start, count))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn guard() -> ExecutionGuard {
        ExecutionGuard::new(PathBuf::from("/work/proj"))
    }

    fn temp_workspace(name: &str) -> (PathBuf, PathBuf) {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "phonton-worker-{name}-{}-{suffix}",
            std::process::id()
        ));
        let root = base.join("workspace");
        std::fs::create_dir_all(&root).expect("create temp workspace");
        (base, root)
    }

    #[test]
    fn allow_read_inside_root() {
        let d = guard().evaluate(&ToolCall::Read {
            path: PathBuf::from("/work/proj/src/lib.rs"),
        });
        assert_eq!(d, GuardDecision::Allow);
    }

    #[test]
    fn approve_read_outside_root() {
        let d = guard().evaluate(&ToolCall::Read {
            path: PathBuf::from("/tmp/other.txt"),
        });
        assert!(matches!(d, GuardDecision::Approve { .. }));
    }

    #[tokio::test]
    async fn read_tool_refuses_canonical_escape_outside_root() {
        let (base, root) = temp_workspace("read-escape");
        let outside = base.join("outside.txt");
        std::fs::write(&outside, "secret").expect("write outside file");

        let result = read_tool(&root, &outside).await.expect("read tool result");

        assert!(result.contains("refusing to read outside project root"));
        assert!(!result.contains("secret"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn write_tool_refuses_absolute_path_outside_root() {
        let (base, root) = temp_workspace("write-escape");
        let outside = base.join("outside.txt");

        let result = write_tool(&root, &outside, "secret")
            .await
            .expect("write tool result");

        assert!(result.contains("refusing to write outside project root"));
        assert!(!outside.exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn block_ssh() {
        let d = guard().evaluate(&ToolCall::Read {
            path: PathBuf::from("/home/u/.ssh/id_rsa"),
        });
        assert!(matches!(d, GuardDecision::Block { .. }));
    }

    #[test]
    fn block_etc() {
        let d = guard().evaluate(&ToolCall::Write {
            path: PathBuf::from("/etc/passwd"),
            content: String::new(),
        });
        assert!(matches!(d, GuardDecision::Block { .. }));
    }

    #[test]
    fn block_windows_system() {
        let d = guard().evaluate(&ToolCall::Write {
            path: PathBuf::from("C:\\Windows\\System32\\config"),
            content: String::new(),
        });
        assert!(matches!(d, GuardDecision::Block { .. }));
    }

    #[test]
    fn allow_cargo_run() {
        let d = guard().evaluate(&ToolCall::Run {
            program: "cargo".into(),
            args: vec!["check".into()],
        });
        assert_eq!(d, GuardDecision::Allow);
    }

    #[test]
    fn approve_arbitrary_bash() {
        let d = guard().evaluate(&ToolCall::Bash {
            command: "echo hi".into(),
        });
        assert!(matches!(d, GuardDecision::Approve { .. }));
    }

    #[test]
    fn approve_rm_outside_root() {
        let d = guard().evaluate(&ToolCall::Run {
            program: "rm".into(),
            args: vec!["-rf".into(), "/tmp/elsewhere".into()],
        });
        assert!(matches!(d, GuardDecision::Approve { .. }));
    }

    #[test]
    fn approve_network() {
        let d = guard().evaluate(&ToolCall::Network {
            url: "https://example.com".into(),
        });
        assert!(matches!(d, GuardDecision::Approve { .. }));
    }

    #[test]
    fn block_bash_targeting_etc() {
        let d = guard().evaluate(&ToolCall::Bash {
            command: "cat /etc/shadow".into(),
        });
        assert!(matches!(d, GuardDecision::Block { .. }));
    }

    #[test]
    fn parse_simple_diff() {
        let text = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,3 @@
 fn a() {}
+fn b() {}
 fn c() {}
";
        let hunks = parse_unified_diff(text).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].file_path, PathBuf::from("src/lib.rs"));
        assert_eq!(hunks[0].lines.len(), 3);
    }

    #[test]
    fn parses_mcp_tool_call_marker() {
        let request = parse_mcp_tool_request(
            r#"MCP_TOOL_CALL {"server":"github","tool":"search_issues","arguments":{"q":"bug"}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(request.server_id, ExtensionId::new("github"));
        assert_eq!(request.tool_name, "search_issues");
        assert_eq!(request.arguments["q"], "bug");
    }

    #[test]
    fn parses_json_mcp_tool_call_object() {
        let request = parse_mcp_tool_request(
            r#"{"mcp_tool_call":{"server_id":"docs","tool_name":"tools/list"}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(request.server_id, ExtensionId::new("docs"));
        assert_eq!(request.tool_name, "tools/list");
        assert_eq!(request.arguments, serde_json::json!({}));
    }

    #[test]
    fn malformed_mcp_tool_call_reports_error() {
        let err = parse_mcp_tool_request(
            r#"MCP_TOOL_CALL {"server":"docs","tool":"read","arguments":["not","object"]}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("arguments"));
    }

    #[test]
    fn full_prompt_omits_empty_context_padding() {
        assert_eq!(
            render_full_prompt("", "# Subtask\nmake chess"),
            "# Subtask\nmake chess"
        );
        assert_eq!(
            render_full_prompt("prior", "# Subtask\nmake chess"),
            "prior\n\n# Subtask\nmake chess"
        );
    }

    #[test]
    fn prompt_manifest_breaks_out_section_costs() {
        let subtask = Subtask {
            id: SubtaskId::new(),
            description: "make chess".into(),
            model_tier: ModelTier::Standard,
            dependencies: Vec::new(),
            attachments: Vec::new(),
            status: SubtaskStatus::Queued,
        };
        let errors = vec!["syntax failed".into()];
        let manifest = prompt_context_manifest(PromptManifestInput {
            system_prompt: &base_system_prompt(),
            subtask: &subtask,
            rendered_context: "prior decision context",
            repo_context: &[],
            context_plan: None,
            prior_errors: &errors,
            mcp_results: &[],
            mcp: None,
            deduped_tokens: 0,
            compacted_tokens: 0,
            budget_limit: Some(DEFAULT_WINDOW_LIMIT as u64),
            attempt: 2,
            repair_attempt: true,
        });
        assert!(manifest.system_tokens > 0);
        assert!(manifest.user_goal_tokens > 0);
        assert!(manifest.memory_tokens > 0);
        assert!(manifest.retry_error_tokens > 0);
        assert_eq!(manifest.attempt, 2);
        assert!(manifest.repair_attempt);
        assert_eq!(
            manifest.total_estimated_tokens,
            manifest
                .system_tokens
                .saturating_add(manifest.user_goal_tokens)
                .saturating_add(manifest.memory_tokens)
                .saturating_add(manifest.attachment_tokens)
                .saturating_add(manifest.repo_map_tokens)
                .saturating_add(manifest.code_context_tokens)
                .saturating_add(manifest.mcp_tool_tokens)
                .saturating_add(manifest.retry_error_tokens)
        );
    }

    #[test]
    fn compact_verify_retry_errors_caps_diagnostic_context() {
        let errors: Vec<String> = (0..8)
            .map(|i| format!("[python syntax] chess.py:{}: {}", i + 1, "x".repeat(700)))
            .collect();
        let compact = compact_verify_retry_errors(VerifyLayer::Syntax, &errors);

        assert_eq!(compact.len(), 7);
        assert!(compact[0].starts_with("Verifier Syntax: [python syntax] chess.py:1"));
        assert!(compact[0].chars().count() < 540);
        assert!(compact
            .last()
            .unwrap()
            .contains("additional verifier error"));
    }

    #[test]
    fn repo_context_dedupe_keeps_one_copy_and_tracks_saved_tokens() {
        let slice = CodeSlice {
            file_path: PathBuf::from("src/lib.rs"),
            symbol_name: "parse".into(),
            signature: "fn parse()".into(),
            docstring: None,
            callsites: Vec::new(),
            token_count: 42,
            origin: SliceOrigin::Semantic,
        };
        let duplicate = slice.clone();
        let deduped = dedupe_code_slices(
            std::slice::from_ref(&slice),
            std::slice::from_ref(&duplicate),
        );

        assert_eq!(deduped.slices.len(), 1);
        assert_eq!(deduped.deduped_tokens, 42);
    }

    #[test]
    fn generated_slice_reads_current_artifact_as_context() {
        let (base, root) = temp_workspace("artifact-context");
        let index = root.join("index.html");
        std::fs::write(
            &index,
            "<!doctype html>\n<div id=\"board\">current board</div>\n",
        )
        .expect("write artifact");
        let task = subtask(
            "Acceptance slice 2/7 for `make chess in html`: show named pieces. Artifact: index.html. Keep the diff minimal.",
        );

        let slices = current_artifact_context_slices(&root, &task, &[]);

        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].file_path, PathBuf::from("index.html"));
        assert!(slices[0].symbol_name.contains("current artifact"));
        assert!(slices[0].signature.contains("current board"));
        assert!(slices[0]
            .signature
            .contains("Patch against this exact current file"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn repair_context_reads_file_named_in_verifier_error() {
        let (base, root) = temp_workspace("repair-artifact-context");
        let path = root.join("src/chessRules.test.ts");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "import { describe, it, expect } from 'vitest';\nimport { getInitialBoard } from './chessRules';\n",
        )
        .unwrap();

        let task = subtask(
            "Vite React chess app acceptance slice 2/7: implement rules. Artifact: src/chessRules.ts.",
        );
        let errors = vec![
            "Verifier Syntax: [typescript syntax] src/chessRules.test.ts: could not reconstruct post-diff file: removed-line mismatch at line 1".to_string(),
        ];

        let slices = current_artifact_context_slices(&root, &task, &errors);

        assert!(slices.iter().any(|slice| {
            slice.file_path.as_path() == Path::new("src/chessRules.test.ts")
                && slice.signature.contains("import { describe, it, expect }")
        }));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn successful_worker_context_omits_full_diff_body() {
        let task =
            subtask("Acceptance slice 2/4 for `make chess`: wire board UI. Artifact: src/App.tsx.");
        let diff = (0..80)
            .map(|line| format!("+const generatedLine{line} = {line};"))
            .collect::<Vec<_>>()
            .join("\n");
        let hunk = DiffHunk {
            file_path: PathBuf::from("src/App.tsx"),
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: 80,
            lines: diff
                .lines()
                .map(|line| DiffLine::Added(line.trim_start_matches('+').to_string()))
                .collect(),
        };

        let frame = compact_success_context(&task, &diff, &[hunk]);

        assert!(frame.contains("Completed subtask"));
        assert!(frame.contains("src/App.tsx"));
        assert!(!frame.contains("generatedLine79"));
        assert!(estimate_prompt_tokens(&frame) < 120);
    }

    #[test]
    fn worker_prompt_policy_sets_low_context_budgets_by_task_class() {
        let docs = subtask("clean up the readme typos");
        let bug = subtask("fix failing config parser panic");
        let generated = subtask("make chess in html");

        assert_eq!(dynamic_worker_context_target(&docs, &[]), 800);
        assert_eq!(dynamic_worker_context_target(&bug, &[]), 1_800);
        assert_eq!(dynamic_worker_context_target(&generated, &[]), 1_200);
        assert_eq!(semantic_slice_limit(&generated), 1);
    }

    #[test]
    fn generated_app_repairs_are_sub_1k_input_context() {
        let generated = subtask("make chess in html");
        let errors = vec!["missing reset behavior".into()];
        let policy = worker_prompt_policy(&generated, true);

        assert_eq!(dynamic_worker_context_target(&generated, &errors), 900);
        assert_eq!(policy.max_repo_map_items, 2);
        assert!(policy.mcp_result_max_chars <= 900);
    }

    #[test]
    fn first_attempt_system_prompt_omits_bulky_diff_example() {
        let prompt = system_prompt_for_attempt(&[], false);

        assert!(prompt.contains("You are a Phonton worker"));
        assert!(!prompt.contains("EXAMPLE OF CREATING A NEW FILE"));
        assert!(!prompt.contains("MCP_TOOL_CALL"));
    }

    #[derive(Clone)]
    struct RecordingProvider {
        calls: Arc<Mutex<Vec<(String, String)>>>,
    }

    #[async_trait::async_trait]
    impl Provider for RecordingProvider {
        async fn call(
            &self,
            system: &str,
            user: &str,
            _slice_origins: &[SliceOrigin],
        ) -> Result<phonton_types::LLMResponse> {
            self.calls
                .lock()
                .unwrap()
                .push((system.to_string(), user.to_string()));
            Ok(phonton_types::LLMResponse {
                content: "this is not a diff".into(),
                input_tokens: 10,
                output_tokens: 2,
                cached_tokens: 0,
                cache_creation_tokens: 0,
                provider: phonton_types::ProviderKind::Anthropic,
                model_name: "recording".into(),
            })
        }

        fn kind(&self) -> phonton_types::ProviderKind {
            phonton_types::ProviderKind::Anthropic
        }

        fn model(&self) -> String {
            "recording".into()
        }

        fn clone_box(&self) -> Box<dyn Provider> {
            Box::new(self.clone())
        }
    }

    #[tokio::test]
    async fn worker_does_not_duplicate_system_prompt_in_user_context() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let provider = RecordingProvider {
            calls: Arc::clone(&calls),
        };
        let worker = Worker::new(Box::new(provider), guard());
        let subtask = Subtask {
            id: SubtaskId::new(),
            description: "make chess".into(),
            model_tier: ModelTier::Standard,
            dependencies: Vec::new(),
            attachments: Vec::new(),
            status: SubtaskStatus::Queued,
        };

        let _ = worker.execute(subtask, Vec::new()).await.unwrap();

        let calls = calls.lock().unwrap();
        assert!(!calls.is_empty());
        let (system, user) = &calls[0];
        assert!(system.contains("You are a Phonton worker"));
        assert!(user.contains("# Subtask"));
        assert!(user.contains("make chess"));
        assert!(!user.contains("You are a Phonton worker"));
    }

    #[tokio::test]
    async fn execute_with_prior_errors_includes_them_on_first_prompt() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let provider = RecordingProvider {
            calls: Arc::clone(&calls),
        };
        let worker = Worker::new(Box::new(provider), guard());
        let subtask = Subtask {
            id: SubtaskId::new(),
            description: "fix generated chess tests".into(),
            model_tier: ModelTier::Standard,
            dependencies: Vec::new(),
            attachments: Vec::new(),
            status: SubtaskStatus::Queued,
        };

        let _ = worker
            .execute_with_prior_errors(
                subtask,
                Vec::new(),
                vec!["Verifier Syntax: src/chessRules.test.ts removed-line mismatch".into()],
            )
            .await
            .unwrap();

        let calls = calls.lock().unwrap();
        let (_, first_user_prompt) = &calls[0];
        assert!(first_user_prompt.contains("# Previous verification failed"));
        assert!(first_user_prompt.contains("removed-line mismatch"));
    }

    #[tokio::test]
    async fn generated_app_first_prompt_includes_current_artifact_snapshot() {
        let (base, root) = temp_workspace("generated-artifact-first-prompt");
        let app = root.join("src").join("App.tsx");
        std::fs::create_dir_all(app.parent().unwrap()).unwrap();
        std::fs::write(
            &app,
            "import './App.css'\n\nfunction App() { return <main>placeholder</main> }\n\nexport default App\n",
        )
        .unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let provider = RecordingProvider {
            calls: Arc::clone(&calls),
        };
        let worker = Worker::new(Box::new(provider), ExecutionGuard::new(root));
        let subtask = Subtask {
            id: SubtaskId::new(),
            description:
                "Vite React chess app acceptance slice 4/7: render board. Artifact: src/App.tsx."
                    .into(),
            model_tier: ModelTier::Standard,
            dependencies: Vec::new(),
            attachments: Vec::new(),
            status: SubtaskStatus::Queued,
        };

        let _ = worker.execute(subtask, Vec::new()).await.unwrap();

        let calls = calls.lock().unwrap();
        let (_, first_user_prompt) = &calls[0];
        assert!(first_user_prompt.contains("Patch against this exact current file"));
        assert!(first_user_prompt.contains("import './App.css'"));
        assert!(first_user_prompt.contains("function App()"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn existing_vite_chess_rules_seed_uses_local_verified_diff_without_provider_call() {
        let (base, root) = temp_workspace("local-chess-rules-seed");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let provider = RecordingProvider {
            calls: Arc::clone(&calls),
        };
        let worker = Worker::new(Box::new(provider), ExecutionGuard::new(root));
        let subtask = Subtask {
            id: SubtaskId::new(),
            description: "Existing Vite React chess app acceptance slice 1/4: create a compile-safe local chess rules seed. Artifacts: src/chessRules.ts, src/chessRules.test.ts.".into(),
            model_tier: ModelTier::Standard,
            dependencies: Vec::new(),
            attachments: Vec::new(),
            status: SubtaskStatus::Queued,
        };

        let result = worker.execute(subtask, Vec::new()).await.unwrap();

        assert!(matches!(result.status, SubtaskStatus::Done { .. }));
        assert!(matches!(result.verify_result, VerifyResult::Pass { .. }));
        assert_eq!(result.token_usage.input_tokens, 0);
        assert_eq!(result.token_usage.output_tokens, 0);
        assert!(calls.lock().unwrap().is_empty());
        assert!(result
            .diff_hunks
            .iter()
            .any(|hunk| hunk.file_path.as_path() == Path::new("src/chessRules.ts")));
        assert!(result
            .diff_hunks
            .iter()
            .any(|hunk| hunk.file_path.as_path() == Path::new("src/chessRules.test.ts")));
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn existing_vite_chess_rules_seed_replaces_partial_failed_artifacts_locally() {
        let (base, root) = temp_workspace("local-chess-rules-seed-repair");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src").join("chessRules.ts"),
            "not valid typescript {",
        )
        .unwrap();
        std::fs::write(root.join("src").join("chessRules.test.ts"), "broken test {").unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let provider = RecordingProvider {
            calls: Arc::clone(&calls),
        };
        let worker = Worker::new(Box::new(provider), ExecutionGuard::new(root));
        let subtask = Subtask {
            id: SubtaskId::new(),
            description: "Existing Vite React chess app acceptance slice 1/4: create a compile-safe local chess rules seed. Artifacts: src/chessRules.ts, src/chessRules.test.ts.".into(),
            model_tier: ModelTier::Standard,
            dependencies: Vec::new(),
            attachments: Vec::new(),
            status: SubtaskStatus::Queued,
        };

        let result = worker.execute(subtask, Vec::new()).await.unwrap();

        assert!(matches!(result.status, SubtaskStatus::Done { .. }));
        assert!(matches!(result.verify_result, VerifyResult::Pass { .. }));
        assert_eq!(result.token_usage.input_tokens, 0);
        assert_eq!(result.token_usage.output_tokens, 0);
        assert!(calls.lock().unwrap().is_empty());
        let rules_hunk = result
            .diff_hunks
            .iter()
            .find(|hunk| hunk.file_path.as_path() == Path::new("src/chessRules.ts"))
            .unwrap();
        assert_eq!(rules_hunk.old_count, 1);
        assert!(rules_hunk.lines.iter().any(
            |line| matches!(line, DiffLine::Removed(line) if line == "not valid typescript {")
        ));
        let _ = std::fs::remove_dir_all(base);
    }

    #[derive(Clone)]
    struct BrokenTsxProvider {
        calls: Arc<Mutex<u64>>,
    }

    #[async_trait::async_trait]
    impl Provider for BrokenTsxProvider {
        async fn call(
            &self,
            _system: &str,
            _user: &str,
            _slice_origins: &[SliceOrigin],
        ) -> Result<phonton_types::LLMResponse> {
            *self.calls.lock().unwrap() += 1;
            Ok(phonton_types::LLMResponse {
                content: "\
--- /dev/null
+++ b/src/App.tsx
@@ -0,0 +1,2 @@
+import React from 'react';
+export default function App() { return <div>{</div>; }
"
                .into(),
                input_tokens: 900,
                output_tokens: 40,
                cached_tokens: 0,
                cache_creation_tokens: 0,
                provider: phonton_types::ProviderKind::OpenAiCompatible,
                model_name: "broken-tsx".into(),
            })
        }

        fn kind(&self) -> phonton_types::ProviderKind {
            phonton_types::ProviderKind::OpenAiCompatible
        }

        fn model(&self) -> String {
            "broken-tsx".into()
        }

        fn clone_box(&self) -> Box<dyn Provider> {
            Box::new(self.clone())
        }
    }

    #[tokio::test]
    async fn generated_web_syntax_failure_does_not_spend_repair_call() {
        let (base, root) = temp_workspace("generated-web-fast-fail");
        let calls = Arc::new(Mutex::new(0));
        let provider = BrokenTsxProvider {
            calls: Arc::clone(&calls),
        };
        let worker = Worker::new(Box::new(provider), ExecutionGuard::new(root));
        let subtask = Subtask {
            id: SubtaskId::new(),
            description: "Vite React chess app acceptance slice 1/7: scaffold src/App.tsx".into(),
            model_tier: ModelTier::Standard,
            dependencies: Vec::new(),
            attachments: Vec::new(),
            status: SubtaskStatus::Queued,
        };

        let result = worker.execute(subtask, Vec::new()).await.unwrap();

        assert_eq!(*calls.lock().unwrap(), 1);
        assert!(matches!(result.status, SubtaskStatus::Failed { .. }));
        match result.verify_result {
            VerifyResult::Fail {
                errors, attempt, ..
            } => {
                assert_eq!(attempt, 1);
                assert!(
                    errors
                        .join("\n")
                        .contains("stopped before generated-app syntax repair"),
                    "{errors:?}"
                );
            }
            other => panic!("expected syntax fail, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn repeated_diagnostic_signature_ignores_attempt_noise() {
        let first = diagnostic_signature(
            VerifyLayer::Syntax,
            &["attempt 1 failed at line 14: removed-line mismatch".into()],
        );
        let second = diagnostic_signature(
            VerifyLayer::Syntax,
            &["attempt 2 failed at line 99: removed-line mismatch".into()],
        );

        assert_eq!(first, second);
    }

    #[test]
    fn render_user_prompt_lists_mcp_servers_and_results() {
        let subtask = Subtask {
            id: SubtaskId::new(),
            description: "Read docs before editing".into(),
            model_tier: ModelTier::Cheap,
            dependencies: Vec::new(),
            attachments: Vec::new(),
            status: SubtaskStatus::Queued,
        };
        let server = phonton_types::McpServerDefinition {
            id: ExtensionId::new("docs"),
            name: "Docs".into(),
            source: phonton_types::ExtensionSource::Workspace,
            transport: phonton_types::McpTransport::Stdio {
                command: "node".into(),
                args: vec!["server.js".into()],
            },
            trust: phonton_types::TrustLevel::ReadOnlyTool,
            permissions: vec![Permission::FsReadWorkspace],
            applies_to: phonton_types::AppliesTo::default(),
            env: Vec::new(),
            enabled: true,
        };
        let runtime = McpRuntime::new(vec![server], guard());
        let results = vec![McpResultContext {
            server_id: ExtensionId::new("docs"),
            tool_name: "tools/list".into(),
            success: true,
            content: "- read_file".into(),
        }];
        let prompt = render_user_prompt(&subtask, &[], None, &[], Some(&runtime), &results);
        assert!(prompt.contains("# MCP servers"));
        assert!(prompt.contains("docs (Docs)"));
        assert!(prompt.contains("# MCP results"));
        assert!(prompt.contains("read_file"));
    }

    #[test]
    fn detects_trait_decision() {
        let st = Subtask {
            id: SubtaskId::new(),
            description: "Introduce a trait MemoryWriter for async appends".into(),
            model_tier: ModelTier::Standard,
            dependencies: Vec::new(),
            attachments: Vec::new(),
            status: SubtaskStatus::Queued,
        };
        let d = detect_decisions(&st, None);
        assert_eq!(d.len(), 1);
        match &d[0] {
            MemoryRecord::Decision { title, .. } => {
                assert!(title.contains("trait"));
                assert!(title.contains("MemoryWriter"));
            }
            other => panic!("unexpected record: {other:?}"),
        }
    }

    #[test]
    fn function_addition_is_not_architectural() {
        let st = Subtask {
            id: SubtaskId::new(),
            description: "Add a function parse_callsites".into(),
            model_tier: ModelTier::Cheap,
            dependencies: Vec::new(),
            attachments: Vec::new(),
            status: SubtaskStatus::Queued,
        };
        assert!(detect_decisions(&st, None).is_empty());
    }

    fn subtask(description: &str) -> Subtask {
        Subtask {
            id: SubtaskId::new(),
            description: description.into(),
            model_tier: ModelTier::Standard,
            dependencies: Vec::new(),
            attachments: Vec::new(),
            status: SubtaskStatus::Queued,
        }
    }

    #[test]
    fn persist_decisions_writes_to_store() {
        let store = Arc::new(Mutex::new(Store::in_memory().unwrap()));
        // Minimal worker stand-in: we bypass the provider path and just
        // exercise the persistence helper directly. The provider trait
        // requires an HTTP client we don't want in a unit test.
        let st = Subtask {
            id: SubtaskId::new(),
            description: "create struct ExecutionGuard for tool gating".into(),
            model_tier: ModelTier::Standard,
            dependencies: Vec::new(),
            attachments: Vec::new(),
            status: SubtaskStatus::Queued,
        };
        let decisions = detect_decisions(&st, None);
        assert_eq!(decisions.len(), 1);
        {
            let guard = store.lock().unwrap();
            for d in &decisions {
                guard.append_memory(d).unwrap();
            }
        }
        let q = store
            .lock()
            .unwrap()
            .search_memory("ExecutionGuard", None, 10)
            .unwrap();
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn next_tier_escalates() {
        assert!(matches!(next_tier(ModelTier::Local), ModelTier::Cheap));
        assert!(matches!(next_tier(ModelTier::Cheap), ModelTier::Standard));
        assert!(matches!(
            next_tier(ModelTier::Standard),
            ModelTier::Frontier
        ));
        assert!(matches!(
            next_tier(ModelTier::Frontier),
            ModelTier::Frontier
        ));
    }
}
