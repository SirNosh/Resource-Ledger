//! The shared ledger: one JSON file, every operation under one exclusive file lock.
//!
//! There is no daemon. Every command and hook runs `tick` under the lock, which
//! applies the clock (out of time, forced expiry) and grants whatever the plan
//! places at `now`; sessions waiting for a turn poll, so something drives the
//! clock while it matters. A ticket is queued until granted, then held until it
//! is released or expires. Closed tickets are dropped from the file.
//!
//! The resources are each GPU's VRAM, RAM and CPUs. Agents' estimates are trusted:
//! what the machine reports in use beyond the grants is "outside use" (the person at
//! the machine, programs that are not in the ledger), and only the rest is handed out.

use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::probe::{self, Gpu, Probe};

const EPS: f64 = 1e-9;
const GRACE_S: f64 = 15.0 * 60.0; // a grant is force-expired this long after its time is up
const WARMUP_S: f64 = 180.0; // a grant this young may not show up in the readings yet
const PROBE_CACHE_S: f64 = 5.0;
const MAX_NOTICES: usize = 100; // undeliverable notices (crashed sessions) must not pile up
const HISTORY: usize = 20; // closed tickets kept for `resource-ledger ledger`

/// What one task asks for.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Need {
    /// GB of VRAM on each of `gpus` GPUs.
    pub vram: f64,
    pub gpus: usize,
    /// The GPUs (ids) it may use; empty means any.
    pub on: Vec<String>,
    pub ram: f64,
    pub cpus: f64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Ticket {
    pub id: u64,
    pub session: String,
    pub purpose: String,
    pub need: Need,
    pub minutes: f64,
    pub granted: Option<f64>,
    pub end: Option<f64>,
    pub warned: bool,
    /// The GPUs (ids) it was granted.
    pub gpus: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Notice {
    pub session: String,
    pub ticket: u64,
    pub text: String,
}

/// A ticket that is no longer open.
#[derive(Serialize, Deserialize)]
pub struct Closed {
    #[serde(flatten)]
    pub ticket: Ticket,
    pub closed_at: f64,
    /// released, cancelled or expired.
    pub how: String,
}

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    next_id: u64,
    probe: Option<Probe>,
    tickets: Vec<Ticket>,
    notices: Vec<Notice>,
    /// The last HISTORY closed tickets, oldest first. Ledgers written before 0.4 lack it.
    #[serde(default)]
    closed: Vec<Closed>,
}

pub struct Polled {
    pub notices: Vec<String>,
    pub queued: bool,
    pub open: bool,
}

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// `RESOURCE_LEDGER_HOME`, else the OS's place for per-user application state.
pub fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("RESOURCE_LEDGER_HOME") {
        return h.into();
    }
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) {
        var("LOCALAPPDATA")
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|h| h.join("Library").join("Application Support"))
    } else {
        var("XDG_STATE_HOME").or_else(|| var("HOME").map(|h| h.join(".local").join("state")))
    };
    base.unwrap_or_else(std::env::temp_dir).join("resource-ledger")
}

pub fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Run `f` on the ledger while holding its lock; the file is rewritten only if `f` changed it.
pub fn with<T>(f: impl FnOnce(&mut State) -> Result<T, String>) -> Result<T, String> {
    let dir = home();
    fs::create_dir_all(&dir).map_err(err)?;
    let lock = fs::OpenOptions::new().create(true).write(true).truncate(false).open(dir.join("ledger.lock")).map_err(err)?;
    lock.lock().map_err(err)?;
    let path = dir.join("ledger.json");
    let before = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(err(e)),
    };
    let mut st: State = if before.is_empty() {
        State::default()
    } else {
        serde_json::from_str(&before).map_err(|e| format!("{}: {e}", path.display()))?
    };
    let out = f(&mut st)?;
    let after = serde_json::to_string_pretty(&st).map_err(err)?;
    if after != before {
        let tmp = dir.join("ledger.json.tmp");
        fs::write(&tmp, &after).map_err(err)?;
        fs::rename(&tmp, &path).map_err(err)?;
    }
    Ok(out)
}

// Amounts of every resource are vectors indexed GPU 0..n, then RAM (n), then CPUs (n + 1).

fn capacity(p: &Probe) -> Vec<f64> {
    // a unified GPU's memory is RAM, limited there
    p.gpus.iter().map(|g| if g.unified { f64::INFINITY } else { g.total }).chain([p.ram_total, p.cpus]).collect()
}

fn unified(p: &Probe) -> Vec<bool> {
    p.gpus.iter().map(|g| g.unified).collect()
}

/// What `need` uses on the GPUs `gpus`; VRAM on a unified GPU is counted as RAM.
fn amounts(unified: &[bool], need: &Need, gpus: &[usize]) -> Vec<f64> {
    let n = unified.len();
    let mut a = vec![0.0; n + 2];
    a[n] = need.ram;
    a[n + 1] = need.cpus;
    for &g in gpus {
        a[if unified[g] { n } else { g }] += need.vram;
    }
    a
}

/// What a grant holds, on those of its GPUs the reading lists.
fn held(p: &Probe, t: &Ticket) -> Vec<f64> {
    let gpus: Vec<usize> = t.gpus.iter().filter_map(|id| p.gpus.iter().position(|g| &g.id == id)).collect();
    amounts(&unified(p), &t.need, &gpus)
}

fn add(to: &mut [f64], a: &[f64]) {
    to.iter_mut().zip(a).for_each(|(x, y)| *x += y);
}

fn eligible(p: &Probe, need: &Need) -> Vec<usize> {
    (0..p.gpus.len()).filter(|&i| need.on.is_empty() || need.on.contains(&p.gpus[i].id)).collect()
}

fn mixed_vendors(p: &Probe) -> bool {
    p.gpus.iter().any(|g| g.vendor != p.gpus[0].vendor)
}

/// Conservative backfill: place queued requests in order, each at the earliest time
/// it fits for its whole duration on top of everything already placed, so nobody is
/// delayed by someone behind them. A request that needs GPUs takes, of its eligible
/// ones, those with the most room over its window: jobs spread across GPUs, because
/// GPU compute is not tracked and two jobs on one card slow each other down.
///
/// Use is piecewise constant and only drops at an interval end, so the earliest
/// feasible start is `now` or one of those ends. Once every interval has ended only
/// outside use is left, so a request that `room` can hold always gets a slot.
///
/// `room`: per resource, what the ledger may hand out (capacity minus outside use).
/// `unified`: per GPU, whether its memory is RAM. `holds`: (end, amounts) of grants.
/// `queued`: (id, duration in s, need, eligible GPUs) in queue order.
/// Returns the start and GPUs of every request that could be placed.
fn plan(
    now: f64,
    room: &[f64],
    unified: &[bool],
    holds: &[(f64, Vec<f64>)],
    queued: &[(u64, f64, &Need, Vec<usize>)],
) -> HashMap<u64, (f64, Vec<usize>)> {
    let n = unified.len();
    let mut placed: Vec<(f64, f64, Vec<f64>)> =
        holds.iter().filter(|h| h.0 > now).map(|(end, a)| (now, *end, a.clone())).collect();
    let mut out = HashMap::new();
    for (id, dur, need, eligible) in queued {
        let mut candidates: Vec<f64> =
            std::iter::once(now).chain(placed.iter().map(|p| p.1).filter(|&e| e > now)).collect();
        candidates.sort_by(f64::total_cmp);
        candidates.dedup();
        for t in candidates {
            let free = free_over(&placed, room, t, t + dur);
            let mut gpus: Vec<usize> =
                eligible.iter().copied().filter(|&g| unified[g] || need.vram <= free[g] + EPS).collect();
            if gpus.len() < need.gpus {
                continue;
            }
            gpus.sort_by(|&a, &b| free[b].total_cmp(&free[a]));
            gpus.truncate(need.gpus);
            let a = amounts(unified, need, &gpus);
            if a[n] <= free[n] + EPS && a[n + 1] <= free[n + 1] + EPS {
                placed.push((t, t + dur, a));
                out.insert(*id, (t, gpus));
                break;
            }
        }
    }
    out
}

/// Per resource, `room` minus the most that `placed` uses at any moment of [start, end).
/// Within the window use only rises where an interval starts, so those moments suffice.
fn free_over(placed: &[(f64, f64, Vec<f64>)], room: &[f64], start: f64, end: f64) -> Vec<f64> {
    let mut free = room.to_vec();
    for x in std::iter::once(start).chain(placed.iter().map(|p| p.0).filter(|&s| start < s && s < end)) {
        let mut used = vec![0.0; room.len()];
        for (s, e, a) in placed {
            if *s <= x && x < *e {
                add(&mut used, a);
            }
        }
        free.iter_mut().zip(room.iter().zip(&used)).for_each(|(f, (r, u))| *f = f.min(r - u));
    }
    free
}

fn r1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn fmt_need(n: &Need) -> String {
    let mut parts = Vec::new();
    if n.vram > EPS {
        parts.push(match n.gpus {
            0 | 1 => format!("{} GB VRAM", r1(n.vram)),
            k => format!("{} GB VRAM on each of {k} GPUs", r1(n.vram)),
        });
    }
    if n.ram > EPS {
        parts.push(format!("{} GB RAM", r1(n.ram)));
    }
    if n.cpus > EPS {
        parts.push(format!("{} CPUs", r1(n.cpus)));
    }
    if parts.is_empty() { "nothing".into() } else { parts.join(", ") }
}

/// A GPU's name and, where known, how a program selects it.
fn label(g: &Gpu) -> String {
    match &g.select {
        Some(s) => format!("{}; {s}", g.name),
        None => g.name.clone(),
    }
}

/// The GPUs as agents see them, one line each.
fn gpu_list(p: &Probe) -> String {
    if p.gpus.is_empty() {
        return "\n  (no GPU the ledger can see)".into();
    }
    p.gpus
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let mem = if g.unified { "unified memory: its VRAM is counted as RAM".into() } else { format!("{} GB VRAM", r1(g.total)) };
            format!("\n  GPU {i}: {} [{}], {mem}", label(g), g.vendor)
        })
        .collect()
}

/// The GPUs `--gpu` names: a vendor (nvidia, amd, intel, apple) or GPU numbers like `0,2`.
fn select(p: &Probe, sel: &str) -> Result<Vec<usize>, String> {
    let sel = sel.trim().to_lowercase();
    let mut picked: Vec<usize> = if sel.chars().all(|c| c.is_ascii_digit() || c == ',' || c == ' ') {
        sel.split(',')
            .map(|s| s.trim().parse::<usize>().ok().filter(|&i| i < p.gpus.len()))
            .collect::<Option<_>>()
            .unwrap_or_default()
    } else {
        (0..p.gpus.len()).filter(|&i| p.gpus[i].vendor == sel).collect()
    };
    picked.sort();
    picked.dedup();
    if picked.is_empty() {
        return Err(format!("--gpu {sel}: no such GPU on this machine. Its GPUs:{}", gpu_list(p)));
    }
    Ok(picked)
}

fn dur(secs: f64) -> String {
    let m = (secs.max(0.0) / 60.0).ceil() as u64;
    match (m / 60, m % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// A session key cut to its client and 8 characters of its id, e.g. `claude:5c1e7a42`.
fn short(session: &str) -> String {
    match session.split_once(':') {
        Some((client, id)) => format!("{client}:{}", id.chars().take(8).collect::<String>()),
        None => session.into(),
    }
}

/// `NAME: none`, or `NAME (detail)` over the rows (the first is the header) as a table
/// whose columns are padded to their widest cell.
fn section(name: &str, detail: &str, rows: &[Vec<String>]) -> String {
    if rows.len() < 2 {
        return format!("{name}: none");
    }
    let widths: Vec<usize> = (0..rows[0].len()).map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0)).collect();
    let lines: Vec<String> = rows
        .iter()
        .map(|r| {
            let cells: Vec<String> = r.iter().zip(&widths).map(|(cell, &w)| format!("{cell:w$}")).collect();
            format!("  {}", cells.join("  ").trim_end())
        })
        .collect();
    format!("{name} ({detail})\n{}", lines.join("\n"))
}

impl State {
    fn readings(&mut self, now: f64) -> Probe {
        match &self.probe {
            Some(p) if (0.0..PROBE_CACHE_S).contains(&(now - p.at)) => p.clone(),
            prev => {
                let p = probe::read(now, prev.as_ref());
                self.probe = Some(p.clone());
                p
            }
        }
    }

    /// Per resource, what the machine reports in use beyond the grants old enough to
    /// show up in the readings, never below 0. A reading the driver does not give
    /// counts as no outside use.
    fn outside(&self, p: &Probe, now: f64) -> Vec<f64> {
        let mut loaded = vec![0.0; p.gpus.len() + 2];
        for t in self.tickets.iter().filter(|t| t.granted.is_some_and(|g| g <= now - WARMUP_S)) {
            add(&mut loaded, &held(p, t));
        }
        let measured = p.gpus.iter().map(|g| if g.unified { None } else { g.used }).chain([Some(p.ram_used), Some(p.cpus_busy)]);
        measured.zip(loaded).map(|(m, l)| m.map_or(0.0, |m| (m - l).max(0.0))).collect()
    }

    fn notify(&mut self, t: &Ticket, text: String) {
        self.notices.push(Notice { session: t.session.clone(), ticket: t.id, text });
    }

    fn close(&mut self, ticket: Ticket, now: f64, how: &str) {
        self.closed.push(Closed { ticket, closed_at: now, how: how.into() });
        let excess = self.closed.len().saturating_sub(HISTORY);
        self.closed.drain(..excess);
    }

    /// Apply the clock, then grant every queued ticket that the plan starts now. Returns the
    /// planned starts of queued tickets; one with none cannot fit even once every grant has
    /// ended, i.e. it waits for outside use to drop.
    fn tick(&mut self, now: f64) -> HashMap<u64, f64> {
        let (mut notes, mut expired) = (Vec::new(), Vec::new());
        self.tickets.retain_mut(|t| {
            let Some(end) = t.end else { return true };
            if now >= end + GRACE_S {
                notes.push((t.clone(), format!(
                    "[resource-ledger] Ticket #{} ({}) was force-expired 15 min after its time ran out; its \
                     resources are now available to others. If that work is still running it may now collide.",
                    t.id, t.purpose)));
                expired.push(t.clone());
                return false;
            }
            if now >= end && !t.warned {
                t.warned = true;
                notes.push((t.clone(), format!(
                    "[resource-ledger] Ticket #{} ({}) is out of time. If finished: `resource-ledger release {}`. If still \
                     running: `resource-ledger extend {} MINUTES`. It will be force-expired in 15 min.",
                    t.id, t.purpose, t.id, t.id)));
            }
            true
        });
        for (t, text) in notes {
            self.notify(&t, text);
        }
        for t in expired {
            self.close(t, now, "expired");
        }
        let excess = self.notices.len().saturating_sub(MAX_NOTICES);
        self.notices.drain(..excess);

        if self.tickets.iter().all(|t| t.granted.is_some()) {
            return HashMap::new();
        }
        let p = self.readings(now);
        let room: Vec<f64> = capacity(&p).iter().zip(self.outside(&p, now)).map(|(c, o)| c - o).collect();
        let placed = {
            let holds: Vec<(f64, Vec<f64>)> = self
                .tickets
                .iter()
                .filter_map(|t| t.end.map(|e| (if now >= e { e + GRACE_S } else { e }, held(&p, t))))
                .collect();
            let queued: Vec<(u64, f64, &Need, Vec<usize>)> = self
                .tickets
                .iter()
                .filter(|t| t.granted.is_none())
                .map(|t| (t.id, t.minutes * 60.0, &t.need, eligible(&p, &t.need)))
                .collect();
            plan(now, &room, &unified(&p), &holds, &queued)
        };
        let mut granted = Vec::new();
        for t in self.tickets.iter_mut().filter(|t| t.granted.is_none()) {
            if let Some((start, gpus)) = placed.get(&t.id).filter(|(start, _)| *start == now) {
                t.granted = Some(*start);
                t.end = Some(now + t.minutes * 60.0);
                t.gpus = gpus.iter().map(|&g| p.gpus[g].id.clone()).collect();
                granted.push(t.clone());
            }
        }
        for t in granted {
            let only = if t.gpus.is_empty() { "" } else { " Use only the GPU(s) named." };
            let text = format!(
                "[resource-ledger] Your turn: ticket #{} ({}) is GRANTED: {} for {} min{}.{only} Do this task, then \
                 `resource-ledger release {}` as soon as it is done; further work needs a new request.",
                t.id, t.purpose, fmt_need(&t.need), t.minutes, self.on_gpus(&t.gpus), t.id);
            self.notify(&t, text);
        }
        placed.into_iter().map(|(id, (start, _))| (id, start)).collect()
    }

    /// A GPU's number and reading, as the last reading knows them.
    fn find_gpu(&self, id: &str) -> Option<(usize, &Gpu)> {
        self.probe.as_ref()?.gpus.iter().enumerate().find(|(_, g)| g.id == id)
    }

    /// " on GPU 0 (name; selector)" for granted GPUs.
    fn on_gpus(&self, ids: &[String]) -> String {
        if ids.is_empty() {
            return String::new();
        }
        let names: Vec<String> = ids
            .iter()
            .map(|id| match self.find_gpu(id) {
                Some((i, g)) => format!("GPU {i} ({})", label(g)),
                None => format!("GPU {id}"),
            })
            .collect();
        format!(" on {}", names.join(" and "))
    }

    /// "0,1": the numbers of granted GPUs.
    fn gpu_numbers(&self, ids: &[String]) -> String {
        let nums: Vec<String> = ids.iter().map(|id| self.find_gpu(id).map_or_else(|| id.clone(), |(i, _)| i.to_string())).collect();
        nums.join(",")
    }

    fn line(&self, t: &Ticket, starts: &HashMap<u64, f64>, now: f64) -> String {
        let state = if t.granted.is_some() { "granted" } else { "queued" };
        let head = format!("#{} [{state}] {} for {} min: {}", t.id, fmt_need(&t.need), t.minutes, t.purpose);
        match t.end {
            None => {
                let pos = self.tickets.iter().filter(|q| q.granted.is_none()).position(|q| q.id == t.id).unwrap_or(0) + 1;
                let when = match starts.get(&t.id) {
                    Some(&s) => format!(", estimated start in {}", dur(s - now)),
                    None => "; waiting for usage outside the ledger to drop (see `resource-ledger status`)".into(),
                };
                format!("{head}. QUEUED: position {pos}{when}. End your turn; you will be woken when it is granted.")
            }
            Some(end) if now < end => format!(
                "{head}{}. GRANTED: {} left. `resource-ledger release {}` as soon as this task is done (further work needs \
                 a new request); `resource-ledger extend {} MINUTES` only if this task runs long.",
                self.on_gpus(&t.gpus), dur(end - now), t.id, t.id),
            Some(end) => format!(
                "{head}{}. OUT OF TIME: `resource-ledger release {}` or `resource-ledger extend {} MINUTES`; force-expires in {}.",
                self.on_gpus(&t.gpus), t.id, t.id, dur(end + GRACE_S - now)),
        }
    }

    fn line_of(&self, id: u64, starts: &HashMap<u64, f64>, now: f64) -> String {
        self.tickets.iter().find(|t| t.id == id).map_or_else(String::new, |t| self.line(t, starts, now))
    }

    /// File a request. `gpu` is the `--gpu` choice (vendor or GPU numbers), if any.
    pub fn request(
        &mut self,
        now: f64,
        session: &str,
        mut need: Need,
        gpu: Option<&str>,
        minutes: f64,
        purpose: &str,
    ) -> Result<String, String> {
        if !(minutes > 0.0 && minutes.is_finite()) || [need.vram, need.ram, need.cpus].iter().any(|x| !(*x >= 0.0 && x.is_finite())) {
            return Err("--minutes must be > 0 and resources must be >= 0".into());
        }
        if need.vram <= EPS && need.ram <= EPS && need.cpus <= EPS {
            return Err("request at least one of --vram, --ram, --cpus".into());
        }
        let p = self.readings(now);
        let mut unified_vram = 0.0;
        if need.vram <= EPS {
            if need.gpus > 0 || gpu.is_some() {
                return Err("--gpus and --gpu go with --vram (GB of VRAM per GPU)".into());
            }
        } else {
            need.gpus = need.gpus.max(1);
            if p.gpus.is_empty() {
                return Err(format!("this machine has no GPU the ledger can see, so --vram cannot be granted.{}", gpu_list(&p)));
            }
            let eligible = match gpu {
                Some(sel) => select(&p, sel)?,
                None => (0..p.gpus.len()).collect(),
            };
            if gpu.is_none() && mixed_vendors(&p) {
                return Err(format!(
                    "this machine has GPUs from more than one vendor, so say which to use: --gpu VENDOR (e.g. --gpu \
                     nvidia for CUDA work) or --gpu NUMBERS (e.g. --gpu 0). Its GPUs:{}",
                    gpu_list(&p)));
            }
            let fit: Vec<usize> =
                eligible.iter().copied().filter(|&i| p.gpus[i].unified || need.vram <= p.gpus[i].total + EPS).collect();
            if fit.len() < need.gpus {
                return Err(format!(
                    "this needs {} GPU(s) with {} GB VRAM each, but only {} of the GPUs it may use have that much:{}",
                    need.gpus, r1(need.vram), fit.len(), gpu_list(&p)));
            }
            if gpu.is_some() {
                need.on = eligible.iter().map(|&i| p.gpus[i].id.clone()).collect();
            }
            if fit.iter().all(|&i| p.gpus[i].unified) {
                unified_vram = need.vram * need.gpus as f64;
            }
        }
        if need.ram + unified_vram > p.ram_total + EPS || need.cpus > p.cpus + EPS {
            return Err(format!("request exceeds this machine's capacity ({} GB RAM, {} CPUs)", r1(p.ram_total), p.cpus));
        }
        self.next_id += 1;
        let id = self.next_id;
        self.tickets.push(Ticket {
            id, session: session.into(), purpose: purpose.into(), need, minutes,
            granted: None, end: None, warned: false, gpus: Vec::new(),
        });
        let starts = self.tick(now);
        self.notices.retain(|n| n.ticket != id); // an immediate grant is reported by this reply
        Ok(self.line_of(id, &starts, now))
    }

    pub fn release(&mut self, now: f64, id: u64) -> Result<String, String> {
        let i = self.tickets.iter().position(|t| t.id == id).ok_or(format!("no open ticket #{id}"))?;
        let t = self.tickets.remove(i);
        self.notices.retain(|n| n.ticket != id);
        let (what, purpose) = (if t.granted.is_some() { "released" } else { "cancelled" }, t.purpose.clone());
        self.close(t, now, what);
        self.tick(now);
        Ok(format!("#{id} {what} ({purpose})."))
    }

    pub fn extend(&mut self, now: f64, id: u64, minutes: f64) -> Result<String, String> {
        if !(minutes > 0.0 && minutes.is_finite()) {
            return Err("minutes must be > 0".into());
        }
        let t = self.tickets.iter_mut().find(|t| t.id == id).ok_or(format!("no open ticket #{id}"))?;
        t.minutes += minutes;
        if let Some(end) = t.end {
            t.end = Some(end.max(now) + minutes * 60.0);
            t.warned = false;
        }
        self.notices.retain(|n| n.ticket != id);
        let starts = self.tick(now);
        Ok(self.line_of(id, &starts, now))
    }

    pub fn status(&mut self, now: f64) -> String {
        let starts = self.tick(now);
        let p = self.readings(now);
        let n = p.gpus.len();
        let outside = self.outside(&p, now);
        let mut holding = vec![0.0; n + 2];
        for t in self.tickets.iter().filter(|t| t.granted.is_some()) {
            add(&mut holding, &held(&p, t));
        }
        let usage = |r: usize| {
            let seen = if r < n && p.gpus[r].used.is_none() { "not reported".into() } else { r1(outside[r]).to_string() };
            format!("held {}, in use outside the ledger {seen}", r1(holding[r]))
        };
        let mut out = String::new();
        for (i, g) in p.gpus.iter().enumerate() {
            out += &match g.unified {
                true => format!("GPU {i}  {}: unified memory, counted as RAM\n", g.name),
                false => format!("GPU {i}  {}: {} GB VRAM, {}\n", label(g), r1(g.total), usage(i)),
            };
        }
        if n == 0 {
            out += "GPUs   none the ledger can see\n";
        }
        out += &format!("RAM    {} GB, {}\nCPUs   {}, {}", r1(p.ram_total), usage(n), p.cpus, usage(n + 1));
        for t in &self.tickets {
            out += &format!("\n  {}  <{}>", self.line(t, &starts, now), t.session);
        }
        if self.tickets.is_empty() {
            out += "\n  (no open reservations)";
        }
        out
    }

    /// The ledger for people and agents: every open ticket, then the last closed ones,
    /// newest first. A table, or JSON.
    pub fn ledger(&mut self, now: f64, as_json: bool) -> Result<String, String> {
        let starts = self.tick(now);
        let closed: Vec<&Closed> = self.closed.iter().rev().collect();
        if as_json {
            let v = json!({"now": now, "open": self.tickets, "starts": starts, "closed": closed});
            return serde_json::to_string_pretty(&v).map_err(err);
        }
        let mut open = vec![["#", "state", "session", "needs", "GPUs", "time", "purpose"].map(String::from).to_vec()];
        for t in &self.tickets {
            let time = match t.end {
                None => starts.get(&t.id).map_or_else(|| "waits for outside use to drop".into(), |s| format!("starts in ~{}", dur(s - now))),
                Some(end) if now < end => format!("{} left", dur(end - now)),
                Some(end) => format!("out of time; expires in {}", dur(end + GRACE_S - now)),
            };
            let state = if t.granted.is_some() { "granted" } else { "queued" };
            open.push(vec![t.id.to_string(), state.into(), short(&t.session), fmt_need(&t.need), self.gpu_numbers(&t.gpus), time, t.purpose.clone()]);
        }
        let mut done = vec![["#", "how", "session", "needs", "GPUs", "closed", "held for", "purpose"].map(String::from).to_vec()];
        for c in closed {
            let t = &c.ticket;
            let held_for = t.granted.map_or_else(String::new, |g| dur(c.closed_at - g));
            let ago = format!("{} ago", dur(now - c.closed_at));
            done.push(vec![t.id.to_string(), c.how.clone(), short(&t.session), fmt_need(&t.need), self.gpu_numbers(&t.gpus), ago, held_for, t.purpose.clone()]);
        }
        Ok(format!(
            "{}\n\n{}",
            section("OPEN", &self.tickets.len().to_string(), &open),
            section("CLOSED", &format!("newest first; the ledger keeps the last {HISTORY}"), &done)))
    }

    /// This machine's resources, for the guide agents get when a session starts.
    pub fn pool(&mut self, now: f64) -> String {
        let p = self.readings(now);
        let mut out = format!("THIS MACHINE{}\n  RAM: {} GB    CPUs: {}", gpu_list(&p), r1(p.ram_total), p.cpus);
        if mixed_vendors(&p) {
            out += "\n  Its GPUs come from more than one vendor: every request with --vram must pass --gpu.";
        }
        out
    }

    /// Tick, then hand over (and forget) this session's undelivered notices.
    pub fn poll(&mut self, now: f64, session: &str) -> Polled {
        self.tick(now);
        let (mine, rest) = std::mem::take(&mut self.notices).into_iter().partition(|n| n.session == session);
        self.notices = rest;
        let mine: Vec<Notice> = mine;
        Polled {
            notices: mine.into_iter().map(|n| n.text).collect(),
            queued: self.tickets.iter().any(|t| t.session == session && t.granted.is_none()),
            open: self.tickets.iter().any(|t| t.session == session),
        }
    }

    pub fn lines_for(&mut self, now: f64, session: &str) -> String {
        let starts = self.tick(now);
        let lines: Vec<String> =
            self.tickets.iter().filter(|t| t.session == session).map(|t| self.line(t, &starts, now)).collect();
        lines.join("\n")
    }

    /// The session is gone: nobody can be woken for its queued tickets any more.
    pub fn end_session(&mut self, now: f64, session: &str) {
        let (gone, kept): (Vec<Ticket>, Vec<Ticket>) =
            std::mem::take(&mut self.tickets).into_iter().partition(|t| t.session == session && t.granted.is_none());
        self.tickets = kept;
        for t in gone {
            self.close(t, now, "cancelled");
        }
        self.notices.retain(|n| n.session != session);
        self.tick(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: f64 = 3600.0;
    const ROOM: [f64; 3] = [12.0, 32.0, 16.0]; // one 12 GB GPU, 32 GB RAM, 16 CPUs
    const T0: f64 = 1_000_000.0;
    const MIN: f64 = 60.0;

    /// [GB VRAM on one GPU, GB RAM, CPUs]
    fn need(v: [f64; 3]) -> Need {
        Need { vram: v[0], gpus: usize::from(v[0] > 0.0), ram: v[1], cpus: v[2], ..Need::default() }
    }

    fn starts(now: f64, holds: &[(f64, [f64; 3])], queued: &[(u64, f64, [f64; 3])]) -> Vec<(u64, f64)> {
        let needs: Vec<Need> = queued.iter().map(|q| need(q.2)).collect();
        let q: Vec<(u64, f64, &Need, Vec<usize>)> = queued.iter().zip(&needs).map(|(q, n)| (q.0, q.1, n, vec![0])).collect();
        let h: Vec<(f64, Vec<f64>)> = holds.iter().map(|h| (h.0, h.1.to_vec())).collect();
        let mut v: Vec<_> = plan(now, &ROOM, &[false], &h, &q).into_iter().map(|(id, (s, _))| (id, s)).collect();
        v.sort_by_key(|p| p.0);
        v
    }

    #[test]
    fn plan_fits_now_when_empty() {
        assert_eq!(starts(0.0, &[], &[(1, H, [8.0, 16.0, 4.0])]), [(1, 0.0)]);
    }

    #[test]
    fn plan_waits_for_hold_to_end() {
        assert_eq!(starts(0.0, &[(2.0 * H, [10.0, 0.0, 0.0])], &[(1, H, [8.0, 0.0, 0.0])]), [(1, 2.0 * H)]);
    }

    #[test]
    fn plan_queue_order_serialises_conflicting_requests() {
        let q = [(1, H, [8.0, 0.0, 0.0]), (2, H, [8.0, 0.0, 0.0])];
        assert_eq!(starts(0.0, &[], &q), [(1, 0.0), (2, H)]);
    }

    #[test]
    fn plan_backfill_uses_idle_dimension_without_delaying_head() {
        // The head needs the GPU, busy for 2 h; a CPU-only job behind it starts now.
        let q = [(1, H, [8.0, 0.0, 0.0]), (2, 3.0 * H, [0.0, 8.0, 8.0])];
        assert_eq!(starts(0.0, &[(2.0 * H, [10.0, 0.0, 0.0])], &q), [(1, 2.0 * H), (2, 0.0)]);
    }

    #[test]
    fn plan_backfill_refused_when_it_would_delay_head() {
        // The head waits for CPUs freed at 1 h; a 2 h job wanting those CPUs would push it back.
        let q = [(1, H, [0.0, 0.0, 16.0]), (2, 2.0 * H, [0.0, 0.0, 6.0])];
        assert_eq!(starts(0.0, &[(H, [0.0, 0.0, 10.0])], &q), [(1, H), (2, 2.0 * H)]);
    }

    #[test]
    fn plan_short_backfill_that_ends_before_head_is_allowed() {
        let q = [(1, H, [0.0, 0.0, 16.0]), (2, H / 2.0, [0.0, 0.0, 6.0])];
        assert_eq!(starts(0.0, &[(H, [0.0, 0.0, 10.0])], &q), [(1, H), (2, 0.0)]);
    }

    #[test]
    fn plan_window_checks_future_starts_not_only_window_start() {
        // 2 needs no VRAM, so it really does not conflict with 1 placed at 1 h.
        let q = [(1, H, [12.0, 0.0, 0.0]), (2, 2.0 * H, [0.0, 0.0, 16.0])];
        assert_eq!(starts(0.0, &[(H, [12.0, 0.0, 0.0])], &q), [(1, H), (2, 0.0)]);
        // 2 fits at 0 alone, but would exceed 12 GB while 1 runs.
        let q = [(1, H, [6.0, 0.0, 0.0]), (2, 2.0 * H, [7.0, 0.0, 0.0])];
        assert_eq!(starts(0.0, &[(H, [6.0, 0.0, 0.0])], &q), [(1, 0.0), (2, H)]);
    }

    #[test]
    fn plan_oversized_request_is_never_placed() {
        assert_eq!(starts(0.0, &[], &[(1, H, [13.0, 0.0, 0.0])]), []);
    }

    #[test]
    fn plan_expired_holds_are_ignored() {
        assert_eq!(starts(10.0, &[(5.0, ROOM)], &[(1, H, [12.0, 0.0, 0.0])]), [(1, 10.0)]);
    }

    fn gpu(id: &str, vendor: &str, total: f64, used: f64) -> Gpu {
        Gpu { id: id.into(), vendor: vendor.into(), name: format!("Test {id}"), total, used: Some(used), ..Gpu::default() }
    }

    /// A 32 GB, 16-CPU machine with these GPUs and this much in use.
    fn machine(gpus: Vec<Gpu>, ram_used: f64, cpus_busy: f64) {
        let p = Probe { gpus, ram_total: 32.0, ram_used, cpus: 16.0, cpus_busy, ..Probe::default() };
        probe::FAKE.set(Some(p));
    }

    fn fresh() -> State {
        machine(vec![gpu("g0", "nvidia", 12.0, 0.0)], 0.0, 0.0);
        State::default()
    }

    fn req(st: &mut State, session: &str, v: [f64; 3], minutes: f64, now: f64) -> String {
        st.request(now, session, need(v), None, minutes, "job").unwrap()
    }

    fn gpus_of(st: &State, id: u64) -> Vec<String> {
        st.tickets.iter().find(|t| t.id == id).unwrap().gpus.clone()
    }

    #[test]
    fn queued_then_granted_with_one_notice_and_eta() {
        let mut st = fresh();
        assert!(req(&mut st, "s:a", [10.0, 0.0, 0.0], 60.0, T0).contains("[granted]"));
        let b = req(&mut st, "s:b", [8.0, 0.0, 0.0], 30.0, T0);
        assert!(b.contains("[queued]") && b.contains("position 1, estimated start in 1 h."), "{b}");
        assert!(st.poll(T0, "s:a").notices.is_empty()); // the immediate grant was in the reply
        assert!(st.poll(T0 + MIN, "s:b").queued);
        st.release(T0 + 5.0 * MIN, 1).unwrap();
        let p = st.poll(T0 + 5.0 * MIN, "s:b");
        assert!(p.notices.len() == 1 && p.notices[0].contains("GRANTED") && p.notices[0].contains("on GPU 0 (Test g0)"));
        assert!(st.poll(T0 + 6.0 * MIN, "s:b").notices.is_empty()); // exactly once
    }

    #[test]
    fn follow_up_request_after_release_goes_behind_the_queue() {
        let mut st = fresh();
        req(&mut st, "s:a", [0.0, 0.0, 16.0], 60.0, T0);
        req(&mut st, "s:b", [0.0, 0.0, 16.0], 30.0, T0);
        st.release(T0 + 10.0 * MIN, 1).unwrap(); // a's task is done: b's turn
        assert!(st.poll(T0 + 10.0 * MIN, "s:b").notices[0].contains("GRANTED"));
        let a = req(&mut st, "s:a", [0.0, 0.0, 16.0], 60.0, T0 + 10.0 * MIN);
        assert!(a.contains("[queued]") && a.contains("position 1, estimated start in 30 min."), "{a}");
        // follow-up work that fits beside b without delaying anyone starts at once
        let small = req(&mut st, "s:a", [4.0, 0.0, 0.0], 20.0, T0 + 10.0 * MIN);
        assert!(small.contains("[granted]"), "{small}");
    }

    #[test]
    fn out_of_time_then_forced_expiry() {
        let mut st = fresh();
        req(&mut st, "s:a", [0.0, 0.0, 16.0], 30.0, T0);
        assert!(st.poll(T0 + 29.0 * MIN, "s:a").notices.is_empty());
        assert!(st.poll(T0 + 30.0 * MIN, "s:a").notices[0].contains("out of time"));
        assert!(st.poll(T0 + 44.0 * MIN, "s:a").open); // still held during grace
        let p = st.poll(T0 + 45.0 * MIN, "s:a");
        assert!(p.notices[0].contains("force-expired") && !p.open);
    }

    #[test]
    fn extend_moves_end_and_rearms_out_of_time() {
        let mut st = fresh();
        req(&mut st, "s:a", [0.0, 0.0, 4.0], 30.0, T0);
        st.poll(T0 + 31.0 * MIN, "s:a");
        assert!(st.extend(T0 + 32.0 * MIN, 1, 60.0).unwrap().contains("GRANTED: 1 h left"));
        assert!(st.poll(T0 + 80.0 * MIN, "s:a").notices.is_empty());
        assert!(st.poll(T0 + 92.0 * MIN, "s:a").notices[0].contains("out of time"));
    }

    #[test]
    fn outside_vram_use_blocks_gpu_jobs_but_not_cpu_jobs() {
        let mut st = fresh();
        machine(vec![gpu("g0", "nvidia", 12.0, 10.0)], 0.0, 0.0); // e.g. a training run started outside the ledger
        let a = req(&mut st, "s:a", [8.0, 0.0, 0.0], 60.0, T0);
        assert!(a.contains("[queued]") && a.contains("outside the ledger"), "{a}");
        assert!(req(&mut st, "s:b", [0.0, 0.0, 4.0], 60.0, T0).contains("[granted]"));
        machine(vec![gpu("g0", "nvidia", 12.0, 0.0)], 0.0, 0.0);
        st.probe = None;
        assert!(st.poll(T0 + 10.0, "s:a").notices[0].contains("GRANTED"));
    }

    #[test]
    fn outside_cpu_use_queues_cpu_jobs_until_it_drops() {
        let mut st = fresh();
        machine(vec![gpu("g0", "nvidia", 12.0, 0.0)], 0.0, 12.0); // someone is compiling
        let a = req(&mut st, "s:a", [0.0, 0.0, 8.0], 30.0, T0);
        assert!(a.contains("[queued]") && a.contains("outside the ledger"), "{a}");
        assert!(req(&mut st, "s:b", [0.0, 0.0, 4.0], 30.0, T0).contains("[granted]"));
        machine(vec![gpu("g0", "nvidia", 12.0, 0.0)], 0.0, 1.0);
        st.probe = None;
        assert!(st.poll(T0 + 10.0, "s:a").notices[0].contains("GRANTED"));
    }

    #[test]
    fn outside_use_is_measured_without_margin() {
        let mut st = fresh();
        machine(vec![gpu("g0", "nvidia", 12.0, 10.1)], 24.8, 0.0); // 1.9 GB VRAM and 7.2 GB RAM free
        assert!(req(&mut st, "s:a", [0.0, 4.0, 8.0], 15.0, T0).contains("[granted]"));
        assert!(req(&mut st, "s:b", [1.9, 0.0, 0.0], 15.0, T0).contains("[granted]"));
    }

    #[test]
    fn a_fresh_grant_counts_in_full_until_it_shows_up() {
        let mut st = fresh();
        machine(vec![gpu("g0", "nvidia", 12.0, 2.0)], 0.0, 0.0); // 2 GB used outside the ledger
        assert!(req(&mut st, "s:a", [6.0, 0.0, 0.0], 60.0, T0).contains("[granted]"));
        // a has not loaded yet, so the reading still says 2 GB: 12 - 2 - 6 leaves 4.
        let b = req(&mut st, "s:b", [5.0, 0.0, 0.0], 60.0, T0 + 1.0);
        assert!(b.contains("[queued]") && b.contains("estimated start in 1 h"), "{b}");
        // a has loaded: 8 GB read, 6 of it a's, so outside use is still 2 GB.
        machine(vec![gpu("g0", "nvidia", 12.0, 8.0)], 0.0, 0.0);
        st.probe = None;
        assert!(st.poll(T0 + 5.0 * MIN, "s:b").queued);
    }

    #[test]
    fn requests_spread_across_gpus() {
        let mut st = State::default();
        machine(vec![gpu("g0", "nvidia", 24.0, 0.0), gpu("g1", "nvidia", 24.0, 0.0)], 0.0, 0.0);
        for _ in 0..3 {
            req(&mut st, "s:a", [8.0, 0.0, 0.0], 60.0, T0);
        }
        assert_eq!([gpus_of(&st, 1), gpus_of(&st, 2), gpus_of(&st, 3)], [["g0"], ["g1"], ["g0"]]);
        assert!(st.status(T0).contains("GPU 0  Test g0: 24 GB VRAM, held 16"));
    }

    #[test]
    fn a_gpu_with_outside_use_loses_the_job_to_a_free_one() {
        let mut st = State::default();
        machine(vec![gpu("g0", "nvidia", 12.0, 10.0), gpu("g1", "nvidia", 12.0, 0.0)], 0.0, 0.0);
        let a = req(&mut st, "s:a", [8.0, 0.0, 0.0], 60.0, T0);
        assert!(a.contains("[granted]") && a.contains("on GPU 1 (Test g1)"), "{a}");
    }

    #[test]
    fn multi_gpu_requests_need_that_many_cards() {
        let mut st = State::default();
        machine(vec![gpu("g0", "nvidia", 24.0, 0.0), gpu("g1", "nvidia", 24.0, 0.0)], 0.0, 0.0);
        let two = Need { vram: 8.0, gpus: 2, ..Need::default() };
        let a = st.request(T0, "s:a", two, None, 60.0, "ddp").unwrap();
        assert!(a.contains("8 GB VRAM on each of 2 GPUs") && a.contains("GPU 0 (Test g0) and GPU 1 (Test g1)"), "{a}");
        assert!(req(&mut st, "s:b", [20.0, 0.0, 0.0], 60.0, T0).contains("[queued]")); // 16 left on each
        st.release(T0 + MIN, 1).unwrap();
        assert!(st.poll(T0 + MIN, "s:b").notices[0].contains("GRANTED"));
        let three = Need { vram: 1.0, gpus: 3, ..Need::default() };
        assert!(st.request(T0, "s:c", three, None, 5.0, "x").unwrap_err().contains("only 2 of the GPUs"));
    }

    #[test]
    fn mixed_vendors_need_gpu_and_gpu_picks_cards() {
        let mut st = State::default();
        machine(vec![gpu("g0", "nvidia", 12.0, 0.0), gpu("g1", "amd", 16.0, 0.0)], 0.0, 0.0);
        let e = st.request(T0, "s", need([4.0, 0.0, 0.0]), None, 5.0, "x").unwrap_err();
        assert!(e.contains("more than one vendor") && e.contains("GPU 1: Test g1 [amd], 16 GB VRAM"), "{e}");
        assert!(st.request(T0, "s", need([4.0, 0.0, 0.0]), Some("amd"), 5.0, "x").unwrap().contains("on GPU 1"));
        assert!(st.request(T0, "s", need([4.0, 0.0, 0.0]), Some("0"), 5.0, "x").unwrap().contains("on GPU 0"));
        assert!(st.request(T0, "s", need([14.0, 0.0, 0.0]), Some("nvidia"), 5.0, "x").unwrap_err().contains("only 0"));
        assert!(st.request(T0, "s", need([4.0, 0.0, 0.0]), Some("intel"), 5.0, "x").unwrap_err().contains("no such GPU"));
        assert!(st.request(T0, "s", need([4.0, 0.0, 0.0]), Some("0,7"), 5.0, "x").unwrap_err().contains("no such GPU"));
        assert!(st.request(T0, "s", need([0.0, 0.0, 2.0]), Some("0"), 5.0, "x").unwrap_err().contains("go with --vram"));
        assert!(st.request(T0, "s", need([0.0, 0.0, 2.0]), None, 5.0, "x").is_ok()); // no GPU, no question
    }

    #[test]
    fn unified_gpu_memory_is_counted_as_ram() {
        let mut st = State::default();
        let apple = Gpu { id: "apple".into(), vendor: "apple".into(), name: "Apple M3".into(), total: 32.0, unified: true, ..Gpu::default() };
        machine(vec![apple], 0.0, 0.0);
        let a = req(&mut st, "s:a", [20.0, 0.0, 0.0], 60.0, T0);
        assert!(a.contains("[granted]") && a.contains("on GPU 0 (Apple M3)"), "{a}");
        assert!(req(&mut st, "s:b", [0.0, 16.0, 0.0], 60.0, T0).contains("[queued]")); // 20 + 16 > 32
        assert!(st.request(T0, "s", need([30.0, 4.0, 0.0]), None, 5.0, "x").unwrap_err().contains("capacity"));
    }

    #[test]
    fn invalid_requests_rejected() {
        let mut st = fresh();
        assert!(st.request(T0, "s", need([13.0, 0.0, 0.0]), None, 10.0, "x").unwrap_err().contains("only 0 of the GPUs"));
        assert!(st.request(T0, "s", need([0.0, 33.0, 0.0]), None, 10.0, "x").unwrap_err().contains("capacity"));
        assert!(st.request(T0, "s", need([0.0, 0.0, 0.0]), None, 10.0, "x").is_err());
        assert!(st.request(T0, "s", need([0.0, 0.0, 1.0]), None, 0.0, "x").is_err());
        assert!(st.request(T0, "s", need([f64::NAN, 0.0, 1.0]), None, 5.0, "x").is_err());
        machine(Vec::new(), 0.0, 0.0);
        st.probe = None;
        assert!(st.request(T0, "s", need([1.0, 0.0, 0.0]), None, 10.0, "x").unwrap_err().contains("no GPU"));
        assert!(st.release(T0, 99).is_err());
        assert!(st.tickets.is_empty());
    }

    #[test]
    fn end_session_cancels_only_its_queued_tickets() {
        let mut st = fresh();
        req(&mut st, "s:x", [0.0, 0.0, 16.0], 60.0, T0);
        req(&mut st, "s:a", [0.0, 0.0, 16.0], 60.0, T0);
        st.end_session(T0, "s:a");
        assert!(!st.poll(T0, "s:a").open);
        assert!(st.poll(T0, "s:x").open);
    }

    #[test]
    fn closed_tickets_are_kept_with_how_they_closed() {
        let mut st = fresh();
        req(&mut st, "s:a", [0.0, 0.0, 16.0], 30.0, T0); // #1 granted
        req(&mut st, "s:b", [0.0, 0.0, 16.0], 30.0, T0); // #2 queued
        req(&mut st, "s:c", [0.0, 0.0, 16.0], 30.0, T0); // #3 queued
        st.release(T0, 2).unwrap();
        st.end_session(T0, "s:c");
        st.poll(T0 + 45.0 * MIN, "s:a"); // #1 is force-expired
        req(&mut st, "s:a", [0.0, 0.0, 1.0], 30.0, T0 + 45.0 * MIN); // #4 granted
        st.release(T0 + 50.0 * MIN, 4).unwrap();
        let hows: Vec<(u64, &str)> = st.closed.iter().map(|c| (c.ticket.id, c.how.as_str())).collect();
        assert_eq!(hows, [(2, "cancelled"), (3, "cancelled"), (1, "expired"), (4, "released")]);
        for _ in 0..HISTORY {
            req(&mut st, "s:a", [0.0, 0.0, 1.0], 30.0, T0 + 50.0 * MIN);
            st.release(T0 + 50.0 * MIN, st.next_id).unwrap();
        }
        assert!(st.closed.len() == HISTORY && st.closed[0].ticket.id == 5);
    }

    #[test]
    fn ledger_view_lists_open_then_closed() {
        let mut st = fresh();
        assert_eq!(st.ledger(T0, false).unwrap(), "OPEN: none\n\nCLOSED: none");
        req(&mut st, "claude:5c1e7a42-8b3d-4f06", [8.0, 16.0, 4.0], 90.0, T0); // #1 granted on GPU 0
        req(&mut st, "codex:0199a2b3-aaaa", [0.0, 0.0, 16.0], 30.0, T0); // #2 queued until #1 ends
        req(&mut st, "s:x", [0.0, 0.0, 1.0], 10.0, T0); // #3 backfilled
        st.release(T0 + 5.0 * MIN, 3).unwrap();
        let v = st.ledger(T0 + 30.0 * MIN, false).unwrap();
        let lines: Vec<&str> = v.lines().collect();
        assert_eq!(lines[0], "OPEN (2)");
        assert_eq!(lines[2], "  1  granted  claude:5c1e7a42  8 GB VRAM, 16 GB RAM, 4 CPUs  0     1 h left        job");
        assert!(lines[3].starts_with("  2  queued   codex:0199a2b3   16 CPUs ") && lines[3].ends_with("starts in ~1 h  job"), "{v}");
        assert_eq!(lines[5], "CLOSED (newest first; the ledger keeps the last 20)");
        assert_eq!(lines[7], "  3  released  s:x      1 CPUs        25 min ago  5 min     job");

        let j: serde_json::Value = serde_json::from_str(&st.ledger(T0 + 30.0 * MIN, true).unwrap()).unwrap();
        assert_eq!(j["open"][0]["session"], "claude:5c1e7a42-8b3d-4f06");
        assert_eq!(j["starts"]["2"], T0 + 90.0 * MIN);
        assert!(j["closed"][0]["id"] == 3 && j["closed"][0]["how"] == "released" && j["closed"][0]["closed_at"] == T0 + 5.0 * MIN);
    }

    #[test]
    fn a_ledger_from_before_history_still_loads() {
        let st: State = serde_json::from_str(r#"{"next_id":3,"probe":null,"tickets":[],"notices":[]}"#).unwrap();
        assert!(st.next_id == 3 && st.closed.is_empty());
    }
}
