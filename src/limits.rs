// Plan-limit tracking. Every few minutes a background thread asks each
// provider's own status endpoint how much of the current plan window is used,
// appends one JSON line per provider to limits.jsonl, and pushes the result to
// the page. Tokens are read from each tool's own credential store, refreshed
// when they are about to expire, and never shown to the page or written to the
// history file. A provider that is not installed or not signed in reports an
// "off"/"expired" sample and the chart keeps working without it.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tao::event_loop::EventLoopProxy;

use crate::{data_dir, esc, UserEvent};

/// Sample a provider every five minutes; the plan windows reset faster than
/// log file stats do, so a finer cadence would add no detail.
const POLL_SECS: u64 = 300;
/// Past this the file is rewritten keeping only the newest share of samples.
const LOG_CAP: u64 = 8 * 1024 * 1024;

/// One plan window: "5h" session, "day", "wk" or "mo" plan period.
#[derive(Clone)]
pub struct Win {
    pub k: &'static str,
    /// percent of the window used, 0-100
    pub pct: f64,
    /// unix time the window resets; 0 when the provider does not say
    pub reset: u64,
}

/// One provider reading. `st`: "ok" numbers fetched, "off" tool not installed
/// or not signed in, "expired" credentials there but refused, "error" anything
/// else went wrong (network, decryption, an unexpected reply).
pub struct Sample {
    pub src: u8,
    pub ts: u64,
    pub st: &'static str,
    pub msg: String,
    pub wins: Vec<Win>,
}

impl Sample {
    fn at(src: u8, ts: u64, st: &'static str, msg: impl Into<String>) -> Sample {
        Sample { src, ts, st, msg: msg.into(), wins: Vec::new() }
    }
}

/// The five tracked providers: harness source index -> sampler.
const TRACKED: [(u8, fn(u64) -> Sample); 5] = [
    (0, claude),
    (1, codex),
    (2, opencode_go),
    (4, droid),
    (6, devin),
];

/// The page labels limits by their plan product, not the log source.
pub const NAMES: [(&str, u8); 5] = [
    ("Claude Code", 0),
    ("Codex", 1),
    ("OpenCode Go", 2),
    ("Droid", 4),
    ("Devin", 6),
];

fn secs_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn log_path() -> PathBuf {
    #[cfg(test)]
    if let Ok(p) = std::env::var("JTOKENS_TEST_LIMITS_LOG") {
        return PathBuf::from(p);
    }
    data_dir().join("limits.jsonl")
}

fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(8)))
        .timeout_recv_response(Some(Duration::from_secs(12)))
        .user_agent(concat!("JTokens/", env!("CARGO_PKG_VERSION")))
        .http_status_as_error(false)
        .build()
        .into()
}

/// GET with headers; (status, body as text). Read once so error bodies are kept.
fn get(url: &str, heads: &[(&str, &str)]) -> Result<(u16, String), String> {
    let mut req = http().get(url);
    for (k, v) in heads {
        req = req.header(*k, *v);
    }
    let mut resp = req.call().map_err(|e| format!("could not reach {url}: {e}"))?;
    Ok((resp.status().as_u16(), read_body(&mut resp, url)?))
}

fn post_json(url: &str, heads: &[(&str, &str)], body: &Value) -> Result<(u16, String), String> {
    let mut req = http().post(url);
    for (k, v) in heads {
        req = req.header(*k, *v);
    }
    let mut resp = req.send_json(body).map_err(|e| format!("could not reach {url}: {e}"))?;
    Ok((resp.status().as_u16(), read_body(&mut resp, url)?))
}

fn post_form(url: &str, form: &[(&str, &str)]) -> Result<(u16, String), String> {
    let mut resp = http().post(url).send_form(form.iter().map(|(k, v)| (*k, *v))).map_err(|e| format!("could not reach {url}: {e}"))?;
    Ok((resp.status().as_u16(), read_body(&mut resp, url)?))
}

fn read_body(resp: &mut ureq::http::Response<ureq::Body>, url: &str) -> Result<String, String> {
    use std::io::Read;
    let mut r = resp.body_mut().with_config().limit(1024 * 1024).reader();
    let mut s = String::new();
    r.read_to_string(&mut s).map_err(|e| format!("could not read the reply from {url}: {e}"))?;
    Ok(s)
}

fn json_of(text: &str, url: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|_| format!("the reply from {url} was not JSON"))
}

/// Credentials a tool itself uses. Writing goes through a .bak backup first,
/// so a crash mid-write never strands the user signed out.
fn write_back(path: &PathBuf, v: &Value) {
    let _ = fs::copy(&*path, path.with_extension("jtokens-bak"));
    let _ = fs::write(path, serde_json::to_string_pretty(v).unwrap_or_default());
}

/// RFC3339 / ISO timestamps ("2026-11-24T02:54:20.580Z") to unix seconds.
fn rfc3339(s: &str) -> Option<u64> {
    let t = s.trim();
    if t.len() < 19 {
        return None;
    }
    let n = |a: usize, b: usize| t[a..b].parse::<u32>().ok();
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let (h, mi, se) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    // days since epoch: Howard Hinnant's civil-to-days
    let y = y as i64 - if mo <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((mo as i64 + 9) % 12) as u64;
    let doy = (153 * mp + 2) / 5 + (d - 1) as u64;
    let doe = yoe as u64 * 365 + yoe as u64 / 4 - yoe as u64 / 100 + doy;
    let mut secs = ((era * 146097 + doe as i64 - 719468) * 86400 + h as i64 * 3600 + mi as i64 * 60 + se as i64) as i64;
    // trailing Z or a numeric offset
    let tail = t[19..].trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
    if let Some(off) = tail.strip_prefix(['+', '-'].as_ref()) {
        if off.len() >= 2 {
            let oh = off[0..2].parse::<i64>().unwrap_or(0);
            let om = off.get(3..5).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
            let sign = if tail.starts_with('-') { -1 } else { 1 };
            secs -= sign * (oh * 3600 + om * 60);
        }
    }
    (secs > 0).then_some(secs as u64)
}

/// A JSON field that may be a number or a number in a string.
fn num(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
}

/// Percent values from providers: some send 0-1 fractions, most send 0-100.
fn pct(v: f64, fraction: bool) -> f64 {
    let p = if fraction && v <= 1.0 { v * 100.0 } else { v };
    p.clamp(0.0, 100.0)
}

// ---------------------------------------------------------------------------
// Claude Code: OAuth token in .credentials.json, usage from api.anthropic.com

fn claude(now: u64) -> Sample {
    let src = 0;
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| crate::home().join(".claude"));
    if !dir.is_dir() {
        return Sample::at(src, now, "off", "Claude Code is not installed");
    }
    let path = dir.join(".credentials.json");
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Sample::at(src, now, "off", "Not signed in to Claude Code"),
    };
    let mut cred: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Sample::at(src, now, "error", "The Claude credentials file is not valid JSON"),
    };
    let oa = cred.pointer("/claudeAiOauth").cloned().unwrap_or(Value::Null);
    let mut tok = oa["accessToken"].as_str().unwrap_or("").to_string();
    let rt = oa["refreshToken"].as_str().unwrap_or("").to_string();
    let exp_ms = oa["expiresAt"].as_u64().unwrap_or(0);
    if tok.is_empty() {
        return Sample::at(src, now, "off", "Not signed in to Claude Code");
    }
    if exp_ms > 0 && exp_ms <= now * 1000 + 120_000 && !rt.is_empty() {
        match claude_refresh(&rt) {
            Some((at, rt2, exp_in)) => {
                tok = at.clone();
                let exp = (now + exp_in.unwrap_or(3600)) * 1000;
                if let Some(o) = cred.pointer_mut("/claudeAiOauth") {
                    o["accessToken"] = Value::String(at);
                    if let Some(r) = rt2 {
                        o["refreshToken"] = Value::String(r);
                    }
                    o["expiresAt"] = Value::from(exp);
                }
                write_back(&path, &cred);
            }
            None => return Sample::at(src, now, "expired", "Claude Code sign-in expired; sign in again"),
        }
    }
    let bearer = format!("Bearer {tok}");
    let (code, body) = match get("https://api.anthropic.com/api/oauth/usage", &[("Authorization", &bearer), ("anthropic-beta", "oauth-2025-04-20")]) {
        Ok(r) => r,
        Err(e) => return Sample::at(src, now, "error", e),
    };
    if code == 401 || code == 403 {
        return Sample::at(src, now, "expired", "Claude Code sign-in expired; sign in again");
    }
    if code != 200 {
        return Sample::at(src, now, "error", format!("Anthropic answered {code}"));
    }
    let v = match json_of(&body, "anthropic.com") {
        Ok(v) => v,
        Err(e) => return Sample::at(src, now, "error", e),
    };
    let mut wins = Vec::new();
    for (key, name) in [("five_hour", "5h"), ("seven_day", "wk")] {
        let w = &v[key];
        let Some(u) = num(&w["utilization"]) else { continue };
        let reset = w["resets_at"].as_str().and_then(rfc3339).unwrap_or(0);
        wins.push(Win { k: name, pct: pct(u, false), reset });
    }
    if wins.is_empty() {
        return Sample::at(src, now, "error", "The usage reply had no plan windows");
    }
    let mut s = Sample::at(src, now, "ok", "");
    s.wins = wins;
    s
}

/// Claude Code refreshes its OAuth token on platform.claude.com (the public
/// client id is the one the CLI itself uses).
fn claude_refresh(rt: &str) -> Option<(String, Option<String>, Option<u64>)> {
    let body = serde_json::json!({"grant_type": "refresh_token", "refresh_token": rt, "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e"});
    for url in ["https://platform.claude.com/v1/oauth/token", "https://console.anthropic.com/v1/oauth/token"] {
        let Ok((code, text)) = post_json(url, &[], &body) else { continue };
        if code != 200 {
            return None; // a bad refresh token will be bad on every host; give up
        }
        let v: Value = serde_json::from_str(&text).ok()?;
        let at = v["access_token"].as_str()?.to_string();
        let rt2 = v["refresh_token"].as_str().map(String::from);
        let exp = num(&v["expires_in"]).map(|f| f as u64);
        return Some((at, rt2, exp));
    }
    None
}

// ---------------------------------------------------------------------------
// Codex: ChatGPT account tokens in auth.json, usage from backend-api wham

fn codex(now: u64) -> Sample {
    let src = 1;
    let dir = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| crate::home().join(".codex"));
    if !dir.is_dir() {
        return Sample::at(src, now, "off", "Codex is not installed");
    }
    let path = dir.join("auth.json");
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Sample::at(src, now, "off", "Not signed in to Codex"),
    };
    let mut cred: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Sample::at(src, now, "error", "The Codex auth file is not valid JSON"),
    };
    let mut tok = cred.pointer("/tokens/access_token").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let acct = cred.pointer("/tokens/account_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let rt = cred.pointer("/tokens/refresh_token").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if tok.is_empty() {
        return Sample::at(src, now, "off", "Not signed in to Codex");
    }
    let fetch = |tok: &str| -> Result<(u16, Value), String> {
        let bearer = format!("Bearer {tok}");
        let mut heads: Vec<(&str, &str)> = vec![("Authorization", &bearer)];
        if !acct.is_empty() {
            heads.push(("ChatGPT-Account-Id", &acct));
        }
        let (code, body) = get("https://chatgpt.com/backend-api/wham/usage", &heads)?;
        Ok((code, if code == 200 { json_of(&body, "chatgpt.com")? } else { Value::Null }))
    };
    let (code, mut v) = match fetch(&tok) {
        Ok(r) => r,
        Err(e) => return Sample::at(src, now, "error", e),
    };
    if code == 401 || code == 403 {
        if rt.is_empty() {
            return Sample::at(src, now, "expired", "Codex sign-in expired; sign in again");
        }
        let Some((at, rt2)) = codex_refresh(&rt) else {
            return Sample::at(src, now, "expired", "Codex sign-in expired; sign in again");
        };
        tok = at;
        if let Some(t) = cred.pointer_mut("/tokens") {
            t["access_token"] = Value::String(tok.clone());
            if let Some(r) = rt2 {
                t["refresh_token"] = Value::String(r);
            }
        }
        write_back(&path, &cred);
        match fetch(&tok) {
            Ok((c, v2)) if c == 200 => v = v2,
            Ok((c, _)) if c == 401 || c == 403 => return Sample::at(src, now, "expired", "Codex sign-in expired; sign in again"),
            Ok((c, _)) => return Sample::at(src, now, "error", format!("ChatGPT answered {c}")),
            Err(e) => return Sample::at(src, now, "error", e),
        }
    } else if code != 200 {
        return Sample::at(src, now, "error", format!("ChatGPT answered {code}"));
    }
    let mut wins = Vec::new();
    for (key, fallback) in [("primary_window", "5h"), ("secondary_window", "wk")] {
        let w = &v["rate_limit"][key];
        let Some(p) = num(&w["used_percent"]) else { continue };
        let span = num(&w["limit_window_seconds"]).unwrap_or(0.0) as u64;
        // the window's length names it when the fixed order does not
        let name = if span > 0 { win_name(span) } else { fallback };
        let reset = num(&w["reset_at"]).or_else(|| num(&w["reset_after_seconds"]).map(|s| now as f64 + s)).unwrap_or(0.0) as u64;
        wins.push(Win { k: name, pct: pct(p, false), reset });
    }
    if wins.is_empty() {
        return Sample::at(src, now, "error", "The usage reply had no plan windows");
    }
    let mut s = Sample::at(src, now, "ok", "");
    s.wins = wins;
    s
}

/// Window length in seconds to a chart key.
fn win_name(secs: u64) -> &'static str {
    if secs <= 6 * 3600 { "5h" } else if secs <= 26 * 3600 { "day" } else if secs <= 9 * 86400 { "wk" } else { "mo" }
}

/// Codex refreshes ChatGPT tokens on auth.openai.com with the CLI's client id.
fn codex_refresh(rt: &str) -> Option<(String, Option<String>)> {
    let body = serde_json::json!({"client_id": "app_EMoamEEZ73f0CkXaXp7hrann", "grant_type": "refresh_token", "refresh_token": rt});
    let (code, text) = post_json("https://auth.openai.com/oauth/token", &[], &body).ok()?;
    if code != 200 {
        return None;
    }
    let v: Value = serde_json::from_str(&text).ok()?;
    Some((v["access_token"].as_str()?.to_string(), v["refresh_token"].as_str().map(String::from)))
}

// ---------------------------------------------------------------------------
// OpenCode Go: API key in auth.json, usage from the Zen Go usage endpoint

fn opencode_go(now: u64) -> Sample {
    let src = 2;
    let dir = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| crate::home().join(".local").join("share")).join("opencode");
    if !dir.is_dir() {
        return Sample::at(src, now, "off", "OpenCode is not installed");
    }
    let path = dir.join("auth.json");
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Sample::at(src, now, "off", "No OpenCode Go key saved"),
    };
    let cred: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Sample::at(src, now, "error", "The OpenCode auth file is not valid JSON"),
    };
    // {"opencode-go":{"type":"api","key":"..."}} — also accept a plain string
    let key = ["opencode-go", "opencode_go", "opencode"]
        .iter()
        .find_map(|k| {
            let e = &cred[k];
            e.as_str().map(String::from).or_else(|| ["key", "api_key", "token", "apiKey", "access_token"].iter().find_map(|f| e[f].as_str().map(String::from)))
        })
        .unwrap_or_default();
    if key.is_empty() {
        return Sample::at(src, now, "off", "No OpenCode Go key saved");
    }
    let bearer = format!("Bearer {key}");
    let (code, body) = match get("https://opencode.ai/zen/go/v1/usage", &[("Authorization", &bearer)]) {
        Ok(r) => r,
        Err(e) => return Sample::at(src, now, "error", e),
    };
    if code == 401 || code == 403 {
        return Sample::at(src, now, "expired", "The OpenCode Go key was refused; check the key");
    }
    if code != 200 {
        return Sample::at(src, now, "error", format!("OpenCode answered {code}"));
    }
    let v = match json_of(&body, "opencode.ai") {
        Ok(v) => v,
        Err(e) => return Sample::at(src, now, "error", e),
    };
    // the reply shape has changed between versions, so the windows are found
    // by key name anywhere in the object
    let mut wins: Vec<Win> = Vec::new();
    oc_walk(&v, "", now, &mut wins);
    wins.sort_by_key(|w| match w.k { "5h" => 0, "day" => 1, "wk" => 2, _ => 3 });
    wins.dedup_by(|a, b| a.k == b.k);
    if wins.is_empty() {
        return Sample::at(src, now, "error", "The usage reply had no plan windows");
    }
    let mut s = Sample::at(src, now, "ok", "");
    s.wins = wins;
    s
}

/// Finds {"percent"|"usagePercent"|"usedPercent"|"utilization"} numbers under
/// keys that name a rolling/weekly/monthly window, at any depth.
fn oc_walk(v: &Value, path: &str, now: u64, out: &mut Vec<Win>) {
    let Some(obj) = v.as_object() else { return };
    for (k, val) in obj {
        let lk = k.to_ascii_lowercase();
        if let Some(o) = val.as_object() {
            let name = if lk.contains("rolling") || lk.contains("five") || lk.contains("5h") || lk.contains("hour") || lk.contains("primary") {
                Some("5h")
            } else if lk.contains("week") || lk.contains("secondary") {
                Some("wk")
            } else if lk.contains("month") || lk.contains("plan") || lk.contains("billing") {
                Some("mo")
            } else if lk.contains("day") || lk.contains("daily") {
                Some("day")
            } else {
                None
            };
            if let Some(wk) = name {
                for f in ["percent", "usagePercent", "usedPercent", "usage_percent", "used_percent", "utilization", "consumed", "pct"] {
                    if let Some(p) = o.get(f).and_then(num) {
                        // a Map indexes by get(); a missing key is not an error
                        let reset = ["resetAt", "reset_at", "resetsAt", "resets_at", "resetDate", "windowEnd"]
                            .iter()
                            .find_map(|f| o.get(*f).and_then(|v| v.as_str()))
                            .and_then(rfc3339)
                            .or_else(|| ["resetAt", "reset_at"].iter().find_map(|f| o.get(*f).and_then(num)).map(|v| v as u64))
                            .or_else(|| ["resetInSec", "reset_after_seconds", "reset_in_seconds"].iter().find_map(|f| o.get(*f).and_then(num)).map(|v| now + v as u64))
                            .unwrap_or(0);
                        out.push(Win { k: wk, pct: pct(p, f == "utilization"), reset });
                        break;
                    }
                }
            }
            let np = if path.is_empty() { lk.clone() } else { format!("{path}.{lk}") };
            oc_walk(val, &np, now, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Droid: encrypted keyring in ~/.factory, billing limits on api.factory.ai.
// The keyring AES key lives in Windows Credential Manager, which is how the
// Droid CLI itself protects it. A refreshed token is sealed back with the same
// key so the CLI keeps working.

/// Windows Credential Manager generic read.
fn credmgr_blob(name: &str) -> Option<Vec<u8>> {
    #[link(name = "advapi32")]
    extern "system" {
        fn CredReadW(target: *const u16, ty: u32, flags: u32, cred: *mut *const WCred) -> i32;
        fn CredFree(cred: *const WCred);
    }
    #[repr(C)]
    struct WCred {
        flags: u32,
        ty: u32,
        target: *const u16,
        comment: *const u16,
        last_written: u64,
        blob_size: u32,
        blob: *const u8,
        persist: u32,
        attr_count: u32,
        attrs: usize,
        alias: *const u16,
        username: *const u16,
    }
    const CRED_TYPE_GENERIC: u32 = 1;
    let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut cred: *const WCred = std::ptr::null();
        if CredReadW(w.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) == 0 || cred.is_null() {
            return None;
        }
        let c = &*cred;
        let out = if !c.blob.is_null() && c.blob_size > 0 { std::slice::from_raw_parts(c.blob, c.blob_size as usize).to_vec() } else { Vec::new() };
        CredFree(cred);
        (!out.is_empty()).then_some(out)
    }
}

// AES-GCM through Windows CNG (bcrypt.dll). The Droid keyring is encrypted
// with a 16 byte IV, which ring's fixed 96 bit nonces cannot open but CNG
// handles the same way Node's crypto does.
type BHandle = usize;

#[repr(C)]
struct Bcmi {
    cb_size: u32,
    version: u32,
    nonce: *const u8,
    nonce_len: u32,
    auth_data: *const u8,
    auth_data_len: u32,
    tag: *mut u8,
    tag_len: u32,
    mac_ctx: *mut u8,
    mac_ctx_len: u32,
    aad_len: u32,
    data_len: u64,
    flags: u32,
}

#[link(name = "bcrypt")]
extern "system" {
    fn BCryptOpenAlgorithmProvider(alg: *mut BHandle, id: *const u16, imp: *const u16, flags: u32) -> i32;
    fn BCryptSetProperty(obj: BHandle, prop: *const u16, val: *const u8, cb: u32, flags: u32) -> i32;
    fn BCryptGenerateSymmetricKey(alg: BHandle, key: *mut BHandle, obj: *mut u8, cb_obj: u32, secret: *const u8, cb_secret: u32, flags: u32) -> i32;
    fn BCryptEncrypt(key: BHandle, inp: *const u8, cb_in: u32, pad: *mut Bcmi, iv: *mut u8, cb_iv: u32, out: *mut u8, cb_out: u32, res: *mut u32, flags: u32) -> i32;
    fn BCryptDecrypt(key: BHandle, inp: *const u8, cb_in: u32, pad: *mut Bcmi, iv: *mut u8, cb_iv: u32, out: *mut u8, cb_out: u32, res: *mut u32, flags: u32) -> i32;
    fn BCryptDestroyKey(k: BHandle) -> i32;
    fn BCryptCloseAlgorithmProvider(a: BHandle, flags: u32) -> i32;
    fn BCryptGenRandom(alg: BHandle, buf: *mut u8, cb: u32, flags: u32) -> i32;
}

fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// An AES-256-GCM key handle: (provider, key); both freed by `gcm_free`.
fn gcm_key(key: &[u8]) -> Option<(BHandle, BHandle)> {
    let (aes, cm, gcm) = (wstr("AES"), wstr("ChainingMode"), wstr("ChainingModeGCM"));
    unsafe {
        let mut alg = 0usize;
        if BCryptOpenAlgorithmProvider(&mut alg, aes.as_ptr(), std::ptr::null(), 0) != 0 {
            return None;
        }
        let mut k = 0usize;
        if BCryptSetProperty(alg, cm.as_ptr(), gcm.as_ptr() as *const u8, (gcm.len() * 2) as u32, 0) != 0
            || BCryptGenerateSymmetricKey(alg, &mut k, std::ptr::null_mut(), 0, key.as_ptr(), key.len() as u32, 0) != 0
        {
            BCryptCloseAlgorithmProvider(alg, 0);
            return None;
        }
        Some((alg, k))
    }
}

fn gcm_free(h: (BHandle, BHandle)) {
    unsafe {
        BCryptDestroyKey(h.1);
        BCryptCloseAlgorithmProvider(h.0, 0);
    }
}

fn bcmi(nonce: &[u8], tag: *mut u8, tag_len: u32) -> Bcmi {
    Bcmi {
        cb_size: std::mem::size_of::<Bcmi>() as u32,
        version: 1,
        nonce: nonce.as_ptr(),
        nonce_len: nonce.len() as u32,
        auth_data: std::ptr::null(),
        auth_data_len: 0,
        tag,
        tag_len,
        mac_ctx: std::ptr::null_mut(),
        mac_ctx_len: 0,
        aad_len: 0,
        data_len: 0,
        flags: 0,
    }
}

fn gcm_open(key: &[u8], nonce: &[u8], ct: &[u8], tag: &[u8]) -> Result<Vec<u8>, ()> {
    let Some(h) = gcm_key(key) else { return Err(()) };
    let mut info = bcmi(nonce, tag.as_ptr() as *mut u8, tag.len() as u32);
    let mut out = vec![0u8; ct.len()];
    let mut got = 0u32;
    let rc = unsafe { BCryptDecrypt(h.1, ct.as_ptr(), ct.len() as u32, &mut info, std::ptr::null_mut(), 0, out.as_mut_ptr(), out.len() as u32, &mut got, 0) };
    gcm_free(h);
    if rc != 0 {
        return Err(());
    }
    out.truncate(got as usize);
    Ok(out)
}

fn gcm_seal(key: &[u8], nonce: &[u8], plain: &[u8]) -> Result<Vec<u8>, ()> {
    let Some(h) = gcm_key(key) else { return Err(()) };
    let mut tag = [0u8; 16];
    let mut info = bcmi(nonce, tag.as_mut_ptr(), tag.len() as u32);
    let mut out = vec![0u8; plain.len()];
    let mut got = 0u32;
    let rc = unsafe { BCryptEncrypt(h.1, plain.as_ptr(), plain.len() as u32, &mut info, std::ptr::null_mut(), 0, out.as_mut_ptr(), out.len() as u32, &mut got, 0) };
    gcm_free(h);
    if rc != 0 {
        return Err(());
    }
    out.truncate(got as usize);
    out.extend_from_slice(&tag);
    Ok(out) // ciphertext || tag
}

/// Fresh keyring nonce via the CNG RNG (BCRYPT_USE_SYSTEM_PREFERRED_RNG).
fn rand_nonce() -> Option<[u8; 16]> {
    let mut b = [0u8; 16];
    (unsafe { BCryptGenRandom(0, b.as_mut_ptr(), b.len() as u32, 2) } == 0).then_some(b)
}

fn b64e(b: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    STANDARD.encode(b)
}

fn b64d(s: &str) -> Option<Vec<u8>> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    STANDARD.decode(s.trim()).ok()
}

/// base64url decode (JWT payload segment).
fn b64url(s: &str) -> Option<Vec<u8>> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    URL_SAFE_NO_PAD.decode(s.as_bytes()).ok()
}

/// A JWT's exp claim, without verifying it (the provider's own check decides).
fn jwt_exp(tok: &str) -> Option<u64> {
    let mid = tok.split('.').nth(1)?;
    let v: Value = serde_json::from_slice(&b64url(mid)?).ok()?;
    num(&v["exp"]).map(|f| f as u64)
}

fn droid(now: u64) -> Sample {
    let src = 4;
    let dir = crate::home().join(".factory");
    if !dir.is_dir() {
        return Sample::at(src, now, "off", "Droid is not installed");
    }
    let path = dir.join("auth.v2.keyring");
    let ring_text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Sample::at(src, now, "off", "Not signed in to Droid"),
    };
    let Some(key_b64) = credmgr_blob("Factory CLI/auth-encryption-key") else {
        return Sample::at(src, now, "error", "The Droid key could not be read from Credential Manager");
    };
    let Some(key) = std::str::from_utf8(&key_b64).ok().and_then(b64d).filter(|k| k.len() == 32) else {
        return Sample::at(src, now, "error", "The Droid key in Credential Manager is not the expected shape");
    };
    let decrypt = |text: &str| -> Result<Value, String> {
        let mut parts = text.trim().split(':');
        let (iv, tag, ct) = (parts.next().and_then(b64d), parts.next().and_then(b64d), parts.next().and_then(b64d));
        let (Some(iv), Some(tag), Some(ct)) = (iv, tag, ct) else {
            return Err("The Droid keyring is not in the expected shape".into());
        };
        let plain = gcm_open(&key, &iv, &ct, &tag).map_err(|_| format!("The Droid keyring could not be decrypted (iv {} B, key {} B)", iv.len(), key.len()))?;
        serde_json::from_slice(&plain).map_err(|_| "The decrypted Droid keyring is not JSON".into())
    };
    let mut cred = match decrypt(&ring_text) {
        Ok(c) => c,
        Err(e) => return Sample::at(src, now, "error", e),
    };
    let mut tok = cred["access_token"].as_str().unwrap_or("").to_string();
    if tok.is_empty() {
        return Sample::at(src, now, "off", "Not signed in to Droid");
    }
    let exp = jwt_exp(&tok).unwrap_or(u64::MAX);
    if exp <= now + 120 {
        let rt = cred["refresh_token"].as_str().unwrap_or("").to_string();
        if rt.is_empty() {
            return Sample::at(src, now, "expired", "Droid sign-in expired; sign in again");
        }
        let (code, text) = match post_form(
            "https://api.workos.com/user_management/authenticate",
            &[("grant_type", "refresh_token"), ("client_id", "client_01HNM792M5G5G1A2THWPXKFMXB"), ("refresh_token", &rt)],
        ) {
            Ok(r) => r,
            Err(e) => return Sample::at(src, now, "error", e),
        };
        if code != 200 {
            return Sample::at(src, now, "expired", "Droid sign-in expired; sign in again");
        }
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => return Sample::at(src, now, "error", "The token refresh reply was not JSON"),
        };
        let Some(at) = v["access_token"].as_str() else {
            return Sample::at(src, now, "error", "The token refresh reply had no token");
        };
        tok = at.to_string();
        cred["access_token"] = Value::String(tok.clone());
        if let Some(r) = v["refresh_token"].as_str() {
            cred["refresh_token"] = Value::String(r.to_string());
        }
        // seal the updated keyring back so the CLI sees the new session
        if let Some(nonce) = rand_nonce() {
            if let Ok(sealed) = gcm_seal(&key, &nonce, serde_json::to_string(&cred).unwrap_or_default().as_bytes()) {
                let (ct, tag) = sealed.split_at(sealed.len() - 16);
                let body = format!("{}:{}:{}", b64e(&nonce), b64e(tag), b64e(ct));
                let _ = fs::copy(&path, path.with_extension("jtokens-bak"));
                let _ = fs::write(&path, body);
            }
        }
    }
    let bearer = format!("Bearer {tok}");
    let (code, body) = match get("https://api.factory.ai/api/billing/limits", &[("Authorization", &bearer)]) {
        Ok(r) => r,
        Err(e) => return Sample::at(src, now, "error", e),
    };
    if code == 401 || code == 403 {
        return Sample::at(src, now, "expired", "Droid sign-in expired; sign in again");
    }
    if code != 200 {
        return Sample::at(src, now, "error", format!("Factory answered {code}"));
    }
    let v = match json_of(&body, "factory.ai") {
        Ok(v) => v,
        Err(e) => return Sample::at(src, now, "error", e),
    };
    let mut wins = Vec::new();
    for tier in ["standard", "core"] {
        for (key, name) in [("fiveHour", "5h"), ("weekly", "wk"), ("monthly", "mo")] {
            let w = &v["limits"][tier][key];
            let Some(p) = num(&w["usedPercent"]) else { continue };
            let reset = w["windowEnd"].as_str().and_then(rfc3339).or_else(|| num(&w["secondsRemaining"]).map(|s| (now as f64 + s) as u64)).unwrap_or(0);
            wins.push(Win { k: name, pct: pct(p, false), reset });
        }
        if !wins.is_empty() {
            break; // standard answers first; core only when standard is absent
        }
    }
    if wins.is_empty() {
        return Sample::at(src, now, "error", "The billing reply had no plan windows");
    }
    let mut s = Sample::at(src, now, "ok", "");
    s.wins = wins;
    s
}

// ---------------------------------------------------------------------------
// Devin: the CLI's api key asks the seat-management service for plan quotas

fn devin(now: u64) -> Sample {
    let src = 6;
    let dir = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| crate::home().join("AppData").join("Roaming")).join("devin");
    if !dir.is_dir() {
        return Sample::at(src, now, "off", "Devin is not installed");
    }
    let path = dir.join("credentials.toml");
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Sample::at(src, now, "off", "Not signed in to Devin"),
    };
    // a TOML section header or the key itself; take the first api key found
    let mut key = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if matches!(k.trim(), "windsurf_api_key" | "api_key" | "apiKey") {
                key = v.trim().trim_matches('"').trim_matches('\'').to_string();
                if k.trim() == "windsurf_api_key" {
                    break; // this is the key the CLI uses
                }
            }
        }
    }
    if key.is_empty() {
        return Sample::at(src, now, "off", "Not signed in to Devin");
    }
    let meta = serde_json::json!({
        "metadata": {
            "apiKey": key, "ideName": "devin-cli", "ideVersion": "1.0.0",
            "extensionName": "devin-cli", "extensionVersion": "1.0.0",
            "locale": "en-US", "os": "windows", "requestId": now,
        }
    });
    let (code, body) = match post_json(
        "https://server.codeium.com/exa.seat_management_pb.SeatManagementService/GetUserStatus",
        &[("x-codeium-client", "devin-cli")],
        &meta,
    ) {
        Ok(r) => r,
        Err(e) => return Sample::at(src, now, "error", e),
    };
    if code == 401 || code == 403 {
        return Sample::at(src, now, "expired", "Devin sign-in expired; sign in again");
    }
    if code != 200 {
        return Sample::at(src, now, "error", format!("Devin answered {code}"));
    }
    let v = match json_of(&body, "devin") {
        Ok(v) => v,
        Err(e) => return Sample::at(src, now, "error", e),
    };
    let ps = &v["userStatus"]["planStatus"];
    let info = &ps["planInfo"];
    let mut wins = Vec::new();
    for (k, rem_key, reset_key, hidden) in [
        ("day", "dailyQuotaRemainingPercent", "dailyQuotaResetAtUnix", "hideDailyQuota"),
        ("wk", "weeklyQuotaRemainingPercent", "weeklyQuotaResetAtUnix", "hideWeeklyQuota"),
    ] {
        if info[hidden].as_bool().unwrap_or(false) {
            continue;
        }
        let Some(rem) = num(&ps[rem_key]) else { continue };
        let reset = num(&ps[reset_key]).unwrap_or(0.0) as u64;
        wins.push(Win { k, pct: pct(100.0 - rem, false), reset });
    }
    if wins.is_empty() {
        return Sample::at(src, now, "error", "The Devin status had no quota windows");
    }
    let mut s = Sample::at(src, now, "ok", "");
    if let Some(n) = info["planName"].as_str() {
        s.msg = format!("{n} plan");
    }
    s.wins = wins;
    s
}

// ---------------------------------------------------------------------------
// History: limits.jsonl keeps one line per provider per poll

fn line_of(s: &Sample) -> String {
    let mut j = String::with_capacity(160);
    let _ = write!(j, "{{\"src\":{},\"ts\":{},\"st\":\"{}\",\"w\":[", s.src, s.ts, s.st);
    for (i, w) in s.wins.iter().enumerate() {
        if i > 0 {
            j.push(',');
        }
        let _ = write!(j, "[\"{}\",{:.2},{}]", w.k, w.pct, w.reset);
    }
    j.push_str("],\"m\":");
    esc(&s.msg, &mut j);
    j.push_str("}\n");
    j
}

fn append(samples: &[Sample]) {
    let p = log_path();
    if let Some(d) = p.parent() {
        let _ = fs::create_dir_all(d);
    }
    let mut body = String::with_capacity(samples.len() * 160);
    for s in samples {
        body.push_str(&line_of(s));
    }
    let _ = fs::OpenOptions::new().create(true).append(true).open(&p).and_then(|mut f| std::io::Write::write_all(&mut f, body.as_bytes()));
    // the file is rewritten once it is too big, keeping only the newest lines
    if let Ok(md) = fs::metadata(&p) {
        if md.len() > LOG_CAP {
            if let Ok(s) = fs::read_to_string(&p) {
                let lines: Vec<&str> = s.lines().collect();
                let keep = &lines[lines.len() * 3 / 10..]; // newest 70%
                let _ = fs::write(&p, keep.join("\n") + "\n");
            }
        }
    }
}

struct Row {
    src: u8,
    ts: u64,
    st: String,
    msg: String,
    wins: Vec<(String, f64, u64)>,
}

fn read_all() -> Vec<Row> {
    let Ok(s) = fs::read_to_string(log_path()) else { return Vec::new() };
    let mut rows = Vec::new();
    for line in s.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let mut wins = Vec::new();
        if let Some(w) = v["w"].as_array() {
            for w in w {
                if let (Some(k), Some(p)) = (w[0].as_str(), num(&w[1])) {
                    wins.push((k.to_string(), p, num(&w[2]).unwrap_or(0.0) as u64));
                }
            }
        }
        rows.push(Row {
            src: num(&v["src"]).unwrap_or(0.0) as u8,
            ts: num(&v["ts"]).unwrap_or(0.0) as u64,
            st: v["st"].as_str().unwrap_or("").to_string(),
            msg: v["m"].as_str().unwrap_or("").to_string(),
            wins,
        });
    }
    rows
}

fn name_of(src: u8) -> &'static str {
    NAMES.iter().find(|(_, s)| *s == src).map(|(n, _)| *n).unwrap_or("")
}

/// The JSON the page gets: each provider's newest sample, plus each reported
/// window's points over the kept history.
fn payload(rows: &[Row]) -> String {
    let mut j = String::with_capacity(64 * 1024);
    j.push_str("{\"latest\":[");
    let mut first = true;
    for &(src, _) in TRACKED.iter() {
        // the newest sample wins; a provider never sampled still gets an entry
        let r = rows.iter().rev().find(|r| r.src == src);
        if !first {
            j.push(',');
        }
        first = false;
        let _ = write!(j, "{{\"src\":{src},\"n\":\"{}\",\"st\":", name_of(src));
        match r {
            Some(r) => {
                esc(&r.st, &mut j);
                let _ = write!(j, ",\"ts\":{},\"m\":", r.ts);
                esc(&r.msg, &mut j);
                j.push_str(",\"w\":[");
                for (i, (k, p, reset)) in r.wins.iter().enumerate() {
                    if i > 0 {
                        j.push(',');
                    }
                    let _ = write!(j, "[\"{k}\",{p:.2},{reset}]");
                }
                j.push(']');
            }
            None => {
                j.push_str("\"wait\",\"ts\":0,\"m\":\"\",\"w\":[]");
            }
        }
        j.push('}');
    }
    j.push_str("],\"series\":[");
    let mut first = true;
    for &(src, _) in TRACKED.iter() {
        // each window key this provider has ever reported, in stable order
        let mut keys: Vec<&str> = Vec::new();
        for r in rows.iter().filter(|r| r.src == src) {
            for (k, _, _) in &r.wins {
                if !keys.contains(&k.as_str()) {
                    keys.push(match k.as_str() { "5h" => "5h", "day" => "day", "wk" => "wk", "mo" => "mo", _ => continue });
                }
            }
        }
        for key in keys {
            if !first {
                j.push(',');
            }
            first = false;
            let _ = write!(j, "{{\"src\":{src},\"w\":\"{key}\",\"pts\":[");
            // keep the chart fast: at most ~2000 points, evenly sampled, newest kept
            let pts: Vec<(u64, f64)> = rows
                .iter()
                .filter(|r| r.src == src && r.st == "ok")
                .filter_map(|r| r.wins.iter().find(|(k, _, _)| k == key).map(|(_, p, _)| (r.ts, *p)))
                .collect();
            let step = (pts.len() / 2000 + 1) as usize;
            let mut i = 0;
            for (n, (ts, p)) in pts.iter().enumerate() {
                if n % step != 0 && n + 1 != pts.len() {
                    continue;
                }
                if i > 0 {
                    j.push(',');
                }
                i += 1;
                let _ = write!(j, "[{ts},{p:.1}]");
            }
            j.push_str("]}");
        }
    }
    j.push_str("]}");
    j
}

/// History JSON for tkLimits/emit_json, re-parsed only when the file changed.
pub fn json() -> String {
    static CACHE: Mutex<(u64, u64, String)> = Mutex::new((0, 0, String::new()));
    let (mtime, len) = fs::metadata(log_path()).map(|m| (m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0), m.len())).unwrap_or((0, 0));
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if c.0 != mtime || c.1 != len || c.2.is_empty() {
        c.2 = payload(&read_all());
        c.0 = mtime;
        c.1 = len;
    }
    c.2.clone()
}

/// `--limits` polls every provider once, appends the samples and prints the
/// payload, so the feature can be checked without opening the window.
pub fn cli() {
    let now = secs_now();
    let samples = poll(now);
    append(&samples);
    println!("{}", json());
}

/// Run all five samplers; failures become "error"/"off" samples, never panics.
pub fn poll(now: u64) -> Vec<Sample> {
    TRACKED.iter().map(|(src, f)| std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(now))).unwrap_or_else(|_| Sample::at(*src, now, "error", "The check failed"))).collect()
}

/// The background thread: poll, append, push to the page, sleep, repeat.
pub fn watch(proxy: EventLoopProxy<UserEvent>) {
    std::thread::spawn(move || loop {
        let now = secs_now();
        let samples = poll(now);
        append(&samples);
        // the payload is rebuilt from the file so restarts draw the same chart
        let _ = proxy.send_event(UserEvent::Limits(json()));
        std::thread::sleep(Duration::from_secs(POLL_SECS));
    });
}
