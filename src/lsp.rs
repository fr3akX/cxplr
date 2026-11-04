//! Minimal Language Server Protocol client for source navigation: hover, go to
//! definition and find references. One server process per language, started
//! on first use with the repository as workspace root.

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::oneshot;

const INIT_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_REFERENCES: usize = 500;

/// Language → candidate server commands; the first one found on PATH is used.
const DEFAULT_SERVERS: &[(&str, &[&str])] = &[
    ("rust", &["rust-analyzer"]),
    ("python", &["pyright-langserver --stdio", "basedpyright-langserver --stdio", "pylsp", "jedi-language-server"]),
    ("typescript", &["typescript-language-server --stdio"]),
    ("go", &["gopls"]),
    ("c", &["clangd"]),
];

pub const LANGUAGES: &[&str] = &["rust", "python", "typescript", "go", "c"];

/// Server language for a file, plus the document's LSP `languageId`.
pub fn language_of(path: &str) -> Option<(&'static str, &'static str)> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => ("rust", "rust"),
        "py" | "pyi" => ("python", "python"),
        "ts" | "mts" | "cts" => ("typescript", "typescript"),
        "tsx" => ("typescript", "typescriptreact"),
        "js" | "mjs" | "cjs" => ("typescript", "javascript"),
        "jsx" => ("typescript", "javascriptreact"),
        "go" => ("go", "go"),
        "c" | "h" => ("c", "c"),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => ("c", "cpp"),
        _ => return None,
    })
}

/// Settings that keep servers from running project code (build scripts,
/// proc macros, `cargo check`) while browsing an untrusted repository.
fn initialization_options(lang: &str) -> Value {
    match lang {
        "rust" => json!({
            "cargo": { "buildScripts": { "enable": false } },
            "procMacro": { "enable": false },
            "checkOnSave": false
        }),
        _ => Value::Null,
    }
}

fn on_path(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// `file://` URI for an absolute path; everything but unreserved bytes and `/` is percent-encoded.
pub fn path_to_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let bytes = uri.strip_prefix("file://")?.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%' && i + 3 <= bytes.len())
            .then(|| std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()))
            .flatten();
        match escaped {
            Some(b) => {
                out.push(b);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok().map(PathBuf::from)
}

type Shared<T> = Arc<Mutex<T>>;
type Pending = Shared<HashMap<i64, oneshot::Sender<Result<Value, String>>>>;
type Stdin = Arc<tokio::sync::Mutex<ChildStdin>>;

struct Server {
    command: String,
    stdin: Stdin,
    pending: Pending,
    next_id: AtomicI64,
    opened: Mutex<HashSet<String>>,
    /// Active `$/progress` tokens (indexing, analysis).
    progress: Shared<HashSet<String>>,
    exited: Shared<Option<String>>,
    _child: Child,
}

async fn write_message(stdin: &Stdin, msg: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    let mut stdin = stdin.lock().await;
    stdin.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes()).await?;
    stdin.write_all(&body).await?;
    stdin.flush().await
}

impl Server {
    fn exit_reason(&self) -> Option<String> {
        self.exited.lock().unwrap().clone()
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        if let Some(reason) = self.exit_reason() {
            return Err(reason);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = write_message(&self.stdin, &msg).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(self.exit_reason().unwrap_or_else(|| format!("{}: {e}", self.command)));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(self.exit_reason().unwrap_or_else(|| "language server stopped".into())),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("{method} timed out after {}s (the server may still be indexing)", timeout.as_secs()))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        write_message(&self.stdin, &msg).await.map_err(|e| format!("{}: {e}", self.command))
    }
}

/// Completes pending requests, answers the server-to-client requests servers
/// wait on, and tracks progress. Ends when the server's stdout closes.
async fn read_loop(
    stdout: ChildStdout,
    stdin: Stdin,
    pending: Pending,
    progress: Shared<HashSet<String>>,
    exited: Shared<Option<String>>,
    stderr_tail: Shared<String>,
    root_uri: String,
) {
    let mut reader = BufReader::new(stdout);
    let mut header = String::new();
    'messages: loop {
        let mut length = None;
        loop {
            header.clear();
            match reader.read_line(&mut header).await {
                Ok(0) | Err(_) => break 'messages,
                Ok(_) => {}
            }
            let line = header.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length:") {
                length = v.trim().parse::<usize>().ok();
            }
        }
        let Some(length) = length else { continue };
        let mut body = vec![0u8; length];
        if reader.read_exact(&mut body).await.is_err() {
            break;
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&body) else { continue };
        match (msg["method"].as_str(), msg.get("id")) {
            (None, Some(id)) => {
                let tx = id.as_i64().and_then(|id| pending.lock().unwrap().remove(&id));
                if let Some(tx) = tx {
                    let result = match msg.get("error") {
                        Some(err) => Err(err["message"].as_str().unwrap_or("request failed").to_owned()),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(result);
                }
            }
            (Some(method), Some(id)) => {
                let result = match method {
                    "workspace/configuration" => {
                        json!(vec![Value::Null; msg["params"]["items"].as_array().map_or(0, Vec::len)])
                    }
                    "workspace/workspaceFolders" => json!([{ "uri": root_uri, "name": "workspace" }]),
                    _ => Value::Null,
                };
                let _ = write_message(&stdin, &json!({ "jsonrpc": "2.0", "id": id, "result": result })).await;
            }
            (Some("$/progress"), None) => {
                let token = msg["params"]["token"].to_string();
                match msg["params"]["value"]["kind"].as_str() {
                    Some("begin") => {
                        progress.lock().unwrap().insert(token);
                    }
                    Some("end") => {
                        progress.lock().unwrap().remove(&token);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    // Give the stderr reader a moment to capture why the server quit.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let tail = stderr_tail.lock().unwrap().clone();
    let reason = tail.lines().rev().find(|l| !l.trim().is_empty()).map(str::trim).map(str::to_owned);
    let reason = match reason {
        Some(line) => format!("language server exited: {line}"),
        None => "language server exited".to_owned(),
    };
    *exited.lock().unwrap() = Some(reason.clone());
    for (_, tx) in pending.lock().unwrap().drain() {
        let _ = tx.send(Err(reason.clone()));
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Location {
    /// Repository-relative path, or `None` when the target is outside the repository.
    pub path: Option<String>,
    /// Path shown to the user (relative inside the repository, absolute outside).
    pub display: String,
    /// 1-based.
    pub line: u64,
    /// 0-based UTF-16 column, as LSP reports it.
    pub character: u64,
    pub preview: String,
}

#[derive(Serialize)]
pub struct SymbolInfo {
    pub hover: Option<String>,
    pub definitions: Vec<Location>,
}

#[derive(Serialize)]
pub struct Status {
    pub language: Option<&'static str>,
    pub server: Option<String>,
    /// `none` (no server for this file), `idle` (not started), `starting`, `ready`, `indexing` or `failed`.
    pub state: &'static str,
    pub error: Option<String>,
}

/// One start attempt per language; concurrent first requests wait on the same cell
/// instead of holding the map lock, so status checks never block on a slow start.
type Slot = Arc<tokio::sync::OnceCell<Result<Arc<Server>, String>>>;

pub struct Lsp {
    root: PathBuf,
    root_uri: String,
    commands: HashMap<String, String>,
    slots: Mutex<HashMap<&'static str, Slot>>,
}

impl Lsp {
    /// `overrides` are `language=command` pairs; an empty command disables that language.
    pub fn new(root: &Path, enabled: bool, overrides: &[(String, String)]) -> Lsp {
        let mut commands = HashMap::new();
        if enabled {
            for (lang, candidates) in DEFAULT_SERVERS {
                if let Some(cmd) = candidates.iter().find(|c| c.split_whitespace().next().is_some_and(on_path)) {
                    commands.insert((*lang).to_owned(), (*cmd).to_owned());
                }
            }
            for (lang, cmd) in overrides {
                if cmd.trim().is_empty() {
                    commands.remove(lang);
                } else {
                    commands.insert(lang.clone(), cmd.trim().to_owned());
                }
            }
        }
        Lsp { root: root.to_owned(), root_uri: path_to_uri(root), commands, slots: Default::default() }
    }

    /// Configured (language, command) pairs, for the startup banner.
    pub fn configured(&self) -> Vec<(String, String)> {
        let mut list: Vec<_> = self.commands.iter().map(|(l, c)| (l.clone(), c.clone())).collect();
        list.sort();
        list
    }

    pub async fn status(&self, path: &str) -> Status {
        let Some((lang, _)) = language_of(path) else {
            return Status { language: None, server: None, state: "none", error: None };
        };
        let Some(cmd) = self.commands.get(lang) else {
            return Status { language: Some(lang), server: None, state: "none", error: None };
        };
        let slot = self.slots.lock().unwrap().get(lang).cloned();
        let (state, error) = match slot.as_ref().map(|s| s.get()) {
            None => ("idle", None),
            Some(None) => ("starting", None),
            Some(Some(Err(e))) => ("failed", Some(e.clone())),
            Some(Some(Ok(s))) => match s.exit_reason() {
                Some(e) => ("failed", Some(e)),
                None if !s.progress.lock().unwrap().is_empty() => ("indexing", None),
                None => ("ready", None),
            },
        };
        Status { language: Some(lang), server: Some(cmd.clone()), state, error }
    }

    async fn server(&self, lang: &'static str) -> Result<Arc<Server>, String> {
        let cmd = self.commands.get(lang).ok_or_else(|| format!("no language server configured for {lang}"))?;
        let slot = self.slots.lock().unwrap().entry(lang).or_default().clone();
        let server = slot.get_or_init(|| self.start(lang, cmd)).await.clone()?;
        server.exit_reason().map_or(Ok(server), Err)
    }

    async fn start(&self, lang: &str, cmd: &str) -> Result<Arc<Server>, String> {
        let argv: Vec<&str> = cmd.split_whitespace().collect();
        let mut child = Command::new(argv[0])
            .args(&argv[1..])
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot start `{cmd}`: {e}"))?;
        let stdin: Stdin = Arc::new(tokio::sync::Mutex::new(child.stdin.take().expect("piped stdin")));
        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");

        let stderr_tail: Shared<String> = Arc::default();
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            while let Ok(n @ 1..) = stderr.read(&mut chunk).await {
                let mut t = tail.lock().unwrap();
                t.push_str(&String::from_utf8_lossy(&chunk[..n]));
                if t.len() > 8192 {
                    let mut cut = t.len() - 8192;
                    while !t.is_char_boundary(cut) {
                        cut += 1;
                    }
                    t.drain(..cut);
                }
            }
        });

        let server = Arc::new(Server {
            command: cmd.to_owned(),
            stdin: stdin.clone(),
            pending: Arc::default(),
            next_id: AtomicI64::new(1),
            opened: Mutex::default(),
            progress: Arc::default(),
            exited: Arc::default(),
            _child: child,
        });
        tokio::spawn(read_loop(
            stdout,
            stdin,
            server.pending.clone(),
            server.progress.clone(),
            server.exited.clone(),
            stderr_tail,
            self.root_uri.clone(),
        ));

        let folder = json!([{ "uri": self.root_uri, "name": "workspace" }]);
        let params = json!({
            "processId": std::process::id(),
            "rootUri": self.root_uri,
            "rootPath": self.root,
            "workspaceFolders": folder,
            "initializationOptions": initialization_options(lang),
            "capabilities": {
                "general": { "positionEncodings": ["utf-16"] },
                "textDocument": {
                    "synchronization": { "didSave": false },
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "definition": { "linkSupport": true },
                    "references": {}
                },
                "window": { "workDoneProgress": true },
                "workspace": { "configuration": true, "workspaceFolders": true }
            }
        });
        server
            .request("initialize", params, INIT_TIMEOUT)
            .await
            .map_err(|e| format!("`{cmd}` failed to initialize: {e}"))?;
        server.notify("initialized", json!({})).await?;
        Ok(server)
    }

    async fn open(&self, full: &Path, rel: &str) -> Result<(Arc<Server>, String), String> {
        let (lang, language_id) = language_of(rel).ok_or("no language server for this file type")?;
        let server = self.server(lang).await?;
        let uri = path_to_uri(full);
        let first = server.opened.lock().unwrap().insert(uri.clone());
        if first {
            let text = fs::read_to_string(full).map_err(|e| format!("{rel}: {e}"))?;
            let doc = json!({ "uri": uri, "languageId": language_id, "version": 1, "text": text });
            server.notify("textDocument/didOpen", json!({ "textDocument": doc })).await?;
        }
        Ok((server, uri))
    }

    fn position(uri: &str, line: u64, character: u64) -> Value {
        json!({ "textDocument": { "uri": uri }, "position": { "line": line.saturating_sub(1), "character": character } })
    }

    /// Start the file's server and open the document so analysis begins before the first click.
    pub async fn warm(&self, full: &Path, rel: &str) {
        let _ = self.open(full, rel).await;
    }

    /// Hover text and definitions for the symbol at a 1-based line and UTF-16 column.
    pub async fn symbol(&self, full: &Path, rel: &str, line: u64, character: u64) -> Result<SymbolInfo, String> {
        let (server, uri) = self.open(full, rel).await?;
        let at = Self::position(&uri, line, character);
        let (hover, definition) = tokio::join!(
            server.request("textDocument/hover", at.clone(), REQUEST_TIMEOUT),
            server.request("textDocument/definition", at, REQUEST_TIMEOUT),
        );
        let definitions = self.locations(&definition?);
        Ok(SymbolInfo { hover: hover.ok().as_ref().and_then(hover_text), definitions })
    }

    pub async fn references(&self, full: &Path, rel: &str, line: u64, character: u64) -> Result<Vec<Location>, String> {
        let (server, uri) = self.open(full, rel).await?;
        let mut params = Self::position(&uri, line, character);
        params["context"] = json!({ "includeDeclaration": true });
        let result = server.request("textDocument/references", params, REQUEST_TIMEOUT).await?;
        let mut refs = self.locations(&result);
        refs.truncate(MAX_REFERENCES);
        Ok(refs)
    }

    /// Accepts `Location`, `Location[]` and `LocationLink[]` results.
    fn locations(&self, result: &Value) -> Vec<Location> {
        let items: Vec<&Value> = match result {
            Value::Array(items) => items.iter().collect(),
            Value::Object(_) => vec![result],
            _ => vec![],
        };
        let mut lines_cache: HashMap<PathBuf, Vec<String>> = HashMap::new();
        let mut out = Vec::new();
        for item in items {
            let (uri, range) = match item.get("targetUri") {
                Some(uri) => (uri, item.get("targetSelectionRange").or_else(|| item.get("targetRange"))),
                None => (&item["uri"], item.get("range")),
            };
            let Some(path) = uri.as_str().and_then(uri_to_path) else { continue };
            let start = &range.cloned().unwrap_or_default()["start"];
            let line = start["line"].as_u64().unwrap_or(0) + 1;
            let character = start["character"].as_u64().unwrap_or(0);
            let inside = path.canonicalize().ok().filter(|p| p.starts_with(&self.root));
            let rel = inside
                .as_ref()
                .and_then(|p| p.strip_prefix(&self.root).ok())
                .map(|p| p.to_string_lossy().into_owned());
            let preview = inside
                .as_ref()
                .and_then(|p| {
                    let lines = lines_cache.entry(p.clone()).or_insert_with(|| {
                        fs::read_to_string(p).map(|t| t.lines().map(str::to_owned).collect()).unwrap_or_default()
                    });
                    lines.get(line as usize - 1).map(|l| l.trim().chars().take(200).collect())
                })
                .unwrap_or_default();
            out.push(Location {
                display: rel.clone().unwrap_or_else(|| path.display().to_string()),
                path: rel,
                line,
                character,
                preview,
            });
        }
        out
    }
}

fn hover_text(result: &Value) -> Option<String> {
    fn part(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => {
                let value = o.get("value")?.as_str()?;
                match o.get("language").and_then(Value::as_str) {
                    Some(lang) => Some(format!("```{lang}\n{value}\n```")),
                    None => Some(value.to_owned()),
                }
            }
            _ => None,
        }
    }
    let contents = result.get("contents")?;
    let text = match contents {
        Value::Array(parts) => parts.iter().filter_map(part).collect::<Vec<_>>().join("\n\n"),
        other => part(other)?,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_round_trip_with_reserved_characters() {
        let path = Path::new("/tmp/a dir/ü#?.py");
        let uri = path_to_uri(path);
        assert_eq!(uri, "file:///tmp/a%20dir/%C3%BC%23%3F.py");
        assert_eq!(uri_to_path(&uri).unwrap(), path);
        assert_eq!(uri_to_path("file:///x/100%").unwrap(), Path::new("/x/100%"));
    }
}
