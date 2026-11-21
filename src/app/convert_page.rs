//! The VM Conversion page (the toolbar's Convert): VMs from VMware ESXi or vCenter, Proxmox, and
//! disk files (.ova, .vmx, .vmdk…), made into VMs of a pool. Its VMs tab lists a source's VMs:
//! tick some, then Convert opens a dialog in two steps (where they go, what each network becomes,
//! each one's template; then each one's preparation, and what happens before and after). Its
//! Conversions tab follows each conversion, step by step.

use super::performance::segmented;
use super::*;
use crate::convert::run::{Job, Mark, NetPlan, NetTo, Plan, VmPlan};
use crate::convert::{self, Family, HelperVm, Kind, Power, Source, SourceConf, SourceVm};
use crate::newvm::{combo, combo_style};

/// How a source stands.
#[derive(Clone, PartialEq)]
pub(crate) enum SrcState {
    /// Its password isn't known (not saved, or the keychain is locked).
    Locked,
    Connecting,
    Ready,
    Failed(String),
}

pub(crate) struct SourceEntry {
    pub conf: SourceConf,
    pub source: Option<Arc<dyn Source>>,
    pub files: Option<Arc<convert::files::Files>>,
    pub state: SrcState,
    pub about: String,
    pub vms: Vec<SourceVm>,
}

impl SourceEntry {
    fn key(&self) -> String {
        self.conf.key()
    }
    /// It can run things in its VMs (through their guest tools).
    fn runs_in_vms(&self) -> bool {
        matches!(self.conf.kind, Kind::Vmware | Kind::Proxmox | Kind::Demo)
    }
    fn kind_word(&self) -> &'static str {
        match self.conf.kind {
            Kind::Demo if self.conf.address.starts_with("pve") => "Proxmox",
            Kind::Demo => "VMware",
            k => k.name(),
        }
    }
}

#[derive(Default)]
pub(crate) struct ConvView {
    pub sources: Vec<SourceEntry>,
    pub current: usize,
    pub filter: usize,
    pub search: String,
    /// The VMs ticked, by id, at the source shown.
    pub picked: HashSet<String>,
    /// VMs marked prepared: source key, VM id, when.
    pub prepared: Vec<(String, String, i64)>,
    /// Preparations under way (None) or failed (why), by source key and VM id.
    pub preparing: HashMap<(String, String), Option<String>>,
    /// Their warnings, once prepared.
    pub prep_notes: HashMap<(String, String), Vec<String>>,
    pub jobs: Vec<Arc<Job>>,
    /// Each job's pool and its plan (with just its VM), for Try Again.
    pub plans: HashMap<u64, (u32, Plan)>,
    pub dialog: Option<ConvDialog>,
    pub next_job: u64,
    pub loaded: bool,
    /// Each pool's Helper VM, as last read (None inside: it has none).
    pub helper_vm: HashMap<u32, Option<HelperVm>>,
    /// A screenshot's state to set up once the demo's VMware is listed (see `--show`).
    pub shot: Option<String>,
}

pub(crate) enum ConvMsg {
    Listed { key: String, result: Result<(String, Vec<SourceVm>), String> },
    Unlocked { key: String, password: Option<String> },
    FilesAdded { results: Vec<Result<usize, String>> },
    /// Files chosen with Add Files….
    FilesPicked(Vec<PathBuf>),
    Prepared { key: String, vm: String, result: Result<String, String> },
    JobDone { id: u64 },
    HelperVm { sid: u32, result: Option<HelperVm> },
}

pub(crate) enum ConvDialog {
    Source(SourceForm),
    Convert(Box<Wizard>),
    Message(String, String),
}

/// Add a Source (or its password again).
pub(crate) struct SourceForm {
    kind: usize,
    address: String,
    user: String,
    password: String,
    remember: bool,
    error: String,
    busy: bool,
    /// An existing source whose password is asked for again.
    again: Option<String>,
}

impl SourceForm {
    fn new() -> SourceForm {
        SourceForm { kind: 0, address: String::new(), user: "root".into(), password: String::new(), remember: true, error: String::new(), busy: false, again: None }
    }
    fn kind(&self) -> Kind {
        if self.kind == 1 {
            Kind::Proxmox
        } else {
            Kind::Vmware
        }
    }
}

/// A VM in the Convert dialog.
pub(crate) struct WizVm {
    vm: SourceVm,
    name: String,
    template: String,
    /// The account in it for VMware's guest operations.
    user: String,
    password: String,
}

/// What a source's network becomes.
#[derive(Clone, PartialEq)]
enum NetChoice {
    Existing(String),
    /// A new network: its name, the card (device) and VLAN it's on.
    New(String, String, i64),
    /// None: the VMs' cards on it are left out (one can be added later, in a VM's Overview).
    Without,
}

struct WizNet {
    source: String,
    vlan: Option<i64>,
    users: Vec<String>,
    choice: NetChoice,
}

pub(crate) struct Wizard {
    key: String,
    step: u8,
    sid: u32,
    vms: Vec<WizVm>,
    /// The server: None for any.
    host: Option<String>,
    sr: String,
    nets: Vec<WizNet>,
    keep_macs: bool,
    shutdown: bool,
    start: bool,
    /// A command just copied (VM id), for its "Mark Prepared".
    copied: HashSet<String>,
}

/// What a pool offers the Convert dialog.
struct PoolView {
    name: String,
    hosts: Vec<(String, String, String, i64)>,
    /// rref, name, its server (local storage), free, thin
    srs: Vec<(String, String, Option<String>, i64, bool, i64, i64)>,
    nets: Vec<(String, String, String, i64)>,
    templates: Vec<(String, String)>,
    /// The card the pool is managed through ("eth0"), for a new VLAN.
    card: String,
    default_sr: Option<String>,
}

impl PoolView {
    fn of(d: &PoolData) -> PoolView {
        let hosts = d.hosts.iter().filter(|h| h.enabled && h.is_up()).map(|h| (h.rref.clone(), h.name.clone(), h.address.clone(), h.mem_free)).collect();
        let srs = d
            .srs
            .iter()
            .filter(|x| !x.is_iso() && x.kind != "udev" && x.size > 0 && x.links.iter().any(|l| l.1))
            .map(|x| (x.rref.clone(), x.name.clone(), x.host.clone(), x.size - x.used, x.is_thin(), x.used, x.size))
            .collect();
        let nets = d.networks.iter().filter(|n| n.bridge != "xenapi" && !n.name.contains("Host internal management")).map(|n| (n.rref.clone(), n.name.clone(), n.device.clone(), n.vlan)).collect();
        let mut templates: Vec<(String, String)> = d.templates.iter().map(|t| (t.rref.clone(), t.name.clone())).collect();
        templates.sort_by_key(|t| t.1.to_lowercase());
        let card = d.hosts.iter().find(|h| h.is_master).and_then(|h| h.nics.iter().find(|n| n.management)).map(|n| n.device.clone()).filter(|c| !c.is_empty()).unwrap_or_else(|| "eth0".into());
        PoolView { name: d.name.clone(), hosts, srs, nets, templates, card, default_sr: d.default_sr.clone() }
    }
}

fn sources_file() -> Option<PathBuf> {
    convert::proxmox::config_file("convert-sources.txt")
}
fn prepared_file() -> Option<PathBuf> {
    convert::proxmox::config_file("convert-prepared.txt")
}

/// "4 CPUs · 8 GB".
fn size_words(v: &SourceVm) -> String {
    format!("{} CPU{} · {}", v.cpus, if v.cpus == 1 { "" } else { "s" }, fmt_bytes(v.memory as i64))
}

fn disks_words(v: &SourceVm) -> String {
    let size: u64 = v.size();
    let known = v.disks.iter().all(|d| d.used > 0);
    if known {
        format!("{} · {} ({} written)", v.disks.len(), fmt_bytes(size as i64), fmt_bytes(v.written() as i64))
    } else {
        format!("{} · {}", v.disks.len(), fmt_bytes(size as i64))
    }
}

fn boot_words(v: &SourceVm) -> String {
    let mut s = if v.uefi { "UEFI".to_string() } else { "BIOS".to_string() };
    if v.secure_boot {
        s.push_str(" · Secure Boot");
    }
    if v.vtpm {
        s.push_str(" · vTPM");
    }
    s
}

fn power_words(p: Power) -> &'static str {
    match p {
        Power::On => "Running",
        Power::Off => "Off",
        Power::Paused => "Suspended",
        Power::Unknown => "—",
    }
}

/// A short text, cut with "…" to fit `w`.
fn fit(p: &egui::Painter, text: &str, font: FontId, col: Color32, w: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, col);
    job.wrap = egui::text::TextWrapping { max_width: w.max(20.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    p.layout_job(job)
}

impl App {
    /// The page opened: the remembered sources (each connects once its password comes from the
    /// keychain), the files one, and in the demo its pretend servers.
    pub(super) fn convert_open(&mut self, ctx: &egui::Context) {
        if self.conv.loaded {
            return;
        }
        self.conv.loaded = true;
        self.conv.prepared = prepared_file().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| convert::parse_prepared(&t)).unwrap_or_default();
        let demo = self.servers.iter().any(|s| s.conn.is_demo());
        if demo {
            for (address, src) in [("esxi-01.lan", convert::demo::DemoSource::vmware()), ("pve.lan", convert::demo::DemoSource::proxmox())] {
                let source: Arc<dyn Source> = Arc::new(src);
                self.conv.sources.push(SourceEntry { conf: SourceConf { kind: Kind::Demo, address: address.into(), user: "root".into() }, source: Some(source), files: None, state: SrcState::Connecting, about: String::new(), vms: vec![] });
                self.convert_list(self.conv.sources.len() - 1, ctx);
            }
        }
        // (a screenshot shows only the demo's)
        let saved = if self.shot_path.is_some() { vec![] } else { sources_file().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| convert::parse_sources(&t)).unwrap_or_default() };
        for conf in saved {
            let key = conf.key();
            let (host, user) = (conf.keychain_host(), conf.user.clone());
            self.conv.sources.push(SourceEntry { conf, source: None, files: None, state: SrcState::Connecting, about: String::new(), vms: vec![] });
            let (tx, ctx2) = (self.tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let password = crate::secrets::load(&host, &user);
                let _ = tx.send(Msg::Convert(ConvMsg::Unlocked { key, password }));
                ctx2.request_repaint();
            });
        }
        let files = Arc::new(convert::files::Files::default());
        self.conv.sources.push(SourceEntry { conf: SourceConf { kind: Kind::Files, address: "Disk files".into(), user: String::new() }, source: Some(files.clone()), files: Some(files), state: SrcState::Ready, about: "On this computer".into(), vms: vec![] });
    }

    /// Lists a source's VMs again, in the background.
    fn convert_list(&mut self, i: usize, ctx: &egui::Context) {
        let Some(e) = self.conv.sources.get_mut(i) else { return };
        let Some(src) = e.source.clone() else { return };
        if e.conf.kind != Kind::Files {
            e.state = SrcState::Connecting;
        }
        let key = e.key();
        let (tx, ctx2) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = src.list().map(|vms| (src.describe(), vms));
            let _ = tx.send(Msg::Convert(ConvMsg::Listed { key, result }));
            ctx2.request_repaint();
        });
    }

    fn save_sources(&self) {
        let list: Vec<SourceConf> = self.conv.sources.iter().map(|e| e.conf.clone()).collect();
        if let Some(p) = sources_file() {
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let _ = std::fs::write(p, convert::sources_text(&list));
        }
    }

    fn save_prepared(&self) {
        if let Some(p) = prepared_file() {
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let _ = std::fs::write(p, convert::prepared_text(&self.conv.prepared));
        }
    }

    fn mark_prepared(&mut self, key: &str, vm: &str) {
        self.conv.prepared.retain(|(k, v, _)| !(k == key && v == vm));
        self.conv.prepared.push((key.to_string(), vm.to_string(), unix_now()));
        self.save_prepared();
    }

    fn is_prepared(&self, key: &str, vm: &str) -> bool {
        self.conv.prepared.iter().any(|(k, v, _)| k == key && v == vm)
    }

    /// Files added (picked or dropped): read in the background.
    pub(super) fn convert_files(&mut self, paths: Vec<PathBuf>, ctx: &egui::Context) {
        self.convert_open(ctx);
        let Some(i) = self.conv.sources.iter().position(|e| e.conf.kind == Kind::Files) else { return };
        self.conv.current = i;
        self.conv.picked.clear();
        let Some(files) = self.conv.sources[i].files.clone() else { return };
        let (tx, ctx2) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let results = paths.iter().filter(|p| convert::files::is_convertible(p)).map(|p| files.add(p).map(|v| v.len()).map_err(|e| e.to_string())).collect();
            let _ = tx.send(Msg::Convert(ConvMsg::FilesAdded { results }));
            ctx2.request_repaint();
        });
    }

    pub(super) fn on_convert_msg(&mut self, m: ConvMsg, ctx: &egui::Context) {
        match m {
            ConvMsg::Listed { key, result } => {
                let Some(e) = self.conv.sources.iter_mut().find(|e| e.key() == key) else { return };
                match result {
                    Ok((about, vms)) => {
                        e.about = about;
                        e.vms = vms;
                        e.state = SrcState::Ready;
                        if key == "demo:esxi-01.lan" {
                            if let Some(shot) = self.conv.shot.take() {
                                self.convert_shot(&shot, ctx);
                                return;
                            }
                        }
                        if let Some(ConvDialog::Source(f)) = &self.conv.dialog {
                            if f.busy {
                                self.conv.dialog = None;
                            }
                        }
                    }
                    Err(why) => {
                        e.state = SrcState::Failed(why.clone());
                        if let Some(ConvDialog::Source(f)) = &mut self.conv.dialog {
                            if f.busy {
                                f.busy = false;
                                f.error = why;
                            }
                        }
                    }
                }
            }
            ConvMsg::Unlocked { key, password } => {
                let Some(i) = self.conv.sources.iter().position(|e| e.key() == key) else { return };
                match password {
                    Some(p) => {
                        let e = &mut self.conv.sources[i];
                        e.source = Some(make_source(&e.conf, &p));
                        self.convert_list(i, ctx);
                    }
                    None => self.conv.sources[i].state = SrcState::Locked,
                }
            }
            ConvMsg::FilesAdded { results } => {
                let errors: Vec<String> = results.iter().filter_map(|r| r.as_ref().err().cloned()).collect();
                if let Some(i) = self.conv.sources.iter().position(|e| e.conf.kind == Kind::Files) {
                    self.convert_list(i, ctx);
                }
                if !errors.is_empty() {
                    self.conv.dialog = Some(ConvDialog::Message("Add Files".into(), errors.join("\n\n")));
                }
            }
            ConvMsg::FilesPicked(paths) => self.convert_files(paths, ctx),
            ConvMsg::Prepared { key, vm, result } => {
                let k = (key.clone(), vm.clone());
                match result.and_then(|printed| convert::prep::outcome(&printed)) {
                    Ok(notes) => {
                        self.conv.preparing.remove(&k);
                        self.mark_prepared(&key, &vm);
                        self.conv.prep_notes.insert(k, notes);
                    }
                    Err(why) => {
                        self.conv.preparing.insert(k, Some(why));
                    }
                }
            }
            ConvMsg::JobDone { id } => {
                let Some(job) = self.conv.jobs.iter().find(|j| j.id == id).cloned() else { return };
                let st = job.snapshot();
                let took = st.started.zip(st.ended).map(|(a, b)| fmt_took(b.duration_since(a))).unwrap_or_default();
                match &st.outcome {
                    Some(Ok(_)) => {
                        let started = st.steps.iter().any(|s| s.0 == "Start" && s.1 == Mark::Done);
                        let failed_start = st.steps.iter().any(|s| s.0 == "Start" && s.1 == Mark::Failed);
                        let text = format!("Converted '{}' from {} (in {}){}", job.name, job.from, took, if started { ", and started it" } else if failed_start { ", but it didn't start" } else { "" });
                        self.end_log(job.log, if failed_start { "Failed" } else { "Done" }, Some(text));
                    }
                    Some(Err(why)) => {
                        let status = if job.cancel.load(std::sync::atomic::Ordering::Relaxed) { "Cancelled" } else { "Failed" };
                        self.end_log(job.log, status, Some(format!("Couldn't convert '{}' from {}: {}", job.name, job.from, why)));
                    }
                    None => {}
                }
                if let Some((sid, _)) = self.conv.plans.get(&id) {
                    self.refresh_soon(*sid);
                }
                // its source's VMs again (one shut down is off now)
                if let Some(i) = self.conv.sources.iter().position(|e| e.conf.address == job.from && e.conf.kind != Kind::Files) {
                    self.convert_list(i, ctx);
                }
            }
            ConvMsg::HelperVm { sid, result } => {
                self.conv.helper_vm.insert(sid, result);
            }
        }
    }

    /// The header's action next to the page's tabs: Add a Source, or Clear Finished.
    pub(super) fn convert_actions(&mut self, ui: &mut Ui, free: Rect, conversions: bool) {
        let pal = self.pal;
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(free).layout(egui::Layout::right_to_left(egui::Align::Center)));
        child.spacing_mut().item_spacing.x = 4.0;
        if conversions {
            let finished = self.conv.jobs.iter().any(|j| j.snapshot().outcome.is_some());
            let compact = free.width() < flat_button_width(&child, "Clear Finished") + 16.0 + SEP_W;
            if flat_button(&mut child, "trash", "Clear Finished", None, finished, compact, &pal).on_hover_text("Take the conversions that ended off this list (their lines under Activities stay)").clicked() {
                self.conv.jobs.retain(|j| j.snapshot().outcome.is_none());
            }
        } else {
            let compact = free.width() < flat_button_width(&child, "Add a Source") + 16.0 + SEP_W;
            if flat_button(&mut child, "plus", "Add a Source", Some(pal.accent), true, compact, &pal).on_hover_text("A VMware ESXi or vCenter, or a Proxmox server, whose VMs you want to bring over").clicked() {
                self.conv.dialog = Some(ConvDialog::Source(SourceForm::new()));
            }
        }
        vsep(&mut child, &pal);
    }

    /// The VMs tab: the sources as cards, then the shown source's VMs to tick.
    pub(super) fn convert_page(&mut self, ui: &mut Ui, _out: &mut Vec<Intent>) {
        let pal = self.pal;
        let ctx = ui.ctx().clone();
        self.convert_open(&ctx);
        let has_pool = self.servers.iter().any(|s| s.data.is_some());
        ui.add_space(10.0);
        // the pool's own VMs converted already: "Converted from 'name' on source"
        let converted: Vec<String> = self.servers.iter().filter_map(|s| s.data.as_ref()).flat_map(|d| d.vms.iter().map(|v| v.description.clone())).filter(|d| d.starts_with("Converted from '")).collect();
        let mut act_list: Option<usize> = None;
        let mut act_forget: Option<usize> = None;
        let mut act_unlock: Option<usize> = None;
        let mut act_pick_files = false;
        let mut act_convert = false;
        egui::ScrollArea::vertical().id_salt("convert-page").auto_shrink([false; 2]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.label(egui::RichText::new("FROM").size(11.0).color(pal.ink_faint));
            // the sources, as cards in rows
            let avail = ui.available_width();
            let per_row = ((avail + 10.0) / 250.0).floor().max(1.0) as usize;
            let w = ((avail - 10.0 * (per_row as f32 - 1.0)) / per_row as f32).floor();
            let n = self.conv.sources.len();
            for row in 0..n.div_ceil(per_row) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    for i in row * per_row..((row + 1) * per_row).min(n) {
                        let e = &self.conv.sources[i];
                        let on = i == self.conv.current;
                        let (r, resp) = ui.allocate_exact_size(vec2(w, 58.0), Sense::click());
                        let p = ui.painter();
                        let stroke = if on { Stroke::new(1.5_f32, pal.accent) } else { Stroke::new(1.0_f32, if resp.hovered() { pal.border_strong } else { pal.border }) };
                        p.rect(r, Rounding::same(9.0), if on { pal.surface } else { pal.sidebar }, stroke);
                        let (dot, line2) = match &e.state {
                            SrcState::Ready if e.conf.kind == Kind::Files => (None, format!("{} · {} VM{}", ".ova .vmx .vmdk .vhdx .qcow2 .vdi", e.vms.len(), if e.vms.len() == 1 { "" } else { "s" })),
                            SrcState::Ready => (Some(pal.green), format!("{} · {} VM{} · {}", e.about, e.vms.len(), if e.vms.len() == 1 { "" } else { "s" }, e.conf.user)),
                            SrcState::Connecting => (Some(pal.accent), "Connecting…".to_string()),
                            SrcState::Locked => (Some(pal.orange), "Its password is needed: click to give it".to_string()),
                            SrcState::Failed(why) => (Some(pal.red), why.clone()),
                        };
                        let mut x = r.min.x + 12.0;
                        match dot {
                            Some(c) => {
                                p.circle_filled(pos2(x + 4.0, r.min.y + 19.0), 4.0, c);
                                x += 14.0;
                            }
                            None => {
                                paint_glyph(p, Rect::from_center_size(pos2(x + 7.0, r.min.y + 19.0), vec2(16.0, 16.0)), "folder", pal.ink_soft);
                                x += 20.0;
                            }
                        }
                        let g = fit(p, &e.conf.address, FontId::new(14.0, FontFamily::Name("bold".into())), pal.ink, r.max.x - x - 10.0);
                        p.galley(pos2(x, r.min.y + 19.0 - g.size().y / 2.0), g, pal.ink);
                        let col = if matches!(e.state, SrcState::Failed(_)) { pal.red } else { pal.ink_soft };
                        let g = fit(p, &line2, FontId::proportional(12.0), col, r.width() - 24.0);
                        p.galley(pos2(r.min.x + 12.0, r.min.y + 40.0 - g.size().y / 2.0), g, col);
                        let tip = match &e.state {
                            SrcState::Failed(why) => why.clone(),
                            _ if e.conf.kind == Kind::Files => "Disk files on this computer: Add Files…, or drop them on the window".into(),
                            _ => format!("{} {}", e.kind_word(), e.conf.address),
                        };
                        let resp = resp.on_hover_text(tip);
                        if resp.clicked() {
                            if self.conv.current != i {
                                self.conv.picked.clear();
                            }
                            self.conv.current = i;
                            if e.state == SrcState::Locked {
                                act_unlock = Some(i);
                            }
                        }
                        resp.context_menu(|ui| {
                            if e.conf.kind != Kind::Files && ui.button("Look again").clicked() {
                                act_list = Some(i);
                                ui.close_menu();
                            }
                            if matches!(e.conf.kind, Kind::Vmware | Kind::Proxmox) && ui.button("Forget this source").clicked() {
                                act_forget = Some(i);
                                ui.close_menu();
                            }
                        });
                    }
                });
            }
            ui.add_space(6.0);
            let Some(e) = self.conv.sources.get(self.conv.current) else { return };
            let key = e.key();
            let files = e.conf.kind == Kind::Files;
            // which VMs: all, off, running; and words in their names
            let lines: Vec<&SourceVm> = e.vms.iter().collect();
            let brought = |v: &SourceVm| converted.iter().any(|d| d.starts_with(&format!("Converted from '{}' on {} ", v.name, e.conf.address)));
            let kinds = [
                ("All", lines.len()),
                ("Off", lines.iter().filter(|v| v.power == Power::Off).count()),
                ("Running", lines.iter().filter(|v| v.power == Power::On).count()),
                ("Brought over", lines.iter().filter(|v| brought(v)).count()),
            ];
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(10.0, 8.0);
                let labels: Vec<String> = kinds.iter().map(|(k, n)| format!("{}  {}", k, n)).collect();
                if let Some(i) = segmented(ui, &pal, &labels, self.conv.filter) {
                    self.conv.filter = i;
                }
                search_box(ui, &pal, &mut self.conv.search, "Search VMs", 180.0);
            });
            let q = self.conv.search.trim().to_lowercase();
            let filter = self.conv.filter;
            let shown: Vec<&SourceVm> = lines
                .iter()
                .copied()
                .filter(|v| match filter {
                    1 => v.power == Power::Off,
                    2 => v.power == Power::On,
                    3 => brought(v),
                    _ => true,
                })
                .filter(|v| q.is_empty() || v.name.to_lowercase().contains(&q) || v.os.to_lowercase().contains(&q))
                .collect();
            // what's ticked, and the buttons for it
            let picked: Vec<&SourceVm> = lines.iter().copied().filter(|v| self.conv.picked.contains(&v.id) && v.blocked.is_none()).collect();
            ui.horizontal_wrapped(|ui| {
                ui.set_min_height(30.0);
                ui.spacing_mut().item_spacing.x = 6.0;
                if picked.is_empty() {
                    let text = if e.vms.is_empty() {
                        match (&e.state, files) {
                            (_, true) => "No files yet: Add Files…, or drop .ova, .vmx, .vmdk, .vhdx, .qcow2 or .vdi files on the window.".to_string(),
                            (SrcState::Ready, _) => "It has no VMs.".to_string(),
                            _ => String::new(),
                        }
                    } else {
                        format!("{} VM{}  ·  tick those to bring over; the originals stay where they are.", shown.len(), if shown.len() == 1 { "" } else { "s" })
                    };
                    ui.label(egui::RichText::new(text).color(pal.ink_soft));
                } else {
                    let disks: usize = picked.iter().map(|v| v.disks.len()).sum();
                    let size: u64 = picked.iter().map(|v| v.size()).sum();
                    let written: u64 = picked.iter().map(|v| v.written()).sum();
                    ui.label(egui::RichText::new(format!("{} ticked", picked.len())).color(pal.ink));
                    ui.label(egui::RichText::new(format!("·  {} disk{}, {} ({} written)", disks, if disks == 1 { "" } else { "s" }, fmt_bytes(size as i64), fmt_bytes(written as i64))).color(pal.ink_soft));
                }
            });
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let label = match picked.len() {
                    0 => "Convert…".to_string(),
                    1 => "Convert 1 VM…".to_string(),
                    n => format!("Convert {} VMs…", n),
                };
                let tip = if !has_pool { "Connect to a server first: VMs are converted into one of its pools" } else if picked.is_empty() { "Tick the VMs to bring over first" } else { "Choose where they go, then convert them" };
                if flat_button(ui, "convert", &label, Some(pal.accent), has_pool && !picked.is_empty(), false, &pal).on_hover_text(tip).on_disabled_hover_text(tip).clicked() {
                    act_convert = true;
                }
                if files {
                    if flat_button(ui, "plus", "Add Files…", None, true, false, &pal).on_hover_text("An .ova, .ovf, .vmx, or a disk image (.vmdk, .vhd, .vhdx, .qcow2, .vdi, .img)").clicked() {
                        act_pick_files = true;
                    }
                } else if flat_button(ui, "rescan", "Look Again", None, e.source.is_some() && e.state != SrcState::Connecting, false, &pal).on_hover_text("List its VMs again").clicked() {
                    act_list = Some(self.conv.current);
                }
                if !picked.is_empty() && flat_button(ui, "close", "Clear", None, true, false, &pal).on_hover_text("Untick them all").clicked() {
                    self.conv.picked.clear();
                }
            });
            if shown.is_empty() {
                return;
            }
            // the table; columns go in a narrow window (Boot, System, then Disks)
            let avail = ui.available_width();
            let (with_system, with_boot, with_disks, with_there) = (avail >= 760.0, avail >= 940.0, avail >= 600.0, avail >= 640.0);
            let mut headers = vec!["", "VM"];
            if with_system {
                headers.push("System");
            }
            headers.push("Size");
            if with_disks {
                headers.push("Disks");
            }
            if with_boot {
                headers.push("Boot");
            }
            if with_there {
                headers.push("There");
            }
            headers.push("Ready?");
            let ready = |v: &SourceVm| -> (String, Color32) {
                let k = (key.clone(), v.id.clone());
                if let Some(b) = &v.blocked {
                    return (b.clone(), pal.red);
                }
                if brought(v) {
                    return ("Brought over".into(), pal.ink_faint);
                }
                if self.is_prepared(&key, &v.id) {
                    return ("Prepared".into(), pal.green);
                }
                match self.conv.preparing.get(&k) {
                    Some(None) => return ("Preparing…".into(), pal.accent),
                    Some(Some(why)) => return (format!("Preparing failed: {}", why), pal.red),
                    None => {}
                }
                match (v.family, files) {
                    (Family::Other, _) => ("—".into(), pal.ink_faint),
                    (_, true) => ("Not known (prepared before export?)".into(), pal.ink_soft),
                    _ => ("Prepare it first".into(), pal.orange),
                }
            };
            let measure = |t: &str| ui.painter().layout_no_wrap(t.to_string(), FontId::proportional(13.0), pal.ink).size().x;
            let rows: Vec<Vec<Cell>> = shown
                .iter()
                .map(|v| {
                    let mut r = vec![Cell::Custom(22.0), Cell::Text(v.name.clone())];
                    if with_system {
                        r.push(Cell::Text(convert::os_name(&v.os)));
                    }
                    r.push(Cell::by_num(v.memory as f64, Cell::Text(size_words(v))));
                    if with_disks {
                        r.push(Cell::by_num(v.size() as f64, Cell::Text(disks_words(v))));
                    }
                    if with_boot {
                        r.push(Cell::Text(boot_words(v)));
                    }
                    if with_there {
                        let there = if files { "File" } else { power_words(v.power) };
                        r.push(Cell::by_text(there, Cell::Custom(measure(there) + 14.0)));
                    }
                    let (text, _) = ready(v);
                    // (cut short, whole under the mouse: a long reason mustn't push the table wider)
                    r.push(Cell::by_text(&text, Cell::Custom(measure(&text).min((avail * 0.2).clamp(110.0, 300.0)))));
                    r
                })
                .collect();
            let ticks = self.conv.picked.clone();
            let col_ready = headers.len() - 1;
            let col_there = if with_there { headers.len() - 2 } else { usize::MAX };
            let mut toggled: Option<String> = None;
            let readies: Vec<(String, Color32)> = shown.iter().map(|v| ready(v)).collect();
            let opened = self.table_with(ui, "convert-vms", &headers, &rows, true, &mut |ui, row, col, r, hot| {
                let v = shown[row];
                let p = ui.painter().clone();
                let font = FontId::proportional(13.0);
                if col == 0 {
                    let b = Rect::from_center_size(pos2(r.min.x + 9.0, r.center().y), vec2(16.0, 16.0));
                    if v.blocked.is_some() {
                        p.rect_stroke(b, Rounding::same(4.0), Stroke::new(1.2_f32, pal.border));
                        return;
                    }
                    let on = ticks.contains(&v.id);
                    let resp = ui.interact(b.expand(4.0), ui.id().with(("conv-tick", &v.id)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                    if on {
                        p.rect_filled(b, Rounding::same(4.0), pal.accent);
                        paint_glyph(&p, b.shrink(1.0), "check", pal.on_accent);
                    } else {
                        p.rect_stroke(b, Rounding::same(4.0), Stroke::new(1.2_f32, if resp.hovered() || hot { pal.ink_soft } else { pal.border_strong }));
                    }
                    if resp.clicked() {
                        toggled = Some(v.id.clone());
                    }
                    return;
                }
                if col == col_there {
                    let (dot, text) = match (files, v.power) {
                        (true, _) => (pal.ink_faint, "File"),
                        (_, Power::On) => (pal.green, "Running"),
                        (_, Power::Paused) => (pal.blue, "Suspended"),
                        (_, Power::Off) => (pal.ink_faint, "Off"),
                        _ => (pal.ink_faint, "—"),
                    };
                    p.circle_filled(pos2(r.min.x + 4.0, r.center().y), 3.5, dot);
                    let g = fit(&p, text, font, pal.ink, r.width() - 14.0);
                    p.galley(pos2(r.min.x + 13.0, r.center().y - g.size().y / 2.0), g, pal.ink);
                    return;
                }
                if col == col_ready {
                    let (text, colr) = &readies[row];
                    let g = fit(&p, text, font, *colr, r.width());
                    let resp = ui.interact(r, ui.id().with(("conv-ready", &v.id)), Sense::hover());
                    p.galley(pos2(r.min.x, r.center().y - g.size().y / 2.0), g, *colr);
                    if g_elided(text, r.width(), &p) {
                        resp.on_hover_text(text);
                    }
                }
            });
            if let Some(id) = toggled.or_else(|| opened.map(|i| shown[i].id.clone()).filter(|id| shown.iter().any(|v| &v.id == id && v.blocked.is_none()))) {
                if !self.conv.picked.remove(&id) {
                    self.conv.picked.insert(id);
                }
            }
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Drop an .ova, .vmx, .vmdk, .vhdx, .qcow2 or .vdi file on the window to convert it too. Right-click a source to look again or forget it.").size(12.0).color(pal.ink_faint));
        });
        if let Some(i) = act_list {
            self.convert_list(i, &ctx);
        }
        if let Some(i) = act_unlock {
            let c = self.conv.sources[i].conf.clone();
            let mut f = SourceForm::new();
            f.kind = if c.kind == Kind::Proxmox { 1 } else { 0 };
            f.address = c.address.clone();
            f.user = c.user.clone();
            f.again = Some(c.key());
            self.conv.dialog = Some(ConvDialog::Source(f));
        }
        if let Some(i) = act_forget {
            let c = self.conv.sources.remove(i).conf;
            let (host, user) = (c.keychain_host(), c.user.clone());
            std::thread::spawn(move || crate::secrets::forget(&host, &user));
            self.conv.current = 0;
            self.conv.picked.clear();
            self.save_sources();
        }
        if act_pick_files {
            let (tx, ctx2) = (self.tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let picked = pollster::block_on(rfd::AsyncFileDialog::new().set_title("Files to convert").add_filter("VMs and disks", convert::files::EXTENSIONS).add_filter("Any file", &["*"]).pick_files());
                if let Some(files) = picked {
                    let paths: Vec<PathBuf> = files.iter().map(|f| f.path().to_path_buf()).collect();
                    let _ = tx.send(Msg::Convert(ConvMsg::FilesPicked(paths)));
                    ctx2.request_repaint();
                }
            });
        }
        if act_convert {
            self.open_wizard(&ctx);
        }
    }

    /// A screenshot's state, once the demo's VMware is listed: two of its VMs ticked; the Convert
    /// dialog open (at its second step); or their conversion started.
    fn convert_shot(&mut self, shot: &str, ctx: &egui::Context) {
        self.conv.current = 0;
        self.conv.picked = ["vm-12", "vm-13"].iter().map(|s| s.to_string()).collect();
        if shot == "convert" {
            return;
        }
        self.open_wizard(ctx);
        // (the demo's VMware Tools ran it in nextcloud already)
        self.mark_prepared_quietly("demo:esxi-01.lan", "vm-13");
        if let Some(ConvDialog::Convert(w)) = &mut self.conv.dialog {
            if shot != "convert-wizard" {
                w.step = 2;
            }
        }
        if shot == "conversions" {
            if let Some(ConvDialog::Convert(w)) = self.conv.dialog.take() {
                let Some(src) = self.conv.sources.iter().find(|e| e.key() == w.key).and_then(|e| e.source.clone()) else { return };
                let Some(pool) = self.server(w.sid).and_then(|s| s.data.as_ref()).map(PoolView::of) else { return };
                self.start_conversion(&w, src, "esxi-01.lan", "VMware", false, &pool, ctx);
            }
        }
    }

    /// Marks it prepared for a screenshot (not kept in the file).
    fn mark_prepared_quietly(&mut self, key: &str, vm: &str) {
        self.conv.prepared.push((key.to_string(), vm.to_string(), unix_now()));
    }

    /// The Convert dialog, for the VMs ticked.
    fn open_wizard(&mut self, ctx: &egui::Context) {
        let Some(e) = self.conv.sources.get(self.conv.current) else { return };
        let vms: Vec<SourceVm> = e.vms.iter().filter(|v| self.conv.picked.contains(&v.id) && v.blocked.is_none()).cloned().collect();
        if vms.is_empty() {
            return;
        }
        let key = e.key();
        let pools: Vec<u32> = self.servers.iter().filter(|s| s.data.is_some()).map(|s| s.id).collect();
        let sid = self.selected_pool().filter(|s| pools.contains(s)).or(pools.first().copied());
        let Some(sid) = sid else { return };
        let mut w = Wizard { key, step: 1, sid, vms: vec![], host: None, sr: String::new(), nets: vec![], keep_macs: true, shutdown: true, start: true, copied: HashSet::new() };
        self.wizard_for_pool(&mut w, vms);
        // whether the pool has a Helper VM that copies disks
        if let Some(s) = self.server(sid) {
            let (conn, tx, ctx2) = (s.conn.clone(), self.tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let result = conn.helper_vm().ok().flatten().and_then(|t| HelperVm::parse(&t, unix_now()));
                let _ = tx.send(Msg::Convert(ConvMsg::HelperVm { sid, result }));
                ctx2.request_repaint();
            });
        }
        self.conv.dialog = Some(ConvDialog::Convert(Box::new(w)));
    }

    /// The pool selected in the tree, if any.
    fn selected_pool(&self) -> Option<u32> {
        self.selected.split(':').nth(1).and_then(|s| s.parse().ok())
    }

    /// Fills the dialog's choices for its pool: where, each network, each VM's template.
    fn wizard_for_pool(&self, w: &mut Wizard, vms: Vec<SourceVm>) {
        let Some(d) = self.server(w.sid).and_then(|s| s.data.as_ref()) else { return };
        let pv = PoolView::of(d);
        // storage: the pool's default, else the one with the most room
        w.sr = pv.default_sr.clone().filter(|r| pv.srs.iter().any(|x| x.0 == *r)).or_else(|| pv.srs.iter().max_by_key(|x| x.3).map(|x| x.0.clone())).unwrap_or_default();
        w.host = pv.srs.iter().find(|x| x.0 == w.sr).and_then(|x| x.2.clone());
        // each source network: the pool's of that name, else of that VLAN, else a new VLAN, else
        // the management card's
        let mut nets: Vec<WizNet> = vec![];
        for v in &vms {
            for n in &v.nics {
                match nets.iter_mut().find(|x| x.source == n.network) {
                    Some(x) => {
                        if !x.users.contains(&v.name) {
                            x.users.push(v.name.clone());
                        }
                    }
                    None => nets.push(WizNet { source: n.network.clone(), vlan: n.vlan, users: vec![v.name.clone()], choice: NetChoice::Existing(String::new()) }),
                }
            }
        }
        let main = pv.nets.iter().find(|x| x.2 == pv.card && x.3 < 0).or(pv.nets.first()).map(|x| x.0.clone()).unwrap_or_default();
        for x in nets.iter_mut() {
            let by_name = pv.nets.iter().find(|n| n.1.eq_ignore_ascii_case(&x.source));
            let by_vlan = x.vlan.and_then(|v| pv.nets.iter().find(|n| n.3 == v));
            x.choice = match (by_name, by_vlan, x.vlan) {
                (Some(n), _, _) => NetChoice::Existing(n.0.clone()),
                (_, Some(n), _) => NetChoice::Existing(n.0.clone()),
                (_, _, Some(v)) => NetChoice::New(x.source.clone(), pv.card.clone(), v),
                _ => NetChoice::Existing(main.clone()),
            };
        }
        w.nets = nets;
        let taken: Vec<String> = d.vms.iter().map(|v| v.name.clone()).collect();
        let windows = |v: &SourceVm| v.family == Family::Windows;
        w.vms = vms
            .into_iter()
            .map(|v| {
                let template = convert::pick_template(&v, &d.templates).or_else(|| d.templates.iter().find(|t| t.name == OTHER_MEDIA)).map(|t| t.rref.clone()).unwrap_or_default();
                let user = if windows(&v) { "Administrator" } else { "root" }.to_string();
                WizVm { name: unique_name(&v.name, &taken), template, user, password: String::new(), vm: v }
            })
            .collect();
        w.shutdown = w.vms.iter().any(|v| v.vm.power == Power::On);
    }

    /// The page's dialogs (Add a Source, Convert), over the window when no other dialog is open.
    pub(super) fn convert_dialogs(&mut self, ctx: &egui::Context) {
        if !matches!(self.dialog, Dialog::None) {
            return;
        }
        let Some(mut d) = self.conv.dialog.take() else { return };
        let keep = match &mut d {
            ConvDialog::Message(title, text) => {
                let pal = self.pal;
                let mut open = true;
                dialog_window(ctx, title).show(ctx, |ui| {
                    ui.set_max_width(460.0);
                    ui.add(egui::Label::new(egui::RichText::new(text.as_str()).color(pal.ink)).wrap());
                    ui.add_space(12.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "check", "OK", Look::Primary, true) || ui.input(|i| i.key_pressed(Key::Escape) || i.key_pressed(Key::Enter)) {
                            open = false;
                        }
                    });
                });
                open
            }
            ConvDialog::Source(f) => self.source_form(ctx, f),
            ConvDialog::Convert(w) => self.wizard_ui(ctx, w),
        };
        // (a dialog may have been replaced meanwhile: a message after a failure)
        if keep && self.conv.dialog.is_none() {
            self.conv.dialog = Some(d);
        }
    }

    /// Add a Source: its kind, address, user and password; connects and lists its VMs.
    fn source_form(&mut self, ctx: &egui::Context, f: &mut SourceForm) -> bool {
        let pal = self.pal;
        let mut keep = true;
        let mut connect = false;
        let title = if f.again.is_some() { "Its Password" } else { "Add a Source" };
        dialog_window(ctx, title).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.add(egui::Label::new(egui::RichText::new("Where the VMs you want to bring over are. The app only reads from it: it lists the VMs and copies their disks. It changes nothing there, except shutting down the ones you convert, if you ask it to.").color(pal.ink_soft)).wrap());
            ui.add_space(10.0);
            let lw = 90.0;
            ui.add_enabled_ui(!f.busy && f.again.is_none(), |ui| {
                ui.horizontal(|ui| {
                    field_label(ui, "Kind:", lw);
                    let mut k = f.kind;
                    if crate::isolib::segmented(ui, &pal, &["VMware ESXi or vCenter", "Proxmox"], &mut k, ui.available_width()) {
                        f.kind = k;
                        f.user = if k == 1 { "root@pam".into() } else { "root".into() };
                    }
                });
                ui.horizontal(|ui| {
                    field_label(ui, "Address:", lw);
                    text_field(ui, &pal, &mut f.address, ui.available_width(), false);
                });
                ui.horizontal(|ui| {
                    field_label(ui, "User:", lw);
                    text_field(ui, &pal, &mut f.user, ui.available_width(), false);
                });
            });
            ui.horizontal(|ui| {
                field_label(ui, "Password:", lw);
                let r = text_field(ui, &pal, &mut f.password, ui.available_width(), true);
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    connect = true;
                }
            });
            ui.horizontal(|ui| {
                ui.add_space(lw + ui.spacing().item_spacing.x);
                tick(ui, &pal, &mut f.remember, "Remember password");
            });
            ui.add_space(4.0);
            let note = if f.kind == 1 {
                "Proxmox's API lists the VMs, but can't hand their disks out: those are read over SSH (port 22), as root with the same password. A cluster shows the VMs of all its nodes."
            } else {
                "A vCenter shows the VMs of all its ESXi servers. The password is kept in your system's keychain."
            };
            ui.horizontal(|ui| {
                ui.add_space(lw + ui.spacing().item_spacing.x);
                ui.add(egui::Label::new(egui::RichText::new(note).size(12.0).color(pal.ink_faint)).wrap());
            });
            if !f.error.is_empty() {
                ui.add_space(6.0);
                ui.add(egui::Label::new(egui::RichText::new(f.error.as_str()).color(pal.red)).wrap());
            }
            ui.add_space(12.0);
            dialog_buttons(ui, |ui| {
                if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                    keep = false;
                }
                let ok = !f.busy && !f.address.trim().is_empty() && !f.password.is_empty();
                if dialog_button(ui, &pal, "connect", if f.busy { "Connecting…" } else { "Connect" }, Look::Primary, ok) {
                    connect = true;
                }
            });
        });
        if connect && !f.busy && !f.address.trim().is_empty() && !f.password.is_empty() {
            let conf = SourceConf { kind: f.kind(), address: f.address.trim().to_string(), user: f.user.trim().to_string() };
            let key = conf.key();
            let source = make_source(&conf, &f.password);
            match self.conv.sources.iter().position(|e| e.key() == key) {
                Some(i) => {
                    self.conv.sources[i].source = Some(source);
                    self.conv.sources[i].conf = conf.clone();
                    self.conv.current = i;
                }
                None => {
                    // (before the files card, which stays last)
                    let at = self.conv.sources.iter().position(|e| e.conf.kind == Kind::Files).unwrap_or(self.conv.sources.len());
                    self.conv.sources.insert(at, SourceEntry { conf: conf.clone(), source: Some(source), files: None, state: SrcState::Connecting, about: String::new(), vms: vec![] });
                    self.conv.current = at;
                }
            }
            self.conv.picked.clear();
            self.save_sources();
            if f.remember {
                let (host, user, pw) = (conf.keychain_host(), conf.user.clone(), f.password.clone());
                std::thread::spawn(move || {
                    let _ = crate::secrets::save(&host, &user, &pw);
                });
            }
            f.busy = true;
            f.error.clear();
            let i = self.conv.current;
            self.convert_list(i, ctx);
        }
        keep
    }

    /// The Convert dialog: step 1 (where, networks, each VM), step 2 (preparation, then go).
    fn wizard_ui(&mut self, ctx: &egui::Context, w: &mut Wizard) -> bool {
        let pal = self.pal;
        let Some(pool) = self.server(w.sid).and_then(|s| s.data.as_ref()).map(PoolView::of) else { return false };
        let pools: Vec<(u32, String)> = self.servers.iter().filter(|s| s.data.is_some()).map(|s| (s.id, s.name())).collect();
        let Some(src) = self.conv.sources.iter().find(|e| e.key() == w.key) else { return false };
        let (from, kind, runs, files, source) = (src.conf.address.clone(), src.kind_word(), src.runs_in_vms(), src.conf.kind == Kind::Files, src.source.clone());
        let helper = self.conv.helper_vm.get(&w.sid).cloned().flatten().filter(|h| h.copies && h.fresh);
        let mut keep = true;
        let mut go = false;
        let mut run_prep: Option<usize> = None;
        let mut mark: Option<String> = None;
        let mut new_pool: Option<u32> = None;
        let title = format!("Convert {} from {}", if w.vms.len() == 1 { "1 VM".to_string() } else { format!("{} VMs", w.vms.len()) }, from);
        let max_h = ctx.screen_rect().height() - 120.0;
        dialog_window(ctx, &title).show(ctx, |ui| {
            ui.set_width(720.0f32.min(ctx.screen_rect().width() - 60.0));
            combo_style(ui, &pal);
            // its two steps
            ui.horizontal(|ui| {
                for (n, label) in [(1u8, "Where and how"), (2, "Prepare and go")] {
                    let on = w.step == n;
                    let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
                    if w.step > n {
                        ui.painter().circle_filled(r.center(), 11.0, pal.hover);
                        paint_glyph(ui.painter(), r.shrink(3.0), "check", pal.link);
                    } else {
                        ui.painter().circle(r.center(), 10.5, if on { pal.accent } else { pal.window }, Stroke::new(1.5_f32, if on { pal.accent } else { pal.border_strong }));
                        ui.painter().text(r.center(), Align2::CENTER_CENTER, n.to_string(), FontId::new(12.0, FontFamily::Name("bold".into())), if on { pal.on_accent } else { pal.ink_soft });
                    }
                    ui.label(egui::RichText::new(label).color(if on { pal.ink } else { pal.ink_soft }).strong());
                    if n == 1 {
                        let (r, _) = ui.allocate_exact_size(vec2(36.0, 22.0), Sense::hover());
                        ui.painter().line_segment([r.left_center(), r.right_center()], Stroke::new(1.0_f32, pal.border_strong));
                    }
                }
            });
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(max_h - 140.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                if w.step == 1 {
                    self.wizard_where(ui, w, &pool, &pools, &mut new_pool);
                } else {
                    wizard_prepare(ui, &pal, w, &self.conv, runs, files, kind, &helper, &mut run_prep, &mut mark);
                }
            });
            ui.add_space(10.0);
            dialog_buttons(ui, |ui| {
                if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                    keep = false;
                }
                if w.step == 1 {
                    let problem = wizard_problem(w, &pool);
                    if dialog_button(ui, &pal, "start", "Next", Look::Primary, problem.is_none()) {
                        w.step = 2;
                    }
                    if let Some(p) = problem {
                        ui.label(egui::RichText::new(p).size(12.5).color(pal.orange));
                    }
                } else {
                    if dialog_button(ui, &pal, "convert", "Convert", Look::Primary, true) {
                        go = true;
                    }
                    if dialog_button(ui, &pal, "back", "Back", Look::Plain, true) {
                        w.step = 1;
                    }
                    let unprepared: Vec<&str> = w.vms.iter().filter(|v| v.vm.family != Family::Other && !self.is_prepared(&w.key, &v.vm.id) && !files).map(|v| v.name.as_str()).collect();
                    if !unprepared.is_empty() {
                        let t = if unprepared.len() == 1 { format!("{} isn't prepared: it may not start", unprepared[0]) } else { format!("{} aren't prepared: they may not start", unprepared.len()) };
                        ui.label(egui::RichText::new(t).size(12.5).color(pal.orange));
                    }
                }
            });
        });
        if let Some(sid) = new_pool {
            w.sid = sid;
            let vms: Vec<SourceVm> = w.vms.drain(..).map(|v| v.vm).collect();
            self.wizard_for_pool(w, vms);
        }
        if let Some(m) = mark {
            let key = w.key.clone();
            self.mark_prepared(&key, &m);
        }
        if let (Some(i), Some(src)) = (run_prep, source.clone()) {
            let v = &w.vms[i];
            let k = (w.key.clone(), v.vm.id.clone());
            self.conv.preparing.insert(k, None);
            let (vm, login) = (v.vm.clone(), (v.user.clone(), v.password.clone()));
            let (key, tx, ctx2) = (w.key.clone(), self.tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let result = src.prepare(&vm, Some((&login.0, &login.1)));
                let _ = tx.send(Msg::Convert(ConvMsg::Prepared { key, vm: vm.id.clone(), result }));
                ctx2.request_repaint();
            });
        }
        if go {
            if let Some(src) = source {
                self.start_conversion(w, src, &from, kind, helper.is_some(), &pool, ctx);
                return false;
            }
        }
        keep
    }

    /// Step 1: the pool, server and storage; what each network becomes; each VM's name and template.
    fn wizard_where(&self, ui: &mut Ui, w: &mut Wizard, pool: &PoolView, pools: &[(u32, String)], new_pool: &mut Option<u32>) {
        let pal = self.pal;
        let lw = LABEL_WIDTH;
        ui.label(egui::RichText::new("WHERE").size(11.0).color(pal.ink_faint));
        if pools.len() > 1 {
            ui.horizontal(|ui| {
                field_label(ui, "Pool:", lw);
                combo("conv-pool", pool.name.clone(), ui.available_width() - 8.0).show_ui(ui, |ui| {
                    for (sid, name) in pools {
                        if ui.selectable_label(*sid == w.sid, name).clicked() && *sid != w.sid {
                            *new_pool = Some(*sid);
                        }
                    }
                });
            });
        }
        // the server: choosing one moves the storage along, when it was another server's own (to
        // this one's, or else to shared storage)
        let sr_host = pool.srs.iter().find(|x| x.0 == w.sr).and_then(|x| x.2.clone());
        let shared = pool.srs.iter().any(|x| x.2.is_none());
        let best_sr = |host: Option<&String>| -> Option<String> {
            let most_free = |a: &&(String, String, Option<String>, i64, bool, i64, i64), b: &&(String, String, Option<String>, i64, bool, i64, i64)| a.3.cmp(&b.3);
            host.and_then(|h| pool.srs.iter().filter(|x| x.2.as_ref() == Some(h)).max_by(most_free)).or_else(|| pool.srs.iter().filter(|x| x.2.is_none()).max_by(most_free)).map(|x| x.0.clone())
        };
        ui.horizontal(|ui| {
            field_label(ui, "Server:", lw);
            let name = |h: &Option<String>| match h {
                None => "Any server".to_string(),
                Some(r) => pool.hosts.iter().find(|x| x.0 == *r).map(|x| format!("{}  ·  {} memory free", x.1, fmt_bytes(x.3))).unwrap_or_else(|| "—".into()),
            };
            combo("conv-host", name(&w.host), ui.available_width() - 8.0).show_ui(ui, |ui| {
                // (any server only onto shared storage)
                if shared && ui.selectable_label(w.host.is_none(), "Any server  ·  on shared storage").clicked() {
                    w.host = None;
                    if sr_host.is_some() {
                        w.sr = best_sr(None).unwrap_or_default();
                    }
                }
                for h in &pool.hosts {
                    if ui.selectable_label(w.host.as_deref() == Some(h.0.as_str()), format!("{}  ·  {} memory free", h.1, fmt_bytes(h.3))).clicked() {
                        w.host = Some(h.0.clone());
                        if sr_host.as_ref().is_some_and(|s| *s != h.0) {
                            w.sr = best_sr(Some(&h.0)).unwrap_or_default();
                        }
                    }
                }
            });
        });
        ui.horizontal(|ui| {
            field_label(ui, "Storage:", lw);
            let label = |x: &(String, String, Option<String>, i64, bool, i64, i64)| {
                let on = x.2.as_ref().and_then(|h| pool.hosts.iter().find(|y| y.0 == *h)).map(|y| format!(" · on {}", y.1)).unwrap_or_default();
                format!("{}  ·  {}, {} free{}", x.1, if x.4 { "thin" } else { "thick" }, fmt_bytes(x.3), on)
            };
            let shown = pool.srs.iter().find(|x| x.0 == w.sr).map(label).unwrap_or_else(|| "Choose…".into());
            combo("conv-sr", shown, ui.available_width() - 8.0).show_ui(ui, |ui| {
                for x in &pool.srs {
                    // another server's own storage isn't offered once a server's chosen
                    if x.2.is_some() && w.host.is_some() && x.2 != w.host {
                        continue;
                    }
                    if ui.selectable_label(w.sr == x.0, label(x)).clicked() {
                        w.sr = x.0.clone();
                        if x.2.is_some() {
                            w.host = x.2.clone();
                        }
                    }
                }
            });
        });
        // how much it takes there
        if let Some(x) = pool.srs.iter().find(|x| x.0 == w.sr) {
            let need: u64 = w.vms.iter().map(|v| if x.4 { v.vm.written() } else { v.vm.size() }).sum();
            let full: u64 = w.vms.iter().map(|v| v.vm.size()).sum();
            ui.horizontal(|ui| {
                ui.add_space(lw + ui.spacing().item_spacing.x);
                ui.vertical(|ui| {
                    let (used, size, free) = (x.5 as f64, x.6.max(1) as f64, x.3);
                    let text = if x.4 { format!("+{} now ({} if they fill up)", fmt_bytes(need as i64), fmt_bytes(full as i64)) } else { format!("+{} (thick: each disk's whole size)", fmt_bytes(need as i64)) };
                    let fits = (need as i64) <= free;
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 14.0;
                        legend(ui, &pal, pal.blue, &format!("{} used", fmt_bytes(x.5)));
                        legend(ui, &pal, Color32::from_rgb(227, 179, 65), &text);
                        legend(ui, &pal, Color32::TRANSPARENT, &format!("{} left", fmt_bytes((free - need as i64).max(0))));
                    });
                    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width() - 8.0, 6.0), Sense::hover());
                    let p = ui.painter();
                    p.rect_filled(r, Rounding::same(3.0), pal.border);
                    let a = (used / size).min(1.0) as f32;
                    let b = ((used + need as f64) / size).min(1.0) as f32;
                    p.rect_filled(Rect::from_min_size(r.min, vec2(r.width() * a, r.height())), Rounding::same(3.0), pal.blue);
                    p.rect_filled(Rect::from_min_max(pos2(r.min.x + r.width() * a, r.min.y), pos2(r.min.x + r.width() * b, r.max.y)), Rounding::same(0.0), if fits { Color32::from_rgb(227, 179, 65) } else { pal.red });
                });
            });
        }
        // the networks
        if !w.nets.is_empty() {
            ui.add_space(6.0);
            ui.label(egui::RichText::new("NETWORKS  ·  WHERE EACH NETWORK CARD GOES IN THE POOL").size(11.0).color(pal.ink_faint));
            egui::Frame::none().stroke(Stroke::new(1.0_f32, pal.border)).rounding(Rounding::same(8.0)).inner_margin(egui::Margin::symmetric(10.0, 6.0)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                for (k, x) in w.nets.iter_mut().enumerate() {
                    if k > 0 {
                        ui.add_space(2.0);
                        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
                        ui.painter().line_segment([r.left_center(), r.right_center()], Stroke::new(1.0_f32, pal.border));
                        ui.add_space(2.0);
                    }
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.set_width(200.0);
                            let name = match x.vlan {
                                Some(v) => format!("{}  ·  VLAN {}", if x.source.is_empty() { "(no network)" } else { &x.source }, v),
                                None => if x.source.is_empty() { "(no network)".to_string() } else { x.source.clone() },
                            };
                            ui.label(egui::RichText::new(name).color(pal.ink));
                            ui.label(egui::RichText::new(x.users.join(", ")).size(11.5).color(pal.ink_soft));
                        });
                        let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
                        ui.painter().arrow(r.left_center() + vec2(2.0, 0.0), vec2(16.0, 0.0), Stroke::new(1.5_f32, pal.grey));
                        let shown = match &x.choice {
                            NetChoice::Existing(r) => pool.nets.iter().find(|n| n.0 == *r).map(|n| net_label(n)).unwrap_or_else(|| "Choose…".into()),
                            NetChoice::New(name, card, v) => format!("New network “{}”  ·  VLAN {} on {}", name, v, card),
                            NetChoice::Without => "No network card (add one later)".into(),
                        };
                        combo(&format!("conv-net-{}", k), shown, ui.available_width() - 8.0).show_ui(ui, |ui| {
                            for n in &pool.nets {
                                if ui.selectable_label(x.choice == NetChoice::Existing(n.0.clone()), net_label(n)).clicked() {
                                    x.choice = NetChoice::Existing(n.0.clone());
                                }
                            }
                            if let Some(v) = x.vlan {
                                let name = if x.source.is_empty() { format!("VLAN {}", v) } else { x.source.clone() };
                                let new = NetChoice::New(name.clone(), pool.card.clone(), v);
                                if ui.selectable_label(x.choice == new, format!("New network “{}”  ·  VLAN {} on {}", name, v, pool.card)).clicked() {
                                    x.choice = new;
                                }
                            }
                            ui.separator();
                            if ui.selectable_label(x.choice == NetChoice::Without, "No network card (add one later)").clicked() {
                                x.choice = NetChoice::Without;
                            }
                        });
                    });
                }
            });
            ui.horizontal(|ui| {
                tick(ui, &pal, &mut w.keep_macs, "Keep their MAC addresses");
                ui.label(egui::RichText::new("·  the same addresses from DHCP, and licences tied to a card").size(12.0).color(pal.ink_soft));
            });
            for x in &w.nets {
                if let NetChoice::New(name, card, v) = &x.choice {
                    ui.label(egui::RichText::new(format!("The pool has nothing on VLAN {}: the app makes “{}” on {} first, on every server.", v, name, card)).size(12.0).color(pal.ink_soft));
                }
            }
            if w.nets.iter().any(|x| x.choice == NetChoice::Without) {
                ui.label(egui::RichText::new("A card left out isn't made: the VM starts without it. Add one later in its Overview, under Networking.").size(12.0).color(pal.ink_soft));
            }
        }
        // each VM: its name and template
        ui.add_space(6.0);
        let heading = if w.vms.len() == 1 { "THE NEW VM  ·  ITS NAME AND TEMPLATE" } else { "THE NEW VMS  ·  THEIR NAMES AND TEMPLATES" };
        ui.label(egui::RichText::new(heading).size(11.0).color(pal.ink_faint));
        for (k, v) in w.vms.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                text_field(ui, &pal, &mut v.name, 150.0, false).on_hover_text("Its name on the pool");
                let shown = pool.templates.iter().find(|t| t.0 == v.template).map(|t| t.1.clone()).unwrap_or_else(|| "Choose a template…".into());
                combo(&format!("conv-tpl-{}", k), shown, 250.0).height(320.0).show_ui(ui, |ui| {
                    for t in &pool.templates {
                        if ui.selectable_label(v.template == t.0, &t.1).clicked() {
                            v.template = t.0.clone();
                        }
                    }
                })
                .response
                .on_hover_text("The template it's made from: XenServer's settings for its system");
                ui.label(egui::RichText::new(boot_words(&v.vm)).size(12.5).color(pal.ink));
                ui.label(egui::RichText::new(size_words(&v.vm)).size(12.5).color(pal.ink_soft));
            });
            if let Some(n) = &v.vm.note {
                ui.label(egui::RichText::new(n.as_str()).size(12.0).color(pal.orange));
            }
        }
        ui.label(egui::RichText::new("The template gives XenServer the right settings for the VM's system; it's picked from what the source says it runs. Its CPUs, memory, boot mode (UEFI or BIOS) and the order of its disks stay as they were.").size(12.0).color(pal.ink_soft));
    }

    /// Makes the jobs and starts them (one after another, in the background), and shows them.
    #[allow(clippy::too_many_arguments)]
    fn start_conversion(&mut self, w: &Wizard, source: Arc<dyn Source>, from: &str, kind: &str, helper: bool, pool: &PoolView, ctx: &egui::Context) {
        let Some(s) = self.server(w.sid) else { return };
        let (conn, server) = (s.conn.clone(), s.name());
        let sr_host = pool.srs.iter().find(|x| x.0 == w.sr).and_then(|x| x.2.clone());
        let address = sr_host.as_ref().and_then(|h| pool.hosts.iter().find(|x| x.0 == *h)).map(|x| x.2.clone());
        let mut names: HashMap<String, String> = HashMap::new();
        for t in &pool.templates {
            names.insert(t.0.clone(), t.1.clone());
        }
        for x in &pool.srs {
            names.insert(x.0.clone(), x.1.clone());
        }
        for n in &pool.nets {
            names.insert(n.0.clone(), n.1.clone());
        }
        for h in &pool.hosts {
            names.insert(h.0.clone(), h.1.clone());
        }
        // (a network left out has no plan: its cards aren't made)
        let nets: Vec<NetPlan> = w
            .nets
            .iter()
            .filter_map(|x| {
                let to = match &x.choice {
                    NetChoice::Existing(r) => NetTo::Existing(r.clone()),
                    NetChoice::New(name, card, v) => NetTo::New(NewNetwork { name: name.clone(), description: format!("Made for VMs converted from {}", from), mtu: 1500, vlan: Some((card.clone(), *v)) }),
                    NetChoice::Without => return None,
                };
                Some(NetPlan { source: x.source.clone(), to })
            })
            .collect();
        let plan = Plan {
            source,
            from: from.into(),
            kind: kind.into(),
            host: w.host.clone(),
            address,
            sr: w.sr.clone(),
            nets,
            keep_macs: w.keep_macs,
            shutdown: w.shutdown,
            start: w.start,
            helper,
            vms: w.vms.iter().map(|v| VmPlan { vm: v.vm.clone(), name: v.name.trim().to_string(), template: v.template.clone() }).collect(),
            names,
        };
        let to = format!("{}{}", pool.srs.iter().find(|x| x.0 == w.sr).map(|x| x.1.clone()).unwrap_or_default(), w.host.as_ref().and_then(|h| pool.hosts.iter().find(|x| x.0 == *h)).map(|x| format!(" on {}", x.1)).unwrap_or_default());
        let mut jobs = vec![];
        for vp in &plan.vms {
            let text = format!("Converting '{}' from {}", vp.vm.name, from);
            let log = self.add_log(w.sid, &server, None, &vp.name, &text, "In progress");
            self.if_closed(log, "The copy stops, and the VM half made on the pool is left there: delete it.".into());
            let prog = self.progress(log);
            self.conv.next_job += 1;
            let job = Arc::new(Job::new(self.conv.next_job, &plan, vp, &to, log, prog));
            let mut one = plan.clone();
            one.vms = vec![vp.clone()];
            // (its networks are made by then)
            self.conv.plans.insert(job.id, (w.sid, one));
            jobs.push(job);
        }
        // (newest first, a batch in the order it runs)
        self.conv.jobs.splice(0..0, jobs.iter().cloned());
        self.conv.picked.clear();
        self.tab = Tab::Conversions;
        let (tx, ctx2) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            convert::run::run(conn, plan, jobs, &|j| {
                let _ = tx.send(Msg::Convert(ConvMsg::JobDone { id: j.id }));
                ctx2.request_repaint();
            });
        });
    }

    /// Try Again: the same VM's conversion as a new job (its source looked at again first).
    fn retry_job(&mut self, id: u64, ctx: &egui::Context) {
        let Some((sid, mut plan)) = self.conv.plans.get(&id).cloned() else { return };
        let Some(s) = self.server(sid) else { return };
        let (conn, server) = (s.conn.clone(), s.name());
        // the networks it made the first time are there now
        if let Some(d) = s.data.as_ref() {
            for n in plan.nets.iter_mut() {
                if let NetTo::New(nw) = &n.to {
                    if let Some(x) = d.networks.iter().find(|x| x.name == nw.name) {
                        n.to = NetTo::Existing(x.rref.clone());
                    }
                }
            }
        }
        let Some(vp) = plan.vms.first().cloned() else { return };
        // its state now at the source
        let fresh = self.conv.sources.iter().find(|e| e.conf.address == plan.from).and_then(|e| e.vms.iter().find(|v| v.id == vp.vm.id).cloned());
        if let Some(v) = fresh {
            plan.vms[0].vm = v;
        }
        let to = self.conv.jobs.iter().find(|j| j.id == id).map(|j| j.to.clone()).unwrap_or_default();
        let log = self.add_log(sid, &server, None, &vp.name, &format!("Converting '{}' from {} (again)", vp.vm.name, plan.from), "In progress");
        self.if_closed(log, "The copy stops, and the VM half made on the pool is left there: delete it.".into());
        let prog = self.progress(log);
        self.conv.next_job += 1;
        let job = Arc::new(Job::new(self.conv.next_job, &plan, &plan.vms[0], &to, log, prog));
        self.conv.plans.insert(job.id, (sid, plan.clone()));
        self.conv.jobs.retain(|j| j.id != id);
        self.conv.jobs.insert(0, job.clone());
        let (tx, ctx2) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            convert::run::run(conn, plan, vec![job], &|j| {
                let _ = tx.send(Msg::Convert(ConvMsg::JobDone { id: j.id }));
                ctx2.request_repaint();
            });
        });
    }

    /// The Conversions tab: each conversion as a card, newest first.
    pub(super) fn conversions_page(&mut self, ui: &mut Ui, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let ctx = ui.ctx().clone();
        ui.add_space(10.0);
        if self.conv.jobs.is_empty() {
            ui.label(egui::RichText::new("No conversions yet. On the VMs tab, tick the VMs to bring over, then Convert.").color(pal.ink_soft));
            return;
        }
        let states: Vec<(Arc<Job>, convert::run::JobState)> = self.conv.jobs.iter().map(|j| {
            j.sample();
            (j.clone(), j.snapshot())
        }).collect();
        let going = states.iter().filter(|s| s.1.outcome.is_none()).count();
        let done = states.iter().filter(|s| matches!(s.1.outcome, Some(Ok(_)))).count();
        let failed = states.len() - going - done;
        let through_here = states.iter().any(|s| s.1.outcome.is_none() && !s.1.via_helper);
        let mut retry: Option<u64> = None;
        egui::ScrollArea::vertical().id_salt("conversions").auto_shrink([false; 2]).show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(1000.0));
            ui.spacing_mut().item_spacing.y = 8.0;
            let mut parts = vec![];
            if going > 0 {
                parts.push(format!("{} going", going));
            }
            if done > 0 {
                parts.push(format!("{} done", done));
            }
            if failed > 0 {
                parts.push(format!("{} failed", failed));
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(parts.join(" · ")).color(pal.ink));
                if through_here {
                    ui.label(egui::RichText::new("The disks pass through this computer: keep it on and on the network until they're done.").size(12.5).color(pal.ink_soft));
                }
            });
            for (job, st) in &states {
                let (border, fill) = match &st.outcome {
                    None if st.started.is_none() => (pal.border, pal.sidebar),
                    None => (pal.accent.gamma_multiply(0.6), pal.sidebar),
                    Some(Ok(_)) => (pal.border, pal.sidebar),
                    Some(Err(_)) if job.cancel.load(std::sync::atomic::Ordering::Relaxed) => (pal.border, pal.sidebar),
                    Some(Err(_)) => (pal.red.gamma_multiply(0.6), pal.sidebar),
                };
                egui::Frame::none().fill(fill).stroke(Stroke::new(1.0_f32, border)).rounding(Rounding::same(10.0)).inner_margin(egui::Margin::symmetric(14.0, 10.0)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 6.0;
                    // its name, where from and to, and its buttons
                    ui.horizontal(|ui| {
                        match &st.outcome {
                            Some(Ok(_)) => {
                                let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                                paint_glyph(ui.painter(), r, "check", pal.green);
                            }
                            Some(Err(_)) => {
                                let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                                paint_glyph(ui.painter(), r, "error", pal.red);
                            }
                            None => {}
                        }
                        ui.label(egui::RichText::new(job.name.as_str()).size(15.0).strong().color(pal.ink));
                        let when = match (&st.outcome, st.ended) {
                            (Some(_), Some(e)) => format!("  ·  ended {} ago", fmt_took(e.elapsed())),
                            _ => String::new(),
                        };
                        ui.label(egui::RichText::new(format!("{} → {}{}", job.from, job.to, when)).size(12.5).color(pal.ink_soft));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            match &st.outcome {
                                None => {
                                    if flat_button(ui, "close", "Cancel", None, !job.cancel.load(std::sync::atomic::Ordering::Relaxed), false, &pal).on_hover_text("Stop it; what was made on the pool is taken away").clicked() {
                                        job.stop();
                                    }
                                }
                                Some(Ok(vm)) => {
                                    if let Some((sid, _)) = self.conv.plans.get(&job.id) {
                                        if flat_button(ui, "newvm", "Open VM", Some(pal.accent), true, false, &pal).clicked() {
                                            out.push(Intent::Select(format!("vm:{}:{}", sid, vm)));
                                        }
                                    }
                                }
                                Some(Err(_)) => {
                                    if flat_button(ui, "retry", "Try Again", None, true, false, &pal).clicked() {
                                        retry = Some(job.id);
                                    }
                                }
                            }
                            if flat_button(ui, "report", "Log", None, true, false, &pal).on_hover_text("What it did, call by call").clicked() {
                                out.push(Intent::ShowLog(job.log));
                            }
                        });
                    });
                    // its steps
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 14.0;
                        for (label, m) in &st.steps {
                            let (col, text_col) = match m {
                                Mark::Done => (pal.green, pal.green),
                                Mark::Now => (pal.accent, pal.link),
                                Mark::Failed => (pal.red, pal.red),
                                _ => (pal.grey, pal.ink_soft),
                            };
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 5.0;
                                let (r, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                                match m {
                                    Mark::Done => paint_glyph(ui.painter(), r.expand(2.0), "check", col),
                                    Mark::Now => {
                                        ui.painter().circle_filled(r.center(), 4.5, col);
                                    }
                                    Mark::Failed => paint_glyph(ui.painter(), r.expand(2.0), "close", col),
                                    _ => {
                                        ui.painter().circle_stroke(r.center(), 3.5, Stroke::new(1.3_f32, col));
                                    }
                                }
                                let t = egui::RichText::new(label.as_str()).size(12.5).color(text_col);
                                ui.label(if *m == Mark::Now { t.strong() } else { t });
                            });
                        }
                    });
                    match &st.outcome {
                        None => {
                            ui.label(egui::RichText::new(st.doing.as_str()).size(12.5).color(pal.ink));
                            if st.total > 0 {
                                let frac = st.sent as f32 / st.total as f32;
                                let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
                                progress_bar(ui.painter(), r, frac, pal.accent, &pal);
                                ui.horizontal(|ui| {
                                    let via = if st.via_helper { "  ·  by the Helper VM, straight from the source" } else { "" };
                                    ui.label(egui::RichText::new(format!("{} of {}{}", fmt_bytes(st.sent as i64), fmt_bytes(st.total as i64), via)).size(12.0).color(pal.ink_soft));
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if let Some(rate) = job.rate() {
                                            let left = ((st.total - st.sent.min(st.total)) as f64 / rate.max(1.0)) as u64;
                                            ui.label(egui::RichText::new(format!("{}/s  ·  {} left", fmt_bytes(rate as i64), fmt_took(Duration::from_secs(left)))).size(12.0).color(pal.ink_soft));
                                        }
                                    });
                                });
                            }
                        }
                        Some(Err(why)) => {
                            ui.add(egui::Label::new(egui::RichText::new(why.as_str()).color(pal.ink)).wrap());
                        }
                        Some(Ok(_)) => {}
                    }
                    for n in &st.notes {
                        ui.add(egui::Label::new(egui::RichText::new(n.as_str()).size(12.5).color(pal.ink_soft)).wrap());
                    }
                });
            }
        });
        if let Some(id) = retry {
            self.retry_job(id, &ctx);
        }
        if going > 0 {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }
}

/// Step 2: each VM's preparation (run in it, or the command to paste), then what happens before
/// and after; how much is copied, and by what.
#[allow(clippy::too_many_arguments)]
fn wizard_prepare(ui: &mut Ui, pal: &Pal, w: &mut Wizard, conv: &ConvView, runs: bool, files: bool, kind: &str, helper: &Option<HelperVm>, run_prep: &mut Option<usize>, mark: &mut Option<String>) {
    ui.label(egui::RichText::new(if files { "PREPARE EACH ONE" } else { "PREPARE EACH ONE  ·  WHILE IT STILL RUNS ON ITS SOURCE" }).size(11.0).color(pal.ink_faint));
    ui.add(
        egui::Label::new(
            egui::RichText::new("So it starts on XenServer. Windows needs the disk drivers XenServer shows it turned on, or it stops at a blue screen (0x7B). Linux needs Xen's drivers in its boot image, and its cards keep their names so its network settings still fit.")
                .size(12.5)
                .color(pal.ink_soft),
        )
        .wrap(),
    );
    if files {
        ui.add(egui::Label::new(egui::RichText::new("A file can't be prepared any more: the command is run in the VM on its old hypervisor, before it's exported. If it wasn't, it may not start; export it again after running it.").size(12.5).color(pal.orange)).wrap());
    }
    let ctx = ui.ctx().clone();
    for (i, v) in w.vms.iter_mut().enumerate() {
        let k = (w.key.clone(), v.vm.id.clone());
        let prepared = conv.prepared.iter().any(|(s, id, _)| *s == w.key && *id == v.vm.id);
        let state = conv.preparing.get(&k);
        egui::Frame::none().stroke(Stroke::new(1.0_f32, pal.border)).rounding(Rounding::same(8.0)).inner_margin(egui::Margin::symmetric(10.0, 8.0)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(v.name.as_str()).strong().color(pal.ink));
                let what = convert::os_name(&v.vm.os);
                let tools = match (v.vm.agent, kind) {
                    (true, "VMware") => " · VMware Tools running",
                    (true, _) => " · its guest agent answers",
                    _ => "",
                };
                ui.label(egui::RichText::new(format!("{}{}", what, tools)).size(12.0).color(pal.ink_soft));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (text, col) = match (v.vm.family, prepared, state) {
                        (Family::Other, _, _) => ("Nothing to prepare".to_string(), pal.ink_faint),
                        (_, true, _) => ("Prepared".to_string(), pal.green),
                        (_, _, Some(None)) => ("Preparing…".to_string(), pal.accent),
                        (_, _, Some(Some(_))) => ("Preparing failed".to_string(), pal.red),
                        _ if files => ("Not known".to_string(), pal.ink_soft),
                        _ => ("Not prepared yet".to_string(), pal.orange),
                    };
                    ui.label(egui::RichText::new(text).size(12.5).color(col));
                });
            });
            if v.vm.family == Family::Other {
                return;
            }
            if let Some(Some(why)) = state {
                ui.add(egui::Label::new(egui::RichText::new(why.as_str()).size(12.5).color(pal.red)).wrap());
            }
            if let Some(notes) = conv.prep_notes.get(&k) {
                for n in notes {
                    ui.add(egui::Label::new(egui::RichText::new(format!("Note: {}", n)).size(12.5).color(pal.orange)).wrap());
                }
            }
            if prepared {
                return;
            }
            // run it there
            if runs && !files && v.vm.agent {
                ui.horizontal(|ui| {
                    if kind == "VMware" {
                        ui.label(egui::RichText::new(if v.vm.family == Family::Windows { "Windows administrator:" } else { "Its root account:" }).size(12.5));
                        text_field(ui, pal, &mut v.user, 140.0, false);
                        text_field(ui, pal, &mut v.password, 130.0, true);
                    }
                    let busy = matches!(state, Some(None));
                    let ok = !busy && (kind != "VMware" || (!v.user.is_empty() && !v.password.is_empty()));
                    if dialog_button(ui, pal, "start", if busy { "Running…" } else { "Run it for me" }, Look::Primary, ok) {
                        *run_prep = Some(i);
                    }
                });
            }
            // or paste it
            let cmd = if v.vm.family == Family::Windows { convert::prep::windows_command() } else { convert::prep::linux_command() };
            egui::Frame::none().fill(pal.rail).stroke(Stroke::new(1.0_f32, pal.border)).rounding(Rounding::same(6.0)).inner_margin(egui::Margin::symmetric(8.0, 5.0)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let copied = w.copied.contains(&v.vm.id);
                    let label = if copied { "Copied" } else { "Copy" };
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if copied && flat_button(ui, "check", "Mark Prepared", Some(pal.green), true, false, pal).on_hover_text("Once you've run it in the VM").clicked() {
                            *mark = Some(v.vm.id.clone());
                        }
                        if flat_button(ui, "copy", label, Some(pal.link), true, false, pal).on_hover_text("Copy the command, to paste it in the VM yourself").clicked() {
                            ctx.copy_text(cmd.clone());
                            w.copied.insert(v.vm.id.clone());
                        }
                        let shown: String = cmd.chars().take(200).collect();
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            let p = ui.painter().clone();
                            let r = ui.available_rect_before_wrap();
                            let g = fit(&p, &shown, FontId::monospace(11.5), pal.ink_soft, r.width());
                            let (rr, _) = ui.allocate_exact_size(vec2(r.width(), g.size().y), Sense::hover());
                            p.galley(rr.min, g, pal.ink_soft);
                        });
                    });
                });
            });
            let how = match (v.vm.family, runs && !files && v.vm.agent, kind) {
                (Family::Windows, true, "VMware") => "“Run it for me” goes through VMware Tools, with that Windows account. Or paste it in an administrator Command Prompt or PowerShell.",
                (Family::Windows, true, _) => "“Run it for me” goes through its guest agent. Or paste it in an administrator Command Prompt or PowerShell.",
                (Family::Windows, false, _) => "Paste it in an administrator Command Prompt or PowerShell in the VM.",
                (_, true, "VMware") => "“Run it for me” goes through VMware Tools, as root. Or paste it in a terminal in the VM (it asks for sudo).",
                (_, true, _) => "“Run it for me” goes through its guest agent. Or paste it in a terminal in the VM (it asks for sudo).",
                _ => "Paste it in a terminal in the VM (it asks for sudo).",
            };
            ui.label(egui::RichText::new(how).size(12.0).color(pal.ink_faint));
        });
    }
    // before and after
    ui.add_space(4.0);
    ui.label(egui::RichText::new("THEN").size(11.0).color(pal.ink_faint));
    if w.vms.iter().any(|v| v.vm.power == Power::On) {
        ui.horizontal(|ui| {
            tick(ui, pal, &mut w.shutdown, format!("Shut them down on {} first", kind));
            ui.label(egui::RichText::new("·  cleanly, through their guest tools; a disk is only copied while its VM is off").size(12.0).color(pal.ink_soft));
        });
    }
    tick(ui, pal, &mut w.start, "Start each one when it's done");
    let written: u64 = w.vms.iter().map(|v| v.vm.written()).sum();
    let mins = (written / (110 << 20)).div_ceil(60);
    let about = match mins {
        0..=1 => "a minute or two".to_string(),
        2..=89 => format!("{} minutes", mins),
        _ => format!("{:.1} hours", mins as f64 / 60.0),
    };
    let mut text = format!("{} to copy: about {} at 1 Gbit/s. ", fmt_bytes(written as i64), about);
    match helper {
        Some(h) if !files => text.push_str(&format!("The pool's Helper VM ({}) copies the disks it can read itself straight from the source; the others pass through this computer. ", h.machine)),
        _ => text.push_str("The disks pass through this computer. "),
    }
    if !files {
        text.push_str(&format!("The originals stay on {}, off; delete them there once the new ones work.", kind));
    }
    egui::Frame::none().fill(pal.hover).rounding(Rounding::same(8.0)).inner_margin(egui::Margin::symmetric(10.0, 8.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.add(egui::Label::new(egui::RichText::new(text).size(12.5).color(pal.ink)).wrap());
    });
}

/// Why step 1 can't go on yet.
fn wizard_problem(w: &Wizard, pool: &PoolView) -> Option<String> {
    let Some(x) = pool.srs.iter().find(|x| x.0 == w.sr) else { return Some("Choose where the disks go.".into()) };
    let need: u64 = w.vms.iter().map(|v| if x.4 { v.vm.written() } else { v.vm.size() }).sum();
    if need as i64 > x.3 {
        return Some(format!("Not enough room on {}: it needs {}, it has {}.", x.1, fmt_bytes(need as i64), fmt_bytes(x.3)));
    }
    if w.vms.iter().any(|v| v.name.trim().is_empty()) {
        return Some("Give each VM a name.".into());
    }
    if w.vms.iter().any(|v| v.template.is_empty()) {
        return Some("Choose a template for each VM.".into());
    }
    if w.nets.iter().any(|n| matches!(&n.choice, NetChoice::Existing(r) if r.is_empty())) {
        return Some("Choose a network for each card (or No network card).".into());
    }
    None
}

fn net_label(n: &(String, String, String, i64)) -> String {
    match (n.2.as_str(), n.3) {
        ("", _) => format!("{}  ·  private", n.1),
        (d, v) if v >= 0 => format!("{}  ·  VLAN {} on {}", n.1, v, d),
        (d, _) => format!("{}  ·  {}", n.1, d),
    }
}

/// A coloured dot and its words, as above the Move dialog's bars.
fn legend(ui: &mut Ui, pal: &Pal, col: Color32, text: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
        if col == Color32::TRANSPARENT {
            ui.painter().circle_stroke(r.center(), 3.2, Stroke::new(1.3_f32, pal.grey));
        } else {
            ui.painter().circle_filled(r.center(), 3.8, col);
        }
        ui.label(egui::RichText::new(text).size(12.0).color(pal.ink_soft));
    });
}

/// Whether `text` is cut short at width `w`.
fn g_elided(text: &str, w: f32, p: &egui::Painter) -> bool {
    p.layout_no_wrap(text.to_string(), FontId::proportional(13.0), Color32::WHITE).size().x > w
}

/// A source's connection, from what's remembered and its password.
fn make_source(conf: &SourceConf, password: &str) -> Arc<dyn Source> {
    match conf.kind {
        Kind::Proxmox => Arc::new(convert::proxmox::Proxmox::new(&conf.address, &conf.user, password)),
        _ => Arc::new(convert::vmware::Vmware::new(&conf.address, &conf.user, password)),
    }
}
