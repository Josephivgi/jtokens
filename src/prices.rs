// API list prices in USD per million tokens. Each row is from the provider's own pricing page
// (see SOURCES); rows marked "aggregator" are not on the provider's page and should be re-checked.

pub const CHECKED: &str = "2026-11-21";
pub const SOURCES: &[&str] = &[
    "https://platform.claude.com/docs/en/about-claude/pricing",
    "https://developers.openai.com/api/docs/pricing",
    "https://api-docs.deepseek.com/quick_start/pricing",
    "https://platform.kimi.ai/docs/pricing/chat",
    "https://docs.x.ai/developers/models",
    "https://docs.z.ai/guides/overview/pricing",
    "https://platform.minimax.io/docs/guides/pricing-paygo",
    "https://docs.devinenterprise.com/desktop/models",
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
    /// Off-peak rate, used outside `deepseek_peak_hour` (DeepSeek only). `base` is the peak rate.
    pub off_peak: Option<Rate>,
}

const Z: Rate = Rate { input: 0.0, cache_w5m: 0.0, cache_w1h: 0.0, cache_read: 0.0, output: 0.0 };

const fn claude(input: f64, read: f64, output: f64) -> Rate {
    Rate { input, cache_w5m: input * 1.25, cache_w1h: input * 2.0, cache_read: read, output }
}
const fn openai(input: f64, cached: f64, output: f64) -> Rate {
    Rate { input, cache_w5m: 0.0, cache_w1h: 0.0, cache_read: cached, output }
}
/// A provider with its own cache write prices (Kimi K3: 5 minute writes at the input price, 1 hour at 2x).
const fn written(input: f64, w5m: f64, w1h: f64, read: f64, output: f64) -> Rate {
    Rate { input, cache_w5m: w5m, cache_w1h: w1h, cache_read: read, output }
}
const fn c(id: &'static str, name: &'static str, input: f64, read: f64, output: f64) -> Price {
    Price { id, name, base: claude(input, read, output), long_over: 0, long: Z, fast_in: 0.0, fast_out: 0.0, read_mult: read / input, off_peak: None }
}
const fn o(id: &'static str, name: &'static str, input: f64, cached: f64, output: f64) -> Price {
    Price { id, name, base: openai(input, cached, output), long_over: 0, long: Z, fast_in: 0.0, fast_out: 0.0, read_mult: 0.0, off_peak: None }
}
/// OpenAI-style row with a long-context tier above 272k prompt tokens.
const fn ol(id: &'static str, name: &'static str, b: (f64, f64, f64), l: (f64, f64, f64)) -> Price {
    olt(id, name, b, 272_000, l)
}
/// OpenAI-style row with a long-context tier above `over` prompt tokens (`over` is the last size still at base rate).
const fn olt(id: &'static str, name: &'static str, b: (f64, f64, f64), over: u64, l: (f64, f64, f64)) -> Price {
    Price { id, name, base: openai(b.0, b.1, b.2), long_over: over, long: openai(l.0, l.1, l.2), fast_in: 0.0, fast_out: 0.0, read_mult: 0.0, off_peak: None }
}
/// Row with its own cache write prices (no long tier).
const fn wr(id: &'static str, name: &'static str, base: Rate) -> Price {
    Price { id, name, base, long_over: 0, long: Z, fast_in: 0.0, fast_out: 0.0, read_mult: 0.0, off_peak: None }
}
const fn fast(mut p: Price, i: f64, out: f64) -> Price {
    p.fast_in = i;
    p.fast_out = out;
    p
}
/// DeepSeek: peak rate is the base; off-peak is the cheaper rate outside its peak hours.
const fn dsk(id: &'static str, name: &'static str, peak: (f64, f64, f64), off: (f64, f64, f64)) -> Price {
    Price { id, name, base: openai(peak.0, peak.1, peak.2), long_over: 0, long: Z, fast_in: 0.0, fast_out: 0.0, read_mult: 0.0, off_peak: Some(openai(off.0, off.1, off.2)) }
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
        fast_in: 0.0, fast_out: 0.0, read_mult: 0.1, off_peak: None,
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
    // DeepSeek (api-docs.deepseek.com). Peak rates are the base; off-peak is half, see deepseek_peak_hour.
    // The API serves deepseek-flash as DeepSeek-V4.1-Flash-1017; the older names map to it.
    dsk("deepseek-v4.1-flash-1017", "DeepSeek V4.1 Flash", (0.30, 0.006, 1.20), (0.15, 0.003, 0.60)),
    dsk("deepseek-v4.1-flash", "DeepSeek V4.1 Flash", (0.30, 0.006, 1.20), (0.15, 0.003, 0.60)),
    dsk("deepseek-flash", "DeepSeek V4.1 Flash", (0.30, 0.006, 1.20), (0.15, 0.003, 0.60)),
    dsk("deepseek-v4-flash", "DeepSeek V4.1 Flash", (0.30, 0.006, 1.20), (0.15, 0.003, 0.60)),
    dsk("deepseek-v4-pro-0813", "DeepSeek V4 Pro", (1.32, 0.044, 3.96), (0.66, 0.022, 1.98)),
    dsk("deepseek-v4-pro", "DeepSeek V4 Pro", (1.32, 0.044, 3.96), (0.66, 0.022, 1.98)),
    // Moonshot Kimi (platform.kimi.ai). Cache writes: 5 minute at input price, 1 hour at 2x (K3 series).
    wr("kimi-k3.1-pro", "Kimi K3.1 Pro", written(2.0, 2.0, 4.0, 0.20, 10.0)),
    wr("kimi-k3.1-pro-highspeed", "Kimi K3.1 Pro highspeed", written(4.0, 4.0, 8.0, 0.20, 20.0)),
    wr("kimi-k3", "Kimi K3", written(3.0, 3.0, 6.0, 0.30, 15.0)),
    // K2 has no cache write price on the page, so its writes are billed as input here.
    wr("kimi-k2.7-code","Kimi K2.7 Code", written(0.95, 0.95, 0.95, 0.19, 4.0)),
    wr("kimi-k2.7-code-highspeed", "Kimi K2.7 Code highspeed", written(1.90, 1.90, 1.90, 0.38, 8.0)),
    wr("kimi-k2.6", "Kimi K2.6", written(0.95, 0.95, 0.95, 0.16, 4.0)),
    // xAI Grok (docs.x.ai). Above 200k prompt tokens the long rate applies to the whole request.
    olt("grok-4.7", "Grok 4.7", (2.0, 0.50, 6.0), 199_999, (4.0, 1.0, 12.0)),
    olt("grok-4.6", "Grok 4.6", (2.0, 0.50, 6.0), 199_999, (4.0, 1.0, 12.0)),
    olt("grok-4.5", "Grok 4.5", (2.0, 0.30, 6.0), 199_999, (4.0, 0.60, 12.0)),
    olt("grok-4.3", "Grok 4.3", (1.25, 0.20, 2.50), 199_999, (2.50, 0.40, 5.00)),
    olt("grok-build-0.1", "Grok Build 0.1", (1.0, 0.20, 2.0), 199_999, (2.0, 0.40, 4.0)),
    // Z.ai GLM (docs.z.ai). Free models (GLM-4.7-Flash, GLM-4.5-Flash, GLM-4.6V-Flash) have no row, so they cost $0.
    o("glm-5.3", "GLM-5.3", 1.40, 0.26, 4.40),
    o("glm-5.3-flash", "GLM-5.3 Flash", 0.15, 0.03, 0.50),
    o("glm-5.3-flashx", "GLM-5.3 FlashX", 0.37, 0.075, 1.25),
    o("glm-5.2", "GLM-5.2", 1.40, 0.26, 4.40),
    o("glm-5.1", "GLM-5.1", 1.40, 0.26, 4.40),
    o("glm-5", "GLM-5", 1.00, 0.20, 3.20),
    o("glm-4.7", "GLM-4.7", 0.60, 0.11, 2.20),
    o("glm-4.7-flashx", "GLM-4.7 FlashX", 0.07, 0.01, 0.40),
    o("glm-4.6", "GLM-4.6", 0.60, 0.11, 2.20),
    o("glm-4.6v", "GLM-4.6V", 0.30, 0.05, 0.90),
    o("glm-4.5", "GLM-4.5", 0.60, 0.11, 2.20),
    o("glm-4.5-x", "GLM-4.5-X", 2.20, 0.45, 8.90),
    o("glm-4.5-air", "GLM-4.5-Air", 0.20, 0.03, 1.10),
    o("glm-4.5-airx", "GLM-4.5-AirX", 1.10, 0.22, 4.50),
    o("glm-4.5v", "GLM-4.5V", 0.60, 0.11, 1.80),
    // MiniMax (platform.minimax.io). Above 512k input tokens the long rate applies.
    olt("minimax-m3", "MiniMax M3", (0.30, 0.06, 1.20), 512_000, (0.60, 0.12, 2.40)),
    o("minimax-m3.1-flash", "MiniMax M3.1 Flash", 0.15, 0.015, 0.60),
    // Cognition SWE (docs.devinenterprise.com/desktop/models). List prices; promos
    // (SWE-2 free to 2026-11-30, Flash free to 2026-12-04 on self-serve) are not
    // modelled. SWE-2 lists no separate cache write rate, so writes are billed at
    // the input price. Lightning uses the app's fast-mode 2x convention; the page
    // only names a fast rate for Claude and GPT.
    fast(
        Price { id: "swe-2", name: "SWE-2", base: written(3.0, 3.0, 3.0, 0.30, 15.0), long_over: 0, long: Z, fast_in: 0.0, fast_out: 0.0, read_mult: 0.1, off_peak: None },
        6.0,
        30.0,
    ),
    fast(
        Price { id: "swe-2-flash", name: "SWE-2 Flash", base: written(0.10, 0.125, 0.125, 0.01, 0.50), long_over: 0, long: Z, fast_in: 0.0, fast_out: 0.0, read_mult: 0.1, off_peak: None },
        0.20,
        1.0,
    ),
];

/// Words Devin adds after the vendor's model id: the effort (low/medium/high/max) and speed
/// (fast/lightning) the session ran at. They are not part of the billed model, so when a bare
/// match fails they are peeled off one at a time. Real models ending in these words are still
/// found first, so they keep their own row.
const PEEL_SUFFIXES: [&str; 6] = ["low", "medium", "high", "max", "fast", "lightning"];

/// Ids compare in canonical form: lowercase, a dash between digits read as a dot,
/// so `gpt-6-1-sol` and `gpt-6.1-sol` name the same row.
fn canon_id(m: &str) -> String {
    let b = m.as_bytes();
    let mut s = String::with_capacity(b.len());
    for (i, &c) in b.iter().enumerate() {
        s.push(if c == b'-' && i > 0 && i + 1 < b.len() && b[i - 1].is_ascii_digit() && b[i + 1].is_ascii_digit() { '.' } else { c.to_ascii_lowercase() as char });
    }
    s
}

/// Id match, ignoring case, after dropping a `provider/` prefix, a date suffix (`-20251001`) or a `[1m]` tag.
pub fn lookup(model: &str) -> Option<&'static Price> {
    let mut m = model.trim();
    if let Some(i) = m.find('[') {
        m = &m[..i];
    }
    if let Some(i) = m.rfind('/') {
        m = &m[i + 1..];
    }
    if let Some(i) = m.rfind('-') {
        let tail = &m[i + 1..];
        if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) {
            m = &m[..i];
        }
    }
    let find = |m: &str| {
        let m = canon_id(m);
        PRICES.iter().find(|p| canon_id(p.id) == m)
    };
    if let Some(p) = find(m) {
        return Some(p);
    }
    let mut base = m;
    for _ in 0..4 {
        let Some(i) = base.rfind('-') else { break };
        if !PEEL_SUFFIXES.contains(&base[i + 1..].to_ascii_lowercase().as_str()) {
            break;
        }
        base = &base[..i];
        if let Some(p) = find(base) {
            return Some(p);
        }
    }
    None
}

pub struct Use {
    pub input: u64,
    pub cache_w5m: u64,
    pub cache_w1h: u64,
    pub cache_read: u64,
    pub output: u64,
    pub fast: bool,
    /// Unix seconds of the request (any second in the hour is fine for DeepSeek's peak check).
    pub ts: u64,
}

/// DeepSeek's peak hours: 01:00-04:00 and 06:00-10:00 UTC, Monday to Friday. Other hours are off-peak.
/// Chinese public holidays are not known here, so a holiday day is priced as a weekday.
pub fn deepseek_peak_hour(ts: u64) -> bool {
    let weekday = (ts / 86_400 + 4) % 7; // 0 = Sunday; 1970-01-01 was a Thursday
    let hour = (ts % 86_400) / 3_600;
    (1..=5).contains(&weekday) && ((1..4).contains(&hour) || (6..10).contains(&hour))
}

pub fn cost(p: &Price, u: &Use) -> f64 {
    let prompt = u.input + u.cache_w5m + u.cache_w1h + u.cache_read;
    let mut r = if p.long_over > 0 && prompt > p.long_over { p.long } else { p.base };
    if let Some(off) = p.off_peak {
        if !deepseek_peak_hour(u.ts) {
            r = off;
        }
    }
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
