//! Command line: `mademind serve` runs the server; everything else is a
//! client of a (local or remote) mademind, shaped after qmd's CLI so scripts
//! and agent prompts written for `qmd` keep working:
//!
//!   search / vsearch / query / get / multi-get / status
//!
//! with qmd's flags (-n, -c, --min-score, --all, --full, --format, --json,
//! --files, --line-numbers, --intent, --no-rerank, -l, --from, --max-bytes).
//! qmd's index-management commands have no client side here: the server owns
//! collections, indexing and embedding through its config.toml.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{anyhow, bail};
use serde_json::{json, Value};

use crate::client::{self, Client, ClientConfig};

const HELP: &str = "mademind - your notes, searchable by your AI agents

Server:
  mademind serve [-c config.toml]   Run the server (config: -c, else $MADEMIND_CONFIG,
                                    else /config/config.toml)
  mademind healthcheck [-c ...]     Exit 0 if the local server answers /healthz

Search (qmd-compatible; talks to the server in ~/.config/mademind/client.json):
  mademind query <query>            Keyword + semantic, reranked (--no-rerank: fast)
  mademind query $'lex: ..\\nvec: ..' Structured query (lex:/vec:/hyde:/intent: lines)
  mademind search <query>           Keywords only (BM25)
  mademind vsearch <query>          Semantic only
  mademind get <file>[:from[:count]]  Show a document (path or #docid)
  mademind multi-get <pattern>      Several documents (glob or comma-separated list)
  mademind status                   Collections, document counts, embedding state

Search options:
  -n <num>              Results (default 5; 20 with --json/--files)
  -c, --collection <n>  Only this collection (repeatable)
  --all                 All matches (use with --min-score)
  --min-score <0-1>     Drop weaker hits (vsearch default 0.3)
  --intent <text>       What the query is about (disambiguates, not searched)
  --no-rerank           query: skip the LLM rerank (slow on CPU)
  -C, --candidate-limit <n>  query: candidates to rerank
  --full                Whole documents instead of snippets
  --line-numbers        Number snippet lines
  --format <f>          cli (default), json, files, csv, md, xml (also --json, --files, ...)
  --full-path           Local paths instead of <collection>/<path> where this
                        machine has the collection (client.json local_roots)

get / multi-get options:
  --from <line>, -l <lines>, --no-line-numbers, --max-bytes <n> (multi-get),
  --format cli|json (multi-get), --full-path

Access:
  mademind genkey [<id>]            Create a signing key for this machine and print
                                    the line to add to the server's [auth] clients
  mademind request METHOD PATH [BODY]  Raw (signed) request, prints the body

  mademind --version | help

Client settings: ~/.config/mademind/client.json {url, client_id, key_file,
local_roots}, or MADEMIND_URL, MADEMIND_CLIENT_ID, MADEMIND_SIGN_KEY_FILE,
MADEMIND_LOCAL_ROOTS. Requests are signed when a key exists.
";

/// qmd commands that manage the index locally; here the server does that.
const SERVER_MANAGED: &[&str] = &[
    "collection",
    "context",
    "embed",
    "update",
    "cleanup",
    "init",
    "trust",
    "pull",
    "ls",
    "mcp",
    "bench",
    "skill",
    "skills",
];

/// Options that take a value (everything else starting with `-` is a flag).
const VALUE_OPTS: &[&str] = &[
    "n",
    "min-score",
    "format",
    "c",
    "collection",
    "config",
    "intent",
    "l",
    "from",
    "max-bytes",
    "C",
    "candidate-limit",
];

#[derive(Default, Debug)]
pub struct Args {
    pub pos: Vec<String>,
    opts: BTreeMap<String, Vec<String>>,
    flags: Vec<String>,
}

impl Args {
    pub fn parse(raw: &[String]) -> anyhow::Result<Args> {
        let mut a = Args::default();
        let mut it = raw.iter();
        let mut only_pos = false;
        while let Some(arg) = it.next() {
            if only_pos || arg == "-" || !arg.starts_with('-') {
                a.pos.push(arg.clone());
                continue;
            }
            if arg == "--" {
                only_pos = true;
                continue;
            }
            let name = arg.trim_start_matches('-');
            let (name, inline) = match name.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (name, None),
            };
            if VALUE_OPTS.contains(&name) {
                let v = match inline {
                    Some(v) => v,
                    None => it
                        .next()
                        .ok_or_else(|| anyhow!("{arg} needs a value"))?
                        .clone(),
                };
                a.opts.entry(canonical(name).into()).or_default().push(v);
            } else {
                a.flags.push(name.to_string());
            }
        }
        Ok(a)
    }

    pub fn opt(&self, name: &str) -> Option<&str> {
        self.opts
            .get(name)
            .and_then(|v| v.last())
            .map(String::as_str)
    }

    fn all(&self, name: &str) -> Vec<String> {
        self.opts.get(name).cloned().unwrap_or_default()
    }

    pub fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    fn num<T: std::str::FromStr>(&self, name: &str) -> anyhow::Result<Option<T>> {
        match self.opt(name) {
            None => Ok(None),
            Some(v) => v
                .parse()
                .map(Some)
                .map_err(|_| anyhow!("--{name}: not a number: {v}")),
        }
    }
}

fn canonical(name: &str) -> &str {
    match name {
        "c" => "collection",
        "C" => "candidate-limit",
        other => other,
    }
}

/// Client commands; returns the exit code. `cmd` is the first argument.
pub fn run(cmd: &str, rest: &[String]) -> anyhow::Result<i32> {
    if SERVER_MANAGED.contains(&cmd) {
        bail!(
            "`{cmd}` is qmd's local index management; mademind's server does that itself \
             (collections and schedules in its config.toml). Client commands: \
             query, search, vsearch, get, multi-get, status."
        );
    }
    let args = Args::parse(rest)?;
    match cmd {
        "genkey" => return genkey(&args),
        "help" | "-h" | "--help" => {
            print!("{HELP}");
            return Ok(0);
        }
        _ => {}
    }
    let client = Client::new(ClientConfig::load()?)?;
    match cmd {
        "search" => search(&client, &args, Mode::Lex),
        "vsearch" | "vector-search" => search(&client, &args, Mode::Vec),
        "query" | "deep-search" => search(&client, &args, Mode::Hybrid),
        "get" => get(&client, &args),
        "multi-get" => multi_get(&client, &args),
        "status" => status(&client, &args),
        "request" => request(&client, &args),
        other => {
            eprint!("{HELP}");
            bail!("unknown command: {other}")
        }
    }
}

pub fn help() {
    print!("{HELP}");
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Lex,
    Vec,
    Hybrid,
}

#[derive(Clone, Copy, PartialEq)]
enum Format {
    Cli,
    Json,
    Files,
    Csv,
    Md,
    Xml,
}

fn format(args: &Args) -> anyhow::Result<Format> {
    for (flag, f) in [
        ("json", Format::Json),
        ("files", Format::Files),
        ("csv", Format::Csv),
        ("md", Format::Md),
        ("xml", Format::Xml),
    ] {
        if args.flag(flag) {
            return Ok(f);
        }
    }
    Ok(match args.opt("format") {
        None | Some("cli") => Format::Cli,
        Some("json") => Format::Json,
        Some("files") => Format::Files,
        Some("csv") => Format::Csv,
        Some("md") => Format::Md,
        Some("xml") => Format::Xml,
        Some(f) => bail!("unknown format {f} (cli, json, files, csv, md, xml)"),
    })
}

/// qmd's query document: every line `lex:`/`vec:`/`hyde:`/`intent:`.
/// None = a plain query.
fn structured(query: &str) -> anyhow::Result<Option<(Vec<Value>, Option<String>)>> {
    let lines: Vec<&str> = query
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let typed = |l: &str| {
        let (k, v) = l.split_once(':')?;
        let k = k.trim().to_lowercase();
        ["lex", "vec", "hyde", "intent"]
            .contains(&k.as_str())
            .then(|| (k, v.trim().to_string()))
    };
    if lines.len() == 1 && typed(lines[0]).is_none() {
        return Ok(None);
    }
    let mut searches = vec![];
    let mut intent = None;
    for (i, l) in lines.iter().enumerate() {
        let Some((k, v)) = typed(l) else {
            bail!(
                "line {} has no lex:/vec:/hyde:/intent: prefix (needed in a multi-line query)",
                i + 1
            );
        };
        if v.is_empty() {
            bail!("line {}: {k}: needs text", i + 1);
        }
        if k == "intent" {
            intent = Some(v);
        } else {
            searches.push(json!({"type": k, "query": v}));
        }
    }
    if searches.is_empty() {
        bail!("a query document needs at least one lex:, vec: or hyde: line");
    }
    Ok(Some((searches, intent)))
}

fn search(client: &Client, args: &Args, mode: Mode) -> anyhow::Result<i32> {
    let query = args.pos.join(" ");
    if query.trim().is_empty() {
        bail!("usage: mademind {} [options] <query>", mode_name(mode));
    }
    let fmt = format(args)?;
    let default_limit = if matches!(fmt, Format::Json | Format::Files) {
        20
    } else {
        5
    };
    let limit: usize = if args.flag("all") {
        500
    } else {
        args.num("n")?.unwrap_or(default_limit)
    };
    let min_score: f64 =
        args.num("min-score")?
            .unwrap_or(if mode == Mode::Vec { 0.3 } else { 0.0 });
    let mut intent = args.opt("intent").map(str::to_string);
    let searches = match mode {
        Mode::Lex => vec![json!({"type": "lex", "query": query})],
        Mode::Vec => vec![json!({"type": "vec", "query": query})],
        Mode::Hybrid => match structured(&query)? {
            Some((s, i)) => {
                intent = intent.or(i);
                s
            }
            None => vec![
                json!({"type": "lex", "query": query}),
                json!({"type": "vec", "query": query}),
            ],
        },
    };
    let mut params = json!({
        "searches": searches,
        "limit": limit,
        "minScore": min_score,
        "rerank": mode == Mode::Hybrid && !args.flag("no-rerank"),
    });
    let collections = args.all("collection");
    if !collections.is_empty() {
        params["collections"] = json!(collections);
    }
    if let Some(i) = intent {
        params["intent"] = json!(i);
    }
    if let Some(c) = args.num::<u64>("candidate-limit")? {
        params["candidateLimit"] = json!(c);
    }
    let hits: Vec<Value> = client
        .query(&params)?
        .into_iter()
        .filter(|h| h["score"].as_f64().unwrap_or(0.0) >= min_score)
        .take(limit)
        .collect();
    let hits: Vec<Hit> = hits
        .iter()
        .map(|h| Hit::new(client, h, args))
        .collect::<anyhow::Result<_>>()?;
    print_hits(&hits, fmt, &query);
    Ok(0)
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Lex => "search",
        Mode::Vec => "vsearch",
        Mode::Hybrid => "query",
    }
}

struct Hit {
    docid: String,
    /// `<collection>/<path>`, or the local path with --full-path.
    file: String,
    line: u64,
    title: String,
    context: Option<String>,
    score: f64,
    /// Snippet, or the whole document with --full.
    text: String,
}

impl Hit {
    fn new(client: &Client, h: &Value, args: &Args) -> anyhow::Result<Hit> {
        let file = h["file"].as_str().unwrap_or("").to_string();
        let shown = match args.flag("full-path") {
            true => client
                .cfg
                .local_path(&file)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| file.clone()),
            false => file.clone(),
        };
        let numbered = args.flag("line-numbers");
        let text = if args.flag("full") {
            let r = client.call_tool("get", json!({"file": file, "lineNumbers": numbered}))?;
            tool_documents(&r).into_iter().map(|(_, t)| t).collect()
        } else {
            let s = h["snippet"].as_str().unwrap_or("");
            if numbered {
                s.to_string()
            } else {
                strip_line_numbers(s)
            }
        };
        Ok(Hit {
            docid: h["docid"].as_str().unwrap_or("").to_string(),
            file: shown,
            line: h["line"].as_u64().unwrap_or(1),
            title: h["title"].as_str().unwrap_or("").to_string(),
            context: h["context"].as_str().map(str::to_string),
            score: h["score"].as_f64().unwrap_or(0.0),
            text,
        })
    }
}

/// The server numbers snippet lines (`N: text`); qmd shows them only with
/// --line-numbers.
fn strip_line_numbers(s: &str) -> String {
    s.lines()
        .map(|l| match l.split_once(": ") {
            Some((n, rest)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => rest,
            _ => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn print_hits(hits: &[Hit], fmt: Format, query: &str) {
    if hits.is_empty() {
        match fmt {
            Format::Json => println!("[]"),
            Format::Csv => println!("docid,score,file,title,context,line,snippet"),
            Format::Xml => println!("<results></results>"),
            Format::Cli => eprintln!("No results found for: {query}"),
            _ => {}
        }
        return;
    }
    match fmt {
        Format::Json => {
            let out: Vec<Value> = hits
                .iter()
                .map(|h| {
                    let mut v = json!({"docid": h.docid, "score": h.score, "file": h.file,
                        "line": h.line, "title": h.title, "snippet": h.text});
                    if let Some(c) = &h.context {
                        v["context"] = json!(c);
                    }
                    v
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        }
        Format::Files => {
            for h in hits {
                let ctx = h
                    .context
                    .as_ref()
                    .map(|c| format!(",\"{}\"", c.replace('"', "\"\"")))
                    .unwrap_or_default();
                println!("{},{:.2},{}{ctx}", h.docid, h.score, h.file);
            }
        }
        Format::Csv => {
            println!("docid,score,file,title,context,line,snippet");
            let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
            for h in hits {
                println!(
                    "{},{:.2},{},{},{},{},{}",
                    h.docid,
                    h.score,
                    q(&h.file),
                    q(&h.title),
                    q(h.context.as_deref().unwrap_or("")),
                    h.line,
                    q(&h.text)
                );
            }
        }
        Format::Md => {
            for h in hits {
                let heading = if h.title.is_empty() {
                    &h.file
                } else {
                    &h.title
                };
                let ctx = h
                    .context
                    .as_ref()
                    .map(|c| format!("**context:** {c}\n"))
                    .unwrap_or_default();
                println!(
                    "---\n# {heading}\n**file:** `{}`\n**docid:** `{}`\n{ctx}\n{}\n",
                    h.file, h.docid, h.text
                );
            }
        }
        Format::Xml => {
            let a = |s: &str| s.replace('&', "&amp;").replace('"', "&quot;");
            for h in hits {
                let ctx = h
                    .context
                    .as_ref()
                    .map(|c| format!(" context=\"{}\"", a(c)))
                    .unwrap_or_default();
                println!(
                    "<file docid=\"{}\" name=\"{}\" title=\"{}\"{ctx}>\n{}\n</file>\n",
                    h.docid,
                    a(&h.file),
                    a(&h.title),
                    h.text
                );
            }
        }
        Format::Cli => {
            let color = std::io::stdout().is_terminal();
            let (cyan, dim, bold, reset) = if color {
                ("\x1b[36m", "\x1b[2m", "\x1b[1m", "\x1b[0m")
            } else {
                ("", "", "", "")
            };
            for (i, h) in hits.iter().enumerate() {
                println!(
                    "{cyan}{}{dim}:{}{reset} {dim}{}{reset}",
                    h.file, h.line, h.docid
                );
                if !h.title.is_empty() {
                    println!("{bold}Title: {}{reset}", h.title);
                }
                if let Some(c) = &h.context {
                    println!("{dim}Context: {c}{reset}");
                }
                println!("Score: {bold}{:>3.0}%{reset}", h.score * 100.0);
                println!();
                println!("{}", h.text);
                if i + 1 < hits.len() {
                    println!("\n");
                }
            }
        }
    }
}

/// `(collection/path, text)` for each document in a get/multi_get result;
/// plain text blocks (errors, skipped files) come back with an empty path.
fn tool_documents(result: &Value) -> Vec<(String, String)> {
    let mut out = vec![];
    for c in result["content"].as_array().into_iter().flatten() {
        if let Some(r) = c.get("resource") {
            let uri = r["uri"].as_str().unwrap_or("");
            let path = client::percent_decode(uri.strip_prefix("qmd://").unwrap_or(uri));
            out.push((path, r["text"].as_str().unwrap_or("").to_string()));
        } else if let Some(t) = c["text"].as_str() {
            out.push((String::new(), t.to_string()));
        }
    }
    out
}

fn tool_error(result: &Value) -> Option<String> {
    result["isError"].as_bool().filter(|e| *e).map(|_| {
        tool_documents(result)
            .into_iter()
            .map(|(_, t)| t)
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// `file`, `file:from` or `file:from:count` (qmd's get syntax).
fn split_line_range(spec: &str) -> (String, Option<u64>, Option<u64>) {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let parts: Vec<&str> = spec.rsplitn(3, ':').collect();
    match parts.as_slice() {
        [count, from, file] if digits(count) && digits(from) && !file.is_empty() => {
            (file.to_string(), from.parse().ok(), count.parse().ok())
        }
        [from, rest @ ..] if digits(from) && !rest.is_empty() => {
            let file = spec[..spec.len() - from.len() - 1].to_string();
            (file, from.parse().ok(), None)
        }
        _ => (spec.to_string(), None, None),
    }
}

fn get(client: &Client, args: &Args) -> anyhow::Result<i32> {
    let Some(spec) = args.pos.first() else {
        bail!("usage: mademind get <file>[:from[:count]] [--from <line>] [-l <lines>] [--no-line-numbers]");
    };
    let (file, from, count) = split_line_range(spec);
    let mut a = json!({"file": file, "lineNumbers": !args.flag("no-line-numbers")});
    if let Some(f) = args.num::<u64>("from")?.or(from) {
        a["fromLine"] = json!(f);
    }
    if let Some(l) = args.num::<u64>("l")?.or(count) {
        a["maxLines"] = json!(l);
    }
    let r = client.call_tool("get", a)?;
    if let Some(e) = tool_error(&r) {
        eprintln!("{e}");
        return Ok(1);
    }
    for (path, text) in tool_documents(&r) {
        print_local(client, &path, args);
        println!("{text}");
    }
    Ok(0)
}

/// Where this machine has the file (stderr, so stdout stays the document).
fn print_local(client: &Client, path: &str, args: &Args) {
    if path.is_empty() {
        return;
    }
    match client.cfg.local_path(path) {
        Some(p) => eprintln!("local: {}", p.display()),
        None if args.flag("full-path") => eprintln!("local: no copy on this machine"),
        None => {}
    }
}

fn multi_get(client: &Client, args: &Args) -> anyhow::Result<i32> {
    let Some(pattern) = args.pos.first() else {
        bail!("usage: mademind multi-get <glob|a.md,b.md> [-l <lines>] [--max-bytes <n>] [--no-line-numbers] [--format cli|json]");
    };
    let mut a = json!({"pattern": pattern, "lineNumbers": !args.flag("no-line-numbers")});
    if let Some(l) = args.num::<u64>("l")? {
        a["maxLines"] = json!(l);
    }
    if let Some(b) = args.num::<u64>("max-bytes")? {
        a["maxBytes"] = json!(b);
    }
    let r = client.call_tool("multi_get", a)?;
    if let Some(e) = tool_error(&r) {
        eprintln!("{e}");
        return Ok(1);
    }
    let docs = tool_documents(&r);
    if format(args)? == Format::Json {
        let out: Vec<Value> = docs
            .iter()
            .map(|(p, t)| {
                let mut v = json!({"file": p, "body": t});
                if let Some(l) = client.cfg.local_path(p) {
                    v["local"] = json!(l.display().to_string());
                }
                v
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return Ok(0);
    }
    for (path, text) in docs {
        if path.is_empty() {
            eprintln!("{text}");
            continue;
        }
        let shown = match (args.flag("full-path"), client.cfg.local_path(&path)) {
            (true, Some(p)) => p.display().to_string(),
            _ => path.clone(),
        };
        println!("=== {shown} ===\n{text}\n");
    }
    Ok(0)
}

fn status(client: &Client, args: &Args) -> anyhow::Result<i32> {
    let r = client.call_tool("status", json!({}))?;
    let s = &r["structuredContent"];
    if args.flag("json") || args.opt("format") == Some("json") {
        println!("{}", serde_json::to_string_pretty(s).unwrap_or_default());
        return Ok(0);
    }
    if s.is_null() {
        for (_, t) in tool_documents(&r) {
            println!("{t}");
        }
        return Ok(0);
    }
    println!("mademind at {}", client.cfg.url);
    println!(
        "  Documents:       {} ({} need embedding)",
        s["totalDocuments"], s["needsEmbedding"]
    );
    println!(
        "  Semantic search: {}",
        if s["hasVectorIndex"].as_bool().unwrap_or(false) {
            "ready"
        } else {
            "not ready yet (embedding in the background)"
        }
    );
    println!("  Collections:");
    for c in s["collections"].as_array().into_iter().flatten() {
        let name = c["name"].as_str().unwrap_or("");
        let local = client
            .cfg
            .local_path(name)
            .map(|p| format!("  local: {}", p.display()))
            .unwrap_or_default();
        println!(
            "    {name:<20} {:>6} docs  updated {}{local}",
            c["documents"].as_i64().unwrap_or(0),
            c["lastUpdated"].as_str().unwrap_or("-")
        );
    }
    Ok(0)
}

fn request(client: &Client, args: &Args) -> anyhow::Result<i32> {
    let (Some(method), Some(path)) = (args.pos.first(), args.pos.get(1)) else {
        bail!("usage: mademind request METHOD PATH [BODY]");
    };
    let body = args.pos.get(2).map(String::as_str).unwrap_or("");
    let (r, _) = client.request(method, path, body, &[])?;
    if let Some(f) = &r.file {
        eprintln!("file: {f}");
        match client.cfg.local_path(f) {
            Some(p) => eprintln!("local: {}", p.display()),
            None => eprintln!("local: no copy on this machine"),
        }
    }
    print!("{}", r.body);
    Ok(if (200..300).contains(&r.status) { 0 } else { 1 })
}

fn genkey(args: &Args) -> anyhow::Result<i32> {
    let cfg = ClientConfig::load()?;
    let (id, path): (String, PathBuf) = match args.pos.first() {
        Some(id) => (id.clone(), client::default_key_file(id)),
        None => (cfg.client_id.clone(), cfg.key_file.clone()),
    };
    let public = client::generate_key(&path)?;
    eprintln!("private key written to {}", path.display());
    eprintln!("add to the server's config.toml [auth] clients (and to a rule's clients):");
    println!(
        "{{ id = \"{id}\", public_key = \"{}\" }}",
        crate::auth::public_key_line(&public)
    );
    if args.pos.first().is_some_and(|i| *i != cfg.client_id) {
        eprintln!(
            "this machine's client id is \"{}\"; set \"client_id\": \"{id}\" in {}",
            cfg.client_id,
            ClientConfig::path().display()
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_qmd_style_args() {
        let a = Args::parse(&s(&[
            "auth",
            "flow",
            "-n",
            "3",
            "-c",
            "notes",
            "--collection=work",
            "--json",
            "--min-score",
            "0.4",
        ]))
        .unwrap();
        assert_eq!(a.pos, s(&["auth", "flow"]));
        assert_eq!(a.opt("n"), Some("3"));
        assert_eq!(a.all("collection"), s(&["notes", "work"]));
        assert!(a.flag("json"));
        assert_eq!(a.num::<f64>("min-score").unwrap(), Some(0.4));
        assert!(Args::parse(&s(&["-n"])).is_err());
        let a = Args::parse(&s(&["--", "-not-a-flag"])).unwrap();
        assert_eq!(a.pos, s(&["-not-a-flag"]));
    }

    #[test]
    fn structured_query_documents() {
        assert!(structured("plain words").unwrap().is_none());
        let (s, i) = structured("lex: a b\nvec: what is a\nintent: x")
            .unwrap()
            .unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0]["type"], "lex");
        assert_eq!(i.as_deref(), Some("x"));
        assert!(structured("lex: a\nno prefix").is_err());
        assert!(structured("intent: only").is_err());
        let (s, _) = structured("hyde: an answer").unwrap().unwrap();
        assert_eq!(s[0]["type"], "hyde");
    }

    #[test]
    fn get_line_ranges() {
        assert_eq!(split_line_range("a/b.md"), ("a/b.md".into(), None, None));
        assert_eq!(
            split_line_range("a/b.md:10"),
            ("a/b.md".into(), Some(10), None)
        );
        assert_eq!(
            split_line_range("a/b.md:10:5"),
            ("a/b.md".into(), Some(10), Some(5))
        );
        assert_eq!(split_line_range("#abc123"), ("#abc123".into(), None, None));
    }

    #[test]
    fn strips_server_line_numbers() {
        assert_eq!(strip_line_numbers("12: foo\n13: bar: baz"), "foo\nbar: baz");
        assert_eq!(strip_line_numbers("no: numbers"), "no: numbers");
    }
}
