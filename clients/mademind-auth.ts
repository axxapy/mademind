#!/usr/bin/env bun
// mademind-auth: keys and signed requests for mademind's request auth.
//
// Usage:
//   mademind-auth genkey [<client-id>]     # write ~/.config/mademind/<id>.key (0600)
//                                          # and print the public-key line for
//                                          # config.toml [auth] clients
//   mademind-auth request METHOD PATH [BODY]
//                                          # signed request; prints the response
//                                          # body, exits 1 on HTTP errors
//   mademind-auth sign <message>           # print base64(Ed25519(message))
//   mademind-auth backend                  # print the Ed25519 backend in use
//
// Server, client id and key come from ~/.config/mademind/client.json and/or
// MADEMIND_URL, MADEMIND_CLIENT_ID, MADEMIND_SIGN_KEY(_FILE) (lib/config.ts);
// MADEMIND_AUTH_BACKEND picks the Ed25519 backend (lib/auth.ts). Runs on bun, or node >= 22.6 (`node --experimental-strip-types`).
//
//   mademind-auth request GET "/file?path=notes/foo.md"
//   mademind-auth request POST /query '{"searches":[{"type":"lex","query":"t2 linux"}]}'

import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import { authHeaders, backend, clientId, keyFile, loadSeed, publicKeyLine } from "./lib/auth.ts";
import { localPath, serverUrl } from "./lib/config.ts";

const BASE = serverUrl();
const USAGE =
  "usage: mademind-auth genkey [<client-id>] | request METHOD PATH [BODY] | sign <message> | backend";

async function main(cmd: string | undefined, rest: string[]): Promise<number> {
  switch (cmd) {
    case "genkey": {
      const id = rest[0] ?? clientId();
      const file = keyFile(id);
      if (existsSync(file)) throw new Error(`${file} exists; remove it first to replace the key`);
      const [seed, pub] = backend().genkey();
      mkdirSync(dirname(file), { recursive: true, mode: 0o700 });
      writeFileSync(file, Buffer.concat([seed, pub]).toString("hex") + "\n", { mode: 0o600, flag: "wx" });
      console.error(`private key written to ${file}`);
      console.error(`add to config.toml [auth] clients (and to a rule's clients):`);
      console.log(`{ id = "${id}", public_key = "${publicKeyLine(pub, id).split(" ").slice(0, 2).join(" ")}" }`);
      return 0;
    }
    case "request": {
      const [method, rawPath, body] = rest;
      if (!method || !rawPath) throw new Error(USAGE);
      const url = BASE + (rawPath.startsWith("/") ? rawPath : `/${rawPath}`);
      const headers: Record<string, string> = authHeaders(method, url, body ?? "");
      if (body) headers["content-type"] = "application/json";
      const res = await fetch(url, { method: method.toUpperCase(), headers, body: body || undefined });
      // /file: the real name of what was served, and where it is on this machine.
      const real = res.headers.get("x-mademind-file");
      if (real) {
        const file = decodeURIComponent(real);
        console.error(`file: ${file}\nlocal: ${localPath(file) ?? "no copy on this machine"}`);
      }
      process.stdout.write(await res.text());
      return res.ok ? 0 : 1;
    }
    case "sign": {
      const seed = loadSeed();
      if (!seed) throw new Error(`no signing key (looked in $MADEMIND_SIGN_KEY and ${keyFile()})`);
      console.log(backend().sign(seed, Buffer.from(rest.join(" "), "utf8")).toString("base64"));
      return 0;
    }
    case "backend":
      console.log(backend().name);
      return 0;
    default:
      console.error(USAGE);
      return 2;
  }
}

try {
  process.exit(await main(process.argv[2], process.argv.slice(3)));
} catch (e) {
  console.error(`mademind-auth: ${e instanceof Error ? e.message : e}`);
  process.exit(1);
}
