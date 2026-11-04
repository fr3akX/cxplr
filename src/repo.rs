//! The explored directory: identity, git state and content manifests used as
//! cache keys.

use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Project-local cache directory; never part of a map's content.
pub const CACHE_DIR_NAME: &str = ".code-xplr";
const MAX_CHANGED: usize = 50;

/// Repository-relative file path → git blob hash of its current working-tree content.
pub type Manifest = BTreeMap<String, String>;

pub struct Repo {
    /// Canonical root directory.
    pub root: PathBuf,
    pub name: String,
    git: bool,
}

/// Git's blob id for file content, so tracked and uncommitted files hash alike.
fn blob_hash(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", bytes.len()).as_bytes());
    h.update(bytes);
    format!("{:x}", h.finalize())
}

impl Repo {
    pub fn open(path: &Path) -> Result<Repo, String> {
        let root = path.canonicalize().map_err(|e| format!("{}: {e}", path.display()))?;
        if !root.is_dir() {
            return Err(format!("{} is not a directory", root.display()));
        }
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string());
        let git = git(&root, &["rev-parse", "HEAD"]).is_some();
        Ok(Repo { root, name, git })
    }

    /// Current `HEAD`, read live so commits made while serving are seen.
    pub fn head(&self) -> Option<String> {
        self.git.then(|| git(&self.root, &["rev-parse", "HEAD"])).flatten()
    }

    /// Uncommitted changes anywhere under the explored directory.
    pub fn is_dirty(&self) -> bool {
        self.git && git(&self.root, &["status", "--porcelain", "--", "."]).is_some_and(|s| !s.is_empty())
    }

    /// Every file under `paths` (repository-relative; `.` is the whole directory) with
    /// the blob hash of its current content. Only content matters: the same files
    /// give the same manifest whether they are committed, staged or uncommitted.
    /// Inside git, clean tracked files reuse the index hashes and only modified and
    /// untracked files are read; elsewhere every file is hashed.
    pub fn manifest(&self, paths: &[String]) -> Manifest {
        let mut specs: Vec<&str> = paths.iter().map(String::as_str).collect();
        specs.sort_unstable();
        specs.dedup();
        if !self.git {
            return specs
                .iter()
                .flat_map(|p| self.walk(p))
                .filter_map(|e| {
                    let rel = e.path().strip_prefix(&self.root).ok()?.to_string_lossy().into_owned();
                    Some((rel, blob_hash(&fs::read(e.path()).ok()?)))
                })
                .collect();
        }
        let listed = |args: &[&str]| -> Vec<String> {
            let mut all = args.to_vec();
            all.push("--");
            all.extend(&specs);
            git(&self.root, &all)
                .unwrap_or_default()
                .split('\0')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let mut manifest = Manifest::new();
        for entry in listed(&["ls-files", "-s", "-z"]) {
            // `<mode> <object> <stage>\t<path>`
            if let Some((meta, path)) = entry.split_once('\t') {
                if let Some(object) = meta.split(' ').nth(1) {
                    manifest.insert(path.to_owned(), object.to_owned());
                }
            }
        }
        let modified = listed(&["diff", "--name-only", "--relative", "-z"]);
        let untracked = listed(&["ls-files", "--others", "--exclude-standard", "-z"]);
        for path in modified.into_iter().chain(untracked) {
            match fs::read(self.root.join(&path)) {
                Ok(bytes) => manifest.insert(path, blob_hash(&bytes)),
                Err(_) => manifest.remove(&path), // deleted in the working tree
            };
        }
        manifest.retain(|p, _| !p.starts_with(CACHE_DIR_NAME));
        manifest
    }

    /// Files added, removed or changed between two manifests, capped, plus the total.
    pub fn changed(old: &Manifest, new: &Manifest) -> (Vec<String>, usize) {
        let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
        let changed: Vec<String> = keys.into_iter().filter(|k| old.get(*k) != new.get(*k)).cloned().collect();
        let total = changed.len();
        (changed.into_iter().take(MAX_CHANGED).collect(), total)
    }

    /// Text lines under `paths`, counting stops once `limit` is reached.
    pub fn line_count(&self, paths: &[String], limit: usize) -> usize {
        let mut total = 0;
        for path in paths {
            for entry in self.walk(path) {
                let Ok(bytes) = fs::read(entry.path()) else { continue };
                if bytes.iter().take(8192).any(|&b| b == 0) {
                    continue;
                }
                total += bytes.iter().filter(|&&b| b == b'\n').count();
                if total >= limit {
                    return total;
                }
            }
        }
        total
    }

    /// Files under a repository-relative path, respecting ignore files and skipping
    /// `.git`, the cache directory and files over 1 MiB.
    fn walk(&self, path: &str) -> impl Iterator<Item = ignore::DirEntry> {
        ignore::WalkBuilder::new(self.root.join(path))
            .hidden(false)
            .filter_entry(|e| e.file_name() != ".git" && e.file_name() != CACHE_DIR_NAME)
            .build()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .filter(|e| e.metadata().is_ok_and(|m| m.len() <= 1024 * 1024))
    }
}

/// Stable digest of a manifest.
pub fn fingerprint(manifest: &Manifest) -> String {
    let mut h = Sha256::new();
    for (path, object) in manifest {
        h.update(path.as_bytes());
        h.update([0]);
        h.update(object.as_bytes());
        h.update([b'\n']);
    }
    format!("{:x}", h.finalize())
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempGit(PathBuf);

    impl TempGit {
        fn new(tag: &str) -> TempGit {
            let dir = std::env::temp_dir().join(format!("code-explore-repo-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("a")).unwrap();
            fs::create_dir_all(dir.join("b")).unwrap();
            fs::write(dir.join("a/x.rs"), "fn x() {}\n").unwrap();
            fs::write(dir.join("b/y.rs"), "fn y() {}\n").unwrap();
            let repo = TempGit(dir.canonicalize().unwrap());
            repo.git(&["init", "-q"]);
            repo.commit_all();
            repo
        }

        fn git(&self, args: &[&str]) {
            let out = Command::new("git")
                .arg("-C")
                .arg(&self.0)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        }

        fn commit_all(&self) {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "--allow-empty", "-m", "change"]);
        }
    }

    impl Drop for TempGit {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn manifests_track_content_not_how_it_is_committed() {
        let dir = TempGit::new("manifest");
        let repo = Repo::open(&dir.0).unwrap();
        let (a, all) = (vec!["a".to_owned()], vec![".".to_owned()]);
        let a0 = repo.manifest(&a);
        let all0 = repo.manifest(&all);
        assert_eq!(a0.keys().collect::<Vec<_>>(), ["a/x.rs"]);

        // An uncommitted edit in `b` changes the whole tree but not `a`.
        fs::write(dir.0.join("b/y.rs"), "fn y() { 1 }\n").unwrap();
        assert_eq!(repo.manifest(&a), a0);
        let dirty = repo.manifest(&all);
        assert_eq!(Repo::changed(&all0, &dirty), (vec!["b/y.rs".to_owned()], 1));

        // Committing that same content keeps the manifest identical.
        dir.commit_all();
        assert_eq!(repo.manifest(&all), dirty);

        // Untracked and deleted files count as changes.
        fs::write(dir.0.join("a/new.rs"), "fn n() {}\n").unwrap();
        fs::remove_file(dir.0.join("a/x.rs")).unwrap();
        let a1 = repo.manifest(&a);
        assert_eq!(a1.keys().collect::<Vec<_>>(), ["a/new.rs"]);
        assert_eq!(Repo::changed(&a0, &a1).1, 2);
    }

    #[test]
    fn uncommitted_files_hash_like_git_blobs() {
        let dir = TempGit::new("blob");
        let repo = Repo::open(&dir.0).unwrap();
        let committed = repo.manifest(&["a".to_owned()])["a/x.rs"].clone();
        assert_eq!(blob_hash(b"fn x() {}\n"), committed);
    }

    #[test]
    fn cache_directory_is_never_part_of_the_content() {
        let dir = TempGit::new("cachedir");
        let repo = Repo::open(&dir.0).unwrap();
        let all = vec![".".to_owned()];
        let before = repo.manifest(&all);
        fs::create_dir_all(dir.0.join(CACHE_DIR_NAME)).unwrap();
        fs::write(dir.0.join(CACHE_DIR_NAME).join("map.json"), "{}").unwrap();
        assert_eq!(repo.manifest(&all), before);
    }
}
