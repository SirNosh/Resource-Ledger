//! `resource-ledger install <client>`: wire resource-ledger into a client's user-level config.
//!
//! Every file is copied to `<file>.resource-ledger-bak-<unix time>` before it is changed.
//! resource-ledger hooks from any earlier install are replaced; everything else (e.g. Orca
//! or herdr hooks) is left exactly as it was. Re-running changes nothing.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::ledger::{self, err};

const PLUGIN: &str = include_str!("../integrations/opencode/resource-ledger.js");

pub fn run(client: &str) -> Result<String, String> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).ok_or("no home directory")?;
    let home = PathBuf::from(home);
    let exe = std::env::current_exe().map_err(err)?.to_string_lossy().into_owned();
    let slashed = exe.replace('\\', "/"); // Claude Code and OpenCode run commands through bash / node
    match client {
        "claude" => {
            let path = home.join(".claude").join("settings.json");
            let hook = |event: &str, timeout: u32| {
                json!({"type": "command", "command": format!("\"{slashed}\" hook claude {event}"), "timeout": timeout})
            };
            let mut stop = hook("stop", 86400);
            stop["asyncRewake"] = json!(true); // background waiter: exit 2 wakes the idle session
            set_hooks(&path, vec![
                ("SessionStart", None, hook("start", 15)),
                ("PreToolUse", Some("Bash|PowerShell"), hook("pre", 15)),
                ("PostToolUse", Some("*"), hook("post", 15)),
                ("Stop", None, stop),
                ("SessionEnd", None, hook("end", 5)),
            ])?;
            Ok(format!("Claude Code hooks set in {}. Open chats pick them up; nothing needs restarting.", path.display()))
        }
        "codex" => {
            let path = home.join(".codex").join("hooks.json");
            let hook = |event: &str, timeout: u32| {
                let cmd = format!("\"{exe}\" hook codex {event}");
                let mut h = json!({"type": "command", "command": cmd, "timeout": timeout});
                if cfg!(windows) {
                    h["commandWindows"] = json!(cmd); // Codex runs hooks through cmd.exe there
                }
                h
            };
            set_hooks(&path, vec![
                ("SessionStart", None, hook("start", 15)),
                ("PostToolUse", None, hook("post", 15)),
                ("Stop", None, hook("stop", 3600)),
            ])?;
            Ok(format!("Codex hooks set in {}. In Codex, open /hooks and trust the three resource-ledger hooks.", path.display()))
        }
        "opencode" => {
            let path = home.join(".config").join("opencode").join("plugins").join("resource-ledger.js");
            fs::create_dir_all(path.parent().unwrap()).map_err(err)?;
            backup(&path)?;
            fs::write(&path, PLUGIN.replace("__RESOURCE_LEDGER_EXE__", &slashed)).map_err(err)?;
            Ok(format!("OpenCode plugin written to {}. Restart OpenCode to load it.", path.display()))
        }
        _ => Err("usage: resource-ledger install claude|codex|opencode".into()),
    }
}

/// `.../resource-ledger[.exe]["] hook ...`, as written on any OS.
fn is_ours(cmd: &str) -> bool {
    cmd.replace(".exe", "").replace('"', "").contains("resource-ledger hook ")
}

/// Remove every resource-ledger hook (and any group or event only resource-ledger used), then append `wanted`.
fn set_hooks(path: &Path, wanted: Vec<(&str, Option<&str>, Value)>) -> Result<(), String> {
    let mut cfg: Value = match fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| format!("{}: {e}", path.display()))?,
        Err(e) if e.kind() == ErrorKind::NotFound => json!({}),
        Err(e) => return Err(err(e)),
    };
    let root = cfg.as_object_mut().ok_or(format!("{} is not a JSON object", path.display()))?;
    let hooks = root.entry("hooks").or_insert_with(|| json!({})).as_object_mut().ok_or("`hooks` is not an object")?;
    let mut emptied = Vec::new();
    for (event, groups) in hooks.iter_mut() {
        let Some(groups) = groups.as_array_mut() else { continue };
        let had = groups.len();
        groups.retain_mut(|g| match g.get_mut("hooks").and_then(Value::as_array_mut) {
            Some(hs) => {
                let n = hs.len();
                hs.retain(|h| !h["command"].as_str().is_some_and(is_ours));
                hs.len() == n || !hs.is_empty()
            }
            None => true,
        });
        if had > 0 && groups.is_empty() {
            emptied.push(event.clone());
        }
    }
    for event in emptied {
        hooks.shift_remove(&event);
    }
    for (event, matcher, hook) in wanted {
        let mut group = Map::new();
        if let Some(m) = matcher {
            group.insert("matcher".into(), json!(m));
        }
        group.insert("hooks".into(), json!([hook]));
        hooks.entry(event).or_insert_with(|| json!([])).as_array_mut().ok_or("hook event is not a list")?.push(group.into());
    }
    backup(path)?;
    fs::create_dir_all(path.parent().unwrap()).map_err(err)?;
    fs::write(path, serde_json::to_string_pretty(&cfg).map_err(err)? + "\n").map_err(err)
}

fn backup(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let name = format!("{}.resource-ledger-bak-{}", path.file_name().unwrap().to_string_lossy(), ledger::now() as u64);
    fs::copy(path, path.with_file_name(name)).map(|_| ()).map_err(err)
}
