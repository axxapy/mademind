//! Prometheus text exposition at GET /metrics (open like /healthz: scrapers
//! can't sign requests, and it exposes only operational counters).
//!
//! A tiny hand-rolled exporter (no prometheus crate): counters and gauges live
//! in a global `name{labels} -> f64` map, mutated from every subsystem; render
//! goes through the family table below.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Metric families in render order: (name, type, help). A sample belongs to a
/// family by its base name (the part before `{`). Keys not in this table are
/// never rendered, so stale keys can't leak into the exposition.
const METRIC_FAMILIES: &[(&str, &str, &str)] = &[
    ("mademind_info", "gauge", "Build info; value is 1."),
    (
        "mademind_mcp_ready",
        "gauge",
        "external: 1 when the supervised qmd mcp child passed its TCP readiness probe.",
    ),
    (
        "mademind_mcp_spawns_total",
        "counter",
        "external: qmd mcp child spawns, including restarts.",
    ),
    (
        "mademind_watcher_events_total",
        "counter",
        "Matching fs events seen by the watcher.",
    ),
    (
        "mademind_update_cycles_total",
        "counter",
        "Index update cycles run by the watcher, by result.",
    ),
    (
        "mademind_update_last_duration_seconds",
        "gauge",
        "Wall time of the last index update.",
    ),
    (
        "mademind_update_last_success_timestamp_seconds",
        "gauge",
        "Unix time of the last successful index update; 0 = never.",
    ),
    (
        "mademind_embed_cycles_total",
        "counter",
        "Embed loop cycles, by result.",
    ),
    (
        "mademind_embed_last_duration_seconds",
        "gauge",
        "Wall time of the last embed cycle.",
    ),
    (
        "mademind_embed_last_success_timestamp_seconds",
        "gauge",
        "Unix time of the last successful embed cycle; 0 = never.",
    ),
    ("mademind_embed_chunks_total", "counter", "Chunks embedded."),
    (
        "mademind_embed_documents_total",
        "counter",
        "Documents embedded.",
    ),
    (
        "mademind_auth_rejections_total",
        "counter",
        "Requests rejected by the auth layer, by HTTP code.",
    ),
    (
        "mademind_http_requests_total",
        "counter",
        "HTTP requests served, by normalized path and response code.",
    ),
];

fn metrics_map() -> &'static Mutex<HashMap<String, f64>> {
    static M: OnceLock<Mutex<HashMap<String, f64>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Canonical sample key: labels sorted by name (render groups by base name).
fn metric_key(name: &str, labels: &[(&str, &str)]) -> String {
    if labels.is_empty() {
        return name.to_string();
    }
    let mut ls: Vec<(&str, &str)> = labels.to_vec();
    ls.sort();
    let inner = ls
        .iter()
        .map(|(k, v)| format!("{k}=\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!("{name}{{{inner}}}")
}

pub fn metric_add(name: &str, labels: &[(&str, &str)], delta: f64) {
    let mut m = metrics_map().lock().unwrap();
    *m.entry(metric_key(name, labels)).or_insert(0.0) += delta;
}

pub fn metric_set(name: &str, labels: &[(&str, &str)], value: f64) {
    metrics_map()
        .lock()
        .unwrap()
        .insert(metric_key(name, labels), value);
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn metrics_render() -> String {
    let m = metrics_map().lock().unwrap();
    let mut out = String::new();
    for (name, ty, help) in METRIC_FAMILIES {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {ty}\n"));
        let prefix = format!("{name}{{");
        let mut keys: Vec<&String> = m
            .keys()
            .filter(|k| *k == name || k.starts_with(&prefix))
            .collect();
        keys.sort();
        for k in keys {
            out.push_str(&format!("{k} {}\n", m[k]));
        }
    }
    out
}

/// Cardinality guard for the http_requests path label: cap length and
/// collapse anything but plain URL path characters into "<other>".
pub fn normalize_path_label(path: &str) -> String {
    if path.len() <= 64
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'.' | b'-' | b'~'))
    {
        path.to_string()
    } else {
        "<other>".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_key_sorts_labels() {
        assert_eq!(metric_key("mademind_x", &[]), "mademind_x");
        assert_eq!(
            metric_key("mademind_x", &[("path", "/a"), ("code", "200")]),
            "mademind_x{code=\"200\",path=\"/a\"}"
        );
    }

    #[test]
    fn metrics_add_and_render_family() {
        // Unique label values to stay isolated from other tests (global map).
        let labels = [("path", "/t-metrics"), ("code", "201")];
        metric_add("mademind_http_requests_total", &labels, 1.0);
        metric_add("mademind_http_requests_total", &labels, 2.0);
        let out = metrics_render();
        let line = out
            .lines()
            .find(|l| {
                l.starts_with("mademind_http_requests_total{code=\"201\",path=\"/t-metrics\"}")
            })
            .expect("sample must render");
        assert_eq!(
            line,
            "mademind_http_requests_total{code=\"201\",path=\"/t-metrics\"} 3"
        );
        assert!(out.contains("# TYPE mademind_http_requests_total counter"));
    }

    #[test]
    fn normalize_path_label_caps_and_collapses() {
        assert_eq!(normalize_path_label("/mcp"), "/mcp");
        assert_eq!(normalize_path_label("/file"), "/file");
        let long = format!("/{}", "a".repeat(100));
        assert_eq!(normalize_path_label(&long), "<other>");
        assert_eq!(normalize_path_label("/a b?x"), "<other>");
    }
}
