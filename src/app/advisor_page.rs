//! The Advisor page (the toolbar's Advisor): each pool's checklist of good practice, as its Helper
//! VM left it in the pool (see helper.rs): by topic, each check with how it went; a click shows
//! why it matters and what to do, in Claude's words when the VM has Claude. Check now asks for a
//! new one.

use super::*;
use crate::helper::{Check, Fix, Status, TOPICS};

impl App {
    pub(super) fn advisor_page(&mut self, ui: &mut Ui, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let ctx = ui.ctx().clone();
        let pools: Vec<(u32, String)> = self.servers.iter().filter_map(|s| s.data.as_ref().map(|d| (s.id, d.name.clone()))).collect();
        for (sid, _) in &pools {
            self.helper_poll(*sid, &ctx);
        }
        let now = unix_now();
        let many = pools.len() > 1;
        let this = &*self;
        egui::ScrollArea::vertical().id_salt("advisor-page").auto_shrink([false; 2]).show(ui, |ui| {
            let pad = 28.0;
            let w = (ui.available_width() - 2.0 * pad).min(860.0);
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                ui.add_space(pad);
                ui.vertical(|ui| {
                    ui.set_width(w);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    if pools.is_empty() {
                        soft(ui, &pal, "Connect to a pool first: its Helper VM checks it against good practice every day, and this page shows what it found.");
                    }
                    for (i, (sid, name)) in pools.iter().enumerate() {
                        if i > 0 {
                            ui.add_space(34.0);
                        }
                        let d = this.server(*sid).and_then(|s| s.data.as_ref());
                        pool_checklist(ui, &pal, *sid, name, many, this.helper.get(sid), d, now, out);
                    }
                    ui.add_space(28.0);
                });
            });
        });
    }
}

/// When it was checked, in words: "today at 08:02", "yesterday at 08:02", "on 19 Nov 2026 at
/// 08:02" (this computer's time).
fn when(t: i64, now: i64) -> String {
    let off = local_offset(t);
    let day = |x: i64| (x + local_offset(x)).div_euclid(86_400);
    let hm = fmt_clock(t, off, false)[..5].to_string();
    match day(now) - day(t) {
        0 => format!("today at {}", hm),
        1 => format!("yesterday at {}", hm),
        _ => format!("on {}", fmt_date_time(t, off).replace(", ", " at ")),
    }
}

fn soft(ui: &mut Ui, pal: &Pal, text: &str) {
    ui.add(egui::Label::new(egui::RichText::new(text).size(13.5).color(pal.ink_soft)).wrap());
}

/// A pool's checklist: when it was checked and Check now, then its checks by topic; or why
/// there's none.
#[allow(clippy::too_many_arguments)]
fn pool_checklist(ui: &mut Ui, pal: &Pal, sid: u32, name: &str, many: bool, v: Option<&super::helper_page::HelperView>, d: Option<&PoolData>, now: i64, out: &mut Vec<Intent>) {
    if many {
        ui.label(egui::RichText::new(name).font(FontId::new(17.0, FontFamily::Name("bold".into()))).color(pal.ink));
        ui.add_space(8.0);
    }
    let Some(v) = v else {
        soft(ui, pal, "Looking for its Helper VM…");
        return;
    };
    match &v.vm {
        None => {
            soft(ui, pal, "Looking for its Helper VM…");
            return;
        }
        Some(Err(e)) => {
            ui.add(egui::Label::new(egui::RichText::new(format!("Couldn't read the pool's settings: {}", e)).size(13.5).color(pal.red)).wrap());
            return;
        }
        Some(Ok(None)) => {
            no_helper(ui, pal, sid, out);
            return;
        }
        Some(Ok(Some(_))) => {}
    }
    let Some(st) = v.status() else { return };
    let checking = v.asked_advice.is_some();
    if super::helper_page::update_card(ui, pal, st, v) {
        out.push(Intent::HelperUpdate(sid));
    }
    if crate::helper::older(&st.version, crate::helper::LATEST) || v.asked_update.is_some() || v.update_note.is_some() {
        ui.add_space(12.0);
    }
    // when it was checked, how it went, and Check now
    ui.horizontal(|ui| {
        ui.set_min_height(32.0);
        let (said, tone) = match &v.advice {
            Some(a) => {
                let (bad, warn) = a.counts();
                let how = match (bad, warn) {
                    (0, 0) => "all fine".to_string(),
                    (0, w) => format!("{} to look at", w),
                    (b, 0) => format!("{} to fix", b),
                    (b, w) => format!("{} to fix, {} to look at", b, w),
                };
                (format!("Checked {}{}  ·  {}", when(a.at, now), if a.by_claude { ", in Claude's words" } else { "" }, how), if bad > 0 { pal.red } else if warn > 0 { pal.orange } else { pal.green })
            }
            None => ("Not checked yet".to_string(), pal.ink_soft),
        };
        let (r, _) = ui.allocate_exact_size(vec2(10.0, 18.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 4.0, tone);
        ui.add_space(4.0);
        ui.label(egui::RichText::new(said).size(13.5).color(pal.ink));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let tip = if !st.requests {
                "Update its Helper VM first: run setup.sh on it again"
            } else {
                "Has the Helper VM check the pool now, rather than at its daily time"
            };
            if flat_button(ui, "rescan", if checking { "Checking…" } else { "Check now" }, Some(pal.accent), st.requests && !checking, false, pal).on_hover_text(tip).clicked() {
                out.push(Intent::AdvisorCheck(sid));
            }
        });
    });
    if checking {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(13.0));
            ui.label(egui::RichText::new(if st.claude { "The Helper VM is checking the pool, and Claude puts it in words: a minute or two…" } else { "The Helper VM is checking the pool: within a minute…" }).size(12.5).color(pal.ink_soft));
        });
    }
    if let Some(n) = &v.advice_note {
        ui.add_space(6.0);
        ui.add(egui::Label::new(egui::RichText::new(n.as_str()).size(12.5).color(pal.red)).wrap());
    }
    if !st.fresh(now) {
        ui.add_space(6.0);
        ui.add(egui::Label::new(egui::RichText::new(format!("The Helper VM last looked at the pool {}: is it running?", super::helper_page::ago(now - st.last_look))).size(12.5).color(pal.orange)).wrap());
    }
    let Some(a) = &v.advice else {
        ui.add_space(10.0);
        soft(ui, pal, "It checks the pool once a day. Check now has it look at once.");
        return;
    };
    for (key, title) in TOPICS {
        let checks: Vec<&Check> = a.checks.iter().filter(|c| c.topic == key).collect();
        if checks.is_empty() {
            continue;
        }
        ui.add_space(18.0);
        super::overview::section(ui, pal, title);
        super::overview::panel(ui, pal, |ui| {
            for (i, c) in checks.iter().enumerate() {
                check_row(ui, pal, sid, c, v, d, out, i + 1 == checks.len());
            }
        });
    }
}

/// Where the pool has no Helper VM: what the Advisor needs, and a way to set one up.
fn no_helper(ui: &mut Ui, pal: &Pal, sid: u32, out: &mut Vec<Intent>) {
    egui::Frame::none().fill(pal.surface).stroke(Stroke::new(1.0_f32, pal.border.gamma_multiply(0.7))).rounding(Rounding::same(12.0)).inner_margin(egui::Margin::symmetric(16.0, 14.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.label(egui::RichText::new("No Helper VM looks after this pool yet").font(FontId::new(14.0, FontFamily::Name("bold".into()))).color(pal.ink));
        soft(ui, pal, "The Advisor's checklist comes from a Helper VM: a small Linux VM in the pool that checks it every day against good practice (backups, room on the storage, the servers' clocks and updates, VM Tools, certificates…) and, with Claude, says why each thing matters and what to do.");
        ui.add_space(4.0);
        if ui.add(egui::Label::new(egui::RichText::new("Set it up on the pool's Helper tab  ›").size(13.0).color(pal.link)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            out.push(Intent::Select(format!("srv:{}", sid)));
            out.push(Intent::Tab(Tab::Helper));
        }
    });
}

/// A check's mark: a round badge in its colour, a tick, an exclamation mark or a cross.
fn mark(p: &egui::Painter, c: egui::Pos2, st: Status, pal: &Pal) {
    let col = match st {
        Status::Ok => pal.green,
        Status::Warn => pal.orange,
        Status::Bad => pal.red,
    };
    p.circle_filled(c, 8.5, col);
    let s = Stroke::new(1.8_f32, Color32::WHITE);
    match st {
        Status::Ok => {
            p.add(egui::Shape::line(vec![c + vec2(-3.8, 0.3), c + vec2(-1.1, 2.9), c + vec2(3.9, -2.6)], s));
        }
        Status::Warn => {
            p.line_segment([c + vec2(0.0, -4.4), c + vec2(0.0, 0.9)], s);
            p.circle_filled(c + vec2(0.0, 3.9), 1.15, Color32::WHITE);
        }
        Status::Bad => {
            p.line_segment([c + vec2(-3.1, -3.1), c + vec2(3.1, 3.1)], s);
            p.line_segment([c + vec2(3.1, -3.1), c + vec2(-3.1, 3.1)], s);
        }
    }
}

/// What a fix's button says, and (for one this app does itself) what it asks before doing it:
/// (label, question, deletes something).
fn fix_words(f: &Fix) -> (&'static str, Option<&'static str>, bool) {
    match f.action.as_str() {
        "autostart" => ("Turn on Start with the server", Some("These will start with their server from now on, also after a power cut:"), false),
        "eject" => ("Eject the ISOs", Some("The ISO comes out of the DVD drive of:"), false),
        "delete_snapshots" => ("Delete these snapshots…", Some("These snapshots, and their copies of the disks, will be deleted. It can't be undone:"), true),
        "delete_disks" => ("Delete these disks…", Some("These disks, which no VM uses, will be deleted with what's on them. It can't be undone:"), true),
        "schedule" => ("Add to a schedule…", None, false),
        "updates" => ("Install the updates…", None, false),
        "renew_cert" => ("Renew the certificate…", None, false),
        "helper_tab" => ("Choose where alerts go…", None, false),
        "vm_storage" => ("Choose a default storage…", None, false),
        _ => ("", None, false),
    }
}

/// The dialog or page that does a fix this app doesn't do by itself.
fn fix_opens(sid: u32, f: &Fix, d: Option<&PoolData>, out: &mut Vec<Intent>) {
    match f.action.as_str() {
        "schedule" => {
            let Some(d) = d else { return };
            let vms: Vec<String> = f.uuids.iter().filter_map(|u| d.vms.iter().find(|v| &v.uuid == u)).map(|v| v.rref.clone()).collect();
            out.push(if d.schedules.is_empty() { Intent::NewSchedule(sid, vms) } else { Intent::PickSchedule(sid, vms) });
        }
        "updates" => out.push(Intent::PoolUpdates(Some(sid))),
        "renew_cert" => {
            if let Some(h) = d.and_then(|d| f.uuids.iter().find_map(|u| d.hosts.iter().find(|h| &h.uuid == u))) {
                out.push(Intent::AskRenewCert(sid, h.rref.clone(), false, None));
            }
        }
        "helper_tab" => {
            out.push(Intent::Select(format!("srv:{}", sid)));
            out.push(Intent::Tab(Tab::Helper));
        }
        "vm_storage" => out.push(Intent::VmStorage),
        _ => {}
    }
}

/// A check's row: its mark and title (Claude's own findings say so); a click opens why it
/// matters, what to do, what it's about, and its fix when this app has one (asked first).
#[allow(clippy::too_many_arguments)]
fn check_row(ui: &mut Ui, pal: &Pal, sid: u32, c: &Check, v: &super::helper_page::HelperView, d: Option<&PoolData>, out: &mut Vec<Intent>, last: bool) {
    let id = egui::Id::new(("advisor-open", sid, c.id.as_str()));
    let mut open = ui.data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
    let fix = c.fix.as_ref().filter(|f| !fix_words(f).0.is_empty() && c.status != Status::Ok);
    let more = !c.note.is_empty() || !c.objects.is_empty() || fix.is_some();
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), if more { Sense::click() } else { Sense::hover() });
    let p = ui.painter();
    if more && resp.hovered() {
        p.rect_filled(rect.shrink2(vec2(4.0, 1.0)), Rounding::same(8.0), pal.hover);
    }
    mark(p, pos2(rect.min.x + 25.0, rect.center().y), c.status, pal);
    let x = rect.min.x + 44.0;
    let room = rect.width() - 44.0 - if more { 44.0 } else { 16.0 } - if c.by_claude { 64.0 } else { 0.0 };
    let (g, cut) = super::overview::fit(p, &c.title, FontId::proportional(14.0), pal.ink, room);
    let tw = g.size().x;
    p.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, pal.ink);
    if c.by_claude {
        // a finding of Claude's own
        let pill = Rect::from_min_size(pos2(x + tw + 8.0, rect.center().y - 9.0), vec2(52.0, 18.0));
        p.rect(pill, Rounding::same(9.0), pal.accent.gamma_multiply(0.15), Stroke::new(1.0_f32, pal.accent.gamma_multiply(0.5)));
        p.text(pill.center(), Align2::CENTER_CENTER, "Claude", FontId::proportional(11.0), pal.accent);
    }
    if more {
        // a chevron: right when closed, down when open
        let cc = pos2(rect.max.x - 24.0, rect.center().y);
        let s = Stroke::new(1.6_f32, pal.ink_faint);
        let pts = if open { vec![cc + vec2(-4.0, -2.0), cc + vec2(0.0, 2.0), cc + vec2(4.0, -2.0)] } else { vec![cc + vec2(-2.0, -4.0), cc + vec2(2.0, 0.0), cc + vec2(-2.0, 4.0)] };
        p.add(egui::Shape::line(pts, s));
    }
    let resp = if cut { resp.on_hover_text(&c.title) } else { resp };
    if resp.on_hover_cursor(if more { egui::CursorIcon::PointingHand } else { egui::CursorIcon::Default }).clicked() {
        open = !open;
        ui.data_mut(|d| d.insert_temp(id, open));
    }
    if open && more {
        ui.horizontal(|ui| {
            ui.add_space(44.0);
            ui.vertical(|ui| {
                ui.set_max_width((ui.available_width() - 44.0).max(160.0));
                ui.spacing_mut().item_spacing.y = 6.0;
                if !c.note.is_empty() {
                    ui.add(egui::Label::new(egui::RichText::new(c.note.as_str()).size(13.0).color(pal.ink_soft)).wrap());
                }
                if !c.objects.is_empty() {
                    ui.add(egui::Label::new(egui::RichText::new(format!("About: {}", c.objects.join(", "))).size(12.5).color(pal.ink_faint)).wrap());
                }
                if let Some(f) = fix {
                    fix_ui(ui, pal, sid, c, f, v, d, out);
                }
            });
        });
        ui.add_space(12.0);
    }
    if !last {
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
        ui.painter().line_segment([pos2(r.min.x + 44.0, r.center().y), pos2(r.max.x, r.center().y)], Stroke::new(1.0_f32, pal.border.gamma_multiply(0.6)));
    }
}

/// A check's fix: its button; for one this app does itself, the question and Do it / Cancel once
/// it's clicked; then the fix under way, or how it went.
#[allow(clippy::too_many_arguments)]
fn fix_ui(ui: &mut Ui, pal: &Pal, sid: u32, c: &Check, f: &Fix, v: &super::helper_page::HelperView, d: Option<&PoolData>, out: &mut Vec<Intent>) {
    let (label, question, deletes) = fix_words(f);
    let asking_id = egui::Id::new(("advisor-confirm", sid, c.id.as_str()));
    let asking = ui.data(|x| x.get_temp::<bool>(asking_id)).unwrap_or(false);
    let busy = v.fixing.is_some();
    ui.add_space(2.0);
    if v.fixing.as_deref() == Some(c.id.as_str()) {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(13.0));
            ui.label(egui::RichText::new("Doing it…").size(12.5).color(pal.ink_soft));
        });
        return;
    }
    if let Some((_, ok, text)) = v.fix_note.as_ref().filter(|n| n.0 == c.id) {
        ui.add(egui::Label::new(egui::RichText::new(text.as_str()).size(12.5).color(if *ok { pal.green } else { pal.red })).wrap());
        if *ok {
            return;
        }
    }
    if !asking {
        ui.horizontal(|ui| {
            if flat_button(ui, if deletes { "trash" } else { "wrench" }, label, Some(if deletes { pal.red } else { pal.accent }), !busy, false, pal).clicked() {
                if question.is_some() {
                    ui.data_mut(|x| x.insert_temp(asking_id, true));
                } else {
                    fix_opens(sid, f, d, out);
                }
            }
        });
        return;
    }
    // asked first: what it does, to what
    egui::Frame::none().fill(pal.surface).stroke(Stroke::new(1.0_f32, if deletes { pal.red.gamma_multiply(0.6) } else { pal.border })).rounding(Rounding::same(10.0)).inner_margin(egui::Margin::symmetric(12.0, 10.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.add(egui::Label::new(egui::RichText::new(question.unwrap_or_default()).size(13.0).color(pal.ink)).wrap());
        let shown = f.names.len().min(12);
        for n in &f.names[..shown] {
            ui.label(egui::RichText::new(format!("•  {}", n)).size(12.5).color(pal.ink_soft));
        }
        if f.names.len() > shown {
            ui.label(egui::RichText::new(format!("…and {} more", f.names.len() - shown)).size(12.5).color(pal.ink_soft));
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let look = if deletes { Look::Danger } else { Look::Primary };
            if dialog_button(ui, pal, if deletes { "trash" } else { "check" }, if deletes { "Delete" } else { "Do it" }, look, !busy) {
                ui.data_mut(|x| x.remove::<bool>(asking_id));
                out.push(Intent::AdvisorFix(sid, c.id.clone(), f.clone()));
            }
            if dialog_button(ui, pal, "close", "Cancel", Look::Plain, true) {
                ui.data_mut(|x| x.remove::<bool>(asking_id));
            }
        });
    });
}
