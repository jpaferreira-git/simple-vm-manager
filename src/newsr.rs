//! The New VM Storage dialog: a folder an NFS server shares, which every server of the pool mounts
//! for VMs' disks, so a VM moves between servers without its disks being copied. Browse asks the
//! server which folders it shares; creating it is the connection test, as for an ISO library, and
//! a failure is explained in plain words with the request (as the `xe` command) and the answer.
//! It can be made the pool's default storage as it's made.

use crate::app::{dialog_button, dialog_buttons, dialog_window, field_label, flat_button, text_field, Look, LABEL_WIDTH};
use crate::conn::ApiError;
use crate::isolib::{advanced, failure_panel, mount_trouble, note, segmented, Failure, MountTrouble, NFS_VERSIONS};
use crate::model::NfsStorage;
use crate::newvm::alert;
use crate::theme::Pal;
use eframe::egui::{self, pos2, vec2, Align2, FontId, Rect, Rounding, Sense};
use std::time::Duration;

pub enum StorageAction {
    None,
    Close,
    /// Create it in the pool with this id.
    Create(u32, NfsStorage),
    /// Ask the pool with this id which folders this server shares.
    Browse(u32, String),
}

/// A VM of the pool that the typed server is (or looks like): its name, its server's name, and
/// whether its address matched (rather than only its name).
pub struct Guest {
    pub vm: String,
    pub host: String,
    pub sure: bool,
}

/// The folders a server shares, as asked: the server, and its answer (None while it's asked).
struct Exports {
    server: String,
    answer: Option<Result<Vec<String>, String>>,
}

pub struct StorageDialog {
    pub sid: u32,
    /// The connected pools it could go in: (id, name, its default storage is already shared).
    pools: Vec<(u32, String, bool)>,
    name: String,
    description: String,
    location: String,
    nfs_version: usize,
    make_default: bool,
    /// The box was ticked or unticked by hand (then a change of pool leaves it as it is).
    default_chosen: bool,
    advanced: bool,
    busy: Option<String>,
    problem: String,
    failure: Option<Failure>,
    exports: Option<Exports>,
}

impl StorageDialog {
    pub fn new(sid: u32, pools: Vec<(u32, String, bool)>) -> StorageDialog {
        let shared = pools.iter().find(|p| p.0 == sid).map_or(false, |p| p.2);
        StorageDialog {
            sid,
            pools,
            name: "NFS storage".into(),
            description: String::new(),
            location: String::new(),
            nfs_version: 0,
            make_default: !shared,
            default_chosen: false,
            advanced: false,
            busy: None,
            problem: String::new(),
            failure: None,
            exports: None,
        }
    }

    /// For the screenshot checks: a location typed in, and the folders found there or not.
    pub fn filled(mut self, location: &str, found: Option<Vec<String>>) -> StorageDialog {
        self.location = location.into();
        if let Some(f) = found {
            self.exports = Some(Exports { server: self.server(), answer: Some(Ok(f)) });
        }
        self
    }

    /// The server in what's typed, so far.
    pub fn server(&self) -> String {
        NfsStorage::parts(&self.location).0
    }

    /// The server's answer to Browse (left alone if another server was asked since).
    pub fn exports_found(&mut self, server: &str, result: Result<Vec<String>, ApiError>) {
        if let Some(x) = self.exports.as_mut().filter(|x| x.server == server) {
            x.answer = Some(result.map_err(|e| why_no_folders(&e)));
        }
    }

    /// The server couldn't create it: say why, and show what was asked and what came back.
    pub fn failed(&mut self, st: &NfsStorage, e: &ApiError) {
        self.busy = None;
        let (title, hint) = explain(e, &st.nfs_version);
        let mut transcript = format!("$ {}\n", st.xe_command());
        transcript.push_str(&format!("Error code: {}\n", e.code));
        for d in e.details.iter().filter(|d| !d.trim().is_empty()) {
            transcript.push_str(d.trim());
            transcript.push('\n');
        }
        self.failure = Some(Failure { title, hint, transcript: transcript.trim_end().to_string(), copied: None });
    }

    /// Checks what was typed; if it looks right, the request to send (and the dialog waits).
    pub fn submit(&mut self) -> StorageAction {
        let st = self.request();
        if let Some(p) = st.location_problem() {
            self.problem = p.into();
            return StorageAction::None;
        }
        self.problem.clear();
        self.failure = None;
        let pool = self.pools.iter().find(|p| p.0 == self.sid).map(|p| p.1.clone()).unwrap_or_default();
        self.busy = Some(format!("Asking {}'s servers to mount {}…", pool, st.location));
        StorageAction::Create(self.sid, st)
    }

    fn request(&self) -> NfsStorage {
        let (server, path) = NfsStorage::parts(&self.location);
        let path = if path.len() > 1 { path.trim_end_matches('/') } else { path.as_str() };
        NfsStorage {
            name: self.name.trim().to_string(),
            description: self.description.trim().to_string(),
            location: format!("{}:{}", server, path),
            nfs_version: NFS_VERSIONS[self.nfs_version].0.into(),
            make_default: self.make_default,
        }
    }

    /// `guest`: the pool's own VM the typed server is, if it is one.
    pub fn ui(&mut self, ctx: &egui::Context, pal: &Pal, guest: Option<&Guest>) -> StorageAction {
        let mut action = StorageAction::None;
        let busy = self.busy.is_some();
        dialog_window(ctx, "New VM Storage").show(ctx, |ui| {
            ui.set_width(500.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            let intro = "A folder an NFS server shares, for your VMs' disks. Every server in the pool mounts it, so a VM moves between them without its disks being copied.";
            ui.add(egui::Label::new(egui::RichText::new(intro).color(pal.ink_soft)).wrap());
            ui.add_space(2.0);
            ui.add_enabled_ui(!busy, |ui| {
                if self.pools.len() > 1 {
                    ui.horizontal(|ui| {
                        field_label(ui, "Pool:", LABEL_WIDTH);
                        let current = self.pools.iter().find(|p| p.0 == self.sid).map(|p| p.1.clone()).unwrap_or_default();
                        let before = self.sid;
                        egui::ComboBox::from_id_salt("storage-pool").selected_text(current).width(ui.available_width() - 8.0).show_ui(ui, |ui| {
                            for (id, name, _) in &self.pools {
                                ui.selectable_value(&mut self.sid, *id, name);
                            }
                        });
                        if self.sid != before {
                            self.exports = None;
                            if !self.default_chosen {
                                self.make_default = !self.pools.iter().find(|p| p.0 == self.sid).map_or(false, |p| p.2);
                            }
                        }
                    });
                }
                ui.horizontal(|ui| {
                    field_label(ui, "Name:", LABEL_WIDTH);
                    text_field(ui, pal, &mut self.name, ui.available_width(), false);
                });
                ui.horizontal(|ui| {
                    field_label(ui, "Location:", LABEL_WIDTH);
                    let room = ui.available_width() - 92.0;
                    if text_field(ui, pal, &mut self.location, room, false).changed() {
                        self.problem.clear();
                    }
                    let server = self.server();
                    let asking = self.exports.as_ref().map_or(false, |x| x.answer.is_none());
                    let tip = "See the folders this server shares";
                    if flat_button(ui, "folder", "Browse", None, !server.is_empty() && !asking, false, pal).on_hover_text(tip).clicked() {
                        self.exports = Some(Exports { server: server.clone(), answer: None });
                        action = StorageAction::Browse(self.sid, server);
                    }
                });
                note(ui, pal, "For example 192.168.1.50:/mnt/tank/vms or nas.home.lan:/mnt/tank/vms. Or type only the server and Browse to pick the folder.");
                self.folders(ui, pal);

                ui.horizontal(|ui| {
                    ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x);
                    if ui.checkbox(&mut self.make_default, "Make it the pool's default storage").changed() {
                        self.default_chosen = true;
                    }
                });
                note(ui, pal, "Where new VMs' disks go, and what suspending and moving VMs use.");

                let summary = NFS_VERSIONS[self.nfs_version].1.to_string();
                advanced(ui, pal, &mut self.advanced, &summary);
                if self.advanced {
                    ui.horizontal(|ui| {
                        field_label(ui, "NFS version:", LABEL_WIDTH);
                        let names: Vec<&str> = NFS_VERSIONS.iter().map(|v| v.1).collect();
                        segmented(ui, pal, &names, &mut self.nfs_version, ui.available_width());
                    });
                    ui.horizontal(|ui| {
                        field_label(ui, "Description:", LABEL_WIDTH);
                        text_field(ui, pal, &mut self.description, ui.available_width(), false);
                    });
                }
            });

            if let Some(g) = guest {
                let server = self.server();
                let (is, title) = if g.sure { ("is", "This share is inside the pool.") } else { ("looks like", "This share may be inside the pool.") };
                let text = format!(
                    "{} {} '{}', a VM on {}. When {} restarts (for an update, say), every VM with a disk here stops working; and when the pool starts, this storage can't be mounted till '{}' is up. A separate computer is safer.",
                    server, is, g.vm, g.host, g.host, g.vm
                );
                alert(ui, pal, false, title, &text, None);
            }
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
                // while the servers are at it there's nothing to cancel: the window closes, it goes on
                if dialog_button(ui, pal, "close", if busy { "Close" } else { "Cancel" }, Look::Plain, true) {
                    action = StorageAction::Close;
                }
                let ready = !busy && !self.name.trim().is_empty() && !self.location.trim().is_empty();
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                if dialog_button(ui, pal, "plus", "Create", Look::Primary, ready) || (ready && enter && self.failure.is_none()) {
                    action = self.submit();
                }
            });
        });
        action
    }

    /// Under the location, after Browse: the server's folders to pick from (the one typed in
    /// marked), or that it's being asked, or why it couldn't say.
    fn folders(&mut self, ui: &mut egui::Ui, pal: &Pal) {
        let Some(x) = &self.exports else { return };
        let indent = LABEL_WIDTH + ui.spacing().item_spacing.x;
        match &x.answer {
            None => {
                ui.horizontal(|ui| {
                    ui.add_space(indent + 2.0);
                    ui.add(egui::Spinner::new().size(13.0));
                    ui.label(egui::RichText::new(format!("Asking {} which folders it shares…", x.server)).size(12.5).color(pal.ink_soft));
                });
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
            Some(Err(why)) => {
                ui.horizontal(|ui| {
                    ui.add_space(indent + 2.0);
                    let text = format!("Couldn't see {}'s folders: {}", x.server, why);
                    ui.add(egui::Label::new(egui::RichText::new(text).size(12.5).color(pal.red)).wrap());
                });
            }
            Some(Ok(paths)) if paths.is_empty() => {
                ui.horizontal(|ui| {
                    ui.add_space(indent + 2.0);
                    ui.label(egui::RichText::new(format!("{} doesn't share any folder with your servers.", x.server)).size(12.5).color(pal.ink_soft));
                });
            }
            Some(Ok(paths)) => {
                let (server, paths) = (x.server.clone(), paths.clone());
                let now = NfsStorage::parts(&self.location).1;
                let outer = egui::Margin { left: indent, ..Default::default() };
                egui::Frame::none().outer_margin(outer).stroke(egui::Stroke::new(1.0_f32, pal.border)).rounding(Rounding::same(6.0)).inner_margin(egui::Margin::same(3.0)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        // about four rows, then it scrolls
                        egui::ScrollArea::vertical().max_height(108.0).auto_shrink([false, true]).show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            for p in &paths {
                                let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click());
                                let on = *p == now;
                                if on || resp.hovered() {
                                    ui.painter().rect_filled(r, Rounding::same(4.0), if on { pal.nav_on } else { pal.hover });
                                }
                                crate::icons::paint_glyph(ui.painter(), Rect::from_center_size(pos2(r.min.x + 14.0, r.center().y), vec2(14.0, 14.0)), "folder", pal.ink_soft);
                                ui.painter().text(pos2(r.min.x + 30.0, r.center().y), Align2::LEFT_CENTER, p, FontId::proportional(13.0), pal.ink);
                                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                    self.location = format!("{}:{}", server, p);
                                    self.problem.clear();
                                }
                            }
                        });
                });
            }
        }
    }
}

/// Why Browse found no folders, in a few words.
fn why_no_folders(e: &ApiError) -> String {
    match mount_trouble(e) {
        Some(MountTrouble::Unreachable) => "your XenServer hosts couldn't reach it.".into(),
        Some(MountTrouble::Refused) => "it refused your XenServer hosts.".into(),
        _ if e.code == "CONNECTION" => "your XenServer didn't answer.".into(),
        _ => e.message(),
    }
}

/// A plain-words title and advice for the server's error.
fn explain(e: &ApiError, version: &str) -> (String, String) {
    if e.code == "CONNECTION" {
        return ("Couldn't reach your XenServer".into(), e.message());
    }
    if e.code == "RBAC_PERMISSION_DENIED" {
        return ("Your account isn't allowed to add storage".into(), "Sign in with an account that has the Pool Admin role.".into());
    }
    match mount_trouble(e) {
        Some(MountTrouble::Refused) => (
            "The NFS server refused your XenServer hosts".into(),
            "Check that the share lets their addresses in, with root access (no_root_squash; in TrueNAS, Maproot User root).".into(),
        ),
        Some(MountTrouble::NotFound) => ("The folder wasn't found".into(), "Check the folder after the colon: Browse lists the ones the server shares.".into()),
        Some(MountTrouble::Unreachable) => (
            "Your XenServer hosts couldn't reach that address".into(),
            "Check the address, and that the NFS server is on and reachable from the XenServer hosts themselves (not only from this computer).".into(),
        ),
        Some(MountTrouble::Unsupported) => (format!("The share doesn't speak NFS version {}", version), "Try another NFS version under Advanced.".into()),
        None => ("Couldn't create the storage".into(), "Your XenServer couldn't use the folder. Its answer is below.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(code: &str, text: &str) -> ApiError {
        ApiError { code: code.into(), details: vec![String::new(), text.into(), String::new()] }
    }

    #[test]
    fn plain_words_for_nfs_errors() {
        assert_eq!(explain(&err("SR_BACKEND_FAILURE_108", "Unable to detect an NFS service on this target."), "3").0, "Your XenServer hosts couldn't reach that address");
        assert_eq!(explain(&err("SR_BACKEND_FAILURE_73", "NFS mount error [opterr=mount failed with return code 32] mount.nfs: access denied by server while mounting nas:/x"), "3").0, "The NFS server refused your XenServer hosts");
        assert_eq!(explain(&err("SR_BACKEND_FAILURE_73", "mount.nfs: mounting nas:/nope failed, reason given by server: No such file or directory"), "3").0, "The folder wasn't found");
        assert_eq!(explain(&err("SR_BACKEND_FAILURE_73", "mount.nfs: requested NFS version or transport protocol is not supported"), "4.1").0, "The share doesn't speak NFS version 4.1");
    }

    #[test]
    fn what_is_sent() {
        let mut d = StorageDialog::new(1, vec![(1, "home".into(), false)]).filled(" nas:/mnt/tank/vms/ ", None);
        assert!(d.make_default, "a pool without shared default storage gets it ticked");
        let StorageAction::Create(sid, st) = d.submit() else { panic!("not sent") };
        assert_eq!(sid, 1);
        assert_eq!(st.location, "nas:/mnt/tank/vms");
        assert_eq!(st.device_config(), vec![("server".to_string(), "nas".to_string()), ("serverpath".to_string(), "/mnt/tank/vms".to_string()), ("nfsversion".to_string(), "3".to_string())]);
        assert_eq!(st.xe_command(), "xe sr-create type=nfs content-type=user shared=true name-label=\"NFS storage\" device-config:server=nas device-config:serverpath=/mnt/tank/vms device-config:nfsversion=3");
        let mut d = StorageDialog::new(1, vec![(1, "home".into(), true)]).filled("nas", None);
        assert!(!d.make_default, "a pool with shared default storage keeps it");
        assert!(matches!(d.submit(), StorageAction::None));
        assert!(!d.problem.is_empty());
    }
}
