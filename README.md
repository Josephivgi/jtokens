# JTokens

Fast viewer for Claude Code and Codex token usage with API cost estimates.

- `src/scan.asm`: MASM x64 scanner (AVX2 with SSE2 fallback). Reads raw JSONL and pulls out usage records without a JSON parser.
- `src/main.rs`: file walking, threads, per-file cache, dedupe, pricing, WebView2 window.
- `src/prices.rs`: API list prices per million tokens, with source links and the date they were checked.
- `ui/index.html`: dashboard. Plain HTML, CSS, JS, and SVG charts. No libraries.

Reads `~/.claude/projects/**/*.jsonl`, `~/.codex/sessions/**`, and `~/.codex/archived_sessions/`.
Claude requests are deduped by message id. Codex forked sessions replay the parent's
token events, so Codex events are deduped by their cumulative total plus per-turn numbers.

## Build

    cargo build --release

Needs the Rust MSVC toolchain and Visual Studio Build Tools (for `ml64.exe`).

## Command line

    jtokens                      open the app
    jtokens --smoke --report r.json         scan twice with no window, write timings and totals
    jtokens --smoke-gui --report r.json     boot the real window hidden, scan, render, report, exit
    jtokens --export out.html               write the dashboard with data baked in
    jtokens --json data.json                write the raw data the UI uses

`python -I tools/verify.py data.json` checks the assembly scanner against a full JSON parse of every log line.

## Inside JWorkspace

`jworkspace.json` makes JTokens a JWorkspace app (add this folder on JWorkspace's Apps page). The page is the same `ui/index.html`; `workspace/preload.cjs` gives it `window.ipc`, and `workspace/workspace.cjs` runs `jtokens --serve` hidden: each `scan` line on stdin answers one line of JSON, with the per-file cache kept warm between scans. JWorkspace's theme is used. Build first (`cargo build --release`).

    jtokens --serve                         scan on each `scan` line from stdin, answer JSON lines (for JWorkspace)
