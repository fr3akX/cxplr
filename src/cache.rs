//! Project-local, content-addressed cache in `<repo>/.cxplr/` (it ignores itself
//! in git).
//!
//! - `maps/<key>.json`: one map. The key covers the cache format, prompt version,
//!   engine, model, scope and the content of every file under the paths the map
//!   describes, so a component keeps its map as long as its own files are unchanged,
//!   however they are committed.
//! - `scopes/<scope hash>.json`: the latest map of a scope, its paths and the file
//!   manifest it was built from. When the files change, the latest map is still
//!   served, marked stale with exactly which files changed, until it is refreshed.
//! - `traces/<key>.json`: example inputs for one flow, keyed by the scope's content
//!   key and the flow name.
//! - `sessions.json` (Codex) / `sessions-claude.json` (Claude Code): every session
//!   cxplr created (id → name), so they can be deleted with `--forget-sessions`.

use crate::engine::Kind;
use crate::map::Map;
use crate::prompt;
use crate::repo::{self, Manifest, Repo};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const FORMAT: &str = "cxplr/2";

#[derive(Serialize, Deserialize)]
struct Binding {
    scope: String,
    paths: Vec<String>,
    key: String,
    commit: Option<String>,
    manifest: Manifest,
    /// Session that produced the map; deeper maps and questions fork it.
    #[serde(default)]
    session: Option<String>,
    /// Engine the session belongs to; `None` in caches written before Claude Code
    /// support, which only had Codex.
    #[serde(default)]
    engine: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Freshness {
    Fresh,
    /// The covered files changed since the cached map was made (at commit `since`).
    Stale { since: Option<String>, changed: Vec<String>, total: usize },
}

pub struct Cached {
    pub map: Map,
    pub freshness: Freshness,
}

pub struct Cache {
    dir: PathBuf,
    engine: &'static str,
    salt: String,
    /// Serializes read-modify-write of the session registry across concurrent jobs.
    sessions_lock: Mutex<()>,
    /// One atlas rebuild at a time.
    atlas_lock: Mutex<()>,
}

fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, path)
}

impl Cache {
    pub fn open(dir: PathBuf, engine: Kind, model: Option<&str>) -> io::Result<Cache> {
        fs::create_dir_all(dir.join("maps"))?;
        fs::create_dir_all(dir.join("scopes"))?;
        fs::create_dir_all(dir.join("traces"))?;
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            fs::write(ignore, "# cxplr cache: local only\n*\n")?;
        }
        // Codex keys predate engines and stay as they were, so existing caches stay fresh.
        let engine_part = match engine {
            Kind::Codex => String::new(),
            other => format!("|engine {}", other.id()),
        };
        let salt = format!("{FORMAT}|prompt {}{engine_part}|model {}", prompt::VERSION, model.unwrap_or("default"));
        Ok(Cache { dir, engine: engine.id(), salt, sessions_lock: Mutex::new(()), atlas_lock: Mutex::new(()) })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn lock_atlas(&self) -> std::sync::MutexGuard<'_, ()> {
        self.atlas_lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Key of the map for `scope` describing exactly the files in `manifest`.
    pub fn key(&self, scope: &str, manifest: &Manifest) -> String {
        digest(&format!("{}\n{scope}\n{}", self.salt, repo::fingerprint(manifest)))[..32].to_owned()
    }

    fn binding_file(&self, scope: &str) -> PathBuf {
        self.dir.join("scopes").join(format!("{}.json", &digest(scope)[..24]))
    }

    fn map_file(&self, key: &str) -> PathBuf {
        self.dir.join("maps").join(format!("{key}.json"))
    }

    fn binding(&self, scope: &str) -> Option<Binding> {
        let b: Binding = serde_json::from_slice(&fs::read(self.binding_file(scope)).ok()?).ok()?;
        (b.scope == scope).then_some(b)
    }

    fn read_map(&self, key: &str) -> Option<Map> {
        serde_json::from_slice(&fs::read(self.map_file(key)).ok()?).ok()
    }

    /// The map for the current content if cached, else the latest one marked stale.
    pub fn load(&self, repo: &Repo, scope: &str) -> Option<Cached> {
        let binding = self.binding(scope)?;
        let current = repo.manifest(&binding.paths);
        if let Some(map) = self.read_map(&self.key(scope, &current)) {
            return Some(Cached { map, freshness: Freshness::Fresh });
        }
        let map = self.read_map(&binding.key)?;
        let (changed, total) = Repo::changed(&binding.manifest, &current);
        Some(Cached { map, freshness: Freshness::Stale { since: binding.commit, changed, total } })
    }

    /// `fresh`, `stale` or `None` (never explored), without reading the map.
    pub fn state(&self, repo: &Repo, scope: &str) -> Option<&'static str> {
        let binding = self.binding(scope)?;
        let key = self.key(scope, &repo.manifest(&binding.paths));
        Some(if self.map_file(&key).is_file() { "fresh" } else { "stale" })
    }

    /// Store a map built from the files in `manifest` (taken when exploration started).
    pub fn store(
        &self,
        scope: &str,
        paths: &[String],
        manifest: Manifest,
        commit: Option<String>,
        session: Option<String>,
        map: &Map,
    ) -> io::Result<()> {
        let key = self.key(scope, &manifest);
        write_atomic(&self.map_file(&key), &serde_json::to_vec(map)?)?;
        let binding = Binding {
            scope: scope.to_owned(),
            paths: paths.to_vec(),
            key,
            commit,
            manifest,
            session,
            engine: Some(self.engine.to_owned()),
        };
        write_atomic(&self.binding_file(scope), &serde_json::to_vec(&binding)?)
    }

    /// The session behind the scope's latest map, if this engine made it.
    pub fn session(&self, scope: &str) -> Option<String> {
        let binding = self.binding(scope)?;
        (binding.engine.as_deref().unwrap_or("codex") == self.engine).then_some(binding.session).flatten()
    }

    fn sessions_file(&self) -> PathBuf {
        match self.engine {
            "codex" => self.dir.join("sessions.json"),
            other => self.dir.join(format!("sessions-{other}.json")),
        }
    }

    /// Every session of this engine cxplr created for this project (id → name).
    pub fn sessions(&self) -> BTreeMap<String, String> {
        fs::read(self.sessions_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn record_session(&self, id: &str, name: &str) -> io::Result<()> {
        let _guard = self.sessions_lock.lock().unwrap();
        let mut all = self.sessions();
        all.insert(id.to_owned(), name.to_owned());
        write_atomic(&self.sessions_file(), &serde_json::to_vec_pretty(&all)?)
    }

    /// Forget deleted sessions: drop them from the registry and from every map binding.
    pub fn forget_sessions(&self, deleted: &[String]) -> io::Result<()> {
        let _guard = self.sessions_lock.lock().unwrap();
        let mut all = self.sessions();
        all.retain(|id, _| !deleted.contains(id));
        write_atomic(&self.sessions_file(), &serde_json::to_vec_pretty(&all)?)?;
        for entry in fs::read_dir(self.dir.join("scopes"))?.filter_map(Result::ok) {
            let Ok(bytes) = fs::read(entry.path()) else { continue };
            let Ok(mut b) = serde_json::from_slice::<Binding>(&bytes) else { continue };
            if b.session.as_ref().is_some_and(|s| deleted.contains(s)) {
                b.session = None;
                write_atomic(&entry.path(), &serde_json::to_vec(&b)?)?;
            }
        }
        Ok(())
    }

    /// Key for the example traces of `flow` in `scope`, if the scope has a map.
    pub fn trace_key(&self, repo: &Repo, scope: &str, flow: &str) -> Option<String> {
        let binding = self.binding(scope)?;
        let key = self.key(scope, &repo.manifest(&binding.paths));
        Some(digest(&format!("{key}\ntrace\n{flow}"))[..32].to_owned())
    }

    pub fn load_trace(&self, key: &str) -> Option<crate::trace::Traces> {
        serde_json::from_slice(&fs::read(self.dir.join("traces").join(format!("{key}.json"))).ok()?).ok()
    }

    pub fn store_trace(&self, key: &str, traces: &crate::trace::Traces) -> io::Result<()> {
        write_atomic(&self.dir.join("traces").join(format!("{key}.json")), &serde_json::to_vec_pretty(traces)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(title: &str) -> Map {
        Map {
            title: title.into(),
            summary: String::new(),
            nodes: vec![],
            edges: vec![],
            flows: vec![],
            diagnostics: vec![],
            meta: None,
        }
    }

    #[test]
    fn maps_stay_fresh_until_their_own_content_changes_then_serve_stale() {
        let dir = std::env::temp_dir().join(format!("cxplr-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a")).unwrap();
        fs::create_dir_all(dir.join("b")).unwrap();
        fs::write(dir.join("a/x.txt"), "one\n").unwrap();
        fs::write(dir.join("b/y.txt"), "two\n").unwrap();
        let repo = Repo::open(&dir).unwrap();
        let cache = Cache::open(dir.join(".cxplr"), Kind::Codex, None).unwrap();
        assert_eq!(fs::read_to_string(dir.join(".cxplr/.gitignore")).unwrap().lines().last(), Some("*"));

        let a = vec!["a".to_owned()];
        let b = vec!["b".to_owned()];
        assert!(cache.load(&repo, "a").is_none());
        cache.store("a", &a, repo.manifest(&a), None, Some("session-a".into()), &map("A")).unwrap();
        cache.store("b", &b, repo.manifest(&b), None, None, &map("B")).unwrap();
        assert_eq!(cache.session("a").as_deref(), Some("session-a"));
        assert_eq!(cache.load(&repo, "a").unwrap().freshness, Freshness::Fresh);

        // Changing `b` leaves `a` fresh; `b` is served from its old map, marked stale
        // with the exact file that changed.
        fs::write(dir.join("b/y.txt"), "two, edited\n").unwrap();
        assert_eq!(cache.state(&repo, "a"), Some("fresh"));
        let stale = cache.load(&repo, "b").unwrap();
        assert_eq!(stale.map.title, "B");
        assert_eq!(stale.freshness, Freshness::Stale { since: None, changed: vec!["b/y.txt".into()], total: 1 });
        assert_eq!(cache.state(&repo, "b"), Some("stale"));

        // Restoring the content makes the old map fresh again.
        fs::write(dir.join("b/y.txt"), "two\n").unwrap();
        assert_eq!(cache.state(&repo, "b"), Some("fresh"));

        // The same scope at a different model is a different key.
        let other = Cache::open(dir.join(".cxplr"), Kind::Codex, Some("another-model")).unwrap();
        assert_eq!(other.state(&repo, "a"), Some("stale"));

        // Another engine has its own keys and never gets a Codex session to fork.
        let claude = Cache::open(dir.join(".cxplr"), Kind::Claude, None).unwrap();
        assert_eq!(claude.state(&repo, "a"), Some("stale"));
        assert_eq!(claude.session("a"), None);
        cache.record_session("session-a", "A").unwrap();
        assert!(claude.sessions().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
