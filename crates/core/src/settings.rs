//! Persistent settings (last folder, window position/size)
//!
//! Saved to `%APPDATA%\ApertureNeoTurbo\settings.json` as JSON.
//! Cheap to load on startup; saved on close.

use std::path::{Path, PathBuf};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use anyhow::Result;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SettingsData {
    /// Last opened folder
    pub last_folder: Option<PathBuf>,
    /// Window position (x, y)
    pub window_position: Option<(i32, i32)>,
    /// Window size (w, h)
    pub window_size: Option<(u32, u32)>,
    /// Max decode dimension cap
    pub max_decode_dimension: Option<u32>,
    /// Fullscreen mode
    pub fullscreen: Option<bool>,
    /// Recently opened folders (most recent first, max 10)
    #[serde(default)]
    pub recent_folders: Vec<PathBuf>,
    /// Favorite folders (for quick access in tree)
    #[serde(default)]
    pub favorite_folders: Vec<PathBuf>,
    /// UI theme: "dark" | "light" (None = dark). "system" reserved.
    #[serde(default)]
    pub theme: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeSetting {
    Dark,
    Light,
}

impl ThemeSetting {
    pub fn toggle(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }
}

pub struct SettingsStore {
    path: PathBuf,
    data: Mutex<SettingsData>,
}

impl SettingsStore {
    pub fn new() -> Result<Self> {
        let path = settings_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let data = if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        } else {
            SettingsData::default()
        };
        Ok(Self { path, data: Mutex::new(data) })
    }

    pub fn snapshot(&self) -> SettingsData {
        self.data.lock().clone()
    }

    pub fn last_folder(&self) -> Option<PathBuf> {
        self.data.lock().last_folder.clone()
    }

    pub fn set_last_folder(&self, folder: PathBuf) {
        self.data.lock().last_folder = Some(folder);
        let _ = self.save();
    }

    pub fn recent_folders(&self) -> Vec<PathBuf> {
        self.data.lock().recent_folders.clone()
    }

    /// Add a folder to the recents list (dedup, most-recent-first, cap 10).
    pub fn push_recent_folder(&self, folder: PathBuf) {
        let mut d = self.data.lock();
        d.recent_folders.retain(|f| f != &folder);
        d.recent_folders.insert(0, folder);
        d.recent_folders.truncate(10);
        drop(d);
        let _ = self.save();
    }

    /// Remove a folder from the recents list.
    pub fn remove_recent_folder(&self, folder: &Path) {
        let mut d = self.data.lock();
        d.recent_folders.retain(|f| f != folder);
        drop(d);
        let _ = self.save();
    }

    /// Get favorite folders.
    pub fn favorite_folders(&self) -> Vec<PathBuf> {
        self.data.lock().favorite_folders.clone()
    }

    /// UI theme — defaults to dark. "system" is reserved for future
    /// follow-the-OS support.
    pub fn theme(&self) -> crate::settings::ThemeSetting {
        match self.data.lock().theme.as_deref() {
            Some("light") => ThemeSetting::Light,
            _ => ThemeSetting::Dark,
        }
    }

    pub fn set_theme(&self, theme: ThemeSetting) {
        self.data.lock().theme = Some(match theme {
            ThemeSetting::Dark => "dark".into(),
            ThemeSetting::Light => "light".into(),
        });
        let _ = self.save();
    }

    /// Add a folder to favorites (dedup, cap 20).
    pub fn add_favorite_folder(&self, folder: PathBuf) {
        let mut d = self.data.lock();
        d.favorite_folders.retain(|f| f != &folder);
        d.favorite_folders.insert(0, folder);
        d.favorite_folders.truncate(20);
        drop(d);
        let _ = self.save();
    }

    /// Remove a folder from favorites.
    pub fn remove_favorite_folder(&self, folder: &Path) {
        let mut d = self.data.lock();
        d.favorite_folders.retain(|f| f != folder);
        drop(d);
        let _ = self.save();
    }

    /// Toggle a folder in favorites.
    pub fn toggle_favorite_folder(&self, folder: PathBuf) {
        let mut d = self.data.lock();
        let was_in = d.favorite_folders.iter().any(|f| f == &folder);
        if was_in {
            d.favorite_folders.retain(|f| f != &folder);
        } else {
            d.favorite_folders.insert(0, folder);
            d.favorite_folders.truncate(20);
        }
        drop(d);
        let _ = self.save();
    }

    pub fn set_window_size(&self, w: u32, h: u32) {
        self.data.lock().window_size = Some((w, h));
        let _ = self.save();
    }

    pub fn window_size(&self) -> Option<(u32, u32)> {
        self.data.lock().window_size
    }

    pub fn set_fullscreen(&self, fullscreen: bool) {
        self.data.lock().fullscreen = Some(fullscreen);
    }

    pub fn save(&self) -> Result<()> {
        let data = self.data.lock().clone();
        let json = serde_json::to_string_pretty(&data)?;
        std::fs::write(&self.path, json)?;
        Ok(())
    }
}

fn settings_path() -> PathBuf {
    let base = std::env::var("APPDATA")
        .ok()
        .map(PathBuf::from)
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("ApertureNeoTurbo").join("settings.json")
}

impl Drop for SettingsStore {
    fn drop(&mut self) {
        let _ = self.save();
    }
}