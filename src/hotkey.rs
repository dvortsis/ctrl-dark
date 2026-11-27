//! Hotkey parsing, Win32 global hotkey registration, and key-release debouncing.

use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN, VK_BACK, VK_CONTROL, VK_DELETE, VK_END, VK_F1, VK_F24, VK_HOME, VK_INSERT,
    VK_LWIN, VK_MENU, VK_NEXT, VK_PAUSE, VK_PRIOR, VK_RWIN, VK_SCROLL, VK_SHIFT, VK_SPACE, VK_TAB,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedHotkey {
    pub modifiers: u32,
    pub vk: u16,
    pub canonical: String,
}

impl ParsedHotkey {
    /// Parses strings like `"Ctrl+Alt+M"`, `"Win+Shift+F12"`, or `"Ctrl+Alt+Shift+M"`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let parts: Vec<&str> = spec
            .split('+')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            return Err("Empty hotkey string".to_string());
        }

        let mut mods = 0u32;
        let mut key_part: Option<&str> = None;

        for part in parts {
            let lower = part.to_ascii_lowercase();
            match lower.as_str() {
                "ctrl" | "control" => mods |= MOD_CONTROL,
                "alt" => mods |= MOD_ALT,
                "shift" => mods |= MOD_SHIFT,
                "win" | "windows" | "super" => mods |= MOD_WIN,
                _ => {
                    if key_part.is_some() {
                        return Err(format!("Multiple non-modifier keys in hotkey '{spec}'"));
                    }
                    key_part = Some(part);
                }
            }
        }

        let key_token =
            key_part.ok_or_else(|| format!("Hotkey '{spec}' is missing a trigger key"))?;
        let vk = parse_virtual_key(key_token)
            .ok_or_else(|| format!("Unsupported hotkey key '{key_token}' in '{spec}'"))?;

        let is_function_key = (VK_F1..=VK_F24).contains(&vk);
        if mods == 0 && !is_function_key {
            return Err(format!(
                "Hotkey '{spec}' must include at least one modifier (Ctrl/Alt/Shift/Win)"
            ));
        }

        let mut canonical_parts = Vec::new();
        if (mods & MOD_CONTROL) != 0 {
            canonical_parts.push("Ctrl".to_string());
        }
        if (mods & MOD_ALT) != 0 {
            canonical_parts.push("Alt".to_string());
        }
        if (mods & MOD_SHIFT) != 0 {
            canonical_parts.push("Shift".to_string());
        }
        if (mods & MOD_WIN) != 0 {
            canonical_parts.push("Win".to_string());
        }
        canonical_parts.push(canonical_key_name(vk));

        Ok(Self {
            modifiers: mods,
            vk,
            canonical: canonical_parts.join("+"),
        })
    }

    /// Returns true if the supplied modifier flags and virtual key match this hotkey.
    pub fn matches(&self, ctrl: bool, alt: bool, shift: bool, win: bool, vk: u16) -> bool {
        self.vk == vk
            && ((self.modifiers & MOD_CONTROL) != 0) == ctrl
            && ((self.modifiers & MOD_ALT) != 0) == alt
            && ((self.modifiers & MOD_SHIFT) != 0) == shift
            && ((self.modifiers & MOD_WIN) != 0) == win
    }
}

fn canonical_key_name(vk: u16) -> String {
    if (b'A' as u16..=b'Z' as u16).contains(&vk) || (b'0' as u16..=b'9' as u16).contains(&vk) {
        return (vk as u8 as char).to_string();
    }
    if (VK_F1..=VK_F24).contains(&vk) {
        return format!("F{}", vk - VK_F1 + 1);
    }
    match vk {
        VK_PAUSE => "Pause".to_string(),
        VK_SCROLL => "ScrollLock".to_string(),
        VK_SPACE => "Space".to_string(),
        VK_TAB => "Tab".to_string(),
        VK_HOME => "Home".to_string(),
        VK_END => "End".to_string(),
        VK_PRIOR => "PageUp".to_string(),
        VK_NEXT => "PageDown".to_string(),
        VK_INSERT => "Insert".to_string(),
        VK_DELETE => "Delete".to_string(),
        VK_BACK => "Backspace".to_string(),
        _ => format!("VK_{vk}"),
    }
}

fn parse_virtual_key(token: &str) -> Option<u16> {
    let upper = token.trim().to_ascii_uppercase();
    if upper.len() == 1 {
        let b = upper.as_bytes()[0];
        if b.is_ascii_alphabetic() || b.is_ascii_digit() {
            return Some(b as u16);
        }
    }
    if let Some(num_str) = upper.strip_prefix('F') {
        if let Ok(n) = num_str.parse::<u16>() {
            if (1..=24).contains(&n) {
                return Some(VK_F1 + (n - 1));
            }
        }
    }
    match upper.as_str() {
        "PAUSE" => Some(VK_PAUSE),
        "SCROLLLOCK" | "SCROLL" => Some(VK_SCROLL),
        "SPACE" => Some(VK_SPACE),
        "TAB" => Some(VK_TAB),
        "HOME" => Some(VK_HOME),
        "END" => Some(VK_END),
        "PAGEUP" | "PGUP" => Some(VK_PRIOR),
        "PAGEDOWN" | "PGDN" => Some(VK_NEXT),
        "INSERT" | "INS" => Some(VK_INSERT),
        "DELETE" | "DEL" => Some(VK_DELETE),
        "BACKSPACE" | "BACK" => Some(VK_BACK),
        _ => None,
    }
}

/// Registers a global hotkey with `MOD_NOREPEAT` on the given daemon window.
pub fn register_with_hwnd(hwnd_raw: usize, id: i32, hotkey: &ParsedHotkey) -> Result<(), String> {
    let ok = unsafe {
        RegisterHotKey(
            hwnd_raw as HWND,
            id,
            hotkey.modifiers | MOD_NOREPEAT,
            hotkey.vk as u32,
        )
    };
    if ok == 0 {
        let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(format!(
            "RegisterHotKey({}) failed with Win32 error {err} (likely already in use)",
            hotkey.canonical
        ));
    }
    Ok(())
}

/// Unregisters a previously registered global hotkey.
pub fn unregister_with_hwnd(hwnd_raw: usize, id: i32) {
    unsafe {
        UnregisterHotKey(hwnd_raw as HWND, id);
    }
}

fn is_vk_down(vk: u16) -> bool {
    (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
}

/// Waits until modifier keys and optional trigger key are released, then waits `settle_ms`
/// so physical key-up events do not immediately cancel `SC_MONITORPOWER = 2`.
pub fn wait_for_keys_released(trigger_vk: Option<u16>, max_wait_ms: u64, settle_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(max_wait_ms);
    while Instant::now() < deadline {
        let any_mod = is_vk_down(VK_CONTROL)
            || is_vk_down(VK_MENU)
            || is_vk_down(VK_SHIFT)
            || is_vk_down(VK_LWIN)
            || is_vk_down(VK_RWIN);
        let trigger_down = trigger_vk.map(is_vk_down).unwrap_or(false);
        if !any_mod && !trigger_down {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if settle_ms > 0 {
        thread::sleep(Duration::from_millis(settle_ms));
    }
}

/// Queries live modifier state using `GetAsyncKeyState`.
pub fn current_modifiers_pressed() -> (bool, bool, bool, bool) {
    (
        is_vk_down(VK_CONTROL),
        is_vk_down(VK_MENU),
        is_vk_down(VK_SHIFT),
        is_vk_down(VK_LWIN) || is_vk_down(VK_RWIN),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_hotkeys() {
        let hk = ParsedHotkey::parse("ctrl + alt + m").unwrap();
        assert_eq!(hk.modifiers, MOD_CONTROL | MOD_ALT);
        assert_eq!(hk.vk, b'M' as u16);
        assert_eq!(hk.canonical, "Ctrl+Alt+M");
        assert!(hk.matches(true, true, false, false, b'M' as u16));
        assert!(!hk.matches(true, true, true, false, b'M' as u16));
    }

    #[test]
    fn parses_function_and_win_keys() {
        let hk = ParsedHotkey::parse("Win+Shift+F12").unwrap();
        assert_eq!(hk.modifiers, MOD_WIN | MOD_SHIFT);
        assert_eq!(hk.vk, VK_F1 + 11);

        let f15 = ParsedHotkey::parse("F15").unwrap();
        assert_eq!(f15.modifiers, 0);
        assert_eq!(f15.vk, VK_F1 + 14);
    }

    #[test]
    fn canonicalizes_key_aliases_uniformly() {
        let a = ParsedHotkey::parse("ctrl+alt+pgup").unwrap();
        let b = ParsedHotkey::parse("Ctrl+Alt+PageUp").unwrap();
        assert_eq!(a.canonical, "Ctrl+Alt+PageUp");
        assert_eq!(a.canonical, b.canonical);
        assert_eq!(
            ParsedHotkey::parse("ctrl+space").unwrap().canonical,
            "Ctrl+Space"
        );
    }

    #[test]
    fn rejects_invalid_hotkeys() {
        assert!(ParsedHotkey::parse("").is_err());
        assert!(ParsedHotkey::parse("Ctrl+Alt").is_err());
        assert!(ParsedHotkey::parse("M").is_err());
        assert!(ParsedHotkey::parse("Ctrl+M+N").is_err());
        assert!(ParsedHotkey::parse("Ctrl+InvalidKey").is_err());
    }
}
