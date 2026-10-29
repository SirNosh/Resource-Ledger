//! The real exe, run the way agents and their hooks run it, each test on its own ledger.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const MACHINE: &str = r#"{"gpus":[{"id":"g0","vendor":"nvidia","name":"Test GPU","total":12,"used":0,"select":"CUDA_VISIBLE_DEVICES=GPU-test"}],
    "ram_total":32,"ram_used":0,"cpus":16,"cpus_busy":0}"#;

fn home(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("resource-ledger-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cmd(home: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_resource-ledger"));
    c.args(args).env("RESOURCE_LEDGER_HOME", home).env("RESOURCE_LEDGER_FAKE_PROBE", MACHINE);
    c.env_remove("CLAUDECODE").env_remove("RESOURCE_LEDGER_SESSION"); // these tests may run inside Claude Code
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    c
}

fn spawn(home: &Path, args: &[&str], stdin: &str) -> Child {
    let mut child = cmd(home, args).spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    child
}

/// (exit code, stdout, stderr), killing the process if it runs past `limit`.
fn finish(mut child: Child, limit: Duration) -> (i32, String, String) {
    let t = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if t.elapsed() > limit {
            child.kill().unwrap();
            panic!("still running after {limit:?}");
        }
        sleep(Duration::from_millis(50));
    }
    let o = child.wait_with_output().unwrap();
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (o.status.code().unwrap_or(-1), text(&o.stdout), text(&o.stderr))
}

fn run(home: &Path, args: &[&str], stdin: &str) -> (i32, String, String) {
    finish(spawn(home, args, stdin), Duration::from_secs(20))
}

fn ok(home: &Path, args: &[&str]) -> String {
    let (code, out, err) = run(home, args, "");
    assert_eq!(code, 0, "{args:?}: {err}");
    out
}

fn request(home: &Path, session: &str, cpus: &str) -> String {
    ok(home, &["request", "--cpus", cpus, "--minutes", "30", "--purpose", "t", "--session", session])
}

fn context(out: &str) -> String {
    if out.trim().is_empty() {
        return String::new();
    }
    let v: Value = serde_json::from_str(out).unwrap();
    v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().into()
}

fn sid(id: &str) -> String {
    json!({"session_id": id}).to_string()
}

#[test]
fn request_queue_release_and_notice_once() {
    let h = home("flow");
    assert!(request(&h, "s:a", "16").contains("[granted]"));
    let b = request(&h, "s:b", "4");
    assert!(b.contains("[queued]") && b.contains("position 1, estimated start in 30 min"), "{b}");
    assert!(ok(&h, &["status"]).contains("<s:b>"));
    assert!(ok(&h, &["release", "1"]).contains("#1 released"));
    let poll = || serde_json::from_str::<Value>(&ok(&h, &["hook", "opencode", "poll", "--session", "s:b"])).unwrap();
    let p = poll();
    assert!(p["notices"][0].as_str().unwrap().contains("GRANTED") && p["open"] == true, "{p}");
    assert_eq!(poll()["notices"], json!([]));
}

#[test]
fn bad_input_is_refused() {
    let h = home("bad");
    let (code, _, err) = run(&h, &["request", "--vgpu", "8", "--minutes", "5", "--purpose", "x"], "");
    assert!(code == 2 && err.contains("unknown option --vgpu"), "{err}");
    let (code, _, err) = run(&h, &["request", "--vram", "13", "--minutes", "5", "--purpose", "x", "--session", "s"], "");
    assert!(code == 2 && err.contains("only 0 of the GPUs") && err.contains("GPU 0: Test GPU"), "{err}");
    let (code, _, err) = run(&h, &["request", "--vram", "4", "--gpus", "two", "--minutes", "5", "--purpose", "x"], "");
    assert!(code == 2 && err.contains("--gpus must be a whole number"), "{err}");
    let o = cmd(&h, &["request", "--cpus", "1", "--minutes", "5", "--purpose", "x"]).env("CLAUDECODE", "1").output().unwrap();
    assert!(o.status.code() == Some(2) && String::from_utf8_lossy(&o.stderr).contains("no session key"));
    assert_eq!(run(&h, &["release", "7"], "").0, 2);
}

#[test]
fn help_is_available_however_an_agent_asks() {
    let h = home("help");
    for args in [&[][..], &["help"], &["--help"], &["-h"], &["request", "--help"]] {
        let (code, out, _) = run(&h, args, "");
        assert!(code == 0 && out.contains("PROTOCOL: one ticket = one task") && out.contains("ledger [--json] [--watch]"), "{args:?}");
    }
    let (code, _, err) = run(&h, &["frobnicate"], "");
    assert!(code == 2 && err.contains("Run `resource-ledger help`"), "{err}");
}

#[test]
fn ledger_as_text_json_and_live() {
    let h = home("ledger");
    request(&h, "claude:5c1e7a42-8b3d", "16");
    request(&h, "codex:0199a2b3-aa", "4");
    ok(&h, &["release", "2"]);
    let text = ok(&h, &["ledger"]);
    assert!(text.contains("OPEN (1)") && text.contains("granted  claude:5c1e7a42") && text.contains("cancelled  codex:0199a2b3"), "{text}");
    let j: Value = serde_json::from_str(&ok(&h, &["ledger", "--json"])).unwrap();
    assert!(j["open"][0]["id"] == 1 && j["closed"][0]["how"] == "cancelled", "{j}");

    let mut watcher = spawn(&h, &["ledger", "--watch"], "");
    sleep(Duration::from_millis(4500)); // two redraws 2 s apart, with slack for a busy machine
    watcher.kill().unwrap();
    let out = String::from_utf8_lossy(&watcher.wait_with_output().unwrap().stdout).into_owned();
    assert!(out.matches("\x1b[H\x1b[2JOPEN (1)").count() >= 2, "{out}");
    let o = cmd(&h, &["ledger", "--watch"]).env("CLAUDECODE", "1").output().unwrap();
    assert!(o.status.code() == Some(2) && String::from_utf8_lossy(&o.stderr).contains("--watch never exits"));
}

#[test]
fn intro_on_session_start_has_guide_machine_and_key() {
    let h = home("intro");
    let guide = ok(&h, &["help"]);
    let claude = context(&run(&h, &["hook", "claude", "start"], &sid("s1")).1);
    let codex = context(&run(&h, &["hook", "codex", "start"], &sid("c9")).1);
    let opencode = ok(&h, &["hook", "opencode", "intro", "--session", "opencode:o1"]);
    for (text, key) in [(&claude, "claude:s1 (applied"), (&codex, "--session codex:c9`"), (&opencode, "opencode:o1 (applied")] {
        assert!(text.contains(guide.trim()), "the whole guide: {text}");
        assert!(text.contains("THIS MACHINE\n  GPU 0: Test GPU; CUDA_VISIBLE_DEVICES=GPU-test [nvidia], 12 GB VRAM\n  RAM: 32 GB    CPUs: 16"), "{text}");
        assert!(text.contains(key), "{text}");
        assert!(text.len() < 10_000, "keep session-start context small: {} characters", text.len());
    }
}

#[test]
fn pre_stamps_ledger_commands_only() {
    let h = home("pre");
    let pre = |tool: &str, input: Value| {
        let out = run(&h, &["hook", "claude", "pre"], &json!({"session_id": "s1", "tool_name": tool, "tool_input": input}).to_string()).1;
        (!out.trim().is_empty()).then(|| serde_json::from_str::<Value>(&out).unwrap()["hookSpecificOutput"]["updatedInput"].clone())
    };
    assert_eq!(
        pre("Bash", json!({"command": "resource-ledger request --cpus 2 --minutes 5 --purpose x", "timeout": 5})),
        Some(json!({"command": "export RESOURCE_LEDGER_SESSION='claude:s1'; resource-ledger request --cpus 2 --minutes 5 --purpose x", "timeout": 5})));
    assert!(pre("PowerShell", json!({"command": "resource-ledger status"})).unwrap()["command"].as_str().unwrap().starts_with("$env:RESOURCE_LEDGER_SESSION="));
    assert!(pre("Bash", json!({"command": "cd x && resource-ledger.exe status"})).is_some());
    assert_eq!(pre("Bash", json!({"command": "ls resource-ledger-notes/"})), None);
    assert_eq!(pre("Bash", json!({"command": "python train.py"})), None);
    assert_eq!(pre("Read", json!({"file_path": "resource-ledger"})), None);
}

#[test]
fn post_delivers_notices_once() {
    let h = home("post");
    request(&h, "s:other", "16");
    assert!(request(&h, "claude:s1", "4").contains("[queued]"));
    assert_eq!(run(&h, &["hook", "claude", "post"], &sid("s1")).1, "");
    ok(&h, &["release", "1"]);
    assert!(context(&run(&h, &["hook", "claude", "post"], &sid("s1")).1).contains("GRANTED"));
    assert_eq!(run(&h, &["hook", "claude", "post"], &sid("s1")).1, "");
}

#[test]
fn claude_stop_exits_at_once_without_tickets() {
    let h = home("idle");
    let t = Instant::now();
    assert_eq!(run(&h, &["hook", "claude", "stop"], &sid("idle")).0, 0);
    assert!(t.elapsed() < Duration::from_secs(3));
}

#[test]
fn claude_stop_wakes_on_grant_and_dedupes() {
    let h = home("wake");
    request(&h, "test:A", "16");
    request(&h, "claude:w", "8");
    let first = spawn(&h, &["hook", "claude", "stop"], &sid("w"));
    sleep(Duration::from_secs(1));
    let t = Instant::now();
    assert_eq!(run(&h, &["hook", "claude", "stop"], &sid("w")).0, 0); // defers to the live first waiter
    assert!(t.elapsed() < Duration::from_secs(3));
    ok(&h, &["release", "1"]);
    let (code, _, err) = finish(first, Duration::from_secs(20));
    assert!(code == 2 && err.contains("GRANTED"), "{code} {err}");
}

#[test]
fn codex_stop_blocks_with_grant_reason() {
    let h = home("codex");
    request(&h, "test:A", "16");
    request(&h, "codex:c1", "8");
    let waiter = spawn(&h, &["hook", "codex", "stop"], &sid("c1"));
    sleep(Duration::from_secs(1));
    ok(&h, &["release", "1"]);
    let (code, out, _) = finish(waiter, Duration::from_secs(20));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert!(code == 0 && v["decision"] == "block" && v["reason"].as_str().unwrap().contains("GRANTED"), "{out}");
    assert_eq!(run(&h, &["hook", "codex", "stop"], &sid("nobody")).1.trim(), "{}");
}

#[test]
fn hook_errors_never_break_the_agent() {
    let h = home("errors");
    assert_eq!(run(&h, &["hook", "claude", "post"], ""), (0, String::new(), String::new()));
    assert_eq!(run(&h, &["hook", "claude", "post"], "not json"), (0, String::new(), String::new()));
    assert_eq!(run(&h, &["hook", "claude", "prompt"], &sid("s1")), (0, String::new(), String::new())); // v1 event
    assert!(std::fs::read_to_string(h.join("hook-errors.log")).unwrap().contains("claude post"));
}

#[test]
fn concurrent_processes_never_oversubscribe() {
    // 8 processes each want 4 of 16 CPUs at once: exactly 4 may be granted.
    let h = home("race");
    let kids: Vec<Child> = (0..8)
        .map(|i| spawn(&h, &["request", "--cpus", "4", "--minutes", "60", "--purpose", "race", "--session", &format!("race:{i}")], ""))
        .collect();
    let outs: Vec<String> = kids.into_iter().map(|k| finish(k, Duration::from_secs(20)).1).collect();
    assert_eq!(outs.iter().filter(|o| o.contains("[granted]")).count(), 4, "{outs:?}");
    assert_eq!(outs.iter().filter(|o| o.contains("[queued]")).count(), 4, "{outs:?}");
}

fn commands(cfg: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for groups in cfg["hooks"].as_object().unwrap().values() {
        for g in groups.as_array().unwrap() {
            for h in g["hooks"].as_array().unwrap() {
                out.push(h["command"].as_str().unwrap().to_string());
            }
        }
    }
    out
}

/// Installer against client configs that already hold other tools' hooks and an older install.
#[test]
fn install_replaces_only_our_hooks() {
    let h = home("install");
    let other = |cmd: &str| json!({"hooks": [{"type": "command", "command": cmd}]});
    let old = |event: &str| json!({"hooks": [{"type": "command", "command": format!("\"/old/bin/resource-ledger\" hook claude {event}")}]});
    let claude = json!({
        "model": "x",
        "hooks": {
            "SessionStart": [other("orca-hook start"), old("start")],
            "Stop": [{"hooks": [{"type": "command", "command": "herdr notify"}, {"type": "command", "command": "C:/old/resource-ledger.exe hook claude stop"}]}],
            "UserPromptSubmit": [old("prompt")],
        },
    });
    let codex = json!({"hooks": {"Stop": [other("orca-hook codex-stop")]}});
    for (rel, cfg) in [(".claude/settings.json", &claude), (".codex/hooks.json", &codex)] {
        std::fs::create_dir_all(h.join(rel).parent().unwrap()).unwrap();
        std::fs::write(h.join(rel), cfg.to_string()).unwrap();
    }
    let read = |rel: &str| serde_json::from_str::<Value>(&std::fs::read_to_string(h.join(rel)).unwrap()).unwrap();
    let install = |client: &str| {
        let o = cmd(&h, &["install", client]).env("USERPROFILE", &h).env("HOME", &h).output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    };

    let before = read(".claude/settings.json");
    install("claude");
    let after = read(".claude/settings.json");
    let kept: Vec<String> = commands(&before).into_iter().filter(|c| !c.contains("resource-ledger")).collect();
    assert!(kept.len() == 2 && kept.iter().all(|c| commands(&after).contains(c)), "Orca/herdr hooks must survive");
    let ours: Vec<String> = commands(&after).into_iter().filter(|c| c.contains("resource-ledger")).collect();
    assert_eq!(ours.len(), 5, "{ours:?}"); // earlier resource-ledger hooks were replaced, not kept
    assert!(ours.iter().all(|c| !c.contains('\\') && !c.contains("/old/")), "{ours:?}");
    assert!(after["hooks"].get("UserPromptSubmit").is_none(), "an event only we used is dropped");
    let stop: Vec<&Value> = after["hooks"]["Stop"].as_array().unwrap().iter()
        .flat_map(|g| g["hooks"].as_array().unwrap()).filter(|x| x["command"].as_str().unwrap().contains("resource-ledger")).collect();
    assert!(stop.len() == 1 && stop[0]["asyncRewake"] == true && stop[0]["timeout"] == 86400);
    let without_hooks = |v: &Value| { let mut v = v.clone(); v.as_object_mut().unwrap().remove("hooks"); v };
    assert_eq!(without_hooks(&before), without_hooks(&after));
    install("claude");
    assert_eq!(read(".claude/settings.json"), after, "re-running changes nothing");
    assert!(std::fs::read_dir(h.join(".claude")).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains("resource-ledger-bak-")));

    let before = read(".codex/hooks.json");
    install("codex");
    let after = read(".codex/hooks.json");
    assert!(commands(&before).iter().filter(|c| !c.contains("resource-ledger")).all(|c| commands(&after).contains(c)));
    assert_eq!(commands(&after).iter().filter(|c| c.contains("resource-ledger")).count(), 3);
    let windows_cmd = after["hooks"]["Stop"].as_array().unwrap().iter().any(|g| g["hooks"][0]["commandWindows"].is_string());
    assert_eq!(windows_cmd, cfg!(windows));

    install("opencode");
    let plugin = std::fs::read_to_string(h.join(".config/opencode/plugins/resource-ledger.js")).unwrap();
    let exe = format!("/resource-ledger{}\"", std::env::consts::EXE_SUFFIX);
    assert!(!plugin.contains("__RESOURCE_LEDGER_EXE__") && plugin.contains(&exe));
}
