//! The main window: menus, toolbar, foldable sidebar with the resource tree, header, tabs, dialogs.
//! Every colour comes from the light or dark palette (theme.rs).

use crate::conn::*;
use crate::console::{self, ConsoleView};
use crate::alerts::{AlertSettings, Severity, Watched, XenMessage};
use crate::history::{Records, Span};
use crate::isolib::{mount_trouble, IsoAction, IsoDialog, MountTrouble};
use crate::newnet::{NetworkAction, NetworkDialog, PoolCards};
use crate::newsr::{Guest, StorageAction, StorageDialog};
use crate::newvm::{NewVmAction, NewVmDialog};
use crate::movevm::{MoveAction, MoveDialog};
use crate::pool_update::{PlanSr, PlanVm, PoolUpdateAction, PoolUpdateDialog, PoolView, UpdateRun};
use crate::upload::{UploadAction, UploadDialog};
use crate::secrets;
use crate::update;
use crate::icons::*;
use crate::model::*;
use crate::theme::{self, Pal, ThemeMode};
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontFamily, FontId, Key, Modifiers, Rect, Rounding, Sense, Stroke, Ui};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod alerts;
mod host_overview;
mod overview;
mod performance;
use alerts::{AlertsView, MessageCache, RulesAct, RulesDialog};
use overview::{OverviewEdit, Thumb};

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
    /// Its pool's coordinator, as last seen (kept in a file): its name and where its Wake-on-LAN
    /// signal goes, to turn it on when it's off and can't be asked.
    wake: Option<(String, WakeTarget)>,
}

/// A server told to turn on, till it answers (see `check_waking`).
struct Waking {
    sid: u32,
    /// Which (None: a connection's own server, known only from the last time it answered).
    host: Option<String>,
    name: String,
    since: Instant,
    log: usize,
}

/// How long a server told to turn on is waited for, and after how long that's longer than usual.
const WAKE_LIMIT: Duration = Duration::from_secs(15 * 60);
const WAKE_SLOW_MIN: u64 = 10;

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
    /// New VM storage was made (its reference), or not; `log`: its entry in Activities.
    StorageDone { sid: u32, st: NfsStorage, log: usize, result: Result<String, ApiError> },
    /// The folders an NFS server shares, asked for by the New VM Storage dialog.
    Exports { server: String, result: Result<Vec<String>, ApiError> },
    /// A new network was made (its reference), or not; `log`: its entry in Activities.
    NetworkMade { sid: u32, nw: NewNetwork, log: usize, result: Result<String, ApiError> },
    /// A network was deleted, or not.
    NetworkDeleted { sid: u32, name: String, log: usize, result: Result<(), ApiError> },
    /// A rescan (forget false) or disconnect (forget true) of a storage ended.
    SrDone { sid: u32, sr: String, name: String, forget: bool, result: Result<(), ApiError> },
    /// Mounting a storage again ended: each server tried and how it went; `auto`: the app did it
    /// by itself on finding it unmounted.
    SrRepaired { sid: u32, sr: String, name: String, auto: bool, result: Result<Vec<(String, Result<(), ApiError>)>, ApiError> },
    /// A snapshot job on VM `vm` ended (taking one, reverting to one, deleting one): its log line,
    /// and what to call it if it failed; what's worth saying though it worked (disks left behind).
    SnapDone { sid: u32, vm: String, snap: Option<String>, log: usize, title: &'static str, text: String, result: Result<Vec<String>, ApiError> },
    /// Making a template (`template`) or a copy of VM `vm` ended: the new one, or why not.
    Made { sid: u32, vm: String, name: String, log: usize, title: &'static str, text: String, template: bool, result: Result<String, ApiError> },
    /// Deleting a template of the user's ended: the disks left behind, or why not.
    TemplateDeleted { sid: u32, tpl: String, name: String, log: usize, result: Result<Vec<String>, ApiError> },
    /// A change to a VM ended.
    Edited { sid: u32, vm: String, log: usize, text: String, result: Result<(), ApiError> },
    /// A running VM's last ten minutes, for its Overview.
    Perf { vm: String, result: Result<Perf, ApiError> },
    /// A server's last ten minutes and its VMs', for its Overview and its pool's.
    HostPerf { host: String, result: Result<HostPerf, ApiError> },
    /// A VM's or a server's records over a span, read for its Performance tab (by its reference).
    History { key: String, span: Span, result: Result<Records, ApiError> },
    /// A pool's messages (its alerts): all of them (`since` None), or the new ones.
    Messages { sid: u32, since: Option<i64>, result: Result<Vec<XenMessage>, ApiError> },
    /// Alerts dismissed on their pool, or why not.
    Dismissed { sid: u32, refs: Vec<String>, result: Result<(), ApiError> },
    /// What's set for the Alerts dialog's VM, server or storage (by its reference); and saved.
    AlertSettingsRead { obj: String, result: Result<AlertSettings, ApiError> },
    AlertSettingsSaved { obj: String, log: usize, result: Result<(), ApiError> },
    /// A pool's updates read; fetched from its channel; installed (or why not).
    PoolUpdatesRead { sid: u32, result: Result<PoolUpdates, ApiError> },
    PoolSynced { sid: u32, log: usize, result: Result<String, ApiError> },
    PoolUpdated { sid: u32, log: usize, result: Result<String, String> },
    /// A server is restarting for an update, or is back from it.
    UpdateRestarting { host: String, back: bool },
    /// Making a storage the pool's default ended.
    DefaultSrSet { sid: u32, log: usize, name: String, result: Result<(), ApiError> },
    /// Moving a VM ended.
    Moved { sid: u32, vm: String, name: String, log: usize, result: Result<(), ApiError> },
    /// Asking a server to take VMs again ended.
    HostEnabled { sid: u32, host: String, name: String, log: usize, result: Result<(), ApiError> },
    /// A look at a running VM's screen, for its Overview (width, height, RGBA), or why not.
    Thumb { vm: String, result: Result<(usize, usize, Vec<u8>), String> },
    /// Saving a snapshot's description ended.
    SnapDescribed { sid: u32, vm: String, snap: String, name: String, text: String, result: Result<(), ApiError> },
    /// Giving a detached library its settings back ended.
    Reattached { sid: u32, sr: String, lib: IsoLibrary, result: Result<(), ApiError> },
    /// Making a new VM ended: its name, and whether it was to start.
    /// A new VM made (its reference), or why not; `start`: it's being started next.
    VmCreated { sid: u32, name: String, start: bool, result: Result<String, ApiError> },
    /// The new VM `vm` started, or why not.
    VmStarted { sid: u32, vm: String, name: String, result: Result<(), ApiError> },
    /// Whether a storage answered, for the New VM dialog.
    SrChecked { sid: u32, sr: String, host: Option<String>, result: Result<(), ApiError> },
    /// Deleting an ISO file ended.
    IsoDeleted { sid: u32, sr: String, vdi: String, name: String, result: Result<(), ApiError> },
    /// Shutting a server down ended.
    HostShutdown { sid: u32, host: String, name: String, reboot: bool, log: usize, result: Result<(), ApiError> },
    /// A server told to turn on (`key` as in `host_waking`): from this computer, and through the
    /// pool, each when it could be.
    PoweredOn { sid: u32, key: String, name: String, log: usize, here: Option<Result<(), ApiError>>, pool: Option<Result<(), ApiError>> },
    /// In maintenance mode now (the VMs it shut down, how many it moved), or not.
    MaintenanceIn { sid: u32, host: String, name: String, log: usize, result: Result<(Vec<String>, usize), ApiError> },
    /// Out of maintenance mode (the VMs that couldn't come back, each with why), or not.
    MaintenanceOut { sid: u32, host: String, name: String, log: usize, result: Result<Vec<String>, ApiError> },
    /// Deleting a VM ended: the disks left behind, or why it wasn't deleted.
    VmDeleted { sid: u32, vm: String, name: String, result: Result<Vec<String>, ApiError> },
    /// A link in the Upload dialog was checked.
    LinkChecked { url: String, result: Result<WebFile, String> },
    /// GitHub's latest release, or why it couldn't be had; `asked`: from Settings' Check now.
    UpdateChecked { asked: bool, result: Result<update::Release, String> },
    /// A new version is in place of this one (its file), or why not.
    UpdateInstalled { result: Result<PathBuf, String> },
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

/// A VM New Template can be made from: its pool, the VM, its name, how it's listed, and why it
/// can't be now (a caution's title and text), if it can't.
struct TemplateSource {
    sid: u32,
    vm: String,
    name: String,
    label: String,
    why: Option<(String, String)>,
}

/// A row of a list of ISO files: its pool, its library and the file; `lib`, in the list of every
/// library, the library's name (and its pool's, with several pools).
struct IsoRow {
    sid: u32,
    sr: String,
    lib: Option<String>,
    file: IsoFile,
}

/// An ISO file's description being edited in its row.
struct IsoEdit {
    vdi: String,
    text: String,
    /// Put the cursor in the box on the next frame.
    focus: bool,
}

/// A VM or template being made when its server went quiet (or the app closed): once that
/// server's back, what's there under its name that wasn't before may be half made.
#[derive(Clone, Debug, PartialEq)]
struct Unfinished {
    host: String,
    user: String,
    name: String,
    /// What already had that name.
    before: Vec<String>,
    /// Its job ended without the server's word (or the app closed meanwhile): look once it's back.
    lost: bool,
}

#[derive(Clone)]
struct LogEntry {
    time: String,
    sid: u32,
    server: String,
    obj_ref: Option<String>,
    obj: String,
    text: String,
    status: String,
    /// When it began; and, for something that runs a while (it began "In progress"), how far its
    /// task on the server is: listed under Activities.
    started: Instant,
    progress: Option<Arc<Progress>>,
    /// Listed under Activities: something that ran a while, or a server going or coming back.
    activity: bool,
    /// For a job the app takes through several steps: what closing the app now would leave
    /// half done (the step under way finishes on the server, the rest doesn't happen).
    if_closed: Option<String>,
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
    /// "Shared Storage", "Virtual Networks" or "Templates" in a pool (rref is "srs", "nets" or "tpls").
    Folder,
    /// A template the user made.
    Template,
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
    /// In the tree but not drawn (Templates and Virtual Networks, on the toolbar's pages instead):
    /// its pages still open.
    hidden: bool,
}

impl Node {
    fn new(key: String, text: String, icon: IconKind, kind: Kind, sid: Option<u32>, rref: Option<String>) -> Node {
        Node { key, text, icon, kind, sid, rref, note: String::new(), children: vec![], hidden: false }
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
    /// A VM's, a server's or a pool's page: what matters most, its load, and (a VM's) its main
    /// settings to change.
    Overview,
    /// A VM's or a server's load over ten minutes, two hours, a week or a year, in full.
    Performance,
    /// Every pool's alerts (All servers; the toolbar's bell, not a tab in the row).
    Alerts,
    General,
    /// Their less used details (General, under another name).
    Details,
    Storage,
    Networking,
    Console,
    IsoFiles,
    /// Every pool's ISO files, by library (All servers).
    IsoLibrary,
    /// Every pool's storage for VMs' disks (All servers).
    VmStorage,
    /// Every pool's networks (All servers).
    Networks,
    Snapshots,
    /// The Templates folder's list.
    Templates,
    Logs,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Home => "Home",
            Tab::Search => "Search",
            Tab::Overview => "Overview",
            Tab::Performance => "Performance",
            Tab::Alerts => "Alerts",
            Tab::General => "General",
            Tab::Details => "Details",
            Tab::Storage => "Storage",
            Tab::Networking => "Networking",
            Tab::Console => "Console",
            Tab::IsoFiles => "ISO Files",
            Tab::IsoLibrary => "ISO Library",
            Tab::VmStorage => "VM Storage",
            Tab::Networks => "Networks",
            Tab::Snapshots => "Snapshots",
            Tab::Templates => "Templates",
            Tab::Logs => "Logs",
        }
    }
}

enum Dialog {
    None,
    /// A VM's, a server's or a storage's alert limits, and its pool's email for alerts.
    AlertRules(Box<RulesDialog>),
    AddServer { server: String, user: String, password: String, error: String, remember: bool },
    Confirm { action: usize, sid: u32, vm: String, title: String, text: String },
    Message { title: String, text: String },
    Iso(Box<IsoDialog>),
    NewStorage(Box<StorageDialog>),
    Upload(Box<UploadDialog>),
    /// A pool's updates: what's waiting, fetching, installing.
    PoolUpdate(Box<PoolUpdateDialog>),
    /// Moving a VM to another server or storage.
    MoveVm(Box<MoveDialog>),
    NewVm(Box<NewVmDialog>),
    /// Disconnect (forget) an ISO library?
    ConfirmSr { sid: u32, sr: String, text: String },
    /// Deleting an ISO file: server, library, disc, its name, the question.
    ConfirmDeleteIso { sid: u32, sr: String, vdi: String, name: String, text: String },
    /// Shutting a server down: which, its name, the VMs that move elsewhere and those that stop, and
    /// whether it's the pool's coordinator (with other servers) or the only server.
    ConfirmHostShutdown { sid: u32, host: String, name: String, moving: Vec<String>, stopping: Vec<String>, coordinator: bool, alone: bool, reboot: bool },
    /// Putting a server in maintenance mode: which, its name, the VMs that move elsewhere and those
    /// that stop, and whether it's the pool's coordinator (with other servers).
    ConfirmMaintenance { sid: u32, host: String, name: String, moving: Vec<String>, stopping: Vec<String>, coordinator: bool },
    /// Taking it out: the VMs maintenance moved off it (still running elsewhere) and shut down
    /// (still off), and whether they come back.
    ExitMaintenance { sid: u32, host: String, name: String, moved: Vec<String>, stopped: Vec<String>, back: bool },
    /// A newer version: offered, downloading, or why it couldn't.
    Update(Box<UpdateDialog>),
    /// The app's settings (the gear on the toolbar); `startup`: the servers it connects to as it
    /// starts (read when it opens).
    Settings { startup: Vec<(String, String)> },
    /// Deleting a VM: whether it's on (turned off first), its disks, how many snapshots it has,
    /// and whether its disks go too (ticked to begin with).
    ConfirmDeleteVm { sid: u32, vm: String, name: String, running: bool, disks: Vec<VmDisk>, snapshots: usize, delete_disks: bool },
    /// Take Snapshot: of VM `vm`; `can_memory`: it's running, so its memory can go in too
    /// (`memory_size` of it); `focus`: put the cursor in the name on the next frame.
    TakeSnapshot { sid: u32, vm: String, vm_name: String, name: String, description: String, memory: bool, can_memory: bool, memory_size: i64, focus: bool },
    /// Reverting to snapshot `snap`: what happens, in words, and whether a snapshot of now is taken first.
    ConfirmRevert { sid: u32, snap: String, vm_name: String, snap_name: String, text: String, first: bool },
    ConfirmDeleteSnapshot { sid: u32, snap: String, vm_name: String, snap_name: String, memory: bool },
    /// Make Template: from `source` (VM `vm`, off, or one of its snapshots), `from` saying which in
    /// words; `power_cut`: a snapshot of its disks taken while it was running. `pick`: the VMs to
    /// choose from (New Template, on the Templates page), empty when it's for one VM.
    MakeTemplate { sid: u32, vm: String, source: String, from: String, name: String, description: String, power_cut: bool, focus: bool, pick: Vec<TemplateSource> },
    /// New Network.
    NewNetwork(Box<NetworkDialog>),
    /// Deleting a network: pool, network, its name, what happens in words.
    ConfirmDeleteNetwork { sid: u32, net: String, name: String, text: String },
    /// Copy VM: where its disks can go (reference, label, whether it's where they are now: an
    /// instant copy), and which.
    CopyVm { sid: u32, vm: String, vm_name: String, name: String, stores: Vec<(String, String, bool)>, store: usize, focus: bool },
    ConfirmDeleteTemplate { sid: u32, tpl: String, name: String, disk: i64 },
    /// Something was being made when contact with the server was lost, and is there now (maybe
    /// half made): delete it?
    ConfirmLeftover { sid: u32, name: String, refs: Vec<String>, server: String },
    /// Before a change that loses something (a disk, a network card).
    ConfirmEditVm { sid: u32, vm: String, change: VmChange, title: String, text: String },
    /// A log entry in full (from Activities or Logs): what happened, and the calls made to the
    /// server with its answers; when the Copy button was last used.
    LogDetail { index: usize, copied: Option<Instant> },
    /// Closing the app while jobs would be left half done: each one's line and what's left; the
    /// dialog that was open, back if the app stays open.
    ConfirmClose { items: Vec<(String, String)>, back: Box<Dialog> },
}

/// The Update dialog: the release, this copy's file when it can replace itself, the download under
/// way, and what went wrong.
struct UpdateDialog {
    release: update::Release,
    exe: Option<PathBuf>,
    transfer: Option<Arc<Transfer>>,
    error: Option<String>,
}

enum Intent {
    /// The Alerts page (the toolbar's bell).
    Alerts,
    /// Dismiss these alerts: each its pool and its message.
    DismissAlerts(Vec<(u32, String)>),
    /// The Alerts dialog for a VM, a server or a storage: its pool, XenAPI class, reference, name.
    AlertRules(u32, &'static str, String, String, Watched),
    Select(String),
    Toggle(String),
    Action(usize),
    AddServer,
    Demo,
    Remove(u32),
    ForgetPassword(u32),
    /// The ISO Library page: every ISO file of every pool (the toolbar's ISO Library).
    IsoLibrary,
    /// The New ISO Library dialog.
    NewIsoLibrary,
    /// The VM Storage page: every storage for VMs' disks of every pool.
    VmStorage,
    /// The New VM Storage dialog.
    NewStorage,
    /// The Templates page: every pool's templates (the toolbar's Templates).
    Templates,
    /// Make Template, choosing the VM.
    NewTemplate,
    /// The Networks page: every pool's networks (the toolbar's Networks).
    Networks,
    /// The New Network dialog.
    NewNetwork,
    /// Ask before deleting a network: pool, network.
    AskDeleteNetwork(u32, String),
    /// A pool's updates (this pool's, else the first).
    PoolUpdates(Option<u32>),
    RescanSr(u32, String),
    AskForgetSr(u32, String),
    /// Mount a storage again where it isn't (asking where it is, if the pool has forgotten).
    RepairSr(u32, String),
    /// Open the Upload dialog of an ISO library.
    Upload(u32, String),
    /// Open the New VM dialog: for this pool (else the selected one), with this ISO's disc.
    NewVm(Option<u32>, Option<String>),
    /// Ask before deleting an ISO file: server, library, disc.
    AskDeleteIso(u32, String, String),
    /// Ask before deleting a VM: server, VM.
    AskDeleteVm(u32, String),
    /// Open the Take Snapshot dialog: server, VM.
    TakeSnapshot(u32, String),
    /// Show a log entry in full.
    ShowLog(usize),
    /// Change a VM (its Overview): server, VM, the change.
    EditVm(u32, String, VmChange),
    /// Ask first, then change it (removing a disk or a card): server, VM, the change, the
    /// question's title and text.
    AskEditVm(u32, String, VmChange, String, String),
    /// Open the Make Template dialog: server, a VM that's off or a snapshot.
    MakeTemplate(u32, String),
    /// Open the Copy VM dialog: server, VM.
    CopyVm(u32, String),
    /// Open the New VM dialog on this template of the user's: server, template.
    NewVmFromTemplate(u32, String),
    /// Ask before deleting a template of the user's: server, template.
    AskDeleteTemplate(u32, String),
    /// Ask before reverting a VM to a snapshot, or deleting one: server, snapshot.
    AskRevert(u32, String),
    AskDeleteSnapshot(u32, String),
    /// Save a snapshot's description: server, snapshot, text.
    SaveSnapshotDescription(u32, String, String),
    /// Ask before shutting a server down: pool, server.
    /// Ask before shutting a server down, or (true) restarting it.
    AskShutdownHost(u32, String, bool),
    /// A server that isn't taking VMs (disabled) takes them again.
    EnableHost(u32, String),
    /// Turn a server on (Wake-on-LAN): pool, server (None: the connection's own server, known
    /// from the last time it answered).
    PowerOn(u32, Option<String>),
    /// Ask before putting a server in maintenance mode, or taking it out: pool, server.
    AskMaintenance(u32, String),
    /// Move a VM (to this server, when it was dropped on one).
    MoveVm(u32, String, Option<String>),
    /// Make a storage the pool's default.
    MakeDefaultSr(u32, String),
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
    /// Jobs under way that log only when they end, by key: their "In progress" line.
    jobs: HashMap<String, usize>,
    /// What was being made when contact was lost, kept in a file till it's sorted out.
    unfinished: Vec<Unfinished>,
    /// What's being typed on the shown VM's Overview.
    ov: OverviewEdit,
    /// Running VMs' load, by VM: when it was read, and what came (or why not).
    perf: HashMap<String, (Instant, Result<Perf, String>)>,
    perf_busy: HashSet<String>,
    /// Servers' load and their VMs', by server: when it was read, and what came (or why not).
    host_perf: HashMap<String, (Instant, Result<HostPerf, String>)>,
    host_perf_busy: HashSet<String>,
    /// The Performance tab: how far back it looks, which of its groups are open (by their keys,
    /// for every VM or server alike), and the records read for it by VM or server and span (when,
    /// and what came or why not), those being read.
    perf_span: Span,
    perf_open: HashSet<String>,
    history: HashMap<(String, Span), (Instant, Result<Records, String>)>,
    history_busy: HashSet<(String, Span)>,
    /// The charts last made from those records (made again from a new reading).
    perf_charts: Option<performance::PerfCharts>,
    /// Each pool's messages for people (its alerts), and the pools whose are being read; the
    /// messages being dismissed (hidden meanwhile); since when each problem this app finds has
    /// been there; the Alerts page's choices.
    messages: HashMap<u32, MessageCache>,
    messages_busy: HashSet<u32>,
    dismissing: HashSet<String>,
    app_alerts_since: HashMap<String, i64>,
    alerts_view: AlertsView,
    /// The last look at each running VM's screen shown in an Overview, and those being taken.
    thumbs: HashMap<String, Thumb>,
    thumb_busy: HashSet<String>,
    /// VMs with a change being made: what; and the changes waiting their turn (one at a time
    /// for each VM).
    vm_saving: HashMap<String, String>,
    edit_queue: Vec<(u32, String, VmChange)>,
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
    /// Look for a newer version as the app starts (Settings).
    check_updates: bool,
    /// Looking for one now; the answer to a look asked for in Settings ("You have the latest").
    update_checking: bool,
    update_note: Option<String>,
    /// A newer version found while another dialog was open: offered once it closes.
    pending_update: Option<update::Release>,
    pal: Pal,
    applied_dark: Option<bool>,
    tab: Tab,
    /// The tab shown before the console was opened (the Console button opens it, and closes it
    /// back to that).
    before_console: Tab,
    closed_sections: HashSet<String>,
    dialog: Dialog,
    console: Option<ConsoleView>,
    /// Earlier servers and the account used on each, newest first.
    known_hosts: Vec<(String, String)>,
    /// None while still asking the system whether it has a keychain.
    keychain: Option<bool>,
    /// Storage being rescanned, repaired or disconnected right now.
    sr_busy: HashSet<String>,
    /// Mount storage again by itself on finding it unmounted (Settings).
    auto_repair: bool,
    /// Storage it has done that for while it stays unmounted (once each), and what to do next.
    auto_tried: HashSet<(u32, String)>,
    auto_queue: Vec<(u32, String)>,
    /// Storage checks under way for the New VM dialog: pool, storage, server.
    sr_checking: HashSet<(u32, String, Option<String>)>,
    /// The ISO file whose description is being typed in, if any.
    iso_edit: Option<IsoEdit>,
    /// Descriptions being saved, by disc, shown until the server answers.
    iso_saving: HashMap<String, String>,
    /// The snapshot whose description is being typed in, if any (keyed by the snapshot).
    snap_edit: Option<IsoEdit>,
    /// Snapshot descriptions being saved, by snapshot, shown until the server answers.
    snap_saving: HashMap<String, String>,
    /// Snapshots being deleted.
    snap_deleting: HashSet<String>,
    /// ISO files being deleted, by disc.
    iso_deleting: HashSet<String>,
    /// VMs being deleted right now.
    vm_deleting: HashSet<String>,
    /// Servers being shut down (or restarted) right now.
    host_busy: HashSet<String>,
    /// Servers restarted from here that aren't back yet.
    host_rebooting: HashMap<String, i64>,
    /// Servers told to turn on, by reference ("srv:<id>" for a connection's own server known
    /// only from the last time).
    host_waking: HashMap<String, Waking>,
    /// Servers going into maintenance mode (true) or coming out of it (false).
    host_maint: HashMap<String, bool>,
    /// Each pool's updates as last read (its /updates), and those being read; those fetching from
    /// their channel (and why the last fetch failed); updates under way.
    pool_updates: HashMap<u32, (Instant, Result<PoolUpdates, String>)>,
    pool_updates_busy: HashMap<u32, Instant>,
    pool_syncing: HashSet<u32>,
    pool_sync_error: HashMap<u32, String>,
    pool_runs: HashMap<u32, Arc<UpdateRun>>,
    /// Servers being asked to take VMs again.
    host_enabling: HashSet<String>,
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
    /// Close anyway was chosen: the window goes without asking again.
    closing: bool,
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
            jobs: HashMap::new(),
            unfinished: load_unfinished(),
            ov: OverviewEdit::default(),
            perf: HashMap::new(),
            perf_busy: HashSet::new(),
            host_perf: HashMap::new(),
            host_perf_busy: HashSet::new(),
            // (a check's screenshot may ask for another span: SVM_PERF_SPAN=minutes|hours|year)
            perf_span: match std::env::var("SVM_PERF_SPAN").ok().filter(|_| shot).as_deref() {
                Some("minutes") => Span::Minutes,
                Some("hours") => Span::Hours,
                Some("year") => Span::Year,
                _ => Span::Week,
            },
            perf_open: ["vms", "disk:0"].map(String::from).into_iter().collect(),
            history: HashMap::new(),
            history_busy: HashSet::new(),
            perf_charts: None,
            messages: HashMap::new(),
            messages_busy: HashSet::new(),
            dismissing: HashSet::new(),
            app_alerts_since: HashMap::new(),
            alerts_view: AlertsView::default(),
            thumbs: HashMap::new(),
            thumb_busy: HashSet::new(),
            vm_saving: HashMap::new(),
            edit_queue: vec![],
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
            check_updates: saved.updates,
            update_checking: false,
            update_note: None,
            pending_update: None,
            pal: theme::LIGHT,
            applied_dark: None,
            tab: Tab::Home,
            before_console: Tab::Overview,
            closed_sections: HashSet::new(),
            dialog: Dialog::None,
            console: None,
            known_hosts: load_history(),
            keychain: None,
            saved: HashMap::new(),
            sr_busy: HashSet::new(),
            auto_repair: saved.repair,
            auto_tried: HashSet::new(),
            auto_queue: vec![],
            sr_checking: HashSet::new(),
            iso_edit: None,
            iso_saving: HashMap::new(),
            snap_edit: None,
            snap_saving: HashMap::new(),
            snap_deleting: HashSet::new(),
            iso_deleting: HashSet::new(),
            vm_deleting: HashSet::new(),
            host_busy: HashSet::new(),
            host_rebooting: HashMap::new(),
            host_waking: HashMap::new(),
            host_maint: HashMap::new(),
            pool_updates: HashMap::new(),
            pool_updates_busy: HashMap::new(),
            pool_syncing: HashSet::new(),
            pool_sync_error: HashMap::new(),
            pool_runs: HashMap::new(),
            host_enabling: HashSet::new(),
            pending_select: None,
            creating: 0,
            closing: false,
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
        if !shot && app.check_updates {
            app.look_for_update(false, &cc.egui_ctx);
        }
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
            match app.shot_show.as_str() {
                "sr-unmounted" | "sr-auto-repair" => std::env::set_var("SVM_DEMO_UNMOUNTED", "sr-iso"),
                "host-down" => std::env::set_var("SVM_DEMO_HOST_DOWN", "host-b"),
                "sr-repair-failed" => {
                    std::env::set_var("SVM_DEMO_UNMOUNTED", "sr-iso");
                    std::env::set_var("SVM_DEMO_REPAIR_FAILS", "sr-iso");
                }
                "sr-detached" => std::env::set_var("SVM_DEMO_DETACHED", "sr-iso"),
                _ => {}
            }
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
            save_settings(&Settings { theme: self.theme_mode, sidebar_open: self.sidebar_open, zoom: self.zoom, updates: self.check_updates, repair: self.auto_repair });
        }
    }

    // ------------------------------------------------------------------ servers

    fn add_server(&mut self, conn: Arc<dyn Connection>) {
        let id = self.next_id;
        self.next_id += 1;
        let host = conn.hostname();
        let wake = if conn.is_demo() { None } else { load_wake(&host) };
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
            wake,
        });
        self.add_log(id, &host, None, &host, &format!("Connecting to {}", host), "In progress");
        self.select(format!("srv:{}", id));
    }

    /// Rescans a storage, or disconnects (forgets) it, in the background.
    fn sr_job(&mut self, sid: u32, sr: String, forget: bool, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let name = s.data.as_ref().and_then(|d| d.sr(&sr)).map(|x| x.name.clone()).unwrap_or_default();
        let (conn, tx, ctx, server) = (s.conn.clone(), self.tx.clone(), ctx.clone(), s.name());
        self.sr_busy.insert(sr.clone());
        let text = if forget { format!("Disconnecting ISO library '{}'", name) } else { format!("Rescanning '{}'", name) };
        let prog = self.job(format!("sr:{}", sr), sid, &server, Some(&sr), &name, &text);
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = if forget { conn.forget_sr(&sr) } else { conn.scan_sr(&sr) };
            let _ = tx.send(Msg::SrDone { sid, sr, name, forget, result });
            ctx.request_repaint();
        });
    }

    /// Repair: mounts a storage again where it isn't; for a library the pool has no settings for
    /// any more, asks where it is first.
    fn ask_repair(&mut self, sid: u32, sr: String, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let Some(x) = s.data.as_ref().and_then(|d| d.sr(&sr)) else { return };
        let (detached, iso, pool, uuid, name, shared) = (x.links.is_empty(), x.is_iso(), s.name(), x.uuid.clone(), x.name.clone(), x.shared);
        if !detached {
            self.repair_job(sid, sr, false, ctx);
        } else if iso {
            self.dialog = Dialog::Iso(Box::new(IsoDialog::reattach(sid, pool, sr, uuid, name, shared)));
        } else {
            let text = format!("The pool has no settings for reaching '{}' any more, so it can't be mounted again from here. Attach it again with XenCenter or xe.", name);
            self.dialog = Dialog::Message { title: "Repair".into(), text };
        }
    }

    /// Mounts a storage again where it isn't, in the background.
    fn repair_job(&mut self, sid: u32, sr: String, auto: bool, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let name = s.data.as_ref().and_then(|d| d.sr(&sr)).map(|x| x.name.clone()).unwrap_or_default();
        let (conn, tx, ctx, server) = (s.conn.clone(), self.tx.clone(), ctx.clone(), s.name());
        self.sr_busy.insert(sr.clone());
        let text = if auto { format!("Found '{}' unmounted; mounting it again", name) } else { format!("Mounting '{}' again", name) };
        let prog = self.job(format!("sr:{}", sr), sid, &server, Some(&sr), &name, &text);
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = conn.repair_sr(&sr);
            let _ = tx.send(Msg::SrRepaired { sid, sr, name, auto, result });
            ctx.request_repaint();
        });
    }

    /// Gives a detached library the settings typed in the Repair dialog, in the background.
    fn reattach_job(&mut self, sid: u32, sr: String, lib: IsoLibrary, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let (conn, tx, ctx, server) = (s.conn.clone(), self.tx.clone(), ctx.clone(), s.name());
        self.sr_busy.insert(sr.clone());
        let prog = self.job(format!("sr:{}", sr), sid, &server, Some(&sr), &lib.name, &format!("Mounting '{}' again from {}", lib.name, lib.location));
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = conn.reattach_sr(&sr, &lib);
            let _ = tx.send(Msg::Reattached { sid, sr, lib, result });
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
        let (conn, tx, ctx, server) = (s.conn.clone(), self.tx.clone(), ctx.clone(), s.name());
        self.creating += 1;
        let what = if spec.from_template { format!("Creating VM '{}' from a template", spec.name) } else { format!("Creating VM '{}'", spec.name) };
        let prog = self.job(format!("newvm:{}:{}", sid, spec.name), sid, &server, None, &spec.name, &what);
        if let Some(&log) = self.jobs.get(&format!("newvm:{}:{}", sid, spec.name)) {
            self.if_closed(log, format!("'{}' is left half made: when the app opens again, it asks whether to delete it.", spec.name));
        }
        self.remember_unfinished(sid, &spec.name);
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
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

    /// Asks GitHub for the latest release in the background (`asked`: from Settings' Check now).
    fn look_for_update(&mut self, asked: bool, ctx: &egui::Context) {
        if self.update_checking {
            return;
        }
        self.update_checking = true;
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = update::latest();
            let _ = tx.send(Msg::UpdateChecked { asked, result });
            ctx.request_repaint();
        });
    }

    /// Downloads the new version into this copy's place, in the background.
    fn install_update(&mut self, ctx: &egui::Context) {
        let Dialog::Update(d) = &mut self.dialog else { return };
        let (Some(exe), Some((url, size))) = (d.exe.clone(), d.release.download.clone()) else { return };
        let t = Arc::new(Transfer::default());
        d.transfer = Some(t.clone());
        d.error = None;
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = update::install(&url, size, &exe, &t).map(|_| exe);
            let _ = tx.send(Msg::UpdateInstalled { result });
            ctx.request_repaint();
        });
    }

    /// Why the app shouldn't restart right now (an upload or a VM being made or deleted).
    fn update_blocked(&self) -> Option<&'static str> {
        if self.uploads.values().any(Upload::active) {
            Some("An ISO file is uploading: wait for it to finish first.")
        } else if self.creating > 0 {
            Some("A new VM is being made: wait for it to finish first.")
        } else if !self.vm_deleting.is_empty() {
            Some("A VM is being deleted: wait for it to finish first.")
        } else {
            None
        }
    }

    /// Turns a server on in the background: a Wake-on-LAN signal from this computer (when its
    /// card's address is known) and through the pool (when the pool answers and it isn't the
    /// coordinator). Then, under Activities, it's waited for till it answers (`check_waking`).
    fn power_on(&mut self, sid: u32, host: Option<String>, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let key = host.clone().unwrap_or_else(|| format!("srv:{}", sid));
        if self.host_waking.contains_key(&key) {
            return;
        }
        let connected = s.state == ConnState::Connected;
        let (name, target, through_pool) = match &host {
            Some(r) => {
                let Some(h) = s.data.as_ref().and_then(|d| d.host(r)) else { return };
                // (a coordinator whose card isn't in the last look: its address kept from before)
                let kept = s.wake.as_ref().filter(|w| h.is_master && w.0 == h.name).map(|w| w.1.clone());
                (h.name.clone(), h.wake_target().or(kept), connected && !h.is_master)
            }
            None => match &s.wake {
                Some((n, w)) => (n.clone(), Some(w.clone()), false),
                None => return,
            },
        };
        // (the demo's servers aren't on this network: nothing is sent from here)
        let target = target.filter(|_| !s.conn.is_demo());
        let (conn, server) = (s.conn.clone(), s.name());
        let log = self.add_log(sid, &server, None, &name, &format!("Turning on server '{}'", name), "In progress");
        self.log[log].activity = true;
        let prog = self.progress(log);
        self.host_waking.insert(key.clone(), Waking { sid, host: host.clone(), name: name.clone(), since: Instant::now(), log });
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let here = target.map(|t| crate::conn::send_wake(&t));
            let pool = match (&host, through_pool) {
                (Some(r), true) => Some(conn.power_on_host(r)),
                _ => None,
            };
            let _ = tx.send(Msg::PoweredOn { sid, key, name, log, here, pool });
            ctx.request_repaint();
        });
    }

    /// Servers told to turn on: done once they answer, failed when they haven't in `WAKE_LIMIT`.
    fn check_waking(&mut self) {
        let keys: Vec<String> = self.host_waking.keys().cloned().collect();
        for key in keys {
            let w = &self.host_waking[&key];
            let up = self.server(w.sid).map_or(false, |s| {
                s.state == ConnState::Connected
                    && match &w.host {
                        Some(r) => s.data.as_ref().and_then(|d| d.host(r)).map_or(false, |h| h.is_up()),
                        None => true,
                    }
            });
            if !up && w.since.elapsed() < WAKE_LIMIT {
                continue;
            }
            let Some(w) = self.host_waking.remove(&key) else { continue };
            if up {
                self.end_log(w.log, "Done", Some(format!("Turned on server '{}'", w.name)));
            } else {
                let text = format!("'{}' didn't answer within {} minutes of being told to turn on: Wake-on-LAN may be off in its firmware (BIOS or UEFI) or for its network card, or it has no power", w.name, WAKE_LIMIT.as_secs() / 60);
                self.end_log(w.log, "Failed", Some(text));
            }
        }
    }

    /// Puts a server in maintenance mode (`exit` None), or takes it out (Some: whether its VMs come
    /// back), in the background.
    fn maintenance(&mut self, sid: u32, host: String, name: String, exit: Option<bool>, ctx: &egui::Context) {
        let Some((conn, server)) = self.server(sid).map(|s| (s.conn.clone(), s.name())) else { return };
        if self.host_maint.contains_key(&host) {
            return;
        }
        let what = match exit {
            None => format!("Putting server '{}' in maintenance mode", name),
            Some(_) => format!("Taking server '{}' out of maintenance mode", name),
        };
        let log = self.add_log(sid, &server, None, &name, &what, "In progress");
        self.log[log].activity = true;
        if exit.is_none() {
            self.if_closed(log, format!("'{}' may be left not taking VMs with only some of its VMs moved off.", name));
        }
        let prog = self.progress(log);
        self.host_maint.insert(host.clone(), exit.is_none());
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let msg = match exit {
                None => Msg::MaintenanceIn { result: conn.enter_maintenance(&host, true), sid, host, name, log },
                Some(back) => Msg::MaintenanceOut { result: conn.exit_maintenance(&host, back), sid, host, name, log },
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Shuts a server down, or (`reboot`) restarts it, in the background (its VMs moved or
    /// stopped first).
    fn shutdown_host(&mut self, sid: u32, host: String, name: String, reboot: bool, ctx: &egui::Context) {
        let Some((conn, server)) = self.server(sid).map(|s| (s.conn.clone(), s.name())) else { return };
        let what = format!("{} server '{}'", if reboot { "Restarting" } else { "Shutting down" }, name);
        let log = self.add_log(sid, &server, None, &name, &what, "In progress");
        self.if_closed(log, format!("'{}' may not be {}, and be left not taking VMs with only some of its VMs moved off.", name, if reboot { "restarted" } else { "shut down" }));
        let prog = self.progress(log);
        self.host_busy.insert(host.clone());
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = if reboot { conn.reboot_host(&host) } else { conn.shutdown_host(&host) };
            let _ = tx.send(Msg::HostShutdown { sid, host, name, reboot, log, result });
            ctx.request_repaint();
        });
    }

    /// Moves a VM as `m` says, in the background: its progress in Activities, "Moving…" on it.
    fn move_vm(&mut self, sid: u32, m: MoveVm, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let Some(d) = s.data.as_ref() else { return };
        let Some(v) = d.vm(&m.vm) else { return };
        let name = v.name.clone();
        let to = d.host(&m.host).map(|h| h.name.clone()).unwrap_or_default();
        let here = v.home().map_or(true, |h| h == m.host);
        let disks = m.disks.len();
        let text = match (here, disks) {
            (true, 1) => format!("Moving the disk of VM '{}' to other storage", name),
            (true, _) => format!("Moving the disks of VM '{}' to other storage", name),
            (false, 0) => format!("Moving VM '{}' to {}", name, to),
            (false, _) => format!("Moving VM '{}' and its disks to {}", name, to),
        };
        let (conn, tx, ctx, server, vm) = (s.conn.clone(), self.tx.clone(), ctx.clone(), s.name(), m.vm.clone());
        let log = self.add_log(sid, &server, Some(&vm), &name, &text, "In progress");
        if !m.running && disks > 0 {
            // a shut-down VM's disks are copied one by one and swapped in by the app
            self.if_closed(log, format!("The disk being copied is left half copied on the new storage (delete it there); '{}' keeps working from where its disks are.", name));
        }
        let prog = self.progress(log);
        if let Some(s) = self.server_mut(sid) {
            s.busy.insert(vm.clone(), ("Moving…".into(), "move"));
        }
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = conn.move_vm(&m);
            let _ = tx.send(Msg::Moved { sid, vm, name, log, result });
            ctx.request_repaint();
        });
    }

    /// A log entry ends: how it went, and (when given) what it says now.
    fn end_log(&mut self, log: usize, status: &str, text: Option<String>) {
        if let Some(e) = self.log.get_mut(log) {
            e.status = status.into();
            if let Some(t) = text {
                e.text = t;
            }
        }
    }

    /// A server's pool is looked at again right away.
    fn refresh_soon(&mut self, sid: u32) {
        if let Some(s) = self.server_mut(sid) {
            s.next_refresh = Instant::now();
        }
    }

    /// Each pool with an update channel has its updates read (its /updates, which asks the
    /// server only, not the channel) every fifteen minutes, and again after a fetch or an update.
    fn poll_pool_updates(&mut self, ctx: &egui::Context) {
        let due: Vec<(u32, Arc<dyn Connection>)> = self
            .servers
            .iter()
            .filter(|s| s.data.as_ref().map_or(false, |d| d.update_channel.is_some() && d.last_update_sync.is_some()))
            .filter(|s| self.pool_updates.get(&s.id).map_or(true, |(t, _)| t.elapsed() >= Duration::from_secs(900)))
            .filter(|s| !self.pool_updates_busy.contains_key(&s.id) && !self.pool_syncing.contains(&s.id))
            .map(|s| (s.id, s.conn.clone()))
            .collect();
        for (sid, conn) in due {
            self.pool_updates_busy.insert(sid, Instant::now());
            let (tx, ctx) = (self.tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let result = conn.pool_updates();
                let _ = tx.send(Msg::PoolUpdatesRead { sid, result });
                ctx.request_repaint();
            });
        }
    }

    /// A pool's updates in words, and how they stand (None when it has no channel to speak of).
    fn pool_update_status(&self, sid: u32) -> Option<(String, Standing)> {
        let d = self.server(sid)?.data.as_ref()?;
        if self.pool_runs.get(&sid).map_or(false, |r| r.ended().is_none()) {
            return Some(("Updating now…".into(), Standing::Unknown));
        }
        let found = self.pool_updates.get(&sid).and_then(|(_, r)| r.as_ref().ok());
        d.update_status(found, crate::pool_update::now())
    }

    /// What the Updates dialog shows of a pool.
    fn pool_view(&self, sid: u32) -> PoolView {
        let s = self.server(sid);
        let d = s.and_then(|s| s.data.as_ref());
        let mut hosts: Vec<(String, String, bool, bool)> = vec![];
        if let Some(d) = d {
            let mut hs: Vec<&HostInfo> = d.hosts.iter().collect();
            hs.sort_by_key(|h| (!h.is_master, h.name.to_lowercase()));
            hosts = hs.iter().map(|h| (h.rref.clone(), h.name.clone(), h.is_up(), h.pending_guidance.iter().any(|g| g.starts_with("reboot_host")))).collect();
        }
        PoolView {
            pools: self.servers.iter().filter(|s| s.data.is_some()).map(|s| (s.id, s.name())).collect(),
            name: s.map(|s| s.name()).unwrap_or_default(),
            channel: d.and_then(|d| d.update_channel.clone()),
            bundle: d.map_or(false, |d| d.update_bundle),
            xcp: d.map_or(false, |d| d.hosts.iter().any(|h| h.brand.to_lowercase().contains("xcp"))),
            status: self.pool_update_status(sid),
            found: self.pool_updates.get(&sid).map(|(_, r)| r.clone()),
            never_checked: d.map_or(false, |d| d.last_update_sync.is_none()),
            reading: self.pool_updates_busy.get(&sid).map(|t| t.elapsed()),
            vm_names: d.map(|d| d.vms.iter().map(|v| (v.rref.clone(), v.name.clone())).collect()).unwrap_or_default(),
            vms: d
                .map(|d| {
                    d.vms
                        .iter()
                        .filter(|v| v.power == PowerState::Running)
                        .map(|v| PlanVm { rref: v.rref.clone(), mem: if v.mem_actual > 0 { v.mem_actual } else { v.mem_dyn_max }, disks: v.disks.iter().filter(|k| k.kind == "Disk" && !k.vdi.is_empty()).map(|k| (k.device.clone(), k.size, k.sr.clone())).collect() })
                        .collect()
                })
                .unwrap_or_default(),
            srs: d
                .map(|d| {
                    d.srs
                        .iter()
                        .filter(|x| !x.is_iso() && x.kind != "udev" && x.size > 0)
                        .map(|x| PlanSr { rref: x.rref.clone(), free: (x.size - x.used).max(0), seen_by: match &x.host { Some(h) => vec![h.clone()], None => x.links.iter().filter(|l| l.1).map(|l| l.0.clone()).collect() }, live_out: x.live_out, live_in: x.live_in })
                        .collect()
                })
                .unwrap_or_default(),
            room: d.map(|d| d.hosts.iter().filter(|h| h.is_up() && h.enabled).map(|h| (h.rref.clone(), h.mem_free)).collect()).unwrap_or_default(),
            stuck: d
                .map(|d| {
                    d.vms
                        .iter()
                        .filter(|v| matches!(v.power, PowerState::Running | PowerState::Paused) && v.host.is_some() && v.host == v.storage_host)
                        .filter_map(|v| d.host(v.host.as_deref()?).map(|h| (h.rref.clone(), h.name.clone(), v.name.clone())))
                        .collect()
                })
                .unwrap_or_default(),
            hosts,
            checking: self.pool_syncing.contains(&sid),
            check_error: self.pool_sync_error.get(&sid).cloned(),
            run: self.pool_runs.get(&sid).cloned(),
        }
    }

    /// Fetches a pool's latest updates from its channel, in the background.
    fn sync_pool(&mut self, sid: u32, ctx: &egui::Context) {
        let Some((conn, server)) = self.server(sid).map(|s| (s.conn.clone(), s.name())) else { return };
        if !self.pool_syncing.insert(sid) {
            return;
        }
        self.pool_sync_error.remove(&sid);
        let log = self.add_log(sid, &server, None, &server, &format!("Fetching {}'s updates from its channel", server), "In progress");
        let prog = self.progress(log);
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = conn.sync_updates();
            let _ = tx.send(Msg::PoolSynced { sid, log, result });
            ctx.request_repaint();
        });
    }

    /// Updates a pool as `plan` says, in the background: its steps in the Updates dialog, the
    /// calls in Activities.
    fn update_pool(&mut self, sid: u32, plan: crate::pool_update::Plan, ctx: &egui::Context) {
        let Some((conn, server)) = self.server(sid).map(|s| (s.conn.clone(), s.name())) else { return };
        if self.pool_runs.get(&sid).map_or(false, |r| r.ended().is_none()) {
            return;
        }
        let log = self.add_log(sid, &server, None, &server, &format!("Updating {}", server), "In progress");
        self.if_closed(
            log,
            "It stops after the step under way: the servers after it aren't updated, one may be left not taking VMs, and VMs shut down for a restart aren't started again.".into(),
        );
        let prog = self.progress(log);
        let run = Arc::new(UpdateRun::new(prog.trace.clone()));
        self.pool_runs.insert(sid, run.clone());
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let say = tx.clone();
            let restarting = move |host: &str, back: bool| {
                let _ = say.send(Msg::UpdateRestarting { host: host.to_string(), back });
            };
            let result = crate::pool_update::run(conn.as_ref(), &plan, &run, &restarting);
            let _ = tx.send(Msg::PoolUpdated { sid, log, result });
            ctx.request_repaint();
        });
    }

    /// Deletes a VM (and, with `disks`, its disks) in the background.
    fn delete_vm(&mut self, sid: u32, vm: String, name: String, disks: bool, ctx: &egui::Context) {
        let Some((conn, server)) = self.server(sid).map(|s| (s.conn.clone(), s.name())) else { return };
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        self.vm_deleting.insert(vm.clone());
        let prog = self.job(format!("vmdel:{}", vm), sid, &server, Some(&vm), &name, &format!("Deleting VM '{}'", name));
        if let Some(&log) = self.jobs.get(&format!("vmdel:{}", vm)) {
            self.if_closed(log, format!("'{}' may be left shut down, with only some of its disks deleted.", name));
        }
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = conn.delete_vm(&vm, disks);
            let _ = tx.send(Msg::VmDeleted { sid, vm, name, result });
            ctx.request_repaint();
        });
    }

    /// Takes a snapshot of a VM in the background, its icon turning meanwhile.
    fn take_snapshot(&mut self, sid: u32, vm: String, name: String, description: String, memory: bool, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let Some(v) = s.data.as_ref().and_then(|d| d.vm(&vm)) else { return };
        let what = if memory { "with its memory" } else { "of its disks" };
        let text = format!("Taking snapshot '{}' of VM '{}' ({})", name, v.name, what);
        let (conn, tx, ctx, server, vm_name) = (s.conn.clone(), self.tx.clone(), ctx.clone(), s.name(), v.name.clone());
        let log = self.add_log(sid, &server, Some(&vm), &vm_name, &text, "In progress");
        let prog = self.progress(log);
        if let Some(s) = self.server_mut(sid) {
            s.busy.insert(vm.clone(), ("Taking a snapshot…".into(), "snapshot"));
        }
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = conn.snapshot_vm(&vm, &name, &description, memory).map(|_| vec![]);
            let _ = tx.send(Msg::SnapDone { sid, vm, snap: None, log, title: "Take Snapshot", text, result });
            ctx.request_repaint();
        });
    }

    /// Reverts a VM to a snapshot in the background (with `first`, after taking a snapshot of
    /// now), its icon turning meanwhile.
    fn revert(&mut self, sid: u32, snap: String, first: bool, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let Some((v, x)) = s.data.as_ref().and_then(|d| d.vms.iter().find_map(|v| v.snapshot(&snap).map(|x| (v, x)))) else { return };
        let (vm, vm_name, snap_name) = (v.rref.clone(), v.name.clone(), x.name.clone());
        let text = format!("Reverting VM '{}' to snapshot '{}'", vm_name, snap_name);
        let (conn, tx, ctx, server) = (s.conn.clone(), self.tx.clone(), ctx.clone(), s.name());
        let log = self.add_log(sid, &server, Some(&vm), &vm_name, &text, "In progress");
        self.if_closed(log, format!("'{}' may be left reverted but shut down: start it yourself.", vm_name));
        let prog = self.progress(log);
        if let Some(s) = self.server_mut(sid) {
            s.busy.insert(vm.clone(), ("Reverting…".into(), "revert"));
        }
        let now_name = format!("Before reverting to {}", snap_name);
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = (|| {
                if first {
                    conn.snapshot_vm(&vm, &now_name, "", false)?;
                }
                conn.revert_vm(&snap)
            })()
            .map(|_| vec![]);
            let _ = tx.send(Msg::SnapDone { sid, vm, snap: Some(snap), log, title: "Revert", text, result });
            ctx.request_repaint();
        });
    }

    /// Deletes a snapshot in the background, its row saying so meanwhile.
    fn delete_snapshot(&mut self, sid: u32, snap: String, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let Some((v, x)) = s.data.as_ref().and_then(|d| d.vms.iter().find_map(|v| v.snapshot(&snap).map(|x| (v, x)))) else { return };
        let (vm, vm_name) = (v.rref.clone(), v.name.clone());
        let text = format!("Deleting snapshot '{}' of VM '{}'", x.name, vm_name);
        let (conn, tx, ctx, server) = (s.conn.clone(), self.tx.clone(), ctx.clone(), s.name());
        let log = self.add_log(sid, &server, Some(&vm), &vm_name, &text, "In progress");
        let prog = self.progress(log);
        self.snap_deleting.insert(snap.clone());
        if self.snap_edit.as_ref().map_or(false, |e| e.vdi == snap) {
            self.snap_edit = None;
        }
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = conn.delete_snapshot(&snap);
            let _ = tx.send(Msg::SnapDone { sid, vm, snap: Some(snap), log, title: "Delete Snapshot", text, result });
            ctx.request_repaint();
        });
    }

    /// Makes a template from a VM that's off, or one of its snapshots, in the background, the VM's
    /// icon turning meanwhile.
    fn make_template(&mut self, sid: u32, vm: String, source: String, name: String, description: String, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let Some(v) = s.data.as_ref().and_then(|d| d.vm(&vm)) else { return };
        let text = format!("Making template '{}' from VM '{}'", name, v.name);
        let (conn, tx, ctx, server, vm_name) = (s.conn.clone(), self.tx.clone(), ctx.clone(), s.name(), v.name.clone());
        let log = self.add_log(sid, &server, Some(&vm), &vm_name, &text, "In progress");
        let prog = self.progress(log);
        self.remember_unfinished(sid, &name);
        if let Some(s) = self.server_mut(sid) {
            s.busy.insert(vm.clone(), ("Making a template…".into(), "copy"));
        }
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = conn.make_template(&source, &name, &description);
            let _ = tx.send(Msg::Made { sid, vm, name, log, title: "Make Template", text, template: true, result });
            ctx.request_repaint();
        });
    }

    /// Copies a VM that's off in the background, its icon turning meanwhile.
    fn copy_vm(&mut self, sid: u32, vm: String, name: String, sr: Option<String>, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let Some(v) = s.data.as_ref().and_then(|d| d.vm(&vm)) else { return };
        let onto = sr.as_deref().and_then(|r| s.data.as_ref().and_then(|d| d.sr(r))).map(|x| format!(" onto {}", x.name)).unwrap_or_default();
        let text = format!("Copying VM '{}' as '{}'{}", v.name, name, onto);
        let (conn, tx, ctx, server, vm_name) = (s.conn.clone(), self.tx.clone(), ctx.clone(), s.name(), v.name.clone());
        let log = self.add_log(sid, &server, Some(&vm), &vm_name, &text, "In progress");
        let prog = self.progress(log);
        self.remember_unfinished(sid, &name);
        if let Some(s) = self.server_mut(sid) {
            s.busy.insert(vm.clone(), ("Copying…".into(), "copy"));
        }
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = conn.copy_vm(&vm, &name, sr.as_deref());
            let _ = tx.send(Msg::Made { sid, vm, name, log, title: "Copy VM", text, template: false, result });
            ctx.request_repaint();
        });
    }

    /// Deletes a template of the user's and its disks in the background.
    fn delete_template(&mut self, sid: u32, tpl: String, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let Some(t) = s.data.as_ref().and_then(|d| d.user_template(&tpl)) else { return };
        let name = t.name.clone();
        let (conn, tx, ctx, server) = (s.conn.clone(), self.tx.clone(), ctx.clone(), s.name());
        let log = self.add_log(sid, &server, Some(&tpl), &name, &format!("Deleting template '{}'", name), "In progress");
        let prog = self.progress(log);
        if let Some(s) = self.server_mut(sid) {
            s.busy.insert(tpl.clone(), ("Deleting…".into(), "delete"));
        }
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = conn.delete_vm(&tpl, true);
            let _ = tx.send(Msg::TemplateDeleted { sid, tpl, name, log, result });
            ctx.request_repaint();
        });
    }

    /// Makes a change to a VM in the background (one at a time for each VM).
    fn edit_vm(&mut self, sid: u32, vm: String, change: VmChange, ctx: &egui::Context) {
        if self.vm_saving.contains_key(&vm) {
            self.edit_queue.push((sid, vm, change));
            return;
        }
        let Some(s) = self.server(sid) else { return };
        let Some(v) = s.data.as_ref().and_then(|d| d.vm(&vm)) else { return };
        let text = change.text(&v.name);
        let (conn, tx, ctx, server, vm_name) = (s.conn.clone(), self.tx.clone(), ctx.clone(), s.name(), v.name.clone());
        let log = self.add_log(sid, &server, Some(&vm), &vm_name, &text, "In progress");
        let prog = self.progress(log);
        self.vm_saving.insert(vm.clone(), text.clone());
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = conn.edit_vm(&vm, &change);
            let _ = tx.send(Msg::Edited { sid, vm, log, text, result });
            ctx.request_repaint();
        });
    }

    fn save_snapshot_description(&mut self, sid: u32, snap: String, text: String, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let Some((v, x)) = s.data.as_ref().and_then(|d| d.vms.iter().find_map(|v| v.snapshot(&snap).map(|x| (v, x)))) else { return };
        let (vm, name) = (v.rref.clone(), x.name.clone());
        let (conn, tx, ctx) = (s.conn.clone(), self.tx.clone(), ctx.clone());
        self.snap_saving.insert(snap.clone(), text.clone());
        std::thread::spawn(move || {
            let result = conn.set_vm_description(&snap, &text);
            let _ = tx.send(Msg::SnapDescribed { sid, vm, snap, name, text, result });
            ctx.request_repaint();
        });
    }

    /// Deletes an ISO file in the background.
    fn delete_iso(&mut self, sid: u32, sr: String, vdi: String, name: String, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let (conn, tx, ctx, server) = (s.conn.clone(), self.tx.clone(), ctx.clone(), s.name());
        let lib = s.data.as_ref().and_then(|d| d.sr(&sr)).map(|x| x.name.clone()).unwrap_or_default();
        self.iso_deleting.insert(vdi.clone());
        if self.iso_edit.as_ref().map_or(false, |e| e.vdi == vdi) {
            self.iso_edit = None;
        }
        let prog = self.job(format!("isodel:{}", vdi), sid, &server, Some(&sr), &lib, &format!("Deleting '{}' from '{}'", name, lib));
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
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

    /// Makes storage `sr` the pool's default in the background (`name`: its name, when the pool
    /// hasn't been read since it was made).
    fn make_default(&mut self, sid: u32, sr: String, name: Option<String>, ctx: &egui::Context) {
        let Some((conn, server, name)) = self.server(sid).and_then(|s| Some((s.conn.clone(), s.name(), name.or_else(|| Some(s.data.as_ref()?.sr(&sr)?.name.clone()))?))) else { return };
        let log = self.add_log(sid, &server, Some(&sr), &name, &format!("Making '{}' the pool's default storage", name), "In progress");
        let prog = self.progress(log);
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = conn.make_default_sr(&sr);
            let _ = tx.send(Msg::DefaultSrSet { sid, log, name, result });
            ctx.request_repaint();
        });
    }

    /// Asks the pool to create NFS storage for VMs' disks (which mounts it everywhere) in the
    /// background.
    fn start_storage(&mut self, sid: u32, st: NfsStorage, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let (conn, server) = (s.conn.clone(), s.name());
        let log = self.add_log(sid, &server, None, &st.name, &format!("Creating storage '{}' from {}", st.name, st.location), "In progress");
        let prog = self.progress(log);
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = conn.create_nfs_storage(&st);
            let _ = tx.send(Msg::StorageDone { sid, st, log, result });
            ctx.request_repaint();
        });
    }

    /// Each pool's network cards a VLAN can go on (its coordinator's: physical cards and bonds,
    /// with the network on each), and the VLANs it has already.
    fn network_cards(&self) -> Vec<PoolCards> {
        self.servers
            .iter()
            .filter_map(|s| {
                let d = s.data.as_ref()?;
                let coordinator = d.hosts.iter().find(|h| h.is_master).or(d.hosts.first())?;
                let cards = coordinator.nics.iter().map(|n| (n.device.clone(), n.network.clone())).collect();
                let taken = d.networks.iter().filter(|n| n.vlan >= 0).map(|n| (n.device.clone(), n.vlan, n.name.clone())).collect();
                Some(PoolCards { sid: s.id, cards, taken })
            })
            .collect()
    }

    /// Deletes network `net` of pool `sid` in the background.
    fn delete_network(&mut self, sid: u32, net: String, name: String, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let (conn, server) = (s.conn.clone(), s.name());
        let vlan = s.data.as_ref().and_then(|d| d.net(&net)).map_or(false, |n| n.vlan >= 0);
        let log = self.add_log(sid, &server, None, &name, &format!("Deleting network '{}'", name), "In progress");
        if vlan {
            self.if_closed(log, format!("'{}' may be left without its VLAN on some servers: delete it again.", name));
        }
        let prog = self.progress(log);
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = conn.delete_network(&net);
            let _ = tx.send(Msg::NetworkDeleted { sid, name, log, result });
            ctx.request_repaint();
        });
    }

    /// Asks pool `sid` to make network `nw` in the background.
    fn start_network(&mut self, sid: u32, nw: NewNetwork, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let (conn, server) = (s.conn.clone(), s.name());
        let how = match &nw.vlan {
            Some((card, tag)) => format!(" (VLAN {} on {})", tag, card),
            None => " (private)".to_string(),
        };
        let log = self.add_log(sid, &server, None, &nw.name, &format!("Making network '{}'{}", nw.name, how), "In progress");
        if nw.vlan.is_some() {
            self.if_closed(log, format!("'{}' may be left made without its VLAN: delete it on the Networks page.", nw.name));
        }
        let prog = self.progress(log);
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
            let result = conn.create_network(&nw);
            let _ = tx.send(Msg::NetworkMade { sid, nw, log, result });
            ctx.request_repaint();
        });
    }

    /// Asks pool `sid` which folders NFS server `server` shares, in the background.
    fn browse_exports(&mut self, sid: u32, server: String, ctx: &egui::Context) {
        let Some(conn) = self.server(sid).map(|s| s.conn.clone()) else { return };
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = conn.nfs_exports(&server);
            let _ = tx.send(Msg::Exports { server, result });
            ctx.request_repaint();
        });
    }

    /// Asks the server to create the ISO library (which mounts the share) in the background.
    fn start_iso_library(&mut self, sid: u32, lib: IsoLibrary, ctx: &egui::Context) {
        let Some(s) = self.server(sid) else { return };
        let (conn, tx, ctx, server) = (s.conn.clone(), self.tx.clone(), ctx.clone(), s.name());
        let prog = self.job(format!("isolib:{}:{}", sid, lib.name), sid, &server, None, &lib.name, &format!("Creating ISO library '{}' from {}", lib.name, lib.location));
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
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
            started: Instant::now(),
            progress: (status == "In progress").then(Progress::new),
            activity: status == "In progress",
            if_closed: None,
        });
        self.log.len() - 1
    }

    /// What job `log` would leave half done if the app closed now (see `LogEntry::if_closed`).
    fn if_closed(&mut self, log: usize, text: String) {
        if let Some(e) = self.log.get_mut(log) {
            e.if_closed = Some(text);
        }
    }

    /// What closing the app now would leave half done: each job's line and what's left, the
    /// uploads too (their bytes come from here). Jobs that are one task on the server aren't
    /// here: the server finishes them by itself.
    fn halfway(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .log
            .iter()
            .filter(|e| e.status == "In progress")
            .filter_map(|e| e.if_closed.as_ref().map(|t| (format!("{}.", e.text), t.clone())))
            .collect();
        for u in self.uploads.values().filter(|u| u.active()) {
            out.push((format!("Uploading '{}' to '{}'.", u.name, u.lib), "The file is left unfinished in the library: delete it there.".into()));
        }
        out
    }

    /// Something worth seeing under Activities that wasn't a job (a server gone, or back).
    fn notice(&mut self, sid: u32, server: &str, obj_ref: Option<&str>, obj: &str, text: &str, status: &str) {
        let i = self.add_log(sid, server, obj_ref, obj, text, status);
        self.log[i].activity = true;
    }

    /// Notes that a VM or template named `name` is being made on server `sid` (what already has
    /// that name, too), in case contact is lost meanwhile.
    fn remember_unfinished(&mut self, sid: u32, name: &str) {
        let Some(s) = self.server(sid).filter(|s| !s.conn.is_demo()) else { return };
        let (host, user) = (s.conn.hostname(), s.conn.username());
        let before = s.data.as_ref().map(|d| d.vms.iter().chain(&d.user_templates).filter(|v| v.name == name).map(|v| v.rref.clone()).collect()).unwrap_or_default();
        self.unfinished.retain(|u| !(u.host == host && u.name == name));
        self.unfinished.push(Unfinished { host, user, name: name.into(), before, lost: false });
        save_unfinished(&self.unfinished);
    }

    /// It was made, or what was made was taken away again: nothing to look for.
    fn forget_unfinished(&mut self, sid: u32, name: &str) {
        let Some(host) = self.server(sid).map(|s| s.conn.hostname()) else { return };
        let before = self.unfinished.len();
        self.unfinished.retain(|u| !(u.host == host && u.name == name));
        if self.unfinished.len() != before {
            save_unfinished(&self.unfinished);
        }
    }

    /// Its job lost contact: looked for once the server's back.
    fn lost_unfinished(&mut self, sid: u32, name: &str) {
        let Some(host) = self.server(sid).map(|s| s.conn.hostname()) else { return };
        for u in self.unfinished.iter_mut().filter(|u| u.host == host && u.name == name) {
            u.lost = true;
        }
    }

    /// Server `sid` answers again: anything left half made there under a lost job's name is
    /// offered for deleting (one at a time); a name with nothing new under it is let go.
    fn check_unfinished(&mut self, sid: u32) {
        let Some(s) = self.server(sid) else { return };
        let Some(d) = s.data.as_ref() else { return };
        let (host, user, server) = (s.conn.hostname(), s.conn.username(), s.name());
        let mut found = None;
        let mut gone = vec![];
        for (i, u) in self.unfinished.iter().enumerate().filter(|(_, u)| u.lost && u.host == host && u.user == user) {
            let left: Vec<String> = d.vms.iter().chain(&d.user_templates).filter(|v| v.name == u.name && !u.before.contains(&v.rref)).map(|v| v.rref.clone()).collect();
            if left.is_empty() {
                gone.push(i);
            } else if found.is_none() {
                found = Some((i, left));
            }
        }
        for &i in gone.iter().rev() {
            let u = self.unfinished.remove(i);
            self.add_log(sid, &server, None, &u.name, &format!("Nothing was left of '{}', which was being made when contact was lost", u.name), "Done");
        }
        if let Some((i, refs)) = found.filter(|_| matches!(self.dialog, Dialog::None)) {
            let i = i - gone.iter().filter(|&&g| g < i).count();
            let u = self.unfinished.remove(i);
            self.dialog = Dialog::ConfirmLeftover { sid, name: u.name, refs, server };
        }
        if !gone.is_empty() || matches!(self.dialog, Dialog::ConfirmLeftover { .. }) {
            save_unfinished(&self.unfinished);
        }
    }

    /// What the job logged as `log` reports its progress to (its thread calls `watch` with it).
    fn progress(&self, log: usize) -> Arc<Progress> {
        self.log.get(log).and_then(|e| e.progress.clone()).unwrap_or_else(Progress::new)
    }

    /// Starts a job known by `key` (one at a time per key): its log line, "In progress" and under
    /// Activities till `job_done`; what its thread reports progress to.
    fn job(&mut self, key: String, sid: u32, server: &str, obj_ref: Option<&str>, obj: &str, text: &str) -> Arc<Progress> {
        let log = self.add_log(sid, server, obj_ref, obj, text, "In progress");
        self.jobs.insert(key, log);
        self.progress(log)
    }

    /// Ends the job known by `key`: its line says how it went (a new line when there was none).
    fn job_done(&mut self, key: &str, sid: u32, server: &str, obj_ref: Option<&str>, obj: &str, text: &str, status: &str) {
        match self.jobs.remove(key).and_then(|i| self.log.get_mut(i)) {
            Some(e) => {
                e.text = text.into();
                e.status = status.into();
                if obj_ref.is_some() {
                    e.obj_ref = obj_ref.map(String::from);
                }
            }
            None => {
                self.add_log(sid, server, obj_ref, obj, text, status);
            }
        }
    }

    fn tick(&mut self, ctx: &egui::Context) {
        if !self.host_waking.is_empty() {
            self.check_waking();
            ctx.request_repaint_after(Duration::from_secs(1));
        }
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
                Msg::StorageDone { sid, st, log, result } => {
                    match result {
                        Ok(sr) => {
                            self.end_log(log, "Done", None);
                            if matches!(self.dialog, Dialog::NewStorage(_)) {
                                self.dialog = Dialog::None;
                            }
                            self.refresh_soon(sid);
                            self.expanded.insert(format!("folder:{}:srs", sid));
                            if st.make_default {
                                // (it's in the pool once the pool's read again: its name said here)
                                self.make_default(sid, sr, Some(st.name.clone()), ctx);
                            }
                        }
                        Err(e) => {
                            self.end_log(log, "Failed", Some(format!("Couldn't create storage '{}' from {}: {}", st.name, st.location, e.message())));
                            if let Dialog::NewStorage(d) = &mut self.dialog {
                                d.failed(&st, &e);
                            }
                        }
                    }
                }
                Msg::NetworkMade { sid, nw, log, result } => match result {
                    Ok(_) => {
                        self.end_log(log, "Done", None);
                        if matches!(self.dialog, Dialog::NewNetwork(_)) {
                            self.dialog = Dialog::None;
                        }
                        self.refresh_soon(sid);
                    }
                    Err(e) => {
                        self.end_log(log, "Failed", Some(format!("Couldn't make network '{}': {}", nw.name, e.message())));
                        if let Dialog::NewNetwork(d) = &mut self.dialog {
                            d.failed(&nw, &e);
                        }
                    }
                },
                Msg::NetworkDeleted { sid, name, log, result } => {
                    match result {
                        Ok(()) => self.end_log(log, "Done", None),
                        Err(e) => {
                            let why = match e.code.as_str() {
                                "NETWORK_CONTAINS_VIF" => "cards are still on it (a VM's, a template's or a snapshot's): move or remove them first".to_string(),
                                "NETWORK_CONTAINS_PIF" => "it's a network card's own network: it goes only with the card".to_string(),
                                _ => e.message(),
                            };
                            self.end_log(log, "Failed", Some(format!("Couldn't delete network '{}': {}", name, why)));
                            self.dialog = Dialog::Message { title: "Delete Network".into(), text: format!("Couldn't delete '{}': {}.", name, why) };
                        }
                    }
                    self.refresh_soon(sid);
                }
                Msg::Exports { server, result } => {
                    if let Dialog::NewStorage(d) = &mut self.dialog {
                        d.exports_found(&server, result);
                    }
                }
                Msg::IsoDone { sid, lib, result } => {
                    let server = self.server(sid).map(|s| s.name()).unwrap_or_default();
                    match result {
                        Ok(()) => {
                            self.job_done(&format!("isolib:{}:{}", sid, lib.name), sid, &server, None, &lib.name, &format!("Created ISO library '{}' from {}", lib.name, lib.location), "Done");
                            if matches!(self.dialog, Dialog::Iso(_)) {
                                self.dialog = Dialog::None;
                            }
                            if let Some(s) = self.server_mut(sid) {
                                s.next_refresh = Instant::now(); // show it right away
                            }
                            self.expanded.insert(format!("folder:{}:srs", sid));
                        }
                        Err(e) => {
                            self.job_done(&format!("isolib:{}:{}", sid, lib.name), sid, &server, None, &lib.name, &format!("Couldn't create ISO library '{}': {}", lib.name, e.message()), "Failed");
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
                            self.job_done(&format!("sr:{}", sr), sid, &server, Some(&sr), &name, &format!("{} '{}'", what, name), "Done");
                            if forget && self.selected == format!("sr:{}:{}", sid, sr) {
                                self.select(format!("srv:{}", sid));
                            }
                        }
                        Err(e) => {
                            let why = if e.code == "VDI_IN_USE" || e.details.join(" ").to_lowercase().contains("in use") {
                                "A VM still has a disc from this library in its DVD drive. Eject it first, then try again.".to_string()
                            } else if e.code == "SR_HAS_NO_PBDS" {
                                "It isn't mounted on the pool's coordinator, so the pool can't look at it. Repair mounts it again.".to_string()
                            } else {
                                e.message()
                            };
                            let title = if forget { "Disconnect ISO Library" } else { "Rescan" };
                            self.job_done(&format!("sr:{}", sr), sid, &server, Some(&sr), &name, &format!("Couldn't {} '{}': {}", if forget { "disconnect" } else { "rescan" }, name, why), "Failed");
                            self.dialog = Dialog::Message { title: title.into(), text: format!("Couldn't {} '{}'.\n\n{}", if forget { "disconnect" } else { "rescan" }, name, why) };
                        }
                    }
                    if let Some(s) = self.server_mut(sid) {
                        s.next_refresh = Instant::now();
                    }
                }
                Msg::SrRepaired { sid, sr, name, auto, result } => {
                    self.sr_busy.remove(&sr);
                    let server = self.server(sid).map(|s| s.name()).unwrap_or_default();
                    let nfs = self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.sr(&sr)).map_or(false, |x| x.share.starts_with("NFS"));
                    let tried = result.unwrap_or_else(|e| vec![(String::new(), Err(e))]);
                    let mounted: Vec<String> = tried.iter().filter(|t| t.1.is_ok()).map(|t| t.0.clone()).collect();
                    // done by itself: said first, as the line may be cut short
                    let found = if auto { format!("Found '{}' unmounted; ", name) } else { String::new() };
                    match tried.into_iter().find_map(|(h, r)| r.err().map(|e| (h, e))) {
                        None => {
                            let on = if mounted.is_empty() { String::new() } else { format!(" on {}", mounted.join(", ")) };
                            let text = if auto { format!("{}mounted it again{}", found, on) } else { format!("Mounted '{}' again{}", name, on) };
                            self.job_done(&format!("sr:{}", sr), sid, &server, Some(&sr), &name, &text, "Done");
                        }
                        Some((host, e)) => {
                            let place = if host.is_empty() { String::new() } else { format!(" on {}", host) };
                            let text = if auto { format!("{}couldn't mount it again{}: {}", found, place, e.message()) } else { format!("Couldn't mount '{}' again{}: {}", name, place, e.message()) };
                            self.job_done(&format!("sr:{}", sr), sid, &server, Some(&sr), &name, &text, "Failed");
                            if !auto {
                                let also = if mounted.is_empty() { String::new() } else { format!("\n\nIt's mounted on {} now.", mounted.join(", ")) };
                                self.dialog = Dialog::Message { title: "Repair".into(), text: format!("Couldn't mount '{}'{}.\n\n{}{}", name, place, mount_problem(&e, nfs), also) };
                            }
                        }
                    }
                    if let Some(s) = self.server_mut(sid) {
                        s.next_refresh = Instant::now();
                    }
                }
                Msg::Reattached { sid, sr, lib, result } => {
                    self.sr_busy.remove(&sr);
                    let server = self.server(sid).map(|s| s.name()).unwrap_or_default();
                    match result {
                        Ok(()) => {
                            self.job_done(&format!("sr:{}", sr), sid, &server, Some(&sr), &lib.name, &format!("Mounted '{}' again from {}", lib.name, lib.location), "Done");
                            if matches!(self.dialog, Dialog::Iso(_)) {
                                self.dialog = Dialog::None;
                            }
                        }
                        Err(e) => {
                            self.job_done(&format!("sr:{}", sr), sid, &server, Some(&sr), &lib.name, &format!("Couldn't mount '{}' from {}: {}", lib.name, lib.location, e.message()), "Failed");
                            if let Dialog::Iso(d) = &mut self.dialog {
                                d.failed(&lib, &e);
                            }
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
                            self.forget_unfinished(sid, &name);
                            self.job_done(&format!("newvm:{}:{}", sid, name), sid, &server, Some(&vm), &name, &format!("Created VM '{}'", name), "Done");
                            if start {
                                self.job(format!("start:{}", vm), sid, &server, Some(&vm), &name, &format!("Starting VM '{}'", name));
                                // the dialog waits for the start, but can be closed
                                if let Dialog::NewVm(d) = &mut self.dialog {
                                    d.starting(vm.clone());
                                }
                            } else if matches!(self.dialog, Dialog::NewVm(_)) {
                                self.dialog = Dialog::None;
                            }
                            // shown once the next refresh brings it
                            self.pending_select = Some((format!("vm:{}:{}", sid, vm), Tab::General));
                        }
                        Err(e) => {
                            // lost contact: something may be left half made, to offer deleting
                            // once the server is back; otherwise what was made was taken away
                            if matches!(e.code.as_str(), LOST_CONTACT | "CONNECTION") {
                                self.lost_unfinished(sid, &name);
                            } else {
                                self.forget_unfinished(sid, &name);
                            }
                            let why = create_problem(&e);
                            self.job_done(&format!("newvm:{}:{}", sid, name), sid, &server, None, &name, &format!("Couldn't create VM '{}': {}", name, why), "Failed");
                            match &mut self.dialog {
                                Dialog::NewVm(d) => d.failed(why),
                                // its dialog was closed: said here, unless another dialog is open
                                Dialog::None => self.dialog = Dialog::Message { title: "New VM".into(), text: format!("Couldn't create '{}'.\n\n{}", name, why) },
                                _ => {}
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
                            self.job_done(&format!("start:{}", vm), sid, &server, Some(&vm), &name, &format!("Started VM '{}'", name), "Done");
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
                            self.job_done(&format!("start:{}", vm), sid, &server, Some(&vm), &name, &format!("Couldn't start VM '{}': {}", name, why), "Failed");
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
                            self.job_done(&format!("isodel:{}", vdi), sid, &server, Some(&sr), &lib, &format!("Deleted '{}' from '{}'", name, lib), "Done");
                        }
                        Err(e) => {
                            let why = delete_problem(&e);
                            self.job_done(&format!("isodel:{}", vdi), sid, &server, Some(&sr), &lib, &format!("Couldn't delete '{}' from '{}': {}", name, lib, why), "Failed");
                            self.dialog = Dialog::Message { title: "Delete ISO File".into(), text: format!("Couldn't delete '{}'.\n\n{}", name, why) };
                        }
                    }
                    if let Some(s) = self.server_mut(sid) {
                        s.next_refresh = Instant::now();
                    }
                }
                Msg::HostShutdown { sid, host, name, reboot, log, result } => {
                    self.host_busy.remove(&host);
                    match result {
                        Ok(()) if reboot => {
                            if let Some(e) = self.log.get_mut(log) {
                                e.status = "Done".into();
                            }
                            // restarting: its going away and coming back are expected
                            self.host_rebooting.insert(host.clone(), crate::pool_update::now());
                        }
                        Ok(()) => {
                            if let Some(e) = self.log.get_mut(log) {
                                e.status = "Done".into();
                            }
                            // off now, before the next look says so
                            if let Some(h) = self.server_mut(sid).and_then(|s| s.data.as_mut()).and_then(|d| d.hosts.iter_mut().find(|h| h.rref == host)) {
                                (h.enabled, h.live) = (false, false);
                            }
                        }
                        Err(e) => {
                            let why = host_problem(&e);
                            if let Some(l) = self.log.get_mut(log) {
                                l.status = "Failed".into();
                                l.text = format!("{}: {}", l.text, why);
                            }
                            let (title, verb) = if reboot { ("Restart Server", "restart") } else { ("Shut Down Server", "shut down") };
                            self.dialog = Dialog::Message { title: title.into(), text: format!("Couldn't {} '{}'; it's taking new VMs again.\n\n{}", verb, name, why) };
                        }
                    }
                    if let Some(s) = self.server_mut(sid) {
                        s.next_refresh = Instant::now();
                    }
                }
                Msg::PoweredOn { sid, key, name, log, here, pool } => {
                    let sent = [&here, &pool].iter().any(|r| matches!(r, Some(Ok(()))));
                    // (one that answered meanwhile is done already)
                    let waiting = self.host_waking.get(&key).map_or(false, |w| w.log == log);
                    if sent && waiting {
                        self.end_log(log, "In progress", Some(format!("Turning on server '{}': waiting for it to answer", name)));
                    } else if !sent && waiting {
                        self.host_waking.remove(&key);
                        let mut why = vec![];
                        if let Some(Err(e)) = &pool {
                            why.push(format!("The pool couldn't send it the signal: {}", host_problem(e)));
                        }
                        if let Some(Err(e)) = &here {
                            why.push(format!("This computer couldn't send it either: {}", e.message()));
                        }
                        if why.is_empty() {
                            why.push("Its network card's address isn't known here, and the pool can't be asked while it doesn't answer.".into());
                        }
                        self.end_log(log, "Failed", Some(format!("Couldn't turn on '{}'", name)));
                        self.dialog = Dialog::Message { title: "Power On".into(), text: format!("Couldn't turn on '{}'.\n\n{}", name, why.join("\n\n")) };
                    }
                    self.refresh_soon(sid);
                }
                Msg::MaintenanceIn { sid, host, name, log, result } => {
                    self.host_maint.remove(&host);
                    match result {
                        Ok((stopped, moved)) => {
                            let mut parts = vec![];
                            if moved > 0 {
                                parts.push(format!("{} VM{} moved off", moved, if moved == 1 { "" } else { "s" }));
                            }
                            if !stopped.is_empty() {
                                parts.push(format!("{} shut down", stopped.len()));
                            }
                            let more = if parts.is_empty() { String::new() } else { format!(" ({})", parts.join(", ")) };
                            self.end_log(log, "Done", Some(format!("Server '{}' is in maintenance mode{}", name, more)));
                            // so till the next look says it
                            if let Some(h) = self.server_mut(sid).and_then(|s| s.data.as_mut()).and_then(|d| d.hosts.iter_mut().find(|h| h.rref == host)) {
                                (h.enabled, h.maintenance) = (false, true);
                            }
                        }
                        Err(e) => {
                            let why = host_problem(&e);
                            self.end_log(log, "Failed", Some(format!("Couldn't put '{}' in maintenance mode: {}", name, why)));
                            self.dialog = Dialog::Message { title: "Maintenance Mode".into(), text: format!("Couldn't put '{}' in maintenance mode.\n\n{}", name, why) };
                        }
                    }
                    self.refresh_soon(sid);
                }
                Msg::MaintenanceOut { sid, host, name, log, result } => {
                    self.host_maint.remove(&host);
                    match result {
                        Ok(failed) => {
                            if failed.is_empty() {
                                self.end_log(log, "Done", Some(format!("Server '{}' is out of maintenance mode", name)));
                            } else {
                                self.end_log(log, "Done", Some(format!("Server '{}' is out of maintenance mode; not all its VMs came back", name)));
                                self.dialog = Dialog::Message { title: "Exit Maintenance Mode".into(), text: format!("'{}' takes VMs again, but these didn't come back:\n\n{}", name, failed.join("\n")) };
                            }
                            if let Some(h) = self.server_mut(sid).and_then(|s| s.data.as_mut()).and_then(|d| d.hosts.iter_mut().find(|h| h.rref == host)) {
                                (h.enabled, h.maintenance) = (true, false);
                            }
                        }
                        Err(e) => {
                            let why = host_problem(&e);
                            self.end_log(log, "Failed", Some(format!("Couldn't take '{}' out of maintenance mode: {}", name, why)));
                            self.dialog = Dialog::Message { title: "Exit Maintenance Mode".into(), text: format!("Couldn't take '{}' out of maintenance mode.\n\n{}", name, why) };
                        }
                    }
                    self.refresh_soon(sid);
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
                                self.job_done(&format!("vmdel:{}", vm), sid, &server, None, &name, &format!("Deleted VM '{}'", name), "Done");
                            } else {
                                let text = format!("Deleted VM '{}', but not all its disks: {}", name, left.join("; "));
                                self.job_done(&format!("vmdel:{}", vm), sid, &server, None, &name, &text, "Done");
                                self.dialog = Dialog::Message { title: "Delete VM".into(), text: format!("'{}' was deleted, but these disks couldn't be:\n\n{}", name, left.join("\n")) };
                            }
                        }
                        Err(e) => {
                            let why = delete_vm_problem(&e);
                            self.job_done(&format!("vmdel:{}", vm), sid, &server, Some(&vm), &name, &format!("Couldn't delete VM '{}': {}", name, why), "Failed");
                            self.dialog = Dialog::Message { title: "Delete VM".into(), text: format!("Couldn't delete '{}'.\n\n{}", name, why) };
                        }
                    }
                    if let Some(s) = self.server_mut(sid) {
                        s.next_refresh = Instant::now();
                    }
                }
                Msg::UpdateChecked { asked, result } => {
                    self.update_checking = false;
                    let current = env!("CARGO_PKG_VERSION");
                    match result {
                        Ok(r) if update::newer(&r.version, current) => {
                            self.update_note = asked.then(|| format!("Version {} is out.", r.version));
                            self.pending_update = Some(r);
                            if asked {
                                // asked for in Settings: shown now, in its place
                                self.dialog = Dialog::None;
                            }
                        }
                        Ok(_) => self.update_note = asked.then(|| format!("You have the latest version ({}).", current)),
                        // a look as the app starts that fails says nothing (offline, say)
                        Err(e) => self.update_note = asked.then(|| format!("Couldn't look: {}", e)),
                    }
                }
                Msg::UpdateInstalled { result } => match result {
                    Ok(exe) => {
                        // the new version starts, this one closes
                        let args: Vec<String> = std::env::args().skip(1).collect();
                        match std::process::Command::new(&exe).args(args).spawn() {
                            Ok(_) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                            Err(e) => {
                                if let Dialog::Update(d) = &mut self.dialog {
                                    d.transfer = None;
                                    d.error = Some(format!("It's updated, but the new version didn't start ({}). Close the app and open it again.", e));
                                }
                            }
                        }
                    }
                    Err(e) => {
                        if let Dialog::Update(d) = &mut self.dialog {
                            d.transfer = None;
                            d.error = Some(e);
                        }
                    }
                },
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
                Msg::SnapDone { sid, vm, snap, log, title, text, result } => {
                    if let Some(s) = self.server_mut(sid) {
                        s.busy.remove(&vm);
                        s.next_refresh = Instant::now(); // re-read right away
                    }
                    if let Some(x) = &snap {
                        self.snap_deleting.remove(x);
                    }
                    let shot = self.shot_path.is_some();
                    match result {
                        Ok(left) => {
                            if let Some(e) = self.log.get_mut(log) {
                                e.status = "Done".into();
                            }
                            if !left.is_empty() && !shot {
                                let text = format!("The snapshot is gone, but some of its disks couldn't be deleted:\n\n{}", left.join("\n"));
                                self.dialog = Dialog::Message { title: title.into(), text };
                            }
                        }
                        Err(e) => {
                            let (status, why) = if e.code == NOT_RESTARTED {
                                // put back, but it didn't start again
                                ("Done", format!("put back, but couldn't start it again: {}", e.details.join(" ")))
                            } else {
                                ("Failed", snapshot_problem(&e))
                            };
                            if let Some(l) = self.log.get_mut(log) {
                                l.status = status.into();
                                l.text = format!("{}: {}", l.text, why);
                            }
                            if !shot {
                                let text = if e.code == NOT_RESTARTED {
                                    format!("{}: done, but it couldn't be started again.\n\n{}", text, e.details.join(" "))
                                } else {
                                    format!("{} failed.\n\n{}", text, why)
                                };
                                self.dialog = Dialog::Message { title: title.into(), text };
                            }
                        }
                    }
                }
                Msg::Made { sid, vm, name, log, title, text, template, result } => {
                    if let Some(s) = self.server_mut(sid) {
                        s.busy.remove(&vm);
                        s.next_refresh = Instant::now(); // re-read right away
                    }
                    match &result {
                        Err(e) if matches!(e.code.as_str(), LOST_CONTACT | "CONNECTION") => self.lost_unfinished(sid, &name),
                        _ => self.forget_unfinished(sid, &name),
                    }
                    match result {
                        Ok(made) => {
                            if let Some(e) = self.log.get_mut(log) {
                                e.status = "Done".into();
                            }
                            // the new one opened, once the server's been read again
                            // (a template: the Templates page, where it's listed)
                            if template {
                                self.expanded.insert("cat:tpls".into());
                                self.pending_select = Some(("root".into(), Tab::Templates));
                            } else {
                                self.pending_select = Some((format!("vm:{}:{}", sid, made), Tab::General));
                            }
                        }
                        Err(e) => {
                            let why = snapshot_problem(&e);
                            if let Some(l) = self.log.get_mut(log) {
                                l.status = "Failed".into();
                                l.text = format!("{}: {}", l.text, why);
                            }
                            if self.shot_path.is_none() {
                                self.dialog = Dialog::Message { title: title.into(), text: format!("{} failed.\n\n{}", text, why) };
                            }
                        }
                    }
                }
                Msg::TemplateDeleted { sid, tpl, name, log, result } => {
                    if let Some(s) = self.server_mut(sid) {
                        s.busy.remove(&tpl);
                        s.next_refresh = Instant::now();
                    }
                    match result {
                        Ok(left) => {
                            if let Some(d) = self.server_mut(sid).and_then(|s| s.data.as_mut()) {
                                d.user_templates.retain(|t| t.rref != tpl);
                            }
                            if self.selected == format!("tpl:{}:{}", sid, tpl) {
                                self.select(format!("srv:{}", sid));
                            }
                            if let Some(e) = self.log.get_mut(log) {
                                e.status = "Done".into();
                                if !left.is_empty() {
                                    e.text = format!("{}, but not all its disks: {}", e.text, left.join("; "));
                                }
                            }
                            if !left.is_empty() {
                                self.dialog = Dialog::Message { title: "Delete Template".into(), text: format!("'{}' was deleted, but these disks couldn't be:\n\n{}", name, left.join("\n")) };
                            }
                        }
                        Err(e) => {
                            let why = delete_vm_problem(&e);
                            if let Some(l) = self.log.get_mut(log) {
                                l.status = "Failed".into();
                                l.text = format!("{}: {}", l.text, why);
                            }
                            self.dialog = Dialog::Message { title: "Delete Template".into(), text: format!("Couldn't delete '{}'.\n\n{}", name, why) };
                        }
                    }
                }
                Msg::Edited { sid, vm, log, text, result } => {
                    self.vm_saving.remove(&vm);
                    // the next change for it, if one's waiting
                    if let Some(i) = self.edit_queue.iter().position(|q| q.1 == vm) {
                        let (qsid, qvm, change) = self.edit_queue.remove(i);
                        self.edit_vm(qsid, qvm, change, ctx);
                    }
                    if let Some(s) = self.server_mut(sid) {
                        s.next_refresh = Instant::now(); // re-read right away
                    }
                    match result {
                        Ok(()) => {
                            if let Some(e) = self.log.get_mut(log) {
                                e.status = "Done".into();
                            }
                            if self.ov.vm == vm {
                                self.ov.done();
                            }
                        }
                        Err(e) => {
                            let why = edit_problem(&e);
                            if let Some(l) = self.log.get_mut(log) {
                                l.status = "Failed".into();
                                l.text = format!("{}: {}", l.text, why);
                            }
                            if self.shot_path.is_none() {
                                self.dialog = Dialog::Message { title: "Change VM".into(), text: format!("{} failed.\n\n{}", text, why) };
                            }
                        }
                    }
                }
                Msg::Perf { vm, result } => {
                    self.perf_busy.remove(&vm);
                    self.perf.insert(vm, (Instant::now(), result.map_err(|e| e.message())));
                }
                Msg::HostPerf { host, result } => {
                    self.host_perf_busy.remove(&host);
                    self.host_perf.insert(host, (Instant::now(), result.map_err(|e| e.message())));
                }
                Msg::Messages { sid, since, result } => self.take_messages(sid, since, result),
                Msg::Dismissed { sid, refs, result } => self.dismissed(sid, refs, result),
                Msg::AlertSettingsRead { obj, result } => self.alert_rules_read(obj, result),
                Msg::AlertSettingsSaved { obj, log, result } => self.alert_rules_saved(obj, log, result),
                Msg::History { key, span, result } => {
                    self.history_busy.remove(&(key.clone(), span));
                    let result = result.map_err(|e| e.message());
                    // a failed read again keeps what was read before (said once that's old)
                    match (self.history.get_mut(&(key.clone(), span)), result) {
                        (Some(old), Err(_)) if old.1.is_ok() => old.0 = Instant::now(),
                        (_, result) => {
                            self.history.insert((key, span), (Instant::now(), result));
                        }
                    }
                }
                Msg::PoolUpdatesRead { sid, result } => {
                    self.pool_updates_busy.remove(&sid);
                    let result = result.map_err(|e| if e.code == "NO_REPOSITORY_ENABLED" { "its update channel hasn't been fetched from yet".to_string() } else { e.message() });
                    self.pool_updates.insert(sid, (Instant::now(), result));
                }
                Msg::PoolSynced { sid, log, result } => {
                    self.pool_syncing.remove(&sid);
                    self.pool_updates.remove(&sid); // read again
                    self.refresh_soon(sid);
                    let server = self.server(sid).map(|s| s.name()).unwrap_or_default();
                    match result {
                        Ok(_) => {
                            self.pool_sync_error.remove(&sid);
                            self.end_log(log, "Done", None);
                        }
                        Err(e) => {
                            self.pool_sync_error.insert(sid, e.message());
                            self.end_log(log, "Failed", Some(format!("Couldn't fetch {}'s updates: {}", server, e.message())));
                        }
                    }
                }
                Msg::PoolUpdated { sid, log, result } => {
                    self.pool_updates.remove(&sid);
                    self.refresh_soon(sid);
                    let server = self.server(sid).map(|s| s.name()).unwrap_or_default();
                    match &result {
                        Ok(said) => self.end_log(log, "Done", Some(format!("Updated {}: {}", server, said))),
                        Err(why) => self.end_log(log, "Failed", Some(format!("Couldn't update {}: {}", server, why))),
                    }
                    if let Some(run) = self.pool_runs.get(&sid) {
                        run.finish(result);
                    }
                }
                Msg::DefaultSrSet { sid, log, name, result } => {
                    match result {
                        Ok(()) => self.end_log(log, "Done", None),
                        Err(e) => self.end_log(log, "Failed", Some(format!("Couldn't make '{}' the pool's default storage: {}", name, e.message()))),
                    }
                    self.refresh_soon(sid);
                }
                Msg::Moved { sid, vm, name, log, result } => {
                    if let Some(s) = self.server_mut(sid) {
                        s.busy.remove(&vm);
                    }
                    match result {
                        Ok(()) => self.end_log(log, "Done", None),
                        Err(e) => {
                            let sr_name = |r: &str| self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.sr(r)).map_or("That storage".to_string(), |x| x.name.clone());
                            let why = match e.code.as_str() {
                                "SR_DOES_NOT_SUPPORT_MIGRATION" => {
                                    let sr = sr_name(e.details.first().map_or("", |x| x.as_str()));
                                    if e.details.iter().any(|d| d == "VDI_MIRROR_IN") {
                                        format!("{} can't take a disk while its VM runs (its kind of storage doesn't support live storage migration). Choose other storage, or shut the VM down to move its disks there.", sr)
                                    } else {
                                        format!("{} can't send a disk away while its VM runs (its kind of storage doesn't support live storage migration). Shut the VM down to move its disks.", sr)
                                    }
                                }
                                "STORAGE_GONE" => format!("{} is storage that no longer exists, and XenServer uses it when moving a VM's disks. Make another storage the pool's default (Make Default, on its page), then move it again.", e.details.first().cloned().unwrap_or_default()),
                                // a setting pointing to storage that's gone (the pool's default storage…)
                                "HANDLE_INVALID" if e.details.first().map_or(false, |c| c == "SR") => {
                                    let r = e.details.get(1).cloned().unwrap_or_default();
                                    let what = self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.gone_storage().into_iter().find(|g| g.1 == r)).map(|g| g.0);
                                    match what {
                                        Some(w) => format!("{} is storage that no longer exists, and XenServer uses it when moving a VM's disks (for its snapshots and saved memory). Set another in XenCenter (the pool's or the server's Properties), or with xe, then move it again.", w),
                                        None => "XenServer was pointed to storage that no longer exists (the pool's default storage, a server's storage for suspended VMs, or one of the VM's snapshots). Check those in XenCenter, then move it again.".to_string(),
                                    }
                                }
                                _ => e.message(),
                            };
                            self.end_log(log, "Failed", Some(format!("Couldn't move VM '{}': {}", name, why)));
                            if matches!(self.dialog, Dialog::None) {
                                self.dialog = Dialog::Message { title: "Move VM".into(), text: format!("Couldn't move '{}'; it stays where it was.\n\n{}", name, why) };
                            }
                        }
                    }
                    self.refresh_soon(sid);
                }
                Msg::HostEnabled { sid, host, name, log, result } => {
                    self.host_enabling.remove(&host);
                    match result {
                        Ok(()) => {
                            self.end_log(log, "Done", None);
                            if let Some(h) = self.server_mut(sid).and_then(|s| s.data.as_mut()).and_then(|d| d.hosts.iter_mut().find(|h| h.rref == host)) {
                                h.enabled = true;
                            }
                        }
                        Err(e) => self.end_log(log, "Failed", Some(format!("'{}' still isn't taking VMs: {}", name, host_problem(&e)))),
                    }
                    self.refresh_soon(sid);
                }
                Msg::UpdateRestarting { host, back } => {
                    if back {
                        self.host_rebooting.remove(&host);
                    } else {
                        self.host_rebooting.insert(host, crate::pool_update::now());
                    }
                }
                Msg::Thumb { vm, result } => {
                    self.thumb_busy.remove(&vm);
                    let t = self.thumbs.entry(vm.clone()).or_default();
                    t.at = Some(Instant::now());
                    match result {
                        Ok((w, h, rgba)) => {
                            t.dark = rgba.chunks_exact(4).all(|p| p[0] < 6 && p[1] < 6 && p[2] < 6);
                            let img = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                            match &mut t.tex {
                                Some(tex) => tex.set(img, egui::TextureOptions::LINEAR),
                                None => t.tex = Some(ctx.load_texture(format!("thumb-{}", vm), img, egui::TextureOptions::LINEAR)),
                            }
                            t.error = None;
                        }
                        Err(e) => t.error = Some(e),
                    }
                }
                Msg::SnapDescribed { sid, vm, snap, name, text, result } => {
                    self.snap_saving.remove(&snap);
                    let server = self.server(sid).map(|s| s.name()).unwrap_or_default();
                    match result {
                        Ok(()) => {
                            // show it right away; the next refresh reads it back from the server
                            if let Some(x) = self.server_mut(sid).and_then(|s| s.data.as_mut()).and_then(|d| d.vms.iter_mut().find(|v| v.rref == vm)).and_then(|v| v.snapshots.iter_mut().find(|x| x.rref == snap)) {
                                x.description = text;
                            }
                            self.add_log(sid, &server, Some(&vm), &name, &format!("Changed the description of snapshot '{}'", name), "Done");
                        }
                        Err(e) => {
                            self.add_log(sid, &server, Some(&vm), &name, &format!("Couldn't change the description of snapshot '{}': {}", name, e.message()), "Failed");
                            self.dialog = Dialog::Message { title: "Description".into(), text: format!("Couldn't change the description of '{}'.\n\n{}", name, e.message()) };
                        }
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
        for (sid, sr) in std::mem::take(&mut self.auto_queue) {
            if !self.sr_busy.contains(&sr) {
                self.repair_job(sid, sr, true, ctx);
            }
        }
    }

    fn on_fetched(&mut self, id: u32, r: Result<PoolData, ApiError>) {
        let shot = self.shot_path.is_some();
        let Some(idx) = self.servers.iter().position(|s| s.id == id) else { return };
        match r {
            Ok(d) => {
                // storage found unmounted is mounted again, once while it stays so (not the
                // detached kind: that needs to be told where it is)
                if self.auto_repair && (!shot || self.shot_show == "sr-auto-repair") {
                    for x in &d.srs {
                        let key = (id, x.rref.clone());
                        match d.sr_trouble(x) {
                            Some(t) if !t.detached => {
                                if self.auto_tried.insert(key.clone()) {
                                    self.auto_queue.push(key);
                                }
                            }
                            _ => {
                                self.auto_tried.remove(&key);
                            }
                        }
                    }
                }
                // servers that stopped answering (or came back) since the last look: said under
                // Activities (a server shut down from here is expected, not a failure)
                let before: HashMap<String, bool> = self.servers[idx].data.as_ref().map(|o| o.hosts.iter().map(|h| (h.rref.clone(), h.is_up())).collect()).unwrap_or_default();
                let mut changes: Vec<(String, String, String, &str)> = vec![];
                for h in &d.hosts {
                    let was = before.get(&h.rref).copied().unwrap_or(true);
                    if was && !h.is_up() {
                        let rebooting = self.host_rebooting.contains_key(&h.rref);
                        let expected = (self.host_busy.contains(&h.rref) || rebooting) && !h.live;
                        let what = if rebooting { "is restarting" } else if h.live { "isn't answering" } else { "is off" };
                        changes.push((h.rref.clone(), h.name.clone(), format!("{} {}", h.name, what), if expected { "Done" } else { "Failed" }));
                    } else if !was && h.is_up() && !self.host_waking.contains_key(&h.rref) {
                        let back = if self.host_rebooting.remove(&h.rref).is_some() { "is back" } else { "answers again" };
                        changes.push((h.rref.clone(), h.name.clone(), format!("{} {}", h.name, back), "Done"));
                    } else if h.is_up() && self.host_rebooting.get(&h.rref).map_or(false, |since| h.boot_time.map_or(false, |b| b >= since - 60)) {
                        // back without being seen gone: the coordinator, whose restart took the
                        // connection with it (it started after it was told to restart)
                        self.host_rebooting.remove(&h.rref);
                        changes.push((h.rref.clone(), h.name.clone(), format!("{} is back", h.name), "Done"));
                    }
                }
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
                // its coordinator's name and Wake-on-LAN address, kept for turning it on
                let wake = d.hosts.iter().find(|h| h.is_master).and_then(|h| h.wake_target().map(|w| (h.name.clone(), w)));
                if wake.is_some() && wake != s.wake {
                    if !s.conn.is_demo() && !shot {
                        save_wake(&s.conn.hostname(), wake.as_ref());
                    }
                    s.wake = wake;
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
                            "home" | "iso-library" | "vm-storage" | "new-storage" | "new-storage-inside" | "new-storage-error" => self.select("root".into()),
                            "storage" | "pool" | "pool-details" | "pool-updates" | "pool-updating" | "pool-update-stopped" => self.select(format!("srv:{}", id)),
                            "isos" | "iso-disconnect" | "iso-delete" | "iso-describe" | "iso-upload" | "iso-upload-choose" | "iso-upload-web" | "iso-upload-done" | "sr-unmounted" | "sr-detached" | "sr-auto-repair" | "sr-repair-failed" => {
                                self.expanded.insert(format!("folder:{}:srs", id));
                                self.select(format!("sr:{}:sr-iso", id));
                            }
                            "search" | "logs" => self.select("root".into()),
                            "host-down" => self.select(format!("host:{}:host-b", id)),
                            "host-slow-restart" => {
                                // told to restart fourteen minutes ago, not back since
                                self.host_rebooting.insert("host-b".into(), crate::pool_update::now() - 14 * 60);
                                self.select(format!("host:{}:host-b", id));
                            }
                            "host" | "host-details" | "host-shutdown" | "host-restart" | "host-maintenance" | "host-performance" => self.select(format!("host:{}:host-a", id)),
                            // (with SVM_DEMO_OFF=host-b, or SVM_DEMO_MAINTENANCE=host-b)
                            "host-off" | "host-in-maintenance" | "host-exit-maintenance" => self.select(format!("host:{}:host-b", id)),
                            "host-waking" => {
                                // told to turn on twelve minutes ago, no answer since
                                let log = self.add_log(id, "Demo Pool", None, "demo-host-02", "Turning on server 'demo-host-02': waiting for it to answer", "In progress");
                                self.log[log].activity = true;
                                let since = Instant::now().checked_sub(Duration::from_secs(12 * 60)).unwrap_or_else(Instant::now);
                                self.host_waking.insert("host-b".into(), Waking { sid: id, host: Some("host-b".into()), name: "demo-host-02".into(), since, log });
                                self.select(format!("host:{}:host-b", id));
                            }
                            "templates" | "networks" | "networks-sorted" | "delete-network" | "new-network" | "new-network-error" => self.select("root".into()),
                            "template" => self.select(format!("tpl:{}:tpl-web", id)),
                            // a running VM chosen: it can't be made a template now
                            "new-template" => self.select(format!("vm:{}:vm-web", id)),
                            "make-template" | "copy-vm" | "template-made" | "vm-off" | "vm-edited" | "move-vm-off" => self.select(format!("vm:{}:vm-desk", id)),
                            "move-vm-local" => self.select(format!("vm:{}:vm-mail", id)),
                            "sr-shared" => {
                                self.expanded.insert(format!("folder:{}:srs", id));
                                self.select(format!("sr:{}:sr-nfs", id));
                            }
                            "vm-bios" => self.select(format!("vm:{}:vm-db", id)),
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
                self.check_unfinished(id);
                let name = self.servers[idx].name();
                for (h, host, text, status) in changes {
                    self.notice(id, &name, Some(&h), &host, &text, status);
                }
                // a refresh asked for: how it went, servers not answering said
                let key = format!("refresh:{}", id);
                if self.jobs.contains_key(&key) {
                    let down: Vec<String> = self.servers[idx].data.as_ref().map(|d| d.hosts.iter().filter(|h| !h.is_up()).map(|h| format!("{} {}", h.name, if h.live { "isn't answering" } else { "is off" })).collect()).unwrap_or_default();
                    if down.is_empty() {
                        self.job_done(&key, id, &name, None, &name, &format!("Refreshed {}", name), "Done");
                    } else {
                        self.job_done(&key, id, &name, None, &name, &format!("Refreshed {}, but {}", name, down.join("; ")), "Failed");
                    }
                }
            }
            Err(e) => {
                let msg = e.message();
                let key = format!("refresh:{}", id);
                if self.jobs.contains_key(&key) {
                    let name = self.servers[idx].name();
                    self.job_done(&key, id, &name, None, &name, &format!("Couldn't refresh {}: {}", name, msg), "Failed");
                }
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
    /// The user's template a tree node stands for, with its server.
    fn template_of<'a>(&'a self, node: &Node) -> Option<(&'a ServerEntry, &'a VmInfo)> {
        if node.kind != Kind::Template {
            return None;
        }
        let s = self.server(node.sid?)?;
        Some((s, s.data.as_ref()?.user_template(node.rref.as_deref()?)?))
    }

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
    /// A server's icon: off when disabled or shut down; a red ring turning while it shuts down, a
    /// green one while it restarts.
    fn host_icon(&self, h: &HostInfo) -> IconKind {
        if self.host_rebooting.contains_key(&h.rref) || self.host_waking.contains_key(&h.rref) {
            IconKind::Busy(Tone::Green)
        } else if self.host_maint.contains_key(&h.rref) {
            IconKind::Busy(Tone::Orange)
        } else if self.host_busy.contains(&h.rref) {
            IconKind::Busy(Tone::Red)
        } else if h.enabled && h.is_up() {
            IconKind::Host
        } else {
            IconKind::HostOff
        }
    }

    /// A VM's icon, or, while a power action is under way, a ring turning in that action's colour.
    /// A VM's icon, or, while a power action is under way, a ring turning in that action's colour;
    /// grey (unknown) while the server it runs on is off or not answering.
    fn vm_icon(s: &ServerEntry, v: &VmInfo) -> IconKind {
        match s.busy.get(&v.rref) {
            Some((_, op)) => IconKind::Busy(Tone::of_action(op)),
            None if s.data.as_ref().map_or(false, |d| d.host_down_for(v).is_some()) => IconKind::Vm(PowerState::Unknown),
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
                Node::new("cat:tpls".into(), "Templates".into(), IconKind::Template, Kind::Category, None, None),
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
                    let icon = self.host_icon(h);
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
                    cats[3].children.push(Node::new(format!("sr:{}:{}", id, x.rref), x.name.clone(), sr_icon(d, x, self.sr_busy.contains(&x.rref)), Kind::Sr, Some(id), Some(x.rref.clone())));
                }
                for t in &d.user_templates {
                    cats[5].children.push(template_node(id, s, t));
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
                let mut tpls: Vec<Node> = d.user_templates.iter().map(|t| template_node(id, s, t)).collect();
                tpls.sort_by_key(|n| n.text.to_lowercase());
                let sr_node = |x: &SrInfo| Node::new(format!("sr:{}:{}", id, x.rref), x.name.clone(), sr_icon(d, x, self.sr_busy.contains(&x.rref)), Kind::Sr, Some(id), Some(x.rref.clone()));
                let folder = |what: &str, label: &str, kids: Vec<Node>| {
                    let mut f = Node::new(format!("folder:{}:{}", id, what), format!("{} ({})", label, kids.len()), IconKind::Folder, Kind::Folder, Some(id), Some(what.to_string()));
                    f.children = kids;
                    // templates and networks have their own pages on the toolbar
                    f.hidden = what == "tpls" || what == "nets";
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
                        let icon = self.host_icon(h);
                        let mut hn = Node::new(format!("host:{}:{}", id, h.rref), h.name.clone(), icon, Kind::Host, Some(id), Some(h.rref.clone()));
                        hn.children.extend(vms.iter().filter(|v| v.home() == Some(h.rref.as_str())).map(|v| vm_node(v)));
                        hn.children.extend(srs.iter().filter(|x| x.host.as_deref() == Some(h.rref.as_str())).map(|x| sr_node(x)));
                        top.children.push(hn);
                    }
                    top.children.extend(vms.iter().filter(|v| v.home().and_then(|h| d.host(h)).is_none()).map(|v| vm_node(v)));
                    if !tpls.is_empty() {
                        top.children.push(folder("tpls", "Templates", tpls.clone()));
                    }
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
                    if !tpls.is_empty() {
                        top.children.push(folder("tpls", "Templates", tpls.clone()));
                    }
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
        let prog = self.progress(log);
        if let Some(s) = self.server_mut(sid) {
            s.busy.insert(vref.clone(), (format!("{}…", a.verb), a.op));
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            crate::conn::watch(&prog);
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
                Intent::RepairSr(sid, sr) => self.ask_repair(sid, sr, ctx),
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
                Intent::MoveVm(sid, vm, to) => {
                    let Some(s) = self.server(sid) else { continue };
                    let busy = s.busy.contains_key(&vm) || self.vm_deleting.contains(&vm);
                    let Some(d) = s.data.as_ref() else { continue };
                    let Some(v) = d.vm(&vm) else { continue };
                    self.dialog = Dialog::MoveVm(Box::new(MoveDialog::new(sid, d, v, to.as_deref(), busy)));
                }
                Intent::MakeDefaultSr(sid, sr) => self.make_default(sid, sr, None, ctx),
                Intent::EnableHost(sid, host) => {
                    let Some((conn, server, name)) = self.server(sid).and_then(|s| Some((s.conn.clone(), s.name(), s.data.as_ref()?.host(&host)?.name.clone()))) else { continue };
                    if self.host_enabling.insert(host.clone()) {
                        let log = self.add_log(sid, &server, None, &name, &format!("Letting '{}' take VMs again", name), "In progress");
                        self.if_closed(log, format!("'{}' may stay not taking VMs (it was still starting): use Take VMs again later.", name));
                        let prog = self.progress(log);
                        let (tx, ctx) = (self.tx.clone(), ctx.clone());
                        std::thread::spawn(move || {
                            crate::conn::watch(&prog);
                            let result = conn.enable_host(&host);
                            let _ = tx.send(Msg::HostEnabled { sid, host, name, log, result });
                            ctx.request_repaint();
                        });
                    }
                }
                Intent::AskShutdownHost(sid, host, reboot) => {
                    if let Some(d) = self.server(sid).and_then(|s| s.data.as_ref()) {
                        if let Some(h) = d.host(&host) {
                            let (moving, stopping) = leaving(d, &host);
                            let (coordinator, alone) = (h.is_master && d.hosts.len() > 1, d.hosts.len() == 1);
                            self.dialog = Dialog::ConfirmHostShutdown { sid, host, name: h.name.clone(), moving, stopping, coordinator, alone, reboot };
                        }
                    }
                }
                Intent::PowerOn(sid, host) => self.power_on(sid, host, ctx),
                Intent::AskMaintenance(sid, host) => {
                    let Some(d) = self.server(sid).and_then(|s| s.data.as_ref()) else { continue };
                    let Some(h) = d.host(&host) else { continue };
                    let dialog = if h.in_maintenance() {
                        // what it moved off and still runs elsewhere, and what it shut down and is still off
                        let vm = |u: &String| d.vms.iter().find(|v| &v.uuid == u);
                        let moved = h.maint_moved.iter().filter_map(vm).filter(|v| v.power == PowerState::Running && v.host.as_deref() != Some(host.as_str())).map(|v| v.name.clone()).collect();
                        let stopped = h.maint_stopped.iter().filter_map(vm).filter(|v| v.power == PowerState::Halted).map(|v| v.name.clone()).collect();
                        Dialog::ExitMaintenance { sid, host: host.clone(), name: h.name.clone(), moved, stopped, back: true }
                    } else {
                        let (moving, stopping) = leaving(d, &host);
                        Dialog::ConfirmMaintenance { sid, host: host.clone(), name: h.name.clone(), moving, stopping, coordinator: h.is_master && d.hosts.len() > 1 }
                    };
                    self.dialog = dialog;
                }
                Intent::AskDeleteVm(sid, vm) => {
                    if self.server(sid).map_or(false, |s| s.busy.contains_key(&vm)) {
                        continue;
                    }
                    if let Some(v) = self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.vm(&vm)) {
                        let disks: Vec<VmDisk> = v.disks.iter().filter(|k| k.kind == "Disk").cloned().collect();
                        let running = v.power != PowerState::Halted;
                        self.dialog = Dialog::ConfirmDeleteVm { sid, vm, name: v.name.clone(), running, disks, snapshots: v.snapshots.len(), delete_disks: true };
                    }
                }
                Intent::TakeSnapshot(sid, vm) => {
                    if let Some(v) = self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.vm(&vm)) {
                        let name = fmt_date_time(unix_now(), local_offset(unix_now()));
                        let can_memory = v.can("checkpoint");
                        let memory_size = if v.mem_actual > 0 { v.mem_actual } else { v.mem_dyn_max };
                        self.dialog = Dialog::TakeSnapshot { sid, vm, vm_name: v.name.clone(), name, description: String::new(), memory: false, can_memory, memory_size, focus: true };
                    }
                }
                Intent::AskRevert(sid, snap) => {
                    if let Some((v, x)) = self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.vms.iter().find_map(|v| v.snapshot(&snap).map(|x| (v, x)))) {
                        let mut parts = vec!["What changed since then is lost.".to_string()];
                        if v.power != PowerState::Halted {
                            parts.push("It's on: it's turned off first, the hard way.".into());
                        }
                        parts.push(if x.memory {
                            "Then it resumes right where it was then, its programs still open.".into()
                        } else if x.was_running {
                            "Then it starts again, as it was running then.".into()
                        } else {
                            "It stays off, as it was then.".into()
                        });
                        self.dialog = Dialog::ConfirmRevert { sid, snap, vm_name: v.name.clone(), snap_name: x.name.clone(), text: parts.join(" "), first: true };
                    }
                }
                Intent::AskDeleteSnapshot(sid, snap) => {
                    if let Some((v, x)) = self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.vms.iter().find_map(|v| v.snapshot(&snap).map(|x| (v, x)))) {
                        self.dialog = Dialog::ConfirmDeleteSnapshot { sid, snap, vm_name: v.name.clone(), snap_name: x.name.clone(), memory: x.memory };
                    }
                }
                Intent::SaveSnapshotDescription(sid, snap, text) => self.save_snapshot_description(sid, snap, text, ctx),
                Intent::MakeTemplate(sid, source) => {
                    let Some(d) = self.server(sid).and_then(|s| s.data.as_ref()) else { continue };
                    let date = fmt_date_time(unix_now(), local_offset(unix_now()));
                    if let Some(v) = d.vm(&source) {
                        let (name, description) = (format!("{} template", v.name), format!("From '{}', {}", v.name, date));
                        self.dialog = Dialog::MakeTemplate { sid, vm: v.rref.clone(), source, from: format!("'{}' (it stays as it is)", v.name), name, description, power_cut: false, focus: true, pick: vec![] };
                    } else if let Some((v, x)) = d.vms.iter().find_map(|v| v.snapshot(&source).map(|x| (v, x))) {
                        let (name, description) = (format!("{} – {}", v.name, x.name), format!("From the snapshot '{}' of '{}'", x.name, v.name));
                        let from = format!("the snapshot '{}' of '{}'", x.name, v.name);
                        self.dialog = Dialog::MakeTemplate { sid, vm: v.rref.clone(), source, from, name, description, power_cut: x.was_running, focus: true, pick: vec![] };
                    }
                }
                Intent::CopyVm(sid, vm) => {
                    let Some(d) = self.server(sid).and_then(|s| s.data.as_ref()) else { continue };
                    let Some(v) = d.vm(&vm) else { continue };
                    // where its disks can go: where they are (an instant copy), or other storage for
                    // disks that the server holding them also sees (a full copy)
                    let now: Option<&SrInfo> = v.disks.iter().find(|k| k.kind == "Disk").and_then(|k| d.sr(&k.sr));
                    let mut stores: Vec<(String, String, bool)> = vec![];
                    if let Some(x) = now {
                        stores.push((x.rref.clone(), format!("{}  ·  where it is, an instant copy", x.name), true));
                    }
                    let mut others: Vec<&SrInfo> = d
                        .srs
                        .iter()
                        .filter(|x| !x.is_iso() && x.kind != "udev" && x.size > 0 && Some(x.rref.as_str()) != now.map(|n| n.rref.as_str()))
                        .filter(|x| match (now.and_then(|n| n.host.as_deref()), x.host.as_deref()) {
                            (Some(a), Some(b)) => a == b,
                            _ => true,
                        })
                        .collect();
                    others.sort_by_key(|x| (x.host.is_some(), -(x.size - x.used)));
                    stores.extend(others.iter().map(|x| (x.rref.clone(), format!("{}  ·  {} free, a full copy", x.name, fmt_bytes((x.size - x.used).max(0))), false)));
                    let taken: Vec<String> = d.vms.iter().map(|v| v.name.clone()).collect();
                    let name = unique_name(&format!("{} (copy)", v.name), &taken);
                    self.dialog = Dialog::CopyVm { sid, vm, vm_name: v.name.clone(), name, stores, store: 0, focus: true };
                }
                Intent::ShowLog(index) => self.dialog = Dialog::LogDetail { index, copied: None },
                Intent::EditVm(sid, vm, change) => self.edit_vm(sid, vm, change, ctx),
                Intent::AskEditVm(sid, vm, change, title, text) => self.dialog = Dialog::ConfirmEditVm { sid, vm, change, title, text },
                Intent::NewVmFromTemplate(sid, tpl) => {
                    if let Some(d) = self.server(sid).and_then(|s| s.data.as_ref()) {
                        self.dialog = Dialog::NewVm(Box::new(NewVmDialog::new(sid, d, None).with_template(&tpl)));
                    }
                }
                Intent::AskDeleteTemplate(sid, tpl) => {
                    if let Some(t) = self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.user_template(&tpl)) {
                        let disk = t.disks.iter().filter(|k| k.kind == "Disk").map(|k| k.size).sum();
                        self.dialog = Dialog::ConfirmDeleteTemplate { sid, tpl, name: t.name.clone(), disk };
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
                Intent::Dropped(paths) => match self.drop_library(sel).map(|(sid, sr, _)| (sid, sr)) {
                    Some((sid, sr)) => {
                        let mut isos = paths.into_iter().filter(|p| p.extension().map_or(false, |e| e.eq_ignore_ascii_case("iso")));
                        match isos.next() {
                            Some(p) => self.start_upload(sid, sr, Source::File(p), None, ctx),
                            None => self.dialog = Dialog::Message { title: "Upload".into(), text: "Only .iso files can go in an ISO library.".into() },
                        }
                    }
                    None => self.dialog = Dialog::Message { title: "Upload".into(), text: "To upload a file, select an ISO library in the tree (or its card on the ISO Library page), then drop the file on the window.".into() },
                },
                Intent::AskForgetSr(sid, sr) => {
                    if let Some(x) = self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.sr(&sr)) {
                        let place = if x.location.is_empty() { String::new() } else { format!(" {}", x.location) };
                        let text = format!("Are you sure you want to disconnect the ISO library '{}'?\n\nThe pool stops using the share{}. The ISO files on it aren't deleted, and you can add it again with New ISO Library (on the ISO Library page).", x.name, place);
                        self.dialog = Dialog::ConfirmSr { sid, sr, text };
                    }
                }
                Intent::PoolUpdates(sid) => {
                    let pools: Vec<u32> = self.servers.iter().filter(|s| s.data.is_some()).map(|s| s.id).collect();
                    if let Some(sid) = sid.or(sel.sid).filter(|id| pools.contains(id)).or(pools.first().copied()) {
                        // an update that ended is behind it: the list again
                        if self.pool_runs.get(&sid).map_or(false, |r| r.ended().is_some()) {
                            self.pool_runs.remove(&sid);
                        }
                        self.dialog = Dialog::PoolUpdate(Box::new(PoolUpdateDialog::new(sid)));
                    }
                }
                // the toolbar's ISO Library and VM Storage: All servers' page of them
                Intent::IsoLibrary => {
                    self.select("root".into());
                    self.tab = Tab::IsoLibrary;
                }
                Intent::VmStorage => {
                    self.select("root".into());
                    self.tab = Tab::VmStorage;
                }
                Intent::Templates => {
                    self.select("root".into());
                    self.tab = Tab::Templates;
                }
                Intent::Networks => {
                    self.select("root".into());
                    self.tab = Tab::Networks;
                }
                Intent::Alerts => {
                    self.select("root".into());
                    self.tab = Tab::Alerts;
                }
                Intent::DismissAlerts(list) => self.dismiss_alerts(list, ctx),
                Intent::AlertRules(sid, cls, obj, name, watched) => self.open_alert_rules(sid, cls, obj, name, watched, ctx),
                Intent::NewTemplate => {
                    // every VM of every pool, the ones that can't be made a template now saying why
                    let mut pick: Vec<TemplateSource> = vec![];
                    let pools = self.servers.iter().filter(|s| s.data.is_some()).count();
                    for s in &self.servers {
                        let Some(d) = s.data.as_ref() else { continue };
                        for v in &d.vms {
                            let why = if let Some(b) = s.busy.get(&v.rref) {
                                Some((format!("'{}' is busy.", v.name), format!("It's {}: wait till that's done.", b.0.trim_end_matches('…').to_lowercase())))
                            } else if v.power != PowerState::Halted {
                                Some((
                                    format!("'{}' is {}.", v.name, v.power.label().to_lowercase()),
                                    format!("A template is made from a shut-down VM: shut '{}' down first, or make the template from one of its snapshots (on its Snapshots tab).", v.name),
                                ))
                            } else {
                                None
                            };
                            let state = if v.power == PowerState::Halted { String::new() } else { format!("  ({})", v.power.label().to_lowercase()) };
                            let label = if pools > 1 { format!("{}  ·  {}{}", v.name, s.name(), state) } else { format!("{}{}", v.name, state) };
                            pick.push(TemplateSource { sid: s.id, vm: v.rref.clone(), name: v.name.clone(), label, why });
                        }
                    }
                    pick.sort_by_key(|p| (p.why.is_some(), p.label.to_lowercase()));
                    // the selected VM first, if it's one; else the first that can be
                    let first = sel.rref.as_ref().filter(|_| sel.kind == Kind::Vm).and_then(|r| pick.iter().position(|p| &p.vm == r)).unwrap_or(0);
                    if let Some(p) = pick.get(first) {
                        let date = fmt_date_time(unix_now(), local_offset(unix_now()));
                        let (sid, vm, name, description) = (p.sid, p.vm.clone(), format!("{} template", p.name), format!("From '{}', {}", p.name, date));
                        let from = format!("'{}' (it stays as it is)", p.name);
                        self.dialog = Dialog::MakeTemplate { sid, vm: vm.clone(), source: vm, from, name, description, power_cut: false, focus: true, pick };
                    } else {
                        self.dialog = Dialog::Message { title: "New Template".into(), text: "There's no VM to make a template from yet.".into() };
                    }
                }
                Intent::AskDeleteNetwork(sid, net) => {
                    let Some(n) = self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.net(&net)) else { continue };
                    // what changes, what doesn't, and how to have it back (it's only offered while no
                    // VM's or template's card is on it)
                    let text = if n.vlan >= 0 {
                        format!(
                            "The pool stops using VLAN {vlan} on {card}, on every server.\n\n\
                             No VM or template has a card on it, so nothing loses its connection; {card} and its other networks go on as they are. Your switch isn't changed: VLAN {vlan} stays set up there.\n\n\
                             To have it back, make it again with New Network (VLAN {vlan} on {card}).",
                            vlan = n.vlan,
                            card = n.device
                        )
                    } else {
                        "No VM or template has a card on it, so nothing loses its connection. It's private (no card): nothing outside the servers is involved.\n\nTo have it back, make it again with New Network.".to_string()
                    };
                    self.dialog = Dialog::ConfirmDeleteNetwork { sid, net, name: n.name.clone(), text };
                }
                Intent::NewNetwork => {
                    let pools: Vec<(u32, String)> = self.servers.iter().filter(|s| s.data.is_some()).map(|s| (s.id, s.name())).collect();
                    let sid = sel.sid.filter(|id| pools.iter().any(|p| p.0 == *id)).or(pools.first().map(|p| p.0));
                    if let Some(sid) = sid {
                        let cards = self.network_cards();
                        self.dialog = Dialog::NewNetwork(Box::new(NetworkDialog::new(sid, pools, cards)));
                    }
                }
                Intent::NewIsoLibrary => {
                    let pools: Vec<(u32, String)> = self.servers.iter().filter(|s| s.data.is_some()).map(|s| (s.id, s.name())).collect();
                    let sid = sel.sid.filter(|id| pools.iter().any(|p| p.0 == *id)).or(pools.first().map(|p| p.0));
                    if let Some(sid) = sid {
                        self.dialog = Dialog::Iso(Box::new(IsoDialog::new(sid, pools)));
                    }
                }
                Intent::NewStorage => {
                    // each pool, and whether its default storage is shared already
                    let pools: Vec<(u32, String, bool)> = self
                        .servers
                        .iter()
                        .filter_map(|s| s.data.as_ref().map(|d| (s.id, s.name(), d.default_sr.as_deref().and_then(|r| d.sr(r)).map_or(false, |x| x.shared))))
                        .collect();
                    let sid = sel.sid.filter(|id| pools.iter().any(|p| p.0 == *id)).or(pools.first().map(|p| p.0));
                    if let Some(sid) = sid {
                        self.dialog = Dialog::NewStorage(Box::new(StorageDialog::new(sid, pools)));
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
                    let ids: Vec<(u32, String)> = self.servers.iter().filter(|s| only.map_or(true, |o| o == s.id)).map(|s| (s.id, s.name())).collect();
                    for (id, name) in ids {
                        // said under Activities: how it went, servers not answering included
                        if !self.jobs.contains_key(&format!("refresh:{}", id)) {
                            self.job(format!("refresh:{}", id), id, &name, None, &name, &format!("Refreshing {}", name));
                        }
                        if let Some(s) = self.server_mut(id) {
                            s.next_refresh = Instant::now();
                            if s.state == ConnState::Disconnected {
                                s.state = ConnState::Connecting;
                            }
                        }
                    }
                }
                Intent::View(objects) => self.objects_view = objects,
                Intent::Tab(t) => {
                    if t == Tab::Console && self.tab != Tab::Console {
                        self.before_console = self.tab;
                    }
                    self.tab = t;
                }
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
        // label becomes a tooltip): first New VM, ISO Library, VM Storage and Updates (rank 2),
        // then all of them.
        let fits = |level: u8| {
            let w = |l: &str, rank: u8| if level >= rank { 32.0 } else { tool_width(ui, l) } + 2.0;
            let buttons: f32 = w("Connect", 3) + w("New VM", 2) + w("ISO Library", 2) + w("VM Storage", 2) + w("Templates", 2) + w("Networks", 2) + w("Updates", 2) + w("Alerts", 2);
            36.0 + 40.0 + 10.0 + 16.0 + buttons + 160.0 <= ui.available_width() // 160: the app's name
        };
        let level = (0..3).find(|&l| fits(l)).unwrap_or(3);
        let compact = |rank: u8| level >= rank;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if icon_button(ui, "settings", &pal, true, false).on_hover_text("Settings").clicked() {
                out.push(Intent::Settings);
            }
            self.activities_button(ui, out);
            // Settings and Activities, then the buttons that make things
            ui.add_space(2.0);
            vsep(ui, &pal);
            ui.add_space(2.0);
            if tool_button(ui, "connect", "Connect", true, compact(3), &pal).clicked() {
                out.push(Intent::AddServer);
            }
            let has_pool = self.servers.iter().any(|s| s.data.is_some());
            if tool_button(ui, "newvm", "New VM", has_pool, compact(2), &pal).on_hover_text("Create a virtual machine that installs from an ISO file").clicked() {
                out.push(Intent::NewVm(sel.sid, None));
            }
            if tool_button(ui, "iso", "ISO Library", has_pool, compact(2), &pal).on_hover_text("Your installation discs (.iso files) in every library, and adding a library").clicked() {
                out.push(Intent::IsoLibrary);
            }
            if tool_button(ui, "storage", "VM Storage", has_pool, compact(2), &pal).on_hover_text("Where your VMs' disks are, and adding shared storage for them").clicked() {
                out.push(Intent::VmStorage);
            }
            if tool_button(ui, "templates", "Templates", has_pool, compact(2), &pal).on_hover_text("Your templates, and making one from a VM").clicked() {
                out.push(Intent::Templates);
            }
            if tool_button(ui, "networks", "Networks", has_pool, compact(2), &pal).on_hover_text("Your VMs' networks, and adding one").clicked() {
                out.push(Intent::Networks);
            }
            // a pool's updates: a red dot when one has security updates waiting, a yellow one (the
            // network tiles' "out") when one has others, or a server to restart for them
            let worst = self.servers.iter().filter_map(|s| self.pool_update_status(s.id)).map(|st| st.1).filter(|st| matches!(st, Standing::Pending | Standing::Bad)).max();
            let resp = tool_button(ui, "updates", "Updates", has_pool, compact(2), &pal);
            if let Some(st) = worst {
                ui.painter().circle_filled(resp.rect.min + vec2(24.0, 6.0), 4.0, if st == Standing::Bad { pal.red } else { pal.orange });
            }
            let tip = match worst {
                Some(Standing::Bad) => "Install your pools' updates: one has security updates waiting",
                Some(_) => "Install your pools' updates: one has updates waiting",
                None => "Install your pools' updates",
            };
            if resp.on_hover_text(tip).clicked() {
                out.push(Intent::PoolUpdates(sel.sid));
            }
            // alerts: a bell, with how many need attention (red when one's critical)
            let need: Vec<Severity> = self.alert_rows().into_iter().map(|r| r.sev).filter(|s| s.needs_attention()).collect();
            let resp = tool_button(ui, "alerts", "Alerts", true, compact(2), &pal);
            if !need.is_empty() {
                let text = if need.len() > 99 { "99+".to_string() } else { need.len().to_string() };
                let col = if need.contains(&Severity::Critical) { pal.red } else { pal.orange };
                let g = ui.painter().layout_no_wrap(text, FontId::new(10.0, FontFamily::Name("bold".into())), Color32::WHITE);
                let at = if compact(2) { resp.rect.center() + vec2(8.0, -8.0) } else { resp.rect.min + vec2(23.0, 8.0) };
                let pill = Rect::from_center_size(at, vec2((g.size().x + 8.0).max(15.0), 15.0));
                ui.painter().rect(pill, Rounding::same(7.5), col, Stroke::new(1.5_f32, pal.toolbar));
                ui.painter().galley(pill.center() - g.size() / 2.0, g, Color32::WHITE);
            }
            let tip = match need.len() {
                0 => "Alerts: nothing needs your attention".to_string(),
                1 => "Alerts: 1 needs your attention".to_string(),
                n => format!("Alerts: {} need your attention", n),
            };
            if resp.on_hover_text(tip).clicked() {
                out.push(Intent::Alerts);
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

    /// How many minutes server `h` has been restarting, when that's longer than a server usually
    /// takes (ten minutes).
    fn slow_restart(&self, h: &HostInfo) -> Option<i64> {
        let since = *self.host_rebooting.get(&h.rref)?;
        let minutes = (crate::pool_update::now() - since) / 60;
        (minutes >= 10).then_some(minutes)
    }

    /// What's wrong with a pool right now, for its Details (each what it's about, and what's
    /// wrong): the connection lost, servers off or not answering, storage not mounted where it
    /// should be, its license running out or run out, its servers on different versions.
    fn pool_issues(&self, s: &ServerEntry) -> Vec<(String, String)> {
        let mut out = vec![];
        let Some(d) = &s.data else { return out };
        // (a server restarting, as asked, isn't a problem; nor is the connection its coordinator's
        // restart takes with it)
        let restarting = |h: &HostInfo| self.host_rebooting.contains_key(&h.rref);
        if s.state == ConnState::Reconnecting && !d.hosts.iter().any(|h| h.is_master && restarting(h)) {
            out.push(("Connection".to_string(), "Lost; reconnecting…".to_string()));
        }
        for h in &d.hosts {
            if let Some(m) = self.slow_restart(h) {
                out.push((h.name.clone(), format!("Taking long to restart: {} minutes since it was told to (look at its screen)", m)));
            }
        }
        for h in d.hosts.iter().filter(|h| h.is_up() && !h.enabled && !h.in_maintenance() && !restarting(h) && !self.host_busy.contains(&h.rref) && !self.host_maint.contains_key(&h.rref)) {
            out.push((h.name.clone(), "Is on but isn't taking new VMs (its Overview has Take VMs again)".to_string()));
        }
        for h in d.hosts.iter().filter(|h| !h.is_up() && !restarting(h) && !self.host_waking.contains_key(&h.rref)) {
            let unknown = d.vms.iter().filter(|v| d.host_down_for(v).map_or(false, |x| x.rref == h.rref)).count();
            let what = if h.live { "Isn't answering" } else { "Is off" };
            let more = match unknown {
                0 => String::new(),
                1 => ": 1 VM's state is unknown".to_string(),
                n => format!(": {} VMs' state is unknown", n),
            };
            out.push((h.name.clone(), format!("{}{}", what, more)));
        }
        for x in &d.srs {
            if let Some(t) = d.sr_trouble(x) {
                out.push((x.name.clone(), if t.nowhere { "Isn't mounted".to_string() } else { "Isn't mounted on every server".to_string() }));
            }
        }
        // settings pointing to storage that's gone: the pool's together when they're the same
        let mut seen: Vec<String> = vec![];
        for (what, r) in d.gone_storage() {
            let pools = what.starts_with("The pool's");
            if pools && seen.contains(&r) {
                continue;
            }
            seen.push(r);
            let what = if pools && what.contains("default") { "Default storage".to_string() } else { what };
            out.push((what, "No longer exists: use Make Default on another storage".to_string()));
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0);
        if let Some((text, standing)) = d.license(now) {
            let trial = d.hosts.iter().any(|h| ["trial", "free"].iter().any(|w| h.edition.to_lowercase().contains(w)));
            if standing == Standing::Bad || (standing == Standing::Watch && !trial) {
                out.push(("License".to_string(), text));
            }
        }
        let versions = d.versions();
        if versions.len() > 1 {
            out.push(("Version".to_string(), format!("Its servers are on different versions: {}", versions.join(", "))));
        }
        out
    }

    /// What's wrong right now, for Activities: connections lost, servers off or not answering
    /// (with the VMs whose state that makes unknown), storage that isn't mounted where it should
    /// be. Each with the tree item it's about, and a softer second line.
    fn problems(&self) -> Vec<(String, String, String)> {
        let mut out = vec![];
        for s in &self.servers {
            let Some(d) = &s.data else {
                if s.state == ConnState::Disconnected {
                    out.push((format!("srv:{}", s.id), format!("Not connected to {}", s.conn.hostname()), s.error.clone().unwrap_or_default()));
                }
                continue;
            };
            // a restart that's taking long
            for h in &d.hosts {
                if let Some(m) = self.slow_restart(h) {
                    out.push((format!("host:{}:{}", s.id, h.rref), format!("{} is taking long to restart", h.name), format!("{} minutes since it was told to; look at its screen", m)));
                }
            }
            // (a server restarting, as asked, isn't a problem; nor the connection its coordinator's
            // restart takes with it)
            let restarting = |h: &HostInfo| self.host_rebooting.contains_key(&h.rref);
            if s.state == ConnState::Reconnecting && !d.hosts.iter().any(|h| h.is_master && restarting(h)) {
                out.push((format!("srv:{}", s.id), format!("Lost the connection to {}", s.name()), "Reconnecting…".into()));
            }
            for h in d.hosts.iter().filter(|h| !h.is_up() && !restarting(h) && !self.host_waking.contains_key(&h.rref)) {
                let what = if h.live { "isn't answering" } else { "is off" };
                let vms: Vec<&str> = d.vms.iter().filter(|v| d.host_down_for(v).map_or(false, |x| x.rref == h.rref)).map(|v| v.name.as_str()).collect();
                let detail = match vms.len() {
                    0 => String::new(),
                    1 => format!("{}'s state is unknown", vms[0]),
                    _ => format!("{} VMs' state is unknown: {}", vms.len(), vms.join(", ")),
                };
                out.push((format!("host:{}:{}", s.id, h.rref), format!("{} {}", h.name, what), detail));
            }
            for x in &d.srs {
                if let Some(t) = d.sr_trouble(x) {
                    let detail = if t.detached { "The pool has no settings for it: Repair asks where it is".to_string() } else { "Repair, on its page, mounts it again".to_string() };
                    out.push((format!("sr:{}:{}", s.id, x.rref), format!("{} isn't mounted{}", x.name, if t.nowhere { "" } else { " everywhere" }), detail));
                }
            }
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0);
            match d.license(now) {
                Some((text, Standing::Bad)) => out.push((format!("srv:{}", s.id), format!("{}'s license ran out", s.name()), text)),
                Some((text, Standing::Watch)) if d.license(now + 30 * 86_400).map_or(false, |l| l.1 == Standing::Bad) => out.push((format!("srv:{}", s.id), format!("{}'s license runs out soon", s.name()), text)),
                _ => {}
            }
        }
        out
    }

    /// Activities, by the gear: what's under way (a ring turning, with how many), each with its
    /// server, how long it's been, how far it is when the server says, and a warning while the
    /// server doesn't answer; then the last few that ended, and a way to the whole log.
    fn activities_button(&self, ui: &mut Ui, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let running: Vec<usize> = (0..self.log.len()).filter(|&i| self.log[i].status == "In progress" && self.log[i].activity).collect();
        let ended: Vec<usize> = (0..self.log.len()).rev().filter(|&i| self.log[i].status != "In progress" && self.log[i].activity).take(5).collect();
        let slot = ui.painter().add(egui::Shape::Noop);
        let inner = |ui: &mut Ui, out: &mut Vec<Intent>| {
            ui.set_min_width(440.0);
            ui.set_max_width(440.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(egui::RichText::new("Activities").font(FontId::new(13.5, FontFamily::Name("bold".into()))).color(pal.ink));
            ui.add_space(2.0);
            // an entry, clickable for its details: its mark, its text (two lines at most), then
            // its server, how long, how far, and a warning while the server doesn't answer
            let entry = |ui: &mut Ui, i: usize, out: &mut Vec<Intent>| {
                let e = &self.log[i];
                let running = e.status == "In progress";
                let p = e.progress.as_ref();
                // an upload's own progress (how much of how much, and how fast), else the server
                // task's (with the bytes it copies, when known)
                let upload = self.uploads.values().find(|u| u.log == i && u.active()).and_then(|u| {
                    let (sent, total) = (u.t.sent.load(Ordering::Relaxed) as i64, u.t.total.load(Ordering::Relaxed) as i64);
                    let secs = u.started.elapsed().as_secs_f64();
                    (total > 0).then(|| (sent, total, (secs >= 2.0 && sent > 0).then(|| sent as f64 / secs)))
                });
                let fraction = upload.map(|(sent, total, _)| sent as f32 / total as f32);
                let fraction = if running { fraction.or_else(|| p.and_then(|p| p.fraction())) } else { None };
                let copied = if running { upload.map(|(s, t, _)| (s, t)).or_else(|| p.and_then(|p| p.copied())) } else { None };
                let rate = if running { upload.and_then(|u| u.2).or_else(|| p.and_then(|p| p.rate())) } else { None };
                let w = ui.available_width();
                // an ended one's time at the right, on its first line
                let time = (!running).then(|| ui.painter().layout_no_wrap(e.time.clone(), FontId::proportional(12.5), pal.ink_faint));
                let time_w = time.as_ref().map_or(0.0, |t| t.size().x + 12.0);
                let ink = if running || e.status == "Failed" { pal.ink } else { pal.ink_soft };
                let mut job = egui::text::LayoutJob::simple(e.text.clone(), FontId::proportional(13.0), ink, w - 30.0 - time_w);
                job.wrap.max_rows = 2;
                job.wrap.overflow_character = Some('…');
                let g = ui.painter().layout_job(job);
                let mut detail = vec![];
                if running {
                    detail.push(e.server.clone());
                    detail.push(fmt_took(e.started.elapsed()));
                    if let Some(f) = fraction {
                        detail.push(format!("{}%", (f * 100.0).round()));
                    }
                    if let Some((done, total)) = copied {
                        detail.push(format!("{} of {}", fmt_bytes(done), fmt_bytes(total)));
                    }
                    if let Some(r) = rate {
                        detail.push(format!("{}/s", fmt_bytes(r as i64)));
                    }
                }
                let quiet = p.and_then(|p| p.silent_for()).filter(|d| running && d.as_secs() >= 5);
                // the text, its details under it, then the bar with room above and below
                let h = 4.0 + g.size().y + if running { 20.0 } else { 0.0 } + if fraction.is_some() { 10.0 } else { 0.0 } + 6.0;
                let (r, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
                if resp.hovered() {
                    ui.painter().rect_filled(r, Rounding::same(5.0), pal.hover);
                }
                let top = r.min + vec2(0.0, 4.0);
                let mark = Rect::from_min_size(top + vec2(4.0, 1.0), vec2(16.0, 16.0));
                if running {
                    paint_icon(ui.painter(), mark, IconKind::Busy(Tone::Accent), &pal, pal.surface);
                } else {
                    ui.painter().circle_filled(mark.center(), 3.5, status_color(&e.status, &pal));
                }
                let gh = g.size().y;
                let first = g.rows.first().map_or(gh, |r| r.rect.height());
                ui.painter().galley(top + vec2(26.0, 0.0), g, ink);
                if let Some(t) = time {
                    ui.painter().galley(pos2(r.max.x - 6.0 - t.size().x, top.y + (first - t.size().y) / 2.0), t, pal.ink_faint);
                }
                let mut y = top.y + gh + 3.0;
                if running {
                    let d = ui.painter().text(pos2(top.x + 26.0, y), Align2::LEFT_TOP, detail.join("  ·  "), FontId::proportional(12.0), pal.ink_soft);
                    if let Some(q) = quiet {
                        let left = LOST_AFTER.saturating_sub(q).as_secs();
                        ui.painter().text(pos2(d.max.x + 10.0, y), Align2::LEFT_TOP, format!("no answer for {} s, gives up in {} s", q.as_secs(), left), FontId::proportional(12.0), pal.orange);
                    }
                    y += 23.0;
                }
                if let Some(f) = fraction {
                    let bar = Rect::from_min_size(pos2(top.x + 26.0, y), vec2(w - 34.0, 4.0));
                    ui.painter().rect_filled(bar, Rounding::same(2.0), pal.border);
                    ui.painter().rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * f.clamp(0.0, 1.0), 4.0)), Rounding::same(2.0), pal.accent);
                }
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Show what happened, and what the server said").clicked() {
                    out.push(Intent::ShowLog(i));
                    ui.close_menu();
                }
            };
            // always two parts: what's under way (or that nothing is), then what ended
            let none = |ui: &mut Ui, text: &str| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.add_space(26.0);
                    ui.label(egui::RichText::new(text).size(13.0).color(pal.ink_soft));
                });
                ui.add_space(4.0);
            };
            ui.label(egui::RichText::new("Under way").size(12.0).color(pal.ink_faint));
            if running.is_empty() {
                none(ui, "Nothing under way right now.");
            }
            for &i in &running {
                entry(ui, i, out);
                ui.ctx().request_repaint_after(Duration::from_millis(500));
            }
            ui.add_space(4.0);
            ui.separator();
            ui.label(egui::RichText::new("Ended").size(12.0).color(pal.ink_faint));
            if ended.is_empty() {
                none(ui, "Nothing has ended yet.");
            }
            for &i in &ended {
                entry(ui, i, out);
            }
            ui.add_space(4.0);
            ui.separator();
            if ui.add(egui::Label::new(egui::RichText::new("All of it in Logs").size(12.5).color(pal.accent)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                out.push(Intent::Select("root".into()));
                out.push(Intent::Tab(Tab::Logs));
                ui.close_menu();
            }
        };
        // with room around it
        let body = |ui: &mut Ui, out: &mut Vec<Intent>| {
            egui::Frame::none().inner_margin(egui::Margin::symmetric(6.0, 4.0)).show(ui, |ui| inner(ui, out));
        };
        let m = egui::menu::menu_custom_button(ui, egui::Button::new("").frame(false).min_size(vec2(32.0, 30.0)), |ui| body(ui, out));
        let r = m.response.rect;
        if m.response.hovered() || m.inner.is_some() {
            ui.painter().set(slot, egui::epaint::RectShape::filled(r, Rounding::same(6.0), pal.hover));
        }
        if self.shot_path.is_some() && (self.shot_show == "activities" || self.shot_show == "activities-moving") {
            // the check's screenshot shows it open
            egui::Area::new(egui::Id::new("activities-shot")).order(egui::Order::Foreground).fixed_pos(r.left_bottom() + vec2(0.0, 4.0)).show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| body(ui, &mut vec![]));
            });
        }
        let icon = Rect::from_center_size(r.center(), vec2(16.0, 16.0));
        if running.is_empty() {
            paint_glyph(ui.painter(), icon, "activity", pal.ink_soft);
        } else {
            paint_icon(ui.painter(), icon, IconKind::Busy(Tone::Accent), &pal, pal.toolbar);
            // how many, in a small badge
            let b = pos2(r.max.x - 7.0, r.min.y + 7.0);
            ui.painter().circle_filled(b, 6.5, pal.accent);
            ui.painter().text(b, Align2::CENTER_CENTER, running.len().to_string(), FontId::proportional(9.5), Color32::WHITE);
        }
        let tip = match running.len() {
            0 => "Activities".to_string(),
            1 => "Activities: 1 under way".to_string(),
            n => format!("Activities: {} under way", n),
        };
        m.response.on_hover_text(tip);
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
                    .stroke(Stroke::new(1.0_f32, pal.border_strong))
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
                    ui.painter().rect(hr, Rounding::same(5.0), pal.surface, Stroke::new(1.0_f32, pal.border));
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
        // a VM can be dragged onto a server of its pool, to move it there
        let draggable = node.kind == Kind::Vm && node.sid.is_some() && node.rref.is_some();
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width().max(200.0), row_h), if draggable { Sense::click_and_drag() } else { Sense::click() });
        let p = ui.painter();
        let x0 = rect.min.x + 6.0 + depth as f32 * 16.0;
        let selected = node.key == self.selected;
        let has_kids = node.children.iter().any(|c| !c.hidden);
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
            let s = Stroke::new(1.4_f32, pal.ink_soft);
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
        if let (true, Some(sid), Some(vm)) = (draggable, node.sid, node.rref.clone()) {
            resp.dnd_set_drag_payload(DragVm { sid, vm, name: node.text.clone() });
        }
        if draggable && resp.dragged() {
            // what's held, by the pointer
            if let Some(at) = ui.ctx().pointer_hover_pos() {
                let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("drag-vm")));
                let g = painter.layout_no_wrap(format!("Move '{}' to a server…", node.text), FontId::proportional(12.5), pal.on_accent);
                let r = Rect::from_min_size(at + vec2(14.0, 10.0), g.size() + vec2(16.0, 8.0));
                painter.rect_filled(r, Rounding::same(6.0), pal.accent);
                painter.galley(r.min + vec2(8.0, 4.0), g, pal.on_accent);
            }
        }
        if node.kind == Kind::Host {
            // a VM of its pool held over it: outlined; dropped: the Move dialog, to it
            if resp.dnd_hover_payload::<DragVm>().map_or(false, |p| Some(p.sid) == node.sid) {
                p.rect_stroke(row, Rounding::same(5.0), Stroke::new(1.5_f32, pal.accent));
            }
            if let Some(p) = resp.dnd_release_payload::<DragVm>() {
                if Some(p.sid) == node.sid {
                    out.push(Intent::Select(format!("vm:{}:{}", p.sid, p.vm)));
                    out.push(Intent::MoveVm(p.sid, p.vm.clone(), node.rref.clone()));
                }
            }
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
        // No menu at all (not even an empty one) for networks, the folders, and storage other than
        // an ISO library (local disks, DVD drives, USB) unless it needs Repair.
        let has_menu = match node.kind {
            Kind::Net | Kind::Folder => false,
            Kind::Template => true,
            Kind::Sr => self.sr_of(node).map_or(false, |(s, x)| x.is_iso() || s.data.as_ref().and_then(|d| d.sr_trouble(x)).is_some()),
            _ => true,
        };
        if !has_menu {
            if open {
                for c in node.children.iter().filter(|c| !c.hidden) {
                    self.tree_ui(ui, c, depth + 1, out);
                }
            }
            return;
        }
        resp.context_menu(|ui| match node.kind {
            Kind::Template => {
                if let (Some(sid), Some(t)) = (node.sid, node.rref.clone()) {
                    let busy = self.server(sid).map_or(false, |s| s.busy.contains_key(&t));
                    if ui.add_enabled(!busy, egui::Button::new("New VM from Template…")).clicked() {
                        out.push(Intent::NewVmFromTemplate(sid, t.clone()));
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.add_enabled(!busy, egui::Button::new(egui::RichText::new("Delete Template…").color(self.pal.red))).clicked() {
                        out.push(Intent::AskDeleteTemplate(sid, t));
                        ui.close_menu();
                    }
                }
            }
            Kind::Sr => {
                // what its page has beside the tabs: an ISO library's actions, Repair when it
                // isn't mounted somewhere
                if let Some((s, x)) = self.sr_of(node) {
                    let (sid, sr, iso) = (s.id, x.rref.clone(), x.is_iso());
                    let trouble = s.data.as_ref().and_then(|d| d.sr_trouble(x));
                    let busy = self.sr_busy.contains(&sr);
                    let uploading = self.uploads.get(&sr).map_or(false, Upload::active);
                    if iso && ui.add_enabled(!busy && !uploading && trouble.is_none(), egui::Button::new("Upload…")).clicked() {
                        out.push(Intent::Upload(sid, sr.clone()));
                        ui.close_menu();
                    }
                    if let Some(t) = &trouble {
                        let label = if busy { "Working…" } else if t.detached { "Repair…" } else { "Repair" };
                        if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                            out.push(Intent::RepairSr(sid, sr.clone()));
                            ui.close_menu();
                        }
                    } else if ui.add_enabled(!busy, egui::Button::new(if busy { "Working…" } else { "Rescan" })).clicked() {
                        out.push(Intent::RescanSr(sid, sr.clone()));
                        ui.close_menu();
                    }
                    if iso {
                        ui.separator();
                        if ui.add_enabled(!busy && !uploading, egui::Button::new(egui::RichText::new("Disconnect…").color(self.pal.red))).clicked() {
                            out.push(Intent::AskForgetSr(sid, sr));
                            ui.close_menu();
                        }
                    }
                }
            }
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
                    // Delete waits while it's moving, copying, taking a snapshot, starting…
                    let busy = self.server(sid).and_then(|s| s.busy.get(&vm)).map(|b| busy_tip(&b.0));
                    let snap_ok = self.vm_of(node).map_or(false, |(s, d, v)| v.can_snapshot() && !s.busy.contains_key(&v.rref) && d.host_down_for(v).is_none());
                    if ui.add_enabled(snap_ok && !deleting, egui::Button::new("Take Snapshot…")).clicked() {
                        out.push(Intent::Select(node.key.clone()));
                        out.push(Intent::TakeSnapshot(sid, vm.clone()));
                        ui.close_menu();
                    }
                    let move_ok = self.vm_of(node).map_or(false, |(s, d, v)| matches!(v.power, PowerState::Running | PowerState::Halted) && !s.busy.contains_key(&v.rref) && d.host_down_for(v).is_none());
                    if ui.add_enabled(move_ok && !deleting, egui::Button::new("Move…")).clicked() {
                        out.push(Intent::Select(node.key.clone()));
                        out.push(Intent::MoveVm(sid, vm.clone(), None));
                        ui.close_menu();
                    }
                    let off = self.vm_of(node).map_or(false, |(s, _, v)| v.power == PowerState::Halted && !s.busy.contains_key(&v.rref));
                    for (label, intent) in [("Copy VM…", Intent::CopyVm(sid, vm.clone())), ("Make Template…", Intent::MakeTemplate(sid, vm.clone()))] {
                        if ui.add_enabled(off && !deleting, egui::Button::new(label)).on_disabled_hover_text(SHUT_DOWN_FIRST).clicked() {
                            out.push(Intent::Select(node.key.clone()));
                            out.push(intent);
                            ui.close_menu();
                        }
                    }
                    let del = ui.add_enabled(!deleting && busy.is_none(), egui::Button::new(egui::RichText::new("Delete VM…").color(pal.red)));
                    if del.on_disabled_hover_text(busy.as_deref().unwrap_or("")).clicked() {
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
            for c in node.children.iter().filter(|c| !c.hidden) {
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

    /// Whether the selected server can be told to turn on from here: pool, server (None: the
    /// connection's own, known only from the last time), its button's tip, and whether it can be
    /// clicked now. A server that's off; or the coordinator while the pool doesn't answer through
    /// it (it may be off).
    fn power_on_of(&self, sel: &Node) -> Option<(u32, Option<String>, String, bool)> {
        if let Some((s, h)) = self.host_of(sel) {
            let lost = h.is_master && s.state != ConnState::Connected;
            let off = !h.live && !h.unreachable && !self.host_busy.contains(&h.rref);
            if !(off || lost) {
                return None;
            }
            let waking = self.host_waking.contains_key(&h.rref);
            let pool = s.state == ConnState::Connected && !h.is_master;
            let known = h.wake_target().is_some() || (h.is_master && s.wake.is_some());
            let tip = if waking {
                "Turning on…".to_string()
            } else if pool || known {
                format!("Turn on {} (Wake-on-LAN)", h.name)
            } else {
                "Its network card's address isn't known here: turn it on at the server".to_string()
            };
            return Some((s.id, Some(h.rref.clone()), tip, !waking && (pool || known)));
        }
        let (sid, _) = self.root_of(sel)?;
        let s = self.server(sid).filter(|s| s.data.is_none() && s.state != ConnState::Connected)?;
        let (name, _) = s.wake.as_ref()?;
        let waking = self.host_waking.contains_key(&format!("srv:{}", sid));
        Some((sid, None, if waking { "Turning on…".into() } else { format!("Turn on {} (Wake-on-LAN from this computer)", name) }, !waking))
    }

    /// How wide the buttons at the right end of the tab row are, as icons (0: none): a VM's power
    /// actions and Delete, a connection's Disconnect, and a console's keyboard.
    fn tab_actions_width(&self, sel: &Node, current: Tab) -> f32 {
        // buttons, each `TOOL_BTN` and a gap
        let mut n = 0;
        if self.vm_of(sel).is_some() {
            n += 5; // power, Take Snapshot, Move, Copy, Delete
        }
        if self.power_on_of(sel).is_some() {
            n += 1; // Power On
        } else if self.host_of(sel).is_some() {
            n += 3; // Maintenance Mode, Restart, Shut Down
        }
        if current == Tab::Console && self.console.is_some() {
            n += 1; // the keyboard
        }
        let mut w = n as f32 * (TOOL_BTN.x + TOOL_GAP);
        if self.root_of(sel).is_some() {
            w += SQUARE.x + TOOL_GAP; // Disconnect (with its name when there's room)
        }
        if self.tabs_for(sel).contains(&Tab::Console) {
            w += CONSOLE_BTN + TOOL_GAP; // Console, before the separator
        }
        if w > 0.0 {
            w + SEP_W + 8.0
        } else {
            0.0
        }
    }

    /// At the right end of the tab row, after a separator, as buttons all the same size (see
    /// `paint_tool_btn`): a console's keyboard (its menu has Ctrl+Alt+Del and the other keys the
    /// desktop keeps), then a VM's power menu, Take Snapshot, Move, Copy (a menu) and Delete; a
    /// server's Maintenance Mode, Restart and Shut Down, or Power On; before the separator, a VM's
    /// or a server's Console, lit while it's shown (a click then goes back to the tab before).
    /// When the row hasn't the `need`ed room, all of it is in one Actions menu instead.
    fn tab_actions(&mut self, ui: &mut Ui, free: Rect, sel: &Node, current: Tab, need: f32, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let console_to = self.tabs_for(sel).contains(&Tab::Console).then(|| if current == Tab::Console { self.before_console } else { Tab::Console });
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
        // the power menu: its face (the VM's state, or a ring turning while an action runs), then
        // its entries (None: a line), the Force ones last
        let face = self.vm_of(sel).map(|(s, _, v)| match s.busy.get(&v.rref) {
            Some((_, op)) => Face::Busy(Tone::of_action(op)),
            None => Face::Tool(match v.power {
                PowerState::Running => "power-running",
                PowerState::Suspended => "power-suspended",
                PowerState::Paused => "power-paused",
                _ => "power-halted",
            }),
        });
        let state = self.vm_of(sel).map(|x| x.2.power.label()).unwrap_or_default();
        let mut entries: Vec<Option<(usize, bool)>> = actions.iter().map(|&(i, ok, _)| Some((i, ok))).collect();
        entries.push(None);
        entries.extend(actions.iter().filter_map(|&(_, _, force)| force).map(Some));
        let deleting = vm.as_ref().map_or(false, |(_, r)| self.vm_deleting.contains(r));
        // Delete waits while it's moving, copying, taking a snapshot, starting…
        let busy = self.vm_of(sel).and_then(|(s, _, v)| s.busy.get(&v.rref)).map(|b| busy_tip(&b.0));
        let snap_ok = self.vm_of(sel).map_or(false, |(s, d, v)| v.can_snapshot() && !s.busy.contains_key(&v.rref) && d.host_down_for(v).is_none());
        // Copy VM and Make Template: only while it's off
        let off = self.vm_of(sel).map_or(false, |(s, _, v)| v.power == PowerState::Halted && !s.busy.contains_key(&v.rref));
        // Move: running or off, nothing under way, its server there
        let move_ok = self.vm_of(sel).map_or(false, |(s, d, v)| matches!(v.power, PowerState::Running | PowerState::Halted) && !s.busy.contains_key(&v.rref) && d.host_down_for(v).is_none());
        let root = self.root_of(sel).map(|(sid, connected)| (sid, if connected { "Disconnect" } else { "Remove" }, self.server(sid).map(|s| s.name()).unwrap_or_default()));
        // a server: Power On while it's off (or its pool, through it, doesn't answer); else
        // Maintenance Mode, Restart and Shut Down, while it's on and nothing's under way on it
        let wake = self.power_on_of(sel);
        let host = self.host_of(sel).filter(|_| wake.is_none()).map(|(s, h)| {
            let idle = !self.host_busy.contains(&h.rref) && !self.host_rebooting.contains_key(&h.rref) && !self.host_maint.contains_key(&h.rref);
            (s.id, h.rref.clone(), h.is_up() && idle, h.in_maintenance())
        });
        let maint_tip = |on: bool, maint: bool| match (on, maint) {
            (false, _) => "It's off",
            (true, true) => "Take it out of maintenance mode…",
            (true, false) => "Maintenance mode: move its VMs off, take no new ones…",
        };
        let mut console = if current == Tab::Console { self.console.as_mut() } else { None };
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(free).layout(egui::Layout::right_to_left(egui::Align::Center)));
        let ui = &mut child;
        ui.spacing_mut().item_spacing.x = TOOL_GAP;
        // a connection's: Disconnect, with its name when there's room
        if let Some((sid, label, name)) = &root {
            let wide = free.width() >= need - SQUARE.x + flat_button_width(ui, label) + 4.0;
            let tip = format!("{} {}", if *label == "Remove" { "Remove" } else { "Disconnect from" }, name);
            if flat_button(ui, "disconnect", label, Some(pal.red), true, !wide, &pal).on_hover_text(tip).clicked() {
                out.push(Intent::Remove(*sid));
            }
            vsep(ui, &pal);
            return;
        }
        if free.width() >= need {
            // right to left: a server's Power On, or its Shut Down, Restart and Maintenance Mode;
            // a VM's Delete, Copy, Move, Take Snapshot and power menu; the keyboard; then the
            // separator and Console
            if let Some((sid, href, tip, ok)) = &wake {
                if tool_btn(ui, Face::Tool("power_on"), tip, *ok, &pal).clicked() {
                    out.push(Intent::PowerOn(*sid, href.clone()));
                }
            }
            if let Some((sid, href, on, maint)) = &host {
                if tool_btn(ui, Face::Tool("clean_shutdown"), if *on { "Shut down this server…" } else { "It's off" }, *on, &pal).clicked() {
                    out.push(Intent::AskShutdownHost(*sid, href.clone(), false));
                }
                if tool_btn(ui, Face::Tool("clean_reboot"), if *on { "Restart this server…" } else { "It's off" }, *on, &pal).clicked() {
                    out.push(Intent::AskShutdownHost(*sid, href.clone(), true));
                }
                if tool_btn(ui, Face::Glyph("wrench", pal.orange), maint_tip(*on, *maint), *on, &pal).clicked() {
                    out.push(Intent::AskMaintenance(*sid, href.clone()));
                }
            }
            if let Some((sid, vref)) = &vm {
                if tool_btn(ui, Face::Glyph("trash", pal.red), busy.as_deref().unwrap_or("Delete VM…"), !deleting && busy.is_none(), &pal).clicked() {
                    out.push(Intent::AskDeleteVm(*sid, vref.clone()));
                }
                copy_menu(ui, &pal, *sid, vref, off && !deleting, out);
                if tool_btn(ui, Face::Glyph("move", pal.accent), "Move to another server or storage…", move_ok && !deleting, &pal).clicked() {
                    out.push(Intent::MoveVm(*sid, vref.clone(), None));
                }
                if tool_btn(ui, Face::Glyph("camera", pal.accent), "Take Snapshot…", snap_ok && !deleting, &pal).clicked() {
                    out.push(Intent::TakeSnapshot(*sid, vref.clone()));
                }
                if let Some(face) = face {
                    power_menu(ui, &pal, face, state, &entries, out);
                }
            }
            if let Some(c) = console.as_deref_mut() {
                keys_button(ui, &pal, c);
            }
            vsep(ui, &pal);
            if let Some(to) = console_to {
                let on = current == Tab::Console;
                if console_button(ui, &pal, on).on_hover_text(if on { "Back to where you were" } else { "Its screen, to work in it" }).clicked() {
                    out.push(Intent::Tab(to));
                }
            }
            return;
        }
        // no room: one Actions menu with all of it, by name
        actions_menu(ui, &pal, |ui| {
            let mut line = false;
            if let Some(to) = console_to {
                let label = if current == Tab::Console { "Close Console" } else { "Console" };
                if ui.button(label).clicked() {
                    out.push(Intent::Tab(to));
                    ui.close_menu();
                }
                line = true;
            }
            if let Some(c) = console.as_deref_mut() {
                ui.add_enabled_ui(c.connected(), |ui| ui.menu_button("Send keys", |ui| c.keys_menu(ui)));
            }
            if let Some((sid, href, tip, ok)) = &wake {
                if line {
                    ui.separator();
                }
                if ui.add_enabled(*ok, egui::Button::new("Power On")).on_hover_text(tip.as_str()).clicked() {
                    out.push(Intent::PowerOn(*sid, href.clone()));
                    ui.close_menu();
                }
            }
            if let Some((sid, href, on, maint)) = &host {
                if line {
                    ui.separator();
                }
                let label = if *maint { "Exit Maintenance Mode…" } else { "Maintenance Mode…" };
                for (label, intent) in [
                    (label, Intent::AskMaintenance(*sid, href.clone())),
                    ("Restart…", Intent::AskShutdownHost(*sid, href.clone(), true)),
                    ("Shut Down…", Intent::AskShutdownHost(*sid, href.clone(), false)),
                ] {
                    if ui.add_enabled(*on, egui::Button::new(label)).on_disabled_hover_text("It's off, or busy").clicked() {
                        out.push(intent);
                        ui.close_menu();
                    }
                }
            }
            if !actions.is_empty() && line {
                ui.separator();
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
                if ui.add_enabled(move_ok && !deleting, egui::Button::new("Move…")).clicked() {
                    out.push(Intent::MoveVm(*sid, vref.clone(), None));
                    ui.close_menu();
                }
                if ui.add_enabled(snap_ok && !deleting, egui::Button::new("Take Snapshot…")).clicked() {
                    out.push(Intent::TakeSnapshot(*sid, vref.clone()));
                    ui.close_menu();
                }
                for (label, intent) in [("Copy VM…", Intent::CopyVm(*sid, vref.clone())), ("Make Template…", Intent::MakeTemplate(*sid, vref.clone()))] {
                    if ui.add_enabled(off && !deleting, egui::Button::new(label)).on_disabled_hover_text(SHUT_DOWN_FIRST).clicked() {
                        out.push(intent);
                        ui.close_menu();
                    }
                }
                let del = ui.add_enabled(!deleting && busy.is_none(), egui::Button::new(egui::RichText::new("Delete VM…").color(pal.red)));
                if del.on_disabled_hover_text(busy.as_deref().unwrap_or("")).clicked() {
                    out.push(Intent::AskDeleteVm(*sid, vref.clone()));
                    ui.close_menu();
                }
            }
        });
        vsep(ui, &pal);
    }

    /// The header and the tabs, on one line when they and the buttons at the row's right end fit
    /// (`room`: the width those want, and the least they can take, as one Actions menu),
    /// otherwise the tabs on a line of their own.
    fn header_and_tabs(&mut self, ui: &mut Ui, sel: &Node, tabs: &[Tab], current: Tab, room: (f32, f32), out: &mut Vec<Intent>) -> Rect {
        let (actions, least) = room;
        let pal = self.pal;
        let (title, sub) = self.header_text(sel);
        // a pool with something wrong: a dot on its Details, which says what
        let dot = (sel.kind == Kind::Pool && sel.sid.and_then(|id| self.server(id)).map_or(false, |s| !self.pool_issues(s).is_empty())).then_some(Tab::Details);
        let (title_font, sub_font, tab_font) = (FontId::new(16.0, FontFamily::Name("bold".into())), FontId::proportional(13.0), FontId::proportional(13.5));
        let width = |t: &str, f: &FontId| ui.painter().layout_no_wrap(t.to_string(), f.clone(), pal.ink).size().x;
        // a VM's name, with a pencil to rename it (or the box it's being renamed in)
        let rename = self.renamable(sel);
        let title_w = match &rename {
            Some((_, vm, _)) if self.ov.vm == *vm && self.ov.name.is_some() => rename_box_width(width(&title, &title_font)),
            Some(_) => width(&title, &title_font) + 28.0,
            None => width(&title, &title_font),
        };
        let head_w = 36.0 + title_w + if sub.is_empty() { 0.0 } else { 10.0 + width(&sub, &sub_font) };
        let tabs_w: f32 = tabs.iter().map(|t| width(t.label(), &tab_font) + 20.0).sum();
        let avail = ui.available_width();
        let hairline = |ui: &Ui, r: Rect| ui.painter().line_segment([r.left_bottom() - vec2(0.0, 0.5), r.right_bottom() - vec2(0.0, 0.5)], Stroke::new(1.0_f32, pal.border));
        // everything on one line, the details beside the name giving way first if need be
        let bare_w = 36.0 + title_w;
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
            self.paint_header(ui, row, sel, &title, &sub, row.center().y, rename, out);
            self.paint_tabs(ui, row, row.min.x + used + 28.0, tabs, current, dot, out)
        } else {
            // the actions go after the tabs when there's room; otherwise on the name's line (whose
            // details beside the name give way if need be), still as icons; failing both, they
            // fold into one button after the tabs
            // (all of them, or the one Actions menu they fold into, on the name's line when they
            // don't fit after the tabs)
            let strip_room = avail - 2.0 - tabs_w - 16.0;
            let head_room = avail - bare_w - 14.0;
            let on_head = actions > 0.0 && strip_room < actions && head_room >= least;
            let takes = if head_room >= actions { actions } else { least };
            let sub = if on_head && avail - head_w - 14.0 < takes { String::new() } else { sub };
            let used = if sub.is_empty() { bare_w } else { head_w };
            let (head, _) = ui.allocate_exact_size(vec2(avail, 38.0), Sense::hover());
            self.paint_header(ui, head, sel, &title, &sub, head.center().y + 2.0, rename, out);
            let (strip, _) = ui.allocate_exact_size(vec2(avail, 32.0), Sense::hover());
            hairline(ui, strip);
            // the tabs closer together if need be, to leave the buttons the least they take
            let keep = if on_head || actions <= 0.0 { 0.0 } else { least + 16.0 };
            let mut after_tabs = self.paint_tabs(ui, Rect::from_min_max(strip.min, pos2(strip.max.x - keep, strip.max.y)), strip.min.x + 2.0, tabs, current, dot, out);
            after_tabs.max.x = strip.max.x - 6.0;
            if on_head {
                Rect::from_min_max(pos2(head.min.x + used + 8.0, head.min.y + 4.0), pos2(head.max.x - 6.0, head.max.y))
            } else {
                after_tabs
            }
        }
    }

    /// The header's icon, title and details. A VM's name (`rename`) has a pencil after it: a click
    /// on either turns it into a box to type a new one in (Enter or a click elsewhere saves it,
    /// Esc puts it back); while a change is being saved, a spinner instead.
    #[allow(clippy::too_many_arguments)]
    fn paint_header(&mut self, ui: &mut Ui, r: Rect, sel: &Node, title: &str, sub: &str, mid: f32, rename: Option<(u32, String, bool)>, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let p = ui.painter().clone();
        let font = FontId::new(16.0, FontFamily::Name("bold".into()));
        paint_icon(&p, Rect::from_center_size(pos2(r.min.x + 20.0, mid), vec2(16.0, 16.0)), sel.icon, &pal, pal.window);
        let g = p.layout_no_wrap(title.to_string(), font.clone(), pal.ink);
        let mut w = g.size().x;
        let x = r.min.x + 36.0;
        match rename {
            Some((sid, vm, _)) if self.ov.vm == vm && self.ov.name.is_some() => {
                w = rename_box_width(w);
                let room = Rect::from_min_size(pos2(x - 7.0, mid - 15.0), vec2(w + 7.0, 30.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(room).layout(egui::Layout::left_to_right(egui::Align::Center)));
                let text = self.ov.name.as_mut().unwrap();
                let field = child.add(egui::TextEdit::singleline(text).font(font).desired_width(room.width() - 14.0).margin(vec2(6.0, 3.0)));
                if self.ov.focus {
                    field.request_focus();
                    // all of it picked, to type over
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), field.id) {
                        let end = egui::text::CCursor::new(text.chars().count());
                        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), end)));
                        state.store(ui.ctx(), field.id);
                    }
                    self.ov.focus = false;
                }
                if field.lost_focus() {
                    let typed = self.ov.name.take().unwrap_or_default();
                    let n = typed.trim().to_string();
                    if !ui.input(|i| i.key_pressed(Key::Escape)) && !n.is_empty() && n != title {
                        out.push(Intent::EditVm(sid, vm, VmChange::Name(n)));
                    }
                }
            }
            Some((_, vm, free)) => {
                p.galley(pos2(x, mid - g.size().y / 2.0), g, pal.ink);
                let pen = Rect::from_center_size(pos2(x + w + 15.0, mid), vec2(22.0, 22.0));
                if self.vm_saving.contains_key(&vm) {
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(pen.shrink(4.0)));
                    child.add(egui::Spinner::new().size(13.0));
                } else {
                    let hit = Rect::from_min_max(pos2(x, r.min.y + 6.0), pos2(pen.max.x, r.max.y - 6.0));
                    let resp = ui.interact(hit, ui.id().with("rename"), if free { Sense::click() } else { Sense::hover() });
                    let hot = free && resp.hovered();
                    if hot {
                        p.rect_filled(pen, Rounding::same(5.0), pal.hover);
                    }
                    paint_glyph(&p, Rect::from_center_size(pen.center(), vec2(14.0, 14.0)), "edit", if hot { pal.ink } else if free { pal.ink_soft } else { pal.ink_faint });
                    let resp = resp.on_hover_text(if free { "Rename" } else { "It can't be renamed while something's being done to it" });
                    if free && resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        if self.ov.vm != vm {
                            self.ov = OverviewEdit { vm: vm.clone(), ..Default::default() };
                        }
                        (self.ov.name, self.ov.focus) = (Some(title.to_string()), true);
                    }
                }
                w += 28.0;
            }
            None => p.galley(pos2(x, mid - g.size().y / 2.0), g, pal.ink),
        }
        p.text(pos2(x + 10.0 + w, mid + 1.0), Align2::LEFT_CENTER, sub, FontId::proportional(13.0), pal.ink_soft);
    }

    /// The VM whose name the header shows, to rename it there: its server, it, and whether it can
    /// be now (not while something's being done to it, or its server isn't there).
    fn renamable(&self, sel: &Node) -> Option<(u32, String, bool)> {
        if sel.kind != Kind::Vm {
            return None;
        }
        let s = self.server(sel.sid?)?;
        let v = s.data.as_ref()?.vm(sel.rref.as_deref()?)?;
        let down = s.data.as_ref()?.host_down_for(v).is_some();
        Some((s.id, v.rref.clone(), !s.busy.contains_key(&v.rref) && !self.vm_saving.contains_key(&v.rref) && !down))
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
                let what = match sel.rref.as_deref() {
                    Some("nets") => "Virtual Networks",
                    Some("tpls") => "Templates",
                    _ => "Shared Storage",
                };
                (what.into(), join(vec![Some(format!("in {}", s.name())), recon]))
            }
            (Kind::Vm, Some((_, d))) => {
                let v = sel.rref.as_deref().and_then(|r| d.vm(r));
                let on = v.and_then(|v| v.host.as_deref()).and_then(|h| d.host(h)).map(|h| format!("on {}", h.name));
                // (a power action under way shows as the icon turning)
                (v.map_or_else(|| sel.text.clone(), |v| v.name.clone()), join(vec![on, recon]))
            }
            (Kind::Template, Some((_, d))) => {
                let t = sel.rref.as_deref().and_then(|r| d.user_template(r));
                (t.map_or_else(|| sel.text.clone(), |t| t.name.clone()), join(vec![Some("template".into()), t.map(|t| t.os.clone()), recon]))
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
            Kind::Root => vec![Tab::Home, Tab::IsoLibrary, Tab::VmStorage, Tab::Templates, Tab::Networks, Tab::Search, Tab::Logs, Tab::Alerts],
            Kind::Category => vec![Tab::Home, Tab::Search, Tab::Logs],
            Kind::Sr if self.sr_of(sel).map_or(false, |(_, x)| x.is_iso()) => vec![Tab::General, Tab::IsoFiles, Tab::Logs],
            Kind::Folder if sel.rref.as_deref() == Some("nets") => vec![Tab::Networking],
            Kind::Folder if sel.rref.as_deref() == Some("tpls") => vec![Tab::Templates],
            Kind::Template => vec![Tab::General, Tab::Logs],
            Kind::Folder => vec![Tab::Storage],
            _ if !has_data => vec![Tab::General, Tab::Logs],
            Kind::Vm => vec![Tab::Overview, Tab::Performance, Tab::Snapshots, Tab::Console, Tab::Logs, Tab::Details],
            Kind::Host => vec![Tab::Overview, Tab::Performance, Tab::Storage, Tab::Networking, Tab::Console, Tab::Logs, Tab::Details],
            Kind::Pool => vec![Tab::Overview, Tab::Storage, Tab::Networking, Tab::Logs, Tab::Details],
            _ => vec![Tab::General, Tab::Logs],
        }
    }

    /// The tabs as text from `x` along `row`, the current one underlined in the accent colour;
    /// returns the room left on their right.
    fn paint_tabs(&self, ui: &mut Ui, row: Rect, x: f32, tabs: &[Tab], current: Tab, dot: Option<Tab>, out: &mut Vec<Intent>) -> Rect {
        let pal = self.pal;
        // closer together when they don't all fit (a narrow window), then smaller
        let room = row.max.x - x - 4.0;
        let n = tabs.len().max(1) as f32;
        let words = |size: f32| -> f32 { tabs.iter().map(|t| ui.painter().layout_no_wrap(t.label().into(), FontId::proportional(size), pal.ink).size().x).sum() };
        let size = if words(13.5) + 10.0 * n <= room { 13.5 } else { 12.5 };
        let pad = ((room - words(size)) / n).clamp(8.0, 20.0);
        let font = FontId::proportional(size);
        let mut x = x;
        for t in tabs {
            let tw = ui.painter().layout_no_wrap(t.label().into(), font.clone(), pal.ink).size().x;
            let r = Rect::from_min_size(pos2(x, row.min.y), vec2(tw + pad, row.height()));
            let resp = ui.interact(r, ui.id().with(t.label()), Sense::click());
            let on = *t == current;
            let col = if on || resp.hovered() { pal.ink } else { pal.ink_soft };
            ui.painter().text(pos2(r.center().x, r.center().y - 1.0), Align2::CENTER_CENTER, t.label(), font.clone(), col);
            if dot == Some(*t) {
                // something there needs attention
                ui.painter().circle_filled(pos2(r.center().x + tw / 2.0 + 5.0, r.center().y - 7.0), 3.5, pal.orange);
            }
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
                p.rect(r, Rounding::same(10.0), pal.surface, Stroke::new(1.0_f32, pal.border));
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
            ui.painter().rect(list, Rounding::same(10.0), pal.surface, Stroke::new(1.0_f32, pal.border));
            let mut y = list.min.y;
            let mut divider = false;
            for (s, h) in &hosts {
                let r = Rect::from_min_size(pos2(list.min.x, y), vec2(list.width(), row_h));
                let resp = ui.interact(r, ui.id().with(("home-host", s.id, h.rref.as_str())), Sense::click());
                let p = ui.painter();
                if divider {
                    p.line_segment([pos2(r.min.x + 14.0, r.min.y), pos2(r.max.x - 14.0, r.min.y)], Stroke::new(1.0_f32, pal.border));
                }
                divider = true;
                if resp.hovered() {
                    p.rect_filled(r.shrink(2.0), Rounding::same(8.0), pal.hover);
                }
                let icon = self.host_icon(h);
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
                    p.line_segment([pos2(r.min.x + 14.0, r.min.y), pos2(r.max.x - 14.0, r.min.y)], Stroke::new(1.0_f32, pal.border));
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
            p.rect(list, Rounding::same(10.0), pal.surface, Stroke::new(1.0_f32, pal.border));
            if recent.is_empty() {
                p.text(pos2(list.min.x + 16.0, list.center().y), Align2::LEFT_CENTER, "Nothing yet", FontId::proportional(13.0), pal.ink_soft);
            }
            for (i, e) in recent.iter().enumerate() {
                let r = Rect::from_min_size(pos2(list.min.x, list.min.y + i as f32 * 36.0), vec2(list.width(), 36.0));
                if i > 0 {
                    p.line_segment([pos2(r.min.x + 14.0, r.min.y), pos2(r.max.x - 14.0, r.min.y)], Stroke::new(1.0_f32, pal.border));
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
                let issues = self.pool_issues(s);
                if !issues.is_empty() {
                    secs.push(("Needs attention".into(), issues));
                }
                let (n, up) = (d.hosts.len(), d.hosts.iter().filter(|h| h.is_up()).count());
                let state = match up {
                    k if k == n => if n == 1 { "On".to_string() } else { format!("On: all {} servers", n) },
                    0 => "No server is on".to_string(),
                    k => format!("{} of {} servers on", k, n),
                };
                let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0);
                let on: Vec<&HostInfo> = d.hosts.iter().filter(|h| h.is_up()).collect();
                secs.push((
                    "General".into(),
                    vec![
                        row("Name", s.name()),
                        row("State", state),
                        row("Pool coordinator", master.map(|h| format!("{}  ({})", h.name, h.address)).unwrap_or_default()),
                        row("Version", d.versions().join(", ")),
                        row("License", d.license(now).map_or("None needed".to_string(), |l| l.0)),
                        row("Servers", n.to_string()),
                        row("Virtual machines", format!("{} ({} running)", d.vms.len(), running)),
                        row("Default storage", match d.default_sr.as_deref() {
                            None => "None".to_string(),
                            Some(r) => d.sr(r).map_or("Storage that no longer exists".to_string(), |x| x.name.clone()),
                        }),
                        row("CPUs", format!("{} in all", d.hosts.iter().map(|h| h.cpu_count).sum::<i64>())),
                        row("Memory", format!("{} in all, {} free", fmt_bytes(d.hosts.iter().map(|h| h.mem_total).sum()), fmt_bytes(on.iter().map(|h| h.mem_free).sum()))),
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
                            row(
                                "Status",
                                if !h.live {
                                    "Off".into()
                                } else if h.unreachable {
                                    "Not answering (the pool takes ten minutes to notice)".into()
                                } else {
                                    "On".into()
                                },
                            ),
                            row("Enabled", if h.enabled { "Yes".into() } else if h.in_maintenance() { "No (maintenance mode)".into() } else { "No".into() }),
                            row("Power on", match h.power_on_mode.as_str() {
                                "" => "Not set up (Power On sets Wake-on-LAN)".into(),
                                "wake-on-lan" => "Wake-on-LAN".into(),
                                m => m.to_string(),
                            }),
                            row("Version", h.version.clone()),
                            row("Running VMs", running.to_string()),
                            row("UUID", h.uuid.clone()),
                        ],
                    ));
                    secs.push(("CPUs".into(), vec![row("Count", h.cpu_count.to_string()), row("Model", h.cpu_model.clone())]));
                    secs.push(("Memory".into(), vec![row("Total", fmt_bytes(h.mem_total)), row("Free", fmt_bytes(h.mem_free))]));
                    // the VMs set to start when it does: those that belong with it, and on the
                    // coordinator those that can start on any server
                    let mut auto: Vec<&VmInfo> = d.vms.iter().filter(|v| v.auto_power_on && (v.home() == Some(h.rref.as_str()) || (v.home().is_none() && h.is_master))).collect();
                    auto.sort_by_key(|v| v.name.to_lowercase());
                    let starts = if auto.is_empty() {
                        vec![row("None", "A VM starts with its server when that's switched on in its Overview, under Startup".into())]
                    } else {
                        auto.iter().map(|v| row(&v.name, format!("{}{}", v.power.label(), if v.home().is_none() { "  ·  on any server in the pool" } else { "" }))).collect()
                    };
                    secs.push(("Starts with this server".into(), starts));
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

                    let state = match (s.busy.get(&v.rref), d.host_down_for(v)) {
                        (Some(b), _) => b.0.clone(),
                        (None, Some(h)) => format!("Unknown: {} {} (it was {} there)", h.name, if h.live { "isn't answering" } else { "is off" }, v.power.label().to_lowercase()),
                        (None, None) => v.power.label().into(),
                    };
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

                    // each disk and DVD drive, at its position, and each network card
                    let mut dev = vec![];
                    for k in &v.disks {
                        let what = if k.kind == "Disk" {
                            format!("{}  ·  {}  ·  {}", or(&k.name, "Disk"), fmt_bytes(k.size), or(&k.sr_name, "unknown storage"))
                        } else if k.name.is_empty() || k.name == "(empty)" {
                            "Empty".to_string()
                        } else {
                            format!("{}  ·  {}", k.name, k.sr_name)
                        };
                        dev.push(row(&format!("{} {}", if k.kind == "Disk" { "Disk" } else { "DVD drive" }, k.device), what));
                    }
                    for n in &v.nics {
                        dev.push(row(&format!("Network card {}", n.device), format!("{}  ·  {}{}", or(&n.network, "no network"), n.mac, if n.ip.is_empty() { String::new() } else { format!("  ·  {}", n.ip) })));
                    }
                    dev.push(row("Snapshots", v.snapshots.len().to_string()));
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
                    if !x.device.is_empty() {
                        g.push(row("Device", x.device.clone()));
                    }
                    if !x.share.is_empty() {
                        g.push(row("Connection", x.share.clone()));
                    }
                    if x.is_iso() {
                        g.push(row("ISO files", x.files.len().to_string()));
                    }
                    g.push(row("Shared", if x.shared { "Yes".into() } else { "No".into() }));
                    if x.kind != "udev" {
                        g.push(row("Status", sr_status(d, x)));
                    }
                    g.push(row("UUID", x.uuid.clone()));
                    secs.push(("General".into(), g));
                    let pct = if x.size > 0 { format!(" ({}%)", 100 * x.used / x.size) } else { String::new() };
                    secs.push(("Size".into(), vec![row("Size", fmt_bytes(x.size)), row("Used", format!("{}{}", fmt_bytes(x.used), pct)), row("Free", fmt_bytes((x.size - x.used).max(0)))]));
                }
            }
            Kind::Template => {
                if let Some(t) = d.user_template(r) {
                    let mut g = vec![row("Name", t.name.clone())];
                    if !t.description.is_empty() {
                        g.push(row("Description", t.description.clone()));
                    }
                    g.push(row("Operating system", or(&t.os, "Unknown").into()));
                    g.push(row("UUID", t.uuid.clone()));
                    secs.push(("General".into(), g));
                    let memory = if t.mem_dyn_min > 0 && t.mem_dyn_min < t.mem_dyn_max { format!("{} – {} (dynamic)", fmt_bytes(t.mem_dyn_min), fmt_bytes(t.mem_dyn_max)) } else { fmt_bytes(t.mem_dyn_max) };
                    secs.push(("CPU and memory".into(), vec![row("Virtual CPUs", t.vcpus.to_string()), row("Memory", memory)]));
                    let mut dev = vec![];
                    for k in t.disks.iter().filter(|k| k.kind == "Disk") {
                        dev.push(row(&format!("Disk {}", k.device), format!("{}  ·  on {}", fmt_bytes(k.size), or(&k.sr_name, "unknown storage"))));
                    }
                    let mut nets: Vec<String> = t.nics.iter().map(|n| n.network.clone()).filter(|n| !n.is_empty()).collect();
                    nets.dedup();
                    dev.push(row("Network interfaces", if nets.is_empty() { t.nics.len().to_string() } else { format!("{}  ·  {}", t.nics.len(), nets.join(", ")) }));
                    let mut boot = vec![];
                    if !t.firmware.is_empty() {
                        boot.push(row("Firmware", t.firmware.clone()));
                    }
                    if !t.boot_order.is_empty() {
                        boot.push(row("Boot order", t.boot_order.clone()));
                    }
                    secs.push(("Devices".into(), dev));
                    if !boot.is_empty() {
                        secs.push(("Boot".into(), boot));
                    }
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
    /// Its sections as cards, one under the other, in the middle 80% of the page (all of it in a
    /// narrow window).
    fn general(&self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        const HEAD: f32 = 38.0;
        const ROW: f32 = 25.0;
        let pal = self.pal;
        let secs = self.general_sections(sel);
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            let full = ui.available_width();
            let (pad, gap) = (16.0, 12.0);
            let room = full - 2.0 * pad;
            let card_w = (full * 0.8).max(room.min(560.0)).min(room);
            let origin = ui.cursor().min + vec2((full - card_w) / 2.0, 14.0);
            let mut total = 0.0;
            let mut cards = vec![];
            for (name, rows) in &secs {
                let open = self.section_open(name);
                let h = HEAD + if open { rows.len() as f32 * ROW + 10.0 } else { 0.0 };
                cards.push((Rect::from_min_size(origin + vec2(0.0, total), vec2(card_w, h)), name, rows, open));
                total += h + gap;
            }
            ui.allocate_exact_size(vec2(full, total + 24.0), Sense::hover());
            // one label column for every card (so the values line up), as wide as the widest label
            let key_font = FontId::proportional(12.5);
            let widest = secs.iter().flat_map(|(_, rows)| rows.iter()).map(|(k, _)| ui.painter().layout_no_wrap(k.clone(), key_font.clone(), pal.ink).size().x).fold(0.0, f32::max);
            let label_w = (widest + 28.0).clamp(100.0, (card_w * 0.42).clamp(100.0, 220.0));
        let title_font = FontId::new(13.5, FontFamily::Name("bold".into()));
            for (r, name, rows, open) in cards {
                let p = ui.painter().clone();
                p.rect(r, Rounding::same(8.0), pal.surface, Stroke::new(1.0_f32, pal.border));
                let head = Rect::from_min_size(r.min, vec2(r.width(), HEAD));
                let hresp = ui.interact(head, ui.id().with(("sec", name.as_str())), Sense::click());
                if hresp.hovered() {
                    let round = if open { Rounding { nw: 8.0, ne: 8.0, sw: 0.0, se: 0.0 } } else { Rounding::same(8.0) };
                    p.rect_filled(head.shrink(1.0), round, pal.hover);
                }
                // what needs attention: its title with the dot its tab has
                let t = p.text(pos2(head.min.x + 16.0, head.center().y), Align2::LEFT_CENTER, name, title_font.clone(), pal.ink);
                if name == "Needs attention" {
                    p.circle_filled(pos2(t.max.x + 9.0, t.center().y), 3.5, pal.orange);
                }
                let c = pos2(head.max.x - 20.0, head.center().y);
                let chev = if open { vec![c + vec2(-4.0, -2.0), c + vec2(0.0, 2.0), c + vec2(4.0, -2.0)] } else { vec![c + vec2(-2.0, -4.0), c + vec2(2.0, 0.0), c + vec2(-2.0, 4.0)] };
                p.add(egui::Shape::line(chev, Stroke::new(1.4_f32, pal.ink_soft)));
                if hresp.clicked() {
                    out.push(Intent::Section(name.clone()));
                }
                if !open {
                    continue;
                }
                p.line_segment([pos2(r.min.x + 16.0, head.max.y), pos2(r.max.x - 16.0, head.max.y)], Stroke::new(1.0_f32, pal.border));
                let mut y = head.max.y + 5.0;
                for (k, v) in rows {
                    let rr = Rect::from_min_size(pos2(r.min.x, y), vec2(r.width(), ROW));
                    // (a long label, such as a VM's name, cut short)
                    let mut kjob = egui::text::LayoutJob::simple_singleline(k.clone(), key_font.clone(), pal.ink_soft);
                    kjob.wrap = egui::text::TextWrapping { max_width: label_w - 10.0, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                    let kg = p.layout_job(kjob);
                    p.galley(pos2(r.min.x + 16.0, rr.center().y - kg.size().y / 2.0), kg, pal.ink_soft);
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

    /// A table: muted headings over a hairline, rows that light up under the mouse, cells cut short
    /// with "…"; in a wide window the columns spread out over all of it. Returns the row clicked,
    /// when `clickable`.
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
                let w = match c.plain() {
                    Cell::By(..) => 0.0,
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
                // Room to spare (a wide window): the columns with words in them share it, each in
                // proportion to its width, as a web page's table does; a usage bar's column and a
                // column of buttons (no heading) keep theirs.
                let flex: Vec<usize> = (0..widths.len()).filter(|&i| !headers.get(i).map_or(true, |h| h.is_empty()) && !rows.iter().any(|r| matches!(r.get(i).map(Cell::plain), Some(Cell::Usage(..))))).collect();
                let base: f32 = flex.iter().map(|&i| widths[i]).sum();
                if base > 0.0 {
                    let grow = (avail - sum) / base;
                    for &i in &flex {
                        widths[i] += widths[i] * grow;
                    }
                } else if let Some(last) = widths.last_mut() {
                    *last += avail - sum;
                }
            } else {
                // Too wide (a narrow window): cut the widest columns short, their text ending in "…",
                // but not below 120 points, and never a usage bar's; only past that does the table
                // scroll sideways.
                let least = 120.0 + 2.0 * pad;
                let fixed: Vec<bool> = (0..widths.len()).map(|i| rows.iter().any(|r| matches!(r.get(i).map(Cell::plain), Some(Cell::Usage(..))))).collect();
                let mut over = sum - avail;
                while over > 0.5 {
                    let Some(wi) = (0..widths.len()).filter(|&i| !fixed[i] && widths[i] > least).max_by(|&a, &b| widths[a].total_cmp(&widths[b])) else { break };
                    let next = (0..widths.len()).filter(|&i| widths[i] < widths[wi]).map(|i| widths[i]).fold(least, f32::max);
                    let cut = over.min(widths[wi] - least).min((widths[wi] - next).max(1.0));
                    widths[wi] -= cut;
                    over -= cut;
                }
            }
            // widths the user set by dragging a header's edge (for this table, this session)
            let set_id = egui::Id::new(("column-widths", id));
            let mut set: Vec<Option<f32>> = ui.data(|d| d.get_temp(set_id)).unwrap_or_default();
            set.resize(widths.len(), None);
            for (w, s) in widths.iter_mut().zip(&set) {
                if let Some(s) = s {
                    *w = *s;
                }
            }
            let sum: f32 = widths.iter().sum();
            if sum < avail && set.iter().any(|s| s.is_some()) {
                if let Some(last) = widths.last_mut() {
                    *last += avail - sum;
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
            // a click on a heading sorts by its column: up, then down, then as it came
            let sort_id = egui::Id::new(("table-sort", id));
            let mut sort: Option<(usize, bool)> = ui.data(|d| d.get_temp(sort_id)).flatten();
            for (i, h) in headers.iter().enumerate() {
                let sortable = !h.is_empty() && rows.iter().any(|r| r.get(i).and_then(Cell::key).is_some());
                let cell = Rect::from_min_size(pos2(xs[i] + 4.0, area.min.y), vec2((widths[i] - 8.0).max(1.0), head_h));
                let resp = if sortable { Some(ui.interact(cell, ui.id().with((id, "head", i)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand)) } else { None };
                let hot = resp.as_ref().map_or(false, |r| r.hovered());
                let t = p.text(pos2(xs[i] + pad, area.min.y + head_h / 2.0), Align2::LEFT_CENTER, *h, head_font.clone(), if hot { pal.ink } else { pal.ink_soft });
                if let Some((_, up)) = sort.filter(|s| s.0 == i) {
                    let c = pos2(t.max.x + 9.0, t.center().y);
                    let pts = if up { vec![c + vec2(-3.5, 2.0), c + vec2(3.5, 2.0), c + vec2(0.0, -2.5)] } else { vec![c + vec2(-3.5, -2.0), c + vec2(3.5, -2.0), c + vec2(0.0, 2.5)] };
                    p.add(egui::Shape::convex_polygon(pts, pal.accent, Stroke::NONE));
                }
                if resp.map_or(false, |r| r.clicked()) {
                    sort = match sort {
                        Some((c, true)) if c == i => Some((i, false)),
                        Some((c, false)) if c == i => None,
                        _ => Some((i, true)),
                    };
                    ui.data_mut(|d| d.insert_temp(sort_id, sort));
                }
            }
            // the rows in that order (those with nothing to sort by last, either way)
            let mut order: Vec<usize> = (0..rows.len()).collect();
            if let Some((col, up)) = sort {
                let keys: Vec<Option<SortKey>> = rows.iter().map(|r| r.get(col).and_then(Cell::key)).collect();
                order.sort_by(|&a, &b| match (&keys[a], &keys[b]) {
                    (Some(x), Some(y)) => {
                        let o = key_order(x, y);
                        if up { o } else { o.reverse() }
                    }
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                });
            }
            // each column's right edge in the header can be dragged to resize it (a double click
            // gives it back its own width)
            for i in 0..widths.len().saturating_sub(1) {
                let x = xs[i] + widths[i];
                let grip = Rect::from_min_max(pos2(x - 4.0, area.min.y + 4.0), pos2(x + 4.0, area.min.y + head_h - 4.0));
                let resp = ui.interact(grip, ui.id().with((id, "grip", i)), Sense::click_and_drag()).on_hover_cursor(egui::CursorIcon::ResizeColumn);
                if resp.hovered() || resp.dragged() {
                    p.line_segment([pos2(x, grip.min.y), pos2(x, grip.max.y)], Stroke::new(2.0_f32, pal.accent));
                } else {
                    p.line_segment([pos2(x, grip.min.y + 4.0), pos2(x, grip.max.y - 4.0)], Stroke::new(1.0_f32, pal.border));
                }
                // the width from where the drag began (the same however often a frame is laid out)
                let from_id = ui.id().with((id, "grip-from", i));
                if resp.drag_started() {
                    if let Some(at) = resp.interact_pointer_pos() {
                        ui.data_mut(|d| d.insert_temp(from_id, (widths[i], at.x)));
                    }
                }
                if resp.dragged() {
                    if let (Some((base, from)), Some(at)) = (ui.data(|d| d.get_temp::<(f32, f32)>(from_id)), resp.interact_pointer_pos()) {
                        set[i] = Some((base + at.x - from).max(48.0));
                        ui.data_mut(|d| d.insert_temp(set_id, set.clone()));
                    }
                }
                if resp.double_clicked() {
                    set[i] = None;
                    ui.data_mut(|d| d.insert_temp(set_id, set.clone()));
                }
            }
            p.line_segment([pos2(area.min.x, area.min.y + head_h - 0.5), pos2(area.max.x, area.min.y + head_h - 0.5)], Stroke::new(1.0_f32, pal.border));
            if rows.is_empty() {
                p.text(pos2(area.min.x + pad, area.min.y + head_h + row_h / 2.0), Align2::LEFT_CENTER, "Nothing to show", font.clone(), pal.ink_soft);
            }
            // (each row is known by where it is in `rows`, wherever it's shown)
            for (di, &ri) in order.iter().enumerate() {
                let r = &rows[ri];
                let rr = Rect::from_min_size(pos2(area.min.x, area.min.y + head_h + di as f32 * row_h), vec2(total_w, row_h));
                let resp = ui.interact(rr, ui.id().with((id, ri)), if clickable { Sense::click() } else { Sense::hover() });
                // (the pointer may be on a button in the row, which then counts as hovered instead)
                let hot = resp.hovered() || ui.rect_contains_pointer(rr);
                if hot {
                    p.rect_filled(rr.shrink2(vec2(2.0, 2.0)), Rounding::same(6.0), pal.hover);
                } else if di > 0 {
                    p.line_segment([pos2(rr.min.x + pad, rr.min.y), pos2(rr.max.x - pad, rr.min.y)], Stroke::new(1.0_f32, pal.border.gamma_multiply(0.6)));
                }
                if resp.clicked() {
                    clicked = Some(ri);
                }
                for (i, c) in r.iter().enumerate().take(widths.len()) {
                    let mut x = xs[i] + pad;
                    let room = widths[i] - 2.0 * pad;
                    let cy = rr.center().y;
                    let text = match c.plain() {
                        Cell::By(..) => continue,
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
        let rows = files.into_iter().map(|file| IsoRow { sid, sr: sr.clone(), lib: None, file }).collect();
        self.iso_table(ui, "isos", rows, out);
    }

    /// ISO files in a table: each one's name, (in the list of every library) its library, size,
    /// description with a pencil to edit it there and then, and at the end New VM and the bin.
    fn iso_table(&mut self, ui: &mut Ui, id: &str, files: Vec<IsoRow>, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let with_lib = files.iter().any(|f| f.lib.is_some());
        let font = FontId::proportional(13.0);
        let measure = |t: &str| ui.painter().layout_no_wrap(t.to_string(), font.clone(), pal.ink).size().x;
        let rows: Vec<Vec<Cell>> = files
            .iter()
            .map(|r| {
                let f = &r.file;
                let editing = self.iso_edit.as_ref().map_or(false, |e| e.vdi == f.vdi);
                let desc_w = if editing { 240.0 } else { (measure(&f.description) + 34.0).min(380.0) };
                let mut cells = vec![Cell::Icon(IconKind::IsoSr, f.name.clone())];
                if let Some(lib) = &r.lib {
                    cells.push(Cell::by_text(lib, Cell::Custom(measure(lib).min(260.0))));
                }
                let desc = if f.description.trim().is_empty() { Cell::Custom(desc_w) } else { Cell::by_text(&f.description, Cell::Custom(desc_w)) };
                cells.extend([Cell::Text(fmt_bytes(f.size)), desc, Cell::Custom(56.0)]);
                cells
            })
            .collect();
        let headers: &[&str] = if with_lib { &["Name", "Library", "Size", "Description", ""] } else { &["Name", "Size", "Description", ""] };
        let (lib_col, end_col) = if with_lib { (Some(1), 4) } else { (None, 3) };
        let mut edit = self.iso_edit.take();
        let (mut close, mut save, mut delete, mut new_from, mut go) = (false, None, None, None, None);
        let (saving, deleting) = (&self.iso_saving, &self.iso_deleting);
        self.table_with(ui, id, headers, &rows, false, &mut |ui, row, col, r, hot| {
            let IsoRow { sid, sr, lib, file: f } = &files[row];
            let gone = deleting.contains(&f.vdi);
            if Some(col) == lib_col {
                // the library, a link to its page
                let resp = ui.interact(r, ui.id().with(("lib", row)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Open this library");
                let text = lib.clone().unwrap_or_default();
                let mut job = egui::text::LayoutJob::simple_singleline(text, font.clone(), if resp.hovered() { pal.accent } else { pal.ink });
                job.wrap = egui::text::TextWrapping { max_width: r.width().max(10.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                let g = ui.painter().layout_job(job);
                ui.painter().galley(pos2(r.min.x, r.center().y - g.size().y / 2.0), g, pal.ink);
                if resp.clicked() {
                    go = Some(format!("sr:{}:{}", sid, sr));
                }
                return;
            }
            if col == end_col {
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
                    new_from = Some((*sid, f.vdi.clone()));
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
                    delete = Some((*sid, sr.clone(), f.vdi.clone()));
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
                        save = Some((*sid, sr.clone(), f.vdi.clone(), f.name.clone(), text));
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
        if let Some((sid, sr, vdi, name, text)) = save {
            out.push(Intent::SaveDescription(sid, sr, vdi, name, text));
        }
        if let Some((sid, sr, vdi)) = delete {
            out.push(Intent::AskDeleteIso(sid, sr, vdi));
        }
        if let Some((sid, vdi)) = new_from {
            out.push(Intent::NewVm(Some(sid), Some(vdi)));
        }
        if let Some(key) = go {
            out.push(Intent::Select(key));
        }
    }

    /// Where a file dropped on the window goes: the ISO library selected, or on the ISO Library
    /// page the only library there is (pool id, library, its name).
    fn drop_library(&self, sel: &Node) -> Option<(u32, String, String)> {
        if let Some((s, x)) = self.sr_of(sel).filter(|(_, x)| x.is_iso()) {
            return Some((s.id, x.rref.clone(), x.name.clone()));
        }
        if sel.kind != Kind::Root || self.tab != Tab::IsoLibrary {
            return None;
        }
        match self.iso_libraries().as_slice() {
            [(sid, _, x)] => Some((*sid, x.rref.clone(), x.name.clone())),
            _ => None,
        }
    }

    /// Every connected pool's ISO libraries: (pool id, pool name, library), the pools in their
    /// order, each one's libraries by name.
    fn iso_libraries(&self) -> Vec<(u32, String, SrInfo)> {
        let mut out = vec![];
        for s in &self.servers {
            let Some(d) = &s.data else { continue };
            let mut libs: Vec<&SrInfo> = d.srs.iter().filter(|x| x.is_iso()).collect();
            libs.sort_by_key(|x| x.name.to_lowercase());
            out.extend(libs.into_iter().map(|x| (s.id, s.name(), x.clone())));
        }
        out
    }

    /// All servers' ISO Library page: every pool's ISO libraries, where each one is and how it's
    /// doing (a click opens it), then every ISO file in them, by name, with its library. New ISO
    /// Library, Upload and Rescan are next to the tabs.
    fn iso_library_page(&mut self, ui: &mut Ui, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let libs = self.iso_libraries();
        let pools = libs.iter().map(|l| l.0).collect::<HashSet<_>>().len();
        ui.add_space(10.0);
        if libs.is_empty() {
            ui.label(egui::RichText::new("No ISO library yet").color(pal.ink_soft));
            let text = "An ISO library is a shared folder with installation discs (.iso files), on a NAS or a Windows computer: your servers mount it to install new VMs from. Add one with New ISO Library.";
            ui.add(egui::Label::new(egui::RichText::new(text).size(12.0).color(pal.ink_faint)).wrap());
            return;
        }
        let count: usize = libs.iter().map(|l| l.2.files.len()).sum();
        let total: i64 = libs.iter().flat_map(|l| l.2.files.iter()).map(|f| f.size).sum();
        let head = format!(
            "{} ISO file{}  ·  {}  ·  in {} librar{}",
            count,
            if count == 1 { "" } else { "s" },
            fmt_bytes(total),
            libs.len(),
            if libs.len() == 1 { "y" } else { "ies" }
        );
        ui.label(egui::RichText::new(head).color(pal.ink_soft));
        ui.label(egui::RichText::new("If your file isn't in the list below, rescan its library.").size(12.0).color(pal.ink_faint));
        for (_, _, x) in &libs {
            if let Some(u) = self.uploads.get(&x.rref) {
                ui.add_space(8.0);
                self.upload_card(ui, &x.rref, u, out);
            }
        }
        ui.add_space(12.0);
        self.library_cards(ui, &libs, pools > 1, out);
        ui.add_space(8.0);
        // each file's library, when there's more than the one on the card above
        let several = libs.len() > 1;
        let mut rows: Vec<IsoRow> = libs
            .iter()
            .flat_map(|(sid, pool, x)| {
                let lib = if pools > 1 { format!("{}  ·  {}", x.name, pool) } else { x.name.clone() };
                x.files.iter().map(move |f| IsoRow { sid: *sid, sr: x.rref.clone(), lib: several.then(|| lib.clone()), file: f.clone() })
            })
            .collect();
        rows.sort_by_key(|r| r.file.name.to_lowercase());
        self.iso_table(ui, "all-isos", rows, out);
    }

    /// Next to the ISO Library page's tabs: Upload (into which library, when there are several),
    /// Rescan (every library) and New ISO Library.
    fn iso_library_actions(&self, ui: &mut Ui, free: Rect, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let libs = self.iso_libraries();
        // the ones that can take a file now: mounted, nothing under way
        let ready: Vec<&(u32, String, SrInfo)> = libs
            .iter()
            .filter(|(sid, _, x)| {
                let mounted = self.server(*sid).and_then(|s| s.data.as_ref()).map_or(false, |d| d.sr_trouble(x).is_none());
                mounted && !self.sr_busy.contains(&x.rref) && !self.uploads.get(&x.rref).map_or(false, Upload::active)
            })
            .collect();
        let busy = libs.iter().any(|l| self.sr_busy.contains(&l.2.rref));
        let pools = libs.iter().map(|l| l.0).collect::<HashSet<_>>().len();
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(free).layout(egui::Layout::right_to_left(egui::Align::Center)));
        child.spacing_mut().item_spacing.x = 4.0;
        let need = flat_button_width(&child, "New ISO Library") + flat_button_width(&child, "Working…") + flat_button_width(&child, "Upload") + 16.0 + SEP_W;
        let compact = free.width() < need;
        let has_pool = self.servers.iter().any(|s| s.data.is_some());
        if flat_button(&mut child, "plus", "New ISO Library", Some(pal.accent), has_pool, compact, &pal).on_hover_text("Add a shared folder with .iso files, from a Windows or NFS share").clicked() {
            out.push(Intent::NewIsoLibrary);
        }
        if !libs.is_empty() {
            let tip = "Look for ISO files added to or removed from every library";
            if flat_button(&mut child, "rescan", if busy { "Working…" } else { "Rescan" }, None, !busy && !ready.is_empty(), compact, &pal).on_hover_text(tip).clicked() {
                out.extend(ready.iter().map(|(sid, _, x)| Intent::RescanSr(*sid, x.rref.clone())));
            }
            let tip = if ready.is_empty() { "No library can take a file now" } else { "Add an .iso file from this computer or from a link on the web" };
            let up = flat_button(&mut child, "upload", "Upload", None, !ready.is_empty(), compact, &pal).on_hover_text(tip).on_disabled_hover_text(tip);
            let menu = child.id().with("upload-to");
            if up.clicked() {
                match ready.as_slice() {
                    [(sid, _, x)] => out.push(Intent::Upload(*sid, x.rref.clone())),
                    _ => child.memory_mut(|m| m.toggle_popup(menu)),
                }
            }
            // into which library
            egui::popup::popup_below_widget(&child, menu, &up, egui::PopupCloseBehavior::CloseOnClick, |ui| {
                ui.set_min_width(200.0);
                ui.label(egui::RichText::new("Upload to").size(12.0).color(pal.ink_soft));
                for (sid, pool, x) in &ready {
                    let label = if pools > 1 { format!("{}  ·  {}", x.name, pool) } else { x.name.clone() };
                    if ui.button(label).clicked() {
                        out.push(Intent::Upload(*sid, x.rref.clone()));
                    }
                }
            });
        }
        vsep(&mut child, &pal);
    }

    /// The libraries side by side, each a card: its icon (how it's doing), its name, where it is,
    /// and how many files it has (or that it isn't mounted); a click opens it.
    fn library_cards(&self, ui: &mut Ui, libs: &[(u32, String, SrInfo)], name_pool: bool, out: &mut Vec<Intent>) {
        let pal = self.pal;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(10.0, 10.0);
            let w = ((ui.available_width() - 20.0) / 3.0).clamp(220.0, 300.0).min(ui.available_width());
            for (sid, pool, x) in libs {
                let Some(d) = self.server(*sid).and_then(|s| s.data.as_ref()) else { continue };
                let (r, resp) = ui.allocate_exact_size(vec2(w, 58.0), Sense::click());
                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                let p = ui.painter();
                let bg = if resp.hovered() { pal.hover } else { pal.surface };
                p.rect(r, Rounding::same(10.0), bg, Stroke::new(1.0_f32, pal.border.gamma_multiply(0.7)));
                paint_icon(p, Rect::from_min_size(r.min + vec2(14.0, 13.0), vec2(16.0, 16.0)), sr_icon(d, x, self.sr_busy.contains(&x.rref)), &pal, bg);
                let trouble = d.sr_trouble(x).map(|t| if t.nowhere { "Not mounted" } else { "Not mounted everywhere" });
                let files = x.files.len();
                let (state, col) = match trouble {
                    Some(t) => (t.to_string(), pal.red),
                    None => (format!("{} file{}", files, if files == 1 { "" } else { "s" }), pal.ink_soft),
                };
                let st = p.text(pos2(r.max.x - 14.0, r.min.y + 21.0), Align2::RIGHT_CENTER, &state, FontId::proportional(12.5), col);
                let (g, _) = overview::fit(p, &x.name, FontId::new(14.0, FontFamily::Name("bold".into())), pal.ink, (st.min.x - r.min.x - 50.0).max(30.0));
                p.galley(pos2(r.min.x + 38.0, r.min.y + 21.0 - g.size().y / 2.0), g, pal.ink);
                let place = [x.location.as_str(), if name_pool { pool.as_str() } else { "" }].into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join("  ·  ");
                let place = if place.is_empty() { x.share.clone() } else { place };
                let (g, _) = overview::fit(p, &place, FontId::proportional(12.0), pal.ink_faint, r.width() - 52.0);
                p.galley(pos2(r.min.x + 38.0, r.min.y + 40.0 - g.size().y / 2.0), g, pal.ink_faint);
                let tip = [x.name.clone(), x.location.clone(), x.share.clone()].into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n");
                if resp.on_hover_text(tip).clicked() {
                    out.push(Intent::Select(format!("sr:{}:{}", sid, x.rref)));
                }
            }
        });
    }

    /// All servers' VM Storage page: every pool's storage for VMs' disks, where it is, its kind,
    /// how full it is, which one is the pool's default (Make Default on the others); a click opens
    /// one. A pool with no shared storage is told what that costs. New VM Storage is next to the
    /// tabs.
    fn vm_storage_page(&mut self, ui: &mut Ui, out: &mut Vec<Intent>) {
        let pal = self.pal;
        struct Store {
            sid: u32,
            pool: String,
            x: SrInfo,
            /// Where it is in words (its server and disk, or its share), and in full for its tooltip.
            place: String,
            full: String,
            default: bool,
            trouble: Option<&'static str>,
        }
        let mut stores: Vec<Store> = vec![];
        let mut unshared: Vec<String> = vec![];
        for s in &self.servers {
            let Some(d) = &s.data else { continue };
            let mut mine: Vec<Store> = d
                .srs
                .iter()
                .filter(|x| !x.is_iso() && x.kind != "udev")
                .map(|x| {
                    let server = x.host.as_deref().and_then(|h| d.host(h)).map_or(String::new(), |h| h.name.clone());
                    let (place, full) = if x.shared || x.host.is_none() {
                        (or(&x.location, "Every server").to_string(), [x.location.as_str(), x.share.as_str()].into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n"))
                    } else {
                        let disk = device_words(&x.device);
                        let place = [server.as_str(), disk.as_str()].into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join("  ·  ");
                        (place, [server.clone(), x.device.replace(',', "\n")].into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n"))
                    };
                    Store {
                        sid: s.id,
                        pool: s.name(),
                        place,
                        full,
                        default: d.default_sr.as_deref() == Some(x.rref.as_str()),
                        trouble: d.sr_trouble(x).map(|t| if t.nowhere { "Not mounted" } else { "Not mounted everywhere" }),
                        x: x.clone(),
                    }
                })
                .collect();
            // shared first, then each server's, by name
            mine.sort_by_key(|t| (!t.x.shared, t.place.to_lowercase(), t.x.name.to_lowercase()));
            if d.hosts.len() > 1 && !mine.iter().any(|t| t.x.shared) {
                unshared.push(s.name());
            }
            stores.extend(mine);
        }
        let pools = stores.iter().map(|t| t.sid).collect::<HashSet<_>>().len();
        ui.add_space(10.0);
        let (used, size) = stores.iter().filter(|t| t.trouble.is_none()).fold((0i64, 0i64), |(u, z), t| (u + t.x.used, z + t.x.size));
        let head = format!("{} storage{} for VMs' disks  ·  {} of {} used", stores.len(), if stores.len() == 1 { "" } else { "s" }, fmt_bytes(used), fmt_bytes(size));
        ui.label(egui::RichText::new(head).color(pal.ink_soft));
        let hint = match unshared.as_slice() {
            [] => "The default storage is where new VMs' disks go, and what suspending and moving VMs use.".to_string(),
            [one] if pools <= 1 => format!("'{}' has no shared storage: a VM moves to another server only by copying its disks. New VM Storage adds an NFS folder that all its servers share.", one),
            names => format!("{} have no shared storage: a VM moves to another server only by copying its disks. New VM Storage adds an NFS folder that all of a pool's servers share.", names.iter().map(|n| format!("'{}'", n)).collect::<Vec<_>>().join(", ")),
        };
        ui.add(egui::Label::new(egui::RichText::new(hint).size(12.0).color(pal.ink_faint)).wrap());
        ui.add_space(6.0);
        let font = FontId::proportional(13.0);
        let measure = |t: &str| ui.painter().layout_no_wrap(t.to_string(), font.clone(), pal.ink).size().x;
        let rows: Vec<Vec<Cell>> = stores
            .iter()
            .map(|t| {
                let usage = match t.trouble {
                    Some(why) => Cell::Text(why.to_string()),
                    None if t.x.size > 0 => Cell::Usage((t.x.used as f32 / t.x.size as f32).clamp(0.0, 1.0), format!("{} of {}", fmt_bytes(t.x.used), fmt_bytes(t.x.size))),
                    None => Cell::Text("—".into()),
                };
                let mut cells = vec![Cell::by_text(&t.x.name, Cell::Custom(24.0 + measure(&t.x.name)))];
                if pools > 1 {
                    cells.push(Cell::Text(t.pool.clone()));
                }
                // the last: Default on the pool's default, Make Default on the others under the mouse
                cells.extend([Cell::Text(sr_kind_words(&t.x)), Cell::by_text(&t.place, Cell::Custom(measure(&t.place).min(300.0))), usage, Cell::Custom(104.0)]);
                cells
            })
            .collect();
        let headers: &[&str] = if pools > 1 { &["Name", "Pool", "Type", "Where", "Used", ""] } else { &["Name", "Type", "Where", "Used", ""] };
        let end = headers.len() - 1;
        let place_col = if pools > 1 { 3 } else { 2 };
        let mut make: Option<(u32, String)> = None;
        let clicked = self.table_with(ui, "all-srs", headers, &rows, true, &mut |ui, row, col, r, hot| {
            let t = &stores[row];
            let Some(d) = self.server(t.sid).and_then(|s| s.data.as_ref()) else { return };
            if col == 0 {
                // its icon and its name
                paint_icon(ui.painter(), Rect::from_min_size(pos2(r.min.x, r.center().y - 8.0), vec2(16.0, 16.0)), sr_icon(d, &t.x, false), &pal, if hot { pal.hover } else { pal.window });
                let (g, _) = overview::fit(ui.painter(), &t.x.name, font.clone(), pal.ink, (r.width() - 24.0).max(20.0));
                ui.painter().galley(pos2(r.min.x + 24.0, r.center().y - g.size().y / 2.0), g, pal.ink);
            } else if col == place_col {
                // where it is, cut short to fit; all of it (the disk's whole path) under the mouse
                let (g, _) = overview::fit(ui.painter(), &t.place, font.clone(), pal.ink, r.width().max(20.0));
                let hit = Rect::from_min_size(pos2(r.min.x, r.center().y - g.size().y / 2.0), g.size());
                ui.painter().galley(hit.min, g, pal.ink);
                if !t.full.is_empty() {
                    ui.interact(hit, ui.id().with(("place", row)), Sense::hover()).on_hover_text(&t.full);
                }
            } else if col == end && t.default {
                let tip = "The pool's default storage: new VMs' disks, suspended VMs' memory and crash dumps go here";
                let w = host_overview::pill(ui.painter(), &pal, pos2(r.min.x, r.center().y), "Default", pal.green);
                ui.interact(Rect::from_min_size(pos2(r.min.x, r.center().y - 9.0), vec2(w, 18.0)), ui.id().with(("default", row)), Sense::hover()).on_hover_text(tip);
            } else if col == end && t.trouble.is_none() && t.x.size > 0 {
                // Make Default: faint till the row's under the mouse
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r.expand2(vec2(8.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
                let tip = "Make this the pool's default storage: for new VMs' disks, suspended VMs' memory and crash dumps (moving a running VM's disks uses it too)";
                if hot && flat_button(&mut child, "check", "Make Default", Some(pal.green), true, false, &pal).on_hover_text(tip).clicked() {
                    make = Some((t.sid, t.x.rref.clone()));
                }
            }
        });
        if let Some((sid, sr)) = make {
            out.push(Intent::MakeDefaultSr(sid, sr));
        } else if let Some(i) = clicked {
            out.push(Intent::Select(format!("sr:{}:{}", stores[i].sid, stores[i].x.rref)));
        }
    }

    /// The Snapshots tab: a VM's snapshots, the newest first, each with when it was taken, what it
    /// holds, a description that's edited in place, and Revert and Delete at the row's end. The
    /// one the VM's current state comes from says "Now". Taking one is the camera by the power
    /// buttons (and here, while there are none).
    fn snapshots_tab(&mut self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let Some((s, _, v)) = self.vm_of(sel) else { return };
        let (sid, vref, snaps, parent) = (s.id, v.rref.clone(), v.snapshots.clone(), v.parent.clone());
        let doing = s.busy.get(&v.rref).map(|b| b.0.clone());
        let gone = self.vm_of(sel).map_or(false, |(_, d, v)| d.host_down_for(v).is_some());
        let can_take = v.can_snapshot() && doing.is_none() && !gone;
        ui.add_space(10.0);
        if let Some(what) = &doing {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0));
                ui.label(egui::RichText::new(what.as_str()).color(pal.ink_soft));
            });
            ui.add_space(4.0);
        }
        if snaps.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.set_max_width(420.0);
                ui.label(egui::RichText::new("No snapshots yet").size(15.0).color(pal.ink));
                ui.add_space(4.0);
                ui.add(egui::Label::new(egui::RichText::new("A snapshot keeps this VM's disks (and, if you like, its memory) as they are now, so you can come back to this moment later.").color(pal.ink_soft)).wrap());
                ui.add_space(10.0);
                if flat_button(ui, "camera", "Take Snapshot…", Some(pal.accent), can_take, false, &pal).clicked() {
                    out.push(Intent::TakeSnapshot(sid, vref.clone()));
                }
            });
            return;
        }
        let count = format!("{} snapshot{}", snaps.len(), if snaps.len() == 1 { "" } else { "s" });
        ui.label(egui::RichText::new(count).color(pal.ink_soft));
        ui.label(egui::RichText::new("Revert puts the VM back as it was then; what changed since is lost.").size(12.0).color(pal.ink_faint));
        let now = unix_now();
        let offset = local_offset(now);
        let font = FontId::proportional(13.0);
        let measure = |t: &str| ui.painter().layout_no_wrap(t.to_string(), font.clone(), pal.ink).size().x;
        const PILL: f32 = 42.0;
        // Revert and Delete always show: in a narrower window "Holds" goes (a memory module after
        // the time instead), then the description (shown over the name instead)
        #[derive(Clone, Copy, PartialEq)]
        enum Col {
            Name,
            Taken,
            Holds,
            Description,
            Actions,
        }
        let avail = ui.available_width();
        let cols: Vec<Col> = if avail >= 700.0 {
            vec![Col::Name, Col::Taken, Col::Holds, Col::Description, Col::Actions]
        } else if avail >= 540.0 {
            vec![Col::Name, Col::Taken, Col::Description, Col::Actions]
        } else {
            vec![Col::Name, Col::Taken, Col::Actions]
        };
        let headers: Vec<&str> = cols
            .iter()
            .map(|c| match c {
                Col::Name => "Name",
                Col::Taken => "Taken",
                Col::Holds => "Holds",
                Col::Description => "Description",
                Col::Actions => "",
            })
            .collect();
        let rows: Vec<Vec<Cell>> = snaps
            .iter()
            .map(|x| {
                cols.iter()
                    .map(|c| match c {
                        Col::Name => Cell::by_text(&x.name, Cell::Custom(24.0 + measure(&x.name) + if parent.as_deref() == Some(x.rref.as_str()) { PILL + 8.0 } else { 0.0 })),
                        Col::Taken => {
                            let when = x.time.map(|t| fmt_when(t, now, offset)).unwrap_or_else(|| "—".into());
                            let c = Cell::Custom(measure(&when) + if x.memory && !cols.contains(&Col::Holds) { 26.0 } else { 0.0 });
                            match x.time {
                                Some(t) => Cell::by_num(t as f64, c),
                                None => c,
                            }
                        }
                        Col::Holds => Cell::Text(if x.memory { "Disks and memory" } else { "Disks" }.into()),
                        Col::Description => {
                            let editing = self.snap_edit.as_ref().map_or(false, |e| e.vdi == x.rref);
                            let c = Cell::Custom(if editing { 240.0 } else { (measure(&x.description) + 34.0).min(380.0) });
                            if x.description.trim().is_empty() { c } else { Cell::by_text(&x.description, c) }
                        }
                        Col::Actions => Cell::Custom(86.0),
                    })
                    .collect()
            })
            .collect();
        let mut edit = self.snap_edit.take();
        let (mut close, mut save, mut delete, mut revert, mut template) = (false, None, None, None, None);
        let (saving, deleting) = (&self.snap_saving, &self.snap_deleting);
        let (with_description, with_holds) = (cols.contains(&Col::Description), cols.contains(&Col::Holds));
        self.table_with(ui, "snaps", &headers, &rows, false, &mut |ui, row, col, r, hot| {
            let x = &snaps[row];
            let gone = deleting.contains(&x.rref);
            let p = ui.painter().clone();
            let current = parent.as_deref() == Some(x.rref.as_str());
            if cols[col] == Col::Name {
                // the camera, the name (cut short if need be), and "Now" on the one the VM's
                // current state comes from
                paint_icon(&p, Rect::from_min_size(pos2(r.min.x, r.center().y - 8.0), vec2(16.0, 16.0)), IconKind::Snapshot, &pal, pal.window);
                let colour = if gone { pal.ink_faint } else { pal.ink };
                let mut job = egui::text::LayoutJob::simple_singleline(x.name.clone(), font.clone(), colour);
                job.wrap = egui::text::TextWrapping { max_width: (r.width() - 24.0 - if current { PILL + 8.0 } else { 0.0 }).max(20.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                let g = p.layout_job(job);
                let end = r.min.x + 24.0 + g.size().x;
                p.galley(pos2(r.min.x + 24.0, r.center().y - g.size().y / 2.0), g, colour);
                if !with_description && !x.description.is_empty() {
                    ui.interact(Rect::from_min_max(r.min, pos2(end, r.max.y)), ui.id().with(("name", &x.rref)), Sense::hover()).on_hover_text(x.description.as_str());
                }
                if current {
                    let pill = Rect::from_min_size(pos2(end + 8.0, r.center().y - 9.0), vec2(PILL, 18.0));
                    p.rect_filled(pill, Rounding::same(9.0), pal.accent.gamma_multiply(if pal.dark { 0.3 } else { 0.15 }));
                    p.text(pill.center(), Align2::CENTER_CENTER, "Now", FontId::proportional(11.5), pal.accent);
                    ui.interact(pill, ui.id().with(("now", &x.rref)), Sense::hover()).on_hover_text("The VM's current state comes from this snapshot");
                }
                return;
            }
            if cols[col] == Col::Taken {
                // when, then (with no Holds column) a memory module on one that holds its memory
                let when = x.time.map(|t| fmt_when(t, now, offset)).unwrap_or_else(|| "—".into());
                let chip = x.memory && !with_holds;
                let mut job = egui::text::LayoutJob::simple_singleline(when, font.clone(), pal.ink);
                job.wrap = egui::text::TextWrapping { max_width: (r.width() - if chip { 26.0 } else { 0.0 }).max(20.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
                let g = p.layout_job(job);
                let end = r.min.x + g.size().x;
                p.galley(pos2(r.min.x, r.center().y - g.size().y / 2.0), g, pal.ink);
                if chip {
                    let m = Rect::from_center_size(pos2(end + 16.0, r.center().y), vec2(16.0, 16.0));
                    paint_glyph(&p, m, "memory", pal.ink_soft);
                    ui.interact(m.expand(3.0), ui.id().with(("mem", &x.rref)), Sense::hover()).on_hover_text("Holds its memory too: reverting resumes the VM where it was");
                }
                return;
            }
            if cols[col] == Col::Actions {
                // Revert, then the bin
                if gone {
                    return;
                }
                let ok = doing.is_none() && !gone;
                // Make Template, from a snapshot of its disks only
                let tb = Rect::from_center_size(pos2(r.max.x - 43.0, r.center().y), vec2(26.0, 26.0));
                let tok = ok && !x.memory;
                let tip = if x.memory { "Templates come from snapshots of the disks only (this one holds its memory too)" } else { "Make a template from this snapshot…" };
                let tresp = ui.interact(tb, ui.id().with(("template", &x.rref)), if tok { Sense::click() } else { Sense::hover() }).on_hover_text(tip);
                if tok && tresp.hovered() {
                    p.rect_filled(tb, Rounding::same(5.0), pal.border);
                }
                let tc = if !tok { pal.ink_faint.gamma_multiply(0.45) } else if tresp.hovered() { pal.accent } else if hot { pal.ink_soft } else { pal.ink_faint };
                paint_glyph(&p, Rect::from_center_size(tb.center(), vec2(16.0, 16.0)), "template", tc);
                if tok && tresp.clicked() {
                    template = Some(x.rref.clone());
                }
                let rb = Rect::from_center_size(pos2(r.max.x - 73.0, r.center().y), vec2(26.0, 26.0));
                let rresp = ui.interact(rb, ui.id().with(("revert", &x.rref)), if ok { Sense::click() } else { Sense::hover() }).on_hover_text("Revert the VM to this snapshot…");
                if ok && rresp.hovered() {
                    p.rect_filled(rb, Rounding::same(5.0), pal.border);
                }
                let rc = if !ok { pal.ink_faint.gamma_multiply(0.45) } else if rresp.hovered() { pal.orange } else if hot { pal.ink_soft } else { pal.ink_faint };
                paint_glyph(&p, Rect::from_center_size(rb.center(), vec2(16.0, 16.0)), "revert", rc);
                if ok && rresp.clicked() {
                    revert = Some(x.rref.clone());
                }
                let b = Rect::from_center_size(pos2(r.max.x - 13.0, r.center().y), vec2(26.0, 26.0));
                let resp = ui.interact(b, ui.id().with(("delete", &x.rref)), Sense::click()).on_hover_text("Delete this snapshot…");
                if resp.hovered() {
                    p.rect_filled(b, Rounding::same(5.0), pal.red.gamma_multiply(if pal.dark { 0.25 } else { 0.12 }));
                }
                let colour = if resp.hovered() { pal.red } else if hot { pal.ink_soft } else { pal.ink_faint };
                paint_glyph(&p, Rect::from_center_size(b.center(), vec2(16.0, 16.0)), "trash", colour);
                if resp.clicked() {
                    delete = Some(x.rref.clone());
                }
                return;
            }
            // the description
            if gone {
                p.text(pos2(r.min.x, r.center().y), Align2::LEFT_CENTER, "Deleting…", font.clone(), pal.ink_faint);
                return;
            }
            if let Some(e) = edit.as_mut().filter(|e| e.vdi == x.rref) {
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
                    if text != x.description {
                        save = Some((x.rref.clone(), text));
                    }
                }
                close |= ok || enter || cancel;
                return;
            }
            let pending = saving.get(&x.rref);
            let (text, col) = match pending {
                Some(t) => (t.as_str(), pal.ink_faint),
                None if x.description.is_empty() => ("—", pal.ink_faint),
                None => (x.description.as_str(), pal.ink),
            };
            let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font.clone(), col);
            job.wrap = egui::text::TextWrapping { max_width: (r.width() - 34.0).max(10.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
            let g = p.layout_job(job);
            let end = r.min.x + g.size().x;
            p.galley(pos2(r.min.x, r.center().y - g.size().y / 2.0), g, col);
            if pending.is_some() {
                return;
            }
            let b = Rect::from_center_size(pos2(end + 19.0, r.center().y), vec2(26.0, 26.0));
            let resp = ui.interact(b, ui.id().with(("describe", &x.rref)), Sense::click()).on_hover_text("Edit the description");
            if resp.hovered() {
                p.rect_filled(b, Rounding::same(5.0), pal.border);
            }
            paint_glyph(&p, Rect::from_center_size(b.center(), vec2(16.0, 16.0)), "edit", if hot { pal.ink_soft } else { pal.ink_faint });
            if resp.clicked() {
                edit = Some(IsoEdit { vdi: x.rref.clone(), text: x.description.clone(), focus: true });
            }
        });
        self.snap_edit = if close { None } else { edit };
        if let Some((snap, text)) = save {
            out.push(Intent::SaveSnapshotDescription(sid, snap, text));
        }
        if let Some(snap) = revert {
            out.push(Intent::AskRevert(sid, snap));
        }
        if let Some(snap) = delete {
            out.push(Intent::AskDeleteSnapshot(sid, snap));
        }
        if let Some(snap) = template {
            out.push(Intent::MakeTemplate(sid, snap));
        }
    }

    /// The Templates folder: the user's templates, each with its system, size and description, and
    /// New VM and Delete at the row's end; a click on a row opens it.
    /// Templates made from VMs: a pool's (its Templates folder), or every pool's (the toolbar's
    /// Templates, All servers' tab), each with New VM and Delete; a click opens one.
    fn templates_tab(&mut self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let all = sel.kind == Kind::Root;
        // (pool, its name, the template, busy)
        let mut tpls: Vec<(u32, String, VmInfo, bool)> = vec![];
        for s in self.servers.iter().filter(|s| all || Some(s.id) == sel.sid) {
            let Some(d) = s.data.as_ref() else { continue };
            tpls.extend(d.user_templates.iter().map(|t| (s.id, s.name(), t.clone(), s.busy.contains_key(&t.rref))));
        }
        tpls.sort_by_key(|t| t.2.name.to_lowercase());
        let pools = tpls.iter().map(|t| t.0).collect::<HashSet<_>>().len();
        ui.add_space(10.0);
        ui.label(egui::RichText::new(format!("{} template{}", tpls.len(), if tpls.len() == 1 { "" } else { "s" })).color(pal.ink_soft));
        ui.label(egui::RichText::new("Made from your VMs, to make new VMs from: New Template picks the VM.").size(12.0).color(pal.ink_faint));
        let wide = ui.available_width() >= 640.0;
        let mut headers = vec!["Name"];
        if all && pools > 1 {
            headers.push("Pool");
        }
        headers.extend(["System", "Size", "Created"]);
        if wide {
            headers.push("Description");
        }
        headers.push("");
        let offset = local_offset(unix_now());
        let rows: Vec<Vec<Cell>> = tpls
            .iter()
            .map(|(_, pool, t, _)| {
                let disk: i64 = t.disks.iter().filter(|k| k.kind == "Disk").map(|k| k.size).sum();
                let mut r = vec![Cell::Icon(IconKind::Template, t.name.clone())];
                if all && pools > 1 {
                    r.push(Cell::Text(pool.clone()));
                }
                // the day it was made
                let created = match t.created {
                    Some(c) => Cell::by_num(c as f64, Cell::Text(fmt_date_time(c, offset).split(',').next().unwrap_or_default().to_string())),
                    None => Cell::Text("—".into()),
                };
                r.extend([
                    Cell::Text(or(&t.os, "—").into()),
                    Cell::Text(format!("{} CPU{}  ·  {}  ·  {} disk", t.vcpus, if t.vcpus == 1 { "" } else { "s" }, fmt_bytes(t.mem_dyn_max), fmt_bytes(disk))),
                    created,
                ]);
                if wide {
                    r.push(Cell::Text(or(&t.description, "—").into()));
                }
                r.push(Cell::Custom(56.0));
                r
            })
            .collect();
        let (mut new_vm, mut delete) = (None, None);
        let opened = self.table_with(ui, "tpls", &headers, &rows, true, &mut |ui, row, _col, r, hot| {
            let (sid, _, t, busy) = &tpls[row];
            if *busy {
                return;
            }
            let p = ui.painter().clone();
            let nb = Rect::from_center_size(pos2(r.max.x - 43.0, r.center().y), vec2(26.0, 26.0));
            let nresp = ui.interact(nb, ui.id().with(("tnew", &t.rref)), Sense::click()).on_hover_text("Make a VM from this template…");
            if nresp.hovered() {
                p.rect_filled(nb, Rounding::same(5.0), pal.border);
            }
            paint_glyph(&p, Rect::from_center_size(nb.center(), vec2(16.0, 16.0)), "newvm", if nresp.hovered() { pal.accent } else if hot { pal.ink_soft } else { pal.ink_faint });
            if nresp.clicked() {
                new_vm = Some((*sid, t.rref.clone()));
            }
            let b = Rect::from_center_size(pos2(r.max.x - 13.0, r.center().y), vec2(26.0, 26.0));
            let resp = ui.interact(b, ui.id().with(("tdel", &t.rref)), Sense::click()).on_hover_text("Delete this template…");
            if resp.hovered() {
                p.rect_filled(b, Rounding::same(5.0), pal.red.gamma_multiply(if pal.dark { 0.25 } else { 0.12 }));
            }
            paint_glyph(&p, Rect::from_center_size(b.center(), vec2(16.0, 16.0)), "trash", if resp.hovered() { pal.red } else if hot { pal.ink_soft } else { pal.ink_faint });
            if resp.clicked() {
                delete = Some((*sid, t.rref.clone()));
            }
        });
        if let Some((sid, t)) = new_vm {
            out.push(Intent::NewVmFromTemplate(sid, t));
        } else if let Some((sid, t)) = delete {
            out.push(Intent::AskDeleteTemplate(sid, t));
        } else if let Some(i) = opened {
            out.push(Intent::Select(format!("tpl:{}:{}", tpls[i].0, tpls[i].2.rref)));
        }
    }

    /// All servers' Networks tab (the toolbar's Networks): every pool's networks, each with how it
    /// reaches outside (a VLAN on a card, a card, or none: private), its bridge, MTU and
    /// description. New Network is next to the tabs.
    fn networks_page(&self, ui: &mut Ui, out: &mut Vec<Intent>) {
        let pal = self.pal;
        // (pool, its name, the network, why it can't be deleted now)
        let mut nets: Vec<(u32, String, NetInfo, Option<String>)> = vec![];
        for s in &self.servers {
            let Some(d) = s.data.as_ref() else { continue };
            for n in &d.networks {
                let users: Vec<&str> = d.vms.iter().chain(&d.user_templates).filter(|v| v.nics.iter().any(|c| c.net == n.rref)).map(|v| v.name.as_str()).collect();
                let why = if !n.device.is_empty() && n.vlan < 0 {
                    Some(format!("It's {}'s own network: it goes only with the card", n.device))
                } else if !users.is_empty() {
                    let more = if users.len() > 3 { format!(" and {} more", users.len() - 3) } else { String::new() };
                    Some(format!("Cards are on it ({}{}): move or remove them first", users.iter().take(3).map(|u| format!("'{}'", u)).collect::<Vec<_>>().join(", "), more))
                } else {
                    None
                };
                nets.push((s.id, s.name(), n.clone(), why));
            }
        }
        nets.sort_by_key(|t| t.2.name.to_lowercase());
        let pools = self.servers.iter().filter(|s| s.data.is_some()).count();
        ui.add_space(10.0);
        ui.label(egui::RichText::new(format!("{} network{}", nets.len(), if nets.len() == 1 { "" } else { "s" })).color(pal.ink_soft));
        ui.label(egui::RichText::new("The networks your VMs' cards join: on a server's network card (or a VLAN of one), or private to each server.").size(12.0).color(pal.ink_faint));
        let mut headers = vec!["Name"];
        if pools > 1 {
            headers.push("Pool");
        }
        headers.extend(["Connection", "Bridge", "MTU", "Description", ""]);
        let rows: Vec<Vec<Cell>> = nets
            .iter()
            .map(|(_, pool, n, _)| {
                let mut r = vec![Cell::Icon(IconKind::Net, n.name.clone())];
                if pools > 1 {
                    r.push(Cell::Text(pool.clone()));
                }
                r.extend([Cell::Text(n.connection()), Cell::Text(n.bridge.clone()), Cell::Text(n.mtu.to_string()), Cell::Text(or(&n.description, "—").into()), Cell::Custom(26.0)]);
                r
            })
            .collect();
        let mut delete = None;
        self.table_with(ui, "all-nets", &headers, &rows, false, &mut |ui, row, _col, r, hot| {
            // the bin: red under the mouse; off, saying why, for a card's own network or one in use
            let (sid, _, n, why) = &nets[row];
            let b = Rect::from_center_size(pos2(r.max.x - 13.0, r.center().y), vec2(26.0, 26.0));
            let resp = ui.interact(b, ui.id().with(("ndel", &n.rref)), if why.is_none() { Sense::click() } else { Sense::hover() });
            let resp = resp.on_hover_text(why.as_deref().unwrap_or("Delete this network…"));
            if why.is_none() && resp.hovered() {
                ui.painter().rect_filled(b, Rounding::same(5.0), pal.red.gamma_multiply(if pal.dark { 0.25 } else { 0.12 }));
            }
            let colour = if why.is_some() {
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
                delete = Some((*sid, n.rref.clone()));
            }
        });
        if let Some((sid, net)) = delete {
            out.push(Intent::AskDeleteNetwork(sid, net));
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
        egui::Frame::none().fill(pal.surface).stroke(Stroke::new(1.0_f32, pal.border)).rounding(Rounding::same(8.0)).inner_margin(egui::Margin::symmetric(12.0, 10.0)).show(ui, |ui| {
            ui.set_width(ui.available_width()); // as wide as the list under it
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

    /// The Logs tab: the newest first (for a VM, its own); a click on one shows it in full, with
    /// what the server said.
    fn logs(&self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        let shown: Vec<usize> = (0..self.log.len())
            .rev()
            .filter(|&i| {
                let e = &self.log[i];
                match sel.sid {
                    Some(sid) => e.sid == sid && (sel.kind != Kind::Vm || e.obj_ref.as_deref() == sel.rref.as_deref()),
                    None => true,
                }
            })
            .collect();
        let rows: Vec<Vec<Cell>> = shown
            .iter()
            .map(|&i| {
                let e = &self.log[i];
                vec![Cell::Text(e.time.clone()), Cell::Text(e.server.clone()), Cell::Text(e.obj.clone()), Cell::Text(e.text.clone()), Cell::Status(e.status.clone())]
            })
            .collect();
        if let Some(r) = self.table(ui, "logs", &["Time", "Server", "Object", "What happened", "Result"], &rows, true) {
            out.push(Intent::ShowLog(shown[r]));
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut add: Option<(String, String, String, bool)> = None;
        let mut iso: Option<(u32, IsoLibrary)> = None;
        let mut reattach: Option<(u32, String, IsoLibrary)> = None;
        // from the Close question: close anyway (true) or stay (false)
        let mut close_app: Option<bool> = None;
        // from New VM Storage: make it (pool, settings), or ask a server for its folders
        let mut storage: Option<(u32, NfsStorage)> = None;
        let mut browse: Option<(u32, String)> = None;
        let mut network: Option<(u32, NewNetwork)> = None;
        let mut delete_net: Option<(u32, String, String)> = None;
        // the pool's own VM that the server typed in New VM Storage is, if it's one
        let guest: Option<Guest> = match &self.dialog {
            Dialog::NewStorage(d) => self.server(d.sid).and_then(|s| s.data.as_ref()).and_then(|pd| {
                let (v, sure) = pd.vm_at(&d.server())?;
                let host = v.host.as_deref().or(v.home()).and_then(|h| pd.host(h)).map_or("a server of the pool".to_string(), |h| h.name.clone());
                Some(Guest { vm: v.name.clone(), host, sure })
            }),
            _ => None,
        };
        let mut forget_sr: Option<(u32, String)> = None;
        let mut delete_iso: Option<(u32, String, String, String)> = None;
        // from the Delete VM question: server, VM, its name, whether its disks go too
        let mut delete_vm: Option<(u32, String, String, bool)> = None;
        // from the snapshot dialogs: take one (server, VM, name, description, memory), revert to
        // one (server, snapshot, a snapshot of now first), delete one (server, snapshot)
        let mut take_snap: Option<(u32, String, String, String, bool)> = None;
        let mut revert_to: Option<(u32, String, bool)> = None;
        let mut delete_snap: Option<(u32, String)> = None;
        // from the template and copy dialogs
        let mut make_tpl: Option<(u32, String, String, String, String)> = None;
        let mut copy_to: Option<(u32, String, String, Option<String>)> = None;
        let mut delete_tpl: Option<(u32, String)> = None;
        // something left half made, to delete: server, what, its name
        let mut leftover: Option<(u32, Vec<String>, String)> = None;
        // a change asked about, now to make
        let mut edit_now: Option<(u32, String, VmChange)> = None;
        // from the Shut Down Server question: pool, server, its name
        let mut shutdown_host: Option<(u32, String, String, bool)> = None;
        // from Maintenance Mode's questions: pool, server, its name; and None to go in, or
        // Some(whether its VMs come back) to come out
        let mut maint: Option<(u32, String, String, Option<bool>)> = None;
        let mut save_rules = false;
        // from Settings: the theme and size chosen, a saved password to forget
        let mut theme: Option<ThemeMode> = None;
        let mut zoom: Option<f32> = None;
        let mut forget: Option<(String, String)> = None;
        // from Settings and the Update dialog
        let mut updates_on = self.check_updates;
        let mut repair_on = self.auto_repair;
        let mut check_now = false;
        let mut start_update = false;
        let (update_checking, update_note, blocked) = (self.update_checking, self.update_note.clone(), self.update_blocked());
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
        let detail: Option<LogEntry> = match &self.dialog {
            Dialog::LogDetail { index, .. } => self.log.get(*index).cloned(),
            _ => None,
        };
        let upload_fraction = |i: usize| self.uploads.values().find(|u| u.log == i && u.active()).and_then(|u| {
            let (sent, total) = (u.t.sent.load(Ordering::Relaxed), u.t.total.load(Ordering::Relaxed));
            (total > 0).then(|| sent as f32 / total as f32)
        });
        let detail_fraction = match &self.dialog {
            Dialog::LogDetail { index, .. } => upload_fraction(*index),
            _ => None,
        };
        // the Updates dialog: what it shows of its pool, and what's asked of it
        let pool_view = match &self.dialog {
            Dialog::PoolUpdate(d) => Some(self.pool_view(d.sid)),
            _ => None,
        };
        let mut pool_action = PoolUpdateAction::None;
        let mut move_action: Option<(u32, MoveAction)> = None;
        match &mut self.dialog {
            Dialog::MoveVm(d) => {
                move_action = Some((d.sid, d.ui(ctx, &pal)));
            }
            Dialog::PoolUpdate(d) => {
                if let Some(v) = &pool_view {
                    pool_action = d.ui(ctx, &pal, v);
                }
            }
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
                        p.rect(r, Rounding::same(5.0), if resp.hovered() { pal.hover } else { pal.input }, Stroke::new(1.0_f32, pal.border_strong));
                        let c = r.center();
                        p.add(egui::Shape::line(vec![pos2(c.x - 4.0, c.y - 2.0), pos2(c.x, c.y + 2.0), pos2(c.x + 4.0, c.y - 2.0)], Stroke::new(1.6_f32, pal.ink_soft)));
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
            Dialog::ConfirmClose { items, .. } => {
                dialog_window(ctx, "Close Simple VM Manager?").show(ctx, |ui| {
                    ui.set_width(480.0);
                    ui.spacing_mut().item_spacing.y = 8.0;
                    let intro = if items.len() == 1 { "This stops halfway if the app closes now:" } else { "These stop halfway if the app closes now:" };
                    ui.add(egui::Label::new(intro).wrap());
                    egui::ScrollArea::vertical().max_height(320.0).auto_shrink([false, true]).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 8.0;
                        for (what, left) in items.iter() {
                            crate::newvm::alert(ui, &pal, false, what, left, None);
                        }
                    });
                    ui.add(egui::Label::new(egui::RichText::new("Anything else under way goes on on the servers by itself.").size(12.5).color(pal.ink_soft)).wrap());
                    ui.add_space(4.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "close", "Close anyway", Look::Danger, true) {
                            close_app = Some(true);
                        }
                        if dialog_button(ui, &pal, "check", "Keep it open", Look::Primary, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close_app = Some(false);
                        }
                    });
                });
            }
            Dialog::NewNetwork(d) => match d.ui(ctx, &pal) {
                NetworkAction::Close => close = true,
                NetworkAction::Create(sid, nw) => network = Some((sid, nw)),
                NetworkAction::None => {}
            },
            Dialog::NewStorage(d) => match d.ui(ctx, &pal, guest.as_ref()) {
                StorageAction::Close => close = true,
                StorageAction::Create(sid, st) => storage = Some((sid, st)),
                StorageAction::Browse(sid, server) => browse = Some((sid, server)),
                StorageAction::None => {}
            },
            Dialog::Iso(d) => match d.ui(ctx, &pal) {
                IsoAction::Close => close = true,
                IsoAction::Create(sid, lib) => iso = Some((sid, lib)),
                IsoAction::Reattach(sid, sr, lib) => reattach = Some((sid, sr, lib)),
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
            Dialog::TakeSnapshot { sid, vm, vm_name, name, description, memory, can_memory, memory_size, focus } => {
                dialog_window(ctx, "Take Snapshot").show(ctx, |ui| {
                    ui.set_width(440.0);
                    ui.spacing_mut().item_spacing.y = 8.0;
                    let what = if *memory { "disks and memory" } else { "disks" };
                    ui.add(egui::Label::new(egui::RichText::new(format!("Keeps the {} of '{}' as they are now, to come back to later.", what, vm_name)).color(pal.ink_soft)).wrap());
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        field_label(ui, "Name:", LABEL_WIDTH);
                        let r = text_field(ui, &pal, name, ui.available_width(), false);
                        if *focus {
                            r.request_focus();
                            *focus = false;
                        }
                    });
                    ui.horizontal(|ui| {
                        field_label(ui, "Description:", LABEL_WIDTH);
                        text_field(ui, &pal, description, ui.available_width(), false);
                    });
                    if *can_memory {
                        ui.horizontal(|ui| {
                            ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x);
                            ui.checkbox(memory, "Include its memory");
                        });
                        ui.horizontal(|ui| {
                            ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x + 2.0);
                            let note = format!("Reverting to it then resumes the VM right where it was. It pauses while its {} of memory is written to disk.", fmt_bytes(*memory_size));
                            ui.add(egui::Label::new(egui::RichText::new(note).size(12.0).color(pal.ink_soft)).wrap());
                        });
                    }
                    ui.add_space(4.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                        let ready = !name.trim().is_empty();
                        let enter = ui.input(|i| i.key_pressed(Key::Enter));
                        if dialog_button(ui, &pal, "camera", "Take Snapshot", Look::Primary, ready) || (ready && enter) {
                            take_snap = Some((*sid, vm.clone(), name.trim().to_string(), description.trim().to_string(), *memory && *can_memory));
                        }
                    });
                });
            }
            Dialog::ConfirmRevert { sid, snap, vm_name, snap_name, text, first } => {
                dialog_window(ctx, "Revert to Snapshot").show(ctx, |ui| {
                    ui.set_width(440.0);
                    ui.add(egui::Label::new(format!("Revert '{}' to its snapshot '{}'?", vm_name, snap_name)).wrap());
                    ui.add_space(8.0);
                    ui.add(egui::Label::new(egui::RichText::new(text.as_str()).color(pal.ink_soft)).wrap());
                    ui.add_space(8.0);
                    ui.checkbox(first, "Take a snapshot of now first, to be able to come back");
                    ui.add_space(12.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                        if dialog_button(ui, &pal, "revert", "Revert", if *first { Look::Primary } else { Look::Danger }, true) {
                            revert_to = Some((*sid, snap.clone(), *first));
                        }
                    });
                });
            }
            Dialog::ConfirmDeleteSnapshot { sid, snap, vm_name, snap_name, memory } => {
                dialog_window(ctx, "Delete Snapshot").show(ctx, |ui| {
                    ui.set_width(420.0);
                    ui.add(egui::Label::new(format!("Delete the snapshot '{}' of '{}'?", snap_name, vm_name)).wrap());
                    ui.add_space(8.0);
                    let mine = if *memory { "Its copies of the disks and its saved memory go" } else { "Its copies of the disks go" };
                    let text = format!("{}; the VM itself isn't touched. The space comes back once the storage has merged the disks, in a few minutes. This can't be undone.", mine);
                    ui.add(egui::Label::new(egui::RichText::new(text).color(pal.ink_soft)).wrap());
                    ui.add_space(12.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                        if dialog_button(ui, &pal, "trash", "Delete", Look::Danger, true) {
                            delete_snap = Some((*sid, snap.clone()));
                        }
                    });
                });
            }
            Dialog::MakeTemplate { sid, vm, source, from, name, description, power_cut, focus, pick } => {
                dialog_window(ctx, if pick.is_empty() { "Make Template" } else { "New Template" }).show(ctx, |ui| {
                    ui.set_width(460.0);
                    ui.spacing_mut().item_spacing.y = 8.0;
                    // why the VM chosen can't be made a template now, if it can't
                    let mut blocked: Option<(String, String)> = None;
                    if pick.is_empty() {
                        ui.add(egui::Label::new(egui::RichText::new(format!("A template from {}, to make new VMs from. Its disks are copied in an instant on their own storage.", from)).color(pal.ink_soft)).wrap());
                    } else {
                        let intro = "A template is a shut-down VM's copy that new VMs are made from; the VM stays as it is. Its disks are copied in an instant on their own storage.";
                        ui.add(egui::Label::new(egui::RichText::new(intro).color(pal.ink_soft)).wrap());
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            field_label(ui, "VM:", LABEL_WIDTH);
                            crate::newvm::combo_style(ui, &pal);
                            let now = pick.iter().position(|p| p.sid == *sid && p.vm == *vm).unwrap_or(0);
                            let mut chosen = now;
                            crate::newvm::combo("tpl-vm", pick[now].label.clone(), ui.available_width()).show_ui(ui, |ui| {
                                for (i, p) in pick.iter().enumerate() {
                                    let text = egui::RichText::new(p.label.as_str()).color(if p.why.is_some() { pal.ink_soft } else { pal.ink });
                                    ui.selectable_value(&mut chosen, i, text);
                                }
                            });
                            if chosen != now {
                                // its name and description follow, unless they were changed
                                let (old, new) = (&pick[now], &pick[chosen]);
                                if *name == format!("{} template", old.name) {
                                    *name = format!("{} template", new.name);
                                }
                                let was = format!("From '{}', ", old.name);
                                if description.starts_with(&was) {
                                    *description = format!("From '{}', {}", new.name, &description[was.len()..]);
                                }
                                (*sid, *vm, *source) = (new.sid, new.vm.clone(), new.vm.clone());
                                *from = format!("'{}' (it stays as it is)", new.name);
                            }
                        });
                        blocked = pick.iter().find(|p| p.sid == *sid && p.vm == *vm).and_then(|p| p.why.clone());
                    }
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        field_label(ui, "Name:", LABEL_WIDTH);
                        let r = text_field(ui, &pal, name, ui.available_width(), false);
                        if *focus {
                            r.request_focus();
                            *focus = false;
                        }
                    });
                    ui.horizontal(|ui| {
                        field_label(ui, "Description:", LABEL_WIDTH);
                        text_field(ui, &pal, description, ui.available_width(), false);
                    });
                    ui.add_space(2.0);
                    if let Some((title, text)) = &blocked {
                        crate::newvm::alert(ui, &pal, false, title, text, None);
                    } else {
                        if *power_cut {
                            crate::newvm::alert(ui, &pal, false, "Taken while the VM was running.", "VMs made from it start as after a power cut: they check their disks, and programs that were busy may need to recover.", None);
                        }
                        let tip = "Windows: run  sysprep /generalize /oobe /shutdown  first.\nLinux: empty the machine ID (truncate -s 0 /etc/machine-id) and delete the SSH host keys (/etc/ssh/ssh_host_*), then shut it down; on Debian and Ubuntu, run  ssh-keygen -A  on each new VM.";
                        crate::newvm::alert(ui, &pal, false, "Generalize it first, so its copies don't clash.", tip, None);
                    }
                    ui.add_space(4.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                        let ready = !name.trim().is_empty() && blocked.is_none();
                        if dialog_button(ui, &pal, "template", "Make Template", Look::Primary, ready) || (ready && ui.input(|i| i.key_pressed(Key::Enter))) {
                            make_tpl = Some((*sid, vm.clone(), source.clone(), name.trim().to_string(), description.trim().to_string()));
                        }
                    });
                });
            }
            Dialog::CopyVm { sid, vm, vm_name, name, stores, store, focus } => {
                dialog_window(ctx, "Copy VM").show(ctx, |ui| {
                    ui.set_width(460.0);
                    ui.spacing_mut().item_spacing.y = 8.0;
                    ui.add(egui::Label::new(egui::RichText::new(format!("A new VM just like '{}', with its own disks; it starts off. New network cards get new addresses.", vm_name)).color(pal.ink_soft)).wrap());
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        field_label(ui, "Name:", LABEL_WIDTH);
                        let r = text_field(ui, &pal, name, ui.available_width(), false);
                        if *focus {
                            r.request_focus();
                            *focus = false;
                        }
                    });
                    if !stores.is_empty() {
                        ui.horizontal(|ui| {
                            field_label(ui, "Disks on:", LABEL_WIDTH);
                            crate::newvm::combo_style(ui, &pal);
                            let shown = stores.get(*store).map(|s| s.1.clone()).unwrap_or_default();
                            crate::newvm::combo("copy-sr", shown, ui.available_width()).show_ui(ui, |ui| {
                                for (i, s) in stores.iter().enumerate() {
                                    ui.selectable_value(store, i, s.1.as_str());
                                }
                            });
                        });
                    }
                    let instant = stores.get(*store).map_or(true, |s| s.2);
                    let note = if instant {
                        "The copy shares the original's disks until either changes them, so it's ready at once and takes no room to begin with."
                    } else {
                        "Its disks are written out in full on the other storage: as long as copying them takes."
                    };
                    ui.add(egui::Label::new(egui::RichText::new(note).size(12.0).color(pal.ink_soft)).wrap());
                    ui.add_space(4.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                        let ready = !name.trim().is_empty();
                        if dialog_button(ui, &pal, "copy", "Copy", Look::Primary, ready) || (ready && ui.input(|i| i.key_pressed(Key::Enter))) {
                            let sr = stores.get(*store).filter(|s| !s.2).map(|s| s.0.clone());
                            copy_to = Some((*sid, vm.clone(), name.trim().to_string(), sr));
                        }
                    });
                });
            }
            Dialog::ConfirmDeleteNetwork { sid, net, name, text } => {
                dialog_window(ctx, "Delete Network").show(ctx, |ui| {
                    ui.set_width(420.0);
                    ui.add(egui::Label::new(format!("Delete the network '{}'?", name)).wrap());
                    ui.add_space(8.0);
                    ui.add(egui::Label::new(egui::RichText::new(text.as_str()).color(pal.ink_soft)).wrap());
                    ui.add_space(12.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                        if dialog_button(ui, &pal, "trash", "Delete", Look::Danger, true) {
                            delete_net = Some((*sid, net.clone(), name.clone()));
                        }
                    });
                });
            }
            Dialog::ConfirmDeleteTemplate { sid, tpl, name, disk } => {
                dialog_window(ctx, "Delete Template").show(ctx, |ui| {
                    ui.set_width(420.0);
                    ui.add(egui::Label::new(format!("Delete the template '{}'?", name)).wrap());
                    ui.add_space(8.0);
                    let text = format!("Its disks ({}) go too. VMs already made from it aren't touched. This can't be undone.", fmt_bytes(*disk));
                    ui.add(egui::Label::new(egui::RichText::new(text).color(pal.ink_soft)).wrap());
                    ui.add_space(12.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                        if dialog_button(ui, &pal, "trash", "Delete", Look::Danger, true) {
                            delete_tpl = Some((*sid, tpl.clone()));
                        }
                    });
                });
            }
            Dialog::ConfirmLeftover { sid, name, refs, server } => {
                dialog_window(ctx, "Left Half Made?").show(ctx, |ui| {
                    ui.set_width(440.0);
                    ui.add(egui::Label::new(format!("'{}' was being made when contact with {} was lost.", name, server)).wrap());
                    ui.add_space(8.0);
                    let what = if refs.len() == 1 { "It's there now, but it may be incomplete (its disks only partly copied)" } else { "There are a few of them now, maybe incomplete" };
                    ui.add(egui::Label::new(egui::RichText::new(format!("{}. Delete it, with its disks? To keep it, check that it works first.", what)).color(pal.ink_soft)).wrap());
                    ui.add_space(12.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "check", "Keep it", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                        if dialog_button(ui, &pal, "trash", "Delete", Look::Danger, true) {
                            leftover = Some((*sid, refs.clone(), name.clone()));
                        }
                    });
                });
            }
            Dialog::ConfirmHostShutdown { sid, host, name, moving, stopping, coordinator, alone, reboot } => {
                dialog_window(ctx, if *reboot { "Restart Server" } else { "Shut Down Server" }).show(ctx, |ui| {
                    ui.set_width(440.0);
                    ui.label(format!("Are you sure you want to {} {}?", if *reboot { "restart" } else { "shut down" }, name));
                    ui.add_space(8.0);
                    let soft = |ui: &mut Ui, text: String| {
                        ui.add(egui::Label::new(egui::RichText::new(text).color(pal.ink_soft)).wrap());
                    };
                    if moving.is_empty() && stopping.is_empty() {
                        soft(ui, "No VMs are running on it.".into());
                    }
                    if !moving.is_empty() {
                        let stay = if *reboot { " They stay there once it's back." } else { "" };
                        soft(ui, format!("These move to the pool's other servers first: {}.{}", moving.join(", "), stay));
                    }
                    if !stopping.is_empty() {
                        if !moving.is_empty() {
                            ui.add_space(4.0);
                        }
                        let why = if *alone { "there's no other server to take them" } else { "their disks are on this server's own storage" };
                        soft(ui, format!("These are shut down, as {}: {}. One without guest tools is turned off the hard way.", why, stopping.join(", ")));
                    }
                    let while_ = if *reboot { "While it restarts" } else { "While it's off" };
                    if *coordinator {
                        ui.add_space(8.0);
                        crate::newvm::alert(ui, &pal, false, "It's the pool's coordinator.", &format!("{}, the pool's other servers can't be managed, from this app or XenCenter.", while_), None);
                    } else if *alone {
                        ui.add_space(8.0);
                        let until = if *reboot { "until it's back, in a few minutes" } else { "until it's back on" };
                        crate::newvm::alert(ui, &pal, false, "It's the only server.", &format!("This app loses its connection to it {}.", until), None);
                    }
                    ui.add_space(8.0);
                    let in_maintenance = self.servers.iter().find(|s| s.id == *sid).and_then(|s| s.data.as_ref()).and_then(|d| d.host(host)).map_or(false, |h| h.in_maintenance());
                    if *reboot && in_maintenance {
                        soft(ui, "It's back in a few minutes, still in maintenance mode; Activities says when it answers.".into());
                    } else if *reboot {
                        soft(ui, "It's back in a few minutes, taking new VMs again; Activities says when it answers.".into());
                    } else {
                        soft(ui, "To turn it on again: Power On here (Wake-on-LAN, when its firmware has it on), or its power button.".into());
                    }
                    ui.add_space(12.0);
                    dialog_buttons(ui, |ui| {
                        // Esc or Cancel keep it on; only a click on the red button shuts it down
                        if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                        let (glyph, label) = if *reboot { ("retry", "Restart") } else { ("power", "Shut Down") };
                        if dialog_button(ui, &pal, glyph, label, Look::Danger, true) {
                            shutdown_host = Some((*sid, host.clone(), name.clone(), *reboot));
                        }
                    });
                });
            }
            Dialog::ConfirmMaintenance { sid, host, name, moving, stopping, coordinator } => {
                dialog_window(ctx, "Maintenance Mode").show(ctx, |ui| {
                    ui.set_width(440.0);
                    ui.label(format!("Put {} in maintenance mode?", name));
                    ui.add_space(8.0);
                    let soft = |ui: &mut Ui, text: String| {
                        ui.add(egui::Label::new(egui::RichText::new(text).color(pal.ink_soft)).wrap());
                    };
                    soft(ui, "It stops taking new VMs, and stays so till you take it out of maintenance mode.".into());
                    ui.add_space(4.0);
                    if moving.is_empty() && stopping.is_empty() {
                        soft(ui, "No VMs are running on it.".into());
                    }
                    if !moving.is_empty() {
                        soft(ui, format!("These move to the pool's other servers: {}.", moving.join(", ")));
                    }
                    if !stopping.is_empty() {
                        let why = if moving.is_empty() && !leaving_somewhere(&self.servers, *sid, host) { "there's no other server to take them" } else { "their disks are on this server's own storage" };
                        soft(ui, format!("These are shut down, as {}: {}. One without guest tools is turned off the hard way.", why, stopping.join(", ")));
                    }
                    if *coordinator {
                        ui.add_space(4.0);
                        soft(ui, "It stays the pool's coordinator: the pool is still managed through it.".into());
                    }
                    if !moving.is_empty() || !stopping.is_empty() {
                        ui.add_space(4.0);
                        soft(ui, "When it comes out, they can come back to it.".into());
                    }
                    ui.add_space(12.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                        let look = if stopping.is_empty() { Look::Primary } else { Look::Danger };
                        if dialog_button(ui, &pal, "wrench", "Enter Maintenance Mode", look, true) {
                            maint = Some((*sid, host.clone(), name.clone(), None));
                        }
                    });
                });
            }
            Dialog::AlertRules(d) => match alerts::rules_window(ctx, &pal, d) {
                RulesAct::Close => close = true,
                RulesAct::Save => save_rules = true,
                RulesAct::Nothing => {}
            },
            Dialog::ExitMaintenance { sid, host, name, moved, stopped, back } => {
                dialog_window(ctx, "Exit Maintenance Mode").show(ctx, |ui| {
                    ui.set_width(440.0);
                    ui.label(format!("Take {} out of maintenance mode?", name));
                    ui.add_space(8.0);
                    let soft = |ui: &mut Ui, text: String| {
                        ui.add(egui::Label::new(egui::RichText::new(text).color(pal.ink_soft)).wrap());
                    };
                    soft(ui, "It takes new VMs again.".into());
                    if !moved.is_empty() || !stopped.is_empty() {
                        ui.add_space(8.0);
                        let n = moved.len() + stopped.len();
                        ui.checkbox(back, if n == 1 { "Bring back the VM it had".to_string() } else { format!("Bring back the {} VMs it had", n) });
                        ui.horizontal(|ui| {
                            ui.add_space(24.0);
                            ui.vertical(|ui| {
                                if !moved.is_empty() {
                                    soft(ui, format!("Moved back to it, without stopping: {}.", moved.join(", ")));
                                }
                                if !stopped.is_empty() {
                                    soft(ui, format!("Started on it again: {}.", stopped.join(", ")));
                                }
                            });
                        });
                    }
                    ui.add_space(12.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                        if dialog_button(ui, &pal, "wrench", "Exit Maintenance Mode", Look::Primary, true) {
                            maint = Some((*sid, host.clone(), name.clone(), Some(*back && !(moved.is_empty() && stopped.is_empty()))));
                        }
                    });
                });
            }
            Dialog::Update(d) => {
                let current = env!("CARGO_PKG_VERSION");
                dialog_window(ctx, "Update").show(ctx, |ui| {
                    ui.set_width(420.0);
                    ui.label(egui::RichText::new(format!("Simple VM Manager {} is out", d.release.version)).font(FontId::new(15.0, FontFamily::Name("bold".into()))).color(pal.ink));
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(format!("You have {}.", current)).color(pal.ink_soft));
                        if !d.release.page.is_empty() {
                            ui.hyperlink_to("What's new", &d.release.page);
                        }
                    });
                    ui.add_space(10.0);
                    let can = d.exe.is_some() && d.release.download.is_some();
                    if let Some(t) = &d.transfer {
                        // downloading: the bar, then "starting"
                        let (sent, total) = (t.sent.load(Ordering::Relaxed), t.total.load(Ordering::Relaxed).max(1));
                        let done = t.finishing.load(Ordering::Relaxed);
                        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
                        progress_bar(ui.painter(), r, (sent as f32 / total as f32).min(1.0), if done { pal.green } else { pal.blue }, &pal);
                        ui.add_space(4.0);
                        let what = if done { "Starting the new version…".to_string() } else { format!("Downloading…  {} of {}", fmt_bytes(sent as i64), fmt_bytes(total as i64)) };
                        ui.label(egui::RichText::new(what).size(12.5).color(pal.ink_soft));
                        ctx.request_repaint_after(Duration::from_millis(100));
                    } else if !can {
                        let why = if d.release.download.is_none() {
                            "There's no ready-made copy for this kind of computer: get it from the release page."
                        } else {
                            "This copy can't update itself: it was installed by the system's package manager, or runs from a build of the source. Update it the way you installed it, e.g. in your copy of the source: git pull && makepkg -si (or ./install.sh)."
                        };
                        ui.add(egui::Label::new(egui::RichText::new(why).color(pal.ink_soft)).wrap());
                    } else {
                        ui.add(egui::Label::new(egui::RichText::new("Update downloads it from GitHub, puts it in this one's place and opens it again. Your servers and settings stay as they are.").color(pal.ink_soft)).wrap());
                    }
                    if let Some(e) = &d.error {
                        ui.add_space(6.0);
                        ui.add(egui::Label::new(egui::RichText::new(format!("Couldn't update. {}", e)).color(pal.red)).wrap());
                    }
                    if let (Some(why), true, None) = (blocked, can, &d.transfer) {
                        ui.add_space(6.0);
                        ui.add(egui::Label::new(egui::RichText::new(why).size(12.5).color(pal.caution_ink)).wrap());
                    }
                    ui.add_space(14.0);
                    dialog_buttons(ui, |ui| {
                        if let Some(t) = &d.transfer {
                            if !t.finishing.load(Ordering::Relaxed) && dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) {
                                t.cancel.store(true, Ordering::Relaxed);
                            }
                            return;
                        }
                        if can {
                            if dialog_button(ui, &pal, "close", "Later", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                                close = true;
                            }
                            let label = if d.error.is_some() { "Try again" } else { "Update" };
                            if dialog_button(ui, &pal, "download", label, Look::Primary, blocked.is_none()) {
                                start_update = true;
                            }
                        } else {
                            if dialog_button(ui, &pal, "close", "Close", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                                close = true;
                            }
                            if !d.release.page.is_empty() && dialog_button(ui, &pal, "globe", "Open the release page", Look::Primary, true) {
                                ctx.open_url(egui::OpenUrl::new_tab(&d.release.page));
                                close = true;
                            }
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
                            egui::Frame::none().stroke(Stroke::new(1.0_f32, pal.border)).rounding(Rounding::same(8.0)).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.spacing_mut().item_spacing.y = 0.0;
                                for (n, (h, u)) in rows.iter().map(|k| (&k.0, &k.1)).enumerate() {
                                    if n > 0 {
                                        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
                                        ui.painter().line_segment([r.left_center(), r.right_center()], Stroke::new(1.0_f32, pal.border));
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
                    ui.add_space(18.0);

                    heading(ui, "Storage");
                    ui.checkbox(&mut repair_on, "Mount storage again when it's found unmounted");
                    ui.add_space(2.0);
                    ui.add(egui::Label::new(egui::RichText::new("As after a server started while the share was off. Each is tried once while it stays so; Logs says how it went.").size(12.0).color(pal.ink_soft)).wrap());
                    ui.add_space(18.0);

                    heading(ui, "Updates");
                    ui.checkbox(&mut updates_on, "Look for a new version when the app starts");
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let label = if update_checking { "Looking…" } else { "Check now" };
                        if flat_button(ui, "rescan", label, None, !update_checking, false, &pal).on_hover_text("Ask GitHub for the latest version").clicked() {
                            check_now = true;
                        }
                        if let Some(n) = &update_note {
                            ui.add(egui::Label::new(egui::RichText::new(n.as_str()).size(12.5).color(pal.ink_soft)).wrap());
                        } else {
                            ui.label(egui::RichText::new(format!("This is version {}.", env!("CARGO_PKG_VERSION"))).size(12.5).color(pal.ink_soft));
                        }
                    });
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
            Dialog::ConfirmEditVm { sid, vm, change, title, text } => {
                dialog_window(ctx, title.as_str()).show(ctx, |ui| {
                    ui.set_width(420.0);
                    ui.add(egui::Label::new(text.as_str()).wrap());
                    ui.add_space(12.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "close", "Cancel", Look::Plain, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }
                        if dialog_button(ui, &pal, "trash", "Remove", Look::Danger, true) {
                            edit_now = Some((*sid, vm.clone(), change.clone()));
                        }
                    });
                });
            }
            Dialog::LogDetail { copied, .. } => match &detail {
                None => close = true,
                Some(e) => {
                    let running = e.status == "In progress";
                    dialog_window(ctx, if e.progress.is_some() { "Activity" } else { "Log Entry" }).show(ctx, |ui| {
                        ui.set_width(560.0);
                        ui.spacing_mut().item_spacing.y = 6.0;
                        // what happened, after its result's dot (a ring while it runs)
                        ui.horizontal_top(|ui| {
                            let (r, _) = ui.allocate_exact_size(vec2(18.0, 20.0), Sense::hover());
                            let icon = Rect::from_center_size(r.center(), vec2(14.0, 14.0));
                            if running {
                                paint_icon(ui.painter(), icon, IconKind::Busy(Tone::Accent), &pal, pal.surface);
                            } else {
                                ui.painter().circle_filled(icon.center(), 4.5, status_color(&e.status, &pal));
                            }
                            ui.add(egui::Label::new(egui::RichText::new(e.text.as_str()).font(FontId::new(14.0, FontFamily::Name("bold".into()))).color(pal.ink)).wrap());
                        });
                        let mut facts = vec![("Server", e.server.clone()), ("About", e.obj.clone()), ("At", e.time.clone())];
                        facts.push(("Result", if running { format!("Under way for {}", fmt_took(e.started.elapsed())) } else { e.status.clone() }));
                        let fraction = detail_fraction.or_else(|| e.progress.as_ref().and_then(|p| p.fraction()));
                        if let (true, Some(f)) = (running, fraction) {
                            facts.push(("Progress", format!("{}%", (f * 100.0).round())));
                        }
                        if let Some(quiet) = e.progress.as_ref().and_then(|p| p.silent_for()).filter(|d| running && d.as_secs() >= 5) {
                            facts.push(("Server", format!("no answer for {} s; given up at {} s", quiet.as_secs(), LOST_AFTER.as_secs())));
                        }
                        egui::Grid::new("log-facts").num_columns(2).spacing(vec2(16.0, 4.0)).show(ui, |ui| {
                            for (k, v) in facts.iter().filter(|(_, v)| !v.is_empty()) {
                                ui.label(egui::RichText::new(*k).color(pal.ink_soft));
                                ui.add(egui::Label::new(v.as_str()).wrap());
                                ui.end_row();
                            }
                        });
                        ui.add_space(6.0);
                        // the calls made to the server, and its answers; a call left without one
                        // (the demo's) gets the result
                        let lines = e.progress.as_ref().map(|p| p.trace.lines()).unwrap_or_default();
                        if lines.is_empty() {
                            ui.label(egui::RichText::new(if running { "Nothing asked of the server yet." } else { "Nothing was asked of the server for this." }).color(pal.ink_soft));
                        } else {
                            let shown = Trace::default();
                            for (k, l) in &lines {
                                shown.push(*k, l.clone());
                            }
                            if !running && lines.last().map_or(false, |l| l.0 == Said::Asked) {
                                shown.push(if e.status == "Failed" { Said::Error } else { Said::Answer }, if e.status == "Failed" { e.text.clone() } else { "OK".into() });
                            }
                            crate::newvm::transcript(ui, &pal, &shown, copied, running, None);
                        }
                        if running {
                            ui.ctx().request_repaint_after(Duration::from_millis(500));
                        }
                        ui.add_space(8.0);
                        dialog_buttons(ui, |ui| {
                            if dialog_button(ui, &pal, "check", "Close", Look::Primary, true) || ui.input(|i| i.key_pressed(Key::Escape)) {
                                close = true;
                            }
                        });
                    });
                }
            },
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
        // (the screenshot of an update under way: it starts once the list is read)
        // (and one that stops: a VM can't move, and none is to be shut down)
        let shot_update = matches!(self.shot_show.as_str(), "pool-updating" | "pool-update-stopped");
        if self.shot_path.is_some() && shot_update && matches!(pool_action, PoolUpdateAction::None) {
            if let Some(v) = pool_view.as_ref().filter(|v| v.run.is_none()) {
                if let Some(plan) = v.plan(true, if self.shot_show == "pool-updating" { crate::pool_update::Stuck::Carry } else { crate::pool_update::Stuck::Keep }) {
                    pool_action = PoolUpdateAction::Install(plan);
                }
            }
        }
        match move_action {
            Some((_, MoveAction::Close)) => close = true,
            Some((sid, MoveAction::Move(m))) => {
                close = true;
                self.move_vm(sid, m, ctx);
            }
            _ => {}
        }
        match pool_action {
            PoolUpdateAction::None => {}
            PoolUpdateAction::Close => close = true,
            PoolUpdateAction::Pick(sid) => self.dialog = Dialog::PoolUpdate(Box::new(PoolUpdateDialog::new(sid))),
            PoolUpdateAction::Reread => {
                if let Dialog::PoolUpdate(d) = &self.dialog {
                    let sid = d.sid;
                    self.pool_updates.remove(&sid);
                }
            }
            PoolUpdateAction::Check => {
                if let Dialog::PoolUpdate(d) = &self.dialog {
                    let sid = d.sid;
                    self.sync_pool(sid, ctx);
                }
            }
            PoolUpdateAction::Install(plan) => {
                if let Dialog::PoolUpdate(d) = &self.dialog {
                    let sid = d.sid;
                    self.update_pool(sid, plan, ctx);
                }
            }
        }
        if close {
            self.dialog = Dialog::None;
        }
        // from the Close question: stay (the dialog that was open comes back), or go
        match close_app {
            Some(false) => {
                if let Dialog::ConfirmClose { back, .. } = std::mem::replace(&mut self.dialog, Dialog::None) {
                    self.dialog = *back;
                }
            }
            Some(true) => {
                self.closing = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            None => {}
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
        if let Some((sid, st)) = storage {
            self.start_storage(sid, st, ctx);
        }
        if let Some((sid, nw)) = network {
            self.start_network(sid, nw, ctx);
        }
        if let Some((sid, net, name)) = delete_net {
            self.dialog = Dialog::None;
            self.delete_network(sid, net, name, ctx);
        }
        if let Some((sid, server)) = browse {
            self.browse_exports(sid, server, ctx);
        }
        if let Some((sid, sr, lib)) = reattach {
            self.reattach_job(sid, sr, lib, ctx);
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
        if updates_on != self.check_updates {
            self.check_updates = updates_on;
            self.save_settings();
        }
        if repair_on != self.auto_repair {
            self.auto_repair = repair_on;
            self.auto_tried.clear();
            self.save_settings();
        }
        if check_now {
            self.update_note = None;
            self.look_for_update(true, ctx);
        }
        if start_update {
            self.install_update(ctx);
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
        if let Some((sid, host, name, reboot)) = shutdown_host {
            self.dialog = Dialog::None;
            self.shutdown_host(sid, host, name, reboot, ctx);
        }
        if let Some((sid, host, name, exit)) = maint {
            self.dialog = Dialog::None;
            self.maintenance(sid, host, name, exit, ctx);
        }
        if save_rules {
            self.save_alert_rules(ctx);
        }
        if let Some((sid, vm, name, disks)) = delete_vm {
            self.dialog = Dialog::None;
            self.delete_vm(sid, vm, name, disks, ctx);
        }
        if let Some((sid, vm, name, description, memory)) = take_snap {
            self.dialog = Dialog::None;
            self.take_snapshot(sid, vm, name, description, memory, ctx);
        }
        if let Some((sid, snap, first)) = revert_to {
            self.dialog = Dialog::None;
            self.revert(sid, snap, first, ctx);
        }
        if let Some((sid, snap)) = delete_snap {
            self.dialog = Dialog::None;
            self.delete_snapshot(sid, snap, ctx);
        }
        if let Some((sid, vm, source, name, description)) = make_tpl {
            self.dialog = Dialog::None;
            self.make_template(sid, vm, source, name, description, ctx);
        }
        if let Some((sid, vm, name, sr)) = copy_to {
            self.dialog = Dialog::None;
            self.copy_vm(sid, vm, name, sr, ctx);
        }
        if let Some((sid, tpl)) = delete_tpl {
            self.dialog = Dialog::None;
            self.delete_template(sid, tpl, ctx);
        }
        if let Some((sid, vm, change)) = edit_now {
            self.dialog = Dialog::None;
            self.edit_vm(sid, vm, change, ctx);
        }
        if let Some((sid, refs, name)) = leftover {
            self.dialog = Dialog::None;
            for r in refs {
                self.delete_vm(sid, r, name.clone(), true, ctx);
            }
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
        // SVM_SHOT_WAIT: at least this many seconds first (to see something long, like an update)
        let wait = std::env::var("SVM_SHOT_WAIT").ok().and_then(|w| w.parse::<u64>().ok()).unwrap_or(0);
        let timed_out = self.started.elapsed() > Duration::from_secs(20 + wait);
        let settled = self.creating == 0
            && self.pending_select.is_none()
            && self.sr_checking.is_empty()
            && self.sr_busy.is_empty()
            && self.auto_queue.is_empty()
            // nothing under way, and no look at the server due (one is right after a job)
            && self.vm_saving.is_empty()
            && self.edit_queue.is_empty()
            && self.servers.iter().all(|s| (s.busy.is_empty() || matches!(self.shot_show.as_str(), "activities-moving" | "close-asked")) && !s.fetching && s.next_refresh > Instant::now());
        if !self.shot_requested && ((self.shot_ready && self.frames >= 120 && settled && self.started.elapsed() >= Duration::from_secs(wait)) || timed_out) {
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
        // a newer version found: offered when nothing else is asked
        if matches!(self.dialog, Dialog::None) {
            if let Some(release) = self.pending_update.take() {
                self.dialog = Dialog::Update(Box::new(UpdateDialog { release, exe: update::replaceable(), transfer: None, error: None }));
            }
        }
        // Ctrl + / Ctrl - change the size too: remembered like the choice in Settings
        let zoom = ctx.zoom_factor();
        if (zoom - self.zoom).abs() > 0.001 {
            self.zoom = zoom;
            self.save_settings();
        }
        let pal = self.pal;
        self.tick(ctx);
        self.poll_pool_updates(ctx);
        self.poll_messages(ctx);
        self.note_app_alerts();
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
        // closing while a job would be left half done (an upload, a VM being made, a pool's
        // update…): asked first; what's one task on the server goes on by itself, unasked
        if !self.closing && self.shot_path.is_none() && ctx.input(|i| i.viewport().close_requested()) {
            let items = self.halfway();
            if !items.is_empty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                let back = match std::mem::replace(&mut self.dialog, Dialog::None) {
                    Dialog::ConfirmClose { back, .. } => back,
                    other => Box::new(other),
                };
                self.dialog = Dialog::ConfirmClose { items, back };
            }
        }
        // --show add-server | confirm | about: a check's screenshot with that dialog open.
        if self.shot_ready && !self.shot_dialog {
            self.shot_dialog = true;
            match self.shot_show.as_str() {
                "add-server" => out.push(Intent::AddServer),
                "confirm" => out.extend(ACTIONS.iter().position(|a| a.confirm && self.can_run(a, &sel)).map(Intent::Action)),
                "about" => out.push(Intent::About),
                "sr-detached" | "sr-repair-failed" => out.extend(sel.sid.map(|sid| Intent::RepairSr(sid, "sr-iso".into()))),
                "snapshots" => out.push(Intent::Tab(Tab::Snapshots)),
                "vm-performance" | "host-performance" => out.push(Intent::Tab(Tab::Performance)),
                "alerts" | "alerts-all" => {
                    out.push(Intent::Alerts);
                    self.alerts_view.every = self.shot_show == "alerts-all";
                    self.alerts_view.open = Some("msg:OpaqueRef:demo-message-2".into());
                }
                "alert-rules" => out.extend(sel.sid.map(|sid| Intent::AlertRules(sid, "VM", "vm-web".into(), "Web Server".into(), Watched::Vm))),
                // (one dismissed as the window opens: it's gone from the page, the count and the server)
                "alerts-dismissed" => {
                    out.push(Intent::Alerts);
                    out.extend(sel.sid.map(|sid| Intent::DismissAlerts(vec![(sid, "OpaqueRef:demo-message-1".into())])));
                }
                "details" | "host-details" | "pool-details" => out.push(Intent::Tab(Tab::Details)),
                "pool-updates" | "pool-updating" | "pool-update-stopped" => out.push(Intent::PoolUpdates(sel.sid)),
                "move-vm" => out.extend(sel.sid.map(|sid| Intent::MoveVm(sid, "vm-web".into(), Some("host-b".into())))),
                "move-vm-local" => out.extend(sel.sid.map(|sid| Intent::MoveVm(sid, "vm-mail".into(), Some("host-a".into())))),
                "move-vm-off" => out.extend(sel.sid.map(|sid| Intent::MoveVm(sid, "vm-desk".into(), Some("host-b".into())))),
                "new-vm-template" => out.extend(sel.sid.map(|sid| Intent::NewVmFromTemplate(sid, "tpl-web".into()))),
                "make-template" => out.extend(sel.sid.map(|sid| Intent::MakeTemplate(sid, "vm-desk".into()))),
                "make-template-snapshot" => {
                    out.push(Intent::Tab(Tab::Snapshots));
                    out.extend(sel.sid.map(|sid| Intent::MakeTemplate(sid, "vm-web-snap-2".into())));
                }
                "copy-vm" => out.extend(sel.sid.map(|sid| Intent::CopyVm(sid, "vm-desk".into()))),
                "log-detail" => {
                    // a copy that lost its server part way, as it'd be logged
                    if let Some(sid) = sel.sid {
                        let text = "Copying VM 'Test Desktop' as 'Test Desktop (copy)' onto Shared NFS storage: demo-host-01 stopped answering for 100 seconds (it may have lost power), so this was given up on.";
                        let i = self.add_log(sid, "Demo Pool", Some("vm-desk"), "Test Desktop", text, "In progress");
                        let p = self.progress(i);
                        p.trace.push(Said::Asked, "VM.copy  OpaqueRef:5f0c2a9e-vm-desk  \"Test Desktop (copy)\"  OpaqueRef:8b1d44c0-sr-nfs");
                        p.trace.push(Said::Note, "No answer from the server while asking how it's going: Couldn't connect to 10.20.0.11: timed out");
                        p.trace.push(Said::Error, "Error code: LOST_CONTACT");
                        p.trace.push(Said::Error, "demo-host-01");
                        self.log[i].status = "Failed".into();
                        self.dialog = Dialog::LogDetail { index: i, copied: None };
                    }
                }
                "vm-edited" => {
                    // real changes made in the demo, one after another, as the Overview makes them
                    if let Some(sid) = sel.sid {
                        let vm = "vm-desk".to_string();
                        for change in [
                            VmChange::Description("Windows 11, for trying updates first".into()),
                            VmChange::Resources { vcpus: 4, cores_per_socket: 4, memory: 6 * 1024 * 1024 * 1024 },
                            VmChange::AddDisk { name: "Test Desktop disk 1".into(), size: 20 * 1024 * 1024 * 1024, sr: "sr-local1".into() },
                            VmChange::Dvd { vbd: "vm-desk-vbd-3".into(), iso: Some("sr-iso-vdi-4".into()) },
                            VmChange::AddNic { net: "net-1".into() },
                            VmChange::AutoPowerOn(true),
                        ] {
                            self.edit_vm(sid, vm.clone(), change, ctx);
                        }
                    }
                }
                "close-asked" => {
                    // closing while a shut-down VM's disk is being copied and a snapshot reverted
                    if let Some(sid) = sel.sid {
                        let vdi = self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.vm("vm-desk")).and_then(|v| v.disks.iter().find(|k| k.kind == "Disk").map(|k| k.vdi.clone()));
                        if let Some(vdi) = vdi {
                            self.move_vm(sid, MoveVm { vm: "vm-desk".into(), host: "host-b".into(), disks: vec![(vdi, "sr-nfs".into())], eject: None, running: false }, ctx);
                        }
                        self.revert(sid, "vm-web-snap-2".into(), false, ctx);
                        let items = self.halfway();
                        self.dialog = Dialog::ConfirmClose { items, back: Box::new(Dialog::None) };
                    }
                }
                "activities-moving" => {
                    // a running VM going to the other server with its disk, still under way when
                    // the screenshot's taken (with SVM_DEMO_SLOW and SVM_SHOT_WAIT)
                    if let Some(sid) = sel.sid {
                        let vdi = self.server(sid).and_then(|s| s.data.as_ref()).and_then(|d| d.vm("vm-mail")).and_then(|v| v.disks.iter().find(|k| k.kind == "Disk").map(|k| k.vdi.clone()));
                        if let Some(vdi) = vdi {
                            self.move_vm(sid, MoveVm { vm: "vm-mail".into(), host: "host-a".into(), disks: vec![(vdi, "sr-local1".into())], eject: None, running: true }, ctx);
                        }
                    }
                }
                "activities" => {
                    // two real jobs in the demo, for Activities to show
                    if let Some(sid) = sel.sid {
                        self.copy_vm(sid, "vm-desk".into(), "Test Desktop (copy)".into(), Some("sr-nfs".into()), ctx);
                        self.take_snapshot(sid, "vm-web".into(), "Before nginx 1.26".into(), String::new(), true, ctx);
                    }
                }
                "template-made" => {
                    // a real one made in the demo, as the dialog's Make Template does
                    if let Some(sid) = sel.sid {
                        self.make_template(sid, "vm-desk".into(), "vm-desk".into(), "Test Desktop template".into(), "Windows 11, sysprepped".into(), ctx);
                    }
                }
                "snapshot-taken" => {
                    // a real one taken in the demo, as the dialog's Take Snapshot does
                    out.push(Intent::Tab(Tab::Snapshots));
                    if let Some(sid) = sel.sid {
                        self.take_snapshot(sid, "vm-web".into(), "Before nginx 1.26".into(), "Config backed up in /root".into(), true, ctx);
                    }
                }
                "snapshot-take" | "snapshot-revert" | "snapshot-delete" => {
                    out.push(Intent::Tab(Tab::Snapshots));
                    if let Some(sid) = sel.sid {
                        out.push(match self.shot_show.as_str() {
                            "snapshot-take" => Intent::TakeSnapshot(sid, "vm-web".into()),
                            "snapshot-revert" => Intent::AskRevert(sid, "vm-web-snap-2".into()),
                            _ => Intent::AskDeleteSnapshot(sid, "vm-web-snap-3".into()),
                        });
                    }
                }
                "sr-auto-repair" => out.push(Intent::Tab(Tab::Logs)),
                "console" => out.push(Intent::Tab(Tab::Console)),
                "update" | "update-progress" => {
                    let release = update::Release { version: "9.9.0".into(), page: "https://github.com/jpaferreira-git/simple-vm-manager/releases".into(), download: Some(("https://example.invalid/a.tar.gz".into(), 9_540_306)) };
                    let transfer = (self.shot_show == "update-progress").then(|| {
                        let t = Arc::new(Transfer::default());
                        t.total.store(9_540_306, Ordering::Relaxed);
                        t.sent.store(4_100_000, Ordering::Relaxed);
                        t
                    });
                    self.dialog = Dialog::Update(Box::new(UpdateDialog { release, exe: Some(PathBuf::from("/tmp/simple-vm-manager")), transfer, error: None }));
                }
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
                "iso" => out.push(Intent::NewIsoLibrary),
                "iso-library" => out.push(Intent::IsoLibrary),
                "templates" => out.push(Intent::Templates),
                "networks" => out.push(Intent::Networks),
                "delete-network" => {
                    out.push(Intent::Networks);
                    out.extend(sel.sid.or(self.servers.first().map(|s| s.id)).map(|sid| Intent::AskDeleteNetwork(sid, "net-2".into())));
                }
                "networks-sorted" => {
                    // sorted by MTU, the largest first (as two clicks on its heading do)
                    out.push(Intent::Networks);
                    ctx.data_mut(|d| d.insert_temp(egui::Id::new(("table-sort", "all-nets")), Some((3usize, false))));
                }
                "new-template" => out.push(Intent::NewTemplate),
                "new-network" | "new-network-error" => {
                    out.push(Intent::Networks);
                    let pools: Vec<(u32, String)> = self.servers.iter().filter(|s| s.data.is_some()).map(|s| (s.id, s.name())).collect();
                    if let Some(&(sid, _)) = pools.first() {
                        let error = self.shot_show == "new-network-error";
                        // (VLAN 666 is taken in the demo)
                        let mut d = NetworkDialog::new(sid, pools, self.network_cards()).filled("VNET-iot", if error { "666" } else { "30" }, "192.168.30.0/24");
                        if error {
                            if let NetworkAction::Create(sid, nw) = d.submit() {
                                self.start_network(sid, nw, ctx);
                            }
                        }
                        self.dialog = Dialog::NewNetwork(Box::new(d));
                    }
                }
                "vm-storage" => out.push(Intent::VmStorage),
                "new-storage" | "new-storage-inside" | "new-storage-error" => {
                    out.push(Intent::VmStorage);
                    if let Some(sid) = self.servers.iter().find(|s| s.data.is_some()).map(|s| s.id) {
                        let pools = vec![(sid, self.server(sid).map(|s| s.name()).unwrap_or_default(), false)];
                        let found = vec!["/mnt/tank/backups".to_string(), "/mnt/tank/isos".into(), "/mnt/tank/vms".into()];
                        let mut d = match self.shot_show.as_str() {
                            "new-storage" => StorageDialog::new(sid, pools).filled("nas:/mnt/tank/vms", Some(found)),
                            // the address of one of the demo's VMs
                            "new-storage-inside" => StorageDialog::new(sid, pools).filled("10.20.1.22:/mnt/tank/vms", None),
                            _ => StorageDialog::new(sid, pools).filled("unreachable:/mnt/tank/vms", None),
                        };
                        if self.shot_show == "new-storage-error" {
                            if let StorageAction::Create(sid, st) = d.submit() {
                                self.start_storage(sid, st, ctx);
                            }
                        }
                        self.dialog = Dialog::NewStorage(Box::new(d));
                    }
                }
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
                "host-shutdown" | "host-restart" => {
                    if let Some(sid) = sel.sid {
                        out.push(Intent::AskShutdownHost(sid, "host-a".into(), self.shot_show == "host-restart"));
                    }
                }
                "host-maintenance" | "host-exit-maintenance" => {
                    if let Some(sid) = sel.sid {
                        out.push(Intent::AskMaintenance(sid, if self.shot_show == "host-maintenance" { "host-a" } else { "host-b" }.into()));
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
                "vm-rename" => {
                    self.ov = OverviewEdit { vm: "vm-web".into(), name: Some("Web Server".into()), focus: true, ..Default::default() };
                }
                "iso-describe" => {
                    out.push(Intent::Tab(Tab::IsoFiles));
                    self.iso_edit = Some(IsoEdit { vdi: "sr-iso-vdi-1".into(), text: "Rocky Linux 9, minimal install".into(), focus: true });
                }
                _ => {}
            }
        }

        let tools_frame = egui::Frame::none().fill(pal.toolbar).inner_margin(egui::Margin::symmetric(6.0, 4.0)).stroke(Stroke::new(1.0_f32, pal.border));
        egui::TopBottomPanel::top("tools").frame(tools_frame).show(ctx, |ui| self.toolbar(ui, &sel, &mut out));

        // The sidebar folds to a narrow strip of icons, with a short animation.
        let rail = egui::SidePanel::left("rail").resizable(false).exact_width(RAIL_WIDTH).frame(egui::Frame::none().fill(pal.rail).stroke(Stroke::new(1.0_f32, pal.border)));
        let full = egui::SidePanel::left("tree").resizable(true).default_width(290.0).width_range(200.0..=560.0).frame(egui::Frame::none().fill(pal.sidebar).stroke(Stroke::new(1.0_f32, pal.border)));
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
            // (a VM's or a server's console is a button at the row's end, not a tab among the others)
            let shown: Vec<Tab> = tabs.iter().copied().filter(|t| *t != Tab::Console && *t != Tab::Alerts).collect();
            // the least room they take: all in one Actions menu (a connection's, its one button)
            let least = if self.root_of(&sel).is_some() { actions } else { (ACTIONS_BTN + TOOL_GAP + SEP_W + 8.0).min(actions) };
            // (the Alerts page's Dismiss all: room for it, its icon at least)
            let page = if sel.kind == Kind::Root && current == Tab::Alerts { SQUARE.x + SEP_W + 12.0 } else { 0.0 };
            let free = self.header_and_tabs(ui, &sel, &shown, current, (actions.max(page), least.max(page)), &mut out);
            if sel.kind == Kind::Root && current == Tab::Alerts {
                self.alerts_actions(ui, free, &mut out);
            } else if actions > 0.0 {
                self.tab_actions(ui, free, &sel, current, actions, &mut out);
            } else if sel.kind == Kind::Root && current == Tab::IsoLibrary {
                self.iso_library_actions(ui, free, &mut out);
            } else if sel.kind == Kind::Root && matches!(current, Tab::VmStorage | Tab::Templates | Tab::Networks) {
                // the page's one action: adding to it
                let (label, tip, intent) = match current {
                    Tab::VmStorage => ("New VM Storage", "Add an NFS folder that every server of a pool shares, for VMs' disks", Intent::NewStorage),
                    Tab::Templates => ("New Template", "Make a template from one of your VMs, to make new VMs from", Intent::NewTemplate),
                    _ => ("New Network", "Add a network for VMs' cards: a VLAN on a network card, or a private one", Intent::NewNetwork),
                };
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(free).layout(egui::Layout::right_to_left(egui::Align::Center)));
                child.spacing_mut().item_spacing.x = 4.0;
                let has_pool = self.servers.iter().any(|s| s.data.is_some());
                let compact = free.width() < flat_button_width(&child, label) + 16.0 + SEP_W;
                if flat_button(&mut child, "plus", label, Some(pal.accent), has_pool, compact, &pal).on_hover_text(tip).clicked() {
                    out.push(intent);
                }
                vsep(&mut child, &pal);
            } else if let Some((s, t)) = self.template_of(&sel) {
                // a template's: New VM from it, and Delete
                let (sid, tref, busy) = (s.id, t.rref.clone(), s.busy.contains_key(&t.rref));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(free).layout(egui::Layout::right_to_left(egui::Align::Center)));
                child.spacing_mut().item_spacing.x = 4.0;
                let compact = free.width() < flat_button_width(&child, "Delete") + flat_button_width(&child, "New VM") + 16.0 + SEP_W;
                if flat_button(&mut child, "trash", "Delete", Some(pal.red), !busy, compact, &pal).on_hover_text("Delete this template and its disks; VMs made from it aren't touched").clicked() {
                    out.push(Intent::AskDeleteTemplate(sid, tref.clone()));
                }
                if flat_button(&mut child, "newvm", "New VM", Some(pal.accent), !busy, compact, &pal).on_hover_text("Make a VM from this template").clicked() {
                    out.push(Intent::NewVmFromTemplate(sid, tref));
                }
                vsep(&mut child, &pal);
            } else if let Some((s, x, trouble)) = self.sr_of(&sel).map(|(s, x)| (s, x, s.data.as_ref().and_then(|d| d.sr_trouble(x)))).filter(|(_, x, t)| x.is_iso() || t.is_some()) {
                // an ISO library's actions, and Repair for storage that isn't mounted somewhere,
                // next to its tabs
                let (sid, sr, iso, broken) = (s.id, x.rref.clone(), x.is_iso(), trouble.is_some());
                let busy = self.sr_busy.contains(&sr);
                let uploading = self.uploads.get(&sr).map_or(false, Upload::active);
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(free).layout(egui::Layout::right_to_left(egui::Align::Center)));
                child.spacing_mut().item_spacing.x = 4.0;
                // with their names when there's room, otherwise as icons
                let mut need = flat_button_width(&child, "Working…") + 16.0 + SEP_W;
                if iso {
                    need += flat_button_width(&child, "Disconnect") + flat_button_width(&child, "Upload");
                }
                let compact = free.width() < need;
                if iso && flat_button(&mut child, "disconnect", "Disconnect", Some(pal.red), !busy && !uploading, compact, &pal).on_hover_text("Stop using this library; nothing on the share is deleted").clicked() {
                    out.push(Intent::AskForgetSr(sid, sr.clone()));
                }
                if broken {
                    if flat_button(&mut child, "connect", if busy { "Working…" } else { "Repair" }, Some(pal.green), !busy, compact, &pal).on_hover_text("Mount it again on the servers where it isn't").clicked() {
                        out.push(Intent::RepairSr(sid, sr.clone()));
                    }
                } else if flat_button(&mut child, "rescan", if busy { "Working…" } else { "Rescan" }, None, !busy, compact, &pal).on_hover_text("Look for ISO files added to or removed from the share").clicked() {
                    out.push(Intent::RescanSr(sid, sr.clone()));
                }
                let tip = if broken { "It isn't mounted: Repair it first" } else { "Add an .iso file from this computer or from a link on the web" };
                if iso && flat_button(&mut child, "upload", "Upload", None, !busy && !uploading && !broken, compact, &pal).on_hover_text(tip).on_disabled_hover_text(tip).clicked() {
                    out.push(Intent::Upload(sid, sr));
                }
                vsep(&mut child, &pal);
            } else if let Some((s, x)) = self.sr_of(&sel).filter(|(_, x)| !x.is_iso() && x.kind != "udev" && x.size > 0) {
                // storage for disks: made the pool's default (where new disks, suspended VMs'
                // memory and crash dumps go)
                let (sid, sr) = (s.id, x.rref.clone());
                let default = s.data.as_ref().map_or(false, |d| d.default_sr.as_deref() == Some(sr.as_str()) && d.suspend_sr.as_deref().map_or(true, |r| r == sr));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(free).layout(egui::Layout::right_to_left(egui::Align::Center)));
                child.spacing_mut().item_spacing.x = 4.0;
                let compact = free.width() < flat_button_width(&child, "Make Default") + flat_button_width(&child, "Alerts…") + 20.0 + SEP_W;
                let (label, tip) = if default {
                    ("Default", "This is the pool's default storage")
                } else {
                    ("Make Default", "Make this the pool's default storage: for new VMs' disks, suspended VMs' memory and crash dumps (moving a running VM's disks uses it too)")
                };
                if flat_button(&mut child, "check", label, Some(pal.green), !default, compact, &pal).on_hover_text(tip).on_disabled_hover_text(tip).clicked() {
                    out.push(Intent::MakeDefaultSr(sid, sr.clone()));
                }
                let tip = "Get an alert when it's fuller than a limit you set (the pool watches, also while this app is closed)";
                if flat_button(&mut child, "bell", "Alerts…", Some(pal.orange), true, compact, &pal).on_hover_text(tip).clicked() {
                    out.push(Intent::AlertRules(sid, "SR", sr, x.name.clone(), Watched::Sr));
                }
                vsep(&mut child, &pal);
            }
            ui.spacing_mut().item_spacing.y = 4.0;
            let margin = if current == Tab::General || current == Tab::Home { 0.0 } else { 10.0 };
            egui::Frame::none().inner_margin(egui::Margin::symmetric(margin, 0.0)).show(ui, |ui| match current {
                Tab::Home => self.home(ui, &mut out),
                Tab::Search => self.search_tab(ui, &mut out),
                Tab::General | Tab::Details => self.general(ui, &sel, &mut out),
                Tab::Overview if sel.kind == Kind::Host => self.host_overview(ui, &sel, &mut out),
                Tab::Overview if sel.kind == Kind::Pool => self.pool_overview(ui, &sel, &mut out),
                Tab::Overview => self.overview(ui, &sel, &mut out),
                Tab::Performance => self.performance(ui, &sel, &mut out),
                Tab::Alerts => self.alerts_page(ui, &mut out),
                Tab::Storage => self.storage(ui, &sel),
                Tab::Networking => self.networking(ui, &sel),
                Tab::Console => self.console_tab(ui, &sel, &mut out),
                Tab::IsoFiles => self.iso_files(ui, &sel, &mut out),
                Tab::IsoLibrary => self.iso_library_page(ui, &mut out),
                Tab::VmStorage => self.vm_storage_page(ui, &mut out),
                Tab::Networks => self.networks_page(ui, &mut out),
                Tab::Snapshots => self.snapshots_tab(ui, &sel, &mut out),
                Tab::Templates => self.templates_tab(ui, &sel, &mut out),
                Tab::Logs => self.logs(ui, &sel, &mut out),
            });
        });

        self.dialogs(ctx);
        self.apply(out, &sel, ctx);
        // files dragged over the window: say where they'd go
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let text = match self.drop_library(&sel) {
                Some((_, _, name)) => format!("Drop to upload to {}", name),
                None => "Select an ISO library first, then drop the file on it".to_string(),
            };
            let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop-files")));
            let r = ctx.screen_rect();
            p.rect_filled(r, 0.0, Color32::from_black_alpha(150));
            p.rect_stroke(r.shrink(14.0), Rounding::same(12.0), Stroke::new(2.0_f32, pal.accent));
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
    let stroke = Stroke::new(1.0_f32, if edit.has_focus() { pal.accent } else { pal.border_strong });
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
        _ if !filled => egui::Button::new("").stroke(Stroke::new(1.0_f32, pal.border_strong)),
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
        NOT_PLUGGED => format!("{} isn't mounted on the server the VM is for, so nothing was made. Repair, on its page, mounts it again; or pick another server, then try again.", e.details.first().map_or("The storage", |n| n.as_str())),
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

/// Why shutting a server down failed, in plain words where the server's answer is a known one.
/// The VMs running on server `host` that would leave it as it's emptied: those that move to the
/// pool's other servers, and those shut down (their disks on its own storage, or no other server
/// to take them). Their names.
fn leaving(d: &PoolData, host: &str) -> (Vec<String>, Vec<String>) {
    let others = d.hosts.iter().any(|o| o.rref != host && o.enabled && o.is_up());
    let mut moving = vec![];
    let mut stopping = vec![];
    for v in d.vms.iter().filter(|v| v.host.as_deref() == Some(host) && matches!(v.power, PowerState::Running | PowerState::Paused)) {
        if others && v.storage_host.as_deref() != Some(host) {
            moving.push(v.name.clone());
        } else {
            stopping.push(v.name.clone());
        }
    }
    (moving, stopping)
}

/// Whether pool `sid` has another server, on and taking VMs, for server `host`'s VMs to go to.
fn leaving_somewhere(servers: &[ServerEntry], sid: u32, host: &str) -> bool {
    servers.iter().find(|s| s.id == sid).and_then(|s| s.data.as_ref()).map_or(false, |d| d.hosts.iter().any(|o| o.rref != host && o.enabled && o.is_up()))
}

fn host_problem(e: &ApiError) -> String {
    match e.code.as_str() {
        "HOST_NOT_ENOUGH_FREE_MEMORY" | "NO_HOSTS_AVAILABLE" => "The pool's other servers haven't the memory free to take its VMs. Shut some VMs down, then try again.".into(),
        "HOST_NOT_LIVE" => "It isn't answering: it may be off already.".into(),
        "RBAC_PERMISSION_DENIED" | "PERMISSION_DENIED" => "Your account isn't allowed to do that.".into(),
        "OTHER_OPERATION_IN_PROGRESS" => "The server is busy with something else. Wait a moment, then try again.".into(),
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

/// A VM being dragged in the tree (onto a server, to move it there).
struct DragVm {
    sid: u32,
    vm: String,
    #[allow(dead_code)]
    name: String,
}

/// Storage's tree icon: a disc for ISO libraries, a drum for the rest.
/// A user's template in the tree (a ring turning while a VM is made from it or it's deleted).
fn template_node(sid: u32, s: &ServerEntry, t: &VmInfo) -> Node {
    let icon = if s.busy.contains_key(&t.rref) { IconKind::Busy(Tone::Accent) } else { IconKind::Template };
    Node::new(format!("tpl:{}:{}", sid, t.rref), t.name.clone(), icon, Kind::Template, Some(sid), Some(t.rref.clone()))
}

/// A storage's icon: a ring turning while it's rescanned, repaired or disconnected; otherwise a
/// green dot when it's mounted on a server that's up, a red one when its server is off or not
/// answering, a "!" when it isn't mounted where it should be. A server's own storage (local
/// disks, DVD drives, USB) has none.
/// Why Delete waits, from what the VM is busy with ("Moving…" → "It's moving: delete it once
/// that's done").
fn busy_tip(doing: &str) -> String {
    format!("It's {}: delete it once that's done", doing.trim_end_matches('…').to_lowercase())
}

/// A storage's kind in a word or two: "NFS (v3)", "LVM", "iSCSI"… (a server's own storage says
/// it's local where it says which server it's in)
fn sr_kind_words(x: &SrInfo) -> String {
    match x.kind.as_str() {
        "nfs" if !x.share.is_empty() => x.share.clone(),
        "nfs" => "NFS".into(),
        "lvm" => "LVM".into(),
        "ext" => "EXT".into(),
        "xfs" => "XFS".into(),
        "largeblock" => "LVM (4K)".into(),
        "file" => "Folder".into(),
        "lvmoiscsi" => "iSCSI".into(),
        "lvmohba" => "Fibre Channel".into(),
        "gfs2" => "GFS2 (shared disk)".into(),
        "smb" => "SMB".into(),
        "zfs" => "ZFS".into(),
        k => k.to_string(),
    }
}

fn sr_icon(d: &PoolData, x: &SrInfo, busy: bool) -> IconKind {
    if busy {
        return IconKind::Busy(Tone::Accent);
    }
    if x.is_local() {
        return if x.is_iso() { IconKind::IsoSr } else { IconKind::Sr };
    }
    let health = match d.sr_trouble(x) {
        Some(t) if t.nowhere => SrHealth::Nowhere,
        Some(_) => SrHealth::Partly,
        None if mounted_on_up(d, x).next().is_some() => SrHealth::Up,
        None => SrHealth::Off,
    };
    IconKind::Storage { iso: x.is_iso(), health }
}

/// The servers that are up and have the storage mounted.
fn mounted_on_up<'a>(d: &'a PoolData, x: &'a SrInfo) -> impl Iterator<Item = &'a HostInfo> {
    x.links.iter().filter(|l| l.1).filter_map(|l| d.host(&l.0)).filter(|h| h.is_up())
}

/// Where a storage is mounted, in words.
fn sr_status(d: &PoolData, x: &SrInfo) -> String {
    let name = |h: &str| match d.host(h) {
        Some(h) if h.is_master && d.is_pool() => format!("{} (coordinator)", h.name),
        Some(h) => h.name.clone(),
        None => h.to_string(),
    };
    match d.sr_trouble(x) {
        None => {
            let on: Vec<String> = mounted_on_up(d, x).map(|h| name(&h.rref)).collect();
            let down = x.links.iter().filter_map(|l| d.host(&l.0)).find(|h| !h.is_up());
            match (on.len(), down) {
                (0, Some(h)) if h.live => format!("Not available: {} isn't answering", h.name),
                (0, Some(h)) => format!("Not available: {} is off", h.name),
                (0, None) => "Not mounted".into(),
                (n, _) if n > 1 && n == d.hosts.len() => format!("Mounted on all {} servers", n),
                _ => format!("Mounted on {}", on.join(", ")),
            }
        }
        Some(t) if t.detached => "Detached: the pool has no settings for reaching it. Repair asks where it is.".into(),
        Some(t) if t.nowhere => "Not mounted on any server. Repair mounts it again.".into(),
        Some(t) => format!("Not mounted on {}. Repair mounts it there.", t.missing.iter().map(|h| name(h)).collect::<Vec<_>>().join(", ")),
    }
}

/// Why a storage couldn't be mounted again, in plain words where the mount's message is a known
/// one, then the server's own words.
fn mount_problem(e: &ApiError, nfs: bool) -> String {
    let plain = match mount_trouble(e) {
        Some(MountTrouble::Refused) if nfs => "The NFS server refused the XenServer hosts: check that its export still allows their addresses.",
        Some(MountTrouble::Refused) => "The share refused the user name and password the pool has for it. If they changed, disconnect the library and add it again with New ISO Library (on the ISO Library page).",
        Some(MountTrouble::NotFound) => "The share or its folder isn't there any more.",
        Some(MountTrouble::Unreachable) => "The XenServer hosts couldn't reach the share's address. It answering this computer isn't enough: the hosts have to reach it themselves.",
        Some(MountTrouble::Unsupported) => "The share no longer speaks the protocol version set for it.",
        None => return e.message(),
    };
    format!("{}\n\n{}", plain, e.message())
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
    /// A cell sorted by this rather than by what it shows (a date, a size; a Custom cell's text).
    By(SortKey, Box<Cell>),
}

/// What a column is sorted by: a number, or words (numbers in them as numbers: "VM 2" before
/// "VM 10").
#[derive(Clone, Debug, PartialEq)]
enum SortKey {
    Num(f64),
    Text(String),
}

impl Cell {
    /// Sorted by `n`.
    fn by_num(n: f64, c: Cell) -> Cell {
        Cell::By(SortKey::Num(n), Box::new(c))
    }
    /// Sorted by `t`.
    fn by_text(t: &str, c: Cell) -> Cell {
        Cell::By(SortKey::Text(t.to_lowercase()), Box::new(c))
    }
    /// The cell as drawn.
    fn plain(&self) -> &Cell {
        match self {
            Cell::By(_, c) => c.plain(),
            c => c,
        }
    }
    /// What it's sorted by; None for nothing ("—", empty, or drawn by the caller with no key),
    /// which goes last. A size ("4.8 GB") counts as its bytes, a plain number as a number.
    fn key(&self) -> Option<SortKey> {
        let t = match self {
            Cell::By(k, _) => return Some(k.clone()),
            Cell::Usage(f, _) => return Some(SortKey::Num(*f as f64)),
            Cell::Custom(_) => return None,
            Cell::Text(t) | Cell::Status(t) | Cell::Icon(_, t) => t.trim(),
        };
        if t.is_empty() || t == "—" {
            return None;
        }
        if let Some((n, unit)) = t.split_once(' ') {
            let mult = match unit {
                "B" => 1.0,
                "KB" => 1024.0,
                "MB" => 1024f64.powi(2),
                "GB" => 1024f64.powi(3),
                "TB" => 1024f64.powi(4),
                "PB" => 1024f64.powi(5),
                _ => 0.0,
            };
            if let (Ok(n), true) = (n.parse::<f64>(), mult > 0.0) {
                return Some(SortKey::Num(n * mult));
            }
        }
        match t.parse::<f64>() {
            Ok(n) => Some(SortKey::Num(n)),
            Err(_) => Some(SortKey::Text(t.to_lowercase())),
        }
    }
}

/// Two keys in order: numbers as numbers, words with the numbers in them as numbers.
fn key_order(a: &SortKey, b: &SortKey) -> std::cmp::Ordering {
    match (a, b) {
        (SortKey::Num(x), SortKey::Num(y)) => x.total_cmp(y),
        (SortKey::Num(_), SortKey::Text(_)) => std::cmp::Ordering::Less,
        (SortKey::Text(_), SortKey::Num(_)) => std::cmp::Ordering::Greater,
        (SortKey::Text(x), SortKey::Text(y)) => natural_order(x, y),
    }
}

/// Words in order, a run of digits counting as its number ("10.20.1.9" before "10.20.1.10").
fn natural_order(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let run = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(c) = it.peek().copied().filter(|c| c.is_ascii_digit()) {
                        s.push(c);
                        it.next();
                    }
                    s.trim_start_matches('0').to_string()
                };
                let (m, n) = (run(&mut a), run(&mut b));
                let o = m.len().cmp(&n.len()).then_with(|| m.cmp(&n));
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                a.next();
                b.next();
            }
        }
    }
}

fn text_rows(rows: Vec<Vec<String>>) -> Vec<Vec<Cell>> {
    rows.into_iter().map(|r| r.into_iter().map(Cell::Text).collect()).collect()
}

/// A frameless button, an icon then its label, with a soft background under the mouse. `tint`
/// colours the icon (red for one that takes something away); `compact` shows only the icon, with
/// the label as its tooltip.
pub(crate) fn flat_button(ui: &mut Ui, glyph: &str, label: &str, tint: Option<Color32>, enabled: bool, compact: bool, pal: &Pal) -> egui::Response {
    let font = FontId::proportional(13.0);
    let size = if compact { SQUARE } else { vec2(flat_button_width(ui, label), 26.0) };
    let (r, mut resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let bg = if enabled && resp.hovered() { pal.hover } else { pal.window };
    if enabled && resp.hovered() {
        ui.painter().rect_filled(r, Rounding::same(if compact { 6.0 } else { 5.0 }), pal.hover);
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
/// A compact flat button's size (an icon alone, its name as its tip).
const SQUARE: egui::Vec2 = vec2(32.0, 30.0);
/// The size of every button at the tab row's end (power, Take Snapshot, Copy, Delete, Restart,
/// Shut Down, the keyboard): room for an icon and a menu's arrow.
const TOOL_BTN: egui::Vec2 = vec2(46.0, 30.0);
/// The gap between them.
const TOOL_GAP: f32 = 4.0;
/// A separator before a group of buttons, with its gap.
const SEP_W: f32 = 11.0;

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

/// Why Copy VM and Make Template wait for a VM to be off.
const SHUT_DOWN_FIRST: &str = "Shut it down first: a copy of a running VM is like one after a power cut";

/// Copy VM and Make Template: one button (the copy icon and an arrow) opening both, each greyed
/// out while the VM is on.
fn copy_menu(ui: &mut Ui, pal: &Pal, sid: u32, vm: &str, off: bool, out: &mut Vec<Intent>) {
    let m = egui::menu::menu_custom_button(ui, egui::Button::new("").frame(false).min_size(TOOL_BTN), |ui| {
        ui.set_min_width(200.0);
        for (glyph, label, intent) in [("copy", "Copy VM…", Intent::CopyVm(sid, vm.to_string())), ("template", "Make Template…", Intent::MakeTemplate(sid, vm.to_string()))] {
            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), if off { Sense::click() } else { Sense::hover() });
            if off && resp.hovered() {
                ui.painter().rect_filled(r, Rounding::same(5.0), pal.hover);
            }
            let ink = if off { pal.ink } else { pal.ink_faint };
            paint_glyph(ui.painter(), Rect::from_min_size(r.min + vec2(7.0, 7.0), vec2(16.0, 16.0)), glyph, if off { pal.accent } else { pal.ink_faint });
            ui.painter().text(pos2(r.min.x + 32.0, r.center().y), Align2::LEFT_CENTER, label, FontId::proportional(13.0), ink);
            let resp = if off { resp } else { resp.on_hover_text(SHUT_DOWN_FIRST) };
            if off && resp.clicked() {
                out.push(intent);
                ui.close_menu();
            }
        }
    });
    paint_tool_btn(ui.painter(), m.response.rect, &Face::Glyph("copy", pal.accent), off, m.response.hovered() || m.inner.is_some(), true, pal);
    m.response.on_hover_text(if off { "Copy VM, or make a template from it" } else { "Copy VM, Make Template: shut it down first" });
}

/// The VM's power menu: one button, the power symbol in the colour of the VM's state (a ring
/// turning while an action runs), and an arrow; a click opens the actions, each with its coloured icon and
/// shortcut, greyed out when it can't run (`entries`: action and whether it can run; None is a
/// line). A click never runs one by itself.
fn power_menu(ui: &mut Ui, pal: &Pal, face: Face, state: &str, entries: &[Option<(usize, bool)>], out: &mut Vec<Intent>) {
    let m = egui::menu::menu_custom_button(ui, egui::Button::new("").frame(false).min_size(TOOL_BTN), |ui| {
        ui.set_min_width(220.0);
        ui.spacing_mut().item_spacing.y = 1.0;
        for e in entries {
            let Some((i, ok)) = *e else {
                ui.separator();
                continue;
            };
            let a = &ACTIONS[i];
            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), if ok { Sense::click() } else { Sense::hover() });
            if ok && resp.hovered() {
                ui.painter().rect_filled(r, Rounding::same(5.0), pal.hover);
            }
            paint_tool(ui.painter(), Rect::from_min_size(r.min + vec2(4.0, 5.0), vec2(20.0, 20.0)), a.op, ok, pal);
            let ink = if ok { pal.ink } else { pal.ink_faint };
            ui.painter().text(pos2(r.min.x + 32.0, r.center().y), Align2::LEFT_CENTER, a.label, FontId::proportional(13.0), ink);
            ui.painter().text(pos2(r.max.x - 8.0, r.center().y), Align2::RIGHT_CENTER, a.shortcut.text(), FontId::proportional(12.0), pal.ink_faint);
            if ok && resp.clicked() {
                out.push(Intent::Action(i));
                ui.close_menu();
            }
        }
    });
    paint_tool_btn(ui.painter(), m.response.rect, &face, true, m.response.hovered() || m.inner.is_some(), true, pal);
    m.response.on_hover_text(format!("Power: {}", state));
}

/// A console's keyboard button: its menu sends Ctrl+Alt+Del and the keys the desktop keeps.
fn keys_button(ui: &mut Ui, pal: &Pal, c: &mut ConsoleView) {
    let on = c.connected();
    ui.add_enabled_ui(on, |ui| {
        let m = egui::menu::menu_custom_button(ui, egui::Button::new("").frame(false).min_size(TOOL_BTN), |ui| c.keys_menu(ui));
        paint_tool_btn(ui.painter(), m.response.rect, &Face::Glyph("keyboard", pal.icon), on, (m.response.hovered() && on) || m.inner.is_some(), true, pal);
        m.response.on_hover_text("Send keys: Ctrl+Alt+Del, the Windows key, Alt+Tab…");
    });
}

/// What a button at the tab row's end shows.
enum Face<'a> {
    /// a toolbar glyph (`paint_tool`'s), in its action's colour
    Tool(&'a str),
    /// a plain glyph (`paint_glyph`'s), in a colour
    Glyph(&'a str, Color32),
    /// a ring turning: an action under way
    Busy(Tone),
}

/// The buttons at the tab row's end (power, Take Snapshot, Copy, Delete; a server's Restart and
/// Shut Down; the keyboard) are all `TOOL_BTN` big: a soft pill in the colour of what it does (a
/// stronger one under the mouse, a grey one when it can't be used), its face, and a small arrow
/// when it opens a menu.
fn paint_tool_btn(p: &egui::Painter, r: Rect, face: &Face, enabled: bool, hot: bool, menu: bool, pal: &Pal) {
    let col = match face {
        Face::Tool(op) => tool_color(op, pal),
        Face::Glyph(_, c) => *c,
        Face::Busy(_) => pal.ink_soft,
    };
    let pill = Rect::from_center_size(r.center(), vec2(r.width() - 2.0, 26.0));
    let tint = if enabled {
        col.gamma_multiply(match (pal.dark, hot) {
            (true, false) => 0.22,
            (true, true) => 0.36,
            (false, false) => 0.14,
            (false, true) => 0.26,
        })
    } else {
        pal.ink_faint.gamma_multiply(0.15)
    };
    p.rect_filled(pill, Rounding::same(7.0), tint);
    let at = if menu { pos2(pill.min.x + 16.0, pill.center().y) } else { pill.center() };
    match face {
        Face::Tool(op) => paint_tool_glyph(p, Rect::from_center_size(at, vec2(22.0, 22.0)), op, enabled, pal),
        Face::Glyph(g, _) => paint_glyph(p, Rect::from_center_size(at, vec2(16.0, 16.0)), g, if enabled { col } else { pal.ink_faint }),
        Face::Busy(tone) => paint_icon(p, Rect::from_center_size(at, vec2(16.0, 16.0)), IconKind::Busy(*tone), pal, pal.window),
    }
    if menu {
        let c = pos2(pill.max.x - 11.0, pill.center().y);
        p.add(egui::Shape::line(vec![pos2(c.x - 3.5, c.y - 1.5), pos2(c.x, c.y + 2.0), pos2(c.x + 3.5, c.y - 1.5)], Stroke::new(1.4_f32, if enabled { col } else { pal.ink_faint })));
    }
}

/// How wide the Console button is.
const CONSOLE_BTN: f32 = 104.0;

/// The Console button before the tab row's separator: a screen and its name on a soft blue pill,
/// filled blue while the console is shown (as the tab it stands for).
fn console_button(ui: &mut Ui, pal: &Pal, on: bool) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(CONSOLE_BTN, TOOL_BTN.y), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let p = ui.painter();
    let pill = Rect::from_center_size(r.center(), vec2(r.width() - 2.0, 26.0));
    let hot = resp.hovered();
    let (fill, ink) = if on {
        (if hot { pal.accent.gamma_multiply(0.85) } else { pal.accent }, pal.on_accent)
    } else {
        (pal.accent.gamma_multiply(match (pal.dark, hot) {
            (true, false) => 0.22,
            (true, true) => 0.36,
            (false, false) => 0.14,
            (false, true) => 0.26,
        }), if pal.dark { pal.ink } else { pal.accent })
    };
    p.rect_filled(pill, Rounding::same(7.0), fill);
    let glyph = if on { pal.on_accent } else { pal.accent };
    let font = FontId::new(13.0, FontFamily::Name("bold".into()));
    let text = p.layout_no_wrap("Console".into(), font, ink);
    let x = pill.center().x - (16.0 + 7.0 + text.size().x) / 2.0;
    paint_glyph_on(p, Rect::from_center_size(pos2(x + 8.0, pill.center().y), vec2(16.0, 16.0)), "console", glyph, fill);
    p.galley(pos2(x + 23.0, pill.center().y - text.size().y / 2.0), text, ink);
    resp
}

/// How wide the Actions button is.
const ACTIONS_BTN: f32 = 100.0;

/// In a narrow window, the tab row's buttons all in one: "Actions" on a soft pill, a menu of them
/// by name under it (`add`).
fn actions_menu(ui: &mut Ui, pal: &Pal, add: impl FnOnce(&mut Ui)) {
    let slot = ui.painter().add(egui::Shape::Noop);
    let m = egui::menu::menu_custom_button(ui, egui::Button::new("").frame(false).min_size(vec2(ACTIONS_BTN, TOOL_BTN.y)), |ui| {
        ui.set_min_width(210.0);
        add(ui);
    });
    let r = m.response.rect;
    let open = m.inner.is_some();
    let pill = Rect::from_center_size(r.center(), vec2(r.width() - 2.0, 26.0));
    let fill = pal.ink_soft.gamma_multiply(match (pal.dark, m.response.hovered() || open) {
        (true, false) => 0.18,
        (true, true) => 0.30,
        (false, false) => 0.12,
        (false, true) => 0.22,
    });
    ui.painter().set(slot, egui::epaint::RectShape::filled(pill, Rounding::same(7.0), fill));
    let p = ui.painter();
    let font = FontId::new(13.0, FontFamily::Name("bold".into()));
    let text = p.layout_no_wrap("Actions".into(), font, pal.ink);
    let x = pill.center().x - (text.size().x + 16.0) / 2.0;
    p.galley(pos2(x, pill.center().y - text.size().y / 2.0), text.clone(), pal.ink);
    let c = pos2(x + text.size().x + 10.0, pill.center().y);
    p.add(egui::Shape::line(vec![pos2(c.x - 3.5, c.y - 1.5), pos2(c.x, c.y + 2.0), pos2(c.x + 3.5, c.y - 1.5)], Stroke::new(1.4_f32, pal.ink_soft)));
    m.response.on_hover_text("Console, power, snapshots and more");
}

/// A button at the tab row's end (see `paint_tool_btn`); its name is its tip.
fn tool_btn(ui: &mut Ui, face: Face, tip: &str, enabled: bool, pal: &Pal) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(TOOL_BTN, if enabled { Sense::click() } else { Sense::hover() });
    paint_tool_btn(ui.painter(), r, &face, enabled, enabled && resp.hovered(), false, pal);
    resp.on_hover_text(tip)
}

/// A thin upright line before a group of buttons.
fn vsep(ui: &mut Ui, pal: &Pal) {
    let (r, _) = ui.allocate_exact_size(vec2(SEP_W - 2.0, 18.0), Sense::hover());
    ui.painter().line_segment([r.center_top(), r.center_bottom()], Stroke::new(1.0_f32, pal.border));
}

fn tool_button(ui: &mut Ui, op: &str, label: &str, enabled: bool, compact: bool, pal: &Pal) -> egui::Response {
    let w = if compact { SQUARE.x } else { tool_width(ui, label) };
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, mut resp) = ui.allocate_exact_size(vec2(w, SQUARE.y), sense);
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
    let s = Stroke::new(1.6_f32, pal.ink_soft);
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

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Why a change to a VM failed, in plain words where the server's answer is a known one.
fn edit_problem(e: &ApiError) -> String {
    match e.code.as_str() {
        "VM_BAD_POWER_STATE" => "The VM has to be shut down for that.".into(),
        "SR_BACKEND_FAILURE_44" => "There isn't enough free space on the storage for that.".into(),
        "VDI_IN_USE" => "Something else is using the disk right now.".into(),
        "OPERATION_NOT_ALLOWED" | "OTHER_OPERATION_IN_PROGRESS" => "Something else is being done to the VM right now. Wait for it to finish, then try again.".into(),
        "MESSAGE_METHOD_UNKNOWN" => "This server's version can't do that.".into(),
        _ => e.message(),
    }
}

/// Why taking, reverting to or deleting a snapshot failed, in plain words where the server's
/// answer is a known one.
fn snapshot_problem(e: &ApiError) -> String {
    match e.code.as_str() {
        "SR_BACKEND_FAILURE_44" => "There isn't enough free space on the storage for the snapshot's copies of the disks.".into(),
        "SR_BACKEND_FAILURE_109" => "A disk has too many snapshots in a row. Delete some of this VM's snapshots, give the storage a few minutes to merge the disks, then try again.".into(),
        "SR_OPERATION_NOT_SUPPORTED" => "The storage its disks are on can't take snapshots.".into(),
        "OTHER_OPERATION_IN_PROGRESS" | "OPERATION_NOT_ALLOWED" => "Something else is being done to the VM right now. Wait for it to finish, then try again.".into(),
        "VM_CHECKPOINT_SUSPEND_FAILED" | "VM_CHECKPOINT_RESUME_FAILED" => "Its memory couldn't be saved (its guest tools may be missing or busy). Try without its memory.".into(),
        "VM_REVERT_FAILED" => "The server couldn't put the VM back; it's been left off.".into(),
        _ => e.message(),
    }
}

/// The time now, as the logs show it: by this computer's clock ("14:59:24"); in UTC, said so,
/// where its time zone can't be had.
fn clock_text() -> String {
    let now = unix_now();
    fmt_clock(now, local_offset(now), !cfg!(unix))
}

/// The box a VM's name is typed in, in the header: room for the name and some more.
fn rename_box_width(name_w: f32) -> f32 {
    (name_w + 60.0).clamp(220.0, 420.0)
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
    /// Look on GitHub for a newer version as the app starts.
    updates: bool,
    /// Mount storage again on finding it unmounted.
    repair: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { theme: ThemeMode::System, sidebar_open: true, zoom: 1.0, updates: true, repair: true }
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
                "updates" => s.updates = v.trim() != "off",
                "repair" => s.repair = v.trim() != "off",
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
    let on = |b: bool| if b { "on" } else { "off" };
    let text = format!("theme={}\nsidebar={}\nzoom={:.2}\nupdates={}\nrepair={}\n", s.theme.as_str(), if s.sidebar_open { "open" } else { "folded" }, s.zoom, on(s.updates), on(s.repair));
    let _ = std::fs::write(dir.join("settings.txt"), text);
}

/// Earlier servers and the account used on each, newest first.
/// What was being made when contact was lost (or the app closed), one per line: server, user,
/// name, and what already had the name. Read at startup as lost: the app was away meanwhile.
fn load_unfinished() -> Vec<Unfinished> {
    let Some(dir) = config_dir() else { return vec![] };
    parse_unfinished(&std::fs::read_to_string(dir.join("unfinished.txt")).unwrap_or_default())
}

fn parse_unfinished(text: &str) -> Vec<Unfinished> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let (host, user, name) = (f.next()?, f.next()?, f.next()?);
            let before = f.next().unwrap_or_default().split(',').filter(|r| !r.is_empty()).map(String::from).collect();
            Some(Unfinished { host: host.into(), user: user.into(), name: name.into(), before, lost: true })
        })
        .collect()
}

fn save_unfinished(list: &[Unfinished]) {
    let Some(dir) = config_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("unfinished.txt"), unfinished_text(list));
}

fn unfinished_text(list: &[Unfinished]) -> String {
    let clean = |t: &str| t.replace(['\t', '\n'], " ");
    list.iter().map(|u| format!("{}\t{}\t{}\t{}\n", clean(&u.host), clean(&u.user), clean(&u.name), u.before.join(","))).collect()
}

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

/// The pool coordinator last seen through `host`, and where its Wake-on-LAN signal goes (wake.txt:
/// the server, its coordinator's name, the card's address, the broadcast address; tabs between).
fn load_wake(host: &str) -> Option<(String, WakeTarget)> {
    let dir = config_dir()?;
    std::fs::read_to_string(dir.join("wake.txt")).ok()?.lines().find_map(|l| {
        let f: Vec<&str> = l.split('\t').collect();
        (f.len() >= 4 && f[0].trim().eq_ignore_ascii_case(host) && parse_mac(f[2]).is_some()).then(|| (f[1].to_string(), WakeTarget { mac: f[2].trim().to_string(), broadcast: f[3].trim().to_string() }))
    })
}

fn save_wake(host: &str, wake: Option<&(String, WakeTarget)>) {
    let Some(dir) = config_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let f = dir.join("wake.txt");
    let clean = |t: &str| t.replace(['\t', '\n'], " ");
    let mut lines: Vec<String> = wake.map(|(n, w)| format!("{}\t{}\t{}\t{}", clean(host), clean(n), clean(&w.mac), clean(&w.broadcast))).into_iter().collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_sort_as_people_expect() {
        use std::cmp::Ordering::*;
        // numbers in words as numbers; addresses part by part
        assert_eq!(natural_order("vm 2", "vm 10"), Less);
        assert_eq!(natural_order("10.20.1.9", "10.20.1.10"), Less);
        assert_eq!(natural_order("vnet-casa", "vnet-citrix"), Less);
        assert_eq!(natural_order("disk 007", "disk 7"), Equal);
        // sizes as bytes, plain numbers as numbers, nothing as nothing
        let k = |t: &str| Cell::Text(t.into()).key();
        assert_eq!(key_order(&k("661 MB").unwrap(), &k("1.8 GB").unwrap()), Less);
        assert_eq!(key_order(&k("9000").unwrap(), &k("1500").unwrap()), Greater);
        assert_eq!(k("—"), None);
        assert_eq!(Cell::by_num(5.0, Cell::Custom(10.0)).key(), Some(SortKey::Num(5.0)));
        assert_eq!(Cell::Custom(10.0).key(), None);
    }

    #[test]
    fn unfinished_round_trip() {
        let list = vec![
            Unfinished { host: "10.0.0.5".into(), user: "root".into(), name: "Web 2".into(), before: vec![], lost: false },
            Unfinished { host: "home".into(), user: "root".into(), name: "Desk\tcopy".into(), before: vec!["OpaqueRef:a".into(), "OpaqueRef:b".into()], lost: false },
        ];
        let back = parse_unfinished(&unfinished_text(&list));
        assert_eq!(back.len(), 2);
        // read back as lost: the app was away meanwhile
        assert!(back.iter().all(|u| u.lost));
        assert_eq!((back[0].name.as_str(), back[0].before.len()), ("Web 2", 0));
        assert_eq!((back[1].name.as_str(), back[1].before.clone()), ("Desk copy", vec!["OpaqueRef:a".to_string(), "OpaqueRef:b".to_string()]));
    }
}
