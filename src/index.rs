//! Full-project indexing: explore the system map and then, level by level, every
//! component worth a deeper map, so browsing afterwards is served from the cache.
//! Fresh maps are skipped, which makes indexing resumable and incremental.
//!
//! Recursion ends at nodes that are not explorable, external, below the size floor
//! or not narrower than the map they appear in. The last rule guarantees that even
//! unlimited depth terminates: every level covers strictly fewer files.

use crate::explore::Explorer;
use crate::map::Map;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio::task::JoinSet;

pub struct Options {
    /// Levels to map (1 = system only, 2 = system and its components, …);
    /// `None` recurses until no component qualifies.
    pub depth: Option<usize>,
    /// Explorations running at the same time.
    pub jobs: usize,
    /// Components with fewer text lines are left as leaves (the source view covers them).
    pub min_lines: usize,
}

#[derive(Default)]
pub struct Summary {
    pub explored: usize,
    pub cached: usize,
    pub failed: usize,
    pub skipped_small: usize,
    pub skipped_not_narrower: usize,
    pub deepest: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

fn level(scope: &str) -> usize {
    if scope.is_empty() { 1 } else { scope.split('/').count() + 1 }
}

fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 { format!("{s}s") } else { format!("{}m{:02}s", s / 60, s % 60) }
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else {
        format!("{}k", n / 1000)
    }
}

struct Item {
    scope: String,
    label: String,
    /// Files covered by the scope; children must cover fewer.
    files: usize,
}

struct Queue {
    pending: VecDeque<Item>,
    seen: HashSet<String>,
}

impl Queue {
    /// Child scopes of a finished map that pass the depth limit, size floor and narrowing rule.
    fn children(&mut self, ex: &Explorer, opts: &Options, parent: &Item, map: &Map, summary: &mut Summary) {
        summary.deepest = summary.deepest.max(level(&parent.scope));
        if opts.depth.is_some_and(|d| level(&parent.scope) >= d) {
            return;
        }
        let trail = map.meta.as_ref().map(|m| m.crumbs.join(" › ")).unwrap_or_default();
        for node in &map.nodes {
            if !node.explorable || node.paths.is_empty() || matches!(node.kind.as_str(), "external" | "actor") {
                continue;
            }
            let scope = if parent.scope.is_empty() { node.id.clone() } else { format!("{}/{}", parent.scope, node.id) };
            if !self.seen.insert(scope.clone()) {
                continue;
            }
            let files = ex.repo.manifest(&node.paths).len();
            if files >= parent.files {
                summary.skipped_not_narrower += 1;
                continue;
            }
            if ex.repo.line_count(&node.paths, opts.min_lines) < opts.min_lines {
                summary.skipped_small += 1;
                continue;
            }
            self.pending.push_back(Item { scope, label: format!("{trail} › {}", node.name), files });
        }
    }
}

pub async fn run(ex: Arc<Explorer>, opts: Options) -> Result<Summary, String> {
    let started = Instant::now();
    let mut summary = Summary::default();
    let root = Item { scope: String::new(), label: ex.repo.name.clone(), files: ex.repo.manifest(&[".".to_owned()]).len() };
    let mut queue = Queue { pending: VecDeque::from([root]), seen: HashSet::new() };
    let mut running: JoinSet<(Item, Duration, Result<Map, String>)> = JoinSet::new();
    println!(
        "indexing {} (depth {}, {} parallel, components under {} lines stay leaves)",
        ex.repo.root.display(),
        opts.depth.map_or_else(|| "full".to_owned(), |d| d.to_string()),
        opts.jobs,
        opts.min_lines
    );

    loop {
        while running.len() < opts.jobs.max(1) {
            let Some(item) = queue.pending.pop_front() else { break };
            if let Some(cached) = ex.cache.load(&ex.repo, &item.scope) {
                if cached.freshness == crate::cache::Freshness::Fresh {
                    summary.cached += 1;
                    println!("  = {} (cached)", item.label);
                    queue.children(&ex, &opts, &item, &cached.map, &mut summary);
                    continue;
                }
            }
            let lineage = if item.scope.is_empty() {
                None
            } else {
                match ex.lineage(&item.scope) {
                    Ok(l) => Some(l),
                    Err(e) => {
                        summary.failed += 1;
                        println!("  ✗ {}: {e}", item.label);
                        continue;
                    }
                }
            };
            println!("  → {}", item.label);
            let ex = ex.clone();
            running.spawn(async move {
                let t = Instant::now();
                let cancel = Notify::new();
                let result = ex.explore(&item.scope, lineage.as_ref(), &cancel, |_| {}).await;
                (item, t.elapsed(), result)
            });
        }
        if running.is_empty() {
            break;
        }
        let joined = tokio::select! {
            joined = running.join_next() => joined,
            _ = tokio::signal::ctrl_c() => {
                // Dropping the set aborts the tasks; their codex processes are killed.
                return Err("interrupted; finished maps are cached, re-run to continue".into());
            }
        };
        let Some(joined) = joined else { break };
        let (item, took, result) = joined.map_err(|e| format!("index task failed: {e}"))?;
        match result {
            Ok(map) => {
                summary.explored += 1;
                let meta = map.meta.as_ref();
                let (input, output) = meta.map_or((0, 0), |m| (m.input_tokens, m.output_tokens));
                summary.input_tokens += input;
                summary.output_tokens += output;
                println!(
                    "  ✓ {} ({}, {} nodes, {} in / {} out tokens) [{} queued, {} running]",
                    item.label,
                    fmt_duration(took),
                    map.nodes.len(),
                    fmt_tokens(input),
                    fmt_tokens(output),
                    queue.pending.len(),
                    running.len()
                );
                queue.children(&ex, &opts, &item, &map, &mut summary);
            }
            Err(e) => {
                summary.failed += 1;
                println!("  ✗ {}: {e}", item.label);
            }
        }
    }
    println!(
        "indexed in {}: {} explored, {} already cached, {} failed, {} levels deep; leaves: {} small, {} not narrower than their parent; {} in / {} out tokens",
        fmt_duration(started.elapsed()),
        summary.explored,
        summary.cached,
        summary.failed,
        summary.deepest,
        summary.skipped_small,
        summary.skipped_not_narrower,
        fmt_tokens(summary.input_tokens),
        fmt_tokens(summary.output_tokens)
    );
    Ok(summary)
}
