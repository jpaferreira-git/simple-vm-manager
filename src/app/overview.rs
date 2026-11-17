//! A VM's Overview: a small picture of its screen with who it is and how it's doing beside it
//! (state, time up, description, system and address), its load over the last ten minutes, and the
//! settings people change most, as rows in a few panels: a row says its value, and a click opens a
//! small popover to change it. The rest is in the Details tab.

use super::*;

const GB: i64 = 1024 * 1024 * 1024;

/// What's being typed or picked on the shown VM's Overview (it goes when another VM is shown, or
/// once a change is made).
#[derive(Default)]
pub(super) struct OverviewEdit {
    pub vm: String,
    pub name: Option<String>,
    pub description: Option<String>,
    /// Picked in the CPUs and the memory popovers, not applied yet (memory in GB).
    pub cpus: Option<i64>,
    pub mem: Option<f64>,
    /// Typed in a disk's popover: the size to grow it to (GB).
    pub grow: String,
    /// Typed and picked in Add disk's popover: its size (GB) and its storage.
    pub new_disk: (String, String),
    /// Typed in the DVD drive's popover to find an ISO.
    pub dvd_search: String,
    /// Put the cursor in the box being typed in on the next frame.
    pub focus: bool,
}

impl OverviewEdit {
    /// A change was made: what was typed for it goes.
    pub fn done(&mut self) {
        let vm = std::mem::take(&mut self.vm);
        *self = OverviewEdit { vm, ..Default::default() };
    }
}

/// The last look at a running VM's screen: when it was taken (or tried), the picture, and why the
/// last try failed.
#[derive(Default)]
pub(super) struct Thumb {
    pub at: Option<Instant>,
    pub tex: Option<egui::TextureHandle>,
    /// It's all black: its system turned the display off to save power (Windows does when idle).
    pub dark: bool,
    pub error: Option<String>,
    /// Its display was woken (a Ctrl tap) since its Overview came into view, and the last frame
    /// that showed it: once per visit, as the Console tab does, so the VM can still go idle and
    /// lock itself while its Overview stays open.
    pub woken: bool,
    pub seen: u64,
}

/// How often a running VM's screen is looked at while its Overview is shown.
const THUMB_EVERY: Duration = Duration::from_secs(10);

/// What the picture of its screen shows.
enum Shot {
    /// The demo server's pretend console.
    Demo,
    /// Not running: what it is instead.
    Off(String),
    Loading,
    /// The picture, its size, and whether the screen is dark.
    Picture(egui::TextureId, egui::Vec2, bool),
    Failed(String),
}

/// What the Overview needs of the pool, taken before drawing.
struct Around {
    sid: u32,
    /// What it's busy with (a power action), or what change is being made.
    busy: Option<String>,
    saving: Option<String>,
    /// Its server, when that's off or not answering (its name, and whether it's live).
    down: Option<(String, bool)>,
    host: Option<String>,
    /// ISO files for its DVD drive (disc, its name, its library), storage for a new disk (with
    /// its room), and networks.
    isos: Vec<(String, String, String)>,
    stores: Vec<(String, String, i64)>,
    nets: Vec<(String, String)>,
    most_cpus: i64,
    most_memory: i64,
}

impl App {
    pub(super) fn overview(&mut self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let Some((s, d, v)) = self.vm_of(sel) else { return };
        let v = v.clone();
        let home = v.home().map(String::from);
        // a server's own storage only when the VM belongs with that server
        let seen = |h: &Option<String>| h.is_none() || *h == home;
        let mut stores: Vec<(String, String, i64)> = d.srs.iter().filter(|x| !x.is_iso() && x.kind != "udev" && x.size > 0 && seen(&x.host)).map(|x| (x.rref.clone(), x.name.clone(), (x.size - x.used).max(0))).collect();
        stores.sort_by_key(|x| -x.2);
        let mut isos: Vec<(String, String, String)> = d.srs.iter().filter(|x| x.is_iso() && seen(&x.host)).flat_map(|x| x.files.iter().map(move |f| (f.vdi.clone(), f.name.clone(), x.name.clone()))).collect();
        isos.sort_by_key(|i| i.1.to_lowercase());
        let host_of = v.host.as_deref().and_then(|h| d.host(h));
        let at = Around {
            sid: s.id,
            busy: s.busy.get(&v.rref).map(|b| b.0.clone()),
            saving: self.vm_saving.get(&v.rref).cloned(),
            down: d.host_down_for(&v).map(|h| (h.name.clone(), h.live)),
            host: host_of.map(|h| h.name.clone()),
            isos,
            stores,
            nets: d.networks.iter().map(|n| (n.rref.clone(), n.name.clone())).collect(),
            most_cpus: d.hosts.iter().map(|h| h.cpu_count).max().unwrap_or(64).max(1),
            most_memory: d.hosts.iter().map(|h| h.mem_total).max().unwrap_or(0),
        };
        let address = host_of.map(|h| h.address.clone()).unwrap_or_default();
        let conn = s.conn.clone();
        let (demo, look) = (s.conn.is_demo(), s.conn.clone());
        if self.ov.vm != v.rref {
            self.ov = OverviewEdit { vm: v.rref.clone(), ..Default::default() };
        }
        // its load, read every five seconds while it runs and is shown
        let running = v.power == PowerState::Running && at.down.is_none();
        if running {
            let due = self.perf.get(&v.rref).map_or(true, |(t, _)| t.elapsed() >= Duration::from_secs(5));
            if due && self.perf_busy.insert(v.rref.clone()) {
                let (tx, ctx, vm, uuid) = (self.tx.clone(), ui.ctx().clone(), v.rref.clone(), v.uuid.clone());
                std::thread::spawn(move || {
                    let result = conn.vm_perf(&uuid, &address);
                    let _ = tx.send(Msg::Perf { vm, result });
                    ctx.request_repaint();
                });
            }
            ui.ctx().request_repaint_after(Duration::from_secs(5));
        }
        let perf = self.perf.get(&v.rref).map(|p| p.1.clone());
        // and a look at its screen every few seconds (none on the demo server: its pretend one)
        if running && !demo {
            let pass = ui.ctx().cumulative_pass_nr();
            let t = self.thumbs.entry(v.rref.clone()).or_default();
            if t.seen + 1 < pass {
                t.woken = false; // its Overview has just come into view
            }
            t.seen = pass;
            // a dark screen is woken right away, once per visit
            let wake = t.dark && !t.woken;
            let due = wake || t.at.map_or(true, |at| at.elapsed() >= THUMB_EVERY);
            if due && self.thumb_busy.insert(v.rref.clone()) {
                t.woken |= wake;
                let (tx, ctx, vm) = (self.tx.clone(), ui.ctx().clone(), v.rref.clone());
                std::thread::spawn(move || {
                    let result = crate::console::snapshot(look.as_ref(), &vm, 560, wake);
                    let _ = tx.send(Msg::Thumb { vm, result });
                    ctx.request_repaint();
                });
            }
        }
        let shot = match (running, self.thumbs.get(&v.rref)) {
            (false, _) => Shot::Off(match (&at.down, v.power) {
                (Some(_), _) => "Its server isn't there".into(),
                (None, p) => p.label().into(),
            }),
            _ if demo => Shot::Demo,
            (true, Some(Thumb { tex: Some(t), dark, .. })) => Shot::Picture(t.id(), t.size_vec2(), *dark),
            (true, Some(Thumb { error: Some(e), .. })) => Shot::Failed(e.clone()),
            (true, _) => Shot::Loading,
        };
        let ov = &mut self.ov;
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            let pad = 20.0;
            let w = ui.available_width() - 2.0 * pad;
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                ui.add_space(pad);
                ui.vertical(|ui| {
                    ui.set_width(w);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    header(ui, &pal, w, &v, &at, &shot, ov, out);
                    ui.add_space(24.0);
                    section(ui, &pal, "Load  ·  the last ten minutes");
                    tiles(ui, &pal, w, &v, &at, perf.as_ref(), running);
                    ui.add_space(24.0);
                    type Card<'c> = &'c dyn Fn(&mut Ui, &mut OverviewEdit, &mut Vec<Intent>);
                    let res: Card = &|ui, ov, out| {
                        section(ui, &pal, "Resources");
                        resources(ui, &pal, &v, &at, ov, out);
                    };
                    let start: Card = &|ui, _, out| {
                        section(ui, &pal, "Startup");
                        startup(ui, &pal, &v, &at, out);
                    };
                    let dsk: Card = &|ui, ov, out| {
                        section(ui, &pal, "Disks");
                        disks(ui, &pal, &v, &at, ov, out);
                    };
                    let net: Card = &|ui, _, out| {
                        section(ui, &pal, "Network");
                        network(ui, &pal, &v, &at, out);
                    };
                    if w >= 720.0 {
                        // two by two: Resources and Disks, then Startup and Network, the two
                        // panels of a row as tall as each other
                        let half = (w - 20.0) / 2.0;
                        pair(ui, "resources-disks", half, ov, out, res, dsk);
                        ui.add_space(24.0);
                        pair(ui, "startup-network", half, ov, out, start, net);
                    } else {
                        for (i, card) in [res, start, dsk, net].into_iter().enumerate() {
                            if i > 0 {
                                ui.add_space(24.0);
                            }
                            card(ui, ov, out);
                        }
                    }
                    ui.add_space(28.0);
                });
            });
        });
    }
}

/// Changes that can be made now: nothing else under way, and its server there.
fn free(at: &Around) -> bool {
    at.busy.is_none() && at.saving.is_none() && at.down.is_none()
}

/// Changes that need it off.
fn off(v: &VmInfo, at: &Around) -> bool {
    free(at) && v.power == PowerState::Halted
}

/// Why a change that needs it off can't be made now.
fn needs_off(v: &VmInfo, at: &Around) -> &'static str {
    if at.down.is_some() {
        "Its server isn't there"
    } else if v.power != PowerState::Halted {
        "Shut it down to change this"
    } else {
        "Wait for what's being done to finish"
    }
}

/// A section's title over its panel: small, soft, in capitals.
pub(super) fn section(ui: &mut Ui, pal: &Pal, title: &str) {
    ui.label(egui::RichText::new(title.to_uppercase()).size(11.0).color(pal.ink_faint));
    ui.add_space(8.0);
}

/// The height the next panel is made at least (set by `pair`, used once), and the height its rows
/// took (for `pair` to read back).
const PANEL_MIN: &str = "overview-panel-min";
const PANEL_ROWS: &str = "overview-panel-rows";

/// A panel of rows, rounded, a hair of a border.
pub(super) fn panel(ui: &mut Ui, pal: &Pal, add: impl FnOnce(&mut Ui)) {
    let min = ui.data_mut(|d| d.remove_temp::<f32>(egui::Id::new(PANEL_MIN))).unwrap_or(0.0);
    egui::Frame::none().fill(pal.surface).stroke(Stroke::new(1.0, pal.border.gamma_multiply(0.7))).rounding(Rounding::same(12.0)).inner_margin(egui::Margin::symmetric(0.0, 3.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        add(ui);
        let rows = ui.min_rect().height();
        ui.data_mut(|d| d.insert_temp(egui::Id::new(PANEL_ROWS), rows));
        if rows < min {
            // (set_min_height would count from below the rows)
            ui.add_space(min - rows);
        }
    });
}

/// Two cards side by side, `half` wide each, their panels as tall as the taller one's rows (as
/// measured the frame before; a frame later when that changes).
pub(super) fn pair<S>(ui: &mut Ui, key: &str, half: f32, ov: &mut S, out: &mut Vec<Intent>, left: &dyn Fn(&mut Ui, &mut S, &mut Vec<Intent>), right: &dyn Fn(&mut Ui, &mut S, &mut Vec<Intent>)) {
    let id = ui.id().with(("pair", key));
    let tall: f32 = ui.data(|d| d.get_temp(id)).unwrap_or(0.0);
    let mut rows = [0.0f32; 2];
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 20.0;
        for (i, card) in [left, right].into_iter().enumerate() {
            ui.vertical(|ui| {
                ui.set_width(half);
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.data_mut(|d| d.insert_temp(egui::Id::new(PANEL_MIN), tall));
                card(ui, ov, out);
                rows[i] = ui.data_mut(|d| d.remove_temp::<f32>(egui::Id::new(PANEL_ROWS))).unwrap_or(0.0);
            });
        }
    });
    let want = rows[0].max(rows[1]);
    if (want - tall).abs() > 0.5 {
        ui.data_mut(|d| d.insert_temp(id, want));
        ui.ctx().request_repaint();
    }
}

/// What's at a row's start.
pub(super) enum Lead {
    Icon(IconKind),
    /// a glyph in the icons' colour (as an `Icon`)
    Mark(&'static str),
    /// a glyph in the accent colour (Add disk…)
    Glyph(&'static str),
}

/// What's at a row's end.
#[derive(PartialEq)]
pub(super) enum Tail {
    /// A click opens a popover to change it.
    Chevron,
    /// A click opens its actions.
    More,
    /// It can't be changed now (the tooltip says why).
    Lock,
    /// An on/off switch, as it is.
    Switch(bool),
    Nothing,
}

/// A row: its icon, its label, its value (soft, on the right, cut short to fit) and its end; it
/// lights up under the mouse when it does something. Its response, and the same for a popover to
/// hang from (the whole row: a click anywhere on it counts as on it).
pub(super) fn row(ui: &mut Ui, pal: &Pal, lead: Option<Lead>, label: &str, value: &str, tail: Tail, active: bool, last: bool) -> (egui::Response, egui::Response) {
    let w = ui.available_width();
    let (r, resp) = ui.allocate_exact_size(vec2(w, 46.0), if active { Sense::click() } else { Sense::hover() });
    let p = ui.painter().clone();
    if active && resp.hovered() {
        p.rect_filled(r.shrink2(vec2(5.0, 2.0)), Rounding::same(8.0), pal.hover);
    }
    let cy = r.center().y;
    let mut x = r.min.x + 16.0;
    let accent = matches!(lead, Some(Lead::Glyph("plus")));
    match lead {
        Some(Lead::Icon(k)) => {
            paint_icon(&p, Rect::from_min_size(pos2(x, cy - 8.0), vec2(16.0, 16.0)), k, pal, if active && resp.hovered() { pal.hover } else { pal.surface });
            x += 28.0;
        }
        Some(Lead::Mark(g)) => {
            paint_glyph(&p, Rect::from_min_size(pos2(x, cy - 8.0), vec2(16.0, 16.0)), g, pal.icon);
            x += 28.0;
        }
        Some(Lead::Glyph(g)) => {
            paint_glyph(&p, Rect::from_min_size(pos2(x, cy - 8.0), vec2(16.0, 16.0)), g, if active { pal.accent } else { pal.ink_faint });
            x += 28.0;
        }
        None => {}
    }
    let label_col = if !active && accent { pal.ink_faint } else if accent { pal.accent } else { pal.ink };
    let lab = p.text(pos2(x, cy), Align2::LEFT_CENTER, label, FontId::proportional(14.0), label_col);
    // the end
    let mut right = r.max.x - 16.0;
    match &tail {
        Tail::Chevron => {
            let c = pos2(right - 3.0, cy);
            p.add(egui::Shape::line(vec![c + vec2(-3.0, -5.0), c + vec2(2.0, 0.0), c + vec2(-3.0, 5.0)], Stroke::new(1.5, pal.ink_faint)));
            right -= 20.0;
        }
        Tail::More => {
            paint_glyph(&p, Rect::from_center_size(pos2(right - 8.0, cy), vec2(16.0, 16.0)), "more", if resp.hovered() { pal.ink } else { pal.ink_soft });
            right -= 26.0;
        }
        Tail::Lock => {
            paint_glyph(&p, Rect::from_center_size(pos2(right - 7.0, cy), vec2(14.0, 14.0)), "lock", pal.ink_faint);
            right -= 24.0;
        }
        Tail::Switch(on) => {
            let sw = Rect::from_min_size(pos2(right - 38.0, cy - 11.0), vec2(38.0, 22.0));
            let track = if *on { pal.accent } else { pal.border_strong };
            p.rect_filled(sw, Rounding::same(11.0), if active { track } else { track.gamma_multiply(0.5) });
            p.circle_filled(pos2(if *on { sw.max.x - 11.0 } else { sw.min.x + 11.0 }, cy), 8.5, Color32::WHITE);
            right -= 48.0;
        }
        Tail::Nothing => {}
    }
    // the value, cut short between the label and the end (all of it under the mouse)
    let mut cut = false;
    if !value.is_empty() {
        let (g, c) = fit(&p, value, FontId::proportional(13.5), pal.ink_soft, right - lab.max.x - 16.0);
        cut = c;
        p.galley(pos2(right - g.size().x, cy - g.size().y / 2.0), g, pal.ink_soft);
    }
    let resp = if cut { resp.on_hover_text(value) } else { resp };
    if !last {
        p.line_segment([pos2(x, r.max.y - 0.5), pos2(r.max.x, r.max.y - 0.5)], Stroke::new(1.0, pal.border.gamma_multiply(0.6)));
    }
    let mut anchor = resp.clone();
    anchor.rect = r;
    (resp, anchor)
}

/// Opens (or closes) the popover `id` when `clicked`, and draws it while it's open at the right of
/// `anchor` (its row), under it, or over it when there's no room under it.
fn popover(ui: &Ui, id: egui::Id, clicked: bool, anchor: &egui::Response, width: f32, add: impl FnOnce(&mut Ui)) {
    if clicked {
        ui.memory_mut(|m| m.toggle_popup(id));
    }
    let mut a = anchor.clone();
    a.rect.min.x = (a.rect.max.x - width - 16.0).max(a.rect.min.x);
    let screen = ui.ctx().screen_rect();
    let tall = ui.ctx().memory(|m| m.area_rect(id)).map_or(0.0, |r| r.height());
    let (below, above) = (screen.max.y - a.rect.max.y, a.rect.min.y - screen.min.y);
    let side = if tall > below && above > below { egui::AboveOrBelow::Above } else { egui::AboveOrBelow::Below };
    egui::popup::popup_above_or_below_widget(ui, id, &a, side, egui::PopupCloseBehavior::CloseOnClickOutside, |ui| {
        egui::Frame::none().inner_margin(egui::Margin::same(6.0)).show(ui, |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 8.0;
            add(ui);
        });
    });
}

/// `text` laid out in `room` at most, cut short with "…" when it's longer (and whether it was).
pub(super) fn fit(p: &egui::Painter, text: &str, font: FontId, col: Color32, room: f32) -> (std::sync::Arc<egui::Galley>, bool) {
    let g = p.layout_no_wrap(text.to_string(), font.clone(), col);
    if g.size().x <= room {
        return (g, false);
    }
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, col);
    job.wrap = egui::text::TextWrapping { max_width: room.max(12.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    (p.layout_job(job), true)
}

/// A popover's head: its title in bold, and something about it on the right, softer.
fn pop_head(ui: &mut Ui, pal: &Pal, title: &str, aside: &str) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
    let p = ui.painter();
    let a = p.text(pos2(r.max.x, r.center().y), Align2::RIGHT_CENTER, aside, FontId::proportional(12.5), pal.ink_faint);
    let (g, _) = fit(p, title, FontId::new(14.0, FontFamily::Name("bold".into())), pal.ink, a.min.x - r.min.x - 12.0);
    p.galley(pos2(r.min.x, r.center().y - g.size().y / 2.0), g, pal.ink);
}

/// A small soft label over a popover's part.
fn pop_label(ui: &mut Ui, pal: &Pal, text: &str) {
    ui.label(egui::RichText::new(text).size(12.0).color(pal.ink_faint));
}

/// A note in a popover, with a lock when it says why something can't be done.
fn pop_note(ui: &mut Ui, pal: &Pal, text: &str, lock: bool) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if lock {
            let (r, _) = ui.allocate_exact_size(vec2(14.0, 16.0), Sense::hover());
            paint_glyph(ui.painter(), Rect::from_center_size(r.center(), vec2(13.0, 13.0)), "lock", pal.ink_faint);
        }
        ui.add(egui::Label::new(egui::RichText::new(text).size(12.5).color(pal.ink_soft)).wrap());
    });
}

/// A thin line across a popover.
fn pop_line(ui: &mut Ui, pal: &Pal) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().line_segment([r.left_center(), r.right_center()], Stroke::new(1.0_f32, pal.border));
}

/// The button that does it: filled in the accent colour, all the width when `full`.
fn primary(ui: &mut Ui, pal: &Pal, text: &str, enabled: bool, full: bool) -> bool {
    let col = if enabled { pal.on_accent } else { pal.on_accent.gamma_multiply(0.7) };
    let g = ui.painter().layout_no_wrap(text.to_string(), FontId::new(13.5, FontFamily::Name("bold".into())), col);
    let w = if full { ui.available_width() } else { g.size().x + 32.0 };
    let (r, resp) = ui.allocate_exact_size(vec2(w, 32.0), if enabled { Sense::click() } else { Sense::hover() });
    let p = ui.painter();
    p.rect_filled(r, Rounding::same(8.0), if enabled { pal.accent } else { pal.accent.gamma_multiply(0.35) });
    if enabled && resp.hovered() {
        p.rect_filled(r, Rounding::same(8.0), Color32::from_white_alpha(22));
    }
    p.galley(r.center() - g.size() / 2.0, g, col);
    enabled && resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// A box for a size in GB, "GB" inside it on the right.
fn size_field(ui: &mut Ui, pal: &Pal, text: &mut String, width: f32) -> egui::Response {
    let (r, _) = ui.allocate_exact_size(vec2(width, 32.0), Sense::hover());
    let bg = ui.painter().add(egui::Shape::Noop);
    let unit = ui.painter().text(pos2(r.max.x - 10.0, r.center().y), Align2::RIGHT_CENTER, "GB", FontId::proportional(13.0), pal.ink_faint);
    let room = Rect::from_min_max(pos2(r.min.x + 10.0, r.min.y), pos2(unit.min.x - 6.0, r.max.y));
    // (in a child of its own: the row it's in goes on after the whole box, not after the text)
    let mut inside = ui.new_child(egui::UiBuilder::new().max_rect(room).layout(egui::Layout::left_to_right(egui::Align::Center)));
    let resp = inside.add(egui::TextEdit::singleline(text).frame(false).font(FontId::proportional(14.0)).desired_width(room.width()).margin(vec2(0.0, 0.0)));
    let border = if resp.has_focus() { pal.accent } else { pal.border_strong };
    ui.painter().set(bg, egui::Shape::Rect(egui::epaint::RectShape::new(r, Rounding::same(8.0), pal.input, Stroke::new(1.0_f32, border))));
    resp
}

fn close_popover(ui: &Ui) {
    ui.memory_mut(|m| m.close_popup());
}

/// A choice in a popover: a tick when it's the one, its text and a softer second part.
fn choice(ui: &mut Ui, pal: &Pal, text: &str, sub: &str, on: bool, enabled: bool) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), if enabled { Sense::click() } else { Sense::hover() });
    let p = ui.painter();
    if enabled && resp.hovered() {
        p.rect_filled(r, Rounding::same(6.0), pal.hover);
    }
    if on {
        paint_glyph(p, Rect::from_center_size(pos2(r.min.x + 13.0, r.center().y), vec2(14.0, 14.0)), "check", pal.accent);
    }
    let ink = if enabled { pal.ink } else { pal.ink_faint };
    // the second part keeps up to a third of the room; the text is cut short to fit the rest
    let (x, right) = (r.min.x + 28.0, r.max.x - 8.0);
    let sub_g = (!sub.is_empty()).then(|| fit(p, sub, FontId::proportional(12.0), pal.ink_faint, (right - x) / 3.0));
    let (g, cut) = fit(p, text, FontId::proportional(13.5), ink, right - x - sub_g.as_ref().map_or(0.0, |s| s.0.size().x + 10.0));
    let end = x + g.size().x;
    p.galley(pos2(x, r.center().y - g.size().y / 2.0), g, ink);
    let mut cut_sub = false;
    if let Some((s, c)) = sub_g {
        cut_sub = c;
        p.galley(pos2(end + 10.0, r.center().y - s.size().y / 2.0), s, pal.ink_faint);
    }
    let resp = if cut || cut_sub { resp.on_hover_text(if sub.is_empty() { text.to_string() } else { format!("{}  ·  {}", text, sub) }) } else { resp };
    enabled && resp.clicked()
}

/// A dangerous action in a popover, in red.
fn danger(ui: &mut Ui, pal: &Pal, text: &str, enabled: bool, why: &str) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), if enabled { Sense::click() } else { Sense::hover() });
    let resp = if enabled { resp } else { resp.on_hover_text(why) };
    if enabled && resp.hovered() {
        ui.painter().rect_filled(r, Rounding::same(6.0), pal.red.gamma_multiply(if pal.dark { 0.22 } else { 0.10 }));
    }
    let col = if enabled { pal.red } else { pal.ink_faint };
    paint_glyph(ui.painter(), Rect::from_center_size(pos2(r.min.x + 13.0, r.center().y), vec2(14.0, 14.0)), "trash", col);
    ui.painter().text(pos2(r.min.x + 28.0, r.center().y), Align2::LEFT_CENTER, text, FontId::proportional(13.5), col);
    enabled && resp.clicked()
}

/// A rounded chip, filled when it's the one picked.
fn chip(ui: &mut Ui, pal: &Pal, text: &str, on: bool) -> bool {
    let w = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(13.0), pal.ink).size().x + 22.0;
    let (r, resp) = ui.allocate_exact_size(vec2(w.max(40.0), 28.0), Sense::click());
    let fill = if on {
        pal.accent
    } else if resp.hovered() {
        pal.hover
    } else {
        pal.input
    };
    ui.painter().rect(r, Rounding::same(14.0), fill, if on { Stroke::NONE } else { Stroke::new(1.0, pal.border) });
    ui.painter().text(r.center(), Align2::CENTER_CENTER, text, FontId::proportional(13.0), if on { pal.on_accent } else { pal.ink });
    resp.clicked()
}

/// A round − or + button.
fn round_button(ui: &mut Ui, pal: &Pal, plus: bool, enabled: bool) -> bool {
    let (r, resp) = ui.allocate_exact_size(vec2(30.0, 30.0), if enabled { Sense::click() } else { Sense::hover() });
    let col = if enabled { pal.ink } else { pal.ink_faint };
    if enabled && resp.hovered() {
        ui.painter().circle_filled(r.center(), 15.0, pal.hover);
    }
    ui.painter().circle_stroke(r.center(), 14.0, Stroke::new(1.0, pal.border_strong));
    let c = r.center();
    ui.painter().line_segment([c - vec2(5.0, 0.0), c + vec2(5.0, 0.0)], Stroke::new(1.6, col));
    if plus {
        ui.painter().line_segment([c - vec2(0.0, 5.0), c + vec2(0.0, 5.0)], Stroke::new(1.6, col));
    }
    enabled && resp.clicked()
}

/// A text box for its name or description, then Save and Cancel; Enter saves, Esc cancels. Some
/// (true) when saved, Some(false) when cancelled.
fn edit_line(ui: &mut Ui, pal: &Pal, text: &mut String, focus: &mut bool, size: f32) -> Option<bool> {
    let mut done = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let field = ui.add(egui::TextEdit::singleline(text).font(FontId::proportional(size)).desired_width((ui.available_width() - 70.0).max(120.0)).margin(vec2(8.0, 5.0)));
        if *focus {
            field.request_focus();
            *focus = false;
        }
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        if flat_button(ui, "check", "Save", Some(pal.green), true, true, pal).clicked() || enter {
            done = Some(true);
        }
        if flat_button(ui, "close", "Cancel", None, true, true, pal).clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
            done = Some(false);
        }
    });
    done
}

/// The picture of its screen on the left, as big as there's room for, and beside it its state,
/// how long it's been up, its description, system and address (its name is in the window's
/// header, and edited there); the change being made under them.
#[allow(clippy::too_many_arguments)]
fn header(ui: &mut Ui, pal: &Pal, w: f32, v: &VmInfo, at: &Around, shot: &Shot, ov: &mut OverviewEdit, out: &mut Vec<Intent>) {
    let (tw, gap) = if w < 480.0 {
        (120.0, 16.0)
    } else if w < 600.0 {
        (150.0, 20.0)
    } else {
        ((w * 0.24).clamp(200.0, 300.0), 22.0)
    };
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        screen(ui, pal, vec2(tw, tw * 0.75), v, shot, out);
        ui.vertical(|ui| {
            ui.set_width(w - tw - gap);
            ui.spacing_mut().item_spacing.y = 0.0;
            facts(ui, pal, v, at, ov, out);
            if let Some(what) = &at.saving {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(13.0));
                    ui.label(egui::RichText::new(format!("{}…", what)).size(12.5).color(pal.ink_soft));
                });
            }
        });
    });
}

/// A small picture of its screen (taken every few seconds while it runs; a click opens its
/// console), or while it's off what it is.
fn screen(ui: &mut Ui, pal: &Pal, size: egui::Vec2, v: &VmInfo, shot: &Shot, out: &mut Vec<Intent>) {
    let live = !matches!(shot, Shot::Off(_));
    let (r, resp) = ui.allocate_exact_size(size, if live { Sense::click() } else { Sense::hover() });
    let round = Rounding::same(10.0);
    let hot = live && resp.hovered();
    let p = ui.painter().clone();
    // a screen's black; while it's off, the panels' colour
    p.rect_filled(r, round, if live { Color32::BLACK } else { pal.surface });
    let soft = if live { Color32::from_gray(150) } else { pal.ink_soft };
    match shot {
        Shot::Demo => crate::console::paint_demo_screen(&p, r.shrink(1.0), &v.name, &v.os),
        Shot::Picture(tex, px, dark) => {
            // all of it, as wide or as tall as the room lets
            let s = (r.width() / px.x).min(r.height() / px.y);
            let img = Rect::from_center_size(r.center(), *px * s);
            let corners = if (img.width() - r.width()).abs() < 1.0 && (img.height() - r.height()).abs() < 1.0 { round } else { Rounding::ZERO };
            egui::Image::from_texture((*tex, *px)).rounding(corners).paint_at(ui, img);
            if *dark && !hot {
                paint_glyph(&p, Rect::from_center_size(r.center() - vec2(0.0, 12.0), vec2(18.0, 18.0)), "theme-dark", Color32::from_gray(110));
                let (g, _) = fit(&p, "Its screen is asleep", FontId::proportional(12.5), soft, r.width() - 20.0);
                p.galley(pos2(r.center().x - g.size().x / 2.0, r.center().y + 8.0), g, soft);
            }
        }
        Shot::Loading => {
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_center_size(r.center(), vec2(18.0, 18.0))));
            child.add(egui::Spinner::new().size(16.0).color(soft));
        }
        Shot::Failed(_) if !hot => {
            p.text(r.center(), Align2::CENTER_CENTER, "No picture of its screen", FontId::proportional(12.0), soft);
        }
        Shot::Failed(_) => {}
        Shot::Off(what) => {
            paint_glyph(&p, Rect::from_center_size(r.center() - vec2(0.0, 12.0), vec2(18.0, 18.0)), "power", pal.ink_faint);
            let (g, _) = fit(&p, what, FontId::proportional(12.5), soft, r.width() - 20.0);
            p.galley(pos2(r.center().x - g.size().x / 2.0, r.center().y + 8.0), g, soft);
        }
    }
    p.rect_stroke(r, round, Stroke::new(1.0_f32, pal.border));
    if hot {
        p.rect_filled(r, round, Color32::from_black_alpha(120));
        p.text(r.center(), Align2::CENTER_CENTER, "Open its console", FontId::proportional(13.5), Color32::WHITE);
    }
    let resp = match shot {
        Shot::Failed(e) => resp.on_hover_text(format!("Its screen couldn't be read: {}", e)),
        Shot::Picture(.., true) => resp.on_hover_text("Its system turned the display off to save power (Windows does when nobody uses it for a while). Opening the console wakes it."),
        _ => resp,
    };
    if live && resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        out.push(Intent::Tab(Tab::Console));
    }
}

/// Its facts in rows, a label then the value: its state (a dot in its colour), how long it's been
/// up, its description (a click edits it), its system and its address (a button copies it). Values
/// too long for the room are cut short, all of them under the mouse.
fn facts(ui: &mut Ui, pal: &Pal, v: &VmInfo, at: &Around, ov: &mut OverviewEdit, out: &mut Vec<Intent>) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0);
    let (dot, state) = match (&at.busy, &at.down, v.power) {
        (Some(b), _, _) => (pal.accent, b.clone()),
        (None, Some((h, live)), _) => (pal.grey, format!("Unknown: {} {}", h, if *live { "isn't answering" } else { "is off" })),
        (None, None, p) => (state_color(pal, p), p.label().to_string()),
    };
    let w = ui.available_width();
    let font = FontId::proportional(14.0);
    // the values start after the longest label
    let label_w = ["State", "Up for", "Description", "Operating system", "IP address"].iter().map(|l| ui.painter().layout_no_wrap(l.to_string(), FontId::proportional(13.0), pal.ink).size().x).fold(0.0, f32::max) + 24.0;
    // a row: its label, and where its value goes
    let line = |ui: &mut Ui, label: &str| -> Rect {
        let (r, _) = ui.allocate_exact_size(vec2(w, 30.0), Sense::hover());
        ui.painter().text(pos2(r.min.x, r.center().y), Align2::LEFT_CENTER, label, FontId::proportional(13.0), pal.ink_faint);
        Rect::from_min_max(pos2(r.min.x + label_w, r.min.y), r.max)
    };
    // a value cut short to `room`, all of it under the mouse; where it ended
    let value = |ui: &mut Ui, at: Rect, text: &str, col: Color32, room: f32, id: &str| -> Rect {
        let (g, cut) = fit(ui.painter(), text, font.clone(), col, room);
        let gr = Rect::from_min_size(pos2(at.min.x, at.center().y - g.size().y / 2.0), g.size());
        ui.painter().galley(gr.min, g, col);
        if cut {
            ui.interact(gr, ui.id().with(id), Sense::hover()).on_hover_text(text);
        }
        gr
    };

    let r = line(ui, "State");
    ui.painter().circle_filled(pos2(r.min.x + 4.0, r.center().y), 4.0, dot);
    value(ui, r.translate(vec2(14.0, 0.0)), &state, pal.ink, r.width() - 14.0, "state");
    if let (PowerState::Running, None, Some(t)) = (v.power, &at.down, v.started) {
        let r = line(ui, "Up for");
        value(ui, r, &fmt_duration(now - t), pal.ink, r.width(), "up");
    }

    // its description: a box to type it in, or the text (a pencil showing under the mouse)
    if let Some(text) = ov.description.as_mut() {
        ui.add_space(3.0);
        match edit_line(ui, pal, text, &mut ov.focus, 14.0) {
            Some(true) => {
                let d = text.trim().to_string();
                if d != v.description {
                    out.push(Intent::EditVm(at.sid, v.rref.clone(), VmChange::Description(d)));
                }
                ov.description = None;
            }
            Some(false) => ov.description = None,
            None => {}
        }
        ui.add_space(3.0);
    } else {
        let r = line(ui, "Description");
        let one = v.description.split_whitespace().collect::<Vec<_>>().join(" ");
        let (shown, col) = if one.is_empty() { ("Add a description", pal.ink_faint) } else { (one.as_str(), pal.ink) };
        let (g, cut) = fit(ui.painter(), shown, font.clone(), col, r.width() - 26.0);
        let gr = Rect::from_min_size(pos2(r.min.x, r.center().y - g.size().y / 2.0), g.size());
        ui.painter().galley(gr.min, g, col);
        let enabled = free(at);
        let hit = Rect::from_min_max(pos2(gr.min.x, r.min.y + 3.0), pos2(gr.max.x + 24.0, r.max.y - 3.0));
        let resp = ui.interact(hit, ui.id().with("description"), if enabled { Sense::click() } else { Sense::hover() });
        if enabled && resp.hovered() {
            paint_glyph(ui.painter(), Rect::from_center_size(pos2(gr.max.x + 13.0, r.center().y), vec2(14.0, 14.0)), "edit", pal.ink_soft);
        }
        let resp = if cut { resp.on_hover_text(&v.description) } else { resp };
        if enabled && resp.on_hover_cursor(egui::CursorIcon::Text).clicked() {
            (ov.description, ov.focus) = (Some(v.description.clone()), true);
        }
    }

    let r = line(ui, "Operating system");
    let os = or(&v.os, "Unknown").to_string();
    value(ui, r, &os, if v.os.is_empty() { pal.ink_faint } else { pal.ink }, r.width(), "os");

    let r = line(ui, "IP address");
    if v.ip.is_empty() {
        value(ui, r, if v.power == PowerState::Running { "None yet" } else { "None while it's off" }, pal.ink_faint, r.width(), "ip");
    } else {
        let gr = value(ui, r, &v.ip, pal.ink, r.width() - 30.0, "ip");
        let b = Rect::from_center_size(pos2(gr.max.x + 17.0, r.center().y), vec2(22.0, 22.0));
        let resp = ui.interact(b, ui.id().with("copy-ip"), Sense::click()).on_hover_text("Copy the address");
        if resp.hovered() {
            ui.painter().rect_filled(b, Rounding::same(5.0), pal.hover);
        }
        paint_glyph(ui.painter(), Rect::from_center_size(b.center(), vec2(14.0, 14.0)), "copy", pal.ink_soft);
        if resp.clicked() {
            ui.ctx().copy_text(v.ip.clone());
        }
    }
}

/// Bytes per second, shortly.
pub(super) fn fmt_rate(b: f64) -> String {
    match b {
        b if b < 1024.0 => format!("{:.0} B/s", b),
        b if b < 1024.0 * 1024.0 => format!("{:.0} KB/s", b / 1024.0),
        b => format!("{:.1} MB/s", b / 1024.0 / 1024.0),
    }
}

/// The four tiles of its load (CPU, memory, disk, network), four in a row or two by two; while
/// it's off, a line saying when they show.
fn tiles(ui: &mut Ui, pal: &Pal, w: f32, v: &VmInfo, at: &Around, perf: Option<&Result<Perf, String>>, running: bool) {
    let say = |ui: &mut Ui, text: &str, spin: bool| say(ui, pal, text, spin);
    if !running {
        let text = match (&at.down, v.power) {
            (Some(_), _) => "Its server isn't there, so its load can't be read.",
            (None, PowerState::Suspended) => "Suspended: its load shows here once it's resumed.",
            _ => "Its load shows here while it runs.",
        };
        say(ui, text, false);
        return;
    }
    let p = match perf {
        None => return say(ui, "Reading its load…", true),
        Some(Err(e)) => return say(ui, &format!("Couldn't read its load: {}", e), false),
        Some(Ok(p)) if p.times.len() < 2 => return say(ui, "Its load shows here in a moment…", true),
        Some(Ok(p)) => p,
    };
    let last = |s: &[f64]| s.last().copied().unwrap_or(0.0);
    let peak = |a: &[f64], b: &[f64]| a.iter().chain(b).fold(1024.0f64, |m, x| m.max(*x)) * 1.15;
    let mem_total = p.mem_total.iter().fold(1.0f64, |m, x| m.max(*x));
    let cpus = v.vcpus.max(1);
    // what it uses, when its tools say; otherwise only what it was given
    let memory = if p.mem_inside {
        Tile { title: "Memory", figs: vec![Fig::Big(fmt_bytes(last(&p.mem_used) as i64)), Fig::Soft(format!("of {}", fmt_bytes(mem_total as i64)))], lines: vec![(&p.mem_used, pal.green, "")], max: mem_total, note: None, tone: None }
    } else {
        let note = if v.tools_installed { "Its guest tools don't say how much of it is in use" } else { "No guest tools to say how much of it is in use" };
        let tip = "XenServer only knows how much memory a VM uses when the guest tools inside it report it, and this VM's don't.";
        Tile { title: "Memory", figs: vec![Fig::Big(fmt_bytes(mem_total as i64)), Fig::Soft("given".into())], lines: vec![], max: mem_total, note: Some((note, tip)), tone: None }
    };
    let tiles = [
        Tile { title: "CPU", figs: vec![Fig::Big(format!("{:.0}%", last(&p.cpu) * 100.0)), Fig::Soft(format!("of {} CPU{}", cpus, if cpus == 1 { "" } else { "s" }))], lines: vec![(&p.cpu, pal.accent, "")], max: 1.0, note: None, tone: None },
        memory,
        Tile { title: "Disk", figs: vec![Fig::Dot(pal.blue), Fig::Big(fmt_rate(last(&p.disk_read))), Fig::Dot(pal.orange), Fig::Big(fmt_rate(last(&p.disk_write)))], lines: vec![(&p.disk_read, pal.blue, "read"), (&p.disk_write, pal.orange, "write")], max: peak(&p.disk_read, &p.disk_write), note: None, tone: None },
        Tile { title: "Network", figs: vec![Fig::Dot(pal.blue), Fig::Big(fmt_rate(last(&p.net_rx))), Fig::Dot(pal.orange), Fig::Big(fmt_rate(last(&p.net_tx)))], lines: vec![(&p.net_rx, pal.blue, "in"), (&p.net_tx, pal.orange, "out")], max: peak(&p.net_rx, &p.net_tx), note: None, tone: None },
    ];
    tile_rows(ui, pal, w, &tiles);
}

/// Where the tiles go while there's nothing to draw: why, in a panel as wide as they are (a
/// spinner when it's coming).
pub(super) fn say(ui: &mut Ui, pal: &Pal, text: &str, spin: bool) {
    egui::Frame::none().fill(pal.surface).stroke(Stroke::new(1.0, pal.border.gamma_multiply(0.7))).rounding(Rounding::same(14.0)).inner_margin(egui::Margin::symmetric(16.0, 16.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            if spin {
                ui.add(egui::Spinner::new().size(13.0));
            }
            ui.add(egui::Label::new(egui::RichText::new(text).color(pal.ink_soft)).wrap());
        });
    });
}

/// The tiles in rows: four abreast in a wide page, two in a narrow one.
pub(super) fn tile_rows(ui: &mut Ui, pal: &Pal, w: f32, tiles: &[Tile]) {
    let per_row = if w >= 700.0 { 4 } else { 2 };
    let gap = 12.0;
    let tw = (w - gap * (per_row as f32 - 1.0)) / per_row as f32;
    for (i, row) in tiles.chunks(per_row).enumerate() {
        if i > 0 {
            ui.add_space(gap);
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for t in row {
                tile(ui, pal, tw, t);
            }
        });
    }
}

/// A tile: its title, its figures, its lines (each with its colour and its name, when it has two)
/// and the most they reach; or, instead of lines, a note saying why there are none (and more about
/// it under the mouse).
pub(super) struct Tile<'a> {
    pub title: &'a str,
    pub figs: Vec<Fig>,
    pub lines: Vec<(&'a [f64], Color32, &'a str)>,
    pub max: f64,
    pub note: Option<(&'a str, &'a str)>,
    /// Near a limit: its border in this colour (amber or red).
    pub tone: Option<Color32>,
}

/// A part of a tile's figure: the number (big), what it's of (soft), or a dot in a line's colour.
pub(super) enum Fig {
    Big(String),
    Soft(String),
    Dot(Color32),
}

/// A tile's figures in a row from `at`, smaller when they don't fit in `room`.
pub(super) fn figures(p: &egui::Painter, pal: &Pal, at: egui::Pos2, figs: &[Fig], room: f32) {
    let lay = |big: f32| -> (Vec<(f32, Option<std::sync::Arc<egui::Galley>>, Option<Color32>)>, f32) {
        let mut x = 0.0;
        let mut out = vec![];
        for (i, f) in figs.iter().enumerate() {
            match f {
                Fig::Dot(c) => {
                    x += if i == 0 { 0.0 } else { big * 0.8 };
                    out.push((x, None, Some(*c)));
                    x += 14.0;
                }
                Fig::Big(t) => {
                    let g = p.layout_no_wrap(t.clone(), FontId::new(big, FontFamily::Name("bold".into())), pal.ink);
                    out.push((x, Some(g.clone()), None));
                    x += g.size().x;
                }
                Fig::Soft(t) => {
                    x += 6.0;
                    let g = p.layout_no_wrap(t.clone(), FontId::proportional(big * 0.68), pal.ink_soft);
                    out.push((x, Some(g.clone()), None));
                    x += g.size().x;
                }
            }
        }
        (out, x)
    };
    // the largest size that fits
    let mut parts = vec![];
    for size in [22.0, 18.0, 15.0, 13.0] {
        let (p, wide) = lay(size);
        parts = p;
        if wide <= room {
            break;
        }
    }
    // everything on one baseline: the big figure's
    let base = parts.iter().filter_map(|p| p.1.as_ref()).map(|g| g.size().y).fold(0.0f32, f32::max);
    for (x, g, dot) in parts {
        match (g, dot) {
            (Some(g), _) => {
                let h = g.size().y;
                p.galley(pos2(at.x + x, at.y + (base - h) * 0.82), g, pal.ink);
            }
            (None, Some(c)) => {
                p.circle_filled(pos2(at.x + x + 4.0, at.y + base * 0.55), 4.0, c);
            }
            _ => {}
        }
    }
}

/// A tile: its title (with its lines' names when it has two), its figures now, and a graph of
/// the last ten minutes, the first line over a soft gradient.
pub(super) fn tile(ui: &mut Ui, pal: &Pal, w: f32, t: &Tile) {
    let (title, figs, lines, max) = (t.title, &t.figs, &t.lines, t.max);
    let (r, resp) = ui.allocate_exact_size(vec2(w, 124.0), Sense::hover());
    if let Some((_, tip)) = t.note {
        resp.on_hover_text(tip);
    }
    let p = ui.painter();
    let edge = t.tone.map_or(Stroke::new(1.0, pal.border.gamma_multiply(0.7)), |c| Stroke::new(1.5, c.gamma_multiply(0.8)));
    p.rect(r, Rounding::same(14.0), pal.surface, edge);
    p.text(r.min + vec2(16.0, 18.0), Align2::LEFT_CENTER, title.to_uppercase(), FontId::proportional(11.0), t.tone.unwrap_or(pal.ink_faint));
    // a key for two lines: a dot in each colour, its name
    let mut x = r.max.x - 16.0;
    for (_, col, name) in lines.iter().rev().filter(|l| !l.2.is_empty()) {
        let t = p.text(pos2(x, r.min.y + 18.0), Align2::RIGHT_CENTER, *name, FontId::proportional(11.0), pal.ink_faint);
        p.circle_filled(pos2(t.min.x - 7.0, t.center().y), 3.0, *col);
        x = t.min.x - 18.0;
    }
    figures(p, pal, r.min + vec2(16.0, 30.0), figs, w - 32.0);
    let area = Rect::from_min_max(r.min + vec2(0.0, 64.0), r.max - vec2(0.0, 1.0));
    if let Some((note, _)) = t.note {
        // (on two lines at most)
        let mut job = egui::text::LayoutJob::simple(note.to_string(), FontId::proportional(12.5), pal.ink_faint, w - 32.0);
        job.wrap = egui::text::TextWrapping { max_width: w - 32.0, max_rows: 2, break_anywhere: false, overflow_character: Some('…') };
        let g = p.layout_job(job);
        p.galley(pos2(r.min.x + 16.0, area.center().y - g.size().y / 2.0), g, pal.ink_faint);
    }
    for (i, (series, col, _)) in lines.iter().enumerate() {
        if series.len() < 2 {
            continue;
        }
        let n = series.len() as f32 - 1.0;
        let inner = area.shrink2(vec2(1.0, 4.0));
        let pts: Vec<egui::Pos2> = series
            .iter()
            .enumerate()
            .map(|(k, v)| pos2(inner.min.x + inner.width() * k as f32 / n, inner.max.y - inner.height() * (v / max.max(1e-9)).clamp(0.0, 1.0) as f32))
            .collect();
        if i == 0 {
            // a gradient under the first line, to nothing at the tile's foot
            let top = col.gamma_multiply(if pal.dark { 0.35 } else { 0.25 });
            let mut mesh = egui::Mesh::default();
            for (k, pt) in pts.iter().enumerate() {
                mesh.colored_vertex(*pt, top);
                mesh.colored_vertex(pos2(pt.x, area.max.y - 10.0), Color32::TRANSPARENT);
                if k > 0 {
                    let b = (k as u32) * 2;
                    mesh.add_triangle(b - 2, b - 1, b);
                    mesh.add_triangle(b - 1, b, b + 1);
                }
            }
            p.add(egui::Shape::mesh(mesh));
        }
        p.add(egui::Shape::line(pts, Stroke::new(1.8, *col)));
    }
}

/// GB as people write it: "4 GB", "1.5 GB".
fn gb_text(g: f64) -> String {
    if (g - g.round()).abs() < 1e-6 {
        format!("{} GB", g.round())
    } else {
        format!("{:.1} GB", g)
    }
}

/// CPUs and memory: each a row, a popover with the usual sizes and − / + to pick one, then Apply
/// (it has to be off).
fn resources(ui: &mut Ui, pal: &Pal, v: &VmInfo, at: &Around, ov: &mut OverviewEdit, out: &mut Vec<Intent>) {
    let can = off(v, at);
    let tail = || if can { Tail::Chevron } else { Tail::Lock };
    let mem_now = (v.mem_dyn_max as f64 / GB as f64 * 2.0).round() / 2.0;
    let mem_text = if v.mem_dyn_min > 0 && v.mem_dyn_min < v.mem_dyn_max { format!("{} to {} (dynamic)", fmt_bytes(v.mem_dyn_min), fmt_bytes(v.mem_dyn_max)) } else { gb_text(mem_now) };
    panel(ui, pal, |ui| {
        let (resp, anchor) = row(ui, pal, Some(Lead::Mark("cpu")), "CPUs", &v.vcpus.max(1).to_string(), tail(), can, false);
        let resp = if can { resp } else { resp.on_hover_text(needs_off(v, at)) };
        if resp.clicked() {
            ov.cpus = Some(v.vcpus.max(1));
        }
        popover(ui, resp.id.with("pop"), resp.clicked(), &anchor, 280.0, |ui| {
            pop_head(ui, pal, "CPUs", &format!("now {}", v.vcpus.max(1)));
            let mut n = ov.cpus.unwrap_or(v.vcpus.max(1));
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                for c in [1, 2, 4, 8, 16, 32].into_iter().filter(|c| *c <= at.most_cpus) {
                    if chip(ui, pal, &c.to_string(), n == c) {
                        n = c;
                    }
                }
            });
            ui.horizontal(|ui| {
                if round_button(ui, pal, false, n > 1) {
                    n -= 1;
                }
                let (r, _) = ui.allocate_exact_size(vec2(60.0, 30.0), Sense::hover());
                ui.painter().text(r.center(), Align2::CENTER_CENTER, n.to_string(), FontId::new(16.0, FontFamily::Name("bold".into())), pal.ink);
                if round_button(ui, pal, true, n < at.most_cpus) {
                    n += 1;
                }
            });
            ov.cpus = Some(n);
            if primary(ui, pal, "Apply", n != v.vcpus, true) {
                out.push(Intent::EditVm(at.sid, v.rref.clone(), VmChange::Resources { vcpus: n, cores_per_socket: n, memory: v.mem_dyn_max }));
                close_popover(ui);
            }
        });
        let (resp, anchor) = row(ui, pal, Some(Lead::Mark("memory")), "Memory", &mem_text, tail(), can, true);
        let resp = if can { resp } else { resp.on_hover_text(needs_off(v, at)) };
        if resp.clicked() {
            ov.mem = Some(mem_now);
        }
        popover(ui, resp.id.with("pop"), resp.clicked(), &anchor, 300.0, |ui| {
            pop_head(ui, pal, "Memory", &format!("now {}", mem_text));
            let most = if at.most_memory > 0 { at.most_memory as f64 / GB as f64 } else { 1024.0 };
            let mut g = ov.mem.unwrap_or(mem_now);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                for c in [1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0].into_iter().filter(|c| *c <= most) {
                    if chip(ui, pal, &gb_text(c), (g - c).abs() < 1e-6) {
                        g = c;
                    }
                }
            });
            ui.horizontal(|ui| {
                // half a GB at a time up to 2 GB, then whole ones
                if round_button(ui, pal, false, g > 0.5) {
                    g = (g - if g <= 2.0 { 0.5 } else { 1.0 }).max(0.5);
                }
                let (r, _) = ui.allocate_exact_size(vec2(80.0, 30.0), Sense::hover());
                ui.painter().text(r.center(), Align2::CENTER_CENTER, gb_text(g), FontId::new(16.0, FontFamily::Name("bold".into())), pal.ink);
                if round_button(ui, pal, true, g + 0.5 <= most) {
                    g += if g < 2.0 { 0.5 } else { 1.0 };
                }
            });
            ov.mem = Some(g);
            if v.mem_dyn_min > 0 && v.mem_dyn_min < v.mem_dyn_max {
                pop_note(ui, pal, "Its memory is dynamic now: a size set here is fixed.", false);
            }
            let bytes = (g * GB as f64) as i64;
            let changed = bytes != v.mem_dyn_max || v.mem_dyn_min != v.mem_dyn_max;
            if primary(ui, pal, "Apply", changed, true) {
                out.push(Intent::EditVm(at.sid, v.rref.clone(), VmChange::Resources { vcpus: v.vcpus.max(1), cores_per_socket: v.vcpus.max(1), memory: bytes }));
                close_popover(ui);
            }
        });
    });
}

/// Starting with its server (a switch), the boot order (a BIOS VM's) and its firmware.
fn startup(ui: &mut Ui, pal: &Pal, v: &VmInfo, at: &Around, out: &mut Vec<Intent>) {
    let can = free(at);
    panel(ui, pal, |ui| {
        let (resp, _) = row(ui, pal, Some(Lead::Mark("power")), "Starts with its server", "", Tail::Switch(v.auto_power_on), can, false);
        if resp.on_hover_text("Started when its server starts, after a power cut say").clicked() {
            out.push(Intent::EditVm(at.sid, v.rref.clone(), VmChange::AutoPowerOn(!v.auto_power_on)));
        }
        if v.virt_mode == "HVM" && v.firmware == "BIOS" {
            let now = boot_letters(&v.boot_order);
            let (resp, anchor) = row(ui, pal, Some(Lead::Mark("boot")), "Boot order", &boot_text(&now).replace(", ", " › "), Tail::Chevron, can, false);
            popover(ui, resp.id.with("pop"), resp.clicked(), &anchor, 280.0, |ui| {
                pop_head(ui, pal, "Boot order", "");
                ui.spacing_mut().item_spacing.y = 2.0;
                let mut orders: Vec<String> = ["cdn", "dcn", "ncd", "cnd", "dnc", "ndc"].iter().map(|s| s.to_string()).collect();
                if !orders.contains(&now) {
                    orders.insert(0, now.clone());
                }
                for o in &orders {
                    if choice(ui, pal, &boot_text(o).replace(", ", " › "), "", *o == now, true) {
                        if *o != now {
                            out.push(Intent::EditVm(at.sid, v.rref.clone(), VmChange::BootOrder(o.clone())));
                        }
                        close_popover(ui);
                    }
                }
            });
        } else if v.firmware == "UEFI" {
            row(ui, pal, Some(Lead::Mark("boot")), "Boot order", "in its UEFI boot menu", Tail::Nothing, false, false).0.on_hover_text("A UEFI VM keeps where it boots from in its own firmware");
        }
        let firmware = match (v.firmware.as_str(), v.secure_boot.as_str()) {
            ("", _) => or(&v.virt_mode, "—").to_string(),
            ("UEFI", sb) if !sb.is_empty() => format!("UEFI  ·  Secure Boot {}", sb.to_lowercase()),
            (f, _) => f.to_string(),
        };
        row(ui, pal, Some(Lead::Mark("shield")), "Firmware", &firmware, Tail::Nothing, false, true);
    });
}

/// Its disks (actions to grow or remove one), its DVD drive (an ISO in it, or none) and a disk to
/// add.
fn disks(ui: &mut Ui, pal: &Pal, v: &VmInfo, at: &Around, ov: &mut OverviewEdit, out: &mut Vec<Intent>) {
    let (can_off, can) = (off(v, at), free(at));
    panel(ui, pal, |ui| {
        for k in v.disks.iter().filter(|k| k.kind == "Disk") {
            let value = format!("{}  ·  {}", fmt_bytes(k.size), or(&k.sr_name, "storage unknown"));
            let (resp, anchor) = row(ui, pal, Some(Lead::Icon(IconKind::Sr)), &format!("Disk {}", k.device), &value, Tail::More, can, false);
            if resp.clicked() {
                ov.grow = format!("{}", (k.size as f64 / GB as f64).ceil() + 10.0);
            }
            popover(ui, resp.id.with("pop"), resp.clicked(), &anchor, 300.0, |ui| {
                pop_head(ui, pal, &format!("Disk {}", k.device), &fmt_bytes(k.size));
                pop_note(ui, pal, &format!("On {}", or(&k.sr_name, "storage unknown")), false);
                pop_line(ui, pal);
                if can_off {
                    pop_label(ui, pal, "Grow it to");
                    let size = ov.grow.trim().replace(',', ".").parse::<f64>().ok().map(|x| (x * GB as f64) as i64);
                    let ok = size.map_or(false, |s| s > k.size);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        size_field(ui, pal, &mut ov.grow, ui.available_width() - 90.0);
                        if primary(ui, pal, "Grow", ok, true) {
                            out.push(Intent::EditVm(at.sid, v.rref.clone(), VmChange::GrowDisk { vdi: k.vdi.clone(), size: size.unwrap_or(k.size) }));
                            close_popover(ui);
                        }
                    });
                    pop_note(ui, pal, "A disk can only grow, never shrink.", false);
                } else {
                    let why = match (&at.down, v.power) {
                        (Some(_), _) => "Its server isn't there, so it can't be changed now.",
                        (None, PowerState::Halted) => "Wait for what's being done to finish.",
                        _ => "Shut it down to grow or remove it.",
                    };
                    pop_note(ui, pal, why, true);
                }
                pop_line(ui, pal);
                if danger(ui, pal, "Remove this disk…", can_off, needs_off(v, at)) {
                    let text = format!("Remove Disk {} ({}, on {}) from '{}'?\n\nThe disk is deleted with everything on it (unless another VM uses it too). This can't be undone.", k.device, fmt_bytes(k.size), or(&k.sr_name, "its storage"), v.name);
                    out.push(Intent::AskEditVm(at.sid, v.rref.clone(), VmChange::RemoveDisk { vbd: k.vbd.clone(), vdi: k.vdi.clone() }, "Remove Disk".into(), text));
                    close_popover(ui);
                }
            });
        }
        // the DVD drive: an ISO from the libraries it sees, or none
        for k in v.disks.iter().filter(|k| k.kind != "Disk") {
            let empty = k.vdi.is_empty() || k.name.is_empty() || k.name == "(empty)";
            let (resp, anchor) = row(ui, pal, Some(Lead::Icon(IconKind::IsoSr)), "DVD drive", if empty { "Empty" } else { &k.name }, Tail::Chevron, can, false);
            if resp.clicked() {
                ov.dvd_search.clear();
            }
            popover(ui, resp.id.with("pop"), resp.clicked(), &anchor, 380.0, |ui| {
                pop_head(ui, pal, "In the DVD drive", &format!("{} ISO{}", at.isos.len(), if at.isos.len() == 1 { "" } else { "s" }));
                // many: a box to find one
                if at.isos.len() > 8 {
                    ui.add(egui::TextEdit::singleline(&mut ov.dvd_search).hint_text("Find an ISO").desired_width(f32::INFINITY).margin(vec2(8.0, 6.0)));
                }
                let find = ov.dvd_search.trim().to_lowercase();
                egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    if choice(ui, pal, "Empty", "", empty, true) {
                        if !empty {
                            out.push(Intent::EditVm(at.sid, v.rref.clone(), VmChange::Dvd { vbd: k.vbd.clone(), iso: None }));
                        }
                        close_popover(ui);
                    }
                    for (vdi, name, lib) in at.isos.iter().filter(|i| find.is_empty() || i.1.to_lowercase().contains(&find)) {
                        let on = !empty && *vdi == k.vdi;
                        if choice(ui, pal, name, lib, on, true) {
                            if !on {
                                out.push(Intent::EditVm(at.sid, v.rref.clone(), VmChange::Dvd { vbd: k.vbd.clone(), iso: Some(vdi.clone()) }));
                            }
                            close_popover(ui);
                        }
                    }
                });
            });
        }
        // a new disk: its size and its storage
        let (resp, anchor) = row(ui, pal, Some(Lead::Glyph("plus")), "Add disk", "", Tail::Nothing, can && !at.stores.is_empty(), true);
        if resp.clicked() {
            ov.new_disk = ("20".into(), at.stores.first().map(|s| s.0.clone()).unwrap_or_default());
        }
        popover(ui, resp.id.with("pop"), resp.clicked(), &anchor, 320.0, |ui| {
            pop_head(ui, pal, "New disk", "");
            pop_label(ui, pal, "Size");
            size_field(ui, pal, &mut ov.new_disk.0, 120.0);
            pop_label(ui, pal, "On the storage");
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for (r, name, room) in &at.stores {
                    if choice(ui, pal, name, &format!("{} free", fmt_bytes(*room)), ov.new_disk.1 == *r, true) {
                        ov.new_disk.1 = r.clone();
                    }
                }
            });
            let size = ov.new_disk.0.trim().replace(',', ".").parse::<f64>().ok().filter(|x| *x > 0.0).map(|x| (x * GB as f64) as i64);
            let room = at.stores.iter().find(|s| s.0 == ov.new_disk.1).map_or(0, |s| s.2);
            if size.map_or(false, |s| s > room) {
                ui.label(egui::RichText::new("There isn't that much room there.").size(12.0).color(pal.red));
            }
            if primary(ui, pal, "Add disk", size.map_or(false, |s| s <= room), true) {
                let next = v.disks.iter().filter(|k| k.kind == "Disk").count();
                out.push(Intent::EditVm(at.sid, v.rref.clone(), VmChange::AddDisk { name: format!("{} disk {}", v.name, next), size: size.unwrap_or(0), sr: ov.new_disk.1.clone() }));
                close_popover(ui);
            }
        });
    });
}

/// Its network cards (another network for one, or remove it) and a card to add.
fn network(ui: &mut Ui, pal: &Pal, v: &VmInfo, at: &Around, out: &mut Vec<Intent>) {
    let can = free(at);
    panel(ui, pal, |ui| {
        for n in &v.nics {
            let value = if n.ip.is_empty() { or(&n.network, "no network").to_string() } else { format!("{}  ·  {}", or(&n.network, "no network"), n.ip) };
            let (resp, anchor) = row(ui, pal, Some(Lead::Icon(IconKind::Net)), &format!("Card {}", n.device), &value, Tail::More, can, false);
            popover(ui, resp.id.with("pop"), resp.clicked(), &anchor, 300.0, |ui| {
                pop_head(ui, pal, &format!("Card {}", n.device), &n.mac);
                pop_label(ui, pal, "On the network");
                ui.scope(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for (r, name) in &at.nets {
                        if choice(ui, pal, name, "", *r == n.net, true) {
                            if *r != n.net {
                                out.push(Intent::EditVm(at.sid, v.rref.clone(), VmChange::MoveNic { vif: n.vif.clone(), net: r.clone() }));
                            }
                            close_popover(ui);
                        }
                    }
                });
                pop_line(ui, pal);
                if danger(ui, pal, "Remove this card…", true, "") {
                    let unplug = if v.power == PowerState::Running { " It's unplugged first: the VM loses that connection." } else { "" };
                    let text = format!("Remove network card {} (on {}) from '{}'?{}", n.device, or(&n.network, "its network"), v.name, unplug);
                    out.push(Intent::AskEditVm(at.sid, v.rref.clone(), VmChange::RemoveNic { vif: n.vif.clone() }, "Remove Network Card".into(), text));
                    close_popover(ui);
                }
            });
        }
        let (resp, anchor) = row(ui, pal, Some(Lead::Glyph("plus")), "Add network card", "", Tail::Nothing, can && !at.nets.is_empty(), true);
        popover(ui, resp.id.with("pop"), resp.clicked(), &anchor, 280.0, |ui| {
            pop_head(ui, pal, "New network card", "");
            pop_label(ui, pal, "On the network");
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for (r, name) in &at.nets {
                    if choice(ui, pal, name, "", false, true) {
                        out.push(Intent::EditVm(at.sid, v.rref.clone(), VmChange::AddNic { net: r.clone() }));
                        close_popover(ui);
                    }
                }
            });
        });
    });
}
