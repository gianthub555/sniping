// The wallet card's balance: `getBalance` of the bot's public key on a public RPC (no key of any
// kind is involved, and the URL carries no secret). Read at most every 30 s (a failure is asked
// again after 10 s); 5 s per request. Null until the first read succeeds.

export type RpcFetcher = (
  url: string,
  init: { method: string; headers: Record<string, string>; body: string; signal: AbortSignal },
) => Promise<{ ok: boolean; status: number; json(): Promise<unknown> }>;

const FRESH_MS = 30_000;
const RETRY_MS = 10_000;

export class Wallet {
  url: string;
  fetcher: RpcFetcher;
  /** The bot's public key (from its state; DASH_WALLET when the bot has never been seen). */
  address: string | null;
  private last: { address: string; lamports: number; at_ms: number } | null = null;
  private tried = 0;
  private pending: Promise<void> | null = null;

  constructor(url: string, fetcher: RpcFetcher = fetch as unknown as RpcFetcher, address = process.env.DASH_WALLET ?? null) {
    this.url = url;
    this.fetcher = fetcher;
    this.address = address && /^[1-9A-HJ-NP-Za-km-z]{32,44}$/.test(address) ? address : null;
  }

  /** The newest balance of `address` (a fresh read when the last is 30 s old). */
  async read(now = Date.now()): Promise<{ lamports: number; at_ms: number } | null> {
    const a = this.address;
    if (!a) return null;
    const have = this.last?.address === a ? this.last : null;
    const due = !have ? now - this.tried > RETRY_MS || this.tried === 0 : now - have.at_ms > FRESH_MS && now - this.tried > RETRY_MS;
    if (due) {
      this.pending ??= this.fetch(a, now).finally(() => (this.pending = null));
      await this.pending;
    }
    const got = this.last?.address === a ? this.last : null;
    return got ? { lamports: got.lamports, at_ms: got.at_ms } : null;
  }

  private async fetch(address: string, now: number): Promise<void> {
    this.tried = now;
    const abort = new AbortController();
    const timer = setTimeout(() => abort.abort(), 5000);
    try {
      const r = await this.fetcher(this.url, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "getBalance", params: [address, { commitment: "confirmed" }] }),
        signal: abort.signal,
      });
      if (!r.ok) return;
      const body = (await r.json()) as { result?: { value?: unknown } };
      const v = body?.result?.value;
      if (typeof v === "number" && Number.isSafeInteger(v) && v >= 0) this.last = { address, lamports: v, at_ms: Date.now() };
    } catch {
      // Kept: the last read (shown with its time), or none.
    } finally {
      clearTimeout(timer);
    }
  }
}
