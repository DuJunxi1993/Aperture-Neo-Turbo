//! Viewer state machine — image display with animations
//!
//! Holds fit/zoom/offset/rotation state and rect/slide animations.
//! The actual rendering is done by the wgpu image-quad pipeline in
//! `window.rs` — this module only computes transforms and uniform data.

use std::sync::Arc;
use crate::texture::DecodedGpuImage;
use crate::animator::{Animator, AffineTransform};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlideDir {
    None,
    Next,
    Previous,
}

pub struct Direct2DViewer {
    pub current_gpu: Option<Arc<DecodedGpuImage>>,
    pub previous_gpu: Option<Arc<DecodedGpuImage>>,
    pub animator: Animator,
    pub slide_dir: SlideDir,
    pub viewport_w: u32,
    pub viewport_h: u32,
    pub viewport_origin: (f32, f32),
    pub zoom: f32,
    pub offset_x: f32,
    pub offset_y: f32,
    pub fit_scale: f32,
    pub bg: [f32; 3],
    rect_anim: Option<RectAnim>,
    pending_viewport_anim_from: Option<(f32, f32, f32, f32)>,
    pending_viewport_target: Option<(f32, f32, f32, f32)>,
    rotation: u8,
}

struct RectAnim {
    from: (f32, f32, f32, f32),
    to: (f32, f32, f32, f32),
    start: std::time::Instant,
    dur: f32,
    window_space: bool,
}

impl Direct2DViewer {
    pub fn new(viewport_w: u32, viewport_h: u32) -> Self {
        Self {
            current_gpu: None,
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
        self.compute_fit();
    }

    /// Build the image-quad uniform for either the current image
    /// (`for_previous = false`) or the outgoing previous image
    /// (`for_previous = true`). When `previous_gpu` is None the call
    /// still succeeds but `has_image = 0`, so the render pass can be
    /// skipped in `submit_wgpu_frame`.
    ///
    /// The affine comes from three sources, in priority order:
    /// 1. `current_rect_anim_transform` — fullscreen / fit-zoom /
    ///    rotation target glide. Wins over the slide animation.
    /// 2. `animator.current_transform` — only meaningful during a
    ///    slide. For the current image this is the incoming slide
    ///    (offset+shift); for the previous image we apply the same
    ///    shift but anchored at the previous image's prior fit
    ///    position so the two images move in opposite directions
    ///    across the viewer rect.
    /// 3. `display_transform(offset, offset_y, zoom)` — the static
    ///    fit, including rotation.
    ///
    /// The shader samples with screen→image, so we invert the
    /// image→screen affine returned by those helpers.
    pub fn gpu_uniforms(
        &self,
        viewer_rect_min: (f32, f32),
        viewer_rect_size: (f32, f32),
        bg: [f32; 3],
    ) -> crate::ImageQuadUniforms {
        self.gpu_uniforms_for(viewer_rect_min, viewer_rect_size, bg, false)
    }

    pub fn gpu_uniforms_for(
        &self,
        viewer_rect_min: (f32, f32),
        viewer_rect_size: (f32, f32),
        bg: [f32; 3],
        for_previous: bool,
    ) -> crate::ImageQuadUniforms {
        let img_ref = if for_previous {
            self.previous_gpu.as_ref()
        } else {
            self.current_gpu.as_ref()
        };
        let (img_w, img_h) = match img_ref {
            Some(img) => (img.width as f32, img.height as f32),
            None => {
                return crate::ImageQuadUniforms {
                    col0: [1.0, 0.0, 0.0],
                    _pad_col0: 0,
                    col1: [0.0, 1.0, 0.0],
                    _pad_col1: 0,
                    col2: [0.0, 0.0, 1.0],
                    _pad_col2: 0,
                    viewer_rect_min: [viewer_rect_min.0, viewer_rect_min.1],
                    viewer_rect_size: [viewer_rect_size.0, viewer_rect_size.1],
                    texture_size: [0.0, 0.0],
                    _pad_texture: [0; 2],
                    bg: [bg[0], bg[1], bg[2], 1.0],
                    has_image: 0,
                    _pad: [0; 3],
                };
            }
        };

        // 1. Rect-anim (fullscreen / fit-glide) wins outright.
        // 2. Slide animator: current image uses slide-in (offset+shift);
        //    previous image uses the same shift but anchored at its
        //    own previous fit position so the two move in opposite
        //    directions across the viewer rect.
        // 3. Static fit (with rotation handled by display_transform).
        let (m11, m12, m21, m22, dx, dy) = if let Some(t) = self.current_rect_anim_transform() {
            (t.m11, t.m12, t.m21, t.m22, t.dx, t.dy)
        } else if self.animator.is_sliding() {
            let (ew, _eh) = self.effective_size();
            let slide = self.animator.current_transform(
                self.zoom, self.offset_x, self.offset_y,
                self.fit_scale, self.slide_dir, ew,
            );
            if for_previous {
                // Previous image: reverse the slide direction so the
                // outgoing image moves opposite the incoming one.
                let dir = match self.slide_dir {
                    crate::viewer::SlideDir::Next => -1.0,
                    crate::viewer::SlideDir::Previous => 1.0,
                    crate::viewer::SlideDir::None => 0.0,
                };
                let vw = self.viewport_w as f32;
                let shift = dir * vw * (1.0 - slide.m11.max(0.0).min(1.0));
                // Place previous at its prior fit (offset,offset_y) and
                // shift by `shift` in the OPPOSITE direction of the
                // incoming slide. `current_transform`'s base is
                // `offset_x` so the inverse direction is `offset_x
                // - shift*sign(incoming)`. We just override dx.
                (slide.m11, slide.m12, slide.m21, slide.m22,
                 self.offset_x - shift, slide.dy)
            } else {
                (slide.m11, slide.m12, slide.m21, slide.m22, slide.dx, slide.dy)
            }
        } else {
            let t = self.display_transform(self.offset_x, self.offset_y, self.zoom);
            (t.m11, t.m12, t.m21, t.m22, t.dx, t.dy)
        };

        // Invert the 2×2 affine in homogeneous coords. For rotation by
        // multiples of 90° + uniform scale, det(m11*m22 - m12*m21) is
        // `s*s` — always positive — so the inverse is well-behaved.
        let det = m11 * m22 - m12 * m21;
        let inv_det = if det.abs() > 1e-6 { 1.0 / det } else { 0.0 };
        let im11 =  m22 * inv_det;
        let im12 = -m12 * inv_det;
        let im21 = -m21 * inv_det;
        let im22 =  m11 * inv_det;
        let idx = -(im11 * dx + im12 * dy);
        let idy = -(im21 * dx + im22 * dy);
        crate::ImageQuadUniforms {
            col0: [im11, im21, 0.0],
            _pad_col0: 0,
            col1: [im12, im22, 0.0],
            _pad_col1: 0,
            col2: [idx, idy, 1.0],
            _pad_col2: 0,
            viewer_rect_min: [viewer_rect_min.0, viewer_rect_min.1],
            viewer_rect_size: [viewer_rect_size.0, viewer_rect_size.1],
            texture_size: [img_w, img_h],
            _pad_texture: [0; 2],
            bg: [bg[0], bg[1], bg[2], 1.0],
            has_image: 1,
            _pad: [0; 3],
        }
    }

    fn effective_size(&self) -> (f32, f32) {
        match &self.current_gpu {
            Some(img) => {
                let (w, h) = (img.width as f32, img.height as f32);
                if self.rotation % 2 == 1 { (h, w) } else { (w, h) }
            }
            None => (0.0, 0.0),
        }
    }

    pub fn display_transform(&self, dx: f32, dy: f32, s: f32) -> AffineTransform {
        let (bw, bh) = match &self.current_gpu {
            Some(img) => (img.width as f32, img.height as f32),
            None => return AffineTransform::identity(),
        };
        let sw = bw * s;
        let sh = bh * s;
        match self.rotation {
            1 => AffineTransform { m11: 0.0, m12: s, m21: -s, m22: 0.0, dx: dx + sh, dy },
            2 => AffineTransform { m11: -s, m12: 0.0, m21: 0.0, m22: -s, dx: dx + sw, dy: dy + sh },
            3 => AffineTransform { m11: 0.0, m12: -s, m21: s, m22: 0.0, dx, dy: dy + sw },
            _ => AffineTransform { m11: s, m12: 0.0, m21: 0.0, m22: s, dx, dy },
        }
    }

    fn compute_fit(&mut self) {
        if let Some(ref img) = self.current_gpu {
            let (dw, dh) = if self.rotation % 2 == 1 {
                (img.height as f32, img.width as f32)
            } else {
                (img.width as f32, img.height as f32)
            };
            if dw == 0.0 || dh == 0.0 {
                self.fit_scale = 1.0;
                return;
            }
            let scale_x = self.viewport_w as f32 / dw;
            let scale_y = self.viewport_h as f32 / dh;
            self.fit_scale = scale_x.min(scale_y);
            self.zoom = self.fit_scale;
            self.offset_x = (self.viewport_w as f32 - dw * self.zoom) * 0.5;
            self.offset_y = (self.viewport_h as f32 - dh * self.zoom) * 0.5;
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
        self.animator.reset();
    }

    pub fn zoom_step(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(0.5, 5.0);
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

    fn clamp_pan(&mut self) {
        let Some(img) = &self.current_gpu else { return };
        let (ew, eh) = if self.rotation % 2 == 1 {
            (img.height as f32, img.width as f32)
        } else {
            (img.width as f32, img.height as f32)
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

    pub fn start_rect_anim(&mut self, target: (f32, f32, f32, f32)) {
        if self.current_gpu.is_none() {
            return;
        }
        let (ew, eh) = self.effective_size();
        let from = (self.offset_x, self.offset_y, ew * self.zoom, eh * self.zoom);
        if (from.2 - target.2).abs() < 0.5 && (from.0 - target.0).abs() < 0.5 {
            return;
        }
        self.rect_anim = Some(RectAnim {
            from,
            to: target,
            start: std::time::Instant::now(),
            dur: 0.28,
            window_space: false,
        });
    }

    pub fn mark_viewport_transition(&mut self) {
        if self.current_gpu.is_some() {
            let (ew, eh) = self.effective_size();
            self.pending_viewport_anim_from = Some((
                self.viewport_origin.0 + self.offset_x,
                self.viewport_origin.1 + self.offset_y,
                ew * self.zoom,
                eh * self.zoom,
            ));
        }
    }

    pub fn set_viewport_target(&mut self, target: (f32, f32, f32, f32)) {
        self.pending_viewport_target = Some(target);
    }

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
        if self.current_gpu.is_some() {
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

    pub fn set_slide_duration(&mut self, secs: f32) {
        self.animator.set_slide_duration(secs);
    }

    pub fn zoom_1_to_1(&mut self) {
        if let Some(ref img) = self.current_gpu {
            let (dw, dh) = if self.rotation % 2 == 1 {
                (img.height as f32, img.width as f32)
            } else {
                (img.width as f32, img.height as f32)
            };
            self.zoom = 1.0;
            self.offset_x = (self.viewport_w as f32 - dw) * 0.5;
            self.offset_y = (self.viewport_h as f32 - dh) * 0.5;
            let target = (self.offset_x, self.offset_y, dw, dh);
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

        self.compute_fit();

        if let Some(mut anim) = self.rect_anim.take() {
            if !anim.window_space && (size_changed || origin_changed) {
                let (ew, eh) = self.effective_size();
                anim.to = (self.offset_x, self.offset_y, ew * self.zoom, eh * self.zoom);
            }
            self.rect_anim = Some(anim);
        }
    }

    /// Update only the physical-pixel viewport fields. Does NOT recompute
    /// fit / offset — those are owned by user actions (pan/zoom) and
    /// `set_image_gpu`. Called every frame from `render_frame` so the
    /// viewer's internal viewport stays in sync with the egui
    /// CentralPanel as panel animations change its width — without
    /// disturbing the user's current pan/zoom state.
    #[inline]
    pub fn set_viewport_physical(&mut self, w: u32, h: u32, x: f32, y: f32) {
        self.viewport_w = w;
        self.viewport_h = h;
        self.viewport_origin = (x, y);
    }

    /// Returns the current rect-anim interpolated transform, or None if
    /// no rect-anim is active. Called by `render_frame` to build the
    /// image-quad uniform each frame.
    pub fn current_rect_anim_transform(&self) -> Option<AffineTransform> {
        let anim = self.rect_anim.as_ref()?;
        let raw = (anim.start.elapsed().as_secs_f32() / anim.dur).min(1.0);
        let t = if raw < 0.5 {
            4.0 * raw * raw * raw
        } else {
            1.0 - (-2.0 * raw + 2.0).powi(3) / 2.0
        };
        let (ew, _eh) = self.effective_size();
        let disp_w = ew;
        let r = (
            anim.from.0 + (anim.to.0 - anim.from.0) * t,
            anim.from.1 + (anim.to.1 - anim.from.1) * t,
            anim.from.2 + (anim.to.2 - anim.from.2) * t,
            anim.from.3 + (anim.to.3 - anim.from.3) * t,
        );
        let s = if disp_w > 0.0 { r.2 / disp_w } else { 1.0 };
        if anim.window_space {
            Some(self.display_transform(
                r.0 - self.viewport_origin.0,
                r.1 - self.viewport_origin.1,
                s,
            ))
        } else {
            Some(self.display_transform(r.0, r.1, s))
        }
    }

    /// True if the rect-anim has completed and should be committed.
    pub fn rect_anim_done(&self) -> bool {
        self.rect_anim.as_ref().map_or(false, |a| {
            a.start.elapsed().as_secs_f32() >= a.dur
        })
    }

    /// Commit the completed rect-anim (set zoom/offset from the target).
    pub fn commit_rect_anim(&mut self) {
        let Some(anim) = &self.rect_anim else { return };
        let (ew, _eh) = self.effective_size();
        let to = anim.to;
        self.zoom = if ew > 0.0 { to.2 / ew } else { self.zoom };
        if anim.window_space {
            self.offset_x = to.0 - self.viewport_origin.0;
            self.offset_y = to.1 - self.viewport_origin.1;
        } else {
            self.offset_x = to.0;
            self.offset_y = to.1;
        }
        self.rect_anim = None;
    }

    pub fn is_transitioning(&self) -> bool {
        self.animator.is_animating() || self.rect_anim.is_some()
    }

    pub fn viewport_size(&self) -> (u32, u32) { (self.viewport_w, self.viewport_h) }
    pub fn zoom_value(&self) -> f32 { self.zoom }
    pub fn offset(&self) -> (f32, f32) { (self.offset_x, self.offset_y) }
    pub fn rotation(&self) -> u8 { self.rotation }

    pub fn set_rotation(&mut self, q: u8) {
        let prev = self.rotation;
        self.rotation = q & 3;
        if prev == self.rotation {
            return;
        }
        // Capture the current on-screen image rect (in viewer coords)
        // as the animation FROM. compute_fit will recompute the new
        // fit for the new rotation; the rect-anim will then glide
        // from this captured rect to the new one.
        let (ew, eh) = self.effective_size_for(prev);
        let from = (
            self.offset_x,
            self.offset_y,
            ew * self.zoom,
            eh * self.zoom,
        );
        self.compute_fit();
        if self.current_gpu.is_some() {
            let (ew2, eh2) = self.effective_size();
            let target = (
                self.offset_x,
                self.offset_y,
                ew2 * self.zoom,
                eh2 * self.zoom,
            );
            self.start_rect_anim_with(target, 0.32, from);
        }
    }

    /// Same as `start_rect_anim` but with a custom duration.
    pub fn start_rect_anim_with(
        &mut self,
        target: (f32, f32, f32, f32),
        dur: f32,
        from: (f32, f32, f32, f32),
    ) {
        if self.current_gpu.is_none() {
            return;
        }
        self.rect_anim = Some(RectAnim {
            from,
            to: target,
            start: std::time::Instant::now(),
            dur,
            window_space: false,
        });
    }

    /// effective_size with an explicit rotation (used to capture the
    /// pre-rotation image rect before compute_fit overwrites the field).
    fn effective_size_for(&self, rot: u8) -> (f32, f32) {
        match &self.current_gpu {
            Some(img) => {
                let (w, h) = (img.width as f32, img.height as f32);
                if rot % 2 == 1 { (h, w) } else { (w, h) }
            }
            None => (0.0, 0.0),
        }
    }

    pub fn is_fit_scale(&self) -> bool {
        (self.zoom - self.fit_scale).abs() < 0.01
    }
}
