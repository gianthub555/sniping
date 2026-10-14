// The dashboard's audit log: one JSON line per login attempt and per
// control call, with time (`at`), source address, user, action and result. Created when the
// dashboard starts if absent. Read back newest first, one page at a time.

import { appendFileSync, mkdirSync, readFileSync } from "node:fs";
import { dirname } from "node:path";

import { clampPage, type Page } from "./history.ts";

export interface AuditRow {
  at: string | null;
  ip: string | null;
  user: string | null;
  action: string;
  result: string;
  mint: string | null;
  /** A buy size as typed (SOL), on set-size lines. */
  sol: string | null;
}

const text = (v: unknown): string | null => (typeof v === "string" ? v : null);

/** Results as written: strings, `true` / `false` from the engine, or an object. */
function resultText(v: unknown): string {
  if (v === true) return "ok";
  if (v === false) return "failed";
  if (typeof v === "string") return v;
  return v == null ? "" : JSON.stringify(v);
}

/**
 * One line as a row. Login lines from before `action` / `result` were written carry only `event`
 * ('login' | 'login_failed' | 'login_locked'); they read the same as new ones.
 */
function row(line: string): AuditRow {
  let e: Record<string, unknown>;
  try {
    e = JSON.parse(line);
  } catch {
    return { at: null, ip: null, user: null, action: "unreadable line", result: line.slice(0, 200), mint: null, sol: null };
  }
  const event = text(e.event) ?? "";
  const login = event.startsWith("login");
  const derived = event === "login" ? "ok" : event === "login_failed" ? "failed" : event === "login_locked" ? "locked" : "";
  return {
    at: text(e.at),
    ip: text(e.ip),
    user: text(e.user),
    action: text(e.action) ?? (login ? "login" : event),
    result: e.result !== undefined ? resultText(e.result) : derived,
    mint: text(e.mint),
    sol: text(e.sol),
  };
}

export class AuditLog {
  path: string;

  constructor(path: string) {
    this.path = path;
  }

  /** Creates the file (and its directory) if absent; never truncates. */
  ensure(): void {
    mkdirSync(dirname(this.path), { recursive: true });
    appendFileSync(this.path, "");
  }

  append(entry: Record<string, unknown>): void {
    this.ensure();
    appendFileSync(this.path, JSON.stringify({ at: new Date().toISOString(), ...entry }) + "\n");
  }

  /**
   * One page, newest first. Reads the whole file per request: it grows by one short line per
   * login or action, so a few MB at most, which reads in milliseconds. Only the page's lines are
   * parsed.
   */
  page(page: number, rows: number): Page<AuditRow> {
    let lines: string[];
    try {
      lines = readFileSync(this.path, "utf8").split("\n").filter((l) => l.trim() !== "");
    } catch {
      lines = [];
    }
    const total = lines.length;
    const at = clampPage(total, page, rows);
    const out: AuditRow[] = [];
    for (let i = at.offset; i < Math.min(total, at.offset + rows); i++) out.push(row(lines[total - 1 - i]!));
    return { rows: out, total, page: at.page, rows_per_page: rows };
  }
}
