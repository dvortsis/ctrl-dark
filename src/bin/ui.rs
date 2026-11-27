#![cfg_attr(not(test), windows_subsystem = "windows")]

//! On-demand lightweight native WebView2 Control Panel window (`monitor-toggle-ui.exe`).
//! Exits completely when closed so `monitor-toggled.exe` maintains minimal resident RAM.

use std::ptr::null;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::windows::WindowExtWindows;
use tao::window::WindowBuilder;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, FindWindowW, SendMessageW, SetForegroundWindow, ShowWindow, HICON, ICON_BIG,
    ICON_SMALL, SW_RESTORE, WM_SETICON,
};
use wry::{WebContext, WebViewBuilder};

use monitor_toggle::autostart;
use monitor_toggle::config;
use monitor_toggle::ipc::{send_request, IpcRequest, IpcResponse};
use monitor_toggle::launcher;
use monitor_toggle::tray::{load_hicon_from_ico, CTRL_DARK_ICO};
use monitor_toggle::win_util::to_wide_null;

const WINDOW_TITLE: &str = "Ctrl + Dark — Control Panel";
const FOCUS_EVENT_NAME: &str = r"Local\MonitorToggleUiFocusEventV1";

#[derive(Debug)]
enum UiEvent {
    FocusAndRefresh(Option<String>),
}

fn main() {
    if std::env::args().skip(1).any(|a| {
        matches!(
            a.trim().to_ascii_lowercase().as_str(),
            "--minimized" | "-m" | "--tray"
        )
    }) {
        if let Err(err) = launcher::ensure_daemon_running(false) {
            eprintln!("error: {err}");
        }
        return;
    }

    let mutex_name = to_wide_null(r"Local\MonitorToggleUiMutexV1");
    let mutex = unsafe { CreateMutexW(null(), 1, mutex_name.as_ptr()) };
    if !mutex.is_null() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        signal_existing_ui_window();
        let title_w = to_wide_null(WINDOW_TITLE);
        let existing = unsafe { FindWindowW(null(), title_w.as_ptr()) };
        if !existing.is_null() {
            unsafe {
                ShowWindow(existing, SW_RESTORE);
                SetForegroundWindow(existing);
            }
        }
        unsafe { CloseHandle(mutex) };
        return;
    }

    let ui_url = match resolve_ui_url() {
        Ok(u) => u,
        Err(err) => {
            eprintln!("error: {err}");
            if !mutex.is_null() {
                unsafe { CloseHandle(mutex) };
            }
            return;
        }
    };

    if let Err(err) = run_webview_window(&ui_url) {
        eprintln!("WebView2 launch failed ({err}); opening in default browser.");
        let _ = autostart::launch_ui_window(Some(&ui_url), true);
    }

    if !mutex.is_null() {
        unsafe { CloseHandle(mutex) };
    }
}

fn signal_existing_ui_window() {
    let event_w = to_wide_null(FOCUS_EVENT_NAME);
    let evt: HANDLE = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, event_w.as_ptr()) };
    if !evt.is_null() {
        unsafe {
            SetEvent(evt);
            CloseHandle(evt);
        }
    }
}

fn resolve_ui_url() -> Result<String, String> {
    let (_, ui_url) = launcher::ensure_daemon_running(false)?;
    let raw = ui_url
        .ok_or_else(|| "Timed out waiting for monitor-toggled Control Panel URL".to_string())?;
    monitor_toggle::ipc::validate_loopback_ui_url(&raw)
}

fn query_daemon_ui_url() -> Option<String> {
    if let Ok(IpcResponse::Ok {
        status: Some(st), ..
    }) = send_request(&IpcRequest::Status, 600)
    {
        return st
            .ui_url
            .and_then(|u| monitor_toggle::ipc::validate_loopback_ui_url(&u).ok());
    }
    None
}

fn url_origin_prefix(validated_url: &str) -> Option<String> {
    let end = validated_url.find("/").and_then(|scheme_idx| {
        if validated_url.get(scheme_idx..scheme_idx + 2) == Some("//") {
            validated_url[scheme_idx + 2..]
                .find('/')
                .map(|rel| scheme_idx + 2 + rel + 1)
        } else {
            None
        }
    })?;
    Some(validated_url[..end].to_string())
}

fn run_webview_window(ui_url: &str) -> Result<(), String> {
    let event_loop = EventLoopBuilder::<UiEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title(WINDOW_TITLE)
        .with_inner_size(LogicalSize::new(1020.0, 740.0))
        .with_min_inner_size(LogicalSize::new(720.0, 520.0))
        .build(&event_loop)
        .map_err(|e| format!("Window creation failed: {e}"))?;

    let icon_small_raw = load_hicon_from_ico(CTRL_DARK_ICO, 16) as usize;
    let icon_big_raw = load_hicon_from_ico(CTRL_DARK_ICO, 32) as usize;
    let win_hwnd = window.hwnd() as HWND;
    unsafe {
        if icon_small_raw != 0 {
            SendMessageW(
                win_hwnd,
                WM_SETICON,
                ICON_SMALL as usize,
                icon_small_raw as isize,
            );
        }
        if icon_big_raw != 0 {
            SendMessageW(
                win_hwnd,
                WM_SETICON,
                ICON_BIG as usize,
                icon_big_raw as isize,
            );
        }
    }

    let webview_dir = config::data_dir().join("webview2");
    let _ = std::fs::create_dir_all(&webview_dir);
    let mut web_ctx = WebContext::new(Some(webview_dir));

    let allowed_origin = url_origin_prefix(ui_url)
        .ok_or_else(|| "Invalid Control Panel loopback origin".to_string())?;
    let nav_origin = allowed_origin.clone();
    let webview = WebViewBuilder::new_with_web_context(&mut web_ctx)
        .with_url(ui_url)
        .with_clipboard(true)
        .with_navigation_handler(move |uri| uri.starts_with(&nav_origin))
        .build(&window)
        .map_err(|e| format!("WebView2 build failed: {e}"))?;

    let stop_flag = Arc::new(AtomicBool::new(false));
    let event_w = to_wide_null(FOCUS_EVENT_NAME);
    let focus_event_raw = unsafe { CreateEventW(null(), 0, 0, event_w.as_ptr()) } as usize;
    let mut watcher_join = None;
    if focus_event_raw != 0 {
        let stop_bg = stop_flag.clone();
        watcher_join = thread::Builder::new()
            .name("ui-focus-watcher".to_string())
            .spawn(move || {
                while !stop_bg.load(Ordering::Relaxed) {
                    let wait = unsafe { WaitForSingleObject(focus_event_raw as HANDLE, 250) };
                    if stop_bg.load(Ordering::Relaxed) {
                        break;
                    }
                    if wait == WAIT_OBJECT_0 {
                        let maybe_url = query_daemon_ui_url();
                        if proxy
                            .send_event(UiEvent::FocusAndRefresh(maybe_url))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            })
            .ok();
    }

    let mut current_url = ui_url.to_string();
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UiEvent::FocusAndRefresh(maybe_url)) => {
                window.set_minimized(false);
                window.set_focus();
                if let Some(new_url) = maybe_url {
                    if let Ok(valid_new) = monitor_toggle::ipc::validate_loopback_ui_url(&new_url) {
                        if valid_new.starts_with(&allowed_origin) && valid_new != current_url {
                            current_url = valid_new.clone();
                            let _ = webview.load_url(&valid_new);
                        }
                    }
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                stop_flag.store(true, Ordering::Relaxed);
                if focus_event_raw != 0 {
                    unsafe {
                        SetEvent(focus_event_raw as HANDLE);
                    }
                }
                *control_flow = ControlFlow::Exit;
            }
            Event::LoopDestroyed => {
                stop_flag.store(true, Ordering::Relaxed);
                if focus_event_raw != 0 {
                    unsafe {
                        SetEvent(focus_event_raw as HANDLE);
                    }
                }
                if let Some(join) = watcher_join.take() {
                    let _ = join.join();
                }
                if focus_event_raw != 0 {
                    unsafe {
                        CloseHandle(focus_event_raw as HANDLE);
                    }
                }
                unsafe {
                    if icon_small_raw != 0 {
                        DestroyIcon(icon_small_raw as HICON);
                    }
                    if icon_big_raw != 0 {
                        DestroyIcon(icon_big_raw as HICON);
                    }
                }
            }
            _ => {}
        }
    });
}
