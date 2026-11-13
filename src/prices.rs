// API list prices in USD per million tokens, checked 2026-11-10.
// Claude: https://platform.claude.com/docs/en/about-claude/pricing
// OpenAI: https://developers.openai.com/api/docs/pricing (Standard tier)

pub const CHECKED: &str = "2026-11-10";
pub const SOURCES: [&str; 2] = [
    "https://platform.claude.com/docs/en/about-claude/pricing",
    "https://developers.openai.com/api/docs/pricing",
];

#[derive(Clone, Copy)]
pub struct Rate {
    pub input: f64,
    pub cache_w5m: f64,
    pub cache_w1h: f64,
    pub cache_read: f64,
    pub output: f64,
}

#[derive(Clone, Copy)]
pub struct Price {
    pub id: &'static str,
    pub name: &'static str,
    pub base: Rate,
    /// Prompt size (tokens) above which `long` applies to the whole request.
    pub long_over: u64,
    pub long: Rate,
    /// Fast mode input/output; cache multipliers are applied on top.
    pub fast_in: f64,
    pub fast_out: f64,
    /// Cache read multiplier of base input (fast mode reuses it).
    pub read_mult: f64,
}

const Z: Rate = Rate { input: 0.0, cache_w5m: 0.0, cache_w1h: 0.0, cache_read: 0.0, output: 0.0 };

const fn claude(input: f64, read: f64, output: f64) -> Rate {
    Rate { input, cache_w5m: input * 1.25, cache_w1h: input * 2.0, cache_read: read, output }
}
const fn openai(input: f64, cached: f64, output: f64) -> Rate {
    Rate { input, cache_w5m: 0.0, cache_w1h: 0.0, cache_read: cached, output }
}
const fn c(id: &'static str, name: &'static str, input: f64, read: f64, output: f64) -> Price {
    Price { id, name, base: claude(input, read, output), long_over: 0, long: Z, fast_in: 0.0, fast_out: 0.0, read_mult: read / input }
}
const fn o(id: &'static str, name: &'static str, input: f64, cached: f64, output: f64) -> Price {
    Price { id, name, base: openai(input, cached, output), long_over: 0, long: Z, fast_in: 0.0, fast_out: 0.0, read_mult: 0.0 }
}
const fn ol(id: &'static str, name: &'static str, b: (f64, f64, f64), l: (f64, f64, f64)) -> Price {
    Price { id, name, base: openai(b.0, b.1, b.2), long_over: 272_000, long: openai(l.0, l.1, l.2), fast_in: 0.0, fast_out: 0.0, read_mult: 0.0 }
}
const fn fast(mut p: Price, i: f64, out: f64) -> Price {
    p.fast_in = i;
    p.fast_out = out;
    p
}

pub static PRICES: &[Price] = &[
    // Anthropic
    fast(c("claude-fable-5-2", "Claude Fable 5.2", 8.0, 0.20, 40.0), 16.0, 80.0),
    c("claude-fable-5-1", "Claude Fable 5.1", 10.0, 0.25, 50.0),
    c("claude-mythos-5-1", "Claude Mythos 5.1", 10.0, 0.25, 50.0),
    c("claude-fable-5", "Claude Fable 5", 10.0, 1.0, 50.0),
    c("claude-mythos-5", "Claude Mythos 5", 10.0, 1.0, 50.0),
    fast(c("claude-opus-5-5", "Claude Opus 5.5", 4.0, 0.20, 20.0), 8.0, 40.0),
    fast(c("claude-opus-5", "Claude Opus 5", 5.0, 0.50, 25.0), 10.0, 50.0),
    fast(c("claude-opus-4-8", "Claude Opus 4.8", 5.0, 0.50, 25.0), 10.0, 50.0),
    c("claude-opus-4-7", "Claude Opus 4.7", 5.0, 0.50, 25.0),
    c("claude-opus-4-6", "Claude Opus 4.6", 5.0, 0.50, 25.0),
    c("claude-opus-4-5", "Claude Opus 4.5", 5.0, 0.50, 25.0),
    c("claude-opus-4-1", "Claude Opus 4.1", 15.0, 1.50, 75.0),
    c("claude-opus-4", "Claude Opus 4", 15.0, 1.50, 75.0),
    c("claude-sonnet-5-5", "Claude Sonnet 5.5", 2.0, 0.10, 10.0),
    c("claude-sonnet-5", "Claude Sonnet 5", 2.0, 0.20, 10.0),
    c("claude-sonnet-4-6", "Claude Sonnet 4.6", 3.0, 0.30, 15.0),
    c("claude-sonnet-4-5", "Claude Sonnet 4.5", 3.0, 0.30, 15.0),
    c("claude-sonnet-4", "Claude Sonnet 4", 3.0, 0.30, 15.0),
    Price {
        id: "claude-haiku-5-5", name: "Claude Haiku 5.5",
        base: claude(0.10, 0.01, 0.50), long_over: 100_000, long: claude(0.50, 0.05, 2.50),
        fast_in: 0.0, fast_out: 0.0, read_mult: 0.1,
    },
    c("claude-haiku-4-5", "Claude Haiku 4.5", 1.0, 0.10, 5.0),
    c("claude-3-5-haiku", "Claude Haiku 3.5", 0.80, 0.08, 4.0),
    // OpenAI (Codex)
    ol("gpt-6-astra", "GPT-6 Astra", (10.0, 1.0, 50.0), (20.0, 2.0, 75.0)),
    ol("gpt-6.1-sol", "GPT-6.1 Sol", (2.0, 0.10, 10.0), (4.0, 0.20, 15.0)),
    ol("gpt-6.1-terra", "GPT-6.1 Terra", (1.0, 0.05, 5.0), (2.0, 0.10, 7.50)),
    ol("gpt-6-luna", "GPT-6 Luna", (0.10, 0.01, 0.50), (0.20, 0.02, 0.75)),
    ol("gpt-6-sol", "GPT-6 Sol", (2.0, 0.20, 10.0), (4.0, 0.40, 15.0)),
    ol("gpt-5.6-sol", "GPT-5.6 Sol", (4.0, 0.40, 20.0), (8.0, 0.80, 30.0)),
    ol("gpt-5.6-terra", "GPT-5.6 Terra", (2.0, 0.20, 12.0), (4.0, 0.40, 18.0)),
    ol("gpt-5.6-luna", "GPT-5.6 Luna", (0.20, 0.02, 1.20), (0.40, 0.04, 1.80)),
    ol("gpt-5.5", "GPT-5.5", (5.0, 0.50, 30.0), (10.0, 1.0, 45.0)),
    ol("gpt-5.5-pro", "GPT-5.5 Pro", (30.0, 30.0, 180.0), (60.0, 60.0, 270.0)),
    ol("gpt-5.4", "GPT-5.4", (2.50, 0.25, 15.0), (5.0, 0.50, 22.50)),
    ol("gpt-5.4-pro", "GPT-5.4 Pro", (30.0, 30.0, 180.0), (60.0, 60.0, 270.0)),
    o("gpt-5.4-mini", "GPT-5.4 mini", 0.75, 0.075, 4.50),
    o("gpt-5.4-nano", "GPT-5.4 nano", 0.20, 0.02, 1.25),
    o("gpt-5.3-codex", "GPT-5.3 Codex", 1.75, 0.175, 14.0),
    o("gpt-5.2", "GPT-5.2", 1.75, 0.175, 14.0),
    o("gpt-5.1", "GPT-5.1", 1.25, 0.125, 10.0),
    o("gpt-5", "GPT-5", 1.25, 0.125, 10.0),
    o("gpt-5-mini", "GPT-5 mini", 0.25, 0.025, 2.0),
    o("gpt-5-nano", "GPT-5 nano", 0.05, 0.005, 0.40),
    o("gpt-4.1", "GPT-4.1", 2.0, 0.50, 8.0),
    o("o3", "o3", 2.0, 0.50, 8.0),
    o("o4-mini", "o4-mini", 1.10, 0.275, 4.40),
];

/// Exact id match after dropping a date suffix (`-20251001`) or a `[1m]` tag.
pub fn lookup(model: &str) -> Option<&'static Price> {
    let mut m = model.trim();
    if let Some(i) = m.find('[') {
        m = &m[..i];
    }
    if let Some(i) = m.rfind('-') {
        let tail = &m[i + 1..];
        if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) {
            m = &m[..i];
        }
    }
    PRICES.iter().find(|p| p.id == m)
}

pub struct Use {
    pub input: u64,
    pub cache_w5m: u64,
    pub cache_w1h: u64,
    pub cache_read: u64,
    pub output: u64,
    pub fast: bool,
}

pub fn cost(p: &Price, u: &Use) -> f64 {
    let prompt = u.input + u.cache_w5m + u.cache_w1h + u.cache_read;
    let mut r = if p.long_over > 0 && prompt > p.long_over { p.long } else { p.base };
    if u.fast && p.fast_in > 0.0 {
        r = Rate {
            input: p.fast_in,
            cache_w5m: p.fast_in * 1.25,
            cache_w1h: p.fast_in * 2.0,
            cache_read: p.fast_in * p.read_mult,
            output: p.fast_out,
        };
    }
    (u.input as f64 * r.input
        + u.cache_w5m as f64 * r.cache_w5m
        + u.cache_w1h as f64 * r.cache_w1h
        + u.cache_read as f64 * r.cache_read
        + u.output as f64 * r.output)
        / 1_000_000.0
}
