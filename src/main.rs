mod agent;
mod cache;
mod map;
mod prompt;
mod repo;
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
    /// Time limit for one exploration, in minutes
    #[arg(long, default_value_t = 20)]
    timeout_mins: u64,
    /// Where explored maps are cached (default: $XDG_CACHE_HOME/code-explore)
    #[arg(long)]
    cache_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let repo = repo::Repo::open(&args.repo)?;
    let base = args.cache_dir.unwrap_or_else(cache::default_base);
    let cache = cache::Cache::new(&base, &repo)?;
    let schema_path = base.join("map-schema.json");
    std::fs::write(&schema_path, serde_json::to_vec_pretty(&map::schema())?)?;
    let agent = agent::AgentConfig {
        bin: args.codex,
        model: args.model,
        timeout: Duration::from_secs(args.timeout_mins.max(1) * 60),
        schema_path,
    };

    let listener = tokio::net::TcpListener::bind((args.host.as_str(), args.port)).await?;
    let commit = repo.commit.as_deref().map_or("no git commit".to_owned(), |c| c[..c.len().min(12)].to_owned());
    println!("code-explore: {} ({commit})", repo.root.display());
    println!("open http://{}", listener.local_addr()?);
    server::serve(listener, server::AppState::new(repo, cache, agent)).await?;
    Ok(())
}
