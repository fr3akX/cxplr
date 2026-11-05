//! Path confinement and citation checking. The agent's claims are only as
//! trustworthy as the `file:line` evidence behind them, so every citation is
//! checked against the actual repository before the map is shown.

use crate::map::{Evidence, Map};
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Resolve a repository-relative (or absolute, inside `root`) path. `root` must be
/// canonical. Returns the canonical path and its repository-relative display form.
/// Symlinks that leave the repository are rejected.
pub fn resolve(root: &Path, raw: &str) -> Result<(PathBuf, String), String> {
    let trimmed = raw.trim();
    let trimmed = trimmed.strip_prefix("./").unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Err("empty path".into());
    }
    let given = Path::new(trimmed);
    let rel = if given.is_absolute() {
        given.strip_prefix(root).map_err(|_| "outside the repository".to_string())?
    } else {
        given
    };
    if rel.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        return Err("outside the repository".into());
    }
    let full = root.join(rel).canonicalize().map_err(|_| "does not exist".to_string())?;
    let shown = full
        .strip_prefix(root)
        .map_err(|_| "outside the repository".to_string())?
        .to_string_lossy()
        .into_owned();
    Ok((full, if shown.is_empty() { ".".into() } else { shown }))
}

fn count_lines(bytes: &[u8]) -> usize {
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    newlines + usize::from(bytes.last().is_some_and(|&b| b != b'\n'))
}

/// A file's repository-relative path and its number of lines.
pub fn file_lines(root: &Path, raw: &str) -> Result<(String, usize), String> {
    let (full, shown) = resolve(root, raw)?;
    if !full.is_file() {
        return Err("not a readable file".into());
    }
    let bytes = fs::read(&full).map_err(|_| "not a readable file".to_string())?;
    Ok((shown, count_lines(&bytes)))
}

struct Verifier<'a> {
    root: &'a Path,
    lines: HashMap<PathBuf, Option<usize>>,
}

impl Verifier<'_> {
    fn check(&mut self, ev: &mut Evidence) {
        ev.verified = false;
        ev.problem = None;
        let (full, shown) = match resolve(self.root, &ev.path) {
            Ok(found) => found,
            Err(problem) => {
                ev.problem = Some(problem);
                return;
            }
        };
        ev.path = shown;
        let total = *self
            .lines
            .entry(full)
            .or_insert_with_key(|p| p.is_file().then(|| fs::read(p).ok().map(|b| count_lines(&b))).flatten());
        match total {
            None => ev.problem = Some("not a readable file".into()),
            Some(_) if ev.line < 1 => ev.problem = Some("line numbers start at 1".into()),
            Some(n) if ev.line as usize > n => ev.problem = Some(format!("file has only {n} lines")),
            Some(_) => ev.verified = true,
        }
    }
}

/// Mark each citation verified or not (answers to questions).
pub fn verify_citations(root: &Path, citations: &mut [Evidence]) {
    let mut verifier = Verifier { root, lines: HashMap::new() };
    for ev in citations {
        verifier.check(ev);
    }
}

/// Normalize node paths and mark every citation verified or not.
pub fn verify(root: &Path, map: &mut Map) {
    let mut verifier = Verifier { root, lines: HashMap::new() };
    let Map { nodes, edges, diagnostics, .. } = map;
    for node in nodes.iter_mut() {
        let mut kept: Vec<String> = Vec::with_capacity(node.paths.len());
        for raw in &node.paths {
            match resolve(root, raw) {
                Ok((_, shown)) if !kept.contains(&shown) => kept.push(shown),
                Ok(_) => {}
                Err(problem) => diagnostics.push(format!("node `{}`: path `{raw}` {problem}", node.id)),
            }
        }
        node.paths = kept;
        if node.paths.is_empty() {
            node.explorable = false;
        }
        for ev in &mut node.evidence {
            verifier.check(ev);
        }
    }
    for edge in edges.iter_mut() {
        for ev in &mut edge.evidence {
            verifier.check(ev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{Edge, Node};

    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new(tag: &str) -> TempRepo {
            let dir = std::env::temp_dir().join(format!("cxplr-test-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::write(dir.join("src/main.rs"), "fn main() {\n    run();\n}").unwrap();
            TempRepo(dir.canonicalize().unwrap())
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn ev(path: &str, line: i64) -> Evidence {
        Evidence { path: path.into(), line, note: String::new(), verified: false, problem: None }
    }

    #[test]
    fn paths_are_confined_to_the_repository() {
        let repo = TempRepo::new("confine");
        let root = &repo.0;
        assert_eq!(resolve(root, "./src/main.rs").unwrap().1, "src/main.rs");
        let absolute = root.join("src/main.rs");
        assert_eq!(resolve(root, absolute.to_str().unwrap()).unwrap().1, "src/main.rs");
        assert_eq!(resolve(root, ".").unwrap().1, ".");
        assert!(resolve(root, "../etc/passwd").is_err());
        assert!(resolve(root, "src/../../etc/passwd").is_err());
        assert!(resolve(root, "/etc/passwd").is_err());
        assert!(resolve(root, "src/missing.rs").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_leaving_the_repository_are_rejected() {
        let repo = TempRepo::new("symlink");
        std::os::unix::fs::symlink("/etc", repo.0.join("escape")).unwrap();
        assert_eq!(resolve(&repo.0, "escape/hostname").unwrap_err(), "outside the repository");
    }

    #[test]
    fn citations_are_checked_against_real_line_counts() {
        let repo = TempRepo::new("lines");
        let mut map = Map {
            title: String::new(),
            summary: String::new(),
            nodes: vec![Node {
                id: "main".into(),
                name: "main".into(),
                kind: "module".into(),
                summary: String::new(),
                paths: vec!["src".into(), "./src".into(), "gone".into()],
                evidence: vec![ev("src/main.rs", 3), ev("src/main.rs", 4), ev("src", 1)],
                explorable: true,
            }],
            edges: vec![Edge {
                from: "main".into(),
                to: "main".into(),
                kind: "call".into(),
                label: String::new(),
                evidence: vec![ev("src/main.rs", 0)],
            }],
            flows: vec![],
            diagnostics: vec![],
            meta: None,
        };
        verify(&repo.0, &mut map);
        let node = &map.nodes[0];
        assert_eq!(node.paths, ["src"]);
        assert!(node.explorable);
        assert_eq!(map.diagnostics.len(), 1);
        assert!(node.evidence[0].verified, "last line without trailing newline counts");
        assert_eq!(node.evidence[1].problem.as_deref(), Some("file has only 3 lines"));
        assert_eq!(node.evidence[2].problem.as_deref(), Some("not a readable file"));
        assert_eq!(map.edges[0].evidence[0].problem.as_deref(), Some("line numbers start at 1"));
    }
}
