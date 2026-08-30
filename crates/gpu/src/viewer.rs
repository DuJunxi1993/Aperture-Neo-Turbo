//! Viewer state machine — image display with animations
//!
//! Holds fit/zoom/offset/rotation state and rect/slide animations.
//! The actual rendering is done by the egui painter in `window.rs` —
//! this module computes the image→screen transform that builds the mesh.

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

/// Eased progress for a rect-anim (0→1).
fn ease(easing: Easing, raw: f32) -> f32 {
    match easing {
        // Fast start, quick settle: feels immediate for repeated zoom steps.
        Easing::EaseOut => 1.0 - (1.0 - raw).powi(3),
        // Slow at both ends: for discrete fit ↔ 1:1 transitions.
        Easing::EaseInOut => {
            if raw < 0.5 {
                4.0 * raw * raw * raw
            } else {
                1.0 - (-2.0 * raw + 2.0).powi(3) / 2.0
            }
        }
    }
}

/// Clamp an image-centre coordinate against a viewport edge rule, returning
/// the centred half-extent when the image fits or when the wall interval
/// would invert. `w` = image extent, `vw` = viewport extent, `overshoot` =
/// visible slack past the far edge.
fn clamp_centre(cur: f32, w: f32, vw: f32, overshoot: f32) -> f32 {
    if w <= vw {
        vw * 0.5
    } else {
        // Interval that the image-centre may occupy. When it inverts
        // (image just slightly larger than the viewport) there is no valid
        // pan range — re-centre rather than calling f32::clamp with min>max.
        let min = vw - w * 0.5 + overshoot;
        let max = w * 0.5 - overshoot;
        if min <= max {
            cur.clamp(min, max)
        } else {
            vw * 0.5
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlideDir {
    None,
    Next,
    Previous,
}

pub struct Direct2DViewer {
    pub current_gpu: Option<Arc<DecodedGpuImage>>,
    pub previous_gpu: Option<Arc<DecodedGpuImage>>,
    /// Images that have left the viewer but were still referenced by a
    /// recently-submitted frame. Each entry is tagged with the render epoch
    /// that was current when the image was last drawn. The app crate owns the
    /// wgpu device: each submitted frame advances an epoch and is recorded as
    /// a `SubmissionIndex`; once `device.poll(Maintain::WaitForSubmissionIndex)`
    /// confirms everything through epoch E has completed, the app calls
    /// [`Self::release_retired_through`] to drop the entries (which releases
    /// their egui textures → free delta) safely past the in-flight window.
    /// This replaces a frame-count heuristic, which is unreliable when the GPU
    /// lags behind (e.g. a decode freeze) — the frame-based window can expire
    /// while the texture is still being sampled.
    retired: std::collections::VecDeque<(Arc<DecodedGpuImage>, u64)>,
    /// Monotonic render epoch, set each frame by the app via [`Self::set_render_epoch`].
    /// Used to tag retired images so the app knows which submission drew them.
    render_epoch: u64,
    /// Highest render epoch whose submission the app has confirmed completed.
    /// Entries retired at an epoch ≤ this are safe to drop.
    safe_release_epoch: u64,
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
    /// Own view state of the OUTGOING image, captured just before
    /// `compute_fit()` overwrites `offset_x/offset_y/zoom` for the next
    /// image. The iOS slide draws the outgoing image from ITS OWN fit
    /// (offset/zoom/rotation) rather than the incoming image's, so the
    /// two images match their respective sizes during the slide.
    prev_offset_x: f32,
    prev_offset_y: f32,
    prev_zoom: f32,
    prev_rot_deg: f32,
    /// Average luminance (0..1) of the CURRENT image, captured from
    /// `DecodedGpuImage::average_luminance` on load. Stable across
    /// zoom/pan; used by the edge-drawer handle to pick a contrasting
    /// translucent color.
    pub image_luminance: f32,
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

#[derive(Clone, Copy, PartialEq)]
pub enum Easing {
    /// Fast start, slow end — feels instant for repeated user zoom steps.
    EaseOut,
    /// Slow at both ends — used for discrete fit ↔ 1:1 transitions.
    EaseInOut,
}

struct RectAnim {
    from: (f32, f32, f32, f32),
    to: (f32, f32, f32, f32),
    start: std::time::Instant,
    dur: f32,
    easing: Easing,
    window_space: bool,
}

impl Direct2DViewer {
    pub fn new(viewport_w: u32, viewport_h: u32) -> Self {
        Self {
            current_gpu: None,
            previous_gpu: None,
            retired: std::collections::VecDeque::new(),
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
            prev_offset_x: 0.0,
            prev_offset_y: 0.0,
            prev_zoom: 1.0,
            prev_rot_deg: 0.0,
            image_luminance: 0.5,
            bg: [0.059, 0.063, 0.067],
            rect_anim: None,
            pending_viewport_anim_from: None,
            pending_viewport_target: None,
            rotation: 0,
            rotation_deg: 0.0,
            rot_anim: None,
            render_epoch: 0,
            safe_release_epoch: 0,
        }
    }

    pub fn set_image_gpu(&mut self, image: Arc<DecodedGpuImage>, direction: SlideDir) {
        // Capture the OUTGOING image's own fit state BEFORE compute_fit
        // (below) overwrites offset_x/offset_y/zoom for the next image.
        // The iOS slide needs this so the outgoing image is drawn at its
        // own size/rotation, not the incoming image's.
        self.prev_offset_x = self.offset_x;
        self.prev_offset_y = self.offset_y;
        self.prev_zoom = self.zoom;
        self.prev_rot_deg = self.rotation_deg;
        self.rotation = 0;
        self.rotation_deg = 0.0;
        self.rot_anim = None;
        self.slide_dir = direction;
        if direction != SlideDir::None {
            // Directional load: keep the outgoing frame so the slide can
            // composite both, and start the slide animation. If a previous
            // image is still being slid out, retire it FIRST (never drop it
            // directly — its texture may still be sampled by an in-flight
            // submit), then adopt the new outgoing.
            if let Some(old_prev) = self.previous_gpu.take() {
                self.retired.push_back((old_prev, self.render_epoch));
            }
            let outgoing = self.current_gpu.take();
            self.current_gpu = Some(image);
            self.previous_gpu = outgoing;
            self.animator.start_slide(direction, self.viewport_w as f32);
        } else {
            // Non-directional load (folder change / held-arrow fast paging /
            // page jump): swap instantly — nothing to slide, so move the
            // outgoing frame into the retire queue (it may still be sampled
            // by a submit from the just-rendered frame).
            let outgoing = self.current_gpu.take();
            self.current_gpu = Some(image);
            if let Some(img) = outgoing {
                self.retired.push_back((img, self.render_epoch));
            }
            self.animator.reset();
        }
        // Track the image's overall brightness (computed at decode time) so
        // the edge-drawer handle can pick a contrasting color that stays
        // valid across zoom/pan.
        self.image_luminance = self.current_gpu
            .as_ref()
            .map(|g| g.average_luminance)
            .unwrap_or(0.5);
        self.compute_fit();
    }

    /// The image→screen affine for either the current image
    /// (`for_previous = false`) or the outgoing previous image
    /// (`for_previous = true`), in viewport-local coordinates. The affine
    /// maps a source-pixel position to a viewer-local screen position.
    ///
    /// The transform comes from three sources, in priority order:
    /// 1. `current_rect_anim_transform` — fullscreen / fit-zoom /
    ///    rotation target glide. Wins over the slide animation (current
    ///    image only; the previous image is only ever drawn during a
    ///    slide, and a slide and a rect-anim never run together).
    /// 2. Slide animator — the current (incoming) image rides
    ///    `offset + dir*VW*(1-t)`; the previous (outgoing) image slides
    ///    from `prev_offset - dir*VW*t`. Each is anchored at ITS OWN
    ///    captured fit (offset/zoom/rotation), so they move at their own
    ///    sizes instead of being drawn at the other image's dimensions.
    /// 3. Static fit (with rotation handled by display_transform).
    fn screen_affine(&self, for_previous: bool) -> AffineTransform {
        if !for_previous {
            if let Some(t) = self.current_rect_anim_transform() {
                return t;
            }
        }
        if self.animator.is_sliding() {
            let t = self.animator.slide_progress();
            let vw = self.viewport_w as f32;
            let dir = match self.slide_dir {
                SlideDir::Next => 1.0,
                SlideDir::Previous => -1.0,
                SlideDir::None => 0.0,
            };
            if for_previous {
                // Outgoing image anchored at ITS own previous fit, sliding
                // out toward the opposite edge: centre(t) = prev_off -
                // dir*VW*t. Use affine_for_size directly (not
                // display_transform) because the latter reads the CURRENT
                // image's dims, but this affine must use the PREVIOUS
                // image's dims/zoom/rotation.
                let (bw, bh) = match &self.previous_gpu {
                    Some(img) => (img.width as f32, img.height as f32),
                    None => (0.0, 0.0),
                };
                let cx = self.prev_offset_x - dir * vw * t;
                let cy = self.prev_offset_y;
                Self::affine_for_size(bw, bh, self.prev_rot_deg, cx, cy, self.prev_zoom)
            } else {
                // Incoming image: centre enters from the direction side and
                // settles at its own fit: centre(t) = offset + dir*VW*(1-t).
                let cx = self.offset_x + dir * vw * (1.0 - t);
                let cy = self.offset_y;
                self.display_transform(cx, cy, self.zoom)
            }
        } else {
            self.display_transform(self.offset_x, self.offset_y, self.zoom)
        }
    }

    /// Build an egui mesh for `image` at its current screen transform. The
    /// four corners of the source image are mapped through `affine` then
    /// translated by the viewport origin and converted to logical (point)
    /// coordinates, so the textured quad lands exactly where the old
    /// screen→image shader put it. Rotations (90° spins) are handled by the
    /// affine's off-diagonal terms, so a single textured mesh covers
    /// fit/zoom/pan/rotation/slide uniformly — no custom pipeline needed.
    fn image_mesh(
        &self,
        painter: &egui::Painter,
        image: &DecodedGpuImage,
        affine: &AffineTransform,
        ppp: f32,
    ) {
        let (w, h) = (image.width as f32, image.height as f32);
        let (ox, oy) = (self.viewport_origin.0 / ppp, self.viewport_origin.1 / ppp);
        let corners = [
            egui::pos2(0.0, 0.0),
            egui::pos2(w, 0.0),
            egui::pos2(w, h),
            egui::pos2(0.0, h),
        ];
        let uv_corners = [
            egui::pos2(0.0, 0.0),
            egui::pos2(1.0, 0.0),
            egui::pos2(1.0, 1.0),
            egui::pos2(0.0, 1.0),
        ];
        let mut mesh = egui::Mesh::with_texture(image.texture.id());
        for i in 0..4 {
            let c = corners[i];
            let sx = affine.m11 * c.x + affine.m12 * c.y + affine.dx;
            let sy = affine.m21 * c.x + affine.m22 * c.y + affine.dy;
            mesh.vertices.push(egui::epaint::Vertex {
                pos: egui::pos2(ox + sx / ppp, oy + sy / ppp),
                uv: uv_corners[i],
                color: egui::Color32::WHITE,
            });
        }
        mesh.indices = vec![0, 1, 2, 0, 2, 3];
        painter.add(egui::Shape::mesh(mesh));
    }

    /// Paint the currently-displayed image (and, during a slide, the
    /// outgoing previous image) into the given egui painter.
    ///
    /// Both are drawn inside the viewer rect (the caller is expected to
    /// have already sized/clipped the painter to the central panel). The
    /// previous image is drawn first, then the current one composites on
    /// top — matching the old two-pass order, but now handled by egui's
    /// own vertex pipeline. `ppp` converts the viewer's physical
    /// (px) coordinate space to egui logical points.
    pub fn paint_viewer(&self, painter: &egui::Painter, ppp: f32) {
        // Outgoing image first (behind), then the incoming over it.
        if self.animator.is_sliding() {
            if let Some(prev) = &self.previous_gpu {
                let affine = self.screen_affine(true);
                self.image_mesh(painter, prev, &affine, ppp);
            }
        }
        if let Some(cur) = &self.current_gpu {
            let affine = self.screen_affine(false);
            self.image_mesh(painter, cur, &affine, ppp);
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
    /// the centre-anchor invariant without a wgpu texture.
    ///
    /// Rotation is CLOCKWISE on screen (matching the rotate button). With a
    /// top-left-origin display (y grows downward) a clockwise visual
    /// rotation is the matrix `[[cos, -sin],[sin, cos]]` — at 90° the top
    /// edge swings right, the right edge swings down, etc.
    fn affine_for_size(
        bw: f32, bh: f32, rot_deg: f32, dx: f32, dy: f32, s: f32,
    ) -> AffineTransform {
        let theta = rot_deg.to_radians();
        let (cos, sin) = (theta.cos(), theta.sin());
        // Rotate the image centre (bw/2, bh/2) about the origin with the
        // clockwise matrix; translate so that point lands at (dx, dy).
        let cx = dx - s * (cos * bw * 0.5 - sin * bh * 0.5);
        let cy = dy - s * (sin * bw * 0.5 + cos * bh * 0.5);
        AffineTransform {
            m11: s * cos,
            m12: -s * sin,
            m21: s * sin,
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
        // Continuous, delta-proportional zoom factor: one full scroll notch
        // (~±2400 from the LineDelta→*100 pipeline) maps to roughly ×1.5 /
        // ÷1.5, and sub-notch pixel scrolls scale proportionally, so a single
        // wheel turn produces an obvious, smooth change instead of needing
        // many turns for a fixed 1.1× step.
        let clamped = (delta as f32).clamp(-6000.0, 6000.0) / 2400.0;
        let zoom_factor = 1.5_f32.powf(clamped);
        let new_zoom = (self.zoom * zoom_factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if new_zoom == self.zoom { return; }
        // cursor is in WINDOW physical coords (router.cursor_pos); the
        // image centre offset is viewer-LOCAL, so shift by the viewport
        // origin first. Otherwise zoom-to-cursor lands off (e.g. with a
        // left tree panel the viewer starts at tree_width, so a physical
        // cursor minus a viewer-local offset computes a bogus target).
        let cx = cursor_x - self.viewport_origin.0;
        let cy = cursor_y - self.viewport_origin.1;
        let cursor_rel_x = cx - self.offset_x;
        let cursor_rel_y = cy - self.offset_y;
        let target_off_x = cx - cursor_rel_x * (new_zoom / self.zoom);
        let target_off_y = cy - cursor_rel_y * (new_zoom / self.zoom);
        self.is_fit = false;
        // Animate the zoom (offset + size interpolated together) so wheel
        // zoom is smooth like a pinch instead of stepping per notch.
        if self.current_gpu.is_some() {
            let (ew, eh) = self.effective_size();
            self.start_rect_anim_eased_with(
                self.anim_rect(),
                (target_off_x, target_off_y, ew * new_zoom, eh * new_zoom),
                0.14,
                Easing::EaseOut,
                false,
            );
        } else {
            self.offset_x = target_off_x;
            self.offset_y = target_off_y;
            self.zoom = new_zoom;
        }
        self.animator.reset();
    }

    pub fn zoom_step(&mut self, factor: f32) {
        let new_zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if new_zoom == self.zoom {
            return;
        }
        let cx = self.viewport_w as f32 * 0.5;
        let cy = self.viewport_h as f32 * 0.5;
        let rel_x = cx - self.offset_x;
        let rel_y = cy - self.offset_y;
        let target_off_x = cx - rel_x * (new_zoom / self.zoom);
        let target_off_y = cy - rel_y * (new_zoom / self.zoom);
        self.is_fit = false;
        if self.current_gpu.is_some() {
            let (ew, eh) = self.effective_size();
            self.start_rect_anim_eased_with(
                self.anim_rect(),
                (target_off_x, target_off_y, ew * new_zoom, eh * new_zoom),
                0.14,
                Easing::EaseOut,
                false,
            );
        } else {
            self.offset_x = target_off_x;
            self.offset_y = target_off_y;
            self.zoom = new_zoom;
        }
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
        //
        // IMPORTANT: when the image is only slightly larger than the
        // viewport (`vw < w < vw + 2·overshoot`) the wall interval
        // INVERTS (min > max), and `f32::clamp` panics on min>max — that
        // is the "scroll-to-zoom crashes the app" bug (killed by
        // panic=abort). Detect the inversion and re-centre instead.
        let overshoot = PAN_OVERSHOOT_PX;
        self.offset_x = clamp_centre(self.offset_x, w, vw, overshoot);
        self.offset_y = clamp_centre(self.offset_y, h, vh, overshoot);
    }

    pub fn start_rect_anim(&mut self, target: (f32, f32, f32, f32)) {
        // Default used by fit ↔ 1:1 transitions: ease-in-out, 0.28s.
        self.start_rect_anim_eased(self.anim_rect(), target, 0.28, Easing::EaseInOut);
    }

    /// Start a rect-anim from an EXPLICIT `from`, with the given duration and
    /// easing. Used by fit/1:1 where `from` must be captured BEFORE the zoom /
    /// offset are mutated (otherwise `from == to` and there's no animation).
    pub fn start_rect_anim_eased(
        &mut self,
        from: (f32, f32, f32, f32),
        target: (f32, f32, f32, f32),
        dur: f32,
        easing: Easing,
    ) {
        self.start_rect_anim_eased_with(from, target, dur, easing, false);
    }

    /// Full control: explicit from/to, duration, easing, and whether the rect
    /// is in WINDOW space (vs viewer-local). Skips a no-op anim (from≈to).
    pub fn start_rect_anim_eased_with(
        &mut self,
        from: (f32, f32, f32, f32),
        target: (f32, f32, f32, f32),
        dur: f32,
        easing: Easing,
        window_space: bool,
    ) {
        if self.current_gpu.is_none() {
            return;
        }
        if (from.2 - target.2).abs() < 0.5 && (from.0 - target.0).abs() < 0.5 {
            return;
        }
        self.rect_anim = Some(RectAnim {
            from,
            to: target,
            start: std::time::Instant::now(),
            dur,
            easing,
            window_space,
        });
    }

    /// The current image rect (offset_x, offset_y, ew·zoom, eh·zoom). If a
    /// rect-anim is mid-flight this returns the INTERPOLATED rect so a new
    /// zoom (e.g. another wheel notch during the animation) chains from the
    /// on-screen position instead of snapping back to the settled value.
    fn anim_rect(&self) -> (f32, f32, f32, f32) {
        if let Some(anim) = &self.rect_anim {
            let raw = (anim.start.elapsed().as_secs_f32() / anim.dur).min(1.0);
            let t = ease(anim.easing, raw);
            return (
                anim.from.0 + (anim.to.0 - anim.from.0) * t,
                anim.from.1 + (anim.to.1 - anim.from.1) * t,
                anim.from.2 + (anim.to.2 - anim.from.2) * t,
                anim.from.3 + (anim.to.3 - anim.from.3) * t,
            );
        }
        let (ew, eh) = self.effective_size();
        (self.offset_x, self.offset_y, ew * self.zoom, eh * self.zoom)
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
        if self.current_gpu.is_some() {
            // Capture the CURRENT on-screen rect BEFORE compute_fit mutates
            // zoom/offset — otherwise the anim's `from` equals the target and
            // the fit snaps instead of animating.
            let (ew, eh) = self.effective_size();
            let from = self.anim_rect();
            self.compute_fit();
            let target = (
                self.offset_x,
                self.offset_y,
                ew * self.zoom,
                eh * self.zoom,
            );
            self.start_rect_anim_eased(from, target, 0.28, Easing::EaseInOut);
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
            let from = self.anim_rect();
            self.zoom = 1.0;
            self.is_fit = false;
            // Image centre at viewport centre (display_transform reads
            // offset_x/y as image-centre; see compute_fit).
            self.offset_x = self.viewport_w as f32 * 0.5;
            self.offset_y = self.viewport_h as f32 * 0.5;
            let target = (self.offset_x, self.offset_y, dw, dh);
            self.start_rect_anim_eased(from, target, 0.28, Easing::EaseInOut);
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
                easing: Easing::EaseInOut,
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
        // Track ANY per-frame change (not a 0.5px/1px hysteresis). During a
        // tree/thumb panel collapse the LEFT edge (origin.x) moves while the
        // width grows; a hysteresis only re-fits once the change crossed a
        // threshold, so the image "pauses then jumps". The thumb panel (right)
        // never changes origin, so the same pause was invisible there. By
        // re-fitting on every real change, tree and thumb stay in lockstep:
        // the image re-scales continuously against the moving edge — the
        // "resize the window" smoothness. compute_fit is trivial (a few
        // multiplies) and the panel animation is a few hundred ms, so this
        // costs nothing.
        let changed = self.viewport_w != w || self.viewport_h != h
            || self.viewport_origin.0 != x || self.viewport_origin.1 != y;
        self.viewport_w = w;
        self.viewport_h = h;
        self.viewport_origin = (x, y);
        if self.current_gpu.is_some() && self.is_fit && changed {
            self.compute_fit();
        }
    }

    /// Continuous-precision viewport sync, used while a side panel animates.
    ///
    /// The egui CentralPanel rect is snapped to WHOLE physical pixels
    /// (`central_rect_phys`), which during the panel-width easing produces a
    /// staircase: on deceleration frames the width/origin quantize to the same
    /// integer, so `set_viewport_physical` sees "no change" and the image
    /// "pauses then jumps" — exactly the tree-collapse stall (and it drags the
    /// bottom bar along because the whole layout shares the quantized width).
    ///
    /// This method instead accepts the CONTINUOUS (unrounded) panel widths so
    /// the per-frame change is never lost: `compute_fit` re-centres the image
    /// against the true animated viewport every frame, giving the smooth
    /// "resize the window" tracking. `origin.x/y` and the size are physical
    /// (ppp-scaled), like the image-quad scissor — the integer scissor is only
    /// a rasterisation bound, so tracking a continuous origin here never
    /// renders outside the panel.
    #[inline]
    pub fn set_viewport_continuous(&mut self, w: f32, h: f32, x: f32, y: f32) {
        let changed = (self.viewport_w as f32 - w).abs() > f32::EPSILON
            || (self.viewport_h as f32 - h).abs() > f32::EPSILON
            || (self.viewport_origin.0 - x).abs() > f32::EPSILON
            || (self.viewport_origin.1 - y).abs() > f32::EPSILON;
        // Store physical as f32 is impossible (u32 field) → round, but keep
        // origin at full f32 precision so sub-pixel edge motion still re-fits.
        self.viewport_w = w.round() as u32;
        self.viewport_h = h.round() as u32;
        self.viewport_origin = (x, y);
        if self.current_gpu.is_some() && self.is_fit && changed {
            self.compute_fit();
        }
    }

    /// Returns the current rect-anim interpolated transform, or None if
    /// no rect-anim is active. Called by `render_frame` to build the
    /// image-quad uniform each frame.
    pub fn current_rect_anim_transform(&self) -> Option<AffineTransform> {
        let anim = self.rect_anim.as_ref()?;
        let (ew, _eh) = self.effective_size();
        let r = self.anim_rect();
        let s = if ew > 0.0 { r.2 / ew } else { 1.0 };
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

    /// Advance the rect-anim to completion. Once the delay has elapsed the
    /// anim is committed (final zoom/offset applied) and dropped. Called
    /// once per frame from `render_frame` — without this, a rect-anim
    /// started by set_rotation / fit_to_screen stays resident forever, so
    /// `current_rect_anim_transform` keeps returning its (frozen) transform
    /// and the render ignores pan/zoom/offset. That is the bug behind
    /// "wheel zoom stops working after a rotation / fullscreen doesn't fit
    /// until I drag" — dragging clears rect_anim via on_pan, which
    /// accidentally unblocked the pipeline.
    pub fn tick_rect_anim(&mut self) {
        if self.rect_anim_done() {
            self.commit_rect_anim();
        }
    }

    /// Release the outgoing image once the slide has finished. `previous_gpu`
    /// is only ever drawn while `animator.is_sliding()`; once the slide's
    /// progress reaches 1.0 the old frame is no longer drawn, but it may
    /// still be sampled by the submit from the just-rendered frame — so move
    /// it into the retire queue instead of dropping it immediately. Called
    /// once per frame from `render_frame`.
    pub fn tick_slide(&mut self) {
        if !self.animator.is_sliding() {
            if let Some(prev) = self.previous_gpu.take() {
                self.retired.push_back((prev, self.render_epoch));
            }
        }
    }

    /// Set the current render epoch (the app calls this once per frame, using
    /// the epoch of the frame it is about to submit). Retired images are
    /// tagged with the epoch current when they became dead; the app only
    /// releases an entry once it has confirmed the corresponding submission
    /// completed on the GPU.
    pub fn set_render_epoch(&mut self, epoch: u64) {
        self.render_epoch = epoch;
    }

    /// Drop any retired image whose tag epoch is at or below `confirmed_epoch`
    /// — i.e. the GPU has finished every submission this image could have been
    /// sampled in. Called by the app once it has fenced past `confirmed_epoch`.
    pub fn release_retired_through(&mut self, confirmed_epoch: u64) {
        self.safe_release_epoch = confirmed_epoch.max(self.safe_release_epoch);
        let mut i = 0;
        while i < self.retired.len() {
            if self.retired[i].1 <= self.safe_release_epoch {
                let (img, _) = self.retired.remove(i).unwrap();
                drop(img);
            } else {
                i += 1;
            }
        }
    }

    pub fn is_transitioning(&self) -> bool {
        self.animator.is_animating() || self.rect_anim.is_some()
    }

    /// The viewer's viewport origin (physical px) — the coordinate anchor
    /// that `offset_x/cx` are relative to. The image-quad shader computes
    /// `screen_local = fb - viewer_rect_min`, so `viewer_rect_min` MUST be
    /// this origin for the image (incl. the outgoing slide image) to land
    /// on-screen. Passing egui's central-panel rect here instead (a
    /// different, quantized value) shifts the image out of view.
    pub fn viewport_origin(&self) -> (f32, f32) {
        (self.viewport_origin.0, self.viewport_origin.1)
    }

    /// The viewer's viewport size in physical px.
    pub fn viewport_size_f(&self) -> (f32, f32) {
        (self.viewport_w as f32, self.viewport_h as f32)
    }

    pub fn viewport_size(&self) -> (u32, u32) { (self.viewport_w, self.viewport_h) }
    pub fn zoom_value(&self) -> f32 { self.zoom }
    pub fn offset(&self) -> (f32, f32) { (self.offset_x, self.offset_y) }
    pub fn rotation(&self) -> u8 { self.rotation }

    /// Snap rotation back to 0 INSTANTLY (no spin animation) and re-fit.
    /// Used when entering fullscreen so the image straightens before the
    /// fullscreen fit — an in-flight angle animation would otherwise fight
    /// the viewport transition and land the image off-centre.
    pub fn reset_rotation(&mut self) {
        if self.rotation == 0 && self.rotation_deg == 0.0 {
            return;
        }
        self.rotation = 0;
        self.rotation_deg = 0.0;
        self.rot_anim = None;
        self.compute_fit();
    }

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
            easing: Easing::EaseInOut,
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

    /// Crash regression: clamp_centre must NOT call f32::clamp with
    /// min>max when the image is only slightly larger than the viewport
    /// (`vw < w < vw + 2*overshoot`). That used to panic (killed by
    /// panic=abort) the moment you scrolled to zoom to ~110%. It re-centres
    /// instead of panicking.
    #[test]
    fn clamp_centre_survives_image_slightly_larger_than_viewport() {
        // viewport 500, overshoot 40 → inversion zone w in (500, 580).
        for w in [510.0_f32, 540.0, 560.0, 575.0] {
            let r = clamp_centre(250.0, w, 500.0, PAN_OVERSHOOT_PX);
            // Should re-centre (no panic) and stay within the viewport.
            assert!(r.is_finite(), "clamp_centre produced NaN for w={w}");
            // Centre stays in a sane band.
            assert!((r - 250.0).abs() < 1.0, "expected centred for w={w}, got {r}");
        }
        // Larger images still clamp to the walls (min<=max).
        let r = clamp_centre(900_000.0, 1000.0, 500.0, PAN_OVERSHOOT_PX);
        assert!((r - (1000.0 * 0.5 - PAN_OVERSHOOT_PX)).abs() < 0.001, "right wall {r}");
    }

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

    /// Phase 5: at the four cardinals the affine matches a clockwise
    /// rotation (matching the rotate button). rot 90 maps
    /// (img_x, img_y) → (-s*img_y, s*img_x); rot 270 is the inverse.
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
                // rot 90 (CW): (img_x, img_y) → (-s*img_y, s*img_x)
                90 => {
                    assert!((a.m21 - s).abs() < 0.001 && (a.m12 + s).abs() < 0.001, "rot90");
                    assert!(a.m11.abs() < 0.001 && a.m22.abs() < 0.001, "rot90 diag");
                }
                // rot 180: (img_x, img_y) → (-s*img_x, -s*img_y)
                180 => {
                    assert!((a.m11 + s).abs() < 0.001 && (a.m22 + s).abs() < 0.001, "rot180");
                    assert!(a.m12.abs() < 0.001 && a.m21.abs() < 0.001, "rot180 off-diag");
                }
                // rot 270 (CW): (img_x, img_y) → (s*img_y, -s*img_x)
                270 => {
                    assert!((a.m12 - s).abs() < 0.001 && (a.m21 + s).abs() < 0.001, "rot270");
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
