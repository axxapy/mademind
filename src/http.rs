//! The one exposed HTTP server.
//!
//!   GET /healthz          -> ok                          (open)
//!   GET /metrics          -> Prometheus text format      (open: scrapers can't sign)
//!   GET /file?path=...    -> a note's bytes (absolute path or search-hit path)
//!   everything else       -> the engine (/mcp, /query, /search, /health)
//!
//! Every request except the two open ones passes the auth layer first.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::auth::{self, AuthState};
use crate::files::{handle_file, Collections};
use crate::metrics::{metric_add, metrics_render, normalize_path_label, unix_now};

/// Largest request body the auth layer buffers to check its signature.
const MAX_SIGNED_BODY: usize = 16 * 1024 * 1024;

pub struct AppState {
    pub collections: Collections,
    pub auth: Option<Arc<AuthState>>,
}

pub fn router(state: Arc<AppState>, engine: Router) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
        .route("/file", get(file))
        .with_state(Arc::clone(&state))
        .fallback_service(engine)
        .layer(middleware::from_fn_with_state(state, auth_layer))
        .layer(middleware::from_fn(count_requests))
}

async fn metrics() -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        metrics_render(),
    )
        .into_response()
}

async fn file(State(st): State<Arc<AppState>>, req: Request) -> Response {
    handle_file(req.uri().query(), &st.collections)
}

async fn auth_layer(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let Some(auth) = st.auth.as_deref() else {
        return next.run(req).await;
    };
    if path == "/healthz" || path == "/metrics" {
        return next.run(req).await;
    }
    // The signature covers the body: buffer it, check, then pass it on.
    let (parts, body) = req.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_SIGNED_BODY).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response();
    };
    let url = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    // IPv4 clients of a dual-stack socket arrive as ::ffff:a.b.c.d.
    let src = peer.ip().to_canonical().to_string();
    if let Err((code, msg)) = auth::check(
        Some(auth),
        &src,
        parts.method.as_str(),
        url,
        &parts.headers,
        &bytes,
        unix_now(),
    ) {
        metric_add(
            "mademind_auth_rejections_total",
            &[("code", code.to_string().as_str())],
            1.0,
        );
        eprintln!(
            "mademind: auth reject {} {url} from {src}: {msg}",
            parts.method
        );
        let status = StatusCode::from_u16(code).unwrap_or(StatusCode::UNAUTHORIZED);
        return (status, msg).into_response();
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

async fn count_requests(req: Request, next: Next) -> Response {
    let path = normalize_path_label(req.uri().path());
    let resp = next.run(req).await;
    metric_add(
        "mademind_http_requests_total",
        &[("path", path.as_str()), ("code", resp.status().as_str())],
        1.0,
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::tests::{auth_state_for, signed_headers, test_keypair};
    use crate::auth::{HDR_CLIENT, HDR_SIGNATURE, HDR_TIMESTAMP};
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::Request as HttpRequest;
    use axum::routing::any;
    use std::fs;
    use std::path::PathBuf;
    use tower::ServiceExt;

    /// The app with an echo "engine" that reports what reached it.
    fn app(roots: Vec<PathBuf>, auth: Option<Arc<AuthState>>) -> Router {
        let collections = roots
            .into_iter()
            .enumerate()
            .map(|(i, p)| (format!("c{i}"), p))
            .collect();
        let engine = Router::new().fallback(any(|req: Request| async move {
            let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                .await
                .unwrap();
            format!("engine:{}", String::from_utf8_lossy(&body))
        }));
        router(Arc::new(AppState { collections, auth }), engine)
            .layer(MockConnectInfo(SocketAddr::from(([192, 168, 1, 9], 5555))))
    }

    async fn call(app: Router, req: HttpRequest<Body>) -> (u16, String) {
        let r = app.oneshot(req).await.unwrap();
        let s = r.status().as_u16();
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (s, String::from_utf8_lossy(&b).into_owned())
    }

    fn get(uri: &str) -> HttpRequest<Body> {
        HttpRequest::get(uri).body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn healthz_metrics_and_engine_fallback() {
        let a = app(vec![], None);
        assert_eq!(call(a.clone(), get("/healthz")).await, (200, "ok".into()));
        let (s, b) = call(a.clone(), get("/metrics")).await;
        assert_eq!(s, 200);
        assert!(b.contains("# TYPE mademind_http_requests_total counter"));
        // the earlier /healthz call was counted
        assert!(b.contains("mademind_http_requests_total{code=\"200\",path=\"/healthz\"}"));
        let r = HttpRequest::post("/mcp").body(Body::from("hi")).unwrap();
        assert_eq!(call(a, r).await, (200, "engine:hi".into()));
    }

    #[tokio::test]
    async fn file_endpoint() {
        let d = std::env::temp_dir().join(format!("mademind-http-file-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("a.md"), "alpha").unwrap();
        let a = app(vec![d.clone()], None);
        let dp = d.to_string_lossy();
        assert_eq!(
            call(a.clone(), get(&format!("/file?path={dp}/a.md"))).await,
            (200, "alpha".into())
        );
        assert_eq!(
            call(a.clone(), get(&format!("/file?path={dp}/../etc/passwd")))
                .await
                .0,
            403
        );
        assert_eq!(call(a.clone(), get("/file?path=/etc/passwd")).await.0, 403);
        assert_eq!(
            call(a.clone(), get(&format!("/file?path={dp}/nope.md")))
                .await
                .0,
            404
        );
        assert_eq!(call(a, get("/file")).await.0, 400);
        let _ = fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn auth_enforces_and_passes_body_through() {
        let (sk, vk, _) = test_keypair("laptop");
        let a = app(vec![], Some(Arc::new(auth_state_for(vk, "laptop", 300))));

        // open endpoints
        assert_eq!(call(a.clone(), get("/healthz")).await.0, 200);
        assert_eq!(call(a.clone(), get("/metrics")).await.0, 200);
        // unsigned / badly signed -> 401
        assert_eq!(call(a.clone(), get("/query")).await.0, 401);
        let bad = HttpRequest::get("/query")
            .header(HDR_CLIENT, "laptop")
            .header(HDR_TIMESTAMP, unix_now().to_string())
            .header(HDR_SIGNATURE, "AAAA")
            .body(Body::empty())
            .unwrap();
        assert_eq!(call(a.clone(), bad).await.0, 401);

        // signed POST with a query string: reaches the engine with its body
        let body = b"{\"searches\":[]}";
        let (c, t, s) = signed_headers(&sk, "laptop", unix_now(), "POST", "/query?x=1", body);
        let ok = HttpRequest::post("/query?x=1")
            .header(HDR_CLIENT, c)
            .header(HDR_TIMESTAMP, t)
            .header(HDR_SIGNATURE, s)
            .body(Body::from(&body[..]))
            .unwrap();
        assert_eq!(call(a, ok).await, (200, "engine:{\"searches\":[]}".into()));
    }
}
