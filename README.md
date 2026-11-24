# JTokens

Fast viewer for token usage with API cost estimates, for Claude Code, Codex, OpenCode, Pi, Droid, Qwen Code, Devin and Cursor.
It reads the logs already on your computer. Nothing is sent anywhere, except the Cursor account
API read that fetches your own billed usage (see below).

## Download

Get `JTokens.exe` from the [latest release](https://github.com/Josephivgi/jtokens/releases/latest) and run it. No installer.
It updates itself from GitHub Releases: it checks when it opens and every 30 minutes, and installs only when you press Install and restart.
`SHA256SUMS.txt` in each release has the file hashes.

Needs Windows 10 or 11 (64-bit) and the Microsoft Edge WebView2 Runtime (Windows 11 has it).
Settings and the WebView2 profile are kept in `%LOCALAPPDATA%\JTokens`.

## Release

1. Set `version` in `Cargo.toml` (and `jworkspace.json`).
2. Add a `## vX.Y.Z` section to `CHANGELOG.md`. It becomes the release notes.
3. Commit, then `git tag vX.Y.Z && git push origin main vX.Y.Z`.

`.github/workflows/release.yml` runs a secret scan, builds, runs the hidden smoke tests, and publishes the release with `JTokens.exe`, a zip, and `SHA256SUMS.txt`.

## How it works

- `src/scan.asm`: MASM x64 scanner (AVX2 with SSE2 fallback). Reads raw JSONL and pulls out usage records without a JSON parser.
- `src/main.rs`: file walking, threads, per-file cache, dedupe, pricing, WebView2 window.
- `src/prices.rs`: API list prices per million tokens for Anthropic, OpenAI, DeepSeek, Moonshot (Kimi), xAI (Grok), Z.ai (GLM) and MiniMax, with source links and the date they were checked. DeepSeek has peak and off-peak rates, chosen by the time of each request.
- `ui/index.html`: dashboard. Plain HTML, CSS, JS, and SVG charts. No libraries.

Reads `~/.claude/projects/**/*.jsonl`, `~/.codex/sessions/**`, and `~/.codex/archived_sessions/`,
`~/.local/share/opencode/opencode.db` (OpenCode, read-only), `~/.pi/agent/sessions/**/*.jsonl` (Pi),
and `~/.factory/sessions/**/*.settings.json` (Droid), `~/.qwen/projects/**/chats/*.jsonl` (Qwen Code), and `%APPDATA%Devinclisessions.db` (Devin, read-only). The source buttons choose one harness or All.
Each harness is deduped on its own: Claude requests by message id, Pi requests by message id and time,
OpenCode by message id. Codex forked sessions replay the parent's token events, so Codex events are
deduped by their cumulative total plus per-turn numbers. The same id under two harnesses counts twice.
Pi and OpenCode record each request. Droid records only a running total per session, so each session's
total is counted at the time its file was last saved.

Cursor keeps no usage history on disk. JTokens reads it from the Cursor account API
(`aiserver.v1.DashboardService/GetFilteredUsageEvents`, the same calls T3 Code's usage page makes)
with the API key from `CURSOR_API_KEY` or the sign-in T3 Code stores in
`~/.t3/**/userdata/secrets/provider-auth-*.bin`. Cursor reports the billed amount on every event,
so those rows show real spend rather than a list-price estimate. Fetched events are kept in
`%LOCALAPPDATA%JTokens\cursor-usage.json`, and each scan only asks for the new tail.

## Build from source

    cargo build --release

Needs the Rust MSVC toolchain and Visual Studio Build Tools (for `ml64.exe`).

## Tests

    cargo test --release --locked      unit tests: every reader, the price math, the dedupe (src/tests.rs)
    cargo build --release --locked
    node tests/e2e/run.mjs             end-to-end: the real exe reads a fake home folder with logs for
                                       all five harnesses, then headless Edge checks each number and the
                                       narrow layout (needs Microsoft Edge; uses no real logs)

The release workflow runs both before it publishes anything.

## Command line

    jtokens                      open the app
    jtokens --smoke --report r.json         scan twice with no window, write timings and totals
    jtokens --smoke-gui --report r.json     boot the real window hidden, scan, render, report, exit
    jtokens --export out.html               write the dashboard with data baked in
    jtokens --json data.json                write the raw data the UI uses
    jtokens --check-update                  say whether a newer release is on GitHub
    jtokens --update [--no-launch]          install the newest release (checked by SHA-256), then start it
    jtokens --version                       print the version

`python -I tools/verify.py data.json` checks the assembly scanner against a full JSON parse of every log line.

## Inside JWorkspace

`jworkspace.json` makes JTokens a JWorkspace app (add this folder on JWorkspace's Apps page). The page is the same `ui/index.html`; `workspace/preload.cjs` gives it `window.ipc`, and `workspace/workspace.cjs` runs `jtokens --serve` hidden: each `scan` line on stdin answers one line of JSON, with the per-file cache kept warm between scans. JWorkspace's theme is used. Build first (`cargo build --release`).

    jtokens --serve                         scan on each `scan` line from stdin, answer JSON lines (for JWorkspace)

## License

MIT. See `LICENSE`.
