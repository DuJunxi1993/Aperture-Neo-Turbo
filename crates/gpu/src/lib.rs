//! GPU rendering engine — wgpu

pub mod animator;
pub mod coordinator;
pub mod decode;
pub mod loader;
pub mod quad;
pub mod texture;
pub mod viewer;

pub use animator::{Animator, AffineTransform};
pub use coordinator::{DecodeCoordinator, DecodeResponse};
pub use decode::{decode_file, probe_image_size, DecodedPixels};
pub use loader::{WicLoader, LoadedBitmap};
pub use quad::{ImageQuadPipeline, ImageQuadUniforms, texture_format_premul_bgra};
pub use texture::DecodedGpuImage;
pub use viewer::{Direct2DViewer, SlideDir};
