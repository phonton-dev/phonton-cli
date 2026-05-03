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
    LLMResponse, ModelMetricsSnapshot, ModelTier, ProviderConfig, ProviderError, ProviderKind,
    SliceOrigin,
};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};

/// Maps a provider name and a requested [`ModelTier`] to a concrete model
/// identifier.
///
/// This is the heart of Phonton's multi-tier routing. For each tier, we
/// pick the best-performing model in its price class as of April 2026.
pub fn model_for_tier(provider: &str, tier: ModelTier) -> String {
    match provider {
        "anthropic" => match tier {
            ModelTier::Local | ModelTier::Cheap => "claude-haiku-4-5-20251001".into(),
            ModelTier::Standard => "claude-sonnet-4-5-20251001".into(),
            ModelTier::Frontier => "claude-opus-4-7-20260115".into(),
        },
        "openai" => match tier {
            ModelTier::Local | ModelTier::Cheap => "gpt-4o-mini".into(),
            ModelTier::Standard => "gpt-4o".into(),
            ModelTier::Frontier => "gpt-5.2-preview".into(),
        },
        "openrouter" => match tier {
            ModelTier::Local | ModelTier::Cheap => "openai/gpt-4o-mini".into(),
            ModelTier::Standard => "openai/gpt-4o".into(),
            ModelTier::Frontier => "anthropic/claude-sonnet-4.5".into(),
        },
        "gemini" => match tier {
            ModelTier::Local | ModelTier::Cheap => "gemini-2.0-flash".into(),
            ModelTier::Standard => "gemini-2.5-flash".into(),
            ModelTier::Frontier => "gemini-2.5-pro".into(),
        },
        "agentrouter" => match tier {
            ModelTier::Local | ModelTier::Cheap => "claude-haiku-4-5".into(),
            ModelTier::Standard => "claude-sonnet-4-5".into(),
            ModelTier::Frontier => "claude-sonnet-4-5".into(),
        },
        "cloudflare" => "@cf/moonshotai/kimi-k2.6".into(),
        "deepseek" => match tier {
            ModelTier::Local | ModelTier::Cheap => "deepseek-chat".into(),
            ModelTier::Standard => "deepseek-chat".into(),
            ModelTier::Frontier => "deepseek-reasoner".into(),
        },
        "xai" | "grok" => match tier {
            ModelTier::Local | ModelTier::Cheap => "grok-2-mini".into(),
            ModelTier::Standard => "grok-2".into(),
            ModelTier::Frontier => "grok-2".into(),
        },
        "groq" => match tier {
            ModelTier::Local | ModelTier::Cheap => "llama-3.3-70b-versatile".into(),
            ModelTier::Standard => "llama-3.3-70b-versatile".into(),
            ModelTier::Frontier => "llama-3.3-70b-versatile".into(),
        },
        "together" => match tier {
            ModelTier::Local | ModelTier::Cheap => "meta-llama/Llama-3.3-70B-Instruct-Turbo".into(),
            ModelTier::Standard => "meta-llama/Llama-3.3-70B-Instruct-Turbo".into(),
            ModelTier::Frontier => "meta-llama/Llama-3.3-70B-Instruct-Turbo".into(),
        },
        "ollama" => "llama3.2:3b".into(),
        _ => "unknown".into(),
    }
}

/// Discover the list of models a given API key has access to. Returns
/// model identifiers in the form the corresponding [`Provider`] expects
/// in [`ProviderConfig`].
///
/// Providers that don't expose a models endpoint (AgentRouter) return a
/// curated static list. `base_url` is honoured for `custom` /
/// `openai-compatible` endpoints.
pub async fn discover_models(
    name: &str,
    api_key: &str,
    base_url: Option<&str>,
) -> Result<Vec<String>> {
    let http = Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    match name {
        "anthropic" => discover_anthropic(&http, api_key).await,
        "openai" => {
            discover_openai_bearer(&http, api_key, "https://api.openai.com/v1/models").await
        }
        "openrouter" => {
            // OpenRouter exposes a public catalogue; auth is optional but
            // including the key scopes the list to what the account can call.
            discover_openrouter(&http, api_key).await
        }
        "agentrouter" => {
            // AgentRouter doesn't expose a /v1/models endpoint.
            // Validate the key with a tiny probe then return the static
            // list of models it routes to.
            discover_agentrouter(&http, api_key).await
        }
        "cloudflare" => {
            // Workers AI's OpenAI-compatible endpoint is the contract we use.
            // Keep discovery deterministic; doctor's completion probe proves
            // account/model access without depending on a catalogue endpoint.
            Ok(cloudflare_static_models())
        }
        "deepseek" => {
            discover_openai_bearer(&http, api_key, "https://api.deepseek.com/v1/models").await
        }
        "xai" | "grok" => {
            discover_openai_bearer(&http, api_key, "https://api.x.ai/v1/models").await
        }
        "groq" => {
            discover_openai_bearer(&http, api_key, "https://api.groq.com/openai/v1/models").await
        }
        "together" => discover_together(&http, api_key).await,
        "gemini" => discover_gemini(&http, api_key).await,
        "ollama" => {
            let base = base_url
                .unwrap_or("http://localhost:11434")
                .trim_end_matches('/');
            discover_ollama(&http, base).await
        }
        "custom" | "openai-compatible" => {
            let base = base_url
                .ok_or_else(|| anyhow!("custom provider requires a Base URL"))?
                .trim_end_matches('/');
            let url = format!("{base}/models");
            discover_openai_bearer(&http, api_key, &url).await
        }
        _ => Err(anyhow!("unknown provider `{name}`")),
    }
}

/// Classify a non-2xx status into a user-readable sentence.
fn http_err_msg(provider: &str, status: StatusCode) -> anyhow::Error {
    match status.as_u16() {
        401 | 403 => anyhow!("invalid or expired API key for {provider} (HTTP {status})"),
        404 => anyhow!("{provider}: models endpoint not found (HTTP 404)"),
        429 => anyhow!("{provider}: rate-limited — wait a moment then retry"),
        s if s >= 500 => anyhow!("{provider} server error (HTTP {s}) — try again"),
        s => anyhow!("{provider} returned HTTP {s}"),
    }
}

async fn discover_openai_bearer(http: &Client, api_key: &str, url: &str) -> Result<Vec<String>> {
    let resp = http
        .get(url)
        .bearer_auth(api_key)
        .send()
        .await
        .with_context(|| format!("connecting to {url}"))?;
    if !resp.status().is_success() {
        // Extract the provider name from the URL for a nicer message.
        let provider = url
            .trim_start_matches("https://")
            .split('/')
            .next()
            .unwrap_or(url);
        return Err(http_err_msg(provider, resp.status()));
    }
    parse_openai_models(resp.json().await.context("parsing models response")?)
}

fn parse_openai_models(v: Value) -> Result<Vec<String>> {
    let arr = v
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("models response missing `data` array — unexpected API shape"))?;
    let mut out: Vec<String> = arr
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

async fn discover_anthropic(http: &Client, api_key: &str) -> Result<Vec<String>> {
    let resp = http
        .get("https://api.anthropic.com/v1/models")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .context("connecting to Anthropic")?;
    if !resp.status().is_success() {
        return Err(http_err_msg("Anthropic", resp.status()));
    }
    parse_openai_models(resp.json().await.context("parsing Anthropic models")?)
}

async fn discover_openrouter(http: &Client, api_key: &str) -> Result<Vec<String>> {
    let resp = http
        .get("https://openrouter.ai/api/v1/models")
        .bearer_auth(api_key)
        .send()
        .await
        .context("connecting to OpenRouter")?;
    if !resp.status().is_success() {
        return Err(http_err_msg("OpenRouter", resp.status()));
    }
    let v: Value = resp.json().await.context("parsing OpenRouter models")?;
    // OpenRouter's catalogue uses the same `data[].id` shape as OpenAI.
    let arr = v
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("OpenRouter models response has unexpected shape"))?;
    let mut out: Vec<String> = arr
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(String::from))
        // Filter to models the account can actually call (pricing.prompt
        // is "0" for free-tier models on OpenRouter).
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// AgentRouter is a gateway — it doesn't expose its own models catalogue.
/// We validate the key with one tiny probe, then return the static list of
/// models it routes to. Last verified 2025-04; update as the service adds
/// new backends.
async fn discover_agentrouter(http: &Client, api_key: &str) -> Result<Vec<String>> {
    // Probe: one minimal chat call to confirm the key is accepted.
    let probe = http
        .post("https://agentrouter.org/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&json!({
            "model": "claude-haiku-4-5",
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send()
        .await
        .context("connecting to AgentRouter")?;

    let status = probe.status();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(http_err_msg("AgentRouter", status));
    }
    // Any response other than 401/403 means the key is accepted
    // (rate-limit, server error, or success all mean the key is valid).

    Ok(agentrouter_static_models())
}

/// Known models available through AgentRouter. Because the gateway doesn't
/// publish a catalogue endpoint this list is maintained by hand.
fn agentrouter_static_models() -> Vec<String> {
    vec![
        // Anthropic
        "claude-opus-4-7".into(),
        "claude-sonnet-4-6".into(),
        "claude-sonnet-4-5".into(),
        "claude-haiku-4-5".into(),
        // OpenAI
        "gpt-4.1".into(),
        "gpt-4.1-mini".into(),
        "gpt-4o".into(),
        "gpt-4o-mini".into(),
        "o4-mini".into(),
        // Google
        "gemini-2.5-pro".into(),
        "gemini-2.5-flash".into(),
        "gemini-2.0-flash".into(),
        // Meta / open-source routed
        "llama-3.3-70b".into(),
        "llama-3.1-8b".into(),
    ]
}

fn cloudflare_static_models() -> Vec<String> {
    vec![
        "@cf/moonshotai/kimi-k2.6".into(),
        "@cf/openai/gpt-oss-120b".into(),
        "@cf/meta/llama-3.1-8b-instruct".into(),
    ]
}

fn cloudflare_workers_ai_base_url(base_url_or_account: Option<&str>) -> Option<String> {
    let raw = base_url_or_account
        .map(str::to_string)
        .or_else(|| std::env::var("CLOUDFLARE_ACCOUNT_ID").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    if raw.starts_with("http://") || raw.starts_with("https://") {
        Some(raw)
    } else {
        Some(format!(
            "https://api.cloudflare.com/client/v4/accounts/{raw}/ai/v1"
        ))
    }
}

async fn discover_together(http: &Client, api_key: &str) -> Result<Vec<String>> {
    let resp = http
        .get("https://api.together.xyz/v1/models")
        .bearer_auth(api_key)
        .send()
        .await
        .context("connecting to Together AI")?;
    if !resp.status().is_success() {
        return Err(http_err_msg("Together AI", resp.status()));
    }
    let v: Value = resp.json().await.context("parsing Together models")?;
    // Together returns either `{data: [...]}` or a bare array depending on
    // API version; handle both shapes.
    let items: Vec<Value> = if let Some(arr) = v.get("data").and_then(Value::as_array) {
        arr.clone()
    } else if let Some(arr) = v.as_array() {
        arr.clone()
    } else {
        return Err(anyhow!("Together models response has unexpected shape"));
    };
    let mut out: Vec<String> = items
        .iter()
        .filter_map(|m| {
            // Together uses `id` like OpenAI, but some responses use
            // `name` instead.
            m.get("id")
                .or_else(|| m.get("name"))
                .and_then(Value::as_str)
                .map(String::from)
        })
        // Only chat / language models — skip embedding and image models.
        .filter(|id| {
            let lc = id.to_lowercase();
            !lc.contains("embed") && !lc.contains("stable-") && !lc.contains("dall-")
        })
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

async fn discover_gemini(http: &Client, api_key: &str) -> Result<Vec<String>> {
    let resp = http
        .get("https://generativelanguage.googleapis.com/v1beta/models")
        .header("x-goog-api-key", api_key)
        .send()
        .await
        .context("connecting to Gemini")?;
    if !resp.status().is_success() {
        return Err(http_err_msg("Gemini", resp.status()));
    }
    let v: Value = resp.json().await.context("parsing Gemini models")?;
    let arr = v
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Gemini models response has unexpected shape"))?;
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
    let resp = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("connecting to Ollama at {base}"))?;
    if !resp.status().is_success() {
        return Err(http_err_msg("Ollama", resp.status()));
    }
    let v: Value = resp.json().await.context("parsing Ollama model list")?;
    let arr = v
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Ollama /api/tags response has unexpected shape"))?;
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
    let pool: &[&String] = if filtered.is_empty() {
        &all_refs
    } else {
        &filtered
    };

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
        "cloudflare" => &["@cf/moonshotai/kimi-k2.6", "kimi-k2.6", "gpt-oss", "llama"],
        _ => &[],
    };
    for needle in preferences {
        if let Some(m) = pool
            .iter()
            .find(|m| m.to_lowercase().contains(&needle.to_lowercase()))
        {
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
            "cloudflare" => {
                let Some(url) = cloudflare_workers_ai_base_url(base_url) else {
                    continue;
                };
                ProviderConfig::OpenAiCompatible {
                    name: "cloudflare".into(),
                    api_key: api_key.into(),
                    model: cand.clone(),
                    base_url: url,
                }
            }
            "deepseek" | "xai" | "grok" | "groq" | "together" | "custom" | "openai-compatible" => {
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

/// Classify an HTTP status code into a [`ProviderError`]. Kept for tests
/// and as the structured shape callers may want when they care about
/// kind-level differentiation; the live adaptors prefer
/// [`annotate_http_error`] which preserves the upstream body.
#[allow(dead_code)]
fn classify_http(status: StatusCode) -> ProviderError {
    match status.as_u16() {
        429 => ProviderError::RateLimit,
        401 | 403 => ProviderError::AuthFailed(status.as_u16()),
        s if s >= 500 => ProviderError::ServerError(s),
        s => ProviderError::ParseFail(format!("unexpected status {s}")),
    }
}

/// Build an `anyhow::Error` that combines the classified status with the
/// upstream response body. The body often carries the actionable detail
/// ("Invalid API key", "model not found", "insufficient quota") that a
/// bare status code hides.
///
/// `body` is truncated to a reasonable preview so a 50KB HTML 503 page
/// doesn't overflow the TUI.
fn annotate_http_error(provider: &str, status: StatusCode, body: &str) -> anyhow::Error {
    // Try to pull `error.message` out of standard JSON error envelopes
    // (OpenAI, Anthropic, Gemini, DeepSeek, xAI, Together, Groq all use
    // `{"error": {"message": "..."}}`); fall back to the raw body.
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.pointer("/message"))
                .or_else(|| v.pointer("/error"))
                .and_then(|x| x.as_str().map(String::from))
                .or_else(|| Some(v.to_string()))
        })
        .unwrap_or_else(|| body.to_string());
    let preview: String = detail.chars().take(400).collect();
    let hint = match status.as_u16() {
        401 => " (check the API key — wrong, expired, or revoked)",
        403 => " (key rejected — wrong provider or insufficient permissions)",
        404 => " (endpoint or model not found — check provider/model name and base URL)",
        429 => " (rate-limited — wait and retry, or check quota)",
        s if s >= 500 => " (provider server error — retry shortly)",
        _ => "",
    };
    anyhow!("{provider} HTTP {}{hint}: {preview}", status.as_u16())
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

    /// The name of the model this provider is configured to call.
    fn model(&self) -> String;

    /// Return a boxed clone of this provider.
    fn clone_box(&self) -> Box<dyn Provider>;
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
        ProviderConfig::AgentRouter { api_key, model } => {
            Box::new(OpenAiCompatibleProvider::agentrouter(api_key, model))
        }
        ProviderConfig::OpenAiCompatible {
            name,
            api_key,
            model,
            base_url,
        } => {
            let endpoint = format!("{}/chat/completions", base_url.trim_end_matches('/'));
            if name == "cloudflare" {
                Box::new(OpenAiCompatibleProvider::new(
                    api_key,
                    model,
                    &endpoint,
                    ProviderKind::Cloudflare,
                ))
            } else {
                Box::new(OpenAiCompatibleProvider::custom(api_key, model, &endpoint))
            }
        }
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
        let Ok(mut map) = self.inner.lock() else {
            return;
        };
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
        let Ok(mut map) = self.inner.lock() else {
            return;
        };
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
        Self {
            inner,
            metrics,
            key,
        }
    }
}

impl Clone for MeteredProvider {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone_box(),
            metrics: self.metrics.clone(),
            key: self.key.clone(),
        }
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

    fn model(&self) -> String {
        self.inner.model()
    }

    fn clone_box(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

// ---------------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------------

/// Adaptor for the Anthropic Messages API. Honours `cache_control`
/// breakpoints in caller-supplied prompts and surfaces
/// `cache_read_input_tokens` as `cached_tokens`.
#[derive(Clone)]
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
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .unwrap_or_default(),
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
        // 8192 is the highest `max_tokens` value every current Claude model
        // accepts (haiku 3.5 caps here; sonnet 4.x/opus go higher). Going
        // above this would 400 on haiku, going below this throws away
        // headroom on the bigger models for normal-length completions.
        let body = json!({
            "model": self.model,
            "max_tokens": 8192,
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

        let status = http_resp.status();
        if !status.is_success() {
            let body = http_resp.text().await.unwrap_or_default();
            return Err(annotate_http_error("Anthropic", status, &body));
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

    fn model(&self) -> String {
        self.model.clone()
    }

    fn clone_box(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

// ---------------------------------------------------------------------------
// OpenAI-compatible chat completions
// ---------------------------------------------------------------------------

/// Adaptor for APIs that expose OpenAI-style `/chat/completions`.
///
/// OpenAI and OpenRouter share this wire shape: Bearer auth, a `messages`
/// array, and `choices[0].message.content` in the response.
#[derive(Clone)]
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
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .unwrap_or_default(),
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
        // OpenAI's *current* spec uses `max_completion_tokens` and o1/o3/o4
        // *reject* `max_tokens` outright. Every other OpenAI-compat provider
        // (DeepSeek, xAI, Together, Groq's deprecated path, OpenRouter,
        // AgentRouter, vLLM, LM Studio, …) was built against the original
        // spec and either ignores `max_completion_tokens` (silently truncating
        // to a tiny default) or rejects it as an unknown field. Picking the
        // right key per back-end is what makes "BYOK" actually work.
        let token_key = if self.kind == ProviderKind::OpenAI {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        let body = json!({
            "model": self.model,
            token_key: 4096,
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
            // OpenRouter's recommended attribution headers — these don't
            // affect routing but make rate-limit dashboards readable.
            req = req
                .header("HTTP-Referer", "https://github.com/phonton/phonton")
                .header("X-Title", "Phonton");
        }

        let http_resp = req
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        let status = http_resp.status();
        if !status.is_success() {
            // Surface the upstream error body so users see *why* (e.g.
            // "Invalid API key", "model not found", "insufficient quota")
            // instead of a bare HTTP code.
            let body = http_resp.text().await.unwrap_or_default();
            return Err(annotate_http_error(
                self.kind.to_string().as_str(),
                status,
                &body,
            ));
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

    fn model(&self) -> String {
        self.model.clone()
    }

    fn clone_box(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
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
#[derive(Clone)]
pub struct GeminiProvider {
    api_key: String,
    /// Wrapped in `Arc<RwLock>` so that on the first 404 (model not found for
    /// this key, common on free-tier Google AI Studio accounts) we can
    /// transparently rewrite to a working model and retry — without the
    /// caller having to reconfigure anything.
    model: Arc<RwLock<String>>,
    http: Client,
}

impl GeminiProvider {
    /// Construct a new adaptor.
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model: Arc::new(RwLock::new(model)),
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .unwrap_or_default(),
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

    async fn raw_generate_with_retry(
        &self,
        model: &str,
        body: &Value,
    ) -> Result<reqwest::Response> {
        let mut delay = std::time::Duration::from_millis(400);
        for attempt in 0..3 {
            let resp = self.raw_generate(model, body).await?;
            if !is_transient_http_status(resp.status()) || attempt == 2 {
                return Ok(resp);
            }
            tokio::time::sleep(delay).await;
            delay = delay.saturating_mul(2);
        }
        unreachable!("retry loop always returns")
    }
}

fn is_transient_http_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

#[async_trait]
impl Provider for GeminiProvider {
    async fn call(
        &self,
        system: &str,
        user: &str,
        slice_origins: &[SliceOrigin],
    ) -> Result<LLMResponse> {
        let system_full = build_system_prompt(system, slice_origins);
        let model = self.current_model();
        let is_gemma = model.contains("gemma");
        let is_json =
            system.to_lowercase().contains("json") || user.to_lowercase().contains("json");

        let mut contents = Vec::new();

        if is_gemma {
            // Gemma (and other small models) often ignore 'system_instruction'
            // when the task is broad. We force obedience via few-shotting
            // and by repeating the system prompt in the user's turn.
            if is_json {
                contents.push(json!({
                    "role": "user",
                    "parts": [{ "text": format!("SYSTEM: You are a software task decomposer. Respond ONLY with a JSON array.\n\nUSER: Break 'add logging' into subtasks.") }]
                }));
                contents.push(json!({
                    "role": "model",
                    "parts": [{ "text": "[{\"description\": \"Add logging crate\", \"model_tier\": \"Standard\", \"depends_on\": []}]" }]
                }));
            } else {
                contents.push(json!({
                    "role": "user",
                    "parts": [{ "text": format!("SYSTEM: {}\n\nUSER: add a comment to lib.rs", system_full) }]
                }));
                contents.push(json!({
                    "role": "model",
                    "parts": [{ "text": "--- a/lib.rs\n+++ b/lib.rs\n@@ -1,1 +1,2 @@\n+// Phonton\n pub fn init() {}" }]
                }));
            }

            let final_user_text = if is_json {
                format!("USER: {}", user)
            } else {
                // Extreme "jail" for chatty models in worker mode.
                // We repeat the critical rules at the end of the user turn
                // because models like Gemma weight the end of the prompt heavily.
                format!(
                    "USER TASK: {}\n\n\
                     STRICT RULES:\n\
                     - NO PROSE\n\
                     - NO PREAMBLE\n\
                     - NO CODE FENCES (```)\n\
                     - START IMMEDIATELY WITH `--- a/` OR `--- /dev/null`\n\n\
                     START DIFF:",
                    user
                )
            };

            contents.push(json!({
                "role": "user",
                "parts": [{ "text": final_user_text }]
            }));
        } else {
            contents.push(json!({
                "role": "user",
                "parts": [{ "text": user }],
            }));
        }

        let body = json!({
            "system_instruction": { "parts": [{ "text": system_full }] },
            "contents": contents,
            "generationConfig": {
                "temperature": 0.1,
                "topP": 0.95,
                "responseMimeType": if is_json { "application/json" } else { "text/plain" },
            }
        });

        let mut http_resp = self.raw_generate_with_retry(&model, &body).await?;

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
                        http_resp = self.raw_generate_with_retry(&picked, &body).await?;
                    }
                }
            }
        }

        let status = http_resp.status();
        if !status.is_success() {
            let body = http_resp.text().await.unwrap_or_default();
            return Err(annotate_http_error("Gemini", status, &body));
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

    fn model(&self) -> String {
        self.current_model()
    }

    fn clone_box(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

// ---------------------------------------------------------------------------
// Ollama
// ---------------------------------------------------------------------------

/// Adaptor for a local Ollama daemon. No prompt caching; `cached_tokens`
/// is always `0`. Uses the non-streaming `/api/chat` shape for simplicity.
#[derive(Clone)]
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
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .unwrap_or_default(),
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

    fn model(&self) -> String {
        self.model.clone()
    }

    fn clone_box(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
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
    fn transient_http_statuses_are_retryable() {
        assert!(is_transient_http_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_transient_http_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_transient_http_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!is_transient_http_status(StatusCode::UNAUTHORIZED));
        assert!(!is_transient_http_status(StatusCode::NOT_FOUND));
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

    #[test]
    fn tier_defaults_cover_public_provider_names() {
        for provider in [
            "anthropic",
            "openai",
            "openrouter",
            "gemini",
            "agentrouter",
            "cloudflare",
            "deepseek",
            "xai",
            "grok",
            "groq",
            "together",
            "ollama",
        ] {
            for tier in [ModelTier::Cheap, ModelTier::Standard, ModelTier::Frontier] {
                assert_ne!(model_for_tier(provider, tier), "unknown", "{provider:?}");
            }
        }
    }

    /// Regression: OpenAI's chat-completions spec uses `max_completion_tokens`
    /// (and o1/o3/o4 *reject* `max_tokens`); every other OpenAI-compat
    /// back-end (DeepSeek, xAI, Together, Groq, OpenRouter, AgentRouter,
    /// vLLM, LM Studio) was built against the original spec and either
    /// ignores `max_completion_tokens` (silently truncating to a tiny
    /// default) or rejects it as unknown. This test pins the per-kind
    /// branching so a refactor can't quietly regress every non-OpenAI
    /// provider.
    #[tokio::test]
    async fn openai_compat_uses_max_tokens_for_non_openai() {
        use std::net::SocketAddr;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        async fn body_capture_server(
            saw_max_tokens: Arc<AtomicBool>,
            saw_max_completion: Arc<AtomicBool>,
        ) -> SocketAddr {
            // Tiny TCP listener that reads one HTTP request, records which
            // token field the caller sent, and answers a valid chat shape.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                if let Ok((mut sock, _)) = listener.accept().await {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                    if raw.contains("\"max_tokens\"") {
                        saw_max_tokens.store(true, Ordering::SeqCst);
                    }
                    if raw.contains("\"max_completion_tokens\"") {
                        saw_max_completion.store(true, Ordering::SeqCst);
                    }
                    let body = r#"{"choices":[{"message":{"content":"ok"}}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(), body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
            });
            addr
        }

        // Non-OpenAI compat (e.g. DeepSeek / xAI / Together) → must send `max_tokens`.
        let saw_mt = Arc::new(AtomicBool::new(false));
        let saw_mc = Arc::new(AtomicBool::new(false));
        let addr = body_capture_server(saw_mt.clone(), saw_mc.clone()).await;
        let endpoint = format!("http://{}/chat/completions", addr);
        let p = OpenAiCompatibleProvider::custom("k".into(), "m".into(), &endpoint);
        let _ = p.call("sys", "u", &[]).await;
        assert!(
            saw_mt.load(Ordering::SeqCst),
            "non-OpenAI compat must send max_tokens"
        );
        assert!(
            !saw_mc.load(Ordering::SeqCst),
            "non-OpenAI compat must NOT send max_completion_tokens"
        );

        // OpenAI proper → must send `max_completion_tokens`.
        let saw_mt = Arc::new(AtomicBool::new(false));
        let saw_mc = Arc::new(AtomicBool::new(false));
        let addr = body_capture_server(saw_mt.clone(), saw_mc.clone()).await;
        let endpoint = format!("http://{}/chat/completions", addr);
        let p =
            OpenAiCompatibleProvider::new("k".into(), "m".into(), &endpoint, ProviderKind::OpenAI);
        let _ = p.call("sys", "u", &[]).await;
        assert!(
            saw_mc.load(Ordering::SeqCst),
            "OpenAI proper must send max_completion_tokens"
        );
        assert!(
            !saw_mt.load(Ordering::SeqCst),
            "OpenAI proper must NOT send max_tokens (rejected by o-series)"
        );
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
