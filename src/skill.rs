//! The cxplr skill for Codex and Claude Code: tells any session working in a
//! repository with a cxplr atlas to orient from it before exploring. Both read
//! `skills/<name>/SKILL.md` with the same frontmatter from their config directory.

use crate::engine::Kind;
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

/// `$CODEX_HOME` (default `~/.codex`) or `$CLAUDE_CONFIG_DIR` (default `~/.claude`).
fn skill_path(engine: Kind) -> io::Result<PathBuf> {
    let (var, default) = match engine {
        Kind::Codex => ("CODEX_HOME", ".codex"),
        Kind::Claude | Kind::Anthropic => ("CLAUDE_CONFIG_DIR", ".claude"),
    };
    let home = env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(default)))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("neither {var} nor HOME is set")))?;
    Ok(home.join("skills").join("cxplr").join("SKILL.md"))
}

/// Write the skill to `<engine config dir>/skills/cxplr/SKILL.md`.
pub fn install(engine: Kind) -> io::Result<PathBuf> {
    let path = skill_path(engine)?;
    fs::create_dir_all(path.parent().expect("skill path has a parent"))?;
    fs::write(&path, SKILL)?;
    Ok(path)
}

/// Keep installed skills in step with this version of cxplr (e.g. after an update).
/// Returns the paths that were rewritten.
pub fn refresh_installed() -> Vec<PathBuf> {
    [Kind::Codex, Kind::Claude]
        .into_iter()
        .filter_map(|engine| {
            let path = skill_path(engine).ok()?;
            let current = fs::read_to_string(&path).ok()?;
            (current != SKILL).then(|| fs::write(&path, SKILL).ok().map(|()| path)).flatten()
        })
        .collect()
}
