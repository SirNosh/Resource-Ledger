//! Agent hook handlers: `resource-ledger hook <client> <event>`, with the hook's JSON on stdin.
//!
//! Claude Code: start (SessionStart), pre (PreToolUse), post (PostToolUse),
//! stop (Stop, run with asyncRewake), end (SessionEnd).
//! Codex: start, post, stop (a synchronous long-poll).
//! OpenCode: intro --session KEY and poll --session KEY (called by the plugin).
//!
//! A hook must never break the agent: any error is logged to hook-errors.log and
//! the hook exits 0 with no output. Unknown events are no-ops.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::panic::{self, AssertUnwindSafe};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::ledger::{self, Polled, err};

const POLL: Duration = Duration::from_secs(5);
const CLAUDE_WAIT: Duration = Duration::from_secs(23 * 3600 + 1800); // hook timeout is 24 h
const CODEX_WAIT: Duration = Duration::from_secs(3500); // hook timeout is 3600 s

pub fn run(client: &str, event: &str, session: Option<&str>) -> i32 {
    panic::set_hook(Box::new(|info| log(&format!("panic: {info}"))));
    match panic::catch_unwind(AssertUnwindSafe(|| dispatch(client, event, session))) {
        Ok(Ok(code)) => code,
        Ok(Err(e)) => {
            log(&format!("{client} {event}: {e}"));
            0
        }
        Err(_) => 0,
    }
}

fn dispatch(client: &str, event: &str, session: Option<&str>) -> Result<i32, String> {
    if client == "opencode" {
        match (event, session) {
            ("intro", Some(key)) => println!("{}", intro(key, true)),
            ("poll", Some(key)) => {
                let p = ledger::with(|st| Ok(st.poll(ledger::now(), key)))?;
                println!("{}", json!({"notices": p.notices, "open": p.open}));
            }
            _ => {}
        }
        return Ok(0);
    }
    let mut raw = Vec::new();
    std::io::stdin().read_to_end(&mut raw).map_err(err)?;
    let raw = String::from_utf8_lossy(&raw);
    let data: Value = if raw.trim().is_empty() { Value::Null } else { serde_json::from_str(&raw).map_err(err)? };
    let Some(sid) = data["session_id"].as_str().filter(|s| !s.is_empty()) else { return Ok(0) };
    match client {
        "claude" => claude(event, &format!("claude:{sid}"), &data),
        "codex" => codex(event, &format!("codex:{sid}")),
        _ => Ok(0),
    }
}

fn claude(event: &str, key: &str, data: &Value) -> Result<i32, String> {
    match event {
        "start" => emit("SessionStart", &intro(key, true)),
        "pre" => {
            if let Some(input) = stamp(data["tool_name"].as_str().unwrap_or(""), &data["tool_input"], key) {
                println!("{}", json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "updatedInput": input}}));
            }
        }
        "post" => emit("PostToolUse", &notices(key)?),
        "stop" => {
            // Runs in the background (asyncRewake): exit 2 wakes the session with stderr.
            let home = ledger::home();
            fs::create_dir_all(&home).map_err(err)?;
            let name: String = key.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
            let lock = OpenOptions::new().create(true).write(true).truncate(false).open(home.join(format!("wait-{name}.lock"))).map_err(err)?;
            match lock.try_lock() {
                Ok(()) => {}
                Err(fs::TryLockError::WouldBlock) => return Ok(0), // this session already has a live waiter
                Err(fs::TryLockError::Error(e)) => return Err(err(e)),
            }
            let text = match wait(key, CLAUDE_WAIT, |p| p.open)? {
                None => return Ok(0),
                Some(n) if !n.is_empty() => n.join("\n"),
                Some(_) => format!(
                    "[resource-ledger] Still holding or waiting on reservations:\n{}\nNothing to do yet; end your turn to keep waiting.",
                    ledger::with(|st| Ok(st.lines_for(ledger::now(), key)))?),
            };
            eprintln!("{text}");
            return Ok(2);
        }
        "end" => ledger::with(|st| {
            st.end_session(ledger::now(), key);
            Ok(())
        })?,
        _ => {}
    }
    Ok(0)
}

fn codex(event: &str, key: &str) -> Result<i32, String> {
    match event {
        "start" => emit("SessionStart", &intro(key, false)),
        "post" => emit("PostToolUse", &notices(key)?),
        "stop" => {
            // Synchronous: hold the turn open only while waiting for a grant, never for
            // a running grant (that would block the session for the grant's whole length).
            let out = match wait(key, CODEX_WAIT, |p| p.queued)? {
                None => json!({}),
                Some(n) if !n.is_empty() => json!({"decision": "block", "reason": n.join("\n")}),
                Some(_) => json!({"decision": "block", "reason": format!(
                    "[resource-ledger] Still queued:\n{}\nEnd your turn to keep waiting.",
                    ledger::with(|st| Ok(st.lines_for(ledger::now(), key)))?)}),
            };
            println!("{out}");
        }
        _ => {}
    }
    Ok(0)
}

/// What an agent is told when its session starts: the whole guide, this machine's
/// resources, and its session key. The machine part is skipped if it cannot be read.
fn intro(key: &str, stamped: bool) -> String {
    let pool = match ledger::with(|st| Ok(st.pool(ledger::now()))) {
        Ok(p) => format!("\n\n{p}"),
        Err(e) => {
            log(&format!("intro: {e}"));
            String::new()
        }
    };
    let how = if stamped {
        " (applied to your resource-ledger commands automatically)".to_string()
    } else {
        format!("\n  Add `--session {key}` to every resource-ledger request.")
    };
    format!(
        "[resource-ledger] This machine has a shared resource ledger for GPU/RAM/CPU-heavy work. \
         Its guide (also `resource-ledger help`):\n\n{}{pool}\n\nYOUR SESSION KEY\n  {key}{how}",
        crate::HELP)
}

fn notices(key: &str) -> Result<String, String> {
    Ok(ledger::with(|st| Ok(st.poll(ledger::now(), key)))?.notices.join("\n"))
}

fn emit(event: &str, text: &str) {
    if !text.is_empty() {
        println!("{}", json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}}));
    }
}

/// Poll until this session has notices (`Some(notices)`), `keep` says there is
/// nothing left to wait for (`None`), or `max` passes (`Some(empty)`).
fn wait(key: &str, max: Duration, keep: impl Fn(&Polled) -> bool) -> Result<Option<Vec<String>>, String> {
    let deadline = Instant::now() + max;
    loop {
        let p = ledger::with(|st| Ok(st.poll(ledger::now(), key)))?;
        if !p.notices.is_empty() {
            return Ok(Some(p.notices));
        }
        if !keep(&p) {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Ok(Some(Vec::new()));
        }
        thread::sleep(POLL);
    }
}

/// The tool input with this session's key added, or None if the call does not run resource-ledger.
fn stamp(tool: &str, input: &Value, key: &str) -> Option<Value> {
    let cmd = input["command"].as_str()?;
    if !runs_ledger(cmd) || cmd.contains("RESOURCE_LEDGER_SESSION") {
        return None;
    }
    let prefix = match tool {
        "Bash" => format!("export RESOURCE_LEDGER_SESSION='{key}'; "),
        "PowerShell" => format!("$env:RESOURCE_LEDGER_SESSION='{key}'; "),
        _ => return None,
    };
    let mut out = input.clone();
    out["command"] = json!(prefix + cmd);
    Some(out)
}

/// True if `resource-ledger` appears as a command word: `(^|[\s;&|(`])resource-ledger(\.exe)?(\s|$)`.
fn runs_ledger(cmd: &str) -> bool {
    const NAME: &str = "resource-ledger";
    cmd.match_indices(NAME).any(|(i, _)| {
        let rest = &cmd[i + NAME.len()..];
        let rest = rest.strip_prefix(".exe").unwrap_or(rest);
        cmd[..i].chars().next_back().is_none_or(|c| c.is_whitespace() || ";&|(`".contains(c))
            && rest.chars().next().is_none_or(char::is_whitespace)
    })
}

fn log(msg: &str) {
    let home = ledger::home();
    let _ = fs::create_dir_all(&home);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(home.join("hook-errors.log")) {
        let _ = writeln!(f, "{} {msg}", ledger::now() as u64);
    }
}
