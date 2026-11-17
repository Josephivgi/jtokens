// Builds a fake home folder with one set of usage logs per harness, and the
// totals the dashboard must show for them. Nothing here touches the real
// ~/.claude, ~/.codex, ~/.pi, ~/.factory or OpenCode data.
//
// Events are placed at three times relative to "now":
//   today  (1 hour ago, never before local midnight) - inside the default 30 day range
//   d45    (45 days ago)  - inside the previous 30 days, outside the range
//   d65    (65 days ago)  - older than the previous period, so it sets the first
//                           kept day and the previous period counts as fair
import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';

const HOUR = 3600e3, DAY = 24 * HOUR;
const WINDOWS = { today: 1 * HOUR, d45: 45 * DAY, d65: 65 * DAY };

// Prices from src/prices.rs, USD per million tokens (checked 2026-11-10).
// Only the rates these fixtures use.
const PRICE = {
  'claude-sonnet-5-5': { input: 2.0, read: 0.10, write5m: 2.5, output: 10.0 },
  'gpt-6-sol': { input: 2.0, read: 0.20, write5m: 0, output: 10.0 },
  'gpt-5.6-sol': { input: 4.0, read: 0.40, write5m: 0, output: 20.0 },
  // DeepSeek: peak rates, and off-peak (half) outside 01-04 and 06-10 UTC on weekdays
  'deepseek-v4.1-flash': { input: 0.30, read: 0.006, write5m: 0, output: 1.20, off: { input: 0.15, read: 0.003, write5m: 0, output: 0.60 } },
};

// DeepSeek's peak hours, in UTC, Monday to Friday (the same rule as src/prices.rs)
function deepseekPeak(ms) {
  const d = new Date(ms);
  const wd = d.getUTCDay();
  const h = d.getUTCHours();
  return wd >= 1 && wd <= 5 && ((h >= 1 && h < 4) || (h >= 6 && h < 10));
}

function costOf(model, u, ms) {
  let p = PRICE[model];
  if (!p) return 0;
  if (p.off && !deepseekPeak(ms)) p = p.off;
  return (u.input * p.input + u.cacheRead * p.read + u.cacheWrite * p.write5m + u.output * p.output) / 1e6;
}

// Each event: harness, window, model, input, output, cacheRead, cacheWrite, reasoning (OpenCode only).
// Each (harness, window) pair is one session file, so session counts are easy to check.
const EVENTS = [
  // Claude Code
  { h: 'claude', w: 'today', model: 'claude-sonnet-5-5', input: 1000, output: 500, cacheRead: 10000, cacheWrite: 200 },
  { h: 'claude', w: 'today', model: 'claude-sonnet-5-5', input: 300, output: 100, cacheRead: 4000, cacheWrite: 0 },
  { h: 'claude', w: 'd45', model: 'claude-sonnet-5-5', input: 100, output: 50, cacheRead: 1000, cacheWrite: 0 },
  { h: 'claude', w: 'd65', model: 'claude-sonnet-5-5', input: 10, output: 10, cacheRead: 0, cacheWrite: 0 },
  // Codex (input_tokens in the log includes the cached part)
  { h: 'codex', w: 'today', model: 'gpt-5.6-sol', input: 2000, output: 300, cacheRead: 6000, cacheWrite: 0 },
  { h: 'codex', w: 'd45', model: 'gpt-5.6-sol', input: 400, output: 40, cacheRead: 0, cacheWrite: 0 },
  // Pi (one zero-usage error message is also written; it must not count)
  { h: 'pi', w: 'today', model: 'claude-sonnet-5-5', input: 500, output: 80, cacheRead: 2000, cacheWrite: 100 },
  { h: 'pi', w: 'today', model: 'deepseek-v4.1-flash', input: 900, output: 60, cacheRead: 0, cacheWrite: 0 },
  { h: 'pi', w: 'd45', model: 'deepseek-v4.1-flash', input: 200, output: 20, cacheRead: 0, cacheWrite: 0 },
  // OpenCode (reasoning is listed apart from output and must be added to output)
  { h: 'opencode', w: 'today', model: 'gpt-5.6-sol', input: 700, output: 90, reasoning: 30, cacheRead: 1500, cacheWrite: 0 },
  { h: 'opencode', w: 'd45', model: 'gpt-5.6-sol', input: 70, output: 10, reasoning: 0, cacheRead: 0, cacheWrite: 0 },
  // Droid (one total per session, placed at the session file's time)
  { h: 'droid', w: 'today', model: 'gpt-6-sol', input: 800, output: 120, cacheRead: 3000, cacheWrite: 40 },
  { h: 'droid', w: 'd45', model: 'gpt-6-sol', input: 90, output: 9, cacheRead: 0, cacheWrite: 0 },
  // Qwen Code (one record per reply; prompt tokens include the cached part; no price row, so cost 0)
  { h: 'qwen', w: 'today', model: 'qwen3.5', input: 600, output: 70, cacheRead: 400, cacheWrite: 0 },
  { h: 'qwen', w: 'd45', model: 'qwen3.5', input: 50, output: 5, cacheRead: 0, cacheWrite: 0 },
];

const HARNESSES = ['Claude Code', 'Codex', 'OpenCode', 'Pi', 'Droid', 'Qwen Code'];
const SRC = { claude: 0, codex: 1, opencode: 2, pi: 3, droid: 4, qwen: 5 };

function jsonl(lines) {
  return lines.map((o) => JSON.stringify(o)).join('\n') + '\n';
}

function write(file, text, mtimeMs) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, text);
  if (mtimeMs) fs.utimesSync(file, mtimeMs / 1000, mtimeMs / 1000);
}

// Returns { home, env, expected }. The caller deletes `home` when done.
export function buildFixtures(home, now = Date.now()) {
  // "today" is an hour ago, but never before local midnight, so a run between
  // 00:00 and 01:00 still puts those events on today's day.
  const startOfToday = new Date(now);
  startOfToday.setHours(0, 0, 0, 0);
  const at = (w) => (w === 'today' ? Math.max(now - WINDOWS.today, startOfToday.getTime() + 60e3) : now - WINDOWS[w]);
  const iso = (ms) => new Date(ms).toISOString();
  const WIN = ['today', 'd45', 'd65'];

  // ---- Claude Code: ~/.claude/projects/<project>/<session>.jsonl
  for (const w of WIN) {
    const evs = EVENTS.filter((e) => e.h === 'claude' && e.w === w);
    if (!evs.length) continue;
    const lines = evs.map((e, i) => ({
      type: 'assistant',
      timestamp: iso(at(w) + i * 1000),
      cwd: 'C:\\Users\\test\\claude-proj',
      message: {
        role: 'assistant',
        model: e.model,
        id: `msg_${w}_${i}`,
        usage: {
          input_tokens: e.input,
          output_tokens: e.output,
          cache_read_input_tokens: e.cacheRead,
          cache_creation_input_tokens: e.cacheWrite,
        },
      },
    }));
    write(path.join(home, '.claude', 'projects', '-C-Users-test-claude-proj', `${w}.jsonl`), jsonl(lines));
  }

  // ---- Codex: ~/.codex/sessions/<y>/<m>/<d>/rollout-...-<session id>.jsonl
  for (const w of WIN) {
    const evs = EVENTS.filter((e) => e.h === 'codex' && e.w === w);
    if (!evs.length) continue;
    let total = 0, cum = { input: 0, cached: 0, output: 0 };
    const lines = [];
    evs.forEach((e, i) => {
      const t = at(w) + i * 1000;
      const inputWithCache = e.input + e.cacheRead; // Codex counts cached tokens inside input_tokens
      lines.push({ timestamp: iso(t), type: 'turn_context', payload: { model: e.model, cwd: 'C:\\Users\\test\\codex-proj' } });
      const last = { input_tokens: inputWithCache, cached_input_tokens: e.cacheRead, output_tokens: e.output, reasoning_output_tokens: 0, total_tokens: inputWithCache + e.output };
      cum = { input: cum.input + inputWithCache, cached: cum.cached + e.cacheRead, output: cum.output + e.output };
      total += last.total_tokens;
      const totalUsage = { input_tokens: cum.input, cached_input_tokens: cum.cached, output_tokens: cum.output, reasoning_output_tokens: 0, total_tokens: total };
      lines.push({ timestamp: iso(t), type: 'event_msg', payload: { type: 'token_count', info: { total_token_usage: totalUsage, last_token_usage: last } } });
    });
    write(path.join(home, '.codex', 'sessions', '2026', '11', '14', `rollout-2026-11-14T10-00-00-${w === 'today' ? '22222222-0000-4000-8000-000000000001' : w === 'd45' ? '22222222-0000-4000-8000-000000000045' : '22222222-0000-4000-8000-000000000065'}.jsonl`), jsonl(lines));
  }

  // ---- Pi: ~/.pi/agent/sessions/<folder>/<timestamp>_<session id>.jsonl
  for (const w of WIN) {
    const evs = EVENTS.filter((e) => e.h === 'pi' && e.w === w);
    if (!evs.length) continue;
    const sid = `33333333-0000-4000-8000-000000000${w === 'today' ? '001' : w === 'd45' ? '045' : '065'}`;
    const lines = [{ type: 'session', version: 3, id: sid, timestamp: iso(at(w)), cwd: 'C:\\Users\\test\\pi-proj' }];
    if (w === 'today') lines.push({ type: 'session_info', id: 'si1', timestamp: iso(at(w)), name: 'Pi fixture title' });
    evs.forEach((e, i) => {
      lines.push({ type: 'message', id: `pm_${w}_${i}`, parentId: null, timestamp: iso(at(w) + i * 1000), message: { role: 'user', content: 'hi', timestamp: at(w) + i * 1000 } });
      lines.push({
        type: 'message', id: `pm_${w}_${i}_a`, parentId: null, timestamp: iso(at(w) + i * 1000 + 500),
        message: { role: 'assistant', provider: 'opencode-go', model: e.model, content: [{ type: 'text', text: 'ok' }], usage: { input: e.input, output: e.output, cacheRead: e.cacheRead, cacheWrite: e.cacheWrite, reasoning: 0, totalTokens: e.input + e.output + e.cacheRead + e.cacheWrite }, timestamp: at(w) + i * 1000 + 500 },
      });
    });
    if (w === 'today') {
      // an error with zero usage: no tokens, must not add a request
      lines.push({ type: 'message', id: 'pm_err', parentId: null, timestamp: iso(at(w) + 9000), message: { role: 'assistant', provider: 'opencode-go', model: 'deepseek-v4.1-flash', stopReason: 'error', usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0 }, timestamp: at(w) + 9000 } });
    }
    write(path.join(home, '.pi', 'agent', 'sessions', '--C--Users-test--', `2026-11-14T10-00-00-000Z_${sid}.jsonl`), jsonl(lines));
  }

  // ---- OpenCode: $XDG_DATA_HOME/opencode/opencode.db
  const dbDir = path.join(home, 'xdg', 'opencode');
  fs.mkdirSync(dbDir, { recursive: true });
  const db = new DatabaseSync(path.join(dbDir, 'opencode.db'));
  db.exec(`
    CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, slug TEXT NOT NULL, directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
    CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
  `);
  for (const w of WIN) {
    const evs = EVENTS.filter((e) => e.h === 'opencode' && e.w === w);
    if (!evs.length) continue;
    const sid = `ses_fixture_${w}`;
    db.prepare('INSERT INTO session VALUES (?,?,?,?,?,?,?,?)').run(sid, 'p1', 'fixture', 'C:/Users/test/oc-proj', `OpenCode ${w}`, '1', at(w), at(w));
    evs.forEach((e, i) => {
      const t = at(w) + i * 1000;
      db.prepare('INSERT INTO message VALUES (?,?,?,?,?)').run(`msg_user_${w}_${i}`, sid, t, t, JSON.stringify({ role: 'user', time: { created: t } }));
      db.prepare('INSERT INTO message VALUES (?,?,?,?,?)').run(`msg_${w}_${i}`, sid, t + 500, t + 500, JSON.stringify({
        role: 'assistant', modelID: e.model, providerID: 'openai', cost: 0,
        tokens: { total: e.input + e.output + e.reasoning + e.cacheRead + e.cacheWrite, input: e.input, output: e.output, reasoning: e.reasoning, cache: { read: e.cacheRead, write: e.cacheWrite } },
        time: { created: t + 500, completed: t + 900 },
      }));
    });
  }
  db.close();

  // ---- Droid: ~/.factory/sessions/<folder>/<session id>.settings.json (+ the session log for the folder)
  for (const w of WIN) {
    const evs = EVENTS.filter((e) => e.h === 'droid' && e.w === w);
    if (!evs.length) continue;
    const sid = `44444444-0000-4000-8000-000000000${w === 'today' ? '001' : w === 'd45' ? '045' : '065'}`;
    const e = evs[0];
    const folder = path.join(home, '.factory', 'sessions', '-C-Users-test-droid');
    // one settings file per session: the totals are the sum of its events
    const sum = (k) => evs.reduce((a, x) => a + x[k], 0);
    write(path.join(folder, `${sid}.settings.json`), JSON.stringify({
      model: e.model,
      tokenUsage: { inputTokens: sum('input'), outputTokens: sum('output'), cacheReadTokens: sum('cacheRead'), cacheCreationTokens: sum('cacheWrite'), thinkingTokens: 0 },
    }, null, 2), at(w));
    write(path.join(folder, `${sid}.jsonl`), jsonl([{ type: 'session_start', id: sid, title: 'New Session', cwd: 'C:\\Users\\test\\droid-proj' }]), at(w));
  }

  // ---- Qwen Code: ~/.qwen/projects/<folder>/chats/<session id>.jsonl, one assistant record per reply
  for (const w of WIN) {
    const evs = EVENTS.filter((e) => e.h === 'qwen' && e.w === w);
    if (!evs.length) continue;
    const sid = `55555555-0000-4000-8000-000000000${w === 'today' ? '001' : w === 'd45' ? '045' : '065'}`;
    const lines = evs.map((e, i) => ({
      uuid: `${sid}-${i}`,
      timestamp: iso(at(w)),
      type: 'assistant',
      cwd: 'C:\\Users\\test\\qwen-proj',
      model: e.model,
      usageMetadata: { promptTokenCount: e.input + e.cacheRead, candidatesTokenCount: e.output, thoughtsTokenCount: 0, cachedContentTokenCount: e.cacheRead },
    }));
    write(path.join(home, '.qwen', 'projects', '-C-Users-test-qwen-proj', 'chats', `${sid}.jsonl`), jsonl(lines), at(w));
  }

  // ---- expected numbers per harness and window
  const expected = HARNESSES.map(() => ({ today: { tokens: 0, requests: 0, cost: 0, sessions: 0 }, d45: { tokens: 0, requests: 0, cost: 0, sessions: 0 }, d65: { tokens: 0, requests: 0, cost: 0, sessions: 0 } }));
  for (const e of EVENTS) {
    const s = expected[SRC[e.h]][e.w];
    const u = { input: e.input, output: e.output + (e.reasoning || 0), cacheRead: e.cacheRead, cacheWrite: e.cacheWrite };
    s.tokens += e.input + e.output + (e.reasoning || 0) + e.cacheRead + e.cacheWrite;
    s.requests += 1;
    s.cost += costOf(e.model, u, at(e.w));
  }
  // each harness writes one session per window that has events
  for (let h = 0; h < HARNESSES.length; h++) for (const w of WIN) {
    if (EVENTS.some((e) => SRC[e.h] === h && e.w === w)) expected[h][w].sessions = 1;
  }
  const sumOf = (list, key) => list.reduce((a, s) => a + s[key], 0);
  return {
    home,
    env: { USERPROFILE: home, HOME: home, LOCALAPPDATA: path.join(home, 'AppData', 'Local'), XDG_DATA_HOME: path.join(home, 'xdg') },
    harnesses: HARNESSES,
    // the default 30 day range, per harness (index = harness number)
    range30: expected.map((e) => e.today),
    // the previous 30 days, per harness: the d45 events
    previous30: expected.map((e) => e.d45),
    // sessions for the All time range: one per window that has any use
    sessionsAllTime: expected.map((e) => WIN.filter((w) => e[w].requests > 0).length),
    allTime: expected.map((e) => ({ tokens: sumOf(WIN.map((w) => e[w]), 'tokens'), requests: sumOf(WIN.map((w) => e[w]), 'requests'), cost: sumOf(WIN.map((w) => e[w]), 'cost') })),
  };
}
