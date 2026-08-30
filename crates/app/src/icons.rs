//! Hand-drawn vector glyphs for the titlebar / tree header buttons.
//!
//! egui path strokes render with miter joins (no round caps/joins), so any
//! rounded corner must be baked into the point list. Each icon is defined in a
//! normalized `[-0.5, 0.5]` box centred at the origin and expanded to the
//! caller's rect; the stroke colour comes from the theme palette, so light and
//! dark variants follow automatically.

use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, Vec2};

/// Expand a normalized point (in `[-0.5, 0.5]`) into the icon rect.
fn map(p: Vec2, rect: Rect) -> Pos2 {
    Pos2::new(
        rect.center().x + p.x * rect.width(),
        rect.center().y + p.y * rect.height(),
    )
}

/// Sample a circle arc (radians) centred at `c`, radius `r`, into `out`.
fn arc_into(out: &mut Vec<Pos2>, c: Pos2, r: f32, a0: f32, a1: f32, seg: usize) {
    let n = seg.max(1) as f32;
    for i in 0..=seg {
        let t = i as f32 / n;
        let a = a0 + (a1 - a0) * t;
        out.push(c + Vec2::new(a.cos(), a.sin()) * r);
    }
}

/// Sample a quadratic Bézier from `a` through control `c` to `b` in `Vec2`
/// (normalized) space.
fn quad_into(out: &mut Vec<Vec2>, a: Vec2, c: Vec2, b: Vec2, seg: usize) {
    for i in 0..=seg {
        let t = i as f32 / seg.max(1) as f32;
        let u = 1.0 - t;
        out.push(a * (u * u) + c * (2.0 * u * t) + b * (t * t));
    }
}

/// Build a closed normalized polyline with `radius`-rounded corners, using
/// quadratic Béziers at each vertex (robust for convex+concave shapes).
fn rounded_closed(vertices: &[Vec2], radius: f32, segs: usize) -> Vec<Vec2> {
    let n = vertices.len();
    let mut out = Vec::new();
    for i in 0..n {
        let p = vertices[i];
        let prev = vertices[(i + n - 1) % n];
        let next = vertices[(i + 1) % n];
        let dir_in = (p - prev).normalized();
        let dir_out = (next - p).normalized();
        // Clamp corner radius to the nearest half-edge so short edges don't
        // produce overlapping corners.
        let r = radius.min((p - prev).length() * 0.5).min((next - p).length() * 0.5);
        let entry = p - dir_in * r;
        let exit = p + dir_out * r;
        if i == 0 {
            out.push(entry);
        }
        quad_into(&mut out, entry, p, exit, segs);
        if i == n - 1 {
            out.push(exit);
        }
    }
    out
}

/// Draw the settings gear: a toothed cog outline with a circular centre hole.
pub fn gear(painter: &Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(1.7_f32, color);
    let cx = rect.center();
    let outer_r = rect.width() * 0.44;
    let teeth = 8;
    let tooth_h = outer_r * 0.30;
    let mut pts = Vec::new();
    for i in 0..teeth {
        let a0 = (i as f32 / teeth as f32) * std::f32::consts::TAU;
        let a1 = ((i as f32 + 1.0) / teeth as f32) * std::f32::consts::TAU;
        let a_mid = (a0 + a1) * 0.5;
        // valley → tooth tip → valley (rounded transitions baked by arcs)
        arc_into(&mut pts, cx, outer_r - tooth_h, a0, a_mid - 0.04, 5);
        arc_into(&mut pts, cx, outer_r, a_mid - 0.04, a_mid + 0.04, 3);
        arc_into(&mut pts, cx, outer_r - tooth_h, a_mid + 0.04, a1, 5);
    }
    painter.add(Shape::closed_line(pts, stroke));
    painter.circle_stroke(cx, outer_r * 0.34, stroke);
}

/// Draw the help speech-bubble: a rounded-square outline with a tail at the
/// bottom-left and a filled `?` glyph.
pub fn help(painter: &Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(1.7_f32, color);
    let w = rect.width();
    let h = rect.height();
    let left = -0.5;
    let right = 0.5;
    let top = -0.5;
    let bottom = 0.5;
    // Outline: rounded square with a tail pointing down-left.
    let verts = [
        Vec2::new(left, top),
        Vec2::new(right, top),
        Vec2::new(right, bottom),
        Vec2::new(-0.04, bottom),
        Vec2::new(-0.42, bottom + 0.34),
        Vec2::new(left, bottom),
    ];
    let pts: Vec<Pos2> = rounded_closed(&verts, 0.30, 7).iter().map(|p| map(*p, rect)).collect();
    painter.add(Shape::closed_line(pts, stroke));

    // Filled "?" — a horseshoe arc, a descending stem, and a dot.
    let qw = w * 0.30;
    let stroke_q = Stroke::new(qw * 0.48, color);
    let arc_r = qw * 0.62;
    let arc_c = Pos2::new(rect.center().x, rect.center().y - h * 0.12);
    let mut qpts = Vec::new();
    arc_into(&mut qpts, arc_c, arc_r, std::f32::consts::PI * 0.90, std::f32::consts::PI * 2.10, 13);
    painter.add(Shape::line(qpts, stroke_q));
    let stem_top = Pos2::new(arc_c.x, arc_c.y + arc_r * 0.40);
    let stem_bot = Pos2::new(rect.center().x, rect.center().y + h * 0.14);
    painter.line_segment([stem_top, stem_bot], stroke_q);
    painter.circle_filled(Pos2::new(stem_bot.x, stem_bot.y + qw * 0.36), qw * 0.26, color);
}

/// Draw the home glyph: a stroked house outline (pitched rounded roof, square
/// body) with a short rounded horizontal door bar inside.
pub fn home(painter: &Painter, rect: Rect, color: Color32) {
    let stroke = Stroke::new(1.7_f32, color);
    let w = rect.width();
    let h = rect.height();
    let left = -0.5;
    let right = 0.5;
    let top = -0.5;
    let bottom = 0.5;
    let verts = [
        Vec2::new(0.0, top),
        Vec2::new(right, top + 0.40),
        Vec2::new(right, bottom),
        Vec2::new(left, bottom),
        Vec2::new(left, top + 0.40),
    ];
    let pts: Vec<Pos2> = rounded_closed(&verts, 0.13, 7).iter().map(|p| map(*p, rect)).collect();
    painter.add(Shape::closed_line(pts, stroke));
    // Door bar: a short rounded horizontal stroke inside, near the bottom.
    let y = rect.center().y + h * 0.26;
    let half = w * 0.16;
    painter.line_segment(
        [Pos2::new(rect.center().x - half, y), Pos2::new(rect.center().x + half, y)],
        Stroke::new(w * 0.22, color),
    );
}
