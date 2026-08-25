//! Folder tree panel

use egui::{Ui, Vec2, Sense, Color32, Stroke, Response, FontId};
use std::path::PathBuf;
use parking_lot::RwLock;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct TreeNode {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub expanded: bool,
    pub children: Vec<TreeNode>,
    pub depth: usize,
}

pub struct TreePanel {
    pub roots: Vec<TreeNode>,
    pub selected_path: Option<PathBuf>,
    pub scroll_offset: f32,
}

impl TreePanel {
    pub fn new() -> Self {
        let roots = vec![
            TreeNode {
                name: "收藏夹".to_string(),
                path: PathBuf::from(""),
                is_dir: true,
                expanded: true,
                children: vec![],
                depth: 0,
            },
            TreeNode {
                name: "最近访问".to_string(),
                path: PathBuf::from(""),
                is_dir: true,
                expanded: false,
                children: vec![],
                depth: 0,
            },
            TreeNode {
                name: "此电脑".to_string(),
                path: PathBuf::from(""),
                is_dir: true,
                expanded: false,
                children: vec![],
                depth: 0,
            },
        ];
        Self { roots, selected_path: None, scroll_offset: 0.0 }
    }

    pub fn ui(&mut self, ui: &mut Ui, height: f32, _width: f32) -> Response {
        let desired = Vec2::new(220.0, height);
        let (rect, response) = ui.allocate_exact_size(desired, Sense::hover());

        // Background
        ui.painter().rect_filled(rect, 0.0, ui.style().visuals.window_fill());

        // Right border
        ui.painter().line_segment(
            [
                egui::Pos2::new(rect.right(), rect.top()),
                egui::Pos2::new(rect.right(), rect.bottom()),
            ],
            Stroke::new(1.0, Color32::from_rgb(230, 230, 230)),
        );

        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show_viewport(ui, |ui, viewport| {
                let mut y = 8.0 + viewport.min.y - self.scroll_offset;
                let mut path_stack: Vec<(TreeNode, usize)> = self.roots.iter().cloned().map(|n| (n, 0)).collect();

                for (node, _) in path_stack.iter() {
                    let node_rect = egui::Rect::from_min_size(
                        egui::Pos2::new(rect.left(), y),
                        Vec2::new(rect.width(), 28.0),
                    );

                    if y > rect.bottom() { break; }

                    if y + 28.0 > rect.top() {
                        let sense = Sense::click();
                        let resp = ui.allocate_rect(node_rect, sense);

                        let indent = node.depth as f32 * 16.0;
                        let icon_x = node_rect.left() + 8.0 + indent;
                        let text_x = icon_x + 20.0;

                        // Expand/collapse arrow
                        let arrow = if node.is_dir && !node.children.is_empty() {
                            if node.expanded { "▼" } else { "▶" }
                        } else { "" };

                        if !arrow.is_empty() {
                            ui.painter().text(
                                egui::Pos2::new(icon_x, node_rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                arrow,
                                FontId::proportional(10.0),
                                Color32::from_rgb(120, 120, 120),
                            );
                        }

                        // Folder/file icon
                        let icon = if node.is_dir { "📁" } else { "🖼" };
                        ui.painter().text(
                            egui::Pos2::new(icon_x + 14.0, node_rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            icon,
                            FontId::proportional(14.0),
                            Color32::from_rgb(80, 80, 80),
                        );

                        // Label
                        let is_selected = self.selected_path.as_ref() == Some(&node.path);
                        let label_color = if is_selected {
                            Color32::from_rgb(75, 105, 255)
                        } else {
                            Color32::from_rgb(30, 30, 30)
                        };

                        ui.painter().text(
                            egui::Pos2::new(text_x, node_rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            &node.name,
                            FontId::proportional(13.0),
                            label_color,
                        );

                        if resp.clicked() {
                            if node.is_dir && !node.children.is_empty() {
                                // toggle expansion — simplified
                            } else {
                                self.selected_path = Some(node.path.clone());
                            }
                        }
                    }

                    y += 28.0;
                }
            });

        response
    }
}