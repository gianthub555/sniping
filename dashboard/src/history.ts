// bigboss's store (data/bigboss.sqlite, src/store/migrations/0001_init.sql), read-only: the trade
// history (one row per coin per mode per run, its every order in the side panel), the calendar
// (the wallet's change per UTC day, the token-account rent locked as its own line), the overview,
// the Snipe page (launches and graduations seen, what the followed deployers' launches became, the
// deployers' records, the fee engine's classes, the distance and region skips, the bundle window),
// the live activity, the rejection funnel and the runs.
//
// Paper and live are never mixed: every trading query takes one mode. Every trading row carries a
// venue ('pumpfun' | 'pumpswap'); "all" shows both. Times are nanoseconds in the store and leave
// SQL as milliseconds (nanoseconds exceed JavaScript's safe integers); ns bounds go in as BigInt.
//
// The store may not exist yet (the bot creates it at its first start with STORE_DB=on): every
// method then answers empty with `available: false`, and the file is looked for again on the next
// request.

import { existsSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";

export const MODES = ["paper", "live"] as const;
export type Mode = (typeof MODES)[number];
export const VENUES = ["all", "pumpfun", "pumpswap"] as const;
export type Show = "all" | "open" | "closed" | "failed";
export const SHOWS: readonly Show[] = ["all", "open", "closed", "failed"];
export const ROWS_PER_PAGE = [10, 25, 50, 100] as const;
export const DEFAULT_ROWS = 25;
export const SORTS = ["last_active", "realized", "total", "balance", "bought", "sold"] as const;
export const DIRS = ["desc", "asc"] as const;
export type Sort = (typeof SORTS)[number];
export const OPEN_STATES = ["OPEN", "SELL_PENDING", "BUY_PENDING", "STUCK"];
/** The activity kinds the bot writes (`ActivityKind`, Debug-formatted). */
export const KINDS = ["all", "Detected", "Skipped", "Bought", "BuyFailed", "Sold", "SellFailed", "Settings"] as const;
/** Hours the Snipe and Rejections pages can show, counted back from now. */
export const WINDOW_HOURS = ["1", "6", "24", "48", "168"] as const;
/** The Snipe page's launch table: every followed launch, or one verdict. */
export const VERDICTS = ["all", "bought", "buy", "buy_failed", "skip", "paused", "seen"] as const;

/** Pump.fun's supply: 1,000,000,000 tokens of 6 decimals. */
const SUPPLY = 1e15;
const DAY_MS = 86400000;
const NS = 1000000n;

export interface Page<T> {
  rows: T[];
  total: number;
  page: number;
  rows_per_page: number;
}

/** The page actually served: `page` clamped to the last one (page 1 when nothing matches). */
export function clampPage(total: number, page: number, rows: number): { page: number; offset: number } {
  const last = Math.max(1, Math.ceil(total / rows));
  const p = Math.min(Math.max(page, 1), last);
  return { page: p, offset: (p - 1) * rows };
}

const emptyPage = <T>(rows: number) => ({ rows: [] as T[], total: 0, page: 1, rows_per_page: rows, available: false });

const quantile = (v: number[], q: number): number | null => {
  if (!v.length) return null;
  const s = [...v].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor((s.length - 1) * q + 0.5))]!;
};

/** A ms time as ns for a bound (BigInt: ns exceed safe integers). */
const nsOf = (ms: number) => BigInt(Math.floor(ms)) * NS;

export interface HistoryRow {
  id: number;
  mint: string;
  symbol: string | null;
  mode: string;
  venue: string;
  state: string;
  run_id: string;
  opened_ms: number;
  bought_ms: number | null;
  closed_ms: number | null;
  sold_ms: number | null;
  last_active_ms: number;
  sol_in: number;
  sol_out: number;
  wallet_delta: number;
  rent_locked: number;
  exit_reason: string | null;
  tokens_held: number;
  fill_tokens: number;
  realized: number;
  cost_sold: number;
  cost_held: number;
  buys: number;
  sells: number;
  /** Landed sell steps of the coin (a position sold in several steps counts each). */
  positions: number;
  deployer: string | null;
  follow_kind: string | null;
  tier: string | null;
  ready_ms: number | null;
  launch_to_landed_ms: number | null;
  mc_buy: number | null;
  mc_sell: number | null;
}

export interface OrderRow {
  client_id: number;
  position_id: number;
  side: string;
  reason: string | null;
  status: string;
  sig: string | null;
  at_ms: number;
  quote_net: number | null;
  tokens: number | null;
  wallet_delta: number | null;
  sell_step: number | null;
  pnl_percent: number | null;
  venue: string;
  error: string | null;
  landed_slot: number | null;
  signal_slot: number | null;
  detect_to_ready_ms: number | null;
  launch_to_landed_ms: number | null;
  /** Fees the order paid: a buy's wallet change beyond its SOL and the rent; a sell's SOL beyond its wallet change. */
  fees: number | null;
  /** A buy's token-account rent (locked: the account is never closed). */
  rent: number | null;
  mc: number | null;
}

export interface CalendarDay {
  day: string;
  /** The wallet's change that day: every order's wallet_delta_lamports (landed and failed). */
  net_lamports: number;
  /** Of that, the token-account rent the day's buys locked (never refunded: accounts are not closed). */
  rent_lamports: number;
  /** Positions closed that day, the winners, and their wallet change split in profit and loss. */
  trades: number;
  wins: number;
  profit_lamports: number;
  loss_lamports: number;
  buys: number;
  sells: number;
  failed: number;
  buy_volume_lamports: number;
  sell_volume_lamports: number;
}

/** Trading filters shared by the history, the calendar and the overview. */
export interface Books {
  mode: Mode;
  venue: string;
  run: string;
  deployer: string;
}

/** Skip reasons (launches.skip_reason, the bot's texts) as funnel steps, in the bot's order. */
export const FUNNEL_STEPS: readonly [string, RegExp | null][] = [
  ["paused", null],
  ["regular stream only", /reached only the regular stream/],
  ["outside trading hours", /outside trading hours/],
  ["venue not traded", /is not traded/],
  ["non-standard curve", /non-standard bonding curve/],
  ["not followed", /deployer not followed/],
  ["mayhem", /mayhem/],
  ["name in capitals", /capitals only/],
  ["reference lost", /reference wallet lost/],
  ["dev buy outside range", /^dev buy .* outside/],
  ["relaunch", /launched another coin/],
  ["too many launches", /coins in the last 24 h/],
  ["big bundle", /wallets in the create's bundle/],
  ["dump risk", /^dump risk/],
  ["no-buyers record", /got no buyers/],
  ["low score", /^score -?\d+ under/],
  ["region (LEADER_REGIONS)", /LEADER_REGIONS/],
  ["route", /\(route: no buy\)/],
  ["distance", /^distance \(/],
  ["deployer balance", /deployer held|deployer balance/],
  ["wallet age", /deployer wallet is only/],
  ["too late", /slots past the launch/],
  ["positions full", /positions already open/],
  ["test: dev buy 1+ SOL", /^snipe test: dev buy/],
  ["test: 2+ bundle buyers", /^snipe test: 2\+ buyers/],
  ["test: bundle buyer unreadable", /^snipe test: a bundle buyer unreadable/],
  ["test: 1+ SOL real", /^snipe test: 1\+ SOL real/],
  ["test: curve complete", /^snipe test: curve complete/],
  ["test: day cap", /^snipe test: the day's snipe cap/],
  ["test: hour cap", /^snipe test: the hour's snipe cap/],
  ["test: loss cap", /^snipe test: the day's loss cap/],
  ["test: wallet", /^snipe test: the wallet/],
  ["grad: not seen under 60 SOL", /^graduation: its curve (was never seen under 60 SOL|'s trades were not seen|'s completing trade was not seen)/],
  ["grad: slow completion", /^graduation: [0-9.]+ s from under 60 SOL/],
  ["grad: not SOL quote", /^graduation: not quoted in SOL/],
  ["grad: mayhem", /^graduation: mayhem/],
  ["grad: jump in migration slot", /^graduation: the insiders' jump/],
  ["grad: open limit", /^graduation: \d+ graduations already open/],
  ["grad: no quote", /^graduation: no quote/],
  ["other", /./],
  ["buy decided", null],
];

/** The funnel step of a launch row: its verdict, else its skip reason's. */
export function stepOf(verdict: string, reason: string | null): string {
  if (verdict === "paused") return "paused";
  if (verdict === "buy" || verdict === "bought" || verdict === "buy_failed") return "buy decided";
  if (verdict === "seen") return "no verdict";
  for (const [step, re] of FUNNEL_STEPS) if (re && re.test(reason ?? "")) return step;
  return "other";
}

/**
 * Realized = the sells' SOL minus the tokens sold's share of `sol_in`; the cost of the tokens
 * still held; last active = the newest landed order, else when it closed, bought or opened.
 */
const COST_SOLD = `(CASE WHEN p.fill_tokens > 0 THEN p.sol_in * 1.0 * (p.fill_tokens - COALESCE(p.tokens_held, 0)) / p.fill_tokens
  WHEN p.state = 'CLOSED' THEN p.sol_in ELSE 0 END)`;
const COST_HELD = "(CASE WHEN p.fill_tokens > 0 THEN p.sol_in * 1.0 * COALESCE(p.tokens_held, 0) / p.fill_tokens ELSE 0 END)";
const REALIZED = `(p.sol_out - ${COST_SOLD})`;
const LAST_ACTIVE = `COALESCE((SELECT MAX(o.t_landed) FROM orders o WHERE o.position_id = p.id AND o.status = 'landed'),
  p.closed_ns, p.bought_ns, p.opened_ns)`;
/** One row per coin per mode and run over `pos`: open if any position is, else stuck, else closed. */
const GROUPED = `SELECT MAX(x.id) AS id, x.run_id, x.mint, x.mode, MIN(x.venue) AS venue,
  CASE WHEN SUM(x.is_open) > 0 THEN 'OPEN' WHEN SUM(x.is_stuck) > 0 THEN 'STUCK'
       WHEN SUM(x.is_closed) > 0 THEN 'CLOSED' ELSE 'BUY_FAILED' END AS state,
  MIN(x.opened_ns) AS opened_ns,
  CASE WHEN SUM(x.is_open) + SUM(x.is_stuck) = 0 THEN MAX(x.closed_ns) END AS closed_ns,
  SUM(x.sol_in) AS sol_in, SUM(x.sol_out) AS sol_out, SUM(x.wallet_delta) AS wallet_delta, SUM(x.rent_locked) AS rent_locked,
  MIN(x.bought_ns) AS bought_ns, MAX(x.last_active_ns) AS last_active_ns,
  SUM(x.tokens_held) AS tokens_held, SUM(x.fill_tokens) AS fill_tokens,
  SUM(x.realized) AS realized, SUM(x.cost_sold) AS cost_sold, SUM(x.cost_held) AS cost_held, SUM(x.total) AS total,
  SUM(x.buys) AS buys, SUM(x.sells) AS sells, MAX(x.sold_ns) AS sold_ns, COUNT(*) AS positions
  FROM pos x GROUP BY x.run_id, x.mint, x.mode`;
const GROUP_SORT_SQL: Record<Sort, string> = {
  last_active: "g.last_active_ns",
  realized: "g.realized",
  total: "g.total",
  balance: "g.cost_held",
  bought: "g.sol_in",
  sold: "g.sol_out",
};

export class History {
  path: string;
  private conn: DatabaseSync | null = null;

  constructor(path: string) {
    this.path = path;
  }

  /** The store, opened read-only once it exists; null before. */
  db(): DatabaseSync | null {
    if (this.conn) return this.conn;
    if (!existsSync(this.path)) return null;
    try {
      this.conn = new DatabaseSync(this.path, { readOnly: true });
      this.conn.exec("PRAGMA busy_timeout = 2000");
    } catch {
      this.conn = null;
    }
    return this.conn;
  }

  close(): void {
    this.conn?.close();
    this.conn = null;
  }

  available(): boolean {
    return this.db() != null;
  }

  private has(table: string): boolean {
    const db = this.db();
    return !!db?.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?").get(table);
  }

  /** Mode, venue, run and deployer conditions on `positions p`. */
  private books(b: Books): { where: string[]; params: string[] } {
    const where = ["p.mode = ?"];
    const params: string[] = [b.mode];
    if (b.venue !== "all") {
      where.push("p.venue = ?");
      params.push(b.venue);
    }
    if (b.run) {
      where.push("p.run_id = ?");
      params.push(b.run);
    }
    if (b.deployer) {
      where.push("p.deployer = ?");
      params.push(b.deployer);
    }
    return { where, params };
  }

  /** Every position under `where` with its own sums and fills (the `pos` table of `page`). */
  private positionsSql(where: string[]): string {
    const fill = (side: string, order: string) => `(SELECT o.quote_net * 1.0 / o.tokens FROM orders o
      WHERE o.position_id = p.id AND o.side = '${side}' AND o.status = 'landed' AND o.tokens > 0 ORDER BY ${order} LIMIT 1)`;
    const landed = (side: string) =>
      `(SELECT COUNT(*) FROM orders o WHERE o.position_id = p.id AND o.side = '${side}' AND o.status = 'landed')`;
    return `SELECT p.id, p.run_id, p.mint, p.mode, p.venue, p.state, p.opened_ns, p.closed_ns, p.sol_in, p.sol_out,
             p.wallet_delta_lamports AS wallet_delta, p.rent_locked_lamports AS rent_locked, p.exit_reason,
             p.deployer, p.follow_kind, p.tier,
             COALESCE(p.tokens_held, 0) AS tokens_held, COALESCE(p.fill_tokens, 0) AS fill_tokens,
             COALESCE(p.bought_ns, (SELECT MIN(o.t_landed) FROM orders o
               WHERE o.position_id = p.id AND o.side = 'buy' AND o.status = 'landed')) AS bought_ns,
             ${LAST_ACTIVE} AS last_active_ns,
             ${REALIZED} AS realized, ${COST_SOLD} AS cost_sold, ${COST_HELD} AS cost_held,
             (CASE WHEN p.state = 'CLOSED' THEN p.wallet_delta_lamports ELSE ${REALIZED} END) AS total,
             ${landed("buy")} AS buys, ${landed("sell")} AS sells,
             ${fill("buy", "o.client_id")} AS buy_price, ${fill("sell", "o.t_landed DESC, o.client_id DESC")} AS sell_price,
             (SELECT MAX(o.t_landed) FROM orders o WHERE o.position_id = p.id AND o.side = 'sell' AND o.status = 'landed') AS sold_ns,
             (p.state IN ('OPEN','SELL_PENDING','BUY_PENDING')) AS is_open, (p.state = 'STUCK') AS is_stuck,
             (p.state = 'CLOSED') AS is_closed
      FROM positions p ${where.length ? "WHERE " + where.join(" AND ") : ""}`;
  }

  /**
   * One page of the trade history: one row per coin per mode and run (a coin bought again is the
   * same row; its sells in steps are counted, every step is in the side panel).
   */
  page(show: Show, b: Books, page: number, rows: number, sort: Sort = "last_active", dir = "desc"): Page<HistoryRow> & {
    hidden_failed: number;
    available: boolean;
  } {
    const db = this.db();
    if (!db) return { ...emptyPage<HistoryRow>(rows), hidden_failed: 0 };
    const d = dir === "asc" ? "ASC" : "DESC";
    const { where, params } = this.books(b);
    const shown: string[] = [];
    if (show === "open") shown.push("g.state IN ('OPEN','SELL_PENDING','BUY_PENDING')");
    if (show === "closed") shown.push("g.state = 'CLOSED'");
    if (show === "failed") shown.push("g.state IN ('BUY_FAILED','STUCK')");
    const withs = `WITH pos AS (${this.positionsSql(where)}), grp AS (${GROUPED})`;
    const count = (extra: string[]) =>
      (db.prepare(`${withs} SELECT COUNT(*) AS n FROM grp g ${extra.length ? "WHERE " + extra.join(" AND ") : ""}`)
        .get(...params) as { n: number }).n;
    const total = count(shown);
    const hidden_failed = show === "open" || show === "closed" ? count(["g.state = 'BUY_FAILED'"]) : 0;
    const at = clampPage(total, page, rows);
    const same = "y.run_id = g.run_id AND y.mint = g.mint AND y.mode = g.mode";
    const oldest = (col: string) => `(SELECT y.${col} FROM pos y WHERE ${same} ORDER BY y.id ASC LIMIT 1)`;
    const firstBuy = (col: string) => `(SELECT o.${col} FROM orders o JOIN pos y ON y.id = o.position_id
      WHERE ${same} AND o.side = 'buy' ORDER BY o.client_id LIMIT 1)`;
    const sql = `${withs}
      SELECT g.id, g.mint, n.symbol, g.mode, g.venue, g.state, g.run_id,
             g.opened_ns / 1000000 AS opened_ms, g.closed_ns / 1000000 AS closed_ms,
             g.sol_in, g.sol_out, g.wallet_delta, g.rent_locked,
             (SELECT y.exit_reason FROM pos y WHERE ${same} AND y.exit_reason IS NOT NULL ORDER BY y.id DESC LIMIT 1) AS exit_reason,
             g.bought_ns / 1000000 AS bought_ms, g.last_active_ns / 1000000 AS last_active_ms,
             g.tokens_held, g.fill_tokens,
             ROUND(g.realized) AS realized, ROUND(g.cost_sold) AS cost_sold, ROUND(g.cost_held) AS cost_held,
             g.buys, g.sells, g.positions, g.sold_ns / 1000000 AS sold_ms,
             ${oldest("deployer")} AS deployer, ${oldest("follow_kind")} AS follow_kind, ${oldest("tier")} AS tier,
             ${firstBuy("detect_to_ready_ms")} AS ready_ms, ${firstBuy("launch_to_landed_ms")} AS launch_to_landed_ms,
             (SELECT y.buy_price FROM pos y WHERE ${same} AND y.buy_price IS NOT NULL ORDER BY y.id ASC LIMIT 1) AS buy_price,
             (SELECT y.sell_price FROM pos y WHERE ${same} AND y.sell_price IS NOT NULL
                ORDER BY y.sold_ns DESC, y.id DESC LIMIT 1) AS sell_price
      FROM grp g LEFT JOIN token_names n ON n.mint = g.mint
      ${shown.length ? "WHERE " + shown.join(" AND ") : ""}
      ORDER BY ${GROUP_SORT_SQL[sort]} ${d}, g.id ${d} LIMIT ? OFFSET ?`;
    const got = db.prepare(sql).all(...params, rows, at.offset) as unknown as (HistoryRow & {
      buy_price: number | null;
      sell_price: number | null;
    })[];
    const mc = (price: number | null) => (price == null ? null : Math.round(price * SUPPLY));
    const out = got.map(({ buy_price, sell_price, ...r }) => ({ ...r, mc_buy: mc(buy_price), mc_sell: mc(sell_price) }));
    return { rows: out, total, page: at.page, rows_per_page: rows, hidden_failed, available: true };
  }

  /**
   * A row's side panel: every order of the coin's positions in its run and mode (landed, failed
   * and timed out), newest first, each sell with its step; the positions' launch context; the
   * exit rule's decisions on the coin.
   */
  orders(id: number): {
    position: {
      id: number; mint: string; symbol: string | null; mode: string; venue: string; state: string; run_id: string;
      deployer: string | null; follow_kind: string | null; tier: string | null; rent_locked: number; score: number | null;
    };
    orders: OrderRow[];
    exits: { at_ms: number; reason: string; share: number; value_x: number; peak_x: number; held_secs: number; net_behind_sol: number }[];
  } | null {
    const db = this.db();
    if (!db) return null;
    const position = db
      .prepare(
        `SELECT p.id, p.mint, n.symbol, p.mode, p.venue, p.state, p.run_id, p.deployer, p.follow_kind, p.tier,
                (SELECT SUM(q.rent_locked_lamports) FROM positions q WHERE q.run_id = p.run_id AND q.mint = p.mint AND q.mode = p.mode)
                  AS rent_locked,
                (SELECT l.score FROM launches l WHERE l.mint = p.mint AND l.score IS NOT NULL LIMIT 1) AS score
         FROM positions p LEFT JOIN token_names n ON n.mint = p.mint WHERE p.id = ?`,
      )
      .get(id) as any;
    if (!position) return null;
    const got = db
      .prepare(
        `SELECT o.client_id, o.position_id, o.side, o.reason, o.status, o.sig,
                COALESCE(o.t_landed, o.t_sent, o.t_signal) / 1000000 AS at_ms,
                o.quote_net, o.tokens, o.wallet_delta_lamports AS wallet_delta, o.sell_step, o.pnl_percent, o.venue, o.error,
                o.landed_slot, o.signal_slot, o.detect_to_ready_ms, o.launch_to_landed_ms,
                CASE WHEN o.side = 'buy' AND o.status = 'landed' AND o.client_id =
                       (SELECT MIN(f.client_id) FROM orders f WHERE f.position_id = o.position_id AND f.side = 'buy')
                     THEN (SELECT q.rent_locked_lamports FROM positions q WHERE q.id = o.position_id) END AS rent,
                CASE WHEN o.status = 'landed' AND o.tokens > 0 THEN o.quote_net * 1.0 / o.tokens END AS price
         FROM orders o
         WHERE o.position_id IN (SELECT q.id FROM positions q JOIN positions me ON me.id = ?
                                 WHERE q.run_id = me.run_id AND q.mint = me.mint AND q.mode = me.mode)
           AND o.status IN ('landed', 'failed', 'timed_out')
         ORDER BY COALESCE(o.t_landed, o.t_sent, o.t_signal) DESC, o.client_id DESC`,
      )
      .all(id) as unknown as (Omit<OrderRow, "mc" | "fees"> & { price: number | null })[];
    const orders = got.map(({ price, ...o }) => {
      let fees: number | null = null;
      if (o.wallet_delta != null) {
        if (o.side === "buy") fees = Math.max(0, -o.wallet_delta - (o.status === "landed" ? o.quote_net ?? 0 : 0) - (o.rent ?? 0));
        else fees = Math.max(0, (o.status === "landed" ? o.quote_net ?? 0 : 0) - o.wallet_delta);
      }
      return { ...o, fees, mc: price == null ? null : Math.round(price * SUPPLY) };
    });
    const exits = this.has("exit_decision")
      ? (db
          .prepare(
            `SELECT t * 1000 AS at_ms, reason, share, value_x, peak_x, held_secs, net_behind_sol FROM exit_decision
             WHERE mint = ? AND paper = ? ORDER BY id DESC LIMIT 50`,
          )
          .all(position.mint, position.mode === "paper" ? 1 : 0) as any[])
      : [];
    return { position: { ...position, rent_locked: position.rent_locked ?? 0 }, orders, exits };
  }

  /** Closed positions: count, winners, the wallet's change and the rent locked in them. */
  summary(b: Books): { trades: number; wins: number; net: number; gross_win: number; gross_loss: number; rent: number } {
    const db = this.db();
    if (!db) return { trades: 0, wins: 0, net: 0, gross_win: 0, gross_loss: 0, rent: 0 };
    const { where, params } = this.books(b);
    const r = db
      .prepare(
        `SELECT COUNT(*) AS trades, COALESCE(SUM(p.wallet_delta_lamports > 0), 0) AS wins,
                COALESCE(SUM(p.wallet_delta_lamports), 0) AS net,
                COALESCE(SUM(CASE WHEN p.wallet_delta_lamports > 0 THEN p.wallet_delta_lamports END), 0) AS gross_win,
                COALESCE(-SUM(CASE WHEN p.wallet_delta_lamports <= 0 THEN p.wallet_delta_lamports END), 0) AS gross_loss,
                COALESCE(SUM(p.rent_locked_lamports), 0) AS rent
         FROM positions p WHERE ${["p.state = 'CLOSED'", ...where].join(" AND ")}`,
      )
      .get(...params) as any;
    return r;
  }

  /** The wallet's change from `fromMs` to `toMs`: every order's wallet_delta, and the rent the buys locked. */
  private walletChange(b: Books, fromMs: number, toMs: number): { net: number; rent: number } {
    const db = this.db()!;
    const { where, params } = this.books(b);
    const at = "COALESCE(o.t_landed, o.t_sent, o.t_signal)";
    const net = db
      .prepare(
        `SELECT COALESCE(SUM(o.wallet_delta_lamports), 0) AS v FROM orders o JOIN positions p ON p.id = o.position_id
         WHERE ${[...where, `${at} >= ?`, `${at} < ?`].join(" AND ")}`,
      )
      .get(...params, nsOf(fromMs), nsOf(toMs)) as { v: number };
    const rent = db
      .prepare(
        `SELECT COALESCE(SUM(p.rent_locked_lamports), 0) AS v FROM positions p
         WHERE ${[...where, "p.bought_ns >= ?", "p.bought_ns < ?"].join(" AND ")}`,
      )
      .get(...params, nsOf(fromMs), nsOf(toMs)) as { v: number };
    return { net: net.v, rent: rent.v };
  }

  /**
   * The calendar: per UTC day of `month`, the wallet's change (every order's wallet_delta: buys,
   * sells and failed attempts' fees), the rent the day's buys locked in token accounts (inside
   * that change, shown on its own line), the positions closed that day and the orders landed.
   */
  calendar(b: Books, month: string): CalendarDay[] {
    const db = this.db();
    if (!db) return [];
    const [y, m] = month.split("-").map(Number) as [number, number];
    const from = nsOf(Date.UTC(y, m - 1, 1));
    const to = nsOf(Date.UTC(y, m, 1));
    const { where, params } = this.books(b);
    const byDay = new Map<string, CalendarDay>();
    const dayOf = (day: string) => {
      let d = byDay.get(day);
      if (!d) {
        d = {
          day, net_lamports: 0, rent_lamports: 0, trades: 0, wins: 0, profit_lamports: 0, loss_lamports: 0,
          buys: 0, sells: 0, failed: 0, buy_volume_lamports: 0, sell_volume_lamports: 0,
        };
        byDay.set(day, d);
      }
      return d;
    };
    const at = "COALESCE(o.t_landed, o.t_sent, o.t_signal)";
    const orders = db
      .prepare(
        `SELECT date(${at} / 1000000000, 'unixepoch') AS day,
                COALESCE(SUM(o.wallet_delta_lamports), 0) AS net,
                COALESCE(SUM(o.side = 'buy' AND o.status = 'landed'), 0) AS buys,
                COALESCE(SUM(o.side = 'sell' AND o.status = 'landed'), 0) AS sells,
                COALESCE(SUM(o.status <> 'landed'), 0) AS failed,
                COALESCE(SUM(CASE WHEN o.side = 'buy' AND o.status = 'landed' THEN o.quote_net END), 0) AS buy_volume,
                COALESCE(SUM(CASE WHEN o.side = 'sell' AND o.status = 'landed' THEN o.quote_net END), 0) AS sell_volume
         FROM orders o JOIN positions p ON p.id = o.position_id
         WHERE ${[...where, `${at} >= ?`, `${at} < ?`].join(" AND ")}
         GROUP BY day`,
      )
      .all(...params, from, to) as any[];
    for (const o of orders) {
      Object.assign(dayOf(o.day), {
        net_lamports: o.net, buys: o.buys, sells: o.sells, failed: o.failed,
        buy_volume_lamports: o.buy_volume, sell_volume_lamports: o.sell_volume,
      });
    }
    const rent = db
      .prepare(
        `SELECT date(p.bought_ns / 1000000000, 'unixepoch') AS day, COALESCE(SUM(p.rent_locked_lamports), 0) AS rent
         FROM positions p WHERE ${[...where, "p.bought_ns >= ?", "p.bought_ns < ?"].join(" AND ")} GROUP BY day`,
      )
      .all(...params, from, to) as any[];
    for (const r of rent) if (r.rent) dayOf(r.day).rent_lamports = r.rent;
    const closed = db
      .prepare(
        `SELECT date(p.closed_ns / 1000000000, 'unixepoch') AS day, COUNT(*) AS trades,
                COALESCE(SUM(p.wallet_delta_lamports > 0), 0) AS wins,
                COALESCE(SUM(CASE WHEN p.wallet_delta_lamports > 0 THEN p.wallet_delta_lamports END), 0) AS profit,
                COALESCE(SUM(CASE WHEN p.wallet_delta_lamports < 0 THEN p.wallet_delta_lamports END), 0) AS loss
         FROM positions p
         WHERE ${[...where, "p.state = 'CLOSED'", "p.closed_ns >= ?", "p.closed_ns < ?"].join(" AND ")}
         GROUP BY day`,
      )
      .all(...params, from, to) as any[];
    for (const c of closed) {
      Object.assign(dayOf(c.day), { trades: c.trades, wins: c.wins, profit_lamports: c.profit, loss_lamports: c.loss });
    }
    return [...byDay.values()].sort((a, b2) => (a.day < b2.day ? -1 : 1));
  }

  /**
   * The overview: closed positions (as `summary`); today's wallet change and rent (UTC, as the
   * calendar's tile); the open positions with the cost of the tokens they hold (valued by the bot).
   */
  overview(b: Books, nowMs = Date.now()): {
    trades: number; wins: number; realized: number; rent_closed: number;
    today: number; today_rent: number; today_trades: number;
    open: { mint: string; cost_held: number }[];
  } {
    const s = this.summary(b);
    const db = this.db();
    if (!db) return { trades: 0, wins: 0, realized: 0, rent_closed: 0, today: 0, today_rent: 0, today_trades: 0, open: [] };
    const midnight = Math.floor(nowMs / DAY_MS) * DAY_MS;
    const today = this.walletChange(b, midnight, midnight + DAY_MS);
    const { where, params } = this.books(b);
    const todayTrades = db
      .prepare(`SELECT COUNT(*) AS n FROM positions p WHERE ${[...where, "p.state = 'CLOSED'", "p.closed_ns >= ?"].join(" AND ")}`)
      .get(...params, nsOf(midnight)) as { n: number };
    const open = db
      .prepare(
        `SELECT p.mint, ROUND(${COST_HELD}) AS cost_held FROM positions p
         WHERE ${[...where, `p.state IN (${OPEN_STATES.map((x) => `'${x}'`).join(", ")})`].join(" AND ")} ORDER BY p.id`,
      )
      .all(...params) as any[];
    return {
      trades: s.trades, wins: s.wins, realized: s.net, rent_closed: s.rent,
      today: today.net, today_rent: today.rent, today_trades: todayTrades.n,
      open: open.map((o) => ({ mint: o.mint, cost_held: o.cost_held })),
    };
  }

  /** The runs: one row per bot start (the runs table), and runs that only have positions (imports). */
  runs(): { run_id: string; mode: string | null; started_ms: number | null; stopped_ms: number | null; clean: number | null;
    settings: Record<string, string> | null; positions: number; closed: number; net: number; first_ms: number | null; last_ms: number | null }[] {
    const db = this.db();
    if (!db) return [];
    const rows = db
      .prepare(
        `WITH agg AS (SELECT run_id, COUNT(*) AS positions, COALESCE(SUM(state = 'CLOSED'), 0) AS closed,
                       COALESCE(SUM(CASE WHEN state = 'CLOSED' THEN wallet_delta_lamports END), 0) AS net,
                       MIN(opened_ns) / 1000000 AS first_ms, MAX(opened_ns) / 1000000 AS last_ms,
                       GROUP_CONCAT(DISTINCT mode) AS mode
                     FROM positions GROUP BY run_id)
         SELECT * FROM (
         SELECT r.run_id, r.mode, r.started_ns / 1000000 AS started_ms, r.stopped_ns / 1000000 AS stopped_ms, r.clean, r.settings,
                COALESCE(a.positions, 0) AS positions, COALESCE(a.closed, 0) AS closed, COALESCE(a.net, 0) AS net,
                a.first_ms, a.last_ms
         FROM runs r LEFT JOIN agg a ON a.run_id = r.run_id
         UNION ALL
         SELECT a.run_id, a.mode, NULL, NULL, NULL, NULL, a.positions, a.closed, a.net, a.first_ms, a.last_ms
         FROM agg a WHERE a.run_id NOT IN (SELECT run_id FROM runs))
         ORDER BY COALESCE(started_ms, last_ms) DESC`,
      )
      .all() as any[];
    return rows.map((r) => {
      let settings: Record<string, string> | null = null;
      try {
        settings = r.settings ? JSON.parse(r.settings) : null;
      } catch {
        settings = null;
      }
      return { ...r, settings };
    });
  }

  /** The newest run (by its start), the bot's current one while it runs. */
  currentRun(): { run_id: string; mode: string; started_ms: number; stopped_ms: number | null } | null {
    const db = this.db();
    if (!db || !this.has("runs")) return null;
    return (db
      .prepare("SELECT run_id, mode, started_ns / 1000000 AS started_ms, stopped_ns / 1000000 AS stopped_ms FROM runs ORDER BY started_ns DESC LIMIT 1")
      .get() as any) ?? null;
  }

  /** The deployers of positions (the trade history's deployer filter), newest first. */
  deployers(mode: Mode): { deployer: string; positions: number }[] {
    const db = this.db();
    if (!db) return [];
    return db
      .prepare(
        `SELECT deployer, COUNT(*) AS positions FROM positions WHERE mode = ? AND deployer IS NOT NULL
         GROUP BY deployer ORDER BY MAX(opened_ns) DESC LIMIT 200`,
      )
      .all(mode) as any[];
  }

  /** Symbols of `mints` (token_names). */
  symbols(mints: string[]): Record<string, string> {
    const db = this.db();
    const out: Record<string, string> = {};
    if (!db || !mints.length) return out;
    const st = db.prepare("SELECT symbol FROM token_names WHERE mint = ?");
    for (const m of mints) {
      const r = st.get(m) as { symbol?: string } | undefined;
      if (r?.symbol) out[m] = r.symbol;
    }
    return out;
  }

  /** The live activity, newest first: every detection, skip, buy, sell and setting change. */
  activity(kind: string, venue: string, run: string, page: number, rows: number) {
    const db = this.db();
    if (!db) return emptyPage<any>(rows);
    const where: string[] = [];
    const params: string[] = [];
    if (kind !== "all") {
      where.push("a.kind = ?");
      params.push(kind);
    }
    if (venue !== "all") {
      where.push("a.venue = ?");
      params.push(venue);
    }
    if (run) {
      where.push("a.run_id = ?");
      params.push(run);
    }
    const w = where.length ? "WHERE " + where.join(" AND ") : "";
    const total = (db.prepare(`SELECT COUNT(*) AS n FROM activity a ${w}`).get(...params) as { n: number }).n;
    const at = clampPage(total, page, rows);
    const out = db
      .prepare(
        `SELECT a.id, a.at_ns / 1000000 AS at_ms, a.kind, a.venue, a.dex, a.mint, a.message, n.symbol, a.run_id
         FROM activity a LEFT JOIN token_names n ON n.mint = a.mint ${w}
         ORDER BY a.at_ns DESC, a.id DESC LIMIT ? OFFSET ?`,
      )
      .all(...params, rows, at.offset);
    return { rows: out, total, page: at.page, rows_per_page: rows, available: true };
  }

  /**
   * The Snipe page over the last `hours`: launches and graduations seen by venue and kind; the
   * followed deployers' launches by verdict and their skip reasons; the deployers' records; the
   * fee engine's leader classes; the distance and region skips; the bundle window. With one page
   * of the followed launches (newest first, `verdict` "all" or one).
   */
  snipe(hours: number, venue: string, verdict: string, page: number, rows: number, nowMs = Date.now()) {
    const db = this.db();
    if (!db) return { ...emptyPage<any>(rows), stats: null };
    const since = nowMs - hours * 3600_000;
    const sinceNs = nsOf(since);
    const vw = venue === "all" ? "" : "AND l.venue = ?";
    const vp = venue === "all" ? [] : [venue];
    const seen = db
      .prepare(
        `SELECT l.venue, l.kind, COUNT(*) AS n, COALESCE(SUM(l.followed), 0) AS followed
         FROM launches l WHERE l.seen_ns >= ? ${vw} GROUP BY l.venue, l.kind`,
      )
      .all(sinceNs, ...vp) as any[];
    const verdicts = db
      .prepare(
        `SELECT l.verdict, COUNT(*) AS n FROM launches l WHERE l.followed = 1 AND l.seen_ns >= ? ${vw} GROUP BY l.verdict`,
      )
      .all(sinceNs, ...vp) as any[];
    const reasons = db
      .prepare(
        `SELECT l.skip_reason AS reason FROM launches l
         WHERE l.followed = 1 AND l.verdict = 'skip' AND l.seen_ns >= ? ${vw}`,
      )
      .all(sinceNs, ...vp) as { reason: string | null }[];
    const byStep: Record<string, number> = {};
    for (const r of reasons) {
      const s = stepOf("skip", r.reason);
      byStep[s] = (byStep[s] ?? 0) + 1;
    }
    const followKinds = db
      .prepare(
        `SELECT COALESCE(l.follow_kind, '–') AS kind, COUNT(*) AS n,
                COALESCE(SUM(l.verdict = 'bought'), 0) AS bought
         FROM launches l WHERE l.followed = 1 AND l.seen_ns >= ? ${vw} GROUP BY 1`,
      )
      .all(sinceNs, ...vp) as any[];
    // The deployers whose launches were followed in the window: what became of them, and their
    // record (deployer_launches: launches with no buyers by the bot's outcome check; launch_result:
    // the replayed result of a 0.5 SOL buy).
    const deployers = db
      .prepare(
        `SELECT l.deployer, COUNT(*) AS launches, COALESCE(SUM(l.verdict = 'bought'), 0) AS bought,
                COALESCE(SUM(l.verdict = 'skip'), 0) AS skipped, COALESCE(SUM(l.verdict = 'paused'), 0) AS paused,
                MAX(l.seen_ns) / 1000000 AS last_ms, MAX(l.score) AS best_score, MIN(l.follow_kind) AS follow_kind
         FROM launches l WHERE l.followed = 1 AND l.deployer IS NOT NULL AND l.seen_ns >= ? ${vw}
         GROUP BY l.deployer ORDER BY launches DESC, last_ms DESC LIMIT 25`,
      )
      .all(sinceNs, ...vp) as any[];
    const rec = db.prepare(
      "SELECT COUNT(*) AS n, COALESCE(SUM(no_pop), 0) AS no_pop FROM deployer_launches WHERE deployer = ?",
    );
    const res = db.prepare(
      `SELECT COUNT(*) AS n, COALESCE(SUM(down), 0) AS down, AVG(pnl_sol) AS avg_pnl
       FROM launch_result WHERE deployer = ? OR creator = ?`,
    );
    for (const d of deployers) {
      const r = rec.get(d.deployer) as any;
      const x = res.get(d.deployer, d.deployer) as any;
      Object.assign(d, { record_launches: r.n, record_no_pop: r.no_pop, results: x.n, results_down: x.down, avg_pnl_sol: x.avg_pnl });
    }
    // The fee engine's leader classes (positions_log: every followed launch and a sample of others).
    const sinceDay = new Date(since).toISOString().slice(0, 10);
    const logRows = db
      .prepare(
        `SELECT leader_class AS cls, kind, our_status, first_position, our_cu_price, rival_cu_price, our_fee_sol, bundle_buys
         FROM positions_log WHERE (rx_ns >= ? OR (rx_ns = 0 AND "date" >= ?))`,
      )
      .all(sinceNs, sinceDay) as any[];
    const classes = new Map<string, any>();
    const bundleBuys: Record<string, number> = { "0": 0, "1": 0, "2": 0, "3-5": 0, "6+": 0 };
    for (const r of logRows) {
      const k = r.cls || "unknown";
      let c = classes.get(k);
      if (!c) {
        c = { cls: k, launches: 0, followed: 0, ours: 0, first: 0, our_cu: [] as number[], rival_cu: [] as number[], fee: [] as number[] };
        classes.set(k, c);
      }
      c.launches++;
      if (r.kind === "followed") c.followed++;
      if (r.our_status && r.our_status !== "-") c.ours++;
      if (Number(r.first_position) === 1) c.first++;
      if (Number(r.our_cu_price) > 0) c.our_cu.push(Number(r.our_cu_price));
      if (Number(r.rival_cu_price) > 0) c.rival_cu.push(Number(r.rival_cu_price));
      if (Number(r.our_fee_sol) > 0) c.fee.push(Number(r.our_fee_sol));
      const b = Number(r.bundle_buys);
      if (Number.isFinite(b)) bundleBuys[b <= 0 ? "0" : b === 1 ? "1" : b === 2 ? "2" : b <= 5 ? "3-5" : "6+"]!++;
    }
    const classTable = [...classes.values()]
      .map((c) => ({
        cls: c.cls, launches: c.launches, followed: c.followed, ours: c.ours, first: c.first,
        our_cu_p50: quantile(c.our_cu, 0.5), rival_cu_p50: quantile(c.rival_cu, 0.5), fee_sol_p50: quantile(c.fee, 0.5),
      }))
      .sort((a, b) => b.launches - a.launches);
    // Distance and LEADER_REGIONS skips (route_skips: `time` is unix seconds).
    const skips = db
      .prepare(
        `SELECT reason, class, COUNT(*) AS n, AVG(rtt_ms) AS rtt_ms FROM route_skips
         WHERE CAST("time" AS INTEGER) >= ? GROUP BY reason, class ORDER BY n DESC`,
      )
      .all(Math.floor(since / 1000)) as any[];
    const regionSkips = db
      .prepare(
        `SELECT COUNT(*) AS n FROM launches l WHERE l.seen_ns >= ? AND l.skip_reason LIKE '%LEADER_REGIONS%' ${vw}`,
      )
      .get(sinceNs, ...vp) as { n: number };
    const distanceDaily = db
      .prepare(`SELECT "date" AS day, "followed" AS followed, passed_rules, skipped_for_distance FROM distance_daily ORDER BY id DESC LIMIT 14`)
      .all() as any[];
    // The bundle window: the deliberate wait after a create for its bundle's buys (hot_stamps).
    const windows = (db
      .prepare(`SELECT window_ns FROM hot_stamps WHERE window_ns > 0 ORDER BY id DESC LIMIT 2000`)
      .all() as { window_ns: number }[]).map((r) => Number(r.window_ns) / 1000);
    const stats = {
      hours,
      seen,
      verdicts: Object.fromEntries(verdicts.map((v) => [v.verdict, v.n])),
      skip_steps: Object.entries(byStep).sort((a, b) => b[1] - a[1]),
      follow_kinds: followKinds,
      deployers,
      classes: classTable,
      route_skips: skips,
      region_skips: regionSkips.n,
      distance_daily: distanceDaily,
      bundle: { window_us_p50: quantile(windows, 0.5), window_us_p99: quantile(windows, 0.99), n: windows.length, buys: bundleBuys },
    };
    // The followed launches, newest first.
    const where = ["l.followed = 1", "l.seen_ns >= ?"];
    const params: (string | bigint)[] = [sinceNs];
    if (venue !== "all") {
      where.push("l.venue = ?");
      params.push(venue);
    }
    if (verdict !== "all") {
      where.push("l.verdict = ?");
      params.push(verdict);
    }
    const w = "WHERE " + where.join(" AND ");
    const total = (db.prepare(`SELECT COUNT(*) AS n FROM launches l ${w}`).get(...params) as { n: number }).n;
    const at = clampPage(total, page, rows);
    const out = db
      .prepare(
        `SELECT l.mint, l.venue, l.kind, l.seen_ns / 1000000 AS seen_ms, l.slot, l.source, l.deployer, l.dev_buy_lamports,
                l.follow_kind, l.score, l.verdict, l.skip_reason, n.symbol
         FROM launches l LEFT JOIN token_names n ON n.mint = l.mint ${w}
         ORDER BY l.seen_ns DESC LIMIT ? OFFSET ?`,
      )
      .all(...params, rows, at.offset);
    return { rows: out, total, page: at.page, rows_per_page: rows, available: true, stats };
  }

  /**
   * The rejection funnel over the last `hours` (launches): every launch counts once, under its
   * verdict's step (paused, buy decided) or its skip reason's; per hour; 20 newest samples per step.
   */
  rejections(hours: number, venue: string, nowMs = Date.now()) {
    const db = this.db();
    const none = { available: false, hours, coins: 0, steps: [] as string[], totals: {}, per_hour: [], samples: {} };
    if (!db) return none;
    const since = nowMs - hours * 3600_000;
    const rows = db
      .prepare(
        `SELECT l.mint, l.venue, l.kind, l.seen_ns / 1000000 AS seen_ms, l.verdict, l.skip_reason, l.followed, l.deployer,
                l.dev_buy_lamports, l.score, n.symbol
         FROM launches l LEFT JOIN token_names n ON n.mint = l.mint
         WHERE l.seen_ns >= ? ${venue === "all" ? "" : "AND l.venue = ?"} ORDER BY l.seen_ns DESC`,
      )
      .all(nsOf(since), ...(venue === "all" ? [] : [venue])) as any[];
    const order = ["no verdict", ...FUNNEL_STEPS.map(([s]) => s)];
    const totals: Record<string, number> = {};
    const hoursMap = new Map<number, { hour_ms: number; coins: number; counts: Record<string, number> }>();
    const samples: Record<string, any[]> = {};
    for (const r of rows) {
      const step = stepOf(r.verdict, r.skip_reason);
      totals[step] = (totals[step] ?? 0) + 1;
      const hour = Math.floor(r.seen_ms / 3600_000) * 3600_000;
      let h = hoursMap.get(hour);
      if (!h) {
        h = { hour_ms: hour, coins: 0, counts: {} };
        hoursMap.set(hour, h);
      }
      h.coins++;
      h.counts[step] = (h.counts[step] ?? 0) + 1;
      const list = (samples[step] ??= []);
      if (list.length < 20) list.push(r);
    }
    const steps = order.filter((s) => totals[s]);
    return {
      available: true,
      hours,
      coins: rows.length,
      steps,
      totals,
      per_hour: [...hoursMap.values()].sort((a, b) => b.hour_ms - a.hour_ms),
      samples,
    };
  }

  /**
   * The status bar's latencies from the hot path's stamps (hot_stamps, newest first): Snipe = the
   * launch frame received → our buy signed (the last 200 signed buys, bundle window included);
   * Feed = frame received → matched as a launch (the last 2000 frames). Microseconds.
   */
  latency(): { snipe: { p50: number; p99: number; n: number } | null; feed: { p50: number; p99: number; n: number } | null } {
    const db = this.db();
    if (!db) return { snipe: null, feed: null };
    const pick = (sql: string) => (db.prepare(sql).all() as { d: number }[]).map((r) => Number(r.d) / 1000).filter((x) => x >= 0);
    const snipe = pick(`SELECT t_signed - t_recv AS d FROM hot_stamps WHERE t_signed > 0 AND t_recv > 0 ORDER BY id DESC LIMIT 200`);
    const feed = pick(`SELECT t_matched - t_recv AS d FROM hot_stamps WHERE t_matched > 0 AND t_recv > 0 ORDER BY id DESC LIMIT 2000`);
    const q = (v: number[]) => (v.length ? { p50: Math.round(quantile(v, 0.5)!), p99: Math.round(quantile(v, 0.99)!), n: v.length } : null);
    return { snipe: q(snipe), feed: q(feed) };
  }

  /** The store's own counters, as the bot writes them every minute (kv 'store_stats'). */
  storeStats(): { enqueued: number; dropped: number; written: number; errors: number; at_ms: number } | null {
    const db = this.db();
    if (!db) return null;
    const r = db.prepare("SELECT value, updated_ns / 1000000 AS at_ms FROM kv WHERE key = 'store_stats'").get() as
      | { value: string; at_ms: number }
      | undefined;
    if (!r) return null;
    try {
      const v = JSON.parse(r.value);
      return { enqueued: v.enqueued ?? 0, dropped: v.dropped ?? 0, written: v.written ?? 0, errors: v.errors ?? 0, at_ms: r.at_ms };
    } catch {
      return null;
    }
  }
}
