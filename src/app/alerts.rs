//! Alerts: what a pool's servers say needs looking at (their messages: a VM that crashed, a limit
//! passed, a certificate running out…) together with what this app finds itself (a server off,
//! storage not mounted). The toolbar's Alerts button (a bell, with how many need attention) opens
//! their page; a VM's or a server's page shows its own at the top; the Alerts dialog sets the
//! limits a pool watches by itself, and its email for them.

use super::overview::{fit, say};
use super::performance::segmented;
use super::*;
use crate::alerts::*;

/// A pool's messages as last read (all of them now and then, the new ones in between), when,
/// and why the last read failed.
pub(super) struct MessageCache {
    pub list: Vec<XenMessage>,
    pub full_at: Instant,
    pub poll_at: Instant,
    pub error: Option<String>,
    /// Its pool didn't take a "since" (message.get_since): read in full, more often, instead.
    pub whole: bool,
}

/// How often a pool's messages are read in full (which drops those dismissed elsewhere), and how
/// often the new ones in between.
const FULL_EVERY: Duration = Duration::from_secs(600);
const NEW_EVERY: Duration = Duration::from_secs(30);
/// How often in full, for a pool that can't say what's new.
const WHOLE_EVERY: Duration = Duration::from_secs(120);

/// Where an alert comes from.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Source {
    /// A server's message.
    Server,
    /// A limit someone set, passed (the server's ALARM).
    Limit,
    /// Something this app finds itself.
    App,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Server => "From the server",
            Source::Limit => "Your limit",
            Source::App => "Found by this app",
        }
    }
}

/// An alert as shown: how much it matters, in words, what it's about (its tree item), its pool,
/// when, and its message to dismiss (None: it goes once what it's about is sorted out).
#[derive(Clone, Debug)]
pub(super) struct AlertRow {
    pub key: String,
    pub sid: u32,
    pub sev: Severity,
    pub title: String,
    pub detail: String,
    pub body: String,
    pub raw: String,
    pub source: Source,
    pub node: Option<String>,
    pub obj: String,
    pub pool: String,
    pub time: i64,
    pub dismiss: Option<String>,
    /// Its Performance tab shows what passed the limit.
    pub perf: bool,
    /// A server's certificate running out: which server, and whether it's the one for the pool
    /// (Renew makes a new one).
    pub renew: Option<(String, bool)>,
}

/// The Alerts page: everything or only what needs attention, which pool (None: all), what's
/// typed in its search, and the alert opened.
#[derive(Default)]
pub(super) struct AlertsView {
    pub every: bool,
    pub pool: Option<u32>,
    pub search: String,
    pub open: Option<String>,
}

/// The Alerts dialog: a VM's, a server's or a storage's limits, and its pool's email for alerts.
pub(super) struct RulesDialog {
    pub sid: u32,
    /// XenAPI's class ("VM", "host", "SR") and reference, its name, and what kind it is.
    pub cls: &'static str,
    pub obj: String,
    pub name: String,
    pub watched: Watched,
    /// What its server said (None: still asking).
    pub read: Option<Result<AlertSettings, String>>,
    pub rules: Vec<Rule>,
    pub mail_on: bool,
    pub mail_to: String,
    pub mail_via: String,
    pub saving: bool,
    pub error: Option<String>,
    /// A storage whose kind doesn't tell the pool how full it is: the pool can't raise its
    /// "fuller than" alert; this app does, while it's open.
    pub app_watches: bool,
}

/// What the Alerts dialog asks for.
pub(super) enum RulesAct {
    Nothing,
    Close,
    Save,
}

/// "08:42", locally.
fn clock(t: i64) -> String {
    fmt_clock(t, local_offset(t), false)[..5].to_string()
}

/// The day of `t` for people: "Today", "Yesterday", "16 Nov", "16 Nov 2025".
fn day_label(t: i64, now: i64) -> String {
    let off = local_offset(t);
    let (day, today) = ((t + off).div_euclid(86_400), (now + off).div_euclid(86_400));
    if day == today {
        return "Today".into();
    }
    if day == today - 1 {
        return "Yesterday".into();
    }
    let (y, _, _) = civil_from_days(day);
    let (ny, _, _) = civil_from_days(today);
    let d = crate::history::fmt_day(t, off);
    if y == ny {
        d
    } else {
        format!("{} {}", d, y)
    }
}

/// An alert's mark: a red circle with "!", an amber triangle, a green tick, a blue "i".
pub(super) fn sev_mark(p: &egui::Painter, c: egui::Pos2, r: f32, sev: Severity, pal: &Pal) {
    let white = Color32::WHITE;
    match sev {
        Severity::Critical => {
            p.circle_filled(c, r, pal.red);
            p.line_segment([c + vec2(0.0, -r * 0.5), c + vec2(0.0, r * 0.12)], Stroke::new(r * 0.22, white));
            p.circle_filled(c + vec2(0.0, r * 0.48), r * 0.13, white);
        }
        Severity::Warning => {
            let pts = vec![c + vec2(0.0, -r * 1.02), c + vec2(r * 1.08, r * 0.86), c + vec2(-r * 1.08, r * 0.86)];
            p.add(egui::Shape::convex_polygon(pts, pal.orange, Stroke::NONE));
            let ink = Color32::from_rgb(27, 30, 35);
            p.line_segment([c + vec2(0.0, -r * 0.38), c + vec2(0.0, r * 0.22)], Stroke::new(r * 0.2, ink));
            p.circle_filled(c + vec2(0.0, r * 0.55), r * 0.12, ink);
        }
        Severity::Recovered => {
            p.circle_filled(c, r, pal.green);
            p.add(egui::Shape::line(vec![c + vec2(-r * 0.45, r * 0.02), c + vec2(-r * 0.12, r * 0.35), c + vec2(r * 0.48, -r * 0.3)], Stroke::new(r * 0.2, white)));
        }
        Severity::Info => {
            p.circle_filled(c, r, if pal.dark { Color32::from_rgb(75, 127, 196) } else { pal.blue });
            p.circle_filled(c + vec2(0.0, -r * 0.45), r * 0.13, white);
            p.line_segment([c + vec2(0.0, -r * 0.12), c + vec2(0.0, r * 0.55)], Stroke::new(r * 0.2, white));
        }
    }
}

impl App {
    /// Reads each connected pool's messages: all of them at first and every ten minutes, the new
    /// ones every half minute in between.
    pub(super) fn poll_messages(&mut self, ctx: &egui::Context) {
        let due: Vec<(u32, Arc<dyn Connection>, Option<i64>)> = self
            .servers
            .iter()
            .filter(|s| s.data.is_some() && s.state == ConnState::Connected && !self.messages_busy.contains(&s.id))
            .filter_map(|s| match self.messages.get(&s.id) {
                None => Some((s.id, s.conn.clone(), None)),
                Some(c) if c.full_at.elapsed() >= if c.whole { WHOLE_EVERY } else { FULL_EVERY } => Some((s.id, s.conn.clone(), None)),
                Some(c) if c.whole => None,
                // (from the newest it has, by the server's own clock)
                Some(c) if c.poll_at.elapsed() >= NEW_EVERY => Some((s.id, s.conn.clone(), Some(c.list.iter().map(|m| m.time).max().unwrap_or_else(|| unix_now() - 120) - 1))),
                _ => None,
            })
            .collect();
        for (sid, conn, since) in due {
            self.messages_busy.insert(sid);
            let (tx, ctx) = (self.tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let result = conn.messages(since);
                let _ = tx.send(Msg::Messages { sid, since, result });
                ctx.request_repaint();
            });
        }
    }

    /// A reading of a pool's messages: all of them in place of what it had, or new ones added.
    pub(super) fn take_messages(&mut self, sid: u32, since: Option<i64>, result: Result<Vec<XenMessage>, ApiError>) {
        self.messages_busy.remove(&sid);
        let now = Instant::now();
        let c = self.messages.entry(sid).or_insert_with(|| MessageCache {
            list: vec![],
            // (a first reading that fails is tried again in a minute)
            full_at: now.checked_sub(FULL_EVERY - Duration::from_secs(60)).unwrap_or(now),
            poll_at: now,
            error: None,
            whole: false,
        });
        c.poll_at = now;
        match result {
            Ok(list) if since.is_none() => {
                c.list = list;
                c.full_at = now;
                c.error = None;
            }
            Ok(list) => {
                for m in list {
                    match c.list.iter_mut().find(|x| x.rref == m.rref) {
                        Some(x) => *x = m,
                        None => c.list.push(m),
                    }
                }
                c.error = None;
            }
            // (what's new not read: the whole list from now on, quietly)
            Err(_) if since.is_some() => c.whole = true,
            Err(e) => c.error = Some(e.message()),
        }
    }

    /// Keeps since when each problem this app finds has been there (for its alert's time).
    pub(super) fn note_app_alerts(&mut self) {
        let now = unix_now();
        let texts: Vec<String> = self.problems().into_iter().map(|p| p.1).collect();
        self.app_alerts_since.retain(|t, _| texts.contains(t));
        for t in texts {
            self.app_alerts_since.entry(t).or_insert(now);
        }
    }

    /// Every alert, the newest first: each pool's messages (but those being dismissed), and the
    /// problems this app finds.
    pub(super) fn alert_rows(&self) -> Vec<AlertRow> {
        let mut out = vec![];
        for (key, text, detail) in self.problems() {
            let sid = key.split(':').nth(1).and_then(|x| x.parse().ok()).unwrap_or(0);
            let pool = self.server(sid).map(|s| s.name()).unwrap_or_default();
            let time = self.app_alerts_since.get(&text).copied().unwrap_or_else(unix_now);
            out.push(AlertRow { key: format!("app:{}", text), sid, sev: Severity::Warning, title: text, detail, body: String::new(), raw: String::new(), source: Source::App, node: Some(key), obj: String::new(), pool, time, dismiss: None, perf: false, renew: None });
        }
        for s in &self.servers {
            let (Some(d), Some(c)) = (&s.data, self.messages.get(&s.id)) else { continue };
            let pool = if d.name.is_empty() { s.name() } else { d.name.clone() };
            for m in c.list.iter().filter(|m| !self.dismissing.contains(&m.rref)) {
                let found = match m.cls.as_str() {
                    "VM" => d.vms.iter().find(|v| v.uuid == m.obj_uuid).map(|v| (v.name.clone(), format!("vm:{}:{}", s.id, v.rref))),
                    "Host" => d.hosts.iter().find(|h| h.uuid == m.obj_uuid).map(|h| (h.name.clone(), format!("host:{}:{}", s.id, h.rref))),
                    // (a server's own storage with its server's name: they're often all "Local storage")
                    "SR" => d.srs.iter().find(|x| x.uuid == m.obj_uuid).map(|x| {
                        let name = match x.host.as_deref().and_then(|h| d.host(h)) {
                            Some(h) => format!("{} on {}", x.name, h.name),
                            None => x.name.clone(),
                        };
                        (name, format!("sr:{}:{}", s.id, x.rref))
                    }),
                    "Pool" => Some((pool.clone(), format!("srv:{}", s.id))),
                    // (a snapshot schedule: its name, no page of its own)
                    "VMSS" => d.schedules.iter().find(|x| x.uuid == m.obj_uuid).map(|x| (x.name.clone(), String::new())),
                    _ => None,
                };
                let (obj, node) = match found {
                    Some((o, n)) => (o, Some(n).filter(|n| !n.is_empty())),
                    None => (String::new(), None),
                };
                let said = describe(m, &obj);
                let perf = said.limit && node.as_deref().map_or(false, |n| n.starts_with("vm:") || n.starts_with("host:"));
                // a server's own certificate running out (or run out): Renew
                let cert = |p: &str| m.name.starts_with(&format!("{}EXPIR", p));
                let renew = match (m.cls.as_str(), cert("HOST_SERVER_CERTIFICATE_"), cert("HOST_INTERNAL_CERTIFICATE_")) {
                    ("Host", true, _) | ("Host", _, true) => d.hosts.iter().find(|h| h.uuid == m.obj_uuid).map(|h| (h.rref.clone(), cert("HOST_INTERNAL_CERTIFICATE_"))),
                    _ => None,
                };
                out.push(AlertRow {
                    key: format!("msg:{}", m.rref),
                    sid: s.id,
                    sev: Severity::of(m.priority),
                    title: said.title,
                    detail: said.detail,
                    // (a limit's body is its settings, as XML: its detail says them)
                    body: if said.limit { String::new() } else { m.body.trim().to_string() },
                    raw: m.name.clone(),
                    source: if said.limit { Source::Limit } else { Source::Server },
                    node,
                    obj,
                    pool: pool.clone(),
                    time: m.time,
                    dismiss: Some(m.rref.clone()),
                    perf,
                    renew,
                });
            }
        }
        out.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| a.title.cmp(&b.title)));
        out
    }

    /// The alerts of the tree item `key` that need attention and can be dismissed (a page's
    /// own, for its top).
    pub(super) fn alerts_of(&self, key: &str) -> Vec<AlertRow> {
        self.alert_rows().into_iter().filter(|r| r.node.as_deref() == Some(key) && r.source != Source::App && r.sev.needs_attention()).collect()
    }

    /// Dismisses alerts: their messages hidden at once and removed from their pools.
    pub(super) fn dismiss_alerts(&mut self, list: Vec<(u32, String)>, ctx: &egui::Context) {
        let mut by_pool: HashMap<u32, Vec<String>> = HashMap::new();
        for (sid, r) in list {
            if self.dismissing.insert(r.clone()) {
                by_pool.entry(sid).or_default().push(r);
            }
        }
        for (sid, refs) in by_pool {
            let Some(conn) = self.server(sid).map(|s| s.conn.clone()) else { continue };
            let (tx, ctx) = (self.tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let result = conn.dismiss_messages(&refs);
                let _ = tx.send(Msg::Dismissed { sid, refs, result });
                ctx.request_repaint();
            });
        }
    }

    /// Their pool's answer: gone for good, or back with why not.
    pub(super) fn dismissed(&mut self, sid: u32, refs: Vec<String>, result: Result<(), ApiError>) {
        for r in &refs {
            self.dismissing.remove(r);
        }
        match result {
            Ok(()) => {
                if let Some(c) = self.messages.get_mut(&sid) {
                    c.list.retain(|m| !refs.contains(&m.rref));
                }
            }
            Err(e) => {
                let n = refs.len();
                let what = if n == 1 { "the alert".to_string() } else { format!("{} alerts", n) };
                self.dialog = Dialog::Message { title: "Couldn't dismiss".into(), text: format!("The server didn't dismiss {}: {}", what, e.message()) };
            }
        }
    }

    /// The Alerts page: what needs attention (or everything), by day, the newest first; a click
    /// opens one (what the server said, and where to go), its X dismisses it.
    pub(super) fn alerts_page(&mut self, ui: &mut Ui, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let rows = self.alert_rows();
        let pools: Vec<(u32, String)> = self.servers.iter().filter(|s| s.data.is_some()).map(|s| (s.id, s.name())).collect();
        let many = pools.len() > 1;
        let reading = self.servers.iter().any(|s| s.data.is_some() && !self.messages.contains_key(&s.id));
        let failed: Vec<String> = self
            .servers
            .iter()
            .filter_map(|s| self.messages.get(&s.id).and_then(|c| c.error.as_ref()).map(|e| format!("Couldn't read {}'s alerts: {}", s.name(), e)))
            .collect();
        let now = unix_now();
        let v = &mut self.alerts_view;
        if v.pool.map_or(false, |p| !pools.iter().any(|x| x.0 == p)) {
            v.pool = None;
        }
        let pool = v.pool;
        let in_pool = |r: &AlertRow| pool.map_or(true, |p| r.sid == p);
        let attention = rows.iter().filter(|r| in_pool(r) && r.sev.needs_attention()).count();
        let all = rows.iter().filter(|r| in_pool(r)).count();
        egui::ScrollArea::vertical().id_salt("alerts").auto_shrink([false; 2]).show(ui, |ui| {
            let pad = 28.0;
            let w = (ui.available_width() - 2.0 * pad).min(1120.0);
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                ui.add_space(pad);
                ui.vertical(|ui| {
                    ui.set_width(w);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    // which: what needs attention or everything, which pool, words in them
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = vec2(12.0, 8.0);
                        let labels = [format!("Needs attention  {}", attention), format!("Everything  {}", all)];
                        if let Some(i) = segmented(ui, &pal, &labels, if v.every { 1 } else { 0 }) {
                            v.every = i == 1;
                        }
                        if pools.len() > 1 {
                            let name = v.pool.and_then(|p| pools.iter().find(|x| x.0 == p)).map_or("All pools".to_string(), |x| x.1.clone());
                            egui::ComboBox::from_id_salt("alerts-pool").selected_text(name).width(170.0).show_ui(ui, |ui| {
                                ui.selectable_value(&mut v.pool, None, "All pools");
                                for (id, n) in &pools {
                                    ui.selectable_value(&mut v.pool, Some(*id), n.as_str());
                                }
                            });
                        }
                        search_box(ui, &pal, &mut v.search, "Search alerts", 240.0);
                    });
                    ui.add_space(6.0);
                    let q = v.search.trim().to_lowercase();
                    let shown: Vec<&AlertRow> = rows
                        .iter()
                        .filter(|r| in_pool(r) && (v.every || r.sev.needs_attention()))
                        .filter(|r| q.is_empty() || [&r.title, &r.detail, &r.obj, &r.pool, &r.body, &r.raw].iter().any(|t| t.to_lowercase().contains(&q)))
                        .collect();
                    for f in &failed {
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new(f).size(12.5).color(pal.red));
                    }
                    if shown.is_empty() {
                        ui.add_space(14.0);
                        let text = if reading {
                            "Reading the servers' alerts…"
                        } else if !q.is_empty() {
                            "No alerts with those words."
                        } else if v.every {
                            "No alerts."
                        } else {
                            "Nothing needs your attention."
                        };
                        say(ui, &pal, text, reading);
                    }
                    // by day, the newest first
                    let mut days: Vec<(String, Vec<&AlertRow>)> = vec![];
                    for r in shown {
                        let d = day_label(r.time, now);
                        match days.last_mut() {
                            Some(last) if last.0 == d => last.1.push(r),
                            _ => days.push((d, vec![r])),
                        }
                    }
                    for (day, list) in days {
                        ui.add_space(18.0);
                        ui.label(egui::RichText::new(day.to_uppercase()).size(11.0).color(pal.ink_faint));
                        ui.add_space(8.0);
                        egui::Frame::none().fill(pal.surface).stroke(Stroke::new(1.0_f32, pal.border.gamma_multiply(0.7))).rounding(Rounding::same(12.0)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.spacing_mut().item_spacing.y = 0.0;
                            for (i, r) in list.iter().enumerate() {
                                let open = v.open.as_deref() == Some(r.key.as_str());
                                let (row, x) = alert_row(ui, &pal, r, open, i == 0, many);
                                if x {
                                    if let Some(m) = &r.dismiss {
                                        out.push(Intent::DismissAlerts(vec![(r.sid, m.clone())]));
                                    }
                                } else if row {
                                    v.open = if open { None } else { Some(r.key.clone()) };
                                }
                                if open {
                                    opened(ui, &pal, r, out);
                                }
                            }
                        });
                    }
                    ui.add_space(18.0);
                    ui.add(egui::Label::new(egui::RichText::new("Alerts from the servers stay on them: dismissing one removes it there, so it goes from XenCenter too. Those this app finds itself (a server that's off, storage not mounted) go once that's sorted out.").size(12.5).color(pal.ink_soft)).wrap());
                    ui.add_space(24.0);
                });
            });
        });
    }

    /// The tab row's button on the Alerts page: Dismiss all (those shown that can be).
    pub(super) fn alerts_actions(&self, ui: &mut Ui, free: Rect, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let v = &self.alerts_view;
        let q = v.search.trim().to_lowercase();
        let all: Vec<(u32, String)> = self
            .alert_rows()
            .into_iter()
            .filter(|r| v.pool.map_or(true, |p| r.sid == p) && (v.every || r.sev.needs_attention()))
            .filter(|r| q.is_empty() || [&r.title, &r.detail, &r.obj, &r.pool, &r.body, &r.raw].iter().any(|t| t.to_lowercase().contains(&q)))
            .filter_map(|r| r.dismiss.map(|m| (r.sid, m)))
            .collect();
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(free).layout(egui::Layout::right_to_left(egui::Align::Center)));
        child.spacing_mut().item_spacing.x = 4.0;
        let compact = free.width() < flat_button_width(&child, "Dismiss all") + 16.0 + SEP_W;
        let tip = if all.is_empty() { "Nothing here can be dismissed" } else { "Dismiss every alert shown here (on their servers too)" };
        if flat_button(&mut child, "check", "Dismiss all", Some(pal.green), !all.is_empty(), compact, &pal).on_hover_text(tip).on_disabled_hover_text(tip).clicked() {
            out.push(Intent::DismissAlerts(all));
        }
        vsep(&mut child, &pal);
    }

    /// Opens the Alerts dialog for a VM, a server or a storage, and asks its server for what's set.
    pub(super) fn open_alert_rules(&mut self, sid: u32, cls: &'static str, obj: String, name: String, watched: Watched, ctx: &egui::Context) {
        let Some(conn) = self.server(sid).map(|s| s.conn.clone()) else { return };
        let app_watches = watched == Watched::Sr && self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.sr(&obj)).map_or(false, |x| !x.stats);
        self.dialog = Dialog::AlertRules(Box::new(RulesDialog { sid, cls, obj: obj.clone(), name, watched, read: None, rules: rules_of(watched, ""), mail_on: false, mail_to: String::new(), mail_via: String::new(), saving: false, error: None, app_watches }));
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = conn.alert_settings(cls, &obj);
            let _ = tx.send(Msg::AlertSettingsRead { obj, result });
            ctx.request_repaint();
        });
    }

    /// What the server has set, into the dialog (if it's still the one open for it).
    pub(super) fn alert_rules_read(&mut self, obj: String, result: Result<AlertSettings, ApiError>) {
        let Dialog::AlertRules(d) = &mut self.dialog else { return };
        if d.obj != obj || d.read.is_some() {
            return;
        }
        if let Ok(a) = &result {
            d.rules = rules_of(d.watched, &a.perfmon);
            d.mail_on = !a.mail_to.is_empty();
            d.mail_to = a.mail_to.clone();
            d.mail_via = a.mail_via.clone();
        }
        d.read = Some(result.map_err(|e| e.message()));
    }

    /// Saves the dialog's limits (and the pool's email, when changed), logged.
    pub(super) fn save_alert_rules(&mut self, ctx: &egui::Context) {
        let Dialog::AlertRules(d) = &mut self.dialog else { return };
        let Some(Ok(was)) = &d.read else { return };
        if let Some(p) = d.rules.iter().find_map(rule_problem) {
            d.error = Some(p);
            return;
        }
        let mail = if d.mail_on { (d.mail_to.trim().to_string(), d.mail_via.trim().to_string()) } else { (String::new(), String::new()) };
        if d.mail_on && (mail.0.is_empty() || mail.1.is_empty() || !mail.0.contains('@')) {
            d.error = Some("For email, type the address to send to and the mail server (like smtp.example.com:587).".into());
            return;
        }
        let mail = (mail != (was.mail_to.clone(), was.mail_via.clone())).then_some(mail);
        let perfmon = perfmon_with(&was.perfmon, &d.rules);
        d.saving = true;
        d.error = None;
        let (sid, cls, obj, name) = (d.sid, d.cls, d.obj.clone(), d.name.clone());
        let Some((conn, server)) = self.server(sid).map(|s| (s.conn.clone(), s.name())) else { return };
        let log = self.add_log(sid, &server, Some(&obj), &name, &format!("Setting the alert limits of '{}'", name), "In progress");
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = conn.set_alert_settings(cls, &obj, &perfmon, mail);
            let _ = tx.send(Msg::AlertSettingsSaved { obj, log, result });
            ctx.request_repaint();
        });
    }

    /// The server's answer: the dialog closes, or says why not.
    pub(super) fn alert_rules_saved(&mut self, obj: String, log: usize, result: Result<(), ApiError>) {
        match &result {
            Ok(()) => self.end_log(log, "Completed", None),
            Err(e) => self.end_log(log, "Failed", Some(e.message())),
        }
        let Dialog::AlertRules(d) = &mut self.dialog else { return };
        if d.obj != obj {
            return;
        }
        match result {
            Ok(()) => self.dialog = Dialog::None,
            Err(e) => {
                d.saving = false;
                d.error = Some(format!("The server didn't save them: {}", e.message()));
            }
        }
    }
}

/// An alert's row: its mark, what happened (two lines at most), what it's about (and its pool,
/// when there are several) and where it comes from, when, and an X to dismiss it. (Clicked, X
/// clicked.)
fn alert_row(ui: &mut Ui, pal: &Pal, r: &AlertRow, open: bool, first: bool, pools: bool) -> (bool, bool) {
    let w = ui.available_width();
    let p = ui.painter().clone();
    let time = p.layout_no_wrap(clock(r.time), FontId::proportional(12.5), pal.ink_soft);
    let right = 16.0 + time.size().x + 12.0 + 32.0;
    let mut job = egui::text::LayoutJob::simple(r.title.clone(), FontId::proportional(14.0), pal.ink, w - 50.0 - right);
    job.wrap.max_rows = 2;
    job.wrap.overflow_character = Some('…');
    let title = p.layout_job(job);
    let h = 12.0 + title.size().y + 4.0 + 16.0 + 12.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if open || resp.hovered() {
        p.rect_filled(rect.shrink2(vec2(4.0, 2.0)), Rounding::same(8.0), if open { pal.hover.gamma_multiply(0.6) } else { pal.hover });
    }
    if !first {
        p.line_segment([pos2(rect.min.x + 50.0, rect.min.y), pos2(rect.max.x, rect.min.y)], Stroke::new(1.0_f32, pal.border.gamma_multiply(0.6)));
    }
    let top = rect.min.y + 12.0;
    sev_mark(&p, pos2(rect.min.x + 26.0, top + 9.0), 9.0, r.sev, pal);
    let th = title.size().y;
    p.galley(pos2(rect.min.x + 50.0, top), title, pal.ink);
    // what it's about, its pool, where it comes from
    let meta_y = top + th + 4.0;
    let mut x = rect.min.x + 50.0;
    // (what it's about only when its title doesn't say)
    let about = match (r.obj.is_empty() || r.obj == r.pool || r.title.contains(&r.obj), pools) {
        (true, true) => r.pool.clone(),
        (true, false) => String::new(),
        (false, true) => format!("{}  ·  {}", r.obj, r.pool),
        (false, false) => r.obj.clone(),
    };
    if !about.is_empty() {
        let (g, _) = fit(&p, &about, FontId::proportional(12.5), pal.ink_soft, (w - 50.0 - right - 130.0).max(60.0));
        let gw = g.size().x;
        p.galley(pos2(x, meta_y), g, pal.ink_soft);
        x += gw + 10.0;
    }
    let tag = p.layout_no_wrap(r.source.label().to_string(), FontId::proportional(11.0), if r.source == Source::App { pal.link } else { pal.ink_soft });
    let tr = Rect::from_min_size(pos2(x, meta_y - 1.0), tag.size() + vec2(12.0, 3.0));
    p.rect_stroke(tr, Rounding::same(5.0), Stroke::new(1.0_f32, pal.border));
    p.galley(tr.min + vec2(6.0, 1.5), tag, pal.ink_soft);
    // when, and the X
    let tw = time.size().x;
    p.galley(pos2(rect.max.x - 16.0 - 32.0 - 8.0 - tw, top + 1.0), time, pal.ink_soft);
    let mut x_clicked = false;
    if r.dismiss.is_some() {
        let xr = Rect::from_min_size(pos2(rect.max.x - 16.0 - 30.0, top - 5.0), vec2(30.0, 28.0));
        let xresp = ui.interact(xr, ui.id().with(("dismiss", &r.key)), Sense::click()).on_hover_text("Dismiss");
        if xresp.hovered() {
            p.rect_filled(xr, Rounding::same(6.0), pal.border);
        }
        paint_glyph(&p, Rect::from_center_size(xr.center(), vec2(14.0, 14.0)), "close", if xresp.hovered() { pal.ink } else { pal.ink_soft });
        x_clicked = xresp.clicked();
    }
    (resp.clicked() && !x_clicked, x_clicked)
}

/// An opened alert, under its row: more of what it says, the server's own words, and where to
/// go from it.
fn opened(ui: &mut Ui, pal: &Pal, r: &AlertRow, out: &mut Vec<Intent>) {
    ui.horizontal(|ui| {
        ui.add_space(50.0);
        ui.vertical(|ui| {
            ui.set_width(ui.available_width() - 16.0);
            ui.spacing_mut().item_spacing.y = 4.0;
            egui::Frame::none().fill(pal.window).stroke(Stroke::new(1.0_f32, pal.border.gamma_multiply(0.7))).rounding(Rounding::same(8.0)).inner_margin(egui::Margin::symmetric(12.0, 10.0)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let mut said = false;
                for t in [&r.detail, &r.body] {
                    if !t.is_empty() {
                        ui.add(egui::Label::new(egui::RichText::new(t.as_str()).size(13.0).color(pal.ink)).wrap());
                        said = true;
                    }
                }
                if !said {
                    ui.label(egui::RichText::new(if r.source == Source::App { "It goes by itself once that's sorted out." } else { "The server says nothing more." }).size(13.0).color(pal.ink_soft));
                }
                if !r.raw.is_empty() {
                    ui.label(egui::RichText::new(format!("{}  ·  {} ({})", r.raw, r.sev.label(), fmt_date_time(r.time, local_offset(r.time)))).size(11.5).monospace().color(pal.ink_soft));
                }
            });
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(18.0, 6.0);
                let link = |ui: &mut Ui, text: &str| ui.add(egui::Label::new(egui::RichText::new(text).size(13.0).color(pal.link)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
                if let Some((h, internal)) = &r.renew {
                    if link(ui, "Renew it  ›") {
                        out.push(Intent::AskRenewCert(r.sid, h.clone(), *internal, r.dismiss.clone()));
                    }
                }
                if r.perf {
                    if let Some(n) = &r.node {
                        if link(ui, "See it in Performance  ›") {
                            out.push(Intent::Select(n.clone()));
                            out.push(Intent::Tab(Tab::Performance));
                        }
                    }
                }
                if let Some(n) = &r.node {
                    let label = if r.obj.is_empty() { "Go to it  ›".to_string() } else { format!("Go to {}  ›", r.obj) };
                    if link(ui, &label) {
                        out.push(Intent::Select(n.clone()));
                    }
                }
                if let Some(m) = &r.dismiss {
                    if link(ui, "Dismiss") {
                        out.push(Intent::DismissAlerts(vec![(r.sid, m.clone())]));
                    }
                }
            });
            ui.add_space(12.0);
        });
    });
}

/// A page's own alerts at its top: each with its mark, what happened, when, a way to its
/// Performance when it passed a limit, and its X; three at most, then a way to the rest.
pub(super) fn alert_strip(ui: &mut Ui, pal: &Pal, rows: &[AlertRow], out: &mut Vec<Intent>) {
    if rows.is_empty() {
        return;
    }
    let (fill, edge, soft) = if pal.dark {
        (Color32::from_rgb(42, 36, 27), Color32::from_rgb(91, 67, 32), Color32::from_rgb(182, 169, 143))
    } else {
        (Color32::from_rgb(255, 247, 222), Color32::from_rgb(236, 200, 128), Color32::from_rgb(120, 92, 40))
    };
    egui::Frame::none().fill(fill).stroke(Stroke::new(1.0_f32, edge)).rounding(Rounding::same(12.0)).inner_margin(egui::Margin::symmetric(14.0, 4.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        for (i, r) in rows.iter().take(3).enumerate() {
            let w = ui.available_width();
            let p = ui.painter().clone();
            let when = p.layout_no_wrap(fmt_when(r.time, unix_now(), local_offset(r.time)), FontId::proportional(12.5), soft);
            let linked = r.perf || r.renew.is_some();
            let link = if r.renew.is_some() { "Renew  ›" } else if r.perf { "See it  ›" } else { "" };
            let link_g = p.layout_no_wrap(link.to_string(), FontId::proportional(13.0), pal.link);
            let right = when.size().x + 14.0 + if linked { link_g.size().x + 14.0 } else { 0.0 } + 34.0;
            let mut job = egui::text::LayoutJob::default();
            job.append(&r.title, 0.0, egui::TextFormat { font_id: FontId::new(13.5, FontFamily::Name("bold".into())), color: pal.ink, ..Default::default() });
            if !r.detail.is_empty() {
                job.append(&format!(".  {}", r.detail), 0.0, egui::TextFormat { font_id: FontId::proportional(13.5), color: pal.ink_soft, ..Default::default() });
            }
            job.wrap = egui::text::TextWrapping { max_width: w - 30.0 - right, max_rows: 2, break_anywhere: false, overflow_character: Some('…') };
            let g = p.layout_job(job);
            let h = g.size().y.max(20.0) + 16.0;
            let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
            if i > 0 {
                p.line_segment([rect.left_top(), rect.right_top()], Stroke::new(1.0_f32, edge.gamma_multiply(0.6)));
            }
            let cy = rect.center().y;
            sev_mark(&p, pos2(rect.min.x + 9.0, cy), 8.0, r.sev, pal);
            p.galley(pos2(rect.min.x + 28.0, cy - g.size().y / 2.0), g, pal.ink);
            let mut x = rect.max.x - 30.0;
            if let Some(m) = &r.dismiss {
                let xr = Rect::from_center_size(pos2(x + 14.0, cy), vec2(26.0, 26.0));
                let xresp = ui.interact(xr, ui.id().with(("strip-dismiss", &r.key)), Sense::click()).on_hover_text("Dismiss");
                if xresp.hovered() {
                    p.rect_filled(xr, Rounding::same(6.0), edge.gamma_multiply(0.5));
                }
                paint_glyph(&p, Rect::from_center_size(xr.center(), vec2(13.0, 13.0)), "close", soft);
                if xresp.clicked() {
                    out.push(Intent::DismissAlerts(vec![(r.sid, m.clone())]));
                }
            }
            if linked {
                x -= 10.0 + link_g.size().x;
                let lr = Rect::from_min_size(pos2(x, cy - link_g.size().y / 2.0), link_g.size());
                p.galley(lr.min, link_g, pal.link);
                let lresp = ui.interact(lr, ui.id().with(("strip-perf", &r.key)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                if lresp.clicked() {
                    match &r.renew {
                        Some((h, internal)) => out.push(Intent::AskRenewCert(r.sid, h.clone(), *internal, r.dismiss.clone())),
                        None => out.push(Intent::Tab(Tab::Performance)),
                    }
                }
            }
            x -= 14.0 + when.size().x;
            p.galley(pos2(x, cy - when.size().y / 2.0), when, soft);
        }
        // under them: the rest in Alerts, and Dismiss all (every one of this page's, shown or not)
        let all: Vec<(u32, String)> = rows.iter().filter_map(|r| r.dismiss.clone().map(|m| (r.sid, m))).collect();
        if rows.len() > 3 || all.len() > 1 {
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
            let p = ui.painter().clone();
            p.line_segment([rect.left_top(), rect.right_top()], Stroke::new(1.0_f32, edge.gamma_multiply(0.6)));
            let link = |text: String, left: bool, key: &str| -> bool {
                let g = p.layout_no_wrap(text, FontId::proportional(13.0), pal.link);
                let at = if left { pos2(rect.min.x + 28.0, rect.center().y - g.size().y / 2.0) } else { pos2(rect.max.x - 16.0 - g.size().x, rect.center().y - g.size().y / 2.0) };
                let lr = Rect::from_min_size(at, g.size()).expand(4.0);
                p.galley(at, g, pal.link);
                ui.interact(lr, ui.id().with(("strip-footer", key)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
            };
            if rows.len() > 3 && link(format!("{} more in Alerts  ›", rows.len() - 3), true, "more") {
                out.push(Intent::Alerts);
            }
            if all.len() > 1 && link(format!("Dismiss all {}", all.len()), false, "all") {
                out.push(Intent::DismissAlerts(all));
            }
        }
    });
}

/// The Alerts dialog: its limits (each on or off, its figure and minutes), then the pool's email.
pub(super) fn rules_window(ctx: &egui::Context, pal: &Pal, d: &mut RulesDialog) -> RulesAct {
    let mut act = RulesAct::Nothing;
    dialog_window(ctx, &format!("Alerts for {}", d.name)).show(ctx, |ui| {
        ui.set_width(500.0);
        let soft = |ui: &mut Ui, text: &str| {
            ui.add(egui::Label::new(egui::RichText::new(text).size(13.0).color(pal.ink_soft)).wrap());
        };
        soft(ui, "The pool watches these itself, also while this app is closed, and raises each alert at most once an hour. They show in Alerts (and in XenCenter).");
        ui.add_space(12.0);
        let ok = matches!(d.read, Some(Ok(_)));
        match &d.read {
            None => {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.label(egui::RichText::new("Reading what's set…").color(pal.ink_soft));
                });
            }
            Some(Err(e)) => {
                ui.add(egui::Label::new(egui::RichText::new(format!("Couldn't read what's set: {}", e)).color(pal.red)).wrap());
            }
            Some(Ok(_)) => {
                ui.label(egui::RichText::new("ALERT ME WHEN").size(11.0).color(pal.ink_faint));
                ui.add_space(6.0);
                for r in d.rules.iter_mut() {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        crate::app::tick(ui, pal, &mut r.on, "");
                        let ink = if r.on { pal.ink } else { pal.ink_soft };
                        ui.label(egui::RichText::new(r.def.before).color(ink));
                        ui.add_enabled(r.on, egui::TextEdit::singleline(&mut r.figure).desired_width(52.0).horizontal_align(egui::Align::Center));
                        ui.label(egui::RichText::new(r.def.unit).color(ink));
                        ui.label(egui::RichText::new("for").color(ink));
                        ui.add_enabled(r.on, egui::TextEdit::singleline(&mut r.minutes).desired_width(40.0).horizontal_align(egui::Align::Center));
                        ui.label(egui::RichText::new("minutes").color(ink));
                    });
                    ui.add_space(4.0);
                }
                ui.add_space(6.0);
                soft(ui, match d.watched {
                    Watched::Vm => "Its network and disks: everything going in and out of all of them.",
                    Watched::Host => "Its network: all its cards. Its free memory: what's left for more VMs.",
                    Watched::Sr => "How full it is: what its disks take of all it has.",
                });
                if d.app_watches {
                    ui.add_space(8.0);
                    let text = "This kind of storage doesn't tell the pool how full it is, so the pool can't raise this alert itself, nor email it. This app checks it instead while it's open, and shows it in Alerts.";
                    egui::Frame::none().fill(pal.orange.gamma_multiply(if pal.dark { 0.16 } else { 0.10 })).rounding(Rounding::same(8.0)).inner_margin(egui::Margin::symmetric(12.0, 9.0)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(egui::Label::new(egui::RichText::new(text).size(12.5).color(pal.ink)).wrap());
                    });
                }
                ui.add_space(14.0);
                ui.label(egui::RichText::new("EMAIL").size(11.0).color(pal.ink_faint));
                ui.add_space(6.0);
                crate::app::tick(ui, pal, &mut d.mail_on, "Email the pool's alerts too");
                if d.mail_on {
                    ui.add_space(4.0);
                    egui::Grid::new("alert-mail").num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
                        ui.label(egui::RichText::new("To").color(pal.ink_soft));
                        ui.add(egui::TextEdit::singleline(&mut d.mail_to).hint_text("you@example.com").desired_width(300.0));
                        ui.end_row();
                        ui.label(egui::RichText::new("Mail server").color(pal.ink_soft));
                        ui.add(egui::TextEdit::singleline(&mut d.mail_via).hint_text("smtp.example.com:587").desired_width(300.0));
                        ui.end_row();
                    });
                }
                ui.add_space(4.0);
                soft(ui, "For the whole pool: its coordinator sends them.");
            }
        }
        if let Some(e) = &d.error {
            ui.add_space(8.0);
            ui.add(egui::Label::new(egui::RichText::new(e.as_str()).color(pal.red)).wrap());
        }
        ui.add_space(14.0);
        dialog_buttons(ui, |ui| {
            if dialog_button(ui, pal, "close", "Cancel", Look::Plain, !d.saving) || ui.input(|i| i.key_pressed(Key::Escape)) {
                act = RulesAct::Close;
            }
            if dialog_button(ui, pal, "check", if d.saving { "Saving…" } else { "Save" }, Look::Primary, ok && !d.saving) {
                act = RulesAct::Save;
            }
        });
    });
    act
}
