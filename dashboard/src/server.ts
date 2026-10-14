// bigboss's dashboard server: listens on 127.0.0.1 only (DASH_PORT, 8790; the bot's own page keeps
// 8787, and neither is reachable from outside); Caddy terminates HTTPS on the server's public
// address. Username + password logins (argon2id), a rate limit on the login, a lockout per source
// address, helmet's headers, a CSRF token on every POST, every control call in the audit log.
//
// It reads the bot's store (data/bigboss.sqlite) read-only and follows the bot's local API
// (src/bot.ts) for its live state. The owner's controls (2026-10-13): trading on/off and the buy
// size, nothing else: no process start or stop, no sells, no paper/live switch.

import Fastify from "fastify";
import cookie from "@fastify/cookie";
import helmet from "@fastify/helmet";
import rateLimit from "@fastify/rate-limit";
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { AuditLog } from "./audit.ts";
import { Auth, SESSION_MS } from "./auth.ts";
import { Bot, linkFromEnv, type BotPosition, type BotState } from "./bot.ts";
import {
  DEFAULT_ROWS,
  DIRS,
  History,
  KINDS,
  MODES,
  OPEN_STATES,
  ROWS_PER_PAGE,
  SHOWS,
  SORTS,
  VENUES,
  VERDICTS,
  WINDOW_HOURS,
  type Books,
  type HistoryRow,
  type Mode,
} from "./history.ts";
import { SolPrices } from "./price.ts";
import { Wallet } from "./wallet.ts";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../..");
const env = (k: string, d: string) => process.env[k] ?? d;
const cfg = {
  host: env("DASH_HOST", "127.0.0.1"),
  port: Number(env("DASH_PORT", "8790")),
  db: resolve(repo, env("DASH_DB", "data/bigboss.sqlite")),
  // The bot's .env: only DASHBOARD_ADDR / DASHBOARD_USER / DASHBOARD_PASSWORD are read from it.
  botEnv: resolve(repo, env("DASH_BOT_ENV", ".env")),
  users: env("DASH_USERS", join(homedir(), ".config/bigboss/dashboard-users.json")),
  audit: resolve(repo, env("DASH_AUDIT", "logs/dashboard-audit.log")),
  // The wallet's balance by its public key (a public RPC: no key, nothing secret in the URL).
  rpc: env("DASH_RPC_URL", "https://api.mainnet-beta.solana.com"),
  // Behind Caddy the cookie must be Secure; DASH_INSECURE=1 only for a local test without HTTPS.
  secure: env("DASH_INSECURE", "") !== "1",
};

const pub = (f: string) => readFileSync(join(here, "../public", f));
const COOKIE = "bb_session";
/** The only controls: trading on, trading off, the buy size. */
const ACTIONS = ["trading-on", "trading-off", "set-size"] as const;
type Action = (typeof ACTIONS)[number];

type Query = Record<string, unknown>;

/** One value of `allowed` (absent or empty = `def`, else the first), or null. */
function choice<T extends string>(v: unknown, allowed: readonly T[], def?: T): T | null {
  if (v === undefined || v === "") return def ?? allowed[0]!;
  return typeof v === "string" && (allowed as readonly string[]).includes(v) ? (v as T) : null;
}

/** `page` (1–9,999,999; past the last page is served as the last) and `rows` (10/25/50/100), or null. */
function paging(q: Query): { page: number; rows: number } | null {
  const { page = "", rows = "" } = q;
  if (typeof page !== "string" || typeof rows !== "string") return null;
  if (page !== "" && !/^[1-9][0-9]{0,6}$/.test(page)) return null;
  const n = rows === "" ? DEFAULT_ROWS : ROWS_PER_PAGE.find((r) => String(r) === rows);
  if (n === undefined) return null;
  return { page: page === "" ? 1 : Number(page), rows: n };
}

/** A run id or "all"/absent (= every run, ""). */
function runOf(v: unknown): string {
  return typeof v === "string" && v !== "all" && /^[A-Za-z0-9._:-]{1,64}$/.test(v) ? v : "";
}

/** A deployer address (base58), absent/"" = every deployer; else null. */
function deployerOf(v: unknown): string | null {
  if (v === undefined || v === "" || v === "all") return "";
  return typeof v === "string" && /^[1-9A-HJ-NP-Za-km-z]{32,44}$/.test(v) ? v : null;
}

function monthOf(v: unknown): string | null {
  if (v === undefined || v === "") return new Date().toISOString().slice(0, 7);
  return typeof v === "string" && /^20[0-9]{2}-(0[1-9]|1[0-2])$/.test(v) ? v : null;
}

/**
 * A SOL amount as typed ("0.0045") in whole lamports (4500000), from its digits: above 0, at most
 * 9 decimals and 6 whole digits; else null.
 */
export function lamportsOf(v: unknown): number | null {
  const m = typeof v === "string" ? /^([0-9]{0,6})(?:\.([0-9]{0,9}))?$/.exec(v) : null;
  if (!m || (!m[1] && !m[2])) return null;
  const n = BigInt(m[1] || "0") * 1000000000n + BigInt((m[2] ?? "").padEnd(9, "0"));
  return n > 0n ? Number(n) : null;
}

/** The bot's mode as the store names it. */
const modeOfBot = (s: BotState | undefined): Mode | null => (s ? (s.mode === "live" ? "live" : "paper") : null);

/** The bot's open position of `mint` in `mode`, if it holds one. */
function liveOf(s: BotState | undefined, mode: string, mint: string): BotPosition | undefined {
  return s?.positions?.find((p) => p.mint === mint && (p.paper ? "paper" : "live") === mode);
}

const LAMPORTS = 1e9;

/**
 * A history row with its money: an open position's value now from the bot (null when the bot
 * doesn't hold it), unrealized = that value − the held tokens' cost, total = realized + unrealized
 * (closed ones: the wallet's change), the share still held, SOL/USD at the buy, last sell and last activity.
 */
function withValues(r: HistoryRow, s: BotState | undefined, usd: (ms: number) => number | null) {
  const open = OPEN_STATES.includes(r.state);
  const live = open ? liveOf(s, r.mode, r.mint) : undefined;
  const value: number | null = open ? (live?.value_sol != null ? Math.round(live.value_sol * LAMPORTS) : null) : 0;
  const unrealized = open && value != null ? value - r.cost_held : null;
  const total = open ? (unrealized == null ? null : r.realized + unrealized) : r.wallet_delta;
  return {
    ...r,
    realized_pct: r.cost_sold > 0 ? r.realized / r.cost_sold : null,
    value,
    unrealized,
    total,
    total_pct: total != null && r.sol_in > 0 ? total / r.sol_in : null,
    position_pct: r.fill_tokens > 0 ? r.tokens_held / r.fill_tokens : open ? null : 0,
    usd_buy: r.bought_ms ? usd(r.bought_ms) : null,
    usd_sell: r.sold_ms ? usd(r.sold_ms) : null,
    usd_last: usd(r.last_active_ms),
  };
}

export interface BuildOpts {
  prices?: SolPrices;
  bot?: Bot;
  wallet?: Wallet;
  history?: History;
}

export async function build(opts: BuildOpts = {}) {
  const prices = opts.prices ?? new SolPrices();
  const history = opts.history ?? new History(cfg.db);
  const bot = opts.bot ?? new Bot(linkFromEnv(cfg.botEnv));
  const wallet = opts.wallet ?? new Wallet(cfg.rpc);
  const auth = new Auth(cfg.users);
  const auditLog = new AuditLog(cfg.audit);
  auditLog.ensure();
  // Caddy on this machine is the only client: trust its X-Forwarded-For for the client address.
  const app = Fastify({ trustProxy: "127.0.0.1", bodyLimit: 16 * 1024, logger: false });
  await app.register(cookie);
  await app.register(helmet, {
    contentSecurityPolicy: {
      directives: {
        defaultSrc: ["'self'"],
        scriptSrc: ["'self'"],
        styleSrc: ["'self'"],
        imgSrc: ["'self'", "data:"],
        connectSrc: ["'self'"],
        frameAncestors: ["'none'"],
      },
    },
  });
  await app.register(rateLimit, { global: false });

  const session = (req: any) => auth.session(req.cookies?.[COOKIE]);
  const botState = () => bot.state()?.state;
  /** The trading filters of a request: mode (default: the bot's, else paper), venue, run, deployer. */
  const booksOf = (q: Query): Books | null => {
    const mode = choice(q.mode, MODES, modeOfBot(botState()) ?? "paper");
    const venue = choice(q.venue, VENUES);
    const deployer = deployerOf(q.deployer);
    if (!mode || !venue || deployer == null) return null;
    return { mode, venue, run: runOf(q.run), deployer };
  };
  const guard = (req: any, reply: any) => {
    if (session(req)) return false;
    reply.code(401).send({ error: "not logged in" });
    return true;
  };

  app.get("/", async (req, reply) => {
    if (!session(req)) return reply.redirect("/login");
    return reply.type("text/html").send(pub("index.html"));
  });
  app.get("/login", async (_req, reply) => reply.type("text/html").send(pub("login.html")));
  app.get("/app.js", async (_req, reply) => reply.type("text/javascript").send(pub("app.js")));
  app.get("/login.js", async (_req, reply) => reply.type("text/javascript").send(pub("login.js")));
  app.get("/style.css", async (_req, reply) => reply.type("text/css").send(pub("style.css")));
  // The logo (top bar, login page, browser tab).
  app.get("/logo.jpg", async (_req, reply) => reply.type("image/jpeg").header("cache-control", "public, max-age=86400").send(pub("logo.jpg")));
  app.get("/favicon.ico", async (_req, reply) => reply.type("image/jpeg").header("cache-control", "public, max-age=86400").send(pub("logo.jpg")));

  app.post(
    "/api/login",
    { config: { rateLimit: { max: 20, timeWindow: "1 minute" } } },
    async (req, reply) => {
      const { username, password } = (req.body ?? {}) as { username?: string; password?: string };
      const ip = req.ip;
      const user = typeof username === "string" ? username.slice(0, 64) : undefined;
      const locked = auth.lockedFor(ip);
      if (locked > 0) {
        auditLog.append({ ip, event: "login_locked", user, action: "login", result: "locked" });
        return reply.code(429).send({ error: `Too many failed logins. Try again in ${Math.ceil(locked / 60)} min.` });
      }
      const s = typeof username === "string" && typeof password === "string" ? await auth.login(username, password, ip) : null;
      if (!s) {
        auditLog.append({ ip, event: "login_failed", user, action: "login", result: "failed" });
        return reply.code(401).send({ error: "Wrong username or password." });
      }
      auditLog.append({ ip, event: "login", user, action: "login", result: "ok" });
      reply.setCookie(COOKIE, s.sid, { path: "/", httpOnly: true, secure: cfg.secure, sameSite: "strict", maxAge: SESSION_MS / 1000 });
      return { ok: true };
    },
  );

  app.post("/api/logout", async (req, reply) => {
    auth.logout(req.cookies?.[COOKIE]);
    reply.clearCookie(COOKIE, { path: "/" });
    return { ok: true };
  });

  app.get("/api/session", async (req, reply) => {
    const s = session(req);
    if (!s) return reply.code(401).send({ error: "not logged in" });
    return { user: s.user, csrf: s.csrf };
  });

  // The status bar, the controls, the open positions and the wallet card (polled every 2 s).
  app.get("/api/status", async (req, reply) => {
    if (guard(req, reply)) return;
    const b = bot.state();
    const s = b?.state;
    const address = s?.wallet ?? wallet.address;
    if (s?.wallet) wallet.address = s.wallet;
    const [w, usd] = await Promise.all([wallet.read(), prices.lookup()]);
    const run = history.currentRun();
    return {
      store: history.available(),
      bot: s
        ? {
            up: true,
            at_ms: b!.at_ms,
            mode: modeOfBot(s),
            trading: s.trading,
            buy_sol: s.buy_sol,
            max_buy_sol: s.max_buy_sol,
            open_positions: s.open_positions,
            max_open_positions: s.max_open_positions,
            uptime_s: s.uptime_secs,
            rules: s.rules ?? [],
            counters: s.counters ?? {},
            shreds: s.shreds ?? null,
            positions: s.positions ?? [],
            // The snipe test: the size is SNIPE_TEST_SOL, read-only here.
            snipe_test: s.snipe_test ?? null,
            // In paper the bot simulates its buys as another funded wallet (DRY_RUN_WALLET).
            simulates_as: s.trading_wallet && s.trading_wallet !== s.wallet ? s.trading_wallet : null,
          }
        : { up: false, error: bot.lastError },
      symbols: history.symbols((s?.positions ?? []).map((p) => p.mint)),
      names: Object.fromEntries((s?.positions ?? []).map((p) => [p.mint, s?.names?.[p.mint]?.symbol]).filter(([, v]) => v)),
      current_run: run,
      latency: history.latency(),
      store_stats: history.storeStats(),
      // The wallet card: the balance read by the public key only, USD as the reference.
      wallet: address
        ? { address, lamports: w?.lamports ?? null, at_ms: w?.at_ms ?? null, usd_per_sol: usd(Date.now()) }
        : null,
    };
  });

  app.get("/api/history", async (req, reply) => {
    if (guard(req, reply)) return;
    const q = req.query as Query;
    const pg = paging(q);
    const show = choice(q.show, SHOWS);
    const sort = choice(q.sort, SORTS);
    const dir = choice(q.dir, DIRS);
    const books = booksOf(q);
    if (!pg || !show || !sort || !dir || !books) return reply.code(400).send({ error: "bad page, rows, show, mode, venue, sort or deployer" });
    const h = history.page(show, books, pg.page, pg.rows, sort, dir);
    const usd = await prices.lookup();
    const s = botState();
    return { ...h, mode: books.mode, rows: h.rows.map((r) => withValues(r, s, usd)), usd_now: usd(Date.now()), summary: history.summary(books) };
  });

  // The calendar: the wallet's change per UTC day of one month (rent locked on its own line), with
  // each day's SOL/USD close for the hover.
  app.get("/api/calendar", async (req, reply) => {
    if (guard(req, reply)) return;
    const q = req.query as Query;
    const books = booksOf(q);
    const month = monthOf(q.month);
    if (!books || !month) return reply.code(400).send({ error: "bad mode, venue, deployer or month" });
    const days = history.calendar(books, month);
    const p = await prices.month(month);
    return { month, mode: books.mode, days, prices: p.prices, price_source: p.source };
  });

  app.get("/api/overview", async (req, reply) => {
    if (guard(req, reply)) return;
    const books = booksOf(req.query as Query);
    if (!books) return reply.code(400).send({ error: "bad mode, venue or deployer" });
    const o = history.overview(books);
    const usd = await prices.lookup();
    const s = botState();
    // Open positions the bot holds are valued at its price; others are counted as unvalued.
    let unrealized: number | null = o.open.length && !s ? null : 0;
    let unvalued = 0;
    if (unrealized != null) {
      for (const p of o.open) {
        const v = liveOf(s, books.mode, p.mint)?.value_sol;
        if (v != null) unrealized += Math.round(v * LAMPORTS) - p.cost_held;
        else unvalued++;
      }
    }
    return {
      run: books.run || "all",
      mode: books.mode,
      venue: books.venue,
      realized: o.realized,
      rent_closed: o.rent_closed,
      unrealized,
      unvalued,
      total: unrealized == null ? null : o.realized + unrealized,
      today: o.today,
      today_rent: o.today_rent,
      today_trades: o.today_trades,
      trades: o.trades,
      wins: o.wins,
      win_rate: o.trades ? o.wins / o.trades : null,
      open: o.open.length,
      usd_now: usd(Date.now()),
    };
  });

  app.get("/api/activity", async (req, reply) => {
    if (guard(req, reply)) return;
    const q = req.query as Query;
    const pg = paging(q);
    const kind = choice(q.kind, KINDS);
    const venue = choice(q.venue, VENUES);
    if (!pg || !kind || !venue) return reply.code(400).send({ error: "bad page, rows, kind or venue" });
    return history.activity(kind, venue, runOf(q.run), pg.page, pg.rows);
  });

  app.get("/api/snipe", async (req, reply) => {
    if (guard(req, reply)) return;
    const q = req.query as Query;
    const pg = paging(q);
    const hours = choice(q.hours, WINDOW_HOURS, "24");
    const venue = choice(q.venue, VENUES);
    const verdict = choice(q.verdict, VERDICTS);
    if (!pg || !hours || !venue || !verdict) return reply.code(400).send({ error: "bad page, rows, hours, venue or verdict" });
    return history.snipe(Number(hours), venue, verdict, pg.page, pg.rows);
  });

  app.get("/api/rejections", async (req, reply) => {
    if (guard(req, reply)) return;
    const q = req.query as Query;
    const hours = choice(q.hours, WINDOW_HOURS, "24");
    const venue = choice(q.venue, VENUES);
    if (!hours || !venue) return reply.code(400).send({ error: "bad hours or venue" });
    return history.rejections(Number(hours), venue);
  });

  app.get("/api/audit", async (req, reply) => {
    if (guard(req, reply)) return;
    const pg = paging(req.query as Query);
    if (!pg) return reply.code(400).send({ error: "bad page or rows" });
    return auditLog.page(pg.page, pg.rows);
  });

  // A history row's side panel: every order of the coin (every sell step), newest first, with SOL/USD at each.
  app.get("/api/position/:id/orders", async (req, reply) => {
    if (guard(req, reply)) return;
    const id = (req.params as { id: string }).id;
    if (!/^[1-9][0-9]{0,17}$/.test(id) || !Number.isSafeInteger(Number(id))) return reply.code(400).send({ error: "bad position id" });
    const d = history.orders(Number(id));
    if (!d) return reply.code(404).send({ error: "no such position" });
    const usd = await prices.lookup();
    return { ...d, orders: d.orders.map((o) => ({ ...o, usd: usd(o.at_ms) })) };
  });

  app.get("/api/runs", async (req, reply) => {
    if (guard(req, reply)) return;
    const mode = choice((req.query as Query).mode, MODES, modeOfBot(botState()) ?? "paper");
    if (!mode) return reply.code(400).send({ error: "bad mode" });
    return { runs: history.runs(), current: history.currentRun(), deployers: history.deployers(mode) };
  });

  // The controls. Every call with a valid session is audited, refused ones included.
  app.post("/api/control/:action", async (req, reply) => {
    const s = session(req);
    if (!s) return reply.code(401).send({ error: "not logged in" });
    const action = (req.params as { action: string }).action.slice(0, 64);
    const refuse = (code: number, error: string, extra: Record<string, unknown> = {}) => {
      auditLog.append({ ip: req.ip, user: s.user, event: "control", action, ...extra, result: error });
      return reply.code(code).send({ ok: false, error });
    };
    if (!auth.checkCsrf(s, req.headers["x-csrf-token"] as string | undefined)) return refuse(403, "bad CSRF token: reload the page");
    if (!(ACTIONS as readonly string[]).includes(action)) return refuse(404, "unknown action");
    const a = action as Action;
    const body = req.body as { sol?: unknown } | undefined;
    let extra: Record<string, unknown> = {};
    let result;
    if (a === "set-size" && botState()?.snipe_test) {
      return refuse(409, "snipe test mode: the size is SNIPE_TEST_SOL in the bot's .env, set by hand");
    }
    if (a === "set-size") {
      const typed = body?.sol === undefined ? undefined : String(body.sol).slice(0, 32);
      const lamports = lamportsOf(body?.sol);
      if (lamports == null) return refuse(400, "a buy size in SOL is needed: above 0, at most 9 decimals", { sol: typed });
      const max = botState()?.max_buy_sol;
      if (max != null && lamports > max * LAMPORTS) return refuse(400, `at most ${max} SOL (the bot's MAX_RUNTIME_BUY_SOL)`, { sol: typed });
      extra = { sol: typed };
      result = await bot.setBuySize(lamports / LAMPORTS);
    } else {
      result = await bot.setTrading(a === "trading-on");
    }
    auditLog.append({ ip: req.ip, user: s.user, event: "control", action, ...extra, result: result.ok ? "ok" : result.error ?? "failed" });
    if (!result.ok) return reply.code(502).send({ ok: false, error: result.error ?? "the bot refused it" });
    return { ok: true, result: result.body };
  });

  app.addHook("onClose", async () => {
    bot.stop();
    history.close();
  });
  return { app, bot };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const { app, bot } = await build();
  bot.start();
  await app.listen({ host: cfg.host, port: cfg.port });
  console.log(`bigboss dashboard on http://${cfg.host}:${cfg.port} (store ${cfg.db})`);
}
