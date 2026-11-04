//! Application configuration in `config.toml` (XDG config directory).
//!
//! Missing or malformed files fall back to defaults rather than failing:
//! configuration is a convenience, never a blocker. The first save (e.g.
//! when pairing a device) creates the file.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::playlist;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// Whether the device syncs playlists at all (paired or not).
    pub enabled: bool,
}

impl SyncConfig {
    /// Used by the sync pairing wizard when it persists the config.
    #[allow(dead_code)]
    pub fn to_toml(&self) -> String {
        format!("[sync]\nenabled = {}\n", self.enabled)
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Human-friendly name used in playlist metadata and conflict labels.
    pub device: String,
    pub sync: SyncConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device: playlist::default_device_name(),
            sync: SyncConfig::default(),
        }
    }
}

impl Config {
    /// Load from the default location; defaults when anything is off.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        Self::load_from(&path).unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error).context("could not read config.toml"),
        };
        toml::from_str(&text).context("config.toml is not valid TOML")
    }

    /// Persists the config; first called by the sync pairing wizard.
    #[allow(dead_code)]
    pub fn save(&self) -> Result<()> {
        let Some(path) = config_path() else {
            anyhow::bail!("could not resolve the configuration directory");
        };
        self.save_to(&path)
    }

    #[allow(dead_code)]
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("could not create the config directory")?;
        }
        let doc = format!(
            "# TuiMu configuration\n\ndevice = {:?}\n\n{}",
            self.device,
            self.sync.to_toml()
        );
        fs::write(path, doc).context("could not write config.toml")?;
        Ok(())
    }
}

fn config_path() -> Option<PathBuf> {
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(root.join("tuimu").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("tuimu-config-{name}-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn missing_file_loads_defaults() {
        let config = Config::load_from(&temp_path("missing")).expect("load");
        assert!(!config.sync.enabled);
        assert!(!config.device.is_empty());
    }

    #[test]
    fn roundtrips_through_disk() {
        let path = temp_path("roundtrip");
        let config = Config {
            device: "pixel".into(),
            sync: SyncConfig { enabled: true },
        };
        config.save_to(&path).expect("save");
        let reloaded = Config::load_from(&path).expect("load");
        assert_eq!(reloaded.device, "pixel");
        assert!(reloaded.sync.enabled);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn garbage_file_falls_back_to_defaults() {
        let path = temp_path("garbage");
        fs::write(&path, "not [ valid toml {{{{").expect("write");
        // load_from surfaces the parse error; Config::load() swallows it.
        assert!(Config::load_from(&path).is_err());
        let _ = fs::remove_file(&path);
    }
}
