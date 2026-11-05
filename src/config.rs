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
    /// Reasoning effort for `engine`; absent: the engine's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Engine for questions, example inputs and walkthroughs; absent: `engine`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_engine: Option<Kind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_effort: Option<String>,
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

/// One role's settings from the command line (`--engine`/`--model`/`--effort` or
/// `--ask-engine`/`--ask-model`/`--ask-effort`).
#[derive(Debug, Default)]
pub struct Flags {
    pub engine: Option<Kind>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// One role's engine, model and reasoning effort for this run (`None`: the engine's
/// default; on the ask side, the indexing engine's setting when it is the same engine).
#[derive(Debug, PartialEq)]
pub struct Pick {
    pub engine: Kind,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct Choice {
    pub index: Pick,
    pub ask: Pick,
}

/// Combine command-line flags with the configuration. A configured model or effort
/// only applies to the engine it was configured for, so `--engine claude` does not
/// inherit the model of a configured `openai` engine.
pub fn choose(cfg: &Config, index: Flags, ask: Flags) -> Choice {
    let cfg_engine = cfg.engine.unwrap_or(Kind::Codex);
    let engine = index.engine.unwrap_or(cfg_engine);
    let own = engine == cfg_engine;
    let configured = |v: &Option<String>, applies: bool| applies.then(|| v.clone()).flatten();
    let index = Pick {
        engine,
        model: index.model.or_else(|| configured(&cfg.model, own)),
        effort: index.effort.or_else(|| configured(&cfg.effort, own)),
    };
    let ask_engine = ask.engine.or(cfg.ask_engine).unwrap_or(engine);
    // The configured ask settings belong to the configured ask engine, or (without one)
    // to the configured indexing engine.
    let cfg_ask_engine = cfg.ask_engine.unwrap_or(cfg_engine);
    let applies = ask_engine == cfg_ask_engine && (cfg.ask_engine.is_some() || own);
    let ask = Pick {
        engine: ask_engine,
        model: ask.model.or_else(|| configured(&cfg.ask_model, applies)),
        effort: ask.effort.or_else(|| configured(&cfg.ask_effort, applies)),
    };
    Choice { index, ask }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    fn pick(engine: Kind, model: Option<&str>, effort: Option<&str>) -> Pick {
        Pick { engine, model: model.map(str::to_owned), effort: effort.map(str::to_owned) }
    }

    fn flags(engine: Option<Kind>, model: Option<&str>, effort: Option<&str>) -> Flags {
        Flags { engine, model: model.map(str::to_owned), effort: effort.map(str::to_owned) }
    }

    #[test]
    fn flags_override_and_settings_stay_with_their_engine() {
        let c = cfg("engine = \"openai\"\nmodel = \"qwen\"\neffort = \"low\"\nask_engine = \"codex\"\nask_model = \"gpt-6.1-sol\"\nask_effort = \"high\"\n");
        // Configured as is.
        assert_eq!(
            choose(&c, Flags::default(), Flags::default()),
            Choice { index: pick(Kind::OpenAi, Some("qwen"), Some("low")), ask: pick(Kind::Codex, Some("gpt-6.1-sol"), Some("high")) }
        );
        // Another indexing engine drops the configured model and effort but keeps the ask side.
        let other = choose(&c, flags(Some(Kind::Claude), None, None), Flags::default());
        assert_eq!(other.index, pick(Kind::Claude, None, None));
        assert_eq!(other.ask, pick(Kind::Codex, Some("gpt-6.1-sol"), Some("high")));
        // Flags win.
        let set = choose(&c, flags(None, Some("big"), Some("medium")), flags(Some(Kind::OpenAi), Some("small"), Some("none")));
        assert_eq!(set.index, pick(Kind::OpenAi, Some("big"), Some("medium")));
        assert_eq!(set.ask, pick(Kind::OpenAi, Some("small"), Some("none")));
        // Another ask engine drops the configured ask model and effort.
        assert_eq!(choose(&c, Flags::default(), flags(Some(Kind::Claude), None, None)).ask, pick(Kind::Claude, None, None));
    }

    #[test]
    fn ask_settings_without_an_ask_engine_belong_to_the_indexing_engine() {
        let c = cfg("engine = \"claude\"\nmodel = \"sonnet\"\nask_model = \"opus\"\nask_effort = \"max\"\n");
        assert_eq!(choose(&c, Flags::default(), Flags::default()).ask, pick(Kind::Claude, Some("opus"), Some("max")));
        assert_eq!(choose(&c, flags(Some(Kind::Codex), None, None), Flags::default()).ask, pick(Kind::Codex, None, None));
        // No configuration: Codex with its own defaults.
        assert_eq!(
            choose(&Config::default(), Flags::default(), Flags::default()),
            Choice { index: pick(Kind::Codex, None, None), ask: pick(Kind::Codex, None, None) }
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
