//! LLM abstraction layer (`packages/ai`). Types + the Anthropic Messages API
//! streaming provider.

pub mod anthropic;
pub mod stream;
pub mod types;

pub use types::{
    Api, Message, Model, ThinkingLevel, Usage, UserMessage,
};

use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock time in milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The default base URL for the Anthropic API.
pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";

/// A curated model registry. A handful of Anthropic models with sensible
/// defaults; resolved/overridden at runtime via env vars and CLI flags.
pub fn builtin_models() -> Vec<Model> {
    let base = std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| DEFAULT_ANTHROPIC_BASE_URL.to_string());
    vec![
        Model {
            id: "claude-sonnet-4-6".into(),
            provider: "anthropic".into(),
            api: Api::AnthropicMessages,
            max_tokens: 64_000,
            context_window: 1_000_000,
            base_url: base.clone(),
            reasoning: true,
            force_adaptive_thinking: true,
            supports_temperature: false,
            input_cost_per_mtok: 3.0,
            output_cost_per_mtok: 15.0,
            cache_read_cost_per_mtok: 0.30,
            cache_write_cost_per_mtok: 3.75,
        },
        Model {
            id: "claude-opus-4-8".into(),
            provider: "anthropic".into(),
            api: Api::AnthropicMessages,
            max_tokens: 64_000,
            context_window: 1_000_000,
            base_url: base.clone(),
            reasoning: true,
            force_adaptive_thinking: true,
            supports_temperature: false,
            input_cost_per_mtok: 15.0,
            output_cost_per_mtok: 75.0,
            cache_read_cost_per_mtok: 1.50,
            cache_write_cost_per_mtok: 18.75,
        },
        Model {
            id: "claude-haiku-4-5".into(),
            provider: "anthropic".into(),
            api: Api::AnthropicMessages,
            max_tokens: 64_000,
            context_window: 1_000_000,
            base_url: base.clone(),
            reasoning: true,
            force_adaptive_thinking: true,
            supports_temperature: false,
            input_cost_per_mtok: 1.0,
            output_cost_per_mtok: 5.0,
            cache_read_cost_per_mtok: 0.10,
            cache_write_cost_per_mtok: 1.25,
        },
    ]
}

/// Resolve a model by `provider/id` pattern (supports just `id`). An optional
/// `:thinking` suffix is tolerated and ignored here (thinking is set elsewhere).
pub fn resolve_model(registry: &[Model], pattern: &str) -> Option<Model> {
    let (provider, id) = match pattern.split_once('/') {
        Some((p, id)) => (Some(p), id),
        None => (None, pattern),
    };
    let needle = id.split(':').next().unwrap_or(id).to_ascii_lowercase();
    let provider = provider.map(str::to_ascii_lowercase);
    registry
        .iter()
        .filter(|m| provider.as_deref().is_none_or(|p| m.provider == p))
        .find(|m| {
            let id = m.id.to_ascii_lowercase();
            id == needle || id.ends_with(&format!("-{needle}")) || id.contains(&needle)
        })
        .cloned()
}
