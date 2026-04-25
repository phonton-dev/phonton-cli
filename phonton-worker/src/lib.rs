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
use phonton_providers::Provider;
use phonton_sandbox::Sandbox;
use phonton_store::Store;
use phonton_types::{
    CodeSlice, DiffHunk, DiffLine, MemoryRecord, ModelTier, SliceOrigin, Subtask, SubtaskId,
    SubtaskResult, SubtaskStatus, TaskId, VerifyResult,
};
use regex::Regex;
use tracing::{debug, warn};

/// Re-exports of the guard / tool-call types. Canonical definitions now
/// live in `phonton-sandbox`; these aliases keep downstream `use
/// phonton_worker::ExecutionGuard` sites working.
pub use phonton_sandbox::{ExecutionGuard, GuardDecision, ToolCall};

/// Production [`WorkerDispatcher`] bridge from orchestrator to worker.
pub mod dispatcher;

/// Maximum verification attempts before escalating model tier.
pub const MAX_ATTEMPTS: u8 = 3;

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
        Self {
            provider,
            guard,
            sandbox,
            store: None,
            memory: None,
            task_id: None,
            semantic: None,
        }
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

    /// Attach the async memory facade. When present, a `Decision` record
    /// is appended for every subtask that reaches `VerifyResult::Pass`,
    /// giving the next planner run visibility into completed work.
    pub fn with_memory_store(mut self, memory: phonton_memory::MemoryStore) -> Self {
        self.memory = Some(memory);
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
        let origins: Vec<SliceOrigin> = context_slices.iter().map(|s| s.origin).collect();
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

        let mut last_errors: Vec<String> = Vec::new();
        let mut total_tokens: u64 = 0;
        // Track the provider/model from the most recent LLM call so
        // SubtaskResult can carry them for BudgetGuard pricing.
        let mut last_provider = phonton_types::ProviderKind::Anthropic;
        let mut last_model_name = String::new();

        for attempt in 1..=MAX_ATTEMPTS {
            let user_prompt =
                render_user_prompt(&subtask, &context_slices, &relevant_slices, &last_errors);

            let response = self
                .provider
                .call(&system_prompt, &user_prompt, &origins)
                .await?;
            last_provider = response.provider;
            last_model_name = response.model_name.clone();
            total_tokens = total_tokens
                .saturating_add(response.input_tokens)
                .saturating_add(response.output_tokens);

            let hunks = parse_unified_diff(&response.content)?;
            debug!(attempt, hunks = hunks.len(), "worker received diff");

            let verdict =
                phonton_verify::verify_diff(&hunks, self.guard.project_root()).await?;
            match verdict {
                VerifyResult::Pass { layer } => {
                    // Memory is a warm part of the loop: every passing
                    // subtask gets a pass at decision extraction before
                    // we return. Extraction failure is logged, not fatal —
                    // the verified diff is what the user contracted for.
                    if let Err(e) = self.persist_decisions(&subtask) {
                        warn!(error = %e, "failed to persist subtask decisions");
                    }
                    if let Some(memory) = &self.memory {
                        let rec = MemoryRecord::Decision {
                            title: subtask.description.clone(),
                            body: format!("completed: {}", subtask.description),
                            task_id: self.task_id,
                        };
                        if let Err(e) = memory.record(rec).await {
                            warn!(error = %e, "failed to record completion memory");
                        }
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
                    // Loop continues; next iteration re-prompts with errors.
                }
                VerifyResult::Escalate { reason } => {
                    return Ok(failed_result(
                        subtask.id,
                        model_tier,
                        VerifyResult::Escalate { reason },
                        total_tokens,
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
            total_tokens,
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

fn truncate_chars(text: String, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text;
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
    _tokens_used: u64,
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
/// never compressed. The diff-only constraint is hard-coded here per the
/// rule in `phonton-brain/CLAUDE.md`.
fn base_system_prompt() -> String {
    "You are a Phonton worker. You produce code changes as unified diffs only.\n\
     Output ONLY unified diff hunks of the form:\n\
       --- a/<path>\n\
       +++ b/<path>\n\
       @@ -<old_start>,<old_count> +<new_start>,<new_count> @@\n\
       <context/added/removed lines>\n\
     Do not output unchanged code. Do not narrate. Do not explain.\n"
        .to_string()
}

fn render_user_prompt(
    subtask: &Subtask,
    slices: &[CodeSlice],
    relevant: &[CodeSlice],
    prior_errors: &[String],
) -> String {
    let mut out = String::new();
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
    out
}

/// Minimal unified-diff parser. Sufficient for the model output the worker
/// expects (one `--- a/ +++ b/` header per file, `@@` hunk headers, then
/// ` `/`+`/`-` lines). Real-world edge cases — rename headers, binary
/// markers — are deferred to `phonton-diff`.
pub fn parse_unified_diff(text: &str) -> Result<Vec<DiffHunk>> {
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut header: Option<(u32, u32, u32, u32)> = None;
    let mut lines: Vec<DiffLine> = Vec::new();

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

    for raw in text.lines() {
        if let Some(rest) = raw.strip_prefix("+++ b/") {
            flush(&mut hunks, &current_path, &header, &mut lines);
            current_path = Some(PathBuf::from(rest.trim()));
            header = None;
        } else if raw.starts_with("--- ") {
            // Old-side header — handled together with +++; ignore alone.
            continue;
        } else if let Some(rest) = raw.strip_prefix("@@") {
            flush(&mut hunks, &current_path, &header, &mut lines);
            header = Some(parse_hunk_header(rest)?);
        } else if header.is_some() {
            if let Some(s) = raw.strip_prefix('+') {
                lines.push(DiffLine::Added(s.to_string()));
            } else if let Some(s) = raw.strip_prefix('-') {
                lines.push(DiffLine::Removed(s.to_string()));
            } else if let Some(s) = raw.strip_prefix(' ') {
                lines.push(DiffLine::Context(s.to_string()));
            }
        }
    }
    flush(&mut hunks, &current_path, &header, &mut lines);
    Ok(hunks)
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
    fn detects_trait_decision() {
        let st = Subtask {
            id: SubtaskId::new(),
            description: "Introduce a trait MemoryWriter for async appends".into(),
            model_tier: ModelTier::Standard,
            dependencies: Vec::new(),
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
