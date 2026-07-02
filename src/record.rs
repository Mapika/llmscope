use serde::{Deserialize, Serialize};

use crate::protocol::{Provider, Usage};

/// One captured LLM API call. Bodies are stored in SQLite but never sent
/// over the stats API — `top` only ever sees this metadata shape.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestRecord {
    pub id: i64,
    /// Request start, unix millis.
    pub ts_ms: i64,
    pub provider: String,
    pub model: String,
    pub path: String,
    pub status: i64,
    /// Billed (non-cached) input tokens.
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// -1 when no body chunk was observed.
    pub ttft_ms: i64,
    pub duration_ms: i64,
    pub cost_usd: f64,
    pub streamed: bool,
    /// True when the provider sent no usage block and numbers are heuristic.
    pub estimated: bool,
    /// Conversation fingerprint grouping requests of one agent run; empty
    /// for non-chat requests and rows captured before this field existed.
    #[serde(default)]
    pub session: String,
}

/// $/Mtok (input, output), matched by model-name prefix. More specific
/// prefixes must come before shorter ones. Unknown models cost $0 — the
/// table is deliberately small and easy to extend.
const PRICES: &[(&str, f64, f64)] = &[
    ("claude-opus-4-1", 15.0, 75.0),
    ("claude-opus-4", 5.0, 25.0),
    ("claude-sonnet-4", 3.0, 15.0),
    ("claude-haiku-4", 1.0, 5.0),
    ("claude-3-5-sonnet", 3.0, 15.0),
    ("claude-3-5-haiku", 0.8, 4.0),
    // Gateways sometimes emit version-first slugs ("claude-4.5-haiku-20251001").
    ("claude-4.5-haiku", 1.0, 5.0),
    ("claude-4.5-sonnet", 3.0, 15.0),
    ("claude-4.6-sonnet", 3.0, 15.0),
    ("gpt-4o-mini", 0.15, 0.6),
    ("gpt-4o", 2.5, 10.0),
    ("gpt-4.1-nano", 0.1, 0.4),
    ("gpt-4.1-mini", 0.4, 1.6),
    ("gpt-4.1", 2.0, 8.0),
    ("gpt-5-nano", 0.05, 0.4),
    ("gpt-5-mini", 0.25, 2.0),
    ("gpt-5", 1.25, 10.0),
    ("o4-mini", 1.1, 4.4),
    ("o3", 2.0, 8.0),
];

pub fn price(model: &str) -> Option<(f64, f64)> {
    // Gateways like OpenRouter vendor-prefix model names ("openai/gpt-4o-mini").
    let bare = model.rsplit('/').next().unwrap_or(model);
    PRICES
        .iter()
        .find(|(prefix, _, _)| model.starts_with(prefix) || bare.starts_with(prefix))
        .map(|(_, i, o)| (*i, *o))
}

pub fn cost_usd(provider: Provider, model: &str, usage: &Usage) -> f64 {
    let Some((pin, pout)) = price(model) else {
        return 0.0;
    };
    // Claude served through an OpenAI-protocol gateway (OpenRouter etc.) still
    // bills with Anthropic's cache multipliers.
    let anthropic_pricing = provider == Provider::Anthropic || model.contains("claude");
    let (read_mult, write_mult) = if anthropic_pricing {
        // Anthropic: cache reads 0.1x, cache writes 1.25x the input price.
        (0.1, 1.25)
    } else {
        // OpenAI: cached input is half price; there is no write surcharge.
        (0.5, 1.0)
    };
    (usage.input as f64 * pin
        + usage.cache_read as f64 * read_mult * pin
        + usage.cache_write as f64 * write_mult * pin
        + usage.output as f64 * pout)
        / 1_000_000.0
}
