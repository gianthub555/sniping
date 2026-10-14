// Logins: username + password only, from anywhere (the copy bot dashboard's, unchanged).
// - Users live in a JSON file outside the repo (mode 0600), passwords hashed with argon2id,
//   at least 16 characters.
// - Sessions: random 32-byte ids in an HttpOnly, Secure, SameSite=Strict cookie, 12 hours, each
//   with its own CSRF token required on every POST.
// - Lockout: 5 failed logins from one source address within 15 minutes lock that address for 15
//   minutes (per address, so nobody can lock the owner out by failing the owner's username).

import { hash, verify } from "@node-rs/argon2";
import { randomBytes, timingSafeEqual } from "node:crypto";
import { chmodSync, existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

export const MIN_PASSWORD = 16;
export const SESSION_MS = 12 * 3600 * 1000;
const LOCK_FAILS = 5;
const LOCK_WINDOW_MS = 15 * 60 * 1000;
const LOCK_MS = 15 * 60 * 1000;

export interface User {
  name: string;
  hash: string;
  role: "admin";
  created: string;
}

interface Session {
  user: string;
  csrf: string;
  expires: number;
}

/** argon2id with the library's defaults (memory-hard), algorithm pinned. */
const ARGON = { algorithm: 2 as const };

export function loadUsers(file: string): User[] {
  if (!existsSync(file)) return [];
  const doc = JSON.parse(readFileSync(file, "utf8"));
  return Array.isArray(doc.users) ? doc.users : [];
}

export function saveUsers(file: string, users: User[]): void {
  mkdirSync(dirname(file), { recursive: true, mode: 0o700 });
  const tmp = `${file}.tmp`;
  writeFileSync(tmp, JSON.stringify({ users }, null, 2) + "\n", { mode: 0o600 });
  chmodSync(tmp, 0o600);
  renameSync(tmp, file);
}

export async function hashPassword(password: string): Promise<string> {
  if (password.length < MIN_PASSWORD) {
    throw new Error(`passwords need at least ${MIN_PASSWORD} characters`);
  }
  return hash(password, ARGON);
}

export class Auth {
  usersFile: string;
  sessions = new Map<string, Session>();
  failures = new Map<string, { at: number[]; lockedUntil: number }>();
  /** A real argon2id hash to verify against for unknown users (same timing as a known one). */
  dummy: Promise<string>;

  constructor(usersFile: string) {
    this.usersFile = usersFile;
    this.dummy = hash(randomBytes(24).toString("hex"), ARGON);
  }

  /** Seconds until this address may try again (0 = not locked). */
  lockedFor(ip: string, now = Date.now()): number {
    const f = this.failures.get(ip);
    return f && f.lockedUntil > now ? Math.ceil((f.lockedUntil - now) / 1000) : 0;
  }

  noteFailure(ip: string, now = Date.now()): void {
    const f = this.failures.get(ip) ?? { at: [], lockedUntil: 0 };
    f.at = f.at.filter((t) => now - t < LOCK_WINDOW_MS);
    f.at.push(now);
    if (f.at.length >= LOCK_FAILS) {
      f.lockedUntil = now + LOCK_MS;
      f.at = [];
    }
    this.failures.set(ip, f);
  }

  /** A session id for valid credentials, or null. Respects the lockout. */
  async login(name: string, password: string, ip: string): Promise<{ sid: string; csrf: string } | null> {
    if (this.lockedFor(ip) > 0) return null;
    const user = loadUsers(this.usersFile).find((u) => u.name === name);
    const ok = await verify(user ? user.hash : await this.dummy, password).catch(() => false);
    if (!user || !ok) {
      this.noteFailure(ip);
      return null;
    }
    this.failures.delete(ip);
    const sid = randomBytes(32).toString("base64url");
    const csrf = randomBytes(24).toString("base64url");
    this.sessions.set(sid, { user: user.name, csrf, expires: Date.now() + SESSION_MS });
    return { sid, csrf };
  }

  session(sid: string | undefined): Session | null {
    if (!sid) return null;
    const s = this.sessions.get(sid);
    if (!s) return null;
    if (s.expires < Date.now()) {
      this.sessions.delete(sid);
      return null;
    }
    // The user may have been removed since.
    if (!loadUsers(this.usersFile).some((u) => u.name === s.user)) {
      this.sessions.delete(sid);
      return null;
    }
    return s;
  }

  checkCsrf(s: Session, token: string | undefined): boolean {
    if (!token) return false;
    const a = Buffer.from(s.csrf);
    const b = Buffer.from(token);
    return a.length === b.length && timingSafeEqual(a, b);
  }

  logout(sid: string | undefined): void {
    if (sid) this.sessions.delete(sid);
  }
}
