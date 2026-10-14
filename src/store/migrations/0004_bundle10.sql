-- 2026-10-13: bundle10 shadow log (BUNDLE10_SHADOW=true; processors::bundle10_shadow). One row
-- per early-path launch with a dev buy where any of the four counts reaches 8, written 300 s
-- after the create. Nothing in it was sent. Columns in processors::bundle10_shadow::COLUMNS order.
CREATE TABLE bundle10_shadow (
    id                      INTEGER PRIMARY KEY,
    "rx_ns"                   INTEGER NOT NULL,
    "run_id"                  TEXT,
    "mint"                    TEXT,
    "deployer"                TEXT,
    "creator"                 TEXT,
    "create_slot"             INTEGER,
    "create_index"            INTEGER,              -- block index on the regular stream (NULL: create not seen there)
    "recv_unix_ms"            INTEGER,              -- the hot path received the create (early feed, unix ms)
    "create_stream_unix_ms"   INTEGER,              -- the regular stream delivered the create (unix ms; the simulated exits use this clock)
    "feed"                    INTEGER,              -- hot::stamps::Feed (1 preprocessed, 2 udp, 3 in-process, 4 LaserStream backup)
    "mayhem"                  INTEGER,
    "cashback_event"          INTEGER,              -- from the CreateEvent (would_pass uses it)
    "cashback_ix"             INTEGER,              -- from the create instruction tail, as the hot path would read it (parity check)
    "token_2022"              INTEGER,
    "sol_quote"               INTEGER,
    "count_decision"          INTEGER,              -- bundle10 count at the decision (early feed, arrival order)
    "count_exact"             INTEGER,              -- bundle10 count by block index (regular stream, trade events)
    "count_hot"               INTEGER,              -- the existing hot path count (decide::bundle; one buy a transaction; NULL: unreadable buyer)
    "count_slow"              INTEGER,              -- the slow path count (shred_feed::bundle_wallets; NULL: unreadable buyer or not seen)
    "txs_decision"            INTEGER,
    "txs_exact"               INTEGER,
    "stop_decision"           TEXT,                 -- open / not_a_buy / gap / four
    "stop_exact"              TEXT,
    "unreadable_decision"     INTEGER,
    "trades_decision"         INTEGER,
    "mixed_decision"          INTEGER,
    "mixed_exact"             INTEGER,
    "too_many_buys"           INTEGER,              -- more than 128 trades in the counted transactions (TooManyBuys refusal)
    "dev_buy_sol"             REAL,
    "bundle_sol"              REAL,                 -- bundle SOL at the decision (the quote)
    "bundle_sol_exact"        REAL,
    "bundle_sol_hot"          REAL,                 -- bundle SOL the existing hot path saw
    "largest_buy_sol"         REAL,
    "largest_buy_sol_exact"   REAL,
    "post_mult"               REAL,                 -- post-bundle price / launch price at the decision
    "post_mult_exact"         REAL,
    "would_pass"              INTEGER,              -- the rule on the decision view, cashback from the CreateEvent
    "miss"                    TEXT,                 -- first part of the rule missed ("" when it passes)
    "would_pass_exact"        INTEGER,
    "miss_exact"              TEXT,
    "blocked_by_15sol_cap"    INTEGER,              -- bundle SOL over the 15 SOL cap other paths keep
    "blocked_by_fee_route"    INTEGER,              -- the fee engine route skips the slot leader class (Harmonic rule)
    "blocked_by_ceiling"      INTEGER,              -- post_mult over BUNDLE10_MAX_POST_BUNDLE_MULT
    "far_producer"            INTEGER,              -- LEADER_REGIONS would skip the slot
    "distance_go"             INTEGER,              -- the distance rule says go
    "existing_reject"         INTEGER,              -- the existing decision reject code (0: none)
    "existing_bought"         INTEGER,
    "window_us"               INTEGER,              -- create seen to window end
    "compute_us"              INTEGER,              -- bundle10 view computed after the window end
    "decision_latency_us"     INTEGER,              -- window_us + compute_us
    "create_seen_exact"       INTEGER,
    "phantom_flags"           TEXT,                 -- per counted transaction: 0 executed, 1 not executed (phantom), 2 unknown
    "phantoms"                INTEGER,              -- counted transactions that did not execute
    "bundle_last_index"       INTEGER,
    "next_index_1"            INTEGER,              -- block index of the next transaction trading the coin after the bundle
    "next_index_2"            INTEGER,
    "next_index_3"            INTEGER,
    "open_at_decision"        INTEGER,              -- simulated would-pass positions open at this decision (no limit)
    "taken_l1"                INTEGER,              -- taken under a limit of 1 open position (l3, l5 alike)
    "taken_l3"                INTEGER,
    "taken_l5"                INTEGER,
    "day_running_sol"         REAL,                 -- realized simulated P&L today before this decision (limit 5, 60 s exit, real costs)
    "day_stopped"             INTEGER,              -- day_running_sol at or under -BUNDLE10_DAY_LOSS_SOL
    "e2_from"                 INTEGER,              -- 0 post-bundle state, 1 or 2 the next trades of the create slot
    "e2_mult"                 REAL,
    "fill_mult"               REAL,
    "tokens"                  INTEGER,
    "exit60"                  TEXT,                 -- trail / deadline / graduated / end_of_data
    "exit60_unix_ms"          INTEGER,
    "sell60_mult"             REAL,
    "peak60_mult"             REAL,
    "pnl60_real_sol"          REAL,                 -- 0.1 SOL trade, trail 15% / 60 s, real fees, fee engine bid, 0slot tip, modeled sell cost
    "pnl60_research_sol"      REAL,                 -- the same with the create fee bps both ways and 0.0011 SOL a transaction
    "exit300"                 TEXT,
    "exit300_unix_ms"         INTEGER,
    "sell300_mult"            REAL,
    "pnl300_real_sol"         REAL,
    "pnl300_research_sol"     REAL,
    "graduated"               INTEGER,              -- the curve completed during either hold (valued at its last state before)
    "pool_first_price"        REAL,                 -- PumpSwap first price after migration (NULL: PumpSwap not on the stream)
    "buy_cost_sol"            REAL,
    "sell_cost_sol"           REAL,
    "create_fee_bps"          INTEGER,
    "ring_drops"              INTEGER               -- shadow records dropped so far by full rings (cumulative)
);
CREATE INDEX bundle10_shadow_time ON bundle10_shadow (recv_unix_ms);
CREATE INDEX bundle10_shadow_mint ON bundle10_shadow (mint);
