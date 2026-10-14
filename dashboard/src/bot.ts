// The running bot, seen through its own local API (src/utils/dashboard.rs, 127.0.0.1:8787): one
// event stream (`/api/events`, the same one the bot's built-in page reads, so the bot does no work
// for this dashboard that it doesn't already do for that page) keeps the latest state here; the two
// controls the owner allows (2026-10-13: trading on/off and the buy size) are POSTs to it.
//
// The bot's address and login come from its .env (DASHBOARD_ADDR, DASHBOARD_USER,
// DASHBOARD_PASSWORD), read once at start; only those three keys are read, and none of them is
// ever logged, returned or written to the audit log.

import { readFileSync } from "node:fs";

/** What the bot's state carries that this dashboard uses (src/utils/dashboard.rs `DashboardState`). */
export interface BotState {
  mode: string; // "dry_run" | "live"
  trading: boolean;
  wallet: string;
  trading_wallet: string;
  balance_sol: number | null;
  buy_sol: number;
  max_buy_sol: number;
  open_positions: number;
  max_open_positions: number;
  rules: string[];
  uptime_secs: number;
  started_at: number;
  counters: Record<string, number>;
  shreds?: { status: string; incomplete_slots?: number; bad_batches?: number; recovered_shreds?: number } | null;
  positions: BotPosition[];
  names?: Record<string, { symbol: string; name: string }>;
  /** The snipe test (SNIPE_TEST=1): its size (by hand in .env), caps and today's count. */
  snipe_test?: SnipeTest | null;
}

export interface SnipeTest {
  sol: number;
  max_per_day: number;
  max_per_hour: number;
  max_loss_sol: number;
  today: number;
  this_hour: number;
  pnl_today_sol: number;
}

/** One open position as the bot shows it (`PositionView`). */
export interface BotPosition {
  mint: string;
  dex: string;
  paper: boolean;
  entry_sol: number;
  token_amount: number;
  value_sol: number | null;
  pnl_percent: number | null;
  peak_pnl_percent: number | null;
  take_profit_armed: boolean;
  sold_percent: number;
  ladder_step: number;
  opened_at: number;
  updated_at: number;
}

export interface BotLink {
  url: string;
  user: string;
  password: string;
}

/** The bot's address and login from its .env; empty fields when absent (then the bot answers 401). */
export function linkFromEnv(path: string, fallbackUrl = "http://127.0.0.1:8787"): BotLink {
  const vals: Record<string, string> = {};
  try {
    for (const raw of readFileSync(path, "utf8").split("\n")) {
      const m = /^\s*(DASHBOARD_ADDR|DASHBOARD_USER|DASHBOARD_PASSWORD)\s*=\s*(.*)$/.exec(raw);
      if (m) vals[m[1]!] = m[2]!.trim().replace(/^(['"])(.*)\1$/, "$2");
    }
  } catch {
    // No .env: the fallback address, no login.
  }
  const addr = vals.DASHBOARD_ADDR;
  return {
    url: addr ? `http://${addr.replace(/^0\.0\.0\.0:/, "127.0.0.1:")}` : fallbackUrl,
    user: vals.DASHBOARD_USER ?? "admin",
    password: vals.DASHBOARD_PASSWORD ?? "",
  };
}

/** The result of a control call: the bot's answer, or why it failed (never the login). */
export interface ControlResult {
  ok: boolean;
  status: number;
  body: unknown;
  error?: string;
}

/**
 * Keeps the latest state of the bot from its event stream, reconnecting after a drop (1 s, then up
 * to 15 s apart). `state()` is null until the first event and while the stream is down for more
 * than `staleMs` (the bot sends at least one event every 10 s).
 */
export class Bot {
  link: BotLink;
  staleMs: number;
  private latest: { state: BotState; at_ms: number } | null = null;
  private abort: AbortController | null = null;
  private stopped = false;
  private retryMs = 1000;
  /** Why the stream is down (an HTTP status or a network error), for the status bar. */
  lastError: string | null = null;
  connected = false;

  constructor(link: BotLink, opts: { staleMs?: number } = {}) {
    this.link = link;
    this.staleMs = opts.staleMs ?? 25_000;
  }

  private headers(): Record<string, string> {
    const token = Buffer.from(`${this.link.user}:${this.link.password}`).toString("base64");
    return { authorization: `Basic ${token}` };
  }

  /** The latest state and when it came, or null (never received, or stale). */
  state(now = Date.now()): { state: BotState; at_ms: number } | null {
    if (!this.latest || now - this.latest.at_ms > this.staleMs) return null;
    return this.latest;
  }

  /** Starts following the event stream (in the background). */
  start(): void {
    this.stopped = false;
    void this.loop();
  }

  stop(): void {
    this.stopped = true;
    this.abort?.abort();
  }

  /** Takes one event's data (tests call it directly). */
  take(data: string, now = Date.now()): void {
    let s: any;
    try {
      s = JSON.parse(data);
    } catch {
      return;
    }
    if (!s || typeof s !== "object") return;
    // The trade log rows are the bot page's own history; this dashboard reads the store instead.
    delete s.trades;
    delete s.activity;
    this.latest = { state: s as BotState, at_ms: now };
  }

  private async loop(): Promise<void> {
    while (!this.stopped) {
      try {
        await this.follow();
        this.retryMs = 1000;
      } catch (e) {
        this.lastError = e instanceof Error ? e.message : String(e);
      }
      this.connected = false;
      if (this.stopped) break;
      await new Promise((r) => setTimeout(r, this.retryMs));
      this.retryMs = Math.min(15_000, this.retryMs * 2);
    }
  }

  /** One connection: reads `event: state` / `data: {...}` blocks until the stream ends. */
  private async follow(): Promise<void> {
    this.abort = new AbortController();
    const r = await fetch(`${this.link.url}/api/events`, {
      headers: { ...this.headers(), accept: "text/event-stream" },
      signal: this.abort.signal,
    });
    if (!r.ok || !r.body) throw new Error(`bot answered HTTP ${r.status}`);
    this.connected = true;
    this.lastError = null;
    this.retryMs = 1000;
    const decoder = new TextDecoder();
    let buf = "";
    let data: string[] = [];
    for await (const chunk of r.body as unknown as AsyncIterable<Uint8Array>) {
      buf += decoder.decode(chunk, { stream: true });
      let nl: number;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl).replace(/\r$/, "");
        buf = buf.slice(nl + 1);
        if (line === "") {
          if (data.length) this.take(data.join("\n"));
          data = [];
        } else if (line.startsWith("data:")) {
          data.push(line.slice(5).replace(/^ /, ""));
        }
      }
      // A runaway line (no newline in 32 MB) is dropped rather than kept growing.
      if (buf.length > 32 * 1024 * 1024) buf = "";
    }
    throw new Error("bot closed the stream");
  }

  /** POSTs `body` to the bot's `path`; 5 s at most. */
  private async post(path: string, body: unknown): Promise<ControlResult> {
    const abort = new AbortController();
    const timer = setTimeout(() => abort.abort(), 5000);
    try {
      const r = await fetch(`${this.link.url}${path}`, {
        method: "POST",
        headers: { ...this.headers(), "content-type": "application/json" },
        body: JSON.stringify(body),
        signal: abort.signal,
      });
      let out: unknown = null;
      try {
        out = await r.json();
      } catch {
        out = null;
      }
      const err = (out as { error?: unknown } | null)?.error;
      return {
        ok: r.ok,
        status: r.status,
        body: out,
        error: r.ok ? undefined : typeof err === "string" ? err : r.status === 401 ? "the bot refused the dashboard's login" : `HTTP ${r.status}`,
      };
    } catch (e) {
      return { ok: false, status: 0, body: null, error: abort.signal.aborted ? "the bot did not answer in 5 s" : "the bot is not reachable" };
    } finally {
      clearTimeout(timer);
    }
  }

  /** Trading on (new launches are bought) or off (no new buys; open positions are still sold). */
  setTrading(enabled: boolean): Promise<ControlResult> {
    return this.post("/api/trading", { enabled });
  }

  /** The buy size in SOL, from the next buy (the bot checks it against MAX_RUNTIME_BUY_SOL and saves it to .env). */
  setBuySize(sol: number): Promise<ControlResult> {
    return this.post("/api/buy-size", { sol });
  }
}
