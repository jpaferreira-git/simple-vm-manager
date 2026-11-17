//! Modern outline icons drawn in code (no artwork copied from the original). Their colours come from
//! the theme palette, so they adapt to light and dark mode.

use crate::model::PowerState;
use crate::theme::Pal;
use eframe::egui::{pos2, vec2, Color32, Painter, Pos2, Rect, Rounding, Shape, Stroke};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum IconKind {
    Home,
    Globe,
    Pool,
    Host,
    HostOff,
    ServerDown,
    /// Something under way: a ring turning, in the colour of what's being done.
    Busy(Tone),
    Sr,
    /// A library of installation discs.
    IsoSr,
    /// A storage (an ISO library or not) with how it's doing: see `SrHealth`.
    Storage { iso: bool, health: SrHealth },
    Net,
    Folder,
    Vm(PowerState),
    /// A VM's snapshot: a camera.
    Snapshot,
    /// A template made by the user: a screen with copies of it behind.
    Template,
}

/// How a storage is doing, as the dot on its icon.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SrHealth {
    /// Mounted where it should be: a green dot.
    Up,
    /// Its server is off or not answering: a red dot.
    Off,
    /// Some servers that should have it mounted don't: an orange "!".
    Partly,
    /// Mounted on no server: a red "!".
    Nowhere,
}

/// A turning ring's colour: green while a VM starts, red while it shuts down…
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tone {
    Accent,
    Green,
    Red,
    Orange,
    Blue,
}

impl Tone {
    /// The colour of a VM power action's button (as on `paint_tool`).
    pub fn of_action(op: &str) -> Tone {
        match op {
            "start" | "resume" | "clean_reboot" => Tone::Green,
            "clean_shutdown" | "hard_shutdown" => Tone::Red,
            "hard_reboot" => Tone::Orange,
            "suspend" => Tone::Blue,
            "revert" => Tone::Orange,
            _ => Tone::Accent,
        }
    }

    pub fn color(self, p: &Pal) -> Color32 {
        match self {
            Tone::Accent => p.accent,
            Tone::Green => p.green,
            Tone::Red => p.red,
            Tone::Orange => p.orange,
            Tone::Blue => p.blue,
        }
    }
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

/// A camera around `c`: its body, the bump on top and the lens.
fn camera(p: &Painter, c: Pos2, s: Stroke) {
    p.rect_stroke(Rect::from_min_max(c + vec2(-6.5, -3.5), c + vec2(6.5, 5.5)), Rounding::same(1.6), s);
    line(p, c, &[(-3.0, -3.5), (-1.8, -5.6), (1.8, -5.6), (3.0, -3.5)], s);
    p.circle_stroke(c + vec2(0.0, 1.0), 2.4, s);
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
        IconKind::Busy(tone) => {
            // a faint ring, and an arc going round it
            let col = tone.color(pal);
            let turn = (p.ctx().input(|i| i.time) * 300.0 % 360.0) as f32;
            p.circle_stroke(o + vec2(8.0, 8.0), 5.8, Stroke::new(1.8, col.gamma_multiply(0.22)));
            arc(p, o + vec2(8.0, 8.0), 5.8, turn, turn + 110.0, Stroke::new(1.9, col));
            p.ctx().request_repaint();
        }
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
        IconKind::IsoSr => {
            // a disc: its edge, the hole and a glint
            p.circle_stroke(o + vec2(8.0, 8.0), 6.3, s);
            p.circle_stroke(o + vec2(8.0, 8.0), 1.7, Stroke::new(1.2, pal.icon));
            arc(p, o + vec2(8.0, 8.0), 4.0, 200.0, 265.0, Stroke::new(1.0, pal.icon.gamma_multiply(0.7)));
        }
        IconKind::Storage { iso, health } => {
            paint_icon(p, rect, if iso { IconKind::IsoSr } else { IconKind::Sr }, pal, bg);
            match health {
                SrHealth::Up => dot(p, o, pal.green, bg),
                SrHealth::Off => dot(p, o, pal.red, bg),
                SrHealth::Partly | SrHealth::Nowhere => {
                    let c = o + vec2(12.5, 12.5);
                    p.circle(c, 3.9, if health == SrHealth::Nowhere { pal.red } else { pal.orange }, Stroke::new(1.4, bg));
                    p.line_segment([c + vec2(0.0, -2.2), c + vec2(0.0, 0.6)], Stroke::new(1.3, Color32::WHITE));
                    p.circle_filled(c + vec2(0.0, 2.0), 0.75, Color32::WHITE);
                }
            }
        }
        IconKind::Snapshot => camera(p, o + vec2(8.0, 8.0), s),
        IconKind::Template => {
            let faint = Stroke::new(1.1, pal.icon.gamma_multiply(0.55));
            line(p, o, &[(4.5, 1.0), (15.0, 1.0), (15.0, 8.0)], faint);
            line(p, o, &[(3.0, 2.5), (13.5, 2.5), (13.5, 9.5)], faint);
            rrect(p, o, 1.0, 4.0, 11.0, 8.0, 1.4, s);
            line(p, o, &[(4.0, 14.5), (9.0, 14.5)], s);
            line(p, o, &[(6.5, 12.0), (6.5, 14.5)], s);
        }
        IconKind::Folder => {
            line(p, o, &[(1.5, 13.0), (1.5, 3.5), (6.0, 3.5), (7.5, 5.0), (14.5, 5.0), (14.5, 13.0), (1.5, 13.0)], s);
            line(p, o, &[(1.5, 7.0), (14.5, 7.0)], Stroke::new(1.0, pal.icon.gamma_multiply(0.6)));
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
    let col = if enabled { tool_color(op, pal) } else { pal.ink_faint };
    let neutral = matches!(op, "sidebar" | "theme-dark" | "theme-light" | "settings");
    if !neutral {
        let tint = if enabled { col.gamma_multiply(if pal.dark { 0.22 } else { 0.14 }) } else { Color32::TRANSPARENT };
        p.rect_filled(rect.shrink(0.5), Rounding::same(6.0), tint);
    }
    paint_tool_glyph(p, rect, op, enabled, pal);
}

/// A toolbar glyph's colour: its action's (a VM's state's, for the power menu's face).
pub fn tool_color(op: &str, pal: &Pal) -> Color32 {
    match op {
        "clean_shutdown" | "hard_shutdown" => pal.red,
        "clean_reboot" => pal.green,
        "hard_reboot" => pal.orange,
        "suspend" | "resume" => pal.blue,
        "start" | "connect" => pal.green,
        // the power menu's face: the VM's state, as its dot in the tree
        "power-running" => pal.green,
        "power-halted" => pal.red,
        "power-suspended" => pal.blue,
        "power-paused" => pal.orange,
        "newvm" => pal.blue,
        "iso" => pal.orange,
        "updates" => Color32::from_rgb(150, 110, 230),
        _ => pal.icon,
    }
}

/// `paint_tool`'s glyph alone, without its square (for a button that draws its own).
pub fn paint_tool_glyph(p: &Painter, rect: Rect, op: &str, enabled: bool, pal: &Pal) {
    let c = rect.center();
    let col = if enabled { tool_color(op, pal) } else { pal.ink_faint };
    let s = Stroke::new(2.0, col);
    let o = rect.min;
    match op {
        "power-running" | "power-halted" | "power-suspended" | "power-paused" => {
            arc(p, c + vec2(0.0, 0.8), 5.6, -55.0, 235.0, s);
            p.line_segment([c + vec2(0.0, -6.5), c + vec2(0.0, -0.5)], s);
        }
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
        "iso" => {
            // a disc: its edge, the hole, and a glint
            p.circle_stroke(c, 7.2, Stroke::new(1.8, col));
            p.circle_stroke(c, 2.0, Stroke::new(1.5, col));
            arc(p, c, 4.7, 200.0, 265.0, Stroke::new(1.3, col));
        }
        "updates" => {
            // a box with an arrow coming down into it
            let s = Stroke::new(1.8, col);
            line(p, o, &[(11.0, 3.0), (11.0, 12.5)], s);
            line(p, o, &[(7.2, 8.8), (11.0, 12.6), (14.8, 8.8)], s);
            line(p, o, &[(5.5, 11.0), (4.0, 11.0), (4.0, 18.5), (18.0, 18.5), (18.0, 11.0), (16.5, 11.0)], s);
        }
        "newvm" => {
            // a screen on its stand, a plus on it
            let s = Stroke::new(1.8, col);
            rrect(p, o, 3.5, 4.0, 15.0, 10.5, 2.0, s);
            line(p, o, &[(8.0, 18.0), (14.0, 18.0)], s);
            line(p, o, &[(11.0, 14.5), (11.0, 18.0)], s);
            line(p, o, &[(11.0, 6.6), (11.0, 11.9)], Stroke::new(1.7, col));
            line(p, o, &[(8.3, 9.25), (13.7, 9.25)], Stroke::new(1.7, col));
        }
        "connect" => {
            // a plug: two pins, a rounded body and its cord
            let s = Stroke::new(1.8, col);
            line(p, o, &[(8.5, 3.5), (8.5, 7.5)], s);
            line(p, o, &[(13.5, 3.5), (13.5, 7.5)], s);
            line(p, o, &[(5.5, 7.5), (16.5, 7.5), (16.5, 10.5), (14.8, 13.6), (11.0, 15.0), (7.2, 13.6), (5.5, 10.5), (5.5, 7.5)], s);
            line(p, o, &[(11.0, 15.0), (11.0, 19.0)], s);
        }
        "settings" => {
            // a gear: eight teeth round a ring, a hole in the middle
            for i in 0..8 {
                let a = (i as f32 * 45.0 + 22.5).to_radians();
                let d = vec2(a.cos(), a.sin());
                p.line_segment([c + d * 5.4, c + d * 8.2], Stroke::new(2.6, col));
            }
            p.circle_stroke(c, 5.6, Stroke::new(1.8, col));
            p.circle_stroke(c, 2.1, Stroke::new(1.5, col));
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


/// A 16×16 one-colour glyph for the icon beside a button's label (Rescan, Edit, OK…), centred in
/// `rect`. Unknown names draw nothing.
pub fn paint_glyph(p: &Painter, rect: Rect, name: &str, col: Color32) {
    paint_glyph_on(p, rect, name, col, Color32::TRANSPARENT);
}

/// `paint_glyph` on a known background `bg`, which cuts a gap around a stroke-through (Disconnect).
pub fn paint_glyph_on(p: &Painter, rect: Rect, name: &str, col: Color32, bg: Color32) {
    let c = rect.center();
    let s = Stroke::new(1.5, col);
    match name {
        "camera" => camera(p, c, s),
        "activity" => line(p, c, &[(-7.0, 1.0), (-3.5, 1.0), (-1.5, -5.0), (1.5, 6.0), (3.5, -1.0), (7.0, -1.0)], s),
        "template" => {
            // a screen with copies of it behind (as the Template icon)
            let o = c - vec2(8.0, 8.0);
            let faint = Stroke::new(1.1, col.gamma_multiply(0.6));
            line(p, o, &[(4.5, 1.0), (15.0, 1.0), (15.0, 8.0)], faint);
            line(p, o, &[(3.0, 2.5), (13.5, 2.5), (13.5, 9.5)], faint);
            p.rect_stroke(Rect::from_min_size(o + vec2(1.0, 4.0), vec2(11.0, 8.0)), Rounding::same(1.4), s);
            line(p, o, &[(4.0, 14.5), (9.0, 14.5)], s);
            line(p, o, &[(6.5, 12.0), (6.5, 14.5)], s);
        }
        "cpu" => {
            // a processor: a square chip, its core, pins on every side
            p.rect_stroke(Rect::from_center_size(c, vec2(9.0, 9.0)), Rounding::same(1.5), s);
            p.rect_filled(Rect::from_center_size(c, vec2(3.4, 3.4)), Rounding::same(0.5), col);
            let pin = Stroke::new(1.2, col);
            for d in [-2.5f32, 0.0, 2.5] {
                line(p, c, &[(d, -4.5), (d, -7.0)], pin);
                line(p, c, &[(d, 4.5), (d, 7.0)], pin);
                line(p, c, &[(-4.5, d), (-7.0, d)], pin);
                line(p, c, &[(4.5, d), (7.0, d)], pin);
            }
        }
        "boot" => {
            // a list in order: a dot and a line, three times
            for y in [-4.5f32, 0.0, 4.5] {
                p.circle_filled(c + vec2(-5.2, y), 1.3, col);
                line(p, c, &[(-2.2, y), (6.5, y)], s);
            }
        }
        "shield" => {
            // a shield (firmware, Secure Boot)
            let pts: Vec<Pos2> = [(0.0, -6.8), (5.6, -4.6), (5.4, 0.6), (3.6, 4.0), (0.0, 6.8), (-3.6, 4.0), (-5.4, 0.6), (-5.6, -4.6)].iter().map(|(x, y)| c + vec2(*x, *y)).collect();
            p.add(Shape::closed_line(pts, s));
        }
        "memory" => {
            // a memory module: the board, three chips, the pins below
            p.rect_stroke(Rect::from_min_max(c + vec2(-7.0, -4.0), c + vec2(7.0, 2.5)), Rounding::same(1.0), s);
            for x in [-4.5f32, -1.0, 2.5] {
                p.rect_filled(Rect::from_min_size(c + vec2(x, -2.2), vec2(2.4, 2.8)), Rounding::ZERO, col);
            }
            for x in [-5.0f32, -2.5, 0.0, 2.5, 5.0] {
                line(p, c, &[(x, 2.5), (x, 4.8)], Stroke::new(1.1, col));
            }
        }
        "revert" => {
            // an arrow going back round (Rescan's, the other way)
            arc(p, c, 5.0, -90.0, 210.0, s);
            poly(p, c, &[(-3.6, -7.4), (-6.9, -2.6), (-1.5, -2.3)], col);
        }
        "rescan" | "retry" => {
            // an arrow going round, its head where it starts
            arc(p, c, 5.0, -30.0, 270.0, s);
            poly(p, c, &[(3.6, -7.4), (6.9, -2.6), (1.5, -2.3)], col);
        }
        "connect" | "disconnect" => {
            // a plug; struck through to disconnect
            let o = c - vec2(8.0, 8.0);
            line(p, o, &[(6.0, 1.5), (6.0, 4.5)], s);
            line(p, o, &[(10.0, 1.5), (10.0, 4.5)], s);
            line(p, o, &[(3.5, 4.5), (12.5, 4.5), (12.5, 7.0), (11.0, 9.6), (8.0, 10.8), (5.0, 9.6), (3.5, 7.0), (3.5, 4.5)], s);
            line(p, o, &[(8.0, 10.8), (8.0, 14.5)], s);
            if name == "disconnect" {
                line(p, o, &[(1.5, 14.5), (14.5, 1.5)], Stroke::new(4.5, bg));
                line(p, o, &[(1.5, 14.5), (14.5, 1.5)], s);
            }
        }
        "edit" => {
            // a pencil, its point at the lower left
            line(p, c, &[(-5.5, 5.5), (-4.8, 2.4), (2.6, -5.0), (5.0, -2.6), (-2.4, 4.8), (-5.5, 5.5)], s);
            line(p, c, &[(0.8, -3.2), (3.2, -0.8)], s);
        }
        "upload" => {
            // an arrow rising out of a tray
            line(p, c, &[(0.0, -6.0), (0.0, 2.5)], s);
            line(p, c, &[(-3.8, -2.4), (0.0, -6.2), (3.8, -2.4)], s);
            line(p, c, &[(-6.0, 1.5), (-6.0, 5.5), (6.0, 5.5), (6.0, 1.5)], s);
        }
        "download" => {
            // an arrow going down into a tray
            line(p, c, &[(0.0, -6.2), (0.0, 2.3)], s);
            line(p, c, &[(-3.8, -1.5), (0.0, 2.3), (3.8, -1.5)], s);
            line(p, c, &[(-6.0, 1.5), (-6.0, 5.5), (6.0, 5.5), (6.0, 1.5)], s);
        }
        "back" => {
            line(p, c, &[(5.5, 0.0), (-5.0, 0.0)], s);
            line(p, c, &[(-1.0, -4.0), (-5.0, 0.0), (-1.0, 4.0)], s);
        }
        "laptop" => {
            // a screen over a keyboard's edge
            p.rect_stroke(Rect::from_min_max(c + vec2(-5.5, -5.5), c + vec2(5.5, 2.5)), Rounding::same(1.5), s);
            line(p, c, &[(-7.5, 5.0), (7.5, 5.0)], s);
        }
        "globe" => {
            // the outline, a meridian and two parallels
            let r = 6.5;
            p.circle_stroke(c, r, s);
            let meridian: Vec<Pos2> = (0..=32).map(|i| {
                let a = i as f32 / 32.0 * std::f32::consts::TAU;
                c + vec2(2.8 * a.cos(), r * a.sin())
            }).collect();
            p.add(Shape::line(meridian, Stroke::new(1.2, col)));
            line(p, c, &[(-r, 0.0), (r, 0.0)], Stroke::new(1.2, col));
            line(p, c, &[(-5.2, -3.4), (5.2, -3.4)], Stroke::new(1.0, col));
            line(p, c, &[(-5.2, 3.4), (5.2, 3.4)], Stroke::new(1.0, col));
        }
        "newvm" => {
            // a screen with a plus
            p.rect_stroke(Rect::from_min_max(c + vec2(-6.5, -5.5), c + vec2(6.5, 3.5)), Rounding::same(1.5), s);
            line(p, c, &[(-3.0, 6.3), (3.0, 6.3)], s);
            line(p, c, &[(0.0, -3.5), (0.0, 1.5)], Stroke::new(1.4, col));
            line(p, c, &[(-2.5, -1.0), (2.5, -1.0)], Stroke::new(1.4, col));
        }
        "trash" => {
            // a bin: its lid and handle, the can, two grooves
            line(p, c, &[(-6.0, -4.0), (6.0, -4.0)], s);
            line(p, c, &[(-2.2, -4.0), (-2.2, -6.2), (2.2, -6.2), (2.2, -4.0)], Stroke::new(1.3, col));
            line(p, c, &[(-4.6, -4.0), (-3.8, 6.2), (3.8, 6.2), (4.6, -4.0)], s);
            line(p, c, &[(-1.4, -1.5), (-1.4, 3.6)], Stroke::new(1.1, col));
            line(p, c, &[(1.4, -1.5), (1.4, 3.6)], Stroke::new(1.1, col));
        }
        "check" => line(p, c, &[(-5.0, 0.3), (-1.6, 3.8), (5.0, -3.6)], Stroke::new(1.7, col)),
        "warning" => {
            // a triangle with an exclamation mark
            line(p, c, &[(0.0, -6.5), (7.0, 5.5), (-7.0, 5.5), (0.0, -6.5)], Stroke::new(1.4, col));
            line(p, c, &[(0.0, -2.2), (0.0, 1.6)], Stroke::new(1.6, col));
            p.circle_filled(c + vec2(0.0, 3.7), 0.95, col);
        }
        "power" => {
            // a power sign: a ring open at the top, a bar through the gap
            arc(p, c + vec2(0.0, 0.8), 5.4, -55.0, 235.0, s);
            p.line_segment([c + vec2(0.0, -6.2), c + vec2(0.0, -0.6)], s);
        }
        "more" => {
            // three dots in a row
            for x in [-5.0, 0.0, 5.0] {
                p.circle_filled(c + vec2(x, 0.0), 1.6, col);
            }
        }
        "error" => {
            // a cross in a circle
            p.circle_stroke(c, 6.8, Stroke::new(1.4, col));
            let s = Stroke::new(1.5, col);
            line(p, c, &[(-2.6, -2.6), (2.6, 2.6)], s);
            line(p, c, &[(2.6, -2.6), (-2.6, 2.6)], s);
        }
        "close" => {
            let s = Stroke::new(1.7, col);
            line(p, c, &[(-4.0, -4.0), (4.0, 4.0)], s);
            line(p, c, &[(4.0, -4.0), (-4.0, 4.0)], s);
        }
        "plus" => {
            let s = Stroke::new(1.7, col);
            line(p, c, &[(-5.0, 0.0), (5.0, 0.0)], s);
            line(p, c, &[(0.0, -5.0), (0.0, 5.0)], s);
        }
        "copy" => {
            // a sheet in front of another
            p.rect_stroke(Rect::from_min_max(c + vec2(-2.5, -2.5), c + vec2(5.5, 5.5)), Rounding::same(1.5), s);
            line(p, c, &[(-5.5, 2.5), (-5.5, -4.0), (-4.0, -5.5), (2.5, -5.5)], s);
        }
        "lock" => {
            // a padlock (Ctrl+Alt+Del brings up Windows' lock screen)
            p.rect_stroke(Rect::from_min_max(c + vec2(-5.0, -1.0), c + vec2(5.0, 6.0)), Rounding::same(1.5), s);
            arc(p, c + vec2(0.0, -2.5), 3.0, 180.0, 360.0, s);
            line(p, c, &[(-3.0, -2.5), (-3.0, -1.0)], s);
            line(p, c, &[(3.0, -2.5), (3.0, -1.0)], s);
        }
        "start" => poly(p, c, &[(-3.5, -5.5), (5.5, 0.0), (-3.5, 5.5)], col),
        "resume" => {
            p.rect_filled(Rect::from_min_size(c + vec2(-5.5, -5.0), vec2(2.2, 10.0)), Rounding::same(1.0), col);
            poly(p, c, &[(-1.5, -5.5), (5.5, 0.0), (-1.5, 5.5)], col);
        }
        "keyboard" => {
            // an outline, two rows of keys and a space bar
            let k = Rect::from_center_size(c, vec2(17.0, 11.0));
            p.rect_stroke(k, Rounding::same(2.0), Stroke::new(1.3, col));
            for row in 0..2 {
                for i in 0..5 {
                    let at = pos2(k.min.x + 3.0 + i as f32 * 2.75, k.min.y + 3.0 + row as f32 * 2.5);
                    p.rect_filled(Rect::from_center_size(at, vec2(1.4, 1.2)), 0.0, col);
                }
            }
            p.line_segment([pos2(k.min.x + 5.0, k.max.y - 2.5), pos2(k.max.x - 5.0, k.max.y - 2.5)], Stroke::new(1.2, col));
        }
        "screen" | "fit" => {
            // a monitor on its stand; to fit, arrows pointing to its corners
            let m = Rect::from_center_size(c - vec2(0.0, 1.5), vec2(17.0, 11.0));
            let s = Stroke::new(1.3, col);
            p.rect_stroke(m, Rounding::same(1.5), s);
            p.line_segment([pos2(m.center().x, m.max.y), pos2(m.center().x, m.max.y + 2.5)], s);
            p.line_segment([pos2(m.center().x - 3.5, m.max.y + 2.5), pos2(m.center().x + 3.5, m.max.y + 2.5)], s);
            if name == "fit" {
                let s = Stroke::new(1.1, col);
                let (a, b) = (m.shrink2(vec2(3.0, 2.5)).min, m.shrink2(vec2(3.0, 2.5)).max);
                for (corner, dx, dy) in [(a, 2.5, 2.0), (pos2(b.x, a.y), -2.5, 2.0), (pos2(a.x, b.y), 2.5, -2.0), (b, -2.5, -2.0)] {
                    p.line_segment([corner, corner + vec2(dx, 0.0)], s);
                    p.line_segment([corner, corner + vec2(0.0, dy)], s);
                }
            }
        }
        _ => {}
    }
}
