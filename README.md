# mademind

Your notes, searchable by your AI agents.

Point mademind at a folder of markdown files and it keeps a search index of them — keyword and semantic — that agents can query over MCP or plain HTTP. Your notes stay plain files wherever they already live (a git repo, an Obsidian vault, a Syncthing folder); mademind only reads them, and re-indexes a couple of seconds after you save.

It's one binary. Search runs on [rqmd](https://github.com/stn/rqmd), a Rust port of Tobi Lütke's [qmd](https://github.com/tobi/qmd), compiled right in — no Python, no Node, no GPU needed. Prefer the original? There's a **classic** mode that runs qmd itself (Bun) as the search engine instead — same API, same config; see [Two engines](#two-engines).

## Quick start

With Docker (images for amd64 and arm64 on [Docker Hub](https://hub.docker.com/r/axxapy/mademind)):

```sh
cp config.example.toml config.toml        # one collection: ./notes
NOTES_DIR=~/notes docker compose up -d    # pulls axxapy/mademind:latest
curl localhost:8888/healthz               # ok
```

Without compose:

```sh
docker run -d --name mademind --network host \
  -v "$PWD/config.toml:/config/config.toml:ro" \
  -v ~/notes:/data/notes:ro \
  -v mademind-cache:/root/.cache/mademind \
  axxapy/mademind:latest
```

On macOS (Docker Desktop, Podman) there's no host networking: publish the port instead (`-p 127.0.0.1:8888:8888`). Requests then don't arrive from loopback, so even local clients sign them (see [Other machines](#other-machines)). File events don't cross into the VM either; set `[watcher] rescan_minutes = 5` so edits still get picked up.

Tags: `latest`, `X.Y.Z`, `X.Y`; `classic` (and `X.Y.Z-classic`) runs the original qmd engine instead, amd64 only.

Or grab a binary from [Releases](../../releases) and run it:

```sh
MADEMIND_CONFIG=./config.toml ./mademind
```

Edit `config.toml` to point `[collections.notes] path` at your notes (or add more collections). Keyword search works right away; about a minute after start mademind downloads a ~300 MB embedding model and semantic search follows once your notes are embedded.

## Connect your agent

**Claude Code** (or any MCP client that speaks stdio):

```sh
claude mcp add mademind -- bun /path/to/mademind/clients/mcp-stdio-proxy.ts http://localhost:8888/mcp
```

The proxy is a small zero-dependency script; it also signs requests when you talk to mademind on another machine (below).

**pi** — drop `clients/pi-extension/mademind.ts` into pi's extensions; you get `mademind_search` and `mademind_read` tools.

**Anything else** — it's HTTP:

```sh
curl localhost:8888/query -H 'content-type: application/json' \
  -d '{"searches":[{"type":"lex","query":"sourdough"},{"type":"vec","query":"how do I feed the starter"}]}'
curl 'localhost:8888/file?path=notes/bread.md'
```

[AGENTS.md](AGENTS.md) has the details agents need; [SPEC.md](SPEC.md) has everything.

## Other machines

Requests from the machine mademind runs on are trusted; everyone else has to sign them. On the client:

```sh
bun clients/mademind-auth.ts genkey laptop
```

That saves a private key to `~/.config/mademind/laptop.key` and prints a line to paste into the server's `config.toml` under `[auth] clients` (and add `"laptop"` to a rule's `clients`). The clients above pick the key up automatically.

Then tell the clients on that machine where the server is (and, optionally, where this machine has its own copy of each collection, so agents get real local paths to edit) in `~/.config/mademind/client.json`:

```json
{
  "url": "http://notes-server:8888",
  "client_id": "laptop",
  "local_roots": { "notes": "~/notes" }
}
```

A local path is only shown when the file is actually there, so a machine without a copy never gets a wrong one.

## Two engines

- **builtin** (default) — rqmd inside mademind. One process, nothing else to install.
- **external**, a.k.a. **classic** — runs the original Bun-based qmd as a child process and proxies to it. Use the `axxapy/mademind:classic` image (`docker compose up -d mademind-classic`, or `Dockerfile.classic` to build it) if you want that.

## Building

```sh
cargo build --release              # needs cmake and a C/C++ compiler (llama.cpp)
cargo build --release --no-default-features   # external engine only, no llama.cpp
make test lint                     # what CI runs
make dist                          # Linux release archives (docker + zig)
```

Pushing a `vX.Y.Z` tag builds all platforms, publishes a GitHub release and pushes the images to Docker Hub.

## License

AGPL-3.0. Search comes from [rqmd](https://github.com/stn/rqmd) (MIT, Akira Ishino), a port of [qmd](https://github.com/tobi/qmd) (MIT, Tobi Lütke) — thanks to both.
