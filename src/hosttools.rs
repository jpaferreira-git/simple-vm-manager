//! A server's tools, from the "…" menu on its page (and a pool's): Back Up and Restore From
//! Backup (its own system, in a .xbk file), the Server Status Report (its logs and settings, as
//! XenServer's support asks for them) and Change Root Password (the pool's).

use crate::app::{dialog_button, dialog_buttons, dialog_window, field_label, text_field, Look};
use crate::model::*;
use crate::theme::Pal;
use crate::xva::{downloads, expand, free_space, tilde};
use eframe::egui;
use std::path::{Path, PathBuf};

const LABEL: f32 = 112.0;

/// What a tool's dialog asks for.
pub enum ToolAction {
    None,
    Close,
    /// The system's file (or folder) chooser.
    Browse,
    Go,
}

/// The date and time a file's named with: "2026-11-20 1843" (this computer's time).
pub fn stamp(now: i64) -> String {
    let t = now + local_offset(now);
    let (y, m, d) = civil_from_days(t.div_euclid(86_400));
    let s = t.rem_euclid(86_400);
    format!("{:04}-{:02}-{:02} {:02}{:02}", y, m, d, s / 3600, s / 60 % 60)
}

/// A name a file system takes: what it won't replaced.
pub fn file_safe(name: &str) -> String {
    let clean: String = name.chars().map(|c| if "/\\:*?\"<>|".contains(c) || c.is_control() { '-' } else { c }).collect();
    let clean = clean.trim().trim_matches('.').trim().to_string();
    if clean.is_empty() {
        "server".into()
    } else {
        clean
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn note(ui: &mut egui::Ui, pal: &Pal, text: &str) {
    ui.add(egui::Label::new(egui::RichText::new(text).color(pal.ink_soft)).wrap());
}

fn indented(ui: &mut egui::Ui, pal: &Pal, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(LABEL + ui.spacing().item_spacing.x);
        ui.add(egui::Label::new(egui::RichText::new(text).size(12.0).color(pal.ink_soft)).wrap());
    });
}

/// Back Up a server (`restore` false): where its file goes; or Restore From Backup: which file.
pub struct BackupDialog {
    pub sid: u32,
    pub host: String,
    pub name: String,
    pub address: String,
    pub restore: bool,
    pub path: String,
    /// The room where it goes, as last looked at (by folder), and the file's size (restore).
    room: Option<(PathBuf, Option<u64>)>,
}

impl BackupDialog {
    pub fn new(sid: u32, host: String, name: String, address: String, restore: bool) -> BackupDialog {
        let path = if restore { String::new() } else { tilde(&downloads().join(format!("{} backup {}.xbk", file_safe(&name), stamp(now())))) };
        BackupDialog { sid, host, name, address, restore, path, room: None }
    }

    /// The path chosen in the system's dialog.
    pub fn chose(&mut self, p: PathBuf) {
        let p = if self.restore || p.extension().map_or(false, |e| e.eq_ignore_ascii_case("xbk")) { p } else { PathBuf::from(format!("{}.xbk", p.display())) };
        self.path = tilde(&p);
    }

    /// The file (".xbk" added to a new one when missing), or why it can't be used.
    pub fn target(&self) -> Result<PathBuf, String> {
        let raw = self.path.trim();
        if raw.is_empty() {
            return Err(String::new());
        }
        let p = expand(raw);
        if self.restore {
            return match std::fs::metadata(&p) {
                Ok(m) if m.is_file() && m.len() > 0 => Ok(p),
                Ok(m) if m.is_dir() => Err("That's a folder: choose the backup file in it.".into()),
                Ok(_) => Err("That file is empty.".into()),
                Err(_) => Err("There's no file there.".into()),
            };
        }
        let p = if p.extension().map_or(false, |e| e.eq_ignore_ascii_case("xbk")) { p } else { PathBuf::from(format!("{}.xbk", p.display())) };
        if p.is_dir() {
            return Err("That's a folder: add the file's name.".into());
        }
        match p.parent().filter(|d| !d.as_os_str().is_empty()) {
            Some(d) if d.is_dir() => Ok(p),
            Some(_) => Err("That folder doesn't exist.".into()),
            None => Err("Type the whole path, or use Browse.".into()),
        }
    }

    /// The folder the system's dialog opens in, and the name it suggests.
    pub fn browse_start(&self) -> (PathBuf, String) {
        let p = expand(self.path.trim());
        let dir = p.parent().filter(|d| d.is_dir()).map(Path::to_path_buf).unwrap_or_else(downloads);
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        (dir, name)
    }

    pub fn ui(&mut self, ctx: &egui::Context, pal: &Pal) -> ToolAction {
        let mut action = ToolAction::None;
        let target = self.target();
        if let Ok(p) = &target {
            let dir = if self.restore { p.clone() } else { p.parent().map(Path::to_path_buf).unwrap_or_default() };
            if self.room.as_ref().map_or(true, |(d, _)| *d != dir) {
                let size = if self.restore { std::fs::metadata(&dir).ok().map(|m| m.len()) } else { free_space(&dir) };
                self.room = Some((dir.clone(), size));
            }
        }
        let title = if self.restore { "Restore From Backup" } else { "Back Up Server" };
        dialog_window(ctx, title).id(egui::Id::new("backup-dialog")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            if self.restore {
                note(ui, pal, &format!("Puts a backup of '{}' (made with Back Up…) on its backup partition. To finish, start {} from the XenServer installer (a USB stick or its ISO) and choose to restore from the backup: it's then as it was when backed up. Its VMs and their disks aren't touched.", self.name, self.name));
            } else {
                note(ui, pal, &format!("Saves the system of '{}' in one file on this computer: XenServer as installed there, with its settings and the pool's database as it has it. Not its VMs or their disks (export those, or keep them on shared storage). Restore From Backup puts it back.", self.name));
            }
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                field_label(ui, if self.restore { "Backup file:" } else { "Save as:" }, LABEL);
                let w = ui.available_width() - 108.0;
                text_field(ui, pal, &mut self.path, w, false);
                if dialog_button(ui, pal, "folder", "Browse…", Look::Plain, true) {
                    action = ToolAction::Browse;
                }
            });
            match (&self.room, self.restore, &target) {
                (Some((_, Some(n))), true, Ok(_)) => indented(ui, pal, &format!("{} to send", fmt_bytes(*n as i64))),
                (Some((_, Some(n))), false, Ok(_)) => indented(ui, pal, &format!("Often a GB or two, made as it's sent: a few minutes  ·  {} free there", fmt_bytes(*n as i64))),
                _ => {}
            }
            match &target {
                Err(why) if !why.is_empty() => {
                    ui.add(egui::Label::new(egui::RichText::new(why.as_str()).size(12.5).color(pal.red)).wrap());
                }
                Ok(p) if !self.restore && p.exists() => {
                    crate::newvm::alert(ui, pal, false, "There's a file by that name.", "Backing up replaces it.", None);
                }
                _ => {}
            }
            if self.restore {
                crate::newvm::alert(ui, pal, false, "It replaces what's on its backup partition.", "That's where the installer keeps the system from before an upgrade, to go back to it: that copy goes.", None);
            }
            ui.add_space(4.0);
            dialog_buttons(ui, |ui| {
                if dialog_button(ui, pal, "close", "Cancel", Look::Plain, true) {
                    action = ToolAction::Close;
                }
                let ready = target.is_ok();
                let (glyph, label, look) = if self.restore { ("upload", "Send to Server", Look::Danger) } else { ("archive", "Back Up", Look::Primary) };
                if dialog_button(ui, pal, glyph, label, look, ready) {
                    action = ToolAction::Go;
                }
            });
        });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            action = ToolAction::Close;
        }
        action
    }
}

/// A server of the Server Status Report's: its reference, name, address, whether it's ticked,
/// and whether it's up (an off one can't make one).
pub struct ReportHost {
    pub host: String,
    pub name: String,
    pub address: String,
    pub ticked: bool,
    pub up: bool,
}

/// The Server Status Report: whose, and the folder their files go in.
pub struct ReportDialog {
    pub sid: u32,
    pub hosts: Vec<ReportHost>,
    pub folder: String,
}

impl ReportDialog {
    pub fn new(sid: u32, hosts: Vec<ReportHost>) -> ReportDialog {
        ReportDialog { sid, hosts, folder: tilde(&downloads()) }
    }

    /// The folder chosen in the system's dialog.
    pub fn chose(&mut self, p: PathBuf) {
        self.folder = tilde(&p);
    }

    pub fn browse_start(&self) -> PathBuf {
        let p = expand(self.folder.trim());
        if p.is_dir() {
            p
        } else {
            downloads()
        }
    }

    /// The folder, or why it can't be used.
    pub fn target(&self) -> Result<PathBuf, String> {
        let p = expand(self.folder.trim());
        if self.folder.trim().is_empty() {
            Err(String::new())
        } else if p.is_dir() {
            Ok(p)
        } else {
            Err("That folder doesn't exist.".into())
        }
    }

    /// Each ticked server's file: (server, name, address, file).
    pub fn files(&self, folder: &Path) -> Vec<(String, String, String, PathBuf)> {
        let at = stamp(now());
        self.hosts.iter().filter(|h| h.ticked && h.up).map(|h| (h.host.clone(), h.name.clone(), h.address.clone(), folder.join(format!("{} status report {}.tar.bz2", file_safe(&h.name), at)))).collect()
    }

    pub fn ui(&mut self, ctx: &egui::Context, pal: &Pal) -> ToolAction {
        let mut action = ToolAction::None;
        let target = self.target();
        dialog_window(ctx, "Server Status Report").id(egui::Id::new("report-dialog")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            note(ui, pal, "Gathers each server's logs and settings into a file: what XenServer's support asks for, or to look into a problem yourself (a snapshot schedule that didn't run, say). A server takes a few minutes to make its own.");
            ui.add_space(2.0);
            ui.horizontal_top(|ui| {
                field_label(ui, "Servers:", LABEL);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for h in self.hosts.iter_mut() {
                        ui.add_enabled_ui(h.up, |ui| {
                            let label = if h.up { h.name.clone() } else { format!("{}  (off, or not answering)", h.name) };
                            ui.checkbox(&mut h.ticked, label);
                        });
                    }
                });
            });
            ui.horizontal(|ui| {
                field_label(ui, "Save in:", LABEL);
                let w = ui.available_width() - 108.0;
                text_field(ui, pal, &mut self.folder, w, false);
                if dialog_button(ui, pal, "folder", "Browse…", Look::Plain, true) {
                    action = ToolAction::Browse;
                }
            });
            indented(ui, pal, "One file a server, \"<server> status report <date>.tar.bz2\"");
            if let Err(why) = &target {
                if !why.is_empty() {
                    ui.add(egui::Label::new(egui::RichText::new(why.as_str()).size(12.5).color(pal.red)).wrap());
                }
            }
            crate::newvm::alert(ui, pal, false, "They hold names, addresses and the like.", "Look at what's in them before you send them to anyone.", None);
            ui.add_space(4.0);
            dialog_buttons(ui, |ui| {
                if dialog_button(ui, pal, "close", "Cancel", Look::Plain, true) {
                    action = ToolAction::Close;
                }
                let any = self.hosts.iter().any(|h| h.ticked && h.up);
                if dialog_button(ui, pal, "report", "Make Report", Look::Primary, any && target.is_ok()) {
                    action = ToolAction::Go;
                }
            });
        });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            action = ToolAction::Close;
        }
        action
    }
}

/// Change Root Password, for a pool (every server's): the one now, the new one twice.
pub struct PasswordDialog {
    pub sid: u32,
    pub pool: String,
    pub user: String,
    pub old: String,
    pub new: String,
    pub again: String,
    pub busy: bool,
    pub error: Option<String>,
    focus: bool,
}

impl PasswordDialog {
    pub fn new(sid: u32, pool: String, user: String) -> PasswordDialog {
        PasswordDialog { sid, pool, user, old: String::new(), new: String::new(), again: String::new(), busy: false, error: None, focus: true }
    }

    /// Why it can't be changed to what's typed, if it can't ("" while something's still empty).
    pub fn problem(&self) -> Option<String> {
        if self.old.is_empty() || self.new.is_empty() || self.again.is_empty() {
            Some(String::new())
        } else if self.new != self.again {
            Some("The new password isn't the same both times.".into())
        } else if self.new == self.old {
            Some("That's the password it has now.".into())
        } else if self.new.chars().any(char::is_control) {
            Some("A password can't hold that character.".into())
        } else {
            None
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, pal: &Pal) -> ToolAction {
        let mut action = ToolAction::None;
        dialog_window(ctx, "Change Root Password").id(egui::Id::new("password-dialog")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            note(ui, pal, &format!("Changes the password of '{}' on every server of '{}': the one this app, XenCenter and the servers' own screens log in with.", self.user, self.pool));
            ui.add_space(2.0);
            let w = 240.0;
            ui.add_enabled_ui(!self.busy, |ui| {
                for (label, value) in [("Current:", &mut self.old), ("New password:", &mut self.new), ("New, again:", &mut self.again)] {
                    ui.horizontal(|ui| {
                        field_label(ui, label, LABEL);
                        let r = text_field(ui, pal, value, w, true);
                        if self.focus && label == "Current:" {
                            r.request_focus();
                            self.focus = false;
                        }
                    });
                }
            });
            if !self.new.is_empty() && self.new.chars().count() < 8 {
                indented(ui, pal, "Short: a longer one (8 or more) is harder to guess.");
            }
            indented(ui, pal, "This app goes on with the new one (and keeps it in the keychain, when it's saved there). Elsewhere it has to be typed again.");
            let problem = self.problem();
            match (&self.error, &problem) {
                (Some(e), _) => {
                    ui.add(egui::Label::new(egui::RichText::new(e.as_str()).size(12.5).color(pal.red)).wrap());
                }
                (None, Some(p)) if !p.is_empty() => {
                    ui.add(egui::Label::new(egui::RichText::new(p.as_str()).size(12.5).color(pal.red)).wrap());
                }
                _ => {}
            }
            ui.add_space(4.0);
            dialog_buttons(ui, |ui| {
                if dialog_button(ui, pal, "close", "Cancel", Look::Plain, !self.busy) {
                    action = ToolAction::Close;
                }
                let ready = problem.is_none() && !self.busy;
                if dialog_button(ui, pal, "key", if self.busy { "Changing…" } else { "Change Password" }, Look::Primary, ready) || (ready && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                    action = ToolAction::Go;
                }
            });
        });
        if !self.busy && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            action = ToolAction::Close;
        }
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_and_stamps() {
        assert_eq!(file_safe("godofredo"), "godofredo");
        assert_eq!(file_safe(" a/b: c "), "a-b- c");
        assert_eq!(file_safe(".."), "server");
        assert_eq!(stamp(0 - local_offset(0)).len(), "1970-01-01 0000".len());
    }

    #[test]
    fn password_checks() {
        let mut d = PasswordDialog::new(1, "home".into(), "root".into());
        assert_eq!(d.problem(), Some(String::new()));
        (d.old, d.new, d.again) = ("a".into(), "b".into(), "c".into());
        assert!(d.problem().unwrap().contains("same both"));
        d.again = "b".into();
        assert_eq!(d.problem(), None);
        d.new = "a".into();
        d.again = "a".into();
        assert!(d.problem().unwrap().contains("has now"));
    }
}
