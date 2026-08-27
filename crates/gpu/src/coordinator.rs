//! Decode coordinator — bridges NavigationService, WicLoader, and Direct2DViewer
//!
//! Pipeline:
//! 1. User requests current image
//! 2. Spawn WIC decode on blocking pool (returns DecodedPixels, thread-safe)
//! 3. On main thread: poll result + upload to BOTH:
//!    - D2D via DecodedBitmap::from_pixels (legacy path, deleted in Phase 4)
//!    - wgpu via DecodedGpuImage::from_pixels (new path, used by Phase 3+)
//! 4. Apply to viewer (D2D side reads `viewer.current`; Phase 3 adds the
//!    wgpu side reading `viewer.gpu_image`)
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
use anyhow::Result;
use aperture_core::{NavigationService, IImageLoader, ImageLoadResult};
use crate::{WicLoader, Direct2DViewer, SlideDir, decode_file, DecodedPixels, DecodedBitmap, DecodedGpuImage};

type DecodeId = u64;

/// Max queued navigation steps before we start coalescing.
const MAX_QUEUED_STEPS: usize = 60;

pub struct DecodeCoordinator {
    nav: Arc<Mutex<NavigationService>>,
    loader: Arc<WicLoader>,
    viewer: Arc<Mutex<Direct2DViewer>>,
    /// wgpu device + queue for the image-quad texture upload (Phase 2+).
    /// Held as `Option` so the coordinator compiles during the brief
    /// window between adding this field and the call-sites catching up.
    /// Removed once Phase 4 deletes the D2D `DecodedBitmap` field.
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
    /// Phase 2: produced but unused. Phase 3: bound to the image-quad
    /// bind group. Phase 4: replaces `DecodedBitmap`.
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
            // Upload to GPU on the main thread. Phase 2 keeps the
            // existing D2D upload (deleted in Phase 4) and adds the new
            // wgpu texture upload alongside it. Both paths are pure —
            // one decode result feeds both. The pixel buffer is reused
            // rather than re-decoded, so the cost is one BGRA→BGRA
            // copy to GPU memory (vs. the D2D path which is an
            // ID2D1Bitmap1 upload via the same bytes).
            let gpu = self.viewer.lock().gpu.clone();
            match DecodedBitmap::from_pixels(&gpu, &pixels) {
                Ok(bitmap) => {
                    self.viewer.lock().set_image(bitmap, resp.direction);
                    tracing::info!("Applied decoded image {} ({}x{}, {} KB pixels)",
                        resp.path.display(), pixels.width, pixels.height,
                        pixels.pixels.len() / 1024);
                }
                Err(e) => {
                    tracing::error!("Failed to upload pixels to D2D for {}: {}", resp.path.display(), e);
                }
            }
            // Phase 2 addition: wgpu texture upload (Phase 3+ consumes).
            if let (Some(device), Some(queue)) =
                (self.device.lock().as_ref(), self.queue.lock().as_ref())
            {
                match DecodedGpuImage::from_pixels(device, queue, &pixels) {
                    Ok(_gpu_image) => {
                        tracing::info!(
                            "Uploaded wgpu texture for {} ({}x{})",
                            resp.path.display(), pixels.width, pixels.height,
                        );
                        // Phase 3 will wire `_gpu_image` into the viewer's
                        // bind group and onto the queue. For now it goes
                        // out of scope at the end of this block — Phase 3
                        // holds it on the viewer / in the bind group.
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