// bigboss dashboard (the copy bot dashboard's front end, adapted): status bar, controls (trading
// on/off, buy size), overview, wallet card and the daily calendar on every page; open positions
// (the bot's, every 2 s) and the trade history (one row per coin, its every order in a side panel)
// on Trades; Snipe; Live activity; Rejections; Runs; the Audit log. Paged tables refresh every
// 10 s: on page 1 the rows are replaced, on page 2+ a "N new — back to top" pill appears instead.
// The tab, table page, rows and filters live in the URL query, so a reload keeps the view.
// Paper and live are never shown together: the Mode filter is one of them.

const $ = (id) => document.getElementById(id);
let csrf = "";
let last = null;
const LAMPORTS = 1e9;
const ROWS = [10, 25, 50, 100];
const DEFAULT_ROWS = 25;
const TABS = ["trades", "snipe", "activity", "rejections", "runs", "audit"];
/** Defaults of the URL filters; a value equal to its default is left out of the URL. */
const DEFAULTS = {
  show: "all",
  venue: "all",
  run: "",
  hours: "24",
  sort: "last_active",
  dir: "desc",
  deployer: "",
  kind: "all",
  verdict: "all",
};
let tab = "trades";
/** Run shown: "" = the current run (the newest start), "all", or a run id. */
let run = "";
/** Mode chosen in the URL (else the bot's); set once the status is known. */
let modeFromUrl = null;
// The viewer picked the mode by hand (the dropdown): only then does the URL keep it (pin=1); otherwise
// the page follows the bot's mode, so a link saved while the bot ran paper shows live once it is live.
let modePinned = false;
let deployer = "";

const sol = (lamports) => (lamports == null ? "–" : (lamports / LAMPORTS).toFixed(6));
const signedSol = (l) => (l == null ? "–" : `${l >= 0 ? "+" : "−"}${Math.abs(l / LAMPORTS).toFixed(6)}`);
const pct = (x) => {
  if (x == null || !isFinite(x)) return { text: "–", cls: "" };
  const up = x >= 0;
  return { text: `${up ? "▲" : "▼"} ${Math.abs(x * 100).toFixed(2)}%`, cls: up ? "gain" : "loss" };
};
const compact = (n) => {
  const a = Math.abs(n);
  if (a >= 1e9) return (n / 1e9).toFixed(1) + "B";
  if (a >= 1e6) return (n / 1e6).toFixed(1) + "M";
  if (a >= 1e3) return (n / 1e3).toFixed(1) + "K";
  return String(Math.round(n));
};
const when = (ms) => (ms ? new Date(ms).toLocaleString("en-US") : "–");
const count = (n) => Number(n ?? 0).toLocaleString("en-US");
const share = (x) => (x == null || !isFinite(x) ? "–" : `${(x * 100).toFixed(1)}%`);
const signedNum = (x, digits) => (x == null || !isFinite(x) ? "–" : `${x >= 0 ? "+" : "−"}${Math.abs(x).toFixed(digits)}`);
const age = (ms) => {
  if (!ms) return "";
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 90) return `${s}s`;
  if (s < 5400) return `${Math.round(s / 60)}m`;
  if (s < 172800) return `${(s / 3600).toFixed(1)}h`;
  return `${(s / 86400).toFixed(1)}d`;
};
/** Lamports as SOL, exactly, without trailing zeros (4500000 → "0.0045"). */
const solExact = (l) => {
  const s = String(Math.round(l)).padStart(10, "0");
  const frac = s.slice(-9).replace(/0+$/, "");
  return s.slice(0, -9) + (frac ? `.${frac}` : "");
};
/** A day's net on its tile: sign, at most 4 decimals (a tiny one: its first significant digit). */
const tileSol = (l) => {
  if (!l) return "0";
  const x = Math.abs(l) / LAMPORTS;
  let s = x.toFixed(4);
  if (Number(s) === 0) s = x.toFixed(Math.min(9, Math.ceil(-Math.log10(x))));
  return `${l > 0 ? "+" : "−"}${s.replace(/\.?0+$/, "")}`;
};
const usdText = (x) => `$${Math.abs(x).toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
const signedUsd = (x) => `${x >= 0 ? "+" : "−"}${usdText(x)}`;
const shortAddr = (a) => (a && a.length > 10 ? `${a.slice(0, 4)}…${a.slice(-4)}` : a || "–");
const VENUE_LABEL = { pumpfun: "pump.fun", pumpswap: "PumpSwap" };
const venueText = (v) => VENUE_LABEL[v] ?? v ?? "–";
const el = (tag, cls, text) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
};
const gainCls = (x) => (x > 0 ? "gain" : x < 0 ? "loss" : "");
const signedPct = (x) => (x == null || !isFinite(x) ? "–" : `${signedNum(x * 100, 2)}%`);
const mcText = (l) => {
  if (l == null) return "–";
  const x = l / LAMPORTS;
  return `${x >= 1000 ? compact(x) : x.toFixed(x < 1 ? 3 : 1)} SOL`;
};
const span = (ms) => {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
};
const utcTime = (ms) => {
  const s = new Date(ms).toISOString();
  return `${s.slice(5, 7)}/${s.slice(8, 10)} ${s.slice(11, 19)}`;
};
function usdTitle(l, px, signed) {
  if (l == null) return "";
  if (px == null) return "USD price unavailable";
  const x = (l / LAMPORTS) * px;
  return `≈ ${signed ? signedUsd(x) : usdText(x)}`;
}
const link = (text, href, title) => {
  const a = el("a", "", text);
  a.href = href;
  a.target = "_blank";
  a.rel = "noopener noreferrer";
  if (title) a.title = title;
  return a;
};

function tokenCell(mint, symbol, sub) {
  const td = el("td", "token");
  if (!mint) {
    td.textContent = "–";
    return td;
  }
  td.append(link(symbol || `${mint.slice(0, 4)}…${mint.slice(-4)}`, `https://gmgn.ai/sol/token/${mint}`, `${mint} on gmgn.ai`), " ");
  const p = link("pump", `https://pump.fun/coin/${mint}`, `${mint} on pump.fun`);
  p.className = "pump";
  td.append(p);
  if (sub) td.append(el("div", "sub", sub));
  return td;
}

function addrCell(addr) {
  const td = el("td", "muted");
  if (!addr) {
    td.textContent = "–";
    return td;
  }
  td.append(link(shortAddr(addr), `https://gmgn.ai/sol/address/${addr}`, addr));
  return td;
}

function num(text, cls) {
  return el("td", `num ${cls || ""}`, text);
}

async function api(path, opts = {}) {
  const r = await fetch(path, { credentials: "same-origin", ...opts });
  if (r.status === 401) {
    location.href = "/login";
    throw new Error("logged out");
  }
  return r.json();
}

function toast(text, bad) {
  const t = $("toast");
  t.textContent = text;
  t.className = `toast ${bad ? "bad" : ""}`;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => (t.className = "toast hidden"), 5000);
}

function confirmBox(title, text, okLabel) {
  return new Promise((resolve) => {
    $("modal-title").textContent = title;
    $("modal-text").textContent = text;
    $("modal-ok").textContent = okLabel;
    $("modal").className = "modal";
    const close = (v) => {
      $("modal").className = "modal hidden";
      $("modal-ok").onclick = $("modal-cancel").onclick = null;
      resolve(v);
    };
    $("modal-ok").onclick = () => close(true);
    $("modal-cancel").onclick = () => close(false);
    $("modal-ok").focus();
  });
}

const modeName = (m) => (m === "live" ? "LIVE" : "paper");

// ---- The controls: trading on/off and the buy size (each confirmed, each in the audit log).

async function post(action, body) {
  const r = await api(`/api/control/${action}`, {
    method: "POST",
    headers: { "content-type": "application/json", "x-csrf-token": csrf },
    body: JSON.stringify(body ?? {}),
  });
  return r.ok ? null : r.error || "failed";
}

async function trading(on) {
  const b = last?.bot;
  const mode = modeName(b?.mode);
  const ok = await confirmBox(
    on ? "Start trading?" : "Stop trading?",
    on
      ? `The bot buys new launches from now on, in ${mode} mode, ${b?.buy_sol ?? "?"} SOL each, under its rules. It does not change paper/live.`
      : "No new buys from now on. Open positions are still managed and sold by the bot's exit rule.",
    on ? "Start trading" : "Stop trading",
  );
  if (!ok) return;
  try {
    const err = await post(on ? "trading-on" : "trading-off");
    toast(err ? `Failed: ${err}` : on ? "Trading started." : "Trading stopped.", !!err);
  } catch (e) {
    toast(`Failed: ${e.message}`, true);
  }
  refreshStatus();
}

/** The typed size as whole lamports, from its digits, or why it can't be sent. */
function parseSize(text) {
  const v = text.trim().replace(",", ".");
  if (v === "") return { error: "Type a size in SOL, e.g. 0.05." };
  if (v.startsWith("-")) return { error: "The size must be above 0." };
  const m = /^([0-9]*)(?:\.([0-9]*))?$/.exec(v);
  if (!m || (!m[1] && !m[2])) return { error: "Not a number: digits and one decimal point, e.g. 0.05." };
  const frac = m[2] ?? "";
  if (frac.length > 9) return { error: "At most 9 decimals (1 lamport = 0.000000001 SOL)." };
  const whole = m[1].replace(/^0+/, "");
  if (whole.length > 6) return { error: "Too large." };
  const lamports = Number(whole || "0") * LAMPORTS + Number(frac.padEnd(9, "0"));
  if (lamports <= 0) return { error: "The size must be above 0." };
  return { lamports, sol: `${whole || "0"}${frac ? `.${frac}` : ""}` };
}

function sizeError(text) {
  const e = $("size-error");
  e.textContent = text || "";
  e.hidden = !text;
  $("size-input").setAttribute("aria-invalid", String(!!text));
}

async function setSize(e) {
  e.preventDefault();
  const b = last?.bot;
  if (!b?.up) return sizeError("The bot is not answering: the size can't be set now.");
  const p = parseSize($("size-input").value);
  if (p.error) return sizeError(p.error);
  if (b.max_buy_sol != null && p.lamports > b.max_buy_sol * LAMPORTS)
    return sizeError(`At most ${b.max_buy_sol} SOL (the bot's limit).`);
  sizeError("");
  const w = last?.wallet;
  const more = b.mode === "live" && w?.lamports != null && p.lamports > w.lamports ? ` That is more than the wallet holds (${sol(w.lamports)} SOL).` : "";
  const ok = await confirmBox(
    `Set the buy size to ${solExact(p.lamports)} SOL?`,
    `It applies from the next buy, in ${modeName(b.mode)} mode (the bot also saves it to its .env).${more}`,
    "Set size",
  );
  if (!ok) return;
  $("size-set").disabled = true;
  try {
    const err = await post("set-size", { sol: p.sol });
    toast(err ? `Failed: ${err}` : `Buy size: ${solExact(p.lamports)} SOL from the next buy.`, !!err);
  } catch (e2) {
    toast(`Failed: ${e2.message}`, true);
  }
  $("size-set").disabled = false;
  $("size-input").blur();
  refreshStatus();
}

/** The buy size row: the bot's size; the typed text is kept while the field has focus or shows an error. */
function renderControls(s) {
  const b = s.bot;
  const input = $("size-input");
  const up = !!b?.up;
  const test = up ? b.snipe_test : null;
  const keep = !test && (document.activeElement === input || !$("size-error").hidden);
  if (!keep) input.value = up ? solExact(Math.round((test ? test.sol : b.buy_sol) * LAMPORTS)) : "";
  input.disabled = !up || !!test;
  input.readOnly = !!test;
  $("size-set").disabled = !up || !!test;
  $("size-set").hidden = !!test;
  $("size-note").textContent = test
    ? `Snipe test: the size is SNIPE_TEST_SOL in the bot's .env, set by hand (read-only here). Caps: ${test.max_per_day} buys a day, ${test.max_per_hour} an hour, ${test.max_loss_sol} SOL loss a day.`
    : up
      ? `Applies from the next buy (${modeName(b.mode)} mode). At most ${b.max_buy_sol} SOL.`
      : "The bot is not answering: trading and the size can't be changed now.";
  const st = $("trading-state");
  st.textContent = up ? (b.trading ? "Trading ON" : "Trading paused") : "Trading –";
  st.className = `trading-state ${up ? (b.trading ? "gain" : "warn") : "muted"}`;
  for (const btn of document.querySelectorAll("[data-action]")) {
    btn.disabled = !up || (btn.dataset.action === "trading-on" ? b.trading : !b.trading);
  }
}

/** The wallet card: the balance by the public key, its USD, the address; never a mode switch. */
function renderWallet(w, b) {
  const box = $("wallet");
  box.textContent = "";
  box.hidden = !w;
  if (!w) return;
  box.append(el("span", "label", "Wallet "));
  if (w.lamports == null) box.append(el("span", "value", "balance not read yet"));
  else {
    const v = el("span", "value", `${sol(w.lamports)} SOL`);
    box.append(v);
    if (w.usd_per_sol != null) box.append(el("span", "label", ` ≈ ${usdText((w.lamports / LAMPORTS) * w.usd_per_sol)}`));
  }
  box.append(el("span", "label", " · "), link(shortAddr(w.address), `https://gmgn.ai/sol/address/${w.address}`, `${w.address} on gmgn.ai`));
  if (w.at_ms) box.append(el("span", "label", ` · read ${age(w.at_ms)} ago`));
  if (w.usd_per_sol != null) box.append(el("span", "label", ` · SOL $${w.usd_per_sol.toFixed(2)}`));
  if (b?.simulates_as)
    box.append(el("div", "muted small", `Paper buys are simulated as ${shortAddr(b.simulates_as)} (DRY_RUN_WALLET); this card shows the bot's own wallet.`));
}

// ---- The overview: totals for the Trades page's filters, as the calendar's.

const overview = { seq: 0 };

function filters() {
  return { mode: $("mode").value, venue: $("venue").value, run: selectedRun(), deployer };
}

async function loadOverview() {
  const seq = ++overview.seq;
  let d;
  try {
    d = await api(`/api/overview?${new URLSearchParams(filters())}`);
  } catch {
    return;
  }
  if (seq === overview.seq && !d.error) renderOverview(d);
}

function ovTile(key, text, cls, title) {
  const dd = $(`ov-${key}`);
  dd.className = cls || "";
  dd.textContent = text;
  if (/^[+−]?[0-9]/.test(text) && cls != null) dd.append(el("span", "ov-unit", " SOL"));
  $(`ov-${key}-tile`).title = title;
}

function scopeText(d) {
  return `· ${d.run === "all" ? "all runs" : d.run} · ${d.mode === "live" ? "Live" : "Paper"}${d.venue && d.venue !== "all" ? ` · ${venueText(d.venue)}` : ""}${deployer ? ` · ${shortAddr(deployer)}` : ""}`;
}

function renderOverview(d) {
  $("overview-scope").textContent = scopeText(d);
  const money = (key, l, what) => {
    if (l == null) return ovTile(key, "–", "", what);
    ovTile(key, signedSol(l), gainCls(l), d.usd_now == null ? what : `${what} ${usdTitle(l, d.usd_now, true)} at the latest SOL price.`);
  };
  const stopped = d.unrealized == null ? " Unknown while the bot is not answering." : "";
  const unvalued = d.unvalued ? ` ${count(d.unvalued)} open position${d.unvalued === 1 ? " has" : "s have"} no value now (the bot doesn't hold it).` : "";
  money("total", d.total, `Realized + unrealized.${stopped}`);
  money("realized", d.realized, `Closed positions' wallet change (rent locked in them: ${sol(d.rent_closed)} SOL, included).`);
  money("unrealized", d.unrealized, `Open positions: their value now minus the cost of the tokens they hold.${stopped}${unvalued}`);
  money("today", d.today, `The wallet's change today (UTC): every order's wallet change, as the calendar's tile. ${count(d.today_trades)} positions closed today.`);
  ovTile("rent", d.today_rent ? `−${sol(d.today_rent)}` : "0", d.today_rent ? "loss" : "", "Token-account rent today's buys locked (inside Today's change; the accounts are not closed, so it is not refunded).");
  ovTile("winrate", d.win_rate == null ? "–" : share(d.win_rate), null, `${count(d.wins)} of ${count(d.trades)} closed positions won.`);
  ovTile("trades", count(d.trades), null, "Closed positions.");
  ovTile("open", count(d.open), null, "Open positions.");
}

// ---- The status bar.

function renderStatus(s) {
  const bar = $("statusbar");
  bar.textContent = "";
  const item = (label, value, cls, title) => {
    const d = el("span", `stat ${cls || ""}`);
    d.append(el("span", "label", label), el("span", "value", value));
    if (title) d.title = title;
    bar.append(d);
  };
  const b = s.bot;
  item("Bot", b.up ? (b.trading ? "trading" : "paused") : "not answering", b.up ? (b.trading ? "ok" : "warn") : "bad",
    b.up ? "The bot's local API answers (its event stream)" : `The bot's local API does not answer${b.error ? `: ${b.error}` : ""}`);
  item("Mode", b.up ? (b.mode === "live" ? "● Live" : "○ Paper") : "–", b.mode === "live" ? "warn" : "");
  const sn = s.latency?.snipe;
  item("Snipe p99", sn ? `${count(sn.p99)} µs` : "–", "",
    sn ? `Launch frame received → our buy signed, last ${sn.n} signed buys: p50 ${count(sn.p50)} µs, p99 ${count(sn.p99)} µs (bundle window included)` : "No signed buy in the hot path's stamps yet");
  const fe = s.latency?.feed;
  item("Feed p99", fe ? `${count(fe.p99)} µs` : "–", "",
    fe ? `Frame received → matched as a launch, last ${fe.n} frames: p50 ${count(fe.p50)} µs, p99 ${count(fe.p99)} µs` : "No hot-path stamps yet");
  const sh = b.up ? b.shreds : null;
  const gaps = sh ? Number(sh.incomplete_slots ?? 0) : null;
  item("Gaps", gaps == null ? "–" : count(gaps), gaps ? "warn" : "",
    sh ? `The in-process shred feed (${sh.status}): slots with missing shreds ${count(sh.incomplete_slots)}, bad batches ${count(sh.bad_batches)}, recovered shreds ${count(sh.recovered_shreds)}` : "No in-process shred feed (ENGINE_INPROC off) or the bot is not answering");
  if (b.up && b.snipe_test) {
    const t = b.snipe_test;
    const capped = t.today >= t.max_per_day || t.this_hour >= t.max_per_hour || t.pnl_today_sol <= -t.max_loss_sol;
    item("Snipe test", `${t.today}/${t.max_per_day} today · ${t.this_hour}/${t.max_per_hour} h · ${t.pnl_today_sol >= 0 ? "+" : "−"}${Math.abs(t.pnl_today_sol).toFixed(4)} SOL`,
      capped ? "warn" : "ok",
      `SNIPE_TEST=1: ${t.sol} SOL a buy. Today (UTC): ${t.today} buys of ${t.max_per_day}, ${t.this_hour} this hour of ${t.max_per_hour}, realized ${t.pnl_today_sol.toFixed(6)} SOL (stops at −${t.max_loss_sol})${capped ? ". A cap is reached: no new entries." : ""}`);
  }
  if (b.up) {
    item("Open", `${b.open_positions}/${b.max_open_positions}`);
    item("Uptime", span(b.uptime_s * 1000), "", `Up since ${when(Date.now() - b.uptime_s * 1000)}`);
  }
  const st = s.store_stats;
  item("Store drops", st ? count(st.dropped) : "–", st?.dropped || st?.errors ? "bad" : st ? "ok" : "",
    st ? `The store's writer (as of ${age(st.at_ms)} ago): queued ${count(st.enqueued)}, written ${count(st.written)}, dropped ${count(st.dropped)}, errors ${count(st.errors)}` : "The bot has not reported its store counters (STORE_DB off, or a build before them)");
  // Red only for a real problem (the bot not answering); a store not created yet is a note.
  const alert = $("alert");
  if (!b.up) {
    alert.className = "alert bad";
    alert.textContent = "The bot is not answering on its local API: live values (open positions, trading, buy size) are unknown.";
  } else if (!s.store) {
    alert.className = "alert note";
    alert.textContent = "No trade history to show yet: the bot's database (data/bigboss.sqlite) has not been created.";
  }
  alert.hidden = b.up && s.store;
  renderControls(s);
  renderWallet(s.wallet, b);
}

function renderOpen(s) {
  const body = $("open").tBodies[0];
  body.textContent = "";
  const rows = s.bot?.positions ?? [];
  $("open-empty").hidden = rows.length > 0;
  $("open-empty").textContent = s.bot?.up ? "No open positions" : "Unknown: the bot is not answering";
  for (const p of rows) {
    const tr = el("tr");
    const sub = [p.paper ? "paper" : "LIVE", p.take_profit_armed ? "take-profit armed" : "", p.ladder_step ? `step ${p.ladder_step}` : ""].filter(Boolean).join(" · ");
    tr.append(tokenCell(p.mint, s.symbols?.[p.mint] || s.names?.[p.mint], sub));
    tr.append(el("td", "", /swap/i.test(p.dex) ? "PumpSwap" : "pump.fun"));
    tr.append(num(compact(p.token_amount / 1e6)));
    tr.append(num(p.entry_sol.toFixed(6)));
    tr.append(num(p.value_sol == null ? "–" : p.value_sol.toFixed(6)));
    const pp = pct(p.pnl_percent == null ? null : p.pnl_percent / 100);
    tr.append(num(pp.text, pp.cls));
    const pk = pct(p.peak_pnl_percent == null ? null : p.peak_pnl_percent / 100);
    tr.append(num(pk.text, pk.cls));
    tr.append(num(`${p.sold_percent.toFixed(0)}%`));
    tr.append(num(span(Date.now() - p.opened_at * 1000)));
    body.append(tr);
  }
}

// ---- Trade history: one row per coin. SOL in the cells, USD at the trade's hour on hover; the
// headers with arrows sort (on the server). A click opens the coin's orders in the side panel.

const OPEN_STATES = ["OPEN", "SELL_PENDING", "BUY_PENDING", "STUCK"];
const SORTS = ["last_active", "realized", "total", "balance", "bought", "sold"];
const STATE_LABEL = { OPEN: "open", SELL_PENDING: "selling", BUY_PENDING: "buying", STUCK: "stuck", BUY_FAILED: "failed" };

function svgIcon(d) {
  const ns = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(ns, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  const path = document.createElementNS(ns, "path");
  path.setAttribute("d", d);
  svg.append(path);
  return svg;
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const t = el("textarea", "offscreen");
    t.value = text;
    document.body.append(t);
    t.select();
    document.execCommand("copy");
    t.remove();
  }
  toast("Copied");
}

function copyButton(mint) {
  const b = el("button", "icon-btn");
  b.type = "button";
  b.title = "Copy the mint";
  b.setAttribute("aria-label", `Copy the mint ${mint}`);
  b.append(svgIcon("M9 9h11v11H9z M5 15H4V4h11v1"));
  b.onclick = (e) => {
    e.stopPropagation();
    copyText(mint);
  };
  return b;
}

function profit(l, p, px) {
  const td = num(signedSol(l), l == null ? "" : gainCls(l));
  if (l == null) return td;
  td.title = usdTitle(l, px, true);
  td.append(el("div", "sub tone", signedPct(p)));
  return td;
}

function fillCell(l, mc, px) {
  const td = num(sol(l));
  td.title = usdTitle(l, px);
  if (mc != null) {
    const s = el("div", "sub", `MC ${mcText(mc)}`);
    s.title = usdTitle(mc, px);
    td.append(s);
  }
  return td;
}

function renderHistory(h) {
  const body = $("history").tBodies[0];
  body.textContent = "";
  $("history-empty").hidden = h.rows.length > 0;
  $("history-empty").textContent = h.available === false ? "No store yet" : "No trades for these filters";
  const s = h.summary;
  $("history-summary").textContent = s
    ? `${h.mode === "live" ? "Live" : "Paper"} · closed ${count(s.trades)}, won ${count(s.wins)} · net ${signedSol(s.net)} SOL (won ${sol(s.gross_win)}, lost ${sol(s.gross_loss)}) · rent locked in them ${sol(s.rent)} SOL`
    : "";
  for (const r of h.rows) {
    const open = OPEN_STATES.includes(r.state);
    const tr = el("tr", "row");
    tr.tabIndex = 0;
    tr.dataset.id = r.id;
    tr.setAttribute("aria-haspopup", "dialog");
    const tok = el("td", "token");
    tok.append(link(r.symbol || `${r.mint.slice(0, 4)}…${r.mint.slice(-4)}`, `https://gmgn.ai/sol/token/${r.mint}`, `${r.mint} on gmgn.ai`));
    const ago = el("span", "muted small", span(Date.now() - r.last_active_ms));
    ago.title = `Last active ${when(r.last_active_ms)}`;
    tok.append(" ", copyButton(r.mint), " ", ago);
    const tags = [
      venueText(r.venue),
      STATE_LABEL[r.state] ?? "",
      r.exit_reason && !open ? r.exit_reason : "",
      r.sells > 1 ? `${r.sells} sell steps` : "",
    ].filter(Boolean).join(" · ");
    const sub = el("div", "sub", tags);
    if (r.deployer) {
      const d = el("span", "", ` · dev ${shortAddr(r.deployer)}`);
      d.title = `Deployer ${r.deployer}${r.follow_kind ? ` (followed: ${r.follow_kind})` : ""}`;
      sub.append(d);
    }
    tok.append(sub);
    tr.append(tok);
    const un = num(open ? signedSol(r.unrealized) : "–", open ? gainCls(r.unrealized) : "");
    if (open) un.title = r.unrealized == null ? "No value now: the bot doesn't hold this position" : usdTitle(r.unrealized, h.usd_now, true);
    tr.append(un);
    tr.append(profit(r.realized, r.realized_pct, r.usd_last));
    const tot = profit(r.total, r.total_pct, open ? h.usd_now : r.usd_last);
    if (r.rent_locked) tot.title = `${tot.title} · includes ${sol(r.rent_locked)} SOL token-account rent locked`;
    tr.append(tot);
    const bal = num(sol(r.value));
    bal.title = usdTitle(r.value, h.usd_now);
    bal.append(el("div", "sub", compact(r.tokens_held / 1e6)));
    tr.append(bal);
    tr.append(num(share(r.position_pct)));
    const from = r.bought_ms || r.opened_ms;
    const to = r.closed_ms || (open ? Date.now() : null);
    const held = num(to ? span(to - from) : "–");
    held.title = `${when(from)} → ${r.closed_ms ? when(r.closed_ms) : open ? "now" : "–"}`;
    tr.append(held);
    tr.append(fillCell(r.sol_in, r.mc_buy, r.usd_buy));
    tr.append(fillCell(r.sol_out, r.mc_sell, r.usd_sell));
    const txs = el("td", "num");
    txs.append(el("span", "gain", count(r.buys)), "/", el("span", "loss", count(r.sells)));
    txs.title = `${r.buys} landed buy${r.buys === 1 ? "" : "s"} / ${r.sells} landed sell${r.sells === 1 ? "" : "s"}`;
    tr.append(txs);
    tr.onclick = (e) => {
      if (!e.target.closest("a, button")) openOrders(r);
    };
    tr.onkeydown = (e) => {
      if (e.target === tr && (e.key === "Enter" || e.key === " ")) {
        e.preventDefault();
        openOrders(r);
      }
    };
    body.append(tr);
  }
}

function renderSort() {
  const t = tables.history;
  for (const b of document.querySelectorAll("#history .sort")) {
    const on = b.dataset.sort === t.sort;
    b.closest("th").setAttribute("aria-sort", on ? (t.dir === "asc" ? "ascending" : "descending") : "none");
    b.dataset.arrow = on ? (t.dir === "asc" ? "▲" : "▼") : "↕";
  }
}

function sortBy(key) {
  const t = tables.history;
  t.dir = t.sort === key && t.dir === "desc" ? "asc" : "desc";
  t.sort = key;
  renderSort();
  filtered(t);
}

// ---- The side panel: every order of a coin (each sell step), newest first.

const drawer = { id: null, seq: 0, timer: 0 };

async function openOrders(r) {
  const d = $("drawer");
  clearTimeout(drawer.timer);
  drawer.id = r.id;
  const seq = ++drawer.seq;
  $("drawer-title").textContent = r.symbol || `${r.mint.slice(0, 4)}…${r.mint.slice(-4)}`;
  const sub = $("drawer-sub");
  sub.textContent = "";
  sub.append(
    `${r.mint.slice(0, 6)}…${r.mint.slice(-4)} `,
    copyButton(r.mint),
    " · ",
    link("gmgn", `https://gmgn.ai/sol/token/${r.mint}`),
    " · ",
    link("pump.fun", `https://pump.fun/coin/${r.mint}`),
  );
  $("drawer-context").textContent = "";
  $("orders").tBodies[0].textContent = "";
  $("exits-block").hidden = true;
  $("orders-empty").hidden = false;
  $("orders-empty").textContent = "Loading…";
  d.hidden = false;
  void d.offsetWidth;
  d.classList.add("open");
  document.body.classList.add("drawer-open");
  $("drawer-close").focus();
  let o;
  try {
    o = await api(`/api/position/${r.id}/orders`);
  } catch {
    return;
  }
  if (seq !== drawer.seq) return;
  if (o.error) {
    $("orders-empty").textContent = `Failed: ${o.error}`;
    return;
  }
  renderOrders(o);
}

function closeOrders() {
  const d = $("drawer");
  if (!d.classList.contains("open")) return;
  drawer.seq++;
  d.classList.remove("open");
  document.body.classList.remove("drawer-open");
  drawer.timer = setTimeout(() => (d.hidden = true), 250);
  document.querySelector(`#history tr[data-id="${drawer.id}"]`)?.focus();
}

function trapFocus(e) {
  if (e.key !== "Tab") return;
  const f = [...$("drawer").querySelectorAll("a[href], button:not(:disabled)")];
  if (!f.length) return;
  if (e.shiftKey && document.activeElement === f[0]) {
    e.preventDefault();
    f.at(-1).focus();
  } else if (!e.shiftKey && document.activeElement === f.at(-1)) {
    e.preventDefault();
    f[0].focus();
  }
}

function renderOrders(d) {
  const p = d.position;
  const ctx = [
    `${p.mode === "live" ? "Live" : "Paper"} · ${venueText(p.venue)} · run ${p.run_id}`,
    p.deployer ? `deployer ${shortAddr(p.deployer)}${p.follow_kind ? ` (${p.follow_kind})` : ""}` : "",
    p.tier != null ? `fee tier ${p.tier}` : "",
    p.score != null ? `score ${p.score}` : "",
    p.rent_locked ? `rent locked ${sol(p.rent_locked)} SOL` : "",
  ].filter(Boolean);
  $("drawer-context").textContent = ctx.join(" · ");
  const body = $("orders").tBodies[0];
  body.textContent = "";
  $("orders-empty").hidden = d.orders.length > 0;
  $("orders-empty").textContent = "No landed or failed orders for this coin";
  for (const o of d.orders) {
    const buy = o.side === "buy";
    const cls = buy ? "gain" : "loss";
    const landed = o.status === "landed";
    const tr = el("tr", landed ? "" : "failed");
    const type = el("td");
    type.append(el("span", `badge ${buy ? "buy" : "sell"}`, buy ? "Buy" : o.sell_step ? `Sell ${o.sell_step}` : "Sell"));
    if (!landed) {
      const f = el("span", "tag loss", o.status === "timed_out" ? "timed out" : "failed");
      if (o.error) f.title = o.error;
      type.append(" ", f);
    }
    if (o.reason) type.append(" ", el("span", "tag", o.reason));
    const mc = num(mcText(o.mc), cls);
    mc.title = usdTitle(o.mc, o.usd);
    const amount = num(o.tokens == null ? "–" : compact(o.tokens / 1e6), cls);
    const total = num(landed ? sol(o.quote_net) : "–", cls);
    total.title = usdTitle(o.quote_net, o.usd);
    if (o.pnl_percent != null && !buy) total.append(el("div", "sub tone", signedNum(o.pnl_percent, 2) + "%"));
    const wd = num(signedSol(o.wallet_delta), gainCls(o.wallet_delta));
    wd.title = usdTitle(o.wallet_delta, o.usd, true);
    if (o.rent) wd.append(el("div", "sub", `rent ${sol(o.rent)}`));
    const fees = num(o.fees == null ? "–" : sol(o.fees));
    fees.title = "Network, priority and tip fees: the wallet change beyond the order's SOL (and a buy's rent)";
    const t = el("td", "time", utcTime(o.at_ms));
    const timing = [
      o.launch_to_landed_ms != null ? `launch → landed ${o.launch_to_landed_ms} ms` : "",
      o.detect_to_ready_ms != null ? `detect → ready ${Number(o.detect_to_ready_ms).toFixed(2)} ms` : "",
      o.landed_slot != null && o.signal_slot != null ? `+${o.landed_slot - o.signal_slot} slots` : "",
    ].filter(Boolean).join(" · ");
    if (timing) t.append(el("div", "sub", timing));
    if (o.sig) t.title = o.sig;
    tr.append(type, mc, amount, total, wd, fees, t);
    body.append(tr);
  }
  const eb = $("exits").tBodies[0];
  eb.textContent = "";
  $("exits-block").hidden = !d.exits?.length;
  for (const x of d.exits ?? []) {
    const tr = el("tr");
    tr.append(el("td", "time", utcTime(x.at_ms)), el("td", "", x.reason), num(share(x.share)), num(`×${Number(x.value_x).toFixed(3)}`),
      num(`×${Number(x.peak_x).toFixed(3)}`), num(`${Number(x.held_secs).toFixed(1)} s`), num(`${Number(x.net_behind_sol).toFixed(2)} SOL`));
    eb.append(tr);
  }
}

// ---- Calendar (every page): the wallet's change per UTC day of a month under the Trades page's
// filters; weeks start on Monday; the tooltip adds USD at the day's close, the rent line, the trades.

const utcMonth = (ms) => new Date(ms).toISOString().slice(0, 7);
const cal = { month: utcMonth(Date.now()), seq: 0, json: "" };

function shiftMonth(by) {
  const [y, m] = cal.month.split("-").map(Number);
  cal.month = utcMonth(Date.UTC(y, m - 1 + by, 1));
  loadCalendar();
}

async function loadCalendar() {
  const seq = ++cal.seq;
  let d;
  try {
    d = await api(`/api/calendar?${new URLSearchParams({ ...filters(), month: cal.month })}`);
  } catch {
    return;
  }
  if (seq !== cal.seq) return;
  if (d.error) return toast(`Failed: ${d.error}`, true);
  renderCalendar(d);
}

function renderCalendar(d) {
  const today = new Date().toISOString().slice(0, 10);
  const json = JSON.stringify(d) + today;
  if (json === cal.json) return;
  cal.json = json;
  const [y, m] = d.month.split("-").map(Number);
  const first = new Date(Date.UTC(y, m - 1, 1));
  const title = first.toLocaleString("en-US", { month: "short", year: "numeric", timeZone: "UTC" });
  $("cal-month").textContent = title;
  $("cal-scope").textContent = d.mode === "live" ? "Live" : "Paper";
  $("cal-next").disabled = d.month >= utcMonth(Date.now());
  const grid = $("cal");
  grid.setAttribute("aria-label", `Days of ${title}`);
  const focused = document.activeElement?.closest?.("#cal [data-day]")?.dataset.day;
  hideTip();
  grid.textContent = "";
  const byDay = new Map(d.days.map((x) => [x.day, x]));
  for (let i = 0; i < (first.getUTCDay() + 6) % 7; i++) {
    const li = el("li", "cal-blank");
    li.setAttribute("aria-hidden", "true");
    grid.append(li);
  }
  const days = new Date(Date.UTC(y, m, 0)).getUTCDate();
  for (let n = 1; n <= days; n++) {
    const day = `${d.month}-${String(n).padStart(2, "0")}`;
    const x = byDay.get(day);
    const result = x ? (x.net_lamports > 0 ? "gain" : x.net_lamports < 0 ? "loss" : "even") : "";
    const li = el("li");
    const tile = el(x ? "button" : "div", `cal-day ${result} ${day === today ? "today" : ""}`);
    tile.append(el("span", "cal-date", String(n)));
    if (x) tile.append(el("span", "cal-net", tileSol(x.net_lamports)));
    if (x) {
      tile.type = "button";
      tile.dataset.day = day;
      tile.setAttribute("aria-describedby", "cal-tip");
      tile.onmouseenter = tile.onfocus = tile.onclick = () => showTip(tile, tipContent(day, x, d, today));
      tile.onmouseleave = tile.onblur = hideTip;
    }
    if (day === today) tile.setAttribute("aria-current", "date");
    li.append(tile);
    grid.append(li);
  }
  if (focused) grid.querySelector(`[data-day="${focused}"]`)?.focus();
}

function tipContent(day, x, d, today) {
  const px = d.prices?.[day];
  const date = new Date(`${day}T00:00:00Z`).toLocaleDateString("en-US", { weekday: "short", month: "short", day: "numeric", year: "numeric", timeZone: "UTC" });
  const head = el("div", "tip-head");
  head.append(el("span", "tip-date", date), el("span", "unit", "SOL"));
  const cols = el("div", "tip-cols");
  const col = (label, value, usd) => {
    const c = el("div", "tip-col");
    c.append(el("div", "tip-label", label), value, el("div", "tip-usd", usd || " "));
    cols.append(c);
  };
  const na = "USD n/a";
  col("Day net", el("div", `tip-value ${gainCls(x.net_lamports)}`, signedSol(x.net_lamports)), px == null ? na : usdTitle(x.net_lamports, px, true));
  col("Rent locked", el("div", `tip-value ${x.rent_lamports ? "loss" : ""}`, x.rent_lamports ? `−${sol(x.rent_lamports)}` : sol(0)),
    px == null ? na : x.rent_lamports ? usdTitle(-x.rent_lamports, px, true) : "");
  const pair = (a, b, fmt) => {
    const v = el("div", "tip-value");
    v.append(el("span", "gain", fmt(a)), " / ", el("span", "loss", fmt(b)));
    return v;
  };
  col("Buy/Sell Txns", pair(x.buys, x.sells, count), x.failed ? `${count(x.failed)} failed` : "");
  col("Buy/Sell Volume", pair(x.buy_volume_lamports, x.sell_volume_lamports, sol),
    px == null ? na : `${usdTitle(x.buy_volume_lamports, px)} / ${usdText((x.sell_volume_lamports / LAMPORTS) * px)}`);
  const foot = [
    `Net without the rent ${signedSol(x.net_lamports + x.rent_lamports)}`,
    x.trades ? `${count(x.trades)} closed, ${count(x.wins)} won (profit ${sol(x.profit_lamports)}, loss ${sol(-x.loss_lamports)})` : "no position closed",
    px == null ? "no SOL/USD price" : `SOL $${px.toFixed(2)} ${day === today ? "now" : "close"}`,
  ];
  return [head, cols, el("div", "tip-foot", foot.join(" · "))];
}

function showTip(tile, nodes) {
  const tip = $("cal-tip");
  tip.textContent = "";
  tip.append(...nodes);
  tip.hidden = false;
  const r = tile.getBoundingClientRect();
  const w = tip.offsetWidth;
  const h = tip.offsetHeight;
  const vw = document.documentElement.clientWidth;
  const m = 8;
  let top = r.top - h - 6;
  if (top < m) top = Math.min(r.bottom + 6, innerHeight - h - m);
  tip.style.left = `${Math.max(m, Math.min(r.left + r.width / 2 - w / 2, vw - w - m))}px`;
  tip.style.top = `${Math.max(m, top)}px`;
}

function hideTip() {
  $("cal-tip").hidden = true;
}

// ---- Snipe.

const VERDICT_CLS = { bought: "gain", buy: "gain", buy_failed: "loss", skip: "muted", paused: "warn", seen: "muted" };
const VERDICT_LABEL = { bought: "bought", buy: "buy decided", buy_failed: "buy failed", skip: "skipped", paused: "paused", seen: "no verdict" };

function fillTable(id, rows, cells) {
  const body = $(id).tBodies[0];
  body.textContent = "";
  for (const r of rows) {
    const tr = el("tr");
    tr.append(...cells(r));
    body.append(tr);
  }
}

function renderSnipeStats(st) {
  if (!st) {
    $("snipe-summary").textContent = "No store yet";
    return;
  }
  const v = st.verdicts ?? {};
  const followed = Object.values(v).reduce((a, b) => a + b, 0);
  const seen = st.seen.reduce((a, r) => a + r.n, 0);
  const grads = st.seen.filter((r) => r.kind === "graduation").reduce((a, r) => a + r.n, 0);
  const pools = st.seen.filter((r) => r.kind === "pool").reduce((a, r) => a + r.n, 0);
  $("snipe-summary").textContent =
    `Last ${st.hours} h: ${count(seen)} seen (graduations ${count(grads)}, direct pools ${count(pools)}) · followed ${count(followed)}: ` +
    `bought ${count(v.bought)}, buy decided ${count(v.buy)}, buy failed ${count(v.buy_failed)}, skipped ${count(v.skip)}, paused ${count(v.paused)}` +
    (st.follow_kinds?.length ? ` · follow kinds: ${st.follow_kinds.map((k) => `${k.kind} ${count(k.n)} (bought ${count(k.bought)})`).join(", ")}` : "");
  fillTable("snipe-seen", st.seen, (r) => [el("td", "", venueText(r.venue)), el("td", "", r.kind), num(count(r.n)), num(count(r.followed))]);
  fillTable("snipe-verdicts", Object.entries(v).sort((a, b) => b[1] - a[1]), ([k, n]) => [
    el("td", VERDICT_CLS[k] || "", VERDICT_LABEL[k] ?? k), num(count(n)), num(share(followed ? n / followed : null)),
  ]);
  fillTable("snipe-skips", st.skip_steps, ([k, n]) => [el("td", "", k), num(count(n))]);
  fillTable("snipe-classes", st.classes, (c) => [
    el("td", "", c.cls), num(count(c.launches)), num(count(c.followed)), num(count(c.ours)), num(count(c.first)),
    num(c.our_cu_p50 == null ? "–" : count(c.our_cu_p50)), num(c.rival_cu_p50 == null ? "–" : count(c.rival_cu_p50)),
    num(c.fee_sol_p50 == null ? "–" : c.fee_sol_p50.toFixed(6)),
  ]);
  fillTable("snipe-route", st.route_skips, (r) => [
    el("td", "", r.reason), el("td", "", r.class ?? "–"), num(count(r.n)), num(r.rtt_ms == null ? "–" : `${Number(r.rtt_ms).toFixed(1)} ms`),
  ]);
  const dd = st.distance_daily ?? [];
  $("snipe-region").textContent =
    `LEADER_REGIONS skips in the window: ${count(st.region_skips)}` +
    (dd.length ? ` · distance per day (followed passed the rules / skipped for distance): ${dd.slice(0, 7).map((x) => `${x.day} ${x.followed ?? ""} ${count(x.passed_rules)}/${count(x.skipped_for_distance)}`).join(", ")}` : "");
  const b = st.bundle;
  $("snipe-bundle").textContent =
    `Window ${b.window_us_p50 == null ? "–" : `${Math.round(b.window_us_p50)} µs p50, ${Math.round(b.window_us_p99)} µs p99`} (last ${count(b.n)} launches) · ` +
    `bundle buys at the create: ${Object.entries(b.buys).map(([k, n]) => `${k}: ${count(n)}`).join(", ")}`;
  const db = $("snipe-deployers").tBodies[0];
  db.textContent = "";
  $("snipe-deployers-empty").hidden = st.deployers.length > 0;
  for (const d of st.deployers) {
    const tr = el("tr");
    const rec = num(d.record_launches ? `${count(d.record_no_pop)} of ${count(d.record_launches)}` : "–");
    rec.title = "Launches with no buyers (NO_POP by the bot's outcome check) of its recorded launches";
    const res = num(d.results ? `${count(d.results_down)} down of ${count(d.results)}` : "–");
    res.title = d.results ? `Replayed 0.5 SOL buys of its launches: average ${Number(d.avg_pnl_sol).toFixed(3)} SOL` : "";
    tr.append(addrCell(d.deployer), el("td", "muted", d.follow_kind ?? "–"), num(count(d.launches)), num(count(d.bought), d.bought ? "gain" : ""),
      num(count(d.skipped)), num(count(d.paused)), num(d.best_score == null ? "–" : String(d.best_score)), rec, res, num(age(d.last_ms)));
    db.append(tr);
  }
}

function renderSnipe(d) {
  const body = $("snipe").tBodies[0];
  body.textContent = "";
  $("snipe-empty").hidden = d.rows.length > 0;
  $("snipe-empty").textContent = d.available === false ? "No store yet" : "No followed launches for these filters";
  for (const r of d.rows) {
    const tr = el("tr");
    tr.append(el("td", "time", when(r.seen_ms)));
    tr.append(tokenCell(r.mint, r.symbol, r.kind !== "launch" ? r.kind : r.source === "regular" ? "regular stream" : null));
    tr.append(el("td", "", venueText(r.venue)));
    tr.append(addrCell(r.deployer));
    tr.append(el("td", "muted", r.follow_kind ?? "–"));
    tr.append(num(r.dev_buy_lamports == null ? "–" : (r.dev_buy_lamports / LAMPORTS).toFixed(3)));
    tr.append(num(r.score == null ? "–" : String(r.score)));
    tr.append(el("td", VERDICT_CLS[r.verdict] || "", VERDICT_LABEL[r.verdict] ?? r.verdict));
    tr.append(el("td", "muted", r.skip_reason ?? ""));
    body.append(tr);
  }
}

// ---- Live activity, runs and the audit log.

const KIND_CLS = { Bought: "gain", Sold: "gain", BuyFailed: "loss", SellFailed: "loss", Skipped: "muted", Settings: "warn", Detected: "" };

function renderActivity(d) {
  const body = $("activity").tBodies[0];
  body.textContent = "";
  $("activity-empty").hidden = d.rows.length > 0;
  $("activity-empty").textContent = d.available === false ? "No store yet" : "No activity for these filters";
  for (const r of d.rows) {
    const tr = el("tr");
    tr.append(el("td", "time", when(r.at_ms)), el("td", KIND_CLS[r.kind] ?? "", r.kind), el("td", "", r.venue ? venueText(r.venue) : "–"));
    tr.append(r.mint ? tokenCell(r.mint, r.symbol) : el("td", "muted", "–"));
    tr.append(el("td", "", r.message));
    body.append(tr);
  }
}

function renderRuns(d) {
  const body = $("runs").tBodies[0];
  body.textContent = "";
  $("runs-empty").hidden = d.runs.length > 0;
  for (const r of d.runs) {
    const tr = el("tr");
    const id = el("td", "", r.run_id);
    if (d.current?.run_id === r.run_id) id.append(el("span", "tag", "current"));
    tr.append(id, el("td", r.mode === "live" ? "warn" : "", r.mode ?? "–"));
    tr.append(el("td", "time", r.started_ms ? when(r.started_ms) : r.first_ms ? `${when(r.first_ms)} (first position)` : "–"));
    tr.append(el("td", "time", r.stopped_ms ? `${when(r.stopped_ms)}${r.clean ? "" : " (not clean)"}` : r.started_ms ? "running or died" : "–"));
    tr.append(num(count(r.positions)), num(count(r.closed)), num(signedSol(r.net), gainCls(r.net)));
    const sw = r.settings ? Object.entries(r.settings).map(([k, v]) => `${k}=${v}`).join(" ") : "";
    tr.append(el("td", "muted small", sw));
    body.append(tr);
  }
}

function renderAudit(d) {
  const body = $("audit").tBodies[0];
  body.textContent = "";
  $("audit-empty").hidden = d.rows.length > 0;
  for (const r of d.rows) {
    const tr = el("tr");
    tr.append(el("td", "time", r.at ? when(Date.parse(r.at)) : "–"));
    tr.append(el("td", "", r.user ?? "–"));
    tr.append(el("td", "", r.sol ? `${r.action} ${r.sol} SOL` : r.action));
    const good = r.result === "ok" || r.result === "done";
    const bad = r.result === "failed" || r.result === "locked";
    tr.append(el("td", good ? "gain" : bad ? "loss" : "", r.result || "–"));
    tr.append(el("td", "muted", r.ip ?? "–"));
    body.append(tr);
  }
}

// ---- Rejections: the funnel (not paged: one view per window).

let funnel = null;

function renderRejections(d) {
  funnel = d;
  const has = d.available && d.coins > 0;
  $("rejections-empty").hidden = has;
  $("rejections-empty").textContent = d.available ? "No launches in this window" : "No store yet";
  $("rejections-body").hidden = !has;
  if (!has) return;
  const fb = $("funnel").tBodies[0];
  fb.textContent = "";
  let left = d.coins;
  const start = el("tr");
  start.append(el("td", "", `Launches (last ${d.hours} h)`), num(""), num(""), num(count(d.coins)));
  fb.append(start);
  for (const step of d.steps) {
    const n = d.totals[step] ?? 0;
    const pass = step === "buy decided";
    if (!pass) left -= n;
    const tr = el("tr");
    tr.append(el("td", pass ? "gain" : "", step), num(pass ? "–" : count(n)), num(share(n / d.coins)), num(count(pass ? n : left)));
    fb.append(tr);
  }
  const head = $("funnel-hours").tHead.rows[0];
  head.textContent = "";
  head.append(el("th", "", "Hour"), el("th", "num", "Launches"));
  for (const step of d.steps) head.append(el("th", "num", step));
  const hb = $("funnel-hours").tBodies[0];
  hb.textContent = "";
  for (const h of d.per_hour) {
    const tr = el("tr");
    tr.append(el("td", "time", when(h.hour_ms)), num(count(h.coins)));
    for (const step of d.steps) tr.append(num(h.counts[step] ? count(h.counts[step]) : "·"));
    hb.append(tr);
  }
  const sel = $("rej-step");
  const keep = sel.value;
  sel.textContent = "";
  for (const step of d.steps) sel.append(new Option(`${step} (${count(d.totals[step])})`, step));
  sel.value = d.steps.includes(keep) ? keep : d.steps[0];
  renderSamples();
}

function renderSamples() {
  const body = $("funnel-samples").tBodies[0];
  body.textContent = "";
  for (const r of funnel?.samples[$("rej-step").value] ?? []) {
    const tr = el("tr");
    tr.append(el("td", "time", when(r.seen_ms)), tokenCell(r.mint, r.symbol), el("td", "", venueText(r.venue)), el("td", "", r.kind));
    tr.append(el("td", r.followed ? "gain" : "muted", r.followed ? "yes" : "no"));
    tr.append(num(r.dev_buy_lamports == null ? "–" : (r.dev_buy_lamports / LAMPORTS).toFixed(3)));
    tr.append(num(r.score == null ? "–" : String(r.score)));
    tr.append(el("td", "muted", r.skip_reason ?? (r.verdict === "paused" ? "trading paused" : "")));
    body.append(tr);
  }
}

// ---- Paged tables: the same controls, footer, pill and URL state for each.

const tables = {
  history: {
    key: "history",
    tab: "trades",
    url: "/api/history",
    noun: "tokens",
    render: renderHistory,
    sort: "last_active",
    dir: "desc",
    filters: () => ({ ...filters(), show: $("show").value, sort: tables.history.sort, dir: tables.history.dir, run }),
  },
  snipe: {
    key: "snipe",
    tab: "snipe",
    url: "/api/snipe",
    noun: "launches",
    render: renderSnipe,
    extra: (d) => renderSnipeStats(d.stats),
    filters: () => ({ hours: $("snipe-hours").value, venue: $("snipe-venue").value, verdict: $("snipe-verdict").value }),
  },
  activity: {
    key: "activity",
    tab: "activity",
    url: "/api/activity",
    noun: "entries",
    render: renderActivity,
    filters: () => ({ kind: $("kind").value, venue: $("activity-venue").value, run }),
  },
  rejections: {
    key: "rejections",
    tab: "rejections",
    url: "/api/rejections",
    render: renderRejections,
    unpaged: true,
    filters: () => ({ hours: $("rej-hours").value, venue: $("rej-venue").value }),
  },
  runs: {
    key: "runs",
    tab: "runs",
    url: "/api/runs",
    render: renderRuns,
    unpaged: true,
    filters: () => ({}),
  },
  audit: {
    key: "audit",
    tab: "audit",
    url: "/api/audit",
    noun: "entries",
    render: renderAudit,
    filters: () => ({}),
  },
};
for (const t of Object.values(tables)) Object.assign(t, { page: 1, rows: DEFAULT_ROWS, base: null, seq: 0 });
const tableOf = (name) => Object.values(tables).find((t) => t.tab === name);

/** The run sent to the API: every run (the whole history) unless one is chosen. The current run
 * alone hid all earlier trades after each restart (15 Oct). */
function selectedRun() {
  return run === "" ? "all" : run;
}

async function load(t, live = false) {
  const seq = ++t.seq;
  const q = t.filters();
  if ("run" in q) q.run = selectedRun();
  if (t.unpaged) {
    let d;
    try {
      d = await api(`${t.url}?${new URLSearchParams(q)}`);
    } catch {
      return;
    }
    if (seq !== t.seq) return;
    if (d.error) return toast(`Failed: ${d.error}`, true);
    t.render(d);
    if (t.tab === tab) writeUrl();
    return;
  }
  let d;
  try {
    d = await api(`${t.url}?${new URLSearchParams({ ...q, page: t.page, rows: t.rows })}`);
  } catch {
    return;
  }
  if (seq !== t.seq) return;
  if (d.error) return toast(`Failed: ${d.error}`, true);
  t.extra?.(d);
  if (live && t.page > 1 && t.base != null) return pill(t, d.total - t.base);
  t.page = d.page;
  t.base = d.total;
  pill(t, 0);
  t.render(d);
  renderFoot(t, d);
  renderPager(t, d);
  if (t.tab === tab) writeUrl();
}

function pill(t, n) {
  const b = $(`${t.key}-new`);
  b.hidden = !(n > 0);
  b.textContent = `${count(n)} new — back to top`;
}

function goTo(t, page) {
  t.page = page;
  t.base = null;
  load(t);
}

function renderFoot(t, d) {
  const from = d.total ? (d.page - 1) * d.rows_per_page + 1 : 0;
  const to = Math.min(d.total, d.page * d.rows_per_page);
  let s = d.total ? `${count(from)}–${count(to)} of ${count(d.total)} ${t.noun}` : `0 ${t.noun}`;
  if (d.hidden_failed > 0) s += ` · ${count(d.hidden_failed)} failed hidden`;
  $(`${t.key}-foot`).textContent = d.available === false ? "" : s;
}

function pageList(cur, lastPage) {
  const want = [...new Set([1, cur - 1, cur, cur + 1, lastPage])].filter((p) => p >= 1 && p <= lastPage).sort((a, b) => a - b);
  const out = [];
  for (const p of want) {
    const prev = out.at(-1);
    if (prev != null && p - prev === 2) out.push(prev + 1);
    else if (prev != null && p - prev > 2) out.push("…");
    out.push(p);
  }
  return out;
}

function renderPager(t, d) {
  const nav = $(`${t.key}-pager`);
  nav.textContent = "";
  const lastPage = Math.max(1, Math.ceil(d.total / d.rows_per_page));
  nav.hidden = lastPage <= 1;
  const btn = (label, page, aria) => {
    const current = typeof label === "number" && page === d.page;
    const b = el("button", `btn small${current ? " current" : ""}`, String(label));
    b.setAttribute("aria-label", aria);
    if (current) b.setAttribute("aria-current", "page");
    b.disabled = page === d.page || page < 1 || page > lastPage;
    b.onclick = () => goTo(t, page);
    nav.append(b);
  };
  btn("«", 1, "First page");
  btn("‹", d.page - 1, "Previous page");
  for (const p of pageList(d.page, lastPage)) {
    if (p === "…") nav.append(el("span", "gap", "…"));
    else btn(p, p, `Page ${p}`);
  }
  btn("›", d.page + 1, "Next page");
  btn("»", lastPage, "Last page");
}

// ---- URL state: the shown page's tab, page, rows and filters (defaults left out); the mode always.

function writeUrl() {
  const t = tableOf(tab);
  const q = new URLSearchParams();
  if (tab !== "trades") q.set("tab", tab);
  if (modePinned) (q.set("mode", $("mode").value), q.set("pin", "1"));
  if (!t.unpaged && t.page > 1) q.set("page", t.page);
  if (!t.unpaged && t.rows !== DEFAULT_ROWS) q.set("rows", t.rows);
  for (const [k, v] of Object.entries(t.filters())) if (k !== "mode" && v !== DEFAULTS[k] && v !== undefined) q.set(k, v);
  window.history.replaceState(null, "", `?${q}`);
}

function readUrl() {
  const q = new URLSearchParams(location.search);
  tab = TABS.includes(q.get("tab")) ? q.get("tab") : "trades";
  const t = tableOf(tab);
  const page = q.get("page") ?? "";
  t.page = /^[1-9][0-9]{0,6}$/.test(page) ? Number(page) : 1;
  t.rows = ROWS.find((r) => String(r) === q.get("rows")) ?? DEFAULT_ROWS;
  if (!t.unpaged) $(`${t.key}-rows`).value = String(t.rows);
  const set = (id, key) => {
    const v = q.get(key);
    if (v != null && [...$(id).options].some((o) => o.value === v)) $(id).value = v;
  };
  set("show", "show");
  set("kind", "kind");
  set("snipe-verdict", "verdict");
  if (tab === "trades") set("venue", "venue");
  if (tab === "snipe") (set("snipe-hours", "hours"), set("snipe-venue", "venue"));
  if (tab === "activity") set("activity-venue", "venue");
  if (tab === "rejections") (set("rej-hours", "hours"), set("rej-venue", "venue"));
  const m = q.get("mode");
  if ((m === "paper" || m === "live") && q.get("pin") === "1") (modeFromUrl = m, modePinned = true);
  const r = q.get("run");
  if (r === "all" || (r && /^[A-Za-z0-9._:-]{1,64}$/.test(r))) run = r;
  if (SORTS.includes(q.get("sort"))) tables.history.sort = q.get("sort");
  if (q.get("dir") === "asc" || q.get("dir") === "desc") tables.history.dir = q.get("dir");
  const dp = q.get("deployer");
  if (dp && /^[1-9A-HJ-NP-Za-km-z]{32,44}$/.test(dp)) deployer = dp;
}

function showTab(name) {
  tab = name;
  for (const p of document.querySelectorAll("[data-page]")) p.hidden = p.dataset.page !== name;
  for (const b of document.querySelectorAll("[data-tab]")) {
    const on = b.dataset.tab === name;
    b.classList.toggle("active", on);
    if (on) b.setAttribute("aria-current", "page");
    else b.removeAttribute("aria-current");
  }
  writeUrl();
  load(tableOf(name));
}

function filtered(...ts) {
  for (const t of ts) {
    t.page = 1;
    t.base = null;
  }
  if (ts.includes(tableOf(tab))) load(tableOf(tab));
}

/** The trading filters changed: the history, the calendar and the overview follow. */
function tradingFiltersChanged() {
  filtered(tables.history, tables.activity);
  loadCalendar();
  loadOverview();
  writeUrl();
}

async function refreshStatus() {
  try {
    const s = await api("/api/status");
    last = s;
    renderStatus(s);
    renderOpen(s);
    loadOverview();
  } catch {}
}

async function refreshRuns() {
  try {
    const d = await api(`/api/runs?${new URLSearchParams({ mode: $("mode").value })}`);
    const current = d.current?.run_id;
    for (const sel of [$("run"), $("activity-run")]) {
      sel.textContent = "";
      sel.append(new Option(current ? `Current (${current})` : "Current", ""));
      for (const r of d.runs) {
        if (r.run_id === current) continue;
        sel.append(new Option(`${r.run_id} · ${r.mode ?? "?"} · ${r.positions} positions`, r.run_id));
      }
      if (run && run !== "all" && ![...sel.options].some((o) => o.value === run)) sel.append(new Option(run, run));
      sel.append(new Option("All runs", "all"));
      sel.value = run;
    }
    const ds = $("deployer");
    ds.textContent = "";
    ds.append(new Option("All", ""));
    for (const x of d.deployers) ds.append(new Option(`${shortAddr(x.deployer)} · ${x.positions}`, x.deployer));
    if (deployer && !d.deployers.some((x) => x.deployer === deployer)) ds.append(new Option(shortAddr(deployer), deployer));
    ds.value = deployer;
    if (tab === "runs") renderRuns(d);
  } catch {}
}

async function main() {
  const s = await api("/api/session");
  csrf = s.csrf;
  $("who").textContent = s.user;
  for (const b of document.querySelectorAll(".controls [data-action]")) b.onclick = () => trading(b.dataset.action === "trading-on");
  readUrl();
  await refreshStatus();
  // The mode: the one picked by hand (pinned in the URL), else the bot's, else paper. Never both.
  $("mode").value = modeFromUrl ?? last?.bot?.mode ?? "paper";
  $("show").onchange = () => filtered(tables.history);
  $("mode").onchange = () => {
    modePinned = true;
    refreshRuns();
    tradingFiltersChanged();
  };
  $("venue").onchange = tradingFiltersChanged;
  $("deployer").onchange = (e) => {
    deployer = e.target.value;
    tradingFiltersChanged();
  };
  $("run").onchange = $("activity-run").onchange = (e) => {
    run = e.target.value;
    $("run").value = $("activity-run").value = run;
    tradingFiltersChanged();
  };
  $("kind").onchange = $("activity-venue").onchange = () => filtered(tables.activity);
  $("snipe-hours").onchange = $("snipe-venue").onchange = $("snipe-verdict").onchange = () => filtered(tables.snipe);
  $("rej-hours").onchange = $("rej-venue").onchange = () => filtered(tables.rejections);
  $("rej-step").onchange = renderSamples;
  for (const t of Object.values(tables)) {
    if (t.unpaged) continue;
    $(`${t.key}-rows`).onchange = (e) => {
      t.rows = Number(e.target.value);
      filtered(t);
    };
    $(`${t.key}-new`).onclick = () => {
      goTo(t, 1);
      $(`${t.key}-panel`).scrollIntoView({ block: "start" });
    };
  }
  for (const b of document.querySelectorAll("[data-tab]")) b.onclick = () => b.dataset.tab !== tab && showTab(b.dataset.tab);
  $("size-form").onsubmit = setSize;
  $("size-input").oninput = () => sizeError("");
  $("cal-prev").onclick = () => shiftMonth(-1);
  $("cal-next").onclick = () => shiftMonth(1);
  for (const b of document.querySelectorAll("#history .sort")) b.onclick = () => sortBy(b.dataset.sort);
  renderSort();
  $("drawer-close").onclick = closeOrders;
  $("drawer").querySelector(".drawer-shade").onclick = closeOrders;
  $("drawer").onkeydown = trapFocus;
  window.addEventListener("scroll", hideTip, { passive: true });
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    hideTip();
    closeOrders();
  });
  $("logout").onclick = async () => {
    await fetch("/api/logout", { method: "POST", credentials: "same-origin" });
    location.href = "/login";
  };
  await refreshRuns();
  showTab(tab);
  loadCalendar();
  setInterval(refreshStatus, 2000);
  setInterval(() => {
    load(tableOf(tab), true);
    loadCalendar();
  }, 10000);
  setInterval(refreshRuns, 60000);
}

main();
