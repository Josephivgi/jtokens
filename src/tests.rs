// Unit tests for the readers and the aggregation. Each test writes its own
// small log files into a temp folder, so nothing depends on the real logs.
// Run with: cargo test --release --locked   (the release profile matches the shipped build)

use super::*;
use std::sync::Once;

static INIT: Once = Once::new();

fn init() {
    INIT.call_once(|| unsafe { tk_init() });
}

/// A fresh folder under the system temp dir, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        use std::sync::atomic::AtomicU32;
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("jtokens-test-{}-{}-{}", std::process::id(), tag, n));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("temp dir");
        TempDir(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, text).unwrap();
        p
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn ev_total(e: &Event) -> u64 {
    e.input + e.output + e.cache_read + e.cache_w5m + e.cache_w1h
}

// ---------------------------------------------------------------- helpers

#[test]
fn key_is_nonzero_stable_and_tells_inputs_apart() {
    let a = key(&[b"msg_1"]);
    assert_ne!(a, 0);
    assert_eq!(a, key(&[b"msg_1"]), "same input, same key");
    assert_ne!(a, key(&[b"msg_2"]));
    // parts are separated, so ("ab","c") and ("a","bc") do not collide
    assert_ne!(key(&[b"ab", b"c"]), key(&[b"a", b"bc"]));
}

#[test]
fn session_ids_come_from_the_file_names() {
    let uuid = "0195f0a2-1111-7222-8333-444455556666";
    let codex = PathBuf::from(format!(r"C:\x\rollout-2026-11-14T10-00-00-{uuid}.jsonl"));
    assert_eq!(codex_session(&codex), uuid);
    // Pi names its files <timestamp>_<session id>
    let pi = PathBuf::from(format!(r"C:\x\2026-11-14T10-00-00-000Z_{uuid}.jsonl"));
    assert_eq!(codex_session(&pi), uuid);
    let droid = PathBuf::from(format!(r"C:\x\-C-Users-test\{uuid}.settings.json"));
    assert_eq!(droid_session(&droid), uuid);
    let claude = PathBuf::from(format!(r"C:\x\proj\{uuid}\subagents\agent-1.jsonl"));
    assert_eq!(claude_session(&claude), uuid, "subagent logs belong to their parent session");
}

#[test]
fn log_filter_matches_by_file_name_suffix() {
    assert!(is_log(Path::new(r"C:\a\b.jsonl"), ".jsonl"));
    assert!(!is_log(Path::new(r"C:\a\b.jsonl.bak"), ".jsonl"));
    assert!(is_log(Path::new(r"C:\a\s.settings.json"), ".settings.json"));
    assert!(!is_log(Path::new(r"C:\a\s.settings.json.bak"), ".settings.json"));
    assert!(!is_log(Path::new(r"C:\a\s.json"), ".settings.json"));
}

#[test]
fn json_strings_are_unescaped() {
    let buf = br#"{"cwd":"C:\\Users\\Joe \"quoted\"","x":1}"#;
    let got = str_after(buf, b"\"cwd\":\"", buf.len()).unwrap();
    assert_eq!(got, r#"C:\Users\Joe "quoted""#);
}

#[test]
fn price_lookup_ignores_case_and_dated_or_tagged_ids() {
    let sol = prices::lookup("gpt-5.6-sol").expect("the id is in the price table");
    assert!(std::ptr::eq(prices::lookup("GPT-5.6-SOL").unwrap(), sol));
    let sonnet = prices::lookup("claude-sonnet-5-5").unwrap();
    assert!(std::ptr::eq(prices::lookup("claude-sonnet-5-5-20260101[1m]").unwrap(), sonnet));
    assert!(prices::lookup("no-such-model").is_none());
}

#[test]
fn json_unicode_escapes_become_characters() {
    // a plain escape, a surrogate pair, and a lone surrogate (replaced, not dropped)
    let buf = br#"Jos\u00e9 \ud83d\ude00 x\ud800y""#;
    assert_eq!(json_str_at(buf, 0), "Jos\u{e9} \u{1F600} x\u{FFFD}y");
}

#[test]
fn prices_match_dated_and_tagged_model_names() {
    assert_eq!(prices::lookup("claude-sonnet-5-5").unwrap().id, "claude-sonnet-5-5");
    assert_eq!(prices::lookup("claude-sonnet-5-5-20260101").unwrap().id, "claude-sonnet-5-5");
    assert_eq!(prices::lookup("claude-opus-5-5[1m]").unwrap().id, "claude-opus-5-5");
    assert!(prices::lookup("muse-spark-1.3-contributor").is_none(), "unpriced models have no price row");
    assert!(prices::lookup("deepseek-v4-flash-free").is_none(), "free tiers have no price row, so they cost 0");
}

#[test]
fn devin_effort_and_speed_tails_still_find_the_model_row() {
    // Devin's generation_model adds the effort and speed to the vendor's id
    let opus = prices::lookup("claude-opus-5-5").unwrap();
    assert!(std::ptr::eq(prices::lookup("claude-opus-5-5-medium").unwrap(), opus));
    assert!(std::ptr::eq(prices::lookup("claude-opus-5-5-medium-fast").unwrap(), opus));
    assert!(std::ptr::eq(prices::lookup("claude-fable-5-2-medium").unwrap(), prices::lookup("claude-fable-5-2").unwrap()));
    // and writes version numbers with dashes: gpt-6-1-sol-low is gpt-6.1-sol at low effort
    assert!(std::ptr::eq(prices::lookup("gpt-6-1-sol-low").unwrap(), prices::lookup("gpt-6.1-sol").unwrap()));
    // Cognition's own models peel the same way to their price rows
    let swe2 = prices::lookup("swe-2").unwrap();
    assert!(std::ptr::eq(prices::lookup("swe-2-high").unwrap(), swe2));
    assert!(std::ptr::eq(prices::lookup("swe-2-high-lightning").unwrap(), swe2));
    assert!(std::ptr::eq(prices::lookup("swe-2-max").unwrap(), swe2));
    let flash = prices::lookup("swe-2-flash").unwrap();
    assert!(std::ptr::eq(prices::lookup("swe-2-flash-medium").unwrap(), flash));
    assert!(std::ptr::eq(prices::lookup("swe-2-flash-medium-lightning").unwrap(), flash));
    assert!(prices::lookup("deepseek-v4-flash-free").is_none(), "a tail that is not a suffix word does not peel");
}

#[test]
fn cost_uses_list_prices_per_million_tokens() {
    let p = prices::lookup("claude-sonnet-5-5").unwrap(); // $2 in, $0.10 cache read, $10 out
    let one_million_input = prices::Use { input: 1_000_000, cache_w5m: 0, cache_w1h: 0, cache_read: 0, output: 0, fast: false, ts: 0 };
    assert!((prices::cost(p, &one_million_input) - 2.0).abs() < 1e-9);
    let mixed = prices::Use { input: 0, cache_w5m: 1_000_000, cache_w1h: 0, cache_read: 1_000_000, output: 1_000_000, fast: false, ts: 0 };
    // cache write (5 minute) is 1.25x input, cache read is 0.10, output is 10
    assert!((prices::cost(p, &mixed) - (2.5 + 0.10 + 10.0)).abs() < 1e-9);
    // SWE-2: $3 in, $0.30 cache read, $15 out; cache write is at the input price
    let swe = prices::lookup("swe-2").unwrap();
    assert!((prices::cost(swe, &prices::Use { input: 1_000_000, cache_w5m: 1_000_000, cache_w1h: 0, cache_read: 1_000_000, output: 1_000_000, fast: false, ts: 0 }) - (3.0 + 3.0 + 0.30 + 15.0)).abs() < 1e-9);
    // lightning doubles it
    assert!((prices::cost(swe, &prices::Use { input: 1_000_000, cache_w5m: 0, cache_w1h: 0, cache_read: 0, output: 0, fast: true, ts: 0 }) - 6.0).abs() < 1e-9);
    // SWE-2 Flash: $0.10 in, $0.125 write, $0.01 read, $0.50 out
    let flash = prices::lookup("swe-2-flash").unwrap();
    assert!((prices::cost(flash, &prices::Use { input: 1_000_000, cache_w5m: 1_000_000, cache_w1h: 0, cache_read: 1_000_000, output: 1_000_000, fast: false, ts: 0 }) - (0.10 + 0.125 + 0.01 + 0.50)).abs() < 1e-9);
}

// ---------------------------------------------------------------- readers

#[test]
fn claude_lines_are_read_by_the_assembly_scanner() {
    init();
    let t = TempDir::new("claude");
    let line = r#"{"type":"assistant","timestamp":"2026-11-14T10:00:00.000Z","cwd":"C:\\Users\\t\\p","message":{"role":"assistant","model":"claude-sonnet-5-5","id":"msg_a","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":1000,"cache_creation_input_tokens":200}}}"#;
    let p = t.write("proj/s1.jsonl", &format!("{line}\n{line}\n"));
    let mut buf = Vec::new();
    let mut recs = Vec::new();
    let fd = parse_file(Src::Claude, &p, "s1".into(), 0, 0, &mut buf, &mut recs).expect("parsed");
    assert_eq!(fd.cwd, r"C:\Users\t\p");
    assert_eq!(fd.events.len(), 2, "two lines, two events (the id dedupe happens later)");
    let e = &fd.events[0];
    assert_eq!((e.input, e.output, e.cache_read, e.cache_w5m), (100, 50, 1000, 200));
    assert_eq!(fd.models.len(), 1);
    assert_eq!(fd.models[e.model as usize], "claude-sonnet-5-5");
}

#[test]
fn pi_reads_usage_title_and_folder_and_skips_zero_usage() {
    let t = TempDir::new("pi");
    let text = [
        r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-11-14T10:00:00.000Z","cwd":"C:\\Users\\t\\pi-proj"}"#,
        r#"{"type":"session_info","id":"i1","name":"First name"}"#,
        r#"{"type":"session_info","id":"i2","name":"Renamed"}"#,
        r#"{"type":"message","id":"m1","message":{"role":"user","content":"hi"}}"#,
        r#"{"type":"message","id":"m2","message":{"role":"assistant","provider":"opencode-go","model":"deepseek-v4.1-flash","usage":{"input":1000,"output":200,"cacheRead":5000,"cacheWrite":300,"totalTokens":6500},"timestamp":1794621600000}}"#,
        r#"{"type":"message","id":"m3","message":{"role":"assistant","model":"deepseek-v4.1-flash","stopReason":"error","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0},"timestamp":1794621601000}}"#,
        r#"{"type":"message","message":{"role":"assistant","usage":{"input":7,"output":1},"timestamp":1794621602000}}"#,
    ]
    .join("\n");
    let p = t.write("s1.jsonl", &text);
    let mut buf = Vec::new();
    let fd = parse_pi(&p, "s1".into(), 1, 2, &mut buf).expect("parsed");
    assert_eq!(fd.cwd, r"C:\Users\t\pi-proj");
    assert_eq!(fd.title, "Renamed", "the last session name wins");
    assert_eq!(fd.events.len(), 2, "the zero-usage error is not a request");
    let e = &fd.events[0];
    assert_eq!((e.input, e.output, e.cache_read, e.cache_w5m), (1000, 200, 5000, 300));
    assert_eq!(ev_total(e), 6500, "matches the totalTokens the log reports");
    assert_eq!(e.ts, 1_794_621_600, "milliseconds become seconds");
    assert_eq!(fd.models[e.model as usize], "deepseek-v4.1-flash");
    assert_ne!(e.id, 0);
    // a message with no id is still counted, but cannot be deduplicated
    assert_eq!(fd.events[1].id, 0);
    assert_eq!(fd.models[fd.events[1].model as usize], "unknown");
}

#[test]
fn pi_keeps_a_line_whose_model_or_count_has_an_odd_type() {
    // a numeric model, a text timestamp and fractional counts must not drop the usage
    let t = TempDir::new("pi-odd-types");
    let text = "{\"type\":\"message\",\"id\":\"odd\",\"message\":{\"model\":42,\"timestamp\":\"soon\",\"usage\":{\"input\":5.0,\"output\":1.5,\"cacheRead\":-3}}}\n";
    let p = t.write("s.jsonl", text);
    let mut buf = Vec::new();
    let fd = parse_pi(&p, "s".into(), 1, 2, &mut buf).expect("parsed, not rejected");
    assert_eq!(fd.events.len(), 1);
    let e = &fd.events[0];
    assert_eq!((e.input, e.output, e.cache_read), (5, 1, 0));
    assert_eq!(fd.models[e.model as usize], "unknown");
}

#[test]
fn pi_skips_a_damaged_line_and_keeps_the_rest() {
    let t = TempDir::new("pi-bad");
    let text = "{\"type\":\"message\",\"id\":\"x\",\"message\":{\"usage\":{\"input\":\n\
                {\"type\":\"message\",\"id\":\"ok\",\"message\":{\"model\":\"m\",\"usage\":{\"input\":5,\"output\":1},\"timestamp\":1000}}\n";
    let p = t.write("s.jsonl", text);
    let mut buf = Vec::new();
    let fd = parse_pi(&p, "s".into(), 1, 2, &mut buf).expect("parsed");
    assert_eq!(fd.events.len(), 1);
    assert_eq!(fd.events[0].input, 5);
}

#[test]
fn droid_totals_come_from_settings_and_folder_from_the_session_log() {
    let t = TempDir::new("droid");
    let settings = t.write(
        "-C-Users-t/abc.settings.json",
        r#"{"model":"gpt-6-sol","tokenUsage":{"inputTokens":800,"outputTokens":120,"cacheReadTokens":3000,"cacheCreationTokens":40,"thinkingTokens":9},"inclusiveTokenUsage":{"inputTokens":999999}}"#,
    );
    t.write("-C-Users-t/abc.jsonl", "{\"type\":\"session_start\",\"id\":\"abc\",\"cwd\":\"C:\\\\Users\\\\t\\\\droid-proj\"}\n");
    let fd = parse_droid(&settings, "abc".into(), 1, 1_794_621_600_000_000_000).expect("parsed");
    assert_eq!(fd.cwd, r"C:\Users\t\droid-proj");
    assert_eq!(fd.events.len(), 1);
    let e = &fd.events[0];
    assert_eq!((e.input, e.output, e.cache_read, e.cache_w5m), (800, 120, 3000, 40));
    assert_eq!(e.ts, 1_794_621_600, "the file's last-saved time");
    assert_eq!(fd.models[e.model as usize], "gpt-6-sol");
    // the inclusive (child session) total is not added, so nothing is counted twice
    assert_eq!(ev_total(e), 800 + 120 + 3000 + 40);
}

#[test]
fn droid_session_with_no_usage_gives_no_events_and_a_broken_file_gives_none() {
    let t = TempDir::new("droid-empty");
    let empty = t.write("-C-Users-t/zero.settings.json", r#"{"model":"auto","tokenUsage":{"inputTokens":0}}"#);
    assert!(parse_droid(&empty, "zero".into(), 1, 1).unwrap().events.is_empty());
    let broken = t.write("-C-Users-t/half.settings.json", r#"{"model":"auto","tokenUsage":{"inpu"#);
    assert!(parse_droid(&broken, "half".into(), 1, 1).is_none(), "a file still being written is skipped, not guessed");
}

#[test]
fn a_droid_model_that_is_not_text_still_counts_its_tokens() {
    let t = TempDir::new("droid-model-type");
    let settings = t.write("-C-Users-t/num.settings.json", r#"{"model":42,"tokenUsage":{"inputTokens":800,"outputTokens":120}}"#);
    let fd = parse_droid(&settings, "num".into(), 1, 1_794_621_600_000_000_000).expect("parsed, not rejected");
    assert_eq!(fd.events.len(), 1);
    assert_eq!(ev_total(&fd.events[0]), 920);
    assert_eq!(fd.models[fd.events[0].model as usize], "unknown");
}

#[test]
fn opencode_keeps_a_row_with_a_missing_title_and_reads_text_counts() {
    let t = TempDir::new("opencode-odd");
    let db = t.path().join("opencode.db");
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            r#"CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT);
               CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, data TEXT NOT NULL);
               INSERT INTO session VALUES ('s1', NULL, NULL);
               INSERT INTO message VALUES ('m1', 's1', 1794621600000, '{"role":"assistant","modelID":"gpt-5.6-sol","tokens":{"input":"500","output":2.5}}');"#,
        )
        .unwrap();
    }
    let mut st = DbScan::default();
    scan_opencode(&db, &mut st).expect("readable database, so not an error");
    let files = st.files;
    assert_eq!(files.len(), 1, "the row with no title is still a session");
    let e = &files[0].events[0];
    assert_eq!((e.input, e.output), (500, 2), "numeric text is read; a fraction is cut to a whole count");
}

#[test]
fn opencode_skips_a_message_of_the_wrong_type_and_keeps_the_rest() {
    let t = TempDir::new("opencode-wrong-type");
    let db = t.path().join("opencode.db");
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            r#"CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT);
               CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, data TEXT NOT NULL);
               INSERT INTO session VALUES ('s1', '/p', 't');
               INSERT INTO message VALUES ('m1', 's1', 1794621600000, '{"role":"assistant","modelID":"gpt-5.6-sol","tokens":{"input":500,"output":90}}');
               INSERT INTO message VALUES ('m2', 's1', 'not a time', '{"role":"assistant","modelID":"gpt-5.6-sol","tokens":{"input":7}}');"#,
        )
        .unwrap();
    }
    let mut st = DbScan::default();
    scan_opencode(&db, &mut st).expect("the read itself succeeded");
    let files = st.files;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].events.len(), 1, "the message with a text time is skipped, the good one is kept");
    assert_eq!((files[0].events[0].input, files[0].events[0].output), (500, 90));
}

#[test]
fn counts_are_capped_so_one_bad_value_cannot_inflate_totals() {
    let t = TempDir::new("opencode-cap");
    let db = t.path().join("opencode.db");
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            r#"CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT);
               CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, data TEXT NOT NULL);
               INSERT INTO session VALUES ('s1', '/p', 't');
               INSERT INTO message VALUES ('m1', 's1', 1794621600000, '{"role":"assistant","modelID":"gpt-5.6-sol","tokens":{"input":99999999999999,"output":3}}');"#,
        )
        .unwrap();
    }
    let mut st = DbScan::default();
    scan_opencode(&db, &mut st).expect("readable");
    let files = st.files;
    assert_eq!(files[0].events[0].input, 1_000_000_000_000);
    assert_eq!(files[0].events[0].output, 3);
}

#[test]
fn pi_counts_are_capped_at_a_trillion() {
    let t = TempDir::new("pi-cap");
    let text = "{\"type\":\"message\",\"id\":\"big\",\"message\":{\"model\":\"m\",\"usage\":{\"input\":1e13,\"output\":4}}}\n";
    let p = t.write("big.jsonl", text);
    let mut buf = Vec::new();
    let fd = parse_pi(&p, "big".into(), 1, 2, &mut buf).expect("parsed");
    assert_eq!(fd.events[0].input, 1_000_000_000_000);
    assert_eq!(fd.events[0].output, 4);
}

#[test]
fn opencode_reads_assistant_usage_per_session_with_reasoning_added_to_output() {
    let t = TempDir::new("opencode");
    let db = t.path().join("opencode.db");
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL, title TEXT NOT NULL);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, data TEXT NOT NULL);",
        )
        .unwrap();
        c.execute("INSERT INTO session VALUES ('ses_1', 'C:/Users/t/oc-proj', 'Fixture title')", []).unwrap();
        c.execute("INSERT INTO session VALUES ('ses_2', 'C:/Users/t/other', 'Second')", []).unwrap();
        let asst = r#"{"role":"assistant","modelID":"gpt-5.6-sol","providerID":"openai","tokens":{"input":700,"output":90,"reasoning":30,"cache":{"read":1500,"write":5}}}"#;
        c.execute("INSERT INTO message VALUES ('u1', 'ses_1', 1794621600000, '{\"role\":\"user\"}')", []).unwrap();
        c.execute("INSERT INTO message VALUES ('m1', 'ses_1', 1794621601000, ?1)", [asst]).unwrap();
        c.execute("INSERT INTO message VALUES ('m2', 'ses_1', 1794621602000, ?1)", [asst]).unwrap();
        c.execute("INSERT INTO message VALUES ('m3', 'ses_2', 1794621603000, '{\"role\":\"assistant\",\"modelID\":\"x\",\"tokens\":{\"input\":0,\"output\":0,\"reasoning\":0,\"cache\":{\"read\":0,\"write\":0}}}')", []).unwrap();
    }
    let mut st = DbScan::default();
    scan_opencode(&db, &mut st).expect("readable database");
    let files = st.files;
    assert_eq!(files.len(), 1, "a session with no usage is left out");
    let fd = &files[0];
    assert_eq!(fd.session, "ses_1");
    assert_eq!(fd.cwd, r"C:\Users\t\oc-proj");
    assert_eq!(fd.title, "Fixture title");
    assert_eq!(fd.events.len(), 2, "the user message is not a request");
    let e = &fd.events[0];
    assert_eq!((e.input, e.output, e.cache_read, e.cache_w5m), (700, 120, 1500, 5));
    assert_eq!(e.ts, 1_794_621_601);
    assert_ne!(fd.events[0].id, fd.events[1].id, "different messages have different ids");
}

#[test]
fn opencode_database_that_is_missing_gives_nothing() {
    let t = TempDir::new("opencode-none");
    // a database that is not there cannot be read, so the caller keeps its last numbers
    assert!(scan_opencode(&t.path().join("nope.db"), &mut DbScan::default()).is_none());
}

// ---------------------------------------------------------------- aggregation

fn one_event_file(src: Src, session: &str, id: u64, input: u64) -> Arc<FileData> {
    let mut fd = FileData { src, size: 0, mtime: 0, session: session.into(), cwd: r"C:\p".into(), title: String::new(), models: vec!["claude-sonnet-5-5".into()], events: Vec::new() };
    fd.events.push(Event { ts: 1_794_621_600, model: 0, input, output: 0, cache_read: 0, cache_w5m: 0, cache_w1h: 0, id, fast: false });
    Arc::new(fd)
}

#[test]
fn a_record_copied_into_two_files_is_counted_once() {
    // a forked session replays its parent's records with the same ids
    let files = vec![one_event_file(Src::Pi, "a", 42, 100), one_event_file(Src::Pi, "b", 42, 100)];
    let st = ScanStats::default();
    let t = build_json(&mut AggCache::default(), &files, &st);
    assert_eq!(t.events, 1);
    assert_eq!(t.tokens, 100);
}

#[test]
fn the_same_id_from_two_harnesses_is_counted_twice() {
    // ids are only unique within one tool, so a clash across tools must not drop an event
    let files = vec![one_event_file(Src::Pi, "a", 42, 100), one_event_file(Src::Droid, "b", 42, 100)];
    let t = build_json(&mut AggCache::default(), &files, &ScanStats::default());
    assert_eq!(t.events, 2);
    assert_eq!(t.tokens, 200);
}

#[test]
fn a_model_used_by_two_harnesses_gets_the_harness_on_its_label() {
    let mut a = one_event_file(Src::Pi, "a", 1, 10);
    let mut b = one_event_file(Src::Droid, "b", 2, 10);
    Arc::get_mut(&mut a).unwrap().models = vec!["claude-sonnet-5-5".into()];
    Arc::get_mut(&mut b).unwrap().models = vec!["claude-sonnet-5-5".into()];
    let t = build_json(&mut AggCache::default(), &[a, b], &ScanStats::default());
    assert!(t.json.contains("· Pi\""), "{}", t.json.len());
    assert!(t.json.contains("· Droid\""));
}

#[test]
fn a_model_used_by_one_harness_keeps_its_plain_label() {
    let t = build_json(&mut AggCache::default(), &[one_event_file(Src::Pi, "a", 1, 10)], &ScanStats::default());
    assert!(!t.json.contains("· Pi"));
}

#[test]
fn two_ids_of_one_model_inside_one_harness_get_no_harness_suffix() {
    let mut a = one_event_file(Src::Pi, "a", 1, 10);
    Arc::get_mut(&mut a).unwrap().models = vec!["claude-sonnet-5-5".into(), "claude-sonnet-5-5-20260101".into()];
    let t = build_json(&mut AggCache::default(), &[a], &ScanStats::default());
    assert!(!t.json.contains("· Pi"), "one harness needs no suffix");
}

#[test]
fn records_without_an_id_are_never_merged() {
    let files = vec![one_event_file(Src::Pi, "a", 0, 100), one_event_file(Src::Pi, "b", 0, 100)];
    let t = build_json(&mut AggCache::default(), &files, &ScanStats::default());
    assert_eq!(t.events, 2);
    assert_eq!(t.tokens, 200);
}

#[test]
fn the_page_gets_every_harness_name_and_model_source() {
    let files = vec![one_event_file(Src::Droid, "d", 7, 5)];
    let t = build_json(&mut AggCache::default(), &files, &ScanStats::default());
    assert!(t.json.contains(r#""harnesses":["Claude Code","Codex","OpenCode","Pi","Droid","Qwen Code","Devin"]"#), "{}", &t.json[..t.json.len().min(300)]);
    assert!(t.json.contains(r#""models":[{"src":4,"#), "model is tagged with its harness");
}

#[test]
fn harness_list_and_source_numbers_agree() {
    assert_eq!(HARNESS_NAMES.len(), 7);
    for (i, src) in [Src::Claude, Src::Codex, Src::OpenCode, Src::Pi, Src::Droid, Src::Qwen, Src::Devin].into_iter().enumerate() {
        assert_eq!(src as usize, i, "the page indexes harnesses by these numbers");
    }
}

#[test]
fn unpriced_models_count_tokens_but_not_cost() {
    let mut fd = FileData { src: Src::OpenCode, size: 0, mtime: 0, session: "s".into(), cwd: r"C:\p".into(), title: String::new(), models: vec!["muse-spark-1.3-contributor".into()], events: Vec::new() };
    fd.events.push(Event { ts: 1_794_621_600, model: 0, input: 1_000_000, output: 0, cache_read: 0, cache_w5m: 0, cache_w1h: 0, id: 1, fast: false });
    let t = build_json(&mut AggCache::default(), &[Arc::new(fd)], &ScanStats::default());
    assert_eq!(t.tokens, 1_000_000);
    assert_eq!(t.cost, 0.0);
    assert!(t.json.contains(r#""priced":false"#));
}

// ---------------------------------------------------------------- prices and the Qwen Code reader

fn use_at(input: u64, output: u64, ts: u64) -> prices::Use {
    prices::Use { input, cache_w5m: 0, cache_w1h: 0, cache_read: 0, output, fast: false, ts }
}

#[test]
fn provider_prefix_is_dropped_before_lookup() {
    assert!(std::ptr::eq(prices::lookup("opencode/glm-5.3-flash").unwrap(), prices::lookup("glm-5.3-flash").unwrap()));
}

#[test]
fn deepseek_uses_peak_rate_in_peak_hours_and_half_off_peak() {
    let p = prices::lookup("deepseek-v4.1-flash").unwrap();
    // 2026-11-19 is a Thursday. 02:00 UTC is peak: $0.30 in, $1.20 out per million
    let peak = 1_795_053_600u64;
    assert_eq!(peak % 86_400 / 3_600, 2);
    assert!(prices::deepseek_peak_hour(peak));
    assert!((prices::cost(p, &use_at(1_000_000, 1_000_000, peak)) - 1.50).abs() < 1e-9);
    // 05:00 UTC the same day is off-peak: half price
    let off = peak + 3 * 3600;
    assert!(!prices::deepseek_peak_hour(off));
    assert!((prices::cost(p, &use_at(1_000_000, 1_000_000, off)) - 0.75).abs() < 1e-9);
    // Saturday 02:00 UTC is off-peak all day
    assert!(!prices::deepseek_peak_hour(peak + 2 * 86_400));
}

#[test]
fn deepseek_peak_hours_are_exact_at_their_edges() {
    let thu = 1_795_046_400u64; // Thursday 2026-11-19 00:00 UTC
    let at = |h: u64| thu + h * 3600;
    assert!(!prices::deepseek_peak_hour(at(0)));
    assert!(prices::deepseek_peak_hour(at(1)));
    assert!(prices::deepseek_peak_hour(at(3)));
    assert!(!prices::deepseek_peak_hour(at(4)));
    assert!(!prices::deepseek_peak_hour(at(5)));
    assert!(prices::deepseek_peak_hour(at(6)));
    assert!(prices::deepseek_peak_hour(at(9)));
    assert!(!prices::deepseek_peak_hour(at(10)));
}

#[test]
fn iso_timestamps_become_unix_seconds() {
    assert_eq!(iso_seconds("1970-01-01T00:00:00.000Z"), Some(0));
    assert_eq!(iso_seconds("2000-03-01T00:00:00Z"), Some(951_868_800));
    assert_eq!(iso_seconds("2026-11-19T02:00:00.000Z"), Some(1_795_053_600));
    assert_eq!(iso_seconds("not a time"), None);
    assert_eq!(iso_seconds("2026-13-01T00:00:00Z"), None);
}

#[test]
fn grok_long_context_tier_starts_at_200k_prompt_tokens() {
    let p = prices::lookup("grok-4.7").unwrap();
    assert!((prices::cost(p, &use_at(199_999, 0, 0)) - 199_999.0 * 2.0 / 1e6).abs() < 1e-9);
    assert!((prices::cost(p, &use_at(200_000, 0, 0)) - 200_000.0 * 4.0 / 1e6).abs() < 1e-9);
}

#[test]
fn kimi_k3_cache_writes_use_its_own_ttl_prices() {
    let p = prices::lookup("kimi-k3").unwrap();
    let writes = prices::Use { input: 0, cache_w5m: 1_000_000, cache_w1h: 1_000_000, cache_read: 1_000_000, output: 0, fast: false, ts: 0 };
    // 5 minute writes at the $3 input price, 1 hour writes at $6, cache read $0.30
    assert!((prices::cost(p, &writes) - (3.0 + 6.0 + 0.30)).abs() < 1e-9);
}

#[test]
fn provider_rows_match_their_pricing_pages() {
    let kimi = prices::lookup("kimi-k3.1-pro").unwrap();
    assert!((kimi.base.input - 2.0).abs() < 1e-9 && (kimi.base.output - 10.0).abs() < 1e-9);
    let mini = prices::lookup("MiniMax-M3").unwrap();
    assert!((mini.base.input - 0.30).abs() < 1e-9 && mini.long_over == 512_000);
    let glm = prices::lookup("glm-5.3").unwrap();
    assert!((glm.base.cache_read - 0.26).abs() < 1e-9);
    assert!(prices::lookup("deepseek-v4.1-flash-1017").unwrap().off_peak.is_some());
}

#[test]
fn qwen_code_reads_usage_thoughts_cache_and_skips_damaged_lines() {
    let t = TempDir::new("qwen");
    let text = [
        r#"{"uuid":"u1","timestamp":"2026-06-13T19:40:48.960Z","type":"user","cwd":"C:\\Users\\Joe\\qproj"}"#,
        r#"{"uuid":"u2","timestamp":"2026-06-13T19:41:00.000Z","type":"assistant","cwd":"C:\\Users\\Joe\\qproj","model":"qwen3.5","usageMetadata":{"promptTokenCount":1000,"candidatesTokenCount":50,"thoughtsTokenCount":20,"cachedContentTokenCount":300,"totalTokenCount":1070}}"#,
        r#"{"uuid":"u3","type":"assistant","model":"qwen3.5","usageMetadata":{"promptTokenCount":0,"candidatesTokenCount":0,"thoughtsTokenCount":0,"cachedContentTokenCount":0}}"#,
        r#"{"uuid":"u4","type":"assistant","model":"qwen3.5","usageMetadata":{"promptTokenCount":"odd","candidatesTokenCount":5}}"#,
        "this line is damaged {",
    ]
    .join("\n");
    let p = t.write("chats/0123456789abcdef0123456789abcdef0123.jsonl", &text);
    let mut buf = Vec::new();
    let fd = parse_qwen(&p, "0123456789abcdef0123456789abcdef0123".into(), 1, 2, &mut buf).expect("parsed");
    assert_eq!(fd.cwd, r"C:\Users\Joe\qproj");
    assert_eq!(fd.events.len(), 2, "the zero line is not a request; the odd and damaged lines keep the rest");
    let e = &fd.events[0];
    assert_eq!((e.input, e.output, e.cache_read), (700, 70, 300), "the cached part is not input; thoughts are output");
    assert_eq!(e.ts, 1_781_379_660);
    assert_eq!(fd.models[e.model as usize], "qwen3.5");
    assert_ne!(e.id, 0);
}

// ---------------------------------------------------------------- Devin CLI sessions database

#[test]
fn devin_reads_assistant_metrics_per_session_and_skips_the_rest() {
    let t = TempDir::new("devin");
    let db = t.path().join("sessions.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, working_directory TEXT NOT NULL, backend_type TEXT NOT NULL, model TEXT NOT NULL, agent_mode TEXT NOT NULL, created_at INTEGER NOT NULL, last_activity_at INTEGER NOT NULL, title TEXT);
             CREATE TABLE message_nodes (row_id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, node_id INTEGER NOT NULL, parent_node_id INTEGER, chat_message TEXT NOT NULL, created_at INTEGER NOT NULL, metadata TEXT);
             INSERT INTO sessions (id, working_directory, backend_type, model, agent_mode, created_at, last_activity_at, title) VALUES ('s1', 'C:/Users/t/devin-proj', 'cli', 'm', 'a', 1, 2, 'Fix it');",
        )
        .unwrap();
        let rows: [(&str, i64, &str, &str); 5] = [
            ("s1", 1_795_053_600_000, "user", r#"{"role":"user","content":"hi"}"#),
            ("s1", 1_795_053_601_000, "assistant", r#"{"role":"assistant","content":"ok","metadata":{"generation_model":"glm-5-3-flash-high","metrics":{"input_tokens":100,"output_tokens":20,"cache_read_tokens":300,"cache_creation_tokens":5}}}"#),
            ("s1", 1_795_053_602_000, "assistant", r#"{"role":"assistant","content":"","metadata":{"generation_model":"glm-5-3-flash-high","metrics":{"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"cache_creation_tokens":0}}}"#),
            ("s2", 1_795_053_601, "assistant", r#"{"role":"assistant","content":"x","metadata":{"metrics":{"input_tokens":"7","output_tokens":-3,"cache_read_tokens":null,"cache_creation_tokens":0}}}"#),
            ("s2", 1_795_053_603_000, "assistant", "this is not json cache_read_tokens"),
        ];
        for (sid, created, _role, msg) in rows {
            conn.execute("INSERT INTO message_nodes (session_id, node_id, chat_message, created_at) VALUES (?1, ?2, ?3, ?4)", rusqlite::params![sid, created % 1000, msg, created]).unwrap();
        }
    }
    let mut st = DbScan::default();
    scan_devin(&db, &mut st).expect("the database reads");
    let files = st.files;
    assert_eq!(files.len(), 2, "one file per session");
    let s1 = files.iter().find(|f| f.session == "s1").unwrap();
    assert_eq!(s1.cwd, r"C:\Users\t\devin-proj");
    assert_eq!(s1.title, "Fix it");
    assert_eq!(s1.events.len(), 1, "the zero-usage reply and the user line are not requests");
    let e = &s1.events[0];
    assert_eq!((e.input, e.output, e.cache_read, e.cache_w5m), (100, 20, 300, 5));
    assert_eq!(e.ts, 1_795_053_601, "milliseconds become seconds");
    assert_eq!(s1.models[e.model as usize], "glm-5-3-flash-high");
    let s2 = files.iter().find(|f| f.session == "s2").unwrap();
    assert_eq!(s2.events.len(), 1, "the broken line is skipped; the text count and negative count are read as 7 and 0");
    assert_eq!((s2.events[0].input, s2.events[0].output), (7, 0));
    assert_eq!(s2.events[0].ts, 1_795_053_601, "a value already in seconds is kept");
    assert_eq!(s2.models[s2.events[0].model as usize], "unknown");
}

#[test]
fn devin_speed_tails_mark_the_event_fast() {
    let t = TempDir::new("devin-fast");
    let db = t.path().join("sessions.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            r#"CREATE TABLE sessions (id TEXT PRIMARY KEY, working_directory TEXT, title TEXT);
             CREATE TABLE message_nodes (row_id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, chat_message TEXT, created_at INTEGER);
             INSERT INTO sessions VALUES ('s1', 'C:/p', 'x');
             INSERT INTO message_nodes VALUES (1, 's1', '{"role":"assistant","metadata":{"generation_model":"claude-opus-5-5-medium-fast","metrics":{"input_tokens":10,"output_tokens":1,"cache_read_tokens":0}}}', 1795053600000);
             INSERT INTO message_nodes VALUES (2, 's1', '{"role":"assistant","metadata":{"generation_model":"swe-2-high-lightning","metrics":{"input_tokens":5,"output_tokens":1,"cache_read_tokens":0}}}', 1795053601000);
             INSERT INTO message_nodes VALUES (3, 's1', '{"role":"assistant","metadata":{"generation_model":"swe-2-high","metrics":{"input_tokens":7,"output_tokens":2,"cache_read_tokens":0}}}', 1795053602000);"#,
        )
        .unwrap();
    }
    let mut st = DbScan::default();
    scan_devin(&db, &mut st).expect("the database reads");
    let evs = &st.files[0].events;
    assert_eq!(evs.len(), 3);
    assert!(evs[0].fast && evs[1].fast, "-fast and -lightning are speed tiers");
    assert!(!evs[2].fast);
}

#[test]
fn devin_database_that_is_missing_or_empty_gives_no_events() {
    let t = TempDir::new("devin-empty");
    assert!(scan_devin(&t.path().join("none.db"), &mut DbScan::default()).is_none(), "a missing database cannot be opened");
    let db = t.path().join("empty.db");
    rusqlite::Connection::open(&db).unwrap().execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY, working_directory TEXT, title TEXT); CREATE TABLE message_nodes (row_id INTEGER PRIMARY KEY, session_id TEXT, chat_message TEXT, created_at INTEGER);").unwrap();
    let mut st = DbScan::default();
    assert_eq!(scan_devin(&db, &mut st).map(|_| st.files.len()), Some(0));
}

// The second scan reads only the tail: new rows appear and a chat_message edited
// after insert inside the overlap window is picked up too.
#[test]
fn devin_rescan_merges_new_and_updated_rows() {
    let t = TempDir::new("devin-incr");
    let db = t.path().join("sessions.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        r#"CREATE TABLE sessions (id TEXT PRIMARY KEY, working_directory TEXT, title TEXT);
           CREATE TABLE message_nodes (row_id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, chat_message TEXT, created_at INTEGER);
           INSERT INTO sessions VALUES ('s1', 'C:/p', 't');
           INSERT INTO message_nodes VALUES (1, 's1', '{"role":"assistant","metadata":{"metrics":{"input_tokens":10,"output_tokens":1,"cache_read_tokens":0,"cache_creation_tokens":0}}}', 1795053600000);"#,
    )
    .unwrap();
    let mut st = DbScan::default();
    scan_devin(&db, &mut st).expect("first read");
    assert_eq!(st.files[0].events.len(), 1);
    // the row's metrics grew after insert; a brand new row arrived too
    conn.execute_batch(
        r#"UPDATE message_nodes SET chat_message = '{"role":"assistant","metadata":{"metrics":{"input_tokens":20,"output_tokens":2,"cache_read_tokens":0,"cache_creation_tokens":0}}}' WHERE row_id = 1;
           INSERT INTO message_nodes VALUES (2, 's1', '{"role":"assistant","metadata":{"metrics":{"input_tokens":5,"output_tokens":1,"cache_read_tokens":0,"cache_creation_tokens":0}}}', 1795053601000);"#,
    )
    .unwrap();
    scan_devin(&db, &mut st).expect("second read");
    let evs = &st.files[0].events;
    assert_eq!(evs.len(), 2);
    assert_eq!(evs[0].input, 20, "the edited row's new numbers replaced the old");
    assert_eq!(evs[1].input, 5);
    // a third scan with no change gives the same files
    scan_devin(&db, &mut st).expect("third read");
    assert_eq!(st.files[0].events.len(), 2);
}

// One test drives the alert engine end to end (the log path is a shared env
// var, so the checks stay in a single function). The hub has toasts off, so
// only the log and the rule state are exercised.
#[test]
fn alert_rules_fire_once_then_rearm_below_half() {
    let t = TempDir::new("alerts");
    let log = t.path().join("alerts.jsonl");
    std::env::set_var("JTOKENS_TEST_ALERTS_LOG", &log);
    std::env::set_var("JTOKENS_TEST_SETTINGS", t.path().join("settings.txt"));

    use tao::platform::windows::EventLoopBuilderExtWindows;
    let el = tao::event_loop::EventLoopBuilder::<UserEvent>::with_user_event().with_any_thread(true).build();
    let hub = alerts::Hub::new(el.create_proxy(), 0, false);
    // tight limits on purpose: costH $1, tokH 100, costD $2
    hub.set_from_json(r#"{"on":true,"watch":false,"costH":1,"tokH":100,"costD":2}"#);
    let count = || alerts::list(60).len();

    // the saved settings round-trip through Cfg::load and keep unrelated keys
    let text = fs::read_to_string(t.path().join("settings.txt")).unwrap();
    assert!(text.contains("alertCostH=1") && text.contains("alertTokH=100"), "limits saved, got: {text}");
    let c2 = alerts::Cfg::load();
    assert_eq!((c2.cost_h, c2.tok_h, c2.cost_d), (1.0, 100, 2.0));
    settings_set("theme", "dark");
    let text = fs::read_to_string(t.path().join("settings.txt")).unwrap();
    assert!(text.contains("theme=dark") && text.contains("alertCostH=1"), "other keys survive a save");

    let mut r = alerts::Recent::default();
    hub.eval(&r);
    assert_eq!(count(), 0, "no usage, no alert");

    r.c1 = 1.50;
    r.t1 = 150;
    r.r1 = 2;
    r.c24 = 3.0;
    r.t24 = 150;
    hub.eval(&r);
    assert_eq!(count(), 3, "every rule over its limit fires once");
    let kinds: Vec<String> = alerts::list(60).iter().map(|a| a.kind.clone()).collect();
    for k in ["costHour", "tokHour", "costDay"] {
        assert!(kinds.iter().any(|x| x == k), "{k} fired");
    }

    hub.eval(&r);
    assert_eq!(count(), 3, "still over the limit but inside the cooldown: quiet");

    // a fresh hub (like a restart) reads the log and keeps the cooldown
    let hub2 = alerts::Hub::new(el.create_proxy(), 0, false);
    hub2.set_from_json(r#"{"costH":1,"tokH":100,"costD":2}"#);
    hub2.eval(&r);
    assert_eq!(count(), 3, "a restarted app does not fire the same alerts again");

    // usage falls back under half the limit: the rules re-arm
    let low = alerts::Recent { c1: 0.10, t1: 10, r1: 1, c24: 0.5, t24: 10 };
    hub.eval(&low);
    hub.eval(&r);
    assert_eq!(count(), 3, "re-armed but still inside the cooldown");

    // alerts off or a zero limit: nothing fires
    hub.set_from_json(r#"{"on":false}"#);
    hub.eval(&alerts::Recent { c1: 99.0, t1: 9_000_000, r1: 50, c24: 99.0, t24: 9_000_000 });
    hub.set_from_json(r#"{"on":true,"costH":0,"tokH":0,"costD":0}"#);
    hub.eval(&alerts::Recent { c1: 99.0, t1: 9_000_000, r1: 50, c24: 99.0, t24: 9_000_000 });
    assert_eq!(count(), 3, "off and zero limits stay silent");
}
