//! Captures and restores top-level application window positions across monitor sleep/toggle.

use std::collections::{HashMap, HashSet};
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HDC, HMONITOR,
    MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowLongW, GetWindowPlacement, GetWindowRect, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, SetWindowPos, ShowWindowAsync,
    GWL_EXSTYLE, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER,
    SW_MAXIMIZE, SW_SHOWMAXIMIZED, SW_SHOWNOACTIVATE, WINDOWPLACEMENT, WS_EX_TOOLWINDOW,
};

use crate::display::DisplayInfo;
use crate::win_util::from_wide_slice;

#[derive(Clone)]
pub struct SavedWindow {
    pub hwnd: usize,
    pub pid: u32,
    pub monitor_stable_key: String,
    pub monitor_work: RECT,
    pub window_rect: RECT,
    pub normal_rect: RECT,
    pub is_maximized: bool,
}

fn workspace_to_screen_rect(normal: RECT, primary_monitor: RECT, primary_work: RECT) -> RECT {
    let dx = primary_work.left - primary_monitor.left;
    let dy = primary_work.top - primary_monitor.top;
    RECT {
        left: normal.left + dx,
        top: normal.top + dy,
        right: normal.right + dx,
        bottom: normal.bottom + dy,
    }
}

fn query_primary_workspace_offset() -> (RECT, RECT) {
    let hmon = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    if !hmon.is_null() {
        let mut mi: MONITORINFO = unsafe { zeroed() };
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        if unsafe { GetMonitorInfoW(hmon, &mut mi) } != 0 {
            return (mi.rcMonitor, mi.rcWork);
        }
    }
    let zero: RECT = unsafe { zeroed() };
    (zero, zero)
}

/// Captures visible, uncloaked, non-tool top-level application windows and maps each
/// to the `stable_key` of the monitor currently hosting it.
pub fn capture_windows(active_displays: &[DisplayInfo]) -> Vec<SavedWindow> {
    let gdi_to_key: HashMap<String, String> = active_displays
        .iter()
        .filter(|d| d.active && !d.gdi_name.is_empty())
        .map(|d| (d.gdi_name.to_ascii_lowercase(), d.stable_key.clone()))
        .collect();

    if gdi_to_key.is_empty() {
        return Vec::new();
    }

    let (primary_mon, primary_work) = query_primary_workspace_offset();

    struct Ctx<'a> {
        gdi_to_key: &'a HashMap<String, String>,
        primary_mon: RECT,
        primary_work: RECT,
        out: Vec<SavedWindow>,
    }

    unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam as *mut Ctx<'_>);
        if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
            return 1;
        }
        if GetWindowTextLengthW(hwnd) <= 0 {
            return 1;
        }
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if (ex_style & WS_EX_TOOLWINDOW) != 0 {
            return 1;
        }
        let mut cloaked: u32 = 0;
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            (&mut cloaked as *mut u32).cast(),
            size_of::<u32>() as u32,
        );
        if hr == 0 && cloaked != 0 {
            return 1;
        }

        let mut placement: WINDOWPLACEMENT = zeroed();
        placement.length = size_of::<WINDOWPLACEMENT>() as u32;
        if GetWindowPlacement(hwnd, &mut placement) == 0 {
            return 1;
        }

        let mut win_rect: RECT = zeroed();
        if GetWindowRect(hwnd, &mut win_rect) == 0 {
            return 1;
        }
        if win_rect.right - win_rect.left <= 0 || win_rect.bottom - win_rect.top <= 0 {
            return 1;
        }

        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if hmon.is_null() {
            return 1;
        }
        let mut mi: MONITORINFOEXW = zeroed();
        mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(hmon, &mut mi.monitorInfo) == 0 {
            return 1;
        }
        let gdi = from_wide_slice(&mi.szDevice).to_ascii_lowercase();
        let Some(stable_key) = ctx.gdi_to_key.get(&gdi) else {
            return 1;
        };

        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);

        // WINDOWPLACEMENT uses workspace coordinates offset by the primary monitor's work-area
        // origin, while SetWindowPos expects screen coordinates.
        let normal_rect = workspace_to_screen_rect(
            placement.rcNormalPosition,
            ctx.primary_mon,
            ctx.primary_work,
        );

        ctx.out.push(SavedWindow {
            hwnd: hwnd as usize,
            pid,
            monitor_stable_key: stable_key.clone(),
            monitor_work: mi.monitorInfo.rcWork,
            window_rect: win_rect,
            normal_rect,
            is_maximized: placement.showCmd == SW_SHOWMAXIMIZED as u32,
        });

        1
    }

    let mut ctx = Ctx {
        gdi_to_key: &gdi_to_key,
        primary_mon,
        primary_work,
        out: Vec::new(),
    };
    unsafe {
        EnumWindows(Some(enum_cb), (&mut ctx as *mut Ctx<'_>) as LPARAM);
    }
    ctx.out
}

/// Polls `EnumDisplayMonitors` until all active displays' GDI names are live (or `max_wait_ms`).
/// Returns `(elapsed_ms, gdi_name -> work_rect)` immediately when GDI already exposes all monitors.
pub fn wait_for_gdi_monitors(
    active_displays: &[DisplayInfo],
    max_wait_ms: u64,
) -> (u64, HashMap<String, RECT>) {
    let t0 = Instant::now();
    let expected: HashSet<String> = active_displays
        .iter()
        .filter(|d| d.active && !d.gdi_name.is_empty())
        .map(|d| d.gdi_name.to_ascii_lowercase())
        .collect();

    unsafe extern "system" fn collect_cb(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let seen = &mut *(lparam as *mut HashMap<String, RECT>);
        let mut mi: MONITORINFOEXW = zeroed();
        mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(hmon, &mut mi.monitorInfo) != 0 {
            seen.insert(
                from_wide_slice(&mi.szDevice).to_ascii_lowercase(),
                mi.monitorInfo.rcWork,
            );
        }
        1
    }

    let deadline = Instant::now() + Duration::from_millis(max_wait_ms);
    let mut seen: HashMap<String, RECT> = HashMap::with_capacity(expected.len().max(4));
    loop {
        seen.clear();
        unsafe {
            EnumDisplayMonitors(
                null_mut(),
                null_mut(),
                Some(collect_cb),
                (&mut seen as *mut HashMap<String, RECT>) as LPARAM,
            );
        }
        if expected.is_empty()
            || expected.iter().all(|g| seen.contains_key(g))
            || Instant::now() >= deadline
        {
            break;
        }
        thread::sleep(Duration::from_millis(15));
    }
    (t0.elapsed().as_millis() as u64, seen)
}

const MAX_RESTORE_THREADS: usize = 8;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestoreStats {
    pub restored: usize,
    pub failed: usize,
    pub threads_used: usize,
    pub max_window_ms: u64,
}

struct PendingWindowRestore<'a> {
    window: &'a SavedWindow,
    cur_show_cmd: u32,
    cur_rect: RECT,
    target_work: RECT,
}

/// Translates a saved screen rectangle relative to its old monitor work area into the
/// destination work area and clamps it so a surviving monitor moving to `(0, 0)` never
/// places restored windows off-screen.
fn compute_restored_window_rect(saved_rect: RECT, old_work: RECT, new_work: RECT) -> RECT {
    let work_w = (new_work.right - new_work.left).max(100);
    let work_h = (new_work.bottom - new_work.top).max(80);
    let width = (saved_rect.right - saved_rect.left).clamp(100, work_w);
    let height = (saved_rect.bottom - saved_rect.top).clamp(80, work_h);
    let dx = saved_rect.left - old_work.left;
    let dy = saved_rect.top - old_work.top;
    let max_x = (new_work.right - width).max(new_work.left);
    let max_y = (new_work.bottom - height).max(new_work.top);
    let left = (new_work.left + dx).clamp(new_work.left, max_x);
    let top = (new_work.top + dy).clamp(new_work.top, max_y);
    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

fn partition_into_buckets<T>(items: Vec<T>, max_buckets: usize) -> Vec<Vec<T>> {
    if items.is_empty() {
        return Vec::new();
    }
    let cap = max_buckets.max(1);
    let bucket_count = items.len().min(cap);
    let mut buckets: Vec<Vec<T>> = (0..bucket_count).map(|_| Vec::new()).collect();
    for (idx, item) in items.into_iter().enumerate() {
        buckets[idx % bucket_count].push(item);
    }
    buckets
}

fn restore_one_window(item: &PendingWindowRestore<'_>) -> (u64, bool) {
    let t0 = Instant::now();
    let w = item.window;
    let hwnd = w.hwnd as HWND;
    let ok = unsafe {
        if w.is_maximized {
            let work = item.target_work;
            let work_w = (work.right - work.left).max(200);
            let work_h = (work.bottom - work.top).max(150);
            let norm_w = (w.normal_rect.right - w.normal_rect.left).clamp(200, work_w);
            let norm_h = (w.normal_rect.bottom - w.normal_rect.top).clamp(150, work_h);
            let nx = if w.normal_rect.left >= work.left && w.normal_rect.left < work.right - 100 {
                w.normal_rect.left
            } else {
                work.left + 40
            };
            let ny = if w.normal_rect.top >= work.top && w.normal_rect.top < work.bottom - 100 {
                w.normal_rect.top
            } else {
                work.top + 40
            };
            if item.cur_show_cmd == SW_SHOWMAXIMIZED as u32 {
                ShowWindowAsync(hwnd, SW_SHOWNOACTIVATE);
            }
            let pos_ok = SetWindowPos(
                hwnd,
                null_mut(),
                nx,
                ny,
                norm_w,
                norm_h,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS,
            ) != 0;
            if pos_ok {
                ShowWindowAsync(hwnd, SW_MAXIMIZE);
            }
            pos_ok
        } else {
            if item.cur_show_cmd == SW_SHOWMAXIMIZED as u32 {
                ShowWindowAsync(hwnd, SW_SHOWNOACTIVATE);
            }
            let dest =
                compute_restored_window_rect(w.window_rect, w.monitor_work, item.target_work);
            let width = dest.right - dest.left;
            let height = dest.bottom - dest.top;
            let cur_w = item.cur_rect.right - item.cur_rect.left;
            let cur_h = item.cur_rect.bottom - item.cur_rect.top;
            let size_flag = if cur_w == width && cur_h == height {
                SWP_NOSIZE
            } else {
                0
            };
            SetWindowPos(
                hwnd,
                null_mut(),
                dest.left,
                dest.top,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS | size_flag,
            ) != 0
        }
    };
    (t0.elapsed().as_millis() as u64, ok)
}

/// Restores displaced windows back onto their saved monitor (`SWP_ASYNCWINDOWPOS` + `ShowWindowAsync`),
/// dispatching distinct `HWND`s concurrently up to `MAX_RESTORE_THREADS` while preserving per-window call ordering.
/// Note: Restoring a previously maximized window via `SW_MAXIMIZE` may activate that window.
pub fn restore_windows(
    saved: &[SavedWindow],
    current_displays: &[DisplayInfo],
    live_work_by_gdi: &HashMap<String, RECT>,
) -> RestoreStats {
    let key_to_gdi: HashMap<String, String> = current_displays
        .iter()
        .filter(|d| d.active && !d.gdi_name.is_empty())
        .map(|d| {
            (
                d.stable_key.to_ascii_lowercase(),
                d.gdi_name.to_ascii_lowercase(),
            )
        })
        .collect();

    if key_to_gdi.is_empty() {
        return RestoreStats::default();
    }

    let mut pending = Vec::new();

    for w in saved {
        let hwnd = w.hwnd as HWND;
        unsafe {
            if IsWindow(hwnd) == 0 || IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
                continue;
            }
            let mut cur_pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut cur_pid);
            if cur_pid != w.pid {
                continue;
            }
            let Some(target_gdi) = key_to_gdi.get(&w.monitor_stable_key.to_ascii_lowercase())
            else {
                continue;
            };

            let mut cur_rect: RECT = zeroed();
            if GetWindowRect(hwnd, &mut cur_rect) == 0 {
                continue;
            }
            let mut cur_place: WINDOWPLACEMENT = zeroed();
            cur_place.length = size_of::<WINDOWPLACEMENT>() as u32;
            if GetWindowPlacement(hwnd, &mut cur_place) == 0 {
                continue;
            }

            let target_work = live_work_by_gdi
                .get(target_gdi)
                .copied()
                .unwrap_or(w.monitor_work);
            let expected_rect = if w.is_maximized {
                w.window_rect
            } else {
                compute_restored_window_rect(w.window_rect, w.monitor_work, target_work)
            };

            let moved_far = (cur_rect.left - expected_rect.left).abs() > 12
                || (cur_rect.top - expected_rect.top).abs() > 12
                || (cur_rect.right - expected_rect.right).abs() > 12
                || (cur_rect.bottom - expected_rect.bottom).abs() > 12
                || (w.is_maximized && cur_place.showCmd != SW_SHOWMAXIMIZED as u32);

            if !moved_far {
                continue;
            }

            pending.push(PendingWindowRestore {
                window: w,
                cur_show_cmd: cur_place.showCmd,
                cur_rect,
                target_work,
            });
        }
    }

    let buckets = partition_into_buckets(pending, MAX_RESTORE_THREADS);
    let threads_used = buckets.len();
    let max_window_ms = AtomicU64::new(0);
    let restored_ok = AtomicUsize::new(0);
    let restored_failed = AtomicUsize::new(0);

    let run_bucket = |bucket: &[PendingWindowRestore<'_>]| {
        for item in bucket {
            let (elapsed, ok) = restore_one_window(item);
            max_window_ms.fetch_max(elapsed, Ordering::Relaxed);
            if ok {
                restored_ok.fetch_add(1, Ordering::Relaxed);
            } else {
                restored_failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    };

    if threads_used <= 1 {
        if let Some(bucket) = buckets.first() {
            run_bucket(bucket);
        }
    } else {
        thread::scope(|scope| {
            for bucket in &buckets {
                scope.spawn(|| run_bucket(bucket));
            }
        });
    }

    RestoreStats {
        restored: restored_ok.load(Ordering::Relaxed),
        failed: restored_failed.load(Ordering::Relaxed),
        threads_used,
        max_window_ms: max_window_ms.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_workspace_coords_using_primary_monitor_offset() {
        let primary_monitor = RECT {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1440,
        };
        // Top + left docked taskbars on primary shift workspace (0,0) relative to screen.
        let primary_work = RECT {
            left: 64,
            top: 48,
            right: 2560,
            bottom: 1440,
        };
        let secondary_normal_ws = RECT {
            left: -2560,
            top: 100,
            right: -1200,
            bottom: 900,
        };
        let screen = workspace_to_screen_rect(secondary_normal_ws, primary_monitor, primary_work);
        assert_eq!(
            (screen.left, screen.top, screen.right, screen.bottom),
            (-2496, 148, -1136, 948)
        );
    }

    #[test]
    fn partitions_windows_round_robin_up_to_thread_cap() {
        assert!(partition_into_buckets::<usize>(Vec::new(), MAX_RESTORE_THREADS).is_empty());

        let small = partition_into_buckets(vec![10, 20, 30], MAX_RESTORE_THREADS);
        assert_eq!(small.len(), 3);
        assert_eq!(small, vec![vec![10], vec![20], vec![30]]);

        let many: Vec<usize> = (0..18).collect();
        let buckets = partition_into_buckets(many, MAX_RESTORE_THREADS);
        assert_eq!(buckets.len(), MAX_RESTORE_THREADS);
        let flattened: HashSet<usize> = buckets.iter().flatten().copied().collect();
        assert_eq!(flattened.len(), 18);
        assert!(buckets.iter().all(|b| (2..=3).contains(&b.len())));
    }

    #[test]
    fn translates_and_clamps_window_rect_when_monitor_origin_shifts() {
        let saved = RECT {
            left: 2660,
            top: 120,
            right: 3660,
            bottom: 820,
        };
        let old_work = RECT {
            left: 2560,
            top: 0,
            right: 5120,
            bottom: 1400,
        };
        // Surviving monitor shifted to origin (0, 0) with slightly smaller work area
        let new_work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let translated = compute_restored_window_rect(saved, old_work, new_work);
        assert_eq!(
            (
                translated.left,
                translated.top,
                translated.right,
                translated.bottom
            ),
            (100, 120, 1100, 820)
        );

        // Even if the offset exceeded new_work bounds, clamp onto new_work cleanly
        let far = RECT {
            left: 4900,
            top: 1200,
            right: 5900,
            bottom: 2000,
        };
        let clamped = compute_restored_window_rect(far, old_work, new_work);
        assert!(clamped.left >= new_work.left && clamped.right <= new_work.right);
        assert!(clamped.top >= new_work.top && clamped.bottom <= new_work.bottom);
    }
}
