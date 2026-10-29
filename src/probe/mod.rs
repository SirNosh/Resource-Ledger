//! Machine readings in GB: what each resource has (capacity) and what is in use right
//! now by everything, inside the ledger or not.
//!
//! NVIDIA GPUs come from `nvidia-smi` on Windows and Linux; other GPUs from the OS
//! itself (DXCore on Windows, sysfs on Linux, the unified GPU on Apple Silicon).
//! Integrated GPUs, whose "VRAM" is system RAM, are left out, except Apple
//! Silicon's, which is the only GPU those machines have. RESOURCE_LEDGER_FAKE_PROBE
//! (a JSON Probe) replaces the real readings; the tests use it.

#[cfg_attr(windows, path = "win.rs")]
#[cfg_attr(target_os = "linux", path = "linux.rs")]
#[cfg_attr(target_os = "macos", path = "mac.rs")]
mod os;

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Gpu {
    /// Stable across readings: NVIDIA UUID, Windows LUID, Linux PCI address, or "apple".
    pub id: String,
    /// nvidia, amd, intel or apple.
    pub vendor: String,
    pub name: String,
    pub total: f64,
    /// In use by everything; None where the driver does not report it.
    pub used: Option<f64>,
    /// How a program targets this GPU, e.g. `CUDA_VISIBLE_DEVICES=GPU-…`.
    pub select: Option<String>,
    /// Apple Silicon: the GPU's memory is system RAM, so VRAM is counted as RAM.
    pub unified: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Probe {
    pub at: f64,
    pub gpus: Vec<Gpu>,
    pub ram_total: f64,
    pub ram_used: f64,
    pub cpus: f64,
    /// CPUs busy, averaged since the previous reading.
    pub cpus_busy: f64,
    /// The OS's cumulative (busy, total) CPU time, kept for the next reading's average.
    pub cpu_ticks: [u64; 2],
}

#[cfg(test)]
thread_local! {
    pub static FAKE: std::cell::RefCell<Option<Probe>> = const { std::cell::RefCell::new(None) };
}

pub const GB: f64 = (1u64 << 30) as f64;
const CPU_SAMPLE: std::time::Duration = std::time::Duration::from_millis(200);

/// Read the machine. `prev` (the last reading) lets the CPU figure average over the
/// time since then instead of sampling for 200 ms.
pub fn read(now: f64, prev: Option<&Probe>) -> Probe {
    #[cfg(test)]
    if let Some(p) = FAKE.with_borrow(Clone::clone) {
        return Probe { at: now, ..p };
    }
    if let Ok(fake) = std::env::var("RESOURCE_LEDGER_FAKE_PROBE") {
        let p: Probe = serde_json::from_str(&fake).expect("RESOURCE_LEDGER_FAKE_PROBE is not a valid probe");
        return Probe { at: now, ..p };
    }
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    let mut ticks = os::cpu_ticks();
    let since = match prev {
        Some(p) if (0.0..60.0).contains(&(now - p.at)) && p.cpu_ticks[1] > 0 && ticks[1] > p.cpu_ticks[1] => p.cpu_ticks,
        _ => {
            let first = ticks;
            std::thread::sleep(CPU_SAMPLE);
            ticks = os::cpu_ticks();
            first
        }
    };
    let (ram_total, ram_used) = os::ram();
    let mut gpus = nvidia_smi();
    gpus.extend(os::gpus());
    Probe { at: now, gpus, ram_total, ram_used, cpus, cpus_busy: busy_share(since, ticks) * cpus, cpu_ticks: ticks }
}

/// Share of CPU time spent busy between two cumulative (busy, total) readings.
fn busy_share(a: [u64; 2], b: [u64; 2]) -> f64 {
    let total = b[1].saturating_sub(a[1]);
    if total == 0 { 0.0 } else { (b[0].saturating_sub(a[0]) as f64 / total as f64).clamp(0.0, 1.0) }
}

/// Every NVIDIA GPU (no other reader lists them). `nvidia-smi` ships with the driver on
/// Windows and Linux; running it as a child keeps the driver's libraries (~20 MB) out of
/// long-lived waiter processes.
fn nvidia_smi() -> Vec<Gpu> {
    if cfg!(target_os = "macos") {
        return Vec::new();
    }
    let mut cmd = Command::new("nvidia-smi");
    cmd.args(["--query-gpu=uuid,name,memory.total,memory.free", "--format=csv,noheader,nounits"]);
    cmd.stdin(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000); // CREATE_NO_WINDOW
    match cmd.output() {
        Ok(out) if out.status.success() => parse_nvidia_smi(&String::from_utf8_lossy(&out.stdout)),
        _ => Vec::new(),
    }
}

/// `uuid, name, total MiB, free MiB` per line; a name may itself contain commas.
fn parse_nvidia_smi(text: &str) -> Vec<Gpu> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            let [uuid, .., total, free] = f.as_slice() else { return None };
            let total: f64 = total.parse().ok()?;
            Some(Gpu {
                id: uuid.to_string(),
                vendor: "nvidia".into(),
                name: f[1..f.len() - 2].join(", "),
                total: total / 1024.0,
                used: free.parse::<f64>().ok().map(|free| (total - free) / 1024.0),
                select: Some(format!("CUDA_VISIBLE_DEVICES={uuid}")),
                unified: false,
            })
        })
        .filter(|g| g.id.starts_with("GPU-") && !g.name.is_empty())
        .collect()
}

/// Discrete AMD and Intel GPUs under `root` (`/sys/class/drm`). NVIDIA cards are left
/// to `nvidia-smi`; integrated Intel GPUs have no local memory and so no VRAM files.
/// AMD APUs cannot be told apart from discrete cards here and are included.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn drm_gpus(root: &Path) -> Vec<Gpu> {
    let mut cards: Vec<(u32, std::path::PathBuf)> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| Some((e.file_name().to_str()?.strip_prefix("card")?.parse().ok()?, e.path())))
        .collect();
    cards.sort();
    cards
        .into_iter()
        .filter_map(|(_, card)| {
            let dev = card.join("device");
            let read = |p: &Path| fs::read_to_string(p).ok().map(|s| s.trim().to_string());
            let num = |p: &Path| read(p)?.parse::<f64>().ok();
            let pci = read(&dev.join("uevent"))?.lines().find_map(|l| l.strip_prefix("PCI_SLOT_NAME=").map(str::to_string))?;
            let (vendor, total, used, select) = match read(&dev.join("vendor"))?.as_str() {
                "0x1002" => {
                    let id = read(&dev.join("unique_id")).and_then(|u| u64::from_str_radix(&u, 16).ok()).filter(|&u| u != 0);
                    let select = id.map(|u| format!("ROCR_VISIBLE_DEVICES=GPU-{u:016x}"));
                    ("amd", num(&dev.join("mem_info_vram_total"))?, num(&dev.join("mem_info_vram_used")), select)
                }
                "0x8086" => {
                    let total = [card.join("lmem_total_bytes"), dev.join("lmem_total_bytes"), dev.join("tile0/physical_vram_size_bytes")]
                        .iter()
                        .find_map(|p| num(p))?;
                    ("intel", total, None, None) // the drivers do not report VRAM in use in sysfs
                }
                _ => return None,
            };
            let name = read(&dev.join("product_name")).filter(|s| !s.is_empty()).unwrap_or_else(|| {
                format!("{} GPU {}", vendor.to_uppercase(), read(&dev.join("device")).unwrap_or_default())
            });
            Some(Gpu { name: format!("{name} (PCI {pci})"), id: pci, vendor: vendor.into(), total: total / GB, used: used.map(|u| u / GB), select, unified: false })
        })
        .collect()
}

/// (total, in use) GB from /proc/meminfo: MemTotal and MemTotal − MemAvailable.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_meminfo(text: &str) -> (f64, f64) {
    let kb = |key: &str| {
        text.lines().find_map(|l| l.strip_prefix(key)?.trim().strip_suffix("kB")?.trim().parse::<f64>().ok()).unwrap_or(0.0)
    };
    let (total, available) = (kb("MemTotal:"), kb("MemAvailable:"));
    (total * 1024.0 / GB, (total - available).max(0.0) * 1024.0 / GB)
}

/// Cumulative (busy, total) CPU time from the `cpu` line of /proc/stat; iowait counts as idle.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_stat(text: &str) -> [u64; 2] {
    let Some(line) = text.lines().find(|l| l.starts_with("cpu ")) else { return [0, 0] };
    let v: Vec<u64> = line.split_whitespace().skip(1).take(8).filter_map(|x| x.parse().ok()).collect();
    let total: u64 = v.iter().sum();
    let idle = v.get(3).unwrap_or(&0) + v.get(4).unwrap_or(&0);
    [total - idle, total]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_smi_lists_every_gpu_with_its_selector() {
        let g = parse_nvidia_smi(
            "GPU-12345678-90ab-cdef-1234-567890abcdef, NVIDIA GeForce RTX 4070 SUPER, 12282, 10587\n\
             GPU-aaaaaaaa-0000-0000-0000-000000000001, NVIDIA RTX A6000, 49140, [N/A]\n",
        );
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].name, "NVIDIA GeForce RTX 4070 SUPER");
        assert!((g[0].total - 11.994).abs() < 0.01 && (g[0].used.unwrap() - 1.655).abs() < 0.01);
        assert_eq!(g[0].select.as_deref(), Some("CUDA_VISIBLE_DEVICES=GPU-12345678-90ab-cdef-1234-567890abcdef"));
        assert!(g[1].used.is_none()); // free not reported
        assert!(parse_nvidia_smi("NVIDIA-SMI has failed because it couldn't communicate with the driver.").is_empty());
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    #[test]
    fn sysfs_finds_discrete_amd_and_intel_but_not_integrated_or_nvidia() {
        let root = std::env::temp_dir().join(format!("resource-ledger-drm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        // card0: AMD discrete with a unique id; card1: Intel iGPU; card2: Intel Arc; card3: NVIDIA
        write(&root, "card0/device/vendor", "0x1002\n");
        write(&root, "card0/device/device", "0x744c\n");
        write(&root, "card0/device/uevent", "DRIVER=amdgpu\nPCI_SLOT_NAME=0000:03:00.0\n");
        write(&root, "card0/device/mem_info_vram_total", "25753026560\n");
        write(&root, "card0/device/mem_info_vram_used", "1073741824\n");
        write(&root, "card0/device/unique_id", "a1b2c3d4e5f60718\n");
        write(&root, "card0-DP-1/status", "connected\n");
        write(&root, "card1/device/vendor", "0x8086\n");
        write(&root, "card1/device/uevent", "PCI_SLOT_NAME=0000:00:02.0\n");
        write(&root, "card2/device/vendor", "0x8086\n");
        write(&root, "card2/device/device", "0xe20b\n");
        write(&root, "card2/device/uevent", "PCI_SLOT_NAME=0000:04:00.0\n");
        write(&root, "card2/device/tile0/physical_vram_size_bytes", "12884901888\n");
        write(&root, "card3/device/vendor", "0x10de\n");
        write(&root, "card3/device/uevent", "PCI_SLOT_NAME=0000:01:00.0\n");
        let g = drm_gpus(&root);
        let _ = fs::remove_dir_all(&root);
        assert_eq!(g.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(), ["0000:03:00.0", "0000:04:00.0"]);
        assert_eq!((g[0].vendor.as_str(), g[0].total, g[0].used), ("amd", 25753026560.0 / GB, Some(1.0)));
        assert_eq!(g[0].select.as_deref(), Some("ROCR_VISIBLE_DEVICES=GPU-a1b2c3d4e5f60718"));
        assert_eq!((g[1].vendor.as_str(), g[1].total, g[1].used), ("intel", 12.0, None));
        assert!(g[1].name.contains("0000:04:00.0"));
    }

    #[test]
    fn proc_files_parse() {
        let (total, used) = parse_meminfo("MemTotal:       32768000 kB\nMemFree:  1000 kB\nMemAvailable:   24576000 kB\n");
        assert!((total - 31.25).abs() < 1e-9 && (used - 7.8125).abs() < 1e-9);
        assert_eq!(parse_proc_stat("cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3 4\n"), [150, 1000]);
        assert_eq!(busy_share([150, 1000], [450, 2000]), 0.3);
        assert_eq!(busy_share([5, 5], [5, 5]), 0.0);
    }

    /// Prints this machine's real readings and checks they are physically possible:
    /// `cargo test real_probe -- --ignored --nocapture` (CI runs it on every OS).
    #[test]
    #[ignore]
    fn real_probe() {
        let first = read(1000.0, None);
        println!("{first:#?}");
        std::thread::sleep(std::time::Duration::from_secs(1));
        let again = read(1001.0, Some(&first));
        println!("busy CPUs over the last second: {:.2}", again.cpus_busy);
        for p in [&first, &again] {
            assert!(p.ram_total > 0.0 && (0.0..=p.ram_total).contains(&p.ram_used), "RAM: {p:?}");
            assert!(p.cpus >= 1.0 && (0.0..=p.cpus + 1e-6).contains(&p.cpus_busy), "CPUs: {p:?}");
            assert!(p.gpus.iter().all(|g| g.total > 0.0 && g.used.is_none_or(|u| (0.0..=g.total).contains(&u))), "GPUs: {p:?}");
        }
    }
}
