//! The shared ledger: one JSON file, every operation under one exclusive file lock.
//!
//! There is no daemon. Every command and hook runs `tick` under the lock, which
//! applies the clock (out of time, forced expiry) and grants whatever the plan
//! places at `now`; sessions waiting for a turn poll, so something drives the
//! clock while it matters. A ticket is queued until granted, then held until it
//! is released or expires. Closed tickets are dropped from the file.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::probe::{self, Probe};

const EPS: f64 = 1e-9;
const GRACE_S: f64 = 15.0 * 60.0; // a grant is force-expired this long after its time is up
const WARMUP_S: f64 = 180.0; // grants younger than this may not have allocated their memory yet
const PROBE_CACHE_S: f64 = 5.0;
const MAX_NOTICES: usize = 100; // undeliverable notices (crashed sessions) must not pile up

/// GB of VRAM, GB of RAM, logical CPUs.
pub type Need = [f64; 3];

#[derive(Clone, Serialize, Deserialize)]
pub struct Ticket {
    pub id: u64,
    pub session: String,
    pub purpose: String,
    pub need: Need,
    pub minutes: f64,
    pub granted: Option<f64>,
    pub end: Option<f64>,
    #[serde(default)]
    pub warned: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Notice {
    pub session: String,
    pub ticket: u64,
    pub text: String,
}

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    probe: Option<Probe>,
    #[serde(default)]
    tickets: Vec<Ticket>,
    #[serde(default)]
    notices: Vec<Notice>,
}

#[derive(Default)]
struct Tick {
    starts: HashMap<u64, f64>,
    blocked: HashSet<u64>, // the ledger has room, the machine's live free memory does not
}

pub struct Polled {
    pub notices: Vec<String>,
    pub queued: bool,
    pub open: bool,
}

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

pub fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("RESOURCE_LEDGER_HOME") {
        return h.into();
    }
    std::env::var_os("LOCALAPPDATA").map_or_else(std::env::temp_dir, PathBuf::from).join("resource-ledger")
}

pub fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Run `f` on the ledger while holding its lock; the file is rewritten only if `f` changed it.
pub fn with<T>(f: impl FnOnce(&mut State) -> Result<T, String>) -> Result<T, String> {
    let dir = home();
    fs::create_dir_all(&dir).map_err(err)?;
    let lock = fs::OpenOptions::new().create(true).write(true).open(dir.join("ledger.lock")).map_err(err)?;
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

/// Conservative backfill: place queued requests in order, each at the earliest time
/// it fits for its whole duration on top of everything already placed, so nobody is
/// delayed by someone behind them.
///
/// Usage is piecewise constant and only drops at an interval end, so the earliest
/// feasible start is `now` or one of those ends; within a window usage only rises
/// at interval starts, so checking the window start plus each start inside it is
/// enough. Once every interval has ended usage is zero, so any request within
/// capacity always gets a slot.
///
/// `holds`: (end, need) of granted tickets, occupying [now, end).
/// `queued`: (id, duration in s, need) in queue order. Ids over capacity are left out.
fn plan(now: f64, cap: Need, holds: &[(f64, Need)], queued: &[(u64, f64, Need)]) -> HashMap<u64, f64> {
    let within = |used: &Need| (0..3).all(|i| used[i] <= cap[i] + EPS);
    let mut placed: Vec<(f64, f64, Need)> =
        holds.iter().filter(|h| h.0 > now).map(|&(end, need)| (now, end, need)).collect();
    let mut starts = HashMap::new();
    for &(id, dur, need) in queued {
        if !within(&need) {
            continue;
        }
        let mut candidates: Vec<f64> =
            std::iter::once(now).chain(placed.iter().map(|p| p.1).filter(|&e| e > now)).collect();
        candidates.sort_by(f64::total_cmp);
        candidates.dedup();
        let fits = |t: f64| {
            let end = t + dur;
            std::iter::once(t).chain(placed.iter().map(|p| p.0).filter(|&s| t < s && s < end)).all(|x| {
                let mut used = need;
                for (s, e, n) in &placed {
                    if *s <= x && x < *e {
                        (0..3).for_each(|i| used[i] += n[i]);
                    }
                }
                within(&used)
            })
        };
        if let Some(t) = candidates.into_iter().find(|&t| fits(t)) {
            starts.insert(id, t);
            placed.push((t, t + dur, need));
        }
    }
    starts
}

pub fn fmt_need(n: Need) -> String {
    let parts: Vec<String> = [(n[0], " GB VRAM"), (n[1], " GB RAM"), (n[2], " CPUs")]
        .iter()
        .filter(|(x, _)| *x > EPS)
        .map(|(x, unit)| format!("{}{unit}", (x * 10.0).round() / 10.0))
        .collect();
    if parts.is_empty() { "nothing".into() } else { parts.join(", ") }
}

fn dur(secs: f64) -> String {
    let m = (secs.max(0.0) / 60.0).ceil() as u64;
    match (m / 60, m % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

impl State {
    fn readings(&mut self, now: f64) -> Probe {
        match self.probe {
            Some(p) if (0.0..PROBE_CACHE_S).contains(&(now - p.at)) => p,
            _ => {
                let p = probe::read(now);
                self.probe = Some(p);
                p
            }
        }
    }

    fn notify(&mut self, t: &Ticket, text: String) {
        self.notices.push(Notice { session: t.session.clone(), ticket: t.id, text });
    }

    /// Apply the clock, then grant every queued ticket that the plan starts now and
    /// that the machine really has the memory for.
    fn tick(&mut self, now: f64) -> Tick {
        let mut notes = Vec::new();
        self.tickets.retain_mut(|t| {
            let Some(end) = t.end else { return true };
            if now >= end + GRACE_S {
                notes.push((t.clone(), format!(
                    "[resource-ledger] Ticket #{} ({}) was force-expired 15 min after its time ran out; its \
                     resources are now available to others. If that work is still running it may now collide.",
                    t.id, t.purpose)));
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
        let excess = self.notices.len().saturating_sub(MAX_NOTICES);
        self.notices.drain(..excess);

        let queued: Vec<(u64, f64, Need)> =
            self.tickets.iter().filter(|t| t.granted.is_none()).map(|t| (t.id, t.minutes * 60.0, t.need)).collect();
        if queued.is_empty() {
            return Tick::default();
        }
        let holds: Vec<(f64, Need)> = self
            .tickets
            .iter()
            .filter_map(|t| t.end.map(|e| (if now >= e { e + GRACE_S } else { e }, t.need)))
            .collect();
        let r = self.readings(now);
        let starts = plan(now, [r.vram_total, r.ram_total, r.cpus], &holds, &queued);

        // Live guard: memory used outside the ledger, and grantees that have not
        // allocated yet, are invisible to the arithmetic above, so a grant also
        // needs the memory to be really free right now.
        let free = [r.vram_free, r.ram_free];
        let mut pending = [0.0, 0.0];
        for t in self.tickets.iter().filter(|t| t.granted.is_some_and(|g| g > now - WARMUP_S)) {
            (0..2).for_each(|i| pending[i] += t.need[i]);
        }
        let mut blocked = HashSet::new();
        let mut granted = Vec::new();
        for t in self.tickets.iter_mut().filter(|t| t.granted.is_none() && starts.get(&t.id) == Some(&now)) {
            if (0..2).any(|i| t.need[i] > EPS && free[i] + EPS < t.need[i] + pending[i]) {
                blocked.insert(t.id);
                continue;
            }
            (0..2).for_each(|i| pending[i] += t.need[i]);
            t.granted = Some(now);
            t.end = Some(now + t.minutes * 60.0);
            granted.push(t.clone());
        }
        for t in granted {
            let text = format!(
                "[resource-ledger] Your turn: ticket #{} ({}) is GRANTED: {} for {} min. Do this task, then `resource-ledger release {}` as soon as it is done; \
                 further work needs a new request.",
                t.id, t.purpose, fmt_need(t.need), t.minutes, t.id);
            self.notify(&t, text);
        }
        Tick { starts, blocked }
    }

    fn line(&self, t: &Ticket, tick: &Tick, now: f64) -> String {
        let state = if t.granted.is_some() { "granted" } else { "queued" };
        let head = format!("#{} [{state}] {} for {} min: {}", t.id, fmt_need(t.need), t.minutes, t.purpose);
        match t.end {
            None => {
                let pos = self.tickets.iter().filter(|q| q.granted.is_none()).position(|q| q.id == t.id).unwrap_or(0) + 1;
                let when = if tick.blocked.contains(&t.id) {
                    "; waiting for memory used outside the ledger to free up".to_string()
                } else if let Some(&s) = tick.starts.get(&t.id) {
                    format!(", estimated start in {}", dur(s - now))
                } else {
                    String::new()
                };
                format!("{head}. QUEUED: position {pos}{when}. End your turn; you will be woken when it is granted.")
            }
            Some(end) if now < end => format!(
                "{head}. GRANTED: {} left. `resource-ledger release {}` as soon as this task is done (further work needs \
                 a new request); `resource-ledger extend {} MINUTES` only if this task runs long.",
                dur(end - now), t.id, t.id),
            Some(end) => format!(
                "{head}. OUT OF TIME: `resource-ledger release {}` or `resource-ledger extend {} MINUTES`; force-expires in {}.",
                t.id, t.id, dur(end + GRACE_S - now)),
        }
    }

    fn line_of(&self, id: u64, tick: &Tick, now: f64) -> String {
        self.tickets.iter().find(|t| t.id == id).map_or_else(String::new, |t| self.line(t, tick, now))
    }

    pub fn request(&mut self, now: f64, session: &str, need: Need, minutes: f64, purpose: &str) -> Result<String, String> {
        if !(minutes > 0.0 && minutes.is_finite()) || need.iter().any(|x| !(*x >= 0.0 && x.is_finite())) {
            return Err("--minutes must be > 0 and resources must be >= 0".into());
        }
        if need.iter().all(|&x| x <= EPS) {
            return Err("request at least one of --vram, --ram, --cpus".into());
        }
        let r = self.readings(now);
        let cap = [r.vram_total, r.ram_total, r.cpus];
        if (0..3).any(|i| need[i] > cap[i] + EPS) {
            return Err(format!("request exceeds this machine's capacity ({})", fmt_need(cap)));
        }
        self.next_id += 1;
        let id = self.next_id;
        self.tickets.push(Ticket {
            id, session: session.into(), purpose: purpose.into(), need, minutes, granted: None, end: None, warned: false,
        });
        let tick = self.tick(now);
        self.notices.retain(|n| n.ticket != id); // an immediate grant is reported by this reply
        Ok(self.line_of(id, &tick, now))
    }

    pub fn release(&mut self, now: f64, id: u64) -> Result<String, String> {
        let i = self.tickets.iter().position(|t| t.id == id).ok_or(format!("no open ticket #{id}"))?;
        let t = self.tickets.remove(i);
        self.notices.retain(|n| n.ticket != id);
        self.tick(now);
        let what = if t.granted.is_some() { "released" } else { "cancelled" };
        Ok(format!("#{id} {what} ({}).", t.purpose))
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
        let tick = self.tick(now);
        Ok(self.line_of(id, &tick, now))
    }

    pub fn status(&mut self, now: f64) -> String {
        let tick = self.tick(now);
        let r = self.readings(now);
        let mut held = [0.0; 3];
        for t in self.tickets.iter().filter(|t| t.granted.is_some()) {
            (0..3).for_each(|i| held[i] += t.need[i]);
        }
        let mut out = format!(
            "capacity {} | held {} | live free {}",
            fmt_need([r.vram_total, r.ram_total, r.cpus]), fmt_need(held), fmt_need([r.vram_free, r.ram_free, 0.0]));
        for t in &self.tickets {
            out += &format!("\n  {}  <{}>", self.line(t, &tick, now), t.session);
        }
        if self.tickets.is_empty() {
            out += "\n  (no open reservations)";
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
        let tick = self.tick(now);
        let lines: Vec<String> =
            self.tickets.iter().filter(|t| t.session == session).map(|t| self.line(t, &tick, now)).collect();
        lines.join("\n")
    }

    /// The session is gone: nobody can be woken for its queued tickets any more.
    pub fn end_session(&mut self, now: f64, session: &str) {
        self.tickets.retain(|t| t.session != session || t.granted.is_some());
        self.notices.retain(|n| n.session != session);
        self.tick(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: f64 = 3600.0;
    const CAP: Need = [12.0, 32.0, 16.0];
    const T0: f64 = 1_000_000.0;
    const MIN: f64 = 60.0;

    fn starts(now: f64, holds: &[(f64, Need)], queued: &[(u64, f64, Need)]) -> Vec<(u64, f64)> {
        let mut v: Vec<_> = plan(now, CAP, holds, queued).into_iter().collect();
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
        assert_eq!(starts(10.0, &[(5.0, CAP)], &[(1, H, [12.0, 0.0, 0.0])]), [(1, 10.0)]);
    }

    fn machine(vram_free: f64, ram_free: f64) {
        probe::FAKE.set(Some(Probe { at: 0.0, vram_total: 12.0, vram_free, ram_total: 32.0, ram_free, cpus: 16.0 }));
    }

    fn fresh() -> State {
        machine(12.0, 32.0);
        State::default()
    }

    fn req(st: &mut State, session: &str, need: Need, minutes: f64, now: f64) -> String {
        st.request(now, session, need, minutes, "job").unwrap()
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
        assert!(p.notices.len() == 1 && p.notices[0].contains("GRANTED"));
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
    fn live_guard_blocks_untracked_vram_but_not_cpu_jobs() {
        let mut st = fresh();
        machine(2.0, 32.0); // e.g. a training run started outside the ledger
        let a = req(&mut st, "s:a", [8.0, 0.0, 0.0], 60.0, T0);
        assert!(a.contains("[queued]") && a.contains("outside the ledger"), "{a}");
        assert!(req(&mut st, "s:b", [0.0, 0.0, 4.0], 60.0, T0).contains("[granted]"));
        machine(12.0, 32.0);
        st.probe = None;
        assert!(st.poll(T0 + 10.0, "s:a").notices[0].contains("GRANTED"));
    }

    #[test]
    fn live_guard_uses_actual_free_memory_without_margin() {
        let mut st = fresh();
        machine(1.9, 7.2); // the chat that v1's fixed 4 GB headroom left stuck
        assert!(req(&mut st, "s:a", [0.0, 4.0, 8.0], 15.0, T0).contains("[granted]"));
        assert!(req(&mut st, "s:b", [1.9, 0.0, 0.0], 15.0, T0).contains("[granted]"));
    }

    #[test]
    fn live_guard_counts_fresh_grants_not_yet_allocated() {
        let mut st = fresh();
        machine(10.0, 32.0); // 2 GB held outside the ledger
        assert!(req(&mut st, "s:a", [6.0, 0.0, 0.0], 60.0, T0).contains("[granted]"));
        // a has not allocated yet, so 10 GB still reads free. The ledger has room for
        // b (6 + 5 <= 12) but the machine will not once a allocates.
        let b = req(&mut st, "s:b", [5.0, 0.0, 0.0], 60.0, T0 + 1.0);
        assert!(b.contains("[queued]") && b.contains("outside the ledger"), "{b}");
    }

    #[test]
    fn invalid_requests_rejected() {
        let mut st = fresh();
        assert!(st.request(T0, "s", [13.0, 0.0, 0.0], 10.0, "x").unwrap_err().contains("capacity"));
        assert!(st.request(T0, "s", [0.0, 0.0, 0.0], 10.0, "x").is_err());
        assert!(st.request(T0, "s", [0.0, 0.0, 1.0], 0.0, "x").is_err());
        assert!(st.request(T0, "s", [f64::NAN, 0.0, 1.0], 5.0, "x").is_err());
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
}
