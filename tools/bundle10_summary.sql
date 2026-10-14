-- bundle10 shadow: the daily summary (the store's bundle10_shadow, migration 0004).
--   python3 tools/bundle10_summary.py [data/bigboss.sqlite]   (read-only; no sqlite3 CLI on this box)
--   or: sqlite3 -readonly data/bigboss.sqlite < tools/bundle10_summary.sql
-- Days are UTC, by the create's arrival on the hot path (recv_unix_ms). "Matches" are rows that
-- would pass the rule at the decision (would_pass); results are the simulated 0.1 SOL trade with
-- the 60 s exit (bundle10's), real costs unless named research.
.headers on
.mode column

SELECT '1. per day' AS "";
SELECT date(recv_unix_ms / 1000, 'unixepoch')                                         AS day,
       COUNT(*)                                                                         AS rows,
       SUM(would_pass)                                                                  AS matches,
       ROUND(1000 * AVG(CASE WHEN would_pass = 1 THEN pnl60_real_sol END), 2)           AS mean_msol,
       ROUND(SUM(CASE WHEN would_pass = 1 THEN pnl60_real_sol END), 4)                  AS total_sol,
       SUM(would_pass = 1 AND pnl60_real_sol > 0)                                       AS wins,
       ROUND(100.0 * SUM(would_pass = 1 AND pnl60_real_sol > 0) / NULLIF(SUM(would_pass = 1 AND pnl60_real_sol IS NOT NULL), 0), 1) AS win_pct,
       ROUND(1000 * AVG(CASE WHEN would_pass = 1 THEN pnl60_research_sol END), 2)       AS mean_msol_research,
       ROUND(SUM(CASE WHEN would_pass = 1 THEN pnl60_research_sol END), 4)              AS total_sol_research,
       ROUND(SUM(CASE WHEN would_pass = 1 THEN pnl300_real_sol END), 4)                 AS total_sol_300s,
       ROUND(100.0 * SUM(would_pass = 1 AND blocked_by_15sol_cap = 1) / NULLIF(SUM(would_pass), 0), 1) AS pct_blocked_15sol
FROM bundle10_shadow
GROUP BY day
ORDER BY day;

SELECT '2. the decision-time count against the exact count (count >= 10)' AS "";
SELECT date(recv_unix_ms / 1000, 'unixepoch')                                         AS day,
       SUM(count_decision >= 10 AND count_exact >= 10)                                  AS both,
       SUM(count_decision >= 10 AND count_exact < 10)                                   AS decision_only,
       SUM(count_decision < 10 AND count_exact >= 10)                                   AS exact_only,
       ROUND(100.0 * SUM(count_decision >= 10 AND count_exact >= 10) / NULLIF(SUM(count_decision >= 10 OR count_exact >= 10), 0), 1) AS agree_pct,
       SUM(count_exact IS NULL)                                                         AS create_not_on_stream,
       SUM(would_pass = 1 AND would_pass_exact = 0)                                     AS pass_decision_only,
       SUM(would_pass = 0 AND would_pass_exact = 1)                                     AS pass_exact_only
FROM bundle10_shadow
GROUP BY day
ORDER BY day;

SELECT '3. phantoms (counted at the decision, never executed), count_decision >= 10' AS "";
SELECT date(recv_unix_ms / 1000, 'unixepoch')                                         AS day,
       COUNT(*)                                                                         AS launches,
       SUM(txs_decision)                                                                AS counted_txs,
       SUM(phantoms)                                                                    AS phantom_txs,
       ROUND(100.0 * SUM(phantoms) / NULLIF(SUM(txs_decision), 0), 2)                   AS phantom_tx_pct,
       SUM(phantoms > 0)                                                                AS launches_with_a_phantom,
       SUM(phantoms > 0 AND would_pass = 1)                                             AS matches_with_a_phantom
FROM bundle10_shadow
WHERE count_decision >= 10 AND create_seen_exact = 1
GROUP BY day
ORDER BY day;

SELECT '4. what else would block a match, and the limits' AS "";
SELECT date(recv_unix_ms / 1000, 'unixepoch')                                         AS day,
       SUM(would_pass)                                                                  AS matches,
       SUM(would_pass = 1 AND blocked_by_fee_route = 1)                                 AS fee_route,
       SUM(would_pass = 1 AND far_producer = 1)                                         AS leader_regions,
       SUM(would_pass = 1 AND distance_go = 0)                                          AS distance,
       SUM(blocked_by_ceiling = 1 AND miss = 'ceiling')                                 AS ceiling_6x,
       SUM(too_many_buys)                                                               AS too_many_buys,
       SUM(mixed_decision)                                                              AS mixed_tx,
       ROUND(SUM(CASE WHEN taken_l1 = 1 THEN pnl60_real_sol END), 4)                    AS total_limit1,
       ROUND(SUM(CASE WHEN taken_l3 = 1 THEN pnl60_real_sol END), 4)                    AS total_limit3,
       ROUND(SUM(CASE WHEN taken_l5 = 1 THEN pnl60_real_sol END), 4)                    AS total_limit5,
       ROUND(SUM(CASE WHEN taken_l5 = 1 AND day_stopped = 0 THEN pnl60_real_sol END), 4) AS total_limit5_day_stop,
       MAX(ring_drops)                                                                  AS ring_drops
FROM bundle10_shadow
GROUP BY day
ORDER BY day;

SELECT '5. the existing hot path count against the exact count (exact >= 10)' AS "";
SELECT date(recv_unix_ms / 1000, 'unixepoch')                                         AS day,
       COUNT(*)                                                                         AS launches,
       ROUND(AVG(count_exact), 1)                                                       AS mean_exact,
       ROUND(AVG(count_hot), 1)                                                         AS mean_hot,
       ROUND(AVG(count_slow), 1)                                                        AS mean_slow,
       SUM(count_hot IS NOT NULL AND count_hot < count_exact)                           AS hot_under,
       SUM(count_slow IS NOT NULL AND count_slow < count_exact)                         AS slow_under,
       SUM(cashback_ix IS NOT NULL AND cashback_ix != cashback_event)                   AS cashback_decode_mismatch
FROM bundle10_shadow
WHERE count_exact >= 10
GROUP BY day
ORDER BY day;
