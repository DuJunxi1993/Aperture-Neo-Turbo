//! Slide animation state machine.
//!
//! Tracks a single directional gallery slide (Next / Previous) and its
//! eased progress in [0, 1]. The viewer uses the progress to place the
//! incoming and outgoing images; the easing is ease-in-out-cubic so the
//! slide has zero velocity at both ends (soft pick-up, smooth landing).

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
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum AnimType {
    None,
    Slide,
}

pub struct Animator {
    anim_type: AnimType,
    start_time: std::time::Instant,
    duration: f32,
    slide_dir: i32, // +1 = next, -1 = prev
    /// Duration used by the next start_slide (set from the viewer's
    /// requested slide duration; fast-forward does not shorten it any more).
    slide_duration: f32,
}

impl Animator {
    pub fn new() -> Self {
        Self {
            anim_type: AnimType::None,
            start_time: std::time::Instant::now(),
            duration: 0.0,
            slide_dir: 0,
            slide_duration: 0.35,
        }
    }

    pub fn start_slide(&mut self, dir: crate::viewer::SlideDir, viewport_w: f32) {
        // `viewport_w` is retained for symmetry/callers but not needed for
        // progress; the viewer computes the pixel shift from its own offset.
        let _ = viewport_w;
        self.anim_type = AnimType::Slide;
        self.start_time = std::time::Instant::now();
        self.duration = self.slide_duration;
        self.slide_dir = match dir {
            crate::viewer::SlideDir::Next => 1,
            crate::viewer::SlideDir::Previous => -1,
            crate::viewer::SlideDir::None => 0,
        };
    }

    /// Set the duration used by subsequent slide animations.
    pub fn set_slide_duration(&mut self, secs: f32) {
        self.slide_duration = secs.clamp(0.08, 0.6);
    }

    pub fn reset(&mut self) {
        self.anim_type = AnimType::None;
    }

    pub fn is_animating(&self) -> bool {
        self.anim_type == AnimType::Slide && self.progress() < 1.0
    }

    pub fn is_sliding(&self) -> bool {
        self.anim_type == AnimType::Slide && self.progress() < 1.0
    }

    /// Eased slide progress in [0, 1]. Returns 1.0 when not sliding so a
    /// caller can treat the animation as finished.
    pub fn slide_progress(&self) -> f32 {
        self.eased_progress()
    }

    fn progress(&self) -> f32 {
        if self.anim_type != AnimType::Slide {
            return 1.0;
        }
        let elapsed = self.start_time.elapsed().as_secs_f32();
        (elapsed / self.duration).min(1.0)
    }

    fn eased_progress(&self) -> f32 {
        let t = self.progress();
        // ease-in-out-cubic: 4t³ for t<0.5, 1 − (−2t+2)³/2 otherwise.
        // Velocity is zero at both ends (soft pick-up, smooth decelerating
        // landing — the push feel of a gallery swipe).
        if t < 0.5 {
            4.0 * t * t * t
        } else {
            1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
        }
    }
}
