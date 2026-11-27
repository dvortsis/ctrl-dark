//! Lightweight `127.0.0.1` loopback HTTP server serving the embedded Control Panel SPA and JSON API.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::autostart;
use crate::config::{AppConfig, HotkeyRuleAction, MonitorHotkeyRule};
use crate::controller::{Controller, DaemonPhase, DeferredUiAction};
use crate::display::ccd::query_displays;
use crate::display::snapshot::{SavedMonitorMode, TopologySnapshot};
use crate::display::{is_scope_satisfied_in_selective_off, DisplayInfo, TargetScope};
use crate::hotkey::ParsedHotkey;
use crate::ipc::DaemonStatus;
use crate::launcher::{self, ShortcutStatus};
use crate::logging;
use crate::win_util::{decode_edid_manufacturer, extract_edid_token};

const INDEX_HTML: &str = include_str!("ui/index.html");
const STYLES_CSS: &str = include_str!("ui/styles.css");
const APP_JS: &str = include_str!("ui/app.js");
const THEME_JS: &str = include_str!("ui/theme.js");
const MASCOT_SVG: &str = include_str!("../assets/source/mascot-master.svg");
const FAVICON_ICO: &[u8] = include_bytes!("../assets/icons/CtrlDark.ico");

pub struct WebServerHandle {
    pub port: u16,
    pub token: String,
    pub ui_url: String,
    pub join: JoinHandle<()>,
}

impl WebServerHandle {
    /// Connects briefly to wake up the blocking `accept()` loop during daemon shutdown.
    pub fn poke_for_shutdown(&self) {
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StateResponse {
    pub status: DaemonStatus,
    pub monitors: Vec<DisplayInfo>,
    pub config: AppConfig,
    pub autostart: bool,
    pub active_rule_indices: Vec<usize>,
    pub shortcuts: ShortcutStatus,
}

#[derive(Debug, Deserialize)]
struct ValidateHotkeyPayload {
    hotkey: String,
}

#[derive(Debug, Serialize)]
struct ValidateHotkeyResponse {
    valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    canonical: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SaveConfigPayload {
    config: AppConfig,
    #[serde(default)]
    expected_config: Option<AppConfig>,
    #[serde(default)]
    autostart: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ActionPayload {
    action: String,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    expected_rule: Option<MonitorHotkeyRule>,
    #[serde(default)]
    stable_key: Option<String>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    hard: bool,
    #[serde(default)]
    off_only: bool,
    #[serde(default)]
    start_menu: bool,
    #[serde(default)]
    desktop: bool,
    #[serde(default)]
    minimized: bool,
}

#[derive(Debug, Serialize)]
struct ActionResponse {
    ok: bool,
    message: String,
    state: StateResponse,
}

#[derive(Debug, Serialize)]
struct ErrorPayload {
    error: String,
}

pub fn spawn_web_server(
    ctrl_arc: Arc<Mutex<Controller>>,
    stop_flag: Arc<AtomicBool>,
) -> Result<WebServerHandle, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("Failed binding Control Panel loopback HTTP socket: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Failed reading bound HTTP port: {e}"))?
        .port();
    let token = generate_session_token()?;
    let ui_url = format!("http://127.0.0.1:{port}/?token={token}");

    let tok = token.clone();
    let join = thread::Builder::new()
        .name("web-ui-server".to_string())
        .spawn(move || {
            let connections = Arc::new(AtomicUsize::new(0));
            for stream in listener.incoming() {
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(mut stream) = stream {
                    if connections.load(Ordering::Relaxed) >= 8 {
                        write_json_error(
                            &mut stream,
                            503,
                            "Too many concurrent Control Panel connections",
                        );
                        continue;
                    }
                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    let ctrl = ctrl_arc.clone();
                    let tok_req = tok.clone();
                    connections.fetch_add(1, Ordering::Relaxed);
                    let permit = ConnectionPermit(connections.clone());
                    thread::Builder::new()
                        .name("web-ui-conn".to_string())
                        .spawn(move || {
                            let _permit = permit;
                            handle_connection(stream, &ctrl, &tok_req);
                        })
                        .ok();
                }
            }
        })
        .map_err(|e| format!("Failed spawning web-ui-server thread: {e}"))?;

    Ok(WebServerHandle {
        port,
        token,
        ui_url,
        join,
    })
}

struct ConnectionPermit(Arc<AtomicUsize>);
impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn generate_session_token() -> Result<String, String> {
    use windows_sys::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    let mut bytes = [0u8; 32];
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(format!("Session token generation failed: {status}"));
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

struct HttpRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct DeadlineReader<'a> {
    stream: &'a TcpStream,
    deadline: Instant,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "HTTP request timed out",
            ));
        }
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(buf)
    }
}

fn read_request(reader: impl Read) -> Result<HttpRequest, String> {
    // Take bounds every allocation, including an unterminated request/header line.
    const MAX_HEADERS: u64 = 16 * 1024;
    const MAX_BODY: usize = 1024 * 1024;
    let mut reader = BufReader::new(reader).take(MAX_HEADERS);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    let parts: Vec<_> = line.split_whitespace().collect();
    if parts.len() != 3 || parts[2] != "HTTP/1.1" || !parts[1].starts_with('/') {
        return Err("Invalid HTTP request line".into());
    }
    let method = parts[0].to_string();
    let target = parts[1].to_string();
    let mut headers = HashMap::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 || !line.ends_with("\r\n") {
            return Err("Incomplete or oversized request headers".into());
        }
        if line == "\r\n" {
            break;
        }
        let (key, value) = line
            .trim_end()
            .split_once(':')
            .ok_or("Malformed HTTP header")?;
        let header_name = key.to_ascii_lowercase();
        let header_val = value.trim().to_string();
        if matches!(
            header_name.as_str(),
            "host"
                | "content-length"
                | "transfer-encoding"
                | "origin"
                | "x-mt-token"
                | "authorization"
        ) {
            if headers.insert(header_name, header_val).is_some() {
                return Err("Duplicate singleton HTTP header".into());
            }
        } else {
            headers
                .entry(header_name.clone())
                .and_modify(|existing| {
                    existing.push_str(if header_name == "cookie" { "; " } else { ", " });
                    existing.push_str(&header_val);
                })
                .or_insert(header_val);
        }
    }
    if headers.contains_key("transfer-encoding") {
        return Err("Transfer-Encoding is not supported".into());
    }
    let len = headers
        .get("content-length")
        .map(|v| v.parse::<usize>())
        .transpose()
        .map_err(|_| "Invalid Content-Length")?
        .unwrap_or(0);
    if len > MAX_BODY {
        return Err("Request body exceeds 1 MiB".into());
    }
    let mut body = vec![0; len];
    reader
        .into_inner()
        .read_exact(&mut body)
        .map_err(|_| "Incomplete request body")?;
    Ok(HttpRequest {
        method,
        target,
        headers,
        body,
    })
}

fn handle_connection(
    mut stream: TcpStream,
    ctrl_arc: &Arc<Mutex<Controller>>,
    session_token: &str,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let request = match read_request(DeadlineReader {
        stream: &stream,
        deadline: Instant::now() + Duration::from_secs(5),
    }) {
        Ok(request) => request,
        Err(message) => {
            write_json_error(&mut stream, 400, &message);
            return;
        }
    };
    let HttpRequest {
        method,
        target,
        headers,
        body,
    } = request;
    let authority = match stream.local_addr() {
        Ok(addr) => addr.to_string(),
        Err(_) => return,
    };
    if headers.get("host") != Some(&authority)
        || headers
            .get("origin")
            .is_some_and(|origin| origin != &format!("http://{authority}"))
    {
        write_json_error(
            &mut stream,
            403,
            "Only same-origin loopback requests are allowed",
        );
        return;
    }
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));

    let query_token = extract_query_param(query, "token");
    let cookie_token = headers
        .get("cookie")
        .and_then(|c| extract_cookie(c, "mt_csrf"));
    let header_token = headers.get("x-mt-token").map(String::as_str);

    let has_valid_page_token = query_token.as_deref() == Some(session_token)
        || cookie_token.as_deref() == Some(session_token);
    let has_valid_api_token = header_token == Some(session_token);

    match (method.as_str(), path) {
        ("GET", "/" | "/index.html") => {
            if !has_valid_page_token {
                write_response(
                    &mut stream,
                    403,
                    "text/plain; charset=utf-8",
                    b"Forbidden: missing or invalid session token. Launch via monitor-toggle-ui or tray icon.",
                    None,
                );
                return;
            }
            let injected = INDEX_HTML.replace("__MT_SESSION_TOKEN__", session_token);
            let cookie = format!("mt_csrf={session_token}; Path=/; SameSite=Strict; HttpOnly");
            write_response(
                &mut stream,
                200,
                "text/html; charset=utf-8",
                injected.as_bytes(),
                Some(&cookie),
            );
        }
        ("GET", "/styles.css") => {
            write_response(
                &mut stream,
                200,
                "text/css; charset=utf-8",
                STYLES_CSS.as_bytes(),
                None,
            );
        }
        ("GET", "/app.js") => {
            write_response(
                &mut stream,
                200,
                "application/javascript; charset=utf-8",
                APP_JS.as_bytes(),
                None,
            );
        }
        ("GET", "/theme.js") => {
            write_response(
                &mut stream,
                200,
                "application/javascript; charset=utf-8",
                THEME_JS.as_bytes(),
                None,
            );
        }
        ("GET", "/mascot.svg") => {
            write_response(
                &mut stream,
                200,
                "image/svg+xml; charset=utf-8",
                MASCOT_SVG.as_bytes(),
                None,
            );
        }
        ("GET", "/favicon.ico") => {
            write_response(&mut stream, 200, "image/x-icon", FAVICON_ICO, None);
        }
        ("GET", "/api/state") => {
            if !has_valid_api_token {
                write_json_error(&mut stream, 403, "Invalid or missing session token");
                return;
            }
            let state = collect_state(ctrl_arc);
            write_json(&mut stream, 200, &state);
        }
        ("POST", "/api/validate-hotkey") => {
            if !has_valid_api_token {
                write_json_error(&mut stream, 403, "Invalid or missing session token");
                return;
            }
            match serde_json::from_slice::<ValidateHotkeyPayload>(&body) {
                Ok(req) => {
                    let trimmed = req.hotkey.trim();
                    if trimmed.is_empty() {
                        write_json(
                            &mut stream,
                            200,
                            &ValidateHotkeyResponse {
                                valid: true,
                                canonical: Some(String::new()),
                                error: None,
                            },
                        );
                    } else {
                        match ParsedHotkey::parse(trimmed) {
                            Ok(parsed) => write_json(
                                &mut stream,
                                200,
                                &ValidateHotkeyResponse {
                                    valid: true,
                                    canonical: Some(parsed.canonical),
                                    error: None,
                                },
                            ),
                            Err(err) => write_json(
                                &mut stream,
                                200,
                                &ValidateHotkeyResponse {
                                    valid: false,
                                    canonical: None,
                                    error: Some(err),
                                },
                            ),
                        }
                    }
                }
                Err(e) => write_json_error(&mut stream, 400, &format!("Invalid JSON: {e}")),
            }
        }
        ("POST", "/api/config") => {
            if !has_valid_api_token {
                write_json_error(&mut stream, 403, "Invalid or missing session token");
                return;
            }
            match handle_save_config(ctrl_arc, &body) {
                Ok(state) => write_json(&mut stream, 200, &state),
                Err(msg) => write_json_error(&mut stream, 400, &msg),
            }
        }
        ("POST", "/api/action") => {
            if !has_valid_api_token {
                write_json_error(&mut stream, 403, "Invalid or missing session token");
                return;
            }
            match handle_action(ctrl_arc, &body) {
                Ok(resp) => write_json(&mut stream, 200, &resp),
                Err(msg) => write_json_error(&mut stream, 400, &msg),
            }
        }
        _ => write_json_error(&mut stream, 404, "Endpoint not found"),
    }
}

fn extract_query_param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == name {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn extract_cookie(cookie_hdr: &str, name: &str) -> Option<String> {
    for part in cookie_hdr.split(';') {
        if let Some((k, v)) = part.trim().split_once('=') {
            if k.trim() == name {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

pub fn collect_state(ctrl_arc: &Arc<Mutex<Controller>>) -> StateResponse {
    let live_ccd = query_displays(true).unwrap_or_default();
    let autostart_on = autostart::is_enabled().ok().flatten().is_some();

    let (status, cfg, snapshot, is_selective) = {
        let ctrl = ctrl_arc.lock().expect("controller lock");
        let mut cfg_norm = ctrl.config.clone();
        cfg_norm.hotkeys.monitor_hotkeys = cfg_norm
            .hotkeys
            .monitor_hotkeys
            .iter()
            .map(MonitorHotkeyRule::normalized_for_ui)
            .collect();
        (
            ctrl.build_status(),
            cfg_norm,
            ctrl.snapshot.clone(),
            ctrl.phase == DaemonPhase::SelectiveOff,
        )
    };

    let monitors = enrich_monitors(live_ccd, snapshot.as_ref());
    let active_rule_indices = if is_selective {
        cfg.hotkeys
            .monitor_hotkeys
            .iter()
            .enumerate()
            .filter_map(|(idx, r)| {
                let scope = r.to_scope().ok()?;
                if is_scope_satisfied_in_selective_off(&monitors, &scope) {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    StateResponse {
        status,
        monitors,
        config: cfg,
        autostart: autostart_on,
        active_rule_indices,
        shortcuts: launcher::shortcut_status(),
    }
}

fn saved_edid_token(m: &SavedMonitorMode) -> String {
    extract_edid_token(&m.stable_key).unwrap_or_else(|| {
        if m.edid_mfg != 0 || m.edid_product != 0 {
            let mfg = decode_edid_manufacturer(m.edid_mfg);
            if !mfg.is_empty() || m.edid_product != 0 {
                return format!("{mfg}{:04X}", m.edid_product);
            }
        }
        String::new()
    })
}

fn rects_overlap(
    pos_a: (i32, i32),
    res_a: (u32, u32),
    pos_b: (i32, i32),
    res_b: (u32, u32),
) -> bool {
    if res_a.0 == 0 || res_a.1 == 0 || res_b.0 == 0 || res_b.1 == 0 {
        return false;
    }
    let ax0 = pos_a.0 as i64;
    let ay0 = pos_a.1 as i64;
    let ax1 = ax0 + res_a.0 as i64;
    let ay1 = ay0 + res_a.1 as i64;
    let bx0 = pos_b.0 as i64;
    let by0 = pos_b.1 as i64;
    let bx1 = bx0 + res_b.0 as i64;
    let by1 = by0 + res_b.1 as i64;
    ax0 < bx1 && ax1 > bx0 && ay0 < by1 && ay1 > by0
}

fn translate_pos(pos: (i32, i32), delta: (i64, i64)) -> (i32, i32) {
    let x = (pos.0 as i64 + delta.0).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    let y = (pos.1 as i64 + delta.1).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    (x, y)
}

fn enrich_monitors(
    mut live: Vec<DisplayInfo>,
    snapshot: Option<&TopologySnapshot>,
) -> Vec<DisplayInfo> {
    let Some(snap) = snapshot else {
        return live;
    };

    let mut live_to_snap: Vec<Option<usize>> = vec![None; live.len()];
    let mut snap_used: Vec<bool> = vec![false; snap.monitors.len()];

    // Pass 1: exact stable_key matches.
    for (i, d) in live.iter().enumerate() {
        if d.stable_key.is_empty() {
            continue;
        }
        if let Some(j) = snap
            .monitors
            .iter()
            .enumerate()
            .find(|(j, m)| !snap_used[*j] && m.stable_key.eq_ignore_ascii_case(&d.stable_key))
            .map(|(j, _)| j)
        {
            live_to_snap[i] = Some(j);
            snap_used[j] = true;
        }
    }

    // Pass 2: unique EDID fallback when both unmatched sets agree on a single panel.
    let live_edids: Vec<String> = live.iter().map(|d| d.edid_code.clone()).collect();
    let snap_edids: Vec<String> = snap.monitors.iter().map(saved_edid_token).collect();
    for (i, code) in live_edids.iter().enumerate() {
        if live_to_snap[i].is_some() || code.is_empty() {
            continue;
        }
        let unmatched_live_same = live_edids
            .iter()
            .enumerate()
            .filter(|(idx, c)| live_to_snap[*idx].is_none() && c.eq_ignore_ascii_case(code))
            .count();
        let candidates: Vec<usize> = snap_edids
            .iter()
            .enumerate()
            .filter(|(j, c)| !snap_used[*j] && c.eq_ignore_ascii_case(code))
            .map(|(j, _)| j)
            .collect();
        if unmatched_live_same == 1 && candidates.len() == 1 {
            let j = candidates[0];
            live_to_snap[i] = Some(j);
            snap_used[j] = true;
        }
    }

    // Align snapshot coordinates into the live desktop origin frame using an active anchor
    // (preferring the live primary monitor, since Windows shifts the origin to (0, 0) when
    // the original primary monitor is disabled).
    let anchor_idx = live
        .iter()
        .enumerate()
        .find(|(i, d)| {
            d.active
                && d.resolution.0 > 0
                && d.resolution.1 > 0
                && d.is_primary
                && live_to_snap[*i].is_some()
        })
        .or_else(|| {
            live.iter().enumerate().find(|(i, d)| {
                d.active && d.resolution.0 > 0 && d.resolution.1 > 0 && live_to_snap[*i].is_some()
            })
        })
        .map(|(i, _)| i);

    let delta = if let Some(i) = anchor_idx {
        let j = live_to_snap[i].expect("anchor matched snapshot");
        let m = &snap.monitors[j];
        (
            live[i].position.0 as i64 - m.position_x as i64,
            live[i].position.1 as i64 - m.position_y as i64,
        )
    } else {
        (0, 0)
    };

    for (i, d) in live.iter_mut().enumerate() {
        let Some(j) = live_to_snap[i] else {
            continue;
        };
        if !d.active || d.resolution == (0, 0) {
            let m = &snap.monitors[j];
            d.position = translate_pos((m.position_x, m.position_y), delta);
            if d.resolution == (0, 0) {
                d.resolution = (m.width, m.height);
            }
            if d.refresh_hz <= 0.0 && m.refresh_den > 0 {
                d.refresh_hz = m.refresh_num as f64 / m.refresh_den as f64;
            }
            if d.friendly_name.trim().is_empty() && !m.friendly_name.is_empty() {
                d.friendly_name = m.friendly_name.clone();
            }
        }
    }

    // Surface saved snapshot monitors that are currently unplugged/offline so users can
    // still see them in their last-known relative slot and include/exclude them in profiles.
    for (j, m) in snap.monitors.iter().enumerate() {
        if snap_used[j] {
            continue;
        }
        live.push(DisplayInfo {
            friendly_name: if m.friendly_name.is_empty() {
                "Saved Display".to_string()
            } else {
                m.friendly_name.clone()
            },
            stable_key: m.stable_key.clone(),
            gdi_name: String::new(),
            adapter_path: String::new(),
            edid_code: saved_edid_token(m),
            role: m.role,
            active: false,
            available: false,
            is_primary: false,
            position: translate_pos((m.position_x, m.position_y), delta),
            resolution: (m.width, m.height),
            refresh_hz: if m.refresh_den > 0 {
                m.refresh_num as f64 / m.refresh_den as f64
            } else {
                60.0
            },
            rotation: m.rotation.max(0) as u32,
        });
        live_to_snap.push(Some(j));
    }

    // If Windows closed the gap between active monitors after a middle monitor was disabled,
    // an inactive monitor can appear to overlap an active neighbor even though the snapshot
    // rectangles were disjoint (preserve cloned/intentional overlaps untouched).
    let mut has_gap_collapse_overlap = false;
    'outer: for a in 0..live.len() {
        for b in (a + 1)..live.len() {
            if live[a].active && live[b].active {
                continue;
            }
            let (Some(sa), Some(sb)) = (live_to_snap[a], live_to_snap[b]) else {
                continue;
            };
            if rects_overlap(
                live[a].position,
                live[a].resolution,
                live[b].position,
                live[b].resolution,
            ) && !rects_overlap(
                (snap.monitors[sa].position_x, snap.monitors[sa].position_y),
                (snap.monitors[sa].width, snap.monitors[sa].height),
                (snap.monitors[sb].position_x, snap.monitors[sb].position_y),
                (snap.monitors[sb].width, snap.monitors[sb].height),
            ) {
                has_gap_collapse_overlap = true;
                break 'outer;
            }
        }
    }

    let all_active_matched = live
        .iter()
        .enumerate()
        .all(|(idx, d)| !d.active || live_to_snap[idx].is_some());
    if has_gap_collapse_overlap && all_active_matched {
        for (idx, d) in live.iter_mut().enumerate() {
            if let Some(sj) = live_to_snap[idx] {
                let m = &snap.monitors[sj];
                d.position = translate_pos((m.position_x, m.position_y), delta);
            }
        }
    }

    // Fallback: never let an unmatched inactive monitor visually occlude another placed monitor.
    for i in 0..live.len() {
        if live[i].active
            || live[i].resolution.0 == 0
            || live[i].resolution.1 == 0
            || live_to_snap[i].is_some()
        {
            continue;
        }
        let overlaps_any = (0..live.len()).any(|k| {
            k != i
                && rects_overlap(
                    live[i].position,
                    live[i].resolution,
                    live[k].position,
                    live[k].resolution,
                )
        });
        if overlaps_any {
            let mut max_right = 0i64;
            let mut min_top: Option<i64> = None;
            for (k, other) in live.iter().enumerate() {
                if k == i || other.resolution.0 == 0 || other.resolution.1 == 0 {
                    continue;
                }
                max_right = max_right.max(other.position.0 as i64 + other.resolution.0 as i64);
                min_top = Some(match min_top {
                    Some(t) => t.min(other.position.1 as i64),
                    None => other.position.1 as i64,
                });
            }
            live[i].position = (
                max_right.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                min_top.unwrap_or(0).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            );
        }
    }

    live
}

fn validate_and_canonicalize_config(cfg: &mut AppConfig) -> Result<(), String> {
    cfg.validate()?;
    let mut used_hotkeys: HashMap<String, String> = HashMap::new();

    let mut check_hk = |raw: &mut String, owner: String| -> Result<(), String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            *raw = String::new();
            return Ok(());
        }
        let parsed = ParsedHotkey::parse(trimmed)?;
        let key = format!("{}:{}", parsed.modifiers, parsed.vk);
        if let Some(prev) = used_hotkeys.insert(key, owner.clone()) {
            return Err(format!(
                "Duplicate shortcut '{}': used by both '{}' and '{}'",
                parsed.canonical, prev, owner
            ));
        }
        *raw = parsed.canonical;
        Ok(())
    };

    check_hk(
        &mut cfg.hotkeys.toggle_all,
        "Toggle All Monitors".to_string(),
    )?;
    check_hk(
        &mut cfg.hotkeys.toggle_externals,
        "Toggle External Monitors".to_string(),
    )?;
    check_hk(
        &mut cfg.hotkeys.force_recover,
        "Force Recover Displays".to_string(),
    )?;

    for (idx, rule) in cfg.hotkeys.monitor_hotkeys.iter_mut().enumerate() {
        *rule = rule.normalized_for_ui();
        let owner = format!("Configuration #{} ({})", idx + 1, rule.display_label());
        rule.to_scope()?;
        check_hk(&mut rule.hotkey, owner)?;
    }

    Ok(())
}

fn handle_save_config(
    ctrl_arc: &Arc<Mutex<Controller>>,
    body: &[u8],
) -> Result<StateResponse, String> {
    let mut payload: SaveConfigPayload =
        serde_json::from_slice(body).map_err(|e| format!("Invalid config JSON: {e}"))?;
    validate_and_canonicalize_config(&mut payload.config)?;

    let (tx, rx) = mpsc::sync_channel(1);
    {
        let mut ctrl = ctrl_arc.lock().expect("controller lock");
        ctrl.enqueue_ui_action(DeferredUiAction::ApplyConfig {
            config: Box::new(payload.config),
            expected_config: payload.expected_config.map(Box::new),
            autostart: payload.autostart,
            reply: tx,
        });
    }

    let apply_res = rx
        .recv_timeout(Duration::from_secs(3))
        .map_err(|_| "Timed out applying configuration on daemon window thread".to_string())?;
    apply_res?;

    Ok(collect_state(ctrl_arc))
}

fn resolve_action_rule(
    rules: &[MonitorHotkeyRule],
    index: usize,
    expected: Option<&MonitorHotkeyRule>,
) -> Result<MonitorHotkeyRule, String> {
    let rule = rules.get(index).ok_or_else(|| {
        "This configuration no longer exists. Refresh the Control Panel and try again.".to_string()
    })?;
    if expected.is_some_and(|expected| expected.normalized_for_ui() != rule.normalized_for_ui()) {
        return Err(
            "This configuration changed. Refresh the Control Panel before running it.".into(),
        );
    }
    Ok(rule.clone())
}

fn handle_action(ctrl_arc: &Arc<Mutex<Controller>>, body: &[u8]) -> Result<ActionResponse, String> {
    let payload: ActionPayload =
        serde_json::from_slice(body).map_err(|e| format!("Invalid action JSON: {e}"))?;

    let message = match payload.action.as_str() {
        "toggle_rule" => {
            let idx = payload
                .index
                .ok_or_else(|| "Missing rule index".to_string())?;
            let mut ctrl = ctrl_arc.lock().expect("controller lock");
            if ctrl.phase == DaemonPhase::Recovering {
                return Err(
                    "Display recovery is currently in progress; wait a moment and try again."
                        .into(),
                );
            }
            let rule = resolve_action_rule(
                &ctrl.config.hotkeys.monitor_hotkeys,
                idx,
                payload.expected_rule.as_ref(),
            )?;
            let scope = rule.to_scope()?;
            let label = rule.display_label();
            match rule.action {
                HotkeyRuleAction::Toggle => {
                    ctrl.enqueue_ui_action(DeferredUiAction::Toggle(scope, None));
                    format!("Toggle queued for '{label}'")
                }
                HotkeyRuleAction::Off => {
                    if ctrl.phase == DaemonPhase::RecoveryFailed {
                        return Err(
                            "Previous display recovery failed. Run Wake All or Force Recover first."
                                .into(),
                        );
                    }
                    if matches!(
                        ctrl.phase,
                        DaemonPhase::LockedOff | DaemonPhase::SleepingNormal
                    ) {
                        return Err("All displays are already sleeping. Use Wake All first.".into());
                    }
                    ctrl.enqueue_ui_action(DeferredUiAction::Off(scope, None));
                    format!("Layout queued for '{label}'")
                }
            }
        }
        "apply_preview" => {
            if payload.include.is_empty() && payload.exclude.is_empty() {
                return Err("Select at least one included or excluded display to test".to_string());
            }
            let scope = TargetScope::Profile {
                include: payload.include,
                exclude: payload.exclude,
            };
            let mut ctrl = ctrl_arc.lock().expect("controller lock");
            if ctrl.phase == DaemonPhase::Recovering {
                return Err(
                    "Display recovery is currently in progress; wait a moment and try again."
                        .into(),
                );
            }
            if payload.off_only {
                if ctrl.phase == DaemonPhase::RecoveryFailed {
                    return Err(
                        "Previous display recovery failed. Run Wake All or Force Recover first."
                            .into(),
                    );
                }
                if matches!(
                    ctrl.phase,
                    DaemonPhase::LockedOff | DaemonPhase::SleepingNormal
                ) {
                    return Err("All displays are already sleeping. Use Wake All first.".into());
                }
                ctrl.enqueue_ui_action(DeferredUiAction::Off(scope, None));
            } else {
                ctrl.enqueue_ui_action(DeferredUiAction::Toggle(scope, None));
            }
            "Test configuration queued".to_string()
        }
        "toggle_monitor" => {
            let key = payload
                .stable_key
                .ok_or_else(|| "Missing monitor stable_key".to_string())?;
            let matched = query_displays(true)?
                .into_iter()
                .find(|d| d.available && d.stable_key.eq_ignore_ascii_case(&key))
                .ok_or_else(|| {
                    "Display is disconnected or unavailable; refresh the Control Panel.".to_string()
                })?;
            let mut ctrl = ctrl_arc.lock().expect("controller lock");
            if ctrl.phase == DaemonPhase::Recovering {
                return Err(
                    "Display recovery is currently in progress; wait a moment and try again."
                        .into(),
                );
            }
            if matched.active && ctrl.phase == DaemonPhase::RecoveryFailed {
                return Err(
                    "Previous display recovery failed. Run Wake All or Force Recover first.".into(),
                );
            }
            ctrl.enqueue_ui_action(DeferredUiAction::Toggle(
                TargetScope::Monitor(matched.stable_key),
                None,
            ));
            "Display toggle queued".to_string()
        }
        "toggle_all" => {
            let mut ctrl = ctrl_arc.lock().expect("controller lock");
            if ctrl.phase == DaemonPhase::Recovering {
                return Err(
                    "Display recovery is currently in progress; wait a moment and try again."
                        .into(),
                );
            }
            ctrl.enqueue_ui_action(DeferredUiAction::Toggle(TargetScope::All, None));
            "All-display toggle queued".to_string()
        }
        "toggle_externals" => {
            let mut ctrl = ctrl_arc.lock().expect("controller lock");
            if ctrl.phase == DaemonPhase::Recovering {
                return Err(
                    "Display recovery is currently in progress; wait a moment and try again."
                        .into(),
                );
            }
            ctrl.enqueue_ui_action(DeferredUiAction::Toggle(TargetScope::ExternalsOnly, None));
            "External-display toggle queued".to_string()
        }
        "wake_all" => {
            let mut ctrl = ctrl_arc.lock().expect("controller lock");
            if ctrl.phase == DaemonPhase::Recovering {
                return Err("Display recovery is already in progress.".into());
            }
            ctrl.enqueue_ui_action(DeferredUiAction::On);
            "Wake & recovery started".to_string()
        }
        "recover" => {
            let mut ctrl = ctrl_arc.lock().expect("controller lock");
            if ctrl.phase == DaemonPhase::Recovering {
                return Err("Display recovery is already in progress.".into());
            }
            ctrl.enqueue_ui_action(DeferredUiAction::Recover(payload.hard));
            if payload.hard {
                "Hard bounce recovery started".to_string()
            } else {
                "Standard recovery started".to_string()
            }
        }
        "snapshot" => {
            let mut ctrl = ctrl_arc.lock().expect("controller lock");
            let count = ctrl.capture_healthy_state()?;
            format!("Captured topology snapshot ({count} active display(s))")
        }
        "install_shortcuts" => {
            let installed = launcher::install_shortcuts(
                payload.start_menu,
                payload.desktop,
                payload.minimized,
            )?;
            format!("Installed {} launcher shortcut(s)", installed.len())
        }
        "remove_shortcuts" => {
            let removed = launcher::remove_shortcuts(payload.start_menu, payload.desktop)?;
            format!("Removed {removed} launcher shortcut(s)")
        }
        other => return Err(format!("Unsupported action '{other}'")),
    };

    // Give the worker thread a short moment so immediate UI refresh often sees the new state.
    if !matches!(
        payload.action.as_str(),
        "snapshot" | "install_shortcuts" | "remove_shortcuts"
    ) {
        thread::sleep(Duration::from_millis(80));
    }
    let state = collect_state(ctrl_arc);
    logging::info(format!("Control Panel action: {message}"));
    Ok(ActionResponse {
        ok: true,
        message,
        state,
    })
}

fn write_json<T: Serialize>(stream: &mut TcpStream, status: u16, value: &T) {
    match serde_json::to_vec(value) {
        Ok(bytes) => write_response(
            stream,
            status,
            "application/json; charset=utf-8",
            &bytes,
            None,
        ),
        Err(e) => write_json_error(stream, 500, &format!("JSON serialization failed: {e}")),
    }
}

fn write_json_error(stream: &mut TcpStream, status: u16, message: &str) {
    let payload = serde_json::to_vec(&ErrorPayload {
        error: message.to_string(),
    })
    .unwrap_or_else(|_| br#"{"error":"Internal error"}"#.to_vec());
    write_response(
        stream,
        status,
        "application/json; charset=utf-8",
        &payload,
        None,
    );
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    set_cookie: Option<&str>,
) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let mut header = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Referrer-Policy: no-referrer\r\n\
         X-Content-Type-Options: nosniff\r\n\
         X-Frame-Options: DENY\r\n\
         Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'\r\n\
         Connection: close\r\n",
        body.len()
    );
    if let Some(cookie) = set_cookie {
        header.push_str(&format!("Set-Cookie: {cookie}\r\n"));
    }
    header.push_str("\r\n");

    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_a_stale_profile_cannot_select_a_different_layout() {
        let original: MonitorHotkeyRule =
            serde_json::from_str(r#"{"label":"Desk","include":["external"],"selector":null}"#)
                .unwrap();
        let mut replacement = original.clone();
        replacement.include = vec!["internal".into()];
        assert!(resolve_action_rule(&[replacement], 0, Some(&original)).is_err());
        assert!(resolve_action_rule(&[], 0, Some(&original)).is_err());
        assert!(resolve_action_rule(std::slice::from_ref(&original), 0, Some(&original)).is_ok());
        let mut legacy = original.clone();
        legacy.include.clear();
        legacy.except = vec!["external".into()];
        assert!(resolve_action_rule(&[legacy], 0, Some(&original)).is_ok());
    }
    use crate::config::HotkeysConfig;
    use crate::display::DisplayRole;

    #[test]
    fn bounds_and_validates_http_framing() {
        let request = read_request(&b"POST /api/action HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nContent-Length: 2\r\n\r\n{}"[..]).unwrap();
        assert_eq!(request.body, b"{}");
        for header in [
            "Content-Length: 1048577",
            "Content-Length: -1",
            "Content-Length: 2\r\nContent-Length: 3",
            "Transfer-Encoding: chunked",
        ] {
            let raw = format!("POST /api/config HTTP/1.1\r\n{header}\r\n\r\n");
            assert!(read_request(raw.as_bytes()).is_err());
        }
        let oversized = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(20000));
        assert!(read_request(oversized.as_bytes()).is_err());
        assert!(read_request(&b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nx"[..]).is_err());

        // Repeatable headers combine while singleton security headers reject duplicates.
        let ok = read_request(
            &b"GET / HTTP/1.1\r\nHost: 127.0.0.1:80\r\nAccept: text/html\r\nAccept: */*\r\n\r\n"[..],
        )
        .unwrap();
        assert_eq!(
            ok.headers.get("accept").map(String::as_str),
            Some("text/html, */*")
        );
        assert!(read_request(
            &b"GET / HTTP/1.1\r\nHost: 127.0.0.1:80\r\nHost: evil.example\r\n\r\n"[..]
        )
        .is_err());
    }

    #[test]
    fn session_tokens_are_random_and_hex_encoded() {
        let a = generate_session_token().unwrap();
        let b = generate_session_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn rejects_hotkey_alias_collisions() {
        let mut cfg = AppConfig::default();
        cfg.hotkeys.toggle_all = "Ctrl+PageUp".into();
        cfg.hotkeys.toggle_externals = "Ctrl+PgUp".into();
        assert!(validate_and_canonicalize_config(&mut cfg)
            .unwrap_err()
            .contains("Duplicate shortcut"));
    }

    #[test]
    fn rejects_duplicate_canonical_hotkeys() {
        let mut cfg = AppConfig {
            hotkeys: HotkeysConfig {
                toggle_all: "Ctrl+Alt+M".into(),
                toggle_externals: "ctrl + alt + m".into(),
                ..HotkeysConfig::default()
            },
            ..AppConfig::default()
        };
        let err = validate_and_canonicalize_config(&mut cfg).unwrap_err();
        assert!(err.contains("Duplicate shortcut 'Ctrl+Alt+M'"));
    }

    #[test]
    fn canonicalizes_hotkeys_and_allows_tray_only_profiles() {
        let mut cfg = AppConfig::default();
        cfg.hotkeys.monitor_hotkeys.push(MonitorHotkeyRule {
            hotkey: "".into(),
            label: Some("Tray Only".into()),
            include: vec!["P27h-20".into()],
            exclude: vec!["MB16AC".into()],
            selector: None,
            except: Vec::new(),
            action: HotkeyRuleAction::Toggle,
        });
        assert!(validate_and_canonicalize_config(&mut cfg).is_ok());
    }

    #[test]
    fn enrich_monitors_preserves_resolution_and_deduplicates_snapshot_keys() {
        let live = vec![DisplayInfo {
            friendly_name: "P27h-20".into(),
            stable_key: "display#len61e9#1".into(),
            gdi_name: r"\\.\DISPLAY1".into(),
            adapter_path: String::new(),
            edid_code: "LEN61E9".into(),
            role: DisplayRole::External,
            active: false,
            available: true,
            is_primary: true,
            position: (0, 0),
            resolution: (0, 0),
            refresh_hz: 0.0,
            rotation: 1,
        }];
        let saved = crate::display::snapshot::SavedMonitorMode {
            stable_key: "DISPLAY#LEN61E9#1".into(),
            friendly_name: "Desk".into(),
            role: DisplayRole::External,
            edid_mfg: 0,
            edid_product: 0,
            preferred_source_id: 0,
            source_group: None,
            position_x: 1920,
            position_y: -100,
            width: 2560,
            height: 1440,
            pixel_format: 3,
            rotation: 1,
            scaling: 1,
            refresh_num: 60000,
            refresh_den: 1000,
            scanline_ordering: 1,
            target_video_signal: None,
        };
        let mut offline = saved.clone();
        offline.stable_key = "display#offline#2".into();
        let snapshot = TopologySnapshot {
            captured_at_unix_ms: 1,
            monitors: vec![saved, offline],
        };
        let result = enrich_monitors(live, Some(&snapshot));
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].resolution, (2560, 1440));
        assert_eq!(result[0].position, (1920, -100));
        assert_eq!(result[0].refresh_hz, 60.0);
        assert!(!result[1].active && !result[1].available);
    }

    fn make_saved_mode(
        key: &str,
        name: &str,
        role: DisplayRole,
        pos: (i32, i32),
        res: (u32, u32),
    ) -> SavedMonitorMode {
        SavedMonitorMode {
            stable_key: key.into(),
            friendly_name: name.into(),
            role,
            edid_mfg: 0,
            edid_product: 0,
            preferred_source_id: 0,
            source_group: None,
            position_x: pos.0,
            position_y: pos.1,
            width: res.0,
            height: res.1,
            pixel_format: 4,
            rotation: 1,
            scaling: 1,
            refresh_num: 60,
            refresh_den: 1,
            scanline_ordering: 1,
            target_video_signal: None,
        }
    }

    fn make_live_display(
        key: &str,
        name: &str,
        role: DisplayRole,
        active: bool,
        is_primary: bool,
        pos: (i32, i32),
        res: (u32, u32),
    ) -> DisplayInfo {
        DisplayInfo {
            friendly_name: name.into(),
            stable_key: key.into(),
            gdi_name: if active {
                r"\\.\DISPLAY1".into()
            } else {
                String::new()
            },
            adapter_path: String::new(),
            edid_code: extract_edid_token(key).unwrap_or_default(),
            role,
            active,
            available: true,
            is_primary,
            position: pos,
            resolution: res,
            refresh_hz: if active { 60.0 } else { 0.0 },
            rotation: 1,
        }
    }

    #[test]
    fn enrich_monitors_aligns_off_and_unplugged_monitors_when_primary_shifts() {
        let snapshot = TopologySnapshot {
            captured_at_unix_ms: 1,
            monitors: vec![
                make_saved_mode(
                    "display#boe2775#4&13be0be7&0&uid8261",
                    "J560T09",
                    DisplayRole::External,
                    (-2566, -1442),
                    (2560, 1600),
                ),
                make_saved_mode(
                    "display#len61e9#4&13be0be7&0&uid12613",
                    "P27h-20",
                    DisplayRole::External,
                    (-2560, 158),
                    (2560, 1440),
                ),
                make_saved_mode(
                    "display#aus1641#9&64eeb96&0&uid256",
                    "ASUS MB16AC",
                    DisplayRole::UsbIndirect,
                    (-6, -1080),
                    (1920, 1080),
                ),
                make_saved_mode(
                    "display#len8aa5#4&13be0be7&0&uid8388688",
                    "Internal Display (LEN8AA5)",
                    DisplayRole::Internal,
                    (0, 0),
                    (2560, 1600),
                ),
            ],
        };

        // Windows shifted J560T09 to (0, 0) as the new primary when Internal + ASUS turned off.
        // Internal Display also exercises stale non-zero CCD resolution with (0, 0) position.
        let live = vec![
            make_live_display(
                "display#boe2775#4&13be0be7&0&uid8261",
                "J560T09",
                DisplayRole::External,
                true,
                true,
                (0, 0),
                (2560, 1600),
            ),
            make_live_display(
                "display#len61e9#4&13be0be7&0&uid12613",
                "P27h-20",
                DisplayRole::External,
                true,
                false,
                (6, 1600),
                (2560, 1440),
            ),
            make_live_display(
                "display#aus1641#9&64eeb96&0&uid256",
                "ASUS MB16AC",
                DisplayRole::UsbIndirect,
                false,
                false,
                (0, 0),
                (0, 0),
            ),
            make_live_display(
                "display#len8aa5#4&13be0be7&0&uid8388688",
                "Internal Display (LEN8AA5)",
                DisplayRole::Internal,
                false,
                false,
                (0, 0),
                (2560, 1600),
            ),
        ];

        let aligned = enrich_monitors(live.clone(), Some(&snapshot));
        assert_eq!(aligned.len(), 4);
        assert_eq!(aligned[0].position, (0, 0));
        assert_eq!(aligned[1].position, (6, 1600));
        assert_eq!(aligned[2].position, (2560, 362));
        assert_eq!(aligned[2].resolution, (1920, 1080));
        assert_eq!(aligned[3].position, (2566, 1442));
        for a in 0..aligned.len() {
            for b in (a + 1)..aligned.len() {
                assert!(
                    !rects_overlap(
                        aligned[a].position,
                        aligned[a].resolution,
                        aligned[b].position,
                        aligned[b].resolution
                    ),
                    "{} and {} overlapped",
                    aligned[a].friendly_name,
                    aligned[b].friendly_name
                );
            }
        }

        // Verify unplugged offline monitor omitted from live is also translated cleanly.
        let mut live_without_usb = live;
        live_without_usb.remove(2);
        let offline = enrich_monitors(live_without_usb, Some(&snapshot));
        assert_eq!(offline.len(), 4);
        assert_eq!(offline[3].friendly_name, "ASUS MB16AC");
        assert!(!offline[3].available);
        assert_eq!(offline[3].position, (2560, 362));
    }

    #[test]
    fn enrich_monitors_preserves_physical_order_when_windows_closes_middle_gap() {
        let snapshot = TopologySnapshot {
            captured_at_unix_ms: 1,
            monitors: vec![
                make_saved_mode(
                    "display#left#1",
                    "Left",
                    DisplayRole::External,
                    (-1920, 0),
                    (1920, 1080),
                ),
                make_saved_mode(
                    "display#mid#2",
                    "Middle",
                    DisplayRole::Internal,
                    (0, 0),
                    (1920, 1080),
                ),
                make_saved_mode(
                    "display#right#3",
                    "Right",
                    DisplayRole::External,
                    (1920, 0),
                    (1920, 1080),
                ),
            ],
        };
        let live = vec![
            make_live_display(
                "display#left#1",
                "Left",
                DisplayRole::External,
                true,
                true,
                (0, 0),
                (1920, 1080),
            ),
            make_live_display(
                "display#right#3",
                "Right",
                DisplayRole::External,
                true,
                false,
                (1920, 0),
                (1920, 1080),
            ),
            make_live_display(
                "display#mid#2",
                "Middle",
                DisplayRole::Internal,
                false,
                false,
                (0, 0),
                (0, 0),
            ),
        ];
        let result = enrich_monitors(live, Some(&snapshot));
        assert_eq!(result[0].position, (0, 0));
        assert_eq!(result[1].position, (3840, 0));
        assert_eq!(result[2].position, (1920, 0));
    }

    #[test]
    fn enrich_monitors_preserves_cloned_displays_without_false_collision() {
        let snapshot = TopologySnapshot {
            captured_at_unix_ms: 1,
            monitors: vec![
                make_saved_mode(
                    "display#a#1",
                    "Primary",
                    DisplayRole::Internal,
                    (0, 0),
                    (1920, 1080),
                ),
                make_saved_mode(
                    "display#b#2",
                    "Mirror",
                    DisplayRole::External,
                    (0, 0),
                    (1920, 1080),
                ),
            ],
        };
        let live = vec![
            make_live_display(
                "display#a#1",
                "Primary",
                DisplayRole::Internal,
                true,
                true,
                (0, 0),
                (1920, 1080),
            ),
            make_live_display(
                "display#b#2",
                "Mirror",
                DisplayRole::External,
                false,
                false,
                (0, 0),
                (0, 0),
            ),
        ];
        let result = enrich_monitors(live, Some(&snapshot));
        assert_eq!(result[0].position, (0, 0));
        assert_eq!(result[1].position, (0, 0));
    }
}
