//! The explored directory: identity, git state and content manifests used as
//! cache keys.

use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Project-local cache directory; never part of a map's content.
pub const CACHE_DIR_NAME: &str = ".cxplr";

/// Directories that hold tool state rather than project content: git, cxplr's cache,
/// and what Metals, Bloop, sbt/BSP and Scala CLI create while a Scala language server
/// runs. They are never part of a map's content, searches or directory listings, so a
/// language server starting does not make maps stale.
const TOOL_DIRS: &[&str] = &[".git", CACHE_DIR_NAME, ".metals", ".bloop", ".bsp", ".scala-build"];

pub fn is_tool_dir(name: &std::ffi::OsStr) -> bool {
    TOOL_DIRS.iter().any(|d| name == *d)
}

const MAX_CHANGED: usize = 50;

/// Repository-relative file path → git blob hash of its current working-tree content.
pub type Manifest = BTreeMap<String, String>;

/// Which files are a project's content for indexing (`--file-types`,
/// `--skip-file-types`). Entries are extensions (`rs`) or whole file names
/// (`Dockerfile`), lowercase. Kept per project in `<cache>/files.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFilter {
    /// Only these types; empty: every type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub only: Vec<String>,
    /// Never these types.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip: Vec<String>,
}

impl FileFilter {
    const FILE: &'static str = "files.json";

    /// `rs, .toml Dockerfile` → `["rs", "toml", "dockerfile"]`.
    pub fn parse_list(list: &str) -> Vec<String> {
        let mut out: Vec<String> = list
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(|t| t.trim().trim_start_matches("*.").trim_start_matches('.').to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    pub fn is_empty(&self) -> bool {
        self.only.is_empty() && self.skip.is_empty()
    }

    /// Whether a repository-relative file counts as content.
    pub fn allows(&self, path: &str) -> bool {
        if self.is_empty() {
            return true;
        }
        let name = path.rsplit('/').next().unwrap_or(path).to_lowercase();
        let ext = Path::new(&name).extension().and_then(|e| e.to_str()).map(str::to_owned);
        let listed = |types: &[String]| types.iter().any(|t| *t == name || ext.as_deref() == Some(t.as_str()));
        (self.only.is_empty() || listed(&self.only)) && !listed(&self.skip)
    }

    /// `only rs, toml · skipping md, txt`
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.only.is_empty() {
            parts.push(format!("only {}", self.only.join(", ")));
        }
        if !self.skip.is_empty() {
            parts.push(format!("skipping {}", self.skip.join(", ")));
        }
        if parts.is_empty() { "all file types".into() } else { parts.join(" · ") }
    }

    /// The project's saved filter in `cache_dir` (none: every type).
    pub fn load(cache_dir: &Path) -> Result<FileFilter, String> {
        let path = cache_dir.join(Self::FILE);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileFilter::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Save it for the project; an empty filter removes the file.
    pub fn save(&self, cache_dir: &Path) -> Result<(), String> {
        let path = cache_dir.join(Self::FILE);
        let result = if self.is_empty() {
            match fs::remove_file(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            }
        } else {
            fs::create_dir_all(cache_dir).and_then(|()| fs::write(&path, serde_json::to_vec_pretty(self).expect("serializable")))
        };
        result.map_err(|e| format!("{}: {e}", path.display()))
    }
}

pub struct Repo {
    /// Canonical root directory.
    pub root: PathBuf,
    pub name: String,
    /// Files that count as content: manifests (cache keys, staleness) and line counts.
    pub filter: FileFilter,
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
        Ok(Repo { root, name, filter: FileFilter::default(), git })
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
    /// the blob hash of its current content, limited to the file-type filter. Only
    /// content matters: the same files give the same manifest whether they are
    /// committed, staged or uncommitted. Inside git, clean tracked files reuse the
    /// index hashes and only modified and untracked files are read; elsewhere every
    /// file is hashed.
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
                    self.filter.allows(&rel).then_some(())?;
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
        manifest.retain(|p, _| !p.split('/').any(|part| TOOL_DIRS.contains(&part)) && self.filter.allows(p));
        manifest
    }

    /// Files added, removed or changed between two manifests, capped, plus the total.
    pub fn changed(old: &Manifest, new: &Manifest) -> (Vec<String>, usize) {
        let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
        let changed: Vec<String> = keys.into_iter().filter(|k| old.get(*k) != new.get(*k)).cloned().collect();
        let total = changed.len();
        (changed.into_iter().take(MAX_CHANGED).collect(), total)
    }

    /// Text lines of the files under `paths` that pass the filter, counting stops once
    /// `limit` is reached.
    pub fn line_count(&self, paths: &[String], limit: usize) -> usize {
        let mut total = 0;
        for path in paths {
            for entry in self.walk(path) {
                let rel = entry.path().strip_prefix(&self.root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                if !self.filter.allows(&rel) {
                    continue;
                }
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
    /// tool directories and files over 1 MiB.
    fn walk(&self, path: &str) -> impl Iterator<Item = ignore::DirEntry> {
        ignore::WalkBuilder::new(self.root.join(path))
            .hidden(false)
            .filter_entry(|e| !is_tool_dir(e.file_name()))
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
            let dir = std::env::temp_dir().join(format!("cxplr-repo-{tag}-{}", std::process::id()));
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
    fn tool_directories_are_never_part_of_the_content() {
        let dir = TempGit::new("cachedir");
        let repo = Repo::open(&dir.0).unwrap();
        let all = vec![".".to_owned()];
        let before = repo.manifest(&all);
        for (d, file) in [(CACHE_DIR_NAME, "map.json"), (".metals", "metals.log"), ("sub/.bloop", "sub.json")] {
            fs::create_dir_all(dir.0.join(d)).unwrap();
            fs::write(dir.0.join(d).join(file), "{}").unwrap();
        }
        assert_eq!(repo.manifest(&all), before);
    }

    #[test]
    fn file_filters_match_extensions_and_names_and_shape_the_manifest() {
        let only = FileFilter { only: FileFilter::parse_list("rs, .toml *.RS Dockerfile"), skip: vec![] };
        assert_eq!(only.only, ["dockerfile", "rs", "toml"]);
        assert!(only.allows("src/main.rs") && only.allows("Cargo.toml") && only.allows("deploy/Dockerfile"));
        assert!(!only.allows("README.md") && !only.allows("notes.txt") && !only.allows(".gitignore"));
        let skip = FileFilter { only: vec![], skip: FileFilter::parse_list("md,txt") };
        assert!(skip.allows("src/main.rs") && skip.allows("LICENSE") && !skip.allows("docs/Guide.MD"));
        assert_eq!(skip.describe(), "skipping md, txt");

        let dir = TempGit::new("filter");
        fs::write(dir.0.join("a/README.md"), "# a\n".repeat(500)).unwrap();
        dir.commit_all();
        let mut repo = Repo::open(&dir.0).unwrap();
        let a = vec!["a".to_owned()];
        assert_eq!(repo.manifest(&a).len(), 2);
        repo.filter = skip;
        assert_eq!(repo.manifest(&a).keys().collect::<Vec<_>>(), ["a/x.rs"]);
        // Lines of skipped files do not count towards a component's size.
        assert_eq!(repo.line_count(&a, 1000), 1);
        // Edits to skipped files leave the manifest (and so the map's key) alone.
        fs::write(dir.0.join("a/README.md"), "changed\n").unwrap();
        assert_eq!(repo.manifest(&a).keys().collect::<Vec<_>>(), ["a/x.rs"]);
    }
}
