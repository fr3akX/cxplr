mod agent;
mod ask;
mod cache;
mod lsp;
mod map;
mod prompt;
mod repo;
mod search;
mod server;
mod verify;

use clap::Parser;
use std::path::PathBuf;
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
    /// Where explored maps are cached (default: $XDG_CACHE_HOME/code-explore)
    #[arg(long)]
    cache_dir: Option<PathBuf>,
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let repo = repo::Repo::open(&args.repo)?;
    let base = args.cache_dir.unwrap_or_else(cache::default_base);
    let cache = cache::Cache::new(&base, &repo)?;
    let schemas = server::Schemas { map: base.join("map-schema.json"), ask: base.join("ask-schema.json") };
    std::fs::write(&schemas.map, serde_json::to_vec_pretty(&map::schema())?)?;
    std::fs::write(&schemas.ask, serde_json::to_vec_pretty(&ask::schema())?)?;
    let agent = agent::AgentConfig {
        bin: args.codex,
        model: args.model,
        timeout: Duration::from_secs(args.timeout_mins.max(1) * 60),
    };
    let lsp = lsp::Lsp::new(&repo.root, !args.no_lsp, &args.lsp);

    let listener = tokio::net::TcpListener::bind((args.host.as_str(), args.port)).await?;
    let commit = repo.commit.as_deref().map_or("no git commit".to_owned(), |c| c[..c.len().min(12)].to_owned());
    println!("code-explore: {} ({commit})", repo.root.display());
    let servers = lsp.configured();
    if servers.is_empty() {
        println!("language servers: none (navigation uses text search)");
    }
    for (lang, cmd) in servers {
        println!("language server: {lang} = {cmd}");
    }
    println!("open http://{}", listener.local_addr()?);
    server::serve(listener, server::AppState::new(repo, cache, agent, lsp, schemas)).await?;
    Ok(())
}
