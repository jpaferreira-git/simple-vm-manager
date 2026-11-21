//! VM Conversion: VMs from VMware ESXi or vCenter, Proxmox, and other hypervisors' disk files
//! (.ova, .ovf, .vmx, .vmdk, .vhd, .vhdx, .qcow2, .vdi), made into VMs of a pool.
//!
//! A source (`Source`) lists its VMs as `SourceVm`s: their CPUs, memory, boot mode, disks and
//! network cards, whether they run, and why one can't be brought over. `run` makes each one on
//! the pool (the networks it needs first), copies its disks into it (through this computer, or
//! through the pool's Helper VM when it has one and the source lets it read the disk itself),
//! and starts it. `prep` holds what's run in a VM before it leaves, so it starts on XenServer.

pub mod demo;
pub mod disk;
pub mod files;
pub mod prep;
pub mod proxmox;
pub mod run;
pub mod vmware;

use crate::model::TemplateInfo;
use std::io::Read;
use std::path::PathBuf;

/// Where VMs come from.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Vmware,
    Proxmox,
    /// Disk files on this computer (.ova, .vmx, .vmdk…).
    Files,
    /// The demo's pretend sources.
    Demo,
}

impl Kind {
    pub fn word(self) -> &'static str {
        match self {
            Kind::Vmware => "vmware",
            Kind::Proxmox => "proxmox",
            Kind::Files => "files",
            Kind::Demo => "demo",
        }
    }
    pub fn parse(w: &str) -> Option<Kind> {
        match w {
            "vmware" => Some(Kind::Vmware),
            "proxmox" => Some(Kind::Proxmox),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Kind::Vmware => "VMware",
            Kind::Proxmox => "Proxmox",
            Kind::Files => "Disk files",
            Kind::Demo => "Demo",
        }
    }
}

/// A source the app connects to, as it's remembered (its password is in the keychain).
#[derive(Clone, Debug, PartialEq)]
pub struct SourceConf {
    pub kind: Kind,
    pub address: String,
    pub user: String,
}

impl SourceConf {
    /// What it's known by: in the remembered list, the prepared marks, the keychain.
    pub fn key(&self) -> String {
        format!("{}:{}", self.kind.word(), self.address.trim().to_lowercase())
    }
    /// The keychain's "host" for its password (never the same as a XenServer's).
    pub fn keychain_host(&self) -> String {
        format!("convert-{}", self.key())
    }
}

/// Remembered sources, one a line: kind, address, user.
pub fn parse_sources(text: &str) -> Vec<SourceConf> {
    text.lines()
        .filter_map(|l| {
            let mut p = l.split('\t');
            let kind = Kind::parse(p.next()?)?;
            let address = p.next()?.trim().to_string();
            let user = p.next().unwrap_or("").trim().to_string();
            (!address.is_empty()).then_some(SourceConf { kind, address, user })
        })
        .collect()
}

pub fn sources_text(list: &[SourceConf]) -> String {
    list.iter().filter(|c| matches!(c.kind, Kind::Vmware | Kind::Proxmox)).map(|c| format!("{}\t{}\t{}\n", c.kind.word(), c.address, c.user)).collect()
}

/// VMs marked as prepared (their system made ready for XenServer), one a line: the source's key,
/// the VM's id, when (seconds since 1970).
pub fn parse_prepared(text: &str) -> Vec<(String, String, i64)> {
    text.lines()
        .filter_map(|l| {
            let mut p = l.split('\t');
            Some((p.next()?.to_string(), p.next()?.to_string(), p.next()?.parse().ok()?))
        })
        .collect()
}

pub fn prepared_text(list: &[(String, String, i64)]) -> String {
    list.iter().map(|(s, v, t)| format!("{}\t{}\t{}\n", s, v, t)).collect()
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Power {
    On,
    Off,
    /// Suspended or paused: its memory is saved, its disks aren't finished.
    Paused,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Family {
    Windows,
    Linux,
    Other,
}

/// What a guest system's name says it is.
pub fn family_of(os: &str) -> Family {
    let l = os.to_lowercase();
    if l.contains("windows") || l.starts_with("win") {
        Family::Windows
    } else if ["linux", "ubuntu", "debian", "centos", "rhel", "red hat", "rocky", "alma", "suse", "sles", "fedora", "oracle", "arch", "l26", "gentoo", "mint", "photon"].iter().any(|w| l.contains(w)) {
        Family::Linux
    } else {
        Family::Other
    }
}

/// Where a disk's bytes are.
#[derive(Clone, Debug, PartialEq)]
pub enum DiskAt {
    /// A disk file on this computer (a VMDK descriptor opens its pieces next to it).
    File(PathBuf),
    /// A file inside an .ova (a tar): from `offset`, `len` bytes.
    InTar { tar: PathBuf, offset: u64, len: u64 },
    /// An ESXi datastore's flat file, over HTTPS (raw bytes, the disk's whole size).
    Http { url: String },
    /// A Proxmox disk: a raw device or file, or an image (qcow2, vmdk), read over SSH.
    Ssh { path: String, format: String },
    /// A pretend disk (the demo).
    Demo,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SourceDisk {
    pub label: String,
    /// Its size, as the VM sees it.
    pub size: u64,
    /// How much of it is written, when known (0: not known).
    pub used: u64,
    pub at: DiskAt,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceNic {
    /// The source's network it's on ("VM Network", "vmbr0").
    pub network: String,
    pub vlan: Option<i64>,
    /// Its hardware address, "aa:bb:cc:dd:ee:ff" (empty: not known).
    pub mac: String,
}

/// A VM at a source.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceVm {
    /// What the source knows it by (VMware's moref, Proxmox's "node/vmid", a file's path).
    pub id: String,
    pub name: String,
    /// Its system, in words ("Microsoft Windows Server 2022 (64-bit)").
    pub os: String,
    pub family: Family,
    pub cpus: i64,
    pub cores: i64,
    pub memory: u64,
    pub uefi: bool,
    pub secure_boot: bool,
    pub vtpm: bool,
    pub disks: Vec<SourceDisk>,
    pub nics: Vec<SourceNic>,
    pub power: Power,
    /// Its guest tools (VMware Tools, the QEMU guest agent) answer: things can be run in it.
    pub agent: bool,
    /// Why it can't be brought over, if it can't.
    pub blocked: Option<String>,
    /// Something to know first.
    pub note: Option<String>,
    /// The ESXi host or Proxmox node it's on.
    pub node: String,
}

impl SourceVm {
    pub fn size(&self) -> u64 {
        self.disks.iter().map(|d| d.size).sum()
    }
    /// What's written on its disks (their size where not known).
    pub fn written(&self) -> u64 {
        self.disks.iter().map(|d| if d.used > 0 { d.used.min(d.size) } else { d.size }).sum()
    }
}

/// A disk's bytes, ready to send: an image whose written parts are known (sent as a VHD with
/// only those), or a stream of the whole disk (sent raw).
pub enum DiskData {
    Image(Box<dyn disk::Disk>),
    Stream { reader: Box<dyn Read + Send>, len: u64 },
}

/// How the pool's Helper VM can read a disk itself: what it's told (JSON), and the password it
/// needs (handed over as a XenAPI secret, never in the job).
pub struct HelperRead {
    pub job: serde_json::Value,
    pub password: String,
}

/// A connection to a source.
pub trait Source: Send + Sync {
    /// What it is, in words ("VMware ESXi 8.0.2", "Proxmox VE 8.2.4").
    fn describe(&self) -> String;
    fn list(&self) -> Result<Vec<SourceVm>, String>;
    /// Shuts the VM down cleanly, and waits till it's off (`say`: what's happening, for the job).
    fn shutdown(&self, vm: &SourceVm, say: &dyn Fn(String)) -> Result<(), String>;
    /// Runs the preparation in the VM through its guest tools (`login`: the guest's account, for
    /// VMware); what it printed.
    fn prepare(&self, vm: &SourceVm, login: Option<(&str, &str)>) -> Result<String, String>;
    /// A disk's bytes.
    fn open_disk(&self, vm: &SourceVm, d: &SourceDisk) -> Result<DiskData, String>;
    /// How the Helper VM can read this disk itself, when it can (a whole raw disk).
    fn helper_read(&self, _vm: &SourceVm, _d: &SourceDisk) -> Option<HelperRead> {
        None
    }
}

// ---------------------------------------------------------------- what's made on the pool

/// A VM to make on the pool for a converted one: from a template, with its CPUs, memory, boot
/// mode, empty disks of these sizes, and these cards.
#[derive(Clone, Debug, Default)]
pub struct Target {
    pub name: String,
    pub description: String,
    pub template: String,
    pub vcpus: i64,
    pub cores: i64,
    pub memory: i64,
    pub uefi: bool,
    pub secure_boot: bool,
    pub vtpm: bool,
    /// Its home server (required on one server's own storage).
    pub host: Option<String>,
    pub sr: String,
    /// Each disk's name and size.
    pub disks: Vec<(String, u64)>,
    /// Each card's network (reference) and MAC (empty: a new one).
    pub nics: Vec<(String, String)>,
    /// What the references are called, for the job's details.
    pub names: std::collections::HashMap<String, String>,
}

/// What was made: the VM and its disks (VDIs), in `Target::disks`' order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Made {
    pub vm: String,
    pub vdis: Vec<String>,
}

/// How a disk copy handed to the Helper VM is going (its task on the pool): the task's status
/// ("pending", "success", "failure", "cancelled"), how far (0 to 1), the helper that took it
/// (empty: none yet), and why it failed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HelperPoll {
    pub status: String,
    pub progress: f64,
    pub claimed: String,
    pub error: String,
}

/// The pool's other_config key of the Helper VM's heartbeat, and the task's keys for a copy it's
/// given (see helper-vm's copy service).
pub const HELPER_VM_KEY: &str = "svm-helper-vm";
pub const COPY_JOB: &str = "svm-copy";
pub const COPY_CLAIMED: &str = "svm-copy-claimed";
pub const COPY_ERROR: &str = "svm-copy-error";

/// The Helper VM, from its heartbeat: its machine's name, its version, whether it copies disks,
/// and whether it was heard from lately (its heartbeat comes every five minutes).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HelperVm {
    pub machine: String,
    pub version: String,
    pub copies: bool,
    pub fresh: bool,
}

impl HelperVm {
    pub fn parse(text: &str, now: i64) -> Option<HelperVm> {
        let j: serde_json::Value = serde_json::from_str(text).ok()?;
        let last = j.get("last_look").and_then(|v| v.as_i64()).unwrap_or(0);
        Some(HelperVm {
            machine: j.get("machine").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            version: j.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            copies: j.get("copy").and_then(|v| v.as_i64()).unwrap_or(0) >= 1,
            fresh: now - last < 15 * 60,
        })
    }
}

// ---------------------------------------------------------------- which template

/// The pool's template that fits a VM's system best: by its name and version, else the newest of
/// its family ("Ubuntu Linux (64-bit)" says no version), else none (Other install media).
pub fn pick_template<'a>(vm: &SourceVm, templates: &'a [TemplateInfo]) -> Option<&'a TemplateInfo> {
    let query = os_words(&vm.os);
    if let Some(t) = crate::model::detect_template(&query, templates) {
        return Some(t);
    }
    let q = query.to_lowercase();
    let server = q.contains("server") || q.contains("srv");
    let family = ["windows", "ubuntu", "debian", "rocky", "almalinux", "alma", "red hat", "rhel", "centos", "oracle", "suse", "sles", "fedora"].into_iter().find(|f| q.contains(f))?;
    let family = match family {
        "alma" => "almalinux",
        "rhel" => "red hat",
        "sles" => "suse",
        f => f,
    };
    let number = |name: &str| -> u64 {
        // its version, for "newest": 2022 > 2019, 24.04 > 22.04, 12 > 11
        let mut v = 0u64;
        for w in name.split(|c: char| !c.is_ascii_digit()).filter(|w| !w.is_empty()).take(2) {
            v = v * 10_000 + w.parse::<u64>().unwrap_or(0).min(9999);
        }
        v
    };
    templates
        .iter()
        .filter(|t| !t.name.contains("32-bit"))
        .filter(|t| t.name.to_lowercase().starts_with(family) || (family == "red hat" && t.name.starts_with("Red Hat")))
        .filter(|t| family != "windows" || t.name.to_lowercase().contains("server") == server)
        .max_by_key(|t| number(t.name.split('(').next().unwrap_or("")))
}

/// A system's name to show: what the source says in words (its id dropped), else its id's
/// meaning ("win10" → "Windows 10"), without "Microsoft" and "(64-bit)".
pub fn os_name(os: &str) -> String {
    let mut words = os.split_whitespace();
    let first = words.next().unwrap_or("");
    let rest: Vec<&str> = words.collect();
    let id_like = first.to_lowercase().ends_with("guest") || first.contains('_') || (first.contains('-') && first.chars().any(|c| c.is_ascii_digit())) || ["l26", "l24", "win11", "win10", "win8", "win7", "w2k8", "wvista", "wxp", "w2k3", "w2k", "solaris", "other"].contains(&first);
    let text = if id_like && !rest.is_empty() {
        rest.join(" ")
    } else if id_like {
        let w = os_words(first);
        if w.trim() == first { first.to_string() } else { w.replacen(first, "", 1).trim().to_string() }
    } else {
        os.to_string()
    };
    text.replace("Microsoft ", "").replace(" (64-bit)", "").replace(" (32-bit)", "").trim().to_string()
}

/// A system's name with VMware's and Proxmox's ids made into words ("windows2019srvNext_64Guest"
/// → "Windows Server 2022"), for `detect_template`.
pub fn os_words(os: &str) -> String {
    let l = os.to_lowercase();
    // VMware's id as its API writes it (windows2019srvNext_64Guest) or as a .vmx does
    // (windows2019srvnext-64), Proxmox's (win11)
    let raw_id = l.split_whitespace().next().unwrap_or("");
    let bare = raw_id.replace(['_', '-'], "");
    let guest = if bare.ends_with("guest") { bare.clone() } else { format!("{}guest", bare) };
    let known: &[(&str, &str)] = &[
        ("windows2022srvnext_64guest", "Windows Server 2025"),
        ("windows2019srvnext_64guest", "Windows Server 2022"),
        ("windows2019srv_64guest", "Windows Server 2019"),
        ("windows9server64guest", "Windows Server 2016"),
        ("windows8server64guest", "Windows Server 2012"),
        ("windows7server64guest", "Windows Server 2008"),
        ("windows11_64guest", "Windows 11"),
        ("windows12_64guest", "Windows 11"),
        ("windows9_64guest", "Windows 10"),
        ("windows9guest", "Windows 10"),
        ("windows8_64guest", "Windows 8"),
        ("windows7_64guest", "Windows 7"),
        ("win11", "Windows 11"),
        ("win10", "Windows 10"),
        ("win8", "Windows 8"),
        ("win7", "Windows 7"),
        ("ubuntu64guest", "Ubuntu"),
        ("rockylinux_64guest", "Rocky Linux"),
        ("almalinux_64guest", "AlmaLinux"),
        ("oraclelinux9_64guest", "Oracle Linux 9"),
        ("oraclelinux8_64guest", "Oracle Linux 8"),
        ("sles15_64guest", "SUSE Linux Enterprise Server 15"),
        ("sles12_64guest", "SUSE Linux Enterprise Server 12"),
        ("centos7_64guest", "CentOS 7"),
        ("centos8_64guest", "CentOS Stream 8"),
        ("centos9_64guest", "CentOS Stream 9"),
        ("l26", "Linux"),
    ];
    if let Some((_, w)) = known.iter().find(|(k, _)| {
        let k = k.replace('_', "");
        k == bare || k == guest
    }) {
        return format!("{} {}", w, os);
    }
    let id = guest.as_str();
    // debian12_64Guest, rhel9_64Guest…
    for (pre, name) in [("debian", "Debian"), ("rhel", "Red Hat Enterprise Linux"), ("centos", "CentOS"), ("oraclelinux", "Oracle Linux"), ("sles", "SUSE Linux Enterprise Server")] {
        if let Some(rest) = id.strip_prefix(pre) {
            let n: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !n.is_empty() && id.ends_with("guest") {
                return format!("{} {} {}", name, n, os);
            }
        }
    }
    os.to_string()
}

// ---------------------------------------------------------------- small things

/// Base64, for the commands pasted into Linux VMs and HTTP basic auth.
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn base64_decode(text: &str) -> Vec<u8> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = vec![];
    let (mut acc, mut bits) = (0u32, 0u32);
    for v in text.bytes().filter_map(val) {
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// An HTTP agent for a source's own server: its certificate is usually its own (self-signed),
/// and taken as it is, as with the pool's servers.
pub fn agent(read_secs: u64) -> ureq::Agent {
    let tls = native_tls::TlsConnector::builder().danger_accept_invalid_certs(true).danger_accept_invalid_hostnames(true).build().expect("TLS");
    ureq::AgentBuilder::new()
        .tls_connector(std::sync::Arc::new(tls))
        .timeout_connect(std::time::Duration::from_secs(15))
        .timeout_read(std::time::Duration::from_secs(read_secs))
        .timeout_write(std::time::Duration::from_secs(60))
        .redirects(0)
        .user_agent(concat!("SimpleVMManager/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// A request's failure, in words.
pub fn http_error(e: ureq::Error, what: &str) -> String {
    match e {
        ureq::Error::Status(401 | 403, _) => format!("{}: the user name or password isn't right (or that user may not do this).", what),
        ureq::Error::Status(404, _) => format!("{}: not found there (HTTP 404).", what),
        ureq::Error::Status(code, r) => {
            let body = r.into_string().unwrap_or_default();
            let fault = body.split("<faultstring>").nth(1).and_then(|s| s.split("</faultstring>").next()).map(str::trim).filter(|s| !s.is_empty());
            let msg = fault.map(str::to_string).or_else(|| serde_json::from_str::<serde_json::Value>(&body).ok().and_then(|j| j.get("message").and_then(|m| m.as_str()).map(|s| s.trim().to_string())));
            match msg {
                Some(m) => format!("{}: {} (HTTP {})", what, m, code),
                None => format!("{}: HTTP {}", what, code),
            }
        }
        ureq::Error::Transport(t) => match t.kind() {
            ureq::ErrorKind::Dns => format!("{}: that address isn't known (check its name).", what),
            ureq::ErrorKind::ConnectionFailed => format!("{}: couldn't connect (is it on, and is the address right?).", what),
            _ => format!("{}: {}", what, t),
        },
    }
}

/// "https://host" for an address typed as "host", "host:port" or a whole URL.
pub fn base_url(address: &str, default_port: u16) -> String {
    let a = address.trim().trim_end_matches('/');
    let a = a.strip_prefix("https://").or_else(|| a.strip_prefix("http://")).unwrap_or(a);
    let host = a.split('/').next().unwrap_or(a);
    let has_port = if host.contains("]:") {
        true
    } else if host.starts_with('[') {
        false
    } else {
        host.matches(':').count() == 1 && host.rsplit(':').next().map_or(false, |p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    };
    if has_port || default_port == 443 {
        format!("https://{}", host)
    } else {
        format!("https://{}:{}", host, default_port)
    }
}

/// A MAC as XenServer writes it: lower case, colons.
pub fn norm_mac(mac: &str) -> String {
    let hex: String = mac.chars().filter(|c| c.is_ascii_hexdigit()).collect::<String>().to_lowercase();
    if hex.len() != 12 {
        return String::new();
    }
    (0..6).map(|i| &hex[i * 2..i * 2 + 2]).collect::<Vec<_>>().join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tpl(name: &str) -> TemplateInfo {
        TemplateInfo { rref: name.into(), name: name.into(), ..Default::default() }
    }

    fn vm(os: &str) -> SourceVm {
        SourceVm {
            id: "1".into(),
            name: "x".into(),
            os: os.into(),
            family: family_of(os),
            cpus: 1,
            cores: 1,
            memory: 1 << 30,
            uefi: false,
            secure_boot: false,
            vtpm: false,
            disks: vec![],
            nics: vec![],
            power: Power::Off,
            agent: false,
            blocked: None,
            note: None,
            node: String::new(),
        }
    }

    #[test]
    fn templates_from_vmware_and_proxmox_names() {
        let ts: Vec<TemplateInfo> = [
            "Windows Server 2019 (64-bit)",
            "Windows Server 2022 (64-bit)",
            "Windows 10 (64-bit)",
            "Windows 11",
            "Ubuntu Focal Fossa 20.04",
            "Ubuntu Jammy Jellyfish 22.04",
            "Ubuntu Noble Numbat 24.04",
            "Debian Bookworm 12",
            "Debian Bullseye 11",
            "Red Hat Enterprise Linux 9",
            "Rocky Linux 9",
            "Other install media",
        ]
        .iter()
        .map(|n| tpl(n))
        .collect();
        let name = |os: &str| pick_template(&vm(os), &ts).map(|t| t.name.clone()).unwrap_or_default();
        assert_eq!(name("windows2019srvNext_64Guest Microsoft Windows Server 2022 (64-bit)"), "Windows Server 2022 (64-bit)");
        assert_eq!(name("Microsoft Windows Server 2019 (64-bit)"), "Windows Server 2019 (64-bit)");
        assert_eq!(name("windows11_64Guest"), "Windows 11");
        assert_eq!(name("win10"), "Windows 10 (64-bit)");
        assert_eq!(name("Ubuntu Linux (64-bit)"), "Ubuntu Noble Numbat 24.04");
        assert_eq!(name("Ubuntu 22.04.4 LTS"), "Ubuntu Jammy Jellyfish 22.04");
        assert_eq!(name("debian12_64Guest Debian GNU/Linux 12 (64-bit)"), "Debian Bookworm 12");
        assert_eq!(name("rhel9_64Guest"), "Red Hat Enterprise Linux 9");
        assert_eq!(name("Rocky Linux (64-bit)"), "Rocky Linux 9");
        assert_eq!(name("FreeBSD 14"), "");
        assert_eq!(family_of("win11"), Family::Windows);
        assert_eq!(os_name("windows2019srvNext_64Guest Microsoft Windows Server 2022 (64-bit)"), "Windows Server 2022");
        assert_eq!(os_name("ubuntu64Guest Ubuntu Linux (64-bit)"), "Ubuntu Linux");
        assert_eq!(os_name("l26 Debian GNU/Linux 12 (bookworm)"), "Debian GNU/Linux 12 (bookworm)");
        assert_eq!(os_name("win10"), "Windows 10");
        assert_eq!(os_name("windows2019srvnext-64"), "Windows Server 2022");
        assert_eq!(os_name("l26"), "Linux");
        assert_eq!(family_of("l26"), Family::Linux);
    }

    #[test]
    fn base64_both_ways() {
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            assert_eq!(base64_decode(&base64(s.as_bytes())), s.as_bytes());
        }
        assert_eq!(base64(b"root:secret"), "cm9vdDpzZWNyZXQ=");
    }

    #[test]
    fn addresses_and_macs() {
        assert_eq!(base_url("pve.lan", 8006), "https://pve.lan:8006");
        assert_eq!(base_url("https://pve.lan:8006/", 8006), "https://pve.lan:8006");
        assert_eq!(base_url("esxi-01.lan", 443), "https://esxi-01.lan");
        assert_eq!(base_url("10.0.0.5:9443", 443), "https://10.0.0.5:9443");
        assert_eq!(norm_mac("00:50:56:AA:bb:0C"), "00:50:56:aa:bb:0c");
        assert_eq!(norm_mac("BC-24-11-01-02-03"), "bc:24:11:01:02:03");
        assert_eq!(norm_mac("junk"), "");
    }

    #[test]
    fn remembered_sources_round_trip() {
        let list = vec![SourceConf { kind: Kind::Vmware, address: "esxi-01.lan".into(), user: "root".into() }, SourceConf { kind: Kind::Proxmox, address: "pve.lan".into(), user: "root@pam".into() }];
        assert_eq!(parse_sources(&sources_text(&list)), list);
        assert_eq!(list[1].key(), "proxmox:pve.lan");
        let p = vec![("vmware:esxi".to_string(), "vm-12".to_string(), 1_700_000_000i64)];
        assert_eq!(parse_prepared(&prepared_text(&p)), p);
    }
}
