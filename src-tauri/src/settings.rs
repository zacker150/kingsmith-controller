//! The settings file: a small JSON file in the app config dir.
//!
//! ```json
//! {
//!   "version": 1,
//!   "pad": { "id": "F076ECBD-…", "mac": "57:4C:4D:0F:62:75", "name": "R1 Pro" },
//!   "displayUnits": "miles",
//!   "stopMethod": "standby",
//!   "presets": [161, 322, 483]
//! }
//! ```
//!
//! - `pad`: the remembered pad: the platform ID (a CoreBluetooth UUID on
//!   macOS, the MAC on Android) and the MAC when known (design §5.2).
//! - `stopMethod`: `standby` (default), `speedZero` or `toggleGuarded`;
//!   supervised test 1 decides. Not editable in the UI on purpose.
//! - `presets`: hundredths of a km/h (161 = 1.0 mph).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkingpad::protocol::DisplayUnits;
use walkingpad::session::{Config, KnownPad, Preset, StopMethod};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub version: u32,
    pub pad: Option<KnownPad>,
    pub display_units: DisplayUnits,
    pub stop_method: StopMethod,
    pub presets: [Preset; 3],
}

impl Default for Settings {
    fn default() -> Self {
        let cfg = Config::default();
        Self {
            version: 1,
            pad: None,
            display_units: DisplayUnits::Miles,
            stop_method: cfg.stop_method,
            presets: cfg.presets,
        }
    }
}

impl Settings {
    /// `settings.json`, or `settings-mock.json` in mock builds so a simulated
    /// pad is never remembered in the real file.
    pub fn path(config_dir: &Path) -> PathBuf {
        if cfg!(feature = "mock") {
            config_dir.join("settings-mock.json")
        } else {
            config_dir.join("settings.json")
        }
    }

    /// Loads the file, falling back to defaults if it is missing or broken
    /// (a broken file is kept as `settings.json.bad`).
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(s) => s,
                Err(e) => {
                    crate::applog::line(&format!("settings unreadable ({e}); using defaults"));
                    let _ = std::fs::rename(path, path.with_extension("json.bad"));
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Writes atomically (temp file, then rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)
    }

    pub fn session_config(&self) -> Config {
        Config {
            display_units: self.display_units,
            stop_method: self.stop_method,
            remembered: self.pad.clone(),
            presets: self.presets,
            ..Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_miles_and_standby() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.display_units, DisplayUnits::Miles);
        assert_eq!(s.stop_method, StopMethod::Standby);
        assert_eq!(s.pad, None);
    }

    #[test]
    fn round_trips_through_json() {
        let s = Settings {
            pad: Some(KnownPad {
                id: "F076ECBD-4FC5-B48F-A341-D7101DBCFBB8".into(),
                mac: Some("57:4C:4D:0F:62:75".into()),
                name: Some("R1 Pro".into()),
            }),
            display_units: DisplayUnits::Km,
            stop_method: StopMethod::SpeedZero,
            ..Settings::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""displayUnits":"km""#), "{json}");
        assert!(json.contains(r#""stopMethod":"speedZero""#), "{json}");
        assert!(json.contains(r#""mac":"57:4C:4D:0F:62:75""#), "{json}");
        assert!(json.contains(r#""presets":[161,322,483]"#), "{json}");
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), s);
    }

    #[test]
    fn save_and_load() {
        let dir = std::env::temp_dir().join(format!("kc-settings-{}", std::process::id()));
        let path = Settings::path(&dir);
        let s = Settings {
            stop_method: StopMethod::ToggleGuarded,
            ..Settings::default()
        };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
        assert!(path.with_extension("json.bad").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
