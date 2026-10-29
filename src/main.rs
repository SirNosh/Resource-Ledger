//! resource-ledger: a shared, advisory resource ledger for the coding-agent sessions on one
//! machine. Agents ask for GPU memory, RAM and CPUs for a while; the ledger answers
//! granted or queued, and hooks wake a queued agent when its turn comes. Nothing is
//! reserved or enforced: agents are trusted to keep the ledger honest.

mod hooks;
mod install;
mod ledger;
mod probe;

use std::collections::HashMap;
use std::time::Duration;

const USAGE: &str = "usage:
  resource-ledger request --minutes M --purpose TEXT [--vram GB [--gpus N] [--gpu WHICH]] [--ram GB] [--cpus N] [--session KEY]
  resource-ledger release ID
  resource-ledger extend ID MINUTES
  resource-ledger status [--watch]
  resource-ledger ledger [--json] [--watch]
  resource-ledger install claude|codex|opencode
Run `resource-ledger help` for the full guide.";

/// Written for the agents that use the tool: when to use it, the protocol, and every command.
/// Agents also get it when a session starts (see hooks.rs).
const HELP: &str = r#"resource-ledger: a shared reservation ledger for the GPUs, RAM and CPUs of this machine.
Every coding-agent session (Claude Code, Codex, OpenCode, or any agent with a shell) checks in
with it before heavy work, so jobs from different sessions do not collide. It is advisory:
nothing is reserved, enforced or killed, and every agent is trusted to keep it accurate.

WHEN TO USE IT
  Before heavy work: training or fine-tuning, GPU inference or model servers, eval sweeps,
  big builds or test suites, anything holding several GB of RAM or several cores for more
  than a few minutes. Reading, editing and small commands need no reservation.

PROTOCOL: one ticket = one task
  1. Ask:     resource-ledger request --vram 8 --ram 16 --cpus 4 --minutes 90 --purpose "LoRA finetune"
  2. GRANTED: start the task, on the GPU(s) the grant names and no others.
     QUEUED:  do not start it. With hooks (Claude Code, Codex, OpenCode), end your turn: you are
              woken when it is granted. Without hooks, check `resource-ledger status` now and then.
  3. Done:    resource-ledger release ID   as soon as the task finishes, even if time is left.
  4. More work afterwards needs a new request; it joins the queue behind earlier ones.
     Use `extend` only when the same task runs past its minutes.

COMMANDS
  request --minutes M --purpose TEXT [--vram GB] [--gpus N] [--gpu WHICH] [--ram GB] [--cpus N] [--session KEY]
      Ask for resources for one task. Give at least one of --vram/--ram/--cpus; omitted ones
      are 0, and GB may be fractional. Estimate honestly: everyone else is scheduled around it.
        --vram GB    VRAM on each GPU the task uses.
        --gpus N     How many GPUs it uses, each with --vram GB (default 1).
        --gpu WHICH  The GPUs it may use: a vendor (nvidia, amd, intel, apple) or GPU numbers as
                     `status` lists them (0, or 0,2). Required if the GPUs are from several vendors.
      Prints one ticket line; the ticket ID follows '#':
        #7 [granted] 8 GB VRAM, 16 GB RAM, 4 CPUs for 90 min: LoRA finetune on GPU 0 (NVIDIA GeForce
           RTX 4090; CUDA_VISIBLE_DEVICES=GPU-...). GRANTED: 1 h 30 min left. ...
        #8 [queued] 16 CPUs for 30 min: test suite. QUEUED: position 1, estimated start in 25 min. ...
  release ID          The task is done: free the grant, or cancel a queued request.
  extend ID MINUTES   The same task needs longer. Requests behind it may start later.
  status [--watch]    Each GPU, RAM and CPUs (held, and in use outside the ledger), and every open
                      ticket with its session.
  ledger [--json] [--watch]
                      The ledger as a table: every open ticket (state, session, needs, GPUs, time
                      left or estimated start), then the last 20 closed ones (released, cancelled or
                      expired). --json prints it as JSON.
                      --watch (status, ledger) redraws every 2 s until Ctrl-C. It never exits, so it
                      is for people only; agents are refused.
  help                This guide. -h and --help work anywhere.
  install claude|codex|opencode
                      One-time setup by a person: adds the hooks or plugin to that client.
  hook CLIENT EVENT   Run by those hooks; not for direct use.

GPUS
  Every GPU the ledger can see is in the pool and scheduled on its own: NVIDIA, AMD and Intel
  graphics cards, and Apple Silicon. Other integrated GPUs are left out, as their memory is RAM.
  Apple Silicon's GPU shares RAM too, so there --vram counts against RAM.
  A grant names its GPUs. Where it gives a variable (CUDA_VISIBLE_DEVICES=..., ROCR_VISIBLE_DEVICES=...),
  set it for the task's processes so they use only those GPUs; otherwise pick them by the name given.

SCHEDULING
  First come, first served, with backfill: a later request starts earlier only if it fits on
  every resource for its whole duration without delaying anyone ahead of it. A GPU request gets
  the eligible GPUs with the most room, so jobs spread out rather than share a card.
  Use outside the ledger counts: VRAM per GPU, RAM and busy CPUs are measured, and whatever is
  in use beyond the grants (the person at the machine, programs not in the ledger) is not handed
  out. For a grant's first 3 min its use is counted on top of the readings, which may not show it yet.
  Estimated starts assume holders use all their time; they move up when holders release early.
  When a grant's minutes run out it gets an "out of time" notice; 15 min later it is
  force-expired and its resources go to the next in line.

SESSION KEY
  Each ticket belongs to a session, so notices reach the right agent. Claude Code and OpenCode
  set it automatically; Codex agents pass --session codex:<id> as their intro says. Otherwise
  it comes from RESOURCE_LEDGER_SESSION, or is "user".

EXIT CODES
  0  success: read the output for GRANTED or QUEUED
  2  error: the reason is on stderr

State: ledger.json in a resource-ledger folder under %LOCALAPPDATA% (Windows), ~/Library/Application
Support (macOS), or $XDG_STATE_HOME, else ~/.local/state (Linux). RESOURCE_LEDGER_HOME overrides it."#;

const FLAGS: [&str; 10] = ["vram", "gpus", "gpu", "ram", "cpus", "minutes", "purpose", "session", "json", "watch"];
const SWITCHES: [&str; 2] = ["json", "watch"]; // options that take no value
const WATCH: Duration = Duration::from_secs(2);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_none_or(|a| a == "help") || args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{HELP}");
        return;
    }
    let code = match parse(&args) {
        Ok((pos, flags)) if pos.len() == 3 && pos[0] == "hook" => hooks::run(pos[1], pos[2], flags.get("session").copied()),
        Ok((pos, flags)) => {
            let watch = flags.contains_key("watch") && matches!(pos.as_slice(), ["status"] | ["ledger"]);
            #[cfg(windows)]
            if watch {
                enable_vt();
            }
            loop {
                match command(&pos, &flags) {
                    Ok(out) if watch => println!("\x1b[H\x1b[2J{out}"), // clear the screen, then redraw
                    Ok(out) => {
                        println!("{out}");
                        break 0;
                    }
                    Err(e) => {
                        eprintln!("resource-ledger: {e}");
                        break 2;
                    }
                }
                std::thread::sleep(WATCH);
            }
        }
        Err(e) => {
            eprintln!("resource-ledger: {e}");
            2
        }
    };
    std::process::exit(code);
}

/// Let the Windows console act on escape codes. Best effort: it fails when stdout is
/// not a console, where they need no handling.
#[cfg(windows)]
fn enable_vt() {
    use windows::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleMode,
    };
    // SAFETY: console calls on this process's own stdout handle, with a valid out-pointer.
    unsafe {
        let Ok(out) = GetStdHandle(STD_OUTPUT_HANDLE) else { return };
        let mut mode = CONSOLE_MODE::default();
        if GetConsoleMode(out, &mut mode).is_ok() {
            let _ = SetConsoleMode(out, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

/// Positional words, `--name value` options, and the SWITCHES, which take no value.
fn parse(args: &[String]) -> Result<(Vec<&str>, HashMap<&str, &str>), String> {
    let (mut pos, mut flags) = (Vec::new(), HashMap::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.strip_prefix("--") {
            Some(name) if SWITCHES.contains(&name) => {
                flags.insert(name, "");
            }
            Some(name) => {
                let value = it.next().ok_or(format!("--{name} needs a value"))?;
                flags.insert(name, value.as_str());
            }
            None => pos.push(a.as_str()),
        }
    }
    Ok((pos, flags))
}

fn command(pos: &[&str], flags: &HashMap<&str, &str>) -> Result<String, String> {
    if let Some(bad) = flags.keys().find(|k| !FLAGS.contains(k)) {
        return Err(format!("unknown option --{bad}\n{USAGE}"));
    }
    if flags.contains_key("watch") && std::env::var_os("CLAUDECODE").is_some() {
        return Err("--watch never exits, so it is for people only; run the command without it.".into());
    }
    let num = |s: &str, what: &str| s.parse::<f64>().map_err(|_| format!("{what} must be a number, got `{s}`"));
    let id = |s: &str| s.parse::<u64>().map_err(|_| format!("ticket ID must be a number, got `{s}`"));
    match pos {
        ["request"] => {
            let get = |k: &str| flags.get(k).map_or(Ok(0.0), |v| num(v, &format!("--{k}")));
            let gpus = flags.get("gpus").map_or(Ok(0), |v| v.parse().map_err(|_| format!("--gpus must be a whole number, got `{v}`")))?;
            let need = ledger::Need { vram: get("vram")?, gpus, ram: get("ram")?, cpus: get("cpus")?, ..Default::default() };
            let minutes = num(flags.get("minutes").ok_or("--minutes is required")?, "--minutes")?;
            let purpose = flags.get("purpose").ok_or("--purpose is required")?;
            let session = session(flags.get("session").copied())?;
            ledger::with(|st| st.request(ledger::now(), &session, need, flags.get("gpu").copied(), minutes, purpose))
        }
        ["release", t] => {
            let t = id(t)?;
            ledger::with(|st| st.release(ledger::now(), t))
        }
        ["extend", t, m] => {
            let (t, m) = (id(t)?, num(m, "MINUTES")?);
            ledger::with(|st| st.extend(ledger::now(), t, m))
        }
        ["status"] => ledger::with(|st| Ok(st.status(ledger::now()))),
        ["ledger"] => ledger::with(|st| st.ledger(ledger::now(), flags.contains_key("json"))),
        ["install", client] => install::run(client),
        _ => Err(USAGE.into()),
    }
}

/// The key a request is filed under, i.e. the session its notices are delivered to.
fn session(explicit: Option<&str>) -> Result<String, String> {
    let key = explicit.map(str::to_owned).or_else(|| std::env::var("RESOURCE_LEDGER_SESSION").ok());
    if let Some(key) = key.filter(|k| !k.is_empty()) {
        return Ok(key);
    }
    if std::env::var_os("CLAUDECODE").is_some() {
        // Inside an agent whose hook did not stamp the command: refuse rather than file
        // the ticket under a key whose notices would never reach this agent.
        return Err("no session key. Pass --session with the key from the resource-ledger intro (e.g. --session claude:<session id>).".into());
    }
    Ok("user".into())
}
