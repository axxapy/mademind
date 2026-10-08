//! GET /file?path=... — serve a note to clients without the notes mounted.
//! `path` is either absolute (inside a collection's root) or a search hit's
//! own path: `<collection>/<rel>` or `qmd://<collection>/<rel>`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Percent-decode (and '+' -> space, query-string style).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let h = (b[i + 1] as char).to_digit(16);
                let l = (b[i + 2] as char).to_digit(16);
                match (h, l) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Collection name -> root directory.
pub type Collections = BTreeMap<String, PathBuf>;

/// Resolve `raw` to a path strictly inside one of the collection roots.
/// Returns None for anything outside, including `..` traversal (also after
/// percent-decoding, so `%2e%2e` does not bypass the check).
fn resolve_data_path(raw: &str, collections: &Collections) -> Option<PathBuf> {
    let p = percent_decode(raw);
    if p.contains("..") {
        return None;
    }
    if let Some(abs) = p.strip_prefix('/') {
        // Absolute: must be a root itself or below one.
        return collections.values().find_map(|root| {
            let lossy = root.to_string_lossy();
            let rp = lossy.trim_start_matches('/');
            if abs == rp {
                Some(root.clone())
            } else {
                abs.strip_prefix(format!("{rp}/").as_str())
                    .filter(|rest| !rest.is_empty())
                    .map(|rest| root.join(rest))
            }
        });
    }
    // A search hit: <collection>/<rel>, optionally qmd://-prefixed.
    let hit = p.strip_prefix("qmd://").unwrap_or(&p);
    let (name, rel) = hit.split_once('/')?;
    if rel.is_empty() || rel.starts_with('/') {
        return None;
    }
    let root = collections.get(name)?;
    let direct = root.join(rel);
    if direct.exists() {
        return Some(direct);
    }
    // The engine normalises the paths it returns (e.g. `AGENT_MEMORY.md` ->
    // `AGENT-MEMORY.md`); map such a hit back to the file on disk.
    #[cfg(feature = "builtin-engine")]
    if let Some(found) = find_by_handle(root, rel) {
        return Some(found);
    }
    Some(direct)
}

/// Walk `root` along `rel`, at each level picking the entry whose rqmd
/// "handle" (its normalised form inside a path) equals that segment.
/// Directories and the final file normalise differently (the file keeps its
/// extension), so each is measured in its position in a path.
#[cfg(feature = "builtin-engine")]
pub(crate) fn find_by_handle(root: &Path, rel: &str) -> Option<PathBuf> {
    use rqmd_core::store::docid::handelize;
    let segments: Vec<&str> = rel.split('/').collect();
    let mut dir = root.to_path_buf();
    for (i, want) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        let entry = fs::read_dir(&dir).ok()?.flatten().find(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == *want {
                return true;
            }
            let handle = if last {
                handelize(&name).ok()
            } else {
                // as a directory: the first component of "<name>/x"
                handelize(&format!("{name}/x"))
                    .ok()
                    .and_then(|h| h.split('/').next().map(str::to_string))
            };
            handle.as_deref() == Some(*want)
        })?;
        dir = entry.path();
    }
    Some(dir)
}

/// Serve a validated file: 404 / 413 / 200 + content-type.
/// Header naming the served file's real path inside its collection
/// (`<collection>/<rel>`, percent-encoded): hit paths are normalised, and
/// clients need the real name to find or edit the file.
pub const FILE_HEADER: &str = "x-mademind-file";

/// `<collection>/<rel>` of a resolved file (or just `<collection>` when the
/// collection root is the file itself).
fn collection_path(target: &Path, collections: &Collections) -> Option<String> {
    collections.iter().find_map(|(name, root)| {
        let rel = target.strip_prefix(root).ok()?;
        let rel = rel.to_string_lossy();
        Some(if rel.is_empty() {
            name.clone()
        } else {
            format!("{name}/{rel}")
        })
    })
}

/// Percent-encode everything but unreserved characters and `/`, so any file
/// name fits in a header value.
fn pct_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn serve_file_bytes(target: &Path) -> Response {
    let Ok(meta) = fs::metadata(target) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    if !meta.is_file() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    if meta.len() > MAX_FILE_BYTES {
        return (StatusCode::PAYLOAD_TOO_LARGE, "too large").into_response();
    }
    match fs::read(target) {
        Ok(bytes) => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], bytes).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// `query` is the raw query string; only a leading `path=` is accepted.
pub fn handle_file(query: Option<&str>, collections: &Collections) -> Response {
    let Some(q) = query.and_then(|q| q.strip_prefix("path=")) else {
        return (StatusCode::BAD_REQUEST, "missing path").into_response();
    };
    let Some(target) = resolve_data_path(q, collections) else {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    };
    let mut resp = serve_file_bytes(&target);
    if resp.status() == StatusCode::OK {
        if let Some(v) = collection_path(&target, collections)
            .and_then(|p| header::HeaderValue::from_str(&pct_encode(&p)).ok())
        {
            resp.headers_mut().insert(FILE_HEADER, v);
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mademind-files-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn roots() -> Collections {
        Collections::from([
            ("notes".into(), PathBuf::from("/data/notes")),
            ("soul".into(), PathBuf::from("/data/agent/SOUL.md")),
        ])
    }

    #[test]
    fn decode_plain_percent_plus_and_invalid() {
        assert_eq!(percent_decode("abc/def.md"), "abc/def.md");
        assert_eq!(percent_decode("%41%2Fb+c"), "A/b c");
        assert_eq!(percent_decode("%D0%BF"), "п");
        assert_eq!(percent_decode("100%z"), "100%z");
        assert_eq!(percent_decode("trailing%"), "trailing%");
        assert_eq!(percent_decode("a%4"), "a%4");
    }

    #[test]
    fn resolve_inside_roots() {
        assert_eq!(
            resolve_data_path("/data/notes/ai/rules.md", &roots()),
            Some(PathBuf::from("/data/notes/ai/rules.md"))
        );
        assert_eq!(
            resolve_data_path("/data/agent/SOUL.md", &roots()),
            Some(PathBuf::from("/data/agent/SOUL.md"))
        );
        assert_eq!(
            resolve_data_path("/data/notes/my%20note.md", &roots()),
            Some(PathBuf::from("/data/notes/my note.md"))
        );
    }

    #[test]
    fn resolve_search_hit_paths() {
        let want = Some(PathBuf::from("/data/notes/ai/rules.md"));
        assert_eq!(resolve_data_path("notes/ai/rules.md", &roots()), want);
        assert_eq!(resolve_data_path("qmd://notes/ai/rules.md", &roots()), want);
        assert_eq!(
            resolve_data_path("qmd%3A%2F%2Fnotes%2Fai%2Frules.md", &roots()),
            want
        );
        assert_eq!(resolve_data_path("other/a.md", &roots()), None);
        assert_eq!(resolve_data_path("notes", &roots()), None);
        assert_eq!(resolve_data_path("notes/", &roots()), None);
        assert_eq!(resolve_data_path("notes//etc/passwd", &roots()), None);
        assert_eq!(resolve_data_path("notes/../x.md", &roots()), None);
    }

    #[cfg(feature = "builtin-engine")]
    #[test]
    fn resolve_maps_normalised_hit_paths_to_disk_names() {
        let d = tmpdir("handle");
        fs::create_dir_all(d.join("My Specs")).unwrap();
        fs::write(d.join("My Specs/AGENT_MEMORY QMD.md"), "spec").unwrap();
        let rts = Collections::from([("notes".into(), d.clone())]);
        let want = d.join("My Specs/AGENT_MEMORY QMD.md");
        // literal path still works
        assert_eq!(
            resolve_data_path("notes/My Specs/AGENT_MEMORY QMD.md", &rts),
            Some(want.clone())
        );
        // the engine's normalised form of it resolves to the same file
        let handle = rqmd_core::store::docid::handelize("My Specs/AGENT_MEMORY QMD.md").unwrap();
        assert_ne!(handle, "My Specs/AGENT_MEMORY QMD.md");
        assert_eq!(
            resolve_data_path(&format!("notes/{handle}"), &rts),
            Some(want)
        );
        assert_eq!(
            handle_file(Some(&format!("path=notes/{handle}")), &rts).status(),
            200
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn resolve_rejects_outside_and_traversal() {
        assert_eq!(resolve_data_path("/etc/passwd", &roots()), None);
        assert_eq!(resolve_data_path("/data/notesX/foo.md", &roots()), None);
        assert_eq!(resolve_data_path("/data/notes/../.ssh/id", &roots()), None);
        assert_eq!(
            resolve_data_path("/data/notes/%2e%2e/.ssh/id", &roots()),
            None
        );
        assert_eq!(resolve_data_path("", &roots()), None);
        assert_eq!(resolve_data_path("/", &roots()), None);
    }

    #[test]
    fn serve_file_statuses_and_content_type() {
        let d = tmpdir("serve");
        let f = d.join("note.md");
        fs::write(&f, "hello note").unwrap();
        let r = serve_file_bytes(&f);
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(serve_file_bytes(&d.join("nope.md")).status(), 404);
        assert_eq!(serve_file_bytes(&d).status(), 404); // a dir, not a file
        let big = d.join("big.md");
        fs::File::create(&big)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap(); // sparse
        assert_eq!(serve_file_bytes(&big).status(), 413);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn handle_file_reports_real_collection_path() {
        let d = tmpdir("hdr");
        fs::create_dir_all(d.join("Заметки")).unwrap();
        fs::write(d.join("Заметки/AGENT_MEMORY.md"), "x").unwrap();
        let rts = Collections::from([("notes".into(), d.clone())]);
        let dp = d.to_string_lossy();
        for q in [
            format!("path={dp}/Заметки/AGENT_MEMORY.md"),
            "path=notes/Заметки/AGENT_MEMORY.md".into(),
        ] {
            let r = handle_file(Some(&q), &rts);
            assert_eq!(r.status(), 200);
            assert_eq!(
                r.headers()[FILE_HEADER],
                "notes/%D0%97%D0%B0%D0%BC%D0%B5%D1%82%D0%BA%D0%B8/AGENT_MEMORY.md"
            );
        }
        assert!(handle_file(Some("path=notes/missing.md"), &rts)
            .headers()
            .get(FILE_HEADER)
            .is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn handle_file_routing() {
        let d = tmpdir("route");
        fs::write(d.join("x.md"), "content-here").unwrap();
        let file_root = d.join("SOUL.md");
        fs::write(&file_root, "soul").unwrap();
        let rts = Collections::from([("d".into(), d.clone()), ("soul".into(), file_root.clone())]);
        let dp = d.to_string_lossy().to_string();
        let q = |s: String| handle_file(Some(&s), &rts).status();
        assert_eq!(q(format!("path={dp}/x.md")), 200);
        assert_eq!(q("path=d/x.md".into()), 200);
        assert_eq!(q("path=qmd://d/x.md".into()), 200);
        assert_eq!(q(format!("path={}", file_root.to_string_lossy())), 200);
        assert_eq!(q(format!("path={dp}/../outside.md")), 403);
        assert_eq!(handle_file(Some("nope=1"), &rts).status(), 400);
        assert_eq!(handle_file(None, &rts).status(), 400);
        let _ = fs::remove_dir_all(&d);
    }
}
