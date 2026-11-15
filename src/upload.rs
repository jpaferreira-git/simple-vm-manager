//! The Upload dialog of an ISO library. First a choice: a file from this computer, or one straight
//! from a link on the web. A link is checked once typing pauses (its file's name and size), and
//! its bytes then go through this app into the library as they arrive, never stored here.

use crate::app::{dialog_button, dialog_buttons, dialog_window, field_label, text_field, Look, LABEL_WIDTH};
use crate::conn::WebFile;
use crate::icons::paint_glyph;
use crate::model::{fmt_bytes, iso_file_name};
use crate::theme::Pal;
use eframe::egui::{self, pos2, vec2, Align2, FontFamily, FontId, Rect, Rounding, Sense, Stroke, Ui};
use std::time::{Duration, Instant};

/// How long typing must pause before the link is checked.
const PAUSE: Duration = Duration::from_millis(600);

pub enum UploadAction {
    None,
    Close,
    /// Open the file chooser.
    PickLocal,
    /// Check this link (answer with `UploadDialog::answer`).
    Check(String),
    /// Download this link into the library under this name.
    Download { url: String, name: String, size: u64 },
}

pub struct UploadDialog {
    pub sid: u32,
    pub sr: String,
    lib: String,
    /// The names already in the library.
    existing: Vec<String>,
    web: bool,
    url: String,
    name: String,
    /// The name was typed, so a newly checked link doesn't replace it.
    name_typed: bool,
    /// The link last sent to be checked, and its answer (None while waiting).
    asked: String,
    info: Option<Result<WebFile, String>>,
    /// When the link last changed.
    changed: Option<Instant>,
    /// Put the cursor in the link box (just switched to the link form).
    focus: bool,
}

impl UploadDialog {
    pub fn new(sid: u32, sr: String, lib: String, existing: Vec<String>) -> UploadDialog {
        UploadDialog { sid, sr, lib, existing, web: false, url: String::new(), name: String::new(), name_typed: false, asked: String::new(), info: None, changed: None, focus: false }
    }

    /// For the screenshot checks: the link form, with a link already checked.
    pub fn with_link(mut self, url: &str, file: WebFile) -> UploadDialog {
        self.web = true;
        self.url = url.into();
        self.asked = url.into();
        self.answer(url, Ok(file));
        self
    }

    /// What checking a link found; an answer for a link that's since changed is dropped.
    pub fn answer(&mut self, url: &str, result: Result<WebFile, String>) {
        if url != self.asked {
            return;
        }
        if let (Ok(f), false) = (&result, self.name_typed) {
            self.name = iso_file_name(&f.name);
        }
        self.info = Some(result);
    }

    pub fn ui(&mut self, ctx: &egui::Context, pal: &Pal) -> UploadAction {
        let mut action = UploadAction::None;
        let title = format!("Upload to {}", self.lib);
        dialog_window(ctx, &title).id(egui::Id::new("upload-dialog")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            if self.web {
                self.link_form(ui, pal, &mut action);
            } else {
                if choice(ui, pal, "laptop", "Upload local file", "An .iso file on this computer.") {
                    action = UploadAction::PickLocal;
                }
                if choice(ui, pal, "globe", "Upload directly via web", "Paste a link to an .iso file; it goes straight into the library.") {
                    self.web = true;
                    self.focus = true;
                }
                ui.add_space(4.0);
                dialog_buttons(ui, |ui| {
                    if dialog_button(ui, pal, "close", "Cancel", Look::Plain, true) {
                        action = UploadAction::Close;
                    }
                });
            }
        });
        if ui_escape(ctx) {
            action = UploadAction::Close;
        }
        action
    }

    fn link_form(&mut self, ui: &mut Ui, pal: &Pal, action: &mut UploadAction) {
        ui.label(egui::RichText::new("Paste the link to an .iso file. The file comes through this app on its way to the library, without being saved on this computer.").color(pal.ink_soft));
        ui.add_space(2.0);
        let link = ui.horizontal(|ui| {
            field_label(ui, "Link:", LABEL_WIDTH);
            text_field(ui, pal, &mut self.url, ui.available_width(), false)
        });
        let link = link.inner;
        if self.focus {
            link.request_focus(); // ready to paste
            self.focus = false;
        }
        if link.changed() {
            self.changed = Some(Instant::now());
            self.info = None;
            self.asked.clear();
        }
        let url = self.url.trim().to_string();
        let valid = url.starts_with("https://") || url.starts_with("http://");
        let typed_enter = link.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        // check the link once typing pauses (or on Enter)
        if valid && url != self.asked {
            let paused = self.changed.map_or(true, |t| t.elapsed() >= PAUSE);
            if paused || typed_enter {
                self.asked = url.clone();
                self.info = None;
                *action = UploadAction::Check(url.clone());
            } else {
                ui.ctx().request_repaint_after(PAUSE);
            }
        }

        // what the link leads to
        let status = |ui: &mut Ui, add: &dyn Fn(&mut Ui)| {
            ui.horizontal(|ui| {
                ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x + 2.0);
                add(ui);
            });
        };
        let soft = |t: &str| egui::RichText::new(t).size(12.0).color(pal.ink_soft);
        if url.is_empty() {
            status(ui, &|ui| {
                ui.label(soft("For example https://releases.ubuntu.com/24.04/ubuntu-24.04.3-live-server-amd64.iso"));
            });
        } else if !valid {
            status(ui, &|ui| {
                ui.label(egui::RichText::new("A link starts with https:// or http://").size(12.0).color(pal.red));
            });
        } else {
            match &self.info {
                None => {
                    status(ui, &|ui| {
                        ui.add(egui::Spinner::new().size(12.0));
                        ui.label(soft("Checking the link…"));
                    });
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
                Some(Ok(f)) => {
                    let text = format!("{}  ·  {}", f.name, fmt_bytes(f.size as i64));
                    status(ui, &|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                        paint_glyph(ui.painter(), r, "check", pal.green);
                        ui.add(egui::Label::new(soft(&text)).truncate());
                    });
                }
                Some(Err(e)) => {
                    let e = e.clone();
                    status(ui, &|ui| {
                        ui.add(egui::Label::new(egui::RichText::new(e.as_str()).size(12.0).color(pal.red)).wrap());
                    });
                }
            }
        }

        let found = matches!(self.info, Some(Ok(_)));
        let name = ui.add_enabled_ui(found, |ui| {
            ui.horizontal(|ui| {
                field_label(ui, "Save as:", LABEL_WIDTH);
                text_field(ui, pal, &mut self.name, ui.available_width(), false)
            })
            .inner
        });
        let name = name.inner;
        if name.changed() {
            self.name_typed = true;
        }
        if name.lost_focus() && !self.name.trim().is_empty() {
            self.name = iso_file_name(&self.name); // the names an ISO library takes
        }
        let final_name = iso_file_name(&self.name);
        let taken = found && self.existing.iter().any(|n| n.eq_ignore_ascii_case(&final_name));
        if taken {
            status(ui, &|ui| {
                ui.label(egui::RichText::new(format!("There's already a file named {} in {}.", final_name, self.lib)).size(12.0).color(pal.red));
            });
        }

        ui.add_space(4.0);
        let ready = found && !taken && !self.name.trim().is_empty();
        dialog_buttons(ui, |ui| {
            if dialog_button(ui, pal, "close", "Cancel", Look::Plain, true) {
                *action = UploadAction::Close;
            }
            if dialog_button(ui, pal, "download", "Download", Look::Primary, ready) {
                if let Some(Ok(f)) = &self.info {
                    *action = UploadAction::Download { url: self.asked.clone(), name: final_name.clone(), size: f.size };
                }
            }
            if dialog_button(ui, pal, "back", "Back", Look::Plain, true) {
                self.web = false;
            }
        });
    }
}

fn ui_escape(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.key_pressed(egui::Key::Escape))
}

/// One of the dialog's two choices: an icon on a tinted square, a title and a line under it.
fn choice(ui: &mut Ui, pal: &Pal, glyph: &str, title: &str, text: &str) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 62.0), Sense::click());
    let p = ui.painter();
    let hot = resp.hovered();
    p.rect(r, Rounding::same(8.0), if hot { pal.hover } else { pal.surface }, Stroke::new(1.0, if hot { pal.accent } else { pal.border }));
    let icon = Rect::from_center_size(pos2(r.min.x + 32.0, r.center().y), vec2(36.0, 36.0));
    p.rect_filled(icon, Rounding::same(8.0), pal.accent.gamma_multiply(if pal.dark { 0.22 } else { 0.12 }));
    paint_glyph(p, Rect::from_center_size(icon.center(), vec2(16.0, 16.0)), glyph, pal.accent);
    let x = r.min.x + 62.0;
    p.text(pos2(x, r.center().y - 9.0), Align2::LEFT_CENTER, title, FontId::new(14.0, FontFamily::Name("bold".into())), pal.ink);
    p.text(pos2(x, r.center().y + 10.0), Align2::LEFT_CENTER, text, FontId::proportional(12.5), pal.ink_soft);
    // a chevron: this leads somewhere
    let c = pos2(r.max.x - 18.0, r.center().y);
    p.add(egui::Shape::line(vec![c + vec2(-2.0, -4.5), c + vec2(2.5, 0.0), c + vec2(-2.0, 4.5)], Stroke::new(1.5, pal.ink_soft)));
    resp.clicked()
}
