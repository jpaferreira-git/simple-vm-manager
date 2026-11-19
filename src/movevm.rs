//! Moving a VM: to another server, its disks to other storage, or both, in one dialog. Running, it
//! keeps running (moved live, its disks copied as it runs); off, its disks are copied and swapped
//! in, and it starts on the chosen server from then on. The server's choices say why one can't
//! take it; each disk offers the storage that server sees, where it is kept when it can be.

use crate::app::{dialog_button, dialog_buttons, dialog_window, Look};
use crate::model::*;
use crate::newvm::alert;
use crate::theme::Pal;
use eframe::egui::{self, Ui};

/// A server it could go to: whether it's where it is, its free memory, and why it can't (if so).
pub struct MoveHost {
    pub rref: String,
    pub name: String,
    pub now: bool,
    pub free: i64,
    pub why_not: Option<String>,
    /// All its memory, and its CPUs, for its bars.
    pub mem_total: i64,
    pub cpus: i64,
}

/// One of its disks: its name and size, what's written on it (0 when not known), and the storage
/// it's on.
pub struct MoveDisk {
    pub vdi: String,
    /// Its position on the VM ("0" for Disk 0), and its name.
    pub device: String,
    pub name: String,
    pub size: i64,
    pub used: i64,
    pub sr: String,
}

impl MoveDisk {
    /// The room it takes on `x`: on thin storage what's written on it (when that's known), on
    /// LVM its whole size.
    fn need(&self, x: &MoveSr) -> i64 {
        if x.thin && self.used > 0 {
            self.used
        } else {
            self.size
        }
    }
}

/// Storage a disk could go to: its name, its room, the servers that see it, and whether it can
/// send or take a running VM's disk (live storage migration).
pub struct MoveSr {
    pub rref: String,
    pub name: String,
    pub free: i64,
    /// Its size and what's on it now, for its bar.
    pub size: i64,
    pub used: i64,
    pub seen_by: Vec<String>,
    pub live_out: bool,
    pub live_in: bool,
    /// A disk on it takes only what's written on it (on LVM, its whole size).
    pub thin: bool,
}

pub enum MoveAction {
    None,
    Close,
    Move(MoveVm),
}

pub struct MoveDialog {
    pub sid: u32,
    pub vm: String,
    name: String,
    running: bool,
    /// Why it can't move at all now (suspended, busy…).
    stuck: Option<String>,
    /// The memory it needs where it goes.
    mem: i64,
    hosts: Vec<MoveHost>,
    disks: Vec<MoveDisk>,
    srs: Vec<MoveSr>,
    /// Its DVD drive with a disc: the drive, the disc's name, and the servers that see its library.
    dvd: Option<(String, String, Vec<String>)>,
    /// What's chosen: the server, and each disk's storage.
    host: String,
    to: Vec<String>,
    /// Settings of the pool's pointing to storage that's gone (XenServer uses them when moving a
    /// running VM's disks).
    gone: Vec<String>,
    /// Its CPUs; how busy each server's CPUs are now, and its own (0 to 1: read while it's open).
    vcpus: i64,
    load: std::collections::HashMap<String, f64>,
    vm_cpu: Option<f64>,
}

impl MoveDialog {
    /// For VM `v` of pool `d`; `to` the server to start from (dropped on it in the tree).
    pub fn new(sid: u32, d: &PoolData, v: &VmInfo, to: Option<&str>, busy: bool) -> MoveDialog {
        let running = matches!(v.power, PowerState::Running);
        let stuck = match v.power {
            _ if busy => Some("Wait for what's being done to it to finish.".to_string()),
            PowerState::Suspended => Some("It's suspended: resume it or shut it down to move it.".to_string()),
            PowerState::Paused => Some("It's paused: unpause it or shut it down to move it.".to_string()),
            _ if d.host_down_for(v).is_some() => Some("Its server isn't there.".to_string()),
            _ => None,
        };
        let mem = if v.mem_actual > 0 { v.mem_actual } else { v.mem_dyn_max };
        let here = v.home().map(String::from);
        let mut hosts: Vec<MoveHost> = d
            .hosts
            .iter()
            .map(|h| {
                let now = here.as_deref() == Some(h.rref.as_str());
                let why_not = if !h.is_up() {
                    Some("off".to_string())
                } else if !h.enabled {
                    Some("not taking VMs".to_string())
                } else if running && !now && h.mem_free < mem {
                    Some(format!("only {} free", fmt_bytes(h.mem_free)))
                } else {
                    None
                };
                MoveHost { rref: h.rref.clone(), name: h.name.clone(), now, free: h.mem_free, why_not, mem_total: h.mem_total, cpus: h.cpu_count }
            })
            .collect();
        hosts.sort_by_key(|h| (!h.now, h.name.to_lowercase()));
        let disks: Vec<MoveDisk> = v.disks.iter().filter(|k| k.kind == "Disk" && !k.vdi.is_empty()).map(|k| MoveDisk { vdi: k.vdi.clone(), device: k.device.clone(), name: k.name.clone(), size: k.size, used: k.used, sr: k.sr.clone() }).collect();
        let srs: Vec<MoveSr> = d
            .srs
            .iter()
            .filter(|x| !x.is_iso() && x.kind != "udev" && x.size > 0)
            .map(|x| {
                let seen_by = match &x.host {
                    Some(h) => vec![h.clone()],
                    None => x.links.iter().filter(|l| l.1).map(|l| l.0.clone()).collect(),
                };
                MoveSr {
                    rref: x.rref.clone(),
                    name: if x.host.is_some() { format!("{} ({})", x.name, x.host.as_deref().and_then(|h| d.host(h)).map_or("", |h| h.name.as_str())) } else { x.name.clone() },
                    free: (x.size - x.used).max(0),
                    size: x.size,
                    used: x.used,
                    seen_by,
                    live_out: x.live_out,
                    live_in: x.live_in,
                    thin: x.is_thin(),
                }
            })
            .collect();
        let dvd = v.disks.iter().find(|k| k.kind == "DVD drive" && !k.vdi.is_empty()).map(|k| {
            let seen_by = match d.sr(&k.sr) {
                Some(x) if x.host.is_some() => x.host.iter().cloned().collect(),
                Some(x) => x.links.iter().filter(|l| l.1).map(|l| l.0.clone()).collect(),
                None => d.hosts.iter().map(|h| h.rref.clone()).collect(),
            };
            (k.vbd.clone(), k.name.clone(), seen_by)
        });
        let start = to.map(String::from).or(here.clone()).or_else(|| hosts.iter().find(|h| h.why_not.is_none()).map(|h| h.rref.clone())).unwrap_or_default();
        let gone = d.gone_storage().into_iter().map(|g| g.0).collect();
        let mut dialog = MoveDialog { sid, vm: v.rref.clone(), name: v.name.clone(), running, stuck, mem, hosts, disks, srs, dvd, host: String::new(), to: vec![], gone, vcpus: v.vcpus, load: Default::default(), vm_cpu: None };
        dialog.to = dialog.disks.iter().map(|k| k.sr.clone()).collect();
        dialog.pick(&start);
        dialog
    }

    /// How busy the servers' CPUs are now (by server), and its own CPUs (0 to 1), as last read.
    pub fn set_load(&mut self, load: std::collections::HashMap<String, f64>, vm_cpu: Option<f64>) {
        (self.load, self.vm_cpu) = (load, vm_cpu);
    }

    /// What it'd be like where it goes, under one heading: the server's memory and CPUs (when
    /// it changes server) and each storage disks go to, as bars of what's in use there now and
    /// what this VM adds (its memory; its CPU use now, while it runs; what its disks take).
    fn bars(&self, ui: &mut Ui, pal: &Pal) {
        let h = self.hosts.iter().find(|h| h.rref == self.host && !h.now && h.why_not.is_none());
        let going = |j: usize| self.to[j] != self.disks[j].sr;
        let mut targets: Vec<&MoveSr> = vec![];
        for j in (0..self.disks.len()).filter(|&j| going(j)) {
            if let Some(x) = self.sr(&self.to[j]).filter(|x| !targets.iter().any(|t| t.rref == x.rref)) {
                targets.push(x);
            }
        }
        if h.is_none() && targets.is_empty() {
            return;
        }
        ui.add_space(6.0);
        ui.label(egui::RichText::new("AFTER THE MOVE").size(11.0).color(pal.ink_faint));
        let w = ui.available_width() - 4.0;
        if let Some(h) = h {
            if h.mem_total > 0 {
                let what = if self.running { "moves there" } else { "needs there to start" };
                let tip = format!("Blue: memory in use on {} now. Yellow: what '{}' {}. Grey: what's left after.", h.name, self.name, what);
                room_bar(ui, pal, w, "Memory", Some((h.mem_total as f64, (h.mem_total - h.free) as f64, self.mem as f64)), Measure::Bytes).on_hover_text(tip);
            }
            if self.running && h.cpus > 0 {
                // its CPU use as a share of that server's: its CPUs' use times how many, over the server's
                let figures = match (self.load.get(&h.rref), self.vm_cpu) {
                    (Some(&busy), Some(own)) => Some((100.0, busy * 100.0, own * self.vcpus.max(1) as f64 / h.cpus as f64 * 100.0)),
                    _ => None,
                };
                let tip = format!("Blue: how busy {}'s CPUs are now. Yellow: what '{}' uses now, as a share of them. CPU isn't set aside like memory: this is only how busy it would be.", h.name, self.name);
                room_bar(ui, pal, w, "CPU", figures, Measure::Percent).on_hover_text(tip);
            }
        }
        for x in &targets {
            let need: i64 = (0..self.disks.len()).filter(|&j| going(j) && self.to[j] == x.rref).map(|j| self.disks[j].need(x)).sum();
            // named when its disks go to more than one
            let what = if targets.len() == 1 { "Storage".to_string() } else { x.name.clone() };
            let tip = format!("Blue: what's on {} now. Yellow: what its disks take there. Grey: what's left after.", x.name);
            room_bar(ui, pal, w, &what, Some((x.size as f64, x.used as f64, need as f64)), Measure::Bytes).on_hover_text(tip);
        }
    }

    /// The storage the server `h` sees.
    fn seen(&self, h: &str) -> Vec<&MoveSr> {
        let mut out: Vec<&MoveSr> = self.srs.iter().filter(|x| x.seen_by.iter().any(|s| s == h)).collect();
        out.sort_by_key(|x| x.name.to_lowercase());
        out
    }

    /// Server `h` chosen: each disk stays where it is when `h` sees that storage, else goes to the
    /// storage `h` sees with the most room (its own first).
    fn pick(&mut self, h: &str) {
        self.host = h.to_string();
        let running = self.running;
        let kept: Vec<String> = self.seen(h).iter().map(|x| x.rref.clone()).collect();
        let mut to = std::mem::take(&mut self.to);
        {
            let seen: Vec<&MoveSr> = self.seen(h).into_iter().filter(|x| !running || x.live_in).collect();
            for (i, k) in self.disks.iter().enumerate() {
                if seen.iter().any(|x| x.rref == to[i]) || (to[i] == k.sr && kept.contains(&k.sr)) {
                    continue;
                }
                to[i] = if kept.contains(&k.sr) {
                    k.sr.clone()
                } else {
                    let own = |x: &&&MoveSr| (x.seen_by.len() == 1, x.free);
                    seen.iter().filter(|x| x.free >= k.need(x)).max_by_key(own).or(seen.iter().max_by_key(|x| x.free)).map_or(String::new(), |x| x.rref.clone())
                };
            }
        }
        self.to = to;
    }

    fn sr(&self, r: &str) -> Option<&MoveSr> {
        self.srs.iter().find(|x| x.rref == r)
    }

    /// What moving would do now, or why it can't.
    fn plan(&self) -> Result<MoveVm, String> {
        if let Some(why) = &self.stuck {
            return Err(why.clone());
        }
        let h = self.hosts.iter().find(|h| h.rref == self.host).ok_or("Choose a server.")?;
        if let Some(why) = &h.why_not {
            return Err(format!("{} is {}.", h.name, why));
        }
        let mut disks = vec![];
        let mut need: Vec<(String, i64)> = vec![];
        for (i, k) in self.disks.iter().enumerate() {
            let to = &self.to[i];
            let x = self.sr(to).ok_or_else(|| format!("{} sees no storage for {}.", h.name, k.name))?;
            if !x.seen_by.iter().any(|s| *s == self.host) {
                return Err(format!("{} doesn't see {}.", h.name, x.name));
            }
            if *to != k.sr {
                // a running VM's disk moves live: its storage must send it, the new one take it
                if self.running {
                    if let Some(from) = self.sr(&k.sr).filter(|f| !f.live_out) {
                        return Err(format!("{} can't send a disk away while its VM runs: shut it down to move {}.", from.name, k.name));
                    }
                    if !x.live_in {
                        return Err(format!("{} can't take a disk while its VM runs: choose other storage, or shut the VM down first.", x.name));
                    }
                }
                disks.push((k.vdi.clone(), to.clone()));
                match need.iter_mut().find(|n| n.0 == *to) {
                    Some(n) => n.1 += k.need(x),
                    None => need.push((to.clone(), k.need(x))),
                }
            }
        }
        for (r, size) in &need {
            if let Some(x) = self.sr(r).filter(|x| x.free < *size) {
                return Err(format!("{} has {} free, and {} has to go there.", x.name, fmt_bytes(x.free), fmt_bytes(*size)));
            }
        }
        if h.now && disks.is_empty() {
            return Err(if self.running { "It's already there: choose another server or storage.".into() } else { "That's where it is: choose another server or storage.".into() });
        }
        Ok(MoveVm { vm: self.vm.clone(), host: self.host.clone(), disks, eject: self.eject(), running: self.running })
    }

    /// Its DVD drive, to empty first: its disc's library isn't seen where it goes.
    fn eject(&self) -> Option<String> {
        let (vbd, _, seen_by) = self.dvd.as_ref()?;
        (!seen_by.iter().any(|h| *h == self.host)).then(|| vbd.clone())
    }

    pub fn ui(&mut self, ctx: &egui::Context, pal: &Pal) -> MoveAction {
        let mut action = MoveAction::None;
        dialog_window(ctx, &format!("Move '{}'", self.name)).id(egui::Id::new("move-vm-dialog")).show(ctx, |ui| {
            ui.set_width(540.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            if let Some(why) = self.stuck.clone() {
                alert(ui, pal, false, "It can't move now.", &why, None);
            } else {
                self.choices(ui, pal);
            }
            let plan = self.plan();
            ui.add_space(4.0);
            match &plan {
                Ok(m) => {
                    let h = self.hosts.iter().find(|h| h.rref == m.host).map_or("", |h| h.name.as_str());
                    let moved: Vec<&MoveDisk> = m.disks.iter().filter_map(|(vdi, _)| self.disks.iter().find(|k| k.vdi == *vdi)).collect();
                    let size: i64 = moved.iter().map(|k| k.size).sum();
                    // what's written on them, when that's known for each
                    let written: i64 = moved.iter().map(|k| k.used).sum();
                    let sizes = if moved.iter().all(|k| k.used > 0) && written < size { format!("{}, {} written", fmt_bytes(size), fmt_bytes(written)) } else { fmt_bytes(size) };
                    let n = m.disks.len();
                    let disks = if n == 1 { format!("its disk ({})", sizes) } else { format!("its {} disks ({})", n, sizes) };
                    let text = match (m.running, self.hosts.iter().any(|x| x.now && x.rref == m.host), n) {
                        (true, false, 0) => format!("It keeps running: moved live to {}, its memory ({}) copied as it runs.", h, fmt_bytes(self.mem)),
                        (true, false, _) => format!("It keeps running: moved live to {}, {} copied as it runs; that can take a while.", h, disks),
                        (true, true, _) => format!("It keeps running: {} copied to the new storage as it runs; that can take a while.", disks),
                        (false, _, 0) => format!("It starts on {} from now on; its disks stay where they are.", h),
                        (false, _, 1) => format!("{} copied to the new storage, the copy taking its place; it starts on {} from now on.", first_up(&disks), h),
                        (false, _, _) => format!("{} copied to the new storage, the copies taking their place; it starts on {} from now on.", first_up(&disks), h),
                    };
                    ui.label(egui::RichText::new(text).size(12.5).color(pal.ink_soft));
                    if m.running && !m.disks.is_empty() && !self.gone.is_empty() {
                        let what = self.gone.join("; ");
                        alert(ui, pal, false, "This will fail.", &format!("{}: storage that no longer exists, which XenServer uses when moving a running VM's disks. Make another storage the pool's default first (Make Default, on its page).", what), None);
                    }
                    if m.eject.is_some() {
                        let disc = self.dvd.as_ref().map_or("", |d| d.1.as_str());
                        alert(ui, pal, false, "Its DVD drive is emptied first.", &format!("'{}' is in a library {} doesn't see.", disc, h), None);
                    }
                }
                Err(why) if self.stuck.is_none() => {
                    ui.label(egui::RichText::new(why).size(12.5).color(pal.ink_faint));
                }
                Err(_) => {}
            }
            ui.add_space(4.0);
            dialog_buttons(ui, |ui| {
                if dialog_button(ui, pal, "move", "Move", Look::Primary, plan.is_ok()) {
                    if let Ok(m) = plan {
                        action = MoveAction::Move(m);
                    }
                }
                if dialog_button(ui, pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    action = MoveAction::Close;
                }
            });
        });
        action
    }

    /// The servers to choose from, then each disk's storage.
    fn choices(&mut self, ui: &mut Ui, pal: &Pal) {
        use crate::app::{field_label, LABEL_WIDTH};
        use crate::newvm::{combo, combo_style};
        // the server, from a list however many there are
        let host_label = |h: &MoveHost| match (&h.why_not, h.now) {
            (Some(why), _) => format!("{}  ·  {}", h.name, why),
            (None, true) => format!("{}  ·  where it is, {} free", h.name, fmt_bytes(h.free)),
            (None, false) => format!("{}  ·  {} free", h.name, fmt_bytes(h.free)),
        };
        let mut picked = None;
        ui.horizontal(|ui| {
            field_label(ui, if self.running { "Server:" } else { "Starts on:" }, LABEL_WIDTH);
            combo_style(ui, pal);
            let shown = self.hosts.iter().find(|h| h.rref == self.host).map_or("Choose a server".to_string(), host_label);
            combo("move-host", shown, ui.available_width() - 4.0).show_ui(ui, |ui| {
                for h in &self.hosts {
                    let on = h.rref == self.host;
                    if ui.add_enabled(h.why_not.is_none(), egui::SelectableLabel::new(on, host_label(h))).clicked() && !on {
                        picked = Some(h.rref.clone());
                    }
                }
            });
        });
        if let Some(h) = picked {
            self.pick(&h);
        }
        // each disk's storage ("Storage:" for its one disk, else by its position, as on its Overview)
        let seen: Vec<(String, String, i64, bool, bool)> = self.seen(&self.host).iter().map(|x| (x.rref.clone(), x.name.clone(), x.free, !self.running || x.live_in, x.thin)).collect();
        let (running, many) = (self.running, self.disks.len() > 1);
        for i in 0..self.disks.len() {
            let now = self.disks[i].sr.clone();
            let label = |r: &str| -> String {
                match seen.iter().find(|s| s.0 == r) {
                    Some((_, n, _, _, _)) if r == now => format!("{}  ·  where it is", n),
                    Some((_, n, _, false, _)) => format!("{}  ·  can't take it while it runs", n),
                    Some((_, n, free, _, thin)) => format!("{}  ·  {}, {} free", n, if *thin { "thin" } else { "thick" }, fmt_bytes(*free)),
                    None => "Choose storage".to_string(),
                }
            };
            let k = &self.disks[i];
            let field = if many { format!("Disk {}:", k.device) } else { "Storage:".to_string() };
            let about = if k.used > 0 && k.used < k.size { format!("{}: {}, {} written", k.name, fmt_bytes(k.size), fmt_bytes(k.used)) } else { format!("{}: {}", k.name, fmt_bytes(k.size)) };
            let mut to = self.to[i].clone();
            ui.horizontal(|ui| {
                field_label(ui, &field, LABEL_WIDTH);
                combo_style(ui, pal);
                combo(&format!("move-disk-{}", i), label(&to), ui.available_width() - 4.0).show_ui(ui, |ui| {
                    for (r, _, _, ok, _) in &seen {
                        let resp = ui.add_enabled(*ok || *r == now, egui::SelectableLabel::new(to == *r, label(r)));
                        let resp = if running { resp.on_disabled_hover_text("Its kind of storage doesn't support moving a running VM's disk to it (live storage migration): shut the VM down to move the disk there") } else { resp };
                        if resp.clicked() {
                            to = r.clone();
                        }
                    }
                })
                .response
                .on_hover_text(about);
            });
            self.to[i] = to;
        }
        self.bars(ui, pal);
    }
}

/// What a bar measures: bytes (storage, memory), or a share of a server's CPUs in percent.
#[derive(Clone, Copy, PartialEq)]
pub enum Measure {
    Bytes,
    Percent,
}

impl Measure {
    fn text(self, v: f64) -> String {
        match self {
            Measure::Bytes => fmt_bytes(v.round() as i64),
            Measure::Percent if v.abs() < 9.95 => format!("{:.1}%", v),
            Measure::Percent => format!("{:.0}%", v),
        }
    }
    /// The words for its parts: in use, what's left, and how much too much.
    fn words(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Measure::Bytes => ("used", "left", "short"),
            Measure::Percent => ("busy", "idle", "over"),
        }
    }
}

/// Room as a thin bar under a line saying what it's of and its figures: what's in use now (a
/// blue dot, the bar's blue), what's being added (yellow; red when it doesn't fit) and what's
/// left (grey). `figures` are the total, what's in use and what's added; None while not known.
pub fn room_bar(ui: &mut Ui, pal: &Pal, width: f32, what: &str, figures: Option<(f64, f64, f64)>, m: Measure) -> egui::Response {
    use egui::{pos2, vec2, Align2, Color32, FontId, Rect, Rounding, Sense, Stroke};
    let (r, resp) = ui.allocate_exact_size(vec2(width, 30.0), Sense::hover());
    let p = ui.painter().clone();
    let line = r.top() + 8.0;
    p.text(pos2(r.left(), line), Align2::LEFT_CENTER, what, FontId::proportional(12.5), pal.ink);
    let bar = Rect::from_min_max(pos2(r.left(), r.bottom() - 8.0), r.right_bottom());
    let round = Rounding::same(4.0);
    p.rect_filled(bar, round, pal.border);
    let font = FontId::proportional(12.0);
    let Some((total, used, need)) = figures else {
        p.text(pos2(r.right(), line), Align2::RIGHT_CENTER, "Reading…", font, pal.ink_faint);
        return resp;
    };
    let total = total.max(1e-9);
    let used = used.clamp(0.0, total);
    let need = need.max(0.0);
    let free = total - used;
    let short = need > free;
    let yellow = if pal.dark { Color32::from_rgb(232, 188, 58) } else { Color32::from_rgb(236, 180, 24) };
    let going = if short { pal.red } else { yellow };
    // the bar: blue, then yellow (at least a sliver of each that's there), then grey
    let at = |b: f64| bar.left() + bar.width() * (b / total) as f32;
    let xu = if used > 0.0 { at(used).max(bar.left() + 3.0) } else { bar.left() };
    let xn = if need > 0.0 { at(used + need.min(free)).max(xu + 3.0).min(bar.right()) } else { xu };
    let part = |a: f32, b: f32| {
        let (l, rt) = (if a <= bar.left() + 0.5 { 4.0 } else { 0.0 }, if b >= bar.right() - 0.5 { 4.0 } else { 0.0 });
        Rounding { nw: l, sw: l, ne: rt, se: rt }
    };
    if xu > bar.left() {
        p.rect_filled(Rect::from_x_y_ranges(bar.left()..=xu, bar.y_range()), part(bar.left(), xu), pal.accent);
    }
    if xn > xu {
        p.rect_filled(Rect::from_x_y_ranges(xu..=xn, bar.y_range()), part(xu, xn), going);
    }
    // the figures, right-aligned, each after a dot of its part's colour (an open one for what's left)
    let (used_word, left_word, short_word) = m.words();
    let mut items: Vec<(String, Option<Color32>, Color32)> = vec![(format!("{} {}", m.text(used), used_word), Some(pal.accent), pal.ink_soft)];
    if need > 0.0 {
        items.push((format!("+{}", m.text(need)), Some(going), pal.ink_soft));
    }
    items.push(if short { (format!("{} {}", m.text(need - free), short_word), None, pal.red) } else { (format!("{} {}", m.text(free - need), left_word), None, pal.ink_soft) });
    let mut x = r.right();
    for (text, dot, ink) in items.into_iter().rev() {
        let g = p.layout_no_wrap(text, font.clone(), ink);
        x -= g.size().x;
        p.galley(pos2(x, line - g.size().y / 2.0), g, ink);
        let c = pos2(x - 8.0, line);
        match dot {
            Some(col) => p.circle_filled(c, 3.5, col),
            None => p.circle_stroke(c, 3.0, Stroke::new(1.0_f32, if short { pal.red } else { pal.ink_faint })),
        };
        x -= 22.0;
    }
    resp
}

/// `text` with its first letter in capitals.
fn first_up(text: &str) -> String {
    let mut c = text.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::{Connection, DemoConnection};

    #[test]
    fn where_it_goes() {
        let d = DemoConnection::new().fetch(0).unwrap();
        let vm = |r: &str| d.vm(r).unwrap();
        let disk = |r: &str| vm(r).disks.iter().find(|k| k.kind == "Disk").unwrap().vdi.clone();
        // running, its disk on shared storage both servers see: live, the disk stays
        let m = MoveDialog::new(1, &d, vm("vm-web"), Some("host-b"), false).plan().unwrap();
        assert_eq!((m.host.as_str(), m.disks.len(), m.running), ("host-b", 0, true));
        // running on its server's own storage: its disk goes to the other server's own
        let m = MoveDialog::new(1, &d, vm("vm-mail"), Some("host-a"), false).plan().unwrap();
        assert_eq!(m.disks, vec![(disk("vm-mail"), "sr-local1".to_string())]);
        // off: the same, and it starts there from then on
        let m = MoveDialog::new(1, &d, vm("vm-desk"), Some("host-b"), false).plan().unwrap();
        assert_eq!((m.disks.clone(), m.running), (vec![(disk("vm-desk"), "sr-local2".to_string())], false));
        // storage that can't take a running VM's disk (no VDI_MIRROR_IN): not chosen for it, the
        // shared one instead; but fine for one that's off
        let mut d2 = d.clone();
        d2.srs.iter_mut().filter(|x| x.rref == "sr-local1").for_each(|x| x.live_in = false);
        let m = MoveDialog::new(1, &d2, d2.vm("vm-mail").unwrap(), Some("host-a"), false).plan().unwrap();
        assert_eq!(m.disks, vec![(disk("vm-mail"), "sr-nfs".to_string())]);
        let mut chosen = MoveDialog::new(1, &d2, d2.vm("vm-mail").unwrap(), Some("host-a"), false);
        chosen.to[0] = "sr-local1".into();
        assert!(chosen.plan().unwrap_err().contains("can't take a disk while its VM runs"));
        d2.srs.iter_mut().filter(|x| x.rref == "sr-local2").for_each(|x| x.live_in = false);
        let m = MoveDialog::new(1, &d2, d2.vm("vm-desk").unwrap(), Some("host-b"), false).plan().unwrap();
        assert_eq!(m.disks, vec![(disk("vm-desk"), "sr-local2".to_string())]);
        // a 60 GB disk with 14 GB written: thin storage with 56 GB free takes it, LVM doesn't
        const GB: i64 = 1 << 30;
        let mut d3 = d.clone();
        let desk = d3.vms.iter_mut().find(|v| v.rref == "vm-desk").unwrap();
        desk.disks.iter_mut().filter(|k| k.kind == "Disk").for_each(|k| (k.size, k.used) = (60 * GB, 14 * GB));
        let x = d3.srs.iter_mut().find(|x| x.rref == "sr-local2").unwrap();
        (x.kind, x.used) = ("ext".into(), x.size - 56 * GB);
        assert_eq!(MoveDialog::new(1, &d3, d3.vm("vm-desk").unwrap(), Some("host-b"), false).plan().unwrap().disks, vec![(disk("vm-desk"), "sr-local2".to_string())]);
        d3.srs.iter_mut().filter(|x| x.rref == "sr-local2").for_each(|x| x.kind = "lvm".into());
        let mut lvm = MoveDialog::new(1, &d3, d3.vm("vm-desk").unwrap(), Some("host-b"), false);
        lvm.to[0] = "sr-local2".into();
        assert_eq!(lvm.plan().unwrap_err(), "Local storage (demo-host-02) has 56 GB free, and 60 GB has to go there.");
        // nowhere new, or can't now
        assert!(MoveDialog::new(1, &d, vm("vm-web"), None, false).plan().is_err());
        assert!(MoveDialog::new(1, &d, vm("vm-build"), Some("host-a"), false).plan().is_err()); // suspended
        assert!(MoveDialog::new(1, &d, vm("vm-web"), Some("host-b"), true).plan().is_err()); // busy
    }
}
