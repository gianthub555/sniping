-- bigboss's store, 2026-10-12 (Part A). One file, data/bigboss.sqlite: the bot is the only writer
-- (one thread, batched transactions, WAL); the dashboard and the scripts open it read-only.
-- Times: *_ns are unix nanoseconds, *_unix unix seconds. SOL amounts in lamports unless a column
-- says _sol. Every trading row carries a venue: 'pumpfun' (a bonding-curve launch) or 'pumpswap'
-- (a PumpSwap pool, Part C).

-- One row per bot start.
CREATE TABLE runs (
    run_id       TEXT PRIMARY KEY,
    started_ns   INTEGER NOT NULL,
    stopped_ns   INTEGER,                -- NULL: still running, or the process died
    mode         TEXT NOT NULL,          -- 'paper' | 'live'
    exe          TEXT NOT NULL,
    clean        INTEGER NOT NULL DEFAULT 0,
    settings     TEXT                    -- the switches in force (JSON, no secrets)
);

-- Positions: the copy bot's columns (its migrations 0004, 0007, 0008, 0009) so the dashboard's
-- queries carry over, plus the venue and bigboss's extras. One row per entry.
CREATE TABLE positions (
    id                    INTEGER PRIMARY KEY,
    run_id                TEXT NOT NULL,
    mint                  TEXT NOT NULL,
    mode                  TEXT NOT NULL,      -- 'paper' | 'live'
    state                 TEXT NOT NULL,      -- 'BUY_PENDING' | 'OPEN' | 'CLOSED' | 'BUY_FAILED' | 'STUCK'
    opened_ns             INTEGER NOT NULL,   -- when the buy was decided
    closed_ns             INTEGER,
    tokens_held           INTEGER NOT NULL DEFAULT 0,
    fill_quote_net        INTEGER NOT NULL DEFAULT 0,
    fill_tokens           INTEGER NOT NULL DEFAULT 0,
    sol_in                INTEGER NOT NULL DEFAULT 0,   -- lamports into the buy
    sol_out               INTEGER NOT NULL DEFAULT 0,   -- lamports out of the sells
    wallet_delta_lamports INTEGER NOT NULL DEFAULT 0,   -- the wallet's change over every landed tx of it
    peak_mult             REAL,
    peak_pnl              REAL,
    tp1_done              INTEGER NOT NULL DEFAULT 0,
    tp2_done              INTEGER NOT NULL DEFAULT 0,
    exit_reason           TEXT,               -- the last sell's reason
    bought_ns             INTEGER,            -- when the buy landed (paper: filled)
    venue                 TEXT NOT NULL DEFAULT 'pumpfun',
    dex                   TEXT,               -- 'pumpfun' | 'pumpswap'
    deployer              TEXT,
    follow_kind           TEXT,               -- 'recent' | 'profitable'
    tier                  TEXT,               -- the fee engine's tier for the launch
    rent_locked_lamports  INTEGER NOT NULL DEFAULT 0  -- token account rent (accounts are never closed)
);
CREATE INDEX positions_run ON positions (run_id, mode, mint);
CREATE INDEX positions_opened ON positions (run_id, opened_ns DESC, id);
CREATE INDEX positions_bought ON positions (run_id, bought_ns DESC, id);
CREATE INDEX positions_mint ON positions (mint, venue);

-- Orders: the copy bot's columns plus the venue and bigboss's extras. One row per buy or sell.
CREATE TABLE orders (
    client_id             INTEGER PRIMARY KEY,
    position_id           INTEGER NOT NULL,
    mint                  TEXT NOT NULL,
    side                  TEXT NOT NULL,      -- 'buy' | 'sell'
    mode                  TEXT NOT NULL,
    reason                TEXT NOT NULL,      -- 'ENTRY' or the exit's reason code
    requested             INTEGER NOT NULL,   -- lamports to spend (buy) or tokens to sell
    status                TEXT NOT NULL,      -- 'landed' | 'failed' | 'timed_out' | 'pending'
    sig                   TEXT,
    t_signal              INTEGER NOT NULL,
    t_ready               INTEGER,
    t_sent                INTEGER,
    t_landed              INTEGER,
    signal_slot           INTEGER,
    landed_slot           INTEGER,
    quote_net             INTEGER,            -- SOL in (buy) or out (sell), lamports
    tokens                INTEGER,
    protocol_fee          INTEGER,
    creator_fee           INTEGER,
    network_fee           INTEGER,
    cu_price_paid         INTEGER,
    tip                   INTEGER,
    rent_paid             INTEGER,
    rent_reclaimed        INTEGER,
    wallet_delta_lamports INTEGER,
    error                 TEXT,
    venue                 TEXT NOT NULL DEFAULT 'pumpfun',
    run_id                TEXT NOT NULL,
    decision_slot         INTEGER,
    fill_slot             INTEGER,
    attempts              INTEGER,
    min_out               INTEGER,
    fill_state            TEXT,
    lane                  TEXT,
    sell_step             INTEGER,            -- 1 for the first sell of a position, 2 for the next...
    detect_to_ready_ms    REAL,
    launch_to_landed_ms   INTEGER,
    pnl_percent           REAL
);
CREATE INDEX orders_position ON orders (position_id);
CREATE INDEX orders_run ON orders (run_id, position_id);
CREATE INDEX orders_mint ON orders (mint, t_signal);

-- Every launch and every graduation the bot saw, with what it decided.
CREATE TABLE launches (
    mint              TEXT NOT NULL,
    venue             TEXT NOT NULL,          -- 'pumpfun' | 'pumpswap'
    kind              TEXT NOT NULL,          -- 'launch' | 'graduation' | 'pool'
    run_id            TEXT NOT NULL,
    seen_ns           INTEGER NOT NULL,
    slot              INTEGER,
    source            TEXT,                   -- 'early' | 'regular'
    deployer          TEXT,
    creator           TEXT,
    dev_buy_lamports  INTEGER,
    followed          INTEGER NOT NULL DEFAULT 0,
    follow_kind       TEXT,
    score             INTEGER,
    verdict           TEXT NOT NULL,          -- 'seen' | 'buy' | 'skip' | 'paused' | 'bought' | 'buy_failed'
    skip_reason       TEXT,
    updated_ns        INTEGER,
    PRIMARY KEY (mint, venue, kind)
);
CREATE INDEX launches_seen ON launches (seen_ns DESC);
CREATE INDEX launches_deployer ON launches (deployer, seen_ns);

-- The dashboard's live activity feed.
CREATE TABLE activity (
    id      INTEGER PRIMARY KEY,
    run_id  TEXT NOT NULL,
    at_ns   INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    venue   TEXT,
    dex     TEXT,
    mint    TEXT,
    message TEXT NOT NULL
);
CREATE INDEX activity_time ON activity (at_ns DESC);

-- Startup data, one table per file.
CREATE TABLE creator_history (            -- creator_history.csv
    unix_time        INTEGER NOT NULL,
    deployer         TEXT NOT NULL,
    mint             TEXT NOT NULL,
    dev_buy_lamports INTEGER NOT NULL
);
CREATE INDEX creator_history_time ON creator_history (unix_time);
CREATE INDEX creator_history_deployer ON creator_history (deployer, unix_time);
CREATE TABLE deployer_launches (          -- logs/deployer_launches.csv
    unix_time INTEGER NOT NULL,
    deployer  TEXT NOT NULL,
    mint      TEXT NOT NULL,
    no_pop    INTEGER NOT NULL
);
CREATE INDEX deployer_launches_time ON deployer_launches (unix_time);
CREATE TABLE reference_trades (           -- reference_deployers.txt
    unix_time      INTEGER NOT NULL,
    key            TEXT NOT NULL,
    lamports_delta INTEGER NOT NULL
);
CREATE INDEX reference_trades_time ON reference_trades (unix_time);
CREATE TABLE lookup_tables (              -- logs/lookup_tables.txt
    address  TEXT PRIMARY KEY,
    added_ns INTEGER NOT NULL
);
CREATE TABLE kv (                         -- logs/cu_calibration.json and other single values
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_ns INTEGER NOT NULL
);
CREATE TABLE validator_regions (          -- logs/validator_regions.json
    ip        TEXT PRIMARY KEY,
    continent TEXT,
    country   TEXT,
    city      TEXT
);
CREATE TABLE token_names (                -- token_names.json
    mint   TEXT PRIMARY KEY,
    symbol TEXT,
    name   TEXT
);

-- The scorecard's data (2026-10-12; moved from bigboss.db unchanged).
CREATE TABLE deployer_wallet (
    key         TEXT PRIMARY KEY,
    first_bt    INTEGER,
    funded_sol  REAL,
    funder      TEXT,
    checked_at  INTEGER NOT NULL
);
CREATE TABLE launch_result (
    mint        TEXT PRIMARY KEY,
    deployer    TEXT NOT NULL,
    creator     TEXT NOT NULL,
    created     INTEGER NOT NULL,
    pnl_sol     REAL NOT NULL,
    down        INTEGER NOT NULL,
    no_pop      INTEGER NOT NULL
);
CREATE INDEX launch_result_creator ON launch_result (creator, created);
CREATE TABLE score (
    mint        TEXT PRIMARY KEY,
    t           INTEGER NOT NULL,
    deployer    TEXT NOT NULL,
    creator     TEXT NOT NULL,
    path        TEXT NOT NULL,
    score       INTEGER NOT NULL,
    threshold   INTEGER NOT NULL,
    passed      INTEGER NOT NULL,
    verdict     TEXT NOT NULL,
    facts       TEXT NOT NULL
);
CREATE INDEX score_time ON score (t);
CREATE TABLE exit_decision (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    mint        TEXT NOT NULL,
    t           INTEGER NOT NULL,
    paper       INTEGER NOT NULL,
    reason      TEXT NOT NULL,
    share       REAL NOT NULL,
    value_x     REAL NOT NULL,
    peak_x      REAL NOT NULL,
    held_secs   REAL NOT NULL,
    net_behind_sol REAL NOT NULL
);

-- Measurement logs: each table has its file's columns, in its file's order (row for row the same
-- as the file while both are written), plus id and rx_ns (when the row was queued). trade_log is
-- auto_sell_logs.csv and auto_sell_logs_paper.csv, told apart by mode.
CREATE TABLE trade_log (
    id                           INTEGER PRIMARY KEY,
    rx_ns                        INTEGER NOT NULL,
    "mode"                       TEXT NOT NULL,
    "timestamp"                  TEXT,
    "action"                     TEXT,
    "dex"                        TEXT,
    "mint"                       TEXT,
    "signature"                  TEXT,
    "status"                     TEXT,
    "sol_in"                     NUMERIC,
    "sol_out"                    NUMERIC,
    "token_amount"               NUMERIC,
    "pnl_percent"                NUMERIC,
    "reason"                     TEXT,
    "wallet_sol_change"          NUMERIC,
    "launch_slot"                NUMERIC,
    "ready_slot"                 NUMERIC,
    "landed_slot"                NUMERIC,
    "detect_to_ready_ms"         NUMERIC,
    "launch_to_landed_ms"        NUMERIC
);
CREATE TABLE positions_log (
    id                           INTEGER PRIMARY KEY,
    rx_ns                        INTEGER NOT NULL,
    "date"                       TEXT,
    "kind"                       TEXT,
    "mint"                       TEXT,
    "create_slot"                NUMERIC,
    "create_index"               NUMERIC,
    "bundle_buys"                NUMERIC,
    "leader"                     TEXT,
    "leader_version"             TEXT,
    "leader_client"              TEXT,
    "leader_city"                TEXT,
    "our_status"                 TEXT,
    "our_slot"                   NUMERIC,
    "our_index"                  NUMERIC,
    "others_between"             NUMERIC,
    "first_position"             NUMERIC,
    "our_leader"                 TEXT,
    "our_leader_version"         TEXT,
    "our_leader_client"          TEXT,
    "our_leader_city"            TEXT,
    "rival_slot_offset"          NUMERIC,
    "rival_gap"                  NUMERIC,
    "rival_cu_price"             NUMERIC,
    "rival_tip"                  NUMERIC,
    "rival_sol"                  NUMERIC,
    "rival_leader_client"        TEXT,
    "rivals_in_create_slot"      NUMERIC,
    "our_cu_price"               NUMERIC,
    "our_fee_sol"                NUMERIC,
    "raised"                     NUMERIC,
    "rival_bid_seen"             NUMERIC,
    "lanes"                      TEXT,
    "landed_lane"                TEXT,
    "our_cu_limit"               NUMERIC,
    "leader_class"               TEXT,
    "our_leader_class"           TEXT,
    "our_rank"                   NUMERIC,
    "our_ms"                     NUMERIC,
    "nb1_slot_offset"            NUMERIC,
    "nb1_after_create"           NUMERIC,
    "nb1_cu_price"               NUMERIC,
    "nb1_cu_limit"               NUMERIC,
    "nb1_tip"                    NUMERIC,
    "nb1_ms"                     NUMERIC,
    "nb2_slot_offset"            NUMERIC,
    "nb2_after_create"           NUMERIC,
    "nb2_cu_price"               NUMERIC,
    "nb2_cu_limit"               NUMERIC,
    "nb2_tip"                    NUMERIC,
    "nb2_ms"                     NUMERIC,
    "tier"                       TEXT,
    "fee_arm"                    TEXT,
    "fee_arm_label"              TEXT,
    "landed_via"                 TEXT,
    "our_sent_ms"                NUMERIC,
    "next_slot_ms"               NUMERIC,
    "leader_rtt_ms"              NUMERIC,
    "would_rank_time"            NUMERIC,
    "rivals_outbidding"          NUMERIC
);
CREATE TABLE rival_buys (
    id                           INTEGER PRIMARY KEY,
    rx_ns                        INTEGER NOT NULL,
    "date"                       TEXT,
    "kind"                       TEXT,
    "mint"                       TEXT,
    "create_slot"                NUMERIC,
    "create_index"               NUMERIC,
    "slot"                       NUMERIC,
    "index"                      NUMERIC,
    "slot_delta"                 NUMERIC,
    "after_create"               NUMERIC,
    "bundled"                    TEXT,
    "rank"                       NUMERIC,
    "ours"                       TEXT,
    "status"                     TEXT,
    "user"                       TEXT,
    "sol"                        NUMERIC,
    "cu_price"                   NUMERIC,
    "cu_limit"                   NUMERIC,
    "priority_lamports"          NUMERIC,
    "tip_lane"                   TEXT,
    "tip_lamports"               NUMERIC,
    "ms_after_create"            NUMERIC,
    "entry_offset"               NUMERIC,
    "leader"                     TEXT,
    "leader_client"              TEXT,
    "leader_class"               TEXT,
    "leader_city"                TEXT,
    "signature"                  TEXT
);
CREATE TABLE route_skips (
    id                           INTEGER PRIMARY KEY,
    rx_ns                        INTEGER NOT NULL,
    "time"                       TEXT,
    "mint"                       TEXT,
    "followed"                   TEXT,
    "slot"                       NUMERIC,
    "reason"                     TEXT,
    "leader"                     TEXT,
    "client"                     TEXT,
    "class"                      TEXT,
    "city"                       TEXT,
    "rtt_ms"                     NUMERIC,
    "next_rtt_ms"                NUMERIC,
    "offset_ms"                  NUMERIC
);
CREATE TABLE hot_stamps (
    id                           INTEGER PRIMARY KEY,
    rx_ns                        INTEGER NOT NULL,
    "mint"                       TEXT,
    "feed"                       TEXT,
    "mode"                       TEXT,
    "flags"                      TEXT,
    "reject"                     NUMERIC,
    "window_ns"                  NUMERIC,
    "t_recv"                     NUMERIC,
    "t_matched"                  NUMERIC,
    "t_picked"                   NUMERIC,
    "t_derived"                  NUMERIC,
    "t_decide_start"             NUMERIC,
    "t_decided"                  NUMERIC,
    "t_signed"                   NUMERIC,
    "t_ready"                    NUMERIC,
    "t_sent_helius"              NUMERIC,
    "t_sent_everstake"           NUMERIC,
    "t_sent_allenhark"           NUMERIC,
    "t_sent_triton"              NUMERIC,
    "t_sent_0slot"               NUMERIC,
    "t_sent_nozomi"              NUMERIC,
    "t_sent_jito"                NUMERIC,
    "t_sent_quic"                NUMERIC,
    "t_shred_recv"               NUMERIC,
    "t_deshred"                  NUMERIC
);
CREATE TABLE lane_answers (
    id                           INTEGER PRIMARY KEY,
    rx_ns                        INTEGER NOT NULL,
    "lane"                       TEXT,
    "kind"                       TEXT,
    "conn"                       TEXT,
    "t_sent"                     NUMERIC,
    "t_answer"                   NUMERIC,
    "answer_us"                  NUMERIC,
    "status"                     TEXT
);
CREATE TABLE hot_verify (
    id                           INTEGER PRIMARY KEY,
    rx_ns                        INTEGER NOT NULL,
    "mint"                       TEXT,
    "verdict_hot"                TEXT,
    "verdict_slow"               TEXT,
    "result"                     TEXT,
    "fields"                     TEXT,
    "late_buys"                  NUMERIC,
    "late_sol"                   NUMERIC,
    "bundle_wallets_hot"         NUMERIC,
    "bundle_wallets_slow"        NUMERIC,
    "insider_sol_hot"            NUMERIC,
    "insider_sol_slow"           NUMERIC
);
CREATE TABLE engine_parity (
    id                           INTEGER PRIMARY KEY,
    rx_ns                        INTEGER NOT NULL,
    "mint"                       TEXT,
    "slot"                       NUMERIC,
    "seen_by"                    TEXT,
    "pp_matched"                 NUMERIC,
    "udp_recv"                   NUMERIC,
    "udp_matched"                NUMERIC,
    "inproc_recv"                NUMERIC,
    "inproc_matched"             NUMERIC,
    "inproc_lead_us"             NUMERIC,
    "udp_later"                  NUMERIC,
    "inproc_later"               NUMERIC,
    "udp_later_sol"              NUMERIC,
    "inproc_later_sol"           NUMERIC,
    "udp_rival"                  NUMERIC,
    "inproc_rival"               NUMERIC,
    "same_view"                  TEXT,
    "pp_recv"                    NUMERIC,
    "leader"                     TEXT,
    "leader_client"              TEXT,
    "leader_class"               TEXT
);
CREATE TABLE shred_lead (
    id                           INTEGER PRIMARY KEY,
    rx_ns                        INTEGER NOT NULL,
    "regular_seen_unix_ms"       NUMERIC,
    "mint"                       TEXT,
    "shred_seen_unix_ms"         NUMERIC,
    "shred_slot"                 NUMERIC,
    "regular_slot"               NUMERIC,
    "lead_ms"                    NUMERIC,
    "first_seen_by"              TEXT
);
CREATE TABLE leader_rtt (
    id                           INTEGER PRIMARY KEY,
    rx_ns                        INTEGER NOT NULL,
    "leader"                     TEXT,
    "slots_this_epoch"           NUMERIC,
    "slots_next_epoch"           NUMERIC,
    "client"                     TEXT,
    "class"                      TEXT,
    "city"                       TEXT,
    "country"                    TEXT,
    "rtt_ms"                     NUMERIC
);
CREATE TABLE preexec_sightings (
    id                           INTEGER PRIMARY KEY,
    rx_ns                        INTEGER NOT NULL,
    "seen_unix_ms"               NUMERIC,
    "signature"                  TEXT,
    "mint"                       TEXT,
    "seller"                     TEXT,
    "slot"                       NUMERIC,
    "executed"                   TEXT,
    "ms_to_executed"             NUMERIC
);
CREATE TABLE distance_daily (
    id                           INTEGER PRIMARY KEY,
    rx_ns                        INTEGER NOT NULL,
    "date"                       TEXT,
    "followed"                   TEXT,
    "passed_rules"               NUMERIC,
    "skipped_for_distance"       NUMERIC
);
CREATE INDEX trade_log_mode ON trade_log (mode, timestamp);
CREATE INDEX trade_log_mint ON trade_log (mint);
CREATE INDEX positions_log_mint ON positions_log (mint);
CREATE INDEX rival_buys_mint ON rival_buys (mint);
CREATE INDEX route_skips_mint ON route_skips (mint);
CREATE INDEX hot_stamps_mint ON hot_stamps (mint);
