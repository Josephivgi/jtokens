// Builds a static preview of the JTokens UI with mock data, like the app's
// UI.replace("<!--DATA-->", ...) does. Usage: node make-preview.cjs <tab>
// tab: "overview" (default) or "prices" or "alerts". Writes preview.html here.
const fs = require("fs");
const path = require("path");

const tab = process.argv[2] || "overview";
const ui = fs.readFileSync(path.join(__dirname, "..", "..", "ui", "index.html"), "utf8");

const prices = [
  { id: "claude-fable-5-2", name: "Claude Fable 5.2", in: 8, w5: 10, w1h: 16, cr: 0.2, out: 40, longOver: 0, lin: 0, lcr: 0, lout: 0, fin: 16, fout: 80 },
  { id: "claude-fable-5", name: "Claude Fable 5", in: 10, w5: 12.5, w1h: 20, cr: 1, out: 50, longOver: 0, lin: 0, lcr: 0, lout: 0, fin: 0, fout: 0 },
  { id: "claude-opus-5-5", name: "Claude Opus 5.5", in: 4, w5: 5, w1h: 8, cr: 0.2, out: 20, longOver: 0, lin: 0, lcr: 0, lout: 0, fin: 8, fout: 40 },
  { id: "claude-sonnet-5-5", name: "Claude Sonnet 5.5", in: 2, w5: 2.5, w1h: 4, cr: 0.1, out: 10, longOver: 0, lin: 0, lcr: 0, lout: 0, fin: 0, fout: 0 },
  { id: "claude-haiku-5-5", name: "Claude Haiku 5.5", in: 0.1, w5: 0.125, w1h: 0.2, cr: 0.01, out: 0.5, longOver: 100000, lin: 0.5, lcr: 0.05, lout: 2.5, fin: 0, fout: 0 },
  { id: "gpt-6.1-terra", name: "GPT-6.1 Terra", in: 1, w5: 0, w1h: 0, cr: 0.05, out: 5, longOver: 272000, lin: 2, lcr: 0.1, lout: 7.5, fin: 0, fout: 0 },
  { id: "gpt-6.1-sol", name: "GPT-6.1 Sol", in: 2, w5: 0, w1h: 0, cr: 0.1, out: 10, longOver: 272000, lin: 4, lcr: 0.2, lout: 15, fin: 0, fout: 0 },
  { id: "gpt-5.3-codex", name: "GPT-5.3 Codex", in: 1.75, w5: 0, w1h: 0, cr: 0.175, out: 14, longOver: 0, lin: 0, lcr: 0, lout: 0, fin: 0, fout: 0 },
  { id: "deepseek-v4.1-flash", name: "DeepSeek V4.1 Flash", in: 0.3, w5: 0, w1h: 0, cr: 0.006, out: 1.2, longOver: 0, lin: 0, lcr: 0, lout: 0, fin: 0, fout: 0, offIn: 0.15, offCr: 0.003, offOut: 0.6 },
  { id: "deepseek-v4-pro", name: "DeepSeek V4 Pro", in: 1.32, w5: 0, w1h: 0, cr: 0.044, out: 3.96, longOver: 0, lin: 0, lcr: 0, lout: 0, fin: 0, fout: 0, offIn: 0.66, offCr: 0.022, offOut: 1.98 },
  { id: "kimi-k3.1-pro", name: "Kimi K3.1 Pro", in: 2, w5: 2, w1h: 4, cr: 0.2, out: 10, longOver: 0, lin: 0, lcr: 0, lout: 0, fin: 0, fout: 0 },
  { id: "grok-4.7", name: "Grok 4.7", in: 2, w5: 0, w1h: 0, cr: 0.5, out: 6, longOver: 199999, lin: 4, lcr: 1, lout: 12, fin: 0, fout: 0 },
  { id: "glm-5.3", name: "GLM-5.3", in: 1.4, w5: 0, w1h: 0, cr: 0.26, out: 4.4, longOver: 0, lin: 0, lcr: 0, lout: 0, fin: 0, fout: 0 },
  { id: "minimax-m3", name: "MiniMax M3", in: 0.3, w5: 0, w1h: 0, cr: 0.06, out: 1.2, longOver: 512000, lin: 0.6, lcr: 0.12, lout: 2.4, fin: 0, fout: 0 },
  { id: "swe-2", name: "SWE-2", in: 3, w5: 3, w1h: 3, cr: 0.3, out: 15, longOver: 0, lin: 0, lcr: 0, lout: 0, fin: 6, fout: 30 },
  { id: "swe-2-flash", name: "SWE-2 Flash", in: 0.1, w5: 0.125, w1h: 0.125, cr: 0.01, out: 0.5, longOver: 0, lin: 0, lcr: 0, lout: 0, fin: 0.2, fout: 1 },
];

const nowHour = Math.floor(Date.now() / 3600000);
const models = [
  { src: 0, id: "claude-fable-5-2", name: "Claude Fable 5.2", priced: true },
  { src: 0, id: "claude-sonnet-5-5", name: "Claude Sonnet 5.5", priced: true },
  { src: 1, id: "gpt-6.1-terra", name: "GPT-6.1 Terra", priced: true },
  { src: 4, id: "swe-2", name: "SWE-2", priced: true },
  { src: 1, id: "codex-auto-review", name: "codex-auto-review", priced: false },
];
const sessions = [
  [0, "sess-a1b2c3", 0, "Fix pricing page", nowHour - 50, nowHour - 1],
  [1, "sess-d4e5f6", 1, "Add filters", nowHour - 30, nowHour - 2],
  [4, "sess-g7h8i9", 0, "Refactor scan", nowHour - 20, nowHour - 3],
];
const rows = [];
for (let d = 0; d < 30; d++) {
  for (const m of [0, 1, 2, 3]) {
    const hr = nowHour - d * 24 - ((m * 5 + d * 3) % 20);
    const inp = 50000 + ((m + 1) * (d + 3) * 7919) % 400000;
    const outp = inp / 4;
    const cr = inp * 3;
    const cw = inp / 5;
    rows.push([m, m % sessions.length, hr, inp, outp, cr, cw, (inp * 3 + outp * 20) / 1e6, 4 + (d % 7)]);
  }
}

const D = {
  scan: { ms: 42.5, files: 128, parsed: 3, bytes: 84500000, parsedBytes: 12000, events: rows.length, avx2: true },
  pricesChecked: "2026-11-21",
  priceSources: [
    "https://platform.claude.com/docs/en/about-claude/pricing",
    "https://developers.openai.com/api/docs/pricing",
    "https://api-docs.deepseek.com/quick_start/pricing",
    "https://platform.kimi.ai/docs/pricing/chat",
    "https://docs.x.ai/developers/models",
    "https://docs.z.ai/guides/overview/pricing",
    "https://platform.minimax.io/docs/guides/pricing-paygo",
    "https://docs.devinenterprise.com/desktop/models",
  ],
  harnesses: ["Claude Code", "Codex", "OpenCode", "Pi", "Devin", "Droid", "Qwen Code"],
  models,
  projects: ["C:\\Users\\Joe\\JTokens", "C:\\Users\\Joe\\other-app"],
  sessions,
  prices,
  rows,
  recent: { c1: 0.42, t1: 120000, r1: 5, c24: 8.31, t24: 2400000 },
  limits: { sources: [] },
};

const theme = process.argv[3] || "dark";
const filter = process.argv[4] || "";
let html = ui.replace("<!--DATA-->", `<script>window.__TK_STATIC=${JSON.stringify(D)};</script>`);
html = html.replace('data-theme="dark"', `data-theme="${theme}"`);
const boot = [];
if (tab !== "overview") boot.push(`document.querySelector('#tab-btn-${tab}').click()`);
if (filter) boot.push(`const f=document.querySelector('#priceFilter'); f.value='${filter}'; f.dispatchEvent(new Event('input',{bubbles:true}))`);
if (boot.length) html = html.replace("</body>", `<script>addEventListener("load",()=>setTimeout(()=>{${boot.join(";")}},50));</script></body>`);
fs.writeFileSync(path.join(__dirname, "preview.html"), html);
console.log("wrote preview.html, tab:", tab, "rows:", rows.length);
