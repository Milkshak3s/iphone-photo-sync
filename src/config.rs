use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const APP_DIR: &str = "iPhonePhotoSync";

/// Where the config, the import state and the log live:
/// `%LOCALAPPDATA%\iPhonePhotoSync`.
pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_DIR)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The folder photos are copied into. `None` until first-run setup is done.
    pub dest: Option<PathBuf>,
    /// Where videos go. `None`: videos are skipped.
    pub video_dest: Option<PathBuf>,
    pub start_on_login: bool,
    pub auto_update: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dest: None,
            video_dest: None,
            start_on_login: true,
            auto_update: true,
        }
    }
}

impl Config {
    fn path() -> PathBuf {
        data_dir().join("config.json")
    }

    pub fn load() -> Self {
        match fs::read(Self::path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                log::warn!("config.json is unreadable, using defaults: {e}");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        fs::create_dir_all(data_dir())?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        fs::rename(&tmp, &path).with_context(|| format!("saving {}", path.display()))
    }
}
