#!/usr/bin/env python3
"""The whale rule's report and gate (WHALE_MODE=1, the owner's spec of 2026-10-13).

Reads logs/whale.csv (one line per trade, processors::whale::HEADER). Per UTC day and in total:
trades (buys that landed), attempts, hit rate, mean PnL per attempt in mSOL with its 95% interval,
total; our modelled or measured impact; exits. Then, as information: by our ratio to the curve's
SOL (0.25-0.5 / 0.5-1 / 1-3x), by others' SOL bought behind us within 90 s (none / < 2 / >= 2 SOL),
by the known followers' SOL behind us, and whether any known follower bought behind us at all.

The gate before any size change: >= 100 live trades on >= 3 UTC days, and the lower 95% bound of
the mean PnL per trade above 0 after all costs. The splits never decide it.

usage: tools/whale_report.py [--mode live|paper] [--since YYYY-MM-DD] [--log PATH]
Standard library only. Read-only.
"""
import argparse, csv, math, os, sys
from collections import defaultdict

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GATE_TRADES, GATE_DAYS = 100, 3


def num(v):
    try:
        return float(v)
    except (TypeError, ValueError):
        return None


def stats(rows):
    pnl = [num(r["pnl_msol"]) or 0.0 for r in rows]
    landed = [r for r in rows if r["tokens"] not in ("", "0")]
    lp = [num(r["pnl_msol"]) or 0.0 for r in landed]
    n = len(pnl)
    mean = sum(pnl) / n if n else None
    sd = math.sqrt(sum((x - mean) ** 2 for x in pnl) / (n - 1)) if n > 1 else None
    half = 1.96 * sd / math.sqrt(n) if sd is not None else None
    imp = [num(r["impact_pct"]) for r in landed if num(r["impact_pct"]) is not None]
    return {
        "tries": n,
        "trades": len(landed),
        "hit": sum(1 for x in lp if x > 0) / len(lp) if lp else None,
        "mean": mean,
        "sd": sd,
        "lo": mean - half if half is not None else None,
        "hi": mean + half if half is not None else None,
        "total": sum(pnl) / 1000.0,
        "impact": sorted(imp)[len(imp) // 2] if imp else None,
    }


def line(label, s, w=16):
    f = lambda x, d=1: "–" if x is None else f"{x:+.{d}f}"
    hit = "–" if s["hit"] is None else f"{100 * s['hit']:.0f}%"
    ci = "–" if s["lo"] is None else f"[{s['lo']:+.1f}, {s['hi']:+.1f}]"
    imp = "–" if s["impact"] is None else f"{s['impact']:.1f}%"
    return (f"  {label:<{w}} {s['trades']:>4} trades {s['tries']:>4} tries  hit {hit:>4}  mean {f(s['mean']):>8} mSOL {ci:>18}  "
            f"total {s['total']:+.4f} SOL  our impact (median) {imp}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mode", default="live")
    ap.add_argument("--since", default="")
    ap.add_argument("--log", default=os.path.join(ROOT, "logs/whale.csv"))
    a = ap.parse_args()
    if not os.path.exists(a.log):
        print(f"{a.log}: no whale trades yet")
        return 0
    with open(a.log, newline="") as f:
        rows = [r for r in csv.DictReader(f) if r.get("mode") == a.mode and None not in r and r["date"] >= a.since]
    print(f"whale rule, {a.mode}: {len(rows)} tries in {a.log}; PnL after every cost (fees, tips, failed buys, rent not recovered)")
    if not rows:
        return 0
    days = defaultdict(list)
    for r in rows:
        days[r["date"]].append(r)
    print("\nBy day (UTC)")
    for d in sorted(days):
        print(line(d, stats(days[d])))
    s = stats(rows)
    print(line("all", s))
    landed = [r for r in rows if r["tokens"] not in ("", "0")]
    ex = defaultdict(int)
    for r in landed:
        ex[r["exit_trigger"] or "–"] += 1
    print("  exits: " + ", ".join(f"{k} {v}" for k, v in sorted(ex.items())))

    def split(title, key):
        groups = defaultdict(list)
        for r in rows:
            groups[key(r)].append(r)
        print(f"\n{title} (information only)")
        for k in sorted(groups):
            print(line(k, stats(groups[k])))

    def ratio_band(r):
        x = num(r["ratio"]) or 0
        return "<0.25x" if x < 0.25 else "0.25-0.5x" if x < 0.5 else "0.5-1x" if x < 1 else "1-3x" if x <= 3 else ">3x"

    def inflow_band(col):
        def k(r):
            x = num(r[col])
            if r["tokens"] in ("", "0"):
                return "buy failed"
            return "none" if not x else "< 2 SOL" if x < 2 else ">= 2 SOL"
        return k

    split("By our ratio to the curve's SOL", ratio_band)
    split("By others' SOL bought behind us within 90 s", inflow_band("behind_sol_90s"))
    split("By the known followers' SOL behind us", inflow_band("follower_sol"))
    split("Any known follower behind us", lambda r: "yes" if r["followers"] else "no")

    ok_n = s["trades"] >= GATE_TRADES
    ok_d = len(days) >= GATE_DAYS
    ok_m = s["lo"] is not None and s["lo"] > 0
    print(f"\nGATE ({a.mode})")
    print(f"  {'ok ' if ok_n else 'not'} {s['trades']} trades of {GATE_TRADES} needed")
    print(f"  {'ok ' if ok_d else 'not'} {len(days)} days of {GATE_DAYS} needed")
    lo_text = "–" if s["lo"] is None else f"{s['lo']:+.1f}"
    print(f"  {'ok ' if ok_m else 'not'} lower 95% bound of the mean PnL per try {lo_text} mSOL (must be above 0)")
    if a.mode != "live":
        print("  (paper: the gate counts live trades only; paper buys never reach the chain, so no follower can buy behind them)")
    print("  => " + ("PASSED: the owner may consider a size change (the bot never changes it)." if ok_n and ok_d and ok_m and a.mode == "live" else "not passed: keep the size."))
    return 0


if __name__ == "__main__":
    sys.exit(main())
