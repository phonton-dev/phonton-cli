//! Typed provider registry — single source for env vars, pricing, and doctor probes.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ProviderRegistryEntry {
    pub id: &'static str,
    pub display_name: &'static str,
    pub env_keys: &'static [&'static str],
    pub default_cheap: &'static str,
    pub default_standard: &'static str,
    pub default_frontier: &'static str,
    pub openai_compatible: bool,
}

pub const PROVIDER_REGISTRY: &[ProviderRegistryEntry] = &[
    ProviderRegistryEntry {
        id: "anthropic",
        display_name: "Anthropic",
        env_keys: &["ANTHROPIC_API_KEY"],
        default_cheap: "claude-haiku-4-5",
        default_standard: "claude-sonnet-4-6",
        default_frontier: "claude-opus-4-6",
        openai_compatible: false,
    },
    ProviderRegistryEntry {
        id: "openai",
        display_name: "OpenAI",
        env_keys: &["OPENAI_API_KEY"],
        default_cheap: "gpt-4.1-mini",
        default_standard: "gpt-4.1",
        default_frontier: "o3",
        openai_compatible: true,
    },
    ProviderRegistryEntry {
        id: "deepseek",
        display_name: "DeepSeek",
        env_keys: &["DEEPSEEK_API_KEY"],
        default_cheap: "deepseek-chat",
        default_standard: "deepseek-chat",
        default_frontier: "deepseek-reasoner",
        openai_compatible: true,
    },
];

pub fn find_provider(id: &str) -> Option<&'static ProviderRegistryEntry> {
    PROVIDER_REGISTRY.iter().find(|entry| entry.id == id)
}
