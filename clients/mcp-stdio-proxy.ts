// stdio -> HTTP MCP proxy for mademind.
//
// Claude Code's MCP client runs its OAuth dance (Dynamic Client
// Registration) against raw HTTP MCP servers and gets stuck when the
// server (correctly, per MCP spec) answers 404 to /register. stdio
// servers never get that treatment - so this thin wrapper speaks
// MCP-over-stdio locally and forwards JSON-RPC to the mademind server's
// Streamable-HTTP endpoint, carrying the Mcp-Session-Id. Requests are signed
// when a key is configured (see lib/auth.ts), so it also works against a
// mademind on another machine.
//
// No dependencies; runs on bun (or node >= 22.6 with --experimental-strip-types).
// Config (claude.json mcpServers):
//   "mademind": { "type": "stdio",
//                 "command": "bun",
//                 "args": ["<this-file>", "http://127.0.0.1:8888/mcp"] }

import { authHeaders } from "./lib/auth.ts";
import { serverUrl } from "./lib/config.ts";

// Server: argv, else client.json / MADEMIND_URL (lib/config.ts).
const UPSTREAM = process.argv[2] ?? `${serverUrl()}/mcp`;
let sessionId: string | undefined;
let pending = 0;
let stdinEnded = false;

function emit(obj: unknown) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

/** Answer a request with a JSON-RPC error; notifications (no id) get nothing. */
function emitError(msg: any, message: string) {
  if (msg.id === undefined) return;
  emit({ jsonrpc: "2.0", id: msg.id, error: { code: -32000, message } });
}

async function post(msg: any) {
  pending++;
  try {
    await forward(msg);
  } catch (e) {
    emitError(msg, `upstream: ${e}`);
  } finally {
    pending--;
    if (stdinEnded && pending === 0) process.exit(0);
  }
}

async function forward(msg: any) {
  const body = JSON.stringify(msg);
  const headers: Record<string, string> = {
    "content-type": "application/json",
    accept: "application/json, text/event-stream",
    ...authHeaders("POST", UPSTREAM, body),
  };
  if (sessionId) headers["mcp-session-id"] = sessionId;
  const res = await fetch(UPSTREAM, { method: "POST", headers, body });
  // An unknown session (server restarted): forget it so the client's next
  // `initialize` starts a fresh one instead of failing forever.
  if (res.status === 404 && sessionId) sessionId = undefined;
  const sid = res.headers.get("mcp-session-id");
  if (sid) sessionId = sid;

  const text = await res.text();
  const out: unknown[] = [];
  if ((res.headers.get("content-type") ?? "").includes("text/event-stream")) {
    for (const line of text.split("\n")) {
      if (!line.startsWith("data:")) continue;
      try {
        out.push(JSON.parse(line.slice(5).trim()));
      } catch {
        /* keep-alive or partial */
      }
    }
  } else if (text) {
    try {
      out.push(JSON.parse(text));
    } catch {
      /* non-JSON body (e.g. an auth error) */
    }
  }
  for (const m of out) emit(annotate(m));
  // HTTP failure without a JSON-RPC answer: don't leave the request hanging.
  if (!res.ok && out.length === 0) {
    emitError(msg, `HTTP ${res.status}: ${text.trim().slice(0, 200)}`);
  }
}

// qmd's `query` tool defaults to rerank:true (LLM expansion + rerank on CPU,
// minutes per novel query; many MCP clients time out sooner). Default it to
// false here so agents get the <1s hybrid path unless they ask for reranking.
function defaultRerankOff(msg: any) {
  if (msg.method !== "tools/call" || msg.params?.name !== "query") return;
  const args = (msg.params.arguments ??= {});
  if (typeof args.rerank !== "boolean") args.rerank = false;
}

// The names users actually say ("search qmd for X"), so agents map them to
// these tools.
const ALIASES =
  'This is the user\'s notes / knowledge base; they may call it "qmd", "kb", "mademind", "the knowledge base" or "my notes".';

// Adjust what the model sees: the real `rerank` default in the query schema,
// and the aliases in the server instructions and the query tool description.
function annotate(msg: any) {
  const result = msg?.result;
  if (typeof result?.instructions === "string") {
    result.instructions = `${ALIASES}\n\n${result.instructions}`;
  } else if (result?.serverInfo && result.instructions === undefined) {
    result.instructions = ALIASES;
  }
  const tools = result?.tools;
  if (!Array.isArray(tools)) return msg;
  for (const t of tools) {
    if (t?.name !== "query") continue;
    if (typeof t.description === "string") t.description = `${ALIASES} ${t.description}`;
    const r = t.inputSchema?.properties?.rerank;
    if (!r) continue;
    r.default = false;
    r.description =
      "Rerank results using LLM (default: false, <1s hybrid BM25+vector). " +
      "true = query expansion + CPU rerank, minutes per novel query; use only when a fast query misses.";
  }
  return msg;
}

// stdin chunks don't align with lines: keep the trailing partial line.
let buffered = "";
process.stdin.on("data", (buf: Buffer) => {
  const lines = (buffered + buf.toString()).split("\n");
  buffered = lines.pop() ?? "";
  for (const line of lines) {
    if (!line.trim()) continue;
    let msg: any;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    // Responses from the client (no method) have nothing to forward.
    if (!msg.method) continue;
    defaultRerankOff(msg);
    post(msg);
  }
});
process.stdin.on("end", () => {
  stdinEnded = true;
  if (pending === 0) process.exit(0);
});
