#!/usr/bin/env python3
"""A filter for the coins bigboss skips as "not followed", found and tested walk-forward (2026-10-14).

Only facts the bot can know when it buys:
  D = 0 (the snipe, 0.25 ms after the create): the create and the creator's own buy, the name, the
        metadata host, the hour, market activity, the deployer's record. Not the create slot's other
        buyers: they land ~37 ms later, after the decision.
  D = 10 / 30 / 60 s: everything traded by then as well.
Walk-forward: the skipped coins in time order, 6 equal folds; for each test fold k = 3..6 the
filter (single conditions, then pairs and triples of the best) and its exit are chosen on folds
1..k-1 only, by the lower end of the 95% interval of their mean (at least 100 train trades), then
applied unchanged to fold k. Reported: the test folds pooled (never seen by the choice), at the
real fixed cost (0.0025 SOL a round trip) and gross (no fixed cost: what a larger size, where the
fixed cost weighs less, could approach), 0.1 SOL a buy.

usage: python3 -I nf_filter.py nf_data.pkl
"""
import math, pickle, sys, time
import numpy as np

d = pickle.load(open(sys.argv[1], "rb"))
RULES, TIPS = d["rules"], d["tips"] / 1e6
recs = sorted([r for r in d["recs"] if r["group"] == "not_followed"], key=lambda r: r["t"])
AT_SNIPE = ["dev_buy_sol", "dev_is_creator", "name_len", "name_caps", "name_digits", "name_non_ascii", "sym_in_name",
            "hour_utc", "launches_60s", "copycats_1h", "dep_prior", "dep_since_last_s", "dep_prior_lived",
            "host_ipfs.io", "host_pinata", "host_metadata.j7tracker", "host_other", "host_none"]
LATER = ["trades", "buyers", "net_in_sol", "real_sol", "price_x", "dev_sold_sol", "biggest_buy_sol", "since_last_trade_s"]


def rule_name(r):
    tp, sl, tr, hold = r
    p = [f"TP x{tp:g}" if tp != math.inf else "no TP"]
    if sl:
        p.append(f"stop x{sl:g}")
    if tr:
        p.append(f"trail {int(tr * 100)}%")
    p.append(f"{hold // 60} min" if hold >= 60 else f"{hold} s")
    return ", ".join(p)


def lb(v):
    return v.mean() - 1.96 * v.std(ddof=1) / math.sqrt(len(v)) if len(v) > 2 else -1e9


def ci(v):
    m, se = v.mean(), v.std(ddof=1) / math.sqrt(len(v))
    return m - 1.96 * se, m + 1.96 * se


def search(X, P, keys, train, min_n=100):
    """Best filter on `train` rows: (name, mask over all rows, exit index, train lower bound)."""
    conds = []
    for j, k in enumerate(keys):
        col = X[train, j]
        vals = np.unique(col)
        qs = vals if len(vals) <= 3 else np.unique(np.quantile(col, [.1, .2, .3, .4, .5, .6, .7, .8, .9]))
        for q in qs:
            conds.append((f"{k} >= {q:.3g}", X[:, j] >= q, k))
            conds.append((f"{k} < {q:.3g}", X[:, j] < q, k))

    def score(m):
        t = m & train
        if t.sum() < min_n:
            return None
        means = P[t].mean(axis=0)
        e = int(np.argmax(means))
        return e, lb(P[t, e])

    scored = [(n, m, ks, s) for n, m, k in conds for ks in [(k,)] for s in [score(m)] if s]
    scored.sort(key=lambda x: -x[3][1])
    best = scored[:30]
    level = best
    for _ in range(2):  # pairs, then triples
        nxt = []
        for n1, m1, k1, _ in level:
            for n2, m2, k2, _ in best:
                if k2[0] in k1:
                    continue
                s = score(m1 & m2)
                if s:
                    nxt.append((f"{n1} AND {n2}", m1 & m2, k1 + k2, s))
        nxt.sort(key=lambda x: -x[3][1])
        scored += nxt[:200]
        level = nxt[:30]
    scored.sort(key=lambda x: -x[3][1])
    n, m, _, (e, l) = scored[0]
    return n, m, e, l


print(f"# A filter for the skipped coins, walk-forward ({len(recs)} skipped coins, "
      f"{time.strftime('%m-%d %H:%M', time.gmtime(recs[0]['t']))} to {time.strftime('%m-%d %H:%M', time.gmtime(recs[-1]['t']))} UTC)\n")
for D in d["entries"]:
    rows = [r for r in recs if D in r["pnl"]]
    keys = AT_SNIPE + ([] if D == 0 else LATER)
    X = np.array([[r["feats"][D][k] for k in keys] for r in rows], dtype=float)
    P = np.stack([r["pnl"][D] for r in rows]) / 1e6
    n = len(rows)
    folds = np.array_split(np.arange(n), 6)
    print(f"## Entry {D} s after the create ({'facts at the snipe only' if D == 0 else 'facts traded by then too'})\n")
    print("| test fold (UTC) | filter chosen on the earlier folds | exit | train low end | test trades | test mSOL | gross (no fixed cost) |")
    print("|---|---|---|---|---|---|---|")
    pooled, pooled_gross, hours = [], [], 0.0
    for k in range(2, 6):
        train = np.zeros(n, bool)
        train[np.concatenate(folds[:k])] = True
        test = np.zeros(n, bool)
        test[folds[k]] = True
        name, m, e, l = search(X, P, keys, train)
        v = P[m & test, e]
        pooled.append(v)
        pooled_gross.append(v + TIPS)
        span = (rows[folds[k][-1]]["t"] - rows[folds[k][0]]["t"]) / 3600
        hours += span
        tf = f"{time.strftime('%m-%d %H:%M', time.gmtime(rows[folds[k][0]]['t']))}-{time.strftime('%H:%M', time.gmtime(rows[folds[k][-1]]['t']))}"
        print(f"| {tf} | {name} | {rule_name(RULES[e])} | {l:+.2f} | {len(v)} | {v.mean() if len(v) else float('nan'):+.2f} | {(v + TIPS).mean() if len(v) else float('nan'):+.2f} |")
    v = np.concatenate(pooled)
    g = np.concatenate(pooled_gross)
    if len(v) > 2:
        lo, hi = ci(v)
        glo, ghi = ci(g)
        print(f"\n**Pooled test (never seen by the choice): {len(v)} trades ({len(v) / max(hours, 1e-9):.1f} an hour), "
              f"{v.mean():+.2f} mSOL a trade [{lo:+.2f}, {hi:+.2f}], {100 * (v > 0).mean():.0f}% positive; "
              f"gross {g.mean():+.2f} [{glo:+.2f}, {ghi:+.2f}].**\n")
