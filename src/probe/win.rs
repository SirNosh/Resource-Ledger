//! Windows: RAM and CPU time from kernel32; AMD/Intel GPUs from DXCore, with their
//! VRAM in use from the graphics kernel's per-segment statistics (system-wide, the
//! figure Task Manager shows as dedicated GPU memory).

use std::ffi::CStr;

use windows::Wdk::Graphics::Direct3D::{
    D3DKMT_QUERYSTATISTICS, D3DKMT_QUERYSTATISTICS_ADAPTER, D3DKMT_QUERYSTATISTICS_SEGMENT, D3DKMTQueryStatistics,
};
use windows::Win32::Foundation::{FILETIME, LUID};
use windows::Win32::Graphics::DXCore::*;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;

use super::{GB, Gpu};

pub fn ram() -> (f64, f64) {
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    // SAFETY: m is a MEMORYSTATUSEX with dwLength set, as the API requires.
    let _ = unsafe { GlobalMemoryStatusEx(&mut m) };
    (m.ullTotalPhys as f64 / GB, m.ullTotalPhys.saturating_sub(m.ullAvailPhys) as f64 / GB)
}

pub fn cpu_ticks() -> [u64; 2] {
    let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: three valid out-pointers.
    let _ = unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) };
    let t = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    let total = t(kernel) + t(user); // kernel time includes idle time
    [total.saturating_sub(t(idle)), total]
}

/// Discrete AMD and Intel GPUs DXCore sees. NVIDIA GPUs come from `nvidia-smi`, and other
/// vendors (e.g. a VM's virtual adapter) are skipped, as on Linux.
pub fn gpus() -> Vec<Gpu> {
    adapters().unwrap_or_default()
}

fn adapters() -> windows::core::Result<Vec<Gpu>> {
    // SAFETY: DXCore COM calls; every property buffer is the size of the property's type.
    unsafe {
        let factory: IDXCoreAdapterFactory = DXCoreCreateAdapterFactory()?;
        let list: IDXCoreAdapterList = factory.CreateAdapterList(&[DXCORE_ADAPTER_ATTRIBUTE_D3D12_CORE_COMPUTE])?;
        let mut out = Vec::new();
        for i in 0..list.GetAdapterCount() {
            let a: IDXCoreAdapter = list.GetAdapter(i)?;
            let vendor = match prop::<DXCoreHardwareID>(&a, HardwareID)?.vendorID {
                0x1002 => "amd",
                0x8086 => "intel",
                _ => continue,
            };
            if prop::<u8>(&a, IsHardware)? == 0 || prop::<u8>(&a, IsIntegrated)? != 0 {
                continue;
            }
            let luid: LUID = prop(&a, InstanceLuid)?;
            let mut name = vec![0u8; a.GetPropertySize(DriverDescription)?];
            a.GetProperty(DriverDescription, name.len(), name.as_mut_ptr().cast())?;
            out.push(Gpu {
                id: format!("luid_0x{:08x}_0x{:08x}", luid.HighPart as u32, luid.LowPart),
                vendor: vendor.into(),
                name: CStr::from_bytes_until_nul(&name).map_or_else(|_| "GPU".into(), |s| s.to_string_lossy().trim().to_string()),
                total: prop::<u64>(&a, DedicatedAdapterMemory)? as f64 / GB,
                used: dedicated_in_use(luid),
                select: None,
                unified: false,
            });
        }
        Ok(out)
    }
}

/// # Safety
/// `T` must be the type DXCore documents for `p`.
unsafe fn prop<T: Default>(a: &IDXCoreAdapter, p: DXCoreAdapterProperty) -> windows::core::Result<T> {
    let mut v = T::default();
    unsafe { a.GetProperty(p, size_of::<T>(), (&mut v as *mut T).cast())? };
    Ok(v)
}

/// GB resident in the adapter's own memory segments (aperture segments are system RAM).
fn dedicated_in_use(luid: LUID) -> Option<f64> {
    // SAFETY: each query is a fully initialised D3DKMT_QUERYSTATISTICS that the kernel fills in
    // (the binding's pointer is const, but the kernel writes the result: hence `&raw mut`).
    unsafe {
        let mut q = D3DKMT_QUERYSTATISTICS { Type: D3DKMT_QUERYSTATISTICS_ADAPTER, AdapterLuid: luid, ..Default::default() };
        D3DKMTQueryStatistics(&raw mut q).ok().ok()?;
        let mut bytes = 0;
        for segment in 0..q.QueryResult.AdapterInformation.NbSegments {
            let mut q = D3DKMT_QUERYSTATISTICS { Type: D3DKMT_QUERYSTATISTICS_SEGMENT, AdapterLuid: luid, ..Default::default() };
            q.Anonymous.QuerySegment.SegmentId = segment;
            D3DKMTQueryStatistics(&raw mut q).ok().ok()?;
            let s = q.QueryResult.SegmentInformation;
            if s.Aperture == 0 {
                bytes += s.BytesResident;
            }
        }
        Some(bytes as f64 / GB)
    }
}
