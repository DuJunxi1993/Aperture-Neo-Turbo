//! GPU rendering engine — wgpu + D3D11

pub mod animator;
pub mod coordinator;
pub mod decode;
pub mod device;
pub mod loader;
pub mod quad;
pub mod texture;
pub mod viewer;

pub use animator::{Animator, AffineTransform};
pub use coordinator::{DecodeCoordinator, DecodeResponse};
pub use decode::{decode_file, probe_image_size, DecodedPixels};
pub use device::GpuContext;
pub use loader::{WicLoader, LoadedBitmap};
pub use quad::{ImageQuadPipeline, ImageQuadUniforms, create_placeholder_texture, texture_format_premul_bgra};
pub use texture::DecodedGpuImage;
pub use viewer::{Direct2DViewer, SlideDir};
