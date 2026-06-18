//! Cost estimation. All rates are USD per 1,000,000 tokens.
//!
//! These are list-price ESTIMATES used purely to show a "cost equivalent" of
//! local usage (the same idea as Theo's "Estimated from local logs"). They are
//! not what you actually pay on a subscription plan. Tune the numbers below to
//! match the models you use and current pricing.

/// Per-million-token rates for one model family.
#[derive(Clone, Copy)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

impl Rates {
    /// Cost in USD for a single request's token breakdown.
    pub fn cost(&self, input: u64, output: u64, cache_write: u64, cache_read: u64) -> f64 {
        let per = 1_000_000.0;
        (input as f64 * self.input
            + output as f64 * self.output
            + cache_write as f64 * self.cache_write
            + cache_read as f64 * self.cache_read)
            / per
    }
}

// ---- Claude (Anthropic) list prices, USD / Mtok -------------------------
const CLAUDE_OPUS: Rates = Rates {
    input: 15.0,
    output: 75.0,
    cache_write: 18.75,
    cache_read: 1.5,
};
const CLAUDE_SONNET: Rates = Rates {
    input: 3.0,
    output: 15.0,
    cache_write: 3.75,
    cache_read: 0.3,
};
const CLAUDE_HAIKU: Rates = Rates {
    input: 0.8,
    output: 4.0,
    cache_write: 1.0,
    cache_read: 0.08,
};

/// Map a Claude model id (e.g. "claude-opus-4-8") to its rate card.
pub fn claude_rates(model: &str) -> Rates {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") {
        CLAUDE_OPUS
    } else if m.contains("haiku") {
        CLAUDE_HAIKU
    } else {
        // sonnet + anything unknown defaults to sonnet pricing
        CLAUDE_SONNET
    }
}

// ---- Codex (OpenAI gpt-5 family) list prices, USD / Mtok ----------------
// Codex token_count reports input_tokens, cached_input_tokens, output_tokens.
// We treat cached input via cache_read; there is no separate cache-write line.
const CODEX_GPT5: Rates = Rates {
    input: 1.25,
    output: 10.0,
    cache_write: 0.0,
    cache_read: 0.125,
};

/// Rates for a Codex model. Falls back to gpt-5 pricing.
pub fn codex_rates(_model: &str) -> Rates {
    CODEX_GPT5
}
