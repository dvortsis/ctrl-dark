//! LUID-independent topology snapshot serialization and dynamic-LUID restoration.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::mem::zeroed;
use std::path::Path;

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Devices::Display::{
    SetDisplayConfig, DISPLAYCONFIG_2DREGION, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_MODE_INFO_TYPE_TARGET,
    DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_RATIONAL, DISPLAYCONFIG_SOURCE_MODE,
    DISPLAYCONFIG_TARGET_MODE, DISPLAYCONFIG_VIDEO_SIGNAL_INFO, DISPLAYCONFIG_VIDEO_SIGNAL_INFO_0,
    SDC_ALLOW_CHANGES, SDC_APPLY, SDC_SAVE_TO_DATABASE, SDC_USE_SUPPLIED_DISPLAY_CONFIG,
};
use windows_sys::Win32::Foundation::POINTL;

use crate::display::ccd::{
    classify_display_role, query_adapter_path, query_available_targets, query_raw_ccd,
    query_target_metadata, source_mode_index, target_mode_index, AvailableCcdTarget,
    DISPLAYCONFIG_PATH_ACTIVE, DISPLAYCONFIG_PATH_MODE_IDX_INVALID,
};
use crate::display::{DisplayInfo, DisplayRole};
use crate::win_util::now_unix_ms;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedVideoSignalInfo {
    pub pixel_rate: u64,
    pub h_sync_num: u32,
    pub h_sync_den: u32,
    pub v_sync_num: u32,
    pub v_sync_den: u32,
    pub active_cx: u32,
    pub active_cy: u32,
    pub total_cx: u32,
    pub total_cy: u32,
    pub video_standard: u32,
    pub scanline_ordering: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedMonitorMode {
    pub stable_key: String,
    pub friendly_name: String,
    pub role: DisplayRole,
    pub edid_mfg: u16,
    pub edid_product: u16,
    #[serde(default)]
    pub preferred_source_id: u32,
    /// Snapshot-local source identity. Shared by mirrored targets; never a persisted LUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_group: Option<u32>,
    pub position_x: i32,
    pub position_y: i32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: i32,
    pub rotation: i32,
    pub scaling: i32,
    pub refresh_num: u32,
    pub refresh_den: u32,
    pub scanline_ordering: i32,
    pub target_video_signal: Option<SavedVideoSignalInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TopologySnapshot {
    pub captured_at_unix_ms: u64,
    pub monitors: Vec<SavedMonitorMode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub restored_keys: Vec<String>,
    pub newly_activated_keys: Vec<String>,
    pub skipped_unplugged_keys: Vec<String>,
    pub used_relaxed_timings: bool,
}

/// Captures the currently active Windows display topology without storing volatile LUIDs.
pub fn capture_active_snapshot() -> Result<TopologySnapshot, String> {
    let raw = query_raw_ccd(true)?;
    let mut monitors = Vec::new();
    let mut seen_keys = HashSet::new();
    let mut source_groups = HashMap::new();

    for path in &raw.paths {
        if (path.flags & DISPLAYCONFIG_PATH_ACTIVE) == 0 {
            continue;
        }
        let meta = query_target_metadata(
            path.targetInfo.adapterId,
            path.targetInfo.id,
            path.targetInfo.outputTechnology,
        );
        if !seen_keys.insert(meta.stable_key.clone()) {
            continue;
        }

        let adapter_path = query_adapter_path(path.targetInfo.adapterId);
        let role = classify_display_role(meta.output_technology, &adapter_path);

        let src_mode = source_mode_index(path)
            .and_then(|idx| raw.modes.get(idx))
            .filter(|m| m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE)
            .map(|m| unsafe { m.Anonymous.sourceMode })
            .ok_or_else(|| format!("Missing source mode for {}", meta.friendly_name))?;

        let target_signal = target_mode_index(path)
            .and_then(|idx| raw.modes.get(idx))
            .filter(|m| m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_TARGET)
            .map(|m| unsafe { m.Anonymous.targetMode.targetVideoSignalInfo })
            .map(|sig| SavedVideoSignalInfo {
                pixel_rate: sig.pixelRate,
                h_sync_num: sig.hSyncFreq.Numerator,
                h_sync_den: sig.hSyncFreq.Denominator,
                v_sync_num: sig.vSyncFreq.Numerator,
                v_sync_den: sig.vSyncFreq.Denominator,
                active_cx: sig.activeSize.cx,
                active_cy: sig.activeSize.cy,
                total_cx: sig.totalSize.cx,
                total_cy: sig.totalSize.cy,
                video_standard: unsafe { sig.Anonymous.videoStandard },
                scanline_ordering: sig.scanLineOrdering,
            });

        let next_group = source_groups.len() as u32;
        let source_group = *source_groups
            .entry((
                path.sourceInfo.adapterId.LowPart,
                path.sourceInfo.adapterId.HighPart,
                path.sourceInfo.id,
            ))
            .or_insert(next_group);
        monitors.push(SavedMonitorMode {
            stable_key: meta.stable_key,
            friendly_name: meta.friendly_name,
            role,
            edid_mfg: meta.edid_mfg,
            edid_product: meta.edid_product,
            preferred_source_id: path.sourceInfo.id,
            source_group: Some(source_group),
            position_x: src_mode.position.x,
            position_y: src_mode.position.y,
            width: src_mode.width,
            height: src_mode.height,
            pixel_format: src_mode.pixelFormat,
            rotation: path.targetInfo.rotation,
            scaling: path.targetInfo.scaling,
            refresh_num: path.targetInfo.refreshRate.Numerator,
            refresh_den: path.targetInfo.refreshRate.Denominator,
            scanline_ordering: path.targetInfo.scanLineOrdering,
            target_video_signal: target_signal,
        });
    }

    if monitors.is_empty() {
        return Err("No active monitors found while capturing snapshot".to_string());
    }

    monitors.sort_by(|a, b| {
        a.position_x
            .cmp(&b.position_x)
            .then_with(|| a.position_y.cmp(&b.position_y))
            .then_with(|| a.stable_key.cmp(&b.stable_key))
    });

    Ok(TopologySnapshot {
        captured_at_unix_ms: now_unix_ms(),
        monitors,
    })
}

/// Atomically writes `snapshot` to disk via temp file + `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`.
pub fn save_snapshot_atomic(snapshot: &TopologySnapshot, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed creating {}: {e}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(snapshot)
        .map_err(|e| format!("Failed serializing snapshot: {e}"))?;
    crate::win_util::write_atomic(path, json.as_bytes())
}

pub fn load_snapshot(path: &Path) -> Result<Option<TopologySnapshot>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        fs::read_to_string(path).map_err(|e| format!("Failed reading {}: {e}", path.display()))?;
    let snap = serde_json::from_str::<TopologySnapshot>(&raw)
        .map_err(|e| format!("Failed parsing {}: {e}", path.display()))?;
    Ok(Some(snap))
}

/// Compares live displays against the same identity matching, clone groups and
/// reanchored origin used by the restore planner (including connector fallback).
pub fn needs_topology_restore(
    snapshot: &TopologySnapshot,
    live_displays: &[DisplayInfo],
    available_targets: &[AvailableCcdTarget],
) -> bool {
    let Some(plan) = plan_restore(snapshot, available_targets, false) else {
        // A connected saved target with no feasible/unambiguous plan needs attention.
        // An entirely unplugged saved setup must not disturb unrelated new monitors.
        return snapshot.monitors.iter().any(|saved| {
            available_targets.iter().any(|target| {
                target.target_available
                    && (target.stable_key.eq_ignore_ascii_case(&saved.stable_key)
                        || ((saved.edid_mfg != 0 || saved.edid_product != 0)
                            && target.edid_mfg == saved.edid_mfg
                            && target.edid_product == saved.edid_product))
            })
        });
    };
    for path in &plan.paths {
        let Some(target) = available_targets.iter().find(|target| {
            target.target_id == path.targetInfo.id
                && target.adapter_id.LowPart == path.targetInfo.adapterId.LowPart
                && target.adapter_id.HighPart == path.targetInfo.adapterId.HighPart
        }) else {
            return true;
        };
        if !target.is_active || target.active_source_id != Some(path.sourceInfo.id) {
            return true;
        }
        let Some(live) = live_displays
            .iter()
            .find(|live| live.active && live.stable_key.eq_ignore_ascii_case(&target.stable_key))
        else {
            return true;
        };
        let mode = &plan.modes[source_mode_index(path).expect("planned source mode")];
        let source = unsafe { mode.Anonymous.sourceMode };
        if live.resolution != (source.width, source.height)
            || live.position != (source.position.x, source.position.y)
            || live.rotation != path.targetInfo.rotation as u32
        {
            return true;
        }
        let rate = path.targetInfo.refreshRate;
        if rate.Denominator > 0
            && live.refresh_hz > 0.0
            && (rate.Numerator as f64 / rate.Denominator as f64 - live.refresh_hz).abs() > 1.0
        {
            return true;
        }
    }
    false
}

pub struct PlannedRestore {
    pub paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    pub modes: Vec<DISPLAYCONFIG_MODE_INFO>,
    pub restored_keys: Vec<String>,
    pub newly_activated_keys: Vec<String>,
    pub skipped_unplugged_keys: Vec<String>,
}

/// Pure planning function that matches saved monitors to live CCD targets by `stable_key`
/// (with EDID mfg/product fallback), remaps volatile `LUID`s and adapter source IDs,
/// skips unplugged targets, and guarantees a `(0, 0)` primary origin.
pub fn plan_restore(
    snapshot: &TopologySnapshot,
    available_targets: &[AvailableCcdTarget],
    include_target_signal: bool,
) -> Option<PlannedRestore> {
    let mut matched: Vec<(&SavedMonitorMode, &AvailableCcdTarget)> = Vec::new();
    let mut used_targets: HashSet<(u32, i32, u32)> = HashSet::new();
    let mut skipped_unplugged_keys = Vec::new();

    // Reserve every exact identity before considering model-only fallbacks. Otherwise
    // an unplugged panel can steal a different same-model panel's exact target.
    let target_id =
        |t: &AvailableCcdTarget| (t.adapter_id.LowPart, t.adapter_id.HighPart, t.target_id);
    let mut assigned = vec![None; snapshot.monitors.len()];
    for (index, saved) in snapshot.monitors.iter().enumerate() {
        if let Some(target) = available_targets.iter().find(|t| {
            t.target_available
                && t.stable_key.eq_ignore_ascii_case(&saved.stable_key)
                && !used_targets.contains(&target_id(t))
        }) {
            used_targets.insert(target_id(target));
            assigned[index] = Some(target);
        }
    }
    for (index, saved) in snapshot.monitors.iter().enumerate() {
        if assigned[index].is_some() || (saved.edid_mfg == 0 && saved.edid_product == 0) {
            continue;
        }
        let unmatched_same_model = snapshot
            .monitors
            .iter()
            .enumerate()
            .filter(|(i, other)| {
                assigned[*i].is_none()
                    && other.edid_mfg == saved.edid_mfg
                    && other.edid_product == saved.edid_product
            })
            .count();
        let candidates: Vec<_> = available_targets
            .iter()
            .filter(|t| {
                t.target_available
                    && t.edid_mfg == saved.edid_mfg
                    && t.edid_product == saved.edid_product
                    && !used_targets.contains(&target_id(t))
            })
            .collect();
        // EDID manufacturer/product does not identify an individual unit.
        if unmatched_same_model == 1 && candidates.len() == 1 {
            assigned[index] = Some(candidates[0]);
            used_targets.insert(target_id(candidates[0]));
        }
    }
    for (saved, target) in snapshot.monitors.iter().zip(assigned) {
        if let Some(target) = target {
            matched.push((saved, target));
        } else {
            skipped_unplugged_keys.push(saved.stable_key.clone());
        }
    }

    if matched.is_empty() {
        return None;
    }

    // Re-anchor origin to (0, 0) if the original primary monitor is currently unplugged.
    let has_zero_origin = matched
        .iter()
        .any(|(s, _)| s.position_x == 0 && s.position_y == 0);
    let (dx, dy) = if has_zero_origin {
        (0, 0)
    } else {
        (matched[0].0.position_x, matched[0].0.position_y)
    };

    // Find a complete matching, preferring live and saved sources. A greedy choice
    // fails for targets with [0, 1] and [0], even though [1, 0] is valid.
    type Source = (u32, i32, u32);
    fn assign_source(
        index: usize,
        candidates: &[Vec<Source>],
        owners: &mut HashMap<Source, usize>,
        seen: &mut HashSet<Source>,
    ) -> bool {
        for source in &candidates[index] {
            if !seen.contains(source) && !owners.contains_key(source) {
                owners.insert(*source, index);
                return true;
            }
        }
        for source in &candidates[index] {
            if !seen.insert(*source) {
                continue;
            }
            if let Some(owner) = owners.get(source).copied() {
                if assign_source(owner, candidates, owners, seen) {
                    owners.insert(*source, index);
                    return true;
                }
            }
        }
        false
    }
    let candidates: Vec<Vec<Source>> = matched
        .iter()
        .map(|(saved, target)| {
            let mut ids = Vec::new();
            if target.is_active {
                ids.extend(target.active_source_id);
            }
            if target
                .available_source_ids
                .contains(&saved.preferred_source_id)
            {
                ids.push(saved.preferred_source_id);
            }
            let mut available = target.available_source_ids.clone();
            available.sort_unstable();
            ids.extend(available);
            let mut seen = HashSet::new();
            ids.into_iter()
                .filter(|id| seen.insert(*id))
                .map(|id| (target.adapter_id.LowPart, target.adapter_id.HighPart, id))
                .collect()
        })
        .collect();
    // Allocate per shared source, intersecting every clone target's supported IDs.
    // Older snapshots have no group metadata and retain independent-source behavior.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut group_ids = HashMap::new();
    let mut path_groups = Vec::new();
    for (index, (saved, target)) in matched.iter().enumerate() {
        let group = if let Some(id) = saved.source_group {
            let key = (target.adapter_id.LowPart, target.adapter_id.HighPart, id);
            *group_ids.entry(key).or_insert_with(|| {
                groups.push(Vec::new());
                groups.len() - 1
            })
        } else {
            groups.push(Vec::new());
            groups.len() - 1
        };
        groups[group].push(index);
        path_groups.push(group);
    }
    let mut group_candidates = Vec::new();
    for members in &groups {
        let first = matched[members[0]].0;
        let mut sources = candidates[members[0]].clone();
        for &member in &members[1..] {
            let saved = matched[member].0;
            if (
                saved.position_x,
                saved.position_y,
                saved.width,
                saved.height,
                saved.pixel_format,
            ) != (
                first.position_x,
                first.position_y,
                first.width,
                first.height,
                first.pixel_format,
            ) {
                return None;
            }
            sources.retain(|source| candidates[member].contains(source));
        }
        sources.sort_by_key(|source| {
            !members.iter().any(|&member| {
                let target = matched[member].1;
                target.is_active && target.active_source_id == Some(source.2)
            })
        });
        group_candidates.push(sources);
    }
    let mut owners = HashMap::new();
    let mut order: Vec<_> = (0..groups.len()).collect();
    order.sort_by_key(|&group| !groups[group].iter().any(|&i| matched[i].1.is_active));
    for group in order {
        if !assign_source(group, &group_candidates, &mut owners, &mut HashSet::new()) {
            return None;
        }
    }
    let mut assigned_sources = vec![0; groups.len()];
    for ((_, _, source), group) in owners {
        assigned_sources[group] = source;
    }

    let mut source_mode_indices = HashMap::new();
    let mut paths = Vec::with_capacity(matched.len());
    let mut modes = Vec::with_capacity(matched.len() * 2);
    let mut restored_keys = Vec::with_capacity(matched.len());
    let mut newly_activated_keys = Vec::with_capacity(matched.len());

    for (idx, (saved, target)) in matched.into_iter().enumerate() {
        let group = path_groups[idx];
        let chosen_source_id = assigned_sources[group];

        let src_mode_idx = *source_mode_indices.entry(group).or_insert_with(|| {
            let index = modes.len() as u32;
            let mut src_mode: DISPLAYCONFIG_MODE_INFO = unsafe { zeroed() };
            src_mode.infoType = DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE;
            src_mode.id = chosen_source_id;
            src_mode.adapterId = target.adapter_id;
            src_mode.Anonymous.sourceMode = DISPLAYCONFIG_SOURCE_MODE {
                width: saved.width,
                height: saved.height,
                pixelFormat: saved.pixel_format,
                position: POINTL {
                    x: saved.position_x - dx,
                    y: saved.position_y - dy,
                },
            };
            modes.push(src_mode);

            index
        });

        let tgt_mode_idx = if include_target_signal {
            if let Some(sig) = &saved.target_video_signal {
                let idx = modes.len() as u32;
                let mut tgt_mode: DISPLAYCONFIG_MODE_INFO = unsafe { zeroed() };
                tgt_mode.infoType = DISPLAYCONFIG_MODE_INFO_TYPE_TARGET;
                tgt_mode.id = target.target_id;
                tgt_mode.adapterId = target.adapter_id;
                tgt_mode.Anonymous.targetMode = DISPLAYCONFIG_TARGET_MODE {
                    targetVideoSignalInfo: DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
                        pixelRate: sig.pixel_rate,
                        hSyncFreq: DISPLAYCONFIG_RATIONAL {
                            Numerator: sig.h_sync_num,
                            Denominator: sig.h_sync_den,
                        },
                        vSyncFreq: DISPLAYCONFIG_RATIONAL {
                            Numerator: sig.v_sync_num,
                            Denominator: sig.v_sync_den,
                        },
                        activeSize: DISPLAYCONFIG_2DREGION {
                            cx: sig.active_cx,
                            cy: sig.active_cy,
                        },
                        totalSize: DISPLAYCONFIG_2DREGION {
                            cx: sig.total_cx,
                            cy: sig.total_cy,
                        },
                        Anonymous: DISPLAYCONFIG_VIDEO_SIGNAL_INFO_0 {
                            videoStandard: sig.video_standard,
                        },
                        scanLineOrdering: sig.scanline_ordering,
                    },
                };
                modes.push(tgt_mode);
                idx
            } else {
                DISPLAYCONFIG_PATH_MODE_IDX_INVALID
            }
        } else {
            DISPLAYCONFIG_PATH_MODE_IDX_INVALID
        };

        let mut path: DISPLAYCONFIG_PATH_INFO = unsafe { zeroed() };
        path.flags = DISPLAYCONFIG_PATH_ACTIVE;
        path.sourceInfo.adapterId = target.adapter_id;
        path.sourceInfo.id = chosen_source_id;
        path.sourceInfo.Anonymous.modeInfoIdx = src_mode_idx;

        path.targetInfo.adapterId = target.adapter_id;
        path.targetInfo.id = target.target_id;
        path.targetInfo.Anonymous.modeInfoIdx = tgt_mode_idx;
        path.targetInfo.outputTechnology = target.output_technology;
        path.targetInfo.rotation = saved.rotation;
        path.targetInfo.scaling = saved.scaling;
        path.targetInfo.refreshRate = DISPLAYCONFIG_RATIONAL {
            Numerator: saved.refresh_num,
            Denominator: saved.refresh_den,
        };
        path.targetInfo.scanLineOrdering = saved.scanline_ordering;
        path.targetInfo.targetAvailable = 1;

        if !target.is_active {
            newly_activated_keys.push(target.stable_key.clone());
        }
        restored_keys.push(target.stable_key.clone());
        paths.push(path);
    }

    Some(PlannedRestore {
        paths,
        modes,
        restored_keys,
        newly_activated_keys,
        skipped_unplugged_keys,
    })
}

/// Applies `snapshot` to live CCD targets with dynamic LUID remapping.
pub fn apply_snapshot(
    snapshot: &TopologySnapshot,
    save_to_db: bool,
) -> Result<ApplyOutcome, String> {
    let available = query_available_targets()?;
    apply_snapshot_with_targets(snapshot, &available, save_to_db)
}

/// Applies `snapshot` against already-queried `AvailableCcdTarget`s in a single `SetDisplayConfig`
/// call (falling back to relaxed timings only if exact signal timings are rejected).
pub fn apply_snapshot_with_targets(
    snapshot: &TopologySnapshot,
    available: &[AvailableCcdTarget],
    save_to_db: bool,
) -> Result<ApplyOutcome, String> {
    let db_flag = if save_to_db { SDC_SAVE_TO_DATABASE } else { 0 };

    for (relaxed, include_signal) in [(false, true), (true, false)] {
        let Some(plan) = plan_restore(snapshot, available, include_signal) else {
            return Err(
                "No feasible, unambiguous saved topology could be matched to the connected displays".into(),
            );
        };

        let apply_rc = unsafe {
            SetDisplayConfig(
                plan.paths.len() as u32,
                plan.paths.as_ptr(),
                plan.modes.len() as u32,
                plan.modes.as_ptr(),
                SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | db_flag,
            )
        };
        if apply_rc == 0 {
            return Ok(ApplyOutcome {
                restored_keys: plan.restored_keys,
                newly_activated_keys: plan.newly_activated_keys,
                skipped_unplugged_keys: plan.skipped_unplugged_keys,
                used_relaxed_timings: relaxed,
            });
        }
        if relaxed {
            return Err(format!(
                "SetDisplayConfig(apply_snapshot) failed with error {apply_rc}"
            ));
        }
    }

    Err("apply_snapshot failed unexpectedly".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_for(saved: &SavedMonitorMode, key: &str, position: (i32, i32)) -> DisplayInfo {
        DisplayInfo {
            friendly_name: saved.friendly_name.clone(),
            stable_key: key.into(),
            gdi_name: String::new(),
            adapter_path: String::new(),
            edid_code: String::new(),
            role: saved.role,
            active: true,
            available: true,
            is_primary: position == (0, 0),
            position,
            resolution: (saved.width, saved.height),
            refresh_hz: saved.refresh_num as f64 / saved.refresh_den as f64,
            rotation: saved.rotation as u32,
        }
    }

    #[test]
    fn recovery_detection_uses_connector_fallback_and_reanchored_origin() {
        let snap = sample_snapshot();
        let saved = &snap.monitors[1];
        let mut target = target_for(saved, 20, vec![0]);
        target.stable_key = "reconnected-connector".into();
        assert!(needs_topology_restore(
            &snap,
            &[],
            std::slice::from_ref(&target)
        ));
        target.is_active = true;
        target.active_source_id = Some(0);
        let mut live = live_for(saved, &target.stable_key.to_uppercase(), (0, 0));
        assert!(!needs_topology_restore(
            &snap,
            &[live.clone()],
            std::slice::from_ref(&target)
        ));
        live.position = (100, 0);
        assert!(needs_topology_restore(&snap, &[live], &[target]));
    }

    #[test]
    fn recovery_detection_does_not_modify_unrelated_monitors() {
        let snap = sample_snapshot();
        let mut target = target_for(&snap.monitors[0], 10, vec![0]);
        target.stable_key = "unrelated".into();
        target.edid_product = 0;
        target.edid_mfg = 0;
        assert!(!needs_topology_restore(&snap, &[], &[target]));
    }

    fn target_for(saved: &SavedMonitorMode, id: u32, sources: Vec<u32>) -> AvailableCcdTarget {
        AvailableCcdTarget {
            adapter_id: LUID {
                LowPart: 1,
                HighPart: 0,
            },
            target_id: id,
            active_source_id: None,
            available_source_ids: sources,
            stable_key: saved.stable_key.clone(),
            friendly_name: saved.friendly_name.clone(),
            edid_mfg: saved.edid_mfg,
            edid_product: saved.edid_product,
            output_technology: 10,
            target_available: true,
            is_active: false,
        }
    }

    #[test]
    fn fallback_matching_never_steals_an_exact_monitor_match() {
        let mut snap = sample_snapshot();
        snap.monitors[1].edid_mfg = snap.monitors[0].edid_mfg;
        snap.monitors[1].edid_product = snap.monitors[0].edid_product;
        let exact = target_for(&snap.monitors[1], 20, vec![0, 1]);
        let plan = plan_restore(&snap, std::slice::from_ref(&exact), false).unwrap();
        assert_eq!(
            plan.restored_keys,
            vec![snap.monitors[1].stable_key.clone()]
        );
        assert_eq!(
            plan.skipped_unplugged_keys,
            vec![snap.monitors[0].stable_key.clone()]
        );

        let mut fallback = target_for(&snap.monitors[0], 10, vec![0, 1]);
        fallback.stable_key = "new-connector".into();
        let plan = plan_restore(&snap, &[exact, fallback], false).unwrap();
        assert_eq!(plan.paths[0].targetInfo.id, 10);
        assert_eq!(plan.paths[1].targetInfo.id, 20);
    }

    #[test]
    fn ambiguous_same_model_monitors_are_not_arbitrarily_reassigned() {
        let mut snap = sample_snapshot();
        snap.monitors[1].edid_mfg = snap.monitors[0].edid_mfg;
        snap.monitors[1].edid_product = snap.monitors[0].edid_product;
        let mut first = target_for(&snap.monitors[0], 10, vec![0, 1]);
        let mut second = target_for(&snap.monitors[1], 20, vec![0, 1]);
        first.stable_key = "new-first".into();
        second.stable_key = "new-second".into();
        assert!(plan_restore(&snap, &[first, second], false).is_none());
    }

    #[test]
    fn source_assignment_finds_a_feasible_layout_beyond_greedy_choices() {
        let mut snap = sample_snapshot();
        snap.monitors[0].preferred_source_id = 0;
        snap.monitors[1].preferred_source_id = 0;
        let targets = [
            target_for(&snap.monitors[0], 10, vec![0, 1]),
            target_for(&snap.monitors[1], 20, vec![0]),
        ];
        let plan = plan_restore(&snap, &targets, false).unwrap();
        assert_eq!(plan.paths[0].sourceInfo.id, 1);
        assert_eq!(plan.paths[1].sourceInfo.id, 0);
    }

    #[test]
    fn mirrored_targets_share_one_supported_source_and_mode() {
        let mut snap = sample_snapshot();
        snap.monitors[0].source_group = Some(7);
        snap.monitors[1] = SavedMonitorMode {
            stable_key: "mirror-target".into(),
            ..snap.monitors[0].clone()
        };
        let mut targets = [
            target_for(&snap.monitors[0], 10, vec![0, 1]),
            target_for(&snap.monitors[1], 20, vec![0, 1]),
        ];
        targets[1].is_active = true;
        targets[1].active_source_id = Some(1);
        let plan = plan_restore(&snap, &targets, false).unwrap();
        assert_eq!(plan.paths.len(), 2);
        assert_eq!(plan.modes.len(), 1);
        for path in &plan.paths {
            assert_eq!(path.sourceInfo.id, 1);
            assert_eq!(source_mode_index(path), Some(0));
        }
        let live: Vec<_> = snap
            .monitors
            .iter()
            .map(|m| live_for(m, &m.stable_key, (0, 0)))
            .collect();
        targets[0].is_active = true;
        targets[0].active_source_id = Some(0);
        assert!(needs_topology_restore(&snap, &live, &targets));
        targets[0].active_source_id = Some(1);
        assert!(!needs_topology_restore(&snap, &live, &targets));
        targets[0].is_active = false;
        targets[0].active_source_id = None;
        targets[0].available_source_ids = vec![0];
        targets[1].available_source_ids = vec![1];
        assert!(plan_restore(&snap, &targets, false).is_none());
    }

    #[test]
    fn clone_groups_do_not_merge_sources_on_different_adapters() {
        let mut snap = sample_snapshot();
        for monitor in &mut snap.monitors {
            monitor.source_group = Some(0);
        }
        let mut targets = [
            target_for(&snap.monitors[0], 10, vec![0]),
            target_for(&snap.monitors[1], 20, vec![0]),
        ];
        targets[1].adapter_id.LowPart = 2;
        let plan = plan_restore(&snap, &targets, false).unwrap();
        assert_eq!(plan.modes.len(), 2);
        assert_ne!(
            source_mode_index(&plan.paths[0]),
            source_mode_index(&plan.paths[1])
        );
    }

    #[test]
    fn legacy_snapshots_load_without_clone_metadata() {
        let snap = sample_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("source_group"));
        assert_eq!(
            serde_json::from_str::<TopologySnapshot>(&json).unwrap(),
            snap
        );
    }
    use windows_sys::Win32::Foundation::LUID;

    fn sample_snapshot() -> TopologySnapshot {
        TopologySnapshot {
            captured_at_unix_ms: 1_700_000_000_000,
            monitors: vec![
                SavedMonitorMode {
                    stable_key: "display#len8aa5#uid8388688".to_string(),
                    friendly_name: "Internal Display (LEN8AA5)".to_string(),
                    role: DisplayRole::Internal,
                    edid_mfg: 0x30AE,
                    edid_product: 0x8AA5,
                    preferred_source_id: 0,
                    source_group: None,
                    position_x: 0,
                    position_y: 0,
                    width: 2560,
                    height: 1600,
                    pixel_format: 3,
                    rotation: 1,
                    scaling: 1,
                    refresh_num: 60000,
                    refresh_den: 1000,
                    scanline_ordering: 1,
                    target_video_signal: None,
                },
                SavedMonitorMode {
                    stable_key: "display#aus1641#uid256".to_string(),
                    friendly_name: "ASUS MB16AC".to_string(),
                    role: DisplayRole::UsbIndirect,
                    edid_mfg: 0x06B3,
                    edid_product: 0x1641,
                    preferred_source_id: 0,
                    source_group: None,
                    position_x: 2560,
                    position_y: 0,
                    width: 1920,
                    height: 1080,
                    pixel_format: 3,
                    rotation: 1,
                    scaling: 1,
                    refresh_num: 60000,
                    refresh_den: 1000,
                    scanline_ordering: 1,
                    target_video_signal: None,
                },
            ],
        }
    }

    #[test]
    fn refuses_to_invent_unavailable_source_ids() {
        let snap = sample_snapshot();
        let targets: Vec<_> = snap
            .monitors
            .iter()
            .enumerate()
            .map(|(index, monitor)| AvailableCcdTarget {
                adapter_id: LUID {
                    LowPart: 1,
                    HighPart: 0,
                },
                target_id: index as u32,
                active_source_id: None,
                available_source_ids: vec![0],
                stable_key: monitor.stable_key.clone(),
                friendly_name: monitor.friendly_name.clone(),
                edid_mfg: monitor.edid_mfg,
                edid_product: monitor.edid_product,
                output_technology: 10,
                target_available: true,
                is_active: false,
            })
            .collect();
        assert!(plan_restore(&snap, &targets, false).is_none());
    }

    #[test]
    fn remaps_volatile_adapter_luids_and_assigns_unique_sources() {
        let snap = sample_snapshot();
        let new_usb_luid = LUID {
            LowPart: 0x9940,
            HighPart: 0,
        };
        let intel_luid = LUID {
            LowPart: 0x1200,
            HighPart: 0,
        };

        let targets = vec![
            AvailableCcdTarget {
                adapter_id: intel_luid,
                target_id: 8388688,
                active_source_id: Some(0),
                available_source_ids: vec![0, 1],
                stable_key: "display#len8aa5#uid8388688".to_string(),
                friendly_name: "Internal".to_string(),
                edid_mfg: 0x30AE,
                edid_product: 0x8AA5,
                output_technology: -2147483648,
                target_available: true,
                is_active: true,
            },
            AvailableCcdTarget {
                adapter_id: new_usb_luid,
                target_id: 256,
                active_source_id: None,
                available_source_ids: vec![0],
                stable_key: "display#aus1641#uid256".to_string(),
                friendly_name: "ASUS MB16AC".to_string(),
                edid_mfg: 0x06B3,
                edid_product: 0x1641,
                output_technology: 4,
                target_available: true,
                is_active: false,
            },
        ];

        let plan = plan_restore(&snap, &targets, false).expect("plan");
        assert_eq!(plan.paths.len(), 2);
        assert!(plan.skipped_unplugged_keys.is_empty());
        assert_eq!(plan.paths[1].targetInfo.adapterId.LowPart, 0x9940);
        assert_eq!(plan.paths[1].sourceInfo.adapterId.LowPart, 0x9940);
        assert_eq!(plan.paths[1].sourceInfo.id, 0);
        let src_idx = unsafe { plan.paths[1].sourceInfo.Anonymous.modeInfoIdx } as usize;
        let pos = unsafe { plan.modes[src_idx].Anonymous.sourceMode.position };
        assert_eq!((pos.x, pos.y), (2560, 0));
    }

    #[test]
    fn skips_unplugged_monitors_and_reanchors_origin() {
        let mut snap = sample_snapshot();
        // Suppose primary was at (0,0) and unplugged; second monitor was at (2560, 0).
        snap.monitors[0].position_x = 0;
        let targets = vec![AvailableCcdTarget {
            adapter_id: LUID {
                LowPart: 0x9940,
                HighPart: 0,
            },
            target_id: 256,
            active_source_id: None,
            available_source_ids: vec![0],
            stable_key: "display#aus1641#uid256".to_string(),
            friendly_name: "ASUS MB16AC".to_string(),
            edid_mfg: 0x06B3,
            edid_product: 0x1641,
            output_technology: 4,
            target_available: true,
            is_active: false,
        }];

        let plan = plan_restore(&snap, &targets, false).expect("plan");
        assert_eq!(plan.paths.len(), 1);
        assert_eq!(
            plan.skipped_unplugged_keys,
            vec!["display#len8aa5#uid8388688"]
        );
        let src_idx = unsafe { plan.paths[0].sourceInfo.Anonymous.modeInfoIdx } as usize;
        let pos = unsafe { plan.modes[src_idx].Anonymous.sourceMode.position };
        assert_eq!((pos.x, pos.y), (0, 0));
    }

    #[test]
    fn preserves_active_and_preferred_source_ids_on_restore() {
        let intel_luid = LUID {
            LowPart: 0x1200,
            HighPart: 0,
        };
        let snap = TopologySnapshot {
            captured_at_unix_ms: 1_700_000_000_000,
            monitors: vec![
                // Sorted leftmost first (J560T09 at -6,-1600), but active on source_id=1.
                SavedMonitorMode {
                    stable_key: "display#boe2775#uid8261".to_string(),
                    friendly_name: "J560T09".to_string(),
                    role: DisplayRole::External,
                    edid_mfg: 0x09E5,
                    edid_product: 0x2775,
                    preferred_source_id: 1,
                    source_group: None,
                    position_x: -6,
                    position_y: -1600,
                    width: 2560,
                    height: 1600,
                    pixel_format: 3,
                    rotation: 1,
                    scaling: 1,
                    refresh_num: 120000,
                    refresh_den: 1000,
                    scanline_ordering: 1,
                    target_video_signal: None,
                },
                // P27h-20 at (0,0), active on source_id=2.
                SavedMonitorMode {
                    stable_key: "display#len61e9#uid12613".to_string(),
                    friendly_name: "P27h-20".to_string(),
                    role: DisplayRole::External,
                    edid_mfg: 0x30AE,
                    edid_product: 0x61E9,
                    preferred_source_id: 2,
                    source_group: None,
                    position_x: 0,
                    position_y: 0,
                    width: 2560,
                    height: 1440,
                    pixel_format: 3,
                    rotation: 1,
                    scaling: 1,
                    refresh_num: 60000,
                    refresh_den: 1000,
                    scanline_ordering: 1,
                    target_video_signal: None,
                },
                // Internal panel currently OFF, preferred_source_id=0.
                SavedMonitorMode {
                    stable_key: "display#len8aa5#uid8388688".to_string(),
                    friendly_name: "Internal Display (LEN8AA5)".to_string(),
                    role: DisplayRole::Internal,
                    edid_mfg: 0x30AE,
                    edid_product: 0x8AA5,
                    preferred_source_id: 0,
                    source_group: None,
                    position_x: 2560,
                    position_y: -158,
                    width: 2560,
                    height: 1600,
                    pixel_format: 3,
                    rotation: 1,
                    scaling: 1,
                    refresh_num: 60000,
                    refresh_den: 1000,
                    scanline_ordering: 1,
                    target_video_signal: None,
                },
            ],
        };

        let targets = vec![
            AvailableCcdTarget {
                adapter_id: intel_luid,
                target_id: 8261,
                active_source_id: Some(1),
                available_source_ids: vec![0, 1, 2, 3],
                stable_key: "display#boe2775#uid8261".to_string(),
                friendly_name: "J560T09".to_string(),
                edid_mfg: 0x09E5,
                edid_product: 0x2775,
                output_technology: 10,
                target_available: true,
                is_active: true,
            },
            AvailableCcdTarget {
                adapter_id: intel_luid,
                target_id: 12613,
                active_source_id: Some(2),
                available_source_ids: vec![0, 1, 2, 3],
                stable_key: "display#len61e9#uid12613".to_string(),
                friendly_name: "P27h-20".to_string(),
                edid_mfg: 0x30AE,
                edid_product: 0x61E9,
                output_technology: 10,
                target_available: true,
                is_active: true,
            },
            AvailableCcdTarget {
                adapter_id: intel_luid,
                target_id: 8388688,
                active_source_id: None,
                available_source_ids: vec![0, 1, 2, 3],
                stable_key: "display#len8aa5#uid8388688".to_string(),
                friendly_name: "Internal Display (LEN8AA5)".to_string(),
                edid_mfg: 0x30AE,
                edid_product: 0x8AA5,
                output_technology: -2147483648,
                target_available: true,
                is_active: false,
            },
        ];

        let plan = plan_restore(&snap, &targets, false).expect("plan");
        assert_eq!(plan.paths[0].sourceInfo.id, 1);
        assert_eq!(plan.paths[1].sourceInfo.id, 2);
        assert_eq!(plan.paths[2].sourceInfo.id, 0);
        assert_eq!(
            plan.newly_activated_keys,
            vec!["display#len8aa5#uid8388688"]
        );
    }
}
