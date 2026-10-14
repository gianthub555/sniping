# A filter for the skipped coins, walk-forward (14528 skipped coins, 10-13 05:06 to 10-14 16:14 UTC)

## Entry 0 s after the create (facts at the snipe only)

| test fold (UTC) | filter chosen on the earlier folds | exit | train low end | test trades | test mSOL | gross (no fixed cost) |
|---|---|---|---|---|---|---|
| 10-14 02:40-06:20 | name_len < 6 AND copycats_1h < 1 AND dep_since_last_s >= 1.32e+03 | TP x2, stop x0.85, trail 35%, 30 s | -1.17 | 142 | -8.61 | -6.11 |
| 10-14 06:20-10:50 | dep_since_last_s >= 8.64e+04 AND launches_60s < 19 AND copycats_1h < 1 | TP x2, stop x0.85, trail 20%, 60 min | -0.48 | 403 | -3.46 | -0.96 |
| 10-14 10:50-13:55 | dep_prior < 1 AND launches_60s < 19 AND copycats_1h < 3 | TP x1.5, stop x0.5, trail 35%, 30 s | -1.96 | 116 | -4.03 | -1.53 |
| 10-14 13:55-16:14 | dep_prior < 1 AND launches_60s < 17 AND copycats_1h < 1 | TP x1.5, 30 s | -1.07 | 11 | +10.13 | +12.63 |

**Pooled test (never seen by the choice): 672 trades (49.6 an hour), -4.43 mSOL a trade [-6.13, -2.73], 15% positive; gross -1.93 [-3.63, -0.23].**

## Entry 30 s after the create (facts traded by then too)

| test fold (UTC) | filter chosen on the earlier folds | exit | train low end | test trades | test mSOL | gross (no fixed cost) |
|---|---|---|---|---|---|---|
| 10-14 02:40-06:20 | real_sol < 0.14 | TP x1.25, 10 min | -4.68 | 1080 | -4.71 | -2.21 |
| 10-14 06:20-10:49 | real_sol < 0.0495 AND net_in_sol < 1.82 | TP x1.25, 10 min | -4.64 | 790 | -4.53 | -2.03 |
| 10-14 10:49-13:55 | real_sol < 0.0506 | TP x1.25, 10 min | -4.63 | 900 | -4.21 | -1.71 |
| 10-14 13:55-16:14 | real_sol < 0.059 | TP x1.25, 10 min | -4.62 | 593 | -4.89 | -2.39 |

**Pooled test (never seen by the choice): 3363 trades (248.0 an hour), -4.57 mSOL a trade [-4.80, -4.33], 1% positive; gross -2.07 [-2.30, -1.83].**

## Entry 60 s after the create (facts traded by then too)

| test fold (UTC) | filter chosen on the earlier folds | exit | train low end | test trades | test mSOL | gross (no fixed cost) |
|---|---|---|---|---|---|---|
| 10-14 02:40-06:20 | real_sol < 0.05 | TP x1.25, 10 min | -4.76 | 892 | -3.45 | -0.95 |
| 10-14 06:20-10:49 | real_sol < 0.00988 | TP x1.25, 10 min | -4.70 | 772 | -4.61 | -2.11 |
| 10-14 10:49-13:54 | real_sol < 0.0164 AND net_in_sol < 1.68 | TP x1.25, 10 min | -4.72 | 922 | -4.28 | -1.78 |
| 10-14 13:54-16:14 | real_sol < 0.0165 | TP x1.25, 10 min | -4.69 | 588 | -4.93 | -2.43 |

**Pooled test (never seen by the choice): 3174 trades (234.0 an hour), -4.25 mSOL a trade [-4.86, -3.64], 1% positive; gross -1.75 [-2.36, -1.14].**

