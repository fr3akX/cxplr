//! Global configuration: `<config dir>/config.toml`, written by `cxplr --configure`
//! (or the first interactive launch). Command-line flags override it, and the usual
//! environment variables (`OPENAI_BASE_URL`, `OPENAI_API_KEY`, `OPENAI_MODEL`)
//! override its OpenAI settings.

use crate::engine::Kind;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

const HEADER: &str = "# cxplr configuration, written by `cxplr --configure`.\n# Command-line flags override these settings.\n\n";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Engine that makes maps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<Kind>,
    /// Model for `engine`; absent: the engine's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Engine for questions, example inputs and walkthroughs; absent: `engine`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_engine: Option<Kind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_base_url: Option<String>,
    /// Stored in a private (0600) file; `$OPENAI_API_KEY` wins when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_api_key: Option<String>,
}

/// `$XDG_CONFIG_HOME/cxplr`, else `$HOME/.config/cxplr`.
pub fn dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or("cannot locate a config directory: set XDG_CONFIG_HOME or HOME")?;
    Ok(base.join("cxplr"))
}

pub fn path() -> Result<PathBuf, String> {
    Ok(dir()?.join("config.toml"))
}

/// The saved configuration, or `None` when there is none yet.
pub fn load() -> Result<Option<Config>, String> {
    let path = path()?;
    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map(Some).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Write the configuration atomically, readable only by the user (it may hold a key).
pub fn save(config: &Config) -> Result<PathBuf, String> {
    let path = path()?;
    let dir = path.parent().expect("config path has a parent");
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let text = format!("{HEADER}{}", toml::to_string(config).map_err(|e| e.to_string())?);
    let tmp = dir.join(format!(".config.toml.{}", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.flush()?;
        fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Engines and models for one run.
#[derive(Debug, PartialEq)]
pub struct Choice {
    pub engine: Kind,
    pub model: Option<String>,
    pub ask_engine: Kind,
    pub ask_model: Option<String>,
}

/// Combine command-line flags with the configuration. A configured model only
/// applies to the engine it was configured for, so `--engine claude` does not
/// inherit the model of a configured `openai` engine.
pub fn choose(cfg: &Config, engine: Option<Kind>, model: Option<String>, ask_engine: Option<Kind>, ask_model: Option<String>) -> Choice {
    let cfg_engine = cfg.engine.unwrap_or(Kind::Codex);
    let engine = engine.unwrap_or(cfg_engine);
    let model = model.or_else(|| (engine == cfg_engine).then(|| cfg.model.clone()).flatten());
    let ask_engine = ask_engine.or(cfg.ask_engine).unwrap_or(engine);
    // The configured ask model belongs to the configured ask engine, or (without one)
    // to the configured indexing engine.
    let cfg_ask_engine = cfg.ask_engine.unwrap_or(cfg_engine);
    let applies = ask_engine == cfg_ask_engine && (cfg.ask_engine.is_some() || engine == cfg_engine);
    let ask_model = ask_model.or_else(|| applies.then(|| cfg.ask_model.clone()).flatten());
    Choice { engine, model, ask_engine, ask_model }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn flags_override_and_models_stay_with_their_engine() {
        let c = cfg("engine = \"openai\"\nmodel = \"qwen\"\nask_engine = \"codex\"\nask_model = \"gpt-6.1-sol\"\n");
        // Configured as is.
        assert_eq!(
            choose(&c, None, None, None, None),
            Choice { engine: Kind::OpenAi, model: Some("qwen".into()), ask_engine: Kind::Codex, ask_model: Some("gpt-6.1-sol".into()) }
        );
        // Another indexing engine drops the configured model but keeps the ask side.
        let other = choose(&c, Some(Kind::Claude), None, None, None);
        assert_eq!((other.engine, other.model.as_deref(), other.ask_engine), (Kind::Claude, None, Kind::Codex));
        assert_eq!(other.ask_model.as_deref(), Some("gpt-6.1-sol"));
        // Flags win.
        let flags = choose(&c, None, Some("big".into()), Some(Kind::OpenAi), Some("small".into()));
        assert_eq!((flags.model.as_deref(), flags.ask_engine, flags.ask_model.as_deref()), (Some("big"), Kind::OpenAi, Some("small")));
        // Another ask engine drops the configured ask model.
        assert_eq!(choose(&c, None, None, Some(Kind::Claude), None).ask_model, None);
    }

    #[test]
    fn an_ask_model_without_an_ask_engine_belongs_to_the_indexing_engine() {
        let c = cfg("engine = \"claude\"\nmodel = \"sonnet\"\nask_model = \"opus\"\n");
        let same = choose(&c, None, None, None, None);
        assert_eq!((same.ask_engine, same.ask_model.as_deref()), (Kind::Claude, Some("opus")));
        let moved = choose(&c, Some(Kind::Codex), None, None, None);
        assert_eq!((moved.ask_engine, moved.ask_model), (Kind::Codex, None));
        // No configuration: Codex with its own defaults.
        assert_eq!(
            choose(&Config::default(), None, None, None, None),
            Choice { engine: Kind::Codex, model: None, ask_engine: Kind::Codex, ask_model: None }
        );
    }

    #[test]
    fn saved_files_round_trip_and_unknown_engines_are_rejected() {
        let c = Config { engine: Some(Kind::OpenAi), openai_base_url: Some("http://h:8080".into()), ..Config::default() };
        let text = toml::to_string(&c).unwrap();
        assert_eq!(text, "engine = \"openai\"\nopenai_base_url = \"http://h:8080\"\n");
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), c);
        assert!(toml::from_str::<Config>("engine = \"gpt\"\n").is_err());
    }
}
