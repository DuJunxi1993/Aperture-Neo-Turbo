//! ViewerPanel — embeds the Direct2D viewer inside egui via a child window region

use egui::{Ui, Vec2, Sense, Response};
use aperture_gpu::Direct2DViewer;
use std::sync::Arc;
use parking_lot::Mutex;

pub struct ViewerPanel {
    pub viewer: Arc<Mutex<Direct2DViewer>>,
    pub hovered: bool,
    pub last_wheel: i32,
    pub last_cursor: (f32, f32),
    pub last_pan: Option<(f32, f32)>,
}

impl ViewerPanel {
    pub fn new(viewer: Arc<Mutex<Direct2DViewer>>) -> Self {
        Self {
            viewer,
            hovered: false,
            last_wheel: 0,
            last_cursor: (0.0, 0.0),
            last_pan: None,
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, width: f32, height: f32) -> Response {
        let (rect, response) = ui.allocate_exact_size(
            Vec2::new(width, height),
            Sense::click_and_drag(),
        );

        // Background
        let bg_color = ui.style().visuals.window_fill();
        ui.painter().rect_filled(rect, 0.0, bg_color);

        // Update viewer viewport size
        if response.hovered() {
            self.hovered = true;
        } else {
            self.hovered = false;
        }

        // Handle wheel events
        if response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y * 100.0) as i32;
            if scroll != 0 {
                self.last_wheel = scroll;
                if let Some(pos) = response.hover_pos() {
                    self.last_cursor = (pos.x - rect.left(), pos.y - rect.top());
                    let mut viewer = self.viewer.lock();
                    viewer.on_wheel(scroll, self.last_cursor.0, self.last_cursor.1);
                }
            }
        }

        // Handle drag (panning)
        if response.drag_started() {
            self.last_pan = Some(self.last_cursor);
        }
        if response.dragged() {
            if let Some(start) = self.last_pan {
                if let Some(pos) = response.hover_pos() {
                    let cur = (pos.x - rect.left(), pos.y - rect.top());
                    let dx = cur.0 - start.0;
                    let dy = cur.1 - start.1;
                    let mut v = self.viewer.lock();
                    v.on_pan(dx, dy);
                    self.last_pan = Some(cur);
                }
            }
        }
        if response.drag_stopped() {
            self.last_pan = None;
        }

        // Placeholder: actual Direct2D render happens in child window
        // The egui layer draws a placeholder rect, and the child HWND overlays it
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Direct2D Viewer (child HWND)",
            egui::FontId::proportional(14.0),
            ui.style().visuals.text_color(),
        );

        response
    }
}