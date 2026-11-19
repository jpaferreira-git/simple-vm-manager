//! A VM's or a server's Performance tab: how it's done over the last ten minutes, two hours, week
//! or year, as its server keeps it. CPU, memory, disk and network as four charts; then everything
//! else the server measures, item by item (each CPU, disk, card, storage…), opened with a click.
//! Under the mouse, a chart says its figures at that time.

use super::overview::{figures_sized, fit, panel, row, say, section, Fig, Lead, Tail};
use super::*;
use crate::alerts::Watched;
use crate::history::*;

/// The charts made from one reading: whose, over what span, and from the reading of when.
pub(super) type PerfCharts = (String, Span, Instant, Arc<(Charts, Vec<i64>)>);

/// What's shown instead of charts, and whether it's coming.
enum Shown {
    Why(String, bool),
    Charts(Arc<(Charts, Vec<i64>)>),
}

impl App {
    pub(super) fn performance(&mut self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let span = self.perf_span;
        // whose records, where they're kept, and why they can't be read now (if not)
        let (conn, key, address, vm_uuid, why_not, rules) = if let Some((s, d, v)) = self.vm_of(sel) {
            let why = if d.host_down_for(v).is_some() {
                Some("Its server isn't answering, so its records can't be read.")
            } else {
                match v.power {
                    PowerState::Running => None,
                    PowerState::Suspended => Some("Suspended: its records show here once it's resumed."),
                    _ => Some("Its records show here while it runs."),
                }
            };
            let address = v.host.as_deref().and_then(|h| d.host(h)).map(|h| h.address.clone()).unwrap_or_default();
            (s.conn.clone(), v.rref.clone(), address, Some(v.uuid.clone()), why, Intent::AlertRules(s.id, "VM", v.rref.clone(), v.name.clone(), Watched::Vm))
        } else if let Some((s, h)) = self.host_of(sel) {
            let why = if !h.live {
                Some("It's off: its records show here while it's on.")
            } else if h.unreachable {
                Some("It isn't answering, so its records can't be read.")
            } else {
                None
            };
            (s.conn.clone(), h.rref.clone(), h.address.clone(), None, why, Intent::AlertRules(s.id, "host", h.rref.clone(), h.name.clone(), Watched::Host))
        } else {
            return;
        };
        let mut rules = Some(rules);
        // read while shown, about as often as there's a new point
        if why_not.is_none() {
            let k = (key.clone(), span);
            let due = self.history.get(&k).map_or(true, |(at, _)| at.elapsed() >= span.every());
            if due && self.history_busy.insert(k) {
                let (tx, ctx, key) = (self.tx.clone(), ui.ctx().clone(), key.clone());
                std::thread::spawn(move || {
                    let result = conn.history(&address, vm_uuid.as_deref(), span);
                    let _ = tx.send(Msg::History { key, span, result });
                    ctx.request_repaint();
                });
            }
            ui.ctx().request_repaint_after(span.every());
        }
        let mut new_charts = None;
        let shown = match (why_not, self.history.get(&(key.clone(), span))) {
            (Some(why), _) => Shown::Why(why.into(), false),
            (None, None) => Shown::Why("Reading its records…".into(), true),
            (None, Some((_, Err(e)))) => Shown::Why(format!("Couldn't read its records: {}", e), false),
            (None, Some((at, Ok(r)))) => {
                // made again only from a new reading
                let made = self.perf_charts.as_ref().filter(|c| c.0 == key && c.1 == span && c.2 == *at).map(|c| c.3.clone());
                let made = made.unwrap_or_else(|| {
                    let charts = match (self.vm_of(sel), self.host_of(sel)) {
                        (Some((_, _, v)), _) => vm_charts(r, v),
                        (_, Some((s, h))) => s.data.as_ref().map(|d| host_charts(r, h, d)).unwrap_or_default(),
                        _ => Charts::default(),
                    };
                    let made = Arc::new((charts, r.times.clone()));
                    new_charts = Some((key.clone(), span, *at, made.clone()));
                    made
                });
                Shown::Charts(made)
            }
        };
        if new_charts.is_some() {
            self.perf_charts = new_charts;
        }
        // (a check's screenshot shows the CPU chart's figures at its peak, as under the mouse)
        let pin = self.shot_path.is_some();
        let mut pick = None;
        let mut fold = None;
        let open = &self.perf_open;
        egui::ScrollArea::vertical().id_salt("performance").auto_shrink([false; 2]).show(ui, |ui| {
            let pad = 20.0;
            let w = ui.available_width() - 2.0 * pad;
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                ui.add_space(pad);
                ui.vertical(|ui| {
                    ui.set_width(w);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    // the span's note beside the choices, or under them when it doesn't fit; at the
                    // right, the limits it's watched for
                    let note = egui::RichText::new(span.note()).size(12.5).color(pal.ink_soft);
                    let note_w = ui.painter().layout_no_wrap(span.note().into(), FontId::proportional(12.5), pal.ink).size().x;
                    let mut alerts_button = |ui: &mut Ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let tip = "Get an alert when it passes a limit you set (the pool watches, also while this app is closed)";
                            if flat_button(ui, "bell", "Alerts…", Some(hue(&pal, Hue::Orange)), true, false, &pal).on_hover_text(tip).clicked() {
                                if let Some(i) = rules.take() {
                                    out.push(i);
                                }
                            }
                        });
                    };
                    if w >= 520.0 + note_w {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 14.0;
                            pick = span_picker(ui, &pal, span);
                            ui.label(note);
                            alerts_button(ui);
                        });
                    } else {
                        ui.horizontal(|ui| {
                            pick = span_picker(ui, &pal, span);
                            alerts_button(ui);
                        });
                        ui.add_space(8.0);
                        ui.label(note);
                    }
                    ui.add_space(16.0);
                    match &shown {
                        Shown::Why(text, spin) => say(ui, &pal, text, *spin),
                        Shown::Charts(made) => {
                            let (charts, times) = (&made.0, &made.1);
                            let gap = 14.0;
                            let per = if w >= 760.0 { 2 } else { 1 };
                            let cw = (w - gap * (per as f32 - 1.0)) / per as f32;
                            for (i, two) in charts.main.chunks(per).enumerate() {
                                if i > 0 {
                                    ui.add_space(gap);
                                }
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = gap;
                                    for c in two {
                                        let pin = if pin && c.title == "CPU" { c.lines.first().and_then(|l| index_of_peak(&l.vals)) } else { None };
                                        chart_card(ui, &pal, cw, c, times, span, false, pal.surface, pin);
                                    }
                                });
                            }
                            if !charts.groups.is_empty() {
                                ui.add_space(26.0);
                                section(ui, &pal, "Everything else it measures");
                                panel(ui, &pal, |ui| {
                                    let n = charts.groups.len();
                                    for (i, g) in charts.groups.iter().enumerate() {
                                        let is_open = open.contains(&g.key);
                                        let last = i + 1 == n;
                                        let lead = match g.kind {
                                            GroupKind::Vms => Lead::Icon(IconKind::Vm(PowerState::Running)),
                                            GroupKind::Cpus => Lead::Mark("cpu"),
                                            GroupKind::Load => Lead::Mark("activity"),
                                            GroupKind::Disk | GroupKind::Storage => Lead::Icon(IconKind::Sr),
                                            GroupKind::Card => Lead::Icon(IconKind::Net),
                                        };
                                        let (resp, _) = row(ui, &pal, Some(lead), &g.name, &g.detail, Tail::Fold(is_open), true, last || is_open);
                                        if resp.clicked() {
                                            fold = Some(g.key.clone());
                                        }
                                        if is_open {
                                            minis(ui, &pal, &g.charts, times, span);
                                            if !last {
                                                let r = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover()).0;
                                                ui.painter().line_segment([r.left_center(), r.right_center()], Stroke::new(1.0_f32, pal.border.gamma_multiply(0.6)));
                                            }
                                        }
                                    }
                                });
                            }
                        }
                    }
                    ui.add_space(24.0);
                });
            });
        });
        if let Some(s) = pick {
            self.perf_span = s;
        }
        if let Some(k) = fold {
            if !self.perf_open.remove(&k) {
                self.perf_open.insert(k);
            }
        }
    }
}

/// How far back: four choices side by side, the one shown filled. The one clicked, if another.
fn span_picker(ui: &mut Ui, pal: &Pal, current: Span) -> Option<Span> {
    let labels: Vec<String> = Span::ALL.iter().map(|s| s.label().to_string()).collect();
    let at = Span::ALL.iter().position(|s| *s == current).unwrap_or(0);
    segmented(ui, pal, &labels, at).map(|i| Span::ALL[i])
}

/// Choices side by side in a rounded box, the current one filled: the one clicked, if another.
pub(super) fn segmented(ui: &mut Ui, pal: &Pal, labels: &[String], current: usize) -> Option<usize> {
    let font = FontId::proportional(13.0);
    let widths: Vec<f32> = labels.iter().map(|l| ui.painter().layout_no_wrap(l.clone(), font.clone(), pal.ink).size().x + 28.0).collect();
    let (r, _) = ui.allocate_exact_size(vec2(widths.iter().sum::<f32>() + 6.0, 34.0), Sense::hover());
    let p = ui.painter().clone();
    p.rect(r, Rounding::same(9.0), pal.surface, Stroke::new(1.0_f32, pal.border));
    let mut x = r.min.x + 3.0;
    let mut out = None;
    for (i, (label, w)) in labels.iter().zip(&widths).enumerate() {
        let b = Rect::from_min_size(pos2(x, r.min.y + 3.0), vec2(*w, r.height() - 6.0));
        let resp = ui.interact(b, ui.id().with(("segmented", i, label.as_str())), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
        let on = i == current;
        if on {
            p.rect_filled(b, Rounding::same(7.0), pal.accent);
        } else if resp.hovered() {
            p.rect_filled(b, Rounding::same(7.0), pal.hover);
        }
        let (f, col) = if on { (FontId::new(13.0, FontFamily::Name("bold".into())), pal.on_accent) } else { (font.clone(), pal.ink_soft) };
        p.text(b.center(), Align2::CENTER_CENTER, label, f, col);
        if resp.clicked() && !on {
            out = Some(i);
        }
        x += w;
    }
    out
}

/// A line's colour.
fn hue(pal: &Pal, h: Hue) -> Color32 {
    let pick = |dark: (u8, u8, u8), light: (u8, u8, u8)| {
        let (r, g, b) = if pal.dark { dark } else { light };
        Color32::from_rgb(r, g, b)
    };
    match h {
        Hue::Accent => pal.accent,
        Hue::Green => pal.green,
        Hue::Blue => pal.blue,
        Hue::Orange => pal.orange,
        Hue::Grey => pal.grey,
        Hue::Purple => pick((180, 140, 242), (124, 77, 204)),
        Hue::Teal => pick((57, 197, 187), (14, 140, 130)),
        Hue::Pink => pick((236, 120, 180), (196, 60, 130)),
    }
}

/// An open group's charts, inside its panel: a wide one alone on its row, the rest as many to a
/// row as fit (up to four).
fn minis(ui: &mut Ui, pal: &Pal, charts: &[Chart], times: &[i64], span: Span) {
    let (pad, gap) = (16.0, 12.0);
    let w = ui.available_width() - 2.0 * pad;
    let per = (((w + gap) / (230.0 + gap)).floor() as usize).clamp(1, 4);
    let cw = (w - gap * (per as f32 - 1.0)) / per as f32;
    let mut rows: Vec<Vec<&Chart>> = vec![];
    for c in charts {
        match rows.last_mut() {
            Some(r) if !c.wide && !r[0].wide && r.len() < per => r.push(c),
            _ => rows.push(vec![c]),
        }
    }
    ui.add_space(4.0);
    for (i, r) in rows.iter().enumerate() {
        if i > 0 {
            ui.add_space(gap);
        }
        ui.horizontal(|ui| {
            ui.add_space(pad);
            ui.spacing_mut().item_spacing.x = gap;
            for c in r {
                chart_card(ui, pal, if c.wide { w } else { cw }, c, times, span, true, pal.window, None);
            }
        });
    }
    ui.add_space(14.0);
}

/// A chart in its card: its title (and a key to its lines), its figures (now, or the average,
/// with its peak and what it's out of), the lines over its scale with the times under them, a
/// hatched stretch where there are no records yet, and under the mouse (or at `pin`) each line's
/// figure at that point. `mini`: smaller, inside an open group.
#[allow(clippy::too_many_arguments)]
fn chart_card(ui: &mut Ui, pal: &Pal, w: f32, c: &Chart, times: &[i64], span: Span, mini: bool, fill: Color32, pin: Option<usize>) {
    let plot_h = if !mini {
        170.0
    } else if c.wide {
        110.0
    } else {
        88.0
    };
    let pad = if mini { 14.0 } else { 16.0 };
    let head = if mini { 54.0 } else { 70.0 };
    let (r, resp) = ui.allocate_exact_size(vec2(w, head + plot_h + 28.0), Sense::hover());
    let p = ui.painter().clone();
    p.rect(r, Rounding::same(if mini { 10.0 } else { 14.0 }), fill, Stroke::new(1.0_f32, pal.border.gamma_multiply(0.7)));
    let title_y = r.min.y + if mini { 15.0 } else { 18.0 };
    p.text(pos2(r.min.x + pad, title_y), Align2::LEFT_CENTER, c.title.to_uppercase(), FontId::proportional(11.0), pal.ink_faint);
    let cols: Vec<Color32> = c.lines.iter().map(|l| hue(pal, l.hue)).collect();
    let crowd = c.lines.len() > 3;
    // a key to two or three lines, at the top right
    if c.lines.len() > 1 && !crowd {
        let mut x = r.max.x - pad;
        for (l, col) in c.lines.iter().zip(&cols).rev() {
            let t = p.text(pos2(x, title_y), Align2::RIGHT_CENTER, &l.name, FontId::proportional(11.0), pal.ink_faint);
            p.circle_filled(pos2(t.min.x - 7.0, t.center().y), 3.0, *col);
            x = t.min.x - 18.0;
        }
    }
    // its figures: now (over ten minutes) or the average
    let live = span == Span::Minutes;
    let figure = |l: &Line| (if live { last(&l.vals) } else { average(&l.vals) }).map_or("–".to_string(), |v| fmt_value(c.unit, v));
    let word = if live { "now" } else { "average" };
    let mut figs = vec![];
    if crowd {
        // many lines: their names, each in its colour
        for (l, col) in c.lines.iter().zip(&cols) {
            figs.push(Fig::Dot(*col));
            figs.push(Fig::Soft(l.name.clone()));
        }
    } else if c.lines.len() > 1 {
        for (l, col) in c.lines.iter().zip(&cols) {
            figs.push(Fig::Dot(*col));
            figs.push(Fig::Big(figure(l)));
        }
        figs.push(Fig::Soft(word.into()));
    } else if let Some(l) = c.lines.first() {
        figs.push(Fig::Big(figure(l)));
        let mut soft = word.to_string();
        if !mini {
            if let Some(top) = peak(&l.vals) {
                soft += &format!("  ·  peak {}", fmt_value(c.unit, top));
            }
            if !c.of.is_empty() {
                soft += &format!("  ·  of {}", c.of);
            }
        }
        figs.push(Fig::Soft(soft));
    }
    let sizes: &[f32] = if mini { &[16.0, 14.0, 12.0] } else { &[22.0, 18.0, 15.0, 13.0] };
    figures_sized(&p, pal, pos2(r.min.x + pad, title_y + if mini { 11.0 } else { 13.0 }), &figs, w - 2.0 * pad, sizes);
    // the plot: its scale's top and half way at its right, the lines over it
    let top = c.scale();
    let small = FontId::proportional(10.5);
    let marks = [fmt_axis(c.unit, top), fmt_axis(c.unit, top / 2.0)].map(|t| p.layout_no_wrap(t, small.clone(), pal.ink_faint));
    let gutter = marks.iter().map(|g| g.size().x).fold(0.0, f32::max) + 10.0;
    let plot = Rect::from_min_size(pos2(r.min.x + pad, r.min.y + head), vec2(w - 2.0 * pad - gutter, plot_h));
    for (g, f) in marks.into_iter().zip([0.0, 0.5]) {
        let y = plot.min.y + plot.height() * f;
        p.galley(pos2(r.max.x - pad - g.size().x, y - g.size().y / 2.0), g, pal.ink_faint);
    }
    let n = times.len();
    let x_at = |i: usize| plot.min.x + if n > 1 { plot.width() * i as f32 / (n - 1) as f32 } else { plot.width() / 2.0 };
    let y_at = |v: f64| plot.max.y - plot.height() * (v / top).clamp(0.0, 1.0) as f32;
    for f in [0.0, 0.5] {
        let y = plot.min.y + plot.height() * f;
        p.line_segment([pos2(plot.min.x, y), pos2(plot.max.x, y)], Stroke::new(1.0_f32, pal.border.gamma_multiply(0.45)));
    }
    p.line_segment([plot.left_bottom(), plot.right_bottom()], Stroke::new(1.0_f32, pal.border));
    // before its records begin: hatched, and said
    match c.lines.iter().filter_map(|l| first_there(&l.vals)).min() {
        None => {
            p.text(plot.center(), Align2::CENTER_CENTER, "No records for this time", FontId::proportional(12.5), pal.ink_faint);
        }
        Some(f) if f > 0 => {
            let gx = x_at(f);
            let gap = Rect::from_min_max(plot.min, pos2(gx, plot.max.y));
            let hp = p.with_clip_rect(gap.intersect(p.clip_rect()));
            let mut k = -gap.height();
            while k < gap.width() {
                hp.line_segment([pos2(gap.min.x + k, gap.max.y), pos2(gap.min.x + k + gap.height(), gap.min.y)], Stroke::new(1.0_f32, pal.border.gamma_multiply(0.4)));
                k += 9.0;
            }
            p.extend(egui::Shape::dashed_line(&[pos2(gx, plot.min.y), pos2(gx, plot.max.y)], Stroke::new(1.0_f32, pal.border_strong), 3.0, 3.0));
            if gap.width() > 90.0 {
                let t = times[f];
                let when = if matches!(span, Span::Week | Span::Year) { fmt_day(t, local_offset(t)) } else { fmt_point(t, span, local_offset(t)) };
                let (g, _) = fit(&p, &format!("No records before {}", when), FontId::proportional(if mini { 11.0 } else { 12.0 }), pal.ink_soft, gap.width() - 16.0);
                let at = gap.center() - g.size() / 2.0;
                p.rect_filled(Rect::from_min_size(at, g.size()).expand2(vec2(5.0, 2.0)), Rounding::same(4.0), fill);
                p.galley(at, g, pal.ink_soft);
            }
        }
        _ => {}
    }
    // each line's stretches between gaps; the first line over a soft gradient (unless many)
    let runs: Vec<Vec<Vec<egui::Pos2>>> = c
        .lines
        .iter()
        .map(|l| {
            let mut out = vec![];
            let mut cur = vec![];
            for (i, v) in l.vals.iter().enumerate() {
                if v.is_finite() {
                    cur.push(pos2(x_at(i), y_at(*v)));
                } else if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            if !cur.is_empty() {
                out.push(cur);
            }
            out
        })
        .collect();
    if let (Some(first), Some(col), false) = (runs.first(), cols.first(), crowd) {
        let shade = col.gamma_multiply(if pal.dark { 0.32 } else { 0.22 });
        for run in first.iter().filter(|r| r.len() > 1) {
            let mut mesh = egui::Mesh::default();
            for (k, pt) in run.iter().enumerate() {
                mesh.colored_vertex(*pt, shade);
                mesh.colored_vertex(pos2(pt.x, plot.max.y), Color32::TRANSPARENT);
                if k > 0 {
                    let b = (k as u32) * 2;
                    mesh.add_triangle(b - 2, b - 1, b);
                    mesh.add_triangle(b - 1, b, b + 1);
                }
            }
            p.add(egui::Shape::mesh(mesh));
        }
    }
    let width = if mini { 1.5_f32 } else { 1.8_f32 };
    for (line, col) in runs.into_iter().zip(&cols).rev() {
        for run in line {
            if run.len() == 1 {
                p.circle_filled(run[0], 1.5, *col);
            } else {
                p.add(egui::Shape::line(run, Stroke::new(width, *col)));
            }
        }
    }
    // the times under it: every one, or every other (every third…) when they don't all fit
    let labels: Vec<(f32, std::sync::Arc<egui::Galley>)> = time_marks(times, span, &local_offset)
        .into_iter()
        .map(|(i, label)| (x_at(i), p.layout_no_wrap(label, FontId::proportional(11.0), pal.ink_faint)))
        .filter(|(x, g)| x - g.size().x / 2.0 >= plot.min.x && x + g.size().x / 2.0 <= plot.max.x)
        .collect();
    let fits = |k: usize| labels.iter().step_by(k).collect::<Vec<_>>().windows(2).all(|w| w[1].0 - w[1].1.size().x / 2.0 >= w[0].0 + w[0].1.size().x / 2.0 + 12.0);
    let k = (1..=labels.len().max(1)).find(|k| fits(*k)).unwrap_or(1);
    for (x, g) in labels.into_iter().step_by(k) {
        p.galley(pos2(x - g.size().x / 2.0, plot.max.y + 6.0), g, pal.ink_faint);
    }
    // under the mouse: each line's figure at that time
    let near = resp.hover_pos().filter(|h| h.x >= plot.min.x - 4.0 && h.x <= plot.max.x + 4.0).map(|h| {
        let f = if n > 1 { (h.x - plot.min.x) / plot.width() * (n - 1) as f32 } else { 0.0 };
        f.round().clamp(0.0, n.saturating_sub(1) as f32) as usize
    });
    let Some(i) = near.or(pin).filter(|i| *i < n && c.lines.iter().any(|l| l.vals.get(*i).map_or(false, |v| v.is_finite()))) else { return };
    let x = x_at(i);
    p.line_segment([pos2(x, plot.min.y), pos2(x, plot.max.y)], Stroke::new(1.0_f32, pal.ink.gamma_multiply(0.35)));
    for (l, col) in c.lines.iter().zip(&cols) {
        if let Some(v) = l.vals.get(i).filter(|v| v.is_finite()) {
            p.circle_filled(pos2(x, y_at(*v)), 5.0, fill);
            p.circle_filled(pos2(x, y_at(*v)), 3.5, *col);
        }
    }
    let lp = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, resp.id.with("figures")));
    let time = lp.layout_no_wrap(fmt_point(times[i], span, local_offset(times[i])), FontId::proportional(12.0), pal.ink_soft);
    let lines: Vec<(Color32, std::sync::Arc<egui::Galley>, std::sync::Arc<egui::Galley>)> = c
        .lines
        .iter()
        .zip(&cols)
        .map(|(l, col)| {
            let v = l.vals.get(i).filter(|v| v.is_finite()).map_or("–".to_string(), |v| fmt_value(c.unit, *v));
            (*col, lp.layout_no_wrap(l.name.clone(), FontId::proportional(13.0), pal.ink_soft), lp.layout_no_wrap(v, FontId::new(13.0, FontFamily::Name("bold".into())), pal.ink))
        })
        .collect();
    let inner = lines.iter().map(|l| 14.0 + l.1.size().x + 20.0 + l.2.size().x).fold(time.size().x, f32::max);
    let size = vec2(inner + 22.0, 10.0 + time.size().y + 6.0 + lines.len() as f32 * 20.0 + 6.0);
    let left = if x > plot.min.x + plot.width() * 0.6 { x - 12.0 - size.x } else { x + 12.0 };
    let b = Rect::from_min_size(pos2(left, plot.min.y + 2.0), size);
    let screen = ui.ctx().screen_rect();
    let b = b.translate(vec2((screen.max.x - 4.0 - b.max.x).min(0.0), (screen.max.y - 4.0 - b.max.y).min(0.0)));
    lp.add(egui::epaint::Shadow { offset: vec2(0.0, 6.0), blur: 18.0, spread: 0.0, color: Color32::from_black_alpha(if pal.dark { 110 } else { 40 }) }.as_shape(b, Rounding::same(9.0)));
    lp.rect(b, Rounding::same(9.0), pal.input, Stroke::new(1.0_f32, pal.border_strong));
    let mut y = b.min.y + 9.0;
    let th = time.size().y;
    lp.galley(pos2(b.min.x + 11.0, y), time, pal.ink_soft);
    y += th + 6.0;
    for (col, name, value) in lines {
        let cy = y + 10.0;
        lp.circle_filled(pos2(b.min.x + 15.0, cy), 3.5, col);
        lp.galley(pos2(b.min.x + 25.0, cy - name.size().y / 2.0), name, pal.ink_soft);
        lp.galley(pos2(b.max.x - 11.0 - value.size().x, cy - value.size().y / 2.0), value, pal.ink);
        y += 20.0;
    }
}
