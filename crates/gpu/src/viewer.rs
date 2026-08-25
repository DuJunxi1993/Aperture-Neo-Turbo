//! Direct2D Viewer — GPU-accelerated image display with animations
//!
//! Each frame: BeginDraw → Clear → SetTransform → DrawBitmap → EndDraw → Present.
//! All transforms are GPU; CPU only computes the matrix array each frame.

use windows::{
    Win32::Foundation::*,
    Win32::Graphics::Direct2D::*,
    Win32::Graphics::Direct2D::Common::*,
    Win32::Graphics::Dxgi::Common::*,
    Foundation::Numerics::Matrix3x2,
};
use std::sync::Arc;
use crate::device::GpuContext;
use crate::bitmap::DecodedBitmap;
use crate::animator::{Animator, AffineTransform};
use crate::swapchain::SwapchainHandle;
use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlideDir {
    None,
    Next,
    Previous,
}

pub struct Direct2DViewer {
    pub gpu: Arc<GpuContext>,
    pub current: Option<DecodedBitmap>,
    /// Outgoing image during a directional slide, with its own fit transform.
    pub previous: Option<PreviousImage>,
    pub animator: Animator,
    pub slide_dir: SlideDir,
    pub viewport_w: u32,
    pub viewport_h: u32,
    /// Viewport top-left in WINDOW coordinates — lets rect transitions
    /// (fullscreen / fit / 1:1) animate across layout changes.
    pub viewport_origin: (f32, f32),
    pub zoom: f32,
    pub offset_x: f32,
    pub offset_y: f32,
    pub fit_scale: f32,
    /// Viewer letterbox/background color (theme-dependent).
    pub bg: [f32; 3],
    /// In-flight screen-rect transition (window coords: x, y, w, h).
    rect_anim: Option<RectAnim>,
    /// Captured image rect before a viewport change (fullscreen toggle) —
    /// the "from" of the transition once the new viewport arrives.
    pending_viewport_anim_from: Option<(f32, f32, f32, f32)>,
}

/// Screen-rect transition between two on-screen image rectangles.
struct RectAnim {
    from: (f32, f32, f32, f32),
    to: (f32, f32, f32, f32),
    start: std::time::Instant,
    dur: f32,
}

/// The image that is sliding out, plus the transform it was displayed with.
pub struct PreviousImage {
    pub bitmap: DecodedBitmap,
    pub fit: f32,
    pub ox: f32,
    pub oy: f32,
}

impl Direct2DViewer {
    pub fn new(gpu: Arc<GpuContext>, viewport_w: u32, viewport_h: u32) -> Self {
        Self {
            gpu,
            current: None,
            previous: None,
            animator: Animator::new(),
            slide_dir: SlideDir::None,
            viewport_w,
            viewport_h,
            viewport_origin: (0.0, 0.0),
            zoom: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
            fit_scale: 1.0,
            bg: [0.059, 0.063, 0.067],
            rect_anim: None,
            pending_viewport_anim_from: None,
        }
    }

    pub fn set_image(&mut self, bitmap: DecodedBitmap, direction: SlideDir) {
        self.slide_dir = direction;
        // Capture the outgoing image with its OWN fit transform before we
        // recompute for the incoming one — both images slide in parallel,
        // each anchored at its own fitted position (iOS Photos style).
        let outgoing = self.current.take().map(|bmp| PreviousImage {
            fit: self.fit_scale,
            ox: self.offset_x,
            oy: self.offset_y,
            bitmap: bmp,
        });
        self.current = Some(bitmap);
        self.compute_fit();
        if direction != SlideDir::None {
            self.previous = outgoing;
            self.animator.start_slide(direction, self.viewport_w as f32);
        } else {
            self.previous = None;
            self.animator.reset();
        }
    }

    fn compute_fit(&mut self) {
        if let Some(ref bmp) = self.current {
            if bmp.width == 0 || bmp.height == 0 {
                self.fit_scale = 1.0;
                return;
            }
            let scale_x = self.viewport_w as f32 / bmp.width as f32;
            let scale_y = self.viewport_h as f32 / bmp.height as f32;
            self.fit_scale = scale_x.min(scale_y);
            self.zoom = self.fit_scale;
            self.offset_x = (self.viewport_w as f32 - bmp.width as f32 * self.zoom) * 0.5;
            self.offset_y = (self.viewport_h as f32 - bmp.height as f32 * self.zoom) * 0.5;
        }
    }

    pub fn render(&mut self, swapchain: &SwapchainHandle, buffer_w: u32, buffer_h: u32) -> Result<()> {
        unsafe {
            self.gpu.d2d_dc.BeginDraw();

            // Clear to the theme background color.
            let bg_color = D2D1_COLOR_F {
                r: self.bg[0], g: self.bg[1], b: self.bg[2], a: 1.0,
            };
            self.gpu.d2d_dc.Clear(Some(&bg_color));

            // Aspect adaptation: when the swapchain buffer's aspect differs
            // from the viewport's (deferred resize during panel drags), the
            // compositor stretches buffer → HWND. Drawing the image fit to
            // a same-aspect sub-rect of the buffer keeps it visually
            // undistorted after the stretch.
            let vw = self.viewport_w as f32;
            let vh = self.viewport_h as f32;
            let base = if buffer_w > 0 && buffer_h > 0 && vw > 0.0 && vh > 0.0 {
                let bw = buffer_w as f32;
                let bh = buffer_h as f32;
                let buf_ar = bw / bh;
                let vp_ar = vw / vh;
                let (cw, ch) = if vp_ar > buf_ar {
                    (bw, bw / vp_ar)
                } else {
                    (bh * vp_ar, bh)
                };
                let ox = (bw - cw) * 0.5;
                let oy = (bh - ch) * 0.5;
                AffineTransform::translate(ox, oy)
                    .mul(AffineTransform::scale(cw / vw))
            } else {
                AffineTransform::identity()
            };

            // Draw previous bitmap (exiting during slide) with its own fit.
            if let (Some(prev), true) = (&self.previous, self.animator.is_sliding()) {
                let prev_t = self.animator.prev_transform(
                    prev.fit, prev.ox, prev.oy, self.slide_dir, self.viewport_w as f32,
                );
                let t = base.mul(prev_t);
                self.draw_bitmap_with_transform(&prev.bitmap.d2d_bitmap, &t);
            }

            // Draw current bitmap — a screen-rect transition (fullscreen /
            // fit / 1:1) overrides the regular transform while in flight.
            if let Some(cur) = &self.current {
                // Finish the transition and commit its final transform.
                if let Some(anim) = &self.rect_anim {
                    if anim.start.elapsed().as_secs_f32() >= anim.dur {
                        let to = anim.to;
                        self.zoom = if cur.width > 0 { to.2 / cur.width as f32 } else { self.zoom };
                        self.offset_x = to.0 - self.viewport_origin.0;
                        self.offset_y = to.1 - self.viewport_origin.1;
                        self.rect_anim = None;
                    }
                }
                let cur_t = if let Some(anim) = &self.rect_anim {
                    let raw = (anim.start.elapsed().as_secs_f32() / anim.dur).min(1.0);
                    let t = 1.0 - (1.0 - raw).powi(4); // ease-out quart
                    let r = (
                        anim.from.0 + (anim.to.0 - anim.from.0) * t,
                        anim.from.1 + (anim.to.1 - anim.from.1) * t,
                        anim.from.2 + (anim.to.2 - anim.from.2) * t,
                        anim.from.3 + (anim.to.3 - anim.from.3) * t,
                    );
                    let s = if cur.width > 0 { r.2 / cur.width as f32 } else { 1.0 };
                    AffineTransform::translate(
                        r.0 - self.viewport_origin.0,
                        r.1 - self.viewport_origin.1,
                    )
                    .mul(AffineTransform::scale(s))
                } else {
                    self.animator.current_transform(
                        self.zoom, self.offset_x, self.offset_y,
                        self.fit_scale, self.slide_dir, self.viewport_w as f32,
                    )
                };
                let t = base.mul(cur_t);
                self.draw_bitmap_with_transform(&cur.d2d_bitmap, &t);
            }

            self.gpu.d2d_dc.EndDraw(None, None)?;
            crate::swapchain::present(swapchain)?;
        }
        Ok(())
    }

    fn draw_bitmap_with_transform(&self, bitmap: &ID2D1Bitmap1, transform: &AffineTransform) {
        unsafe {
            let arr = transform.to_array();
            let matrix = Matrix3x2 {
                M11: arr[0], M12: arr[1],
                M21: arr[2], M22: arr[3],
                M31: arr[4], M32: arr[5],
            };
            self.gpu.d2d_dc.SetTransform(&matrix);
            self.gpu.d2d_dc.DrawBitmap(
                bitmap,
                None,
                1.0,
                D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
                None,
                None,
            );
        }
    }

    pub fn on_wheel(&mut self, delta: i32, cursor_x: f32, cursor_y: f32) {
        let zoom_factor = if delta > 0 { 1.1 } else { 1.0 / 1.1 };
        let new_zoom = (self.zoom * zoom_factor).clamp(0.5, 5.0);
        let cursor_rel_x = cursor_x - self.offset_x;
        let cursor_rel_y = cursor_y - self.offset_y;
        self.offset_x = cursor_x - cursor_rel_x * (new_zoom / self.zoom);
        self.offset_y = cursor_y - cursor_rel_y * (new_zoom / self.zoom);
        self.zoom = new_zoom;
        self.clamp_pan();
        // Instant — animated per-tick zoom feels laggy/jumpy.
        self.animator.reset();
    }

    /// Step zoom by a multiplier (Ctrl + +/-), clamped to 50%..500%.
    pub fn zoom_step(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(0.5, 5.0);
        // Keep the viewport center anchored.
        let cx = self.viewport_w as f32 * 0.5;
        let cy = self.viewport_h as f32 * 0.5;
        let rel_x = cx - self.offset_x;
        let rel_y = cy - self.offset_y;
        self.offset_x = cx - rel_x * factor;
        self.offset_y = cy - rel_y * factor;
        self.clamp_pan();
        self.animator.reset();
    }

    pub fn on_pan(&mut self, dx: f32, dy: f32) {
        self.offset_x += dx;
        self.offset_y += dy;
        self.clamp_pan();
        self.rect_anim = None;
    }

    /// Clamp offsets so the image can never be dragged fully out of view:
    /// images smaller than the viewport stay centered; larger images keep
    /// at least some content covering the viewport.
    fn clamp_pan(&mut self) {
        let Some(bmp) = &self.current else { return };
        let w = bmp.width as f32 * self.zoom;
        let h = bmp.height as f32 * self.zoom;
        let vw = self.viewport_w as f32;
        let vh = self.viewport_h as f32;
        self.offset_x = if w <= vw {
            (vw - w) * 0.5
        } else {
            self.offset_x.clamp(vw - w + 40.0, vw - 40.0)
        };
        self.offset_y = if h <= vh {
            (vh - h) * 0.5
        } else {
            self.offset_y.clamp(vh - h + 40.0, vh - 40.0)
        };
    }

    /// Current on-screen image rectangle in WINDOW coordinates.
    fn image_rect(&self) -> (f32, f32, f32, f32) {
        match &self.current {
            Some(bmp) => (
                self.viewport_origin.0 + self.offset_x,
                self.viewport_origin.1 + self.offset_y,
                bmp.width as f32 * self.zoom,
                bmp.height as f32 * self.zoom,
            ),
            None => (0.0, 0.0, 0.0, 0.0),
        }
    }

    /// Begin a screen-rect transition from the current image rect to
    /// `target` (window coords). Used by fit / 1:1 / fullscreen toggles —
    /// the image travels along the path instead of a context-free zoom.
    pub fn start_rect_anim(&mut self, target: (f32, f32, f32, f32)) {
        if self.current.is_none() {
            return;
        }
        let from = self.image_rect();
        if (from.2 - target.2).abs() < 0.5 && (from.0 - target.0).abs() < 0.5 {
            return; // already there
        }
        self.rect_anim = Some(RectAnim { from, to: target, start: std::time::Instant::now(), dur: 0.28 });
    }

    /// Capture the current image rect as the "from" of an upcoming
    /// viewport transition (fullscreen toggle).
    pub fn mark_viewport_transition(&mut self) {
        if self.current.is_some() {
            self.pending_viewport_anim_from = Some(self.image_rect());
        }
    }

    pub fn fit_to_screen(&mut self) {
        self.compute_fit();
        if let Some(bmp) = &self.current {
            let target = (
                self.viewport_origin.0 + self.offset_x,
                self.viewport_origin.1 + self.offset_y,
                bmp.width as f32 * self.zoom,
                bmp.height as f32 * self.zoom,
            );
            self.start_rect_anim(target);
        }
    }

    /// Set the duration of subsequent slide animations (fast-forward support).
    pub fn set_slide_duration(&mut self, secs: f32) {
        self.animator.set_slide_duration(secs);
    }

    /// 1:1 — one image pixel per screen pixel, centered.
    pub fn zoom_1_to_1(&mut self) {
        if let Some(ref bmp) = self.current {
            self.zoom = 1.0;
            self.offset_x = (self.viewport_w as f32 - bmp.width as f32 * 1.0) * 0.5;
            self.offset_y = (self.viewport_h as f32 - bmp.height as f32 * 1.0) * 0.5;
            let target = (
                self.viewport_origin.0 + self.offset_x,
                self.viewport_origin.1 + self.offset_y,
                bmp.width as f32,
                bmp.height as f32,
            );
            self.start_rect_anim(target);
        }
    }

    pub fn resize(&mut self, w: u32, h: u32, x: f32, y: f32) {
        let size_changed = self.viewport_w != w || self.viewport_h != h;
        let origin_changed =
            (self.viewport_origin.0 - x).abs() > 0.5 || (self.viewport_origin.1 - y).abs() > 0.5;
        self.viewport_w = w;
        self.viewport_h = h;
        self.viewport_origin = (x, y);
        self.compute_fit();
        // Fullscreen (or panel-layout) transitions: animate the image from
        // where it was to the new fit — a path animation, not a zoom.
        if (size_changed || origin_changed) && !self.animator.is_sliding() {
            if let Some(from) = self.pending_viewport_anim_from.take() {
                if let Some(bmp) = &self.current {
                    let target = (
                        x + self.offset_x,
                        y + self.offset_y,
                        bmp.width as f32 * self.zoom,
                        bmp.height as f32 * self.zoom,
                    );
                    self.rect_anim = Some(RectAnim {
                        from,
                        to: target,
                        start: std::time::Instant::now(),
                        dur: 0.30,
                    });
                }
            }
        }
    }

    pub fn viewport_size(&self) -> (u32, u32) { (self.viewport_w, self.viewport_h) }
    pub fn zoom_value(&self) -> f32 { self.zoom }
    pub fn offset(&self) -> (f32, f32) { (self.offset_x, self.offset_y) }
}