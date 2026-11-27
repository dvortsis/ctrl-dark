# Repository review — 25 November 2026

Reviewed all Rust modules and binary entry points, embedded HTML/CSS/JavaScript, configuration and snapshot persistence, local transports, display recovery, input guards, and user workflows. Changes are in the working tree. The repository's original files were already untracked at review start; no commits or deployment were performed.

## Blocking and high-impact findings fixed

| Finding | Impact | Implemented fix |
| --- | --- | --- |
| Embedded DisplayPort, UDI and LVDS panels classified as external | External-only toggling could switch off the laptop panel | Classify embedded outputs as internal; recognize wired indirect outputs without relying on adapter-name substrings |
| Virtual CCD indices decoded from low bits and flags cleared before decoding | Wrong mode references during topology changes | Decode high 16 bits, preserve indices before normalizing paths, test invalid sentinels |
| Hard bounce could deactivate paths without a restore baseline | Displays could remain disabled after recovery | Capture the current topology before bouncing; abort if capture fails; propagate restore failures |
| Recovery reported completion despite missing active displays | UI could claim success and replace the good baseline | Verify reactivation, expose `recovery_failed`, preserve snapshot and allow retry |
| Non-ASCII selector slicing | UTF-8 boundary panic could abort the daemon | Use checked prefix access and Unicode regression cases |
| Numeric selectors also matched digits in names/IDs | Selecting display 1 could affect other displays | Resolve numeric selectors strictly as enumeration indices in Rust and the preview |
| Input guard shutdown raced queue creation | Immediate wake could hang joining the guard thread | Create the message queue before publishing its thread ID; fail open on hook installation failure |
| Held keys counted as repeated escape/hotkey presses | Auto-repeat could trigger unintended actions | Track held keys; count distinct presses; re-arm guard mappings when hotkeys change |
| IPC timeout covered only pipe connection | Stalled peers could hang CLI reads or daemon shutdown | Overlapped operations with cancellation, bounded message sizes, partial-write handling, receipt acknowledgement instead of unbounded flush; reject remote clients |
| Unbounded HTTP request lines/headers and connection threads | Local connections could exhaust resources | 16 KiB header limit, 1 MiB body limit, eight connections, request deadline, strict framing |
| API accepted ambient cookies as mutation credentials | Browser-origin boundary was too permissive | Require explicit token header, validate Host/Origin, use OS random tokens, prohibit framing and referrer leakage |

Windows structure decisions were checked against Microsoft's [source mode structure](https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ns-wingdi-displayconfig_path_source_info), [target mode structure](https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ns-wingdi-displayconfig_path_target_info), and [output technology definitions](https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ne-wingdi-displayconfig_video_output_technology).

## Important reliability and UX findings fixed

| Finding | Impact | Implemented fix |
| --- | --- | --- |
| Config file truncation and shared snapshot temporary file | Interrupted/concurrent saves could lose data | Synced unique temporary files, serialized replacement, atomic Windows rename; persist baseline before changing displays |
| Snapshot capture allowed during selective-off/recovery | Good restore topology could be replaced with an incomplete layout | Reject capture outside the active phase; CLI preserves daemon rejection |
| DDC timeout multiplied by monitor count and workers accumulated | Slow recovery and resource growth with stuck drivers | Shared batch deadline, fixed outstanding-worker budget, visit all physical panels |
| Restore planner invented source IDs when exhausted | Invalid topology submitted to Windows | Reject a plan without enough reported sources |
| Shortcut aliases bypassed duplicate checks | Registration conflicts discovered only after saving | Compare modifier/virtual-key identity |
| Config changed on disk while runtime remained old after autostart error | Inconsistent live behavior | Apply saved config consistently and report partial autostart failure explicitly |
| UI refresh replaced dirty settings and recreated focused controls | Lost edits and keyboard focus | Preserve drafts, render changed lists only, restore focus by stable control key, add Discard changes |
| Multiple editors could overwrite newer changes | Lost configurations | Compare the editor's baseline on the daemon thread; retain the draft on conflict |
| Recorder cancellation left instruction text in fields | Invalid saved shortcuts | Restore previous value on cancellation/switch; captured shortcuts dispatch input events |
| Builder used volatile names and reordered monitor indices | Profiles could target a different panel | Save stable device keys and retain the editor's monitor mapping; display friendly labels |
| Offline monitors appeared in the live outcome | Misleading preview | Show outcomes only for available displays; do not label offline snapshots primary |
| Form error was hidden by CSS and modal errors appeared behind the dialog | Failed actions had no usable feedback | Show selection validation and errors inside the active modal |
| Initial controls were enabled before state loaded | Null-state errors and unusable actions | Connecting/disconnected states, guarded actions, save serialization and fetch deadlines |
| Small layouts overflowed and faded cards failed contrast | Inaccessible content at narrow widths and in both themes | Flexible intrinsic sizing, wrapping controls, preserved text contrast, stronger control boundaries, reduced-motion and touch-target support |
| Window restoration missed resize-only changes and mixed coordinate systems | Incorrect restored bounds | Compare all bounds, convert saved workspace coordinates, preserve currently minimized windows |
| Benchmark dry run called window restoration and live benchmarks raced the daemon | Unexpected desktop changes and conflicting recovery operations | Make dry run read-only; require daemon stopped for live benchmark, limit hold duration, save a rollback baseline |

## Verification

- Rust unit/integration suite: 30 tests, including real named-pipe round trips and stalled-peer cancellation, concurrent file replacement, malformed HTTP requests, Unicode/numeric selection, embedded-panel roles, virtual indices, and source exhaustion.
- Browser suite: 14 tests using simulated daemon data. Includes dirty edits, automatic refresh/focus, shortcut capture/cancel, profile validation, offline outcomes, modal errors, disconnected controls, stale editors, duplicate submission, long names, discard, and reduced motion.
- Overview and modal accessibility scans with axe in light/dark themes at 320, 768, 1024 and 1920 px. Zero WCAG A/AA violations in those fixtures. Screenshots were also inspected at desktop and narrow widths.
- Formatting, strict Clippy and release compilation. Separate release output avoids replacing the running daemon.
- Windows CI added for formatting, lint, Rust tests, release build, and Chromium browser tests. The workflow itself has not run remotely in this review.

## Remaining validation limits

The automated suite does not power-cycle real monitors or manipulate the user's windows. Validate actual all-screen sleep/wake, Esc × 3, dock unplug/replug, DisplayLink reconnect, mixed-DPI/rotation, mirrored displays, and maximized application restoration on the intended hardware before treating a release as hardware-qualified. Native display/driver calls can still block inside Windows; DDC worker limits contain resource growth but cannot terminate a hung driver call. Application-specific maximize/focus behavior needs live verification.

The current fixed named pipe is intended for one interactive user/session. It is not a multi-session service design. Browser tests use Edge/Chromium, not a live WebView2 host. Full screen-reader and physical keyboard testing remains separate from axe checks.

## UI review strengths and next validation

The existing interface has clear task grouping, semantic labels and native dialogs, a useful spatial display map, and a consistent light/dark palette. The highest-impact UI correction was preserving edits and focus across state refreshes. The highest-value remaining validation is a supervised hardware sleep/recovery pass with the intended dock and monitors.

## Follow-up fixes & release promotion

- Stopped passing the session token via `--url` CLI arguments to `monitor-toggle-ui.exe`; the native UI host now resolves the loopback URL strictly over the local Named Pipe (`\\.\pipe\monitor-toggle-v1`) and listens on a named focus event (`Local\MonitorToggleUiFocusEventV1`) so re-launching reconnects automatically if the daemon restarted.
- Fixed false stale-conflict errors when saving global settings after in-page profile changes, routed non-hotkey modal save errors to the dialog alert banner while keeping shortcut errors on the shortcut field, fixed `WINDOWPLACEMENT` primary-monitor workspace coordinate conversion in [src/window_restore.rs](file:///c:/src/Github/monitor-toggle/src/window_restore.rs), tracked modifiers directly in [src/input_guard.rs](file:///c:/src/Github/monitor-toggle/src/input_guard.rs) with in-place hotkey updates, returned explicit errors for blocked/redundant actions and `503` on connection caps in [src/web_server.rs](file:///c:/src/Github/monitor-toggle/src/web_server.rs), normalized default commented config creation, and deduplicated tray single-clicks.
- Promoted the release build into `target/release` and removed `target/review-release`. Passed **32 Rust tests**, **15 Playwright/axe tests**, `cargo fmt --check`, strict Clippy, and live release HTTP/IPC smoke checks.
