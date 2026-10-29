//! Machine readings in GB: totals (capacity) and live free memory (the grant guard).
//!
//! Windows WDDM does not attribute VRAM per process, so only aggregate GPU 0
//! figures are used. RESOURCE_LEDGER_FAKE_PROBE (a JSON object with the fields below)
//! replaces the real readings; the tests use it.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Probe {
    #[serde(default)]
    pub at: f64,
    pub vram_total: f64,
    pub vram_free: f64,
    pub ram_total: f64,
    pub ram_free: f64,
    pub cpus: f64,
}

#[cfg(test)]
thread_local! {
    pub static FAKE: std::cell::Cell<Option<Probe>> = const { std::cell::Cell::new(None) };
}

const GB: f64 = (1u64 << 30) as f64;

pub fn read(now: f64) -> Probe {
    #[cfg(test)]
    if let Some(p) = FAKE.get() {
        return Probe { at: now, ..p };
    }
    if let Ok(fake) = std::env::var("RESOURCE_LEDGER_FAKE_PROBE") {
        let p: Probe = serde_json::from_str(&fake).expect("RESOURCE_LEDGER_FAKE_PROBE is not a valid probe");
        return Probe { at: now, ..p };
    }
    let (ram_total, ram_free) = ram();
    let mut p = Probe {
        at: now,
        ram_total,
        ram_free,
        cpus: std::thread::available_parallelism().map_or(1, |n| n.get()) as f64,
        ..Probe::default()
    };
    // No NVIDIA GPU or driver: VRAM capacity stays 0 and GPU requests are refused.
    if let Some((total, free)) = gpu() {
        p.vram_total = total;
        p.vram_free = free;
    }
    p
}

/// GPU 0 (total, free) in GB. nvidia-smi ships with the NVIDIA driver and costs about
/// the same as loading NVML in-process, but as a child it keeps the driver's libraries
/// (~20 MB) out of long-lived waiter processes.
fn gpu() -> Option<(f64, f64)> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("nvidia-smi")
        .args(["--id=0", "--query-gpu=memory.total,memory.free", "--format=csv,noheader,nounits"])
        .stdin(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut mib = text.trim().split(',').map(|s| s.trim().parse::<f64>());
    match (mib.next(), mib.next()) {
        (Some(Ok(total)), Some(Ok(free))) if out.status.success() => Some((total / 1024.0, free / 1024.0)),
        _ => None,
    }
}

fn ram() -> (f64, f64) {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: MEMORYSTATUSEX is plain data; dwLength is set as the API requires.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    unsafe { GlobalMemoryStatusEx(&mut m) };
    (m.ullTotalPhys as f64 / GB, m.ullAvailPhys as f64 / GB)
}
