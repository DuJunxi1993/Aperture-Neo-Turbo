//! Thumbnail grid panel

use egui::{Ui, Rect, Vec2, Sense, Response, Color32, Stroke, Image, TextureHandle, ImageButton};
use aperture_core::ImageItem;
use std::sync::Arc;
use parking_lot::RwLock;

pub struct ThumbPanel {
    pub items: Arc<RwLock<Vec<ImageItem>>>,
    pub selected_index: usize,
    pub thumb_size: f32,
    pub scroll_offset: f32,
}

impl ThumbPanel {
    pub fn new(items: Arc<RwLock<Vec<ImageItem>>>) -> Self {
        Self {
            items,
            selected_index: 0,
            thumb_size: 80.0,
            scroll_offset: 0.0,
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, height: f32, _width: f32) -> Response {
        let items = self.items.read().clone();

        let desired = Vec2::new(120.0, height);
        let (outer_rect, outer_resp) = ui.allocate_exact_size(desired, Sense::hover());

        // Panel background
        ui.painter().rect_filled(outer_rect, 0.0, ui.style().visuals.window_fill());

        // Right border
        ui.painter().line_segment(
            [
                egui::Pos2::new(outer_rect.right(), outer_rect.top()),
                egui::Pos2::new(outer_rect.right(), outer_rect.bottom()),
            ],
            Stroke::new(1.0, Color32::from_rgb(230, 230, 230)),
        );

        // Scrollable area
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show_viewport(ui, |ui, viewport| {
            let padding = 8.0;
            let mut y = padding + viewport.min.y - self.scroll_offset;

            for (i, item) in items.iter().enumerate() {
                if y + self.thumb_size + padding > outer_rect.bottom() {
                    break;
                }
                if y > outer_rect.top() {
                    let thumb_rect = Rect::from_min_size(
                        egui::Pos2::new(outer_rect.left() + padding, y),
                        Vec2::new(self.thumb_size, self.thumb_size),
                    );

                    let is_selected = i == self.selected_index;
                    let bg = if is_selected {
                        Color32::from_rgb(75, 105, 255)
                    } else {
                        Color32::from_rgb(240, 240, 240)
                    };

                    let sense = Sense::click();
                    let resp = ui.allocate_rect(thumb_rect, sense);

                    ui.painter().rect_filled(thumb_rect, 4.0, bg);
                    if is_selected {
                        ui.painter().rect_stroke(thumb_rect, 4.0, Stroke::new(2.0, Color32::from_rgb(75, 105, 255)));
                    }

                    // Placeholder for thumbnail image
                    // Real implementation would load from ThumbCache and convert to egui::TextureHandle
                    ui.painter().text(
                        thumb_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "🖼",
                        egui::FontId::proportional(28.0),
                        if is_selected { Color32::WHITE } else { Color32::GRAY },
                    );

                    if resp.clicked() {
                        self.selected_index = i;
                    }
                }

                y += self.thumb_size + padding;
            }
        });

        outer_resp
    }
}