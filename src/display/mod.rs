//! Display discovery, CCD topology snapshots, DDC/CI assist, and 4-stage recovery.

pub mod ccd;
pub mod ddc;
pub mod recovery;
pub mod snapshot;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayRole {
    Internal,
    External,
    UsbIndirect,
}

impl DisplayRole {
    pub fn label(self) -> &'static str {
        match self {
            DisplayRole::Internal => "Internal",
            DisplayRole::External => "External",
            DisplayRole::UsbIndirect => "UsbIndirect",
        }
    }

    pub fn is_external_like(self) -> bool {
        !matches!(self, DisplayRole::Internal)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetScope {
    All,
    ExternalsOnly,
    Monitor(String),
    Monitors(Vec<String>),
    Except(Vec<String>),
    Profile {
        #[serde(default)]
        include: Vec<String>,
        #[serde(default)]
        exclude: Vec<String>,
    },
}

impl TargetScope {
    /// Parses a CLI/config scope string (`all`, `externals`, `internal`, comma-separated
    /// selectors, `!A,B`, `except:A,B`, or a single monitor selector).
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        let lower = trimmed.to_ascii_lowercase();
        if trimmed.is_empty() || lower == "all" {
            return TargetScope::All;
        }
        if matches!(lower.as_str(), "external" | "externals" | "ext") {
            return TargetScope::ExternalsOnly;
        }
        if let Some(rest) = trimmed
            .strip_prefix('!')
            .or_else(|| strip_prefix_ignore_ascii_case(trimmed, "except:"))
        {
            let keep: Vec<String> = rest
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect();
            if !keep.is_empty() {
                return TargetScope::Except(keep);
            }
        }
        if trimmed.contains(',') {
            let list: Vec<String> = trimmed
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect();
            if list.len() > 1 {
                return TargetScope::Monitors(list);
            }
            if let Some(only) = list.into_iter().next() {
                return TargetScope::Monitor(only);
            }
        }
        TargetScope::Monitor(trimmed.to_string())
    }

    /// Returns true for scopes that declare an explicit keep-ON layout (`Except` or a
    /// `Profile` with non-empty `include`) rather than additively turning off target displays.
    pub fn is_explicit_keep_layout(&self) -> bool {
        match self {
            TargetScope::Except(_) => true,
            TargetScope::Profile { include, .. } => !include.is_empty(),
            TargetScope::All
            | TargetScope::ExternalsOnly
            | TargetScope::Monitor(_)
            | TargetScope::Monitors(_) => false,
        }
    }
}

fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DisplayInfo {
    pub friendly_name: String,
    pub stable_key: String,
    pub gdi_name: String,
    pub adapter_path: String,
    pub edid_code: String,
    pub role: DisplayRole,
    pub active: bool,
    pub available: bool,
    pub is_primary: bool,
    pub position: (i32, i32),
    pub resolution: (u32, u32),
    pub refresh_hz: f64,
    pub rotation: u32,
}

/// Case-insensitive selector matching against `"all"`, `"externals"`, `"internal"`,
/// 1-based index, `stable_key`, `edid_code`, `friendly_name`, or `gdi_name`.
pub fn matches_selector(display: &DisplayInfo, index_1based: usize, selector: &str) -> bool {
    let sel = selector.trim();
    if sel.is_empty() {
        return false;
    }
    let lower = sel.to_ascii_lowercase();
    match lower.as_str() {
        "all" => return true,
        "external" | "externals" | "ext" => return display.role.is_external_like(),
        "internal" => return display.role == DisplayRole::Internal,
        _ => {}
    }

    if let Ok(idx) = sel.parse::<usize>() {
        return idx == index_1based;
    }

    display.stable_key.to_ascii_lowercase().contains(&lower)
        || display.edid_code.to_ascii_lowercase().contains(&lower)
        || display.friendly_name.to_ascii_lowercase().contains(&lower)
        || display.gdi_name.to_ascii_lowercase() == lower
}

/// Resolves which `stable_key`s from `displays` match `scope`.
pub fn resolve_scope_keys(displays: &[DisplayInfo], scope: &TargetScope) -> Vec<String> {
    match scope {
        TargetScope::All => displays.iter().map(|d| d.stable_key.clone()).collect(),
        TargetScope::ExternalsOnly => displays
            .iter()
            .filter(|d| d.role.is_external_like())
            .map(|d| d.stable_key.clone())
            .collect(),
        TargetScope::Monitor(sel) => displays
            .iter()
            .enumerate()
            .filter(|(idx, d)| matches_selector(d, idx + 1, sel))
            .map(|(_, d)| d.stable_key.clone())
            .collect(),
        TargetScope::Monitors(sels) => displays
            .iter()
            .enumerate()
            .filter(|(idx, d)| sels.iter().any(|sel| matches_selector(d, idx + 1, sel)))
            .map(|(_, d)| d.stable_key.clone())
            .collect(),
        TargetScope::Except(keep_sels) => displays
            .iter()
            .enumerate()
            .filter(|(idx, d)| {
                !keep_sels
                    .iter()
                    .any(|sel| matches_selector(d, idx + 1, sel))
            })
            .map(|(_, d)| d.stable_key.clone())
            .collect(),
        TargetScope::Profile { include, exclude } => {
            if !include.is_empty() {
                // Keep ON only displays that match `include` and are NOT in `exclude`.
                // Everything else turns OFF.
                displays
                    .iter()
                    .enumerate()
                    .filter(|(idx, d)| {
                        let in_include =
                            include.iter().any(|sel| matches_selector(d, idx + 1, sel));
                        let in_exclude =
                            exclude.iter().any(|sel| matches_selector(d, idx + 1, sel));
                        !in_include || in_exclude
                    })
                    .map(|(_, d)| d.stable_key.clone())
                    .collect()
            } else {
                // Only exclusions specified: turn OFF matching displays; leave others ON.
                displays
                    .iter()
                    .enumerate()
                    .filter(|(idx, d)| exclude.iter().any(|sel| matches_selector(d, idx + 1, sel)))
                    .map(|(_, d)| d.stable_key.clone())
                    .collect()
            }
        }
    }
}

/// Returns true when the connected available displays already match `scope`'s
/// selective-off target state (all displays to turn off are inactive, and all
/// displays to keep are active).
pub fn is_scope_satisfied_in_selective_off(displays: &[DisplayInfo], scope: &TargetScope) -> bool {
    let available: Vec<DisplayInfo> = displays.iter().filter(|d| d.available).cloned().collect();
    if available.is_empty() {
        return false;
    }
    let off_keys = resolve_scope_keys(&available, scope);
    if off_keys.is_empty() || off_keys.len() >= available.len() {
        return false;
    }
    available.iter().all(|d| {
        let should_be_off = off_keys
            .iter()
            .any(|k| k.eq_ignore_ascii_case(&d.stable_key));
        if should_be_off {
            !d.active
        } else {
            d.active
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_displays() -> Vec<DisplayInfo> {
        vec![
            DisplayInfo {
                friendly_name: "Internal Display (LEN8AA5)".to_string(),
                stable_key: "display#len8aa5#4&13be0be7&0&uid8388688".to_string(),
                gdi_name: r"\\.\DISPLAY1".to_string(),
                adapter_path: "pci#ven_8086".to_string(),
                edid_code: "LEN8AA5".to_string(),
                role: DisplayRole::Internal,
                active: true,
                available: true,
                is_primary: true,
                position: (0, 0),
                resolution: (2560, 1600),
                refresh_hz: 60.0,
                rotation: 1,
            },
            DisplayInfo {
                friendly_name: "ASUS MB16AC".to_string(),
                stable_key: "display#aus1641#9&64eeb96&0&uid256".to_string(),
                gdi_name: r"\\.\DISPLAY21".to_string(),
                adapter_path: "usb#vid_17e9&pid_4374".to_string(),
                edid_code: "AUS1641".to_string(),
                role: DisplayRole::UsbIndirect,
                active: true,
                available: true,
                is_primary: false,
                position: (2560, 0),
                resolution: (1920, 1080),
                refresh_hz: 60.0,
                rotation: 1,
            },
        ]
    }

    #[test]
    fn resolves_scopes_and_selectors() {
        let displays = sample_displays();
        assert_eq!(
            resolve_scope_keys(&displays, &TargetScope::Monitor("1".into())),
            vec![displays[0].stable_key.clone()]
        );
        assert!(resolve_scope_keys(&displays, &TargetScope::Monitor("0".into())).is_empty());
        assert_eq!(resolve_scope_keys(&displays, &TargetScope::All).len(), 2);
        assert_eq!(
            resolve_scope_keys(&displays, &TargetScope::ExternalsOnly),
            vec!["display#aus1641#9&64eeb96&0&uid256"]
        );
        assert_eq!(
            resolve_scope_keys(&displays, &TargetScope::Monitor("MB16AC".to_string())),
            vec!["display#aus1641#9&64eeb96&0&uid256"]
        );
        assert_eq!(
            resolve_scope_keys(&displays, &TargetScope::Monitor("internal".to_string())),
            vec!["display#len8aa5#4&13be0be7&0&uid8388688"]
        );
    }

    #[test]
    fn resolves_except_scope_for_p27h_and_j560t09() {
        let mut displays = sample_displays();
        displays.push(DisplayInfo {
            friendly_name: "P27h-20".to_string(),
            stable_key: "display#len61e9#4&13be0be7&0&uid12613".to_string(),
            gdi_name: r"\\.\DISPLAY3".to_string(),
            adapter_path: "pci#ven_8086".to_string(),
            edid_code: "LEN61E9".to_string(),
            role: DisplayRole::External,
            active: true,
            available: true,
            is_primary: false,
            position: (0, 0),
            resolution: (2560, 1440),
            refresh_hz: 60.0,
            rotation: 1,
        });
        displays.push(DisplayInfo {
            friendly_name: "J560T09".to_string(),
            stable_key: "display#boe2775#4&13be0be7&0&uid8261".to_string(),
            gdi_name: r"\\.\DISPLAY2".to_string(),
            adapter_path: "pci#ven_8086".to_string(),
            edid_code: "BOE2775".to_string(),
            role: DisplayRole::External,
            active: true,
            available: true,
            is_primary: false,
            position: (-6, -1600),
            resolution: (2560, 1600),
            refresh_hz: 120.0,
            rotation: 1,
        });

        let scope1 = TargetScope::parse("!P27h-20,J560T09");
        let scope2 = TargetScope::parse("except:P27h-20, J560T09");
        let expected_scope = TargetScope::Except(vec!["P27h-20".into(), "J560T09".into()]);
        assert_eq!(scope1, expected_scope);
        assert_eq!(scope2, expected_scope);

        assert_eq!(
            resolve_scope_keys(&displays, &expected_scope),
            vec![
                "display#len8aa5#4&13be0be7&0&uid8388688",
                "display#aus1641#9&64eeb96&0&uid256",
            ]
        );
    }

    #[test]
    fn resolves_profile_include_exclude_combinations() {
        let mut displays = sample_displays();
        displays.push(DisplayInfo {
            friendly_name: "P27h-20".to_string(),
            stable_key: "display#len61e9#4&13be0be7&0&uid12613".to_string(),
            gdi_name: r"\\.\DISPLAY3".to_string(),
            adapter_path: "pci#ven_8086".to_string(),
            edid_code: "LEN61E9".to_string(),
            role: DisplayRole::External,
            active: true,
            available: true,
            is_primary: false,
            position: (0, 0),
            resolution: (2560, 1440),
            refresh_hz: 60.0,
            rotation: 1,
        });
        displays.push(DisplayInfo {
            friendly_name: "J560T09".to_string(),
            stable_key: "display#boe2775#4&13be0be7&0&uid8261".to_string(),
            gdi_name: r"\\.\DISPLAY2".to_string(),
            adapter_path: "pci#ven_8086".to_string(),
            edid_code: "BOE2775".to_string(),
            role: DisplayRole::External,
            active: true,
            available: true,
            is_primary: false,
            position: (-6, -1600),
            resolution: (2560, 1600),
            refresh_hz: 120.0,
            rotation: 1,
        });

        // Include externals, but also explicitly exclude ASUS MB16AC
        let combo = TargetScope::Profile {
            include: vec!["externals".into()],
            exclude: vec!["MB16AC".into()],
        };
        assert_eq!(
            resolve_scope_keys(&displays, &combo),
            vec![
                "display#len8aa5#4&13be0be7&0&uid8388688",
                "display#aus1641#9&64eeb96&0&uid256",
            ]
        );

        // Exclude-only turns off only matching displays
        let exc_only = TargetScope::Profile {
            include: Vec::new(),
            exclude: vec!["internal".into()],
        };
        assert_eq!(
            resolve_scope_keys(&displays, &exc_only),
            vec!["display#len8aa5#4&13be0be7&0&uid8388688"]
        );

        // Check selective satisfaction detection
        displays[0].active = false;
        assert!(is_scope_satisfied_in_selective_off(&displays, &exc_only));
        assert!(!is_scope_satisfied_in_selective_off(&displays, &combo));
    }
}
