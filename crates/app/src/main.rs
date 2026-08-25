//! Aperture Neo Turbo — Main entry point

#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use anyhow::Result;
use std::path::PathBuf;
use tracing::info;

mod window;
mod viewer_child;
mod event_router;
mod file_tree;
mod texture_cache;
mod path_shorten;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
        )
        .init();

    info!("Aperture Neo Turbo v{}", env!("CARGO_PKG_VERSION"));

    // Build a tokio runtime for the lifetime of the app.
    // Used by the decode coordinator to spawn async WIC decode tasks.
    let _rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("aperture-tokio")
        .build()?;
    let _guard = _rt.enter();

    // Optional argument: a folder of images, or a single image file
    // (e.g. launched as the default viewer by double-clicking a picture).
    let arg = std::env::args().nth(1).map(PathBuf::from);
    let target = match arg {
        Some(p) if p.is_file() && aperture_core::SupportedFormats::is_supported(&p) => {
            let (width, height) = aperture_gpu::probe_image_size(&p).unwrap_or((1280, 800));
            info!("Initial file from argv: {} ({}x{})", p.display(), width, height);
            window::LaunchTarget::SingleImage { path: p, width, height }
        }
        Some(p) => {
            info!("Initial folder from argv: {}", p.display());
            window::LaunchTarget::Folder(p)
        }
        None => window::LaunchTarget::None,
    };

    let event_loop = winit::event_loop::EventLoop::new()?;
    // Phase 5: switch from Poll to Wait. The previous Poll
    // caused the event loop to re-enter ~60 times per second even
    // when nothing was happening, which is wasteful — each tick
    // pulled a frame, ran the egui frame, and presented a
    // swapchain image. With Wait, the loop only re-enters when
    // we explicitly request a redraw (every input, every
    // animation tick, every decoded bitmap, every panel
    // width tween). Idle CPU drops to near zero. The same
    // `request_redraw()` call sites already enumerated in
    // MainWindow drive the new behaviour; we just stop
    // unconditionally re-arming the redraw at the bottom of
    // every frame in the WindowEvent::RedrawRequested handler.
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);

    let mut app = window::MainWindow::new(target);
    event_loop.run_app(&mut app)?;

    Ok(())
}