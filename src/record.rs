use serde::{Deserialize, Serialize};

use crate::protocol::Usage;

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

/// $/Mtok. `cache_read`/`cache_write` are absolute prices, not multipliers —
/// providers discount caching very differently (Anthropic reads at 0.1x with
/// a 1.25x write surcharge; GPT-5 reads at 0.1x, gpt-4o at 0.5x, gpt-4.1 at
/// 0.25x, none with a write surcharge). Entries with no published cache
/// price carry the plain input price.
pub struct ModelPrice {
    pub prefix: &'static str,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

pub fn price(model: &str) -> Option<&'static ModelPrice> {
    // Gateways like OpenRouter vendor-prefix model names ("openai/gpt-4o-mini").
    let bare = model.rsplit('/').next().unwrap_or(model);
    crate::prices::PRICES
        .iter()
        .find(|p| model.starts_with(p.prefix) || bare.starts_with(p.prefix))
}

/// Dollars saved by cache reads (vs paying full input price), and dollars
/// wasted on full-price input that a warm cache would have served — the
/// record must be a follow-up turn (`had_prior_turn`) for waste to count.
pub fn cache_economics(rec: &RequestRecord, had_prior_turn: bool) -> (f64, f64) {
    let Some(p) = price(&rec.model) else {
        return (0.0, 0.0);
    };
    let discount = (p.input - p.cache_read).max(0.0);
    let saved = rec.cache_read_tokens as f64 * discount / 1_000_000.0;
    // Below ~1k tokens the prefix wouldn't cache anyway.
    let wasted = if had_prior_turn && rec.cache_read_tokens == 0 && rec.input_tokens > 1_024 {
        rec.input_tokens as f64 * discount / 1_000_000.0
    } else {
        0.0
    };
    (saved, wasted)
}

/// Cost of one request. A gateway-reported cost wins outright — OpenRouter
/// sends the authoritative USD amount in `usage.cost` on every response —
/// otherwise token counts are priced against the built-in table.
pub fn cost_usd(model: &str, usage: &Usage) -> f64 {
    if let Some(reported) = usage.reported_cost {
        return reported;
    }
    let Some(p) = price(model) else {
        return 0.0;
    };
    (usage.input as f64 * p.input
        + usage.cache_read as f64 * p.cache_read
        + usage.cache_write as f64 * p.cache_write
        + usage.output as f64 * p.output)
        / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_flagship_models_are_priced() {
        for m in [
            "claude-sonnet-5",
            "claude-fable-5",
            "claude-opus-4-8",
            "claude-haiku-4-5-20251001",
            "gpt-5.2-codex",
            "gemini-3-pro-preview",
        ] {
            assert!(price(m).is_some(), "{m} missing from price table");
        }
    }

    #[test]
    fn most_specific_prefix_wins() {
        // The lookup takes the first match, so the generated table must stay
        // ordered longest-prefix-first (equal-length prefixes can never both
        // match one model name).
        let lens: Vec<usize> = crate::prices::PRICES.iter().map(|p| p.prefix.len()).collect();
        assert!(lens.windows(2).all(|w| w[0] >= w[1]), "table not sorted");
        assert_eq!(price("gpt-5.2-pro").unwrap().input, 21.0);
        assert_eq!(price("gpt-5.2").unwrap().input, 1.75);
        assert_eq!(price("gpt-5-mini-2025-08-07").unwrap().input, 0.25);
    }

    #[test]
    fn gateway_slugs_price_like_first_party() {
        assert_eq!(
            price("anthropic/claude-sonnet-5").unwrap().prefix,
            price("claude-sonnet-5").unwrap().prefix,
        );
    }

    #[test]
    fn cache_read_prices_are_per_model() {
        // GPT-5 caches at 0.1x input, gpt-4o at 0.5x — a flat multiplier
        // would misprice one of them.
        let g5 = price("gpt-5").unwrap();
        let g4o = price("gpt-4o").unwrap();
        assert!((g5.cache_read / g5.input - 0.1).abs() < 1e-9);
        assert!((g4o.cache_read / g4o.input - 0.5).abs() < 1e-9);
    }

    #[test]
    fn reported_cost_overrides_table() {
        let usage = Usage {
            input: 1_000_000,
            reported_cost: Some(0.0123),
            ..Usage::default()
        };
        assert_eq!(cost_usd("claude-sonnet-5", &usage), 0.0123);
    }
}
