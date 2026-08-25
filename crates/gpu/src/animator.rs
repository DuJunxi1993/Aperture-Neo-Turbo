//! Animation state machine — slide, zoom, fit transitions
//!
//! Returns AffineTransform that the caller (D2D code) converts to a
//! D2D matrix. Platform-agnostic — no `windows::*` types.

#[derive(Debug, Clone, Copy)]
pub struct AffineTransform {
    pub m11: f32, pub m12: f32,
    pub m21: f32, pub m22: f32,
    pub dx: f32,  pub dy: f32,
}

impl AffineTransform {
    pub fn identity() -> Self {
        Self { m11: 1.0, m12: 0.0, m21: 0.0, m22: 1.0, dx: 0.0, dy: 0.0 }
    }

    pub fn scale(s: f32) -> Self {
        Self { m11: s, m12: 0.0, m21: 0.0, m22: s, dx: 0.0, dy: 0.0 }
    }

    pub fn translate(x: f32, y: f32) -> Self {
        Self { m11: 1.0, m12: 0.0, m21: 0.0, m22: 1.0, dx: x, dy: y }
    }

    pub fn mul(self, other: Self) -> Self {
        Self {
            m11: self.m11 * other.m11 + self.m12 * other.m21,
            m12: self.m11 * other.m12 + self.m12 * other.m22,
            m21: self.m21 * other.m11 + self.m22 * other.m21,
            m22: self.m21 * other.m12 + self.m22 * other.m22,
            dx: self.m11 * other.dx + self.m12 * other.dy + self.dx,
            dy: self.m21 * other.dx + self.m22 * other.dy + self.dy,
        }
    }

    /// Convert to a D2D matrix array [m11, m12, m21, m22, dx, dy]
    pub fn to_array(&self) -> [f32; 6] {
        [self.m11, self.m12, self.m21, self.m22, self.dx, self.dy]
    }

    pub fn lerp(self, target: Self, t: f32) -> Self {
        Self {
            m11: self.m11 + (target.m11 - self.m11) * t,
            m12: self.m12 + (target.m12 - self.m12) * t,
            m21: self.m21 + (target.m21 - self.m21) * t,
            m22: self.m22 + (target.m22 - self.m22) * t,
            dx: self.dx + (target.dx - self.dx) * t,
            dy: self.dy + (target.dy - self.dy) * t,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum AnimType {
    None,
    Fit,
    Zoom,
    Pan,
    Slide,
}

pub struct Animator {
    anim_type: AnimType,
    start_time: std::time::Instant,
    duration: f32,
    from_transform: AffineTransform,
    to_transform: AffineTransform,
    slide_dir: i32, // +1 = next, -1 = prev
    viewport_w: f32,
    /// Duration used by the next start_slide (fast-forward shortens it).
    slide_duration: f32,
}

impl Animator {
    pub fn new() -> Self {
        Self {
            anim_type: AnimType::None,
            start_time: std::time::Instant::now(),
            duration: 0.0,
            from_transform: AffineTransform::identity(),
            to_transform: AffineTransform::identity(),
            slide_dir: 0,
            viewport_w: 0.0,
            slide_duration: 0.38,
        }
    }

    pub fn start_fit(&mut self, fit_scale: f32) {
        self.anim_type = AnimType::Fit;
        self.start_time = std::time::Instant::now();
        self.duration = 0.18;
        self.from_transform = AffineTransform::scale(1.0);
        self.to_transform = AffineTransform::scale(fit_scale);
    }

    pub fn start_zoom(&mut self, zoom: f32, offset_x: f32, offset_y: f32) {
        self.anim_type = AnimType::Zoom;
        self.start_time = std::time::Instant::now();
        self.duration = 0.18;
        self.from_transform = AffineTransform::identity();
        self.to_transform = AffineTransform::translate(offset_x, offset_y)
            .mul(AffineTransform::scale(zoom));
    }

    pub fn start_pan(&mut self, offset_x: f32, offset_y: f32) {
        self.anim_type = AnimType::Pan;
        self.start_time = std::time::Instant::now();
        self.duration = 0.18;
        self.from_transform = AffineTransform::identity();
        self.to_transform = AffineTransform::translate(offset_x, offset_y);
    }

    pub fn start_slide(&mut self, dir: crate::viewer::SlideDir, viewport_w: f32) {
        self.anim_type = AnimType::Slide;
        self.start_time = std::time::Instant::now();
        self.duration = self.slide_duration;
        self.slide_dir = match dir {
            crate::viewer::SlideDir::Next => 1,
            crate::viewer::SlideDir::Previous => -1,
            crate::viewer::SlideDir::None => 0,
        };
        self.viewport_w = viewport_w;
    }

    /// Set the duration used by subsequent slide animations.
    pub fn set_slide_duration(&mut self, secs: f32) {
        self.slide_duration = secs.clamp(0.08, 0.6);
    }

    pub fn reset(&mut self) {
        self.anim_type = AnimType::None;
    }

    pub fn is_animating(&self) -> bool {
        self.anim_type != AnimType::None && self.progress() < 1.0
    }

    pub fn is_sliding(&self) -> bool {
        self.anim_type == AnimType::Slide && self.progress() < 1.0
    }

    fn progress(&self) -> f32 {
        let elapsed = self.start_time.elapsed().as_secs_f32();
        (elapsed / self.duration).min(1.0)
    }

    fn eased_progress(&self) -> f32 {
        let t = self.progress();
        match self.anim_type {
            // iOS-style slide: fast departure, gentle decelerating landing.
            AnimType::Slide => 1.0 - (1.0 - t).powi(5),
            _ => t * t * (3.0 - 2.0 * t),
        }
    }

    pub fn current_transform(
        &self,
        zoom: f32, offset_x: f32, offset_y: f32,
        fit_scale: f32, slide_dir: crate::viewer::SlideDir, viewport_w: f32,
    ) -> AffineTransform {
        if !self.is_animating() {
            return AffineTransform::translate(offset_x, offset_y)
                .mul(AffineTransform::scale(zoom));
        }

        let t = self.eased_progress();

        match self.anim_type {
            AnimType::Fit => {
                let scale = 1.0 + (fit_scale - 1.0) * t;
                AffineTransform::scale(scale)
            }
            AnimType::Zoom | AnimType::Pan => {
                self.from_transform.lerp(self.to_transform, t)
            }
            AnimType::Slide => {
                let dir = match slide_dir {
                    crate::viewer::SlideDir::Next => 1.0,
                    crate::viewer::SlideDir::Previous => -1.0,
                    crate::viewer::SlideDir::None => 0.0,
                };
                let shift = dir * viewport_w * (1.0 - t);
                AffineTransform::translate(offset_x + shift, offset_y)
                    .mul(AffineTransform::scale(zoom))
            }
            AnimType::None => AffineTransform::identity(),
        }
    }

    pub fn prev_transform(
        &self,
        prev_fit: f32, prev_ox: f32, prev_oy: f32,
        slide_dir: crate::viewer::SlideDir, viewport_w: f32,
    ) -> AffineTransform {
        if !self.is_sliding() { return AffineTransform::identity(); }

        let t = self.eased_progress();
        let dir = match slide_dir {
            crate::viewer::SlideDir::Next => 1.0,
            crate::viewer::SlideDir::Previous => -1.0,
            crate::viewer::SlideDir::None => 0.0,
        };
        // Exit from its own fitted position, in parallel with the incoming
        // image (same easing → same speed → carousel-strip feel).
        let shift = -dir * viewport_w * t;
        AffineTransform::translate(prev_ox + shift, prev_oy)
            .mul(AffineTransform::scale(prev_fit))
    }
}