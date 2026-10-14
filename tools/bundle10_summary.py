#!/usr/bin/env python3
"""bundle10 shadow: the daily report and the Stage C gate, from the store's bundle10_shadow
(read-only; no sqlite3 CLI needed). tools/bundle10_summary.sql holds the plain-SQL tables.

usage: python3 tools/bundle10_summary.py [data/bigboss.sqlite] [--day YYYY-MM-DD | --all]
  default: the latest UTC day, then every day together and the gate.

Matches are rows that would pass the rule at the decision (would_pass, what the live path would
buy); "exact" matches pass on the regular stream's exact count. Results are the simulated 0.1 SOL
trade. Days and hours are UTC, by the create's arrival on the hot path (recv_unix_ms).
"""
import datetime, math, os, sqlite3, sys

GATE_DAYS = 7
GATE_MATCHES = 300
GATE_AGREE = 97.0


def day_of(ms):
    return datetime.datetime.fromtimestamp(ms / 1000, datetime.timezone.utc).strftime("%Y-%m-%d")


def hour_of(ms):
    return datetime.datetime.fromtimestamp(ms / 1000, datetime.timezone.utc).hour


def stats(values):
    """n, mean, 95% interval (normal), total, wins: in mSOL except the total (SOL)."""
    v = [x * 1000 for x in values if x is not None]
    n = len(v)
    if n == 0:
        return dict(n=0, mean=None, lo=None, hi=None, total=0.0, wins=0)
    mean = sum(v) / n
    h = 1.96 * math.sqrt(sum((x - mean) ** 2 for x in v) / (n - 1)) / math.sqrt(n) if n > 1 else None
    return dict(n=n, mean=mean, lo=None if h is None else mean - h, hi=None if h is None else mean + h, total=sum(v) / 1000, wins=sum(x > 0 for x in v))


def fmt(s):
    if s["n"] == 0:
        return "n 0"
    ci = "" if s["lo"] is None else f" [{s['lo']:+.2f}, {s['hi']:+.2f}]"
    return f"n {s['n']:>4}  mean {s['mean']:+7.2f} mSOL{ci}  total {s['total']:+.4f} SOL  wins {s['wins']} ({100 * s['wins'] / s['n']:.0f}%)"


def pct(a, b):
    return f"{100 * a / b:.1f}%" if b else "-"


def report(rows, title):
    print(f"\n=== {title}: {len(rows)} rows ===")
    m = [r for r in rows if r["would_pass"] == 1]
    mx = [r for r in rows if r["would_pass_exact"] == 1]
    print(f"matches: decision-time {len(m)}, exact {len(mx)}, both {sum(1 for r in rows if r['would_pass'] == 1 and r['would_pass_exact'] == 1)}")
    ten = [r for r in rows if r["count_exact"] is not None and (r["count_decision"] >= 10 or r["count_exact"] >= 10)]
    both = sum(1 for r in ten if r["count_decision"] >= 10 and r["count_exact"] >= 10)
    print(
        f"count >= 10: decision {sum(r['count_decision'] >= 10 for r in rows)}, exact {sum((r['count_exact'] or 0) >= 10 for r in rows)}; "
        f"agree {pct(both, len(ten))} ({both} of {len(ten)}; decision only {sum(r['count_decision'] >= 10 > (r['count_exact'] or 0) for r in ten)}, "
        f"exact only {sum((r['count_exact'] or 0) >= 10 > r['count_decision'] for r in ten)}); create not on the stream {sum(r['count_exact'] is None for r in rows)}"
    )
    ph = [r for r in rows if r["count_decision"] >= 10 and r["create_seen_exact"] == 1]
    txs = sum(r["txs_decision"] for r in ph)
    phantoms = sum(r["phantoms"] or 0 for r in ph)
    print(f"phantoms (count_decision >= 10): {phantoms} of {txs} counted transactions ({pct(phantoms, txs)}); launches with one {sum((r['phantoms'] or 0) > 0 for r in ph)} of {len(ph)}; matches with one {sum((r['phantoms'] or 0) > 0 for r in ph if r['would_pass'] == 1)}")
    print("P&L of the matches (simulated 0.1 SOL):")
    for col, name in [("pnl60_real_sol", "60 s, real costs    "), ("pnl60_research_sol", "60 s, research costs"), ("pnl300_real_sol", "300 s, real costs   "), ("pnl300_research_sol", "300 s, research costs")]:
        print(f"  {name}  {fmt(stats([r[col] for r in m]))}")
    blocked = sum(r["blocked_by_15sol_cap"] == 1 for r in m)
    print(f"15 SOL cap: {blocked} of {len(m)} matches over it ({pct(blocked, len(m))}); 60 s real:")
    print(f"  bundle <= 15 SOL  {fmt(stats([r['pnl60_real_sol'] for r in m if r['blocked_by_15sol_cap'] == 0]))}")
    print(f"  bundle  > 15 SOL  {fmt(stats([r['pnl60_real_sol'] for r in m if r['blocked_by_15sol_cap'] == 1]))}")
    print("by hour (UTC), 60 s real:")
    for h in sorted({hour_of(r["recv_unix_ms"]) for r in m}):
        print(f"  {h:02d}h  {fmt(stats([r['pnl60_real_sol'] for r in m if hour_of(r['recv_unix_ms']) == h]))}")
    print("open-position limits (60 s real, the matches each limit takes):")
    for k in ("1", "3", "5"):
        print(f"  limit {k}  {fmt(stats([r['pnl60_real_sol'] for r in m if r['taken_l' + k] == 1]))}")
    print(f"  limit 5 with the day's loss stop  {fmt(stats([r['pnl60_real_sol'] for r in m if r['taken_l5'] == 1 and r['day_stopped'] == 0]))}")
    other = {k: sum(r[c] == v for r in m) for k, c, v in [("fee route", "blocked_by_fee_route", 1), ("leader regions", "far_producer", 1), ("distance", "distance_go", 0)]}
    print(f"would also block a match: {other}; 6x ceiling refused {sum(r['miss'] == 'ceiling' for r in rows)}; too many buys {sum(r['too_many_buys'] == 1 for r in rows)}; mixed transactions {sum(r['mixed_decision'] == 1 for r in rows)}")
    ex = [r for r in rows if (r["count_exact"] or 0) >= 10]
    print(
        f"existing counts on exact >= 10 ({len(ex)}): hot under exact {sum(r['count_hot'] is not None and r['count_hot'] < r['count_exact'] for r in ex)}, "
        f"slow under exact {sum(r['count_slow'] is not None and r['count_slow'] < r['count_exact'] for r in ex)}; cashback decode mismatches {sum(r['cashback_ix'] is not None and r['cashback_ix'] != r['cashback_event'] for r in rows)}"
    )
    print(f"ring_drops: {max((r['ring_drops'] or 0 for r in rows), default=0)}")
    return dict(m=m, ten=ten, both=both, phantoms=phantoms, txs=txs)


def gate(rows):
    days = sorted({day_of(r["recv_unix_ms"]) for r in rows})
    s = report(rows, f"all days ({days[0]} .. {days[-1]}, {len(days)} days)")
    pnl = stats([r["pnl60_real_sol"] for r in s["m"]])
    agree = 100 * s["both"] / len(s["ten"]) if s["ten"] else 0.0
    drops = max((r["ring_drops"] or 0 for r in rows), default=0)
    lower = "-" if pnl["lo"] is None else f"{pnl['lo']:+.2f}"
    checks = [
        (f"about a week of shadow ({len(days)} of {GATE_DAYS} days)", len(days) >= GATE_DAYS),
        (f">= {GATE_MATCHES} matches ({len(s['m'])})", len(s["m"]) >= GATE_MATCHES),
        (f"60 s real-cost P&L, 95% lower bound above 0 ({lower} mSOL)", pnl["lo"] is not None and pnl["lo"] > 0),
        (f"phantom rate about 0 ({s['phantoms']} of {s['txs']} counted transactions)", s["txs"] > 0 and s["phantoms"] <= 0.001 * s["txs"]),
        (f"decision/exact agreement >= {GATE_AGREE:.0f}% ({agree:.1f}%)", agree >= GATE_AGREE),
        (f"no ring drops ({drops})", drops == 0),
    ]
    print("\n=== the gate for building Stage C ===")
    for text, ok in checks:
        print(f"  [{'PASS' if ok else 'not yet'}] {text}")
    print(f"  => {'all met' if all(ok for _, ok in checks) else 'not met'}")


def main():
    args = sys.argv[1:]
    path, i = "data/bigboss.sqlite", 0
    while i < len(args):
        if args[i] == "--day":
            i += 2
        elif args[i].startswith("--"):
            i += 1
        else:
            path, i = args[i], i + 1
    db = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    db.row_factory = sqlite3.Row
    try:
        rows = [dict(r) for r in db.execute("SELECT * FROM bundle10_shadow ORDER BY recv_unix_ms")]
    except sqlite3.OperationalError as e:
        sys.exit(f"{path}: {e} (the store is migrated to 0004 when the new build starts)")
    if not rows:
        print(f"{path}: no bundle10 shadow rows yet (rows are written 300 s after each create)")
        kv = db.execute("SELECT value FROM kv WHERE key = 'bundle10_shadow'").fetchone()
        if kv:
            print(f"counters: {kv[0]}")
        return
    if "--all" not in args:
        day = args[args.index("--day") + 1] if "--day" in args else day_of(rows[-1]["recv_unix_ms"])
        report([r for r in rows if day_of(r["recv_unix_ms"]) == day], f"UTC day {day}")
    gate(rows)
    kv = db.execute("SELECT value FROM kv WHERE key = 'bundle10_shadow'").fetchone()
    if kv:
        print(f"\nlatest counters: {kv[0]}")


if __name__ == "__main__":
    main()
