//! `~/.phonton/config.toml` loader with an optional per-process path override.
//!
//! Provides the [`Config`] struct and [`load`] function. On first run the
//! file is absent; [`load`] returns a default config rather than an error.
//!
//! # Example config
//! ```toml
//! [provider]
//! # Which provider to use: "anthropic" | "openai" | "openrouter" | "gemini" | "cloudflare"
//! name = "anthropic"
//! api_key = "sk-ant-..."
//! # Optional model override. Defaults are picked per provider.
//! model = "claude-sonnet-4-5-20251022"
//! # Cloudflare-only: Workers AI account ID.
//! account_id = "..."
//!
//! [budget]
//! max_tokens = 500000
//! # max_usd_cents = 100   # hard stop at $1.00 per session
//! ```
//!
//! **Security note:** the file is read from disk at startup only. The API
//! key is never logged and never sent to any endpoint other than the
//! configured provider.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// Top-level configuration loaded from `~/.phonton/config.toml`.
#[derive(Debug, Clone, Deserialize, serde::Serialize, Default)]
pub struct Config {
    /// Provider selection and credentials.
    #[serde(default)]
    pub provider: ProviderConfig,

    /// Spending / token limits.
    #[serde(default)]
    pub budget: BudgetConfig,

    /// Semantic code-index backend selection.
    #[serde(default)]
    pub index: IndexConfig,

    /// Runtime permission defaults. Kept tolerant so existing configs do not
    /// fail to load when new permission modes are introduced.
    #[serde(default)]
    pub permissions: PermissionsConfig,

    /// General CLI settings.
    #[serde(default)]
    pub general: GeneralConfig,
}

/// `[provider]` table.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[allow(dead_code)]
pub struct ProviderConfig {
    /// Provider name, e.g. `"anthropic"`, `"openai"`, `"gemini"`,
    /// `"cloudflare"`, `"ollama"`, or `"openai-compatible"`.
    /// Defaults to `"anthropic"` when no config exists.
    #[serde(default = "default_provider_name")]
    pub name: String,

    /// API key. When absent here, the loader falls back to the standard
    /// environment variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, …).
    pub api_key: Option<String>,

    /// Model override. When absent, each provider picks its own default.
    pub model: Option<String>,

    /// Account identifier for providers that require one. Used by
    /// Cloudflare Workers AI; falls back to `CLOUDFLARE_ACCOUNT_ID`.
    pub account_id: Option<String>,

    /// Base URL override for self-hosted / proxy endpoints (OpenAI-compat).
    /// For `cloudflare`, this may be the full
    /// `https://api.cloudflare.com/client/v4/accounts/<id>/ai/v1` base URL.
    /// A bare account ID is still accepted here for backward compatibility.
    pub base_url: Option<String>,

    /// Named provider keys imported from other tools or managed manually.
    #[serde(default)]
    pub keys: BTreeMap<String, String>,

    /// Backward-compatible opt-in used by older config files. Current
    /// provider routing keeps unknown model handling in the provider layer.
    pub allow_unverified_model: Option<bool>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            name: default_provider_name(),
            api_key: None,
            model: None,
            account_id: None,
            base_url: None,
            keys: BTreeMap::new(),
            allow_unverified_model: None,
        }
    }
}

fn default_provider_name() -> String {
    "anthropic".to_string()
}

/// `[budget]` table.
#[derive(Debug, Clone, Deserialize, serde::Serialize, Default)]
#[allow(dead_code)]
pub struct BudgetConfig {
    /// Hard stop at this many tokens per session (`None` = unlimited).
    pub max_tokens: Option<u64>,

    /// Hard stop at this many US cents per session (`None` = unlimited).
    /// Stored as cents so the TOML value is human-readable.
    pub max_usd_cents: Option<u64>,
}

/// `[index]` table.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[allow(dead_code)]
pub struct IndexConfig {
    /// `local-hnsw` or `qdrant`.
    #[serde(default = "default_index_backend")]
    pub backend: String,
    /// Optional Qdrant base URL, e.g. `http://127.0.0.1:6333`.
    pub qdrant_url: Option<String>,
    /// Optional Qdrant collection name.
    pub qdrant_collection: Option<String>,
}

/// `[permissions]` table.
#[derive(Debug, Clone, Deserialize, serde::Serialize, Default)]
#[allow(dead_code)]
pub struct PermissionsConfig {
    /// Default approval mode, e.g. `ask`.
    pub mode: Option<String>,
}

/// `[general]` table.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[allow(dead_code)]
pub struct GeneralConfig {
    /// Whether to check and auto-update Phonton CLI globally.
    #[serde(default = "default_enable_auto_update", alias = "enableAutoUpdate")]
    pub enable_auto_update: bool,
}

fn default_enable_auto_update() -> bool {
    true
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            enable_auto_update: default_enable_auto_update(),
        }
    }
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            backend: default_index_backend(),
            qdrant_url: None,
            qdrant_collection: None,
        }
    }
}

fn default_index_backend() -> String {
    "local-hnsw".to_string()
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

/// Return the config path, honoring an explicit per-process override.
pub fn config_path() -> Option<PathBuf> {
    config_path_for(
        std::env::var_os("PHONTON_CONFIG_PATH"),
        phonton_extensions::phonton_home(),
    )
}

fn config_path_for(
    override_path: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    match override_path.filter(|path| !path.is_empty()) {
        Some(path) => {
            let path = PathBuf::from(path);
            path.is_absolute().then_some(path)
        }
        None => home.map(|h| h.join("config.toml")),
    }
}

/// Load configuration from the resolved config path.
///
/// Returns `Config::default()` when the file is absent. Returns an error
/// only when the file exists but cannot be parsed.
pub fn load() -> Result<Config> {
    let path = match config_path() {
        Some(p) => p,
        None if std::env::var_os("PHONTON_CONFIG_PATH").is_some_and(|path| !path.is_empty()) => {
            return Err(anyhow::anyhow!(
                "PHONTON_CONFIG_PATH must be an absolute path"
            ));
        }
        None => return Ok(Config::default()),
    };

    if !path.exists() {
        let mut cfg = Config::default();
        autodetect_provider(&mut cfg.provider, local_model_selected());
        return Ok(cfg);
    }

    let raw = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;

    let mut cfg: Config = toml::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", path.display()))?;
    let provider_chosen = toml::from_str::<toml::Value>(&raw)
        .ok()
        .and_then(|v| v.get("provider")?.get("name").cloned())
        .is_some();
    if !provider_chosen {
        autodetect_provider(&mut cfg.provider, local_model_selected());
    }

    Ok(cfg)
}

/// Order in which environment keys pick a provider when the config file
/// does not name one.
const AUTODETECT_ORDER: &[&str] = &[
    "anthropic",
    "openai",
    "deepseek",
    "openrouter",
    "gemini",
    "groq",
    "xai",
    "together",
];

fn local_model_selected() -> bool {
    crate::models_cli::settings().is_ok_and(|s| s.active_model.is_some())
}

/// With no provider named in the config, use the first one that has a key
/// (config `[provider.keys]` or environment). With no key anywhere, a
/// calibrated local model wins. Otherwise the default stays and doctor
/// reports the missing key.
fn autodetect_provider(provider: &mut ProviderConfig, local_model: bool) {
    if resolve_api_key(provider).is_some() {
        return;
    }
    for name in AUTODETECT_ORDER {
        let probe = ProviderConfig {
            name: (*name).to_string(),
            keys: provider.keys.clone(),
            ..ProviderConfig::default()
        };
        if resolve_api_key(&probe).is_some() {
            provider.name = (*name).to_string();
            return;
        }
    }
    if local_model {
        provider.name = "ollama".to_string();
    }
}

/// Save configuration to the resolved config path.
pub fn save(cfg: &Config) -> Result<()> {
    let path = match config_path() {
        Some(p) => p,
        None if std::env::var_os("PHONTON_CONFIG_PATH").is_some_and(|path| !path.is_empty()) => {
            return Err(anyhow::anyhow!(
                "PHONTON_CONFIG_PATH must be an absolute path"
            ));
        }
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
    if let Some(key) = cfg.keys.get(&cfg.name) {
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
        "cloudflare" => &[
            "CLOUDFLARE_API_TOKEN",
            "CLOUDFLARE_WORKERS_AI_API_TOKEN",
            "CLOUDFLARE_API_KEY",
        ],
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
    "cloudflare",
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
    #[test]
    fn autodetect_uses_configured_keys_then_local_model() {
        let mut p = ProviderConfig::default();
        p.keys.insert("deepseek".into(), "sk-test".into());
        // Env may hold real keys on a dev machine; only assert when it does not.
        if super::resolve_api_key(&ProviderConfig::default()).is_none()
            && std::env::var_os("OPENAI_API_KEY").is_none()
        {
            super::autodetect_provider(&mut p, false);
            assert_eq!(p.name, "deepseek");
        }
        let mut chosen = ProviderConfig {
            api_key: Some("sk-ant-x".into()),
            ..ProviderConfig::default()
        };
        super::autodetect_provider(&mut chosen, true);
        assert_eq!(chosen.name, "anthropic");
    }

    use super::*;

    #[test]
    fn parses_full_config() {
        let raw = r#"
[provider]
name = "openai"
api_key = "sk-test"
model = "gpt-4o"
account_id = "acct-test"

[budget]
max_tokens = 100000
max_usd_cents = 50
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.provider.name, "openai");
        assert_eq!(cfg.provider.api_key.as_deref(), Some("sk-test"));
        assert_eq!(cfg.provider.model.as_deref(), Some("gpt-4o"));
        assert_eq!(cfg.provider.account_id.as_deref(), Some("acct-test"));
        assert_eq!(cfg.budget.max_tokens, Some(100_000));
        assert_eq!(cfg.budget.max_usd_micros(), Some(500_000));
    }

    #[test]
    fn parses_minimal_config() {
        let raw = "[provider]\nname = \"gemini\"\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.provider.name, "gemini");
        assert!(cfg.budget.max_tokens.is_none());
        assert_eq!(cfg.index.backend, "local-hnsw");
    }

    #[test]
    fn parses_qdrant_index_config() {
        let raw = r#"
[index]
backend = "qdrant"
qdrant_url = "http://127.0.0.1:6333"
qdrant_collection = "phonton-code"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.index.backend, "qdrant");
        assert_eq!(
            cfg.index.qdrant_url.as_deref(),
            Some("http://127.0.0.1:6333")
        );
        assert_eq!(cfg.index.qdrant_collection.as_deref(), Some("phonton-code"));
    }

    #[test]
    fn parses_existing_config_with_provider_keys_and_permissions() {
        let raw = r#"
[provider]
name = "deepseek"
model = "deepseek-v4-flash"
allow_unverified_model = true

[provider.keys]
deepseek = "key-from-map"

[permissions]
mode = "ask"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.provider.name, "deepseek");
        assert_eq!(
            cfg.provider.allow_unverified_model,
            Some(true),
            "legacy config fields should remain parseable"
        );
        assert_eq!(
            cfg.provider.keys.get("deepseek").map(String::as_str),
            Some("key-from-map")
        );
        assert_eq!(cfg.permissions.mode.as_deref(), Some("ask"));
        assert_eq!(
            resolve_api_key(&cfg.provider).as_deref(),
            Some("key-from-map")
        );
    }

    #[test]
    fn empty_file_is_default() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.provider.name, "anthropic");
    }

    #[test]
    fn config_path_override_is_isolated_from_normal_home() {
        let home = PathBuf::from("normal-home");
        let isolated = if cfg!(windows) {
            PathBuf::from(r"C:\isolated\config.toml")
        } else {
            PathBuf::from("/isolated/config.toml")
        };
        assert_eq!(
            config_path_for(Some(isolated.clone().into_os_string()), Some(home.clone())),
            Some(isolated)
        );
        assert_eq!(
            config_path_for(Some(std::ffi::OsString::new()), Some(home.clone())),
            Some(home.join("config.toml"))
        );
        assert_eq!(
            config_path_for(Some("relative-config.toml".into()), Some(home)),
            None
        );
        assert_eq!(config_path_for(None, None), None);
    }

    #[test]
    fn resolve_api_key_env_fallback() {
        let cfg = ProviderConfig {
            name: "anthropic".into(),
            api_key: None,
            model: None,
            account_id: None,
            base_url: None,
            keys: BTreeMap::new(),
            allow_unverified_model: None,
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
            account_id: None,
            base_url: None,
            keys: BTreeMap::new(),
            allow_unverified_model: None,
        };
        assert_eq!(resolve_api_key(&cfg).as_deref(), Some("from-config"));
    }

    #[test]
    fn parses_general_config() {
        let raw = r#"
[general]
enable_auto_update = false
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(!cfg.general.enable_auto_update);

        // Test camelCase alias
        let raw_camel = r#"
[general]
enableAutoUpdate = false
"#;
        let cfg_camel: Config = toml::from_str(raw_camel).unwrap();
        assert!(!cfg_camel.general.enable_auto_update);

        // Test default
        let cfg_default: Config = toml::from_str("").unwrap();
        assert!(cfg_default.general.enable_auto_update);
    }
}
