//! Decode coordinator — bridges NavigationService, WicLoader, and Direct2DViewer
//!
//! Pipeline:
//! 1. User requests current image
//! 2. Spawn WIC decode on blocking pool (returns DecodedPixels, thread-safe)
//! 3. On main thread: poll result + upload wgpu texture
//! 4. Apply to viewer via set_image_gpu
//!
//! Rapid navigation: while a decode is in flight, further navigation steps are
//! queued and replayed one-by-one after the current decode lands, so every
//! image is shown sequentially instead of skipping to the last one. While the
//! queue is non-empty the slide animation shortens (fast-forward feel).

use std::path::PathBuf;
use std::sync::Arc;
use parking_lot::Mutex;
use tokio::sync::oneshot;
use std::collections::VecDeque;
use aperture_core::{NavigationService, IImageLoader, ImageLoadResult};
use crate::{WicLoader, Direct2DViewer, SlideDir, decode_file, DecodedPixels, DecodedGpuImage};

type DecodeId = u64;

/// Max queued navigation steps before we start coalescing.
const MAX_QUEUED_STEPS: usize = 60;

pub struct DecodeCoordinator {
    nav: Arc<Mutex<NavigationService>>,
    loader: Arc<WicLoader>,
    viewer: Arc<Mutex<Direct2DViewer>>,
    /// wgpu device + queue for the image-quad texture upload.
    device: Mutex<Option<Arc<wgpu::Device>>>,
    queue: Mutex<Option<Arc<wgpu::Queue>>>,
    next_id: Mutex<DecodeId>,
    pending: Mutex<Option<oneshot::Receiver<DecodeResponse>>>,
    /// Navigation steps waiting to be replayed after the in-flight decode.
    queued_steps: Mutex<VecDeque<SlideDir>>,
}

pub struct DecodeResponse {
    pub id: DecodeId,
    pub path: PathBuf,
    pub result: ImageLoadResult,
    pub direction: SlideDir,
    pub pixels: Option<DecodedPixels>,
    /// wgpu texture view populated by `poll()` once the decode lands.
    pub gpu_image: Option<Arc<DecodedGpuImage>>,
}

impl DecodeCoordinator {
    pub fn new(
        nav: Arc<Mutex<NavigationService>>,
        loader: Arc<WicLoader>,
        viewer: Arc<Mutex<Direct2DViewer>>,
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
    ) -> Self {
        Self {
            nav,
            loader,
            viewer,
            device: Mutex::new(Some(device)),
            queue: Mutex::new(Some(queue)),
            next_id: Mutex::new(0),
            pending: Mutex::new(None),
            queued_steps: Mutex::new(VecDeque::new()),
        }
    }

    pub fn request_current(&self, direction: SlideDir) -> DecodeId {
        // If a decode is already in flight, queue this step for sequential
        // replay instead of overwriting the pending request.
        {
            let mut pending = self.pending.lock();
            if pending.is_some() {
                let mut q = self.queued_steps.lock();
                if q.len() < MAX_QUEUED_STEPS {
                    q.push_back(direction);
                }
                return 0;
            }
            // Reserve the slot so chained requests see "busy".
            // We insert a placeholder None replaced below.
            *pending = None;
        }

        let nav_guard = self.nav.lock();
        let item = match nav_guard.current() {
            Some(it) => it,
            None => {
                tracing::debug!("request_current: no current item");
                return 0;
            }
        };
        tracing::debug!("request_current: decoding {}", item.path.display());

        let mut next = self.next_id.lock();
        *next += 1;
        let id = *next;
        drop(next);

        let path = item.path.clone();
        let max_dim = self.loader.max_decode_dimension();
        let viewport = self.viewer.lock().viewport_size();
        let tw = (viewport.0 * 2).clamp(1080, max_dim);
        let th = (viewport.1 * 2).clamp(1080, max_dim);

        let (tx, rx) = oneshot::channel();
        *self.pending.lock() = Some(rx);

        let path_for_error = path.clone();
        let path_for_send = path.clone();

        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                decode_file(&path, tw, th)
            })
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("Join error: {}", e)));

            match result {
                Ok(pixels) => {
                    tracing::info!("decode OK: {}x{} for {}", pixels.width, pixels.height, path_for_send.display());
                    let res = ImageLoadResult {
                        path: path_for_send.to_string_lossy().to_string(),
                        width: pixels.width,
                        height: pixels.height,
                        source_width: pixels.source_width,
                        source_height: pixels.source_height,
                        is_success: true,
                        error_message: None,
                    };
                    let _ = tx.send(DecodeResponse {
                        id,
                        path: path_for_send,
                        result: res,
                        direction,
                        pixels: Some(pixels),
                        gpu_image: None, // populated on the main thread in poll()
                    });
                }
                Err(e) => {
                    tracing::warn!("decode failed for {}: {}", path_for_error.display(), e);
                    let res = ImageLoadResult::failed(
                        path_for_error.to_string_lossy().to_string(),
                        format!("{}", e),
                    );
                    let _ = tx.send(DecodeResponse {
                        id,
                        path: path_for_error,
                        result: res,
                        direction,
                        pixels: None,
                        gpu_image: None,
                    });
                }
            }
        });

        id
    }

    pub fn poll(&self) -> Option<DecodeId> {
        let mut pending = self.pending.lock();
        let rx = pending.as_mut()?;
        let resp = match rx.try_recv() {
            Ok(r) => r,
            Err(oneshot::error::TryRecvError::Empty) => return None,
            Err(oneshot::error::TryRecvError::Closed) => {
                *pending = None;
                return None;
            }
        };

        *pending = None;
        drop(pending);

        let current = *self.next_id.lock();
        if resp.id != current {
            tracing::debug!("Discarding stale decode {} (current={})", resp.id, current);
        } else if !resp.result.is_success {
            tracing::warn!("Decode failed for {}: {:?}", resp.path.display(), resp.result.error_message);
        } else if let Some(pixels) = resp.pixels {
            // Upload to wgpu on the main thread.
            if let (Some(device), Some(queue)) =
                (self.device.lock().as_ref(), self.queue.lock().as_ref())
            {
                match DecodedGpuImage::from_pixels(device, queue, &pixels) {
                    Ok(gpu_image) => {
                        tracing::info!(
                            "Uploaded wgpu texture for {} ({}x{})",
                            resp.path.display(), pixels.width, pixels.height,
                        );
                        // Phase 3: hand the texture to the viewer. The
                        // viewer stores it in `current_gpu` so the image
                        // quad pipeline's bind group can reference it
                        // when render_frame builds the per-frame uniform.
                        self.viewer.lock().set_image_gpu(Arc::new(gpu_image), resp.direction);
                    }
                    Err(e) => {
                        tracing::error!(
                            "Failed to upload wgpu texture for {}: {}",
                            resp.path.display(), e,
                        );
                    }
                }
            }
        }

        // Replay the next queued navigation step (sequential display).
        let next_step = self.queued_steps.lock().pop_front();
        if let Some(dir) = next_step {
            // Fast-forward: shorten the slide while steps remain queued.
            let fast = self.has_queued();
            self.viewer.lock().set_slide_duration(if fast { 0.14 } else { 0.24 });
            self.request_current(dir);
        } else {
            // Queue drained — back to the full-duration slide.
            self.viewer.lock().set_slide_duration(0.38);
        }

        Some(resp.id)
    }

    /// True while navigation steps are still waiting to be displayed.
    pub fn has_queued(&self) -> bool {
        !self.queued_steps.lock().is_empty()
    }

    pub fn current_id(&self) -> DecodeId {
        *self.next_id.lock()
    }
}