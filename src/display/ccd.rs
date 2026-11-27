//! Win32 Connecting and Configuring Displays (CCD) & GDI scanout helpers.

use std::collections::{HashMap, HashSet};
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::time::Instant;

use windows_sys::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig, SetDisplayConfig,
    DISPLAYCONFIG_ADAPTER_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
    DISPLAYCONFIG_MODE_INFO_TYPE_TARGET, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL,
    DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME,
    QDC_ALL_PATHS, QDC_ONLY_ACTIVE_PATHS, SDC_ALLOW_CHANGES, SDC_APPLY, SDC_FORCE_MODE_ENUMERATION,
    SDC_USE_SUPPLIED_DISPLAY_CONFIG,
};
use windows_sys::Win32::Devices::Display::{
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED,
};
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LUID};
use windows_sys::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplaySettingsExW, CDS_RESET, DEVMODEW, ENUM_CURRENT_SETTINGS,
};

use crate::display::{DisplayInfo, DisplayRole};
use crate::win_util::{
    decode_edid_manufacturer, extract_edid_token, from_wide_slice, normalize_stable_monitor_key,
    to_wide_null,
};

pub const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x0000_0001;
pub const DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE: u32 = 0x0000_0008;
pub const DISPLAYCONFIG_PATH_MODE_IDX_INVALID: u32 = 0xFFFF_FFFF;

const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

#[derive(Clone)]
pub struct RawCcdState {
    pub paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    pub modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

#[derive(Clone)]
pub struct AvailableCcdTarget {
    pub adapter_id: LUID,
    pub target_id: u32,
    pub active_source_id: Option<u32>,
    pub available_source_ids: Vec<u32>,
    pub stable_key: String,
    pub friendly_name: String,
    pub edid_mfg: u16,
    pub edid_product: u16,
    pub output_technology: i32,
    pub target_available: bool,
    pub is_active: bool,
}

#[derive(Debug, Clone)]
pub struct TargetDeviceMetadata {
    pub friendly_name: String,
    pub monitor_device_path: String,
    pub edid_mfg: u16,
    pub edid_product: u16,
    pub connector_instance: u32,
    pub output_technology: i32,
    pub edid_code: String,
    pub stable_key: String,
}

/// Queries raw Windows CCD paths and modes.
pub fn query_raw_ccd(active_only: bool) -> Result<RawCcdState, String> {
    let flags = if active_only {
        QDC_ONLY_ACTIVE_PATHS
    } else {
        QDC_ALL_PATHS
    };

    for _attempt in 0..4 {
        let mut num_paths = 0u32;
        let mut num_modes = 0u32;
        let err = unsafe { GetDisplayConfigBufferSizes(flags, &mut num_paths, &mut num_modes) };
        if err != ERROR_SUCCESS {
            return Err(format!(
                "GetDisplayConfigBufferSizes failed: Win32 error {err}"
            ));
        }

        let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> = vec![unsafe { zeroed() }; num_paths as usize];
        let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> = vec![unsafe { zeroed() }; num_modes as usize];

        let qerr = unsafe {
            QueryDisplayConfig(
                flags,
                &mut num_paths,
                paths.as_mut_ptr(),
                &mut num_modes,
                modes.as_mut_ptr(),
                null_mut(),
            )
        };
        if qerr == ERROR_SUCCESS {
            paths.truncate(num_paths as usize);
            modes.truncate(num_modes as usize);
            return Ok(RawCcdState { paths, modes });
        }
        if qerr != ERROR_INSUFFICIENT_BUFFER {
            return Err(format!("QueryDisplayConfig failed: Win32 error {qerr}"));
        }
    }

    Err("QueryDisplayConfig failed repeatedly with ERROR_INSUFFICIENT_BUFFER".to_string())
}

pub fn query_target_metadata(
    adapter_id: LUID,
    target_id: u32,
    fallback_output_tech: i32,
) -> TargetDeviceMetadata {
    let mut pkt: DISPLAYCONFIG_TARGET_DEVICE_NAME = unsafe { zeroed() };
    pkt.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
        size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
        adapterId: adapter_id,
        id: target_id,
    };
    let rc = unsafe { DisplayConfigGetDeviceInfo(&mut pkt.header) };
    let (raw_friendly, device_path, edid_mfg, edid_product, connector_instance, out_tech) =
        if rc == 0 {
            (
                from_wide_slice(&pkt.monitorFriendlyDeviceName),
                from_wide_slice(&pkt.monitorDevicePath),
                pkt.edidManufactureId,
                pkt.edidProductCodeId,
                pkt.connectorInstance,
                pkt.outputTechnology,
            )
        } else {
            (
                String::new(),
                String::new(),
                0,
                0,
                target_id,
                fallback_output_tech,
            )
        };

    let edid_code = extract_edid_token(&device_path).unwrap_or_else(|| {
        let mfg = decode_edid_manufacturer(edid_mfg);
        if !mfg.is_empty() || edid_product != 0 {
            format!("{mfg}{edid_product:04X}")
        } else {
            format!("ID{target_id}")
        }
    });

    let friendly_name = if !raw_friendly.is_empty() {
        raw_friendly
    } else if classify_display_role(out_tech, "") == DisplayRole::Internal {
        format!("Internal Display ({edid_code})")
    } else {
        format!("External Display ({edid_code})")
    };

    let stable_key =
        normalize_stable_monitor_key(&device_path, edid_mfg, edid_product, connector_instance);

    TargetDeviceMetadata {
        friendly_name,
        monitor_device_path: device_path,
        edid_mfg,
        edid_product,
        connector_instance,
        output_technology: out_tech,
        edid_code,
        stable_key,
    }
}

pub fn query_source_gdi_name(adapter_id: LUID, source_id: u32) -> String {
    let mut pkt: DISPLAYCONFIG_SOURCE_DEVICE_NAME = unsafe { zeroed() };
    pkt.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
        size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
        adapterId: adapter_id,
        id: source_id,
    };
    let rc = unsafe { DisplayConfigGetDeviceInfo(&mut pkt.header) };
    if rc == 0 {
        from_wide_slice(&pkt.viewGdiDeviceName)
    } else {
        String::new()
    }
}

pub fn query_adapter_path(adapter_id: LUID) -> String {
    let mut pkt: DISPLAYCONFIG_ADAPTER_NAME = unsafe { zeroed() };
    pkt.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
        size: size_of::<DISPLAYCONFIG_ADAPTER_NAME>() as u32,
        adapterId: adapter_id,
        id: 0,
    };
    let rc = unsafe { DisplayConfigGetDeviceInfo(&mut pkt.header) };
    if rc == 0 {
        from_wide_slice(&pkt.adapterDevicePath)
    } else {
        String::new()
    }
}

pub fn classify_display_role(output_technology: i32, adapter_path: &str) -> DisplayRole {
    if matches!(
        output_technology,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED
    ) {
        return DisplayRole::Internal;
    }
    let lower = adapter_path.to_ascii_lowercase();
    if output_technology == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED
        || lower.contains("usb#")
        || lower.contains("vid_17e9")
        || lower.contains("indirect")
        || lower.contains("displaylink")
    {
        DisplayRole::UsbIndirect
    } else {
        DisplayRole::External
    }
}

/// Extracts source mode index from `DISPLAYCONFIG_PATH_INFO` (handles virtual mode flag safely).
pub fn source_mode_index(path: &DISPLAYCONFIG_PATH_INFO) -> Option<usize> {
    let raw = unsafe { path.sourceInfo.Anonymous.modeInfoIdx };
    let idx = if (path.flags & DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE) != 0 {
        // High 16 bits hold sourceModeInfoIdx; low bits hold cloneGroupId.
        let low = raw >> 16;
        if low == 0xFFFF {
            return None;
        }
        low
    } else {
        if raw == DISPLAYCONFIG_PATH_MODE_IDX_INVALID {
            return None;
        }
        raw
    };
    Some(idx as usize)
}

/// Extracts target mode index from `DISPLAYCONFIG_PATH_INFO`.
pub fn target_mode_index(path: &DISPLAYCONFIG_PATH_INFO) -> Option<usize> {
    let raw = unsafe { path.targetInfo.Anonymous.modeInfoIdx };
    let idx = if (path.flags & DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE) != 0 {
        let low = raw >> 16;
        if low == 0xFFFF {
            return None;
        }
        low
    } else {
        if raw == DISPLAYCONFIG_PATH_MODE_IDX_INVALID {
            return None;
        }
        raw
    };
    Some(idx as usize)
}

/// Queries live displays. When `include_inactive` is true, deduplicates `QDC_ALL_PATHS`
/// by `(adapterId, targetId)` so each physical/connected monitor appears once.
pub fn query_displays(include_inactive: bool) -> Result<Vec<DisplayInfo>, String> {
    let raw = query_raw_ccd(!include_inactive)?;
    let mut adapter_cache: HashMap<(u32, i32), String> = HashMap::new();
    let mut best_by_target: HashMap<(u32, i32, u32), DISPLAYCONFIG_PATH_INFO> = HashMap::new();

    for &path in &raw.paths {
        let is_active = (path.flags & DISPLAYCONFIG_PATH_ACTIVE) != 0;
        let is_avail = path.targetInfo.targetAvailable != 0;
        if !is_active && (!include_inactive || !is_avail) {
            continue;
        }
        let key = (
            path.targetInfo.adapterId.LowPart,
            path.targetInfo.adapterId.HighPart,
            path.targetInfo.id,
        );
        match best_by_target.get(&key) {
            None => {
                best_by_target.insert(key, path);
            }
            Some(existing) => {
                let existing_active = (existing.flags & DISPLAYCONFIG_PATH_ACTIVE) != 0;
                if is_active && !existing_active {
                    best_by_target.insert(key, path);
                }
            }
        }
    }

    let mut displays = Vec::with_capacity(best_by_target.len());
    for path in best_by_target.into_values() {
        let adapter_key = (
            path.targetInfo.adapterId.LowPart,
            path.targetInfo.adapterId.HighPart,
        );
        let adapter_path = adapter_cache
            .entry(adapter_key)
            .or_insert_with(|| query_adapter_path(path.targetInfo.adapterId))
            .clone();

        let meta = query_target_metadata(
            path.targetInfo.adapterId,
            path.targetInfo.id,
            path.targetInfo.outputTechnology,
        );
        let active = (path.flags & DISPLAYCONFIG_PATH_ACTIVE) != 0;
        let available = path.targetInfo.targetAvailable != 0;
        let role = classify_display_role(meta.output_technology, &adapter_path);
        let gdi_name = if active {
            query_source_gdi_name(path.sourceInfo.adapterId, path.sourceInfo.id)
        } else {
            String::new()
        };

        let mut position = (0i32, 0i32);
        let mut resolution = (0u32, 0u32);
        if let Some(s_idx) = source_mode_index(&path) {
            if let Some(mode) = raw.modes.get(s_idx) {
                if mode.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                    let src = unsafe { mode.Anonymous.sourceMode };
                    position = (src.position.x, src.position.y);
                    resolution = (src.width, src.height);
                }
            }
        }

        let refresh_hz = if path.targetInfo.refreshRate.Denominator > 0 {
            path.targetInfo.refreshRate.Numerator as f64
                / path.targetInfo.refreshRate.Denominator as f64
        } else {
            0.0
        };

        let is_primary = active && position == (0, 0);

        displays.push(DisplayInfo {
            friendly_name: meta.friendly_name,
            stable_key: meta.stable_key,
            gdi_name,
            adapter_path,
            edid_code: meta.edid_code,
            role,
            active,
            available,
            is_primary,
            position,
            resolution,
            refresh_hz,
            rotation: path.targetInfo.rotation as u32,
        });
    }

    displays.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then_with(|| b.is_primary.cmp(&a.is_primary))
            .then_with(|| a.position.0.cmp(&b.position.0))
            .then_with(|| a.position.1.cmp(&b.position.1))
            .then_with(|| a.friendly_name.cmp(&b.friendly_name))
    });

    Ok(displays)
}

/// Queries every target in `QDC_ALL_PATHS` with live adapter LUIDs and valid source IDs.
pub fn query_available_targets() -> Result<Vec<AvailableCcdTarget>, String> {
    let raw = query_raw_ccd(false)?;
    struct Agg {
        adapter_id: LUID,
        target_id: u32,
        output_technology: i32,
        target_available: bool,
        is_active: bool,
        active_source_id: Option<u32>,
        source_ids: Vec<u32>,
    }

    let mut map: HashMap<(u32, i32, u32), Agg> = HashMap::new();
    for path in &raw.paths {
        let key = (
            path.targetInfo.adapterId.LowPart,
            path.targetInfo.adapterId.HighPart,
            path.targetInfo.id,
        );
        let is_active = (path.flags & DISPLAYCONFIG_PATH_ACTIVE) != 0;
        let avail = path.targetInfo.targetAvailable != 0;
        let entry = map.entry(key).or_insert_with(|| Agg {
            adapter_id: path.targetInfo.adapterId,
            target_id: path.targetInfo.id,
            output_technology: path.targetInfo.outputTechnology,
            target_available: avail,
            is_active,
            active_source_id: if is_active {
                Some(path.sourceInfo.id)
            } else {
                None
            },
            source_ids: Vec::new(),
        });
        entry.target_available |= avail;
        if is_active {
            entry.is_active = true;
            entry.active_source_id = Some(path.sourceInfo.id);
        }
        if !entry.source_ids.contains(&path.sourceInfo.id) {
            entry.source_ids.push(path.sourceInfo.id);
        }
    }

    let mut out = Vec::with_capacity(map.len());
    for mut agg in map.into_values() {
        if !agg.target_available && !agg.is_active {
            continue;
        }
        agg.source_ids.sort_unstable();
        let meta = query_target_metadata(agg.adapter_id, agg.target_id, agg.output_technology);
        out.push(AvailableCcdTarget {
            adapter_id: agg.adapter_id,
            target_id: agg.target_id,
            active_source_id: agg.active_source_id,
            available_source_ids: agg.source_ids,
            stable_key: meta.stable_key,
            friendly_name: meta.friendly_name,
            edid_mfg: meta.edid_mfg,
            edid_product: meta.edid_product,
            output_technology: meta.output_technology,
            target_available: agg.target_available,
            is_active: agg.is_active,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeactivateTimings {
    pub query_ms: u64,
    pub meta_ms: u64,
    pub plan_ms: u64,
    pub apply_ms: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DeactivateResult {
    Deactivated(usize),
    WouldDisableAll,
    NothingMatched,
    Superseded,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DeactivateOutcome {
    pub result: DeactivateResult,
    pub timings: DeactivateTimings,
}

/// Deactivates selected active displays via `SetDisplayConfig` without overwriting Windows'
/// persistent docking database (`SDC_SAVE_TO_DATABASE` is intentionally omitted), reporting
/// stage-by-stage timings and checking `should_abort` right before invoking `SetDisplayConfig`.
pub fn deactivate_targets_timed<F: Fn() -> bool>(
    target_keys: &[String],
    should_abort: F,
) -> Result<DeactivateOutcome, String> {
    if target_keys.is_empty() {
        return Ok(DeactivateOutcome {
            result: DeactivateResult::NothingMatched,
            timings: DeactivateTimings::default(),
        });
    }
    let key_set: HashSet<String> = target_keys.iter().map(|s| s.to_ascii_lowercase()).collect();

    let t_query = Instant::now();
    let raw = query_raw_ccd(true)?;
    let mut timings = DeactivateTimings {
        query_ms: t_query.elapsed().as_millis() as u64,
        ..Default::default()
    };

    let t_meta = Instant::now();
    let mut kept_paths: Vec<DISPLAYCONFIG_PATH_INFO> = Vec::new();
    let mut deactivated_count = 0usize;

    for path in raw.paths {
        let meta = query_target_metadata(
            path.targetInfo.adapterId,
            path.targetInfo.id,
            path.targetInfo.outputTechnology,
        );
        if key_set.contains(&meta.stable_key.to_ascii_lowercase()) {
            deactivated_count += 1;
            continue;
        }

        kept_paths.push(path);
    }
    timings.meta_ms = t_meta.elapsed().as_millis() as u64;

    if deactivated_count == 0 {
        return Ok(DeactivateOutcome {
            result: DeactivateResult::NothingMatched,
            timings,
        });
    }
    if kept_paths.is_empty() {
        return Ok(DeactivateOutcome {
            result: DeactivateResult::WouldDisableAll,
            timings,
        });
    }

    let t_plan = Instant::now();
    let mut new_modes = remap_active_modes(&mut kept_paths, &raw.modes);

    // Ensure at least one remaining active source mode sits at (0, 0).
    normalize_primary_origin_in_modes(&kept_paths, &mut new_modes);
    timings.plan_ms = t_plan.elapsed().as_millis() as u64;

    if should_abort() {
        return Ok(DeactivateOutcome {
            result: DeactivateResult::Superseded,
            timings,
        });
    }

    let t_apply = Instant::now();
    let rc = unsafe {
        SetDisplayConfig(
            kept_paths.len() as u32,
            kept_paths.as_ptr(),
            new_modes.len() as u32,
            new_modes.as_ptr(),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
        )
    };
    timings.apply_ms = t_apply.elapsed().as_millis() as u64;
    if rc != 0 {
        return Err(format!(
            "SetDisplayConfig(deactivate_targets) failed with error {rc} [query={}ms, meta={}ms, plan={}ms, apply={}ms]",
            timings.query_ms, timings.meta_ms, timings.plan_ms, timings.apply_ms
        ));
    }

    Ok(DeactivateOutcome {
        result: DeactivateResult::Deactivated(deactivated_count),
        timings,
    })
}

/// Deactivates selected active displays via `SetDisplayConfig` without overwriting Windows'
/// persistent docking database (`SDC_SAVE_TO_DATABASE` is intentionally omitted).
pub fn deactivate_targets(target_keys: &[String]) -> Result<DeactivateResult, String> {
    deactivate_targets_timed(target_keys, || false).map(|o| o.result)
}

/// Convert virtual indices to standard indices and share one mode entry per identity.
/// Clone paths must reference the same source entry in SetDisplayConfig's mode table.
fn remap_active_modes(
    paths: &mut [DISPLAYCONFIG_PATH_INFO],
    original: &[DISPLAYCONFIG_MODE_INFO],
) -> Vec<DISPLAYCONFIG_MODE_INFO> {
    let mut modes = Vec::new();
    let mut indices = HashMap::new();
    for path in paths {
        let source = source_mode_index(path);
        let target = target_mode_index(path);
        let mut copy_mode = |index: Option<usize>, kind, adapter: LUID, id| {
            let Some(mode) = index.and_then(|i| original.get(i)).filter(|m| {
                m.infoType == kind
                    && m.id == id
                    && m.adapterId.LowPart == adapter.LowPart
                    && m.adapterId.HighPart == adapter.HighPart
            }) else {
                return DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
            };
            *indices
                .entry((kind, adapter.LowPart, adapter.HighPart, id))
                .or_insert_with(|| {
                    let next = modes.len() as u32;
                    modes.push(*mode);
                    next
                })
        };
        let source_index = copy_mode(
            source,
            DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
            path.sourceInfo.adapterId,
            path.sourceInfo.id,
        );
        let target_index = if source_index == DISPLAYCONFIG_PATH_MODE_IDX_INVALID {
            // A target mode without a source mode is invalid.
            DISPLAYCONFIG_PATH_MODE_IDX_INVALID
        } else {
            copy_mode(
                target,
                DISPLAYCONFIG_MODE_INFO_TYPE_TARGET,
                path.targetInfo.adapterId,
                path.targetInfo.id,
            )
        };
        path.flags = DISPLAYCONFIG_PATH_ACTIVE;
        path.sourceInfo.Anonymous.modeInfoIdx = source_index;
        path.targetInfo.Anonymous.modeInfoIdx = target_index;
    }
    modes
}

fn normalize_primary_origin_in_modes(
    paths: &[DISPLAYCONFIG_PATH_INFO],
    modes: &mut [DISPLAYCONFIG_MODE_INFO],
) {
    let mut has_origin = false;
    let mut anchor: Option<(i32, i32)> = None;
    for path in paths {
        let idx = unsafe { path.sourceInfo.Anonymous.modeInfoIdx };
        if idx != DISPLAYCONFIG_PATH_MODE_IDX_INVALID {
            if let Some(m) = modes.get(idx as usize) {
                if m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                    let pos = unsafe { m.Anonymous.sourceMode.position };
                    if anchor.is_none() {
                        anchor = Some((pos.x, pos.y));
                    }
                    if pos.x == 0 && pos.y == 0 {
                        has_origin = true;
                    }
                }
            }
        }
    }

    if !has_origin {
        if let Some((dx, dy)) = anchor {
            for mode in modes.iter_mut() {
                if mode.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                    unsafe {
                        mode.Anonymous.sourceMode.position.x -= dx;
                        mode.Anonymous.sourceMode.position.y -= dy;
                    }
                }
            }
        }
    }
}

/// Checks if a live GDI device currently reports valid display settings.
pub fn has_live_gdi_mode(gdi_name: &str) -> bool {
    if gdi_name.is_empty() {
        return false;
    }
    let gdi_w = to_wide_null(gdi_name);
    let mut dm: DEVMODEW = unsafe { zeroed() };
    dm.dmSize = size_of::<DEVMODEW>() as u16;
    let ok = unsafe { EnumDisplaySettingsExW(gdi_w.as_ptr(), ENUM_CURRENT_SETTINGS, &mut dm, 0) };
    ok != 0 && dm.dmPelsWidth > 0 && dm.dmPelsHeight > 0
}

/// Fast targeted scanout reset (`ChangeDisplaySettingsExW(..., CDS_RESET)`) without triggering
/// a global `SetDisplayConfig(SDC_FORCE_MODE_ENUMERATION)` across unaffected displays.
pub fn reset_gdi_scanout(gdi_names: &[String]) {
    for gdi in gdi_names {
        if gdi.is_empty() {
            continue;
        }
        let gdi_w = to_wide_null(gdi);
        let mut dm: DEVMODEW = unsafe { zeroed() };
        dm.dmSize = size_of::<DEVMODEW>() as u16;
        if unsafe { EnumDisplaySettingsExW(gdi_w.as_ptr(), ENUM_CURRENT_SETTINGS, &mut dm, 0) } != 0
        {
            let _ = unsafe {
                ChangeDisplaySettingsExW(gdi_w.as_ptr(), &dm, null_mut(), CDS_RESET, null())
            };
        }
    }
}

/// Re-commits active CCD paths with `SDC_FORCE_MODE_ENUMERATION` and calls
/// `ChangeDisplaySettingsExW(..., CDS_RESET)` on target GDI devices to break DP/DisplayLink stalls
/// without disturbing monitor coordinates.
pub fn force_mode_recommit(gdi_names: &[String]) -> Result<(), String> {
    let raw = query_raw_ccd(true)?;
    if !raw.paths.is_empty() {
        let mut clean_paths = raw.paths.clone();
        let clean_modes = remap_active_modes(&mut clean_paths, &raw.modes);

        let rc = unsafe {
            SetDisplayConfig(
                clean_paths.len() as u32,
                clean_paths.as_ptr(),
                clean_modes.len() as u32,
                clean_modes.as_ptr(),
                SDC_APPLY
                    | SDC_USE_SUPPLIED_DISPLAY_CONFIG
                    | SDC_ALLOW_CHANGES
                    | SDC_FORCE_MODE_ENUMERATION,
            )
        };
        if rc != 0 {
            return Err(format!(
                "SetDisplayConfig(SDC_FORCE_MODE_ENUMERATION) failed with error {rc}"
            ));
        }
    }

    reset_gdi_scanout(gdi_names);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_remapping_deduplicates_shared_clone_sources() {
        let mut source: DISPLAYCONFIG_MODE_INFO = unsafe { zeroed() };
        source.infoType = DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE;
        source.id = 3;
        source.adapterId.LowPart = 10;
        let mut first: DISPLAYCONFIG_PATH_INFO = unsafe { zeroed() };
        first.flags = DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE;
        first.sourceInfo.id = 3;
        first.sourceInfo.adapterId = source.adapterId;
        first.sourceInfo.Anonymous.modeInfoIdx = 0x0000ffff;
        first.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
        let mut paths = [first, first];
        let modes = remap_active_modes(&mut paths, &[source]);
        assert_eq!(modes.len(), 1);
        for path in paths {
            assert_eq!(path.flags, DISPLAYCONFIG_PATH_ACTIVE);
            assert_eq!(source_mode_index(&path), Some(0));
            assert_eq!(target_mode_index(&path), None);
        }
    }

    #[test]
    fn mode_remapping_does_not_attach_a_mode_to_the_wrong_device() {
        let mut mode: DISPLAYCONFIG_MODE_INFO = unsafe { zeroed() };
        mode.infoType = DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE;
        mode.id = 99;
        let mut path: DISPLAYCONFIG_PATH_INFO = unsafe { zeroed() };
        path.sourceInfo.Anonymous.modeInfoIdx = 0;
        path.targetInfo.Anonymous.modeInfoIdx = 0;
        let mut paths = [path];
        assert!(remap_active_modes(&mut paths, &[mode]).is_empty());
        assert_eq!(source_mode_index(&paths[0]), None);
        assert_eq!(target_mode_index(&paths[0]), None);
    }

    #[test]
    fn embedded_panels_are_never_external_toggle_targets() {
        for technology in [
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL,
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS,
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED,
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED,
        ] {
            assert_eq!(
                classify_display_role(technology, "pci#integrated"),
                DisplayRole::Internal
            );
        }
        assert_eq!(
            classify_display_role(
                DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED,
                "root#adapter"
            ),
            DisplayRole::UsbIndirect
        );
        assert_eq!(classify_display_role(5, "pci#gpu"), DisplayRole::External);
    }

    #[test]
    fn virtual_mode_indices_use_high_bits_and_invalid_sentinel() {
        let mut path: DISPLAYCONFIG_PATH_INFO = unsafe { zeroed() };
        path.flags = DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE;
        path.sourceInfo.Anonymous.modeInfoIdx = (7 << 16) | 3;
        path.targetInfo.Anonymous.modeInfoIdx = (9 << 16) | 4;
        assert_eq!(source_mode_index(&path), Some(7));
        assert_eq!(target_mode_index(&path), Some(9));
        path.sourceInfo.Anonymous.modeInfoIdx = 0xffff0002;
        path.targetInfo.Anonymous.modeInfoIdx = 0xffff0003;
        assert_eq!(source_mode_index(&path), None);
        assert_eq!(target_mode_index(&path), None);
        path.flags = DISPLAYCONFIG_PATH_ACTIVE;
        path.sourceInfo.Anonymous.modeInfoIdx = 70000;
        assert_eq!(source_mode_index(&path), Some(70000));
    }

    #[test]
    fn deactivate_targets_timed_handles_empty_keys_and_timings_defaults() {
        let outcome = deactivate_targets_timed(&[], || true).expect("empty keys ok");
        assert_eq!(outcome.result, DeactivateResult::NothingMatched);
        assert_eq!(outcome.timings, DeactivateTimings::default());
        assert_eq!(
            deactivate_targets(&[]).expect("wrapper ok"),
            DeactivateResult::NothingMatched
        );
    }
}
