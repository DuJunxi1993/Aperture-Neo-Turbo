//! Viewer state machine — image display with animations
//!
//! Holds fit/zoom/offset/rotation state and rect/slide animations.
//! The actual rendering is done by the wgpu image-quad pipeline in
//! `window.rs` — this module only computes transforms and uniform data.

use std::sync::Arc;
use crate::texture::DecodedGpuImage;
use crate::animator::{Animator, AffineTransform};

/// Minimum zoom factor (as a fraction of original image size). The
/// previous hard-coded `0.5` caused a counterintuitive bump-up when the
/// window was small enough that fit-to-screen produced a scale below
/// 0.5: pressing zoom-out would *increase* the displayed size before
/// letting the user shrink the image. Lowered to 0.05 (5 %) so the
/// user can always shrink to thumbnail overview without that bump.
const MIN_ZOOM: f32 = 0.05;
/// Maximum zoom factor (as a fraction of original image size).
const MAX_ZOOM: f32 = 5.0;

/// Pixel slack past the viewport edge when a larger-than-viewport
/// image is dragged past its natural wall. Both walls of `clamp_pan`
/// use the same overshoot so left/right (and top/bottom) drag
/// distances are symmetric — the previous code had `vw - 40` for the
/// right wall and `vw - w + 40` for the left wall, which let the user
/// drag much further to the right than to the left.
const PAN_OVERSHOOT_PX: f32 = 40.0;

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
    /// Whether the viewer is currently in "fit image to viewport" mode.
    /// A real flag (not `zoom == fit_scale`) so that `set_viewport_physical`
    /// can reliably re-centre the image when the viewport changes (tree/
    /// thumb animation, fullscreen transition) without depending on a
    /// fragile float comparison that breaks after a rotation or a zoom
    /// that accidentally matches fit_scale.
    pub is_fit: bool,
    pub bg: [f32; 3],
    rect_anim: Option<RectAnim>,
    pending_viewport_anim_from: Option<(f32, f32, f32, f32)>,
    pending_viewport_target: Option<(f32, f32, f32, f32)>,
    rotation: u8,
    /// Continuous rotation angle in degrees, animated during a rotation so
    /// the image spins smoothly to the target quadrant instead of snapping
    /// to 0/90/180/270. `display_transform` uses this to build the affine.
    rotation_deg: f32,
    /// In-flight rotation animation (from_deg → to_deg). None when idle.
    rot_anim: Option<RotAnim>,
}

/// A smooth rotation animation (degrees).
struct RotAnim {
    from_deg: f32,
    to_deg: f32,
    start: std::time::Instant,
    dur: f32,
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
            is_fit: true,
            bg: [0.059, 0.063, 0.067],
            rect_anim: None,
            pending_viewport_anim_from: None,
            pending_viewport_target: None,
            rotation: 0,
            rotation_deg: 0.0,
            rot_anim: None,
        }
    }

    pub fn set_image_gpu(&mut self, image: Arc<DecodedGpuImage>, direction: SlideDir) {
        self.rotation = 0;
        self.rotation_deg = 0.0;
        self.rot_anim = None;
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
            let vw = self.viewport_w as f32;
            let slide = self.animator.current_transform(
                self.zoom, self.offset_x, self.offset_y,
                self.fit_scale, self.slide_dir, vw,
            );
            if for_previous {
                // Previous image: reverse the slide direction so the
                // outgoing image moves opposite the incoming one.
                let dir = match self.slide_dir {
                    crate::viewer::SlideDir::Next => -1.0,
                    crate::viewer::SlideDir::Previous => 1.0,
                    crate::viewer::SlideDir::None => 0.0,
                };
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

    /// Advance any in-flight rotation animation. Called once per frame from
    /// `render_frame` so `rotation_deg` reflects the interpolated angle when
    /// `display_transform` reads it.
    pub fn tick_rotation(&mut self) {
        if let Some(anim) = self.rot_anim.as_ref() {
            let raw = (anim.start.elapsed().as_secs_f32() / anim.dur).min(1.0);
            let t = raw * raw * (3.0 - 2.0 * raw);
            let deg = anim.from_deg + (anim.to_deg - anim.from_deg) * t;
            if raw >= 1.0 {
                self.rotation_deg = anim.to_deg.rem_euclid(360.0);
                self.rot_anim = None;
            } else {
                self.rotation_deg = deg;
            }
        }
    }

    /// Current continuous rotation angle in degrees (advances any in-flight
    /// rot_anim). Used by `display_transform` to build the affine so the
    /// image spins smoothly instead of snapping between quadrants.
    pub fn rotation_angle_deg(&self) -> f32 {
        self.rotation_deg
    }

    /// Pure affine-builder: image(w×h) rotated by `rot_deg` about its
    /// centre, scaled by `s`, with the image centre mapped to `(dx, dy)`.
    /// Separate from [`Self::display_transform`] so unit tests can verify
    /// the centre-anchor invariant without a wgpu DecodedGpuImage.
    ///
    /// Rotation is CLOCKWISE on screen (the existing quadrant convention:
    /// rot 1 = 90° CW maps (img_x, img_y) → (s*img_y, -s*img_x)). With a
    /// top-left-origin display (y grows downward), a clockwise visual
    /// rotation is the matrix `[[cos, sin],[-sin, cos]]`.
    fn affine_for_size(
        bw: f32, bh: f32, rot_deg: f32, dx: f32, dy: f32, s: f32,
    ) -> AffineTransform {
        let theta = rot_deg.to_radians();
        let (cos, sin) = (theta.cos(), theta.sin());
        // Rotate the image centre (bw/2, bh/2) about the origin with the
        // clockwise matrix; translate so that point lands at (dx, dy).
        let cx = dx - s * (cos * bw * 0.5 + sin * bh * 0.5);
        let cy = dy - s * (-sin * bw * 0.5 + cos * bh * 0.5);
        AffineTransform {
            m11: s * cos,
            m12: s * sin,
            m21: -s * sin,
            m22: s * cos,
            dx: cx,
            dy: cy,
        }
    }

    /// Build the image→screen affine with the rotation centred on the
    /// image itself. `dx, dy` is the viewer-space position of the
    /// IMAGE CENTRE. Uses the continuous `rotation_deg` so the image spins
    /// smoothly about its geometric centre `(bw/2, bh/2)`, which is mapped
    /// to `(dx, dy)` regardless of angle — this guarantees the image never
    /// drifts out of the viewport during a rotation.
    pub fn display_transform(&self, dx: f32, dy: f32, s: f32) -> AffineTransform {
        let (bw, bh) = match &self.current_gpu {
            Some(img) => (img.width as f32, img.height as f32),
            None => return AffineTransform::identity(),
        };
        Self::affine_for_size(bw, bh, self.rotation_deg, dx, dy, s)
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
            // Centre the image: its centre goes to the viewport's centre
            // (in viewer-local coords). display_transform reads
            // `offset_x/y` as the image-centre position, so we set it
            // explicitly to viewport_centre here. The previous version
            // stored the image-LEFT edge slack, which display_transform
            // then incorrectly treated as image centre — producing an
            // image shifted by `−sw/2` from its intended position.
            self.offset_x = self.viewport_w as f32 * 0.5;
            self.offset_y = self.viewport_h as f32 * 0.5;
            self.is_fit = true;
        }
    }

    pub fn on_wheel(&mut self, delta: i32, cursor_x: f32, cursor_y: f32) {
        let zoom_factor = if delta > 0 { 1.1 } else { 1.0 / 1.1 };
        let new_zoom = (self.zoom * zoom_factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let cursor_rel_x = cursor_x - self.offset_x;
        let cursor_rel_y = cursor_y - self.offset_y;
        self.offset_x = cursor_x - cursor_rel_x * (new_zoom / self.zoom);
        self.offset_y = cursor_y - cursor_rel_y * (new_zoom / self.zoom);
        self.zoom = new_zoom;
        self.is_fit = false;
        self.clamp_pan();
        self.animator.reset();
    }

    pub fn zoom_step(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let cx = self.viewport_w as f32 * 0.5;
        let cy = self.viewport_h as f32 * 0.5;
        let rel_x = cx - self.offset_x;
        let rel_y = cy - self.offset_y;
        self.offset_x = cx - rel_x * factor;
        self.offset_y = cy - rel_y * factor;
        self.is_fit = false;
        self.clamp_pan();
        self.animator.reset();
    }

    pub fn on_pan(&mut self, dx: f32, dy: f32) {
        self.offset_x += dx;
        self.offset_y += dy;
        self.is_fit = false;
        self.clamp_pan();
        self.rect_anim = None;
    }

    /// Clamp pan offsets to keep the image inside the viewport with a
    /// symmetric `PAN_OVERSHOOT_PX` of slack past each viewport edge.
    ///
    /// Symmetry convention (when `w > vw`, image bigger than viewport):
    ///   - LEFT  wall: image's RIGHT edge is `PAN_OVERSHOOT_PX` past
    ///     viewport's RIGHT edge. Offset = `vw - w + PAN_OVERSHOOT_PX`.
    ///   - RIGHT wall: image's LEFT  edge is `PAN_OVERSHOOT_PX` past
    ///     viewport's LEFT  edge. Offset = `-PAN_OVERSHOOT_PX`.
    ///
    /// The previous version used `vw - 40` for the right wall, which
    /// let the user push the image almost fully off the right side
    /// (only a 40-px sliver of the image's left edge remained visible),
    /// while the left wall kept the image mostly inside with just a
    /// 40-px overflow past the right edge. Equal-and-opposite fix:
    /// both walls now use the same "40 px past the far edge" rule,
    /// so drag distance from the centred position is the same in
    /// every direction.
    fn clamp_pan(&mut self) {
        let Some(img) = &self.current_gpu else { return };
        let (ew, eh) = if self.rotation % 2 == 1 {
            (img.height as f32, img.width as f32)
        } else {
            (img.width as f32, img.height as f32)
        };
        let w = ew * self.zoom;
        let h = eh * self.zoom;
        self.clamp_pan_to(w, h, self.viewport_w as f32, self.viewport_h as f32);
    }

    /// Clamp pan offsets to the given image/viewport sizes without
    /// needing a real `current_gpu`. Pure logic, used both by the
    /// public `clamp_pan` and by unit tests.
    fn clamp_pan_to(&mut self, w: f32, h: f32, vw: f32, vh: f32) {
        // offset_x/y is the IMAGE-CENTRE position (in viewer-local coords);
        // see compute_fit / zoom_1_to_1. The image spans
        //   [offset_x - w/2, offset_x + w/2]  in viewer-local x
        //   [offset_y - h/2, offset_y + h/2]  in viewer-local y
        //
        // For w ≤ vw the image fits, so re-centre it. For w > vw we clamp
        // the centre position so the image's edges stay within
        // `±PAN_OVERSHOOT_PX` of the viewport. Using image-CENTRE coords
        // gives a symmetric wall on both sides.
        //
        //   LEFT  wall: image is mostly off-viewport-RIGHT (the user
        //   dragged left to see the right edge). Limit so its RIGHT edge
        //   sits at `vw + PAN_OVERSHOOT_PX`:
        //       offset_x + w/2 = vw + overshoot
        //       offset_x = vw - w/2 + overshoot
        //
        //   RIGHT wall: image is mostly off-viewport-LEFT (dragged right
        //   to see the left edge). Limit so its LEFT edge sits at
        //   `-PAN_OVERSHOOT_PX`:
        //       offset_x - w/2 = -overshoot
        //       offset_x = w/2 - overshoot
        let overshoot = PAN_OVERSHOOT_PX;
        self.offset_x = if w <= vw {
            vw * 0.5
        } else {
            self.offset_x.clamp(vw - w * 0.5 + overshoot, w * 0.5 - overshoot)
        };
        self.offset_y = if h <= vh {
            vh * 0.5
        } else {
            self.offset_y.clamp(vh - h * 0.5 + overshoot, h * 0.5 - overshoot)
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
            self.is_fit = false;
            // Image centre at viewport centre (display_transform reads
            // offset_x/y as image-centre; see compute_fit).
            self.offset_x = self.viewport_w as f32 * 0.5;
            self.offset_y = self.viewport_h as f32 * 0.5;
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
    /// CentralPanel as panel animations change its width.
    ///
    /// HOWEVER: if the image is currently in FIT state (zoom == fit_scale)
    /// and the viewport size/origin CHANGED, we must re-fit. Otherwise the
    /// image's centre (offset_x/y) was computed against the OLD viewport
    /// and stays put in the CENTRAL PANEL while the panel itself slides
    /// (e.g. the tree/thumb width animation on startup, or the fullscreen
    /// transition) — leaving the image visibly off-centre. Re-fitting here
    /// keeps the image centred as the panel animates.
    #[inline]
    pub fn set_viewport_physical(&mut self, w: u32, h: u32, x: f32, y: f32) {
        let size_changed = self.viewport_w != w || self.viewport_h != h;
        let origin_changed =
            (self.viewport_origin.0 - x).abs() > 0.5 || (self.viewport_origin.1 - y).abs() > 0.5;
        self.viewport_w = w;
        self.viewport_h = h;
        self.viewport_origin = (x, y);
        if self.current_gpu.is_some() && self.is_fit && (size_changed || origin_changed) {
            self.compute_fit();
        }
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
            // Start a continuous angle animation so the image SPINS about
            // its geometric centre instead of snapping quadrants. We take
            // the current angle and rotate by the SHORTEST delta (±90° or
            // ±180°) that reaches the target quadrant, so the spin is a
            // single smooth motion, never a long around-the-back turn.
            let target_deg = (q & 3) as f32 * 90.0;
            let cur = self.rotation_deg;
            let mut delta = (target_deg - cur).rem_euclid(360.0);
            if delta > 180.0 { delta -= 360.0; }
            self.rot_anim = Some(RotAnim {
                from_deg: cur,
                to_deg: cur + delta,
                start: std::time::Instant::now(),
                dur: 0.32,
            });
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug G regression: clamp_pan_to must give the same drag distance
    /// from the centred position in both directions when the image is
    /// bigger than the viewport. `offset_x/y` is the IMAGE-CENTRE
    /// position (in viewer-local coords); both walls use the
    /// `PAN_OVERSHOOT_PX` past-the-far-edge rule so the distance
    /// from the centred position is identical in every direction.
    #[test]
    fn pan_walls_are_symmetric_when_image_bigger_than_viewport() {
        // 1000x1000 image, 500x500 viewport, 1x zoom → w > vw.
        let mut v = Direct2DViewer::new(500, 500);
        // Centred start: image CENTRE at viewport centre = (vw/2, vh/2).
        v.offset_x = 250.0; // vw/2
        v.offset_y = 250.0;
        let centre = v.offset_x;

        // Drag WAY past the left wall.
        v.offset_x -= 10_000.0;
        v.clamp_pan_to(1000.0, 1000.0, 500.0, 500.0);
        let left_wall = v.offset_x;

        // Reset and drag WAY past the right wall.
        let mut v = Direct2DViewer::new(500, 500);
        v.offset_x = centre;
        v.offset_y = 250.0;
        v.offset_x += 10_000.0;
        v.clamp_pan_to(1000.0, 1000.0, 500.0, 500.0);
        let right_wall = v.offset_x;

        let left_distance = centre - left_wall;
        let right_distance = right_wall - centre;
        assert!(
            (left_distance - right_distance).abs() < 0.5,
            "left_distance={left_distance}, right_distance={right_distance} — clamp_pan_to is asymmetric"
        );

        // Both walls use `vw - w/2 + overshoot` (LEFT wall — dragged
        // left so the image's right edge is at viewport right + overshoot)
        // and `w/2 - overshoot` (RIGHT wall — dragged right so the
        // image's left edge is at viewport left - overshoot). In both
        // cases the image centre is `vw/2` away from the centre when
        // the image fits; larger `vw` → same distance.
        let overshoot = PAN_OVERSHOOT_PX;
        let expected_left  = 500.0_f32 - 1000.0 * 0.5 + overshoot; // = 40
        let expected_right = 1000.0 * 0.5 - overshoot;              // = 460
        assert!(
            (left_wall - expected_left).abs() < 0.5,
            "left_wall={left_wall}, expected={expected_left}"
        );
        assert!(
            (right_wall - expected_right).abs() < 0.5,
            "right_wall={right_wall}, expected={expected_right}"
        );
    }

    /// When the image is SMALLER than the viewport, the offset
    /// should snap to image-centre-at-viewport-centre (offset = vw/2).
    #[test]
    fn pan_centre_when_image_smaller_than_viewport() {
        let mut v = Direct2DViewer::new(800, 800);
        v.offset_x = 999.0;
        v.offset_y = 999.0;
        v.clamp_pan_to(200.0, 200.0, 800.0, 800.0);
        // image (200) < viewport (800) → centre the image: 800/2 = 400.
        assert!((v.offset_x - 400.0).abs() < 0.001);
        assert!((v.offset_y - 400.0).abs() < 0.001);
    }

    /// Y-axis symmetric clamp (top/bottom drag should match).
    #[test]
    fn pan_walls_are_symmetric_on_y_axis() {
        let mut v = Direct2DViewer::new(500, 500);
        v.offset_x = 250.0;
        v.offset_y = 250.0; // centred
        v.offset_y -= 10_000.0;
        v.clamp_pan_to(1000.0, 1000.0, 500.0, 500.0);
        let bottom_wall = v.offset_y;

        let mut v = Direct2DViewer::new(500, 500);
        v.offset_x = 250.0;
        v.offset_y = 250.0;
        v.offset_y += 10_000.0;
        v.clamp_pan_to(1000.0, 1000.0, 500.0, 500.0);
        let top_wall = v.offset_y;

        let centre = 250.0;
        let bottom_distance = centre - bottom_wall;
        let top_distance = top_wall - centre;
        assert!(
            (bottom_distance - top_distance).abs() < 0.5,
            "bottom_distance={bottom_distance}, top_distance={top_distance}"
        );
    }

    /// Phase 5: rotation must be centred on the image's GEOMETRIC CENTRE.
    /// For any rotation angle (including the in-between interpolation
    /// angles 0/30/60/90/…/360), applying the affine to the image centre
    /// pixel (bw/2, bh/2) must yield exactly (dx, dy). This guarantees the
    /// image never drifts out of the viewport while spinning.
    #[test]
    fn rotation_is_centred_on_image_geometric_centre() {
        let (bw, bh) = (1920.0_f32, 1440.0_f32);
        let (dx, dy) = (600.0_f32, 400.0_f32);
        let s = 0.6_f32;
        // Sample every 15° including the four quadrant endpoints.
        for deg in (0..=360).step_by(15) {
            let a = Direct2DViewer::affine_for_size(bw, bh, deg as f32, dx, dy, s);
            // Forward: image → screen. Image centre → (dx, dy).
            let sx = a.m11 * (bw * 0.5) + a.m12 * (bh * 0.5) + a.dx;
            let sy = a.m21 * (bw * 0.5) + a.m22 * (bh * 0.5) + a.dy;
            assert!(
                (sx - dx).abs() < 0.01 && (sy - dy).abs() < 0.01,
                "rot={deg}: centre mapped to ({sx},{sy}), expected ({dx},{dy}) — image drifts off its anchor"
            );
        }
    }

    /// Phase 5: at the four cardinals the affine must agree with the old
    /// hard-coded quadrant matrices (0/90/180/270) so existing perception
    /// of rotation direction is preserved.
    #[test]
    fn rotation_cardinals_match_quadrant_matrices() {
        let (bw, bh) = (200.0_f32, 100.0_f32);
        let (dx, dy) = (50.0_f32, 60.0_f32);
        let s = 1.25_f32;
        for deg in [0.0_f32, 90.0, 180.0, 270.0] {
            let a = Direct2DViewer::affine_for_size(bw, bh, deg, dx, dy, s);
            match deg as i32 {
                // rot 0: identity scale
                0 => {
                    assert!((a.m11 - s).abs() < 0.001 && (a.m22 - s).abs() < 0.001, "rot0");
                    assert!(a.m12.abs() < 0.001 && a.m21.abs() < 0.001, "rot0 off-diag");
                }
                // rot 90: (img_x, img_y) → ( s*img_y, -s*img_x )
                90 => {
                    assert!((a.m12 - s).abs() < 0.001 && (a.m21 + s).abs() < 0.001, "rot90");
                    assert!(a.m11.abs() < 0.001 && a.m22.abs() < 0.001, "rot90 diag");
                }
                // rot 180: (img_x, img_y) → (-s*img_x, -s*img_y )
                180 => {
                    assert!((a.m11 + s).abs() < 0.001 && (a.m22 + s).abs() < 0.001, "rot180");
                    assert!(a.m12.abs() < 0.001 && a.m21.abs() < 0.001, "rot180 off-diag");
                }
                // rot 270: (img_x, img_y) → (-s*img_y,  s*img_x )
                270 => {
                    assert!((a.m12 + s).abs() < 0.001 && (a.m21 - s).abs() < 0.001, "rot270");
                    assert!(a.m11.abs() < 0.001 && a.m22.abs() < 0.001, "rot270 diag");
                }
                _ => unreachable!(),
            }
            // And the centre must still land on (dx, dy) at each cardinal.
            let sx = a.m11 * (bw * 0.5) + a.m12 * (bh * 0.5) + a.dx;
            let sy = a.m21 * (bw * 0.5) + a.m22 * (bh * 0.5) + a.dy;
            assert!((sx - dx).abs() < 0.01 && (sy - dy).abs() < 0.01, "rot={deg} centre");
        }
    }
}
