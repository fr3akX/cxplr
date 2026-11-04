//! Project-local, content-addressed cache in `<repo>/.code-xplr/` (it ignores itself
//! in git).
//!
//! - `maps/<key>.json`: one map. The key covers the cache format, prompt version,
//!   model, scope and the content of every file under the paths the map describes,
//!   so a component keeps its map as long as its own files are unchanged, however
//!   they are committed.
//! - `scopes/<scope hash>.json`: the latest map of a scope, its paths and the file
//!   manifest it was built from. When the files change, the latest map is still
//!   served, marked stale with exactly which files changed, until it is refreshed.
//! - `traces/<key>.json`: example inputs for one flow, keyed by the scope's content
//!   key and the flow name.

use crate::map::Map;
use crate::prompt;
use crate::repo::{self, Manifest, Repo};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const FORMAT: &str = "code-xplr/2";

#[derive(Serialize, Deserialize)]
struct Binding {
    scope: String,
    paths: Vec<String>,
    key: String,
    commit: Option<String>,
    manifest: Manifest,
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
    salt: String,
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
    pub fn open(dir: PathBuf, model: Option<&str>) -> io::Result<Cache> {
        fs::create_dir_all(dir.join("maps"))?;
        fs::create_dir_all(dir.join("scopes"))?;
        fs::create_dir_all(dir.join("traces"))?;
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            fs::write(ignore, "# code-explore cache: local only\n*\n")?;
        }
        let salt = format!("{FORMAT}|prompt {}|model {}", prompt::VERSION, model.unwrap_or("default"));
        Ok(Cache { dir, salt })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
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
    pub fn store(&self, scope: &str, paths: &[String], manifest: Manifest, commit: Option<String>, map: &Map) -> io::Result<()> {
        let key = self.key(scope, &manifest);
        write_atomic(&self.map_file(&key), &serde_json::to_vec(map)?)?;
        let binding = Binding { scope: scope.to_owned(), paths: paths.to_vec(), key, commit, manifest };
        write_atomic(&self.binding_file(scope), &serde_json::to_vec(&binding)?)
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
        let dir = std::env::temp_dir().join(format!("code-explore-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a")).unwrap();
        fs::create_dir_all(dir.join("b")).unwrap();
        fs::write(dir.join("a/x.txt"), "one\n").unwrap();
        fs::write(dir.join("b/y.txt"), "two\n").unwrap();
        let repo = Repo::open(&dir).unwrap();
        let cache = Cache::open(dir.join(".code-xplr"), None).unwrap();
        assert_eq!(fs::read_to_string(dir.join(".code-xplr/.gitignore")).unwrap().lines().last(), Some("*"));

        let a = vec!["a".to_owned()];
        let b = vec!["b".to_owned()];
        assert!(cache.load(&repo, "a").is_none());
        cache.store("a", &a, repo.manifest(&a), None, &map("A")).unwrap();
        cache.store("b", &b, repo.manifest(&b), None, &map("B")).unwrap();
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
        let other = Cache::open(dir.join(".code-xplr"), Some("another-model")).unwrap();
        assert_eq!(other.state(&repo, "a"), Some("stale"));
        let _ = fs::remove_dir_all(&dir);
    }
}
