mod anthropic;
mod anthropic_auth;
mod anthropic_wire;
mod ask;
mod atlas;
mod cache;
mod claude;
mod codex;
mod config;
mod configure;
mod engine;
mod explore;
mod index;
mod lsp;
mod map;
mod native;
mod openai;
mod prompt;
mod repo;
mod repo_tools;
mod search;
mod server;
mod skill;
mod term;
mod trace;
mod update;
mod verify;
mod walk;

use clap::Parser;
use std::path::{Path, PathBuf};
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
    /// Agent: codex app-server, claude -p, native Anthropic Messages, or an
    /// OpenAI-compatible Chat Completions server (default: the configured engine,
    /// else codex)
    #[arg(long, value_enum)]
    engine: Option<engine::Kind>,
    /// Model (default: the configured model; CLI engines use their own config,
    /// native Anthropic claude-sonnet-5-5, openai $OPENAI_MODEL or the server's only model)
    #[arg(long)]
    model: Option<String>,
    /// Reasoning effort for indexing (default: the configured effort, else the
    /// engine's own). Codex: a level the model offers (e.g. low, medium, high);
    /// claude: low, medium, high, xhigh, max; anthropic: off, low, medium, high,
    /// xhigh, max (thinking budget); openai: sent as `reasoning_effort`
    #[arg(long)]
    effort: Option<String>,
    /// OpenAI-compatible API root for --engine openai (also $OPENAI_BASE_URL; a bare
    /// host gets /v1). The API key, if the server needs one, comes from $OPENAI_API_KEY
    #[arg(long)]
    openai_base_url: Option<String>,
    /// Engine for questions, example inputs and walkthroughs (default: --engine). With
    /// another engine or model than indexing, their sessions start seeded with the
    /// atlas instead of forking the map's session
    #[arg(long, value_enum)]
    ask_engine: Option<engine::Kind>,
    /// Model for questions, example inputs and walkthroughs (default: --model when
    /// the ask engine is the indexing engine, else that engine's default)
    #[arg(long)]
    ask_model: Option<String>,
    /// Reasoning effort for questions, example inputs and walkthroughs (default:
    /// --effort when the ask engine is the indexing engine). Another effort alone
    /// still forks the map's session, which keeps what it read
    #[arg(long)]
    ask_effort: Option<String>,
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
    /// Set up the global configuration interactively (engines, models, OpenAI server)
    /// and exit; also offered on the first interactive launch without one
    #[arg(long)]
    configure: bool,
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

/// Engine settings shared by the indexing and ask engines, already resolved from
/// flags, the environment and the configuration.
struct EngineOpts {
    codex: String,
    claude: String,
    openai_base_url: String,
    openai_api_key: Option<String>,
    timeout: Duration,
}

async fn start_engine(kind: engine::Kind, model: Option<String>, effort: Option<String>, o: &EngineOpts, root: &Path, cache_dir: &Path) -> Result<engine::Engine, String> {
    let timeout = o.timeout;
    Ok(match kind {
        engine::Kind::Codex => {
            let cfg = codex::CodexConfig { bin: o.codex.clone(), model, effort, timeout };
            engine::Engine::Codex(codex::Codex::start(cfg, root).await?)
        }
        engine::Kind::Claude => {
            let cfg = claude::ClaudeConfig { bin: o.claude.clone(), model, effort, timeout };
            engine::Engine::Claude(Arc::new(claude::Claude::start(cfg, root).await?))
        }
        engine::Kind::Anthropic => {
            let model = model.or_else(|| Some(anthropic::DEFAULT_MODEL.to_owned()));
            engine::Engine::Anthropic(Arc::new(anthropic::Anthropic::start(root, cache_dir, model, effort, timeout).await?))
        }
        engine::Kind::OpenAi => {
            let cfg = openai::OpenAiConfig { base_url: o.openai_base_url.clone(), api_key: o.openai_api_key.clone(), model, effort, timeout };
            engine::Engine::OpenAi(Arc::new(openai::OpenAi::start(root, cache_dir, cfg).await?))
        }
    })
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
    if args.configure {
        if !term::interactive() {
            return Err("--configure asks questions: run it in a terminal".into());
        }
        configure::run(config::load()?).await?;
        return Ok(());
    }
    if args.update {
        // The new binary refreshes an installed skill on its first start.
        println!("{}", update::update_now().await?);
        return Ok(());
    }
    let mut cfg = config::load()?;
    // First interactive launch without a configuration: set one up, then carry on.
    // Explicit engine flags, one-off actions and non-interactive runs skip it.
    let engine_flags = args.engine.is_some()
        || args.model.is_some()
        || args.effort.is_some()
        || args.ask_engine.is_some()
        || args.ask_model.is_some()
        || args.ask_effort.is_some()
        || args.openai_base_url.is_some();
    if cfg.is_none() && !engine_flags && !args.install_skill && !args.forget_sessions && term::interactive() {
        println!("No cxplr configuration yet; let's set it up first (`cxplr --configure` changes it later).\n");
        cfg = Some(configure::run(None).await?);
    }
    let cfg = cfg.unwrap_or_default();
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let index_flags = config::Flags { engine: args.engine, model: args.model.clone(), effort: args.effort };
    let ask_flags = config::Flags { engine: args.ask_engine, model: args.ask_model.clone(), effort: args.ask_effort };
    let config::Choice { mut index, mut ask } = config::choose(&cfg, index_flags, ask_flags);
    // $OPENAI_MODEL sits between flags and the configuration, like the other OpenAI variables.
    if index.engine == engine::Kind::OpenAi && args.model.is_none() {
        index.model = env("OPENAI_MODEL").or(index.model);
    }
    if ask.engine == engine::Kind::OpenAi && ask.engine != index.engine && args.ask_model.is_none() {
        ask.model = env("OPENAI_MODEL").or(ask.model);
    }
    if args.install_skill {
        let path = skill::install(index.engine).map_err(|e| e.to_string())?;
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
    let opts = EngineOpts {
        codex: args.codex,
        claude: args.claude,
        openai_base_url: args.openai_base_url.or_else(|| env("OPENAI_BASE_URL")).or(cfg.openai_base_url).unwrap_or_else(|| openai::DEFAULT_BASE_URL.to_owned()),
        openai_api_key: env("OPENAI_API_KEY").or(cfg.openai_api_key),
        timeout: Duration::from_secs(args.timeout_mins.max(1) * 60),
    };
    let engine = start_engine(index.engine, index.model, index.effort, &opts, &repo.root, &cache_dir).await?;
    // The ask side gets its own engine when anything differs; on the same engine,
    // unset ask settings mean the indexing engine's.
    let same_engine = ask.engine == index.engine;
    let model_differs = ask.model.as_deref().is_some_and(|m| Some(m) != engine.model());
    let effort_differs = ask.effort.is_some() && ask.effort.as_deref() != engine.effort();
    let asker = if same_engine && !model_differs && !effort_differs {
        None
    } else {
        let inherit = |own: Option<String>, from: Option<&str>| if same_engine { own.or_else(|| from.map(str::to_owned)) } else { own };
        let model = inherit(ask.model, engine.model());
        let effort = inherit(ask.effort, engine.effort());
        Some(start_engine(ask.engine, model, effort, &opts, &repo.root, &cache_dir).await?)
    };
    // Keys include the model the engine actually uses (the openai engine may pick it).
    let cache = cache::Cache::open(cache_dir.clone(), index.engine, engine.model())
        .map_err(|e| format!("cache {}: {e} (use --cache-dir for read-only checkouts)", cache_dir.display()))?;
    let ex = Arc::new(explore::Explorer { repo, cache, engine, asker });

    let commit = ex.repo.head().map_or("no git commit".to_owned(), |c| c[..c.len().min(12)].to_owned());
    let describe = |e: &engine::Engine| {
        let details: Vec<String> = [e.model().map(str::to_owned), e.effort().map(|x| format!("effort {x}"))].into_iter().flatten().collect();
        if details.is_empty() { e.kind().id().to_owned() } else { format!("{} ({})", e.kind().id(), details.join(", ")) }
    };
    let asks = ex.asker.as_ref().map_or(String::new(), |a| format!(", asks {}", describe(a)));
    println!("cxplr: {} ({commit}), engine {}{asks}, cache {}", ex.repo.root.display(), describe(&ex.engine), cache_dir.display());

    if args.forget_sessions {
        // Each engine's registry once (an ask engine of the same kind shares it).
        let engines = std::iter::once(&ex.engine).chain(ex.asker.iter().filter(|a| a.kind() != ex.engine.kind()));
        for engine in engines {
            let sessions = ex.cache.sessions(engine.kind());
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
                    match engine.delete_session(id).await {
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
            ex.cache.forget_sessions(engine.kind(), &deleted)?;
            println!("forgot {} of {} cxplr {} sessions; cached maps are kept", deleted.len(), sessions.len(), engine.kind().id());
        }
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
