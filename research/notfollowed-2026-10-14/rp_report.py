#!/usr/bin/env python3
"""rp_build.py's coins: what first place behind the bundle earns, after the fee that buys it.

Fixed cost a round trip = priority fee (paid on every attempt, a failed one too) + 0.001 SOL tip
(landed buys) + 0.0016 SOL for the sells. Priority fee levels from the first outside buyer's bid
on 14 Oct (Frankfurt positions.csv, 120k CU): p50 0.0032 SOL (first ~half the time), p75 0.0085
(~3 in 4), p90 0.028 (~9 in 10). "first 75%": first place on 3 of 4 attempts, behind the snipers
(after the create slot) on the rest, at the p75 fee.
Exit: walk-forward, 6 time folds; for folds 3..6 the exit with the best mean on the earlier folds
(net, same group and size), applied to the fold; test folds pooled with a 95% interval.

usage: python3 -I rp_report.py rp_data.pkl
"""
import math, pickle, sys, time
import numpy as np

d = pickle.load(open(sys.argv[1], "rb"))
recs, RULES, SIZES = sorted(d["recs"], key=lambda r: r["t"]), d["rules"], d["sizes"]
TIP, SELLS = 0.001e9, 0.0016e9
PRIO = {"p50": 0.0032e9, "p75": 0.0085e9, "p90": 0.028e9}


def rule_name(r):
    tp, sl, tr, hold = r
    p = [f"TP x{tp:g}" if tp != math.inf else "no TP"]
    if sl:
        p.append(f"stop x{sl:g}")
    if tr:
        p.append(f"trail {int(tr * 100)}%")
    p.append(f"{hold} s")
    return ", ".join(p)


def walk_forward(rows, net_of):
    """Pooled test PnL (SOL a trade) with the exit chosen on earlier folds; also the last choice."""
    if len(rows) < 60:
        return None
    N = np.stack([net_of(r) for r in rows])
    folds = np.array_split(np.arange(len(rows)), 6)
    out, last = [], None
    for k in range(2, 6):
        train = np.concatenate(folds[:k])
        e = int(np.argmax(N[train].mean(axis=0)))
        out.append(N[folds[k], e])
        last = e
    v = np.concatenate(out) / 1e9
    se = v.std(ddof=1) / math.sqrt(len(v))
    return v.mean(), v.mean() - 1.96 * se, v.mean() + 1.96 * se, len(v), (v > 0).mean(), last


hours = (recs[-1]["t"] - recs[0]["t"]) / 3600
print(f"# First place behind the creator's bundle: what it earns after its fee\n")
print(f"{len(recs)} coins with a logged bundle size, {time.strftime('%m-%d %H:%M', time.gmtime(recs[0]['t']))} to "
      f"{time.strftime('%m-%d %H:%M', time.gmtime(recs[-1]['t']))} UTC ({hours:.0f} h). Test = later time folds only (exit chosen on earlier ones).\n")
groups = {
    "followed, 8RvtT8 list (today's rule)": lambda r: r["follow_old"],
    "followed, 4-wallet list (new)": lambda r: r["follow_new"],
    "followed, new list, creator bought >= 0.85 SOL": lambda r: r["follow_new"] and r["dev_buy_sol"] >= 0.85,
    "every coin (no filter)": lambda r: True,
}
for gname, gf in groups.items():
    rows = [r for r in recs if gf(r)]
    print(f"## {gname}: {len(rows)} coins\n")
    print("| size | entry | priority fee | **test SOL a trade** | 95% interval | per 1 SOL bought | trades | positive | exit (last chosen) |")
    print("|---|---|---|---|---|---|---|---|---|")
    for s in SIZES:
        def net(entry, prio):
            return lambda r: r["x"][(entry, s)] * s - s - (prio + TIP + SELLS)
        have = [r for r in rows if ("first", s) in r["x"] and ("slot", s) in r["x"]]
        cases = [("behind snipers (now)", net("slot", 0.0001e9), "today's ~0.0001"),
                 ("first place", net("first", PRIO["p50"]), "p50 0.0032"),
                 ("first place", net("first", PRIO["p75"]), "p75 0.0085"),
                 ("first place", net("first", PRIO["p90"]), "p90 0.028")]
        mix = lambda r: 0.75 * (r["x"][("first", s)] * s) + 0.25 * (r["x"][("slot", s)] * s) - s - (PRIO["p75"] + TIP + SELLS)
        cases.append(("first 75% / behind 25%", mix, "p75 0.0085"))
        for entry, f, pname in cases:
            w = walk_forward(have, f)
            if not w:
                continue
            m, lo, hi, n, pos, e = w
            print(f"| {s / 1e9:g} SOL | {entry} | {pname} | **{m:+.4f}** | [{lo:+.4f}, {hi:+.4f}] | {m / (s / 1e9):+.3f} | {n} | {100 * pos:.0f}% | {rule_name(RULES[e])} |")
    print()
