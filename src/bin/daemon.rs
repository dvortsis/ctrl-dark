#![cfg_attr(not(test), windows_subsystem = "windows")]

//! Background system-tray, global-hotkey, power-notification, and recovery daemon.

use std::mem::zeroed;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, RegisterClassW,
    RegisterWindowMessageW, TranslateMessage, MSG, WM_COMMAND, WM_DESTROY, WM_DEVICECHANGE,
    WM_DISPLAYCHANGE, WM_HOTKEY, WM_LBUTTONUP, WM_POWERBROADCAST, WM_RBUTTONUP, WM_SETTINGCHANGE,
    WM_TIMER, WNDCLASSW,
};

use monitor_toggle::autostart;
use monitor_toggle::config::{self, AppConfig};
use monitor_toggle::controller::{
    handle_ipc_request, Controller, WM_APP_IPC_ACTION, WM_APP_WORKER_DONE,
};
use monitor_toggle::input_guard::{WM_APP_GUARD_HOTKEY, WM_APP_GUARD_UNLOCK};
use monitor_toggle::ipc::{poke_pipe_for_shutdown, spawn_server};
use monitor_toggle::logging;
use monitor_toggle::tray::{show_context_menu_for_hwnd, WM_APP_TRAY};
use monitor_toggle::web_server::spawn_web_server;
use monitor_toggle::win_util::to_wide_null;

static CONTROLLER: OnceLock<Arc<Mutex<Controller>>> = OnceLock::new();
static TASKBAR_CREATED_MSG: OnceLock<u32> = OnceLock::new();

fn main() {
    let mutex_name = to_wide_null(r"Local\MonitorToggleDaemonMutexV1");
    let mutex = unsafe { CreateMutexW(null(), 1, mutex_name.as_ptr()) };
    if !mutex.is_null() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(mutex) };
        return;
    }

    logging::init(config::log_path());
    logging::info("Ctrl + Dark daemon (monitor-toggled) starting");

    let cfg = match AppConfig::load_or_create_default() {
        Ok(c) => c,
        Err(e) => {
            logging::error(format!("Config load error ({e}); using defaults"));
            AppConfig::default()
        }
    };

    let hinst: HINSTANCE = unsafe { GetModuleHandleW(null_mut()) };
    let class_w = to_wide_null("MonitorToggleDaemonWnd");
    let title_w = to_wide_null("Ctrl + Dark Daemon");

    let mut wc: WNDCLASSW = unsafe { zeroed() };
    wc.lpfnWndProc = Some(daemon_wndproc);
    wc.hInstance = hinst;
    wc.lpszClassName = class_w.as_ptr();
    unsafe {
        RegisterClassW(&wc);
    }

    let taskbar_msg_name = to_wide_null("TaskbarCreated");
    let _ = TASKBAR_CREATED_MSG.set(unsafe { RegisterWindowMessageW(taskbar_msg_name.as_ptr()) });

    let hwnd: HWND = unsafe {
        CreateWindowExW(
            0,
            class_w.as_ptr(),
            title_w.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            hinst,
            null_mut(),
        )
    };
    if hwnd.is_null() {
        logging::error("CreateWindowExW failed for daemon window");
        if !mutex.is_null() {
            unsafe { CloseHandle(mutex) };
        }
        return;
    }

    let ctrl_arc = Arc::new(Mutex::new(Controller::new(hwnd as usize, cfg)));
    let _ = CONTROLLER.set(ctrl_arc.clone());

    if std::env::args().any(|a| a.eq_ignore_ascii_case("--notify-tray")) {
        if let Ok(mut ctrl) = ctrl_arc.lock() {
            ctrl.show_tray_balloon(
                "Ctrl + Dark",
                "Running minimized in the system tray. Left-click the tray icon to open the Control Panel.",
            );
        }
    }

    let stop_ipc = Arc::new(AtomicBool::new(false));
    let web_handle = match spawn_web_server(ctrl_arc.clone(), stop_ipc.clone()) {
        Ok(h) => {
            if let Ok(mut ctrl) = ctrl_arc.lock() {
                ctrl.set_launch_minter(h.minter.clone());
            }
            logging::info(format!(
                "Control Panel HTTP API listening at 127.0.0.1:{}",
                h.port
            ));
            Some(h)
        }
        Err(e) => {
            logging::error(format!("Failed starting Control Panel HTTP server: {e}"));
            None
        }
    };

    let ipc_ctrl = ctrl_arc.clone();
    let ipc_join = spawn_server(stop_ipc.clone(), move |req| {
        handle_ipc_request(&ipc_ctrl, req)
    });

    let mut msg: MSG = unsafe { zeroed() };
    unsafe {
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    stop_ipc.store(true, Ordering::Relaxed);
    if let Some(h) = web_handle {
        h.poke_for_shutdown();
        let _ = h.join.join();
    }
    poke_pipe_for_shutdown();
    let _ = ipc_join.join();
    if let Some(worker_join) = ctrl_arc
        .lock()
        .ok()
        .and_then(|mut ctrl| ctrl.take_worker_join())
    {
        let _ = worker_join.join();
    }

    unsafe {
        DestroyWindow(hwnd);
        if !mutex.is_null() {
            CloseHandle(mutex);
        }
    }
    logging::info("monitor-toggled exited cleanly");
}

unsafe extern "system" fn daemon_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if let Some(&tb_msg) = TASKBAR_CREATED_MSG.get() {
        if tb_msg != 0 && msg == tb_msg {
            if let Some(ctrl_arc) = CONTROLLER.get() {
                if let Ok(mut ctrl) = ctrl_arc.lock() {
                    ctrl.reattach_tray();
                }
            }
            return 0;
        }
    }

    match msg {
        WM_HOTKEY => {
            if let Some(ctrl_arc) = CONTROLLER.get() {
                if let Ok(mut ctrl) = ctrl_arc.lock() {
                    ctrl.handle_hotkey(wparam as i32);
                }
            }
            0
        }
        WM_APP_GUARD_HOTKEY => {
            if let Some(ctrl_arc) = CONTROLLER.get() {
                if let Ok(mut ctrl) = ctrl_arc.lock() {
                    ctrl.handle_hotkey(wparam as i32);
                }
            }
            0
        }
        WM_APP_GUARD_UNLOCK => {
            logging::info("Emergency Esc x3 unlock triggered");
            if let Some(ctrl_arc) = CONTROLLER.get() {
                if let Ok(mut ctrl) = ctrl_arc.lock() {
                    ctrl.trigger_on();
                }
            }
            0
        }
        WM_APP_WORKER_DONE => {
            if let Some(ctrl_arc) = CONTROLLER.get() {
                if let Ok(mut ctrl) = ctrl_arc.lock() {
                    ctrl.on_worker_done();
                }
            }
            0
        }
        WM_APP_IPC_ACTION => {
            if let Some(ctrl_arc) = CONTROLLER.get() {
                if let Ok(mut ctrl) = ctrl_arc.lock() {
                    ctrl.drain_ui_actions();
                }
            }
            0
        }
        WM_POWERBROADCAST => {
            if let Some(ctrl_arc) = CONTROLLER.get() {
                if let Ok(mut ctrl) = ctrl_arc.lock() {
                    ctrl.on_power_broadcast(wparam, lparam);
                }
            }
            1
        }
        monitor_toggle::controller::WM_WTSSESSION_CHANGE => {
            if let Some(ctrl_arc) = CONTROLLER.get() {
                if let Ok(mut ctrl) = ctrl_arc.lock() {
                    ctrl.on_session_change(wparam);
                }
            }
            0
        }
        WM_DISPLAYCHANGE => {
            if let Some(ctrl_arc) = CONTROLLER.get() {
                if let Ok(mut ctrl) = ctrl_arc.lock() {
                    ctrl.on_display_change();
                }
            }
            0
        }
        WM_DEVICECHANGE => {
            if let Some(ctrl_arc) = CONTROLLER.get() {
                if let Ok(mut ctrl) = ctrl_arc.lock() {
                    ctrl.on_device_change(wparam);
                }
            }
            1
        }
        WM_TIMER => {
            if let Some(ctrl_arc) = CONTROLLER.get() {
                if let Ok(mut ctrl) = ctrl_arc.lock() {
                    ctrl.on_timer(wparam);
                }
            }
            0
        }
        WM_SETTINGCHANGE => {
            if let Some(ctrl_arc) = CONTROLLER.get() {
                if let Ok(mut ctrl) = ctrl_arc.lock() {
                    ctrl.refresh_tray_theme();
                }
            }
            0
        }
        WM_APP_TRAY => {
            let sub = lparam as u32;
            if sub == WM_RBUTTONUP {
                let menu_data = CONTROLLER.get().and_then(|arc| {
                    arc.lock()
                        .ok()
                        .map(|mut ctrl| ctrl.prepare_tray_menu_data())
                });
                if let Some(data) = menu_data {
                    show_context_menu_for_hwnd(&data);
                }
            } else if sub == WM_LBUTTONUP {
                let url = CONTROLLER
                    .get()
                    .and_then(|arc| arc.lock().ok().and_then(|ctrl| ctrl.mint_ui_url()));
                if let Err(e) = autostart::launch_ui_window(url.as_deref(), false) {
                    logging::warn(format!("Failed opening Control Panel from tray click: {e}"));
                }
            }
            0
        }
        WM_COMMAND => {
            let cmd_id = wparam & 0xFFFF;
            if let Some(ctrl_arc) = CONTROLLER.get() {
                if let Ok(mut ctrl) = ctrl_arc.lock() {
                    ctrl.handle_menu_command(cmd_id);
                }
            }
            0
        }
        WM_DESTROY => {
            if let Some(ctrl_arc) = CONTROLLER.get() {
                if let Ok(mut ctrl) = ctrl_arc.lock() {
                    ctrl.shutdown();
                }
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
