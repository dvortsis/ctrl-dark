# Fourth review (Launcher & Start Minimized to Tray) — November 25, 2026

Implemented `MonitorToggle.exe`, `start_minimized_to_tray` configuration/tray/UI support, `monitor-toggle launch` / `shortcut` / `ui --minimized` CLI subcommands, pure-Rust `.ico` emblem generation, and a comprehensive Audit #4 pass across the new and modified components.

## Findings fixed during Audit #4

| Component | Finding | Correction |
| --- | --- | --- |
| `src/launcher.rs` | `ensure_daemon_running` could return `(true, None)` if an already-starting daemon had not finished publishing `ui_url` yet. | Poll until `st.ui_url.is_some()` when IPC responds before `ui_url` is ready, preserving `was_running`. |
| `src/launcher.rs` | `expand_env_vars` only handled 5 hardcoded variable names, risking unexpanded redirected Desktop registry paths (e.g., `%OneDrive%\Desktop`). | Implemented case-insensitive generic `%VAR%` expansion against `std::env::vars()` with unit test coverage. |
| `src/launcher.rs` | `write_windows_shortcut` skipped clearing `Arguments` when `MT_ARGS` was empty while overwriting an existing `.lnk`. | Unconditionally set `$s.Arguments = [string]$env:MT_ARGS` so `--minimized` is cleared when reinstalling without it. |
| `src/bin/launcher.rs` | Non-autostart rapid double-clicks both bypassed the single-instance mutex and could race cold startup. | Serialize manual launches across `Local\MonitorToggleLauncherMutexV1` using `WaitForSingleObject` and release via `ReleaseMutex`. |
| `src/autostart.rs` | Duplicated daemon executable fallback logic inside `enable()`. | Extracted `resolve_autostart_command()`. |
| `src/tray.rs` | `show_balloon` set `NIF_INFO` alone, which would register an icon without callback message or `hIcon` if `add_or_update` had not yet succeeded. | Include `NIF_MESSAGE \| NIF_ICON` alongside `NIF_INFO` when `!self.added`. |
| `src/launcher.rs`, `src/autostart.rs` | Detached `monitor-toggled.exe` and `monitor-toggle-ui.exe` spawns inherited caller stdio handles, risking pipe EOF hangs in PowerShell/CI callers. | Detach stdio cleanly with `Stdio::null()` on all spawned child processes. |
| `src/ui/index.html` | Nested `role="region"` wrappers inside `<section class="panel">` created redundant landmarks around `<fieldset>` controls. | Removed the redundant `role="region"` wrappers and clarified shortcut behavior text. |
