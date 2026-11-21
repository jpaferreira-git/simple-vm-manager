//! All servers' Snapshots page (the toolbar's Snapshots): every pool's VMs, with how many
//! snapshots each has, its latest and its oldest, and the schedule it's on. A click opens a VM's
//! Snapshots tab; the VMs ticked get a snapshot each, or a schedule, at once. Its second tab,
//! Schedules, lists each pool's snapshot schedules.

use super::performance::segmented;
use super::*;

/// What the Snapshots page shows.
#[derive(Default)]
pub(crate) struct SnapView {
    /// All VMs, those with snapshots, those without, those on a schedule.
    pub filter: usize,
    pub search: String,
    /// The VMs ticked (pool, VM).
    pub picked: HashSet<(u32, String)>,
}

/// A snapshot older than this many days is said to be old (in orange): it slows its VM's disks
/// a little, and holds on to everything changed since.
const OLD_DAYS: i64 = 30;

/// A VM as the page lists it.
struct Line {
    sid: u32,
    pool: String,
    vm: String,
    name: String,
    power: PowerState,
    server: String,
    count: usize,
    latest: Option<SnapshotInfo>,
    oldest: Option<i64>,
    /// Its schedule: name, when, on.
    schedule: Option<(String, String, bool)>,
}

/// "today", "1 day", "94 days".
fn age(t: i64, now: i64) -> String {
    match (now - t).max(0) / 86_400 {
        0 => "today".into(),
        1 => "1 day".into(),
        n => format!("{} days", n),
    }
}

impl App {
    /// Every pool's VMs, as the page lists them, by name. A snapshot being deleted counts no more.
    fn snapshot_lines(&self) -> Vec<Line> {
        let mut lines = vec![];
        for s in &self.servers {
            let Some(d) = s.data.as_ref() else { continue };
            for v in &d.vms {
                let snaps: Vec<&SnapshotInfo> = v.snapshots.iter().filter(|x| !self.snap_deleting.contains(&x.rref)).collect();
                let server = v.host.as_deref().or(v.home()).and_then(|h| d.host(h)).map_or("—".to_string(), |h| h.name.clone());
                let schedule = v.schedule.as_deref().and_then(|r| d.schedules.iter().find(|x| x.rref == r)).map(|x| (x.name.clone(), x.here(d).when(), x.enabled));
                lines.push(Line {
                    sid: s.id,
                    pool: s.name(),
                    vm: v.rref.clone(),
                    name: v.name.clone(),
                    power: v.power,
                    server,
                    count: snaps.len(),
                    latest: snaps.iter().filter(|x| x.time.is_some()).max_by_key(|x| x.time).map(|x| (*x).clone()),
                    oldest: snaps.iter().filter_map(|x| x.time).min(),
                    schedule,
                });
            }
        }
        lines.sort_by_key(|l| l.name.to_lowercase());
        lines
    }

    /// The Schedules tab's action next to its tabs: New Schedule (with several pools, each
    /// pool's list has its own instead).
    pub(super) fn schedules_actions(&self, ui: &mut Ui, free: Rect, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let pools: Vec<(u32, &PoolData)> = self.servers.iter().filter_map(|s| s.data.as_ref().map(|d| (s.id, d))).collect();
        if pools.len() > 1 {
            return;
        }
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(free).layout(egui::Layout::right_to_left(egui::Align::Center)));
        child.spacing_mut().item_spacing.x = 4.0;
        let off = pools.first().and_then(|p| p.1.schedules_off.clone());
        let tip = off.as_deref().unwrap_or("A schedule that takes its VMs' snapshots by itself");
        let compact = free.width() < flat_button_width(&child, "New Schedule") + 16.0 + SEP_W;
        if flat_button(&mut child, "plus", "New Schedule", Some(pal.accent), !pools.is_empty() && off.is_none(), compact, &pal).on_hover_text(tip).clicked() {
            out.extend(pools.first().map(|p| Intent::NewSchedule(p.0, vec![])));
        }
        vsep(&mut child, &pal);
    }

    /// The Schedules tab: each pool's snapshot schedules, as cards (on or off, run now, edit,
    /// delete), with what the server last said about each.
    pub(super) fn schedules_page(&mut self, ui: &mut Ui, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let pools: Vec<(u32, String)> = self.servers.iter().filter(|s| s.data.is_some()).map(|s| (s.id, s.name())).collect();
        ui.add_space(10.0);
        if pools.is_empty() {
            ui.label(egui::RichText::new("Connect to a server to see its snapshot schedules.").color(pal.ink_soft));
            return;
        }
        let mut asking = self.sched_asking.take();
        let mut acts = vec![];
        egui::ScrollArea::vertical().id_salt("schedules").auto_shrink([false; 2]).show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(900.0));
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.add(egui::Label::new(egui::RichText::new("Each schedule takes a snapshot of its VMs by itself, at its times (shown in your time; the servers keep their own clock), and keeps the last few it took, deleting the oldest. A VM is on one schedule at most. A time missed (the server off then, or the schedule changed after it) is skipped till the next one.").size(12.0).color(pal.ink_faint)).wrap());
            ui.add_space(8.0);
            for (sid, name) in &pools {
                let Some(d) = self.server(*sid).and_then(|s| s.data.as_ref()) else { continue };
                if pools.len() > 1 {
                    // the pool's name, and its own New Schedule
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(name.as_str()).size(14.0).color(pal.ink));
                        ui.add_space(6.0);
                        if flat_button(ui, "plus", "New Schedule", Some(pal.accent), d.schedules_off.is_none(), false, &pal).clicked() {
                            acts.push(Intent::NewSchedule(*sid, vec![]));
                        }
                    });
                }
                if let Some(why) = &d.schedules_off {
                    ui.label(egui::RichText::new(why.as_str()).size(12.5).color(pal.orange));
                }
                if d.schedules.is_empty() {
                    ui.add_space(14.0);
                    ui.vertical_centered(|ui| {
                        ui.set_max_width(420.0);
                        ui.label(egui::RichText::new("No snapshot schedules yet").size(15.0).color(pal.ink));
                        ui.add_space(4.0);
                        ui.add(egui::Label::new(egui::RichText::new("A schedule takes its VMs' snapshots by itself, every hour, day or week, and keeps the last few.").color(pal.ink_soft)).wrap());
                        ui.add_space(10.0);
                        if pools.len() == 1 && flat_button(ui, "plus", "New Schedule", Some(pal.accent), d.schedules_off.is_none(), false, &pal).clicked() {
                            acts.push(Intent::NewSchedule(*sid, vec![]));
                        }
                    });
                    ui.add_space(14.0);
                    continue;
                }
                let news = self.schedule_news(*sid);
                let pool = SchedPool { sid: *sid, data: d };
                match crate::vmss::schedule_list(ui, &pal, &pool, &self.sched_busy, &news, &mut asking) {
                    ListAction::Edit(sid, x) => acts.push(Intent::EditSchedule(sid, x)),
                    ListAction::Delete(sid, x) => acts.push(Intent::DeleteSchedule(sid, x)),
                    ListAction::Enable(sid, x, on) => acts.push(Intent::EnableSchedule(sid, x, on)),
                    ListAction::Run(sid, x) => acts.push(Intent::RunSchedule(sid, x)),
                    ListAction::None => {}
                }
                ui.add_space(8.0);
            }
        });
        self.sched_asking = asking;
        out.extend(acts);
    }

    pub(super) fn snapshots_page(&mut self, ui: &mut Ui, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let lines = self.snapshot_lines();
        let pools = self.servers.iter().filter(|s| s.data.is_some()).count();
        let now = unix_now();
        let offset = local_offset(now);
        // ticks of VMs that are gone go
        self.snap_view.picked.retain(|(sid, vm)| lines.iter().any(|l| l.sid == *sid && l.vm == *vm));
        ui.add_space(10.0);
        if pools == 0 {
            ui.label(egui::RichText::new("Connect to a server to see its VMs' snapshots.").color(pal.ink_soft));
            return;
        }
        // which VMs: all, with snapshots, without, on a schedule; and words in their names
        let kinds = [
            ("All", lines.len()),
            ("With snapshots", lines.iter().filter(|l| l.count > 0).count()),
            ("Without", lines.iter().filter(|l| l.count == 0).count()),
            ("On a schedule", lines.iter().filter(|l| l.schedule.is_some()).count()),
        ];
        let v = &mut self.snap_view;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(12.0, 8.0);
            let labels: Vec<String> = kinds.iter().map(|(k, n)| format!("{}  {}", k, n)).collect();
            if let Some(i) = segmented(ui, &pal, &labels, v.filter) {
                v.filter = i;
            }
            search_box(ui, &pal, &mut v.search, "Search VMs", 220.0);
        });
        let q = v.search.trim().to_lowercase();
        let filter = v.filter;
        let shown: Vec<&Line> = lines
            .iter()
            .filter(|l| match filter {
                1 => l.count > 0,
                2 => l.count == 0,
                3 => l.schedule.is_some(),
                _ => true,
            })
            .filter(|l| q.is_empty() || l.name.to_lowercase().contains(&q) || l.server.to_lowercase().contains(&q) || l.schedule.as_ref().map_or(false, |x| x.0.to_lowercase().contains(&q)))
            .collect();
        // what's ticked: Take Snapshot and Schedule… for them all; else how many there are
        ui.add_space(6.0);
        let picked: Vec<(u32, String)> = lines.iter().filter(|l| v.picked.contains(&(l.sid, l.vm.clone()))).map(|l| (l.sid, l.vm.clone())).collect();
        ui.horizontal_wrapped(|ui| {
            ui.set_min_height(28.0);
            if picked.is_empty() {
                let snaps: usize = shown.iter().map(|l| l.count).sum();
                ui.label(egui::RichText::new(format!("{} VM{}  ·  {} snapshot{}", shown.len(), if shown.len() == 1 { "" } else { "s" }, snaps, if snaps == 1 { "" } else { "s" })).color(pal.ink_soft));
                ui.label(egui::RichText::new("Click a VM for its snapshots; tick some to snapshot or schedule them at once.").size(12.0).color(pal.ink_faint));
            } else {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(egui::RichText::new(format!("{} ticked", picked.len())).color(pal.ink));
                ui.add_space(4.0);
                if flat_button(ui, "camera", "Take Snapshot…", Some(pal.accent), true, false, &pal).on_hover_text("A snapshot of each VM ticked").clicked() {
                    out.push(Intent::TakeSnapshots(picked.clone()));
                }
                let one_pool = picked.iter().all(|p| p.0 == picked[0].0);
                let tip = if one_pool { "The snapshot schedule they go on (or none)" } else { "Tick VMs of one pool: a schedule is a pool's" };
                if flat_button(ui, "clock", "Schedule…", Some(pal.accent), one_pool, false, &pal).on_hover_text(tip).on_disabled_hover_text(tip).clicked() {
                    out.push(Intent::PickSchedule(picked[0].0, picked.iter().map(|p| p.1.clone()).collect()));
                }
                if flat_button(ui, "close", "Clear", None, true, false, &pal).on_hover_text("Untick them all").clicked() {
                    v.picked.clear();
                }
            }
        });
        if shown.is_empty() {
            ui.add_space(14.0);
            let text = if !q.is_empty() { "No VMs with those words." } else if lines.is_empty() { "The pools have no VMs." } else { "No VMs like that." };
            ui.label(egui::RichText::new(text).color(pal.ink_faint));
            return;
        }
        // the columns: some go in a narrow window (Server, then Oldest)
        let avail = ui.available_width();
        let (with_server, with_oldest) = (avail >= 960.0, avail >= 700.0);
        let mut headers = vec!["", "VM"];
        if pools > 1 {
            headers.push("Pool");
        }
        if with_server {
            headers.push("Server");
        }
        // (how many: "#" when narrow)
        headers.extend([if avail >= 700.0 { "Snapshots" } else { "#" }, "Latest"]);
        if with_oldest {
            headers.push("Oldest");
        }
        headers.push("Schedule");
        let font = FontId::proportional(13.0);
        let measure = |t: &str| ui.painter().layout_no_wrap(t.to_string(), font.clone(), pal.ink).size().x;
        // when first (what matters most, kept when the name is cut short), then its name
        // (its name only when it says more than when it was taken: a schedule's, or one named
        // with the date and time as Take Snapshot names them, says no more)
        let latest_text = |l: &Line| {
            l.latest.as_ref().map(|x| {
                let when = x.time.map(|t| fmt_when(t, now, offset)).unwrap_or_default();
                let dated = x.time.map_or(false, |t| (0..=10).any(|k| x.name == fmt_date_time(t - k * 60, local_offset(t))));
                if dated { when } else { format!("{}  ·  {}", when, x.name) }
            })
        };
        let rows: Vec<Vec<Cell>> = shown
            .iter()
            .map(|l| {
                let mut r = vec![Cell::Custom(22.0), Cell::Icon(IconKind::Vm(l.power), l.name.clone())];
                if pools > 1 {
                    r.push(Cell::Text(l.pool.clone()));
                }
                if with_server {
                    r.push(Cell::Text(l.server.clone()));
                }
                r.push(Cell::by_num(l.count as f64, Cell::Text(l.count.to_string())));
                r.push(match (&l.latest, latest_text(l)) {
                    (Some(x), Some(t)) => Cell::by_num(x.time.unwrap_or(0) as f64, Cell::Custom(measure(&t) + if x.scheduled { 22.0 } else { 0.0 })),
                    _ => Cell::Text("—".into()),
                });
                if with_oldest {
                    r.push(match l.oldest {
                        Some(t) => Cell::by_num((now - t) as f64, Cell::Custom(measure(&age(t, now)) + 22.0)),
                        None => Cell::Text("—".into()),
                    });
                }
                r.push(match &l.schedule {
                    Some((name, when, _)) => Cell::by_text(name, Cell::Custom(measure(name) + measure(when) + 16.0)),
                    None => Cell::Text("—".into()),
                });
                r
            })
            .collect();
        let ticks = v.picked.clone();
        let col_latest = headers.iter().position(|h| *h == "Latest");
        let col_oldest = headers.iter().position(|h| *h == "Oldest");
        let col_schedule = headers.iter().position(|h| *h == "Schedule");
        let mut ticked: Option<(u32, String)> = None;
        let opened = self.table_with(ui, "all-snaps", &headers, &rows, true, &mut |ui, row, col, r, hot| {
            let l = shown[row];
            let p = ui.painter().clone();
            if col == 0 {
                // its tick box
                let on = ticks.contains(&(l.sid, l.vm.clone()));
                let b = Rect::from_center_size(pos2(r.min.x + 9.0, r.center().y), vec2(16.0, 16.0));
                let resp = ui.interact(b.expand(4.0), ui.id().with(("tick", l.sid, &l.vm)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                if on {
                    p.rect_filled(b, Rounding::same(4.0), pal.accent);
                    paint_glyph(&p, b.shrink(1.0), "check", pal.on_accent);
                } else {
                    p.rect_stroke(b, Rounding::same(4.0), Stroke::new(1.2_f32, if resp.hovered() || hot { pal.ink_soft } else { pal.border_strong }));
                }
                if resp.clicked() {
                    ticked = Some((l.sid, l.vm.clone()));
                }
                return;
            }
            if Some(col) == col_latest {
                // a clock first on one a schedule took; then its name and when
                let Some(x) = &l.latest else { return };
                let mut x0 = r.min.x;
                if x.scheduled {
                    let c = Rect::from_center_size(pos2(x0 + 8.0, r.center().y), vec2(16.0, 16.0));
                    paint_glyph(&p, c, "clock", pal.ink_soft);
                    ui.interact(c, ui.id().with(("auto", l.sid, &l.vm)), Sense::hover()).on_hover_text("Taken by its snapshot schedule");
                    x0 += 22.0;
                }
                let text = latest_text(l).unwrap_or_default();
                let mut job = egui::text::LayoutJob::simple_singleline(text, font.clone(), pal.ink);
                job.wrap = egui::text::TextWrapping { max_width: (r.max.x - x0).max(20.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                let g = p.layout_job(job);
                p.galley(pos2(x0, r.center().y - g.size().y / 2.0), g, pal.ink);
                return;
            }
            if Some(col) == col_schedule {
                // its schedule's name, then when it runs (or that it's paused), softer; cut short
                let Some((name, when, on)) = &l.schedule else { return };
                let mut job = egui::text::LayoutJob::default();
                job.append(name, 0.0, egui::TextFormat::simple(font.clone(), pal.ink));
                let after = if *on { when.to_lowercase() } else { "paused".into() };
                job.append(&format!("  ·  {}", after), 0.0, egui::TextFormat::simple(FontId::proportional(12.5), if *on { pal.ink_faint } else { pal.orange }));
                job.wrap = egui::text::TextWrapping { max_width: r.width().max(20.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                let g = p.layout_job(job);
                p.galley(pos2(r.min.x, r.center().y - g.size().y / 2.0), g, pal.ink);
                return;
            }
            if Some(col) == col_oldest {
                // how old; orange, with a warning, past OLD_DAYS
                let Some(t) = l.oldest else { return };
                let old = (now - t) / 86_400 > OLD_DAYS;
                let col = if old { pal.orange } else { pal.ink };
                let g = p.layout_no_wrap(age(t, now), font.clone(), col);
                let end = r.min.x + g.size().x;
                p.galley(pos2(r.min.x, r.center().y - g.size().y / 2.0), g, col);
                if old {
                    let w = Rect::from_center_size(pos2(end + 12.0, r.center().y), vec2(16.0, 16.0));
                    paint_glyph(&p, w, "warning", pal.orange);
                    ui.interact(Rect::from_min_max(r.min, pos2(w.max.x, r.max.y)), ui.id().with(("old", l.sid, &l.vm)), Sense::hover())
                        .on_hover_text(format!("Older than {} days: an old snapshot slows its VM's disks a little and holds on to everything changed since. Delete it once it's no longer needed.", OLD_DAYS));
                }
            }
        });
        if let Some(k) = ticked {
            let picked = &mut self.snap_view.picked;
            if !picked.remove(&k) {
                picked.insert(k);
            }
        } else if let Some(i) = opened {
            out.push(Intent::OpenVmSnapshots(shown[i].sid, shown[i].vm.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_in_days() {
        let now = 1_000_000_000;
        assert_eq!(age(now - 3600, now), "today");
        assert_eq!(age(now - 86_400 - 5, now), "1 day");
        assert_eq!(age(now - 94 * 86_400, now), "94 days");
    }
}
