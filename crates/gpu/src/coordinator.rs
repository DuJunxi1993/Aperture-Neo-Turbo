//! Decode coordinator — bridges NavigationService, WicLoader, and Direct2DViewer
//!
//! Pipeline:
//! 1. User requests current image
//! 2. Spawn WIC decode on blocking pool (returns DecodedPixels, thread-safe)
//! 3. On main thread: poll result + upload egui texture
//! 4. Apply to viewer via set_image_gpu
//!
//! Rapid navigation: a small LRU pre-decode cache (capped) holds decoded
//! images so a fast hold hits the cache and shows instantly instead of
//! waiting on WIC each step. Adjacent images (±2) are pre-decoded in the
//! background so a pause between navigations warms the cache. While a
//! decode is in flight, further navigation steps are queued and replayed
//! one-by-one after the current decode lands (cover-style: overwrite to the
//! newest so a held key "follows the hand" without replaying a backlog).

use std::path::PathBuf;
use std::time::SystemTime;
use std::sync::Arc;
use parking_lot::Mutex;
use tokio::sync::oneshot;
use std::collections::VecDeque;
use aperture_core::{NavigationService, ImageLoadResult};
use crate::{Direct2DViewer, SlideDir, decode_file, DecodedPixels, DecodedGpuImage};

type DecodeId = u64;

/// Quality tier of a cached decode. The immediate neighbours (±1) are decoded
/// at full display size; the further neighbours (±2) at a low clamp so
/// warm-up is cheap and a flick-past still shows something instantly. A
/// low-tier hit can be upgraded to full asynchronously (progressive display).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Full,
    Low,
}

/// Clamp dimension for the low (preview) tier. Large enough that a
/// flick-past / walk image looks crisp as a soft on-screen frame, but small
/// enough that decoding it is near-instant (a 640px decode is ~O(1/12) the
/// pixels of a 2×4K full decode). Full-size decodes still happen in the
/// background to upgrade the displayed image.
const LOW_RES_DIM: u32 = 640;

/// Resolution ceiling for the full (display) tier. A folder of 8K/5K wallpapers
/// would otherwise trigger near-full-resolution WIC decodes (up to `max_dim` =
/// 7680), which is expensive per image and — for several concurrent decodes —
/// enough to stall the machine. Capping the full tier at 4096 keeps the
/// displayed image crisp on the vast majority of displays while bounding the
/// decode cost. In practice this is additionally clamped to the device's
/// `max_texture_dimension_2d` (often 2048/4096) so the uploaded texture never
/// exceeds what the GPU can hold.
const FULL_RES_DIM: u32 = 4096;

/// Compute the full-tier decode clamp: at least 1080 (never decode so small a
/// full image looks soft on a large display), at most `min(FULL_RES_DIM, limit)`.
/// `limit` is the device's `max_texture_dimension_2d`; the clamp never exceeds
/// it because egui's `load_texture` asserts on an oversized texture.
fn full_clamp(viewport_max: u32, limit: u32) -> u32 {
    (viewport_max * 2).clamp(1080, FULL_RES_DIM.min(limit))
}

/// Pre-decode cache for decoded images (LRU, capped). Adjacent images
/// are decoded ahead so a fast navigation shows the target instantly
/// instead of blocking on WIC (the "predecode adjacent + hit-display"
/// idea, adapted to egui: we cache the fully-uploaded `DecodedGpuImage`).
pub struct PredecodeCache {
    entries: Mutex<VecDeque<(PathBuf, SystemTime, Arc<DecodedGpuImage>, Tier)>>,
    cap: usize,
}

impl PredecodeCache {
    pub fn new(cap: usize) -> Self {
        Self { entries: Mutex::new(VecDeque::new()), cap: cap.max(1) }
    }

    /// Fetch (and remove) a cached image by `path` if its file `modified`
    /// stamp matches (so a file edited on disk invalidates the entry), and
    /// only if it is at least the requested tier. A `Low` hit is never
    /// returned when `Full` was requested — the caller then knows to start a
    /// full-size decode.
    pub fn take(&self, path: &PathBuf, modified: SystemTime, min_tier: Tier)
        -> Option<Arc<DecodedGpuImage>>
    {
        let mut e = self.entries.lock();
        let pos = e.iter().position(|(p, m, _, t)| p == path && *m == modified && tier_at_least(*t, min_tier))?;
        let (_, _, img, _) = e.remove(pos).unwrap();
        Some(img)
    }

    /// Insert an image keyed by `(path, modified)`, evicting the LRU tail if
    /// over capacity. One logical entry per `(path, modified)`: a full insert
    /// replaces/upgrades any existing low entry; a low insert is ignored if a
    /// full entry is already present (it would be a wasteful downgrade below
    /// what's already available). The entry is moved to the front (LRU).
    pub fn insert(&self, path: PathBuf, modified: SystemTime, img: Arc<DecodedGpuImage>, tier: Tier) {
        let mut e = self.entries.lock();
        // Is there already a Full entry? (it satisfies any tier requirement)
        if let Some(pos) = e.iter().position(|(p, m, _, t)| p == &path && *m == modified && *t == Tier::Full) {
            if tier == Tier::Full {
                // Replace pixels and refresh LRU position.
                let mut entry = e.remove(pos).unwrap();
                entry.2 = img;
                e.push_front(entry);
            } else {
                // Full already present; a Low insert is redundant.
                let entry = e.remove(pos).unwrap();
                e.push_front(entry);
            }
            return;
        }
        // No Full entry. Replace any existing Low entry (either tier insert
        // updates it — a Full insert here is an upgrade to Low→Full).
        if let Some(pos) = e.iter().position(|(p, m, _, _)| p == &path && *m == modified) {
            e.remove(pos);
        }
        e.push_front((path, modified, img, tier));
        while e.len() > self.cap {
            e.pop_back();
        }
    }

    pub fn clear(&self) {
        self.entries.lock().clear();
    }

    /// True if an entry for `path` with a matching `modified` stamp exists at
    /// (or above) the requested tier.
    pub fn contains(&self, path: &PathBuf, modified: SystemTime, min_tier: Tier) -> bool {
        let e = self.entries.lock();
        e.iter().any(|(p, m, _, t)| p == path && *m == modified && tier_at_least(*t, min_tier))
    }
}

/// Whether `have` satisfies the requirement `want` (Full ≥ Low).
fn tier_at_least(have: Tier, want: Tier) -> bool {
    match (have, want) {
        (Tier::Full, Tier::Low) | (Tier::Full, Tier::Full) | (Tier::Low, Tier::Low) => true,
        (Tier::Low, Tier::Full) => false,
    }
}

pub struct DecodeCoordinator {
    nav: Arc<Mutex<NavigationService>>,
    viewer: Arc<Mutex<Direct2DViewer>>,
    /// egui context for uploading decoded frames as egui textures.
    /// `poll()` runs on the main thread, where `load_texture` is safe.
    ctx: egui::Context,
    /// The GPU's 2D texture dimension limit (from device limits). Decoded
    /// textures must never exceed this on either axis, or egui's `load_texture`
    /// debug-asserts (and wgpu rejects) the upload. Guards the full-tier clamp.
    max_texture_dim: u32,
    next_id: Mutex<DecodeId>,
    pending: Mutex<Option<oneshot::Receiver<DecodeResponse>>>,
    /// Navigation steps waiting to be replayed after the in-flight decode.
    queued_steps: Mutex<VecDeque<SlideDir>>,
    /// Background pre-decode jobs (adjacent-image warm-up). Each carries its
    /// own oneshot receiver; `poll` drains them and caches the uploaded
    /// GPU image in `precache`. These never touch the viewer — they only
    /// warm the cache so the next `request_current` is an instant hit.
    prefetch: Mutex<Vec<PrefetchJob>>,
    /// Decoded-GPU-image LRU cache for instant navigation hits.
    pub precache: PredecodeCache,
    /// Path of the image currently displayed in the viewer. Used so a
    /// background full-size upgrade can detect "this low frame is still the
    /// current image" and sharpen it in place when the full decode lands.
    displayed_path: Mutex<PathBuf>,
    /// Set when a decode was spawned as a LOW preview for a path that still
    /// needs a FULL upgrade. `poll` consumes it after the low decode lands to
    /// kick the full-size decode.
    pending_upgrade_path: Mutex<Option<PathBuf>>,
}

/// A background pre-decode job. `rx` receives the decode result; `path` +
/// `modified` key the cache entry on completion. `tier` records which clamp
/// the decode used (`Full` or `Low`). `upgrade` marks a job whose full-size
/// result should ALSO replace a currently-displayed low-tier image
/// (progressive sharpen), not just warm the cache.
struct PrefetchJob {
    path: PathBuf,
    modified: SystemTime,
    rx: oneshot::Receiver<DecodeResponse>,
    tier: Tier,
    upgrade: bool,
}

pub struct DecodeResponse {
    pub id: DecodeId,
    pub path: PathBuf,
    pub result: ImageLoadResult,
    pub direction: SlideDir,
    pub pixels: Option<DecodedPixels>,
    /// egui texture view populated by `poll()` once the decode lands.
    pub gpu_image: Option<Arc<DecodedGpuImage>>,
}

impl DecodeCoordinator {
    pub fn new(
        nav: Arc<Mutex<NavigationService>>,
        viewer: Arc<Mutex<Direct2DViewer>>,
        ctx: egui::Context,
        max_texture_dim: u32,
    ) -> Self {
        Self {
            nav,
            viewer,
            ctx,
            max_texture_dim: max_texture_dim.max(1),
            next_id: Mutex::new(0),
            pending: Mutex::new(None),
            queued_steps: Mutex::new(VecDeque::new()),
            prefetch: Mutex::new(Vec::new()),
            precache: PredecodeCache::new(5),
            displayed_path: Mutex::new(PathBuf::new()),
            pending_upgrade_path: Mutex::new(None),
        }
    }

    pub fn request_current(&self, direction: SlideDir) -> DecodeId {
        // Cover-style navigation: if a decode is already in flight, the
        // user is likely holding an arrow key and paging FAST. We don't
        // want to replay a backlog (that lags behind and keeps flipping
        // after the key is released) — we OVERWRITE any queued step with
        // the newest direction so the next decode jumps straight to the
        // latest requested image ("follow the hand"). This makes hold-to-
        // page feel immediate and stop exactly on release.
        {
            let mut pending = self.pending.lock();
            if pending.is_some() {
                let mut q = self.queued_steps.lock();
                q.clear();
                q.push_back(direction);
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
        let path = item.path.clone();
        let modified = item.modified;
        // Release the nav lock before taking a cache hit / spawning decode.
        drop(nav_guard);

        // Full-quality cache hit → display instantly (no WIC wait). Keyed on
        // file mtime so a file edited on disk invalidates the stale entry.
        if let Some(cached) = self.precache.take(&path, modified, Tier::Full) {
            tracing::info!("full precache hit, applying {} instantly", path.display());
            *self.displayed_path.lock() = path.clone();
            self.viewer.lock().set_image_gpu(cached, direction);
            return 0;
        }
        // Low-quality cache hit → show it immediately (direct cut, no slide)
        // and kick a background full-size decode to progressively sharpen it.
        if let Some(cached) = self.precache.take(&path, modified, Tier::Low) {
            tracing::info!("low precache hit, applying {} instantly (upgrading)", path.display());
            // Display the low tier now, sharpening to full when the async
            // upgrade lands. No slide on a low hit (it may be a flick-past).
            *self.displayed_path.lock() = path.clone();
            let low = cached;
            self.viewer.lock().set_image_gpu(low, SlideDir::None);
            self.request_full_upgrade(path, modified);
            return 0;
        }
        // No cache entry at all. Decode the LOW tier first (cheap, so the
        // image appears without a blank frame), then request the full-size
        // upgrade. This is the always-forward "never show a blank" path.
        tracing::debug!("request_current: decoding (low) {}", path.display());
        *self.displayed_path.lock() = path.clone();
        *self.pending_upgrade_path.lock() = Some(path.clone());
        let mut next = self.next_id.lock();
        *next += 1;
        let id = *next;
        drop(next);
        let rx = self.spawn_decode(path.clone(), LOW_RES_DIM, LOW_RES_DIM, SlideDir::None, id);
        *self.pending.lock() = Some(rx);
        id
    }

    /// A low-tier image is now displayed (or about to be); decode the
    /// full-size version in the background and apply it when ready
    /// (progressive sharpen). The upgrade runs as a background prefetch job
    /// that warms `precache` (Full) and, if the full decode lands while this
    /// path is still the displayed image, `drain_prefetch` applies it to the
    /// viewer as well — sharpening the low frame in place with a direct cut.
    fn request_full_upgrade(&self, path: PathBuf, modified: SystemTime) {
        tracing::debug!("request_current: upgrading {} to full", path.display());
        let viewport = self.viewer.lock().viewport_size();
        let tw = full_clamp(viewport.0.max(viewport.1), self.max_texture_dim);
        let th = full_clamp(viewport.0.max(viewport.1), self.max_texture_dim);
        let mut jobs = self.prefetch.lock();
        let existing = jobs.iter().any(|j| j.path == path);
        if !existing {
            let rx = self.spawn_decode(path.clone(), tw, th, SlideDir::None, 0);
            jobs.push(PrefetchJob { path, modified, rx, tier: Tier::Full, upgrade: true });
        } else {
            // An in-flight prefetch already covers this path; mark it as an
            // upgrade so it applies to the viewer if still displayed.
            if let Some(j) = jobs.iter_mut().find(|j| j.path == path) {
                j.upgrade = true;
            }
        }
    }

    /// Spawn a WIC decode for `path` at the given target clamp and return
    /// its oneshot receiver. The decode runs on `spawn_blocking`; the
    /// receiver is drained by `poll` (main request) or by the prefetch
    /// path. `direction` is echoed back so `poll` knows which image the
    /// request belongs to. `id` is the monotonic decode id for the main
    /// request (0 for background prefetches).
    fn spawn_decode(
        &self,
        path: PathBuf,
        tw: u32,
        th: u32,
        direction: SlideDir,
        id: u64,
    ) -> oneshot::Receiver<DecodeResponse> {
        let (tx, rx) = oneshot::channel();

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
                    tracing::debug!("decode OK: {}x{} for {}", pixels.width, pixels.height, path_for_send.display());
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

        rx
    }

    /// Warm the pre-decode cache with the images adjacent (±2) to the
    /// current nav index, so a fast hold hits the cache instead of waiting
    /// on WIC. Never touches the viewer — it only fills `precache`. Call
    /// after a navigation lands.
    pub fn predecode_adjacent(&self) {
        let nav_guard = self.nav.lock();
        let items = nav_guard.items().to_vec();
        let idx = nav_guard.current_index();
        let count = items.len();
        // No images (e.g. a folder with nothing decodable) → nothing to
        // pre-decode, and the modulo below would panic on `% 0`.
        if count == 0 {
            return;
        }
        let viewport = { self.viewer.lock().viewport_size() };
        let full_dim = full_clamp(viewport.0.max(viewport.1), self.max_texture_dim);
        drop(nav_guard);

        // Candidate window: ±2 around the current index. The immediate
        // neighbours (±1) are decoded FULL so a single tapped next is a full
        // instant hit (and can slide); the far neighbours (±2) are decoded
        // LOW so warm-up is near-free and a quick flick-past shows something
        // instead of a blank frame (then upgrades to full in the background).
        let mut targets: Vec<(PathBuf, SystemTime, Tier)> = Vec::with_capacity(4);
        for d in 1..=2 {
            // `count` is > 0 here (guarded above). Use `d % count` so the
            // subtraction `idx + count - (d % count)` can't underflow a
            // usize when count < d (e.g. count=1, d=2): `% count` gives 0,
            // so `idx + count - 0 = count`, then `% count = 0` — safe.
            let tier = if d == 1 { Tier::Full } else { Tier::Low };
            let back = (idx + count - d % count) % count;
            let fwd = (idx + d) % count;
            if let Some(it) = items.get(back) {
                targets.push((it.path.clone(), it.modified, tier));
            }
            if let Some(it) = items.get(fwd) {
                targets.push((it.path.clone(), it.modified, tier));
            }
        }

        let mut jobs = self.prefetch.lock();
        // Drop prefetch jobs for paths that are already cached (re-run must
        // not re-decode what's already warm).
        for (path, modified, tier) in targets {
            if self.precache.contains(&path, modified, tier) {
                continue;
            }
            if jobs.iter().any(|j| j.path == path) {
                continue;
            }
            let (td, clim) = match tier {
                Tier::Full => (full_dim, full_dim),
                Tier::Low => (LOW_RES_DIM, LOW_RES_DIM),
            };
            let rx = self.spawn_decode(path.clone(), td, clim, SlideDir::None, 0);
            jobs.push(PrefetchJob { path, modified, rx, tier, upgrade: false });
        }
    }

    pub fn poll(&self) -> Option<DecodeId> {
        // Drain completed background prefetches FIRST so a full-size upgrade
        // that finished since last frame is applied even on frames where no
        // main decode lands (the `pending` slot is empty).
        self.drain_prefetch();

        let mut pending = self.pending.lock();
        let rx = match pending.as_mut() {
            Some(rx) => rx,
            None => return None,
        };
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
            // Upload to an egui texture on the main thread and hand it to
            // the viewer, which stores it in `current_gpu` so the egui
            // painter can render it next frame.
            if let Some(gpu_image) = self.upload_pixels(&pixels) {
                *self.displayed_path.lock() = resp.path.clone();
                self.viewer.lock().set_image_gpu(gpu_image, resp.direction);
            }
        }

        // If the just-landed decode was a LOW preview that still needs a
        // FULL upgrade, kick the full-size decode now (it runs as a
        // prefetch, warming the cache and sharpening the displayed image).
        if let Some(p) = self.pending_upgrade_path.lock().take() {
            if *self.displayed_path.lock() == p {
                let modified = self.nav.lock().current().map(|it| it.modified).unwrap_or_else(|| {
                    // Fall back to a 0-mod stamp; only used for cache keying.
                    std::time::SystemTime::UNIX_EPOCH
                });
                self.request_full_upgrade(p, modified);
            }
        }

        // Replay the next queued navigation step (sequential display). While
        // there are still steps queued, the target will be replaced again
        // immediately, so skip the slide (direct cut) for intermediate steps;
        // only the final settled step plays the full slide. This keeps a fast
        // held walk responsive instead of animating every intermediate frame.
        let mut nq = self.queued_steps.lock();
        let next_step = nq.pop_front();
        let more_queued = !nq.is_empty();
        drop(nq);
        if let Some(dir) = next_step {
            let display_dir = if more_queued { SlideDir::None } else { dir };
            self.viewer.lock().set_slide_duration(0.35);
            self.request_current(display_dir);
        } else {
            // Queue drained — back to the full-duration slide.
            self.viewer.lock().set_slide_duration(0.35);
        }

        // Drain background prefetches: upload + cache them so the next
        // navigation is an instant hit.
        Some(resp.id)
    }

    /// Upload `pixels` to an egui texture (main thread). Returns the
    /// image, or `None` if the upload failed.
    fn upload_pixels(&self, pixels: &DecodedPixels) -> Option<Arc<DecodedGpuImage>> {
        match DecodedGpuImage::from_pixels(&self.ctx, pixels) {
            Ok(gpu_image) => {
                tracing::info!(
                    "Uploaded egui texture for {} ({}x{})",
                    pixels.path, pixels.width, pixels.height,
                );
                Some(Arc::new(gpu_image))
            }
            Err(e) => {
                tracing::error!(
                    "Failed to upload egui texture for {}: {}",
                    pixels.path, e,
                );
                None
            }
        }
    }

    /// Drain completed background prefetch decodes and cache the GPU
    /// images. Failed/incomplete prefetches are dropped silently.
    fn drain_prefetch(&self) {
        let mut jobs = self.prefetch.lock();
        let mut i = 0;
        while i < jobs.len() {
            let done = match jobs[i].rx.try_recv() {
                Ok(r) => Some(r),
                Err(oneshot::error::TryRecvError::Empty) => {
                    i += 1;
                    None
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    jobs.swap_remove(i);
                    None
                }
            };
            let Some(resp) = done else { continue };
            let job = jobs.swap_remove(i);
            if resp.result.is_success {
                if let Some(pixels) = resp.pixels {
                    if let Some(gpu_image) = self.upload_pixels(&pixels) {
                        self.precache.insert(job.path.clone(), job.modified, gpu_image.clone(), job.tier);
                        // This was a full-size upgrade for an image still
                        // displayed as low → sharpen it in place (direct cut).
                        if job.upgrade
                            && job.tier == Tier::Full
                            && *self.displayed_path.lock() == job.path
                        {
                            self.viewer.lock().set_image_gpu(gpu_image, SlideDir::None);
                        }
                    }
                }
            }
            // Stay at index `i` after swap_remove — the next job shifted in.
        }
    }

    /// Clear the pre-decode cache and cancel any in-flight prefetches.
    /// Called on folder navigation so stale entries don't leak across
    /// folders.
    pub fn clear_caches(&self) {
        self.precache.clear();
        // Dropping the receivers makes the tokio send a no-op (Closed),
        // which drain_prefetch will later prune.
        self.prefetch.lock().clear();
    }

    /// True while navigation steps are still waiting to be displayed.
    pub fn has_queued(&self) -> bool {
        !self.queued_steps.lock().is_empty()
    }

    /// True while a decode is in flight OR a navigation step is queued.
    /// Used by the "ready-confirm" arrow-key hold to wait until the
    /// previous image has fully decoded+displayed before advancing.
    pub fn is_busy(&self) -> bool {
        self.pending.lock().is_some() || self.has_queued()
    }

    pub fn current_id(&self) -> DecodeId {
        *self.next_id.lock()
    }
}

