//! Questions about the code, answered by the agent with the developer's
//! current view (map, component, selected source lines) as context. The first
//! question of a conversation forks the viewed map's session; follow-ups continue
//! in that session, so earlier answers are real context, not pasted text.

use crate::map::{Evidence, Map};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt::Write;

const MAX_SELECTION_LINES: usize = 400;

#[derive(Deserialize, Default)]
pub struct Context {
    /// Map scope the developer is viewing; empty for the system map.
    #[serde(default)]
    pub scope: String,
    /// Selected node in that map.
    pub node: Option<String>,
    /// Repository-relative file, optionally with a 1-based inclusive line range.
    pub path: Option<String>,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
}

#[derive(Deserialize)]
pub struct Request {
    pub question: String,
    #[serde(default)]
    pub context: Context,
    /// Conversation to continue (returned with the previous answer).
    #[serde(default)]
    pub session: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Answer {
    /// Markdown.
    pub answer: String,
    pub citations: Vec<Evidence>,
}

pub fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["answer", "citations"],
        "properties": {
            "answer": { "type": "string" },
            "citations": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["path", "line", "note"],
                    "properties": {
                        "path": { "type": "string" },
                        "line": { "type": "integer" },
                        "note": { "type": "string" }
                    }
                }
            }
        }
    })
}


/// What the developer is looking at, resolved by the server from the cache and disk.
pub struct View<'a> {
    pub crumbs: Vec<String>,
    pub map: Option<&'a Map>,
    pub node: Option<&'a crate::map::Node>,
    /// (path, first line number, the selected text)
    pub selection: Option<(String, usize, String)>,
}

/// A question with what the developer is looking at. Follow-ups skip the
/// answering instructions, which are already in the session.
pub fn prompt(repo_name: &str, view: &View, question: &str, follow_up: bool) -> String {
    let mut p = if follow_up {
        String::from("Follow-up question; answer as before (Markdown, `path:line` locations, citations).\n")
    } else {
        format!(
            "You are helping a developer understand the repository `{repo_name}` in the current \
working directory. Investigate read-only (shell commands such as rg, sed -n, nl -ba and git \
log, or file read and search tools). Do not modify files, install packages, or run the \
project's code, builds or tests.

Answer in GitHub-flavoured Markdown: concise, concrete, naming files, functions and \
configuration keys. Use fenced code blocks for code. When you refer to code, write the \
location as `path:line` (repository-relative, 1-based, taken from tool output that shows \
line numbers; never estimate). Also list the most important locations in `citations`. Say so \
when something is inferred rather than seen, or when the repository does not answer the \
question.

Return ONLY the JSON object required by the output schema.
"
        )
    };

    p.push_str("\nWhat the developer is looking at:\n");
    if !view.crumbs.is_empty() {
        let _ = writeln!(p, "- Map: {}", view.crumbs.join(" › "));
    }
    if let Some(map) = view.map {
        let _ = writeln!(p, "  {}: {}", map.title, map.summary);
        let parts: Vec<String> = map.nodes.iter().map(|n| format!("{} ({})", n.name, n.kind)).collect();
        let _ = writeln!(p, "  Parts on this map: {}", parts.join(", "));
    }
    if let Some(node) = view.node {
        let _ = writeln!(p, "- Selected: {} ({}). {}", node.name, node.kind, node.summary);
        if !node.paths.is_empty() {
            let _ = writeln!(p, "  Paths: {}", node.paths.join(", "));
        }
    }
    if let Some((path, first, text)) = &view.selection {
        let count = text.lines().count();
        let _ = writeln!(p, "- Source selection: {path}:{first}-{}\n```", first + count.saturating_sub(1));
        for (i, line) in text.lines().enumerate() {
            let _ = writeln!(p, "{:>6}\t{line}", first + i);
        }
        p.push_str("```\n");
    }

    let _ = write!(p, "\nQuestion: {}\n", question.trim());
    p
}

/// The selected lines of a file, capped to keep the prompt bounded.
pub fn selection(text: &str, start: Option<usize>, end: Option<usize>) -> (usize, String) {
    let total = text.lines().count().max(1);
    let first = start.unwrap_or(1).clamp(1, total);
    let last = end.unwrap_or(first + MAX_SELECTION_LINES - 1).clamp(first, total).min(first + MAX_SELECTION_LINES - 1);
    let picked: Vec<&str> = text.lines().skip(first - 1).take(last - first + 1).collect();
    (first, picked.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::selection;

    #[test]
    fn selections_are_clamped_to_the_file_and_capped() {
        let text: String = (1..=1000).map(|i| format!("l{i}\n")).collect();
        assert_eq!(selection(&text, Some(3), Some(4)), (3, "l3\nl4".to_owned()));
        assert_eq!(selection(&text, Some(5), Some(2)).1, "l5");
        assert_eq!(selection(&text, Some(990), Some(5000)).1.lines().count(), 11);
        assert_eq!(selection(&text, Some(1), None).1.lines().count(), 400);
        assert_eq!(selection("only", Some(9), Some(12)), (1, "only".to_owned()));
    }
}
