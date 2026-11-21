// High-usage alerts. A watch thread rescans on a timer (this works while the
// window is minimized, which webview timers cannot), each scan's rolling
// windows are checked against the user's limits, and a crossed limit fires a
// Windows toast plus a taskbar flash and is written to alerts.jsonl.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tao::event_loop::EventLoopProxy;

use crate::{build_json, data_dir, esc, settings_get, settings_set, Scanner, UserEvent};

const AUMID: &str = "JTokens";

/// Usage inside rolling windows, worked out alongside the rows in build_json.
/// The alert rules read this; the UI gets it too, so "right now" matches what
/// the rules see.
#[derive(Default, Clone, Copy)]
pub struct Recent {
    pub c1: f64,
    pub t1: u64,
    pub r1: u32,
    pub c24: f64,
    pub t24: u64,
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Cfg {
    pub on: bool,
    /// rescans on a timer even without the Live checkbox; needed for alerts
    /// while the window sits in the background
    pub watch: bool,
    /// dollars in the last hour; 0 turns the rule off
    #[serde(rename = "costH")]
    pub cost_h: f64,
    /// tokens of every kind in the last hour; 0 turns the rule off
    #[serde(rename = "tokH")]
    pub tok_h: u64,
    /// dollars in the last 24 hours; 0 turns the rule off
    #[serde(rename = "costD")]
    pub cost_d: f64,
}

impl Default for Cfg {
    fn default() -> Cfg {
        Cfg { on: true, watch: true, cost_h: 3.0, tok_h: 5_000_000, cost_d: 20.0 }
    }
}

impl Cfg {
    pub fn load() -> Cfg {
        let mut c = Cfg::default();
        if let Some(v) = settings_get("alerts") {
            c.on = v != "0";
        }
        if let Some(v) = settings_get("alertWatch") {
            c.watch = v != "0";
        }
        if let Some(v) = settings_get("alertCostH").and_then(|s| s.parse::<f64>().ok()).filter(|f| f.is_finite() && *f >= 0.0) {
            c.cost_h = v;
        }
        if let Some(v) = settings_get("alertTokH").and_then(|s| s.parse::<f64>().ok()).filter(|f| f.is_finite() && *f >= 0.0) {
            c.tok_h = v as u64;
        }
        if let Some(v) = settings_get("alertCostD").and_then(|s| s.parse::<f64>().ok()).filter(|f| f.is_finite() && *f >= 0.0) {
            c.cost_d = v;
        }
        c
    }

    pub fn save(&self) {
        settings_set("alerts", if self.on { "1" } else { "0" });
        settings_set("alertWatch", if self.watch { "1" } else { "0" });
        settings_set("alertCostH", &self.cost_h.to_string());
        settings_set("alertTokH", &self.tok_h.to_string());
        settings_set("alertCostD", &self.cost_d.to_string());
    }
}

fn secs_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn log_path() -> PathBuf {
    // tests point this at a temp file so the real alert history is untouched
    #[cfg(test)]
    if let Ok(p) = std::env::var("JTOKENS_TEST_ALERTS_LOG") {
        return PathBuf::from(p);
    }
    data_dir().join("alerts.jsonl")
}

pub struct Alert {
    pub ts: u64,
    pub kind: String,
    pub title: String,
    pub detail: String,
}

/// Every logged alert, newest first, up to `max`.
pub fn list(max: usize) -> Vec<Alert> {
    let Ok(s) = fs::read_to_string(log_path()) else { return Vec::new() };
    let mut out = Vec::new();
    for line in s.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        out.push(Alert {
            ts: v.get("ts").and_then(|x| x.as_u64()).unwrap_or(0),
            kind: v.get("kind").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            title: v.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            detail: v.get("detail").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        });
    }
    out.sort_by(|a, b| b.ts.cmp(&a.ts));
    out.truncate(max);
    out
}

/// The last time each rule fired, so a restarted app keeps the cooldown.
fn last_fired() -> HashMap<String, u64> {
    let mut m = HashMap::new();
    for a in list(200) {
        // the list is newest first, so the first entry per kind is the latest
        m.entry(a.kind).or_insert(a.ts);
    }
    m
}

fn append_log(a: &Alert) {
    let p = log_path();
    let mut body = String::new();
    // keep the file small: past 64 KB, only the newest 200 lines survive
    if let Ok(s) = fs::read_to_string(&p) {
        if s.len() > 65_536 {
            let mut lines: Vec<&str> = s.lines().collect();
            if lines.len() > 200 {
                lines = lines[lines.len() - 200..].to_vec();
            }
            for l in lines {
                body.push_str(l);
                body.push('\n');
            }
        } else {
            body = s;
        }
    }
    let _ = write!(body, "{{\"ts\":{},\"kind\":\"{}\",\"title\":", a.ts, a.kind);
    esc(&a.title, &mut body);
    body.push_str(",\"detail\":");
    esc(&a.detail, &mut body);
    body.push_str("}\n");
    if let Some(d) = p.parent() {
        let _ = fs::create_dir_all(d);
    }
    let _ = fs::write(&p, body);
}

// ---------------------------------------------------------------------------
// Rules

struct Rule {
    kind: &'static str,
    limit: fn(&Cfg) -> f64,
    value: fn(&Recent) -> f64,
    /// minimum seconds between two firings
    cooldown: u64,
    text: fn(&Recent, f64) -> (String, String),
}

fn money(v: f64) -> String {
    if v >= 99.95 { format!("${:.0}", v) } else { format!("${:.2}", v) }
}

fn tok(v: u64) -> String {
    if v >= 1_000_000 { format!("{:.1}M", v as f64 / 1e6) } else if v >= 1_000 { format!("{:.1}K", v as f64 / 1e3) } else { format!("{v}") }
}

const RULES: [Rule; 3] = [
    Rule {
        kind: "costHour",
        limit: |c| c.cost_h,
        value: |r| r.c1,
        cooldown: 45 * 60,
        text: |r, lim| ("High API usage".to_string(), format!("API cost in the last hour: {} (limit {}). {} request{}.", money(r.c1), money(lim), r.r1, if r.r1 == 1 { "" } else { "s" })),
    },
    Rule {
        kind: "tokHour",
        limit: |c| c.tok_h as f64,
        value: |r| r.t1 as f64,
        cooldown: 45 * 60,
        text: |r, lim| ("High token usage".to_string(), format!("Tokens in the last hour: {} (limit {}). {} request{}.", tok(r.t1), tok(lim as u64), r.r1, if r.r1 == 1 { "" } else { "s" })),
    },
    Rule {
        kind: "costDay",
        limit: |c| c.cost_d,
        value: |r| r.c24,
        cooldown: 4 * 3600,
        text: |r, lim| ("High API usage today".to_string(), format!("API cost in the last 24 hours: {} (limit {}).", money(r.c24), money(lim))),
    },
];

// ---------------------------------------------------------------------------
// Windows bits: the AUMID registration toasts need, and the taskbar flash

/// Toasts from an unpackaged app need an AppUserModelId registered under
/// HKCU\Software\Classes\AppUserModelId; without it the notifier cannot be made.
fn ensure_aumid() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        #[link(name = "advapi32")]
        extern "system" {
            fn RegCreateKeyExW(root: usize, sub: *const u16, reserved: u32, class: *const u16, options: u32, sam: u32, security: *const u8, out: *mut usize, disposition: *mut u32) -> i32;
            fn RegSetValueExW(key: usize, name: *const u16, reserved: u32, ty: u32, data: *const u8, len: u32) -> i32;
            fn RegCloseKey(key: usize) -> i32;
        }
        const HKEY_CURRENT_USER: usize = 0x8000_0001;
        const KEY_WRITE: u32 = 0x0002_0006;
        const REG_SZ: u32 = 1;
        let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        unsafe {
            let sub = w(&format!("Software\\Classes\\AppUserModelId\\{AUMID}"));
            let mut key = 0usize;
            if RegCreateKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, std::ptr::null(), 0, KEY_WRITE, std::ptr::null(), &mut key, std::ptr::null_mut()) == 0 && key != 0 {
                let name = w("DisplayName");
                let val = w("JTokens");
                let _ = RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, val.as_ptr() as *const u8, (val.len() * 2) as u32);
                let _ = RegCloseKey(key);
            }
        }
    });
}

fn flash_window(hwnd: isize) {
    #[link(name = "user32")]
    extern "system" {
        fn FlashWindowEx(info: *const FlashInfo) -> i32;
    }
    #[repr(C)]
    struct FlashInfo {
        size: u32,
        hwnd: isize,
        flags: u32,
        count: u32,
        timeout: u32,
    }
    // FLASHW_ALL | FLASHW_TIMERNOFG: flash the taskbar button until it is focused
    let info = FlashInfo { size: std::mem::size_of::<FlashInfo>() as u32, hwnd, flags: 0x3 | 0xC, count: 4, timeout: 0 };
    unsafe {
        let _ = FlashWindowEx(&info);
    }
}

// ---------------------------------------------------------------------------
// The hub the window's scan and the watch thread both report to

pub struct Hub {
    cfg: Mutex<Cfg>,
    wake: Condvar,
    /// a fired rule stays off until usage falls back under half its limit
    armed: Mutex<HashMap<&'static str, bool>>,
    fired: Mutex<HashMap<String, u64>>,
    last_err: Mutex<Option<String>>,
    proxy: EventLoopProxy<UserEvent>,
    hwnd: isize,
    /// false in the hidden smoke test: no toasts, no registry writes there
    toast_ok: bool,
}

impl Hub {
    pub fn new(proxy: EventLoopProxy<UserEvent>, hwnd: isize, toast_ok: bool) -> Hub {
        Hub {
            cfg: Mutex::new(Cfg::load()),
            wake: Condvar::new(),
            armed: Mutex::new(HashMap::new()),
            fired: Mutex::new(last_fired()),
            last_err: Mutex::new(None),
            proxy,
            hwnd,
            toast_ok,
        }
    }

    pub fn cfg(&self) -> Cfg {
        *self.cfg.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `alertcfg:` from the page: merge the fields it sent, save, wake the watch.
    pub fn set_from_json(&self, s: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(s) else { return };
        let mut c = self.cfg();
        if let Some(b) = v.get("on").and_then(|x| x.as_bool()) {
            c.on = b;
        }
        if let Some(b) = v.get("watch").and_then(|x| x.as_bool()) {
            c.watch = b;
        }
        let num = |k: &str| v.get(k).and_then(|x| x.as_f64()).filter(|f| f.is_finite() && *f >= 0.0);
        if let Some(n) = num("costH") {
            c.cost_h = n;
        }
        if let Some(n) = num("tokH") {
            c.tok_h = n as u64;
        }
        if let Some(n) = num("costD") {
            c.cost_d = n;
        }
        *self.cfg.lock().unwrap_or_else(|e| e.into_inner()) = c;
        c.save();
        self.wake.notify_all();
        self.push_state();
    }

    /// Check the rolling windows after a scan. Fired rules go quiet for their
    /// cooldown and stay disarmed until usage drops under half the limit.
    pub fn eval(&self, r: &Recent) {
        let cfg = self.cfg();
        if !cfg.on {
            return;
        }
        let now = secs_now();
        let mut fired_any = false;
        for rl in RULES {
            let lim = (rl.limit)(&cfg);
            if lim <= 0.0 {
                continue;
            }
            let v = (rl.value)(r);
            {
                let mut armed = self.armed.lock().unwrap_or_else(|e| e.into_inner());
                if v < lim * 0.5 {
                    armed.insert(rl.kind, true);
                }
                let disarmed = !*armed.get(rl.kind).unwrap_or(&true);
                let cooled = now.saturating_sub(*self.fired.lock().unwrap_or_else(|e| e.into_inner()).get(rl.kind).unwrap_or(&0)) >= rl.cooldown;
                if !(v >= lim && !disarmed && cooled) {
                    continue;
                }
                armed.insert(rl.kind, false);
            }
            let (title, detail) = (rl.text)(r, lim);
            let a = Alert { ts: now, kind: rl.kind.to_string(), title, detail };
            append_log(&a);
            self.fired.lock().unwrap_or_else(|e| e.into_inner()).insert(a.kind.clone(), now);
            self.fire(&a);
            fired_any = true;
        }
        if fired_any {
            self.push_state();
        }
    }

    /// The "Send a test notification" button.
    pub fn test(&self) {
        let a = Alert {
            ts: secs_now(),
            kind: "test".to_string(),
            title: "JTokens test notification".to_string(),
            detail: "High-usage alerts look like this. The Alerts tab keeps the history.".to_string(),
        };
        append_log(&a);
        self.fire(&a);
        self.push_state();
    }

    /// `--toast-test`: register the AUMID and fire one notification, no window.
    /// The exe is a windows-subsystem binary, so the result also goes to
    /// toast-test.txt next to it for callers without a console.
    pub fn toast_test() -> Result<(), String> {
        let r = (|| {
            ensure_aumid();
            tauri_winrt_notification::Toast::new(AUMID)
                .title("JTokens test notification")
                .text1("High-usage alerts look like this. The Alerts tab keeps the history.")
                .show()
                .map_err(|e| format!("{e}"))
        })();
        let _ = fs::write(
            data_dir().join("toast-test.txt"),
            match &r {
                Ok(()) => "sent".to_string(),
                Err(e) => format!("failed: {e}"),
            },
        );
        r
    }

    fn fire(&self, a: &Alert) {
        if self.toast_ok {
            ensure_aumid();
            let r = tauri_winrt_notification::Toast::new(AUMID).title(&a.title).text1(&a.detail).show();
            if let Err(e) = r {
                *self.last_err.lock().unwrap_or_else(|e2| e2.into_inner()) = Some(format!("the notification could not be shown ({e})"));
            }
            flash_window(self.hwnd);
        }
    }

    pub fn push_state(&self) {
        let _ = self.proxy.send_event(UserEvent::Alerts(self.json()));
    }

    /// Everything the Alerts tab needs, as JSON.
    pub fn json(&self) -> String {
        let cfg = self.cfg();
        let mut j = String::with_capacity(2048);
        j.push_str("{\"cfg\":");
        j.push_str(&serde_json::to_string(&cfg).unwrap_or_else(|_| "null".to_string()));
        let _ = write!(j, ",\"watching\":{}", cfg.on && cfg.watch);
        j.push_str(",\"err\":");
        match self.last_err.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            Some(e) => esc(&e, &mut j),
            None => j.push_str("null"),
        }
        j.push_str(",\"list\":[");
        for (i, a) in list(60).iter().enumerate() {
            if i > 0 {
                j.push(',');
            }
            let _ = write!(j, "{{\"ts\":{},\"kind\":\"{}\",\"title\":", a.ts, a.kind);
            esc(&a.title, &mut j);
            j.push_str(",\"detail\":");
            esc(&a.detail, &mut j);
            j.push('}');
        }
        j.push_str("]}");
        j
    }
}

/// The background watcher: a scan every 30 s while alerts watch is on. Scans are
/// warm-cached, so a tick is cheap; its JSON also keeps the dashboard fresh.
pub fn watch(hub: Arc<Hub>, scanner: Arc<Mutex<Scanner>>, proxy: EventLoopProxy<UserEvent>) {
    loop {
        let mut g = hub.cfg.lock().unwrap_or_else(|e| e.into_inner());
        let active = g.on && g.watch;
        drop(g);
        if active {
            let (json, recent) = {
                let mut sc = scanner.lock().unwrap_or_else(|e| e.into_inner());
                let (files, st) = sc.scan();
                let t = build_json(&files, &st);
                (t.json, t.recent)
            };
            let _ = proxy.send_event(UserEvent::Data(json));
            hub.eval(&recent);
        }
        g = hub.cfg.lock().unwrap_or_else(|e| e.into_inner());
        let (_unused, _timeout) = hub.wake.wait_timeout(g, Duration::from_secs(if active { 30 } else { 300 })).unwrap_or_else(|e| e.into_inner());
    }
}
