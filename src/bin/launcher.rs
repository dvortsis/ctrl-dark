#![cfg_attr(not(test), windows_subsystem = "windows")]

//! Single-entry desktop launcher (`CtrlDark.exe`) for Ctrl + Dark supporting minimized-to-tray
//! cold start, sign-in autostart, and on-demand Control Panel opening.

use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

use monitor_toggle::launcher::{execute_launch, parse_launcher_args};
use monitor_toggle::win_util::to_wide_null;

fn main() {
    let opts = parse_launcher_args(std::env::args().skip(1));

    let mutex_name = to_wide_null(r"Local\MonitorToggleLauncherMutexV1");
    let mutex = unsafe { CreateMutexW(null(), 1, mutex_name.as_ptr()) };
    let already = !mutex.is_null() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already && opts.autostart {
        unsafe { CloseHandle(mutex) };
        return;
    }

    let owns_mutex = if mutex.is_null() {
        false
    } else if !already {
        true
    } else {
        let wait = unsafe { WaitForSingleObject(mutex, 4000) };
        wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED
    };

    if let Err(err) = execute_launch(&opts) {
        if !opts.autostart {
            let title_w = to_wide_null("Ctrl + Dark Launcher Error");
            let msg_w = to_wide_null(&err);
            unsafe {
                MessageBoxW(
                    null_mut(),
                    msg_w.as_ptr(),
                    title_w.as_ptr(),
                    MB_OK | MB_ICONERROR,
                );
            }
        }
    }

    if !mutex.is_null() {
        unsafe {
            if owns_mutex {
                ReleaseMutex(mutex);
            }
            CloseHandle(mutex);
        }
    }
}
