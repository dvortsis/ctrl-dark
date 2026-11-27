//! Central daemon state machine coordinating hotkeys, power broadcasts, IPC, and async worker jobs.

use std::collections::HashSet;
use std::ptr::null_mut;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows_sys::Win32::System::Power::{
    RegisterPowerSettingNotification, UnregisterPowerSettingNotification, HPOWERNOTIFY,
    POWERBROADCAST_SETTING,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    KillTimer, PostMessageW, PostQuitMessage, SetTimer, DEVICE_NOTIFY_WINDOW_HANDLE,
    PBT_APMRESUMEAUTOMATIC, PBT_POWERSETTINGCHANGE, SW_SHOWNORMAL, WM_APP,
};

use crate::autostart;
use crate::config::{self, AppConfig, HotkeyRuleAction, MonitorHotkeyRule, WakeMode};
use crate::display::ccd::{
    deactivate_targets, force_mode_recommit, has_live_gdi_mode, query_displays, DeactivateResult,
};
use crate::display::recovery::{
    run_recovery, send_monitors_off_power, RecoveryLevel, RecoveryReport, WakeContext,
};
use crate::display::snapshot::{
    apply_snapshot, capture_active_snapshot, load_snapshot, save_snapshot_atomic, TopologySnapshot,
};
use crate::display::{
    is_scope_satisfied_in_selective_off, resolve_scope_keys, DisplayInfo, TargetScope,
};
use crate::hotkey::{self, ParsedHotkey};
use crate::input_guard::InputGuardHandle;
use crate::ipc::{DaemonStatus, IpcRequest, IpcResponse};
use crate::logging;
use crate::tray::{TrayManager, TrayMenuData, TrayVisualState};
use crate::win_util::to_wide_null;
use crate::window_restore::{capture_windows, SavedWindow};

pub const WM_APP_WORKER_DONE: u32 = WM_APP + 20;
pub const WM_APP_IPC_ACTION: u32 = WM_APP + 21;
pub const WM_APP_EXIT: u32 = WM_APP + 22;

pub const TIMER_HPD_SETTLE: usize = 1;
const DBT_DEVNODES_CHANGED: usize = 0x0007;
const HPD_ENFORCE_WINDOW: Duration = Duration::from_secs(3);
const HPD_REENFORCE_COOLDOWN: Duration = Duration::from_millis(400);
const HPD_MAX_REENFORCE_ATTEMPTS: u32 = 2;

const HOTKEY_TOGGLE_ALL: i32 = 1;
const HOTKEY_TOGGLE_EXTERNALS: i32 = 2;
const HOTKEY_FORCE_RECOVER: i32 = 3;
const HOTKEY_MONITOR_BASE: i32 = 100;

/// `GUID_CONSOLE_DISPLAY_STATE` (`{6FE69556-704A-47A0-8F24-C28D936FDA47}`).
const GUID_CONSOLE_DISPLAY_STATE: windows_sys::core::GUID = windows_sys::core::GUID {
    data1: 0x6fe69556,
    data2: 0x704a,
    data3: 0x47a0,
    data4: [0x8f, 0x24, 0xc2, 0x8d, 0x93, 0x6f, 0xda, 0x47],
};

fn guids_eq(a: &windows_sys::core::GUID, b: &windows_sys::core::GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonPhase {
    ActiveOn,
    LockedOff,
    SleepingNormal,
    SelectiveOff,
    Recovering,
    RecoveryFailed,
}

impl DaemonPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            DaemonPhase::ActiveOn => "active_on",
            DaemonPhase::LockedOff => "locked_off",
            DaemonPhase::SleepingNormal => "sleeping_normal",
            DaemonPhase::SelectiveOff => "selective_off",
            DaemonPhase::Recovering => "recovering",
            DaemonPhase::RecoveryFailed => "recovery_failed",
        }
    }
}

#[derive(Clone)]
enum WorkerJob {
    SleepAll {
        trigger_vk: Option<u16>,
        wake_mode: WakeMode,
    },
    DeactivateSelective {
        target_keys: Vec<String>,
        target_names: String,
    },
    ApplyFilteredSnapshot {
        filtered_snapshot: TopologySnapshot,
        config: AppConfig,
    },
    Recover {
        level: RecoveryLevel,
        config: AppConfig,
        snapshot: Option<TopologySnapshot>,
        saved_windows: Vec<SavedWindow>,
        wake_ctx: WakeContext,
    },
}

#[derive(Debug)]
pub enum WorkerResult {
    SleepAllArmed { wake_mode: WakeMode },
    SelectiveApplied { live_displays: Vec<DisplayInfo> },
    SelectiveNeedsFullSleep,
    Failed(String),
    RecoveryFinished(Result<RecoveryReport, String>),
}

#[derive(Debug)]
pub enum DeferredUiAction {
    Toggle(TargetScope, Option<u16>),
    Off(TargetScope, Option<WakeMode>),
    On,
    Recover(bool),
    SetWakeMode(WakeMode),
    ReloadConfig,
    ApplyConfig {
        config: Box<AppConfig>,
        expected_config: Option<Box<AppConfig>>,
        autostart: Option<bool>,
        reply: mpsc::SyncSender<Result<DaemonStatus, String>>,
    },
    Shutdown,
}

pub struct Controller {
    pub hwnd: usize,
    pub ui_url: Option<String>,
    pub config: AppConfig,
    pub phase: DaemonPhase,
    pub snapshot: Option<TopologySnapshot>,
    pub saved_windows: Vec<SavedWindow>,
    pub disabled_keys: Vec<String>,
    pub last_recovery_summary: Option<String>,
    pub hotkey_warnings: Vec<String>,
    registered_hotkeys: Vec<(i32, ParsedHotkey, TargetScope, HotkeyRuleAction, String)>,
    power_notify: HPOWERNOTIFY,
    input_guard: Option<InputGuardHandle>,
    last_console_display_state: u32,
    console_off_since: Option<Instant>,
    last_resleep_pulse: Option<Instant>,
    selective_job_in_flight: bool,
    last_hpd_change_at: Option<Instant>,
    last_hpd_diff: String,
    last_reenforce_at: Option<Instant>,
    reenforce_attempts_for_hpd: u32,
    pub tray: Option<TrayManager>,
    pub tray_displays: Vec<DisplayInfo>,
    worker_tx: Sender<WorkerJob>,
    worker_res_rx: Receiver<WorkerResult>,
    pending_ipc_actions: Vec<DeferredUiAction>,
}

impl Controller {
    pub fn new(hwnd_raw: usize, config: AppConfig) -> Self {
        let hwnd = hwnd_raw as HWND;
        let (job_tx, job_rx) = mpsc::channel::<WorkerJob>();
        let (res_tx, res_rx) = mpsc::channel::<WorkerResult>();
        let cfg_clone = config.clone();
        let _ = cfg_clone;

        thread::Builder::new()
            .name("display-worker".to_string())
            .spawn(move || worker_loop(hwnd_raw, job_rx, res_tx))
            .expect("spawn display-worker thread");

        let power_notify = unsafe {
            RegisterPowerSettingNotification(
                hwnd,
                &GUID_CONSOLE_DISPLAY_STATE,
                DEVICE_NOTIFY_WINDOW_HANDLE,
            )
        };

        let mut snapshot = load_snapshot(&config::snapshot_path()).unwrap_or(None);
        let displays = query_displays(true).unwrap_or_default();
        if let Ok(live_snap) = capture_active_snapshot() {
            let should_replace = match &snapshot {
                None => true,
                Some(prev) => live_snap.monitors.len() >= prev.monitors.len(),
            };
            if should_replace {
                let _ = save_snapshot_atomic(&live_snap, &config::snapshot_path());
                snapshot = Some(live_snap);
            }
        }

        let initial_disabled: Vec<String> = displays
            .iter()
            .filter(|d| d.available && !d.active)
            .map(|d| d.stable_key.clone())
            .collect();
        let initial_phase = if initial_disabled.is_empty() {
            DaemonPhase::ActiveOn
        } else {
            DaemonPhase::SelectiveOff
        };

        let tray = TrayManager::new(hwnd_raw, "Ctrl + Dark — Active");

        let mut ctrl = Self {
            hwnd: hwnd_raw,
            ui_url: None,
            config,
            phase: initial_phase,
            snapshot,
            saved_windows: Vec::new(),
            disabled_keys: initial_disabled,
            last_recovery_summary: None,
            hotkey_warnings: Vec::new(),
            registered_hotkeys: Vec::new(),
            power_notify,
            input_guard: None,
            last_console_display_state: 1,
            console_off_since: None,
            last_resleep_pulse: None,
            selective_job_in_flight: false,
            last_hpd_change_at: None,
            last_hpd_diff: String::new(),
            last_reenforce_at: None,
            reenforce_attempts_for_hpd: 0,
            tray: Some(tray),
            tray_displays: displays,
            worker_tx: job_tx,
            worker_res_rx: res_rx,
            pending_ipc_actions: Vec::new(),
        };

        ctrl.register_all_hotkeys();
        ctrl.update_tray_visual();
        ctrl
    }

    pub fn register_all_hotkeys(&mut self) {
        self.unregister_all_hotkeys();
        self.hotkey_warnings.clear();

        let mut specs = vec![
            (
                HOTKEY_TOGGLE_ALL,
                self.config.hotkeys.toggle_all.clone(),
                TargetScope::All,
                HotkeyRuleAction::Toggle,
                "Toggle All Monitors".to_string(),
            ),
            (
                HOTKEY_TOGGLE_EXTERNALS,
                self.config.hotkeys.toggle_externals.clone(),
                TargetScope::ExternalsOnly,
                HotkeyRuleAction::Toggle,
                "Toggle External Monitors".to_string(),
            ),
            (
                HOTKEY_FORCE_RECOVER,
                self.config.hotkeys.force_recover.clone(),
                TargetScope::All,
                HotkeyRuleAction::Toggle,
                "Force Recover Displays".to_string(),
            ),
        ];

        for (i, rule) in self.config.hotkeys.monitor_hotkeys.iter().enumerate() {
            match rule.to_scope() {
                Ok(scope) => {
                    if !rule.hotkey.trim().is_empty() {
                        specs.push((
                            HOTKEY_MONITOR_BASE + i as i32,
                            rule.hotkey.clone(),
                            scope,
                            rule.action,
                            rule.display_label(),
                        ));
                    }
                }
                Err(e) => {
                    logging::warn(&e);
                    self.hotkey_warnings.push(e);
                }
            }
        }

        for (id, raw_spec, scope, action, label) in specs {
            if raw_spec.trim().is_empty() {
                continue;
            }
            match ParsedHotkey::parse(&raw_spec) {
                Ok(parsed) => match hotkey::register_with_hwnd(self.hwnd, id, &parsed) {
                    Ok(()) => {
                        logging::info(format!(
                            "Registered hotkey {} ({label}, id={id})",
                            parsed.canonical
                        ));
                        self.registered_hotkeys
                            .push((id, parsed, scope, action, label));
                    }
                    Err(e) => {
                        logging::warn(&e);
                        self.hotkey_warnings.push(e);
                    }
                },
                Err(e) => {
                    logging::warn(&e);
                    self.hotkey_warnings.push(e);
                }
            }
        }
        if self.phase == DaemonPhase::LockedOff {
            if let Some(guard) = &self.input_guard {
                let hk_pairs: Vec<(i32, ParsedHotkey)> = self
                    .registered_hotkeys
                    .iter()
                    .map(|(id, hk, _, _, _)| (*id, hk.clone()))
                    .collect();
                guard.update_hotkeys(hk_pairs);
            } else {
                self.arm_input_guard();
            }
        }
    }

    pub fn unregister_all_hotkeys(&mut self) {
        for (id, _, _, _, _) in self.registered_hotkeys.drain(..) {
            hotkey::unregister_with_hwnd(self.hwnd, id);
        }
    }

    pub fn handle_hotkey(&mut self, id: i32) {
        if id == HOTKEY_FORCE_RECOVER {
            self.trigger_recover(true);
            return;
        }
        if let Some((_, hk, scope, action, _)) = self
            .registered_hotkeys
            .iter()
            .find(|(reg_id, _, _, _, _)| *reg_id == id)
            .cloned()
        {
            match action {
                HotkeyRuleAction::Toggle => self.trigger_toggle(scope, Some(hk.vk)),
                HotkeyRuleAction::Off => self.trigger_off(scope, None, Some(hk.vk)),
            }
        }
    }

    pub fn trigger_toggle(&mut self, scope: TargetScope, trigger_vk: Option<u16>) {
        match (&self.phase, &scope) {
            (DaemonPhase::RecoveryFailed, _) => {
                self.trigger_on();
                return;
            }
            (DaemonPhase::LockedOff | DaemonPhase::SleepingNormal, TargetScope::All) => {
                self.trigger_on();
                return;
            }
            (DaemonPhase::LockedOff | DaemonPhase::SleepingNormal, _) => {
                // Any toggle while all screens are sleeping wakes everything safely first.
                self.trigger_on();
                return;
            }
            (DaemonPhase::SelectiveOff, _) => {
                let live = query_displays(true).unwrap_or_default();
                if is_scope_satisfied_in_selective_off(&live, &scope) {
                    self.trigger_on();
                    return;
                }
                let target_keys = resolve_scope_keys(&live, &scope);
                let any_target_active = live.iter().any(|d| {
                    d.active
                        && target_keys
                            .iter()
                            .any(|k| k.eq_ignore_ascii_case(&d.stable_key))
                });
                if !any_target_active {
                    self.trigger_on();
                    return;
                }
            }
            _ => {}
        }

        self.trigger_off(scope, None, trigger_vk);
    }

    pub fn trigger_off(
        &mut self,
        scope: TargetScope,
        wake_override: Option<WakeMode>,
        trigger_vk: Option<u16>,
    ) {
        if matches!(
            self.phase,
            DaemonPhase::Recovering | DaemonPhase::RecoveryFailed
        ) {
            return;
        }
        if matches!(
            self.phase,
            DaemonPhase::LockedOff | DaemonPhase::SleepingNormal
        ) {
            // Repeated OFF is idempotent; changing guards while asleep can leave
            // normal-wake mode swallowing all keyboard and mouse input.
            return;
        }

        let all_connected: Option<Vec<DisplayInfo>> = if matches!(scope, TargetScope::All) {
            None
        } else {
            Some(
                query_displays(true)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|d| d.available)
                    .collect(),
            )
        };

        // Persist the rollback baseline before changing displays when all available
        // monitors are currently active, so an older background save cannot overwrite a newer snapshot.
        if self.phase == DaemonPhase::ActiveOn {
            let live_has_inactive = match &all_connected {
                Some(list) => list.iter().any(|d| !d.active),
                None => query_displays(true)
                    .map(|list| list.iter().any(|d| d.available && !d.active))
                    .unwrap_or(false),
            };
            if !live_has_inactive {
                let _ = self.capture_healthy_state_inner();
            }
        }

        match scope {
            TargetScope::All => {
                let wake_mode = wake_override.unwrap_or(self.config.wake_mode);
                if wake_mode == WakeMode::HotkeyOnly {
                    self.arm_input_guard();
                } else {
                    self.disarm_input_guard();
                }
                self.selective_job_in_flight = false;
                self.last_hpd_change_at = None;
                self.last_hpd_diff.clear();
                self.last_reenforce_at = None;
                self.reenforce_attempts_for_hpd = 0;
                self.phase = if wake_mode == WakeMode::HotkeyOnly {
                    DaemonPhase::LockedOff
                } else {
                    DaemonPhase::SleepingNormal
                };
                self.update_tray_visual();
                let _ = self.worker_tx.send(WorkerJob::SleepAll {
                    trigger_vk,
                    wake_mode,
                });
            }
            TargetScope::ExternalsOnly
            | TargetScope::Monitor(_)
            | TargetScope::Monitors(_)
            | TargetScope::Except(_)
            | TargetScope::Profile { .. } => {
                let all_connected = all_connected.unwrap_or_default();

                let mut plan = plan_selective_off(
                    &self.phase,
                    &all_connected,
                    &scope,
                    self.snapshot.is_some(),
                );

                if let SelectiveOffPlan::ApplyFilteredKeep(ref keep_keys) = plan {
                    let keep_displays: Vec<&DisplayInfo> = all_connected
                        .iter()
                        .filter(|d| {
                            keep_keys
                                .iter()
                                .any(|k| k.eq_ignore_ascii_case(&d.stable_key))
                        })
                        .collect();
                    if let Some(snap) = &self.snapshot {
                        let filtered = filter_snapshot_for_displays(snap, &keep_displays);
                        if !filtered.monitors.is_empty() {
                            let disabled_keys: Vec<String> = all_connected
                                .iter()
                                .filter(|d| {
                                    keep_displays
                                        .iter()
                                        .all(|k| !k.stable_key.eq_ignore_ascii_case(&d.stable_key))
                                })
                                .map(|d| d.stable_key.clone())
                                .collect();
                            self.phase = DaemonPhase::SelectiveOff;
                            self.disabled_keys = disabled_keys;
                            self.selective_job_in_flight = true;
                            self.update_tray_visual();
                            let _ = self.worker_tx.send(WorkerJob::ApplyFilteredSnapshot {
                                filtered_snapshot: filtered,
                                config: self.config.clone(),
                            });
                            return;
                        }
                    }
                    // Fall back to additive deactivation if the saved snapshot cannot satisfy the keep layout.
                    plan =
                        plan_selective_off(&DaemonPhase::ActiveOn, &all_connected, &scope, false);
                }

                match plan {
                    SelectiveOffPlan::SleepAll => {
                        self.trigger_off(TargetScope::All, wake_override, trigger_vk);
                    }
                    SelectiveOffPlan::DeactivateActive(keys) => {
                        self.phase = DaemonPhase::SelectiveOff;
                        for k in &keys {
                            if !self.disabled_keys.iter().any(|d| d.eq_ignore_ascii_case(k)) {
                                self.disabled_keys.push(k.clone());
                            }
                        }
                        let target_names = describe_keys(&all_connected, &keys);
                        self.selective_job_in_flight = true;
                        self.update_tray_visual();
                        let _ = self.worker_tx.send(WorkerJob::DeactivateSelective {
                            target_keys: keys,
                            target_names,
                        });
                    }
                    SelectiveOffPlan::NoOp | SelectiveOffPlan::ApplyFilteredKeep(_) => {
                        logging::warn(format!("No active display matched scope {scope:?}"));
                    }
                }
            }
        }
    }

    pub fn trigger_on(&mut self) {
        self.disarm_input_guard();
        self.trigger_recover(false);
    }

    pub fn trigger_recover(&mut self, hard: bool) {
        self.disarm_input_guard();
        if self.phase == DaemonPhase::Recovering {
            return;
        }
        let was_selective_off = self.phase == DaemonPhase::SelectiveOff;
        let console_display_off = self.last_console_display_state == 0;
        self.selective_job_in_flight = false;
        self.last_hpd_change_at = None;
        self.last_hpd_diff.clear();
        self.last_reenforce_at = None;
        self.reenforce_attempts_for_hpd = 0;
        self.phase = DaemonPhase::Recovering;
        self.update_tray_visual();

        let level = if hard {
            RecoveryLevel::HardBounce
        } else {
            RecoveryLevel::Standard
        };
        let _ = self.worker_tx.send(WorkerJob::Recover {
            level,
            config: self.config.clone(),
            snapshot: self.snapshot.clone(),
            saved_windows: self.saved_windows.clone(),
            wake_ctx: WakeContext {
                was_selective_off,
                console_display_off,
            },
        });
    }

    pub fn on_worker_done(&mut self) {
        while let Ok(res) = self.worker_res_rx.try_recv() {
            match res {
                WorkerResult::Failed(error) => {
                    logging::error(&error);
                    self.selective_job_in_flight = false;
                    self.last_recovery_summary = Some(format!("Error: {error}"));
                    if self.phase != DaemonPhase::Recovering {
                        let live = query_displays(true).unwrap_or_default();
                        self.disabled_keys = live
                            .iter()
                            .filter(|d| d.available && !d.active)
                            .map(|d| d.stable_key.clone())
                            .collect();
                        self.phase = if self.disabled_keys.is_empty() {
                            DaemonPhase::ActiveOn
                        } else {
                            DaemonPhase::SelectiveOff
                        };
                        self.update_tray_visual();
                    }
                }
                WorkerResult::SleepAllArmed { wake_mode } => {
                    logging::info(format!("Monitors entered sleep ({wake_mode:?})"));
                }
                WorkerResult::SelectiveApplied { live_displays } => {
                    self.selective_job_in_flight = false;
                    let in_hpd_window = self
                        .last_hpd_change_at
                        .is_some_and(|t| t.elapsed() <= HPD_ENFORCE_WINDOW);
                    let midflight_reactivated = in_hpd_window
                        && self.reenforce_attempts_for_hpd < HPD_MAX_REENFORCE_ATTEMPTS
                        && self.disabled_keys.iter().any(|prev_key| {
                            live_displays.iter().any(|d| {
                                d.available
                                    && d.active
                                    && d.stable_key.eq_ignore_ascii_case(prev_key)
                            })
                        })
                        && live_displays.iter().any(|d| {
                            d.available
                                && d.active
                                && !self
                                    .disabled_keys
                                    .iter()
                                    .any(|k| k.eq_ignore_ascii_case(&d.stable_key))
                        });

                    if midflight_reactivated {
                        unsafe {
                            SetTimer(self.hwnd as HWND, TIMER_HPD_SETTLE, 300, None);
                        }
                    } else {
                        self.disabled_keys =
                            reconcile_disabled_keys(&self.disabled_keys, &live_displays);
                        if self.disabled_keys.is_empty() {
                            self.phase = DaemonPhase::ActiveOn;
                            self.last_hpd_diff.clear();
                        }
                    }
                    self.tray_displays = live_displays;
                    self.update_tray_visual();
                }
                WorkerResult::SelectiveNeedsFullSleep => {
                    self.selective_job_in_flight = false;
                    self.trigger_off(TargetScope::All, None, None);
                }
                WorkerResult::RecoveryFinished(outcome) => {
                    self.selective_job_in_flight = false;
                    let failed = outcome.is_err();
                    match outcome {
                        Ok(report) => {
                            self.last_recovery_summary = Some(report.summary);
                        }
                        Err(e) => {
                            logging::error(format!("Recovery failed: {e}"));
                            self.last_recovery_summary = Some(format!("Error: {e}"));
                        }
                    }
                    let live = query_displays(true).unwrap_or_default();
                    self.disabled_keys = live
                        .iter()
                        .filter(|d| d.available && !d.active)
                        .map(|d| d.stable_key.clone())
                        .collect();
                    self.phase = if failed {
                        DaemonPhase::RecoveryFailed
                    } else if self.disabled_keys.is_empty() {
                        DaemonPhase::ActiveOn
                    } else {
                        DaemonPhase::SelectiveOff
                    };
                    self.tray_displays = query_displays(true).unwrap_or_default();
                    self.update_tray_visual();
                }
            }
        }
    }

    pub fn on_power_broadcast(&mut self, wparam: WPARAM, lparam: LPARAM) {
        let evt = wparam as u32;
        if evt == PBT_APMRESUMEAUTOMATIC {
            if self.config.recovery.recover_on_system_wake && self.phase != DaemonPhase::LockedOff {
                logging::info("System resume detected; verifying display recovery");
                self.trigger_recover(false);
            }
            return;
        }

        if evt == PBT_POWERSETTINGCHANGE && lparam != 0 {
            let setting = unsafe { &*(lparam as *const POWERBROADCAST_SETTING) };
            if guids_eq(&setting.PowerSetting, &GUID_CONSOLE_DISPLAY_STATE)
                && setting.DataLength >= 4
            {
                let state = unsafe { *(setting.Data.as_ptr() as *const u32) };
                let prev = self.last_console_display_state;
                self.last_console_display_state = state;

                if state == 0 {
                    self.console_off_since = Some(Instant::now());
                } else if state == 1 && prev == 0 {
                    let off_duration = self
                        .console_off_since
                        .take()
                        .map(|t| t.elapsed())
                        .unwrap_or_default();

                    if self.phase == DaemonPhase::LockedOff {
                        // Mouse bump or raw HID woke console display while locked off -> instant re-sleep.
                        let allow_pulse = self
                            .last_resleep_pulse
                            .map(|t| t.elapsed() >= Duration::from_millis(150))
                            .unwrap_or(true);
                        if allow_pulse {
                            self.last_resleep_pulse = Some(Instant::now());
                            logging::info(
                                "Suppressed unwanted wake in LockedOff mode; re-sleeping",
                            );
                            send_monitors_off_power(Some(self.hwnd));
                        }
                    } else if self.phase == DaemonPhase::SleepingNormal
                        || (self.phase == DaemonPhase::ActiveOn
                            && self.config.recovery.recover_on_system_wake
                            && off_duration >= Duration::from_secs(2))
                    {
                        logging::info("Display wake detected; running anti-black-screen recovery");
                        self.trigger_recover(false);
                    }
                }
            }
        }
    }

    pub fn on_display_change(&mut self) {
        let live = query_displays(true).unwrap_or_default();
        let prev = std::mem::replace(&mut self.tray_displays, live.clone());

        if self.phase == DaemonPhase::ActiveOn {
            if let Ok(live_snap) = capture_active_snapshot() {
                let should_update = match &self.snapshot {
                    None => true,
                    Some(prev_snap) => live_snap.monitors.len() >= prev_snap.monitors.len(),
                };
                if should_update {
                    let _ = save_snapshot_atomic(&live_snap, &config::snapshot_path());
                    self.snapshot = Some(live_snap);
                }
            }
        } else if self.phase == DaemonPhase::SelectiveOff && !self.disabled_keys.is_empty() {
            let other_changed = other_topology_changed(&prev, &live, &self.disabled_keys);
            if other_changed {
                self.last_hpd_change_at = Some(Instant::now());
                self.reenforce_attempts_for_hpd = 0;
                let diff = describe_other_topology_diff(&prev, &live, &self.disabled_keys);
                if !diff.is_empty() {
                    self.last_hpd_diff = diff;
                }
                unsafe {
                    SetTimer(self.hwnd as HWND, TIMER_HPD_SETTLE, 500, None);
                }
            }

            let within_hpd_window = other_changed
                || self
                    .last_hpd_change_at
                    .is_some_and(|t| t.elapsed() <= HPD_ENFORCE_WINDOW);
            let cooldown_elapsed = self
                .last_reenforce_at
                .map(|t| t.elapsed() >= HPD_REENFORCE_COOLDOWN)
                .unwrap_or(true);

            match plan_selective_display_change(
                &live,
                &self.disabled_keys,
                within_hpd_window,
                self.selective_job_in_flight,
                self.reenforce_attempts_for_hpd,
                cooldown_elapsed,
            ) {
                SelectiveChangeAction::ReDeactivate(keys) => {
                    let names = describe_keys(&live, &keys);
                    let live_diff = describe_other_topology_diff(&prev, &live, &self.disabled_keys);
                    let diff = if !live_diff.is_empty() {
                        live_diff.as_str()
                    } else {
                        self.last_hpd_diff.as_str()
                    };
                    if diff.is_empty() {
                        logging::info(format!(
                            "Re-disabling {} selectively-off display(s) after monitor hot-plug change [{names}]",
                            keys.len()
                        ));
                    } else {
                        logging::info(format!(
                            "Re-disabling {} selectively-off display(s) after monitor hot-plug ({diff}) [{names}]",
                            keys.len()
                        ));
                    }
                    self.selective_job_in_flight = true;
                    self.last_reenforce_at = Some(Instant::now());
                    self.reenforce_attempts_for_hpd += 1;
                    let _ = self.worker_tx.send(WorkerJob::DeactivateSelective {
                        target_keys: keys,
                        target_names: names,
                    });
                }
                SelectiveChangeAction::SyncDisabledKeys(next_disabled) => {
                    logging::info(format!(
                        "Synced selective-off state after display change (remaining_disabled={})",
                        next_disabled.len()
                    ));
                    self.disabled_keys = next_disabled;
                    if self.disabled_keys.is_empty() {
                        self.phase = DaemonPhase::ActiveOn;
                        self.last_hpd_diff.clear();
                    }
                    self.update_tray_visual();
                }
                SelectiveChangeAction::None => {}
            }
        }
    }

    pub fn on_device_change(&mut self, wparam: WPARAM) {
        if wparam == DBT_DEVNODES_CHANGED && self.phase == DaemonPhase::SelectiveOff {
            self.on_display_change();
        }
    }

    pub fn on_timer(&mut self, timer_id: usize) {
        if timer_id == TIMER_HPD_SETTLE {
            unsafe {
                KillTimer(self.hwnd as HWND, TIMER_HPD_SETTLE);
            }
            if self.phase == DaemonPhase::SelectiveOff {
                self.on_display_change();
            }
        }
    }

    pub fn capture_healthy_state(&mut self) -> Result<usize, String> {
        if self.phase != DaemonPhase::ActiveOn {
            return Err(
                "Wake all displays and wait for recovery before replacing the recovery snapshot"
                    .into(),
            );
        }
        self.capture_healthy_state_inner()
    }

    fn capture_healthy_state_inner(&mut self) -> Result<usize, String> {
        let live_displays = query_displays(false)?;
        self.saved_windows = capture_windows(&live_displays);
        let snap = capture_active_snapshot()?;
        let count = snap.monitors.len();
        save_snapshot_atomic(&snap, &config::snapshot_path())?;
        self.snapshot = Some(snap);
        self.tray_displays = live_displays;
        Ok(count)
    }

    pub fn handle_menu_command(&mut self, cmd_id: usize) {
        match cmd_id {
            crate::tray::MENU_OPEN_UI => {
                if let Err(e) = autostart::launch_ui_window(self.ui_url.as_deref(), false) {
                    logging::warn(format!("Failed launching Control Panel: {e}"));
                }
            }
            crate::tray::MENU_TOGGLE_ALL => self.trigger_toggle(TargetScope::All, None),
            crate::tray::MENU_TOGGLE_EXTERNALS => {
                self.trigger_toggle(TargetScope::ExternalsOnly, None);
            }
            crate::tray::MENU_FORCE_RECOVER => self.trigger_recover(true),
            crate::tray::MENU_TOGGLE_WAKE_MODE => {
                self.config.wake_mode = match self.config.wake_mode {
                    WakeMode::HotkeyOnly => WakeMode::AnyInput,
                    WakeMode::AnyInput => WakeMode::HotkeyOnly,
                };
                if let Err(e) = self.config.save() {
                    logging::warn(format!("Failed saving wake mode: {e}"));
                }
                self.update_tray_visual();
            }
            crate::tray::MENU_TOGGLE_AUTOSTART => {
                let enabled = autostart::is_enabled().ok().flatten().is_some();
                let res = if enabled {
                    autostart::disable().map(|_| ())
                } else {
                    autostart::enable().map(|_| ())
                };
                if let Err(e) = res {
                    logging::warn(format!("Failed toggling autostart from tray: {e}"));
                }
            }
            crate::tray::MENU_TOGGLE_START_MINIMIZED => {
                self.config.start_minimized_to_tray = !self.config.start_minimized_to_tray;
                if let Err(e) = self.config.save() {
                    logging::warn(format!("Failed saving start_minimized_to_tray: {e}"));
                }
                self.update_tray_visual();
            }
            crate::tray::MENU_SNAPSHOT_NOW => {
                if let Err(e) = self.capture_healthy_state() {
                    logging::warn(format!("Tray snapshot ignored: {e}"));
                }
            }
            crate::tray::MENU_OPEN_CONFIG => {
                let path = config::config_path();
                if !path.exists() {
                    let _ = config::write_default_commented_toml(&path);
                }
                let op = to_wide_null("open");
                let file_w = to_wide_null(&path.to_string_lossy());
                unsafe {
                    ShellExecuteW(
                        self.hwnd as HWND,
                        op.as_ptr(),
                        file_w.as_ptr(),
                        null_mut(),
                        null_mut(),
                        SW_SHOWNORMAL,
                    );
                }
            }
            crate::tray::MENU_RELOAD_CONFIG => {
                self.reload_config();
            }
            crate::tray::MENU_EXIT => {
                self.shutdown();
            }
            id if id >= crate::tray::MENU_RULE_BASE => {
                let idx = id - crate::tray::MENU_RULE_BASE;
                if let Some(rule) = self.config.hotkeys.monitor_hotkeys.get(idx).cloned() {
                    if let Ok(scope) = rule.to_scope() {
                        match rule.action {
                            HotkeyRuleAction::Toggle => self.trigger_toggle(scope, None),
                            HotkeyRuleAction::Off => self.trigger_off(scope, None, None),
                        }
                    }
                }
            }
            id if id >= crate::tray::MENU_MONITOR_BASE => {
                let idx = id - crate::tray::MENU_MONITOR_BASE;
                if let Some(disp) = self.tray_displays.get(idx).cloned() {
                    if disp.available {
                        self.trigger_toggle(TargetScope::Monitor(disp.stable_key), None);
                    }
                }
            }
            _ => {}
        }
    }

    pub fn reload_config(&mut self) {
        match AppConfig::load_or_create_default() {
            Ok(new_cfg) => {
                self.config = new_cfg;
                self.register_all_hotkeys();
                self.update_tray_visual();
            }
            Err(e) => logging::error(format!("Reload config failed: {e}")),
        }
    }

    pub fn enqueue_ui_action(&mut self, action: DeferredUiAction) {
        self.pending_ipc_actions.push(action);
        unsafe {
            PostMessageW(self.hwnd as HWND, WM_APP_IPC_ACTION, 0, 0);
        }
    }

    pub fn drain_ui_actions(&mut self) {
        let actions = std::mem::take(&mut self.pending_ipc_actions);
        for act in actions {
            match act {
                DeferredUiAction::Toggle(scope, vk) => self.trigger_toggle(scope, vk),
                DeferredUiAction::Off(scope, mode) => self.trigger_off(scope, mode, None),
                DeferredUiAction::On => self.trigger_on(),
                DeferredUiAction::Recover(hard) => self.trigger_recover(hard),
                DeferredUiAction::SetWakeMode(mode) => {
                    self.config.wake_mode = mode;
                    let _ = self.config.save();
                    self.update_tray_visual();
                }
                DeferredUiAction::ReloadConfig => self.reload_config(),
                DeferredUiAction::ApplyConfig {
                    config,
                    expected_config,
                    autostart,
                    reply,
                } => {
                    let mut current = self.config.clone();
                    current.hotkeys.monitor_hotkeys = current
                        .hotkeys
                        .monitor_hotkeys
                        .iter()
                        .map(MonitorHotkeyRule::normalized_for_ui)
                        .collect();
                    if expected_config.is_some_and(|expected| *expected != current) {
                        let _ = reply.send(Err("Settings changed since editing began. Refresh and reopen the editor before saving.".into()));
                        continue;
                    }
                    let res = self.apply_config_now(*config, autostart);
                    let _ = reply.send(res);
                }
                DeferredUiAction::Shutdown => self.shutdown(),
            }
        }
    }

    pub fn apply_config_now(
        &mut self,
        new_cfg: AppConfig,
        want_autostart: Option<bool>,
    ) -> Result<DaemonStatus, String> {
        new_cfg.save()?;
        // Keep runtime and disk consistent even if the independent autostart update fails.
        self.config = new_cfg;
        self.register_all_hotkeys();
        self.update_tray_visual();
        if let Some(enable) = want_autostart {
            let currently_on = autostart::is_enabled().ok().flatten().is_some();
            if enable && !currently_on {
                autostart::enable()
                    .map_err(|e| format!("Settings saved, but enabling autostart failed: {e}"))?;
            } else if !enable && currently_on {
                autostart::disable()
                    .map_err(|e| format!("Settings saved, but disabling autostart failed: {e}"))?;
            }
        }
        Ok(self.build_status())
    }

    pub fn build_status(&self) -> DaemonStatus {
        let live = query_displays(true).unwrap_or_default();
        let active_count = live.iter().filter(|d| d.active).count();
        let avail_count = live.iter().filter(|d| d.available).count();

        let custom_hotkeys = self
            .config
            .hotkeys
            .monitor_hotkeys
            .iter()
            .map(|r| {
                let hk = if r.hotkey.trim().is_empty() {
                    "tray-only"
                } else {
                    r.hotkey.trim()
                };
                format!("{hk} -> {}", r.display_label())
            })
            .collect();

        DaemonStatus {
            pid: std::process::id(),
            state: self.phase.as_str().to_string(),
            wake_mode: self.config.wake_mode,
            toggle_all_hotkey: self.config.hotkeys.toggle_all.clone(),
            toggle_externals_hotkey: self.config.hotkeys.toggle_externals.clone(),
            force_recover_hotkey: self.config.hotkeys.force_recover.clone(),
            custom_hotkeys,
            snapshot_monitors: self
                .snapshot
                .as_ref()
                .map(|s| s.monitors.len())
                .unwrap_or(0),
            snapshot_captured_unix_ms: self.snapshot.as_ref().map(|s| s.captured_at_unix_ms),
            active_monitors: active_count,
            total_available_monitors: avail_count,
            disabled_monitors: self.disabled_keys.clone(),
            hotkey_warnings: self.hotkey_warnings.clone(),
            last_recovery_summary: self.last_recovery_summary.clone(),
            ui_url: self.ui_url.clone(),
        }
    }

    pub fn prepare_tray_menu_data(&mut self) -> TrayMenuData {
        self.tray_displays = query_displays(true).unwrap_or_default();
        let autostart_on = autostart::is_enabled().ok().flatten().is_some();
        let is_selective = self.phase == DaemonPhase::SelectiveOff;
        let custom_rules = self
            .config
            .hotkeys
            .monitor_hotkeys
            .iter()
            .map(|r| {
                let active = is_selective
                    && r.to_scope()
                        .map(|s| is_scope_satisfied_in_selective_off(&self.tray_displays, &s))
                        .unwrap_or(false);
                (r.display_label(), r.hotkey.clone(), active)
            })
            .collect();
        TrayMenuData {
            hwnd_raw: self.hwnd,
            displays: self.tray_displays.clone(),
            wake_mode: self.config.wake_mode,
            autostart_on,
            start_minimized_to_tray: self.config.start_minimized_to_tray,
            toggle_all_hk: self.config.hotkeys.toggle_all.clone(),
            toggle_ext_hk: self.config.hotkeys.toggle_externals.clone(),
            recover_hk: self.config.hotkeys.force_recover.clone(),
            custom_rules,
        }
    }

    pub fn show_tray_balloon(&mut self, title: &str, body: &str) {
        if let Some(tray) = &mut self.tray {
            tray.show_balloon(title, body);
        }
    }

    pub fn reattach_tray(&mut self) {
        self.tray.take();
        self.tray = Some(TrayManager::new(self.hwnd, "Ctrl + Dark — Active"));
        self.update_tray_visual();
    }

    pub fn refresh_tray_theme(&mut self) {
        if let Some(tray) = &mut self.tray {
            tray.refresh_theme_icon();
        }
    }

    pub fn shutdown(&mut self) {
        self.disarm_input_guard();
        self.unregister_all_hotkeys();
        unsafe {
            KillTimer(self.hwnd as HWND, TIMER_HPD_SETTLE);
        }
        if self.power_notify != 0 {
            unsafe {
                UnregisterPowerSettingNotification(self.power_notify);
            }
            self.power_notify = 0;
        }
        self.tray.take();
        unsafe {
            PostQuitMessage(0);
        }
    }

    fn arm_input_guard(&mut self) {
        self.disarm_input_guard();
        let hk_pairs: Vec<(i32, ParsedHotkey)> = self
            .registered_hotkeys
            .iter()
            .map(|(id, hk, _, _, _)| (*id, hk.clone()))
            .collect();
        match InputGuardHandle::start(
            self.hwnd,
            hk_pairs,
            self.config.recovery.blackout_curtain_when_locked,
        ) {
            Ok(h) => self.input_guard = Some(h),
            Err(e) => logging::warn(format!("Could not arm InputGuard: {e}")),
        }
    }

    fn disarm_input_guard(&mut self) {
        if let Some(guard) = self.input_guard.take() {
            guard.stop();
        }
    }

    fn update_tray_visual(&mut self) {
        let visual = match self.phase {
            DaemonPhase::ActiveOn => TrayVisualState::Active,
            _ => TrayVisualState::OffOrRecovering,
        };
        let tip = match self.phase {
            DaemonPhase::ActiveOn => {
                format!("Ctrl + Dark — Active ({})", self.config.wake_mode.as_str())
            }
            DaemonPhase::LockedOff => "Ctrl + Dark — Locked Off (Hotkey / Esc x3)".to_string(),
            DaemonPhase::SleepingNormal => "Ctrl + Dark — Sleeping".to_string(),
            DaemonPhase::SelectiveOff => "Ctrl + Dark — Selective Off".to_string(),
            DaemonPhase::Recovering => "Ctrl + Dark — Recovering...".to_string(),
            DaemonPhase::RecoveryFailed => {
                "Ctrl + Dark — Recovery failed; try Force Recover".to_string()
            }
        };
        if let Some(tray) = &mut self.tray {
            tray.add_or_update(visual, &tip);
        }
    }
}

/// Processes IPC requests safely, deferring UI/Win32-thread mutations onto the daemon HWND loop.
pub fn handle_ipc_request(ctrl_arc: &Arc<Mutex<Controller>>, req: IpcRequest) -> IpcResponse {
    match req {
        IpcRequest::Status => {
            let ctrl = ctrl_arc.lock().expect("controller lock");
            IpcResponse::Ok {
                message: "Daemon running".to_string(),
                status: Some(Box::new(ctrl.build_status())),
                monitors: None,
            }
        }
        IpcRequest::ListMonitors => match query_displays(true) {
            Ok(monitors) => IpcResponse::Ok {
                message: format!("{} monitor(s) enumerated", monitors.len()),
                status: None,
                monitors: Some(monitors),
            },
            Err(e) => IpcResponse::Error { message: e },
        },
        IpcRequest::SnapshotNow => {
            let mut ctrl = ctrl_arc.lock().expect("controller lock");
            match ctrl.capture_healthy_state() {
                Ok(n) => IpcResponse::Ok {
                    message: format!("Captured topology snapshot for {n} active display(s)"),
                    status: Some(Box::new(ctrl.build_status())),
                    monitors: None,
                },
                Err(e) => IpcResponse::Error { message: e },
            }
        }
        IpcRequest::Toggle { scope } => {
            let mut ctrl = ctrl_arc.lock().expect("controller lock");
            ctrl.enqueue_ui_action(DeferredUiAction::Toggle(scope, None));
            IpcResponse::Ok {
                message: "Toggle queued".to_string(),
                status: None,
                monitors: None,
            }
        }
        IpcRequest::Off { scope, wake_mode } => {
            let mut ctrl = ctrl_arc.lock().expect("controller lock");
            ctrl.enqueue_ui_action(DeferredUiAction::Off(scope, wake_mode));
            IpcResponse::Ok {
                message: "Monitor off queued".to_string(),
                status: None,
                monitors: None,
            }
        }
        IpcRequest::On => {
            let mut ctrl = ctrl_arc.lock().expect("controller lock");
            ctrl.enqueue_ui_action(DeferredUiAction::On);
            IpcResponse::Ok {
                message: "Monitor wake & recovery queued".to_string(),
                status: None,
                monitors: None,
            }
        }
        IpcRequest::Recover { hard } => {
            let mut ctrl = ctrl_arc.lock().expect("controller lock");
            ctrl.enqueue_ui_action(DeferredUiAction::Recover(hard));
            IpcResponse::Ok {
                message: if hard {
                    "Hard bounce recovery queued".to_string()
                } else {
                    "Standard recovery queued".to_string()
                },
                status: None,
                monitors: None,
            }
        }
        IpcRequest::SetWakeMode { mode } => {
            let mut ctrl = ctrl_arc.lock().expect("controller lock");
            ctrl.enqueue_ui_action(DeferredUiAction::SetWakeMode(mode));
            IpcResponse::Ok {
                message: format!("Wake mode set to {}", mode.as_str()),
                status: None,
                monitors: None,
            }
        }
        IpcRequest::ReloadConfig => {
            let mut ctrl = ctrl_arc.lock().expect("controller lock");
            ctrl.enqueue_ui_action(DeferredUiAction::ReloadConfig);
            IpcResponse::Ok {
                message: "Configuration reload queued".to_string(),
                status: None,
                monitors: None,
            }
        }
        IpcRequest::Shutdown => {
            let mut ctrl = ctrl_arc.lock().expect("controller lock");
            ctrl.enqueue_ui_action(DeferredUiAction::Shutdown);
            IpcResponse::Ok {
                message: "Daemon shutting down".to_string(),
                status: None,
                monitors: None,
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectiveOffPlan {
    SleepAll,
    ApplyFilteredKeep(Vec<String>),
    DeactivateActive(Vec<String>),
    NoOp,
}

fn plan_selective_off(
    phase: &DaemonPhase,
    all_connected: &[DisplayInfo],
    scope: &TargetScope,
    has_snapshot: bool,
) -> SelectiveOffPlan {
    let off_keys = resolve_scope_keys(all_connected, scope);
    if !all_connected.is_empty() && off_keys.len() >= all_connected.len() {
        return SelectiveOffPlan::SleepAll;
    }

    let keep_displays: Vec<&DisplayInfo> = all_connected
        .iter()
        .filter(|d| {
            !off_keys
                .iter()
                .any(|k| k.eq_ignore_ascii_case(&d.stable_key))
        })
        .collect();
    if scope.is_explicit_keep_layout()
        && *phase == DaemonPhase::SelectiveOff
        && has_snapshot
        && keep_displays.iter().any(|d| !d.active)
    {
        return SelectiveOffPlan::ApplyFilteredKeep(
            keep_displays.iter().map(|d| d.stable_key.clone()).collect(),
        );
    }

    let active_displays: Vec<&DisplayInfo> = all_connected.iter().filter(|d| d.active).collect();
    let active_off_keys: Vec<String> = active_displays
        .iter()
        .filter(|d| {
            off_keys
                .iter()
                .any(|k| k.eq_ignore_ascii_case(&d.stable_key))
        })
        .map(|d| d.stable_key.clone())
        .collect();
    if active_off_keys.is_empty() {
        return SelectiveOffPlan::NoOp;
    }
    if active_off_keys.len() >= active_displays.len() {
        return SelectiveOffPlan::SleepAll;
    }

    SelectiveOffPlan::DeactivateActive(active_off_keys)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SelectiveChangeAction {
    None,
    ReDeactivate(Vec<String>),
    SyncDisabledKeys(Vec<String>),
}

fn other_topology_changed(
    prev_displays: &[DisplayInfo],
    curr_displays: &[DisplayInfo],
    disabled_keys: &[String],
) -> bool {
    let build_sig = |list: &[DisplayInfo]| {
        let mut sig: Vec<(String, bool, bool)> = list
            .iter()
            .filter(|d| {
                !disabled_keys
                    .iter()
                    .any(|k| k.eq_ignore_ascii_case(&d.stable_key))
            })
            .map(|d| (d.stable_key.to_ascii_lowercase(), d.available, d.active))
            .collect();
        sig.sort();
        sig
    };
    build_sig(prev_displays) != build_sig(curr_displays)
}

fn plan_selective_display_change(
    curr_displays: &[DisplayInfo],
    disabled_keys: &[String],
    within_hpd_window: bool,
    selective_job_in_flight: bool,
    reenforce_attempts: u32,
    cooldown_elapsed: bool,
) -> SelectiveChangeAction {
    let reactivated: Vec<String> = curr_displays
        .iter()
        .filter(|d| {
            d.available
                && d.active
                && disabled_keys
                    .iter()
                    .any(|k| k.eq_ignore_ascii_case(&d.stable_key))
        })
        .map(|d| d.stable_key.clone())
        .collect();
    let still_disabled: Vec<String> = curr_displays
        .iter()
        .filter(|d| d.available && !d.active)
        .map(|d| d.stable_key.clone())
        .collect();

    if reactivated.is_empty() {
        if still_disabled.is_empty() && !within_hpd_window && !selective_job_in_flight {
            return SelectiveChangeAction::SyncDisabledKeys(Vec::new());
        }
        return SelectiveChangeAction::None;
    }

    let remaining_other_active = curr_displays
        .iter()
        .filter(|d| {
            d.available
                && d.active
                && !disabled_keys
                    .iter()
                    .any(|k| k.eq_ignore_ascii_case(&d.stable_key))
        })
        .count();

    // Never turn off the last remaining active screen when an external monitor disconnects.
    if remaining_other_active == 0 {
        return SelectiveChangeAction::SyncDisabledKeys(still_disabled);
    }

    if within_hpd_window {
        if selective_job_in_flight {
            return SelectiveChangeAction::None;
        }
        if reenforce_attempts >= HPD_MAX_REENFORCE_ATTEMPTS {
            return SelectiveChangeAction::SyncDisabledKeys(still_disabled);
        }
        if !cooldown_elapsed {
            return SelectiveChangeAction::None;
        }
        return SelectiveChangeAction::ReDeactivate(reactivated);
    }

    if selective_job_in_flight {
        return SelectiveChangeAction::None;
    }
    SelectiveChangeAction::SyncDisabledKeys(still_disabled)
}

fn monitor_edid_token(m: &crate::display::snapshot::SavedMonitorMode) -> String {
    crate::win_util::extract_edid_token(&m.stable_key).unwrap_or_else(|| {
        if m.edid_mfg != 0 || m.edid_product != 0 {
            let mfg = crate::win_util::decode_edid_manufacturer(m.edid_mfg);
            if !mfg.is_empty() {
                return format!("{mfg}{:04X}", m.edid_product);
            }
        }
        String::new()
    })
}

fn filter_snapshot_for_displays(
    snap: &TopologySnapshot,
    keep_displays: &[&DisplayInfo],
) -> TopologySnapshot {
    let mut exact_used_monitors = HashSet::new();
    let mut exact_used_displays = HashSet::new();
    let mut selected = Vec::new();

    for m in &snap.monitors {
        if let Some((disp_idx, _)) = keep_displays
            .iter()
            .enumerate()
            .find(|(_, d)| d.stable_key.eq_ignore_ascii_case(&m.stable_key))
        {
            exact_used_monitors.insert(m.stable_key.to_ascii_lowercase());
            exact_used_displays.insert(disp_idx);
            selected.push(m.clone());
        }
    }

    // Reserve all exact identities before allowing unique model fallback.
    for m in &snap.monitors {
        if exact_used_monitors.contains(&m.stable_key.to_ascii_lowercase()) {
            continue;
        }
        let token = monitor_edid_token(m);
        if token.is_empty() {
            continue;
        }
        let unmatched_saved_same = snap
            .monitors
            .iter()
            .filter(|other| {
                !exact_used_monitors.contains(&other.stable_key.to_ascii_lowercase())
                    && monitor_edid_token(other).eq_ignore_ascii_case(&token)
            })
            .count();
        if unmatched_saved_same != 1 {
            continue;
        }
        let matching_avail: Vec<usize> = keep_displays
            .iter()
            .enumerate()
            .filter(|(idx, d)| {
                !exact_used_displays.contains(idx) && d.edid_code.eq_ignore_ascii_case(&token)
            })
            .map(|(idx, _)| idx)
            .collect();
        if matching_avail.len() == 1 {
            exact_used_displays.insert(matching_avail[0]);
            selected.push(m.clone());
        }
    }

    TopologySnapshot {
        captured_at_unix_ms: snap.captured_at_unix_ms,
        monitors: selected,
    }
}

fn describe_keys(displays: &[DisplayInfo], keys: &[String]) -> String {
    keys.iter()
        .map(|k| {
            displays
                .iter()
                .find(|d| d.stable_key.eq_ignore_ascii_case(k))
                .map(|d| d.friendly_name.as_str())
                .unwrap_or(k.as_str())
                .to_string()
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn describe_other_topology_diff(
    prev_displays: &[DisplayInfo],
    curr_displays: &[DisplayInfo],
    disabled_keys: &[String],
) -> String {
    let state_map = |list: &[DisplayInfo]| {
        let mut map = std::collections::BTreeMap::new();
        for d in list {
            if disabled_keys
                .iter()
                .any(|k| k.eq_ignore_ascii_case(&d.stable_key))
            {
                continue;
            }
            map.insert(
                d.stable_key.to_ascii_lowercase(),
                (d.friendly_name.clone(), d.available, d.active),
            );
        }
        map
    };
    let prev = state_map(prev_displays);
    let curr = state_map(curr_displays);
    let mut keys: Vec<String> = prev
        .keys()
        .chain(curr.keys())
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    keys.sort();
    let mut diffs = Vec::new();
    for key in keys {
        match (prev.get(&key), curr.get(&key)) {
            (Some((_, p_avail, p_act)), Some((name, c_avail, c_act))) => {
                if (*p_avail, *p_act) != (*c_avail, *c_act) {
                    let event = if !*c_avail && *p_avail {
                        "disconnected"
                    } else if *c_avail && !*p_avail {
                        "connected"
                    } else if *c_act && !*p_act {
                        "turned on"
                    } else {
                        "turned off"
                    };
                    diffs.push(format!("{name} {event}"));
                }
            }
            (Some((name, p_avail, _)), None) if *p_avail => {
                diffs.push(format!("{name} disconnected"));
            }
            (None, Some((name, c_avail, _))) if *c_avail => {
                diffs.push(format!("{name} connected"));
            }
            _ => {}
        }
    }
    diffs.join(", ")
}

fn reconcile_disabled_keys(
    intended_disabled: &[String],
    live_displays: &[DisplayInfo],
) -> Vec<String> {
    let mut reconciled = Vec::new();
    for key in intended_disabled {
        if reconciled
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(key))
        {
            continue;
        }
        let is_now_active = live_displays
            .iter()
            .any(|d| d.available && d.active && d.stable_key.eq_ignore_ascii_case(key));
        if !is_now_active {
            reconciled.push(key.clone());
        }
    }
    reconciled
}

fn worker_loop(hwnd_raw: usize, rx: Receiver<WorkerJob>, tx: Sender<WorkerResult>) {
    while let Ok(job) = rx.recv() {
        let result = match job {
            WorkerJob::SleepAll {
                trigger_vk,
                wake_mode,
            } => {
                hotkey::wait_for_keys_released(trigger_vk, 800, 80);
                send_monitors_off_power(Some(hwnd_raw));
                WorkerResult::SleepAllArmed { wake_mode }
            }
            WorkerJob::DeactivateSelective {
                target_keys,
                target_names,
            } => {
                let t = Instant::now();
                match deactivate_targets(&target_keys) {
                    Ok(DeactivateResult::WouldDisableAll) => WorkerResult::SelectiveNeedsFullSleep,
                    Ok(DeactivateResult::Deactivated(n)) => {
                        let ms = t.elapsed().as_millis();
                        let live_displays = query_displays(true).unwrap_or_default();
                        logging::info(format!(
                            "Selective off deactivated {n} display(s) in {ms}ms [targets: {target_names}]"
                        ));
                        WorkerResult::SelectiveApplied { live_displays }
                    }
                    Ok(DeactivateResult::NothingMatched) => WorkerResult::Failed(
                        "No active display matched the requested selection".into(),
                    ),
                    Err(e) => WorkerResult::Failed(format!("DeactivateSelective failed: {e}")),
                }
            }
            WorkerJob::ApplyFilteredSnapshot {
                filtered_snapshot,
                config,
            } => {
                let t = Instant::now();
                match apply_snapshot(&filtered_snapshot, false) {
                    Err(e) => WorkerResult::Failed(format!("ApplyFilteredSnapshot failed: {e}")),
                    Ok(outcome) => {
                        let mut retrained_usb = 0usize;
                        if config.recovery.retrain_usb_displays_on_wake {
                            let live = query_displays(false).unwrap_or_default();
                            let usb_gdis: Vec<String> = live
                                .into_iter()
                                .filter(|d| {
                                    d.role == crate::display::DisplayRole::UsbIndirect
                                        && !d.gdi_name.is_empty()
                                        && !has_live_gdi_mode(&d.gdi_name)
                                })
                                .map(|d| d.gdi_name)
                                .collect();
                            if !usb_gdis.is_empty() {
                                retrained_usb = usb_gdis.len();
                                let _ = force_mode_recommit(&usb_gdis);
                            }
                        }
                        let live_after = query_displays(true).unwrap_or_default();
                        let active_after: Vec<&DisplayInfo> =
                            live_after.iter().filter(|d| d.active).collect();
                        let verified = outcome.restored_keys.iter().all(|k| {
                            active_after
                                .iter()
                                .any(|d| d.stable_key.eq_ignore_ascii_case(k))
                        });
                        if !verified {
                            WorkerResult::Failed(
                                "ApplyFilteredSnapshot left one or more target displays inactive"
                                    .into(),
                            )
                        } else {
                            let ms = t.elapsed().as_millis();
                            logging::info(format!(
                                "Selective profile applied in {ms}ms [kept={}, relaxed={}, retrained_usb={retrained_usb}]",
                                outcome.restored_keys.len(),
                                outcome.used_relaxed_timings,
                            ));
                            WorkerResult::SelectiveApplied {
                                live_displays: live_after,
                            }
                        }
                    }
                }
            }
            WorkerJob::Recover {
                level,
                config,
                snapshot,
                saved_windows,
                wake_ctx,
            } => {
                let res = run_recovery(
                    level,
                    &config,
                    snapshot.as_ref(),
                    &saved_windows,
                    Some(hwnd_raw),
                    wake_ctx,
                );
                WorkerResult::RecoveryFinished(res)
            }
        };

        let _ = tx.send(result);
        unsafe {
            PostMessageW(hwnd_raw as HWND, WM_APP_WORKER_DONE, 0, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::snapshot::SavedMonitorMode;
    use crate::display::DisplayRole;

    #[test]
    fn filtered_snapshot_uses_unique_edid_fallback_without_stealing_exact_matches() {
        let saved = TopologySnapshot {
            captured_at_unix_ms: 1,
            monitors: vec![
                SavedMonitorMode {
                    stable_key: "display#len61e9#old".into(),
                    friendly_name: "P27h-20".into(),
                    role: DisplayRole::External,
                    edid_mfg: 0x30AE,
                    edid_product: 0x61E9,
                    preferred_source_id: 0,
                    source_group: None,
                    position_x: 0,
                    position_y: 0,
                    width: 2560,
                    height: 1440,
                    pixel_format: 3,
                    rotation: 1,
                    scaling: 1,
                    refresh_num: 60000,
                    refresh_den: 1000,
                    scanline_ordering: 1,
                    target_video_signal: None,
                },
                SavedMonitorMode {
                    stable_key: "display#boe2775#exact".into(),
                    friendly_name: "J560T09".into(),
                    role: DisplayRole::External,
                    edid_mfg: 0x09E5,
                    edid_product: 0x2775,
                    preferred_source_id: 1,
                    source_group: None,
                    position_x: 2560,
                    position_y: 0,
                    width: 2560,
                    height: 1600,
                    pixel_format: 3,
                    rotation: 1,
                    scaling: 1,
                    refresh_num: 120000,
                    refresh_den: 1000,
                    scanline_ordering: 1,
                    target_video_signal: None,
                },
            ],
        };
        let keep = [DisplayInfo {
            friendly_name: "P27h-20".into(),
            stable_key: "display#len61e9#newport".into(),
            gdi_name: r"\\.\DISPLAY2".into(),
            adapter_path: String::new(),
            edid_code: "LEN61E9".into(),
            role: DisplayRole::External,
            active: false,
            available: true,
            is_primary: false,
            position: (0, 0),
            resolution: (0, 0),
            refresh_hz: 0.0,
            rotation: 1,
        }];
        let keep_refs: Vec<&DisplayInfo> = keep.iter().collect();
        let filtered = filter_snapshot_for_displays(&saved, &keep_refs);
        assert_eq!(filtered.monitors.len(), 1);
        assert_eq!(filtered.monitors[0].stable_key, "display#len61e9#old");
    }

    fn make_disp(key: &str, name: &str, role: DisplayRole, active: bool) -> DisplayInfo {
        DisplayInfo {
            friendly_name: name.into(),
            stable_key: key.into(),
            gdi_name: if active {
                r"\\.\DISPLAY1".into()
            } else {
                String::new()
            },
            adapter_path: String::new(),
            edid_code: crate::win_util::extract_edid_token(key).unwrap_or_default(),
            role,
            active,
            available: true,
            is_primary: false,
            position: (0, 0),
            resolution: if active { (2560, 1600) } else { (0, 0) },
            refresh_hz: if active { 60.0 } else { 0.0 },
            rotation: 1,
        }
    }

    #[test]
    fn turning_off_another_monitor_keeps_existing_off_monitors_off() {
        let displays = vec![
            make_disp("display#boe2775#1", "J560T09", DisplayRole::External, true),
            make_disp("display#len61e9#2", "P27h-20", DisplayRole::External, true),
            make_disp(
                "display#aus1641#3",
                "ASUS MB16AC",
                DisplayRole::UsbIndirect,
                true,
            ),
            make_disp(
                "display#len8aa5#4",
                "Internal Display",
                DisplayRole::Internal,
                false,
            ),
        ];

        assert_eq!(
            plan_selective_off(
                &DaemonPhase::SelectiveOff,
                &displays,
                &TargetScope::Monitor("display#aus1641#3".into()),
                true,
            ),
            SelectiveOffPlan::DeactivateActive(vec!["display#aus1641#3".into()])
        );
        assert_eq!(
            plan_selective_off(
                &DaemonPhase::SelectiveOff,
                &displays,
                &TargetScope::Monitors(vec!["ASUS MB16AC".into(), "P27h-20".into()]),
                true,
            ),
            SelectiveOffPlan::DeactivateActive(vec![
                "display#len61e9#2".into(),
                "display#aus1641#3".into(),
            ])
        );
        assert_eq!(
            plan_selective_off(
                &DaemonPhase::SelectiveOff,
                &displays,
                &TargetScope::Profile {
                    include: Vec::new(),
                    exclude: vec!["display#aus1641#3".into()],
                },
                true,
            ),
            SelectiveOffPlan::DeactivateActive(vec!["display#aus1641#3".into()])
        );
        assert_eq!(
            plan_selective_off(
                &DaemonPhase::SelectiveOff,
                &displays,
                &TargetScope::Monitor("display#len8aa5#4".into()),
                true,
            ),
            SelectiveOffPlan::NoOp
        );
    }

    #[test]
    fn turning_off_last_active_monitor_sleeps_all_instead_of_waking_others() {
        let all_externals_remaining = vec![
            make_disp("display#boe2775#1", "J560T09", DisplayRole::External, true),
            make_disp("display#len61e9#2", "P27h-20", DisplayRole::External, true),
            make_disp(
                "display#len8aa5#4",
                "Internal Display",
                DisplayRole::Internal,
                false,
            ),
        ];
        assert_eq!(
            plan_selective_off(
                &DaemonPhase::SelectiveOff,
                &all_externals_remaining,
                &TargetScope::ExternalsOnly,
                true,
            ),
            SelectiveOffPlan::SleepAll
        );

        let one_remaining = vec![
            make_disp("display#boe2775#1", "J560T09", DisplayRole::External, true),
            make_disp("display#len61e9#2", "P27h-20", DisplayRole::External, false),
            make_disp(
                "display#aus1641#3",
                "ASUS MB16AC",
                DisplayRole::UsbIndirect,
                false,
            ),
            make_disp(
                "display#len8aa5#4",
                "Internal Display",
                DisplayRole::Internal,
                false,
            ),
        ];
        assert_eq!(
            plan_selective_off(
                &DaemonPhase::SelectiveOff,
                &one_remaining,
                &TargetScope::Monitor("display#boe2775#1".into()),
                true,
            ),
            SelectiveOffPlan::SleepAll
        );
    }

    #[test]
    fn explicit_keep_layout_profile_still_switches_between_profiles() {
        let displays = vec![
            make_disp("display#boe2775#1", "J560T09", DisplayRole::External, true),
            make_disp("display#len61e9#2", "P27h-20", DisplayRole::External, false),
            make_disp(
                "display#aus1641#3",
                "ASUS MB16AC",
                DisplayRole::UsbIndirect,
                false,
            ),
            make_disp(
                "display#len8aa5#4",
                "Internal Display",
                DisplayRole::Internal,
                true,
            ),
        ];
        assert_eq!(
            plan_selective_off(
                &DaemonPhase::SelectiveOff,
                &displays,
                &TargetScope::Profile {
                    include: vec!["P27h-20".into(), "J560T09".into()],
                    exclude: Vec::new(),
                },
                true,
            ),
            SelectiveOffPlan::ApplyFilteredKeep(vec![
                "display#boe2775#1".into(),
                "display#len61e9#2".into(),
            ])
        );
        assert_eq!(
            plan_selective_off(
                &DaemonPhase::SelectiveOff,
                &displays,
                &TargetScope::Except(vec!["P27h-20".into(), "J560T09".into()]),
                true,
            ),
            SelectiveOffPlan::ApplyFilteredKeep(vec![
                "display#boe2775#1".into(),
                "display#len61e9#2".into(),
            ])
        );
    }

    #[test]
    fn hpd_power_off_or_on_of_external_monitor_re_disables_internal_display() {
        let disabled = vec!["display#len8aa5#4".to_string()];

        // Case 1: Power button OFF on 27" P27h-20 while Internal Display was off;
        // Windows drops P27h-20 and reactivates Internal Display.
        let prev_off = vec![
            make_disp("display#boe2775#1", "J560T09", DisplayRole::External, true),
            make_disp("display#len61e9#2", "P27h-20", DisplayRole::External, true),
            make_disp(
                "display#len8aa5#4",
                "Internal Display",
                DisplayRole::Internal,
                false,
            ),
        ];
        let curr_off = vec![
            make_disp("display#boe2775#1", "J560T09", DisplayRole::External, true),
            make_disp(
                "display#len8aa5#4",
                "Internal Display",
                DisplayRole::Internal,
                true,
            ),
        ];
        assert!(other_topology_changed(&prev_off, &curr_off, &disabled));
        assert_eq!(
            plan_selective_display_change(&curr_off, &disabled, true, false, 0, true),
            SelectiveChangeAction::ReDeactivate(vec!["display#len8aa5#4".into()])
        );

        // Case 2: Power button ON on 27" P27h-20 while Internal Display was off;
        // Windows reconnects P27h-20 and loads persisted topology with Internal Display ON.
        let prev_on = vec![
            make_disp("display#boe2775#1", "J560T09", DisplayRole::External, true),
            make_disp(
                "display#len8aa5#4",
                "Internal Display",
                DisplayRole::Internal,
                false,
            ),
        ];
        let curr_on = vec![
            make_disp("display#boe2775#1", "J560T09", DisplayRole::External, true),
            make_disp("display#len61e9#2", "P27h-20", DisplayRole::External, true),
            make_disp(
                "display#len8aa5#4",
                "Internal Display",
                DisplayRole::Internal,
                true,
            ),
        ];
        assert!(other_topology_changed(&prev_on, &curr_on, &disabled));
        assert_eq!(
            plan_selective_display_change(&curr_on, &disabled, true, false, 0, true),
            SelectiveChangeAction::ReDeactivate(vec!["display#len8aa5#4".into()])
        );
    }

    #[test]
    fn unplugging_last_active_external_keeps_internal_display_on_for_safety() {
        let disabled = vec!["display#len8aa5#4".to_string()];
        let prev = vec![
            make_disp("display#boe2775#1", "J560T09", DisplayRole::External, true),
            make_disp(
                "display#len8aa5#4",
                "Internal Display",
                DisplayRole::Internal,
                false,
            ),
        ];
        let curr = vec![make_disp(
            "display#len8aa5#4",
            "Internal Display",
            DisplayRole::Internal,
            true,
        )];
        assert!(other_topology_changed(&prev, &curr, &disabled));
        assert_eq!(
            plan_selective_display_change(&curr, &disabled, true, false, 0, true),
            SelectiveChangeAction::SyncDisabledKeys(Vec::new())
        );
    }

    #[test]
    fn manual_windows_reenable_without_hpd_is_respected_and_retry_cap_prevents_loops() {
        let disabled = vec!["display#len8aa5#4".to_string()];
        let curr = vec![
            make_disp("display#boe2775#1", "J560T09", DisplayRole::External, true),
            make_disp(
                "display#len8aa5#4",
                "Internal Display",
                DisplayRole::Internal,
                true,
            ),
        ];

        assert_eq!(
            plan_selective_display_change(&curr, &disabled, false, false, 0, true),
            SelectiveChangeAction::SyncDisabledKeys(Vec::new())
        );
        assert_eq!(
            plan_selective_display_change(&curr, &disabled, true, false, 2, true),
            SelectiveChangeAction::SyncDisabledKeys(Vec::new())
        );
        assert_eq!(
            plan_selective_display_change(&curr, &disabled, true, true, 0, true),
            SelectiveChangeAction::None
        );
    }

    #[test]
    fn other_topology_diff_describes_connect_and_disconnect() {
        let disabled = vec!["display#len8aa5#4".to_string()];
        let prev = vec![
            make_disp("display#boe2775#1", "J560T09", DisplayRole::External, true),
            make_disp("display#len61e9#2", "P27h-20", DisplayRole::External, true),
            make_disp(
                "display#len8aa5#4",
                "Internal Display",
                DisplayRole::Internal,
                false,
            ),
        ];
        let after_power_off = vec![
            make_disp("display#boe2775#1", "J560T09", DisplayRole::External, true),
            make_disp(
                "display#len8aa5#4",
                "Internal Display",
                DisplayRole::Internal,
                true,
            ),
        ];
        assert_eq!(
            describe_other_topology_diff(&prev, &after_power_off, &disabled),
            "P27h-20 disconnected"
        );
        assert_eq!(
            describe_other_topology_diff(&after_power_off, &prev, &disabled),
            "P27h-20 connected"
        );
    }

    #[test]
    fn reconcile_disabled_keys_never_claims_unrelated_powered_off_monitor() {
        let intended = vec![
            "display#len8aa5#4".to_string(),
            "display#aus1641#3".to_string(),
        ];
        let mut p27 = make_disp("display#len61e9#2", "P27h-20", DisplayRole::External, false);
        p27.available = true; // Powered off externally, still reporting available=true.
        let mut usb = make_disp(
            "display#aus1641#3",
            "ASUS MB16AC",
            DisplayRole::UsbIndirect,
            false,
        );
        usb.available = false; // Dropped from available set while disabled.

        let live = vec![
            make_disp("display#boe2775#1", "J560T09", DisplayRole::External, true),
            p27.clone(),
            make_disp(
                "display#len8aa5#4",
                "Internal Display",
                DisplayRole::Internal,
                false,
            ),
            usb,
        ];
        assert_eq!(
            reconcile_disabled_keys(&intended, &live),
            vec![
                "display#len8aa5#4".to_string(),
                "display#aus1641#3".to_string(),
            ]
        );

        let reactivated_live = vec![
            make_disp("display#boe2775#1", "J560T09", DisplayRole::External, true),
            p27,
            make_disp(
                "display#len8aa5#4",
                "Internal Display",
                DisplayRole::Internal,
                true,
            ),
        ];
        assert_eq!(
            reconcile_disabled_keys(&intended, &reactivated_live),
            vec!["display#aus1641#3".to_string()]
        );
    }
}
