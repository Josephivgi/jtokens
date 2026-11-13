//! Updates from GitHub Releases. The window checks for a newer release when it
//! opens and every 30 minutes, and the user decides whether to install it.
//! Installing downloads the new JTokens.exe, checks its SHA-256 against the
//! one GitHub reports, swaps it in place of the running exe and restarts.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const REPO: &str = "Josephivgi/jtokens";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const ASSET: &str = "JTokens.exe";
/// How often an open window looks for a new release.
pub const CHECK_EVERY: Duration = Duration::from_secs(30 * 60);

#[derive(Clone)]
pub struct Release {
    pub version: String,
    pub notes: String,
    pub page: String,
    asset_url: String,
    size: u64,
    sha256: String,
}

#[derive(Clone)]
pub enum State {
    Idle,
    Checking,
    UpToDate,
    NoReleases,
    Available(Release),
    Downloading(u64, u64),
    /// The new version is in place and has been started; this window closes.
    Restarting,
    Failed(String),
}

impl State {
    /// The page's view of the state (window.tkUpdate).
    pub fn json(&self) -> String {
        let mut j = String::from("{\"current\":");
        crate::esc(CURRENT, &mut j);
        j.push_str(",\"s\":");
        match self {
            State::Idle => j.push_str("\"idle\""),
            State::Checking => j.push_str("\"checking\""),
            State::UpToDate => j.push_str("\"uptodate\""),
            State::NoReleases => j.push_str("\"none\""),
            State::Restarting => j.push_str("\"restarting\""),
            State::Available(r) => {
                j.push_str("\"available\",\"version\":");
                crate::esc(&r.version, &mut j);
                j.push_str(",\"notes\":");
                crate::esc_multiline(&r.notes, &mut j);
                j.push_str(",\"page\":");
                crate::esc(&r.page, &mut j);
            }
            State::Downloading(done, total) => {
                let _ = write!(j, "\"downloading\",\"done\":{done},\"total\":{total}");
            }
            State::Failed(msg) => {
                j.push_str("\"failed\",\"msg\":");
                crate::esc(msg, &mut j);
            }
        }
        j.push('}');
        j
    }
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

/// "v1.2.3" or "1.2.3" as comparable numbers.
fn parse_version(v: &str) -> Option<Vec<u64>> {
    v.trim().trim_start_matches(['v', 'V']).split('.').map(|p| p.parse().ok()).collect()
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .user_agent(concat!("JTokens/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// The latest published release, or None if there is none yet. Asks the
/// GitHub API first (it has the release notes). Without a login the API allows
/// 60 requests an hour per connection, so when that is used up it asks
/// github.com itself, which has no such limit.
fn latest() -> Result<Option<Release>, String> {
    match latest_api() {
        Ok(r) => Ok(r),
        Err(ApiErr::Limited) => latest_web(),
        // A network that blocks api.github.com may still reach github.com.
        Err(ApiErr::Other(e)) => latest_web().map_err(|_| e),
    }
}

enum ApiErr {
    Limited,
    Other(String),
}

fn latest_api() -> Result<Option<Release>, ApiErr> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut resp = match agent().get(&url).header("Accept", "application/vnd.github+json").call() {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(ureq::Error::StatusCode(403 | 429)) => return Err(ApiErr::Limited),
        Err(e) => return Err(ApiErr::Other(format!("Could not reach GitHub: {e}"))),
    };
    release_from_api(&mut resp).map_err(ApiErr::Other)
}

/// The newest release from github.com: /releases/latest redirects to the
/// release's tag, and the release's SHA256SUMS.txt gives the checksum.
fn latest_web() -> Result<Option<Release>, String> {
    let page = format!("https://github.com/{REPO}/releases/latest");
    let no_redirect: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .user_agent(concat!("JTokens/", env!("CARGO_PKG_VERSION")))
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into();
    let resp = no_redirect.get(&page).call().map_err(|e| format!("Could not reach GitHub: {e}"))?;
    let loc = resp.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let Some(tag) = loc.rsplit_once("/releases/tag/").map(|(_, t)| t.to_string()) else {
        // /releases/latest with no release redirects to the releases list
        return if resp.status().is_redirection() { Ok(None) } else { Err(format!("Unexpected reply from GitHub ({})", resp.status())) };
    };
    let sums_url = format!("https://github.com/{REPO}/releases/download/{tag}/SHA256SUMS.txt");
    let sums = agent()
        .get(&sums_url)
        .call()
        .map_err(|e| format!("Release {tag} has no checksum list yet ({e})."))?
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("Could not read the checksum list: {e}"))?;
    let sha256 = sums
        .lines()
        .find_map(|l| {
            let (hash, name) = l.trim().split_once(char::is_whitespace)?;
            name.trim().trim_start_matches('*').eq_ignore_ascii_case(ASSET).then(|| hash.to_ascii_lowercase())
        })
        .filter(|h| h.len() == 64)
        .ok_or_else(|| format!("Release {tag} has no checksum for {ASSET}, so it was not trusted."))?;
    Ok(Some(Release {
        version: tag.trim_start_matches(['v', 'V']).to_string(),
        notes: String::new(),
        page: format!("https://github.com/{REPO}/releases/tag/{tag}"),
        asset_url: format!("https://github.com/{REPO}/releases/download/{tag}/{ASSET}"),
        size: 0,
        sha256,
    }))
}

fn release_from_api(resp: &mut ureq::http::Response<ureq::Body>) -> Result<Option<Release>, String> {
    let rel: GhRelease = resp.body_mut().read_json().map_err(|e| format!("Unexpected reply from GitHub: {e}"))?;
    let asset = rel
        .assets
        .into_iter()
        .find(|a| a.name.eq_ignore_ascii_case(ASSET))
        .ok_or_else(|| format!("Release {} has no {ASSET} to download yet.", rel.tag_name))?;
    let sha256 = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| format!("Release {} has no checksum, so it was not trusted.", rel.tag_name))?;
    Ok(Some(Release {
        version: rel.tag_name.trim_start_matches(['v', 'V']).to_string(),
        notes: rel.body.unwrap_or_default().trim().to_string(),
        page: rel.html_url,
        asset_url: asset.browser_download_url,
        size: asset.size,
        sha256,
    }))
}

type Notify = Arc<dyn Fn(&State) + Send + Sync>;

pub struct Updater {
    state: Mutex<State>,
    notify: Notify,
}

impl Updater {
    pub fn new(notify: Notify) -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(State::Idle), notify })
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    fn set(&self, s: State) {
        *self.state.lock().unwrap() = s.clone();
        (self.notify)(&s);
    }

    fn busy(&self) -> bool {
        matches!(self.state(), State::Checking | State::Downloading(..) | State::Restarting)
    }

    /// Checks when the window opens and every 30 minutes. A found update or a
    /// failure waits for the user (Install, or Try again) instead.
    pub fn start_timer(self: &Arc<Self>) {
        let me = self.clone();
        std::thread::spawn(move || loop {
            if !matches!(me.state(), State::Available(_) | State::Failed(_)) {
                me.check();
            }
            std::thread::sleep(CHECK_EVERY);
        });
    }

    pub fn check(self: &Arc<Self>) {
        if self.busy() {
            return;
        }
        self.set(State::Checking);
        let me = self.clone();
        std::thread::spawn(move || {
            let s = match latest() {
                Ok(Some(r)) if is_newer(&r.version, CURRENT) => State::Available(r),
                Ok(Some(_)) => State::UpToDate,
                Ok(None) => State::NoReleases,
                Err(e) => State::Failed(e),
            };
            me.set(s);
        });
    }

    /// Downloads, verifies and installs the available release, then starts it.
    /// `on_restart` runs once the new version has started, to close this window.
    pub fn install(self: &Arc<Self>, on_restart: impl FnOnce() + Send + 'static) {
        let State::Available(rel) = self.state() else { return };
        self.set(State::Downloading(0, rel.size));
        let me = self.clone();
        std::thread::spawn(move || {
            let progress = |done, total| me.set(State::Downloading(done, total));
            match download(&rel, progress).and_then(|file| swap(&file)).and_then(|exe| launch(&exe)) {
                Ok(()) => {
                    me.set(State::Restarting);
                    on_restart();
                }
                Err(e) => me.set(State::Failed(e)),
            }
        });
    }
}

/// `--check-update` prints whether a newer release exists; `--update` also
/// installs it and starts it, unless `--no-launch` is given too.
pub fn cli(install: bool, launch_after: bool) -> Result<(), String> {
    println!("JTokens {CURRENT}");
    let rel = match latest()? {
        Some(r) if is_newer(&r.version, CURRENT) => r,
        Some(r) => {
            println!("Up to date (latest release is {}).", r.version);
            return Ok(());
        }
        None => {
            println!("No releases published yet.");
            return Ok(());
        }
    };
    println!("Version {} is available: {}", rel.version, rel.page);
    if !install {
        return Ok(());
    }
    let file = download(&rel, |_, _| {})?;
    println!("Downloaded and verified, sha256 {}", rel.sha256);
    let exe = swap(&file)?;
    println!("Installed {} at {}", rel.version, exe.display());
    if launch_after {
        launch(&exe)?;
        println!("Started the new version.");
    }
    Ok(())
}

fn update_dir() -> PathBuf {
    let dir = crate::data_dir().join("update");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn download(rel: &Release, progress: impl Fn(u64, u64)) -> Result<PathBuf, String> {
    let fail = |e: &dyn std::fmt::Display| format!("Download failed: {e}");
    let mut resp = agent().get(&rel.asset_url).call().map_err(|e| fail(&e))?;
    let path = update_dir().join(format!("JTokens-{}.exe", rel.version));
    let mut file = std::fs::File::create(&path).map_err(|e| fail(&e))?;
    // The size is unknown when the release came from github.com instead of the API.
    let total = if rel.size > 0 {
        rel.size
    } else {
        resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(0)
    };
    let limit = if rel.size > 0 { rel.size + 1 } else { 300 * 1024 * 1024 };
    let mut reader = resp.body_mut().with_config().limit(limit).reader();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut done = 0u64;
    let mut last_report = Instant::now();
    loop {
        let n = reader.read(&mut buf).map_err(|e| fail(&e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| fail(&e))?;
        done += n as u64;
        if last_report.elapsed() >= Duration::from_millis(100) {
            last_report = Instant::now();
            progress(done, total);
        }
    }
    file.flush().map_err(|e| fail(&e))?;
    drop(file);
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if (rel.size > 0 && done != rel.size) || got != rel.sha256 {
        let _ = std::fs::remove_file(&path);
        return Err("The download did not match the release checksum, so it was not installed.".into());
    }
    progress(done, done);
    Ok(path)
}

fn old_exe(exe: &Path) -> PathBuf {
    let stem = exe.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "JTokens".into());
    exe.with_file_name(format!("{stem}.old.exe"))
}

/// Puts the new exe where the running one is and returns its path.
fn swap(new_exe: &Path) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let old = old_exe(&exe);
    let _ = std::fs::remove_file(&old);
    // Windows lets a running exe be renamed but not overwritten.
    std::fs::rename(&exe, &old)
        .map_err(|e| format!("Could not replace {}: {e}. Move JTokens to a folder you can write to.", exe.display()))?;
    if let Err(e) = std::fs::copy(new_exe, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("Could not install the new version: {e}"));
    }
    let _ = std::fs::remove_file(new_exe);
    Ok(exe)
}

fn launch(exe: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    std::process::Command::new(exe)
        .creation_flags(DETACHED_PROCESS)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Installed, but could not start the new version: {e}. Open JTokens again."))
}

/// Removes the exe left behind by the last update. The old window may still be
/// closing for a moment, so this retries briefly.
pub fn cleanup_after_update() {
    let Ok(exe) = std::env::current_exe() else { return };
    let old = old_exe(&exe);
    if !old.exists() {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..30 {
            if std::fs::remove_file(&old).is_ok() || !old.exists() {
                let _ = std::fs::remove_dir_all(update_dir());
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_numbers() {
        assert!(is_newer("v1.10.0", "1.9.9"));
        assert!(is_newer("1.0.1", "1.0.0"));
        assert!(!is_newer("v1.0.0", "1.0.0"));
        assert!(!is_newer("0.9.0", "1.0.0"));
        assert!(!is_newer("garbage", "1.0.0"));
    }

    #[test]
    fn state_json_escapes_notes() {
        let r = Release { version: "1.1.0".into(), notes: "**New**\n- \"quoted\" <b>".into(), page: "https://x".into(), asset_url: String::new(), size: 1, sha256: String::new() };
        let j = State::Available(r).json();
        assert!(j.contains("\"s\":\"available\""));
        assert!(j.contains("\\n- \\\"quoted\\\" \\u003cb>"));
    }
}
