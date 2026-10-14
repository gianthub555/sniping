#!/usr/bin/env python3
"""Report on a scorecard + pXGQ-exit test run (2026-10-12): what the entry scored, what it tried to
buy, what filled, how pXGQ's exit sold, and the result, from the SQLite database and the trade log.

usage: tools/scorecard_report.py [SINCE]   SINCE e.g. 2026-10-12T22:00 (default: the last 24 h)
Reads bigboss.db (DB_PATH from .env) and auto_sell_logs_paper.csv (auto_sell_logs.csv when
DRY_RUN=false). Standard library only.
"""
import csv, datetime as dt, json, os, sqlite3, statistics, sys
from collections import Counter, defaultdict

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def env():
    out = {}
    for line in open(os.path.join(ROOT, ".env")):
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            k, v = line.split("=", 1)
            out[k.strip()] = v.strip().strip('"')
    return out


def main():
    e = env()
    since = sys.argv[1] if len(sys.argv) > 1 else (dt.datetime.now(dt.timezone.utc) - dt.timedelta(hours=24)).strftime("%Y-%m-%dT%H:%M")
    t0 = int(dt.datetime.fromisoformat(since).replace(tzinfo=dt.timezone.utc).timestamp())
    db = sqlite3.connect(f"file:{os.path.join(ROOT, e.get('DB_PATH', 'bigboss.db'))}?mode=ro", uri=True)
    paper = e.get("DRY_RUN", "true").lower() != "false"
    log = os.path.join(ROOT, "auto_sell_logs_paper.csv" if paper else "auto_sell_logs.csv")
    hours = max((dt.datetime.now(dt.timezone.utc).timestamp() - t0) / 3600, 1e-9)
    print(f"since {since} UTC ({hours:.1f} h), {'paper' if paper else 'LIVE'}, threshold {e.get('SCORECARD_MIN', '61')}, exit {e.get('SELL_RULE', 'ref')}")

    rows = db.execute("SELECT mint, t, score, passed, verdict, facts FROM score WHERE t >= ?", (t0,)).fetchall()
    passed = [r for r in rows if r[3]]
    print(f"\nENTRY: {len(rows)} followed launches scored ({len(rows) / hours:.1f}/h); {len(passed)} at the threshold or more ({len(passed) / hours:.1f}/h)")
    if rows:
        scores = sorted(r[2] for r in rows)
        print(f"  score p10/50/90: {scores[len(scores) // 10]} / {scores[len(scores) // 2]} / {scores[9 * len(scores) // 10]}")
        worst = Counter()
        for r in rows:
            if not r[3]:
                f = json.loads(r[5])
                for k, v in sorted(f.items(), key=lambda kv: kv[1]["points"])[:2]:
                    if v["points"] < 0:
                        worst[k] += 1
        print("  facts that most often sank a skipped launch:", ", ".join(f"{k} ({n})" for k, n in worst.most_common(5)))

    buys, sells = {}, defaultdict(list)
    if os.path.exists(log):
        for r in csv.DictReader(open(log)):
            ts = dt.datetime.fromisoformat(r["timestamp"].replace("Z", "+00:00")).timestamp()
            if ts < t0:
                continue
            if r["action"] == "BUY":
                buys[r["mint"]] = r
            elif r["action"] == "SELL":
                sells[r["mint"]].append(r)
    tried = [m for m in buys]
    filled = [m for m, r in buys.items() if r["status"] in ("simulated", "confirmed")]
    why = Counter(buys[m]["status"].split(":")[0] for m in tried if m not in filled)
    print(f"\nBUYS: {len(tried)} attempted, {len(filled)} filled ({len(filled) / hours:.1f}/h)")
    if why:
        print("  not filled:", ", ".join(f"{k} ({n})" for k, n in why.most_common()))

    ex = db.execute("SELECT mint, reason, share, value_x, peak_x, held_secs FROM exit_decision WHERE t >= ?", (t0,)).fetchall()
    print(f"\nEXIT decisions (pXGQ): {len(ex)}; by reason: " + ", ".join(f"{k} {n}" for k, n in Counter(r[1] for r in ex).most_common()))
    results = []
    for m in filled:
        cost = float(buys[m]["sol_in"] or 0)
        got = sum(float(s["sol_out"] or 0) for s in sells[m])
        if cost and sells[m]:
            results.append((m, got - cost, got / cost))
    if results:
        pnl = [r[1] for r in results]
        print(f"\nRESULT on {len(results)} closed positions: {sum(pnl):+.4f} SOL (before tips), {sum(p > 0 for p in pnl)}/{len(pnl)} won, "
              f"median x{statistics.median(r[2] for r in results):.3f}")
        holds = [r[5] for r in ex if r[2] >= 0.999]
        if holds:
            print(f"  hold to the final sell: median {statistics.median(holds):.0f} s, max {max(holds):.0f} s")
    else:
        print("\nRESULT: no closed positions yet")
    lr = db.execute("SELECT COUNT(*), AVG(pnl_sol), AVG(down) FROM launch_result WHERE created >= ?", (t0,)).fetchone()
    if lr[0]:
        print(f"\nREFERENCE: {lr[0]} followed launches replayed (0.5 SOL behind the bundle, the bot's exit): mean {lr[1]:+.4f} SOL, {100 * lr[2]:.0f}% went down")


if __name__ == "__main__":
    main()
