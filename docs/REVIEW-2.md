# Second review — November 25, 2026

This pass reviewed the follow-up changes from the first review, then traced input guarding, topology planning and recovery, profile actions, and asynchronous UI saves through their callers. UI decisions follow the installed design-review and application-interface-design skills, with the existing responsive and accessibility checks retained.

## Findings fixed

| Priority | Finding | Correction |
| --- | --- | --- |
| High | Starting the input guard with Ctrl/Alt/Shift held recorded both aggregate and physical modifier keys. Releasing the physical key left the aggregate key pressed, preventing bare Esc × 3. | Seed only left/right keys and test release back to an empty modifier state. |
| High | An input-guard thread starting after the startup timeout could install hooks without a returned stop handle. | Use a rendezvous handoff; publish guard state and install hooks only after the caller receives the thread ID. A failed handoff cannot clear another guard's state. |
| High | A missing monitor's model fallback could consume another monitor's exact identity; identical models could be assigned arbitrarily. | Reserve all exact identities first, then accept only a unique unmatched saved model and unique available target. |
| High | Greedy source assignment rejected feasible layouts, such as targets supporting `[0,1]` and `[0]`. | Find a complete source assignment, retaining active sources when possible and moving them only when needed for a feasible layout. |
| High | Mirrored paths emitted duplicate source-mode records during selective deactivation/recommit; snapshots lost the shared-source relationship. | Deduplicate mode records and validate their device identity. Capture snapshot-local source groups, intersect clone targets' supported sources, and emit one shared source mode per clone group. |
| High | Recovery decisions used exact keys only, bypassing connector fallback; final checks could report success despite a layout still needing restoration. | Compare live state with the restore planner's identity matching, normalized origin, modes and source groups, both before restoration and before reporting success. Case differences no longer imply drift. |
| Medium | Closing/reopening an editor during asynchronous shortcut validation could redirect a pending save; polling could replace settings during validation. | Hold one submission transaction through validation and persistence, discard earlier state responses, pause polling, disable editing/cancellation, show “Saving…”, and restore error focus afterward. |
| Medium | Running a profile by index could execute another client's replacement profile. | Send the rendered rule with the action; compare it with the normalized current rule under the controller lock before queueing the action. Stale actions return a refresh instruction. |

The shared-mode correction follows Microsoft's documented [SetDisplayConfig mode-table requirements](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setdisplayconfig#remarks).

## Compatibility

Existing snapshots still deserialize. Their absent `source_group` field keeps the former independent-source behavior; capture a fresh snapshot while the intended mirrored desktop is active to record clone relationships. The group numbers are local to each snapshot and do not persist adapter LUIDs. New captures remain readable by the previous deserializer, which ignores unknown fields.

Older API callers can still run profiles by index without a precondition; the bundled control panel always sends `expected_rule`. Profile comparison accepts equivalent legacy selector representations. Recovery outcomes now identify the current target key after connector fallback.

## Verification

- **44 Rust tests passed**, including 12 additional cases covering modifier release, exact/fallback identity precedence, ambiguous models, source allocation, clone mode deduplication, device identity validation, clone restoration, legacy snapshots, connector changes, normalized origins, unrelated displays and stale profile actions.
- **18 browser tests passed** across the full run and a focused rerun after correcting a new test's button label. New cases hold shortcut validation pending to exercise Escape/cancel behavior, polling, error focus and profile preconditions.
- Existing axe WCAG A/AA checks passed for the overview and modal in light/dark themes at 320, 768, 1024 and 1920 px.
- Formatting and strict Clippy passed.
- All three optimized executables were compiled in `target/review-release/release` during the initial review and then promoted directly to `target/release` (with `target/review-release` removed).
- A read-only native probe captured the current four-display desktop, planned four paths/four source modes, and confirmed that the live topology correctly requires no recovery. It did not apply a display configuration or write a snapshot.

## Validation limits

Automated tests and the native probe do not exercise physical sleep/wake, hooks consuming physical input, dock reconnection, or actual mirrored-display switching. Those driver-dependent checks remain necessary. Existing snapshots cannot recover clone metadata that was never recorded. Ambiguous identical-model replacements are intentionally skipped rather than guessed.

The running daemon was not replaced or restarted during this review. See the first [review report](REVIEW.md) for the broader hardware and session limitations.
