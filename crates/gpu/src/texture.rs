//! GPU texture wrapper around a decoded image.
//!
//! Holds a `wgpu::Texture` + view + dimensions. The texture is uploaded
//! from `DecodedPixels` via `from_pixels`, which writes the BGRA
//! premultiplied pixel buffer to a `TextureFormat::Bgra8UnormSrgb`
//! texture (matching the WIC source layout — no channel swap needed).
//!
//! Phase 2: produced alongside `DecodedBitmap`; Phase 3 reads it.
//! Phase 4: the D2D `DecodedBitmap` is deleted and this becomes the
//! sole upload target.

use std::sync::Arc;
use anyhow::Result;
use crate::DecodedPixels;

/// A decoded image uploaded to a wgpu texture, ready to be sampled by
/// the image-quad shader.
///
/// Wrapped in `Arc` so the bind group can hold a reference and the
/// texture outlives any individual render pass. The texture is created
/// with `TEXTURE_BINDING | COPY_DST`; `COPY_SRC` and `RENDER_ATTACHMENT`
/// are intentionally omitted (this texture is never read back or used
/// as a render target).
pub struct DecodedGpuImage {
    pub texture: Arc<wgpu::Texture>,
    pub view: Arc<wgpu::TextureView>,
    pub width: u32,
    pub height: u32,
    /// Original (unclamped) image size from the source file, used for
    /// fit calculations in `crates/gpu/src/viewer.rs::compute_fit`.
    pub source_width: u32,
    pub source_height: u32,
}

impl DecodedGpuImage {
    /// Upload a decoded BGRA pixel buffer to a wgpu texture.
    ///
    /// The pixel buffer layout is assumed to be `Bgra8UnormSrgb` —
    /// no channel reorder. Premultiplied alpha is preserved on the GPU
    /// side; the image-quad shader unpremultiplies in fragment.
    pub fn from_pixels(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pixels: &DecodedPixels,
    ) -> Result<Self> {
        let format = wgpu::TextureFormat::Bgra8UnormSrgb;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("decoded_image_texture"),
            size: wgpu::Extent3d {
                width: pixels.width,
                height: pixels.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        // Bytes per row. wgpu's ImageDataLayout takes `bytes_per_row`
        // as `Option<NonZero<u32>>` — unwrap on the assumption that
        // `width * 4` never overflows u32 (4K is ~16 MiB per row, well
        // within u32 range).
        let bytes_per_row = pixels.stride();
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &pixels.pixels,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(pixels.height),
            },
            wgpu::Extent3d {
                width: pixels.width,
                height: pixels.height,
                depth_or_array_layers: 1,
            },
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Ok(Self {
            texture: Arc::new(texture),
            view: Arc::new(view),
            width: pixels.width,
            height: pixels.height,
            source_width: pixels.source_width,
            source_height: pixels.source_height,
        })
    }
}