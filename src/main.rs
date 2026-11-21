#![windows_subsystem = "windows"]

mod alerts;
mod prices;
mod update;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Assembly scanner (src/scan.asm)

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rec {
    input: u64,
    output: u64,
    cache_read: u64,
    cw: u64,
    c1h: u64,
    id: u64,
    moff: u32,
    ts: u32,
    mlen: u16,
    kind: u8,
    flags: u8,
    _pad: u32,
}

extern "C" {
    fn tk_init();
    fn tk_has_avx2() -> i64;
    fn tk_find(hay: *const u8, len: usize, needle: *const u8, nlen: usize) -> isize;
    fn tk_scan_claude(buf: *const u8, len: usize, out: *mut Rec, max: usize) -> usize;
    fn tk_scan_codex(buf: *const u8, len: usize, out: *mut Rec, max: usize) -> usize;
}

const PAD: usize = 64;

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let r = unsafe { tk_find(hay.as_ptr(), hay.len(), needle.as_ptr(), needle.len()) };
    (r >= 0).then_some(r as usize)
}

/// Reads a JSON string value starting right after its opening quote.
fn json_str_at(buf: &[u8], start: usize) -> String {
    let mut out = Vec::new();
    let mut i = start;
    while i < buf.len() && out.len() < 600 {
        match buf[i] {
            b'"' => break,
            b'\\' if i + 1 < buf.len() => {
                i += 1;
                match buf[i] {
                    b'n' | b't' | b'r' => out.push(b' '),
                    b'u' => {
                        // \uXXXX becomes its UTF-8 bytes; a surrogate pair 😀 becomes one character
                        let hex = |at: usize| buf.get(at..at + 4).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u32::from_str_radix(h, 16).ok());
                        if let Some(mut cp) = hex(i + 1) {
                            i += 4;
                            if (0xD800..0xDC00).contains(&cp) && buf.get(i + 1) == Some(&b'\\') && buf.get(i + 2) == Some(&b'u') {
                                if let Some(lo) = hex(i + 3).filter(|lo| (0xDC00..0xE000).contains(lo)) {
                                    cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                    i += 6;
                                }
                            }
                            let mut tmp = [0u8; 4];
                            out.extend_from_slice(char::from_u32(cp).unwrap_or('\u{FFFD}').encode_utf8(&mut tmp).as_bytes());
                        }
                    }
                    c => out.push(c),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn str_after(buf: &[u8], key: &[u8], limit: usize) -> Option<String> {
    let lim = buf.len().min(limit);
    find(&buf[..lim], key).map(|i| json_str_at(buf, i + key.len()))
}

/// The string value for `key` (`"role"`), skipping `":` and spaces. None when
/// the key or a string is not there.
fn str_field(buf: &[u8], key: &[u8]) -> Option<String> {
    let mut i = find(buf, key)? + key.len();
    while buf.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    if buf.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    while buf.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    (buf.get(i) == Some(&b'"')).then(|| json_str_at(buf, i + 1))
}

/// The byte span of the `{...}` object value for `key` (`"metadata"`). Cheap to
/// run on a huge row: the rest of the row never gets parsed as JSON.
fn json_obj_span(buf: &[u8], key: &[u8]) -> Option<(usize, usize)> {
    let mut i = find(buf, key)? + key.len();
    while buf.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    if buf.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    while buf.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    if buf.get(i) != Some(&b'{') {
        return None;
    }
    let start = i;
    let mut depth = 0usize;
    let (mut in_str, mut esc) = (false, false);
    while i < buf.len() {
        match buf[i] {
            _ if esc => esc = false,
            b'\\' if in_str => esc = true,
            b'"' => in_str = !in_str,
            b'{' if !in_str => depth += 1,
            b'}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return Some((start, i + 1));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Parsed files

/// One value per harness. The number is the index into `HARNESS_NAMES`, which
/// the page gets as `harnesses`; add a harness by adding a name and a reader.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Src {
    Claude = 0,
    Codex = 1,
    OpenCode = 2,
    Pi = 3,
    Droid = 4,
    Qwen = 5,
    Devin = 6,
}

const HARNESS_NAMES: [&str; 7] = ["Claude Code", "Codex", "OpenCode", "Pi", "Droid", "Qwen Code", "Devin"];

struct Event {
    ts: u32,
    model: u16,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_w5m: u64,
    cache_w1h: u64,
    id: u64,
    fast: bool,
}

struct FileData {
    src: Src,
    size: u64,
    mtime: u64,
    session: String,
    cwd: String,
    title: String,
    models: Vec<String>,
    events: Vec<Event>,
}

fn read_padded(path: &Path, buf: &mut Vec<u8>) -> std::io::Result<()> {
    buf.clear();
    let mut f = fs::File::open(path)?;
    f.read_to_end(buf)?;
    buf.extend_from_slice(&[0u8; PAD]);
    Ok(())
}

fn codex_key(total: u64, input: u64, output: u64, cached: u64) -> u64 {
    let mut h = 0x9e37_79b9_7f4a_7c15u64 ^ 0xc0de;
    for v in [total, input, output, cached] {
        h = (h ^ v).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        h ^= h >> 31;
    }
    h | 1
}

fn intern(models: &mut Vec<String>, s: &[u8]) -> u16 {
    if let Some(i) = models.iter().position(|m| m.as_bytes() == s) {
        return i as u16;
    }
    models.push(String::from_utf8_lossy(s).into_owned());
    (models.len() - 1) as u16
}

/// Stable id for one usage record, so a copied record (a forked session) counts once.
fn key(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for &b in p.iter() {
            h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
        h = (h ^ 0xff).wrapping_mul(0x0100_0000_01b3);
    }
    h | 1
}

// Pi: one JSON object per line. Assistant messages carry `usage` and a
// millisecond `timestamp`; the header line carries the project `cwd`.
#[derive(serde::Deserialize)]
struct PiLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    cwd: Option<String>,
    name: Option<String>,
    message: Option<PiMsg>,
}

#[derive(serde::Deserialize)]
struct PiMsg {
    // any JSON type here: a line with an odd model or timestamp must not be dropped whole
    model: Option<serde_json::Value>,
    timestamp: Option<serde_json::Value>,
    usage: Option<PiUsage>,
}

#[derive(serde::Deserialize, Default)]
struct PiUsage {
    // read as numbers of any kind; a fraction or a negative count is cut to a whole, non-negative count below
    input: Option<f64>,
    output: Option<f64>,
    #[serde(rename = "cacheRead")]
    cache_read: Option<f64>,
    #[serde(rename = "cacheWrite")]
    cache_write: Option<f64>,
}

fn parse_pi(path: &Path, session: String, size: u64, mtime: u64, buf: &mut Vec<u8>) -> Option<FileData> {
    buf.clear();
    fs::File::open(path).ok()?.read_to_end(buf).ok()?;
    let mut fd = FileData { src: Src::Pi, size, mtime, session, cwd: String::new(), title: String::new(), models: Vec::new(), events: Vec::new() };
    for line in buf.split(|&c| c == b'\n') {
        // skip the big lines that cannot hold usage or the header/title before parsing them
        let usage = find(line, b"\"usage\"").is_some();
        if !usage && find(line, b"\"type\":\"session").is_none() {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<PiLine>(line) else { continue };
        match v.kind.as_deref() {
            Some("session") => fd.cwd = v.cwd.unwrap_or_default(),
            Some("session_info") => {
                if let Some(n) = v.name.filter(|n| !n.is_empty()) {
                    fd.title = n; // the last name wins, like Claude's ai-title
                }
            }
            Some("message") => {
                let Some(m) = v.message else { continue };
                let Some(u) = m.usage else { continue };
                // infinite counts are dropped; negative counts and NaN become 0; huge ones are capped
                let whole = |x: Option<f64>| x.filter(|f| f.is_finite()).map_or(0, |f| f.clamp(0.0, 1e12) as u64);
                let (input, output) = (whole(u.input), whole(u.output));
                let (cache_read, cache_write) = (whole(u.cache_read), whole(u.cache_write));
                if input + output + cache_read + cache_write == 0 {
                    continue;
                }
                let ts = m.timestamp.as_ref().and_then(|v| v.as_f64()).map(|ms| (ms / 1000.0) as u32).unwrap_or((mtime / 1_000_000_000) as u32);
                let model = m.model.as_ref().and_then(|v| v.as_str()).filter(|s| !s.is_empty()).unwrap_or("unknown");
                let id = match v.id.as_deref().filter(|s| !s.is_empty()) {
                    Some(id) => key(&[id.as_bytes(), &ts.to_le_bytes()]),
                    None => 0,
                };
                let model = intern(&mut fd.models, model.as_bytes());
                // pi's output already includes reasoning; cache writes use the 5 minute rate
                fd.events.push(Event { ts, model, input, output, cache_read, cache_w5m: cache_write, cache_w1h: 0, id, fast: false });
            }
            _ => {}
        }
    }
    Some(fd)
}

// Droid keeps no per-message usage on disk. Each session's settings file holds
// its running totals, so one event per session is made, at the time the file
// was last saved.
#[derive(serde::Deserialize)]
struct DroidSettings {
    // any JSON type here: a non-text model must not make the whole file unreadable
    model: Option<serde_json::Value>,
    #[serde(rename = "tokenUsage")]
    usage: Option<DroidUsage>,
}

#[derive(serde::Deserialize, Default)]
struct DroidUsage {
    // counts of any number type (or null); cut to whole, non-negative counts in parse_droid
    #[serde(rename = "inputTokens", default)]
    input: Option<f64>,
    #[serde(rename = "outputTokens", default)]
    output: Option<f64>,
    #[serde(rename = "cacheReadTokens", default)]
    cache_read: Option<f64>,
    #[serde(rename = "cacheCreationTokens", default)]
    cache_write: Option<f64>,
}

fn parse_droid(path: &Path, session: String, size: u64, mtime: u64) -> Option<FileData> {
    let s: DroidSettings = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let mut fd = FileData { src: Src::Droid, size, mtime, session, cwd: String::new(), title: String::new(), models: Vec::new(), events: Vec::new() };
    // the settings file has no folder; the session's own log starts with it
    if let Ok(mut f) = fs::File::open(path.with_file_name(format!("{}.jsonl", fd.session))) {
        let mut head = vec![0u8; 1 << 16];
        let n = f.read(&mut head).unwrap_or(0);
        fd.cwd = str_after(&head[..n], b"\"cwd\":\"", n).unwrap_or_default();
    }
    // infinite counts are dropped; negative counts and NaN become 0; huge ones are capped
    let whole = |x: Option<f64>| x.filter(|f| f.is_finite()).map_or(0, |f| f.clamp(0.0, 1e12) as u64);
    let u = s.usage.unwrap_or_default();
    let (input, output, cache_read, cache_write) = (whole(u.input), whole(u.output), whole(u.cache_read), whole(u.cache_write));
    if input + output + cache_read + cache_write == 0 {
        return Some(fd);
    }
    let model = s.model.as_ref().and_then(|v| v.as_str()).filter(|m| !m.is_empty()).unwrap_or("unknown");
    let model = intern(&mut fd.models, model.as_bytes());
    let id = key(&[fd.session.as_bytes(), b"droid"]);
    fd.events.push(Event {
        ts: (mtime / 1_000_000_000) as u32,
        model,
        input,
        output,
        cache_read,
        cache_w5m: cache_write,
        cache_w1h: 0,
        id,
        fast: false,
    });
    Some(fd)
}

// Qwen Code: one JSON object per line in ~/.qwen/projects/<folder>/chats/<session>.jsonl. Each
// model reply carries `usageMetadata` (Gemini style): promptTokenCount includes the cached
// part, and thoughts are billed as output. Every line also carries `cwd` and an ISO timestamp.
#[derive(serde::Deserialize)]
struct QwenLine {
    uuid: Option<String>,
    timestamp: Option<String>,
    // any JSON type here: a line with an odd model must not be dropped whole
    model: Option<serde_json::Value>,
    #[serde(rename = "usageMetadata")]
    usage: Option<QwenUsage>,
}

#[derive(serde::Deserialize, Default)]
struct QwenUsage {
    // any JSON type here: a count that is not a number counts as 0 and the rest of the line is kept
    #[serde(rename = "promptTokenCount", default)]
    prompt: Option<serde_json::Value>,
    #[serde(rename = "candidatesTokenCount", default)]
    candidates: Option<serde_json::Value>,
    #[serde(rename = "thoughtsTokenCount", default)]
    thoughts: Option<serde_json::Value>,
    #[serde(rename = "cachedContentTokenCount", default)]
    cached: Option<serde_json::Value>,
}

fn parse_qwen(path: &Path, session: String, size: u64, mtime: u64, buf: &mut Vec<u8>) -> Option<FileData> {
    buf.clear();
    fs::File::open(path).ok()?.read_to_end(buf).ok()?;
    let mut fd = FileData { src: Src::Qwen, size, mtime, session, cwd: String::new(), title: String::new(), models: Vec::new(), events: Vec::new() };
    // a count that is not a number reads as 0; infinite counts are dropped; negative counts and NaN become 0; huge ones are capped
    let whole = |x: &Option<serde_json::Value>| x.as_ref().and_then(|v| v.as_f64()).filter(|f| f.is_finite()).map_or(0, |f| f.clamp(0.0, 1e12) as u64);
    for line in buf.split(|&c| c == b'\n') {
        if fd.cwd.is_empty() {
            fd.cwd = str_after(line, b"\"cwd\":\"", line.len()).unwrap_or_default();
        }
        if find(line, b"\"usageMetadata\"").is_none() {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<QwenLine>(line) else { continue };
        let Some(u) = v.usage else { continue };
        let prompt = whole(&u.prompt);
        let cached = whole(&u.cached).min(prompt);
        let output = whole(&u.candidates) + whole(&u.thoughts);
        if prompt + output == 0 {
            continue;
        }
        let ts = v.timestamp.as_deref().and_then(iso_seconds).unwrap_or(mtime / 1_000_000_000) as u32;
        let model = v.model.as_ref().and_then(|m| m.as_str()).filter(|s| !s.is_empty()).unwrap_or("unknown");
        let id = match v.uuid.as_deref().filter(|s| !s.is_empty()) {
            Some(uuid) => key(&[fd.session.as_bytes(), uuid.as_bytes()]),
            None => 0,
        };
        let model = intern(&mut fd.models, model.as_bytes());
        fd.events.push(Event { ts, model, input: prompt - cached, output, cache_read: cached, cache_w5m: 0, cache_w1h: 0, id, fast: false });
    }
    Some(fd)
}

// "2026-06-13T19:40:48.960Z" -> unix seconds (UTC). None for anything else.
fn iso_seconds(s: &str) -> Option<u64> {
    let num = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || !(0..=23).contains(&h) || !(0..=59).contains(&mi) || !(0..=60).contains(&se) {
        return None;
    }
    // days since 1970-01-01 (proleptic Gregorian, H. Hinnant's civil-from-days inverse)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = (if y2 >= 0 { y2 } else { y2 - 399 }) / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3_600 + mi * 60 + se).ok()
}

// A stored value as text, whatever its type; None for NULL or binary data.
fn stored_text(v: rusqlite::types::Value) -> Option<String> {
    use rusqlite::types::Value;
    match v {
        Value::Text(s) => Some(s),
        Value::Integer(i) => Some(i.to_string()),
        Value::Real(f) => Some(f.to_string()),
        _ => None,
    }
}

// A stored value as a whole time; None when it is not a time.
fn stored_time(v: rusqlite::types::Value) -> Option<i64> {
    use rusqlite::types::Value;
    match v {
        Value::Integer(i) => Some(i),
        Value::Real(f) if f.is_finite() => Some(f as i64),
        Value::Text(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

// OpenCode keeps everything in one SQLite database. Read it whole (it is small
// next to the logs) and group the assistant messages by session.
// None when the database cannot be read right now (locked, busy, schema changed): the
// caller keeps its last numbers and tries again on the next scan.
fn scan_opencode(db: &Path) -> Option<Vec<Arc<FileData>>> {
    use rusqlite::types::Value;
    use rusqlite::{Connection, OpenFlags};
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(2));
    let mut stmt = conn
        .prepare(
            "SELECT m.id, m.session_id, m.time_created, m.data, s.directory, s.title \
             FROM message m JOIN session s ON s.id = m.session_id \
             WHERE m.data LIKE '%\"tokens\"%' ORDER BY m.time_created",
        )
        .ok()?;
    let rows = stmt
        .query_map([], |r| {
            // every column is read as it is stored, so a value of an odd type is skipped below, not fatal
            Ok((r.get::<_, Value>(0)?, r.get::<_, Value>(1)?, r.get::<_, Value>(2)?, r.get::<_, Value>(3)?, r.get::<_, Value>(4)?, r.get::<_, Value>(5)?))
        })
        .ok()?;
    // only a failed read (locked, busy, damaged) is fatal: the caller keeps the last numbers and retries
    let rows: Vec<_> = rows.collect::<Result<Vec<_>, _>>().ok()?;
    let mut by: HashMap<String, FileData> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for (mid, sid, created, data, dir, title) in rows {
        let (Some(mid), Some(sid), Some(data), Some(created)) = (stored_text(mid), stored_text(sid), stored_text(data), stored_time(created)) else { continue };
        let (dir, title) = (stored_text(dir), stored_text(title));
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else { continue };
        if v.get("role").and_then(|x| x.as_str()) != Some("assistant") {
            continue;
        }
        // a fraction or a negative count is cut to a whole, non-negative count instead of becoming 0
        // counts may be numbers or numeric text; infinite values are dropped and huge ones capped
        let num = |p: &str| {
            v.pointer(p)
                .and_then(|x| {
                    x.as_u64().map(|u| u.min(1_000_000_000_000)).or_else(|| {
                        x.as_f64()
                            .or_else(|| x.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
                            .filter(|f| f.is_finite())
                            .map(|f| f.clamp(0.0, 1e12) as u64)
                    })
                })
                .unwrap_or(0)
        };
        let (input, output, reasoning) = (num("/tokens/input"), num("/tokens/output"), num("/tokens/reasoning"));
        let (cache_read, cache_write) = (num("/tokens/cache/read"), num("/tokens/cache/write"));
        if input + output + reasoning + cache_read + cache_write == 0 {
            continue;
        }
        let model = v.get("modelID").and_then(|x| x.as_str()).filter(|s| !s.is_empty()).unwrap_or("unknown");
        if !by.contains_key(&sid) {
            order.push(sid.clone());
            by.insert(
                sid.clone(),
                FileData {
                    src: Src::OpenCode,
                    size: 0,
                    mtime: 0,
                    session: sid.clone(),
                    cwd: dir.unwrap_or_default().replace('/', "\\"),
                    title: title.unwrap_or_default(),
                    models: Vec::new(),
                    events: Vec::new(),
                },
            );
        }
        let fd = by.get_mut(&sid).expect("inserted above");
        let model = intern(&mut fd.models, model.as_bytes());
        // OpenCode lists reasoning apart from output; both are billed as output
        fd.events.push(Event {
            ts: (created / 1000) as u32,
            model,
            input,
            output: output + reasoning,
            cache_read,
            cache_w5m: cache_write,
            cache_w1h: 0,
            id: key(&[mid.as_bytes()]),
            fast: false,
        });
    }
    Some(order.into_iter().filter_map(|s| by.remove(&s)).map(Arc::new).collect())
}

// A count of any JSON type: a number, numeric text, or nothing. Fractions and negative
// values are cut to a whole, non-negative count; infinite values are dropped; huge ones capped.
fn json_count(x: Option<&serde_json::Value>) -> u64 {
    x.and_then(|x| {
        x.as_u64().map(|u| u.min(1_000_000_000_000)).or_else(|| {
            x.as_f64().or_else(|| x.as_str().and_then(|s| s.trim().parse::<f64>().ok())).filter(|f| f.is_finite()).map(|f| f.clamp(0.0, 1e12) as u64)
        })
    })
    .unwrap_or(0)
}

// Devin CLI keeps its sessions in one SQLite database. Each assistant reply in message_nodes
// carries its usage in chat_message.metadata.metrics. Input is the part that was not cached,
// cache_read_tokens and cache_creation_tokens are the cached parts (the same reading the
// Devin token dashboard uses). The database is big and changes all the time, so parsed rows
// are kept in DvScan and a rescan reads only the new tail. None when the read fails; the
// caller keeps its last numbers.

/// One Devin usage row, kept so a rescan only has to read new rows.
struct DvRow {
    sid: String,
    secs: i64,
    model: String,
    fast: bool,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

#[derive(Default)]
struct DvScan {
    sig: Option<(u64, u64, u64, u64)>,
    /// the largest row_id scanned
    max: i64,
    /// parsed rows by rowid; iterating it walks rows in ORDER BY row_id order
    rows: BTreeMap<i64, DvRow>,
    /// session id -> (working directory, title), last seen wins
    sessions: HashMap<String, (String, String)>,
    files: Vec<Arc<FileData>>,
    /// last time every row was read; hourly, in case old rows were edited or deleted
    last_full: Option<Instant>,
}

/// A rescan re-reads back to max - DV_TAIL, which catches chat_message rows edited after insert.
const DV_TAIL: i64 = 1000;

fn scan_devin(db: &Path, st: &mut DvScan) -> Option<()> {
    use rusqlite::types::Value;
    use rusqlite::{Connection, OpenFlags};
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(2));
    let full = st.last_full.map_or(true, |t| t.elapsed().as_secs() > 3600);
    let tail = if full { 0 } else { (st.max - DV_TAIL).max(0) };
    let mut stmt = conn
        .prepare(
            "SELECT m.row_id, m.session_id, m.created_at, m.chat_message, s.working_directory, s.title \
             FROM message_nodes m LEFT JOIN sessions s ON s.id = m.session_id \
             WHERE m.row_id >= ?1 AND m.chat_message LIKE '%cache_read_tokens%' ORDER BY m.row_id",
        )
        .ok()?;
    let mut rows = stmt.query([tail]).ok()?;
    let mut got: Vec<(i64, DvRow, String, String)> = Vec::new();
    loop {
        // a failed read (locked, busy, damaged) is fatal: the caller keeps the last numbers
        let r = match rows.next() {
            Ok(Some(r)) => r,
            Ok(None) => break,
            Err(_) => return None,
        };
        let (Ok(rid), Ok(sid), Ok(created), Ok(chat), Ok(dir), Ok(title)) =
            (r.get::<_, Value>(0), r.get::<_, Value>(1), r.get::<_, Value>(2), r.get::<_, Value>(3), r.get::<_, Value>(4), r.get::<_, Value>(5)) else { continue };
        let (Some(rid), Some(sid), Some(chat), Some(created)) = (stored_text(rid), stored_text(sid), stored_text(chat), stored_time(created)) else { continue };
        let (dir, title) = (stored_text(dir).unwrap_or_default(), stored_text(title).unwrap_or_default());
        let cb = chat.as_bytes();
        if str_field(cb, b"\"role\"").as_deref() != Some("assistant") {
            continue;
        }
        // metrics sits in the small metadata object; the rest of the row is not parsed
        let Some((a, b)) = json_obj_span(cb, b"\"metadata\"") else { continue };
        let Ok(md) = serde_json::from_str::<serde_json::Value>(&chat[a..b]) else { continue };
        let Some(metrics) = md.get("metrics") else { continue };
        let (input, output) = (json_count(metrics.get("input_tokens")), json_count(metrics.get("output_tokens")));
        let (cache_read, cache_write) = (json_count(metrics.get("cache_read_tokens")), json_count(metrics.get("cache_creation_tokens")));
        if input + output + cache_read + cache_write == 0 {
            continue;
        }
        let model = md.get("generation_model").and_then(|x| x.as_str()).filter(|s| !s.is_empty()).unwrap_or("unknown");
        // a -fast or -lightning tail is Devin's speed tier, billed like other fast modes
        let lower = model.to_ascii_lowercase();
        let fast = lower.ends_with("-fast") || lower.ends_with("-lightning");
        // created_at is milliseconds in current databases; a value that small is already seconds
        let secs = if created > 1_000_000_000_000 { created / 1000 } else { created };
        let Ok(rid) = rid.parse::<i64>() else { continue };
        got.push((
            rid,
            DvRow { sid, secs, model: model.to_string(), fast, input, output, cache_read, cache_write },
            dir.replace('/', "\\"),
            title,
        ));
    }
    // the read worked: drop the overlap window and put back its fresh copies
    if tail > 0 {
        st.rows.split_off(&tail);
    } else {
        st.rows.clear();
        st.sessions.clear();
    }
    for (rid, row, dir, title) in got {
        st.max = st.max.max(rid);
        st.sessions.insert(row.sid.clone(), (dir, title));
        st.rows.insert(rid, row);
    }
    if full {
        st.last_full = Some(Instant::now());
    }
    st.files = dv_rebuild(st);
    Some(())
}

/// Groups the kept Devin rows into one FileData per session, in row_id order.
fn dv_rebuild(st: &DvScan) -> Vec<Arc<FileData>> {
    let mut by: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<FileData> = Vec::new();
    for (rid, r) in &st.rows {
        let i = match by.get(&r.sid) {
            Some(&i) => i,
            None => {
                let (dir, title) = st.sessions.get(&r.sid).cloned().unwrap_or_default();
                out.push(FileData {
                    src: Src::Devin,
                    size: 0,
                    mtime: 0,
                    session: r.sid.clone(),
                    cwd: dir,
                    title,
                    models: Vec::new(),
                    events: Vec::new(),
                });
                by.insert(r.sid.clone(), out.len() - 1);
                out.len() - 1
            }
        };
        let fd = &mut out[i];
        let model = intern(&mut fd.models, r.model.as_bytes());
        fd.events.push(Event {
            ts: r.secs as u32,
            model,
            input: r.input,
            output: r.output,
            cache_read: r.cache_read,
            cache_w5m: r.cache_write,
            cache_w1h: 0,
            id: key(&[r.sid.as_bytes(), rid.to_string().as_bytes()]),
            fast: r.fast,
        });
    }
    out.into_iter().map(Arc::new).collect()
}

fn parse_file(src: Src, path: &Path, session: String, size: u64, mtime: u64, buf: &mut Vec<u8>, recs: &mut Vec<Rec>) -> Option<FileData> {
    match src {
        Src::Pi => return parse_pi(path, session, size, mtime, buf),
        Src::Droid => return parse_droid(path, session, size, mtime),
        Src::Qwen => return parse_qwen(path, session, size, mtime, buf),
        _ => {}
    }
    read_padded(path, buf).ok()?;
    let len = buf.len() - PAD;
    let data = &buf[..len];
    let max = len / 64 + 16;
    recs.clear();
    recs.reserve(max);
    let n = unsafe {
        let f = if src == Src::Claude { tk_scan_claude } else { tk_scan_codex };
        f(buf.as_ptr(), len, recs.as_mut_ptr(), max)
    };
    unsafe { recs.set_len(n) };

    let mut fd = FileData {
        src,
        size,
        mtime,
        session,
        cwd: String::new(),
        title: String::new(),
        models: Vec::new(),
        events: Vec::with_capacity(n),
    };
    fd.cwd = str_after(data, b"\"cwd\":\"", 1 << 20).unwrap_or_default();
    if src == Src::Claude {
        // last ai-title wins; it is rewritten as the session evolves
        let key = b"\"aiTitle\":\"";
        let mut at = 0;
        while let Some(i) = find(&data[at..], key) {
            fd.title = json_str_at(data, at + i + key.len());
            at += i + key.len();
        }
    }

    // events logged before the first turn_context get the file's first model
    let mut cur_model: Option<u16> = recs
        .iter()
        .find(|r| r.kind == 2)
        .map(|r| intern(&mut fd.models, &data[r.moff as usize..(r.moff as usize + r.mlen as usize).min(len)]));
    let mut last_total = 0u64;
    for r in recs.iter() {
        let model_bytes = &data[r.moff as usize..(r.moff as usize + r.mlen as usize).min(len)];
        match (src, r.kind) {
            (Src::Codex, 2) => cur_model = Some(intern(&mut fd.models, model_bytes)),
            (Src::Codex, 1) => {
                // repeated token_count events carry an unchanged cumulative total
                if r.c1h != 0 && r.c1h == last_total {
                    continue;
                }
                last_total = r.c1h;
                if r.input == 0 && r.output == 0 {
                    continue;
                }
                let m = match cur_model {
                    Some(m) => m,
                    None => intern(&mut fd.models, b"unknown"),
                };
                let cached = r.cache_read.min(r.input);
                // Forked sessions replay the parent's token_count events. The
                // cumulative total plus the per-turn numbers identify an event.
                let id = if r.c1h != 0 { codex_key(r.c1h, r.input, r.output, r.cache_read) } else { 0 };
                fd.events.push(Event {
                    ts: r.ts,
                    model: m,
                    input: r.input - cached,
                    output: r.output,
                    cache_read: cached,
                    cache_w5m: 0,
                    cache_w1h: 0,
                    id,
                    fast: false,
                });
            }
            (Src::Claude, 1) => {
                if model_bytes.first() == Some(&b'<') {
                    continue; // <synthetic>
                }
                let m = intern(&mut fd.models, if model_bytes.is_empty() { b"unknown" } else { model_bytes });
                let w1h = r.c1h.min(r.cw);
                fd.events.push(Event {
                    ts: r.ts,
                    model: m,
                    input: r.input,
                    output: r.output,
                    cache_read: r.cache_read,
                    cache_w5m: r.cw - w1h,
                    cache_w1h: w1h,
                    id: r.id,
                    fast: r.flags & 1 != 0,
                });
            }
            _ => {}
        }
    }
    Some(fd)
}

// ---------------------------------------------------------------------------
// Directory walk + cache

struct Job {
    src: Src,
    path: PathBuf,
    session: String,
    size: u64,
    mtime: u64,
}

/// Collects (path, size, mtime). On Windows DirEntry metadata comes from the
/// directory listing itself, so no extra file open per entry.
fn is_log(p: &Path, suffix: &str) -> bool {
    // Windows file names are not case-sensitive: *.JSONL is a log too
    p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.to_ascii_lowercase().ends_with(&suffix.to_ascii_lowercase()))
}

fn walk(dir: &Path, suffix: &str, out: &mut Vec<(PathBuf, u64, u64)>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(md) = e.metadata() else { continue };
        let p = e.path();
        if md.is_dir() {
            walk(&p, suffix, out);
        } else if is_log(&p, suffix) {
            let mtime = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0);
            out.push((p, md.len(), mtime));
        }
    }
}

/// Walks each second-level directory on its own thread (Codex keeps one
/// folder per day, Claude one per project).
fn walk_parallel(root: &Path, suffix: &str) -> Vec<(PathBuf, u64, u64)> {
    let mut out = Vec::new();
    let mut dirs = Vec::new();
    let Ok(rd) = fs::read_dir(root) else { return out };
    for e in rd.flatten() {
        let Ok(md) = e.metadata() else { continue };
        let p = e.path();
        if md.is_dir() {
            match fs::read_dir(&p) {
                Ok(rd2) => {
                    for e2 in rd2.flatten() {
                        let Ok(md2) = e2.metadata() else { continue };
                        let p2 = e2.path();
                        if md2.is_dir() {
                            dirs.push(p2);
                        } else if is_log(&p2, suffix) {
                            let mt = md2.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0);
                            out.push((p2, md2.len(), mt));
                        }
                    }
                }
                Err(_) => continue,
            }
        } else if is_log(&p, suffix) {
            let mt = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0);
            out.push((p, md.len(), mt));
        }
    }
    let next = AtomicUsize::new(0);
    let found = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                IN_SCAN.with(|c| c.set(true)); // a panic here is caught by the scan, not shown in a box
                let mut local = Vec::new();
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    if k >= dirs.len() {
                        break;
                    }
                    walk(&dirs[k], suffix, &mut local);
                }
                found.lock().unwrap().extend(local);
            });
        }
    });
    out.extend(found.into_inner().unwrap());
    out
}

fn home() -> PathBuf {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_default()
}

fn claude_session(path: &Path) -> String {
    // <project>/<session>.jsonl or <project>/<session>/subagents/agent-x.jsonl
    let parent = path.parent();
    if parent.and_then(|p| p.file_name()).is_some_and(|n| n == "subagents") {
        if let Some(s) = parent.and_then(|p| p.parent()).and_then(|p| p.file_name()) {
            return s.to_string_lossy().into_owned();
        }
    }
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn codex_session(path: &Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    // the last 36 characters (the session id); by characters, so a non-ASCII name cannot split a char
    match stem.char_indices().rev().nth(35) {
        Some((i, _)) => stem[i..].to_string(),
        None => stem,
    }
}

fn droid_session(path: &Path) -> String {
    // <session id>.settings.json
    let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    name.strip_suffix(".settings.json").map(str::to_string).unwrap_or(name)
}

/// (length, last-modified nanos) of a file, or zeros when it is missing.
fn file_sig(p: &Path) -> (u64, u64) {
    match fs::metadata(p) {
        Ok(md) => (md.len(), md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0)),
        Err(_) => (0, 0),
    }
}

#[derive(Default)]
struct ScanStats {
    files: usize,
    parsed: usize,
    bytes: u64,
    parsed_bytes: u64,
    ms: f64,
    /// timing detail: directory walk, each database read, and ms per log-file harness
    walk_ms: f64,
    opencode_ms: f64,
    devin_ms: f64,
    by_src_ms: [f64; 7],
}

#[derive(Default)]
struct Scanner {
    cache: HashMap<PathBuf, Arc<FileData>>,
    /// OpenCode's database signature and the sessions read from it last time
    oc_sig: Option<(u64, u64, u64, u64)>,
    oc_files: Vec<Arc<FileData>>,
    /// Devin's incremental database state
    dv: DvScan,
}

impl Scanner {
    fn scan(&mut self) -> (Vec<Arc<FileData>>, ScanStats) {
        let t0 = Instant::now();
        let h = home();
        let claude_dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| h.join(".claude"));
        let codex_dir = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| h.join(".codex"));
        let pi_dir = h.join(".pi").join("agent").join("sessions");
        let droid_dir = h.join(".factory").join("sessions");
        let qwen_dir = h.join(".qwen").join("projects");
        let oc_db = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| h.join(".local").join("share")).join("opencode").join("opencode.db");
        let mut stats = ScanStats::default();
        let mut jobs = Vec::new();
        for (src, dir, suffix) in [
            (Src::Claude, claude_dir.join("projects"), ".jsonl"),
            (Src::Codex, codex_dir.join("sessions"), ".jsonl"),
            (Src::Codex, codex_dir.join("archived_sessions"), ".jsonl"),
            (Src::Pi, pi_dir, ".jsonl"),
            (Src::Droid, droid_dir, ".settings.json"),
            (Src::Qwen, qwen_dir, ".jsonl"),
        ] {
            let tw = Instant::now();
            let paths = walk_parallel(&dir, suffix);
            stats.walk_ms += tw.elapsed().as_secs_f64() * 1000.0;
            for (p, size, mtime) in paths {
                let session = match src {
                    Src::Claude => claude_session(&p),
                    Src::Droid => droid_session(&p),
                    _ => codex_session(&p), // Codex and Pi: the 36 character session id ends the file name
                };
                jobs.push(Job { src, path: p, session, size, mtime });
            }
        }

        // OpenCode: one database. Re-read it only when the database or its write-ahead log changed.
        // Devin: the same rule for its sessions database. Both readers run on their own threads,
        // next to the log parse below, so a big database does not add to the scan's wall time.
        let (db_len, db_mt) = file_sig(&oc_db);
        let (wal_len, wal_mt) = file_sig(&oc_db.with_extension("db-wal"));
        let sig = (db_len, db_mt, wal_len, wal_mt);
        let devin_db = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| h.join("AppData").join("Roaming")).join("Devin").join("cli").join("sessions.db");
        let (dv_len, dv_mt) = file_sig(&devin_db);
        let (dvw_len, dvw_mt) = file_sig(&devin_db.with_extension("db-wal"));
        let dsig = (dv_len, dv_mt, dvw_len, dvw_mt);
        let oc_t = (self.oc_sig != Some(sig)).then(|| {
            let db = oc_db.clone();
            std::thread::spawn(move || {
                IN_SCAN.with(|c| c.set(true));
                let t = Instant::now();
                // a database that cannot be read now keeps the last numbers, and is tried again on the next scan
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| if db_len == 0 { Some(Vec::new()) } else { scan_opencode(&db) }));
                (r.unwrap_or_default(), t.elapsed())
            })
        });
        let dv_t = (self.dv.sig != Some(dsig)).then(|| {
            let db = devin_db.clone();
            let mut dv = std::mem::take(&mut self.dv);
            std::thread::spawn(move || {
                IN_SCAN.with(|c| c.set(true));
                let t = Instant::now();
                let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if dv_len == 0 {
                        dv.rows.clear();
                        dv.sessions.clear();
                        dv.max = 0;
                        dv.files = Vec::new();
                        Some(())
                    } else {
                        scan_devin(&db, &mut dv)
                    }
                }))
                .unwrap_or_default()
                .is_some();
                (dv, ok, t.elapsed())
            })
        });

        // the OpenCode and Devin databases count as one file each, however many sessions they hold
        stats.files = jobs.len() + usize::from(db_len > 0) + usize::from(dv_len > 0);
        let mut result: Vec<Option<Arc<FileData>>> = vec![None; jobs.len()];
        let mut todo = Vec::new();
        for (i, j) in jobs.iter().enumerate() {
            stats.bytes += j.size;
            match self.cache.get(&j.path) {
                Some(fd) if fd.size == j.size && fd.mtime == j.mtime => result[i] = Some(fd.clone()),
                _ => todo.push(i),
            }
        }
        // biggest files first so threads finish together
        todo.sort_by_key(|&i| std::cmp::Reverse(jobs[i].size));
        stats.parsed = todo.len();
        stats.parsed_bytes = todo.iter().map(|&i| jobs[i].size).sum();

        let next = AtomicUsize::new(0);
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(32);
        let parsed: Mutex<Vec<(usize, Arc<FileData>)>> = Mutex::new(Vec::with_capacity(todo.len()));
        // per-harness worker nanoseconds, so the report can name the slow provider
        let src_ns: [AtomicUsize; 7] = Default::default();
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    IN_SCAN.with(|c| c.set(true));
                    let mut buf = Vec::new();
                    let mut recs = Vec::new();
                    let mut local = Vec::new();
                    loop {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        if k >= todo.len() {
                            break;
                        }
                        let j = &jobs[todo[k]];
                        let tp = Instant::now();
                        if let Some(fd) = parse_file(j.src, &j.path, j.session.clone(), j.size, j.mtime, &mut buf, &mut recs) {
                            local.push((todo[k], Arc::new(fd)));
                        }
                        src_ns[j.src as usize].fetch_add(tp.elapsed().as_nanos() as usize, Ordering::Relaxed);
                    }
                    parsed.lock().unwrap().extend(local);
                });
            }
        });
        for (i, a) in src_ns.iter().enumerate() {
            stats.by_src_ms[i] += a.load(Ordering::Relaxed) as f64 / 1e6;
        }
        for (i, fd) in parsed.into_inner().unwrap() {
            self.cache.insert(jobs[i].path.clone(), fd.clone());
            result[i] = Some(fd);
        }
        // forget files that disappeared
        let live: HashSet<&PathBuf> = jobs.iter().map(|j| &j.path).collect();
        self.cache.retain(|k, _| live.contains(k));

        // the database threads have had the whole log parse to finish; join them now
        if let Some(h) = oc_t {
            if let Ok((r, dt)) = h.join() {
                stats.opencode_ms = dt.as_secs_f64() * 1000.0;
                if let Some(files) = r {
                    self.oc_files = files;
                    self.oc_sig = Some(sig);
                }
            }
        }
        if let Some(h) = dv_t {
            if let Ok((dv, ok, dt)) = h.join() {
                stats.devin_ms = dt.as_secs_f64() * 1000.0;
                self.dv = dv;
                if ok {
                    self.dv.sig = Some(dsig);
                }
            }
        }
        stats.ms = t0.elapsed().as_secs_f64() * 1000.0;
        let mut files: Vec<Arc<FileData>> = result.into_iter().flatten().collect();
        files.extend(self.oc_files.iter().cloned());
        files.extend(self.dv.files.iter().cloned());
        (files, stats)
    }
}

// ---------------------------------------------------------------------------
// Aggregation -> JSON for the UI

fn esc(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' | '\r' | '\t' => out.push(' '),
            c if (c as u32) < 0x20 => {}
            '<' => out.push_str("\\u003c"),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Like esc, but keeps line breaks (release notes).
fn esc_multiline(s: &str, out: &mut String) {
    let mut first = true;
    let mut tmp = String::new();
    for line in s.lines() {
        if !first {
            tmp.push('\u{E000}');
        }
        first = false;
        tmp.push_str(line);
    }
    let mut j = String::new();
    esc(&tmp, &mut j);
    out.push_str(&j.replace('\u{E000}', "\\n"));
}

fn codex_titles() -> HashMap<String, String> {
    let mut m = HashMap::new();
    let p = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".codex")).join("session_index.jsonl");
    if let Ok(mut b) = fs::read(&p) {
        b.extend_from_slice(&[0u8; PAD]);
        for line in b.split(|&c| c == b'\n') {
            let (Some(i), Some(t)) = (find(line, b"\"id\":\""), find(line, b"\"thread_name\":\"")) else { continue };
            // an empty name never replaces a real one
            let title = json_str_at(line, t + 15);
            if !title.is_empty() {
                m.insert(json_str_at(line, i + 6), title);
            }
        }
    }
    m
}

#[derive(Default, Clone, Copy)]
struct Agg {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
    reqs: u32,
}

struct Totals {
    json: String,
    events: usize,
    cost: f64,
    tokens: u64,
    recent: alerts::Recent,
}

/// Writes a file for a command-line flag. A failure is printed and returns false, so the run exits with an error.
fn save_or_report(what: &str, path: &str, body: &str) -> bool {
    match fs::write(path, body) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("JTokens could not write the {what} to {path}: {e}");
            false
        }
    }
}

fn build_json(files: &[Arc<FileData>], st: &ScanStats) -> Totals {
    let titles = codex_titles();
    let mut models: Vec<(Src, String)> = Vec::new();
    let mut model_idx: HashMap<(Src, String), usize> = HashMap::new();
    let mut projects: Vec<String> = Vec::new();
    let mut proj_idx: HashMap<String, usize> = HashMap::new();
    struct Sess {
        src: Src,
        id: String,
        proj: usize,
        title: String,
        first: u32,
        last: u32,
    }
    let mut sessions: Vec<Sess> = Vec::new();
    let mut sess_idx: HashMap<(Src, String), usize> = HashMap::new();
    let mut rows: HashMap<(usize, usize, u32), Agg> = HashMap::new(); // (model, session, hour)
    // Keyed by harness too, so an id from one tool can never drop an event from another.
    let mut seen: HashSet<(u8, u64)> = HashSet::new();
    let mut events = 0usize;
    let (mut tcost, mut ttok) = (0.0f64, 0u64);
    let mut recent = alerts::Recent::default();
    let now_secs = std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

    // Oldest first, by first event: an original event is always older than a
    // replayed copy in a forked session, so dedupe keeps the original.
    let mut order: Vec<&Arc<FileData>> = files.iter().collect();
    order.sort_by_key(|f| (f.events.iter().map(|e| e.ts).min().unwrap_or(u32::MAX), f.mtime));

    for fd in order {
        if fd.events.is_empty() {
            continue;
        }
        let proj_name = if fd.cwd.is_empty() { "(unknown)".to_string() } else { fd.cwd.clone() };
        let p = *proj_idx.entry(proj_name.clone()).or_insert_with(|| {
            projects.push(proj_name);
            projects.len() - 1
        });
        let skey = (fd.src, fd.session.clone());
        let s = match sess_idx.get(&skey) {
            Some(&s) => s,
            None => {
                let title = if fd.src == Src::Codex { titles.get(&fd.session).cloned().unwrap_or_default() } else { String::new() };
                sessions.push(Sess { src: fd.src, id: fd.session.clone(), proj: p, title, first: u32::MAX, last: 0 });
                sess_idx.insert(skey, sessions.len() - 1);
                sessions.len() - 1
            }
        };
        if !fd.title.is_empty() && sessions[s].title.is_empty() {
            sessions[s].title = fd.title.clone();
        }
        let local_models: Vec<(usize, Option<&prices::Price>)> = fd
            .models
            .iter()
            .map(|m| {
                let key = (fd.src, m.clone());
                let i = *model_idx.entry(key.clone()).or_insert_with(|| {
                    models.push(key);
                    models.len() - 1
                });
                (i, prices::lookup(m))
            })
            .collect();
        for e in &fd.events {
            if e.id != 0 && !seen.insert((fd.src as u8, e.id)) {
                continue;
            }
            events += 1;
            let (mi, price) = local_models[e.model as usize];
            let u = prices::Use {
                input: e.input,
                cache_w5m: e.cache_w5m,
                cache_w1h: e.cache_w1h,
                cache_read: e.cache_read,
                output: e.output,
                fast: e.fast,
                ts: e.ts as u64,
            };
            let cost = price.map(|p| prices::cost(p, &u)).unwrap_or(0.0);
            let a = rows.entry((mi, s, e.ts / 3600)).or_default();
            a.input += e.input;
            a.output += e.output;
            a.cache_read += e.cache_read;
            a.cache_write += e.cache_w5m + e.cache_w1h;
            a.cost += cost;
            a.reqs += 1;
            tcost += cost;
            let etok = e.input + e.output + e.cache_read + e.cache_w5m + e.cache_w1h;
            ttok += etok;
            if e.ts as u64 + 3600 >= now_secs {
                recent.c1 += cost;
                recent.t1 += etok;
                recent.r1 += 1;
            }
            if e.ts as u64 + 86400 >= now_secs {
                recent.c24 += cost;
                recent.t24 += etok;
            }
            let ss = &mut sessions[s];
            ss.first = ss.first.min(e.ts);
            ss.last = ss.last.max(e.ts);
        }
    }

    let mut j = String::with_capacity(rows.len() * 48 + 65536);
    let _ = write!(
        j,
        "{{\"scan\":{{\"ms\":{:.1},\"files\":{},\"parsed\":{},\"bytes\":{},\"parsedBytes\":{},\"events\":{},\"avx2\":{}}},",
        st.ms,
        st.files,
        st.parsed,
        st.bytes,
        st.parsed_bytes,
        events,
        unsafe { tk_has_avx2() } != 0
    );
    let _ = write!(j, "\"pricesChecked\":\"{}\",\"priceSources\":[", prices::CHECKED);
    for (i, s) in prices::SOURCES.iter().enumerate() {
        if i > 0 {
            j.push(',');
        }
        esc(s, &mut j);
    }
    j.push_str("],\"harnesses\":[");
    for (i, n) in HARNESS_NAMES.iter().enumerate() {
        if i > 0 {
            j.push(',');
        }
        esc(n, &mut j);
    }
    j.push_str("],\"models\":[");
    // The same model under two harnesses gets the harness name on its label, so the
    // legend and the By model list can tell the two apart.
    let base_name = |m: &str| -> String { prices::lookup(m).map(|p| p.name.to_string()).unwrap_or_else(|| m.to_string()) };
    // a suffix only when two harnesses share the name; two ids inside one harness do not need it
    let mut name_uses: HashMap<String, HashSet<u8>> = HashMap::new();
    for (src, m) in models.iter() {
        name_uses.entry(base_name(m.as_str())).or_default().insert(*src as u8);
    }
    for (i, (src, m)) in models.iter().enumerate() {
        if i > 0 {
            j.push(',');
        }
        let p = prices::lookup(m);
        let base = base_name(m.as_str());
        let shown = if name_uses.get(&base).map_or(false, |s| s.len() > 1) { format!("{base} · {}", HARNESS_NAMES[*src as usize]) } else { base };
        let _ = write!(j, "{{\"src\":{},\"id\":", *src as u8);
        esc(m, &mut j);
        j.push_str(",\"name\":");
        esc(&shown, &mut j);
        let _ = write!(j, ",\"priced\":{}}}", p.is_some());
    }
    j.push_str("],\"projects\":[");
    for (i, p) in projects.iter().enumerate() {
        if i > 0 {
            j.push(',');
        }
        esc(p, &mut j);
    }
    j.push_str("],\"sessions\":[");
    for (i, s) in sessions.iter().enumerate() {
        if i > 0 {
            j.push(',');
        }
        let _ = write!(j, "[{},", s.src as u8);
        esc(&s.id, &mut j);
        let _ = write!(j, ",{},", s.proj);
        esc(&s.title, &mut j);
        let _ = write!(j, ",{},{}]", if s.first == u32::MAX { 0 } else { s.first }, s.last);
    }
    j.push_str("],\"prices\":[");
    for (i, p) in prices::PRICES.iter().enumerate() {
        if i > 0 {
            j.push(',');
        }
        j.push('{');
        j.push_str("\"id\":");
        esc(p.id, &mut j);
        j.push_str(",\"name\":");
        esc(p.name, &mut j);
        let b = &p.base;
        let _ = write!(
            j,
            ",\"in\":{},\"w5\":{},\"w1h\":{},\"cr\":{},\"out\":{},\"longOver\":{},\"lin\":{},\"lcr\":{},\"lout\":{},\"fin\":{},\"fout\":{}}}",
            b.input, b.cache_w5m, b.cache_w1h, b.cache_read, b.output, p.long_over, p.long.input, p.long.cache_read, p.long.output, p.fast_in, p.fast_out
        );
    }
    // rows: [model, session, hour, input, output, cacheRead, cacheWrite, cost, reqs]
    j.push_str("],\"rows\":[");
    let mut first = true;
    let mut sorted: Vec<_> = rows.into_iter().collect();
    // hour first, then model and session, so the same data always gives the same text
    sorted.sort_unstable_by_key(|(k, _)| (k.2, k.0, k.1));
    for ((m, s, hr), a) in sorted {
        if !first {
            j.push(',');
        }
        first = false;
        let _ = write!(j, "[{},{},{},{},{},{},{},{:.6},{}]", m, s, hr, a.input, a.output, a.cache_read, a.cache_write, a.cost, a.reqs);
    }
    // the rolling windows the alert rules use, so the page can show the same numbers
    let _ = write!(j, "],\"recent\":{{\"c1\":{:.6},\"t1\":{},\"r1\":{},\"c24\":{:.6},\"t24\":{}}}}}", recent.c1, recent.t1, recent.r1, recent.c24, recent.t24);
    Totals { json: j, events, cost: tcost, tokens: ttok, recent }
}

// ---------------------------------------------------------------------------
// App

const UI: &str = include_str!("../ui/index.html");

fn attach_console() {
    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(pid: u32) -> i32;
    }
    unsafe {
        AttachConsole(u32::MAX);
    }
}

// --serve: a long-lived scanner for hosts like JWorkspace. Each "scan" line on
// stdin answers one line of the same JSON the window gets. The per-file cache
// stays warm between scans, so refreshes after the first are fast. Ends when
// stdin closes.
fn serve() {
    use std::io::{BufRead, Write};
    let mut sc = Scanner::default();
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim() != "scan" {
            continue;
        }
        let (files, st) = sc.scan();
        let t = build_json(&files, &st);
        if writeln!(out, "{}", t.json).and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}

const HELP: &str = "JTokens: token usage and API cost for Claude Code, Codex, OpenCode, Pi and Droid

  jtokens                                open the app
  jtokens --smoke --report r.json        scan twice with no window, write timings and totals
  jtokens --smoke-gui --report r.json    boot the real window hidden, scan, render, report, exit
  jtokens --export out.html              write the dashboard with data baked in
  jtokens --json data.json               write the raw data the UI uses
  jtokens --serve                        scan on each `scan` line from stdin, answer JSON lines
  jtokens --check-update                 say whether a newer release is on GitHub
  jtokens --update [--no-launch]         install the newest release, then start it
  jtokens --toast-test                   show a test Windows notification
  jtokens --version                      print the version";

thread_local! {
    /// set on a scan thread: its panics are caught and shown in the page, not in a box
    static IN_SCAN: std::cell::Cell<bool> = std::cell::Cell::new(false);
}

/// Shows an error box. The release build has no console,
/// so without this a failure would close the app with no message.
fn error_box(msg: &str) {
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(h: isize, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }
    let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (t, c) = (w(msg), w("JTokens"));
    unsafe {
        MessageBoxW(0, t.as_ptr(), c.as_ptr(), 0x10); // MB_ICONERROR
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    // a flag's value is the next argument, unless that argument is another flag
    let value = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).filter(|a| !a.starts_with("--")).cloned();
    let quiet = flag("--serve") || flag("--smoke") || flag("--smoke-gui") || value("--export").is_some() || value("--json").is_some();
    if !quiet {
        std::panic::set_hook(Box::new(|info| if IN_SCAN.with(|c| c.get()) {
            // the page says "Scan failed"; keep the cause so it can be reported.
            // thread::scope re-raises a worker panic with this generic message; the worker already wrote the real one
            let p = info.payload();
            if p.downcast_ref::<&str>().copied().or_else(|| p.downcast_ref::<String>().map(|s| s.as_str())) == Some("a scoped thread panicked") {
                return;
            }
            let _ = fs::create_dir_all(data_dir());
            let _ = fs::write(data_dir().join("scan-error.txt"), format!("JTokens {} scan failed:\n{info}\n", env!("CARGO_PKG_VERSION")));
        } else { error_box(&format!("JTokens stopped because of an error.

{info}")) }));
    }

    if flag("--version") || flag("-V") {
        attach_console();
        println!("jtokens {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if flag("--check-update") || flag("--update") {
        attach_console();
        if let Err(e) = update::cli(flag("--update"), !flag("--no-launch")) {
            println!("{e}");
            std::process::exit(1);
        }
        return;
    }
    if flag("--help") || flag("-h") {
        attach_console();
        println!("{HELP}");
        return;
    }
    unsafe { tk_init() };

    if flag("--serve") {
        serve();
        return;
    }
    if flag("--toast-test") {
        attach_console();
        match alerts::Hub::toast_test() {
            Ok(()) => println!("JTokens test notification sent."),
            Err(e) => {
                eprintln!("JTokens could not show the notification: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // a file flag with no path is an error, not a silent run of the window
    for f in ["--export", "--json"] {
        if flag(f) && value(f).is_none() {
            attach_console();
            eprintln!("JTokens: {f} needs a file path");
            std::process::exit(2);
        }
    }
    if flag("--smoke") || value("--export").is_some() || value("--json").is_some() {
        attach_console();
        let mut sc = Scanner::default();
        let (files, st) = sc.scan();
        let t = build_json(&files, &st);
        let (files2, st2) = sc.scan();
        let tj0 = Instant::now();
        let _warm = build_json(&files2, &st2);
        let tj = tj0.elapsed().as_secs_f64() * 1000.0;
        // the two scans must give the same data; the timings in the scan stats are left out of the comparison
        let warm_same = build_json(&files, &ScanStats::default()).json == build_json(&files2, &ScanStats::default()).json;
        // per-provider timing detail for the cold scan and the warm one
        let detail = |s: &ScanStats| {
            let mut d = String::new();
            for (i, ms) in s.by_src_ms.iter().enumerate() {
                if i > 0 {
                    d.push(',');
                }
                let _ = write!(d, "\"{}\":{:.0}", HARNESS_NAMES[i], ms);
            }
            format!("{{\"walkMs\":{:.0},\"openCodeMs\":{:.0},\"devinMs\":{:.0},\"bySrcMs\":{{{d}}}}}", s.walk_ms, s.opencode_ms, s.devin_ms)
        };
        let report = format!(
            "{{\"coldScanMs\":{:.1},\"warmScanMs\":{:.1},\"warmParsed\":{},\"jsonMs\":{:.1},\"files\":{},\"gb\":{:.2},\"events\":{},\"tokens\":{},\"costUsd\":{:.2},\"avx2\":{},\"warmSame\":{},\"cold\":{},\"warm\":{}}}",
            st.ms,
            st2.ms,
            st2.parsed,
            tj,
            st.files,
            st.bytes as f64 / 1e9,
            t.events,
            t.tokens,
            t.cost,
            unsafe { tk_has_avx2() } != 0,
            warm_same,
            detail(&st),
            detail(&st2),
        );
        let mut saved = true;
        if let Some(p) = value("--json") {
            saved &= save_or_report("JSON data", &p, &t.json);
        }
        if let Some(p) = value("--export") {
            let html = UI.replace("<!--DATA-->", &format!("<script>window.__TK_STATIC={};</script>", t.json));
            saved &= save_or_report("dashboard", &p, &html);
        }
        if let Some(p) = value("--report") {
            saved &= save_or_report("report", &p, &report);
        }
        println!("{report}");
        if !saved {
            std::process::exit(1);
        }
        return;
    }
    run_gui(flag("--smoke-gui"), value("--report"));
}

enum UserEvent {
    Data(String),
    Rendered(String),
    Update(String),
    Alerts(String),
    Fullscreen(bool),
    ScanFailed,
    Exit,
}

fn run_gui(smoke: bool, report: Option<String>) {
    use tao::dpi::LogicalSize;
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::window::WindowBuilder;
    use wry::WebViewBuilder;

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    // 1360x900, or smaller when the screen (after scaling) has less room; room is left for the taskbar and title bar
    let fit = event_loop
        .primary_monitor()
        .map(|m| {
            let s = m.size().to_logical::<f64>(m.scale_factor());
            LogicalSize::new((s.width - 40.0).clamp(560.0, 1360.0), (s.height - 100.0).clamp(360.0, 900.0))
        })
        .unwrap_or(LogicalSize::new(1360.0, 900.0));
    let window = match WindowBuilder::new()
        .with_title(format!("JTokens {}", env!("CARGO_PKG_VERSION")))
        .with_inner_size(fit)
        .with_min_inner_size(LogicalSize::new(fit.width.min(720.0), fit.height.min(480.0)))
        .with_visible(!smoke)
        .with_window_icon(<tao::window::Icon as tao::platform::windows::IconExtWindows>::from_resource(1, None).ok())
        .build(&event_loop)
    {
        Ok(w) => w,
        Err(e) => return fail(smoke, &report, &format!("Could not open the window: {e}")),
    };

    // Updates: only the real window checks, never the hidden smoke test.
    let update_proxy = proxy.clone();
    let updater = update::Updater::new(Arc::new(move |s: &update::State| {
        let _ = update_proxy.send_event(UserEvent::Update(s.json()));
    }));
    if !smoke {
        update::cleanup_after_update();
    }

    let scanner = Arc::new(Mutex::new(Scanner::default()));
    let hub = Arc::new(alerts::Hub::new(proxy.clone(), {
        use tao::platform::windows::WindowExtWindows;
        window.hwnd() as isize
    }, !smoke));
    if !smoke {
        let (h, sc, p) = (hub.clone(), scanner.clone(), proxy.clone());
        std::thread::spawn(move || alerts::watch(h, sc, p));
    }
    let busy = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ipc_proxy = proxy.clone();
    let ipc_hub = hub.clone();
    let log_path = report.as_ref().map(|r| format!("{r}.log"));
    // WebView2 keeps its profile next to the exe by default, which fails when
    // JTokens sits in a folder the user can't write to (Program Files).
    let mut web_context = wry::WebContext::new(Some(data_dir().join("WebView2")));
    let webview = match WebViewBuilder::new_with_web_context(&mut web_context)
        .with_html(UI.replacen("data-theme=\"dark\"", &format!("data-theme=\"{}\"", load_theme()), 1))
        .with_background_color(if load_theme() == "light" { (243, 243, 241, 255) } else { (17, 17, 16, 255) })
        .with_devtools(cfg!(debug_assertions))
        .with_ipc_handler(move |req| {
            let body = req.body();
            if body == "scan" {
                if busy.swap(true, Ordering::SeqCst) {
                    return;
                }
                let sc = scanner.clone();
                let p = ipc_proxy.clone();
                let busy = busy.clone();
                let h = ipc_hub.clone();
                std::thread::spawn(move || {
                    // a panic in one scan must not leave Refresh and Live stuck
                    IN_SCAN.with(|c| c.set(true));
                    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let mut g = sc.lock().unwrap_or_else(|e| e.into_inner());
                        let (files, st) = g.scan();
                        let t = build_json(&files, &st);
                        (t.json, t.recent)
                    }));
                    busy.store(false, Ordering::SeqCst);
                    match res {
                        Ok((json, recent)) => {
                            let _ = p.send_event(UserEvent::Data(json));
                            h.eval(&recent);
                        }
                        Err(_) => {
                            let _ = p.send_event(UserEvent::ScanFailed);
                        }
                    }
                });
            } else if let Some(rest) = body.strip_prefix("rendered:") {
                let _ = ipc_proxy.send_event(UserEvent::Rendered(rest.to_string()));
            } else if let Some(t) = body.strip_prefix("theme:") {
                save_theme(t);
            } else if let Some(msg) = body.strip_prefix("log:") {
                if let Some(p) = &log_path {
                    use std::io::Write;
                    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(p) {
                        let _ = writeln!(f, "{msg}");
                    }
                }
            } else if body == "alerts:get" {
                let _ = ipc_proxy.send_event(UserEvent::Alerts(ipc_hub.json()));
            } else if let Some(cfg) = body.strip_prefix("alertcfg:") {
                ipc_hub.set_from_json(cfg);
            } else if body == "alerttest" {
                let h = ipc_hub.clone();
                std::thread::spawn(move || h.test());
            } else if body == "update:check" {
                updater.check();
            } else if body == "update:install" {
                let p = ipc_proxy.clone();
                updater.install(move || {
                    let _ = p.send_event(UserEvent::Exit);
                });
            } else if body == "update:ready" {
                // The page is loaded: start checking (not in the hidden smoke test).
                if !smoke && matches!(updater.state(), update::State::Idle) {
                    updater.start_timer();
                } else {
                    let _ = ipc_proxy.send_event(UserEvent::Update(updater.state().json()));
                }
            } else if let Some(on) = body.strip_prefix("fullscreen:") {
                let _ = ipc_proxy.send_event(UserEvent::Fullscreen(on == "on"));
            } else if let Some(url) = body.strip_prefix("open:") {
                if url.starts_with("https://") {
                    open_url(url);
                }
            }
        })
        .build(&window)
    {
        Ok(w) => w,
        Err(e) => {
            let msg = format!(
                "JTokens needs the Microsoft Edge WebView2 Runtime, and it could not start.

Install it from https://developer.microsoft.com/microsoft-edge/webview2/ and open JTokens again.

Details: {e}"
            );
            return fail(smoke, &report, &msg);
        }
    };

    let started = Instant::now();
    event_loop.run(move |event, _, cf| {
        *cf = if smoke { ControlFlow::WaitUntil(Instant::now() + std::time::Duration::from_millis(250)) } else { ControlFlow::Wait };
        match event {
            Event::UserEvent(UserEvent::Data(json)) => {
                let mut js = String::with_capacity(json.len() + 32);
                js.push_str("window.tkData(");
                js.push_str(&json);
                js.push_str(");");
                let _ = webview.evaluate_script(&js);
            }
            Event::UserEvent(UserEvent::Update(json)) => {
                let _ = webview.evaluate_script(&format!("window.tkUpdate && window.tkUpdate({json});"));
            }
            Event::UserEvent(UserEvent::Alerts(json)) => {
                let _ = webview.evaluate_script(&format!("window.tkAlerts && window.tkAlerts({json});"));
            }
            Event::UserEvent(UserEvent::Exit) => *cf = ControlFlow::Exit,
            Event::UserEvent(UserEvent::Fullscreen(on)) => {
                window.set_fullscreen(if on { Some(tao::window::Fullscreen::Borderless(None)) } else { None });
                let _ = webview.evaluate_script(&format!("window.tkFullscreen && window.tkFullscreen({on});"));
            }
            Event::UserEvent(UserEvent::ScanFailed) => {
                let _ = webview.evaluate_script("window.tkScanError && window.tkScanError();");
            }
            Event::UserEvent(UserEvent::Rendered(info)) => {
                if smoke {
                    if let Some(p) = &report {
                        let _ = fs::write(p, &info);
                    }
                    *cf = ControlFlow::Exit;
                }
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => *cf = ControlFlow::Exit,
            _ => {}
        }
        if smoke && started.elapsed().as_secs() > 60 {
            if let Some(p) = &report {
                let _ = fs::write(p, "{\"error\":\"timeout\"}");
            }
            *cf = ControlFlow::Exit;
        }
    });
}

fn fail(smoke: bool, report: &Option<String>, msg: &str) {
    if smoke {
        if let Some(p) = report {
            let mut j = String::from("{\"error\":");
            esc(msg, &mut j);
            j.push('}');
            let _ = fs::write(p, j);
        }
    } else {
        error_box(msg);
    }
}

fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(home).join("JTokens")
}

fn settings_path() -> PathBuf {
    // tests point this at a temp file so the real settings are untouched
    #[cfg(test)]
    if let Ok(p) = std::env::var("JTOKENS_TEST_SETTINGS") {
        return PathBuf::from(p);
    }
    data_dir().join("settings.txt")
}

// settings.txt holds one key=value per line (theme, alert limits).
fn settings_read() -> Vec<(String, String)> {
    fs::read_to_string(settings_path())
        .map(|s| {
            s.lines()
                .filter_map(|l| l.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
                .filter(|(k, _)| !k.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn settings_get(key: &str) -> Option<String> {
    settings_read().into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn settings_set(key: &str, val: &str) {
    let mut lines = settings_read();
    match lines.iter_mut().find(|(k, _)| k == key) {
        Some(kv) => kv.1 = val.to_string(),
        None => lines.push((key.to_string(), val.to_string())),
    }
    let mut s = String::new();
    for (k, v) in lines {
        let _ = writeln!(s, "{k}={v}");
    }
    let p = settings_path();
    if let Some(d) = p.parent() {
        let _ = fs::create_dir_all(d);
    }
    let _ = fs::write(p, s);
}

fn load_theme() -> &'static str {
    match settings_get("theme").as_deref() {
        Some("light") => "light",
        _ => "dark",
    }
}

fn save_theme(t: &str) {
    settings_set("theme", if t == "light" { "light" } else { "dark" });
}

fn open_url(url: &str) {
    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(h: isize, op: *const u16, file: *const u16, params: *const u16, dir: *const u16, show: i32) -> isize;
    }
    let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (op, file) = (w("open"), w(url));
    unsafe {
        ShellExecuteW(0, op.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), 1);
    }
}
