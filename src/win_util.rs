//! Common Win32 string, EDID, and stable device-key helpers.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Replace a file only after its complete contents reach disk. Unique temporary files
/// allow concurrent snapshot writers without sharing or deleting each other's buffers.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    static WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // Windows can reject simultaneous replacement of the same destination.
    let _writer = WRITER.lock().map_err(|_| "Atomic writer lock poisoned")?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|e| format!("Create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| format!("Create {}: {e}", tmp.display()))?;
    let result = (|| {
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| format!("Write {}: {e}", tmp.display()))?;
        drop(file);
        let src: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
        let dst: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe {
            MoveFileExW(
                src.as_ptr(),
                dst.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(format!(
                "Replace {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Encodes a Rust string slice as a null-terminated UTF-16 buffer for Win32 W-APIs.
pub fn to_wide_null(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// Decodes a null-terminated UTF-16 slice into a trimmed Rust `String`.
pub fn from_wide_slice(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len]).trim().to_string()
}

/// Extracts the 2nd `#`-delimited hardware segment (e.g. `LEN8AA5`) from a Windows
/// monitor interface path such as `\\?\DISPLAY#LEN8AA5#4&13be0be7&0&UID8388688#{...}`.
pub fn extract_edid_token(device_path: &str) -> Option<String> {
    let trimmed = device_path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.split('#');
    let _prefix = parts.next()?;
    let token = parts.next()?.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_ascii_uppercase())
    }
}

/// Produces a LUID-independent, interface-GUID-free hardware identifier for a monitor.
pub fn normalize_stable_monitor_key(
    device_path: &str,
    edid_mfg: u16,
    edid_prod: u16,
    connector: u32,
) -> String {
    let mut cleaned = device_path.trim();
    if let Some(rest) = cleaned.strip_prefix(r"\\?\") {
        cleaned = rest;
    }
    if let Some((before_guid, _)) = cleaned.rsplit_once("#{") {
        cleaned = before_guid;
    }
    let cleaned = cleaned.trim();
    if !cleaned.is_empty() {
        cleaned.to_ascii_lowercase()
    } else {
        format!("edid:{edid_mfg:04x}:{edid_prod:04x}:{connector}")
    }
}

/// Decodes a 16-bit EDID/PnP manufacturer ID (big-endian packed 5-bit letters) to 3 chars.
pub fn decode_edid_manufacturer(mfg: u16) -> String {
    if mfg == 0 {
        return String::new();
    }
    // Windows stores edidManufactureId in little-endian u16 representing big-endian EDID bytes.
    let be = mfg.swap_bytes();
    let c1 = (((be >> 10) & 0x1F) as u8).saturating_add(b'A' - 1);
    let c2 = (((be >> 5) & 0x1F) as u8).saturating_add(b'A' - 1);
    let c3 = ((be & 0x1F) as u8).saturating_add(b'A' - 1);
    if c1.is_ascii_uppercase() && c2.is_ascii_uppercase() && c3.is_ascii_uppercase() {
        format!("{}{}{}", c1 as char, c2 as char, c3 as char)
    } else {
        format!("{mfg:04X}")
    }
}

/// Returns current UNIX epoch time in milliseconds.
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_writes_never_mix_concurrent_contents() {
        let dir = std::env::temp_dir().join(format!(
            "monitor-toggle-atomic-{}-{}",
            std::process::id(),
            now_unix_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        write_atomic(&path, b"original").unwrap();
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || write_atomic(&path, &vec![b'a' + i; 8192]).unwrap())
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 8192);
        assert!(bytes.iter().all(|b| *b == bytes[0]));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn normalizes_device_path_consistently() {
        let raw =
            r"\\?\DISPLAY#LEN61E9#4&13be0be7&0&UID12613#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
        assert_eq!(
            normalize_stable_monitor_key(raw, 0, 0, 0),
            "display#len61e9#4&13be0be7&0&uid12613"
        );
        assert_eq!(extract_edid_token(raw).as_deref(), Some("LEN61E9"));
    }

    #[test]
    fn falls_back_when_path_missing() {
        assert_eq!(
            normalize_stable_monitor_key("", 0x30AE, 0x61E9, 2),
            "edid:30ae:61e9:2"
        );
    }
}
