//! Example traces for simulation: concrete inputs that travel one flow of a map,
//! each taking the branches the code really has, with the data shape at every hop.

use crate::map::{slug, Evidence, Flow, Map};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fmt::Write;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceStep {
    pub from: String,
    pub to: String,
    pub action: String,
    pub payload: String,
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    pub name: String,
    pub input: String,
    pub outcome: String,
    pub steps: Vec<TraceStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceMeta {
    pub flow: String,
    pub generated_at: u64,
    pub duration_ms: u64,
    pub commands: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Traces {
    pub scenarios: Vec<Scenario>,
    #[serde(default)]
    pub diagnostics: Vec<String>,
    #[serde(default)]
    pub meta: Option<TraceMeta>,
}

pub fn schema() -> Value {
    let evidence = json!({
        "type": "array",
        "items": {
            "type": "object",
            "additionalProperties": false,
            "required": ["path", "line", "note"],
            "properties": { "path": { "type": "string" }, "line": { "type": "integer" }, "note": { "type": "string" } }
        }
    });
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["scenarios"],
        "properties": {
            "scenarios": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "input", "outcome", "steps"],
                    "properties": {
                        "name": { "type": "string" },
                        "input": { "type": "string" },
                        "outcome": { "type": "string" },
                        "steps": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["from", "to", "action", "payload", "evidence"],
                                "properties": {
                                    "from": { "type": "string" },
                                    "to": { "type": "string" },
                                    "action": { "type": "string" },
                                    "payload": { "type": "string" },
                                    "evidence": evidence
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

pub fn prompt(map: &Map, flow: &Flow) -> String {
    let mut p = String::from(
        "You are helping a developer learn a repository by watching concrete inputs travel \
through it. The repository root is the current working directory. Investigate with read-only \
shell commands (ls, rg, sed -n, nl -ba, cat). Do not modify files, install packages, or run \
the project's code, builds or tests.

Return ONLY the JSON object required by the output schema.
",
    );
    let _ = writeln!(p, "\nArchitecture map: {}. {}", map.title, map.summary);
    p.push_str("Nodes (use these ids):\n");
    for n in &map.nodes {
        let paths = if n.paths.is_empty() { String::new() } else { format!(" [{}]", n.paths.join(", ")) };
        let _ = writeln!(p, "- {}: {} ({}). {}{paths}", n.id, n.name, n.kind, n.summary);
    }
    p.push_str("Edges:\n");
    for e in &map.edges {
        let _ = writeln!(p, "- {} → {} [{}]: {}", e.from, e.to, e.kind, e.label);
    }
    let _ = writeln!(p, "\nFlow to trace: \"{}\". {}", flow.name, flow.description);
    for (i, s) in flow.steps.iter().enumerate() {
        let _ = writeln!(p, "{}. {} → {}: {}", i + 1, s.from, s.to, s.action);
    }
    p.push_str(
        "
Instructions:
- scenarios: 2-5 concrete, realistic inputs that take different paths through this flow: the \
normal path plus rejections, validation failures, errors, retries or alternative routes. Only \
use branches you confirm in the code.
- name: a short label. input: the concrete value entering the flow (a request body, message, \
command, event), with realistic field names and values. outcome: what that input ends as.
- steps: the hops this input takes, in order, using node ids from the list above. A scenario \
may stop early or leave the listed flow where the code branches.
- payload: the data as it looks on that hop, using field names from the code (JSON-like, at \
most 300 characters).
- action: what happens on the hop, including the condition that sends this input this way.
- evidence: 1-2 citations per step (repository-relative path and the 1-based line you read in \
`nl -ba` or `rg -n` output) showing the branch or transformation.
",
    );
    p
}

/// Use the map's node ids and drop hops that reference nodes not on the map.
pub fn normalize(traces: &mut Traces, map: &Map) {
    let ids: HashSet<&str> = map.nodes.iter().map(|n| n.id.as_str()).collect();
    let Traces { scenarios, diagnostics, .. } = traces;
    for sc in scenarios.iter_mut() {
        sc.steps.retain_mut(|s| {
            s.from = slug(&s.from);
            s.to = slug(&s.to);
            let known = ids.contains(s.from.as_str()) && ids.contains(s.to.as_str());
            if !known {
                diagnostics.push(format!("scenario `{}`: hop `{}` → `{}` is not on the map: dropped", sc.name, s.from, s.to));
            }
            known
        });
    }
    scenarios.retain(|sc| {
        if sc.steps.is_empty() {
            diagnostics.push(format!("scenario `{}` has no hops on the map: dropped", sc.name));
        }
        !sc.steps.is_empty()
    });
}
