#!/usr/bin/env python3
"""The coins bigboss skipped as "deployer not followed": what became of them (2026-10-14).

Input: the survival recorder's hourly TSVs (every create and trade, Germany, from 2026-10-13 05:06),
and the Germany bot's own skip decisions from its log (skips.tsv: time, mint, kind). For every
SOL-curve, non-mayhem coin created at least 60 min before the recording's end:

  group      the bot's decision: not_followed / followed (passed the follow check: a score or bundle
             skip, or a paper buy) / other (another early skip) / unseen (not in the bot's log)
  features   at D = 0, 10, 30, 60 s after the create (analyze.features + the deployer's record)
  outcome    life (last trade), outsider trades after 3/10/30/60 min, peak x of a slot-0 entry
             within 10 and 60 min, completion (the curve finished)
  replay     a buy at D = 0, 30, 60 s (strategy.entry_path's mechanics, horizon 60 min) against a
             grid of exits, the PnL of each (lamports, size S, 1.25% a side, fixed cost TIPS)

Output: nf_data.pkl (one record per coin) for nf_report.py.

usage: python3 -I nf_build.py DATA_DIR SKIPS_TSV PAPER_BUYS_CSV OUT.pkl
"""
import collections, itertools, math, os, pickle, sys, time
import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
DATA, SKIPS, BUYS, OUT = sys.argv[1:5]
sys.argv = [sys.argv[0], DATA]
sys.path.insert(0, HERE)
import analyze as A  # noqa: E402

S = 0.1e9
FEE = 0.0125
TIPS = 0.0025e9
HORIZON = 3600
ENTRIES = [0, 30, 60]
TPS = [1.25, 1.5, 2.0, 3.0, 5.0, math.inf]
SLS = [0.0, 0.85, 0.7, 0.5]
TRAILS = [0.0, 0.2, 0.35]
HOLDS = [30, 120, 600, 1800, 3600]
RULES = list(itertools.product(TPS, SLS, TRAILS, HOLDS))


def entry_path(c, tr, d):
    """strategy.entry_path with a 60 min horizon: (entry time, x, t, slot, x at entry) or None."""
    if d == 0:
        slot_tr = [t for t in tr if t[1] == c["slot"]]
        st = slot_tr[-1] if slot_tr else None
        t_entry = (slot_tr[-1][0] if slot_tr else c["t"]) + 0.4
    else:
        before = [t for t in tr if t[0] <= c["t"] + d]
        st = before[-1] if before else None
        t_entry = c["t"] + d
    vs, vt = (st[7], st[8]) if st else (c["vsol"], c["vtok"])
    if vt <= 279_900_000_000_000:
        return None
    net = S / (1 + FEE)
    k = vs * vt
    tok = vt - k / (vs + net)
    later = [t for t in tr if t_entry < t[0] <= t_entry + HORIZON]

    def x_of(vs_, vt_):
        vs2, vt2 = vs_ + net, max(vt_ - tok, 1.0)
        return vs2 * tok / (vt2 + tok) * (1 - FEE) / S

    x0 = x_of(vs, vt)
    if not later:
        return t_entry, np.array([x0]), np.array([t_entry]), np.array([c["slot"]]), x0
    return (t_entry, np.array([x_of(l[7], l[8]) for l in later]), np.array([l[0] for l in later]),
            np.array([l[1] for l in later]), x0)


def exit_pnl(path, rule):
    """strategy.exit_pnl: PnL in lamports of one exit on one entry path (trail armed at 1.1x)."""
    t_entry, x, t, sl, x0 = path
    tp, stop, trail, hold = rule
    n = len(x)
    lim = min(np.searchsorted(t, t_entry + hold, side="left"), n)
    cand = [lim if lim < n else None]
    if lim > 0:
        xs = x[:lim]
        if tp != math.inf:
            i = np.argmax(xs >= tp)
            if xs[i] >= tp:
                cand.append(i)
        if stop:
            i = np.argmax(xs <= stop)
            if xs[i] <= stop:
                cand.append(i)
        if trail:
            peak = np.maximum.accumulate(xs)
            hit = (peak >= 1.1) & (xs <= peak * (1 - trail))
            i = np.argmax(hit)
            if hit[i]:
                cand.append(i)
    cand = [c for c in cand if c is not None]
    if not cand:
        xv = x[lim - 1] if lim > 0 else x0
        return xv * S - S - TIPS
    i = min(cand)
    j = max(np.searchsorted(sl, sl[i] + 1, side="right") - 1, i)
    return x[j] * S - S - TIPS


def main():
    t0 = time.time()
    decision = {}
    for line in open(SKIPS):
        p = line.rstrip("\n").split("\t")
        if len(p) == 3:
            kind = p[2]
            g = "not_followed" if kind == "not_followed" else ("followed" if kind.startswith(("score", "bundle")) else ("mayhem" if kind == "mayhem" else "other"))
            # The first decision on a mint stands (an early skip can be followed by a later one).
            prev = decision.get(p[1])
            if prev is None or (prev == "other" and g != "other"):
                decision[p[1]] = g
    if os.path.exists(BUYS):
        import csv
        for r in csv.DictReader(open(BUYS)):
            if r.get("action") == "BUY" and r.get("mint"):
                decision[r["mint"]] = "followed"
    coins, trades, files = A.load()
    end = max(t[0] for tl in trades.values() for t in tl)
    print(f"loaded {len(coins)} coins from {len(files)} files in {time.time() - t0:.0f} s; data ends {time.strftime('%m-%d %H:%M', time.gmtime(end))} UTC", flush=True)
    ordered = sorted(coins.values(), key=lambda c: c["t"])
    times = [c["t"] for c in ordered]
    dep_hist = collections.defaultdict(list)
    names = collections.defaultdict(list)
    recs = []
    for i, c in enumerate(ordered):
        tr = sorted(trades.get(c["mint"], []))
        ctx_base = {"launches_60s": sum(1 for t in times[max(0, i - 400):i] if c["t"] - t <= 60),
                    "copycats": sum(1 for t in names[c["name"].lower()] if c["t"] - t <= 3600)}
        names[c["name"].lower()].append(c["t"])
        hist = dep_hist[c["user"]]
        sol_curve = c["vsol"] == A.V_SOL0 or abs(c["vsol"] - A.V_SOL0) < 1e9
        complete_window = c["t"] + HORIZON + 60 <= end
        lived3 = None
        if complete_window and sol_curve and not c["mayhem"]:
            _, sw = A.features(c, tr, 0, dict(ctx_base, dep_prior=0, dep_since=86_400, dep_lived=-1.0))
            outsiders = [t for t in tr if t[6] not in sw]
            late = {m: sum(1 for t in outsiders if t[0] >= c["t"] + m * 60) for m in (3, 10, 30, 60)}
            lived3 = late[3] >= A.MIN_LATE
            feats = {}
            for d in (0, 10, 30, 60):
                resolved = [o for (t0_, o) in hist if t0_ + A.LIFE_S + 5 <= c["t"] + d and o is not None]
                ctx = dict(ctx_base, dep_prior=len(hist), dep_since=(c["t"] - hist[-1][0]) if hist else 86_400,
                           dep_lived=(sum(resolved) / len(resolved)) if resolved else -1.0)
                feats[d], _ = A.features(c, tr, d, ctx)
            p0 = entry_path(c, tr, 0)
            def peak(path, secs):
                if path is None:
                    return float("nan")
                t_entry, x, t, _, x0 = path
                m = x[t <= t_entry + secs]
                return float(max([x0] + list(m)))
            completed = any(t[8] <= 279_900_000_000_000 for t in tr)
            pnl = {}
            for d in ENTRIES:
                path = p0 if d == 0 else entry_path(c, tr, d)
                if path is not None:
                    pnl[d] = np.array([exit_pnl(path, r) for r in RULES], dtype=np.float32)
            recs.append(dict(
                mint=c["mint"], t=c["t"], name=c["name"], sym=c["sym"], user=c["user"], creator=c["creator"], uri=c["uri"],
                group=decision.get(c["mint"], "unseen"), feats=feats,
                life_s=(tr[-1][0] - c["t"]) if tr else 0.0, late=late, trades=len(tr),
                peak10=peak(p0, 600), peak60=peak(p0, 3600), completed=completed, pnl=pnl))
        hist.append((c["t"], lived3))
        if i % 5000 == 0:
            print(f"  {i}/{len(ordered)} coins, {len(recs)} kept, {time.time() - t0:.0f} s", flush=True)
    with open(OUT, "wb") as f:
        pickle.dump(dict(recs=recs, rules=RULES, entries=ENTRIES, size=S, tips=TIPS, end=end, files=files), f)
    print(f"wrote {len(recs)} coins to {OUT} in {time.time() - t0:.0f} s; groups {collections.Counter(r['group'] for r in recs)}", flush=True)


if __name__ == "__main__":
    main()
