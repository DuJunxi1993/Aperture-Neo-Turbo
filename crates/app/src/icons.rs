//! Hand-drawn vector glyphs for the titlebar / tree header buttons.
//!
//! egui path strokes render with miter joins (no round caps/joins), so any
//! rounded corner must be baked into the point list. Each icon is defined in a
//! normalized `[-0.5, 0.5]` box centred at the origin and expanded to the
//! caller's rect; the stroke colour comes from the theme palette, so light and
//! dark variants follow automatically.

use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, Vec2};

/// The side of the square box all icons live in. Buttons are non-square
/// (e.g. 42×30), so scale by the SMALLER edge and centre the box — otherwise a
/// width-based radius overflows the button height and clips into a stray line.
/// The 0.70 factor leaves ~30% internal padding so the glyph sits compact and
/// centred (Linear-style) instead of nearly filling the button.
fn icon_side(rect: Rect) -> f32 {
    rect.width().min(rect.height()) * 0.70
}

/// Expand a normalized point (in `[-0.5, 0.5]`) into a centred square box of
/// side `u` inside `rect`.
fn map(p: Vec2, rect: Rect, u: f32) -> Pos2 {
    Pos2::new(
        rect.center().x + p.x * u,
        rect.center().y + p.y * u,
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
    let u = icon_side(rect);
    let stroke = Stroke::new(u / 15.0, color);
    let cx = rect.center();
    let outer_r = u * 0.42;
    let teeth = 8;
    let tooth_h = outer_r * 0.26;
    let mut pts = Vec::new();
    for i in 0..teeth {
        let a0 = (i as f32 / teeth as f32) * std::f32::consts::TAU;
        let a1 = ((i as f32 + 1.0) / teeth as f32) * std::f32::consts::TAU;
        let a_mid = (a0 + a1) * 0.5;
        // valley → tooth tip → valley (rounded transitions baked by arcs)
        arc_into(&mut pts, cx, outer_r - tooth_h, a0, a_mid - 0.05, 5);
        arc_into(&mut pts, cx, outer_r, a_mid - 0.05, a_mid + 0.05, 3);
        arc_into(&mut pts, cx, outer_r - tooth_h, a_mid + 0.05, a1, 5);
    }
    painter.add(Shape::closed_line(pts, stroke));
    painter.circle_stroke(cx, outer_r * 0.36, stroke);
}

/// Draw the help glyph: just a filled `?` (no speech-bubble outline).
pub fn help(painter: &Painter, rect: Rect, color: Color32) {
    let u = icon_side(rect);
    let c = rect.center();
    let qw = u * 0.34;
    let stroke_q = Stroke::new(qw * 0.46, color);
    let arc_r = qw * 0.62;
    // Horseshoe arc (the top curve of the "?"), open at the bottom.
    let arc_c = Pos2::new(c.x, c.y - u * 0.14);
    let mut qpts = Vec::new();
    arc_into(&mut qpts, arc_c, arc_r, std::f32::consts::PI * 0.92, std::f32::consts::PI * 2.08, 13);
    painter.add(Shape::line(qpts, stroke_q));
    // Descending stem into the centre, then a dot.
    let stem_top = Pos2::new(arc_c.x, arc_c.y + arc_r * 0.42);
    let stem_bot = Pos2::new(c.x, c.y + u * 0.14);
    painter.line_segment([stem_top, stem_bot], stroke_q);
    painter.circle_filled(Pos2::new(stem_bot.x, stem_bot.y + qw * 0.38), qw * 0.27, color);
}

/// Draw the home glyph: a stroked house outline (pitched rounded roof, square
/// body) with a short rounded horizontal door bar inside.
pub fn home(painter: &Painter, rect: Rect, color: Color32) {
    let u = icon_side(rect);
    let stroke = Stroke::new(u / 15.0, color);
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
    let pts: Vec<Pos2> = rounded_closed(&verts, 0.13, 7).iter().map(|p| map(*p, rect, u)).collect();
    painter.add(Shape::closed_line(pts, stroke));
    // Door bar: a short rounded horizontal stroke inside, near the bottom.
    let y = rect.center().y + u * 0.26;
    let half = u * 0.16;
    painter.line_segment(
        [Pos2::new(rect.center().x - half, y), Pos2::new(rect.center().x + half, y)],
        Stroke::new(u / 14.0, color),
    );
}
