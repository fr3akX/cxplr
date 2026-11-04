//! Maps are expensive to produce, so each one is stored per repository, commit
//! and scope. Re-exploring overwrites the entry.

use crate::map::Map;
use crate::repo::Repo;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::{env, fs, io};

pub fn default_base() -> PathBuf {
    let base = env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(env::temp_dir);
    base.join("code-explore")
}

fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn new(base: &Path, repo: &Repo) -> io::Result<Cache> {
        let label: String = repo
            .name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        let repo_key = format!("{label}-{}", &digest(&repo.root.to_string_lossy())[..12]);
        let dir = base.join(repo_key).join(repo.commit.as_deref().unwrap_or("no-commit"));
        fs::create_dir_all(&dir)?;
        Ok(Cache { dir })
    }

    fn file(&self, scope: &str) -> PathBuf {
        if scope.is_empty() {
            self.dir.join("system.json")
        } else {
            self.dir.join(format!("scope-{}.json", &digest(scope)[..16]))
        }
    }

    pub fn load(&self, scope: &str) -> Option<Map> {
        serde_json::from_slice(&fs::read(self.file(scope)).ok()?).ok()
    }

    pub fn store(&self, scope: &str, map: &Map) -> io::Result<()> {
        let file = self.file(scope);
        let tmp = file.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(map)?)?;
        fs::rename(tmp, file)
    }
}
