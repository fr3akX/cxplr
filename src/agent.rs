//! Runs `codex exec` as a read-only exploration agent inside the target
//! repository and returns its schema-constrained map.

use crate::map::Map;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Notify;

/// Integrations the explorer never needs. Disabling them keeps user hooks,
/// plugins and browser/computer tools out of runs over untrusted repositories.
const DISABLED_FEATURES: &[&str] =
    &["hooks", "plugins", "apps", "memories", "browser_use", "computer_use", "in_app_browser"];

const STDERR_TAIL: usize = 8 * 1024;

pub struct AgentConfig {
    pub bin: String,
    pub model: Option<String>,
    pub timeout: Duration,
    pub schema_path: PathBuf,
}

pub struct Outcome {
    pub map: Map,
    pub commands: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Default)]
struct Stream {
    last_message: Option<String>,
    failure: Option<String>,
    last_error: Option<String>,
    commands: u32,
    input_tokens: u64,
    output_tokens: u64,
}

impl Stream {
    fn handle(&mut self, line: &str, progress: &mut impl FnMut(String)) {
        let Ok(event) = serde_json::from_str::<Value>(line) else { return };
        let item = &event["item"];
        match event["type"].as_str().unwrap_or_default() {
            "item.started" if item["type"] == "command_execution" => {
                self.commands += 1;
                progress(format!("$ {}", unwrap_shell(item["command"].as_str().unwrap_or_default())));
            }
            "item.completed" => {
                let text = item["text"].as_str().unwrap_or_default();
                match item["type"].as_str().unwrap_or_default() {
                    "agent_message" => {
                        if !text.trim_start().starts_with('{') {
                            progress(format!("› {}", one_line(text, 300)));
                        }
                        self.last_message = Some(text.to_owned());
                    }
                    "reasoning" if !text.is_empty() => progress(format!("… {}", one_line(text, 300))),
                    _ => {}
                }
            }
            "turn.completed" => {
                let usage = &event["usage"];
                self.input_tokens += usage["input_tokens"].as_u64().unwrap_or(0);
                self.output_tokens += usage["output_tokens"].as_u64().unwrap_or(0);
            }
            "turn.failed" => {
                let msg = event["error"]["message"].as_str().map(str::to_owned);
                self.failure = Some(msg.unwrap_or_else(|| event.to_string()));
            }
            // Codex also reports transient conditions (reconnects) as errors;
            // only a failed turn or missing answer ends the run.
            "error" => {
                let msg = event["message"].as_str().unwrap_or_default().to_owned();
                progress(format!("! {msg}"));
                self.last_error = Some(msg);
            }
            _ => {}
        }
    }
}

/// `/usr/bin/zsh -lc 'rg -n '"'"'fn main'"'"''` → `rg -n 'fn main'`: decode the
/// single POSIX shell word after `-lc` so progress shows the command as written.
fn unwrap_shell(command: &str) -> String {
    let Some(i) = command.find(" -lc ") else { return command.to_owned() };
    let mut chars = command[i + 5..].trim_start().chars();
    let mut word = String::new();
    while let Some(c) = chars.next() {
        match c {
            '\'' => word.extend(chars.by_ref().take_while(|&c| c != '\'')),
            '"' => {
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some(e @ ('$' | '`' | '"' | '\\')) => word.push(e),
                            Some('\n') | None => {}
                            Some(e) => word.extend(['\\', e]),
                        },
                        c => word.push(c),
                    }
                }
            }
            '\\' => word.extend(chars.next()),
            c if c.is_whitespace() => break,
            c => word.push(c),
        }
    }
    word
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}

pub async fn explore(
    cfg: &AgentConfig,
    root: &Path,
    prompt: &str,
    cancel: &Notify,
    mut progress: impl FnMut(String),
) -> Result<Outcome, String> {
    let mut cmd = Command::new(&cfg.bin);
    cmd.args(["exec", "--json", "--color", "never", "--sandbox", "read-only"])
        .args(["-c", "approval_policy=\"never\"", "--skip-git-repo-check", "--ephemeral"]);
    for feature in DISABLED_FEATURES {
        cmd.args(["--disable", feature]);
    }
    cmd.arg("--cd").arg(root).arg("--output-schema").arg(&cfg.schema_path);
    if let Some(model) = &cfg.model {
        cmd.args(["--model", model]);
    }
    cmd.arg("-")
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| format!("cannot start `{}`: {e}", cfg.bin))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stdout = child.stdout.take().expect("piped stdout");

    let stderr_tail = tokio::spawn(async move {
        let mut tail = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Ok(n @ 1..) = stderr.read(&mut chunk).await {
            tail.extend_from_slice(&chunk[..n]);
            if tail.len() > STDERR_TAIL {
                tail.drain(..tail.len() - STDERR_TAIL);
            }
        }
        String::from_utf8_lossy(&tail).into_owned()
    });

    stdin.write_all(prompt.as_bytes()).await.map_err(|e| format!("cannot send prompt: {e}"))?;
    drop(stdin);

    let mut stream = Stream::default();
    let mut lines = BufReader::new(stdout).lines();
    let deadline = tokio::time::sleep(cfg.timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => stream.handle(&line, &mut progress),
                Ok(None) => break,
                Err(e) => {
                    let _ = child.kill().await;
                    return Err(format!("reading codex output: {e}"));
                }
            },
            _ = cancel.notified() => {
                let _ = child.kill().await;
                return Err("cancelled".into());
            }
            _ = &mut deadline => {
                let _ = child.kill().await;
                return Err(format!("timed out after {} minutes", cfg.timeout.as_secs() / 60));
            }
        }
    }

    let status = child.wait().await.map_err(|e| format!("waiting for codex: {e}"))?;
    let stderr_text = stderr_tail.await.unwrap_or_default();
    if let Some(failure) = stream.failure {
        return Err(format!("codex turn failed: {failure}"));
    }
    let Some(answer) = stream.last_message.filter(|_| status.success()) else {
        let detail = stream.last_error.unwrap_or_else(|| stderr_text.trim().to_owned());
        return Err(format!("codex exited ({status}) without an answer: {detail}"));
    };
    let map = serde_json::from_str(answer.trim())
        .map_err(|e| format!("codex answer is not a valid map ({e}): {}", one_line(&answer, 400)))?;
    Ok(Outcome {
        map,
        commands: stream.commands,
        input_tokens: stream.input_tokens,
        output_tokens: stream.output_tokens,
    })
}
