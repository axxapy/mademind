//! Search engines behind one interface: the builtin rqmd (linked in, runs
//! in-process) or an external qmd binary (child processes + reverse proxy).

#[cfg(feature = "builtin-engine")]
pub mod builtin;
pub mod external;
#[cfg(feature = "builtin-engine")]
mod mcp;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;

use crate::config::EmbedConfig;
use crate::metrics::{metric_add, metric_set, unix_now};

pub struct EmbedStats {
    pub chunks: Option<u64>,
    pub docs: Option<u64>,
    pub summary: String,
}

pub enum Engine {
    #[cfg(feature = "builtin-engine")]
    Builtin(builtin::Builtin),
    External(Arc<external::External>),
}

impl Engine {
    pub fn name(&self) -> &'static str {
        match self {
            #[cfg(feature = "builtin-engine")]
            Engine::Builtin(_) => "builtin",
            Engine::External(_) => "external",
        }
    }

    /// Service for every request the server doesn't answer itself
    /// (/mcp, /query, /search, /health).
    pub fn router(&self) -> Router {
        match self {
            #[cfg(feature = "builtin-engine")]
            Engine::Builtin(b) => b.router(),
            Engine::External(e) => external::router(Arc::clone(e)),
        }
    }

    /// Incremental re-index of all collections; returns a one-line summary.
    pub async fn update(&self) -> anyhow::Result<String> {
        match self {
            #[cfg(feature = "builtin-engine")]
            Engine::Builtin(b) => b.update().await,
            Engine::External(e) => e.update().await,
        }
    }

    pub async fn embed(&self, cfg: &EmbedConfig) -> anyhow::Result<EmbedStats> {
        match self {
            #[cfg(feature = "builtin-engine")]
            Engine::Builtin(b) => b.embed(cfg).await,
            Engine::External(e) => e.embed(cfg).await,
        }
    }

    pub async fn shutdown(&self) {
        #[cfg(feature = "builtin-engine")]
        if let Engine::Builtin(b) = self {
            b.shutdown().await;
        }
    }
}

/// Serialises updates: the watcher and the rescan timer can fire together.
static UPDATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One update cycle with logging and metrics (the watcher's action).
pub async fn run_update(engine: &Engine) {
    let _guard = UPDATE_LOCK.lock().await;
    let started = Instant::now();
    eprintln!("mademind: update triggered");
    match engine.update().await {
        Ok(summary) => {
            let dur = started.elapsed().as_secs_f64();
            eprintln!("mademind: update ok in {dur:.2}s — {summary}");
            metric_add("mademind_update_cycles_total", &[("result", "ok")], 1.0);
            metric_set("mademind_update_last_duration_seconds", &[], dur);
            metric_set(
                "mademind_update_last_success_timestamp_seconds",
                &[],
                unix_now() as f64,
            );
        }
        Err(e) => {
            metric_add("mademind_update_cycles_total", &[("result", "failed")], 1.0);
            eprintln!("mademind: update failed: {e:#}");
        }
    }
}

/// Re-index every `minutes`, for mounts that deliver no file events (e.g.
/// Docker/Podman on macOS). Updates are incremental, so a quiet tick is cheap.
pub async fn run_rescan_loop(engine: Arc<Engine>, minutes: u64) {
    eprintln!("mademind: rescan loop: every {minutes}min");
    let period = Duration::from_secs(minutes * 60);
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        run_update(&engine).await;
    }
}

/// Embed every `interval_minutes`, after `boot_delay_seconds`.
pub async fn run_embed_loop(engine: Arc<Engine>, cfg: EmbedConfig) {
    if !cfg.enabled {
        eprintln!("mademind: embed loop disabled by config");
        return;
    }
    eprintln!(
        "mademind: embed loop: every {}min, boot delay {}s",
        cfg.interval_minutes, cfg.boot_delay_seconds
    );
    tokio::time::sleep(Duration::from_secs(cfg.boot_delay_seconds)).await;
    loop {
        let started = Instant::now();
        match engine.embed(&cfg).await {
            Ok(stats) => {
                let dur = started.elapsed().as_secs_f64();
                eprintln!("mademind: embed ok in {dur:.1}s — {}", stats.summary);
                metric_add("mademind_embed_cycles_total", &[("result", "ok")], 1.0);
                metric_set("mademind_embed_last_duration_seconds", &[], dur);
                metric_set(
                    "mademind_embed_last_success_timestamp_seconds",
                    &[],
                    unix_now() as f64,
                );
                if let Some(c) = stats.chunks {
                    metric_add("mademind_embed_chunks_total", &[], c as f64);
                }
                if let Some(d) = stats.docs {
                    metric_add("mademind_embed_documents_total", &[], d as f64);
                }
            }
            Err(e) => {
                metric_add("mademind_embed_cycles_total", &[("result", "failed")], 1.0);
                eprintln!("mademind: embed failed: {e:#}");
            }
        }
        tokio::time::sleep(Duration::from_secs(cfg.interval_minutes * 60)).await;
    }
}
