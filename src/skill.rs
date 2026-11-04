//! The cxplr skill for Codex: tells any Codex session working in a repository
//! with a cxplr atlas to orient from it before exploring.

use std::path::PathBuf;
use std::{env, fs, io};

const SKILL: &str = r#"---
name: cxplr
description: Use when the repository contains `.cxplr/atlas/README.md`, a generated architecture map (cxplr). Read it to orient — components, their paths, interactions and data flows — before exploring the code, and check its citations and freshness.
---

# cxplr architecture atlas

The repository has been mapped by cxplr. `.cxplr/atlas/` holds one Markdown page per
explored part of the system, laid out like the architecture:
`README.md` (system), `<component>/README.md`, `<component>/<sub>/README.md`, ….

When the task needs an understanding of how the code is organised:

1. Read `.cxplr/atlas/README.md` first: what the system does, its components (with
   repository paths), how they interact, and the main data flows.
2. Follow the component links to the part relevant to the task instead of scanning
   the tree. "not explored" means there is no deeper page: explore that component's
   listed paths directly.
3. Citations are `path:line` from the repository root. Open the cited lines before
   relying on a claim; the atlas is model-generated and only checks that cited lines
   exist.
4. Check each page's **Status** line. A stale page describes an earlier version of
   the code and lists the files changed since; read those files directly, the page
   may be wrong about them.
5. The atlas is generated: never edit files under `.cxplr/`. The user refreshes it
   with `cxplr <repository> --index`.
"#;

fn skill_path() -> io::Result<PathBuf> {
    let home = env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "neither CODEX_HOME nor HOME is set"))?;
    Ok(home.join("skills").join("cxplr").join("SKILL.md"))
}

/// Write the skill to `$CODEX_HOME/skills/cxplr/SKILL.md` (default `~/.codex`).
pub fn install() -> io::Result<PathBuf> {
    let path = skill_path()?;
    fs::create_dir_all(path.parent().expect("skill path has a parent"))?;
    fs::write(&path, SKILL)?;
    Ok(path)
}

/// Keep an installed skill in step with this version of cxplr (e.g. after an update).
/// Returns the path when it was rewritten.
pub fn refresh_if_installed() -> Option<PathBuf> {
    let path = skill_path().ok()?;
    let current = fs::read_to_string(&path).ok()?;
    (current != SKILL).then(|| fs::write(&path, SKILL).ok().map(|()| path)).flatten()
}
