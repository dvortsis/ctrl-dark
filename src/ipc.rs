//! Named Pipe JSON IPC (`\\.\pipe\monitor-toggle-v1`) between `monitor-toggle` CLI and `monitor-toggled`.

use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};

use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use crate::config::WakeMode;
use crate::display::{DisplayInfo, TargetScope};
use crate::logging;
use crate::win_util::to_wide_null;

pub const PIPE_NAME: &str = r"\\.\pipe\monitor-toggle-v1";
const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
const ERROR_PIPE_CONNECTED: u32 = 535;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum IpcRequest {
    Toggle {
        scope: TargetScope,
    },
    Off {
        scope: TargetScope,
        wake_mode: Option<WakeMode>,
    },
    On,
    Recover {
        hard: bool,
    },
    Status,
    ListMonitors,
    SnapshotNow,
    SetWakeMode {
        mode: WakeMode,
    },
    ReloadConfig,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonStatus {
    pub pid: u32,
    pub state: String,
    pub wake_mode: WakeMode,
    pub toggle_all_hotkey: String,
    pub toggle_externals_hotkey: String,
    pub force_recover_hotkey: String,
    #[serde(default)]
    pub custom_hotkeys: Vec<String>,
    pub snapshot_monitors: usize,
    pub snapshot_captured_unix_ms: Option<u64>,
    pub active_monitors: usize,
    pub total_available_monitors: usize,
    pub disabled_monitors: Vec<String>,
    pub hotkey_warnings: Vec<String>,
    pub last_recovery_summary: Option<String>,
    #[serde(default)]
    pub ui_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IpcResponse {
    Ok {
        message: String,
        #[serde(default)]
        status: Option<Box<DaemonStatus>>,
        #[serde(default)]
        monitors: Option<Vec<DisplayInfo>>,
    },
    Error {
        message: String,
    },
}

const MAX_PIPE_INSTANCES: usize = 4;

struct PipeClientPermit(Arc<AtomicUsize>);
impl Drop for PipeClientPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Spawns the Named Pipe server loop on a dedicated background thread.
pub fn spawn_server<F>(stop_flag: Arc<AtomicBool>, handler: F) -> thread::JoinHandle<()>
where
    F: Fn(IpcRequest) -> IpcResponse + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    let active_clients = Arc::new(AtomicUsize::new(0));
    thread::Builder::new()
        .name("ipc-server".to_string())
        .spawn(move || {
            let pipe_w = to_wide_null(PIPE_NAME);
            while !stop_flag.load(Ordering::Relaxed) {
                while active_clients.load(Ordering::Acquire) >= MAX_PIPE_INSTANCES
                    && !stop_flag.load(Ordering::Relaxed)
                {
                    thread::sleep(Duration::from_millis(10));
                }
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }

                let handle: HANDLE = unsafe {
                    CreateNamedPipeW(
                        pipe_w.as_ptr(),
                        PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                        PIPE_TYPE_BYTE
                            | PIPE_READMODE_BYTE
                            | PIPE_WAIT
                            | PIPE_REJECT_REMOTE_CLIENTS,
                        MAX_PIPE_INSTANCES as u32,
                        65536,
                        65536,
                        1000,
                        null_mut(),
                    )
                };
                if handle == INVALID_HANDLE_VALUE || handle.is_null() {
                    let err = unsafe { GetLastError() };
                    if err != 231 {
                        // ERROR_PIPE_BUSY when max instances are briefly occupied
                        logging::error(format!("CreateNamedPipeW failed with error {err}"));
                    }
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }

                let connected = pipe_io(
                    handle,
                    Instant::now() + Duration::from_millis(250),
                    |ov| unsafe {
                        let ok = ConnectNamedPipe(handle, ov);
                        if ok == 0 && GetLastError() == ERROR_PIPE_CONNECTED {
                            1
                        } else {
                            ok
                        }
                    },
                );
                if connected.is_err() {
                    unsafe { CloseHandle(handle) };
                    continue;
                }

                if stop_flag.load(Ordering::Relaxed) {
                    unsafe {
                        DisconnectNamedPipe(handle);
                        CloseHandle(handle);
                    }
                    break;
                }

                active_clients.fetch_add(1, Ordering::AcqRel);
                let permit = PipeClientPermit(active_clients.clone());
                let handler_ref = handler.clone();
                let handle_raw = handle as usize;
                if thread::Builder::new()
                    .name("ipc-client".to_string())
                    .spawn(move || {
                        let _permit = permit;
                        let client_handle = handle_raw as HANDLE;
                        serve_single_client(client_handle, &*handler_ref);
                        unsafe {
                            DisconnectNamedPipe(client_handle);
                            CloseHandle(client_handle);
                        }
                    })
                    .is_err()
                {
                    // Drop the permit in this scope cleanly if thread spawn ever fails.
                    serve_single_client(handle, &*handler);
                    unsafe {
                        DisconnectNamedPipe(handle);
                        CloseHandle(handle);
                    }
                }
            }
        })
        .expect("spawn ipc-server thread")
}

const MAX_MESSAGE_BYTES: usize = 1_048_576;

/// Every operation owns an event and keeps its buffers alive through cancellation.
fn pipe_io(
    handle: HANDLE,
    deadline: Instant,
    start: impl FnOnce(*mut OVERLAPPED) -> i32,
) -> Result<u32, String> {
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if event.is_null() {
        return Err("Could not create IPC event".into());
    }
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event;
    let mut transferred = 0;
    let result = if start(&mut overlapped) != 0 {
        // ConnectNamedPipe may already be connected without a pending operation.
        unsafe {
            GetOverlappedResult(handle, &overlapped, &mut transferred, 0);
        }
        Ok(transferred)
    } else {
        let error = unsafe { GetLastError() };
        if error != ERROR_IO_PENDING {
            Err(format!("IPC operation failed (Win32 error {error})"))
        } else {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(u32::MAX as u128 - 1) as u32;
            if unsafe { WaitForSingleObject(event, remaining) } != WAIT_OBJECT_0 {
                unsafe {
                    CancelIoEx(handle, &overlapped);
                    // Cancellation is asynchronous: drain before dropping stack/buffers.
                    GetOverlappedResult(handle, &overlapped, &mut transferred, 1);
                }
                Err("IPC operation timed out".into())
            } else if unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 0) } == 0
            {
                Err(format!("IPC operation failed (Win32 error {})", unsafe {
                    GetLastError()
                }))
            } else {
                Ok(transferred)
            }
        }
    };
    unsafe {
        CloseHandle(event);
    }
    result
}

fn read_message(handle: HANDLE, deadline: Instant) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = pipe_io(handle, deadline, |ov| unsafe {
            ReadFile(
                handle,
                chunk.as_mut_ptr(),
                chunk.len() as u32,
                null_mut(),
                ov,
            )
        })? as usize;
        if n == 0 {
            return Err("IPC peer closed before sending a complete message".into());
        }
        if bytes.len() + n > MAX_MESSAGE_BYTES {
            return Err("IPC message exceeds 1 MiB".into());
        }
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(end) = bytes.iter().position(|&b| b == b'\n') {
            bytes.truncate(end);
            return Ok(bytes);
        }
        if Instant::now() >= deadline {
            return Err("IPC operation timed out".into());
        }
    }
}

fn write_message(handle: HANDLE, bytes: &[u8], deadline: Instant) -> Result<(), String> {
    let mut offset = 0;
    while offset < bytes.len() {
        let n = pipe_io(handle, deadline, |ov| unsafe {
            WriteFile(
                handle,
                bytes[offset..].as_ptr(),
                (bytes.len() - offset) as u32,
                null_mut(),
                ov,
            )
        })? as usize;
        if n == 0 {
            return Err("IPC write made no progress".into());
        }
        offset += n;
    }
    Ok(())
}

fn serve_single_client<F>(handle: HANDLE, handler: &F)
where
    F: Fn(IpcRequest) -> IpcResponse,
{
    let response = match read_message(handle, Instant::now() + Duration::from_secs(2)) {
        Ok(bytes) => match serde_json::from_slice::<IpcRequest>(&bytes) {
            Ok(req) => handler(req),
            Err(e) => IpcResponse::Error {
                message: format!("Invalid IPC request JSON: {e}"),
            },
        },
        Err(_) => return,
    };
    if let Ok(mut out) = serde_json::to_vec(&response) {
        out.push(b'\n');
        let deadline = Instant::now() + Duration::from_secs(2);
        if write_message(handle, &out, deadline).is_ok() {
            // Client acknowledges receipt. Unlike FlushFileBuffers, this wait is bounded.
            let _ = read_message(handle, deadline);
        }
    }
}

/// Sends a request with a deadline covering connection, write and response reads.
pub fn send_request(req: &IpcRequest, timeout_ms: u32) -> Result<IpcResponse, String> {
    send_request_to(PIPE_NAME, req, timeout_ms)
}

fn send_request_to(name: &str, req: &IpcRequest, timeout_ms: u32) -> Result<IpcResponse, String> {
    const ERROR_FILE_NOT_FOUND: u32 = 2;
    const ERROR_PIPE_BUSY: u32 = 231;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1) as u64);
    let pipe_w = to_wide_null(name);
    let handle = loop {
        let remaining_ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(u32::MAX as u128 - 1) as u32;
        if remaining_ms > 0 {
            let _ = unsafe { WaitNamedPipeW(pipe_w.as_ptr(), remaining_ms) };
        }
        let h = unsafe {
            CreateFileW(
                pipe_w.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                0,
                null_mut(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE && !h.is_null() {
            break h;
        }
        let err = unsafe { GetLastError() };
        if Instant::now() >= deadline {
            return Err(format!(
                "Daemon is not running or pipe unavailable (Win32 error {err})"
            ));
        }
        if err == ERROR_PIPE_BUSY {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        if err == ERROR_FILE_NOT_FOUND {
            thread::sleep(Duration::from_millis(15));
            continue;
        }
        return Err(format!(
            "Daemon is not running or pipe unavailable (Win32 error {err})"
        ));
    };
    let result = (|| {
        let mut payload = serde_json::to_vec(req).map_err(|e| e.to_string())?;
        payload.push(b'\n');
        write_message(handle, &payload, deadline)?;
        let response = read_message(handle, deadline)?;
        let _ = write_message(handle, b"\n", deadline);
        serde_json::from_slice::<IpcResponse>(&response)
            .map_err(|e| format!("Invalid IPC response JSON: {e}"))
    })();
    unsafe {
        CloseHandle(handle);
    }
    result
}

/// Sends a dummy connect/disconnect wake-up so a blocking `ConnectNamedPipe` exits on shutdown.
pub fn poke_pipe_for_shutdown() {
    let pipe_w = to_wide_null(PIPE_NAME);
    let handle = unsafe {
        CreateFileW(
            pipe_w.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            0,
            null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle != INVALID_HANDLE_VALUE && !handle.is_null() {
        unsafe {
            CloseHandle(handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_server(
        handler: impl FnOnce(HANDLE) + Send + 'static,
    ) -> (String, thread::JoinHandle<()>) {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let name = format!(
            r"\\.\pipe\monitor-toggle-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let server_name = name.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let join = thread::spawn(move || {
            let wide = to_wide_null(&server_name);
            let handle = unsafe {
                CreateNamedPipeW(
                    wide.as_ptr(),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    65536,
                    65536,
                    1000,
                    null_mut(),
                )
            };
            assert_ne!(handle, INVALID_HANDLE_VALUE);
            ready_tx.send(()).unwrap();
            pipe_io(
                handle,
                Instant::now() + Duration::from_secs(2),
                |ov| unsafe {
                    let ok = ConnectNamedPipe(handle, ov);
                    if ok == 0 && GetLastError() == ERROR_PIPE_CONNECTED {
                        1
                    } else {
                        ok
                    }
                },
            )
            .unwrap();
            handler(handle);
            unsafe {
                DisconnectNamedPipe(handle);
                CloseHandle(handle);
            }
        });
        ready_rx.recv().unwrap();
        (name, join)
    }

    #[test]
    fn named_pipe_round_trip_and_acknowledgement() {
        let (name, server) = test_server(|handle| {
            serve_single_client(handle, &|request| {
                assert_eq!(request, IpcRequest::Status);
                IpcResponse::Ok {
                    message: "alive".into(),
                    status: None,
                    monitors: None,
                }
            });
        });
        assert!(
            matches!(send_request_to(&name, &IpcRequest::Status, 1000).unwrap(), IpcResponse::Ok { message, .. } if message == "alive")
        );
        server.join().unwrap();
    }

    #[test]
    fn stalled_server_cannot_exceed_client_read_deadline() {
        let (name, server) = test_server(|handle| {
            read_message(handle, Instant::now() + Duration::from_secs(1)).unwrap();
            thread::sleep(Duration::from_millis(250));
        });
        let start = Instant::now();
        assert!(send_request_to(&name, &IpcRequest::Status, 50)
            .unwrap_err()
            .contains("timed out"));
        assert!(start.elapsed() < Duration::from_millis(200));
        server.join().unwrap();
    }

    #[test]
    fn ipc_round_trips_requests_and_responses() {
        let req = IpcRequest::Off {
            scope: TargetScope::ExternalsOnly,
            wake_mode: Some(WakeMode::HotkeyOnly),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: IpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);

        let resp = IpcResponse::Ok {
            message: "ok".to_string(),
            status: None,
            monitors: None,
        };
        let rjson = serde_json::to_string(&resp).unwrap();
        let rback: IpcResponse = serde_json::from_str(&rjson).unwrap();
        assert_eq!(resp, rback);
    }

    #[test]
    fn concurrent_clients_succeed_against_multi_instance_pipe() {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let pipe_name = format!(
            r"\\.\pipe\monitor-toggle-multi-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let server_pipe = pipe_name.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let wide = to_wide_null(&server_pipe);
            let mut joins = Vec::new();
            for _ in 0..2 {
                let handle = unsafe {
                    CreateNamedPipeW(
                        wide.as_ptr(),
                        PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                        PIPE_TYPE_BYTE
                            | PIPE_READMODE_BYTE
                            | PIPE_WAIT
                            | PIPE_REJECT_REMOTE_CLIENTS,
                        2,
                        65536,
                        65536,
                        1000,
                        null_mut(),
                    )
                };
                assert_ne!(handle, INVALID_HANDLE_VALUE);
                let h_raw = handle as usize;
                let tx = ready_tx.clone();
                joins.push(thread::spawn(move || {
                    let h = h_raw as HANDLE;
                    tx.send(()).unwrap();
                    pipe_io(h, Instant::now() + Duration::from_secs(2), |ov| unsafe {
                        let ok = ConnectNamedPipe(h, ov);
                        if ok == 0 && GetLastError() == ERROR_PIPE_CONNECTED {
                            1
                        } else {
                            ok
                        }
                    })
                    .unwrap();
                    serve_single_client(h, &|_| IpcResponse::Ok {
                        message: "ok".into(),
                        status: None,
                        monitors: None,
                    });
                    unsafe {
                        DisconnectNamedPipe(h);
                        CloseHandle(h);
                    }
                }));
            }
            for j in joins {
                j.join().unwrap();
            }
        });
        ready_rx.recv().unwrap();
        ready_rx.recv().unwrap();
        let p1 = pipe_name.clone();
        let p2 = pipe_name;
        let c1 = thread::spawn(move || send_request_to(&p1, &IpcRequest::Status, 1000));
        let c2 = thread::spawn(move || send_request_to(&p2, &IpcRequest::Status, 1000));
        assert!(
            matches!(c1.join().unwrap().unwrap(), IpcResponse::Ok { message, .. } if message == "ok")
        );
        assert!(
            matches!(c2.join().unwrap().unwrap(), IpcResponse::Ok { message, .. } if message == "ok")
        );
        server.join().unwrap();
    }
}
