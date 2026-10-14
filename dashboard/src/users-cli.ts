// Dashboard users: `npm run user -- add|passwd|remove|list [name]` (in dashboard/).
// The password is typed twice without echo, at least 16 characters, stored as an argon2id hash in
// ~/.config/bigboss/dashboard-users.json (mode 0600). Changes apply to the next login; removing
// a user ends their sessions.

import { homedir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";

import { MIN_PASSWORD, hashPassword, loadUsers, saveUsers } from "./auth.ts";

const file = process.env.DASH_USERS ?? join(homedir(), ".config/bigboss/dashboard-users.json");

function hidden(prompt: string): Promise<string> {
  return new Promise((resolve) => {
    const rl = createInterface({ input: process.stdin, output: process.stdout, terminal: true });
    const out = rl as unknown as { _writeToOutput: (s: string) => void };
    let asked = false;
    out._writeToOutput = (s: string) => {
      if (!asked) {
        process.stdout.write(s);
        asked = true;
      }
    };
    rl.question(prompt, (answer) => {
      rl.close();
      process.stdout.write("\n");
      resolve(answer);
    });
  });
}

async function newPassword(): Promise<string> {
  for (;;) {
    const a = await hidden(`Password (at least ${MIN_PASSWORD} characters): `);
    if (a.length < MIN_PASSWORD) {
      console.log(`Too short: ${a.length} characters.`);
      continue;
    }
    const b = await hidden("Again: ");
    if (a !== b) {
      console.log("They don't match.");
      continue;
    }
    return a;
  }
}

const [cmd, name] = process.argv.slice(2);
const users = loadUsers(file);
const valid = (n?: string) => typeof n === "string" && /^[A-Za-z0-9._-]{2,32}$/.test(n);

switch (cmd) {
  case "list":
    for (const u of users) console.log(`${u.name}\t(created ${u.created})`);
    if (!users.length) console.log("no users yet");
    break;
  case "add":
  case "passwd": {
    if (!valid(name)) {
      console.error("usage: add|passwd <name>   (2–32 of A–Z a–z 0–9 . _ -)");
      process.exit(2);
    }
    const existing = users.find((u) => u.name === name);
    if (cmd === "add" && existing) {
      console.error(`${name} exists; use passwd`);
      process.exit(1);
    }
    if (cmd === "passwd" && !existing) {
      console.error(`no user ${name}`);
      process.exit(1);
    }
    const hash = await hashPassword(await newPassword());
    if (existing) existing.hash = hash;
    else users.push({ name, hash, role: "admin", created: new Date().toISOString() });
    saveUsers(file, users);
    console.log(`${cmd === "add" ? "Added" : "Password changed for"} ${name} (${file}).`);
    break;
  }
  case "remove": {
    const left = users.filter((u) => u.name !== name);
    if (left.length === users.length) {
      console.error(`no user ${name}`);
      process.exit(1);
    }
    saveUsers(file, left);
    console.log(`Removed ${name}; their sessions end at their next request.`);
    break;
  }
  default:
    console.log("usage: npm run user -- add|passwd|remove|list [name]");
    process.exit(2);
}
