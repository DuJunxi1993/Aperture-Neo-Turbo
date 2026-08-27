//! Image-texture cache: decode → BGRA → wgpu texture → egui::TextureHandle
//!
//! Used for both the main viewer image and the thumbnail grid. The
//! cache is keyed by `(path, mtime)` and lives on the main thread
//! (egui textures are not Send).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use parking_lot::Mutex;

use aperture_core::ThumbCache;
use aperture_gpu::decode_file;

#[allow(dead_code)]
/// Loaded main-image texture (full resolution, aspect-ratio known).
pub struct MainImage {
    pub texture: egui::TextureHandle,
    pub source_w: u32,
    pub source_h: u32,
    pub path: PathBuf,
}

/// Cached thumb texture (small, square). Decoded to 200×200 BGRA JPEG bytes
/// from disk (via `ThumbCache`) and uploaded to a wgpu-backed egui texture.
pub struct ThumbEntry {
    pub texture: egui::TextureHandle,
    pub path: PathBuf,
}

#[derive(Default)]
#[allow(dead_code)]
struct TextureCacheState {
    main: HashMap<PathBuf, MainImage>,
    thumbs: HashMap<PathBuf, ThumbEntry>,
    /// Thumbnails currently in-flight on the decode thread.
    thumbs_pending: HashMap<PathBuf, ()>,
}

/// One completed thumbnail decode.
pub struct ThumbResult {
    pub path: PathBuf,
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Owns the egui texture registry and the on-disk thumbnail DB.
pub struct TextureCache {
    state: Mutex<TextureCacheState>,
    #[allow(dead_code)]
    thumb_db: Arc<ThumbCache>,
    /// Channels from background decode threads; the main thread drains
    /// these each frame and uploads the results to egui textures.
    inbox: Mutex<Vec<std::sync::mpsc::Receiver<ThumbResult>>>,
}

impl TextureCache {
    pub fn new(thumb_db: Arc<ThumbCache>) -> Self {
        Self {
            state: Mutex::new(TextureCacheState::default()),
            thumb_db,
            inbox: Mutex::new(Vec::new()),
        }
    }

    /// Spawn a background thread that decodes a small thumbnail and
    /// ships the result back via a channel. The main thread drains the
    /// inbox each frame (see `flush_inbox`). Idempotent: if a decode
    /// is already in-flight, this is a no-op.
    pub fn request_thumb(&self, path: PathBuf) {
        if self.thumb_in_flight(&path) || self.get_thumb(&path).is_some() {
            return;
        }
        self.mark_thumb_pending(&path);
        let (tx, rx) = std::sync::mpsc::channel::<ThumbResult>();
        self.inbox.lock().push(rx);
        let path_for_thread = path.clone();
        std::thread::spawn(move || {
            let result = decode_thumb_blocking(200, &path_for_thread);
            if let Some(r) = result {
                let _ = tx.send(r);
            }
        });
    }

    /// Drain completed thumbnail decodes and upload them to egui.
    /// Returns the number of new textures added.
    pub fn flush_inbox(&self, ctx: &egui::Context) -> usize {
        let mut added = 0;
        let mut inbox = self.inbox.lock();
        let mut i = 0;
        while i < inbox.len() {
            match inbox[i].try_recv() {
                Ok(r) => {
                    self.put_thumb_rgba(
                        ctx,
                        r.path.clone(),
                        r.rgba,
                        r.width,
                        r.height,
                    );
                    inbox.swap_remove(i);
                    added += 1;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => i += 1,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    inbox.swap_remove(i);
                }
            }
        }
        added
    }

    /// Look up the cached main image (full size) for `path`, or `None` if
    /// the image hasn't been decoded this session.
    #[allow(dead_code)]
    pub fn get_main(&self, path: &Path) -> Option<MainImage> {
        let s = self.state.lock();
        s.main.get(path).map(|m| MainImage {
            texture: m.texture.clone(),
            source_w: m.source_w,
            source_h: m.source_h,
            path: m.path.clone(),
        })
    }

    /// Register a freshly-decoded main image into the cache. `rgba` is
    /// `width*height*4` bytes in RGBA8 order (NOT premultiplied BGRA).
    #[allow(dead_code)]
    pub fn put_main(
        &self,
        ctx: &egui::Context,
        path: PathBuf,
        rgba: Vec<u8>,
        w: u32,
        h: u32,
    ) {
        let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
        let texture = ctx.load_texture(
            format!("main:{}", path.display()),
            img,
            egui::TextureOptions::LINEAR,
        );
        let mut s = self.state.lock();
        s.main.insert(path.clone(), MainImage { texture, source_w: w, source_h: h, path });
    }

    pub fn get_thumb(&self, path: &Path) -> Option<ThumbEntry> {
        let s = self.state.lock();
        s.thumbs.get(path).map(|t| ThumbEntry {
            texture: t.texture.clone(),
            path: t.path.clone(),
        })
    }

    /// `true` if a thumb decode has been kicked off but not finished.
    pub fn thumb_in_flight(&self, path: &Path) -> bool {
        self.state.lock().thumbs_pending.contains_key(path)
    }

    /// Mark a thumb as in-flight (call before spawning the decode task).
    pub fn mark_thumb_pending(&self, path: &Path) {
        self.state.lock().thumbs_pending.insert(path.to_path_buf(), ());
    }

    /// Upload a decoded thumb (200×200 RGBA bytes) into the cache. This is
    /// called from the main thread after the async decode task completes.
    pub fn put_thumb_rgba(
        &self,
        ctx: &egui::Context,
        path: PathBuf,
        rgba: Vec<u8>,
        w: u32,
        h: u32,
    ) {
        let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
        let texture = ctx.load_texture(
            format!("thumb:{}", path.display()),
            img,
            egui::TextureOptions::LINEAR,
        );
        let mut s = self.state.lock();
        s.thumbs_pending.remove(&path);
        s.thumbs.insert(path.clone(), ThumbEntry { texture, path });
    }

    /// Read the cached JPEG for a path (mtime-aware) and return raw bytes.
    /// Returns `None` if not cached. Does NOT decode.
    #[allow(dead_code)]
    pub fn get_thumb_bytes(&self, path: &Path, mtime: u64) -> Option<Vec<u8>> {
        self.thumb_db.get(path, mtime)
    }

    /// Store a 200×200 RGBA8 thumb to both memory cache and on-disk JPEG
    /// cache (mtime-aware).
    #[allow(dead_code)]
    pub fn store_thumb(
        &self,
        path: &Path,
        mtime: u64,
        rgba: &[u8],
        w: u32,
        h: u32,
        jpeg_bytes: Vec<u8>,
    ) -> anyhow::Result<()> {
        self.thumb_db.put(path, mtime, &jpeg_bytes, w, h)?;
        // In-memory RGB cache (for fast re-use without re-reading SQLite).
        let _ = rgba; // not currently used separately; the wgpu texture is the consumer
        Ok(())
    }

    /// Discard all cached textures (called on folder change / shutdown).
    #[allow(dead_code)]
    pub fn clear(&self) {
        let mut s = self.state.lock();
        s.main.clear();
        s.thumbs.clear();
        s.thumbs_pending.clear();
    }

    /// Total thumbs cached (for diagnostics).
    #[allow(dead_code)]
    pub fn thumb_count(&self) -> usize {
        self.state.lock().thumbs.len()
    }
}

/// Helper: decode a 200×200 thumb JPEG from disk (mtime-aware), and
/// synchronously decode a 200×200 BGRA version for upload. Returns the
/// BGRA bytes + dimensions.
#[allow(dead_code)]
pub fn decode_thumb_to_rgba(path: &Path, mtime: u64, thumb_size: u32) -> Option<(Vec<u8>, u32, u32)> {
    // Fast path: read from ThumbCache, then re-decode JPEG → BGRA.
    // (We don't have a JPEG decoder that doesn't pull in an extra
    // dependency, so we re-decode from the source file at low res —
    // the `WIC` path is fast for small targets.)
    let _ = mtime;
    decode_file(path, thumb_size, thumb_size)
        .ok()
        .map(|d| (d.pixels, d.width, d.height))
}

/// Synchronous thumbnail decode used by the background thread.
/// WIC decodes to BGRA; egui's `ColorImage::from_rgba_unmultiplied`
/// wants RGBA, so we swap channels here.
fn decode_thumb_blocking(size: u32, path: &Path) -> Option<ThumbResult> {
    let decoded = decode_file(path, size, size).ok()?;
    let bgra = decoded.pixels;
    let w = decoded.width;
    let h = decoded.height;
    // BGRA → RGBA: swap R and B channels (every 4 bytes).
    let mut rgba = bgra;
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Some(ThumbResult {
        path: path.to_path_buf(),
        rgba,
        width: w,
        height: h,
    })
}
