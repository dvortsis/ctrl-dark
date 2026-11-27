//! Bounded-timeout DDC/CI VCP 0xD6 (Power Mode) helper via `dxva2.dll`.
//! Always executed off the Win32 message thread with a strict timeout so hung I2C/AUX
//! channels never block hotkey or mouse input.

use std::collections::HashSet;
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Devices::Display::{
    DestroyPhysicalMonitors, GetNumberOfPhysicalMonitorsFromHMONITOR,
    GetPhysicalMonitorsFromHMONITOR, GetVCPFeatureAndVCPFeatureReply, SetVCPFeature,
    PHYSICAL_MONITOR,
};
use windows_sys::Win32::Foundation::{BOOL, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};

use crate::display::{DisplayInfo, DisplayRole};
use crate::win_util::from_wide_slice;

const VCP_POWER_MODE: u8 = 0xD6;
const VCP_POWER_ON: u32 = 0x01;
const DDC_TIMEOUT_MS: u64 = 750;

static DDC_WORKERS: AtomicUsize = AtomicUsize::new(0);
struct DdcPermit;
impl DdcPermit {
    fn acquire() -> Option<Self> {
        let mut count = DDC_WORKERS.load(Ordering::Relaxed);
        while count < 8 {
            match DDC_WORKERS.compare_exchange_weak(
                count,
                count + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(Self),
                Err(current) => count = current,
            }
        }
        None
    }
}
impl Drop for DdcPermit {
    fn drop(&mut self) {
        DDC_WORKERS.fetch_sub(1, Ordering::Release);
    }
}

#[derive(Debug, Clone)]
pub struct DdcPowerReading {
    pub gdi_name: String,
    pub vcp_power_mode: u32,
}

fn collect_hmonitors(allowed_gdi: HashSet<String>) -> Vec<(String, isize)> {
    struct Ctx {
        allowed: HashSet<String>,
        found: Vec<(String, isize)>,
    }

    unsafe extern "system" fn enum_proc(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let ctx = &mut *(lparam as *mut Ctx);
        let mut info: MONITORINFOEXW = zeroed();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(hmon, &mut info.monitorInfo) != 0 {
            let gdi = from_wide_slice(&info.szDevice);
            if ctx.allowed.contains(&gdi) {
                ctx.found.push((gdi, hmon as isize));
            }
        }
        1
    }

    let mut ctx = Ctx {
        allowed: allowed_gdi,
        found: Vec::new(),
    };
    unsafe {
        EnumDisplayMonitors(
            null_mut(),
            null_mut(),
            Some(enum_proc),
            (&mut ctx as *mut Ctx) as LPARAM,
        );
    }
    ctx.found
}

fn ddc_eligible_gdis(displays: &[DisplayInfo]) -> HashSet<String> {
    displays
        .iter()
        .filter(|d| d.active && d.role == DisplayRole::External && !d.gdi_name.is_empty())
        .map(|d| d.gdi_name.clone())
        .collect()
}

/// Fires DDC/CI `VCP 0xD6 = 0x01` wake commands asynchronously so I2C never delays Stage 2.
pub fn wake_external_monitors_async(displays: Vec<DisplayInfo>) {
    if ddc_eligible_gdis(&displays).is_empty() {
        return;
    }
    thread::Builder::new()
        .name("ddc-wake".to_string())
        .spawn(move || {
            let woken = wake_external_monitors(&displays);
            if woken > 0 {
                crate::logging::info(format!(
                    "Async DDC wake signaled {woken} external display(s)"
                ));
            }
        })
        .ok();
}

fn pick_power_reading(readings: impl IntoIterator<Item = Option<u32>>) -> Option<u32> {
    let mut fallback = None;
    for val in readings.into_iter().flatten() {
        if val != VCP_POWER_ON {
            return Some(val);
        }
        if fallback.is_none() {
            fallback = Some(val);
        }
    }
    fallback
}

/// Sends DDC/CI `VCP 0xD6 = 0x01` (Power On) to external DDC-capable displays with a strict timeout.
pub fn wake_external_monitors(displays: &[DisplayInfo]) -> usize {
    let eligible = ddc_eligible_gdis(displays);
    if eligible.is_empty() {
        return 0;
    }
    let targets = collect_hmonitors(eligible);
    if targets.is_empty() {
        return 0;
    }

    let mut receivers = Vec::with_capacity(targets.len());
    let deadline = Instant::now() + Duration::from_millis(DDC_TIMEOUT_MS);
    for (_gdi, hmon_raw) in targets {
        let Some(permit) = DdcPermit::acquire() else {
            continue;
        };
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _permit = permit;
            let results = run_on_physical_monitors(hmon_raw as HMONITOR, |phys| {
                let rc =
                    unsafe { SetVCPFeature(phys.hPhysicalMonitor, VCP_POWER_MODE, VCP_POWER_ON) };
                rc != 0
            });
            let _ = tx.send(results.into_iter().any(|ok| ok));
        });
        receivers.push(rx);
    }

    let mut woken = 0usize;
    for rx in receivers {
        if matches!(
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now())),
            Ok(true)
        ) {
            woken += 1;
        }
    }
    woken
}

/// Reads DDC/CI `VCP 0xD6` power state for external DDC-capable displays within `DDC_TIMEOUT_MS`.
pub fn read_external_power_states(displays: &[DisplayInfo]) -> Vec<DdcPowerReading> {
    let eligible = ddc_eligible_gdis(displays);
    if eligible.is_empty() {
        return Vec::new();
    }
    let targets = collect_hmonitors(eligible);
    if targets.is_empty() {
        return Vec::new();
    }

    let mut receivers = Vec::with_capacity(targets.len());
    let deadline = Instant::now() + Duration::from_millis(DDC_TIMEOUT_MS);
    for (gdi, hmon_raw) in targets {
        let Some(permit) = DdcPermit::acquire() else {
            continue;
        };
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _permit = permit;
            let readings = run_on_physical_monitors(hmon_raw as HMONITOR, |phys| {
                let mut vct = 0i32;
                let mut cur = 0u32;
                let mut max_v = 0u32;
                let rc = unsafe {
                    GetVCPFeatureAndVCPFeatureReply(
                        phys.hPhysicalMonitor,
                        VCP_POWER_MODE,
                        &mut vct,
                        &mut cur,
                        &mut max_v,
                    )
                };
                if rc != 0 {
                    Some(cur)
                } else {
                    None
                }
            });
            let _ = tx.send(pick_power_reading(readings).map(|v| DdcPowerReading {
                gdi_name: gdi,
                vcp_power_mode: v,
            }));
        });
        receivers.push(rx);
    }

    let mut out = Vec::new();
    for rx in receivers {
        if let Ok(Some(reading)) =
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            out.push(reading);
        }
    }
    out
}

fn run_on_physical_monitors<T, F>(hmon: HMONITOR, mut op: F) -> Vec<T>
where
    F: FnMut(&PHYSICAL_MONITOR) -> T,
{
    let mut count = 0u32;
    if unsafe { GetNumberOfPhysicalMonitorsFromHMONITOR(hmon, &mut count) } == 0 || count == 0 {
        return Vec::new();
    }
    let mut arr: Vec<PHYSICAL_MONITOR> = vec![unsafe { zeroed() }; count as usize];
    if unsafe { GetPhysicalMonitorsFromHMONITOR(hmon, count, arr.as_mut_ptr()) } == 0 {
        return Vec::new();
    }
    let results: Vec<T> = arr.iter().map(&mut op).collect();
    unsafe {
        DestroyPhysicalMonitors(count, arr.as_ptr());
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stalled_ddc_workers_have_a_fixed_budget() {
        let permits: Vec<_> = (0..8).map(|_| DdcPermit::acquire().unwrap()).collect();
        assert!(DdcPermit::acquire().is_none());
        drop(permits);
        assert!(DdcPermit::acquire().is_some());
    }

    #[test]
    fn pick_power_reading_prefers_non_on_secondary_panels() {
        assert_eq!(pick_power_reading([Some(0x01), Some(0x04)]), Some(0x04));
        assert_eq!(pick_power_reading([None, Some(0x01)]), Some(0x01));
        assert_eq!(pick_power_reading([None, None]), None);
    }
}
