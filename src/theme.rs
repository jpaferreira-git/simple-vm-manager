//! Light and dark colour palettes. Every colour the window draws comes from here, so the whole
//! app (icons included) follows the chosen theme.

use eframe::egui::{self, Color32};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ThemeMode {
    System,
    Light,
    Dark,
}

impl ThemeMode {
    pub fn parse(s: &str) -> Option<ThemeMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "system" => Some(ThemeMode::System),
            "light" => Some(ThemeMode::Light),
            "dark" => Some(ThemeMode::Dark),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ThemeMode::System => "system",
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Pal {
    pub dark: bool,
    pub ink: Color32,
    pub ink_soft: Color32,
    pub ink_faint: Color32,
    pub window: Color32,
    pub surface: Color32,
    pub sidebar: Color32,
    pub rail: Color32,
    pub input: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub toolbar: Color32,
    pub hover: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub link: Color32,
    pub icon: Color32,
    pub art: Color32,
    pub art_fill: Color32,
    pub nav_on: Color32,
    pub green: Color32,
    pub red: Color32,
    pub orange: Color32,
    pub blue: Color32,
    pub grey: Color32,
    pub stripe: Color32,
    /// A caution's background and its text: not enough memory, and the like.
    pub caution: Color32,
    pub caution_ink: Color32,
    /// A stop's background and its text: storage that's offline, and the like.
    pub danger: Color32,
    pub danger_ink: Color32,
}

pub const LIGHT: Pal = Pal {
    dark: false,
    ink: Color32::from_rgb(30, 34, 40),
    ink_soft: Color32::from_rgb(94, 102, 114),
    ink_faint: Color32::from_rgb(160, 166, 176),
    window: Color32::from_rgb(246, 247, 249),
    surface: Color32::WHITE,
    sidebar: Color32::WHITE,
    rail: Color32::from_rgb(240, 242, 246),
    input: Color32::WHITE,
    border: Color32::from_rgb(214, 218, 224),
    border_strong: Color32::from_rgb(160, 166, 176),
    toolbar: Color32::from_rgb(244, 246, 250),
    hover: Color32::from_rgb(229, 241, 251),
    accent: Color32::from_rgb(0, 120, 215),
    on_accent: Color32::WHITE,
    link: Color32::from_rgb(40, 90, 170),
    icon: Color32::from_rgb(70, 80, 96),
    art: Color32::from_rgb(70, 110, 160),
    art_fill: Color32::from_rgb(236, 242, 250),
    nav_on: Color32::from_rgb(218, 223, 232),
    green: Color32::from_rgb(34, 150, 60),
    red: Color32::from_rgb(205, 55, 50),
    orange: Color32::from_rgb(222, 140, 20),
    blue: Color32::from_rgb(30, 110, 210),
    grey: Color32::from_rgb(120, 128, 140),
    stripe: Color32::from_rgb(244, 246, 249),
    caution: Color32::from_rgb(255, 235, 156),
    caution_ink: Color32::from_rgb(156, 87, 0),
    danger: Color32::from_rgb(255, 199, 206),
    danger_ink: Color32::from_rgb(156, 0, 6),
};

pub const DARK: Pal = Pal {
    dark: true,
    ink: Color32::from_rgb(228, 231, 236),
    ink_soft: Color32::from_rgb(160, 168, 180),
    ink_faint: Color32::from_rgb(98, 106, 118),
    window: Color32::from_rgb(24, 27, 32),
    surface: Color32::from_rgb(33, 37, 44),
    sidebar: Color32::from_rgb(29, 32, 38),
    rail: Color32::from_rgb(22, 25, 30),
    input: Color32::from_rgb(38, 43, 51),
    border: Color32::from_rgb(52, 58, 68),
    border_strong: Color32::from_rgb(78, 86, 98),
    toolbar: Color32::from_rgb(33, 37, 44),
    hover: Color32::from_rgb(44, 56, 74),
    accent: Color32::from_rgb(56, 139, 233),
    on_accent: Color32::WHITE,
    link: Color32::from_rgb(110, 168, 254),
    icon: Color32::from_rgb(184, 192, 204),
    art: Color32::from_rgb(122, 162, 216),
    art_fill: Color32::from_rgb(38, 46, 60),
    nav_on: Color32::from_rgb(46, 52, 62),
    green: Color32::from_rgb(63, 185, 80),
    red: Color32::from_rgb(248, 81, 73),
    orange: Color32::from_rgb(240, 160, 48),
    blue: Color32::from_rgb(88, 166, 255),
    grey: Color32::from_rgb(140, 148, 160),
    stripe: Color32::from_rgb(38, 42, 50),
    caution: Color32::from_rgb(255, 235, 156),
    caution_ink: Color32::from_rgb(156, 87, 0),
    danger: Color32::from_rgb(255, 199, 206),
    danger_ink: Color32::from_rgb(156, 0, 6),
};

/// egui's own widgets (menus, text boxes, dialogs, tables) in the same colours.
pub fn visuals(p: &Pal) -> egui::Visuals {
    let mut v = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = p.window;
    v.window_fill = p.surface;
    v.window_stroke = egui::Stroke::new(1.0_f32, p.border);
    v.extreme_bg_color = p.input;
    v.faint_bg_color = p.stripe;
    v.hyperlink_color = p.link;
    v.selection.bg_fill = p.accent;
    v.selection.stroke = egui::Stroke::new(1.0_f32, p.on_accent);
    v.override_text_color = Some(p.ink);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, p.border);
    v.widgets.hovered.weak_bg_fill = p.hover;
    v.widgets.hovered.bg_fill = p.hover;
    v.window_rounding = egui::Rounding::same(8.0);
    v.menu_rounding = egui::Rounding::same(6.0);
    v
}
