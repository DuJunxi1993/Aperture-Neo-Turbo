//! Decode abstraction layer — trait + routing + result types

use std::path::Path;
use anyhow::Result;

/// Result of a successful image decode
#[derive(Debug, Clone)]
pub struct ImageLoadResult {
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub is_success: bool,
    pub error_message: Option<String>,
}

impl ImageLoadResult {
    pub fn failed(path: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            width: 0,
            height: 0,
            source_width: 0,
            source_height: 0,
            is_success: false,
            error_message: Some(msg.into()),
        }
    }
}

/// Decoded bitmap ready for GPU upload
#[derive(Debug)]
pub struct DecodedBitmap {
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub pixels: Vec<u8>, // BGRA8 premultiplied
    pub path: String,
}

/// Trait for image loaders (WIC, Skia, etc.)
pub trait IImageLoader: Send + Sync {
    fn load(&self, path: &Path, target_w: u32, target_h: u32) -> Result<ImageLoadResult>;
    fn max_decode_dimension(&self) -> u32;
    fn set_max_decode_dimension(&mut self, dim: u32);
}

/// Codec selection strategy
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecRoute {
    WicPrimary,
    WicWithExtension,
    SkiaFallback,
}

pub struct CodecProbe;

impl CodecProbe {
    pub fn route_for(path: &Path) -> CodecRoute {
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
        match ext.as_str() {
            "jpg" | "jpeg" | "png" | "bmp" | "gif" | "tiff" | "tif" | "webp" | "avif" => CodecRoute::WicPrimary,
            "heic" | "heif" => CodecRoute::WicWithExtension,
            "psd" | "raw" | "cr2" | "nef" | "arw" | "dng" | "orf" | "rw2" => CodecRoute::SkiaFallback,
            _ => CodecRoute::WicPrimary,
        }
    }
}