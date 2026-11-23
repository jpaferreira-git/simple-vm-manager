//! A server's and a pool's tools, in the "…" menu of their tab row: Restart Toolstack, Back Up,
//! Restore From Backup and Server Status Report on a server; Change Root Password and the status
//! report on a pool. And Renew, on an alert that a server's certificate is running out.

use super::*;
use crate::hosttools::{BackupDialog, PasswordDialog, ReportDialog, ReportHost, ToolAction};

/// What's done once a tool's job ends, besides its log line.
pub(super) enum ToolThen {
    Nothing,
    /// The pool's read again.
    Refresh,
    /// The pool's alert about it dismissed (Renew).
    Dismiss(Option<String>),
    /// The new password kept (in the keychain too, when it's saved there).
    Password(String),
    /// What to do next, said (Restore From Backup): the server's name.
    Restored(String),
}

/// A tool's menu entry: its glyph, its label, what it does, whether it can now, why not.
pub(super) type Entry = (&'static str, &'static str, Intent, bool, &'static str);

impl App {
    pub(super) fn host_entries(&self, sid: u32, host: &str, on: bool) -> Vec<Entry> {
        let free = |k: &str| on && !self.tool_busy.contains(&format!("{}:{}", k, host));
        vec![
            ("rescan", "Restart Toolstack…", Intent::AskToolstack(sid, host.to_string()), free("toolstack"), "It's off, or it's being done"),
            ("archive", "Back Up…", Intent::BackupHost(sid, host.to_string(), false), free("backup"), "It's off, or it's being done"),
            ("upload", "Restore From Backup…", Intent::BackupHost(sid, host.to_string(), true), free("restore"), "It's off, or it's being done"),
            ("report", "Server Status Report…", Intent::StatusReport(sid, Some(host.to_string())), free("report"), "It's off, or it's being done"),
        ]
    }

    pub(super) fn pool_entries(&self, sid: u32, on: bool) -> Vec<Entry> {
        let busy = self.tool_busy.contains(&format!("password:{}", sid));
        vec![
            ("key", "Change Root Password…", Intent::ChangePassword(sid), on && !busy, "Not connected, or it's being done"),
            ("report", "Server Status Report…", Intent::StatusReport(sid, None), on, "Not connected"),
        ]
    }

    /// Opens a tool's dialog (or its question).
    pub(super) fn open_tool(&mut self, intent: Intent) {
        let host_of = |s: &ServerEntry, h: &str| s.data.as_ref().and_then(|d| d.host(h)).map(|x| (x.name.clone(), x.address.clone(), x.is_master, d_is_pool(s)));
        match intent {
            Intent::AskToolstack(sid, host) => {
                let Some((name, _, coordinator, _)) = self.server(sid).and_then(|s| host_of(s, &host)) else { return };
                self.dialog = Dialog::ConfirmToolstack { sid, host, name, coordinator };
            }
            Intent::AskRenewCert(sid, host, internal, msg) => {
                let Some((name, ..)) = self.server(sid).and_then(|s| host_of(s, &host)) else { return };
                self.dialog = Dialog::ConfirmRenewCert { sid, host, name, internal, msg };
            }
            Intent::BackupHost(sid, host, restore) => {
                let Some((name, address, ..)) = self.server(sid).and_then(|s| host_of(s, &host)) else { return };
                self.dialog = Dialog::Backup(Box::new(BackupDialog::new(sid, host, name, address, restore)));
            }
            Intent::StatusReport(sid, only) => {
                let Some(d) = self.server(sid).and_then(|s| s.data.as_ref()) else { return };
                let hosts = d
                    .hosts
                    .iter()
                    .map(|h| ReportHost { host: h.rref.clone(), name: h.name.clone(), address: h.address.clone(), ticked: only.as_deref().map_or(true, |o| o == h.rref), up: h.is_up() })
                    .collect();
                self.dialog = Dialog::Report(Box::new(ReportDialog::new(sid, hosts)));
            }
            Intent::ChangePassword(sid) => {
                let Some(s) = self.server(sid) else { return };
                self.dialog = Dialog::Password(Box::new(PasswordDialog::new(sid, s.name(), s.conn.username())));
            }
            _ => {}
        }
    }

    /// Draws a tool's dialog when one is open: what it asks for.
    pub(super) fn tool_dialog(&mut self, ctx: &egui::Context) -> ToolAction {
        let pal = self.pal;
        match &mut self.dialog {
            Dialog::Backup(d) => d.ui(ctx, &pal),
            Dialog::Report(d) => d.ui(ctx, &pal),
            Dialog::Password(d) => d.ui(ctx, &pal),
            Dialog::ConfirmToolstack { name, coordinator, .. } => {
                let mut action = ToolAction::None;
                dialog_window(ctx, "Restart Toolstack").show(ctx, |ui| {
                    ui.set_width(440.0);
                    let pool = if *coordinator { " Nor does the pool, through it, as it's the coordinator: this app connects again by itself." } else { "" };
                    let text = format!(
                        "Restart the toolstack of '{}'?\n\nIts management (xapi) starts again: its VMs keep running, but it doesn't answer for a minute or so.{}\n\nIt helps when the server misbehaves: tasks that never end, VMs that won't start or stop.",
                        name, pool
                    );
                    ui.add(egui::Label::new(text).wrap());
                    ui.add_space(12.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            action = ToolAction::Close;
                        }
                        if dialog_button(ui, &pal, "rescan", "Restart Toolstack", Look::Danger, true) {
                            action = ToolAction::Go;
                        }
                    });
                });
                action
            }
            Dialog::ConfirmRenewCert { name, internal, .. } => {
                let mut action = ToolAction::None;
                dialog_window(ctx, "Renew Certificate").show(ctx, |ui| {
                    ui.set_width(440.0);
                    let text = if *internal {
                        format!("Give '{}' a new certificate for talking to the pool's other servers?\n\nIt makes a new one itself (self-signed) and the pool takes it; nothing else changes.", name)
                    } else {
                        format!("Give '{}' a new certificate for HTTPS?\n\nIt makes a new one itself (self-signed), and its HTTPS starts again with it for a moment. This app takes it as it is; XenCenter asks once to trust it.", name)
                    };
                    ui.add(egui::Label::new(text).wrap());
                    ui.add_space(12.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            action = ToolAction::Close;
                        }
                        if dialog_button(ui, &pal, "shield", "Renew", Look::Primary, true) {
                            action = ToolAction::Go;
                        }
                    });
                });
                action
            }
            _ => ToolAction::None,
        }
    }

    /// The system's file (or folder) chooser for the open tool's dialog.
    pub(super) fn tool_browse(&mut self, ctx: &egui::Context) {
        enum Pick {
            Save(PathBuf, String),
            Open(PathBuf),
            Folder(PathBuf),
        }
        let pick = match &self.dialog {
            Dialog::Backup(d) if d.restore => Pick::Open(d.browse_start().0),
            Dialog::Backup(d) => {
                let (dir, name) = d.browse_start();
                Pick::Save(dir, name)
            }
            Dialog::Report(d) => Pick::Folder(d.browse_start()),
            _ => return,
        };
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let filter = ("XenServer backup (.xbk)", &["xbk", "XBK"][..]);
            let path = match pick {
                Pick::Save(dir, name) => pollster::block_on(rfd::AsyncFileDialog::new().set_title("Back up to").add_filter(filter.0, filter.1).set_directory(dir).set_file_name(name).save_file()),
                Pick::Open(dir) => pollster::block_on(rfd::AsyncFileDialog::new().set_title("The backup to restore").add_filter(filter.0, filter.1).add_filter("Any file", &["*"]).set_directory(dir).pick_file()),
                Pick::Folder(dir) => pollster::block_on(rfd::AsyncFileDialog::new().set_title("Save the reports in").set_directory(dir).pick_folder()),
            }
            .map(|f| f.path().to_path_buf());
            let _ = tx.send(Msg::ToolPicked(path));
            ctx.request_repaint();
        });
    }

    /// The path the system's chooser gave, into the open tool's dialog.
    pub(super) fn tool_picked(&mut self, path: Option<PathBuf>) {
        let Some(p) = path else { return };
        match &mut self.dialog {
            Dialog::Backup(d) => d.chose(p),
            Dialog::Report(d) => d.chose(p),
            _ => {}
        }
    }

    /// Does what the open tool's dialog asks.
    pub(super) fn tool_go(&mut self, ctx: &egui::Context) {
        match std::mem::replace(&mut self.dialog, Dialog::None) {
            Dialog::ConfirmToolstack { sid, host, name, .. } => {
                let h = host.clone();
                let doing = format!("Restarting the toolstack of '{}'", name);
                let failed = format!("Couldn't restart the toolstack of '{}'", name);
                let done = format!("Restarted the toolstack of '{}'", name);
                self.tool_job(sid, format!("toolstack:{}", host), (Some(host), name), doing, failed, None, ToolThen::Refresh, move |c, _| c.restart_toolstack(&h).map(|_| done), ctx);
            }
            Dialog::ConfirmRenewCert { sid, host, name, internal, msg } => {
                let h = host.clone();
                let which = if internal { "certificate for the pool" } else { "HTTPS certificate" };
                let doing = format!("Renewing the {} of '{}'", which, name);
                let failed = format!("Couldn't renew the {} of '{}'", which, name);
                let done = format!("Renewed the {} of '{}'", which, name);
                let work = move |c: &dyn Connection, _: &Arc<Transfer>| {
                    c.renew_certificate(&h, internal).map(|till| match till {
                        Some(t) => format!("{}: good till {}", done, fmt_date(t)),
                        None => done,
                    })
                };
                self.tool_job(sid, format!("cert:{}", host), (Some(host), name), doing, failed, None, ToolThen::Dismiss(msg), work, ctx);
            }
            Dialog::Backup(d) => {
                let Ok(path) = d.target() else {
                    self.dialog = Dialog::Backup(d);
                    return;
                };
                let file = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let address = d.address.clone();
                if d.restore {
                    let doing = format!("Sending {} to '{}' to restore it", file, d.name);
                    let failed = format!("Couldn't send {} to '{}'", file, d.name);
                    let done = format!("Sent {} to '{}': it's on its backup partition, to restore from its installer", file, d.name);
                    let stream = Some((false, format!("Sending {} stops; '{}' keeps what was on its backup partition, or part of the file.", file, d.name)));
                    self.tool_job(d.sid, format!("restore:{}", d.host), (Some(d.host.clone()), d.name.clone()), doing, failed, stream, ToolThen::Restored(d.name.clone()), move |c, t| c.restore_host(&address, &path, t).map(|_| done), ctx);
                } else {
                    let doing = format!("Backing up '{}' to {}", d.name, file);
                    let failed = format!("Couldn't back up '{}'", d.name);
                    let folder = path.parent().map(short_home).unwrap_or_default();
                    let done = format!("Backed up '{}' to {} in {}", d.name, file, folder);
                    let stream = Some((true, format!("The file {} isn't finished: what's been saved is deleted.", file)));
                    let work = move |c: &dyn Connection, t: &Arc<Transfer>| c.backup_host(&address, &path, t).map(|n| format!("{} ({})", done, fmt_bytes(n as i64)));
                    self.tool_job(d.sid, format!("backup:{}", d.host), (Some(d.host.clone()), d.name.clone()), doing, failed, stream, ToolThen::Nothing, work, ctx);
                }
            }
            Dialog::Report(d) => {
                let Ok(folder) = d.target() else {
                    self.dialog = Dialog::Report(d);
                    return;
                };
                let at = short_home(&folder);
                for (host, name, address, path) in d.files(&folder) {
                    let file = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let doing = format!("Making the status report of '{}'", name);
                    let failed = format!("Couldn't make the status report of '{}'", name);
                    let done = format!("Made the status report of '{}': {} in {}", name, file, at);
                    let stream = Some((true, format!("The file {} isn't finished: what's been saved is deleted.", file)));
                    let h = host.clone();
                    let work = move |c: &dyn Connection, t: &Arc<Transfer>| {
                        // what's in it: what the server puts in one by default
                        let parts: Vec<String> = c.report_parts(&h)?.into_iter().filter(|p| p.default).map(|p| p.key).collect();
                        if parts.is_empty() {
                            return Err(ApiError::other("The server said nothing could go in it."));
                        }
                        c.status_report(&address, &parts, &path, t).map(|n| format!("{} ({})", done, fmt_bytes(n as i64)))
                    };
                    self.tool_job(d.sid, format!("report:{}", host), (Some(host), name), doing, failed, stream, ToolThen::Nothing, work, ctx);
                }
            }
            Dialog::Password(mut d) => {
                if d.problem().is_some() {
                    self.dialog = Dialog::Password(d);
                    return;
                }
                d.busy = true;
                d.error = None;
                let (sid, old, new) = (d.sid, d.old.clone(), d.new.clone());
                let pool = d.pool.clone();
                self.dialog = Dialog::Password(d);
                let kept = new.clone();
                let done = format!("Changed the root password of '{}'", pool);
                let doing = format!("Changing the root password of '{}'", pool);
                let failed = format!("Couldn't change the root password of '{}'", pool);
                self.tool_job(sid, format!("password:{}", sid), (None, pool), doing, failed, None, ToolThen::Password(kept), move |c, _| c.change_password(&old, &new).map(|_| done), ctx);
            }
            other => self.dialog = other,
        }
    }

    /// Starts a tool's job in the background (one at a time per `key`), logged and under
    /// Activities as `doing`; `about`: the server it's about (reference, name), else the pool.
    /// `stream`: a file's sent or received (true), with what closing the app would leave.
    #[allow(clippy::too_many_arguments)]
    fn tool_job(
        &mut self,
        sid: u32,
        key: String,
        about: (Option<String>, String),
        doing: String,
        failed: String,
        stream: Option<(bool, String)>,
        then: ToolThen,
        work: impl FnOnce(&dyn Connection, &Arc<Transfer>) -> Result<String, ApiError> + Send + 'static,
        ctx: &egui::Context,
    ) {
        let Some(s) = self.server(sid) else { return };
        if self.tool_busy.contains(&key) {
            return;
        }
        let (conn, tx, ctx, server) = (s.conn.clone(), self.tx.clone(), ctx.clone(), s.name());
        let log = self.add_log(sid, &server, about.0.as_deref(), &about.1, &doing, "In progress");
        let prog = self.progress(log);
        let t = Arc::new(Transfer::default());
        if let Some((down, left)) = stream {
            self.if_closed(log, left);
            self.streams.insert(log, (t.clone(), Instant::now(), down));
        }
        self.tool_busy.insert(key.clone());
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = work(conn.as_ref(), &t);
            let _ = tx.send(Msg::ToolDone { sid, key, log, failed, then, result });
            ctx.request_repaint();
        });
    }

    /// A tool's job ended: its log line, and what follows from it.
    pub(super) fn tool_done(&mut self, sid: u32, key: String, log: usize, failed: String, then: ToolThen, result: Result<String, ApiError>, ctx: &egui::Context) {
        self.tool_busy.remove(&key);
        self.streams.remove(&log);
        match result {
            Ok(text) => {
                let took = fmt_took(self.log.get(log).map(|e| e.started.elapsed()).unwrap_or_default());
                self.end_log(log, "Done", Some(format!("{} (in {})", text, took)));
                match then {
                    ToolThen::Nothing => {}
                    ToolThen::Refresh => self.refresh_soon(sid),
                    ToolThen::Dismiss(msg) => {
                        if let Some(m) = msg {
                            self.dismiss_alerts(vec![(sid, m)], ctx);
                        }
                        self.refresh_soon(sid);
                    }
                    ToolThen::Password(new) => {
                        self.keep_new_password(sid, new);
                        if matches!(self.dialog, Dialog::Password(_)) {
                            self.dialog = Dialog::None;
                        }
                    }
                    ToolThen::Restored(name) => {
                        if matches!(self.dialog, Dialog::None) {
                            let text = format!("The backup is on the backup partition of '{}'. To finish:\n\n1. Start {} from the XenServer installer (a USB stick or its ISO, the same version).\n2. Choose to restore from the backup when it offers to.\n3. Once it starts again, it's as it was when backed up.", name, name);
                            self.dialog = Dialog::Message { title: "Restore From Backup".into(), text };
                        }
                    }
                }
            }
            Err(e) if e.code == CANCELLED => {
                self.end_log(log, "Cancelled", Some(format!("{}: stopped", failed.replacen("Couldn't ", "Didn't ", 1))));
            }
            Err(e) => {
                let why = tool_problem(&e);
                self.end_log(log, "Failed", Some(format!("{}: {}", failed, why)));
                match &mut self.dialog {
                    Dialog::Password(d) if matches!(then, ToolThen::Password(_)) => {
                        d.busy = false;
                        d.error = Some(why);
                    }
                    Dialog::None if self.shot_path.is_none() => self.dialog = Dialog::Message { title: "Couldn't do it".into(), text: format!("{}.\n\n{}", failed, why) },
                    _ => {}
                }
            }
        }
    }

    /// The pool's new root password, in the keychain where the old one was kept.
    fn keep_new_password(&mut self, sid: u32, new: String) {
        let Some(s) = self.server(sid) else { return };
        let (host, user) = (s.conn.hostname(), s.conn.username());
        // (kept under the address it was typed as; a pool's other servers' too)
        let mut at: Vec<String> = vec![host];
        if let Some(d) = &s.data {
            at.extend(d.hosts.iter().flat_map(|h| [h.address.clone(), h.name.clone(), h.hostname.clone()]));
        }
        let keys: Vec<(String, String)> = self.saved.keys().filter(|(h, u)| *u == user && at.iter().any(|a| !a.is_empty() && a.eq_ignore_ascii_case(h))).cloned().collect();
        for (h, u) in keys {
            self.keep_password(&h, &u, Some(new.clone()));
        }
    }
}

/// Whether a server entry's pool has more than one server.
fn d_is_pool(s: &ServerEntry) -> bool {
    s.data.as_ref().map_or(false, |d| d.is_pool())
}

/// Why a tool's job failed, in plain words where the server's answer is a known one.
fn tool_problem(e: &ApiError) -> String {
    match e.code.as_str() {
        "WRONG_PASSWORD" => "That isn't the password it has now.".into(),
        "CHANGE_PASSWORD_REJECTED" => format!("The server didn't take the new one: {}.", e.details.join(" ").trim().trim_end_matches('.')),
        "USER_IS_NOT_LOCAL_SUPERUSER" => "Only root can change it, and this connection isn't logged in as root.".into(),
        "HA_IS_ENABLED" => "Not while high availability is on in the pool.".into(),
        "BACKUP_SCRIPT_FAILED" => format!("The server's backup script failed: {}", e.details.join(" ").trim()),
        "RESTORE_SCRIPT_FAILED" => format!("The server's restore script failed (is it a backup of this server, and whole?): {}", e.details.join(" ").trim()),
        "SYSTEM_STATUS_RETRIEVAL_FAILED" => format!("The server couldn't gather its report: {}", e.details.join(" ").trim()),
        LOST_CONTACT => "The server stopped answering.".into(),
        _ => e.message(),
    }
}

/// A menu's entries, each with its glyph, greyed out with why when it can't be done now.
pub(super) fn tool_entries(ui: &mut Ui, pal: &Pal, entries: Vec<Entry>, out: &mut Vec<Intent>) {
    ui.set_min_width(220.0);
    for (glyph, label, intent, ok, why) in entries {
        let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), if ok { Sense::click() } else { Sense::hover() });
        if ok && resp.hovered() {
            ui.painter().rect_filled(r, Rounding::same(5.0), pal.hover);
        }
        let ink = if ok { pal.ink } else { pal.ink_faint };
        paint_glyph(ui.painter(), Rect::from_min_size(r.min + vec2(7.0, 7.0), vec2(16.0, 16.0)), glyph, if ok { pal.accent } else { pal.ink_faint });
        ui.painter().text(pos2(r.min.x + 32.0, r.center().y), Align2::LEFT_CENTER, label, FontId::proportional(13.0), ink);
        let resp = if ok { resp } else { resp.on_hover_text(why) };
        if ok && resp.clicked() {
            out.push(intent);
            ui.close_menu();
        }
    }
}

/// What's at a menu row's start: a toolbar icon in its action's colour, or a glyph in a colour.
pub(super) enum Icon<'a> {
    Tool(&'a str),
    Glyph(&'a str, Color32),
}

/// A menu's row: its icon, its name, a shortcut on the right; greyed out with why when it can't
/// be done now.
#[allow(clippy::too_many_arguments)]
pub(super) fn menu_row(ui: &mut Ui, pal: &Pal, icon: Icon, label: &str, shortcut: &str, ok: bool, why: &str, intent: Intent, out: &mut Vec<Intent>) {
    if action_row(ui, pal, icon, label, shortcut, ok, why) {
        out.push(intent);
    }
}

/// A menu's row as `menu_row`, for an action done on the spot: whether it was clicked (the menu
/// then closes).
#[allow(clippy::too_many_arguments)]
pub(super) fn action_row(ui: &mut Ui, pal: &Pal, icon: Icon, label: &str, shortcut: &str, ok: bool, why: &str) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), if ok { Sense::click() } else { Sense::hover() });
    if ok && resp.hovered() {
        ui.painter().rect_filled(r, Rounding::same(5.0), pal.hover);
    }
    match icon {
        Icon::Tool(op) => paint_tool(ui.painter(), Rect::from_min_size(r.min + vec2(4.0, 5.0), vec2(20.0, 20.0)), op, ok, pal),
        Icon::Glyph(g, col) => paint_glyph(ui.painter(), Rect::from_min_size(r.min + vec2(7.0, 7.0), vec2(16.0, 16.0)), g, if ok { col } else { pal.ink_faint }),
    }
    ui.painter().text(pos2(r.min.x + 32.0, r.center().y), Align2::LEFT_CENTER, label, FontId::proportional(13.0), if ok { pal.ink } else { pal.ink_faint });
    if !shortcut.is_empty() {
        ui.painter().text(pos2(r.max.x - 8.0, r.center().y), Align2::RIGHT_CENTER, shortcut, FontId::proportional(12.0), pal.ink_faint);
    }
    let resp = if ok || why.is_empty() { resp } else { resp.on_hover_text(why) };
    let clicked = ok && resp.clicked();
    if clicked {
        ui.close_menu();
    }
    clicked
}

/// A submenu's row in a menu, lined up with `menu_row`'s: its glyph, its name, the arrow; opens
/// on hover.
pub(super) fn submenu_row(ui: &mut Ui, pal: &Pal, glyph: &str, label: &str, ok: bool, add_contents: impl FnOnce(&mut Ui)) {
    ui.scope(|ui| {
        ui.spacing_mut().interact_size.y = 30.0;
        ui.add_enabled_ui(ok, |ui| {
            // the name after room for the glyph, as the other rows have it
            let mut job = egui::text::LayoutJob::default();
            let pad = ui.spacing().button_padding.x;
            job.append(label, 32.0 - pad, egui::TextFormat { font_id: FontId::proportional(13.0), color: Color32::PLACEHOLDER, ..Default::default() });
            let r = ui.menu_button(job, add_contents).response.rect;
            paint_glyph(ui.painter(), Rect::from_min_size(pos2(r.min.x + 7.0, r.center().y - 8.0), vec2(16.0, 16.0)), glyph, if ok { pal.accent } else { pal.ink_faint });
        });
    });
}

/// A menu's entry in red (Disconnect).
pub(super) fn red_entry(ui: &mut Ui, pal: &Pal, glyph: &str, label: &str, intent: Intent, out: &mut Vec<Intent>) {
    red_entry_if(ui, pal, glyph, label, intent, true, "", out);
}

/// A menu's entry in red (Delete VM), greyed out with why when it can't be done now.
#[allow(clippy::too_many_arguments)]
pub(super) fn red_entry_if(ui: &mut Ui, pal: &Pal, glyph: &str, label: &str, intent: Intent, ok: bool, why: &str, out: &mut Vec<Intent>) {
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), if ok { Sense::click() } else { Sense::hover() });
    if ok && resp.hovered() {
        ui.painter().rect_filled(r, Rounding::same(5.0), pal.red.gamma_multiply(if pal.dark { 0.22 } else { 0.10 }));
    }
    let col = if ok { pal.red } else { pal.ink_faint };
    paint_glyph(ui.painter(), Rect::from_min_size(r.min + vec2(7.0, 7.0), vec2(16.0, 16.0)), glyph, col);
    ui.painter().text(pos2(r.min.x + 32.0, r.center().y), Align2::LEFT_CENTER, label, FontId::proportional(13.0), col);
    let resp = if ok || why.is_empty() { resp } else { resp.on_hover_text(why) };
    if ok && resp.clicked() {
        out.push(intent);
        ui.close_menu();
    }
}
