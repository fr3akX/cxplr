use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Repo {
    /// Canonical root directory.
    pub root: PathBuf,
    pub name: String,
    /// `HEAD` at startup; maps are cached per commit.
    pub commit: Option<String>,
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
        let commit = git(&root, &["rev-parse", "HEAD"]);
        Ok(Repo { root, name, commit })
    }

    /// Uncommitted changes under the explored directory, checked when a map is generated.
    pub fn is_dirty(&self) -> bool {
        self.commit.is_some()
            && git(&self.root, &["status", "--porcelain", "--", "."]).is_some_and(|s| !s.is_empty())
    }
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
