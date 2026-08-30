//! Linear-style decorative painters: soft EdgeShine around selected
//! elements (gradient edge that fades to transparent) and a faint grain
//! overlay that breaks up large flat surfaces. All drawn with egui's
//! `egui::Painter` — no GPU textures, so cost is negligible.

use egui::{Color32, Mesh, Painter, Pos2, Rect, Shape, Vec2};

/// Draw a "Linear-style" 4-edge gradient highlight around `rect` — a
/// fade-from-accent to fully-transparent strip on each edge. `accent` is
/// the dominant color (e.g. selection indigo), `alpha` is the max alpha
/// at the inner edge. Implemented as 4 thin meshes so the gradient flows
/// inward cleanly on all four sides.
pub fn paint_edge_shine(painter: &Painter, rect: Rect, accent: Color32, alpha: u8) {
    let w = rect.width();
    let h = rect.height();
    if w < 6.0 || h < 6.0 || alpha == 0 {
        return;
    }
    let inner = Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 0);
    let outer = Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), alpha);
    // Top edge
    let mut mesh = Mesh::with_texture(egui::TextureId::default());
    push_quad(
        &mut mesh,
        [
            rect.left_top(),
            Pos2::new(rect.right(), rect.top()),
            rect.right_top() + Vec2::new(0.0, 2.0),
            rect.left_top() + Vec2::new(0.0, 2.0),
        ],
        [outer, outer, inner, inner],
    );
    painter.add(Shape::mesh(mesh));
    // Bottom edge
    let mut mesh = Mesh::with_texture(egui::TextureId::default());
    push_quad(
        &mut mesh,
        [
            Pos2::new(rect.left(), rect.bottom() - 2.0),
            rect.right_bottom(),
            rect.right_bottom(),
            Pos2::new(rect.left(), rect.bottom() - 2.0),
        ],
        [inner, inner, outer, outer],
    );
    painter.add(Shape::mesh(mesh));
    // Left edge
    let mut mesh = Mesh::with_texture(egui::TextureId::default());
    push_quad(
        &mut mesh,
        [
            rect.left_top(),
            Pos2::new(rect.left() + 2.0, rect.top()),
            Pos2::new(rect.left() + 2.0, rect.bottom()),
            rect.left_bottom(),
        ],
        [outer, inner, inner, outer],
    );
    painter.add(Shape::mesh(mesh));
    // Right edge
    let mut mesh = Mesh::with_texture(egui::TextureId::default());
    push_quad(
        &mut mesh,
        [
            Pos2::new(rect.right() - 2.0, rect.top()),
            rect.right_top(),
            rect.right_bottom(),
            Pos2::new(rect.right() - 2.0, rect.bottom()),
        ],
        [inner, outer, outer, inner],
    );
    painter.add(Shape::mesh(mesh));
}

/// Paint a faint deterministic grain overlay across `rect`. Uses a simple
/// hash to keep the noise static per-frame (Linear uses a fixed grain).
pub fn paint_grain(painter: &Painter, rect: Rect, alpha: u8) {
    if alpha == 0 {
        return;
    }
    let cells_x = ((rect.width() / 3.0) as i32).max(0);
    let cells_y = ((rect.height() / 3.0) as i32).max(0);
    if cells_x == 0 || cells_y == 0 {
        return;
    }
    let mut mesh = Mesh::with_texture(egui::TextureId::default());
    let seed_x = ((rect.left().round() as i64).wrapping_mul(2654435761)) as i32;
    let seed_y = ((rect.top().round() as i64).wrapping_mul(40503)) as i32;
    let mut rectangles = 0;
    for cy in 0..cells_y {
        for cx in 0..cells_x {
            // Cheap deterministic pseudo-noise (no RNG cost). Mix in the
            // rect's position so the pattern stays stable across frames.
            let h = ((cx ^ seed_x ^ (cy ^ seed_y)) as u32).wrapping_mul(22695477).rotate_left(7);
            let v = (h ^ 0xa3a3a3a3) >> 16;
            if (v & 0xff) > 0xc0 {
                let cell = Rect::from_min_size(
                    Pos2::new(
                        rect.left() + cx as f32 * 3.0,
                        rect.top() + cy as f32 * 3.0,
                    ),
                    Vec2::new(2.0, 2.0),
                );
                let gray = ((v >> 8) & 0xff) as u8;
                let c = Color32::from_rgba_unmultiplied(gray, gray, gray, alpha);
                push_quad_solid(&mut mesh, cell, c);
                rectangles += 1;
                if rectangles > 256 {
                    painter.add(Shape::mesh(std::mem::take(&mut mesh)));
                    rectangles = 0;
                }
            }
        }
    }
    if !mesh.vertices.is_empty() {
        painter.add(Shape::mesh(mesh));
    }
}

fn push_quad(mesh: &mut Mesh, pts: [Pos2; 4], colors: [Color32; 4]) {
    let uvs = [
        Pos2::new(0.0, 0.0),
        Pos2::new(1.0, 0.0),
        Pos2::new(1.0, 1.0),
        Pos2::new(0.0, 1.0),
    ];
    for i in 0..4 {
        mesh.vertices.push(egui::epaint::Vertex {
            pos: pts[i],
            uv: uvs[i],
            color: colors[i],
        });
    }
    mesh.indices
        .extend_from_slice(&[0u32, 1, 2, 0, 2, 3].map(|i| mesh.vertices.len() as u32 - 4 + i));
}

fn push_quad_solid(mesh: &mut Mesh, rect: Rect, color: Color32) {
    push_quad(
        mesh,
        [
            rect.left_top(),
            rect.right_top(),
            rect.right_bottom(),
            rect.left_bottom(),
        ],
        [color; 4],
    );
}
