# Solana Sniper Bot (Rust)

Watches pump.fun launches in real time over Yellowstone gRPC and trades them with rules measured on a
reference wallet (`8RvtT8…`, 813 pump.fun tokens from 8 to 23 September 2026, 86% won): it buys the
launches of deployers that wallet trades, at the launch, and sells within seconds on a clock that
reacts to the buyers landing behind the fill. The rules are code (`src/config/rules.rs`), not
settings. The processors for PumpSwap and Raydium (AMM v4, CPMM, CLMM, LaunchLab) remain in the code
but are not traded.

Built on [carbon](https://github.com/sevenlabs-hq/carbon) 2.0 decoders and the Solana 4.x client
crates. Every instruction layout was checked against the live programs on 2026-09-17 (see
[Verification](#verification)); the programs change often, so re-run the checks before trading.

> Trading new tokens is extremely risky. Most launches go to zero, and fees, tips and price impact
> make a round trip cost several percent before the price moves at all. Start with `DRY_RUN=true`.

## How it works

1. **Stream.** One gRPC subscription at `processed` commitment, filtered to the enabled DEX programs.
   Startup fails fast if the endpoint rejects the token.
2. **Detect.** Carbon decodes every instruction, including CPIs. Each processor reacts to a pool
   creation and reads the pool state from the same transaction's events where possible (pump.fun
   dev buy, PumpSwap boost, LaunchLab trade), so no RPC round trip happens before the buy.
3. **Entry rules.** A pump.fun launch is bought only when its deployer or coin creator is followed:
   the reference wallet traded it within the last 41 h, or it made that wallet 0.77 SOL or more over
   its history. The launch must also have a dev buy of 0.7-20 SOL, a deployer that held 2+ SOL
   before the create and whose wallet is older than 24 h, and no mayhem mode. A deployer is skipped
   while half or more of its recent launches (2 or more, the last 10 within 7 days) got no buyers:
   every followed deployer's launch is watched for its first 3 s and judged as the exit would have
   sold a buy right behind the bundle (NO_POP or not), in `logs/deployer_launches.csv`. A launch is
   also skipped when the same deployer launched within the last 5 minutes. These are the 28 Sep
   rules, restored on 7 Oct (see `DECISIONS.md`). Every skip appears in the dashboard's live feed
   with its reason.
4. **Buy.** At the launch (at most one slot late), the full stake when the dev buy is 1 SOL or more
   and 65% of it below that, with the reference wallet's 1.25% price guard (8% from 29 Sep to 7 Oct). The transaction is
   `[compute budget, tip, wrap SOL, create token account, swap, unwrap]`, signed with a cached
   blockhash and sent through Jito / Nozomi / 0slot or plain RPC. If the service rejects the request
   (for example a rate limit) the same signed transaction goes to RPC.
5. **Sell.** Every trade on the curve is recorded from before the buy is sent. The first 30% is sold
   once 1.1 SOL of other wallets' buys land behind the fill (or the price is 16% over it); if others
   put 6+ SOL into the curve in the buy's slot and the next 4, everything is sold. With no sell 2 s
   after the buy, the position is sold whole when the price is under +5%. After that, 33% of the
   rest is sold 1 s after each sell lands (2 s after the first), waiting a second (up to 4 times)
   while the coin is running: the price within 2% of its high since the buy, or 1+ SOL bought or
   1+ SOL sold by others in the last second. A drop does not delay the next sell; there is no
   trailing stop. When a sell comes due with the price still under the last sell and nobody has
   bought for 2 s, everything is sold at once instead of stepping down.
   Everything is sold when 10% or less is left, or after 40 s. Launches that fit the fast-rug profile
   (launch-tool metadata or a 3-10 SOL dev buy) are tagged in the log and on the dashboard.

Every live buy and sell is appended to `auto_sell_logs.csv` and every paper (dry-run) one to
`auto_sell_logs_paper.csv`: the two histories never mix. Every pump.fun launch seen is appended to
`creator_history.csv`.

## Quick start

```bash
cargo build --release
cp .env.example .env        # fill in RPC_ENDPOINT, LOCAL_GRPC_URL, GRPC_TOKEN, PRIVATE_KEY
cargo run --release         # DRY_RUN=true in .env.example: simulates, never sends
```

Requirements: a Yellowstone gRPC endpoint with a token (for example Helius LaserStream,
`https://laserstream-mainnet-<region>.helius-rpc.com`, with your Helius API key as `GRPC_TOKEN`) and
an RPC endpoint that allows `simulateTransaction` and `getMultipleAccounts` at a reasonable rate.

### Running in the background

`bot.sh` runs the release build in the background and keeps a PID file and a log:

```bash
./bot.sh build     # compile the release binary
./bot.sh start     # build if needed, start, and wait until startup checks pass
./bot.sh stop      # graceful stop (Ctrl-C signal), forced after 15 s
./bot.sh restart   # rebuild first, then stop and start (a failed build leaves the running bot alone)
./bot.sh status    # running or not, and dry run or live
./bot.sh logs      # follow logs/bot.log
./bot.sh buy-size 0.01   # set SOL per buy (live if running, saved to .env)
./bot.sh compare 2026-09-26T00:00   # replay every buy since then over the token's real trades
```

Open positions are tracked in memory: stopping or restarting the bot stops auto-selling them. Tokens
stay in the wallet, so sell them manually or let them run before restarting a live bot.

### Dry run

With `DRY_RUN=true` every buy is simulated against current chain state and logged with the compute
units it used; successful simulations open *paper* positions so the auto-sell rules run too. The
gRPC stream runs ahead of RPC nodes, so dry-run simulations ask the RPC for at least the creation
slot (`minContextSlot`) before simulating.
`DRY_RUN_WALLET=<pubkey>` simulates as another funded wallet, so you can test before funding the bot.

Paper trades fill where live ones would. The simulation checks that the transaction is valid, but
it reads RPC state about 190 ms after the launch, while a live buy from Frankfurt lands after
32–269 ms (median 53 ms, `FRANKFURT_LANDING_MS` in `src/config/trade_setting.rs`, from the live
probes). So each paper buy draws one of those landing delays and fills on the stream's curve at
that moment: the buy guard is checked there, buys the stream shows after that moment count as
behind the fill (the FIRST 30% slice), and the exit clock starts there. Paper sells draw a delay
the same way. With `DRY_RUN_CHARGE_FEES=true`, buys and sells that would fail on chain (the buy
guard, a creator switch, the sell slippage) book the fee and priority fee a live one pays; the tip
reverts with the transaction.

### Live against paper

`./bot.sh compare [SINCE]` checks the bot's own buys (default: the last 24 h). `tools/fetch_tapes.py`
fetches each bought token's first two minutes of trades into `logs/tapes/`, and
`rules_check play` replays the exit from the curve state each buy filled at. It prints, per buy, the
replayed result next to the bot's (quoted, and the wallet's change with fees and tips), then totals
for live and paper separately. The gap between the bot and the replay on paper is the baseline;
live running further below it measures what paper cannot see, mainly buyers who skip a coin
because our own buy moved its price.

### Hot path and the two live switches

`FAST_PATH=true` hands the early feeds to the hot path (`src/hot`): pinned threads decode each
create, decide, fill a pre-built buy and sign it in about 50 us, then fire the same bytes on every
send lane: the staked gateways at once (Helius Sender SWQoS-only, Everstake, AllenHark Relay,
Triton Jet), then 0slot and Nozomi, then Jito, then direct QUIC to the leader as the last fallback.
The shreds come from the bot's own shred engine (`ENGINE_INPROC`, `src/hot/shreds`, ported from
pumpfeed): the ShredStream proxy's output and a capture of its input, reassembled with Reed-Solomon
recovery in-process, no other process on the path. Real buys from it need **two** switches:

| `DRY_RUN` | `HOT_LIVE` | What happens |
|---|---|---|
| `true` | any | Nothing is sent. With `HOT_DRY_LANES=true`, unlandable copies (all-zero blockhash) of paper buys and of one other launch every `HOT_DRY_SAMPLE_MS` go out on the lanes to time them; they cannot land and cost nothing. |
| `false` | `false` (default) | The hot path stays off; the slow path buys as before. |
| `false` | `true` | The hot path sends real buys. It is the only buyer of the launches it decides; the slow path buys only those it left over (it needed an RPC answer), so no launch is bought twice. |

`HOT_LIVE` is a deliberate second switch: turning the slow path live (`DRY_RUN=false`) never makes
the hot path send by itself. For a small live test, `LIVE_SLOW_PATH_BUYS=false` makes the hot path
the only buyer, `LANE_ROTATE=true` sends each buy through one lane in turn (paying only its tip, so
the landing lane is known), and the `LIVE_*` limits (`utils::live_guard`) stop buying for good
after a number of buys, a run of failed ones, a low wallet or a silent feed, while open positions
are still sold. The priority fee comes from the fee engine (`FEE_MODE`, `src/hot/fee.rs`): `off` keeps
the lanes' default bid with the 10% outbid; `table` reads a per leader-client, per launch-tier table a
side task keeps from the market's recent bids (`hot::bids`), our own positions and caps; `ladder` fixes
one of `FEE_LADDER_SOL` per launch for a fee test. In table mode the route follows the slot
leader's client (`FEE_ROUTE_<CLASS>`): a Jito bundle alone for JitoLabs leaders (behind a relayer
or not), one gateway copy for BAM, Firedancer, Frankendancer and Agave, no buy on Harmonic and
unknown clients. On top of it the distance rule (`hot::distance`) buys only when the slot's leader
answers our QUIC handshake under 20 ms, measured every 5 minutes, and skips a late create in a
leader's last slot before a far one (`logs/route_skips.csv`, `logs/distance_daily.csv`). The lane
threads run on a core of their own (`HOT_LANE_CORE`; `deploy/cpu-isolation.sh` isolates cores 4-7
at boot). The buy's compute limit is measured
(`hot::cu`, `logs/cu_calibration.json`). Timing goes to `logs/hot_stats.log` and `logs/hot_stamps.csv`, lane
answers to `logs/hot_lane_answers.csv` (every lane, with the time to its answer or ack), block positions to `logs/positions.csv` (a daily table in
`logs/positions_daily.md`), and the hot/slow cross-check to `logs/hot_verify.csv`. Settings:
`.env.example`, decisions and measurements: `DECISIONS.md`.

### Lane race

Which send lane lands first, measured with real transactions (ticket 5.1). Every round sends each
enabled lane its own copy of a 1-lamport transfer to self on one durable nonce, each tipping only its
lane, so at most one copy lands per round:

```bash
./target/release/lane_race                                # dry: the plan and the most it can cost
./target/release/lane_race --create-nonce --yes           # a nonce account for the wallet in .env
./target/release/lane_race --nonce <ADDRESS> --yes        # 1,000 rounds, one every 1.6 s
```

Per lane: land rate (misses and failures in the denominator), median, p90 and p99 slot delay,
same-slot share, SOL spent; also written to `logs/lane_race_<unix>.md`.

### Dashboard

While the bot runs it serves a dashboard at **http://127.0.0.1:8787**: realized PnL, win rate, buys,
a cumulative PnL chart, a live activity feed (detections, skips, buys, sells), open positions with
live PnL, and the full trade history, filterable by time range and DEX. It updates in real time: the
bot pushes changes to the page as they happen (server-sent events). Open positions are re-valued
on every trade on their curve and at least every 100 ms.

**Stake.** Change the stake per buy without restarting, from the dashboard's *Buy size* box
or with `./bot.sh buy-size 0.01` (`./bot.sh buy-size` shows the current value). The change applies
to the next buy, is saved to `BUY_SOL_AMOUNT` in `.env`, and does not affect open positions. Values
above 100 SOL are refused; edit `.env` directly for that. When the bot is stopped, the command just
updates `.env`.

- **VS Code Remote:** open the *Ports* panel; port 8787 is forwarded automatically, then open
  `http://localhost:8787` in your browser.
- **Plain SSH:** `ssh -L 8787:127.0.0.1:8787 <user>@<server>`, then open `http://localhost:8787`.

History comes from `auto_sell_logs.csv` (live) and `auto_sell_logs_paper.csv` (paper), so it
survives restarts; the *History* filter shows one of them at a time (the bot's mode by default), and
every figure on the page is for that one. For live trades the bot records
each landed transaction's real wallet SOL change (fees, tips and rent included), and realized PnL
uses those; dry-run trades are shown as estimates from quotes. The dashboard has no password, so
keep `DASHBOARD_ADDR` on `127.0.0.1` (the default) and reach it through a tunnel. The API only
answers requests whose host and origin are this machine, so other websites open in your browser
cannot read it or change the buy size.

### Measure latency

`./bot.sh latency [SECONDS] [--land-test N]` listens to the gRPC stream (default 120 s, next to a
running bot) and prints the launch-to-buy chain stage by stage: how long after a slot starts a
launch reaches you, the Helius-to-server transport delay, the bot's own reaction time (from the
buys in `auto_sell_logs.csv`), the round trip to the landing service and RPC, and how fast other
wallets buy the same launches. It sends nothing unless you add `--land-test N`, which sends N memo
transactions from `PRIVATE_KEY`'s wallet through `CONFIRM_SERVICE` (each pays the service tip, e.g.
0.001 SOL for Nozomi, plus fees) and times how long they take to land.

`probe --via nozomi|zero_slot|jito|helius|all [--count 6] [--slots slots.json]` measures where a
buy would land: on each pump.fun create seen on the gRPC stream it sends a small memo transaction
through one landing path (that path's tip, the buy's priority fee per compute unit, a 20k CU
limit) and reports its slot and block position relative to the create. Each probe costs the tip
plus about 0.0001 SOL. On 24-25 Sep 2026 from Ashburn: Nozomi landed fastest (median 113 ms after
the create), 0slot and Helius Sender slower, Jito 0.4-1 s; with a nearby leader Nozomi landed
47 ms after the create, 5 of 6 in the create's slot. 66% of leader slots were in Europe (mostly
Frankfurt), 5.5% near Ashburn: the server's location decides the buy time more than any setting.

Each buy also records its own timing in `auto_sell_logs.csv` (`launch_slot`, `ready_slot`,
`landed_slot`, `detect_to_ready_ms`, `launch_to_landed_ms`) and in the dashboard's Speed column.
Landing is only measured for real (live) buys.

### Check the integrations yourself

Two tools use the same code as the bot and need no funds:

```bash
# Quote and simulate a buy and a sell on a live pool, comparing the simulated balance
# changes with the quotes. Pass pump.fun mints, pool addresses for the others.
cargo run --release --bin verify -- pumpswap <pool> --buyer <funded wallet> --seller <token holder>

# Push real recent creations through the processors and buy flow over plain RPC
# (DRY_RUN forced on). Useful without a gRPC token.
cargo run --release --bin replay -- pumpfun 5
cargo run --release --bin replay -- raydium_launchpad --tx <signature>
cargo run --release --bin replay -- raydium_clmm 3
```

## Rules

**Sell: the slicing exit in the table below, again since 7 Oct** (the 28 Sep rules). From 29 Sep
to 7 Oct the bot used the patient exit (`rules::PATIENT_EXIT`): everything at +50% over the
entry; from 30 s after the buy, everything once the price is under the entry; everything at 60 s. No
slices, no NO_POP check. Replay of the launches the entry keeps (25-29 Sep, the bot's real landing
positions, 8% guard, 0.1 SOL): +15.5% a fill against +12.3% for the reference wallet's exit below,
held-out days +15.5% against +11.2%, but bigger losing trades (worst tenth -21% or worse against
-6%). `PATIENT_EXIT = Some(PATIENT_SETTINGS)` brings it back.

All trading rules are constants in `src/config/rules.rs`, each with the measurement behind it; the
startup summary and the dashboard list them. `.env` cannot change them. The reference wallet's
history per deployer and coin creator is in `reference_deployers.txt` (`unix key lamports_delta`,
seeded from the study); every trade of that wallet the stream shows is appended, so the followed
list stays current across restarts. Without the file only deployers the wallet trades from now on
are followed.

| Rule | Value | Measured on the reference wallet |
|---|---|---|
| Follow a deployer | traded in the last 41 h, or made it 0.77+ SOL | 19% of such launches attempted, against 1.25% overall |
| Dev buy | 0.7-20 SOL | 92% of attempts at 0.7+ SOL, 99% at 20 or less |
| Deployer before the create | 2+ SOL, wallet older than 24 h | 89% and 97% of attempts |
| Mayhem mode | never | never bought |
| No-buyers record | skip a deployer while 50%+ of its last 10 launches (2+, 7 days) would have been sold NO_POP by a buy right behind the bundle | replay of the 1,005 launches attempted 25-27 Sep at realistic fills (next slot, 1.25% guard): the 94 skipped were 50 of 277 fills, 80% NO_POP, -1.24 SOL; the rest +26.0 SOL, NO_POP 32% -> 21%. The bot wins most easily where nobody else wants the coin |
| All-caps names | off (since 1 Oct); from 29 Sep: skip a launch whose name is in capitals only ("PEPE 2.0", "$WIF") | the owner's rule (29 Sep): of 4,594 followed launches, 1.2% of all-caps ones were alive at 30 min (3.1% overall) and 57% dead within 5 min (41%); replay on the creator-filtered launches: next slot +20.0 SOL without them against +16.0, held-out days +7.8 against +4.7 |
| Bundle size | off since 7 Oct (the 28 Sep rules); from 29 Sep: skip a launch with more than 2 wallets (1 on 7 Oct) in the create's bundle besides the dev (seen on the preprocessed stream) | the owner's rule (29 Sep). It costs on the 40 s trades: replay, the 189 kept launches with 3+ bundle wallets made +0.048 SOL an attempt against +0.044 for the rest (+9.1 of +35.5 SOL); held longer they fall more (5 min after NO_POP: -34% to -40% against -11% with no bundle). `MAX_BUNDLE_WALLETS = None` turns it off |
| Creator pace | skip a launch within 5 min of the same deployer's previous launch (the 28 Sep rule, again since 7 Oct; 10-20 min and a daily cap from 29 Sep) | the bot cannot win the launch-slot race (live 26-29 Sep: 33 of 259 buys filled). Replay of 4,348 followed launches (25-29 Sep) at the bot's real landing positions: the kept 17% made +4.5% an attempt against +2.9% under the old 5-min rule; at the top of the next slot +12.2 SOL against -6.8; held out (28-29 Sep) +1.3 against -17.0. Bundle features (size, SOL, share, tips, payer) did not hold out |
| Buy timing | at most 1 slot after the launch | 87% in the launch slot, 13% one slot later |
| Bet | stake; 65% of it on dev buys under 1 SOL | 2.93 SOL, 1.9 SOL on small dev buys |
| Buy price guard | 1.25% (8% from 29 Sep to 7 Oct) | 8%: the owner's call; replay of the launches the entry keeps: fills 43% of attempts against 34%, +13.8% a fill against +16.7%, +5.9% an attempt against +5.6%; the added fills are behind snipers, the kind that lost on paper on 25 Sep. 1.25% was the reference wallet's cap: his buys cap the cost at 1.0125x. A "one fast buyer" guard (at most 3 SOL ahead, `BUY_AHEAD_MAX_SOL`) was tried on 25 Sep: on paper its fills behind snipers went 1 win, 11 losses (-1.37 SOL), so it is off |
| First sell | 30% at 1.1 SOL behind the fill or +16% | 82% / 75% right on held-out days |
| Check at 2 s | all out under +5%, or with 1+ SOL behind under +30% (1.5 s, under the entry unless it touched +3%, from 29 Sep to 7 Oct) | NO_POP line moved from +3% to the entry on 29 Sep: after a NO_POP sell the median coin kept falling (-25% at 5 min) but a third rose 30%+, mostly ones that had popped before the check; replay on the launches the entry keeps: +28.5 SOL at real landings against +26.4, next slot +7.4 against +6.3. Earlier: | 75% / 57-67% sold everything. A 4 s check with an insider exit (all out once the dev or a bundle wallet sells) was run on paper 26-27 Sep and removed: on its 82 paper fills the replay gave +1.94 SOL against +3.35 with these rules |
| Steps | 33% (25% on 5+ SOL bets) 1 s after each sell lands, 2 s after the first | 71% of his later sells within 0.3 s of a whole second |
| First sell waits for 6 SOL | the first sell waits through the buy's slot and the next 4; if others put 6+ SOL into the curve in them (buys less sells), sell everything, else the usual first sell after them (4 SOL from 29 Sep to 7 Oct) | 4 SOL, 1.5 s and +3% since 29 Sep: on the launches the creator rules keep, +4.52% an attempt against +4.33%, top of the next slot +1.46% against +0.50%. Earlier, 6 SOL was the owner's rule; replay: bot's own entries +67.2 -> +77.8 SOL, his 749 positions +348.2 -> +301.8 SOL, 25 Sep paper buys +0.16 -> +0.58 SOL (SQQQ -0.08 -> +0.33) |
| Falling, no buyers | a due sell under the last sell's price with no buys for 2 s sells everything | replay on the bot's fills: better than stepping in 7 of 8 cases (e.g. +5.7% to +6.7% a trade on 302 affected launches); selling everything on any drop was worse |
| Wait | price within 2% of its high, or 1+ SOL bought or 1+ SOL sold by others in the last second; no wait on drops; max 4 (he holds about one tick while rising; 4 replayed best) | at his 1 s tick he sells 49% with no buys, 12-22% after 1+ SOL bought, 16% at the high vs 32-35% under it; no trailing stop (same 15-19% a slot at the high and 35% under) |
| Close-out | 10% or less left; 40 s | his longest hold was 42 s |
| Fast-rug profile | tagged only | dev dumps within 10 s on 73% of such launches; a 4 s cap cut the replay result from +356 to +326 SOL |
| Safety (not measured) | 1 open position, 0.01 SOL reserve, 20% sell price guard | |

## Configuration (`.env`)

| Variable | Meaning |
|---|---|
| `RPC_ENDPOINT` | RPC for blockhashes, account reads, simulation, confirmation and plain sends |
| `LOCAL_GRPC_URL`, `GRPC_TOKEN` | Yellowstone gRPC endpoint and token |
| `PRIVATE_KEY`, `PUBKEY` | Base58 or JSON byte array (64-byte keypair or 32-byte secret); `PUBKEY` optional, must match |
| `DRY_RUN`, `DRY_RUN_WALLET` | Simulate instead of sending; optionally as another funded wallet |
| `DRY_RUN_BUY_LATENCY_MS`, `DRY_RUN_SELL_LATENCY_MS`, `DRY_RUN_CHARGE_FEES` | When paper trades land after sending: unset draws from the Frankfurt probe delays, a number or comma list sets them, `0` fills at once; book modeled fees |
| `BUY_SOL_AMOUNT` | Stake per buy in SOL (the rules scale it by the dev buy) |
| `CU`, `PRIORITY_FEE_MICRO_LAMPORT`, `SELL_PRIORITY_FEE_MICRO_LAMPORT` | Compute unit limit (buys and sells) and price. pump.fun buys use a median 94k CU and up to 115k (260 simulated buys to 26 Sep 2026; 20k of it creates the token account; 6 live buys used 85k–96k), sells 51k–68k, so `CU=120000` fits both (105000 would fail about 6% of buys); the priority fee is charged on the whole limit, and block producers rank by fee over the requested limit, so a tight limit ranks higher for the same SOL |
| `NOZOMI_BUNDLE`, `BUY_THIRD_PARTY_FEE` | `NOZOMI_BUNDLE=true`: every buy goes two ways at once, as a one-transaction Nozomi bundle (`/api/sendBundle`, block builders Jito and Harmonic, all-or-nothing: a failing buy is dropped free) and as a plain Nozomi send of the same transaction (direct-to-leader paths; with `PRIORITY_FEE_MICRO_LAMPORT=0` a failing copy costs the 5,000-lamport base fee). It lands at most once, on whichever path arrives first. The plain copy uses Nozomi API v2 (`/api/sendTransaction2`, a base64 body, no JSON: their lowest-latency path over a warm connection). `JITO_COPY=true` (with `JITO_UUID` and `JITO_BUNDLE_ONLY=true`) adds a third copy as a Jito bundle for the Jito tip on landed buys; paired probes on 26 Sep 2026 put Jito ahead on 1 of 5 launches and Nozomi on 3. Paired probes on 26 Sep 2026: the two paths landed within 3 block positions of each other (a bundle once slipped two slots), and neither a 0.02 SOL tip nor a priority fee moved the position, only arrival time does (live on 26 Sep 2026, 30 lost races on plain sends with a 0.004 SOL priority fee cost 0.12 SOL). Nozomi orders by the tip and bids it with the builders ("start with 100% of your bid in the Nozomi tip"), so put the bid in `BUY_THIRD_PARTY_FEE` (buys only, paid only when the buy lands) and set `PRIORITY_FEE_MICRO_LAMPORT=0`. A buy not seen within 3 s is logged "not included: bundle dropped, no fee" (a late landing is still sold). Each bundle goes to the two fastest Nozomi regions at once (Nozomi does not forward bundles between regions; the same signed transaction lands at most once). Sells are unchanged. The bot no longer re-sends through Nozomi (Nozomi retries itself; its docs say client re-sends lower your priority) |
| `NONCE_FANOUT`, `FANOUT_SERVICES` | `NONCE_FANOUT=true`: every buy goes out as one transaction per service of `FANOUT_SERVICES` (default `NOZOMI,JITO,HELIUS,ZERO_SLOT`), each tipping only its own service (AUTO_TIP: that service's market tip) and all advancing one durable nonce account, so the first copy to land is the buy and the others are void (no fee). A landed buy pays one tip instead of every service's. Takes precedence over `NOZOMI_BUNDLE`/`JITO_COPY` for buys; sells are unchanged. Needs nonce accounts: `cargo run --release --bin nonce -- create 4 --yes` (0.00106 SOL rent each, back with `nonce close --yes`; `nonce list` shows them), listed in `nonce_accounts.txt`. Head-to-head probes on 8 launches (30 Sep 2026, `probe --vias nozomi_both,fanout`): one copy landed every time, Nozomi's 6 times and Jito's 2, within 2-5 block positions of the old path: it saves the second tip but does not make buys faster. 0slot rejected every copy (API key no longer valid) |
| `TRADING_HOURS_UTC` | Buy only inside this daily window, e.g. `01:00-07:00` (UTC; may wrap past midnight); open positions are always sold. Competition varies by hour: a fast sniper was in the launch slot on 42-48% of followed launches 21:00-01:00 US Eastern but 67-84% at 07:00-09:00 (26 Sep 2026), and the live test at 01:30 ET won 6 of 16 races against 2 of 32 at 08:00 ET |
| `DRY_RUN_FILL_BY_POSITION` | Paper buys land at a block position drawn from 47 real landings from this server (launch slot +70 to +1036, or the next slot; 38% next slot) and are priced from the trades that landed before that position; "behind the fill" counts by position too (live as well, from the stream's transaction index). The old time model (a landing delay after sending, checked against the stream's latest state) filled 42% of races where live filled 8-17%, because snipers at positions 50-180 beat a buy at 200-700 whatever the wall clock said; and the coins it filled that live cannot win were the better ones (paper +10% a trade on launches with a fast sniper against +7% without). A replay of the 78 attempts of 26 Sep 11:51-14:50 at position 140-200: 71-79% lost to the guard, fills won 17-25%, +6% to +13% of the stake. Paper sells land the same way: a position from the same draw, counted from where the stream is when the sell is decided, priced after the trades that landed ahead of it (48 live sells of 26 Sep: 12 had other trades land between the decision and the sell, the price moving -7% to +13%). `false` restores the time model for both |
| `LEADER_REGIONS` | Buy only launches whose block producer is in these continents (e.g. `EU`; empty: all). The epoch's leader schedule comes from RPC, each producer's location from its gossip address via ip-api.com (cached in `logs/validator_regions.json`); a launch in its producer's last slot also needs the next producer nearby; unknown producers pass. From Frankfurt, with another sniper racing, we were first on 36% of launches produced in Europe, 11% in the US and 20% in Asia (76 real landings, 25-26 Sep 2026). Use it on Nozomi with a higher bid; with Jito bundles lost races cost nothing |
| `LOADED_DATA_LIMIT_MIB` | Account data a buy may load (buys only; unset = 64 MiB). Block producers rank by fee over cost, and the cost includes 8 CU per 32 KiB of this limit: 16,384 CU at 64 MiB, 4,096 at 16. A pump.fun buy loads about 12.6 MiB, mostly program code (pump.fun's program data is 10 MiB), so `16` leaves room. With `CU=120000` it cuts a buy's cost from ~145k to ~128k: ~14% higher rank at the same fee. Paper mode simulates with it, so a limit that is too small shows up there first |
| `CONFIRM_SERVICE`, `CONFIRM_SERVICES`, `SELL_CONFIRM_SERVICES` | `JITO`, `NOZOMI`, `ZERO_SLOT`, `HELIUS` or empty for RPC; a list sends to all at once |
| `JITO_TIP_VALUE`, `THIRD_PARTY_FEE`, `HELIUS_SENDER_TIP_SOL` | Tips in SOL (Nozomi / 0slot need 0.001+) |
| `JITO_BUNDLE_ONLY` | `true`: Jito sends each transaction as a one-transaction bundle (`bundleOnly=true`). A bundle that would fail is never included, so a lost race costs nothing (no fee, no tip); Jito orders bundles by tip per compute unit, and runs on 98.9% of stake (26 Sep). With `CONFIRM_SERVICES=JITO` every buy is no-fail: a dropped bundle is logged "not included" after 8 s (checked again at 15 s and 60 s in case it landed late). Keep Nozomi/RPC for sells. **Not working as of 26 Sep 2026:** Jito accepted but dropped every bundle sent without an auth key (0 of 30 landed, `getInflightBundleStatuses` "Invalid" within 150 ms, in Frankfurt, Amsterdam and global, whatever the bundle format; plain Jito sends landed). Jito support: bundles are hard to land without an auth key, set `JITO_UUID`. `probe --vias jito` with `JITO_BUNDLE_ONLY=true` prints each bundle id and its status |
| `JITO_BLOCK_ENGINE_URL`, `NOZOMI_API_URL` / `_KEY`, `ZERO_SLOT_API_URL` / `_KEY`, `HELIUS_SENDER_URL` | Service endpoints and keys |
| `JITO_UUID` | Jito auth key, sent as `x-jito-auth` on every Jito send (and by `probe`). Requested through a Jito support ticket (discord.gg/jito, "Searcher Support"); Jito support said on 26 Sep 2026 that bundles are hard to land without one |
| `BIRDEYE_API_KEY` | Optional fallback valuation |
| `DASHBOARD_ENABLED`, `DASHBOARD_ADDR`, `DASHBOARD_USER`, `DASHBOARD_PASSWORD` | Local dashboard |
| `HELIUS_PREPROCESSED` | `true`: read Helius's preprocessed stream (pump.fun transactions before they execute, key taken from `RPC_ENDPOINT`; or set `PREPROCESSED_WS_URL`). With `SHRED_SNIPE=true` launches are bought from it. Measured from Frankfurt: a median 4-5 ms ahead of LaserStream (p90 10 ms); about 224 transactions a second, roughly 58M credits a month. Followed deployers' balances are kept live over gRPC so the 2+ SOL check needs no RPC call. A launch is quoted after the buys that reach the stream within 0.5 ms of its create (its dev bundle: a median 0.03-0.09 ms), timed on the OS clock because Tokio's 1 ms sleep took 2.3 ms; the 5-minute report counts buys that came later. Accounts a transaction takes from address lookup tables are resolved from a cache (fetched over RPC the first time a launch uses a table, remembered in `logs/lookup_tables.txt`); before that, launches whose wrapped SOL came from a table (27% of snipes on 26 Sep 2026) fell back to the regular stream. Takes precedence over the options below |
| `SHRED_UDP_PORT` | Deshredded entries pushed to this server over UDP, one datagram per batch: the slot (u64 LE) then a bincode `Vec<Entry>` (up to 65 KB; AllenHark's Direct UDP delivery, measured 26 Sep 2026). Register the server IP with the provider and open the port. Used when no `SHRED_PROXY_URL`; `HELIUS_PREPROCESSED` takes precedence |
| `SHRED_PROXY_URL` | Optional local Jito ShredStream proxy entries service (`--grpc-service-port`); takes precedence over `SHRED_GRPC_URL` |
| `SHRED_GRPC_URL`, `SHRED_TOKEN`, `SHRED_SNIPE` | Optional decoded shred stream provider; `SHRED_SNIPE=true` buys from shred sightings |
| `FAST_PATH`, `HOT_LIVE` | The hot path on; `HOT_LIVE=true` is also needed for it to send real buys (with `DRY_RUN=false`). See "Hot path and the two live switches" |
| `LANE_<NAME>_ENABLED`, `_URL`, `_KEY`, `_TIP_LAMPORTS`, `LANE_SET`, `LANE_DEFAULT` | The hot path's send lanes (HELIUS, EVERSTAKE, ALLENHARK, TRITON, ZEROSLOT, NOZOMI, JITO, QUIC): on/off, endpoint, key, tip. `LANE_SET=race` uses every enabled lane, `default` those in `LANE_DEFAULT`. `QUIC_IDENTITY_KEYPAIR`: the direct QUIC lane's identity (empty: unstaked) |
| `BUY_PRIORITY_FEE_SOL`, `BUY_FEE_CAP_SOL`, `HOT_OUTBID_BPS` | Hot buys: priority fee with no rival seen (0.0007 SOL at the CU limit), cap on priority plus tips (0.005 SOL), bid over the highest rival on the coin (1000 = 10%) |
| `ENGINE_INPROC`, `ENGINE_SHRED_BIND`, `ENGINE_CAPTURE_IFACE`, `ENGINE_CAPTURE_PORTS` | The in-process shred engine (default on): where the proxy sends shreds, and the capture of its input port. `false`: pumpfeed's UDP push on `SHRED_UDP_PORT` (this release only) |
| `HOT_DRY_LANES`, `HOT_DRY_SAMPLE_MS`, `HOT_RESEND_BEFORE_CREATE` | DRY_RUN lane timing with unlandable copies; resend once a buy that failed before the create (off) |

Numbers accept `_` separators (`200_000`).

**Parallel sending.** With `CONFIRM_SERVICES=NOZOMI,ZERO_SLOT,JITO` the bot builds *one*
transaction that carries every listed service's tip and sends that same signed transaction to
all of them at the same time. Whichever service reaches a leader first lands it; the other
copies have the identical signature, so the network drops them and the buy can never execute
twice. Each send that fails (rate limit, bad key) is logged and the others continue; if all of
them fail the transaction still goes out through `RPC_ENDPOINT`. The cost per buy is the sum of
the tips in that transaction, paid whether or not the buy lands, so keep the list to the services
you actually have keys for.

**Re-sending.** A live buy is not sent once and forgotten: the same signed transaction is
offered to the services again every 100 ms for up to 1.5 s, until the gRPC stream shows the
bot's own transaction on-chain. Every copy carries the same signature, so only one can execute
and the extra sends cost nothing beyond the request. DRY_RUN simulates instead of sending, so
this path only runs live.

## DEX coverage

| DEX | Detected on | Buy | Sell | State for the first quote |
|---|---|---|---|---|
| pump.fun | `create_v2`, `create` | `buy_exact_sol_in` (18 accounts) | `sell` (16) | CreateEvent + dev-buy TradeEvent |
| PumpSwap | `create_pool` (graduations; SPL-token user pools) | `buy_exact_quote_in` (26/27) | `sell` (24/26) | CreatePoolEvent + InitBoostEvent |
| Raydium AMM v4 | `initialize2` | `swap_base_in_v2` (8) | `swap_base_in_v2` | Initial amounts (waits for open time) |
| Raydium CPMM | `initialize`, `initialize_with_permission` | `swap_base_input` (13) | `swap_base_input` | RPC (pools open one second after creation) |
| Raydium CLMM | `create_pool` / `create_customizable_pool` with a position opened in the same transaction | `swap_v2` + bitmap extension + tick arrays | `swap_v2` | RPC |
| Raydium LaunchLab | `initialize_v2`, `initialize_with_token_2022`, `initialize` | `buy_exact_in` (18) | `sell_exact_in` (18) | Dev-buy TradeEvent, else RPC |

Only SOL-quoted pools are traded. LaunchLab is limited to constant-product curves (the only curve
whose math could be checked on chain). Token-2022 transfer fees are included in quotes.

CLMM quotes use `src/instructions/raydium_clmm_math.rs`, a line-by-line port of the program's swap
math (tick math, tick crossing, bitmap extension, dynamic fees, limit orders) from raydium-clmm
commit `ed7c84a`. Replaying 614 consecutive mainnet swaps on 13 pools reproduced every input, output,
fee and end state exactly; limit-order fills were never hit on chain and are covered only by the
program's own test vectors.

## Verification

On 2026-09-17 `verify` quoted and simulated trades on live mainnet pools; the simulated balance
change equalled the quote to the unit in every case below:

| DEX | Buys | Sells |
|---|---|---|
| pump.fun (incl. mayhem-mode coins) | 5 | 1 |
| PumpSwap (boosted graduation pools) | 2 | 1 |
| Raydium AMM v4 | 5 | 2 |
| Raydium CPMM | 3 | 2 |
| Raydium CLMM (incl. a Token-2022 transfer-fee token) | 4 | 2 |
| Raydium LaunchLab (SPL and Token-2022 bases) | 2 | 1 |

A 3.5-minute live dry run on Helius LaserStream (all six DEXes, 1% slippage) processed ~90,000
transactions with nothing queued; 20 of 29 launches that passed the filters produced successful
simulated buys, the rest failed on slippage (plus one creator reassignment, recovered by the retry).

`replay` pushed real creation transactions through the processors: pump.fun, PumpSwap, CPMM, CLMM
and LaunchLab creations produced successful simulated buys; the failures were pools whose price had
moved past the slippage limit since creation. The live gRPC path itself was not exercised (no
gRPC token was available), only its startup check.

## Project layout

```
src/
├── main.rs                  startup checks, gRPC pipeline
├── config/                  rules.rs (every trading rule), env parsing, wallet, RPC/HTTP clients
├── processors/              one carbon processor per DEX + shared snipe flow (common.rs)
├── instructions/            per-DEX market state, quotes and buy/sell instructions; types.rs = Market,
│                            raydium_clmm_math.rs = ported CLMM swap math
├── service/                 Jito / Nozomi / 0slot senders, tip accounts, region latency probes
├── utils/                   auto-sell, blockhash + epoch cache, tx building, CSV log, parsing, gRPC check,
│                            creator_history.rs (pump.fun deployer history), reference.rs (followed deployers),
│                            dashboard.rs + dashboard.html (local web dashboard)
└── bin/verify.rs, replay.rs verification tools; latency.rs, probe.rs latency and landing
    measurement; rules_check.rs rule replays on recorded data
```

## Limitations

- Speed depends on your gRPC and RPC providers and their distance from the leader; there is no
  shred-level detection or bundle co-location.
- CPMM, CLMM and some LaunchLab snipes read pool state over RPC first; a lagging RPC delays them.
- Some pump.fun launchpads reassign a coin's creator to a fee-sharing config in a second
  transaction in the launch slot. Buys built from launch data then fail with `ConstraintSeeds`
  (2006). Such a buy is not sent again: the bot buys each launch at most once. Sells always
  re-read the creator.
- Fee tiers, fee recipients and account lists are admin-controlled and have changed several times
  this year. GlobalConfig / FeeConfig are re-read every 5 minutes, but a program upgrade that adds
  accounts will break trades until the code is updated. Re-run `verify` after upgrades.
- The bot follows the reference wallet's deployers, not its buys: it does not copy that wallet's
  trades, and it cannot be faster than it on the same launch.
