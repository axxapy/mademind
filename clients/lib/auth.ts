// Shared client side of mademind's request auth (see src/auth.rs).
//
// A signed request carries
//   x-mademind-client:    client id
//   x-mademind-timestamp: unix seconds
//   x-mademind-signature: base64(Ed25519(message))
// over
//   mademind-auth-v1\n<client-id>\n<unix-ts>\n<METHOD> <path?query>\n<sha256-hex(body)>
//
// Keys: 64-byte hex = 32-byte seed || 32-byte public key, in
// $MADEMIND_SIGN_KEY, else the file $MADEMIND_SIGN_KEY_FILE, else
// ~/.config/mademind/<client-id>.key. The client id is $MADEMIND_CLIENT_ID,
// else the short hostname.
//
// No dependencies. Ed25519 comes from the first backend that works: the
// runtime's node:crypto (bun or node), then the system `openssl` CLI
// (OpenSSL >= 3.0 for `pkeyutl -rawin`; macOS's LibreSSL lacks it). Force one
// with MADEMIND_AUTH_BACKEND=node|openssl. Ed25519 is deterministic, so both
// produce byte-identical signatures.

import { execFileSync } from "node:child_process";
import * as crypto from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { homedir, hostname, tmpdir } from "node:os";
import { join } from "node:path";

export const AUTH_VERSION = "mademind-auth-v1";

// DER prefixes: PKCS#8 private key around a 32-byte seed, SPKI public key
// around a 32-byte raw key (RFC 8410).
const PKCS8_PREFIX = Buffer.from("302e020100300506032b657004220420", "hex");
const SPKI_PREFIX = Buffer.from("302a300506032b6570032100", "hex");

export interface Backend {
  name: string;
  /** Fresh keypair: [32-byte seed, 32-byte public key]. */
  genkey(): [Buffer, Buffer];
  /** Raw 64-byte Ed25519 signature of message with seed. */
  sign(seed: Buffer, message: Buffer): Buffer;
}

const nodeBackend: Backend = {
  name: "node",
  genkey() {
    const { privateKey, publicKey } = crypto.generateKeyPairSync("ed25519");
    const seed = privateKey.export({ format: "der", type: "pkcs8" }).subarray(PKCS8_PREFIX.length);
    const pub = publicKey.export({ format: "der", type: "spki" }).subarray(SPKI_PREFIX.length);
    return [Buffer.from(seed), Buffer.from(pub)];
  },
  sign(seed, message) {
    const key = crypto.createPrivateKey({ key: Buffer.concat([PKCS8_PREFIX, seed]), format: "der", type: "pkcs8" });
    return crypto.sign(null, message, key);
  },
};

const openssl = (args: string[], input?: Buffer) =>
  execFileSync("openssl", args, { input, stdio: ["pipe", "pipe", "pipe"] });

const opensslBackend: Backend = {
  name: "openssl",
  genkey() {
    const der = openssl(["genpkey", "-algorithm", "ed25519", "-outform", "DER"]);
    const pub = openssl(["pkey", "-inform", "DER", "-pubout", "-outform", "DER"], der);
    return [der.subarray(der.length - 32), pub.subarray(pub.length - 32)];
  },
  sign(seed, message) {
    // pkeyutl -rawin can't read stdin (needs the size up front); use a
    // private temp dir for the key and message.
    const dir = mkdtempSync(join(tmpdir(), "mademind-auth-"));
    try {
      const keyFile = join(dir, "key.der");
      const msgFile = join(dir, "msg");
      writeFileSync(keyFile, Buffer.concat([PKCS8_PREFIX, seed]), { mode: 0o600 });
      writeFileSync(msgFile, message, { mode: 0o600 });
      return openssl(["pkeyutl", "-sign", "-rawin", "-inkey", keyFile, "-keyform", "DER", "-in", msgFile]);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  },
};

const BACKENDS: Record<string, Backend> = { node: nodeBackend, openssl: opensslBackend };

/** A backend works if it can sign a probe message with a valid-looking result. */
function usable(b: Backend): boolean {
  try {
    return b.sign(Buffer.alloc(32, 1), Buffer.from("probe")).length === 64;
  } catch {
    return false;
  }
}

let picked: Backend | undefined;
export function backend(): Backend {
  if (picked) return picked;
  const forced = process.env.MADEMIND_AUTH_BACKEND;
  if (forced) {
    const b = BACKENDS[forced];
    if (!b) throw new Error(`MADEMIND_AUTH_BACKEND=${forced}: expected one of ${Object.keys(BACKENDS).join(", ")}`);
    if (!usable(b)) throw new Error(`MADEMIND_AUTH_BACKEND=${forced}: backend not usable on this machine`);
    return (picked = b);
  }
  picked = Object.values(BACKENDS).find(usable);
  if (!picked) throw new Error("no Ed25519 backend: need node:crypto with ed25519 support or openssl >= 3.0");
  return picked;
}

export function clientId(): string {
  return process.env.MADEMIND_CLIENT_ID || hostname().split(".")[0];
}

export function keyFile(id = clientId()): string {
  return process.env.MADEMIND_SIGN_KEY_FILE || join(homedir(), ".config", "mademind", `${id}.key`);
}

/** The signing seed, or null when no key is configured (unsigned requests). */
export function loadSeed(): Buffer | null {
  let hex = process.env.MADEMIND_SIGN_KEY ?? "";
  if (!hex) {
    try {
      hex = readFileSync(keyFile(), "utf8");
    } catch {
      return null;
    }
  }
  hex = hex.replace(/\s+/g, "").toLowerCase();
  if (!/^[0-9a-f]{128}$/.test(hex)) throw new Error("signing key: expected 64-byte hex");
  return Buffer.from(hex.slice(0, 64), "hex");
}

/** Line for the server's [auth] clients: `ssh-ed25519 <base64(blob)> <comment>`. */
export function publicKeyLine(pub32: Buffer, comment: string): string {
  // OpenSSH blob: len(11) "ssh-ed25519" len(32) <32-byte raw key>
  const blob = Buffer.alloc(4 + 11 + 4 + 32);
  blob.writeUInt32BE(11, 0);
  blob.write("ssh-ed25519", 4, "latin1");
  blob.writeUInt32BE(32, 4 + 11);
  pub32.copy(blob, 4 + 11 + 4);
  return `ssh-ed25519 ${blob.toString("base64")} ${comment}`;
}

/**
 * Auth headers for one request to `url` (signed over its path + query), or
 * none when no key is configured — fine for sources the server leaves open.
 */
export function authHeaders(method: string, url: string, body: string | Uint8Array = ""): Record<string, string> {
  const seed = loadSeed();
  if (!seed) return {};
  const id = clientId();
  const ts = Math.floor(Date.now() / 1000).toString();
  const u = new URL(url);
  const digest = crypto.createHash("sha256").update(body).digest("hex");
  const message = `${AUTH_VERSION}\n${id}\n${ts}\n${method.toUpperCase()} ${u.pathname}${u.search}\n${digest}`;
  const sig = backend().sign(seed, Buffer.from(message, "utf8"));
  return {
    "x-mademind-client": id,
    "x-mademind-timestamp": ts,
    "x-mademind-signature": sig.toString("base64"),
  };
}
