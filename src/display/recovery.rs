//! 4-stage anti-black-screen display recovery pipeline.

use std::collections::HashSet;
use std::mem::size_of;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows_sys::Win32::System::Power::{
    SetThreadExecutionState, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, GetDesktopWindow, MB_OK, SC_MONITORPOWER, WM_SYSCOMMAND,
};

#[link(name = "user32")]
extern "system" {
    fn MessageBeep(utype: u32) -> i32;
}

use crate::config::AppConfig;
use crate::display::ccd::{
    deactivate_targets, force_mode_recommit, has_live_gdi_mode, query_available_targets,
    query_displays, reset_gdi_scanout, DeactivateResult,
};
use crate::display::snapshot::{
    apply_snapshot, apply_snapshot_with_targets, capture_active_snapshot, needs_topology_restore,
    TopologySnapshot,
};
use crate::display::{ddc, matches_selector, DisplayInfo, DisplayRole};
use crate::logging;
use crate::window_restore::{self, SavedWindow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryLevel {
    Standard,
    HardBounce,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WakeContext {
    pub was_selective_off: bool,
    pub console_display_off: bool,
    pub from_session_unlock: bool,
}

fn is_transient_unlock_ccd_error(err: &str) -> bool {
    err.ends_with("error 5") || err.ends_with("error 31")
}

#[derive(Debug, Clone)]
pub struct RecoveryReport {
    pub level: RecoveryLevel,
    pub topology_restored: bool,
    pub retrained_displays: Vec<String>,
    pub bounced_paths: bool,
    pub windows_restored: usize,
    pub stage1_ms: u64,
    pub stage2_ms: u64,
    pub stage3_ms: u64,
    pub stage4_ms: u64,
    pub stage4_gdi_wait_ms: u64,
    pub total_ms: u64,
    pub used_relaxed_timings: bool,
    pub summary: String,
}

/// Sends OS DPMS monitor-sleep command (`SC_MONITORPOWER = 2`).
pub fn send_monitors_off_power(hwnd_raw: Option<usize>) {
    let target = hwnd_raw
        .map(|h| h as HWND)
        .unwrap_or_else(|| unsafe { GetDesktopWindow() });
    unsafe {
        DefWindowProcW(
            target,
            WM_SYSCOMMAND,
            SC_MONITORPOWER as WPARAM,
            2 as LPARAM,
        );
    }
}

/// Sends Stage-1 authoritative OS wake pulses + micro mouse nudge.
pub fn send_os_wake_pulse(hwnd_raw: Option<usize>) {
    let target = hwnd_raw
        .map(|h| h as HWND)
        .unwrap_or_else(|| unsafe { GetDesktopWindow() });
    unsafe {
        SetThreadExecutionState(ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
        DefWindowProcW(
            target,
            WM_SYSCOMMAND,
            SC_MONITORPOWER as WPARAM,
            (-1isize) as LPARAM,
        );
    }
    send_micro_mouse_nudge();
}

fn send_micro_mouse_nudge() {
    let inputs = [
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 1,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: -1,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    ];
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        );
    }
}

/// Returns the effective HPD settle timeout for Stage 2. When waking from a software
/// `SelectiveOff` state while the console display stayed on, connected monitors are already
/// enumerated — waiting `settle_timeout_ms` for an unplugged/powered-off display would stall.
fn effective_settle_timeout_ms(
    configured_ms: u64,
    level: RecoveryLevel,
    wake_ctx: WakeContext,
) -> u64 {
    if level == RecoveryLevel::Standard
        && wake_ctx.was_selective_off
        && !wake_ctx.console_display_off
    {
        0
    } else {
        configured_ms
    }
}

/// Decides whether Stage 3 should run a scanout retrain on `disp`.
fn should_retrain_display(
    level: RecoveryLevel,
    topology_restored: bool,
    retrain_usb_on_wake: bool,
    disp: &DisplayInfo,
    is_always_bounce: bool,
    has_live_gdi: bool,
    ddc_non_on: bool,
) -> bool {
    if level == RecoveryLevel::HardBounce || is_always_bounce || ddc_non_on {
        return true;
    }
    if !topology_restored && !has_live_gdi {
        return true;
    }
    retrain_usb_on_wake
        && disp.role == DisplayRole::UsbIndirect
        && !topology_restored
        && (!has_live_gdi || disp.resolution == (0, 0))
}

/// Executes the full 4-stage display recovery pipeline.
pub fn run_recovery(
    level: RecoveryLevel,
    config: &AppConfig,
    snapshot: Option<&TopologySnapshot>,
    saved_windows: &[SavedWindow],
    hwnd_raw: Option<usize>,
    wake_ctx: WakeContext,
) -> Result<RecoveryReport, String> {
    let t_total = Instant::now();
    logging::info(format!("Starting display recovery ({level:?})"));

    // Stage 1: Non-blocking OS wake pulse + async DDC wake.
    let t1 = Instant::now();
    let needs_os_wake = !wake_ctx.was_selective_off
        || wake_ctx.console_display_off
        || level == RecoveryLevel::HardBounce;
    if needs_os_wake {
        send_os_wake_pulse(hwnd_raw);
    }
    if config.recovery.ddc_assist
        && (!wake_ctx.was_selective_off || level == RecoveryLevel::HardBounce)
    {
        if let Ok(live) = query_displays(false) {
            ddc::wake_external_monitors_async(live);
        }
    }
    let stage1_ms = t1.elapsed().as_millis() as u64;

    // Stage 2: Fast HPD settle poll + single-pass dynamic-LUID CCD topology restoration.
    let t2 = Instant::now();
    let mut topology_restored = false;
    let mut used_relaxed_timings = false;
    let mut cached_active: Option<Vec<DisplayInfo>> = None;

    if let Some(snap) = snapshot {
        let settle_ms =
            effective_settle_timeout_ms(config.recovery.settle_timeout_ms, level, wake_ctx);
        let deadline = Instant::now() + Duration::from_millis(settle_ms);
        let expected_keys: HashSet<String> = snap
            .monitors
            .iter()
            .map(|m| m.stable_key.to_ascii_lowercase())
            .collect();

        let avail_targets = loop {
            let targets = query_available_targets().unwrap_or_default();
            let avail_keys: HashSet<String> = targets
                .iter()
                .filter(|t| t.target_available)
                .map(|t| t.stable_key.to_ascii_lowercase())
                .collect();

            if expected_keys.iter().all(|k| avail_keys.contains(k)) || Instant::now() >= deadline {
                break targets;
            }
            thread::sleep(Duration::from_millis(50));
        };

        let active_displays = query_displays(false).unwrap_or_default();
        if needs_topology_restore(snap, &active_displays, &avail_targets) {
            let save_db = level == RecoveryLevel::HardBounce;
            match apply_snapshot_with_targets(snap, &avail_targets, save_db) {
                Ok(outcome) => {
                    topology_restored = true;
                    used_relaxed_timings = outcome.used_relaxed_timings;
                    logging::info(format!(
                        "Stage 2 CCD topology restored (newly_activated={}, relaxed_timings={})",
                        outcome.newly_activated_keys.len(),
                        outcome.used_relaxed_timings
                    ));
                }
                Err(e) if wake_ctx.from_session_unlock && is_transient_unlock_ccd_error(&e) => {
                    logging::info(format!(
                        "Transient CCD lock error on session unlock ({e}); retrying in 150ms"
                    ));
                    thread::sleep(Duration::from_millis(150));
                    let retry_targets = query_available_targets().unwrap_or(avail_targets);
                    match apply_snapshot_with_targets(snap, &retry_targets, save_db) {
                        Ok(outcome) => {
                            topology_restored = true;
                            used_relaxed_timings = outcome.used_relaxed_timings;
                            logging::info(format!(
                                "Stage 2 CCD topology restored on unlock retry (newly_activated={}, relaxed_timings={})",
                                outcome.newly_activated_keys.len(),
                                outcome.used_relaxed_timings
                            ));
                        }
                        Err(e2) => {
                            logging::warn(format!("Stage 2 apply_snapshot warning: {e2}"));
                        }
                    }
                }
                Err(e) => {
                    logging::warn(format!("Stage 2 apply_snapshot warning: {e}"));
                }
            }
        } else {
            cached_active = Some(active_displays);
        }
    } else if needs_os_wake {
        thread::sleep(Duration::from_millis(120));
    }
    let stage2_ms = t2.elapsed().as_millis() as u64;

    // Stage 3: Targeted anti-black-screen link retrain & optional hard path bounce.
    let t3 = Instant::now();
    let mut live_after_stage2 = match cached_active {
        Some(active) if !topology_restored => active,
        _ => query_displays(false).unwrap_or_default(),
    };
    let mut retrain_gdis: Vec<String> = Vec::new();
    let mut retrained_names: Vec<String> = Vec::new();

    let ddc_states = if level == RecoveryLevel::HardBounce && config.recovery.ddc_assist {
        ddc::read_external_power_states(&live_after_stage2)
    } else {
        Vec::new()
    };

    for (idx, disp) in live_after_stage2.iter().enumerate() {
        let is_always_bounce = config
            .recovery
            .always_bounce_monitors
            .iter()
            .any(|sel| matches_selector(disp, idx + 1, sel));
        let has_live_gdi = if level == RecoveryLevel::HardBounce || !topology_restored {
            has_live_gdi_mode(&disp.gdi_name)
        } else {
            true
        };
        let ddc_non_on = ddc_states
            .iter()
            .any(|r| r.gdi_name == disp.gdi_name && r.vcp_power_mode != 0x01);

        let should_retrain = should_retrain_display(
            level,
            topology_restored,
            config.recovery.retrain_usb_displays_on_wake,
            disp,
            is_always_bounce,
            has_live_gdi,
            ddc_non_on,
        );

        if should_retrain && !disp.gdi_name.is_empty() {
            retrain_gdis.push(disp.gdi_name.clone());
            retrained_names.push(disp.friendly_name.clone());
        }
    }

    let mut recommit_failed = false;
    if level == RecoveryLevel::HardBounce {
        let targets = if retrain_gdis.is_empty() {
            live_after_stage2
                .iter()
                .filter(|d| !d.gdi_name.is_empty())
                .map(|d| d.gdi_name.clone())
                .collect()
        } else {
            retrain_gdis.clone()
        };
        if let Err(e) = force_mode_recommit(&targets) {
            logging::warn(format!("Stage 3 force_mode_recommit warning: {e}"));
            recommit_failed = true;
        }
    } else if !retrain_gdis.is_empty() {
        reset_gdi_scanout(&retrain_gdis);
    }

    let mut bounced_paths = false;
    let needs_hard_bounce = level == RecoveryLevel::HardBounce
        || recommit_failed
        || (!topology_restored
            && live_after_stage2
                .iter()
                .any(|d| d.active && !has_live_gdi_mode(&d.gdi_name)));

    if needs_hard_bounce {
        bounced_paths = perform_targeted_path_bounce(&live_after_stage2)?;
    }
    let stage3_ms = t3.elapsed().as_millis() as u64;

    if !retrain_gdis.is_empty() || bounced_paths || level == RecoveryLevel::HardBounce {
        live_after_stage2 = query_displays(false)?;
    }
    if live_after_stage2.is_empty() {
        return Err("Recovery completed without any active displays".into());
    }
    if let Some(saved) = snapshot {
        let available = query_available_targets()?;
        if needs_topology_restore(saved, &live_after_stage2, &available) {
            return Err("Recovery could not restore the saved layout of the connected displays. Check connections and try Force Recover.".into());
        }
    }

    // Stage 4: Wait briefly for GDI monitor registration only if needed, then async-restore windows.
    let t4 = Instant::now();
    let mut stage4_gdi_wait_ms = 0u64;
    let restore_stats = if config.recovery.restore_windows && !saved_windows.is_empty() {
        let (gdi_wait, live_work) = window_restore::wait_for_gdi_monitors(&live_after_stage2, 200);
        stage4_gdi_wait_ms = gdi_wait;
        window_restore::restore_windows(saved_windows, &live_after_stage2, &live_work)
    } else {
        window_restore::RestoreStats::default()
    };
    let windows_restored = restore_stats.restored;
    let stage4_ms = t4.elapsed().as_millis() as u64;
    let total_ms = t_total.elapsed().as_millis() as u64;

    if level == RecoveryLevel::HardBounce && config.recovery.audio_cue_on_force_recover {
        unsafe {
            MessageBeep(MB_OK);
        }
    }

    let summary = format!(
        "Recovery {:?} complete in {}ms [s1={}ms, s2={}ms, s3={}ms, s4={}ms(gdi={}ms,max_win={}ms,thr={},failed={})]: active={}, topology_restored={}, relaxed={}, retrained={}, bounced={}, windows_restored={}",
        level,
        total_ms,
        stage1_ms,
        stage2_ms,
        stage3_ms,
        stage4_ms,
        stage4_gdi_wait_ms,
        restore_stats.max_window_ms,
        restore_stats.threads_used,
        restore_stats.failed,
        live_after_stage2.len(),
        topology_restored,
        used_relaxed_timings,
        retrained_names.len(),
        bounced_paths,
        windows_restored
    );
    logging::info(&summary);

    Ok(RecoveryReport {
        level,
        topology_restored,
        retrained_displays: retrained_names,
        bounced_paths,
        windows_restored,
        stage1_ms,
        stage2_ms,
        stage3_ms,
        stage4_ms,
        stage4_gdi_wait_ms,
        total_ms,
        used_relaxed_timings,
        summary,
    })
}

fn perform_targeted_path_bounce(live_displays: &[DisplayInfo]) -> Result<bool, String> {
    // A bounce must restore the exact live topology, including displays absent from
    // an older saved snapshot. Never deactivate paths without a rollback baseline.
    let baseline = match capture_active_snapshot() {
        Ok(snapshot) => snapshot,
        Err(e) => {
            logging::warn(format!(
                "Skipping path bounce: cannot capture rollback topology: {e}"
            ));
            return Ok(false);
        }
    };
    let mut external_keys: Vec<String> = live_displays
        .iter()
        .filter(|d| d.active && d.role.is_external_like())
        .map(|d| d.stable_key.clone())
        .collect();

    if external_keys.is_empty() {
        return Ok(false);
    }

    // If no internal panel is active, leave the first monitor active so Windows does not reject
    // deactivating every CCD path, and bounce the remaining monitors.
    let has_internal = live_displays
        .iter()
        .any(|d| d.active && d.role == DisplayRole::Internal);
    if !has_internal && external_keys.len() > 1 {
        external_keys.remove(0);
    }

    let did_bounce = match deactivate_targets(&external_keys) {
        Ok(DeactivateResult::Deactivated(n)) if n > 0 => {
            logging::info(format!(
                "Stage 3 hard bounce deactivated {n} path(s) for 450ms"
            ));
            thread::sleep(Duration::from_millis(450));
            true
        }
        _ => false,
    };

    if did_bounce {
        apply_snapshot(&baseline, false).map_err(|e| format!("Path bounce restore failed: {e}"))?;
        let gdis: Vec<String> = query_displays(false)
            .unwrap_or_default()
            .into_iter()
            .filter(|d| !d.gdi_name.is_empty())
            .map(|d| d.gdi_name)
            .collect();
        let _ = force_mode_recommit(&gdis);
    }

    Ok(did_bounce)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_disp(role: DisplayRole, resolution: (u32, u32)) -> DisplayInfo {
        DisplayInfo {
            friendly_name: "Sample".into(),
            stable_key: "display#sample#1".into(),
            gdi_name: r"\\.\DISPLAY1".into(),
            adapter_path: String::new(),
            edid_code: "SMP0001".into(),
            role,
            active: true,
            available: true,
            is_primary: false,
            position: (0, 0),
            resolution,
            refresh_hz: 60.0,
            rotation: 1,
        }
    }

    #[test]
    fn selective_off_standard_wake_skips_hpd_settle_stall() {
        assert_eq!(
            effective_settle_timeout_ms(
                4000,
                RecoveryLevel::Standard,
                WakeContext {
                    was_selective_off: true,
                    console_display_off: false,
                    ..WakeContext::default()
                },
            ),
            0
        );
        assert_eq!(
            effective_settle_timeout_ms(
                4000,
                RecoveryLevel::Standard,
                WakeContext {
                    was_selective_off: false,
                    console_display_off: false,
                    ..WakeContext::default()
                },
            ),
            4000
        );
        assert_eq!(
            effective_settle_timeout_ms(
                4000,
                RecoveryLevel::Standard,
                WakeContext {
                    was_selective_off: true,
                    console_display_off: true,
                    ..WakeContext::default()
                },
            ),
            4000
        );
        assert_eq!(
            effective_settle_timeout_ms(
                4000,
                RecoveryLevel::HardBounce,
                WakeContext {
                    was_selective_off: true,
                    console_display_off: false,
                    ..WakeContext::default()
                },
            ),
            4000
        );
    }

    #[test]
    fn healthy_usb_display_skips_redundant_standard_retrain() {
        let usb = sample_disp(DisplayRole::UsbIndirect, (1920, 1080));
        assert!(!should_retrain_display(
            RecoveryLevel::Standard,
            false,
            true,
            &usb,
            false,
            true,
            false,
        ));
        assert!(should_retrain_display(
            RecoveryLevel::Standard,
            false,
            true,
            &usb,
            false,
            false,
            false,
        ));
        let usb_zero = sample_disp(DisplayRole::UsbIndirect, (0, 0));
        assert!(should_retrain_display(
            RecoveryLevel::Standard,
            false,
            true,
            &usb_zero,
            false,
            true,
            false,
        ));
        assert!(should_retrain_display(
            RecoveryLevel::Standard,
            false,
            true,
            &usb,
            true,
            true,
            false,
        ));
        assert!(should_retrain_display(
            RecoveryLevel::HardBounce,
            false,
            true,
            &usb,
            false,
            true,
            false,
        ));
    }

    #[test]
    fn detects_transient_unlock_ccd_errors() {
        assert!(is_transient_unlock_ccd_error(
            "SetDisplayConfig(apply_snapshot) failed with error 5"
        ));
        assert!(is_transient_unlock_ccd_error(
            "SetDisplayConfig(apply_snapshot) failed with error 31"
        ));
        assert!(!is_transient_unlock_ccd_error(
            "SetDisplayConfig(apply_snapshot) failed with error 87"
        ));
        assert!(!is_transient_unlock_ccd_error(
            "No feasible, unambiguous saved topology"
        ));
    }
}
