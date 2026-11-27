//! Lightweight rotating file logger writing to `%LOCALAPPDATA%\monitor-toggle\daemon.log`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const MAX_LOG_BYTES: u64 = 1024 * 1024;

static LOGGER: OnceLock<Mutex<LoggerState>> = OnceLock::new();

#[repr(C)]
#[derive(Default)]
struct SystemTimeWin {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

#[link(name = "kernel32")]
extern "system" {
    fn GetLocalTime(lp_system_time: *mut SystemTimeWin);
}

fn format_timestamp_parts(
    year: u16,
    month: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    millis: u16,
) -> String {
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{millis:03}")
}

fn local_timestamp() -> String {
    let mut st = SystemTimeWin::default();
    unsafe {
        GetLocalTime(&mut st);
    }
    format_timestamp_parts(
        st.year,
        st.month,
        st.day,
        st.hour,
        st.minute,
        st.second,
        st.milliseconds,
    )
}

struct LoggerState {
    path: Option<PathBuf>,
}

/// Initializes file logging at `path`. Safe to call multiple times; first call wins.
pub fn init(path: PathBuf) {
    let _ = LOGGER.get_or_init(|| {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        Mutex::new(LoggerState { path: Some(path) })
    });
}

/// Formats and appends a log line if initialized.
pub fn log(level: &str, msg: &str) {
    let ts = local_timestamp();
    let line = format!("[{ts}] [{level}] {msg}\n");

    if let Some(lock) = LOGGER.get() {
        if let Ok(state) = lock.lock() {
            if let Some(path) = &state.path {
                if let Ok(meta) = fs::metadata(path) {
                    if meta.len() >= MAX_LOG_BYTES {
                        let backup = path.with_extension("log.1");
                        let _ = fs::remove_file(&backup);
                        let _ = fs::rename(path, &backup);
                    }
                }
                if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                    let _ = file.write_all(line.as_bytes());
                }
            }
        }
    }
}

pub fn info(msg: impl AsRef<str>) {
    log("INFO", msg.as_ref());
}

pub fn warn(msg: impl AsRef<str>) {
    log("WARN", msg.as_ref());
}

pub fn error(msg: impl AsRef<str>) {
    log("ERROR", msg.as_ref());
}

#[cfg(test)]
mod tests {
    use super::format_timestamp_parts;

    #[test]
    fn formats_local_timestamps_cleanly() {
        assert_eq!(
            format_timestamp_parts(2026, 11, 26, 16, 17, 5, 42),
            "2026-11-26 16:17:05.042"
        );
    }
}
