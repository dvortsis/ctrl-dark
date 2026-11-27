//! Isolated low-level input guard + optional blackout curtain active ONLY while in `LockedOff`.
//! Keeps `WH_MOUSE_LL` / `WH_KEYBOARD_LL` unhooked 100% of the time during normal desktop use.

use std::collections::HashSet;
use std::mem::zeroed;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{GetStockObject, BLACK_BRUSH, HBRUSH};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU,
    VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetSystemMetrics, PeekMessageW, PostMessageW, PostThreadMessageW, RegisterClassW, SetCursor,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, MSG,
    MSLLHOOKSTRUCT, PM_NOREMOVE, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_KEYDOWN, WM_KEYUP, WM_QUIT,
    WM_SETCURSOR, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

use crate::hotkey::ParsedHotkey;
use crate::logging;
use crate::win_util::to_wide_null;

pub const WM_APP_GUARD_HOTKEY: u32 = WM_APP + 10;
pub const WM_APP_GUARD_UNLOCK: u32 = WM_APP + 11;

const LLMHF_INJECTED: u32 = 0x0000_0001;
const LLKHF_INJECTED: u32 = 0x0000_0010;

struct GuardShared {
    daemon_hwnd: usize,
    hotkeys: Vec<(i32, ParsedHotkey)>,
    esc_presses: Vec<Instant>,
    held_keys: HashSet<u16>,
    held_modifiers: HashSet<u16>,
}

static ACTIVE_SHARED: Mutex<Option<Arc<Mutex<GuardShared>>>> = Mutex::new(None);
static REGISTER_CLASS_ONCE: AtomicBool = AtomicBool::new(false);

pub struct InputGuardHandle {
    thread_id: u32,
    join: Option<thread::JoinHandle<()>>,
}

impl InputGuardHandle {
    /// Starts the temporary input guard & optional blackout curtain on its own thread.
    pub fn start(
        daemon_hwnd_raw: usize,
        hotkeys: Vec<(i32, ParsedHotkey)>,
        show_curtain: bool,
    ) -> Result<Self, String> {
        let held_modifiers = initial_modifiers(is_down);
        let shared = Arc::new(Mutex::new(GuardShared {
            daemon_hwnd: daemon_hwnd_raw,
            hotkeys,
            esc_presses: Vec::with_capacity(4),
            held_keys: HashSet::new(),
            held_modifiers,
        }));

        // A rendezvous guarantees a successful send has a live receiver/stop handle.
        let (tid_tx, tid_rx) = std::sync::mpsc::sync_channel(0);
        let join = thread::Builder::new()
            .name("input-guard".to_string())
            .spawn(move || {
                // PostThreadMessage fails until the receiving thread has a queue.
                let mut msg: MSG = unsafe { zeroed() };
                unsafe {
                    PeekMessageW(&mut msg, null_mut(), 0, 0, PM_NOREMOVE);
                }
                let tid = unsafe { GetCurrentThreadId() };
                // If startup timed out, never arm an orphan guard without a stop handle.
                if tid_tx.send(tid).is_ok() {
                    if let Ok(mut slot) = ACTIVE_SHARED.lock() {
                        *slot = Some(shared.clone());
                    }
                    run_guard_loop(show_curtain);
                    if let Ok(mut slot) = ACTIVE_SHARED.lock() {
                        if slot
                            .as_ref()
                            .is_some_and(|active| Arc::ptr_eq(active, &shared))
                        {
                            *slot = None;
                        }
                    }
                }
            })
            .map_err(|e| format!("Failed spawning input-guard thread: {e}"))?;

        let thread_id = tid_rx
            .recv_timeout(Duration::from_millis(1000))
            .map_err(|_| "Timed out waiting for input-guard thread ID".to_string())?;

        Ok(Self {
            thread_id,
            join: Some(join),
        })
    }

    /// Updates active guard hotkeys in place without tearing down the blackout curtain.
    pub fn update_hotkeys(&self, hotkeys: Vec<(i32, ParsedHotkey)>) {
        if let Ok(slot) = ACTIVE_SHARED.lock() {
            if let Some(shared_arc) = slot.as_ref() {
                if let Ok(mut shared) = shared_arc.lock() {
                    shared.hotkeys = hotkeys;
                }
            }
        }
    }

    /// Stops the input guard, removes both hooks, and destroys the curtain window.
    pub fn stop(mut self) {
        if self.thread_id != 0 {
            unsafe {
                PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
            }
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for InputGuardHandle {
    fn drop(&mut self) {
        if self.thread_id != 0 {
            unsafe {
                PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
            }
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn run_guard_loop(show_curtain: bool) {
    let hinst: HINSTANCE = unsafe { GetModuleHandleW(null_mut()) };
    let curtain_hwnd = if show_curtain {
        create_curtain_window(hinst)
    } else {
        null_mut()
    };

    let mouse_hook: HHOOK = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinst, 0) };
    let kbd_hook: HHOOK =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), hinst, 0) };

    if mouse_hook.is_null() || kbd_hook.is_null() {
        logging::warn("InputGuard failed to install one or both low-level hooks");
        // Fail open: never keep a curtain/input lock without a working escape path.
        unsafe {
            if !mouse_hook.is_null() {
                UnhookWindowsHookEx(mouse_hook);
            }
            if !kbd_hook.is_null() {
                UnhookWindowsHookEx(kbd_hook);
            }
            if !curtain_hwnd.is_null() {
                DestroyWindow(curtain_hwnd);
            }
        }
        if let Ok(slot) = ACTIVE_SHARED.lock() {
            if let Some(shared) = slot.as_ref() {
                if let Ok(shared) = shared.lock() {
                    unsafe {
                        PostMessageW(shared.daemon_hwnd as HWND, WM_APP_GUARD_UNLOCK, 0, 0);
                    }
                }
            }
        }
        return;
    }

    let mut msg: MSG = unsafe { zeroed() };
    unsafe {
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    unsafe {
        if !mouse_hook.is_null() {
            UnhookWindowsHookEx(mouse_hook);
        }
        if !kbd_hook.is_null() {
            UnhookWindowsHookEx(kbd_hook);
        }
        if !curtain_hwnd.is_null() {
            DestroyWindow(curtain_hwnd);
        }
    }
}

fn create_curtain_window(hinst: HINSTANCE) -> HWND {
    let class_name = to_wide_null("MonitorToggleBlackCurtain");
    if !REGISTER_CLASS_ONCE.swap(true, Ordering::SeqCst) {
        let mut wc: WNDCLASSW = unsafe { zeroed() };
        wc.lpfnWndProc = Some(curtain_wndproc);
        wc.hInstance = hinst;
        wc.hbrBackground = unsafe { GetStockObject(BLACK_BRUSH) as HBRUSH };
        wc.lpszClassName = class_name.as_ptr();
        unsafe {
            RegisterClassW(&wc);
        }
    }

    let vx = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let vy = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let vw = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(1);
    let vh = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(1);
    let title = to_wide_null("Monitor Toggle Curtain");

    unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_VISIBLE,
            vx,
            vy,
            vw,
            vh,
            null_mut(),
            null_mut(),
            hinst,
            null_mut(),
        )
    }
}

unsafe extern "system" fn curtain_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_SETCURSOR {
        SetCursor(null_mut());
        return 1;
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam != 0 {
        let info = &*(lparam as *const MSLLHOOKSTRUCT);
        if (info.flags & LLMHF_INJECTED) != 0 {
            return CallNextHookEx(null_mut(), code, wparam, lparam);
        }
        // Swallow physical mouse moves/clicks/scrolls while monitors are locked off.
        return 1;
    }
    CallNextHookEx(null_mut(), code, wparam, lparam)
}

fn is_modifier_vk(vk: u16) -> bool {
    matches!(
        vk,
        VK_CONTROL
            | VK_LCONTROL
            | VK_RCONTROL
            | VK_MENU
            | VK_LMENU
            | VK_RMENU
            | VK_SHIFT
            | VK_LSHIFT
            | VK_RSHIFT
            | VK_LWIN
            | VK_RWIN
    )
}

fn is_down(vk: u16) -> bool {
    (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
}

fn initial_modifiers(is_pressed: impl Fn(u16) -> bool) -> HashSet<u16> {
    // Low-level events release specific left/right keys. Seeding VK_CONTROL/VK_MENU/
    // VK_SHIFT as well would leave those aggregate aliases permanently pressed.
    [
        VK_LCONTROL,
        VK_RCONTROL,
        VK_LMENU,
        VK_RMENU,
        VK_LSHIFT,
        VK_RSHIFT,
        VK_LWIN,
        VK_RWIN,
    ]
    .into_iter()
    .filter(|vk| is_pressed(*vk))
    .collect()
}

fn modifier_flags_from_held(held_modifiers: &HashSet<u16>) -> (bool, bool, bool, bool) {
    let ctrl = held_modifiers.contains(&VK_CONTROL)
        || held_modifiers.contains(&VK_LCONTROL)
        || held_modifiers.contains(&VK_RCONTROL);
    let alt = held_modifiers.contains(&VK_MENU)
        || held_modifiers.contains(&VK_LMENU)
        || held_modifiers.contains(&VK_RMENU);
    let shift = held_modifiers.contains(&VK_SHIFT)
        || held_modifiers.contains(&VK_LSHIFT)
        || held_modifiers.contains(&VK_RSHIFT);
    let win = held_modifiers.contains(&VK_LWIN) || held_modifiers.contains(&VK_RWIN);
    (ctrl, alt, shift, win)
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam != 0 {
        let info = &*(lparam as *const KBDLLHOOKSTRUCT);
        if (info.flags & LLKHF_INJECTED) != 0 {
            return CallNextHookEx(null_mut(), code, wparam, lparam);
        }

        let vk = info.vkCode as u16;
        let is_keydown = wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN;
        let is_keyup = wparam as u32 == WM_KEYUP || wparam as u32 == WM_SYSKEYUP;
        if is_modifier_vk(vk) {
            if let Ok(slot) = ACTIVE_SHARED.lock() {
                if let Some(shared) = slot.as_ref() {
                    if let Ok(mut shared) = shared.lock() {
                        if is_keydown {
                            shared.held_modifiers.insert(vk);
                        } else if is_keyup {
                            shared.held_modifiers.remove(&vk);
                        }
                    }
                }
            }
            // Keep modifier state consistent in Windows without leaking keystrokes.
            return CallNextHookEx(null_mut(), code, wparam, lparam);
        }

        if let Ok(slot) = ACTIVE_SHARED.lock() {
            if let Some(shared) = slot.as_ref() {
                if let Ok(mut shared) = shared.lock() {
                    if is_keydown {
                        if !shared.held_keys.insert(vk) {
                            return 1;
                        }
                    } else if is_keyup {
                        shared.held_keys.remove(&vk);
                    }
                }
            }
        }
        if is_keydown {
            if let Ok(guard_slot) = ACTIVE_SHARED.lock() {
                if let Some(shared_arc) = guard_slot.as_ref() {
                    if let Ok(mut shared) = shared_arc.lock() {
                        let (ctrl, alt, shift, win) =
                            modifier_flags_from_held(&shared.held_modifiers);
                        if vk == VK_ESCAPE && !ctrl && !alt && !shift && !win {
                            let now = Instant::now();
                            shared
                                .esc_presses
                                .retain(|t| now.duration_since(*t) <= Duration::from_secs(2));
                            shared.esc_presses.push(now);
                            if shared.esc_presses.len() >= 3 {
                                shared.esc_presses.clear();
                                PostMessageW(shared.daemon_hwnd as HWND, WM_APP_GUARD_UNLOCK, 0, 0);
                            }
                            return 1;
                        }

                        for (id, hk) in &shared.hotkeys {
                            if hk.matches(ctrl, alt, shift, win, vk) {
                                PostMessageW(
                                    shared.daemon_hwnd as HWND,
                                    WM_APP_GUARD_HOTKEY,
                                    *id as WPARAM,
                                    0,
                                );
                                return 1;
                            }
                        }
                    }
                }
            }
        }

        // Swallow all non-hotkey physical keys while locked off.
        return 1;
    }

    CallNextHookEx(null_mut(), code, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_modifiers_clear_after_physical_key_release() {
        let mut held =
            initial_modifiers(|vk| matches!(vk, VK_CONTROL | VK_LCONTROL | VK_MENU | VK_RMENU));
        assert_eq!(modifier_flags_from_held(&held), (true, true, false, false));
        held.remove(&VK_LCONTROL);
        held.remove(&VK_RMENU);
        assert_eq!(
            modifier_flags_from_held(&held),
            (false, false, false, false)
        );
    }

    #[test]
    fn modifier_flags_recognize_left_and_right_keys() {
        let mut held = HashSet::new();
        assert_eq!(
            modifier_flags_from_held(&held),
            (false, false, false, false)
        );
        held.insert(VK_RCONTROL);
        held.insert(VK_LMENU);
        assert_eq!(modifier_flags_from_held(&held), (true, true, false, false));
        held.insert(VK_RWIN);
        held.insert(VK_RSHIFT);
        assert_eq!(modifier_flags_from_held(&held), (true, true, true, true));
    }
}
