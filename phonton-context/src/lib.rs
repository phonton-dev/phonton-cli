//! Sliding-window context compression for phonton workers.
//!
//! A [`ContextManager`] owns the ordered frame list a worker feeds into each
//! LLM call. Every [`push`](ContextManager::push) recomputes the total token
//! count; when the window crosses the compression threshold (default 80% of
//! the configured model limit), the manager compresses the lowest-priority
//! [`ContextFrame::Summarizable`] frames into a single "History Summary"
//! frame by calling out to `phonton-providers` at the `Cheap` tier.
//!
//! Invariants (enforced, not documented):
//!
//! * [`ContextFrame::Verbatim`] frames are **never** evicted or rewritten.
//!   The system prompt and the task goal are pinned for the lifetime of the
//!   worker session.
//! * Compression targets priorities 1–3 only. Higher-priority Summarizable
//!   frames are kept verbatim until the low tier is exhausted.
//! * The summary frame inserted back into the window is itself
//!   `Summarizable` at priority [`SUMMARY_PRIORITY`] — above the eviction
//!   band, so it is not immediately re-compressed on the next push.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use phonton_providers::Provider;
use phonton_types::{
    CodeSlice, ContextFrame, ContextPlan, ContextPlanItem, ContextPlanKind, SliceOrigin,
};
use tracing::{debug, warn};

/// Default fraction of the window at which compression fires.
pub const DEFAULT_THRESHOLD_RATIO: f32 = 0.80;

/// Priority assigned to the synthetic "History Summary" frame.
///
/// Chosen above the 1–3 eviction band so the summary survives subsequent
/// compression passes. A second round of growth will compress newer
/// low-priority frames around it.
pub const SUMMARY_PRIORITY: u8 = 5;

/// Priorities eligible for compression.
pub const COMPRESS_MIN: u8 = 1;
/// Priorities eligible for compression.
pub const COMPRESS_MAX: u8 = 3;

/// Default target for one worker prompt after context compilation.
///
/// This is deliberately far below common model windows. The compiler can
/// spend more when fixed sections already exceed the target, but the default
/// posture is "smallest context that can still verify."
pub const DEFAULT_WORKER_CONTEXT_TARGET_TOKENS: u64 = 3_500;

// ---------------------------------------------------------------------------
// Token counting
// ---------------------------------------------------------------------------

/// Strategy for estimating the token footprint of a string.
///
/// Production builds plug in a real BPE-based counter; tests use the
/// deterministic [`CharHeuristic`].
pub trait TokenCounter: Send + Sync + 'static {
    /// Estimated token count for `s`.
    fn count(&self, s: &str) -> usize;
}

// ---------------------------------------------------------------------------
// Context compiler
// ---------------------------------------------------------------------------

/// Request for compiling one worker prompt context.
#[derive(Debug, Clone)]
pub struct ContextPlanRequest<'a> {
    /// Current subtask or goal text.
    pub goal: &'a str,
    /// Candidate repository slices, already ranked by semantic relevance.
    pub candidate_slices: &'a [CodeSlice],
    /// Estimated system-prompt tokens.
    pub system_tokens: u64,
    /// Estimated retained memory/context tokens.
    pub memory_tokens: u64,
    /// Estimated attachment/artifact tokens.
    pub attachment_tokens: u64,
    /// Estimated retry-diagnostic tokens.
    pub retry_error_tokens: u64,
    /// Estimated MCP/tool tokens.
    pub mcp_tool_tokens: u64,
    /// Hard provider/model context limit when known.
    pub budget_limit: Option<u64>,
    /// Desired prompt target. Defaults to
    /// [`DEFAULT_WORKER_CONTEXT_TARGET_TOKENS`].
    pub target_tokens: Option<u64>,
}

/// Result of context compilation: a typed plan plus the selected slices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledContextPlan {
    /// Auditable token/context decision.
    pub plan: ContextPlan,
    /// Candidate slices allowed into the provider prompt.
    pub selected_slices: Vec<CodeSlice>,
}

/// Deterministic context budgeter for worker prompts.
///
/// It does not call a model. It turns ranked candidate slices into a bounded
/// worker packet and records every inclusion/omission as a [`ContextPlan`].
pub struct ContextCompiler {
    counter: Arc<dyn TokenCounter>,
}

impl ContextCompiler {
    /// Build a compiler using the cheap character heuristic.
    pub fn new() -> Self {
        Self {
            counter: Arc::new(CharHeuristic),
        }
    }

    /// Build a compiler with a caller-provided tokenizer.
    pub fn with_counter(counter: Arc<dyn TokenCounter>) -> Self {
        Self { counter }
    }

    /// Compile one prompt context plan from ranked candidates.
    pub fn compile(&self, request: ContextPlanRequest<'_>) -> CompiledContextPlan {
        let target = request
            .target_tokens
            .unwrap_or(DEFAULT_WORKER_CONTEXT_TARGET_TOKENS)
            .min(request.budget_limit.unwrap_or(u64::MAX))
            .max(512);
        let goal_tokens = self.count(request.goal);
        let fixed_tokens = request
            .system_tokens
            .saturating_add(goal_tokens)
            .saturating_add(request.memory_tokens)
            .saturating_add(request.attachment_tokens)
            .saturating_add(request.retry_error_tokens)
            .saturating_add(request.mcp_tool_tokens);

        let repo_map_items = self.repo_map_items(request.candidate_slices, 12);
        let repo_map_tokens = repo_map_items
            .iter()
            .filter(|item| item.included)
            .map(|item| item.estimated_tokens)
            .sum::<u64>();
        let mut remaining = target.saturating_sub(fixed_tokens.saturating_add(repo_map_tokens));

        let mut selected_slices = Vec::new();
        let mut selected_code_tokens = 0u64;
        let mut omitted_code_tokens = 0u64;
        let mut items = Vec::new();
        items.push(ContextPlanItem {
            kind: ContextPlanKind::Goal,
            id: "goal".into(),
            summary: truncate_for_summary(request.goal, 120),
            estimated_tokens: goal_tokens,
            included: true,
            reason: "current task is always included".into(),
        });
        items.extend(repo_map_items);

        for slice in request.candidate_slices {
            let estimated = self.slice_tokens(slice);
            let id = format!("{}#{}", slice.file_path.display(), slice.symbol_name);
            let summary = format!("{} {}", slice.symbol_name, slice.signature);
            if estimated <= remaining || selected_slices.is_empty() {
                remaining = remaining.saturating_sub(estimated);
                selected_code_tokens = selected_code_tokens.saturating_add(estimated);
                selected_slices.push(slice.clone());
                items.push(ContextPlanItem {
                    kind: ContextPlanKind::CodeSlice,
                    id,
                    summary: truncate_for_summary(&summary, 180),
                    estimated_tokens: estimated,
                    included: true,
                    reason: "ranked relevant slice fits the context budget".into(),
                });
            } else {
                omitted_code_tokens = omitted_code_tokens.saturating_add(estimated);
                items.push(ContextPlanItem {
                    kind: ContextPlanKind::CodeSlice,
                    id,
                    summary: truncate_for_summary(&summary, 180),
                    estimated_tokens: estimated,
                    included: false,
                    reason: "omitted to stay within the worker context target".into(),
                });
            }
        }

        let estimated_total_tokens = fixed_tokens
            .saturating_add(repo_map_tokens)
            .saturating_add(selected_code_tokens);

        CompiledContextPlan {
            plan: ContextPlan {
                budget_limit: request.budget_limit,
                target_tokens: target,
                fixed_tokens,
                repo_map_tokens,
                selected_code_tokens,
                omitted_code_tokens,
                estimated_total_tokens,
                items,
            },
            selected_slices,
        }
    }

    fn repo_map_items(&self, slices: &[CodeSlice], max_items: usize) -> Vec<ContextPlanItem> {
        slices
            .iter()
            .take(max_items)
            .map(|slice| {
                let summary = format!("{}: {}", slice.file_path.display(), slice.symbol_name);
                ContextPlanItem {
                    kind: ContextPlanKind::RepoMap,
                    id: slice.file_path.display().to_string(),
                    estimated_tokens: self.count(&summary),
                    summary,
                    included: true,
                    reason: "compact orientation for ranked candidate context".into(),
                }
            })
            .collect()
    }

    fn slice_tokens(&self, slice: &CodeSlice) -> u64 {
        let fallback = self.count(&format!(
            "{} {} {}",
            slice.file_path.display(),
            slice.symbol_name,
            slice.signature
        ));
        (slice.token_count as u64).max(fallback).max(1)
    }

    fn count(&self, text: &str) -> u64 {
        self.counter.count(text) as u64
    }
}

impl Default for ContextCompiler {
    fn default() -> Self {
        Self::new()
    }
}

/// Four-chars-per-token heuristic. Cheap, provider-agnostic, and good
/// enough for budget decisions; real counts land inside [`Provider::call`].
#[derive(Debug, Default, Clone, Copy)]
pub struct CharHeuristic;

impl TokenCounter for CharHeuristic {
    fn count(&self, s: &str) -> usize {
        // Divide char count by 4, rounding up so short strings still cost 1.
        s.chars().count().div_ceil(4)
    }
}

/// Real BPE counter backed by `tiktoken-rs`'s `cl100k_base` encoding.
///
/// `cl100k_base` is GPT-4/3.5-turbo's BPE and tracks Claude's published
/// token counts closely enough for budget decisions. Construction loads
/// the merge table from an embedded bundle; cache the instance where
/// possible rather than building one per call.
pub struct TiktokenCounter {
    bpe: tiktoken_rs::CoreBPE,
}

impl TiktokenCounter {
    /// Build a counter bound to the `cl100k_base` encoding.
    ///
    /// Returns `Err` only if the embedded merge table fails to load —
    /// effectively never in a correctly-packaged build.
    pub fn new() -> Result<Self> {
        let bpe = tiktoken_rs::cl100k_base()
            .map_err(|e| anyhow!("failed to load cl100k_base encoding: {e}"))?;
        Ok(Self { bpe })
    }
}

impl TokenCounter for TiktokenCounter {
    fn count(&self, s: &str) -> usize {
        // `encode_ordinary` skips special-token handling — correct for
        // budget accounting where `<|endoftext|>` and friends should
        // cost whatever their literal BPE encoding costs.
        self.bpe.encode_ordinary(s).len()
    }
}

// ---------------------------------------------------------------------------
// ContextManager
// ---------------------------------------------------------------------------

/// Sliding-window frame manager with automatic summarisation.
pub struct ContextManager {
    frames: Vec<ContextFrame>,
    limit_tokens: usize,
    threshold_ratio: f32,
    provider: Arc<dyn Provider>,
    counter: Arc<dyn TokenCounter>,
}

impl ContextManager {
    /// Construct a manager bound to `provider`, with a token `limit_tokens`
    /// (typically the model's context window) and the default 80%
    /// compression threshold.
    pub fn new(provider: Arc<dyn Provider>, limit_tokens: usize) -> Self {
        Self {
            frames: Vec::new(),
            limit_tokens,
            threshold_ratio: DEFAULT_THRESHOLD_RATIO,
            provider,
            counter: Arc::new(CharHeuristic),
        }
    }

    /// Override the compression threshold (fraction of `limit_tokens`).
    /// Clamped to `(0.0, 1.0]`.
    pub fn with_threshold(mut self, ratio: f32) -> Self {
        self.threshold_ratio = ratio.clamp(f32::MIN_POSITIVE, 1.0);
        self
    }

    /// Swap in a non-default token counter (e.g. a real BPE tokenizer).
    pub fn with_counter(mut self, counter: Arc<dyn TokenCounter>) -> Self {
        self.counter = counter;
        self
    }

    /// Current frame list in insertion order.
    pub fn frames(&self) -> &[ContextFrame] {
        &self.frames
    }

    /// Total estimated tokens across the whole window.
    pub fn total_tokens(&self) -> usize {
        self.frames
            .iter()
            .map(|f| self.counter.count(frame_content(f)))
            .sum()
    }

    /// Render all frames into a single string for LLM consumption.
    pub fn render(&self) -> String {
        self.frames
            .iter()
            .map(frame_content)
            .collect::<Vec<&str>>()
            .join("\n\n")
    }

    /// Configured token ceiling.
    pub fn limit_tokens(&self) -> usize {
        self.limit_tokens
    }

    /// Compression trip point in tokens.
    pub fn compress_threshold(&self) -> usize {
        ((self.limit_tokens as f32) * self.threshold_ratio) as usize
    }

    /// Alias for [`push`](Self::push). Matches the API naming used in
    /// the context-compression spec.
    pub async fn push_frame(&mut self, frame: ContextFrame) -> Result<bool> {
        self.push(frame).await
    }

    /// Alias for [`compress`](Self::compress). Matches the API naming
    /// used in the context-compression spec.
    pub async fn compress_frames(&mut self) -> Result<bool> {
        self.compress().await
    }

    /// Append `frame` and, if the window now sits above the compression
    /// threshold, compress the lowest-priority `Summarizable` frames into
    /// a single "History Summary".
    ///
    /// Returns `Ok(true)` when a compression pass actually ran.
    pub async fn push(&mut self, frame: ContextFrame) -> Result<bool> {
        self.frames.push(frame);

        if self.total_tokens() < self.compress_threshold() {
            return Ok(false);
        }

        debug!(
            tokens = self.total_tokens(),
            threshold = self.compress_threshold(),
            "context window over threshold; compressing"
        );
        self.compress().await
    }

    /// Run one compression pass. Public for explicit invocation — the `push`
    /// path calls this internally.
    ///
    /// Returns `Ok(true)` if any frames were actually compressed, `Ok(false)`
    /// if no compressible frames existed (i.e. the window is dominated by
    /// `Verbatim` frames and the caller must raise the budget).
    pub async fn compress(&mut self) -> Result<bool> {
        let indices = self.compressible_indices();
        if indices.is_empty() {
            warn!("compression requested but no Summarizable frames in 1..=3 priority band");
            return Ok(false);
        }

        let bodies: Vec<String> = indices
            .iter()
            .map(|&i| frame_content(&self.frames[i]).to_string())
            .collect();

        let summary = self.summarise(&bodies).await?;
        self.replace_with_summary(&indices, summary);
        Ok(true)
    }

    /// Indices of all `Summarizable` frames whose priority is in
    /// `[COMPRESS_MIN, COMPRESS_MAX]`, sorted ascending (original order).
    fn compressible_indices(&self) -> Vec<usize> {
        self.frames
            .iter()
            .enumerate()
            .filter_map(|(i, f)| match f {
                ContextFrame::Summarizable { priority, .. }
                    if (COMPRESS_MIN..=COMPRESS_MAX).contains(priority) =>
                {
                    Some(i)
                }
                _ => None,
            })
            .collect()
    }

    /// Call the provider to reduce `bodies` to a single short summary.
    async fn summarise(&self, bodies: &[String]) -> Result<String> {
        if bodies.is_empty() {
            return Err(anyhow!("summarise called with no bodies"));
        }
        let system = "You are a context compressor. Reduce the supplied \
                      frames to a terse factual summary — preserve \
                      identifiers, decisions, file paths, and error \
                      signatures. Drop narration.";
        let mut frames_text = String::new();
        for (i, body) in bodies.iter().enumerate() {
            frames_text.push_str(&format!("--- frame {} ---\n{body}\n\n", i + 1));
        }
        let user = format!(
            "Summarize the following context to under 200 tokens, preserving key decisions and file paths:\n\n{frames_text}"
        );
        let resp = self
            .provider
            .call(system, &user, &[] as &[SliceOrigin])
            .await?;
        let trimmed = resp.content.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("provider returned empty summary"));
        }
        Ok(format!("History Summary:\n{trimmed}"))
    }

    /// Remove `indices` (descending) and insert the summary frame at the
    /// position of the first removed index, preserving relative order of
    /// everything else.
    fn replace_with_summary(&mut self, indices: &[usize], summary: String) {
        let insert_at = match indices.first() {
            Some(&i) => i,
            None => return,
        };
        for &i in indices.iter().rev() {
            self.frames.remove(i);
        }
        self.frames.insert(
            insert_at.min(self.frames.len()),
            ContextFrame::Summarizable {
                content: summary,
                priority: SUMMARY_PRIORITY,
            },
        );
    }
}

fn frame_content(f: &ContextFrame) -> &str {
    match f {
        ContextFrame::Verbatim(s) => s,
        ContextFrame::Summarizable { content, .. } => content,
    }
}

fn truncate_for_summary(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use phonton_types::{LLMResponse, ProviderKind};

    /// Test provider: returns a fixed summary and records every call.
    #[derive(Clone)]
    struct StubProvider {
        summary: String,
        calls: Arc<Mutex<usize>>,
    }

    impl StubProvider {
        fn new(summary: &str) -> Self {
            Self {
                summary: summary.into(),
                calls: Arc::new(Mutex::new(0)),
            }
        }
    }

    #[async_trait]
    impl Provider for StubProvider {
        async fn call(
            &self,
            _system: &str,
            _user: &str,
            _slice_origins: &[SliceOrigin],
        ) -> Result<LLMResponse> {
            *self.calls.lock().unwrap() += 1;
            Ok(LLMResponse {
                content: self.summary.clone(),
                input_tokens: 10,
                output_tokens: 5,
                cached_tokens: 0,
                cache_creation_tokens: 0,
                provider: ProviderKind::Anthropic,
                model_name: "stub".into(),
            })
        }

        fn kind(&self) -> ProviderKind {
            ProviderKind::Anthropic
        }

        fn model(&self) -> String {
            "stub".into()
        }

        fn clone_box(&self) -> Box<dyn Provider> {
            Box::new(self.clone())
        }
    }

    fn verbatim(s: &str) -> ContextFrame {
        ContextFrame::Verbatim(s.into())
    }

    fn summ(s: &str, priority: u8) -> ContextFrame {
        ContextFrame::Summarizable {
            content: s.into(),
            priority,
        }
    }

    fn big(n: usize) -> String {
        "x".repeat(n)
    }

    #[tokio::test]
    async fn push_below_threshold_does_not_compress() {
        let provider = Arc::new(StubProvider::new("summary"));
        let mut ctx = ContextManager::new(provider.clone(), 1000);
        let compressed = ctx.push(summ(&big(40), 2)).await.unwrap(); // ~10 tokens
        assert!(!compressed);
        assert_eq!(*provider.calls.lock().unwrap(), 0);
        assert_eq!(ctx.frames().len(), 1);
    }

    #[tokio::test]
    async fn push_over_threshold_compresses_low_priority() {
        let provider = Arc::new(StubProvider::new("rolled-up history"));
        let mut ctx = ContextManager::new(provider.clone(), 100);
        // Fill window with low-priority frames. Threshold = 80 tokens;
        // 4 x 100 chars = 4 x 25 = 100 tokens — over.
        for _ in 0..4 {
            ctx.push(summ(&big(100), 2)).await.unwrap();
        }
        assert_eq!(*provider.calls.lock().unwrap(), 1);
        // Post-compression: exactly one Summarizable frame at SUMMARY_PRIORITY.
        assert_eq!(ctx.frames().len(), 1);
        match &ctx.frames()[0] {
            ContextFrame::Summarizable { content, priority } => {
                assert_eq!(*priority, SUMMARY_PRIORITY);
                assert!(content.starts_with("History Summary:"));
                assert!(content.contains("rolled-up history"));
            }
            _ => panic!("expected Summarizable summary"),
        }
    }

    #[tokio::test]
    async fn verbatim_is_never_evicted() {
        let provider = Arc::new(StubProvider::new("sum"));
        let mut ctx = ContextManager::new(provider.clone(), 100);
        ctx.push(verbatim(&format!("SYSTEM: {}", big(30))))
            .await
            .unwrap();
        ctx.push(verbatim(&format!("GOAL: {}", big(30))))
            .await
            .unwrap();
        // Now add a pile of low-priority summarizables.
        for _ in 0..4 {
            ctx.push(summ(&big(40), 1)).await.unwrap();
        }
        // Verbatim frames must still be present, in original order.
        let verbatims: Vec<&ContextFrame> = ctx
            .frames()
            .iter()
            .filter(|f| matches!(f, ContextFrame::Verbatim(_)))
            .collect();
        assert_eq!(verbatims.len(), 2);
        match verbatims[0] {
            ContextFrame::Verbatim(s) => assert!(s.starts_with("SYSTEM:")),
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn high_priority_summarizables_survive() {
        let provider = Arc::new(StubProvider::new("sum"));
        let mut ctx = ContextManager::new(provider.clone(), 100);
        // One high-priority (9) frame — must survive.
        ctx.push(summ(&format!("KEEP: {}", big(30)), 9))
            .await
            .unwrap();
        // Low-priority frames — should roll up once threshold crossed.
        for _ in 0..4 {
            ctx.push(summ(&big(100), 2)).await.unwrap();
        }
        // Expect: [keep, summary]  (exact sizes depend on ordering).
        assert!(ctx
            .frames()
            .iter()
            .any(|f| matches!(f, ContextFrame::Summarizable { priority: 9, .. })));
        assert!(ctx.frames().iter().any(|f| matches!(
            f,
            ContextFrame::Summarizable { priority: p, content } if *p == SUMMARY_PRIORITY && content.starts_with("History Summary:")
        )));
    }

    #[tokio::test]
    async fn no_compressible_frames_is_noop() {
        let provider = Arc::new(StubProvider::new("sum"));
        let mut ctx = ContextManager::new(provider.clone(), 100);
        // Only high-priority Summarizables and Verbatims — nothing in 1..=3.
        for _ in 0..4 {
            ctx.push(summ(&big(40), 8)).await.unwrap();
        }
        // Nothing was eligible, so no provider call was made.
        assert_eq!(*provider.calls.lock().unwrap(), 0);
        assert_eq!(ctx.frames().len(), 4);
    }

    #[tokio::test]
    async fn summary_frame_not_immediately_re_compressed() {
        let provider = Arc::new(StubProvider::new("sum"));
        let mut ctx = ContextManager::new(provider.clone(), 100);
        for _ in 0..4 {
            ctx.push(summ(&big(100), 2)).await.unwrap();
        }
        // First pass compressed.
        assert_eq!(*provider.calls.lock().unwrap(), 1);
        // Adding a Verbatim frame keeps us over threshold but the surviving
        // summary sits at SUMMARY_PRIORITY (>3) and Verbatim is pinned, so
        // no second compression happens.
        ctx.push(verbatim(&big(200))).await.unwrap();
        assert_eq!(*provider.calls.lock().unwrap(), 1);
    }

    #[test]
    fn char_heuristic_counts() {
        let c = CharHeuristic;
        assert_eq!(c.count(""), 0);
        assert_eq!(c.count("abcd"), 1);
        assert_eq!(c.count("abcde"), 2);
    }

    #[tokio::test]
    async fn compression_reduces_frame_count_via_mock_provider() {
        let provider = Arc::new(StubProvider::new("compressed digest"));
        let mut ctx = ContextManager::new(provider.clone(), 100);
        // 5 compressible frames, each ~25 tokens under CharHeuristic.
        for i in 0..5 {
            ctx.push_frame(summ(&big(100 + i), 2)).await.unwrap();
        }
        // Provider was invoked at least once to produce the rollup.
        assert!(*provider.calls.lock().unwrap() >= 1);
        // Net: fewer frames than the 5 we pushed. Strict reduction.
        assert!(
            ctx.frames().len() < 5,
            "expected compression to reduce frame count, got {}",
            ctx.frames().len()
        );
        // And at least one of the surviving frames is the synthetic
        // summary produced by the mock provider.
        assert!(ctx.frames().iter().any(|f| matches!(
            f,
            ContextFrame::Summarizable { content, .. } if content.contains("compressed digest")
        )));
    }

    #[test]
    fn tiktoken_counter_returns_positive_counts() {
        let c = TiktokenCounter::new().expect("load cl100k_base");
        assert_eq!(c.count(""), 0);
        // Non-empty text must cost at least one token.
        assert!(c.count("hello world") > 0);
        // And a much longer string must cost more.
        assert!(c.count(&"hello world ".repeat(50)) > c.count("hello world"));
    }

    #[test]
    fn context_compiler_keeps_ranked_slices_under_budget() {
        let slices = vec![
            code_slice("src/a.rs", "alpha", 100),
            code_slice("src/b.rs", "beta", 900),
            code_slice("src/c.rs", "gamma", 100),
        ];
        let compiled = ContextCompiler::default().compile(ContextPlanRequest {
            goal: "change alpha",
            candidate_slices: &slices,
            system_tokens: 100,
            memory_tokens: 0,
            attachment_tokens: 0,
            retry_error_tokens: 0,
            mcp_tool_tokens: 0,
            budget_limit: Some(450),
            target_tokens: Some(450),
        });

        assert_eq!(compiled.selected_slices.len(), 2);
        assert!(compiled
            .selected_slices
            .iter()
            .any(|slice| slice.symbol_name == "alpha"));
        assert!(compiled.plan.omitted_code_tokens > 0);
        assert!(compiled.plan.estimated_total_tokens <= compiled.plan.target_tokens);
        assert!(compiled
            .plan
            .items
            .iter()
            .any(|item| item.kind == ContextPlanKind::RepoMap && item.included));
        assert!(compiled
            .plan
            .items
            .iter()
            .any(|item| item.kind == ContextPlanKind::CodeSlice && !item.included));
    }

    #[test]
    fn context_compiler_selects_at_least_one_slice_when_fixed_budget_is_full() {
        let slices = vec![code_slice("src/large.rs", "large", 700)];
        let compiled = ContextCompiler::default().compile(ContextPlanRequest {
            goal: "fix large",
            candidate_slices: &slices,
            system_tokens: 900,
            memory_tokens: 0,
            attachment_tokens: 0,
            retry_error_tokens: 0,
            mcp_tool_tokens: 0,
            budget_limit: Some(1_000),
            target_tokens: Some(1_000),
        });

        assert_eq!(compiled.selected_slices.len(), 1);
        assert!(compiled.plan.estimated_total_tokens > compiled.plan.target_tokens);
    }

    fn code_slice(path: &str, symbol: &str, token_count: usize) -> CodeSlice {
        CodeSlice {
            file_path: PathBuf::from(path),
            symbol_name: symbol.into(),
            signature: format!("fn {symbol}()"),
            docstring: None,
            callsites: Vec::new(),
            token_count,
            origin: SliceOrigin::Semantic,
        }
    }
}
