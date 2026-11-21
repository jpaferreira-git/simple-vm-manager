//! Snapshot schedules (XenServer's VMSS): a pool's list of them (the Snapshots page's Schedules
//! tab), one schedule's settings and VMs (New Schedule, Edit), and choosing the schedule some
//! VMs are on (Schedule…, on the page for the VMs ticked, or a VM's Snapshots tab).

use crate::app::{dialog_button, dialog_buttons, dialog_window, field_label, text_field, Look, LABEL_WIDTH};
use crate::model::*;
use crate::newvm::{combo, combo_style};
use crate::theme::Pal;
use eframe::egui::{self, pos2, vec2, Color32, Rect, Rounding, Sense, Stroke, Ui};
use std::collections::HashSet;

/// A pool, as the schedule dialogs see it.
pub struct SchedPool<'a> {
    pub sid: u32,
    pub data: &'a PoolData,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// "Web Server", "Web Server and Database", "Web Server, Database and 3 more".
pub fn names_of(names: &[String]) -> String {
    match names.len() {
        0 => "no VMs".into(),
        1 => names[0].clone(),
        2 => format!("{} and {}", names[0], names[1]),
        3 => format!("{}, {} and {}", names[0], names[1], names[2]),
        n => format!("{}, {} and {} more", names[0], names[1], n - 2),
    }
}

fn vm_names(d: &PoolData, vms: &[String]) -> Vec<String> {
    let mut names: Vec<String> = vms.iter().filter_map(|r| d.vm(r).map(|v| v.name.clone())).collect();
    names.sort_by_key(|n| n.to_lowercase());
    names
}

/// An on/off switch, small.
fn switch(ui: &mut Ui, pal: &Pal, on: bool, enabled: bool) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(34.0, 20.0), if enabled { Sense::click() } else { Sense::hover() });
    let track = if on { pal.accent } else { pal.border_strong };
    ui.painter().rect_filled(r, Rounding::same(10.0), if enabled { track } else { track.gamma_multiply(0.5) });
    ui.painter().circle_filled(pos2(if on { r.max.x - 10.0 } else { r.min.x + 10.0 }, r.center().y), 7.5, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A small square button with a glyph (Edit, Delete); true when clicked.
pub fn glyph_button(ui: &mut Ui, pal: &Pal, glyph: &str, tip: &str, danger: bool, enabled: bool) -> bool {
    let (b, resp) = ui.allocate_exact_size(vec2(26.0, 26.0), if enabled { Sense::click() } else { Sense::hover() });
    let resp = resp.on_hover_text(tip);
    let hot = enabled && resp.hovered();
    if hot {
        let fill = if danger { pal.red.gamma_multiply(if pal.dark { 0.25 } else { 0.12 }) } else { pal.border };
        ui.painter().rect_filled(b, Rounding::same(5.0), fill);
    }
    let col = if !enabled {
        pal.ink_faint.gamma_multiply(0.45)
    } else if hot {
        if danger { pal.red } else { pal.accent }
    } else {
        pal.ink_soft
    };
    crate::icons::paint_glyph(ui.painter(), Rect::from_center_size(b.center(), vec2(16.0, 16.0)), glyph, col);
    enabled && resp.clicked()
}

fn small(ui: &mut Ui, pal: &Pal, text: &str) {
    ui.add(egui::Label::new(egui::RichText::new(text).size(12.0).color(pal.ink_soft)).wrap());
}

// ---------------------------------------------------------------------------------------------
// The list (the Snapshots page's Schedules tab)

pub enum ListAction {
    None,
    Edit(u32, String),
    Delete(u32, String),
    Enable(u32, String, bool),
    Run(u32, String),
}

/// What the server last said about a schedule (its vmss plugin's messages): when, whether its
/// run went well, and in words.
pub struct SchedNews {
    pub time: i64,
    pub ok: bool,
    pub text: String,
}

/// When it last ran, in words: "last ran today, 03:00", or that it hasn't yet.
pub fn last_ran(x: &ScheduleInfo) -> String {
    x.last_run.map_or("hasn't run yet (the server looks every 15 minutes)".to_string(), |t| format!("last ran {}", fmt_when(t, now(), local_offset(now())).to_lowercase()))
}

/// What went wrong on its last run, if the server's latest word about it is that it failed
/// (after it last ran well).
pub fn failure<'a>(x: &ScheduleInfo, news: Option<&'a SchedNews>) -> Option<&'a SchedNews> {
    news.filter(|n| !n.ok && x.last_run.map_or(true, |t| n.time >= t))
}

/// Why Run now is off for it, if it is.
pub fn run_off(x: &ScheduleInfo) -> Option<&'static str> {
    if !x.enabled {
        Some("Turn it on first: the server doesn't run a paused schedule, not even now")
    } else if x.vms.is_empty() {
        Some("It has no VMs to take snapshots of")
    } else {
        None
    }
}

/// A pool's snapshot schedules, as cards: each turned on or off, run now, edited or deleted
/// (asked about first, in place: `asking`). `busy`: schedules being saved, turned on or off,
/// deleted ("<ref>") or run ("run:<ref>").
pub fn schedule_list(ui: &mut Ui, pal: &Pal, pool: &SchedPool, busy: &HashSet<String>, news: &std::collections::HashMap<String, SchedNews>, asking: &mut Option<String>) -> ListAction {
    let mut action = ListAction::None;
    let d = pool.data;
    let sid = pool.sid;
    for x in &d.schedules {
        let working = busy.contains(&x.rref);
        let running = busy.contains(&format!("run:{}", x.rref));
        let failed = failure(x, news.get(&x.rref));
        egui::Frame::none().fill(pal.surface).stroke(Stroke::new(1.0_f32, pal.border)).rounding(Rounding::same(8.0)).inner_margin(egui::Margin::symmetric(12.0, 9.0)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.horizontal(|ui| {
                let tip = if x.enabled { "On: it takes its snapshots. Click to pause it." } else { "Paused: it takes no snapshots. Click to turn it on." };
                if switch(ui, pal, x.enabled, !working && !running).on_hover_text(tip).clicked() {
                    action = ListAction::Enable(sid, x.rref.clone(), !x.enabled);
                }
                ui.add_space(4.0);
                ui.label(egui::RichText::new(&x.name).size(14.0).color(if x.enabled { pal.ink } else { pal.ink_soft }));
                if !x.enabled {
                    ui.label(egui::RichText::new("paused").size(12.0).color(pal.orange));
                }
                if working || running {
                    ui.add(egui::Spinner::new().size(13.0));
                }
                if running {
                    ui.label(egui::RichText::new("taking its snapshots…").size(12.0).color(pal.ink_soft));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let free = !working && !running;
                    if glyph_button(ui, pal, "trash", "Delete this schedule…", true, free) {
                        *asking = Some(x.rref.clone());
                    }
                    if glyph_button(ui, pal, "edit", "Edit: when it runs, how many it keeps, its VMs…", false, free) {
                        action = ListAction::Edit(sid, x.rref.clone());
                    }
                    let tip = run_off(x).unwrap_or("Run it now: a snapshot of each of its VMs, as at its times");
                    if glyph_button(ui, pal, "camera", tip, false, free && run_off(x).is_none()) {
                        action = ListAction::Run(sid, x.rref.clone());
                    }
                });
            });
            let holds = if x.memory { "disks and memory" } else { "disks" };
            ui.label(egui::RichText::new(format!("{}  ·  {}", x.here(d).summary(), holds)).size(12.5).color(pal.ink_soft));
            let names = vm_names(d, &x.vms);
            let vms = if names.is_empty() { "No VMs on it".to_string() } else { names_of(&names) };
            ui.label(egui::RichText::new(format!("{}  ·  {}", vms, last_ran(x))).size(12.0).color(pal.ink_faint));
            if let Some(n) = failed {
                let when = fmt_when(n.time, now(), local_offset(now())).to_lowercase();
                ui.add(egui::Label::new(egui::RichText::new(format!("Its run {} failed: {}", when, n.text)).size(12.0).color(pal.red)).wrap());
            }
        });
        if asking.as_deref() == Some(x.rref.as_str()) {
            let n = x.vms.len();
            let text = match n {
                0 => "The snapshots it took stay.".to_string(),
                1 => "Its VM stops getting snapshots from it; the snapshots it took stay.".to_string(),
                _ => format!("Its {} VMs stop getting snapshots from it; the snapshots it took stay.", n),
            };
            ui.add_space(2.0);
            egui::Frame::none().fill(pal.red.gamma_multiply(if pal.dark { 0.18 } else { 0.08 })).rounding(Rounding::same(8.0)).inner_margin(egui::Margin::symmetric(12.0, 9.0)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new(format!("Delete the schedule '{}'?", x.name)).color(pal.ink));
                small(ui, pal, &text);
                ui.horizontal(|ui| {
                    if dialog_button(ui, pal, "trash", "Delete", Look::Danger, true) {
                        action = ListAction::Delete(sid, x.rref.clone());
                        *asking = None;
                    }
                    if dialog_button(ui, pal, "close", "Keep it", Look::Plain, true) {
                        *asking = None;
                    }
                });
            });
        }
        ui.add_space(6.0);
    }
    if asking.as_ref().map_or(false, |r| !d.schedules.iter().any(|x| x.rref == *r)) {
        *asking = None;
    }
    action
}

// ---------------------------------------------------------------------------------------------
// One schedule (New Schedule, Edit)

pub enum EditAction {
    None,
    Close,
    Save(u32, ScheduleInfo),
}

/// A schedule's settings and its VMs, new or to change.
pub struct ScheduleDialog {
    pub sid: u32,
    /// The schedule, its times on this computer's clock (`shift` seconds ahead of the pool's,
    /// when that's known), turned back to the pool's when saved.
    x: ScheduleInfo,
    shift: Option<i64>,
    search: String,
    focus: bool,
}

impl ScheduleDialog {
    /// For `x` (None: a new one, on the VMs `vms`), shown on this computer's clock: `shift`
    /// seconds ahead of the pool's (see `PoolData::schedule_shift`).
    pub fn new(sid: u32, x: Option<&ScheduleInfo>, vms: Vec<String>, shift: Option<i64>) -> Self {
        let x = match x {
            Some(x) => x.shifted(shift.unwrap_or(0)),
            None => ScheduleInfo { enabled: true, kind: "snapshot".into(), keep: 7, every: Every::Daily, hour: 3, min: 0, days: vec!["Sunday".into()], vms, ..Default::default() },
        };
        let focus = x.rref.is_empty();
        ScheduleDialog { sid, x, shift, search: String::new(), focus }
    }

    /// What stops it being saved.
    fn problem(&self) -> Option<&'static str> {
        if self.x.name.trim().is_empty() {
            Some("Give it a name.")
        } else if self.x.every == Every::Weekly && self.x.days.is_empty() {
            Some("Pick at least one day.")
        } else {
            None
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, pal: &Pal, pool: Option<&SchedPool>, saving: bool) -> EditAction {
        let mut action = EditAction::None;
        let title = if self.x.rref.is_empty() { "New Schedule".to_string() } else { format!("Edit '{}'", self.x.name) };
        dialog_window(ctx, &title).id(egui::Id::new("schedule-dialog")).show(ctx, |ui| {
            ui.set_width(540.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            let x = &mut self.x;
            ui.horizontal(|ui| {
                field_label(ui, "Name:", LABEL_WIDTH);
                let r = text_field(ui, pal, &mut x.name, ui.available_width() - 4.0, false);
                if self.focus {
                    r.request_focus();
                    self.focus = false;
                }
            });
            // how often, then (weekly) which days
            ui.horizontal(|ui| {
                field_label(ui, "Every:", LABEL_WIDTH);
                let mut at = match x.every {
                    Every::Hourly => 0,
                    Every::Daily => 1,
                    Every::Weekly => 2,
                };
                if crate::isolib::segmented(ui, pal, &["Hour", "Day", "Week"], &mut at, ui.available_width() - 4.0) {
                    x.every = [Every::Hourly, Every::Daily, Every::Weekly][at];
                }
            });
            if x.every == Every::Weekly {
                ui.horizontal(|ui| {
                    field_label(ui, "On:", LABEL_WIDTH);
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let w = ((ui.available_width() - 4.0 - 6.0 * 4.0) / 7.0).max(30.0);
                    for day in WEEK_DAYS {
                        let on = x.days.iter().any(|d| d == day);
                        let (r, resp) = ui.allocate_exact_size(vec2(w, 26.0), Sense::click());
                        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                        let p = ui.painter();
                        if on {
                            p.rect_filled(r, Rounding::same(6.0), pal.accent);
                        } else {
                            p.rect(r, Rounding::same(6.0), if resp.hovered() { pal.hover } else { pal.input }, Stroke::new(1.0_f32, pal.border_strong));
                        }
                        p.text(r.center(), egui::Align2::CENTER_CENTER, &day[..3], egui::FontId::proportional(12.5), if on { pal.on_accent } else { pal.ink_soft });
                        if resp.clicked() {
                            if on {
                                x.days.retain(|d| d != day);
                            } else {
                                x.days.push(day.to_string());
                            }
                        }
                    }
                });
            }
            // when: the minute past each hour, or the time of day
            ui.horizontal(|ui| {
                field_label(ui, "At:", LABEL_WIDTH);
                combo_style(ui, pal);
                if x.every != Every::Hourly {
                    combo("schedule-hour", format!("{:02}", x.hour), 64.0).show_ui(ui, |ui| {
                        for h in 0..24u32 {
                            ui.selectable_value(&mut x.hour, h, format!("{:02}", h));
                        }
                    });
                    ui.label(":");
                } else {
                    ui.label(egui::RichText::new("past each hour, minute").color(pal.ink_soft));
                }
                combo("schedule-min", format!("{:02}", x.min), 64.0).show_ui(ui, |ui| {
                    for m in [0u32, 15, 30, 45] {
                        ui.selectable_value(&mut x.min, m, format!("{:02}", m));
                    }
                });
                // on this computer's clock, and what that is on the servers'
                let note = match self.shift {
                    None => "on the pool's own clock".to_string(),
                    Some(by) => {
                        let there = x.shifted(-by);
                        if there.hour == x.hour && there.min == x.min && there.days == x.days {
                            "your time (the servers' too)".to_string()
                        } else {
                            format!("your time  ·  {} on the servers", there.at())
                        }
                    }
                };
                ui.label(egui::RichText::new(note).size(12.0).color(pal.ink_faint));
            });
            ui.horizontal(|ui| {
                field_label(ui, "Keep:", LABEL_WIDTH);
                combo_style(ui, pal);
                combo("schedule-keep", x.keep.to_string(), 64.0).show_ui(ui, |ui| {
                    for k in 1..=10i64 {
                        ui.selectable_value(&mut x.keep, k, k.to_string());
                    }
                });
                ui.label(egui::RichText::new("per VM; then the oldest it took is deleted").color(pal.ink_soft));
            });
            ui.horizontal(|ui| {
                field_label(ui, "Holds:", LABEL_WIDTH);
                let mut at = if x.memory { 1 } else { 0 };
                if crate::isolib::segmented(ui, pal, &["Disks", "Disks and memory"], &mut at, ui.available_width() - 4.0) {
                    x.memory = at == 1;
                }
            });
            if x.memory {
                ui.horizontal(|ui| {
                    ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x);
                    small(ui, pal, "Reverting to one resumes the VM where it was. Each running VM pauses while its memory is written.");
                });
            }
            ui.horizontal(|ui| {
                field_label(ui, "Description:", LABEL_WIDTH);
                text_field(ui, pal, &mut x.description, ui.available_width() - 4.0, false);
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x);
                crate::app::tick(ui, pal, &mut x.enabled, "On: it takes its snapshots (untick to pause it)");
            });
            // its VMs: ticked to be on it (one on another schedule moves here)
            if let Some(pool) = pool {
                let d = pool.data;
                let mut vms: Vec<&VmInfo> = d.vms.iter().collect();
                vms.sort_by_key(|v| v.name.to_lowercase());
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    let on = vms.iter().filter(|v| x.vms.contains(&v.rref)).count();
                    ui.label(egui::RichText::new(format!("VMS  ·  {} OF {} ON IT", on, vms.len())).size(11.0).color(pal.ink_faint));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        crate::app::search_box(ui, pal, &mut self.search, "Search VMs", 180.0);
                    });
                });
                let q = self.search.trim().to_lowercase();
                ui.add_space(4.0);
                egui::Frame::none().stroke(Stroke::new(1.0_f32, pal.border)).rounding(Rounding::same(8.0)).inner_margin(egui::Margin::symmetric(8.0, 6.0)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical().max_height(200.0).auto_shrink([false, true]).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 4.0;
                        let shown: Vec<&&VmInfo> = vms.iter().filter(|v| q.is_empty() || v.name.to_lowercase().contains(&q)).collect();
                        if shown.is_empty() {
                            ui.label(egui::RichText::new(if vms.is_empty() { "The pool has no VMs." } else { "No VMs with those words." }).color(pal.ink_faint));
                        }
                        for v in shown {
                            let mut on = x.vms.contains(&v.rref);
                            ui.horizontal(|ui| {
                                if crate::app::tick(ui, pal, &mut on, v.name.as_str()).changed() {
                                    if on {
                                        x.vms.push(v.rref.clone());
                                    } else {
                                        x.vms.retain(|r| *r != v.rref);
                                    }
                                }
                                // on another schedule: it moves here
                                let other = v.schedule.as_deref().filter(|s| *s != x.rref || x.rref.is_empty()).and_then(|s| d.schedules.iter().find(|y| y.rref == s));
                                if let Some(y) = other {
                                    let text = if on { format!("moves here from '{}'", y.name) } else { format!("on '{}'", y.name) };
                                    ui.label(egui::RichText::new(text).size(12.0).color(if on { pal.orange } else { pal.ink_faint }));
                                }
                            });
                        }
                    });
                });
            }
            let problem = self.problem();
            ui.add_space(10.0);
            dialog_buttons(ui, |ui| {
                if dialog_button(ui, pal, "close", "Cancel", Look::Plain, !saving) || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    action = EditAction::Close;
                }
                let ready = problem.is_none() && !saving;
                if dialog_button(ui, pal, "check", if saving { "Saving…" } else { "Save" }, Look::Primary, ready) {
                    // (on the pool's clock)
                    let mut x = self.x.shifted(-self.shift.unwrap_or(0));
                    x.name = x.name.trim().to_string();
                    x.description = x.description.trim().to_string();
                    action = EditAction::Save(self.sid, x);
                }
                if let Some(why) = problem {
                    ui.label(egui::RichText::new(why).size(12.5).color(pal.ink_faint));
                }
            });
        });
        action
    }
}

// ---------------------------------------------------------------------------------------------
// Choosing the schedule VMs are on (Schedule…)

pub enum PickAction {
    None,
    Close,
    Set(u32, Vec<String>, Option<String>),
    New(u32, Vec<String>),
    /// One of the pool's schedules to edit.
    Edit(u32, String),
}

/// Which schedule some VMs of a pool are on: one of its schedules, or none.
pub struct PickDialog {
    pub sid: u32,
    vms: Vec<String>,
    choice: Option<String>,
}

impl PickDialog {
    /// For the VMs `vms`; `now` the schedule they're all on, if they are.
    pub fn new(sid: u32, vms: Vec<String>, now: Option<String>) -> Self {
        PickDialog { sid, vms, choice: now }
    }

    pub fn ui(&mut self, ctx: &egui::Context, pal: &Pal, pool: Option<&SchedPool>, saving: bool) -> PickAction {
        let mut action = PickAction::None;
        dialog_window(ctx, "Snapshot Schedule").id(egui::Id::new("pick-schedule-dialog")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            let Some(pool) = pool else {
                action = PickAction::Close;
                return;
            };
            let d = pool.data;
            let names = vm_names(d, &self.vms);
            let text = if names.len() == 1 {
                format!("The snapshot schedule for '{}': it takes its snapshots by itself and keeps the last few.", names[0])
            } else {
                format!("The snapshot schedule for {} VMs ({}): it takes their snapshots by itself and keeps the last few.", names.len(), names_of(&names))
            };
            ui.add(egui::Label::new(egui::RichText::new(text).color(pal.ink_soft)).wrap());
            if let Some(why) = &d.schedules_off {
                ui.label(egui::RichText::new(why).size(12.5).color(pal.orange));
            }
            ui.radio_value(&mut self.choice, None, "None: no snapshots taken by themselves");
            for x in &d.schedules {
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.choice, Some(x.rref.clone()), x.name.as_str());
                    let off = if x.enabled { String::new() } else { "  ·  paused".to_string() };
                    ui.label(egui::RichText::new(format!("{}{}", x.here(d).summary(), off)).size(12.0).color(pal.ink_faint));
                    // its settings and VMs, to change
                    if glyph_button(ui, pal, "edit", "Edit this schedule: when, how many it keeps, its VMs…", false, !saving) {
                        action = PickAction::Edit(self.sid, x.rref.clone());
                    }
                });
            }
            ui.add_space(4.0);
            dialog_buttons(ui, |ui| {
                if dialog_button(ui, pal, "close", "Cancel", Look::Plain, !saving) || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    action = PickAction::Close;
                }
                if dialog_button(ui, pal, "check", if saving { "Saving…" } else { "Save" }, Look::Primary, !saving) {
                    action = PickAction::Set(self.sid, self.vms.clone(), self.choice.clone());
                }
                if dialog_button(ui, pal, "plus", "New Schedule…", Look::Plain, !saving && d.schedules_off.is_none()) {
                    action = PickAction::New(self.sid, self.vms.clone());
                }
            });
        });
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_said_shortly() {
        let n = |v: &[&str]| names_of(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(n(&["Web"]), "Web");
        assert_eq!(n(&["Web", "Db"]), "Web and Db");
        assert_eq!(n(&["Web", "Db", "Mail"]), "Web, Db and Mail");
        assert_eq!(n(&["Web", "Db", "Mail", "Desk", "Build"]), "Web, Db and 3 more");
    }

    #[test]
    fn schedules_in_words() {
        let mut x = ScheduleInfo { keep: 7, every: Every::Daily, hour: 3, min: 0, ..Default::default() };
        assert_eq!(x.summary(), "Daily at 03:00  ·  keeps 7");
        x.every = Every::Hourly;
        x.min = 15;
        assert_eq!(x.when(), "Every hour at :15");
        assert_eq!(x.schedule_map(), vec![("min", "15".to_string())]);
        x.every = Every::Weekly;
        (x.hour, x.min, x.days) = (22, 30, vec!["Friday".into(), "Monday".into()]);
        assert_eq!(x.when(), "Mon, Fri at 22:30");
        assert_eq!(x.schedule_map(), vec![("min", "30".to_string()), ("hour", "22".to_string()), ("days", "Monday,Friday".to_string())]);
        x.days = WEEK_DAYS.iter().map(|d| d.to_string()).collect();
        assert_eq!(x.when(), "Every day at 22:30");
        // what it holds: a checkpoint when memory, a quiesced snapshot kept as one
        assert_eq!(ScheduleInfo { memory: true, ..Default::default() }.kind_to_save(), "checkpoint");
        assert_eq!(ScheduleInfo { kind: "snapshot_with_quiesce".into(), ..Default::default() }.kind_to_save(), "snapshot_with_quiesce");
        assert_eq!(ScheduleInfo { kind: "checkpoint".into(), ..Default::default() }.kind_to_save(), "snapshot");
    }
}
