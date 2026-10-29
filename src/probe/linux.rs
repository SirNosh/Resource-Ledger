//! Linux: /proc for RAM and CPU time, sysfs for AMD and Intel GPUs.

use std::fs;
use std::path::Path;

use super::Gpu;

pub fn ram() -> (f64, f64) {
    super::parse_meminfo(&fs::read_to_string("/proc/meminfo").unwrap_or_default())
}

pub fn cpu_ticks() -> [u64; 2] {
    super::parse_proc_stat(&fs::read_to_string("/proc/stat").unwrap_or_default())
}

/// NVIDIA cards expose no VRAM files in sysfs, so they only ever come from nvidia-smi.
pub fn gpus() -> Vec<Gpu> {
    super::drm_gpus(Path::new("/sys/class/drm"))
}
