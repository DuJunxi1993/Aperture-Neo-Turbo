//! Settings panel (simplified — single window modal)

use egui::{Ui, Vec2, Response, Sense, Color32, Stroke, FontId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub max_decode_dimension: u32,
    pub thumbnail_size: u32,
    pub enable_animations: bool,
    pub enable_gpu_decode: bool,
    pub auto_fit_on_load: bool,
    pub show_floating_bar: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_decode_dimension: 7680,
            thumbnail_size: 200,
            enable_animations: true,
            enable_gpu_decode: true,
            auto_fit_on_load: true,
            show_floating_bar: true,
        }
    }
}

pub struct SettingsPanel {
    pub settings: Settings,
    pub visible: bool,
}

impl SettingsPanel {
    pub fn new() -> Self {
        Self { settings: Settings::default(), visible: false }
    }

    pub fn ui(&mut self, ui: &mut Ui, height: f32, _width: f32) -> Response {
        if !self.visible {
            return ui.allocate_response(Vec2::ZERO, Sense::hover());
        }

        let panel_w = 400.0;
        let panel_h = height * 0.7;
        let rect = egui::Rect::from_min_size(
            egui::Pos2::new(40.0, 40.0),
            Vec2::new(panel_w, panel_h),
        );

        let response = ui.allocate_rect(rect, Sense::hover());

        ui.painter().rect_filled(rect, 8.0, Color32::from_rgb(255, 255, 255));
        ui.painter().rect_stroke(rect, 8.0, Stroke::new(1.0_f32, Color32::from_rgb(230, 230, 230)));

        ui.painter().text(
            rect.left_top() + Vec2::new(16.0, 16.0),
            egui::Align2::LEFT_TOP,
            "Settings",
            FontId::proportional(16.0),
            Color32::from_rgb(20, 20, 20),
        );

        let mut y = rect.top() + 50.0;
        let item_h = 28.0;

        // Max decode dimension slider
        ui.painter().text(
            egui::Pos2::new(rect.left() + 16.0, y + item_h * 0.5),
            egui::Align2::LEFT_CENTER,
            format!("Max decode dim: {} px", self.settings.max_decode_dimension),
            FontId::proportional(13.0),
            Color32::from_rgb(40, 40, 40),
        );
        y += item_h;

        // Thumbnail size slider
        ui.painter().text(
            egui::Pos2::new(rect.left() + 16.0, y + item_h * 0.5),
            egui::Align2::LEFT_CENTER,
            format!("Thumbnail size: {} px", self.settings.thumbnail_size),
            FontId::proportional(13.0),
            Color32::from_rgb(40, 40, 40),
        );
        y += item_h;

        // GPU decode toggle
        ui.painter().text(
            egui::Pos2::new(rect.left() + 16.0, y + item_h * 0.5),
            egui::Align2::LEFT_CENTER,
            format!("GPU decode (WIC DXVA): {}", if self.settings.enable_gpu_decode { "✓" } else { "✗" }),
            FontId::proportional(13.0),
            Color32::from_rgb(40, 40, 40),
        );
        y += item_h;

        // Animations toggle
        ui.painter().text(
            egui::Pos2::new(rect.left() + 16.0, y + item_h * 0.5),
            egui::Align2::LEFT_CENTER,
            format!("Animations: {}", if self.settings.enable_animations { "✓" } else { "✗" }),
            FontId::proportional(13.0),
            Color32::from_rgb(40, 40, 40),
        );

        response
    }
}