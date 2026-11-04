//! The architecture map returned by the agent for one zoom level, plus the
//! fields the server adds (verification results, diagnostics, metadata).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;

pub const NODE_KINDS: &[&str] = &[
    "component", "service", "library", "module", "class", "function", "datastore", "queue",
    "external", "actor", "ui", "config",
];

pub const EDGE_KINDS: &[&str] = &[
    "call", "http", "rpc", "event", "queue", "read", "write", "file", "socket", "process",
    "deploy", "config", "import", "other",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub path: String,
    pub line: i64,
    pub note: String,
    /// Set by the server: the file exists inside the repository and has this line.
    #[serde(default)]
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub summary: String,
    pub paths: Vec<String>,
    pub evidence: Vec<Evidence>,
    pub explorable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub label: String,
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub from: String,
    pub to: String,
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flow {
    pub name: String,
    pub description: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    /// Node ids joined by `/`; empty for the system map.
    pub scope: String,
    /// Display names for the breadcrumb: system title, then each node on the path.
    pub crumbs: Vec<String>,
    pub generated_at: u64,
    pub commit: Option<String>,
    pub dirty: bool,
    pub model: Option<String>,
    pub duration_ms: u64,
    pub commands: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Input tokens served from the provider's cache (shared session context).
    #[serde(default)]
    pub cached_input_tokens: u64,
    /// Codex session the map was made in (`codex resume <id>` continues it).
    #[serde(default)]
    pub session: Option<String>,
    /// Refreshed in its existing session rather than explored from scratch.
    #[serde(default)]
    pub refreshed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Map {
    pub title: String,
    pub summary: String,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub flows: Vec<Flow>,
    #[serde(default)]
    pub diagnostics: Vec<String>,
    #[serde(default)]
    pub meta: Option<Meta>,
}

/// JSON schema for the agent's final answer. Strict-mode compatible: every
/// object closes `additionalProperties` and requires all of its properties.
pub fn schema() -> Value {
    let evidence = json!({
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
    });
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["title", "summary", "nodes", "edges", "flows"],
        "properties": {
            "title": { "type": "string" },
            "summary": { "type": "string" },
            "nodes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id", "name", "kind", "summary", "paths", "evidence", "explorable"],
                    "properties": {
                        "id": { "type": "string" },
                        "name": { "type": "string" },
                        "kind": { "type": "string", "enum": NODE_KINDS },
                        "summary": { "type": "string" },
                        "paths": { "type": "array", "items": { "type": "string" } },
                        "evidence": evidence,
                        "explorable": { "type": "boolean" }
                    }
                }
            },
            "edges": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["from", "to", "kind", "label", "evidence"],
                    "properties": {
                        "from": { "type": "string" },
                        "to": { "type": "string" },
                        "kind": { "type": "string", "enum": EDGE_KINDS },
                        "label": { "type": "string" },
                        "evidence": evidence
                    }
                }
            },
            "flows": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "description", "steps"],
                    "properties": {
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "steps": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["from", "to", "action"],
                                "properties": {
                                    "from": { "type": "string" },
                                    "to": { "type": "string" },
                                    "action": { "type": "string" }
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

/// Node ids become scope path segments, so they must never contain `/`.
pub fn slug(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() { "node".to_owned() } else { out.to_owned() }
}

/// Make ids path-safe and unique, and drop references to nodes that do not exist.
pub fn normalize(map: &mut Map) {
    let Map { nodes, edges, flows, diagnostics, .. } = map;
    let mut ids = HashSet::new();
    nodes.retain_mut(|n| {
        n.id = slug(&n.id);
        let unique = ids.insert(n.id.clone());
        if !unique {
            diagnostics.push(format!("duplicate node id `{}`: later node dropped", n.id));
        }
        unique
    });
    edges.retain_mut(|e| {
        e.from = slug(&e.from);
        e.to = slug(&e.to);
        let known = ids.contains(&e.from) && ids.contains(&e.to);
        if !known {
            diagnostics.push(format!("edge `{}` → `{}` references an unknown node: dropped", e.from, e.to));
        }
        known
    });
    for flow in flows.iter_mut() {
        flow.steps.retain_mut(|s| {
            s.from = slug(&s.from);
            s.to = slug(&s.to);
            let known = ids.contains(&s.from) && ids.contains(&s.to);
            if !known {
                diagnostics.push(format!(
                    "flow `{}`: step `{}` → `{}` references an unknown node: dropped",
                    flow.name, s.from, s.to
                ));
            }
            known
        });
    }
    flows.retain(|f| {
        if f.steps.is_empty() {
            diagnostics.push(format!("flow `{}` has no valid steps: dropped", f.name));
        }
        !f.steps.is_empty()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str) -> Node {
        Node {
            id: id.into(),
            name: id.into(),
            kind: "component".into(),
            summary: String::new(),
            paths: vec![],
            evidence: vec![],
            explorable: false,
        }
    }

    fn edge(from: &str, to: &str) -> Edge {
        Edge { from: from.into(), to: to.into(), kind: "call".into(), label: String::new(), evidence: vec![] }
    }

    #[test]
    fn ids_cannot_escape_scope_segments() {
        assert_eq!(slug("api/server"), "api-server");
        assert_eq!(slug("../../etc"), "etc");
        assert_eq!(slug("  Web UI  "), "web-ui");
        assert_eq!(slug("///"), "node");
    }

    #[test]
    fn references_follow_normalized_ids_and_unknowns_are_dropped() {
        let mut map = Map {
            title: String::new(),
            summary: String::new(),
            nodes: vec![node("Web UI"), node("api/server"), node("web-ui")],
            edges: vec![edge("Web UI", "api/server"), edge("api-server", "ghost")],
            flows: vec![
                Flow {
                    name: "request".into(),
                    description: String::new(),
                    steps: vec![
                        Step { from: "web ui".into(), to: "api-server".into(), action: String::new() },
                        Step { from: "ghost".into(), to: "web-ui".into(), action: String::new() },
                    ],
                },
                Flow {
                    name: "phantom".into(),
                    description: String::new(),
                    steps: vec![Step { from: "ghost".into(), to: "ghost".into(), action: String::new() }],
                },
            ],
            diagnostics: vec![],
            meta: None,
        };
        normalize(&mut map);
        let ids: Vec<_> = map.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["web-ui", "api-server"]);
        assert_eq!(map.edges.len(), 1);
        assert_eq!((map.edges[0].from.as_str(), map.edges[0].to.as_str()), ("web-ui", "api-server"));
        assert_eq!(map.flows.len(), 1);
        assert_eq!(map.flows[0].steps.len(), 1);
        // duplicate node, dangling edge, two dangling steps, emptied flow
        assert_eq!(map.diagnostics.len(), 5);
    }
}
