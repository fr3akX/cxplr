//! Whole-word text search across the repository: the navigation fallback for
//! files without a language server (YAML, shell, configs, …).

use serde::Serialize;
use std::fs;
use std::path::Path;

const MAX_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Serialize)]
pub struct Hit {
    pub path: String,
    /// 1-based.
    pub line: usize,
    pub preview: String,
}

#[derive(Serialize)]
pub struct Results {
    pub hits: Vec<Hit>,
    pub truncated: bool,
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Occurrences of `word` not embedded in a longer identifier.
fn contains_word(line: &str, word: &str) -> bool {
    let bytes = line.as_bytes();
    line.match_indices(word).any(|(i, m)| {
        let before = i.checked_sub(1).map(|j| bytes[j]);
        let after = bytes.get(i + m.len()).copied();
        !before.is_some_and(is_word_byte) && !after.is_some_and(is_word_byte)
    })
}

/// Search tracked-looking files (respecting .gitignore, skipping `.git`,
/// binaries and files over 1 MiB), stopping after `limit` hits.
pub fn search(root: &Path, word: &str, limit: usize) -> Results {
    let mut hits = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git" && e.file_name() != crate::repo::CACHE_DIR_NAME)
        .build();
    for entry in walker.filter_map(Result::ok) {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if entry.metadata().map_or(true, |m| m.len() > MAX_FILE_BYTES) {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path()) else { continue };
        if bytes.iter().take(8192).any(|&b| b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        if !text.contains(word) {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path()).to_string_lossy().into_owned();
        for (i, line) in text.lines().enumerate() {
            if contains_word(line, word) {
                if hits.len() == limit {
                    return Results { hits, truncated: true };
                }
                hits.push(Hit { path: rel.clone(), line: i + 1, preview: line.trim().chars().take(200).collect() });
            }
        }
    }
    hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    Results { hits, truncated: false }
}

#[cfg(test)]
mod tests {
    use super::contains_word;

    #[test]
    fn matches_whole_identifiers_only() {
        assert!(contains_word("let user = load(user_id);", "user"));
        assert!(contains_word("x.user)", "user"));
        assert!(!contains_word("let users = user_id;", "user"));
        assert!(!contains_word("$user", "user"));
        assert!(contains_word("user", "user"));
    }
}
