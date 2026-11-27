//! Desktop launcher orchestration, start-minimized policy, Windows `.ico` emblem generation,
//! and Start Menu / Desktop `.lnk` shortcut management.

use std::env;
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr::null_mut;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
};

use crate::autostart;
use crate::config::{self, AppConfig};
use crate::ipc::{send_request, IpcRequest, IpcResponse};
use crate::win_util::{from_wide_slice, to_wide_null, write_atomic};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchOptions {
    pub force_minimized: bool,
    pub force_open: bool,
    pub browser: bool,
    pub autostart: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchAction {
    StayInTray { notify: bool },
    OpenUi { browser: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShortcutStatus {
    pub start_menu: bool,
    pub desktop: bool,
    pub start_menu_path: String,
    pub desktop_path: String,
}

/// Parses CLI flags passed to `MonitorToggle.exe` or `monitor-toggle launch`.
pub fn parse_launcher_args<I, S>(args: I) -> LaunchOptions
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut opts = LaunchOptions::default();
    for raw in args {
        match raw.as_ref().trim().to_ascii_lowercase().as_str() {
            "--minimized" | "-m" | "--tray" => opts.force_minimized = true,
            "--open" | "--show" => opts.force_open = true,
            "--browser" => opts.browser = true,
            "--autostart" => opts.autostart = true,
            _ => {}
        }
    }
    opts
}

/// Determines whether launching should remain in the system tray or open the Control Panel UI.
pub fn decide_launch_action(
    opts: &LaunchOptions,
    daemon_was_running: bool,
    start_minimized_to_tray: bool,
) -> LaunchAction {
    if opts.force_minimized {
        return LaunchAction::StayInTray {
            notify: !opts.autostart && !daemon_was_running,
        };
    }
    if opts.force_open || opts.browser {
        return LaunchAction::OpenUi {
            browser: opts.browser,
        };
    }
    if opts.autostart {
        return if start_minimized_to_tray {
            LaunchAction::StayInTray { notify: false }
        } else {
            LaunchAction::OpenUi { browser: false }
        };
    }
    if daemon_was_running {
        LaunchAction::OpenUi { browser: false }
    } else if start_minimized_to_tray {
        LaunchAction::StayInTray { notify: true }
    } else {
        LaunchAction::OpenUi { browser: false }
    }
}

/// Resolves `CtrlDark.exe` adjacent to the current executable.
pub fn resolve_launcher_exe_path() -> Result<PathBuf, String> {
    let cur = env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    let dir = cur
        .parent()
        .ok_or_else(|| "Executable has no parent directory".to_string())?;
    Ok(dir.join("CtrlDark.exe"))
}

/// Ensures `monitor-toggled.exe` is running and returns `(was_already_running, ui_url)`.
pub fn ensure_daemon_running(notify_tray: bool) -> Result<(bool, Option<String>), String> {
    let mut was_running = false;
    if let Ok(IpcResponse::Ok {
        status: Some(st), ..
    }) = send_request(&IpcRequest::Status, 250)
    {
        if st.ui_url.is_some() {
            return Ok((true, st.ui_url));
        }
        was_running = true;
    }

    if !was_running {
        let daemon_exe = autostart::resolve_daemon_exe_path()?;
        if !daemon_exe.exists() {
            return Err(format!(
                "Could not find {} next to binary",
                daemon_exe.display()
            ));
        }

        let spawn_with_flags = |flags: u32| {
            let mut cmd = Command::new(&daemon_exe);
            if notify_tray {
                cmd.arg("--notify-tray");
            }
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(flags)
                .spawn()
        };

        spawn_with_flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB)
            .or_else(|_| spawn_with_flags(CREATE_NO_WINDOW | DETACHED_PROCESS))
            .map_err(|e| format!("Failed starting {}: {e}", daemon_exe.display()))?;
    }

    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        if let Ok(IpcResponse::Ok {
            status: Some(st), ..
        }) = send_request(&IpcRequest::Status, 300)
        {
            if st.ui_url.is_some() {
                return Ok((was_running, st.ui_url));
            }
        }
        thread::sleep(Duration::from_millis(60));
    }

    Err("Started monitor-toggled.exe, but Control Panel/IPC did not become ready within 4s".into())
}

/// Executes launcher behavior using the loaded config, daemon state, and CLI options.
pub fn execute_launch(opts: &LaunchOptions) -> Result<LaunchAction, String> {
    let cfg = AppConfig::load_or_create_default().unwrap_or_default();
    let (was_running, initial_url) = match send_request(&IpcRequest::Status, 250) {
        Ok(IpcResponse::Ok {
            status: Some(st), ..
        }) => (true, st.ui_url),
        _ => (false, None),
    };

    let action = decide_launch_action(opts, was_running, cfg.start_minimized_to_tray);
    let ui_url = match action {
        LaunchAction::StayInTray { notify } => {
            if !was_running {
                let (_, url) = ensure_daemon_running(notify)?;
                url
            } else {
                initial_url
            }
        }
        LaunchAction::OpenUi { browser } => {
            let url = if was_running && initial_url.is_some() {
                initial_url
            } else {
                let (_, url) = ensure_daemon_running(false)?;
                url
            };
            autostart::launch_ui_window(url.as_deref(), browser)?;
            url
        }
    };
    let _ = ui_url;
    Ok(action)
}

pub fn icon_file_path() -> PathBuf {
    config::data_dir().join("CtrlDark.ico")
}

/// Ensures `%LOCALAPPDATA%\monitor-toggle\CtrlDark.ico` exists and matches the embedded brand asset.
pub fn ensure_icon_file() -> Result<PathBuf, String> {
    let path = icon_file_path();
    let bytes = crate::tray::CTRL_DARK_ICO;
    let unchanged = fs::read(&path)
        .map(|existing| existing == bytes)
        .unwrap_or(false);
    if !unchanged {
        write_atomic(&path, bytes)?;
    }
    Ok(path)
}

pub fn start_menu_shortcut_path() -> PathBuf {
    let base = env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Ctrl + Dark.lnk")
}

fn expand_env_vars_with<F>(raw: &str, mut lookup: F) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    let s = raw.trim();
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find('%') {
            let var_name = &after[..end];
            if !var_name.is_empty() {
                if let Some(val) = lookup(var_name) {
                    out.push_str(&val);
                    rest = &after[end + 1..];
                    continue;
                }
            }
            out.push('%');
            out.push_str(var_name);
            out.push('%');
            rest = &after[end + 1..];
        } else {
            out.push_str(&rest[start..]);
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

fn expand_env_vars(raw: &str) -> String {
    expand_env_vars_with(raw, |name| {
        env::var(name).ok().or_else(|| {
            env::vars()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v)
        })
    })
}

fn query_user_shell_desktop() -> Option<PathBuf> {
    let subkey_w =
        to_wide_null(r"Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders");
    let val_w = to_wide_null("Desktop");
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
        return None;
    }

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
        unsafe { RegCloseKey(hkey) };
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
    unsafe { RegCloseKey(hkey) };
    if rc_read != ERROR_SUCCESS {
        return None;
    }
    let decoded = expand_env_vars(&from_wide_slice(&buf));
    if decoded.is_empty() {
        None
    } else {
        Some(PathBuf::from(decoded))
    }
}

pub fn desktop_shortcut_path() -> PathBuf {
    let dir = query_user_shell_desktop().unwrap_or_else(|| {
        env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Desktop")
    });
    dir.join("Ctrl + Dark.lnk")
}

/// Fast pure-Rust check of whether Start Menu and Desktop shortcuts currently exist.
pub fn shortcut_status() -> ShortcutStatus {
    let sm = start_menu_shortcut_path();
    let desk = desktop_shortcut_path();
    ShortcutStatus {
        start_menu: sm.exists(),
        desktop: desk.exists(),
        start_menu_path: sm.display().to_string(),
        desktop_path: desk.display().to_string(),
    }
}

fn write_windows_shortcut(
    lnk_path: &Path,
    target_exe: &Path,
    args: &str,
    icon_path: &Path,
) -> Result<(), String> {
    if let Some(parent) = lnk_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed creating {}: {e}", parent.display()))?;
    }
    let work_dir = target_exe
        .parent()
        .ok_or_else(|| "Target executable has no parent directory".to_string())?;

    let ps_script = r#"$s=(New-Object -ComObject WScript.Shell).CreateShortcut($env:MT_LNK_PATH);$s.TargetPath=$env:MT_TARGET;$s.Arguments=[string]$env:MT_ARGS;$s.WorkingDirectory=$env:MT_WORKDIR;$s.IconLocation="$($env:MT_ICON),0";$s.Description=$env:MT_DESC;$s.Save()"#;
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            ps_script,
        ])
        .env("MT_LNK_PATH", lnk_path)
        .env("MT_TARGET", target_exe)
        .env("MT_ARGS", args)
        .env("MT_WORKDIR", work_dir)
        .env("MT_ICON", icon_path)
        .env(
            "MT_DESC",
            "Ctrl + Dark — Turn off your monitors with a hotkey",
        )
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("Failed running powershell.exe for shortcut creation: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Failed creating shortcut {}: {}",
            lnk_path.display(),
            stderr.trim()
        ));
    }
    Ok(())
}

/// Installs Start Menu and/or Desktop `.lnk` shortcuts pointing to `MonitorToggle.exe`.
pub fn install_shortcuts(
    start_menu: bool,
    desktop: bool,
    minimized_arg: bool,
) -> Result<Vec<PathBuf>, String> {
    if !start_menu && !desktop {
        return Err("Select at least one shortcut location (Start Menu or Desktop)".into());
    }
    let target = resolve_launcher_exe_path()?;
    if !target.exists() {
        return Err(format!(
            "Launcher executable not found at {}",
            target.display()
        ));
    }
    let icon = ensure_icon_file()?;
    let args = if minimized_arg { "--minimized" } else { "" };

    let mut installed = Vec::new();
    if start_menu {
        let path = start_menu_shortcut_path();
        write_windows_shortcut(&path, &target, args, &icon)?;
        installed.push(path);
    }
    if desktop {
        let path = desktop_shortcut_path();
        write_windows_shortcut(&path, &target, args, &icon)?;
        installed.push(path);
    }
    Ok(installed)
}

/// Removes Start Menu and/or Desktop shortcuts if present, returning how many were deleted.
pub fn remove_shortcuts(start_menu: bool, desktop: bool) -> Result<usize, String> {
    if !start_menu && !desktop {
        return Err("Select at least one shortcut location (Start Menu or Desktop)".into());
    }
    let mut removed = 0usize;
    for (enabled, path) in [
        (start_menu, start_menu_shortcut_path()),
        (desktop, desktop_shortcut_path()),
    ] {
        if enabled && path.exists() {
            fs::remove_file(&path)
                .map_err(|e| format!("Failed removing {}: {e}", path.display()))?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_launcher_flags_case_insensitively() {
        let opts = parse_launcher_args(["MonitorToggle.exe", "--Minimized", "--Browser"]);
        assert!(opts.force_minimized);
        assert!(opts.browser);
        assert!(!opts.force_open);
        assert!(!opts.autostart);

        let opts2 = parse_launcher_args(["--autostart", "--open"]);
        assert!(opts2.autostart);
        assert!(opts2.force_open);
    }

    #[test]
    fn launch_action_matrix_respects_cold_warm_and_overrides() {
        let def = LaunchOptions::default();
        // Cold manual start with start_minimized=true -> tray + balloon notification
        assert_eq!(
            decide_launch_action(&def, false, true),
            LaunchAction::StayInTray { notify: true }
        );
        // Second click while already running -> opens Control Panel window
        assert_eq!(
            decide_launch_action(&def, true, true),
            LaunchAction::OpenUi { browser: false }
        );
        // Cold manual start with start_minimized=false -> opens Control Panel window
        assert_eq!(
            decide_launch_action(&def, false, false),
            LaunchAction::OpenUi { browser: false }
        );
        // Explicit --minimized always stays in tray (notifies only when cold manual launch)
        let min = parse_launcher_args(["-m"]);
        assert_eq!(
            decide_launch_action(&min, false, false),
            LaunchAction::StayInTray { notify: true }
        );
        assert_eq!(
            decide_launch_action(&min, true, false),
            LaunchAction::StayInTray { notify: false }
        );
        // Autostart stays quiet in tray when minimized=true, or opens UI when false
        let auto = parse_launcher_args(["--autostart"]);
        assert_eq!(
            decide_launch_action(&auto, false, true),
            LaunchAction::StayInTray { notify: false }
        );
        assert_eq!(
            decide_launch_action(&auto, false, false),
            LaunchAction::OpenUi { browser: false }
        );
        // --open / --browser override start_minimized
        let open = parse_launcher_args(["--open"]);
        assert_eq!(
            decide_launch_action(&open, false, true),
            LaunchAction::OpenUi { browser: false }
        );
        let br = parse_launcher_args(["--browser"]);
        assert_eq!(
            decide_launch_action(&br, false, true),
            LaunchAction::OpenUi { browser: true }
        );
    }

    #[test]
    fn embedded_brand_ico_contains_all_expected_sizes() {
        let ico = crate::tray::CTRL_DARK_ICO;
        assert_eq!(&ico[0..6], &[0, 0, 1, 0, 10, 0]);
        for px in [16u32, 20, 24, 32, 40, 48, 64, 96, 128, 256] {
            assert!(crate::tray::select_ico_frame(ico, px).is_some());
        }
    }

    #[test]
    fn expands_arbitrary_env_vars_case_insensitively() {
        let expanded = expand_env_vars_with(r"  %OneDrive%\Desktop\Sub ", |name| {
            if name.eq_ignore_ascii_case("onedrive") {
                Some(r"C:\Users\alice\OneDrive".to_string())
            } else {
                None
            }
        });
        assert_eq!(expanded, r"C:\Users\alice\OneDrive\Desktop\Sub");
    }
}
