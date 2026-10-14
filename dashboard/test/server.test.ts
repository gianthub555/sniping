// bigboss dashboard: logins, lockout, sessions, CSRF, the two controls (and nothing else), the
// trade history per coin with its sell steps, paper and live never mixed, the venue and deployer
// filters, the calendar's day net and rent line, the overview, Snipe, the funnel, the activity,
// the runs, the status bar, a missing store; against a scratch store built from the bot's own
// migration (src/store/migrations), a fake bot API, fake price APIs and a fake RPC. Nothing here
// touches the running bot, its store or the network.

import { test, after } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { DatabaseSync } from "node:sqlite";

const here = dirname(fileURLToPath(import.meta.url));
const dir = mkdtempSync(join(tmpdir(), "bb-dash-"));
const dbPath = join(dir, "store.sqlite");
process.env.DASH_USERS = join(dir, "users.json");
process.env.DASH_AUDIT = join(dir, "audit.log");
process.env.DASH_DB = dbPath;
process.env.DASH_BOT_ENV = join(dir, "bot.env");
process.env.DASH_INSECURE = "";

const { saveUsers, hashPassword } = await import("../src/auth.ts");
const { build, lamportsOf } = await import("../src/server.ts");
const { Bot, linkFromEnv } = await import("../src/bot.ts");
const { History, stepOf } = await import("../src/history.ts");
const { SolPrices } = await import("../src/price.ts");
const { Wallet } = await import("../src/wallet.ts");

const PASSWORD = "correct-horse-battery-staple";
const BOT_PASSWORD = "bot-secret-never-shown-0123";
saveUsers(process.env.DASH_USERS!, [{ name: "owner", hash: await hashPassword(PASSWORD), role: "admin", created: "2026-10-13" }]);

// ---- The scratch store: the bot's schema, then a few rows.

const NOW = Date.now();
const ns = (ms: number) => BigInt(Math.round(ms)) * 1000000n;
const MIN = 60_000;
const MINT_A = "MintAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAApump";
const MINT_B = "MintBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBpump";
const MINT_L = "MintLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLpump";
const MINT_S = "MintSSSSSSSSSSSSSSSSSSSSSSSSSSSSSSSSSSSSpump";
const DEP_1 = "Dep1111111111111111111111111111111111111111";
const DEP_2 = "Dep2222222222222222222222222222222222222222";
const RENT = 2074080;
{
  const d = new DatabaseSync(dbPath);
  d.exec(readFileSync(resolve(here, "../../src/store/migrations/0001_init.sql"), "utf8"));
  d.exec(readFileSync(resolve(here, "../../src/store/migrations/0002_snipe_test.sql"), "utf8"));
  d.exec("PRAGMA journal_mode = WAL; PRAGMA user_version = 2");
  const run = d.prepare("INSERT INTO runs (run_id, started_ns, stopped_ns, mode, exe, clean, settings) VALUES (?, ?, ?, ?, 'sniper-bot', ?, ?)");
  run.run("run-old", ns(NOW - 3 * 86400_000), ns(NOW - 2 * 86400_000), "paper", 1, '{"DRY_RUN":"true"}');
  run.run("run-now", ns(NOW - 60 * MIN), null, "paper", 0, '{"DRY_RUN":"true","SCORECARD":"true"}');
  const pos = d.prepare(`INSERT INTO positions (id, run_id, mint, mode, state, opened_ns, closed_ns, tokens_held, fill_tokens, sol_in, sol_out,
    wallet_delta_lamports, exit_reason, bought_ns, venue, dex, deployer, follow_kind, tier, rent_locked_lamports)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`);
  const ord = d.prepare(`INSERT INTO orders (client_id, position_id, mint, side, mode, reason, requested, status, t_signal, t_landed,
    quote_net, tokens, wallet_delta_lamports, venue, run_id, sell_step, pnl_percent, error) VALUES (?, ?, ?, ?, ?, ?, 0, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`);
  // A: paper, pump.fun, DEP_1, bought 0.5 SOL, sold in two steps (0.3 + 0.2): closed today.
  const tA = NOW - 30 * MIN;
  pos.run(1, "run-now", MINT_A, "paper", "CLOSED", ns(tA), ns(tA + 20_000), 0, 1000, 500000000, 500000000,
    -2074080 - 10000, "TRAIL", ns(tA + 400), "pumpfun", "pumpfun", DEP_1, "recent", "2", RENT);
  ord.run(11, 1, MINT_A, "buy", "paper", "SNIPE", "landed", ns(tA), ns(tA + 400), 500000000, 1000, -(500000000 + RENT + 5000), "pumpfun", "run-now", null, null, null);
  ord.run(12, 1, MINT_A, "sell", "paper", "SCALE", "landed", ns(tA + 5000), ns(tA + 5000), 300000000, 500, 300000000 - 2500, "pumpfun", "run-now", 1, 20, null);
  ord.run(13, 1, MINT_A, "sell", "paper", "TRAIL", "landed", ns(tA + 20_000), ns(tA + 20_000), 200000000, 500, 200000000 - 2500, "pumpfun", "run-now", 2, -20, null);
  // B: paper, PumpSwap, DEP_2, still open (half sold).
  const tB = NOW - 10 * MIN;
  pos.run(2, "run-now", MINT_B, "paper", "OPEN", ns(tB), null, 500, 1000, 400000000, 250000000, 0, "SCALE", ns(tB + 300), "pumpswap", "pumpswap", DEP_2, "profitable", "1", RENT);
  ord.run(21, 2, MINT_B, "buy", "paper", "SNIPE", "landed", ns(tB), ns(tB + 300), 400000000, 1000, -(400000000 + RENT + 5000), "pumpswap", "run-now", null, null, null);
  ord.run(22, 2, MINT_B, "sell", "paper", "SCALE", "landed", ns(tB + 3000), ns(tB + 3000), 250000000, 500, 250000000, "pumpswap", "run-now", 1, 25, null);
  // L: live, closed yesterday-ish (old run): never shown with paper.
  const tL = NOW - 2.5 * 86400_000;
  pos.run(3, "run-old", MINT_L, "live", "CLOSED", ns(tL), ns(tL + 9000), 0, 1000, 100000000, 150000000, 47900000, "FIRST", ns(tL + 500), "pumpfun", "pumpfun", DEP_1, "recent", "0", RENT);
  ord.run(31, 3, MINT_L, "buy", "live", "SNIPE", "landed", ns(tL), ns(tL + 500), 100000000, 1000, -(100000000 + RENT + 26000), "pumpfun", "run-old", null, null, null);
  ord.run(32, 3, MINT_L, "sell", "live", "FIRST", "landed", ns(tL + 9000), ns(tL + 9000), 150000000, 1000, 150000000 - 26000, "pumpfun", "run-old", 1, 50, null);
  // S: paper, failed buy (only its fee).
  pos.run(4, "run-now", MINT_S, "paper", "BUY_FAILED", ns(NOW - 5 * MIN), ns(NOW - 5 * MIN), 0, 0, 0, 0, -5000, null, null, "pumpfun", "pumpfun", DEP_1, "recent", "2", 0);
  ord.run(41, 4, MINT_S, "buy", "paper", "SNIPE", "failed", ns(NOW - 5 * MIN), null, null, null, -5000, "pumpfun", "run-now", null, null, "lost race: price moved past the 1.25% buy guard");
  d.prepare("INSERT INTO token_names VALUES (?, ?, ?)").run(MINT_A, "AAA", "Coin A");
  // Launches: two followed (bought, skipped low score), one not followed, one paused, one pool.
  const la = d.prepare(`INSERT INTO launches (mint, venue, kind, run_id, seen_ns, slot, source, deployer, creator, dev_buy_lamports, followed,
    follow_kind, score, verdict, skip_reason) VALUES (?, ?, ?, 'run-now', ?, 1, 'early', ?, ?, 1000000000, ?, ?, ?, ?, ?)`);
  la.run(MINT_A, "pumpfun", "launch", ns(tA), DEP_1, DEP_1, 1, "recent", 70, "bought", null);
  la.run("MintK1", "pumpfun", "launch", ns(NOW - 20 * MIN), DEP_1, DEP_1, 1, "recent", 40, "skip", "score 40 under 61 (lowest: x)");
  la.run("MintK2", "pumpfun", "launch", ns(NOW - 15 * MIN), "Dep3", "Dep3", 0, null, null, "skip", "deployer not followed: the reference wallet has not traded it in 41 h");
  la.run("MintK3", "pumpfun", "launch", ns(NOW - 14 * MIN), DEP_2, DEP_2, 1, "profitable", null, "paused", null);
  la.run(MINT_B, "pumpswap", "pool", ns(tB), DEP_2, DEP_2, 1, "profitable", null, "bought", null);
  la.run("MintOld", "pumpfun", "launch", ns(NOW - 3 * 86400_000), DEP_1, DEP_1, 1, "recent", null, "skip", "mayhem-mode launch");
  d.prepare("INSERT INTO deployer_launches VALUES (?, ?, ?, ?)").run(Math.floor(NOW / 1000) - 100, DEP_1, "x", 1);
  const act = d.prepare("INSERT INTO activity (run_id, at_ns, kind, venue, dex, mint, message) VALUES (?, ?, ?, ?, ?, ?, ?)");
  for (let i = 0; i < 30; i++) act.run("run-now", ns(NOW - i * 1000), i % 3 ? "Skipped" : "Detected", "pumpfun", "PumpFun", `Mint${i}`, `Skipped: thing ${i}`);
  act.run("run-now", ns(NOW + 100), "Settings", null, null, null, "Trading started");
  // Hot stamps: 100 signed buys at 1..100 µs from frame to signature.
  const hs = d.prepare("INSERT INTO hot_stamps (rx_ns, t_recv, t_matched, t_signed, window_ns) VALUES (0, ?, ?, ?, 250000)");
  for (let i = 1; i <= 100; i++) hs.run(1_000_000, 1_000_000 + 5_000, 1_000_000 + i * 1000);
  d.prepare("INSERT INTO kv VALUES ('store_stats', ?, ?)").run('{"enqueued":10,"dropped":3,"written":7,"errors":0}', ns(NOW));
  d.prepare(`INSERT INTO route_skips (rx_ns, "time", mint, "followed", slot, reason, class, rtt_ms) VALUES (0, ?, 'm', 'true', 1, 'distance: leader too far', 'BAM', 80)`)
    .run(String(Math.floor(NOW / 1000) - 60));
  d.close();
}

// ---- The fake bot: its event stream and the two control endpoints, behind basic auth.

const botCalls: { path: string; body: any; auth: string | undefined }[] = [];
let botDown = false;
const botState = {
  mode: "dry_run", trading: false, wallet: "Wa11etPubkey1111111111111111111111111111111", trading_wallet: "StandIn1111111111111111111111111111111111111",
  balance_sol: 99, buy_sol: 0.05, max_buy_sol: 1, open_positions: 1, max_open_positions: 1, rules: ["r1"], uptime_secs: 3600, started_at: 0,
  counters: { detected: 5 }, shreds: { status: "ok", incomplete_slots: 2, bad_batches: 0, recovered_shreds: 1 },
  positions: [{ mint: MINT_B, dex: "PumpSwap", paper: true, entry_sol: 0.4, token_amount: 500, value_sol: 0.3, pnl_percent: 37.5, peak_pnl_percent: 40,
    take_profit_armed: true, sold_percent: 50, ladder_step: 1, opened_at: Math.floor(NOW / 1000) - 600, updated_at: 0 }],
  trades: [{ mint: "x" }], activity: [], names: {},
};
const fakeBot = createServer((req: IncomingMessage, res: ServerResponse) => {
  const want = `Basic ${Buffer.from(`admin:${BOT_PASSWORD}`).toString("base64")}`;
  if (botDown) return res.destroy();
  if (req.headers.authorization !== want) {
    res.writeHead(401).end("login required");
    return;
  }
  if (req.url === "/api/events") {
    res.writeHead(200, { "content-type": "text/event-stream" });
    res.write(`event: state\ndata: ${JSON.stringify(botState)}\n\n`);
    return;
  }
  let body = "";
  req.on("data", (c) => (body += c));
  req.on("end", () => {
    const parsed = JSON.parse(body || "{}");
    botCalls.push({ path: req.url!, body: parsed, auth: req.headers.authorization });
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(req.url === "/api/trading" ? { trading: parsed.enabled } : { buy_sol: parsed.sol }));
  });
});
await new Promise<void>((r) => fakeBot.listen(0, "127.0.0.1", () => r()));
const botPort = (fakeBot.address() as { port: number }).port;
writeFileSync(process.env.DASH_BOT_ENV!, `PRIVATE_KEY=never-read\nDASHBOARD_ADDR=127.0.0.1:${botPort}\nDASHBOARD_USER=admin\nDASHBOARD_PASSWORD="${BOT_PASSWORD}"\n`);

const fakePrices = async (url: string) => {
  const day = Math.floor(NOW / 86400_000) * 86400;
  const rows = [[day - 86400, "0", "0", "0", "150.0"], [day, "0", "0", "0", "160.0"]];
  return { ok: true, status: 200, json: async () => (url.includes("kraken") ? { error: [], result: { SOLUSD: rows, last: 0 } } : []) };
};
const rpcCalls: any[] = [];
const fakeRpc = async (_url: string, init: { body: string }) => {
  rpcCalls.push(JSON.parse(init.body));
  return { ok: true, status: 200, json: async () => ({ result: { value: 2_500_000_000 } }) };
};

const bot = new Bot(linkFromEnv(process.env.DASH_BOT_ENV!));
bot.start();
const { app } = await build({ prices: new SolPrices(fakePrices as any), bot, wallet: new Wallet("https://rpc.invalid", fakeRpc as any) });
after(async () => {
  await app.close();
  fakeBot.closeAllConnections();
  fakeBot.close();
});
for (let i = 0; i < 50 && !bot.state(); i++) await new Promise((r) => setTimeout(r, 20));

async function login(ip = "1.1.1.1", password = PASSWORD) {
  return app.inject({ method: "POST", url: "/api/login", payload: { username: "owner", password }, remoteAddress: ip });
}
let cookie = "";
let csrf = "";
{
  const r = await login();
  cookie = String(r.headers["set-cookie"]).split(";")[0]!;
  csrf = (await app.inject({ url: "/api/session", headers: { cookie } })).json().csrf;
}
const get = async (url: string) => {
  const r = await app.inject({ url, headers: { cookie } });
  return { code: r.statusCode, body: r.json() };
};
const control = (action: string, payload: unknown = {}, token = csrf) =>
  app.inject({ method: "POST", url: `/api/control/${action}`, headers: { cookie, "x-csrf-token": token }, payload: payload as any });
const auditText = () => readFileSync(process.env.DASH_AUDIT!, "utf8");

test("nothing without a login", async () => {
  assert.equal((await app.inject({ url: "/" })).statusCode, 302);
  for (const url of ["/api/status", "/api/history", "/api/calendar", "/api/overview", "/api/snipe", "/api/activity", "/api/rejections",
    "/api/runs", "/api/audit", "/api/position/1/orders"]) {
    assert.equal((await app.inject({ url })).statusCode, 401, url);
  }
  assert.equal((await app.inject({ method: "POST", url: "/api/control/trading-on" })).statusCode, 401);
  assert.equal((await app.inject({ url: "/login" })).statusCode, 200);
  // The logo is public (the login page shows it) and is an image.
  const logo = await app.inject({ url: "/logo.jpg" });
  assert.equal(logo.statusCode, 200);
  assert.equal(logo.headers["content-type"], "image/jpeg");
  assert.ok(logo.rawPayload.length > 1000);
});

test("the session cookie is HttpOnly, Secure and SameSite=Strict; helmet's CSP is on", async () => {
  const r = await login("1.1.1.2");
  const c = String(r.headers["set-cookie"]);
  assert.match(c, /HttpOnly/);
  assert.match(c, /Secure/);
  assert.match(c, /SameSite=Strict/);
  const page = await app.inject({ url: "/", headers: { cookie } });
  assert.match(String(page.headers["content-security-policy"]), /default-src 'self'/);
  assert.equal(page.headers["x-frame-options"], "SAMEORIGIN");
});

test("five failed logins lock that address for 15 minutes, not others", async () => {
  for (let i = 0; i < 5; i++) assert.equal((await login("9.9.9.9", "wrong-password-xxxxxxx")).statusCode, 401);
  assert.equal((await login("9.9.9.9")).statusCode, 429);
  assert.equal((await login("8.8.8.8")).statusCode, 200);
  assert.match(auditText(), /"result":"locked"/);
});

test("a control without the CSRF token is refused and audited", async () => {
  const r = await control("trading-on", {}, "bad-token");
  assert.equal(r.statusCode, 403);
  assert.equal(botCalls.length, 0);
  assert.match(auditText(), /"action":"trading-on".*bad CSRF/);
});

test("only trading on/off and the buy size exist: no start, stop or sells", async () => {
  for (const a of ["start", "stop", "sell", "sell-all", "bot_stop", "mode", "live"]) {
    const r = await control(a, { mint: MINT_B });
    assert.equal(r.statusCode, 404, a);
  }
  assert.equal(botCalls.length, 0);
});

test("trading on and off reach the bot with its login, and are audited", async () => {
  assert.equal((await control("trading-on")).statusCode, 200);
  assert.equal((await control("trading-off")).statusCode, 200);
  assert.deepEqual(botCalls.map((c) => [c.path, c.body.enabled]), [["/api/trading", true], ["/api/trading", false]]);
  assert.equal(botCalls[0]!.auth, `Basic ${Buffer.from(`admin:${BOT_PASSWORD}`).toString("base64")}`);
  assert.match(auditText(), /"action":"trading-on","result":"ok"/);
  assert.match(auditText(), /"action":"trading-off","result":"ok"/);
  botCalls.length = 0;
});

test("the buy size: digits only, above 0, at most the bot's maximum", async () => {
  for (const bad of ["", "abc", "-1", "0", "0.0000000001", "1e3", " 1"]) {
    assert.equal((await control("set-size", { sol: bad })).statusCode, 400, bad);
  }
  assert.equal((await control("set-size", { sol: "2" })).statusCode, 400, "over max_buy_sol 1");
  assert.equal(botCalls.length, 0);
  assert.equal((await control("set-size", { sol: "0.075" })).statusCode, 200);
  assert.deepEqual(botCalls.pop(), { path: "/api/buy-size", body: { sol: 0.075 }, auth: botCalls[0]?.auth ?? `Basic ${Buffer.from(`admin:${BOT_PASSWORD}`).toString("base64")}` });
  assert.match(auditText(), /"action":"set-size","sol":"0.075","result":"ok"/);
  assert.equal(lamportsOf("0.000000001"), 1);
  assert.equal(lamportsOf("123456.5"), 123456500000000);
});

test("snipe test mode: the size is read-only, the test's state is shown, its skips are funnel steps", async () => {
  (botState as any).snipe_test = { sol: 0.1, max_per_day: 50, max_per_hour: 2, max_loss_sol: 0.5, today: 3, this_hour: 1, pnl_today_sol: -0.012 };
  bot.take(JSON.stringify(botState));
  const before = botCalls.length;
  const r = await control("set-size", { sol: "0.2" });
  assert.equal(r.statusCode, 409);
  assert.match(r.json().error, /SNIPE_TEST_SOL/);
  assert.equal(botCalls.length, before, "never sent to the bot");
  assert.match(auditText(), /"action":"set-size","result":"snipe test mode/);
  const s = (await get("/api/status")).body;
  assert.equal(s.bot.snipe_test.today, 3);
  assert.equal((await control("trading-off")).statusCode, 200, "trading on/off still works");
  botCalls.length = 0;
  delete (botState as any).snipe_test;
  bot.take(JSON.stringify(botState));
  assert.equal(stepOf("skip", "snipe test: 2+ buyers besides the dev in the bundle"), "test: 2+ bundle buyers");
  assert.equal(stepOf("skip", "snipe test: the hour's snipe cap is reached"), "test: hour cap");
  assert.equal(stepOf("skip", "snipe test: 1+ SOL real in the curve after the bundle"), "test: 1+ SOL real");
  assert.equal(stepOf("skip", "graduation: 3.4 s from under 60 SOL to complete (more than 2 s)"), "grad: slow completion");
  assert.equal(stepOf("skip", "graduation: the insiders' jump came in the migration slot (x1.83 of the opening)"), "grad: jump in migration slot");
  assert.equal(stepOf("skip", "graduation: its curve was never seen under 60 SOL"), "grad: not seen under 60 SOL");
});

test("no secret reaches a response or the audit log", async () => {
  const all = [await get("/api/status"), await get("/api/runs"), await get("/api/audit?rows=100")].map((x) => JSON.stringify(x.body)).join("");
  assert.ok(!all.includes(BOT_PASSWORD));
  assert.ok(!auditText().includes(BOT_PASSWORD));
  assert.ok(!auditText().includes(PASSWORD));
  assert.ok(!all.includes("never-read"));
});

test("the status bar: bot, mode, latencies, gaps, store drops; the wallet card by its public key", async () => {
  const { code, body: s } = await get("/api/status");
  assert.equal(code, 200);
  assert.equal(s.bot.up, true);
  assert.equal(s.bot.mode, "paper");
  assert.equal(s.bot.trading, false);
  assert.equal(s.bot.shreds.incomplete_slots, 2);
  assert.equal(s.bot.simulates_as, botState.trading_wallet);
  assert.equal(s.bot.positions.length, 1);
  assert.equal(s.latency.snipe.n, 100);
  assert.equal(s.latency.snipe.p99, 99);
  assert.equal(s.latency.feed.p50, 5);
  assert.equal(s.store_stats.dropped, 3);
  assert.equal(s.current_run.run_id, "run-now");
  assert.equal(s.wallet.address, botState.wallet);
  assert.equal(s.wallet.lamports, 2_500_000_000);
  assert.equal(s.wallet.usd_per_sol, 160);
  // The balance of the bot's own key, never the paper stand-in's.
  assert.deepEqual(rpcCalls.at(-1).params[0], botState.wallet);
  assert.equal(rpcCalls.at(-1).method, "getBalance");
});

test("paper and live are never mixed; 'all' is not a mode", async () => {
  const p = await get("/api/history?mode=paper&run=all");
  assert.deepEqual(p.body.rows.map((r: any) => r.mode).sort(), ["paper", "paper", "paper"]);
  const l = await get("/api/history?mode=live&run=all");
  assert.deepEqual(l.body.rows.map((r: any) => r.mint), [MINT_L]);
  assert.equal((await get("/api/history?mode=all")).code, 400);
  assert.equal((await get("/api/calendar?mode=all")).code, 400);
  // Without a mode: the bot's (paper).
  assert.equal((await get("/api/history?run=all")).body.mode, "paper");
});

test("a coin sold in steps is one row; the side panel lists every step and the rent", async () => {
  const h = await get("/api/history?mode=paper&run=all");
  const a = h.body.rows.find((r: any) => r.mint === MINT_A);
  assert.equal(a.symbol, "AAA");
  assert.equal(a.state, "CLOSED");
  assert.equal(a.buys, 1);
  assert.equal(a.sells, 2);
  assert.equal(a.realized, 0);
  assert.equal(a.total, -2074080 - 10000);
  assert.equal(a.rent_locked, RENT);
  const o = await get(`/api/position/${a.id}/orders`);
  assert.deepEqual(o.body.orders.map((x: any) => [x.side, x.sell_step, x.reason]), [["sell", 2, "TRAIL"], ["sell", 1, "SCALE"], ["buy", null, "SNIPE"]]);
  const buy = o.body.orders.at(-1);
  assert.equal(buy.rent, RENT);
  assert.equal(buy.fees, 5000);
  assert.equal(o.body.orders[0].fees, 2500);
  assert.equal(o.body.position.deployer, DEP_1);
  assert.equal(o.body.position.score, 70);
  assert.equal((await get("/api/position/999/orders")).code, 404);
  assert.equal((await get("/api/position/abc/orders")).code, 400);
});

test("an open coin is valued by the bot; failed buys show under 'failed'", async () => {
  const h = await get("/api/history?mode=paper&run=all&show=open");
  assert.equal(h.body.rows.length, 1);
  const b = h.body.rows[0];
  assert.equal(b.value, 300000000);
  assert.equal(b.unrealized, 300000000 - 200000000);
  assert.equal(h.body.hidden_failed, 1);
  const f = await get("/api/history?mode=paper&run=all&show=failed");
  assert.deepEqual(f.body.rows.map((r: any) => r.mint), [MINT_S]);
});

test("venue, run and deployer filters", async () => {
  const swap = await get("/api/history?mode=paper&run=all&venue=pumpswap");
  assert.deepEqual(swap.body.rows.map((r: any) => r.mint), [MINT_B]);
  const dep = await get(`/api/history?mode=paper&run=all&deployer=${DEP_2}`);
  assert.deepEqual(dep.body.rows.map((r: any) => r.mint), [MINT_B]);
  assert.equal((await get("/api/history?mode=paper&deployer=not-base58!")).code, 400);
  assert.equal((await get("/api/history?mode=paper&venue=raydium")).code, 400);
  const old = await get("/api/history?mode=paper&run=run-old");
  assert.equal(old.body.total, 0);
});

test("the calendar's day net is the wallet's change, the rent on its own line", async () => {
  const month = new Date(NOW).toISOString().slice(0, 7);
  const c = await get(`/api/calendar?mode=paper&run=all&month=${month}`);
  const today = new Date(NOW).toISOString().slice(0, 10);
  const sameDay = (ms: number) => new Date(ms).toISOString().slice(0, 10) === today;
  const d = c.body.days.find((x: any) => x.day === today);
  // Every paper order today (the times above all fall today unless the test runs just after 00:00 UTC).
  if (sameDay(NOW - 30 * MIN)) {
    const want = -(500000000 + RENT + 5000) + 300000000 - 2500 + 200000000 - 2500 - (400000000 + RENT + 5000) + 250000000 - 5000;
    assert.equal(d.net_lamports, want);
    assert.equal(d.rent_lamports, 2 * RENT);
    assert.equal(d.trades, 1);
    assert.equal(d.failed, 1);
  }
  assert.equal(c.body.prices[today], 160);
  assert.equal((await get("/api/calendar?mode=paper&month=2026-13")).code, 400);
});

test("the overview: realized, today, rent today, unrealized from the bot", async () => {
  const o = (await get("/api/overview?mode=paper&run=all")).body;
  assert.equal(o.trades, 1);
  assert.equal(o.realized, -2074080 - 10000);
  assert.equal(o.unrealized, 100000000);
  assert.equal(o.open, 1);
  if (new Date(NOW - 30 * MIN).toISOString().slice(0, 10) === new Date(NOW).toISOString().slice(0, 10)) assert.equal(o.today_rent, 2 * RENT);
  const live = (await get("/api/overview?mode=live&run=all")).body;
  assert.equal(live.realized, 47900000);
  assert.equal(live.unrealized, 0);
});

test("Snipe: seen by venue and kind, followed verdicts and skip reasons, deployers", async () => {
  const s = (await get("/api/snipe?hours=24")).body;
  const seen = Object.fromEntries(s.stats.seen.map((r: any) => [`${r.venue}/${r.kind}`, r.n]));
  assert.deepEqual(seen, { "pumpfun/launch": 4, "pumpswap/pool": 1 });
  assert.deepEqual(s.stats.verdicts, { bought: 2, skip: 1, paused: 1 });
  assert.deepEqual(s.stats.skip_steps, [["low score", 1]]);
  const d1 = s.stats.deployers.find((d: any) => d.deployer === DEP_1);
  assert.equal(d1.launches, 2);
  assert.equal(d1.record_no_pop, 1);
  assert.equal(s.total, 4);
  assert.equal(s.stats.route_skips[0].n, 1);
  assert.equal(s.stats.bundle.window_us_p50, 250);
  assert.equal((await get("/api/snipe?hours=24&verdict=paused")).body.total, 1);
  assert.equal((await get("/api/snipe?hours=24&venue=pumpswap")).body.total, 1);
  assert.equal((await get("/api/snipe?hours=168")).body.stats.skip_steps.length, 2);
  assert.equal((await get("/api/snipe?hours=5")).code, 400);
});

test("the rejection funnel: one step per launch, per hour, samples by venue", async () => {
  const r = (await get("/api/rejections?hours=24")).body;
  assert.equal(r.coins, 5);
  assert.deepEqual(r.totals, { "buy decided": 2, "low score": 1, "not followed": 1, paused: 1 });
  assert.deepEqual(r.steps, ["paused", "not followed", "low score", "buy decided"]);
  assert.equal(r.samples["paused"][0].mint, "MintK3");
  const swap = (await get("/api/rejections?hours=24&venue=pumpswap")).body;
  assert.equal(swap.coins, 1);
  assert.equal(stepOf("skip", "block producer in AS, outside LEADER_REGIONS=EU"), "region (LEADER_REGIONS)");
  assert.equal(stepOf("skip", "distance (leader too far)"), "distance");
  assert.equal(stepOf("skip", "BAM leader (route: no buy)"), "route");
  assert.equal(stepOf("skip", "something new"), "other");
  assert.equal(stepOf("seen", null), "no verdict");
});

test("live activity is paged newest first and filters by kind", async () => {
  const a = (await get("/api/activity?rows=10")).body;
  assert.equal(a.total, 31);
  assert.equal(a.rows.length, 10);
  assert.equal(a.rows[0].kind, "Settings");
  assert.equal((await get("/api/activity?kind=Detected")).body.total, 10);
  assert.equal((await get("/api/activity?page=99&rows=10")).body.page, 4);
  assert.equal((await get("/api/activity?kind=Nope")).code, 400);
});

test("runs: every start with its switches, positions and net", async () => {
  const r = (await get("/api/runs")).body;
  assert.deepEqual(r.runs.map((x: any) => x.run_id), ["run-now", "run-old"]);
  assert.equal(r.runs[0].settings.SCORECARD, "true");
  assert.equal(r.runs[0].positions, 3);
  assert.equal(r.runs[1].net, 47900000);
  assert.equal(r.current.run_id, "run-now");
  assert.deepEqual(r.deployers.map((d: any) => d.deployer).sort(), [DEP_1, DEP_2].sort());
});

test("the store is opened read-only", () => {
  const h = new History(dbPath);
  assert.throws(() => h.db()!.exec("INSERT INTO kv VALUES ('x', 'y', 1)"), /readonly/i);
  h.close();
});

test("a missing store: empty answers, no errors", async () => {
  const h = new History(join(dir, "nothing-here.sqlite"));
  assert.equal(h.available(), false);
  assert.equal(h.page("all", { mode: "paper", venue: "all", run: "", deployer: "" }, 1, 25).available, false);
  assert.deepEqual(h.calendar({ mode: "paper", venue: "all", run: "", deployer: "" }, "2026-10"), []);
  assert.equal(h.snipe(24, "all", "all", 1, 25).stats, null);
  assert.equal(h.rejections(24, "all").available, false);
  assert.deepEqual(h.runs(), []);
  assert.deepEqual(h.latency(), { snipe: null, feed: null });
});

test("the bot link reads only its three keys from .env", () => {
  const l = linkFromEnv(process.env.DASH_BOT_ENV!);
  assert.deepEqual(l, { url: `http://127.0.0.1:${botPort}`, user: "admin", password: BOT_PASSWORD });
  const none = linkFromEnv(join(dir, "missing.env"));
  assert.equal(none.url, "http://127.0.0.1:8787");
  assert.equal(none.password, "");
  // The trade log rows of the bot's own page are dropped from the kept state.
  assert.equal((bot.state()!.state as any).trades, undefined);
});

test("the bot not answering: controls fail cleanly and are audited", async () => {
  botDown = true;
  const r = await control("trading-on");
  assert.equal(r.statusCode, 502);
  assert.equal(r.json().ok, false);
  assert.match(auditText(), /"action":"trading-on","result":"the bot is not reachable"/);
  botDown = false;
  // A stale state counts as down.
  const b = new Bot({ url: "http://127.0.0.1:1", user: "a", password: "b" }, { staleMs: 10 });
  b.take(JSON.stringify(botState), Date.now() - 1000);
  assert.equal(b.state(), null);
});
