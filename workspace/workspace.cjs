"use strict";

// JTokens as a JWorkspace app. The page is the same ui/index.html the window
// shows; preload.cjs gives it the window.ipc that wry gives it there. The
// scanning stays in jtokens.exe (assembly scanner): one `jtokens --serve`
// process per activation keeps its per-file cache warm, so only the first
// scan reads every log.

const fs = require("node:fs");
const path = require("node:path");
const { spawn } = require("node:child_process");
const { nativeTheme, shell } = require("electron");

let ctx = null;
let proc = null;
let buf = "";
let busy = false;
let stopping = false;
let onTheme = null;

const exePath = (dir) => path.join(dir, "target", "release", "jtokens.exe");
const page = () => (ctx && ctx.surface().webContents) || null;
const themeName = () => (nativeTheme.shouldUseDarkColors ? "dark" : "light");

function run(js) {
  const wc = page();
  if (wc && !wc.isDestroyed()) wc.executeJavaScript(js).catch((e) => ctx.log("page: " + e.message));
}

function startScanner() {
  buf = "";
  busy = false;
  const p = spawn(exePath(ctx.dir), ["--serve"], { cwd: ctx.dir, windowsHide: true, stdio: ["pipe", "pipe", "pipe"] });
  proc = p;
  p.stdout.setEncoding("utf8");
  p.stdout.on("data", (d) => {
    buf += d;
    let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, i).trim();
      buf = buf.slice(i + 1);
      busy = false;
      // The scanner writes JSON; the page takes it the same way the window does.
      if (line.startsWith("{")) run("window.tkData(" + line + ")");
    }
  });
  p.stderr.on("data", (d) => ctx.log("scanner: " + String(d).trim()));
  p.on("error", (e) => ctx.log("scanner failed to start: " + e.message));
  p.on("exit", (code) => {
    if (proc === p) proc = null;
    busy = false;
    if (!stopping) ctx.log("scanner exited with code " + code);
  });
}

function scan() {
  if (busy) return;
  if (!proc) startScanner();
  busy = true;
  proc.stdin.write("scan\n");
}

exports.activate = async (c) => {
  ctx = c;
  stopping = false;
  if (!fs.existsSync(exePath(ctx.dir))) throw new Error("JTokens is not built yet. Run cargo build --release in " + ctx.dir + ", then press Reload.");
  startScanner();
  ctx.ipc.on("tk", (_e, body) => {
    const msg = String(body);
    if (msg === "scan") scan();
    else if (msg.startsWith("log:")) ctx.log(msg.slice(4));
    // Alerts need the real window: a toast notifier and a watch timer live
    // there. Here the tab still shows the usage report, but cfg stays null.
    else if (msg === "alerts:get") run("window.tkAlerts && window.tkAlerts({cfg:null,watching:false,err:null,list:[]})");
    else if (msg.startsWith("alertcfg:") || msg === "alerttest") {}
    else if (msg.startsWith("open:")) {
      const url = msg.slice(5);
      if (url.startsWith("https://")) shell.openExternal(url);
    }
    // theme: and rendered: are for the window; here JWorkspace's theme rules.
  });
  const wc = page();
  if (wc) wc.on("dom-ready", () => run("applyTheme(" + JSON.stringify(themeName()) + ")"));
  onTheme = () => run("applyTheme(" + JSON.stringify(themeName()) + ")");
  nativeTheme.on("updated", onTheme);
};

exports.deactivate = async () => {
  stopping = true;
  if (onTheme) nativeTheme.removeListener("updated", onTheme);
  onTheme = null;
  const p = proc;
  proc = null;
  if (!p) return;
  const gone = new Promise((r) => p.once("exit", r));
  p.stdin.end();
  await Promise.race([gone, new Promise((r) => setTimeout(r, 2000))]);
  if (p.exitCode === null) p.kill();
};
