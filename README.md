# resource-ledger

A shared reservation ledger for GPU memory, RAM and CPUs, for all the coding-agent sessions
running on one machine: Claude Code, Codex, OpenCode, or any agent that has a shell.

Before heavy work, such as training, GPU inference or a large test suite, an agent asks the
ledger for what it needs and for how long. The ledger answers **granted** or **queued, estimated
start in N min**. When a queued agent's turn comes, a hook wakes it, so nobody has to watch the
queue. Agents run their own work and release the ticket when they are done.

The ledger is advisory: it never reserves, enforces or kills anything, and every agent is
trusted to keep it accurate. That is enough to stop sessions from different projects
colliding on a GPU or swapping a machine to death.

- One ~0.5 MB executable. No runtime, daemon, database or server.
- Measured on a Windows 11 workstation: a hook call takes about 17 ms, and a session waiting
  for its turn uses under 1 MB.

## Contents

- [Requirements](#requirements)
- [Install](#install)
- [How agents use it](#how-agents-use-it)
- [Commands](#commands)
- [Scheduling](#scheduling)
- [Notifications and wake-up](#notifications-and-wake-up)
- [Session keys](#session-keys)
- [Using it from other agents](#using-it-from-other-agents)
- [Files](#files)
- [Uninstall](#uninstall)
- [Development](#development)
- [Limitations](#limitations)

## Requirements

- **Windows 10 or 11.** It uses the Win32 memory API and Windows process flags.
- **Rust 1.89 or later** to build, because it uses `std::fs::File::lock`.
- **An NVIDIA driver, for GPU requests.** VRAM is read from GPU 0 with `nvidia-smi`, which ships
  with the driver. Without one, VRAM capacity is 0 and GPU requests are refused; RAM and CPU
  requests still work.

## Install

```
cargo install --git https://github.com/SirNosh/Resource-Ledger
```

This puts `resource-ledger.exe` in `%USERPROFILE%\.cargo\bin`, which rustup adds to PATH.
Alternatively, run `cargo build --release` and copy `target\release\resource-ledger.exe` to any
folder on PATH.

Then connect each agent client you use. Each command only edits your user-level config:

```
resource-ledger install claude      # Claude Code: hooks in ~/.claude/settings.json
resource-ledger install codex       # Codex: hooks in ~/.codex/hooks.json, then trust them in Codex via /hooks
resource-ledger install opencode    # OpenCode: plugin in ~/.config/opencode/plugins/, then restart OpenCode
```

- **Backups:** each install first copies the file it changes to `<file>.resource-ledger-bak-<unix time>`.
- **Your other hooks are kept:** it replaces only its own hooks, and leaves every other hook and setting as it was.
- **Re-running is safe:** running it again changes nothing.
- **Exe path:** the hooks call the exe by the absolute path it had when you ran `install`. Run `install` again if you move it.
- **Open chats:** Claude Code chats that are already open pick up the hooks at once, without restarting.

## How agents use it

Agents are told how to use the ledger in two ways:
- **Clients with hooks:** each new session gets a short intro.
- **Any agent:** `resource-ledger help` prints the full guide.

The protocol is **one ticket = one task**:

1. **Ask** before heavy work:
   `resource-ledger request --vram 8 --ram 16 --cpus 4 --minutes 90 --purpose "LoRA finetune"`
2. **If the answer is GRANTED,** start the task.
   **If it is QUEUED,** don't start it. End your turn. You are woken when it is granted.
3. **When the task is done, release it straight away,** even if time is left: `resource-ledger release 7`.
4. **For further work, make a new request,** which joins the queue behind the requests already
   waiting. `resource-ledger extend 7 30` is only for the same task when it runs past its minutes.

What counts as heavy work:
- training or fine-tuning;
- GPU inference or model servers;
- eval sweeps;
- big builds or test suites;
- anything holding several GB of RAM, or several cores, for more than a few minutes.

Reading, editing and small commands need no reservation.

A ticket moves through these states:

```
request ─► QUEUED ──(its turn, and the memory is actually free)──► GRANTED ──release──► done
             │                                                       │
             └─ release, or the session ends ─► cancelled            └─ minutes run out ─► "out of time" notice
                                                                                    └─ 15 min later ─► force-expired
```

## Commands

```
resource-ledger request --minutes M --purpose TEXT [--vram GB] [--ram GB] [--cpus N] [--session KEY]
resource-ledger release ID
resource-ledger extend ID MINUTES
resource-ledger status
resource-ledger help                      (also -h / --help, anywhere)
resource-ledger install claude|codex|opencode
resource-ledger hook CLIENT EVENT         (run by the installed hooks; not for direct use)
```

**`request`** asks for resources for one task.
- Give at least one of `--vram`, `--ram` or `--cpus`. Omitted ones count as 0, and GB may be fractional.
- A request larger than the machine is refused.
- It prints one ticket line. The ticket ID follows the `#`:

```
#7 [granted] 8 GB VRAM, 16 GB RAM, 4 CPUs for 90 min: LoRA finetune. GRANTED: 1 h 30 min left. ...
#8 [queued] 16 CPUs for 30 min: test suite. QUEUED: position 1, estimated start in 25 min. ...
```

**`release`** marks the task done and frees the grant. On a queued request it cancels the request.

**`extend`** adds time to the same task. Requests behind it may start later.

**`status`** shows the machine's capacity, the total held, live free memory, and every open
ticket with the session it belongs to:

```
capacity 12 GB VRAM, 31.2 GB RAM, 16 CPUs | held 2 GB VRAM, 4 GB RAM, 4 CPUs | live free 3.1 GB VRAM, 9.4 GB RAM
  #1 [granted] 2 GB VRAM, 4 GB RAM, 4 CPUs for 90 min: LoRA finetune. GRANTED: 1 h 30 min left. ...  <claude:3f2a>
  #2 [queued] 16 CPUs for 30 min: test suite. QUEUED: position 1, estimated start in 1 h 30 min. ...  <codex:9b1c>
```

**Exit codes:** `0` means success; read the output for GRANTED or QUEUED. `2` means an error; the
reason is on stderr, and a usage error points to `resource-ledger help`.

## Scheduling

There is no daemon. Every command and every hook call takes the ledger's file lock and
re-plans the queue from scratch. The time-based events below are applied whenever that happens.

**Capacity** is what the machine has: GPU 0's VRAM (from `nvidia-smi`), physical RAM, and logical CPUs.

**Order is first come, first served, with conservative backfill.**
- Queued requests are placed one at a time, in arrival order.
- Each one gets the earliest start at which it fits on every resource, for its whole duration,
  alongside everything already placed. "Already placed" means the current grants (until they
  end) and the requests ahead of it in the queue.
- A later request can therefore start before an earlier one only if it fits into a gap without
  delaying anyone ahead of it.
- This is also what produces the estimated start times.
- There are no priorities.

**Example** on a 12 GB GPU, 32 GB RAM and 16 CPUs. A holds 8 GB of VRAM for another 60 min.
Requests then arrive in this order:

| Request | Result |
|---|---|
| B: 10 GB VRAM, 30 min | Doesn't fit beside A (8 + 10 > 12). **Queued, estimated start in 60 min.** |
| C: 8 CPUs, 20 min | Fits now and takes nothing B needs. **Granted at once** (backfill). |
| D: 3 GB VRAM, 90 min | Fits beside A now, but would still be running when B starts (10 + 3 > 12), which would delay B. **Queued, estimated start in 90 min**, after B. |
| D′: 3 GB VRAM, 45 min | Finishes before B starts. **Granted at once.** |

**Live memory check.** A request the plan would start now is granted only if its VRAM and RAM
are actually free on the machine at that moment.
- Memory already promised to grants made in the last 3 minutes doesn't count as free, because
  those jobs may not have allocated it yet.
- There is no fixed safety margin.
- If the memory isn't free, the request waits ("waiting for memory used outside the ledger to
  free up") and is checked again within seconds.
- This protects processes that never used the ledger. CPUs aren't checked this way.

**Releasing.** A release is applied immediately. The queue is re-planned in the same call, and
whoever fits now is granted at that moment.

**Estimated starts** assume every grant uses its full time. They move up when someone releases
early, and back when someone extends.

**Time limits.**
- When a grant's minutes run out, its session gets an **out of time** notice: release it if
  the task is finished, or extend it if it is still running.
- 15 minutes later the grant is **force-expired** and its resources go to the next in line.
- Nothing is killed. The session is told that if its work is still running, it may now collide.

## Notifications and wake-up

The ledger sends a notice to the owning session when a queued request is granted, when a grant
runs out of time, and when a grant is force-expired. Each notice is delivered once. If a
request is granted immediately, the answer to `request` is the notice.

| Client | Mechanism | What it does |
|---|---|---|
| Claude Code | `SessionStart` hook | Adds the intro: the protocol and the session key. |
| | `PreToolUse` hook (Bash, PowerShell) | Puts the session key in front of any command that runs `resource-ledger`. |
| | `PostToolUse` hook | Delivers notices after each tool call, mid-turn. |
| | `Stop` hook (`asyncRewake`) | While the session has open tickets, waits in the background (checking every 5 s) and wakes the idle chat when a notice arrives. |
| | `SessionEnd` hook | Cancels the session's queued requests. |
| Codex | `SessionStart` hook | Adds the intro, which tells the agent to pass `--session codex:<id>`. |
| | `PostToolUse` hook | Delivers notices after each tool call. |
| | `Stop` hook | While a request is queued, keeps the turn open (up to about 58 min) and continues the agent with the grant notice. |
| OpenCode | plugin | Sets the session key in the shell environment, adds the intro to the system prompt, checks every 15 s while a session has tickets, and prompts the session when it is idle. |

A hook never breaks its agent: any error goes to `hook-errors.log`, and the hook exits cleanly.

## Session keys

Each ticket belongs to a session key such as `claude:<session id>`, so its notices reach the
right agent.
- **Claude Code and OpenCode** set the key automatically.
- **Codex** agents pass `--session codex:<id>`, as their intro tells them to.
- **Anything else** gets the key from `RESOURCE_LEDGER_SESSION` if it is set, and `user` otherwise.

Inside Claude Code, a request with no key is refused, because its notices could never reach the chat.

## Using it from other agents

Any agent with a shell can use the ledger. Add this to its instructions (AGENTS.md, a system
prompt, or similar):

```
This machine runs resource-ledger, a shared reservation ledger for GPU memory, RAM and CPUs.
Before heavy work (training, GPU inference, eval sweeps, big builds or test suites), run
`resource-ledger help` once and follow its protocol: request first, start only when GRANTED,
release as soon as the task is done, and make a new request for any further work.
```

Without hooks, nothing wakes the agent. A queued agent checks `resource-ledger status` now and then.

## Files

Everything lives in `%LOCALAPPDATA%\resource-ledger\`. Set `RESOURCE_LEDGER_HOME` to use a different folder.

| File | Purpose |
|---|---|
| `ledger.json` | The ledger: open tickets and undelivered notices. Closed tickets are dropped, so it stays small. |
| `ledger.lock` | Exclusive lock held during every read-modify-write. |
| `wait-<session>.lock` | Ensures at most one background waiter per Claude Code session. |
| `hook-errors.log` | Hook errors. A hook never fails its agent. |

## Uninstall

- **Claude Code / Codex:** remove the hook entries whose command contains
  `resource-ledger.exe" hook`. You can also restore the `*.resource-ledger-bak-*` copy made at
  install time, provided you haven't changed the file since.
- **OpenCode:** delete `~/.config/opencode/plugins/resource-ledger.js`.
- Then delete the exe and `%LOCALAPPDATA%\resource-ledger\`.

## Development

```
cargo test
```

- **Unit tests** cover the scheduler's placement rules and the ticket lifecycle on a fake clock:
  grants, notices, out of time, expiry, extend, the live memory check, and session end.
- **Integration tests** run the built exe the way hooks run it. They cover each hook event, help
  and errors, and an 8-process race that must never oversubscribe.
- **Installer test:** it copies your real `~/.claude/settings.json` and `~/.codex/hooks.json` to
  a temp folder and installs into the copies. The test is skipped if those files are missing,
  and it never touches the originals.

**Environment variables for testing:**
- `RESOURCE_LEDGER_HOME`: use a scratch ledger folder.
- `RESOURCE_LEDGER_FAKE_PROBE`: replace the machine readings, for example
  `{"vram_total":12,"vram_free":12,"ram_total":32,"ram_free":32,"cpus":16}`.

| Path | Contents |
|---|---|
| `src/main.rs` | Command-line parsing, usage and help |
| `src/ledger.rs` | Ledger file and lock, scheduler, time rules, notices |
| `src/probe.rs` | Machine readings (RAM via Win32, VRAM via `nvidia-smi`) |
| `src/hooks.rs` | Hook handlers for each client |
| `src/install.rs` | Config installers |
| `integrations/opencode/resource-ledger.js` | OpenCode plugin |
| `tests/cli.rs` | Integration tests |

## Limitations

- **Windows only.**
- **One NVIDIA GPU.** Only GPU 0's totals are used, because Windows doesn't report VRAM per process.
- **Advisory.** An agent that skips the ledger, or holds more than it asked for, isn't stopped.
  The live memory check protects grants from untracked memory use, but not from untracked CPU use.
- **Crashed chats keep their grants for a while.** If a chat crashes right after being granted,
  its grant stays booked for its requested minutes plus 15. A normal session end cancels its
  queued requests.
- **No daemon,** so the time limits are applied whenever any command or hook runs. While a
  session is waiting, its hook checks every 5 s (every 15 s in OpenCode), which keeps the clock moving.
