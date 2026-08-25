//! WicLoader — wraps the WIC factory, separate from D2D
//!
//! WIC decoding doesn't need D2D/D3D. The WIC factory is initialized once
//! and shared across threads (CoCreateInstance is thread-safe with COINIT_MULTITHREADED).

use std::path::Path;
use std::sync::Arc;
use parking_lot::Mutex;
use aperture_core::{IImageLoader, ImageLoadResult, CodecRoute, CodecProbe};
use crate::decode::{decode_file, DecodedPixels};
use anyhow::Result;

pub struct LoadedBitmap {
    pub result: ImageLoadResult,
    pub pixels: Option<DecodedPixels>,
}

pub struct WicLoader {
    pub max_dim: Mutex<u32>,
}

impl WicLoader {
    pub fn new() -> Self {
        Self {
            max_dim: Mutex::new(7680),
        }
    }
}

impl IImageLoader for WicLoader {
    fn load(&self, path: &Path, target_w: u32, target_h: u32) -> anyhow::Result<ImageLoadResult> {
        let route = CodecProbe::route_for(path);
        let max_dim = *self.max_dim.lock();
        let (tw, th) = if target_w == 0 && target_h == 0 {
            (max_dim, max_dim)
        } else if target_w == 0 {
            (target_h, target_h)
        } else if target_h == 0 {
            (target_w, target_w)
        } else {
            (target_w, target_h)
        };

        match route {
            CodecRoute::WicPrimary | CodecRoute::WicWithExtension => {
                match decode_file(path, tw, th) {
                    Ok(p) => Ok(ImageLoadResult {
                        path: path.to_string_lossy().to_string(),
                        width: p.width,
                        height: p.height,
                        source_width: p.source_width,
                        source_height: p.source_height,
                        is_success: true,
                        error_message: None,
                    }),
                    Err(e) => Ok(ImageLoadResult::failed(
                        path.to_string_lossy(),
                        format!("WIC failed: {}", e),
                    )),
                }
            }
            CodecRoute::SkiaFallback => {
                Ok(ImageLoadResult::failed(
                    path.to_string_lossy(),
                    "Skia fallback not implemented",
                ))
            }
        }
    }

    fn max_decode_dimension(&self) -> u32 {
        *self.max_dim.lock()
    }

    fn set_max_decode_dimension(&mut self, dim: u32) {
        *self.max_dim.lock() = dim;
    }
}

/// Standalone decode function returning pixels + result
pub fn load_pixels(
    path: &Path,
    target_w: u32,
    target_h: u32,
) -> anyhow::Result<(ImageLoadResult, DecodedPixels)> {
    let pixels = decode_file(path, target_w, target_h)?;
    let result = ImageLoadResult {
        path: path.to_string_lossy().to_string(),
        width: pixels.width,
        height: pixels.height,
        source_width: pixels.source_width,
        source_height: pixels.source_height,
        is_success: true,
        error_message: None,
    };
    Ok((result, pixels))
}