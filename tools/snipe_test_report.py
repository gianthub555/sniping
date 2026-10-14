#!/usr/bin/env python3
"""The snipe test's daily report and its gate (SNIPE_TEST=1, the owner's spec of 2026-10-13): one test
of the whole bot, launches (pump.fun) and graduations (PumpSwap, SNIPE_TEST_POOL=on) together.

Reads logs/snipe_test.csv (one line per snipe, utils::snipe_test::HEADER). For each UTC day and in
total: snipes (buys that landed) and attempts (buys sent, failed ones included), hit rate (landed
snipes ending above 0), mean PnL per attempt and per landed snipe in mSOL with its 95% interval,
total PnL. PnL is after every cost: fees, tips, failed buys' fees, and token-account rent not
recovered. Then the same numbers split by each of the bot's own rules and facts, as information.

The gate, one for the whole bot (both kinds together): at least 300 live trades on at least 3
different UTC days, and the lower 95% bound of the mean PnL per attempt above 0. The launch-only
and graduation-only numbers and every split are information (to switch a losing kind off), never
the gate.

usage: tools/snipe_test_report.py [--mode live|paper] [--since YYYY-MM-DD] [--day YYYY-MM-DD] [--log PATH]
Standard library only. Read-only.
"""
import argparse, csv, math, os, sys
from collections import defaultdict

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GATE_SNIPES, GATE_DAYS = 300, 3


def num(v):
    try:
        return float(v)
    except (TypeError, ValueError):
        return None


def stats(rows):
    """n attempts, landed, hits, mean/sd/lower/upper per attempt (mSOL), mean per landed, total SOL."""
    pnl = [num(r["pnl_msol"]) or 0.0 for r in rows]
    landed = [r for r in rows if r["tokens"] not in ("", "0")]
    lp = [num(r["pnl_msol"]) or 0.0 for r in landed]
    n = len(pnl)
    mean = sum(pnl) / n if n else None
    sd = math.sqrt(sum((x - mean) ** 2 for x in pnl) / (n - 1)) if n > 1 else None
    half = 1.96 * sd / math.sqrt(n) if sd is not None else None
    return {
        "attempts": n,
        "snipes": len(landed),
        "hits": sum(1 for x in lp if x > 0),
        "hit_rate": (sum(1 for x in lp if x > 0) / len(lp)) if lp else None,
        "mean": mean,
        "sd": sd,
        "lo": mean - half if half is not None else None,
        "hi": mean + half if half is not None else None,
        "mean_landed": sum(lp) / len(lp) if lp else None,
        "total_sol": sum(pnl) / 1000.0,
        "rent_left_sol": sum(num(r["rent_unrecovered_sol"]) or 0.0 for r in rows),
    }


def fmt(x, d=1, signed=True):
    if x is None:
        return "–"
    return f"{x:+.{d}f}" if signed else f"{x:.{d}f}"


def line(label, s, width=34):
    hr = "–" if s["hit_rate"] is None else f"{100 * s['hit_rate']:.0f}%"
    ci = "–" if s["lo"] is None else f"[{s['lo']:+.1f}, {s['hi']:+.1f}]"
    return (f"  {label:<{width}} {s['snipes']:>5} snipes {s['attempts']:>5} tries  hit {hr:>4}  "
            f"mean {fmt(s['mean']):>7} mSOL/try {ci:>16}  per snipe {fmt(s['mean_landed']):>7}  total {s['total_sol']:+.4f} SOL")


def bucket(name, r):
    """The split keys of one line: each of the bot's rules and facts it can be told apart by."""
    out = [("kind", r.get("kind") or "launch")]
    score = num(r["score"])
    if score is not None:
        out.append(("score", "61-64" if score < 65 else "65-69" if score < 70 else "70-79" if score < 80 else "80+"))
    out.append(("follow", r["follow_kind"] or "–"))
    age = num(r["wallet_age_h"])
    out.append(("wallet age", "unknown" if age is None else "<24 h" if age < 24 else "1-7 d" if age < 168 else "7-30 d" if age < 720 else "30 d+"))
    funded = num(r["funded_sol"])
    out.append(("funded", "unknown" if funded is None else "<0.5" if funded < 0.5 else "0.5-2" if funded < 2 else "2-10" if funded < 10 else "10+"))
    dev = num(r["dev_buy_sol"]) or 0
    out.append(("dev buy", "<0.25" if dev < 0.25 else "0.25-0.5" if dev < 0.5 else "0.5-1"))
    out.append(("bundle buyers", r["bundle_wallets"] or "?"))
    out.append(("landed slot", "+" + r["slot_offset"] if r["slot_offset"] else "–"))
    out.append(("exit", r["exit_trigger"] or ("buy failed" if r["tokens"] in ("", "0") else "–")))
    out.append(("whole create slot", "would pass" if r["filter_full_slot"] == "pass" else r["filter_full_slot"] or "unknown"))
    out.append(("path", r["path"]))
    for part in (r["facts"] or "").split(";"):
        if "=" in part and ":" in part:
            fact, rest = part.split("=", 1)
            pts = rest.rsplit(":", 1)[1]
            out.append((f"fact {fact}", f"{int(pts):+d} pts" if pts.lstrip("-").isdigit() else pts))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mode", default="live")
    ap.add_argument("--since", default="")
    ap.add_argument("--day", default="")
    ap.add_argument("--log", default=os.path.join(ROOT, "logs/snipe_test.csv"))
    a = ap.parse_args()
    if not os.path.exists(a.log):
        print(f"{a.log}: no snipes yet")
        return 0
    with open(a.log, newline="") as f:
        rows = [r for r in csv.DictReader(f) if r.get("mode") == a.mode and None not in r]
    rows = [r for r in rows if r["date"] >= a.since and (not a.day or r["date"] == a.day)]
    print(f"snipe test, {a.mode}: {len(rows)} tries in {a.log}{' since ' + a.since if a.since else ''}{' on ' + a.day if a.day else ''}")
    if not rows:
        return 0
    print("PnL after every cost (fees, tips, failed buys, rent not recovered); mean ± 95% interval per try.")
    by_day = defaultdict(list)
    for r in rows:
        by_day[r["date"]].append(r)
    print("\nBy day (UTC)")
    for d in sorted(by_day):
        print(line(d, stats(by_day[d]), 12))
    s = stats(rows)
    print(line("all", s, 12))
    print(f"  rent not recovered: {s['rent_left_sol']:.6f} SOL in total")

    # Visible vs whole slot: how often the bot's view and the full create slot disagree.
    full = [r for r in rows if r["filter_full_slot"]]
    if full:
        differ = [r for r in full if r["filter_full_slot"] != "pass"]
        print(f"\nThe whole create slot (regular stream) would have refused {len(differ)} of {len(full)} "
              f"({100 * len(differ) / len(full):.0f}%) of these snipes: "
              + ", ".join(f"{k} {v}" for k, v in sorted(defaultdict_count(r["filter_full_slot"] for r in differ).items())))

    print("\nSplits by the bot's own rules and facts (information only; the gate is the line 'all')")
    splits = defaultdict(lambda: defaultdict(list))
    for r in rows:
        for k, v in bucket("", r):
            splits[k][v].append(r)
    for k in sorted(splits, key=lambda x: (x.startswith("fact "), x)):
        print(f" {k}")
        for v in sorted(splits[k], key=lambda x: str(x)):
            print(line(str(v), stats(splits[k][v]), 18))

    # By kind (information only), and the graduations' own facts.
    print("\nBy kind (information only)")
    kinds = defaultdict(list)
    for r in rows:
        kinds[r.get("kind") or "launch"].append(r)
    for k in sorted(kinds):
        print(line(k, stats(kinds[k]), 12))
    grads = [r for r in kinds.get("graduation", []) if r["tokens"] not in ("", "0")]
    if grads:
        jb = [r for r in grads if r["jump_before_fill"] != ""]
        jumped = sum(1 for r in jb if r["jump_before_fill"] == "1")
        print(f"\nGraduations bought: {len(grads)}" + (f"; the insiders' jump (1.5x the opening) came before our fill on {jumped} of {len(jb)} ({100 * jumped / len(jb):.0f}%)" if jb else ""))
        fast = sorted(num(r["grad_fast_ms"]) for r in grads if num(r["grad_fast_ms"]) is not None)
        if fast:
            print(f"  curve from under 60 SOL to complete: median {fast[len(fast) // 2]:.0f} ms")
        for col, label in (("px_30s", "+30 s"), ("px_60s", "+60 s"), ("px_120s", "+120 s"), ("px_240s", "+240 s"), ("px_600s", "+600 s")):
            xs = sorted(num(r[col]) / num(r["fill_price"]) for r in grads if num(r[col]) and num(r["fill_price"]))
            if xs:
                print(f"  pool price {label:>7} after the fill: median x{xs[len(xs) // 2]:.3f} of our fill price, above it on {100 * sum(1 for x in xs if x > 1) / len(xs):.0f}% (n {len(xs)})")

    days = sorted(by_day)
    ok_n = s["snipes"] >= GATE_SNIPES
    ok_d = len(days) >= GATE_DAYS
    ok_m = s["lo"] is not None and s["lo"] > 0
    print(f"\nGATE (one for the whole bot, launches and graduations together, {a.mode})")
    print(f"  {'ok ' if ok_n else 'not'} {s['snipes']} trades of {GATE_SNIPES} needed")
    print(f"  {'ok ' if ok_d else 'not'} {len(days)} days of {GATE_DAYS} needed ({', '.join(days)})")
    print(f"  {'ok ' if ok_m else 'not'} lower 95% bound of the mean PnL per try {fmt(s['lo'])} mSOL (must be above 0; mean {fmt(s['mean'])}, sd {fmt(s['sd'], 1, False)})")
    if a.mode != "live":
        print("  (paper: the gate counts live snipes only)")
    print("  => " + ("PASSED: the numbers allow the owner to consider a size increase (never raised by the bot)."
                     if ok_n and ok_d and ok_m and a.mode == "live" else "not passed: keep the size."))
    return 0


def defaultdict_count(items):
    c = defaultdict(int)
    for i in items:
        c[i] += 1
    return c


if __name__ == "__main__":
    sys.exit(main())
