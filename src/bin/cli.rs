//! Lightweight CLI controller for `monitor-toggled`.

use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use monitor_toggle::autostart;
use monitor_toggle::config::{self, AppConfig, WakeMode};
use monitor_toggle::display::ccd::{
    deactivate_targets, query_available_targets, query_displays, DeactivateResult,
};
use monitor_toggle::display::recovery::{run_recovery, RecoveryLevel, WakeContext};
use monitor_toggle::display::snapshot::{
    capture_active_snapshot, load_snapshot, plan_restore, save_snapshot_atomic,
};
use monitor_toggle::display::{resolve_scope_keys, TargetScope};
use monitor_toggle::ipc::{send_request, IpcRequest, IpcResponse};
use monitor_toggle::launcher::{self, LaunchAction, LaunchOptions};
use monitor_toggle::window_restore::{capture_windows, wait_for_gdi_monitors};

#[derive(Parser)]
#[command(
    name = "monitor-toggle",
    version,
    about = "Ctrl + Dark CLI — Turn off monitors via hotkey/profile and recover displays reliably"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Toggle monitors off/on (scope: all, externals, internal, !A,B, or monitor name/EDID/index)
    Toggle {
        #[arg(default_value = "all")]
        scope: String,
        /// Turn off/toggle all displays EXCEPT these (comma-separated, e.g. --except P27h-20,J560T09)
        #[arg(long, value_delimiter = ',')]
        except: Vec<String>,
    },
    /// Turn monitors off
    Off {
        #[arg(default_value = "all")]
        scope: String,
        /// Turn off all displays EXCEPT these (comma-separated, e.g. --except P27h-20,J560T09)
        #[arg(long, value_delimiter = ',')]
        except: Vec<String>,
        #[arg(long, value_enum)]
        mode: Option<CliWakeMode>,
    },
    /// Wake monitors and run standard anti-black-screen recovery
    On,
    /// Force wake & multi-stage display recovery (works standalone if daemon is stopped)
    Recover {
        /// Perform 450ms per-path CCD deactivate/reactivate bounce to break stalled links
        #[arg(long)]
        hard: bool,
        /// Bypass daemon and run recovery directly in this process
        #[arg(long)]
        standalone: bool,
    },
    /// List connected CCD displays with stable keys, roles, and modes
    Monitors,
    /// Show daemon status, hotkeys, and saved topology snapshot
    Status,
    /// Capture and save current healthy display topology snapshot
    Snapshot,
    /// Manage the background `monitor-toggled.exe` daemon
    Daemon {
        #[arg(value_enum)]
        action: DaemonAction,
    },
    /// Manage current-user Windows sign-in autostart (HKCU Run)
    Autostart {
        #[arg(value_enum)]
        action: AutostartAction,
    },
    /// Inspect or initialize `config.toml`
    Config {
        #[arg(value_enum)]
        action: ConfigAction,
    },
    /// Open the graphical Ctrl + Dark Control Panel
    Ui {
        /// Open the Control Panel in the default web browser instead of the native window
        #[arg(long)]
        browser: bool,
        /// Start the background tray daemon minimized without opening the Control Panel window
        #[arg(short, long)]
        minimized: bool,
    },
    /// Run desktop launcher behavior (respecting `start_minimized_to_tray` unless overridden)
    Launch {
        /// Force starting minimized in the system tray without opening the UI window
        #[arg(short, long)]
        minimized: bool,
        /// Force opening the Control Panel window immediately
        #[arg(long)]
        open: bool,
        /// Force opening the Control Panel in the default web browser
        #[arg(long)]
        browser: bool,
    },
    /// Install, remove, or inspect Start Menu & Desktop shortcuts for `CtrlDark.exe`
    Shortcut {
        #[arg(value_enum)]
        action: ShortcutAction,
        /// Target the current user's Desktop shortcut
        #[arg(long)]
        desktop: bool,
        /// Target the current user's Start Menu Programs shortcut
        #[arg(long)]
        start_menu: bool,
        /// Always pass `--minimized` on the created shortcut
        #[arg(long)]
        minimized: bool,
    },
    /// Benchmark live selective-off and recovery latency across stages
    Bench {
        /// Displays to keep ON during selective-off benchmark (comma-separated)
        #[arg(long, value_delimiter = ',', default_value = "P27h-20,J560T09")]
        except: Vec<String>,
        /// Milliseconds to hold in selective-off state before recovering
        #[arg(long, default_value_t = 600)]
        hold_ms: u64,
        /// Measure software overhead only (skip SetDisplayConfig calls)
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum CliWakeMode {
    HotkeyOnly,
    AnyInput,
}

impl From<CliWakeMode> for WakeMode {
    fn from(value: CliWakeMode) -> Self {
        match value {
            CliWakeMode::HotkeyOnly => WakeMode::HotkeyOnly,
            CliWakeMode::AnyInput => WakeMode::AnyInput,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum DaemonAction {
    Start,
    Stop,
    Status,
}

#[derive(Clone, Copy, ValueEnum)]
enum AutostartAction {
    Enable,
    Disable,
    Status,
}

#[derive(Clone, Copy, ValueEnum)]
enum ConfigAction {
    Path,
    Init,
}

#[derive(Clone, Copy, ValueEnum)]
enum ShortcutAction {
    Install,
    Remove,
    Status,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn resolve_cli_scope(scope: &str, except: Vec<String>) -> TargetScope {
    let keep: Vec<String> = except
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if !keep.is_empty() {
        TargetScope::Except(keep)
    } else {
        TargetScope::parse(scope)
    }
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Commands::Toggle { scope, except } => {
            ensure_daemon_running()?;
            let resp = send_request(
                &IpcRequest::Toggle {
                    scope: resolve_cli_scope(&scope, except),
                },
                1500,
            )?;
            print_simple_response(resp)
        }
        Commands::Off {
            scope,
            except,
            mode,
        } => {
            ensure_daemon_running()?;
            let resp = send_request(
                &IpcRequest::Off {
                    scope: resolve_cli_scope(&scope, except),
                    wake_mode: mode.map(Into::into),
                },
                1500,
            )?;
            print_simple_response(resp)
        }
        Commands::On => {
            ensure_daemon_running()?;
            let resp = send_request(&IpcRequest::On, 1500)?;
            print_simple_response(resp)
        }
        Commands::Recover { hard, standalone } => {
            if !standalone {
                if let Ok(resp) = send_request(&IpcRequest::Recover { hard }, 1000) {
                    return print_simple_response(resp);
                }
            }
            let cfg = AppConfig::load_or_create_default()?;
            let snap = load_snapshot(&config::snapshot_path())?;
            let level = if hard {
                RecoveryLevel::HardBounce
            } else {
                RecoveryLevel::Standard
            };
            let report = run_recovery(
                level,
                &cfg,
                snap.as_ref(),
                &[],
                None,
                WakeContext::default(),
            )?;
            println!("{}", report.summary);
            Ok(())
        }
        Commands::Monitors => {
            let displays = match send_request(&IpcRequest::ListMonitors, 500) {
                Ok(IpcResponse::Ok {
                    monitors: Some(m), ..
                }) => m,
                _ => query_displays(true)?,
            };
            print_monitors_table(&displays);
            Ok(())
        }
        Commands::Status => match send_request(&IpcRequest::Status, 800) {
            Ok(IpcResponse::Ok {
                status: Some(st), ..
            }) => {
                println!("Daemon PID          : {}", st.pid);
                println!("Phase               : {}", st.state);
                println!("Wake Mode           : {}", st.wake_mode.as_str());
                println!("Toggle All Hotkey   : {}", st.toggle_all_hotkey);
                println!("Toggle Ext Hotkey   : {}", st.toggle_externals_hotkey);
                println!("Force Recover Hotkey: {}", st.force_recover_hotkey);
                for rule in &st.custom_hotkeys {
                    println!("Custom Hotkey       : {rule}");
                }
                println!(
                    "Monitors            : {} active / {} available ({} in snapshot)",
                    st.active_monitors, st.total_available_monitors, st.snapshot_monitors
                );
                if !st.disabled_monitors.is_empty() {
                    println!("Disabled Selectively: {}", st.disabled_monitors.join(", "));
                }
                if !st.hotkey_warnings.is_empty() {
                    println!("Hotkey Warnings     : {}", st.hotkey_warnings.join(" | "));
                }
                if let Some(last) = st.last_recovery_summary {
                    println!("Last Recovery       : {last}");
                }
                Ok(())
            }
            Ok(IpcResponse::Error { message }) => Err(message),
            Ok(_) => Err("Unexpected response from daemon".to_string()),
            Err(_) => {
                println!("Daemon Status: stopped");
                if let Ok(Some(snap)) = load_snapshot(&config::snapshot_path()) {
                    println!(
                        "Saved Snapshot: {} monitor(s) at unix_ms={}",
                        snap.monitors.len(),
                        snap.captured_at_unix_ms
                    );
                }
                Ok(())
            }
        },
        Commands::Snapshot => {
            if let Ok(response) = send_request(&IpcRequest::SnapshotNow, 1000) {
                return print_simple_response(response);
            }
            let snap = capture_active_snapshot()?;
            let count = snap.monitors.len();
            save_snapshot_atomic(&snap, &config::snapshot_path())?;
            println!(
                "Saved snapshot ({count} active display(s)) to {}",
                config::snapshot_path().display()
            );
            Ok(())
        }
        Commands::Daemon { action } => match action {
            DaemonAction::Start => {
                let already = send_request(&IpcRequest::Status, 250).is_ok();
                if already {
                    println!("monitor-toggled is already running.");
                    Ok(())
                } else {
                    start_daemon_process()?;
                    println!("monitor-toggled started.");
                    Ok(())
                }
            }
            DaemonAction::Stop => match send_request(&IpcRequest::Shutdown, 800) {
                Ok(resp) => print_simple_response(resp),
                Err(_) => {
                    println!("monitor-toggled is not running.");
                    Ok(())
                }
            },
            DaemonAction::Status => {
                if let Ok(IpcResponse::Ok {
                    status: Some(st), ..
                }) = send_request(&IpcRequest::Status, 500)
                {
                    println!("running (PID {}, phase={})", st.pid, st.state);
                } else {
                    println!("stopped");
                }
                Ok(())
            }
        },
        Commands::Autostart { action } => match action {
            AutostartAction::Enable => {
                let cmd = autostart::enable()?;
                println!("Autostart enabled: {cmd}");
                Ok(())
            }
            AutostartAction::Disable => {
                autostart::disable()?;
                println!("Autostart disabled.");
                Ok(())
            }
            AutostartAction::Status => {
                match autostart::is_enabled()? {
                    Some(cmd) => println!("Autostart enabled: {cmd}"),
                    None => println!("Autostart disabled."),
                }
                Ok(())
            }
        },
        Commands::Config { action } => match action {
            ConfigAction::Path => {
                println!("{}", config::config_path().display());
                Ok(())
            }
            ConfigAction::Init => {
                let path = config::config_path();
                if path.exists() {
                    println!("Config already exists: {}", path.display());
                } else {
                    config::write_default_commented_toml(&path)?;
                    println!("Created default config: {}", path.display());
                }
                Ok(())
            }
        },
        Commands::Ui { browser, minimized } => {
            if minimized {
                ensure_daemon_running()?;
                println!("monitor-toggled is running minimized in the system tray.");
                return Ok(());
            }
            ensure_daemon_running()?;
            let url = match send_request(&IpcRequest::Status, 1500)? {
                IpcResponse::Ok {
                    status: Some(st), ..
                } => st.ui_url,
                IpcResponse::Error { message } => return Err(message),
                _ => None,
            };
            autostart::launch_ui_window(url.as_deref(), browser)
        }
        Commands::Launch {
            minimized,
            open,
            browser,
        } => {
            let action = launcher::execute_launch(&LaunchOptions {
                force_minimized: minimized,
                force_open: open,
                browser,
                autostart: false,
            })?;
            match action {
                LaunchAction::StayInTray { .. } => {
                    println!("Ctrl + Dark is running minimized in the system tray.");
                }
                LaunchAction::OpenUi { .. } => {
                    println!("Opened Ctrl + Dark Control Panel.");
                }
            }
            Ok(())
        }
        Commands::Shortcut {
            action,
            desktop,
            start_menu,
            minimized,
        } => {
            let (use_sm, use_desk) = if !start_menu && !desktop {
                (true, false)
            } else {
                (start_menu, desktop)
            };
            match action {
                ShortcutAction::Status => {
                    let st = launcher::shortcut_status();
                    println!(
                        "Start Menu Shortcut : {} ({})",
                        if st.start_menu {
                            "installed"
                        } else {
                            "not installed"
                        },
                        st.start_menu_path
                    );
                    println!(
                        "Desktop Shortcut    : {} ({})",
                        if st.desktop {
                            "installed"
                        } else {
                            "not installed"
                        },
                        st.desktop_path
                    );
                    Ok(())
                }
                ShortcutAction::Install => {
                    let paths = launcher::install_shortcuts(use_sm, use_desk, minimized)?;
                    for p in paths {
                        println!("Installed shortcut: {}", p.display());
                    }
                    Ok(())
                }
                ShortcutAction::Remove => {
                    let count = launcher::remove_shortcuts(use_sm, use_desk)?;
                    println!("Removed {count} shortcut(s).");
                    Ok(())
                }
            }
        }
        Commands::Bench {
            except,
            hold_ms,
            dry_run,
        } => run_bench(except, hold_ms, dry_run),
    }
}

fn run_bench(except: Vec<String>, hold_ms: u64, dry_run: bool) -> Result<(), String> {
    if hold_ms > 60_000 {
        return Err("Benchmark hold_ms must not exceed 60000".into());
    }
    if !dry_run && send_request(&IpcRequest::Status, 800).is_ok() {
        return Err("Stop the daemon before a live benchmark so its recovery worker cannot race the benchmark. Use --dry-run for read-only inspection.".into());
    }
    let cfg = AppConfig::load_or_create_default()?;
    let t_cap = Instant::now();
    let active = query_displays(false)?;
    if active.len() < 2 {
        return Err(format!(
            "Need at least 2 active displays to benchmark selective-off (found {})",
            active.len()
        ));
    }

    let snap = capture_active_snapshot()?;
    let saved_windows = capture_windows(&active);
    let cap_ms = t_cap.elapsed().as_millis();
    let keys = resolve_scope_keys(&active, &TargetScope::Except(except.clone()));
    if keys.is_empty() {
        return Err("No active displays matched for selective-off".to_string());
    }

    if dry_run {
        let t_q = Instant::now();
        let avail = query_available_targets()?;
        let q_ms = t_q.elapsed().as_millis();

        let t_plan = Instant::now();
        let plan = plan_restore(&snap, &avail, true);
        let plan_ms = t_plan.elapsed().as_millis();

        let (gdi_ms, _) = wait_for_gdi_monitors(&active, 200);

        println!(
            "Dry-Run Software Overhead ({} active displays, {} saved windows):",
            active.len(),
            saved_windows.len()
        );
        println!("  Snapshot+Windows Capture : {cap_ms} ms");
        println!("  CCD QDC_ALL_PATHS Query  : {q_ms} ms");
        println!(
            "  Dynamic-LUID Plan Build  : {plan_ms} ms (paths={})",
            plan.map(|p| p.paths.len()).unwrap_or(0)
        );
        println!("  GDI Monitor Ready Poll   : {gdi_ms} ms");
        println!("  Window Restore           : skipped (read-only dry run)");
        return Ok(());
    }

    println!(
        "Benchmarking selective-off (keeping {}, disabling {} of {} displays, capture={}ms, hold={}ms)...",
        except.join(", "),
        keys.len(),
        active.len(),
        cap_ms,
        hold_ms
    );

    let snap_path = config::snapshot_path();
    let should_save = load_snapshot(&snap_path)
        .ok()
        .flatten()
        .map(|prev| snap.monitors.len() >= prev.monitors.len())
        .unwrap_or(true);
    if should_save {
        save_snapshot_atomic(&snap, &snap_path)?;
    }
    let t_off = Instant::now();
    match deactivate_targets(&keys) {
        Ok(DeactivateResult::Deactivated(n)) => {
            let off_ms = t_off.elapsed().as_millis();
            println!("Off Latency      : {off_ms} ms ({n} display(s) deactivated)");
        }
        Ok(other) => {
            return Err(format!("Unexpected deactivate result: {other:?}"));
        }
        Err(e) if e.contains("error 5") => {
            return Err(format!(
                "{e} (Windows blocks SetDisplayConfig when run from a non-Default/sandboxed desktop; run `bench` from your interactive Windows terminal)"
            ));
        }
        Err(e) => return Err(e),
    }

    thread::sleep(Duration::from_millis(hold_ms));

    let report = run_recovery(
        RecoveryLevel::Standard,
        &cfg,
        Some(&snap),
        &saved_windows,
        None,
        WakeContext {
            was_selective_off: true,
            console_display_off: false,
            from_session_unlock: false,
        },
    )
    .map_err(|e| {
        format!("{e}. Run 'monitor-toggle recover --hard --standalone' to restore displays.")
    })?;

    println!(
        "Recovery Total   : {} ms (Stage1={}ms, Stage2={}ms, Stage3={}ms, Stage4={}ms [gdi_wait={}ms])",
        report.total_ms,
        report.stage1_ms,
        report.stage2_ms,
        report.stage3_ms,
        report.stage4_ms,
        report.stage4_gdi_wait_ms
    );
    println!(
        "Details          : topology_restored={}, relaxed_timings={}, retrained={}, bounced={}, windows_restored={}",
        report.topology_restored,
        report.used_relaxed_timings,
        report.retrained_displays.len(),
        report.bounced_paths,
        report.windows_restored
    );

    let post = query_displays(false)?;
    println!(
        "Post-Check       : {}/{} active display(s) restored",
        post.len(),
        snap.monitors.len()
    );
    if post.len() < snap.monitors.len() {
        return Err(format!(
            "Post-check mismatch: expected {} active displays, got {}",
            snap.monitors.len(),
            post.len()
        ));
    }
    Ok(())
}

fn print_simple_response(resp: IpcResponse) -> Result<(), String> {
    match resp {
        IpcResponse::Ok { message, .. } => {
            println!("{message}");
            Ok(())
        }
        IpcResponse::Error { message } => Err(message),
    }
}

fn ensure_daemon_running() -> Result<(), String> {
    launcher::ensure_daemon_running(false).map(|_| ())
}

fn start_daemon_process() -> Result<(), String> {
    launcher::ensure_daemon_running(false).map(|_| ())
}

fn print_monitors_table(displays: &[monitor_toggle::display::DisplayInfo]) {
    if displays.is_empty() {
        println!("No displays found.");
        return;
    }
    println!(
        "{:<3} {:<26} {:<12} {:<12} {:<7} {:<13} {:<12} Stable Key",
        "#", "Name", "Role", "GDI", "State", "Resolution", "Position"
    );
    println!("{}", "-".repeat(115));
    for (idx, d) in displays.iter().enumerate() {
        let state = if d.active {
            if d.is_primary {
                "PRI"
            } else {
                "ON"
            }
        } else if d.available {
            "OFF"
        } else {
            "GONE"
        };
        let res = if d.resolution.0 > 0 {
            format!("{}x{}@{:.0}", d.resolution.0, d.resolution.1, d.refresh_hz)
        } else {
            "-".to_string()
        };
        let pos = format!("({},{})", d.position.0, d.position.1);
        let gdi = if d.gdi_name.is_empty() {
            "-"
        } else {
            d.gdi_name.as_str()
        };
        println!(
            "{:<3} {:<26} {:<12} {:<12} {:<7} {:<13} {:<12} {}",
            idx + 1,
            truncate(&d.friendly_name, 26),
            d.role.label(),
            gdi,
            state,
            res,
            pos,
            d.stable_key
        );
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
