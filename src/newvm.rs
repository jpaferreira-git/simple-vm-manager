//! The New VM dialog: the fewest choices that make a VM (the ISO to install from, a size, the
//! server it runs on, where its disk goes and its network), with the rest worked out: the
//! template from the ISO's name (or, when nothing fits, Other install media: BIOS, nothing
//! assumed), the name from the template, all of a VM's CPUs in one socket. Advanced holds the
//! template, the boot mode, the CPU layout and a description. From a file (a .xva exported here
//! or from XenCenter), it's an import: the file's VM, with its name, server, storage and network.

use crate::app::{dialog_button, dialog_buttons, dialog_window, field_label, flat_button, fmt_took, text_field, Look, FIELD_HEIGHT};
use crate::conn::{ApiError, Progress, Said, Trace, CHECK_SECS, NOT_ANSWERING, NOT_PLUGGED};
use crate::icons::paint_glyph;
use crate::model::*;
use crate::theme::Pal;
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontFamily, FontId, Rect, Rounding, Sense, Stroke, Ui};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const GB: i64 = 1024 * 1024 * 1024;
const WIDTH: f32 = 540.0;
/// The labels' column (wider than the other dialogs': "Install from:", "CPU layout:").
const LABEL_WIDTH: f32 = 100.0;
/// The space between the form's rows.
const ROW_GAP: f32 = 12.0;

/// What it's doing while it's made.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Step {
    Ready,
    Check,
    Copy,
    Clone,
    Disk,
    SetUp,
    CloudInit,
    Start,
    Undo,
}

pub enum NewVmAction {
    None,
    Close,
    /// Make it, in the pool with this id.
    Create(u32, NewVm),
    /// Open the file chooser for a .xva.
    PickFile,
    /// Bring the file into the pool with this id.
    Import(u32, ImportVm),
}

/// One of the user's own templates (made from a VM): its disks come with it.
/// cloud-init's user and SSH keys as last typed (this session), offered again.
static CI_LAST: std::sync::Mutex<crate::cloudinit::CloudInit> = std::sync::Mutex::new(crate::cloudinit::CloudInit { hostname: String::new(), user: String::new(), password: String::new(), keys: String::new(), custom: None });

struct Own {
    rref: String,
    name: String,
    os: String,
    vcpus: i64,
    memory: i64,
    mem_min: i64,
    /// Its disks' size, and their storage.
    disk: i64,
    sr: String,
    /// Its network cards' networks, by name.
    nets: Vec<String>,
    /// A VM's snapshot (its name), not a template: the new VM is named after the VM.
    of: Option<String>,
}

struct Iso {
    vdi: String,
    name: String,
    /// Its library: the reference, the name, and where the share is.
    sr: String,
    lib: String,
    location: String,
    /// The server whose own library holds it (None: a shared library).
    host: Option<String>,
}

struct Store {
    rref: String,
    name: String,
    host: Option<String>,
    free: i64,
    /// Reached over the network, so checked before it's used; and where it is.
    remote: bool,
    location: String,
    /// A disk on it takes only what's written on it (not its whole size, as on LVM).
    thin: bool,
}

/// A storage being checked (since when), found answering, or not (why).
enum Check {
    Running(Instant),
    Fine,
    Failed(ApiError),
}

struct Server {
    rref: String,
    name: String,
    mem_free: i64,
    cpus: i64,
}

pub struct NewVmDialog {
    pub sid: u32,
    pool: String,
    isos: Vec<Iso>,
    iso: usize,
    templates: Vec<TemplateInfo>,
    /// The template the ISO's name points to (Other install media when none does).
    detected: usize,
    template: usize,
    template_picked: bool,
    name: String,
    name_typed: bool,
    taken: Vec<String>,
    /// 0, 1, 2: the ready sizes; 3: Custom.
    size: usize,
    cpus: String,
    memory: String,
    disk: String,
    servers: Vec<Server>,
    /// The server it runs on (None: any, which needs its disk on shared storage).
    server: Option<usize>,
    stores: Vec<Store>,
    store: usize,
    /// The pool's default storage.
    default_store: Option<usize>,
    nets: Vec<(String, String)>,
    /// None: no network.
    net: Option<usize>,
    boot: Option<Boot>,
    sockets: i64,
    description: String,
    advanced: bool,
    /// cloud-init, from a template or a snapshot: shown (its part open), used, what it's told,
    /// whether its hostname was typed (else it follows the name), the user-data typed by hand.
    ci_open: bool,
    ci_on: bool,
    ci: crate::cloudinit::CloudInit,
    ci_host_typed: bool,
    ci_custom: bool,
    ci_text: String,
    start: bool,
    busy: Option<String>,
    error: Option<String>,
    /// What was asked of the server while making it, and its answers.
    trace: Option<Arc<Trace>>,
    /// While it's made: how far the server's task is (as under Activities), since when, and the
    /// step it was at, when and how far when first seen (for the time left).
    progress: Option<Arc<Progress>>,
    since: Option<Instant>,
    pace: Option<(Step, Instant, f32)>,
    copied: Option<Instant>,
    /// The VM, once made (while it starts).
    made_vm: Option<String>,
    /// Made, but it didn't start: why.
    not_started: Option<String>,
    /// Whether the ISO's library, and the disk's storage when it's on the network, answer: by
    /// storage and the server it's for (None: any).
    checks: HashMap<(String, Option<String>), Check>,
    /// For the screenshot checks: Create, as if clicked.
    press: bool,
    /// Browse was clicked (the file chooser opens).
    press_pick: bool,
    /// The user's own templates; making it from one of them (else from an ISO), and which.
    own: Vec<Own>,
    from_own: bool,
    own_pick: usize,
    /// From a template of the user's (or a file): keep its network cards (else `net`).
    keep_nics: bool,
    /// Each server's address (an import onto a server's own storage goes to that server).
    addresses: HashMap<String, String>,
    /// Brought in from a .xva file (an import), and which, with what it holds or why it can't be read.
    from_file: bool,
    file: Option<std::path::PathBuf>,
    file_info: Option<Result<crate::xva::XvaInfo, String>>,
}

impl NewVmDialog {
    /// For pool `sid`, with `iso` (a disc) chosen, or else the first ISO file there is.
    pub fn new(sid: u32, d: &PoolData, iso: Option<&str>) -> NewVmDialog {
        let mut isos: Vec<Iso> = d
            .srs
            .iter()
            .filter(|x| x.is_iso())
            .flat_map(|x| x.files.iter().map(move |f| Iso { vdi: f.vdi.clone(), name: f.name.clone(), sr: x.rref.clone(), lib: x.name.clone(), location: x.location.clone(), host: x.host.clone() }))
            .collect();
        isos.sort_by_key(|i| i.name.to_lowercase());
        // where a disk can go: storage for disks (not ISO libraries, not drives), shared first, the most room first
        let mut stores: Vec<Store> = d
            .srs
            .iter()
            .filter(|x| !x.is_iso() && x.kind != "udev" && x.size > 0)
            .map(|x| Store { rref: x.rref.clone(), name: x.name.clone(), host: x.host.clone(), free: (x.size - x.used).max(0), remote: x.can_go_offline(), location: x.location.clone(), thin: x.is_thin() })
            .collect();
        stores.sort_by_key(|s| (s.host.is_some(), -s.free));
        let default_store = d.default_sr.as_deref().and_then(|r| stores.iter().position(|s| s.rref == r));
        // every network (VLANs and private ones too: their bridges are "xapi…" as well; the
        // servers' internal ones were left out when the pool was read), the first card's first
        let nets: Vec<(String, String)> = d.networks.iter().map(|n| (n.rref.clone(), n.name.clone())).collect();
        let net = d.networks.iter().position(|n| n.bridge == "xenbr0").or(if nets.is_empty() { None } else { Some(0) });
        let mut servers: Vec<Server> = d.hosts.iter().map(|h| Server { rref: h.rref.clone(), name: h.name.clone(), mem_free: h.mem_free, cpus: h.cpu_count }).collect();
        servers.sort_by_key(|s| s.name.to_lowercase());
        let mut own: Vec<Own> = d
            .user_templates
            .iter()
            .map(|t| {
                let disks: Vec<&VmDisk> = t.disks.iter().filter(|k| k.kind == "Disk").collect();
                Own {
                    rref: t.rref.clone(),
                    name: t.name.clone(),
                    os: t.os.clone(),
                    vcpus: t.vcpus.max(1),
                    memory: t.mem_dyn_max.max(t.mem_static_max.min(t.mem_dyn_max.max(1))),
                    mem_min: t.mem_static_min,
                    disk: disks.iter().map(|k| k.size).sum(),
                    sr: disks.first().map(|k| k.sr.clone()).unwrap_or_default(),
                    nets: t.nics.iter().map(|n| n.network.clone()).filter(|n| !n.is_empty()).collect(),
                    of: None,
                }
            })
            .collect();
        own.sort_by_key(|t| t.name.to_lowercase());
        let mut templates = d.templates.clone();
        if !templates.iter().any(|t| t.name == OTHER_MEDIA) {
            templates.insert(0, TemplateInfo { name: OTHER_MEDIA.into(), supports_uefi: true, mem_min: GB, vcpus: 1, ..Default::default() });
        }
        let mut dlg = NewVmDialog {
            sid,
            pool: d.name.clone(),
            iso: iso.and_then(|v| isos.iter().position(|i| i.vdi == v)).unwrap_or(0),
            isos,
            templates,
            detected: 0,
            template: 0,
            template_picked: false,
            name: String::new(),
            name_typed: false,
            taken: d.vms.iter().map(|v| v.name.clone()).collect(),
            size: 1,
            cpus: String::new(),
            memory: String::new(),
            disk: String::new(),
            servers,
            server: None,
            stores,
            store: default_store.unwrap_or(0),
            default_store,
            nets,
            net,
            boot: None,
            sockets: 1,
            description: String::new(),
            advanced: false,
            ci_open: false,
            ci_on: false,
            ci: CI_LAST.lock().map(|c| c.clone()).unwrap_or_default(),
            ci_host_typed: false,
            ci_custom: false,
            ci_text: String::new(),
            start: true,
            busy: None,
            error: None,
            trace: None,
            progress: None,
            since: None,
            pace: None,
            copied: None,
            made_vm: None,
            not_started: None,
            checks: HashMap::new(),
            press: false,
            press_pick: false,
            own,
            from_own: false,
            own_pick: 0,
            keep_nics: true,
            addresses: d.hosts.iter().map(|h| (h.rref.clone(), h.address.clone())).collect(),
            from_file: false,
            file: None,
            file_info: None,
        };
        dlg.server = dlg.first_server();
        dlg.server_changed();
        dlg.iso_changed();
        dlg
    }

    /// Made from the user's template `tpl` (else as it was).
    pub fn with_template(mut self, tpl: &str) -> NewVmDialog {
        if let Some(i) = self.own.iter().position(|t| t.rref == tpl) {
            self.from_own = true;
            self.own_pick = i;
            self.own_changed();
        }
        self
    }

    /// cloud-init on and open, with a user (for the screenshots).
    pub fn with_cloud_init(mut self, user: &str, key: &str) -> NewVmDialog {
        (self.ci_open, self.ci_on, self.ci.user, self.ci.password, self.ci.keys) = (true, true, user.into(), String::new(), key.into());
        self
    }

    /// For the screenshot checks: cloud-init's user-data typed by hand.
    pub fn with_own_user_data(mut self) -> NewVmDialog {
        let mut c = self.ci.clone();
        c.hostname = crate::cloudinit::hostname_for(&self.name);
        self.ci_text = c.user_data() + "packages:\n  - nginx\nruncmd:\n  - systemctl enable --now nginx\n";
        (self.ci_open, self.ci_on, self.ci_custom) = (true, true, true);
        self
    }

    /// For the screenshot checks: being made, its disks copied in full, 37% there after three
    /// minutes.
    pub fn shot_copying(mut self) -> NewVmDialog {
        if let Some(i) = self.stores.iter().position(|s| self.own().map_or(false, |t| t.sr != s.rref) && s.host.is_some()) {
            if let Some(h) = self.stores[i].host.clone() {
                self.server = self.server_index(&h);
            }
            self.store = i;
        }
        let trace = Arc::new(Trace::default());
        let me = self.own().map(|t| t.name.clone()).unwrap_or_default();
        let to = self.chosen_store().map(|s| s.name.clone()).unwrap_or_default();
        trace.push(Said::Asked, format!("VM.copy  vm=\"{}\"  new_name=\"{}\"  sr=\"{}\"", me, self.name.trim(), to));
        self.creating(trace);
        let p = Progress::new();
        p.set_fraction(0.37);
        self.watch(p);
        let ago = |s| Instant::now().checked_sub(Duration::from_secs(s));
        self.since = ago(184);
        self.pace = ago(170).map(|t| (Step::Copy, t, 0.02));
        self
    }

    /// From one of a VM's snapshots: its disks as they were then, copied for a VM of its own (a
    /// snapshot is kept as a template is). Its CPUs, memory and cards are the VM's.
    pub fn with_snapshot(mut self, d: &PoolData, snap: &str) -> NewVmDialog {
        let Some((v, x)) = d.vms.iter().find_map(|v| v.snapshot(snap).map(|x| (v, x))) else { return self };
        let disks: Vec<&VmDisk> = v.disks.iter().filter(|k| k.kind == "Disk").collect();
        self.own.push(Own {
            rref: snap.into(),
            name: x.name.clone(),
            os: format!("snapshot of {}", v.name),
            vcpus: v.vcpus.max(1),
            memory: v.mem_dyn_max.max(1),
            mem_min: v.mem_static_min,
            disk: disks.iter().map(|k| k.size).sum(),
            sr: disks.first().map(|k| k.sr.clone()).unwrap_or_default(),
            nets: v.nics.iter().map(|n| n.network.clone()).filter(|n| !n.is_empty()).collect(),
            of: Some(v.name.clone()),
        });
        self.from_own = true;
        self.own_pick = self.own.len() - 1;
        self.own_changed();
        self
    }

    /// Another of the user's templates (or a switch to them): its name (unless one was typed), its
    /// size, its disks staying where they are (on the server that sees them), its network cards.
    fn own_changed(&mut self) {
        let Some(t) = self.own.get(self.own_pick) else { return };
        if !self.name_typed {
            let base = t.of.as_deref().unwrap_or_else(|| t.name.trim_end_matches(" template").trim_end_matches(" Template").trim());
            self.name = unique_name(if base.is_empty() { &t.name } else { base }, &self.taken);
        }
        self.size = 0;
        self.keep_nics = true;
        let sr = t.sr.clone();
        if let Some(i) = self.stores.iter().position(|s| s.rref == sr) {
            self.store = i;
            if let Some(h) = self.stores[i].host.clone() {
                self.server = self.server_index(&h);
            }
        }
    }

    /// The user's template chosen, when making it from one.
    fn own(&self) -> Option<&Own> {
        self.own.get(self.own_pick).filter(|_| self.from_own && !self.from_file)
    }

    /// Brought in from a file: `path` chosen (else the form waits for one).
    pub fn with_file(mut self, path: Option<std::path::PathBuf>) -> NewVmDialog {
        self.switch_to_file();
        if let Some(p) = path {
            self.set_file(p);
        }
        self
    }

    fn switch_to_file(&mut self) {
        // not started unless asked: a copy of a VM that's running would come up beside it, with
        // the same addresses and name on the network
        (self.from_file, self.from_own, self.name_typed, self.keep_nics, self.start) = (true, false, false, true, false);
        self.size = 0;
        self.file_changed();
    }

    /// The file chosen: what it holds is read (only its description, at the start).
    pub fn set_file(&mut self, p: std::path::PathBuf) {
        self.file_info = Some(crate::xva::read_header(&p));
        self.file = Some(p);
        self.name_typed = false;
        self.file_changed();
    }

    /// What the file holds, once it's been read and could be.
    fn xva(&self) -> Option<&crate::xva::XvaInfo> {
        self.file_info.as_ref().and_then(|r| r.as_ref().ok()).filter(|_| self.from_file)
    }

    /// Another file: its VM's name (unless one was typed), its cards kept as they are.
    fn file_changed(&mut self) {
        self.keep_nics = true;
        if let (Some(x), false) = (self.xva(), self.name_typed) {
            let name = if x.name.trim().is_empty() { "Imported VM".to_string() } else { x.name.trim().to_string() };
            self.name = unique_name(&name, &self.taken);
        } else if !self.name_typed {
            self.name.clear();
        }
        self.server = self.first_server();
        self.server_changed();
    }

    /// The import asked for, or why the choices don't add up.
    fn try_import(&self) -> Result<ImportVm, String> {
        let file = self.file.clone().ok_or("Pick the file to import.")?;
        let x = self.xva().ok_or("The file can't be read.")?;
        let host = self.server.map(|i| &self.servers[i]);
        let store = self.chosen_store().ok_or("Pick where its disks go.")?;
        let net = if self.keep_nics { None } else { self.net.and_then(|i| self.nets.get(i)) };
        if !self.keep_nics && net.is_none() {
            return Err("Pick its network.".into());
        }
        let mut names = HashMap::new();
        names.insert(store.rref.clone(), store.name.clone());
        if let Some(n) = net {
            names.insert(n.0.clone(), n.1.clone());
        }
        if let Some(h) = host {
            names.insert(h.rref.clone(), h.name.clone());
        }
        Ok(ImportVm {
            file,
            name: self.name.trim().to_string(),
            sr: store.rref.clone(),
            address: store.host.as_ref().and_then(|h| self.addresses.get(h).cloned()),
            host: host.map(|h| h.rref.clone()),
            network: net.map(|n| n.0.clone()),
            start: self.start && !x.template,
            template: x.template,
            names,
        })
    }

    /// For the screenshot checks: Advanced open.
    pub fn with_advanced(mut self) -> NewVmDialog {
        self.advanced = true;
        self
    }

    /// For the screenshot checks: Custom, with these CPUs and GB of memory and disk.
    pub fn with_custom(mut self, cpus: i64, memory: i64, disk: i64) -> NewVmDialog {
        self.size = 3;
        (self.cpus, self.memory, self.disk) = (cpus.to_string(), memory.to_string(), disk.to_string());
        self
    }

    /// For the screenshot checks: Create clicked.
    pub fn pressed(mut self) -> NewVmDialog {
        self.press = true;
        self
    }

    /// The server is making it: the dialog waits, showing each step in `trace` as it happens
    /// (Advanced folds, to leave room).
    pub fn creating(&mut self, trace: Arc<Trace>) {
        self.error = None;
        self.advanced = false;
        self.trace = Some(trace);
        (self.progress, self.since, self.pace) = (None, Some(Instant::now()), None);
        self.busy = Some(format!("Creating {}…", self.name.trim()));
    }

    /// How far the server is, as under Activities (the job's own progress).
    pub fn watch(&mut self, p: Arc<Progress>) {
        self.progress = Some(p);
    }

    /// What it's doing now, by the calls asked so far.
    fn step(&self) -> Step {
        let mut step = Step::Ready;
        for (k, text) in self.trace.as_ref().map(|t| t.lines()).unwrap_or_default() {
            match (k, text.split_whitespace().next().unwrap_or("")) {
                (Said::Note, _) if text.starts_with("cloud-init") => step = Step::CloudInit,
                (Said::Asked, "VM.copy") => step = Step::Copy,
                (Said::Asked, "VM.clone") => step = Step::Clone,
                (Said::Asked, "VM.provision") => step = Step::Disk,
                (Said::Asked, "VM.start") => step = Step::Start,
                (Said::Asked, "SR.scan" | "SR.get_PBDs") if step == Step::Ready => step = Step::Check,
                (Said::Asked, "VM.destroy") => step = Step::Undo,
                (Said::Asked, _) if step != Step::CloudInit => step = Step::SetUp,
                _ => {}
            }
        }
        step
    }

    /// While it's made, as under Activities: what it's doing now, for how long, how far (with the
    /// time left once that can be told) and a bar; a sliding one while there's no figure.
    fn progress_part(&mut self, ui: &mut Ui, pal: &Pal) {
        let step = self.step();
        let store = self.chosen_store().map(|s| s.name.clone()).unwrap_or_default();
        let words = match step {
            Step::Ready => "Getting ready".to_string(),
            Step::Check => "Checking the storage answers".to_string(),
            Step::Copy => format!("Copying its disks to {}", store),
            Step::Clone if self.own().is_some() => "Copying its disks, in an instant".to_string(),
            Step::Clone => "Making it from the template".to_string(),
            Step::Disk => format!("Making its disk on {}", store),
            Step::SetUp => "Setting up its CPUs, memory and network".to_string(),
            Step::CloudInit => "Writing its cloud-init disk".to_string(),
            Step::Start => "Starting it".to_string(),
            Step::Undo => "Undoing what was made".to_string(),
        };
        // the server's own figure, for the long steps it measures
        let fraction = self.progress.as_ref().and_then(|p| p.fraction()).filter(|_| matches!(step, Step::Copy | Step::Clone | Step::Disk));
        let now = Instant::now();
        if self.pace.as_ref().map_or(false, |p| p.0 != step) {
            self.pace = None;
        }
        if let (None, Some(f)) = (&self.pace, fraction) {
            self.pace = Some((step, now, f));
        }
        let left = match (&self.pace, fraction) {
            (Some((_, t0, f0)), Some(f)) if f > f0 + 0.01 && f < 1.0 && now.duration_since(*t0) >= Duration::from_secs(10) => {
                Some(Duration::from_secs_f64(now.duration_since(*t0).as_secs_f64() * (1.0 - f) as f64 / (f - f0) as f64))
            }
            _ => None,
        };
        let mut detail = vec![format!("{} so far", fmt_took(self.since.map(|t| t.elapsed()).unwrap_or_default()))];
        if let Some(l) = left {
            let m = (l.as_secs() + 59) / 60;
            detail.push(match m {
                0..=1 => "about a minute left".to_string(),
                2..=59 => format!("about {} min left", m),
                _ => format!("about {} h {:02} min left", m / 60, m % 60),
            });
        }
        let quiet = self.progress.as_ref().and_then(|p| p.silent_for()).filter(|d| d.as_secs() >= 5);

        let w = ui.available_width();
        let (r, _) = ui.allocate_exact_size(vec2(w, 70.0), Sense::hover());
        let p = ui.painter();
        p.rect(r, Rounding::same(8.0), pal.rail, Stroke::new(1.0_f32, pal.border));
        let (x, right) = (r.min.x + 40.0, r.max.x - 14.0);
        egui::Spinner::new().size(16.0).color(pal.accent).paint_at(ui, Rect::from_center_size(pos2(r.min.x + 21.0, r.min.y + 21.0), vec2(16.0, 16.0)));
        let pct = fraction.map(|f| p.text(pos2(right, r.min.y + 21.0), Align2::RIGHT_CENTER, format!("{}%", (f * 100.0).floor()), FontId::new(14.0, FontFamily::Name("bold".into())), pal.ink));
        let room = pct.map_or(right, |g| g.min.x - 12.0) - x;
        let g = p.layout_no_wrap(words, FontId::proportional(14.0), pal.ink);
        let g = if g.size().x > room { p.layout(g.job.text.clone(), FontId::proportional(13.0), pal.ink, room) } else { g };
        p.galley(pos2(x, r.min.y + 21.0 - g.size().y / 2.0), g, pal.ink);
        let d = p.text(pos2(x, r.min.y + 40.0), Align2::LEFT_CENTER, detail.join("  ·  "), FontId::proportional(12.5), pal.ink_soft);
        if let Some(q) = quiet {
            let gone = crate::conn::LOST_AFTER.saturating_sub(q).as_secs();
            p.text(pos2(d.max.x + 10.0, r.min.y + 40.0), Align2::LEFT_CENTER, format!("no answer for {} s, gives up in {} s", q.as_secs(), gone), FontId::proportional(12.5), pal.orange);
        }
        let bar = Rect::from_min_max(pos2(x, r.min.y + 55.0), pos2(right, r.min.y + 60.0));
        p.rect_filled(bar, Rounding::same(2.5), pal.border);
        match fraction {
            Some(f) => {
                p.rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * f.clamp(0.0, 1.0), bar.height())), Rounding::same(2.5), pal.accent);
            }
            None => {
                // a piece sliding along it: under way, how far not known
                let t = ui.input(|i| i.time) as f32 % 1.6 / 1.6;
                let len = bar.width() * 0.22;
                let a = bar.min.x - len + t * (bar.width() + len);
                let seg = Rect::from_min_max(pos2(a.max(bar.min.x), bar.min.y), pos2((a + len).min(bar.max.x), bar.max.y));
                if seg.width() > 0.0 {
                    p.rect_filled(seg, Rounding::same(2.5), pal.accent);
                }
            }
        }
        if self.made_vm.is_some() {
            ui.label(egui::RichText::new("It's made: closing this leaves it starting.").size(12.5).color(pal.ink_faint));
        }
    }

    /// It couldn't be made: why, in the dialog.
    pub fn failed(&mut self, why: String) {
        self.busy = None;
        self.error = Some(why);
        // storage may have gone away meanwhile: checked again on the way back to the form
        self.checks.clear();
    }

    /// It was made (`vm`), and it's starting: the dialog may close now; the start goes on.
    pub fn starting(&mut self, vm: String) {
        self.busy = Some(format!("Starting {}… It's made, so closing this leaves it starting.", self.name.trim()));
        self.made_vm = Some(vm);
    }

    /// The VM, once made.
    pub fn made_vm(&self) -> Option<&str> {
        self.made_vm.as_deref()
    }

    /// It was made, but didn't start: why, and nothing left to do but close.
    pub fn not_started(&mut self, why: String) {
        self.busy = None;
        self.not_started = Some(why);
    }

    /// The storage to check, each with the server it's for: the ISO's library, and the disk's
    /// storage when it's reached over the network.
    fn to_check(&self) -> Vec<(String, Option<String>)> {
        let host = self.server.map(|i| self.servers[i].rref.clone());
        let mut out = vec![];
        if self.from_file {
            // only where its disks go
        } else if let Some(t) = self.own() {
            // the template's disks' storage, when it's on the network
            if self.stores.iter().any(|s| s.rref == t.sr && s.remote) {
                out.push((t.sr.clone(), host.clone()));
            }
        } else if let Some(iso) = self.isos.get(self.iso) {
            out.push((iso.sr.clone(), host.clone()));
        }
        if let Some(st) = self.chosen_store().filter(|s| s.remote && !out.iter().any(|o| o.0 == s.rref)) {
            out.push((st.rref.clone(), host));
        }
        out
    }

    /// The checks to start now (each storage chosen that hasn't been checked for its server);
    /// marked as running. None while it's being made.
    pub fn wanted_checks(&mut self) -> Vec<(String, Option<String>)> {
        if self.busy.is_some() || self.trace.is_some() {
            return vec![];
        }
        let mut out = vec![];
        for k in self.to_check() {
            if !self.checks.contains_key(&k) {
                self.checks.insert(k.clone(), Check::Running(Instant::now()));
                out.push(k);
            }
        }
        out
    }

    /// A check's outcome.
    pub fn checked(&mut self, sr: String, host: Option<String>, result: Result<(), ApiError>) {
        if let Some(c) = self.checks.get_mut(&(sr, host)) {
            *c = match result {
                Ok(()) => Check::Fine,
                Err(e) => Check::Failed(e),
            };
        }
    }

    /// Whether every storage chosen has answered.
    fn checks_passed(&self) -> bool {
        self.to_check().iter().all(|k| matches!(self.checks.get(k), Some(Check::Fine)))
    }

    /// A storage's name, and where it is.
    fn sr_place(&self, sr: &str) -> (String, String) {
        match self.isos.iter().find(|i| i.sr == sr) {
            Some(i) => (i.lib.clone(), i.location.clone()),
            None => self.stores.iter().find(|s| s.rref == sr).map(|s| (s.name.clone(), s.location.clone())).unwrap_or_default(),
        }
    }

    /// A failed check in words: a title, then what happened and what to do.
    fn check_problem(&self, sr: &str, host: Option<&str>, e: &ApiError) -> (String, String) {
        let (name, location) = self.sr_place(sr);
        let instead = if self.isos.iter().any(|i| i.sr == sr) { "install from an ISO in another library" } else { "put the disk on other storage" };
        match e.code.as_str() {
            NOT_ANSWERING => {
                let what = if location.is_empty() { format!("It didn't answer in {} seconds.", CHECK_SECS) } else { format!("The pool can't reach {}: it didn't answer in {} seconds.", location, CHECK_SECS) };
                (format!("{} is offline.", name), format!("{} Turn it on, or {}.", what, instead))
            }
            NOT_PLUGGED => {
                let on = host.and_then(|h| self.servers.iter().find(|s| s.rref == h)).map_or("any server".to_string(), |s| s.name.clone());
                (format!("{} isn't connected.", name), format!("It isn't mounted on {} (Repair, on its page, mounts it again). Pick another server, or {}.", on, instead))
            }
            _ => (format!("{} isn't working.", name), format!("{} Try again, or {}.", e.message(), instead)),
        }
    }

    fn other_media(&self) -> usize {
        self.templates.iter().position(|t| t.name == OTHER_MEDIA).unwrap_or(0)
    }

    /// A new ISO: its template (unless one was picked), its name (unless one was typed), and its
    /// server when its library is a server's own.
    fn iso_changed(&mut self) {
        let iso = self.isos.get(self.iso).map(|i| i.name.clone()).unwrap_or_default();
        self.detected = detect_template(&iso, &self.templates).and_then(|t| self.templates.iter().position(|x| x.rref == t.rref && x.name == t.name)).unwrap_or(self.other_media());
        if !self.template_picked {
            self.template_changed(self.detected);
        }
        if !self.name_typed {
            self.name = unique_name(&vm_name_for(Some(&self.templates[self.template]), &iso), &self.taken);
        }
        if let Some(h) = self.iso_host() {
            let i = self.server_index(&h);
            if i.is_some() && i != self.server {
                self.server = i;
                self.server_changed();
            }
        }
    }

    /// Another template: its own boot mode again.
    fn template_changed(&mut self, i: usize) {
        self.template = i;
        self.boot = None;
    }

    /// Another server: its disk on storage that server sees (the one chosen if it still can be,
    /// else the pool's default, else the most room, shared first).
    fn server_changed(&mut self) {
        if self.stores.get(self.store).map_or(false, |s| self.usable(s)) {
            return;
        }
        let default = self.default_store.filter(|&i| self.usable(&self.stores[i]));
        if let Some(i) = default.or_else(|| self.stores.iter().position(|s| self.usable(s))) {
            self.store = i;
        }
    }

    /// The server to start with: the one the ISO's library or the pool's default storage belongs
    /// to; any, when the default storage is shared (or there is shared storage but no default);
    /// else the one with the most memory free that has storage.
    fn first_server(&self) -> Option<usize> {
        if let Some(h) = self.iso_host() {
            return self.server_index(&h);
        }
        match self.default_store.map(|i| &self.stores[i]) {
            Some(s) => s.host.as_deref().and_then(|h| self.server_index(h)),
            None if self.stores.iter().any(|s| s.host.is_none()) => None,
            None => self
                .servers
                .iter()
                .enumerate()
                .filter(|(_, x)| self.stores.iter().any(|s| s.host.as_deref() == Some(x.rref.as_str())))
                .max_by_key(|(_, x)| x.mem_free)
                .map(|(i, _)| i),
        }
    }

    /// The server whose own library holds the ISO (None: a shared library).
    fn iso_host(&self) -> Option<String> {
        if self.from_file || self.own().is_some() {
            return None;
        }
        self.isos.get(self.iso).and_then(|i| i.host.clone())
    }

    fn server_index(&self, r: &str) -> Option<usize> {
        self.servers.iter().position(|s| s.rref == r)
    }

    /// Whether the chosen server sees this storage (any server: only shared storage).
    fn usable(&self, s: &Store) -> bool {
        match (self.server, &s.host) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some(i), Some(h)) => self.servers[i].rref == *h,
        }
    }

    /// The disk's storage, if the chosen server sees it.
    fn chosen_store(&self) -> Option<&Store> {
        self.stores.get(self.store).filter(|s| self.usable(s))
    }

    /// Small, Medium and Large for this template: CPUs, GB of memory, GB of disk (Windows needs
    /// more), never below what the template asks for.
    fn presets(&self) -> [(i64, i64, i64); 3] {
        let t = &self.templates[self.template];
        let base = if t.is_windows() { [(2, 4, 64), (4, 8, 80), (8, 16, 128)] } else { [(1, 2, 20), (2, 4, 40), (4, 8, 80)] };
        let at_least = |v: i64, floor: i64| v.max(floor);
        base.map(|(c, m, k)| (at_least(c, t.vcpus), at_least(m, (t.mem_min + GB - 1) / GB), at_least(k, (t.disk + GB - 1) / GB)))
    }

    /// The chosen CPUs, memory and disk (in bytes), or why Custom's numbers won't do.
    fn sizes(&self) -> Result<(i64, i64, i64), String> {
        if self.from_file {
            return self.xva().map(|x| (x.vcpus, x.memory, x.disks.iter().sum())).ok_or_else(|| "Pick the file to import.".to_string());
        }
        if let Some(t) = self.own() {
            if self.size < 3 {
                return Ok((t.vcpus, t.memory, t.disk));
            }
            let cpus = self.cpus.trim().parse::<i64>().ok().filter(|v| *v > 0).ok_or("CPUs must be a whole number, 1 or more.")?;
            let mem = self.memory.trim().replace(',', ".").parse::<f64>().ok().filter(|v| *v > 0.0).ok_or("Memory must be a number of GB.")?;
            return Ok((cpus, (mem * GB as f64) as i64, t.disk));
        }
        if self.size < 3 {
            let (c, m, k) = self.presets()[self.size];
            return Ok((c, m * GB, k * GB));
        }
        let num = |t: &str| t.trim().replace(',', ".").parse::<f64>().ok().filter(|v| *v > 0.0);
        let cpus = self.cpus.trim().parse::<i64>().ok().filter(|v| *v > 0).ok_or("CPUs must be a whole number, 1 or more.")?;
        let mem = num(&self.memory).ok_or("Memory must be a number of GB.")?;
        let disk = num(&self.disk).ok_or("Disk must be a number of GB.")?;
        Ok((cpus, (mem * GB as f64) as i64, (disk * GB as f64) as i64))
    }

    /// The boot mode the template gives.
    fn template_boot(&self) -> Boot {
        let t = &self.templates[self.template];
        match (t.uefi, t.secure_boot.as_str()) {
            (false, _) => Boot::Bios,
            (true, "true" | "auto") => Boot::UefiSecure,
            (true, _) => Boot::Uefi,
        }
    }

    /// The servers it may run on: the chosen one, or all of them.
    fn can_run(&self) -> Vec<&Server> {
        match self.server {
            Some(i) => vec![&self.servers[i]],
            None => self.servers.iter().collect(),
        }
    }

    /// What stops it being made.
    fn problems(&self) -> Vec<String> {
        let mut stop = vec![];
        if self.from_file {
            let Some(x) = self.xva() else {
                stop.push("Pick the file to import.".into());
                return stop;
            };
            if self.name.trim().is_empty() {
                stop.push("Give it a name.".into());
            }
            match self.chosen_store() {
                None if self.stores.is_empty() => stop.push("There's no storage for disks in this pool.".into()),
                None => stop.push("Pick where its disks go.".into()),
                Some(s) => {
                    // on thin storage what's written on its disks has to fit (all of them, when the
                    // file doesn't say); on LVM each disk takes its whole size
                    let need = if s.thin && x.used > 0 { x.used } else { x.disks.iter().sum() };
                    if need > s.free {
                        stop.push(format!("{} has only {} free; its disks need {}.", s.name, fmt_bytes(s.free), fmt_bytes(need)));
                    }
                }
            }
            let most_cpus = self.can_run().iter().map(|s| s.cpus).max().unwrap_or(0);
            if !x.template && most_cpus > 0 && x.vcpus > most_cpus {
                stop.push(format!("It has {} CPUs, more than the server has ({}).", x.vcpus, most_cpus));
            }
            return stop;
        }
        if let Some(t) = self.own() {
            if self.name.trim().is_empty() {
                stop.push("Give it a name.".into());
            }
            match self.sizes() {
                Err(e) => stop.push(e),
                Ok((cpus, mem, _)) => {
                    let most_cpus = self.can_run().iter().map(|s| s.cpus).max().unwrap_or(0);
                    if most_cpus > 0 && cpus > most_cpus {
                        stop.push(format!("{} CPUs is more than the server has ({}).", cpus, most_cpus));
                    }
                    if mem < t.mem_min {
                        stop.push(format!("The template needs at least {} of memory.", fmt_bytes(t.mem_min)));
                    }
                }
            }
            match self.chosen_store() {
                None => stop.push("Pick where its disks go.".into()),
                Some(s) if s.rref != t.sr && t.disk > s.free => stop.push(format!("{} has only {} free; the disks need {}.", s.name, fmt_bytes(s.free), fmt_bytes(t.disk))),
                _ => {}
            }
            if let Some(why) = self.cloud_init().and_then(|c| c.problem()) {
                stop.push(why.into());
            }
            return stop;
        }
        if self.isos.is_empty() {
            stop.push("There are no ISO files in this pool. Add an ISO library, or upload one to it, first.".into());
        }
        if self.stores.is_empty() {
            stop.push("There's no storage for a disk in this pool.".into());
        } else if self.chosen_store().is_none() {
            let on = self.server.map(|i| self.servers[i].name.clone()).unwrap_or_default();
            stop.push(format!("{} has no storage for disks of its own, and the pool has none shared. Pick another server.", on));
        }
        if self.name.trim().is_empty() {
            stop.push("Give it a name.".into());
        }
        match self.sizes() {
            Err(e) => stop.push(e),
            Ok((cpus, mem, disk)) => {
                if let Some(s) = self.chosen_store().filter(|s| disk > s.free) {
                    stop.push(format!("{} has only {} free; the disk needs {}.", s.name, fmt_bytes(s.free), fmt_bytes(disk)));
                }
                let most_cpus = self.can_run().iter().map(|s| s.cpus).max().unwrap_or(0);
                if most_cpus > 0 && cpus > most_cpus {
                    stop.push(format!("{} CPUs is more than the server has ({}).", cpus, most_cpus));
                }
                let t = &self.templates[self.template];
                if mem < t.mem_min {
                    stop.push(format!("{} needs at least {} of memory.", vm_name_for(Some(t), ""), fmt_bytes(t.mem_min)));
                }
            }
        }
        stop
    }

    /// When the server (or, for any server, none of them) hasn't the memory free to start it.
    fn short_of_memory(&self) -> Option<String> {
        if self.from_file && (!self.start || self.xva().map_or(true, |x| x.template)) {
            return None;
        }
        let (_, mem, _) = self.sizes().ok()?;
        let room = self.can_run().iter().map(|s| s.mem_free).max()?;
        if mem <= room {
            return None;
        }
        let then = if self.from_file { "it can be imported, but won't start until there's room." } else { "it can be created, but won't start until there's room." };
        Some(match self.server {
            Some(i) => format!("{} has {} free and this VM needs {}: {}", self.servers[i].name, fmt_bytes(room), fmt_bytes(mem), then),
            None => format!("No server has {} free ({} at most): {}", fmt_bytes(mem), fmt_bytes(room), then),
        })
    }

    /// What to make, from the choices; None while they don't add up.
    #[cfg(test)]
    fn request(&self) -> Option<NewVm> {
        self.try_request().ok()
    }

    /// What to make, or why the choices don't add up.
    fn try_request(&self) -> Result<NewVm, String> {
        if let Some(t) = self.own() {
            let (vcpus, memory, disk) = self.sizes()?;
            let sockets = if vcpus % self.sockets == 0 { self.sockets } else { 1 };
            let host = self.server.map(|i| &self.servers[i]);
            let store = self.chosen_store().ok_or("Pick where its disks go.")?;
            let net = if self.keep_nics { None } else { self.net.and_then(|i| self.nets.get(i)) };
            let mut names = HashMap::new();
            names.insert(t.rref.clone(), t.name.clone());
            names.insert(store.rref.clone(), store.name.clone());
            if let Some(n) = net {
                names.insert(n.0.clone(), n.1.clone());
            }
            if let Some(h) = host {
                names.insert(h.rref.clone(), h.name.clone());
            }
            return Ok(NewVm {
                name: self.name.trim().to_string(),
                description: self.description.trim().to_string(),
                template: t.rref.clone(),
                vcpus,
                cores_per_socket: vcpus / sockets,
                memory,
                disk,
                sr: store.rref.clone(),
                network: net.map(|n| n.0.clone()),
                host: host.map(|h| h.rref.clone()),
                start: self.start,
                names,
                check: self.to_check().into_iter().map(|(sr, _)| sr).collect(),
                from_template: true,
                copy_to: (store.rref != t.sr).then(|| store.rref.clone()),
                keep_nics: self.keep_nics,
                cloud_init: self.cloud_init(),
                ..Default::default()
            });
        }
        let (vcpus, memory, disk) = self.sizes()?;
        let sockets = if vcpus % self.sockets == 0 { self.sockets } else { 1 };
        let host = self.server.map(|i| &self.servers[i]);
        let iso = self.isos.get(self.iso).ok_or("Pick the ISO to install from.")?;
        if iso.host.is_some() && iso.host.as_deref() != host.map(|h| h.rref.as_str()) {
            return Err("The ISO is in a library only one server sees: pick that server.".into());
        }
        let store = self.chosen_store().ok_or("Pick where its disk goes.")?;
        let template = &self.templates[self.template];
        let net = self.net.and_then(|i| self.nets.get(i));
        let mut names = HashMap::new();
        names.insert(template.rref.clone(), template.name.clone());
        names.insert(iso.vdi.clone(), iso.name.clone());
        names.insert(iso.sr.clone(), iso.lib.clone());
        names.insert(store.rref.clone(), store.name.clone());
        if let Some(n) = net {
            names.insert(n.0.clone(), n.1.clone());
        }
        if let Some(h) = host {
            names.insert(h.rref.clone(), h.name.clone());
        }
        Ok(NewVm {
            cloud_init: None,
            name: self.name.trim().to_string(),
            description: self.description.trim().to_string(),
            template: template.rref.clone(),
            iso: iso.vdi.clone(),
            vcpus,
            cores_per_socket: vcpus / sockets,
            memory,
            disk,
            sr: store.rref.clone(),
            network: net.map(|n| n.0.clone()),
            host: host.map(|h| h.rref.clone()),
            boot: self.boot.filter(|b| *b != self.template_boot()),
            start: self.start,
            names,
            check: self.to_check().into_iter().map(|(sr, _)| sr).collect(),
            from_template: false,
            copy_to: None,
            keep_nics: false,
        })
    }

    pub fn ui(&mut self, ctx: &egui::Context, pal: &Pal) -> NewVmAction {
        let mut action = NewVmAction::None;
        let busy = self.busy.is_some();
        let title = if self.pool.is_empty() { "New VM".to_string() } else { format!("New VM in {}", self.pool) };
        let made = self.not_started.is_some();
        // the server turned it down: why, and what it was asked
        let refused = !busy && self.error.is_some() && self.trace.is_some();
        // (the form scrolls in a window too short for it all, as with cloud-init's part open;
        // what's under it, and the buttons, stay in view)
        let room = (ctx.screen_rect().height() - 230.0).max(200.0);
        dialog_window(ctx, &title).id(egui::Id::new("new-vm")).show(ctx, |ui| {
            ui.set_width(WIDTH);
            ui.spacing_mut().item_spacing.y = ROW_GAP;
            // while it's made, and after: what it is instead of the form
            if busy || made || refused {
                self.summary(ui, pal);
            } else {
                // (its bar beside the fields, not over them)
                ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                egui::ScrollArea::vertical().max_height(room).auto_shrink([false, true]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = ROW_GAP;
                    self.form(ui, pal);
                });
            }
            let stop = self.problems();
            if let Some(why) = &self.not_started {
                alert(ui, pal, false, &format!("{} was created, but it didn't start.", self.name.trim()), why, None);
            } else if refused {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.label(egui::RichText::new(format!("Couldn't create {}.", self.name.trim())).font(FontId::new(14.0, FontFamily::Name("bold".into()))).color(pal.red));
                    ui.add(egui::Label::new(egui::RichText::new(self.error.as_deref().unwrap_or_default()).color(pal.ink_soft)).wrap());
                });
            } else if !busy {
                // the storage chosen: offline in red, with Try again; a check that's slow, said
                let mut again = None;
                for (sr, host) in self.to_check() {
                    match self.checks.get(&(sr.clone(), host.clone())) {
                        Some(Check::Failed(e)) => {
                            let (title, text) = self.check_problem(&sr, host.as_deref(), e);
                            if alert(ui, pal, true, &title, &text, Some(("retry", "Try again"))) {
                                again = Some((sr, host));
                            }
                        }
                        Some(Check::Running(since)) => {
                            if since.elapsed() > Duration::from_millis(600) {
                                let name = self.sr_place(&sr).0;
                                ui.horizontal(|ui| {
                                    ui.add(egui::Spinner::new().size(12.0));
                                    ui.label(egui::RichText::new(format!("Checking that {} answers…", name)).size(12.5).color(pal.ink_soft));
                                });
                            }
                            ctx.request_repaint_after(Duration::from_millis(200));
                        }
                        _ => {}
                    }
                }
                if let Some(k) = again {
                    self.checks.remove(&k);
                }
                if let Some(short) = self.short_of_memory() {
                    alert(ui, pal, false, "Not enough memory.", &short, None);
                }
                for p in stop.iter().filter(|p| !p.starts_with("Give it a name") && !p.starts_with("Pick the file")) {
                    ui.add(egui::Label::new(egui::RichText::new(p.as_str()).size(12.5).color(pal.red)).wrap());
                }
                if !self.from_file {
                    crate::app::tick(ui, pal, &mut self.start, "Start it and open the console");
                } else if self.xva().map_or(false, |x| !x.template) {
                    crate::app::tick(ui, pal, &mut self.start, "Start it once it's imported");
                }
            }
            if self.busy.is_some() {
                self.progress_part(ui, pal);
                ctx.request_repaint_after(Duration::from_millis(50));
            }
            if let (Some(e), false) = (&self.error, refused) {
                ui.add(egui::Label::new(egui::RichText::new(e.as_str()).color(pal.red)).wrap());
            }
            if let (Some(tr), true) = (&self.trace, busy || made || refused) {
                transcript(ui, pal, tr, &mut self.copied, busy, None);
            }
            dialog_buttons(ui, |ui| {
                if made {
                    if dialog_button(ui, pal, "check", "Close", Look::Primary, true) {
                        action = NewVmAction::Close;
                    }
                    return;
                }
                if refused {
                    if dialog_button(ui, pal, "close", "Cancel", Look::Plain, true) {
                        action = NewVmAction::Close;
                    }
                    if dialog_button(ui, pal, "retry", "Try again", Look::Primary, true) {
                        match self.try_request() {
                            Ok(spec) => action = NewVmAction::Create(self.sid, spec),
                            Err(why) => self.error = Some(why),
                        }
                    }
                    if dialog_button(ui, pal, "back", "Back", Look::Plain, true) {
                        // to the form, to change something
                        (self.error, self.trace) = (None, None);
                    }
                    return;
                }
                if self.made_vm.is_some() {
                    // made, and starting: closing leaves it starting
                    if dialog_button(ui, pal, "close", "Close", Look::Plain, true) {
                        action = NewVmAction::Close;
                    }
                    return;
                }
                // while it's being made there's nothing to cancel: the window closes, the making
                // goes on (under Activities)
                if dialog_button(ui, pal, "close", if busy { "Close" } else { "Cancel" }, Look::Plain, true) {
                    action = NewVmAction::Close;
                }
                let press = std::mem::take(&mut self.press);
                let ready = !busy && stop.is_empty() && self.checks_passed();
                if self.from_file {
                    if dialog_button(ui, pal, "upload", "Import", Look::Primary, ready) || (press && ready) {
                        match self.try_import() {
                            Ok(imp) => action = NewVmAction::Import(self.sid, imp),
                            Err(why) => self.error = Some(why),
                        }
                    }
                    return;
                }
                if dialog_button(ui, pal, "plus", "Create", Look::Primary, ready) || press {
                    match self.try_request() {
                        Ok(spec) => action = NewVmAction::Create(self.sid, spec),
                        Err(why) => self.error = Some(why),
                    }
                }
            });
        });
        if std::mem::take(&mut self.press_pick) {
            action = NewVmAction::PickFile;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            action = NewVmAction::Close;
        }
        action
    }

    /// What's being made, in two lines: its name, then its size and where it goes.
    fn summary(&self, ui: &mut Ui, pal: &Pal) {
        let Ok(spec) = self.try_request() else { return };
        let mut parts = vec![
            format!("{} CPU{}", spec.vcpus, if spec.vcpus == 1 { "" } else { "s" }),
            format!("{} memory", fmt_bytes(spec.memory)),
            format!("{} disk on {}", fmt_bytes(spec.disk), self.chosen_store().map(|s| s.name.as_str()).unwrap_or_default()),
        ];
        parts.push(self.server.map(|i| self.servers[i].name.clone()).unwrap_or_else(|| "any server".into()));
        let net = match self.own() {
            Some(t) if self.keep_nics => if t.nets.is_empty() { "no network".to_string() } else { t.nets.join(", ") },
            _ => self.net.and_then(|i| self.nets.get(i)).map(|n| n.1.clone()).unwrap_or_else(|| "no network".into()),
        };
        parts.push(net);
        egui::Frame::none().fill(pal.rail).stroke(Stroke::new(1.0_f32, pal.border)).rounding(Rounding::same(8.0)).inner_margin(egui::Margin::symmetric(12.0, 10.0)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.label(egui::RichText::new(spec.name.as_str()).font(FontId::new(14.0, FontFamily::Name("bold".into()))).color(pal.ink));
            let what = if self.own().map_or(false, |t| t.of.is_some()) { "snapshot" } else { "template" };
            let from = match self.own() {
                Some(t) if spec.copy_to.is_some() => format!("Made from the {} {}, its disks copied in full", what, t.name),
                Some(t) => format!("Made from the {} {}, in an instant", what, t.name),
                None => format!("Installs from {}", self.isos.get(self.iso).map(|i| i.name.as_str()).unwrap_or_default()),
            };
            ui.add(egui::Label::new(egui::RichText::new(parts.join("  ·  ")).size(12.5).color(pal.ink_soft)).wrap());
            ui.add(egui::Label::new(egui::RichText::new(from).size(12.5).color(pal.ink_soft)).truncate());
        });
    }

    fn form(&mut self, ui: &mut Ui, pal: &Pal) {
        combo_style(ui, pal);
        // from an ISO, from one of the user's templates (when there are any), or a file
        ui.horizontal(|ui| {
            field_label(ui, "Create from:", LABEL_WIDTH);
            let options: &[&str] = if self.own.is_empty() { &["An ISO", "A file"] } else { &["An ISO", "A template", "A file"] };
            let file_at = options.len() - 1;
            let mut i = if self.from_file { file_at } else { self.from_own as usize };
            if crate::isolib::segmented(ui, pal, options, &mut i, ui.available_width()) {
                self.name_typed = false;
                self.error = None;
                if i == file_at {
                    self.switch_to_file();
                } else {
                    if self.from_file {
                        self.start = true;
                    }
                    self.from_file = false;
                    self.from_own = i == 1;
                    self.size = if self.from_own { 0 } else { 1 };
                    if self.from_own {
                        self.own_changed();
                    } else {
                        self.keep_nics = true;
                        self.server = self.first_server();
                        self.server_changed();
                        self.iso_changed();
                    }
                }
            }
        });
        if self.from_file {
            self.file_form(ui, pal);
            return;
        }
        if self.from_own {
            ui.horizontal(|ui| {
                field_label(ui, if self.own().map_or(false, |t| t.of.is_some()) { "Snapshot:" } else { "Template:" }, LABEL_WIDTH);
                let label = |t: &Own| if t.os.is_empty() { t.name.clone() } else { format!("{}   ·  {}", t.name, t.os) };
                let shown = self.own.get(self.own_pick).map(label).unwrap_or_default();
                let mut picked = self.own_pick;
                combo("newvm-own", shown, ui.available_width()).height(320.0).show_ui(ui, |ui| {
                    for (i, t) in self.own.iter().enumerate() {
                        ui.selectable_value(&mut picked, i, label(t));
                    }
                });
                if picked != self.own_pick {
                    self.own_pick = picked;
                    self.own_changed();
                }
            });
        }
        // Install from (an ISO)
        if !self.from_own {
            ui.horizontal(|ui| {
            field_label(ui, "Install from:", LABEL_WIDTH);
            let shown = self.isos.get(self.iso).map(|i| i.name.clone()).unwrap_or_else(|| "No ISO files".into());
            let libs = self.isos.iter().map(|i| &i.lib).collect::<std::collections::HashSet<_>>().len();
            let mut picked = self.iso;
            combo("newvm-iso", shown, ui.available_width()).height(320.0).show_ui(ui, |ui| {
                for (i, iso) in self.isos.iter().enumerate() {
                    let text = if libs > 1 { format!("{}   ·  {}", iso.name, iso.lib) } else { iso.name.clone() };
                    ui.selectable_value(&mut picked, i, text);
                }
            });
            if picked != self.iso {
                self.iso = picked;
                self.iso_changed();
            }
            });
        }
        // Name
        ui.horizontal(|ui| {
            field_label(ui, "Name:", LABEL_WIDTH);
            if text_field(ui, pal, &mut self.name, ui.available_width(), false).changed() {
                self.name_typed = true;
            }
        });
        // Size
        ui.horizontal(|ui| {
            let (h, title_y) = card_metrics(ui);
            // the label on the cards' titles
            ui.allocate_ui_with_layout(vec2(LABEL_WIDTH, h), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.add_space(title_y - FIELD_HEIGHT / 2.0);
                field_label(ui, "Size:", LABEL_WIDTH);
            });
            let gap = 8.0;
            if let Some(t) = self.own() {
                // as the template, or other CPUs and memory (its disks come as they are)
                let w = (ui.available_width() - gap) / 2.0;
                ui.spacing_mut().item_spacing.x = gap;
                let (c, m, k) = (t.vcpus, t.memory, t.disk);
                let lines = vec![format!("{} CPU{}", c, if c == 1 { "" } else { "s" }), format!("{} memory", fmt_bytes(m)), format!("{} disk", fmt_bytes(k))];
                let custom = vec!["Other CPUs".to_string(), "and memory".to_string()];
                let same = if t.of.is_some() { "As the VM" } else { "As the template" };
                for (i, title, lines) in [(0usize, same, lines), (3usize, "Custom", custom)] {
                    if size_card(ui, pal, vec2(w, h), title, &lines, self.size == i).clicked() && self.size != i {
                        if i == 3 {
                            (self.cpus, self.memory) = (c.to_string(), format!("{}", (m as f64 / GB as f64 * 10.0).round() / 10.0));
                        }
                        self.size = i;
                    }
                }
                return;
            }
            let presets = self.presets();
            let w = (ui.available_width() - 3.0 * gap) / 4.0;
            ui.spacing_mut().item_spacing.x = gap;
            for (i, title) in ["Small", "Medium", "Large", "Custom"].iter().enumerate() {
                let lines: Vec<String> = if i < 3 {
                    let (c, m, k) = presets[i];
                    vec![format!("{} CPU{}", c, if c == 1 { "" } else { "s" }), format!("{} GB memory", m), format!("{} GB disk", k)]
                } else {
                    vec!["Your own".into(), "numbers".into()]
                };
                if size_card(ui, pal, vec2(w, h), title, &lines, self.size == i).clicked() && self.size != i {
                    if i == 3 {
                        let (c, m, k) = presets[self.size.min(2)];
                        (self.cpus, self.memory, self.disk) = (c.to_string(), m.to_string(), k.to_string());
                    }
                    self.size = i;
                }
            }
        });
        if self.size == 3 {
            self.custom_sizes(ui, pal);
        }
        // Server, then the storage it sees
        ui.horizontal(|ui| {
            field_label(ui, "Server:", LABEL_WIDTH);
            let label = |s: &Server| format!("{}  ·  {} memory free", s.name, fmt_bytes(s.mem_free));
            let shown = self.server.map(|i| label(&self.servers[i])).unwrap_or_else(|| "Any server".into());
            let iso_host = self.iso_host();
            let only = iso_host.as_deref().map(|h| format!("The ISO is in a library only {} sees.", self.servers.iter().find(|s| s.rref == h).map(|s| s.name.as_str()).unwrap_or("one server")));
            let mut picked = self.server;
            combo("newvm-server", shown, ui.available_width()).show_ui(ui, |ui| {
                if self.stores.iter().any(|s| s.host.is_none()) {
                    let r = ui.add_enabled_ui(only.is_none(), |ui| ui.selectable_value(&mut picked, None, "Any server")).inner;
                    if let Some(why) = &only {
                        r.on_disabled_hover_text(why.as_str());
                    }
                }
                for (i, s) in self.servers.iter().enumerate() {
                    let ok = iso_host.as_ref().map_or(true, |h| *h == s.rref);
                    let r = ui.add_enabled_ui(ok, |ui| ui.selectable_value(&mut picked, Some(i), label(s))).inner;
                    if let Some(why) = &only {
                        r.on_disabled_hover_text(why.as_str());
                    }
                }
            });
            if picked != self.server {
                self.server = picked;
                self.server_changed();
            }
        });
        ui.horizontal(|ui| {
            field_label(ui, if self.from_own { "Disks on:" } else { "Disk on:" }, LABEL_WIDTH);
            let tpl_sr = self.own().map(|t| t.sr.clone());
            let what = if self.own().map_or(false, |t| t.of.is_some()) { "snapshot" } else { "template" };
            let label = |s: &Store| match &tpl_sr {
                Some(r) if *r == s.rref => format!("{}  ·  with the {}, an instant copy", s.name, what),
                Some(_) => format!("{}  ·  {} free, a full copy", s.name, fmt_bytes(s.free)),
                None => format!("{}  ·  {} free", s.name, fmt_bytes(s.free)),
            };
            let shown = self.chosen_store().map(label).unwrap_or_else(|| "No storage".into());
            let mut picked = self.store;
            combo("newvm-sr", shown, ui.available_width()).show_ui(ui, |ui| {
                for (i, s) in self.stores.iter().enumerate().filter(|(_, s)| self.usable(s)) {
                    ui.selectable_value(&mut picked, i, label(s));
                }
            });
            self.store = picked;
        });
        // Network
        ui.horizontal(|ui| {
            field_label(ui, "Network:", LABEL_WIDTH);
            let as_template = self.own().map(|t| {
                let what = if t.of.is_some() { "VM" } else { "template" };
                if t.nets.is_empty() { format!("As in the {} (none)", what) } else { format!("As in the {} ({})", what, t.nets.join(", ")) }
            });
            let shown = match (&as_template, self.keep_nics) {
                (Some(a), true) => a.clone(),
                _ => self.net.and_then(|i| self.nets.get(i)).map(|n| n.1.clone()).unwrap_or_else(|| "No network".into()),
            };
            // (None, keep) as the template; (Some(i), _) that network; (None, no keep) none
            let mut choice = if as_template.is_some() && self.keep_nics { (None, true) } else { (self.net, false) };
            combo("newvm-net", shown, ui.available_width()).show_ui(ui, |ui| {
                if let Some(a) = &as_template {
                    ui.selectable_value(&mut choice, (None, true), a.as_str());
                    ui.separator();
                }
                for (i, n) in self.nets.iter().enumerate() {
                    ui.selectable_value(&mut choice, (Some(i), false), n.1.as_str());
                }
                ui.separator();
                ui.selectable_value(&mut choice, (None, false), "No network");
            });
            self.keep_nics = choice.1 && as_template.is_some();
            if !self.keep_nics {
                self.net = choice.0;
            }
        });
        self.advanced_part(ui, pal);
        self.cloud_init_part(ui, pal);
    }

    /// What cloud-init is told, when it's used.
    fn cloud_init(&self) -> Option<crate::cloudinit::CloudInit> {
        if !self.ci_on || self.own().is_none() {
            return None;
        }
        let mut c = self.ci.clone();
        if !self.ci_host_typed {
            c.hostname = crate::cloudinit::hostname_for(&self.name);
        }
        c.custom = self.ci_custom.then(|| self.ci_text.clone());
        Some(c)
    }

    /// cloud-init, folded like Advanced: on or off, its hostname, a user (with a password, SSH
    /// keys or both), or user-data typed by hand.
    fn cloud_init_part(&mut self, ui: &mut Ui, pal: &Pal) {
        if self.own().is_none() {
            return;
        }
        let summary = match self.cloud_init() {
            None => "Off".to_string(),
            Some(c) if c.custom.is_some() => "Your own user-data".to_string(),
            Some(c) if c.user.trim().is_empty() => c.hostname.clone(),
            Some(c) => format!("{}  ·  user {}", c.hostname, c.user.trim()),
        };
        let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(r, Rounding::same(5.0), pal.hover);
        }
        let c = pos2(r.min.x + 10.0, r.center().y);
        let chev = if self.ci_open { vec![c + vec2(-3.5, -1.5), c + vec2(0.0, 2.0), c + vec2(3.5, -1.5)] } else { vec![c + vec2(-1.5, -3.5), c + vec2(2.0, 0.0), c + vec2(-1.5, 3.5)] };
        ui.painter().add(egui::Shape::line(chev, Stroke::new(1.4_f32, pal.ink_soft)));
        ui.painter().text(pos2(r.min.x + 22.0, r.center().y), Align2::LEFT_CENTER, "Cloud-init", FontId::proportional(13.0), pal.ink);
        ui.painter().text(pos2(r.max.x - 8.0, r.center().y), Align2::RIGHT_CENTER, summary, FontId::proportional(12.0), if self.ci_on { pal.ink_soft } else { pal.ink_faint });
        if resp.clicked() {
            self.ci_open = !self.ci_open;
            if self.ci_open {
                // (brought into view, in a short window)
                resp.scroll_to_me(Some(egui::Align::TOP));
            }
        }
        if !self.ci_open {
            return;
        }
        // off; a hostname and a user (from the fields); or user-data typed by hand
        ui.horizontal(|ui| {
            field_label(ui, "Set up:", LABEL_WIDTH);
            let mut mode = if !self.ci_on { 0 } else if self.ci_custom { 2 } else { 1 };
            if crate::isolib::segmented(ui, pal, &["Off", "Hostname and user", "My own user-data"], &mut mode, ui.available_width()) {
                if mode == 2 && self.ci_text.trim().is_empty() {
                    // started from what the fields say
                    let mut c = self.ci.clone();
                    if !self.ci_host_typed {
                        c.hostname = crate::cloudinit::hostname_for(&self.name);
                    }
                    c.custom = None;
                    self.ci_text = c.user_data();
                }
                (self.ci_on, self.ci_custom) = (mode > 0, mode == 2);
            }
        });
        let note = |ui: &mut Ui, text: &str| crate::isolib::note(ui, pal, text);
        if !self.ci_on {
            note(ui, "For systems with cloud-init in them (cloud images, or a template made from a VM that has it): it sets their hostname and a user to log in with.");
            return;
        }
        let hint = |ui: &Ui, r: &egui::Response, empty: bool, text: &str| {
            if empty && !r.has_focus() {
                ui.painter().text(r.rect.left_center() + vec2(8.0, 0.0), Align2::LEFT_CENTER, text, FontId::proportional(13.0), pal.ink_faint);
            }
        };
        if self.ci_custom {
            ui.horizontal_top(|ui| {
                field_label(ui, "User-data:", LABEL_WIDTH);
                ui.add(egui::TextEdit::multiline(&mut self.ci_text).desired_rows(8).desired_width(ui.available_width()).font(egui::TextStyle::Monospace).code_editor());
            });
        } else {
            ui.horizontal(|ui| {
                field_label(ui, "Hostname:", LABEL_WIDTH);
                if !self.ci_host_typed {
                    self.ci.hostname = crate::cloudinit::hostname_for(&self.name);
                }
                if text_field(ui, pal, &mut self.ci.hostname, ui.available_width(), false).changed() {
                    self.ci_host_typed = true;
                }
            });
            ui.horizontal(|ui| {
                field_label(ui, "User:", LABEL_WIDTH);
                let r = text_field(ui, pal, &mut self.ci.user, ui.available_width(), false);
                hint(ui, &r, self.ci.user.is_empty(), "to log in with (it gets sudo); none: only the hostname");
            });
            if !self.ci.user.trim().is_empty() {
                ui.horizontal(|ui| {
                    field_label(ui, "Password:", LABEL_WIDTH);
                    let r = text_field(ui, pal, &mut self.ci.password, ui.available_width(), true);
                    hint(ui, &r, self.ci.password.is_empty(), "none: an SSH key only");
                });
                ui.horizontal_top(|ui| {
                    field_label(ui, "SSH keys:", LABEL_WIDTH);
                    ui.add(egui::TextEdit::multiline(&mut self.ci.keys).desired_rows(2).desired_width(ui.available_width()).font(egui::TextStyle::Monospace).hint_text("ssh-ed25519 AAAA… you@computer  (one a line)"));
                });
            }
        }
        note(ui, "It goes on a 4 MB disk, 'cloud-init config'. Delete that disk once the VM is set up: it holds the password.");
        if let Ok(mut last) = CI_LAST.lock() {
            (last.user, last.keys) = (self.ci.user.clone(), self.ci.keys.clone());
        }
    }

    /// From a file: the file, what it holds, then its name, server, storage and network.
    fn file_form(&mut self, ui: &mut Ui, pal: &Pal) {
        ui.horizontal(|ui| {
            field_label(ui, "File:", LABEL_WIDTH);
            let w = ui.available_width() - 108.0;
            let (r, resp) = ui.allocate_exact_size(vec2(w, FIELD_HEIGHT), Sense::click());
            ui.painter().rect(r, Rounding::same(5.0), pal.input, Stroke::new(1.0_f32, pal.border_strong));
            let shown = self.file.as_ref().map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
            let (text, ink) = match &shown {
                Some(n) => (n.clone(), pal.ink),
                None => ("No file chosen: a .xva, exported here or from XenCenter".to_string(), pal.ink_faint),
            };
            let clip = r.shrink2(vec2(8.0, 0.0));
            let g = ui.painter().layout_no_wrap(text, FontId::proportional(13.0), ink);
            ui.painter().with_clip_rect(clip).galley(pos2(clip.min.x, r.center().y - g.size().y / 2.0), g, ink);
            if let Some(p) = &self.file {
                resp.clone().on_hover_text(p.display().to_string());
            }
            let browse = dialog_button(ui, pal, "folder", "Browse…", Look::Plain, true);
            if browse || resp.clicked() {
                self.press_pick = true;
            }
        });
        match &self.file_info {
            Some(Ok(x)) => {
                let x = x.clone();
                ui.horizontal(|ui| {
                    ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x);
                    let w = ui.available_width();
                    egui::Frame::none().fill(pal.rail).stroke(Stroke::new(1.0_f32, pal.border)).rounding(Rounding::same(8.0)).inner_margin(egui::Margin::symmetric(12.0, 9.0)).show(ui, |ui| {
                        ui.set_width(w - 24.0);
                        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        let title = if x.template { format!("{}  ·  a template", x.name) } else { x.name.clone() };
                        ui.add(egui::Label::new(egui::RichText::new(title).font(FontId::new(13.5, FontFamily::Name("bold".into()))).color(pal.ink)).truncate());
                        ui.add(egui::Label::new(egui::RichText::new(crate::xva::summary(&x)).size(12.5).color(pal.ink_soft)).wrap());
                        let mut more = vec![format!("{} file", fmt_bytes(x.size as i64))];
                        if x.used > 0 {
                            more.push(format!("{} written on its disks", fmt_bytes(x.used)));
                        }
                        if let Some(c) = x.compressed {
                            more.push(format!("compressed ({})", c));
                        }
                        if x.template {
                            more.push("it comes in as a template".into());
                        }
                        ui.add(egui::Label::new(egui::RichText::new(more.join("  ·  ")).size(12.0).color(pal.ink_faint)).wrap());
                        });
                    });
                });
            }
            Some(Err(why)) => {
                let why = why.clone();
                alert(ui, pal, true, "This file can't be imported.", &why, None);
            }
            None => {}
        }
        if self.xva().is_none() {
            return;
        }
        ui.horizontal(|ui| {
            field_label(ui, "Name:", LABEL_WIDTH);
            if text_field(ui, pal, &mut self.name, ui.available_width(), false).changed() {
                self.name_typed = true;
            }
        });
        ui.horizontal(|ui| {
            field_label(ui, "Server:", LABEL_WIDTH);
            let label = |s: &Server| format!("{}  ·  {} memory free", s.name, fmt_bytes(s.mem_free));
            let shown = self.server.map(|i| label(&self.servers[i])).unwrap_or_else(|| "Any server".into());
            let mut picked = self.server;
            combo("import-server", shown, ui.available_width()).show_ui(ui, |ui| {
                if self.stores.iter().any(|s| s.host.is_none()) {
                    ui.selectable_value(&mut picked, None, "Any server");
                }
                for (i, s) in self.servers.iter().enumerate() {
                    ui.selectable_value(&mut picked, Some(i), label(s));
                }
            });
            if picked != self.server {
                self.server = picked;
                self.server_changed();
            }
        });
        ui.horizontal(|ui| {
            field_label(ui, "Disks on:", LABEL_WIDTH);
            let label = |s: &Store| format!("{}  ·  {} free", s.name, fmt_bytes(s.free));
            let shown = self.chosen_store().map(label).unwrap_or_else(|| "No storage".into());
            let mut picked = self.store;
            combo("import-sr", shown, ui.available_width()).show_ui(ui, |ui| {
                for (i, s) in self.stores.iter().enumerate().filter(|(_, s)| self.usable(s)) {
                    ui.selectable_value(&mut picked, i, label(s));
                }
            });
            self.store = picked;
        });
        let file_nets = self.xva().map(|x| x.nets.clone()).unwrap_or_default();
        if !file_nets.is_empty() {
            ui.horizontal(|ui| {
                field_label(ui, "Network:", LABEL_WIDTH);
                let mut uniq = file_nets.clone();
                uniq.dedup();
                let as_file = format!("As in the file ({})", uniq.join(", "));
                let shown = if self.keep_nics { as_file.clone() } else { self.net.and_then(|i| self.nets.get(i)).map(|n| n.1.clone()).unwrap_or_default() };
                let mut choice = if self.keep_nics { None } else { self.net };
                combo("import-net", shown, ui.available_width()).show_ui(ui, |ui| {
                    ui.selectable_value(&mut choice, None, as_file.as_str());
                    ui.separator();
                    for (i, n) in self.nets.iter().enumerate() {
                        ui.selectable_value(&mut choice, Some(i), n.1.as_str());
                    }
                });
                self.keep_nics = choice.is_none();
                if choice.is_some() {
                    self.net = choice;
                }
            });
            if self.keep_nics {
                let known: Vec<&String> = file_nets.iter().filter(|n| !self.nets.iter().any(|m| m.1 == **n)).collect();
                if !known.is_empty() {
                    let names: Vec<String> = known.iter().map(|n| format!("'{}'", n)).collect();
                    note(ui, pal, &format!("This pool has no network called {}: the server connects those cards as it sees fit. Pick a network to be sure.", names.join(" or ")));
                }
            }
        }
    }

    /// Custom's CPUs, memory and disk, centred in a panel under the cards.
    fn custom_sizes(&mut self, ui: &mut Ui, pal: &Pal) {
        ui.horizontal(|ui| {
            ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x);
            let w = ui.available_width();
            let frame = egui::Frame::none().fill(pal.rail).stroke(Stroke::new(1.0_f32, pal.border)).rounding(Rounding::same(8.0)).inner_margin(egui::Margin::symmetric(0.0, 11.0));
            frame.show(ui, |ui| {
                ui.set_width(w);
                // centred by the width the row took last time
                let id = ui.id().with("custom-width");
                let before: f32 = ui.data(|d| d.get_temp(id)).unwrap_or(w);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.add_space(((w - before) / 2.0).max(0.0));
                    let start = ui.cursor().min.x;
                    let mut end = start;
                    let fields = [("CPUs", &mut self.cpus, ""), ("Memory", &mut self.memory, "GB"), ("Disk", &mut self.disk, "GB")];
                    let shown = if self.from_own { 2 } else { 3 };
                    for (i, (label, value, unit)) in fields.into_iter().take(shown).enumerate() {
                        if i > 0 {
                            ui.add_space(18.0);
                        }
                        ui.label(label);
                        end = text_field(ui, pal, value, 52.0, false).rect.max.x;
                        if !unit.is_empty() {
                            end = ui.label(egui::RichText::new(unit).color(pal.ink_soft)).rect.max.x;
                        }
                    }
                    let took = end - start;
                    if (took - before).abs() > 0.5 {
                        ui.data_mut(|d| d.insert_temp(id, took));
                        ui.ctx().request_repaint();
                    }
                });
            });
        });
    }

    fn advanced_part(&mut self, ui: &mut Ui, pal: &Pal) {
        if self.from_own {
            // from a template of the user's: its system and boot mode come with it
            let cpus = self.sizes().map(|s| s.0).unwrap_or(1);
            if cpus % self.sockets != 0 {
                self.sockets = 1;
            }
            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(r, Rounding::same(5.0), pal.hover);
            }
            let c = pos2(r.min.x + 10.0, r.center().y);
            let chev = if self.advanced { vec![c + vec2(-3.5, -1.5), c + vec2(0.0, 2.0), c + vec2(3.5, -1.5)] } else { vec![c + vec2(-1.5, -3.5), c + vec2(2.0, 0.0), c + vec2(-1.5, 3.5)] };
            ui.painter().add(egui::Shape::line(chev, Stroke::new(1.4_f32, pal.ink_soft)));
            ui.painter().text(pos2(r.min.x + 22.0, r.center().y), Align2::LEFT_CENTER, "Advanced", FontId::proportional(13.0), pal.ink);
            ui.painter().text(pos2(r.max.x - 8.0, r.center().y), Align2::RIGHT_CENTER, sockets_text(self.sockets, cpus), FontId::proportional(12.0), pal.ink_soft);
            if resp.clicked() {
                self.advanced = !self.advanced;
            }
            if self.advanced {
                ui.horizontal(|ui| {
                    field_label(ui, "CPU layout:", LABEL_WIDTH);
                    combo("newvm-sockets", sockets_text(self.sockets, cpus), ui.available_width()).show_ui(ui, |ui| {
                        for s in (1..=cpus).filter(|s| cpus % s == 0) {
                            ui.selectable_value(&mut self.sockets, s, sockets_text(s, cpus));
                        }
                    });
                });
                ui.horizontal(|ui| {
                    field_label(ui, "Description:", LABEL_WIDTH);
                    text_field(ui, pal, &mut self.description, ui.available_width(), false);
                });
            }
            return;
        }
        let t = self.templates[self.template].clone();
        let boot = self.boot.unwrap_or(self.template_boot());
        let cpus = self.sizes().map(|s| s.0).unwrap_or(1);
        if cpus % self.sockets != 0 {
            self.sockets = 1;
        }
        let os = if t.name == OTHER_MEDIA { "Other system".to_string() } else { vm_name_for(Some(&t), "") };
        let summary = format!("{}  ·  {}  ·  {}", os, boot_name(boot), sockets_text(self.sockets, cpus));
        let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(r, Rounding::same(5.0), pal.hover);
        }
        let c = pos2(r.min.x + 10.0, r.center().y);
        let chev = if self.advanced { vec![c + vec2(-3.5, -1.5), c + vec2(0.0, 2.0), c + vec2(3.5, -1.5)] } else { vec![c + vec2(-1.5, -3.5), c + vec2(2.0, 0.0), c + vec2(-1.5, 3.5)] };
        ui.painter().add(egui::Shape::line(chev, Stroke::new(1.4_f32, pal.ink_soft)));
        ui.painter().text(pos2(r.min.x + 22.0, r.center().y), Align2::LEFT_CENTER, "Advanced", FontId::proportional(13.0), pal.ink);
        ui.painter().text(pos2(r.max.x - 8.0, r.center().y), Align2::RIGHT_CENTER, summary, FontId::proportional(12.0), pal.ink_soft);
        if resp.clicked() {
            self.advanced = !self.advanced;
        }
        if !self.advanced {
            return;
        }
        // the system (its template)
        ui.horizontal(|ui| {
            field_label(ui, "System:", LABEL_WIDTH);
            let label = |i: usize, t: &TemplateInfo| if i == self.detected && t.name != OTHER_MEDIA { format!("{}  (detected)", t.name) } else { t.name.clone() };
            let mut picked = self.template;
            combo("newvm-template", label(self.template, &t), ui.available_width()).height(320.0).show_ui(ui, |ui| {
                for (i, x) in self.templates.iter().enumerate() {
                    ui.selectable_value(&mut picked, i, label(i, x));
                }
            });
            if picked != self.template {
                self.template_picked = true;
                self.template_changed(picked);
                if !self.name_typed {
                    let iso = self.isos.get(self.iso).map(|i| i.name.clone()).unwrap_or_default();
                    self.name = unique_name(&vm_name_for(Some(&self.templates[picked]), &iso), &self.taken);
                }
            }
        });
        // boot mode: what the template allows
        ui.horizontal(|ui| {
            field_label(ui, "Boot:", LABEL_WIDTH);
            let allowed = [!t.vtpm, t.supports_uefi || t.uefi, t.supports_secure_boot || t.secure_boot == "true"];
            let modes = [Boot::Bios, Boot::Uefi, Boot::UefiSecure];
            let w = (ui.available_width() - 4.0) / 3.0;
            let (bar, _) = ui.allocate_exact_size(vec2(w * 3.0, 26.0), Sense::hover());
            ui.painter().rect_filled(bar, Rounding::same(6.0), pal.rail);
            for (i, m) in modes.iter().enumerate() {
                let pr = Rect::from_min_size(pos2(bar.min.x + i as f32 * w, bar.min.y), vec2(w, 26.0)).shrink(2.5);
                let resp = ui.interact(pr, ui.id().with(("boot", i)), if allowed[i] { Sense::click() } else { Sense::hover() });
                let on = boot == *m;
                if on {
                    ui.painter().rect(pr, Rounding::same(4.5), pal.surface, Stroke::new(1.0_f32, pal.border));
                }
                let col = if !allowed[i] { pal.ink_faint } else if on || resp.hovered() { pal.ink } else { pal.ink_soft };
                ui.painter().text(pr.center(), Align2::CENTER_CENTER, boot_name(*m), FontId::proportional(12.5), col);
                let resp = if allowed[i] { resp } else { resp.on_hover_text(if i == 0 { "This system needs UEFI (it has a virtual TPM)" } else { "This system's template doesn't offer it" }) };
                if resp.clicked() {
                    self.boot = Some(*m);
                }
            }
        });
        if t.vtpm {
            note(ui, pal, "Comes with a virtual TPM, which Windows 11 needs.");
        }
        // CPU layout
        ui.horizontal(|ui| {
            field_label(ui, "CPU layout:", LABEL_WIDTH);
            combo("newvm-sockets", sockets_text(self.sockets, cpus), ui.available_width()).show_ui(ui, |ui| {
                for s in (1..=cpus).filter(|s| cpus % s == 0) {
                    ui.selectable_value(&mut self.sockets, s, sockets_text(s, cpus));
                }
            });
        });
        note(ui, pal, "How the CPUs look to the system. One socket suits nearly everything: Windows 10 and 11 use at most two sockets (Home, one).");
        ui.horizontal(|ui| {
            field_label(ui, "Description:", LABEL_WIDTH);
            text_field(ui, pal, &mut self.description, ui.available_width(), false);
        });
    }
}

/// A drop-down list as wide as `width`, a text too long for it cut short with "…" (a long ISO
/// name would otherwise widen the dialog).
pub(crate) fn combo(id: &str, shown: String, width: f32) -> egui::ComboBox {
    egui::ComboBox::from_id_salt(id).selected_text(shown).width(width).truncate()
}

/// Drop-down lists drawn like the text boxes: as tall, with the same border and fill.
pub(crate) fn combo_style(ui: &mut Ui, pal: &Pal) {
    ui.spacing_mut().interact_size.y = FIELD_HEIGHT;
    ui.spacing_mut().button_padding = vec2(7.0, 3.0);
    let v = ui.visuals_mut();
    for (w, line) in [(&mut v.widgets.inactive, pal.border_strong), (&mut v.widgets.hovered, pal.border_strong), (&mut v.widgets.open, pal.accent)] {
        w.weak_bg_fill = pal.input;
        w.bg_fill = pal.input;
        w.bg_stroke = Stroke::new(1.0_f32, line);
        w.rounding = Rounding::same(5.0);
        w.expansion = 0.0;
    }
}

fn boot_name(b: Boot) -> &'static str {
    match b {
        Boot::Bios => "BIOS",
        Boot::Uefi => "UEFI",
        Boot::UefiSecure => "UEFI + Secure Boot",
    }
}

fn sockets_text(sockets: i64, cpus: i64) -> String {
    let cores = (cpus / sockets.max(1)).max(1);
    format!("{} socket{} with {} core{}", sockets, if sockets == 1 { "" } else { "s" }, cores, if cores == 1 { "" } else { "s" })
}

/// A small grey line just under a field.
fn note(ui: &mut Ui, pal: &Pal, text: &str) {
    ui.add_space(4.0 - ROW_GAP);
    ui.horizontal(|ui| {
        ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x + 2.0);
        ui.add(egui::Label::new(egui::RichText::new(text).size(12.0).color(pal.ink_soft)).wrap());
    });
}

/// Each call made to the server and its answer, as in a terminal (the latest at the bottom),
/// with Copy; `height` tall always when given, else as tall as the window leaves room for.
pub(crate) fn transcript(ui: &mut Ui, pal: &Pal, tr: &Trace, copied: &mut Option<Instant>, running: bool, height: Option<f32>) {
    let lines = tr.lines();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("What the server said").size(12.0).color(pal.ink_soft));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let done = copied.map_or(false, |t| t.elapsed() < Duration::from_secs(2));
            let (glyph, label) = if done { ("check", "Copied") } else { ("copy", "Copy") };
            if flat_button(ui, glyph, label, None, !lines.is_empty(), false, pal).on_hover_text("Copy the calls and the server's answers").clicked() {
                ui.ctx().copy_text(tr.text());
                *copied = Some(Instant::now());
            }
            if done {
                ui.ctx().request_repaint_after(Duration::from_millis(250));
            }
        });
    });
    ui.add_space(4.0 - ROW_GAP);
    // as tall as the window leaves room for (its title and the buttons still to come)
    let room = (ui.ctx().screen_rect().height() - ui.min_rect().height() - 160.0).clamp(90.0, 300.0);
    egui::Frame::none().fill(Color32::from_rgb(13, 17, 23)).rounding(Rounding::same(6.0)).inner_margin(egui::Margin::same(10.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let area = match height {
            Some(h) => egui::ScrollArea::vertical().min_scrolled_height(h).max_height(h).auto_shrink([false, false]),
            None => egui::ScrollArea::vertical().max_height(room).auto_shrink([false, true]),
        };
        area.stick_to_bottom(true).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let line = |ui: &mut Ui, text: String, col: Color32| {
                ui.add(egui::Label::new(egui::RichText::new(text).monospace().size(11.5).color(col)).wrap());
            };
            for (k, l) in &lines {
                match k {
                    Said::Asked => line(ui, format!("> {}", l), Color32::from_rgb(126, 231, 135)),
                    Said::Answer => line(ui, format!("  {}", l), Color32::from_gray(210)),
                    Said::Error => line(ui, format!("  {}", l), Color32::from_rgb(255, 123, 114)),
                    Said::Note => line(ui, format!("  {}", l), Color32::from_gray(140)),
                }
            }
            if running {
                match tr.waiting() {
                    _ if lines.is_empty() => line(ui, "  Connecting…".into(), Color32::from_gray(140)),
                    Some(w) if w.as_secs() >= 1 => line(ui, format!("  waiting for the answer… {} s", w.as_secs()), Color32::from_gray(140)),
                    Some(_) => line(ui, "  …".into(), Color32::from_gray(140)),
                    None => {}
                }
            }
        });
    });
}

/// A box under the form: yellow for a caution (a warning sign), red for what stops it being made
/// (a cross); `title` in bold, then `text`, and `button` (its icon and label) at the foot.
/// Whether the button was clicked.
pub(crate) fn alert(ui: &mut Ui, pal: &Pal, stop: bool, title: &str, text: &str, button: Option<(&str, &str)>) -> bool {
    let (bg, ink, sign) = if stop { (pal.danger, pal.danger_ink, "error") } else { (pal.caution, pal.caution_ink, "warning") };
    let size = 12.5;
    let mut job = egui::text::LayoutJob::default();
    let fmt = |family: FontFamily| egui::TextFormat { font_id: FontId::new(size, family), color: ink, ..Default::default() };
    job.append(title, 0.0, fmt(FontFamily::Name("bold".into())));
    job.append(" ", 0.0, fmt(FontFamily::Proportional));
    job.append(text, 0.0, fmt(FontFamily::Proportional));
    let margin = egui::Margin { left: 34.0, right: 12.0, top: 9.0, bottom: if button.is_some() { 5.0 } else { 9.0 } };
    let mut clicked = false;
    let shown = egui::Frame::none().fill(bg).rounding(Rounding::same(6.0)).inner_margin(margin).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.add(egui::Label::new(job).wrap());
        if let Some((glyph, label)) = button {
            // a small button in the box's own colours, its icon in line with the sign's
            ui.add_space(4.0);
            let font = FontId::new(size, FontFamily::Name("bold".into()));
            let w = 29.0 + ui.painter().layout_no_wrap(label.to_string(), font.clone(), ink).size().x;
            ui.horizontal(|ui| {
                // its icon under the text's first letter
                let (r, resp) = ui.allocate_exact_size(vec2(w, 24.0), Sense::click());
                if resp.hovered() {
                    ui.painter().rect_filled(r.expand2(vec2(4.0, 0.0)), Rounding::same(5.0), ink.gamma_multiply(0.1));
                }
                paint_glyph(ui.painter(), Rect::from_center_size(pos2(r.min.x + 7.0, r.center().y), vec2(14.0, 14.0)), glyph, ink);
                ui.painter().text(pos2(r.min.x + 19.0, r.center().y), Align2::LEFT_CENTER, label, font, ink);
                clicked = resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
            });
        }
    });
    let r = shown.response.rect;
    let row = ui.fonts(|f| f.row_height(&FontId::proportional(size)));
    let at = Rect::from_center_size(pos2(r.min.x + 17.0, r.min.y + margin.top + row / 2.0), vec2(16.0, 16.0));
    paint_glyph(ui.painter(), at, sign, ink);
    clicked
}

const CARD_PAD: f32 = 11.0;
const CARD_TITLE: f32 = 13.5;
const CARD_LINE: f32 = 11.5;
const CARD_GAP: f32 = 4.0;

/// The size cards' height (room for a title and three lines, with even margins), and where
/// their titles' middle is.
fn card_metrics(ui: &Ui) -> (f32, f32) {
    let (title, line) = ui.fonts(|f| (f.row_height(&FontId::new(CARD_TITLE, FontFamily::Name("bold".into()))), f.row_height(&FontId::proportional(CARD_LINE))));
    (2.0 * CARD_PAD + title + CARD_GAP + 3.0 * line, CARD_PAD + title / 2.0)
}

/// One of the size cards: a title and up to three lines, centred; the chosen one outlined in the
/// accent colour.
fn size_card(ui: &mut Ui, pal: &Pal, size: egui::Vec2, title: &str, lines: &[String], on: bool) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(size, Sense::click());
    let p = ui.painter();
    let fill = if on { pal.accent.gamma_multiply(if pal.dark { 0.18 } else { 0.08 }) } else if resp.hovered() { pal.hover } else { pal.surface };
    let stroke = if on { Stroke::new(1.6_f32, pal.accent) } else { Stroke::new(1.0_f32, if resp.hovered() { pal.border_strong } else { pal.border }) };
    p.rect(r, Rounding::same(8.0), fill, stroke);
    let title_font = FontId::new(CARD_TITLE, FontFamily::Name("bold".into()));
    let (title_h, line_h) = ui.fonts(|f| (f.row_height(&title_font), f.row_height(&FontId::proportional(CARD_LINE))));
    let x = r.center().x;
    p.text(pos2(x, r.min.y + CARD_PAD), Align2::CENTER_TOP, title, title_font, if on { pal.accent } else { pal.ink });
    // fewer lines sit in the middle of the room for three
    let top = r.min.y + CARD_PAD + title_h + CARD_GAP + (3 - lines.len().min(3)) as f32 * line_h / 2.0;
    for (i, l) in lines.iter().take(3).enumerate() {
        p.text(pos2(x, top + i as f32 * line_h), Align2::CENTER_TOP, l, FontId::proportional(CARD_LINE), pal.ink_soft);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::{Connection, DemoConnection};

    #[test]
    fn every_network_offered() {
        // VLANs' bridges are "xapi…" too: offered like the rest, the first card's network first
        let d = DemoConnection::new().fetch(0).unwrap();
        let dlg = NewVmDialog::new(1, &d, None);
        let names: Vec<&str> = dlg.nets.iter().map(|n| n.1.as_str()).collect();
        assert!(names.contains(&"Office VLAN"), "{:?}", names);
        assert_eq!(dlg.net.map(|i| dlg.nets[i].1.as_str()), Some("Pool-wide network 0"));
    }

    #[test]
    fn server_then_storage() {
        let mut d = DemoConnection::new().fetch(0).unwrap();
        let place = |dlg: &NewVmDialog| dlg.request().map(|s| (s.host, s.sr)).unwrap();
        // shared default storage: any server
        d.default_sr = Some("sr-nfs".into());
        assert_eq!(place(&NewVmDialog::new(1, &d, None)), (None, "sr-nfs".into()));
        // a server's own default storage: that server
        d.default_sr = Some("sr-local2".into());
        let mut dlg = NewVmDialog::new(1, &d, None);
        assert_eq!(place(&dlg), (Some("host-b".into()), "sr-local2".into()));
        // another server doesn't see it: shared storage instead
        dlg.server = dlg.server_index("host-a");
        dlg.server_changed();
        assert_eq!(place(&dlg), (Some("host-a".into()), "sr-nfs".into()));
        // a disk on host-a's own storage
        dlg.store = dlg.stores.iter().position(|s| s.rref == "sr-local1").unwrap();
        assert_eq!(place(&dlg), (Some("host-a".into()), "sr-local1".into()));
        // any server: only shared storage
        dlg.server = None;
        dlg.server_changed();
        assert_eq!(place(&dlg), (None, "sr-nfs".into()));
        // more memory than any server has free
        let dlg = dlg.with_custom(4, 1000, 80);
        assert!(dlg.short_of_memory().unwrap().starts_with("No server has 1000 GB free"));
        assert!(NewVmDialog::new(1, &d, None).short_of_memory().is_none());
        // an ISO in a server's own library: that server
        d.srs.iter_mut().find(|x| x.rref == "sr-iso").unwrap().host = Some("host-a".into());
        let dlg = NewVmDialog::new(1, &d, None);
        assert_eq!(place(&dlg), (Some("host-a".into()), "sr-nfs".into()));
    }

    #[test]
    fn from_a_template() {
        let d = DemoConnection::new().fetch(0).unwrap();
        let mut dlg = NewVmDialog::new(1, &d, None).with_template("tpl-web");
        let spec = dlg.request().unwrap();
        assert!(spec.from_template && spec.keep_nics && spec.iso.is_empty());
        assert_eq!((spec.template.as_str(), spec.name.as_str()), ("tpl-web", "Ubuntu 24.04 web server"));
        assert_eq!((spec.vcpus, spec.memory, spec.disk), (2, 4 * GB, 40 * GB));
        // its disks stay on its own storage: an instant copy, checked first (it's NFS)
        assert_eq!((spec.sr.as_str(), spec.copy_to.as_ref()), ("sr-nfs", None));
        assert_eq!(spec.check, vec!["sr-nfs".to_string()]);
        // onto a server's own storage: a full copy there
        dlg.server = dlg.server_index("host-a");
        dlg.store = dlg.stores.iter().position(|s| s.rref == "sr-local1").unwrap();
        assert_eq!(dlg.request().unwrap().copy_to.as_deref(), Some("sr-local1"));
        // other CPUs and memory, its disk as it is
        dlg.size = 3;
        (dlg.cpus, dlg.memory) = ("6".into(), "12".into());
        let spec = dlg.request().unwrap();
        assert_eq!((spec.vcpus, spec.memory, spec.disk), (6, 12 * GB, 40 * GB));
    }

    #[test]
    fn storage_checked_before_create() {
        let d = DemoConnection::new().fetch(0).unwrap();
        // shared default storage: any server; the ISO's library and the NFS storage both checked
        let mut d2 = d.clone();
        d2.default_sr = Some("sr-nfs".into());
        let mut dlg = NewVmDialog::new(1, &d2, None);
        let wanted = dlg.wanted_checks();
        assert_eq!(wanted, vec![("sr-iso".to_string(), None), ("sr-nfs".to_string(), None)]);
        assert!(dlg.wanted_checks().is_empty(), "asked once");
        assert!(!dlg.checks_passed());
        assert_eq!(dlg.request().unwrap().check, vec!["sr-iso".to_string(), "sr-nfs".to_string()]);
        dlg.checked("sr-nfs".into(), None, Ok(()));
        dlg.checked("sr-iso".into(), None, Err(ApiError { code: NOT_ANSWERING.into(), details: vec![] }));
        assert!(!dlg.checks_passed());
        let (title, text) = dlg.check_problem("sr-iso", None, &ApiError { code: NOT_ANSWERING.into(), details: vec![] });
        assert_eq!(title, "ISO library is offline.");
        assert!(text.starts_with("The pool can't reach //nas/isos"), "{}", text);
        dlg.checked("sr-iso".into(), None, Ok(()));
        assert!(dlg.checks_passed());
        // local storage isn't checked, only the library, for the server it's on
        let mut d3 = d.clone();
        d3.default_sr = Some("sr-local2".into());
        let mut dlg = NewVmDialog::new(1, &d3, None);
        assert_eq!(dlg.wanted_checks(), vec![("sr-iso".to_string(), Some("host-b".to_string()))]);
        // a failure clears them, so they're asked again
        dlg.failed("no".into());
        dlg.error = None;
        assert_eq!(dlg.wanted_checks().len(), 1);
    }
}
