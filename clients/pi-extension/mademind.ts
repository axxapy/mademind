/**
 * mademind: search + read your notes from a pi agent.
 *
 * Tools:
 *   mademind_search — hybrid (BM25 + vector, + optional LLM rerank) search over
 *                     the indexed collections. Results give file + line; then
 *                     READ THE FILE with mademind_read. Never cite snippets as
 *                     facts.
 *   mademind_read   — read a file by the path a search hit gave
 *                     (`<collection>/<rel>`), via the server's /file endpoint.
 *
 * Index freshness is the server's job (watcher -> update, 2s debounce; embed
 * every 10 min), not the agents': no refresh hooks here.
 *
 * Env:
 *   MADEMIND_URL        server base URL (default http://127.0.0.1:8888)
 *   MADEMIND_CLIENT_ID  client id for request signing (default: short hostname)
 *   MADEMIND_SIGN_KEY   64-byte hex key, else the file MADEMIND_SIGN_KEY_FILE
 *                       (default ~/.config/mademind/<client-id>.key)
 *
 * Deploys as this one file (pi loads it directly), so request signing is
 * inlined here rather than imported from ../lib/auth.ts; keep the two in sync.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { createHash, createPrivateKey, sign } from "node:crypto";
import { readFileSync } from "node:fs";
import { homedir, hostname } from "node:os";
import { join } from "node:path";

const BASE = (process.env.MADEMIND_URL ?? "http://127.0.0.1:8888").replace(/\/+$/, "");
const CLIENT_ID = process.env.MADEMIND_CLIENT_ID || hostname().split(".")[0];
const PKCS8_PREFIX = Buffer.from("302e020100300506032b657004220420", "hex"); // Ed25519 seed -> DER

/** Signing key from env or key file; null = send unsigned (fine for open sources). */
let seed: Buffer | null | undefined;
function loadSeed(): Buffer | null {
  if (seed !== undefined) return seed;
  let hex = process.env.MADEMIND_SIGN_KEY ?? "";
  if (!hex) {
    try {
      hex = readFileSync(
        process.env.MADEMIND_SIGN_KEY_FILE || join(homedir(), ".config", "mademind", `${CLIENT_ID}.key`),
        "utf8",
      );
    } catch {
      hex = "";
    }
  }
  hex = hex.replace(/\s+/g, "");
  seed = /^[0-9a-fA-F]{128}$/.test(hex) ? Buffer.from(hex.slice(0, 64), "hex") : null;
  return seed;
}

/** mademind-auth-v1 headers for one request, signed over its path + query. */
function authHeaders(method: string, url: string, body = ""): Record<string, string> {
  const s = loadSeed();
  if (!s) return {};
  const ts = Math.floor(Date.now() / 1000).toString();
  const u = new URL(url);
  const digest = createHash("sha256").update(body).digest("hex");
  const message = `mademind-auth-v1\n${CLIENT_ID}\n${ts}\n${method} ${u.pathname}${u.search}\n${digest}`;
  const key = createPrivateKey({ key: Buffer.concat([PKCS8_PREFIX, s]), format: "der", type: "pkcs8" });
  return {
    "x-mademind-client": CLIENT_ID,
    "x-mademind-timestamp": ts,
    "x-mademind-signature": sign(null, Buffer.from(message, "utf8"), key).toString("base64"),
  };
}

// Hit paths are knowledge-base paths (<collection>/<file>), not local files;
// spell out how to read each one so agents don't reach for a file tool.
const READ_HINT =
  "Paths above are knowledge-base paths (<collection>/<file>), not local files: read them with mademind_read, not a file-read tool.";

function formatHit(r: any): string {
  const ctx = String(r.context ?? "");
  return [
    `${r.score?.toFixed(3) ?? "?"}  ${r.file ?? "?"}  (line ${r.line ?? "?"})`,
    `     read: mademind_read {"path": ${JSON.stringify(r.file ?? "")}}`,
    ctx ? `     context: ${ctx.length > 140 ? `${ctx.slice(0, 140)}…` : ctx}` : "",
    `     ${String(r.snippet ?? "").split("\n").slice(0, 3).join(" | ")}`.trim(),
  ]
    .filter(Boolean)
    .join("\n");
}

const text = (t: string) => ({ content: [{ type: "text" as const, text: t }] });

export default function (pi: ExtensionAPI) {
  pi.registerTool({
    name: "mademind_search",
    label: "Notes search",
    description:
      "Search the user's notes — their knowledge base, which they may call \"qmd\", \"kb\", \"mademind\" or \"my notes\" — with hybrid BM25+vector (+ optional LLM rerank). Returns knowledge-base paths (<collection>/<file>, not filesystem paths) and lines — then read each with mademind_read for the full content (retrieve-then-read). Use for 'I forgot the words' recall, facts, and prior decisions.",
    parameters: Type.Object({
      query: Type.String({ description: "Natural-language query" }),
      limit: Type.Optional(Type.Number({ description: "Max results (default 5)" })),
      collections: Type.Optional(
        Type.Array(Type.String(), { description: "Restrict to collections (OR)" }),
      ),
      rerank: Type.Optional(
        Type.Boolean({
          description:
            "LLM rerank+expansion (default false). false = hybrid RRF, <1s. true = higher quality but can take MINUTES on novel queries (CPU; results are cached afterwards).",
        }),
      ),
    }),
    async execute(_id, params) {
      const url = `${BASE}/query`;
      const body = JSON.stringify({
        searches: [
          { type: "lex", query: params.query },
          { type: "vec", query: params.query },
        ],
        limit: params.limit ?? 5,
        rerank: params.rerank ?? false,
        ...(params.collections ? { collections: params.collections } : {}),
      });
      const res = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json", ...authHeaders("POST", url, body) },
        body,
      });
      if (!res.ok) return text(`mademind error ${res.status}: ${await res.text()}`);
      const results = (await res.json()).results ?? [];
      if (!results.length) return text("No results.");
      return {
        content: [{ type: "text", text: `${results.map(formatHit).join("\n\n")}\n\n${READ_HINT}` }],
        details: { hits: results.length },
      };
    },
  });

  pi.registerTool({
    name: "mademind_read",
    label: "Notes read file",
    description:
      "Read a file from the user's notes (their knowledge base: \"qmd\", \"kb\") by the exact path a mademind_search hit gave (a knowledge-base path like notes/ideas/foo.md, not a filesystem path).",
    parameters: Type.Object({
      path: Type.String({ description: "File path from mademind_search output" }),
    }),
    async execute(_id, params) {
      const url = `${BASE}/file?path=${encodeURIComponent(params.path)}`;
      const res = await fetch(url, { headers: authHeaders("GET", url) });
      if (res.status === 404) return text(`not found: ${params.path}`);
      if (res.status === 403) return text(`forbidden (not in any collection): ${params.path}`);
      if (!res.ok) return text(`mademind /file error ${res.status}: ${await res.text()}`);
      return text(await res.text());
    },
  });
}
