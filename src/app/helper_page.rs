//! A pool's Helper tab: its Helper VM (see helper.rs), as its heartbeat says, or how to set one
//! up; then its settings: where alerts go, what it watches, what it does by itself. The Advisor
//! page (advisor_page.rs) shows the checklist the same VM leaves in the pool.

use super::*;
use crate::helper::{Advice, Channel, Settings, VmStatus};

/// What a helper's job was, for what follows it.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Did {
    Saved,
    /// A request left for the Helper VM (Check now, Send a test, an update).
    Asked,
    /// One of the Advisor's fixes.
    Fixed,
}

/// A pool's Helper VM, as this app knows it: its heartbeat and the Advisor's checklist as last
/// read, its settings as read and as being changed, and what's been asked of it.
#[derive(Default)]
pub(super) struct HelperView {
    /// Its heartbeat (None: the pool has no Helper VM; Err: why it couldn't be read).
    pub vm: Option<Result<Option<VmStatus>, String>>,
    pub advice: Option<Advice>,
    pub polled: Option<Instant>,
    pub polling: bool,
    /// The Advisor's check and the test alert asked of it: when (seconds since 1970), while
    /// unanswered.
    pub asked_advice: Option<i64>,
    pub asked_test: Option<i64>,
    /// What the Advisor page should say about its last Check now, when it didn't work.
    pub advice_note: Option<String>,
    /// The update asked of it: when, while unanswered; and how it went (fine, what).
    pub asked_update: Option<i64>,
    pub update_note: Option<(bool, String)>,
    /// The Advisor's check being fixed, and how the last fix went: (check, fine, what).
    pub fixing: Option<String>,
    pub fix_note: Option<(String, bool, String)>,
    /// A fix was done: the Helper VM's to check the pool again.
    pub recheck: bool,
    pub saved: Option<Result<Settings, String>>,
    pub reading: bool,
    pub draft: Option<Settings>,
    pub busy: Option<Did>,
    /// The last job's outcome: (fine, what).
    pub note: Option<(bool, String)>,
}

impl HelperView {
    /// Its heartbeat, when the pool has a Helper VM.
    pub fn status(&self) -> Option<&VmStatus> {
        self.vm.as_ref().and_then(|r| r.as_ref().ok()).and_then(|v| v.as_ref())
    }

    /// Read, and the pool has none.
    pub fn none(&self) -> bool {
        matches!(self.vm, Some(Ok(None)))
    }
}

/// How long an answer to a request is waited for (an update, longer).
const ANSWER_WITHIN: i64 = 300;
const UPDATE_WITHIN: i64 = 900;

impl App {
    /// Reads pool `sid`'s Helper VM heartbeat and Advisor checklist, when that's due: every
    /// minute, every five seconds while it's asked something.
    pub(super) fn helper_poll(&mut self, sid: u32, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        if s.data.is_none() {
            return;
        }
        let conn = s.conn.clone();
        let now = unix_now();
        // after a fix, the Helper VM looks again (its checklist then says so)
        if std::mem::take(&mut self.helper.entry(sid).or_default().recheck) {
            self.helper_ask(sid, "advice", ctx);
        }
        let v = self.helper.entry(sid).or_default();
        let waiting = [v.asked_advice, v.asked_test].iter().flatten().any(|t| now - t < ANSWER_WITHIN) || v.asked_update.is_some_and(|t| now - t < UPDATE_WITHIN);
        let every = Duration::from_secs(if waiting { 5 } else { 60 });
        if v.polling || v.polled.is_some_and(|t| t.elapsed() < every) {
            return;
        }
        v.polling = true;
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = conn.helper_state();
            let _ = tx.send(Msg::HelperState { sid, result });
            ctx.request_repaint();
        });
    }

    pub(super) fn helper_state_done(&mut self, sid: u32, result: Result<(String, String), ApiError>) {
        let now = unix_now();
        let v = self.helper.entry(sid).or_default();
        v.polling = false;
        v.polled = Some(Instant::now());
        let (beat, advice) = match result {
            Ok(x) => x,
            Err(e) => {
                v.vm = Some(Err(e.message()));
                return;
            }
        };
        v.vm = Some(Ok(VmStatus::parse(&beat)));
        v.advice = Advice::parse(&advice);
        // what was asked, answered
        if let Some(t) = v.asked_advice {
            if v.advice.as_ref().is_some_and(|a| a.request >= t) {
                v.asked_advice = None;
            } else if now - t >= ANSWER_WITHIN {
                v.asked_advice = None;
                v.advice_note = Some("The Helper VM didn't answer in five minutes: is it running? ('svm-helper status' on it tells.)".into());
            }
        }
        if let Some(t) = v.asked_update {
            let st = v.status().cloned();
            if let Some(st) = st.filter(|st| !crate::helper::older(&st.version, crate::helper::LATEST)) {
                v.asked_update = None;
                v.update_note = Some((true, format!("Updated: the Helper VM is version {} now.", st.version)));
            } else if let Some((_, _, false, why)) = v.status().and_then(|st| st.update.clone()).filter(|u| u.1 >= t - 300) {
                v.asked_update = None;
                v.update_note = Some((false, format!("The update didn't work: {}.", if why.is_empty() { "see /var/log/svm-helper-update.log on the Helper VM" } else { why.trim_end_matches('.') })));
            } else if now - t >= UPDATE_WITHIN {
                v.asked_update = None;
                v.update_note = Some((false, "The Helper VM didn't update in fifteen minutes: is it running, and does it reach GitHub?".into()));
            }
        }
        if let Some(t) = v.asked_test {
            let said = v.status().and_then(|st| st.test.clone()).filter(|(req, _)| *req >= t);
            if let Some((_, results)) = said {
                v.asked_test = None;
                v.note = Some(test_note(&results));
            } else if now - t >= ANSWER_WITHIN {
                v.asked_test = None;
                v.note = Some((false, "The Helper VM didn't answer in five minutes: is it running?".into()));
            }
        }
    }

    /// Its settings, read once.
    pub(super) fn helper_read(&mut self, sid: u32, ctx: &egui::Context) {
        let Some(conn) = self.server(sid).map(|s| s.conn.clone()) else { return };
        let v = self.helper.entry(sid).or_default();
        if v.saved.is_some() || v.reading {
            return;
        }
        v.reading = true;
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = conn.helper_settings();
            let _ = tx.send(Msg::HelperRead { sid, result });
            ctx.request_repaint();
        });
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

    /// Asks pool `sid`'s Helper VM for `what` ("advice" or "test"); its answer is read as it comes.
    pub(super) fn helper_ask(&mut self, sid: u32, what: &'static str, ctx: &egui::Context) {
        let now = unix_now();
        let v = self.helper.entry(sid).or_default();
        match what {
            "advice" => {
                v.asked_advice = Some(now);
                v.advice_note = None;
            }
            "update" => {
                v.asked_update = Some(now);
                v.update_note = None;
            }
            _ => v.asked_test = Some(now),
        }
        self.helper_job(sid, Did::Asked, None, move |c| c.helper_ask(what, now).map(|_| String::new()), ctx);
    }

    /// A helper's job in the background (`text`: its line under Activities, if it has one).
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
                    let text = self.log.get(l).map(|x| x.text.replacen("Saving", "Couldn't save", 1)).unwrap_or_default();
                    self.end_log(l, "Failed", Some(format!("{}: {}", text, e.message())));
                }
            }
        }
        let v = self.helper.entry(sid).or_default();
        v.busy = None;
        match (what, result) {
            (Did::Saved, Ok(_)) => {
                v.saved = v.draft.clone().map(Ok);
                v.note = Some((true, "Saved: the Helper VM goes by them from its next look (within a minute).".into()));
                self.refresh_soon(sid);
            }
            // (its answer comes with its next look: helper_poll reads it)
            (Did::Asked, Ok(_)) => v.polled = None,
            (Did::Asked, Err(e)) => {
                v.asked_advice = None;
                v.asked_test = None;
                v.asked_update = None;
                v.update_note = Some((false, format!("Couldn't ask the Helper VM: {}", e.message())));
                v.advice_note = Some(format!("Couldn't ask the Helper VM: {}", e.message()));
                v.note = Some((false, format!("Couldn't ask the Helper VM: {}", e.message())));
            }
            (Did::Fixed, result) => {
                let id = v.fixing.take().unwrap_or_default();
                v.fix_note = Some(match result {
                    Ok(_) => (id, true, "Done. The Helper VM checks the pool again: its list follows in a minute or two.".into()),
                    Err(e) => (id, false, format!("It didn't work: {}", e.message())),
                });
                v.recheck = true;
                self.refresh_soon(sid);
            }
            (_, Err(e)) => v.note = Some((false, e.message())),
        }
    }

    /// One of the Advisor's fixes that this app does itself, on pool `sid` (the others open the
    /// dialog that does it: see advisor_page.rs).
    pub(super) fn advisor_fix(&mut self, sid: u32, id: String, fix: crate::helper::Fix, ctx: &egui::Context) {
        let names = match fix.names.len() {
            0 => String::new(),
            1..=3 => fix.names.join(", "),
            n => format!("{} and {} more", fix.names[..2].join(", "), n - 2),
        };
        let text = match fix.action.as_str() {
            "autostart" => format!("Advisor: turning on Start with the server for {}", names),
            "eject" => format!("Advisor: ejecting the ISO of {}", names),
            "delete_snapshots" => format!("Advisor: deleting {}", names),
            "delete_disks" => format!("Advisor: deleting the disks {}", names),
            _ => return,
        };
        let v = self.helper.entry(sid).or_default();
        v.fixing = Some(id);
        v.fix_note = None;
        let (action, uuids) = (fix.action.clone(), fix.uuids.clone());
        self.helper_job(sid, Did::Fixed, Some(text), move |c| c.advisor_fix(&action, &uuids), ctx);
    }

    /// The Helper tab of pool `sel`.
    pub(super) fn helper_page(&mut self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let Some(sid) = sel.sid else { return };
        let ctx = ui.ctx().clone();
        self.helper_poll(sid, &ctx);
        self.helper_read(sid, &ctx);
        let Some(d) = self.server(sid).and_then(|s| s.data.clone()) else { return };
        let mut act: Option<HelperAct> = None;
        let now = unix_now();
        let v = self.helper.entry(sid).or_default();
        egui::ScrollArea::vertical().id_salt("helper-page").auto_shrink([false; 2]).show(ui, |ui| {
            let pad = 28.0;
            let w = (ui.available_width() - 2.0 * pad).min(860.0);
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                // centred in the window, as wide as it reads well
                ui.add_space(((ui.available_width() - w) / 2.0).max(pad));
                ui.vertical(|ui| {
                    ui.set_width(w);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    vm_card(ui, &pal, v, now, &mut act);
                    if let Some(st) = v.status().cloned() {
                        ui.add_space(22.0);
                        settings_ui(ui, &pal, v, &d, st.requests, &mut act);
                    }
                    ui.add_space(28.0);
                });
            });
        });
        let Some(a) = act else { return };
        match a {
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
                self.helper_job(sid, Did::Saved, Some(format!("Saving the Helper VM's settings for '{}'", pool)), move |c| c.save_helper_settings(&text, &autostart).map(|_| String::new()), &ctx);
            }
            HelperAct::Undo => {
                let v = self.helper.entry(sid).or_default();
                v.draft = v.saved.clone().and_then(|r| r.ok());
                v.note = None;
            }
            HelperAct::Test => self.helper_ask(sid, "test", &ctx),
            HelperAct::Advisor => out.push(Intent::Advisor),
            HelperAct::Update => self.helper_ask(sid, "update", &ctx),
        }
    }
}

/// What the page asked for.
enum HelperAct {
    Save,
    Undo,
    Test,
    Advisor,
    Update,
}

/// When the Helper VM is older than the one this app comes with: a card that says so, with Update
/// it (when it can update itself) or the commands for its terminal; and the update under way, or
/// how it went. Whether Update it was clicked.
pub(super) fn update_card(ui: &mut Ui, pal: &Pal, st: &VmStatus, v: &HelperView) -> bool {
    let old = crate::helper::older(&st.version, crate::helper::LATEST);
    if !old && v.asked_update.is_none() && v.update_note.is_none() {
        return false;
    }
    let mut clicked = false;
    let (fill, edge, soft) = if pal.dark {
        (Color32::from_rgb(25, 36, 52), Color32::from_rgb(42, 72, 112), Color32::from_rgb(150, 170, 198))
    } else {
        (Color32::from_rgb(237, 244, 254), Color32::from_rgb(176, 204, 240), Color32::from_rgb(68, 96, 138))
    };
    egui::Frame::none().fill(fill).stroke(Stroke::new(1.0_f32, edge)).rounding(Rounding::same(12.0)).inner_margin(egui::Margin::symmetric(14.0, 10.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
            paint_glyph(ui.painter(), r, "download", pal.accent);
            ui.add_space(4.0);
            let room = (ui.available_width() - 170.0).max(160.0);
            ui.vertical(|ui| {
                ui.set_max_width(room);
                ui.spacing_mut().item_spacing.y = 2.0;
                let title = if old { format!("A new version of the Helper VM: {}", crate::helper::LATEST) } else { "The Helper VM is up to date".to_string() };
                ui.label(egui::RichText::new(title).font(FontId::new(13.5, FontFamily::Name("bold".into()))).color(pal.ink));
                let said = if v.asked_update.is_some() {
                    "It's downloading it from GitHub and installing it: a minute or two.".to_string()
                } else if old && st.updates {
                    format!("It has {}. This version of Simple VM Manager comes with {}: Update it has the VM install it by itself.", st.version, crate::helper::LATEST)
                } else if old {
                    format!("It has {}. This once, run setup.sh on it again (your answers stay): from then on, it updates from here.", st.version)
                } else {
                    String::new()
                };
                if !said.is_empty() {
                    ui.add(egui::Label::new(egui::RichText::new(said).size(12.5).color(soft)).wrap());
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if v.asked_update.is_some() {
                    ui.add(egui::Spinner::new().size(14.0));
                } else if old && st.updates && dialog_button(ui, pal, "download", "Update it", Look::Primary, true) {
                    clicked = true;
                }
            });
        });
        if let Some((ok, text)) = &v.update_note {
            ui.add(egui::Label::new(egui::RichText::new(text.as_str()).size(12.5).color(if *ok { pal.green } else { pal.red })).wrap());
        }
        if old && !st.updates && v.asked_update.is_none() {
            ui.add_space(4.0);
            for c in crate::helper::setup_commands() {
                command_row(ui, pal, &c);
            }
        }
    });
    clicked
}

/// What a test alert's answer says: (fine, what).
fn test_note(results: &[(String, String)]) -> (bool, String) {
    if results.is_empty() {
        (false, "No channel is on to send it to: turn one on and save first.".into())
    } else if results.iter().all(|(_, r)| r == "ok") {
        (true, format!("Sent to {}: look there.", results.iter().map(|(k, _)| channel_name(k)).collect::<Vec<_>>().join(", ")))
    } else {
        (false, results.iter().map(|(k, r)| format!("{}: {}", channel_name(k), if r == "ok" { "sent" } else { r.as_str() })).collect::<Vec<_>>().join("  ·  "))
    }
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

/// How long ago, in words: "20 s ago", "3 min ago", "2 h ago", "3 days ago".
pub(super) fn ago(s: i64) -> String {
    match s.max(0) {
        s if s < 90 => format!("{} s ago", s),
        s if s < 5400 => format!("{} min ago", s / 60),
        s if s < 2 * 86400 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86400),
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

/// A command to type, in a box, with Copy beside it.
fn command_row(ui: &mut Ui, pal: &Pal, command: &str) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2((ui.available_width() - 80.0).max(200.0), 32.0), Sense::hover());
        ui.painter().rect(r, Rounding::same(6.0), if pal.dark { Color32::from_rgb(15, 16, 19) } else { Color32::from_rgb(244, 245, 247) }, Stroke::new(1.0_f32, pal.border));
        let (g, _) = super::overview::fit(ui.painter(), command, FontId::monospace(12.0), pal.ink, r.width() - 20.0);
        ui.painter().galley(pos2(r.min.x + 10.0, r.center().y - g.size().y / 2.0), g, pal.ink);
        if flat_button(ui, "copy", "Copy", None, true, false, pal).on_hover_text(command).clicked() {
            ui.ctx().copy_text(command.to_string());
        }
    });
}

/// A line of the Helper VM's panel: a dot of its colour, a name, what it says; `link` at its
/// right (whether that was clicked).
fn status_row(ui: &mut Ui, pal: &Pal, dot: Color32, name: &str, text: &str, link: Option<&str>, last: bool) -> bool {
    let mut clicked = false;
    ui.add_space(9.0);
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        let (r, _) = ui.allocate_exact_size(vec2(10.0, 18.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 4.0, dot);
        ui.add_space(6.0);
        let (r, _) = ui.allocate_exact_size(vec2(118.0, 18.0), Sense::hover());
        ui.painter().text(pos2(r.min.x, r.center().y), Align2::LEFT_CENTER, name, FontId::proportional(13.5), pal.ink_soft);
        let room = (ui.available_width() - if link.is_some() { 150.0 } else { 16.0 }).max(120.0);
        ui.vertical(|ui| {
            ui.set_max_width(room);
            ui.add(egui::Label::new(egui::RichText::new(text).size(13.5).color(pal.ink)).wrap());
        });
        if let Some(l) = link {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(16.0);
                clicked = ui.add(egui::Label::new(egui::RichText::new(format!("{}  ›", l)).size(13.0).color(pal.link)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
            });
        }
    });
    ui.add_space(9.0);
    if !last {
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
        ui.painter().line_segment([pos2(r.min.x + 16.0, r.center().y), pos2(r.max.x, r.center().y)], Stroke::new(1.0_f32, pal.border.gamma_multiply(0.6)));
    }
    clicked
}

/// The Helper VM: how it's doing, as its heartbeat says; or what it is, and how to set one up.
fn vm_card(ui: &mut Ui, pal: &Pal, v: &HelperView, now: i64, act: &mut Option<HelperAct>) {
    let st = match &v.vm {
        None => {
            soft(ui, pal, "Looking for this pool's Helper VM…");
            return;
        }
        Some(Err(e)) => {
            ui.add(egui::Label::new(egui::RichText::new(format!("Couldn't read the pool's settings: {}", e)).size(13.5).color(pal.red)).wrap());
            return;
        }
        Some(Ok(Some(st))) => st,
        Some(Ok(None)) => {
            soft(ui, pal, "A small Linux VM in this pool that looks after it, also while this app is closed: the pool's alerts on your phone (Telegram, ntfy, Discord, Home Assistant or any webhook), any storage nearly full, missed snapshot schedule runs made up, the NAS mounted again after a server started before it, VMs started in order, a weekly review of the pool, and a daily check of good practice for the Advisor, in Claude's words. Nothing is installed on the servers.");
            ui.add_space(14.0);
            boxed(ui, pal, |ui| {
                ui.label(egui::RichText::new("Set it up").font(FontId::new(14.0, FontFamily::Name("bold".into()))).color(pal.ink));
                ui.add_space(6.0);
                soft_small(ui, pal, "1.  Make a small VM in this pool: Debian 12 (or Ubuntu, Fedora, Rocky, Arch), with 1 vCPU, 1 GB of memory and 8 GB of disk.");
                ui.add_space(6.0);
                soft_small(ui, pal, "2.  In its terminal, run these two commands:");
                ui.add_space(6.0);
                for c in crate::helper::setup_commands() {
                    command_row(ui, pal, &c);
                    ui.add_space(4.0);
                }
                ui.add_space(4.0);
                soft_small(ui, pal, "3.  Answer its questions: this pool's address and root password, where alerts go, and Claude (your Claude subscription, at no extra cost, by scanning a QR code with your phone).");
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("It's free software (MIT), like this app, given as it is, with no warranty: read it before you run it.").size(12.5).color(pal.ink_soft));
                    if ui.add(egui::Label::new(egui::RichText::new("Read about it on GitHub  ›").size(12.5).color(pal.link)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        open_url(&crate::helper::about_url());
                    }
                });
            });
            ui.add_space(8.0);
            soft_small(ui, pal, "This page shows it as soon as it has looked at the pool once.");
            return;
        }
    };
    let (green, orange, grey) = (pal.green, pal.orange, pal.ink_faint);
    super::overview::section(ui, pal, "Helper VM");
    super::overview::panel(ui, pal, |ui| {
        let at = if st.address.is_empty() { String::new() } else { format!(" ({})", st.address) };
        let looked = if st.fresh(now) { format!("looked {}", ago(now - st.last_look)) } else { format!("not heard from for {}: is the VM running?", ago(now - st.last_look).trim_end_matches(" ago")) };
        status_row(ui, pal, if st.fresh(now) { green } else { orange }, "Machine", &format!("{}{}, version {}: {}", st.machine, at, st.version, looked), None, false);
        let claude = if st.claude { "On: it explains serious alerts, writes the weekly review, and words the Advisor's checks" } else { "Off: its review and checks are in plain words ('sudo svm-helper setup' on the VM turns Claude on)" };
        status_row(ui, pal, if st.claude { green } else { grey }, "Claude", claude, None, false);
        let review = match st.review {
            (false, _) => "Off".to_string(),
            (true, 0) => "On: none written yet".to_string(),
            (true, last) => format!("On: the last one {}", ago(now - last)),
        };
        status_row(ui, pal, if st.review.0 { green } else { grey }, "Weekly review", &review, None, false);
        let (on, last, _, bad, warn) = st.advice;
        let advisor = if !st.requests {
            "Comes with version 2.2: run setup.sh on the VM again to update it".to_string()
        } else if last == 0 {
            "Not checked yet".to_string()
        } else {
            format!("Checked {}: {} to fix, {} to look at", ago(now - last), bad, warn)
        };
        let dot = if bad > 0 { pal.red } else if warn > 0 { orange } else if on && last > 0 { green } else { grey };
        let last_row = st.watching == 0;
        if status_row(ui, pal, dot, "Advisor", &advisor, st.requests.then_some("Open the Advisor"), last_row) {
            *act = Some(HelperAct::Advisor);
        }
        if st.watching > 0 {
            status_row(ui, pal, green, "Watching", &format!("{} service{} of yours", st.watching, if st.watching == 1 { "" } else { "s" }), None, true);
        }
    });
    ui.add_space(10.0);
    if update_card(ui, pal, st, v) {
        *act = Some(HelperAct::Update);
    }
}

fn settings_ui(ui: &mut Ui, pal: &Pal, v: &mut HelperView, d: &PoolData, can_ask: bool, act: &mut Option<HelperAct>) {
    let busy = v.busy.is_some();
    let testing = v.asked_test.is_some();
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
                let (r, _) = ui.allocate_exact_size(vec2(28.0, 34.0), Sense::hover());
                channel_logo(ui.painter(), r.center(), name);
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
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
                        ui.add_space(52.0);
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
    // which alerts, and when not
    ui.add_space(20.0);
    super::overview::section(ui, pal, "Which ones, and when");
    super::overview::panel(ui, pal, |ui| {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                ui.label(egui::RichText::new("Which alerts").size(14.0).color(pal.ink));
                ui.label(egui::RichText::new("As bad as these, or worse").size(12.0).color(pal.ink_soft));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(16.0);
                let mut at = match x.min_priority {
                    p if p <= 2 => 0,
                    3 | 4 => 1,
                    _ => 2,
                };
                if crate::isolib::segmented(ui, pal, &["Critical only", "And warnings", "Everything"], &mut at, 330.0_f32.min(ui.available_width() - 20.0)) {
                    x.min_priority = [2, 3, 5][at];
                }
            });
        });
        ui.add_space(8.0);
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
        ui.painter().line_segment([pos2(r.min.x + 16.0, r.center().y), pos2(r.max.x, r.center().y)], Stroke::new(1.0_f32, pal.border.gamma_multiply(0.6)));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                ui.label(egui::RichText::new("Quiet hours").size(14.0).color(pal.ink));
                ui.label(egui::RichText::new("Only critical alerts then").size(12.0).color(pal.ink_soft));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(16.0);
                let tip = if x.quiet.0 { "On: only critical alerts in these hours" } else { "Off" };
                switch(ui, pal, &mut x.quiet.0, tip);
                ui.add_space(14.0);
                // (right to left: "from 23:00 to 07:00" reads in the usual order)
                let on = x.quiet.0;
                ui.add_enabled_ui(on, |ui| {
                    text_field(ui, pal, &mut x.quiet.2, 58.0, false);
                    ui.label(egui::RichText::new("to").size(13.0).color(pal.ink_soft));
                    text_field(ui, pal, &mut x.quiet.1, 58.0, false);
                    ui.label(egui::RichText::new("from").size(13.0).color(pal.ink_soft));
                });
            });
        });
        ui.add_space(8.0);
    });
    // a test, of the channels as saved
    ui.add_space(10.0);
    let changed = v.saved.as_ref().and_then(|r| r.as_ref().ok()) != Some(&*x);
    ui.horizontal(|ui| {
        let why_not = if testing {
            Some("The Helper VM sends it within a minute…")
        } else if !can_ask {
            Some("Update the Helper VM first to send one from here.")
        } else if changed {
            Some("Save first: it sends one to the channels as saved.")
        } else if !x.sends() {
            Some("Turn a channel on first.")
        } else {
            None
        };
        if dialog_button(ui, pal, "bell", if testing { "Sending…" } else { "Send a test alert" }, Look::Plain, why_not.is_none() && !busy) {
            *act = Some(HelperAct::Test);
        }
        if let Some(w) = why_not {
            ui.label(egui::RichText::new(w).size(12.5).color(pal.ink_soft));
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
    // changed since the last save: that, rather than what the save said
    if changed && v.busy != Some(Did::Saved) {
        ui.add_space(8.0);
        ui.add(egui::Label::new(egui::RichText::new("Not saved yet: Save puts these changes in place, Undo changes goes back to what's saved.").size(12.5).color(pal.orange)).wrap());
    } else if let Some((ok, text)) = &v.note {
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
            ui.spacing_mut().item_spacing.y = 3.0;
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
    let bold = FontId::new(12.0, FontFamily::Name("bold".into()));
    for (i, (uuid, wait)) in x.order.iter_mut().enumerate() {
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
        ui.painter().line_segment([pos2(row.min.x + 52.0, row.center().y), pos2(row.max.x, row.center().y)], Stroke::new(1.0_f32, pal.border.gamma_multiply(0.5)));
        ui.horizontal(|ui| {
            // (as tall as its boxes from the start, so everything sits level)
            ui.set_min_height(38.0);
            ui.add_space(16.0);
            // its place: a number in a round badge
            let (r, _) = ui.allocate_exact_size(vec2(28.0, 22.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 11.0, pal.accent.gamma_multiply(0.18));
            ui.painter().text(r.center(), Align2::CENTER_CENTER, (i + 1).to_string(), bold.clone(), pal.accent);
            ui.add_space(8.0);
            ui.label(egui::RichText::new(name_of(uuid)).size(14.0).color(pal.ink));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(12.0);
                if order_button(ui, pal, "×", true, "Take it off the list (it starts by itself again)") {
                    drop = Some(i);
                }
                if order_button(ui, pal, "↓", i + 1 < n, "Later") {
                    swap = Some((i, i + 1));
                }
                if order_button(ui, pal, "↑", i > 0, "Sooner") {
                    swap = Some((i - 1, i));
                }
                ui.add_space(12.0);
                // the wait before the next one (none after the last)
                if i + 1 < n {
                    ui.label(egui::RichText::new("s before the next").size(12.5).color(pal.ink_soft));
                    let mut text = wait.to_string();
                    if ui.add(egui::TextEdit::singleline(&mut text).desired_width(38.0).horizontal_align(egui::Align::Center)).on_hover_text("Seconds to wait after this one starts").changed() {
                        if let Ok(w) = text.trim().parse::<i64>() {
                            *wait = w.clamp(0, 3600);
                        }
                    }
                    ui.label(egui::RichText::new("then").size(12.5).color(pal.ink_soft));
                }
            });
        });
    }
    if let Some((a, b)) = swap {
        x.order.swap(a, b);
    }
    if let Some(i) = drop {
        x.order.remove(i);
    }
    let left: Vec<&VmInfo> = d.vms.iter().filter(|v| !x.order.iter().any(|o| o.0 == v.uuid)).collect();
    if n > 0 {
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
        ui.painter().line_segment([pos2(row.min.x + 52.0, row.center().y), pos2(row.max.x, row.center().y)], Stroke::new(1.0_f32, pal.border.gamma_multiply(0.5)));
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add_space(52.0);
        egui::ComboBox::from_id_salt("helper-order-add").selected_text(if x.order.is_empty() { "Add the first VM…" } else { "Add a VM…" }).width(260.0).show_ui(ui, |ui| {
            for v in &left {
                if ui.selectable_label(false, v.name.as_str()).clicked() {
                    x.order.push((v.uuid.clone(), 30));
                }
            }
        });
    });
    ui.add_space(12.0);
}

/// A small square button with a sign on it (↑, ↓, ×), lit up under the mouse; whether it was
/// clicked.
fn order_button(ui: &mut Ui, pal: &Pal, sign: &str, enabled: bool, tip: &str) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(26.0, 26.0), if enabled { Sense::click() } else { Sense::hover() });
    if enabled && resp.hovered() {
        ui.painter().rect_filled(r, Rounding::same(6.0), pal.hover);
    }
    let col = if enabled { pal.ink_soft } else { pal.ink_faint.gamma_multiply(0.5) };
    ui.painter().text(r.center(), Align2::CENTER_CENTER, sign, FontId::proportional(15.0), col);
    enabled && resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
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

/// The pool Overview's card while the pool has no Helper VM: what it does, a way to set it up
/// (Some(true)) and an X to put it away (Some(false)).
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
                ui.label(egui::RichText::new("No Helper VM looks after this pool").font(FontId::new(13.5, FontFamily::Name("bold".into()))).color(pal.ink));
                ui.add(egui::Label::new(egui::RichText::new("Alerts on your phone, a weekly review, the Advisor's daily check of good practice: also while this app is closed. A small VM in the pool does it; nothing goes on the servers.").size(12.5).color(soft)).wrap());
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

/// A channel's little logo, drawn (no images): Telegram's paper plane, ntfy's message, Discord's
/// face, Home Assistant's house, a webhook's braces; each on its own colour.
fn channel_logo(p: &egui::Painter, c: egui::Pos2, name: &str) {
    let white = Color32::WHITE;
    let at = |x: f32, y: f32| c + vec2(x, y);
    let poly = |pts: &[(f32, f32)], col: Color32| egui::Shape::convex_polygon(pts.iter().map(|&(x, y)| at(x, y)).collect(), col, Stroke::NONE);
    match name {
        "Telegram" => {
            p.circle_filled(c, 13.0, Color32::from_rgb(42, 171, 238));
            p.add(poly(&[(-7.0, -0.4), (6.4, -6.0), (3.4, 6.4), (-0.4, 2.6)], white));
            p.add(poly(&[(-0.4, 2.6), (3.4, 6.4), (0.8, 2.0)], Color32::from_rgb(200, 226, 242)));
        }
        "ntfy" => {
            let bg = Color32::from_rgb(51, 133, 116);
            p.rect_filled(egui::Rect::from_center_size(c, vec2(26.0, 26.0)), Rounding::same(7.0), bg);
            p.rect_filled(egui::Rect::from_min_max(at(-7.5, -6.0), at(7.5, 4.0)), Rounding::same(3.0), white);
            p.add(poly(&[(-5.5, 3.5), (-1.5, 3.5), (-5.0, 7.5)], white));
            let s = Stroke::new(1.6_f32, bg);
            p.line_segment([at(-4.5, -2.6), at(4.5, -2.6)], s);
            p.line_segment([at(-4.5, 0.6), at(1.5, 0.6)], s);
        }
        "Discord" => {
            let bg = Color32::from_rgb(88, 101, 242);
            p.rect_filled(egui::Rect::from_center_size(c, vec2(26.0, 26.0)), Rounding::same(7.0), bg);
            p.rect_filled(egui::Rect::from_min_max(at(-7.5, -4.5), at(7.5, 5.0)), Rounding::same(4.5), white);
            p.add(poly(&[(-7.5, -1.0), (-5.0, -6.0), (-2.5, -4.5)], white));
            p.add(poly(&[(7.5, -1.0), (5.0, -6.0), (2.5, -4.5)], white));
            p.circle_filled(at(-2.7, 0.6), 1.7, bg);
            p.circle_filled(at(2.7, 0.6), 1.7, bg);
        }
        "Home Assistant" => {
            let bg = Color32::from_rgb(24, 188, 242);
            p.circle_filled(c, 13.0, bg);
            p.add(poly(&[(0.0, -8.0), (8.0, -0.5), (-8.0, -0.5)], white));
            p.rect_filled(egui::Rect::from_min_max(at(-5.5, -1.0), at(5.5, 7.0)), Rounding::same(0.5), white);
            let s = Stroke::new(1.4_f32, bg);
            p.line_segment([at(0.0, -0.5), at(0.0, 7.0)], s);
            p.line_segment([at(0.0, 3.2), at(-2.4, 1.2)], s);
            p.line_segment([at(0.0, 4.6), at(2.4, 2.6)], s);
            p.circle_filled(at(0.0, -0.8), 1.5, bg);
        }
        _ => {
            p.rect_filled(egui::Rect::from_center_size(c, vec2(26.0, 26.0)), Rounding::same(7.0), Color32::from_rgb(124, 92, 214));
            p.text(c + vec2(0.0, -0.5), Align2::CENTER_CENTER, "{ }", FontId::new(13.0, FontFamily::Name("bold".into())), white);
        }
    }
}
