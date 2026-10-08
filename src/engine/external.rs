//! External engine: a separate qmd binary (the original Bun qmd). mademind
//! spawns and supervises its `mcp --http` server on loopback, reverse-proxies
//! to it, and runs its `update` / `embed` commands as child processes.
//! Proxy-only mode ([external] upstream set) spawns nothing.

use std::io;
use std::process::{Child, Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;

use super::EmbedStats;
use crate::config::{Config, EmbedConfig, ExternalConfig};
use crate::metrics::{metric_add, metric_set};

/// Largest request body forwarded upstream.
const MAX_BODY: usize = 16 * 1024 * 1024;

pub struct External {
    bin: String,
    upstream: String,
    client: reqwest::Client,
}

impl External {
    pub fn new(cfg: &ExternalConfig, timeout: Duration) -> anyhow::Result<External> {
        let upstream = if cfg.upstream.is_empty() {
            format!("http://{}:{}", cfg.host, cfg.port)
        } else {
            cfg.upstream.trim_end_matches('/').to_string()
        };
        let builder = reqwest::Client::builder().timeout(timeout);
        // reqwest's 30s TCP_USER_TIMEOUT default (a Linux-family socket
        // option) would cut slow CPU queries short; `timeout` is the only cap.
        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        let builder = builder.tcp_user_timeout(None);
        let client = builder.build().context("building upstream client")?;
        Ok(External {
            bin: cfg.bin.clone(),
            upstream,
            client,
        })
    }

    pub fn upstream(&self) -> &str {
        &self.upstream
    }

    pub async fn update(&self) -> anyhow::Result<String> {
        let out = self.run(vec!["update".into()]).await?;
        let text = output_text(&out);
        if !out.status.success() {
            bail!(
                "{}: {}",
                out.status,
                text.chars().take(500).collect::<String>()
            );
        }
        Ok(text
            .lines()
            .filter(|l| l.contains("Indexed:") || l.to_lowercase().contains("error"))
            .collect::<Vec<_>>()
            .join(" | ")
            .chars()
            .take(300)
            .collect())
    }

    pub async fn embed(&self, cfg: &EmbedConfig) -> anyhow::Result<EmbedStats> {
        let out = self.run(embed_args(cfg)).await?;
        let text = output_text(&out);
        let tail: String = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" | ")
            .chars()
            .take(300)
            .collect();
        if !out.status.success() {
            bail!("{}: {tail}", out.status);
        }
        let counts = parse_embed_summary(&text);
        Ok(EmbedStats {
            chunks: counts.map(|c| c.0),
            docs: counts.map(|c| c.1),
            summary: tail,
        })
    }

    /// Run `<bin> <args>` to completion off the async runtime.
    async fn run(&self, args: Vec<String>) -> anyhow::Result<Output> {
        if self.bin.is_empty() {
            bail!("no [external] bin configured");
        }
        let bin = self.bin.clone();
        tokio::task::spawn_blocking(move || retry_busy(|| Command::new(&bin).args(&args).output()))
            .await?
            .with_context(|| format!("running {}", self.bin))
    }
}

/// Write the collections from config.toml as qmd's index.yml (JSON, which
/// qmd's YAML parser reads) into `dir`, for QMD_CONFIG_DIR.
pub fn write_index_config(cfg: &Config, dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let text = serde_json::to_string_pretty(&cfg.index_json())?;
    let file = dir.join("index.yml");
    std::fs::write(&file, text).with_context(|| format!("writing {}", file.display()))
}

fn output_text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// Retry a spawn that hit ETXTBSY: the binary was just written and another
/// thread's fork still holds it open for writing (a short-lived race).
fn retry_busy<T>(mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    const ETXTBSY: i32 = 26;
    let mut tries = 0;
    loop {
        match f() {
            Err(e) if e.raw_os_error() == Some(ETXTBSY) && tries < 20 => {
                tries += 1;
                thread::sleep(Duration::from_millis(50));
            }
            r => return r,
        }
    }
}

/// CLI args for `qmd embed`.
fn embed_args(e: &EmbedConfig) -> Vec<String> {
    let mut a = vec!["embed".to_string()];
    if e.force {
        a.push("-f".into());
    }
    for c in &e.collections {
        a.push("-c".into());
        a.push(c.clone());
    }
    if let Some(n) = e.max_docs_per_batch {
        a.push("--max-docs-per-batch".into());
        a.push(n.to_string());
    }
    if let Some(n) = e.max_batch_mb {
        a.push("--max-batch-mb".into());
        a.push(n.to_string());
    }
    if let Some(n) = e.timeout_minutes {
        a.push("--timeout".into());
        a.push(n.to_string());
    }
    a
}

/// Parse "Embedded 5 chunks from 3 documents" out of qmd's embed output.
/// Tolerates progress-bar noise around it; the last match wins.
fn parse_embed_summary(text: &str) -> Option<(u64, u64)> {
    let toks: Vec<&str> = text.split_whitespace().collect();
    let mut best = None;
    for i in 0..toks.len() {
        if toks[i] != "Embedded" {
            continue;
        }
        let (Some(n_c), Some(n_d)) = (toks.get(i + 1), toks.get(i + 4)) else {
            continue;
        };
        let (Ok(c), Ok(d)) = (n_c.parse::<u64>(), n_d.parse::<u64>()) else {
            continue;
        };
        if toks.get(i + 2) == Some(&"chunks")
            && toks.get(i + 3) == Some(&"from")
            && toks.get(i + 5).is_some_and(|t| t.starts_with("documents"))
        {
            best = Some((c, d));
        }
    }
    best
}

// ---------------------------------------------------------------------------
// reverse proxy
// ---------------------------------------------------------------------------

/// Connection-level headers that must not be forwarded either way.
fn is_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection" | "keep-alive" | "transfer-encoding" | "upgrade" | "te" | "trailer" | "host"
    )
}

pub fn router(ext: Arc<External>) -> Router {
    Router::new().fallback(proxy).with_state(ext)
}

async fn proxy(State(ext): State<Arc<External>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let pq = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let url = format!("{}{pq}", ext.upstream);
    let body = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response(),
    };
    let mut rb = ext.client.request(parts.method, &url).body(body);
    for (name, value) in &parts.headers {
        if !is_hop_header(name) {
            rb = rb.header(name, value);
        }
    }
    // reqwest sets Host from the upstream URL (loopback for a spawned qmd).
    let resp = match rb.send().await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("upstream: {e}")).into_response(),
    };
    let mut out = Response::builder().status(resp.status());
    for (name, value) in resp.headers() {
        if !is_hop_header(name) {
            out = out.header(name, value);
        }
    }
    // Streamed through, so SSE responses reach the client as they're produced.
    out.body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|e| {
            (
                StatusCode::BAD_GATEWAY,
                [(header::CONTENT_TYPE, "text/plain")],
                format!("upstream: {e}"),
            )
                .into_response()
        })
}

// ---------------------------------------------------------------------------
// mcp supervision
// ---------------------------------------------------------------------------

fn mcp_args(host: &str, port: u16) -> Vec<String> {
    vec![
        "mcp".into(),
        "--http".into(),
        "--host".into(),
        host.into(),
        "--port".into(),
        port.to_string(),
    ]
}

fn spawn_mcp_child(bin: &str, host: &str, port: u16) -> io::Result<Child> {
    retry_busy(|| Command::new(bin).args(mcp_args(host, port)).spawn())
}

/// Supervise `qmd mcp`: spawn, probe readiness, and respawn it after `retry`
/// whenever it exits, until `stop` is set. If mademind itself dies, the
/// container runtime takes the child down with it.
pub fn run_supervisor(bin: &str, host: &str, port: u16, retry: Duration, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match spawn_mcp_child(bin, host, port) {
            Ok(mut child) => {
                metric_add("mademind_mcp_spawns_total", &[], 1.0);
                eprintln!(
                    "mademind: spawned qmd mcp (pid {}) on {host}:{port}",
                    child.id()
                );
                // Poll the child and probe readiness together, so both a server
                // that comes up and a process that dies at once are handled.
                let deadline = Instant::now() + Duration::from_secs(120);
                let mut ready = false;
                let mut warned = false;
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            metric_set("mademind_mcp_ready", &[], 0.0);
                            eprintln!("mademind: qmd mcp exited ({status}); restarting");
                            break;
                        }
                        Ok(None) => {
                            if stop.load(Ordering::Relaxed) {
                                let _ = child.kill();
                                let _ = child.wait();
                                return;
                            }
                            if !ready {
                                if std::net::TcpStream::connect((host, port)).is_ok() {
                                    ready = true;
                                    metric_set("mademind_mcp_ready", &[], 1.0);
                                    eprintln!("mademind: qmd mcp ready on {host}:{port}");
                                } else if !warned && Instant::now() > deadline {
                                    warned = true;
                                    eprintln!(
                                        "mademind: qmd mcp not ready on {host}:{port} after 120s"
                                    );
                                }
                            }
                            thread::sleep(Duration::from_millis(200));
                        }
                        Err(e) => {
                            eprintln!("mademind: qmd mcp wait error: {e}");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                metric_set("mademind_mcp_ready", &[], 0.0);
                eprintln!("mademind: qmd mcp spawn failed: {e}");
            }
        }
        thread::sleep(retry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use tower::ServiceExt;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mademind-ext-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn script(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn wait_for_file(p: &Path) -> String {
        for _ in 0..100 {
            if let Ok(s) = fs::read_to_string(p) {
                if !s.is_empty() {
                    return s;
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("{p:?} never written");
    }

    fn ext_with(bin: &str, upstream: &str, timeout: Duration) -> External {
        let cfg = ExternalConfig {
            bin: bin.into(),
            upstream: upstream.into(),
            ..ExternalConfig::default()
        };
        External::new(&cfg, timeout).unwrap()
    }

    #[test]
    fn write_index_config_emits_collections() {
        let d = tmpdir("index");
        let mut cfg = Config::default();
        cfg.collections.insert(
            "notes".into(),
            crate::config::CollectionConfig {
                path: "/data/notes".into(),
                ..Default::default()
            },
        );
        write_index_config(&cfg, &d).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(d.join("index.yml")).unwrap()).unwrap();
        assert_eq!(v["collections"]["notes"]["path"], "/data/notes");
        assert_eq!(v["collections"]["notes"]["pattern"], "**/*.md");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn embed_args_minimal_and_full() {
        assert_eq!(embed_args(&EmbedConfig::default()), vec!["embed"]);
        let e = EmbedConfig {
            force: true,
            collections: vec!["a".into(), "b".into()],
            max_docs_per_batch: Some(42),
            max_batch_mb: Some(1024),
            timeout_minutes: Some(50),
            ..EmbedConfig::default()
        };
        assert_eq!(
            embed_args(&e),
            vec![
                "embed",
                "-f",
                "-c",
                "a",
                "-c",
                "b",
                "--max-docs-per-batch",
                "42",
                "--max-batch-mb",
                "1024",
                "--timeout",
                "50",
            ]
        );
    }

    #[test]
    fn parse_embed_summary_extracts_chunks_and_docs() {
        assert_eq!(
            parse_embed_summary("✓ Done! Embedded 5 chunks from 3 documents in 5s"),
            Some((5, 3))
        );
        let noisy =
            "\u{1b}[?25l████ 50%\r████████ 100%\u{1b}[K ✓ Done! Embedded 7 chunks from 2 documents";
        assert_eq!(parse_embed_summary(noisy), Some((7, 2)));
        assert_eq!(
            parse_embed_summary("✓ All content hashes already have embeddings."),
            None
        );
        assert_eq!(parse_embed_summary("Embedded nothing"), None);
    }

    #[tokio::test]
    async fn update_runs_command_and_reports_failure() {
        let d = tmpdir("update");
        let ok = d.join("ok.sh");
        script(
            &ok,
            &format!("echo \"$@\" > {}/args\necho 'Indexed: 3'", d.display()),
        );
        let s = ext_with(ok.to_str().unwrap(), "", Duration::from_secs(5))
            .update()
            .await
            .unwrap();
        assert_eq!(s, "Indexed: 3");
        assert_eq!(fs::read_to_string(d.join("args")).unwrap().trim(), "update");

        let bad = d.join("bad.sh");
        script(&bad, "echo broken; exit 3");
        let e = ext_with(bad.to_str().unwrap(), "", Duration::from_secs(5))
            .update()
            .await
            .unwrap_err();
        assert!(e.to_string().contains("broken"), "{e}");
        assert!(ext_with("/nonexistent-qmd-xyz", "", Duration::from_secs(5))
            .update()
            .await
            .is_err());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn spawn_mcp_child_uses_loopback_args() {
        let d = tmpdir("spawn");
        let marker = d.join("args.txt");
        let bin = d.join("fake-qmd.sh");
        script(
            &bin,
            &format!("echo \"$@\" > {}\nsleep 5", marker.display()),
        );
        let mut child = spawn_mcp_child(bin.to_str().unwrap(), "127.0.0.1", 1).unwrap();
        let args = wait_for_file(&marker);
        assert_eq!(args.trim(), "mcp --http --host 127.0.0.1 --port 1");
        child.kill().unwrap();
        child.wait().unwrap();
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn supervisor_restarts_exiting_child() {
        let d = tmpdir("supv");
        let count = d.join("runs");
        let bin = d.join("fake-qmd.sh");
        script(&bin, &format!("echo run >> {}", count.display())); // exits at once
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = Arc::clone(&stop);
        let bin_s = bin.to_str().unwrap().to_string();
        let h = thread::spawn(move || {
            run_supervisor(&bin_s, "127.0.0.1", 1, Duration::from_millis(100), stop_t)
        });
        thread::sleep(Duration::from_millis(1500));
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        let runs = fs::read_to_string(&count).unwrap().lines().count();
        assert!(runs >= 3, "expected >=3 restarts, got {runs}");
        let _ = fs::remove_dir_all(&d);
    }

    // -- proxy (mock upstream on an ephemeral port) ----------------------------

    async fn mock_upstream() -> String {
        use axum::routing::get;
        let app = Router::new()
            .route(
                "/echo",
                get(|| async { ([("x-upstream", "yes")], "hello-echo") }),
            )
            .route(
                "/sse",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        "event: message\ndata: {\"ok\":true}\n\n",
                    )
                }),
            )
            .route(
                "/err",
                get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
            )
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    "late"
                }),
            )
            .fallback(|req: Request| async move {
                let host = req.headers()[header::HOST].to_str().unwrap().to_string();
                let custom = req
                    .headers()
                    .get("x-custom")
                    .map(|v| v.to_str().unwrap().to_string())
                    .unwrap_or_default();
                let method = req.method().to_string();
                let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                    .await
                    .unwrap();
                format!(
                    "{method}|{host}|{custom}|{}",
                    String::from_utf8_lossy(&body)
                )
            });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn call(
        app: Router,
        req: HttpRequest<Body>,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let r = app.oneshot(req).await.unwrap();
        let (status, headers) = (r.status(), r.headers().clone());
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (status, headers, String::from_utf8_lossy(&b).into_owned())
    }

    #[tokio::test]
    async fn proxy_passes_method_body_headers_and_status() {
        let up = mock_upstream().await;
        let app = router(Arc::new(ext_with("", &up, Duration::from_secs(5))));

        let (s, h, b) = call(
            app.clone(),
            HttpRequest::get("/echo").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(s, 200);
        assert_eq!(h["x-upstream"], "yes");
        assert_eq!(b, "hello-echo");

        let (s, _, b) = call(
            app.clone(),
            HttpRequest::post("/mcp")
                .header("x-custom", "keep-me")
                .header(header::HOST, "notes.example:8888")
                .body(Body::from("{\"id\":1}"))
                .unwrap(),
        )
        .await;
        assert_eq!(s, 200);
        let host = up.trim_start_matches("http://");
        assert_eq!(b, format!("POST|{host}|keep-me|{{\"id\":1}}"));

        let (_, h, b) = call(
            app.clone(),
            HttpRequest::get("/sse").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(h[header::CONTENT_TYPE], "text/event-stream");
        assert!(b.starts_with("event: message"));

        let (s, _, b) = call(app, HttpRequest::get("/err").body(Body::empty()).unwrap()).await;
        assert_eq!((s.as_u16(), b.as_str()), (500, "boom"));
    }

    #[tokio::test]
    async fn proxy_dead_upstream_and_timeout_are_502() {
        let app = router(Arc::new(ext_with(
            "",
            "http://127.0.0.1:1",
            Duration::from_secs(5),
        )));
        let (s, _, b) = call(app, HttpRequest::get("/x").body(Body::empty()).unwrap()).await;
        assert_eq!(s, 502);
        assert!(b.starts_with("upstream:"), "{b}");

        // The configured timeout (1s) wins, not reqwest's 30s socket default.
        let up = mock_upstream().await;
        let app = router(Arc::new(ext_with("", &up, Duration::from_secs(1))));
        let t0 = Instant::now();
        let (s, _, _) = call(app, HttpRequest::get("/slow").body(Body::empty()).unwrap()).await;
        assert_eq!(s, 502);
        assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
    }
}
