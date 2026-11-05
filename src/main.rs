mod anthropic;
mod anthropic_auth;
mod anthropic_tools;
mod anthropic_wire;
mod ask;
mod atlas;
mod cache;
mod claude;
mod codex;
mod engine;
mod explore;
mod index;
mod lsp;
mod map;
mod prompt;
mod repo;
mod search;
mod server;
mod skill;
mod trace;
mod update;
mod verify;
mod walk;

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
    /// Agent: codex app-server, claude -p, or native Anthropic Messages
    #[arg(long, value_enum, default_value = "codex")]
    engine: engine::Kind,
    /// Model (CLI engines use their config; native Anthropic defaults to claude-sonnet-5-5)
    #[arg(long)]
    model: Option<String>,
    /// Codex executable (run as `codex app-server`)
    #[arg(long, default_value = "codex")]
    codex: String,
    /// Claude Code executable (run as `claude -p`)
    #[arg(long, default_value = "claude")]
    claude: String,
    /// Sign into native Anthropic with Claude subscription OAuth and exit
    #[arg(long)]
    anthropic_login: bool,
    /// Time limit for one exploration or question, in minutes
    #[arg(long, default_value_t = 20)]
    timeout_mins: u64,
    /// Cache directory (default: `.cxplr` in the explored directory)
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Index the project before serving: explore the system map and every
    /// component down to --depth, then open the browser
    #[arg(long)]
    index: bool,
    /// Index and exit without serving
    #[arg(long, conflicts_with = "index")]
    index_only: bool,
    /// Levels to index: 1 = system map, 2 = system and its components, …, or `full`
    /// to recurse until no component is explorable, large enough and narrower than its parent
    #[arg(long, default_value = "2", value_name = "N|full", value_parser = parse_depth)]
    depth: Depth,
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
    /// an empty command disables it. Languages: rust, python, typescript, go, c, scala
    #[arg(long = "lsp", value_name = "LANG=COMMAND", value_parser = parse_lsp)]
    lsp: Vec<(String, String)>,
    /// Disable language servers; navigation falls back to text search
    #[arg(long)]
    no_lsp: bool,
    /// Let language servers import the project build for full navigation (Metals:
    /// runs the build tool, e.g. sbt, on the repository). Off: Scala stays read-only
    #[arg(long)]
    lsp_import_build: bool,
    /// Delete every session of the selected engine cxplr created for this project and
    /// exit (cached maps are kept; later explorations start new sessions)
    #[arg(long)]
    forget_sessions: bool,
    /// Install the cxplr skill for the selected engine
    /// (`<config dir>/skills/cxplr/SKILL.md`) and exit; it points that agent's
    /// sessions at `.cxplr/atlas` in repositories that have one
    #[arg(long)]
    install_skill: bool,
    /// Install the latest release now (for installs made with the release installer) and exit
    #[arg(long)]
    update: bool,
    /// Skip the check for a newer release (also: CXPLR_NO_UPDATE=1)
    #[arg(long)]
    no_update: bool,
}

/// Index depth: a level count, or `None` for the full hierarchy.
#[derive(Clone, Copy)]
struct Depth(Option<usize>);

fn parse_depth(raw: &str) -> Result<Depth, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "full" | "all" => Ok(Depth(None)),
        n => n
            .parse::<usize>()
            .ok()
            .filter(|&d| d >= 1)
            .map(|d| Depth(Some(d)))
            .ok_or_else(|| format!("expected a number of levels (1, 2, …) or `full`, got `{raw}`")),
    }
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

async fn rebuild_atlas(ex: &Arc<explore::Explorer>) {
    let ex = ex.clone();
    match tokio::task::spawn_blocking(move || atlas::rebuild(&ex.repo, &ex.cache).map(|n| (n, ex.cache.dir().join("atlas")))).await {
        Ok(Ok((0, _))) => {}
        Ok(Ok((pages, dir))) => println!("atlas: {pages} pages in {}", dir.display()),
        Ok(Err(e)) => println!("atlas not updated: {e}"),
        Err(e) => println!("atlas not updated: {e}"),
    }
}

/// Raise the soft open-file limit to the hard limit. `codex app-server` keeps every
/// session it has loaded open until it exits, and a large index loads dozens, which
/// runs out of descriptors under macOS's default soft limit of 256. The app-server
/// and language servers inherit the raised limit.
#[cfg(unix)]
fn raise_open_file_limit() {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit/setrlimit only read and write the struct passed in.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return;
    }
    // macOS rejects soft limits above OPEN_MAX, even when the hard limit is unlimited.
    #[cfg(target_os = "macos")]
    let want = lim.rlim_max.min(10240);
    #[cfg(not(target_os = "macos"))]
    let want = lim.rlim_max;
    if want > lim.rlim_cur {
        let raised = libc::rlimit { rlim_cur: want, rlim_max: lim.rlim_max };
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == 0 {
            lim.rlim_cur = want;
        }
    }
    if lim.rlim_cur < 1024 {
        println!("warning: open-file limit is {}; large indexes may fail (raise it with `ulimit -n`)", lim.rlim_cur);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.anthropic_login {
        anthropic_auth::login().await?;
        return Ok(());
    }
    if args.update {
        // The new binary refreshes an installed skill on its first start.
        println!("{}", update::update_now().await?);
        return Ok(());
    }
    if args.install_skill {
        let path = skill::install(args.engine)?;
        println!("installed the cxplr skill at {}", path.display());
        return Ok(());
    }
    for path in skill::refresh_installed() {
        println!("updated the cxplr skill at {}", path.display());
    }
    if !args.no_update {
        // Off the startup path; prints a one-line notice when a newer release exists.
        tokio::spawn(async {
            if let Some(message) = update::notice().await {
                println!("{message}");
            }
        });
    }
    #[cfg(unix)]
    raise_open_file_limit();
    let repo = repo::Repo::open(&args.repo)?;
    let cache_dir = args.cache_dir.unwrap_or_else(|| repo.root.join(repo::CACHE_DIR_NAME));
    let model = match (args.engine, args.model) {
        (engine::Kind::Anthropic, None) => Some(anthropic::DEFAULT_MODEL.to_owned()),
        (_, model) => model,
    };
    let cache = cache::Cache::open(cache_dir.clone(), args.engine, model.as_deref())
        .map_err(|e| format!("cache {}: {e} (use --cache-dir for read-only checkouts)", cache_dir.display()))?;
    let timeout = Duration::from_secs(args.timeout_mins.max(1) * 60);
    let engine = match args.engine {
        engine::Kind::Codex => {
            let cfg = codex::CodexConfig { bin: args.codex, model, timeout };
            engine::Engine::Codex(codex::Codex::start(cfg, &repo.root).await?)
        }
        engine::Kind::Claude => {
            let cfg = claude::ClaudeConfig { bin: args.claude, model, timeout };
            engine::Engine::Claude(Arc::new(claude::Claude::start(cfg, &repo.root).await?))
        }
        engine::Kind::Anthropic => {
            let native = anthropic::Anthropic::start(&repo.root, &cache_dir, model, timeout).await?;
            engine::Engine::Anthropic(Arc::new(native))
        }
    };
    let ex = Arc::new(explore::Explorer { repo, cache, engine });

    let commit = ex.repo.head().map_or("no git commit".to_owned(), |c| c[..c.len().min(12)].to_owned());
    println!("cxplr: {} ({commit}), engine {}, cache {}", ex.repo.root.display(), args.engine.id(), cache_dir.display());

    if args.forget_sessions {
        let sessions = ex.cache.sessions();
        // Codex refuses to delete a session while forks of it exist. Ids are
        // time-ordered, so newest first removes forks before their parents; further
        // passes retry whatever a remaining fork still blocked.
        let mut pending: Vec<(&String, &String)> = sessions.iter().rev().collect();
        let mut deleted = Vec::new();
        let mut errors = Vec::new();
        loop {
            errors.clear();
            let before = pending.len();
            let mut blocked = Vec::new();
            for (id, name) in pending {
                match ex.engine.delete_session(id).await {
                    Ok(()) => {
                        println!("  deleted {id} ({name})");
                        deleted.push(id.clone());
                    }
                    Err(e) => {
                        errors.push(format!("  ✗ {id} ({name}): {e}"));
                        blocked.push((id, name));
                    }
                }
            }
            pending = blocked;
            if pending.is_empty() || pending.len() == before {
                break;
            }
        }
        for e in &errors {
            println!("{e}");
        }
        ex.cache.forget_sessions(&deleted)?;
        println!("forgot {} of {} cxplr sessions; cached maps are kept", deleted.len(), sessions.len());
        return Ok(());
    }

    if args.index || args.index_only {
        let opts = index::Options { depth: args.depth.0, jobs: args.jobs.into(), min_lines: args.min_lines };
        let summary = index::run(ex.clone(), opts).await?;
        if args.index_only {
            rebuild_atlas(&ex).await;
            drop(ex); // stops codex app-server before exiting
            std::process::exit(i32::from(summary.failed > 0));
        }
    }
    // Freshness depends on the working tree, so refresh the atlas on every start.
    rebuild_atlas(&ex).await;

    let lsp = lsp::Lsp::new(&ex.repo.root, !args.no_lsp, &args.lsp, args.lsp_import_build);
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
    server::serve(listener, server::AppState::new(ex, lsp)).await?;
    Ok(())
}
