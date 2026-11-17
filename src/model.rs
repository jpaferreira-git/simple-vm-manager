//! Data the app shows, and the VM power-command rules read from the original
//! (see MECHANICS.md for the method addresses each rule comes from).

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
pub enum PowerState {
    Halted,
    Paused,
    Running,
    Suspended,
    #[default]
    Unknown,
}

impl PowerState {
    pub fn parse(s: &str) -> PowerState {
        match s.to_ascii_lowercase().as_str() {
            "halted" => PowerState::Halted,
            "paused" => PowerState::Paused,
            "running" => PowerState::Running,
            "suspended" => PowerState::Suspended,
            _ => PowerState::Unknown,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            PowerState::Halted => "Shut down",
            PowerState::Paused => "Paused",
            PowerState::Running => "Running",
            PowerState::Suspended => "Suspended",
            PowerState::Unknown => "Unknown",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct VmDisk {
    pub name: String,
    pub device: String,
    /// Its drive (VBD) and disc (VDI; empty for an empty DVD drive), to change them.
    pub vbd: String,
    pub vdi: String,
    /// Its storage (reference), and what it's called.
    pub sr: String,
    pub sr_name: String,
    pub kind: String,
    pub size: i64,
}

#[derive(Clone, Debug, Default)]
pub struct VmNic {
    pub device: String,
    /// The card (VIF) and its network's reference, to change them.
    pub vif: String,
    pub net: String,
    pub mac: String,
    pub network: String,
    pub ip: String,
}

#[derive(Clone, Debug, Default)]
pub struct VmInfo {
    pub rref: String,
    pub uuid: String,
    pub name: String,
    pub description: String,
    /// The server it's running on (also while paused or suspended there).
    pub host: Option<String>,
    /// The server whose local storage holds its disks: the only one it can start on.
    pub storage_host: Option<String>,
    /// The home server chosen for it, if any.
    pub affinity: Option<String>,
    pub os: String,
    pub ip: String,
    pub power: PowerState,
    pub mem_static_max: i64,
    pub mem_dyn_min: i64,
    pub mem_dyn_max: i64,
    pub vcpus: i64,
    pub allowed: Vec<String>,
    pub disks: Vec<VmDisk>,
    pub nics: Vec<VmNic>,
    // details for the General tab
    pub tags: Vec<String>,
    /// XenCenter's folder for it, like "/Servers/Linux".
    pub folder: String,
    /// The guest's kernel, as its tools report it.
    pub kernel: String,
    /// "HVM", "PV", "PVH" or "PV in PVH".
    pub virt_mode: String,
    /// "BIOS" or "UEFI" (HVM guests).
    pub firmware: String,
    /// "On", "Off" or "Automatic" (UEFI guests).
    pub secure_boot: String,
    /// The boot order of an HVM guest, like "DVD drive, Disk".
    pub boot_order: String,
    pub auto_power_on: bool,
    /// High availability: "Restart", "Best effort", or empty when off.
    pub ha: String,
    /// Its snapshots, the newest first.
    pub snapshots: Vec<SnapshotInfo>,
    /// The snapshot its current state comes from: the last one taken, or the one it was last
    /// put back to.
    pub parent: Option<String>,
    pub vcpus_max: i64,
    pub mem_static_min: i64,
    /// The memory the VM has right now (while running).
    pub mem_actual: i64,
    /// When it last started, in seconds since 1970 (while running).
    pub started: Option<i64>,
    pub tools_installed: bool,
    /// The guest tools' version, like "9.4.0"; empty when not known.
    pub tools: String,
    /// Every IP address its tools report.
    pub ips: Vec<String>,
}

/// A change to a VM, made on its Overview.
#[derive(Clone, Debug, PartialEq)]
pub enum VmChange {
    Name(String),
    Description(String),
    /// Starts when its server does (other_config auto_poweron, with the pool's).
    AutoPowerOn(bool),
    /// A BIOS VM's boot order, as letters: c disk, d DVD drive, n network ("cdn").
    BootOrder(String),
    /// CPUs (and how many to a socket) and memory; the VM has to be off.
    Resources { vcpus: i64, cores_per_socket: i64, memory: i64 },
    AddDisk { name: String, size: i64, sr: String },
    /// Larger only; the VM has to be off.
    GrowDisk { vdi: String, size: i64 },
    /// Its drive goes, and the disk with it unless something else has it; the VM has to be off.
    RemoveDisk { vbd: String, vdi: String },
    /// An ISO in the DVD drive `vbd`, or none.
    Dvd { vbd: String, iso: Option<String> },
    AddNic { net: String },
    MoveNic { vif: String, net: String },
    RemoveNic { vif: String },
}

impl VmChange {
    /// What's being done, for the logs: "Renaming 'Web' to 'Web 2'".
    pub fn text(&self, vm: &str) -> String {
        match self {
            VmChange::Name(n) => format!("Renaming VM '{}' to '{}'", vm, n),
            VmChange::Description(_) => format!("Changing the description of VM '{}'", vm),
            VmChange::AutoPowerOn(true) => format!("Making VM '{}' start with its server", vm),
            VmChange::AutoPowerOn(false) => format!("Making VM '{}' stay off when its server starts", vm),
            VmChange::BootOrder(_) => format!("Changing the boot order of VM '{}'", vm),
            VmChange::Resources { vcpus, memory, .. } => format!("Giving VM '{}' {} CPU{} and {} of memory", vm, vcpus, if *vcpus == 1 { "" } else { "s" }, fmt_bytes(*memory)),
            VmChange::AddDisk { size, .. } => format!("Adding a {} disk to VM '{}'", fmt_bytes(*size), vm),
            VmChange::GrowDisk { size, .. } => format!("Growing a disk of VM '{}' to {}", vm, fmt_bytes(*size)),
            VmChange::RemoveDisk { .. } => format!("Removing a disk from VM '{}'", vm),
            VmChange::Dvd { iso: Some(_), .. } => format!("Putting an ISO in the DVD drive of VM '{}'", vm),
            VmChange::Dvd { iso: None, .. } => format!("Ejecting the DVD of VM '{}'", vm),
            VmChange::AddNic { .. } => format!("Adding a network card to VM '{}'", vm),
            VmChange::MoveNic { .. } => format!("Moving a network card of VM '{}' to another network", vm),
            VmChange::RemoveNic { .. } => format!("Removing a network card from VM '{}'", vm),
        }
    }
}

/// A BIOS VM's boot order (HVM_boot_params order: c disk, d DVD drive, n network) in words.
pub fn boot_text(letters: &str) -> String {
    letters
        .chars()
        .filter_map(|c| match c {
            'c' => Some("Disk"),
            'd' => Some("DVD drive"),
            'n' => Some("Network"),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The other way round: "Disk, DVD drive, Network" as "cdn".
pub fn boot_letters(text: &str) -> String {
    text.split(", ")
        .filter_map(|w| match w {
            "Disk" => Some('c'),
            "DVD drive" => Some('d'),
            "Network" => Some('n'),
            _ => None,
        })
        .collect()
}

/// A VM's recent load, from its server's records (one point every 5 seconds, the last 10
/// minutes), oldest first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Perf {
    pub times: Vec<i64>,
    /// CPU in use, 0 to 1 (all its CPUs together).
    pub cpu: Vec<f64>,
    /// Memory in use (what its tools say the system uses, else what it has), and what it has;
    /// bytes.
    pub mem_used: Vec<f64>,
    pub mem_total: Vec<f64>,
    /// Its guest tools say how much memory is free inside it (memory_internal_free): without
    /// them, or with tools that don't say (XenServer VM Tools for Windows 9), what it uses isn't
    /// known and `mem_used` is all it has.
    pub mem_inside: bool,
    /// Disk and network, bytes per second.
    pub disk_read: Vec<f64>,
    pub disk_write: Vec<f64>,
    pub net_rx: Vec<f64>,
    pub net_tx: Vec<f64>,
}

/// The server's own time at the end of an rrd_updates answer (XML <end> or JSON "end"): its clock
/// may differ from this computer's.
pub fn rrd_end(text: &str) -> Option<i64> {
    let t = text.trim_start();
    if t.starts_with('{') {
        let i = t.find("\"end\"")?;
        let rest = t[i + 5..].trim_start_matches([':', ' ', '"']);
        return rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok();
    }
    t.split("<end>").nth(1)?.split("</end>").next()?.trim().parse().ok()
}

/// A server's rrd_updates answer read for VM `uuid` (XML, or JSON from newer servers): its CPUs
/// averaged, its memory, its disks' and network cards' traffic added up. Gaps (NaN) count as
/// nothing.
pub fn parse_rrd_updates(text: &str, uuid: &str) -> Perf {
    let (legend, rows) = rrd_table(text);
    perf_from(&legend, rows, uuid)
}

/// A server's recent load and its VMs', from one rrd_updates answer (asked with host=true).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HostPerf {
    /// The server's: its CPUs averaged, its memory in use (everything but what's free) and all of
    /// it, its VMs' disks added up, and its network cards' traffic added up.
    pub host: Perf,
    /// Each network card's (its device, like "eth0"): in and out, bytes per second.
    pub nics: Vec<(String, Vec<f64>, Vec<f64>)>,
    /// Each VM running there (its uuid), as `parse_rrd_updates` reads it.
    pub vms: Vec<(String, Perf)>,
}

impl HostPerf {
    pub fn vm(&self, uuid: &str) -> Option<&Perf> {
        self.vms.iter().find(|v| v.0 == uuid).map(|v| &v.1)
    }
}

/// A server's rrd_updates answer read for the server `uuid` and the VMs in it.
pub fn parse_host_rrd(text: &str, uuid: &str) -> HostPerf {
    #[derive(Clone, Copy, PartialEq)]
    enum C {
        Cpu,
        CpuAvg,
        MemTotal,
        MemFree,
        /// a card's traffic (which card), and all of its physical cards' (aggr)
        Rx(usize),
        Tx(usize),
        AllRx,
        AllTx,
        Other,
    }
    let (legend, mut rows) = rrd_table(text);
    rows.sort_by_key(|r| r.0);
    let mut uuids: Vec<&str> = legend.iter().filter_map(|e| e.split(":vm:").nth(1)?.split(':').next()).collect();
    uuids.sort();
    uuids.dedup();
    let vms: Vec<(String, Perf)> = uuids.iter().map(|u| (u.to_string(), perf_from(&legend, rows.clone(), u))).collect();
    let prefix = format!(":host:{}:", uuid);
    let mut devices: Vec<String> = vec![];
    let cols: Vec<C> = legend
        .iter()
        .map(|e| match e.split_once(prefix.as_str()).map(|(_, n)| n) {
            Some(n) if n.starts_with("cpu") && n.len() > 3 && n[3..].chars().all(|c| c.is_ascii_digit()) => C::Cpu,
            Some("cpu_avg") => C::CpuAvg,
            Some("memory_total_kib") => C::MemTotal,
            Some("memory_free_kib") => C::MemFree,
            Some("pif_aggr_rx") => C::AllRx,
            Some("pif_aggr_tx") => C::AllTx,
            Some(n) if n.starts_with("pif_") && (n.ends_with("_rx") || n.ends_with("_tx")) && n.len() > 7 => {
                let dev = &n[4..n.len() - 3];
                if dev == "lo" {
                    return C::Other;
                }
                let i = devices.iter().position(|d| d == dev).unwrap_or_else(|| {
                    devices.push(dev.to_string());
                    devices.len() - 1
                });
                if n.ends_with("_rx") {
                    C::Rx(i)
                } else {
                    C::Tx(i)
                }
            }
            _ => C::Other,
        })
        .collect();
    let has = |c: C| cols.contains(&c);
    let cpus = cols.iter().filter(|c| **c == C::Cpu).count();
    let mut nics: Vec<(String, Vec<f64>, Vec<f64>)> = devices.iter().map(|d| (d.clone(), vec![], vec![])).collect();
    let mut host = Perf { mem_inside: true, ..Default::default() };
    for (k, (t, vals)) in rows.iter().enumerate() {
        let sum = |want: C| -> f64 { cols.iter().zip(vals).filter(|(c, _)| **c == want).map(|(_, v)| *v).sum() };
        host.times.push(*t);
        host.cpu.push(if cpus > 0 { sum(C::Cpu) / cpus as f64 } else { sum(C::CpuAvg) }.clamp(0.0, 1.0));
        let (total, free) = (sum(C::MemTotal) * 1024.0, sum(C::MemFree) * 1024.0);
        host.mem_total.push(total);
        host.mem_used.push((total - free).max(0.0));
        for (i, n) in nics.iter_mut().enumerate() {
            n.1.push(sum(C::Rx(i)));
            n.2.push(sum(C::Tx(i)));
        }
        // all the cards': the server's own count when it has one (a bond's members aren't counted twice)
        host.net_rx.push(if has(C::AllRx) { sum(C::AllRx) } else { nics.iter().map(|n| n.1[k]).sum() });
        host.net_tx.push(if has(C::AllTx) { sum(C::AllTx) } else { nics.iter().map(|n| n.2[k]).sum() });
        host.disk_read.push(vms.iter().map(|v| v.1.disk_read.get(k).copied().unwrap_or(0.0)).sum());
        host.disk_write.push(vms.iter().map(|v| v.1.disk_write.get(k).copied().unwrap_or(0.0)).sum());
    }
    nics.sort_by(|a, b| a.0.cmp(&b.0));
    HostPerf { host, nics, vms }
}

/// An rrd_updates answer's legend (its columns' names) and rows (a time and a value per column),
/// from XML or JSON.
fn rrd_table(text: &str) -> (Vec<String>, Vec<(i64, Vec<f64>)>) {
    if text.trim_start().starts_with('{') {
        return rrd_json(text);
    }
    rrd_xml(text)
}

/// {"meta": {…, "legend": […]}, "data": [{"t": …, "values": […]}, …]}, read leniently: servers
/// send its numbers in quotes or not, and NaN bare.
fn rrd_json(text: &str) -> (Vec<String>, Vec<(i64, Vec<f64>)>) {
    let number = |s: &str| s.trim().trim_matches('"').trim().parse::<f64>().ok().filter(|v| v.is_finite()).unwrap_or(0.0);
    let Some(l) = text.find("legend") else { return (vec![], vec![]) };
    let (Some(a), Some(b)) = (text[l..].find('['), text[l..].find(']')) else { return (vec![], vec![]) };
    let legend: Vec<String> = text[l + a + 1..l + b].split(',').map(|s| s.trim().trim_matches('"').to_string()).filter(|s| !s.is_empty()).collect();
    let mut rows: Vec<(i64, Vec<f64>)> = vec![];
    let data = &text[l + b..];
    let data = &data[data.find("data").unwrap_or(0)..];
    // each row: an object, its time before its values
    for obj in data.split('{').skip(1) {
        let Some(v) = obj.find("values") else { continue };
        let t = obj[..v].split_once(':').and_then(|(_, x)| x.trim_start_matches([' ', '"']).split(|c: char| !c.is_ascii_digit()).next()?.parse::<i64>().ok());
        let after = &obj[v..];
        let (Some(a), Some(b)) = (after.find('['), after.find(']')) else { continue };
        if let Some(t) = t {
            rows.push((t, after[a + 1..b].split(',').map(number).collect()));
        }
    }
    (legend, rows)
}

fn rrd_xml(xml: &str) -> (Vec<String>, Vec<(i64, Vec<f64>)>) {
    let tags = |name: &str| -> Vec<String> {
        let (open, close) = (format!("<{}>", name), format!("</{}>", name));
        let mut out = vec![];
        let mut rest = xml;
        while let Some(i) = rest.find(&open) {
            let after = &rest[i + open.len()..];
            let Some(j) = after.find(&close) else { break };
            out.push(after[..j].to_string());
            rest = &after[j + close.len()..];
        }
        out
    };
    let mut rows: Vec<(i64, Vec<f64>)> = vec![];
    for row in tags("row") {
        let t = row.split("<t>").nth(1).and_then(|x| x.split("</t>").next()).and_then(|x| x.trim().parse::<i64>().ok());
        let vals: Vec<f64> = row.split("<v>").skip(1).map(|x| x.split("</v>").next().unwrap_or("").trim().parse::<f64>().ok().filter(|v| v.is_finite()).unwrap_or(0.0)).collect();
        if let Some(t) = t {
            rows.push((t, vals));
        }
    }
    (tags("entry"), rows)
}

/// The figures for VM `uuid` from a legend (its columns' names) and rows (a time and a value per
/// column).
fn perf_from(legend: &[String], mut rows: Vec<(i64, Vec<f64>)>, uuid: &str) -> Perf {
    #[derive(Clone, Copy, PartialEq)]
    enum C {
        Cpu,
        Mem,
        MemFree,
        Read,
        Write,
        Rx,
        Tx,
        Other,
    }
    let prefix = format!(":vm:{}:", uuid);
    let cols: Vec<C> = legend
        .iter()
        .map(|e| match e.split_once(prefix.as_str()).map(|(_, n)| n) {
            Some(n) if n.starts_with("cpu") && n[3..].chars().all(|c| c.is_ascii_digit()) && n.len() > 3 => C::Cpu,
            Some("memory") => C::Mem,
            Some("memory_internal_free") => C::MemFree,
            Some(n) if n.starts_with("vbd_") && n.ends_with("_read") => C::Read,
            Some(n) if n.starts_with("vbd_") && n.ends_with("_write") => C::Write,
            Some(n) if n.starts_with("vif_") && n.ends_with("_rx") => C::Rx,
            Some(n) if n.starts_with("vif_") && n.ends_with("_tx") => C::Tx,
            _ => C::Other,
        })
        .collect();
    let cpus = cols.iter().filter(|c| **c == C::Cpu).count().max(1) as f64;
    rows.sort_by_key(|r| r.0);
    let mut p = Perf::default();
    for (t, vals) in rows {
        let sum = |want: C| -> f64 { cols.iter().zip(&vals).filter(|(c, _)| **c == want).map(|(_, v)| *v).sum() };
        let (mem, free) = (sum(C::Mem), sum(C::MemFree) * 1024.0);
        p.times.push(t);
        p.cpu.push((sum(C::Cpu) / cpus).clamp(0.0, 1.0));
        p.mem_total.push(mem);
        p.mem_used.push(if free > 0.0 && free < mem { mem - free } else { mem });
        p.mem_inside |= free > 0.0;
        p.disk_read.push(sum(C::Read));
        p.disk_write.push(sum(C::Write));
        p.net_rx.push(sum(C::Rx));
        p.net_tx.push(sum(C::Tx));
    }
    p
}

/// A VM's snapshot: its disks as they were, and maybe its memory.
#[derive(Clone, Debug, Default)]
pub struct SnapshotInfo {
    pub rref: String,
    pub name: String,
    pub description: String,
    /// When it was taken, in seconds since 1970.
    pub time: Option<i64>,
    /// It holds the VM's memory too (a checkpoint): reverting to it resumes the VM where it was.
    pub memory: bool,
    /// The VM was running when it was taken.
    pub was_running: bool,
}

impl VmInfo {
    /// The server the VM belongs with: where it runs, or for a VM that's off, the server with its
    /// disks, or else its home server (as the original's VM.Home). None: any server in the pool.
    pub fn home(&self) -> Option<&str> {
        self.host.as_deref().or(self.storage_host.as_deref()).or(self.affinity.as_deref())
    }

    pub fn can(&self, op: &str) -> bool {
        self.allowed.iter().any(|a| a == op)
    }

    /// A snapshot can be taken now: of its disks, or (running) of its disks and memory.
    pub fn can_snapshot(&self) -> bool {
        self.can("snapshot") || self.can("checkpoint")
    }

    pub fn snapshot(&self, r: &str) -> Option<&SnapshotInfo> {
        self.snapshots.iter().find(|x| x.rref == r)
    }
}

#[derive(Clone, Debug, Default)]
pub struct HostInfo {
    pub rref: String,
    pub uuid: String,
    pub name: String,
    pub description: String,
    pub address: String,
    pub version: String,
    pub cpu_model: String,
    pub cpu_count: i64,
    pub mem_total: i64,
    pub mem_free: i64,
    pub enabled: bool,
    pub is_master: bool,
    /// Its control domain (dom0): the VM whose console is the server's own text console.
    pub control_domain: String,
    /// Up and answering (its metrics say it's live); false once it's shut down.
    pub live: bool,
    /// The pool still counts it as live, but it doesn't answer: one that goes down without
    /// shutting down (power cut, crash, cable) stays live for ten minutes, till the pool gives up
    /// on its heartbeat (xapi's host_assumed_dead_interval). See `RealConnection::knock_hosts`.
    pub unreachable: bool,
    /// When it last started, in seconds since 1970 (its other_config's boot_time).
    pub boot_time: Option<i64>,
    /// Its product's name, like "XenServer" or "XCP-ng".
    pub brand: String,
    /// Its license: its edition as the server names it ("xenserver-premium", "trial"…), its name
    /// for people (sku_marketing_name, when it says), and when it runs out.
    pub edition: String,
    pub license_name: String,
    pub license_expiry: Option<i64>,
    /// Its network cards: the physical ones and bonds (a bond's members and VLANs are in those).
    pub nics: Vec<HostNic>,
}

/// A server's network card (a PIF).
#[derive(Clone, Debug, Default)]
pub struct HostNic {
    /// Like "eth0" or "bond0".
    pub device: String,
    /// The name of the network it's on.
    pub network: String,
    /// Its address, when it has one (the card the server is managed through, a storage card).
    pub ip: String,
    /// The server is managed through it.
    pub management: bool,
    /// A cable is in and the link is up, at `speed` Mbit/s (0 when not known).
    pub carrier: bool,
    pub speed: i64,
}

impl HostInfo {
    /// On and answering.
    pub fn is_up(&self) -> bool {
        self.live && !self.unreachable
    }

    /// Its license's name for people: the server's own, else its edition's words ("Premium
    /// Edition"); empty when it has none to speak of (XCP-ng).
    pub fn edition_name(&self) -> String {
        if !self.license_name.is_empty() {
            return self.license_name.clone();
        }
        let e = self.edition.trim().to_lowercase();
        if e.is_empty() || e == "xcp-ng" {
            return String::new();
        }
        let words: Vec<String> = e.trim_start_matches("xenserver-").split(['-', '_', ' ']).filter(|w| !w.is_empty()).map(|w| w[..1].to_uppercase() + &w[1..]).collect();
        format!("{} Edition", words.join(" "))
    }

    /// Its version with its product's name: "XenServer 8.4.0".
    pub fn version_text(&self) -> String {
        match (self.brand.is_empty(), self.version.is_empty()) {
            (_, true) => String::new(),
            (true, false) => self.version.clone(),
            (false, false) => format!("{} {}", self.brand, self.version),
        }
    }
}

/// How something stands: fine, worth a look, or wrong.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Standing {
    Good,
    Watch,
    Bad,
}

/// A license running out sooner than this is worth a look.
const LICENSE_SOON: i64 = 30 * 86_400;

#[derive(Clone, Debug, Default)]
pub struct SrInfo {
    pub rref: String,
    pub uuid: String,
    pub name: String,
    pub description: String,
    pub kind: String,
    pub size: i64,
    pub used: i64,
    pub shared: bool,
    /// The server this storage belongs to (local disks, DVD drives, USB); None for shared storage.
    pub host: Option<String>,
    /// Where it lives, like "//nas/isos" or "nas:/mnt/tank/isos" (from its device configuration).
    pub location: String,
    /// How it's reached, in words: "Windows share (SMB 3.0)", "NFS (v3)"…
    pub share: String,
    /// An ISO library's discs.
    pub files: Vec<IsoFile>,
    /// Its connections to the servers (PBDs): each one's server, and whether the storage is
    /// mounted there now. None at all: the pool has no settings for reaching it (detached).
    pub links: Vec<(String, bool)>,
}

/// Storage that should be mounted somewhere it isn't.
#[derive(Clone, Debug, PartialEq)]
pub struct SrTrouble {
    /// The servers (on, and that should have it) where it isn't mounted: the pool's coordinator
    /// first.
    pub missing: Vec<String>,
    /// It isn't mounted on any server.
    pub nowhere: bool,
    /// The pool has no settings for reaching it any more (XenCenter's Detach does that): Repair
    /// has to ask where it is.
    pub detached: bool,
}

#[derive(Clone, Debug, Default)]
pub struct IsoFile {
    /// The file's disc (VDI) on the server.
    pub vdi: String,
    pub name: String,
    pub size: i64,
    /// The disc's description (VDI name_description), kept by the pool, not on the share.
    pub description: String,
    /// The VMs that have it in a DVD drive, each with whether that holds on to it (running or
    /// suspended); a shut-down VM's drive can be emptied.
    pub in_drives: Vec<(String, bool)>,
    /// The server allows deleting it (its allowed_operations has destroy).
    pub can_delete: bool,
}

impl IsoFile {
    /// The running (or suspended) VMs that keep it from being deleted.
    pub fn held_by(&self) -> Vec<&str> {
        self.in_drives.iter().filter(|d| d.1).map(|d| d.0.as_str()).collect()
    }
}

#[derive(Clone, Debug, Default)]
pub struct NetInfo {
    pub rref: String,
    pub uuid: String,
    pub name: String,
    pub description: String,
    pub bridge: String,
    pub mtu: i64,
}

#[derive(Clone, Debug, Default)]
pub struct PoolData {
    pub name: String,
    pub hosts: Vec<HostInfo>,
    pub vms: Vec<VmInfo>,
    pub srs: Vec<SrInfo>,
    pub networks: Vec<NetInfo>,
    /// The built-in templates new VMs are made from.
    pub templates: Vec<TemplateInfo>,
    /// The pool's default storage, if set.
    pub default_sr: Option<String>,
    /// The templates people made (from their VMs or snapshots), not the built-in ones; they have
    /// disks of their own, copied into each VM made from them.
    pub user_templates: Vec<VmInfo>,
}

/// A built-in VM template: what it sets up and recommends.
#[derive(Clone, Debug, Default)]
pub struct TemplateInfo {
    pub rref: String,
    pub name: String,
    /// It boots with UEFI unless told otherwise.
    pub uefi: bool,
    /// Its platform:secureboot ("true", "false", "auto" = when the pool has the certificates).
    pub secure_boot: String,
    /// It brings a virtual TPM (Windows 11): UEFI only.
    pub vtpm: bool,
    pub supports_uefi: bool,
    pub supports_secure_boot: bool,
    /// Its minimum memory, and its usual CPUs and disk size.
    pub mem_min: i64,
    pub vcpus: i64,
    pub disk: i64,
}

impl TemplateInfo {
    pub fn is_windows(&self) -> bool {
        self.name.to_lowercase().starts_with("windows")
    }
}

/// The name of the template used when the ISO isn't recognised: BIOS, nothing assumed.
pub const OTHER_MEDIA: &str = "Other install media";

/// How a new VM boots.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Boot {
    Bios,
    Uefi,
    UefiSecure,
}

/// Everything needed to create a VM, as chosen in the New VM dialog.
#[derive(Clone, Debug, Default)]
pub struct NewVm {
    pub name: String,
    pub description: String,
    pub template: String,
    /// The ISO file's disc, put in the DVD drive.
    pub iso: String,
    pub vcpus: i64,
    pub cores_per_socket: i64,
    pub memory: i64,
    pub disk: i64,
    /// The storage for its disk.
    pub sr: String,
    /// The network for its one network card; None for none.
    pub network: Option<String>,
    /// Its home server (required when its disk or ISO is on one server's own storage).
    pub host: Option<String>,
    /// The boot mode, when it differs from the template's.
    pub boot: Option<Boot>,
    pub start: bool,
    /// `template` is one of the user's own templates, with its disks: they're copied (in an
    /// instant on their own storage, or in full onto `copy_to`), and there's no ISO or new disk.
    pub from_template: bool,
    pub copy_to: Option<String>,
    /// Keep the template's network cards (`network` is ignored).
    pub keep_nics: bool,
    /// What the references above are called (the template, storage, ISO, network, server), for
    /// the steps shown while it's made.
    pub names: std::collections::HashMap<String, String>,
    /// Storage to make sure answers before anything is made: the ISO's library, and the disk's
    /// storage when it's reached over the network.
    pub check: Vec<String>,
}

impl SrInfo {
    /// A library of installation discs.
    pub fn is_iso(&self) -> bool {
        self.kind == "iso"
    }

    /// A server's "DVD drives" or "Removable storage" (type udev) with no disc or USB drive in it:
    /// left out of the lists.
    pub fn is_empty_drive(&self) -> bool {
        self.kind == "udev" && self.size == 0
    }

    /// A server's own storage: its local disks, DVD drives and USB (udev), or anything else not
    /// shared that isn't an ISO library. It gets no dot or "!" and no Repair.
    pub fn is_local(&self) -> bool {
        self.kind == "udev" || (!self.shared && !self.is_iso())
    }

    /// Storage that can go away under the pool: shared storage, and anything reached over the
    /// network or a SAN (NFS, SMB, iSCSI, Fibre Channel, GFS2). A share that's gone doesn't
    /// fail, it never answers, so it's checked before it's used.
    pub fn can_go_offline(&self) -> bool {
        self.shared || matches!(self.kind.as_str(), "iso" | "nfs" | "smb" | "cifs" | "lvmoiscsi" | "lvmohba" | "gfs2")
    }
}

impl PoolData {
    pub fn host(&self, r: &str) -> Option<&HostInfo> {
        self.hosts.iter().find(|h| h.rref == r)
    }
    pub fn vm(&self, r: &str) -> Option<&VmInfo> {
        self.vms.iter().find(|v| v.rref == r)
    }
    pub fn sr(&self, r: &str) -> Option<&SrInfo> {
        self.srs.iter().find(|s| s.rref == r)
    }
    pub fn net(&self, r: &str) -> Option<&NetInfo> {
        self.networks.iter().find(|n| n.rref == r)
    }
    /// For a VM the pool says is running (or paused) on a server that's off or not answering, that
    /// server: what the VM is really doing can't be known, and nothing can be done to it there.
    pub fn host_down_for(&self, v: &VmInfo) -> Option<&HostInfo> {
        if !matches!(v.power, PowerState::Running | PowerState::Paused) {
            return None;
        }
        self.host(v.host.as_deref()?).filter(|h| !h.is_up())
    }
    pub fn user_template(&self, r: &str) -> Option<&VmInfo> {
        self.user_templates.iter().find(|t| t.rref == r)
    }

    /// Where a storage should be mounted but isn't, if anywhere: a server that's on and has its
    /// connection unplugged, or (shared storage) a server of the pool with no connection to it.
    /// A server's own storage (`is_local`) never is.
    pub fn sr_trouble(&self, x: &SrInfo) -> Option<SrTrouble> {
        if x.is_local() {
            return None;
        }
        let nowhere = !x.links.iter().any(|l| l.1);
        if x.links.is_empty() {
            return Some(SrTrouble { missing: self.hosts.iter().filter(|h| h.is_up()).map(|h| h.rref.clone()).collect(), nowhere, detached: true });
        }
        let mut missing: Vec<&HostInfo> = self
            .hosts
            .iter()
            .filter(|h| h.is_up())
            .filter(|h| match x.links.iter().find(|l| l.0 == h.rref) {
                Some(l) => !l.1,
                None => x.shared,
            })
            .collect();
        missing.sort_by_key(|h| !h.is_master);
        if missing.is_empty() {
            return None;
        }
        Some(SrTrouble { missing: missing.iter().map(|h| h.rref.clone()).collect(), nowhere, detached: false })
    }
    /// The pool's license in words (its edition, till when or how soon it runs out) and how it
    /// stands: running out within 30 days, a trial, or not the same on every server is worth a
    /// look; run out is wrong. None when its servers have no license to speak of (XCP-ng).
    pub fn license(&self, now: i64) -> Option<(String, Standing)> {
        let hosts: Vec<&HostInfo> = self.hosts.iter().filter(|h| !h.edition_name().is_empty()).collect();
        let main = hosts.iter().find(|h| h.is_master).or(hosts.first())?;
        let name = main.edition_name();
        let mixed = hosts.iter().any(|h| h.edition != main.edition);
        let trial = ["trial", "free"].iter().any(|w| main.edition.to_lowercase().contains(w));
        // (one that never runs out says so with a date far away)
        let expiry = hosts.iter().filter_map(|h| h.license_expiry).filter(|t| *t < now + 10 * 365 * 86_400).min();
        let fine = if trial || mixed { Standing::Watch } else { Standing::Good };
        let name = if mixed { format!("{} (not on every server)", name) } else { name };
        Some(match expiry {
            Some(t) if t <= now => (format!("{}, ran out on {}", name, fmt_date(t)), Standing::Bad),
            Some(t) if t - now <= LICENSE_SOON => {
                let days = (t - now) / 86_400;
                let when = match days {
                    0 => "today".to_string(),
                    1 => "tomorrow".to_string(),
                    n => format!("in {} days", n),
                };
                (format!("{}, runs out {}", name, when), Standing::Watch)
            }
            Some(t) => (format!("{}, until {}", name, fmt_date(t)), fine),
            None => (name, fine),
        })
    }

    /// Its servers' versions ("XenServer 8.4.0"), the coordinator's first: more than one while
    /// it's being upgraded.
    pub fn versions(&self) -> Vec<String> {
        let mut hosts: Vec<&HostInfo> = self.hosts.iter().collect();
        hosts.sort_by_key(|h| !h.is_master);
        let mut out: Vec<String> = vec![];
        for v in hosts.iter().map(|h| h.version_text()).filter(|v| !v.is_empty()) {
            if !out.contains(&v) {
                out.push(v);
            }
        }
        out
    }

    pub fn any_enabled_host(&self) -> bool {
        self.hosts.iter().any(|h| h.enabled)
    }
    pub fn is_pool(&self) -> bool {
        !self.name.is_empty() || self.hosts.len() != 1
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shortcut {
    None,
    CtrlB,
    CtrlE,
    CtrlR,
    CtrlY,
}

impl Shortcut {
    pub fn text(self) -> &'static str {
        match self {
            Shortcut::None => "",
            Shortcut::CtrlB => "Ctrl+B",
            Shortcut::CtrlE => "Ctrl+E",
            Shortcut::CtrlR => "Ctrl+R",
            Shortcut::CtrlY => "Ctrl+Y",
        }
    }
}

/// One VM power command.
pub struct VmAction {
    pub op: &'static str,
    pub label: &'static str,
    pub method: &'static str,
    pub verb: &'static str,
    pub shortcut: Shortcut,
    pub confirm: bool,
    pub extra_false_args: bool,
}

/// Shortcut keys, confirmations and operations as read from each XenAdmin.Commands.*VMCommand.
/// Suspend and Resume share Ctrl+Y (both return Keys 131161); the key does whichever can run.
pub const ACTIONS: [VmAction; 7] = [
    VmAction { op: "start", label: "Start", method: "VM.start", verb: "Starting", shortcut: Shortcut::CtrlB, confirm: false, extra_false_args: true },
    VmAction { op: "clean_shutdown", label: "Shut Down", method: "VM.clean_shutdown", verb: "Shutting down", shortcut: Shortcut::CtrlE, confirm: true, extra_false_args: false },
    VmAction { op: "clean_reboot", label: "Reboot", method: "VM.clean_reboot", verb: "Rebooting", shortcut: Shortcut::CtrlR, confirm: true, extra_false_args: false },
    VmAction { op: "suspend", label: "Suspend", method: "VM.suspend", verb: "Suspending", shortcut: Shortcut::CtrlY, confirm: true, extra_false_args: false },
    VmAction { op: "resume", label: "Resume", method: "VM.resume", verb: "Resuming", shortcut: Shortcut::CtrlY, confirm: false, extra_false_args: true },
    VmAction { op: "hard_shutdown", label: "Force Shut Down", method: "VM.hard_shutdown", verb: "Forcing shut down of", shortcut: Shortcut::None, confirm: true, extra_false_args: false },
    VmAction { op: "hard_reboot", label: "Force Reboot", method: "VM.hard_reboot", verb: "Forcing reboot of", shortcut: Shortcut::None, confirm: true, extra_false_args: false },
];

impl VmAction {
    pub fn is_force(&self) -> bool {
        self.op == "hard_shutdown" || self.op == "hard_reboot"
    }

    /// The original's CanRun rule for each command. `busy` means this app is already running a task on the
    /// VM: then only Force Shut Down / Force Reboot of a running VM are allowed (HasRunningTasks branch).
    pub fn can_run(&self, vm: &VmInfo, d: &PoolData, busy: bool) -> bool {
        // its server's gone: the pool can't reach the VM
        if d.host_down_for(vm).is_some() {
            return false;
        }
        if busy {
            return self.is_force() && vm.power == PowerState::Running;
        }
        match self.op {
            // StartVMCommand.CanRun 0x0059e88c
            "start" => vm.can("start") && d.any_enabled_host() && vm.power == PowerState::Halted,
            // ShutDownVMCommand.CanRun 0x0059e4b9
            "clean_shutdown" => vm.can("clean_shutdown"),
            // RebootVMCommand.CanRun 0x0059c118
            "clean_reboot" => {
                let host_ok = match vm.host.as_deref().and_then(|h| d.host(h)) {
                    Some(h) => h.enabled || d.any_enabled_host(),
                    None => d.any_enabled_host(),
                };
                vm.can("clean_reboot") && host_ok
            }
            // SuspendVMCommand.CanRun 0x0059f47c
            "suspend" => vm.power == PowerState::Running && vm.can("suspend"),
            // ResumeVMCommand.CanRun 0x0059d344
            "resume" => vm.can("resume") && vm.power == PowerState::Suspended && d.any_enabled_host(),
            // ForceVMShutDownCommand.CanRun 0x00599804
            "hard_shutdown" => vm.can("hard_shutdown"),
            // ForceVMRebootCommand.CanRun 0x005995c8
            "hard_reboot" => vm.can("hard_reboot") && d.any_enabled_host(),
            _ => vm.can(self.op),
        }
    }

    /// Which power buttons the toolbar shows, greyed out when they can't run: Suspend and Resume
    /// share one place (Resume while the VM is suspended), and the Force ones aren't buttons.
    pub fn on_toolbar(&self, vm: Option<&VmInfo>) -> bool {
        let suspended = vm.map_or(false, |v| v.power == PowerState::Suspended);
        match self.op {
            "suspend" => !suspended,
            "resume" => suspended,
            "hard_shutdown" | "hard_reboot" => false, // in the arrow menus next to Shut Down / Reboot
            _ => true,
        }
    }

    /// Why a command can't run, following each command's GetCantRunReasonCore.
    pub fn cant_run_reason(&self, vm: &VmInfo, d: &PoolData, busy: bool) -> Option<String> {
        if self.can_run(vm, d, busy) {
            return None;
        }
        if let Some(h) = d.host_down_for(vm) {
            return Some(format!("Its server, {}, {}: what the VM is doing can't be known, and nothing can be done to it there.", h.name, if h.live { "isn't answering" } else { "is off" }));
        }
        let s = if busy {
            "This VM is busy with another task."
        } else if self.op == "resume" {
            "This VM is not suspended."
        } else if self.op == "suspend" && vm.power == PowerState::Suspended {
            "This VM is already suspended."
        } else {
            match vm.power {
                PowerState::Halted => "This VM is shut down.",
                PowerState::Paused => "This VM is paused.",
                PowerState::Suspended => "This VM is suspended.",
                PowerState::Running => "This VM is already running.",
                PowerState::Unknown => "The server doesn't allow this right now.",
            }
        };
        Some(s.to_string())
    }
}

/// "12 days, 3 hours", "3 hours, 20 minutes", "5 minutes".
pub fn fmt_duration(secs: i64) -> String {
    let (d, h, m) = (secs / 86_400, (secs / 3600) % 24, (secs / 60) % 60);
    let unit = |n: i64, what: &str| format!("{} {}{}", n, what, if n == 1 { "" } else { "s" });
    if d > 0 {
        format!("{}, {}", unit(d, "day"), unit(h, "hour"))
    } else if h > 0 {
        format!("{}, {}", unit(h, "hour"), unit(m, "minute"))
    } else {
        unit(m.max(1), "minute")
    }
}

/// XenAPI's times, like "20261114T13:22:01Z", in seconds since 1970.
/// The civil date (year, month 1–12, day) of a day counted from 1970 (Howard Hinnant's algorithm,
/// the inverse of the one in `parse_xen_time`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

/// When something happened, for people: "Today, 08:42", "Yesterday, 19:10", "3 Nov, 14:00", or
/// "3 Nov 2025, 14:00" in another year. `t` and `now` are seconds since 1970; `offset` is the
/// local time zone's, in seconds east of UTC.
pub fn fmt_when(t: i64, now: i64, offset: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (lt, ln) = (t + offset, now + offset);
    let (day, today) = (lt.div_euclid(86_400), ln.div_euclid(86_400));
    let clock = format!("{:02}:{:02}", lt.rem_euclid(86_400) / 3600, lt.rem_euclid(3600) / 60);
    let (y, m, d) = civil_from_days(day);
    if day == today {
        format!("Today, {}", clock)
    } else if day == today - 1 {
        format!("Yesterday, {}", clock)
    } else if y == civil_from_days(today).0 {
        format!("{} {}, {}", d, MONTHS[(m - 1) as usize], clock)
    } else {
        fmt_date_time(t, offset)
    }
}

/// A time of day: `t` (seconds since 1970) moved by `offset` (seconds east of UTC), "14:59:24";
/// with " UTC" after it when `utc` (the time zone wasn't known).
pub fn fmt_clock(t: i64, offset: i64, utc: bool) -> String {
    let s = (t + offset).rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}{}", s / 3600, s / 60 % 60, s % 60, if utc { " UTC" } else { "" })
}

/// A date and time in full, local: "16 Nov 2026, 08:42" (as a new snapshot's name).
pub fn fmt_date_time(t: i64, offset: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let lt = t + offset;
    let (y, m, d) = civil_from_days(lt.div_euclid(86_400));
    format!("{} {} {}, {:02}:{:02}", d, MONTHS[(m - 1) as usize], y, lt.rem_euclid(86_400) / 3600, lt.rem_euclid(3600) / 60)
}

/// A day as people write it: "12 Mar 2027" (UTC).
pub fn fmt_date(t: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (y, m, d) = civil_from_days(t.div_euclid(86_400));
    format!("{} {} {}", d, MONTHS[(m - 1) as usize], y)
}

pub fn parse_xen_time(t: &str) -> Option<i64> {
    let t = t.trim().trim_end_matches('Z').replace('-', "");
    let (date, time) = t.split_once('T')?;
    if date.len() != 8 {
        return None;
    }
    let (y, mo, d): (i64, i64, i64) = (date[0..4].parse().ok()?, date[4..6].parse().ok()?, date[6..8].parse().ok()?);
    let mut hms = time.split(':').map(|x| x.split('.').next().unwrap_or("0").parse::<i64>().unwrap_or(0));
    let (h, mi, s) = (hms.next()?, hms.next().unwrap_or(0), hms.next().unwrap_or(0));
    // days from the civil date (Howard Hinnant's algorithm)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + s;
    (secs > 0).then_some(secs)
}

pub fn fmt_bytes(b: i64) -> String {
    if b <= 0 {
        return "0 B".into();
    }
    let units = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v >= 100.0 || i == 0 {
        format!("{:.0} {}", v, units[i])
    } else {
        let s = format!("{:.1}", v);
        let s = s.trim_end_matches(".0").to_string();
        format!("{} {}", s, units[i])
    }
}

pub fn or<'a>(s: &'a str, fallback: &'a str) -> &'a str {
    if s.is_empty() {
        fallback
    } else {
        s
    }
}

// ---------------------------------------------------------------- ISO libraries

/// A new ISO library: a Windows (SMB) or NFS share the servers mount to find installation discs.
#[derive(Clone, Debug, Default)]
pub struct IsoLibrary {
    pub name: String,
    pub description: String,
    pub nfs: bool,
    /// "//server/share/folder" for SMB, "server:/path" for NFS.
    pub location: String,
    pub username: String,
    pub password: String,
    /// "3.0" or "1.0".
    pub smb_version: String,
    /// "3", "4" or "4.1".
    pub nfs_version: String,
    /// Mounted on every server of the pool, not just the coordinator.
    pub shared: bool,
}

/// The name an uploaded disc gets in an ISO library. The server's ISO driver only accepts letters,
/// digits, dots and hyphens, ending in .iso or .img, so anything else becomes a hyphen:
/// "Win11_24H2 English.iso" → "Win11-24H2-English.iso".
pub fn iso_file_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        let c = if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '-' };
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    let lower = out.to_ascii_lowercase();
    let (stem, ext) = if lower.ends_with(".iso") || lower.ends_with(".img") { out.split_at(out.len() - 4) } else { (out.as_str(), ".iso") };
    let stem = stem.trim_matches(|c| c == '-' || c == '.');
    format!("{}{}", if stem.is_empty() { "disc" } else { stem }, ext)
}

/// A name's words for matching: lower case, letters and digits apart ("Win11_24H2" → win 11 24 h 2).
fn words(name: &str) -> Vec<String> {
    let mut out = vec![];
    for run in name.to_lowercase().split(|c: char| !c.is_ascii_alphanumeric()).filter(|r| !r.is_empty()) {
        let mut cur = String::new();
        for c in run.chars() {
            if !cur.is_empty() && cur.chars().last().map_or(false, |l| l.is_ascii_digit() != c.is_ascii_digit()) {
                out.push(std::mem::take(&mut cur));
            }
            cur.push(c);
        }
        out.push(cur);
    }
    out
}

/// The built-in template that fits an ISO, from its file name: the same family (Windows,
/// Ubuntu, Debian…) and every version number in the template's name. Microsoft's ISOs often carry
/// only a build number ("26100.1742…CLIENT…"), which says which Windows it is. None when nothing
/// fits (then: Other install media).
pub fn detect_template<'a>(iso: &str, templates: &'a [TemplateInfo]) -> Option<&'a TemplateInfo> {
    let mut w = words(iso);
    let server = w.iter().any(|x| x == "server");
    let build: Option<&str> = w.iter().find(|x| x.len() == 5 && x.chars().all(|c| c.is_ascii_digit())).map(|x| x.as_str());
    let from_build: &[&str] = match (build, server) {
        (Some("26100"), true) => &["windows", "server", "2025"],
        (Some("20348"), true) => &["windows", "server", "2022"],
        (Some("17763"), true) => &["windows", "server", "2019"],
        (Some("14393"), true) => &["windows", "server", "2016"],
        (Some("26100" | "22631" | "22621" | "22000"), false) => &["windows", "11"],
        (Some("19041" | "19042" | "19043" | "19044" | "19045"), false) => &["windows", "10"],
        _ => &[],
    };
    w.extend(from_build.iter().map(|s| s.to_string()));
    let alias = |x: &str| match x {
        "win" => "windows",
        "alma" => "almalinux",
        "rhel" => "red",
        "sles" | "sle" => "suse",
        other => other,
    }
    .to_string();
    let families: Vec<String> = w.iter().map(|x| alias(x)).collect();
    let numbers: Vec<&String> = w.iter().filter(|x| x.chars().all(|c| c.is_ascii_digit())).collect();
    templates
        .iter()
        .filter(|t| !t.name.contains("32-bit"))
        .filter_map(|t| {
            let bare = t.name.split('(').next().unwrap_or("");
            let tw = words(bare);
            let family = tw.first()?;
            let versions: Vec<&String> = tw.iter().filter(|x| x.chars().all(|c| c.is_ascii_digit())).collect();
            let fits = families.contains(family)
                && !versions.is_empty()
                && versions.iter().all(|v| numbers.contains(v))
                && (family != "windows" || tw.iter().any(|x| x == "server") == server);
            fits.then_some((versions.len(), t))
        })
        .max_by_key(|(n, _)| *n)
        .map(|(_, t)| t)
}

/// A new VM's suggested name: its system's ("Ubuntu 24.04", "Windows 11"), or else the ISO's.
pub fn vm_name_for(template: Option<&TemplateInfo>, iso: &str) -> String {
    match template.filter(|t| t.name != OTHER_MEDIA) {
        Some(t) => {
            let bare = t.name.split('(').next().unwrap_or("").trim();
            let parts: Vec<&str> = bare.split_whitespace().collect();
            match parts.first().map(|f| f.to_lowercase()) {
                // "Ubuntu Noble Numbat 24.04" → "Ubuntu 24.04"; "Debian Bookworm 12" → "Debian 12"
                Some(f) if (f == "ubuntu" || f == "debian") && parts.len() > 2 => format!("{} {}", parts[0], parts[parts.len() - 1]),
                _ => bare.to_string(),
            }
        }
        None => {
            let stem = iso.rsplit_once('.').map_or(iso, |(s, ext)| if ext.eq_ignore_ascii_case("iso") || ext.eq_ignore_ascii_case("img") { s } else { iso });
            readable_name(stem).unwrap_or_else(|| stem.chars().take(40).collect())
        }
    }
}

/// A file name's product and version, as a VM name: "fedora-coreos-44.20260928.3.1-live-iso.x86_64"
/// → "Fedora CoreOS 44", "ubuntu-26.04.1-live-server-amd64" → "Ubuntu 26.04". The words before
/// the first version, in their usual spelling, then that version's first one or two short
/// parts; the rest (builds, architectures, "live", "server") left out. None without a word.
fn readable_name(stem: &str) -> Option<String> {
    const SPELLED: [(&str, &str); 17] = [
        ("coreos", "CoreOS"), ("truenas", "TrueNAS"), ("freebsd", "FreeBSD"), ("opnsense", "OPNsense"), ("pfsense", "pfSense"),
        ("nixos", "NixOS"), ("almalinux", "AlmaLinux"), ("centos", "CentOS"), ("opensuse", "openSUSE"), ("rhel", "RHEL"),
        ("sles", "SLES"), ("vyos", "VyOS"), ("ipfire", "IPFire"), ("xcp", "XCP"), ("ng", "ng"), ("os", "OS"),
        ("archlinux", "Arch Linux"),
    ];
    let mut words: Vec<String> = vec![];
    let mut version = String::new();
    for tok in stem.split(|c: char| c == '-' || c == '_' || c == ' ').filter(|t| !t.is_empty()) {
        if tok.starts_with(|c: char| c.is_ascii_digit()) {
            // "44.20260928.3.1" → "44"; "26.04.1" → "26.04"; "2025" → "2025"
            if tok.chars().all(|c| c.is_ascii_digit() || c == '.') {
                let parts: Vec<&str> = tok.split('.').filter(|p| !p.is_empty()).collect();
                let mut keep = vec![parts[0]];
                if let Some(p) = parts.get(1).filter(|p| p.len() <= 2 && parts[0].len() <= 2) {
                    keep.push(p);
                }
                version = keep.join(".");
            }
            break;
        }
        if !tok.chars().any(|c| c.is_alphabetic()) {
            break;
        }
        let lower = tok.to_lowercase();
        let word = match SPELLED.iter().find(|(k, _)| *k == lower) {
            Some((_, v)) => v.to_string(),
            // as written when it has capitals ("eLux"), else capitalised ("fedora" → "Fedora")
            None if tok.chars().any(|c| c.is_uppercase()) => tok.to_string(),
            None => {
                let mut c = tok.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
            }
        };
        words.push(word);
    }
    if words.is_empty() {
        return None;
    }
    let name = if version.is_empty() { words.join(" ") } else { format!("{} {}", words.join(" "), version) };
    Some(name.chars().take(40).collect())
}

/// `base`, or "base (2)", "base (3)"… whichever no VM has yet.
pub fn unique_name(base: &str, taken: &[String]) -> String {
    let used = |n: &str| taken.iter().any(|t| t.eq_ignore_ascii_case(n));
    if !used(base) {
        return base.to_string();
    }
    (2..).map(|i| format!("{} ({})", base, i)).find(|n| !used(n)).unwrap_or_default()
}

impl IsoLibrary {
    /// "\\nas\isos" or "nas/isos" as "//nas/isos"; NFS locations as typed.
    pub fn normalise_location(nfs: bool, typed: &str) -> String {
        let t = typed.trim();
        if nfs {
            return t.to_string();
        }
        let t = t.replace('\\', "/");
        format!("//{}", t.trim_start_matches('/'))
    }

    /// Why the location can't be right, if it can't.
    pub fn location_problem(&self) -> Option<&'static str> {
        if self.nfs {
            match self.location.split_once(':') {
                Some((host, path)) if !host.is_empty() && path.starts_with('/') => None,
                _ => Some("Type the NFS share as server:/path, for example nas:/mnt/tank/isos."),
            }
        } else {
            let parts: Vec<&str> = self.location.trim_start_matches('/').split('/').filter(|p| !p.is_empty()).collect();
            if parts.len() >= 2 {
                None
            } else {
                Some("Type the share as \\\\server\\share, for example \\\\nas\\isos.")
            }
        }
    }

    /// The SR's device configuration, as the server's ISO driver expects it.
    pub fn device_config(&self) -> Vec<(String, String)> {
        let mut c = vec![("location".to_string(), self.location.clone())];
        if self.nfs {
            c.push(("type".into(), "nfs_iso".into()));
            c.push(("nfsversion".into(), self.nfs_version.clone()));
        } else {
            c.push(("type".into(), "cifs".into()));
            c.push(("vers".into(), self.smb_version.clone()));
            if !self.username.is_empty() {
                c.push(("username".into(), self.username.clone()));
                c.push(("cifspassword".into(), self.password.clone()));
            }
        }
        c
    }

    /// The same request as an `xe` command for the server's console, password hidden. With
    /// `reattach` (the library's UUID), the connection given back to a detached library instead.
    pub fn xe_command(&self, reattach: Option<&str>) -> String {
        let quote = |v: &str| if v.is_empty() || v.contains(|c: char| c.is_whitespace() || c == '"') { format!("\"{}\"", v.replace('"', "\\\"")) } else { v.to_string() };
        let mut parts = match reattach {
            Some(uuid) => vec!["xe pbd-create".to_string(), format!("sr-uuid={}", uuid), "host-uuid=<coordinator>".into()],
            None => vec![
                "xe sr-create".to_string(),
                "type=iso".into(),
                "content-type=iso".into(),
                format!("shared={}", self.shared),
                format!("name-label={}", quote(&self.name)),
            ],
        };
        for (k, v) in self.device_config() {
            let v = if k.ends_with("password") { "********".to_string() } else { quote(&v) };
            parts.push(format!("device-config:{}={}", k, v));
        }
        parts.join(" ")
    }
}

#[cfg(test)]
mod iso_tests {
    use super::*;

    fn templates() -> Vec<TemplateInfo> {
        [
            "Other install media", "Windows 10 (64-bit)", "Windows 11", "Windows Server 2019 (64-bit)", "Windows Server 2022 (64-bit)",
            "Windows Server 2025 (64-bit)", "Ubuntu Jammy Jellyfish 22.04", "Ubuntu Noble Numbat 24.04", "Debian Bullseye 11",
            "Debian Bookworm 12", "Rocky Linux 8", "Rocky Linux 9", "AlmaLinux 9", "CentOS Stream 9", "Red Hat Enterprise Linux 9",
        ]
        .iter()
        .map(|n| TemplateInfo { name: n.to_string(), ..Default::default() })
        .collect()
    }

    #[test]
    fn templates_from_iso_names() {
        let t = templates();
        let pick = |iso: &str| detect_template(iso, &t).map(|t| t.name.clone()).unwrap_or_default();
        assert_eq!(pick("ubuntu-24.04.3-live-server-amd64.iso"), "Ubuntu Noble Numbat 24.04");
        assert_eq!(pick("ubuntu-22.04.5-desktop-amd64.iso"), "Ubuntu Jammy Jellyfish 22.04");
        assert_eq!(pick("ubuntu-26.04.1-live-server-amd64.iso"), ""); // no template yet: Other install media
        assert_eq!(pick("debian-12.7.0-amd64-netinst.iso"), "Debian Bookworm 12");
        assert_eq!(pick("rocky-9.4-x86_64-minimal.iso"), "Rocky Linux 9");
        assert_eq!(pick("Rocky-8.10-x86_64-dvd1.iso"), "Rocky Linux 8");
        assert_eq!(pick("AlmaLinux-9.4-x86_64-boot.iso"), "AlmaLinux 9");
        assert_eq!(pick("CentOS-Stream-9-latest-x86_64-dvd1.iso"), "CentOS Stream 9");
        assert_eq!(pick("rhel-9.4-x86_64-dvd.iso"), "Red Hat Enterprise Linux 9");
        assert_eq!(pick("Win11_24H2_English_x64.iso"), "Windows 11");
        assert_eq!(pick("Win10_22H2_English_x64v1.iso"), "Windows 10 (64-bit)");
        assert_eq!(pick("windows-server-2025.iso"), "Windows Server 2025 (64-bit)");
        assert_eq!(pick("en-us_windows_server_2022_updated_x64_dvd.iso"), "Windows Server 2022 (64-bit)");
        // Microsoft's build-number names
        assert_eq!(pick("26100.1742.240906-0331.ge_release_svc_refresh_CLIENT_CONSUMER_x64FRE_en-us.iso"), "Windows 11");
        assert_eq!(pick("26100.1742.240906-0331.ge_release_svc_refresh_SERVER_EVAL_x64FRE_en-us.iso"), "Windows Server 2025 (64-bit)");
        assert_eq!(pick("17763.737.190906-2324.rs5_release_svc_refresh_SERVER_EVAL_x64FRE_en-us_1.iso"), "Windows Server 2019 (64-bit)");
        assert_eq!(pick("eLux-7.2609.0-5.iso"), "");
        assert_eq!(pick("debian-13.1.0-amd64-netinst.iso"), "");
    }

    #[test]
    fn suggested_vm_names() {
        let t = templates();
        let named = |n: &str| t.iter().find(|x| x.name == n);
        assert_eq!(vm_name_for(named("Ubuntu Noble Numbat 24.04"), "x.iso"), "Ubuntu 24.04");
        assert_eq!(vm_name_for(named("Debian Bookworm 12"), "x.iso"), "Debian 12");
        assert_eq!(vm_name_for(named("Windows Server 2025 (64-bit)"), "x.iso"), "Windows Server 2025");
        assert_eq!(vm_name_for(named("Rocky Linux 9"), "x.iso"), "Rocky Linux 9");
        assert_eq!(vm_name_for(named("Other install media"), "eLux-7.2609.0-5.iso"), "eLux 7");
        assert_eq!(vm_name_for(None, "ubuntu-26.04.1-live-server-amd64.iso"), "Ubuntu 26.04");
        assert_eq!(vm_name_for(None, "fedora-coreos-44.20260928.3.1-live-iso.x86_64.iso"), "Fedora CoreOS 44");
        assert_eq!(vm_name_for(None, "TrueNAS-SCALE-25.04.2.iso"), "TrueNAS SCALE 25.04");
        assert_eq!(vm_name_for(None, "alpine-virt-3.22.1-x86_64.iso"), "Alpine Virt 3.22");
        assert_eq!(vm_name_for(None, "OPNsense-25.7-dvd-amd64.iso"), "OPNsense 25.7");
        assert_eq!(vm_name_for(None, "archlinux-2026.11.01-x86_64.iso"), "Arch Linux 2026");
        // no words to go by: the file's name
        assert_eq!(vm_name_for(None, "26100.1742.240906-0331.iso"), "26100.1742.240906-0331");
        let taken = vec!["Ubuntu 24.04".to_string(), "Ubuntu 24.04 (2)".to_string()];
        assert_eq!(unique_name("Ubuntu 24.04", &taken), "Ubuntu 24.04 (3)");
        assert_eq!(unique_name("Debian 12", &taken), "Debian 12");
    }

    #[test]
    fn uploaded_file_names() {
        assert_eq!(iso_file_name("ubuntu-24.04.1-live-server-amd64.iso"), "ubuntu-24.04.1-live-server-amd64.iso");
        assert_eq!(iso_file_name("Win11_24H2_English_x64.iso"), "Win11-24H2-English-x64.iso");
        assert_eq!(iso_file_name("  Instalação do Windows (2025).ISO "), "Instala-o-do-Windows-2025.ISO");
        assert_eq!(iso_file_name("_.iso"), "disc.iso");
        assert_eq!(iso_file_name("firmware.img"), "firmware.img");
        assert_eq!(iso_file_name("notes"), "notes.iso");
    }

    fn smb(loc: &str) -> IsoLibrary {
        IsoLibrary { name: "ISO Library".into(), location: IsoLibrary::normalise_location(false, loc), username: "joao".into(), password: "s3cret".into(), smb_version: "3.0".into(), nfs_version: "3".into(), shared: true, ..Default::default() }
    }

    #[test]
    fn smb_locations() {
        assert_eq!(smb("\\\\nas\\isos").location, "//nas/isos");
        assert_eq!(smb("nas/isos/win").location, "//nas/isos/win");
        assert_eq!(smb("//nas/isos").location_problem(), None);
        assert!(smb("\\\\nas").location_problem().is_some());
    }

    #[test]
    fn nfs_locations() {
        let mut l = IsoLibrary { nfs: true, nfs_version: "3".into(), ..Default::default() };
        l.location = "nas:/mnt/tank/isos".into();
        assert_eq!(l.location_problem(), None);
        l.location = "nas/mnt".into();
        assert!(l.location_problem().is_some());
    }

    #[test]
    fn xe_command_hides_the_password() {
        let cmd = smb("\\\\nas\\isos").xe_command(None);
        assert_eq!(cmd, "xe sr-create type=iso content-type=iso shared=true name-label=\"ISO Library\" device-config:location=//nas/isos device-config:type=cifs device-config:vers=3.0 device-config:username=joao device-config:cifspassword=********");
        assert!(!cmd.contains("s3cret"));
        let again = smb("//nas/isos").xe_command(Some("1234"));
        assert!(again.starts_with("xe pbd-create sr-uuid=1234 host-uuid=<coordinator> device-config:location=//nas/isos"), "{}", again);
        assert!(!again.contains("s3cret"));
    }

    #[test]
    fn vms_on_a_server_thats_gone() {
        let host = |r: &str, live: bool, unreachable: bool| HostInfo { rref: r.into(), name: r.into(), live, unreachable, enabled: true, ..Default::default() };
        let d = PoolData { hosts: vec![host("a", true, false), host("b", true, true), host("c", false, false)], ..Default::default() };
        let vm = |h: &str, power: PowerState| VmInfo { host: Some(h.into()), power, allowed: vec!["clean_shutdown".into(), "suspend".into()], ..Default::default() };
        let shut_down = &ACTIONS[1];
        assert!(d.host_down_for(&vm("a", PowerState::Running)).is_none());
        assert!(shut_down.can_run(&vm("a", PowerState::Running), &d, false));
        // on a server that doesn't answer, or that's off: unknown, and nothing can be done
        for h in ["b", "c"] {
            let v = vm(h, PowerState::Running);
            assert_eq!(d.host_down_for(&v).map(|x| x.rref.as_str()), Some(h));
            assert!(!shut_down.can_run(&v, &d, false));
            assert!(shut_down.cant_run_reason(&v, &d, false).unwrap().contains(h));
        }
        // a suspended VM isn't on any server
        assert!(d.host_down_for(&vm("b", PowerState::Suspended)).is_none());
    }

    #[test]
    fn rrd_updates_read() {
        let xml = r#"<xport><meta><start>100</start><step>5</step><end>110</end><rows>2</rows><columns>8</columns><legend>
            <entry>AVERAGE:vm:u1:cpu0</entry><entry>AVERAGE:vm:u1:cpu1</entry><entry>AVERAGE:vm:u1:memory</entry>
            <entry>AVERAGE:vm:u1:memory_internal_free</entry><entry>AVERAGE:vm:u1:vbd_xvda_read</entry><entry>AVERAGE:vm:u1:vbd_xvda_read_latency</entry>
            <entry>AVERAGE:vm:u1:vif_0_rx</entry><entry>AVERAGE:vm:u2:cpu0</entry></legend></meta><data>
            <row><t>110</t><v>0.5</v><v>0.3</v><v>4294967296</v><v>1048576</v><v>2048</v><v>9</v><v>NaN</v><v>1.0</v></row>
            <row><t>105</t><v>0.1</v><v>0.1</v><v>4294967296</v><v>2097152</v><v>0</v><v>9</v><v>100</v><v>1.0</v></row>
            </data></xport>"#;
        let p = parse_rrd_updates(xml, "u1");
        assert_eq!(p.times, vec![105, 110]); // oldest first
        assert!((p.cpu[1] - 0.4).abs() < 1e-9 && (p.cpu[0] - 0.1).abs() < 1e-9); // averaged over its two CPUs, not the other VM's
        assert_eq!(p.mem_total[1], 4294967296.0);
        assert_eq!(p.mem_used[1], 4294967296.0 - 1048576.0 * 1024.0);
        assert!(p.mem_inside);
        assert_eq!((p.disk_read[1], p.net_rx[1], p.net_rx[0]), (2048.0, 0.0, 100.0)); // latency left out, NaN as nothing
    }

    #[test]
    fn rrd_updates_json_and_end() {
        let json = r#"{"meta":{"start":100,"step":5,"end":110,"rows":2,"columns":2,"legend":["AVERAGE:vm:u1:cpu0","AVERAGE:vm:u1:memory"]},
            "data":[{"t":110,"values":[0.5,2048]},{"t":105,"values":[0.25,"NaN"]}]}"#;
        let p = parse_rrd_updates(json, "u1");
        assert_eq!(p.times, vec![105, 110]);
        assert_eq!((p.cpu[0], p.cpu[1], p.mem_total[1]), (0.25, 0.5, 2048.0));
        // no memory_internal_free (Windows with XenServer VM Tools 9, or no tools): what it uses isn't known
        assert!(!p.mem_inside);
        assert_eq!(rrd_end(json), Some(110));
        assert_eq!(rrd_end("<xport><meta><start>1</start><end>1700000600</end></meta></xport>"), Some(1700000600));
        // a XenServer's: its numbers in quotes, NaN bare
        let quoted = r#"{"meta":{"start":"100","step":"5","end":"110","rows":"2","columns":"3","legend":["AVERAGE:vm:u1:cpu0","AVERAGE:vm:u1:cpu1","AVERAGE:vm:u2:cpu0"]},
            "data":[{"t":"110","values":["0.5","0.3",NaN]},{"t":"105","values":[0.1,NaN,"0.9"]}]}"#;
        let p = parse_rrd_updates(quoted, "u1");
        assert_eq!(p.times, vec![105, 110]);
        assert!((p.cpu[0] - 0.05).abs() < 1e-9 && (p.cpu[1] - 0.4).abs() < 1e-9);
        assert_eq!(rrd_end(quoted), Some(110));
    }

    #[test]
    fn pool_license() {
        let now = 1_800_000_000;
        let host = |master: bool, edition: &str, name: &str, expiry: Option<i64>| HostInfo { is_master: master, edition: edition.into(), license_name: name.into(), license_expiry: expiry, ..Default::default() };
        let pool = |hosts: Vec<HostInfo>| PoolData { hosts, ..Default::default() };
        let far = now + 400 * 86_400;
        assert_eq!(pool(vec![host(true, "xenserver-premium", "", Some(far))]).license(now), Some((format!("Premium Edition, until {}", fmt_date(far)), Standing::Good)));
        assert_eq!(pool(vec![host(true, "xenserver-premium", "XenServer Premium Edition", Some(now + 3 * 86_400))]).license(now).unwrap(), ("XenServer Premium Edition, runs out in 3 days".to_string(), Standing::Watch));
        assert_eq!(pool(vec![host(true, "xenserver-premium", "", Some(now - 10))]).license(now).unwrap().1, Standing::Bad);
        // never runs out (a date far away); a trial; not the same everywhere
        assert_eq!(pool(vec![host(true, "xenserver-standard", "", Some(now + 30 * 365 * 86_400))]).license(now), Some(("Standard Edition".to_string(), Standing::Good)));
        assert_eq!(pool(vec![host(true, "trial", "", None)]).license(now).unwrap().1, Standing::Watch);
        assert_eq!(pool(vec![host(false, "xenserver-standard", "", None), host(true, "xenserver-premium", "", None)]).license(now), Some(("Premium Edition (not on every server)".to_string(), Standing::Watch)));
        // XCP-ng: none
        assert_eq!(pool(vec![host(true, "xcp-ng", "", None)]).license(now), None);
    }

    #[test]
    fn host_rrd_read() {
        let json = r#"{"meta":{"start":100,"step":5,"end":110,"rows":2,"columns":10,"legend":[
            "AVERAGE:host:h1:cpu0","AVERAGE:host:h1:cpu1","AVERAGE:host:h1:cpu_avg","AVERAGE:host:h1:memory_total_kib","AVERAGE:host:h1:memory_free_kib",
            "AVERAGE:host:h1:pif_eth0_rx","AVERAGE:host:h1:pif_eth1_rx","AVERAGE:host:h1:pif_lo_rx",
            "AVERAGE:vm:u1:cpu0","AVERAGE:vm:u1:vbd_xvda_read"]},
            "data":[{"t":110,"values":[0.5,0.3,0.4,1024,256,100,50,999,0.7,4096]},{"t":105,"values":[0.1,0.1,0.1,1024,512,0,0,0,0.2,0]}]}"#;
        let p = parse_host_rrd(json, "h1");
        assert_eq!(p.host.times, vec![105, 110]);
        assert!((p.host.cpu[1] - 0.4).abs() < 1e-9); // its CPUs averaged (cpu_avg not counted twice)
        assert_eq!((p.host.mem_total[1], p.host.mem_used[1], p.host.mem_used[0]), (1024.0 * 1024.0, 768.0 * 1024.0, 512.0 * 1024.0));
        // its cards (not the loopback), each and together; its VMs' disks added up
        assert_eq!(p.nics.iter().map(|n| n.0.as_str()).collect::<Vec<_>>(), vec!["eth0", "eth1"]);
        assert_eq!((p.nics[0].1[1], p.host.net_rx[1]), (100.0, 150.0));
        assert_eq!(p.host.disk_read[1], 4096.0);
        assert!((p.vm("u1").unwrap().cpu[1] - 0.7).abs() < 1e-9);
        assert!(p.vm("u2").is_none());
    }

    #[test]
    fn clock_times() {
        let t = parse_xen_time("20261116T11:42:05Z").unwrap();
        assert_eq!(fmt_clock(t, -3 * 3600, false), "08:42:05"); // Brazil
        assert_eq!(fmt_clock(t, 0, true), "11:42:05 UTC");
        assert_eq!(fmt_clock(parse_xen_time("20261116T01:00:00Z").unwrap(), -3 * 3600, false), "22:00:00"); // the day before, there
    }

    #[test]
    fn when_for_people() {
        let now = parse_xen_time("20261116T11:42:00Z").unwrap(); // 08:42 in Brazil (UTC-3)
        let br = -3 * 3600;
        assert_eq!(fmt_when(now - 60, now, br), "Today, 08:41");
        assert_eq!(fmt_when(now - 9 * 3600, now, br), "Yesterday, 23:42");
        assert_eq!(fmt_when(parse_xen_time("20261103T17:00:00Z").unwrap(), now, br), "3 Nov, 14:00");
        assert_eq!(fmt_when(parse_xen_time("20250101T02:30:00Z").unwrap(), now, br), "31 Dec 2024, 23:30");
        assert_eq!(fmt_when(parse_xen_time("20240229T12:00:00Z").unwrap(), now, 0), "29 Feb 2024, 12:00");
    }

    #[test]
    fn storage_trouble() {
        let host = |r: &str, master: bool, live: bool| HostInfo { rref: r.into(), is_master: master, live, ..Default::default() };
        let d = PoolData { hosts: vec![host("b", false, true), host("a", true, true), host("c", false, false)], ..Default::default() };
        let sr = |shared: bool, links: &[(&str, bool)]| SrInfo { kind: "iso".into(), shared, links: links.iter().map(|(h, on)| (h.to_string(), *on)).collect(), ..Default::default() };
        // all mounted (the server that's off doesn't count)
        assert_eq!(d.sr_trouble(&sr(true, &[("a", true), ("b", true), ("c", false)])), None);
        // unplugged on every server: the coordinator first
        let t = d.sr_trouble(&sr(true, &[("b", false), ("a", false), ("c", false)])).unwrap();
        assert_eq!((t.missing, t.nowhere, t.detached), (vec!["a".to_string(), "b".to_string()], true, false));
        // shared, and a server with no connection to it
        let t = d.sr_trouble(&sr(true, &[("a", true)])).unwrap();
        assert_eq!((t.missing, t.nowhere), (vec!["b".to_string()], false));
        // a server's own storage only counts there
        assert_eq!(d.sr_trouble(&sr(false, &[("a", true)])), None);
        // detached
        assert!(d.sr_trouble(&sr(true, &[])).unwrap().detached);
        // a server's own storage never: DVD drives, USB, local disks
        assert_eq!(d.sr_trouble(&SrInfo { kind: "udev".into(), ..Default::default() }), None);
        assert_eq!(d.sr_trouble(&SrInfo { kind: "lvm".into(), links: vec![("a".into(), false)], ..Default::default() }), None);
    }

    #[test]
    fn guest_access_sends_no_credentials() {
        let mut l = smb("//nas/isos");
        l.username.clear();
        assert!(!l.device_config().iter().any(|(k, _)| k == "username" || k == "cifspassword"));
    }
}
