mod agent;
mod ask;
mod cache;
mod explore;
mod index;
mod lsp;
mod map;
mod prompt;
mod repo;
mod search;
mod server;
mod trace;
mod verify;

use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Explore a repository's architecture top-down in a local web UI: system map,
/// components, interactions and data flows, then drill into any component.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Repository (or directory) to explore
    #[arg(default_value = ".")]
    repo: PathBuf,
    /// Address to serve the UI on; keep it on loopback
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 7878)]
    port: u16,
    /// Codex model (default: the model in your Codex config)
    #[arg(long)]
    model: Option<String>,
    /// Codex executable
    #[arg(long, default_value = "codex")]
    codex: String,
    /// Time limit for one exploration or question, in minutes
    #[arg(long, default_value_t = 20)]
    timeout_mins: u64,
    /// Cache directory (default: `.code-xplr` in the explored directory)
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Index the project before serving: explore the system map and every
    /// component down to --depth, then open the browser
    #[arg(long)]
    index: bool,
    /// Index and exit without serving
    #[arg(long, conflicts_with = "index")]
    index_only: bool,
    /// Levels to index: 1 = system map, 2 = system and its components, …
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u16).range(1..=6))]
    depth: u16,
    /// Explorations to run at the same time while indexing
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=16))]
    jobs: u16,
    /// Components with fewer text lines stay leaves while indexing
    #[arg(long, default_value_t = 300)]
    min_lines: usize,
    /// Open the browser once the server is up (always after --index)
    #[arg(long)]
    open: bool,
    /// Language server for a language, e.g. `python=pyright-langserver --stdio`;
    /// an empty command disables it. Languages: rust, python, typescript, go, c
    #[arg(long = "lsp", value_name = "LANG=COMMAND", value_parser = parse_lsp)]
    lsp: Vec<(String, String)>,
    /// Disable language servers; navigation falls back to text search
    #[arg(long)]
    no_lsp: bool,
}

fn parse_lsp(raw: &str) -> Result<(String, String), String> {
    let (lang, cmd) = raw.split_once('=').ok_or("expected LANG=COMMAND")?;
    let lang = lang.trim().to_ascii_lowercase();
    if !lsp::LANGUAGES.contains(&lang.as_str()) {
        return Err(format!("unknown language `{lang}`; expected one of {}", lsp::LANGUAGES.join(", ")));
    }
    Ok((lang, cmd.trim().to_owned()))
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let opened = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok();
    if !opened {
        println!("could not start `{opener}`; open {url} yourself");
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let repo = repo::Repo::open(&args.repo)?;
    let cache_dir = args.cache_dir.unwrap_or_else(|| repo.root.join(repo::CACHE_DIR_NAME));
    let cache = cache::Cache::open(cache_dir.clone(), args.model.as_deref())
        .map_err(|e| format!("cache {}: {e} (use --cache-dir for read-only checkouts)", cache_dir.display()))?;
    let map_schema = cache_dir.join("map-schema.json");
    let ask_schema = cache_dir.join("ask-schema.json");
    let trace_schema = cache_dir.join("trace-schema.json");
    std::fs::write(&map_schema, serde_json::to_vec_pretty(&map::schema())?)?;
    std::fs::write(&ask_schema, serde_json::to_vec_pretty(&ask::schema())?)?;
    std::fs::write(&trace_schema, serde_json::to_vec_pretty(&trace::schema())?)?;
    let agent = agent::AgentConfig {
        bin: args.codex,
        model: args.model,
        timeout: Duration::from_secs(args.timeout_mins.max(1) * 60),
    };
    let ex = Arc::new(explore::Explorer { repo, cache, agent, map_schema });

    let commit = ex.repo.head().map_or("no git commit".to_owned(), |c| c[..c.len().min(12)].to_owned());
    println!("code-explore: {} ({commit}), cache {}", ex.repo.root.display(), cache_dir.display());

    if args.index || args.index_only {
        let opts = index::Options { depth: args.depth.into(), jobs: args.jobs.into(), min_lines: args.min_lines };
        let summary = index::run(ex.clone(), opts).await?;
        if args.index_only {
            std::process::exit(i32::from(summary.failed > 0));
        }
    }

    let lsp = lsp::Lsp::new(&ex.repo.root, !args.no_lsp, &args.lsp);
    let servers = lsp.configured();
    if servers.is_empty() {
        println!("language servers: none (navigation uses text search)");
    }
    for (lang, cmd) in servers {
        println!("language server: {lang} = {cmd}");
    }
    let listener = tokio::net::TcpListener::bind((args.host.as_str(), args.port)).await?;
    let url = format!("http://{}", listener.local_addr()?);
    println!("open {url}");
    if args.open || args.index {
        open_browser(&url);
    }
    server::serve(listener, server::AppState::new(ex, lsp, ask_schema, trace_schema)).await?;
    Ok(())
}
