//! Prompts for each zoom level. The system map is explored from scratch; a
//! deeper map is explored with the higher-level context so the agent knows
//! which interfaces the component has to explain.

use crate::map::{Map, Node};
use std::fmt::Write;

/// Part of every cache key: bump when prompts or the map schema change meaning.
pub const VERSION: &str = "1";

const RULES: &str = "\
You are mapping a software repository so a developer can learn it top-down. The repository \
root is the current working directory. Explore it with read-only shell commands (ls, rg, \
sed -n, nl -ba, cat, git log). Do not modify files, install packages, or run the project's \
code, builds or tests.

When done, answer ONLY with the JSON object required by the output schema.

Map rules:
- Node ids: short lowercase slugs (a-z, 0-9, '-', '_'), unique within this map. Edges and \
flow steps refer to nodes by id.
- name: human-readable. summary: 1-3 sentences on the node's responsibility and why it \
matters. Plain text; `backticks` allowed for identifiers.
- paths: repository-relative files or directories implementing the node. Empty for \
external systems and actors.
- edges: concrete interactions. Direction is the direction the request or data travels \
(caller → callee, producer → consumer, writer → store). label says what moves, e.g. \
\"POST /jobs with the job spec\" or \"reads config.toml at startup\".
- flows: the most important end-to-end paths through this map, as ordered steps; each \
step should correspond to an edge.
- evidence: repository-relative path and 1-based line number of a line you actually read \
that shows the claim (definition, call site, route, config entry). Take line numbers from \
`nl -ba` or `rg -n` output; never estimate them. Prefer 1-3 strong citations per node or edge.
- explorable: true when the node has enough internal structure for a deeper map (several \
files, modules or classes); false for small single files, external systems and actors.
- When something is inferred rather than seen in code or config, write \"(inferred)\" in \
the summary or label.
- Ignore vendored, generated and build output (node_modules, target, dist, build, .git, \
.cxplr, lock files).
";

pub fn system(repo_name: &str) -> String {
    format!(
        "{RULES}
Task: produce the top-level system map of the repository `{repo_name}`.
- title: the project's name. summary: 2-5 sentences on what the system does, who or what \
uses it, and how it is built, deployed or run.
- nodes: 4-14 nodes. The main components (services, binaries, libraries or packages, major \
subsystems, UIs) plus the datastores, external systems and actors they interact with. Group \
small things; this is the 10,000-foot view.
- Start from the README and docs, build and deployment manifests (Cargo.toml, package.json, \
pyproject.toml, go.mod, Dockerfile, compose files, Kubernetes, systemd units, CI), and entry \
points. Then confirm each interaction in code or configuration.
- flows: 2-6 main data flows, such as handling a user request, a background job, or a \
deployment.
"
    )
}

/// `crumbs` holds (name, summary) for the system and every node from the top down to and
/// including `node`; `parent` is the map that contains `node`.
pub fn component(crumbs: &[(String, String)], parent: &Map, node: &Node) -> String {
    let mut p = String::from(RULES);
    let (system_name, system_summary) = &crumbs[0];
    let trail: Vec<&str> = crumbs.iter().map(|(name, _)| name.as_str()).collect();
    let _ = write!(
        p,
        "
Task: produce the internal map of one component, one zoom level deeper than the map it \
appears in.

Context from the higher-level maps:
System: {system_name}. {system_summary}
Location: {trail}
Component: {name} ({kind}). {summary}
Owned paths: {paths}
",
        trail = trail.join(" › "),
        name = node.name,
        kind = node.kind,
        summary = node.summary,
        paths = node.paths.join(", "),
    );
    if crumbs.len() > 2 {
        let (enclosing, enclosing_summary) = &crumbs[crumbs.len() - 2];
        let _ = writeln!(p, "It is part of: {enclosing}. {enclosing_summary}");
    }

    let name_of = |id: &str| {
        parent.nodes.iter().find(|n| n.id == id).map_or(id.to_owned(), |n| format!("{} ({})", n.name, n.kind))
    };
    let interactions: Vec<String> = parent
        .edges
        .iter()
        .filter_map(|e| {
            if e.from == node.id && e.to != node.id {
                Some(format!("- outbound to {}: [{}] {}", name_of(&e.to), e.kind, e.label))
            } else if e.to == node.id && e.from != node.id {
                Some(format!("- inbound from {}: [{}] {}", name_of(&e.from), e.kind, e.label))
            } else {
                None
            }
        })
        .collect();
    if interactions.is_empty() {
        p.push_str("Interactions at the higher level: none recorded.\n");
    } else {
        p.push_str("Interactions at the higher level:\n");
        p.push_str(&interactions.join("\n"));
        p.push('\n');
    }

    p.push_str(
        "
Instructions:
- nodes: 3-14 internal parts (modules, packages, classes, key functions, internal stores) \
inside the owned paths. Group trivial helpers.
- Also add one node with kind \"external\" for each neighbour the internals interact with, \
named after the neighbour, so inbound and outbound interfaces stay visible. Do not map the \
neighbours' internals.
- edges: how the internal parts call or feed each other, and which part handles each \
interaction with a neighbour.
- flows: 1-5 flows showing how the higher-level interactions are carried out inside this \
component.
- title: the component's name. summary: 2-4 sentences on how it works internally.
- Stay inside the owned paths, except for brief checks of how neighbours call in.
",
    );
    p
}

/// Update a stale map inside the session that made it: the earlier exploration and
/// map are in context, so only the changed files need to be read again.
pub fn refresh(changed: &[String], total: usize) -> String {
    let mut p = String::from(RULES);
    p.push_str(
        "
Task: your earlier map of this scope is above in this session, but the repository has \
changed since. Files added, removed or changed:
",
    );
    for f in changed {
        let _ = writeln!(p, "- {f}");
    }
    if total > changed.len() {
        let _ = writeln!(p, "- … and {} more (use `git status` and `git diff` to see them)", total - changed.len());
    }
    p.push_str(
        "
Re-read those files; what you saw before is out of date, including line numbers. Check whether \
components, interactions or flows changed, and return the complete updated map for the same \
scope. Keep node ids for parts that did not change, and re-verify every citation you keep.
",
    );
    p
}
