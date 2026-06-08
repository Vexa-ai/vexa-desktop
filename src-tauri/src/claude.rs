//! Local Claude Code CLI integration.
//!
//! Detects the `claude` binary and runs it in headless streaming mode against the
//! knowledge vault (`cwd = vault`, writes scoped via `--add-dir`, `acceptEdits`).
//! stdout is JSON-lines (`--output-format stream-json`); we parse each line and
//! emit Tauri events (`claude-delta`, `claude-tool`, `claude-result`,
//! `claude-error`) so the UI can stream the assistant's work. The returned
//! `session_id` is persisted so chat follow-ups can `--resume`.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::storage;

#[derive(Debug, Clone, Serialize)]
pub struct ClaudeInfo {
    pub path: String,
    pub version: String,
}

/// Candidate locations — GUI apps on macOS don't inherit the shell PATH, so we
/// probe explicit paths in addition to `claude` on PATH.
fn candidates(explicit: Option<&str>) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    if let Some(e) = explicit {
        if !e.is_empty() {
            v.push(PathBuf::from(e));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        v.push(PathBuf::from(format!("{home}/.local/bin/claude")));
        v.push(PathBuf::from(format!("{home}/.claude/local/claude")));
    }
    v.push(PathBuf::from("claude"));
    v.push(PathBuf::from("/opt/homebrew/bin/claude"));
    v.push(PathBuf::from("/usr/local/bin/claude"));
    v
}

/// Find a working `claude` binary and its version.
pub fn detect(explicit: Option<&str>) -> Result<ClaudeInfo> {
    for cand in candidates(explicit) {
        if let Ok(out) = Command::new(&cand).arg("--version").output() {
            if out.status.success() {
                let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
                return Ok(ClaudeInfo {
                    path: cand.to_string_lossy().to_string(),
                    version,
                });
            }
        }
    }
    Err(anyhow!(
        "Claude Code CLI not found. Install it and run `claude` once to log in."
    ))
}

pub struct RunOpts {
    pub claude_path: String,
    pub vault: PathBuf,
    pub db_path: PathBuf,
    /// Our app session id — used to tag events and persist chat history.
    pub app_session: String,
    pub prompt: String,
    pub model: String,
    pub resume_sid: Option<String>,
    pub max_budget: Option<f64>,
    /// Extra system prompt (the vault's `agent.md`), appended to Claude's default.
    pub append_system: Option<String>,
    /// Routing tag for UI events: "chat" (assistant panel) or "story" (story tab).
    pub kind: String,
    /// Persist the claude session id + reply into chat history (chat only).
    pub persist: bool,
}

#[derive(Clone, Serialize)]
struct DeltaEvent {
    session: String,
    kind: String,
    text: String,
}
#[derive(Clone, Serialize)]
struct ToolEvent {
    session: String,
    kind: String,
    name: String,
    file: Option<String>,
}
#[derive(Clone, Serialize)]
struct ResultEvent {
    session: String,
    kind: String,
    text: String,
    cost_usd: Option<f64>,
}
#[derive(Clone, Serialize)]
struct ErrEvent {
    session: String,
    kind: String,
    message: String,
}

/// Spawn a Claude Code run on a background thread. Events stream to the UI; on
/// completion the claude session id + assistant reply are persisted.
pub fn spawn_run(app: AppHandle, opts: RunOpts) {
    std::thread::Builder::new()
        .name("vexa-claude".into())
        .spawn(move || {
            if let Err(e) = run(&app, &opts) {
                let _ = app.emit(
                    "claude-error",
                    ErrEvent {
                        session: opts.app_session.clone(),
                        kind: opts.kind.clone(),
                        message: format!("{e:#}"),
                    },
                );
            }
        })
        .ok();
}

/// Strip env that would hijack the spawned `claude`'s auth/endpoint so it always
/// uses the user's own Claude Code login (keychain/OAuth). Critical when the app
/// is launched from a shell that itself runs inside Claude Code.
fn scrub_env(cmd: &mut Command) {
    for (k, _) in std::env::vars() {
        let up = k.to_uppercase();
        if up.starts_with("CLAUDE")
            || up == "ANTHROPIC_BASE_URL"
            || up == "ANTHROPIC_API_KEY"
            || up == "ANTHROPIC_AUTH_TOKEN"
            || up == "ANTHROPIC_MODEL"
            || up == "ANTHROPIC_SMALL_FAST_MODEL"
        {
            cmd.env_remove(k);
        }
    }
}

/// One-shot Claude call (text output) that can Read images — used for speaker
/// naming from frames. Returns stdout.
pub fn run_sync(
    claude_path: &str,
    cwd: &Path,
    add_dir: &Path,
    model: &str,
    prompt: &str,
) -> Result<String> {
    let mut cmd = Command::new(claude_path);
    cmd.current_dir(cwd)
        .arg("-p")
        .arg("--add-dir")
        .arg(add_dir)
        .arg("--allowedTools")
        .arg("Read")
        .arg("--model")
        .arg(model)
        .arg(prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scrub_env(&mut cmd);
    let out = cmd.output().context("run claude (vision)")?;
    if !out.status.success() {
        return Err(anyhow!(
            "claude failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Minimal one-shot text completion: `claude -p --model <m> <prompt>` with no
/// tools or dir access. Used for transcript polishing (text in → markdown out).
pub fn run_prompt(claude_path: &str, model: &str, prompt: &str) -> Result<String> {
    let mut cmd = Command::new(claude_path);
    cmd.arg("-p")
        .arg("--model")
        .arg(model)
        .arg(prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scrub_env(&mut cmd);
    let out = cmd.output().context("run claude (polish)")?;
    if !out.status.success() {
        return Err(anyhow!(
            "claude failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn run(app: &AppHandle, opts: &RunOpts) -> Result<()> {
    let mut cmd = Command::new(&opts.claude_path);
    cmd.current_dir(&opts.vault)
        .arg("-p")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--add-dir")
        .arg(&opts.vault)
        .arg("--permission-mode")
        .arg("acceptEdits")
        .arg("--allowedTools")
        .arg("Read Write Edit Glob Grep")
        .arg("--model")
        .arg(&opts.model);
    if let Some(sys) = &opts.append_system {
        if !sys.trim().is_empty() {
            cmd.arg("--append-system-prompt").arg(sys);
        }
    }
    if let Some(sid) = &opts.resume_sid {
        cmd.arg("--resume").arg(sid);
    }
    if let Some(b) = opts.max_budget {
        cmd.arg("--max-budget-usd").arg(format!("{b}"));
    }
    cmd.arg(&opts.prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scrub_env(&mut cmd);

    let mut child = cmd.spawn().with_context(|| {
        format!("failed to launch claude at {}", opts.claude_path)
    })?;

    // Drain stderr on a thread so it can't deadlock the pipe.
    let stderr = child.stderr.take();
    let err_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut e) = stderr {
            use std::io::Read;
            let _ = e.read_to_string(&mut buf);
        }
        buf
    });

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("no stdout from claude"))?;
    let reader = BufReader::new(stdout);

    let mut claude_sid: Option<String> = None;
    let mut final_text = String::new();
    let mut cost: Option<f64> = None;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // ignore non-JSON noise
        };
        if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
            claude_sid = Some(sid.to_string());
        }
        match v.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                if let Some(content) = v
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                {
                    for item in content {
                        match item.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                                    let _ = app.emit(
                                        "claude-delta",
                                        DeltaEvent {
                                            session: opts.app_session.clone(),
                                            kind: opts.kind.clone(),
                                            text: t.to_string(),
                                        },
                                    );
                                }
                            }
                            Some("tool_use") => {
                                let name = item
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("tool")
                                    .to_string();
                                let file = item
                                    .get("input")
                                    .and_then(|i| {
                                        i.get("file_path").or_else(|| i.get("path"))
                                    })
                                    .and_then(|p| p.as_str())
                                    .map(|s| s.to_string());
                                let _ = app.emit(
                                    "claude-tool",
                                    ToolEvent {
                                        session: opts.app_session.clone(),
                                        kind: opts.kind.clone(),
                                        name,
                                        file,
                                    },
                                );
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some("result") => {
                final_text = v
                    .get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .to_string();
                cost = v.get("total_cost_usd").and_then(|c| c.as_f64());
            }
            _ => {}
        }
    }

    let status = child.wait().ok();
    let stderr_text = err_handle.join().unwrap_or_default();

    // Persist claude session id (for resume) + the assistant reply (chat only).
    if opts.persist {
        if let Ok(conn) = storage::open(&opts.db_path) {
            if let Some(sid) = &claude_sid {
                let _ = storage::set_claude_session(&conn, &opts.app_session, sid);
            }
            if !final_text.is_empty() {
                let now = chrono::Utc::now().to_rfc3339();
                let _ =
                    storage::insert_chat(&conn, &opts.app_session, "assistant", &final_text, &now);
            }
        }
    }

    let ok = status.map(|s| s.success()).unwrap_or(false);
    if !ok && final_text.is_empty() {
        return Err(anyhow!(
            "claude exited without a result{}",
            if stderr_text.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", stderr_text.trim())
            }
        ));
    }

    let _ = app.emit(
        "claude-result",
        ResultEvent {
            session: opts.app_session.clone(),
            kind: opts.kind.clone(),
            text: final_text,
            cost_usd: cost,
        },
    );
    Ok(())
}
