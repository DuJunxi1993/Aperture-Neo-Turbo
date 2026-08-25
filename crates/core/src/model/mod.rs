//! Domain models

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageItem {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub file_size: u64,
    pub modified: SystemTime,
    pub is_favorite: bool,
    pub thumbnail: Option<Vec<u8>>, // JPEG bytes
}

impl ImageItem {
    pub fn from_path(path: PathBuf) -> std::io::Result<Self> {
        let meta = std::fs::metadata(&path)?;
        Ok(Self {
            width: 0,
            height: 0,
            file_size: meta.len(),
            modified: meta.modified()?,
            is_favorite: false,
            thumbnail: None,
            path,
        })
    }

    pub fn filename(&self) -> String {
        self.path.file_name().unwrap_or_default().to_string_lossy().to_string()
    }

    pub fn stem(&self) -> String {
        self.path.file_stem().unwrap_or_default().to_string_lossy().to_string()
    }
}