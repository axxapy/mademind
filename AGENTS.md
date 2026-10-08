# AGENTS.md

Two audiences: agents that want to **use** a mademind server (search someone's notes), and agents **working on this repo**. Full reference: [SPEC.md](SPEC.md).

## Using a mademind server

mademind serves a person's markdown notes on one port (default `http://localhost:8888`). Ask your user for the URL if it isn't local.

**Pick a way in:**

| You have      | Use                                                                                            |
| ------------- | ---------------------------------------------------------------------------------------------- |
| An MCP client | `POST /mcp` (Streamable HTTP), or the stdio wrapper `bun clients/mcp-stdio-proxy.ts <url>/mcp` |
| HTTP only     | `POST /query` to search, `GET /file?path=` to read                                             |
| pi            | `clients/pi-extension/mademind.ts` (`mademind_search`, `mademind_read`)                        |

**MCP tools:** `query` (search), `get` (read a file by path or `#docid`, optionally a line range), `multi_get` (glob or list), `status` (collections and index health).

**Search** — `POST /query` with JSON (the MCP `query` tool takes the same arguments):

```json
{
  "searches": [
    { "type": "lex", "query": "\"connection pool\" timeout -redis" },
    { "type": "vec", "query": "why do requests time out under load?" }
  ],
  "limit": 5,
  "rerank": false,
  "collections": ["notes"]
}
```

- `lex` = keywords (BM25: `"exact phrase"`, `-exclude`, prefix match), `vec` = meaning, `hyde` = write 50–100 words that look like the answer. Combine `lex` + `vec` for recall; the first sub-search weighs double.
- Keep `rerank: false` unless a fast search misses: reranking runs an LLM on CPU and can take minutes on a new query.
- Optional: `intent` (what you're after, steers snippets), `minScore`, `candidateLimit`.
- Response: `{"results": [{"docid", "file", "title", "score", "context", "line", "snippet"}]}`. `file` is `<collection>/<path>`.

**Read before you cite.** Results are pointers; read the file (`GET /file?path=<file>` with the hit's `file`, or the MCP `get` tool) and quote that, never the snippet. `/file` also takes absolute paths inside a collection; anything else is 403. Hit paths can be normalised (`AGENT_MEMORY.md` comes back as `AGENT-MEMORY.md`); `/file` still finds the file and returns its real `<collection>/<path>` in the `x-mademind-file` header. To *edit* a note, use a real local path: the pi tool and `mademind-auth.ts request` print one when this machine has the collection (`local_roots` in `~/.config/mademind/client.json`) — never guess one.

**Auth.** Requests from the server's own machine usually need nothing. From elsewhere, sign each request (otherwise 401):

```
x-mademind-client:    <client id>
x-mademind-timestamp: <unix seconds>
x-mademind-signature: base64(Ed25519(key, "mademind-auth-v1\n<client id>\n<timestamp>\n<METHOD> <path?query>\n<sha256 hex of body>"))
```

Easiest: let the shipped clients do it — `bun clients/mademind-auth.ts request POST /query '<json>'` (key in `~/.config/mademind/<id>.key`, created by `genkey`). The same signature can't be reused, so don't send two identical requests in the same second. `GET /healthz` and `GET /metrics` are always open.

**Freshness.** The index follows the files: keyword search sees a saved note within ~3 s, semantic search within ~10 min. Don't try to trigger re-indexing.

## Working on this repo

Rust server (`src/`), TypeScript clients (`clients/`), packaging (`Dockerfile*`, `scripts/dist.sh`, `.github/workflows/`).

```sh
cargo test                           # builtin engine (compiles llama.cpp: cmake + C++ compiler)
cargo test --no-default-features     # external engine only
cargo clippy --all-targets -- -D warnings   # both feature sets, like CI
cargo fmt
```

Where things are:

| Path                     | What                                                                                         |
| ------------------------ | -------------------------------------------------------------------------------------------- |
| `src/main.rs`            | startup: config, engine, watcher thread, embed task, HTTP server, `healthcheck`, `--version` |
| `src/config.rs`          | `config.toml` schema, defaults, env overrides                                                |
| `src/http.rs`            | axum router, auth layer, request metrics                                                     |
| `src/auth.rs`            | signature verification, per-IP rules                                                         |
| `src/files.rs`           | `/file`                                                                                      |
| `src/engine/builtin.rs`  | rqmd in-process: query side + indexer thread                                                 |
| `src/engine/mcp.rs`      | MCP handler: rqmd-mcp server with instructions rebuilt from the live index                   |
| `src/engine/external.rs` | qmd child process: supervisor, proxy, `update`/`embed` commands                              |
| `src/watcher.rs`         | inotify + debounce                                                                           |
| `clients/lib/auth.ts`    | the one client-side implementation of request signing                                        |

Things that bite:

- `config.reference.toml` must match `Config::default()` (a test checks); update both together. `config.example.toml` is the short starting point (also tested to parse).
- The pi extension inlines its own copy of the signing code (pi loads one file) — keep it in sync with `clients/lib/auth.ts` and `src/auth.rs`.
- `llama-cpp-2`/`-sys-2` are pinned to `=0.1.146` because rqmd-core only builds against that; don't `cargo update` them.
- Commits are small and logical (one concern each); docs are markdown without hard line wraps.
