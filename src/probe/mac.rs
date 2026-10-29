//! macOS: RAM and CPU time from the Mach host statistics. Apple Silicon has one GPU,
//! which uses system RAM (unified memory). GPUs in Intel Macs are not supported.

use std::ffi::CStr;

use super::{GB, Gpu};

fn sysctl_u64(name: &CStr) -> u64 {
    let (mut v, mut len) = (0u64, size_of::<u64>());
    // SAFETY: the buffer is a u64 and `len` is its size.
    unsafe { libc::sysctlbyname(name.as_ptr(), (&mut v as *mut u64).cast(), &mut len, std::ptr::null_mut(), 0) };
    v
}

fn sysctl_string(name: &CStr) -> String {
    let mut len = 0usize;
    // SAFETY: the first call only asks for the size; the second fills a buffer of that size.
    unsafe {
        libc::sysctlbyname(name.as_ptr(), std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0);
        let mut buf = vec![0u8; len];
        libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0);
        CStr::from_bytes_until_nul(&buf).map_or_else(|_| String::new(), |s| s.to_string_lossy().into_owned())
    }
}

#[allow(deprecated)] // libc points to the mach2 crate for mach_host_self; the call itself is stable
fn host() -> libc::mach_port_t {
    // SAFETY: returns a send right to this host's port.
    unsafe { libc::mach_host_self() }
}

pub fn ram() -> (f64, f64) {
    let total = sysctl_u64(c"hw.memsize");
    // SAFETY: plain C struct, filled by host_statistics64 with `count` set to its size.
    let mut vm: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    unsafe { libc::host_statistics64(host(), libc::HOST_VM_INFO64, (&mut vm as *mut libc::vm_statistics64).cast(), &mut count) };
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let available = (u64::from(vm.free_count) + u64::from(vm.inactive_count)) * page;
    (total as f64 / GB, total.saturating_sub(available) as f64 / GB)
}

pub fn cpu_ticks() -> [u64; 2] {
    // SAFETY: plain C struct, filled by host_statistics with `count` set to its size.
    let mut info: libc::host_cpu_load_info = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    unsafe { libc::host_statistics(host(), libc::HOST_CPU_LOAD_INFO, (&mut info as *mut libc::host_cpu_load_info).cast(), &mut count) };
    let t = |s: libc::c_int| u64::from(info.cpu_ticks[s as usize]);
    let busy = t(libc::CPU_STATE_USER) + t(libc::CPU_STATE_SYSTEM) + t(libc::CPU_STATE_NICE);
    [busy, busy + t(libc::CPU_STATE_IDLE)]
}

pub fn gpus() -> Vec<Gpu> {
    if !cfg!(target_arch = "aarch64") {
        return Vec::new();
    }
    vec![Gpu {
        id: "apple".into(),
        vendor: "apple".into(),
        name: sysctl_string(c"machdep.cpu.brand_string"),
        total: sysctl_u64(c"hw.memsize") as f64 / GB,
        used: None, // its memory is RAM, measured as RAM
        select: None,
        unified: true,
    }]
}
