//! Current-user Windows sign-in autostart management via HKCU Run registry key.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::win_util::{from_wide_slice, to_wide_null};

const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "CtrlDark";
const LEGACY_VALUE_NAME: &str = "MonitorToggle";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// Resolves `monitor-toggle-ui.exe` adjacent to the current executable.
pub fn resolve_ui_exe_path() -> Result<PathBuf, String> {
    let cur = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    let dir = cur
        .parent()
        .ok_or_else(|| "Executable has no parent directory".to_string())?;
    Ok(dir.join("monitor-toggle-ui.exe"))
}

/// Launches the Control Panel native WebView2 host (`monitor-toggle-ui.exe`), or falls
/// back to the user's default browser when `force_browser` is true or the UI binary is absent.
pub fn launch_ui_window(ui_url: Option<&str>, force_browser: bool) -> Result<(), String> {
    if !force_browser {
        if let Ok(ui_exe) = resolve_ui_exe_path() {
            if ui_exe.exists() {
                Command::new(&ui_exe)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
                    .spawn()
                    .map_err(|e| format!("Failed launching {}: {e}", ui_exe.display()))?;
                return Ok(());
            }
        }
    }

    let url = ui_url.ok_or_else(|| "Control Panel web URL is not yet available".to_string())?;
    let validated_url = crate::ipc::validate_loopback_ui_url(url)?;
    let op_w = to_wide_null("open");
    let url_w = to_wide_null(&validated_url);
    let rc = unsafe {
        ShellExecuteW(
            null_mut(),
            op_w.as_ptr(),
            url_w.as_ptr(),
            null_mut(),
            null_mut(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if rc <= 32 {
        return Err(format!(
            "ShellExecuteW failed opening Control Panel URL (code {rc})"
        ));
    }
    Ok(())
}

/// Resolves the `monitor-toggled.exe` path adjacent to the current executable.
pub fn resolve_daemon_exe_path() -> Result<PathBuf, String> {
    let cur = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    let dir = cur
        .parent()
        .ok_or_else(|| "Executable has no parent directory".to_string())?;
    let candidate = dir.join("monitor-toggled.exe");
    if candidate.exists() {
        Ok(candidate)
    } else if cur
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("monitor-toggled.exe"))
    {
        Ok(cur)
    } else {
        Ok(candidate)
    }
}

fn query_run_value(hkey: HKEY, name: &str) -> Option<String> {
    let val_w = to_wide_null(name);
    let mut reg_type = 0u32;
    let mut bytes = 0u32;
    let rc_size = unsafe {
        RegQueryValueExW(
            hkey,
            val_w.as_ptr(),
            null_mut(),
            &mut reg_type,
            null_mut(),
            &mut bytes,
        )
    };
    if rc_size != ERROR_SUCCESS || bytes == 0 {
        return None;
    }

    let mut buf = vec![0u16; (bytes as usize / 2) + 1];
    let rc_read = unsafe {
        RegQueryValueExW(
            hkey,
            val_w.as_ptr(),
            null_mut(),
            &mut reg_type,
            buf.as_mut_ptr() as *mut u8,
            &mut bytes,
        )
    };
    if rc_read != ERROR_SUCCESS {
        return None;
    }
    Some(from_wide_slice(&buf))
}

/// Checks whether autostart is registered in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
pub fn is_enabled() -> Result<Option<String>, String> {
    let subkey_w = to_wide_null(RUN_SUBKEY);
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
        return Err(format!("RegOpenKeyExW failed with error {rc}"));
    }

    let found =
        query_run_value(hkey, VALUE_NAME).or_else(|| query_run_value(hkey, LEGACY_VALUE_NAME));
    unsafe { RegCloseKey(hkey) };
    Ok(found)
}

fn resolve_autostart_command() -> Result<String, String> {
    if let Ok(launcher_path) = crate::launcher::resolve_launcher_exe_path() {
        if launcher_path.exists() {
            return Ok(format!("\"{}\" --autostart", launcher_path.display()));
        }
    }
    let daemon_path = resolve_daemon_exe_path()?;
    if !daemon_path.exists() {
        return Err(format!(
            "Daemon executable not found at {}",
            daemon_path.display()
        ));
    }
    Ok(format!("\"{}\"", daemon_path.display()))
}

/// Registers `CtrlDark.exe --autostart` (or falls back to `monitor-toggled.exe`)
/// to start at current-user Windows sign-in.
pub fn enable() -> Result<String, String> {
    let command = resolve_autostart_command()?;
    let subkey_w = to_wide_null(RUN_SUBKEY);
    let val_w = to_wide_null(VALUE_NAME);
    let legacy_w = to_wide_null(LEGACY_VALUE_NAME);
    let data_w = to_wide_null(&command);

    let mut hkey: HKEY = null_mut();
    let rc = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey_w.as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(format!("RegOpenKeyExW failed with error {rc}"));
    }

    let byte_len = (data_w.len() * std::mem::size_of::<u16>()) as u32;
    let rc_set = unsafe {
        RegSetValueExW(
            hkey,
            val_w.as_ptr(),
            0,
            REG_SZ,
            data_w.as_ptr() as *const u8,
            byte_len,
        )
    };
    if rc_set == ERROR_SUCCESS {
        let _ = unsafe { RegDeleteValueW(hkey, legacy_w.as_ptr()) };
    }
    unsafe { RegCloseKey(hkey) };
    if rc_set != ERROR_SUCCESS {
        return Err(format!("RegSetValueExW failed with error {rc_set}"));
    }
    Ok(command)
}

/// Removes the HKCU Run registration if present.
pub fn disable() -> Result<(), String> {
    let subkey_w = to_wide_null(RUN_SUBKEY);
    let mut hkey: HKEY = null_mut();
    let rc = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey_w.as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(format!("RegOpenKeyExW failed with error {rc}"));
    }
    for name in [VALUE_NAME, LEGACY_VALUE_NAME] {
        let val_w = to_wide_null(name);
        let rc_del = unsafe { RegDeleteValueW(hkey, val_w.as_ptr()) };
        if rc_del != ERROR_SUCCESS && rc_del != 2 {
            unsafe { RegCloseKey(hkey) };
            return Err(format!("RegDeleteValueW failed with error {rc_del}"));
        }
    }
    unsafe { RegCloseKey(hkey) };
    Ok(())
}
