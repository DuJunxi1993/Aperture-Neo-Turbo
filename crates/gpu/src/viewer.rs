//! Direct2D Viewer — GPU-accelerated image display with animations
//!
//! Each frame: BeginDraw → Clear → SetTransform → DrawBitmap → EndDraw → Present.
//! All transforms are GPU; CPU only computes the matrix array each frame.

use windows::{
    Win32::Graphics::Direct2D::*,
    Win32::Graphics::Direct2D::Common::*,
    Foundation::Numerics::Matrix3x2,
};
use std::sync::Arc;
use crate::device::GpuContext;
use crate::bitmap::DecodedBitmap;
use crate::texture::DecodedGpuImage;
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
    /// Phase 3: parallel `current` holding a wgpu texture. The image
    /// quad shader samples this. Decoded by `DecodeCoordinator` and
    /// uploaded to a Bgra8UnormSrgb texture. Phase 4 deletes
    /// `current` and this becomes the sole source-of-truth.
    pub current_gpu: Option<Arc<DecodedGpuImage>>,
    /// Outgoing image during a directional slide, with its own fit transform.
    pub previous: Option<PreviousImage>,
    /// Phase 3: parallel `previous` holding the wgpu texture of the
    /// outgoing image during a directional slide.
    pub previous_gpu: Option<Arc<DecodedGpuImage>>,
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
    /// Phase 11: the fullscreen transition TARGET in window coords,
    /// supplied by the host (which knows the FINAL panel layout).
    /// Consumed together with `pending_viewport_anim_from` on the
    /// first resize after the toggle. Keeping the target in window
    /// coords makes the image's animation path independent of the
    /// tree/thumb panels' collapse animation — the viewport origin
    /// churns during the transition but the path does not.
    pending_viewport_target: Option<(f32, f32, f32, f32)>,
    /// Phase 3: image rotation in quarter-turns (0 / 1 / 2 / 3 =
    /// 0° / 90° / 180° / 270° clockwise). Stored as an integer so
    /// round-tripping through set_image (which resets to 0) is
    /// lossless; the actual transform is `rotation * 90°` degrees.
    /// NOT persisted across images (each new image starts at 0).
    rotation: u8,
}

/// Screen-rect transition between two on-screen image rectangles.
/// `window_space`: when true, `from`/`to` are in WINDOW coordinates
/// and the render step subtracts the CURRENT viewport origin each
/// frame (fullscreen transitions — the viewport moves under the
/// animation); when false they are viewer-relative (fit / 1:1 — the
/// viewport is static).
struct RectAnim {
    from: (f32, f32, f32, f32),
    to: (f32, f32, f32, f32),
    start: std::time::Instant,
    dur: f32,
    window_space: bool,
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
            current_gpu: None,
            previous: None,
            previous_gpu: None,
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
            pending_viewport_target: None,
            rotation: 0,
        }
    }

    pub fn set_image(&mut self, bitmap: DecodedBitmap, direction: SlideDir) {
        // Phase 3: every new image starts unrotated. Persisted
        // rotation across images was an explicit design choice —
        // most image viewers reset on navigate-next.
        self.rotation = 0;
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
        // Mirror the outgoing to the wgpu side. Phase 4 collapses
        // these into a single wgpu field.
        self.previous_gpu = self.current_gpu.take();
        self.compute_fit();
        if direction != SlideDir::None {
            self.previous = outgoing;
            self.animator.start_slide(direction, self.viewport_w as f32);
        } else {
            self.previous = None;
            self.animator.reset();
        }
    }

    /// Phase 3: receive the wgpu texture uploaded by the decode
    /// coordinator. Caller is `MainWindow::render_frame` after
    /// `coordinator.poll()`. The texture is stored in `current_gpu` so
    /// the image quad bind group can reference it.
    pub fn set_image_gpu(&mut self, image: Arc<DecodedGpuImage>, direction: SlideDir) {
        self.rotation = 0;
        self.slide_dir = direction;
        let outgoing = self.current_gpu.take();
        self.current_gpu = Some(image);
        self.previous_gpu = outgoing;
        if direction != SlideDir::None {
            self.animator.start_slide(direction, self.viewport_w as f32);
        } else {
            self.animator.reset();
        }
    }

    /// Phase 3: per-frame uniform for the wgpu image-quad pipeline.
    ///
    /// Builds the mat3x3 transform that maps a screen-local framebuffer
    /// pixel position (relative to the viewer's top-left) to the
    /// corresponding decoded-image pixel coordinate. The transform is
    /// column-major; the shader's `mat3x3 * vec3(pixel, 1.0)` produces
    /// the image-pixel position.
    ///
    /// **Initial implementation**: handles the static fit (no rotation,
    /// no slide animation, no rect_anim). This is sufficient for the
    /// first commit of Phase 3 — the image appears at its fit-to-viewer
    /// size, no rotation, no slide. Subsequent commits add rotation +
    /// slide + rect-anim by extending this method.
    pub fn gpu_uniforms(
        &self,
        viewer_rect_min: (f32, f32),
        viewer_rect_size: (f32, f32),
        bg: [f32; 3],
    ) -> crate::ImageQuadUniforms {
        // Lazy conversion: not in the call-site hot path but keeps the
        // math readable. We compute image-pixel space from screen-local
        // pixel space via the displayed-bitmap's per-quadrant transform,
        // then translate that into the mat3x3 column-major form the
        // shader expects.
        let (img_w, img_h) = match &self.current_gpu {
            Some(img) => (img.width as f32, img.height as f32),
            None => {
                // No image bound yet; emit the bg-only uniform and let
                // the fragment shader paint the whole rect as bg.
                return crate::ImageQuadUniforms {
                    col0: [1.0, 0.0, 0.0],
                    col1: [0.0, 1.0, 0.0],
                    col2: [0.0, 0.0, 1.0],
                    viewer_rect_min: [viewer_rect_min.0, viewer_rect_min.1],
                    viewer_rect_size: [viewer_rect_size.0, viewer_rect_size.1],
                    texture_size: [0.0, 0.0],
                    bg: [bg[0], bg[1], bg[2], 1.0],
                    has_image: 0,
                    _pad: [0; 3],
                };
            }
        };

        // The image fills the viewer rect via compute_fit (called from
        // set_image / set_image_gpu). The fit transforms screen-space
        // (relative to the viewer's top-left, after subtracting
        // offset_x/offset_y) to image-space. With rotation=0 and no
        // slide, this is a uniform scale + translation:
        //   image_x = (screen_x - offset_x) / fit_scale   (zoom = fit_scale;
        //                                                       offset_x is
        //                                                       already in
        //                                                       screen-local)
        // Wait — the D2D code uses zoom/offset with offset_x being
        // CENTER-of-image offset. In the existing math:
        //   x' = s·(x − ox)  where s = zoom, ox = offset_x (already
        //   in source pixels). So:
        //   image_x = (screen_x - ox) / s
        // We use self.zoom and self.offset_x directly.
        let s = self.zoom;
        let ox = self.offset_x;
        let oy = self.offset_y;

        // mat3x3 column-major: col0 = (1/s, 0, 0),
        //                       col1 = (0, 1/s, 0),
        //                       col2 = (ox/s, oy/s, 1).
        // Test: mat * vec3(screen_x, screen_y, 1) = (screen_x/s + ox/s,
        //                                                screen_y/s + oy/s,
        //                                                1)
        //            image_x = screen_x / s + ox / s = (screen_x + ox) / s
        // But the D2D convention is `x' = s·(x − ox)`, which is the
        // screen-space (already without offset) perspective. The shader
        // takes screen-local pixel (relative to viewer rect), so:
        //   image_x = (screen_local_x - ox) / s
        // We need mat that maps screen_local → image such that:
        //   img = (screen_local - offset) / scale
        //       = screen_local / scale - offset / scale
        // So mat (col-major) is:
        //   col0 = (1/s, 0, 0)
        //   col1 = (0, 1/s, 0)
        //   col2 = (-ox/s, -oy/s, 1)
        // Check: mat * vec3(screen_local.x, screen_local.y, 1)
        //      = (screen_local.x/s - ox/s, screen_local.y/s - oy/s, 1)
        //      = ((screen_local - offset) / s, 1) ✓
        let inv_s = if s > 0.0 { 1.0 / s } else { 1.0 };
        crate::ImageQuadUniforms {
            col0: [inv_s, 0.0, 0.0],
            col1: [0.0, inv_s, 0.0],
            col2: [-ox * inv_s, -oy * inv_s, 1.0],
            viewer_rect_min: [viewer_rect_min.0, viewer_rect_min.1],
            viewer_rect_size: [viewer_rect_size.0, viewer_rect_size.1],
            texture_size: [img_w, img_h],
            bg: [bg[0], bg[1], bg[2], 1.0],
            has_image: 1,
            _pad: [0; 3],
        }
    }

    /// Phase 3: build a transform that rotates the current image
    /// by `rotation * 90°` clockwise around the image's
    /// pre-rotation centre. Combined with the existing fit/zoom
    /// Displayed (rotation-aware) dimensions: a 90/270 turn swaps
    /// the bitmap's width and height. Used by fit / rect-anim targets.
    fn effective_size(&self) -> (f32, f32) {
        match &self.current {
            Some(b) => {
                let (w, h) = (b.width as f32, b.height as f32);
                if self.rotation % 2 == 1 { (h, w) } else { (w, h) }
            }
            None => (0.0, 0.0),
        }
    }

    /// Phase 11: build the FULL image transform for a given viewer-
    /// relative position and scale, with rotation as a PER-QUADRANT
    /// hardcoded matrix. No sin/cos, no mul chains — each entry is the
    /// closed-form mapping written directly against the convention
    /// D2D actually applies (row-vector: x' = x·M11 + y·M21 + M31,
    /// y' = x·M12 + y·M22 + M32 — confirmed empirically by the Phase
    /// 7/10 rotation attempts, where column-convention entries
    /// misplaced 90/270 exactly as a transpose would).
    ///
    /// Desired screen mappings (y-down, +90 = clockwise), with
    /// sx = x·s, sy = y·s, sw = bitmap width·s, sh = height·s:
    ///
    ///   0°:   (sx + dx,        sy + dy)
    ///   90°:  (−sy + dx + sh,  sx + dy)
    ///   180°: (−sx + dx + sw,  −sy + dy + sh)
    ///   270°: (sy + dx,        −sx + dy + sw)
    ///
    /// In every quadrant the displayed bounding box lands exactly at
    /// [d, d + eff·s], matching rotation-aware compute_fit. (If the
    /// rotation DIRECTION ever feels inverted, swap the 90/270 arms —
    /// centering is unaffected either way.)
    fn display_transform(&self, dx: f32, dy: f32, s: f32) -> AffineTransform {
        let (bw, bh) = match &self.current {
            Some(b) => (b.width as f32, b.height as f32),
            None => return AffineTransform::identity(),
        };
        let sw = bw * s;
        let sh = bh * s;
        match self.rotation {
            // 90 CW: x' = −s·y + dx + sh ; y' = s·x + dy
            1 => AffineTransform { m11: 0.0, m12: s, m21: -s, m22: 0.0, dx: dx + sh, dy },
            // 180: x' = −s·x + dx + sw ; y' = −s·y + dy + sh
            2 => AffineTransform { m11: -s, m12: 0.0, m21: 0.0, m22: -s, dx: dx + sw, dy: dy + sh },
            // 270 CW: x' = s·y + dx ; y' = −s·x + dy + sw
            3 => AffineTransform { m11: 0.0, m12: -s, m21: s, m22: 0.0, dx, dy: dy + sw },
            // 0: identity scale + translate
            _ => AffineTransform { m11: s, m12: 0.0, m21: 0.0, m22: s, dx, dy },
        }
    }

    fn compute_fit(&mut self) {
        if let Some(ref bmp) = self.current {
            if bmp.width == 0 || bmp.height == 0 {
                self.fit_scale = 1.0;
                return;
            }
            // Phase 9: rotation-aware fit — a 90/270 turn swaps the
            // displayed width and height, so the fit must be computed
            // against the ROTATED dimensions or the rotated image
            // overflows the viewport.
            let (dw, dh) = if self.rotation % 2 == 1 {
                (bmp.height as f32, bmp.width as f32)
            } else {
                (bmp.width as f32, bmp.height as f32)
            };
            let scale_x = self.viewport_w as f32 / dw;
            let scale_y = self.viewport_h as f32 / dh;
            self.fit_scale = scale_x.min(scale_y);
            self.zoom = self.fit_scale;
            self.offset_x = (self.viewport_w as f32 - dw * self.zoom) * 0.5;
            self.offset_y = (self.viewport_h as f32 - dh * self.zoom) * 0.5;
        }
    }

    pub fn render(&mut self, swapchain: &SwapchainHandle, buffer_w: u32, buffer_h: u32) -> Result<()> {
        // NOTE: do NOT skip the draw pass when "nothing changed".
        // A multi-buffer DXGI flip-model swapchain rotates buffers on
        // every Present, so presenting without redrawing shows a
        // STALE back buffer (the frame from 2 presents ago), which
        // alternates with freshly drawn frames as an visible flicker.
        // Every frame must fully redraw BeginDraw/Clear/DrawBitmap.
        unsafe {
            self.gpu.d2d_dc.BeginDraw();

            // Clear to the theme background color.
            let bg_color = D2D1_COLOR_F {
                r: self.bg[0], g: self.bg[1], b: self.bg[2], a: 1.0,
            };
            self.gpu.d2d_dc.Clear(Some(&bg_color));

            // STRETCH pre-compensation: when the swapchain buffer's
            // size differs from the viewport's (deferred ResizeBuffers
            // during a fullscreen toggle, panel drag, or animation),
            // DXGI_SCALING_STRETCH non-uniformly stretches the buffer
            // to fit the HWND. The aspect-sub-rect base we used to use
            // (draw a same-aspect sub-rect inside the buffer, leaving
            // letterbox bands) only worked when the buffer/viewport
            // aspect ratio matched within STRETCH's tolerance — for
            // large jumps (e.g. fullscreen enter) the OS still
            // non-uniformly scales the buffer and the letterbox
            // edges go visibly off-axis.
            //
            // New approach: pre-scale the source content by (bw/vw,
            // bh/vh) so the buffer content, after STRETCH, lands
            // pixel-perfect on the viewport. buffer==viewport
            // degenerates to scale(1,1) (identity), so steady state
            // is unchanged. Combined with the affine-multiply chain
            // (`base × current_image_transform × rotation_matrix`)
            // the math composes cleanly and survives arbitrarily
            // large aspect mismatches.
            let vw = self.viewport_w as f32;
            let vh = self.viewport_h as f32;
            let base = if buffer_w > 0 && buffer_h > 0 && vw > 0.0 && vh > 0.0 {
                let bw = buffer_w as f32;
                let bh = buffer_h as f32;
                AffineTransform::scale_xy(bw / vw, bh / vh)
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
                // Phase 9: rotation-aware displayed width for the
                // rect-anim scale factor (odd quarter-turns swap w/h).
                let disp_w = if self.rotation % 2 == 1 { cur.height as f32 } else { cur.width as f32 };
                // Finish the transition and commit its final transform.
                if let Some(anim) = &self.rect_anim {
                    if anim.start.elapsed().as_secs_f32() >= anim.dur {
                        let to = anim.to;
                        self.zoom = if disp_w > 0.0 { to.2 / disp_w } else { self.zoom };
                        if anim.window_space {
                            // Phase 11: window-space target — convert
                            // back to viewer-relative using the CURRENT
                            // origin (panels have settled by commit time;
                            // the anim outlives the 120ms panel tween).
                            self.offset_x = to.0 - self.viewport_origin.0;
                            self.offset_y = to.1 - self.viewport_origin.1;
                        } else {
                            self.offset_x = to.0;
                            self.offset_y = to.1;
                        }
                        self.rect_anim = None;
                    }
                }
                let cur_t = if let Some(anim) = &self.rect_anim {
                    let raw = (anim.start.elapsed().as_secs_f32() / anim.dur).min(1.0);
                    // Phase 9: ease-IN-OUT cubic (zero velocity at both
                    // ends). The previous quart-OUT peaked in speed at
                    // t=0, so the first frame of the path animation
                    // visually teleported away from the resting
                    // position before the eye could track it.
                    let t = if raw < 0.5 {
                        4.0 * raw * raw * raw
                    } else {
                        1.0 - (-2.0 * raw + 2.0).powi(3) / 2.0
                    };
                    let r = (
                        anim.from.0 + (anim.to.0 - anim.from.0) * t,
                        anim.from.1 + (anim.to.1 - anim.from.1) * t,
                        anim.from.2 + (anim.to.2 - anim.from.2) * t,
                        anim.from.3 + (anim.to.3 - anim.from.3) * t,
                    );
                    let s = if disp_w > 0.0 { r.2 / disp_w } else { 1.0 };
                    if anim.window_space {
                        // Phase 11: subtract the CURRENT viewport origin
                        // each frame — the image's window-space path is
                        // fixed while the viewport (panel widths) churns
                        // underneath, so the motion cannot sway.
                        self.display_transform(
                            r.0 - self.viewport_origin.0,
                            r.1 - self.viewport_origin.1,
                            s,
                        )
                    } else {
                        // Phase 10: full per-quadrant chain (see
                        // display_transform).
                        self.display_transform(r.0, r.1, s)
                    }
                } else if !self.animator.is_sliding() && self.rotation != 0 {
                    // Phase 10: resting frame with rotation — build the
                    // full chain directly (animator's transform has no
                    // rotation slot).
                    self.display_transform(self.offset_x, self.offset_y, self.zoom)
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
        // Phase 10: rotation-aware — the displayed size for odd
        // quarter-turns is height×width, so panning bounds must be
        // computed from the EFFECTIVE dims or dragging a rotated
        // image misbehaves (clamped against the wrong box).
        let Some(bmp) = &self.current else { return };
        let (ew, eh) = if self.rotation % 2 == 1 {
            (bmp.height as f32, bmp.width as f32)
        } else {
            (bmp.width as f32, bmp.height as f32)
        };
        let w = ew * self.zoom;
        let h = eh * self.zoom;
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

    /// Begin a screen-rect transition from the current image rect to
    /// `target`. Both `from` and `target` are in VIEWER-RELATIVE
    /// coords (offset from the viewport's top-left) — the same coord
    /// system the non-animated path uses (`offset_x/offset_y`). The
    /// width/height components are EFFECTIVE (rotation-aware)
    /// displayed sizes, matching compute_fit. Used by fit / 1:1
    /// toggles — the image travels along the path instead of a
    /// context-free zoom.
    pub fn start_rect_anim(&mut self, target: (f32, f32, f32, f32)) {
        if self.current.is_none() {
            return;
        }
        let (ew, eh) = self.effective_size();
        let from = (self.offset_x, self.offset_y, ew * self.zoom, eh * self.zoom);
        if (from.2 - target.2).abs() < 0.5 && (from.0 - target.0).abs() < 0.5 {
            return; // already there
        }
        self.rect_anim = Some(RectAnim {
            from,
            to: target,
            start: std::time::Instant::now(),
            dur: 0.28,
            window_space: false,
        });
    }

    /// Capture the current image rect as the "from" of an upcoming
    /// viewport transition (fullscreen toggle). Phase 11: captured in
    /// WINDOW coords (origin + offset) — the fullscreen animation runs
    /// in window space so the tree/thumb panels' collapse animation
    /// (which churns the viewport origin every frame) cannot bend the
    /// image's path.
    pub fn mark_viewport_transition(&mut self) {
        if self.current.is_some() {
            let (ew, eh) = self.effective_size();
            self.pending_viewport_anim_from = Some((
                self.viewport_origin.0 + self.offset_x,
                self.viewport_origin.1 + self.offset_y,
                ew * self.zoom,
                eh * self.zoom,
            ));
        }
    }

    /// Phase 11: supply the fullscreen transition TARGET in window
    /// coords. The host computes it from the FINAL panel layout
    /// (panel user-widths, not the mid-flight animation widths), so
    /// the target is stable from the moment the toggle happens and
    /// the image glides straight to it while the viewport settles
    /// underneath.
    pub fn set_viewport_target(&mut self, target: (f32, f32, f32, f32)) {
        self.pending_viewport_target = Some(target);
    }

    /// Phase 11: where the image WILL sit once a viewport of
    /// (fx, fy, fw, fh) settles — window coords, centered fit. Used
    /// by the host to build the fullscreen animation target before
    /// the viewport actually reaches that layout.
    pub fn window_target_for_viewport(
        &self, fx: f32, fy: f32, fw: u32, fh: u32,
    ) -> (f32, f32, f32, f32) {
        let (ew, eh) = self.effective_size();
        if ew <= 0.0 || eh <= 0.0 || fw == 0 || fh == 0 {
            return (fx, fy, 0.0, 0.0);
        }
        let z = (fw as f32 / ew).min(fh as f32 / eh);
        (
            fx + (fw as f32 - ew * z) * 0.5,
            fy + (fh as f32 - eh * z) * 0.5,
            ew * z,
            eh * z,
        )
    }

    pub fn fit_to_screen(&mut self) {
        self.compute_fit();
        if self.current.is_some() {
            // Phase 8: viewer-relative target (matches `from` in
            // start_rect_anim and the non-animated offset_x/y path).
            // Phase 9: effective (rotation-aware) displayed size.
            let (ew, eh) = self.effective_size();
            let target = (
                self.offset_x,
                self.offset_y,
                ew * self.zoom,
                eh * self.zoom,
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
            // Phase 9: effective dims — for odd quarter-turns the
            // DISPLAYED pixel size is height×width of the bitmap.
            let (dw, dh) = if self.rotation % 2 == 1 {
                (bmp.height as f32, bmp.width as f32)
            } else {
                (bmp.width as f32, bmp.height as f32)
            };
            self.zoom = 1.0;
            self.offset_x = (self.viewport_w as f32 - dw) * 0.5;
            self.offset_y = (self.viewport_h as f32 - dh) * 0.5;
            let target = (
                self.offset_x,
                self.offset_y,
                dw,
                dh,
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

        // Phase 11: fullscreen transition — the target was supplied by
        // the host in WINDOW coords (final panel layout), and the from
        // was captured in window coords at toggle time. Create the
        // window-space anim on the first resize after the toggle;
        // afterwards the viewport can churn freely (panels collapsing)
        // without touching the path. Re-target only when the actual
        // WINDOW size changes (rare mid-transition).
        if let (Some(from), Some(to)) = (
            self.pending_viewport_anim_from.take(),
            self.pending_viewport_target.take(),
        ) {
            self.rect_anim = Some(RectAnim {
                from,
                to,
                start: std::time::Instant::now(),
                dur: 0.35,
                window_space: true,
            });
        }

        // Fit for the new viewport (drives zoom/offset once any anim
        // completes; the completion commit converts the window-space
        // target back using the CURRENT origin).
        self.compute_fit();

        if let Some(mut anim) = self.rect_anim.take() {
            // Re-target only on a real window resize (not viewport
            // churn): recompute the window-space target against the
            // new window size. The host refreshes pending target via
            // set_viewport_target on toggle; here we keep the existing
            // target unless the anim is viewer-local (fit / 1:1).
            if !anim.window_space && (size_changed || origin_changed) {
                let (ew, eh) = self.effective_size();
                anim.to = (self.offset_x, self.offset_y, ew * self.zoom, eh * self.zoom);
            }
            self.rect_anim = Some(anim);
        }
    }

    /// Phase 9: true while any visual transition is running (slide or
    /// rect path animation). ViewerChildWindow::flush_pending_resize
    /// checks this to force an immediate ResizeBuffers instead of the
    /// 150 ms debounce — presenting through a stale buffer during a
    /// transition applies DXGI_STRETCH's non-uniform scale on top of
    /// the animated transform, which reads as bounce / skew.
    pub fn is_transitioning(&self) -> bool {
        self.animator.is_animating() || self.rect_anim.is_some()
    }

    pub fn viewport_size(&self) -> (u32, u32) { (self.viewport_w, self.viewport_h) }
    pub fn zoom_value(&self) -> f32 { self.zoom }
    pub fn offset(&self) -> (f32, f32) { (self.offset_x, self.offset_y) }

    /// Phase 3: read the current rotation in quarter-turns.
    pub fn rotation(&self) -> u8 { self.rotation }

    /// Phase 3: set the rotation in quarter-turns (0..=3). Persists
    /// only until the next set_image (which resets to 0).
    ///
    /// Phase 9: snap immediately instead of running the fit rect
    /// animation. The rotation matrix applies instantly while a rect
    /// anim would still be interpolating the UNROTATED rect — the two
    /// disagree for every intermediate frame, which read as the image
    /// teleporting / spinning around the wrong pivot. A direct
    /// compute_fit + snap is visually clean: one frame it's portrait,
    /// the next it's landscape, centred and fully in view.
    pub fn set_rotation(&mut self, q: u8) {
        self.rotation = q & 3;
        self.rect_anim = None;
        self.pending_viewport_anim_from = None;
        self.pending_viewport_target = None;
        self.compute_fit();
    }

    /// Phase 3: true when the current zoom is within 1% of the
    /// auto-fit scale. Drives the fit↔1:1 cycle button label /
    /// behaviour.
    pub fn is_fit_scale(&self) -> bool {
        (self.zoom - self.fit_scale).abs() < 0.01
    }
}
