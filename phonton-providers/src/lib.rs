//! BYOK multi-provider routing: Anthropic, OpenAI, OpenRouter, Gemini, Ollama.
//!
//! Concrete adaptors live behind a single [`Provider`] trait so the worker
//! never touches a provider-specific payload. Cached-token accounting is
//! preserved end-to-end — Anthropic's `cache_read_input_tokens` flows
//! straight into [`LLMResponse::cached_tokens`]; providers without prompt
//! caching report `0`.
//!
//! Context-confidence handling: callers pass `slice_origins` describing the
//! provenance of every [`phonton_types::CodeSlice`] they fed into the
//! prompt. If any are [`SliceOrigin::Fallback`], [`build_system_prompt`]
//! prepends a "Low Confidence Context" banner so the model treats the
//! slices with caution. See `01-architecture/failure-modes.md` Risk 3.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use phonton_types::{
    LLMResponse, ModelMetricsSnapshot, ProviderConfig, ProviderError, ProviderKind, SliceOrigin,
};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};

/// Discover the list of models a given API key has access to. Returns
/// model identifiers in the form the corresponding [`Provider`] expects
/// in [`ProviderConfig`].
///
/// `name` is the lowercase provider name as accepted by the CLI (one of
/// `KNOWN_PROVIDERS`). `base_url` is honoured for `custom` /
/// `openai-compatible` and overrides the default endpoint for OpenAI-style
/// providers when provided.
///
/// Network errors and non-2xx responses bubble up as `Err`. An empty list
/// is *not* an error — callers must treat it as "key is valid but no
/// models accessible".
pub async fn discover_models(
    name: &str,
    api_key: &str,
    base_url: Option<&str>,
) -> Result<Vec<String>> {
    let http = Client::new();
    match name {
        "anthropic" => discover_anthropic(&http, api_key).await,
        "openai" => {
            discover_openai_style(&http, api_key, "https://api.openai.com/v1/models").await
        }
        "openrouter" => {
            // OpenRouter's /models is public but accept the key anyway.
            discover_openai_style(
                &http,
                api_key,
                "https://openrouter.ai/api/v1/models",
            )
            .await
        }
        "agentrouter" => {
            discover_openai_style(
                &http,
                api_key,
                "https://agentrouter.org/v1/models",
            )
            .await
        }
        "deepseek" => {
            discover_openai_style(&http, api_key, "https://api.deepseek.com/v1/models").await
        }
        "xai" | "grok" => {
            discover_openai_style(&http, api_key, "https://api.x.ai/v1/models").await
        }
        "groq" => {
            discover_openai_style(&http, api_key, "https://api.groq.com/openai/v1/models")
                .await
        }
        "together" => {
            discover_openai_style(&http, api_key, "https://api.together.xyz/v1/models").await
        }
        "gemini" => discover_gemini(&http, api_key).await,
        "ollama" => {
            let base = base_url.unwrap_or("http://localhost:11434").trim_end_matches('/');
            discover_ollama(&http, base).await
        }
        "custom" | "openai-compatible" => {
            let base = base_url
                .ok_or_else(|| anyhow!("custom provider needs a base_url"))?
                .trim_end_matches('/');
            let url = format!("{base}/models");
            discover_openai_style(&http, api_key, &url).await
        }
        _ => Err(anyhow!("unknown provider `{name}`")),
    }
}

async fn discover_openai_style(http: &Client, api_key: &str, url: &str) -> Result<Vec<String>> {
    let resp = http
        .get(url)
        .bearer_auth(api_key)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("{} returned HTTP {}", url, resp.status()));
    }
    let v: Value = resp.json().await.context("parse models JSON")?;
    let arr = v
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("models response missing `data` array"))?;
    let mut out: Vec<String> = arr
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

async fn discover_anthropic(http: &Client, api_key: &str) -> Result<Vec<String>> {
    let url = "https://api.anthropic.com/v1/models";
    let resp = http
        .get(url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .context("GET anthropic /v1/models")?;
    if !resp.status().is_success() {
        return Err(anyhow!("anthropic /v1/models returned HTTP {}", resp.status()));
    }
    let v: Value = resp.json().await.context("parse anthropic models JSON")?;
    let arr = v
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("anthropic models response missing `data` array"))?;
    let mut out: Vec<String> = arr
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

async fn discover_gemini(http: &Client, api_key: &str) -> Result<Vec<String>> {
    let url = "https://generativelanguage.googleapis.com/v1beta/models";
    let resp = http
        .get(url)
        .header("x-goog-api-key", api_key)
        .send()
        .await
        .context("GET gemini /v1beta/models")?;
    if !resp.status().is_success() {
        return Err(anyhow!("gemini /v1beta/models returned HTTP {}", resp.status()));
    }
    let v: Value = resp.json().await.context("parse gemini models JSON")?;
    let arr = v
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("gemini models response missing `models` array"))?;
    let mut out: Vec<String> = arr
        .iter()
        .filter(|m| {
            m.get("supportedGenerationMethods")
                .and_then(Value::as_array)
                .map(|methods| {
                    methods
                        .iter()
                        .any(|s| s.as_str() == Some("generateContent"))
                })
                .unwrap_or(true)
        })
        .filter_map(|m| {
            m.get("name")
                .and_then(Value::as_str)
                .map(|n| n.trim_start_matches("models/").to_string())
        })
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

async fn discover_ollama(http: &Client, base: &str) -> Result<Vec<String>> {
    let url = format!("{base}/api/tags");
    let resp = http.get(&url).send().await.with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("{url} returned HTTP {}", resp.status()));
    }
    let v: Value = resp.json().await.context("parse ollama tags JSON")?;
    let arr = v
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("ollama tags response missing `models` array"))?;
    let mut out: Vec<String> = arr
        .iter()
        .filter_map(|m| m.get("name").and_then(Value::as_str).map(String::from))
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// Pick a sensible default model from a discovered list for a given
/// provider name. Returns `None` if the list is empty.
///
/// The heuristic prefers the most-capable model a typical free-tier key
/// has access to, and falls back through cheaper / older variants. Code
/// generation needs reasoning depth, so for Gemini we prefer `2.5-pro`
/// over `flash` even though flash is faster — the verify-failure cost
/// of a weaker model far outweighs the latency saving on retries.
///
/// Models with `preview`, `experimental`, `thinking`, or `tts` in the
/// name are filtered out: previews break unpredictably, audio models
/// don't speak the chat-completions shape we use, and "thinking" models
/// are pay-per-thought even on otherwise-free tiers.
pub fn pick_default_from_list(name: &str, models: &[String]) -> Option<String> {
    if models.is_empty() {
        return None;
    }
    let filtered: Vec<&String> = models
        .iter()
        .filter(|m| {
            let lc = m.to_lowercase();
            !lc.contains("preview")
                && !lc.contains("experiment")
                && !lc.contains("vision")
                && !lc.contains("audio")
                && !lc.contains("tts")
                && !lc.contains("embedding")
                && !lc.contains("aqa")
                && !lc.contains("imagen")
                && !lc.contains("veo")
                && !lc.contains("learnlm")
        })
        .collect();
    let all_refs: Vec<&String> = models.iter().collect();
    let pool: &[&String] = if filtered.is_empty() { &all_refs } else { &filtered };

    let preferences: &[&str] = match name {
        "gemini" => &[
            // Strongest free-tier code generators first.
            "gemini-2.5-pro",
            "gemini-2.0-pro",
            "gemini-2.5-flash",
            "gemini-2.0-flash",
            "gemini-flash-latest",
            "gemini-pro-latest",
            "gemini-1.5-pro",
            "gemini-1.5-flash",
            "pro",
            "flash",
        ],
        "anthropic" => &[
            "claude-sonnet-4-5",
            "claude-opus-4",
            "claude-haiku-4-5",
            "sonnet",
            "haiku",
            "opus",
        ],
        "openai" => &[
            "gpt-4.1",
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4.1-mini",
            "o4-mini",
            "mini",
        ],
        "openrouter" => &[
            "anthropic/claude-sonnet",
            "openai/gpt-4o",
            "google/gemini-2.5-pro",
            "openai/gpt-4o-mini",
            "anthropic/claude-haiku",
        ],
        "groq" => &[
            "llama-3.3-70b-versatile",
            "llama-3.1-70b",
            "mixtral",
            "llama-3.1-8b-instant",
            "llama",
        ],
        "deepseek" => &["deepseek-chat", "deepseek-coder", "deepseek"],
        "xai" | "grok" => &["grok-2", "grok-beta", "grok"],
        "together" => &[
            "meta-llama/Llama-3.3-70B-Instruct-Turbo",
            "Qwen/Qwen2.5-Coder-32B-Instruct",
            "Llama-3.3",
            "Llama-3.1-70B",
            "llama",
        ],
        "agentrouter" => &["claude-sonnet", "gpt-4o", "claude-haiku"],
        _ => &[],
    };
    for needle in preferences {
        if let Some(m) = pool.iter().find(|m| m.to_lowercase().contains(&needle.to_lowercase())) {
            return Some((*m).clone());
        }
    }
    pool.first().map(|m| (*m).clone())
}

/// Discover the model list, then ping each of the top candidates with a
/// tiny chat request and return the first that answers successfully.
///
/// This is the "smart" picker the CLI uses on startup and on Settings →
/// Detect: a discovered model in the catalogue is not the same as a
/// model your key is *quota-allowed* to call right now (free-tier keys
/// frequently lie). Probing weeds out the rate-limited / region-locked
/// entries before the orchestrator wastes a goal on them.
///
/// Probes at most `max_probe` candidates (default 3) to keep the up-
/// front cost bounded — discovery + 3 tiny pings is typically <2s.
pub async fn select_best_working_model(
    name: &str,
    api_key: &str,
    base_url: Option<&str>,
    max_probe: usize,
) -> Result<Option<String>> {
    let models = discover_models(name, api_key, base_url).await?;
    if models.is_empty() {
        return Ok(None);
    }
    let preferred = pick_default_from_list(name, &models);

    // Ranked candidate list: the heuristic pick first, then everything
    // else that survives the same filter ordering.
    let mut ordered: Vec<String> = Vec::new();
    if let Some(p) = preferred {
        ordered.push(p);
    }
    for m in &models {
        if !ordered.iter().any(|x| x == m) {
            ordered.push(m.clone());
        }
    }

    let probe_n = max_probe.max(1).min(ordered.len());
    for cand in ordered.iter().take(probe_n) {
        let cfg = match name {
            "anthropic" => ProviderConfig::Anthropic {
                api_key: api_key.into(),
                model: cand.clone(),
            },
            "openai" => ProviderConfig::OpenAI {
                api_key: api_key.into(),
                model: cand.clone(),
            },
            "openrouter" => ProviderConfig::OpenRouter {
                api_key: api_key.into(),
                model: cand.clone(),
            },
            "agentrouter" => ProviderConfig::AgentRouter {
                api_key: api_key.into(),
                model: cand.clone(),
            },
            "gemini" => ProviderConfig::Gemini {
                api_key: api_key.into(),
                model: cand.clone(),
            },
            "ollama" => ProviderConfig::Ollama {
                base_url: base_url.unwrap_or("http://localhost:11434").into(),
                model: cand.clone(),
            },
            "deepseek" | "xai" | "grok" | "groq" | "together" | "custom"
            | "openai-compatible" => {
                let url = match name {
                    "deepseek" => "https://api.deepseek.com/v1".to_string(),
                    "xai" | "grok" => "https://api.x.ai/v1".to_string(),
                    "groq" => "https://api.groq.com/openai/v1".to_string(),
                    "together" => "https://api.together.xyz/v1".to_string(),
                    _ => base_url.unwrap_or("").trim_end_matches('/').to_string(),
                };
                if url.is_empty() {
                    continue;
                }
                ProviderConfig::OpenAiCompatible {
                    name: name.into(),
                    api_key: api_key.into(),
                    model: cand.clone(),
                    base_url: url,
                }
            }
            _ => continue,
        };
        let provider = provider_for(cfg);
        // Tight, single-token-ish probe. We don't care about the content,
        // only that the call returns Ok.
        if provider.call("ping", "ok", &[]).await.is_ok() {
            return Ok(Some(cand.clone()));
        }
    }
    // Nothing answered — fall back to the heuristic pick so the user
    // sees *something* in the Model field rather than silence.
    Ok(pick_default_from_list(name, &models))
}

/// System-prompt length threshold above which the Anthropic adaptor attaches
/// a `cache_control` breakpoint. Below this the cache write overhead is not
/// worth the eventual read savings.
const ANTHROPIC_CACHE_MIN_CHARS: usize = 1024;

/// Classify an HTTP status code into a [`ProviderError`]. Used by the
/// Anthropic adaptor — other adaptors can reuse it when they grow error
/// maps of their own.
fn classify_http(status: StatusCode) -> ProviderError {
    match status.as_u16() {
        429 => ProviderError::RateLimit,
        401 | 403 => ProviderError::AuthFailed(status.as_u16()),
        s if s >= 500 => ProviderError::ServerError(s),
        s => ProviderError::ParseFail(format!("unexpected status {s}")),
    }
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// A single provider back-end.
///
/// Implementations must:
/// * Translate `system` / `user` / `slice_origins` into the provider's wire
///   format.
/// * Populate every [`LLMResponse`] field, including `cached_tokens`
///   (zero is fine for providers without prompt caching).
/// * Never panic on malformed responses — return `Err` instead.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Issue one completion call.
    ///
    /// `user` is the worker's task-specific prompt (typically a goal plus
    /// the rendered context slices). `system` is the base instructions; the
    /// adaptor wraps it with the low-confidence banner when appropriate.
    async fn call(
        &self,
        system: &str,
        user: &str,
        slice_origins: &[SliceOrigin],
    ) -> Result<LLMResponse>;

    /// Which back-end this adaptor targets.
    fn kind(&self) -> ProviderKind;
}

/// Construct the appropriate adaptor for a [`ProviderConfig`].
///
/// This is the one factory worker code should use — it keeps the concrete
/// adaptor types out of the public surface.
pub fn provider_for(config: ProviderConfig) -> Box<dyn Provider> {
    match config {
        ProviderConfig::Anthropic { api_key, model } => {
            Box::new(AnthropicProvider::new(api_key, model))
        }
        ProviderConfig::OpenAI { api_key, model } => {
            Box::new(OpenAiCompatibleProvider::openai(api_key, model))
        }
        ProviderConfig::OpenRouter { api_key, model } => {
            Box::new(OpenAiCompatibleProvider::openrouter(api_key, model))
        }
        ProviderConfig::Gemini { api_key, model } => Box::new(GeminiProvider::new(api_key, model)),
        ProviderConfig::Ollama { base_url, model } => {
            Box::new(OllamaProvider::new(base_url, model))
        }
        ProviderConfig::AgentRouter { api_key, model } => Box::new(
            OpenAiCompatibleProvider::agentrouter(api_key, model),
        ),
        ProviderConfig::OpenAiCompatible {
            name: _,
            api_key,
            model,
            base_url,
        } => Box::new(OpenAiCompatibleProvider::custom(
            api_key,
            model,
            &format!("{}/chat/completions", base_url.trim_end_matches('/')),
        )),
    }
}

/// Build the effective system prompt, prepending a low-confidence banner
/// when any of the slices fed into the user prompt came from a fallback
/// (heuristic) extraction. Public so tests and other crates can reuse it.
pub fn build_system_prompt(base: &str, slice_origins: &[SliceOrigin]) -> String {
    let has_fallback = slice_origins
        .iter()
        .any(|o| matches!(o, SliceOrigin::Fallback));
    if !has_fallback {
        return base.to_string();
    }
    let banner = "\
[LOW CONFIDENCE CONTEXT]
Some of the code slices below were extracted heuristically because the \
parser could not produce a precise symbol boundary (or the language is \
not in Phonton's semantic-parse tier). Symbol names, signatures, and \
surrounding lines may be approximate. Treat the slices as hints, not \
ground truth: prefer asking for a Read of the underlying file before \
emitting a diff that depends on exact symbol shape.
";
    format!("{banner}\n{base}")
}

// ---------------------------------------------------------------------------
// Dynamic-routing metrics
// ---------------------------------------------------------------------------

/// Stable key identifying one model under one provider.
///
/// Used as the index for [`ModelMetrics`] so multiple models served through
/// the same back-end (e.g. Haiku and Sonnet both via Anthropic) are kept
/// statistically separate.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelKey {
    /// Back-end that served the call.
    pub provider: ProviderKind,
    /// Model name as configured — e.g. `claude-haiku-4-5`, `gpt-5.2`.
    pub model: String,
}

impl ModelKey {
    /// Shorthand constructor.
    pub fn new(provider: ProviderKind, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
        }
    }
}

/// Smoothing factor for the exponentially-weighted moving averages that
/// [`ModelMetrics`] tracks. Low enough that one bad call doesn't flip a
/// model's routing verdict; high enough that steady-state drift lands
/// within a few dozen samples.
const EWMA_ALPHA: f64 = 0.2;

/// In-memory per-model latency and verify-rate registry.
///
/// `phonton-providers` records every completed LLM call (via a metered
/// wrapper) along with each downstream `phonton-verify` verdict. The
/// router reads snapshots out to decide whether the nominal tier for a
/// subtask is still the right one — true token efficiency is not "fewest
/// tokens" but "cheapest token that can still pass verification."
///
/// The registry is cheap to clone (`Arc<Mutex<_>>` inside) and safe to
/// share across tasks. Snapshots returned via [`snapshot`] are plain
/// `ModelMetricsSnapshot` values for the UI / serialisation layers.
///
/// [`snapshot`]: ModelMetrics::snapshot
#[derive(Clone, Default)]
pub struct ModelMetrics {
    inner: Arc<Mutex<HashMap<ModelKey, MetricsEntry>>>,
}

#[derive(Debug, Default)]
struct MetricsEntry {
    provider: Option<ProviderKind>,
    calls: u64,
    ms_per_token_ewma: Option<f64>,
    failed_verify_ewma: Option<f64>,
}

impl ModelMetrics {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one completed LLM call against `key`. `elapsed_ms` is the
    /// wall-clock duration of the call; `output_tokens` is what the
    /// provider reported in [`LLMResponse::output_tokens`].
    ///
    /// A zero `output_tokens` skips the latency update (division by zero
    /// would otherwise poison the EWMA).
    pub fn record_call(&self, key: &ModelKey, elapsed_ms: u64, output_tokens: u64) {
        let Ok(mut map) = self.inner.lock() else { return };
        let entry = map.entry(key.clone()).or_default();
        entry.provider = Some(key.provider);
        entry.calls = entry.calls.saturating_add(1);
        if output_tokens > 0 {
            let sample = elapsed_ms as f64 / output_tokens as f64;
            entry.ms_per_token_ewma = Some(ewma(entry.ms_per_token_ewma, sample));
        }
    }

    /// Record the outcome of running `phonton-verify` against a diff
    /// produced by `key`. `failed = true` means the verify layer rejected
    /// the diff (syntax, crate-check, workspace-check, or test failure).
    pub fn record_verification(&self, key: &ModelKey, failed: bool) {
        let Ok(mut map) = self.inner.lock() else { return };
        let entry = map.entry(key.clone()).or_default();
        entry.provider = Some(key.provider);
        let sample = if failed { 1.0 } else { 0.0 };
        entry.failed_verify_ewma = Some(ewma(entry.failed_verify_ewma, sample));
    }

    /// Snapshot of the current moving averages for `key`, or `None` if no
    /// samples have been recorded yet.
    pub fn snapshot(&self, key: &ModelKey) -> Option<ModelMetricsSnapshot> {
        let map = self.inner.lock().ok()?;
        let entry = map.get(key)?;
        Some(ModelMetricsSnapshot {
            provider: entry.provider.unwrap_or(key.provider),
            calls: entry.calls,
            ms_per_token: entry.ms_per_token_ewma,
            failed_verification_rate: entry.failed_verify_ewma,
        })
    }

    /// All recorded snapshots, one per observed model. Cheap to call — the
    /// inner map is typically small (a handful of models).
    pub fn snapshots(&self) -> Vec<(ModelKey, ModelMetricsSnapshot)> {
        let Ok(map) = self.inner.lock() else {
            return Vec::new();
        };
        map.iter()
            .map(|(k, e)| {
                (
                    k.clone(),
                    ModelMetricsSnapshot {
                        provider: e.provider.unwrap_or(k.provider),
                        calls: e.calls,
                        ms_per_token: e.ms_per_token_ewma,
                        failed_verification_rate: e.failed_verify_ewma,
                    },
                )
            })
            .collect()
    }
}

fn ewma(prev: Option<f64>, sample: f64) -> f64 {
    match prev {
        None => sample,
        Some(p) => EWMA_ALPHA * sample + (1.0 - EWMA_ALPHA) * p,
    }
}

/// Wraps any [`Provider`] to record call latency + token usage in a
/// shared [`ModelMetrics`] registry.
///
/// The wrapper is transparent to callers — the orchestrator constructs
/// one of these per configured provider at startup, and the metrics
/// registry handles routing decisions and UI observability.
pub struct MeteredProvider {
    inner: Box<dyn Provider>,
    metrics: ModelMetrics,
    key: ModelKey,
}

impl MeteredProvider {
    /// Wrap `inner`, recording every call into `metrics` under `key`.
    pub fn new(inner: Box<dyn Provider>, metrics: ModelMetrics, key: ModelKey) -> Self {
        Self { inner, metrics, key }
    }
}

#[async_trait]
impl Provider for MeteredProvider {
    async fn call(
        &self,
        system: &str,
        user: &str,
        slice_origins: &[SliceOrigin],
    ) -> Result<LLMResponse> {
        let started = Instant::now();
        let resp = self.inner.call(system, user, slice_origins).await?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        self.metrics
            .record_call(&self.key, elapsed_ms, resp.output_tokens);
        Ok(resp)
    }

    fn kind(&self) -> ProviderKind {
        self.inner.kind()
    }
}

// ---------------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------------

/// Adaptor for the Anthropic Messages API. Honours `cache_control`
/// breakpoints in caller-supplied prompts and surfaces
/// `cache_read_input_tokens` as `cached_tokens`.
pub struct AnthropicProvider {
    api_key: String,
    model: String,
    http: Client,
}

impl AnthropicProvider {
    /// Construct a new adaptor. The HTTP client is created eagerly so
    /// `call` is cheap to invoke repeatedly.
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model,
            http: Client::new(),
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn call(
        &self,
        system: &str,
        user: &str,
        slice_origins: &[SliceOrigin],
    ) -> Result<LLMResponse> {
        let system = build_system_prompt(system, slice_origins);
        // Only attach a cache breakpoint once the system prompt is long
        // enough that the write cost amortises over future reads. Short
        // prompts (ad-hoc asks, tiny tests) skip it.
        let system_block = if system.len() > ANTHROPIC_CACHE_MIN_CHARS {
            json!({
                "type": "text",
                "text": system,
                "cache_control": { "type": "ephemeral" }
            })
        } else {
            json!({ "type": "text", "text": system })
        };
        let body = json!({
            "model": self.model,
            "max_tokens": 4096,
            "system": [system_block],
            "messages": [{ "role": "user", "content": user }],
        });

        let http_resp = self
            .http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        if !http_resp.status().is_success() {
            return Err(classify_http(http_resp.status()).into());
        }

        let resp: Value = http_resp
            .json()
            .await
            .map_err(|e| ProviderError::ParseFail(e.to_string()))?;

        let content = resp
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("anthropic response missing content[0].text"))?
            .to_string();
        let usage = resp
            .get("usage")
            .ok_or_else(|| anyhow!("anthropic response missing usage"))?;

        Ok(LLMResponse {
            content,
            input_tokens: usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cached_tokens: usage
                .get("cache_read_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_creation_tokens: usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            provider: ProviderKind::Anthropic,
            model_name: self.model.clone(),
        })
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }
}

// ---------------------------------------------------------------------------
// OpenAI-compatible chat completions
// ---------------------------------------------------------------------------

/// Adaptor for APIs that expose OpenAI-style `/chat/completions`.
///
/// OpenAI and OpenRouter share this wire shape: Bearer auth, a `messages`
/// array, and `choices[0].message.content` in the response.
pub struct OpenAiCompatibleProvider {
    api_key: String,
    model: String,
    endpoint: String,
    kind: ProviderKind,
    http: Client,
}

impl OpenAiCompatibleProvider {
    /// Construct an adaptor for OpenAI's hosted API.
    pub fn openai(api_key: String, model: String) -> Self {
        Self::new(
            api_key,
            model,
            "https://api.openai.com/v1/chat/completions",
            ProviderKind::OpenAI,
        )
    }

    /// Construct an adaptor for OpenRouter's hosted router API.
    pub fn openrouter(api_key: String, model: String) -> Self {
        Self::new(
            api_key,
            model,
            "https://openrouter.ai/api/v1/chat/completions",
            ProviderKind::OpenRouter,
        )
    }

    /// Construct an adaptor for AgentRouter's hosted gateway. AgentRouter
    /// (`agentrouter.org`) speaks the OpenAI Chat Completions wire format
    /// at `/v1/chat/completions` and ships free credits for premium
    /// models — see https://agentrouter.org for sign-up.
    pub fn agentrouter(api_key: String, model: String) -> Self {
        Self::new(
            api_key,
            model,
            "https://agentrouter.org/v1/chat/completions",
            ProviderKind::AgentRouter,
        )
    }

    /// Construct an adaptor for an arbitrary OpenAI-compatible endpoint —
    /// `endpoint` is the *fully-qualified* URL of the chat-completions
    /// route (i.e. it should already end with `/chat/completions`). Use
    /// this for DeepSeek (`https://api.deepseek.com/v1/chat/completions`),
    /// xAI (`https://api.x.ai/v1/chat/completions`), Groq, Together, and
    /// self-hosted vLLM / LM Studio servers.
    pub fn custom(api_key: String, model: String, endpoint: &str) -> Self {
        Self::new(api_key, model, endpoint, ProviderKind::OpenAiCompatible)
    }

    fn new(api_key: String, model: String, endpoint: &str, kind: ProviderKind) -> Self {
        Self {
            api_key,
            model,
            endpoint: endpoint.to_string(),
            kind,
            http: Client::new(),
        }
    }
}

#[async_trait]
impl Provider for OpenAiCompatibleProvider {
    async fn call(
        &self,
        system: &str,
        user: &str,
        slice_origins: &[SliceOrigin],
    ) -> Result<LLMResponse> {
        let system = build_system_prompt(system, slice_origins);
        let body = json!({
            "model": self.model,
            "max_completion_tokens": 4096,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
        });

        let mut req = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .json(&body);

        if self.kind == ProviderKind::OpenRouter {
            req = req.header("X-OpenRouter-Title", "Phonton");
        }

        let http_resp = req
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        if !http_resp.status().is_success() {
            return Err(classify_http(http_resp.status()).into());
        }

        let resp: Value = http_resp
            .json()
            .await
            .map_err(|e| ProviderError::ParseFail(e.to_string()))?;

        let content = resp
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("{} response missing choices[0].message.content", self.kind))?
            .to_string();
        let usage = resp.get("usage").unwrap_or(&Value::Null);

        Ok(LLMResponse {
            content,
            input_tokens: usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cached_tokens: usage
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_creation_tokens: 0,
            provider: self.kind,
            model_name: self.model.clone(),
        })
    }

    fn kind(&self) -> ProviderKind {
        self.kind
    }
}

// ---------------------------------------------------------------------------
// Gemini
// ---------------------------------------------------------------------------

/// Adaptor for the Google Gemini `generateContent` endpoint.
///
/// Gemini exposes `cachedContentTokenCount` on responses that read from a
/// pre-created cached-content handle; we surface it as `cached_tokens`.
/// When no cache is in play the field is absent and we report `0`.
pub struct GeminiProvider {
    api_key: String,
    /// Wrapped in `RwLock` so that on the first 404 (model not found for
    /// this key, common on free-tier Google AI Studio accounts) we can
    /// transparently rewrite to a working model and retry — without the
    /// caller having to reconfigure anything.
    model: RwLock<String>,
    http: Client,
}

impl GeminiProvider {
    /// Construct a new adaptor.
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model: RwLock::new(model),
            http: Client::new(),
        }
    }

    fn current_model(&self) -> String {
        self.model
            .read()
            .ok()
            .map(|m| m.clone())
            .unwrap_or_default()
    }

    async fn raw_generate(&self, model: &str, body: &Value) -> Result<reqwest::Response> {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
        );
        self.http
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()).into())
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    async fn call(
        &self,
        system: &str,
        user: &str,
        slice_origins: &[SliceOrigin],
    ) -> Result<LLMResponse> {
        let system = build_system_prompt(system, slice_origins);
        let body = json!({
            "system_instruction": { "parts": [{ "text": system }] },
            "contents": [{
                "role": "user",
                "parts": [{ "text": user }],
            }],
        });

        let model = self.current_model();
        let mut http_resp = self.raw_generate(&model, &body).await?;

        // Free-tier keys frequently reject the configured model with 404.
        // Auto-discover an accessible model and retry once before failing
        // — the user explicitly asked the CLI to "route to a model
        // correctly" rather than surface a raw error.
        if http_resp.status() == StatusCode::NOT_FOUND {
            if let Ok(list) = discover_gemini(&self.http, &self.api_key).await {
                if let Some(picked) = pick_default_from_list("gemini", &list) {
                    if picked != model {
                        if let Ok(mut m) = self.model.write() {
                            *m = picked.clone();
                        }
                        http_resp = self.raw_generate(&picked, &body).await?;
                    }
                }
            }
        }

        if !http_resp.status().is_success() {
            return Err(classify_http(http_resp.status()).into());
        }

        let resp: Value = http_resp
            .json()
            .await
            .map_err(|e| ProviderError::ParseFail(e.to_string()))?;

        let content = resp
            .pointer("/candidates/0/content/parts/0/text")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("gemini response missing candidates[0].content.parts[0].text"))?
            .to_string();
        let usage = resp
            .get("usageMetadata")
            .ok_or_else(|| anyhow!("gemini response missing usageMetadata"))?;

        Ok(LLMResponse {
            content,
            input_tokens: usage
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cached_tokens: usage
                .get("cachedContentTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_creation_tokens: 0,
            provider: ProviderKind::Gemini,
            model_name: self.current_model(),
        })
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Gemini
    }
}

// ---------------------------------------------------------------------------
// Ollama
// ---------------------------------------------------------------------------

/// Adaptor for a local Ollama daemon. No prompt caching; `cached_tokens`
/// is always `0`. Uses the non-streaming `/api/chat` shape for simplicity.
pub struct OllamaProvider {
    base_url: String,
    model: String,
    http: Client,
}

impl OllamaProvider {
    /// Construct a new adaptor pointing at `base_url`
    /// (e.g. `http://localhost:11434`).
    pub fn new(base_url: String, model: String) -> Self {
        Self {
            base_url,
            model,
            http: Client::new(),
        }
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    async fn call(
        &self,
        system: &str,
        user: &str,
        slice_origins: &[SliceOrigin],
    ) -> Result<LLMResponse> {
        let system = build_system_prompt(system, slice_origins);
        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));
        let body = json!({
            "model": self.model,
            "stream": false,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user",   "content": user   },
            ],
        });

        let resp: Value = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("ollama request failed")?
            .error_for_status()
            .context("ollama returned non-2xx")?
            .json()
            .await
            .context("ollama response was not JSON")?;

        let content = resp
            .pointer("/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("ollama response missing message.content"))?
            .to_string();

        Ok(LLMResponse {
            content,
            input_tokens: resp
                .get("prompt_eval_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: resp.get("eval_count").and_then(Value::as_u64).unwrap_or(0),
            cached_tokens: 0,
            cache_creation_tokens: 0,
            provider: ProviderKind::Ollama,
            model_name: self.model.clone(),
        })
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_only_leaves_prompt_untouched() {
        let s = build_system_prompt("base", &[SliceOrigin::Semantic, SliceOrigin::Semantic]);
        assert_eq!(s, "base");
    }

    #[test]
    fn any_fallback_prepends_banner() {
        let s = build_system_prompt("base", &[SliceOrigin::Semantic, SliceOrigin::Fallback]);
        assert!(s.starts_with("[LOW CONFIDENCE CONTEXT]"));
        assert!(s.ends_with("base"));
    }

    #[test]
    fn empty_origins_no_banner() {
        let s = build_system_prompt("base", &[]);
        assert_eq!(s, "base");
    }

    #[test]
    fn classify_http_maps_expected_codes() {
        assert!(matches!(
            classify_http(StatusCode::TOO_MANY_REQUESTS),
            ProviderError::RateLimit
        ));
        assert!(matches!(
            classify_http(StatusCode::UNAUTHORIZED),
            ProviderError::AuthFailed(401)
        ));
        assert!(matches!(
            classify_http(StatusCode::FORBIDDEN),
            ProviderError::AuthFailed(403)
        ));
        assert!(matches!(
            classify_http(StatusCode::INTERNAL_SERVER_ERROR),
            ProviderError::ServerError(500)
        ));
    }

    #[test]
    fn metrics_first_sample_sets_ewma_directly() {
        let m = ModelMetrics::new();
        let k = ModelKey::new(ProviderKind::Anthropic, "claude-haiku-4-5");
        m.record_call(&k, 500, 100);
        let snap = m.snapshot(&k).unwrap();
        assert_eq!(snap.calls, 1);
        assert!((snap.ms_per_token.unwrap() - 5.0).abs() < 1e-9);
        assert!(snap.failed_verification_rate.is_none());
    }

    #[test]
    fn metrics_second_sample_blends_via_ewma() {
        let m = ModelMetrics::new();
        let k = ModelKey::new(ProviderKind::Anthropic, "claude-haiku-4-5");
        m.record_call(&k, 100, 100); // 1.0 ms/token
        m.record_call(&k, 1000, 100); // 10.0 ms/token
        let snap = m.snapshot(&k).unwrap();
        // 0.2 * 10 + 0.8 * 1 = 2.8
        assert!((snap.ms_per_token.unwrap() - 2.8).abs() < 1e-9);
        assert_eq!(snap.calls, 2);
    }

    #[test]
    fn metrics_records_verify_outcomes() {
        let m = ModelMetrics::new();
        let k = ModelKey::new(ProviderKind::OpenAI, "gpt-5.2");
        m.record_verification(&k, true);
        m.record_verification(&k, false);
        m.record_verification(&k, false);
        let snap = m.snapshot(&k).unwrap();
        // EWMA: 1 -> 0.2*0 + 0.8*1 = 0.8 -> 0.2*0 + 0.8*0.8 = 0.64
        let r = snap.failed_verification_rate.unwrap();
        assert!((0.0..=1.0).contains(&r));
        assert!(r < 1.0 && r > 0.5);
    }

    #[test]
    fn metrics_zero_output_tokens_skips_latency_update() {
        let m = ModelMetrics::new();
        let k = ModelKey::new(ProviderKind::Ollama, "llama3.2:3b");
        m.record_call(&k, 500, 0);
        let snap = m.snapshot(&k).unwrap();
        assert_eq!(snap.calls, 1);
        assert!(snap.ms_per_token.is_none());
    }

    #[test]
    fn factory_routes_openai_to_openai_provider() {
        let p = provider_for(ProviderConfig::OpenAI {
            api_key: "sk-test".into(),
            model: "gpt-5.2".into(),
        });
        assert_eq!(p.kind(), ProviderKind::OpenAI);
    }

    #[test]
    fn factory_routes_openrouter_to_openrouter_provider() {
        let p = provider_for(ProviderConfig::OpenRouter {
            api_key: "sk-or-test".into(),
            model: "openai/gpt-5.2".into(),
        });
        assert_eq!(p.kind(), ProviderKind::OpenRouter);
    }

    /// Live round-trip against the real Anthropic Messages API. Gated behind
    /// the `live-tests` feature so CI doesn't hit the network, and further
    /// gated on `ANTHROPIC_API_KEY` so a developer running the feature
    /// locally without credentials gets a clean skip instead of a panic.
    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn anthropic_live_smoke() {
        let Ok(key) = std::env::var("ANTHROPIC_API_KEY") else {
            eprintln!("skipping: ANTHROPIC_API_KEY not set");
            return;
        };
        let p = AnthropicProvider::new(key, "claude-haiku-4-5-20251001".into());
        let resp = p
            .call("You are a terse assistant.", "Reply with exactly: ok", &[])
            .await
            .expect("anthropic call failed");
        assert!(!resp.content.is_empty(), "content must not be empty");
        assert!(resp.input_tokens > 0, "input_tokens must be reported");
        assert!(resp.output_tokens > 0, "output_tokens must be reported");
    }
}
