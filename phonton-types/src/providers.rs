//! Model provider abstractions.
//!
//! `phonton-providers` is the concrete implementation; this module only
//! defines the types that cross crate boundaries — configuration shape,
//! the set of supported back-ends, and the shape of a normalised LLM
//! response.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ModelTier;

// ---------------------------------------------------------------------------
// Provider identity
// ---------------------------------------------------------------------------

/// The set of back-ends Phonton knows how to call.
///
/// Used as a tag inside [`LLMResponse`] so downstream accounting can
/// separate spend by provider without peeking into [`ProviderConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProviderKind {
    /// Anthropic (`claude-*` family).
    Anthropic,
    /// OpenAI (`gpt-*`, `o*` families).
    OpenAI,
    /// OpenRouter (`provider/model` catalog).
    OpenRouter,
    /// Google (`gemini-*` family).
    Gemini,
    /// Local Ollama server — zero API cost.
    Ollama,
    /// AgentRouter unified gateway (`agentrouter.org`).
    AgentRouter,
    /// Cloudflare Workers AI OpenAI-compatible endpoint.
    Cloudflare,
    /// Generic OpenAI-compatible endpoint (DeepSeek, xAI, Groq, vLLM, …).
    OpenAiCompatible,
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::OpenAI => "openai",
            ProviderKind::OpenRouter => "openrouter",
            ProviderKind::Gemini => "gemini",
            ProviderKind::Ollama => "ollama",
            ProviderKind::AgentRouter => "agentrouter",
            ProviderKind::Cloudflare => "cloudflare",
            ProviderKind::OpenAiCompatible => "openai-compatible",
        };
        f.write_str(s)
    }
}

// ---------------------------------------------------------------------------
// Provider configuration
// ---------------------------------------------------------------------------

/// User-supplied configuration for a single provider, loaded from env vars
/// or `~/.phonton/config.toml`.
///
/// One `ProviderConfig` per active provider; phonton-providers routes a
/// [`crate::ModelTier`] to a concrete config at call time.
///
/// API keys are never serialized — use environment variables or a secrets
/// manager for persistence. Round-tripping a `ProviderConfig` through any
/// serde format will drop the `api_key` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum ProviderConfig {
    /// Anthropic API (hosted).
    Anthropic {
        /// API key — treat as a secret; never log. Skipped on serialize.
        #[serde(skip_serializing)]
        api_key: String,
        /// Model name, e.g. `claude-sonnet-4-6`.
        model: String,
    },
    /// OpenAI API (hosted).
    OpenAI {
        /// API key — treat as a secret; never log. Skipped on serialize.
        #[serde(skip_serializing)]
        api_key: String,
        /// Model name, e.g. `gpt-4o-mini`.
        model: String,
    },
    /// OpenRouter API (hosted multi-provider router).
    OpenRouter {
        /// API key — treat as a secret; never log. Skipped on serialize.
        #[serde(skip_serializing)]
        api_key: String,
        /// Model name, e.g. `openai/gpt-5.2`.
        model: String,
    },
    /// Google Gemini API (hosted).
    Gemini {
        /// API key — treat as a secret; never log. Skipped on serialize.
        #[serde(skip_serializing)]
        api_key: String,
        /// Model name, e.g. `gemini-2.5-pro`.
        model: String,
    },
    /// Local Ollama daemon.
    Ollama {
        /// Base URL of the Ollama server, typically `http://localhost:11434`.
        base_url: String,
        /// Model name as known to Ollama, e.g. `llama3.2:3b`.
        model: String,
    },
    /// AgentRouter — OpenAI-compatible router exposing Claude / GPT / Gemini
    /// at https://agentrouter.org/v1. Treated as its own variant (rather
    /// than forcing users into [`OpenRouter`]) because the model catalogue,
    /// pricing, and free-tier credits are different.
    AgentRouter {
        /// API key — treat as a secret; never log. Skipped on serialize.
        #[serde(skip_serializing)]
        api_key: String,
        /// Model name as exposed by AgentRouter, e.g. `claude-sonnet-4-5`.
        model: String,
    },
    /// Generic OpenAI-compatible endpoint. Use this for self-hosted gateways
    /// (vLLM, LM Studio, Together, DeepSeek, xAI, Groq, …) and any other
    /// service that speaks the `/v1/chat/completions` shape with Bearer auth.
    OpenAiCompatible {
        /// Display name used in the UI and in metrics keys. e.g. `"deepseek"`.
        name: String,
        /// API key — treat as a secret; never log.
        #[serde(skip_serializing)]
        api_key: String,
        /// Model identifier passed to the endpoint verbatim.
        model: String,
        /// Full base URL up to and including `/v1` — e.g.
        /// `https://api.deepseek.com/v1` or `https://api.x.ai/v1`.
        base_url: String,
    },
}

impl ProviderConfig {
    /// The [`ProviderKind`] this config targets.
    pub fn kind(&self) -> ProviderKind {
        match self {
            ProviderConfig::Anthropic { .. } => ProviderKind::Anthropic,
            ProviderConfig::OpenAI { .. } => ProviderKind::OpenAI,
            ProviderConfig::OpenRouter { .. } => ProviderKind::OpenRouter,
            ProviderConfig::Gemini { .. } => ProviderKind::Gemini,
            ProviderConfig::Ollama { .. } => ProviderKind::Ollama,
            ProviderConfig::AgentRouter { .. } => ProviderKind::AgentRouter,
            ProviderConfig::OpenAiCompatible { name, .. } if name == "cloudflare" => {
                ProviderKind::Cloudflare
            }
            ProviderConfig::OpenAiCompatible { .. } => ProviderKind::OpenAiCompatible,
        }
    }
}

// ---------------------------------------------------------------------------
// Normalised response
// ---------------------------------------------------------------------------

/// Provider-agnostic result of a single LLM call.
///
/// Every provider adaptor in `phonton-providers` returns this shape, so
/// worker code never sees raw Anthropic / OpenAI / Gemini payloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMResponse {
    /// Model-generated text. Workers expect this to contain only a unified
    /// diff; validation happens in `phonton-worker`, not here.
    pub content: String,
    /// Input tokens billed for this call.
    pub input_tokens: u64,
    /// Output tokens billed for this call.
    pub output_tokens: u64,
    /// Cached input tokens (Anthropic `cache_read_input_tokens` or equivalent).
    /// Always `0` for providers without prompt caching.
    pub cached_tokens: u64,
    /// Tokens written *into* the cache on this call — Anthropic's
    /// `cache_creation_input_tokens`. These are billed at a premium but pay
    /// back on subsequent hits; callers surface them separately so cost
    /// accounting can distinguish cold-start writes from steady-state reads.
    /// Always `0` for providers without prompt caching.
    pub cache_creation_tokens: u64,
    /// Which back-end served this response.
    pub provider: ProviderKind,
    /// Model name as reported by the back-end (e.g. `claude-haiku-4-5-20251001`).
    /// Used by `BudgetGuard` to price the call; empty string means unknown.
    pub model_name: String,
}

/// Token usage for one worker result, preserving provider-reported buckets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Non-cached input tokens billed for the request.
    pub input_tokens: u64,
    /// Output tokens generated by the model.
    pub output_tokens: u64,
    /// Input tokens read from a provider-side prompt cache.
    pub cached_tokens: u64,
    /// Input tokens written into a provider-side prompt cache.
    pub cache_creation_tokens: u64,
    /// True when this was reconstructed from older aggregate-only data.
    pub estimated: bool,
}

impl TokenUsage {
    /// Reconstruct usage from a legacy aggregate count.
    pub fn estimated(total_tokens: u64) -> Self {
        Self {
            input_tokens: total_tokens,
            estimated: true,
            ..Self::default()
        }
    }

    /// Add one provider response into this aggregate.
    pub fn add_response(&mut self, response: &LLMResponse) {
        self.input_tokens = self.input_tokens.saturating_add(response.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(response.output_tokens);
        self.cached_tokens = self.cached_tokens.saturating_add(response.cached_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(response.cache_creation_tokens);
        self.estimated = false;
    }

    /// Tokens that count toward the run's token budget.
    pub fn budget_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_creation_tokens)
    }
}

/// User-facing cost estimate for a provider/model usage bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostSummary {
    /// False when the pricing table does not contain the provider/model.
    pub pricing_known: bool,
    /// Input-side estimated cost in micro-dollars.
    pub input_usd_micros: u64,
    /// Output-side estimated cost in micro-dollars.
    pub output_usd_micros: u64,
    /// Total estimated cost in micro-dollars.
    pub total_usd_micros: u64,
}

// ---------------------------------------------------------------------------
// Dynamic routing: metrics and budget
// ---------------------------------------------------------------------------

/// Per-model observability snapshot maintained by `phonton-providers`.
///
/// The dynamic router reads this to make cost/latency-aware tier choices:
/// a nominally "Cheap" model that verifies at 20% is worse than a Standard
/// model that verifies at 90%, and the ms-per-token figure lets the UI
/// surface honest progress estimates. See
/// `phonton_providers::ModelMetrics` for the live registry.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelMetricsSnapshot {
    /// Which back-end served the sampled calls.
    pub provider: ProviderKind,
    /// Total completed calls contributing to this snapshot.
    pub calls: u64,
    /// Exponentially-weighted moving average of wall-clock ms per output
    /// token. `None` when no timed samples have landed yet.
    pub ms_per_token: Option<f64>,
    /// Exponentially-weighted moving average of the fraction of verified
    /// attempts that failed `phonton-verify`. `None` until at least one
    /// verification outcome has been recorded. Range: `[0.0, 1.0]`.
    pub failed_verification_rate: Option<f64>,
}

/// USD-per-million-token pricing for a single model.
///
/// Stored as integer micro-dollars per million tokens so the orchestrator
/// can arithmetic without floating-point drift. `1_000_000` here means
/// `$1.00 per million tokens` — i.e. `1 micro-dollar per token`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPricing {
    /// Price of one million input tokens in micro-dollars.
    pub input_usd_micros_per_mtok: u64,
    /// Price of one million output tokens in micro-dollars.
    pub output_usd_micros_per_mtok: u64,
}

impl ModelPricing {
    /// Cost in micro-dollars for `input` + `output` tokens at this price.
    pub fn cost_micros(&self, input: u64, output: u64) -> u64 {
        let i = (input as u128 * self.input_usd_micros_per_mtok as u128) / 1_000_000;
        let o = (output as u128 * self.output_usd_micros_per_mtok as u128) / 1_000_000;
        (i + o) as u64
    }
}

/// Published Opus-class list price used only for frontier counterfactuals.
///
/// `$15 / MTok` input and `$75 / MTok` output. This is a labeled estimate, not
/// a live quote from a provider dashboard.
pub const FRONTIER_REFERENCE_PRICING: ModelPricing = ModelPricing {
    input_usd_micros_per_mtok: 15_000_000,
    output_usd_micros_per_mtok: 75_000_000,
};

/// Published list-price stand-ins used when a model is not in `BudgetGuard`.
///
/// Cheap ≈ DeepSeek Flash off-peak. Standard ≈ Sonnet-class. Frontier uses
/// [`FRONTIER_REFERENCE_PRICING`]. Local is zero.
pub fn reference_pricing_for_tier(tier: ModelTier) -> ModelPricing {
    match tier {
        ModelTier::Local => ModelPricing {
            input_usd_micros_per_mtok: 0,
            output_usd_micros_per_mtok: 0,
        },
        ModelTier::Cheap => ModelPricing {
            input_usd_micros_per_mtok: 220_000,
            output_usd_micros_per_mtok: 660_000,
        },
        ModelTier::Standard => ModelPricing {
            input_usd_micros_per_mtok: 3_000_000,
            output_usd_micros_per_mtok: 15_000_000,
        },
        ModelTier::Frontier => FRONTIER_REFERENCE_PRICING,
    }
}

/// Outcome of one model attempt on the cheap-first verify loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RouteOutcome {
    /// Worker is still running at this tier.
    Running,
    /// Diff failed verification at this tier.
    FailedVerify,
    /// Diff passed verification at this tier.
    Passed,
    /// Orchestrator escalated to a stronger tier after retries.
    Escalated,
}

impl fmt::Display for RouteOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            RouteOutcome::Running => "running",
            RouteOutcome::FailedVerify => "failed-verify",
            RouteOutcome::Passed => "passed",
            RouteOutcome::Escalated => "escalated",
        };
        f.write_str(s)
    }
}

/// One hop in the cheap-first route: which model ran, at which tier, and
/// whether verification accepted the diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteStep {
    /// Provider model id when known; empty until the first LLM response.
    pub model: String,
    /// Planner/orchestrator tier for this attempt.
    pub tier: ModelTier,
    /// What happened after this attempt.
    pub outcome: RouteOutcome,
}

impl RouteStep {
    /// Build a route hop.
    pub fn new(model: impl Into<String>, tier: ModelTier, outcome: RouteOutcome) -> Self {
        Self {
            model: model.into(),
            tier,
            outcome,
        }
    }
}

/// Goal-level cost receipt: estimated model spend vs a frontier counterfactual.
///
/// This is the product metric: verified success per dollar, not raw tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostReceipt {
    /// Estimated model spend for the models used. The serialized field name
    /// predates the estimate label and is retained for stored receipts.
    pub actual_usd_micros: u64,
    /// What the same token mix would have cost at [`FRONTIER_REFERENCE_PRICING`].
    pub frontier_equivalent_usd_micros: u64,
    /// `frontier - estimated spend`, saturating at zero.
    pub saved_usd_micros: u64,
    /// True when every charged call had a registered `BudgetGuard` rate.
    pub pricing_known: bool,
    /// Ordered hops: cheap first, then repair, then escalation if needed.
    pub route: Vec<RouteStep>,
}

impl CostReceipt {
    /// Assemble a receipt from observed usage, estimated micros, and the route.
    pub fn from_usage(
        actual_usd_micros: u64,
        usage: &TokenUsage,
        pricing_known: bool,
        route: Vec<RouteStep>,
    ) -> Self {
        let frontier =
            FRONTIER_REFERENCE_PRICING.cost_micros(usage.input_tokens, usage.output_tokens);
        let saved = frontier.saturating_sub(actual_usd_micros);
        Self {
            actual_usd_micros,
            frontier_equivalent_usd_micros: frontier,
            saved_usd_micros: saved,
            pricing_known,
            route,
        }
    }

    /// Percent saved vs the frontier counterfactual. `None` when there is
    /// no frontier baseline (zero tokens).
    pub fn saved_percent(&self) -> Option<i64> {
        if self.frontier_equivalent_usd_micros == 0 {
            return None;
        }
        let diff = self.frontier_equivalent_usd_micros as i64 - self.actual_usd_micros as i64;
        Some(((diff as f64 / self.frontier_equivalent_usd_micros as f64) * 100.0).round() as i64)
    }
}

#[cfg(test)]
mod cost_receipt_tests {
    use super::*;

    #[test]
    fn cheap_tokens_save_against_frontier_reference() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            ..TokenUsage::default()
        };
        let actual = reference_pricing_for_tier(ModelTier::Cheap).cost_micros(1_000_000, 0);
        let receipt = CostReceipt::from_usage(actual, &usage, true, Vec::new());
        assert_eq!(receipt.actual_usd_micros, 220_000);
        assert_eq!(receipt.frontier_equivalent_usd_micros, 15_000_000);
        assert_eq!(receipt.saved_usd_micros, 14_780_000);
        assert_eq!(receipt.saved_percent(), Some(99));
    }
}

/// User-facing budget ceiling enforced by `phonton-orchestrator`'s
/// `BudgetGuard`.
///
/// Either limit is optional; a `BudgetLimits::default()` imposes no cap.
/// When both are set, whichever ceiling is hit first pauses the run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetLimits {
    /// Cap on total tokens (input + output, summed across tiers).
    pub max_tokens: Option<u64>,
    /// Cap on total spend, in micro-dollars.
    pub max_usd_micros: Option<u64>,
}

/// Verdict returned by `BudgetGuard` after each worker call is accounted for.
///
/// `Ok` keeps the run going; `Pause` tells the orchestrator to abort the
/// in-flight DAG and surface a paused status to the UI so the user can
/// lift the cap or approve continuation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BudgetDecision {
    /// Under budget; continue dispatching.
    Ok,
    /// Ceiling hit — orchestrator must pause.
    Pause {
        /// Which limit tripped — `"tokens"` or `"usd"`.
        limit: String,
        /// Observed value at the time of the pause.
        observed: u64,
        /// Configured ceiling that was crossed.
        ceiling: u64,
    },
}

// ---------------------------------------------------------------------------
// Provider errors
// ---------------------------------------------------------------------------

/// Typed error returned by HTTP-backed provider adaptors.
///
/// Lets callers retry intelligently (back off on [`RateLimit`], abort on
/// [`AuthFailed`]) without string-matching `anyhow` messages.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// HTTP 429 — throttled. Callers should back off and retry.
    #[error("provider rate-limited (429)")]
    RateLimit,
    /// HTTP 401/403 — bad or missing API key. Not retryable.
    #[error("provider auth failed ({0})")]
    AuthFailed(u16),
    /// HTTP 5xx — provider-side problem. Usually retryable.
    #[error("provider server error ({0})")]
    ServerError(u16),
    /// Response was not the JSON shape we expect.
    #[error("provider response parse failed: {0}")]
    ParseFail(String),
    /// Transport-level failure (DNS, TLS, connection reset).
    #[error("provider transport error: {0}")]
    Transport(String),
}
