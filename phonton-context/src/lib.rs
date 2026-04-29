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
use phonton_types::{ContextFrame, SliceOrigin};
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use phonton_types::{LLMResponse, ProviderKind};
    use std::sync::Mutex;

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
        ctx.push(verbatim(&format!("SYSTEM: {}", big(30)))).await.unwrap();
        ctx.push(verbatim(&format!("GOAL: {}", big(30)))).await.unwrap();
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
        ctx.push(summ(&format!("KEEP: {}", big(30)), 9)).await.unwrap();
        // Low-priority frames — should roll up once threshold crossed.
        for _ in 0..4 {
            ctx.push(summ(&big(100), 2)).await.unwrap();
        }
        // Expect: [keep, summary]  (exact sizes depend on ordering).
        assert!(ctx.frames().iter().any(|f| matches!(
            f,
            ContextFrame::Summarizable { priority: 9, .. }
        )));
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
}
