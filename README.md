# resource-ledger

A shared reservation ledger for the GPUs, RAM and CPUs of one machine, for all the coding-agent
sessions running on it: Claude Code, Codex, OpenCode, or any agent that has a shell.

Before heavy work, such as training, GPU inference or a large test suite, an agent asks the
ledger for what it needs and for how long. The ledger answers **granted** (naming the GPUs to
use) or **queued, estimated start in N min**. When a queued agent's turn comes, a hook wakes it,
so nobody has to watch the queue. Agents run their own work and release the ticket when they
are done.

The ledger is advisory: it never reserves, enforces or kills anything, and every agent is
trusted to keep it accurate. It also measures what is actually in use on the machine, so the
person at the keyboard and programs that never use the ledger are not run over. That is enough
to stop sessions from different projects colliding on a GPU or swapping a machine to death.

- **Every GPU is in the pool:** NVIDIA, AMD and Intel graphics cards, and Apple Silicon. Each
  is scheduled on its own, and a task can ask for several.
- **Windows, Linux and macOS.**
- **One ~0.6 MB executable.** No runtime, daemon, database or server.
- **Cheap.** Measured on a Windows 11 workstation:
  - a hook call after each tool call takes about 7 ms;
  - a session start takes about 0.3 s, including one reading of the machine;
  - a session waiting for its turn uses about 1.2 MB of private memory.

## Contents

- [Requirements](#requirements)
- [Install](#install)
- [How agents use it](#how-agents-use-it)
- [Commands](#commands)
- [GPUs](#gpus)
- [Scheduling](#scheduling)
- [Use outside the ledger](#use-outside-the-ledger)
- [Notifications and wake-up](#notifications-and-wake-up)
- [Session keys](#session-keys)
- [Using it from other agents](#using-it-from-other-agents)
- [Files](#files)
- [Uninstall](#uninstall)
- [Development](#development)
- [Limitations](#limitations)

## Requirements

- **Windows 10 or 11, Linux, or macOS.**
- **Rust 1.89 or later** to build, because it uses `std::fs::File::lock`.
- **For NVIDIA GPUs:** the NVIDIA driver, whose `nvidia-smi` the ledger runs to read them.
- **Other GPUs** need nothing extra: they are read from the operating system (see [GPUs](#gpus)).

A machine without a GPU the ledger can see still works: RAM and CPU requests are scheduled as
usual, and GPU requests are refused.

## Install

```
cargo install --git https://github.com/SirNosh/Resource-Ledger
```

This puts `resource-ledger` in `~/.cargo/bin` (`%USERPROFILE%\.cargo\bin` on Windows), which
rustup adds to PATH. Alternatively, run `cargo build --release` and copy
`target/release/resource-ledger` (`resource-ledger.exe` on Windows) to any folder on PATH.

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

Agents learn how to use the ledger in two ways:
- **Clients with hooks:** every new session starts with the full guide (the text of
  `resource-ledger help`), this machine's GPUs, RAM and CPUs, and the session's key. That is
  about 5,000 characters. Claude Code also gets it again after the conversation is compacted.
- **Any agent:** `resource-ledger help` prints the full guide.

The protocol is **one ticket = one task**:

1. **Ask** before heavy work:
   `resource-ledger request --vram 8 --ram 16 --cpus 4 --minutes 90 --purpose "LoRA finetune"`
2. **If the answer is GRANTED,** start the task, on the GPUs the grant names and no others.
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
request ─► QUEUED ──(its turn, and it fits beside use outside the ledger)──► GRANTED ──release──► done
             │                                                                 │
             └─ release, or the session ends ─► cancelled                      └─ minutes run out ─► "out of time" notice
                                                                                              └─ 15 min later ─► force-expired
```

## Commands

```
resource-ledger request --minutes M --purpose TEXT [--vram GB [--gpus N] [--gpu WHICH]] [--ram GB] [--cpus N] [--session KEY]
resource-ledger release ID
resource-ledger extend ID MINUTES
resource-ledger status [--watch]
resource-ledger ledger [--json] [--watch]
resource-ledger help                      (also -h / --help, anywhere)
resource-ledger install claude|codex|opencode
resource-ledger hook CLIENT EVENT         (run by the installed hooks; not for direct use)
```

**`request`** asks for resources for one task.
- Give at least one of `--vram`, `--ram` or `--cpus`. Omitted ones count as 0, and GB may be fractional.
- `--vram GB` is per GPU. `--gpus N` asks for N GPUs with that much each (default 1).
- `--gpu WHICH` limits the GPUs it may use: a vendor (`nvidia`, `amd`, `intel`, `apple`) or GPU
  numbers as `status` lists them (`0`, or `0,2`). It is required on a machine whose GPUs come
  from more than one vendor (see [GPUs](#gpus)).
- A request no GPU or the machine could ever hold is refused, with the list of GPUs.
- It prints one ticket line. The ticket ID follows the `#`, and a grant names its GPUs:

```
#1 [granted] 20 GB VRAM, 16 GB RAM, 4 CPUs for 90 min: LoRA finetune on GPU 1 (NVIDIA GeForce RTX 4090; CUDA_VISIBLE_DEVICES=GPU-3c4d). GRANTED: 1 h 30 min left. ...
#2 [queued] 8 GB VRAM on each of 2 GPUs for 60 min: DDP eval. QUEUED: position 1, estimated start in 1 h 30 min. ...
```

**`release`** marks the task done and frees the grant. On a queued request it cancels the request.

**`extend`** adds time to the same task. Requests behind it may start later.

**`status`** shows each GPU, RAM and CPUs, with what grants hold and what is in use outside the
ledger, then every open ticket with the session it belongs to:

```
GPU 0  NVIDIA GeForce RTX 4090; CUDA_VISIBLE_DEVICES=GPU-1a2b: 24 GB VRAM, held 0, in use outside the ledger 1.2
GPU 1  NVIDIA GeForce RTX 4090; CUDA_VISIBLE_DEVICES=GPU-3c4d: 24 GB VRAM, held 20, in use outside the ledger 0.4
RAM    64 GB, held 16, in use outside the ledger 9.5
CPUs   32, held 28, in use outside the ledger 2.3
  #1 [granted] 20 GB VRAM, 16 GB RAM, 4 CPUs for 90 min: LoRA finetune on GPU 1 (...). GRANTED: 1 h 30 min left. ...  <claude:3f2a>
  #2 [queued] 8 GB VRAM on each of 2 GPUs for 60 min: DDP eval. QUEUED: position 1, estimated start in 1 h 30 min. ...  <codex:9b1c>
  #3 [granted] 24 CPUs for 30 min: test suite. GRANTED: 30 min left. ...  <opencode:77e0>
```

**`ledger`** shows the ledger as a table, for people and agents alike: every open ticket, then the
last 20 closed ones, newest first. A ticket closes when it is released, cancelled (released while
queued, or its session ended) or force-expired. Sessions are shortened to `client:` plus 8
characters of the id; `status` and `--json` give the full key.

```
OPEN (2)
  #   state    session          needs                         GPUs  time                   purpose
  12  granted  claude:5c1e7a42  8 GB VRAM, 16 GB RAM, 4 CPUs  0     1 h 12 min left        LoRA finetune
  13  queued   codex:0199a2b3   8 CPUs                              starts in ~1 h 12 min  test suite

CLOSED (newest first; the ledger keeps the last 20)
  #   how        session          needs    GPUs  closed      held for  purpose
  11  released   claude:5c1e7a42  24 CPUs        12 min ago  40 min    build
  10  cancelled  codex:0199a2b3   4 CPUs         1 h ago               flaky test rerun
```

- **`--json`** prints `{"now", "open", "starts", "closed"}`: the open tickets, the estimated start
  of each queued one (Unix seconds, keyed by ticket ID), and the closed tickets with `closed_at`
  and `how`.
- **`--watch`** (with `status` or `ledger`) redraws every 2 s until Ctrl-C. It never exits, so it
  is for people at a terminal; inside Claude Code it is refused.

**Exit codes:** `0` means success; read the output for GRANTED or QUEUED. `2` means an error; the
reason is on stderr, and a usage error points to `resource-ledger help`.

## GPUs

Every GPU the ledger can see is in the pool, and each one's VRAM is a separate resource.

| | Windows | Linux | macOS |
|---|---|---|---|
| **NVIDIA** | `nvidia-smi`: every GPU, VRAM total and in use | `nvidia-smi`: the same | — |
| **AMD** | DXCore: discrete GPUs; VRAM in use from the graphics kernel | sysfs (`amdgpu`): VRAM total and in use | — |
| **Intel** | DXCore: discrete GPUs (Arc); VRAM in use from the graphics kernel | sysfs (`i915` or `xe`): discrete GPUs' VRAM total; **in use is not reported** | — |
| **Apple** | — | — | Apple Silicon's GPU, with unified memory |
| **RAM** | `GlobalMemoryStatusEx` | `/proc/meminfo`: MemTotal − MemAvailable | `hw.memsize`; in use = all but free and inactive pages |
| **Busy CPUs** | `GetSystemTimes` | `/proc/stat` | `host_statistics` (CPU load) |

**Integrated GPUs are left out.** Their "VRAM" is system RAM, which the ledger already tracks.
Apple Silicon's GPU is the exception, because it is the only GPU those machines have. Its
memory is RAM, so on a Mac `--vram` is counted against RAM. GPUs from other vendors, such as a
virtual machine's display adapter, are left out too.

**Grants name their GPUs.** Where there is a standard way to select a GPU, the grant gives it.
The agent sets it for the task's processes so they use only those GPUs:

| GPU | Selector in the grant |
|---|---|
| NVIDIA (any OS) | `CUDA_VISIBLE_DEVICES=GPU-<uuid>` |
| AMD on Linux | `ROCR_VISIBLE_DEVICES=GPU-<unique id>` |
| Others | none; the grant gives the GPU's name (and PCI address on Linux) |

**GPUs are spread, not packed.** A request takes, of the GPUs it may use, those with the most
room over its whole window. The ledger tracks GPU memory, not GPU compute, so two jobs sharing a
card would slow each other down.

**Machines with GPUs from more than one vendor** (for example NVIDIA and AMD) need `--gpu` on every
request with `--vram`, because CUDA work can't run on an AMD card and vice versa. Without it the
request is refused with the list:

```
resource-ledger: this machine has GPUs from more than one vendor, so say which to use: --gpu VENDOR (e.g. --gpu nvidia for CUDA work) or --gpu NUMBERS (e.g. --gpu 0). Its GPUs:
  GPU 0: NVIDIA GeForce RTX 4070 SUPER; CUDA_VISIBLE_DEVICES=GPU-1a2b [nvidia], 12 GB VRAM
  GPU 1: AMD Radeon RX 7900 XTX (PCI 0000:03:00.0); ROCR_VISIBLE_DEVICES=GPU-5f6e7d8c9b0a1b2c [amd], 24 GB VRAM
```

**Stable identity.** Tickets remember each GPU by a stable id: the NVIDIA UUID, the Windows
adapter LUID, the Linux PCI address, or `apple`. A grant can't move to another card if the GPUs
are listed in a different order later.

## Scheduling

There is no daemon. Every command and every hook call takes the ledger's file lock and
re-plans the queue from scratch. The time-based events below are applied whenever that happens.

**Resources:** each GPU's VRAM, RAM, and logical CPUs. What the ledger hands out is each one's
capacity minus [use outside the ledger](#use-outside-the-ledger).

**Order is first come, first served, with conservative backfill.**
- Queued requests are placed one at a time, in arrival order.
- Each one gets the earliest start at which it fits on every resource, for its whole duration,
  alongside everything already placed. "Already placed" means the current grants (until they
  end) and the requests ahead of it in the queue.
- A later request can therefore start before an earlier one only if it fits into a gap without
  delaying anyone ahead of it.
- This is also what produces the estimated start times.
- There are no priorities.

**Example** on one 12 GB GPU, 32 GB RAM and 16 CPUs, with nothing else running. A holds 8 GB of
VRAM for another 60 min. Requests then arrive in this order:

| Request | Result |
|---|---|
| B: 10 GB VRAM, 30 min | Doesn't fit beside A (8 + 10 > 12). **Queued, estimated start in 60 min.** |
| C: 8 CPUs, 20 min | Fits now and takes nothing B needs. **Granted at once** (backfill). |
| D: 3 GB VRAM, 90 min | Fits beside A now, but would still be running when B starts (10 + 3 > 12), which would delay B. **Queued, estimated start in 90 min**, after B. |
| D′: 3 GB VRAM, 45 min | Finishes before B starts. **Granted at once.** |

**Releasing.** A release is applied immediately. The queue is re-planned in the same call, and
whoever fits now is granted at that moment.

**Estimated starts** assume every grant uses its full time. They move up when someone releases
early, and back when someone extends.

**Time limits.**
- When a grant's minutes run out, its session gets an **out of time** notice: release it if
  the task is finished, or extend it if it is still running.
- 15 minutes later the grant is **force-expired** and its resources go to the next in line.
- Nothing is killed. The session is told that if its work is still running, it may now collide.

## Use outside the ledger

Agents' estimates are trusted, and the ledger never checks whether a job uses what it asked
for. What it measures is everything else: the person using the machine, and programs that never
asked the ledger.

**What is measured:** VRAM in use on each GPU, RAM in use, and busy CPUs. Busy CPUs are averaged
since the previous reading, or over 200 ms if there is no recent one.

**When:** only when something needs it: a request, `status`, a session start, or while a request
is queued. A reading is reused for 5 s.

**Outside use** of each resource is what is measured, minus the estimates of the grants that are
over 3 minutes old, never below 0.
- A grant younger than 3 minutes may not show up in the readings yet, so its estimate counts
  on top of them.
- If a job uses more than its estimate, the excess shows up as outside use, so others wait.
  Nothing else happens.

**What it does:** outside use is taken off each resource's capacity, now and in every future
plan, so estimated starts allow for it. If it is so high that a request can't fit even after
every grant ends, the request says it is "waiting for usage outside the ledger to drop", and
`status` shows the amounts. With hooks, it is granted within seconds of the use dropping.

A GPU whose VRAM in use isn't reported (Intel on Linux) has no outside use, so only the ledger's
own arithmetic protects it.

## Notifications and wake-up

The ledger sends a notice to the owning session when a queued request is granted, when a grant
runs out of time, and when a grant is force-expired. Each notice is delivered once. If a
request is granted immediately, the answer to `request` is the notice.

| Client | Mechanism | What it does |
|---|---|---|
| Claude Code | `SessionStart` hook | Adds the full guide, this machine's resources and the session key. |
| | `PreToolUse` hook (Bash, PowerShell) | Puts the session key in front of any command that runs `resource-ledger`. |
| | `PostToolUse` hook | Delivers notices after each tool call, mid-turn. |
| | `Stop` hook (`asyncRewake`) | While the session has open tickets, waits in the background (checking every 5 s) and wakes the idle chat when a notice arrives. |
| | `SessionEnd` hook | Cancels the session's queued requests. |
| Codex | `SessionStart` hook | Adds the same intro, which tells the agent to pass `--session codex:<id>`. |
| | `PostToolUse` hook | Delivers notices after each tool call. |
| | `Stop` hook | While a request is queued, keeps the turn open (up to about 58 min) and continues the agent with the grant notice. |
| OpenCode | plugin | Sets the session key in the shell environment, adds the same intro to the system prompt (once per session, from `resource-ledger hook opencode intro`), checks every 15 s while a session has tickets, and prompts the session when it is idle. |

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
This machine runs resource-ledger, a shared reservation ledger for its GPUs, RAM and CPUs.
Before heavy work (training, GPU inference, eval sweeps, big builds or test suites), run
`resource-ledger help` once and follow its protocol: request first, start only when GRANTED
(on the GPUs it names), release as soon as the task is done, and make a new request for any
further work.
```

Without hooks, nothing wakes the agent. A queued agent checks `resource-ledger status` now and then.

## Files

Everything lives in a `resource-ledger` folder under:

| OS | Folder |
|---|---|
| Windows | `%LOCALAPPDATA%` |
| macOS | `~/Library/Application Support` |
| Linux | `$XDG_STATE_HOME`, else `~/.local/state` |

Set `RESOURCE_LEDGER_HOME` to use a different folder.

| File | Purpose |
|---|---|
| `ledger.json` | The ledger: open tickets, the last 20 closed ones, undelivered notices and the last machine reading. It stays small. |
| `ledger.lock` | Exclusive lock held during every read-modify-write. |
| `wait-<session>.lock` | Ensures at most one background waiter per Claude Code session. |
| `hook-errors.log` | Hook errors. A hook never fails its agent. |

## Uninstall

- **Claude Code / Codex:** remove the hook entries whose command runs `resource-ledger hook`
  (`"…/resource-ledger.exe" hook …` on Windows). You can also restore the
  `*.resource-ledger-bak-*` copy made at install time, provided you haven't changed the file since.
- **OpenCode:** delete `~/.config/opencode/plugins/resource-ledger.js`.
- Then delete the exe and the ledger folder (see [Files](#files)).

## Development

```
cargo test
cargo test real_probe -- --ignored --nocapture    # this machine's real readings, checked for sanity
```

- **Unit tests** cover:
  - the scheduler's placement rules;
  - the ticket lifecycle on a fake clock: grants, notices, out of time, expiry, extend and session end;
  - the closed-ticket history, and the `ledger` table and JSON;
  - several GPUs: spreading, multi-GPU requests, `--gpu`, the mixed-vendor rule and refusals;
  - outside use of VRAM and CPUs, and the 3-minute rule;
  - Apple's unified memory;
  - the readers, on recorded `nvidia-smi` output, sysfs trees (AMD, Intel, an integrated GPU),
    `/proc/stat` and `/proc/meminfo`.
- **Integration tests** run the built exe the way hooks run it. They cover each hook event, the
  session-start intro, help and errors, `ledger` (text, JSON and `--watch`), the installer (on
  configs that also hold other tools' hooks), and an 8-process race that must never oversubscribe.
- **CI** (`.github/workflows/ci.yml`) runs both commands above on Linux, macOS and Windows.

**Environment variables for testing:**
- `RESOURCE_LEDGER_HOME`: use a scratch ledger folder.
- `RESOURCE_LEDGER_FAKE_PROBE`: replace the machine readings, for example
  `{"gpus":[{"id":"g0","vendor":"nvidia","name":"Test GPU","total":12,"used":0}],"ram_total":32,"ram_used":0,"cpus":16,"cpus_busy":0}`.

| Path | Contents |
|---|---|
| `src/main.rs` | Command-line parsing, usage and help |
| `src/ledger.rs` | Ledger file and lock, scheduler, outside use, time rules, notices |
| `src/probe/mod.rs` | Machine readings shared by every OS: `nvidia-smi`, the sysfs GPU reader, CPU averaging |
| `src/probe/win.rs` | Windows: RAM, CPU time, DXCore GPUs and their VRAM in use |
| `src/probe/linux.rs` | Linux: `/proc` and sysfs |
| `src/probe/mac.rs` | macOS: RAM, CPU load and the Apple Silicon GPU |
| `src/hooks.rs` | Hook handlers for each client, and the session-start intro |
| `src/install.rs` | Config installers |
| `integrations/opencode/resource-ledger.js` | OpenCode plugin |
| `tests/cli.rs` | Integration tests |

## Limitations

- **Advisory.** An agent that skips the ledger isn't stopped. One that uses more than it asked
  for is only caught indirectly, as use outside the ledger.
- **GPU memory, not GPU compute.** Jobs are spread across GPUs, but two jobs on one card are not
  kept from slowing each other down.
- **Briefly over-cautious after a grant.** For a grant's first 3 minutes, its use can count twice:
  once in the readings as soon as it shows up, and once as its estimate. CPU use shows up almost
  at once, so for those minutes slightly fewer CPUs are handed out than are really free.
- **Intel GPUs on Linux:** the driver doesn't report VRAM in use, so use outside the ledger isn't
  seen on them.
- **AMD APUs on Linux** look like graphics cards in sysfs, so they are listed as GPUs, with the
  VRAM carved out for them.
- **Intel Macs:** CPU and RAM only; their GPUs aren't in the pool.
- **Tested hardware.** The Windows readers are verified on an NVIDIA machine. The Linux and macOS
  GPU readers are tested on recorded data, and their CPU and RAM readers run in CI. On Windows, AMD
  and Intel discrete GPUs and the integrated-GPU filter have not yet been seen on real hardware.
- **Crashed chats keep their grants for a while.** If a chat crashes right after being granted,
  its grant stays booked for its requested minutes plus 15. A normal session end cancels its
  queued requests.
- **No daemon,** so the time limits are applied whenever any command or hook runs. While a
  session is waiting, its hook checks every 5 s (every 15 s in OpenCode), which keeps the clock moving.
