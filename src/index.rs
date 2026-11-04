//! Full-project indexing: explore the system map and then, level by level, every
//! component worth a deeper map, so browsing afterwards is served from the cache.
//! Fresh maps are skipped, which makes indexing resumable and incremental.

use crate::explore::Explorer;
use crate::map::Map;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio::task::JoinSet;

pub struct Options {
    /// Levels to map: 1 = system only, 2 = system and its components, …
    pub depth: usize,
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

struct Queue {
    pending: VecDeque<(String, String)>,
    seen: HashSet<String>,
}

impl Queue {
    /// Child scopes of a finished map that pass the depth limit and size floor.
    fn children(&mut self, ex: &Explorer, opts: &Options, scope: &str, map: &Map, summary: &mut Summary) {
        if level(scope) >= opts.depth {
            return;
        }
        let trail = map.meta.as_ref().map(|m| m.crumbs.join(" › ")).unwrap_or_default();
        for node in &map.nodes {
            if !node.explorable || node.paths.is_empty() || matches!(node.kind.as_str(), "external" | "actor") {
                continue;
            }
            let child = if scope.is_empty() { node.id.clone() } else { format!("{scope}/{}", node.id) };
            if !self.seen.insert(child.clone()) {
                continue;
            }
            if ex.repo.line_count(&node.paths, opts.min_lines) < opts.min_lines {
                summary.skipped_small += 1;
                continue;
            }
            self.pending.push_back((child, format!("{trail} › {}", node.name)));
        }
    }
}

pub async fn run(ex: Arc<Explorer>, opts: Options) -> Result<Summary, String> {
    let started = Instant::now();
    let mut summary = Summary::default();
    let mut queue = Queue { pending: VecDeque::from([(String::new(), ex.repo.name.clone())]), seen: HashSet::new() };
    let mut running: JoinSet<(String, String, Duration, Result<Map, String>)> = JoinSet::new();
    println!(
        "indexing {} (depth {}, {} parallel, components under {} lines stay leaves)",
        ex.repo.root.display(),
        opts.depth,
        opts.jobs,
        opts.min_lines
    );

    loop {
        while running.len() < opts.jobs.max(1) {
            let Some((scope, label)) = queue.pending.pop_front() else { break };
            if let Some(cached) = ex.cache.load(&ex.repo, &scope) {
                if cached.freshness == crate::cache::Freshness::Fresh {
                    summary.cached += 1;
                    println!("  = {label} (cached)");
                    queue.children(&ex, &opts, &scope, &cached.map, &mut summary);
                    continue;
                }
            }
            let lineage = if scope.is_empty() {
                None
            } else {
                match ex.lineage(&scope) {
                    Ok(l) => Some(l),
                    Err(e) => {
                        summary.failed += 1;
                        println!("  ✗ {label}: {e}");
                        continue;
                    }
                }
            };
            println!("  → {label}");
            let ex = ex.clone();
            running.spawn(async move {
                let t = Instant::now();
                let cancel = Notify::new();
                let result = ex.explore(&scope, lineage.as_ref(), &cancel, |_| {}).await;
                (scope, label, t.elapsed(), result)
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
        let (scope, label, took, result) = joined.map_err(|e| format!("index task failed: {e}"))?;
        match result {
            Ok(map) => {
                summary.explored += 1;
                let meta = map.meta.as_ref();
                let (input, output) = meta.map_or((0, 0), |m| (m.input_tokens, m.output_tokens));
                summary.input_tokens += input;
                summary.output_tokens += output;
                println!(
                    "  ✓ {label} ({}, {} nodes, {} in / {} out tokens) [{} queued, {} running]",
                    fmt_duration(took),
                    map.nodes.len(),
                    fmt_tokens(input),
                    fmt_tokens(output),
                    queue.pending.len(),
                    running.len()
                );
                queue.children(&ex, &opts, &scope, &map, &mut summary);
            }
            Err(e) => {
                summary.failed += 1;
                println!("  ✗ {label}: {e}");
            }
        }
    }
    println!(
        "indexed in {}: {} explored, {} already cached, {} failed, {} small components left as leaves; {} in / {} out tokens",
        fmt_duration(started.elapsed()),
        summary.explored,
        summary.cached,
        summary.failed,
        summary.skipped_small,
        fmt_tokens(summary.input_tokens),
        fmt_tokens(summary.output_tokens)
    );
    Ok(summary)
}
