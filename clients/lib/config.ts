// Per-machine client settings: which server, who we are, and where this
// machine keeps local copies of the collections.
//
// File: $MADEMIND_CLIENT_CONFIG, else ~/.config/mademind/client.json
//   {
//     "url": "http://warp.home:8888",            // server; default http://127.0.0.1:8888
//     "client_id": "laptop",                     // default: short hostname
//     "key_file": "~/.config/mademind/laptop.key", // default: ~/.config/mademind/<client_id>.key
//     "local_roots": {                           // collection -> local folder (optional)
//       "notes": "~/notes"
//     }
//   }
// Env overrides: MADEMIND_URL, MADEMIND_CLIENT_ID, MADEMIND_SIGN_KEY_FILE,
// MADEMIND_LOCAL_ROOTS ("notes=~/notes,work=/srv/work").
//
// local_roots only ever describe THIS machine; a local path is reported only
// when the file actually exists there.

import { existsSync, readFileSync } from "node:fs";
import { homedir, hostname } from "node:os";
import { join } from "node:path";

export interface ClientConfig {
  url?: string;
  client_id?: string;
  key_file?: string;
  local_roots?: Record<string, string>;
}

const expand = (p: string) => (p === "~" || p.startsWith("~/") ? join(homedir(), p.slice(1)) : p);

export function configFile(): string {
  return process.env.MADEMIND_CLIENT_CONFIG || join(homedir(), ".config", "mademind", "client.json");
}

let cached: ClientConfig | undefined;
export function loadConfig(): ClientConfig {
  if (cached) return cached;
  try {
    cached = JSON.parse(readFileSync(configFile(), "utf8")) as ClientConfig;
  } catch (e: any) {
    if (e?.code !== "ENOENT") throw new Error(`${configFile()}: ${e.message}`);
    cached = {};
  }
  return cached;
}

export function serverUrl(): string {
  return (process.env.MADEMIND_URL || loadConfig().url || "http://127.0.0.1:8888").replace(/\/+$/, "");
}

export function clientId(): string {
  return process.env.MADEMIND_CLIENT_ID || loadConfig().client_id || hostname().split(".")[0];
}

export function keyFile(id = clientId()): string {
  const f = process.env.MADEMIND_SIGN_KEY_FILE || loadConfig().key_file;
  return f ? expand(f) : join(homedir(), ".config", "mademind", `${id}.key`);
}

export function localRoots(): Record<string, string> {
  const roots: Record<string, string> = {};
  for (const [k, v] of Object.entries(loadConfig().local_roots ?? {})) roots[k] = expand(v);
  for (const pair of (process.env.MADEMIND_LOCAL_ROOTS ?? "").split(",")) {
    const i = pair.indexOf("=");
    if (i > 0) roots[pair.slice(0, i).trim()] = expand(pair.slice(i + 1).trim());
  }
  return roots;
}

/** Local path of `<collection>/<rel>` on this machine, if the file is there. */
export function localPath(collectionPath: string): string | null {
  const i = collectionPath.indexOf("/");
  const root = localRoots()[i < 0 ? collectionPath : collectionPath.slice(0, i)];
  if (!root) return null;
  const p = i < 0 ? root : join(root, collectionPath.slice(i + 1));
  return existsSync(p) ? p : null;
}
