# mademind — specification

What mademind does, precisely. For a quick start see [README.md](README.md); for agents, [AGENTS.md](AGENTS.md). Version 0.2.0.

## 1. Overview

mademind is a single binary that indexes collections of markdown files and serves search and file reads to AI agents over one HTTP port.

```
agent ──HTTP :8888──► mademind
                       ├─ auth layer (per-source-IP rules, Ed25519 signatures)
                       ├─ GET /healthz, GET /metrics, GET /file
                       └─ engine: /mcp, /query, /search, /health
                            builtin: rqmd in-process
                            external: qmd child on loopback, reverse-proxied
          watcher (inotify) ──► engine update   (debounced, ~2 s)
          embed task        ──► engine embed    (every 10 min)
```

Notes are only ever read. The index and models live in a cache directory.

## 2. Engines

`[engine] kind` selects one; unknown values abort startup.

### 2.1 builtin (default)

rqmd (`rqmd-core` + `rqmd-mcp` 0.2.1) linked into the binary (cargo feature `builtin-engine`, on by default).

- **Query side:** rqmd-mcp's `QmdMcpServer` owns one store handle on its own worker thread. mademind mounts its MCP service at `/mcp` and re-creates rqmd-mcp's REST routes (`/query`, `/search`, `/health`), since rqmd-mcp keeps its router private. rmcp's Host-header check is disabled; the auth layer is the access control.
- **Write side:** an `indexer` thread owns a second store handle and runs update and embed jobs one at a time on its own runtime.
- Both handles open the same SQLite index (WAL) with a 30 s busy timeout.
- Collections are passed inline from `config.toml`; opening a store syncs them into the index (added, changed and removed collections).
- Settings reach rqmd and llama.cpp through environment variables set at startup, before any thread exists (§5.3).

### 2.2 external

A separate qmd binary (the original Bun qmd, `@tobilu/qmd`).

- mademind writes the collections as qmd's `index.yml` (JSON, which qmd's YAML parser reads) into a private temp directory and sets `QMD_CONFIG_DIR` for all children.
- **Supervisor:** spawns `<bin> mcp --http --host <host> --port <port>` on loopback, probes TCP readiness (warns after 120 s), and respawns `retry_seconds` after the child exits. Not spawned when `[external] upstream` is set (proxy-only) or `bin` is empty.
- **Proxy:** every request the server doesn't answer itself goes to the upstream with method, path, query, headers and body intact (hop-by-hop headers and `Host` dropped). Responses stream back. `[http] timeout_secs` bounds each request; upstream errors and timeouts are 502 `upstream: <error>`.
- **update / embed:** `<bin> update` and `<bin> embed [-f] [-c <collection>]… [--max-docs-per-batch N] [--max-batch-mb N] [--timeout N]` as child processes.

Known gap: readiness only checks that the port accepts connections, so another process already on that port is mistaken for the child.

## 3. Indexing

- **Watcher:** watches every collection path recursively. Create, modify and remove events on files with a configured extension (`[watcher] extensions`, case-insensitive) arm a debounce; after `debounce_ms` without further events, one update runs. Events during an update queue up and trigger the next one. One update also runs at startup.
- **Rescan:** with `[watcher] rescan_minutes` > 0, an update also runs on that timer, for mounts that deliver no file events (Docker/Podman on macOS). Updates never overlap.
- **Embed:** after `boot_delay_seconds`, then every `interval_minutes`, embeds documents that need it (incremental; `force = true` re-embeds everything). `collections` limits it; empty means all.
- **Freshness:** keyword search follows a save within roughly debounce + update time; semantic search within one embed interval.

## 4. HTTP API

Listens on `0.0.0.0:<port>`. Every request except `/healthz` and `/metrics` passes the auth layer (§6) first.

| Method, path             | Served by | Response                                                                      |
| ------------------------ | --------- | ----------------------------------------------------------------------------- |
| `GET /healthz`           | mademind  | `200 ok`. Open.                                                               |
| `GET /metrics`           | mademind  | Prometheus text format (§7). Open.                                            |
| `GET /file?path=<p>`     | mademind  | The file as `text/plain; charset=utf-8` (§4.2)                                |
| `POST /query`, `/search` | engine    | Search (§4.1)                                                                 |
| `GET /health`            | engine    | `{"status":"ok","uptime":<s>}`                                                |
| `/mcp`                   | engine    | MCP Streamable HTTP (sessions). Tools: `query`, `get`, `multi_get`, `status`. |

### 4.1 Search

Request (`POST /query`; the MCP `query` tool takes the same fields):

| Field                        | Type                        | Meaning                                                                                                                                 |
| ---------------------------- | --------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| `searches`                   | `[{type, query}]`, required | `lex` (BM25: prefix match, `"phrase"`, `-exclude`), `vec` (semantic), `hyde` (hypothetical answer). The first sub-search weighs double. |
| `limit`                      | number                      | Max results                                                                                                                             |
| `collections`                | `[string]`                  | Restrict to these collections                                                                                                           |
| `rerank`                     | bool, default true          | LLM query expansion + rerank on CPU; can take minutes on a new query. Clients normally send false.                                      |
| `intent`                     | string                      | What the caller is after; steers snippets                                                                                               |
| `minScore`, `candidateLimit` | number                      | Score floor; candidates before fusion                                                                                                   |

Response: `{"results": [{"docid", "file", "title", "score", "context", "line", "snippet"}]}`, where `file` is `<collection>/<path>`, `line` is the 1-indexed line of the best match, and `context` is the collection context that applies. Errors: 400 `{"error": …}` for invalid JSON or a missing `searches` array, 500 for engine failures.

### 4.2 Files

`path` (percent-decoded, `+` = space) is one of:

- an absolute path equal to, or below, a collection root;
- a search hit's path: `<collection>/<rel>` or `qmd://<collection>/<rel>`. The engine normalises the paths it returns (e.g. `AGENT_MEMORY.md` → `AGENT-MEMORY.md`); when such a path doesn't exist literally, the builtin engine maps it back to the file on disk.

Anything containing `..`, outside every collection, or naming an unknown collection is 403. Missing `path` is 400, a missing file or a directory 404, files over 2 MiB 413. A served file comes with `x-mademind-file: <collection>/<rel>`: its real path inside the collection (percent-encoded), since hit paths may be normalised.

## 5. Configuration

One TOML file: `$MADEMIND_CONFIG`, default `/config/config.toml`. Every key has a default; unknown keys or invalid TOML make mademind log the error and use the built-in defaults. `config.example.toml` is the short starting point (collections, engine, auth); `config.reference.toml` lists every key with its default and is kept equal to the defaults by a test.

### 5.1 Sections

| Section                | Keys                                                                                                                                                                  |
| ---------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `[collections.<name>]` | `path`, `pattern` (`**/*.md`), `ignore` (gitignore patterns), `context` (path prefix → description), `include_by_default` (true)                                      |
| `[engine]`             | `kind` (`builtin`), `context` (applies to all collections), `cache_dir`, `db`, `threads` (4)                                                                          |
| `[engine.models]`      | `embed`, `rerank`, `generate`: `hf:<org>/<repo>/<file>.gguf` or a local path; empty = engine default                                                                  |
| `[external]`           | `bin` (`/usr/local/bin/qmd`), `host` (`127.0.0.1`), `port` (8181), `retry_seconds` (5), `upstream` (empty)                                                            |
| `[http]`               | `port` (8888), `timeout_secs` (600, external proxy only)                                                                                                              |
| `[watcher]`            | `debounce_ms` (2000), `extensions` (`["md","markdown"]`), `rescan_minutes` (0)                                                                                        |
| `[embed]`              | `enabled` (true), `interval_minutes` (10), `boot_delay_seconds` (60), `force`, `collections`, `max_docs_per_batch`, `max_batch_mb`, `timeout_minutes` (external only) |
| `[auth]`               | `clients` (`[{id, public_key}]`), `rules` (`[{ips, required, clients}]`), `tolerance_secs` (300)                                                                      |

`cache_dir` defaults to `$XDG_CACHE_HOME/mademind`, else `~/.cache/mademind`; `db` to `<cache_dir>/index.sqlite`. Downloaded models go to `<cache_dir>/qmd/models/`.

### 5.2 Environment overrides

Win over the file: `MADEMIND_ENGINE`, `MADEMIND_CACHE_DIR`, `MADEMIND_DB`, `MADEMIND_THREADS`, `MADEMIND_QMD_BIN`, `MADEMIND_UPSTREAM`, `PORT`, `MADEMIND_RESCAN_INTERVAL_MIN`, `MADEMIND_EMBED_INTERVAL_MIN`, `MADEMIND_EMBED_BOOT_DELAY_SEC`, `MADEMIND_AUTH_TOLERANCE_SEC`.

### 5.3 Builtin engine environment

At startup mademind sets `RQMD_CACHE_DIR` and `XDG_CACHE_HOME` = `cache_dir` (rqmd reads the latter for its model directory) and `GGML_N_THREADS` = `threads` (if > 0), and copies `MADEMIND_<name>` to `RQMD_<name>` for: `EMBED_MODEL`, `GENERATE_MODEL`, `RERANK_MODEL`, `FORCE_CPU`, `LLAMA_GPU`, `EMBED_PARALLELISM`, `RERANK_PARALLELISM`, `EMBED_CONTEXT_SIZE`, `RERANK_CONTEXT_SIZE`, `EXPAND_CONTEXT_SIZE`, `EXPAND_USER_MESSAGE_PREFIX`, `EXPAND_SYSTEM_MESSAGE`, `EXPAND_FALLBACK_HYDE_TEMPLATE`, `EXPAND_TEMP`, `EXPAND_TOP_K`, `EXPAND_TOP_P`.

## 6. Authentication

### 6.1 Rules

`[auth] rules` are checked top to bottom against the client's IP address (IPv4-mapped IPv6 is treated as IPv4). `ips` holds CIDRs, bare addresses (exact match) or `any`. The first matching rule decides:

- no rule matches → 403;
- `required = false` → allowed without a signature;
- `required = true` → the request must name a client listed in the rule's `clients` and carry a valid signature, else 401.

No rules at all disables auth. A rule naming a client not defined in `[auth] clients` aborts startup. Without a config file the default is one fail-closed rule (`any`, required, no clients). The example config opens loopback and requires signatures from everyone else.

### 6.2 Signatures

Headers:

```
x-mademind-client:    <client id>
x-mademind-timestamp: <unix seconds>
x-mademind-signature: base64(Ed25519 signature)
```

Signed message (UTF-8, `\n`-separated, no trailing newline):

```
mademind-auth-v1
<client id>
<timestamp>
<METHOD> <path?query exactly as sent>
<lowercase hex sha256 of the request body>
```

The server rejects timestamps more than `tolerance_secs` from its clock, unknown clients, bad signatures, and a signature it has already accepted within the window (replay). Ed25519 is deterministic, so two identical requests in the same second are rejected as a replay. Bodies up to 16 MiB are buffered for the check.

Keys: `[auth] clients` holds OpenSSH public key bodies (`ssh-ed25519 AAAA…`). The clients store keys as 64-byte hex (32-byte seed ‖ 32-byte public key) in `~/.config/mademind/<id>.key`; `clients/mademind-auth.ts genkey` creates one.

## 7. Metrics

`GET /metrics`, Prometheus text format 0.0.4:

| Metric                                           | Type    | Labels              |
| ------------------------------------------------ | ------- | ------------------- |
| `mademind_info`                                  | gauge   | `version`, `engine` |
| `mademind_http_requests_total`                   | counter | `path`, `code`      |
| `mademind_auth_rejections_total`                 | counter | `code`              |
| `mademind_watcher_events_total`                  | counter |                     |
| `mademind_update_cycles_total`                   | counter | `result`            |
| `mademind_update_last_duration_seconds`          | gauge   |                     |
| `mademind_update_last_success_timestamp_seconds` | gauge   |                     |
| `mademind_embed_cycles_total`                    | counter | `result`            |
| `mademind_embed_last_duration_seconds`           | gauge   |                     |
| `mademind_embed_last_success_timestamp_seconds`  | gauge   |                     |
| `mademind_embed_chunks_total`                    | counter |                     |
| `mademind_embed_documents_total`                 | counter |                     |
| `mademind_mcp_ready`                             | gauge   | (external)          |
| `mademind_mcp_spawns_total`                      | counter | (external)          |

`path` labels longer than 64 characters or with unusual characters become `<other>`.

## 8. Clients

| File                               | Purpose                                                                                           |
| ---------------------------------- | ------------------------------------------------------------------------------------------------- |
| `clients/lib/auth.ts`              | Request signing: key loading, Ed25519 via `node:crypto` or the `openssl` CLI (≥ 3.0)              |
| `clients/lib/config.ts`            | Per-machine settings (`client.json` + env): server, client id, key file, local roots              |
| `clients/mademind-auth.ts`         | CLI: `genkey [id]`, `request METHOD PATH [BODY]`, `sign <message>`, `backend`                     |
| `clients/mcp-stdio-proxy.ts`       | stdio ↔ Streamable HTTP MCP bridge; signs requests, defaults `rerank` to false                    |
| `clients/pi-extension/mademind.ts` | pi tools `mademind_search` and `mademind_read`; single file with its own copy of the signing code |

All run on Bun, or Node ≥ 22.6 with `--experimental-strip-types`. Per-machine settings live in `~/.config/mademind/client.json` (or `$MADEMIND_CLIENT_CONFIG`): `url` (default `http://127.0.0.1:8888`), `client_id` (default: short hostname), `key_file` (default `~/.config/mademind/<client_id>.key`), `local_roots` (collection → folder where this machine has a copy). Environment overrides: `MADEMIND_URL`, `MADEMIND_CLIENT_ID`, `MADEMIND_SIGN_KEY` / `MADEMIND_SIGN_KEY_FILE`, `MADEMIND_LOCAL_ROOTS` (`name=/path,…`), plus `MADEMIND_AUTH_BACKEND` (`node`/`openssl`). Reading a file reports its real collection path and, if `local_roots` maps its collection and the file exists there, its path on this machine — never a guessed one.

## 9. Process

- `mademind` runs the server; `mademind healthcheck` exits 0 if the local server answers `/healthz` (used by the Docker health check).
- SIGTERM or Ctrl-C: graceful HTTP shutdown, engine shutdown, the external child is stopped.
- Startup errors (unknown engine, unbindable port, engine that fails to open) exit with status 2; an auth rule with an undefined client exits 3.

## 10. Builds and releases

| Artifact                        | How                                                                                                                                                                                                                                                                                                                     |
| ------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Dockerfile` (default)          | mademind with the builtin engine on `debian:trixie-slim`; cache at `/root/.cache/mademind`                                                                                                                                                                                                                              |
| `Dockerfile.classic`            | mademind built without the builtin engine, plus qmd on Bun (`kind = "external"`)                                                                                                                                                                                                                                        |
| `scripts/dist.sh`, `make dist`  | Release archives: Linux glibc (Debian bookworm container; glibc ≥ 2.34; libgomp static), Linux musl (zig; static), macOS and Windows (native toolchains in CI)                                                                                                                                                          |
| `.github/workflows/release.yml` | On `vX.Y.Z` tags: builds seven platforms (no Windows arm64 yet; it runs the x86_64 build) and publishes a GitHub Release with `SHA256SUMS`; builds the image for amd64 + arm64 and the classic image for amd64 and pushes `axxapy/mademind` to Docker Hub (`X.Y.Z`, `X.Y`, `latest`; `-classic` variants and `classic`) |
| `.github/workflows/ci.yml`      | On pull requests and `master`: fmt, clippy, tests for both feature sets; client and script checks                                                                                                                                                                                                                       |

`llama-cpp-2` and `llama-cpp-sys-2` are pinned to `=0.1.146`, the version rqmd-core 0.2.1 builds against. On Linux, OpenSSL is compiled in (`vendored`), used by rqmd's model downloader.
