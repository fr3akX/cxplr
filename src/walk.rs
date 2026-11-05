//! Code walkthroughs: one example input followed through the code, statement by
//! statement, so the viewer can step forward and back through it. Each step is a
//! verified line range; the UI derives the map component from the step's path, so
//! the same walkthrough highlights the right box at every zoom level.

use crate::map::{Flow, Map};
use crate::trace::Scenario;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt::Write;
use std::path::Path;

/// Part of the cache key: bump when the prompt or schema changes.
pub const VERSION: &str = "1";
/// Longest range one step may highlight.
const MAX_SPAN: i64 = 60;
pub const KINDS: [&str; 7] = ["entry", "call", "branch", "transform", "io", "return", "error"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalkStep {
    pub path: String,
    pub line: i64,
    pub end_line: i64,
    pub kind: String,
    pub title: String,
    pub detail: String,
    /// Concrete values at this point for the example input.
    pub state: String,
    /// Set by the server: the file exists and the range is inside it.
    #[serde(default)]
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalkMeta {
    pub flow: String,
    pub scenario: String,
    pub input: String,
    pub generated_at: u64,
    pub duration_ms: u64,
    pub commands: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Walk {
    pub steps: Vec<WalkStep>,
    #[serde(default)]
    pub meta: Option<WalkMeta>,
}

pub fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["steps"],
        "properties": {
            "steps": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["path", "line", "end_line", "kind", "title", "detail", "state"],
                    "properties": {
                        "path": { "type": "string" },
                        "line": { "type": "integer" },
                        "end_line": { "type": "integer" },
                        "kind": { "type": "string", "enum": KINDS },
                        "title": { "type": "string" },
                        "detail": { "type": "string" },
                        "state": { "type": "string" }
                    }
                }
            }
        }
    })
}

pub fn prompt(map: &Map, flow: &Flow, scenario: &Scenario) -> String {
    let mut p = String::from(
        "You are helping a developer learn a repository by stepping through the code that one \
concrete input executes, like a debugger they move forward and back. The repository root is \
the current working directory. Investigate read-only (shell commands such as ls, rg, sed -n \
and nl -ba, or file read and search tools). Do not modify files, install packages, or run the \
project's code, builds or tests.

Return ONLY the JSON object required by the output schema.
",
    );
    let _ = writeln!(p, "\nArchitecture map: {}. {}", map.title, map.summary);
    p.push_str("Components:\n");
    for n in &map.nodes {
        let paths = if n.paths.is_empty() { String::new() } else { format!(" [{}]", n.paths.join(", ")) };
        let _ = writeln!(p, "- {} ({}){paths}", n.name, n.kind);
    }
    let _ = writeln!(p, "\nFlow: \"{}\". {}", flow.name, flow.description);
    let _ = writeln!(p, "Example input \"{}\": {}", scenario.name, scenario.input);
    let _ = writeln!(p, "Expected outcome: {}", scenario.outcome);
    p.push_str("Its hops between components (starting hints, with code they cited):\n");
    for (i, s) in scenario.steps.iter().enumerate() {
        let cites: Vec<String> = s.evidence.iter().map(|e| format!("{}:{}", e.path, e.line)).collect();
        let _ = writeln!(p, "{}. {} → {}: {} [{}]", i + 1, s.from, s.to, s.action, cites.join(", "));
    }
    p.push_str(
        "
Instructions:
- steps: 8-40 locations in the order this exact input executes them, from where it enters the \
repository to where its outcome is produced. Follow the input into the functions it calls, \
across files and components. Skip trivial plumbing; never include code this input does not run.
- One step per meaningful point: entry (where the input arrives), call (into another function \
or file), branch (a condition: say which way this input goes and why), transform (data is \
validated, parsed, mapped or computed), io (database, file, network, queue or another service; \
the path leaves the repository there, continue after it returns), return (a result goes back), \
error (an exception or rejection this input causes).
- path and line: repository-relative file and the 1-based first line of the code the step is \
about; end_line: its last line (at most 40 lines later). Take line numbers from tool output \
that shows them (`nl -ba`, `rg -n`, numbered file reads); never estimate them.
- title: what happens, at most 80 characters. detail: what the code does here for this input \
and why, at most 400 characters.
- state: the concrete values of the relevant variables or data at this point for this input, \
using names from the code (JSON-like, at most 300 characters); empty when nothing changes.
",
    );
    p
}

/// Check every step against the files: unknown files or lines are marked with a
/// problem, ranges are clamped to the file and to a readable span.
pub fn verify(root: &Path, walk: &mut Walk) {
    let mut files: HashMap<String, Result<(String, usize), String>> = HashMap::new();
    for step in &mut walk.steps {
        step.verified = false;
        step.problem = None;
        if !KINDS.contains(&step.kind.as_str()) {
            step.kind = "transform".into();
        }
        let found = files.entry(step.path.clone()).or_insert_with(|| crate::verify::file_lines(root, &step.path));
        let (shown, total) = match found {
            Ok((shown, total)) => (shown.clone(), *total as i64),
            Err(problem) => {
                step.problem = Some(problem.clone());
                continue;
            }
        };
        step.path = shown;
        if step.line < 1 {
            step.problem = Some("line numbers start at 1".into());
        } else if step.line > total {
            step.problem = Some(format!("file has only {total} lines"));
        } else {
            step.end_line = step.end_line.clamp(step.line, total.min(step.line + MAX_SPAN));
            step.verified = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn step(path: &str, line: i64, end_line: i64, kind: &str) -> WalkStep {
        WalkStep {
            path: path.into(),
            line,
            end_line,
            kind: kind.into(),
            title: String::new(),
            detail: String::new(),
            state: String::new(),
            verified: false,
            problem: None,
        }
    }

    #[test]
    fn steps_are_checked_and_ranges_clamped_to_the_file() {
        let dir = std::env::temp_dir().join(format!("cxplr-walk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/a.py"), (1..=100).map(|n| format!("line {n}\n")).collect::<String>()).unwrap();
        let root = dir.canonicalize().unwrap();
        let mut walk = Walk {
            steps: vec![
                step("./src/a.py", 10, 12, "call"),
                step("src/a.py", 95, 140, "branch"),
                step("src/a.py", 20, 5, "return"),
                step("src/a.py", 1, 500, "unknown-kind"),
                step("src/a.py", 101, 101, "call"),
                step("src/missing.py", 1, 2, "call"),
                step("../outside.py", 1, 2, "call"),
            ],
            meta: None,
        };
        verify(&root, &mut walk);
        let got: Vec<(bool, i64, &str)> = walk.steps.iter().map(|s| (s.verified, s.end_line, s.kind.as_str())).collect();
        assert_eq!(
            got,
            [
                (true, 12, "call"),
                (true, 100, "branch"),     // past the end of the file
                (true, 20, "return"),      // an end before the start shows one line
                (true, 61, "transform"),   // spans are capped; unknown kinds fall back
                (false, 101, "call"),
                (false, 2, "call"),
                (false, 2, "call"),
            ]
        );
        assert_eq!(walk.steps[0].path, "src/a.py");
        assert_eq!(walk.steps[4].problem.as_deref(), Some("file has only 100 lines"));
        assert_eq!(walk.steps[6].problem.as_deref(), Some("outside the repository"));
        let _ = fs::remove_dir_all(&dir);
    }
}
