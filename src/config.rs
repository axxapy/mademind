//! Configuration: one TOML file ($MADEMIND_CONFIG, default /config/config.toml)
//! plus env overrides, which win over the file. It covers the server and what
//! gets indexed ([collections.*]). Every field has a built-in default
//! (config.reference.toml lists them all; config.example.toml is the short
//! starting point); unknown keys are rejected.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub engine: EngineConfig,
    /// What gets indexed, by collection name.
    pub collections: BTreeMap<String, CollectionConfig>,
    pub external: ExternalConfig,
    pub http: HttpConfig,
    pub watcher: WatcherConfig,
    pub embed: EmbedConfig,
    pub auth: AuthConfig,
}

/// Which search engine serves queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    /// rqmd linked into this binary: search, update and embed run in-process.
    Builtin,
    /// A separate qmd binary ([external]): spawned, supervised and proxied.
    External,
}

impl EngineKind {
    /// Unknown values are a loud error (never silently pick another engine).
    pub fn parse(s: &str) -> Option<EngineKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "builtin" => Some(EngineKind::Builtin),
            "external" => Some(EngineKind::External),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineConfig {
    /// "builtin" or "external".
    pub kind: String,
    /// Context shown to agents for every collection (qmd's global_context).
    pub context: String,
    /// builtin: index (index.sqlite) and downloaded models (qmd/models/).
    /// Empty = $XDG_CACHE_HOME/mademind, else ~/.cache/mademind.
    pub cache_dir: String,
    /// builtin: SQLite index path. Empty = <cache_dir>/index.sqlite.
    pub db: String,
    /// builtin: llama.cpp CPU threads. Its default uses every core, which
    /// oversubscribes many-core boxes; 0 = llama.cpp's default.
    pub threads: u32,
    /// Model overrides; empty = the engine's defaults.
    pub models: ModelsConfig,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            kind: "builtin".into(),
            context: String::new(),
            cache_dir: String::new(),
            db: String::new(),
            threads: 4,
            models: ModelsConfig::default(),
        }
    }
}

/// GGUF model URIs (`hf:<org>/<repo>/<file>.gguf` or a local path).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ModelsConfig {
    pub embed: String,
    pub rerank: String,
    pub generate: String,
}

/// One indexed tree of notes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CollectionConfig {
    /// Root directory (also watched, and served by /file).
    pub path: String,
    pub pattern: String,
    /// Gitignore-style patterns to skip.
    pub ignore: Vec<String>,
    /// Path prefix within the collection ("/" = all of it) -> description
    /// shown to agents alongside results.
    pub context: BTreeMap<String, String>,
    /// false = searched only when a query names this collection.
    pub include_by_default: bool,
}

impl Default for CollectionConfig {
    fn default() -> Self {
        CollectionConfig {
            path: String::new(),
            pattern: "**/*.md".into(),
            ignore: vec![],
            context: BTreeMap::new(),
            include_by_default: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExternalConfig {
    /// qmd binary for `mcp`, `update` and `embed`. Empty = no update/embed.
    pub bin: String,
    /// Loopback address the spawned `qmd mcp --http` binds.
    pub host: String,
    pub port: u16,
    /// Delay before respawning an exited `qmd mcp`.
    pub retry_seconds: u64,
    /// Proxy target. Empty = the spawned server at http://host:port; set =
    /// proxy-only (nothing is spawned).
    pub upstream: String,
}

impl Default for ExternalConfig {
    fn default() -> Self {
        ExternalConfig {
            bin: "/usr/local/bin/qmd".into(),
            host: "127.0.0.1".into(),
            port: 8181,
            retry_seconds: 5,
            upstream: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
    pub port: u16,
    /// external: per-request timeout for the proxied call, seconds. Must cover
    /// slow CPU inference (rerank queries can take minutes) while still
    /// bounding a wedged upstream.
    pub timeout_secs: u64,
}

impl Default for HttpConfig {
    fn default() -> Self {
        HttpConfig {
            port: 8888,
            timeout_secs: 600,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatcherConfig {
    pub debounce_ms: u64,
    pub extensions: Vec<String>,
    /// Also re-index every this many minutes (0 = only on file events).
    pub rescan_minutes: u64,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        WatcherConfig {
            debounce_ms: 2000,
            extensions: vec!["md".into(), "markdown".into()],
            rescan_minutes: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbedConfig {
    pub enabled: bool,
    pub interval_minutes: u64,
    pub boot_delay_seconds: u64,
    /// true = drop and re-embed everything every cycle
    pub force: bool,
    /// empty = all collections
    pub collections: Vec<String>,
    pub max_docs_per_batch: Option<u32>,
    pub max_batch_mb: Option<u32>,
    /// external only: qmd's `embed --timeout`
    pub timeout_minutes: Option<u32>,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        EmbedConfig {
            enabled: true,
            interval_minutes: 10,
            boot_delay_seconds: 60,
            force: false,
            collections: vec![],
            max_docs_per_batch: None,
            max_batch_mb: None,
            timeout_minutes: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ClientKey {
    /// Client identity; must match the X-Mademind-Client header.
    pub id: String,
    /// OpenSSH public key line body: `ssh-ed25519 <base64>` (the comment part of
    /// an authorized_keys line, without the trailing comment).
    pub public_key: String,
}

/// One source-IP auth rule. First matching rule (top to bottom in [auth])
/// decides how a request is treated; `ips` holds CIDRs or "any".
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthRule {
    /// CIDR list (IPv4/IPv6) or the literal "any".
    pub ips: Vec<String>,
    /// Reject unauthenticated requests (401) from sources matching this rule.
    pub required: bool,
    /// Client ids allowed under this rule (must be defined in [auth] clients).
    /// Empty = no client allowed (with required = true this is a block rule).
    pub clients: Vec<String>,
}

impl Default for AuthRule {
    fn default() -> Self {
        AuthRule {
            ips: vec!["any".into()],
            required: true,
            clients: vec![],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// Allowed clients (the key allowlist): { id, public_key = "ssh-ed25519 <base64>" }.
    pub clients: Vec<ClientKey>,
    /// Per-source-IP rules; first match wins. A request whose source matches
    /// no rule is rejected with 403.
    pub rules: Vec<AuthRule>,
    /// ± window (seconds) accepted for the signed timestamp (clock skew).
    pub tolerance_secs: i64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            clients: vec![],
            rules: vec![AuthRule::default()],
            tolerance_secs: 300,
        }
    }
}

pub fn parse_config_text(text: &str) -> Config {
    match toml::from_str::<Config>(text) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("mademind: config invalid ({e}); using built-in defaults");
            Config::default()
        }
    }
}

pub fn load_config(path: &Path) -> Config {
    match fs::read_to_string(path) {
        Ok(text) => {
            eprintln!("mademind: config loaded from {path:?}");
            parse_config_text(&text)
        }
        Err(_) => {
            eprintln!("mademind: no config at {path:?}; using built-in defaults");
            Config::default()
        }
    }
}

pub fn config_path() -> PathBuf {
    env::var("MADEMIND_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/config/config.toml"))
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    env::var(key).ok().and_then(|v| v.parse().ok())
}

/// Env overrides (env wins over the config file). Applied after load_config.
pub fn apply_env_overrides(cfg: &mut Config) {
    if let Ok(v) = env::var("MADEMIND_ENGINE") {
        cfg.engine.kind = v;
    }
    if let Ok(v) = env::var("MADEMIND_CACHE_DIR") {
        cfg.engine.cache_dir = v;
    }
    if let Ok(v) = env::var("MADEMIND_DB") {
        cfg.engine.db = v;
    }
    if let Some(v) = env_parse("MADEMIND_THREADS") {
        cfg.engine.threads = v;
    }
    if let Ok(v) = env::var("MADEMIND_QMD_BIN") {
        cfg.external.bin = v;
    }
    if let Ok(v) = env::var("MADEMIND_UPSTREAM") {
        cfg.external.upstream = v;
    }
    if let Some(v) = env_parse("PORT") {
        cfg.http.port = v;
    }
    if let Some(v) = env_parse("MADEMIND_RESCAN_INTERVAL_MIN") {
        cfg.watcher.rescan_minutes = v;
    }
    if let Some(v) = env_parse("MADEMIND_EMBED_INTERVAL_MIN") {
        cfg.embed.interval_minutes = v;
    }
    if let Some(v) = env_parse("MADEMIND_EMBED_BOOT_DELAY_SEC") {
        cfg.embed.boot_delay_seconds = v;
    }
    if let Some(v) = env_parse("MADEMIND_AUTH_TOLERANCE_SEC") {
        cfg.auth.tolerance_secs = v;
    }
}

// Used by the builtin engine only.
#[cfg_attr(not(feature = "builtin-engine"), allow(dead_code))]
impl EngineConfig {
    /// The cache dir with its default applied.
    pub fn cache_dir(&self) -> PathBuf {
        if !self.cache_dir.is_empty() {
            return PathBuf::from(&self.cache_dir);
        }
        match env::var_os("XDG_CACHE_HOME") {
            Some(d) if !d.is_empty() => PathBuf::from(d).join("mademind"),
            _ => env::home_dir()
                .unwrap_or_else(|| PathBuf::from("/root"))
                .join(".cache")
                .join("mademind"),
        }
    }

    /// The index path with its default applied.
    pub fn db_path(&self) -> PathBuf {
        if self.db.is_empty() {
            self.cache_dir().join("index.sqlite")
        } else {
            PathBuf::from(&self.db)
        }
    }
}

impl Config {
    /// Content roots: the collection paths (deduplicated). The watcher's
    /// targets and the only paths /file serves.
    pub fn data_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = self
            .collections
            .values()
            .map(|c| PathBuf::from(&c.path))
            .collect();
        roots.sort();
        roots.dedup();
        roots
    }

    /// Collection name -> root directory (for /file).
    pub fn collection_roots(&self) -> crate::files::Collections {
        self.collections
            .iter()
            .map(|(name, c)| (name.clone(), PathBuf::from(&c.path)))
            .collect()
    }

    /// The collections in qmd's index.yml shape, as JSON (a YAML subset, so
    /// qmd reads it as index.yml; rqmd deserializes it into its ConfigData).
    pub fn index_json(&self) -> serde_json::Value {
        let mut collections = serde_json::Map::new();
        for (name, c) in &self.collections {
            let mut o = serde_json::Map::new();
            o.insert("path".into(), c.path.clone().into());
            o.insert("pattern".into(), c.pattern.clone().into());
            if !c.ignore.is_empty() {
                o.insert("ignore".into(), c.ignore.clone().into());
            }
            if !c.context.is_empty() {
                o.insert("context".into(), serde_json::to_value(&c.context).unwrap());
            }
            if !c.include_by_default {
                o.insert("includeByDefault".into(), false.into());
            }
            collections.insert(name.clone(), o.into());
        }
        let mut root = serde_json::Map::new();
        if !self.engine.context.is_empty() {
            root.insert("global_context".into(), self.engine.context.clone().into());
        }
        root.insert("collections".into(), collections.into());
        let m = &self.engine.models;
        let models: serde_json::Map<String, serde_json::Value> = [
            ("embed", &m.embed),
            ("rerank", &m.rerank),
            ("generate", &m.generate),
        ]
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| (k.to_string(), v.clone().into()))
        .collect();
        if !models.is_empty() {
            root.insert("models".into(), models.into());
        }
        root.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = env::temp_dir().join(format!("mademind-cfg-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn engine_kind_parse_accepts_and_rejects() {
        assert_eq!(EngineKind::parse("builtin"), Some(EngineKind::Builtin));
        assert_eq!(EngineKind::parse(" External "), Some(EngineKind::External));
        assert_eq!(EngineKind::parse("rqmd"), None);
        assert_eq!(EngineKind::parse(""), None);
    }

    #[test]
    fn env_overrides_beat_config_file() {
        env::set_var("PORT", "9911");
        env::set_var("MADEMIND_ENGINE", "external");
        let mut cfg = parse_config_text("[http]\nport = 8888\n[engine]\nkind = \"builtin\"\n");
        apply_env_overrides(&mut cfg);
        assert_eq!(cfg.http.port, 9911);
        assert_eq!(cfg.engine.kind, "external");
        env::remove_var("PORT");
        env::remove_var("MADEMIND_ENGINE");
    }

    #[test]
    fn config_defaults_when_missing() {
        let cfg = load_config(Path::new("/nonexistent-mademind-config.toml"));
        assert_eq!(cfg.engine.kind, "builtin");
        assert!(cfg.collections.is_empty());
        assert_eq!(cfg.http.port, 8888);
        assert_eq!(cfg.external.host, "127.0.0.1");
        assert_eq!(cfg.external.port, 8181);
        assert_eq!(cfg.external.retry_seconds, 5);
        assert_eq!(cfg.watcher.debounce_ms, 2000);
        assert_eq!(
            cfg.watcher.extensions,
            vec!["md".to_string(), "markdown".to_string()]
        );
        assert_eq!(cfg.watcher.rescan_minutes, 0);
        assert!(cfg.embed.enabled);
        assert_eq!(cfg.embed.interval_minutes, 10);
        assert!(!cfg.embed.force);
        assert!(cfg.embed.collections.is_empty());
    }

    #[test]
    fn config_parses_full_toml() {
        let d = tmpdir("full");
        let f = d.join("config.toml");
        fs::write(
            &f,
            r#"
[engine]
kind = "external"
context = "My notes"
db = "/x/index.sqlite"
threads = 8

[engine.models]
embed = "/models/embed.gguf"

[collections.notes]
path = "/data/notes"
ignore = ["drafts/**"]
context = { "/" = "Personal notes", "/work" = "Work notes" }

[collections.archive]
path = "/data/archive"
pattern = "**/*.{md,txt}"
include_by_default = false

[external]
bin = "/opt/qmd"
host = "127.0.0.1"
port = 7777
retry_seconds = 11
upstream = "http://10.0.0.5:8181"

[http]
port = 9999
timeout_secs = 1234

[watcher]
debounce_ms = 500
extensions = ["md"]

[embed]
enabled = false
interval_minutes = 30
boot_delay_seconds = 5
force = true
collections = ["notes", "memories"]
max_docs_per_batch = 42
max_batch_mb = 1024
timeout_minutes = 50
"#,
        )
        .unwrap();
        let cfg = load_config(&f);
        assert_eq!(cfg.engine.kind, "external");
        assert_eq!(cfg.engine.db, "/x/index.sqlite");
        assert_eq!(
            cfg.data_roots(),
            vec![PathBuf::from("/data/archive"), PathBuf::from("/data/notes")]
        );
        assert_eq!(
            cfg.index_json(),
            serde_json::json!({
                "global_context": "My notes",
                "collections": {
                    "archive": {"path": "/data/archive", "pattern": "**/*.{md,txt}", "includeByDefault": false},
                    "notes": {
                        "path": "/data/notes",
                        "pattern": "**/*.md",
                        "ignore": ["drafts/**"],
                        "context": {"/": "Personal notes", "/work": "Work notes"}
                    }
                },
                "models": {"embed": "/models/embed.gguf"}
            })
        );
        assert_eq!(cfg.engine.threads, 8);
        assert_eq!(cfg.external.bin, "/opt/qmd");
        assert_eq!(cfg.external.port, 7777);
        assert_eq!(cfg.external.retry_seconds, 11);
        assert_eq!(cfg.external.upstream, "http://10.0.0.5:8181");
        assert_eq!(cfg.http.port, 9999);
        assert_eq!(cfg.http.timeout_secs, 1234);
        assert_eq!(cfg.watcher.debounce_ms, 500);
        assert_eq!(cfg.watcher.extensions, vec!["md".to_string()]);
        assert!(!cfg.embed.enabled);
        assert_eq!(cfg.embed.interval_minutes, 30);
        assert!(cfg.embed.force);
        assert_eq!(
            cfg.embed.collections,
            vec!["notes".to_string(), "memories".to_string()]
        );
        assert_eq!(cfg.embed.max_docs_per_batch, Some(42));
        assert_eq!(cfg.embed.max_batch_mb, Some(1024));
        assert_eq!(cfg.embed.timeout_minutes, Some(50));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn cache_dir_and_db_path_defaults() {
        let mut e = EngineConfig {
            cache_dir: "/var/cache/md".into(),
            ..EngineConfig::default()
        };
        assert_eq!(e.cache_dir(), PathBuf::from("/var/cache/md"));
        assert_eq!(e.db_path(), PathBuf::from("/var/cache/md/index.sqlite"));
        e.db = "/x/i.sqlite".into();
        assert_eq!(e.db_path(), PathBuf::from("/x/i.sqlite"));
        // unset: ends in .../mademind (XDG_CACHE_HOME or ~/.cache)
        assert!(EngineConfig::default().cache_dir().ends_with("mademind"));
    }

    #[test]
    fn config_partial_toml_keeps_other_defaults() {
        let d = tmpdir("partial");
        let f = d.join("config.toml");
        fs::write(&f, "[embed]\ninterval_minutes = 15\n").unwrap();
        let cfg = load_config(&f);
        assert_eq!(cfg.embed.interval_minutes, 15);
        assert_eq!(cfg.http.port, 8888);
        assert!(cfg.embed.enabled);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn config_invalid_or_unknown_field_falls_back_to_defaults() {
        let d = tmpdir("bad");
        let f = d.join("config.toml");
        fs::write(&f, "[http]\nport = 9000\n[nope]\nx = 1\n").unwrap();
        assert_eq!(load_config(&f).http.port, 8888); // unknown table -> defaults
        fs::write(&f, "not toml {{{").unwrap();
        assert_eq!(load_config(&f).http.port, 8888); // malformed -> defaults
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn reference_parses_and_matches_builtins() {
        // The reference must stay parseable and in sync with Config::default.
        // Deliberate exceptions: it defines one collection, and it opens
        // loopback (required = false) before the catch-all, while the built-in
        // default is a single fail-closed catch-all for when no config exists.
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.reference.toml");
        let text = fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
        let cfg: Config = toml::from_str(&text).expect("config.reference.toml must parse");
        let mut expected = Config::default();
        expected.collections.insert(
            "notes".into(),
            CollectionConfig {
                path: "/data/notes".into(),
                context: BTreeMap::from([("/".into(), "Personal notes".into())]),
                ..CollectionConfig::default()
            },
        );
        expected.auth.rules = vec![
            AuthRule {
                ips: vec!["127.0.0.1/32".into(), "::1/128".into()],
                required: false,
                clients: vec![],
            },
            AuthRule {
                ips: vec!["any".into()],
                required: true,
                clients: vec![],
            },
        ];
        assert_eq!(cfg, expected);
        assert!(Config::default().auth.rules[0].required);
    }

    #[test]
    fn example_parses() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
        let text = fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
        let cfg: Config = toml::from_str(&text).expect("config.example.toml must parse");
        assert_eq!(
            EngineKind::parse(&cfg.engine.kind),
            Some(EngineKind::Builtin)
        );
        assert_eq!(cfg.collections["notes"].path, "/data/notes");
        assert_eq!(cfg.auth.rules.len(), 2);
        assert!(!cfg.auth.rules[0].required);
    }
}
