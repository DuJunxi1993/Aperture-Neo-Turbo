//! Direct2D bitmap upload from raw pixels
//!
//! Runs on the main thread — takes DecodedPixels and uploads to a GPU bitmap.

use windows::{
    Win32::Graphics::Direct2D::*,
    Win32::Graphics::Direct2D::Common::*,
    Win32::Graphics::Dxgi::Common::*,
};
use std::sync::Arc;
use crate::device::GpuContext;
use crate::decode::DecodedPixels;
use anyhow::Result;

pub struct DecodedBitmap {
    pub d2d_bitmap: ID2D1Bitmap1,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub path: String,
}

impl DecodedBitmap {
    /// Upload raw pixels to a Direct2D bitmap (GPU texture)
    pub fn from_pixels(gpu: &Arc<GpuContext>, pixels: &DecodedPixels) -> Result<Self> {
        unsafe {
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS(0),
                colorContext: core::mem::ManuallyDrop::new(None),
            };

            let size = D2D_SIZE_U {
                width: pixels.width,
                height: pixels.height,
            };

            let d2d_bitmap = gpu.d2d_dc.CreateBitmap(
                size,
                Some(pixels.pixels.as_ptr() as *const _),
                pixels.stride(),
                &props,
            ).map_err(|e| anyhow::anyhow!("CreateBitmap: {:?}", e))?;

            Ok(Self {
                d2d_bitmap,
                width: pixels.width,
                height: pixels.height,
                source_width: pixels.source_width,
                source_height: pixels.source_height,
                path: pixels.path.clone(),
            })
        }
    }

    /// Create a 1×1 transparent placeholder
    pub fn placeholder(gpu: &Arc<GpuContext>) -> Result<Self> {
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS(0),
            colorContext: core::mem::ManuallyDrop::new(None),
        };
        unsafe {
            let size = D2D_SIZE_U { width: 1, height: 1 };
            let pixel: u32 = 0;
            let d2d_bitmap = gpu.d2d_dc.CreateBitmap(
                size,
                Some(&pixel as *const _ as *const _),
                4,
                &props,
            ).map_err(|e| anyhow::anyhow!("CreateBitmap placeholder: {:?}", e))?;
            Ok(Self {
                d2d_bitmap,
                width: 1,
                height: 1,
                source_width: 1,
                source_height: 1,
                path: String::new(),
            })
        }
    }
}