//! Proxmox VE as a source: its API (port 8006) lists the VMs and their settings, shuts them down,
//! and runs the preparation in a VM through the QEMU guest agent. Its API can't hand a disk's
//! contents out, so disks are read over SSH, as root with the same password, through the
//! system's own `ssh`: a raw disk (LVM-thin, ZFS, a .raw file) as one stream, an image (qcow2,
//! vmdk) through a small Perl reader on the Proxmox server that sends the parts asked for.

use super::disk::ReadAt;
use super::*;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Proxmox {
    base: String,
    host: String,
    user: String,
    password: String,
    agent: ureq::Agent,
    /// Its ticket and CSRF token, once logged in; its version.
    login: Mutex<Option<(String, String)>>,
    version: Mutex<String>,
}

/// The size in a disk's setting ("32G", "512M", "10240K", "1T") in bytes.
pub fn size_of(s: &str) -> u64 {
    let s = s.trim();
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len()));
    let n: f64 = n.parse().unwrap_or(0.0);
    let m = match unit.trim().to_uppercase().as_str() {
        "K" => 1u64 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        "T" => 1 << 40,
        _ => 1,
    };
    (n * m as f64) as u64
}

/// A setting's options: "local-lvm:vm-100-disk-0,size=32G,ssd=1" → first part, then key=value.
fn options(v: &str) -> (String, Vec<(String, String)>) {
    let mut parts = v.split(',');
    let first = parts.next().unwrap_or("").to_string();
    let kv = parts.filter_map(|p| p.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))).collect();
    (first, kv)
}

fn opt<'a>(kv: &'a [(String, String)], k: &str) -> Option<&'a str> {
    kv.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
}

fn json_of(r: ureq::Response) -> Result<serde_json::Value, String> {
    let text = r.into_string().map_err(|e| format!("Proxmox's answer broke off: {}", e))?;
    serde_json::from_str(&text).map_err(|e| format!("Proxmox's answer can't be read: {}", e))
}

fn jstr(v: &serde_json::Value, k: &str) -> String {
    match v.get(k) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn jnum(v: &serde_json::Value, k: &str) -> u64 {
    match v.get(k) {
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// One of a VM's disks as its settings say: its key ("scsi0"), its volume ("local-lvm:vm-100-disk-0"
/// or a device path), its size.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfDisk {
    pub key: String,
    pub volume: String,
    pub size: u64,
}

/// A VM from its settings (GET …/qemu/{vmid}/config), as far as they say: everything but where
/// its disks' bytes are.
pub fn vm_from_config(vmid: &str, node: &str, status: &str, conf: &serde_json::Value) -> (SourceVm, Vec<ConfDisk>) {
    let name = Some(jstr(conf, "name")).filter(|n| !n.is_empty()).unwrap_or_else(|| format!("VM {}", vmid));
    let cores = jnum(conf, "cores").max(1) as i64;
    let sockets = jnum(conf, "sockets").max(1) as i64;
    let memory = (if jnum(conf, "memory") > 0 { jnum(conf, "memory") } else { 512 }) << 20;
    let uefi = jstr(conf, "bios") == "ovmf";
    let efidisk = jstr(conf, "efidisk0");
    let secure = uefi && efidisk.contains("pre-enrolled-keys=1");
    let vtpm = uefi && !jstr(conf, "tpmstate0").is_empty();
    let agent_on = {
        let a = jstr(conf, "agent");
        a == "1" || a.starts_with("1,") || a.contains("enabled=1")
    };
    let ostype = jstr(conf, "ostype");
    // its disks, the boot disk first (as its boot order says), then by bus and number
    let mut found: Vec<ConfDisk> = vec![];
    let mut blocked = None;
    if let Some(map) = conf.as_object() {
        for (k, v) in map {
            let bus = ["scsi", "virtio", "sata", "ide"].iter().find(|b| k.starts_with(*b) && k[b.len()..].chars().all(|c| c.is_ascii_digit()) && k.len() > b.len());
            if bus.is_none() {
                continue;
            }
            let v = v.as_str().unwrap_or("");
            let (volume, kv) = options(v);
            if opt(&kv, "media") == Some("cdrom") || volume == "none" || volume.is_empty() {
                continue;
            }
            if volume.starts_with('/') {
                blocked.get_or_insert_with(|| format!("{} is a disk passed through from the server ({}), not one of Proxmox's volumes.", k, volume));
                continue;
            }
            found.push(ConfDisk { key: k.clone(), volume, size: opt(&kv, "size").map(size_of).unwrap_or(0) });
        }
    }
    let boot = jstr(conf, "boot");
    let order: Vec<String> = match boot.strip_prefix("order=") {
        Some(o) => o.split(';').map(str::to_string).collect(),
        None => vec![jstr(conf, "bootdisk")],
    };
    let rank = |d: &ConfDisk| -> (usize, usize, u32) {
        let pos = order.iter().position(|o| *o == d.key).unwrap_or(99);
        let bus = ["scsi", "virtio", "sata", "ide"].iter().position(|b| d.key.starts_with(b)).unwrap_or(9);
        let n = d.key.trim_start_matches(|c: char| c.is_ascii_alphabetic()).parse().unwrap_or(0);
        (pos, bus, n)
    };
    found.sort_by_key(rank);
    // its cards: "virtio=BC:24:11:00:00:01,bridge=vmbr0,tag=20"
    let mut nics: Vec<(u32, SourceNic)> = vec![];
    if let Some(map) = conf.as_object() {
        for (k, v) in map {
            let Some(n) = k.strip_prefix("net").and_then(|n| n.parse::<u32>().ok()) else { continue };
            let (model, kv) = options(v.as_str().unwrap_or(""));
            let mac = model.split_once('=').map(|(_, m)| norm_mac(m)).unwrap_or_default();
            nics.push((n, SourceNic { network: opt(&kv, "bridge").unwrap_or("").to_string(), vlan: opt(&kv, "tag").and_then(|t| t.parse().ok()), mac }));
        }
    }
    nics.sort_by_key(|n| n.0);
    let power = match status {
        "running" => Power::On,
        "stopped" => Power::Off,
        "paused" | "suspended" => Power::Paused,
        _ => Power::Unknown,
    };
    if jstr(conf, "template") == "1" {
        blocked = Some("It's a template.".into());
    }
    if found.is_empty() && blocked.is_none() {
        blocked = Some("It has no disks.".into());
    }
    let vm = SourceVm {
        id: format!("{}/{}", node, vmid),
        name,
        family: family_of(&ostype),
        os: ostype,
        cpus: cores * sockets,
        cores,
        memory,
        uefi,
        secure_boot: secure,
        vtpm,
        disks: vec![],
        nics: nics.into_iter().map(|n| n.1).collect(),
        power,
        agent: agent_on && power == Power::On,
        blocked,
        note: None,
        node: node.into(),
    };
    (vm, found)
}

impl Proxmox {
    pub fn new(address: &str, user: &str, password: &str) -> Proxmox {
        let base = base_url(address, 8006);
        let host = base.trim_start_matches("https://").rsplit_once(':').map(|(h, _)| h.to_string()).unwrap_or_default();
        let user = if user.contains('@') { user.to_string() } else { format!("{}@pam", if user.is_empty() { "root" } else { user }) };
        Proxmox { base, host, user, password: password.into(), agent: agent(120), login: Mutex::new(None), version: Mutex::new(String::new()) }
    }

    /// The SSH user: the API user without its realm (root@pam → root).
    fn ssh_user(&self) -> String {
        self.user.split('@').next().unwrap_or("root").to_string()
    }

    fn log_in(&self) -> Result<(String, String), String> {
        let r = self
            .agent
            .post(&format!("{}/api2/json/access/ticket", self.base))
            .send_form(&[("username", &self.user), ("password", &self.password)])
            .map_err(|e| match e {
                ureq::Error::Status(401, _) => "Proxmox: the user name or password isn't right.".to_string(),
                e => http_error(e, "Proxmox"),
            })?;
        let j = json_of(r)?;
        let d = &j["data"];
        let (ticket, csrf) = (jstr(d, "ticket"), jstr(d, "CSRFPreventionToken"));
        if ticket.is_empty() {
            return Err("Proxmox didn't log in (no ticket): is that a Proxmox VE server?".into());
        }
        *self.login.lock().unwrap() = Some((ticket.clone(), csrf.clone()));
        Ok((ticket, csrf))
    }

    fn ticket(&self) -> Result<(String, String), String> {
        if let Some(t) = self.login.lock().unwrap().clone() {
            return Ok(t);
        }
        self.log_in()
    }

    pub fn get(&self, path: &str) -> Result<serde_json::Value, String> {
        let (t, _) = self.ticket()?;
        let r = self.agent.get(&format!("{}/api2/json{}", self.base, path)).set("Cookie", &format!("PVEAuthCookie={}", t)).call();
        let r = match r {
            Err(ureq::Error::Status(401, _)) => {
                let (t, _) = self.log_in()?;
                self.agent.get(&format!("{}/api2/json{}", self.base, path)).set("Cookie", &format!("PVEAuthCookie={}", t)).call()
            }
            r => r,
        };
        let j = json_of(r.map_err(|e| http_error(e, "Proxmox"))?)?;
        Ok(j["data"].clone())
    }

    pub fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
        let (t, csrf) = self.ticket()?;
        let r = self.agent.post(&format!("{}/api2/json{}", self.base, path)).set("Cookie", &format!("PVEAuthCookie={}", t)).set("CSRFPreventionToken", &csrf).set("Content-Type", "application/json").send_string(&body.to_string());
        let j = json_of(r.map_err(|e| http_error(e, "Proxmox"))?)?;
        Ok(j["data"].clone())
    }

    fn vm_path(vm: &SourceVm) -> String {
        let (node, vmid) = vm.id.split_once('/').unwrap_or(("", &vm.id));
        format!("/nodes/{}/qemu/{}", node, vmid)
    }

    fn status(&self, vm: &SourceVm) -> Result<String, String> {
        Ok(jstr(&self.get(&format!("{}/status/current", Self::vm_path(vm)))?, "status"))
    }

    /// Where a volume's bytes are on its node (a device or file), its format, size and what's
    /// written, from the storage's own listing.
    fn volume(&self, node: &str, volume: &str) -> Option<(String, String, u64, u64)> {
        let storage = volume.split(':').next()?;
        let enc: String = volume.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.".contains(&b) { (b as char).to_string() } else { format!("%{:02X}", b) }).collect();
        let d = self.get(&format!("/nodes/{}/storage/{}/content/{}", node, storage, enc)).ok()?;
        let path = jstr(&d, "path");
        (!path.is_empty()).then(|| (path, jstr(&d, "format"), jnum(&d, "size"), jnum(&d, "used")))
    }

    fn ssh(&self, remote: &str) -> Result<Child, String> {
        ssh_spawn(&self.host, &self.ssh_user(), &self.password, remote)
    }
}

impl Source for Proxmox {
    fn describe(&self) -> String {
        let v = self.version.lock().unwrap().clone();
        if v.is_empty() { "Proxmox VE".into() } else { format!("Proxmox VE {}", v) }
    }

    fn list(&self) -> Result<Vec<SourceVm>, String> {
        if let Ok(v) = self.get("/version") {
            *self.version.lock().unwrap() = jstr(&v, "version");
        }
        let all = self.get("/cluster/resources?type=vm")?;
        let mut out = vec![];
        for r in all.as_array().cloned().unwrap_or_default() {
            let (node, vmid, status) = (jstr(&r, "node"), jstr(&r, "vmid"), jstr(&r, "status"));
            if jstr(&r, "type") != "qemu" || jnum(&r, "template") == 1 {
                continue;
            }
            let conf = match self.get(&format!("/nodes/{}/qemu/{}/config", node, vmid)) {
                Ok(c) => c,
                Err(e) => {
                    out.push(SourceVm { id: format!("{}/{}", node, vmid), name: jstr(&r, "name"), os: String::new(), family: Family::Other, cpus: 1, cores: 1, memory: 0, uefi: false, secure_boot: false, vtpm: false, disks: vec![], nics: vec![], power: Power::Unknown, agent: false, blocked: Some(e), note: None, node: node.clone() });
                    continue;
                }
            };
            let (mut vm, confd) = vm_from_config(&vmid, &node, &status, &conf);
            for d in confd {
                match self.volume(&node, &d.volume) {
                    Some((path, format, size, used)) => {
                        if !path.starts_with('/') {
                            vm.blocked.get_or_insert_with(|| format!("{} is on storage that isn't a device or file on the server ({}): move it to local storage first (Disk Action, Move Storage).", d.key, path));
                            continue;
                        }
                        let size = if size > 0 { size } else { d.size };
                        vm.disks.push(SourceDisk { label: format!("{} ({})", d.key, d.volume), size, used: used.min(size), at: DiskAt::Ssh { path, format: if format.is_empty() { "raw".into() } else { format } } });
                    }
                    None => {
                        vm.blocked.get_or_insert_with(|| format!("Proxmox didn't say where {} ({}) is.", d.key, d.volume));
                    }
                }
            }
            // its system's own name, from the guest agent, when it answers
            if vm.agent {
                let info = self.get(&format!("/nodes/{}/qemu/{}/agent/get-osinfo", node, vmid)).ok();
                match info.as_ref().map(|i| &i["result"]) {
                    Some(res) if !jstr(res, "pretty-name").is_empty() => {
                        vm.os = format!("{} {}", vm.os, jstr(res, "pretty-name"));
                        vm.family = family_of(&vm.os);
                    }
                    Some(_) => {}
                    None => vm.agent = false,
                }
            }
            out.push(vm);
        }
        out.sort_by_key(|v| v.name.to_lowercase());
        Ok(out)
    }

    fn shutdown(&self, vm: &SourceVm, say: &dyn Fn(String)) -> Result<(), String> {
        if self.status(vm)? == "stopped" {
            return Ok(());
        }
        say("Shutting it down on Proxmox…".into());
        self.post(&format!("{}/status/shutdown", Self::vm_path(vm)), serde_json::json!({ "timeout": 600 }))?;
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(620) {
            std::thread::sleep(Duration::from_secs(4));
            if self.status(vm)? == "stopped" {
                return Ok(());
            }
        }
        Err("It didn't shut down within 10 minutes: shut it down in Proxmox, then Try Again.".into())
    }

    fn prepare(&self, vm: &SourceVm, _login: Option<(&str, &str)>) -> Result<String, String> {
        let path = Self::vm_path(vm);
        let body = if vm.family == Family::Windows {
            serde_json::json!({ "command": ["cmd.exe", "/c", prep::windows_inner()] })
        } else {
            serde_json::json!({ "command": ["sh"], "input-data": prep::LINUX_SCRIPT })
        };
        let started = self.post(&format!("{}/agent/exec", path), body).map_err(|e| if e.contains("not running") || e.contains("QEMU guest agent") { "The QEMU guest agent isn't running in it: install it (qemu-guest-agent), or paste the command yourself.".to_string() } else { e })?;
        let pid = jnum(&started, "pid");
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(1200) {
            std::thread::sleep(Duration::from_secs(3));
            let s = self.get(&format!("{}/agent/exec-status?pid={}", path, pid))?;
            if jnum(&s, "exited") == 1 {
                let out = jstr(&s, "out-data") + &jstr(&s, "err-data");
                // (the agent's output is base64 in its protocol; Proxmox decodes it, older ones may not)
                let decoded = String::from_utf8(base64_decode(&out)).ok().filter(|d| d.contains(prep::DONE));
                let out = decoded.unwrap_or(out);
                if out.trim().is_empty() {
                    return if jnum(&s, "exitcode") == 0 { Ok(prep::DONE.into()) } else { Err(format!("It ended with code {} in the VM.", jnum(&s, "exitcode"))) };
                }
                return Ok(out);
            }
        }
        Err("It's still running in the VM after 20 minutes.".into())
    }

    fn open_disk(&self, _vm: &SourceVm, d: &SourceDisk) -> Result<DiskData, String> {
        let DiskAt::Ssh { path, format } = &d.at else { return Err("Not a Proxmox disk.".into()) };
        if format == "raw" {
            let mut child = self.ssh(&raw_command(path))?;
            let mut out = BufReader::new(child.stdout.take().ok_or("ssh gave no output")?);
            let mut first = String::new();
            out.read_line(&mut first).map_err(|e| format!("Reading the disk over SSH: {}", e))?;
            let len = first.trim().parse::<u64>().map_err(|_| ssh_failed(&mut child, &first))?;
            if len != d.size {
                return Err(format!("The disk is {} bytes on the server, not {} as Proxmox says.", len, d.size));
            }
            return Ok(DiskData::Stream { reader: Box::new(SshStream { child, out }), len });
        }
        let remote = RemoteAt::open(&self.host, &self.ssh_user(), &self.password, path)?;
        disk::open(Box::new(remote), None).map(DiskData::Image).map_err(|e| format!("{}: {}", path, e))
    }

    fn helper_read(&self, _vm: &SourceVm, d: &SourceDisk) -> Option<HelperRead> {
        let DiskAt::Ssh { path, format } = &d.at else { return None };
        (format == "raw").then(|| HelperRead { job: serde_json::json!({ "kind": "ssh", "host": self.host, "port": 22, "user": self.ssh_user(), "command": raw_command(path), "size": d.size }), password: self.password.clone() })
    }
}

// ---------------------------------------------------------------- SSH

/// A shell word: 'it'.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The remote command for a raw disk: its size on the first line, then its bytes. A thin LVM
/// volume of a VM that's off may be inactive: it's activated first.
pub fn raw_command(path: &str) -> String {
    let p = sh_quote(path);
    format!("P={}; [ -e \"$P\" ] || lvchange -ay \"$P\" >/dev/null 2>&1; (blockdev --getsize64 \"$P\" 2>/dev/null || stat -Lc %s \"$P\") && exec cat \"$P\"", p)
}

/// The Perl reader: the file's size, then the bytes asked for ("offset length" lines).
const READER: &str = r#"use strict; my $p = shift; if (!-e $p) { my $pid = fork; if (!$pid) { open STDOUT, ">", "/dev/null"; open STDERR, ">", "/dev/null"; exec "lvchange", "-ay", $p; exit 1; } waitpid($pid, 0); } open(my $f, "<", $p) or die "cannot open $p: $!\n"; binmode $f; binmode STDOUT; $| = 1; my $n = sysseek($f, 0, 2); $n = 0 if $n eq "0 but true"; print "$n\n"; while (my $l = <STDIN>) { my ($o, $len) = split " ", $l; sysseek($f, $o, 0); my $buf = ""; my $got = 0; while ($got < $len) { my $r = sysread($f, $buf, $len - $got, $got); last unless $r; $got += $r; } $buf .= "\0" x ($len - $got) if $got < $len; print $buf; }"#;

/// Where the app keeps a file of its own (with its settings).
pub fn config_file(name: &str) -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(base.join("SimpleVMManager").join(name))
}

/// The environment variables that make this app answer `ssh`'s password question (see main.rs).
pub const ASKPASS: &str = "SVM_ASKPASS";
pub const ASKPASS_SECRET: &str = "SVM_ASKPASS_SECRET";

/// Runs `remote` on `host` with the system's ssh, logging in with the password: ssh asks this
/// same program for it (SSH_ASKPASS), which prints it from its environment. The server's key is
/// taken the first time and checked after (its own known_hosts file).
pub fn ssh_spawn(host: &str, user: &str, password: &str, remote: &str) -> Result<Child, String> {
    let exe = std::env::current_exe().map_err(|e| format!("Can't find this program to answer ssh: {}", e))?;
    let known = config_file("convert_known_hosts").map(|p| {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        p.display().to_string()
    });
    let mut c = Command::new("ssh");
    c.args(["-T", "-o", "StrictHostKeyChecking=accept-new", "-o", "ConnectTimeout=15", "-o", "ServerAliveInterval=30", "-o", "NumberOfPasswordPrompts=1"]);
    c.args(["-o", "PubkeyAuthentication=no", "-o", "PreferredAuthentications=password,keyboard-interactive"]);
    if let Some(k) = known {
        c.arg("-o").arg(format!("UserKnownHostsFile={}", k));
    }
    c.arg("-l").arg(user).arg(host).arg(remote);
    c.env("SSH_ASKPASS", exe).env("SSH_ASKPASS_REQUIRE", "force").env(ASKPASS, "1").env(ASKPASS_SECRET, password);
    if std::env::var_os("DISPLAY").is_none() {
        c.env("DISPLAY", ":0");
    }
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    c.spawn().map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { "This computer has no ssh program (OpenSSH): install it to read Proxmox's disks.".to_string() } else { format!("ssh couldn't start: {}", e) })
}

/// Why an ssh command gave nothing usable: what it said on stderr.
fn ssh_failed(child: &mut Child, got: &str) -> String {
    let _ = child.kill();
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut e, &mut err);
    }
    let _ = child.wait();
    let said = err.lines().filter(|l| !l.trim().is_empty() && !l.contains("Warning: Permanently added")).last().unwrap_or(got).trim().to_string();
    if said.contains("Permission denied") {
        "SSH to Proxmox: the password isn't right, or root may not log in with a password (PermitRootLogin).".into()
    } else if said.contains("Host key verification failed") || said.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        "SSH to Proxmox: its key isn't the one seen before. If the server was reinstalled, remove its line from convert_known_hosts in the app's settings folder.".into()
    } else if said.is_empty() {
        "SSH to Proxmox gave nothing back.".into()
    } else {
        format!("SSH to Proxmox: {}", said)
    }
}

/// A raw disk's bytes coming over ssh (the process ends with it).
struct SshStream {
    child: Child,
    out: BufReader<ChildStdout>,
}

impl std::io::Read for SshStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.out.read(buf)
    }
}

impl Drop for SshStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A file on the Proxmox server, read at any place through the Perl reader.
pub struct RemoteAt {
    child: Child,
    input: ChildStdin,
    out: BufReader<ChildStdout>,
    len: u64,
}

impl RemoteAt {
    pub fn open(host: &str, user: &str, password: &str, path: &str) -> Result<RemoteAt, String> {
        let mut child = ssh_spawn(host, user, password, &format!("perl -e {} {}", sh_quote(READER), sh_quote(path)))?;
        let input = child.stdin.take().ok_or("ssh took no input")?;
        let mut out = BufReader::new(child.stdout.take().ok_or("ssh gave no output")?);
        let mut first = String::new();
        out.read_line(&mut first).map_err(|e| format!("Reading over SSH: {}", e))?;
        let len = first.trim().parse::<u64>().map_err(|_| ssh_failed(&mut child, &first))?;
        Ok(RemoteAt { child, input, out, len })
    }
}

impl ReadAt for RemoteAt {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> std::io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        writeln!(self.input, "{} {}", off, buf.len())?;
        self.input.flush()?;
        std::io::Read::read_exact(&mut self.out, buf)
    }
    fn len(&self) -> u64 {
        self.len
    }
}

impl Drop for RemoteAt {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vm_from_its_settings() {
        let conf = serde_json::json!({
            "name": "pihole", "cores": 2, "sockets": 1, "memory": "2048", "bios": "ovmf", "ostype": "l26",
            "efidisk0": "local-lvm:vm-101-disk-0,efitype=4m,pre-enrolled-keys=1,size=4M",
            "scsi1": "local-lvm:vm-101-disk-2,size=8G", "scsi0": "local-lvm:vm-101-disk-1,iothread=1,size=32G",
            "ide2": "local:iso/debian-12.iso,media=cdrom,size=600M", "boot": "order=scsi0;ide2;net0",
            "net0": "virtio=BC:24:11:AA:BB:CC,bridge=vmbr0,firewall=1,tag=20", "net1": "e1000=BC:24:11:00:00:01,bridge=vmbr1",
            "agent": "1", "scsihw": "virtio-scsi-single"
        });
        let (v, disks) = vm_from_config("101", "pve", "running", &conf);
        assert_eq!(v.id, "pve/101");
        assert_eq!((v.cpus, v.cores, v.memory), (2, 2, 2 << 30));
        assert!(v.uefi && v.secure_boot && !v.vtpm);
        assert!(v.agent);
        assert_eq!(v.family, Family::Linux);
        assert_eq!(disks.iter().map(|d| d.key.as_str()).collect::<Vec<_>>(), vec!["scsi0", "scsi1"]);
        assert_eq!(disks[0].size, 32 << 30);
        assert_eq!(v.nics[0], SourceNic { network: "vmbr0".into(), vlan: Some(20), mac: "bc:24:11:aa:bb:cc".into() });
        assert_eq!(v.nics[1].network, "vmbr1");
        assert!(v.blocked.is_none());
    }

    #[test]
    fn sizes_and_quoting() {
        assert_eq!(size_of("32G"), 32 << 30);
        assert_eq!(size_of("512M"), 512 << 20);
        assert_eq!(size_of("1T"), 1 << 40);
        assert_eq!(size_of("4096"), 4096);
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert!(raw_command("/dev/pve/vm-100-disk-0").starts_with("P='/dev/pve/vm-100-disk-0';"));
    }

    #[test]
    fn the_reader_and_raw_command_work_here() {
        // the same commands, run by sh and perl on this computer instead of over ssh
        if Command::new("perl").arg("-v").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("svm-pve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("d.raw");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&f, &data).unwrap();
        // the Perl reader
        let mut child = Command::new("perl").arg("-e").arg(READER).arg(&f).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let mut r = RemoteAt { input: child.stdin.take().unwrap(), out: BufReader::new(child.stdout.take().unwrap()), child, len: 0 };
        let mut first = String::new();
        r.out.read_line(&mut first).unwrap();
        r.len = first.trim().parse().unwrap();
        assert_eq!(r.len, 300_000);
        let mut buf = vec![0u8; 1000];
        r.read_at(123_456, &mut buf).unwrap();
        assert_eq!(&buf[..], &data[123_456..124_456]);
        // past the end: zeros
        r.read_at(299_500, &mut buf).unwrap();
        assert_eq!(&buf[..500], &data[299_500..]);
        assert!(buf[500..].iter().all(|&b| b == 0));
        // the raw command: size, then the bytes
        let out = Command::new("sh").arg("-c").arg(raw_command(&f.display().to_string())).output().unwrap();
        let nl = out.stdout.iter().position(|&b| b == b'\n').unwrap();
        assert_eq!(std::str::from_utf8(&out.stdout[..nl]).unwrap(), "300000");
        assert_eq!(&out.stdout[nl + 1..], &data[..]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
