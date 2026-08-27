//! GPU rendering engine — Direct2D/D3D11

pub mod animator;
pub mod bitmap;
pub mod coordinator;
pub mod decode;
pub mod device;
pub mod loader;
pub mod quad;
pub mod swapchain;
pub mod viewer;

pub use animator::{Animator, AffineTransform};
pub use bitmap::DecodedBitmap;
pub use coordinator::{DecodeCoordinator, DecodeResponse};
pub use decode::{decode_file, probe_image_size, DecodedPixels};
pub use device::GpuContext;
pub use loader::{WicLoader, LoadedBitmap};
pub use quad::{ImageQuadPipeline, ImageQuadUniforms, create_placeholder_texture, texture_format_premul_bgra};
pub use swapchain::{SwapchainHandle, buffer_size, create_swapchain_for_hwnd, resize_swapchain, present};
pub use viewer::{Direct2DViewer, SlideDir};