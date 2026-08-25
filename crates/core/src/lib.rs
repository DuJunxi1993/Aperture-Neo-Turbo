//! Aperture Neo Turbo — Core platform-agnostic logic
//!
//! This crate contains all logic that doesn't directly depend on Windows APIs:
//! - Decode abstraction (IImageLoader trait + codec routing)
//! - Thumbnail cache (SQLite + memory LRU)
//! - Navigation state machine
//! - File system traversal and watching
//! - Persistent settings (last folder, window geometry)
//! - Domain models

pub mod cache;
pub mod decode;
pub mod fs;
pub mod model;
pub mod nav;
pub mod settings;

pub use cache::{ThumbCache, ThumbCacheConfig};
pub use decode::{IImageLoader, CodecRoute, ImageLoadResult, DecodedBitmap, CodecProbe};
pub use fs::{SupportedFormats, FileSystemWatcher};
pub use model::ImageItem;
pub use nav::{NavigationService, NavigationDirection};
pub use settings::{SettingsStore, SettingsData, ThemeSetting};