#!/usr/bin/env python3
"""What first place behind the creator's bundle would earn (2026-10-14).

For every coin whose bundle size the bots logged (logs/positions.csv of Germany and Frankfurt:
bundle_buys = buys in the create's bundle besides the create transaction's own), in the survival
recorder's data (trades in execution order, analyze.chain_order):
  entry "first": our buy right after the create transaction's own buys and the bundle's buys, ahead
                 of every other buyer of the create slot (the position a winning priority fee buys);
  entry "slot":  our buy after the whole create slot (behind the snipers: where the bot lands now).
Our buy moves the curve (constant product, 1.25% fee a side); every later price counts it; a sell
executes at the end of the slot after its trigger. Sizes 0.3, 0.5, 1 SOL; exits: a grid. PnL is
gross of fixed costs (priority fee, tips: nf_rp_report.py takes them off).
Follow rule (bigboss): the coin's creator or deployer traded by a reference wallet in the 41 h
before the create, or that key's profit to them >= 0.77 SOL by then (the history file's lines
before the create only). Two histories: the 8RvtT8-only file (before 14 Oct 18:00) and the
4-wallet file (8RvtT8, 2c4Nd25B, pXGQ6J2z; 81eQ has no curve trades).

usage: python3 -I rp_build.py DATA_DIR POSITIONS_CSV[,..] HISTORY_OLD HISTORY_NEW OUT.pkl
"""
import collections, csv, glob, itertools, math, os, pickle, sys, time
import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
DATA, POS, H_OLD, H_NEW, OUT = sys.argv[1:6]
sys.argv = [sys.argv[0], DATA]
sys.path.insert(0, HERE)
import analyze as A  # noqa: E402

FEE = 0.0125
SIZES = [0.3e9, 0.5e9, 1.0e9]
HORIZON = 600
TPS = [1.15, 1.25, 1.5, 2.0, 3.0, math.inf]
SLS = [0.0, 0.85, 0.7]
TRAILS = [0.0, 0.2]
HOLDS = [15, 30, 60, 120, 300, 600]
RULES = list(itertools.product(TPS, SLS, TRAILS, HOLDS))
FOLLOW_S = 41 * 3600
MIN_PROFIT = 0.77e9


def load_history(path):
    by_key = collections.defaultdict(list)
    for line in open(path):
        p = line.split()
        if len(p) == 3 and not p[0].startswith("#"):
            try:
                by_key[p[1]].append((int(p[0]), int(p[2])))
            except ValueError:
                pass
    for k in by_key:
        by_key[k].sort()
    return by_key


def followed(hist, keys, t):
    for k in keys:
        rows = hist.get(k)
        if not rows:
            continue
        times = [a for a, _ in rows]
        i = np.searchsorted(times, t, side="left")
        if i and t - times[i - 1] <= FOLLOW_S:
            return True
        if sum(l for _, l in rows[:i]) >= MIN_PROFIT:
            return True
    return False


def path_from(state, t_entry, later, size, slot0):
    vs, vt = state
    if vt <= 279_900_000_000_000:
        return None
    net = size / (1 + FEE)
    tok = vt - vs * vt / (vs + net)

    def x_of(vs_, vt_):
        vs2, vt2 = vs_ + net, max(vt_ - tok, 1.0)
        return vs2 * tok / (vt2 + tok) * (1 - FEE) / size

    x0 = x_of(vs, vt)
    if not later:
        return t_entry, np.array([x0]), np.array([t_entry]), np.array([slot0]), x0
    return (t_entry, np.array([x_of(l[7], l[8]) for l in later]), np.array([l[0] for l in later]),
            np.array([l[1] for l in later]), x0)


def exit_x(path, rule):
    """The x (SOL back over SOL paid) of one exit on one entry path."""
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
        return x[lim - 1] if lim > 0 else x0
    i = min(cand)
    j = max(np.searchsorted(sl, sl[i] + 1, side="right") - 1, i)
    return x[j]


def main():
    t0 = time.time()
    bundle = {}
    for f in POS.split(","):
        for r in csv.DictReader(open(f)):
            try:
                bundle[r["mint"]] = int(r["bundle_buys"])
            except (ValueError, KeyError):
                pass
    h_old, h_new = load_history(H_OLD), load_history(H_NEW)
    coins, trades = {}, collections.defaultdict(list)
    for path in sorted(glob.glob(os.path.join(DATA, "live_*.tsv"))):
        for line in open(path, errors="replace"):
            p = line.rstrip("\n").split("\t")
            if p[0] == "c" and len(p) >= 17 and p[4] in bundle and p[4] not in coins:
                coins[p[4]] = dict(t=float(p[1]), slot=int(p[2]), sig=p[3], mint=p[4], user=p[5], creator=p[6], name=p[7],
                                   vtok=int(p[10]), vsol=int(p[11]), mayhem=int(p[15]))
            elif p[0] == "t" and len(p) >= 14 and p[4] in bundle:
                trades[p[4]].append((float(p[1]), int(p[2]), p[3], int(p[5]), int(p[6]), int(p[7]), p[8], int(p[9]), int(p[10]), int(p[11]), int(p[12])))
    end = max(t[0] for tl in trades.values() for t in tl)
    print(f"{len(bundle)} mints with a bundle size; {len(coins)} in the recording; loaded in {time.time() - t0:.0f} s", flush=True)
    recs = []
    for m, c in sorted(coins.items(), key=lambda kv: kv[1]["t"]):
        if c["mayhem"] or abs(c["vsol"] - A.V_SOL0) >= 1e9 or c["t"] + HORIZON + 30 > end:
            continue
        tr = A.chain_order(trades.get(m, []))
        slot_tr = [t for t in tr if t[1] == c["slot"]]
        own = [t for t in slot_tr if t[2] == c["sig"]]
        rest = [t for t in slot_tr if t[2] != c["sig"]]
        b = bundle[m]
        before_us = own + [t for t in rest if t[3] == 1][:b]
        # The state we buy into: after the create's own buys and the bundle's (in execution order).
        cut = max((slot_tr.index(t) for t in before_us), default=-1)
        st = slot_tr[cut] if cut >= 0 else None
        state_first = (st[7], st[8]) if st else (c["vsol"], c["vtok"])
        t_first = st[0] if st else c["t"]
        after_first = slot_tr[cut + 1:] + [t for t in tr if t[1] != c["slot"] and t[0] <= t_first + HORIZON]
        last_slot = slot_tr[-1] if slot_tr else None
        state_slot = (last_slot[7], last_slot[8]) if last_slot else (c["vsol"], c["vtok"])
        t_slot = (last_slot[0] if last_slot else c["t"]) + 0.4
        after_slot = [t for t in tr if t[0] > t_slot and t[0] <= t_slot + HORIZON]
        snipers = [t for t in rest if t[3] == 1][b:]
        out = {}
        for s in SIZES:
            for name, state, te, later in (("first", state_first, t_first, after_first), ("slot", state_slot, t_slot, after_slot)):
                p = path_from(state, te, later, s, c["slot"])
                if p is not None:
                    out[(name, s)] = np.array([exit_x(p, r) for r in RULES], dtype=np.float32)
        dev = sum(t[4] for t in own if t[3]) / 1e9
        recs.append(dict(mint=m, t=c["t"], name=c["name"], bundle=b, dev_buy_sol=dev, snipers=len(snipers),
                         sniper_sol=sum(t[4] for t in snipers) / 1e9,
                         follow_old=followed(h_old, {c["creator"], c["user"]}, c["t"]),
                         follow_new=followed(h_new, {c["creator"], c["user"]}, c["t"]), x=out))
    with open(OUT, "wb") as f:
        pickle.dump(dict(recs=recs, rules=RULES, sizes=SIZES), f)
    print(f"wrote {len(recs)} coins in {time.time() - t0:.0f} s; followed old {sum(r['follow_old'] for r in recs)}, new {sum(r['follow_new'] for r in recs)}", flush=True)


if __name__ == "__main__":
    main()
