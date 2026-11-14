//! The main window: menus, toolbar, foldable sidebar with the resource tree, header, tabs, dialogs.
//! Every colour comes from the light or dark palette (theme.rs).

use crate::conn::*;
use crate::console::{self, ConsoleView};
use crate::icons::*;
use crate::model::*;
use crate::theme::{self, Pal, ThemeMode};
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontFamily, FontId, Key, Modifiers, Rect, Rounding, Sense, Stroke, Ui};
use std::collections::{HashMap, HashSet};
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
    busy: HashMap<String, String>,
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
    ActionDone { sid: u32, vm: String, log: usize, err: Option<String>, label: &'static str, text: String },
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
            Tab::Logs => "Logs",
        }
    }
}

enum Dialog {
    None,
    AddServer { server: String, user: String, password: String, error: String },
    Confirm { action: usize, sid: u32, vm: String, title: String, text: String },
    Message { title: String, text: String },
}

enum Intent {
    Select(String),
    Toggle(String),
    Action(usize),
    AddServer,
    Demo,
    Remove(u32),
    Reconnect(u32),
    Refresh(Option<u32>),
    View(bool),
    Tab(Tab),
    Section(String),
    Theme(ThemeMode),
    ToggleTheme,
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
    pal: Pal,
    applied_dark: Option<bool>,
    tab: Tab,
    open_sections: HashSet<String>,
    closed_sections: HashSet<String>,
    dialog: Dialog,
    console: Option<ConsoleView>,
    known_hosts: Vec<String>,
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
            pal: theme::LIGHT,
            applied_dark: None,
            tab: Tab::Home,
            open_sections: HashSet::new(),
            closed_sections: HashSet::new(),
            dialog: Dialog::None,
            console: None,
            known_hosts: load_history(),
            shot_path: start.shot_path,
            shot_show: start.shot_show,
            frames: 0,
            shot_ready: false,
            shot_requested: false,
            shot_dialog: false,
            started: Instant::now(),
        };
        app.apply_theme(&cc.egui_ctx);
        if shot {
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
            save_settings(&Settings { theme: self.theme_mode, sidebar_open: self.sidebar_open });
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
        });
        self.add_log(id, &host, None, &host, &format!("Connecting to {}", host), "In progress");
        self.select(format!("srv:{}", id));
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
                Msg::ActionDone { sid, vm, log, err, label, text } => {
                    if let Some(s) = self.server_mut(sid) {
                        s.busy.remove(&vm);
                        s.next_refresh = Instant::now(); // re-read right away
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
                    }
                    self.expanded.insert(format!("srv:{}", id));
                    self.expanded.extend(host_keys);
                    if shot && is_demo {
                        match self.shot_show.as_str() {
                            "home" => self.select("root".into()),
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
                            self.dialog = Dialog::AddServer { server: host, user, password: String::new(), error: msg };
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
        self.selected = key;
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

    fn vm_text(s: &ServerEntry, v: &VmInfo) -> String {
        match s.busy.get(&v.rref) {
            Some(b) => format!("{}  ({})", v.name, b),
            None => v.name.clone(),
        }
    }
    fn vm_icon(s: &ServerEntry, v: &VmInfo) -> IconKind {
        if s.busy.contains_key(&v.rref) {
            IconKind::Busy
        } else {
            IconKind::Vm(v.power)
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
                    cats[3].children.push(Node::new(format!("sr:{}:{}", id, x.rref), x.name.clone(), IconKind::Sr, Kind::Sr, Some(id), Some(x.rref.clone())));
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
                    let icon = if s.state == ConnState::Connecting { IconKind::Busy } else { IconKind::ServerDown };
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
                let sr_node = |x: &SrInfo| Node::new(format!("sr:{}:{}", id, x.rref), x.name.clone(), IconKind::Sr, Kind::Sr, Some(id), Some(x.rref.clone()));
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
            s.busy.insert(vref.clone(), format!("{}…", a.verb));
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let r = conn.run_action(&vref, &ACTIONS[i]);
            let _ = tx.send(Msg::ActionDone { sid, vm: vref, log, err: r.err().map(|e| e.message()), label: ACTIONS[i].label, text });
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
                Intent::AddServer => self.dialog = Dialog::AddServer { server: String::new(), user: "root".into(), password: String::new(), error: String::new() },
                Intent::Demo => self.connect_demo(),
                Intent::Remove(sid) => self.remove_server(sid, true),
                Intent::Reconnect(sid) => {
                    if let Some(s) = self.server(sid) {
                        let (h, u) = (s.conn.hostname(), s.conn.username());
                        self.remove_server(sid, false);
                        self.dialog = Dialog::AddServer { server: h, user: u, password: String::new(), error: String::new() };
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
                    let open = self.open_sections.contains(&name) || (!self.closed_sections.contains(&name) && name == "General");
                    if open {
                        self.open_sections.remove(&name);
                        self.closed_sections.insert(name);
                    } else {
                        self.closed_sections.remove(&name);
                        self.open_sections.insert(name);
                    }
                }
                Intent::Theme(m) => {
                    self.theme_mode = m;
                    self.apply_theme(ctx);
                    self.save_settings();
                }
                Intent::ToggleTheme => {
                    self.theme_mode = if self.pal.dark { ThemeMode::Light } else { ThemeMode::Dark };
                    self.apply_theme(ctx);
                    self.save_settings();
                }
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
                        text: "Simple VM Manager\n\nA simpler console for virtualization servers: add servers, browse their pools, servers, virtual machines, storage and networks, and start, stop, restart, suspend or resume virtual machines.\n\nRuns on Windows, Linux and macOS, in a light or dark theme. It talks to servers over HTTPS (port 443) and accepts their certificates without asking.".into(),
                    }
                }
            }
        }
    }

    /// The first section starts open and the others closed, as in the original; clicks are remembered.
    fn section_open(&self, name: &str, index: usize) -> bool {
        if self.open_sections.contains(name) {
            return true;
        }
        if self.closed_sections.contains(name) {
            return false;
        }
        index == 0
    }

    // ------------------------------------------------------------------ drawing

    fn toolbar(&self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let vm = self.vm_of(sel).map(|x| x.2);
        let shown: Vec<&VmAction> = ACTIONS.iter().filter(|a| a.on_toolbar(vm)).collect();
        let (theme_op, theme_label) = if pal.dark { ("theme-light", "Light") } else { ("theme-dark", "Dark") };
        // When the window is too narrow for every label, buttons show only their icons (the label
        // becomes a tooltip): first the Force buttons and the theme switch, then all of them.
        let fits = |level: u8| {
            let w = |l: &str, minor: bool| if level == 2 || (level == 1 && minor) { 32.0 } else { tool_width(ui, l) } + 2.0;
            let buttons: f32 = w("Connect", false) + shown.iter().map(|a| w(a.label, a.is_force())).sum::<f32>() + w(theme_label, true);
            36.0 + 2.0 * 10.0 + 16.0 + buttons <= ui.available_width()
        };
        let level = (0..2).find(|&l| fits(l)).unwrap_or(2);
        let compact = |minor: bool| level == 2 || (level == 1 && minor);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let tip = if self.sidebar_open { "Fold the sidebar" } else { "Show the sidebar" };
            if icon_button(ui, "sidebar", &pal, true, self.sidebar_open).on_hover_text(tip).clicked() {
                out.push(Intent::Sidebar(!self.sidebar_open));
            }
            ui.add_space(4.0);
            if tool_button(ui, "add", "Connect", true, compact(false), &pal).clicked() {
                out.push(Intent::AddServer);
            }
            ui.separator();
            let mut force_group = false;
            for a in &shown {
                if a.is_force() && !force_group {
                    ui.separator();
                    force_group = true;
                }
                if tool_button(ui, a.op, a.label, self.can_run(a, sel), compact(a.is_force()), &pal).clicked() {
                    out.extend(ACTIONS.iter().position(|b| std::ptr::eq(b, *a)).map(Intent::Action));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // A click switches light / dark; a right-click also offers following the system.
                let b = tool_button(ui, theme_op, theme_label, true, compact(true), &pal).on_hover_text("Right-click for more choices");
                if b.clicked() {
                    out.push(Intent::ToggleTheme);
                }
                b.context_menu(|ui| {
                    for m in [ThemeMode::Light, ThemeMode::Dark, ThemeMode::System] {
                        if ui.radio(self.theme_mode == m, m.label()).clicked() {
                            out.push(Intent::Theme(m));
                            ui.close_menu();
                        }
                    }
                });
            });
        });
    }

    /// The unfolded sidebar: search box, resource tree, and the Infrastructure / Objects buttons.
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
        });
        let nav_h = 72.0;
        let tree_h = (ui.available_height() - nav_h).max(50.0);
        egui::ScrollArea::both().max_height(tree_h).min_scrolled_height(tree_h).auto_shrink([false; 2]).show(ui, |ui| {
            ui.add_space(2.0);
            self.tree_ui(ui, tree, 0, out);
        });
        ui.painter().line_segment([ui.cursor().left_top(), ui.cursor().right_top()], Stroke::new(1.0, pal.border));
        ui.add_space(4.0);
        for (label, objects, icon) in [("Infrastructure", false, IconKind::Home), ("Objects", true, IconKind::Vm(PowerState::Running))] {
            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::click());
            let r = r.shrink2(vec2(6.0, 2.0));
            let on = self.objects_view == objects;
            let p = ui.painter();
            let fill = if on { pal.nav_on } else if resp.hovered() { pal.hover } else { Color32::TRANSPARENT };
            p.rect_filled(r, Rounding::same(6.0), fill);
            if on {
                p.rect_filled(Rect::from_min_size(r.min + vec2(0.0, 6.0), vec2(3.0, r.height() - 12.0)), Rounding::same(1.5), pal.accent);
            }
            paint_icon(p, Rect::from_min_size(r.min + vec2(12.0, 6.0), vec2(16.0, 16.0)), icon, &pal, if on { pal.nav_on } else { pal.sidebar });
            p.text(r.min + vec2(36.0, r.height() / 2.0), Align2::LEFT_CENTER, label, FontId::proportional(14.0), pal.ink);
            if resp.clicked() {
                out.push(Intent::View(objects));
            }
        }
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
            if rail_icon(ui, &pal, RailIcon::Tool("add"), false).on_hover_text("Connect").clicked() {
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
                    if ui.button("Refresh Now").clicked() {
                        out.push(Intent::Refresh(Some(sid)));
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

    fn header(&self, ui: &mut Ui, sel: &Node) {
        let pal = self.pal;
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
        let p = ui.painter();
        gradient(p, rect, pal.header_a, pal.header_b, false);
        p.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, pal.border_strong));
        paint_icon(p, Rect::from_min_size(rect.min + vec2(8.0, 6.0), vec2(16.0, 16.0)), sel.icon, &pal, pal.header_a);
        let (title, sub) = self.header_text(sel);
        let g = p.layout_no_wrap(title, FontId::new(14.0, FontFamily::Name("bold".into())), pal.ink);
        let w = g.size().x;
        p.galley(pos2(rect.min.x + 30.0, rect.center().y - g.size().y / 2.0), g, pal.ink);
        p.text(pos2(rect.min.x + 38.0 + w, rect.center().y), Align2::LEFT_CENTER, sub, FontId::proportional(13.0), pal.ink_soft);
    }

    fn header_text(&self, sel: &Node) -> (String, String) {
        let s = sel.sid.and_then(|id| self.server(id));
        let recon = if s.map_or(false, |s| s.state == ConnState::Reconnecting) { "  (reconnecting…)" } else { "" };
        match (sel.kind, s.and_then(|s| s.data.as_ref().map(|d| (s, d)))) {
            (Kind::Root, _) => ("All servers".into(), String::new()),
            (Kind::Category, _) => (sel.text.clone(), String::new()),
            (Kind::Folder, Some((s, _))) => {
                let what = if sel.rref.as_deref() == Some("nets") { "Virtual Networks" } else { "Shared Storage" };
                (what.into(), format!("in '{}'{}", s.name(), recon))
            }
            (Kind::Vm, Some((s, d))) => {
                let v = sel.rref.as_deref().and_then(|r| d.vm(r));
                let title = match v {
                    Some(v) => match v.host.as_deref().and_then(|h| d.host(h)) {
                        Some(h) => format!("{} on '{}'", v.name, h.name),
                        None => v.name.clone(),
                    },
                    None => sel.text.clone(),
                };
                let busy = v.and_then(|v| s.busy.get(&v.rref)).map(|b| format!("({})", b)).unwrap_or_default();
                (title, format!("{}{}", busy, recon))
            }
            (Kind::Pool, Some((s, d))) => (s.name(), format!("({} servers){}", d.hosts.len(), recon)),
            (Kind::Host, Some((_, d))) => {
                let h = sel.rref.as_deref().and_then(|r| d.host(r));
                let role = if h.map_or(false, |h| h.is_master) { "(pool coordinator)" } else { "" };
                (h.map(|h| h.name.clone()).unwrap_or_default(), format!("{}{}", role, recon))
            }
            (_, Some(_)) => (sel.text.clone(), recon.to_string()),
            (_, None) => {
                let what = if s.map_or(false, |s| s.state == ConnState::Connecting) { "Connecting…" } else { "Not connected" };
                (s.map(|s| s.conn.hostname()).unwrap_or_default(), what.into())
            }
        }
    }

    fn tabs_for(&self, sel: &Node) -> Vec<Tab> {
        let has_data = sel.sid.and_then(|id| self.server(id)).map_or(false, |s| s.data.is_some());
        match sel.kind {
            Kind::Root | Kind::Category => vec![Tab::Home, Tab::Search, Tab::Logs],
            Kind::Folder if sel.rref.as_deref() == Some("nets") => vec![Tab::Networking],
            Kind::Folder => vec![Tab::Storage],
            _ if !has_data => vec![Tab::General, Tab::Logs],
            Kind::Vm => vec![Tab::General, Tab::Memory, Tab::Storage, Tab::Networking, Tab::Console, Tab::Logs],
            Kind::Pool | Kind::Host => vec![Tab::General, Tab::Memory, Tab::Storage, Tab::Networking, Tab::Logs],
            _ => vec![Tab::General, Tab::Logs],
        }
    }

    /// Draws the tabs; returns the room left on their right.
    fn tab_strip(&self, ui: &mut Ui, tabs: &[Tab], current: Tab, out: &mut Vec<Intent>) -> Rect {
        let pal = self.pal;
        let (strip, _) = ui.allocate_exact_size(vec2(ui.available_width(), 27.0), Sense::hover());
        ui.painter().rect_filled(strip, 0.0, pal.tab_strip);
        let mut x = strip.min.x + 4.0;
        for t in tabs {
            let font = FontId::proportional(13.0);
            let w = ui.painter().layout_no_wrap(t.label().into(), font.clone(), pal.ink).size().x + 26.0;
            let r = Rect::from_min_size(pos2(x, strip.min.y + 4.0), vec2(w, 23.0));
            let resp = ui.interact(r, ui.id().with(t.label()), Sense::click());
            let on = *t == current;
            let fill = if on {
                pal.tab_on
            } else if resp.hovered() {
                pal.hover
            } else {
                pal.tab_off
            };
            let rounding = Rounding { nw: 5.0, ne: 5.0, sw: 0.0, se: 0.0 };
            ui.painter().rect(r, rounding, fill, Stroke::new(1.0, pal.border));
            if on {
                ui.painter().rect_filled(Rect::from_min_size(r.min + vec2(1.0, 0.0), vec2(r.width() - 2.0, 2.0)), Rounding::same(1.0), pal.accent);
            }
            ui.painter().text(r.center(), Align2::CENTER_CENTER, t.label(), font, if on { pal.ink } else { pal.ink_soft });
            if resp.clicked() {
                out.push(Intent::Tab(*t));
            }
            x += w + 2.0;
        }
        Rect::from_min_max(pos2(x + 10.0, strip.min.y + 2.0), pos2(strip.max.x - 6.0, strip.max.y - 1.0))
    }

    fn home(&self, ui: &mut Ui, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let avail = ui.available_size();
        // The bullet lines wrap to the page width, and the three links share it, so nothing is cut
        // off in a narrow window.
        let lines = ["Connect to a server and see its pool, VMs, storage and networks", "Start, shut down, reboot or suspend any virtual machine", "Try everything first with the demo server"];
        let wrap = (avail.x - 72.0 - 24.0).max(120.0);
        let galleys: Vec<_> = lines.iter().map(|l| ui.painter().layout(l.to_string(), FontId::proportional(15.0), pal.ink, wrap)).collect();
        let links_top = 150.0 + galleys.iter().map(|g| g.size().y + 12.0).sum::<f32>() + 80.0;
        let (rect, _) = ui.allocate_exact_size(vec2(avail.x, avail.y.max(links_top + 160.0)), Sense::hover());
        let p = ui.painter().clone();
        p.rect_filled(rect, 0.0, pal.surface);
        let o = rect.min;
        p.text(o + vec2(46.0, 24.0), Align2::LEFT_TOP, "Server Manager", FontId::new(40.0, FontFamily::Name("bold".into())), pal.ink);
        p.text(o + vec2(52.0, 86.0), Align2::LEFT_TOP, "A simple console for your virtualization servers\nand the virtual machines they run", FontId::proportional(17.0), pal.ink);
        let mut y = 150.0;
        for g in galleys {
            let h = g.size().y;
            p.circle_filled(o + vec2(60.0, y + 10.0), 2.2, pal.ink_soft);
            p.galley(o + vec2(72.0, y), g, pal.ink);
            y += h + 12.0;
        }
        if rect.width() >= 800.0 {
            paint_cloud(&p, o + vec2(rect.width() - 230.0, 60.0), &pal); // only beside the text, never over it
        }
        // Three pictures with a link under each, side by side; when the page is too narrow for
        // them, just the links, one per line.
        let step = ((rect.width() - 46.0 - 16.0) / 3.0).min(260.0);
        let pictures = step >= 150.0;
        let links = [("add", "Connect to a Server"), ("demo", "Try the Demo Server"), ("about", "About this app")];
        for (i, (kind, label)) in links.iter().enumerate() {
            let link = egui::Link::new(egui::RichText::new(*label).size(15.0).color(pal.link));
            let resp = if pictures {
                let cx = o.x + 46.0 + (i as f32 + 0.5) * step;
                paint_home_icon(&p, pos2(cx - 40.0, o.y + links_top), kind, &pal);
                ui.put(Rect::from_center_size(pos2(cx, o.y + links_top + 120.0), vec2(step, 24.0)), link)
            } else {
                let lr = Rect::from_min_size(o + vec2(52.0, links_top - 40.0 + i as f32 * 32.0), vec2(rect.width() - 60.0, 24.0));
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(lr), |ui| ui.add(link)).inner
            };
            if resp.clicked() {
                out.push(match *kind {
                    "add" => Intent::AddServer,
                    "demo" => Intent::Demo,
                    _ => Intent::About,
                });
            }
        }
    }

    fn search_tab(&self, ui: &mut Ui, out: &mut Vec<Intent>) {
        let pal = self.pal;
        egui::ScrollArea::both().auto_shrink([false; 2]).show(ui, |ui| {
            ui.add_space(6.0);
            egui::Grid::new("vms").striped(true).min_col_width(70.0).spacing([18.0, 6.0]).show(ui, |ui| {
                for h in ["Name", "State", "Pool or server", "Running on", "CPUs", "Memory", "IP address"] {
                    ui.strong(h);
                }
                ui.end_row();
                for s in &self.servers {
                    let Some(d) = &s.data else { continue };
                    for v in &d.vms {
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                            paint_icon(ui.painter(), r, Self::vm_icon(s, v), &pal, pal.window);
                            if ui.link(&v.name).clicked() {
                                out.push(Intent::Select(format!("vm:{}:{}", s.id, v.rref)));
                            }
                        });
                        ui.label(v.power.label());
                        ui.label(s.name());
                        ui.label(v.host.as_deref().and_then(|h| d.host(h)).map(|h| h.name.clone()).unwrap_or_else(|| "—".into()));
                        ui.label(v.vcpus.to_string());
                        ui.label(fmt_bytes(v.mem_dyn_max));
                        ui.label(or(&v.ip, "—"));
                        ui.end_row();
                    }
                }
            });
        });
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
                    let home = v.home().and_then(|h| d.host(h)).map(|h| h.name.clone()).unwrap_or_else(|| "<None>".into());
                    let state = s.busy.get(&v.rref).cloned().unwrap_or_else(|| v.power.label().into());
                    secs.push((
                        "General".into(),
                        vec![
                            row("Name", v.name.clone()),
                            row("Description", v.description.clone()),
                            row("Power state", state),
                            row("Operating System", or(&v.os, "Unknown").into()),
                            row("IP address", or(&v.ip, "<None>").into()),
                            row("Home Server", home),
                            row("UUID", v.uuid.clone()),
                        ],
                    ));
                    secs.push(("CPUs and Memory".into(), vec![row("Virtual CPUs", v.vcpus.to_string()), row("Memory", fmt_bytes(v.mem_dyn_max))]));
                    let disks = v.disks.iter().filter(|x| x.kind == "Disk").count();
                    secs.push(("Devices".into(), vec![row("Disks", disks.to_string()), row("Network interfaces", v.nics.len().to_string())]));
                    let busy = s.busy.contains_key(&v.rref);
                    let can: Vec<&str> = ACTIONS.iter().filter(|a| a.can_run(v, d, busy)).map(|a| a.label).collect();
                    let text = if can.is_empty() { "Nothing right now".to_string() } else { can.join(", ") };
                    secs.push(("Available Actions".into(), vec![row("You can", text)]));
                }
            }
            Kind::Sr => {
                if let Some(x) = d.sr(r) {
                    secs.push((
                        "General".into(),
                        vec![row("Name", x.name.clone()), row("Description", x.description.clone()), row("Type", x.kind.clone()), row("Shared", if x.shared { "Yes".into() } else { "No".into() }), row("UUID", x.uuid.clone())],
                    ));
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

    /// The General tab: boxed sections that fold.
    fn general(&self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let secs = self.general_sections(sel);
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let w = ui.available_width();
            ui.add_space(14.0);
            let border = Stroke::new(1.0, pal.border);
            let chev_s = Stroke::new(1.3, pal.ink_soft);
            for (i, (name, rows)) in secs.iter().enumerate() {
                let open = self.section_open(name, i);
                let h = 36.0 + if open { rows.len() as f32 * 28.0 + 8.0 } else { 0.0 };
                let (outer, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
                let bx = Rect::from_min_max(outer.min + vec2(14.0, 0.0), outer.max - vec2(14.0, 0.0));
                let head = Rect::from_min_size(bx.min, vec2(bx.width(), 36.0));
                let hresp = ui.interact(head, ui.id().with(("sec", name.as_str())), Sense::click());
                let p = ui.painter();
                p.rect(bx, Rounding::same(6.0), pal.surface, border);
                if hresp.hovered() {
                    p.rect_filled(head.shrink(1.0), Rounding::same(5.0), pal.hover);
                }
                p.text(head.min + vec2(12.0, 18.0), Align2::LEFT_CENTER, name, FontId::proportional(17.0), pal.ink);
                let c = pos2(bx.max.x - 24.0, head.center().y);
                p.circle_stroke(c, 9.0, chev_s);
                let chev = if open { vec![c + vec2(-4.0, 2.0), c + vec2(0.0, -2.0), c + vec2(4.0, 2.0)] } else { vec![c + vec2(-4.0, -2.0), c + vec2(0.0, 2.0), c + vec2(4.0, -2.0)] };
                p.add(egui::Shape::line(chev, chev_s));
                if hresp.clicked() {
                    out.push(Intent::Section(name.clone()));
                }
                if open {
                    let mut y = head.max.y;
                    for (ri, (k, v)) in rows.iter().enumerate() {
                        let rr = Rect::from_min_size(pos2(bx.min.x + 1.0, y), vec2(bx.width() - 2.0, 28.0));
                        let first = i == 0 && ri == 0;
                        if first {
                            p.rect_filled(rr, 0.0, pal.accent);
                        }
                        let (kc, vc) = if first { (pal.on_accent, pal.on_accent) } else { (pal.ink_soft, pal.ink) };
                        p.text(pos2(bx.min.x + 20.0, rr.center().y), Align2::LEFT_CENTER, format!("{}:", k), FontId::proportional(13.0), kc);
                        let clip = p.with_clip_rect(Rect::from_min_max(pos2(bx.min.x + 170.0, rr.min.y), pos2(bx.max.x - 8.0, rr.max.y)));
                        clip.text(pos2(bx.min.x + 170.0, rr.center().y), Align2::LEFT_CENTER, v, FontId::proportional(13.0), vc);
                        y += 28.0;
                    }
                }
                ui.add_space(10.0);
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

    fn table(ui: &mut Ui, id: &str, headers: &[&str], rows: Vec<Vec<String>>) {
        egui::ScrollArea::both().auto_shrink([false; 2]).show(ui, |ui| {
            ui.add_space(6.0);
            egui::Grid::new(id).striped(true).min_col_width(60.0).spacing([20.0, 6.0]).show(ui, |ui| {
                for h in headers {
                    ui.strong(*h);
                }
                ui.end_row();
                for r in rows {
                    for c in r {
                        ui.label(c);
                    }
                    ui.end_row();
                }
            });
        });
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
            Self::table(ui, "disks", &["Position", "Name", "Size", "Storage", "Type"], rows);
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
                    let pct = if x.size > 0 { format!(" ({}%)", 100 * x.used / x.size) } else { String::new() };
                    vec![x.name.clone(), x.kind.clone(), fmt_bytes(x.size), format!("{}{}", fmt_bytes(x.used), pct), fmt_bytes((x.size - x.used).max(0)), if x.shared { "Yes".into() } else { "No".into() }]
                })
                .collect();
            Self::table(ui, "srs", &["Name", "Type", "Size", "Used", "Free", "Shared"], rows);
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
            Self::table(ui, "nics", &["Device", "MAC address", "Network", "IP address"], rows);
        } else {
            let rows = d.networks.iter().map(|n| vec![n.name.clone(), n.bridge.clone(), n.mtu.to_string(), n.description.clone()]).collect();
            Self::table(ui, "nets", &["Name", "Bridge", "MTU", "Description"], rows);
        }
    }

    /// The VM's screen. A shut down or suspended VM has none: offer to start or resume it.
    fn console_tab(&mut self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let Some((s, _, vm)) = self.vm_of(sel) else { return };
        let (sid, vref, name, os, power, conn) = (s.id, vm.rref.clone(), vm.name.clone(), vm.os.clone(), vm.power, s.conn.clone());
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
                    if dialog_button(ui, &pal, ACTIONS[i].label, true, self.can_run(&ACTIONS[i], sel)) {
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
        let rows: Vec<Vec<String>> = self
            .log
            .iter()
            .rev()
            .filter(|e| match sel.sid {
                Some(sid) => e.sid == sid && (sel.kind != Kind::Vm || e.obj_ref.as_deref() == sel.rref.as_deref()),
                None => true,
            })
            .map(|e| vec![e.time.clone(), e.server.clone(), e.obj.clone(), e.text.clone(), e.status.clone()])
            .collect();
        Self::table(ui, "logs", &["Time", "Server", "Object", "What happened", "Result"], rows);
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut add: Option<(String, String, String)> = None;
        let mut confirm: Option<(usize, u32, String)> = None;
        let hosts = self.known_hosts.clone();
        let red = self.pal.red;
        let pal = self.pal;
        match &mut self.dialog {
            Dialog::None => {}
            Dialog::AddServer { server, user, password, error } => {
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
                            for h in &hosts {
                                if ui.selectable_label(false, h).clicked() {
                                    *server = h.clone();
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
                    if !error.is_empty() {
                        ui.colored_label(red, error.as_str());
                    }
                    ui.add_space(4.0);
                    dialog_buttons(ui, |ui| {
                        if dialog_button(ui, &pal, "Cancel", false, true) {
                            close = true;
                        }
                        let ok = !server.trim().is_empty() && !user.trim().is_empty();
                        let enter = ui.input(|i| i.key_pressed(Key::Enter));
                        if dialog_button(ui, &pal, "Connect", true, ok) || (ok && enter) {
                            if server.trim().contains(' ') {
                                *error = "A server name can't contain spaces.".into();
                            } else {
                                add = Some((server.trim().to_string(), user.trim().to_string(), password.clone()));
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
                        if dialog_button(ui, &pal, "No", false, true) {
                            close = true;
                        }
                        if dialog_button(ui, &pal, "Yes", true, true) {
                            confirm = Some((*action, *sid, vm.clone()));
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
                        if dialog_button(ui, &pal, "OK", true, true) {
                            close = true;
                        }
                    });
                });
            }
        }
        if close {
            self.dialog = Dialog::None;
        }
        if let Some((h, u, p)) = add {
            self.dialog = Dialog::None;
            if let Some(dup) = self.servers.iter().find(|s| !s.conn.is_demo() && s.conn.hostname().eq_ignore_ascii_case(&h)).map(|s| s.id) {
                self.remove_server(dup, false);
            }
            self.add_server(Arc::new(RealConnection::new(&h, &u, &p)));
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
        if !self.shot_requested && ((self.shot_ready && self.frames >= 120) || timed_out) {
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
        let pal = self.pal;
        self.tick(ctx);
        let tree = self.build_tree();
        let sel = tree.find(&self.selected).cloned().unwrap_or_else(|| tree.clone());
        if sel.key != self.selected {
            self.selected = sel.key.clone();
        }
        // A VM's console stays connected while that VM is selected (on any of its tabs).
        if self.console.as_ref().map_or(false, |c| sel.sid != Some(c.sid) || sel.rref.as_deref() != Some(c.vm.as_str())) {
            self.console = None;
        }
        let mut out = self.shortcuts(ctx, &sel);
        // --show add-server | confirm | about: a check's screenshot with that dialog open.
        if self.shot_ready && !self.shot_dialog {
            self.shot_dialog = true;
            match self.shot_show.as_str() {
                "add-server" => out.push(Intent::AddServer),
                "confirm" => out.extend(ACTIONS.iter().position(|a| a.confirm && self.can_run(a, &sel)).map(Intent::Action)),
                "about" => out.push(Intent::About),
                "console" => out.push(Intent::Tab(Tab::Console)),
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
            self.header(ui, &sel);
            let tabs = self.tabs_for(&sel);
            let current = if tabs.contains(&self.tab) { self.tab } else { tabs[0] };
            let free = self.tab_strip(ui, &tabs, current, &mut out);
            if current == Tab::Console {
                if let Some(c) = self.console.as_mut() {
                    c.tab_controls(ui, free, &pal);
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
                Tab::Logs => self.logs(ui, &sel),
            });
        });

        self.dialogs(ctx);
        self.apply(out, &sel, ctx);
        self.screenshot(ctx);
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

const FIELD_HEIGHT: f32 = 26.0;
const LABEL_WIDTH: f32 = 80.0;
const GROUP_INSET: f32 = 11.0; // a group box's margin plus its 1px border

/// A dialog window: centred, fixed size, with room around its contents.
fn dialog_window(ctx: &egui::Context, title: &str) -> egui::Window<'static> {
    egui::Window::new(title.to_string())
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .frame(egui::Frame::window(&ctx.style()).inner_margin(egui::Margin::same(14.0)))
}

/// A left-aligned field label of fixed width, so the boxes after it line up.
fn field_label(ui: &mut Ui, text: &str, width: f32) {
    ui.allocate_ui_with_layout(vec2(width, FIELD_HEIGHT), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_size(vec2(width, FIELD_HEIGHT));
        ui.label(text);
    });
}

/// A one-line text box with a visible border (blue while typing), drawn like the sidebar's search
/// box. egui's own box has no border until hovered, and its focus outline is white.
fn text_field(ui: &mut Ui, pal: &Pal, text: &mut String, width: f32, password: bool) -> egui::Response {
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
fn dialog_buttons(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(vec2(ui.available_width(), FIELD_HEIGHT), egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        add(ui);
    });
}

/// A dialog button; the main one (Add, Yes, OK) is filled with the accent colour.
fn dialog_button(ui: &mut Ui, pal: &Pal, label: &str, primary: bool, enabled: bool) -> bool {
    let b = if primary && enabled {
        egui::Button::new(egui::RichText::new(label).color(pal.on_accent)).fill(pal.accent)
    } else {
        egui::Button::new(label).stroke(Stroke::new(1.0, pal.border_strong))
    };
    ui.add_enabled(enabled, b.min_size(vec2(80.0, FIELD_HEIGHT)).rounding(Rounding::same(5.0))).clicked()
}

/// How wide a labelled toolbar button is.
fn tool_width(ui: &Ui, label: &str) -> f32 {
    32.0 + ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(13.0), Color32::WHITE).size().x + 10.0
}

/// A labelled toolbar button with a theme-coloured glyph; `compact` shows only the glyph, with the
/// label as a tooltip.
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
    Some(base.join("SimpleVMManager"))
}

struct Settings {
    theme: ThemeMode,
    sidebar_open: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { theme: ThemeMode::System, sidebar_open: true }
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
                _ => {}
            }
        }
    }
    s
}

fn save_settings(s: &Settings) {
    let Some(dir) = config_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let text = format!("theme={}\nsidebar={}\n", s.theme.as_str(), if s.sidebar_open { "open" } else { "folded" });
    let _ = std::fs::write(dir.join("settings.txt"), text);
}

fn load_history() -> Vec<String> {
    let Some(dir) = config_dir() else { return vec![] };
    let text = std::fs::read_to_string(dir.join("servers.txt")).unwrap_or_default();
    let mut out: Vec<String> = vec![];
    for line in text.lines() {
        let h = line.split('\t').next().unwrap_or("").trim().to_string();
        if !h.is_empty() && !out.contains(&h) {
            out.push(h);
        }
    }
    out
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

/// Segoe UI (the original's font) when the system has it, otherwise egui's built-in font.
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut bold_family = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if cfg!(windows) {
        let dir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
        if let Ok(bytes) = std::fs::read(format!("{}\\Fonts\\segoeui.ttf", dir)) {
            fonts.font_data.insert("segoe".into(), egui::FontData::from_owned(bytes));
            fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "segoe".into());
            bold_family.insert(0, "segoe".into());
        }
        if let Ok(bytes) = std::fs::read(format!("{}\\Fonts\\segoeuib.ttf", dir)) {
            fonts.font_data.insert("segoe-bold".into(), egui::FontData::from_owned(bytes));
            bold_family.insert(0, "segoe-bold".into());
        }
    }
    fonts.families.insert(FontFamily::Name("bold".into()), bold_family);
    ctx.set_fonts(fonts);
}
