//! A server's Overview, and its pool's: who it is and how it's doing, its load over the last ten
//! minutes, how much of it its VMs have taken (amber, then red, near a limit), the VMs on it and
//! what each one uses, and its storage and network cards. The pool's adds its servers up, and lists
//! them, its VMs and its storage. The rest is in the Details tab.

use super::overview::{fit, fmt_rate, pair, panel, row, say, section, section_link, tile_rows, Fig, Lead, Tail, Tile};
use super::*;

/// How many lines a list shows before "Show all".
const FEW: usize = 8;

/// A card of the grid: a section's title and its panel.
type Card<'c> = &'c dyn Fn(&mut Ui, &mut (), &mut Vec<Intent>);

/// Near a limit: amber, then red. By the share of memory left…
fn mem_tone(pal: &Pal, free: f64, total: f64) -> Option<Color32> {
    match free / total.max(1.0) {
        f if f < 0.05 => Some(pal.red),
        f if f < 0.15 => Some(pal.orange),
        _ => None,
    }
}

/// …the CPUs' load over the last minute…
fn cpu_tone(pal: &Pal, load: f64) -> Option<Color32> {
    match load {
        l if l >= 0.9 => Some(pal.red),
        l if l >= 0.75 => Some(pal.orange),
        _ => None,
    }
}

/// …the virtual CPUs given out for each real one…
fn vcpu_tone(pal: &Pal, each: f64) -> Option<Color32> {
    match each {
        e if e > 2.0 => Some(pal.red),
        e if e > 1.0 => Some(pal.orange),
        _ => None,
    }
}

/// …and the share of a storage in use.
fn disk_tone(pal: &Pal, used: f64) -> Option<Color32> {
    match used {
        u if u >= 0.95 => Some(pal.red),
        u if u >= 0.85 => Some(pal.orange),
        _ => None,
    }
}

/// The mean of a series' last minute.
fn recent(s: &[f64]) -> f64 {
    let k = s.len().min(12);
    if k == 0 {
        return 0.0;
    }
    s[s.len() - k..].iter().sum::<f64>() / k as f64
}

/// A line of the facts at the top: its label, its value (soft when it says there's none), a dot
/// before it, and whether a button copies it.
struct Fact {
    label: &'static str,
    value: String,
    dot: Option<Color32>,
    copy: bool,
    faint: bool,
    /// A click on it does something (it lights up under the mouse); and what's said under the mouse.
    link: bool,
    tip: String,
    /// A small button after its value (its label), and whether it can be clicked now.
    button: Option<(&'static str, bool)>,
}

impl Fact {
    fn new(label: &'static str, value: impl Into<String>) -> Fact {
        Fact { label, value: value.into(), dot: None, copy: false, faint: false, link: false, tip: String::new(), button: None }
    }
}

/// What a server's or a pool's load is, for its tiles.
enum Load<'a> {
    /// Not running: why there's none.
    Off(&'a str),
    Reading,
    Failed(String),
    Got(&'a Perf),
}

/// What's in use and what's free: memory (all of it, free, given to running VMs) and CPUs (real,
/// and virtual given to running VMs).
struct Room {
    total: i64,
    free: i64,
    vms_mem: i64,
    cpus: i64,
    vcpus: i64,
}

/// A line in a list of VMs or servers: what it uses now; a click shows it.
struct LoadLine<'a> {
    key: String,
    icon: IconKind,
    name: String,
    /// Softer, after its name (a VM's server in the pool's list, "coordinator").
    aside: String,
    /// Its CPU over the last ten minutes (while it runs); else its state instead.
    cpu: Option<&'a [f64]>,
    state: String,
    mem: String,
    off: bool,
}

impl App {
    /// Reads the load of the servers `hosts` (reference, uuid, address) every five seconds while
    /// they're shown.
    fn poll_hosts(&mut self, ctx: &egui::Context, conn: &Arc<dyn Connection>, hosts: Vec<(String, String, String)>) {
        for (host, uuid, address) in hosts {
            let due = self.host_perf.get(&host).map_or(true, |(t, _)| t.elapsed() >= Duration::from_secs(5));
            if due && self.host_perf_busy.insert(host.clone()) {
                let (tx, ctx, conn) = (self.tx.clone(), ctx.clone(), conn.clone());
                std::thread::spawn(move || {
                    let result = conn.host_perf(&uuid, &address);
                    let _ = tx.send(Msg::HostPerf { host, result });
                    ctx.request_repaint();
                });
            }
        }
        ctx.request_repaint_after(Duration::from_secs(5));
    }

    /// How a server is doing: its dot's colour, and in words.
    fn host_state(&self, h: &HostInfo) -> (Color32, String) {
        let pal = &self.pal;
        if let Some(m) = self.slow_restart(h) {
            (pal.orange, format!("Restarting for {} min: longer than usual", m))
        } else if self.host_rebooting.contains_key(&h.rref) {
            (pal.accent, "Restarting…".into())
        } else if self.host_busy.contains(&h.rref) {
            (pal.red, "Shutting down…".into())
        } else if let Some(m) = self.slow_power_on(h) {
            (pal.orange, format!("Turning on ({} min)…", m))
        } else if self.host_waking.contains_key(&h.rref) {
            (pal.accent, "Turning on…".into())
        } else if let Some(&entering) = self.host_maint.get(&h.rref) {
            (pal.accent, if entering { "Entering maintenance mode…" } else { "Leaving maintenance mode…" }.into())
        } else if h.unreachable {
            (pal.grey, "Not answering".into())
        } else if !h.live {
            (pal.grey, "Off".into())
        } else if h.in_maintenance() {
            (pal.orange, "Maintenance mode".into())
        } else if !h.enabled {
            (pal.orange, "On, not taking new VMs".into())
        } else {
            (pal.green, "On".into())
        }
    }

    /// How many minutes server `h` has been told to turn on without answering, when that's longer
    /// than a server usually takes.
    fn slow_power_on(&self, h: &HostInfo) -> Option<u64> {
        let minutes = self.host_waking.get(&h.rref)?.since.elapsed().as_secs() / 60;
        (minutes >= WAKE_SLOW_MIN).then_some(minutes)
    }

    /// The load read last for a server: its own, and its VMs'.
    fn read_load(&self, h: &HostInfo) -> Option<&Result<HostPerf, String>> {
        self.host_perf.get(&h.rref).map(|p| &p.1)
    }

    /// Lines for `vms`: the running ones first, the busiest first; then the others by name. With
    /// `aside`, each one's server after its name.
    fn vm_lines<'a>(&'a self, s: &'a ServerEntry, d: &'a PoolData, vms: Vec<&'a VmInfo>, aside: bool) -> Vec<LoadLine<'a>> {
        let mut lines: Vec<(f64, LoadLine)> = vms
            .into_iter()
            .map(|v| {
                let down = d.host_down_for(v).is_some();
                let running = v.power == PowerState::Running && !down;
                let cpu = if running {
                    v.host.as_deref().and_then(|h| self.host_perf.get(h)).and_then(|p| p.1.as_ref().ok()).and_then(|p| p.vm(&v.uuid)).map(|p| p.cpu.as_slice()).filter(|c| c.len() >= 2)
                } else {
                    None
                };
                let rank = match (running, v.power) {
                    (true, _) => 1.0 + cpu.and_then(|c| c.last().copied()).unwrap_or(0.0),
                    (false, PowerState::Paused | PowerState::Suspended) => 0.5,
                    _ => 0.0,
                };
                let mem = if matches!(v.power, PowerState::Running | PowerState::Paused) && v.mem_actual > 0 { v.mem_actual } else { v.mem_dyn_max };
                let line = LoadLine {
                    key: format!("vm:{}:{}", s.id, v.rref),
                    icon: Self::vm_icon(s, v),
                    name: v.name.clone(),
                    aside: if aside { v.home().and_then(|h| d.host(h)).map(|h| h.name.clone()).unwrap_or_default() } else { String::new() },
                    cpu,
                    state: if down { "Unknown".into() } else { v.power.label().to_string() },
                    mem: fmt_bytes(mem),
                    off: !running,
                };
                (rank, line)
            })
            .collect();
        lines.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase())));
        lines.into_iter().map(|l| l.1).collect()
    }

    /// A server's Overview.
    pub(super) fn host_overview(&mut self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let Some((s, h)) = self.host_of(sel) else { return };
        let (conn, want) = (s.conn.clone(), if h.is_up() { vec![(h.rref.clone(), h.uuid.clone(), h.address.clone())] } else { vec![] });
        self.poll_hosts(ui.ctx(), &conn, want);
        let this = &*self;
        let Some((s, h)) = this.host_of(sel) else { return };
        let Some(d) = s.data.as_ref() else { return };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0);
        // its own alerts, at the top
        let strip = this.alerts_of(&sel.key);

        // who it is
        let (dot, state) = this.host_state(h);
        let here: Vec<&VmInfo> = d.vms.iter().filter(|v| v.home() == Some(h.rref.as_str())).collect();
        let running = here.iter().filter(|v| v.power == PowerState::Running).count();
        // after its state, a button: Power On while it's off; Exit maintenance while it's in
        // maintenance mode; Take VMs again while it's on but not taking VMs (as left by a restart)
        let enabling = this.host_enabling.contains(&h.rref);
        let maint_op = this.host_maint.get(&h.rref).copied();
        let disabled = h.is_up() && !h.enabled && !h.in_maintenance() && maint_op.is_none() && !this.host_busy.contains(&h.rref) && !this.host_rebooting.contains_key(&h.rref);
        let waking = this.host_waking.contains_key(&h.rref);
        let (button, action) = if waking {
            // (its state says so)
            (None, None)
        } else if let Some((sid, r, _, ok)) = this.power_on_of(sel) {
            (Some(("Power On", ok)), Some(Intent::PowerOn(sid, r)))
        } else if (h.in_maintenance() && h.is_up()) || maint_op == Some(false) {
            (Some(if maint_op.is_some() { ("Leaving…", false) } else { ("Exit maintenance", true) }), Some(Intent::AskMaintenance(s.id, h.rref.clone())))
        } else if disabled {
            (Some(if enabling { ("Asking…", false) } else { ("Take VMs again", true) }), Some(Intent::EnableHost(s.id, h.rref.clone())))
        } else {
            (None, None)
        };
        // (taken once, inside the page's closure)
        let action = std::cell::Cell::new(action);
        let mut left = vec![Fact { dot: Some(dot), button, ..Fact::new("State", state) }];
        if let (true, Some(t)) = (h.is_up(), h.boot_time) {
            left.push(Fact::new("Up for", fmt_duration(now - t)));
        }
        left.push(Fact { copy: true, ..Fact::new("Address", h.address.clone()) });
        left.push(Fact::new("Role", if !d.is_pool() { "Standalone server" } else if h.is_master { "Pool coordinator" } else { "Pool member" }));
        let right = vec![
            Fact::new("Version", or(&h.version, "Unknown").to_string()),
            Fact::new("CPUs", if h.cpu_model.is_empty() { h.cpu_count.to_string() } else { format!("{}  ·  {}", h.cpu_count, h.cpu_model) }),
            Fact::new("Memory", fmt_bytes(h.mem_total)),
            Fact::new("VMs", format!("{} of {} running", running, here.len())),
        ];

        // its load, and what's taken
        let read = this.read_load(h);
        let load = match read {
            _ if h.unreachable => Load::Off("It isn't answering, so its load can't be read."),
            _ if !h.is_up() => Load::Off("Its load shows here while it's on."),
            None => Load::Reading,
            Some(Err(e)) => Load::Failed(e.clone()),
            Some(Ok(p)) => Load::Got(&p.host),
        };
        let on_it = |v: &&VmInfo| v.host.as_deref() == Some(h.rref.as_str()) && matches!(v.power, PowerState::Running | PowerState::Paused);
        let room = Room {
            total: h.mem_total,
            free: h.mem_free,
            vms_mem: d.vms.iter().filter(on_it).map(|v| if v.mem_actual > 0 { v.mem_actual } else { v.mem_dyn_max }).sum(),
            cpus: h.cpu_count,
            vcpus: d.vms.iter().filter(on_it).map(|v| v.vcpus).sum(),
        };
        let vms = this.vm_lines(s, d, here, false);

        // its storage: its own, and the shared storage it should have mounted
        let mut stores: Vec<(&SrInfo, Option<bool>)> = d
            .srs
            .iter()
            .filter(|x| x.kind != "udev" && x.size > 0)
            .filter_map(|x| {
                let link = x.links.iter().find(|l| l.0 == h.rref);
                (x.host.as_deref() == Some(h.rref.as_str()) || link.is_some()).then(|| (x, link.map(|l| l.1)))
            })
            .collect();
        stores.sort_by_key(|(x, _)| (x.host.is_none(), x.name.to_lowercase()));
        let nic_load = read.and_then(|r| r.as_ref().ok()).map(|p| &p.nics);

        let sid = s.id;
        let cpus = h.cpu_count;
        let up = h.is_up();
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            page(ui, |ui, w| {
                if !strip.is_empty() {
                    super::alerts::alert_strip(ui, &pal, &strip, out);
                    ui.add_space(16.0);
                }
                // a restart taking longer than a server usually does: what it may be, what to do
                if let Some(m) = this.slow_restart(h) {
                    let title = format!("{} is taking longer than usual to restart.", h.name);
                    let text = format!(
                        "It was told to restart {} minutes ago and hasn't answered since; a server usually takes 3 to 8 minutes. It may be stuck while starting (its firmware, a disk check, a message waiting for a key) or up but not on the network: look at its screen, or ping {}. The pool goes on without it.",
                        m,
                        if h.address.is_empty() { "it".to_string() } else { h.address.clone() }
                    );
                    crate::newvm::alert(ui, &pal, false, &title, &text, None);
                    ui.add_space(16.0);
                }
                // told to turn on a while ago, no answer yet: what it may be
                if let Some(m) = this.slow_power_on(h) {
                    let title = format!("{} hasn't turned on yet.", h.name);
                    let text = format!(
                        "It was sent a Wake-on-LAN signal {} minutes ago; a server usually answers within 3 to 8. Wake-on-LAN may be off in its firmware (BIOS or UEFI) or for its network card, or it may have no power or no cable. The signal only reaches it on its own network.",
                        m
                    );
                    crate::newvm::alert(ui, &pal, false, &title, &text, None);
                    ui.add_space(16.0);
                }
                if facts(ui, &pal, w, &left, &right) {
                    if let Some(a) = action.take() {
                        out.push(a);
                    }
                }
                ui.add_space(24.0);
                if section_link(ui, &pal, "Load  ·  the last ten minutes", "Performance") {
                    out.push(Intent::Tab(Tab::Performance));
                }
                load_tiles(ui, &pal, w, cpus, load);
                ui.add_space(24.0);
                let cap: Card = &|ui, _, _| {
                    section(ui, &pal, "Capacity");
                    if up {
                        capacity(ui, &pal, &room, "Server");
                    } else {
                        // (an off server's figures say nothing, and its free memory may read none)
                        panel(ui, &pal, |ui| {
                            row(ui, &pal, Some(Lead::Mark("memory")), "Memory", fmt_bytes(h.mem_total).as_str(), Tail::Nothing, false, false);
                            row(ui, &pal, Some(Lead::Mark("cpu")), "CPUs", &h.cpu_count.to_string(), Tail::Nothing, false, true);
                        });
                    }
                };
                let list: Card = &|ui, _, out| {
                    section(ui, &pal, "VMs on this server");
                    load_rows(ui, &pal, "host-vms", &vms, "No VMs on this server", out);
                };
                let sto: Card = &|ui, _, out| {
                    section(ui, &pal, "Storage");
                    panel(ui, &pal, |ui| {
                        if stores.is_empty() {
                            row(ui, &pal, None, "No storage", "", Tail::Nothing, false, true);
                        }
                        for (i, (x, mounted)) in stores.iter().enumerate() {
                            let trouble = match mounted {
                                Some(false) if up => Some("Not mounted here"),
                                _ => None,
                            };
                            if store_row(ui, &pal, d, x, "", trouble, i + 1 == stores.len()).clicked() {
                                out.push(Intent::Select(format!("sr:{}:{}", sid, x.rref)));
                            }
                        }
                    });
                };
                let net: Card = &|ui, _, _| {
                    section(ui, &pal, "Network");
                    panel(ui, &pal, |ui| {
                        if h.nics.is_empty() {
                            row(ui, &pal, None, "No network cards found", "", Tail::Nothing, false, true);
                        }
                        for (i, n) in h.nics.iter().enumerate() {
                            let traffic = nic_load.and_then(|l| l.iter().find(|l| l.0 == n.device));
                            nic_row(ui, &pal, n, traffic.filter(|_| up), i + 1 == h.nics.len());
                        }
                    });
                };
                grid(ui, w, out, [cap, list, sto, net], ["capacity-vms", "storage-network"]);
            });
        });
    }

    /// A pool's Overview: its servers added up.
    pub(super) fn pool_overview(&mut self, ui: &mut Ui, sel: &Node, out: &mut Vec<Intent>) {
        let pal = self.pal;
        let Some(s) = sel.sid.and_then(|id| self.server(id)) else { return };
        let Some(d) = s.data.as_ref() else { return };
        let conn = s.conn.clone();
        let want = d.hosts.iter().filter(|h| h.is_up()).map(|h| (h.rref.clone(), h.uuid.clone(), h.address.clone())).collect();
        self.poll_hosts(ui.ctx(), &conn, want);
        let this = &*self;
        let Some(s) = sel.sid.and_then(|id| this.server(id)) else { return };
        let Some(d) = s.data.as_ref() else { return };
        let up: Vec<&HostInfo> = d.hosts.iter().filter(|h| h.is_up()).collect();

        // one line: its version and its license (the rest, and what's wrong, in its Details)
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0);
        let running = d.vms.iter().filter(|v| v.power == PowerState::Running).count();
        let versions = d.versions();
        let tone = |st: Standing| crate::pool_update::tone(&pal, st);
        let mut line = vec![match versions.len() {
            0 => Fact { faint: true, ..Fact::new("Version", "Unknown") },
            1 => Fact::new("Version", versions[0].clone()),
            _ => Fact { dot: Some(pal.orange), ..Fact::new("Version", versions.join(", ")) },
        }];
        if let Some((text, st)) = d.license(now) {
            line.push(Fact { dot: Some(tone(st)), ..Fact::new("License", text) });
        }
        // its updates (a click opens them)
        // (when they were checked: under the mouse, and in the dialog)
        if let Some((text, st)) = this.pool_update_status(s.id) {
            let (short, when) = text.split_once("  ·  ").map_or((text.clone(), String::new()), |(a, b)| (a.to_string(), b.to_string()));
            let tip = if when.is_empty() { "Click to see them".to_string() } else { format!("{}{}; click to see them", when[..1].to_uppercase(), &when[1..]) };
            line.push(Fact { dot: Some(tone(st)), link: true, tip, ..Fact::new("Updates", short) });
        }

        // its load: its servers' added up (those read so far)
        let reads: Vec<(&HostInfo, Option<&Result<HostPerf, String>>)> = up.iter().map(|h| (*h, this.read_load(h))).collect();
        let got: Vec<(&Perf, f64)> = reads.iter().filter_map(|(h, r)| r.and_then(|r| r.as_ref().ok()).map(|p| (&p.host, h.cpu_count.max(1) as f64))).collect();
        let all = add_up(&got);
        let load = if up.is_empty() {
            Load::Off("Its load shows here while its servers are on.")
        } else if !got.is_empty() {
            Load::Got(&all)
        } else if let Some(e) = reads.iter().find_map(|(_, r)| r.and_then(|r| r.as_ref().err())) {
            Load::Failed(e.clone())
        } else {
            Load::Reading
        };
        let cpus: i64 = up.iter().map(|h| h.cpu_count).sum();
        let on = |v: &&VmInfo| v.host.as_deref().and_then(|h| d.host(h)).map_or(false, |h| h.is_up()) && matches!(v.power, PowerState::Running | PowerState::Paused);
        let room = Room {
            total: up.iter().map(|h| h.mem_total).sum(),
            free: up.iter().map(|h| h.mem_free).sum(),
            vms_mem: d.vms.iter().filter(on).map(|v| if v.mem_actual > 0 { v.mem_actual } else { v.mem_dyn_max }).sum(),
            cpus,
            vcpus: d.vms.iter().filter(on).map(|v| v.vcpus).sum(),
        };

        // its servers, the coordinator first
        let mut hosts: Vec<&HostInfo> = d.hosts.iter().collect();
        hosts.sort_by_key(|h| (!h.is_master, h.name.to_lowercase()));
        let servers: Vec<LoadLine> = hosts
            .iter()
            .map(|h| {
                let cpu = this.read_load(h).and_then(|r| r.as_ref().ok()).map(|p| p.host.cpu.as_slice()).filter(|c| c.len() >= 2 && h.is_up());
                LoadLine {
                    key: format!("host:{}:{}", s.id, h.rref),
                    icon: this.host_icon(h),
                    name: h.name.clone(),
                    aside: if h.is_master { "coordinator".into() } else { String::new() },
                    cpu,
                    state: this.host_state(h).1,
                    mem: if h.is_up() { format!("{} of {}", fmt_bytes(h.mem_total - h.mem_free), fmt_bytes(h.mem_total)) } else { String::new() },
                    off: !h.is_up(),
                }
            })
            .collect();
        let vms = this.vm_lines(s, d, d.vms.iter().collect(), true);
        // its storage: shared first, then each server's own
        let mut stores: Vec<&SrInfo> = d.srs.iter().filter(|x| x.kind != "udev" && x.size > 0).collect();
        stores.sort_by_key(|x| (x.host.is_some(), x.host.as_deref().and_then(|h| d.host(h)).map(|h| h.name.to_lowercase()), x.name.to_lowercase()));

        let sid = s.id;
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            page(ui, |ui, w| {
                if strip(ui, &pal, w, &line).map_or(false, |i| line[i].link) {
                    out.push(Intent::PoolUpdates(Some(sid)));
                }
                ui.add_space(22.0);
                section(ui, &pal, "Load  ·  the last ten minutes");
                load_tiles(ui, &pal, w, cpus, load);
                ui.add_space(24.0);
                let cap: Card = &|ui, _, _| {
                    section(ui, &pal, "Capacity");
                    capacity(ui, &pal, &room, "Servers");
                };
                let srv: Card = &|ui, _, out| {
                    section(ui, &pal, "Servers");
                    load_rows(ui, &pal, "pool-hosts", &servers, "No servers", out);
                };
                let list: Card = &|ui, _, out| {
                    section(ui, &pal, &format!("VMs  ·  {} of {} running", running, d.vms.len()));
                    load_rows(ui, &pal, "pool-vms", &vms, "No VMs in this pool", out);
                };
                let sto: Card = &|ui, _, out| {
                    section(ui, &pal, "Storage");
                    panel(ui, &pal, |ui| {
                        if stores.is_empty() {
                            row(ui, &pal, None, "No storage", "", Tail::Nothing, false, true);
                        }
                        for (i, x) in stores.iter().enumerate() {
                            let aside = x.host.as_deref().and_then(|h| d.host(h)).map_or("", |h| h.name.as_str());
                            let trouble = match d.sr_trouble(x) {
                                Some(t) if t.nowhere => Some("Not mounted"),
                                Some(_) => Some("Not mounted everywhere"),
                                None => None,
                            };
                            if store_row(ui, &pal, d, x, aside, trouble, i + 1 == stores.len()).clicked() {
                                out.push(Intent::Select(format!("sr:{}:{}", sid, x.rref)));
                            }
                        }
                    });
                };
                grid(ui, w, out, [cap, srv, list, sto], ["capacity-servers", "vms-storage"]);
            });
        });
    }
}

/// A page as the VM's Overview has it: `w` wide, a margin on each side.
fn page(ui: &mut Ui, add: impl FnOnce(&mut Ui, f32)) {
    let pad = 20.0;
    let w = ui.available_width() - 2.0 * pad;
    ui.add_space(18.0);
    ui.horizontal(|ui| {
        ui.add_space(pad);
        ui.vertical(|ui| {
            ui.set_width(w);
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui, w);
            ui.add_space(28.0);
        });
    });
}

/// Four cards two by two, each row's two as tall as each other (`keys` names the rows); one
/// under the other in a narrow page.
fn grid(ui: &mut Ui, w: f32, out: &mut Vec<Intent>, cards: [Card; 4], keys: [&str; 2]) {
    if w >= 720.0 {
        let half = (w - 20.0) / 2.0;
        pair(ui, keys[0], half, &mut (), out, cards[0], cards[1]);
        ui.add_space(24.0);
        pair(ui, keys[1], half, &mut (), out, cards[2], cards[3]);
    } else {
        for (i, card) in cards.into_iter().enumerate() {
            if i > 0 {
                ui.add_space(24.0);
            }
            card(ui, &mut (), out);
        }
    }
}

/// The facts at the top in two columns (one under the other in a narrow page).
/// (Whether a fact's button was clicked.)
fn facts(ui: &mut Ui, pal: &Pal, w: f32, left: &[Fact], right: &[Fact]) -> bool {
    let mut clicked = false;
    let mut column = |ui: &mut Ui, cw: f32, list: &[Fact]| {
        ui.vertical(|ui| {
            ui.set_width(cw);
            ui.spacing_mut().item_spacing.y = 0.0;
            let label_w = list.iter().map(|f| ui.painter().layout_no_wrap(f.label.to_string(), FontId::proportional(13.0), pal.ink).size().x).fold(0.0, f32::max) + 24.0;
            for f in list {
                clicked |= fact(ui, pal, cw, label_w, f).1;
            }
        });
    };
    if w >= 640.0 {
        let gap = 40.0;
        let cw = (w - gap) / 2.0;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            column(ui, cw, left);
            column(ui, cw, right);
        });
    } else {
        let all: Vec<&Fact> = left.iter().chain(right).collect();
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let label_w = all.iter().map(|f| ui.painter().layout_no_wrap(f.label.to_string(), FontId::proportional(13.0), pal.ink).size().x).fold(0.0, f32::max) + 24.0;
            for f in all {
                clicked |= fact(ui, pal, w, label_w, f).1;
            }
        });
    }
    clicked
}

/// Facts on one line, one after the other (on more when they don't fit), each its label then its
/// value; a long one cut short, all of it under the mouse.
fn strip(ui: &mut Ui, pal: &Pal, w: f32, list: &[Fact]) -> Option<usize> {
    let gap = 32.0;
    let label = |f: &Fact| ui.painter().layout_no_wrap(f.label.to_string(), FontId::proportional(13.0), pal.ink).size().x;
    let value = |f: &Fact| ui.painter().layout_no_wrap(f.value.clone(), FontId::proportional(14.0), pal.ink).size().x + if f.dot.is_some() { 14.0 } else { 0.0 };
    let wants: Vec<(f32, f32)> = list.iter().map(|f| (label(f) + 12.0, value(f))).collect();
    let mut clicked = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(gap, 0.0);
        for (i, (f, (lw, vw))) in list.iter().zip(wants).enumerate() {
            if fact(ui, pal, (lw + vw).min(w), lw, f).0.clicked() {
                clicked = Some(i);
            }
        }
    });
    clicked
}

/// A fact: its label, then its value (cut short to fit, all of it under the mouse), and its
/// button. Its response, and whether its button was clicked.
fn fact(ui: &mut Ui, pal: &Pal, w: f32, label_w: f32, f: &Fact) -> (egui::Response, bool) {
    let (r, resp) = ui.allocate_exact_size(vec2(w, 30.0), if f.link { Sense::click() } else { Sense::hover() });
    let resp = if f.link { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
    let resp = if f.tip.is_empty() { resp } else { resp.on_hover_text(&f.tip) };
    let p = ui.painter().clone();
    if f.link && resp.hovered() {
        p.rect_filled(r.expand2(vec2(8.0, -1.0)), Rounding::same(6.0), pal.hover);
    }
    p.text(pos2(r.min.x, r.center().y), Align2::LEFT_CENTER, f.label, FontId::proportional(13.0), pal.ink_faint);
    let mut x = r.min.x + label_w;
    if let Some(c) = f.dot {
        p.circle_filled(pos2(x + 4.0, r.center().y), 4.0, c);
        x += 14.0;
    }
    let copy = f.copy && !f.value.is_empty();
    let (text, col) = if f.value.is_empty() { ("None", pal.ink_faint) } else { (f.value.as_str(), if f.faint { pal.ink_faint } else { pal.ink }) };
    let button_w = f.button.map_or(0.0, |(label, _)| p.layout_no_wrap(label.to_string(), FontId::proportional(12.5), pal.ink).size().x + 34.0);
    let (g, cut) = fit(&p, text, FontId::proportional(14.0), col, r.max.x - x - if copy { 30.0 } else { 0.0 } - button_w);
    let gr = Rect::from_min_size(pos2(x, r.center().y - g.size().y / 2.0), g.size());
    p.galley(gr.min, g, col);
    if cut {
        ui.interact(gr, ui.id().with(("fact", f.label)), Sense::hover()).on_hover_text(text);
    }
    if copy {
        let b = Rect::from_center_size(pos2(gr.max.x + 17.0, r.center().y), vec2(22.0, 22.0));
        let resp = ui.interact(b, ui.id().with(("copy", f.label)), Sense::click()).on_hover_text("Copy the address");
        if resp.hovered() {
            p.rect_filled(b, Rounding::same(5.0), pal.hover);
        }
        paint_glyph(&p, Rect::from_center_size(b.center(), vec2(14.0, 14.0)), "copy", pal.ink_soft);
        if resp.clicked() {
            ui.ctx().copy_text(f.value.clone());
        }
    }
    // its button: an outlined pill, the accent's text
    let mut pressed = false;
    if let Some((label, enabled)) = f.button {
        let bw = button_w - 14.0;
        let b = Rect::from_min_size(pos2(gr.max.x + 14.0, r.center().y - 12.0), vec2(bw, 24.0));
        let hit = ui.interact(b, ui.id().with(("fact-button", f.label)), if enabled { Sense::click() } else { Sense::hover() });
        let hit = if enabled { hit.on_hover_cursor(egui::CursorIcon::PointingHand) } else { hit };
        p.rect(b, Rounding::same(12.0), if enabled && hit.hovered() { pal.hover } else { Color32::TRANSPARENT }, Stroke::new(1.0_f32, if enabled { pal.accent.gamma_multiply(0.7) } else { pal.border }));
        p.text(b.center(), Align2::CENTER_CENTER, label, FontId::proportional(12.5), if enabled { pal.accent } else { pal.ink_faint });
        pressed = enabled && hit.clicked();
    }
    (resp, pressed)
}

/// Servers' loads as one: their CPUs weighed by how many each has (the weight given with each),
/// the rest added up; over the time they all have.
fn add_up(perfs: &[(&Perf, f64)]) -> Perf {
    let n = perfs.iter().map(|(p, _)| p.times.len()).min().unwrap_or(0);
    let mut all = Perf { mem_inside: true, ..Default::default() };
    if n == 0 {
        return all;
    }
    let weight: f64 = perfs.iter().map(|(_, w)| w).sum();
    // the last n of each
    let at = |s: &Vec<f64>, k: usize| s.get(s.len().saturating_sub(n) + k).copied().unwrap_or(0.0);
    for k in 0..n {
        all.times.push(perfs[0].0.times[perfs[0].0.times.len() - n + k]);
        all.cpu.push(perfs.iter().map(|(p, w)| at(&p.cpu, k) * w).sum::<f64>() / weight.max(1.0));
        for (out, get) in [
            (&mut all.mem_used, (|p: &Perf| &p.mem_used) as fn(&Perf) -> &Vec<f64>),
            (&mut all.mem_total, |p: &Perf| &p.mem_total),
            (&mut all.disk_read, |p: &Perf| &p.disk_read),
            (&mut all.disk_write, |p: &Perf| &p.disk_write),
            (&mut all.net_rx, |p: &Perf| &p.net_rx),
            (&mut all.net_tx, |p: &Perf| &p.net_tx),
        ] {
            out.push(perfs.iter().map(|(p, _)| at(get(p), k)).sum());
        }
    }
    all
}

/// CPU, memory, disk and network over the last ten minutes, as tiles; or why not.
fn load_tiles(ui: &mut Ui, pal: &Pal, w: f32, cpus: i64, load: Load) {
    let p = match load {
        Load::Off(text) => return say(ui, pal, text, false),
        Load::Reading => return say(ui, pal, "Reading its load…", true),
        Load::Failed(e) => return say(ui, pal, &format!("Couldn't read its load: {}", e), false),
        Load::Got(p) if p.times.len() < 2 => return say(ui, pal, "Its load shows here in a moment…", true),
        Load::Got(p) => p,
    };
    let last = |s: &[f64]| s.last().copied().unwrap_or(0.0);
    let peak = |a: &[f64], b: &[f64]| a.iter().chain(b).fold(1024.0f64, |m, x| m.max(*x)) * 1.15;
    let total = last(&p.mem_total);
    let used = last(&p.mem_used);
    let cpu_tone = cpu_tone(pal, recent(&p.cpu));
    let mem_tone = mem_tone(pal, total - used, total);
    let tiles = [
        Tile { title: "CPU", figs: vec![Fig::Big(format!("{:.0}%", last(&p.cpu) * 100.0)), Fig::Soft(format!("of {} CPU{}", cpus, if cpus == 1 { "" } else { "s" }))], lines: vec![(&p.cpu, cpu_tone.unwrap_or(pal.accent), "")], max: 1.0, note: None, tone: cpu_tone },
        Tile { title: "Memory", figs: vec![Fig::Big(fmt_bytes(used as i64)), Fig::Soft(format!("of {}", fmt_bytes(total as i64)))], lines: vec![(&p.mem_used, mem_tone.unwrap_or(pal.green), "")], max: total.max(1.0), note: None, tone: mem_tone },
        Tile { title: "Disk", figs: vec![Fig::Dot(pal.blue), Fig::Big(fmt_rate(last(&p.disk_read))), Fig::Dot(pal.orange), Fig::Big(fmt_rate(last(&p.disk_write)))], lines: vec![(&p.disk_read, pal.blue, "read"), (&p.disk_write, pal.orange, "write")], max: peak(&p.disk_read, &p.disk_write), note: None, tone: None },
        Tile { title: "Network", figs: vec![Fig::Dot(pal.blue), Fig::Big(fmt_rate(last(&p.net_rx))), Fig::Dot(pal.orange), Fig::Big(fmt_rate(last(&p.net_tx)))], lines: vec![(&p.net_rx, pal.blue, "in"), (&p.net_tx, pal.orange, "out")], max: peak(&p.net_rx, &p.net_tx), note: None, tone: None },
    ];
    tile_rows(ui, pal, w, &tiles);
}

/// Memory and CPUs: what the server (`own`: "Server" or "Servers") keeps, what its running VMs
/// have, and what's free; amber, then red, near a limit.
fn capacity(ui: &mut Ui, pal: &Pal, r: &Room, own: &str) {
    let total = r.total.max(1) as f64;
    let vms = r.vms_mem.min(r.total - r.free).max(0);
    let kept = (r.total - r.free - vms).max(0);
    let tone = mem_tone(pal, r.free as f64, total);
    let warn = tone.map(|c| (c, if c == pal.red { format!("Almost full: new VMs may not start ({} free)", fmt_bytes(r.free)) } else { format!("Little room left for new VMs ({} free)", fmt_bytes(r.free)) }));
    panel(ui, pal, |ui| {
        meter(
            ui,
            pal,
            "memory",
            "Memory",
            &format!("{} of {}", fmt_bytes(r.total - r.free), fmt_bytes(r.total)),
            &[(kept as f64 / total, pal.grey), (vms as f64 / total, tone.unwrap_or(pal.green))],
            &[(pal.grey, format!("{} {}", own, fmt_bytes(kept))), (tone.unwrap_or(pal.green), format!("VMs {}", fmt_bytes(vms))), (pal.stripe, format!("Free {}", fmt_bytes(r.free)))],
            warn,
            "What the server keeps for itself (its control domain and Xen), what its running VMs have, and what's free for more.",
            false,
        );
        let each = r.vcpus as f64 / r.cpus.max(1) as f64;
        let tone = vcpu_tone(pal, each);
        let warn = tone.map(|c| (c, format!("{:.1} virtual CPUs for each real one: busy VMs take turns", each)));
        meter(
            ui,
            pal,
            "cpu",
            "CPUs",
            &format!("{} virtual on {}", r.vcpus, r.cpus),
            &[(each.min(1.0), tone.unwrap_or(pal.accent))],
            &[(tone.unwrap_or(pal.accent), format!("VMs {}", r.vcpus)), (pal.stripe, format!("Free {}", (r.cpus - r.vcpus).max(0)))],
            warn,
            "The CPUs the running VMs have, against the server's real ones. More virtual than real is common; when they're all busy, they wait their turn.",
            true,
        );
    });
}

/// A row with a bar: its icon, label and value (in the warning's colour near a limit), the bar in
/// parts (each a share of the whole, in its colour), and under it a key, or near a limit what that
/// means (more under the mouse).
#[allow(clippy::too_many_arguments)]
fn meter(ui: &mut Ui, pal: &Pal, glyph: &'static str, label: &str, value: &str, parts: &[(f64, Color32)], key: &[(Color32, String)], warn: Option<(Color32, String)>, tip: &str, last: bool) {
    let w = ui.available_width();
    let (r, resp) = ui.allocate_exact_size(vec2(w, 84.0), Sense::hover());
    let p = ui.painter().clone();
    let x = r.min.x + 16.0;
    let top = r.min.y + 24.0;
    paint_glyph(&p, Rect::from_min_size(pos2(x, top - 8.0), vec2(16.0, 16.0)), glyph, pal.icon);
    let tx = x + 28.0;
    let right = r.max.x - 16.0;
    let lab = p.text(pos2(tx, top), Align2::LEFT_CENTER, label, FontId::proportional(14.0), pal.ink);
    let col = warn.as_ref().map_or(pal.ink_soft, |w| w.0);
    let (g, _) = fit(&p, value, FontId::proportional(13.5), col, right - lab.max.x - 16.0);
    p.galley(pos2(right - g.size().x, top - g.size().y / 2.0), g, col);
    bar(&p, pal, Rect::from_min_max(pos2(tx, r.min.y + 42.0), pos2(right, r.min.y + 50.0)), parts);
    let ky = r.min.y + 66.0;
    match &warn {
        Some((c, text)) => {
            let (g, _) = fit(&p, text, FontId::proportional(12.5), *c, right - tx);
            p.galley(pos2(tx, ky - g.size().y / 2.0), g, *c);
        }
        None => {
            let mut kx = tx;
            for (c, text) in key {
                let g = p.layout_no_wrap(text.clone(), FontId::proportional(12.0), pal.ink_faint);
                if kx + 10.0 + g.size().x > right {
                    break;
                }
                if *c == pal.stripe {
                    // what's free: a ring, as the bar's empty track
                    p.circle_stroke(pos2(kx + 3.5, ky), 3.0, Stroke::new(1.2_f32, pal.ink_faint));
                } else {
                    p.circle_filled(pos2(kx + 3.5, ky), 3.5, *c);
                }
                p.galley(pos2(kx + 11.0, ky - g.size().y / 2.0), g.clone(), pal.ink_faint);
                kx += 11.0 + g.size().x + 16.0;
            }
        }
    }
    let tip = match &warn {
        Some(_) => format!("{}\n\n{}", key.iter().map(|k| k.1.as_str()).collect::<Vec<_>>().join("  ·  "), tip),
        None => tip.to_string(),
    };
    resp.on_hover_text(tip);
    if !last {
        p.line_segment([pos2(tx, r.max.y - 0.5), pos2(r.max.x, r.max.y - 0.5)], Stroke::new(1.0_f32, pal.border.gamma_multiply(0.6)));
    }
}

/// A bar in parts, each a share of the whole in its colour, over a track; rounded at its ends.
fn bar(p: &egui::Painter, pal: &Pal, r: Rect, parts: &[(f64, Color32)]) {
    let round = r.height() / 2.0;
    p.rect_filled(r, Rounding::same(round), pal.stripe);
    let mut x = r.min.x;
    for (share, col) in parts {
        let pw = (*share as f32 * r.width()).clamp(0.0, r.max.x - x);
        if pw < 0.5 {
            continue;
        }
        let (first, end) = (x <= r.min.x + 0.5, x + pw >= r.max.x - 0.5);
        let rounding = Rounding { nw: if first { round } else { 0.0 }, sw: if first { round } else { 0.0 }, ne: if end { round } else { 0.0 }, se: if end { round } else { 0.0 } };
        p.rect_filled(Rect::from_min_size(pos2(x, r.min.y), vec2(pw, r.height())), rounding, *col);
        x += pw;
    }
}

/// VMs or servers, each with what it uses now; past a few, the rest behind "Show all".
fn load_rows(ui: &mut Ui, pal: &Pal, id: &str, lines: &[LoadLine], empty: &str, out: &mut Vec<Intent>) {
    panel(ui, pal, |ui| {
        if lines.is_empty() {
            row(ui, pal, None, empty, "", Tail::Nothing, false, true);
            return;
        }
        let all_id = egui::Id::new(("show-all", id));
        let all: bool = ui.data(|d| d.get_temp(all_id)).unwrap_or(false);
        let more = lines.len() > FEW;
        let shown = if all || !more { lines.len() } else { FEW - 1 };
        // the memory column as wide as its widest
        let mem_w = lines.iter().map(|l| ui.painter().layout_no_wrap(l.mem.clone(), FontId::proportional(13.0), pal.ink).size().x).fold(0.0, f32::max);
        for (i, l) in lines[..shown].iter().enumerate() {
            if load_row(ui, pal, l, mem_w, i + 1 == shown && !more).clicked() {
                out.push(Intent::Select(l.key.clone()));
            }
        }
        if more {
            let text = if all { "Show fewer".to_string() } else { format!("Show all {}", lines.len()) };
            if row(ui, pal, Some(Lead::Mark("more")), &text, "", Tail::Nothing, true, true).0.clicked() {
                ui.data_mut(|d| d.insert_temp(all_id, !all));
            }
        }
    });
}

/// A VM or a server: its icon and name (and what's beside it), and on the right its CPU over the
/// last ten minutes and now (or its state when it isn't running), and its memory.
fn load_row(ui: &mut Ui, pal: &Pal, l: &LoadLine, mem_w: f32, last: bool) -> egui::Response {
    let w = ui.available_width();
    let (r, resp) = ui.allocate_exact_size(vec2(w, 46.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let p = ui.painter().clone();
    let hot = resp.hovered();
    if hot {
        p.rect_filled(r.shrink2(vec2(5.0, 2.0)), Rounding::same(8.0), pal.hover);
    }
    let cy = r.center().y;
    paint_icon(&p, Rect::from_min_size(pos2(r.min.x + 16.0, cy - 8.0), vec2(16.0, 16.0)), l.icon, pal, if hot { pal.hover } else { pal.surface });
    let x = r.min.x + 44.0;
    // from the right: its memory, its CPU now, its CPU's last ten minutes
    let mut right = r.max.x - 16.0;
    if !l.mem.is_empty() {
        p.text(pos2(right, cy), Align2::RIGHT_CENTER, &l.mem, FontId::proportional(13.0), if l.off { pal.ink_faint } else { pal.ink_soft });
        right -= mem_w + 18.0;
    }
    match l.cpu {
        Some(cpu) if !l.off => {
            let now = cpu.last().copied().unwrap_or(0.0);
            let tone = cpu_tone(pal, recent(cpu));
            p.text(pos2(right, cy), Align2::RIGHT_CENTER, format!("{:.0}%", now * 100.0), FontId::proportional(13.5), tone.unwrap_or(pal.ink));
            right -= 40.0;
            if right - 64.0 - x > 110.0 {
                spark(&p, Rect::from_min_max(pos2(right - 60.0, cy - 9.0), pos2(right - 4.0, cy + 9.0)), cpu, tone.unwrap_or(pal.accent));
                right -= 72.0;
            }
        }
        _ => {
            let g = p.text(pos2(right, cy), Align2::RIGHT_CENTER, &l.state, FontId::proportional(13.0), pal.ink_faint);
            right = g.min.x - 14.0;
        }
    }
    // its name, and beside it what's beside it, cut short in what's left
    let col = if l.off { pal.ink_soft } else { pal.ink };
    let (g, cut) = fit(&p, &l.name, FontId::proportional(14.0), col, (right - x).max(30.0));
    let end = x + g.size().x;
    p.galley(pos2(x, cy - g.size().y / 2.0), g, col);
    if !l.aside.is_empty() && right - end > 70.0 {
        let (g, _) = fit(&p, &l.aside, FontId::proportional(12.5), pal.ink_faint, right - end - 10.0);
        p.galley(pos2(end + 10.0, cy - g.size().y / 2.0), g, pal.ink_faint);
    }
    if !last {
        p.line_segment([pos2(x, r.max.y - 0.5), pos2(r.max.x, r.max.y - 0.5)], Stroke::new(1.0_f32, pal.border.gamma_multiply(0.6)));
    }
    if cut {
        resp.on_hover_text(&l.name)
    } else {
        resp
    }
}

/// A small line of a CPU's last ten minutes, as high as its peak lets it be.
fn spark(p: &egui::Painter, r: Rect, s: &[f64], col: Color32) {
    if s.len() < 2 {
        return;
    }
    let top = (s.iter().fold(0.0f64, |m, v| m.max(*v)) * 1.15).max(0.2);
    let n = (s.len() - 1) as f32;
    let pts: Vec<egui::Pos2> = s.iter().enumerate().map(|(k, v)| pos2(r.min.x + r.width() * k as f32 / n, r.max.y - r.height() * (v / top).clamp(0.0, 1.0) as f32)).collect();
    p.line_segment([pos2(r.min.x, r.max.y + 0.5), pos2(r.max.x, r.max.y + 0.5)], Stroke::new(1.0_f32, col.gamma_multiply(0.25)));
    p.add(egui::Shape::line(pts, Stroke::new(1.4_f32, col)));
}

/// A small rounded tag in `col` (its text in it, on a wash of it) from `at` (its left middle);
/// how wide it is.
pub(super) fn pill(p: &egui::Painter, pal: &Pal, at: egui::Pos2, text: &str, col: Color32) -> f32 {
    let g = p.layout_no_wrap(text.to_string(), FontId::new(11.0, egui::FontFamily::Name("bold".into())), col);
    let r = Rect::from_min_size(pos2(at.x, at.y - 9.0), vec2(g.size().x + 14.0, 18.0));
    p.rect_filled(r, Rounding::same(9.0), col.gamma_multiply(if pal.dark { 0.2 } else { 0.13 }));
    p.galley(pos2(r.min.x + 7.0, r.center().y - g.size().y / 2.0), g, col);
    r.width()
}

/// A storage: its icon and name (and its server's, in the pool's list), how full it is, and a bar;
/// or, when it isn't mounted where it should be, that instead.
fn store_row(ui: &mut Ui, pal: &Pal, d: &PoolData, x: &SrInfo, aside: &str, trouble: Option<&str>, last: bool) -> egui::Response {
    let w = ui.available_width();
    let (r, resp) = ui.allocate_exact_size(vec2(w, 58.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let p = ui.painter().clone();
    let hot = resp.hovered();
    if hot {
        p.rect_filled(r.shrink2(vec2(5.0, 2.0)), Rounding::same(8.0), pal.hover);
    }
    let top = r.min.y + 22.0;
    paint_icon(&p, Rect::from_min_size(pos2(r.min.x + 16.0, top - 8.0), vec2(16.0, 16.0)), sr_icon(d, x, false), pal, if hot { pal.hover } else { pal.surface });
    let tx = r.min.x + 44.0;
    let right = r.max.x - 16.0;
    let share = x.used as f64 / x.size.max(1) as f64;
    let tone = disk_tone(pal, share);
    let (value, col) = match trouble {
        Some(t) => (t.to_string(), pal.red),
        None => (format!("{} of {}", fmt_bytes(x.used), fmt_bytes(x.size)), tone.unwrap_or(pal.ink_soft)),
    };
    let v = p.text(pos2(right, top), Align2::RIGHT_CENTER, &value, FontId::proportional(13.0), col);
    // the pool's default storage says so after its name
    let default = d.default_sr.as_deref() == Some(x.rref.as_str());
    let (g, cut) = fit(&p, &x.name, FontId::proportional(14.0), pal.ink, (v.min.x - tx - 14.0 - if default { 70.0 } else { 0.0 }).max(30.0));
    let mut end = tx + g.size().x;
    p.galley(pos2(tx, top - g.size().y / 2.0), g, pal.ink);
    if default {
        end += 8.0 + pill(&p, pal, pos2(end + 8.0, top), "Default", pal.green);
    }
    if !aside.is_empty() && v.min.x - end > 70.0 {
        let (g, _) = fit(&p, aside, FontId::proportional(12.5), pal.ink_faint, v.min.x - end - 24.0);
        p.galley(pos2(end + 10.0, top - g.size().y / 2.0), g, pal.ink_faint);
    }
    bar(&p, pal, Rect::from_min_max(pos2(tx, r.min.y + 38.0), pos2(right, r.min.y + 42.0)), &[(share, tone.unwrap_or(pal.blue))]);
    if !last {
        p.line_segment([pos2(tx, r.max.y - 0.5), pos2(r.max.x, r.max.y - 0.5)], Stroke::new(1.0_f32, pal.border.gamma_multiply(0.6)));
    }
    let tip = format!(
        "{}: {} free{}{}",
        x.name,
        fmt_bytes((x.size - x.used).max(0)),
        if x.shared { ", shared by the pool's servers" } else { "" },
        if default { ". The pool's default storage: new VMs' disks go here" } else { "" }
    );
    if cut || trouble.is_none() {
        resp.on_hover_text(tip)
    } else {
        resp
    }
}

/// A network card: its device (and whether the server is managed through it) and what goes in and
/// out of it now; under that its network, its address and its speed.
fn nic_row(ui: &mut Ui, pal: &Pal, n: &HostNic, traffic: Option<&(String, Vec<f64>, Vec<f64>)>, last: bool) {
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(vec2(w, 58.0), Sense::hover());
    let p = ui.painter().clone();
    let top = r.min.y + 21.0;
    paint_icon(&p, Rect::from_min_size(pos2(r.min.x + 16.0, top - 8.0), vec2(16.0, 16.0)), IconKind::Net, pal, pal.surface);
    let tx = r.min.x + 44.0;
    let right = r.max.x - 16.0;
    let dev = p.text(pos2(tx, top), Align2::LEFT_CENTER, &n.device, FontId::proportional(14.0), if n.carrier { pal.ink } else { pal.ink_soft });
    if n.management {
        p.text(pos2(dev.max.x + 10.0, top), Align2::LEFT_CENTER, "management", FontId::proportional(12.5), pal.ink_faint);
    }
    if !n.carrier {
        p.text(pos2(right, top), Align2::RIGHT_CENTER, "Not connected", FontId::proportional(13.0), pal.ink_faint);
    } else if let Some((_, rx, tx_)) = traffic {
        // out, then in, from the right: a dot in its tile's colour before each
        let font = FontId::proportional(13.0);
        let o = p.text(pos2(right, top), Align2::RIGHT_CENTER, fmt_rate(tx_.last().copied().unwrap_or(0.0)), font.clone(), pal.ink_soft);
        p.circle_filled(pos2(o.min.x - 7.0, top), 3.5, pal.orange);
        let i = p.text(pos2(o.min.x - 20.0, top), Align2::RIGHT_CENTER, fmt_rate(rx.last().copied().unwrap_or(0.0)), font, pal.ink_soft);
        p.circle_filled(pos2(i.min.x - 7.0, top), 3.5, pal.blue);
    }
    let speed = match n.speed {
        s if s >= 1000 && s % 1000 == 0 => format!("{} Gb/s", s / 1000),
        s if s >= 1000 => format!("{:.1} Gb/s", s as f64 / 1000.0),
        s if s > 0 => format!("{} Mb/s", s),
        _ => String::new(),
    };
    let about: Vec<&str> = [n.network.as_str(), n.ip.as_str(), if n.carrier { speed.as_str() } else { "" }].into_iter().filter(|t| !t.is_empty()).collect();
    let (g, _) = fit(&p, &about.join("  ·  "), FontId::proportional(12.5), pal.ink_faint, right - tx);
    p.galley(pos2(tx, r.min.y + 40.0 - g.size().y / 2.0), g, pal.ink_faint);
    if !last {
        p.line_segment([pos2(tx, r.max.y - 0.5), pos2(r.max.x, r.max.y - 0.5)], Stroke::new(1.0_f32, pal.border.gamma_multiply(0.6)));
    }
}
