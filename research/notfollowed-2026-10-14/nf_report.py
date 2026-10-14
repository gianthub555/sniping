#!/usr/bin/env python3
"""Report on the coins bigboss skipped as "deployer not followed" (nf_build.py's data).

1. Each group's coins: life (outsider trades after 3/10/30/60 min), peaks of a slot-0 entry,
   completion; where the winners went.
2. The skipped winners by name (the 25 best peaks within 60 min).
3. What the skipped winners looked like when the bot decided (D = 0) and a minute later (D = 60):
   medians against the other skipped coins, and the winner rate by quintile of each feature.
4. Money: a 0.1 SOL buy of every skipped coin at D = 0/30/60 with each exit (the best exit picked on
   the same coins: an upper bound); then filters found honestly on the skipped coins in time order
   (60% train / 20% validation / 20% test): conditions on the features at D, the exit chosen on
   train, ranked on validation, reported on test only, with a 95% interval.

usage: python3 -I nf_report.py nf_data.pkl > report.md
"""
import collections, math, pickle, sys, time
import numpy as np

d = pickle.load(open(sys.argv[1], "rb"))
recs, RULES, ENTRIES = d["recs"], d["rules"], d["entries"]
hours = (max(r["t"] for r in recs) - min(r["t"] for r in recs)) / 3600


def rule_name(r):
    tp, sl, tr, hold = r
    parts = [f"TP x{tp:g}" if tp != math.inf else "no TP"]
    if sl:
        parts.append(f"stop x{sl:g}")
    if tr:
        parts.append(f"trail {int(tr * 100)}%")
    parts.append(f"{hold // 60} min max" if hold >= 60 else f"{hold} s max")
    return ", ".join(parts)


def ci(v):
    v = np.asarray(v, dtype=float)
    if len(v) < 2:
        return float("nan"), float("nan")
    m, se = v.mean(), v.std(ddof=1) / math.sqrt(len(v))
    return m - 1.96 * se, m + 1.96 * se


def pct(a, b):
    return f"{100 * a / b:.1f}%" if b else "-"


def winner(r):
    return r["peak10"] >= 2.0


def longlife(r):
    return r["late"][30] >= 5 and r["peak60"] >= 2.0


print(f"# Coins bigboss skipped as \"deployer not followed\" (recording {time.strftime('%m-%d %H:%M', time.gmtime(min(r['t'] for r in recs)))} to "
      f"{time.strftime('%m-%d %H:%M', time.gmtime(d['end']))} UTC)\n")
print(f"SOL-curve, non-mayhem coins with 60 min of data after the create: {len(recs)} over {hours:.1f} h. "
      f"Group = the Germany bot's own decision for the mint (its log). A slot-0 entry = right after the create slot "
      f"(the bot's snipe); x = what selling all would return over the {d['size'] / 1e9:g} SOL paid (1.25% a side). "
      f"Winner = peak x >= 2 within 10 min; long life = 5+ outsider trades after 30 min and peak x >= 2 within 60 min.\n")

print("## 1. The groups\n")
print("| group | coins | an hour | lives 3 min | 10 min | 30 min | 60 min | median life | winner (2x in 10 min) | 5x in 60 min | 10x in 60 min | long life | completed |")
print("|---|---|---|---|---|---|---|---|---|---|---|---|---|")
groups = collections.defaultdict(list)
for r in recs:
    groups[r["group"]].append(r)
for g in ("not_followed", "followed", "other", "unseen"):
    v = groups.get(g, [])
    if not v:
        continue
    n = len(v)
    live = {m: sum(1 for r in v if r["late"][m] >= 5) for m in (3, 10, 30, 60)}
    print(f"| {g} | {n} | {n / hours:.0f} | {pct(live[3], n)} | {pct(live[10], n)} | {pct(live[30], n)} | {pct(live[60], n)} | "
          f"{np.median([r['life_s'] for r in v]) / 60:.1f} min | {pct(sum(winner(r) for r in v), n)} | {pct(sum(r['peak60'] >= 5 for r in v), n)} | "
          f"{pct(sum(r['peak60'] >= 10 for r in v), n)} | {pct(sum(longlife(r) for r in v), n)} | {pct(sum(r['completed'] for r in v), n)} |")
W = [r for r in recs if winner(r)]
L = [r for r in recs if longlife(r)]
print(f"\nWinners: {len(W)} ({len(W) / hours:.0f} an hour): " + ", ".join(f"{g} {sum(1 for r in W if r['group'] == g)}" for g in ("not_followed", "followed", "other", "unseen")))
print(f"Long-life coins: {len(L)} ({len(L) / hours:.0f} an hour): " + ", ".join(f"{g} {sum(1 for r in L if r['group'] == g)}" for g in ("not_followed", "followed", "other", "unseen")))

NF = sorted(groups["not_followed"], key=lambda r: r["t"])
print("\n## 2. The skipped winners by name (best peak of a slot-0 entry within 60 min)\n")
print("| created (UTC) | name | symbol | mint | peak 10 min | peak 60 min | life | outsider trades after 30 min | completed | dev buy SOL | create-slot buyers / SOL | deployer's earlier coins (recording) |")
print("|---|---|---|---|---|---|---|---|---|---|---|---|")
for r in sorted(NF, key=lambda r: -r["peak60"])[:25]:
    f0 = r["feats"][0]
    print(f"| {time.strftime('%m-%d %H:%M', time.gmtime(r['t']))} | {r['name'][:24].replace('|', '/')} | {r['sym'][:10].replace('|', '/')} | `{r['mint']}` | "
          f"{r['peak10']:.1f}x | {r['peak60']:.1f}x | {r['life_s'] / 60:.0f} min | {r['late'][30]} | {'yes' if r['completed'] else 'no'} | "
          f"{f0['dev_buy_sol']:.2f} | {f0['slot_buyers']:.0f} / {f0['slot_buy_sol']:.1f} | {f0['dep_prior']:.0f} |")

print("\n## 3. What they looked like when the bot decided\n")
for D in (0, 60):
    keys = list(NF[0]["feats"][D].keys())
    win = [r for r in NF if winner(r)]
    rest = [r for r in NF if not winner(r)]
    print(f"### D = {D} s ({len(win)} winners of {len(NF)} skipped coins, {pct(len(win), len(NF))})\n")
    print("| feature | winners (median) | the rest (median) | winner rate by quintile of the feature [coins] |")
    print("|---|---|---|---|")
    X = np.array([[r["feats"][D][k] for k in keys] for r in NF], dtype=float)
    y = np.array([1.0 if winner(r) else 0.0 for r in NF])
    for j, k in enumerate(keys):
        col = X[:, j]
        if len(set(col)) <= 1:
            continue
        if len(set(col)) <= 3:
            parts = [f"{v:g}: {100 * y[col == v].mean():.0f}% [{(col == v).sum()}]" for v in sorted(set(col))]
        else:
            qs = np.unique(np.quantile(col, [0, .2, .4, .6, .8, 1]))
            parts = []
            for a, b in zip(qs[:-1], qs[1:]):
                m = (col >= a) & ((col <= b) if b == qs[-1] else (col < b))
                if m.sum():
                    parts.append(f"{a:.3g}-{b:.3g}: {100 * y[m].mean():.0f}% [{m.sum()}]")
        print(f"| {k} | {np.median(col[y == 1]):.3g} | {np.median(col[y == 0]):.3g} | {' / '.join(parts)} |")
    print()

print("## 4. Money: a 0.1 SOL buy (1.25% a side, 0.0025 SOL a round trip)\n")
print("### Every skipped coin, the best exit on the same coins (an upper bound)\n")
print("| entry | coins | best exit | mean mSOL a trade | 95% interval | positive |")
print("|---|---|---|---|---|---|")
for D in ENTRIES:
    rows = [r for r in NF if D in r["pnl"]]
    P = np.stack([r["pnl"][D] for r in rows]) / 1e6
    j = int(np.argmax(P.mean(axis=0)))
    lo, hi = ci(P[:, j])
    print(f"| {D} s | {len(rows)} | {rule_name(RULES[j])} | {P[:, j].mean():+.2f} | [{lo:+.2f}, {hi:+.2f}] | {pct((P[:, j] > 0).sum(), len(rows))} |")

print("\n### Filters found honestly (time order: 60% train, 20% validation, 20% test)\n")
print("Conditions on the features at D (single ones, then pairs of the best 40), thresholds at the train "
      "quintiles; each candidate's exit is the best one on train (30+ train trades); candidates ranked on "
      "validation (15+ trades, mean and the low end of the interval); the 3 best reported on test only.\n")
for D in ENTRIES:
    rows = [r for r in NF if D in r["pnl"]]
    n = len(rows)
    a, b = int(n * 0.6), int(n * 0.8)
    split = {"train": slice(0, a), "val": slice(a, b), "test": slice(b, n)}
    keys = list(rows[0]["feats"][D].keys())
    X = np.array([[r["feats"][D][k] for k in keys] for r in rows], dtype=float)
    P = np.stack([r["pnl"][D] for r in rows]) / 1e6
    conds = []
    for j, k in enumerate(keys):
        col = X[split["train"], j]
        for q in np.unique(np.quantile(col, [.2, .4, .6, .8])):
            conds.append((f"{k} >= {q:.3g}", X[:, j] >= q))
            conds.append((f"{k} < {q:.3g}", X[:, j] < q))

    def score(mask):
        tr = mask.copy(); tr[a:] = False
        if tr.sum() < 30:
            return None
        e = int(np.argmax(P[tr].mean(axis=0)))
        va = mask.copy(); va[:a] = False; va[b:] = False
        if va.sum() < 15:
            return None
        v = P[va, e]
        return e, P[tr, e].mean(), v.mean(), ci(v)[0], int(tr.sum()), int(va.sum())

    scored = []
    for name, m in conds:
        s = score(m)
        if s:
            scored.append((name, m, s))
    scored.sort(key=lambda x: -x[2][1])
    top = scored[:40]
    for (n1, m1, _), (n2, m2, _) in ((x, y) for i, x in enumerate(top) for y in top[i + 1:]):
        if n1.split()[0] == n2.split()[0]:
            continue
        s = score(m1 & m2)
        if s:
            scored.append((f"{n1} AND {n2}", m1 & m2, s))
    scored.sort(key=lambda x: (-(x[2][2] + x[2][3]) / 2))
    print(f"#### Entry {D} s after the create ({n} skipped coins; train to {time.strftime('%m-%d %H:%M', time.gmtime(rows[a]['t']))}, test from {time.strftime('%m-%d %H:%M', time.gmtime(rows[b]['t']))} UTC)\n")
    print("| filter | exit (from train) | train mSOL [n] | validation mSOL [n] | **test mSOL** | test 95% interval | test trades (an hour) | test positive |")
    print("|---|---|---|---|---|---|---|---|")
    test_hours = (rows[-1]["t"] - rows[b]["t"]) / 3600
    for name, m, (e, trm, vam, valo, ntr, nva) in scored[:3]:
        te = m.copy(); te[:b] = False
        v = P[te, e]
        lo, hi = ci(v)
        print(f"| {name} | {rule_name(RULES[e])} | {trm:+.2f} [{ntr}] | {vam:+.2f} [{nva}] | **{v.mean():+.2f}** | [{lo:+.2f}, {hi:+.2f}] | {len(v)} ({len(v) / max(test_hours, 1e-9):.1f}) | {pct((v > 0).sum(), len(v))} |")
    print()

# 5. The Frankfurt bot's own skips (its log), when a second skips file is given.
if len(sys.argv) > 2:
    fr = {}
    for line in open(sys.argv[2]):
        p = line.rstrip("\n").split("\t")
        if len(p) == 3:
            k = p[2]
            g = "not_followed" if k == "not_followed" else ("followed" if k.startswith(("score", "bundle")) else ("mayhem" if k == "mayhem" else "other"))
            fr.setdefault(p[1], g)
    by_mint = {r["mint"]: r for r in recs}
    print("## 5. The Frankfurt bot's skips (this server's log, 14 Oct)\n")
    print(f"Mints in the Frankfurt log: {len(fr)}; of them with 60 min of recorded data after the create (SOL curve, non-mayhem): "
          f"{sum(1 for m in fr if m in by_mint)}.\n")
    print("| Frankfurt decision | coins | lives 30 min | winner (2x in 10 min) | 5x in 60 min | long life | completed |")
    print("|---|---|---|---|---|---|---|")
    fg = collections.defaultdict(list)
    for m, g in fr.items():
        if m in by_mint:
            fg[g].append(by_mint[m])
    for g in ("not_followed", "followed", "other"):
        v = fg.get(g, [])
        if v:
            n = len(v)
            print(f"| {g} | {n} | {pct(sum(r['late'][30] >= 5 for r in v), n)} | {pct(sum(winner(r) for r in v), n)} | "
                  f"{pct(sum(r['peak60'] >= 5 for r in v), n)} | {pct(sum(longlife(r) for r in v), n)} | {pct(sum(r['completed'] for r in v), n)} |")
    v = fg.get("not_followed", [])
    if v:
        print("\nThe best of Frankfurt's skipped coins:\n")
        print("| created (UTC) | name | mint | peak 10 min | peak 60 min | life | outsider trades after 30 min |")
        print("|---|---|---|---|---|---|---|")
        for r in sorted(v, key=lambda r: -r["peak60"])[:15]:
            print(f"| {time.strftime('%m-%d %H:%M', time.gmtime(r['t']))} | {r['name'][:24].replace('|', '/')} | `{r['mint']}` | {r['peak10']:.1f}x | {r['peak60']:.1f}x | {r['life_s'] / 60:.0f} min | {r['late'][30]} |")
        P = np.stack([r["pnl"][0] for r in v if 0 in r["pnl"]]) / 1e6
        j = int(np.argmax(P.mean(axis=0)))
        lo, hi = ci(P[:, j])
        print(f"\nBuying every Frankfurt-skipped coin at the snipe (0.1 SOL), best exit on the same coins: {P[:, j].mean():+.2f} mSOL a trade "
              f"[{lo:+.2f}, {hi:+.2f}] ({rule_name(RULES[j])}, {len(P)} trades) — an upper bound.")
