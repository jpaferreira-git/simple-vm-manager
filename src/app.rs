//! The main window: menus, toolbar, foldable sidebar with the resource tree, header, tabs, dialogs.
//! Every colour comes from the light or dark palette (theme.rs).

use crate::conn::*;
use crate::console::{self, ConsoleView};
use crate::isolib::{IsoAction, IsoDialog};
use crate::newvm::{NewVmAction, NewVmDialog};
use crate::upload::{UploadAction, UploadDialog};
use crate::secrets;
use crate::icons::*;
use crate::model::*;
use crate::theme::{self, Pal, ThemeMode};
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontFamily, FontId, Key, Modifiers, Rect, Rounding, Sense, Stroke, Ui};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const REFRESH_MS: u64 = 15_000; // XenConnection.HEARTBEAT_INTERVAL
pub const RECONNECT_FIRST_MS: u64 = 5_000; // XenConnection.RECONNECT_SHORT_TIMEOUT_MS
pub const RECONNECT_LATER_MS: u64 = 120_000; // XenConnection.RECONNECT_HOST_TIMEOUT_MS
pub const FETCH_TIMEOUT_MS: u64 = 30_000; // estimated
const RAIL_WIDTH: f32 = 46.0;

#[derive(Clone, Copy, PartialEq, Debug)]
enum ConnState {
    Connecting,
    Connected,
    Reconnecting,
    Disconnected,
}

struct ServerEntry {
    id: u32,
    conn: Arc<dyn Connection>,
    data: Option<PoolData>,
    state: ConnState,
    error: Option<String>,
    next_refresh: Instant,
    fetching: bool,
    ever: bool,
    failures: u32,
    /// VMs with a power action under way: what's being done ("Starting…") and the action.
    busy: HashMap<String, (String, &'static str)>,
    /// Once the first login works: (true, password) keeps the password in the keychain and
    /// reconnects at startup; (false, _) forgets any saved one. None: leave things as they are.
    pending_save: Option<(bool, String)>,
}

impl ServerEntry {
    /// "–  Logged in as root", after the connection's name in the tree.
    fn login_note(&self) -> String {
        format!("–  Logged in as {}", self.conn.username())
    }

    fn name(&self) -> String {
        match &self.data {
            Some(d) if !d.name.is_empty() => d.name.clone(),
            Some(d) if d.hosts.len() == 1 => d.hosts[0].name.clone(),
            _ => self.conn.hostname(),
        }
    }
}

enum Msg {
    Fetched(u32, Result<PoolData, ApiError>),
    /// At startup: whether there's a keychain, the passwords saved in it for earlier servers, and
    /// the servers to reconnect to.
    Keychain { available: bool, saved: Vec<(String, String, String)>, session: Vec<(String, String)> },
    KeychainError(String),
    IsoDone { sid: u32, lib: IsoLibrary, result: Result<(), ApiError> },
    /// A rescan (forget false) or disconnect (forget true) of a storage ended.
    SrDone { sid: u32, sr: String, name: String, forget: bool, result: Result<(), ApiError> },
    /// Making a new VM ended: its name, and whether it was to start.
    /// A new VM made (its reference), or why not; `start`: it's being started next.
    VmCreated { sid: u32, name: String, start: bool, result: Result<String, ApiError> },
    /// The new VM `vm` started, or why not.
    VmStarted { sid: u32, vm: String, name: String, result: Result<(), ApiError> },
    /// Whether a storage answered, for the New VM dialog.
    SrChecked { sid: u32, sr: String, host: Option<String>, result: Result<(), ApiError> },
    /// Deleting an ISO file ended.
    IsoDeleted { sid: u32, sr: String, vdi: String, name: String, result: Result<(), ApiError> },
    /// Deleting a VM ended: the disks left behind, or why it wasn't deleted.
    VmDeleted { sid: u32, vm: String, name: String, result: Result<Vec<String>, ApiError> },
    /// A link in the Upload dialog was checked.
    LinkChecked { url: String, result: Result<WebFile, String> },
    /// The file chooser closed: the file picked to upload to library `sr`, if any.
    Picked { sid: u32, sr: String, path: Option<PathBuf> },
    /// An upload to library `sr` ended.
    UploadDone { sr: String, result: Result<(), ApiError> },
    /// Saving an ISO file's description ended.
    DescriptionDone { sid: u32, sr: String, vdi: String, name: String, text: String, result: Result<(), ApiError> },
    ActionDone { sid: u32, vm: String, log: usize, err: Option<String>, label: &'static str, op: &'static str, text: String },
}

/// A file being copied into an ISO library, from this computer or from the web.
struct Upload {
    sid: u32,
    /// The library's name.
    lib: String,
    /// The name it gets in the library.
    name: String,
    web: bool,
    /// The site it comes from (for a download).
    from: String,
    t: Arc<Transfer>,
    started: Instant,
    /// Its line in the log, finished when it ends.
    log: usize,
    /// How long it took, once it's done; its card then stays a moment, green.
    done: Option<Duration>,
    ended: Option<Instant>,
}

impl Upload {
    fn active(&self) -> bool {
        self.done.is_none()
    }
}

/// How long a finished upload's card stays.
const DONE_CARD: Duration = Duration::from_secs(8);

/// An ISO file's description being edited in its row.
struct IsoEdit {
    vdi: String,
    text: String,
    /// Put the cursor in the box on the next frame.
    focus: bool,
}

struct LogEntry {
    time: String,
    sid: u32,
    server: String,
    obj_ref: Option<String>,
    obj: String,
    text: String,
    status: String,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Root,
    Category,
    Server,
    Pool,
    Host,
    Vm,
    Sr,
    Net,
    /// "Shared Storage" or "Virtual Networks" in a pool (rref is "srs" or "nets").
    Folder,
}

#[derive(Clone)]
struct Node {
    key: String,
    text: String,
    icon: IconKind,
    kind: Kind,
    sid: Option<u32>,
    rref: Option<String>,
    /// Shown after the name in a softer colour, and not searched (who's logged in to a connection).
    note: String,
    children: Vec<Node>,
}

impl Node {
    fn new(key: String, text: String, icon: IconKind, kind: Kind, sid: Option<u32>, rref: Option<String>) -> Node {
        Node { key, text, icon, kind, sid, rref, note: String::new(), children: vec![] }
    }
    fn find(&self, key: &str) -> Option<&Node> {
        if self.key == key {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(key))
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tab {
    Home,
    Search,
    General,
    Memory,
    Storage,
    Networking,
    Console,
    IsoFiles,
    Logs,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Home => "Home",
            Tab::Search => "Search",
            Tab::General => "General",
            Tab::Memory => "Memory",
            Tab::Storage => "Storage",
            Tab::Networking => "Networking",
            Tab::Console => "Console",
            Tab::IsoFiles => "ISO Files",
            Tab::Logs => "Logs",
        }
    }
}

enum Dialog {
    None,
    AddServer { server: String, user: String, password: String, error: String, remember: bool },
    Confirm { action: usize, sid: u32, vm: String, title: String, text: String },
    Message { title: String, text: String },
    Iso(Box<IsoDialog>),
    Upload(Box<UploadDialog>),
    NewVm(Box<NewVmDialog>),
    /// Disconnect (forget) an ISO library?
    ConfirmSr { sid: u32, sr: String, text: String },
    /// Deleting an ISO file: server, library, disc, its name, the question.
    ConfirmDeleteIso { sid: u32, sr: String, vdi: String, name: String, text: String },
    /// The app's settings (the gear on the toolbar); `startup`: the servers it connects to as it
    /// starts (read when it opens).
    Settings { startup: Vec<(String, String)> },
    /// Deleting a VM: whether it's on (turned off first), its disks, how many snapshots it has,
    /// and whether its disks go too (ticked to begin with).
    ConfirmDeleteVm { sid: u32, vm: String, name: String, running: bool, disks: Vec<VmDisk>, snapshots: usize, delete_disks: bool },
}

enum Intent {
    Select(String),
    Toggle(String),
    Action(usize),
    AddServer,
    Demo,
    Remove(u32),
    ForgetPassword(u32),
    IsoLibrary,
    RescanSr(u32, String),
    AskForgetSr(u32, String),
    /// Open the Upload dialog of an ISO library.
    Upload(u32, String),
    /// Open the New VM dialog: for this pool (else the selected one), with this ISO's disc.
    NewVm(Option<u32>, Option<String>),
    /// Ask before deleting an ISO file: server, library, disc.
    AskDeleteIso(u32, String, String),
    /// Ask before deleting a VM: server, VM.
    AskDeleteVm(u32, String),
    /// Hide a finished upload's card.
    DismissUpload(String),
    /// Files dropped on the window.
    Dropped(Vec<PathBuf>),
    /// Stop the upload to this library.
    CancelUpload(String),
    /// Save an ISO file's description: server, library, disc, file name, text.
    SaveDescription(u32, String, String, String, String),
    Reconnect(u32),
    Refresh(Option<u32>),
    View(bool),
    Tab(Tab),
    Section(String),
    /// Open the Settings dialog.
    Settings,
    Sidebar(bool),
    SearchFromRail,
    About,
}

pub struct Startup {
    pub shot_path: Option<String>,
    pub shot_show: String,
    pub theme: Option<ThemeMode>,
    pub sidebar_open: Option<bool>,
}

pub struct App {
    servers: Vec<ServerEntry>,
    next_id: u32,
    log: Vec<LogEntry>,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    selected: String,
    expanded: HashSet<String>,
    search: String,
    focus_search: bool,
    objects_view: bool,
    sidebar_open: bool,
    theme_mode: ThemeMode,
    /// The interface's size (egui's zoom factor), kept in the settings.
    zoom: f32,
    pal: Pal,
    applied_dark: Option<bool>,
    tab: Tab,
    closed_sections: HashSet<String>,
    dialog: Dialog,
    console: Option<ConsoleView>,
    /// Earlier servers and the account used on each, newest first.
    known_hosts: Vec<(String, String)>,
    /// None while still asking the system whether it has a keychain.
    keychain: Option<bool>,
    /// Storage being rescanned or disconnected right now.
    sr_busy: HashSet<String>,
    /// Storage checks under way for the New VM dialog: pool, storage, server.
    sr_checking: HashSet<(u32, String, Option<String>)>,
    /// The ISO file whose description is being typed in, if any.
    iso_edit: Option<IsoEdit>,
    /// Descriptions being saved, by disc, shown until the server answers.
    iso_saving: HashMap<String, String>,
    /// ISO files being deleted, by disc.
    iso_deleting: HashSet<String>,
    /// VMs being deleted right now.
    vm_deleting: HashSet<String>,
    /// A new VM to select (and the tab to show) once it's in the tree.
    pending_select: Option<(String, Tab)>,
    /// VMs being made right now (a check's screenshot waits for them).
    creating: u32,
    /// Files being uploaded, by ISO library (one at a time each).
    uploads: HashMap<String, Upload>,
    /// Passwords saved in the keychain, by (server, user).
    saved: HashMap<(String, String), String>,
    shot_path: Option<String>,
    shot_show: String,
    frames: u64,
    shot_ready: bool,
    shot_requested: bool,
    shot_dialog: bool,
    started: Instant,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, start: Startup) -> App {
        setup_fonts(&cc.egui_ctx);
        let shot = start.shot_path.is_some();
        // A check's screenshot doesn't use the person's saved settings: light theme, sidebar open,
        // unless --theme / --sidebar say otherwise.
        let saved = if shot { Settings::default() } else { load_settings() };
        let theme_mode = start.theme.unwrap_or(if shot { ThemeMode::Light } else { saved.theme });
        let (tx, rx) = mpsc::channel();
        let mut app = App {
            servers: vec![],
            next_id: 1,
            log: vec![],
            tx,
            rx,
            selected: "root".into(),
            expanded: ["root".to_string()].into_iter().collect(),
            search: String::new(),
            focus_search: false,
            objects_view: false,
            sidebar_open: start.sidebar_open.unwrap_or(saved.sidebar_open),
            theme_mode,
            zoom: saved.zoom,
            pal: theme::LIGHT,
            applied_dark: None,
            tab: Tab::Home,
            closed_sections: HashSet::new(),
            dialog: Dialog::None,
            console: None,
            known_hosts: load_history(),
            keychain: None,
            saved: HashMap::new(),
            sr_busy: HashSet::new(),
            sr_checking: HashSet::new(),
            iso_edit: None,
            iso_saving: HashMap::new(),
            iso_deleting: HashSet::new(),
            vm_deleting: HashSet::new(),
            pending_select: None,
            creating: 0,
            uploads: HashMap::new(),
            shot_path: start.shot_path,
            shot_show: start.shot_show,
            frames: 0,
            shot_ready: false,
            shot_requested: false,
            shot_dialog: false,
            started: Instant::now(),
        };
        app.apply_theme(&cc.egui_ctx);
        cc.egui_ctx.set_zoom_factor(app.zoom);
        if !shot {
            // The keychain may take a moment (or ask to be unlocked): read it in the background.
            let (tx, ctx) = (app.tx.clone(), cc.egui_ctx.clone());
            let (hosts, session) = (app.known_hosts.clone(), load_session());
            std::thread::spawn(move || {
                let available = secrets::available();
                let mut saved = vec![];
                if available {
                    let mut wanted: Vec<(String, String)> = hosts.into_iter().chain(session.iter().cloned()).collect();
                    wanted.sort();
                    wanted.dedup();
                    for (h, u) in wanted {
                        if let Some(p) = secrets::load(&h, &u) {
                            saved.push((h, u, p));
                        }
                    }
                }
                let _ = tx.send(Msg::Keychain { available, saved, session });
                ctx.request_repaint();
            });
        }
        if shot && app.shot_show == "welcome" {
            app.shot_ready = true; // Home with nothing connected
        } else if shot {
            app.add_server(Arc::new(DemoConnection::new()));
        }
        app
    }

    // ------------------------------------------------------------------ theme and settings

    fn wants_dark(&self, ctx: &egui::Context) -> bool {
        match self.theme_mode {
            ThemeMode::Light => false,
            ThemeMode::Dark => true,
            ThemeMode::System => ctx.input(|i| i.raw.system_theme) == Some(egui::Theme::Dark),
        }
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        let dark = self.wants_dark(ctx);
        if self.applied_dark == Some(dark) {
            return;
        }
        self.pal = if dark { theme::DARK } else { theme::LIGHT };
        ctx.set_theme(if dark { egui::ThemePreference::Dark } else { egui::ThemePreference::Light });
        ctx.set_visuals(theme::visuals(&self.pal));
        self.applied_dark = Some(dark);
    }

    fn save_settings(&self) {
        if self.shot_path.is_none() {
            save_settings(&Settings { theme: self.theme_mode, sidebar_open: self.sidebar_open, zoom: self.zoom });
        }
    }

    // ------------------------------------------------------------------ servers

    fn add_server(&mut self, conn: Arc<dyn Connection>) {
        let id = self.next_id;
        self.next_id += 1;
        let host = conn.hostname();
        self.servers.push(ServerEntry {
            id,
            conn,
            data: None,
            state: ConnState::Connecting,
            error: None,
            next_refresh: Instant::now(),
            fetching: false,
            ever: false,
            failures: 0,
            busy: HashMap::new(),
            pending_save: None,
        });
        self.add_log(id, &host, None, &host, &format!("Connecting to {}", host), "In progress");
        self.select(format!("srv:{}", id));
    }

    /// Rescans a storage, or disconnects (forgets) it, in the background.
    fn sr_job(&mut self, sid: u32, sr: String, forget: bool, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let name = s.data.as_ref().and_then(|d| d.sr(&sr)).map(|x| x.name.clone()).unwrap_or_default();
        let (conn, tx, ctx) = (s.conn.clone(), self.tx.clone(), ctx.clone());
        self.sr_busy.insert(sr.clone());
        std::thread::spawn(move || {
            let result = if forget { conn.forget_sr(&sr) } else { conn.scan_sr(&sr) };
            let _ = tx.send(Msg::SrDone { sid, sr, name, forget, result });
            ctx.request_repaint();
        });
    }

    /// Opens the system's file chooser (on a worker thread, so the window keeps drawing).
    fn pick_iso(&mut self, sid: u32, sr: String, ctx: &egui::Context) {
        let lib = self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.sr(&sr)).map(|x| x.name.clone()).unwrap_or_default();
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut dialog = rfd::AsyncFileDialog::new().set_title(format!("Upload to {}", lib)).add_filter("ISO images", &["iso", "ISO"]);
            let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
            if let Some(dir) = home.map(|h| h.join("Downloads")).filter(|d| d.is_dir()) {
                dialog = dialog.set_directory(dir);
            }
            let path = pollster::block_on(dialog.pick_file()).map(|f| f.path().to_path_buf());
            let _ = tx.send(Msg::Picked { sid, sr, path });
            ctx.request_repaint();
        });
    }

    /// Starts copying a file (or a link's file) into ISO library `sr` in the background. A
    /// local file keeps its own name, made into one the library takes.
    fn start_upload(&mut self, sid: u32, sr: String, src: Source, name: Option<String>, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let Some(d) = s.data.as_ref() else { return };
        let Some(x) = d.sr(&sr) else { return };
        let name = match (&src, name) {
            (_, Some(n)) => n,
            (Source::File(p), None) => iso_file_name(&p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()),
            (Source::Web { .. }, None) => return,
        };
        let (web, from) = match &src {
            Source::Web { url, .. } => (true, url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or("").to_string()),
            Source::File(_) => (false, String::new()),
        };
        let lib = x.name.clone();
        if self.uploads.get(&sr).map_or(false, Upload::active) {
            self.dialog = Dialog::Message { title: "Upload".into(), text: format!("'{}' is already uploading a file. Wait for it to finish, then try again.", lib) };
            return;
        }
        if x.files.iter().any(|f| f.name.eq_ignore_ascii_case(&name)) {
            self.dialog = Dialog::Message { title: "Upload".into(), text: format!("There's already a file named '{}' in '{}'.", name, lib) };
            return;
        }
        // a library only one server sees takes the file on that server
        let address = x.host.as_deref().and_then(|h| d.host(h)).map(|h| h.address.clone());
        let (conn, server, tx, ctx) = (s.conn.clone(), s.name(), self.tx.clone(), ctx.clone());
        let what = if web { format!("Downloading '{}' from {} into '{}'", name, from, lib) } else { format!("Uploading '{}' to '{}'", name, lib) };
        let log = self.add_log(sid, &server, Some(&sr), &lib, &what, "In progress");
        let t = Arc::new(Transfer::default());
        self.uploads.insert(sr.clone(), Upload { sid, lib, name: name.clone(), web, from, t: t.clone(), started: Instant::now(), log, done: None, ended: None });
        std::thread::spawn(move || {
            let result = conn.upload_iso(&sr, address.as_deref(), &src, &name, &t);
            let _ = tx.send(Msg::UploadDone { sr, result });
            ctx.request_repaint();
        });
    }

    /// Checks in the background that a storage answers on a server, for the New VM dialog (one
    /// check at a time for each: a dialog opened again meanwhile gets the same one's answer).
    fn check_sr(&mut self, sid: u32, sr: String, host: Option<String>, ctx: &egui::Context) {
        let Some(conn) = self.server(sid).map(|s| s.conn.clone()) else { return };
        if !self.sr_checking.insert((sid, sr.clone(), host.clone())) {
            return;
        }
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = conn.check_sr(&sr, host.as_deref(), &HashMap::new(), &Trace::default());
            let _ = tx.send(Msg::SrChecked { sid, sr, host, result });
            ctx.request_repaint();
        });
    }

    /// Makes a VM in the background (the dialog waits, showing each call in `trace`).
    fn create_vm(&mut self, sid: u32, spec: NewVm, trace: Arc<Trace>, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else {
            if let Dialog::NewVm(d) = &mut self.dialog {
                d.failed("This pool isn't connected any more. Connect to it again, then try again.".into());
            }
            return;
        };
        let (conn, tx, ctx) = (s.conn.clone(), self.tx.clone(), ctx.clone());
        self.creating += 1;
        std::thread::spawn(move || {
            let result = conn.create_vm(&spec, &trace);
            let made = result.as_ref().ok().cloned();
            let _ = tx.send(Msg::VmCreated { sid, name: spec.name.clone(), start: spec.start, result });
            ctx.request_repaint();
            // then started, which can take a while: the dialog can close meanwhile
            if let (Some(vm), true) = (made, spec.start) {
                let result = conn.start_traced(&vm, &format!("\"{}\"", spec.name), &trace);
                let _ = tx.send(Msg::VmStarted { sid, vm, name: spec.name.clone(), result });
                ctx.request_repaint();
            }
        });
    }

    /// Deletes a VM (and, with `disks`, its disks) in the background.
    fn delete_vm(&mut self, sid: u32, vm: String, name: String, disks: bool, ctx: &egui::Context) {
        let Some(conn) = self.server(sid).map(|s| s.conn.clone()) else { return };
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        self.vm_deleting.insert(vm.clone());
        std::thread::spawn(move || {
            let result = conn.delete_vm(&vm, disks);
            let _ = tx.send(Msg::VmDeleted { sid, vm, name, result });
            ctx.request_repaint();
        });
    }

    /// Deletes an ISO file in the background.
    fn delete_iso(&mut self, sid: u32, sr: String, vdi: String, name: String, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let (conn, tx, ctx) = (s.conn.clone(), self.tx.clone(), ctx.clone());
        self.iso_deleting.insert(vdi.clone());
        if self.iso_edit.as_ref().map_or(false, |e| e.vdi == vdi) {
            self.iso_edit = None;
        }
        std::thread::spawn(move || {
            let result = conn.delete_iso(&vdi);
            let _ = tx.send(Msg::IsoDeleted { sid, sr, vdi, name, result });
            ctx.request_repaint();
        });
    }

    /// Saves an ISO file's description in the background.
    fn save_description(&mut self, sid: u32, sr: String, vdi: String, name: String, text: String, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let (conn, tx, ctx) = (s.conn.clone(), self.tx.clone(), ctx.clone());
        self.iso_saving.insert(vdi.clone(), text.clone());
        std::thread::spawn(move || {
            let result = conn.set_vdi_description(&vdi, &text);
            let _ = tx.send(Msg::DescriptionDone { sid, sr, vdi, name, text, result });
            ctx.request_repaint();
        });
    }

    /// Asks the server to create the ISO library (which mounts the share) in the background.
    fn start_iso_library(&mut self, sid: u32, lib: IsoLibrary, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let (conn, tx, ctx) = (s.conn.clone(), self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = conn.create_iso_library(&lib);
            let _ = tx.send(Msg::IsoDone { sid, lib, result });
            ctx.request_repaint();
        });
    }

    /// Keeps a server's password in the keychain and reconnects there at startup (Some), or forgets
    /// it and doesn't (None). The keychain is written in the background.
    fn keep_password(&mut self, host: &str, user: &str, password: Option<String>) {
        let key = (host.to_string(), user.to_string());
        let (h, u, tx) = (key.0.clone(), key.1.clone(), self.tx.clone());
        match password {
            Some(p) => {
                session_add(host, user);
                if self.saved.get(&key) != Some(&p) {
                    self.saved.insert(key, p.clone());
                    std::thread::spawn(move || {
                        if let Err(e) = secrets::save(&h, &u, &p) {
                            let _ = tx.send(Msg::KeychainError(e));
                        }
                    });
                }
            }
            None => {
                session_remove(host, user);
                if self.saved.remove(&key).is_some() {
                    std::thread::spawn(move || secrets::forget(&h, &u));
                }
            }
        }
    }

    fn remove_server(&mut self, sid: u32, log: bool) {
        if let Some(i) = self.servers.iter().position(|s| s.id == sid) {
            let s = self.servers.remove(i);
            let name = s.name();
            let conn = s.conn.clone();
            std::thread::spawn(move || conn.close());
            if log {
                self.add_log(sid, &name, None, &name, &format!("Disconnected from {}", name), "Done");
            }
        }
    }

    fn connect_demo(&mut self) {
        if let Some(s) = self.servers.iter().find(|s| s.conn.is_demo()) {
            let k = format!("srv:{}", s.id);
            self.select(k);
            return;
        }
        self.add_server(Arc::new(DemoConnection::new()));
    }

    fn server(&self, sid: u32) -> Option<&ServerEntry> {
        self.servers.iter().find(|s| s.id == sid)
    }
    fn server_mut(&mut self, sid: u32) -> Option<&mut ServerEntry> {
        self.servers.iter_mut().find(|s| s.id == sid)
    }

    fn add_log(&mut self, sid: u32, server: &str, obj_ref: Option<&str>, obj: &str, text: &str, status: &str) -> usize {
        self.log.push(LogEntry {
            time: clock_text(),
            sid,
            server: server.into(),
            obj_ref: obj_ref.map(String::from),
            obj: obj.into(),
            text: text.into(),
            status: status.into(),
        });
        self.log.len() - 1
    }

    fn tick(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        for s in self.servers.iter_mut() {
            if !s.fetching && now >= s.next_refresh && s.state != ConnState::Disconnected {
                s.fetching = true;
                let (conn, tx, ctx, id) = (s.conn.clone(), self.tx.clone(), ctx.clone(), s.id);
                std::thread::spawn(move || {
                    let r = conn.fetch(FETCH_TIMEOUT_MS);
                    let _ = tx.send(Msg::Fetched(id, r));
                    ctx.request_repaint();
                });
            }
        }
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Fetched(id, r) => self.on_fetched(id, r),
                Msg::Keychain { available, saved, session } => {
                    self.keychain = Some(available);
                    self.saved = saved.into_iter().map(|(h, u, p)| ((h, u), p)).collect();
                    // reconnect to the servers left connected last time, without moving the selection
                    let selected = self.selected.clone();
                    for (h, u) in session {
                        let known = self.servers.iter().any(|s| !s.conn.is_demo() && s.conn.hostname().eq_ignore_ascii_case(&h));
                        if let (false, Some(p)) = (known, self.saved.get(&(h.clone(), u.clone())).cloned()) {
                            self.add_server(Arc::new(RealConnection::new(&h, &u, &p).with_members(load_members(&h))));
                        }
                    }
                    self.selected = selected;
                }
                Msg::IsoDone { sid, lib, result } => {
                    let server = self.server(sid).map(|s| s.name()).unwrap_or_default();
                    match result {
                        Ok(()) => {
                            self.add_log(sid, &server, None, &lib.name, &format!("Created ISO library '{}' from {}", lib.name, lib.location), "Done");
                            if matches!(self.dialog, Dialog::Iso(_)) {
                                self.dialog = Dialog::None;
                            }
                            if let Some(s) = self.server_mut(sid) {
                                s.next_refresh = Instant::now(); // show it right away
                            }
                            self.expanded.insert(format!("folder:{}:srs", sid));
                        }
                        Err(e) => {
                            self.add_log(sid, &server, None, &lib.name, &format!("Couldn't create ISO library '{}': {}", lib.name, e.message()), "Failed");
                            if let Dialog::Iso(d) = &mut self.dialog {
                                d.failed(&lib, &e);
                            }
                        }
                    }
                }
                Msg::SrDone { sid, sr, name, forget, result } => {
                    self.sr_busy.remove(&sr);
                    let server = self.server(sid).map(|s| s.name()).unwrap_or_default();
                    let what = if forget { "Disconnected ISO library" } else { "Rescanned" };
                    match result {
                        Ok(()) => {
                            self.add_log(sid, &server, Some(&sr), &name, &format!("{} '{}'", what, name), "Done");
                            if forget && self.selected == format!("sr:{}:{}", sid, sr) {
                                self.select(format!("srv:{}", sid));
                            }
                        }
                        Err(e) => {
                            let why = if e.code == "VDI_IN_USE" || e.details.join(" ").to_lowercase().contains("in use") {
                                "A VM still has a disc from this library in its DVD drive. Eject it first, then try again.".to_string()
                            } else {
                                e.message()
                            };
                            let title = if forget { "Disconnect ISO Library" } else { "Rescan" };
                            self.add_log(sid, &server, Some(&sr), &name, &format!("Couldn't {} '{}': {}", if forget { "disconnect" } else { "rescan" }, name, why), "Failed");
                            self.dialog = Dialog::Message { title: title.into(), text: format!("Couldn't {} '{}'.\n\n{}", if forget { "disconnect" } else { "rescan" }, name, why) };
                        }
                    }
                    if let Some(s) = self.server_mut(sid) {
                        s.next_refresh = Instant::now();
                    }
                }
                Msg::Picked { sid, sr, path } => {
                    if let Some(p) = path {
                        let ctx = ctx.clone();
                        self.start_upload(sid, sr, Source::File(p), None, &ctx);
                    }
                }
                Msg::VmCreated { sid, name, start, result } => {
                    let server = self.server(sid).map(|s| s.name()).unwrap_or_default();
                    if result.is_err() || !start {
                        self.creating = self.creating.saturating_sub(1);
                    }
                    match result {
                        Ok(vm) => {
                            self.add_log(sid, &server, Some(&vm), &name, &format!("Created VM '{}'", name), "Done");
                            if start {
                                // the dialog waits for the start, but can be closed
                                if let Dialog::NewVm(d) = &mut self.dialog {
                                    d.starting(vm.clone());
                                }
                            } else {
                                self.dialog = Dialog::None;
                            }
                            // shown once the next refresh brings it
                            self.pending_select = Some((format!("vm:{}:{}", sid, vm), Tab::General));
                        }
                        Err(e) => {
                            let why = create_problem(&e);
                            self.add_log(sid, &server, None, &name, &format!("Couldn't create VM '{}': {}", name, why), "Failed");
                            if let Dialog::NewVm(d) = &mut self.dialog {
                                d.failed(why);
                            }
                        }
                    }
                    if let Some(s) = self.server_mut(sid) {
                        s.next_refresh = Instant::now();
                    }
                }
                Msg::SrChecked { sid, sr, host, result } => {
                    self.sr_checking.remove(&(sid, sr.clone(), host.clone()));
                    if let Dialog::NewVm(d) = &mut self.dialog {
                        if d.sid == sid {
                            d.checked(sr, host, result);
                        }
                    }
                }
                Msg::VmStarted { sid, vm, name, result } => {
                    self.creating = self.creating.saturating_sub(1);
                    let server = self.server(sid).map(|s| s.name()).unwrap_or_default();
                    // still watching it in the dialog?
                    let watching = matches!(&self.dialog, Dialog::NewVm(d) if d.made_vm() == Some(vm.as_str()));
                    match result {
                        Ok(()) => {
                            self.add_log(sid, &server, Some(&vm), &name, &format!("Started VM '{}'", name), "Done");
                            // running now, before the refresh says so (its console opens straight away)
                            if let Some(v) = self.server_mut(sid).and_then(|s| s.data.as_mut()).and_then(|d| d.vms.iter_mut().find(|v| v.rref == vm)) {
                                v.power = PowerState::Running;
                            }
                            if watching {
                                self.dialog = Dialog::None;
                                self.pending_select = Some((format!("vm:{}:{}", sid, vm), Tab::Console));
                            }
                        }
                        Err(e) => {
                            let why = create_problem(&e);
                            self.add_log(sid, &server, Some(&vm), &name, &format!("Couldn't start VM '{}': {}", name, why), "Failed");
                            match &mut self.dialog {
                                Dialog::NewVm(d) if watching => d.not_started(why),
                                // its dialog was closed: said here, unless another dialog is open
                                Dialog::None => self.dialog = Dialog::Message { title: "New VM".into(), text: format!("'{}' was created, but it couldn't start.\n\n{}", name, why) },
                                _ => {}
                            }
                        }
                    }
                    if let Some(s) = self.server_mut(sid) {
                        s.next_refresh = Instant::now();
                    }
                }
                Msg::IsoDeleted { sid, sr, vdi, name, result } => {
                    self.iso_deleting.remove(&vdi);
                    let server = self.server(sid).map(|s| s.name()).unwrap_or_default();
                    let lib = self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.sr(&sr)).map(|x| x.name.clone()).unwrap_or_default();
                    match result {
                        Ok(()) => {
                            // gone from the list right away; the next refresh agrees
                            if let Some(x) = self.server_mut(sid).and_then(|s| s.data.as_mut()).and_then(|d| d.srs.iter_mut().find(|x| x.rref == sr)) {
                                x.files.retain(|f| f.vdi != vdi);
                            }
                            self.add_log(sid, &server, Some(&sr), &lib, &format!("Deleted '{}' from '{}'", name, lib), "Done");
                        }
                        Err(e) => {
                            let why = delete_problem(&e);
                            self.add_log(sid, &server, Some(&sr), &lib, &format!("Couldn't delete '{}' from '{}': {}", name, lib, why), "Failed");
                            self.dialog = Dialog::Message { title: "Delete ISO File".into(), text: format!("Couldn't delete '{}'.\n\n{}", name, why) };
                        }
                    }
                    if let Some(s) = self.server_mut(sid) {
                        s.next_refresh = Instant::now();
                    }
                }
                Msg::VmDeleted { sid, vm, name, result } => {
                    self.vm_deleting.remove(&vm);
                    let server = self.server(sid).map(|s| s.name()).unwrap_or_default();
                    match result {
                        Ok(left) => {
                            // gone from the tree right away (the next refresh agrees), its server shown
                            if let Some(d) = self.server_mut(sid).and_then(|s| s.data.as_mut()) {
                                d.vms.retain(|v| v.rref != vm);
                            }
                            if self.selected == format!("vm:{}:{}", sid, vm) {
                                self.select(format!("srv:{}", sid));
                            }
                            if left.is_empty() {
                                self.add_log(sid, &server, None, &name, &format!("Deleted VM '{}'", name), "Done");
                            } else {
                                let text = format!("Deleted VM '{}', but not all its disks: {}", name, left.join("; "));
                                self.add_log(sid, &server, None, &name, &text, "Done");
                                self.dialog = Dialog::Message { title: "Delete VM".into(), text: format!("'{}' was deleted, but these disks couldn't be:\n\n{}", name, left.join("\n")) };
                            }
                        }
                        Err(e) => {
                            let why = delete_vm_problem(&e);
                            self.add_log(sid, &server, Some(&vm), &name, &format!("Couldn't delete VM '{}': {}", name, why), "Failed");
                            self.dialog = Dialog::Message { title: "Delete VM".into(), text: format!("Couldn't delete '{}'.\n\n{}", name, why) };
                        }
                    }
                    if let Some(s) = self.server_mut(sid) {
                        s.next_refresh = Instant::now();
                    }
                }
                Msg::LinkChecked { url, result } => {
                    if let Dialog::Upload(d) = &mut self.dialog {
                        d.answer(&url, result);
                    }
                }
                Msg::UploadDone { sr, result } => {
                    let Some(mut u) = self.uploads.remove(&sr) else { continue };
                    let verb = if u.web { "download" } else { "upload" };
                    let (status, text) = match &result {
                        Ok(()) if u.web => ("Done", format!("Downloaded '{}' from {} into '{}'", u.name, u.from, u.lib)),
                        Ok(()) => ("Done", format!("Uploaded '{}' to '{}'", u.name, u.lib)),
                        Err(e) if e.code == CANCELLED => ("Cancelled", format!("Cancelled the {} of '{}' to '{}'", verb, u.name, u.lib)),
                        Err(e) => ("Failed", format!("Couldn't {} '{}' to '{}': {}", verb, u.name, u.lib, upload_problem(e))),
                    };
                    if let Some(entry) = self.log.get_mut(u.log) {
                        entry.status = status.into();
                        entry.text = text;
                    }
                    if let (Err(e), "Failed") = (&result, status) {
                        self.dialog = Dialog::Message { title: "Upload".into(), text: format!("Couldn't {} '{}' to '{}'.\n\n{}", verb, u.name, u.lib, upload_problem(e)) };
                    }
                    if let Some(s) = self.server_mut(u.sid) {
                        s.next_refresh = Instant::now(); // the new disc shows up
                    }
                    if result.is_ok() {
                        // its card stays a moment, green
                        u.done = Some(u.started.elapsed());
                        u.ended = Some(Instant::now());
                        self.uploads.insert(sr, u);
                    }
                }
                Msg::DescriptionDone { sid, sr, vdi, name, text, result } => {
                    self.iso_saving.remove(&vdi);
                    let server = self.server(sid).map(|s| s.name()).unwrap_or_default();
                    match result {
                        Ok(()) => {
                            // show it right away; the next refresh reads it back from the server
                            if let Some(f) = self.server_mut(sid).and_then(|s| s.data.as_mut()).and_then(|d| d.srs.iter_mut().find(|x| x.rref == sr)).and_then(|x| x.files.iter_mut().find(|f| f.vdi == vdi)) {
                                f.description = text;
                            }
                            self.add_log(sid, &server, Some(&sr), &name, &format!("Changed the description of '{}'", name), "Done");
                        }
                        Err(e) => {
                            self.add_log(sid, &server, Some(&sr), &name, &format!("Couldn't change the description of '{}': {}", name, e.message()), "Failed");
                            self.dialog = Dialog::Message { title: "Description".into(), text: format!("Couldn't change the description of '{}'.\n\n{}", name, e.message()) };
                        }
                    }
                    if let Some(s) = self.server_mut(sid) {
                        s.next_refresh = Instant::now();
                    }
                }
                Msg::KeychainError(e) => {
                    self.dialog = Dialog::Message { title: "Remember password".into(), text: format!("Couldn't save the password in the system's keychain.\n\n{}", e) };
                }
                Msg::ActionDone { sid, vm, log, err, label, op, text } => {
                    if let Some(s) = self.server_mut(sid) {
                        s.busy.remove(&vm);
                        s.next_refresh = Instant::now(); // re-read right away
                        // its new state shown now, not the old one until that refresh comes
                        let now = match op {
                            "start" | "resume" | "clean_reboot" | "hard_reboot" => Some(PowerState::Running),
                            "clean_shutdown" | "hard_shutdown" => Some(PowerState::Halted),
                            "suspend" => Some(PowerState::Suspended),
                            _ => None,
                        };
                        if let (None, Some(p)) = (&err, now) {
                            if let Some(v) = s.data.as_mut().and_then(|d| d.vms.iter_mut().find(|v| v.rref == vm)) {
                                v.power = p;
                            }
                        }
                    }
                    if let Some(e) = self.log.get_mut(log) {
                        e.status = if err.is_none() { "Done".into() } else { "Failed".into() };
                        if let Some(er) = &err {
                            e.text = format!("{}: {}", e.text, er);
                        }
                    }
                    if let (Some(er), None) = (err, &self.shot_path) {
                        self.dialog = Dialog::Message { title: label.into(), text: format!("{} failed.\n\n{}", text, er) };
                    }
                }
            }
        }
    }

    fn on_fetched(&mut self, id: u32, r: Result<PoolData, ApiError>) {
        let shot = self.shot_path.is_some();
        let Some(idx) = self.servers.iter().position(|s| s.id == id) else { return };
        match r {
            Ok(d) => {
                let s = &mut self.servers[idx];
                s.fetching = false;
                let first = !s.ever;
                let was_reconnecting = s.state == ConnState::Reconnecting;
                let host_keys: Vec<String> = d.hosts.iter().map(|h| format!("host:{}:{}", id, h.rref)).collect();
                // the pool's servers' addresses, kept for logging in elsewhere should this one go
                let members: Vec<String> = d.hosts.iter().map(|h| h.address.clone()).filter(|a| !a.is_empty()).collect();
                let members_changed = s.data.as_ref().map_or(true, |old| old.hosts.iter().map(|h| h.address.clone()).filter(|a| !a.is_empty()).collect::<Vec<_>>() != members);
                if members_changed && !s.conn.is_demo() && !shot {
                    save_members(&s.conn.hostname(), &members);
                }
                s.data = Some(d);
                s.state = ConnState::Connected;
                s.error = None;
                s.failures = 0;
                s.ever = true;
                s.next_refresh = Instant::now() + Duration::from_millis(REFRESH_MS);
                let name = s.name();
                let is_demo = s.conn.is_demo();
                let (host, user) = (s.conn.hostname(), s.conn.username());
                if first {
                    if let Some(e) = self.log.iter_mut().rev().find(|e| e.sid == id && e.status == "In progress" && e.text.starts_with("Connecting to")) {
                        e.status = "Done".into();
                    }
                    self.add_log(id, &name, None, &name, &format!("Connected to {}", name), "Done");
                    if !is_demo {
                        remember_host(&host, &user);
                        self.known_hosts = load_history();
                        if let Some((remember, password)) = self.servers[idx].pending_save.take() {
                            if self.keychain != Some(false) {
                                self.keep_password(&host, &user, remember.then_some(password));
                            }
                        }
                    }
                    self.expanded.insert(format!("srv:{}", id));
                    self.expanded.extend(host_keys);
                    if shot && is_demo {
                        match self.shot_show.as_str() {
                            "home" => self.select("root".into()),
                            "storage" => self.select(format!("srv:{}", id)),
                            "isos" | "iso-disconnect" | "iso-delete" | "iso-describe" | "iso-upload" | "iso-upload-choose" | "iso-upload-web" | "iso-upload-done" => {
                                self.expanded.insert(format!("folder:{}:srs", id));
                                self.select(format!("sr:{}:sr-iso", id));
                            }
                            "search" | "logs" => self.select("root".into()),
                            "networks" => {
                                self.expanded.insert(format!("folder:{}:nets", id));
                                self.select(format!("folder:{}:nets", id));
                            }
                            "objects" => {
                                self.objects_view = true;
                                self.expanded.insert("cat:vms".into());
                                self.select(format!("vm:{}:vm-web", id));
                            }
                            _ => self.select(format!("vm:{}:vm-web", id)),
                        }
                        self.shot_ready = true;
                    }
                } else if was_reconnecting {
                    self.add_log(id, &name, None, &name, &format!("Connection to {} is back", name), "Done");
                }
            }
            Err(e) => {
                let msg = e.message();
                let s = &mut self.servers[idx];
                s.fetching = false;
                s.failures += 1;
                let host = s.conn.hostname();
                let user = s.conn.username();
                if !s.ever {
                    s.state = ConnState::Disconnected;
                    s.error = Some(msg.clone());
                    if let Some(l) = self.log.iter_mut().rev().find(|l| l.sid == id && l.status == "In progress") {
                        l.status = "Failed".into();
                    }
                    self.add_log(id, &host, None, &host, &format!("Couldn't connect to {}: {}", host, msg), "Failed");
                    if !shot {
                        if e.code == "SESSION_AUTHENTICATION_FAILED" {
                            self.remove_server(id, false);
                            self.dialog = Dialog::AddServer { server: host, user, password: String::new(), error: msg, remember: self.keychain != Some(false) };
                        } else {
                            self.dialog = Dialog::Message { title: "Connect to Server".into(), text: format!("Couldn't connect to {}.\n\n{}", host, msg) };
                        }
                    }
                } else {
                    let name = s.name();
                    let first_loss = s.state != ConnState::Reconnecting;
                    s.state = ConnState::Reconnecting;
                    s.error = Some(msg.clone());
                    // first retry after the short delay, later ones after the long one
                    let delay = if s.failures <= 1 { RECONNECT_FIRST_MS } else { RECONNECT_LATER_MS };
                    s.next_refresh = Instant::now() + Duration::from_millis(delay);
                    if first_loss {
                        self.add_log(id, &name, None, &name, &format!("Lost the connection to {}: {}", name, msg), "Reconnecting");
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------ selection

    fn select(&mut self, key: String) {
        if key == self.selected {
            return;
        }
        self.iso_edit = None;
        self.selected = key;
    }

    /// The storage a tree node stands for, with its server.
    fn sr_of<'a>(&'a self, node: &Node) -> Option<(&'a ServerEntry, &'a SrInfo)> {
        if node.kind != Kind::Sr {
            return None;
        }
        let s = self.server(node.sid?)?;
        Some((s, s.data.as_ref()?.sr(node.rref.as_deref()?)?))
    }

    fn host_of<'a>(&'a self, node: &Node) -> Option<(&'a ServerEntry, &'a HostInfo)> {
        if node.kind != Kind::Host {
            return None;
        }
        let s = self.server(node.sid?)?;
        Some((s, s.data.as_ref()?.host(node.rref.as_deref()?)?))
    }

    /// What the Console tab shows for a node: a VM's own screen, or a server's text console (its
    /// control domain's): the pool, the VM to connect to, a name and system for the demo's
    /// screen, and whether it's on (a server always is).
    fn console_target(&self, node: &Node) -> Option<(u32, String, String, String, PowerState)> {
        if let Some((s, _, vm)) = self.vm_of(node) {
            return Some((s.id, vm.rref.clone(), vm.name.clone(), vm.os.clone(), vm.power));
        }
        let (s, h) = self.host_of(node)?;
        Some((s.id, h.control_domain.clone(), h.name.clone(), format!("XenServer {}", h.version), PowerState::Running))
    }

    fn vm_of<'a>(&'a self, node: &Node) -> Option<(&'a ServerEntry, &'a PoolData, &'a VmInfo)> {
        if node.kind != Kind::Vm {
            return None;
        }
        let s = self.server(node.sid?)?;
        let d = s.data.as_ref()?;
        let v = d.vm(node.rref.as_deref()?)?;
        Some((s, d, v))
    }

    fn can_run(&self, a: &VmAction, node: &Node) -> bool {
        self.vm_of(node).map_or(false, |(s, d, v)| a.can_run(v, d, s.busy.contains_key(&v.rref)))
    }

    // ------------------------------------------------------------------ tree

    fn vm_text(_s: &ServerEntry, v: &VmInfo) -> String {
        v.name.clone()
    }
    /// A VM's icon, or, while a power action is under way, a ring turning in that action's colour.
    fn vm_icon(s: &ServerEntry, v: &VmInfo) -> IconKind {
        match s.busy.get(&v.rref) {
            Some((_, op)) => IconKind::Busy(Tone::of_action(op)),
            None => IconKind::Vm(v.power),
        }
    }

    fn build_tree(&self) -> Node {
        let mut root = Node::new("root".into(), "All servers".into(), IconKind::Home, Kind::Root, None, None);
        if self.objects_view {
            let mut cats = vec![
                Node::new("cat:pools".into(), "Pools".into(), IconKind::Pool, Kind::Category, None, None),
                Node::new("cat:hosts".into(), "Servers".into(), IconKind::Host, Kind::Category, None, None),
                Node::new("cat:vms".into(), "Virtual machines".into(), IconKind::Vm(PowerState::Running), Kind::Category, None, None),
                Node::new("cat:srs".into(), "Storage".into(), IconKind::Sr, Kind::Category, None, None),
                Node::new("cat:nets".into(), "Networks".into(), IconKind::Net, Kind::Category, None, None),
            ];
            for s in &self.servers {
                let id = s.id;
                let Some(d) = &s.data else {
                    cats[1].children.push(Node::new(format!("srv:{}", id), format!("{}  (not connected)", s.conn.hostname()), IconKind::ServerDown, Kind::Server, Some(id), None));
                    continue;
                };
                if d.is_pool() {
                    let mut pn = Node::new(format!("srv:{}", id), s.name(), IconKind::Pool, Kind::Pool, Some(id), None);
                    pn.note = s.login_note();
                    cats[0].children.push(pn);
                }
                for h in &d.hosts {
                    let icon = if h.enabled { IconKind::Host } else { IconKind::HostOff };
                    let mut hn = Node::new(format!("host:{}:{}", id, h.rref), h.name.clone(), icon, Kind::Host, Some(id), Some(h.rref.clone()));
                    if !d.is_pool() {
                        hn.note = s.login_note();
                    }
                    cats[1].children.push(hn);
                }
                for v in &d.vms {
                    cats[2].children.push(Node::new(format!("vm:{}:{}", id, v.rref), Self::vm_text(s, v), Self::vm_icon(s, v), Kind::Vm, Some(id), Some(v.rref.clone())));
                }
                for x in d.srs.iter().filter(|x| !x.is_empty_drive()) {
                    cats[3].children.push(Node::new(format!("sr:{}:{}", id, x.rref), x.name.clone(), sr_icon(x), Kind::Sr, Some(id), Some(x.rref.clone())));
                }
                for n in &d.networks {
                    cats[4].children.push(Node::new(format!("net:{}:{}", id, n.rref), n.name.clone(), IconKind::Net, Kind::Net, Some(id), Some(n.rref.clone())));
                }
            }
            for c in cats.iter_mut() {
                c.children.sort_by_key(|n| n.text.to_lowercase());
                c.text = format!("{} ({})", c.text, c.children.len());
            }
            root.children = cats;
        } else {
            for s in &self.servers {
                let id = s.id;
                let skey = format!("srv:{}", id);
                let Some(d) = &s.data else {
                    let what = if s.state == ConnState::Connecting { "connecting…" } else { "not connected" };
                    let icon = if s.state == ConnState::Connecting { IconKind::Busy(Tone::Accent) } else { IconKind::ServerDown };
                    root.children.push(Node::new(skey, format!("{}  ({})", s.conn.hostname(), what), icon, Kind::Server, Some(id), None));
                    continue;
                };
                let suffix = if s.state == ConnState::Reconnecting { "  (reconnecting…)" } else { "" };
                let mut vms: Vec<&VmInfo> = d.vms.iter().collect();
                vms.sort_by_key(|v| v.name.to_lowercase());
                let mut srs: Vec<&SrInfo> = d.srs.iter().filter(|x| !x.is_empty_drive()).collect();
                srs.sort_by_key(|x| x.name.to_lowercase());
                let mut nets: Vec<&NetInfo> = d.networks.iter().collect();
                nets.sort_by_key(|n| n.name.to_lowercase());
                let vm_node = |v: &VmInfo| Node::new(format!("vm:{}:{}", id, v.rref), Self::vm_text(s, v), Self::vm_icon(s, v), Kind::Vm, Some(id), Some(v.rref.clone()));
                let sr_node = |x: &SrInfo| Node::new(format!("sr:{}:{}", id, x.rref), x.name.clone(), sr_icon(x), Kind::Sr, Some(id), Some(x.rref.clone()));
                let folder = |what: &str, label: &str, kids: Vec<Node>| {
                    let mut f = Node::new(format!("folder:{}:{}", id, what), format!("{} ({})", label, kids.len()), IconKind::Folder, Kind::Folder, Some(id), Some(what.to_string()));
                    f.children = kids;
                    f
                };
                // Each server holds its VMs and then its own storage (local disks, DVD drives, USB);
                // the pool holds the VMs that aren't running anywhere, then the shared storage and the
                // networks, each in a folder.
                let mut top;
                if d.is_pool() {
                    let icon = if s.state == ConnState::Reconnecting { IconKind::ServerDown } else { IconKind::Pool };
                    top = Node::new(skey, format!("{}{}", s.name(), suffix), icon, Kind::Pool, Some(id), None);
                    for h in &d.hosts {
                        let icon = if h.enabled { IconKind::Host } else { IconKind::HostOff };
                        let mut hn = Node::new(format!("host:{}:{}", id, h.rref), h.name.clone(), icon, Kind::Host, Some(id), Some(h.rref.clone()));
                        hn.children.extend(vms.iter().filter(|v| v.home() == Some(h.rref.as_str())).map(|v| vm_node(v)));
                        hn.children.extend(srs.iter().filter(|x| x.host.as_deref() == Some(h.rref.as_str())).map(|x| sr_node(x)));
                        top.children.push(hn);
                    }
                    top.children.extend(vms.iter().filter(|v| v.home().and_then(|h| d.host(h)).is_none()).map(|v| vm_node(v)));
                    let shared: Vec<Node> = srs.iter().filter(|x| x.host.as_deref().and_then(|h| d.host(h)).is_none()).map(|x| sr_node(x)).collect();
                    if !shared.is_empty() {
                        top.children.push(folder("srs", "Shared Storage", shared));
                    }
                } else {
                    let h = &d.hosts[0];
                    let icon = if s.state == ConnState::Reconnecting { IconKind::ServerDown } else { IconKind::Host };
                    top = Node::new(skey, format!("{}{}", h.name, suffix), icon, Kind::Host, Some(id), Some(h.rref.clone()));
                    top.children.extend(vms.iter().map(|v| vm_node(v)));
                    top.children.extend(srs.iter().map(|x| sr_node(x)));
                }
                top.note = s.login_note();
                if !nets.is_empty() {
                    let kids = nets.iter().map(|n| Node::new(format!("net:{}:{}", id, n.rref), n.name.clone(), IconKind::Net, Kind::Net, Some(id), Some(n.rref.clone()))).collect();
                    top.children.push(folder("nets", "Virtual Networks", kids));
                }
                root.children.push(top);
            }
        }
        let f = self.search.trim().to_lowercase();
        if !f.is_empty() {
            prune(&mut root.children, &f);
        }
        root
    }

    // ------------------------------------------------------------------ actions

    fn request_action(&mut self, i: usize, sel: &Node) {
        let a = &ACTIONS[i];
        let Some((s, d, vm)) = self.vm_of(sel) else {
            return;
        };
        let busy = s.busy.contains_key(&vm.rref);
        if busy && !a.can_run(vm, d, true) {
            return;
        }
        if let Some(reason) = a.cant_run_reason(vm, d, busy) {
            self.dialog = Dialog::Message { title: a.label.into(), text: format!("Can't {} '{}'. {}", a.label.to_lowercase(), vm.name, reason) };
            return;
        }
        let (sid, vref) = (s.id, vm.rref.clone());
        if a.confirm {
            let mut q = format!("Are you sure you want to {} the VM '{}'?", a.label.to_lowercase(), vm.name);
            if a.is_force() {
                q.push_str("\n\nThis is like pulling the power plug: unsaved work inside the VM may be lost.");
            }
            self.dialog = Dialog::Confirm { action: i, sid, vm: vref, title: a.label.into(), text: q };
        } else {
            self.run_action(i, sid, vref);
        }
    }

    fn run_action(&mut self, i: usize, sid: u32, vref: String) {
        let a = &ACTIONS[i];
        let Some(s) = self.server(sid) else { return };
        let Some(vm) = s.data.as_ref().and_then(|d| d.vm(&vref)) else { return };
        let text = format!("{} VM '{}'", a.verb, vm.name);
        let (vm_name, server_name, conn) = (vm.name.clone(), s.name(), s.conn.clone());
        let log = self.add_log(sid, &server_name, Some(&vref), &vm_name, &text, "In progress");
        if let Some(s) = self.server_mut(sid) {
            s.busy.insert(vref.clone(), (format!("{}…", a.verb), a.op));
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let r = conn.run_action(&vref, &ACTIONS[i]);
            let _ = tx.send(Msg::ActionDone { sid, vm: vref, log, err: r.err().map(|e| e.message()), label: ACTIONS[i].label, op: ACTIONS[i].op, text });
        });
    }

    fn apply(&mut self, intents: Vec<Intent>, sel: &Node, ctx: &egui::Context) {
        for it in intents {
            match it {
                Intent::Select(k) => self.select(k),
                Intent::Toggle(k) => {
                    if !self.expanded.remove(&k) {
                        self.expanded.insert(k);
                    }
                }
                Intent::Action(i) => self.request_action(i, sel),
                Intent::AddServer => self.dialog = Dialog::AddServer { server: String::new(), user: "root".into(), password: String::new(), error: String::new(), remember: self.keychain != Some(false) },
                Intent::RescanSr(sid, sr) => self.sr_job(sid, sr, false, ctx),
                Intent::SaveDescription(sid, sr, vdi, name, text) => self.save_description(sid, sr, vdi, name, text, ctx),
                Intent::Upload(sid, sr) => {
                    if let Some(x) = self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.sr(&sr)) {
                        let existing = x.files.iter().map(|f| f.name.clone()).collect();
                        self.dialog = Dialog::Upload(Box::new(UploadDialog::new(sid, sr, x.name.clone(), existing)));
                    }
                }
                Intent::NewVm(sid, iso) => {
                    let sid = sid.or(sel.sid).filter(|id| self.server(*id).map_or(false, |s| s.data.is_some()));
                    let sid = sid.or_else(|| self.servers.iter().find(|s| s.data.is_some()).map(|s| s.id));
                    if let Some(d) = sid.and_then(|id| self.server(id)).and_then(|s| s.data.as_ref()) {
                        self.dialog = Dialog::NewVm(Box::new(NewVmDialog::new(sid.unwrap_or_default(), d, iso.as_deref())));
                    }
                }
                Intent::AskDeleteIso(sid, sr, vdi) => {
                    let found = self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.sr(&sr)).and_then(|x| x.files.iter().find(|f| f.vdi == vdi).map(|f| (x, f)));
                    if let Some((x, f)) = found {
                        let place = if x.location.is_empty() { String::new() } else { format!(" {}", x.location) };
                        let mut text = format!("Are you sure you want to delete '{}'?\n\nThe file ({}) is deleted from the share{}. This can't be undone.", f.name, fmt_bytes(f.size), place);
                        let parked: Vec<&str> = f.in_drives.iter().filter(|d| !d.1).map(|d| d.0.as_str()).collect();
                        if !parked.is_empty() {
                            let which = if parked.len() == 1 { "which is shut down" } else { "which are shut down" };
                            text.push_str(&format!("\n\nIt's in the DVD drive of {}, {}: the drive is left empty.", parked.join(", "), which));
                        }
                        self.dialog = Dialog::ConfirmDeleteIso { sid, sr, vdi, name: f.name.clone(), text };
                    }
                }
                Intent::AskDeleteVm(sid, vm) => {
                    if let Some(v) = self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.vm(&vm)) {
                        let disks: Vec<VmDisk> = v.disks.iter().filter(|k| k.kind == "Disk").cloned().collect();
                        let running = v.power != PowerState::Halted;
                        self.dialog = Dialog::ConfirmDeleteVm { sid, vm, name: v.name.clone(), running, disks, snapshots: v.snapshots, delete_disks: true };
                    }
                }
                Intent::DismissUpload(sr) => {
                    self.uploads.remove(&sr);
                }
                Intent::CancelUpload(sr) => {
                    if let Some(u) = self.uploads.get(&sr) {
                        u.t.cancel.store(true, Ordering::Relaxed);
                    }
                }
                Intent::Dropped(paths) => match self.sr_of(sel).filter(|(_, x)| x.is_iso()).map(|(s, x)| (s.id, x.rref.clone())) {
                    Some((sid, sr)) => {
                        let mut isos = paths.into_iter().filter(|p| p.extension().map_or(false, |e| e.eq_ignore_ascii_case("iso")));
                        match isos.next() {
                            Some(p) => self.start_upload(sid, sr, Source::File(p), None, ctx),
                            None => self.dialog = Dialog::Message { title: "Upload".into(), text: "Only .iso files can go in an ISO library.".into() },
                        }
                    }
                    None => self.dialog = Dialog::Message { title: "Upload".into(), text: "To upload a file, select an ISO library in the tree, then drop the file on the window.".into() },
                },
                Intent::AskForgetSr(sid, sr) => {
                    if let Some(x) = self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.sr(&sr)) {
                        let place = if x.location.is_empty() { String::new() } else { format!(" {}", x.location) };
                        let text = format!("Are you sure you want to disconnect the ISO library '{}'?\n\nThe pool stops using the share{}. The ISO files on it aren't deleted, and you can add it again with ISO Library.", x.name, place);
                        self.dialog = Dialog::ConfirmSr { sid, sr, text };
                    }
                }
                Intent::IsoLibrary => {
                    let pools: Vec<(u32, String)> = self.servers.iter().filter(|s| s.data.is_some()).map(|s| (s.id, s.name())).collect();
                    let sid = sel.sid.filter(|id| pools.iter().any(|p| p.0 == *id)).or(pools.first().map(|p| p.0));
                    if let Some(sid) = sid {
                        self.dialog = Dialog::Iso(Box::new(IsoDialog::new(sid, pools)));
                    }
                }
                Intent::ForgetPassword(sid) => {
                    if let Some(s) = self.server(sid) {
                        let (h, u) = (s.conn.hostname(), s.conn.username());
                        self.keep_password(&h, &u, None);
                    }
                }
                Intent::Demo => self.connect_demo(),
                Intent::Remove(sid) => {
                    if let Some(s) = self.server(sid).filter(|s| !s.conn.is_demo()) {
                        session_remove(&s.conn.hostname(), &s.conn.username());
                    }
                    self.remove_server(sid, true);
                }
                Intent::Reconnect(sid) => {
                    if let Some(s) = self.server(sid) {
                        let (h, u) = (s.conn.hostname(), s.conn.username());
                        self.remove_server(sid, false);
                        let password = self.saved.get(&(h.clone(), u.clone())).cloned().unwrap_or_default();
                        self.dialog = Dialog::AddServer { server: h, user: u, password, error: String::new(), remember: self.keychain != Some(false) };
                    }
                }
                Intent::Refresh(only) => {
                    for s in self.servers.iter_mut().filter(|s| only.map_or(true, |o| o == s.id)) {
                        s.next_refresh = Instant::now();
                        if s.state == ConnState::Disconnected {
                            s.state = ConnState::Connecting;
                        }
                    }
                }
                Intent::View(objects) => self.objects_view = objects,
                Intent::Tab(t) => self.tab = t,
                Intent::Section(name) => {
                    if !self.closed_sections.remove(&name) {
                        self.closed_sections.insert(name);
                    }
                }
                Intent::Settings => self.dialog = Dialog::Settings { startup: load_session() },
                Intent::Sidebar(open) => {
                    self.sidebar_open = open;
                    self.save_settings();
                }
                Intent::SearchFromRail => {
                    self.sidebar_open = true;
                    self.focus_search = true;
                    self.save_settings();
                }
                Intent::About => {
                    self.dialog = Dialog::Message {
                        title: "About".into(),
                        text: format!("Simple VM Manager {}\n\n{}", env!("CARGO_PKG_VERSION"), "A simpler console for virtualization servers: add servers, browse their pools, servers, virtual machines, storage and networks, and start, stop, restart, suspend or resume virtual machines.\n\nRuns on Windows, Linux and macOS, in a light or dark theme. It talks to servers over HTTPS (port 443) and accepts their certificates without asking."),
                    }
                }
            }
        }
    }

    /// The first section starts open and the others closed, as in the original; clicks are remembered.
    /// Every section starts open; the ones folded are remembered.
    fn section_open(&self, name: &str) -> bool {
        !self.closed_sections.contains(name)
    }

    // ------------------------------------------------------------------ drawing

    fn toolbar(&self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        let pal = self.pal;
        // (A VM's power actions are next to its tabs: see tab_actions; the theme is in Settings.)
        // When the window is too narrow for every label, buttons show only their icons (the
        // label becomes a tooltip): first New VM and ISO Library (rank 2), then all of them.
        let fits = |level: u8| {
            let w = |l: &str, rank: u8| if level >= rank { 32.0 } else { tool_width(ui, l) } + 2.0;
            let buttons: f32 = w("Connect", 3) + w("New VM", 2) + w("ISO Library", 2);
            36.0 + 10.0 + 16.0 + buttons + 160.0 <= ui.available_width() // 160: the app's name
        };
        let level = (0..3).find(|&l| fits(l)).unwrap_or(3);
        let compact = |rank: u8| level >= rank;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if icon_button(ui, "settings", &pal, true, false).on_hover_text("Settings").clicked() {
                out.push(Intent::Settings);
            }
            ui.add_space(4.0);
            if tool_button(ui, "connect", "Connect", true, compact(3), &pal).clicked() {
                out.push(Intent::AddServer);
            }
            ui.separator();
            let has_pool = self.servers.iter().any(|s| s.data.is_some());
            if tool_button(ui, "newvm", "New VM", has_pool, compact(2), &pal).on_hover_text("Create a virtual machine that installs from an ISO file").clicked() {
                out.push(Intent::NewVm(sel.sid, None));
            }
            if tool_button(ui, "iso", "ISO Library", has_pool, compact(2), &pal).on_hover_text("Add a library of installation discs (.iso files) from a Windows or NFS share").clicked() {
                out.push(Intent::IsoLibrary);
            }
            // the app's name in the right corner; a click shows About
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                let font = FontId::new(15.5, FontFamily::Name("bold".into()));
                let text = egui::RichText::new("Simple VM Manager").font(font).color(pal.ink);
                let name = ui.add(egui::Label::new(text).sense(Sense::click())).on_hover_text("About Simple VM Manager").on_hover_cursor(egui::CursorIcon::PointingHand);
                if name.clicked() {
                    out.push(Intent::About);
                }
            });
        });
    }

    /// The unfolded sidebar: search box, the Infrastructure / Objects switch, and the resource tree.
    fn sidebar(&mut self, ui: &mut Ui, tree: &Node, out: &mut Vec<Intent>) {
        let pal = self.pal;
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::Frame::none().inner_margin(egui::Margin::symmetric(6.0, 6.0)).show(ui, |ui| {
            ui.horizontal(|ui| {
                let w = ui.available_width() - 30.0;
                egui::Frame::none()
                    .fill(pal.input)
                    .stroke(Stroke::new(1.0, pal.border_strong))
                    .rounding(Rounding::same(6.0))
                    .inner_margin(egui::Margin::symmetric(6.0, 3.0))
                    .show(ui, |ui| {
                        ui.set_width(w - 14.0);
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(vec2(16.0, 18.0), Sense::hover());
                            paint_search(ui.painter(), r, pal.ink_soft);
                            let resp = ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search...").frame(false).desired_width(f32::INFINITY));
                            if self.focus_search {
                                resp.request_focus();
                                self.focus_search = false;
                            }
                        });
                    });
                if chevron_button(ui, &pal, true).on_hover_text("Fold the sidebar").clicked() {
                    out.push(Intent::Sidebar(false));
                }
            });
            ui.add_space(8.0);
            // Infrastructure | Objects: a two-part switch, the chosen half raised.
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
            ui.painter().rect_filled(r, Rounding::same(7.0), pal.rail);
            let half = r.width() / 2.0;
            for (i, (label, objects)) in [("Infrastructure", false), ("Objects", true)].into_iter().enumerate() {
                let hr = Rect::from_min_size(pos2(r.min.x + i as f32 * half, r.min.y), vec2(half, r.height())).shrink(2.5);
                let resp = ui.interact(hr, ui.id().with(("view", label)), Sense::click());
                let on = self.objects_view == objects;
                if on {
                    ui.painter().rect(hr, Rounding::same(5.0), pal.surface, Stroke::new(1.0, pal.border));
                }
                let col = if on || resp.hovered() { pal.ink } else { pal.ink_soft };
                ui.painter().text(hr.center(), Align2::CENTER_CENTER, label, FontId::proportional(12.5), col);
                if resp.clicked() {
                    out.push(Intent::View(objects));
                }
            }
        });
        let tree_h = ui.available_height().max(50.0);
        egui::ScrollArea::both().max_height(tree_h).min_scrolled_height(tree_h).auto_shrink([false; 2]).show(ui, |ui| {
            ui.add_space(2.0);
            self.tree_ui(ui, tree, 0, out);
        });
    }

    /// The folded sidebar: a narrow strip of icon buttons.
    fn rail(&self, ui: &mut Ui, out: &mut Vec<Intent>) {
        let pal = self.pal;
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.add_space(6.0);
            if icon_button(ui, "sidebar", &pal, true, false).on_hover_text("Show the sidebar").clicked() {
                out.push(Intent::Sidebar(true));
            }
            if rail_icon(ui, &pal, RailIcon::Search, false).on_hover_text("Search").clicked() {
                out.push(Intent::SearchFromRail);
            }
            ui.add_space(4.0);
            if rail_icon(ui, &pal, RailIcon::Icon(IconKind::Home), !self.objects_view).on_hover_text("Infrastructure").clicked() {
                out.push(Intent::View(false));
                out.push(Intent::Sidebar(true));
            }
            if rail_icon(ui, &pal, RailIcon::Icon(IconKind::Vm(PowerState::Running)), self.objects_view).on_hover_text("Objects").clicked() {
                out.push(Intent::View(true));
                out.push(Intent::Sidebar(true));
            }
            ui.add_space(4.0);
            if rail_icon(ui, &pal, RailIcon::Tool("connect"), false).on_hover_text("Connect").clicked() {
                out.push(Intent::AddServer);
            }
        });
    }

    fn tree_ui(&self, ui: &mut Ui, node: &Node, depth: usize, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let row_h = 22.0;
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width().max(200.0), row_h), Sense::click());
        let p = ui.painter();
        let x0 = rect.min.x + 6.0 + depth as f32 * 16.0;
        let selected = node.key == self.selected;
        let has_kids = !node.children.is_empty();
        let open = has_kids && (self.expanded.contains(&node.key) || !self.search.trim().is_empty());
        let font = FontId::proportional(13.0);
        let mut tw = p.layout_no_wrap(node.text.clone(), font.clone(), pal.ink).size().x;
        let note_x = tw + 8.0;
        if !node.note.is_empty() {
            tw += 8.0 + p.layout_no_wrap(node.note.clone(), font.clone(), pal.ink_soft).size().x;
        }
        let row = Rect::from_min_size(pos2(x0 + 14.0, rect.min.y + 1.0), vec2((tw + 32.0).min(rect.max.x - x0 - 16.0), row_h - 2.0));
        let bg = if selected {
            pal.accent
        } else if resp.hovered() {
            pal.hover
        } else {
            pal.sidebar
        };
        if selected || resp.hovered() {
            p.rect_filled(row, Rounding::same(5.0), bg);
        }
        if has_kids {
            let c = pos2(x0 + 5.0, rect.center().y);
            let s = Stroke::new(1.4, pal.ink_soft);
            let pts = if open {
                vec![c + vec2(-3.5, -1.5), c + vec2(0.0, 2.0), c + vec2(3.5, -1.5)]
            } else {
                vec![c + vec2(-1.5, -3.5), c + vec2(2.0, 0.0), c + vec2(-1.5, 3.5)]
            };
            p.add(egui::Shape::line(pts, s));
        }
        let icon_bg = if selected || resp.hovered() { bg } else { pal.sidebar };
        paint_icon(p, Rect::from_min_size(pos2(x0 + 18.0, rect.min.y + 3.0), vec2(16.0, 16.0)), node.icon, &pal, icon_bg);
        let text_col = if selected { pal.on_accent } else { pal.ink };
        p.text(pos2(x0 + 40.0, rect.center().y), Align2::LEFT_CENTER, &node.text, font.clone(), text_col);
        // The note is cut short with "…" when the sidebar is too narrow for it.
        let note_room = rect.max.x - (x0 + 40.0 + note_x) - 6.0;
        if !node.note.is_empty() && note_room > 24.0 {
            let note_col = if selected { pal.on_accent.gamma_multiply(0.8) } else { pal.ink_soft };
            let mut job = egui::text::LayoutJob::simple_singleline(node.note.clone(), font, note_col);
            job.wrap = egui::text::TextWrapping { max_width: note_room, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
            let g = p.layout_job(job);
            p.galley(pos2(x0 + 40.0 + note_x, rect.center().y - g.size().y / 2.0), g, note_col);
        }
        if resp.clicked() {
            let on_chevron = has_kids && resp.interact_pointer_pos().map_or(false, |pp| pp.x < x0 + 13.0);
            out.push(if on_chevron { Intent::Toggle(node.key.clone()) } else { Intent::Select(node.key.clone()) });
        }
        if resp.double_clicked() && has_kids {
            out.push(Intent::Toggle(node.key.clone()));
        }
        if resp.secondary_clicked() {
            out.push(Intent::Select(node.key.clone()));
        }
        resp.context_menu(|ui| match node.kind {
            Kind::Vm => {
                for (i, a) in ACTIONS.iter().enumerate() {
                    if ui.add_enabled(self.can_run(a, node), egui::Button::new(a.label).shortcut_text(a.shortcut.text())).clicked() {
                        out.push(Intent::Select(node.key.clone()));
                        out.push(Intent::Action(i));
                        ui.close_menu();
                    }
                }
                if let (Some(sid), Some(vm)) = (node.sid, node.rref.clone()) {
                    ui.separator();
                    let pal = self.pal;
                    let deleting = self.vm_deleting.contains(&vm);
                    if ui.add_enabled(!deleting, egui::Button::new(egui::RichText::new("Delete VM…").color(pal.red))).clicked() {
                        out.push(Intent::Select(node.key.clone()));
                        out.push(Intent::AskDeleteVm(sid, vm));
                        ui.close_menu();
                    }
                }
            }
            Kind::Root | Kind::Category => {
                if ui.button("Connect…").clicked() {
                    out.push(Intent::AddServer);
                    ui.close_menu();
                }
                if ui.button("Connect to Demo Server").clicked() {
                    out.push(Intent::Demo);
                    ui.close_menu();
                }
            }
            _ => {
                if let Some(sid) = node.sid {
                    let st = self.server(sid).map(|s| (s.state, s.conn.is_demo()));
                    if let Some((ConnState::Disconnected, false)) = st {
                        if ui.button("Connect again…").clicked() {
                            out.push(Intent::Reconnect(sid));
                            ui.close_menu();
                        }
                    }
                    let has_data = self.server(sid).map_or(false, |s| s.data.is_some());
                    if has_data && ui.button("New VM…").clicked() {
                        out.push(Intent::NewVm(Some(sid), None));
                        ui.close_menu();
                    }
                    if ui.button("Refresh Now").clicked() {
                        out.push(Intent::Refresh(Some(sid)));
                        ui.close_menu();
                    }
                    let has_saved = self.server(sid).map_or(false, |s| self.saved.contains_key(&(s.conn.hostname(), s.conn.username())));
                    if has_saved && ui.button("Forget saved password").clicked() {
                        out.push(Intent::ForgetPassword(sid));
                        ui.close_menu();
                    }
                    let label = if matches!(st, Some((ConnState::Disconnected, _))) { "Remove" } else { "Disconnect" };
                    if ui.button(label).clicked() {
                        out.push(Intent::Remove(sid));
                        ui.close_menu();
                    }
                }
            }
        });
        if open {
            for c in &node.children {
                self.tree_ui(ui, c, depth + 1, out);
            }
        }
    }

    /// The selected item's name (bold, with softer details beside it) and its tabs, on one line when
    /// they fit and on two otherwise. Returns the room left right of the tabs.
    /// The connection whose own page (its pool, or its one server) `sel` is, and whether it's
    /// connected (not just listed after failing).
    fn root_of(&self, sel: &Node) -> Option<(u32, bool)> {
        let sid = sel.sid?;
        if !matches!(sel.kind, Kind::Pool | Kind::Host) || sel.key != format!("srv:{}", sid) {
            return None;
        }
        self.server(sid).map(|s| (sid, s.state != ConnState::Disconnected))
    }

    /// How wide the buttons at the right end of the tab row are, as icons (0: none): a VM's power
    /// actions and Delete, a connection's Disconnect, and a console's keyboard.
    fn tab_actions_width(&self, sel: &Node, current: Tab) -> f32 {
        let vm = self.vm_of(sel).map(|x| x.2);
        let mut w = 0.0;
        if let Some(v) = vm {
            w += ACTIONS.iter().filter(|a| a.on_toolbar(Some(v))).map(|a| 34.0 + if force_of(a.op).is_some() { ARROW_W } else { 0.0 }).sum::<f32>();
            w += 11.0 + 32.0; // a separator, then Delete
        }
        if self.root_of(sel).is_some() {
            w += 32.0; // Disconnect (with its name when there's room)
        }
        if current == Tab::Console && self.console.is_some() {
            w += 32.0 + if vm.is_some() || self.root_of(sel).is_some() { 11.0 } else { 0.0 };
        }
        if w > 0.0 {
            w + 8.0
        } else {
            0.0
        }
    }

    /// At the right end of the tab row, as icons: a console's keyboard (its menu has Ctrl+Alt+Del
    /// and the other keys the desktop keeps), then a VM's power actions (in their colours; Force
    /// Shut Down and Force Reboot behind the small arrows) and Delete. When the row hasn't the
    /// `need`ed room, all of it is behind one "more" button instead, with names.
    fn tab_actions(&mut self, ui: &mut Ui, free: Rect, sel: &Node, current: Tab, need: f32, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let vm = self.vm_of(sel).map(|(s, _, v)| (s.id, v.rref.clone()));
        // each power action shown: which, whether it can run, and its Force partner's
        let actions: Vec<(usize, bool, Option<(usize, bool)>)> = match self.vm_of(sel).map(|x| x.2) {
            Some(v) => ACTIONS
                .iter()
                .enumerate()
                .filter(|(_, a)| a.on_toolbar(Some(v)))
                .map(|(i, a)| (i, self.can_run(a, sel), force_of(a.op).map(|f| (f, self.can_run(&ACTIONS[f], sel)))))
                .collect(),
            None => vec![],
        };
        let deleting = vm.as_ref().map_or(false, |(_, r)| self.vm_deleting.contains(r));
        let root = self.root_of(sel).map(|(sid, connected)| (sid, if connected { "Disconnect" } else { "Remove" }, self.server(sid).map(|s| s.name()).unwrap_or_default()));
        let mut console = if current == Tab::Console { self.console.as_mut() } else { None };
        let tip = |i: usize| match ACTIONS[i].shortcut.text() {
            "" => ACTIONS[i].label.to_string(),
            k => format!("{}  ({})", ACTIONS[i].label, k),
        };
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(free).layout(egui::Layout::right_to_left(egui::Align::Center)));
        let ui = &mut child;
        ui.spacing_mut().item_spacing.x = 2.0;
        if let Some((sid, label, name)) = root {
            // a connection's own page: Disconnect at the end (its name too when there's room)
            let wide = free.width() >= need - 32.0 + flat_button_width(ui, label) + 4.0;
            let tip = format!("{} {}", if label == "Remove" { "Remove" } else { "Disconnect from" }, name);
            if flat_button(ui, "disconnect", label, Some(pal.red), true, !wide, &pal).on_hover_text(tip).clicked() {
                out.push(Intent::Remove(sid));
            }
            if let Some(c) = console.as_deref_mut() {
                vsep(ui, &pal);
                keys_button(ui, &pal, c);
            }
            return;
        }
        if free.width() >= need {
            // right to left: Delete, the power actions, the keyboard
            if let Some((sid, vref)) = &vm {
                if flat_button(ui, "trash", "Delete VM…", Some(pal.red), !deleting, true, &pal).clicked() {
                    out.push(Intent::AskDeleteVm(*sid, vref.clone()));
                }
                vsep(ui, &pal);
                for &(i, ok, force) in actions.iter().rev() {
                    let clicked = match force {
                        Some((fi, fok)) => split_tool(ui, &pal, ACTIONS[i].op, &tip(i), ok, fi, fok, out),
                        None => tool_button(ui, ACTIONS[i].op, &tip(i), ok, true, &pal).clicked(),
                    };
                    if clicked {
                        out.push(Intent::Action(i));
                    }
                }
            }
            if let Some(c) = console.as_deref_mut() {
                if vm.is_some() {
                    vsep(ui, &pal);
                }
                keys_button(ui, &pal, c);
            }
            return;
        }
        // no room: one button with all of it
        let slot = ui.painter().add(egui::Shape::Noop);
        let m = egui::menu::menu_custom_button(ui, egui::Button::new("").frame(false).min_size(vec2(30.0, 26.0)), |ui| {
            ui.set_min_width(200.0);
            if let Some(c) = console.as_deref_mut() {
                ui.add_enabled_ui(c.connected(), |ui| ui.menu_button("Send keys", |ui| c.keys_menu(ui)));
                if vm.is_some() {
                    ui.separator();
                }
            }
            for &(i, ok, force) in &actions {
                for (j, can) in std::iter::once((i, ok)).chain(force) {
                    let a = &ACTIONS[j];
                    if ui.add_enabled(can, egui::Button::new(a.label).shortcut_text(a.shortcut.text())).clicked() {
                        out.push(Intent::Action(j));
                        ui.close_menu();
                    }
                }
            }
            if let Some((sid, vref)) = &vm {
                ui.separator();
                if ui.add_enabled(!deleting, egui::Button::new(egui::RichText::new("Delete VM…").color(pal.red))).clicked() {
                    out.push(Intent::AskDeleteVm(*sid, vref.clone()));
                    ui.close_menu();
                }
            }
        });
        let r = m.response.rect;
        if m.response.hovered() || m.inner.is_some() {
            ui.painter().set(slot, egui::epaint::RectShape::filled(r, Rounding::same(5.0), pal.hover));
        }
        paint_glyph(ui.painter(), Rect::from_center_size(r.center(), vec2(16.0, 16.0)), "more", pal.ink_soft);
        m.response.on_hover_text(if vm.is_some() { "Power, Delete and more" } else { "Send keys" });
    }

    /// The header and the tabs, on one line when they and `actions` (the width the buttons at
    /// the row's right end want) fit, otherwise the tabs on a line of their own.
    fn header_and_tabs(&self, ui: &mut Ui, sel: &Node, tabs: &[Tab], current: Tab, actions: f32, out: &mut Vec<Intent>) -> Rect {
        let pal = self.pal;
        let (title, sub) = self.header_text(sel);
        let (title_font, sub_font, tab_font) = (FontId::new(16.0, FontFamily::Name("bold".into())), FontId::proportional(13.0), FontId::proportional(13.5));
        let width = |t: &str, f: &FontId| ui.painter().layout_no_wrap(t.to_string(), f.clone(), pal.ink).size().x;
        let head_w = 36.0 + width(&title, &title_font) + if sub.is_empty() { 0.0 } else { 10.0 + width(&sub, &sub_font) };
        let tabs_w: f32 = tabs.iter().map(|t| width(t.label(), &tab_font) + 20.0).sum();
        let avail = ui.available_width();
        let hairline = |ui: &Ui, r: Rect| ui.painter().line_segment([r.left_bottom() - vec2(0.0, 0.5), r.right_bottom() - vec2(0.0, 0.5)], Stroke::new(1.0, pal.border));
        // everything on one line, the details beside the name giving way first if need be
        let bare_w = 36.0 + width(&title, &title_font);
        let one_line = if head_w + 28.0 + tabs_w + 24.0 + actions <= avail {
            Some((head_w, sub.clone()))
        } else if actions > 0.0 && bare_w + 28.0 + tabs_w + 24.0 + actions <= avail {
            Some((bare_w, String::new()))
        } else {
            None
        };
        if let Some((used, sub)) = one_line {
            let (row, _) = ui.allocate_exact_size(vec2(avail, 46.0), Sense::hover());
            hairline(ui, row);
            self.paint_header(ui, row, sel, &title, &sub, row.center().y);
            self.paint_tabs(ui, row, row.min.x + used + 28.0, tabs, current, out)
        } else {
            // the actions go after the tabs when there's room; otherwise on the name's line (whose
            // details beside the name give way if need be), still as icons; failing both, they
            // fold into one button after the tabs
            let strip_room = avail - 2.0 - tabs_w - 16.0;
            let on_head = actions > 0.0 && strip_room < actions && avail - bare_w - 14.0 >= actions;
            let sub = if on_head && avail - head_w - 14.0 < actions { String::new() } else { sub };
            let used = if sub.is_empty() { bare_w } else { head_w };
            let (head, _) = ui.allocate_exact_size(vec2(avail, 38.0), Sense::hover());
            self.paint_header(ui, head, sel, &title, &sub, head.center().y + 2.0);
            let (strip, _) = ui.allocate_exact_size(vec2(avail, 32.0), Sense::hover());
            hairline(ui, strip);
            let after_tabs = self.paint_tabs(ui, strip, strip.min.x + 2.0, tabs, current, out);
            if on_head {
                Rect::from_min_max(pos2(head.min.x + used + 8.0, head.min.y + 4.0), pos2(head.max.x - 6.0, head.max.y))
            } else {
                after_tabs
            }
        }
    }

    fn paint_header(&self, ui: &Ui, r: Rect, sel: &Node, title: &str, sub: &str, mid: f32) {
        let pal = self.pal;
        let p = ui.painter();
        paint_icon(p, Rect::from_center_size(pos2(r.min.x + 20.0, mid), vec2(16.0, 16.0)), sel.icon, &pal, pal.window);
        let g = p.layout_no_wrap(title.to_string(), FontId::new(16.0, FontFamily::Name("bold".into())), pal.ink);
        let w = g.size().x;
        p.galley(pos2(r.min.x + 36.0, mid - g.size().y / 2.0), g, pal.ink);
        p.text(pos2(r.min.x + 46.0 + w, mid + 1.0), Align2::LEFT_CENTER, sub, FontId::proportional(13.0), pal.ink_soft);
    }

    /// The header's title and the details shown beside it.
    fn header_text(&self, sel: &Node) -> (String, String) {
        let s = sel.sid.and_then(|id| self.server(id));
        let recon = s.map_or(false, |s| s.state == ConnState::Reconnecting).then(|| "reconnecting…".to_string());
        let join = |parts: Vec<Option<String>>| parts.into_iter().flatten().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("  ·  ");
        match (sel.kind, s.and_then(|s| s.data.as_ref().map(|d| (s, d)))) {
            (Kind::Root, _) => ("All servers".into(), String::new()),
            (Kind::Category, _) => (sel.text.clone(), String::new()),
            (Kind::Folder, Some((s, _))) => {
                let what = if sel.rref.as_deref() == Some("nets") { "Virtual Networks" } else { "Shared Storage" };
                (what.into(), join(vec![Some(format!("in {}", s.name())), recon]))
            }
            (Kind::Vm, Some((_, d))) => {
                let v = sel.rref.as_deref().and_then(|r| d.vm(r));
                let on = v.and_then(|v| v.host.as_deref()).and_then(|h| d.host(h)).map(|h| format!("on {}", h.name));
                // (a power action under way shows as the icon turning)
                (v.map_or_else(|| sel.text.clone(), |v| v.name.clone()), join(vec![on, recon]))
            }
            (Kind::Pool, Some((s, d))) => (s.name(), join(vec![Some(format!("{} servers", d.hosts.len())), recon])),
            (Kind::Host, Some((_, d))) => {
                let h = sel.rref.as_deref().and_then(|r| d.host(r));
                let role = h.filter(|h| h.is_master).map(|_| "pool coordinator".to_string());
                (h.map(|h| h.name.clone()).unwrap_or_default(), join(vec![role, recon]))
            }
            (_, Some(_)) => (sel.text.clone(), join(vec![recon])),
            (_, None) => {
                let what = if s.map_or(false, |s| s.state == ConnState::Connecting) { "connecting…" } else { "not connected" };
                (s.map(|s| s.conn.hostname()).unwrap_or_default(), what.into())
            }
        }
    }

    fn tabs_for(&self, sel: &Node) -> Vec<Tab> {
        let has_data = sel.sid.and_then(|id| self.server(id)).map_or(false, |s| s.data.is_some());
        match sel.kind {
            Kind::Root | Kind::Category => vec![Tab::Home, Tab::Search, Tab::Logs],
            Kind::Sr if self.sr_of(sel).map_or(false, |(_, x)| x.is_iso()) => vec![Tab::General, Tab::IsoFiles, Tab::Logs],
            Kind::Folder if sel.rref.as_deref() == Some("nets") => vec![Tab::Networking],
            Kind::Folder => vec![Tab::Storage],
            _ if !has_data => vec![Tab::General, Tab::Logs],
            Kind::Vm => vec![Tab::General, Tab::Memory, Tab::Storage, Tab::Networking, Tab::Console, Tab::Logs],
            Kind::Host => vec![Tab::General, Tab::Memory, Tab::Storage, Tab::Networking, Tab::Console, Tab::Logs],
            Kind::Pool => vec![Tab::General, Tab::Memory, Tab::Storage, Tab::Networking, Tab::Logs],
            _ => vec![Tab::General, Tab::Logs],
        }
    }

    /// The tabs as text from `x` along `row`, the current one underlined in the accent colour;
    /// returns the room left on their right.
    fn paint_tabs(&self, ui: &mut Ui, row: Rect, x: f32, tabs: &[Tab], current: Tab, out: &mut Vec<Intent>) -> Rect {
        let pal = self.pal;
        let font = FontId::proportional(13.5);
        let mut x = x;
        for t in tabs {
            let tw = ui.painter().layout_no_wrap(t.label().into(), font.clone(), pal.ink).size().x;
            let r = Rect::from_min_size(pos2(x, row.min.y), vec2(tw + 20.0, row.height()));
            let resp = ui.interact(r, ui.id().with(t.label()), Sense::click());
            let on = *t == current;
            let col = if on || resp.hovered() { pal.ink } else { pal.ink_soft };
            ui.painter().text(pos2(r.center().x, r.center().y - 1.0), Align2::CENTER_CENTER, t.label(), font.clone(), col);
            if on {
                let line = Rect::from_center_size(pos2(r.center().x, row.max.y - 1.0), vec2(tw + 4.0, 2.5));
                ui.painter().rect_filled(line, Rounding::same(1.25), pal.accent);
            }
            if resp.clicked() {
                out.push(Intent::Tab(*t));
            }
            x += r.width();
        }
        Rect::from_min_max(pos2(x + 10.0, row.min.y + 2.0), pos2(row.max.x - 6.0, row.max.y - 3.0))
    }

    /// The Home tab: an overview of everything connected, or a first step when nothing is.
    fn home(&self, ui: &mut Ui, out: &mut Vec<Intent>) {
        if self.servers.is_empty() {
            self.welcome(ui, out);
            return;
        }
        let pal = self.pal;
        let connected: Vec<&ServerEntry> = self.servers.iter().filter(|s| s.data.is_some()).collect();
        let hosts: Vec<(&ServerEntry, &HostInfo)> = connected.iter().flat_map(|s| s.data.iter().flat_map(move |d| d.hosts.iter().map(move |h| (*s, h)))).collect();
        let vms: Vec<&VmInfo> = connected.iter().flat_map(|s| s.data.iter().flat_map(|d| d.vms.iter())).collect();
        let count = |p: PowerState| vms.iter().filter(|v| v.power == p).count();
        let (mem_used, mem_total) = hosts.iter().fold((0i64, 0i64), |(u, t), (_, h)| (u + (h.mem_total - h.mem_free).max(0), t + h.mem_total));
        let srs: Vec<&SrInfo> = connected.iter().flat_map(|s| s.data.iter().flat_map(|d| d.srs.iter())).filter(|x| x.size > 0 && x.kind != "iso" && x.kind != "udev").collect();
        let (st_used, st_total) = srs.iter().fold((0i64, 0i64), |(u, t), x| (u + x.used, t + x.size));
        let frac = |u: i64, t: i64| if t > 0 { (u as f32 / t as f32).clamp(0.0, 1.0) } else { 0.0 };
        let not_connected = self.servers.len() - connected.len();
        let pools = connected.len();
        let mut server_note = format!("in {} {}", pools, if pools == 1 { "pool" } else { "pools" });
        if not_connected > 0 {
            server_note.push_str(&format!("  ·  {} not connected", not_connected));
        }
        let cards: [(&str, String, String, Option<f32>); 4] = [
            ("Servers", hosts.len().to_string(), server_note, None),
            ("Virtual machines", format!("{} running", count(PowerState::Running)), format!("{} in all  ·  {} off  ·  {} suspended", vms.len(), count(PowerState::Halted), count(PowerState::Suspended)), None),
            ("Memory in use", format!("{}%", (frac(mem_used, mem_total) * 100.0).round()), format!("{} of {}", fmt_bytes(mem_used), fmt_bytes(mem_total)), Some(frac(mem_used, mem_total))),
            ("Storage in use", format!("{}%", (frac(st_used, st_total) * 100.0).round()), format!("{} of {}", fmt_bytes(st_used), fmt_bytes(st_total)), Some(frac(st_used, st_total))),
        ];
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let full = ui.available_width();
            let (pad, gap) = (16.0, 12.0);
            let w = full - 2.0 * pad;
            ui.add_space(16.0);

            // the summary cards, as many per row as fit
            let cols = (((w + gap) / (190.0 + gap)).floor() as usize).clamp(1, 4);
            let (card_w, card_h) = ((w - gap * (cols - 1) as f32) / cols as f32, 104.0);
            let rows = (cards.len() + cols - 1) / cols;
            let (area, _) = ui.allocate_exact_size(vec2(full, rows as f32 * (card_h + gap) - gap), Sense::hover());
            let p = ui.painter().clone();
            for (i, (title, value, note, bar)) in cards.iter().enumerate() {
                let r = Rect::from_min_size(area.min + vec2(pad + (i % cols) as f32 * (card_w + gap), (i / cols) as f32 * (card_h + gap)), vec2(card_w, card_h));
                p.rect(r, Rounding::same(10.0), pal.surface, Stroke::new(1.0, pal.border));
                p.text(r.min + vec2(16.0, 16.0), Align2::LEFT_TOP, *title, FontId::proportional(12.5), pal.ink_soft);
                p.text(r.min + vec2(16.0, 36.0), Align2::LEFT_TOP, value, FontId::new(24.0, FontFamily::Name("bold".into())), pal.ink);
                let note_y = if bar.is_some() { r.max.y - 34.0 } else { r.max.y - 26.0 };
                let mut job = egui::text::LayoutJob::simple_singleline(note.clone(), FontId::proportional(12.0), pal.ink_soft);
                job.wrap = egui::text::TextWrapping { max_width: r.width() - 32.0, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                p.galley(pos2(r.min.x + 16.0, note_y), p.layout_job(job), pal.ink_soft);
                if let Some(f) = bar {
                    usage_bar(&p, Rect::from_min_size(pos2(r.min.x + 16.0, r.max.y - 16.0), vec2(r.width() - 32.0, 5.0)), *f, &pal);
                }
            }

            // every server, with its load
            ui.add_space(26.0);
            home_title(ui, "Servers", pad, &pal);
            let row_h = 44.0;
            let listed = hosts.len() + not_connected;
            let (list, _) = ui.allocate_exact_size(vec2(full, listed as f32 * row_h), Sense::hover());
            let list = Rect::from_min_max(list.min + vec2(pad, 0.0), list.max - vec2(pad, 0.0));
            ui.painter().rect(list, Rounding::same(10.0), pal.surface, Stroke::new(1.0, pal.border));
            let mut y = list.min.y;
            let mut divider = false;
            for (s, h) in &hosts {
                let r = Rect::from_min_size(pos2(list.min.x, y), vec2(list.width(), row_h));
                let resp = ui.interact(r, ui.id().with(("home-host", s.id, h.rref.as_str())), Sense::click());
                let p = ui.painter();
                if divider {
                    p.line_segment([pos2(r.min.x + 14.0, r.min.y), pos2(r.max.x - 14.0, r.min.y)], Stroke::new(1.0, pal.border));
                }
                divider = true;
                if resp.hovered() {
                    p.rect_filled(r.shrink(2.0), Rounding::same(8.0), pal.hover);
                }
                let icon = if h.enabled { IconKind::Host } else { IconKind::HostOff };
                paint_icon(p, Rect::from_center_size(pos2(r.min.x + 24.0, r.center().y), vec2(16.0, 16.0)), icon, &pal, if resp.hovered() { pal.hover } else { pal.surface });
                let g = p.layout_no_wrap(h.name.clone(), FontId::proportional(13.5), pal.ink);
                let name_w = g.size().x;
                p.galley(pos2(r.min.x + 42.0, r.center().y - g.size().y / 2.0), g, pal.ink);
                let running = s.data.as_ref().map_or(0, |d| d.vms.iter().filter(|v| v.host.as_deref() == Some(h.rref.as_str()) && v.power == PowerState::Running).count());
                let mf = frac((h.mem_total - h.mem_free).max(0), h.mem_total);
                // on the right: the memory in use; then, when there's room, the pool and its VMs
                let bar_w = (r.width() * 0.22).clamp(70.0, 140.0);
                let pct = format!("{}%", (mf * 100.0).round());
                p.text(pos2(r.max.x - 16.0, r.center().y), Align2::RIGHT_CENTER, &pct, FontId::proportional(12.5), pal.ink_soft);
                let bar = Rect::from_min_size(pos2(r.max.x - 56.0 - bar_w, r.center().y - 2.5), vec2(bar_w, 5.0));
                usage_bar(p, bar, mf, &pal);
                let detail = format!("{}  ·  {} VM{} running", s.name(), running, if running == 1 { "" } else { "s" });
                let room = bar.min.x - 20.0 - (r.min.x + 52.0 + name_w);
                if room > 60.0 {
                    let mut job = egui::text::LayoutJob::simple_singleline(detail, FontId::proportional(12.5), pal.ink_soft);
                    job.wrap = egui::text::TextWrapping { max_width: room, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                    let g = p.layout_job(job);
                    p.galley(pos2(r.min.x + 52.0 + name_w, r.center().y - g.size().y / 2.0), g, pal.ink_soft);
                }
                if resp.on_hover_text(format!("Memory in use: {} of {}", fmt_bytes((h.mem_total - h.mem_free).max(0)), fmt_bytes(h.mem_total))).clicked() {
                    out.push(Intent::Select(format!("host:{}:{}", s.id, h.rref)));
                }
                y += row_h;
            }
            for s in self.servers.iter().filter(|s| s.data.is_none()) {
                let r = Rect::from_min_size(pos2(list.min.x, y), vec2(list.width(), row_h));
                let p = ui.painter();
                if divider {
                    p.line_segment([pos2(r.min.x + 14.0, r.min.y), pos2(r.max.x - 14.0, r.min.y)], Stroke::new(1.0, pal.border));
                }
                divider = true;
                paint_icon(p, Rect::from_center_size(pos2(r.min.x + 24.0, r.center().y), vec2(16.0, 16.0)), IconKind::ServerDown, &pal, pal.surface);
                p.text(pos2(r.min.x + 42.0, r.center().y), Align2::LEFT_CENTER, s.conn.hostname(), FontId::proportional(13.5), pal.ink);
                let what = if s.state == ConnState::Connecting { "Connecting…" } else { "Not connected" };
                p.text(pos2(r.max.x - 16.0, r.center().y), Align2::RIGHT_CENTER, what, FontId::proportional(12.5), pal.ink_soft);
                y += row_h;
            }

            // the latest things that happened
            ui.add_space(26.0);
            home_title(ui, "Recent activity", pad, &pal);
            let recent: Vec<&LogEntry> = self.log.iter().rev().take(6).collect();
            let (list, _) = ui.allocate_exact_size(vec2(full, recent.len().max(1) as f32 * 36.0), Sense::hover());
            let list = Rect::from_min_max(list.min + vec2(pad, 0.0), list.max - vec2(pad, 0.0));
            let p = ui.painter();
            p.rect(list, Rounding::same(10.0), pal.surface, Stroke::new(1.0, pal.border));
            if recent.is_empty() {
                p.text(pos2(list.min.x + 16.0, list.center().y), Align2::LEFT_CENTER, "Nothing yet", FontId::proportional(13.0), pal.ink_soft);
            }
            for (i, e) in recent.iter().enumerate() {
                let r = Rect::from_min_size(pos2(list.min.x, list.min.y + i as f32 * 36.0), vec2(list.width(), 36.0));
                if i > 0 {
                    p.line_segment([pos2(r.min.x + 14.0, r.min.y), pos2(r.max.x - 14.0, r.min.y)], Stroke::new(1.0, pal.border));
                }
                p.circle_filled(pos2(r.min.x + 20.0, r.center().y), 3.5, status_color(&e.status, &pal));
                let tg = p.layout_no_wrap(e.time.clone(), FontId::proportional(12.0), pal.ink_soft);
                let tw = tg.size().x;
                p.galley(pos2(r.max.x - 16.0 - tw, r.center().y - tg.size().y / 2.0), tg, pal.ink_soft);
                let mut job = egui::text::LayoutJob::simple_singleline(e.text.clone(), FontId::proportional(13.0), pal.ink);
                job.wrap = egui::text::TextWrapping { max_width: (r.width() - 70.0 - tw).max(20.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                let g = p.layout_job(job);
                p.galley(pos2(r.min.x + 34.0, r.center().y - g.size().y / 2.0), g, pal.ink);
            }

            // ways in
            ui.add_space(22.0);
            ui.horizontal(|ui| {
                ui.add_space(pad);
                ui.spacing_mut().item_spacing.x = 18.0;
                for (label, intent) in [("Connect to a server", Intent::AddServer), ("Try the demo server", Intent::Demo), ("About this app", Intent::About)] {
                    if ui.link(egui::RichText::new(label).color(pal.link)).clicked() {
                        out.push(intent);
                    }
                }
            });
            ui.add_space(20.0);
        });
    }

    /// Home with no servers at all: what the app does, and the ways to start.
    fn welcome(&self, ui: &mut Ui, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let avail = ui.available_rect_before_wrap();
        let col_w = avail.width().min(440.0) - 32.0;
        let top = avail.min.y + (avail.height() * 0.5 - 230.0).max(24.0);
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_size(pos2(avail.center().x - col_w / 2.0, top), vec2(col_w, 420.0))).layout(egui::Layout::top_down(egui::Align::Center)));
        let ui = &mut child;
        // the picture spans about 3..168 across and 5..181 down from where it's drawn
        let (art, _) = ui.allocate_exact_size(vec2(172.0, 182.0), Sense::hover());
        paint_cloud(ui.painter(), art.min + vec2(-1.0, -4.0), &pal);
        ui.add_space(12.0);
        ui.label(egui::RichText::new("Welcome to Simple VM Manager").font(FontId::new(20.0, FontFamily::Name("bold".into()))).color(pal.ink));
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Connect to a XenServer or XCP-ng server to see its pools, servers, virtual machines, storage and networks, and to use your VMs' consoles.").color(pal.ink_soft));
        ui.add_space(20.0);
        if dialog_button(ui, &pal, "connect", "Connect to a server", Look::Primary, true) {
            out.push(Intent::AddServer);
        }
        ui.add_space(10.0);
        if ui.link(egui::RichText::new("Try the demo server").color(pal.link)).clicked() {
            out.push(Intent::Demo);
        }
        ui.add_space(4.0);
        if ui.link(egui::RichText::new("About this app").color(pal.link)).clicked() {
            out.push(Intent::About);
        }
    }

    /// Every VM of every server; a click opens it.
    fn search_tab(&self, ui: &mut Ui, out: &mut Vec<Intent>) {
        let mut keys = vec![];
        let mut rows = vec![];
        for s in &self.servers {
            let Some(d) = &s.data else { continue };
            let mut vms: Vec<&VmInfo> = d.vms.iter().collect();
            vms.sort_by_key(|v| v.name.to_lowercase());
            for v in vms {
                keys.push(format!("vm:{}:{}", s.id, v.rref));
                rows.push(vec![
                    Cell::Icon(Self::vm_icon(s, v), v.name.clone()),
                    Cell::Text(v.power.label().into()),
                    Cell::Text(s.name()),
                    Cell::Text(v.host.as_deref().and_then(|h| d.host(h)).map(|h| h.name.clone()).unwrap_or_else(|| "—".into())),
                    Cell::Text(v.vcpus.to_string()),
                    Cell::Text(fmt_bytes(v.mem_dyn_max)),
                    Cell::Text(or(&v.ip, "—").into()),
                ]);
            }
        }
        if let Some(i) = self.table(ui, "vms", &["Name", "State", "Pool or server", "Running on", "CPUs", "Memory", "IP address"], &rows, true) {
            out.push(Intent::Select(keys[i].clone()));
        }
    }

    fn general_sections(&self, sel: &Node) -> Vec<(String, Vec<(String, String)>)> {
        let mut secs: Vec<(String, Vec<(String, String)>)> = vec![];
        fn row(a: &str, b: String) -> (String, String) {
            (a.to_string(), b)
        }
        let Some(s) = sel.sid.and_then(|id| self.server(id)) else { return secs };
        let Some(d) = &s.data else {
            let status = if s.state == ConnState::Connecting { "Connecting…" } else { "Not connected" };
            let mut g = vec![row("Server", s.conn.hostname()), row("Username", s.conn.username()), row("Status", status.into())];
            if let Some(e) = &s.error {
                g.push(row("Problem", e.clone()));
            }
            secs.push(("General".into(), g));
            return secs;
        };
        let r = sel.rref.as_deref().unwrap_or("");
        match sel.kind {
            Kind::Pool => {
                let master = d.hosts.iter().find(|h| h.is_master);
                let running = d.vms.iter().filter(|v| v.power == PowerState::Running).count();
                secs.push((
                    "General".into(),
                    vec![
                        row("Name", s.name()),
                        row("Description", String::new()),
                        row("Pool coordinator", master.map(|h| h.name.clone()).unwrap_or_default()),
                        row("Version", master.map(|h| h.version.clone()).unwrap_or_default()),
                        row("Servers", d.hosts.len().to_string()),
                        row("Virtual machines", format!("{} ({} running)", d.vms.len(), running)),
                    ],
                ));
                let status = format!("{:?}{}", s.state, s.error.as_ref().map(|e| format!(": {}", e)).unwrap_or_default());
                secs.push(("Connection".into(), vec![row("Connected to", s.conn.hostname()), row("Logged in as", s.conn.username()), row("Status", status)]));
                secs.push(("Resources".into(), vec![row("Storage repositories", d.srs.len().to_string()), row("Networks", d.networks.len().to_string())]));
            }
            Kind::Host => {
                if let Some(h) = d.host(r) {
                    let running = d.vms.iter().filter(|v| v.host.as_deref() == Some(h.rref.as_str()) && v.power == PowerState::Running).count();
                    secs.push((
                        "General".into(),
                        vec![
                            row("Name", h.name.clone()),
                            row("Description", h.description.clone()),
                            row("Address", h.address.clone()),
                            row("Role", if h.is_master { "Pool coordinator".into() } else { "Pool member".into() }),
                            row("Enabled", if h.enabled { "Yes".into() } else { "No (maintenance mode)".into() }),
                            row("Version", h.version.clone()),
                            row("Running VMs", running.to_string()),
                            row("UUID", h.uuid.clone()),
                        ],
                    ));
                    secs.push(("CPUs".into(), vec![row("Count", h.cpu_count.to_string()), row("Model", h.cpu_model.clone())]));
                    secs.push(("Memory".into(), vec![row("Total", fmt_bytes(h.mem_total)), row("Free", fmt_bytes(h.mem_free))]));
                }
            }
            Kind::Vm => {
                if let Some(v) = d.vm(r) {
                    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0);
                    let running = matches!(v.power, PowerState::Running | PowerState::Paused);
                    let mut g = vec![row("Name", v.name.clone())];
                    if !v.description.is_empty() {
                        g.push(row("Description", v.description.clone()));
                    }
                    if !v.tags.is_empty() {
                        g.push(row("Tags", v.tags.join(", ")));
                    }
                    if !v.folder.is_empty() {
                        g.push(row("Folder", v.folder.clone()));
                    }
                    g.push(row("Operating system", or(&v.os, "Unknown").into()));
                    if !v.kernel.is_empty() {
                        g.push(row("Kernel", v.kernel.clone()));
                    }
                    g.push(row("UUID", v.uuid.clone()));
                    secs.push(("General".into(), g));

                    let state = s.busy.get(&v.rref).map(|b| b.0.clone()).unwrap_or_else(|| v.power.label().into());
                    let mut st = vec![row("Power state", state)];
                    let host_name = |h: Option<&str>| h.and_then(|h| d.host(h)).map(|h| h.name.clone());
                    if running {
                        st.push(row("Running on", host_name(v.host.as_deref()).unwrap_or_default()));
                        if let Some(t) = v.started {
                            st.push(row("Up for", fmt_duration(now - t)));
                        }
                    } else {
                        st.push(row("Home server", host_name(v.home()).unwrap_or_else(|| "Any server in the pool".into())));
                    }
                    st.push(row(if v.ips.len() > 1 { "IP addresses" } else { "IP address" }, if v.ips.is_empty() { or(&v.ip, "None").into() } else { v.ips.join(", ") }));
                    let tools = match (v.tools_installed, v.tools.is_empty()) {
                        (false, _) => "Not installed".to_string(),
                        (true, true) => "Installed".to_string(),
                        (true, false) => format!("Installed ({})", v.tools),
                    };
                    st.push(row("Guest tools", tools));
                    secs.push(("Status".into(), st));

                    let cpus = if v.vcpus_max > v.vcpus { format!("{} (up to {})", v.vcpus, v.vcpus_max) } else { v.vcpus.to_string() };
                    let memory = if v.mem_dyn_min > 0 && v.mem_dyn_min < v.mem_dyn_max { format!("{} – {} (dynamic)", fmt_bytes(v.mem_dyn_min), fmt_bytes(v.mem_dyn_max)) } else { fmt_bytes(v.mem_dyn_max) };
                    let mut cm = vec![row("Virtual CPUs", cpus), row("Memory", memory)];
                    if v.mem_static_max > v.mem_dyn_max {
                        cm.push(row("Maximum memory", fmt_bytes(v.mem_static_max)));
                    }
                    if running && v.mem_actual > 0 {
                        cm.push(row("Memory now", fmt_bytes(v.mem_actual)));
                    }
                    secs.push(("CPU and memory".into(), cm));

                    let yes_no = |b: bool| if b { "Yes".to_string() } else { "No".to_string() };
                    let mut boot = vec![];
                    if !v.virt_mode.is_empty() {
                        boot.push(row("Virtualization", v.virt_mode.clone()));
                    }
                    if !v.firmware.is_empty() {
                        boot.push(row("Firmware", v.firmware.clone()));
                    }
                    if !v.secure_boot.is_empty() {
                        boot.push(row("Secure Boot", v.secure_boot.clone()));
                    }
                    if !v.boot_order.is_empty() {
                        boot.push(row("Boot order", v.boot_order.clone()));
                    }
                    boot.push(row("Starts with the server", yes_no(v.auto_power_on)));
                    boot.push(row("High availability", if v.ha.is_empty() { "Off".into() } else { v.ha.clone() }));
                    secs.push(("Boot and virtualization".into(), boot));

                    let disks: Vec<&VmDisk> = v.disks.iter().filter(|x| x.kind == "Disk").collect();
                    let total: i64 = disks.iter().map(|x| x.size).sum();
                    let dvd = v.disks.iter().find(|x| x.kind != "Disk").map(|x| if x.name.is_empty() || x.name == "(empty)" { "Empty".to_string() } else { x.name.clone() });
                    let mut nets: Vec<String> = v.nics.iter().map(|n| n.network.clone()).filter(|n| !n.is_empty()).collect();
                    nets.dedup();
                    let mut dev = vec![row("Disks", if disks.is_empty() { "None".into() } else { format!("{}  ·  {}", disks.len(), fmt_bytes(total)) })];
                    if let Some(dvd) = dvd {
                        dev.push(row("DVD drive", dvd));
                    }
                    dev.push(row("Network interfaces", if nets.is_empty() { v.nics.len().to_string() } else { format!("{}  ·  {}", v.nics.len(), nets.join(", ")) }));
                    dev.push(row("Snapshots", v.snapshots.to_string()));
                    secs.push(("Devices".into(), dev));
                }
            }
            Kind::Sr => {
                if let Some(x) = d.sr(r) {
                    let mut g = vec![row("Name", x.name.clone())];
                    if !x.description.is_empty() {
                        g.push(row("Description", x.description.clone()));
                    }
                    g.push(row("Type", if x.is_iso() { "ISO library".into() } else { x.kind.clone() }));
                    if !x.location.is_empty() {
                        g.push(row("Location", x.location.clone()));
                    }
                    if !x.share.is_empty() {
                        g.push(row("Connection", x.share.clone()));
                    }
                    if x.is_iso() {
                        g.push(row("ISO files", x.files.len().to_string()));
                    }
                    g.push(row("Shared", if x.shared { "Yes".into() } else { "No".into() }));
                    g.push(row("UUID", x.uuid.clone()));
                    secs.push(("General".into(), g));
                    let pct = if x.size > 0 { format!(" ({}%)", 100 * x.used / x.size) } else { String::new() };
                    secs.push(("Size".into(), vec![row("Size", fmt_bytes(x.size)), row("Used", format!("{}{}", fmt_bytes(x.used), pct)), row("Free", fmt_bytes((x.size - x.used).max(0)))]));
                }
            }
            Kind::Net => {
                if let Some(n) = d.net(r) {
                    secs.push((
                        "General".into(),
                        vec![row("Name", n.name.clone()), row("Description", n.description.clone()), row("Bridge", n.bridge.clone()), row("MTU", n.mtu.to_string()), row("UUID", n.uuid.clone())],
                    ));
                }
            }
            _ => {}
        }
        secs
    }

    /// The General tab: cards of details that fold, one under another, no wider than reads well.
    /// Clicking a value copies it.
    fn general(&self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        const HEAD: f32 = 38.0;
        const ROW: f32 = 25.0;
        const MAX_W: f32 = 780.0;
        let pal = self.pal;
        let secs = self.general_sections(sel);
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            let full = ui.available_width();
            let (pad, gap) = (16.0, 12.0);
            let card_w = (full - 2.0 * pad).min(MAX_W);
            let origin = ui.cursor().min + vec2(pad, 14.0);
            let mut total = 0.0;
            let mut cards = vec![];
            for (name, rows) in &secs {
                let open = self.section_open(name);
                let h = HEAD + if open { rows.len() as f32 * ROW + 10.0 } else { 0.0 };
                cards.push((Rect::from_min_size(origin + vec2(0.0, total), vec2(card_w, h)), name, rows, open));
                total += h + gap;
            }
            ui.allocate_exact_size(vec2(full, total + 24.0), Sense::hover());
            let title_font = FontId::new(13.5, FontFamily::Name("bold".into()));
            for (r, name, rows, open) in cards {
                let p = ui.painter().clone();
                p.rect(r, Rounding::same(8.0), pal.surface, Stroke::new(1.0, pal.border));
                let head = Rect::from_min_size(r.min, vec2(r.width(), HEAD));
                let hresp = ui.interact(head, ui.id().with(("sec", name.as_str())), Sense::click());
                if hresp.hovered() {
                    let round = if open { Rounding { nw: 8.0, ne: 8.0, sw: 0.0, se: 0.0 } } else { Rounding::same(8.0) };
                    p.rect_filled(head.shrink(1.0), round, pal.hover);
                }
                p.text(pos2(head.min.x + 16.0, head.center().y), Align2::LEFT_CENTER, name, title_font.clone(), pal.ink);
                let c = pos2(head.max.x - 20.0, head.center().y);
                let chev = if open { vec![c + vec2(-4.0, -2.0), c + vec2(0.0, 2.0), c + vec2(4.0, -2.0)] } else { vec![c + vec2(-2.0, -4.0), c + vec2(2.0, 0.0), c + vec2(-2.0, 4.0)] };
                p.add(egui::Shape::line(chev, Stroke::new(1.4, pal.ink_soft)));
                if hresp.clicked() {
                    out.push(Intent::Section(name.clone()));
                }
                if !open {
                    continue;
                }
                p.line_segment([pos2(r.min.x + 16.0, head.max.y), pos2(r.max.x - 16.0, head.max.y)], Stroke::new(1.0, pal.border));
                let label_w = (r.width() * 0.36).clamp(110.0, 170.0);
                let mut y = head.max.y + 5.0;
                for (k, v) in rows {
                    let rr = Rect::from_min_size(pos2(r.min.x, y), vec2(r.width(), ROW));
                    p.text(pos2(r.min.x + 16.0, rr.center().y), Align2::LEFT_CENTER, k, FontId::proportional(12.5), pal.ink_soft);
                    let vx = r.min.x + 16.0 + label_w;
                    let room = r.max.x - 14.0 - vx;
                    let mut job = egui::text::LayoutJob::simple_singleline(v.clone(), FontId::proportional(13.0), pal.ink);
                    job.wrap = egui::text::TextWrapping { max_width: room.max(10.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                    let g = p.layout_job(job);
                    let vr = Rect::from_min_size(pos2(vx, rr.center().y - g.size().y / 2.0), g.size());
                    p.galley(vr.min, g, pal.ink);
                    if !v.is_empty() {
                        let vresp = ui.interact(vr, ui.id().with(("val", name.as_str(), k.as_str())), Sense::click()).on_hover_text(format!("{}\nClick to copy", v));
                        if vresp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if vresp.clicked() {
                            ui.ctx().copy_text(v.clone());
                        }
                    }
                    y += ROW;
                }
            }
        });
    }

    fn memory(&self, ui: &mut Ui, sel: &Node) {
        let pal = self.pal;
        struct Bar {
            title: String,
            total: i64,
            parts: Vec<(String, i64)>,
            caption: String,
            dim: bool,
        }
        let mut bars: Vec<Bar> = vec![];
        if let Some(d) = sel.sid.and_then(|id| self.server(id)).and_then(|s| s.data.as_ref()) {
            let hosts: Vec<&HostInfo> = match sel.kind {
                Kind::Pool => d.hosts.iter().collect(),
                Kind::Host => sel.rref.as_deref().and_then(|r| d.host(r)).into_iter().collect(),
                _ => vec![],
            };
            for h in hosts {
                let mut parts = vec![];
                let mut vms_used = 0;
                for v in d.vms.iter().filter(|v| v.host.as_deref() == Some(h.rref.as_str()) && matches!(v.power, PowerState::Running | PowerState::Paused)) {
                    parts.push((v.name.clone(), v.mem_dyn_max));
                    vms_used += v.mem_dyn_max;
                }
                let other = (h.mem_total - h.mem_free - vms_used).max(0);
                if other > 0 {
                    parts.insert(0, ("Server itself".to_string(), other));
                }
                let caption = format!("{} used of {}  ·  {} free", fmt_bytes(h.mem_total - h.mem_free), fmt_bytes(h.mem_total), fmt_bytes(h.mem_free));
                bars.push(Bar { title: h.name.clone(), total: h.mem_total, parts, caption, dim: false });
            }
            if sel.kind == Kind::Vm {
                if let Some(v) = sel.rref.as_deref().and_then(|r| d.vm(r)) {
                    let label = if v.power == PowerState::Running { "In use by this VM" } else { "Set aside when it runs" };
                    let caption = format!("{} (can shrink to {}, at most {})", fmt_bytes(v.mem_dyn_max), fmt_bytes(v.mem_dyn_min), fmt_bytes(v.mem_static_max));
                    bars.push(Bar { title: v.name.clone(), total: v.mem_static_max.max(v.mem_dyn_max), parts: vec![(label.into(), v.mem_dyn_max)], caption, dim: v.power != PowerState::Running });
                }
            }
        }
        let palette = [pal.blue, pal.green, pal.orange, Color32::from_rgb(150, 100, 200), Color32::from_rgb(20, 160, 170), Color32::from_rgb(210, 90, 130), Color32::from_rgb(120, 140, 60)];
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            ui.add_space(12.0);
            if bars.is_empty() {
                ui.label("No memory information for this item.");
            }
            for b in bars {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(b.title).font(FontId::new(15.0, FontFamily::Name("bold".into()))));
                let (r, _) = ui.allocate_exact_size(vec2(ui.available_width() - 24.0, 26.0), Sense::hover());
                let p = ui.painter();
                p.rect_filled(r, Rounding::same(5.0), pal.stripe);
                let mut x = r.min.x;
                let mut legend = vec![];
                for (i, (name, val)) in b.parts.iter().enumerate() {
                    let pw = if b.total > 0 { (*val as f32 / b.total as f32 * r.width()).min(r.max.x - x) } else { 0.0 };
                    let mut c = if name == "Server itself" { pal.grey } else { palette[i % palette.len()] };
                    if b.dim {
                        c = c.gamma_multiply(0.6);
                    }
                    p.rect_filled(Rect::from_min_size(pos2(x, r.min.y), vec2(pw, r.height())), 0.0, c);
                    legend.push((c, format!("{}  {}", name, fmt_bytes(*val))));
                    x += pw;
                }
                p.rect_stroke(r, Rounding::same(5.0), Stroke::new(1.0, pal.border));
                ui.label(egui::RichText::new(b.caption).color(pal.ink_soft));
                ui.horizontal_wrapped(|ui| {
                    for (c, t) in legend {
                        let (sw, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                        ui.painter().rect_filled(sw, Rounding::same(2.0), c);
                        ui.label(t);
                        ui.add_space(10.0);
                    }
                });
                ui.add_space(14.0);
            }
        });
    }

    /// A table: muted headings over a hairline, rows that light up under the mouse, cells cut short
    /// with "…"; the last column takes any room left. Returns the row clicked, when `clickable`.
    fn table(&self, ui: &mut Ui, id: &str, headers: &[&str], rows: &[Vec<Cell>], clickable: bool) -> Option<usize> {
        self.table_with(ui, id, headers, rows, clickable, &mut |_, _, _, _, _| {})
    }

    /// A table whose `Cell::Custom` cells are drawn by `custom(ui, row, column, rect, row hovered)`.
    fn table_with(&self, ui: &mut Ui, id: &str, headers: &[&str], rows: &[Vec<Cell>], clickable: bool, custom: &mut dyn FnMut(&mut Ui, usize, usize, Rect, bool)) -> Option<usize> {
        let pal = self.pal;
        let (head_font, font) = (FontId::new(12.0, FontFamily::Name("bold".into())), FontId::proportional(13.0));
        let measure = |t: &str, f: &FontId| ui.painter().layout_no_wrap(t.to_string(), f.clone(), pal.ink).size().x;
        let mut widths: Vec<f32> = headers.iter().map(|h| measure(h, &head_font)).collect();
        for r in rows {
            for (i, c) in r.iter().enumerate().take(widths.len()) {
                let w = match c {
                    Cell::Text(t) => measure(t, &font),
                    Cell::Usage(_, t) => 120.0 + measure(t, &font),
                    Cell::Status(t) => 16.0 + measure(t, &font),
                    Cell::Icon(_, t) => 24.0 + measure(t, &font),
                    Cell::Custom(w) => *w,
                };
                widths[i] = widths[i].max(w.min(380.0));
            }
        }
        let pad = 14.0;
        let mut widths: Vec<f32> = widths.iter().map(|w| w + 2.0 * pad).collect();
        let mut clicked = None;
        egui::ScrollArea::both().id_salt(id).auto_shrink([false; 2]).show(ui, |ui| {
            let avail = ui.available_width();
            let sum: f32 = widths.iter().sum();
            if sum < avail {
                if let Some(last) = widths.last_mut() {
                    *last += avail - sum;
                }
            } else {
                // Too wide (a narrow window): cut the widest columns short, their text ending in "…",
                // but not below 120 points, and never a usage bar's; only past that does the table
                // scroll sideways.
                let least = 120.0 + 2.0 * pad;
                let fixed: Vec<bool> = (0..widths.len()).map(|i| rows.iter().any(|r| matches!(r.get(i), Some(Cell::Usage(..))))).collect();
                let mut over = sum - avail;
                while over > 0.5 {
                    let Some(wi) = (0..widths.len()).filter(|&i| !fixed[i] && widths[i] > least).max_by(|&a, &b| widths[a].total_cmp(&widths[b])) else { break };
                    let next = (0..widths.len()).filter(|&i| widths[i] < widths[wi]).map(|i| widths[i]).fold(least, f32::max);
                    let cut = over.min(widths[wi] - least).min((widths[wi] - next).max(1.0));
                    widths[wi] -= cut;
                    over -= cut;
                }
            }
            let total_w = widths.iter().sum::<f32>();
            let (head_h, row_h) = (32.0, 34.0);
            ui.add_space(4.0);
            let (area, _) = ui.allocate_exact_size(vec2(total_w, head_h + rows.len().max(1) as f32 * row_h), Sense::hover());
            let xs: Vec<f32> = widths.iter().scan(area.min.x, |x, w| {
                let at = *x;
                *x += w;
                Some(at)
            }).collect();
            let p = ui.painter().clone();
            for (i, h) in headers.iter().enumerate() {
                p.text(pos2(xs[i] + pad, area.min.y + head_h / 2.0), Align2::LEFT_CENTER, *h, head_font.clone(), pal.ink_soft);
            }
            p.line_segment([pos2(area.min.x, area.min.y + head_h - 0.5), pos2(area.max.x, area.min.y + head_h - 0.5)], Stroke::new(1.0, pal.border));
            if rows.is_empty() {
                p.text(pos2(area.min.x + pad, area.min.y + head_h + row_h / 2.0), Align2::LEFT_CENTER, "Nothing to show", font.clone(), pal.ink_soft);
            }
            for (ri, r) in rows.iter().enumerate() {
                let rr = Rect::from_min_size(pos2(area.min.x, area.min.y + head_h + ri as f32 * row_h), vec2(total_w, row_h));
                let resp = ui.interact(rr, ui.id().with((id, ri)), if clickable { Sense::click() } else { Sense::hover() });
                // (the pointer may be on a button in the row, which then counts as hovered instead)
                let hot = resp.hovered() || ui.rect_contains_pointer(rr);
                if hot {
                    p.rect_filled(rr.shrink2(vec2(2.0, 2.0)), Rounding::same(6.0), pal.hover);
                } else if ri > 0 {
                    p.line_segment([pos2(rr.min.x + pad, rr.min.y), pos2(rr.max.x - pad, rr.min.y)], Stroke::new(1.0, pal.border.gamma_multiply(0.6)));
                }
                if resp.clicked() {
                    clicked = Some(ri);
                }
                for (i, c) in r.iter().enumerate().take(widths.len()) {
                    let mut x = xs[i] + pad;
                    let room = widths[i] - 2.0 * pad;
                    let cy = rr.center().y;
                    let text = match c {
                        Cell::Text(t) => t,
                        Cell::Usage(f, t) => {
                            usage_bar(&p, Rect::from_min_size(pos2(x, cy - 2.5), vec2(110.0, 5.0)), *f, &pal);
                            x += 120.0;
                            t
                        }
                        Cell::Status(t) => {
                            p.circle_filled(pos2(x + 4.0, cy), 3.5, status_color(t, &pal));
                            x += 16.0;
                            t
                        }
                        Cell::Icon(k, t) => {
                            paint_icon(&p, Rect::from_center_size(pos2(x + 8.0, cy), vec2(16.0, 16.0)), *k, &pal, if hot { pal.hover } else { pal.window });
                            x += 24.0;
                            t
                        }
                        Cell::Custom(_) => {
                            custom(ui, ri, i, Rect::from_min_size(pos2(x, rr.min.y), vec2(room, row_h)), hot);
                            continue;
                        }
                    };
                    let mut job = egui::text::LayoutJob::simple_singleline(text.clone(), font.clone(), pal.ink);
                    job.wrap = egui::text::TextWrapping { max_width: (room - (x - xs[i] - pad)).max(10.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                    let g = p.layout_job(job);
                    p.galley(pos2(x, cy - g.size().y / 2.0), g, pal.ink);
                }
            }
        });
        clicked
    }

    fn storage(&self, ui: &mut Ui, sel: &Node) {
        let Some(d) = sel.sid.and_then(|id| self.server(id)).and_then(|s| s.data.as_ref()) else { return };
        if sel.kind == Kind::Vm {
            let rows = sel
                .rref
                .as_deref()
                .and_then(|r| d.vm(r))
                .map(|v| v.disks.iter().map(|k| vec![k.device.clone(), k.name.clone(), if k.size > 0 { fmt_bytes(k.size) } else { "—".into() }, k.sr_name.clone(), k.kind.clone()]).collect())
                .unwrap_or_default();
            self.table(ui, "disks", &["Position", "Name", "Size", "Storage", "Type"], &text_rows(rows), false);
        } else {
            // A server's own storage and the shared storage; the Shared Storage folder only the latter.
            let owner = match sel.kind {
                Kind::Host => sel.rref.clone(),
                Kind::Folder => Some(String::new()),
                _ => None,
            };
            let rows = d
                .srs
                .iter()
                .filter(|x| !x.is_empty_drive())
                .filter(|x| match &owner {
                    None => true,
                    Some(h) => x.host.as_deref().and_then(|xh| d.host(xh)).map_or(true, |xh| !h.is_empty() && xh.rref == *h),
                })
                .map(|x| {
                    // a drive with a disc in it is always "full": just its size
                    let usage = if x.kind == "udev" {
                        Cell::Text(fmt_bytes(x.size))
                    } else if x.size > 0 {
                        Cell::Usage((x.used as f32 / x.size as f32).clamp(0.0, 1.0), format!("{} of {}", fmt_bytes(x.used), fmt_bytes(x.size)))
                    } else {
                        Cell::Text("—".into())
                    };
                    vec![Cell::Text(x.name.clone()), Cell::Text(x.kind.clone()), usage, Cell::Text(fmt_bytes((x.size - x.used).max(0))), Cell::Text(if x.shared { "Yes".into() } else { "No".into() })]
                })
                .collect::<Vec<_>>();
            self.table(ui, "srs", &["Name", "Type", "Used", "Free", "Shared"], &rows, false);
        }
    }

    fn networking(&self, ui: &mut Ui, sel: &Node) {
        let Some(d) = sel.sid.and_then(|id| self.server(id)).and_then(|s| s.data.as_ref()) else { return };
        if sel.kind == Kind::Vm {
            let rows = sel
                .rref
                .as_deref()
                .and_then(|r| d.vm(r))
                .map(|v| v.nics.iter().map(|n| vec![n.device.clone(), n.mac.clone(), n.network.clone(), or(&n.ip, "—").to_string()]).collect())
                .unwrap_or_default();
            self.table(ui, "nics", &["Device", "MAC address", "Network", "IP address"], &text_rows(rows), false);
        } else {
            let rows = d.networks.iter().map(|n| vec![n.name.clone(), n.bridge.clone(), n.mtu.to_string(), n.description.clone()]).collect();
            self.table(ui, "nets", &["Name", "Bridge", "MTU", "Description"], &text_rows(rows), false);
        }
    }

    /// An ISO library's discs, each with its description and a pencil to edit it there and then.
    fn iso_files(&mut self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let Some((s, x)) = self.sr_of(sel) else { return };
        let (sid, sr, files) = (s.id, x.rref.clone(), x.files.clone());
        let total: i64 = files.iter().map(|f| f.size).sum();
        ui.add_space(10.0);
        let count = format!("{} ISO file{}  ·  {}", files.len(), if files.len() == 1 { "" } else { "s" }, fmt_bytes(total));
        ui.label(egui::RichText::new(count).color(pal.ink_soft));
        ui.label(egui::RichText::new("If your file isn't in the list below, try rescanning the folder.").size(12.0).color(pal.ink_faint));
        if let Some(u) = self.uploads.get(&sr) {
            ui.add_space(8.0);
            self.upload_card(ui, &sr, u, out);
        }
        let font = FontId::proportional(13.0);
        let measure = |t: &str| ui.painter().layout_no_wrap(t.to_string(), font.clone(), pal.ink).size().x;
        let rows: Vec<Vec<Cell>> = files
            .iter()
            .map(|f| {
                let editing = self.iso_edit.as_ref().map_or(false, |e| e.vdi == f.vdi);
                let desc_w = if editing { 240.0 } else { (measure(&f.description) + 34.0).min(380.0) };
                vec![Cell::Icon(IconKind::IsoSr, f.name.clone()), Cell::Text(fmt_bytes(f.size)), Cell::Custom(desc_w), Cell::Custom(56.0)]
            })
            .collect();
        let mut edit = self.iso_edit.take();
        let (mut close, mut save, mut delete, mut new_from) = (false, None, None, None);
        let (saving, deleting) = (&self.iso_saving, &self.iso_deleting);
        self.table_with(ui, "isos", &["Name", "Size", "Description", ""], &rows, false, &mut |ui, row, col, r, hot| {
            let f = &files[row];
            let gone = deleting.contains(&f.vdi);
            if col == 3 {
                // the bin, at the row's end: faint, red under the mouse; off while a running VM holds the disc
                if gone {
                    return;
                }
                // a new VM that installs from this ISO
                let nb = Rect::from_center_size(pos2(r.max.x - 43.0, r.center().y), vec2(26.0, 26.0));
                let nresp = ui.interact(nb, ui.id().with(("newvm", &f.vdi)), Sense::click()).on_hover_text("Create a VM that installs from this ISO");
                if nresp.hovered() {
                    ui.painter().rect_filled(nb, Rounding::same(5.0), pal.border);
                }
                paint_glyph(ui.painter(), Rect::from_center_size(nb.center(), vec2(16.0, 16.0)), "newvm", if nresp.hovered() { pal.accent } else if hot { pal.ink_soft } else { pal.ink_faint });
                if nresp.clicked() {
                    new_from = Some(f.vdi.clone());
                }
                let held = f.held_by();
                let can = f.can_delete && held.is_empty();
                let b = Rect::from_center_size(pos2(r.max.x - 13.0, r.center().y), vec2(26.0, 26.0));
                let resp = ui.interact(b, ui.id().with(("delete", &f.vdi)), if can { Sense::click() } else { Sense::hover() });
                let tip = if !held.is_empty() {
                    format!("In the DVD drive of {}, which is running: eject it there before deleting it", held.join(", "))
                } else if !f.can_delete {
                    "The server doesn't allow deleting this file right now".to_string()
                } else {
                    "Delete this file from the share".to_string()
                };
                let resp = resp.on_hover_text(tip);
                if can && resp.hovered() {
                    ui.painter().rect_filled(b, Rounding::same(5.0), pal.red.gamma_multiply(if pal.dark { 0.25 } else { 0.12 }));
                }
                let colour = if !can {
                    pal.ink_faint.gamma_multiply(0.45)
                } else if resp.hovered() {
                    pal.red
                } else if hot {
                    pal.ink_soft
                } else {
                    pal.ink_faint
                };
                paint_glyph(ui.painter(), Rect::from_center_size(b.center(), vec2(16.0, 16.0)), "trash", colour);
                if resp.clicked() {
                    delete = Some(f.vdi.clone());
                }
                return;
            }
            if gone {
                ui.painter().text(pos2(r.min.x, r.center().y), Align2::LEFT_CENTER, "Deleting…", font.clone(), pal.ink_faint);
                return;
            }
            if let Some(e) = edit.as_mut().filter(|e| e.vdi == f.vdi) {
                // a text box, then Save and Cancel; Enter saves, Esc cancels
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r).layout(egui::Layout::left_to_right(egui::Align::Center)));
                child.spacing_mut().item_spacing.x = 4.0;
                let field = text_field(&mut child, &pal, &mut e.text, (r.width() - 64.0).clamp(80.0, 520.0), false);
                if e.focus {
                    field.request_focus();
                    e.focus = false;
                }
                let enter = field.lost_focus() && child.input(|i| i.key_pressed(Key::Enter));
                let ok = flat_button(&mut child, "check", "Save", Some(pal.green), true, true, &pal).clicked();
                let cancel = flat_button(&mut child, "close", "Cancel", None, true, true, &pal).clicked() || child.input(|i| i.key_pressed(Key::Escape));
                if ok || enter {
                    let text = e.text.trim().to_string();
                    if text != f.description {
                        save = Some((f.vdi.clone(), f.name.clone(), text));
                    }
                }
                close |= ok || enter || cancel;
                return;
            }
            // the description (or the one being saved, faded), then the pencil
            let pending = saving.get(&f.vdi);
            let (text, col) = match pending {
                Some(t) => (t.as_str(), pal.ink_faint),
                None if f.description.is_empty() => ("—", pal.ink_faint),
                None => (f.description.as_str(), pal.ink),
            };
            let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font.clone(), col);
            job.wrap = egui::text::TextWrapping { max_width: (r.width() - 34.0).max(10.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
            let g = ui.painter().layout_job(job);
            let end = r.min.x + g.size().x;
            ui.painter().galley(pos2(r.min.x, r.center().y - g.size().y / 2.0), g, col);
            if pending.is_some() {
                return;
            }
            let b = Rect::from_center_size(pos2(end + 19.0, r.center().y), vec2(26.0, 26.0));
            let resp = ui.interact(b, ui.id().with(("describe", &f.vdi)), Sense::click()).on_hover_text("Edit the description");
            if resp.hovered() {
                ui.painter().rect_filled(b, Rounding::same(5.0), pal.border);
            }
            paint_glyph(ui.painter(), Rect::from_center_size(b.center(), vec2(16.0, 16.0)), "edit", if hot { pal.ink_soft } else { pal.ink_faint });
            if resp.clicked() {
                edit = Some(IsoEdit { vdi: f.vdi.clone(), text: f.description.clone(), focus: true });
            }
        });
        self.iso_edit = if close { None } else { edit };
        if let Some((vdi, name, text)) = save {
            out.push(Intent::SaveDescription(sid, sr.clone(), vdi, name, text));
        }
        if let Some(vdi) = delete {
            out.push(Intent::AskDeleteIso(sid, sr, vdi));
        }
        if let Some(vdi) = new_from {
            out.push(Intent::NewVm(Some(sid), Some(vdi)));
        }
    }

    /// An upload's progress: the file, a bar (blue, then green when it's done), how much and how
    /// fast, and Cancel; once done, it stays a few seconds with how long it took.
    fn upload_card(&self, ui: &mut Ui, sr: &str, u: &Upload, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let (sent, total) = (u.t.sent.load(Ordering::Relaxed), u.t.total.load(Ordering::Relaxed));
        let (finishing, cancelling) = (u.t.finishing.load(Ordering::Relaxed), u.t.cancel.load(Ordering::Relaxed));
        let done = u.done.is_some();
        let frac = if done { 1.0 } else if total > 0 { (sent as f64 / total as f64) as f32 } else { 0.0 };
        let secs = u.started.elapsed().as_secs_f64().max(0.001);
        let speed = sent as f64 / secs;
        let detail = if let Some(took) = u.done {
            format!("{} in {}  ·  now in {}", fmt_bytes(total as i64), fmt_took(took), u.lib)
        } else if cancelling {
            "Cancelling, and removing the unfinished file…".to_string()
        } else if finishing {
            "Finishing on the server…".to_string()
        } else if total == 0 || sent == 0 {
            if u.web { "Starting the download…".to_string() } else { "Starting…".to_string() }
        } else {
            let left = (total - sent.min(total)) as f64 / speed.max(1.0);
            let left = if left < 60.0 { format!("about {} s left", left.ceil() as u64) } else { format!("about {} min left", (left / 60.0).ceil() as u64) };
            format!("{} of {}  ·  {}/s  ·  {}", fmt_bytes(sent as i64), fmt_bytes(total as i64), fmt_bytes(speed as i64), left)
        };
        let title = match (done, u.web) {
            (false, false) => format!("Uploading {}", u.name),
            (false, true) => format!("Downloading {} from {}", u.name, u.from),
            (true, false) => format!("Uploaded {}", u.name),
            (true, true) => format!("Downloaded {}", u.name),
        };
        egui::Frame::none().fill(pal.surface).stroke(Stroke::new(1.0, pal.border)).rounding(Rounding::same(8.0)).inner_margin(egui::Margin::symmetric(12.0, 10.0)).show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(640.0)); // (a bare 640 would widen a narrow window's page)
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                let (glyph, col) = match (done, u.web) {
                    (true, _) => ("check", pal.green),
                    (false, true) => ("download", pal.accent),
                    (false, false) => ("upload", pal.accent),
                };
                paint_glyph(ui.painter(), r, glyph, col);
                // the title gives way to the button when the window is narrow
                let button = if done { 28.0 } else { flat_button_width(ui, "Cancel") };
                let room = (ui.available_width() - button - 8.0).max(40.0);
                ui.allocate_ui_with_layout(vec2(room, 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(title).color(pal.ink)).truncate());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if done {
                        if flat_button(ui, "close", "Dismiss", None, true, true, &pal).clicked() {
                            out.push(Intent::DismissUpload(sr.to_string()));
                        }
                    } else if flat_button(ui, "close", "Cancel", None, !finishing && !cancelling, false, &pal).on_hover_text("Stop; the unfinished file is removed from the share").clicked() {
                        out.push(Intent::CancelUpload(sr.to_string()));
                    }
                });
            });
            ui.add_space(4.0);
            let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
            progress_bar(ui.painter(), bar, frac, if done { pal.green } else { pal.accent }, &pal);
            ui.add_space(4.0);
            ui.label(egui::RichText::new(detail).size(12.0).color(pal.ink_soft));
        });
    }

    /// The VM's screen. A shut down or suspended VM has none: offer to start or resume it.
    fn console_tab(&mut self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let Some((sid, vref, name, os, power)) = self.console_target(sel) else { return };
        let Some(conn) = self.server(sid).map(|s| s.conn.clone()) else { return };
        if vref.is_empty() {
            // a server whose control domain the pool didn't list
            self.console = None;
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.label(egui::RichText::new("This server's console isn't available.").size(15.0).color(pal.ink_soft));
            });
            return;
        }
        if !matches!(power, PowerState::Running | PowerState::Paused) {
            self.console = None;
            let (text, op) = match power {
                PowerState::Suspended => ("This VM is suspended. Resume it to see its console.", "resume"),
                _ => ("This VM is shut down. Start it to see its console.", "start"),
            };
            let action = ACTIONS.iter().position(|a| a.op == op);
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.label(egui::RichText::new(text).size(15.0).color(pal.ink_soft));
                ui.add_space(12.0);
                if let Some(i) = action {
                    if dialog_button(ui, &pal, op, ACTIONS[i].label, Look::Primary, self.can_run(&ACTIONS[i], sel)) {
                        out.push(Intent::Action(i));
                    }
                }
            });
            return;
        }
        if self.console.as_ref().map_or(true, |c| c.sid != sid || c.vm != vref) {
            self.console = Some(ConsoleView::new(sid, vref, &name, &os, conn, ui.ctx().clone()));
        }
        if let Some(c) = self.console.as_mut() {
            let allow_keyboard = matches!(self.dialog, Dialog::None);
            c.ui(ui, &pal, allow_keyboard);
        }
    }

    fn logs(&self, ui: &mut Ui, sel: &Node) {
        let rows = self
            .log
            .iter()
            .rev()
            .filter(|e| match sel.sid {
                Some(sid) => e.sid == sid && (sel.kind != Kind::Vm || e.obj_ref.as_deref() == sel.rref.as_deref()),
                None => true,
            })
            .map(|e| vec![Cell::Text(e.time.clone()), Cell::Text(e.server.clone()), Cell::Text(e.obj.clone()), Cell::Text(e.text.clone()), Cell::Status(e.status.clone())])
            .collect::<Vec<_>>();
        self.table(ui, "logs", &["Time", "Server", "Object", "What happened", "Result"], &rows, false);
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut add: Option<(String, String, String, bool)> = None;
        let mut iso: Option<(u32, IsoLibrary)> = None;
        let mut forget_sr: Option<(u32, String)> = None;
        let mut delete_iso: Option<(u32, String, String, String)> = None;
        // from the Delete VM question: server, VM, its name, whether its disks go too
        let mut delete_vm: Option<(u32, String, String, bool)> = None;
        // from Settings: the theme and size chosen, a saved password to forget
        let mut theme: Option<ThemeMode> = None;
        let mut zoom: Option<f32> = None;
        let mut forget: Option<(String, String)> = None;
        let (theme_mode, zoom_now) = (self.theme_mode, self.zoom);
        let saved_keys: Vec<(String, String)> = self.saved.keys().cloned().collect();
        let mut new_vm: Option<(u32, NewVm, Arc<Trace>)> = None;
        // storage the New VM dialog wants checked: pool, storage, server
        let mut checks: Vec<(u32, String, Option<String>)> = vec![];
        // from the Upload dialog: the file chooser (None), or a link's file and its name
        let mut upload: Option<(u32, String, Option<(Source, String)>)> = None;
        let (keychain, saved) = (self.keychain, self.saved.clone());
        let mut confirm: Option<(usize, u32, String)> = None;
        let hosts = self.known_hosts.clone();
        let red = self.pal.red;
        let pal = self.pal;
        match &mut self.dialog {
            Dialog::None => {}
            Dialog::AddServer { server, user, password, error, remember } => {
                dialog_window(ctx, "Connect to Server").show(ctx, |ui| {
                    ui.set_width(400.0);
                    ui.spacing_mut().item_spacing.y = 8.0;
                    ui.label("Type the name or IP address of the server, and the account you use to sign in to it.");
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        // wider by the credentials box's inset, so every box starts at the same x
                        field_label(ui, "Server:", LABEL_WIDTH + GROUP_INSET);
                        let w = ui.available_width() - FIELD_HEIGHT - ui.spacing().item_spacing.x;
                        let field = text_field(ui, &pal, server, w, false).rect;
                        // Earlier servers: a small ▾ button opens the list under the box.
                        let (r, resp) = ui.allocate_exact_size(vec2(FIELD_HEIGHT, FIELD_HEIGHT), Sense::click());
                        let p = ui.painter();
                        p.rect(r, Rounding::same(5.0), if resp.hovered() { pal.hover } else { pal.input }, Stroke::new(1.0, pal.border_strong));
                        let c = r.center();
                        p.add(egui::Shape::line(vec![pos2(c.x - 4.0, c.y - 2.0), pos2(c.x, c.y + 2.0), pos2(c.x + 4.0, c.y - 2.0)], Stroke::new(1.6, pal.ink_soft)));
                        let popup = ui.make_persistent_id("known-hosts");
                        if resp.on_hover_text("Earlier servers").clicked() {
                            ui.memory_mut(|m| m.toggle_popup(popup));
                        }
                        let anchor = ui.interact(field.union(r), popup.with("anchor"), Sense::hover());
                        egui::popup::popup_below_widget(ui, popup, &anchor, egui::PopupCloseBehavior::CloseOnClick, |ui| {
                            ui.set_min_width(field.union(r).width() - 12.0);
                            for (h, u) in &hosts {
                                let label = egui::RichText::new(format!("{}   ", h)).color(pal.ink);
                                let resp = ui.horizontal(|ui| {
                                    let r = ui.selectable_label(false, label);
                                    ui.label(egui::RichText::new(u).color(pal.ink_soft));
                                    r
                                });
                                if resp.inner.clicked() {
                                    // the account used there, and its saved password
                                    *server = h.clone();
                                    *user = u.clone();
                                    *password = saved.get(&(h.clone(), u.clone())).cloned().unwrap_or_default();
                                    ui.memory_mut(|m| m.close_popup());
                                }
                            }
                            if hosts.is_empty() {
                                ui.label(egui::RichText::new("No earlier servers").color(pal.ink_soft));
                            }
                        });
                    });
                    egui::Frame::group(ui.style()).inner_margin(egui::Margin::same(GROUP_INSET - 1.0)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 8.0;
                        ui.label(egui::RichText::new("User login credentials").color(pal.ink_soft));
                        ui.horizontal(|ui| {
                            field_label(ui, "Username:", LABEL_WIDTH);
                            text_field(ui, &pal, user, ui.available_width(), false);
                        });
                        ui.horizontal(|ui| {
                            field_label(ui, "Password:", LABEL_WIDTH);
                            text_field(ui, &pal, password, ui.available_width(), true);
                        });
                    });
                    // Remember password: kept in the system's keychain, and the server reconnects at startup.
                    let can_save = keychain != Some(false);
                    ui.horizontal(|ui| {
                        ui.add_space(GROUP_INSET);
                        ui.add_enabled(can_save, egui::Checkbox::new(remember, "Remember password"));
                    });
                    let note = if can_save {
                        "Saved in your system's keychain. The app connects to this server again each time it starts."
                    } else {
                        "There's no keychain on this system to keep it in (such as GNOME Keyring, KWallet or KeePassXC)."
                    };
                    ui.horizontal(|ui| {
                        ui.add_space(GROUP_INSET + 24.0);
                        ui.add(egui::Label::new(egui::RichText::new(note).size(12.0).color(pal.ink_soft)).wrap());
                    });
                    if !error.is_empty() {
                        ui.colored_label(red, error.as_str());
                    }
                    ui.add_space(4.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) {
                            close = true;
                        }
                        let ok = !server.trim().is_empty() && !user.trim().is_empty();
                        let enter = ui.input(|i| i.key_pressed(Key::Enter));
                        if dialog_button(ui, &pal, "connect", "Connect", Look::Primary, ok) || (ok && enter) {
                            if server.trim().contains(' ') {
                                *error = "A server name can't contain spaces.".into();
                            } else {
                                add = Some((server.trim().to_string(), user.trim().to_string(), password.clone(), *remember && can_save));
                            }
                        }
                    });
                });
            }
            Dialog::Confirm { action, sid, vm, title, text } => {
                dialog_window(ctx, title.as_str()).show(ctx, |ui| {
                    ui.set_max_width(420.0);
                    ui.label(text.as_str());
                    ui.add_space(12.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "close", "No", Look::Plain, true) {
                            close = true;
                        }
                        if dialog_button(ui, &pal, "check", "Yes", Look::Primary, true) {
                            confirm = Some((*action, *sid, vm.clone()));
                        }
                    });
                });
            }
            Dialog::Iso(d) => match d.ui(ctx, &pal) {
                IsoAction::Close => close = true,
                IsoAction::Create(sid, lib) => iso = Some((sid, lib)),
                IsoAction::None => {}
            },
            Dialog::NewVm(d) => {
                match d.ui(ctx, &pal) {
                    NewVmAction::None => {}
                    NewVmAction::Close => close = true,
                    NewVmAction::Create(sid, spec) => {
                        let trace = Arc::new(Trace::default());
                        d.creating(trace.clone());
                        new_vm = Some((sid, spec, trace));
                    }
                }
                let sid = d.sid;
                checks.extend(d.wanted_checks().into_iter().map(|(sr, host)| (sid, sr, host)));
            }
            Dialog::Upload(d) => {
                let (sid, sr) = (d.sid, d.sr.clone());
                match d.ui(ctx, &pal) {
                    UploadAction::None => {}
                    UploadAction::Close => close = true,
                    UploadAction::PickLocal => upload = Some((sid, sr, None)),
                    UploadAction::Check(url) => {
                        let (tx, ctx) = (self.tx.clone(), ctx.clone());
                        std::thread::spawn(move || {
                            let result = probe_url(&url);
                            let _ = tx.send(Msg::LinkChecked { url, result });
                            ctx.request_repaint();
                        });
                    }
                    UploadAction::Download { url, name, size } => upload = Some((sid, sr, Some((Source::Web { url, size }, name)))),
                }
            }
            Dialog::ConfirmDeleteIso { sid, sr, vdi, name, text } => {
                dialog_window(ctx, "Delete ISO File").show(ctx, |ui| {
                    ui.set_max_width(420.0);
                    ui.label(text.as_str());
                    ui.add_space(12.0);
                    dialog_buttons(ui, |ui| {
                        // Esc or Cancel keep it; only a click on the red button deletes
                        if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                        if dialog_button(ui, &pal, "trash", "Delete", Look::Danger, true) {
                            delete_iso = Some((*sid, sr.clone(), vdi.clone(), name.clone()));
                        }
                    });
                });
            }
            Dialog::ConfirmDeleteVm { sid, vm, name, running, disks, snapshots, delete_disks } => {
                dialog_window(ctx, "Delete VM").show(ctx, |ui| {
                    ui.set_max_width(420.0);
                    ui.label(format!("Are you sure you want to delete '{}'?", name));
                    if *running {
                        ui.add_space(8.0);
                        ui.label("It's on: it's turned off first, the way pulling the plug would, without shutting its system down.");
                    }
                    if !disks.is_empty() {
                        ui.add_space(8.0);
                        let size = fmt_bytes(disks.iter().map(|k| k.size).sum());
                        let text = match disks.as_slice() {
                            [one] if !one.sr_name.is_empty() => format!("Delete its disk too ({}, on {})", size, one.sr_name),
                            [_] => format!("Delete its disk too ({})", size),
                            many => format!("Delete its {} disks too ({} in all)", many.len(), size),
                        };
                        ui.checkbox(delete_disks, text);
                    }
                    ui.add_space(8.0);
                    let mut rest = vec![];
                    if *snapshots > 0 {
                        rest.push(if *snapshots == 1 { "Its snapshot is deleted too.".to_string() } else { format!("Its {} snapshots are deleted too.", snapshots) });
                    }
                    rest.push("ISO files in its DVD drive aren't touched. This can't be undone.".into());
                    ui.label(egui::RichText::new(rest.join(" ")).color(pal.ink_soft));
                    ui.add_space(12.0);
                    dialog_buttons(ui, |ui| {
                        // Esc or Cancel keep it; only a click on the red button deletes
                        if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                        if dialog_button(ui, &pal, "trash", if *running { "Shut down and delete" } else { "Delete" }, Look::Danger, true) {
                            delete_vm = Some((*sid, vm.clone(), name.clone(), *delete_disks && !disks.is_empty()));
                        }
                    });
                });
            }
            Dialog::Settings { startup } => {
                dialog_window(ctx, "Settings").show(ctx, |ui| {
                    ui.set_width(440.0);
                    let label_w = 70.0;
                    let heading = |ui: &mut Ui, text: &str| {
                        ui.label(egui::RichText::new(text).font(FontId::new(14.0, FontFamily::Name("bold".into()))).color(pal.ink));
                        ui.add_space(8.0);
                    };
                    let note = |ui: &mut Ui, text: &str| {
                        ui.horizontal(|ui| {
                            ui.add_space(label_w + ui.spacing().item_spacing.x + 2.0);
                            ui.add(egui::Label::new(egui::RichText::new(text).size(12.0).color(pal.ink_soft)).wrap());
                        });
                    };
                    heading(ui, "Appearance");
                    ui.horizontal(|ui| {
                        field_label(ui, "Theme:", label_w);
                        let modes = [ThemeMode::Light, ThemeMode::Dark, ThemeMode::System];
                        let mut chosen = modes.iter().position(|m| *m == theme_mode).unwrap_or(2);
                        if crate::isolib::segmented(ui, &pal, &["Light", "Dark", "System"], &mut chosen, ui.available_width()) {
                            theme = Some(modes[chosen]);
                        }
                    });
                    ui.add_space(2.0);
                    note(ui, "System follows your desktop's light or dark setting.");
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        field_label(ui, "Size:", label_w);
                        let mut chosen = ZOOMS.iter().enumerate().min_by(|a, b| (a.1 - zoom_now).abs().total_cmp(&(b.1 - zoom_now).abs())).map_or(1, |(i, _)| i);
                        let names: Vec<String> = ZOOMS.iter().map(|z| format!("{:.0}%", z * 100.0)).collect();
                        let names: Vec<&str> = names.iter().map(String::as_str).collect();
                        if crate::isolib::segmented(ui, &pal, &names, &mut chosen, ui.available_width()) {
                            zoom = Some(ZOOMS[chosen]);
                        }
                    });
                    ui.add_space(2.0);
                    note(ui, "How big everything is drawn. Ctrl + and Ctrl - change it too.");
                    ui.add_space(18.0);

                    heading(ui, "Saved passwords");
                    let mut rows: Vec<&(String, String)> = saved_keys.iter().collect();
                    rows.sort();
                    match keychain {
                        None => note_left(ui, &pal, "Looking at the system's keychain…"),
                        Some(false) => note_left(ui, &pal, "This system has no keychain (Secret Service), so passwords can't be saved."),
                        Some(true) if rows.is_empty() => note_left(ui, &pal, "None yet. Tick Remember password when you connect to keep one."),
                        Some(true) => {
                            egui::Frame::none().stroke(Stroke::new(1.0, pal.border)).rounding(Rounding::same(8.0)).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.spacing_mut().item_spacing.y = 0.0;
                                for (n, (h, u)) in rows.iter().map(|k| (&k.0, &k.1)).enumerate() {
                                    if n > 0 {
                                        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
                                        ui.painter().line_segment([r.left_center(), r.right_center()], Stroke::new(1.0, pal.border));
                                    }
                                    let w = ui.available_width();
                                    ui.allocate_ui_with_layout(vec2(w, 40.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                        ui.add_space(12.0);
                                        let at_start = startup.iter().any(|(sh, su)| sh.eq_ignore_ascii_case(h) && su == u);
                                        // one line or two, in the middle of the row
                                        let (big, small) = ui.fonts(|f| (f.row_height(&egui::TextStyle::Body.resolve(ui.style())), f.row_height(&FontId::proportional(11.5))));
                                        let tall = big + if at_start { small + 1.0 } else { 0.0 };
                                        ui.vertical(|ui| {
                                            ui.spacing_mut().item_spacing.y = 1.0;
                                            ui.add_space(((40.0 - tall) / 2.0).max(0.0));
                                            ui.label(egui::RichText::new(format!("{}  ·  {}", h, u)).color(pal.ink));
                                            if at_start {
                                                ui.label(egui::RichText::new("Connects when the app starts").size(11.5).color(pal.ink_soft));
                                            }
                                        });
                                        let rest = ui.available_width();
                                        ui.allocate_ui_with_layout(vec2(rest, 40.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            ui.add_space(6.0);
                                            let tip = "Delete this password from the keychain; the app stops connecting here when it starts";
                                            if flat_button(ui, "trash", "Forget", Some(pal.red), true, false, &pal).on_hover_text(tip).clicked() {
                                                forget = Some(((*h).clone(), (*u).clone()));
                                            }
                                        });
                                    });
                                }
                            });
                        }
                    }
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("Passwords are kept in your system's keychain, never in the app's own files.").size(12.0).color(pal.ink_soft));
                    ui.add_space(14.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "check", "Close", Look::Primary, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                    });
                });
                if let Some(k) = &forget {
                    startup.retain(|(h, u)| !(h.eq_ignore_ascii_case(&k.0) && *u == k.1));
                }
            }
            Dialog::ConfirmSr { sid, sr, text } => {
                dialog_window(ctx, "Disconnect ISO Library").show(ctx, |ui| {
                    ui.set_max_width(420.0);
                    ui.label(text.as_str());
                    ui.add_space(12.0);
                    dialog_buttons(ui, |ui| {
                        // Esc or Cancel keep it; only a click on the red button disconnects
                        if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                        if dialog_button(ui, &pal, "disconnect", "Disconnect", Look::Danger, true) {
                            forget_sr = Some((*sid, sr.clone()));
                        }
                    });
                });
            }
            Dialog::Message { title, text } => {
                dialog_window(ctx, title.as_str()).show(ctx, |ui| {
                    ui.set_max_width(420.0);
                    ui.label(text.as_str());
                    ui.add_space(12.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "check", "OK", Look::Primary, true) {
                            close = true;
                        }
                    });
                });
            }
        }
        if close {
            self.dialog = Dialog::None;
        }
        if let Some((h, u, p, remember)) = add {
            self.dialog = Dialog::None;
            if let Some(dup) = self.servers.iter().find(|s| !s.conn.is_demo() && s.conn.hostname().eq_ignore_ascii_case(&h)).map(|s| s.id) {
                self.remove_server(dup, false);
            }
            self.add_server(Arc::new(RealConnection::new(&h, &u, &p).with_members(load_members(&h))));
            if let Some(s) = self.servers.last_mut() {
                // (the keychain may still be being looked for; only a missing one rules saving out)
                s.pending_save = (self.keychain != Some(false)).then_some((remember, p));
            }
        }
        if let Some((sid, lib)) = iso {
            self.start_iso_library(sid, lib, ctx);
        }
        if let Some((sid, sr, web)) = upload {
            self.dialog = Dialog::None;
            match web {
                None => self.pick_iso(sid, sr, ctx),
                Some((src, name)) => self.start_upload(sid, sr, src, Some(name), ctx),
            }
        }
        if let Some((sid, spec, trace)) = new_vm {
            self.create_vm(sid, spec, trace, ctx);
        }
        for (sid, sr, host) in checks {
            self.check_sr(sid, sr, host, ctx);
        }
        if let Some((sid, sr, vdi, name)) = delete_iso {
            self.dialog = Dialog::None;
            self.delete_iso(sid, sr, vdi, name, ctx);
        }
        if let Some(m) = theme {
            self.theme_mode = m;
            self.apply_theme(ctx);
            self.save_settings();
        }
        if let Some(z) = zoom {
            self.zoom = z;
            ctx.set_zoom_factor(z);
            self.save_settings();
        }
        if let Some((h, u)) = forget {
            self.keep_password(&h, &u, None);
        }
        if let Some((sid, vm, name, disks)) = delete_vm {
            self.dialog = Dialog::None;
            self.delete_vm(sid, vm, name, disks, ctx);
        }
        if let Some((sid, sr)) = forget_sr {
            self.dialog = Dialog::None;
            self.sr_job(sid, sr, true, ctx);
        }
        if let Some((a, sid, vm)) = confirm {
            self.dialog = Dialog::None;
            self.run_action(a, sid, vm);
        }
    }

    fn shortcuts(&self, ctx: &egui::Context, sel: &Node) -> Vec<Intent> {
        let mut out = vec![];
        if !matches!(self.dialog, Dialog::None) || console::has_focus(ctx) {
            return out; // keys go to the dialog, or to the VM's console
        }
        let keys = [(Key::B, Shortcut::CtrlB), (Key::E, Shortcut::CtrlE), (Key::R, Shortcut::CtrlR), (Key::Y, Shortcut::CtrlY)];
        for (k, sc) in keys {
            if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, k)) {
                // Ctrl+Y belongs to Suspend and Resume: use the one that can run for this VM.
                let pick = ACTIONS.iter().position(|a| a.shortcut == sc && self.can_run(a, sel));
                let first = ACTIONS.iter().position(|a| a.shortcut == sc);
                if let Some(i) = pick.or(first) {
                    out.push(Intent::Action(i));
                }
            }
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F5)) {
            out.push(Intent::Refresh(None));
        }
        out
    }

    fn screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.shot_path.clone() else { return };
        ctx.request_repaint();
        self.frames += 1;
        let timed_out = self.started.elapsed() > Duration::from_secs(20);
        let settled = self.creating == 0 && self.pending_select.is_none() && self.sr_checking.is_empty();
        if !self.shot_requested && ((self.shot_ready && self.frames >= 120 && settled) || timed_out) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
            self.shot_requested = true;
        }
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = image {
            let [w, h] = img.size;
            let mut buf = Vec::with_capacity(w * h * 4);
            for c in &img.pixels {
                buf.extend_from_slice(&c.to_array());
            }
            if let Some(dir) = std::path::Path::new(&path).parent() {
                if !dir.as_os_str().is_empty() {
                    let _ = std::fs::create_dir_all(dir);
                }
            }
            match image::RgbaImage::from_raw(w as u32, h as u32, buf) {
                Some(rgba) => {
                    if let Err(e) = rgba.save_with_format(&path, image::ImageFormat::Png) {
                        eprintln!("Screenshot failed: {}", e);
                    }
                }
                None => eprintln!("Screenshot failed: bad image size"),
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.apply_theme(ctx); // follows the system theme when set to "Same as the system"
        // Ctrl + / Ctrl - change the size too: remembered like the choice in Settings
        let zoom = ctx.zoom_factor();
        if (zoom - self.zoom).abs() > 0.001 {
            self.zoom = zoom;
            self.save_settings();
        }
        let pal = self.pal;
        self.tick(ctx);
        let tree = self.build_tree();
        if let Some((key, tab)) = self.pending_select.clone() {
            if tree.find(&key).is_some() {
                self.pending_select = None;
                self.select(key);
                self.tab = tab;
            }
        }
        let sel = tree.find(&self.selected).cloned().unwrap_or_else(|| tree.clone());
        if sel.key != self.selected {
            self.selected = sel.key.clone();
        }
        // A console stays connected while its VM or server is selected (on any of its tabs).
        let target = self.console_target(&sel).map(|t| (t.0, t.1));
        if self.console.as_ref().map_or(false, |c| target.as_ref() != Some(&(c.sid, c.vm.clone()))) {
            self.console = None;
        }
        let mut out = self.shortcuts(ctx, &sel);
        // files dropped on the window go to the selected ISO library (winit 0.30 delivers drops on
        // Windows, macOS and X11, not on Wayland: there, Upload's file chooser is the way)
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if !dropped.is_empty() {
            out.push(Intent::Dropped(dropped));
        }
        self.uploads.retain(|_, u| u.ended.map_or(true, |t| t.elapsed() < DONE_CARD));
        if !self.uploads.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(250)); // the progress moves, a finished card goes
        }
        if self.uploads.values().any(Upload::active) {
            // closing now would leave an unfinished file in the library
            if ctx.input(|i| i.viewport().close_requested()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                let names: Vec<String> = self.uploads.values().filter(|u| u.active()).map(|u| format!("'{}' to '{}'", u.name, u.lib)).collect();
                let text = format!("Still uploading {}.\n\nWait for it to finish, or cancel it, before closing the app; closing now would leave an unfinished file in the library.", names.join(", "));
                self.dialog = Dialog::Message { title: "Upload in progress".into(), text };
            }
        }
        // --show add-server | confirm | about: a check's screenshot with that dialog open.
        if self.shot_ready && !self.shot_dialog {
            self.shot_dialog = true;
            match self.shot_show.as_str() {
                "add-server" => out.push(Intent::AddServer),
                "confirm" => out.extend(ACTIONS.iter().position(|a| a.confirm && self.can_run(a, &sel)).map(Intent::Action)),
                "about" => out.push(Intent::About),
                "console" => out.push(Intent::Tab(Tab::Console)),
                "settings" => {
                    // two made-up saved passwords, one connecting at startup
                    self.keychain = Some(true);
                    self.saved.insert(("10.20.0.11".into(), "root".into()), String::new());
                    self.saved.insert(("lab-xen.example.net".into(), "admin".into()), String::new());
                    self.dialog = Dialog::Settings { startup: vec![("10.20.0.11".into(), "root".into())] };
                }
                "vm-busy" => {
                    // one VM starting, another shutting down: their icons turning
                    if let Some(s) = sel.sid.and_then(|id| self.server_mut(id)) {
                        s.busy.insert("vm-desk".into(), ("Starting…".into(), "start"));
                        s.busy.insert("vm-web".into(), ("Shutting down…".into(), "clean_shutdown"));
                    }
                }
                "host-console" => {
                    if let Some(sid) = sel.sid {
                        out.push(Intent::Select(format!("host:{}:host-a", sid)));
                        out.push(Intent::Tab(Tab::Console));
                    }
                }
                "iso" => out.push(Intent::IsoLibrary),
                "iso-advanced" | "iso-error" => {
                    let pools: Vec<(u32, String)> = self.servers.iter().filter(|s| s.data.is_some()).map(|s| (s.id, s.name())).collect();
                    if let Some(&(sid, _)) = pools.first() {
                        let error = self.shot_show == "iso-error";
                        let mut d = IsoDialog::new(sid, pools).filled("\\\\nas\\isos", "joao", if error { "wrong" } else { "" }, !error);
                        if error {
                            if let IsoAction::Create(sid, lib) = d.submit() {
                                self.start_iso_library(sid, lib, ctx);
                            }
                        }
                        self.dialog = Dialog::Iso(Box::new(d));
                    }
                }
                "storage" => out.push(Intent::Tab(Tab::Storage)),
                "search" => out.push(Intent::Tab(Tab::Search)),
                "logs" => out.push(Intent::Tab(Tab::Logs)),
                "isos" => out.push(Intent::Tab(Tab::IsoFiles)),
                "iso-disconnect" => {
                    out.push(Intent::Tab(Tab::IsoFiles));
                    if let Some(sid) = sel.sid {
                        out.push(Intent::AskForgetSr(sid, "sr-iso".into()));
                    }
                }
                "iso-upload" => {
                    out.push(Intent::Tab(Tab::IsoFiles));
                    if let Some(sid) = sel.sid {
                        self.start_upload(sid, "sr-iso".into(), Source::File(PathBuf::from("/nonexistent/Win11_24H2_English_x64 (1).iso")), None, ctx);
                    }
                }
                "iso-upload-choose" | "iso-upload-web" => {
                    out.push(Intent::Tab(Tab::IsoFiles));
                    if let Some(sid) = sel.sid {
                        let d = UploadDialog::new(sid, "sr-iso".into(), "ISO library".into(), vec![]);
                        let d = if self.shot_show == "iso-upload-web" {
                            let url = "https://releases.ubuntu.com/24.04/ubuntu-24.04.3-live-server-amd64.iso";
                            d.with_link(url, WebFile { name: "ubuntu-24.04.3-live-server-amd64.iso".into(), size: 3_213_064_192 })
                        } else {
                            d
                        };
                        self.dialog = Dialog::Upload(Box::new(d));
                    }
                }
                "iso-upload-done" => {
                    out.push(Intent::Tab(Tab::IsoFiles));
                    if let Some(sid) = sel.sid {
                        let t = Arc::new(Transfer::default());
                        t.total.store(3_213_064_192, Ordering::Relaxed);
                        let now = Instant::now();
                        let (name, from) = ("ubuntu-24.04.3-live-server-amd64.iso".to_string(), "releases.ubuntu.com".to_string());
                        let u = Upload { sid, lib: "ISO library".into(), name, web: true, from, t, started: now, log: 0, done: Some(Duration::from_secs(74)), ended: Some(now + Duration::from_secs(30)) };
                        self.uploads.insert("sr-iso".into(), u);
                    }
                }
                "new-vm" | "new-vm-advanced" | "new-vm-custom" | "new-vm-no-start" | "new-vm-created" | "new-vm-offline" => {
                    if self.shot_show == "new-vm-offline" {
                        // the demo's ISO library doesn't answer
                        std::env::set_var("SVM_DEMO_OFFLINE", "sr-iso");
                    }
                    if let Some(sid) = sel.sid {
                        if let Some(d) = self.server(sid).and_then(|s| s.data.as_ref()) {
                            let iso = d.sr("sr-iso").and_then(|x| x.files.get(2)).map(|f| f.vdi.clone());
                            let dlg = NewVmDialog::new(sid, d, iso.as_deref());
                            let dlg = match self.shot_show.as_str() {
                                "new-vm-advanced" => dlg.with_advanced(),
                                // more memory than a server has free
                                "new-vm-custom" => dlg.with_custom(4, 160, 80),
                                // Create clicked with that: made, but it can't start
                                "new-vm-no-start" => dlg.with_custom(4, 160, 80).pressed(),
                                // Create clicked as it comes: made, started, its console open
                                "new-vm-created" => dlg.pressed(),
                                _ => dlg,
                            };
                            self.dialog = Dialog::NewVm(Box::new(dlg));
                        }
                    }
                }
                "vm-delete" => {
                    if let Some(sid) = sel.sid {
                        out.push(Intent::AskDeleteVm(sid, "vm-web".into()));
                    }
                }
                "iso-delete" => {
                    out.push(Intent::Tab(Tab::IsoFiles));
                    if let Some(sid) = sel.sid {
                        out.push(Intent::AskDeleteIso(sid, "sr-iso".into(), "sr-iso-vdi-1".into()));
                    }
                }
                "iso-describe" => {
                    out.push(Intent::Tab(Tab::IsoFiles));
                    self.iso_edit = Some(IsoEdit { vdi: "sr-iso-vdi-1".into(), text: "Rocky Linux 9, minimal install".into(), focus: true });
                }
                _ => {}
            }
        }

        let tools_frame = egui::Frame::none().fill(pal.toolbar).inner_margin(egui::Margin::symmetric(6.0, 4.0)).stroke(Stroke::new(1.0, pal.border));
        egui::TopBottomPanel::top("tools").frame(tools_frame).show(ctx, |ui| self.toolbar(ui, &sel, &mut out));

        // The sidebar folds to a narrow strip of icons, with a short animation.
        let rail = egui::SidePanel::left("rail").resizable(false).exact_width(RAIL_WIDTH).frame(egui::Frame::none().fill(pal.rail).stroke(Stroke::new(1.0, pal.border)));
        let full = egui::SidePanel::left("tree").resizable(true).default_width(290.0).width_range(200.0..=560.0).frame(egui::Frame::none().fill(pal.sidebar).stroke(Stroke::new(1.0, pal.border)));
        let open = self.sidebar_open;
        egui::SidePanel::show_animated_between(ctx, open, rail, full, |ui, how_expanded| {
            if how_expanded < 0.5 {
                self.rail(ui, &mut out);
            } else {
                self.sidebar(ui, &tree, &mut out);
            }
        });

        egui::CentralPanel::default().frame(egui::Frame::none().fill(pal.window)).show(ctx, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let tabs = self.tabs_for(&sel);
            let current = if tabs.contains(&self.tab) { self.tab } else { tabs[0] };
            let actions = self.tab_actions_width(&sel, current);
            let free = self.header_and_tabs(ui, &sel, &tabs, current, actions, &mut out);
            if actions > 0.0 {
                self.tab_actions(ui, free, &sel, current, actions, &mut out);
            } else if let Some((s, x)) = self.sr_of(&sel).filter(|(_, x)| x.is_iso()) {
                // an ISO library's actions, next to its tabs
                let (sid, sr) = (s.id, x.rref.clone());
                let busy = self.sr_busy.contains(&sr);
                let uploading = self.uploads.get(&sr).map_or(false, Upload::active);
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(free).layout(egui::Layout::right_to_left(egui::Align::Center)));
                child.spacing_mut().item_spacing.x = 4.0;
                // with their names when there's room, otherwise as icons
                let compact = free.width() < flat_button_width(&child, "Disconnect") + flat_button_width(&child, "Working…") + flat_button_width(&child, "Upload") + 16.0;
                if flat_button(&mut child, "disconnect", "Disconnect", Some(pal.red), !busy && !uploading, compact, &pal).on_hover_text("Stop using this library; nothing on the share is deleted").clicked() {
                    out.push(Intent::AskForgetSr(sid, sr.clone()));
                }
                if flat_button(&mut child, "rescan", if busy { "Working…" } else { "Rescan" }, None, !busy, compact, &pal).on_hover_text("Look for ISO files added to or removed from the share").clicked() {
                    out.push(Intent::RescanSr(sid, sr.clone()));
                }
                if flat_button(&mut child, "upload", "Upload", None, !busy && !uploading, compact, &pal).on_hover_text("Add an .iso file from this computer or from a link on the web").clicked() {
                    out.push(Intent::Upload(sid, sr));
                }
            }
            ui.spacing_mut().item_spacing.y = 4.0;
            let margin = if current == Tab::General || current == Tab::Home { 0.0 } else { 10.0 };
            egui::Frame::none().inner_margin(egui::Margin::symmetric(margin, 0.0)).show(ui, |ui| match current {
                Tab::Home => self.home(ui, &mut out),
                Tab::Search => self.search_tab(ui, &mut out),
                Tab::General => self.general(ui, &sel, &mut out),
                Tab::Memory => self.memory(ui, &sel),
                Tab::Storage => self.storage(ui, &sel),
                Tab::Networking => self.networking(ui, &sel),
                Tab::Console => self.console_tab(ui, &sel, &mut out),
                Tab::IsoFiles => self.iso_files(ui, &sel, &mut out),
                Tab::Logs => self.logs(ui, &sel),
            });
        });

        self.dialogs(ctx);
        self.apply(out, &sel, ctx);
        // files dragged over the window: say where they'd go
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let text = match self.sr_of(&sel).filter(|(_, x)| x.is_iso()) {
                Some((_, x)) => format!("Drop to upload to {}", x.name),
                None => "Select an ISO library first, then drop the file on it".to_string(),
            };
            let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop-files")));
            let r = ctx.screen_rect();
            p.rect_filled(r, 0.0, Color32::from_black_alpha(150));
            p.rect_stroke(r.shrink(14.0), Rounding::same(12.0), Stroke::new(2.0, pal.accent));
            p.text(r.center(), Align2::CENTER_CENTER, text, FontId::proportional(18.0), Color32::WHITE);
        }
        self.screenshot(ctx);
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

pub(crate) const FIELD_HEIGHT: f32 = 26.0;
pub(crate) const LABEL_WIDTH: f32 = 80.0;
pub(crate) const GROUP_INSET: f32 = 11.0; // a group box's margin plus its 1px border

/// A dialog window: centred, fixed size, with room around its contents.
pub(crate) fn dialog_window(ctx: &egui::Context, title: &str) -> egui::Window<'static> {
    egui::Window::new(title.to_string())
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .frame(egui::Frame::window(&ctx.style()).inner_margin(egui::Margin::same(14.0)))
}

/// A left-aligned field label of fixed width, so the boxes after it line up.
pub(crate) fn field_label(ui: &mut Ui, text: &str, width: f32) {
    ui.allocate_ui_with_layout(vec2(width, FIELD_HEIGHT), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_size(vec2(width, FIELD_HEIGHT));
        ui.label(text);
    });
}

/// A one-line text box with a visible border (blue while typing), drawn like the sidebar's search
/// box. egui's own box has no border until hovered, and its focus outline is white.
pub(crate) fn text_field(ui: &mut Ui, pal: &Pal, text: &mut String, width: f32, password: bool) -> egui::Response {
    let bg = ui.painter().add(egui::Shape::Noop);
    let row = ui.text_style_height(&egui::TextStyle::Body);
    let pad = egui::Margin::symmetric(7.0, ((FIELD_HEIGHT - row) / 2.0).max(0.0));
    let mut edit = ui.add_sized([width, FIELD_HEIGHT], egui::TextEdit::singleline(text).password(password).frame(false).margin(pad));
    edit.rect = edit.rect + pad; // egui reports the box without its margin
    let stroke = Stroke::new(1.0, if edit.has_focus() { pal.accent } else { pal.border_strong });
    ui.painter().set(bg, egui::epaint::RectShape::new(edit.rect, Rounding::same(5.0), pal.input, stroke));
    edit
}

/// The row of buttons at the bottom of a dialog, right-aligned and one row tall. (A bare
/// right-to-left layout would stretch an auto-sized window to the whole screen height.)
pub(crate) fn dialog_buttons(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(vec2(ui.available_width(), FIELD_HEIGHT), egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        add(ui);
    });
}

/// How a dialog button looks: outlined; filled with the accent colour (the main choice: Connect,
/// Yes, OK); or red (a choice that takes something away, like Disconnect).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Look {
    Plain,
    Primary,
    Danger,
}

/// A dialog button: an icon (a `paint_glyph` name), then its label.
pub(crate) fn dialog_button(ui: &mut Ui, pal: &Pal, glyph: &str, label: &str, look: Look, enabled: bool) -> bool {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let content = 22.0 + ui.painter().layout_no_wrap(label.to_string(), font.clone(), pal.ink).size().x;
    let filled = enabled && look != Look::Plain;
    let b = match look {
        _ if !filled => egui::Button::new("").stroke(Stroke::new(1.0, pal.border_strong)),
        Look::Danger => egui::Button::new("").fill(pal.red),
        _ => egui::Button::new("").fill(pal.accent),
    };
    let resp = ui.add_enabled(enabled, b.min_size(vec2((content + 28.0).max(80.0), FIELD_HEIGHT)).rounding(Rounding::same(5.0)));
    let (text, icon) = if filled {
        (pal.on_accent, pal.on_accent)
    } else if enabled {
        (pal.ink, pal.ink_soft)
    } else {
        (pal.ink_faint, pal.ink_faint)
    };
    let (x, cy) = (resp.rect.center().x - content / 2.0, resp.rect.center().y);
    let bg = match look {
        _ if !filled => ui.style().interact(&resp).weak_bg_fill,
        Look::Danger => pal.red,
        _ => pal.accent,
    };
    paint_glyph_on(ui.painter(), Rect::from_center_size(pos2(x + 8.0, cy), vec2(16.0, 16.0)), glyph, icon, bg);
    ui.painter().text(pos2(x + 22.0, cy), Align2::LEFT_CENTER, label, font, text);
    resp.clicked()
}

/// How wide a labelled toolbar button is.
fn tool_width(ui: &Ui, label: &str) -> f32 {
    32.0 + ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(13.0), Color32::WHITE).size().x + 10.0
}

/// A thin rounded progress bar filled to `frac` in `col` (blue under way, green when done).
fn progress_bar(p: &egui::Painter, r: Rect, frac: f32, col: Color32, pal: &Pal) {
    let round = Rounding::same(r.height() / 2.0);
    p.rect_filled(r, round, pal.border);
    if frac > 0.0 {
        p.rect_filled(Rect::from_min_size(r.min, vec2((r.width() * frac.clamp(0.0, 1.0)).max(r.height()), r.height())), round, col);
    }
}

/// "45 s", "3 min 20 s", "1 h 05 min".
fn fmt_took(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{} s", s.max(1)),
        60..=3599 => format!("{} min {} s", s / 60, s % 60),
        _ => format!("{} h {:02} min", s / 3600, s % 3600 / 60),
    }
}

/// Why making or starting a VM failed, in plain words where the server's answer is a known one.
fn create_problem(e: &ApiError) -> String {
    let all = format!("{} {}", e.code, e.details.join(" ")).to_lowercase();
    match e.code.as_str() {
        "HOST_NOT_ENOUGH_FREE_MEMORY" => "No server has enough free memory for it right now. Pick a smaller size, or stop another VM, then start it.".into(),
        NOT_ANSWERING => format!("{} is offline: it didn't answer in {} seconds, so nothing was made. Turn it on, or pick another, then try again.", e.details.first().map_or("The storage", |n| n.as_str()), CHECK_SECS),
        NOT_PLUGGED => format!("{} isn't plugged in on the server the VM is for, so nothing was made. Pick another server, then try again.", e.details.first().map_or("The storage", |n| n.as_str())),
        "SR_FULL" => "There isn't enough room on the storage for its disk.".into(),
        "RBAC_PERMISSION_DENIED" | "PERMISSION_DENIED" => "Your account isn't allowed to do that.".into(),
        "VM_SECUREBOOT_CERTIFICATES_MISSING" | "POOL_SECURE_BOOT_CERTIFICATES_MISSING" => {
            "UEFI Secure Boot needs the pool's certificates, which aren't installed. Choose plain UEFI under Advanced, or install them on the pool (secureboot-certs install).".into()
        }
        _ if all.contains("secureboot") || all.contains("secure boot") => {
            "UEFI Secure Boot couldn't be set up on this pool. Choose plain UEFI under Advanced, or install the pool's certificates (secureboot-certs install).".into()
        }
        _ if all.contains("no space") || all.contains("insufficient space") => "There isn't enough room on the storage for its disk.".into(),
        _ => e.message(),
    }
}

/// Why deleting a VM failed, in plain words where the server's answer is a known one.
fn delete_vm_problem(e: &ApiError) -> String {
    match e.code.as_str() {
        "OPERATION_BLOCKED" => "This VM is protected from being deleted (its blocked operations include destroy). Lift that first, in XenCenter or with xe vm-param-clear, then try again.".into(),
        "VM_BAD_POWER_STATE" => "It couldn't be turned off first. Shut it down, then try again.".into(),
        "OTHER_OPERATION_IN_PROGRESS" => "The server is busy with something else on this VM. Wait a moment, then try again.".into(),
        _ => e.message(),
    }
}

/// Why deleting an ISO file failed, in plain words where the server's answer is a known one.
fn delete_problem(e: &ApiError) -> String {
    let all = format!("{} {}", e.code, e.details.join(" ")).to_lowercase();
    if e.code == "VDI_IN_USE" {
        "A running VM has this disc in its DVD drive. Eject it there first, then try again.".into()
    } else if all.contains("vdidelete") || all.contains("permission denied") || all.contains("read-only") || all.contains("vdi deletion failed") {
        "The server couldn't delete the file on the share: the account this library uses to reach the share can't change files there. Give it write access on the NAS, or delete the file on the share yourself and Rescan.".into()
    } else if e.code == "VM_BAD_POWER_STATE" {
        "A VM that has this disc in its drive is suspended, so the disc can't be taken out. Resume or shut down that VM first.".into()
    } else {
        e.message()
    }
}

/// Why an upload failed, in plain words where the server's answer is a known one.
fn upload_problem(e: &ApiError) -> String {
    let all = format!("{} {}", e.code, e.details.join(" ")).to_lowercase();
    if all.contains("already exists") {
        "There's already a file with that name in the library.".into()
    } else if all.contains("could not create file") || all.contains("permission denied") || all.contains("read-only") {
        "The server couldn't create the file on the share: the account this library uses to reach the share can't write there. Give it write access on the NAS, or copy the file to the share yourself and Rescan.".into()
    } else if all.contains("no space") {
        "The share is out of space.".into()
    } else if all.contains("operation_not_supported") || all.contains("not supported") {
        "This library doesn't take new files from the server. Copy the file to the share yourself, then Rescan.".into()
    } else {
        e.message()
    }
}

/// Storage's tree icon: a disc for ISO libraries, a drum for the rest.
fn sr_icon(x: &SrInfo) -> IconKind {
    if x.is_iso() {
        IconKind::IsoSr
    } else {
        IconKind::Sr
    }
}

/// A cell of a table.
enum Cell {
    Text(String),
    /// A bar filled to the fraction, then a caption ("120 GB of 900 GB").
    Usage(f32, String),
    /// A log entry's result, after a coloured dot.
    Status(String),
    /// An icon, then the text.
    Icon(IconKind, String),
    /// Drawn by the caller (see `table_with`), at least this wide.
    Custom(f32),
}

fn text_rows(rows: Vec<Vec<String>>) -> Vec<Vec<Cell>> {
    rows.into_iter().map(|r| r.into_iter().map(Cell::Text).collect()).collect()
}

/// A frameless button, an icon then its label, with a soft background under the mouse. `tint`
/// colours the icon (red for one that takes something away); `compact` shows only the icon, with
/// the label as its tooltip.
pub(crate) fn flat_button(ui: &mut Ui, glyph: &str, label: &str, tint: Option<Color32>, enabled: bool, compact: bool, pal: &Pal) -> egui::Response {
    let font = FontId::proportional(13.0);
    let w = if compact { 28.0 } else { flat_button_width(ui, label) };
    let (r, mut resp) = ui.allocate_exact_size(vec2(w, 26.0), if enabled { Sense::click() } else { Sense::hover() });
    let bg = if enabled && resp.hovered() { pal.hover } else { pal.window };
    if enabled && resp.hovered() {
        ui.painter().rect_filled(r, Rounding::same(5.0), pal.hover);
    }
    let icon = if enabled { tint.unwrap_or(pal.ink_soft) } else { pal.ink_faint };
    let at = if compact { r.center() } else { pos2(r.min.x + 18.0, r.center().y) };
    paint_glyph_on(ui.painter(), Rect::from_center_size(at, vec2(16.0, 16.0)), glyph, icon, bg);
    if compact {
        resp = resp.on_hover_text(label);
    } else {
        ui.painter().text(pos2(r.min.x + 32.0, r.center().y), Align2::LEFT_CENTER, label, font, if enabled { pal.ink } else { pal.ink_faint });
    }
    resp
}

/// How wide a `flat_button` with its label is: 10 + icon 16 + 6 + label + 10.
fn flat_button_width(ui: &Ui, label: &str) -> f32 {
    42.0 + ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(13.0), Color32::WHITE).size().x
}

/// A thin rounded bar filled to `frac`: accent blue, orange past 80%, red past 90%.
fn usage_bar(p: &egui::Painter, r: Rect, frac: f32, pal: &Pal) {
    let round = Rounding::same(r.height() / 2.0);
    p.rect_filled(r, round, pal.border);
    let col = if frac > 0.9 { pal.red } else if frac > 0.8 { pal.orange } else { pal.accent };
    if frac > 0.0 {
        p.rect_filled(Rect::from_min_size(r.min, vec2((r.width() * frac).max(r.height()), r.height())), round, col);
    }
}

/// A Home section's small heading.
fn home_title(ui: &mut Ui, text: &str, pad: f32, pal: &Pal) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
    ui.painter().text(pos2(r.min.x + pad + 2.0, r.min.y + 6.0), Align2::LEFT_TOP, text, FontId::new(13.5, FontFamily::Name("bold".into())), pal.ink);
}

/// The dot colour for a log entry's result.
fn status_color(status: &str, pal: &Pal) -> Color32 {
    match status {
        "Done" => pal.green,
        "Failed" => pal.red,
        _ => pal.orange,
    }
}

/// The width of the small arrow next to Shut Down and Reboot that opens their Force menu.
const ARROW_W: f32 = 16.0;

/// A labelled toolbar button with a theme-coloured glyph; `compact` shows only the glyph, with the
/// label as a tooltip.
/// The interface sizes Settings offers.
const ZOOMS: [f32; 4] = [0.9, 1.0, 1.1, 1.25];

/// A grey line of explanation, from the left edge.
fn note_left(ui: &mut Ui, pal: &Pal, text: &str) {
    ui.add(egui::Label::new(egui::RichText::new(text).size(12.5).color(pal.ink_soft)).wrap());
}

/// Force Shut Down for Shut Down, Force Reboot for Reboot: in the menu of the small arrow beside it.
fn force_of(op: &str) -> Option<usize> {
    match op {
        "clean_shutdown" => ACTIONS.iter().position(|a| a.op == "hard_shutdown"),
        "clean_reboot" => ACTIONS.iter().position(|a| a.op == "hard_reboot"),
        _ => None,
    }
}

/// Shut Down or Reboot (`op`) and the small arrow whose menu holds its Force partner
/// (ACTIONS[`fi`]), as one button in two parts: the mouse over either lights both, a hairline
/// between them. Whether the main part was clicked.
fn split_tool(ui: &mut Ui, pal: &Pal, op: &str, tip: &str, enabled: bool, fi: usize, force_ok: bool, out: &mut Vec<Intent>) -> bool {
    let (r, _) = ui.allocate_exact_size(vec2(32.0 + ARROW_W, 30.0), Sense::hover());
    let back = ui.painter().add(egui::Shape::Noop);
    let main_r = Rect::from_min_size(r.min, vec2(32.0, r.height()));
    let arrow_r = Rect::from_min_max(pos2(main_r.max.x, r.min.y), r.max);
    let main = ui.interact(main_r, ui.id().with(("split", op)), if enabled { Sense::click() } else { Sense::hover() });
    paint_tool(ui.painter(), Rect::from_center_size(main_r.center(), vec2(22.0, 22.0)), op, enabled, pal);
    let main = main.on_hover_text(tip);
    // the arrow and its menu
    let force = &ACTIONS[fi];
    let (mut arrow_lit, mut open) = (false, false);
    let mut part = ui.new_child(egui::UiBuilder::new().max_rect(arrow_r).layout(egui::Layout::left_to_right(egui::Align::Center)));
    part.add_enabled_ui(force_ok, |ui| {
        let m = egui::menu::menu_custom_button(ui, egui::Button::new("").frame(false).min_size(arrow_r.size()), |ui| {
            ui.set_min_width(170.0);
            if ui.button(force.label).clicked() {
                out.push(Intent::Action(fi));
                ui.close_menu();
            }
        });
        arrow_lit = m.response.hovered() && force_ok;
        open = m.inner.is_some();
        m.response.on_hover_text(force.label);
    });
    let c = arrow_r.center();
    let col = if force_ok { pal.ink_soft } else { pal.ink_faint };
    ui.painter().add(egui::Shape::line(vec![pos2(c.x - 3.5, c.y - 1.5), pos2(c.x, c.y + 2.0), pos2(c.x + 3.5, c.y - 1.5)], Stroke::new(1.4, col)));
    if (main.hovered() && enabled) || arrow_lit || open {
        ui.painter().set(back, egui::epaint::RectShape::filled(r, Rounding::same(6.0), pal.hover));
        ui.painter().line_segment([pos2(main_r.max.x, r.min.y + 7.0), pos2(main_r.max.x, r.max.y - 7.0)], Stroke::new(1.0, pal.border_strong));
    }
    main.clicked()
}

/// A console's keyboard button: its menu sends Ctrl+Alt+Del and the keys the desktop keeps.
fn keys_button(ui: &mut Ui, pal: &Pal, c: &mut ConsoleView) {
    let on = c.connected();
    ui.add_enabled_ui(on, |ui| {
        let slot = ui.painter().add(egui::Shape::Noop);
        let m = egui::menu::menu_custom_button(ui, egui::Button::new("").frame(false).min_size(vec2(30.0, 26.0)), |ui| c.keys_menu(ui));
        let r = m.response.rect;
        if (m.response.hovered() && on) || m.inner.is_some() {
            ui.painter().set(slot, egui::epaint::RectShape::filled(r, Rounding::same(5.0), pal.hover));
        }
        paint_glyph(ui.painter(), Rect::from_center_size(r.center(), vec2(16.0, 16.0)), "keyboard", if on { pal.ink_soft } else { pal.ink_faint });
        m.response.on_hover_text("Send keys: Ctrl+Alt+Del, the Windows key, Alt+Tab…");
    });
}

/// A thin upright line between groups of buttons.
fn vsep(ui: &mut Ui, pal: &Pal) {
    let (r, _) = ui.allocate_exact_size(vec2(9.0, 18.0), Sense::hover());
    ui.painter().line_segment([r.center_top(), r.center_bottom()], Stroke::new(1.0, pal.border));
}

fn tool_button(ui: &mut Ui, op: &str, label: &str, enabled: bool, compact: bool, pal: &Pal) -> egui::Response {
    let w = if compact { 32.0 } else { tool_width(ui, label) };
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, mut resp) = ui.allocate_exact_size(vec2(w, 30.0), sense);
    let p = ui.painter();
    if enabled && resp.hovered() {
        p.rect_filled(rect, Rounding::same(6.0), pal.hover);
    }
    if compact {
        paint_tool(p, Rect::from_center_size(rect.center(), vec2(22.0, 22.0)), op, enabled, pal);
        resp = resp.on_hover_text(label);
    } else {
        paint_tool(p, Rect::from_min_size(rect.min + vec2(4.0, 4.0), vec2(22.0, 22.0)), op, enabled, pal);
        p.text(pos2(rect.min.x + 32.0, rect.center().y), Align2::LEFT_CENTER, label, FontId::proportional(13.0), if enabled { pal.ink } else { pal.ink_faint });
    }
    resp
}

/// A square icon-only button (used for the sidebar toggle).
fn icon_button(ui: &mut Ui, op: &str, pal: &Pal, enabled: bool, active: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(32.0, 30.0), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, Rounding::same(6.0), pal.hover);
    } else if active {
        p.rect_filled(rect, Rounding::same(6.0), pal.nav_on);
    }
    paint_tool(p, Rect::from_center_size(rect.center(), vec2(22.0, 22.0)), op, enabled, pal);
    resp
}

/// The small « button that folds the sidebar.
fn chevron_button(ui: &mut Ui, pal: &Pal, left: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(24.0, 26.0), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, Rounding::same(5.0), pal.hover);
    }
    let c = rect.center();
    let s = Stroke::new(1.6, pal.ink_soft);
    let d = if left { 1.0 } else { -1.0 };
    for off in [-2.5f32, 2.5] {
        let x = c.x + off;
        p.add(egui::Shape::line(vec![pos2(x + 2.5 * d, c.y - 4.5), pos2(x - 2.5 * d, c.y), pos2(x + 2.5 * d, c.y + 4.5)], s));
    }
    resp
}

enum RailIcon {
    Icon(IconKind),
    Tool(&'static str),
    Search,
}

/// One button of the folded sidebar.
fn rail_icon(ui: &mut Ui, pal: &Pal, icon: RailIcon, active: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(34.0, 34.0), Sense::click());
    let p = ui.painter();
    let bg = if active {
        pal.nav_on
    } else if resp.hovered() {
        pal.hover
    } else {
        pal.rail
    };
    if active || resp.hovered() {
        p.rect_filled(rect, Rounding::same(7.0), bg);
    }
    if active {
        p.rect_filled(Rect::from_min_size(pos2(rect.min.x - 5.0, rect.min.y + 8.0), vec2(3.0, 18.0)), Rounding::same(1.5), pal.accent);
    }
    match icon {
        RailIcon::Icon(k) => paint_icon(p, Rect::from_center_size(rect.center(), vec2(16.0, 16.0)), k, pal, bg),
        RailIcon::Tool(op) => paint_tool(p, Rect::from_center_size(rect.center(), vec2(22.0, 22.0)), op, true, pal),
        RailIcon::Search => paint_search(p, Rect::from_center_size(rect.center(), vec2(18.0, 18.0)), pal.icon),
    }
    resp
}

/// Keeps only items whose name holds the search text, with the folders above them.
fn prune(nodes: &mut Vec<Node>, f: &str) {
    for n in nodes.iter_mut() {
        prune(&mut n.children, f);
    }
    nodes.retain(|n| !n.children.is_empty() || n.text.to_lowercase().contains(f));
}

fn clock_text() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let t = secs % 86_400;
    format!("{:02}:{:02}:{:02} UTC", t / 3600, (t / 60) % 60, t % 60)
}

fn config_dir() -> Option<std::path::PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(std::path::PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
    }?;
    let dir = base.join("SimpleVMManager");
    // while the app was briefly called XenCraft, its settings, servers and sessions lived there:
    // they move back once
    let old = base.join("XenCraft");
    if !dir.exists() && old.is_dir() {
        let _ = std::fs::rename(&old, &dir);
    }
    Some(dir)
}

struct Settings {
    theme: ThemeMode,
    sidebar_open: bool,
    /// The interface's size, 1.0 being the system's own (Settings, or Ctrl + / Ctrl -).
    zoom: f32,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { theme: ThemeMode::System, sidebar_open: true, zoom: 1.0 }
    }
}

fn load_settings() -> Settings {
    let mut s = Settings::default();
    let Some(dir) = config_dir() else { return s };
    for line in std::fs::read_to_string(dir.join("settings.txt")).unwrap_or_default().lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "theme" => s.theme = ThemeMode::parse(v).unwrap_or(s.theme),
                "sidebar" => s.sidebar_open = v.trim() != "folded",
                "zoom" => s.zoom = v.trim().parse::<f32>().ok().filter(|z| (0.5..=3.0).contains(z)).unwrap_or(1.0),
                _ => {}
            }
        }
    }
    s
}

fn save_settings(s: &Settings) {
    let Some(dir) = config_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let text = format!("theme={}\nsidebar={}\nzoom={:.2}\n", s.theme.as_str(), if s.sidebar_open { "open" } else { "folded" }, s.zoom);
    let _ = std::fs::write(dir.join("settings.txt"), text);
}

/// Earlier servers and the account used on each, newest first.
fn load_history() -> Vec<(String, String)> {
    let Some(dir) = config_dir() else { return vec![] };
    let text = std::fs::read_to_string(dir.join("servers.txt")).unwrap_or_default();
    let mut out: Vec<(String, String)> = vec![];
    for line in text.lines() {
        let mut parts = line.split('\t');
        let h = parts.next().unwrap_or("").trim().to_string();
        let u = parts.next().unwrap_or("root").trim().to_string();
        if !h.is_empty() && !out.iter().any(|(x, _)| *x == h) {
            out.push((h, if u.is_empty() { "root".into() } else { u }));
        }
    }
    out
}

/// The servers to connect to at startup (their passwords are in the keychain, not here).
fn load_session() -> Vec<(String, String)> {
    let Some(dir) = config_dir() else { return vec![] };
    std::fs::read_to_string(dir.join("session.txt"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(h, u)| (h.trim().to_string(), u.trim().to_string()))
        .filter(|(h, u)| !h.is_empty() && !u.is_empty())
        .collect()
}

fn save_session(list: &[(String, String)]) {
    let Some(dir) = config_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let text: Vec<String> = list.iter().map(|(h, u)| format!("{}\t{}", h, u)).collect();
    let _ = std::fs::write(dir.join("session.txt"), text.join("\n"));
}

fn session_add(host: &str, user: &str) {
    let mut list = load_session();
    list.retain(|(h, _)| !h.eq_ignore_ascii_case(host));
    list.push((host.to_string(), user.to_string()));
    save_session(&list);
}

fn session_remove(host: &str, user: &str) {
    let mut list = load_session();
    list.retain(|(h, u)| !(h.eq_ignore_ascii_case(host) && u == user));
    save_session(&list);
}

/// The pool servers' addresses last seen through `host` (members.txt: the server, a tab, the
/// addresses with commas).
fn load_members(host: &str) -> Vec<String> {
    let Some(dir) = config_dir() else { return vec![] };
    std::fs::read_to_string(dir.join("members.txt"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .find(|(h, _)| h.trim().eq_ignore_ascii_case(host))
        .map(|(_, m)| m.split(',').map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect())
        .unwrap_or_default()
}

fn save_members(host: &str, members: &[String]) {
    let Some(dir) = config_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let f = dir.join("members.txt");
    let mut lines = vec![format!("{}\t{}", host, members.join(","))];
    for line in std::fs::read_to_string(&f).unwrap_or_default().lines() {
        if !line.split('\t').next().unwrap_or("").trim().eq_ignore_ascii_case(host) && lines.len() < 20 {
            lines.push(line.to_string());
        }
    }
    let _ = std::fs::write(f, lines.join("\n"));
}

fn remember_host(host: &str, user: &str) {
    let Some(dir) = config_dir() else { return };
    let f = dir.join("servers.txt");
    let mut lines = vec![format!("{}\t{}", host, user)];
    for line in std::fs::read_to_string(&f).unwrap_or_default().lines() {
        if line.split('\t').next().unwrap_or("").trim() != host && lines.len() < 20 {
            lines.push(line.to_string());
        }
    }
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(f, lines.join("\n"));
}

/// Inter for all text, bundled so the app looks the same on every system (assets/fonts, SIL Open
/// Font License); its SemiBold for titles. egui's own fonts stay behind it for symbols Inter lacks.
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("inter".into(), egui::FontData::from_static(include_bytes!("../assets/fonts/Inter-Regular.ttf")));
    fonts.font_data.insert("inter-semibold".into(), egui::FontData::from_static(include_bytes!("../assets/fonts/Inter-SemiBold.ttf")));
    let regular = fonts.families.entry(FontFamily::Proportional).or_default();
    regular.insert(0, "inter".into());
    let mut bold = regular.clone();
    bold.insert(0, "inter-semibold".into());
    fonts.families.insert(FontFamily::Name("bold".into()), bold);
    ctx.set_fonts(fonts);
    ctx.style_mut(|s| {
        use egui::TextStyle::*;
        s.text_styles.insert(Body, FontId::proportional(13.0));
        s.text_styles.insert(Button, FontId::proportional(13.0));
        s.text_styles.insert(Small, FontId::proportional(11.0));
        s.text_styles.insert(Heading, FontId::new(18.0, FontFamily::Name("bold".into())));
    });
}
