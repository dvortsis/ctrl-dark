//! System tray icon (DPI- & taskbar-theme-aware Ctrl+Dark ICO assets) and dynamic context menu.

use std::mem::{size_of, zeroed};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HWND, POINT};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
    REG_DWORD,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos,
    GetSystemMetrics, PostMessageW, SetForegroundWindow, SetMenuDefaultItem, TrackPopupMenu, HICON,
    HMENU, LR_DEFAULTCOLOR, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MF_UNCHECKED,
    SM_CXSMICON, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTBUTTON, WM_APP, WM_NULL,
};

use crate::config::WakeMode;
use crate::display::DisplayInfo;
use crate::win_util::to_wide_null;

pub const CTRL_DARK_ICO: &[u8] = include_bytes!("../assets/icons/CtrlDark.ico");
const TRAY_DARK_ACTIVE_ICO: &[u8] = include_bytes!("../assets/tray/dark-active.ico");
const TRAY_LIGHT_ACTIVE_ICO: &[u8] = include_bytes!("../assets/tray/light-active.ico");

pub const WM_APP_TRAY: u32 = WM_APP + 1;
const TRAY_ICON_UID: u32 = 1;

pub const MENU_OPEN_UI: usize = 1000;
pub const MENU_TOGGLE_ALL: usize = 1001;
pub const MENU_TOGGLE_EXTERNALS: usize = 1002;
pub const MENU_FORCE_RECOVER: usize = 1003;
pub const MENU_TOGGLE_WAKE_MODE: usize = 1004;
pub const MENU_TOGGLE_AUTOSTART: usize = 1005;
pub const MENU_SNAPSHOT_NOW: usize = 1006;
pub const MENU_OPEN_CONFIG: usize = 1007;
pub const MENU_RELOAD_CONFIG: usize = 1008;
pub const MENU_EXIT: usize = 1009;
pub const MENU_TOGGLE_START_MINIMIZED: usize = 1010;
pub const MENU_MONITOR_BASE: usize = 2000;
pub const MENU_RULE_BASE: usize = 3000;

pub struct TrayMenuData {
    pub hwnd_raw: usize,
    pub displays: Vec<DisplayInfo>,
    pub wake_mode: WakeMode,
    pub autostart_on: bool,
    pub start_minimized_to_tray: bool,
    pub toggle_all_hk: String,
    pub toggle_ext_hk: String,
    pub recover_hk: String,
    /// `(label, hotkey, is_currently_active)`
    pub custom_rules: Vec<(String, String, bool)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayVisualState {
    Active,
    OffOrRecovering,
}

pub struct TrayManager {
    hwnd: usize,
    icon_active_dark: usize,
    icon_active_light: usize,
    icon_off: usize,
    added: bool,
    last_state: TrayVisualState,
    last_tooltip: String,
}

impl TrayManager {
    pub fn new(hwnd_raw: usize, tooltip: &str) -> Self {
        let desired_px = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
        let icon_active_dark = load_hicon_from_ico(TRAY_DARK_ACTIVE_ICO, desired_px) as usize;
        let icon_active_light = load_hicon_from_ico(TRAY_LIGHT_ACTIVE_ICO, desired_px) as usize;
        let icon_off = load_hicon_from_ico(CTRL_DARK_ICO, desired_px) as usize;
        let mut mgr = Self {
            hwnd: hwnd_raw,
            icon_active_dark,
            icon_active_light,
            icon_off,
            added: false,
            last_state: TrayVisualState::Active,
            last_tooltip: tooltip.to_string(),
        };
        mgr.add_or_update(TrayVisualState::Active, tooltip);
        mgr
    }

    fn current_icon(&self, state: TrayVisualState) -> HICON {
        match state {
            TrayVisualState::Active => {
                if taskbar_uses_light_theme() {
                    self.icon_active_light as HICON
                } else {
                    self.icon_active_dark as HICON
                }
            }
            TrayVisualState::OffOrRecovering => self.icon_off as HICON,
        }
    }

    pub fn add_or_update(&mut self, state: TrayVisualState, tooltip: &str) {
        self.last_state = state;
        self.last_tooltip = tooltip.to_string();

        let mut nid: NOTIFYICONDATAW = unsafe { zeroed() };
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.hwnd as HWND;
        nid.uID = TRAY_ICON_UID;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_APP_TRAY;
        nid.hIcon = self.current_icon(state);
        write_wide_fixed(&mut nid.szTip, tooltip);

        let msg = if self.added { NIM_MODIFY } else { NIM_ADD };
        let ok = unsafe { Shell_NotifyIconW(msg, &nid) };
        if ok != 0 {
            self.added = true;
        }
    }

    pub fn refresh_theme_icon(&mut self) {
        let state = self.last_state;
        let tooltip = self.last_tooltip.clone();
        self.add_or_update(state, &tooltip);
    }

    pub fn show_balloon(&mut self, title: &str, body: &str) {
        let mut nid: NOTIFYICONDATAW = unsafe { zeroed() };
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.hwnd as HWND;
        nid.uID = TRAY_ICON_UID;
        nid.uFlags = if self.added {
            NIF_INFO
        } else {
            nid.hIcon = self.current_icon(self.last_state);
            nid.uCallbackMessage = WM_APP_TRAY;
            NIF_MESSAGE | NIF_ICON | NIF_INFO
        };
        nid.dwInfoFlags = NIIF_INFO;
        write_wide_fixed(&mut nid.szInfoTitle, title);
        write_wide_fixed(&mut nid.szInfo, body);
        let msg = if self.added { NIM_MODIFY } else { NIM_ADD };
        let ok = unsafe { Shell_NotifyIconW(msg, &nid) };
        if ok != 0 {
            self.added = true;
        }
    }

    pub fn show_context_menu(&self, data: &TrayMenuData) {
        show_context_menu_for_hwnd(data);
    }
}

pub fn show_context_menu_for_hwnd(data: &TrayMenuData) {
    let hwnd = data.hwnd_raw as HWND;
    let hmenu: HMENU = unsafe { CreatePopupMenu() };
    if hmenu.is_null() {
        return;
    }

    append_item(hmenu, MF_STRING, MENU_OPEN_UI, "Open Control Panel...");
    unsafe {
        SetMenuDefaultItem(hmenu, MENU_OPEN_UI as u32, 0);
        AppendMenuW(hmenu, MF_SEPARATOR, 0, null_mut());
    }

    append_item(
        hmenu,
        MF_STRING,
        MENU_TOGGLE_ALL,
        &format_menu_item("Toggle All Monitors", &data.toggle_all_hk),
    );
    append_item(
        hmenu,
        MF_STRING,
        MENU_TOGGLE_EXTERNALS,
        &format_menu_item("Toggle External Monitors", &data.toggle_ext_hk),
    );
    for (idx, (label, hk, active)) in data.custom_rules.iter().enumerate() {
        let flags = MF_STRING | if *active { MF_CHECKED } else { MF_UNCHECKED };
        append_item(
            hmenu,
            flags,
            MENU_RULE_BASE + idx,
            &format_menu_item(label, hk),
        );
    }
    append_item(
        hmenu,
        MF_STRING,
        MENU_FORCE_RECOVER,
        &format_menu_item("Force Recover Displays", &data.recover_hk),
    );

    unsafe {
        AppendMenuW(hmenu, MF_SEPARATOR, 0, null_mut());
    }

    if data.displays.is_empty() {
        append_item(
            hmenu,
            MF_STRING | MF_GRAYED,
            0,
            "No active displays detected",
        );
    } else {
        for (idx, d) in data.displays.iter().enumerate() {
            if !d.available {
                let label = format!("Toggle {} [Offline]", d.friendly_name);
                append_item(hmenu, MF_STRING | MF_GRAYED, 0, &label);
            } else {
                let state_tag = if d.active { "ON" } else { "OFF" };
                let label = format!("Toggle {} [{}]", d.friendly_name, state_tag);
                let flags = MF_STRING | if d.active { MF_CHECKED } else { MF_UNCHECKED };
                append_item(hmenu, flags, MENU_MONITOR_BASE + idx, &label);
            }
        }
    }

    unsafe {
        AppendMenuW(hmenu, MF_SEPARATOR, 0, null_mut());
    }

    let lock_flags = MF_STRING
        | if data.wake_mode == WakeMode::HotkeyOnly {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        };
    append_item(
        hmenu,
        lock_flags,
        MENU_TOGGLE_WAKE_MODE,
        "Stay Off Until Hotkey (Ignore Mouse Bumps)",
    );

    let auto_flags = MF_STRING
        | if data.autostart_on {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        };
    append_item(
        hmenu,
        auto_flags,
        MENU_TOGGLE_AUTOSTART,
        "Run at Windows Sign-In",
    );

    let min_flags = MF_STRING
        | if data.start_minimized_to_tray {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        };
    append_item(
        hmenu,
        min_flags,
        MENU_TOGGLE_START_MINIMIZED,
        "Start Minimized to Tray",
    );

    unsafe {
        AppendMenuW(hmenu, MF_SEPARATOR, 0, null_mut());
    }
    append_item(
        hmenu,
        MF_STRING,
        MENU_SNAPSHOT_NOW,
        "Snapshot Current Layout",
    );
    append_item(hmenu, MF_STRING, MENU_OPEN_CONFIG, "Open config.toml");
    append_item(hmenu, MF_STRING, MENU_RELOAD_CONFIG, "Reload Config");
    unsafe {
        AppendMenuW(hmenu, MF_SEPARATOR, 0, null_mut());
    }
    append_item(hmenu, MF_STRING, MENU_EXIT, "Exit");

    let mut pt: POINT = unsafe { zeroed() };
    unsafe {
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        TrackPopupMenu(
            hmenu,
            TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            0,
            hwnd,
            null_mut(),
        );
        // Required per KB135788 so clicks outside the menu dismiss it cleanly.
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(hmenu);
    }
}

impl Drop for TrayManager {
    fn drop(&mut self) {
        if self.added {
            let mut nid: NOTIFYICONDATAW = unsafe { zeroed() };
            nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = self.hwnd as HWND;
            nid.uID = TRAY_ICON_UID;
            unsafe {
                Shell_NotifyIconW(NIM_DELETE, &nid);
            }
        }
        unsafe {
            for raw in [self.icon_active_dark, self.icon_active_light, self.icon_off] {
                if raw != 0 {
                    DestroyIcon(raw as HICON);
                }
            }
        }
    }
}

fn escape_menu_label(label: &str) -> String {
    label.replace('&', "&&")
}

fn format_menu_item(label: &str, hk: &str) -> String {
    let trimmed = hk.trim();
    if trimmed.is_empty() {
        label.to_string()
    } else {
        format!("{label}\t{trimmed}")
    }
}

fn append_item(hmenu: HMENU, flags: u32, id: usize, label: &str) {
    let escaped = escape_menu_label(label);
    let w = to_wide_null(&escaped);
    unsafe {
        AppendMenuW(hmenu, flags, id, w.as_ptr());
    }
}

fn write_wide_fixed<const N: usize>(dst: &mut [u16; N], s: &str) {
    dst.fill(0);
    for (slot, ch) in dst.iter_mut().take(N - 1).zip(s.encode_utf16()) {
        *slot = ch;
    }
}

/// Reads Windows taskbar light/dark theme (`SystemUsesLightTheme == 1` => light taskbar).
pub fn taskbar_uses_light_theme() -> bool {
    let subkey_w = to_wide_null(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let val_w = to_wide_null("SystemUsesLightTheme");
    let mut hkey: HKEY = null_mut();
    let rc = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey_w.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut hkey,
        )
    };
    if rc != ERROR_SUCCESS {
        return false;
    }

    let mut reg_type = 0u32;
    let mut val = 0u32;
    let mut bytes = size_of::<u32>() as u32;
    let rc_read = unsafe {
        RegQueryValueExW(
            hkey,
            val_w.as_ptr(),
            null_mut(),
            &mut reg_type,
            (&mut val as *mut u32).cast(),
            &mut bytes,
        )
    };
    unsafe { RegCloseKey(hkey) };
    rc_read == ERROR_SUCCESS && reg_type == REG_DWORD && val != 0
}

/// Parses an ICO container and returns the best matching PNG/DIB frame slice for `desired_px`.
pub fn select_ico_frame(ico: &[u8], desired_px: u32) -> Option<&[u8]> {
    if ico.len() < 6 {
        return None;
    }
    let reserved = u16::from_le_bytes([ico[0], ico[1]]);
    let ico_type = u16::from_le_bytes([ico[2], ico[3]]);
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    if reserved != 0 || ico_type != 1 || count == 0 {
        return None;
    }
    let dir_end = 6usize.checked_add(count.checked_mul(16)?)?;
    if ico.len() < dir_end {
        return None;
    }

    let mut best_ge: Option<(u32, &[u8])> = None;
    let mut largest: Option<(u32, &[u8])> = None;

    for i in 0..count {
        let base = 6 + i * 16;
        let width = match ico[base] {
            0 => 256u32,
            w => w as u32,
        };
        let len = u32::from_le_bytes(ico[base + 8..base + 12].try_into().ok()?) as usize;
        let offset = u32::from_le_bytes(ico[base + 12..base + 16].try_into().ok()?) as usize;
        let end = offset.checked_add(len)?;
        if end > ico.len() || len == 0 {
            continue;
        }
        let slice = &ico[offset..end];
        if width >= desired_px && best_ge.is_none_or(|(best_w, _)| width < best_w) {
            best_ge = Some((width, slice));
        }
        if largest.is_none_or(|(max_w, _)| width > max_w) {
            largest = Some((width, slice));
        }
    }

    best_ge.or(largest).map(|(_, slice)| slice)
}

/// Decodes the closest-size frame of an embedded `.ico` asset into a native `HICON`.
pub fn load_hicon_from_ico(ico: &[u8], desired_px: i32) -> HICON {
    let target = desired_px.max(16);
    let Some(frame) = select_ico_frame(ico, target as u32) else {
        return null_mut();
    };
    unsafe {
        CreateIconFromResourceEx(
            frame.as_ptr(),
            frame.len() as u32,
            1,
            0x0003_0000,
            target,
            target,
            LR_DEFAULTCOLOR,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_ampersands_and_omits_empty_hotkey_tabs() {
        assert_eq!(
            escape_menu_label("Only P27h-20 & J560T09"),
            "Only P27h-20 && J560T09"
        );
        assert_eq!(format_menu_item("Toggle All", ""), "Toggle All");
        assert_eq!(format_menu_item("Toggle All", "   "), "Toggle All");
        assert_eq!(
            format_menu_item("Toggle All", " Ctrl+Alt+M "),
            "Toggle All\tCtrl+Alt+M"
        );
    }

    #[test]
    fn embedded_ico_frames_select_and_decode_natively() {
        const PNG_MAGIC: &[u8; 4] = b"\x89PNG";
        for (asset, sizes) in [
            (CTRL_DARK_ICO, &[16u32, 20, 24, 32, 48, 256][..]),
            (TRAY_DARK_ACTIVE_ICO, &[16u32, 20, 24, 32, 48, 64][..]),
            (TRAY_LIGHT_ACTIVE_ICO, &[16u32, 20, 24, 32, 48, 64][..]),
        ] {
            for &px in sizes {
                let frame = select_ico_frame(asset, px).expect("ico frame");
                assert!(frame.starts_with(PNG_MAGIC), "expected PNG frame at {px}px");
                let icon = load_hicon_from_ico(asset, px as i32);
                assert!(!icon.is_null(), "CreateIconFromResourceEx failed at {px}px");
                unsafe { DestroyIcon(icon) };
            }
        }
        assert!(select_ico_frame(&[], 16).is_none());
        assert!(select_ico_frame(&[0, 0, 2, 0, 1, 0], 16).is_none());
    }
}
