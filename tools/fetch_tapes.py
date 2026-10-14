#!/usr/bin/env python3
"""Fetch each traded token's first two minutes of pump.fun trades (its "tape") for the replay
check (`./bot.sh compare`): the tokens bought since SINCE in auto_sell_logs.csv (live) and
auto_sell_logs_paper.csv (dry run), one file per
token in logs/tapes/<mint>.json, the format `rules_check play` reads. Uses RPC_ENDPOINT from .env
(Helius getTransactionsForAddress). Tapes already fetched are kept.

usage: tools/fetch_tapes.py [SINCE, e.g. 2026-09-26T00:00]  (default: the last 24 h)
"""
import concurrent.futures as cf
import csv
import datetime as dt
import json
import os
import struct
import sys
import time
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "logs", "tapes")
PUMP = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"
EVENT_TAG = bytes([0xE4, 0x45, 0xA5, 0x2E, 0x51, 0xCB, 0x9A, 0x1D])
TRADE_EVENT = bytes([0xBD, 0xDB, 0x7F, 0xD3, 0x4E, 0xE6, 0x61, 0xEE])
B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
# Before the buy (the create is a fraction of a second earlier) and after it (longer than any hold).
BEFORE_SECS, AFTER_SECS, MAX_TXS = 30, 120, 3000


def env():
    out = {}
    for line in open(os.path.join(ROOT, ".env")):
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            k, v = line.split("=", 1)
            out[k.strip()] = v.strip().strip('"')
    return out


RPC = env()["RPC_ENDPOINT"]


def rpc(method, params, tries=6):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    for i in range(tries):
        try:
            req = urllib.request.Request(RPC, data=body, headers={"Content-Type": "application/json"})
            reply = json.load(urllib.request.urlopen(req, timeout=60))
            if "error" in reply:
                if i < tries - 1 and reply["error"].get("code") in (-32429, 429, -32603):
                    time.sleep(1 + i)
                    continue
                raise RuntimeError(f"{method}: {reply['error']}")
            return reply["result"]
        except (urllib.error.URLError, TimeoutError):
            time.sleep(1 + i)
    raise RuntimeError(f"{method} failed")


def b58d(s):
    n = 0
    for c in s:
        n = n * 58 + B58.index(c)
    b = n.to_bytes((n.bit_length() + 7) // 8, "big") if n else b""
    return b"\0" * (len(s) - len(s.lstrip("1"))) + b


def b58e(b):
    n = int.from_bytes(b, "big")
    s = ""
    while n:
        n, r = divmod(n, 58)
        s = B58[r] + s
    return "1" * (len(b) - len(b.lstrip(b"\0"))) + s


def trade_events(tx):
    """Payloads of the pump.fun trade events (self-CPI) of one transaction."""
    msg = tx["transaction"]["message"]
    loaded = tx["meta"].get("loadedAddresses") or {}
    keys = msg["accountKeys"] + loaded.get("writable", []) + loaded.get("readonly", [])
    ixs = list(msg["instructions"])
    for inner in tx["meta"].get("innerInstructions") or []:
        ixs += inner["instructions"]
    out = []
    for ix in ixs:
        if keys[ix["programIdIndex"]] != PUMP:
            continue
        d = b58d(ix.get("data", ""))
        if len(d) >= 16 and d[:8] == EVENT_TAG and d[8:16] == TRADE_EVENT:
            out.append(d[16:])
    return out


def tape(mint, lo, hi):
    events, token, n, capped = [], None, 0, False
    while True:
        cfg = {
            "transactionDetails": "full",
            "sortOrder": "asc",
            "limit": 100,
            "encoding": "json",
            "maxSupportedTransactionVersion": 1,
            "filters": {"blockTime": {"gte": lo, "lte": hi}, "status": "succeeded"},
        }
        if token:
            cfg["paginationToken"] = token
        r = rpc("getTransactionsForAddress", [mint, cfg])
        n += len(r["data"])
        for tx in r["data"]:
            for k, b in enumerate(trade_events(tx)):
                if b58e(b[0:32]) != mint:
                    continue
                u = lambda o: struct.unpack_from("<Q", b, o)[0]
                events.append(dict(slot=tx["slot"], idx=tx.get("transactionIndex", 0), k=k, bt=tx["blockTime"],
                                   is_buy=b[48] == 1, sol=u(32), tok=u(40), user=b58e(b[49:81]), vsol=u(89), vtok=u(97)))
        token = r.get("paginationToken")
        if not token or not r["data"]:
            break
        if n >= MAX_TXS:
            capped = True
            break
    events.sort(key=lambda e: (e["slot"], e["idx"], e["k"]))
    return events, n, capped


def main():
    since = sys.argv[1] if len(sys.argv) > 1 else (dt.datetime.now(dt.timezone.utc) - dt.timedelta(hours=24)).strftime("%Y-%m-%dT%H:%M")
    os.makedirs(OUT, exist_ok=True)
    buys = {}
    for name in ("auto_sell_logs.csv", "auto_sell_logs_paper.csv"):
        path = os.path.join(ROOT, name)
        if not os.path.exists(path):
            continue
        for r in csv.DictReader(open(path)):
            if r["action"] == "BUY" and r["timestamp"] >= since and r["mint"] not in buys:
                buys[r["mint"]] = int(dt.datetime.fromisoformat(r["timestamp"].replace("Z", "+00:00")).timestamp())
    todo = [(m, t) for m, t in buys.items() if not os.path.exists(os.path.join(OUT, f"{m}.json"))]
    print(f"{len(buys)} tokens bought since {since}, {len(todo)} tapes to fetch", flush=True)

    def fetch(item):
        mint, bought = item
        events, n, capped = tape(mint, bought - BEFORE_SECS, bought + AFTER_SECS)
        if not events:
            return mint, 0
        json.dump(dict(mint=mint, bt=events[0]["bt"], slot=events[0]["slot"], tape=events, tape_ntx=n, tape_capped=capped),
                  open(os.path.join(OUT, f"{mint}.json"), "w"))
        return mint, len(events)

    with cf.ThreadPoolExecutor(6) as ex:
        for mint, n in ex.map(fetch, todo):
            if n == 0:
                print(f"  {mint}: no trades found", flush=True)
    print("done", flush=True)


if __name__ == "__main__":
    main()
