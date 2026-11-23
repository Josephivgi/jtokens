// End-to-end test: the real jtokens.exe scans a fake home folder (see
// fixtures.mjs), exports the dashboard, and headless Edge drives that page.
// Every check compares what the page shows with the numbers the fixtures put
// on disk. Nothing here opens a visible window or touches the real logs.
//
//   node tests/e2e/run.mjs                 (after cargo build --release)
//   JT_EXE=path\to\jtokens.exe JT_EDGE=path\to\msedge.exe node tests/e2e/run.mjs
//   JT_SHOTS=folder node tests/e2e/run.mjs  (also save screenshots there)
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { spawn, spawnSync } from 'node:child_process';
import { buildFixtures } from './fixtures.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..', '..');
const exe = process.env.JT_EXE || path.join(root, 'target', 'release', 'jtokens.exe');
const edge = process.env.JT_EDGE || ['C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe', 'C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe'].find((p) => fs.existsSync(p));
const shots = process.env.JT_SHOTS;
const port = 9400 + Math.floor(Math.random() * 400);

const results = [];
const skipped = [];
const emptyDirs = []; // a home folder with no logs, removed at the end
const check = (name, ok, detail = '') => results.push({ name, ok: !!ok, detail });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const fmtInt = (n) => Math.round(n).toLocaleString('en-US');

if (!fs.existsSync(exe)) { console.error(`jtokens.exe not found at ${exe}. Build it first: cargo build --release`); process.exit(2); }
if (!edge) { console.error('Microsoft Edge not found. Set JT_EDGE to msedge.exe.'); process.exit(2); }

const home = fs.mkdtempSync(path.join(os.tmpdir(), 'jt-e2e-'));
const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'jt-edge-'));
const out = path.join(home, 'dashboard.html');
let edgeProc = null, ws = null;

function cleanup() {
  try { ws && ws.close(); } catch {}
  // stop the headless browser we started (matched by its debugging port, so no other Edge is touched)
  spawnSync('powershell', ['-NoProfile', '-Command', `Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -like '*remote-debugging-port=${port}*' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }`], { windowsHide: true, timeout: 60000 });
  for (const d of [home, profile, ...emptyDirs]) { try { fs.rmSync(d, { recursive: true, force: true }); } catch {} }
}

async function main() {
  const fx = buildFixtures(home);

  // 1. the real scanner and exporter, pointed at the fake home folder
  const run = spawnSync(exe, ['--export', out], { env: { ...process.env, ...fx.env }, windowsHide: true, timeout: 180000 });
  check('jtokens --export runs and writes the dashboard', run.status === 0 && fs.existsSync(out), `exit ${run.status} ${run.stderr || ''}`);
  const badOut = path.join(home, 'no-such-folder', 'out.html');
  const badRun = spawnSync(exe, ['--export', badOut], { env: { ...process.env, ...fx.env }, windowsHide: true, timeout: 180000 });
  check('export into a folder that does not exist: exits with an error code, writes nothing', badRun.status === 1 && !fs.existsSync(badOut), `exit ${badRun.status}`);
  const noPath = spawnSync(exe, ['--export'], { env: { ...process.env, ...fx.env }, windowsHide: true, timeout: 180000 });
  check('--export with no file path: exits with code 2 and does not open the window', noPath.status === 2, `exit ${noPath.status}`);
  const smokeReport = path.join(home, 'smoke.json');
  const smokeRun = spawnSync(exe, ['--smoke', '--report', smokeReport], { env: { ...process.env, ...fx.env }, windowsHide: true, timeout: 180000 });
  const smokeText = fs.existsSync(smokeReport) ? fs.readFileSync(smokeReport, 'utf8') : '';
  check('smoke run on a fixed home: the two scans give the same data', smokeRun.status === 0 && smokeText.includes('"warmSame":true'), smokeText.slice(-80) || `exit ${smokeRun.status}`);
  if (!fs.existsSync(out)) return;
  const html = fs.readFileSync(out, 'utf8');
  check('exported page has its data embedded', html.includes('window.__TK_STATIC=') && !html.includes('<!--DATA-->'));

  // 2. headless browser
  edgeProc = spawn(edge, ['--headless=new', `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, '--no-first-run', '--disable-gpu', '--window-size=1360,900', 'about:blank'], { windowsHide: true, stdio: 'ignore' });
  let page = null;
  for (let i = 0; i < 60 && !page; i++) {
    await sleep(500);
    try { page = (await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()).find((t) => t.type === 'page'); } catch {}
  }
  if (!page) { check('headless browser starts', false); return; }

  ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((r, j) => { ws.onopen = r; ws.onerror = j; });
  let id = 0; const pending = new Map(); const errors = [];
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
    if (m.method === 'Runtime.exceptionThrown') errors.push(m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text);
    if (m.method === 'Runtime.consoleAPICalled' && m.params.type === 'error') errors.push('console.error: ' + m.params.args.map((a) => a.value ?? a.description).join(' '));
  };
  const send = (method, params = {}) => new Promise((r) => { const i = ++id; pending.set(i, r); ws.send(JSON.stringify({ id: i, method, params })); });
  const evaluate = async (expr) => {
    const r = await send('Runtime.evaluate', { expression: expr, returnByValue: true, awaitPromise: true });
    if (r.result.exceptionDetails) return { error: r.result.exceptionDetails.exception?.description || r.result.exceptionDetails.text };
    return r.result.result.value;
  };
  const shot = async (name) => { if (!shots) return; fs.mkdirSync(shots, { recursive: true }); const s = await send('Page.captureScreenshot', { format: 'png' }); fs.writeFileSync(path.join(shots, name + '.png'), Buffer.from(s.result.data, 'base64')); };
  const kpi = (label) => evaluate(`(() => { const k = [...document.querySelectorAll('#kpis .kpi')].find((x) => x.querySelector('.l').textContent === ${JSON.stringify(label)}); return k ? { v: k.querySelector('.v').textContent, s: k.querySelector('.s').textContent } : null; })()`);
  const pick = (v) => evaluate(`document.querySelector('#srcSeg button[data-v="${v}"]').click()`);
  const range = (v) => evaluate(`document.querySelector('#rangeSeg button[data-v="${v}"]').click()`);
  const money = (s) => Number(String(s).replace(/[$,]/g, ''));

  await send('Runtime.enable'); await send('Page.enable');
  await send('Page.navigate', { url: pathToFileURL(out).href });
  for (let i = 0; i < 40; i++) { await sleep(250); if ((await evaluate('typeof D !== "undefined" && D !== null')) === true) break; }
  await sleep(500);

  // 3. the scanner's JSON, per harness, for every event on disk
  const data = await evaluate(`({ harnesses: D.harnesses, models: D.models.map((m) => ({ src: m.src, id: m.id })), rows: D.rows, sessions: D.sessions.map((s) => s[0]) })`);
  check('scanner reports the five harnesses in order', JSON.stringify(data.harnesses) === JSON.stringify(fx.harnesses), JSON.stringify(data.harnesses));
  const tot = fx.harnesses.map((_, h) => ({ tokens: 0, requests: 0, cost: 0 }));
  for (const r of data.rows) { const t = tot[data.models[r[0]].src]; t.tokens += r[3] + r[4] + r[5] + r[6]; t.requests += r[8]; t.cost += r[7]; }
  fx.harnesses.forEach((name, h) => {
    const want = fx.allTime[h], got = tot[h];
    check(`${name}: all-time tokens match the logs`, got.tokens === want.tokens, `got ${got.tokens}, want ${want.tokens}`);
    check(`${name}: all-time requests match the logs`, got.requests === want.requests, `got ${got.requests}, want ${want.requests}`);
    check(`${name}: all-time API cost matches the price table`, Math.abs(got.cost - want.cost) < 1e-6, `got ${got.cost}, want ${want.cost}`);
    check(`${name}: sessions match the session files`, data.sessions.filter((s) => s === h).length === fx.sessionsAllTime[h], `got ${data.sessions.filter((s) => s === h).length}, want ${fx.sessionsAllTime[h]}`);
  });

  // 4. the source buttons
  const buttons = await evaluate(`[...document.querySelectorAll('#srcSeg button')].map((b) => b.textContent)`);
  check('source buttons: All, then each harness', JSON.stringify(buttons) === JSON.stringify(['All', ...fx.harnesses]), JSON.stringify(buttons));

  // 5. each source, all time: what the cards show
  await range(0);
  await sleep(300);
  for (const [h, name] of [['all', 'All']].concat(fx.harnesses.map((n, i) => [String(i), n]))) {
    await pick(h);
    await sleep(250);
    const want = h === 'all'
      ? { requests: fx.allTime.reduce((a, t) => a + t.requests, 0) }
      : { requests: fx.allTime[+h].requests, sessions: fx.sessionsAllTime[+h] };
    const req = await kpi('Requests');
    const ses = await kpi('Sessions');
    check(`${name} (all time): Requests card`, req && req.v === fmtInt(want.requests), JSON.stringify(req));
    if (h !== 'all') check(`${name} (all time): Sessions card`, ses && ses.v === fmtInt(want.sessions), JSON.stringify(ses));
    if (h !== 'all') {
      const rows = await evaluate(`document.querySelectorAll('#sessions tbody tr').length`);
      check(`${name} (all time): session list has one row per session`, rows === fx.sessionsAllTime[+h], `rows ${rows}`);
    }
  }
  await pick('all');
  await sleep(250);
  await shot('all-time-all');

  // 6. the default 30 day range
  await range(30);
  await sleep(300);
  await pick('all');
  await sleep(250);
  const r30 = fx.range30.reduce((a, t) => ({ tokens: a.tokens + t.tokens, requests: a.requests + t.requests, cost: a.cost + t.cost }), { tokens: 0, requests: 0, cost: 0 });
  const cost30 = await kpi('API cost');
  check('All, 30 days: Requests card', (await kpi('Requests'))?.v === fmtInt(r30.requests), JSON.stringify(await kpi('Requests')));
  check('All, 30 days: API cost card', cost30 && Math.abs(money(cost30.v) - r30.cost) <= Math.max(0.01, r30.cost * 0.005), `shows ${cost30?.v}, want ${r30.cost.toFixed(2)}`);
  for (const [i, name] of fx.harnesses.entries()) {
    await pick(i);
    await sleep(250);
    const want = fx.range30[i];
    const req = await kpi('Requests');
    const c = await kpi('API cost');
    check(`${name}, 30 days: Requests card`, req && req.v === fmtInt(want.requests), JSON.stringify(req));
    check(`${name}, 30 days: API cost card`, c && Math.abs(money(c.v) - want.cost) <= Math.max(0.01, want.cost * 0.005), `shows ${c?.v}, want ${want.cost.toFixed(2)}`);
  }

  // 7. the change against the previous 30 days must read as a multiple or a percent, never a silly percent
  await pick('all');
  await sleep(250);
  const tok = await kpi('Tokens');
  const now = r30.tokens, before = fx.previous30.reduce((a, t) => a + t.tokens, 0);
  const ratio = before ? now / before : Infinity;
  if (new Date().getHours() === 0) {
    // Between 00:00 and 01:00 the fixtures' "today" events sit in the hour in progress,
    // which the page leaves out of comparisons, so this check cannot run then.
    skipped.push('change vs previous 30 days (runs between 00:00 and 01:00 local)');
    console.log('SKIP  ' + skipped[skipped.length - 1]);
  } else if (ratio >= 10) {
    check('Tokens change vs previous 30 days reads as a multiple (x)', /[↑↓] [\d,]+×/.test(tok?.s || ''), JSON.stringify(tok));
    check('Tokens change shows the real ratio', (tok?.s || '').includes(`${fmtInt(Math.round(ratio))}×`), `want ${fmtInt(Math.round(ratio))}×, got ${JSON.stringify(tok)}`);
    check('Tokens change: no percent above 999%', !/\d{4,}(\.\d)?%/.test(tok?.s || ''), JSON.stringify(tok));
  } else {
    check('Tokens change vs previous 30 days is a percent', /[↑↓] [\d.,]+%/.test(tok?.s || ''), JSON.stringify(tok));
  }
  // the API cost change follows the same rule: a percent below ten times, a multiple from ten times up
  // (same midnight-hour rule as the token change above: the page has no comparison to show then)
  if (new Date().getHours() !== 0) {
    const costKpi = await kpi('API cost');
    check('API cost change reads as a percent or a multiple', /[↑↓] (?:[\d.,]+%|[\d,]+×|>999×)/.test(costKpi?.s || ''), JSON.stringify(costKpi));
  } else {
    skipped.push('API cost change (runs between 00:00 and 01:00 local)');
    console.log('SKIP  ' + skipped[skipped.length - 1]);
  }
  await shot('30-days-all');

  // 7b. a model used by two harnesses carries the harness name in the legend
  const legend = await evaluate(`[...document.querySelectorAll('#dailyLegend em')].map((e) => e.textContent).join(' | ')`);
  check('legend: a model used by two harnesses names its harness', legend.includes('· Codex') && legend.includes('· Pi'), String(legend));

  // 8. a narrow window (phone-width): nothing sideways, nothing crushed
  await send('Emulation.setDeviceMetricsOverride', { width: 480, height: 900, deviceScaleFactor: 1, mobile: false });
  await sleep(400);
  const narrow = await evaluate(`(() => {
    const cards = [...document.querySelectorAll('.card')].filter((c) => c.offsetParent !== null);
    const names = [...document.querySelectorAll('#models .hbar .nm')];
    const kpis = [...document.querySelectorAll('#kpis .kpi .v')].filter((e) => e.scrollWidth > e.clientWidth + 1).map((e) => e.textContent);
    return {
      scrollWide: document.documentElement.scrollWidth > window.innerWidth + 1,
      narrowCards: cards.filter((c) => c.getBoundingClientRect().width < 200).map((c) => c.querySelector('h2') ? c.querySelector('h2').textContent : c.id),
      // a name cut short (its text does not fit) while left under 60px wide
      crushedNames: names.filter((e) => e.scrollWidth > e.clientWidth + 1 && e.clientWidth < 60).length,
      nameCount: names.length,
      clippedKpis: kpis,
    };
  })()`);
  check('480px wide: no sideways scrolling', narrow && !narrow.scrollWide, JSON.stringify(narrow));
  check('480px wide: every card at least 200px wide', narrow && narrow.narrowCards.length === 0, JSON.stringify(narrow));
  check('480px wide: model names keep room to read', narrow && narrow.crushedNames === 0, JSON.stringify(narrow));
  check('480px wide: no clipped KPI numbers', narrow && narrow.clippedKpis.length === 0, JSON.stringify(narrow));
  // the Sessions table fits its card, normal and full screen
  const sessFit = () => evaluate(`(() => { const t = document.querySelector('#sessions table'), b = document.querySelector('#c-sessions .body'); return { sessWide: !!(t && b && t.scrollWidth > b.clientWidth + 1), tableW: t ? t.scrollWidth : 0, bodyW: b ? b.clientWidth : 0 }; })()`);
  const sess480 = await sessFit();
  check('480px wide: the Sessions table fits its card', sess480 && !sess480.sessWide, JSON.stringify(sess480));
  await evaluate(`document.querySelector('#c-sessions [data-expand]').click()`);
  await sleep(200);
  const sess480Full = await sessFit();
  check('480px wide: the Sessions card full screen fits', sess480Full && !sess480Full.sessWide, JSON.stringify(sess480Full));
  await evaluate(`document.querySelector('#c-sessions [data-expand]').click()`);
  await sleep(200);
  await shot('30-days-all-480');
  await send('Emulation.setDeviceMetricsOverride', { width: 1360, height: 900, deviceScaleFactor: 1, mobile: false });
  await sleep(300);

  // 8b. the middle width: nothing sideways, the Sessions table fits its card
  await send('Emulation.setDeviceMetricsOverride', { width: 820, height: 1000, deviceScaleFactor: 1, mobile: false });
  await sleep(400);
  const mid = await evaluate(`(() => {
    const t = document.querySelector('#sessions table'), b = document.querySelector('#c-sessions .body');
    return { scrollWide: document.documentElement.scrollWidth > window.innerWidth + 1, sessWide: !!(t && b && t.scrollWidth > b.clientWidth + 1) };
  })()`);
  check('820px wide: no sideways scrolling', mid && !mid.scrollWide, JSON.stringify(mid));
  check('820px wide: the Sessions table fits its card', mid && !mid.sessWide, JSON.stringify(mid));
  // the same card opened full screen: the extra columns must go, so the numbers stay on the card
  await evaluate(`document.querySelector('#c-sessions [data-expand]').click()`);
  await sleep(200);
  const full = await evaluate(`(() => {
    const t = document.querySelector('#sessions table'), b = document.querySelector('#c-sessions .body');
    return { sessWide: !!(t && b && t.scrollWidth > b.clientWidth + 1), tableW: t ? t.scrollWidth : 0, bodyW: b ? b.clientWidth : 0 };
  })()`);
  check('820px wide: the Sessions card full screen fits', full && !full.sessWide, JSON.stringify(full));
  await evaluate(`document.querySelector('#c-sessions [data-expand]').click()`);
  await send('Emulation.setDeviceMetricsOverride', { width: 1360, height: 900, deviceScaleFactor: 1, mobile: false });
  await sleep(300);

  // 8c. the light theme switches on the page, and back
  const darkBefore = await evaluate(`document.documentElement.dataset.theme || ''`);
  await evaluate(`document.getElementById('theme').click()`);
  await sleep(200);
  const lightOn = await evaluate(`document.documentElement.dataset.theme || ''`);
  check('light theme: the toggle switches the page theme', lightOn !== darkBefore && lightOn !== '', `${darkBefore} -> ${lightOn}`);
  await evaluate(`document.getElementById('theme').click()`);
  await sleep(200);
  const back = await evaluate(`document.documentElement.dataset.theme || ''`);
  check('light theme: the toggle switches back', back === darkBefore, `${lightOn} -> ${back}`);

  // 8c2. the arrow keys move along the source buttons, as in a radio group
  const arrowed = await evaluate(`(() => { const b = document.querySelector('#srcSeg button[data-v="all"]'); b.focus(); b.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true })); return document.querySelector('#srcSeg button[aria-pressed="true"]')?.textContent || ''; })()`);
  check('source buttons: the right arrow chooses the next source', arrowed === 'Claude Code', String(arrowed));
  await pick('all');
  await sleep(200);

  // 8c3. Home, End and Down also move along the source buttons
  const keys = await evaluate(`(() => {
    const seg = document.querySelector('#srcSeg'), btns = [...seg.querySelectorAll('button')];
    const press = (el, k) => el.dispatchEvent(new KeyboardEvent('keydown', { key: k, bubbles: true }));
    const chosen = () => seg.querySelector('button[aria-pressed="true"]').textContent;
    press(btns[0], 'End'); const end = chosen();
    press(btns[btns.length - 1], 'Home'); const home = chosen();
    press(btns[0], 'ArrowDown'); const down = chosen();
    return { end, home, down, last: btns[btns.length - 1].textContent, first: btns[0].textContent, second: btns[1].textContent };
  })()`);
  check('source buttons: End and Home jump to the last and first source', keys && keys.end === keys.last && keys.home === keys.first, JSON.stringify(keys));
  check('source buttons: the down arrow chooses the next source', keys && keys.down === keys.second, JSON.stringify(keys));
  await pick('all');
  await sleep(200);

  // 8c4. at 1200px: the session names keep room to read, and the API prices page shows "In your logs" without scrolling
  await send('Emulation.setDeviceMetricsOverride', { width: 1200, height: 900, deviceScaleFactor: 1, mobile: false });
  await sleep(400);
  const nameRoom = await evaluate(`(() => { const cells = [...document.querySelectorAll('#sessions td.wrap')]; return cells.length ? Math.min(...cells.map((c) => c.clientWidth)) : 0; })()`);
  check('1200px wide: the session name column keeps room to read', nameRoom >= 90, `narrowest name cell ${nameRoom}px`);
  await evaluate(`document.querySelector('#c-sessions [data-expand]').click()`);
  await sleep(400);
  const mdlFull = await evaluate(`(() => { const c = document.querySelector('#sessions td.mdl'); return c ? { scroll: c.scrollWidth, client: c.clientWidth, text: c.textContent } : null; })()`);
  check('1200px full screen: the main model names are not cut', !!mdlFull && mdlFull.scroll <= mdlFull.client + 1, JSON.stringify(mdlFull));
  await evaluate(`document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))`);
  await sleep(300);
  await evaluate(`document.querySelector('[role="tab"][data-tab="prices"]').click()`);
  await sleep(400);
  const priceCol = await evaluate(`(() => { const ths = [...document.querySelectorAll('#priceTable th')]; const idTh = ths.find((t) => t.textContent.trim() === 'ID'); if (!idTh) return null; const box = idTh.closest('.body').getBoundingClientRect(), r = idTh.getBoundingClientRect(); const filtersHidden = ['#srcSeg', '#rangeSeg', '#metricSeg'].every((s) => getComputedStyle(document.querySelector(s)).display === 'none'); const other = ths.find((t) => t.textContent.trim() === 'Other rates'); const tag = document.querySelector('#priceTable .tag'); const groups = document.querySelectorAll('#priceTable .pgroup').length; return { visible: r.right <= box.right + 1 && r.left >= box.left - 1, otherVisible: !!other && other.getBoundingClientRect().right <= box.right + 1, tagVisible: !!tag && tag.getBoundingClientRect().right <= box.right + 1, groups, filtersHidden }; })()`);
  check('API prices at 1200px: the ID column shows without scrolling', !!(priceCol && priceCol.visible), JSON.stringify(priceCol));
  check('API prices at 1200px: the Other rates column is not cut off', !!(priceCol && priceCol.otherVisible), JSON.stringify(priceCol));
  check('API prices: models in the logs are tagged and rows are grouped by provider', !!(priceCol && priceCol.tagVisible && priceCol.groups >= 3), JSON.stringify(priceCol));
  check('API prices: the overview filters are hidden there', !!(priceCol && priceCol.filtersHidden), JSON.stringify(priceCol));
  await evaluate(`document.querySelector('[role="tab"][data-tab="overview"]').click()`);
  await send('Emulation.setDeviceMetricsOverride', { width: 1360, height: 900, deviceScaleFactor: 1, mobile: false });
  await sleep(400);

  // 8c6. the Alerts tab: the report renders from the same rows the dashboard uses
  await evaluate(`document.querySelector('[role="tab"][data-tab="alerts"]').click()`);
  await sleep(300);
  const al = await evaluate(`(() => ({
    rep: document.querySelector('#report').innerText,
    status: document.querySelector('#alStatus').textContent,
    rangeHidden: getComputedStyle(document.querySelector('#rangeSeg')).display === 'none',
    metricHidden: getComputedStyle(document.querySelector('#metricSeg')).display === 'none',
    srcVisible: getComputedStyle(document.querySelector('#srcSeg')).display !== 'none',
    cfgDisabled: document.querySelector('#alOn').disabled && document.querySelector('#alTest').disabled,
    list: document.querySelector('#alertList').innerText,
  }))()`);
  check('Alerts tab: the report lists top days by cost and tokens', !!al && al.rep.includes('Top days by API cost') && al.rep.includes('Top days by tokens'), String(al && al.rep).slice(0, 200));
  check('Alerts tab: the report has a "Right now" section', !!al && al.rep.includes('This hour') && al.rep.includes('Last 24 hours'), String(al && al.rep).slice(0, 200));
  check('Alerts tab: range and measure filters hide, the source picker stays', !!al && al.rangeHidden && al.metricHidden && al.srcVisible, JSON.stringify(al));
  check('Alerts tab: no app behind the export disables the notification controls', !!al && al.cfgDisabled && al.status.includes('JTokens'), String(al && al.status));
  check('Alerts tab: the empty alert history reads as empty', !!al && al.list.includes('No alerts yet'), String(al && al.list).slice(0, 120));
  await evaluate(`document.querySelector('[role="tab"][data-tab="overview"]').click()`);
  await sleep(200);

  // 8d. a home folder with no logs: the page says so, and shows no usage
  const emptyHome = fs.mkdtempSync(path.join(os.tmpdir(), 'jt-e2e-empty-'));
  emptyDirs.push(emptyHome);
  const emptyOut = path.join(emptyHome, 'empty.html');
  const emptyEnv = Object.fromEntries(Object.keys(fx.env).map((k) => [k, emptyHome]));
  const emptyRun = spawnSync(exe, ['--export', emptyOut], { env: { ...process.env, ...emptyEnv }, windowsHide: true, timeout: 180000 });
  check('empty home: the exporter runs and writes the page', emptyRun.status === 0 && fs.existsSync(emptyOut), `exit ${emptyRun.status}`);
  await send('Page.navigate', { url: pathToFileURL(emptyOut).href });
  for (let i = 0; i < 40; i++) { await sleep(250); if ((await evaluate('typeof D !== "undefined" && D !== null')) === true) break; }
  await sleep(400);
  const emptyText = await evaluate(`document.body.innerText`);
  check('empty home: the page says no usage logs were found', typeof emptyText === 'string' && emptyText.includes('No usage logs found on this computer'), String(emptyText).slice(0, 120));
  const emptyTotal = await kpi('Requests');
  check('empty home: the Requests card reads 0', emptyTotal && emptyTotal.v === '0', JSON.stringify(emptyTotal));

  // 9. no script errors anywhere in the run
  check('no script errors while using the page', errors.length === 0, errors.slice(0, 5).join(' | '));
}

try {
  await main();
} catch (e) {
  check('test run finished without an exception', false, String(e && e.stack || e));
} finally {
  cleanup();
}

let failed = 0;
for (const r of results) {
  if (!r.ok) failed++;
  console.log(`${r.ok ? 'PASS' : 'FAIL'}  ${r.name}${!r.ok && r.detail ? `\n        ${r.detail}` : ''}`);
}
console.log(`\n${results.length - failed} passed, ${failed} failed, ${skipped.length} skipped`);
process.exit(failed ? 1 : 0);
