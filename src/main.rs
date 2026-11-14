#![windows_subsystem = "windows"]

mod prices;
mod update;

use std::collections::{HashMap, HashSet};
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
                        // keep it simple: skip \uXXXX
                        i += 4;
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

// ---------------------------------------------------------------------------
// Parsed files

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Src {
    Claude = 0,
    Codex = 1,
}

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

fn parse_file(src: Src, path: &Path, session: String, size: u64, mtime: u64, buf: &mut Vec<u8>, recs: &mut Vec<Rec>) -> Option<FileData> {
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
fn walk(dir: &Path, out: &mut Vec<(PathBuf, u64, u64)>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(md) = e.metadata() else { continue };
        let p = e.path();
        if md.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            let mtime = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0);
            out.push((p, md.len(), mtime));
        }
    }
}

/// Walks each second-level directory on its own thread (Codex keeps one
/// folder per day, Claude one per project).
fn walk_parallel(root: &Path) -> Vec<(PathBuf, u64, u64)> {
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
                        } else if p2.extension().is_some_and(|x| x == "jsonl") {
                            let mt = md2.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0);
                            out.push((p2, md2.len(), mt));
                        }
                    }
                }
                Err(_) => continue,
            }
        } else if p.extension().is_some_and(|x| x == "jsonl") {
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
                let mut local = Vec::new();
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    if k >= dirs.len() {
                        break;
                    }
                    walk(&dirs[k], &mut local);
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
    if stem.len() >= 36 {
        stem[stem.len() - 36..].to_string()
    } else {
        stem
    }
}

#[derive(Default)]
struct ScanStats {
    files: usize,
    parsed: usize,
    bytes: u64,
    parsed_bytes: u64,
    ms: f64,
}

#[derive(Default)]
struct Scanner {
    cache: HashMap<PathBuf, Arc<FileData>>,
}

impl Scanner {
    fn scan(&mut self) -> (Vec<Arc<FileData>>, ScanStats) {
        let t0 = Instant::now();
        let h = home();
        let claude_dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| h.join(".claude"));
        let codex_dir = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| h.join(".codex"));
        let mut jobs = Vec::new();
        for (src, dir) in [
            (Src::Claude, claude_dir.join("projects")),
            (Src::Codex, codex_dir.join("sessions")),
            (Src::Codex, codex_dir.join("archived_sessions")),
        ] {
            let paths = walk_parallel(&dir);
            for (p, size, mtime) in paths {
                let session = if src == Src::Claude { claude_session(&p) } else { codex_session(&p) };
                jobs.push(Job { src, path: p, session, size, mtime });
            }
        }

        let mut stats = ScanStats { files: jobs.len(), ..Default::default() };
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
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    let mut buf = Vec::new();
                    let mut recs = Vec::new();
                    let mut local = Vec::new();
                    loop {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        if k >= todo.len() {
                            break;
                        }
                        let j = &jobs[todo[k]];
                        if let Some(fd) = parse_file(j.src, &j.path, j.session.clone(), j.size, j.mtime, &mut buf, &mut recs) {
                            local.push((todo[k], Arc::new(fd)));
                        }
                    }
                    parsed.lock().unwrap().extend(local);
                });
            }
        });
        for (i, fd) in parsed.into_inner().unwrap() {
            self.cache.insert(jobs[i].path.clone(), fd.clone());
            result[i] = Some(fd);
        }
        // forget files that disappeared
        let live: HashSet<&PathBuf> = jobs.iter().map(|j| &j.path).collect();
        self.cache.retain(|k, _| live.contains(k));
        stats.ms = t0.elapsed().as_secs_f64() * 1000.0;
        (result.into_iter().flatten().collect(), stats)
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
            m.insert(json_str_at(line, i + 6), json_str_at(line, t + 15));
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
    let mut seen: HashSet<u64> = HashSet::new();
    let mut events = 0usize;
    let (mut tcost, mut ttok) = (0.0f64, 0u64);

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
            if e.id != 0 && !seen.insert(e.id) {
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
            ttok += e.input + e.output + e.cache_read + e.cache_w5m + e.cache_w1h;
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
    j.push_str("],\"models\":[");
    for (i, (src, m)) in models.iter().enumerate() {
        if i > 0 {
            j.push(',');
        }
        let p = prices::lookup(m);
        let _ = write!(j, "{{\"src\":{},\"id\":", *src as u8);
        esc(m, &mut j);
        j.push_str(",\"name\":");
        esc(p.map(|p| p.name).unwrap_or(m), &mut j);
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
    sorted.sort_unstable_by_key(|(k, _)| k.2);
    for ((m, s, hr), a) in sorted {
        if !first {
            j.push(',');
        }
        first = false;
        let _ = write!(j, "[{},{},{},{},{},{},{},{:.6},{}]", m, s, hr, a.input, a.output, a.cache_read, a.cache_write, a.cost, a.reqs);
    }
    j.push_str("]}");
    Totals { json: j, events, cost: tcost, tokens: ttok }
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

const HELP: &str = "JTokens: Claude Code and Codex token usage and API cost

  jtokens                                open the app
  jtokens --smoke --report r.json        scan twice with no window, write timings and totals
  jtokens --smoke-gui --report r.json    boot the real window hidden, scan, render, report, exit
  jtokens --export out.html              write the dashboard with data baked in
  jtokens --json data.json               write the raw data the UI uses
  jtokens --serve                        scan on each `scan` line from stdin, answer JSON lines
  jtokens --check-update                 say whether a newer release is on GitHub
  jtokens --update [--no-launch]         install the newest release, then start it
  jtokens --version                      print the version";

/// Shows an error box. The release build has no console and aborts on panic,
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
    let value = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    let quiet = flag("--serve") || flag("--smoke") || flag("--smoke-gui") || value("--export").is_some() || value("--json").is_some();
    if !quiet {
        std::panic::set_hook(Box::new(|info| error_box(&format!("JTokens stopped because of an error.

{info}"))));
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

    if flag("--smoke") || value("--export").is_some() || value("--json").is_some() {
        attach_console();
        let mut sc = Scanner::default();
        let (files, st) = sc.scan();
        let t = build_json(&files, &st);
        let (files2, st2) = sc.scan();
        let tj0 = Instant::now();
        let t2 = build_json(&files2, &st2);
        let tj = tj0.elapsed().as_secs_f64() * 1000.0;
        let report = format!(
            "{{\"coldScanMs\":{:.1},\"warmScanMs\":{:.1},\"warmParsed\":{},\"jsonMs\":{:.1},\"files\":{},\"gb\":{:.2},\"events\":{},\"tokens\":{},\"costUsd\":{:.2},\"avx2\":{},\"warmSame\":{}}}",
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
            t.json.len() == t2.json.len()
        );
        if let Some(p) = value("--json") {
            let _ = fs::write(&p, &t.json);
        }
        if let Some(p) = value("--export") {
            let html = UI.replace("<!--DATA-->", &format!("<script>window.__TK_STATIC={};</script>", t.json));
            let _ = fs::write(&p, html);
        }
        if let Some(p) = value("--report") {
            let _ = fs::write(&p, &report);
        }
        println!("{report}");
        return;
    }
    run_gui(flag("--smoke-gui"), value("--report"));
}

enum UserEvent {
    Data(String),
    Rendered(String),
    Update(String),
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
    let window = match WindowBuilder::new()
        .with_title(format!("JTokens {}", env!("CARGO_PKG_VERSION")))
        .with_inner_size(LogicalSize::new(1360.0, 900.0))
        .with_min_inner_size(LogicalSize::new(720.0, 480.0))
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
    let busy = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ipc_proxy = proxy.clone();
    let log_path = report.as_ref().map(|r| format!("{r}.log"));
    // WebView2 keeps its profile next to the exe by default, which fails when
    // JTokens sits in a folder the user can't write to (Program Files).
    let mut web_context = wry::WebContext::new(Some(data_dir().join("WebView2")));
    let webview = match WebViewBuilder::new_with_web_context(&mut web_context)
        .with_html(UI.replacen("data-theme=\"dark\"", &format!("data-theme=\"{}\"", load_theme()), 1))
        .with_background_color((17, 17, 16, 255))
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
                std::thread::spawn(move || {
                    // a panic in one scan must not leave Refresh and Live stuck
                    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let mut g = sc.lock().unwrap_or_else(|e| e.into_inner());
                        let (files, st) = g.scan();
                        build_json(&files, &st).json
                    }));
                    busy.store(false, Ordering::SeqCst);
                    let _ = p.send_event(match res {
                        Ok(json) => UserEvent::Data(json),
                        Err(_) => UserEvent::ScanFailed,
                    });
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
    data_dir().join("settings.txt")
}

fn load_theme() -> &'static str {
    match fs::read_to_string(settings_path()) {
        Ok(s) if s.lines().any(|l| l.trim() == "theme=light") => "light",
        _ => "dark",
    }
}

fn save_theme(t: &str) {
    let p = settings_path();
    if let Some(d) = p.parent() {
        let _ = fs::create_dir_all(d);
    }
    let _ = fs::write(p, format!("theme={}
", if t == "light" { "light" } else { "dark" }));
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
