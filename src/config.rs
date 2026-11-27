//! Configuration loading, default creation, and persistence (`%APPDATA%\monitor-toggle\config.toml`).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::display::TargetScope;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WakeMode {
    /// Stay off until toggle/recover hotkey or 3x Esc failsafe; suppresses mouse bumps.
    #[default]
    HotkeyOnly,
    /// Normal DPMS sleep; any input or hotkey wakes and runs full display recovery.
    AnyInput,
}

impl WakeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            WakeMode::HotkeyOnly => "hotkey_only",
            WakeMode::AnyInput => "any_input",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyRuleAction {
    #[default]
    Toggle,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MonitorHotkeyRule {
    #[serde(default)]
    pub hotkey: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub except: Vec<String>,
    #[serde(default)]
    pub action: HotkeyRuleAction,
}

fn clean_list(items: &[String]) -> Vec<String> {
    items
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

impl MonitorHotkeyRule {
    /// Returns the effective list of included (kept ON) selectors, migrating legacy `except`
    /// and `!`/`except:` selector forms seamlessly.
    pub fn effective_include(&self) -> Vec<String> {
        let inc = clean_list(&self.include);
        if !inc.is_empty() {
            return inc;
        }
        let legacy_except = clean_list(&self.except);
        if !legacy_except.is_empty() {
            return legacy_except;
        }
        if let Some(sel) = &self.selector {
            let t = sel.trim();
            if let Some(rest) = t.strip_prefix('!').or_else(|| {
                if t.get(..7)
                    .is_some_and(|head| head.eq_ignore_ascii_case("except:"))
                {
                    Some(&t[7..])
                } else {
                    None
                }
            }) {
                return rest
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string)
                    .collect();
            }
        }
        Vec::new()
    }

    /// Returns the effective list of excluded (turned OFF) selectors, migrating legacy `selector`.
    pub fn effective_exclude(&self) -> Vec<String> {
        let exc = clean_list(&self.exclude);
        if !exc.is_empty() {
            return exc;
        }
        if clean_list(&self.include).is_empty() && clean_list(&self.except).is_empty() {
            if let Some(sel) = &self.selector {
                let t = sel.trim();
                let is_except_form = t.starts_with('!')
                    || t.get(..7)
                        .is_some_and(|head| head.eq_ignore_ascii_case("except:"));
                if !t.is_empty() && !is_except_form {
                    return t
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToString::to_string)
                        .collect();
                }
            }
        }
        Vec::new()
    }

    /// Returns a normalized copy with uniform `include` and `exclude` lists for UI/API use.
    pub fn normalized_for_ui(&self) -> Self {
        Self {
            hotkey: self.hotkey.trim().to_string(),
            label: self
                .label
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            include: self.effective_include(),
            exclude: self.effective_exclude(),
            selector: None,
            except: Vec::new(),
            action: self.action,
        }
    }

    pub fn to_scope(&self) -> Result<TargetScope, String> {
        let inc = clean_list(&self.include);
        let exc = clean_list(&self.exclude);
        if !inc.is_empty() || !exc.is_empty() {
            return Ok(TargetScope::Profile {
                include: inc,
                exclude: exc,
            });
        }
        let keep = clean_list(&self.except);
        if !keep.is_empty() {
            return Ok(TargetScope::Except(keep));
        }
        if let Some(sel) = &self.selector {
            let trimmed = sel.trim();
            if !trimmed.is_empty() {
                return Ok(TargetScope::parse(trimmed));
            }
        }
        Err(format!(
            "Configuration '{}' must include or exclude at least one monitor",
            self.display_label()
        ))
    }

    pub fn display_label(&self) -> String {
        if let Some(lbl) = &self.label {
            let trimmed = lbl.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        let inc = self.effective_include();
        let exc = self.effective_exclude();
        match (inc.is_empty(), exc.is_empty()) {
            (false, true) => format!("Only {}", inc.join(" & ")),
            (true, false) => format!("Exclude {}", exc.join(" & ")),
            (false, false) => format!("Include {} / Exclude {}", inc.join(", "), exc.join(", ")),
            (true, true) => "Monitor Configuration".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct HotkeysConfig {
    pub toggle_all: String,
    pub toggle_externals: String,
    pub force_recover: String,
    pub monitor_hotkeys: Vec<MonitorHotkeyRule>,
}

impl Default for HotkeysConfig {
    fn default() -> Self {
        Self {
            toggle_all: "Ctrl+Alt+M".to_string(),
            toggle_externals: "Ctrl+Alt+E".to_string(),
            force_recover: "Ctrl+Alt+Shift+M".to_string(),
            monitor_hotkeys: vec![MonitorHotkeyRule {
                hotkey: "Ctrl+Alt+D".to_string(),
                label: Some("Only P27h-20 & J560T09".to_string()),
                include: vec!["P27h-20".to_string(), "J560T09".to_string()],
                exclude: Vec::new(),
                selector: None,
                except: Vec::new(),
                action: HotkeyRuleAction::Toggle,
            }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RecoveryConfig {
    pub settle_timeout_ms: u64,
    pub retrain_usb_displays_on_wake: bool,
    pub ddc_assist: bool,
    pub always_bounce_monitors: Vec<String>,
    pub restore_windows: bool,
    pub recover_on_system_wake: bool,
    pub blackout_curtain_when_locked: bool,
    pub audio_cue_on_force_recover: bool,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            settle_timeout_ms: 4000,
            retrain_usb_displays_on_wake: true,
            ddc_assist: true,
            always_bounce_monitors: Vec::new(),
            restore_windows: true,
            recover_on_system_wake: true,
            blackout_curtain_when_locked: true,
            audio_cue_on_force_recover: true,
        }
    }
}

fn default_start_minimized() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AppConfig {
    #[serde(default = "default_start_minimized")]
    pub start_minimized_to_tray: bool,
    pub wake_mode: WakeMode,
    #[serde(default)]
    pub lock_workstation_on_sleep: bool,
    #[serde(default)]
    pub wake_on_session_unlock: bool,
    pub hotkeys: HotkeysConfig,
    pub recovery: RecoveryConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            start_minimized_to_tray: true,
            wake_mode: WakeMode::HotkeyOnly,
            lock_workstation_on_sleep: false,
            wake_on_session_unlock: false,
            hotkeys: HotkeysConfig::default(),
            recovery: RecoveryConfig::default(),
        }
    }
}

impl AppConfig {
    /// Parses a TOML string into `AppConfig`.
    pub fn from_toml_str(raw: &str) -> Result<Self, String> {
        let cfg = toml::from_str::<Self>(raw).map_err(|e| format!("Invalid config TOML: {e}"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.recovery.settle_timeout_ms > 30_000 {
            return Err("settle_timeout_ms must be between 0 and 30000".into());
        }
        Ok(())
    }

    /// Loads `%APPDATA%\monitor-toggle\config.toml`, creating a commented default if missing.
    pub fn load_or_create_default() -> Result<Self, String> {
        let path = config_path();
        if !path.exists() {
            write_default_commented_toml(&path)?;
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path)
            .map_err(|e| format!("Failed reading {}: {e}", path.display()))?;
        Self::from_toml_str(&raw)
    }

    /// Persists the current configuration cleanly to `%APPDATA%\monitor-toggle\config.toml`.
    pub fn save(&self) -> Result<(), String> {
        self.validate()?;
        let path = config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed creating {}: {e}", parent.display()))?;
        }
        let body =
            toml::to_string_pretty(self).map_err(|e| format!("Failed serializing config: {e}"))?;
        let header = "# monitor-toggle configuration\n# Hotkey changes take effect after Reload Config or daemon restart.\n\n";
        crate::win_util::write_atomic(&path, format!("{header}{body}").as_bytes())
    }
}

pub fn config_dir() -> PathBuf {
    env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("monitor-toggle")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn data_dir() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|base| base.join("monitor-toggle"))
        .unwrap_or_else(config_dir)
}

pub fn snapshot_path() -> PathBuf {
    data_dir().join("last-known-good.json")
}

pub fn log_path() -> PathBuf {
    data_dir().join("daemon.log")
}

/// Writes a documented default `config.toml` if it does not yet exist.
pub fn write_default_commented_toml(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed creating {}: {e}", parent.display()))?;
    }
    let content = r#"# monitor-toggle configuration
# Start minimized to the system tray (hide the Control Panel window on cold launch)
start_minimized_to_tray = true
# Supported wake modes:
#   "hotkey_only" - Ignore mouse bumps & stray keys while toggled off (press Esc 3x as failsafe)
#   "any_input"   - Normal DPMS sleep (any mouse/key or hotkey wakes & recovers displays)
wake_mode = "hotkey_only"
# Lock Windows (Win+L equivalent) when turning off monitors
lock_workstation_on_sleep = false
# Wake monitors and run display recovery automatically when unlocking Windows
wake_on_session_unlock = false

[hotkeys]
# Global toggle for all monitors
toggle_all = "Ctrl+Alt+M"
# Toggle external displays only (keeps internal laptop panel on)
toggle_externals = "Ctrl+Alt+E"
# Blind force-recover / link bounce (works anytime, even if screens are black)
force_recover = "Ctrl+Alt+Shift+M"

# Optional per-monitor configurations / hotkeys.
# Use `include` to keep selected displays ON, and/or `exclude` to turn selected displays OFF.
[[hotkeys.monitor_hotkeys]]
hotkey = "Ctrl+Alt+D"
label = "Only P27h-20 & J560T09"
include = ["P27h-20", "J560T09"]
action = "toggle"

[recovery]
settle_timeout_ms = 4000
retrain_usb_displays_on_wake = true
ddc_assist = true
always_bounce_monitors = []
restore_windows = true
recover_on_system_wake = true
blackout_curtain_when_locked = true
audio_cue_on_force_recover = true
"#;
    crate::win_util::write_atomic(path, content.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_selectors_do_not_panic_or_change_meaning() {
        for selector in ["显示器显示器", "éééé", "日本語モニター"] {
            let rule = MonitorHotkeyRule {
                hotkey: String::new(),
                label: None,
                include: vec![],
                exclude: vec![],
                selector: Some(selector.into()),
                except: vec![],
                action: HotkeyRuleAction::Toggle,
            };
            assert!(rule.effective_include().is_empty());
            assert_eq!(rule.effective_exclude(), vec![selector]);
            assert_eq!(
                rule.to_scope().unwrap(),
                TargetScope::Monitor(selector.into())
            );
        }
    }

    #[test]
    fn rejects_unbounded_recovery_timeout() {
        assert!(
            AppConfig::from_toml_str("[recovery]\nsettle_timeout_ms = 18446744073709551615")
                .is_err()
        );
    }

    #[test]
    fn default_config_serializes_and_parses() {
        let cfg = AppConfig::default();
        let raw = toml::to_string_pretty(&cfg).expect("serialize default");
        let parsed = AppConfig::from_toml_str(&raw).expect("parse default");
        assert_eq!(cfg, parsed);
        assert_eq!(parsed.wake_mode, WakeMode::HotkeyOnly);
        assert_eq!(parsed.hotkeys.toggle_all, "Ctrl+Alt+M");
    }

    #[test]
    fn parses_per_monitor_rules_and_overrides() {
        let toml_text = r#"
wake_mode = "any_input"

[hotkeys]
toggle_all = "Ctrl+Shift+M"

[[hotkeys.monitor_hotkeys]]
hotkey = "Ctrl+Alt+1"
selector = "P27h-20"

[[hotkeys.monitor_hotkeys]]
hotkey = "Ctrl+Alt+D"
label = "Only P27h-20 & J560T09"
except = ["P27h-20", "J560T09"]
action = "off"

[recovery]
always_bounce_monitors = ["MB16AC"]
"#;
        let parsed = AppConfig::from_toml_str(toml_text).expect("parse custom");
        assert_eq!(parsed.wake_mode, WakeMode::AnyInput);
        assert_eq!(parsed.hotkeys.toggle_all, "Ctrl+Shift+M");
        assert_eq!(parsed.hotkeys.toggle_externals, "Ctrl+Alt+E");
        assert_eq!(parsed.hotkeys.monitor_hotkeys.len(), 2);
        assert_eq!(
            parsed.hotkeys.monitor_hotkeys[0].to_scope().unwrap(),
            TargetScope::Monitor("P27h-20".into())
        );
        assert_eq!(
            parsed.hotkeys.monitor_hotkeys[1].to_scope().unwrap(),
            TargetScope::Except(vec!["P27h-20".into(), "J560T09".into()])
        );
        assert_eq!(
            parsed.hotkeys.monitor_hotkeys[1].action,
            HotkeyRuleAction::Off
        );
        assert_eq!(parsed.recovery.always_bounce_monitors, vec!["MB16AC"]);
    }

    #[test]
    fn parses_profile_include_exclude_and_normalizes_legacy_rules() {
        let toml_text = r#"
[[hotkeys.monitor_hotkeys]]
hotkey = ""
label = "Desk Focus"
include = ["P27h-20", "J560T09"]
exclude = ["MB16AC"]

[[hotkeys.monitor_hotkeys]]
hotkey = "Ctrl+Alt+D"
label = "Legacy Keep"
except = ["P27h-20", "J560T09"]
"#;
        let parsed = AppConfig::from_toml_str(toml_text).expect("parse profiles");
        assert_eq!(parsed.hotkeys.monitor_hotkeys.len(), 2);
        assert_eq!(
            parsed.hotkeys.monitor_hotkeys[0].to_scope().unwrap(),
            TargetScope::Profile {
                include: vec!["P27h-20".into(), "J560T09".into()],
                exclude: vec!["MB16AC".into()],
            }
        );

        let migrated = parsed.hotkeys.monitor_hotkeys[1].normalized_for_ui();
        assert_eq!(migrated.include, vec!["P27h-20", "J560T09"]);
        assert!(migrated.exclude.is_empty());
        assert!(migrated.except.is_empty());
    }

    #[test]
    fn whitespace_only_include_falls_back_to_legacy_selector() {
        let rule = MonitorHotkeyRule {
            hotkey: String::new(),
            label: None,
            include: vec!["   ".into()],
            exclude: vec![],
            selector: Some("P27h-20".into()),
            except: vec!["  ".into()],
            action: HotkeyRuleAction::Toggle,
        };
        assert!(rule.effective_include().is_empty());
        assert_eq!(rule.effective_exclude(), vec!["P27h-20"]);
    }

    #[test]
    fn start_minimized_defaults_true_for_legacy_configs_and_parses_explicit_false() {
        let legacy = AppConfig::from_toml_str("wake_mode = \"any_input\"").unwrap();
        assert!(legacy.start_minimized_to_tray);

        let explicit = AppConfig::from_toml_str(
            "start_minimized_to_tray = false\nwake_mode = \"hotkey_only\"",
        )
        .unwrap();
        assert!(!explicit.start_minimized_to_tray);
    }

    #[test]
    fn lock_and_unlock_options_default_false_and_parse_independently() {
        let legacy = AppConfig::from_toml_str("wake_mode = \"hotkey_only\"").unwrap();
        assert!(!legacy.lock_workstation_on_sleep);
        assert!(!legacy.wake_on_session_unlock);

        let lock_only = AppConfig::from_toml_str("lock_workstation_on_sleep = true").unwrap();
        assert!(lock_only.lock_workstation_on_sleep);
        assert!(!lock_only.wake_on_session_unlock);

        let unlock_only = AppConfig::from_toml_str("wake_on_session_unlock = true").unwrap();
        assert!(!unlock_only.lock_workstation_on_sleep);
        assert!(unlock_only.wake_on_session_unlock);
    }
}
