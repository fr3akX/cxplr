//! Full-project indexing: explore the system map and then, level by level, every
//! component worth a deeper map, so browsing afterwards is served from the cache.
//! Fresh maps are skipped, which makes indexing resumable and incremental.
//!
//! Recursion ends at nodes that are not explorable, external, below the size floor
//! or not narrower than the map they appear in. The last rule guarantees that even
//! unlimited depth terminates: every level covers strictly fewer files.

use crate::engine::Usage;
use crate::explore::Explorer;
use crate::map::Map;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
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
    pub cached_input_tokens: u64,
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
    } else if n >= 1_000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

/// `269k in (73% cached) / 13k out`
fn fmt_usage(input: u64, cached: u64, output: u64) -> String {
    let share = if input > 0 { format!(" ({}% cached)", cached * 100 / input) } else { String::new() };
    format!("{} in{share} / {} out", fmt_tokens(input), fmt_tokens(output))
}

/// `ctx 85k/475k (18%)`, or `ctx 85k` when the window is unknown.
fn fmt_context(u: &Usage) -> String {
    match u.context_window.filter(|w| *w > 0) {
        Some(w) => format!("ctx {}/{} ({}%)", fmt_tokens(u.context_tokens), fmt_tokens(w), u.context_tokens * 100 / w),
        None => format!("ctx {}", fmt_tokens(u.context_tokens)),
    }
}

/// The live display: an overall bar plus one line per running exploration. Without a
/// terminal the bars stay hidden and lines are printed as they happen.
struct Display {
    multi: MultiProgress,
    overall: ProgressBar,
    board: Mutex<Board>,
}

#[derive(Default)]
struct Board {
    /// Token totals of finished maps.
    finished: (u64, u64, u64),
    /// Usage of the explorations running now, by scope.
    live: HashMap<String, Usage>,
    running: usize,
    queued: usize,
}

/// One running exploration's line.
#[derive(Default)]
struct JobLine {
    usage: Usage,
    calls: u32,
    last: String,
}

impl JobLine {
    fn render(&self) -> String {
        let mut parts = vec![format!("{} calls", self.calls)];
        if self.usage.requests > 0 {
            parts.push(fmt_context(&self.usage));
            parts.push(fmt_usage(self.usage.input_tokens, self.usage.cached_input_tokens, self.usage.output_tokens));
        }
        if !self.last.is_empty() {
            parts.push(self.last.clone());
        }
        parts.join(" · ")
    }
}

impl Display {
    fn new() -> Arc<Display> {
        let multi = MultiProgress::new();
        let overall = multi.add(ProgressBar::new(1));
        overall.set_style(
            ProgressStyle::with_template("{bar:28.cyan/blue} {pos}/{len} maps · {elapsed} · {wide_msg}")
                .expect("valid template")
                .progress_chars("█▉▊▋▌▍▎▏ "),
        );
        overall.enable_steady_tick(Duration::from_millis(250));
        Arc::new(Display { multi, overall, board: Mutex::new(Board::default()) })
    }

    /// A line above the bars (or plain output without a terminal).
    fn say(&self, line: String) {
        if self.multi.is_hidden() {
            println!("{line}");
        } else {
            let _ = self.multi.println(line);
        }
    }

    fn counts(&self, done: usize, running: usize, queued: usize) {
        self.overall.set_length((done + running + queued).max(1) as u64);
        self.overall.set_position(done as u64);
        let mut board = self.board.lock().unwrap();
        board.running = running;
        board.queued = queued;
        self.refresh(&board);
    }

    fn refresh(&self, board: &Board) {
        let (mut input, mut cached, mut output) = board.finished;
        for u in board.live.values() {
            input += u.input_tokens;
            cached += u.cached_input_tokens;
            output += u.output_tokens;
        }
        self.overall.set_message(format!("{} running · {} queued · {}", board.running, board.queued, fmt_usage(input, cached, output)));
    }

    /// A spinner line for an exploration that starts now.
    fn start(&self, label: &str) -> ProgressBar {
        if self.multi.is_hidden() {
            println!("  → {label}");
        }
        let bar = self.multi.add(ProgressBar::new_spinner());
        bar.set_style(ProgressStyle::with_template("  {spinner:.green} {prefix:.bold} {elapsed} · {wide_msg}").expect("valid template"));
        // The component's own name; finished lines above keep the whole trail.
        bar.set_prefix(label.rsplit(" › ").next().unwrap_or(label).to_owned());
        bar.set_message("starting");
        bar.enable_steady_tick(Duration::from_millis(120));
        bar
    }

    fn live_usage(&self, scope: &str, usage: Usage) {
        let mut board = self.board.lock().unwrap();
        board.live.insert(scope.to_owned(), usage);
        self.refresh(&board);
    }

    fn finished(&self, scope: &str, input: u64, cached: u64, output: u64) {
        let mut board = self.board.lock().unwrap();
        board.live.remove(scope);
        board.finished.0 += input;
        board.finished.1 += cached;
        board.finished.2 += output;
        self.refresh(&board);
    }

    fn clear(&self) {
        self.overall.finish_and_clear();
        let _ = self.multi.clear();
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
    let mut running: JoinSet<(Item, ProgressBar, Duration, Result<Map, String>)> = JoinSet::new();
    println!(
        "indexing {} (depth {}, {} parallel, components under {} lines stay leaves)",
        ex.repo.root.display(),
        opts.depth.map_or_else(|| "full".to_owned(), |d| d.to_string()),
        opts.jobs,
        opts.min_lines
    );
    let display = Display::new();
    let done = |s: &Summary| s.explored + s.cached + s.failed;

    loop {
        while running.len() < opts.jobs.max(1) {
            let Some(item) = queue.pending.pop_front() else { break };
            if let Some(cached) = ex.cache.load(&ex.repo, &item.scope) {
                if cached.freshness == crate::cache::Freshness::Fresh {
                    summary.cached += 1;
                    display.say(format!("  = {} (cached)", item.label));
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
                        display.say(format!("  ✗ {}: {e}", item.label));
                        continue;
                    }
                }
            };
            let bar = display.start(&item.label);
            let (ex, display) = (ex.clone(), display.clone());
            running.spawn(async move {
                let t = Instant::now();
                let cancel = Notify::new();
                let line = Arc::new(Mutex::new(JobLine::default()));
                let (on_line, on_usage) = (line.clone(), line);
                let (bar_l, bar_u, scope) = (bar.clone(), bar.clone(), item.scope.clone());
                let progress = move |text: String| {
                    let mut l = on_line.lock().unwrap();
                    if text.starts_with("$ ") {
                        l.calls += 1;
                    }
                    if text.starts_with("$ ") || text.starts_with("› ") || text.starts_with("… ") {
                        l.last = text;
                    }
                    bar_l.set_message(l.render());
                };
                let usage = move |u: Usage| {
                    let mut l = on_usage.lock().unwrap();
                    l.usage = u;
                    bar_u.set_message(l.render());
                    display.live_usage(&scope, u);
                };
                let result = ex.explore(&item.scope, lineage.as_ref(), &cancel, progress, usage).await;
                (item, bar, t.elapsed(), result)
            });
        }
        display.counts(done(&summary), running.len(), queue.pending.len());
        if running.is_empty() {
            break;
        }
        let joined = tokio::select! {
            joined = running.join_next() => joined,
            _ = tokio::signal::ctrl_c() => {
                display.clear();
                // Dropping the set aborts the tasks and with them their engine turns.
                return Err("interrupted; finished maps are cached, re-run to continue".into());
            }
        };
        let Some(joined) = joined else { break };
        let (item, bar, took, result) = joined.map_err(|e| format!("index task failed: {e}"))?;
        bar.finish_and_clear();
        match result {
            Ok(map) => {
                summary.explored += 1;
                let meta = map.meta.as_ref();
                let (input, cached, output) = meta.map_or((0, 0, 0), |m| (m.input_tokens, m.cached_input_tokens, m.output_tokens));
                summary.input_tokens += input;
                summary.cached_input_tokens += cached;
                summary.output_tokens += output;
                display.finished(&item.scope, input, cached, output);
                display.say(format!(
                    "  ✓ {} ({}, {} nodes, {} tokens) [{} queued, {} running]",
                    item.label,
                    fmt_duration(took),
                    map.nodes.len(),
                    fmt_usage(input, cached, output),
                    queue.pending.len(),
                    running.len()
                ));
                queue.children(&ex, &opts, &item, &map, &mut summary);
            }
            Err(e) => {
                summary.failed += 1;
                display.finished(&item.scope, 0, 0, 0);
                display.say(format!("  ✗ {}: {e}", item.label));
            }
        }
    }
    display.clear();
    println!(
        "indexed in {}: {} explored, {} already cached, {} failed, {} levels deep; leaves: {} small, {} not narrower than their parent; {} tokens",
        fmt_duration(started.elapsed()),
        summary.explored,
        summary.cached,
        summary.failed,
        summary.deepest,
        summary.skipped_small,
        summary.skipped_not_narrower,
        fmt_usage(summary.input_tokens, summary.cached_input_tokens, summary.output_tokens)
    );
    Ok(summary)
}
