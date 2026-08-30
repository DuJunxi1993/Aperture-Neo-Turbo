//! egui texture wrapper around a decoded image.
//!
//! The image is rendered through egui's painter (`painter.image` / a
//! textured mesh), so a decoded frame is uploaded as an
//! [`egui::TextureHandle`] rather than a raw wgpu texture. This removes the
//! custom image-quad pipeline: egui owns the texture lifecycle, the
//! mesh/vertex pipeline, clipping, and alpha compositing.
//!
//! WIC decodes to straight BGRA; egui's `ColorImage::from_rgba_unmultiplied`
//! wants RGBA, so we swap the R and B channels on upload. `average_luminance`
//! is still computed from the BGRA buffer (it's a downsample over all
//! channels, so channel order is irrelevant for the coarse signal).

use anyhow::Result;
use crate::DecodedPixels;

/// A decoded image uploaded to an egui texture, ready to be painted.
///
/// Wrapped in `Arc` so the viewer can hold it long-lived and clone it cheaply
/// (egui textures are reference-counted; the last handle frees the texture).
pub struct DecodedGpuImage {
    pub texture: egui::TextureHandle,
    pub width: u32,
    pub height: u32,
    /// Original (unclamped) image size from the source file, used for
    /// fit calculations in `crates/gpu/src/viewer.rs::compute_fit`.
    pub source_width: u32,
    pub source_height: u32,
    /// Average luminance (0..1) of the decoded image, computed once on the
    /// CPU side from the BGRA pixels before GPU upload. Used by the edge
    /// drawer's handle to pick a contrasting translucent color (light image
    /// → dark handle, dark image → light handle). Computed at decode time so
    /// it's stable across zoom/pan — the handle color follows the IMAGE's
    /// overall brightness, not the changing pixels under the cursor.
    pub average_luminance: f32,
}

impl DecodedGpuImage {
    /// Upload a decoded BGRA pixel buffer to an egui texture.
    ///
    /// WIC produces straight BGRA; egui's `ColorImage` is RGBA, so we swap
    /// the R and B channels (every 4 bytes), then hand it to egui's texture
    /// manager. `name` is passed through to egui for debugging.
    pub fn from_pixels(ctx: &egui::Context, pixels: &DecodedPixels) -> Result<Self> {
        let width = pixels.width;
        let height = pixels.height;
        let mut rgba = pixels.pixels.clone();
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [width as usize, height as usize],
            &rgba,
        );
        let average_luminance = Self::average_luminance(&pixels.pixels);
        let texture = ctx.load_texture(
            "aperture_main",
            color_image,
            egui::TextureOptions::LINEAR,
        );
        Ok(Self {
            texture,
            width,
            height,
            source_width: pixels.source_width,
            source_height: pixels.source_height,
            average_luminance,
        })
    }

    /// Downsampled average luminance (0..1) of a BGRA8 pixel buffer. Samples
    /// roughly every 16th pixel in each axis so the cost is O(n/256) — the
    /// actual average of a photo is well approximated by a sparse grid, and
    /// the drawer handle only needs a coarse bright/dark signal.
    fn average_luminance(pixels: &[u8]) -> f32 {
        if pixels.is_empty() {
            return 0.5;
        }
        // stride assumed 4 bytes/px (BGRA8). Count rows so we can skip
        // every Nth row and column for the downsample.
        let byte_len = pixels.len();
        // Derive a rough row stride from the byte length is not possible
        // without width here, so we just sample every 64th byte (≈ every
        // 16th pixel) uniformly across the buffer.
        let step = 64usize.max(1);
        let mut sum = 0.0f64;
        let mut count = 0u32;
        let mut i = 0usize;
        while i < byte_len {
            // Sample the G channel (index 1) for a decent luminance proxy.
            sum += pixels[i + 1] as f64;
            count += 1;
            i += step;
        }
        if count == 0 {
            return 0.5;
        }
        let avg = sum / count as f64;
        (avg / 255.0) as f32
    }
}
