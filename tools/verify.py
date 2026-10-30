"""Independent check of JTokens' assembly scanner: full JSON parse of every
line, then per-model token totals compared with `jtokens --json` output."""
import json, os, sys
from collections import defaultdict

home = "\\\\?\\" + os.path.expanduser("~")  # long-path prefix
tot = defaultdict(lambda: [0, 0, 0, 0, 0])  # input, output, cache_read, cache_write, events

def jsonl(root):
    for d, _, fs in os.walk(root):
        for f in fs:
            if f.endswith(".jsonl"):
                yield os.path.join(d, f)

seen = set()
files = sorted(jsonl(os.path.join(home, ".claude", "projects")), key=os.path.getmtime)
for p in files:
    with open(p, "rb") as fh:
        for line in fh:
            if b'"usage"' not in line:
                continue
            try:
                o = json.loads(line)
            except Exception:
                continue
            if o.get("type") != "assistant":
                continue
            m = o.get("message") or {}
            u = m.get("usage")
            if not isinstance(u, dict) or m.get("role") != "assistant":
                continue
            model = m.get("model") or "unknown"
            if model.startswith("<"):
                continue
            i, out = u.get("input_tokens", 0) or 0, u.get("output_tokens", 0) or 0
            cr, cw = u.get("cache_read_input_tokens", 0) or 0, u.get("cache_creation_input_tokens", 0) or 0
            if i == out == cr == cw == 0:
                continue
            mid = m.get("id")
            if mid:
                if mid in seen:
                    continue
                seen.add(mid)
            t = tot[(0, model)]
            t[0] += i; t[1] += out; t[2] += cr; t[3] += cw; t[4] += 1

cx = []  # (first ts, events) per file; dedupe replayed fork history oldest first
for root in ("sessions", "archived_sessions"):
    for p in jsonl(os.path.join(home, ".codex", root)):
        model, last, evs, first_model = None, 0, [], None
        with open(p, "rb") as fh:
            for line in fh:
                if b'"turn_context"' not in line and b'"token_count"' not in line:
                    continue
                try:
                    o = json.loads(line)
                except Exception:
                    continue
                pl = o.get("payload") or {}
                if o.get("type") == "turn_context":
                    if pl.get("model"):
                        model = pl["model"]
                        first_model = first_model or model
                    continue
                if o.get("type") != "event_msg" or pl.get("type") != "token_count":
                    continue
                info = pl.get("info") or {}
                lu = info.get("last_token_usage")
                if not lu:
                    continue
                total = (info.get("total_token_usage") or {}).get("total_tokens", 0) or 0
                if total and total == last:
                    continue
                last = total
                i, out = lu.get("input_tokens", 0) or 0, lu.get("output_tokens", 0) or 0
                if i == 0 and out == 0:
                    continue
                c0 = lu.get("cached_input_tokens", 0) or 0
                c = min(c0, i)
                key = (total, i, out, c0) if total else None
                evs.append([o.get("timestamp", ""), key, model, i - c, out, c])
        for e in evs:
            e[2] = e[2] or first_model or "unknown"
        if evs:
            cx.append((min(e[0] for e in evs), evs))
cseen = set()
for _, evs in sorted(cx, key=lambda x: x[0]):
    for ts, key, model, i, out, c in evs:
        if key is not None:
            if key in cseen:
                continue
            cseen.add(key)
        t = tot[(1, model)]
        t[0] += i; t[1] += out; t[2] += c; t[4] += 1

app = json.load(open(sys.argv[1], encoding="utf8"))
amod = defaultdict(lambda: [0, 0, 0, 0, 0])
for r in app["rows"]:
    m = app["models"][r[0]]
    a = amod[(m["src"], m["id"])]
    a[0] += r[3]; a[1] += r[4]; a[2] += r[5]; a[3] += r[6]; a[4] += r[8]

bad = 0
for k in sorted(set(tot) | set(amod), key=lambda k: -sum(tot[k][:4])):
    py, asm = tot[k], amod[k]
    diff = [a - b for a, b in zip(asm, py)]
    rel = max(abs(d) / max(1, p) for d, p in zip(diff[:4], py[:4]))
    flag = "OK " if rel < 0.001 else "DIFF"
    bad += flag != "OK "
    print(f"{flag} {['claude','codex'][k[0]]:6} {k[1]:28} events py={py[4]:>7} asm={asm[4]:>7}  tokens py={sum(py[:4]):>14,} asm={sum(asm[:4]):>14,}  maxrel={rel:.2e}")
print("models with >0.1% difference:", bad)
