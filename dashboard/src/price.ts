// SOL/USD for the dashboard's USD values (on hover), fetched by the server (the page may only talk
// to its own origin). Daily closes (the Trades calendar): Kraken's daily candles, Binance's when
// Kraken fails. Hourly closes (a trade's hour, the last 720 hours): Kraken's hourly candles, else
// the day's close, else the latest price. Each series is kept in memory for 10 minutes (a failure
// for 1 minute, so a down API isn't asked on every refresh); each request times out after 5 s. A
// failure gives no prices, never an error: the page shows SOL without USD.

/** The part of `fetch` used here; tests pass a fake one (they never reach the network). */
export type Fetcher = (
  url: string,
  init: { signal: AbortSignal },
) => Promise<{ ok: boolean; status: number; json(): Promise<unknown> }>;

export type PriceSource = "kraken" | "binance";

/** Closes of one series by candle start: "YYYY-MM-DD" (daily) or "YYYY-MM-DDTHH" (hourly, UTC). */
export interface Closes {
  closes: Record<string, number>;
  source: PriceSource;
  /** The newest candle's key; its close is the latest price. */
  newest: string;
}

type Series = "daily" | "hourly";

export const KRAKEN_URL = "https://api.kraken.com/0/public/OHLC?pair=SOLUSD&interval=1440";
export const KRAKEN_HOURLY_URL = "https://api.kraken.com/0/public/OHLC?pair=SOLUSD&interval=60";
export const BINANCE_URL = "https://api.binance.com/api/v3/klines?symbol=SOLUSDT&interval=1d&limit=1000";
const CACHE_MS = 10 * 60 * 1000;
const RETRY_MS = 60 * 1000;
const DAY_MS = 86400000;

const utcDay = (ms: number) => new Date(ms).toISOString().slice(0, 10);
const utcHour = (ms: number) => new Date(ms).toISOString().slice(0, 13);

/** Candle rows as closes by `key` of their start; throws if no row is usable. `at`: the start (ms). */
function closesOf(
  rows: unknown,
  at: (r: unknown[]) => number,
  close: (r: unknown[]) => number,
  key: (ms: number) => string,
): Record<string, number> {
  const out: Record<string, number> = {};
  for (const r of Array.isArray(rows) ? rows : []) {
    if (!Array.isArray(r)) continue;
    const t = at(r);
    const c = close(r);
    if (Number.isFinite(t) && t > 0 && Number.isFinite(c) && c > 0) out[key(t)] = c;
  }
  if (!Object.keys(out).length) throw new Error("no candles in the answer");
  return out;
}

/** Kraken: `{ error: [], result: { <pair>: [[time s, open, high, low, close, vwap, volume, count], ...], last } }`. */
export function parseKraken(body: any, key: (ms: number) => string = utcDay): Record<string, number> {
  if (!body || typeof body !== "object") throw new Error("kraken: unreadable answer");
  if (Array.isArray(body.error) && body.error.length) throw new Error(`kraken: ${body.error.join("; ")}`);
  const rows = Object.entries(body.result ?? {}).find(([k, v]) => k !== "last" && Array.isArray(v))?.[1];
  return closesOf(rows, (r) => Number(r[0]) * 1000, (r) => Number(r[4]), key);
}

/** Binance: `[[open time ms, open, high, low, close, volume, close time, ...], ...]`. */
export function parseBinance(body: unknown): Record<string, number> {
  return closesOf(body, (r) => Number(r[0]), (r) => Number(r[4]), utcDay);
}

const SOURCES: Record<Series, readonly [PriceSource, string, (body: any) => Record<string, number>][]> = {
  daily: [
    ["kraken", KRAKEN_URL, (b) => parseKraken(b)],
    ["binance", BINANCE_URL, parseBinance],
  ],
  hourly: [["kraken", KRAKEN_HOURLY_URL, (b) => parseKraken(b, utcHour)]],
};

export class SolPrices {
  fetcher: Fetcher;
  timeoutMs: number;
  now: () => number;
  private cached = new Map<Series, { at: number; value: Closes | null }>();
  private pending = new Map<Series, Promise<Closes | null>>();

  constructor(fetcher: Fetcher = fetch, opts: { timeoutMs?: number; now?: () => number } = {}) {
    this.fetcher = fetcher;
    this.timeoutMs = opts.timeoutMs ?? 5000;
    this.now = opts.now ?? Date.now;
  }

  /** Every daily close the source has, from the cache while fresh; null if no source answered. */
  daily(): Promise<Closes | null> {
    return this.series("daily");
  }

  /** The hourly closes (Kraken: the last 720 hours); null if it didn't answer. */
  hourly(): Promise<Closes | null> {
    return this.series("hourly");
  }

  /**
   * SOL/USD at a time (ms): the close of its hour, else of its UTC day, else the latest price; null
   * when no price could be fetched. The current hour's and day's close is the latest price.
   */
  async lookup(): Promise<(ms: number) => number | null> {
    const [h, d] = await Promise.all([this.hourly(), this.daily()]);
    const latest = (h ? h.closes[h.newest] : d ? d.closes[d.newest] : undefined) ?? null;
    return (ms) => (Number.isFinite(ms) ? h?.closes[utcHour(ms)] ?? d?.closes[utcDay(ms)] : undefined) ?? latest;
  }

  /**
   * The closes of `month` ("YYYY-MM"), today's being the latest price; none (source null) if no
   * source answered. Just after 00:00 UTC (or from a cache filled before) the newest candle can
   * be yesterday's: its close is then today's latest price.
   */
  async month(month: string): Promise<{ prices: Record<string, number>; source: PriceSource | null }> {
    const d = await this.daily();
    if (!d) return { prices: {}, source: null };
    const prices = Object.fromEntries(Object.entries(d.closes).filter(([day]) => day.startsWith(`${month}-`)));
    const now = this.now();
    const today = utcDay(now);
    if (today.startsWith(`${month}-`) && !(today in prices) && d.newest === utcDay(now - DAY_MS)) {
      prices[today] = d.closes[d.newest]!;
    }
    return { prices, source: d.source };
  }

  /** One series, from the cache while fresh; one fetch at a time (requests meanwhile wait for it). */
  private series(kind: Series): Promise<Closes | null> {
    const c = this.cached.get(kind);
    if (c && this.now() - c.at < (c.value ? CACHE_MS : RETRY_MS)) return Promise.resolve(c.value);
    let p = this.pending.get(kind);
    if (!p) {
      p = this.load(kind).then((value) => {
        this.cached.set(kind, { at: this.now(), value });
        this.pending.delete(kind);
        return value;
      });
      this.pending.set(kind, p);
    }
    return p;
  }

  private async load(kind: Series): Promise<Closes | null> {
    for (const [source, url, parse] of SOURCES[kind]) {
      try {
        const closes = parse(await this.get(url));
        return { closes, source, newest: Object.keys(closes).sort().at(-1)! };
      } catch {
        // The next source, or none.
      }
    }
    return null;
  }

  /** One GET's JSON body; throws on an HTTP error or after `timeoutMs` (the request is aborted). */
  private async get(url: string): Promise<unknown> {
    const abort = new AbortController();
    let timer: ReturnType<typeof setTimeout> | undefined;
    const late = new Promise<never>((_, reject) => {
      timer = setTimeout(() => {
        abort.abort();
        reject(new Error(`no answer in ${this.timeoutMs} ms`));
      }, this.timeoutMs);
    });
    const answer = (async () => {
      const r = await this.fetcher(url, { signal: abort.signal });
      if (!r.ok) throw new Error(`HTTP ${r.status}`);
      return r.json();
    })();
    try {
      return await Promise.race([answer, late]);
    } finally {
      clearTimeout(timer);
    }
  }
}
