//! Session-scoped Named Pipe JSON IPC between `monitor-toggle` CLI/UI/launcher and `monitor-toggled`.

use std::fmt;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_IO_PENDING, HANDLE, INVALID_HANDLE_VALUE, WAIT_ABANDONED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, ReleaseMutex, WaitForSingleObject,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use crate::config::WakeMode;
use crate::display::{DisplayInfo, TargetScope};
use crate::logging;
use crate::win_util::{from_wide_slice, to_wide_null};

const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;
const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;
const ERROR_PIPE_CONNECTED: u32 = 535;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PIPE_BUSY: u32 = 231;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_USER_CLASS: u32 = 1;
const SDDL_REVISION_1: u32 = 1;
const SYNCHRONIZE: u32 = 0x0010_0000;

const DAEMON_MUTEX_NAME: &str = r"Local\MonitorToggleDaemonMutexV1";
const DISPLAY_OP_MUTEX_NAME: &str = r"Local\MonitorToggleDisplayOpMutexV1";

#[repr(C)]
struct SidAndAttributes {
    sid: *mut std::ffi::c_void,
    attributes: u32,
}

#[repr(C)]
struct TokenUser {
    user: SidAndAttributes,
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> HANDLE;
    fn GetCurrentProcessId() -> u32;
    fn ProcessIdToSessionId(process_id: u32, session_id: *mut u32) -> i32;
    fn GetNamedPipeServerProcessId(pipe: HANDLE, server_process_id: *mut u32) -> i32;
    fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> HANDLE;
    fn OpenMutexW(desired_access: u32, inherit_handle: i32, name: *const u16) -> HANDLE;
    fn LocalFree(mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}

#[link(name = "advapi32")]
extern "system" {
    fn OpenProcessToken(
        process_handle: HANDLE,
        desired_access: u32,
        token_handle: *mut HANDLE,
    ) -> i32;
    fn GetTokenInformation(
        token_handle: HANDLE,
        token_information_class: u32,
        token_information: *mut std::ffi::c_void,
        token_information_length: u32,
        return_length: *mut u32,
    ) -> i32;
    fn ConvertSidToStringSidW(sid: *mut std::ffi::c_void, string_sid: *mut *mut u16) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string_security_descriptor: *const u16,
        string_sd_revision: u32,
        security_descriptor: *mut *mut std::ffi::c_void,
        security_descriptor_size: *mut u32,
    ) -> i32;
    fn EqualSid(sid1: *const std::ffi::c_void, sid2: *const std::ffi::c_void) -> i32;
}

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

struct LocalAllocPtr(*mut std::ffi::c_void);
impl Drop for LocalAllocPtr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
}

struct ProcessIdentity {
    session_id: u32,
    sid_buf: Vec<u8>,
    sid_string: String,
}

fn query_process_identity(process_handle: HANDLE, pid: u32) -> Result<ProcessIdentity, String> {
    let mut session_id = 0u32;
    if unsafe { ProcessIdToSessionId(pid, &mut session_id) } == 0 {
        return Err(format!(
            "ProcessIdToSessionId failed for PID {pid} (Win32 error {})",
            unsafe { GetLastError() }
        ));
    }

    let mut raw_token: HANDLE = null_mut();
    if unsafe { OpenProcessToken(process_handle, TOKEN_QUERY, &mut raw_token) } == 0 {
        return Err(format!(
            "OpenProcessToken failed for PID {pid} (Win32 error {})",
            unsafe { GetLastError() }
        ));
    }
    let _token = OwnedHandle(raw_token);

    let mut needed = 0u32;
    unsafe {
        GetTokenInformation(raw_token, TOKEN_USER_CLASS, null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        return Err(format!(
            "GetTokenInformation size failed (Win32 error {})",
            unsafe { GetLastError() }
        ));
    }

    // u64-aligned storage ensures TokenUser and the trailing SID match Win32 alignment rules.
    let word_len = (needed as usize).div_ceil(std::mem::size_of::<u64>());
    let mut words = vec![0u64; word_len];
    let ok = unsafe {
        GetTokenInformation(
            raw_token,
            TOKEN_USER_CLASS,
            words.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(format!(
            "GetTokenInformation failed (Win32 error {})",
            unsafe { GetLastError() }
        ));
    }

    let token_user = unsafe { &*(words.as_ptr() as *const TokenUser) };
    let sid_ptr = token_user.user.sid;
    let mut str_ptr: *mut u16 = null_mut();
    if unsafe { ConvertSidToStringSidW(sid_ptr, &mut str_ptr) } == 0 || str_ptr.is_null() {
        return Err(format!(
            "ConvertSidToStringSidW failed (Win32 error {})",
            unsafe { GetLastError() }
        ));
    }
    let _str_alloc = LocalAllocPtr(str_ptr.cast());
    let wide_slice = unsafe {
        let mut len = 0usize;
        while *str_ptr.add(len) != 0 {
            len += 1;
        }
        std::slice::from_raw_parts(str_ptr, len + 1)
    };
    let sid_string = from_wide_slice(wide_slice);
    let sid_bytes = unsafe {
        let byte_ptr = words.as_ptr() as *const u8;
        let start = byte_ptr as usize;
        let offset = (sid_ptr as usize).saturating_sub(start);
        let len = (needed as usize).saturating_sub(offset);
        std::slice::from_raw_parts(sid_ptr as *const u8, len).to_vec()
    };

    Ok(ProcessIdentity {
        session_id,
        sid_buf: sid_bytes,
        sid_string,
    })
}

fn current_identity() -> Result<ProcessIdentity, String> {
    let pid = unsafe { GetCurrentProcessId() };
    let proc_handle = unsafe { GetCurrentProcess() };
    query_process_identity(proc_handle, pid)
}

/// Returns the current logon session + user SID scoped Named Pipe path.
pub fn pipe_name() -> &'static str {
    static PIPE_NAME_CELL: OnceLock<String> = OnceLock::new();
    PIPE_NAME_CELL.get_or_init(|| match current_identity() {
        Ok(id) => {
            let safe_sid: String = id
                .sid_string
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            format!(r"\\.\pipe\monitor-toggle-v1-s{}-{safe_sid}", id.session_id)
        }
        Err(_) => format!(r"\\.\pipe\monitor-toggle-v1-p{}", unsafe {
            GetCurrentProcessId()
        }),
    })
}

fn create_owner_only_security_descriptor() -> Result<LocalAllocPtr, String> {
    let id = current_identity()?;
    let sddl = format!("D:(A;;GA;;;{})", id.sid_string);
    let sddl_w = to_wide_null(&sddl);
    let mut sd_ptr: *mut std::ffi::c_void = null_mut();
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_w.as_ptr(),
            SDDL_REVISION_1,
            &mut sd_ptr,
            null_mut(),
        )
    };
    if ok == 0 || sd_ptr.is_null() {
        return Err(format!(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW failed (Win32 error {})",
            unsafe { GetLastError() }
        ));
    }
    Ok(LocalAllocPtr(sd_ptr))
}

fn verify_pipe_server_identity(pipe_handle: HANDLE) -> Result<(), String> {
    let mut server_pid = 0u32;
    if unsafe { GetNamedPipeServerProcessId(pipe_handle, &mut server_pid) } == 0 {
        return Err(format!(
            "Failed verifying IPC server PID (Win32 error {})",
            unsafe { GetLastError() }
        ));
    }

    let me = current_identity()?;
    if server_pid == unsafe { GetCurrentProcessId() } {
        return Ok(());
    }

    let server_proc = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, server_pid) };
    if server_proc.is_null() {
        return Err(format!(
            "Refusing IPC server PID {server_pid} (cannot query server identity, error {})",
            unsafe { GetLastError() }
        ));
    }
    let _proc_guard = OwnedHandle(server_proc);
    let server = query_process_identity(server_proc, server_pid)?;
    if server.session_id != me.session_id {
        return Err(format!(
            "Refusing IPC server in session {} (expected session {})",
            server.session_id, me.session_id
        ));
    }
    if unsafe { EqualSid(server.sid_buf.as_ptr().cast(), me.sid_buf.as_ptr().cast()) } == 0 {
        return Err("Refusing IPC server running under a different user account".into());
    }
    Ok(())
}

/// Validates that a Control Panel URL strictly targets `http://127.0.0.1:<port>/` with an
/// optional single hexadecimal query credential (`launch` or `token`) and no other URI components.
pub fn validate_loopback_ui_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '@' | '\\' | '#'))
        || trimmed.contains("..")
    {
        return Err("Rejected malformed or unsafe Control Panel URL".into());
    }

    let Some(after_scheme) = trimmed.strip_prefix("http://127.0.0.1:") else {
        return Err("Control Panel URL must use http://127.0.0.1:<port>/".into());
    };
    let Some((port_str, suffix)) = after_scheme.split_once('/') else {
        return Err("Control Panel URL must include '/' after loopback port".into());
    };
    let port: u16 = port_str
        .parse()
        .map_err(|_| "Control Panel URL has invalid port".to_string())?;
    if port == 0 {
        return Err("Control Panel URL port must be non-zero".into());
    }

    if !suffix.is_empty() {
        let Some(query) = suffix.strip_prefix('?') else {
            return Err("Control Panel URL must target root path '/'".into());
        };
        if query.contains('&') {
            return Err("Control Panel URL must not contain extra query parameters".into());
        }
        let val = query
            .strip_prefix("launch=")
            .or_else(|| query.strip_prefix("token="))
            .ok_or_else(|| "Control Panel URL contains unexpected query parameter".to_string())?;
        if val.is_empty() || val.len() > 128 || !val.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("Control Panel URL contains malformed launch credential".into());
        }
    }

    Ok(trimmed.to_string())
}

/// Returns true if the daemon's session-local instance mutex already exists.
pub fn is_daemon_mutex_active() -> bool {
    let name_w = to_wide_null(DAEMON_MUTEX_NAME);
    let h = unsafe { OpenMutexW(SYNCHRONIZE, 0, name_w.as_ptr()) };
    if !h.is_null() {
        unsafe { CloseHandle(h) };
        true
    } else {
        unsafe { GetLastError() == ERROR_ACCESS_DENIED }
    }
}

/// Cross-process mutex serializing live display topology mutations (`SetDisplayConfig` / snapshot save)
/// across the daemon worker and standalone CLI commands.
pub struct DisplayOpGuard(usize);

impl DisplayOpGuard {
    pub fn try_acquire(timeout_ms: u32) -> Result<Self, String> {
        let name_w = to_wide_null(DISPLAY_OP_MUTEX_NAME);
        let h = unsafe { CreateMutexW(std::ptr::null(), 0, name_w.as_ptr()) };
        if h.is_null() {
            return Err(format!(
                "CreateMutexW({DISPLAY_OP_MUTEX_NAME}) failed (Win32 error {})",
                unsafe { GetLastError() }
            ));
        }
        let wait = unsafe { WaitForSingleObject(h, timeout_ms) };
        if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
            Ok(Self(h as usize))
        } else {
            unsafe { CloseHandle(h) };
            if wait == WAIT_TIMEOUT {
                Err("Another display operation is already in progress; try again shortly.".into())
            } else {
                Err(format!(
                    "Failed waiting for display operation lock (Win32 error {})",
                    unsafe { GetLastError() }
                ))
            }
        }
    }
}

impl Drop for DisplayOpGuard {
    fn drop(&mut self) {
        let h = self.0 as HANDLE;
        if !h.is_null() {
            unsafe {
                ReleaseMutex(h);
                CloseHandle(h);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcError {
    NotRunning(String),
    TimedOut(String),
    Protocol(String),
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpcError::NotRunning(m) | IpcError::TimedOut(m) | IpcError::Protocol(m) => {
                f.write_str(m)
            }
        }
    }
}

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
    pub last_lock_error: Option<String>,
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

/// Spawns the session- & owner-scoped Named Pipe server loop on a dedicated background thread.
pub fn spawn_server<F>(stop_flag: Arc<AtomicBool>, handler: F) -> thread::JoinHandle<()>
where
    F: Fn(IpcRequest) -> IpcResponse + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    let active_clients = Arc::new(AtomicUsize::new(0));
    thread::Builder::new()
        .name("ipc-server".to_string())
        .spawn(move || {
            let pipe_w = to_wide_null(pipe_name());
            let sd_guard = match create_owner_only_security_descriptor() {
                Ok(sd) => Some(sd),
                Err(e) => {
                    logging::error(format!("Failed creating IPC security descriptor ({e}); aborting pipe server"));
                    return;
                }
            };
            let mut is_first_instance = true;
            while !stop_flag.load(Ordering::Relaxed) {
                while active_clients.load(Ordering::Acquire) >= MAX_PIPE_INSTANCES
                    && !stop_flag.load(Ordering::Relaxed)
                {
                    thread::sleep(Duration::from_millis(10));
                }
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }

                let sa = SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: sd_guard.as_ref().map_or(null_mut(), |g| g.0),
                    bInheritHandle: 0,
                };
                let open_mode = PIPE_ACCESS_DUPLEX
                    | FILE_FLAG_OVERLAPPED
                    | if is_first_instance {
                        FILE_FLAG_FIRST_PIPE_INSTANCE
                    } else {
                        0
                    };
                let handle: HANDLE = unsafe {
                    CreateNamedPipeW(
                        pipe_w.as_ptr(),
                        open_mode,
                        PIPE_TYPE_BYTE
                            | PIPE_READMODE_BYTE
                            | PIPE_WAIT
                            | PIPE_REJECT_REMOTE_CLIENTS,
                        MAX_PIPE_INSTANCES as u32,
                        65536,
                        65536,
                        1000,
                        &sa,
                    )
                };
                if handle == INVALID_HANDLE_VALUE || handle.is_null() {
                    let err = unsafe { GetLastError() };
                    if is_first_instance && err == ERROR_ACCESS_DENIED {
                        logging::error("Refusing to attach IPC server to existing untrusted pipe instance (ERROR_ACCESS_DENIED)");
                        break;
                    }
                    if err != ERROR_PIPE_BUSY {
                        logging::error(format!("CreateNamedPipeW failed with error {err}"));
                    }
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                is_first_instance = false;

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
    send_request_typed(req, timeout_ms).map_err(|e| e.to_string())
}

/// Sends a request while distinguishing daemon-absent errors from in-flight timeouts.
pub fn send_request_typed(req: &IpcRequest, timeout_ms: u32) -> Result<IpcResponse, IpcError> {
    send_request_to(pipe_name(), req, timeout_ms)
}

fn send_request_to(name: &str, req: &IpcRequest, timeout_ms: u32) -> Result<IpcResponse, IpcError> {
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
                FILE_ATTRIBUTE_NORMAL
                    | FILE_FLAG_OVERLAPPED
                    | SECURITY_SQOS_PRESENT
                    | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE && !h.is_null() {
            break h;
        }
        let err = unsafe { GetLastError() };
        if Instant::now() >= deadline {
            return if err == ERROR_FILE_NOT_FOUND {
                Err(IpcError::NotRunning(format!(
                    "Daemon is not running or pipe unavailable (Win32 error {err})"
                )))
            } else if err == ERROR_PIPE_BUSY {
                Err(IpcError::TimedOut(format!(
                    "IPC pipe busy past {timeout_ms}ms deadline (Win32 error {err})"
                )))
            } else {
                Err(IpcError::Protocol(format!(
                    "IPC pipe connection failed (Win32 error {err})"
                )))
            };
        }
        if err == ERROR_PIPE_BUSY {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        if err == ERROR_FILE_NOT_FOUND {
            thread::sleep(Duration::from_millis(15));
            continue;
        }
        return Err(IpcError::Protocol(format!(
            "Daemon IPC unavailable (Win32 error {err})"
        )));
    };
    let result = (|| -> Result<IpcResponse, IpcError> {
        verify_pipe_server_identity(handle).map_err(IpcError::Protocol)?;
        let mut payload = serde_json::to_vec(req).map_err(|e| IpcError::Protocol(e.to_string()))?;
        payload.push(b'\n');
        let map_io_err = |msg: String| -> IpcError {
            if msg.contains("timed out") {
                IpcError::TimedOut(msg)
            } else {
                IpcError::Protocol(msg)
            }
        };
        write_message(handle, &payload, deadline).map_err(map_io_err)?;
        let response = read_message(handle, deadline).map_err(map_io_err)?;
        let _ = write_message(handle, b"\n", deadline);
        serde_json::from_slice::<IpcResponse>(&response)
            .map_err(|e| IpcError::Protocol(format!("Invalid IPC response JSON: {e}")))
    })();
    unsafe {
        CloseHandle(handle);
    }
    result
}

/// Sends a dummy connect/disconnect wake-up so a blocking `ConnectNamedPipe` exits on shutdown.
pub fn poke_pipe_for_shutdown() {
    let pipe_w = to_wide_null(pipe_name());
    let handle = unsafe {
        CreateFileW(
            pipe_w.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            0,
            null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
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
        let err = send_request_to(&name, &IpcRequest::Status, 50).unwrap_err();
        assert!(matches!(err, IpcError::TimedOut(_)));
        assert!(err.to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_millis(200));
        server.join().unwrap();
    }

    #[test]
    fn ipc_error_distinguishes_not_running_from_timeout() {
        let missing_pipe = format!(
            r"\\.\pipe\monitor-toggle-nonexistent-{}",
            std::process::id()
        );
        let err = send_request_to(&missing_pipe, &IpcRequest::Status, 25).unwrap_err();
        assert!(matches!(err, IpcError::NotRunning(_)));
    }

    #[test]
    fn validate_loopback_ui_url_enforces_strict_allowlist() {
        assert!(validate_loopback_ui_url("http://127.0.0.1:49152/").is_ok());
        assert!(validate_loopback_ui_url("http://127.0.0.1:49152/?launch=0123abcd").is_ok());
        assert!(validate_loopback_ui_url("http://127.0.0.1:49152/?token=deadbeef").is_ok());

        for bad in [
            "",
            "https://example.com/",
            "http://localhost:49152/",
            "http://127.0.0.1:0/",
            "http://127.0.0.1:49152/evil",
            "http://127.0.0.1:49152/?launch=01&extra=2",
            "http://127.0.0.1:49152/?launch=not_hex!",
            "http://user@127.0.0.1:49152/",
            "file:///C:/Windows/System32/calc.exe",
            "calc.exe",
        ] {
            assert!(
                validate_loopback_ui_url(bad).is_err(),
                "expected URL rejection for {bad}"
            );
        }
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
