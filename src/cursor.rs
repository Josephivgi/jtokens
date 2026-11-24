//! Cursor's usage history is not on disk: it is read from the account API, the
//! same `aiserver.v1.DashboardService` calls T3 Code's usage page makes. The
//! API key comes from the CURSOR_API_KEY environment variable or the sign-in
//! T3 Code stores in ~/.t3/**/userdata/secrets/provider-auth-*.bin; reading the
//! same files keeps both apps on one sign-in.
//!
//! Cursor reports the billed amount on every event (`totalCents`), so its rows
//! carry a real cost, not a token-times-list-price estimate. Fetched events are
//! kept in cursor-usage.json under the data dir: events Cursor later forgets
//! stay counted, and each scan only asks for the new tail.

use crate::{data_dir, home, intern, key, Event, FileData, Src};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

const ENDPOINT_DEFAULT: &str = "https://api2.cursor.sh";
const PAGE_SIZE: u32 = 1000;
/// Account reads are skipped while the last one is this fresh.
const TTL_MS: i64 = 60_000;
/// A rescan re-reads this far back: Cursor can finalize events after first read.
const OVERLAP_MS: i64 = 3_600_000;

/// One billed event from GetFilteredUsageEvents.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct Rec {
    ts: i64,                    // milliseconds
    m: String,                  // model as Cursor names it
    s: String,                  // conversationId
    i: u64,
    o: u64,
    cr: u64,
    cw: u64,
    c: Option<f64>,             // billed USD (totalCents / 100); None prices by table
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Acct {
    #[serde(default)]
    until_ms: i64,
    #[serde(default)]
    fetched_ms: i64,
    #[serde(default)]
    records: Vec<Rec>,
}

/// State kept between scans: the event history per Cursor account (hashed
/// subject), rebuilt FileData, and per-credential backoff.
#[derive(Default)]
pub(crate) struct CursorScan {
    loaded: bool,
    accounts: BTreeMap<String, Acct>,
    files: Vec<Arc<FileData>>,
    dirty: bool,
    /// api key hash -> when it last failed; a failing key is not retried for a minute
    failed: HashMap<u64, Instant>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(8)))
        .timeout_recv_response(Some(Duration::from_secs(15)))
        .user_agent(concat!("JTokens/", env!("CARGO_PKG_VERSION")))
        .http_status_as_error(false)
        .max_redirects(0) // a bearer token is never forwarded through a redirect
        .https_only(true)
        .build()
        .into()
}

fn post(url: &str, bearer: &str, body: &Value) -> Result<(u16, Value), String> {
    let mut resp = http()
        .post(url)
        .header("Authorization", &format!("Bearer {bearer}"))
        .header("connect-protocol-version", "1")
        .send_json(body)
        .map_err(|e| format!("could not reach {url}: {e}"))?;
    let code = resp.status().as_u16();
    use std::io::Read;
    let mut text = String::new();
    resp.body_mut()
        .with_config()
        .limit(32 * 1024 * 1024)
        .reader()
        .read_to_string(&mut text)
        .map_err(|e| format!("could not read the reply from {url}: {e}"))?;
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok((code, json))
}

/// Hash helper for credential fingerprints and event ids (JTokens has no hex crate).
fn sha_hex(s: &str) -> String {
    use sha2::Digest;
    let h = sha2::Sha256::digest(s.as_bytes());
    h.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Credentials

struct Cred {
    api_key: String,
    backend: String,
}

/// Cursor API keys the user is signed in with: the CURSOR_API_KEY environment
/// variable first (the name the Cursor agent CLI uses), then the sign-ins T3
/// Code keeps under ~/.t3 — secrets/provider-auth-*.bin files holding
/// {"apiKey","backendUrl"} JSON for every linked provider instance.
fn credentials() -> Vec<Cred> {
    let mut out = Vec::new();
    if let Ok(k) = std::env::var("CURSOR_API_KEY") {
        let k = k.trim().to_string();
        if !k.is_empty() {
            out.push(Cred { api_key: k, backend: ENDPOINT_DEFAULT.into() });
        }
    }
    let t3 = home().join(".t3");
    let mut roots: Vec<PathBuf> = vec![t3.join("userdata")];
    if let Ok(rd) = fs::read_dir(&t3) {
        for e in rd.flatten() {
            let ud = e.path().join("userdata");
            if e.path().is_dir() && ud.is_dir() {
                roots.push(ud);
            }
        }
    }
    for ud in roots {
        let secrets = ud.join("secrets");
        let Ok(rd) = fs::read_dir(&secrets) else { continue };
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(n) = name.to_str() else { continue };
            if !n.starts_with("provider-auth-") || !n.ends_with(".bin") {
                continue;
            }
            let Ok(text) = fs::read_to_string(e.path()) else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
            let Some(api_key) = v.get("apiKey").and_then(|k| k.as_str()) else { continue };
            let backend = v.get("backendUrl").and_then(|u| u.as_str()).unwrap_or(ENDPOINT_DEFAULT);
            if !(api_key.starts_with("crsr_") || backend.contains("cursor.sh")) {
                continue;
            }
            // T3 stores an optional expiry; a dead key only wastes a round trip
            if v.get("apiKeyExpiresAtMs").and_then(|x| x.as_i64()).is_some_and(|e| e <= now_ms()) {
                continue;
            }
            let cred = Cred { api_key: api_key.to_string(), backend: backend.trim_end_matches('/').to_string() };
            if !out.iter().any(|c| c.api_key == cred.api_key) {
                out.push(cred);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Account API (aiserver.v1.DashboardService)

/// api key -> short-lived access token, as @cursor/sdk does.
fn exchange(c: &Cred) -> Result<String, String> {
    let (code, v) = post(&format!("{}/auth/exchange_user_api_key", c.backend), &c.api_key, &Value::Object(Default::default()))?;
    if code == 401 || code == 403 {
        return Err("Cursor sign-in expired; sign in again".into());
    }
    if !(200..300).contains(&code) {
        return Err(format!("Cursor answered {code}"));
    }
    v.get("accessToken").and_then(|t| t.as_str()).map(str::to_string).ok_or_else(|| "Cursor answered with no access token".into())
}

/// The JWT payload's subject: the stable account id. Two keys to one account
/// share it, so their events merge instead of counting twice.
fn jwt_sub(token: &str) -> Option<String> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.get("sub").and_then(|s| s.as_str()).map(str::to_string)
}

fn membership(backend: &str, token: &str, method: &str) -> Result<Value, String> {
    let (code, v) = post(&format!("{backend}/aiserver.v1.DashboardService/{method}"), token, &Value::Object(Default::default()))?;
    if !(200..300).contains(&code) {
        return Err(format!("Cursor {method} answered {code}"));
    }
    if v.get("error").is_some() {
        return Err(format!("Cursor {method} returned an error"));
    }
    Ok(v)
}

/// A JSON value with object keys sorted, so equal events compare byte-equal.
fn canonical(v: &Value) -> String {
    match v {
        Value::Array(a) => format!("[{}]", a.iter().map(canonical).collect::<Vec<_>>().join(",")),
        Value::Object(m) => {
            let mut pairs: Vec<_> = m.iter().collect();
            pairs.sort_by(|(a, _), (b, _)| a.cmp(b));
            format!("{{{}}}", pairs.iter().map(|(k, x)| format!("{}:{}", serde_json::to_string(k).unwrap_or_default(), canonical(x))).collect::<Vec<_>>().join(","))
        }
        _ => serde_json::to_string(v).unwrap_or_else(|_| "null".into()),
    }
}

/// How much of `prev`'s tail `cur`'s head repeats (page overlap), as a KMP match
/// of the concatenated sequences. Ported from T3's accountUsage.ts.
fn boundary_overlap(prev: &[String], cur: &[String]) -> usize {
    let mut seq = Vec::with_capacity(prev.len() + cur.len() + 1);
    seq.extend(cur.iter().cloned());
    seq.push(String::new());
    seq.extend(prev.iter().cloned());
    let mut l = vec![0usize; seq.len()];
    for i in 1..seq.len() {
        let mut len = l[i - 1];
        while len > 0 && seq[i] != seq[len] {
            len = l[len - 1];
        }
        if seq[i] == seq[len] {
            len += 1;
        }
        l[i] = len;
    }
    l.last().copied().unwrap_or(0).min(cur.len())
}

/// One scope (personal = None, or a team id), paged until a short page.
/// Identical events have no id; overlapping page boundaries are dropped the way
/// T3 does it, against the server's own totalUsageEventsCount.
fn read_scope(backend: &str, token: &str, user_id: u64, team_id: Option<u64>, since_ms: i64, until_ms: i64) -> Result<Vec<Value>, String> {
    let url = format!("{backend}/aiserver.v1.DashboardService/GetFilteredUsageEvents");
    let mut events: Vec<Value> = Vec::new();
    let mut prev: Vec<String> = Vec::new();
    let mut total: Option<i64> = None;
    for page in 1..=10_000u32 {
        let mut body = serde_json::json!({
            "page": page,
            "pageSize": PAGE_SIZE,
            "startDate": since_ms.to_string(),
            "endDate": until_ms.to_string(),
        });
        if let Some(t) = team_id {
            body["teamId"] = Value::from(t);
            body["userId"] = Value::from(user_id);
        }
        let (code, v) = post(&url, token, &body)?;
        if code == 401 || code == 403 {
            return Err("Cursor sign-in expired; sign in again".into());
        }
        if !(200..300).contains(&code) {
            return Err(format!("Cursor usage answered {code}"));
        }
        let obj = v.as_object().cloned().unwrap_or_default();
        if obj.is_empty() {
            break;
        }
        if let Some(c) = obj.get("totalUsageEventsCount").and_then(|c| c.as_i64()) {
            match total {
                None => total = Some(c),
                Some(t) if t != c => return Err("Cursor usage page count changed mid-read".into()),
                _ => {}
            }
        }
        let page_events: Vec<Value> = obj.get("usageEventsDisplay").and_then(|e| e.as_array()).cloned().unwrap_or_default();
        if page_events.len() > PAGE_SIZE as usize {
            return Err("Cursor usage page was oversized".into());
        }
        // drop the events this page repeats from the previous one's tail
        let cur: Vec<String> = page_events.iter().map(canonical).collect();
        let may_overlap = page_events
            .first()
            .and_then(|e| e.get("timestamp"))
            .map(|t| prev.iter().any(|p| p.contains(&canonical(t))))
            .unwrap_or(true);
        let overlap = if may_overlap && !prev.is_empty() { boundary_overlap(&prev, &cur) } else { 0 };
        let n = page_events.len();
        events.extend(page_events.into_iter().skip(overlap));
        prev = cur;
        if n < PAGE_SIZE as usize {
            break;
        }
        if let Some(t) = total {
            if events.len() as i64 >= t {
                break;
            }
        }
    }
    if let Some(t) = total {
        if (events.len() as i64) < t {
            return Err("Cursor usage pages were incomplete".into());
        }
    }
    Ok(events)
}

fn num(v: Option<&Value>) -> u64 {
    v.and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.trim().parse::<f64>().ok())))
        .filter(|f| f.is_finite())
        .map_or(0, |f| f.clamp(0.0, 1e15) as u64)
}

fn fetch_events(c: &Cred, st: &mut CursorScan, now: i64) -> Result<(), String> {
    let token = exchange(c)?;
    let sub = jwt_sub(&token).ok_or_else(|| "Cursor access token has no subject".to_string())?;
    let acct_key = sha_hex(&sub)[..16].to_string();
    {
        let a = st.accounts.get(&acct_key);
        if a.is_some_and(|a| now - a.fetched_ms < TTL_MS) {
            return Ok(()); // another key to this account already refreshed it
        }
    }
    let me = membership(&c.backend, &token, "GetMe")?;
    let user_id = me.get("userId").and_then(|u| u.as_u64()).ok_or_else(|| "Cursor GetMe returned no user".to_string())?;
    let teams = membership(&c.backend, &token, "GetTeams")?;
    let mut scopes: Vec<Option<u64>> = vec![None];
    if let Some(list) = teams.get("teams").and_then(|t| t.as_array()) {
        for t in list {
            if let Some(id) = t.get("id").and_then(|id| id.as_u64()) {
                if !scopes.contains(&Some(id)) {
                    scopes.push(Some(id));
                }
            }
        }
    }
    let acct = st.accounts.entry(acct_key.clone()).or_default();
    let since = if acct.until_ms - OVERLAP_MS > 0 { acct.until_ms - OVERLAP_MS } else { 0 };
    let mut fresh: Vec<Rec> = Vec::new();
    for team_id in scopes {
        for ev in read_scope(&c.backend, &token, user_id, team_id, since, now)? {
            let usage = match ev.get("tokenUsage") {
                Some(u) => u,
                None => continue, // non-token dashboard rows (plan changes, etc.)
            };
            let ts = ev
                .get("timestamp")
                .and_then(|t| t.as_str().and_then(|s| s.trim().parse::<i64>().ok()).or_else(|| t.as_i64()))
                .filter(|t| *t > 0);
            let Some(ts) = ts else { continue };
            if ts < since || ts > now {
                continue;
            }
            let Some(model) = ev.get("model").and_then(|m| m.as_str()).filter(|m| !m.is_empty()) else { continue };
            let session = ev.get("conversationId").and_then(|s| s.as_str()).unwrap_or_default();
            let cents = usage.get("totalCents").and_then(|c| c.as_f64()).filter(|c| c.is_finite() && *c >= 0.0);
            fresh.push(Rec {
                ts,
                m: model.to_string(),
                s: session.to_string(),
                i: num(usage.get("inputTokens")),
                o: num(usage.get("outputTokens")),
                cr: num(usage.get("cacheReadTokens")),
                cw: num(usage.get("cacheWriteTokens")),
                c: cents.map(|c| c / 100.0),
            });
        }
    }
    acct.records.retain(|r| r.ts < since || r.ts > now);
    acct.records.extend(fresh);
    acct.until_ms = now;
    acct.fetched_ms = now;
    st.dirty = true;
    Ok(())
}

// ---------------------------------------------------------------------------
// Scan entry point

/// One FileData per (account, conversation). The billed cents land on the
/// event's `cost`; a row with none falls back to the price table, where the
/// Cursor model spellings reach the same rows as the vendor's own harnesses.
fn rebuild(st: &mut CursorScan) {
    let mut groups: BTreeMap<String, Vec<Rec>> = BTreeMap::new();
    for (acct_key, a) in &st.accounts {
        for r in &a.records {
            groups.entry(format!("{acct_key}\u{0}{}", if r.s.is_empty() { "cursor" } else { &r.s })).or_default().push(r.clone());
        }
    }
    let mut files = Vec::new();
    for (key_s, mut recs) in groups {
        let session = key_s.split('\u{0}').nth(1).unwrap_or("cursor").to_string();
        // identical billed rows are real (two calls, same second, same totals):
        // each needs its own id, numbered the same way every build
        recs.sort_by(|a, b| (a.ts, &a.m, &a.s, a.i, a.o, a.cr, a.cw, a.c.map(|c| c.to_bits())).cmp(&(b.ts, &b.m, &b.s, b.i, b.o, b.cr, b.cw, b.c.map(|c| c.to_bits()))));
        let mut fd = FileData { src: Src::Cursor, size: 0, mtime: 0, session, cwd: String::new(), title: String::new(), models: Vec::new(), events: Vec::new() };
        let mut occ: HashMap<u64, u64> = HashMap::new();
        let mut last = 0i64;
        for r in recs {
            last = last.max(r.ts);
            // the acct half of the session key dedupes the same event fetched
            // through two keys; the acct sits in the id via the session key too
            let model = r.m.strip_prefix("cursor-").unwrap_or(&r.m);
            let mi = intern(&mut fd.models, model.as_bytes());
            let ident = key(&[
                key_s.as_bytes(),
                &r.ts.to_le_bytes(),
                model.as_bytes(),
                &r.i.to_le_bytes(),
                &r.o.to_le_bytes(),
                &r.cr.to_le_bytes(),
                &r.cw.to_le_bytes(),
                &r.c.map(|c| c.to_bits()).unwrap_or(0).to_le_bytes(),
            ]);
            let n = occ.entry(ident).or_insert(0);
            *n += 1;
            let id = key(&[&ident.to_le_bytes(), &(*n).to_le_bytes()]);
            fd.events.push(Event {
                ts: (r.ts / 1000) as u32,
                model: mi,
                input: r.i,
                output: r.o,
                cache_read: r.cr,
                cache_w5m: r.cw,
                cache_w1h: 0,
                id,
                fast: false,
                cost: r.c,
            });
        }
        fd.mtime = (last * 1_000_000) as u64; // nanos, like the file walk
        files.push(Arc::new(fd));
    }
    st.files = files;
}

fn cache_path() -> PathBuf {
    data_dir().join("cursor-usage.json")
}

fn load(st: &mut CursorScan) {
    st.loaded = true;
    let Ok(text) = fs::read_to_string(cache_path()) else { return };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return };
    if v.get("v").and_then(|x| x.as_u64()) != Some(1) {
        return;
    }
    let Some(accounts) = v.get("accounts").and_then(|a| a.as_object()) else { return };
    for (key_s, av) in accounts {
        let Ok(a) = serde_json::from_value::<Acct>(av.clone()) else { continue };
        st.accounts.insert(key_s.clone(), a);
    }
    st.dirty = !st.accounts.is_empty();
}

fn persist(st: &CursorScan) {
    let v = serde_json::json!({
        "v": 1,
        "accounts": st.accounts,
    });
    let tmp = cache_path().with_extension("json.tmp");
    let _ = fs::create_dir_all(data_dir());
    if fs::write(&tmp, serde_json::to_string(&v).unwrap_or_default()).is_ok() {
        let _ = fs::rename(&tmp, cache_path());
    }
}

/// Reads (or reuses) the account event history and returns the FileData set.
/// Failures keep the last good numbers, like the database readers.
pub(crate) fn scan(st: &mut CursorScan) -> Vec<Arc<FileData>> {
    if !st.loaded {
        load(st);
    }
    let now = now_ms();
    for cred in credentials() {
        let kh = key(&[cred.api_key.as_bytes()]);
        if st.failed.get(&kh).is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            continue;
        }
        if let Err(e) = fetch_events(&cred, st, now) {
            st.failed.insert(kh, Instant::now());
            let _ = fs::create_dir_all(data_dir());
            let _ = fs::write(data_dir().join("cursor-error.txt"), format!("JTokens {} cursor read failed:\n{e}\n", env!("CARGO_PKG_VERSION")));
        } else {
            let _ = fs::remove_file(data_dir().join("cursor-error.txt"));
        }
    }
    if st.dirty {
        persist(st);
        rebuild(st);
        st.dirty = false;
    }
    st.files.clone()
}
