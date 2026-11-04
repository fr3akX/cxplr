//! Updates for a cxplr installed with `cargo install --git`. Cargo records the
//! commit it built from; when GitHub's `main` has moved on, cxplr reinstalls itself
//! in the background and the new binary is used from the next start.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs};

pub const REPO_URL: &str = "https://github.com/fr3akX/cxplr";
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
const REMOTE_TIMEOUT: Duration = Duration::from_secs(8);

fn cargo_home() -> Option<PathBuf> {
    env::var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
}

fn state_dir() -> Option<PathBuf> {
    env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .map(|d| d.join("cxplr"))
}

/// The commit cargo installed this crate from, from `.crates2.json` content.
fn installed_commit_in(crates2: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(crates2).ok()?;
    let source = format!("(git+{REPO_URL}");
    v["installs"].as_object()?.keys().find_map(|k| {
        let rest = k.strip_prefix("cxplr ")?;
        let at = rest.find(&source)?;
        let sha = rest[at..].split('#').nth(1)?.trim_end_matches(')');
        (sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit())).then(|| sha.to_owned())
    })
}

/// `Some(commit)` when the running binary is the one `cargo install --git` put in
/// `$CARGO_HOME/bin`; builds from a checkout are never updated.
fn installed_commit() -> Option<String> {
    let home = cargo_home()?;
    let installed = home.join("bin").join(format!("cxplr{}", env::consts::EXE_SUFFIX)).canonicalize().ok()?;
    if env::current_exe().ok()?.canonicalize().ok()? != installed {
        return None;
    }
    installed_commit_in(&fs::read_to_string(home.join(".crates2.json")).ok()?)
}

fn remote_main() -> Option<String> {
    let mut child = Command::new("git")
        .args(["ls-remote", REPO_URL, "refs/heads/main"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = std::time::Instant::now();
    loop {
        if child.try_wait().ok()?.is_some() {
            break;
        }
        if started.elapsed() > REMOTE_TIMEOUT {
            let _ = child.kill();
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = child.wait_with_output().ok()?;
    let sha = String::from_utf8_lossy(&out.stdout).split_whitespace().next()?.to_owned();
    (sha.len() == 40).then_some(sha)
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    checked_at: u64,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn install_command() -> Command {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.args(["install", "--git", REPO_URL, "--locked", "--force", "cxplr"]);
    cmd
}

/// Check (at most every few hours) and, when `main` has moved on, reinstall in the
/// background. Returns a message for the user when something was started.
pub fn check_in_background() -> Option<String> {
    if env::var_os("CXPLR_NO_UPDATE").is_some_and(|v| !v.is_empty() && v != "0") {
        return None;
    }
    let installed = installed_commit()?;
    let dir = state_dir()?;
    let state_file = dir.join("update.json");
    let state: State = fs::read(&state_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    if now().saturating_sub(state.checked_at) < CHECK_EVERY.as_secs() {
        return None;
    }
    // Record the attempt first so concurrent or failing runs do not retry in a loop.
    fs::create_dir_all(&dir).ok()?;
    fs::write(&state_file, serde_json::to_vec(&State { checked_at: now() }).ok()?).ok()?;
    let remote = remote_main()?;
    if remote == installed {
        return None;
    }
    let log_path = dir.join("update.log");
    let log = fs::File::create(&log_path).ok()?;
    let mut cmd = install_command();
    cmd.stdin(Stdio::null()).stdout(log.try_clone().ok()?).stderr(log);
    // Own process group: a Ctrl-C in this terminal must not interrupt the build.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd.spawn().ok()?;
    Some(format!(
        "updating cxplr {} → {} in the background (log: {}); the new version is used from the next start",
        &installed[..7],
        &remote[..7],
        log_path.display()
    ))
}

/// `cxplr --update`: reinstall the latest `main` now, showing cargo's output.
pub fn update_now() -> Result<(), String> {
    let status = install_command().status().map_err(|e| format!("cannot run cargo: {e}"))?;
    if status.success() {
        if let Some(dir) = state_dir() {
            let _ = fs::create_dir_all(&dir);
            let _ = fs::write(dir.join("update.json"), serde_json::to_vec(&State { checked_at: now() }).unwrap_or_default());
        }
        Ok(())
    } else {
        Err(format!("cargo install failed ({status})"))
    }
}

#[cfg(test)]
mod tests {
    use super::installed_commit_in;

    #[test]
    fn reads_the_git_commit_cargo_installed_from() {
        let crates2 = r#"{"installs":{
            "ripgrep 14.1.0 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["rg"]},
            "cxplr 0.1.0 (git+https://github.com/fr3akX/cxplr#94c3cf5a1b2c3d4e5f60718293a4b5c6d7e8f901)":{"bins":["cxplr"]}
        }}"#;
        assert_eq!(installed_commit_in(crates2).as_deref(), Some("94c3cf5a1b2c3d4e5f60718293a4b5c6d7e8f901"));
        // Installed from a local path or another fork: not ours to update.
        assert_eq!(installed_commit_in(r#"{"installs":{"cxplr 0.1.0 (path+file:///src/cxplr)":{}}}"#), None);
        assert_eq!(installed_commit_in(r#"{"installs":{"cxplr 0.1.0 (git+https://github.com/other/cxplr#94c3cf5a)":{}}}"#), None);
    }
}
