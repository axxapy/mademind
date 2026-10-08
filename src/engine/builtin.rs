//! Builtin engine: rqmd (rqmd-core + rqmd-mcp) linked into this binary.
//!
//! Two handles on the same SQLite index (WAL):
//!   * the query side — rqmd-mcp's `QmdMcpServer`, which moves its store onto
//!     its own worker thread; serves /mcp, /query, /search, /health.
//!   * the write side — owned by the "indexer" thread, which runs `update` and
//!     `embed` jobs one at a time on its own runtime. Serializing them avoids
//!     SQLITE_BUSY between the two writers, and keeps blocking SQLite and
//!     llama.cpp work off the HTTP runtime.
//!
//! rqmd-mcp keeps its axum router private, so `router()` rebuilds the same
//! surface from its public pieces (mirrors rqmd-mcp 0.2.1 `http.rs`, MIT).

use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rqmd_core::{
    EmbedResult, RqmdStore, RqmdStoreOptions, StoreOpsEmbedOptions, UpdateOptions, UpdateResult,
};
use rqmd_mcp::QmdMcpServer;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use super::mcp::{Instructions, McpServer};
use super::EmbedStats;
use crate::config::{Config, EmbedConfig, EngineConfig};

/// How long a writer waits on a locked database before SQLITE_BUSY.
const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

enum Job {
    Update(oneshot::Sender<anyhow::Result<UpdateResult>>),
    Embed(
        Vec<StoreOpsEmbedOptions>,
        oneshot::Sender<anyhow::Result<EmbedResult>>,
    ),
}

pub struct Builtin {
    jobs: mpsc::Sender<Job>,
    server: QmdMcpServer,
    instructions: Instructions,
    started: Instant,
}

/// rqmd's tuning knobs, settable as MADEMIND_<name> (passed on as RQMD_<name>).
const RQMD_PASSTHROUGH: &[&str] = &[
    "EMBED_MODEL",
    "GENERATE_MODEL",
    "RERANK_MODEL",
    "FORCE_CPU",
    "LLAMA_GPU",
    "EMBED_PARALLELISM",
    "RERANK_PARALLELISM",
    "EMBED_CONTEXT_SIZE",
    "RERANK_CONTEXT_SIZE",
    "EXPAND_CONTEXT_SIZE",
    "EXPAND_USER_MESSAGE_PREFIX",
    "EXPAND_SYSTEM_MESSAGE",
    "EXPAND_FALLBACK_HYDE_TEMPLATE",
    "EXPAND_TEMP",
    "EXPAND_TOP_K",
    "EXPAND_TOP_P",
];

/// Translate mademind's settings into the environment rqmd and llama.cpp
/// read. Must run before any thread starts (set_var is not thread-safe).
pub fn export_env(cfg: &EngineConfig) {
    let cache = cfg.cache_dir();
    std::env::set_var("RQMD_CACHE_DIR", &cache);
    // rqmd keeps downloaded models in $XDG_CACHE_HOME/qmd/models (it ignores
    // RQMD_CACHE_DIR for them); point that into the cache dir too, so the
    // index and models live together (and in the Docker volume).
    std::env::set_var("XDG_CACHE_HOME", &cache);
    if cfg.threads > 0 {
        std::env::set_var("GGML_N_THREADS", cfg.threads.to_string());
    }
    for name in RQMD_PASSTHROUGH {
        if let Some(v) = std::env::var_os(format!("MADEMIND_{name}")) {
            std::env::set_var(format!("RQMD_{name}"), v);
        }
    }
}

fn open_store(opts: RqmdStoreOptions) -> anyhow::Result<RqmdStore> {
    let store = RqmdStore::open(opts).context("opening rqmd index")?;
    store
        .internal()
        .with_connection(|c| c.busy_timeout(BUSY_TIMEOUT))
        .context("setting busy timeout")?;
    Ok(store)
}

impl Builtin {
    pub fn open(cfg: &Config) -> anyhow::Result<Builtin> {
        let db_path = cfg.engine.db_path();
        if let Some(dir) = db_path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        // Collections come inline from config.toml; opening a store syncs them
        // into the index (added, changed and removed collections alike).
        let index: rqmd_core::ConfigData =
            serde_json::from_value(cfg.index_json()).context("collections config")?;
        eprintln!(
            "mademind: builtin engine: index {db_path:?}, collections: {}",
            cfg.collections
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
        let opts = RqmdStoreOptions {
            db_path,
            config_path: None,
            config: Some(index),
        };
        // Query side first: opening syncs the collections file into the DB.
        let server = QmdMcpServer::new(open_store(opts.clone())?);
        let writer = open_store(opts)?;
        let instructions = Instructions::new(&server);
        instructions.refresh(&writer);

        let (jobs, rx) = mpsc::channel::<Job>();
        let live = instructions.clone();
        thread::Builder::new()
            .name("indexer".into())
            .spawn(move || run_indexer(writer, rx, live))
            .context("spawning indexer thread")?;
        Ok(Builtin {
            jobs,
            server,
            instructions,
            started: Instant::now(),
        })
    }

    pub fn router(&self) -> Router {
        let server = McpServer::new(self.server.clone(), self.instructions.clone());
        // Host validation is off: requests reach this router only through the
        // auth layer, and LAN clients send their own Host (rmcp's default
        // allows loopback names only).
        let mcp = StreamableHttpService::new(
            move || Ok(server.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default().disable_allowed_hosts(),
        );
        let handle = self.server.handle();
        let query = move |body: Bytes| {
            let handle = handle.clone();
            async move {
                let params: Value = match serde_json::from_slice(&body) {
                    Ok(v) => v,
                    Err(_) => return error_json(StatusCode::BAD_REQUEST, "Invalid JSON body"),
                };
                if !params.get("searches").map(Value::is_array).unwrap_or(false) {
                    return error_json(
                        StatusCode::BAD_REQUEST,
                        "Missing required field: searches (array)",
                    );
                }
                // The args type is rqmd-mcp's (unnameable here); inferred from run_query.
                let args = match serde_json::from_value(params) {
                    Ok(a) => a,
                    Err(e) => {
                        return error_json(
                            StatusCode::BAD_REQUEST,
                            &format!("Invalid request: {e}"),
                        )
                    }
                };
                match handle.run_query(args).await {
                    Ok(items) => Json(json!({ "results": items })).into_response(),
                    Err(e) => error_json(StatusCode::INTERNAL_SERVER_ERROR, &e.message),
                }
            }
        };
        let started = self.started;
        Router::new()
            .route(
                "/health",
                get(move || async move {
                    Json(json!({ "status": "ok", "uptime": started.elapsed().as_secs() }))
                }),
            )
            .route("/query", post(query.clone()))
            .route("/search", post(query))
            .nest_service("/mcp", mcp)
            .fallback(|| async { (StatusCode::NOT_FOUND, "Not Found") })
    }

    pub async fn update(&self) -> anyhow::Result<String> {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(Job::Update(tx))
            .context("indexer thread gone")?;
        let r = rx.await.context("indexer thread gone")??;
        Ok(format!(
            "{} collection(s): {} new, {} updated, {} unchanged, {} removed; {} need embedding",
            r.collections, r.indexed, r.updated, r.unchanged, r.removed, r.needs_embedding
        ))
    }

    pub async fn embed(&self, cfg: &EmbedConfig) -> anyhow::Result<EmbedStats> {
        let base = StoreOpsEmbedOptions {
            force: cfg.force,
            max_docs_per_batch: cfg.max_docs_per_batch.map(|n| n as usize),
            max_batch_bytes: cfg.max_batch_mb.map(|mb| mb as usize * 1024 * 1024),
            ..Default::default()
        };
        // rqmd embeds one collection or all of them per call.
        let opts = if cfg.collections.is_empty() {
            vec![base]
        } else {
            cfg.collections
                .iter()
                .map(|c| StoreOpsEmbedOptions {
                    collection: Some(c.clone()),
                    ..base.clone()
                })
                .collect()
        };
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(Job::Embed(opts, tx))
            .context("indexer thread gone")?;
        let r = rx.await.context("indexer thread gone")??;
        Ok(EmbedStats {
            chunks: Some(r.chunks_embedded as u64),
            docs: Some(r.docs_processed as u64),
            summary: format!(
                "{} chunk(s) from {} document(s), {} error(s)",
                r.chunks_embedded, r.docs_processed, r.errors
            ),
        })
    }

    /// Tear down the query side's LLM workers.
    pub async fn shutdown(&self) {
        self.server.shutdown().await;
    }
}

fn error_json(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

/// Owns the write-side store; runs jobs in arrival order until the engine is
/// dropped. After each job, refreshes the MCP instructions from the index.
fn run_indexer(mut store: RqmdStore, rx: mpsc::Receiver<Job>, instructions: Instructions) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("mademind: indexer runtime failed: {e}");
            return;
        }
    };
    while let Ok(job) = rx.recv() {
        match job {
            Job::Update(reply) => {
                let r = rt.block_on(store.update(UpdateOptions::default()));
                instructions.refresh(&store);
                let _ = reply.send(r.map_err(Into::into));
            }
            Job::Embed(opts, reply) => {
                let r = rt.block_on(async {
                    let mut total = EmbedResult {
                        docs_processed: 0,
                        chunks_embedded: 0,
                        errors: 0,
                        failures: vec![],
                        duration_ms: 0,
                    };
                    for o in opts {
                        let r = store.embed(o).await?;
                        total.docs_processed += r.docs_processed;
                        total.chunks_embedded += r.chunks_embedded;
                        total.errors += r.errors;
                        total.failures.extend(r.failures);
                        total.duration_ms += r.duration_ms;
                    }
                    Ok::<_, rqmd_core::Error>(total)
                });
                instructions.refresh(&store);
                let _ = reply.send(r.map_err(Into::into));
            }
        }
    }
    rt.block_on(store.close());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CollectionConfig;
    use axum::body::Body;
    use axum::http::Request;
    use std::path::PathBuf;
    use tower::ServiceExt;

    fn tmpdir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("mademind-builtin-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Index two notes, then find one through /query with a keyword search
    /// (BM25 only: no models needed).
    #[tokio::test(flavor = "multi_thread")]
    async fn update_then_query_finds_note() {
        let d = tmpdir("query");
        let notes = d.join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(
            notes.join("a.md"),
            "# Sourdough\n\nFeed the starter twice a day.\n",
        )
        .unwrap();
        std::fs::write(notes.join("b.md"), "# Bikes\n\nChain lube every 300 km.\n").unwrap();
        let mut cfg = Config::default();
        cfg.engine.db = d.join("index.sqlite").to_string_lossy().into();
        cfg.collections.insert(
            "notes".into(),
            CollectionConfig {
                path: notes.to_string_lossy().into(),
                ..CollectionConfig::default()
            },
        );
        let engine = Builtin::open(&cfg).unwrap();
        let summary = engine.update().await.unwrap();
        assert!(summary.contains("2 new"), "{summary}");

        let body = r#"{"searches":[{"type":"lex","query":"starter"}],"limit":5,"rerank":false}"#;
        let resp = engine
            .router()
            .oneshot(
                Request::post("/query")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "{v}");
        assert!(
            results[0]["file"].as_str().unwrap().ends_with("a.md"),
            "{v}"
        );

        // A new MCP session sees the index as it is now, not as it was at
        // startup (0 documents).
        let init = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
        let resp = engine
            .router()
            .oneshot(
                Request::post("/mcp")
                    .header("host", "localhost")
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .body(Body::from(init))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("over 2 markdown documents"), "{text}");
        assert!(!text.contains("rqmd embed"), "{text}");

        // Missing searches -> 400, same as qmd.
        let resp = engine
            .router()
            .oneshot(Request::post("/query").body(Body::from("{}")).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        engine.shutdown().await;
        let _ = std::fs::remove_dir_all(&d);
    }
}
