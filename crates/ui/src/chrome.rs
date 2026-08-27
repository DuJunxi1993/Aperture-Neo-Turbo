//! Chrome UI — title bar, floating toolbar

use egui::{Ui, Color32, Stroke, Rect, Vec2, Pos2, Response, Sense, Align2, FontId};

#[derive(Debug, Clone)]
pub struct Theme {
    pub bg: Color32,
    pub surface: Color32,
    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub accent: Color32,
    pub border: Color32,
}

impl Default for Theme {
    fn default() -> Self {
        // Linear-style light theme (matching ApertureNeo main project)
        Self {
            bg: Color32::from_rgb(248, 248, 248),
            surface: Color32::from_rgb(255, 255, 255),
            text_primary: Color32::from_rgb(20, 20, 20),
            text_secondary: Color32::from_rgb(120, 120, 120),
            accent: Color32::from_rgb(75, 105, 255),
            border: Color32::from_rgb(230, 230, 230),
        }
    }
}

pub struct TitleBar {
    pub title: String,
    pub show_settings: bool,
}

impl TitleBar {
    pub fn new(title: impl Into<String>) -> Self {
        Self { title: title.into(), show_settings: false }
    }

    pub fn ui(&mut self, ui: &mut Ui, theme: &Theme, _width: f32) -> Response {
        let desired = Vec2::new(ui.available_width(), 44.0);
        let (rect, response) = ui.allocate_exact_size(desired, Sense::click_and_drag());

        // Background
        ui.painter().rect_filled(rect, 0.0, theme.surface);

        // Bottom border
        ui.painter().line_segment(
            [Pos2::new(rect.left(), rect.bottom()), Pos2::new(rect.right(), rect.bottom())],
            Stroke::new(1.0_f32, theme.border),
        );

        // Title
        ui.painter().text(
            Pos2::new(rect.left() + 16.0, rect.center().y),
            Align2::LEFT_CENTER,
            &self.title,
            FontId::proportional(13.0),
            theme.text_primary,
        );

        // Right-side buttons (placeholder for toolbar icons)
        let icon_y = rect.center().y;
        let mut btn_x = rect.right() - 16.0;

        // Settings button
        btn_x -= 24.0;
        ui.painter().text(
            Pos2::new(btn_x, icon_y),
            Align2::CENTER_CENTER,
            "⚙",
            FontId::proportional(16.0),
            theme.text_secondary,
        );

        // Window controls placeholder (min/max/close handled by OS chrome)
        btn_x -= 60.0;
        ui.painter().text(
            Pos2::new(btn_x, icon_y),
            Align2::CENTER_CENTER,
            "─ □ ✕",
            FontId::proportional(12.0),
            theme.text_secondary,
        );

        // Drag region (respond to drag for window movement)
        if response.dragged() {
            // Window dragging is handled by WM_NCHITTEST in app crate
        }

        response
    }
}

pub struct FloatingBar {
    pub visible: bool,
    pub zoom: f32,
    pub show_info: bool,
    pub show_settings: bool,
}

impl FloatingBar {
    pub fn new() -> Self {
        Self { visible: true, zoom: 1.0, show_info: false, show_settings: false }
    }

    pub fn ui(&mut self, ui: &mut Ui, theme: &Theme, viewport_size: (u32, u32)) -> Response {
        if !self.visible {
            return ui.allocate_response(Vec2::ZERO, Sense::hover());
        }

        let bar_w = 320.0;
        let bar_h = 40.0;
        let center_x = viewport_size.0 as f32 * 0.5;
        let y = viewport_size.1 as f32 - bar_h - 24.0;

        let rect = Rect::from_min_size(
            Pos2::new(center_x - bar_w * 0.5, y),
            Vec2::new(bar_w, bar_h),
        );

        let response = ui.allocate_rect(rect, Sense::click());

        // Background (rounded)
        ui.painter().rect_filled(rect, 8.0, theme.surface);

        // Border
        ui.painter().rect_stroke(rect, 8.0, Stroke::new(1.0_f32, theme.border));

        // Zoom display
        ui.painter().text(
            Pos2::new(rect.left() + 16.0, rect.center().y),
            Align2::LEFT_CENTER,
            format!("{}%", (self.zoom * 100.0) as i32),
            FontId::proportional(13.0),
            theme.text_primary,
        );

        // Info button
        let info_btn = Rect::from_center_size(
            Pos2::new(rect.center().x, rect.center().y),
            Vec2::new(24.0, 24.0),
        );
        ui.painter().text(
            info_btn.center(),
            Align2::CENTER_CENTER,
            "i",
            FontId::proportional(14.0),
            theme.text_secondary,
        );

        // Settings button
        let settings_btn = Rect::from_center_size(
            Pos2::new(rect.right() - 24.0, rect.center().y),
            Vec2::new(24.0, 24.0),
        );
        ui.painter().text(
            settings_btn.center(),
            Align2::CENTER_CENTER,
            "⚙",
            FontId::proportional(14.0),
            theme.text_secondary,
        );

        response
    }
}