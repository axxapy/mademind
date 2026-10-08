//! mademind: one binary that serves your notes to AI agents on one port.
//!
//!   http (tokio, 0.0.0.0:$PORT) : auth -> /healthz, /metrics, /file, else the engine
//!   engine "builtin"            : rqmd linked in; search, update and embed in-process
//!   engine "external"           : a qmd binary, supervised on loopback and proxied
//!   thread "watcher"            : debounced re-index when a note changes
//!   task "rescan"               : re-index every [watcher] rescan_minutes (if > 0)
//!   task "embed"                : vector embedding every [embed] interval_minutes
//!
//! Run the test suite:  cargo test   (no Docker, no models, no network needed)

mod auth;
mod config;
mod engine;
mod files;
mod http;
mod metrics;
mod watcher;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use config::{apply_env_overrides, config_path, load_config, Config, EngineKind};
use engine::Engine;

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("mademind: {msg}");
    std::process::exit(2);
}

fn main() {
    let mut cfg = load_config(&config_path());
    apply_env_overrides(&mut cfg);
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        std::process::exit(healthcheck(cfg.http.port));
    }
    let Some(kind) = EngineKind::parse(&cfg.engine.kind) else {
        die(format!(
            "unknown engine {:?} (want builtin or external)",
            cfg.engine.kind
        ));
    };
    // The builtin engine (rqmd, llama.cpp) reads its settings from the
    // environment; set it before any thread exists.
    #[cfg(feature = "builtin-engine")]
    if kind == EngineKind::Builtin {
        engine::builtin::export_env(&cfg.engine);
    }
    // external: qmd reads its collections from $QMD_CONFIG_DIR/index.yml,
    // generated here from config.toml and inherited by every qmd child.
    if kind == EngineKind::External {
        let dir = std::env::temp_dir().join(format!("mademind-qmd-{}", std::process::id()));
        if let Err(e) = engine::external::write_index_config(&cfg, &dir) {
            die(format!("external engine: {e:#}"));
        }
        std::env::set_var("QMD_CONFIG_DIR", &dir);
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| die(format!("runtime: {e}")));
    let _rt_guard = rt.enter();

    let stop = Arc::new(AtomicBool::new(false));
    let (engine, supervisor) = open_engine(kind, &cfg, &stop);
    let engine = Arc::new(engine);
    eprintln!("mademind: engine {}", engine.name());
    metrics::metric_set(
        "mademind_info",
        &[
            ("version", env!("CARGO_PKG_VERSION")),
            ("engine", engine.name()),
        ],
        1.0,
    );

    let roots = cfg.data_roots();
    {
        let (engine, roots, handle) = (Arc::clone(&engine), roots.clone(), rt.handle().clone());
        let window = Duration::from_millis(cfg.watcher.debounce_ms);
        let exts = cfg.watcher.extensions.clone();
        thread::Builder::new()
            .name("watcher".into())
            .spawn(move || {
                watcher::run_watcher(&roots, window, exts, || {
                    handle.block_on(engine::run_update(&engine))
                })
            })
            .unwrap_or_else(|e| die(format!("spawn watcher: {e}")));
    }

    let state = Arc::new(http::AppState {
        collections: cfg.collection_roots(),
        auth: auth::load_auth_state(&cfg.auth),
    });
    let app = http::router(state, engine.router());
    let port = cfg.http.port;
    rt.block_on(async {
        tokio::spawn(engine::run_embed_loop(
            Arc::clone(&engine),
            cfg.embed.clone(),
        ));
        if cfg.watcher.rescan_minutes > 0 {
            tokio::spawn(engine::run_rescan_loop(
                Arc::clone(&engine),
                cfg.watcher.rescan_minutes,
            ));
        }
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
            .await
            .unwrap_or_else(|e| die(format!("cannot bind :{port}: {e}")));
        eprintln!("mademind: http on :{port}");
        let served = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal())
        .await;
        if let Err(e) = served {
            eprintln!("mademind: http server error: {e}");
        }
        engine.shutdown().await;
    });
    stop.store(true, Ordering::Relaxed);
    if let Some(h) = supervisor {
        let _ = h.join();
    }
    eprintln!("mademind: stopped");
}

fn open_engine(
    kind: EngineKind,
    cfg: &Config,
    stop: &Arc<AtomicBool>,
) -> (Engine, Option<thread::JoinHandle<()>>) {
    match kind {
        #[cfg(feature = "builtin-engine")]
        EngineKind::Builtin => match engine::builtin::Builtin::open(cfg) {
            Ok(b) => (Engine::Builtin(b), None),
            Err(e) => die(format!("builtin engine: {e:#}")),
        },
        #[cfg(not(feature = "builtin-engine"))]
        EngineKind::Builtin => {
            die("built without the builtin engine (feature builtin-engine); set [engine] kind = \"external\"")
        }
        EngineKind::External => {
            let ext = engine::external::External::new(
                &cfg.external,
                Duration::from_secs(cfg.http.timeout_secs),
            )
            .unwrap_or_else(|e| die(format!("external engine: {e:#}")));
            eprintln!("mademind: external engine -> {}", ext.upstream());
            // Proxy-only when an upstream is configured: nothing to supervise.
            let supervisor = (cfg.external.upstream.is_empty() && !cfg.external.bin.is_empty())
                .then(|| {
                    let x = cfg.external.clone();
                    let stop = Arc::clone(stop);
                    thread::Builder::new()
                        .name("supervisor".into())
                        .spawn(move || {
                            engine::external::run_supervisor(
                                &x.bin,
                                &x.host,
                                x.port,
                                Duration::from_secs(x.retry_seconds),
                                stop,
                            )
                        })
                        .unwrap_or_else(|e| die(format!("spawn supervisor: {e}")))
                });
            (Engine::External(Arc::new(ext)), supervisor)
        }
    }
}

/// `mademind healthcheck`: exit 0 when the local server answers /healthz
/// (the container healthcheck; the slim image has no curl).
fn healthcheck(port: u16) -> i32 {
    use std::io::{Read, Write};
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5)) else {
        return 1;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut resp = String::new();
    let ok = s
        .write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .and_then(|_| s.read_to_string(&mut resp))
        .is_ok();
    i32::from(!(ok && resp.starts_with("HTTP/1.") && resp.contains(" 200 ")))
}

/// Resolves on Ctrl-C, or SIGTERM (`docker stop`) on Unix.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    eprintln!("mademind: shutting down");
}
