//! File system traversal, format detection, and watching

use std::path::{Path, PathBuf};
use std::sync::Arc;
use notify::{Event, RecommendedWatcher, Watcher, RecursiveMode};
use anyhow::Result;
use tracing::debug;

pub struct SupportedFormats;

impl SupportedFormats {
    pub const EXTENSIONS: &'static [&'static str] = &[
        "jpg", "jpeg", "png", "bmp", "gif", "tiff", "tif",
        "webp", "heic", "heif", "avif", "ico",
    ];

    pub fn is_supported(path: &Path) -> bool {
        path.extension()
            .and_then(|s| s.to_str())
            .map(|e| Self::EXTENSIONS.contains(&e.to_lowercase().as_str()))
            .unwrap_or(false)
    }

    pub fn filter_files(paths: Vec<PathBuf>) -> Vec<PathBuf> {
        paths.into_iter().filter(|p| Self::is_supported(p)).collect()
    }
}

pub struct FileSystemWatcher {
    _watcher: RecommendedWatcher,
    pub rx: Arc<parking_lot::Mutex<std::sync::mpsc::Receiver<Event>>>,
}

impl FileSystemWatcher {
    pub fn new(paths: Vec<PathBuf>) -> Result<Arc<Self>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher: RecommendedWatcher = Watcher::new(
            move |res: Result<Event, notify::Error>| {
                if let Ok(e) = res {
                    let _ = tx.send(e);
                }
            },
            notify::Config::default(),
        )?;
        for p in paths {
            if p.exists() {
                watcher.watch(&p, RecursiveMode::NonRecursive)?;
                debug!("Watching: {}", p.display());
            }
        }
        Ok(Arc::new(Self {
            _watcher: watcher,
            rx: Arc::new(parking_lot::Mutex::new(rx)),
        }))
    }
}

pub fn enumerate_images(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut results = Vec::new();
    if !dir.is_dir() { return Ok(results); }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && SupportedFormats::is_supported(&path) {
            results.push(path);
        }
    }
    results.sort();
    Ok(results)
}