//! Client side of the CLI: which server to talk to, request signing, and the
//! two ways in (REST `/query`, `/file`; MCP tools over Streamable HTTP).
//!
//! Settings: `$MADEMIND_CLIENT_CONFIG`, else `~/.config/mademind/client.json`
//! (the same file the TypeScript clients read):
//!
//! ```json
//! { "url": "http://warp.home:8888", "client_id": "laptop",
//!   "key_file": "~/.config/mademind/laptop.key",
//!   "local_roots": { "notes": "~/notes" } }
//! ```
//!
//! Env overrides: MADEMIND_URL, MADEMIND_CLIENT_ID, MADEMIND_SIGN_KEY (hex),
//! MADEMIND_SIGN_KEY_FILE, MADEMIND_LOCAL_ROOTS ("notes=~/notes,work=/srv").
//! Requests are signed only when a key exists; sources the server leaves open
//! (its own machine, by default) need none.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use ed25519_dalek::{Signer, SigningKey};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{signing_message, HDR_CLIENT, HDR_SIGNATURE, HDR_TIMESTAMP};
use crate::files::FILE_HEADER;

const DEFAULT_URL: &str = "http://127.0.0.1:8888";

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ClientFile {
    url: Option<String>,
    client_id: Option<String>,
    key_file: Option<String>,
    local_roots: BTreeMap<String, String>,
}

pub struct ClientConfig {
    pub url: String,
    pub client_id: String,
    pub key_file: PathBuf,
    local_roots: BTreeMap<String, PathBuf>,
}

fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if p == "~" => home(),
        None => PathBuf::from(p),
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn short_hostname() -> String {
    let h = std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let h = h.split('.').next().unwrap_or("").to_string();
    if h.is_empty() {
        "client".into()
    } else {
        h
    }
}

impl ClientConfig {
    pub fn path() -> PathBuf {
        env("MADEMIND_CLIENT_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config/mademind/client.json"))
    }

    pub fn load() -> anyhow::Result<ClientConfig> {
        let path = Self::path();
        let file: ClientFile = match std::fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("{}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ClientFile::default(),
            Err(e) => return Err(e).with_context(|| format!("{}", path.display())),
        };
        let url = env("MADEMIND_URL")
            .or(file.url)
            .unwrap_or_else(|| DEFAULT_URL.into())
            .trim_end_matches('/')
            .to_string();
        let client_id = env("MADEMIND_CLIENT_ID")
            .or(file.client_id)
            .unwrap_or_else(short_hostname);
        let key_file = env("MADEMIND_SIGN_KEY_FILE")
            .or(file.key_file)
            .map(|f| expand(&f))
            .unwrap_or_else(|| default_key_file(&client_id));
        let mut local_roots: BTreeMap<String, PathBuf> = file
            .local_roots
            .iter()
            .map(|(k, v)| (k.clone(), expand(v)))
            .collect();
        for pair in env("MADEMIND_LOCAL_ROOTS").unwrap_or_default().split(',') {
            if let Some((k, v)) = pair.split_once('=') {
                local_roots.insert(k.trim().into(), expand(v.trim()));
            }
        }
        Ok(ClientConfig {
            url,
            client_id,
            key_file,
            local_roots,
        })
    }

    /// The signing key, or None for unsigned requests.
    fn signing_key(&self) -> anyhow::Result<Option<SigningKey>> {
        let hex = match env("MADEMIND_SIGN_KEY") {
            Some(h) => h,
            None => match std::fs::read_to_string(&self.key_file) {
                Ok(h) => h,
                Err(_) => return Ok(None),
            },
        };
        let hex: String = hex.split_whitespace().collect::<String>().to_lowercase();
        let bytes = decode_hex(&hex)
            .filter(|b| b.len() == 64)
            .ok_or_else(|| anyhow!("signing key: expected 64 bytes of hex"))?;
        let seed: [u8; 32] = bytes[..32].try_into().unwrap();
        Ok(Some(SigningKey::from_bytes(&seed)))
    }

    /// Local path of `<collection>/<rel>` on this machine, if the file is
    /// there. Hit paths can be normalised by the engine (`a_b.md` shows as
    /// `a-b.md`); those are matched back to the real name.
    pub fn local_path(&self, collection_path: &str) -> Option<PathBuf> {
        let (coll, rel) = collection_path
            .split_once('/')
            .unwrap_or((collection_path, ""));
        let root = self.local_roots.get(coll)?;
        if rel.is_empty() {
            return root.exists().then(|| root.clone());
        }
        let p = root.join(rel);
        if p.exists() {
            return Some(p);
        }
        #[cfg(feature = "builtin-engine")]
        return crate::files::find_by_handle(root, rel);
        #[cfg(not(feature = "builtin-engine"))]
        None
    }
}

pub fn default_key_file(id: &str) -> PathBuf {
    home().join(format!(".config/mademind/{id}.key"))
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn encode_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Write a new key (seed + public key, hex) to `path`, owner-only. Returns
/// the 32-byte public key.
pub fn generate_key(path: &Path) -> anyhow::Result<[u8; 32]> {
    if path.exists() {
        bail!(
            "{} exists; remove it first to replace the key",
            path.display()
        );
    }
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow!("random: {e}"))?;
    let key = SigningKey::from_bytes(&seed);
    let public = key.verifying_key().to_bytes();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = format!("{}{}\n", encode_hex(&seed), encode_hex(&public));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    std::io::Write::write_all(&mut opts.open(path)?, text.as_bytes())?;
    Ok(public)
}

/// The three auth headers for one request.
fn sign(
    key: &SigningKey,
    client: &str,
    ts: i64,
    method: &str,
    wire: &str,
    body: &[u8],
) -> [(&'static str, String); 3] {
    let msg = signing_message(client, ts, method, wire, body);
    let sig = key.sign(msg.as_bytes());
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, sig.to_bytes());
    [
        (HDR_CLIENT, client.to_string()),
        (HDR_TIMESTAMP, ts.to_string()),
        (HDR_SIGNATURE, b64),
    ]
}

/// A response: status, body, and the `/file` real-name header if present.
pub struct Reply {
    pub status: u16,
    pub body: String,
    pub file: Option<String>,
}

pub struct Client {
    pub cfg: ClientConfig,
    key: Option<SigningKey>,
    http: reqwest::Client,
    rt: tokio::runtime::Runtime,
}

impl Client {
    pub fn new(cfg: ClientConfig) -> anyhow::Result<Client> {
        let key = cfg.signing_key()?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(Client {
            cfg,
            key,
            http: reqwest::Client::new(),
            rt,
        })
    }

    /// One request to `target` (path + query) on the configured server.
    pub fn request(
        &self,
        method: &str,
        target: &str,
        body: &str,
        extra: &[(&str, &str)],
    ) -> anyhow::Result<(Reply, reqwest::header::HeaderMap)> {
        let target = if target.starts_with('/') {
            target.to_string()
        } else {
            format!("/{target}")
        };
        let url = format!("{}{target}", self.cfg.url);
        let method = method.to_uppercase();
        let m = reqwest::Method::from_bytes(method.as_bytes())?;
        let mut req = self.http.request(m, &url);
        if let Some(key) = &self.key {
            // Signed over the target as it goes on the wire (reqwest keeps
            // an already-encoded path and query as they are).
            let parsed = reqwest::Url::parse(&url)?;
            let wire = match parsed.query() {
                Some(q) => format!("{}?{q}", parsed.path()),
                None => parsed.path().to_string(),
            };
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64;
            for (k, v) in sign(
                key,
                &self.cfg.client_id,
                ts,
                &method,
                &wire,
                body.as_bytes(),
            ) {
                req = req.header(k, v);
            }
        }
        if !body.is_empty() {
            req = req
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        let resp = self
            .rt
            .block_on(req.send())
            .with_context(|| format!("cannot reach {}", self.cfg.url))?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let file = headers
            .get(FILE_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(percent_decode);
        let body = self.rt.block_on(resp.text())?;
        Ok((Reply { status, body, file }, headers))
    }

    /// REST `/query`: the hits array.
    pub fn query(&self, params: &Value) -> anyhow::Result<Vec<Value>> {
        let (r, _) = self.request("POST", "/query", &params.to_string(), &[])?;
        let v: Value = serde_json::from_str(&r.body).unwrap_or(Value::Null);
        if r.status != 200 {
            bail!("{}", error_text(r.status, &r.body, &v));
        }
        Ok(v["results"].as_array().cloned().unwrap_or_default())
    }

    /// Call one MCP tool in a short-lived session; returns the tool result.
    pub fn call_tool(&self, name: &str, args: Value) -> anyhow::Result<Value> {
        let accept = ("accept", "application/json, text/event-stream");
        let init = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": "2025-03-26", "capabilities": {},
            "clientInfo": {"name": "mademind-cli", "version": env!("CARGO_PKG_VERSION")}}});
        let (r, headers) = self.request("POST", "/mcp", &init.to_string(), &[accept])?;
        if r.status != 200 {
            bail!("{}", error_text(r.status, &r.body, &Value::Null));
        }
        let sid = headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow!("server sent no MCP session"))?
            .to_string();
        let session = ("mcp-session-id", sid.as_str());
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        self.request("POST", "/mcp", &note.to_string(), &[accept, session])?;
        let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": args}});
        let (r, _) = self.request("POST", "/mcp", &call.to_string(), &[accept, session])?;
        let _ = self.request("DELETE", "/mcp", "", &[session]);
        let msg = rpc_message(&r.body).ok_or_else(|| anyhow!("no MCP answer: {}", r.body))?;
        if let Some(e) = msg.get("error") {
            bail!("{}", e["message"].as_str().unwrap_or("MCP error"));
        }
        Ok(msg["result"].clone())
    }
}

/// The JSON-RPC message in a response body: plain JSON or an SSE stream.
fn rpc_message(body: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        return Some(v);
    }
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
        .find(|v| v.get("result").is_some() || v.get("error").is_some())
}

fn error_text(status: u16, body: &str, v: &Value) -> String {
    let detail = v["error"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| body.trim().chars().take(200).collect());
    match status {
        401 => format!("401 unauthorized: {detail} (sign requests: `mademind genkey`, then add the key on the server)"),
        403 => format!("403 forbidden: {detail}"),
        _ => format!("HTTP {status}: {detail}"),
    }
}

/// Undo `%XX` escapes (the `/file` header and `qmd://` URIs use them).
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_message_reads_json_and_sse() {
        let j = r#"{"jsonrpc":"2.0","id":1,"result":{"x":1}}"#;
        assert_eq!(rpc_message(j).unwrap()["result"]["x"], 1);
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"x\":2}}\n\n";
        assert_eq!(rpc_message(sse).unwrap()["result"]["x"], 2);
    }

    #[test]
    fn percent_decode_handles_escapes_and_stray_percent() {
        assert_eq!(percent_decode("notes/a%20b.md"), "notes/a b.md");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn generated_key_passes_the_servers_auth_check() {
        let d = std::env::temp_dir().join(format!("mademind-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let path = d.join("k.key");
        let public = generate_key(&path).unwrap();
        assert!(generate_key(&path).is_err(), "never overwrites a key");
        let cfg = ClientConfig {
            url: DEFAULT_URL.into(),
            client_id: "laptop".into(),
            key_file: path.clone(),
            local_roots: BTreeMap::new(),
        };
        let key = cfg.signing_key().unwrap().unwrap();
        // What the server would load from `[auth] clients`.
        let line = crate::auth::public_key_line(&public);
        let vk = ed25519_dalek::VerifyingKey::from_bytes(&public).unwrap();
        assert!(line.starts_with("ssh-ed25519 "));
        let state = crate::auth::tests::auth_state_for(vk, "laptop", 300);
        let mut headers = axum::http::HeaderMap::new();
        let body = br#"{"searches":[]}"#;
        for (k, v) in sign(&key, "laptop", 1000, "POST", "/query?x=1", body) {
            headers.insert(k, v.parse().unwrap());
        }
        let check = |url: &str| {
            crate::auth::check(Some(&state), "10.0.0.5", "POST", url, &headers, body, 1000)
        };
        assert_eq!(check("/query?x=2"), Err((401, "bad signature")));
        assert_eq!(check("/query?x=1"), Ok(()));
        let _ = std::fs::remove_dir_all(&d);
    }
}
