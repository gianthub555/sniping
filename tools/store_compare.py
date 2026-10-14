#!/usr/bin/env python3
"""Do the files and the store hold the same rows? (2026-10-12, Part A.)

While STORE_DB=on dual-writes, every file and its table should agree: per UTC day, the number of
rows and, for the trade logs and the SOL-carrying files, the daily sums. Turn the files off
(STORE_FILES=off) only once this prints no mismatch for the days since the dual-write began.

usage: tools/store_compare.py [--since YYYY-MM-DD] [--db data/bigboss.sqlite] [--tapes data/tapes]
Exit code 0 only when everything compared matches. Read-only on both sides. Standard library only.
"""
import argparse, csv, datetime as dt, os, sqlite3, sys
from collections import defaultdict

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def day_of_unix(v):
    try:
        x = float(v)
    except (TypeError, ValueError):
        return None
    if x > 1e17:
        x /= 1e9
    elif x > 1e14:
        x /= 1e6
    elif x > 1e11:
        x /= 1e3
    return dt.datetime.fromtimestamp(x, dt.timezone.utc).strftime("%Y-%m-%d") if x > 0 else None


def day_of_iso(v):
    if v and len(v) >= 10 and v[4] == "-":
        return v[:10]
    return day_of_unix(v)  # a time column holding unix seconds (route_skips.csv)


def num(v):
    try:
        return float(v)
    except (TypeError, ValueError):
        return 0.0


# (label, file, table, extra WHERE, day column, day parser, summed columns)
PAIRS = [
    ("trades live", "auto_sell_logs.csv", "trade_log", "mode = 'live'", "timestamp", day_of_iso, ["sol_in", "sol_out", "wallet_sol_change"]),
    ("trades paper", "auto_sell_logs_paper.csv", "trade_log", "mode = 'paper'", "timestamp", day_of_iso, ["sol_in", "sol_out", "wallet_sol_change"]),
    ("creator history", "creator_history.csv", "creator_history", None, "unix_time", day_of_unix, ["dev_buy_lamports"]),
    ("deployer launches", "logs/deployer_launches.csv", "deployer_launches", None, "unix_time", day_of_unix, ["no_pop"]),
    ("positions log", "logs/positions.csv", "positions_log", None, "date", day_of_iso, []),
    ("rival buys", "logs/rival_buys.csv", "rival_buys", None, "date", day_of_iso, ["sol"]),
    ("route skips", "logs/route_skips.csv", "route_skips", None, "time", day_of_iso, []),
    ("shred lead", "logs/shred_lead.csv", "shred_lead", None, "regular_seen_unix_ms", day_of_unix, []),
    ("preexec sightings", "logs/preexec_sightings.csv", "preexec_sightings", None, "seen_unix_ms", day_of_unix, []),
    ("distance daily", "logs/distance_daily.csv", "distance_daily", None, "date", day_of_iso, ["followed"]),
    ("hot stamps", "logs/hot_stamps.csv", "hot_stamps", None, None, None, []),
    ("lane answers", "logs/hot_lane_answers.csv", "lane_answers", None, None, None, []),
    ("hot verify", "logs/hot_verify.csv", "hot_verify", None, None, None, []),
    ("engine parity", "logs/engine_parity.csv", "engine_parity", None, None, None, []),
    ("leader rtt (snapshot)", "logs/leader_rtt.csv", "leader_rtt", None, None, None, []),
]


def file_days(path, day_col, parse, sums):
    out = defaultdict(lambda: [0] + [0.0] * len(sums))
    with open(path, newline="") as f:
        r = csv.DictReader(f)
        for row in r:
            if None in row:  # a line with more fields than the header (malformed)
                continue
            d = parse(row.get(day_col)) if day_col else "all"
            if d is None:
                continue
            acc = out[d]
            acc[0] += 1
            for i, c in enumerate(sums):
                acc[i + 1] += num(row.get(c))
    return out


def table_days(conn, table, where, day_col, parse, sums):
    out = defaultdict(lambda: [0] + [0.0] * len(sums))
    cols = ([f'"{day_col}"'] if day_col else ["1"]) + [f'"{c}"' for c in sums]
    q = f"SELECT {', '.join(cols)} FROM {table}" + (f" WHERE {where}" if where else "")
    for row in conn.execute(q):
        d = parse(str(row[0]) if row[0] is not None else None) if day_col else "all"
        if d is None:
            continue
        acc = out[d]
        acc[0] += 1
        for i in range(len(sums)):
            acc[i + 1] += num(row[i + 1])
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--since", default="")
    ap.add_argument("--db", default=os.environ.get("STORE_PATH", os.path.join(ROOT, "data/bigboss.sqlite")))
    ap.add_argument("--tapes", default=os.environ.get("STORE_TAPES_DIR", os.path.join(ROOT, "data/tapes")))
    a = ap.parse_args()
    conn = sqlite3.connect(f"file:{a.db}?mode=ro", uri=True)
    bad = 0
    print(f"{a.db}{', days from ' + a.since if a.since else ''}")
    for label, path, table, where, day_col, parse, sums in PAIRS:
        path = os.path.join(ROOT, path)
        if not os.path.exists(path):
            print(f"  {label:22s} no file")
            continue
        f = file_days(path, day_col, parse, sums)
        t = table_days(conn, table, where, day_col, parse, sums)
        days = sorted(d for d in set(f) | set(t) if d >= a.since or d == "all")
        diff = []
        for d in days:
            fv, tv = f.get(d, [0] + [0.0] * len(sums)), t.get(d, [0] + [0.0] * len(sums))
            if fv[0] != tv[0] or any(abs(x - y) > 1e-6 * max(1.0, abs(x)) for x, y in zip(fv[1:], tv[1:])):
                diff.append((d, fv, tv))
        n_f = sum(f[d][0] for d in days if d in f)
        n_t = sum(t[d][0] for d in days if d in t)
        if diff:
            bad += 1
            print(f"  {label:22s} MISMATCH on {len(diff)} day(s): file {n_f} rows, store {n_t}")
            for d, fv, tv in diff[:5]:
                extra = "".join(f", {c} {x:.9g} vs {y:.9g}" for c, x, y in zip(sums, fv[1:], tv[1:]) if abs(x - y) > 1e-9)
                print(f"      {d}: rows {fv[0]} vs {tv[0]}{extra}")
        else:
            print(f"  {label:22s} ok ({n_f} rows over {len([d for d in days if d != 'all']) or 1} day(s))")
    # tapes: lines per day file against rows per day database
    tdir = os.path.join(ROOT, "logs/tapes")
    if os.path.isdir(tdir):
        for name in sorted(os.listdir(tdir)):
            if not name.endswith(".jsonl") or name[:10] < a.since:
                continue
            day = name[:10]
            with open(os.path.join(tdir, name)) as fh:
                n_f = sum(1 for line in fh if line.strip())
            dbp = os.path.join(a.tapes, f"{day}.sqlite")
            n_t = sqlite3.connect(f"file:{dbp}?mode=ro", uri=True).execute("SELECT COUNT(*) FROM tapes").fetchone()[0] if os.path.exists(dbp) else 0
            ok = n_f == n_t
            bad += 0 if ok else 1
            print(f"  {'tapes ' + day:22s} {'ok' if ok else 'MISMATCH'} (file {n_f} launches, store {n_t})")
    # the trade log against orders: one order per trade row
    for mode in ("live", "paper"):
        n_log = conn.execute("SELECT COUNT(*) FROM trade_log WHERE mode = ?", (mode,)).fetchone()[0]
        n_ord = conn.execute("SELECT COUNT(*) FROM orders WHERE mode = ?", (mode,)).fetchone()[0]
        ok = n_log == n_ord
        bad += 0 if ok else 1
        print(f"  {'orders ' + mode:22s} {'ok' if ok else 'MISMATCH'} ({n_log} trade-log rows, {n_ord} orders)")
    print("all match" if not bad else f"{bad} mismatch(es)")
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
