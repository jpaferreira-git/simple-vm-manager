//! The ISO Library dialog: a Windows (SMB) or NFS share that the servers mount as a library of
//! installation discs for new VMs. What's needed comes first; protocol versions and the rest wait
//! under Advanced, set to the usual choices. Creating it is the connection test: the server mounts
//! the share right away, and if that fails the dialog explains why in plain words and shows the
//! request (as the equivalent `xe` command) with the server's answer, ready to copy.

use crate::app::{dialog_button, dialog_buttons, dialog_window, field_label, flat_button, text_field, Look, LABEL_WIDTH};
use crate::conn::ApiError;
use crate::model::IsoLibrary;
use crate::theme::Pal;
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Rect, Rounding, Sense, Stroke, Ui};
use std::time::{Duration, Instant};

const SMB_VERSIONS: [&str; 2] = ["3.0", "1.0"];
pub(crate) const NFS_VERSIONS: [(&str, &str); 3] = [("3", "NFSv3"), ("4", "NFSv4"), ("4.1", "NFSv4.1")];

pub(crate) struct Failure {
    pub title: String,
    pub hint: String,
    pub transcript: String,
    pub copied: Option<Instant>,
}

pub enum IsoAction {
    None,
    Close,
    /// Create it in the pool with this id.
    Create(u32, IsoLibrary),
    /// Give library `sr` of pool `sid`, detached, these settings and mount it again.
    Reattach(u32, String, IsoLibrary),
}

/// A detached library being given its settings back: its reference and UUID.
struct Reattach {
    sr: String,
    uuid: String,
}

pub struct IsoDialog {
    sid: u32,
    /// The connected pools it could go in: (id, name).
    pools: Vec<(u32, String)>,
    name: String,
    description: String,
    nfs: bool,
    location: String,
    username: String,
    password: String,
    smb_version: usize,
    nfs_version: usize,
    shared: bool,
    advanced: bool,
    busy: Option<String>,
    problem: String,
    failure: Option<Failure>,
    reattach: Option<Reattach>,
}

impl IsoDialog {
    pub fn new(sid: u32, pools: Vec<(u32, String)>) -> IsoDialog {
        IsoDialog {
            sid,
            pools,
            name: "ISO Library".into(),
            description: String::new(),
            nfs: false,
            location: String::new(),
            username: String::new(),
            password: String::new(),
            smb_version: 0,
            nfs_version: 0,
            shared: true,
            advanced: false,
            busy: None,
            problem: String::new(),
            failure: None,
            reattach: None,
        }
    }

    /// For library `sr` (`name`, `uuid`) of pool `sid`, which the pool has no settings for any more
    /// (detached): asks where it is, to mount it again as it was.
    pub fn reattach(sid: u32, pool: String, sr: String, uuid: String, name: String, shared: bool) -> IsoDialog {
        IsoDialog { name, shared, reattach: Some(Reattach { sr, uuid }), ..IsoDialog::new(sid, vec![(sid, pool)]) }
    }

    /// For the screenshot checks: filled in, Advanced open or not.
    pub fn filled(mut self, location: &str, user: &str, password: &str, advanced: bool) -> IsoDialog {
        self.location = location.into();
        self.username = user.into();
        self.password = password.into();
        self.advanced = advanced;
        self
    }

    /// The server couldn't create it: say why, and show what was asked and what came back.
    pub fn failed(&mut self, lib: &IsoLibrary, e: &ApiError) {
        self.busy = None;
        let (mut title, hint) = explain(e, lib);
        if self.reattach.is_some() && mount_trouble(e).is_none() && e.code != "CONNECTION" && e.code != "RBAC_PERMISSION_DENIED" {
            title = format!("Couldn't mount '{}'", self.name);
        }
        let mut transcript = format!("$ {}\n", lib.xe_command(self.reattach.as_ref().map(|r| r.uuid.as_str())));
        transcript.push_str(&format!("Error code: {}\n", e.code));
        for d in e.details.iter().filter(|d| !d.trim().is_empty()) {
            transcript.push_str(d.trim());
            transcript.push('\n');
        }
        self.failure = Some(Failure { title, hint, transcript: transcript.trim_end().to_string(), copied: None });
    }

    /// Checks what was typed; if it looks right, the request to send (and the dialog waits).
    pub fn submit(&mut self) -> IsoAction {
        let lib = self.request();
        if let Some(p) = lib.location_problem() {
            self.problem = p.into();
            return IsoAction::None;
        }
        self.problem.clear();
        self.failure = None;
        let pool = self.pools.iter().find(|p| p.0 == self.sid).map(|p| p.1.clone()).unwrap_or_default();
        self.busy = Some(format!("Asking {} to connect to {}…", pool, lib.location));
        match &self.reattach {
            Some(r) => IsoAction::Reattach(self.sid, r.sr.clone(), lib),
            None => IsoAction::Create(self.sid, lib),
        }
    }

    fn request(&self) -> IsoLibrary {
        IsoLibrary {
            name: self.name.trim().to_string(),
            description: self.description.trim().to_string(),
            nfs: self.nfs,
            location: IsoLibrary::normalise_location(self.nfs, &self.location),
            username: if self.nfs { String::new() } else { self.username.trim().to_string() },
            password: if self.nfs { String::new() } else { self.password.clone() },
            smb_version: SMB_VERSIONS[self.smb_version].into(),
            nfs_version: NFS_VERSIONS[self.nfs_version].0.into(),
            shared: self.shared,
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, pal: &Pal) -> IsoAction {
        let mut action = IsoAction::None;
        let busy = self.busy.is_some();
        let again = self.reattach.is_some();
        dialog_window(ctx, if again { "Repair ISO Library" } else { "New ISO Library" }).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            let intro = if again {
                format!("The pool has no settings for reaching '{}' any more. Say where it is, and your servers mount it again, its ISO files and their descriptions as they were.", self.name)
            } else {
                "A shared folder with installation discs (.iso files). Your servers mount it to install new VMs from.".to_string()
            };
            ui.add(egui::Label::new(egui::RichText::new(intro).color(pal.ink_soft)).wrap());
            ui.add_space(2.0);
            ui.add_enabled_ui(!busy, |ui| {
                if self.pools.len() > 1 && !again {
                    ui.horizontal(|ui| {
                        field_label(ui, "Pool:", LABEL_WIDTH);
                        let current = self.pools.iter().find(|p| p.0 == self.sid).map(|p| p.1.clone()).unwrap_or_default();
                        egui::ComboBox::from_id_salt("iso-pool").selected_text(current).width(ui.available_width() - 8.0).show_ui(ui, |ui| {
                            for (id, name) in &self.pools {
                                ui.selectable_value(&mut self.sid, *id, name);
                            }
                        });
                    });
                }
                if !again {
                    ui.horizontal(|ui| {
                        field_label(ui, "Name:", LABEL_WIDTH);
                        text_field(ui, pal, &mut self.name, ui.available_width(), false);
                    });
                }
                ui.horizontal(|ui| {
                    field_label(ui, "Share type:", LABEL_WIDTH);
                    let mut i = self.nfs as usize;
                    if segmented(ui, pal, &["Windows (SMB)", "NFS"], &mut i, ui.available_width()) {
                        self.nfs = i == 1;
                        self.problem.clear();
                    }
                });
                ui.horizontal(|ui| {
                    field_label(ui, "Location:", LABEL_WIDTH);
                    text_field(ui, pal, &mut self.location, ui.available_width(), false);
                });
                note(ui, pal, if self.nfs { "For example 192.168.1.50:/mnt/tank/isos or nas.home.lan:/mnt/tank/isos" } else { "For example \\\\192.168.1.50\\isos or \\\\nas.home.lan\\isos (a folder inside the share works too)" });
                if !self.nfs {
                    ui.horizontal(|ui| {
                        field_label(ui, "Username:", LABEL_WIDTH);
                        text_field(ui, pal, &mut self.username, ui.available_width(), false);
                    });
                    ui.horizontal(|ui| {
                        field_label(ui, "Password:", LABEL_WIDTH);
                        text_field(ui, pal, &mut self.password, ui.available_width(), true);
                    });
                    note(ui, pal, "Leave both empty if the share allows guests.");
                }

                // Advanced: the less usual settings, already set to the common choices.
                let summary = if self.nfs { NFS_VERSIONS[self.nfs_version].1.to_string() } else { format!("SMB {}", SMB_VERSIONS[self.smb_version]) };
                advanced(ui, pal, &mut self.advanced, &summary);
                if self.advanced {
                    if self.nfs {
                        ui.horizontal(|ui| {
                            field_label(ui, "NFS version:", LABEL_WIDTH);
                            let names: Vec<&str> = NFS_VERSIONS.iter().map(|v| v.1).collect();
                            segmented(ui, pal, &names, &mut self.nfs_version, ui.available_width());
                        });
                    } else {
                        ui.horizontal(|ui| {
                            field_label(ui, "SMB version:", LABEL_WIDTH);
                            segmented(ui, pal, &SMB_VERSIONS, &mut self.smb_version, ui.available_width());
                        });
                        note(ui, pal, "1.0 only for old servers that can't do 3.0.");
                    }
                    if !again {
                        ui.horizontal(|ui| {
                            field_label(ui, "Description:", LABEL_WIDTH);
                            text_field(ui, pal, &mut self.description, ui.available_width(), false);
                        });
                        ui.horizontal(|ui| {
                            ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x);
                            ui.checkbox(&mut self.shared, "Available to every server in the pool");
                        });
                    }
                }
            });

            if !self.problem.is_empty() {
                ui.colored_label(pal.red, self.problem.as_str());
            }
            if let Some(what) = &self.busy {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.label(egui::RichText::new(what.as_str()).color(pal.ink_soft));
                });
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            if let Some(f) = self.failure.as_mut() {
                failure_panel(ui, pal, f);
            }

            ui.add_space(4.0);
            dialog_buttons(ui, |ui| {
                // while the server's at it there's nothing to cancel: the window closes, it goes on
                if dialog_button(ui, pal, "close", if busy { "Close" } else { "Cancel" }, Look::Plain, true) {
                    action = IsoAction::Close;
                }
                let ready = !busy && !self.name.trim().is_empty() && !self.location.trim().is_empty();
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                let (glyph, label) = if again { ("connect", "Repair") } else { ("plus", "Create") };
                if dialog_button(ui, pal, glyph, label, Look::Primary, ready) || (ready && enter && self.failure.is_none()) {
                    action = self.submit();
                }
            });
        });
        action
    }
}

/// The Advanced row that opens and folds the less usual settings, with what they're set to on
/// its right.
pub(crate) fn advanced(ui: &mut Ui, pal: &Pal, open: &mut bool, summary: &str) {
    ui.add_space(2.0);
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(r, Rounding::same(5.0), pal.hover);
    }
    let c = pos2(r.min.x + 10.0, r.center().y);
    let chev = if *open { vec![c + vec2(-3.5, -1.5), c + vec2(0.0, 2.0), c + vec2(3.5, -1.5)] } else { vec![c + vec2(-1.5, -3.5), c + vec2(2.0, 0.0), c + vec2(-1.5, 3.5)] };
    ui.painter().add(egui::Shape::line(chev, Stroke::new(1.4_f32, pal.ink_soft)));
    ui.painter().text(pos2(r.min.x + 22.0, r.center().y), Align2::LEFT_CENTER, "Advanced", FontId::proportional(13.0), pal.ink);
    ui.painter().text(pos2(r.max.x - 8.0, r.center().y), Align2::RIGHT_CENTER, summary, FontId::proportional(12.0), pal.ink_soft);
    if resp.clicked() {
        *open = !*open;
    }
}

/// A small grey line under a field.
pub(crate) fn note(ui: &mut Ui, pal: &Pal, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x + 2.0);
        ui.add(egui::Label::new(egui::RichText::new(text).size(12.0).color(pal.ink_soft)).wrap());
    });
}

/// A row of options, the chosen one raised; returns true when the choice changed.
pub(crate) fn segmented(ui: &mut Ui, pal: &Pal, options: &[&str], chosen: &mut usize, width: f32) -> bool {
    let (r, _) = ui.allocate_exact_size(vec2(width, 26.0), Sense::hover());
    ui.painter().rect_filled(r, Rounding::same(6.0), pal.rail);
    let part = r.width() / options.len() as f32;
    let mut changed = false;
    for (i, o) in options.iter().enumerate() {
        let pr = Rect::from_min_size(pos2(r.min.x + i as f32 * part, r.min.y), vec2(part, r.height())).shrink(2.5);
        let resp = ui.interact(pr, ui.id().with(("seg", *o)), Sense::click());
        let on = *chosen == i;
        if on {
            ui.painter().rect(pr, Rounding::same(4.5), pal.surface, Stroke::new(1.0_f32, pal.border));
        }
        let col = if !ui.is_enabled() { pal.ink_faint } else if on || resp.hovered() { pal.ink } else { pal.ink_soft };
        ui.painter().text(pr.center(), Align2::CENTER_CENTER, *o, FontId::proportional(12.5), col);
        if resp.clicked() && !on {
            *chosen = i;
            changed = true;
        }
    }
    changed
}

/// What went wrong in plain words, then the request and the server's answer as in a terminal.
pub(crate) fn failure_panel(ui: &mut Ui, pal: &Pal, f: &mut Failure) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(&f.title).font(FontId::new(14.0, egui::FontFamily::Name("bold".into()))).color(pal.red));
    ui.label(egui::RichText::new(&f.hint).color(pal.ink_soft));
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("What the server said").size(12.0).color(pal.ink_soft));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let copied = f.copied.map_or(false, |t| t.elapsed() < Duration::from_secs(2));
            let (glyph, label) = if copied { ("check", "Copied") } else { ("copy", "Copy") };
            if flat_button(ui, glyph, label, None, true, false, pal).on_hover_text("Copy the command and the server's answer").clicked() {
                ui.ctx().copy_text(f.transcript.clone());
                f.copied = Some(Instant::now());
            }
            if copied {
                ui.ctx().request_repaint_after(Duration::from_millis(250));
            }
        });
    });
    egui::Frame::none().fill(Color32::from_rgb(13, 17, 23)).rounding(Rounding::same(6.0)).inner_margin(egui::Margin::same(10.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::ScrollArea::vertical().max_height(150.0).auto_shrink([false, true]).show(ui, |ui| {
            for (i, line) in f.transcript.lines().enumerate() {
                let col = if i == 0 { Color32::from_rgb(126, 231, 135) } else if line.starts_with("Error code:") { Color32::from_rgb(255, 123, 114) } else { Color32::from_gray(210) };
                ui.add(egui::Label::new(egui::RichText::new(line).monospace().size(12.0).color(col)).wrap());
            }
        });
    });
}

/// A plain-words title and advice for the server's error.
/// Why a share couldn't be mounted, from the mount's own message.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum MountTrouble {
    /// Wrong user name or password, or the servers aren't let in.
    Refused,
    /// No such share or folder.
    NotFound,
    /// The address doesn't answer the servers.
    Unreachable,
    /// The share doesn't speak the protocol version asked for.
    Unsupported,
}

pub(crate) fn mount_trouble(e: &ApiError) -> Option<MountTrouble> {
    let all = format!("{} {}", e.code, e.details.join(" ")).to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| all.contains(w));
    if has(&["permission denied", "error(13)", "access denied", "logon failure", "nt_status_logon"]) {
        Some(MountTrouble::Refused)
    } else if has(&["no such file", "error(2)", "not found", "does not exist", "bad network name", "error(6)"]) {
        Some(MountTrouble::NotFound)
    } else if has(&["no route to host", "error(113)", "host is down", "error(112)", "timed out", "connection refused", "error(111)", "unreachable", "could not resolve", "name or service not known", "unable to find suitable address", "unable to detect an nfs service"]) {
        Some(MountTrouble::Unreachable)
    } else if has(&["operation not supported", "error(95)", "protocol not supported", "invalid argument", "error(22)", "requested nfs version"]) {
        Some(MountTrouble::Unsupported)
    } else {
        None
    }
}

fn explain(e: &ApiError, lib: &IsoLibrary) -> (String, String) {
    if e.code == "CONNECTION" {
        return ("Couldn't reach your XenServer".into(), e.message());
    }
    if e.code == "RBAC_PERMISSION_DENIED" {
        return ("Your account isn't allowed to add storage".into(), "Sign in with an account that has the Pool Admin role.".into());
    }
    let trouble = mount_trouble(e);
    if trouble == Some(MountTrouble::Refused) {
        let hint = if lib.nfs {
            "The NFS server refused your XenServer hosts: check that the export allows their addresses."
        } else if lib.username.is_empty() {
            "The share doesn't let guests in: type a user name and password."
        } else {
            "Check the user name and password, and that this user may read the share."
        };
        return ("The share refused the connection".into(), hint.into());
    }
    if trouble == Some(MountTrouble::NotFound) {
        return ("The share or folder wasn't found".into(), "Check the share's name and the folder after it.".into());
    }
    if trouble == Some(MountTrouble::Unreachable) {
        return (
            "Your XenServer hosts couldn't reach that address".into(),
            "Check the address, and that the share's server is on and reachable from the XenServer hosts themselves (not only from this computer).".into(),
        );
    }
    if trouble == Some(MountTrouble::Unsupported) {
        return if lib.nfs {
            (format!("The share doesn't speak NFS version {}", lib.nfs_version), "Try another NFS version under Advanced.".into())
        } else {
            (format!("The share doesn't speak SMB {}", lib.smb_version), "Very old servers only do SMB 1.0: you can choose it under Advanced.".into())
        };
    }
    ("Couldn't create the ISO library".into(), "Your XenServer couldn't use the share. Its answer is below.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(text: &str) -> ApiError {
        ApiError { code: "SR_BACKEND_FAILURE_222".into(), details: vec![String::new(), text.into(), String::new()] }
    }

    #[test]
    fn plain_words_for_mount_errors() {
        let smb = IsoLibrary { username: "joao".into(), smb_version: "3.0".into(), ..Default::default() };
        assert_eq!(explain(&err("mount error(13): Permission denied"), &smb).0, "The share refused the connection");
        assert_eq!(explain(&err("mount error(2): No such file or directory"), &smb).0, "The share or folder wasn't found");
        assert_eq!(explain(&err("mount error(113): No route to host"), &smb).0, "Your XenServer hosts couldn't reach that address");
        assert_eq!(explain(&err("mount error(95): Operation not supported"), &smb).0, "The share doesn't speak SMB 3.0");
        assert_eq!(explain(&err("something else"), &smb).0, "Couldn't create the ISO library");
    }

    #[test]
    fn transcript_shows_command_and_answer() {
        let lib = IsoLibrary { name: "ISO Library".into(), location: "//nas/isos".into(), username: "joao".into(), password: "x".into(), smb_version: "3.0".into(), shared: true, ..Default::default() };
        let mut d = IsoDialog::new(1, vec![(1, "home".into())]);
        d.failed(&lib, &err("mount error(13): Permission denied"));
        let t = &d.failure.as_ref().unwrap().transcript;
        assert!(t.starts_with("$ xe sr-create type=iso"));
        assert!(t.contains("Error code: SR_BACKEND_FAILURE_222\nmount error(13): Permission denied"));
        assert!(!t.contains("cifspassword=x"));
    }
}
