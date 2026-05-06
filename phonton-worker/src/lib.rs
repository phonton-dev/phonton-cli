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

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use phonton_context::{ContextManager, TiktokenCounter};
use phonton_mcp::{McpCallResult, McpRuntime, McpTool};
use phonton_providers::Provider;
use phonton_sandbox::Sandbox;
use phonton_store::Store;
use phonton_types::{
    CodeSlice, ContextFrame, DiffHunk, DiffLine, ExtensionId, MemoryRecord, ModelTier, Permission,
    PromptContextManifest, SliceOrigin, Subtask, SubtaskId, SubtaskResult, SubtaskStatus, TaskId,
    TokenUsage, VerifyLayer, VerifyResult,
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

/// Maximum verification attempts before escalating model tier.
pub const MAX_ATTEMPTS: u8 = 3;

/// Maximum MCP tool calls a worker may make for one subtask.
pub const MAX_MCP_CALLS_PER_SUBTASK: usize = 3;

/// Maximum MCP result characters fed back into the model.
pub const MCP_RESULT_MAX_CHARS: usize = 4_000;

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
    /// prepends the top-5 HNSW-retrieved slices to the user prompt under
    /// a `# Relevant code` section.
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
    /// Approval decisions are handled by the orchestrator layer, so this
    /// low-level executor only enforces terminal blocks.
    pub async fn execute_tool(&self, call: ToolCall) -> Result<String> {
        if let GuardDecision::Block { .. } = self.guard.evaluate(&call) {
            return Ok("[blocked by sandbox policy]".into());
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
        let model_tier = subtask.model_tier;
        let system_prompt = base_system_prompt();

        let relevant_slices: Vec<CodeSlice> = match &self.semantic {
            Some(ctx) => {
                phonton_index::query_relevant_slices(
                    &ctx.index,
                    &ctx.embedder,
                    &subtask.description,
                    5,
                )
                .await
            }
            None => Vec::new(),
        };
        let origins: Vec<SliceOrigin> = context_slices
            .iter()
            .chain(relevant_slices.iter())
            .map(|s| s.origin)
            .collect();

        let mut last_errors: Vec<String> = Vec::new();
        let mut total_tokens: u64 = 0;
        let mut token_usage = TokenUsage::default();
        let mut last_provider = phonton_types::ProviderKind::Anthropic;
        let mut last_model_name = String::new();
        let mut mcp_results: Vec<McpResultContext> = Vec::new();
        let mut mcp_calls = 0usize;

        for attempt in 1..=MAX_ATTEMPTS {
            let user_prompt = render_user_prompt(
                &subtask,
                &context_slices,
                &relevant_slices,
                &last_errors,
                self.mcp.as_deref(),
                &mcp_results,
            );

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
            let (rendered_context, full_prompt) = {
                let ctx = self.context.lock().await;
                let rendered_context = ctx.render();
                let full_prompt = render_full_prompt(&rendered_context, &user_prompt);
                (rendered_context, full_prompt)
            };
            if let Some(tx) = &self.msg_tx {
                let manifest = prompt_context_manifest(
                    &system_prompt,
                    &subtask,
                    &rendered_context,
                    last_errors.as_slice(),
                    mcp_results.as_slice(),
                    self.mcp.as_deref(),
                );
                let _ = tx.try_send(
                    phonton_types::messages::OrchestratorMessage::PromptManifest {
                        id: subtask.id,
                        manifest,
                    },
                );
            }

            let response = self
                .provider
                .call_with_attachments(&system_prompt, &full_prompt, &origins, &subtask.attachments)
                .await?;
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
                                content: truncate_chars(&rendered, MCP_RESULT_MAX_CHARS),
                            });
                            last_errors.clear();
                        }
                        Err(e) => {
                            mcp_results.push(McpResultContext {
                                server_id,
                                tool_name,
                                success: false,
                                content: truncate_chars(e.to_string(), MCP_RESULT_MAX_CHARS),
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
                    last_errors = vec![format!(
                        "Your previous response could not be parsed as a unified diff. \
                         Reply with ONLY a unified diff (no prose, no code fences). \
                         Each file starts with `--- a/<path>` then `+++ b/<path>`, \
                         followed by `@@ -old,n +new,n @@` hunk headers and \
                         ` `/`+`/`-` line prefixes. Parser said: {e}"
                    )];
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
                            content: format!(
                                "USER: {}\n\nASSISTANT: {}",
                                user_prompt, response.content
                            ),
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
                    warn!(
                        attempt,
                        ?layer,
                        n = errors.len(),
                        "verify failed; will retry"
                    );
                    last_errors = errors;
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

        // Exhausted MAX_ATTEMPTS retries at the current tier. Surface an
        // escalation with the last error set so the orchestrator can
        // re-dispatch at the next tier.
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
        let escalated = next_tier(model_tier);
        let reason = format!(
            "verify failed {MAX_ATTEMPTS} attempts at {model_tier}; escalating to {escalated}: {}",
            last_errors.join("; ")
        );
        Ok(failed_result(
            subtask.id,
            model_tier,
            VerifyResult::Escalate { reason },
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

/// One-step model-tier escalation table. `Frontier` already at the top
/// returns itself — orchestrator inspects this and decides whether to
/// surface the failure to the user.
fn next_tier(t: ModelTier) -> ModelTier {
    match t {
        ModelTier::Local => ModelTier::Cheap,
        ModelTier::Cheap => ModelTier::Standard,
        ModelTier::Standard => ModelTier::Frontier,
        ModelTier::Frontier => ModelTier::Frontier,
    }
}

// ---------------------------------------------------------------------------
// Prompt rendering and diff parsing
// ---------------------------------------------------------------------------

/// Base system prompt. Pinned `Verbatim` in the worker's context window —
/// Sent only in the provider system slot so it is not duplicated inside the
/// user-context render. The diff-only constraint is hard-coded here because
/// review and verification expect parseable unified diffs.
fn base_system_prompt() -> String {
    "You are a Phonton worker. You produce code changes as unified diffs ONLY.\n\
     Your output must be a single, parseable unified diff. NO PROSE. NO COMMENTARY. NO EXPLANATION.\n\
     The only exception is when the user prompt explicitly lists MCP servers: then you may output exactly one MCP_TOOL_CALL JSON marker instead of a diff, and nothing else. After MCP results are provided, return to unified-diff output.\n\n\
     EXAMPLE OF CREATING A NEW FILE `main.c`:\n\
     --- /dev/null\n\
     +++ b/main.c\n\
     @@ -0,0 +1,1 @@\n\
     +int main() { return 0; }\n\n\
     CRITICAL RULES:\n\
     1. START YOUR RESPONSE WITH `--- a/` OR `--- /dev/null`. DO NOT ADD ANY TEXT BEFORE IT.\n\
     2. Do NOT wrap your output in markdown code fences (```diff).\n\
     3. Do NOT explain what you are doing. Output the diff and nothing else.\n\
     4. Do NOT include unchanged code in the diff unless it is for context (max 3 lines).\n\
     5. If you have nothing to change, output an empty response.\n\
     6. If requesting MCP, output exactly `MCP_TOOL_CALL {\"server\":\"<server-id>\",\"tool\":\"<tool-name>\",\"arguments\":{...}}`.\n\
     7. ANY PROSE, COMMENTARY, OR NARRATION WILL CAUSE THE TASK TO FAIL.\n"
        .to_string()
}

fn render_user_prompt(
    subtask: &Subtask,
    slices: &[CodeSlice],
    relevant: &[CodeSlice],
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
    if !relevant.is_empty() {
        out.push_str("# Relevant code\n");
        for s in relevant {
            out.push_str(&format!(
                "- {} ({}): {}\n",
                s.symbol_name,
                s.file_path.display(),
                s.signature
            ));
        }
        out.push('\n');
    }
    out.push_str("# Subtask\n");
    out.push_str(&subtask.description);
    out.push_str("\n\n# Context slices\n");
    for s in slices {
        out.push_str(&format!(
            "- {} ({}): {}\n",
            s.symbol_name,
            s.file_path.display(),
            s.signature
        ));
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

fn prompt_context_manifest(
    system_prompt: &str,
    subtask: &Subtask,
    rendered_context: &str,
    prior_errors: &[String],
    mcp_results: &[McpResultContext],
    mcp: Option<&McpRuntime>,
) -> PromptContextManifest {
    let system_tokens = estimate_prompt_tokens(system_prompt);
    let user_goal_tokens = estimate_prompt_tokens(&subtask.description);
    let memory_tokens = estimate_prompt_tokens(rendered_context);
    let attachment_tokens = estimate_prompt_tokens(&phonton_types::render_prompt_attachments(
        &subtask.attachments,
    ));
    let retry_error_tokens = estimate_prompt_tokens(&prior_errors.join("\n"));
    let mut mcp_text = String::new();
    if let Some(runtime) = mcp {
        for server in runtime.servers() {
            mcp_text.push_str(server.id.as_str());
            mcp_text.push(' ');
            mcp_text.push_str(&server.name);
            mcp_text.push('\n');
        }
    }
    for result in mcp_results {
        mcp_text.push_str(&result.server_id.to_string());
        mcp_text.push(' ');
        mcp_text.push_str(&result.tool_name);
        mcp_text.push(' ');
        mcp_text.push_str(&result.content);
        mcp_text.push('\n');
    }
    let mcp_tool_tokens = estimate_prompt_tokens(&mcp_text);
    let total_estimated_tokens = system_tokens
        .saturating_add(user_goal_tokens)
        .saturating_add(memory_tokens)
        .saturating_add(attachment_tokens)
        .saturating_add(mcp_tool_tokens)
        .saturating_add(retry_error_tokens);

    PromptContextManifest {
        system_tokens,
        user_goal_tokens,
        memory_tokens,
        attachment_tokens,
        mcp_tool_tokens,
        retry_error_tokens,
        total_estimated_tokens,
    }
}

fn estimate_prompt_tokens(text: &str) -> u64 {
    if text.trim().is_empty() {
        return 0;
    }
    ((text.chars().count() as u64).saturating_add(3)) / 4
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

    fn guard() -> ExecutionGuard {
        ExecutionGuard::new(PathBuf::from("/work/proj"))
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
        let manifest = prompt_context_manifest(
            &base_system_prompt(),
            &subtask,
            "prior decision context",
            &["syntax failed".into()],
            &[],
            None,
        );
        assert!(manifest.system_tokens > 0);
        assert!(manifest.user_goal_tokens > 0);
        assert!(manifest.memory_tokens > 0);
        assert!(manifest.retry_error_tokens > 0);
        assert_eq!(
            manifest.total_estimated_tokens,
            manifest
                .system_tokens
                .saturating_add(manifest.user_goal_tokens)
                .saturating_add(manifest.memory_tokens)
                .saturating_add(manifest.attachment_tokens)
                .saturating_add(manifest.mcp_tool_tokens)
                .saturating_add(manifest.retry_error_tokens)
        );
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
        let prompt = render_user_prompt(&subtask, &[], &[], &[], Some(&runtime), &results);
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
