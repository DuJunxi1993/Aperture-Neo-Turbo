//! WIC decode (thread-safe) → raw BGRA pixels
//!
//! WIC decoding runs on a background thread (tokio::spawn_blocking).
//! Result is BGRA pixel buffer + dimensions; D2D upload happens on main thread.

use windows::{
    Win32::Foundation::GENERIC_READ,
    Win32::Graphics::Imaging::*,
};
use windows_core::PCWSTR;
use std::path::Path;
use anyhow::Result;

/// Decoded image data ready for GPU upload
#[derive(Debug, Clone)]
pub struct DecodedPixels {
    pub pixels: Vec<u8>,        // BGRA8 premultiplied
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub path: String,
}

impl DecodedPixels {
    pub fn stride(&self) -> u32 {
        self.width * 4
    }
}

/// Decode a file using WIC — thread-safe, no D2D involvement
pub fn decode_file(path: &Path, target_w: u32, target_h: u32) -> Result<DecodedPixels> {
    unsafe {
        use windows::Win32::System::Com::*;

        // COM must be initialized per-thread; spawn_blocking does this for us
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;

        let path_str: Vec<u16> = path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let wic_factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;

        let decoder = wic_factory.CreateDecoderFromFilename(
            PCWSTR(path_str.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )?;

        let frame = decoder.GetFrame(0)?;

        let mut src_w: u32 = 0;
        let mut src_h: u32 = 0;
        frame.GetSize(&mut src_w, &mut src_h)?;
        if src_w == 0 || src_h == 0 {
            anyhow::bail!("Invalid source dimensions");
        }

        let (tw, th) = clamp_target(target_w, target_h, src_w, src_h);

        // Scale to target
        let scaler = wic_factory.CreateBitmapScaler()?;
        scaler.Initialize(&frame, tw, th, WICBitmapInterpolationModeFant)?;

        // Convert to BGRA8
        let converter = wic_factory.CreateFormatConverter()?;
        converter.Initialize(
            &scaler,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeMedianCut,
        )?;

        // Allocate pixel buffer
        let stride = tw * 4;
        let buffer_size = (stride * th) as usize;
        let mut pixels = vec![0u8; buffer_size];

        converter.CopyPixels(
            std::ptr::null(),
            stride,
            pixels.as_mut_slice(),
        )?;

        Ok(DecodedPixels {
            pixels,
            width: tw,
            height: th,
            source_width: src_w,
            source_height: src_h,
            path: path.to_string_lossy().to_string(),
        })
    }
}

/// Read only the pixel dimensions of an image (fast — no pixel decode).
///
/// NOTE: balances its own CoInitializeEx with CoUninitialize — this runs
/// on the main thread before winit, whose OleInitialize would otherwise
/// fail with RPC_E_CHANGED_MODE.
pub fn probe_image_size(path: &Path) -> Option<(u32, u32)> {
    unsafe {
        use windows::Win32::System::Com::*;
        let init = CoInitializeEx(None, COINIT_MULTITHREADED);
        let result = probe_inner(path);
        if init.is_ok() {
            // Balance our init ref (covers both S_OK and S_FALSE).
            CoUninitialize();
        }
        result
    }
}

unsafe fn probe_inner(path: &Path) -> Option<(u32, u32)> {
    use windows::Win32::System::Com::*;
    let path_str: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let wic_factory: IWICImagingFactory =
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
    let decoder = wic_factory
        .CreateDecoderFromFilename(
            PCWSTR(path_str.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )
        .ok()?;
    let frame = decoder.GetFrame(0).ok()?;
    let (mut w, mut h) = (0u32, 0u32);
    frame.GetSize(&mut w, &mut h).ok()?;
    if w == 0 || h == 0 { None } else { Some((w, h)) }
}

fn clamp_target(target_w: u32, target_h: u32, src_w: u32, src_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (src_w.max(1), src_h.max(1));
    }
    let max_w = if target_w == 0 { src_w } else { target_w.min(src_w) };
    let max_h = if target_h == 0 { src_h } else { target_h.min(src_h) };
    // Fit (tw, th) inside the (max_w, max_h) box preserving src aspect.
    let src_ar = src_w as f64 / src_h as f64;
    let box_ar = max_w as f64 / max_h as f64;
    let (tw, th) = if src_ar > box_ar {
        (max_w, ((max_w as f64 / src_ar).round() as u32).max(1))
    } else {
        (((max_h as f64 * src_ar).round() as u32).max(1), max_h)
    };
    (tw, th)
}