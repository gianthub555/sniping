# Decisions

## 2026-10-07: hot path and send path rebuild (Parts A-D)

Approved by the owner on 2026-10-07:

1. **Buy template: 18 accounts, as the chain accepts them.** pump.fun's published IDL
   (pump-fun/pump-public-docs, `idl/pump.json`, last changed 2026-09-12) lists 16 accounts for
   `buy_exact_sol_in`; all 209 successful `buy_exact_sol_in` buys sampled on chain (30 Sep - 1 Oct)
   carry 18: the 16, then `bonding_curve_v2` (PDA `["bonding-curve-v2", mint]`) and a
   `buyback_fee_recipient`. Discriminators match the IDL and `sha256("global:<name>")[..8]`:
   `create` [24,30,200,40,5,28,7,119], `create_v2` [214,144,76,236,95,139,49,180],
   `buy_exact_sol_in` [56,252,116,8,158,223,205,95], `buy_exact_quote_in_v2` [194,171,28,70,104,77,91,47].
2. **Compute limits:** buys 120,000 (measured 83-115k), sells 80,000 (measured 51-68k).
3. **0slot route:** `de1.0slot.trade` (0.2 ms, same data centre); `de.0slot.trade` resolves to Cloudflare.
4. **Hot-path target:** 0.4 ms median measured excluding the 250 us bundle wait, which stays.
5. **Resend guard:** built and tested, off. Every buy that fails because the bonding curve did not
   exist yet is counted and reported as a share of all buys; the owner decides on enabling it from that.
6. **Paper run before live:** 4 hours on DRY_RUN, not 30 minutes. Paper mode is not evidence of landing quality.
7. **Fee cap:** tips plus priority fee at most 0.005 SOL per transaction.
8. **pumpfeed's build code is ported, not linked** (pumpfeed uses solana-sdk 2.2, the bot newer component crates).
9. **systemd:** the bot, pumpfeed and the shredstream proxy are enabled with `Restart=always`, the bot in DRY_RUN.

Also decided:

- **Baseline:** the four deleted tracked files were restored and the tree committed as tag
  `baseline-2026-10-07`; this work starts on top of it.
- **Credentials leave git:** `.env` and `keys/` are no longer tracked (they stay on disk).

## 2026-10-07: security before Part B

- **Wallet rotated.** The old wallet `Dibzn3v4A9amgPZUncGx3gAGwKpnrphPRcD7r9JDrVyV` was committed in git and
  is burned. Everything it controlled moved to `92t8LzsPNbzud7aT9GBtA5UHePwb9tYK8Wt5FcC5VsVd`
  (`keys/wallet-2026-10-07.json`, in `.env` and as pumpfeed's `keys/trader.json`): 0.2513 SOL, the
  rent of 7 empty token accounts and 4 nonce accounts (`src/bin/wallet.rs`; transactions
  2PPZ8ftHPoa53Dknkn4TTz9jyWywS6tfzGRAb2fjPAcH3ZntAR9MzuEkfELvaLwzbCTZGyJwUgsKxXhhsiaMqnA4 and
  35zAWtyWAh9Dzehg1miJLjvVg7jNS1dsxAUiJ3Y8bdnMsEBiBFerTxFooj6X9BCvLN7xzt74DXTtkwtcAmkCHzsc).
  The old wallet ends at 0 SOL with no accounts.
- **History purged.** `git filter-repo --invert-paths --path .env --path keys/`, force-pushed; a fresh
  mirror clone of GitHub shows no commit touching either. Every object of the old history was
  scanned first: no other file ever held a key.
- **pre-commit hook** (`githooks/`, `git config core.hooksPath githooks`): refuses `.env` files,
  `keys/`, PEM files and any staged keypair (JSON or base58, checked as a real ed25519 keypair, so
  transaction signatures pass).
- **API keys** (Helius, AllenHark, Nozomi, 0slot): rotated by the owner on their dashboards; the bot
  uses the new `.env` values from then on.

- **Strategy B** is not changed; its `fixtures/` are read only.

## 2026-10-07: Part A measured (DRY_RUN, real feeds, shadow builds of every create)

Frame in to signed, base64-encoded buy bytes, the 250 us bundle window excluded, 30 minutes each:

| setup | launches | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| SPSC, no warm-up (my compiles removed) | 409 | 66 us | 114 us | 2.9 ms | 5.9 ms |
| inline, no warm-up | 674 | 64 us | 93 us | 142 us | 622 us |
| **SPSC, warm-up 1 ms (kept)** | 704 | **52 us** | **80 us** | **137 us** | 418 us |
| inline, warm-up 1 ms | 756 | 55 us | 86 us | 151 us | 435 us |

- **A2:** the SPSC hand-off to a pinned busy-polling decision thread is kept, with the 1 ms idle
  warm-up on (`HOT_WARM_US=1000`, now the default).
- **Feeds:** Helius preprocessed saw 783 of 787 creates first, a median 2.6 ms ahead of pumpfeed's
  UDP push; the UDP feed is a backup.
- **Tails:** the millisecond tail in the first run came from other processes using the hot cores
  (cargo builds, and something else at 07:18-07:23). The bot keeps its own threads off them; other
  processes need a systemd slice (proposed with Part C).

## 2026-10-07: Part B built (send path), C4 (services and cores)

- **Lanes**, fired in this order with the same signed bytes: QUIC straight to the TPU of the current
  and next leader (an unstaked placeholder identity until the staked one is provided:
  `QUIC_IDENTITY_KEYPAIR`), 0slot `de1` and Nozomi `fra2` plain sends (raw HTTP written from the
  decision thread onto warm keep-alive sockets), Jito bundle-only last, on its own thread.
- **Fees:** 0.0007 SOL priority at the 120k CU limit; over the highest rival bid seen on the coin
  (in the create's bundle window, not the deployer's, creator's or fee payer's) by 10%
  (`HOT_OUTBID_BPS`); priority plus tips capped at 0.005 SOL. Tips: 0slot 0.001, Nozomi 0.001,
  Jito 0.0001 SOL. Sells: 80k CU. A lane without its endpoint is off and its tip is not paid.
- **Who buys when live:** the hot path records its decision on every launch; the slow path buys only
  launches the hot path left to it (it needed an RPC answer, or could not read the create). No
  launch can be bought twice.
- **Live sends need two switches:** `DRY_RUN=false` and `HOT_LIVE=true`.
- **Resend guard** built and tested, off (`HOT_RESEND_BEFORE_CREATE`): it resends only a buy that
  failed on chain before the create (Custom 3012), once. Those failures are counted as a share of
  all live buys either way.
- **DRY_RUN lane timing:** `HOT_DRY_LANES=true` sends unlandable copies (all-zero blockhash) of paper
  buys and of one other launch every `HOT_DRY_SAMPLE_MS` through the real lanes. They cannot land
  and cost nothing.
- **C4:** `deploy/install.sh` installs `bigboss.service` (Restart=always) in `hot.slice`, the only
  slice allowed on cores 5-7; system.slice, user.slice and init.scope are held to 0-4. pumpfeed,
  the shredstream proxy and the bot are enabled. `bot.sh` start/stop/status go through systemd.

## 2026-10-07: pumpfeed's engine merged into the bot (ENGINE_INPROC)

- **One process on the hot path.** pumpfeed's shred receive (the proxy's UDP output and packet
  capture of its input port), slot reassembly with Reed-Solomon recovery, the batch walk with its
  pump.fun filter, and the blockhash from shreds now run in the bot (`src/hot/shreds`). Launches and
  buys come out of the same handler the UDP path used (`handle_entry_batch`), so the decider gets
  the same events from the same bytes: no decision changes.
- **Capture kept:** in pumpfeed's stats the capture of the proxy's input came first on 71.3M of
  78.8M shreds; the proxy's output trails it by 0.08 ms median, 1.25 ms p90, 3.3 ms p99.
- **Not ported:** streaming decode of partial batches (pumpfeed measured launches 0.63 ms earlier
  with it). It would change which feed decides a launch and on what bundle view: the owner's call.
- **Parity, 1 hour (15:59-16:59 UTC), DRY_RUN, both paths on dedicated cores** (system and user
  slices held to 0-3 for the hour, the in-process thread alone on core 4):

  | | pumpfeed UDP | in-process |
  |---|---|---|
  | creates decoded (feed counters) | 2,738 | 2,738 |
  | seen by one path only | 0 | 0 |
  | delay behind the preprocessed stream p10 / p50 / p90 / p99 | 0.906 / 2.166 / 5.881 / 23.925 ms | 0.745 / 2.038 / 5.674 / 23.741 ms |
  | first, create by create | 13.7% | 86.3% (lead p50 0.088 ms, p90 0.315 ms) |
  | same buys after the create, SOL and rival bid | 2,652 of 2,652 | |
  | blockhash, slot by slot | 17,900 agree, 0 differ (21 slots from before the bot's start) | |

  pumpfeed was first by more than 0.2 ms on 34 creates (1.3%) and by 4.3 ms once. The first
  13 minutes, with the in-process thread sharing core 4, were even at p90 (6.052 against 6.047 ms):
  the thread needs its own core.
- **Switched:** `ENGINE_INPROC=true` is the default; pumpfeed.service is stopped and disabled (its
  files and keys kept); the proxy sends to the bot only (127.0.0.1:20003); the bot's unit no longer
  waits for pumpfeed; `ENGINE_INPROC=false` (pumpfeed's UDP push) stays compiled in for this
  release only.
- **Allocations:** none from shred to signed buy without packet loss; one per Reed-Solomon recovery
  with the codec cached (17 the first time a set shape is seen; mainnet's 32:32 is built at start).

## 2026-10-07: EU-only producer filter removed (owner's decision)

- `LEADER_REGIONS=EU` is cleared: launches are no longer skipped for a block producer outside
  Europe, in both the hot and the slow path. In effect from 19:05 UTC, during the paper run.
- Numbers it was decided on: the filter skipped 13 of the 36 followed launches of 2026-10-07 (36%).
  It came in on 27 Sep, after the night of 25-26 Sep that made +18.3 SOL in paper (186 paper
  buys at 1 SOL stakes, 185 SOL staked, the old time-based paper model that filled far more races
  than live did; live buys that night: -0.03 SOL). At today's 0.1 SOL stake that night is about
  +1.8 SOL. From Frankfurt the bot was first on 36% of Europe-produced launches against 11% (US)
  and 20% (Asia) in the 25-26 Sep landings: outside Europe more races are lost, and live each lost
  race pays its fee.

## 2026-10-07: strategy change, paper first (owner's yes)

- **Entry:** dev buy 0.7-2 SOL (was 0.7-20); at most 1 wallet in the create's bundle besides the dev
  (was 2). Exit and every other rule unchanged. **Paper stake** 0.5 SOL (was 0.1). In paper from
  the 22:10 UTC restart; live only after October's tapes confirm it and the owner says yes.
- **Why, in numbers** (research/strategy-2026-10-07; September launches replayed with the bot's
  rules, its real costs and the positions its live buys landed at on 26 Sep; thresholds looked at
  on 8-22 Sep, checked on 25-27 Sep): per attempt at a 0.5 SOL stake, the new lines +2.0% / +4.4%
  (90% interval on 25-27 Sep: +3.2% to +5.5%) against +0.7% / +2.2% for the old ones; +6.5% /
  +10.3% if buys land 100 positions after the create; +1.3% / +2.9% even if every buy lands in the
  next slot. About one followed launch in five passes.
- **Also found:** buying every launch loses at any position; buying after early demand shows
  (delayed entry) lost 4-15% a try on the test days; the 25-26 Sep slicing exit is no better than
  today's patient exit once each sell's tip is counted; at a 0.1 SOL stake the tips (0.0031 SOL a
  buy with every lane) and sell tips (0.0011) take about 5% of each trade.
- **The bundle rule's earlier replay (25-29 Sep) found 3+ wallet bundles profitable**; it counted
  no tips or sell costs. With them, at real landing positions, they lose.

## 2026-10-07: the 28 Sep rules restored, paper only (owner's instruction)

- **Why:** the strategy above left 2-3 tries an hour (one paper fill in its first hour); the owner
  asked for the bot as it was on 28 Sep (commit d9cb8ee), which bought many coins an hour.
- **Rules, back to their 28 Sep values:** dev buy 0.7-20 SOL (was 0.7-2); no bundle-wallet limit
  (was 1); deployers the reference wallet lost on bought again; dump-risk skip off; relaunch skip
  5 min (was 10); buy price guard 1.25% (was 8%); the slicing exit instead of the patient exit
  (30% first slice, check at 2 s with all out under +5%, 33% steps, sell all early on a 6 SOL
  gain in the first 4 slots (was 4), no "first sell only when up", no NO_POP hold after +3%, out
  by 40 s); launches seen only on the regular stream are bought again. Rules that were the same
  on 28 Sep stay: followed deployers only, deployer 2+ SOL and 24+ h old, no mayhem, the no-buyers
  record, at most 1 open position. Paper stake 0.5 SOL (owner's choice). The paper fill model is
  the 28 Sep one (the 47 real landing positions).
- **Numbers shown to the owner first** (research/strategy-2026-10-07 data, 0.5 SOL, real costs, the
  47 landing positions): the 28 Sep rules made +26.8 SOL on 2,033 tries over 8-22 Sep (+2.6% a
  try, 47% filled) and +37.1 SOL on 3,060 over 25-27 Sep (+2.4%, 35% filled), against +7.1 SOL
  (723 tries) and +11.6 SOL (533) for the strategy above: less or about the same per try, 3-6
  times as many tries. The replay leaves out the deployer-balance, wallet-age and relaunch rules
  and the one-position limit, so real tries are fewer. Live on 26-29 Sep the 1.25% guard filled 33
  of 259 buys (13%), where paper and the replay fill about 35%: paper will look better than live.
- **Follow list rebuilt:** the 8 Sep-27 Sep 04:02 history from the research copy
  (`~/bigboss-research/bf/reference_deployers.txt`), the 27 Sep 04:02-2 Oct 13:11 gap from Helius
  (4,737 of the reference wallet's trades on 1,167 coins, read the way `utils::reference::backfill` reads them; on an
  overlapping 6 h window it gave the same keys and per-key totals as the bot's own record) and
  everything since. Followed deployers 578 -> see the restart's summary line.

## 2026-10-08: small live test (owner's instruction)

- **Asked:** go live small: 0.0001 SOL a launch, priority fee at the default, lane tips at their
  floors, lanes 0slot, Everstake and Helius Sender only (Nozomi, Jito, AllenHark off); stop after
  10 buys or under 0.01 SOL in the wallet; kill switch on a feed stall or 3 failed buys in a row;
  sells by the current rules (the 28 Sep ones), closing the token account; a per-buy report.
- **Built** (commit 6f6690c): `utils::live_guard` (the limits; a stop the dashboard cannot undo;
  a feed counts as stalled after 5 s silent), `LANE_ROTATE` (one lane per buy in turn, only its
  tip paid, so the landing lane is known: every lane carrying the same bytes cannot say which one
  landed it, except Jito), `LIVE_SLOW_PATH_BUYS=false` (only the hot path buys, so every buy goes
  through those three lanes and is counted). A sell of everything already closed the token account
  (`close_token_account` in every pump.fun sell; only partial sells drop it).
- **Choices made for the owner** (the owner said "go" with the questions unanswered; each can change):
  one lane per buy (0slot, Everstake, Helius in turn) rather than all three at once; sells through
  Helius Sender + RPC (Nozomi off: tip 0.000005 SOL instead of 0.001); about an hour of DRY_RUN with
  the same settings first, the owner's rule for buy/sell/send changes asking a few hours; "10 buys" = 10
  buys that got tokens; a failed buy = any that got none (lost the race, failed, never landed).
- **Paper rehearsal from 00:07 UTC:** each lane accepted its rotated copies with only its own tip
  (0slot refuses a transaction without its tip, so its acceptance proves the tip is there).

## 2026-10-08: fee engine, Part A (inputs: compute limit, position log, bid capture)

- **Reference report:** the build prompt names `docs/Automatic priority fee for sniping.md` (copy
  the report there first). The report was not on this server, in the owner's artifacts or docs, and
  was not pasted; the owner said "go", so Parts A-C follow the prompt alone (the prompt wins where
  they differ). Nothing in `docs/` claims to be that report.
- **A1, compute limit per buy path.** The bot has one buy instruction (`buy_exact_sol_in`) and one
  template; the paths are read as the two kinds of coin, SPL Token (`create`) and Token-2022
  (`create_v2`). `hot::cu` simulates the hot path's own buy against launches the stream just showed
  and sets the limit to the largest usage × 1.10 (up to 100 CU), per path, in
  `logs/cu_calibration.json`; it re-simulates on every pump.fun upgrade (program-data deploy slot)
  and six-hourly, alerting (log and dashboard) when usage moves over 15%. First run, Token-2022: 8
  simulations, p50 102,226 CU, max 102,437 → limit **120,000 → 112,700 CU**; at the same 0.0007 SOL
  the bid goes 5.83 → 6.21 lamports/CU (+6.5%). No SPL Token launch in the sample (300 of 300
  launches on 8 Oct were Token-2022): that path keeps 120,000 until one is measured. No alert channel
  exists beyond the log and the dashboard (ALERT_TELEGRAM_* are unused).
- **A2, position log.** `logs/positions.csv` gains our compute limit, the leaders' client class,
  our order among the buyers outside the bundle, ms after the create, and the first two such
  buyers (position, fee per CU, compute limit, tip, ms); the old file moved to
  `positions.csv.2026-10-08.old`. New `logs/rival_buys.csv`: one row per buy on every tracked launch
  (live, paper, samples and now every followed launch), ours included, a failed live buy of ours
  with its block position from the chain. The leader schedule now always runs (it ran only with
  the EU filter, so live rows had no leader). Entry/tick offsets are not on the regular stream (empty
  column). Backfill from the chain (`research/fee-engine-2026-10-08/backfill_rivals.py`): 351
  followed launches of 7-8 Oct and the 3 live buys, 6,951 buys, `logs/rival_buys_backfill.csv`.
- **Bundle rule, changed from the prompt's on the data.** The prompt: "tip transfer present and
  index adjacent to the create, or same Jito bundle". On those launches the run of buys right after
  the create was 1-4 buys, never more (a Jito bundle is at most 5 transactions); 206 of 223 runs had
  no tipped buy, and their buys bid a median 0.3 lamports/CU against 23 for the first two buys after
  the run. A tip on each buy would have called 92% of bundles rivals (it put live buy 2's first rival
  at position 1 paying 1.0 instead of 88 paying 142.7). Rule used: the run of at most 4 buys right
  after the create in its slot.
- **A3, bid capture.** `hot::bids`: every pump.fun buy seen on the preprocessed frames and shred
  batches (the slow path's copies, off the hot path), with fee per CU, compute limit and tip,
  completed by the regular stream with block position, bundle flag and rank; one bounded ring per
  leader client class. v1 transactions carry the fee in their config: read there too (26 of the
  first 100 transactions on one coin were v1).
- **A4, first picture** (`research/fee-engine-2026-10-08/a4_2026-10-08.md`): first buyer outside
  the bundle, in the create's slot, fee per CU p50 / p90: Jito-Agave 5.0 / 228 (73 launches), BAM
  37.4 / 782 (78), Harmonic 25.4 / 1,155 (52), Firedancer 37.4 / 625 (16), Agave 3 launches. The
  first outbid the second on 75% of Firedancer launches, 65% BAM, 58% Harmonic, 42% Jito-Agave.
  Our 3 live buys bid 5.8; their first rivals bid 1.4, 142.7 and 1,140.6.

## 2026-10-08: fee engine, Part B (built, off by default)

- `hot::fee` (FEE_MODE=table): the hot path reads `table[client class][tier]` (two atomic loads,
  tested allocation-free), tier = dev-buy bucket (<1, 1-3, 3+ SOL) × reputation (made the reference
  wallet money, or only recent) × mayhem; class from a lock-free class-per-slot ring. A side task
  rewrites the table every 400 ms: floor p92.5 over the class's last 50 slots of pump.fun bids (×3
  on tiers where a buyer outside the bundle usually lands in the create's slot, min 10 lamports/CU),
  times the slow controller (log-fee, step 20%, target position 2, EMA over 8 launches, frozen below
  10 outcomes), under the class cap, the EV cap (κ × buy size − tip) and BUY_FEE_CAP_SOL; Harmonic
  fees jittered off round numbers and its tip from Jito's tip floor p75 (lane floor ≤ tip ≤ 0.002
  SOL). Breakers: >30% fill failures in 20 sends pauses live buys and re-simulates the compute limit;
  30 at-cap attempts with first-or-second under 20% stops raising; a first-position fee over 3× its
  rolling median widens exploration and is not followed. Daily fee budget (failed buys count) stops
  buying. The 10% outbid stays on top, under the caps. Sells unchanged.
- **Where the caps and the floor meet:** when a cap is under the 10 lamports/CU floor the cap wins
  (the engine never overspends its EV rule). With κ = 2% the EV cap binds hard: at the 0.1 SOL paper
  stake it is 8.87 lamports/CU (0.001 SOL at 112,700 CU after a 0.001 SOL tip); at 0.0001 SOL it is
  nil. Replay of 367 recorded launches (`research/fee-engine-2026-10-08/b6_replay_2026-10-08.md`):
  the engine bids 8.9 on every one (cap bound), at or over the first buyer outside the bundle on 40
  of 79 Jito-Agave launches, 25 of 60 Harmonic, 30 of 86 BAM, 4 of 16 Firedancer. The class caps
  (20 / 20 / 100 / 100 / 60 / 20 lamports/CU) are first guesses.
- The replay's floors (180-230 lamports/CU) come from buys on recorded launches only, the contested
  ones; live the floor runs over every pump.fun buy (`hot::bids`), likely lower.

## 2026-10-08: fee-ladder live test, Part C (set up; the owner flips it on)

- **Settings** (`.env`): FEE_MODE=ladder, FEE_LADDER_SOL=0.002,0.005,0.010 (total priority fee at
  the measured limit: 17.7 / 44.4 / 88.7 lamports/CU at 112,700 CU), the arm with the fewest sends
  in the leader's client class, ties by the mint's hash; BUY_SOL_AMOUNT=0.0001; lanes 0slot and
  Everstake in turn (LANE_ROTATE, Helius, Nozomi, Jito, AllenHark, QUIC off), each paying only its
  own 0.001 SOL tip (refunded on a failed buy); LIVE_SLOW_PATH_BUYS=false; stops: 15 buys included in
  a block (LIVE_MAX_INCLUDED, new: tokens or a failure on chain), 6 in a row with no tokens, wallet
  under 0.15 SOL, a hot feed silent for 5 s. DRY_RUN / HOT_LIVE stay the owner's switches.
- **Compute limit, fixed before the test:** the second calibration measured 91.3k CU where the
  first had 102.4k under the same program (different Token-2022 coins), and would have cut the
  limit to 100,400, under the heavier coins' usage. Within one program deployment the limit now
  follows the largest usage measured (112,700 restored); an upgrade starts over.
- **Paper rehearsal from 03:25 UTC:** both lanes accepted every ladder copy (7 of 7 each in the
  first minutes), arms spread over the classes. Report: `research/fee-engine-2026-10-08/part_c_report.py`.

## 2026-10-08: fee-ladder test (Part C) results, and the owner's changes after it

- **Part C, live 08:08-08:32 UTC:** stopped at 9 of 15 included buys by the fee engine's daily
  budget (0.05 SOL), which should not have applied to a fee test: our mistake, fixed below. 9 of 9
  included, 2 got tokens (both 0.010 SOL on BAM leaders, ranks 1 and 2 in the create's slot), 0.053
  SOL in fees and tips. The A4 leader-class picture predicted first-or-second (or not) on 6 of 9:
  3 of 3 on Jito-Agave and Agave (missed whatever the fee), 3 of 4 on BAM, 0 of 2 on Harmonic (first
  rivals bid 2,551-3,484 lamports/CU there, 0.19-0.29 SOL). The first rivals on three launches bid
  no priority fee and tipped Jito ~0.0001 SOL, landing 4-17 ms after the create: Jito bundles.
- **Owner's changes (8 Oct):** the cap is max(κ × buy size, the class's median first-buyer fee ×
  CU limit), never under 10 lamports/CU, κ 2% (BUY_FEE_CAP_SOL stays the ceiling: at 0.005 SOL with
  a 0.001 tip it holds Firedancer, BAM and Harmonic to 35.5 lamports/CU where their medians are 136,
  29 and 26 in the latest replay); the daily budget only in table mode; routes by leader class
  (Jito-Agave and Agave: the class floor and a Jito bundle copy of the same signed buy; BAM: fee and
  the copy; Firedancer: fee; Harmonic: skipped while its median first-buyer fee is over its cap); the
  landing lane (bundle or gateway, from Jito's bundle status) and, for later-slot landings, our send
  time against the first rival and the slot's end, on the regular stream's clock.
- **Live test 3 (set up, the owner flips it):** 12 included buys, 0.0001 SOL, same stops (6 in a
  row, 0.15 SOL, 5 s). Arms: Jito-Agave and Agave `jito:0.0001,0.0005,0.002` (priority at the class
  floor, 10 lamports/CU now); BAM `fee:0.010` / `jito:0.0005`; Firedancer and Frankendancer
  `fee:0.005,0.010`; Harmonic and other skipped. Lanes 0slot and Everstake in turn, the Jito copy on
  its own lane. Paper from 11:38 UTC: every lane took every copy (0slot 15, Everstake 14, Jito 14),
  arms by class as set. Report: `research/fee-engine-2026-10-08/test3_report.py`.

## 2026-10-08: client routing, the distance rule, the lane core (owner's instructions, paper first)

- **Routing by leader client (table mode, `FEE_ROUTE_<CLASS>`):** JitoLabs leaders behind a Jito
  relayer (TPU QUIC port 11000-11999, or a TPU at another address than gossip: 41 of 141 JitoLabs
  leaders on 8 Oct) get only a one-transaction Jito bundle (`sendBundle`, Frankfurt block engine),
  the tip inside the buy with `jitodontfront` on its transfer, the priority fee at the class floor;
  no gateway copy, so no nonce. The tip starts at 0.0003 SOL and follows rivals' Jito tips on
  followed launches (p50) or Jito's tip floor p75, whichever is higher, capped at 0.001 SOL. A
  refusal for the block engine's rate limit (HTTP 429 or "rate limit") is counted and logged
  ("RATE LIMITED"). Harmonic: no bid, the launch is logged (`logs/route_skips.csv`). BAM and
  Firedancer: one gateway copy (0slot or Everstake in turn, `FEE_GATEWAYS_<CLASS>`), the fee from
  the engine, only that gateway's tip. The two-leg nonce design is not built.
  **Our choices, to confirm:** JitoLabs leaders without a relayer go Jito-only too (same reasoning:
  the cheapest way in; `FEE_ROUTE_JITOAGAVE=fee` changes it), Agave and Frankendancer go to the
  gateways like Firedancer, and leaders whose client gossip does not name ("other") are skipped.
- **The distance rule (`hot::distance`, Frankfurt only):** every 5 minutes the QUIC handshake time
  to the TPU of every leader of this epoch and the next, cached per identity. Each TPU address is
  measured once (the fastest of three tries, 100 ms apart) and counts as its best answer over the
  last three sweeps: BAM's 387 leaders sit behind 17 TPU addresses (the BAM nodes) and Harmonic's 78
  behind 8, and a first version that measured per leader sent hundreds of handshakes to one node in
  seconds and got no answer from 92 of the 105 leaders behind the Frankfurt BAM node (2 ms away). A
  leader that refuses the connection still answered; none within 600 ms counts as far, and so does a
  leader not measured yet. Buy only when the create slot's leader answers under 20 ms; in its last
  slot (slot % 4 = 3) with a next leader at 20 ms or more, only when the create came within 250 ms
  of the slot's first shred or frame here. Both paths apply it (the hot path before the position is
  taken; the slow path, which makes the paper buys, with the hot path's receive time of the create).
  Every skip goes to `logs/route_skips.csv` with the leader's city, handshake time and client; per
  day, followed launches, those past the entry rules, and those skipped for distance
  (`logs/distance_daily.csv`, and every 5 minutes in the log); each sweep to `logs/leader_rtt.csv`
  (`research/feed-race-2026-10-08/distance_report.py`). For a leader behind a relayer, a BAM node
  or a Harmonic node the measured address is that node's, which is where our buy has to go.
  **Reach (16:20 UTC sweep):** 347 of 667 leaders, 56% of this epoch's slots: Germany 100%,
  Netherlands 98%, London 8% (median 25 ms), the US and Asia none. By class: JitoLabs behind a
  relayer 95%, JitoLabs 68%, Harmonic 56%, BAM 45%, Firedancer 28%.
  **For the owner:** slots are 263-271 ms now (getRecentPerformanceSamples, 8 Oct), not 400, so
  "the first 250 ms of the slot" covers ~94% of it and the last-slot clause skips little; the same
  share as 250 of 400 ms would be ~165 ms (`DISTANCE_LAST_SLOT_MS`). Not changed.
- **CPU (measured 14:11-14:41 UTC, 1,955 launches, 44-85 a minute):** steal time is negligible:
  0.01-0.08% a minute on average, never above 2% in any second; on the 8 busiest minutes (77
  launches a minute) 7 of 438 seconds showed any steal, all at 1%. The hot cores 5-7 had the least
  (0.05-0.08% mean). The run queue reached 9-11 while builds ran on cores 0-4.
  Applied (`deploy/cpu-isolation.sh`): every slice but the bot's held to cores 0-3; the lane threads
  pinned to core 4 (`HOT_LANE_CORE`), primed ones yielding the core to each other; the kernel line
  `isolcpus=managed_irq,domain,4-7 nohz_full=4-7 rcu_nocbs=4-7 irqaffinity=0-3` in grub, which takes
  effect at the next reboot (not done; the owner decides when). The network queue's interrupts stay
  on cores 2 and 3 (pinned); the other movable interrupts were moved to 0-3.
- **Helius raw shreds:** wired as a third input of the in-process decoder
  (`ENGINE_HELIUS_SHRED_BIND`), raced shred by shred against AllenHark's stream by leader class. No
  raw shreds reach this host: they are a separately paid Helius product (a seat per IP), which the
  owner's "no new paid services" rules out, so the race ran without them.
- **The feed race, 15:16-16:16 UTC (2,017 creates, both feeds saw every one):** Helius's
  preprocessed stream had the create decoded first on 2,015, by a median 2.26 ms (p90 6.9 ms);
  AllenHark's shreds (the proxy's output or the capture of its input, reassembled in-process) were
  first on 2, by 0.11 and 0.47 ms. Same on every leader client: BAM 1 of 754, Harmonic 1 of 535,
  JitoLabs 0 of 343, JitoLabs behind a relayer 0 of 208, Firedancer 0 of 127 (preprocessed ahead by
  a median 5.3 ms there). `research/feed-race-2026-10-08/race_2026-10-08_1516-1616.md`. What the
  shreds still give us if AllenHark is cancelled: each slot's first-seen time (the distance rule's
  last-slot clause; the preprocessed stream only marks a slot's first pump.fun transaction), the
  blockhash from shreds (RPC's remains), and the shred-lead logs.
- **Preconfirmations:** not run. Our Helius plan answers `preconfSubscribe` with "Method not
  available on your current plan".

## 2026-10-08 (evening): off AllenHark, new cutoffs, JitoLabs alternation (owner's decisions)

- **Cutoffs:** the distance rule's limit is 30 ms (London included; 20 before), the late-create
  cutoff in a leader's last slot 165 ms (~62% of a 267 ms slot; 250 before).
- **JitoLabs leaders without a relayer:** launch by launch in turn, a Jito bundle alone, then one
  gateway copy (0slot or Everstake in turn), `FEE_ROUTE_JITOAGAVE=alternate`; the row's
  `fee_arm_label` says which ("jito bundle" / "gateway"). Relayer leaders stay bundle-only.
- **Off AllenHark (the owner cancels it once nothing depends on it).** What the shred stream gave,
  and where it comes from now:
  - creates for the hot path: Helius's preprocessed stream (first on 2,015 of 2,017 creates), and
    when it is silent 2 s, LaserStream's executed transactions through a feed thread of their own
    (`hot::backup`, feed "Grpc", `HOT_BACKUP_AFTER_MS`); the live guard stops on a stall only
    when both are silent;
  - slot starts for the last-slot clause: LaserStream's slot updates ("first shred received" from
    Helius's node; `utils::slot_feed`), compared every 5 minutes with the other feeds' sightings;
  - the backup blockhash (RPC's older than 2 s): LaserStream's block meta, two blocks behind the
    newest (`blockhash::GRPC_HASH`), in place of the shred book's;
  - rival bids (`hot::bids`): already read from the preprocessed frames and the regular stream
    (LaserStream); the shreds only gave an earlier sighting of the same transactions;
  - the slow path's early sightings: the preprocessed frames already;
  - logs only: `shred_lead.csv`, the engine parity and the feed race.
  `ENGINE_INPROC=false` runs without shreds. Outside this bot: the strategy-b engine (another
  session) reads the same proxy's output for rival bids and a fallback blockhash, and uses AllenHark
  Relay as one of its five send lanes; it keeps running without them, minus those.
- **Positions log:** `leader_rtt_ms` (the create slot's leader's handshake time), and for paper rows
  `would_rank_time` (rank among the buyers outside the bundle by time alone, had ours reached the
  leader half a handshake after it left: optimistic, it ignores fee ordering) and
  `rivals_outbidding` (buyers in the create's slot with a higher fee per CU).
  `research/routing-2026-10-08/report.py` reports per client class and by handshake bucket.

## Open before any live run

- **The follow list was rebuilt on 2026-10-07** (see the entry above); keep a copy of
  `reference_deployers.txt` outside the bot's folder, since it is not in git.

## 2026-10-11: hot path speed, detection to buy and sell (owner's "upgrade bigboss's speed")

Branch `hot-speed-2026-10-11`, not merged; bigboss stayed stopped (the copy bot holds cores 5-7
and the shred port). Every piece sits behind its own switch, on by default, with the old path as
the fallback. Nothing ran live yet: the before/after on chain comes from `logs/hot_stamps.csv`
(stages `build_sign`, `derive`, `total_excl_wait`, and the new `Sell` feed) on the next run.

**Baseline** (`logs/hot_stamps.csv`, 47,964 preprocessed launches, SPSC, 2026-10-07/08; p50 / p90
/ p99 µs): decode 6.3 / 12.9 / 34.6; SPSC hand-off 0.7 / 28.9 / 128.9; derive 10.5 / 50.7 /
114.9; bundle window 227.6 / 239.1 / 256.8; decide 8.3 / 17.1 / 43.2; sign 46.3 / 70.2 / 120.2;
base64 1.0; frame in to bytes ready 309.9 / 343.6 / 433.4 with the window, 59.9 / 93.6 / 183.4
without. The copy bot (strategy-b) does the same work in 10 µs to its first signed lane, with no
window; its techniques were reviewed against this path and the missing ones ported.

1. **Presigned Ed25519** (`HOT_PRESIGN=true`, `hot::presign`, the copy bot's `sign::presign`
   ported). The signature's R = r·B is made ahead of time by a thread off the hot cores (1,024
   pairs, refilled every millisecond); the hot thread finishes each signature with one SHA-512
   (ring's) and a multiply-add. An r signs one message only (used once, wiped). Empty queue:
   the keypair signs as before. `hotbench`, 20,000 runs: build+sign 20.7 → 2.2 µs p50, 37.5 →
   3.6 p90, 61.5 → 12.3 p99. Tests: signatures verify strictly and through the Solana crates, no R
   twice, the fallback is byte for byte the keypair's, no allocation; the create-to-signed
   allocation audit now runs presigned.
2. **Built for this CPU** (`.cargo/config.toml`: `target-cpu=haswell`, measured 3% on signing and
   hashing on the copy bot). The bot builds and runs on this machine only.
3. **Creator vault derived once per creator** (`decide::creator_vault_of`): a thread-local
   4,096-entry table; a repeat deployer skips one `find_program_address` (~6-10 µs of `derive`).
   Test: same address, no allocation on a hit.
4. **Sells through the hot lanes** (`HOT_SELL_LANES=true`, `hot::sell`). The slow path still
   decides and builds the sell's instructions; they are signed with a presigned nonce (its own
   queue), one tip per tipping lane inside (the lanes' tips, as a buy), the sell profile's
   priority fee and compute limit, and handed as signed bytes over a 64-slot ring to the decision
   thread, which primes the threaded lanes and writes every lane as it does a buy: no HTTP client,
   no allocation on that thread. Stamped as `Feed::Sell` (`t_recv` queued, `t_picked`, `t_ready`,
   lane writes) in `hot_stamps.csv` / `hot_stats.log`. The landing is awaited by signature as
   before. Not taken (the slow path sends as before): DRY_RUN, inline mode, a full queue, no
   blockhash, a transaction over a packet. Tests: one valid signature, every tipping lane's tip in
   firing order with rotation, the sell after the tips, a full queue refuses, the decision thread's
   write allocates nothing and stamps the sell.
5. **The 250 µs bundle window: measured, not changed.** With the copy bot's `pp_bundle_watch`
   (every Pump.fun transaction on Helius's preprocessed stream, 60 minutes, 2026-10-11 20:44-21:44
   UTC): 2,391 creates; 691 (29%) got a same-slot buy. A bundle's buys (same slot, within 5 ms of the
   create; later same-slot buys are other snipers, first one 37 ms after the create at p50) are in
   421 creates (18%): the first bundle buy arrives 54 µs after the create at p50, 138 at p90; the
   whole bundle is in by 100 µs for 71%, 150 µs 86%, 200 µs 91%, 250 µs 93%, 500 µs 96%; a
   bundle spans 10 µs p50 / 61 p90 / 106 p95 from its first buy to its last. Proposal (not
   applied, the owner's call): (a) a 200 µs window loses 2% of whole bundles for 50 µs on every
   launch; (b) close the window 80 µs after the first bundle buy, 250 µs at most: the same 93%
   coverage, and the window closes at 134 µs p50 / 218 p90 on the 18% of launches that have a
   bundle, 250 µs on the rest. Raw capture and the numbers: strategy-b-live/research/out/live-1/
   pp_bundle_2026-10-11.{csv,txt}. **Rechecked on 12 Oct** (`research/hot-speed-2026-10-11/
   window.py`, same raw file): (b) covers 90.5% of whole bundles, not 93% (7% of bundles span more
   than 80 µs), and saves 18 µs on average over all launches (232 vs 250 µs), since 82% have no
   bundle and still wait 250. Saving more needs closing early when no bundle buy has come: +80 µs
   or 150 µs with none averages 148 µs but covers 87.2%; or 180 µs with none, 174 µs and 89.1%.
   The bundle rule and the price guard read these buys; the first other sniper arrives 37 ms after
   the create (p50), 150 times the window. Recommendation: keep 250 µs. Not changed.

Still on the table, not built: lookup tables for the buy (a third off the message, costs rent);
a second writer thread (7 µs on the last lane, measured on the copy bot).

**Continued on 12 Oct (the owner's speed spec, Steps 0-6; same branch).**

- **Step 0, from the stamps** (`research/hot-speed-2026-10-11/stages.py` over `logs/hot_stamps.csv`,
  47,964 preprocessed launches, 22 runs, 7-8 Oct; p50 / p90 / p99 µs): decode 6.3 / 12.9 / 34.6;
  hand-off 0.7 / 28.9 / 128.9; derive 10.5 / 50.7 / 114.9; window 244 / 248 / 289; decide 8.3 /
  17.1 / 43.2; build+sign 46.3 / 70.2 / 120.2; base64 1.0 / 1.5 / 4.1; frame in to bytes ready
  309.9 / 343.6 / 433.4, less the window 59.9 / 93.6 / 183.4. Bytes ready to the lane's write
  (runs 17-21, with the lane core): 0slot 43 / 70 / 109; Everstake 88 / 144 / 262; Jito 75 / 167
  / 218 (before the lane core, 8 Oct morning: Jito p90 2.9 ms, Everstake p99 5.5 ms).
- **The hand-off's tail was the warm-up, not a park.** 5,484 of the 5,698 hand-offs over 20 µs
  came while the decision thread did no launch work: its idle warm-up (a derive and a signature in
  one go, every 1 ms) held the ring for 60-130 µs. **`HOT_WARM_STEPS=true`**: the warm-up in six
  steps of at most one address search, the rings looked at between steps (`decide::derive_step`).
  Bench (`hotbench`, a producer at random 0.2-2 ms gaps on another core): hand-off p90 260 µs
  with warm-ups in one go, 128 µs in steps, 109 µs with no warm-up at all (cores 0-1, shared:
  their tails are other work's; the live stamps will show the real tail).
- **The decision's lookups moved into the window. `HOT_PREDECIDE=true`** (`decide::prepare` at the
  create's arrival, `decide::decide_prepared` at the window's end): the follow list, deployer
  history, balance, wallet age and records (each a lock and a map) are read while the window runs
  anyway; at its end only the bundle's arithmetic, the rules and the quote are left. Same checks in
  the same order; a value missing at arrival (balance, wallet age, config) is looked up again at the
  end. `decide` is `prepare` then `decide_prepared`, so the old path is unchanged. Tests: identical
  outcomes on every fixture, a balance arriving mid-window is used, neither half allocates. Bench
  0.42 → 0.21 µs; live the decision took 8.3 µs p50 / 43 µs p99 (cold caches, big tables, locks
  shared with the slow path), most of it these lookups.
- **The creator vault cache got its switch** (`HOT_VAULT_CACHE`, default true); bench 4.3 → 0.03 µs.
- **Lanes primed before the derive** (was after; up to 115 µs earlier), so threaded lanes are
  spinning sooner.
- **Paper runs now exercise the sell path.** With DRY_RUN and the lanes timing paper trades
  (HOT_DRY_LANES), each paper sell sends an unlandable copy (all-zero blockhash, as the paper buys')
  through the hot sell queue and lanes (`sell::send_dry`), stamped as `Feed::Sell`; before, the hot
  sell path could first run on a live start.
- **Threaded lanes' write time stamped where it happens.** Their `t_sent` was taken when reqwest
  built the request, before its task ran; now at the request's first poll (when a warm connection
  writes it). `hot_stats.log` adds per lane `pickup` (handed over → taken by the lane thread) and
  `build` (taken → first poll), so the next run shows where the 75-90 µs goes.
- **Allocation audit extended:** the lane writes on real sockets (prime, every lane, each turn: no
  allocation on the writing thread; a test-only record in `Lanes::fire` grew and was reserved),
  the prepared decision, the stepped warm-up. With the earlier ones (create to signed, presigned and
  RFC 8032, shreds to signed, fee lookup, the sell's lane write): every hot path, zero allocations.
- **Measured and left alone:** the 250 µs window (above; owner's call); the lane firing order
  (threaded lanes are handed over first, ~0.1 µs each, then the direct writes, ~40 µs for the first
  and ~6 µs for each next: with routing a buy writes one gateway and at most one Jito copy, so order
  is worth µs, and no landing-order data per lane exists yet); the LaserStream backup feed's thread
  parks between frames (1 ms), but its transactions arrive after execution, hundreds of ms late;
  derive (10-115 µs) runs inside the window; lookup tables.
- **Fat LTO** (`Cargo.toml`, one codegen unit as before, `thin` before): measured alternately
  against thin, three runs each on the same cores (`lto_thin_vs_fat.md`): single-stage benches
  within ±2%, the whole inline path (frame to bytes ready, window excluded) 8.6 → 6.7 µs p50
  (decide −18%, build+sign −12%, derive −12%): inlining across crates. The 187 tests pass under it.
  Costs build time: 12 minutes for the first build after the switch, about 5 after a code change, on
  cores 0-3 (`bot.sh` builds only `sniper-bot`).
- **Benches** (`research/hot-speed-2026-10-11/hotbench_*.md`) ran on cores 0-1, shared with the copy
  bot's other threads and the shred proxy (cores 4-7 are isolated; 5-7 held by the copy bot): p50s
  are sound, tails are other work's. The live numbers come from the stamps of the next run.

How to turn each piece off (`.env`, then restart): HOT_PRESIGN=false, HOT_SELL_LANES=false,
HOT_PREDECIDE=false, HOT_WARM_STEPS=false, HOT_VAULT_CACHE=false; target-cpu: delete
`.cargo/config.toml` and rebuild; LTO: `lto = "thin"` in `Cargo.toml` and rebuild.

## 2026-10-12: three upgrades, step 0 and step 2 (owner's answers; branch landing-2026-10-12)

**Step 0, the owner's answers.** PumpSwap: A only (a followed deployer's coin graduates: buy its
canonical pool at the migration), paper first; B (a pool created directly) after a 24-hour count
once step 4's subscription exists. A coin bought at launch may be bought again at graduation once
the launch position is closed (a claim per venue). The bundle window stays 250 µs, no switch built
now. Order 2 → 5 → 1 → 3 → 4.
- **Correction:** the 25 copied pool buys that lost (−0.178 SOL, strategy-b `docs/COPY-POOL.md`)
  were pXGQ's, the copy bot's leader, not bigboss's reference wallet (8RvtT8): the reference
  wallet's PumpSwap results are untested.
- **Followed coins that graduated** (`research/pumpswap-2026-10-12/graduations.py`; launches from
  `logs/deployer_launches.csv`, the migration from each curve's token account, whose newest
  successful transaction is the `MigrateV2`): 7 Oct 19 of 921 (2.1%), 8 Oct 26 of 1,480 (1.8%).
  Of the 39 whose migration was found, **none graduated in the launch burst** (≤ 5 s): the fastest
  49 s after launch, the median 13 min (7 Oct) and 9 min (8 Oct). The copy bot's 7.7× pump at
  +30 s belongs to coins that graduate in the launch bundle: not these. Bigboss bought 2 of the 45
  at launch (paper, 7 Oct).

**Step 2: landing first.** Each piece behind its own switch, off by default; 201 tests pass.
1. **Paper never signs with the wallet's key** (always on, the owner's rule): the hot path's
   unlandable copies of paper buys and sells are signed by a throwaway key made at each start
   (`config::PAPER_KEY`), a paper sell's copy names it as seller; the slow path's paper
   simulations carry a blank signature (they simulate with signature checks off).
2. **`ASYNC_FILE_WRITES`**: a live buy's trade row is queued after its sell monitor starts (it was
   written first); the reference wallet's trade appends and the shred-lead rows leave the regular
   stream's task that feeds the sells. One writer thread, bounded queue, a full queue drops and
   counts (`hot_stats.log` "file writer").
3. **`HOT_NONCE_FANOUT`**: every lane of a buy's route gets its own copy, all on one durable nonce,
   each paying only its lane's tip: at most one lands, one tip is paid, and the landed copy names
   its lane. The accounts' rules after the copy bot's pool (own code, `hot::nonce_pool`): an
   account is reused only once a read at or after its landing's slot shows a new value, or our
   cancel settled; a copy not landed in 2 s is cancelled; a live start advances each once; the
   copy bot's bug (a read freeing an account before its cancel landed) is a test. The decision
   thread takes free accounts from a lock-free ring and reports each use on another: no lock, no
   allocation (tested on a real socket, the wire copy read back byte for byte).
4. **`HOT_SELL_FANOUT`**: sells the same way (was: one transaction paying every lane's tip,
   0.002-0.003 SOL a sell); re-bid every slot on the same nonce with the fee doubled, 4 times at
   most, the whole priority fee under `SELL_FEE_CAP_SOL` (else `BUY_FEE_CAP_SOL`); then cancelled,
   and once the cancel has settled (no old copy can land) a fresh sell with a fresh quote. With
   the switch on the bot never stops selling while tokens remain. The sell's instructions are
   built once per position at the buy, the amounts patched per sell.
5. **`SELL_PREEXEC_TRIGGERS`**: an insider's (dev's, bundle wallet's) top-level pump.fun sell seen
   on the preprocessed stream is recorded and checked against the regular stream (executed, or
   not within 10 s: `logs/preexec_sightings.csv`, `hot_stats.log`); with the switch on it triggers
   our sell then. The paper run measures how often such sightings do not execute, for the owner
   before it goes live. Sells routed through another program are not seen there.

**Bench** (`hotbench`, cores 0-1 shared with the copy bot's other threads: p50s): one buy fan-out
copy (fill, nonce, presigned, base64) 2.8 µs, so each extra lane's copy is ready 2.8 µs after the
one before; one sell copy built and presigned on the slow path 4.8 µs (the slow path's sell build
was 75 µs). Landing per lane, tips per buy and sell, and the pre-execution sightings come from the
measured run (step 5).

**Nonce accounts (not created; the owner decides):** 5, at 1,056,640 lamports rent each (0.0053
SOL in all, refunded when closed): 2 kept ready for the decision thread, 1 for a previous buy's
account while it settles, 1 for the open position's sells and re-bids (MAX_OPEN_POSITIONS is 1),
1 spare. Each unlanded copy costs a cancel (5,000 lamports and a small fee); each live start
advances every account once. `cargo run --release --bin nonce -- create 5 --yes`, `list`, and
`close --yes` (withdraws everything back to the wallet).

**The owner's answers to step 2 (12 Oct) and what changed:**
- **Live guard:** the buy limits count live buys only; a paper buy never trips them, so
  `LIVE_MAX_INCLUDED=12` and `LIVE_MAX_FAILED_IN_A_ROW=6` stay set in paper runs.
- **STUCK** (with `HOT_SELL_FANOUT`): selling stops for a coin that can't be sold, after 3 fresh
  sells in a row fail with the same lasting program error (the price guard 6002/6003/6042, the
  creator's account moving 2006, a blockhash or nonce, and not landing are temporary and never
  count), or once the fees its sells spent exceed what its tokens are worth now. Dashboard alert;
  the tokens stay in the wallet.
- **Paper sells** with `HOT_SELL_FANOUT` send one copy per lane on a synthetic nonce (the throwaway
  key, nothing can land); re-bids and cancels need landings, so only a live run measures them.
- **Nonce accounts: not created.** The owner said yes to 5 on bigboss's wallet (`92t8Lzs…`, not
  the copy bot's `E3YwB5…`), but that wallet held 0.001979 SOL on 12 Oct against 0.0053 SOL of
  rent plus fees needed. The paper run uses synthetic nonces and doesn't need them.
- **PumpSwap (step 4) postponed:** followed coins graduate 49 s to hours after launch, the class
  the copy bot's study found flat at migration and near 0.16× by +10 min. No buying rule; only the
  24-hour count of option B (pools created directly by followed deployers) stays, as a later paper
  task.
- **Measured run:** paper only, 3-4 hours inside 08:00-15:00 UTC while the copy bot's leader is
  quiet; the owner stops the copy bot (when it holds nothing), starts bigboss in paper, and switches
  back after. Switches on: `ASYNC_FILE_WRITES`, `HOT_NONCE_FANOUT`, `HOT_SELL_FANOUT`,
  `SELL_PREEXEC_TRIGGERS` (and the paper key, always), plus `HOT_PRESIGN`, `HOT_SELL_LANES`,
  `HOT_PREDECIDE`, `HOT_WARM_STEPS`, `HOT_VAULT_CACHE`. No live start on this branch before the
  owner has seen the paper report.

## 2026-10-12: paused (the owner's change of plan)

The copy bot (strategy-b) is the priority now, sniping isn't. The paper run of step 5 is cancelled:
the owner won't stop the copy bot for bigboss. bigboss stays stopped and disabled; the branch
`landing-2026-10-12` is kept as it is, its code at 82de75e (built, 203 tests pass, nothing of steps 2
onward has run on live or paper traffic). `.env` still has the paper run's switches on
(`ASYNC_FILE_WRITES`, `HOT_NONCE_FANOUT`, `HOT_SELL_FANOUT`, `SELL_PREEXEC_TRIGGERS` and the speed
switches) with `DRY_RUN=true`, `HOT_LIVE=false`. No nonce accounts exist. Work stops here until the
owner comes back to it; the next step would be the paper run, then its report, before any live start.


## 2026-10-12: the German copy (this server, srv2054763 moved to Germany, 187.7.75.220, 4 vCPU EPYC 9355P)

The owner moved this Hostinger VPS from the US to Germany; the move wiped the disk. bigboss was
copied again from the OVH Frankfurt server's /home/ubuntu/Documents/bigboss, without target/, .git
or logs/tapes. Settings only, no code changes. DRY_RUN=true and HOT_LIVE=false unchanged; the bot was
not started.

- **Distances from here** (TCP connect, best of 5), against the OVH Frankfurt server where the copy
  bot runs:

  | Endpoint | Here | OVH Frankfurt |
  |---|---:|---:|
  | Frankfurt lanes and feeds (jito, everstake, nozomi fra2, helius sender, laserstream fra) | 3.6 ms | 1.5 ms |
  | 0slot de1 (sits in OVH's Frankfurt data centre) | 21 ms | 0.16 ms |
  | 0slot ams1 | 3.6 ms | 18.9 ms |
  | Amsterdam lanes | 6.3 ms | 12.9 ms |
  | Helius RPC (anycast) | 0.65 ms | 1.5 ms |

  ZERO_SLOT_API_URL is therefore http://ams1.0slot.trade/. Everything else stays on its Frankfurt
  endpoint.
- **LEADER_REGIONS=EU**: this copy buys only launches whose block producer is in Europe.
  DISTANCE_RULE stays on at 30 ms, measured from here.
- **Cores for 4 vCPU:** HOT_CORES=2,2,3 (feed 2, decision 3, the off shred feed sharing 2),
  HOT_LANE_CORE=1, the rest on core 0. No isolcpus or hot.slice (the owner's call: it needs a
  reboot and a system service).
- **Build:** target-cpu=native (AVX-512); Rust 1.99; release 4 min 13 s; cargo test --release: 204
  passed, 0 failed.
- **`latency --seconds 120`** (sends nothing): 53 launches; Helius -> this server 0.04 ms; Nozomi
  Frankfurt round trip 3.87 ms (the US copy: 8.5 ms); RPC getSlot 13 ms.
- **Wallet:** the Frankfurt wallet (92t8..., about 0.002 SOL). This is now the EU copy; the OVH
  server's bigboss stays stopped, so the two never trade at the same time.
- **.env and keys/ are 600/700, owned by root.**

## 2026-10-12: scorecard entry, pXGQ's exit, SQLite for the new data (owner's instructions)

**Asked:** implement the slot-0 entry filter and pXGQ's sell rule, keep the new data in SQLite,
test for one day in paper. Owner's answers: sell rule pXGQ's (shown first that on the bot's own
followed launches at position 0 it scored +0.070-0.082 SOL a 0.5 SOL try against the bot's exit's
+0.103-0.104, research/filter-pXGQ-2026-10-12/pxsell.py); threshold "precision" (top 20%);
SQLite for the new data only (the CSV and text files are unchanged); paper.

**Entry (`SCORECARD=true`, `config::scorecard`, `rules::score_facts`):** a followed launch is bought
at `SCORECARD_MIN` (61) points or more from 15 facts known in the create's slot; the score
replaces the dev-buy range, the deployer's minimum balance and wallet age, the relaunch skip and
the no-buyers record (on Oct 8-12 the old rules kept 13.5 launches an hour at +0.073 net a try,
the score alone 8.7 at +0.102, both stacked 4.1 at +0.108; research FILTER_SPEC.md and README §7).
The follow rule and the mayhem skip stay. Card `research/filter-pXGQ-2026-10-12/scorecard_bot.json`
(the four create-transaction facts left out: no change in the out-of-time result, and no change to
the shred decoder). Card parity with the research checked bin by bin. Both paths score the same
`LaunchFacts`; the hot path skips its wallet-age rule when the score is on.
- **Launches without a top-level dev buy are not bought:** the early feed skips them (`shred_feed`,
  "no top-level dev buy") as before, since it can't tell no dev buy from one inside another
  program. On Oct 8-12 they were 3.6 of the 8.7 high-score launches an hour (+0.111 a try); the
  bot can buy ~5.1 an hour (+0.096 a try, 77% won). Supporting them is a shred-decoder change, not
  made before the test.

**New data, SQLite (`DB_PATH`, default `bigboss.db`; `utils::db`, one writer thread):**
`deployer_wallet` (each deployer wallet's first transaction and the SOL it received, RPC once per
wallet, prefetched for every reference key at startup: `utils::deployer_wallet`), `launch_result`
(every followed launch's replayed result for 0.5 SOL behind the bundle with the bot's exit, and
whether it went under 0.9x before its best within 30 s, keyed by coin creator:
`utils::launch_results`, `launch_outcome` now watches 160 slots; the replay is `rules_check dump`'s,
identical on 400 test tapes, `rules_check results`), `score` (every followed launch's score, facts
and points, verdict), `exit_decision` (every sell decision of pXGQ's exit).

**Exit (`SELL_RULE=pxgq`, `auto_sell::PxExit`, `Exit` picks it per position):** all out at x0.85 of
cost; 2-8 s after the buy all out under x1.05 with under 1 SOL net bought behind; 50% at x1.25
near the high, then the rest 20% off the peak or at x3; without that, all out 15% off the peak
while at cost or better; all out under x0.95 when others sold 4+ SOL in 3 s; 600 s cap. PX_*
overrides. `SELL_RULE=ref` is the bot's exit, unchanged. `MAX_OPEN_POSITIONS` in the environment
(default 1): pXGQ's exit holds longer (p90 268 s on the replay; 1 position missed 11% of
high-score launches, 2 missed 1%).

**Also:** `reference_deployers.txt` had stopped at 8 Oct 20:11 and the startup backfill reaches 3
days; the reference wallet's 2,858 transactions since then (4,236 lines, the file's format) were
appended (original kept as `reference_deployers.txt.before-backfill-2026-10-12`). Source before
the change: `backup-before-scorecard-2026-10-12/`. 216 tests pass (new: card bins and parity,
verdict with the score, missing facts, PxExit branches, launch-result replay, database rows).

**Paper smoke run 22:06 UTC:** startup, database, 3,345 wallets cached, launch results and scores
recorded with their breakdowns. Trading starts paused with the dashboard on (press "Start
trading"); it was started from the dashboard's API for the smoke run (22:26-22:55): 37 followed
launches scored, 12 at 61+, 9 buys attempted, 1 filled; 8 lost the race (3-9 other buys for 4-9 SOL
landed ahead and moved the price past the 1.25% guard). The filled one was sold whole by pXGQ's
exit at 2.1 s (NO_WAVE: nothing bought behind it), x0.975. High-score launches pump in their first
trades (Oct 8-12: price +9% behind one other trade, +16% behind 3-5), so behind the bundle the guard
lets about a quarter of them fill (those still made +0.05 a try); expect 1-2 fills an hour, not 5.
Restarted on the final binary at 22:55, paused, for the owner's one-day test. Report:
`tools/scorecard_report.py SINCE`.

## 2026-10-12 (night): Part A, the store (SQLite), behind STORE_DB (default off)

**Engine (`src/store`):** the copy bot's model. `data/bigboss.sqlite`, WAL, synchronous=NORMAL,
busy_timeout 5000; one writer thread on a bounded channel (65,536 rows; a full queue drops and counts
the row, never blocks); transactions of 250 ms / 500 rows; stats (enqueued, dropped, written, errors,
batches, last and longest batch) in the startup line and every 60 s; migrations in
`PRAGMA user_version`, a newer database refused; tapes in `data/tapes/YYYY-MM-DD.sqlite`, 7 days kept.
Hot threads unchanged (rings, reporter threads): every write is a queue push from where the file was
written before. `launch_results::earlier` allocated on the hot path when a creator had history (the
scorecard's, 2026-10-12): fixed, and the allocation audit now seeds that history.

**Schema 0001:** positions and orders with the copy bot's columns (its 0004/0007/0008/0009) plus
`venue` ('pumpfun'/'pumpswap') and bigboss's extras (positions: dex, deployer, follow_kind, tier,
rent_locked_lamports; orders: lane, sell_step, detect_to_ready_ms, launch_to_landed_ms, pnl_percent);
runs (one per start, the switches in force); launches (every launch seen, venue, deployer, verdict and
skip reason, score); activity; one table per file (creator_history, deployer_launches,
reference_trades, lookup_tables, kv for the CU calibration, validator_regions, token_names, and the
measurement logs with their files' columns); the scorecard's four tables (moved from bigboss.db,
copied once when the store's are empty).

**Switches:** `STORE_DB=on` dual-writes (every file's rows, the trade log also as orders and
positions, launches and their verdicts from the activity feed's Detected/Skipped plus the silent
exits). `STORE_FILES=off` (only with STORE_DB=on) stops the files and reads the startup data from the
store (creator history and the no-buyers record 7 days, by indexed queries; each load logged with its
time). The store also opens with only `SCORECARD=true` (its tables), writing nothing else.

**Measured:** importer, all files of this server: 330,453 rows and 2,591 tape launches in 2.9 s, no
errors (one malformed hot_stamps line, 25 fields under 24 columns, skipped and counted). Export
round trip identical value for value (paper trades 296/296, creator history 103,863/103,863, rival
buys 11,189/11,189, reference 26,228/26,228). Startup loads (`store timings`): the store is not
faster (creator history 7 days: file 13.7 ms, query 19.5 ms; the rest under 5 ms either way); what
changes is that the queries stay bounded as the files grow. `append_csv` read the whole positions or
rival-buys file on every 500 ms batch to check its header (4 MB): now the first line only. The
backup is online (one-step SQLite backup) and was checked with a writer inserting 14,020 rows during
the copy: integrity ok, counts in range.

**Tools:** `store import [--force]` (files untouched; each source once, recorded in kv),
`store export <table> [--mode]` (the file's format), `store stats`, `store timings`;
`tools/store_compare.py [--since DAY]` (per-day counts and daily sums, files against tables; exit 0
only when all match); `scripts/db-backup.py`.

**Switchover (the owner's steps):** stop the bot; `target/release/store import`; set `STORE_DB=on`;
start. After a day or more: `tools/store_compare.py --since <that day>`; when it says "all match",
`STORE_FILES=off` and restart. Until the owner restarts with this build, the running bot is the old
binary (its scorecard data in bigboss.db, copied over at the next start). 226 tests pass.

## 2026-10-13 — Part B: the dashboard (dashboard/, bigboss-dashboard.service, Caddy on the public IP)

**What:** the copy bot's dashboard (Node 22, Fastify 5.12.5, argon2id logins, 5-fail lockout per
address, login rate limit 20/min, helmet CSP, HttpOnly/Secure/SameSite=Strict session, CSRF on
every POST, audit log) ported into `dashboard/` (code copied, nothing linked). It reads
`data/bigboss.sqlite` read-only and follows the bot's existing local API (`/api/events`, the stream
the bot's own page reads, so the bot does no new work for it). Mapping: leader filter → deployer
filter; a venue filter (pump.fun / PumpSwap) everywhere; Copy → Snipe (launches and graduations
seen; followed launches bought / skipped with the reason / paused; the deployers' records; the fee
engine's leader classes; distance and LEADER_REGIONS skips; the bundle window); Signals → Live
activity; Rejections → the funnel per hour with samples by venue; a Runs tab. Status bar: Bot,
Mode, Snipe p99 (frame → signed, hot_stamps), Feed p99 (frame → matched), Gaps (in-process shred
feed), Open, Uptime, Store drops (kv 'store_stats', written by the store's existing minute report).
Mode is paper or live, never both. One history row per coin; every sell step in the side panel.
The calendar's day net is the sum of the day's orders' wallet_delta_lamports, the token-account
rent on its own line. Wallet card: getBalance of the bot's own public key on a public RPC, USD as
the reference, no mode switch.

**Controls (the owner's word):** trading on/off and the buy size only, each confirmed in the page
and audited. No process start/stop, no sells, no paper/live switch; the server refuses any other
action (404).

**Serving:** `bigboss-dashboard.service` on 127.0.0.1:8790 (the bot's page stays on
127.0.0.1:8787), CPU 0, Nice 10, ProtectSystem=strict, no capabilities. Caddy 2.11.7 (.deb,
checksum verified) on TCP 443 only (no port 80, no HTTP/3), Let's Encrypt short-lived IP
certificate over TLS-ALPN; obtained for 187.7.75.220 on the first try. `scripts/dashboard-caddy.sh`
rewrites the Caddyfile from `deploy/Caddyfile.template` for the server's current public IP before
every Caddy start (drop-in `deploy/caddy-bigboss.conf`), so a new VPS is served at its own IP. ufw
was left as it was (inactive): enabling it risks SSH, xrdp and the bot's UDP 36301.

**Tests:** `cd dashboard && npm test`: 23 (logins, lockout, cookie flags, CSRF, only the two
controls, the bot's login never in a response or the audit log, mode never mixed, per-coin rows and
steps, filters, calendar and rent, overview, Snipe, funnel, activity, runs, read-only store, a
missing store, the bot down). Bot: 227 lib tests (with `counters_are_published_for_the_dashboard`).

## 2026-10-13 — The snipe test (SNIPE_TEST=1, default off)

**What (the owner's spec, with fixes 1, 2, 3, 5 as accepted):** pre-registered rule = followed
deployer + scorecard >= 61 (unchanged, applied first) + four rules on what the bot sees at the
decision (dev buy < 1 SOL; <= 1 buyer besides the dev in the bundle, an unreadable buyer refused;
< 1 SOL real in the curve after the bundle; curve not complete) + the exit: all out at +30% (the
tokens' executable value over their cost) or 15 s after the fill, no stop, no trail. Size
`SNIPE_TEST_SOL` (0.1), never scaled (no 65% small bet, no cut to the wallet), refused by the bot's
API and the dashboard in test mode. Caps (UTC): 50 buys a day, 2 an hour, 0.5 SOL realized loss a
day; an entry is taken at the commit point (hot path: after the position slot, before the send;
slow path: before the buy), so a launch refused there was never sent; the counts are read back from
the log at start. Hot path: the checks are allocation-free (audit test added).

**Rent:** the live full sell already closes the token account in the same transaction (the pump.fun
sell builder adds the close; partial sells strip it), so live rent comes back; paper never charged
it. The chain's rent is 1,488,440 lamports (165-byte SPL) and 1,513,840 (170-byte Token-2022), read
again at start; the store's constants were the old 2,039,280 / 2,074,080 and its positions kept
rent "locked" after the close: both fixed (a closed position has none, paper none). Rent left in an
account (a position not fully sold) is a cost in the test's PnL.

**Log:** one line per snipe (`logs/snipe_test.csv`, store table `snipe_test`, migration 0002):
mint, create slot, landed slot, block index and rank in the slot (live from the regular stream's
tape; paper the drawn landing), dev buy, bundle buyers and SOL, real SOL after the bundle, the whole
create slot's buyers / real SOL / completeness and whether the filter would pass on it, fill price
vs the price after the bundle, priority fee and tip, buy and sell fees, exit trigger (TP/TIME), exit
SOL, hold, rent not recovered, PnL after every cost, wallet age, funding, the scorecard's facts.
`tools/snipe_test_report.py`: per day and in total, splits by each rule and fact (information
only), and the gate: >= 300 live snipes on >= 3 UTC days and the lower 95% bound of the mean PnL per
try above 0. The bot never raises the size.

**Baseline on what the bot sees (fix 2), `research/snipe-test-2026-10-13/baseline.py` on this
server's tapes (4,589 launches, 3.2 h, Oct 12 22:06 - Oct 13 01:20 UTC), 0.1 SOL, 1.25% a side +
0.0025 SOL tips, rent recovered:** A (the recording's whole-slot filter) -3.3 mSOL [-4.6, -2.0],
28% positive; B (bot-visible filter, entry behind the bundle) -2.2 [-3.6, -0.8], 33%; C (bot-visible,
landing after the create slot) -3.1 [-4.5, -1.8], 31%. The two filters disagree on 4.8% of launches
(7.2% of those the bot-visible filter passes; never the other way). The follow rule + scorecard
subset can't be replayed from the tapes (no follow state recorded): only the live test measures it.

**Tests:** 237 lib tests pass (filter, both exits, caps by hour/day/loss and across a restart, the line
and its PnL with rent, a failed buy, paper rent, the tape's create-slot view, allocation audit);
dashboard 24.

## 2026-10-13 (later) — The snipe test becomes one test of the whole bot: launches + graduations

**Owner's spec.** One test, one wallet, two kinds at once. Launches: bigboss's own rules (follow,
scorecard >= 61, every launch rule) and its own exit (SELL_RULE=pxgq); the four test rules and the
+30% / 15 s exit only with `SNIPE_TEST_RULES=on` (off). Graduations (`SNIPE_TEST_POOL=on`): curve
from < 60 SOL real to complete within 2 s (bigboss's pump.fun stream, receive times); canonical pool,
SOL quote, not mayhem; at the migration slot's end the pool < 1.5x its opening; buy at once aiming
at migration + 2, 10% price guard; all out 240 s after the fill. Shared: size `SNIPE_TEST_SOL` 0.1
never scaled; caps 4 a UTC hour, 100 a UTC day for both kinds together, 0.5 SOL realized loss a UTC
day stops all buying; one log with `kind` (launch / graduation), graduation rows with the pool's
price at +30/+60/+120/+240/+600 s after the fill; ONE gate (>= 300 live trades, >= 3 days, lower 95%
bound of the mean > 0 after all costs), per-kind numbers as information.

**Graduations (`processors::graduation`).** The PumpSwap program joins the gRPC subscription only
with the switch on. Each curve's last moment under 60 SOL and its completing trade are kept from the
pump.fun stream; a canonical CreatePool (pump's pool authority, index 0; a CPI of `migrate`, carbon
flattens inner instructions) starts a watch of the pool; its state after every trade comes from the
trade events. The decision waits for the stream's first later slot (at most 1.5 s). Graduations
have their own limit of 2 open positions, apart from MAX_OPEN_POSITIONS=1, so neither kind blocks the
other. Paper: the buy at the landing slot's (migration + 2) worst state (highest price), the sell
two slots after the stream's at that slot's worst (lowest price), modeled fees as the launches'.
Live: the bot's services (SendProfile::Buy / Sell), not the hot lanes; built but unused until the
owner's go.

**Builders checked (`src/bin/pool_check.rs`, nothing sent).** Mainnet's PumpSwap trades use the v1
instructions bigboss builds (`buy_exact_quote_in`, `sell`; none of 120 recent transactions used the
reference's v2 `buy_exact_quote_in_v2` / `sell_v2` from pump-rust-client). On real SOL-quoted trades
(found by `research/snipe-test-2026-10-13/find_pool_trades.py`): our buy 26 accounts and our sell 24
match account for account (fee recipients: any of the program's lists), data byte for byte; on the
trade's own pool state our `quote_buy` gives the chain's 2,280,602 tokens and `quote_sell` its
603,529 lamports exactly, once the virtual quote is taken from the trade's event (it moves trade by
trade: a pool read seconds later was 43 lamports off; `instructions::pumpswap_events` reads it, as
the reference's `pump-decode::amm`). The math is the reference's (`exact_in_fees`, `sell_base_input`).
simulateTransaction: our wallet's 0.001 SOL buy OK (125-134k CU); a holder's sell with our builder
OK (84-87k CU).

**Tests:** 241 lib tests (curve completion rule, slot end, worst state of a landing slot, price at a
time, the PumpSwap event against a real sell, the graduation line, caps defaults 4/100); dashboard 24.
Paper set up: DRY_RUN=true, HOT_LIVE=false, STORE_DB=on, SNIPE_TEST=1, SNIPE_TEST_POOL=on,
SNIPE_TEST_RULES=off, BUY_SOL_AMOUNT=0.1 (.env backed up to backups/env.before-snipe-test-2026-10-13).

## 2026-10-13 — The whale rule (WHALE_MODE=1, default off)

**Owner's spec, after pXGQ6J2ziR's 7 days** (985 positions, +456 SOL, 78% winners; his edge is
buying 0.5-3x the curve's SOL into tiny quiet young coins and selling into ~8 follower bots at
+23%). Built as specified, `processors::whale`: a tracker of every young coin from the pump.fun
stream (creation, dev buy, real SOL, price, distinct buyers, last trade slot, net inflow per slot);
at every new slot each coin 60 s - 6.5 min old is checked (real SOL 0.3-5.4, price 1.0-1.4x launch,
<= 71 buyers, no trade for 5 slots, <= 0.1 SOL net in over 10 slots, not mayhem, SOL quote, known
token program, not complete, dev buy < 1 SOL, never held, open < WHALE_MAX_OPEN); follow rule and
scorecard not applied (the follow kind is logged). Size min(WHALE_SIZE_SOL, max(0.5, 0.75 x real
SOL)), never scaled otherwise; 10% slippage against our own quote, no 1.25% guard. Exit
(`auto_sell::TestExit::whale`): all out at +23% over the fill or 90 s, no stop; positions run in the
bot's monitor (live sells: the hot lanes' fan-out, never giving up). Caps 2 an hour, 30 a day,
3 SOL realized loss a day (whale only); the live guard and Start/Stop trading apply. Paper: the
buy lands 2 slots after the decision at that slot's worst state, our own SOL kept in the curve for
every later price (our impact counted). One line a trade: `logs/whale.csv` + table `whale`
(migration 0003): age, real SOL, our SOL and ratio, our impact, price at +5/+20/+60/+214 slots,
others' SOL and wallets behind us within 90 s, the 8 known followers matched by address prefix (the
full list file is not on this server), exit, fees, rent, PnL. `tools/whale_report.py`: per day,
splits by ratio band and follower inflow (information), the gate (>= 100 live trades, >= 3 days,
lower 95% bound > 0). The funnel (coins that passed each rule) goes to the log every 10 min and to
kv 'whale_funnel'.

**Not done (the owner said no):** a study of whether the followers buy behind wallets other than
pXGQ. Paper can't show it; the live run decides.

**Tests:** 248 lib tests (the filter at each edge, the size rule, both exits, the caps, the
landing slot's worst state, what came behind, followers by prefix, the line and its read-back at a
start; allocation audit of the check). Paper set up: WHALE_MODE=1, WHALE_SIZE_SOL=1.5, STORE_DB=on,
DRY_RUN=true, HOT_LIVE=false (.env backed up to backups/env.before-whale-2026-10-13).

## 2026-10-13 (night) — pump.fun's v3 trades and creators moving to fee sharing (fixes)

**Seen:** after the owner pressed Start at 21:27 UTC, 6 launches passed the rules in 18 min; 1 paper
fill, 3 refused by the 1.25% price guard, 2 failed simulation with pump.fun 2006.

**1. buy_v3 (07051dc4f5176550), buy_exact_quote_in_v3 (e1f7501e…), sell_v3 (1c92de77…).** New in
pump.fun's IDL (pump-public-docs `idl/pump.json`): the coin at account 1, the user at 8, two u64
then an optional OptionBool, no creator vault. Dev buys moved to buy_v3 during 13 Oct: the early
path's "no top-level dev buy" skips went from 280 an hour (03:00 UTC) to 953 (21:00) while the
regular stream showed a dev buy in ~90% of creates all day. Those launches were skipped early and
bought (if at all) from the regular stream a slot late: the 3 price-guard refusals (5 other buys,
+24 to +31%, landed first). Both early decoders now read them (`hot::decode::buy_of`/`sell_of`,
`shred_feed::pump_buys_by`): dev buys, bundle buys and their quote, rival bids, bundle10's count;
the position feed's pre-execution insider sells read sell, sell_v2 and sell_v3 (`decode::sell_of`).
The carbon decoder (2.0.0) does not know them either; the regular stream still sees their
TradeEvents (the hooks run on the event's CPI).

**2. A creator's move to fee sharing in the create's bundle.** pump fees' create_fee_sharing_config
(c34e564c6f34fbd5, the coin at account 4) calls pump.fun's migrate_bonding_curve_creator: the
curve's creator becomes the coin's sharing config, PDA ["sharing-config", mint] of
pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ (checked on both coins). Deployer Ce8x… does it in the
transaction right after its create (create+1, before the bundle's buys), so every buy paying the old
creator vault fails (2006). Now: the hot path takes the move from its window (a frame of it, or the
create's own transaction) and its buy pays creator-vault(sharing config); the rules keep the named
creator; the position gets the new creator for its sells. The slow path notes the move from the
early frames (`processors::fee_sharing`) and `apply_stream_view` sets the buy's creator before it
is built (the regular stream's view still wins when it has one). Per frame: a discriminator check
an instruction, the program's key only on a match (+0.02 µs p50 on the tape replay benchmark).

**Tests:** 291 lib tests: the two real transactions as fixtures (`testdata/pump_create_buy_v3.json`,
`testdata/pump_fee_sharing.json`, with their lookup tables), both decoders on v3 trades, the
sharing config on chain values, the slow path's creator, and the hot buy's bytes paying the named
creator's vault without the move and the sharing config's with it.
