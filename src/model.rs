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
    pub sr_name: String,
    pub kind: String,
    pub size: i64,
}

#[derive(Clone, Debug, Default)]
pub struct VmNic {
    pub device: String,
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
    pub snapshots: usize,
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

impl VmInfo {
    /// The server the VM belongs with: where it runs, or for a VM that's off, the server with its
    /// disks, or else its home server (as the original's VM.Home). None: any server in the pool.
    pub fn home(&self) -> Option<&str> {
        self.host.as_deref().or(self.storage_host.as_deref()).or(self.affinity.as_deref())
    }

    pub fn can(&self, op: &str) -> bool {
        self.allowed.iter().any(|a| a == op)
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
}

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

    /// The same request as an `xe` command for the server's console, password hidden.
    pub fn xe_command(&self) -> String {
        let quote = |v: &str| if v.is_empty() || v.contains(|c: char| c.is_whitespace() || c == '"') { format!("\"{}\"", v.replace('"', "\\\"")) } else { v.to_string() };
        let mut parts = vec![
            "xe sr-create".to_string(),
            "type=iso".into(),
            "content-type=iso".into(),
            format!("shared={}", self.shared),
            format!("name-label={}", quote(&self.name)),
        ];
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
        let cmd = smb("\\\\nas\\isos").xe_command();
        assert_eq!(cmd, "xe sr-create type=iso content-type=iso shared=true name-label=\"ISO Library\" device-config:location=//nas/isos device-config:type=cifs device-config:vers=3.0 device-config:username=joao device-config:cifspassword=********");
        assert!(!cmd.contains("s3cret"));
    }

    #[test]
    fn guest_access_sends_no_credentials() {
        let mut l = smb("//nas/isos");
        l.username.clear();
        assert!(!l.device_config().iter().any(|(k, _)| k == "username" || k == "cifspassword"));
    }
}
