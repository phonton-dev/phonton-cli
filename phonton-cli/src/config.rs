//! `~/.phonton/config.toml` loader.
//!
//! Provides the [`Config`] struct and [`load`] function. On first run the
//! file is absent; [`load`] returns a default config rather than an error.
//!
//! # Example config
//! ```toml
//! [provider]
//! # Which provider to use: "anthropic" | "openai" | "openrouter" | "gemini"
//! name = "anthropic"
//! api_key = "sk-ant-..."
//! # Optional model override. Defaults are picked per provider.
//! model = "claude-sonnet-4-5-20251022"
//!
//! [budget]
//! max_tokens = 500000
//! # max_usd_cents = 100   # hard stop at $1.00 per session
//! ```
//!
//! **Security note:** the file is read from disk at startup only. The API
//! key is never logged and never sent to any endpoint other than the
//! configured provider.

use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Top-level configuration loaded from `~/.phonton/config.toml`.
#[derive(Debug, Clone, Deserialize, serde::Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Provider selection and credentials.
    #[serde(default)]
    pub provider: ProviderConfig,

    /// Spending / token limits.
    #[serde(default)]
    pub budget: BudgetConfig,
}

/// `[provider]` table.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub struct ProviderConfig {
    /// Provider name, e.g. `"anthropic"`, `"openai"`, `"gemini"`,
    /// `"ollama"`, or `"openai-compatible"`.
    /// Defaults to `"anthropic"` when no config exists.
    #[serde(default = "default_provider_name")]
    pub name: String,

    /// API key. When absent here, the loader falls back to the standard
    /// environment variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, …).
    pub api_key: Option<String>,

    /// Model override. When absent, each provider picks its own default.
    pub model: Option<String>,

    /// Base URL override for self-hosted / proxy endpoints (OpenAI-compat).
    pub base_url: Option<String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            name: default_provider_name(),
            api_key: None,
            model: None,
            base_url: None,
        }
    }
}

fn default_provider_name() -> String {
    "anthropic".to_string()
}

/// `[budget]` table.
#[derive(Debug, Clone, Deserialize, serde::Serialize, Default)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
pub struct BudgetConfig {
    /// Hard stop at this many tokens per session (`None` = unlimited).
    pub max_tokens: Option<u64>,

    /// Hard stop at this many US cents per session (`None` = unlimited).
    /// Stored as cents so the TOML value is human-readable.
    pub max_usd_cents: Option<u64>,
}

#[allow(dead_code)]
impl BudgetConfig {
    /// Convert to micro-dollars for `BudgetLimits`.
    pub fn max_usd_micros(&self) -> Option<u64> {
        self.max_usd_cents.map(|c| c.saturating_mul(10_000)) // 1 cent = 10_000 µ$
    }
}

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

/// Return the path to `~/.phonton/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".phonton").join("config.toml"))
}

/// Load configuration from `~/.phonton/config.toml`.
///
/// Returns `Config::default()` when the file is absent. Returns an error
/// only when the file exists but cannot be parsed.
pub fn load() -> Result<Config> {
    let path = match config_path() {
        Some(p) => p,
        None => return Ok(Config::default()),
    };

    if !path.exists() {
        return Ok(Config::default());
    }

    let raw = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;

    let cfg: Config = toml::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", path.display()))?;

    Ok(cfg)
}

/// Save configuration to `~/.phonton/config.toml`.
pub fn save(cfg: &Config) -> Result<()> {
    let path = match config_path() {
        Some(p) => p,
        None => return Err(anyhow::anyhow!("could not determine config path")),
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let raw = toml::to_string(cfg)?;
    std::fs::write(&path, raw)?;

    Ok(())
}

/// Resolve the effective API key for the configured provider.
///
/// Priority: config file `api_key` → environment variable.
pub fn resolve_api_key(cfg: &ProviderConfig) -> Option<String> {
    if let Some(ref key) = cfg.api_key {
        return Some(key.clone());
    }
    // Each provider gets its own canonical env var so users with multiple
    // keys configured can switch between them without re-pasting.
    let candidates: &[&str] = match cfg.name.as_str() {
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "gemini" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        "agentrouter" => &["AGENTROUTER_API_KEY", "ANTHROPIC_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "xai" | "grok" => &["XAI_API_KEY", "GROK_API_KEY"],
        "groq" => &["GROQ_API_KEY"],
        "together" => &["TOGETHER_API_KEY", "TOGETHER_AI_API_KEY"],
        "ollama" | "custom" | "openai-compatible" => return None,
        _ => return None,
    };
    for var in candidates {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// All provider names recognised by the CLI Settings panel, in the order
/// they should be presented to the user. Cycling with Tab on the Provider
/// field walks this list.
pub const KNOWN_PROVIDERS: &[&str] = &[
    "anthropic",
    "openai",
    "openrouter",
    "gemini",
    "agentrouter",
    "deepseek",
    "xai",
    "groq",
    "together",
    "ollama",
    "openai-compatible",
    "custom",
];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let raw = r#"
[provider]
name = "openai"
api_key = "sk-test"
model = "gpt-4o"

[budget]
max_tokens = 100000
max_usd_cents = 50
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.provider.name, "openai");
        assert_eq!(cfg.provider.api_key.as_deref(), Some("sk-test"));
        assert_eq!(cfg.provider.model.as_deref(), Some("gpt-4o"));
        assert_eq!(cfg.budget.max_tokens, Some(100_000));
        assert_eq!(cfg.budget.max_usd_micros(), Some(500_000));
    }

    #[test]
    fn parses_minimal_config() {
        let raw = "[provider]\nname = \"gemini\"\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.provider.name, "gemini");
        assert!(cfg.budget.max_tokens.is_none());
    }

    #[test]
    fn empty_file_is_default() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.provider.name, "anthropic");
    }

    #[test]
    fn resolve_api_key_env_fallback() {
        let cfg = ProviderConfig {
            name: "anthropic".into(),
            api_key: None,
            model: None,
            base_url: None,
        };
        // No env var set in test — should return None.
        // (In production the real key is present.)
        let _ = resolve_api_key(&cfg); // must not panic
    }

    #[test]
    fn resolve_api_key_prefers_config_over_env() {
        let cfg = ProviderConfig {
            name: "anthropic".into(),
            api_key: Some("from-config".into()),
            model: None,
            base_url: None,
        };
        assert_eq!(resolve_api_key(&cfg).as_deref(), Some("from-config"));
    }
}
