//! Modern outline icons drawn in code (no artwork copied from the original). Their colours come from
//! the theme palette, so they adapt to light and dark mode.

use crate::model::PowerState;
use crate::theme::Pal;
use eframe::egui::{self, pos2, vec2, Color32, Painter, Pos2, Rect, Rounding, Shape, Stroke};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum IconKind {
    Home,
    Globe,
    Pool,
    Host,
    HostOff,
    ServerDown,
    Busy,
    Sr,
    Net,
    Vm(PowerState),
}

pub fn state_color(p: &Pal, s: PowerState) -> Color32 {
    match s {
        PowerState::Running => p.green,
        PowerState::Halted => p.red,
        PowerState::Suspended => p.orange,
        PowerState::Paused => p.blue,
        PowerState::Unknown => p.grey,
    }
}

/// A horizontal (or vertical) colour gradient over a rectangle.
pub fn gradient(p: &Painter, r: Rect, a: Color32, b: Color32, horizontal: bool) {
    let mut m = egui::Mesh::default();
    let (tl, tr, br, bl) = if horizontal { (a, b, b, a) } else { (a, a, b, b) };
    m.colored_vertex(r.left_top(), tl);
    m.colored_vertex(r.right_top(), tr);
    m.colored_vertex(r.right_bottom(), br);
    m.colored_vertex(r.left_bottom(), bl);
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    p.add(Shape::mesh(m));
}

pub fn arc(p: &Painter, c: Pos2, r: f32, start_deg: f32, end_deg: f32, stroke: Stroke) {
    let steps = 28;
    let pts: Vec<Pos2> = (0..=steps)
        .map(|i| {
            let a = (start_deg + (end_deg - start_deg) * i as f32 / steps as f32).to_radians();
            pos2(c.x + r * a.cos(), c.y + r * a.sin())
        })
        .collect();
    p.add(Shape::line(pts, stroke));
}

fn poly(p: &Painter, o: Pos2, pts: &[(f32, f32)], fill: Color32) {
    let v: Vec<Pos2> = pts.iter().map(|(x, y)| pos2(o.x + x, o.y + y)).collect();
    p.add(Shape::convex_polygon(v, fill, Stroke::NONE));
}

fn line(p: &Painter, o: Pos2, pts: &[(f32, f32)], s: Stroke) {
    let v: Vec<Pos2> = pts.iter().map(|(x, y)| pos2(o.x + x, o.y + y)).collect();
    p.add(Shape::line(v, s));
}

fn rrect(p: &Painter, o: Pos2, x: f32, y: f32, w: f32, h: f32, r: f32, s: Stroke) {
    p.rect_stroke(Rect::from_min_size(o + vec2(x, y), vec2(w, h)), Rounding::same(r), s);
}

/// The little status dot in an icon's lower-right corner, ringed with the background colour.
fn dot(p: &Painter, o: Pos2, c: Color32, ring: Color32) {
    p.circle(o + vec2(12.5, 12.5), 3.3, c, Stroke::new(1.4, ring));
}

/// A 16×16 outline icon for the tree, the header and lists. `bg` is what it sits on.
pub fn paint_icon(p: &Painter, rect: Rect, kind: IconKind, pal: &Pal, bg: Color32) {
    let o = rect.min;
    let s = Stroke::new(1.3, pal.icon);
    match kind {
        IconKind::Home => {
            line(p, o, &[(1.5, 8.0), (8.0, 2.0), (14.5, 8.0)], s);
            line(p, o, &[(3.5, 6.5), (3.5, 14.0), (12.5, 14.0), (12.5, 6.5)], s);
            rrect(p, o, 6.5, 9.5, 3.0, 4.5, 0.5, s);
        }
        IconKind::Globe => {
            p.circle_stroke(o + vec2(8.0, 8.0), 6.3, s);
            p.add(Shape::ellipse_stroke(o + vec2(8.0, 8.0), vec2(2.8, 6.3), s));
            line(p, o, &[(1.8, 8.0), (14.2, 8.0)], s);
        }
        IconKind::Pool => {
            // stacked layers
            for (i, y) in [10.5f32, 6.5, 2.5].iter().enumerate() {
                let r = Rect::from_min_size(o + vec2(1.5 + i as f32, *y), vec2(12.5 - i as f32 * 2.0, 3.5));
                p.rect(r, Rounding::same(1.2), bg, s);
            }
        }
        IconKind::Host | IconKind::HostOff | IconKind::ServerDown => {
            rrect(p, o, 3.0, 1.5, 9.0, 13.0, 1.8, s);
            line(p, o, &[(5.5, 5.0), (9.5, 5.0)], s);
            line(p, o, &[(5.5, 8.0), (9.5, 8.0)], s);
            match kind {
                IconKind::Host => dot(p, o, pal.green, bg),
                IconKind::HostOff => dot(p, o, pal.red, bg),
                _ => {
                    p.circle(o + vec2(12.5, 12.5), 3.6, pal.red, Stroke::new(1.4, bg));
                    let w = Stroke::new(1.2, Color32::WHITE);
                    p.line_segment([o + vec2(11.2, 11.2), o + vec2(13.8, 13.8)], w);
                    p.line_segment([o + vec2(13.8, 11.2), o + vec2(11.2, 13.8)], w);
                }
            }
        }
        IconKind::Busy => arc(p, o + vec2(8.0, 8.0), 5.8, -90.0, 180.0, Stroke::new(1.8, pal.accent)),
        IconKind::Sr => {
            p.add(Shape::ellipse_stroke(o + vec2(8.0, 4.0), vec2(5.8, 2.3), s));
            line(p, o, &[(2.2, 4.0), (2.2, 12.0)], s);
            line(p, o, &[(13.8, 4.0), (13.8, 12.0)], s);
            arc(p, o + vec2(8.0, 12.0), 5.8, 0.0, 180.0, s);
            let pts: Vec<Pos2> = (0..=16).map(|i| {
                let a = (i as f32 / 16.0 * 180.0).to_radians();
                o + vec2(8.0 + 5.8 * a.cos(), 8.0 + 2.3 * a.sin())
            }).collect();
            p.add(Shape::line(pts, Stroke::new(1.0, pal.icon.gamma_multiply(0.6))));
        }
        IconKind::Net => {
            line(p, o, &[(8.0, 5.0), (8.0, 8.0)], s);
            line(p, o, &[(3.5, 11.0), (3.5, 8.0), (12.5, 8.0), (12.5, 11.0)], s);
            for c in [vec2(8.0, 3.0), vec2(3.5, 12.8), vec2(12.5, 12.8)] {
                p.circle(o + c, 2.1, bg, s);
            }
        }
        IconKind::Vm(state) => {
            rrect(p, o, 1.5, 2.0, 13.0, 9.0, 1.6, s);
            line(p, o, &[(5.5, 14.0), (10.5, 14.0)], s);
            line(p, o, &[(8.0, 11.0), (8.0, 14.0)], s);
            dot(p, o, state_color(pal, state), bg);
        }
    }
}

/// A 22×22 toolbar glyph on a soft tinted square, in its action's colour (adapts to the theme).
pub fn paint_tool(p: &Painter, rect: Rect, op: &str, enabled: bool, pal: &Pal) {
    let c = rect.center();
    let base = match op {
        "clean_shutdown" | "hard_shutdown" => pal.red,
        "clean_reboot" => pal.green,
        "hard_reboot" => pal.orange,
        "suspend" | "resume" => pal.blue,
        "start" | "add" => pal.green,
        _ => pal.icon,
    };
    let col = if enabled { base } else { pal.ink_faint };
    let neutral = matches!(op, "sidebar" | "theme-dark" | "theme-light");
    if !neutral {
        let tint = if enabled { col.gamma_multiply(if pal.dark { 0.22 } else { 0.14 }) } else { Color32::TRANSPARENT };
        p.rect_filled(rect.shrink(0.5), Rounding::same(6.0), tint);
    }
    let s = Stroke::new(2.0, col);
    let o = rect.min;
    match op {
        "clean_shutdown" | "hard_shutdown" => {
            arc(p, c + vec2(0.0, 0.8), 5.6, -55.0, 235.0, s);
            p.line_segment([c + vec2(0.0, -6.5), c + vec2(0.0, -0.5)], s);
            if op == "hard_shutdown" {
                poly(p, o, &[(16.5, 12.0), (20.5, 12.0), (18.5, 15.5), (21.0, 15.5), (15.0, 22.0), (16.2, 17.5), (14.0, 17.5)], if enabled { pal.orange } else { pal.ink_faint });
            }
        }
        "clean_reboot" | "hard_reboot" => {
            arc(p, c, 5.6, -30.0, 270.0, s);
            poly(p, c, &[(4.2, -8.2), (7.6, -3.0), (1.8, -2.6)], col);
        }
        "suspend" => {
            p.rect_filled(Rect::from_min_size(c + vec2(-4.5, -5.0), vec2(3.2, 10.0)), Rounding::same(1.0), col);
            p.rect_filled(Rect::from_min_size(c + vec2(1.3, -5.0), vec2(3.2, 10.0)), Rounding::same(1.0), col);
        }
        "resume" => {
            p.rect_filled(Rect::from_min_size(c + vec2(-5.5, -5.0), vec2(2.4, 10.0)), Rounding::same(1.0), col);
            poly(p, c, &[(-1.5, -5.5), (5.5, 0.0), (-1.5, 5.5)], col);
        }
        "start" => poly(p, c, &[(-3.5, -6.0), (6.0, 0.0), (-3.5, 6.0)], col),
        "add" => {
            rrect(p, o, 4.0, 4.0, 10.0, 14.0, 2.0, Stroke::new(1.6, if enabled { pal.icon } else { pal.ink_faint }));
            p.line_segment([o + vec2(6.5, 8.5), o + vec2(11.5, 8.5)], Stroke::new(1.4, if enabled { pal.icon } else { pal.ink_faint }));
            p.circle(o + vec2(16.0, 15.5), 5.2, col, Stroke::NONE);
            let w = Stroke::new(1.8, Color32::WHITE);
            p.line_segment([o + vec2(13.4, 15.5), o + vec2(18.6, 15.5)], w);
            p.line_segment([o + vec2(16.0, 12.9), o + vec2(16.0, 18.1)], w);
        }
        "sidebar" => {
            let s = Stroke::new(1.6, col);
            p.rect_stroke(Rect::from_center_size(c, vec2(16.0, 13.0)), Rounding::same(2.5), s);
            p.line_segment([c + vec2(-2.5, -6.5), c + vec2(-2.5, 6.5)], s);
            for y in [-3.0, 0.0, 3.0] {
                p.line_segment([c + vec2(-6.0, y), c + vec2(-4.5, y)], Stroke::new(1.2, col));
            }
        }
        "theme-dark" => {
            // a crescent moon: switch to dark
            p.circle_filled(c, 6.5, col);
            p.circle_filled(c + vec2(3.2, -2.6), 5.6, pal.toolbar);
        }
        "theme-light" => {
            // a sun: switch to light
            p.circle_stroke(c, 3.6, Stroke::new(1.8, col));
            for i in 0..8 {
                let a = (i as f32 * 45.0).to_radians();
                let d = vec2(a.cos(), a.sin());
                p.line_segment([c + d * 6.0, c + d * 8.3], Stroke::new(1.6, col));
            }
        }
        _ => {}
    }
}

/// The search box's magnifier.
pub fn paint_search(p: &Painter, rect: Rect, col: Color32) {
    let s = Stroke::new(1.7, col);
    let c = rect.center();
    p.circle_stroke(c - vec2(1.5, 1.5), 4.6, s);
    p.line_segment([c + vec2(2.0, 2.0), c + vec2(6.0, 6.0)], s);
}

/// The Home page drawing: a cloud with two screens below it.
pub fn paint_cloud(p: &Painter, o: Pos2, pal: &Pal) {
    let s = Stroke::new(1.6, pal.art);
    let fill = pal.art_fill;
    for (c, r) in [(vec2(35.0, 70.0), 32.0), (vec2(80.0, 50.0), 45.0), (vec2(135.0, 68.0), 33.0)] {
        p.circle(o + c, r, fill, s);
    }
    p.rect_filled(Rect::from_min_max(o + vec2(10.0, 62.0), o + vec2(160.0, 100.0)), 0.0, fill);
    p.line_segment([o + vec2(25.0, 100.5), o + vec2(150.0, 100.5)], s);
    let dash = Stroke::new(1.2, pal.art);
    for (a, b) in [((85.0, 101.0), (85.0, 125.0)), ((45.0, 125.0), (125.0, 125.0)), ((45.0, 125.0), (45.0, 140.0)), ((125.0, 125.0), (125.0, 140.0))] {
        p.add(Shape::dashed_line(&[o + vec2(a.0, a.1), o + vec2(b.0, b.1)], dash, 2.0, 2.0));
    }
    paint_monitor(p, o + vec2(20.0, 140.0), 50.0, pal);
    paint_monitor(p, o + vec2(100.0, 140.0), 50.0, pal);
    p.circle_stroke(o + vec2(128.0, 36.0), 10.0, Stroke::new(1.6, pal.red));
    p.circle_stroke(o + vec2(128.0, 36.0), 4.0, Stroke::new(1.6, pal.red));
}

pub fn paint_monitor(p: &Painter, o: Pos2, w: f32, pal: &Pal) {
    let s = Stroke::new(1.5, pal.art);
    let h = w * 2.0 / 3.0;
    p.rect_stroke(Rect::from_min_size(o, vec2(w, h)), Rounding::same(3.0), s);
    let cell = (w - 12.0) / 3.0;
    for i in 0..3 {
        for j in 0..2 {
            let r = Rect::from_min_size(o + vec2(6.0 + i as f32 * cell, 7.0 + j as f32 * (h / 2.6)), vec2(cell - 3.0, h / 3.6));
            p.rect_stroke(r, Rounding::same(1.0), Stroke::new(1.2, pal.red));
        }
    }
    p.line_segment([o + vec2(w / 2.0, h), o + vec2(w / 2.0, h + 8.0)], s);
    p.line_segment([o + vec2(w / 2.0 - 10.0, h + 8.0), o + vec2(w / 2.0 + 10.0, h + 8.0)], s);
}

/// The three big pictures above the Home page's links.
pub fn paint_home_icon(p: &Painter, o: Pos2, kind: &str, pal: &Pal) {
    let s = Stroke::new(1.6, pal.art);
    let r = Stroke::new(2.0, pal.red);
    match kind {
        "add" => {
            p.circle_stroke(o + vec2(22.0, 28.0), 18.0, s);
            p.circle_stroke(o + vec2(50.0, 25.0), 25.0, s);
            p.circle_stroke(o + vec2(72.0, 30.0), 15.0, s);
            p.rect_filled(Rect::from_min_size(o + vec2(10.0, 30.0), vec2(70.0, 24.0)), 0.0, pal.surface);
            p.circle_stroke(o + vec2(41.0, 19.0), 11.0, r);
            p.line_segment([o + vec2(41.0, 13.0), o + vec2(41.0, 25.0)], r);
            p.line_segment([o + vec2(35.0, 19.0), o + vec2(47.0, 19.0)], r);
            for i in 0..3 {
                p.rect_stroke(Rect::from_min_size(o + vec2(18.0, 40.0 + i as f32 * 14.0), vec2(50.0, 12.0)), Rounding::same(2.0), s);
                p.circle_filled(o + vec2(26.0, 46.0 + i as f32 * 14.0), 2.0, pal.art);
            }
        }
        "demo" => paint_monitor(p, o + vec2(0.0, 4.0), 80.0, pal),
        _ => {
            p.circle_stroke(o + vec2(40.0, 30.0), 25.0, s);
            p.text(o + vec2(40.0, 30.0), egui::Align2::CENTER_CENTER, "i", egui::FontId::proportional(28.0), pal.red);
            p.circle_stroke(o + vec2(7.0, 69.0), 7.0, s);
            p.circle_stroke(o + vec2(73.0, 69.0), 7.0, s);
        }
    }
}
