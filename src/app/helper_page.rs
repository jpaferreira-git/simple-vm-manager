//! A pool's Helper tab: the Server Helper (see helper.rs) on each of its servers, installed from
//! a server's console (this app types the command there), then its settings: where alerts go,
//! what it watches, what it does by itself; and what it did lately.

use super::*;
use crate::helper::{Channel, Presence, Settings};

/// What a helper's job was, for what follows it.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Did {
    Saved,
    Tested,
    Looked,
    Removed,
    Installed,
}

/// A pool's Helper tab: its servers' helpers as last asked, its settings as read and as being
/// changed, what it did lately, and what's under way.
#[derive(Default)]
pub(super) struct HelperView {
    pub hosts: Vec<(String, Presence)>,
    pub checked: Option<Instant>,
    pub checking: bool,
    pub saved: Option<Result<Settings, String>>,
    pub reading: bool,
    pub draft: Option<Settings>,
    /// Its log (newest last), when it was last read, and whether it's being read.
    pub status: Option<Result<(i64, Vec<String>), String>>,
    pub status_at: Option<Instant>,
    pub status_busy: bool,
    /// Ticked: the notice was read, and installing it is on the person.
    pub accepted: bool,
    pub busy: Option<Did>,
    /// The last job's outcome: (fine, what).
    pub note: Option<(bool, String)>,
    /// The server whose removal is being asked about ("" for all of them).
    pub removing: Option<String>,
}

impl HelperView {
    fn coordinator_has_it(&self, master: &str) -> bool {
        self.hosts.iter().any(|(h, p)| h == master && p.installed())
    }
}

impl App {
    /// Asks each server of pool `sid` whether the helper is on it, when that's due (every
    /// minute; every five seconds while one is being installed).
    pub(super) fn helper_poll(&mut self, sid: u32, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let Some(d) = s.data.as_ref() else { return };
        let hosts: Vec<(String, bool)> = d.hosts.iter().map(|h| (h.rref.clone(), h.is_up())).collect();
        let conn = s.conn.clone();
        let v = self.helper.entry(sid).or_default();
        let waiting = v.accepted && v.hosts.iter().any(|(_, p)| !p.installed());
        let every = if waiting { Duration::from_secs(5) } else { Duration::from_secs(60) };
        if !v.checking && v.checked.map_or(true, |t| t.elapsed() >= every) {
            v.checking = true;
            let (tx, ctx) = (self.tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let result = hosts
                    .into_iter()
                    .map(|(h, up)| {
                        let p = if !up {
                            Presence::Unknown("It's off, or not answering".into())
                        } else {
                            match conn.helper_version(&h) {
                                Ok(Some(v)) => Presence::Installed(v),
                                Ok(None) => Presence::Missing,
                                Err(e) => Presence::Unknown(e.message()),
                            }
                        };
                        (h, p)
                    })
                    .collect();
                let _ = tx.send(Msg::HelperChecked { sid, result });
                ctx.request_repaint();
            });
        }
    }

    /// Its settings and (`log`) its log, when the coordinator has it: read once, its log every
    /// 30 s.
    pub(super) fn helper_read(&mut self, sid: u32, master: &str, log: bool, ctx: &egui::Context) {
        let Some(conn) = self.server(sid).map(|s| s.conn.clone()) else { return };
        let v = self.helper.entry(sid).or_default();
        if !v.coordinator_has_it(master) {
            return;
        }
        if v.saved.is_none() && !v.reading {
            v.reading = true;
            let (tx, ctx, c) = (self.tx.clone(), ctx.clone(), conn.clone());
            std::thread::spawn(move || {
                let result = c.helper_settings();
                let _ = tx.send(Msg::HelperRead { sid, result });
                ctx.request_repaint();
            });
        }
        if log && !v.status_busy && v.status_at.map_or(true, |t| t.elapsed() >= Duration::from_secs(30)) {
            v.status_busy = true;
            let (tx, ctx, host) = (self.tx.clone(), ctx.clone(), master.to_string());
            std::thread::spawn(move || {
                let result = conn.helper_call(&host, "status", &[("lines", "20".into())]);
                let _ = tx.send(Msg::HelperStatus { sid, result });
                ctx.request_repaint();
            });
        }
    }

    pub(super) fn helper_checked(&mut self, sid: u32, result: Vec<(String, Presence)>) {
        let v = self.helper.entry(sid).or_default();
        v.checking = false;
        v.checked = Some(Instant::now());
        v.hosts = result;
    }

    pub(super) fn helper_read_done(&mut self, sid: u32, result: Result<String, ApiError>) {
        let v = self.helper.entry(sid).or_default();
        v.reading = false;
        match result {
            Ok(text) => {
                let x = Settings::parse(&text);
                if v.draft.is_none() {
                    v.draft = Some(x.clone());
                }
                v.saved = Some(Ok(x));
            }
            Err(e) => v.saved = Some(Err(e.message())),
        }
    }

    pub(super) fn helper_status_done(&mut self, sid: u32, result: Result<String, ApiError>) {
        let v = self.helper.entry(sid).or_default();
        v.status_busy = false;
        v.status_at = Some(Instant::now());
        v.status = Some(match result {
            Ok(text) => {
                let j: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                let lines = j.get("log").and_then(|l| l.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
                Ok((j.get("last_tick").and_then(|t| t.as_f64()).unwrap_or(0.0) as i64, lines))
            }
            Err(e) => Err(e.message()),
        });
    }

    /// A helper's job in the background (`log`: its line under Activities, if it has one).
    fn helper_job(&mut self, sid: u32, what: Did, text: Option<String>, work: impl FnOnce(&dyn Connection) -> Result<String, ApiError> + Send + 'static, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let (conn, tx, ctx, server) = (s.conn.clone(), self.tx.clone(), ctx.clone(), s.name());
        let log = text.map(|t| self.add_log(sid, &server, None, &server, &t, "In progress"));
        let prog = log.map(|l| self.progress(l));
        let v = self.helper.entry(sid).or_default();
        v.busy = Some(what);
        v.note = None;
        std::thread::spawn(move || {
            if let Some(p) = &prog {
                crate::conn::watch(p);
            }
            let result = work(conn.as_ref());
            let _ = tx.send(Msg::HelperDone { sid, log, what, result });
            ctx.request_repaint();
        });
    }

    pub(super) fn helper_done(&mut self, sid: u32, log: Option<usize>, what: Did, result: Result<String, ApiError>) {
        if let Some(l) = log {
            match &result {
                Ok(_) => self.end_log(l, "Done", None),
                Err(e) => {
                    let text = self.log.get(l).map(|x| x.text.replacen("Saving", "Couldn't save", 1).replacen("Removing", "Couldn't remove", 1)).unwrap_or_default();
                    self.end_log(l, "Failed", Some(format!("{}: {}", text, e.message())));
                }
            }
        }
        let v = self.helper.entry(sid).or_default();
        v.busy = None;
        match (what, result) {
            (Did::Saved, Ok(_)) => {
                v.saved = v.draft.clone().map(Ok);
                v.note = Some((true, "Saved: the helper goes by them from its next look (within a minute).".into()));
                self.refresh_soon(sid);
            }
            (Did::Tested, Ok(text)) => {
                let j: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                let parts: Vec<(String, String)> = j.as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect()).unwrap_or_default();
                v.note = Some(if parts.is_empty() {
                    (false, "No channel is on to send it to (saved ones are tested: save first).".into())
                } else if parts.iter().all(|(_, r)| r == "ok") {
                    (true, format!("Sent to {}: look there.", parts.iter().map(|(k, _)| channel_name(k)).collect::<Vec<_>>().join(", ")))
                } else {
                    (false, parts.iter().map(|(k, r)| format!("{}: {}", channel_name(k), if r == "ok" { "sent" } else { r.as_str() })).collect::<Vec<_>>().join("  ·  "))
                });
            }
            (Did::Looked, Ok(text)) => {
                let lines: Vec<String> = serde_json::from_str(&text).unwrap_or_default();
                v.note = Some((true, format!("It looked: {}.", lines.join("; "))));
                v.status_at = None;
            }
            (Did::Removed, Ok(_)) | (Did::Installed, Ok(_)) => {
                v.checked = None;
                v.removing = None;
            }
            (_, Err(e)) => {
                v.note = Some((false, e.message()));
                v.removing = None;
            }
        }
    }

    /// The Helper tab of pool `sel`.
    pub(super) fn helper_page(&mut self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let Some(sid) = sel.sid else { return };
        let ctx = ui.ctx().clone();
        self.helper_poll(sid, &ctx);
        let Some(d) = self.server(sid).and_then(|s| s.data.clone()) else { return };
        let demo = self.server(sid).map_or(false, |s| s.conn.is_demo());
        let master = d.hosts.iter().find(|h| h.is_master).map(|h| h.rref.clone()).unwrap_or_default();
        self.helper_read(sid, &master, true, &ctx);
        let command = crate::helper::install_command();
        let mut act: Option<HelperAct> = None;
        let v = self.helper.entry(sid).or_default();
        let any = v.hosts.iter().any(|(_, p)| p.installed());
        let on_master = v.coordinator_has_it(&master);
        egui::ScrollArea::vertical().id_salt("helper-page").auto_shrink([false; 2]).show(ui, |ui| {
            let pad = 28.0;
            let w = (ui.available_width() - 2.0 * pad).min(860.0);
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                ui.add_space(pad);
                ui.vertical(|ui| {
                    ui.set_width(w);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    // what it is, before it's installed anywhere
                    if !any {
                        soft(ui, &pal, "An optional add-on for this pool's servers, so they do more while this app is closed: the pool's alerts on your phone (Telegram, ntfy, Discord, Home Assistant or any webhook), any storage nearly full, missed snapshot schedule runs made up, the NAS mounted again after a server started before it, and VMs started in order when the pool starts.");
                        ui.add_space(12.0);
                        boxed(ui, &pal, |ui| {
                            ui.label(egui::RichText::new("What it adds on each server").font(FontId::new(13.5, FontFamily::Name("bold".into()))).color(pal.ink));
                            ui.add_space(4.0);
                            for line in [
                                "One plugin file, which this app talks to: /etc/xapi.d/plugins/svm-helper",
                                "One timer that looks every minute: /etc/cron.d/svm-helper",
                                "Its settings live in the pool itself, so every server has them. Nothing else changes, and Remove takes it all away again.",
                            ] {
                                ui.add(egui::Label::new(egui::RichText::new(format!("•  {}", line)).size(13.0).color(pal.ink_soft)).wrap());
                            }
                            ui.add_space(8.0);
                            ui.horizontal_wrapped(|ui| {
                                ui.label(egui::RichText::new("It's open source (MIT), like this app: read every line before you install it. The command checks the file's fingerprint (SHA-256) before it puts it in place.").size(13.0).color(pal.ink_soft));
                                if ui.add(egui::Label::new(egui::RichText::new("See its code on GitHub  ›").size(13.0).color(pal.link)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                    open_url(&crate::helper::source_url());
                                }
                            });
                        });
                        ui.add_space(12.0);
                        crate::newvm::alert(
                            ui,
                            &pal,
                            false,
                            "It's free software, given as it is, with no warranty.",
                            "You install it on your servers at your own risk, and you're responsible for it, as for anything else you run there. It isn't part of XenServer, nor supported by its makers: updates keep it, but reinstalling XenServer on a server removes it (this app then offers it again).",
                            None,
                        );
                        ui.add_space(10.0);
                        ui.checkbox(&mut v.accepted, "I've read this, and I take responsibility for installing it on my servers.");
                        ui.add_space(20.0);
                    }
                    // its servers
                    super::overview::section(ui, &pal, "On each server");
                    super::overview::panel(ui, &pal, |ui| {
                        let rows = d.hosts.len();
                        for (i, h) in d.hosts.iter().enumerate() {
                            let p = v.hosts.iter().find(|(r, _)| *r == h.rref).map(|x| x.1.clone()).unwrap_or(Presence::Checking);
                            host_row(ui, &pal, h, &p, v.accepted || any, demo, i + 1 == rows, &mut act);
                        }
                    });
                    if v.hosts.iter().any(|(_, p)| !p.installed() || p.outdated()) && (v.accepted || any) {
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(vec2((ui.available_width() - 80.0).max(200.0), 32.0), Sense::hover());
                            ui.painter().rect(r, Rounding::same(6.0), if pal.dark { Color32::from_rgb(15, 16, 19) } else { Color32::from_rgb(244, 245, 247) }, Stroke::new(1.0_f32, pal.border));
                            let (g, _) = super::overview::fit(ui.painter(), &command, FontId::monospace(12.0), pal.ink, r.width() - 20.0);
                            ui.painter().galley(pos2(r.min.x + 10.0, r.center().y - g.size().y / 2.0), g, pal.ink);
                            if flat_button(ui, "copy", "Copy", None, true, false, &pal).on_hover_text(&command).clicked() {
                                ui.ctx().copy_text(command.clone());
                            }
                        });
                        ui.add_space(6.0);
                        soft_small(ui, &pal, "In a server's Console, choose Local Command Shell (or log in as root), then Type in its console types the command there: press Enter to run it. It fetches the helper from GitHub, so the server needs the internet. Or run it over SSH.");
                    }
                    // its settings, once the coordinator has it
                    if on_master {
                        ui.add_space(22.0);
                        settings_ui(ui, &pal, v, &d, &mut act);
                        // what it did lately
                        ui.add_space(22.0);
                        super::overview::section(ui, &pal, "What it did lately");
                        super::overview::panel(ui, &pal, |ui| {
                            ui.add_space(6.0);
                            match &v.status {
                                None => {
                                    ui.horizontal(|ui| {
                                        ui.add_space(16.0);
                                        ui.label(egui::RichText::new("Reading…").size(13.0).color(pal.ink_soft));
                                    });
                                }
                                Some(Err(e)) => {
                                    ui.horizontal(|ui| {
                                        ui.add_space(16.0);
                                        ui.add(egui::Label::new(egui::RichText::new(format!("Couldn't read it: {}", e)).size(13.0).color(pal.red)).wrap());
                                    });
                                }
                                Some(Ok((last, lines))) => {
                                    let now = unix_now();
                                    let when = if *last > 0 { format!("It last looked {}.", ago(now - *last)) } else { "It hasn't looked yet.".into() };
                                    ui.horizontal(|ui| {
                                        ui.add_space(16.0);
                                        ui.label(egui::RichText::new(when).size(13.0).color(pal.ink_soft));
                                    });
                                    if lines.is_empty() {
                                        ui.horizontal(|ui| {
                                            ui.add_space(16.0);
                                            ui.label(egui::RichText::new("Nothing to say yet: all well.").size(13.0).color(pal.ink_soft));
                                        });
                                    }
                                    for l in lines.iter().rev().take(12) {
                                        ui.add_space(4.0);
                                        ui.horizontal(|ui| {
                                            ui.add_space(16.0);
                                            ui.add(egui::Label::new(egui::RichText::new(l.as_str()).size(12.5).color(pal.ink)).wrap());
                                        });
                                    }
                                }
                            }
                            ui.add_space(8.0);
                        });
                    }
                    ui.add_space(28.0);
                });
            });
        });
        // what was asked
        let Some(a) = act else { return };
        match a {
            HelperAct::Type(host) => {
                let Some(h) = d.host(&host) else { return };
                if demo {
                    // (the demo's console is a pretend one: it's as if the command ran there)
                    self.helper_job(sid, Did::Installed, None, move |c| c.helper_call(&host, "demo-install", &[]), &ctx);
                    return;
                }
                self.helper_type = Some((sid, h.control_domain.clone(), command));
                out.push(Intent::Select(format!("host:{}:{}", sid, host)));
                out.push(Intent::Tab(Tab::Console));
            }
            HelperAct::Save => {
                let Some(x) = self.helper.get(&sid).and_then(|v| v.draft.clone()) else { return };
                let before = self.helper.get(&sid).and_then(|v| v.saved.clone()).and_then(|r| r.ok()).unwrap_or_default();
                // the VMs it starts in order don't start by themselves; those taken off do again
                let set = |s: &Settings| -> Vec<String> { if s.order_on { s.order.iter().map(|o| o.0.clone()).collect() } else { vec![] } };
                let (was, now) = (set(&before), set(&x));
                let by_uuid = |u: &str| d.vms.iter().find(|v| v.uuid == u).map(|v| v.rref.clone());
                let mut autostart: Vec<(String, bool)> = now.iter().filter(|u| !was.contains(u)).filter_map(|u| by_uuid(u)).map(|r| (r, false)).collect();
                autostart.extend(was.iter().filter(|u| !now.contains(u)).filter_map(|u| by_uuid(u)).map(|r| (r, true)));
                let text = x.to_json();
                let pool = d.name.clone();
                self.helper_job(sid, Did::Saved, Some(format!("Saving the Server Helper's settings for '{}'", pool)), move |c| c.save_helper_settings(&text, &autostart).map(|_| String::new()), &ctx);
            }
            HelperAct::Undo => {
                let v = self.helper.entry(sid).or_default();
                v.draft = v.saved.clone().and_then(|r| r.ok());
                v.note = None;
            }
            HelperAct::Test => {
                let Some(x) = self.helper.get(&sid).and_then(|v| v.draft.clone()) else { return };
                let (text, host) = (x.to_json(), master.clone());
                self.helper_job(sid, Did::Tested, None, move |c| c.helper_call(&host, "test", &[("config", text)]), &ctx);
            }
            HelperAct::Look => {
                let host = master.clone();
                self.helper_job(sid, Did::Looked, None, move |c| c.helper_call(&host, "tick", &[]), &ctx);
            }
            HelperAct::AskRemove(h) => self.helper.entry(sid).or_default().removing = Some(h),
            HelperAct::Remove => {
                let installed: Vec<String> = self.helper.get(&sid).map(|v| v.hosts.iter().filter(|(_, p)| p.installed()).map(|(h, _)| h.clone()).collect()).unwrap_or_default();
                let pool = d.name.clone();
                self.helper_job(
                    sid,
                    Did::Removed,
                    Some(format!("Removing the Server Helper from '{}'", pool)),
                    move |c| {
                        for h in &installed {
                            c.helper_call(h, "uninstall", &[])?;
                        }
                        Ok(String::new())
                    },
                    &ctx,
                );
            }
        }
    }
}

/// What the page asked for.
enum HelperAct {
    Type(String),
    Save,
    Undo,
    Test,
    Look,
    AskRemove(String),
    Remove,
}

fn channel_name(k: &str) -> &str {
    match k {
        "telegram" => "Telegram",
        "ntfy" => "ntfy",
        "discord" => "Discord",
        "homeassistant" => "Home Assistant",
        "webhook" => "the webhook",
        _ => k,
    }
}

/// How long ago, in words: "20 s ago", "3 min ago", "2 h ago".
fn ago(s: i64) -> String {
    match s.max(0) {
        s if s < 90 => format!("{} s ago", s),
        s if s < 5400 => format!("{} min ago", s / 60),
        s => format!("{} h ago", s / 3600),
    }
}

fn soft(ui: &mut Ui, pal: &Pal, text: &str) {
    ui.add(egui::Label::new(egui::RichText::new(text).size(13.5).color(pal.ink_soft)).wrap());
}

fn soft_small(ui: &mut Ui, pal: &Pal, text: &str) {
    ui.add(egui::Label::new(egui::RichText::new(text).size(12.5).color(pal.ink_soft)).wrap());
}

/// A rounded box, a hair of a border, room inside.
fn boxed(ui: &mut Ui, pal: &Pal, add: impl FnOnce(&mut Ui)) {
    egui::Frame::none().fill(pal.surface).stroke(Stroke::new(1.0_f32, pal.border.gamma_multiply(0.7))).rounding(Rounding::same(12.0)).inner_margin(egui::Margin::symmetric(16.0, 12.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 2.0;
        add(ui);
    });
}

/// An on/off switch; whether it was flipped.
fn switch(ui: &mut Ui, pal: &Pal, on: &mut bool, tip: &str) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(38.0, 22.0), Sense::click());
    let resp = resp.on_hover_text(tip);
    let track = if *on { pal.accent } else { pal.border_strong };
    ui.painter().rect_filled(r, Rounding::same(11.0), track);
    ui.painter().circle_filled(pos2(if *on { r.max.x - 11.0 } else { r.min.x + 11.0 }, r.center().y), 8.5, Color32::WHITE);
    if resp.clicked() {
        *on = !*on;
        return true;
    }
    false
}

/// A server's row: its name (the coordinator said so), its helper, and the ways to install it.
#[allow(clippy::too_many_arguments)]
fn host_row(ui: &mut Ui, pal: &Pal, h: &HostInfo, p: &Presence, can: bool, demo: bool, last: bool, act: &mut Option<HelperAct>) {
    ui.horizontal(|ui| {
        ui.set_min_height(46.0);
        ui.add_space(16.0);
        let dot = match p {
            Presence::Installed(_) if !p.outdated() => pal.green,
            Presence::Installed(_) => pal.orange,
            Presence::Missing => pal.border_strong,
            Presence::Checking | Presence::Unknown(_) => pal.ink_faint,
        };
        let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 4.5, dot);
        ui.add_space(4.0);
        ui.label(egui::RichText::new(h.name.as_str()).size(14.0).color(pal.ink));
        if h.is_master {
            ui.label(egui::RichText::new("coordinator").size(12.0).color(pal.ink_soft));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(12.0);
            let (text, col) = match p {
                Presence::Checking => ("Looking…".to_string(), pal.ink_soft),
                Presence::Installed(v) if p.outdated() => (format!("Version {}: update it", v), pal.orange),
                Presence::Installed(v) => (format!("Installed  ·  {}", v), pal.green),
                Presence::Missing => ("Not installed".to_string(), pal.ink_soft),
                Presence::Unknown(why) => (why.clone(), pal.ink_faint),
            };
            if matches!(p, Presence::Missing) || p.outdated() {
                let label = if demo { "Install (demo)" } else { "Type in its console" };
                let tip = if can { "Opens its Console and types the command there: press Enter to run it" } else { "Tick the box above first" };
                if flat_button(ui, "console", label, Some(pal.accent), can && h.is_up(), false, pal).on_hover_text(tip).on_disabled_hover_text(tip).clicked() {
                    *act = Some(HelperAct::Type(h.rref.clone()));
                }
            }
            ui.label(egui::RichText::new(text).size(13.0).color(col));
        });
    });
    if !last {
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
        ui.painter().line_segment([pos2(r.min.x + 40.0, r.center().y), pos2(r.max.x, r.center().y)], Stroke::new(1.0_f32, pal.border.gamma_multiply(0.6)));
    }
}

/// The settings: where alerts go, what it watches, what it does by itself; Save and Undo.
fn settings_ui(ui: &mut Ui, pal: &Pal, v: &mut HelperView, d: &PoolData, act: &mut Option<HelperAct>) {
    let busy = v.busy.is_some();
    // its state, Look now, Remove (under it, in a narrow window)
    let n = v.hosts.iter().filter(|(_, p)| p.installed()).count();
    let wide = ui.available_width() >= 600.0;
    let mut buttons = |ui: &mut Ui, v: &mut HelperView| {
        if flat_button(ui, "trash", "Remove…", Some(pal.red), !busy, false, pal).on_hover_text("Remove it from every server").clicked() {
            *act = Some(HelperAct::AskRemove(String::new()));
        }
        if flat_button(ui, "rescan", if v.busy == Some(Did::Looked) { "Looking…" } else { "Look now" }, Some(pal.accent), !busy, false, pal).on_hover_text("Have it look now, rather than within the minute").clicked() {
            *act = Some(HelperAct::Look);
        }
    };
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("Server Helper {}", crate::helper::VERSION)).font(FontId::new(14.0, FontFamily::Name("bold".into()))).color(pal.ink));
        ui.label(egui::RichText::new(format!("on {} of {} servers", n, v.hosts.len())).size(13.0).color(pal.ink_soft));
        if wide {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| buttons(ui, v));
        }
    });
    if !wide {
        ui.add_space(6.0);
        ui.horizontal(|ui| ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| buttons(ui, v)));
    }
    if v.removing.is_some() {
        ui.add_space(8.0);
        crate::newvm::alert(ui, pal, true, "Remove the Server Helper from every server?", "Its two files, its state and its log go; its settings stay in the pool, should you install it again. The VMs it started in order don't start by themselves again: turn on Starts with its server on those you want.", None);
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if dialog_button(ui, pal, "trash", "Remove", Look::Danger, !busy) {
                *act = Some(HelperAct::Remove);
            }
            if dialog_button(ui, pal, "close", "Keep it", Look::Plain, true) {
                v.removing = None;
            }
        });
    }
    ui.add_space(16.0);
    let x = match (&mut v.draft, &v.saved) {
        (Some(x), _) => x,
        (None, Some(Err(e))) => {
            ui.label(egui::RichText::new(format!("Couldn't read its settings: {}", e)).color(pal.red));
            return;
        }
        (None, _) => {
            ui.label(egui::RichText::new("Reading its settings…").color(pal.ink_soft));
            return;
        }
    };
    let field_w: f32 = 300.0;
    // where alerts go
    super::overview::section(ui, pal, "Where alerts go");
    super::overview::panel(ui, pal, |ui| {
        let channels: [(&str, &str, &mut Channel, &str, &str, bool); 5] = [
            ("Telegram", "Your bot's message to a chat", &mut x.telegram, "Bot token", "Chat id", true),
            ("ntfy", "A topic on ntfy.sh, or your own server", &mut x.ntfy, "Topic address", "Access token (if it has one)", true),
            ("Discord", "A channel's webhook", &mut x.discord, "Webhook address", "", false),
            ("Home Assistant", "A webhook automation: a notification on your phones", &mut x.homeassistant, "Webhook address", "", false),
            ("Any webhook", "A JSON POST to an address of yours", &mut x.webhook, "Address", "", false),
        ];
        let n = channels.len();
        for (i, (name, sub, c, first, second, has_second)) in channels.into_iter().enumerate() {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(name).size(14.0).color(pal.ink));
                    ui.label(egui::RichText::new(sub).size(12.0).color(pal.ink_soft));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(16.0);
                    let tip = if c.on { "On: alerts go here" } else { "Off" };
                    switch(ui, pal, &mut c.on, tip);
                });
            });
            if c.on {
                ui.add_space(6.0);
                for (label, value, password) in [(first, &mut c.url, name == "Telegram"), (second, &mut c.extra, name == "ntfy")] {
                    if label.is_empty() || (!has_second && label == second) {
                        continue;
                    }
                    ui.horizontal(|ui| {
                        ui.add_space(16.0);
                        field_label(ui, label, 170.0);
                        text_field(ui, pal, value, field_w.min(ui.available_width() - 20.0), password);
                    });
                    ui.add_space(4.0);
                }
            }
            ui.add_space(8.0);
            if i + 1 < n {
                let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
                ui.painter().line_segment([pos2(r.min.x + 16.0, r.center().y), pos2(r.max.x, r.center().y)], Stroke::new(1.0_f32, pal.border.gamma_multiply(0.6)));
            }
        }
    });
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        field_label(ui, "Send:", 60.0);
        let mut at = match x.min_priority {
            p if p <= 2 => 0,
            3 | 4 => 1,
            _ => 2,
        };
        if crate::isolib::segmented(ui, pal, &["Critical only", "Critical and warnings", "Everything"], &mut at, 420.0_f32.min(ui.available_width() - 4.0)) {
            x.min_priority = [2, 3, 5][at];
        }
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        switch(ui, pal, &mut x.quiet.0, "Quiet hours: only critical alerts then");
        ui.label(egui::RichText::new("Quiet from").color(pal.ink));
        text_field(ui, pal, &mut x.quiet.1, 64.0, false);
        ui.label(egui::RichText::new("to").color(pal.ink));
        text_field(ui, pal, &mut x.quiet.2, 64.0, false);
        ui.label(egui::RichText::new("(only critical ones then)").size(12.5).color(pal.ink_soft));
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if flat_button(ui, "bell", if v.busy == Some(Did::Tested) { "Sending…" } else { "Send a test" }, Some(pal.accent), !busy && x.sends(), false, pal).on_hover_text("Sends a test alert to the channels that are on, as typed here").clicked() {
            *act = Some(HelperAct::Test);
        }
    });
    // what it watches
    ui.add_space(20.0);
    super::overview::section(ui, pal, "What it watches");
    super::overview::panel(ui, pal, |ui| {
        line_switch(ui, pal, &mut x.storage.0, "Storage fuller than a limit", "Any kind of storage, local, NFS or SMB too. A limit set in a storage's own Alerts… wins.", Some(&mut x.storage.1), false);
        line_switch(ui, pal, &mut x.schedules, "Snapshot schedules: make up a missed time", "When a time went by without its snapshots (the server off, or the schedule changed after it).", None, true);
    });
    ui.add_space(6.0);
    soft_small(ui, pal, "And everything the pool itself raises goes out too: a VM that crashed, a certificate running out, a limit you set in Alerts…");
    // what it does by itself
    ui.add_space(20.0);
    super::overview::section(ui, pal, "What it does by itself");
    super::overview::panel(ui, pal, |ui| {
        line_switch(ui, pal, &mut x.remount, "Mount shared storage again", "On a server that started before the NAS was there, as soon as it's back.", None, false);
        let last = !x.order_on;
        line_switch(ui, pal, &mut x.order_on, "Start VMs in order when the pool starts", "Each after the one before, with a wait between them. These no longer start by themselves.", None, last);
        if x.order_on {
            order_ui(ui, pal, x, d);
        }
    });
    // Save, Undo
    ui.add_space(16.0);
    let changed = v.saved.as_ref().and_then(|r| r.as_ref().ok()) != Some(&*x);
    let problem = x.problem();
    ui.horizontal(|ui| {
        if dialog_button(ui, pal, "check", if v.busy == Some(Did::Saved) { "Saving…" } else { "Save" }, Look::Primary, changed && problem.is_none() && !busy) {
            *act = Some(HelperAct::Save);
        }
        if dialog_button(ui, pal, "revert", "Undo changes", Look::Plain, changed && !busy) {
            *act = Some(HelperAct::Undo);
        }
        if let Some(p) = &problem {
            ui.label(egui::RichText::new(p.as_str()).size(12.5).color(pal.red));
        }
    });
    if let Some((ok, text)) = &v.note {
        ui.add_space(8.0);
        ui.add(egui::Label::new(egui::RichText::new(text.as_str()).size(12.5).color(if *ok { pal.green } else { pal.red })).wrap());
    }
}

/// A panel's line with a switch: its name, what it means; a figure (%) beside it when it has one.
fn line_switch(ui: &mut Ui, pal: &Pal, on: &mut bool, title: &str, sub: &str, figure: Option<&mut i64>, last: bool) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.vertical(|ui| {
            ui.set_max_width((ui.available_width() - 200.0).max(200.0));
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(title).size(14.0).color(pal.ink));
                if let Some(f) = figure {
                    let mut text = f.to_string();
                    if ui.add_enabled(*on, egui::TextEdit::singleline(&mut text).desired_width(36.0).horizontal_align(egui::Align::Center)).changed() {
                        if let Ok(n) = text.trim().parse::<i64>() {
                            *f = n.clamp(1, 100);
                        }
                    }
                    ui.label(egui::RichText::new("%").color(pal.ink));
                }
            });
            ui.add(egui::Label::new(egui::RichText::new(sub).size(12.0).color(pal.ink_soft)).wrap());
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(16.0);
            switch(ui, pal, on, if *on { "On" } else { "Off" });
        });
    });
    ui.add_space(8.0);
    if !last {
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
        ui.painter().line_segment([pos2(r.min.x + 16.0, r.center().y), pos2(r.max.x, r.center().y)], Stroke::new(1.0_f32, pal.border.gamma_multiply(0.6)));
    }
}

/// The VMs it starts in order: each with its wait after it, moved up or down, taken off; and a VM
/// to add.
fn order_ui(ui: &mut Ui, pal: &Pal, x: &mut Settings, d: &PoolData) {
    let name_of = |u: &str| d.vms.iter().find(|v| v.uuid == u).map(|v| v.name.clone()).unwrap_or_else(|| "A VM that's gone".into());
    let mut swap: Option<(usize, usize)> = None;
    let mut drop: Option<usize> = None;
    let n = x.order.len();
    for (i, (uuid, wait)) in x.order.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.add_space(30.0);
            ui.label(egui::RichText::new(format!("{}.", i + 1)).color(pal.ink_soft));
            ui.label(egui::RichText::new(name_of(uuid)).color(pal.ink));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(16.0);
                if flat_button(ui, "close", "Take it off", None, true, true, pal).clicked() {
                    drop = Some(i);
                }
                if flat_button(ui, "download", "Later", None, i + 1 < n, true, pal).clicked() {
                    swap = Some((i, i + 1));
                }
                if flat_button(ui, "upload", "Sooner", None, i > 0, true, pal).clicked() {
                    swap = Some((i - 1, i));
                }
                ui.label(egui::RichText::new("s after").size(12.5).color(pal.ink_soft));
                let mut text = wait.to_string();
                if ui.add(egui::TextEdit::singleline(&mut text).desired_width(40.0).horizontal_align(egui::Align::Center)).changed() {
                    if let Ok(w) = text.trim().parse::<i64>() {
                        *wait = w.clamp(0, 3600);
                    }
                }
                ui.label(egui::RichText::new("wait").size(12.5).color(pal.ink_soft));
            });
        });
        ui.add_space(4.0);
    }
    if let Some((a, b)) = swap {
        x.order.swap(a, b);
    }
    if let Some(i) = drop {
        x.order.remove(i);
    }
    let left: Vec<&VmInfo> = d.vms.iter().filter(|v| !x.order.iter().any(|o| o.0 == v.uuid)).collect();
    ui.horizontal(|ui| {
        ui.add_space(30.0);
        egui::ComboBox::from_id_salt("helper-order-add").selected_text(if x.order.is_empty() { "Add the first VM…" } else { "Add a VM…" }).width(260.0).show_ui(ui, |ui| {
            for v in &left {
                if ui.selectable_label(false, v.name.as_str()).clicked() {
                    x.order.push((v.uuid.clone(), 30));
                }
            }
        });
    });
    ui.add_space(10.0);
}

/// Opens a link in the system's browser.
pub(super) fn open_url(url: &str) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
}

/// The pool Overview's card while the helper isn't on any of its servers: what it does, a way to
/// set it up (Some(true)) and an X to put it away (Some(false)).
pub(super) fn helper_card(ui: &mut Ui, pal: &Pal) -> Option<bool> {
    let (fill, edge, soft) = if pal.dark {
        (Color32::from_rgb(25, 36, 52), Color32::from_rgb(42, 72, 112), Color32::from_rgb(150, 170, 198))
    } else {
        (Color32::from_rgb(237, 244, 254), Color32::from_rgb(176, 204, 240), Color32::from_rgb(68, 96, 138))
    };
    let mut said = None;
    egui::Frame::none().fill(fill).stroke(Stroke::new(1.0_f32, edge)).rounding(Rounding::same(12.0)).inner_margin(egui::Margin::symmetric(14.0, 10.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
            paint_glyph(ui.painter(), r, "shield", pal.accent);
            ui.add_space(4.0);
            let room = (ui.available_width() - 150.0).max(160.0);
            ui.vertical(|ui| {
                ui.set_max_width(room);
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(egui::RichText::new("Server Helper isn't installed").font(FontId::new(13.5, FontFamily::Name("bold".into()))).color(pal.ink));
                ui.add(egui::Label::new(egui::RichText::new("Alerts on your phone, any storage nearly full, missed snapshots made up, the NAS mounted again, VMs started in order: also while this app is closed.").size(12.5).color(soft)).wrap());
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if super::glyph_button_small(ui, pal, "close", "Don't show this again") {
                    said = Some(false);
                }
                if ui.add(egui::Label::new(egui::RichText::new("Set it up  ›").size(13.0).color(pal.link)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    said = Some(true);
                }
            });
        });
    });
    said
}
