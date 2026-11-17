//! Talking to servers: JSON-RPC 2.0 over HTTPS at /jsonrpc (as the original's XenAPI.JsonRpcClient),
//! and an in-memory demo pool (the original has simulated connections too).

use crate::model::*;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const DEFAULT_PORT: u16 = 443; // XenAPI.HTTP.DEFAULT_HTTPS_PORT
/// How many servers a login tries (the address given, the pool's other servers, and where they
/// point as their coordinator), each once.
pub const MAX_LOGIN_ATTEMPTS: usize = 8;
/// How long reaching a server may take: a server that's off doesn't refuse, it just never
/// answers, so without this each try would wait for the whole call's time.
pub const CONNECT_TIMEOUT_MS: u64 = 6_000;
/// How long a pool may take to say which updates its servers lack (seconds).
const UPDATES_WITHIN: u64 = 15 * 60;
pub const ACTION_TIMEOUT_MS: u64 = 86_400_000; // XenAPI.Session.STANDARD_TIMEOUT
pub const DEMO_ACTION_MS: u64 = 1500; // estimated
/// A storage check's error codes (see `Connection::check_sr`): it didn't answer in time; it isn't
/// plugged into the server it's for. The storage's name is the first detail.
pub const NOT_ANSWERING: &str = "SR_NOT_ANSWERING";
pub const NOT_PLUGGED: &str = "SR_NOT_PLUGGED";
/// A revert that worked, but the VM couldn't be started (or resumed) again after it.
pub const NOT_RESTARTED: &str = "REVERTED_NOT_STARTED";
/// A long job's server stopped answering (a power cut, say): its name in the details.
pub const LOST_CONTACT: &str = "LOST_CONTACT";
/// How long a server may not answer while a job runs on it before the job counts as failed.
pub const LOST_AFTER: Duration = Duration::from_secs(100);

/// How far a long job's task on the server is, and whether the server still answers: shared
/// between the thread doing the job (see `watch`) and the Activities list.
pub struct Progress {
    /// Every call the job made and the server's answers, for its details.
    pub trace: Arc<Trace>,
    /// Per mille; u32::MAX while the server hasn't said.
    permille: AtomicU64,
    /// Since when the server hasn't answered, while it doesn't.
    silent_since: Mutex<Option<Instant>>,
}

impl Progress {
    pub fn new() -> Arc<Progress> {
        Arc::new(Progress { trace: Arc::new(Trace::default()), permille: AtomicU64::new(u64::MAX), silent_since: Mutex::new(None) })
    }
    /// How far it is (0 to 1), once the server says.
    pub fn fraction(&self) -> Option<f32> {
        let p = self.permille.load(Ordering::Relaxed);
        (p != u64::MAX).then(|| p.min(1000) as f32 / 1000.0)
    }
    pub fn set_fraction(&self, f: f64) {
        self.permille.store((f.clamp(0.0, 1.0) * 1000.0) as u64, Ordering::Relaxed);
    }
    /// How long the server hasn't answered, while it doesn't.
    pub fn silent_for(&self) -> Option<Duration> {
        self.silent_since.lock().unwrap().map(|t| t.elapsed())
    }
    fn heard(&self, yes: bool) {
        let mut s = self.silent_since.lock().unwrap();
        match (yes, *s) {
            (true, _) => *s = None,
            (false, None) => *s = Some(Instant::now()),
            _ => {}
        }
    }
}

thread_local! {
    /// The progress of the job this thread is doing, if it's watched.
    static WATCHED: std::cell::RefCell<Option<Arc<Progress>>> = const { std::cell::RefCell::new(None) };
}

/// From now on this thread's long calls report to `p` (call first thing in a job's thread).
pub fn watch(p: &Arc<Progress>) {
    WATCHED.with(|w| *w.borrow_mut() = Some(p.clone()));
}

fn watched() -> Option<Arc<Progress>> {
    WATCHED.with(|w| w.borrow().clone())
}

/// Notes in the watched job's details a call it's making: the method and its arguments, any
/// password hidden.
fn said(method: &str, args: &[Value]) {
    if let Some(w) = watched() {
        w.trace.push(Said::Asked, format!("{}  {}", method, args_text(args)).trim_end().to_string());
    }
}

/// Notes the app's own remark in the watched job's details.
fn note(text: impl Into<String>) {
    if let Some(w) = watched() {
        w.trace.push(Said::Note, text);
    }
}

/// Minutes and seconds, "4 min 12 s".
fn took_text(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 { format!("{} s", s) } else { format!("{} min {} s", s / 60, s % 60) }
}

/// Notes in the watched job's details how its last call went.
fn answered(r: &Result<Value, ApiError>, took: Duration) {
    if let Some(w) = watched() {
        w.trace.outcome(r, took);
    }
}

/// A call's arguments as text: references as they are, strings quoted, records compact, any
/// password hidden, the whole cut short when very long.
fn args_text(args: &[Value]) -> String {
    fn hide(v: &Value) -> Value {
        match v {
            Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), if k.contains("password") { json!("********") } else { hide(v) })).collect()),
            Value::Array(a) => Value::Array(a.iter().map(hide).collect()),
            other => other.clone(),
        }
    }
    let text = args
        .iter()
        .map(|a| match a {
            Value::String(s) if s.starts_with("OpaqueRef:") => s.clone(),
            other => hide(other).to_string(),
        })
        .collect::<Vec<_>>()
        .join("  ");
    if text.chars().count() > 400 {
        format!("{}…", text.chars().take(400).collect::<String>())
    } else {
        text
    }
}
/// A login's error code when the pool's servers point to a coordinator that doesn't answer; its
/// address is the detail.
pub const COORDINATOR_DOWN: &str = "COORDINATOR_DOWN";
/// How long a storage has to answer a check (a rescan takes a second or two).
pub const CHECK_SECS: u64 = 15;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub code: String,
    pub details: Vec<String>,
}

impl ApiError {
    pub fn other(msg: impl Into<String>) -> ApiError {
        ApiError { code: "CONNECTION".into(), details: vec![msg.into()] }
    }
    pub fn message(&self) -> String {
        match self.code.as_str() {
            "SESSION_AUTHENTICATION_FAILED" => "The user name or password is wrong.".into(),
            "HOST_IS_SLAVE" => "That server is a pool member; connecting to its pool coordinator.".into(),
            "SESSION_INVALID" => "The session expired.".into(),
            "RBAC_PERMISSION_DENIED" => "Your account isn't allowed to do that.".into(),
            "VM_BAD_POWER_STATE" => "The VM isn't in the right state for that.".into(),
            "VM_MISSING_PV_DRIVERS" => "The VM needs its guest tools installed for a clean shut down or suspend.".into(),
            "HOST_NOT_ENOUGH_FREE_MEMORY" => "No server has enough free memory to start the VM.".into(),
            "CONNECTION" => self.details.join(" "),
            LOST_CONTACT => format!(
                "{} stopped answering for {} seconds (it may have lost power), so this was given up on. When it's back, check what's there: the app offers to delete anything it left half made.",
                self.details.first().map(String::as_str).unwrap_or("The server"),
                LOST_AFTER.as_secs()
            ),
            COORDINATOR_DOWN => format!(
                "The pool's coordinator ({}) isn't answering, and its other servers can't be managed without it. Turn it back on. If it's gone for good, make another server the coordinator: on that server's console, run xe pool-emergency-transition-to-master.",
                self.details.first().map(String::as_str).unwrap_or("its main server")
            ),
            _ => {
                // the server often leaves some of the details empty
                let details: Vec<&str> = self.details.iter().map(|d| d.trim()).filter(|d| !d.is_empty()).collect();
                if details.is_empty() {
                    format!("The server said: {}", self.code)
                } else {
                    format!("The server said: {} ({})", self.code, details.join(", "))
                }
            }
        }
    }
}

pub trait Connection: Send + Sync {
    fn hostname(&self) -> String;
    fn username(&self) -> String;
    fn is_demo(&self) -> bool;
    fn fetch(&self, timeout_ms: u64) -> Result<PoolData, ApiError>;
    fn run_action(&self, vm_ref: &str, action: &VmAction) -> Result<(), ApiError>;
    /// A stream to the VM's screen, ready for the VNC (RFB) handshake (see console.rs).
    fn open_console(&self, vm_ref: &str) -> Result<ConsoleStream, ApiError>;
    /// Creates an ISO library; the server mounts the share right away, so a wrong address or
    /// password fails here, with the mount's own message in the error's details.
    fn create_iso_library(&self, lib: &IsoLibrary) -> Result<(), ApiError>;
    /// Looks at the storage again for files added or removed (SR.scan).
    fn scan_sr(&self, sr_ref: &str) -> Result<(), ApiError>;
    /// Unplugs the storage from every server and forgets it; nothing on it is deleted.
    fn forget_sr(&self, sr_ref: &str) -> Result<(), ApiError>;
    /// Mounts a storage again where it isn't (XenCenter's Repair): shared storage first gets a
    /// connection on each server of the pool that has none (the same settings as the others),
    /// then each unplugged connection on a server that's on is plugged, the coordinator's first.
    /// If the coordinator's fails, the rest aren't tried (they'd wait as long for the same share).
    /// Each server tried, by name, and how it went.
    fn repair_sr(&self, sr_ref: &str) -> Result<Vec<(String, Result<(), ApiError>)>, ApiError>;
    /// Gives a library the pool has no settings for any more (detached) new ones, on every server
    /// of the pool for shared storage (otherwise on the coordinator), and mounts it, keeping its
    /// discs and their descriptions. If the coordinator can't mount it, nothing is kept.
    fn reattach_sr(&self, sr_ref: &str, lib: &IsoLibrary) -> Result<(), ApiError>;
    /// Whether a storage answers: plugged into `host` (any server, for None), then rescanned
    /// within CHECK_SECS. A share that's gone away doesn't fail, the server waits for it for
    /// ever: that's NOT_ANSWERING. Each call goes in `trace`, named from `names`.
    fn check_sr(&self, sr_ref: &str, host: Option<&str>, names: &HashMap<String, String>, trace: &Trace) -> Result<(), ApiError>;
    /// Makes a VM the way XenCenter does: a copy of the template, made into a VM, its CPUs,
    /// memory and boot mode set, then its disk, a DVD drive with the ISO and a network card; then
    /// started, if asked. Anything made before a failure is taken away again. A VM that's made but
    /// won't start stays, with the reason.
    /// Makes the VM (not started), each call in `trace`; its reference.
    fn create_vm(&self, spec: &NewVm, trace: &Trace) -> Result<String, ApiError>;
    /// Starts a VM, in `trace` as `label`, waiting as long as the server takes.
    fn start_traced(&self, vm: &str, label: &str, trace: &Trace) -> Result<(), ApiError>;
    /// Deletes an ISO file: its disc, and the file on the share. Shut-down VMs that have it in a
    /// DVD drive get the drive emptied first, as destroying a disc also destroys the drives it's in.
    fn delete_iso(&self, vdi_ref: &str) -> Result<(), ApiError>;
    /// Shuts a server down the way the pool expects: no new VMs sent to it, the VMs that can't
    /// leave it (disks on its own storage, no other server) shut down (the hard way if a clean
    /// shut down fails, as for a VM without guest tools), the rest moved to the pool's other
    /// servers, then the server itself. If any step fails it's enabled again.
    fn shutdown_host(&self, host_ref: &str) -> Result<(), ApiError>;
    /// Restarts a server, as `shutdown_host` (its VMs stopped or moved) but restarting it; it
    /// takes new VMs again once it's back.
    fn reboot_host(&self, host_ref: &str) -> Result<(), ApiError>;
    /// Deletes a VM: turned off first if it isn't (forced), its snapshots, then the VM; with
    /// `disks`, then the disks only it and its snapshots use (a disk another VM has stays). DVD
    /// drives just go, the ISO files in them untouched. The disks that couldn't be deleted (the
    /// VM is gone by then): each one's name and why.
    fn delete_vm(&self, vm_ref: &str, disks: bool) -> Result<Vec<String>, ApiError>;
    /// Changes a disc's description (an ISO file's, in an ISO library).
    fn set_vdi_description(&self, vdi_ref: &str, text: &str) -> Result<(), ApiError>;
    /// Changes a VM's (or a snapshot's) description.
    fn set_vm_description(&self, vm_ref: &str, text: &str) -> Result<(), ApiError>;
    /// Takes a snapshot of a VM, named `name`: its disks, or with `memory` (a running VM) its disks
    /// and memory too (a checkpoint: the VM pauses while its memory is written out). Its reference.
    fn snapshot_vm(&self, vm_ref: &str, name: &str, description: &str, memory: bool) -> Result<String, ApiError>;
    /// Puts a VM back as it was at a snapshot: turned off first (the hard way) if it's on, then
    /// put back, then as it was then: resumed for a snapshot with memory, started if it was
    /// running, otherwise left off. Put back but not started again is NOT_RESTARTED, the reason in
    /// its details.
    fn revert_vm(&self, snapshot_ref: &str) -> Result<(), ApiError>;
    /// Makes a template from a VM that's off (a copy of it: the VM stays as it is) or from one of
    /// its snapshots, named `name`: its disks copied in an instant on their own storage. Its
    /// reference.
    fn make_template(&self, source_ref: &str, name: &str, description: &str) -> Result<String, ApiError>;
    /// Copies a VM that's off, named `name`: in an instant on its own storage (`sr` None), or in
    /// full onto `sr`. Its reference.
    fn copy_vm(&self, vm_ref: &str, name: &str, sr: Option<&str>) -> Result<String, ApiError>;
    /// Changes a VM (see `VmChange`): what can be done while it runs is; a disk or card added to a
    /// running VM is plugged in when it can be (else at its next start).
    fn edit_vm(&self, vm_ref: &str, change: &VmChange) -> Result<(), ApiError>;
    /// The last ten minutes of a running VM's load, from the server it runs on (its address).
    fn vm_perf(&self, vm_uuid: &str, host_address: &str) -> Result<Perf, ApiError>;
    /// The last ten minutes of a server's load and its running VMs', from the server itself (its
    /// uuid and address).
    fn host_perf(&self, host_uuid: &str, host_address: &str) -> Result<HostPerf, ApiError>;
    /// The updates the pool's servers lack, as last fetched from its update channel (its
    /// /updates answer).
    fn pool_updates(&self) -> Result<PoolUpdates, ApiError>;
    /// Fetches the latest updates from the pool's update channel (pool.sync_updates): the hash to
    /// install them by.
    fn sync_updates(&self) -> Result<String, ApiError>;
    /// Why the pool can't be updated now (pool.check_update_readiness); none when it can.
    fn update_readiness(&self) -> Result<Vec<String>, ApiError>;
    /// Installs the updates last fetched (by their `hash`) on a server; what it said about them
    /// (livepatches that failed…).
    fn apply_updates(&self, host_ref: &str, hash: &str) -> Result<Vec<String>, ApiError>;
    /// What a server must still do for the updates installed on it (its pending guidances, the
    /// recommended ones too: "reboot_host", "restart_toolstack"…).
    fn pending_guidance(&self, host_ref: &str) -> Result<Vec<String>, ApiError>;
    /// The running VMs that can't move off a server, and why (host.get_vms_which_prevent_evacuation:
    /// an error code and its details, like "VM_REQUIRES_SR").
    fn vms_blocking_evacuation(&self, host_ref: &str) -> Result<Vec<(String, String)>, ApiError>;
    /// Empties a server: no new VMs sent to it, the VMs that can't move stopped (with `stop`; without,
    /// it fails before anything when there are any), the rest moved to the other servers. The VMs
    /// it stopped, and how many it moved. (Taking VMs again if a step fails.)
    fn empty_host(&self, host_ref: &str, stop: bool) -> Result<(Vec<String>, usize), ApiError>;
    /// Restarts a server (emptied first) and waits, up to `limit`, till it answers again.
    fn restart_host_and_wait(&self, host_ref: &str, limit: Duration) -> Result<(), ApiError>;
    /// Restarts a server's toolstack and waits till it answers again.
    fn restart_toolstack(&self, host_ref: &str) -> Result<(), ApiError>;
    /// The server takes VMs again: asked till it does (one that's just restarted may not at
    /// once), for up to five minutes.
    fn enable_host(&self, host_ref: &str) -> Result<(), ApiError>;
    /// Starts a VM on a server.
    fn start_vm_on(&self, vm_ref: &str, host_ref: &str) -> Result<(), ApiError>;
    /// Deletes a snapshot and its own copies of the disks (and its saved memory). The ones that
    /// couldn't be deleted (the snapshot is gone by then): each one's name and why.
    fn delete_snapshot(&self, snapshot_ref: &str) -> Result<Vec<String>, ApiError>;
    /// Copies a file into an ISO library as `name` (see `iso_file_name`): a new disc in the library,
    /// then the file's bytes sent to it, from this computer or as they arrive from the web.
    /// `address` is the server to send them to for a library that only one server sees; a shared
    /// one takes them on the pool coordinator. A failed or cancelled upload leaves nothing behind.
    fn upload_iso(&self, sr_ref: &str, address: Option<&str>, src: &Source, name: &str, t: &Arc<Transfer>) -> Result<(), ApiError>;
    fn close(&self);
}

/// What a job asked the server and what it answered, line by line, as it happens: the New VM
/// dialog shows it in a terminal-like box. Shared with the thread doing the job.
#[derive(Default)]
pub struct Trace {
    lines: Mutex<Vec<(Said, String)>>,
    /// When the last line came.
    last: Mutex<Option<Instant>>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Said {
    /// A call: its method and arguments.
    Asked,
    /// The server's answer to it.
    Answer,
    /// An error: its code, then its details.
    Error,
    /// The app's own remark ("carrying on", "undoing…").
    Note,
}

impl Trace {
    pub fn push(&self, k: Said, text: impl Into<String>) {
        self.lines.lock().unwrap().push((k, text.into()));
        *self.last.lock().unwrap() = Some(Instant::now());
    }
    pub fn lines(&self) -> Vec<(Said, String)> {
        self.lines.lock().unwrap().clone()
    }
    /// How long the last call has gone unanswered (None when it has its answer).
    pub fn waiting(&self) -> Option<Duration> {
        let asked = self.lines.lock().unwrap().last().map_or(false, |(k, _)| *k == Said::Asked);
        if asked {
            self.last.lock().unwrap().map(|t| t.elapsed())
        } else {
            None
        }
    }
    /// The whole of it to copy: each call after "> ", the answers under it, indented.
    pub fn text(&self) -> String {
        self.lines().iter().map(|(k, l)| if *k == Said::Asked { format!("> {}", l) } else { format!("  {}", l) }).collect::<Vec<_>>().join("\n")
    }
    /// A call's outcome: its answer (OK, a reference, a value) and how long it took, or its error.
    fn outcome(&self, r: &Result<Value, ApiError>, took: Duration) {
        match r {
            Ok(v) => {
                let what = match v {
                    Value::Null => "OK".to_string(),
                    Value::String(s) if s.is_empty() => "OK".to_string(),
                    Value::String(s) => s.clone(),
                    Value::Array(a) => format!("{} item{}", a.len(), if a.len() == 1 { "" } else { "s" }),
                    Value::Object(_) => "OK (a record)".to_string(),
                    other => other.to_string(),
                };
                self.push(Said::Answer, format!("{}  ({:.1} s)", what, took.as_secs_f32()));
            }
            Err(e) => {
                self.push(Said::Error, format!("Error code: {}", e.code));
                for d in e.details.iter().filter(|d| !d.trim().is_empty()) {
                    self.push(Said::Error, d.trim());
                }
            }
        }
    }
}

/// How the trace names a reference: by what it's called, if known, else as it is.
fn named(names: &HashMap<String, String>, r: &str) -> String {
    names.get(r).map(|n| format!("\"{}\"", n)).unwrap_or_else(|| r.to_string())
}

/// The value of a template recommendation (<restriction field="…" value="…"/>), if it's there.
fn restriction(xml: &str, field: &str) -> Option<String> {
    let at = xml.find(&format!("field=\"{}\"", field))?;
    let start = xml[..at].rfind('<')?;
    let end = at + xml[at..].find('>')?;
    let tag = &xml[start..end];
    let v = tag.find("value=\"")? + 7;
    Some(tag[v..].split('"').next()?.to_string())
}

/// The size of the first disk in a template's provisioning list (<disk … size="…"/>).
fn provision_size(xml: &str) -> i64 {
    xml.find("size=\"").and_then(|i| xml[i + 6..].split('"').next()?.parse().ok()).unwrap_or(0)
}

/// Where an upload's bytes come from.
#[derive(Clone, Debug)]
pub enum Source {
    File(PathBuf),
    /// A link on the web, and the size it said it had when checked.
    Web { url: String, size: u64 },
}

/// What a link on the web leads to.
#[derive(Clone, Debug, PartialEq)]
pub struct WebFile {
    pub name: String,
    pub size: u64,
}

/// An agent for the web, checking certificates as a browser does (unlike the one for the
/// servers, whose certificates are usually their own).
pub(crate) fn web_agent() -> Result<ureq::Agent, String> {
    let tls = native_tls::TlsConnector::new().map_err(|e| e.to_string())?;
    Ok(ureq::AgentBuilder::new()
        .tls_connector(Arc::new(tls))
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(60))
        .redirects(8)
        .user_agent(concat!("SimpleVMManager/", env!("CARGO_PKG_VERSION")))
        .build())
}

/// Starts downloading a link: the answer, its body not read yet.
pub(crate) fn web_get(url: &str) -> Result<ureq::Response, String> {
    match web_agent()?.get(url.trim()).call() {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(404, _)) => Err("There's no file at that link (the site says 404, not found).".into()),
        Err(ureq::Error::Status(code @ (401 | 403), _)) => Err(format!("The site won't send that file (HTTP {}).", code)),
        Err(ureq::Error::Status(code, _)) => Err(format!("The site answered with HTTP {}.", code)),
        Err(e) => Err(match e.kind() {
            ureq::ErrorKind::Dns => "Couldn't find that site: check the address.".into(),
            ureq::ErrorKind::ConnectionFailed => "Couldn't connect to that site.".into(),
            ureq::ErrorKind::InvalidUrl | ureq::ErrorKind::UnknownScheme => "That isn't a valid link.".into(),
            _ => e.to_string(),
        }),
    }
}

/// Checks a link before downloading it: its file's name and size. A site that doesn't tell the
/// size can't be used, as the library needs it before the first byte.
pub fn probe_url(url: &str) -> Result<WebFile, String> {
    let r = web_get(url)?;
    if r.content_type().contains("html") {
        return Err("That link opens a web page, not a file. Use the link of the .iso itself (where the page's Download button points).".into());
    }
    let size = r.header("Content-Length").and_then(|v| v.trim().parse::<u64>().ok()).filter(|n| *n > 0);
    let size = size.ok_or("The site doesn't say how big the file is, so it can't go straight into the library. Download it to this computer, then use Upload local file.")?;
    let name = disposition_name(r.header("Content-Disposition").unwrap_or("")).or_else(|| url_file_name(r.get_url())).unwrap_or_else(|| "download.iso".into());
    Ok(WebFile { name, size })
}

/// The file name in a Content-Disposition header (filename*=UTF-8''… or filename="…").
fn disposition_name(h: &str) -> Option<String> {
    let mut plain = None;
    for part in h.split(';').map(str::trim) {
        if let Some(v) = part.strip_prefix("filename*=") {
            let v = v.rsplit('\'').next().unwrap_or(v);
            return Some(percent_decode(v.trim_matches('"'))).filter(|n| !n.is_empty());
        }
        if let Some(v) = part.strip_prefix("filename=") {
            plain = Some(v.trim_matches('"').to_string()).filter(|n| !n.is_empty());
        }
    }
    plain
}

/// The last part of a link's path ("…/ubuntu-24.04.iso?x=1" → "ubuntu-24.04.iso").
fn url_file_name(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let after_host = path.splitn(4, '/').nth(3).unwrap_or("");
    after_host.rsplit('/').next().map(percent_decode).filter(|n| !n.is_empty())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// An upload under way, shared by its worker and the window.
#[derive(Default)]
pub struct Transfer {
    pub sent: AtomicU64,
    pub total: AtomicU64,
    /// Every byte is sent; the server is finishing the file.
    pub finishing: AtomicBool,
    /// Set by the window to stop it.
    pub cancel: AtomicBool,
}

/// The code of the error an upload ends with when it's cancelled.
pub const CANCELLED: &str = "UPLOAD_CANCELLED";

/// Reads what's being uploaded, counting what's been read, and stops when it's cancelled.
pub(crate) struct Counting {
    pub(crate) inner: Box<dyn Read + Send>,
    pub(crate) t: Arc<Transfer>,
}

impl Read for Counting {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.t.cancel.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.t.sent.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// The connection to a VM's screen: TLS to a real server, plain TCP for a local test VNC server.
pub enum ConsoleStream {
    Tls(Box<native_tls::TlsStream<TcpStream>>),
    Plain(TcpStream),
}

impl ConsoleStream {
    pub fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        match self {
            ConsoleStream::Tls(s) => s.get_ref().set_read_timeout(d),
            ConsoleStream::Plain(s) => s.set_read_timeout(d),
        }
    }
}

impl Read for ConsoleStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ConsoleStream::Tls(s) => s.read(buf),
            ConsoleStream::Plain(s) => s.read(buf),
        }
    }
}

impl Write for ConsoleStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            ConsoleStream::Tls(s) => s.write(buf),
            ConsoleStream::Plain(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            ConsoleStream::Tls(s) => s.flush(),
            ConsoleStream::Plain(s) => s.flush(),
        }
    }
}

/// For trying the Console tab without a real server: with SVM_DEBUG_CONSOLE=host:port the demo
/// server's VMs show that plain VNC server instead of their pretend console (for example
/// `qemu-system-x86_64 -vnc :1` listens on 127.0.0.1:5901).
pub fn debug_console_address() -> Option<String> {
    std::env::var("SVM_DEBUG_CONSOLE").ok().filter(|a| !a.trim().is_empty())
}

/// "host", "host:port", "[v6 address]:port" or a bare IPv6 address.
fn split_host_port(a: &str) -> (String, Option<u16>) {
    if let Some(rest) = a.strip_prefix('[') {
        if let Some((h, p)) = rest.split_once(']') {
            return (h.to_string(), p.strip_prefix(':').and_then(|p| p.parse().ok()));
        }
    }
    match a.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h.to_string(), p.parse().ok()),
        _ => (a.to_string(), None),
    }
}

/// Whether something answers on that address and port within two seconds.
fn knock(host: &str, port: u16) -> bool {
    let Ok(addrs) = (host, port).to_socket_addrs() else { return false };
    addrs.into_iter().any(|a| TcpStream::connect_timeout(&a, Duration::from_secs(2)).is_ok())
}

fn tcp_connect(host: &str, port: u16) -> Result<TcpStream, ApiError> {
    let addrs = (host, port).to_socket_addrs().map_err(|e| ApiError::other(format!("Couldn't find {}: {}", host, e)))?;
    let mut last = String::from("no address");
    for a in addrs {
        match TcpStream::connect_timeout(&a, Duration::from_secs(10)) {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(ApiError::other(format!("Couldn't reach {} for the console ({}).", host, last)))
}

// ---------------------------------------------------------------- real servers

struct RealState {
    host: String,
    session: Option<String>,
    /// The pool's servers' addresses, from the last look at it: where to log in when `host`
    /// doesn't answer.
    members: Vec<String>,
}

pub struct RealConnection {
    port: u16,
    user: String,
    password: String,
    /// Only ever held for a moment (the window asks it for the server's name as it draws), never
    /// while waiting on the network.
    state: Mutex<RealState>,
    /// Held while logging in, so only one login happens at a time.
    login: Mutex<()>,
    id: AtomicU64,
    tls: Arc<native_tls::TlsConnector>,
    /// How many looks in a row each server (by reference) hasn't answered (see `knock_hosts`).
    silent: Mutex<HashMap<String, u32>>,
}

/// A server is counted as not answering after this many looks in a row (a look every 15 seconds)
/// without an answer, so a moment's hiccup on the network doesn't turn it red.
const SILENT_LOOKS: u32 = 2;

impl RealConnection {
    pub fn new(host_and_port: &str, user: &str, password: &str) -> RealConnection {
        let mut host = host_and_port.trim().to_string();
        let mut port = DEFAULT_PORT;
        if let Some(i) = host.rfind(':') {
            if host.find(':') == Some(i) {
                if let Ok(p) = host[i + 1..].parse::<u16>() {
                    port = p;
                    host.truncate(i);
                }
            }
        }
        // Certificates are accepted without the original's prompt in this simpler version.
        let tls = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .expect("TLS is available");
        RealConnection {
            port,
            user: user.into(),
            password: password.into(),
            state: Mutex::new(RealState { host, session: None, members: vec![] }),
            login: Mutex::new(()),
            id: AtomicU64::new(0),
            tls: Arc::new(tls),
            silent: Mutex::new(HashMap::new()),
        }
    }

    /// New VM from one of the user's templates: its disks copied (in an instant on their own
    /// storage, in full onto `copy_to`), made a VM, then its CPUs, memory, home server and
    /// network card as chosen; the ISO in its DVD drive, if any, taken out. Anything made before a
    /// failure is taken away again.
    fn from_template(&self, v: &NewVm, tr: &Trace) -> Result<String, ApiError> {
        let t = 120_000;
        let call = |shown: String, method: &str, args: Vec<Value>, timeout: u64| {
            tr.push(Said::Asked, shown);
            let started = Instant::now();
            let r = self.call(method, args, timeout);
            tr.outcome(&r, started.elapsed());
            r
        };
        let mut names = v.names.clone();
        let tpl = named(&names, &v.template);
        let vm = match &v.copy_to {
            None => call(format!("VM.clone  vm={}  new_name=\"{}\"", tpl, v.name), "VM.clone", vec![json!(v.template), json!(v.name)], 1_800_000)?,
            Some(sr) => {
                let shown = format!("VM.copy  vm={}  new_name=\"{}\"  sr={}", tpl, v.name, named(&names, sr));
                call(shown, "VM.copy", vec![json!(v.template), json!(v.name), json!(sr)], 6 * 3_600_000)?
            }
        };
        let vm = vm.as_str().unwrap_or_default().to_string();
        names.insert(vm.clone(), v.name.clone());
        let me = named(&names, &vm);
        let made = (|| -> Result<(), ApiError> {
            call(format!("VM.set_is_a_template  vm={}  value=false", me), "VM.set_is_a_template", vec![json!(vm), json!(false)], t)?;
            call(format!("VM.set_name_description  vm={}  value=\"{}\"", me, v.description), "VM.set_name_description", vec![json!(vm), json!(v.description)], t)?;
            let max = n(&call(format!("VM.get_record  vm={}", me), "VM.get_record", vec![json!(vm)], t)?, "VCPUs_max");
            let order = if v.vcpus > max { ["VM.set_VCPUs_max", "VM.set_VCPUs_at_startup"] } else { ["VM.set_VCPUs_at_startup", "VM.set_VCPUs_max"] };
            for m in order {
                call(format!("{}  vm={}  value={}", m, me, v.vcpus), m, vec![json!(vm), json!(v.vcpus)], t)?;
            }
            let _ = call(format!("VM.remove_from_platform  vm={}  key=\"cores-per-socket\"", me), "VM.remove_from_platform", vec![json!(vm), json!("cores-per-socket")], t);
            let shown = format!("VM.add_to_platform  vm={}  key=\"cores-per-socket\"  value=\"{}\"", me, v.cores_per_socket);
            call(shown, "VM.add_to_platform", vec![json!(vm), json!("cores-per-socket"), json!(v.cores_per_socket.to_string())], t)?;
            let floor = n(&call(format!("VM.get_record  vm={}", me), "VM.get_record", vec![json!(vm)], t)?, "memory_static_min").min(v.memory);
            let shown = format!("VM.set_memory_limits  vm={}  static_min={}  dynamic_min={}  dynamic_max={}  static_max={}", me, floor, v.memory, v.memory, v.memory);
            call(shown, "VM.set_memory_limits", vec![json!(vm), json!(floor), json!(v.memory), json!(v.memory), json!(v.memory)], t)?;
            if let Some(h) = &v.host {
                call(format!("VM.set_affinity  vm={}  host={}", me, named(&names, h)), "VM.set_affinity", vec![json!(vm), json!(h)], t)?;
            }
            // an ISO left in its DVD drive comes out (it'd start the installer again)
            for vbd in call(format!("VM.get_VBDs  vm={}", me), "VM.get_VBDs", vec![json!(vm)], t)?.as_array().cloned().unwrap_or_default() {
                let id = vbd.as_str().unwrap_or_default().to_string();
                let r = call(format!("VBD.get_record  vbd={}", id), "VBD.get_record", vec![vbd.clone()], t)?;
                if s(&r, "type") == "CD" && !b(&r, "empty") {
                    call(format!("VBD.eject  vbd={}", id), "VBD.eject", vec![vbd], t)?;
                }
            }
            if !v.keep_nics {
                for vif in call(format!("VM.get_VIFs  vm={}", me), "VM.get_VIFs", vec![json!(vm)], t)?.as_array().cloned().unwrap_or_default() {
                    call(format!("VIF.destroy  vif={}", vif.as_str().unwrap_or_default()), "VIF.destroy", vec![vif], t)?;
                }
                if let Some(net) = &v.network {
                    let mtu = call(format!("network.get_MTU  network={}", named(&names, net)), "network.get_MTU", vec![json!(net)], t).map(|m| n(&json!({ "m": m }), "m")).unwrap_or(1500);
                    let card = json!({
                        "device": "0", "network": net, "VM": vm, "MAC": "", "MTU": mtu.max(68), "other_config": {},
                        "qos_algorithm_type": "", "qos_algorithm_params": {}, "locking_mode": "network_default",
                        "ipv4_allowed": [], "ipv6_allowed": [], "currently_attached": false,
                    });
                    call(format!("VIF.create  VM={}  network={}  device=0  MTU={}", me, named(&names, net), mtu.max(68)), "VIF.create", vec![card], t)?;
                }
            }
            Ok(())
        })();
        if let Err(e) = made {
            tr.push(Said::Note, "Undoing what was made:");
            tr.push(Said::Asked, format!("VM.destroy  vm={}  (with its copies of the disks)", me));
            let _ = self.delete_vm(&vm, true);
            return Err(e);
        }
        Ok(vm)
    }

    /// Knocks on each server's management address (all at once, two seconds each at most), as the
    /// pool counts one that goes down without shutting down as live for ten minutes. A server
    /// that the pool calls live but that hasn't answered `SILENT_LOOKS` times in a row is marked
    /// unreachable, but only while the coordinator's address answers: otherwise this computer
    /// just can't reach the servers directly, and the pool's word is all there is.
    fn knock_hosts(&self, hosts: &mut [HostInfo]) {
        if hosts.len() < 2 {
            return; // a lone server that's down can't be looked at at all
        }
        let port = self.port;
        let answers: Vec<bool> = std::thread::scope(|sc| {
            let knocks: Vec<_> = hosts.iter().map(|h| h.address.clone()).map(|a| sc.spawn(move || a.is_empty() || knock(&a, port))).collect();
            knocks.into_iter().map(|k| k.join().unwrap_or(true)).collect()
        });
        if !hosts.iter().zip(&answers).any(|(h, a)| h.is_master && *a) {
            return;
        }
        let mut silent = self.silent.lock().unwrap();
        for (h, answered) in hosts.iter_mut().zip(answers) {
            let n = silent.entry(h.rref.clone()).or_insert(0);
            *n = if answered { 0 } else { *n + 1 };
            h.unreachable = h.live && *n >= SILENT_LOOKS;
        }
    }

    /// The pool's servers' addresses, remembered from last time: tried when the address given
    /// doesn't answer.
    pub fn with_members(self, members: Vec<String>) -> RealConnection {
        self.state.lock().unwrap().members = members;
        self
    }

    fn url(&self, host: &str) -> String {
        if self.port == DEFAULT_PORT {
            format!("https://{}/jsonrpc", host)
        } else {
            format!("https://{}:{}/jsonrpc", host, self.port)
        }
    }

    fn rpc(&self, host: &str, id: u64, method: &str, params: Vec<Value>, timeout_ms: u64) -> Result<Value, ApiError> {
        let agent = ureq::AgentBuilder::new()
            .tls_connector(self.tls.clone())
            .timeout_connect(Duration::from_millis(CONNECT_TIMEOUT_MS.min(timeout_ms)))
            .timeout(Duration::from_millis(timeout_ms))
            .user_agent(concat!("SimpleVMManager/", env!("CARGO_PKG_VERSION")))
            .build();
        let body = json!({"jsonrpc": "2.0", "method": method, "params": params, "id": id}).to_string();
        let text = match agent.post(&self.url(host)).set("Content-Type", "application/json").send_string(&body) {
            Ok(r) => r.into_string().map_err(|e| ApiError::other(e.to_string()))?,
            Err(ureq::Error::Status(code, r)) => {
                let t = r.into_string().unwrap_or_default();
                if !t.trim_start().starts_with('{') {
                    return Err(ApiError::other(format!("The server answered with HTTP {}.", code)));
                }
                t
            }
            Err(e) => return Err(ApiError::other(e.to_string())),
        };
        let v: Value = serde_json::from_str(&text).map_err(|_| ApiError::other("The server sent an answer this app doesn't understand."))?;
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            return Err(to_error(err));
        }
        let result = v.get("result").cloned().unwrap_or(Value::Null);
        // Older answer shape: {"Status":"Failure","ErrorDescription":[...]}
        if let Some(st) = result.get("Status").and_then(|s| s.as_str()) {
            if st == "Failure" {
                return Err(to_error(result.get("ErrorDescription").unwrap_or(&Value::Null)));
            }
            if st == "Success" {
                return Ok(result.get("Value").cloned().unwrap_or(Value::Null));
            }
        }
        Ok(result)
    }

    /// Logs in: at the address given, else at the pool's other servers; a pool member points to
    /// its coordinator, which is tried next. The server and session it got, or why not. Holds no
    /// lock on `state` while it waits.
    fn log_in(&self, timeout_ms: u64) -> Result<(String, String), ApiError> {
        let (first, members) = {
            let st = self.state.lock().unwrap();
            (st.host.clone(), st.members.clone())
        };
        let mut queue: std::collections::VecDeque<String> = std::iter::once(first.clone()).chain(members).collect();
        let mut tried: Vec<String> = vec![];
        let mut unreachable: Vec<String> = vec![];
        // the coordinators the pool's servers pointed to
        let mut pointed: Vec<String> = vec![];
        let mut last = ApiError::other("Couldn't log in.");
        while let Some(host) = queue.pop_front() {
            if host.is_empty() || tried.iter().any(|t| t.eq_ignore_ascii_case(&host)) {
                continue;
            }
            if tried.len() >= MAX_LOGIN_ATTEMPTS {
                break;
            }
            tried.push(host.clone());
            let id = self.id.fetch_add(1, Ordering::Relaxed) + 1;
            let params = vec![json!(self.user), json!(self.password), json!("2.0"), json!("SimpleVMManager")];
            match self.rpc(&host, id, "session.login_with_password", params, timeout_ms) {
                Ok(v) => return Ok((host, v.as_str().unwrap_or_default().to_string())),
                Err(e) if e.code == "HOST_IS_SLAVE" && !e.details.is_empty() => {
                    let coordinator = e.details[0].clone();
                    if !pointed.contains(&coordinator) {
                        pointed.push(coordinator.clone());
                    }
                    queue.push_front(coordinator);
                    last = e;
                }
                Err(e) if e.code == "CONNECTION" => {
                    unreachable.push(host);
                    last = e;
                }
                Err(e) => return Err(e), // wrong password etc.: no point trying elsewhere
            }
        }
        if let Some(c) = pointed.iter().find(|c| unreachable.iter().any(|u| u.eq_ignore_ascii_case(c))) {
            return Err(ApiError { code: COORDINATOR_DOWN.into(), details: vec![c.clone()] });
        }
        if last.code == "CONNECTION" {
            let what = if tried.len() > 1 { format!("{} (nor the pool's other servers)", first) } else { first };
            return Err(ApiError::other(format!("{} isn't answering. It may be off, or out of reach from this computer.", what)));
        }
        Err(last)
    }

    /// A session to call with, and the server it's on: the one there is, or a new login (one at a
    /// time; a login that another call just made is used as it is).
    fn session(&self, timeout_ms: u64) -> Result<(String, String), ApiError> {
        let current = || {
            let st = self.state.lock().unwrap();
            st.session.clone().map(|s| (st.host.clone(), s))
        };
        if let Some(x) = current() {
            return Ok(x);
        }
        let _one = self.login.lock().unwrap();
        if let Some(x) = current() {
            return Ok(x);
        }
        let (host, session) = self.log_in(timeout_ms)?;
        let mut st = self.state.lock().unwrap();
        st.host = host.clone();
        st.session = Some(session.clone());
        Ok((host, session))
    }

    /// Forgets `session` (expired, or its server stopped answering), unless another call already
    /// replaced it: the next call logs in again.
    fn drop_session(&self, session: &str) {
        let mut st = self.state.lock().unwrap();
        if st.session.as_deref() == Some(session) {
            st.session = None;
        }
    }

    /// Waits for a task to end: Ok(true) when it succeeded, Ok(false) when it's still going at the
    /// deadline, and the task's own error when it failed.
    fn task_result(&self, task: &str, limit: Duration) -> Result<bool, ApiError> {
        let end = Instant::now() + limit;
        loop {
            match self.call("task.get_status", vec![json!(task)], 30_000)?.as_str().unwrap_or("") {
                "success" => return Ok(true),
                "failure" => {
                    let info = self.call("task.get_error_info", vec![json!(task)], 30_000)?;
                    let info: Vec<String> = info.as_array().map(|a| a.iter().map(|x| x.as_str().unwrap_or_default().to_string()).collect()).unwrap_or_default();
                    let code = info.first().cloned().unwrap_or_else(|| "UNKNOWN".into());
                    return Err(ApiError { code, details: info.into_iter().skip(1).collect() });
                }
                "cancelled" => return Err(ApiError { code: CANCELLED.into(), details: vec![] }),
                _ => {}
            }
            if Instant::now() >= end {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// A call to the server. One that may take long (allowed over a minute: copying disks,
    /// mounting a share, starting a VM…) runs as a task on the server, looked at every two seconds,
    /// so that a server that goes away part way (a power cut) is noticed within `LOST_AFTER`
    /// instead of being waited on for the whole time; the task's progress goes to the thread's
    /// watched `Progress`.
    /// `shutdown_host` and `reboot_host`: no new VMs sent to it, the VMs that can't leave stopped,
    /// the rest moved, then it goes off or restarts; enabled again if a step fails.
    fn host_off(&self, host: &str, reboot: bool) -> Result<(), ApiError> {
        self.emptied(host, true)?;
        let done = self.call(if reboot { "host.reboot" } else { "host.shutdown" }, vec![json!(host)], ACTION_TIMEOUT_MS).map(|_| ());
        if done.is_err() {
            let _ = self.call("host.enable", vec![json!(host)], 60_000);
        }
        done
    }

    /// The running VMs that can't move off a server, and why.
    fn blocking(&self, host: &str) -> Result<Vec<(String, String)>, ApiError> {
        let t = 60_000;
        let stuck = self.call("host.get_vms_which_prevent_evacuation", vec![json!(host)], t)?;
        let mut out = vec![];
        for (vm, why) in stuck.as_object().cloned().unwrap_or_default() {
            if matches!(self.call("VM.get_power_state", vec![json!(vm)], t)?.as_str(), Some("Running") | Some("Paused")) {
                let why = why.as_array().map(|a| a.iter().map(val_str).collect::<Vec<_>>().join(" ")).unwrap_or_else(|| val_str(&why));
                out.push((vm, why));
            }
        }
        Ok(out)
    }

    /// No new VMs sent to a server, the VMs that can't move elsewhere stopped (with `stop`;
    /// without, it fails first when there are any), the rest moved: the VMs it stopped. Enabled
    /// again if a step fails.
    fn emptied(&self, host: &str, stop: bool) -> Result<(Vec<String>, usize), ApiError> {
        let t = 60_000;
        if !stop {
            let stuck = self.blocking(host)?;
            if !stuck.is_empty() {
                return Err(ApiError { code: "VMS_CANT_MOVE".into(), details: stuck.into_iter().map(|s| s.0).collect() });
            }
        }
        self.call("host.disable", vec![json!(host)], t)?;
        let done = (|| -> Result<(Vec<String>, usize), ApiError> {
            let mut stopped = vec![];
            let stuck = self.call("host.get_vms_which_prevent_evacuation", vec![json!(host)], t)?;
            for vm in stuck.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
                let power = self.call("VM.get_power_state", vec![json!(vm)], t)?;
                let clean = match power.as_str() {
                    Some("Halted") | Some("Suspended") => continue,
                    // (one that couldn't move a moment ago: never stopped when asked not to)
                    _ if !stop => return Err(ApiError { code: "VMS_CANT_MOVE".into(), details: vec![vm] }),
                    // cleanly when its guest tools let it be (asked first, not tried and failed)
                    Some("Running") if self.call_raw("VM.get_allowed_operations", vec![json!(vm)], t).ok().and_then(|v| v.as_array().map(|a| a.iter().any(|o| o.as_str() == Some("clean_shutdown")))).unwrap_or(true) => {
                        self.call("VM.clean_shutdown", vec![json!(vm)], ACTION_TIMEOUT_MS).map(|_| ())
                    }
                    Some("Running") => {
                        note("no guest tools to shut it down cleanly: turned off instead");
                        Err(ApiError::other("no clean shutdown"))
                    }
                    _ => Err(ApiError::other("not running")),
                };
                if clean.is_err() {
                    self.call("VM.hard_shutdown", vec![json!(vm)], ACTION_TIMEOUT_MS)?;
                }
                stopped.push(vm);
            }
            // what's left running there moves (its control domain stays)
            let left = self
                .call("host.get_resident_VMs", vec![json!(host)], t)?
                .as_array()
                .map(|a| a.iter().filter(|vm| !self.call("VM.get_is_control_domain", vec![(*vm).clone()], t).ok().and_then(|x| x.as_bool()).unwrap_or(true)).count())
                .unwrap_or(0);
            self.call("host.evacuate", vec![json!(host)], ACTION_TIMEOUT_MS)?;
            Ok((stopped, left))
        })();
        if done.is_err() {
            let _ = self.call("host.enable", vec![json!(host)], t);
        }
        done
    }

    /// The pool's reference.
    fn pool_ref(&self) -> Result<Value, ApiError> {
        self.call("pool.get_all", vec![], 30_000)?.as_array().and_then(|a| a.first().cloned()).ok_or_else(|| ApiError::other("the server named no pool"))
    }

    fn call(&self, method: &str, extra: Vec<Value>, timeout_ms: u64) -> Result<Value, ApiError> {
        if runs_as_task(method, timeout_ms) {
            return self.as_task(method, extra, timeout_ms);
        }
        self.call_now(method, extra, timeout_ms)
    }

    /// `method` as a task on the server (its Async form), its status looked at every two seconds
    /// till it ends or `timeout_ms` passes; its result (a reference) as the call's would be.
    fn as_task(&self, method: &str, extra: Vec<Value>, timeout_ms: u64) -> Result<Value, ApiError> {
        said(method, &extra);
        let began = Instant::now();
        let r = self.task_watched(method, extra, timeout_ms);
        answered(&r, began.elapsed());
        r
    }

    fn task_watched(&self, method: &str, extra: Vec<Value>, timeout_ms: u64) -> Result<Value, ApiError> {
        let task = self.call_raw(&format!("Async.{}", method), extra, 60_000)?.as_str().unwrap_or_default().to_string();
        let watch = watched();
        let end = Instant::now() + Duration::from_millis(timeout_ms);
        let mut heard = Instant::now();
        loop {
            std::thread::sleep(Duration::from_secs(2));
            match self.call_raw("task.get_record", vec![json!(task)], 20_000) {
                Ok(r) => {
                    heard = Instant::now();
                    if let Some(w) = &watch {
                        w.heard(true);
                        if let Some(f) = r.get("progress").and_then(|p| p.as_f64()) {
                            w.set_fraction(f);
                        }
                    }
                    let done = match s(&r, "status").as_str() {
                        "success" => Some(Ok(task_value(&s(&r, "result")))),
                        "failure" => {
                            let info: Vec<String> = list(&r, "error_info");
                            Some(Err(ApiError { code: info.first().cloned().unwrap_or_else(|| "UNKNOWN".into()), details: info.into_iter().skip(1).collect() }))
                        }
                        "cancelled" => Some(Err(ApiError { code: CANCELLED.into(), details: vec![] })),
                        _ => None,
                    };
                    if let Some(result) = done {
                        let _ = self.call_raw("task.destroy", vec![json!(task)], 20_000);
                        return result;
                    }
                }
                // the server forgot the task: it restarted, and the job with it
                Err(e) if e.code == "HANDLE_INVALID" => return Err(ApiError { code: LOST_CONTACT.into(), details: vec![self.hostname()] }),
                Err(e) => {
                    if let Some(w) = &watch {
                        if w.silent_for().is_none() {
                            w.trace.push(Said::Note, format!("No answer from the server while asking how it's going: {}", e.message()));
                        }
                        w.heard(false);
                    }
                    if heard.elapsed() >= LOST_AFTER {
                        return Err(ApiError { code: LOST_CONTACT.into(), details: vec![self.hostname()] });
                    }
                }
            }
            if Instant::now() >= end {
                return Err(ApiError::other(format!("It's still going on the server after {} minutes (its task {} goes on).", timeout_ms / 60_000, task)));
            }
        }
    }

    /// A call straight away, noted in the watched job's details.
    fn call_now(&self, method: &str, extra: Vec<Value>, timeout_ms: u64) -> Result<Value, ApiError> {
        said(method, &extra);
        let began = Instant::now();
        let r = self.call_raw(method, extra, timeout_ms);
        answered(&r, began.elapsed());
        r
    }

    /// A call straight away, not noted anywhere.
    fn call_raw(&self, method: &str, extra: Vec<Value>, timeout_ms: u64) -> Result<Value, ApiError> {
        let (host, session) = self.session(timeout_ms.min(60_000))?;
        let mut params = vec![json!(session)];
        params.extend(extra.iter().cloned());
        let id = self.id.fetch_add(1, Ordering::Relaxed) + 1;
        match self.rpc(&host, id, method, params, timeout_ms) {
            Err(e) if e.code == "SESSION_INVALID" => {
                self.drop_session(&session);
                let (host, session) = self.session(timeout_ms.min(60_000))?;
                let mut params = vec![json!(session)];
                params.extend(extra);
                let id = self.id.fetch_add(1, Ordering::Relaxed) + 1;
                self.rpc(&host, id, method, params, timeout_ms)
            }
            Err(e) if e.code == "CONNECTION" => {
                // its server may be gone (shut down): the next call logs in again, elsewhere if need be
                self.drop_session(&session);
                Err(e)
            }
            other => other,
        }
    }

    /// The last ten minutes of the records of the server at `address` (the coordinator when
    /// empty), every five seconds: its running VMs', and its own too when `host`; read by `read`,
    /// which says how many points it found. With none, the server's clock may be behind this
    /// computer's: asked again from its own time.
    fn rrd<T>(&self, address: &str, host: bool, read: impl Fn(&str) -> (T, usize)) -> Result<T, ApiError> {
        let (coordinator, session) = self.session(20_000)?;
        let at = if address.is_empty() { coordinator } else { address.to_string() };
        let at = if at.contains(':') && !at.starts_with('[') { format!("[{}]", at) } else { at };
        let port = if self.port == DEFAULT_PORT { String::new() } else { format!(":{}", self.port) };
        let agent = ureq::AgentBuilder::new().tls_connector(self.tls.clone()).timeout_connect(Duration::from_secs(5)).timeout(Duration::from_secs(15)).build();
        let get = |start: i64| -> Result<String, ApiError> {
            let url = format!("https://{}{}/rrd_updates?session_id={}&start={}&cf=AVERAGE&interval=5&host={}", at, port, session, start, host);
            agent.get(&url).call().map_err(web_error)?.into_string().map_err(|e| ApiError::other(e.to_string()))
        };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let mut text = get(now - 600)?;
        let (mut got, mut n) = read(&text);
        if n < 2 {
            if let Some(end) = rrd_end(&text).filter(|e| (*e - now).abs() > 5) {
                text = get(end - 600)?;
                (got, n) = read(&text);
            }
        }
        if n < 2 {
            let head: String = text.trim().chars().take(160).collect();
            return Err(ApiError::other(format!("the server sent no figures for it ({} bytes): {}", text.len(), head)));
        }
        Ok(got)
    }

    fn all(&self, cls: &str, timeout_ms: u64) -> Result<Map<String, Value>, ApiError> {
        Ok(self.call(&format!("{}.get_all_records", cls), vec![], timeout_ms)?.as_object().cloned().unwrap_or_default())
    }
}

/// The calls that can take long and that the server can run as a task (an Async form): only
/// these go as tasks. A field's setter (set_*, add_to_*, remove_from_*) has no Async form: the
/// server answers "No async mode for this operation".
const TASKS: [&str; 34] = [
    "VM.clone", "VM.copy", "VM.provision", "VM.snapshot", "VM.checkpoint", "VM.revert", "VM.start", "VM.start_on", "VM.resume", "VM.resume_on",
    "VM.suspend", "VM.clean_shutdown", "VM.hard_shutdown", "VM.clean_reboot", "VM.hard_reboot", "VM.destroy", "VDI.create", "VDI.destroy",
    "VDI.resize", "VBD.eject", "VBD.insert", "VBD.plug", "VIF.plug", "VIF.unplug", "VIF.move", "PBD.plug", "PBD.unplug", "SR.create", "SR.scan",
    "SR.forget", "host.evacuate", "host.shutdown", "pool.sync_updates", "host.apply_updates",
];

/// Whether a call runs as a task on the server (see `call`): one allowed over a minute that can.
fn runs_as_task(method: &str, timeout_ms: u64) -> bool {
    timeout_ms > 60_000 && TASKS.contains(&method)
}

/// A network the servers keep for themselves, not shown (as XenCenter doesn't): the host internal
/// management network (the guest installer network, on older versions), and any marked
/// HideFromXenCenter.
fn hidden_network(other_config: &Value) -> bool {
    ["is_host_internal_management_network", "is_guest_installer_network", "HideFromXenCenter"].iter().any(|k| s(other_config, k) == "true")
}

/// A task's result: the reference (or value) inside its <value>…</value>, or nothing.
fn task_value(result: &str) -> Value {
    let inner = result.trim().trim_start_matches("<value>").trim_end_matches("</value>").trim();
    if inner.is_empty() {
        Value::Null
    } else {
        json!(inner)
    }
}

fn to_error(err: &Value) -> ApiError {
    let mut code = "UNKNOWN_ERROR".to_string();
    let mut details = vec![];
    if let Some(o) = err.as_object() {
        if let Some(m) = o.get("message").and_then(|m| m.as_str()) {
            code = m.to_string();
        }
        if let Some(d) = o.get("data").and_then(|d| d.as_array()) {
            details = d.iter().map(val_str).collect();
        }
    } else if let Some(a) = err.as_array() {
        if let Some(first) = a.first() {
            code = val_str(first);
        }
        details = a.iter().skip(1).map(val_str).collect();
    } else if !err.is_null() {
        code = val_str(err);
    }
    ApiError { code, details }
}

/// A web request's error in words, never with its address: the address carries the session's id,
/// which is as good as the password while it lasts.
fn web_error(e: ureq::Error) -> ApiError {
    match e {
        ureq::Error::Status(code, r) => ApiError::other(format!("the server answered {} {}", code, r.status_text())),
        ureq::Error::Transport(t) => {
            let mut parts = vec![t.kind().to_string()];
            parts.extend(t.message().map(String::from));
            parts.extend(std::error::Error::source(&t).map(|c| c.to_string()));
            parts.dedup();
            ApiError::other(hide_session(&parts.join(": ")))
        }
    }
}

/// `text` with any session id in it hidden: what follows "session_id=" up to a "&", a space or a
/// ": " (an OpaqueRef has colons of its own).
fn hide_session(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("session_id=") {
        out.push_str(&rest[..i + 11]);
        out.push('…');
        rest = &rest[i + 11..];
        let end = rest.char_indices().find(|&(k, c)| c == '&' || c.is_whitespace() || (c == ':' && rest[k + 1..].starts_with(' '))).map_or(rest.len(), |(k, _)| k);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn or_default(v: &str, default: &str) -> String {
    if v.is_empty() { default.into() } else { v.into() }
}

fn val_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
fn s(o: &Value, k: &str) -> String {
    o.get(k).map(val_str).unwrap_or_default()
}
fn n(o: &Value, k: &str) -> i64 {
    match o.get(k) {
        Some(Value::Number(x)) => x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)).unwrap_or(0),
        Some(Value::String(t)) => t.trim().parse().unwrap_or(0),
        _ => 0,
    }
}
fn b(o: &Value, k: &str) -> bool {
    match o.get(k) {
        Some(Value::Bool(x)) => *x,
        Some(Value::String(t)) => t.eq_ignore_ascii_case("true"),
        _ => false,
    }
}
fn list(o: &Value, k: &str) -> Vec<String> {
    o.get(k).and_then(|v| v.as_array()).map(|a| a.iter().map(val_str).collect()).unwrap_or_default()
}

/// The General tab's details of a VM, from its record, its guest tools' report and its metrics.
fn vm_details(vm: &mut VmInfo, r: &Value, guest: &Value, metrics: &Value) {
    let null = Value::Null;
    let other = r.get("other_config").unwrap_or(&null);
    vm.kernel = s(guest.get("os_version").unwrap_or(&null), "uname");
    // Tools that don't name the operating system: the template the VM was made from does.
    if vm.os.is_empty() {
        vm.os = s(other, "base_template_name");
    }
    vm.tags = list(r, "tags");
    vm.folder = s(other, "folder");
    vm.auto_power_on = s(other, "auto_poweron") == "true";
    let hvm = !s(r, "HVM_boot_policy").is_empty();
    vm.virt_mode = match s(r, "domain_type").as_str() {
        "hvm" => "HVM",
        "pv" => "PV",
        "pvh" => "PVH",
        "pv_in_pvh" => "PV in PVH",
        _ if hvm => "HVM",
        _ => "PV",
    }
    .into();
    if vm.virt_mode == "HVM" {
        let boot = r.get("HVM_boot_params").unwrap_or(&null);
        vm.firmware = if s(boot, "firmware") == "uefi" { "UEFI" } else { "BIOS" }.into();
        vm.boot_order = boot_text(&s(boot, "order"));
    }
    if vm.firmware == "UEFI" {
        vm.secure_boot = match s(r.get("platform").unwrap_or(&null), "secureboot").as_str() {
            "true" => "On",
            "auto" => "Automatic",
            _ => "Off",
        }
        .into();
    }
    vm.ha = match s(r, "ha_restart_priority").as_str() {
        "restart" => "Restart",
        "best-effort" => "Best effort",
        _ => "",
    }
    .into();
    vm.vcpus_max = n(r, "VCPUs_max");
    vm.mem_static_min = n(r, "memory_static_min");
    if matches!(vm.power, PowerState::Running | PowerState::Paused) {
        vm.mem_actual = n(metrics, "memory_actual");
        vm.started = parse_xen_time(&s(metrics, "start_time"));
    }
    // The tools report their version as major / minor / micro; without tools the server says -1.
    let pv = guest.get("PV_drivers_version").unwrap_or(&null);
    let version: Vec<i64> = ["major", "minor", "micro"].iter().filter_map(|k| s(pv, k).trim().parse().ok()).collect();
    let real = !version.is_empty() && version.iter().all(|&x| x >= 0);
    vm.tools_installed = real || b(guest, "PV_drivers_detected");
    vm.tools = if real && version.len() >= 2 { version.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(".") } else { String::new() };
    // every address the tools report ("0/ip", "0/ipv4/0", "0/ipv6/1"…), IPv4 first, no link-local
    if let Some(m) = guest.get("networks").and_then(|v| v.as_object()) {
        let mut ips: Vec<String> = m
            .iter()
            .filter(|(k, _)| k.ends_with("/ip") || k.contains("/ipv4/") || k.contains("/ipv6/"))
            .map(|(_, v)| val_str(v))
            .filter(|ip| !ip.is_empty() && !ip.to_ascii_lowercase().starts_with("fe80"))
            .collect();
        ips.sort();
        ips.dedup();
        ips.sort_by_key(|ip| ip.contains(':'));
        vm.ips = ips;
    }
}

impl Connection for RealConnection {
    fn hostname(&self) -> String {
        let h = self.state.lock().unwrap().host.clone();
        if self.port == DEFAULT_PORT { h } else { format!("{}:{}", h, self.port) }
    }
    fn username(&self) -> String {
        self.user.clone()
    }
    fn is_demo(&self) -> bool {
        false
    }

    fn fetch(&self, t: u64) -> Result<PoolData, ApiError> {
        let mut pd = PoolData::default();
        let mut master = String::new();
        let mut repos: Vec<String> = vec![];
        for (_, r) in self.all("pool", t)? {
            pd.name = s(&r, "name_label");
            master = s(&r, "master");
            pd.default_sr = Some(s(&r, "default_SR")).filter(|x| !x.is_empty() && x != "OpaqueRef:NULL");
            repos = list(&r, "repositories");
            pd.last_update_sync = parse_xen_time(&s(&r, "last_update_sync"));
        }
        // its update channel: the enabled repository that has updates (not the base one)
        if !repos.is_empty() {
            let all = self.all("Repository", t).unwrap_or_default();
            let enabled: Vec<&Value> = repos.iter().filter_map(|r| all.get(r)).collect();
            if let Some(r) = enabled.iter().find(|r| b(r, "update")).or(enabled.first()) {
                let last = s(r, "binary_url").trim_end_matches('/').rsplit('/').next().unwrap_or_default().to_lowercase();
                pd.update_channel = Some(match last.as_str() {
                    "normal" => "Normal".to_string(),
                    "earlyaccess" => "Early access".to_string(),
                    _ => or_default(&s(r, "name_label"), "Its own"),
                });
                pd.update_bundle = s(r, "origin") == "bundle";
            }
        }
        let metrics = self.all("host_metrics", t)?;
        for (k, r) in self.all("host", t)? {
            let sw = r.get("software_version").cloned().unwrap_or(Value::Null);
            let cpu = r.get("cpu_info").cloned().unwrap_or(Value::Null);
            let m = metrics.get(&s(&r, "metrics")).cloned().unwrap_or(Value::Null);
            let version = s(&sw, "product_version");
            pd.hosts.push(HostInfo {
                is_master: k == master,
                rref: k,
                uuid: s(&r, "uuid"),
                name: s(&r, "name_label"),
                description: s(&r, "name_description"),
                address: s(&r, "address"),
                version: if version.is_empty() { s(&sw, "platform_version") } else { version },
                cpu_model: s(&cpu, "modelname"),
                cpu_count: n(&cpu, "cpu_count"),
                mem_total: n(&m, "memory_total"),
                mem_free: n(&m, "memory_free"),
                enabled: b(&r, "enabled"),
                control_domain: String::new(), // filled in from the VMs below
                live: m.get("live").map_or(true, |x| x.as_bool().unwrap_or(true)),
                unreachable: false, // see knock_hosts
                boot_time: r.get("other_config").map(|o| s(o, "boot_time")).and_then(|t| t.trim().parse::<f64>().ok()).filter(|t| *t > 0.0).map(|t| t as i64),
                brand: s(&sw, "product_brand"),
                edition: s(&r, "edition"),
                license_name: r.get("license_params").map(|l| s(l, "sku_marketing_name")).unwrap_or_default(),
                license_expiry: r.get("license_params").and_then(|l| parse_xen_time(&s(l, "expiry"))),
                updates_applied: s(&r, "latest_synced_updates_applied"),
                pending_guidance: list(&r, "pending_guidances").into_iter().chain(list(&r, "pending_guidances_recommended")).collect(),
                last_update: parse_xen_time(&s(&r, "last_software_update")),
                nics: vec![], // filled in from the PIFs below
            });
        }
        self.knock_hosts(&mut pd.hosts);
        // Each storage is plugged into the servers that can use it (a PBD per server): local storage
        // into one, shared storage into all of them.
        let mut plugged: HashMap<String, Vec<(String, bool)>> = HashMap::new();
        let mut configs: HashMap<String, Value> = HashMap::new();
        for (_, r) in self.all("PBD", t)? {
            plugged.entry(s(&r, "SR")).or_default().push((s(&r, "host"), b(&r, "currently_attached")));
            configs.entry(s(&r, "SR")).or_insert_with(|| r.get("device_config").cloned().unwrap_or(Value::Null));
        }
        for (k, r) in self.all("SR", t)? {
            let shared = b(&r, "shared");
            let links = plugged.get(&k).cloned().unwrap_or_default();
            let host = if !shared && links.len() == 1 { Some(links[0].0.clone()) } else { None };
            let config = configs.get(&k).cloned().unwrap_or(Value::Null);
            let share = match s(&config, "type").as_str() {
                "cifs" => format!("Windows share (SMB {})", or_default(&s(&config, "vers"), "3.0")),
                "nfs_iso" => format!("NFS (v{})", or_default(&s(&config, "nfsversion"), "3")),
                _ => String::new(),
            };
            pd.srs.push(SrInfo {
                location: s(&config, "location"),
                share,
                rref: k,
                uuid: s(&r, "uuid"),
                name: s(&r, "name_label"),
                description: s(&r, "name_description"),
                kind: s(&r, "type"),
                size: n(&r, "physical_size"),
                used: n(&r, "physical_utilisation"),
                shared,
                host,
                files: vec![], // filled in from the VDIs below
                links,
            });
        }
        // every network's name (a VM's card may be on one that isn't shown)
        let mut net_names: HashMap<String, String> = HashMap::new();
        for (k, r) in self.all("network", t)? {
            net_names.insert(k.clone(), s(&r, "name_label"));
            if hidden_network(r.get("other_config").unwrap_or(&Value::Null)) {
                continue;
            }
            pd.networks.push(NetInfo {
                rref: k,
                uuid: s(&r, "uuid"),
                name: s(&r, "name_label"),
                description: s(&r, "name_description"),
                bridge: s(&r, "bridge"),
                mtu: n(&r, "MTU"),
            });
        }
        // each server's network cards: the physical ones and bonds, not a bond's members or VLANs
        // (if these can't be read, the rest still shows)
        let pif_metrics = self.all("PIF_metrics", t).unwrap_or_default();
        for (_, r) in self.all("PIF", t).unwrap_or_default() {
            let null = |k: &str| matches!(s(&r, k).as_str(), "" | "OpaqueRef:NULL");
            let vlan = r.get("VLAN").map_or(-1, |_| n(&r, "VLAN"));
            if !null("bond_slave_of") || vlan >= 0 || (!b(&r, "physical") && list(&r, "bond_master_of").is_empty()) {
                continue;
            }
            let m = pif_metrics.get(&s(&r, "metrics")).cloned().unwrap_or(Value::Null);
            let nic = HostNic {
                device: s(&r, "device"),
                network: net_names.get(&s(&r, "network")).cloned().unwrap_or_default(),
                ip: s(&r, "IP"),
                management: b(&r, "management"),
                carrier: b(&m, "carrier"),
                speed: n(&m, "speed"),
            };
            if let Some(h) = pd.hosts.iter_mut().find(|h| h.rref == s(&r, "host")) {
                h.nics.push(nic);
            }
        }
        for h in pd.hosts.iter_mut() {
            h.nics.sort_by(|a, b| a.device.cmp(&b.device));
        }
        let vbds = self.all("VBD", t)?;
        let vdis = self.all("VDI", t)?;
        let vm_records = self.all("VM", t)?;
        // the built-in templates (a copy made by someone has no default_template mark)
        for (k, r) in vm_records.iter().filter(|(_, r)| b(r, "is_a_template") && !b(r, "is_a_snapshot")) {
            let other = r.get("other_config").cloned().unwrap_or(Value::Null);
            if s(&other, "default_template") != "true" {
                continue;
            }
            let platform = r.get("platform").cloned().unwrap_or(Value::Null);
            let boot = r.get("HVM_boot_params").cloned().unwrap_or(Value::Null);
            let rec = s(r, "recommendations");
            pd.templates.push(TemplateInfo {
                rref: k.clone(),
                name: s(r, "name_label"),
                uefi: s(&boot, "firmware").eq_ignore_ascii_case("uefi"),
                secure_boot: s(&platform, "secureboot").to_lowercase(),
                vtpm: s(&platform, "vtpm").eq_ignore_ascii_case("true"),
                supports_uefi: restriction(&rec, "supports-uefi").as_deref() == Some("yes"),
                supports_secure_boot: restriction(&rec, "supports-secure-boot").as_deref() == Some("yes"),
                mem_min: n(r, "memory_static_min"),
                vcpus: n(r, "VCPUs_at_startup"),
                disk: provision_size(&s(&other, "disks")),
            });
        }
        pd.templates.sort_by_key(|t| (t.name != OTHER_MEDIA, t.name.to_lowercase()));
        // an ISO library's discs are its VDIs; the VMs with one in a DVD drive, through its VBDs
        let in_drives = |v: &Value| -> Vec<(String, bool)> {
            list(v, "VBDs")
                .iter()
                .filter_map(|r| vbds.get(r))
                .filter(|vbd| !b(vbd, "empty"))
                .filter_map(|vbd| {
                    let vm = vm_records.get(&s(vbd, "VM"))?;
                    let held = b(vbd, "currently_attached") || b(vbd, "reserved") || s(vm, "power_state") != "Halted";
                    Some((s(vm, "name_label"), held))
                })
                .collect()
        };
        for x in pd.srs.iter_mut().filter(|x| x.is_iso()) {
            x.files = vdis
                .iter()
                .filter(|(_, v)| s(v, "SR") == x.rref)
                .map(|(k, v)| IsoFile {
                    vdi: k.clone(),
                    name: s(v, "name_label"),
                    size: n(v, "virtual_size"),
                    description: s(v, "name_description"),
                    in_drives: in_drives(v),
                    can_delete: list(v, "allowed_operations").iter().any(|o| o == "destroy"),
                })
                .collect();
            x.files.sort_by_key(|f| f.name.to_lowercase());
        }
        let vifs = self.all("VIF", t)?;
        let guests = self.all("VM_guest_metrics", t)?;
        let vm_metrics = self.all("VM_metrics", t)?;
        let sr_names: HashMap<String, String> = pd.srs.iter().map(|x| (x.rref.clone(), x.name.clone())).collect();
        let sr_hosts: HashMap<String, String> = pd.srs.iter().filter_map(|x| x.host.clone().map(|h| (x.rref.clone(), h))).collect();
        // each server's control domain (dom0), for its console
        let mut dom0: HashMap<String, String> = HashMap::new();
        // each VM's snapshots (they're VM records too), the newest first
        let mut snapshots: HashMap<String, Vec<SnapshotInfo>> = HashMap::new();
        for (k, r) in vm_records.iter().filter(|(_, r)| b(r, "is_a_snapshot")) {
            let info = r.get("snapshot_info").cloned().unwrap_or(Value::Null);
            snapshots.entry(s(r, "snapshot_of")).or_default().push(SnapshotInfo {
                rref: k.clone(),
                name: s(r, "name_label"),
                description: s(r, "name_description"),
                time: parse_xen_time(&s(r, "snapshot_time")),
                // a checkpoint's record is suspended: its memory is saved with it
                memory: s(r, "power_state") == "Suspended",
                was_running: s(&info, "power-state-at-snapshot") == "Running",
            });
        }
        for list in snapshots.values_mut() {
            list.sort_by_key(|x| std::cmp::Reverse(x.time));
        }
        for (k, r) in vm_records {
            if b(&r, "is_control_domain") && !b(&r, "is_a_snapshot") {
                dom0.insert(s(&r, "resident_on"), k.clone());
            }
            // the templates people made (not the built-in ones) are read like VMs, and kept apart
            let user_template = b(&r, "is_a_template") && !b(&r, "is_a_snapshot") && s(r.get("other_config").unwrap_or(&Value::Null), "default_template") != "true";
            if (b(&r, "is_a_template") && !user_template) || b(&r, "is_control_domain") || b(&r, "is_a_snapshot") {
                continue;
            }
            let host = s(&r, "resident_on");
            let g = guests.get(&s(&r, "guest_metrics")).cloned().unwrap_or(Value::Null);
            let netmap = g.get("networks").cloned().unwrap_or(Value::Null);
            let affinity = s(&r, "affinity");
            let mut vm = VmInfo {
                rref: k,
                uuid: s(&r, "uuid"),
                name: s(&r, "name_label"),
                description: s(&r, "name_description"),
                host: if host.is_empty() || host == "OpaqueRef:NULL" { None } else { Some(host) },
                storage_host: None,
                affinity: if pd.hosts.iter().any(|h| h.rref == affinity) { Some(affinity) } else { None },
                // Windows guests add "|C:\Windows|…" after the name
                os: s(g.get("os_version").unwrap_or(&Value::Null), "name").split('|').next().unwrap_or_default().trim().to_string(),
                ip: String::new(),
                power: PowerState::parse(&s(&r, "power_state")),
                mem_static_max: n(&r, "memory_static_max"),
                mem_dyn_min: n(&r, "memory_dynamic_min"),
                mem_dyn_max: n(&r, "memory_dynamic_max"),
                vcpus: n(&r, "VCPUs_at_startup"),
                allowed: list(&r, "allowed_operations"),
                disks: vec![],
                nics: vec![],
                ..Default::default()
            };
            vm_details(&mut vm, &r, &g, vm_metrics.get(&s(&r, "metrics")).unwrap_or(&Value::Null));
            vm.snapshots = snapshots.remove(&vm.rref).unwrap_or_default();
            let parent = s(&r, "parent");
            vm.parent = vm.snapshots.iter().any(|x| x.rref == parent).then_some(parent);
            for vbd_ref in list(&r, "VBDs") {
                let Some(vbd) = vbds.get(&vbd_ref) else { continue };
                let cd = s(vbd, "type") == "CD";
                let mut d = VmDisk { device: s(vbd, "userdevice"), kind: if cd { "DVD drive".into() } else { "Disk".into() }, vbd: vbd_ref.clone(), ..Default::default() };
                if let Some(vdi) = vdis.get(&s(vbd, "VDI")) {
                    d.vdi = s(vbd, "VDI");
                    if vm.storage_host.is_none() {
                        vm.storage_host = sr_hosts.get(&s(vdi, "SR")).cloned();
                    }
                    d.name = s(vdi, "name_label");
                    d.size = n(vdi, "virtual_size");
                    d.sr = s(vdi, "SR");
                    d.sr_name = sr_names.get(&d.sr).cloned().unwrap_or_default();
                } else if cd {
                    d.name = "(empty)".into();
                }
                vm.disks.push(d);
            }
            vm.disks.sort_by(|a, b| a.device.cmp(&b.device));
            for vif_ref in list(&r, "VIFs") {
                let Some(vif) = vifs.get(&vif_ref) else { continue };
                let dev = s(vif, "device");
                let mut ip = s(&netmap, &format!("{}/ip", dev));
                if ip.is_empty() {
                    ip = s(&netmap, &format!("{}/ipv4/0", dev));
                }
                if vm.ip.is_empty() && !ip.is_empty() {
                    vm.ip = ip.clone();
                }
                vm.nics.push(VmNic { vif: vif_ref.clone(), net: s(vif, "network"), mac: s(vif, "MAC"), network: net_names.get(&s(vif, "network")).cloned().unwrap_or_default(), ip, device: dev });
            }
            vm.nics.sort_by(|a, b| a.device.cmp(&b.device));
            if user_template {
                pd.user_templates.push(vm);
            } else {
                pd.vms.push(vm);
            }
        }
        for h in pd.hosts.iter_mut() {
            h.control_domain = dom0.remove(&h.rref).unwrap_or_default();
        }
        // where to log in next time, should this server stop answering
        self.state.lock().unwrap().members = pd.hosts.iter().map(|h| h.address.clone()).filter(|a| !a.is_empty()).collect();
        if pd.name.is_empty() && pd.hosts.len() > 1 {
            pd.name = "Pool".into();
        }
        Ok(pd)
    }

    fn run_action(&self, vm_ref: &str, a: &VmAction) -> Result<(), ApiError> {
        let mut args = vec![json!(vm_ref)];
        if a.extra_false_args {
            args.push(json!(false));
            args.push(json!(false));
        }
        self.call(a.method, args, ACTION_TIMEOUT_MS).map(|_| ())
    }

    /// SR.create on the pool coordinator; a shared library gets plugged into every server.
    fn create_iso_library(&self, lib: &IsoLibrary) -> Result<(), ApiError> {
        let t = 30_000;
        let coordinator = self.all("pool", t)?.values().map(|p| s(p, "master")).next().unwrap_or_default();
        let config: Map<String, Value> = lib.device_config().into_iter().map(|(k, v)| (k, json!(v))).collect();
        // mounting an unreachable share can take a minute or two to give up
        let args = vec![json!(coordinator), Value::Object(config), json!(0), json!(lib.name), json!(lib.description), json!("iso"), json!("iso"), json!(lib.shared), json!({})];
        self.call("SR.create", args, 180_000).map(|_| ())
    }

    fn scan_sr(&self, sr_ref: &str) -> Result<(), ApiError> {
        self.call("SR.scan", vec![json!(sr_ref)], 120_000).map(|_| ())
    }

    fn forget_sr(&self, sr_ref: &str) -> Result<(), ApiError> {
        let t = 60_000;
        for pbd in self.call("SR.get_PBDs", vec![json!(sr_ref)], t)?.as_array().cloned().unwrap_or_default() {
            if self.call("PBD.get_currently_attached", vec![pbd.clone()], t)?.as_bool().unwrap_or(false) {
                self.call("PBD.unplug", vec![pbd], 120_000)?;
            }
        }
        self.call("SR.forget", vec![json!(sr_ref)], t).map(|_| ())
    }
    fn repair_sr(&self, sr: &str) -> Result<Vec<(String, Result<(), ApiError>)>, ApiError> {
        let t = 30_000;
        let coordinator = self.all("pool", t)?.values().map(|p| s(p, "master")).next().unwrap_or_default();
        let hosts = self.all("host", t)?;
        let metrics = self.all("host_metrics", t)?;
        let live = |h: &str| hosts.get(h).and_then(|r| metrics.get(&s(r, "metrics"))).map_or(true, |m| m.get("live").map_or(true, |x| x.as_bool().unwrap_or(true)));
        let name = |h: &str| hosts.get(h).map(|r| s(r, "name_label")).unwrap_or_else(|| h.to_string());
        let shared = self.call("SR.get_shared", vec![json!(sr)], t)?.as_bool().unwrap_or(false);
        // (connection, server, mounted)
        let mut pbds: Vec<(Value, String, bool)> = vec![];
        let mut config = Value::Null;
        for p in self.call("SR.get_PBDs", vec![json!(sr)], t)?.as_array().cloned().unwrap_or_default() {
            let r = self.call("PBD.get_record", vec![p.clone()], t)?;
            if config.is_null() {
                config = r.get("device_config").cloned().unwrap_or(Value::Null);
            }
            pbds.push((p, s(&r, "host"), b(&r, "currently_attached")));
        }
        if pbds.is_empty() {
            return Err(ApiError { code: "SR_HAS_NO_PBDS".into(), details: vec![sr.to_string()] });
        }
        let mut out = vec![];
        if shared {
            let without: Vec<String> = hosts.keys().filter(|h| !pbds.iter().any(|p| &p.1 == *h)).cloned().collect();
            for h in &without {
                let rec = json!({ "host": h, "SR": sr, "device_config": config, "other_config": {} });
                match self.call("PBD.create", vec![rec], t) {
                    Ok(p) => pbds.push((p, h.clone(), false)),
                    Err(e) => out.push((name(h), Err(e))),
                }
            }
        }
        pbds.retain(|p| !p.2 && live(&p.1));
        pbds.sort_by_key(|p| p.1 != coordinator);
        for (p, h, _) in pbds {
            // mounting a share that doesn't answer can take a minute or two to give up
            let r = self.call("PBD.plug", vec![p], 180_000).map(|_| ());
            let stop = r.is_err() && h == coordinator && shared;
            out.push((name(&h), r));
            if stop {
                break;
            }
        }
        Ok(out)
    }

    fn reattach_sr(&self, sr: &str, lib: &IsoLibrary) -> Result<(), ApiError> {
        let t = 30_000;
        let coordinator = self.all("pool", t)?.values().map(|p| s(p, "master")).next().unwrap_or_default();
        let shared = self.call("SR.get_shared", vec![json!(sr)], t)?.as_bool().unwrap_or(false);
        let mut hosts = vec![coordinator.clone()];
        if shared {
            hosts.extend(self.all("host", t)?.keys().filter(|h| **h != coordinator).cloned());
        }
        let config: Map<String, Value> = lib.device_config().into_iter().map(|(k, v)| (k, json!(v))).collect();
        let undo = |made: &[Value]| {
            for p in made {
                let _ = self.call("PBD.destroy", vec![p.clone()], t);
            }
        };
        let mut made = vec![];
        for h in &hosts {
            let rec = json!({ "host": h, "SR": sr, "device_config": config, "other_config": {} });
            match self.call("PBD.create", vec![rec], t) {
                Ok(p) => made.push(p),
                Err(e) => {
                    undo(&made);
                    return Err(e);
                }
            }
        }
        // the coordinator's first: if the share can't be mounted there, nothing is kept
        if let Err(e) = self.call("PBD.plug", vec![made[0].clone()], 180_000) {
            undo(&made);
            return Err(e);
        }
        // a member that can't is left unplugged, for Repair to show
        for p in &made[1..] {
            let _ = self.call("PBD.plug", vec![p.clone()], 180_000);
        }
        Ok(())
    }

    fn check_sr(&self, sr: &str, host: Option<&str>, names: &HashMap<String, String>, tr: &Trace) -> Result<(), ApiError> {
        let t = 30_000;
        let label = named(names, sr);
        let gone = |code: &str| ApiError { code: code.into(), details: vec![names.get(sr).cloned().unwrap_or_default()] };
        // plugged into the server it's for (or any)
        tr.push(Said::Asked, format!("SR.get_PBDs  SR={}", label));
        let started = Instant::now();
        let plugged = (|| -> Result<Vec<(String, bool)>, ApiError> {
            let mut out = vec![];
            for p in self.call("SR.get_PBDs", vec![json!(sr)], t)?.as_array().cloned().unwrap_or_default() {
                let r = self.call("PBD.get_record", vec![p], t)?;
                out.push((s(&r, "host"), b(&r, "currently_attached")));
            }
            Ok(out)
        })();
        let plugged = plugged.map_err(|e| {
            tr.outcome(&Err(e.clone()), started.elapsed());
            e
        })?;
        let on = host.map(|h| named(names, h)).unwrap_or_else(|| "a server".into());
        if !plugged.iter().any(|(h, attached)| *attached && host.map_or(true, |x| x == h)) {
            tr.push(Said::Error, format!("not plugged in on {}", on));
            return Err(gone(NOT_PLUGGED));
        }
        tr.push(Said::Answer, format!("plugged in on {}  ({:.1} s)", on, started.elapsed().as_secs_f32()));
        // then looked at
        tr.push(Said::Asked, format!("SR.scan  SR={}", label));
        let started = Instant::now();
        let task = self.call("Async.SR.scan", vec![json!(sr)], t).map_err(|e| {
            tr.outcome(&Err(e.clone()), started.elapsed());
            e
        })?;
        let task = task.as_str().unwrap_or_default().to_string();
        match self.task_result(&task, Duration::from_secs(CHECK_SECS)) {
            Ok(false) => {
                // it goes on (on the server) till the share is back; asked to stop anyway
                let _ = self.call("task.cancel", vec![json!(task)], t);
                tr.push(Said::Error, format!("no answer in {} s", CHECK_SECS));
                Err(gone(NOT_ANSWERING))
            }
            r => {
                let _ = self.call("task.destroy", vec![json!(task)], t);
                let r = r.map(|_| Value::Null);
                tr.outcome(&r, started.elapsed());
                r.map(|_| ())
            }
        }
    }

    fn set_vdi_description(&self, vdi_ref: &str, text: &str) -> Result<(), ApiError> {
        self.call("VDI.set_name_description", vec![json!(vdi_ref), json!(text)], 30_000).map(|_| ())
    }

    fn set_vm_description(&self, vm_ref: &str, text: &str) -> Result<(), ApiError> {
        self.call("VM.set_name_description", vec![json!(vm_ref), json!(text)], 30_000).map(|_| ())
    }

    fn snapshot_vm(&self, vm: &str, name: &str, description: &str, memory: bool) -> Result<String, ApiError> {
        // a checkpoint writes the VM's memory out, which takes as long as a suspend
        let (method, wait) = if memory { ("VM.checkpoint", 3_600_000) } else { ("VM.snapshot", 1_800_000) };
        let snap = self.call(method, vec![json!(vm), json!(name)], wait)?.as_str().unwrap_or_default().to_string();
        if !description.is_empty() {
            self.call("VM.set_name_description", vec![json!(snap), json!(description)], 30_000)?;
        }
        Ok(snap)
    }

    fn make_template(&self, source: &str, name: &str, description: &str) -> Result<String, ApiError> {
        let t = 60_000;
        // a clone of a snapshot is a template already; a VM's is made one
        let tpl = self.call("VM.clone", vec![json!(source), json!(name)], 1_800_000)?.as_str().unwrap_or_default().to_string();
        let done = (|| -> Result<(), ApiError> {
            if !self.call("VM.get_is_a_template", vec![json!(tpl)], t)?.as_bool().unwrap_or(false) {
                self.call("VM.set_is_a_template", vec![json!(tpl), json!(true)], t)?;
            }
            self.call("VM.set_name_description", vec![json!(tpl), json!(description)], t)?;
            Ok(())
        })();
        if let Err(e) = done {
            let _ = self.delete_vm(&tpl, true);
            return Err(e);
        }
        Ok(tpl)
    }

    fn copy_vm(&self, vm: &str, name: &str, sr: Option<&str>) -> Result<String, ApiError> {
        let made = match sr {
            None => self.call("VM.clone", vec![json!(vm), json!(name)], 1_800_000)?,
            // a full copy takes as long as writing its disks out again
            Some(sr) => self.call("VM.copy", vec![json!(vm), json!(name), json!(sr)], 6 * 3_600_000)?,
        };
        Ok(made.as_str().unwrap_or_default().to_string())
    }

    fn edit_vm(&self, vm: &str, change: &VmChange) -> Result<(), ApiError> {
        let (t, long) = (30_000, 600_000);
        let running = || self.call("VM.get_power_state", vec![json!(vm)], t).map(|p| p.as_str() == Some("Running"));
        // a map's key set afresh (an old value goes first; it may not be there)
        let set_key = |obj: &str, map: &str, r: &str, k: &str, v: &str| -> Result<(), ApiError> {
            let _ = self.call(&format!("{}.remove_from_{}", obj, map), vec![json!(r), json!(k)], t);
            self.call(&format!("{}.add_to_{}", obj, map), vec![json!(r), json!(k), json!(v)], t).map(|_| ())
        };
        match change {
            VmChange::Name(n) => self.call("VM.set_name_label", vec![json!(vm), json!(n)], t).map(|_| ()),
            VmChange::Description(d) => self.call("VM.set_name_description", vec![json!(vm), json!(d)], t).map(|_| ()),
            VmChange::AutoPowerOn(on) => {
                set_key("VM", "other_config", vm, "auto_poweron", if *on { "true" } else { "false" })?;
                // the pool has to allow it too
                if *on {
                    let pool = self.call("pool.get_all", vec![], t)?.as_array().and_then(|a| a.first().cloned()).unwrap_or(Value::Null);
                    let pool = pool.as_str().unwrap_or_default().to_string();
                    set_key("pool", "other_config", &pool, "auto_poweron", "true")?;
                }
                Ok(())
            }
            VmChange::BootOrder(order) => set_key("VM", "HVM_boot_params", vm, "order", order),
            VmChange::Resources { vcpus, cores_per_socket, memory } => {
                let r = self.call("VM.get_record", vec![json!(vm)], t)?;
                let order = if *vcpus > n(&r, "VCPUs_max") { ["VM.set_VCPUs_max", "VM.set_VCPUs_at_startup"] } else { ["VM.set_VCPUs_at_startup", "VM.set_VCPUs_max"] };
                for m in order {
                    self.call(m, vec![json!(vm), json!(vcpus)], t)?;
                }
                set_key("VM", "platform", vm, "cores-per-socket", &cores_per_socket.to_string())?;
                let floor = n(&r, "memory_static_min").min(*memory);
                self.call("VM.set_memory_limits", vec![json!(vm), json!(floor), json!(memory), json!(memory), json!(memory)], t).map(|_| ())
            }
            VmChange::AddDisk { name, size, sr } => {
                let device = self.call("VM.get_allowed_VBD_devices", vec![json!(vm)], t)?.as_array().and_then(|a| a.first().cloned()).and_then(|d| d.as_str().map(String::from)).ok_or_else(|| ApiError::other("The VM has no room for another disk."))?;
                let record = json!({
                    "name_label": name, "name_description": "Created by Simple VM Manager", "SR": sr,
                    "virtual_size": size, "type": "user", "sharable": false, "read_only": false,
                    "other_config": {}, "xenstore_data": {}, "sm_config": { "vmhint": vm }, "tags": [],
                });
                let vdi = self.call("VDI.create", vec![record], long)?.as_str().unwrap_or_default().to_string();
                let drive = json!({
                    "VM": vm, "VDI": vdi, "userdevice": device, "bootable": false, "mode": "RW", "type": "Disk", "unpluggable": true,
                    "empty": false, "other_config": { "owner": "" }, "qos_algorithm_type": "", "qos_algorithm_params": {}, "device": "", "currently_attached": false,
                });
                let vbd = match self.call("VBD.create", vec![drive], t) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = self.call("VDI.destroy", vec![json!(vdi)], long);
                        return Err(e);
                    }
                };
                // plugged in now if it can be (a VM without its tools gets it at its next start)
                if running()? {
                    let _ = self.call("VBD.plug", vec![vbd], long);
                }
                Ok(())
            }
            VmChange::GrowDisk { vdi, size } => self.call("VDI.resize", vec![json!(vdi), json!(size)], long).map(|_| ()),
            VmChange::RemoveDisk { vbd, vdi } => {
                self.call("VBD.destroy", vec![json!(vbd)], t)?;
                // the disk too, unless something else has it
                let others = self.call("VDI.get_VBDs", vec![json!(vdi)], t)?.as_array().map_or(0, |a| a.len());
                if others == 0 {
                    self.call("VDI.destroy", vec![json!(vdi)], long)?;
                }
                Ok(())
            }
            VmChange::Dvd { vbd, iso } => {
                if !self.call("VBD.get_empty", vec![json!(vbd)], t)?.as_bool().unwrap_or(true) {
                    self.call("VBD.eject", vec![json!(vbd)], long)?;
                }
                if let Some(iso) = iso {
                    self.call("VBD.insert", vec![json!(vbd), json!(iso)], long)?;
                }
                Ok(())
            }
            VmChange::AddNic { net } => {
                let device = self.call("VM.get_allowed_VIF_devices", vec![json!(vm)], t)?.as_array().and_then(|a| a.first().cloned()).and_then(|d| d.as_str().map(String::from)).ok_or_else(|| ApiError::other("The VM has no room for another network card."))?;
                let mtu = self.call("network.get_MTU", vec![json!(net)], t).map(|m| n(&json!({ "m": m }), "m")).unwrap_or(1500);
                let card = json!({
                    "device": device, "network": net, "VM": vm, "MAC": "", "MTU": mtu.max(68), "other_config": {},
                    "qos_algorithm_type": "", "qos_algorithm_params": {}, "locking_mode": "network_default",
                    "ipv4_allowed": [], "ipv6_allowed": [], "currently_attached": false,
                });
                let vif = self.call("VIF.create", vec![card], t)?;
                if running()? {
                    let _ = self.call("VIF.plug", vec![vif], long);
                }
                Ok(())
            }
            VmChange::MoveNic { vif, net } => self.call("VIF.move", vec![json!(vif), json!(net)], long).map(|_| ()),
            VmChange::RemoveNic { vif } => {
                if self.call("VIF.get_currently_attached", vec![json!(vif)], t)?.as_bool().unwrap_or(false) {
                    self.call("VIF.unplug", vec![json!(vif)], long)?;
                }
                self.call("VIF.destroy", vec![json!(vif)], t).map(|_| ())
            }
        }
    }

    fn vm_perf(&self, uuid: &str, address: &str) -> Result<Perf, ApiError> {
        self.rrd(address, false, |text| {
            let p = parse_rrd_updates(text, uuid);
            let n = p.times.len();
            (p, n)
        })
    }

    fn host_perf(&self, uuid: &str, address: &str) -> Result<HostPerf, ApiError> {
        self.rrd(address, true, |text| {
            let p = parse_host_rrd(text, uuid);
            let n = p.host.times.len();
            (p, n)
        })
    }

    fn pool_updates(&self) -> Result<PoolUpdates, ApiError> {
        let (coordinator, session) = self.session(20_000)?;
        let at = if coordinator.contains(':') && !coordinator.starts_with('[') { format!("[{}]", coordinator) } else { coordinator };
        let port = if self.port == DEFAULT_PORT { String::new() } else { format!(":{}", self.port) };
        // (the first time after a fetch, the coordinator asks each server what it lacks before it
        // answers: minutes, on a pool of a few servers)
        let agent = ureq::AgentBuilder::new().tls_connector(self.tls.clone()).timeout_connect(Duration::from_secs(5)).timeout(Duration::from_secs(UPDATES_WITHIN)).build();
        let url = format!("https://{}{}/updates?session_id={}", at, port, session);
        said("GET /updates", &[]);
        let began = Instant::now();
        let got = match agent.get(&url).call() {
            Ok(r) => r.into_string().map_err(|e| ApiError::other(e.to_string())),
            // (no channel set up, or never fetched from)
            Err(ureq::Error::Status(404, _)) => Err(ApiError { code: "NO_REPOSITORY_ENABLED".into(), details: vec![] }),
            Err(ureq::Error::Transport(t)) if std::error::Error::source(&t).map_or(false, |c| c.to_string().contains("timed out")) => {
                Err(ApiError::other(format!("the server didn't answer within {} minutes", UPDATES_WITHIN / 60)))
            }
            Err(e) => Err(web_error(e)),
        };
        let r = got.and_then(|text| parse_pool_updates(&text).map_err(ApiError::other));
        answered(&r.as_ref().map(|p| json!(format!("{} updates", p.updates.len()))).map_err(|e| e.clone()), began.elapsed());
        r
    }

    fn sync_updates(&self) -> Result<String, ApiError> {
        let pool = self.pool_ref()?;
        let t = 3_600_000;
        // (no client ID needed since XenServer 8.4; older servers take only the token's two)
        let full = vec![pool.clone(), json!(false), json!(""), json!(""), json!(""), json!("")];
        let hash = match self.call("pool.sync_updates", full, t) {
            Err(e) if e.code == "MESSAGE_PARAMETER_COUNT_MISMATCH" => self.call("pool.sync_updates", vec![pool, json!(false), json!(""), json!("")], t)?,
            r => r?,
        };
        Ok(val_str(&hash))
    }

    fn update_readiness(&self) -> Result<Vec<String>, ApiError> {
        let pool = self.pool_ref()?;
        let r = self.call("pool.check_update_readiness", vec![pool, json!(false)], 120_000)?;
        Ok(r.as_array()
            .map(|a| {
                a.iter()
                    .map(|x| match x {
                        Value::Array(parts) => parts.iter().map(val_str).collect::<Vec<_>>().join(" "),
                        x => val_str(x),
                    })
                    .filter(|x| !x.trim().is_empty())
                    .collect()
            })
            .unwrap_or_default())
    }

    fn apply_updates(&self, host: &str, hash: &str) -> Result<Vec<String>, ApiError> {
        let r = self.call("host.apply_updates", vec![json!(host), json!(hash)], 7_200_000)?;
        Ok(r.as_array()
            .map(|a| a.iter().map(|x| if let Value::Array(p) = x { p.iter().map(val_str).collect::<Vec<_>>().join(" ") } else { val_str(x) }).filter(|x| !x.is_empty()).collect())
            .unwrap_or_default())
    }

    fn pending_guidance(&self, host: &str) -> Result<Vec<String>, ApiError> {
        let mut out = vec![];
        for field in ["host.get_pending_guidances", "host.get_pending_guidances_recommended"] {
            match self.call(field, vec![json!(host)], 30_000) {
                Ok(v) => out.extend(v.as_array().map(|a| a.iter().map(val_str).collect::<Vec<_>>()).unwrap_or_default()),
                // (older servers have only the first)
                Err(e) if e.code == "MESSAGE_METHOD_UNKNOWN" && !out.is_empty() => {}
                Err(e) if e.code == "MESSAGE_METHOD_UNKNOWN" => {}
                Err(e) => return Err(e),
            }
        }
        out.dedup();
        Ok(out)
    }

    fn vms_blocking_evacuation(&self, host: &str) -> Result<Vec<(String, String)>, ApiError> {
        self.blocking(host)
    }

    fn empty_host(&self, host: &str, stop: bool) -> Result<(Vec<String>, usize), ApiError> {
        self.emptied(host, stop)
    }

    fn restart_host_and_wait(&self, host: &str, limit: Duration) -> Result<(), ApiError> {
        let t = 30_000;
        let boot = || self.call("host.get_other_config", vec![json!(host)], t).map(|o| s(&o, "boot_time"));
        let before = boot().unwrap_or_default();
        self.call("host.reboot", vec![json!(host)], ACTION_TIMEOUT_MS)?;
        // back: it started again (a new boot time), or it was seen gone and is live again. While
        // it restarts, asking fails (the coordinator's own restart: every call): expected, so
        // these looks aren't noted, only the wait
        let began = Instant::now();
        note("restarting: it doesn't answer for a few minutes, as expected; waiting for it to come back…");
        let quiet = |m: &str, args: Vec<Value>| self.call_raw(m, args, t);
        let end = began + limit;
        let mut gone = false;
        std::thread::sleep(Duration::from_secs(20));
        loop {
            let live = (|| -> Result<bool, ApiError> {
                let started = s(&quiet("host.get_other_config", vec![json!(host)])?, "boot_time");
                let m = quiet("host.get_metrics", vec![json!(host)])?;
                let live = quiet("host_metrics.get_live", vec![m])?.as_bool().unwrap_or(false);
                Ok(live && (gone || (!started.is_empty() && started != before)))
            })();
            let was_gone = gone;
            match live {
                Ok(true) => {
                    note(format!("back after {}", took_text(began.elapsed())));
                    return Ok(());
                }
                Ok(false) => gone |= quiet("host.get_metrics", vec![json!(host)]).and_then(|m| quiet("host_metrics.get_live", vec![m])).map_or(true, |l| l.as_bool() == Some(false)),
                Err(_) => gone = true,
            }
            // its two halves: going down (it's still seen live), then starting up again
            if gone && !was_gone {
                note(format!("went down after {}; now waiting for it to start up again (its console shows how that goes)", took_text(began.elapsed())));
            }
            if Instant::now() >= end {
                return Err(ApiError::other(format!("it didn't answer again within {} minutes", limit.as_secs() / 60)));
            }
            std::thread::sleep(Duration::from_secs(15));
        }
    }

    fn restart_toolstack(&self, host: &str) -> Result<(), ApiError> {
        self.call("host.restart_agent", vec![json!(host)], 60_000)?;
        // it restarts after ten seconds; then answers again within a minute or two (asking fails
        // meanwhile, as expected: not noted)
        let began = Instant::now();
        note("its toolstack restarts: it doesn't answer for a minute or so, as expected; waiting…");
        std::thread::sleep(Duration::from_secs(25));
        let end = Instant::now() + Duration::from_secs(600);
        loop {
            if self.call_raw("host.get_enabled", vec![json!(host)], 15_000).is_ok() {
                note(format!("answering again after {}", took_text(began.elapsed())));
                return Ok(());
            }
            if Instant::now() >= end {
                return Err(ApiError::other("its toolstack didn't answer again within 10 minutes"));
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    fn enable_host(&self, host: &str) -> Result<(), ApiError> {
        // asked quietly: one just back says it's still starting up (HOST_STILL_BOOTING), as
        // expected, then takes VMs once it's done; only five minutes of that is a problem
        said("host.enable", &[json!(host)]);
        let began = Instant::now();
        let mut last = self.call_raw("host.enable", vec![json!(host)], 60_000).err();
        let mut said_waiting = false;
        loop {
            if self.call_raw("host.get_enabled", vec![json!(host)], 30_000).ok().and_then(|v| v.as_bool()) == Some(true) {
                note(if said_waiting { format!("taking VMs again after {}", took_text(began.elapsed())) } else { "taking VMs".to_string() });
                return Ok(());
            }
            if began.elapsed() >= Duration::from_secs(300) {
                return Err(last.unwrap_or_else(|| ApiError::other("it still doesn't take VMs after five minutes")));
            }
            if !said_waiting {
                note("still starting up: it takes VMs once it's done; waiting…");
                said_waiting = true;
            }
            std::thread::sleep(Duration::from_secs(10));
            last = self.call_raw("host.enable", vec![json!(host)], 60_000).err();
        }
    }

    fn start_vm_on(&self, vm: &str, host: &str) -> Result<(), ApiError> {
        self.call("VM.start_on", vec![json!(vm), json!(host), json!(false), json!(false)], ACTION_TIMEOUT_MS).map(|_| ())
    }

    fn revert_vm(&self, snap: &str) -> Result<(), ApiError> {
        let t = 60_000;
        let vm = self.call("VM.get_snapshot_of", vec![json!(snap)], t)?.as_str().unwrap_or_default().to_string();
        let r = self.call("VM.get_record", vec![json!(snap)], t)?;
        let memory = s(&r, "power_state") == "Suspended";
        let was_running = s(r.get("snapshot_info").unwrap_or(&Value::Null), "power-state-at-snapshot") == "Running";
        if self.call("VM.get_power_state", vec![json!(vm)], t)?.as_str() != Some("Halted") {
            self.call("VM.hard_shutdown", vec![json!(vm)], 300_000)?;
        }
        self.call("VM.revert", vec![json!(snap)], 1_800_000)?;
        // then as it was at the time
        let again = if memory {
            self.call("VM.resume", vec![json!(vm), json!(false), json!(false)], 1_800_000)
        } else if was_running {
            self.call("VM.start", vec![json!(vm), json!(false), json!(false)], 600_000)
        } else {
            return Ok(());
        };
        again.map(|_| ()).map_err(|e| ApiError { code: NOT_RESTARTED.into(), details: vec![e.message()] })
    }

    fn delete_snapshot(&self, snap: &str) -> Result<Vec<String>, ApiError> {
        let t = 60_000;
        let strings = |v: Value| -> Vec<String> { v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default() };
        let valid = |r: &str| !r.is_empty() && r != "OpaqueRef:NULL";
        // its own copies of the disks (never a disk something else uses), and its saved memory,
        // found before it goes
        let mut vdis: Vec<(String, String)> = vec![];
        for vbd in strings(self.call("VM.get_VBDs", vec![json!(snap)], t)?) {
            let r = self.call("VBD.get_record", vec![json!(vbd)], t)?;
            let vdi = s(&r, "VDI");
            if s(&r, "type") != "Disk" || !valid(&vdi) || !self.call("VDI.get_is_a_snapshot", vec![json!(vdi)], t)?.as_bool().unwrap_or(false) {
                continue;
            }
            let name = self.call("VDI.get_name_label", vec![json!(vdi)], t)?.as_str().unwrap_or_default().to_string();
            vdis.push((vdi, name));
        }
        let memory = self.call("VM.get_suspend_VDI", vec![json!(snap)], t)?.as_str().unwrap_or_default().to_string();
        if valid(&memory) {
            vdis.push((memory, "its saved memory".into()));
        }
        self.call("VM.destroy", vec![json!(snap)], t)?;
        let mut left = vec![];
        for (vdi, name) in vdis {
            // something else still has it (it shouldn't): left alone
            if !strings(self.call("VDI.get_VBDs", vec![json!(vdi)], t)?).is_empty() {
                continue;
            }
            if let Err(e) = self.call("VDI.destroy", vec![json!(vdi)], 300_000) {
                left.push(format!("{}: {}", if name.is_empty() { vdi } else { name }, e.message()));
            }
        }
        Ok(left)
    }

    fn create_vm(&self, v: &NewVm, tr: &Trace) -> Result<String, ApiError> {
        let t = 120_000;
        // each call goes in the trace, with its answer
        let call = |shown: String, method: &str, args: Vec<Value>, timeout: u64| {
            tr.push(Said::Asked, shown);
            let started = Instant::now();
            let r = self.call(method, args, timeout);
            tr.outcome(&r, started.elapsed());
            r
        };
        // a call whose failure doesn't matter
        let may = |shown: String, method: &str, args: Vec<Value>| {
            if call(shown, method, args, t).is_err() {
                tr.push(Said::Note, "(not needed; carrying on)");
            }
        };
        // first, that the ISO's library (and the disk's storage, if it's on the network) answers
        for sr in &v.check {
            self.check_sr(sr, v.host.as_deref(), &v.names, tr)?;
        }
        if v.from_template {
            return self.from_template(v, tr);
        }
        let mut names = v.names.clone();
        let vm = call(format!("VM.clone  vm={}  new_name=\"{}\"", named(&names, &v.template), v.name), "VM.clone", vec![json!(v.template), json!(v.name)], 300_000)?;
        let vm = vm.as_str().unwrap_or_default().to_string();
        names.insert(vm.clone(), v.name.clone());
        let me = named(&names, &vm);
        let mut disk: Option<String> = None;
        let made = (|| -> Result<(), ApiError> {
            let tpl = call(format!("VM.get_record  vm={}", named(&names, &v.template)), "VM.get_record", vec![json!(v.template)], t)?;
            // the vendor device (Windows gets its drivers through Windows Update), when the
            // template recommends it and the pool's policy allows it; a licence may still say no
            if restriction(&s(&tpl, "recommendations"), "has-vendor-device").as_deref() == Some("true") {
                let pool = call("pool.get_all".into(), "pool.get_all", vec![], t)?.as_array().and_then(|a| a.first().cloned()).unwrap_or(Value::Null);
                let shown = format!("pool.get_policy_no_vendor_device  pool={}", pool.as_str().unwrap_or_default());
                let refused = call(shown, "pool.get_policy_no_vendor_device", vec![pool], t)?.as_bool().unwrap_or(true);
                if !refused {
                    may(format!("VM.set_has_vendor_device  vm={}  value=true", me), "VM.set_has_vendor_device", vec![json!(vm), json!(true)]);
                }
            }
            // its disk is made below, not from the template's own list; provision also turns the
            // copy of the template into a VM
            may(format!("VM.remove_from_other_config  vm={}  key=\"disks\"", me), "VM.remove_from_other_config", vec![json!(vm), json!("disks")]);
            call(format!("VM.provision  vm={}", me), "VM.provision", vec![json!(vm)], 300_000)?;
            if !v.description.is_empty() {
                call(format!("VM.set_name_description  vm={}  value=\"{}\"", me, v.description), "VM.set_name_description", vec![json!(vm), json!(v.description)], t)?;
            }
            // CPUs: raise the maximum before the count at start, or lower the count first
            let (max, at_start) = ("VM.set_VCPUs_max", "VM.set_VCPUs_at_startup");
            let order = if v.vcpus > n(&tpl, "VCPUs_max") { [max, at_start] } else { [at_start, max] };
            for m in order {
                call(format!("{}  vm={}  value={}", m, me, v.vcpus), m, vec![json!(vm), json!(v.vcpus)], t)?;
            }
            may(format!("VM.remove_from_platform  vm={}  key=\"cores-per-socket\"", me), "VM.remove_from_platform", vec![json!(vm), json!("cores-per-socket")]);
            let shown = format!("VM.add_to_platform  vm={}  key=\"cores-per-socket\"  value=\"{}\"", me, v.cores_per_socket);
            call(shown, "VM.add_to_platform", vec![json!(vm), json!("cores-per-socket"), json!(v.cores_per_socket.to_string())], t)?;
            // memory: fixed at the chosen size
            let floor = n(&tpl, "memory_static_min").min(v.memory);
            let shown = format!("VM.set_memory_limits  vm={}  static_min={}  dynamic_min={}  dynamic_max={}  static_max={}", me, floor, v.memory, v.memory, v.memory);
            call(shown, "VM.set_memory_limits", vec![json!(vm), json!(floor), json!(v.memory), json!(v.memory), json!(v.memory)], t)?;
            // the boot mode, only when it isn't the template's
            if let Some(boot) = v.boot {
                let firmware = if boot == Boot::Bios { "bios" } else { "uefi" };
                let secure = if boot == Boot::UefiSecure { "true" } else { "false" };
                may(format!("VM.remove_from_HVM_boot_params  vm={}  key=\"firmware\"", me), "VM.remove_from_HVM_boot_params", vec![json!(vm), json!("firmware")]);
                let shown = format!("VM.add_to_HVM_boot_params  vm={}  key=\"firmware\"  value=\"{}\"", me, firmware);
                call(shown, "VM.add_to_HVM_boot_params", vec![json!(vm), json!("firmware"), json!(firmware)], t)?;
                may(format!("VM.remove_from_platform  vm={}  key=\"secureboot\"", me), "VM.remove_from_platform", vec![json!(vm), json!("secureboot")]);
                let shown = format!("VM.add_to_platform  vm={}  key=\"secureboot\"  value=\"{}\"", me, secure);
                call(shown, "VM.add_to_platform", vec![json!(vm), json!("secureboot"), json!(secure)], t)?;
            }
            if let Some(h) = &v.host {
                call(format!("VM.set_affinity  vm={}  host={}", me, named(&names, h)), "VM.set_affinity", vec![json!(vm), json!(h)], t)?;
            }
            // its disk
            let label = format!("{} disk", v.name);
            let shown = format!("VDI.create  SR={}  name_label=\"{}\"  virtual_size={}  type=system", named(&names, &v.sr), label, v.disk);
            let record = json!({
                "name_label": label, "name_description": "Created by Simple VM Manager", "SR": v.sr,
                "virtual_size": v.disk, "type": "system", "sharable": false, "read_only": false,
                "other_config": {}, "xenstore_data": {}, "sm_config": { "vmhint": vm }, "tags": [],
            });
            let vdi = call(shown, "VDI.create", vec![record], 300_000)?.as_str().unwrap_or_default().to_string();
            names.insert(vdi.clone(), label);
            disk = Some(vdi.clone());
            let drive = |vdi: &str, device: &str, cd: bool| {
                json!({
                    "VM": vm, "VDI": vdi, "userdevice": device, "bootable": cd, "mode": if cd { "RO" } else { "RW" },
                    "type": if cd { "CD" } else { "Disk" }, "unpluggable": true, "empty": false,
                    "other_config": if cd { json!({}) } else { json!({ "owner": "" }) },
                    "qos_algorithm_type": "", "qos_algorithm_params": {}, "device": "", "currently_attached": false,
                })
            };
            let shown = format!("VBD.create  VM={}  VDI={}  userdevice=0  type=Disk  mode=RW  bootable=false", me, named(&names, &vdi));
            call(shown, "VBD.create", vec![drive(&vdi, "0", false)], t)?;
            // the DVD drive, with the ISO in it (one the template brought is used)
            let mut cd = None;
            for vbd in call(format!("VM.get_VBDs  vm={}", me), "VM.get_VBDs", vec![json!(vm)], t)?.as_array().cloned().unwrap_or_default() {
                let id = vbd.as_str().unwrap_or_default().to_string();
                let r = call(format!("VBD.get_record  vbd={}", id), "VBD.get_record", vec![vbd.clone()], t)?;
                if s(&r, "type") == "CD" {
                    if !b(&r, "empty") {
                        call(format!("VBD.eject  vbd={}", id), "VBD.eject", vec![vbd.clone()], t)?;
                    }
                    cd = Some((vbd, id));
                    break;
                }
            }
            let iso = named(&names, &v.iso);
            match cd {
                Some((vbd, id)) => {
                    call(format!("VBD.insert  vbd={}  vdi={}", id, iso), "VBD.insert", vec![vbd, json!(v.iso)], t)?;
                }
                None => {
                    let shown = format!("VBD.create  VM={}  VDI={}  userdevice=3  type=CD  mode=RO  bootable=true", me, iso);
                    call(shown, "VBD.create", vec![drive(&v.iso, "3", true)], t)?;
                }
            }
            // its network card (any the template brought go)
            for vif in call(format!("VM.get_VIFs  vm={}", me), "VM.get_VIFs", vec![json!(vm)], t)?.as_array().cloned().unwrap_or_default() {
                call(format!("VIF.destroy  vif={}", vif.as_str().unwrap_or_default()), "VIF.destroy", vec![vif], t)?;
            }
            if let Some(net) = &v.network {
                let mtu = call(format!("network.get_MTU  network={}", named(&names, net)), "network.get_MTU", vec![json!(net)], t).map(|m| n(&json!({ "m": m }), "m")).unwrap_or(1500);
                let card = json!({
                    "device": "0", "network": net, "VM": vm, "MAC": "", "MTU": mtu.max(68), "other_config": {},
                    "qos_algorithm_type": "", "qos_algorithm_params": {}, "locking_mode": "network_default",
                    "ipv4_allowed": [], "ipv6_allowed": [], "currently_attached": false,
                });
                call(format!("VIF.create  VM={}  network={}  device=0  MTU={}", me, named(&names, net), mtu.max(68)), "VIF.create", vec![card], t)?;
            }
            Ok(())
        })();
        if let Err(e) = made {
            tr.push(Said::Note, "Undoing what was made:");
            if let Some(d) = disk {
                let _ = call(format!("VDI.destroy  vdi={}", named(&names, &d)), "VDI.destroy", vec![json!(d)], t);
            }
            let _ = call(format!("VM.destroy  vm={}", me), "VM.destroy", vec![json!(vm)], t); // with its drives and card
            return Err(e);
        }
        Ok(vm)
    }

    /// As a task on the server (Async.VM.start), watched until it ends: a start can take a while
    /// (the server may first have to free memory, or reach the ISO's share).
    fn start_traced(&self, vm: &str, label: &str, tr: &Trace) -> Result<(), ApiError> {
        tr.push(Said::Asked, format!("VM.start  vm={}  start_paused=false  force=false", label));
        let started = Instant::now();
        let r = self.call("Async.VM.start", vec![json!(vm), json!(false), json!(false)], 60_000).and_then(|task| {
            let task = task.as_str().unwrap_or_default().to_string();
            let r = self.task_result(&task, Duration::from_secs(600));
            if !matches!(r, Ok(false)) {
                let _ = self.call("task.destroy", vec![json!(task)], 30_000);
            }
            match r {
                Ok(true) => Ok(Value::Null),
                Ok(false) => Err(ApiError::other(format!("It's still starting after 10 minutes (the server's task {} goes on).", task))),
                Err(e) => Err(e),
            }
        });
        tr.outcome(&r, started.elapsed());
        r.map(|_| ())
    }

    fn delete_iso(&self, vdi_ref: &str) -> Result<(), ApiError> {
        let t = 60_000;
        let drives = self.call("VDI.get_VBDs", vec![json!(vdi_ref)], t)?.as_array().cloned().unwrap_or_default();
        let mut records = vec![];
        for vbd in drives {
            let r = self.call("VBD.get_record", vec![vbd.clone()], t)?;
            if b(&r, "currently_attached") || b(&r, "reserved") {
                return Err(ApiError { code: "VDI_IN_USE".into(), details: vec![vdi_ref.into(), "destroy".into()] });
            }
            records.push((vbd, r));
        }
        for (vbd, _) in records.into_iter().filter(|(_, r)| !b(r, "empty")) {
            self.call("VBD.eject", vec![vbd], 120_000)?;
        }
        self.call("VDI.destroy", vec![json!(vdi_ref)], 120_000).map(|_| ())
    }
    fn delete_vm(&self, vm: &str, disks: bool) -> Result<Vec<String>, ApiError> {
        let t = 60_000;
        let strings = |v: Value| -> Vec<String> { v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default() };
        // off, the hard way (a suspended VM's saved memory goes too)
        if self.call("VM.get_power_state", vec![json!(vm)], t)?.as_str() != Some("Halted") {
            self.call("VM.hard_shutdown", vec![json!(vm)], 300_000)?;
        }
        // the VM and its snapshots; then the disks that only they use, found before they go
        let mut going = vec![vm.to_string()];
        going.extend(strings(self.call("VM.get_snapshots", vec![json!(vm)], t)?));
        let mut vdis: Vec<(String, String)> = vec![];
        if disks {
            for owner in &going {
                for vbd in strings(self.call("VM.get_VBDs", vec![json!(owner)], t)?) {
                    let r = self.call("VBD.get_record", vec![json!(vbd)], t)?;
                    let vdi = s(&r, "VDI");
                    if s(&r, "type") != "Disk" || vdi.is_empty() || vdi == "OpaqueRef:NULL" || vdis.iter().any(|(v, _)| *v == vdi) {
                        continue;
                    }
                    let mut shared = false;
                    for other in strings(self.call("VDI.get_VBDs", vec![json!(vdi)], t)?) {
                        let user = self.call("VBD.get_VM", vec![json!(other)], t)?;
                        shared |= !going.iter().any(|g| Some(g.as_str()) == user.as_str());
                    }
                    if !shared {
                        let name = self.call("VDI.get_name_label", vec![json!(vdi)], t)?.as_str().unwrap_or_default().to_string();
                        vdis.push((vdi, name));
                    }
                }
            }
        }
        for snapshot in &going[1..] {
            self.call("VM.destroy", vec![json!(snapshot)], t)?;
        }
        self.call("VM.destroy", vec![json!(vm)], t)?;
        let mut left = vec![];
        for (vdi, name) in vdis {
            if let Err(e) = self.call("VDI.destroy", vec![json!(vdi)], 300_000) {
                left.push(format!("{}: {}", if name.is_empty() { vdi } else { name }, e.message()));
            }
        }
        Ok(left)
    }
    fn shutdown_host(&self, host: &str) -> Result<(), ApiError> {
        self.host_off(host, false)
    }
    fn reboot_host(&self, host: &str) -> Result<(), ApiError> {
        self.host_off(host, true)
    }

    /// VDI.create in the library (sm_config "path" names the file on the share), then the bytes in
    /// an HTTP PUT to /import_raw_vdi, then the task's verdict. XenServer answers the PUT before
    /// reading it, so only the task says whether the file was written.
    fn upload_iso(&self, sr_ref: &str, address: Option<&str>, src: &Source, name: &str, t: &Arc<Transfer>) -> Result<(), ApiError> {
        let (size, reader): (u64, Box<dyn Read + Send>) = match src {
            Source::File(file) => {
                let size = std::fs::metadata(file).map_err(|e| ApiError::other(format!("Couldn't read {}: {}", file.display(), e)))?.len();
                let f = std::fs::File::open(file).map_err(|e| ApiError::other(format!("Couldn't open {}: {}", file.display(), e)))?;
                (size, Box::new(std::io::BufReader::with_capacity(1 << 20, f)))
            }
            Source::Web { url, .. } => {
                let r = web_get(url).map_err(|e| ApiError::other(format!("Couldn't download it: {}", e)))?;
                let size = r.header("Content-Length").and_then(|v| v.trim().parse::<u64>().ok()).filter(|n| *n > 0);
                let size = size.ok_or_else(|| ApiError::other("The site no longer says how big the file is."))?;
                (size, Box::new(r.into_reader()))
            }
        };
        t.total.store(size, Ordering::Relaxed);
        let record = json!({
            "name_label": name, "name_description": "", "SR": sr_ref, "virtual_size": size, "type": "user",
            "sharable": false, "read_only": false, "other_config": {}, "xenstore_data": {}, "sm_config": { "path": name }, "tags": [],
        });
        let vdi = self.call("VDI.create", vec![record], 120_000)?.as_str().unwrap_or_default().to_string();
        let task = match self.call("task.create", vec![json!("Upload ISO"), json!(name)], 30_000) {
            Ok(v) => v.as_str().unwrap_or_default().to_string(),
            Err(e) => {
                let _ = self.call("VDI.destroy", vec![json!(vdi)], 60_000);
                return Err(e);
            }
        };
        let (coordinator, session) = {
            let st = self.state.lock().unwrap();
            (st.host.clone(), st.session.clone().unwrap_or_default())
        };
        let host = address.filter(|a| !a.is_empty()).map(str::to_string).unwrap_or(coordinator);
        let host = if host.contains(':') && !host.starts_with('[') { format!("[{}]", host) } else { host };
        let port = if self.port == DEFAULT_PORT { String::new() } else { format!(":{}", self.port) };
        let url = format!("https://{}{}/import_raw_vdi?session_id={}&vdi={}&task_id={}&format=raw", host, port, session, vdi, task);
        let agent = ureq::AgentBuilder::new()
            .tls_connector(self.tls.clone())
            .timeout_connect(Duration::from_secs(30))
            .timeout_write(Duration::from_secs(300))
            .timeout_read(Duration::from_secs(600))
            .redirects(0)
            .user_agent(concat!("SimpleVMManager/", env!("CARGO_PKG_VERSION")))
            .build();
        let body = Counting { inner: reader, t: t.clone() };
        let sent = agent.put(&url).set("Content-Length", &size.to_string()).send(body);
        t.finishing.store(true, Ordering::Relaxed);
        let cancelled = t.cancel.load(Ordering::Relaxed);
        let put_error = match sent {
            Ok(_) => None,
            Err(ureq::Error::Status(code, _)) => Some(ApiError::other(format!("The server answered the upload with HTTP {}.", code))),
            Err(e) => Some(ApiError::other(e.to_string())),
        };
        if cancelled || put_error.is_some() {
            let _ = self.call("task.cancel", vec![json!(task)], 30_000);
        }
        // a broken upload's task ends soon (or never started); a whole one may still be writing
        let wait = if cancelled || put_error.is_some() { 30 } else { 600 };
        let verdict = self.task_result(&task, Duration::from_secs(wait));
        let _ = self.call("task.destroy", vec![json!(task)], 30_000);
        let result = if cancelled {
            Err(ApiError { code: CANCELLED.into(), details: vec![] })
        } else {
            match verdict {
                Ok(true) => Ok(()),
                // the task failed for a reason of its own
                Err(e) if e.code != CANCELLED => Err(e),
                // still going, or cancelled above because the upload broke off
                _ => Err(put_error.unwrap_or_else(|| ApiError::other("The server didn't finish the upload in time."))),
            }
        };
        if result.is_err() {
            let _ = self.call("VDI.destroy", vec![json!(vdi)], 120_000); // takes the unfinished file away
        }
        result
    }

    /// The VM's VNC console through an HTTP CONNECT tunnel on the server's HTTPS port, as the
    /// original's console: CONNECT <console location>&session_id=<session> HTTP/1.0.
    fn open_console(&self, vm_ref: &str) -> Result<ConsoleStream, ApiError> {
        let t = 30_000;
        let mut location = String::new();
        for c in self.call("VM.get_consoles", vec![json!(vm_ref)], t)?.as_array().cloned().unwrap_or_default() {
            let rec = self.call("console.get_record", vec![c], t)?;
            if s(&rec, "protocol") == "rfb" {
                location = s(&rec, "location");
                break;
            }
        }
        if location.is_empty() {
            return Err(ApiError::other("There's no console to show here."));
        }
        let (session, conn_host) = {
            let st = self.state.lock().unwrap();
            (st.session.clone().unwrap_or_default(), st.host.clone())
        };
        // The location looks like https://<server address>/console?uuid=<console uuid>
        let rest = location.split_once("://").map_or(location.as_str(), |x| x.1);
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = split_host_port(authority);
        let port = port.unwrap_or(if host == conn_host { self.port } else { DEFAULT_PORT });
        let tcp = tcp_connect(&host, port)?;
        let _ = tcp.set_read_timeout(Some(Duration::from_secs(15)));
        let mut tls = self.tls.connect(&host, tcp).map_err(|e| ApiError::other(format!("Couldn't open a secure connection for the console ({}).", e)))?;
        let sep = if path.contains('?') { '&' } else { '?' };
        let request = format!("CONNECT {}{}session_id={} HTTP/1.0\r\n\r\n", path, sep, session);
        tls.write_all(request.as_bytes()).and_then(|_| tls.flush()).map_err(|e| ApiError::other(format!("Couldn't ask for the console ({}).", e)))?;
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match tls.read(&mut byte) {
                Ok(1) if head.len() < 16 * 1024 => head.push(byte[0]),
                Ok(_) => return Err(ApiError::other("The server closed the console connection.")),
                Err(e) => return Err(ApiError::other(format!("The server didn't open the console ({}).", e))),
            }
        }
        let status = String::from_utf8_lossy(&head).lines().next().unwrap_or_default().trim().to_string();
        if status.split_whitespace().nth(1) != Some("200") {
            return Err(ApiError::other(format!("The server refused to open the console ({}).", status)));
        }
        Ok(ConsoleStream::Tls(Box::new(tls)))
    }

    fn close(&self) {
        let (host, session) = {
            let mut st = self.state.lock().unwrap();
            (st.host.clone(), st.session.take())
        };
        if let Some(sess) = session {
            let _ = self.rpc(&host, 0, "session.logout", vec![json!(sess)], 5000);
        }
    }
}

// ---------------------------------------------------------------- demo pool

const GB: i64 = 1024 * 1024 * 1024;

/// A made-up UUID for demo objects (they only need to look different from each other).
fn demo_uuid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let mut x = nanos ^ COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let (a, b) = (next(), next());
    format!("{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}", a >> 32, (a >> 16) & 0xffff, a & 0xfff, 0x8000 | (b >> 48) & 0x3fff, b & 0xffff_ffff_ffff)
}

pub struct DemoConnection {
    data: Mutex<PoolData>,
    /// What its update channel has for its servers.
    updates: Mutex<PoolUpdates>,
}

impl DemoConnection {
    /// A server's VMs leave it: those on its own storage (or with nowhere to go) stop, the others
    /// move to another server that's on.
    fn host_off(&self, host: &str) -> Result<(), ApiError> {
        std::thread::sleep(Duration::from_millis(1800));
        let mut d = self.data.lock().unwrap();
        if d.host(host).is_none() {
            return Err(ApiError { code: "HANDLE_INVALID".into(), details: vec!["host".into(), host.into()] });
        }
        let other = d.hosts.iter().find(|h| h.rref != host && h.enabled && h.live).map(|h| h.rref.clone());
        for v in d.vms.iter_mut().filter(|v| v.host.as_deref() == Some(host) && matches!(v.power, PowerState::Running | PowerState::Paused)) {
            if v.storage_host.as_deref() == Some(host) || other.is_none() {
                v.power = PowerState::Halted;
                v.host = None;
            } else {
                v.host = other.clone();
            }
        }
        Ok(())
    }
    pub fn new() -> DemoConnection {
        let mut d = PoolData { name: "Demo Pool".into(), templates: demo_templates(), ..Default::default() };
        let host = |r: &str, name: &str, addr: &str, master: bool| HostInfo {
            rref: r.into(),
            uuid: demo_uuid(),
            name: name.into(),
            description: if master { "Pool coordinator".into() } else { "Pool member".into() },
            address: addr.into(),
            version: "8.4.0".into(),
            cpu_model: "Example 16-core processor @ 2.9 GHz".into(),
            cpu_count: 16,
            mem_total: 128 * GB,
            mem_free: 0,
            enabled: true,
            is_master: master,
            control_domain: format!("dom0-{}", r),
            live: true,
            unreachable: false,
            boot_time: Some(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) - if master { 34 * 86400 + 7380 } else { 9 * 86400 + 600 }),
            brand: "XenServer".into(),
            edition: "xenserver-premium".into(),
            license_name: String::new(),
            updates_applied: String::new(),
            pending_guidance: vec![],
            last_update: None,
            // till 31 July next year
            license_expiry: parse_xen_time(&format!("{}0731T00:00:00Z", 1971 + SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) / 31_556_952)),
            nics: vec![
                HostNic { device: "eth0".into(), network: "Pool-wide network 0".into(), ip: addr.into(), management: true, carrier: true, speed: 1000 },
                HostNic { device: "eth1".into(), network: "Storage network".into(), ip: addr.replace("10.20.", "10.30."), management: false, carrier: true, speed: 10000 },
                HostNic { device: "eth2".into(), network: "Pool-wide network 2".into(), ip: String::new(), management: false, carrier: false, speed: 0 },
            ],
        };
        d.hosts.push(host("host-a", "demo-host-01", "10.20.0.11", true));
        d.hosts.push(host("host-b", "demo-host-02", "10.20.0.12", false));
        let sr = |r: &str, name: &str, kind: &str, size: i64, used: i64, host: Option<&str>| SrInfo {
            rref: r.into(),
            uuid: demo_uuid(),
            name: name.into(),
            description: if host.is_none() { "Shared by every server in the pool".into() } else { "Disks on this server only".into() },
            kind: kind.into(),
            size,
            used,
            shared: host.is_none(),
            host: host.map(String::from),
            ..Default::default()
        };
        d.srs.push(sr("sr-local1", "Local storage", "lvm", 900 * GB, 0, Some("host-a")));
        d.srs.push(sr("sr-dvd1", "DVD drives", "udev", 4_700_000_000, 4_700_000_000, Some("host-a"))); // a disc in it
        d.srs.push(sr("sr-local2", "Local storage", "lvm", 900 * GB, 0, Some("host-b")));
        d.srs.push(sr("sr-dvd2", "DVD drives", "udev", 0, 0, Some("host-b"))); // empty: not listed
        d.srs.push(sr("sr-usb2", "Removable storage", "udev", 0, 0, Some("host-b"))); // empty: not listed
        d.srs.push(sr("sr-nfs", "Shared NFS storage", "nfs", 4096 * GB, 0, None));
        d.srs.push(SrInfo { location: "//nas/isos".into(), share: "Windows share (SMB 3.0)".into(), files: demo_isos("sr-iso"), ..sr("sr-iso", "ISO library", "iso", 250 * GB, 87 * GB, None) });
        // SVM_DEMO_LICENSE: the pool's license runs out soon, has run out, or is a trial
        if let Ok(how) = std::env::var("SVM_DEMO_LICENSE") {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            for h in d.hosts.iter_mut() {
                match how.as_str() {
                    "soon" => h.license_expiry = Some(now + 12 * 86_400 + 3600),
                    "expired" => h.license_expiry = Some(now - 5 * 86_400),
                    "trial" => (h.edition, h.license_expiry) = ("trial".into(), None),
                    _ => {}
                }
            }
        }
        // the servers named in SVM_DEMO_DISABLED are on but don't take VMs (left so by a restart)
        if let Ok(off) = std::env::var("SVM_DEMO_DISABLED") {
            for h in d.hosts.iter_mut().filter(|h| off.split(',').any(|x| x.trim() == h.rref)) {
                h.enabled = false;
            }
        }
        // the servers named in SVM_DEMO_FULL are small for their VMs: little memory left, more
        // virtual CPUs than real ones (to see their Overview's warnings)
        if let Ok(full) = std::env::var("SVM_DEMO_FULL") {
            for h in d.hosts.iter_mut().filter(|h| full.split(',').any(|x| x.trim() == h.rref)) {
                (h.mem_total, h.cpu_count) = (26 * GB, 8);
            }
        }
        // the server named in SVM_DEMO_HOST_DOWN went down without shutting down: the pool still
        // counts it as live, but it doesn't answer
        if let Ok(down) = std::env::var("SVM_DEMO_HOST_DOWN") {
            for h in d.hosts.iter_mut().filter(|h| down.split(',').any(|x| x.trim() == h.rref)) {
                h.unreachable = true;
            }
        }
        // mounted where it belongs; but the storage named in SVM_DEMO_UNMOUNTED (references,
        // comma-separated) is mounted nowhere, as after the servers started while the share was
        // off, and the one in SVM_DEMO_DETACHED has no connections at all, so Repair can be tried
        let listed = |var: &str, r: &str| std::env::var(var).map_or(false, |v| v.split(',').any(|x| x.trim() == r));
        let all: Vec<String> = d.hosts.iter().map(|h| h.rref.clone()).collect();
        for x in d.srs.iter_mut() {
            let on = !listed("SVM_DEMO_UNMOUNTED", &x.rref);
            x.links = x.host.as_ref().map_or(all.clone(), |h| vec![h.clone()]).into_iter().map(|h| (h, on)).collect();
            if listed("SVM_DEMO_DETACHED", &x.rref) {
                x.links.clear();
                x.location.clear();
                x.share.clear();
            }
        }
        d.networks.push(NetInfo { rref: "net-0".into(), uuid: demo_uuid(), name: "Pool-wide network 0".into(), description: String::new(), bridge: "xenbr0".into(), mtu: 1500 });
        d.networks.push(NetInfo { rref: "net-1".into(), uuid: demo_uuid(), name: "Storage network".into(), description: String::new(), bridge: "xenbr1".into(), mtu: 9000 });

        let vms: [(&str, &str, &str, PowerState, Option<&str>, &str, &str, i64, i64, &str, i64); 6] = [
            ("vm-web", "Web Server", "Public website front end", PowerState::Running, Some("host-a"), "Ubuntu 24.04 LTS", "10.20.1.21", 4, 4, "sr-nfs", 40),
            ("vm-db", "Database", "Main database server: PostgreSQL 15 for the website, the shop and the team's own tools, backed up to the NAS every night", PowerState::Running, Some("host-a"), "Rocky Linux 9", "10.20.1.22", 8, 16, "sr-nfs", 200),
            ("vm-mail", "Mail Relay", "Outgoing mail", PowerState::Running, Some("host-b"), "Debian 12", "10.20.1.23", 2, 2, "sr-local2", 20),
            ("vm-build", "Build Agent", "Builds and tests", PowerState::Suspended, Some("host-b"), "Windows Server 2022", "", 4, 8, "sr-nfs", 120),
            ("vm-desk", "Test Desktop", "For trying updates", PowerState::Halted, None, "Windows 11", "", 2, 4, "sr-local1", 64),
            ("vm-backup", "Backup Appliance", "Nightly backups", PowerState::Halted, None, "Debian 12", "", 2, 2, "sr-nfs", 500),
        ];
        for (i, (r, name, desc, p, host, os, ip, cpus, mem, srr, disk)) in vms.iter().enumerate() {
            let sr_name = d.sr(srr).map(|x| x.name.clone()).unwrap_or_default();
            let storage_host = d.sr(srr).and_then(|x| x.host.clone());
            if let Some(x) = d.srs.iter_mut().find(|x| x.rref == *srr) {
                x.used += disk * GB;
            }
            d.vms.push(VmInfo {
                rref: r.to_string(),
                uuid: demo_uuid(),
                name: name.to_string(),
                description: desc.to_string(),
                host: host.map(|h| h.to_string()),
                storage_host,
                affinity: None,
                os: os.to_string(),
                ip: ip.to_string(),
                power: *p,
                mem_static_max: mem * GB,
                mem_dyn_min: mem * GB / 2,
                mem_dyn_max: mem * GB,
                vcpus: *cpus,
                allowed: vec![],
                disks: vec![
                    VmDisk { name: format!("{} disk 0", name), device: "0".into(), vbd: format!("{}-vbd-0", r), vdi: format!("{}-vdi-0", r), sr: srr.to_string(), sr_name, kind: "Disk".into(), size: disk * GB },
                    VmDisk { name: "(empty)".into(), device: "3".into(), vbd: format!("{}-vbd-3", r), kind: "DVD drive".into(), ..Default::default() },
                ],
                nics: vec![VmNic { device: "0".into(), vif: format!("{}-vif-0", r), net: "net-0".into(), mac: format!("aa:16:3e:{:02x}:10:{:02x}", i + 1, 0x20 + i), network: "Pool-wide network 0".into(), ip: ip.to_string() }],
                ..Default::default()
            });
            demo_details(d.vms.last_mut().unwrap());
        }
        recompute(&mut d);
        // two templates made from VMs like these, ready to copy
        let made: [(&str, &str, &str, &str, i64, i64, i64); 2] = [
            ("tpl-web", "Ubuntu 24.04 web server", "nginx and PHP 8.3, generalized", "Ubuntu 24.04 LTS", 2, 4, 40),
            ("tpl-win", "Windows Server 2022 base", "Sysprepped, with the October updates", "Windows Server 2022", 4, 8, 80),
        ];
        for (r, name, description, os, cpus, mem, disk) in made {
            let mut t = VmInfo {
                rref: r.into(),
                uuid: demo_uuid(),
                name: name.into(),
                description: description.into(),
                os: os.into(),
                power: PowerState::Halted,
                vcpus: cpus,
                vcpus_max: cpus,
                mem_static_min: mem * GB / 2,
                mem_static_max: mem * GB,
                mem_dyn_min: mem * GB,
                mem_dyn_max: mem * GB,
                virt_mode: "HVM".into(),
                firmware: "UEFI".into(),
                boot_order: "Disk, DVD drive, Network".into(),
                ..Default::default()
            };
            t.disks.push(VmDisk { name: format!("{} disk 0", name), device: "0".into(), vbd: format!("{}-vbd-0", r), vdi: format!("{}-vdi-0", r), sr: "sr-nfs".into(), sr_name: "Shared NFS storage".into(), kind: "Disk".into(), size: disk * GB });
            t.nics.push(VmNic { device: "0".into(), vif: format!("{}-vif-0", r), net: "net-0".into(), mac: format!("aa:16:3e:7e:00:{:02x}", disk), network: "Pool-wide network 0".into(), ip: String::new() });
            d.user_templates.push(t);
        }
        // its update channel: fetched three days ago; two security updates among five, the second
        // server lacking four (SVM_DEMO_UPDATES=none: every server has every one)
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0);
        (d.update_channel, d.last_update_sync) = (Some("Normal".into()), Some(now - 3 * 86_400 - 5000));
        let none = std::env::var("SVM_DEMO_UPDATES").map_or(false, |v| v == "none");
        for h in d.hosts.iter_mut() {
            h.updates_applied = if none { "yes".into() } else { "no".into() };
        }
        let update = |id: &str, summary: &str, kind: &str, severity: &str, days: i64, guidance: &[&str], description: &str| UpdateInfo {
            id: id.into(),
            summary: summary.into(),
            description: description.into(),
            kind: kind.into(),
            severity: severity.into(),
            issued: Some((now - days * 86_400) / 86_400 * 86_400),
            url: format!("https://docs.xenserver.com/en-us/xenserver/8/whats-new/normal#{}", id.to_lowercase()),
            guidance: guidance.iter().map(|g| g.to_string()).collect(),
        };
        let updates = vec![
            update("XSU-2026-1104", "Xen hypervisor: fixes CVE-2026-31012 and CVE-2026-31013", "SecurityFix", "High", 6, &["RebootHost"], "A guest could read memory of other guests through a flaw in the handling of nested page tables."),
            update("XSU-2026-1101", "OpenSSL: fixes CVE-2026-2290", "SecurityFix", "None", 9, &["RestartToolstack"], "A crafted TLS handshake could make the management service stop answering."),
            update("XSU-2026-1029", "Storage migration no longer stalls at 99%", "BugFix", "None", 14, &["RestartToolstack"], "Moving a VM's disk between NFS repositories could wait for ever on its last step."),
            update("XSU-2026-1022", "Network drivers for Broadcom NetXtreme-E cards", "BugFix", "None", 21, &["RebootHost"], "Fixes link flaps under heavy load."),
            update("XSU-2026-1015", "XenServer VM Tools for Windows 9.4.2", "Improvement", "None", 28, &[], "The tools' ISO, for VMs to install."),
        ];
        let lacking = |ids: &[&str]| -> HostUpdates {
            let mut guidance: Vec<String> = vec![];
            for g in updates.iter().filter(|u| ids.contains(&u.id.as_str())).flat_map(|u| u.guidance.clone()) {
                if !guidance.contains(&g) {
                    guidance.push(g);
                }
            }
            HostUpdates { updates: ids.iter().map(|i| i.to_string()).collect(), guidance, packages: ids.len() * 3, ..Default::default() }
        };
        let hosts = if none {
            vec![HostUpdates { host: "host-a".into(), ..Default::default() }, HostUpdates { host: "host-b".into(), ..Default::default() }]
        } else {
            vec![
                HostUpdates { host: "host-a".into(), ..lacking(&["XSU-2026-1104", "XSU-2026-1101", "XSU-2026-1029", "XSU-2026-1022", "XSU-2026-1015"]) },
                HostUpdates { host: "host-b".into(), ..lacking(&["XSU-2026-1104", "XSU-2026-1101", "XSU-2026-1029", "XSU-2026-1015"]) },
            ]
        };
        let found = PoolUpdates { hash: "6f1c0e9a".into(), updates, hosts };
        DemoConnection { data: Mutex::new(d), updates: Mutex::new(found) }
    }

    /// New VM from one of the user's templates: the calls a real server gets, with made-up answers.
    fn demo_from_template(&self, v: &NewVm, tr: &Trace) -> Result<String, ApiError> {
        let tpl = self.data.lock().unwrap().user_template(&v.template).cloned().ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["VM".into(), v.template.clone()] })?;
        let rref = format!("vm-new-{}", self.data.lock().unwrap().vms.len());
        let me = format!("\"{}\"", v.name);
        let mut steps = vec![match &v.copy_to {
            None => format!("VM.clone  vm={}  new_name={}", named(&v.names, &v.template), me),
            Some(sr) => format!("VM.copy  vm={}  new_name={}  sr={}", named(&v.names, &v.template), me, named(&v.names, sr)),
        }];
        steps.push(format!("VM.set_is_a_template  vm={}  value=false", me));
        steps.push(format!("VM.set_VCPUs_at_startup  vm={}  value={}", me, v.vcpus));
        steps.push(format!("VM.set_memory_limits  vm={}  static_min={}  dynamic_min={}  dynamic_max={}  static_max={}", me, v.memory, v.memory, v.memory, v.memory));
        if !v.keep_nics {
            steps.push(format!("VIF.destroy  vif=OpaqueRef:{}-vif-0", tpl.rref));
            if let Some(net) = &v.network {
                steps.push(format!("VIF.create  VM={}  network={}  device=0  MTU=1500", me, named(&v.names, net)));
            }
        }
        demo_work(if v.copy_to.is_some() { 1500 } else { 300 });
        for asked in steps {
            tr.push(Said::Asked, asked);
            std::thread::sleep(Duration::from_millis(150));
            tr.push(Said::Answer, "OK  (0.1 s)");
        }
        let mut d = self.data.lock().unwrap();
        let mut vm = demo_copy(&tpl, &rref, &v.name, &v.description, v.copy_to.as_deref(), &d.srs);
        (vm.vcpus, vm.mem_static_max, vm.mem_dyn_min, vm.mem_dyn_max, vm.affinity) = (v.vcpus, v.memory, v.memory, v.memory, v.host.clone());
        if !v.keep_nics {
            let net = v.network.as_deref().and_then(|r| d.networks.iter().find(|n| n.rref == r));
            vm.nics = net.map(|n| VmNic { device: "0".into(), vif: format!("{}-vif-0", rref), net: n.rref.clone(), mac: "aa:bb:cc:00:11:23".into(), network: n.name.clone(), ip: String::new() }).into_iter().collect();
        }
        d.vms.push(vm);
        recompute(&mut d);
        Ok(rref)
    }
}

/// Some of XenServer's built-in templates, with what they set up.
fn demo_templates() -> Vec<TemplateInfo> {
    let t = |name: &str, uefi: bool, sb: &str, vtpm: bool, mem_gb: i64, vcpus: i64, disk_gb: i64| TemplateInfo {
        rref: format!("tpl-{}", name.to_lowercase().replace(|c: char| !c.is_ascii_alphanumeric(), "-")),
        name: name.into(),
        uefi,
        secure_boot: sb.into(),
        vtpm,
        supports_uefi: true,
        supports_secure_boot: name.starts_with("Windows") || name.starts_with("Ubuntu") || name.starts_with("Debian"),
        mem_min: mem_gb * GB,
        vcpus,
        disk: disk_gb * GB,
    };
    vec![
        t("Other install media", false, "", false, 1, 1, 0),
        t("Debian Bookworm 12", false, "", false, 1, 1, 10),
        t("Rocky Linux 9", false, "", false, 2, 1, 10),
        t("Ubuntu Jammy Jellyfish 22.04", false, "", false, 2, 1, 10),
        t("Ubuntu Noble Numbat 24.04", false, "", false, 2, 1, 10),
        t("Windows 10 (64-bit)", false, "", false, 2, 2, 32),
        t("Windows 11", true, "auto", true, 4, 2, 64),
        t("Windows Server 2022 (64-bit)", true, "auto", false, 2, 2, 32),
        t("Windows Server 2025 (64-bit)", true, "auto", false, 2, 2, 32),
    ]
}

/// The discs in the demo's ISO library `sr`, two of them described.
/// The demo's stand-in for a server task: `ms` of work, its progress reported to the thread's
/// watched `Progress`.
/// A VM's made-up ten minutes (see `DemoConnection::vm_perf`).
fn demo_perf(v: &VmInfo) -> Perf {
    let seed = v.name.bytes().map(|b| b as f64).sum::<f64>();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0) / 5 * 5;
    let mut p = Perf::default();
    for i in 0..120 {
        let t = now - (119 - i) * 5;
        let x = t as f64 / 37.0 + seed;
        let wave = |k: f64| ((x * k).sin() * 0.5 + 0.5) * ((x * k * 0.37 + 1.3).sin() * 0.3 + 0.7);
        p.times.push(t);
        p.cpu.push((0.08 + 0.35 * wave(1.0)).min(1.0));
        p.mem_total.push(v.mem_dyn_max as f64);
        p.mem_used.push(v.mem_dyn_max as f64 * (0.48 + 0.04 * wave(0.2)));
        p.disk_read.push(2.0e6 * wave(2.3).powi(3));
        p.disk_write.push(0.8e6 * wave(1.7).powi(2));
        p.net_rx.push(5.0e5 * wave(1.3));
        p.net_tx.push(1.5e5 * wave(0.9));
    }
    p.mem_inside = v.tools_installed;
    p
}

fn demo_work(ms: u64) {
    let watch = watched();
    let steps = (ms / 100).max(1);
    for i in 1..=steps {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(w) = &watch {
            w.set_fraction(i as f64 / steps as f64);
        }
    }
}

/// A demo VM or template made from `from` (a VM's or template's details), off, with its disks on
/// `sr` when given (a full copy) and new network cards' addresses.
fn demo_copy(from: &VmInfo, rref: &str, name: &str, description: &str, sr: Option<&str>, srs: &[SrInfo]) -> VmInfo {
    let mut v = from.clone();
    v.rref = rref.to_string();
    v.uuid = demo_uuid();
    v.name = name.to_string();
    v.description = description.to_string();
    (v.power, v.host, v.started, v.mem_actual, v.parent) = (PowerState::Halted, None, None, 0, None);
    v.snapshots.clear();
    v.allowed.clear();
    (v.ip, v.ips) = (String::new(), vec![]);
    for (i, n) in v.nics.iter_mut().enumerate() {
        n.ip.clear();
        n.mac = format!("aa:16:3e:{}:{:02x}", &rref[rref.len().saturating_sub(5)..].replace('-', "0"), i);
    }
    if let Some(x) = sr.and_then(|r| srs.iter().find(|x| x.rref == r)) {
        for k in v.disks.iter_mut().filter(|k| k.kind == "Disk") {
            (k.sr, k.sr_name) = (x.rref.clone(), x.name.clone());
        }
        v.storage_host = x.host.clone();
    }
    v
}

/// The demo's pretend mount: an address with "unreachable" in it doesn't answer, and the password
/// "wrong" is refused, so the error page can be tried out.
fn demo_mount(lib: &IsoLibrary) -> Result<(), ApiError> {
    let fail = |text: &str| {
        let e = ApiError { code: "SR_BACKEND_FAILURE_222".into(), details: vec![String::new(), text.into(), String::new()] };
        answered(&Err(e.clone()), Duration::from_millis(900));
        Err(e)
    };
    let host = if lib.nfs { lib.location.split(':').next().unwrap_or("") } else { lib.location.trim_start_matches('/').split('/').next().unwrap_or("") };
    if lib.location.contains("unreachable") {
        return if lib.nfs {
            fail(&format!("ISO mount error [opterr=mount failed with return code 32] mount.nfs: Connection timed out (server {})", host))
        } else {
            fail("ISO mount error [opterr=mount failed with return code 32] mount error(113): No route to host. Refer to the mount.cifs(8) manual page (e.g. man mount.cifs) and kernel log messages (dmesg)")
        };
    }
    if lib.password == "wrong" {
        return fail("ISO mount error [opterr=mount failed with return code 32] mount error(13): Permission denied. Refer to the mount.cifs(8) manual page (e.g. man mount.cifs) and kernel log messages (dmesg)");
    }
    Ok(())
}

fn demo_isos(sr: &str) -> Vec<IsoFile> {
    [
        ("debian-12.7.0-amd64-netinst.iso", 661, "Network install, for small servers"),
        ("rocky-9.4-x86_64-minimal.iso", 1_800, ""),
        ("ubuntu-24.04.1-live-server-amd64.iso", 2_700, "Ubuntu Server LTS, for the web servers"),
        ("Win11_24H2_English_x64.iso", 5_800, ""),
        ("windows-server-2025.iso", 5_200, ""),
        ("26100.1742.240906-0331.ge_release_svc_refresh_CLIENT_IOT_LTSC_EVAL_x64FRE_en-us.iso", 4_900, "Windows 11 IoT Enterprise LTSC, to try"),
    ]
    .iter()
    .enumerate()
    .map(|(i, (n, mb, d))| {
        // in the first library, Rocky sits in a shut-down VM's drive and Ubuntu in a running one's
        let in_drives = match (sr, i) {
            ("sr-iso", 1) => vec![("Test Desktop".to_string(), false)],
            ("sr-iso", 2) => vec![("Web Server".to_string(), true)],
            _ => vec![],
        };
        let can_delete = in_drives.iter().all(|d| !d.1);
        IsoFile { vdi: format!("{}-vdi-{}", sr, i), name: n.to_string(), size: mb * 1024 * 1024, description: d.to_string(), in_drives, can_delete }
    })
    .collect()
}

/// Made-up details for the demo VMs' General tab.
fn demo_details(v: &mut VmInfo) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let windows = v.os.contains("Windows");
    v.virt_mode = if v.rref == "vm-backup" { "PV" } else { "HVM" }.into();
    if v.virt_mode == "HVM" {
        v.firmware = if windows || v.rref == "vm-web" { "UEFI" } else { "BIOS" }.into();
        v.boot_order = "Disk, DVD drive, Network".into();
    }
    if v.firmware == "UEFI" {
        v.secure_boot = if windows { "On" } else { "Off" }.into();
    }
    v.vcpus_max = v.vcpus;
    v.mem_static_min = v.mem_dyn_min;
    let (tags, folder, days, auto, ha): (&[&str], &str, i64, bool, &str) = match v.rref.as_str() {
        "vm-web" => (&["web", "production"], "/Production", 12, true, "Restart"),
        "vm-db" => (&["database", "production"], "/Production", 31, true, "Restart"),
        "vm-mail" => (&["mail"], "/Production", 4, true, "Best effort"),
        "vm-build" => (&["ci"], "/Development", 0, false, ""),
        "vm-desk" => (&["testing"], "/Development", 0, false, ""),
        _ => (&["backup"], "", 0, false, ""),
    };
    // snapshots, oldest first: name, description, how long ago (hours), with memory, was running
    let snaps: &[(&str, &str, i64, bool, bool)] = match v.rref.as_str() {
        "vm-web" => &[
            ("Fresh install", "Ubuntu 24.04 with nginx, nothing else", 40 * 24 + 5, false, false),
            ("Before kernel update", "", 9 * 24 + 2, false, true),
            ("Before PHP 8.3", "Site working on PHP 8.2", 27, true, true),
        ],
        "vm-db" => &[("Before upgrade to 16", "PostgreSQL 15.8", 20 * 24, false, true), ("Before schema change", "", 50, false, true)],
        "vm-build" => &[("Clean agent", "", 15 * 24, false, false)],
        "vm-desk" => &[("Windows 11 clean", "", 60 * 24, false, false), ("Before 24H2", "", 30 * 24, false, false), ("Before driver update", "", 3 * 24, false, true)],
        _ => &[],
    };
    v.snapshots = snaps
        .iter()
        .rev()
        .enumerate()
        .map(|(i, (name, description, hours, memory, was_running))| SnapshotInfo {
            rref: format!("{}-snap-{}", v.rref, snaps.len() - i),
            name: name.to_string(),
            description: description.to_string(),
            time: Some(now - hours * 3600 - 17 * 60),
            memory: *memory,
            was_running: *was_running,
        })
        .collect();
    v.parent = v.snapshots.first().map(|x| x.rref.clone());
    v.tags = tags.iter().map(|t| t.to_string()).collect();
    v.folder = folder.into();
    v.auto_power_on = auto;
    v.ha = ha.into();
    v.tools_installed = v.rref != "vm-backup";
    v.tools = if !v.tools_installed { "" } else if windows { "9.4.0" } else { "8.4.0" }.into();
    v.kernel = if windows { String::new() } else { "6.8.0-45-generic".into() };
    if v.power == PowerState::Running {
        v.started = Some(now - days * 86_400 - 3 * 3600 - 17 * 60);
    }
}

/// Allowed operations from the power state (as a real server reports them), and each host's free memory.
fn recompute(d: &mut PoolData) {
    for (i, v) in d.vms.iter_mut().enumerate() {
        v.allowed = match v.power {
            PowerState::Halted => vec!["start", "snapshot"],
            PowerState::Running => vec!["clean_shutdown", "clean_reboot", "suspend", "hard_shutdown", "hard_reboot", "snapshot", "checkpoint"],
            PowerState::Suspended => vec!["resume", "hard_shutdown", "snapshot"],
            PowerState::Paused => vec!["hard_shutdown"],
            PowerState::Unknown => vec![],
        }
        .into_iter()
        .map(String::from)
        .collect();
        if v.power != PowerState::Running {
            v.ip.clear();
        } else if v.ip.is_empty() {
            v.ip = format!("10.20.1.{}", 30 + i);
        }
        v.ips = if v.ip.is_empty() { vec![] } else { vec![v.ip.clone()] };
        if matches!(v.power, PowerState::Running | PowerState::Paused) {
            v.mem_actual = v.mem_dyn_max;
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            v.started.get_or_insert(now);
        } else if v.power == PowerState::Halted {
            v.mem_actual = 0;
            v.started = None;
        }
        let ip = v.ip.clone();
        for nic in v.nics.iter_mut() {
            nic.ip = ip.clone();
        }
    }
    let used: Vec<(String, i64)> = d
        .hosts
        .iter()
        .map(|h| {
            let vms: i64 = d.vms.iter().filter(|v| v.host.as_deref() == Some(h.rref.as_str()) && matches!(v.power, PowerState::Running | PowerState::Paused)).map(|v| v.mem_dyn_max).sum();
            (h.rref.clone(), 4 * GB + vms)
        })
        .collect();
    for (r, u) in used {
        if let Some(h) = d.hosts.iter_mut().find(|h| h.rref == r) {
            h.mem_free = (h.mem_total - u).max(0);
        }
    }
}

fn pick_host(d: &mut PoolData, need: i64) -> Option<String> {
    recompute(d);
    d.hosts.iter().filter(|h| h.mem_free >= need).max_by_key(|h| h.mem_free).map(|h| h.rref.clone())
}

impl Connection for DemoConnection {
    fn hostname(&self) -> String {
        "Demo server".into()
    }
    fn username(&self) -> String {
        "demo".into()
    }
    fn is_demo(&self) -> bool {
        true
    }
    fn fetch(&self, _t: u64) -> Result<PoolData, ApiError> {
        std::thread::sleep(Duration::from_millis(150));
        Ok(self.data.lock().unwrap().clone())
    }
    fn run_action(&self, vm_ref: &str, a: &VmAction) -> Result<(), ApiError> {
        said(a.method, &[json!(vm_ref)]);
        std::thread::sleep(Duration::from_millis(DEMO_ACTION_MS));
        let mut d = self.data.lock().unwrap();
        let Some(idx) = d.vms.iter().position(|v| v.rref == vm_ref) else {
            return Err(ApiError { code: "HANDLE_INVALID".into(), details: vec!["VM".into(), vm_ref.into()] });
        };
        if !d.vms[idx].can(a.op) {
            return Err(ApiError { code: "VM_BAD_POWER_STATE".into(), details: vec![] });
        }
        let need = d.vms[idx].mem_dyn_max;
        match a.op {
            "start" => {
                // A VM whose disks are on a server's local storage can only start on that server.
                let target = match d.vms[idx].storage_host.clone() {
                    Some(h) => {
                        recompute(&mut d);
                        d.host(&h).filter(|x| x.mem_free >= need).map(|x| x.rref.clone())
                    }
                    None => pick_host(&mut d, need),
                };
                let Some(h) = target else {
                    return Err(ApiError { code: "HOST_NOT_ENOUGH_FREE_MEMORY".into(), details: vec![] });
                };
                d.vms[idx].power = PowerState::Running;
                d.vms[idx].host = Some(h);
            }
            "clean_shutdown" | "hard_shutdown" => {
                d.vms[idx].power = PowerState::Halted;
                d.vms[idx].host = None;
            }
            "suspend" => d.vms[idx].power = PowerState::Suspended,
            "resume" => {
                if d.vms[idx].host.is_none() {
                    d.vms[idx].host = pick_host(&mut d, need);
                }
                d.vms[idx].power = PowerState::Running;
            }
            _ => {} // reboots: stays running
        }
        recompute(&mut d);
        Ok(())
    }
    /// Pretends to mount the share: see `demo_mount`.
    fn create_iso_library(&self, lib: &IsoLibrary) -> Result<(), ApiError> {
        std::thread::sleep(Duration::from_millis(900));
        demo_mount(lib)?;
        let mut d = self.data.lock().unwrap();
        let rref = format!("sr-iso-{}", d.srs.len());
        let files = demo_isos(&rref);
        let links = d.hosts.iter().map(|h| (h.rref.clone(), true)).collect();
        d.srs.push(SrInfo {
            rref,
            uuid: demo_uuid(),
            name: lib.name.clone(),
            description: lib.description.clone(),
            kind: "iso".into(),
            size: 2048 * GB,
            used: 310 * GB,
            shared: lib.shared,
            host: None,
            location: lib.location.clone(),
            share: if lib.nfs { format!("NFS (v{})", lib.nfs_version) } else { format!("Windows share (SMB {})", lib.smb_version) },
            files,
            links,
        });
        Ok(())
    }
    /// Mounts it everywhere in about a second and a half; but a storage named in
    /// SVM_DEMO_REPAIR_FAILS doesn't answer on the coordinator.
    fn repair_sr(&self, sr_ref: &str) -> Result<Vec<(String, Result<(), ApiError>)>, ApiError> {
        said("PBD.plug", &[json!(format!("{}-pbd-host-a", sr_ref))]);
        std::thread::sleep(Duration::from_millis(1500));
        let mut d = self.data.lock().unwrap();
        let names: HashMap<String, String> = d.hosts.iter().map(|h| (h.rref.clone(), h.name.clone())).collect();
        let Some(x) = d.srs.iter_mut().find(|x| x.rref == sr_ref) else {
            return Err(ApiError { code: "HANDLE_INVALID".into(), details: vec!["SR".into(), sr_ref.into()] });
        };
        if x.links.is_empty() {
            return Err(ApiError { code: "SR_HAS_NO_PBDS".into(), details: vec![sr_ref.into()] });
        }
        if std::env::var("SVM_DEMO_REPAIR_FAILS").map_or(false, |v| v.split(',').any(|r| r.trim() == sr_ref)) {
            let lib = IsoLibrary { location: "//unreachable/isos".into(), ..Default::default() };
            return Ok(vec![(names["host-a"].clone(), demo_mount(&lib))]);
        }
        let mut out = vec![];
        for l in x.links.iter_mut().filter(|l| !l.1) {
            l.1 = true;
            out.push((names.get(&l.0).cloned().unwrap_or_default(), Ok(())));
        }
        Ok(out)
    }
    fn reattach_sr(&self, sr_ref: &str, lib: &IsoLibrary) -> Result<(), ApiError> {
        said("PBD.create", &[json!({ "SR": sr_ref, "device_config": { "location": lib.location, "cifspassword": lib.password } })]);
        std::thread::sleep(Duration::from_millis(1200));
        demo_mount(lib)?;
        let mut d = self.data.lock().unwrap();
        let all: Vec<String> = d.hosts.iter().map(|h| h.rref.clone()).collect();
        if let Some(x) = d.srs.iter_mut().find(|x| x.rref == sr_ref) {
            x.links = all.into_iter().map(|h| (h, true)).collect();
            x.location = lib.location.clone();
            x.share = if lib.nfs { format!("NFS (v{})", lib.nfs_version) } else { format!("Windows share (SMB {})", lib.smb_version) };
        }
        Ok(())
    }
    fn scan_sr(&self, _sr_ref: &str) -> Result<(), ApiError> {
        said("SR.scan", &[json!(_sr_ref)]);
        std::thread::sleep(Duration::from_millis(600));
        Ok(())
    }
    fn forget_sr(&self, sr_ref: &str) -> Result<(), ApiError> {
        said("SR.forget", &[json!(sr_ref)]);
        std::thread::sleep(Duration::from_millis(600));
        let mut d = self.data.lock().unwrap();
        if d.vms.iter().any(|v| v.power == PowerState::Running && v.disks.iter().any(|k| k.kind != "Disk" && d.sr(sr_ref).map_or(false, |x| x.name == k.sr_name))) {
            return Err(ApiError { code: "VDI_IN_USE".into(), details: vec![] });
        }
        d.srs.retain(|x| x.rref != sr_ref);
        Ok(())
    }
    /// Answers in under a second, but the storage named in SVM_DEMO_OFFLINE (a comma-separated
    /// list of references, such as sr-iso) is offline, so the red box can be tried out.
    fn check_sr(&self, sr: &str, host: Option<&str>, names: &HashMap<String, String>, tr: &Trace) -> Result<(), ApiError> {
        let on = host.map(|h| named(names, h)).unwrap_or_else(|| "a server".into());
        tr.push(Said::Asked, format!("SR.get_PBDs  SR={}", named(names, sr)));
        std::thread::sleep(Duration::from_millis(150));
        tr.push(Said::Answer, format!("plugged in on {}  (0.1 s)", on));
        tr.push(Said::Asked, format!("SR.scan  SR={}", named(names, sr)));
        if std::env::var("SVM_DEMO_OFFLINE").map_or(false, |v| v.split(',').any(|x| x.trim() == sr)) {
            std::thread::sleep(Duration::from_millis(2500));
            tr.push(Said::Error, format!("no answer in {} s", CHECK_SECS));
            return Err(ApiError { code: NOT_ANSWERING.into(), details: vec![names.get(sr).cloned().unwrap_or_default()] });
        }
        std::thread::sleep(Duration::from_millis(250));
        tr.push(Said::Answer, "OK  (0.3 s)");
        Ok(())
    }
    /// Pretends to send the file in about four seconds; a name already in the library is refused
    /// the way the server's ISO driver refuses it.
    fn upload_iso(&self, sr_ref: &str, _address: Option<&str>, src: &Source, name: &str, t: &Arc<Transfer>) -> Result<(), ApiError> {
        let size = match src {
            Source::File(file) => std::fs::metadata(file).map(|m| m.len()).unwrap_or(700 * 1024 * 1024),
            Source::Web { size, .. } => *size,
        };
        t.total.store(size, Ordering::Relaxed);
        if self.data.lock().unwrap().sr(sr_ref).map_or(false, |x| x.files.iter().any(|f| f.name == name)) {
            return Err(ApiError { code: "SR_BACKEND_FAILURE_79".into(), details: vec![String::new(), "VDI Invalid size [opterr=The VDI already exists]".into(), String::new()] });
        }
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(4) {
            if t.cancel.load(Ordering::Relaxed) {
                return Err(ApiError { code: CANCELLED.into(), details: vec![] });
            }
            let part = start.elapsed().as_secs_f64() / 4.0;
            t.sent.store((size as f64 * part) as u64, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(50));
        }
        t.sent.store(size, Ordering::Relaxed);
        t.finishing.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(700));
        let mut d = self.data.lock().unwrap();
        let x = d.srs.iter_mut().find(|x| x.rref == sr_ref).ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["SR".into(), sr_ref.into()] })?;
        let vdi = format!("{}-vdi-{}", sr_ref, x.files.len() + 100);
        x.files.push(IsoFile { vdi, name: name.to_string(), size: size as i64, can_delete: true, ..Default::default() });
        x.files.sort_by_key(|f| f.name.to_lowercase());
        Ok(())
    }
    /// Adds the VM to the demo pool, started on the server with the most memory free if asked.
    fn create_vm(&self, v: &NewVm, tr: &Trace) -> Result<String, ApiError> {
        for sr in &v.check {
            self.check_sr(sr, v.host.as_deref(), &v.names, tr)?;
        }
        if v.from_template {
            return self.demo_from_template(v, tr);
        }
        // the calls a real server gets, a few of them, with made-up answers
        let rref = format!("vm-new-{}", self.data.lock().unwrap().vms.len());
        let me = format!("\"{}\"", v.name);
        let steps = [
            (format!("VM.clone  vm={}  new_name={}", named(&v.names, &v.template), me), format!("OpaqueRef:{}", rref)),
            (format!("VM.provision  vm={}", me), "OK".into()),
            (format!("VM.set_VCPUs_at_startup  vm={}  value={}", me, v.vcpus), "OK".into()),
            (format!("VM.set_memory_limits  vm={}  static_min={}  dynamic_min={}  dynamic_max={}  static_max={}", me, v.memory, v.memory, v.memory, v.memory), "OK".into()),
            (format!("VDI.create  SR={}  name_label=\"{} disk\"  virtual_size={}  type=system", named(&v.names, &v.sr), v.name, v.disk), "OpaqueRef:demo-disk".into()),
            (format!("VBD.create  VM={}  VDI=\"{} disk\"  userdevice=0  type=Disk  mode=RW  bootable=false", me, v.name), "OpaqueRef:demo-vbd-0".into()),
            (format!("VBD.create  VM={}  VDI={}  userdevice=3  type=CD  mode=RO  bootable=true", me, named(&v.names, &v.iso)), "OpaqueRef:demo-vbd-3".into()),
        ];
        for (asked, answer) in steps {
            tr.push(Said::Asked, asked);
            std::thread::sleep(Duration::from_millis(150));
            tr.push(Said::Answer, format!("{}  (0.1 s)", answer));
        }
        if let Some(net) = &v.network {
            tr.push(Said::Asked, format!("VIF.create  VM={}  network={}  device=0  MTU=1500", me, named(&v.names, net)));
            tr.push(Said::Answer, "OpaqueRef:demo-vif-0  (0.1 s)");
        }
        let mut d = self.data.lock().unwrap();
        let template = d.templates.iter().find(|t| t.rref == v.template).map(|t| t.name.clone()).unwrap_or_default();
        let iso = d.srs.iter().flat_map(|x| x.files.iter().map(move |f| (x, f))).find(|(_, f)| f.vdi == v.iso).map(|(x, f)| (x.name.clone(), f.name.clone(), f.size));
        let sr_name = d.sr(&v.sr).map(|x| x.name.clone()).unwrap_or_default();
        let network = v.network.as_deref().and_then(|r| d.networks.iter().find(|n| n.rref == r)).map(|n| n.name.clone());
        let mut vm = VmInfo {
            rref: rref.clone(),
            uuid: demo_uuid(),
            name: v.name.clone(),
            description: v.description.clone(),
            os: if template == OTHER_MEDIA { String::new() } else { template },
            power: PowerState::Halted,
            vcpus: v.vcpus,
            mem_static_max: v.memory,
            mem_dyn_min: v.memory,
            mem_dyn_max: v.memory,
            affinity: v.host.clone(),
            storage_host: d.sr(&v.sr).and_then(|x| x.host.clone()),
            allowed: vec!["start".into(), "start_on".into()],
            ..Default::default()
        };
        vm.disks.push(VmDisk { device: "0".into(), name: format!("{} disk", v.name), vbd: format!("{}-vbd-0", rref), vdi: format!("{}-vdi-0", rref), sr: v.sr.clone(), sr_name, kind: "Disk".into(), size: v.disk });
        if let Some((lib, file, size)) = iso {
            vm.disks.push(VmDisk { device: "3".into(), name: file, vbd: format!("{}-vbd-3", rref), vdi: v.iso.clone(), sr_name: lib, kind: "DVD drive".into(), size, ..Default::default() });
        }
        if let Some(net) = network {
            vm.nics.push(VmNic { device: "0".into(), vif: format!("{}-vif-0", rref), net: v.network.clone().unwrap_or_default(), mac: "aa:bb:cc:00:11:22".into(), network: net, ip: String::new() });
        }
        d.vms.push(vm);
        recompute(&mut d);
        drop(d);
        Ok(rref)
    }
    fn start_traced(&self, vm: &str, label: &str, tr: &Trace) -> Result<(), ApiError> {
        tr.push(Said::Asked, format!("VM.start  vm={}  start_paused=false  force=false", label));
        let r = self.run_action(vm, ACTIONS.iter().find(|a| a.op == "start").unwrap());
        tr.outcome(&r.as_ref().map(|_| Value::Null).map_err(|e| e.clone()), Duration::from_millis(DEMO_ACTION_MS));
        r
    }
    /// A disc in a running VM's drive can't go, as on a real server.
    fn delete_iso(&self, vdi_ref: &str) -> Result<(), ApiError> {
        std::thread::sleep(Duration::from_millis(500));
        let mut d = self.data.lock().unwrap();
        let x = d.srs.iter_mut().find(|x| x.files.iter().any(|f| f.vdi == vdi_ref));
        let x = x.ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["VDI".into(), vdi_ref.into()] })?;
        if x.files.iter().any(|f| f.vdi == vdi_ref && !f.held_by().is_empty()) {
            return Err(ApiError { code: "VDI_IN_USE".into(), details: vec![vdi_ref.into(), "destroy".into()] });
        }
        x.files.retain(|f| f.vdi != vdi_ref);
        Ok(())
    }
    /// Takes the VM out of the demo pool (a running one is "turned off" first, taking longer).
    fn delete_vm(&self, vm_ref: &str, _disks: bool) -> Result<Vec<String>, ApiError> {
        said("VM.destroy", &[json!(vm_ref)]);
        if self.data.lock().unwrap().user_template(vm_ref).is_some() {
            std::thread::sleep(Duration::from_millis(700));
            self.data.lock().unwrap().user_templates.retain(|t| t.rref != vm_ref);
            return Ok(vec![]);
        }
        let running = self.data.lock().unwrap().vm(vm_ref).map(|v| v.power != PowerState::Halted);
        let Some(running) = running else {
            return Err(ApiError { code: "HANDLE_INVALID".into(), details: vec!["VM".into(), vm_ref.into()] });
        };
        std::thread::sleep(Duration::from_millis(if running { 1500 } else { 700 }));
        self.data.lock().unwrap().vms.retain(|v| v.rref != vm_ref);
        Ok(vec![])
    }
    /// Its VMs on its own storage stop, the others move to another server, and it's off.
    fn shutdown_host(&self, host: &str) -> Result<(), ApiError> {
        self.host_off(host)?;
        let mut d = self.data.lock().unwrap();
        let h = d.hosts.iter_mut().find(|h| h.rref == host).ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["host".into(), host.into()] })?;
        (h.enabled, h.live, h.mem_free) = (false, false, 0);
        Ok(())
    }
    /// As `shutdown_host`, but it's back on at once (its VMs stay where they went).
    fn reboot_host(&self, host: &str) -> Result<(), ApiError> {
        self.host_off(host)
    }
    fn set_vdi_description(&self, vdi_ref: &str, text: &str) -> Result<(), ApiError> {
        std::thread::sleep(Duration::from_millis(400));
        let mut d = self.data.lock().unwrap();
        let f = d.srs.iter_mut().flat_map(|x| x.files.iter_mut()).find(|f| f.vdi == vdi_ref);
        let f = f.ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["VDI".into(), vdi_ref.into()] })?;
        f.description = text.to_string();
        Ok(())
    }
    fn set_vm_description(&self, vm_ref: &str, text: &str) -> Result<(), ApiError> {
        std::thread::sleep(Duration::from_millis(400));
        let mut d = self.data.lock().unwrap();
        for v in d.vms.iter_mut() {
            if v.rref == vm_ref {
                v.description = text.to_string();
                return Ok(());
            }
            if let Some(x) = v.snapshots.iter_mut().find(|x| x.rref == vm_ref) {
                x.description = text.to_string();
                return Ok(());
            }
        }
        Err(ApiError { code: "HANDLE_INVALID".into(), details: vec!["VM".into(), vm_ref.into()] })
    }
    /// About a second (two and a half with memory).
    fn snapshot_vm(&self, vm_ref: &str, name: &str, description: &str, memory: bool) -> Result<String, ApiError> {
        said(if memory { "VM.checkpoint" } else { "VM.snapshot" }, &[json!(vm_ref), json!(name)]);
        demo_work(if memory { 2500 } else { 1200 });
        let mut d = self.data.lock().unwrap();
        let v = d.vms.iter_mut().find(|v| v.rref == vm_ref).ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["VM".into(), vm_ref.into()] })?;
        if !v.can(if memory { "checkpoint" } else { "snapshot" }) {
            return Err(ApiError { code: "VM_BAD_POWER_STATE".into(), details: vec![] });
        }
        let rref = format!("snap-{}", demo_uuid());
        let time = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).ok();
        let was_running = v.power == PowerState::Running;
        v.snapshots.insert(0, SnapshotInfo { rref: rref.clone(), name: name.into(), description: description.into(), time, memory, was_running });
        v.parent = Some(rref.clone());
        Ok(rref)
    }
    /// Half a second (a disk a second and a half), as a real server would take it.
    fn edit_vm(&self, vm: &str, change: &VmChange) -> Result<(), ApiError> {
        let method = match change {
            VmChange::Name(_) => "VM.set_name_label",
            VmChange::Description(_) => "VM.set_name_description",
            VmChange::AutoPowerOn(_) => "VM.add_to_other_config",
            VmChange::BootOrder(_) => "VM.add_to_HVM_boot_params",
            VmChange::Resources { .. } => "VM.set_memory_limits",
            VmChange::AddDisk { .. } => "VDI.create",
            VmChange::GrowDisk { .. } => "VDI.resize",
            VmChange::RemoveDisk { .. } => "VBD.destroy",
            VmChange::Dvd { iso: Some(_), .. } => "VBD.insert",
            VmChange::Dvd { iso: None, .. } => "VBD.eject",
            VmChange::AddNic { .. } => "VIF.create",
            VmChange::MoveNic { .. } => "VIF.move",
            VmChange::RemoveNic { .. } => "VIF.destroy",
        };
        said(method, &[json!(vm)]);
        demo_work(if matches!(change, VmChange::AddDisk { .. } | VmChange::GrowDisk { .. }) { 1500 } else { 500 });
        let mut d = self.data.lock().unwrap();
        let (nets, srs) = (d.networks.clone(), d.srs.clone());
        let v = d.vms.iter_mut().find(|v| v.rref == vm).ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["VM".into(), vm.into()] })?;
        let off = v.power == PowerState::Halted;
        let bad = || Err(ApiError { code: "VM_BAD_POWER_STATE".into(), details: vec![] });
        let next = |used: Vec<&str>, skip: &[&str]| (0..16).map(|i| i.to_string()).find(|d| !used.contains(&d.as_str()) && !skip.contains(&d.as_str())).unwrap_or_default();
        match change {
            VmChange::Name(n) => v.name = n.clone(),
            VmChange::Description(x) => v.description = x.clone(),
            VmChange::AutoPowerOn(on) => v.auto_power_on = *on,
            VmChange::BootOrder(o) => v.boot_order = boot_text(o),
            VmChange::Resources { vcpus, memory, .. } => {
                if !off {
                    return bad();
                }
                (v.vcpus, v.vcpus_max, v.mem_static_max, v.mem_dyn_min, v.mem_dyn_max) = (*vcpus, *vcpus, *memory, *memory, *memory);
            }
            VmChange::AddDisk { name, size, sr } => {
                let device = next(v.disks.iter().map(|k| k.device.as_str()).collect(), &["3"]);
                let sr_name = srs.iter().find(|x| x.rref == *sr).map(|x| x.name.clone()).unwrap_or_default();
                v.disks.push(VmDisk { name: name.clone(), device: device.clone(), vbd: format!("{}-vbd-{}", vm, device), vdi: format!("{}-vdi-{}", vm, device), sr: sr.clone(), sr_name, kind: "Disk".into(), size: *size });
                v.disks.sort_by(|a, b| a.device.cmp(&b.device));
            }
            VmChange::GrowDisk { vdi, size } => {
                if !off {
                    return bad();
                }
                if let Some(k) = v.disks.iter_mut().find(|k| k.vdi == *vdi) {
                    k.size = k.size.max(*size);
                }
            }
            VmChange::RemoveDisk { vbd, .. } => {
                if !off {
                    return bad();
                }
                v.disks.retain(|k| k.vbd != *vbd);
            }
            VmChange::Dvd { vbd, iso } => {
                let file = iso.as_deref().and_then(|i| srs.iter().find_map(|x| x.files.iter().find(|f| f.vdi == i).map(|f| (x.name.clone(), f.name.clone(), f.size))));
                if let Some(k) = v.disks.iter_mut().find(|k| k.vbd == *vbd) {
                    match file {
                        Some((lib, name, size)) => (k.name, k.sr_name, k.size, k.vdi) = (name, lib, size, iso.clone().unwrap_or_default()),
                        None => (k.name, k.sr_name, k.size, k.vdi) = ("(empty)".into(), String::new(), 0, String::new()),
                    }
                }
            }
            VmChange::AddNic { net } => {
                let device = next(v.nics.iter().map(|n| n.device.as_str()).collect(), &[]);
                let name = nets.iter().find(|n| n.rref == *net).map(|n| n.name.clone()).unwrap_or_default();
                v.nics.push(VmNic { device: device.clone(), vif: format!("{}-vif-{}", vm, device), net: net.clone(), mac: format!("aa:16:3e:5c:00:{:02}", device.parse::<u8>().unwrap_or(0)), network: name, ip: String::new() });
            }
            VmChange::MoveNic { vif, net } => {
                let name = nets.iter().find(|n| n.rref == *net).map(|n| n.name.clone()).unwrap_or_default();
                if let Some(n) = v.nics.iter_mut().find(|n| n.vif == *vif) {
                    (n.net, n.network) = (net.clone(), name);
                }
            }
            VmChange::RemoveNic { vif } => v.nics.retain(|n| n.vif != *vif),
        }
        Ok(())
    }
    /// A made-up but lively ten minutes: CPU and traffic going up and down, memory steady.
    fn vm_perf(&self, uuid: &str, _host_address: &str) -> Result<Perf, ApiError> {
        std::thread::sleep(Duration::from_millis(150));
        let d = self.data.lock().unwrap();
        let v = d.vms.iter().find(|v| v.uuid == uuid).ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["VM".into(), uuid.into()] })?;
        Ok(demo_perf(v))
    }
    /// Its running VMs' made-up ten minutes, and the server's from theirs: their CPUs on its own
    /// and a little of its own, its memory as it stands give or take, their traffic on its first
    /// card and their disks' on its storage card.
    fn host_perf(&self, uuid: &str, _host_address: &str) -> Result<HostPerf, ApiError> {
        std::thread::sleep(Duration::from_millis(150));
        let d = self.data.lock().unwrap();
        let h = d.hosts.iter().find(|h| h.uuid == uuid).ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["host".into(), uuid.into()] })?;
        let vms: Vec<&VmInfo> = d.vms.iter().filter(|v| v.host.as_deref() == Some(h.rref.as_str()) && v.power == PowerState::Running).collect();
        let mut out = HostPerf { vms: vms.iter().map(|v| (v.uuid.clone(), demo_perf(v))).collect(), ..Default::default() };
        let times = demo_perf(&VmInfo::default()).times;
        let mut nics: Vec<(String, Vec<f64>, Vec<f64>)> = h.nics.iter().map(|n| (n.device.clone(), vec![], vec![])).collect();
        let p = &mut out.host;
        p.mem_inside = true;
        for (i, t) in times.iter().enumerate() {
            let x = *t as f64 / 29.0 + h.cpu_count as f64;
            let own = (x.sin() * 0.5 + 0.5) * 0.4 + 0.2;
            let busy: f64 = vms.iter().zip(&out.vms).map(|(v, (_, q))| q.cpu[i] * v.vcpus as f64).sum();
            let sum = |f: fn(&Perf) -> &Vec<f64>| -> f64 { out.vms.iter().map(|(_, q)| f(q)[i]).sum() };
            p.times.push(*t);
            p.cpu.push(((busy + own) / h.cpu_count.max(1) as f64).min(1.0));
            p.mem_total.push(h.mem_total as f64);
            p.mem_used.push((h.mem_total - h.mem_free) as f64 + 0.3e9 * own);
            p.disk_read.push(sum(|q| &q.disk_read));
            p.disk_write.push(sum(|q| &q.disk_write));
            let (rx, tx) = (sum(|q| &q.net_rx) + 3.0e3 * own, sum(|q| &q.net_tx) + 2.0e3 * own);
            for n in nics.iter_mut() {
                let (a, b) = match n.0.as_str() {
                    "eth0" => (rx, tx),
                    "eth1" => (p.disk_write[i] * 1.02, p.disk_read[i] * 1.02),
                    _ => (0.0, 0.0),
                };
                n.1.push(a);
                n.2.push(b);
            }
            p.net_rx.push(nics.iter().map(|n| n.1[i]).sum());
            p.net_tx.push(nics.iter().map(|n| n.2[i]).sum());
        }
        out.nics = nics;
        Ok(out)
    }
    fn pool_updates(&self) -> Result<PoolUpdates, ApiError> {
        said("GET /updates", &[]);
        std::thread::sleep(Duration::from_millis(300));
        Ok(self.updates.lock().unwrap().clone())
    }
    /// A few seconds, as if fetching from the channel.
    fn sync_updates(&self) -> Result<String, ApiError> {
        said("pool.sync_updates", &[json!("OpaqueRef:demo-pool"), json!(false)]);
        demo_work(3000);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0);
        self.data.lock().unwrap().last_update_sync = Some(now);
        Ok(self.updates.lock().unwrap().hash.clone())
    }
    fn update_readiness(&self) -> Result<Vec<String>, ApiError> {
        said("pool.check_update_readiness", &[json!("OpaqueRef:demo-pool"), json!(false)]);
        std::thread::sleep(Duration::from_millis(600));
        Ok(vec![])
    }
    /// A couple of seconds; then it lacks none, and must restart (or restart its toolstack) when
    /// they asked for that.
    fn apply_updates(&self, host: &str, hash: &str) -> Result<Vec<String>, ApiError> {
        said("host.apply_updates", &[json!(host), json!(hash)]);
        demo_work(2500);
        let mut u = self.updates.lock().unwrap();
        if hash != u.hash {
            return Err(ApiError { code: "UPDATEINFO_HASH_MISMATCH".into(), details: vec![] });
        }
        let Some(h) = u.hosts.iter_mut().find(|h| h.host == host) else { return Err(ApiError { code: "HANDLE_INVALID".into(), details: vec!["host".into(), host.into()] }) };
        let pending: Vec<String> = if h.guidance.iter().any(|g| g == "RebootHost") {
            vec!["reboot_host".into()]
        } else if h.guidance.iter().any(|g| g == "RestartToolstack") {
            vec!["restart_toolstack".into()]
        } else {
            vec![]
        };
        *h = HostUpdates { host: host.into(), ..Default::default() };
        let mut d = self.data.lock().unwrap();
        if let Some(h) = d.hosts.iter_mut().find(|h| h.rref == host) {
            (h.pending_guidance, h.updates_applied) = (pending, "yes".into());
            h.last_update = Some(SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0));
        }
        Ok(vec![])
    }
    fn pending_guidance(&self, host: &str) -> Result<Vec<String>, ApiError> {
        said("host.get_pending_guidances", &[json!(host)]);
        Ok(self.data.lock().unwrap().host(host).map(|h| h.pending_guidance.clone()).unwrap_or_default())
    }
    /// Those on its own storage (or, with no other server on, all of them).
    fn vms_blocking_evacuation(&self, host: &str) -> Result<Vec<(String, String)>, ApiError> {
        said("host.get_vms_which_prevent_evacuation", &[json!(host)]);
        std::thread::sleep(Duration::from_millis(300));
        let d = self.data.lock().unwrap();
        let other = d.hosts.iter().any(|h| h.rref != host && h.enabled && h.live);
        Ok(d.vms
            .iter()
            .filter(|v| v.host.as_deref() == Some(host) && matches!(v.power, PowerState::Running | PowerState::Paused))
            .filter_map(|v| match (v.storage_host.as_deref() == Some(host), other) {
                (true, _) => Some((v.rref.clone(), format!("VM_REQUIRES_SR {} {}", v.rref, v.disks.first().map_or("", |x| x.sr.as_str())))),
                (false, false) => Some((v.rref.clone(), "NO_HOSTS_AVAILABLE".to_string())),
                _ => None,
            })
            .collect())
    }
    fn empty_host(&self, host: &str, stop: bool) -> Result<(Vec<String>, usize), ApiError> {
        if !stop {
            let stuck = self.vms_blocking_evacuation(host)?;
            if !stuck.is_empty() {
                return Err(ApiError { code: "VMS_CANT_MOVE".into(), details: stuck.into_iter().map(|s| s.0).collect() });
            }
        }
        said("host.evacuate", &[json!(host)]);
        std::thread::sleep(Duration::from_millis(1800));
        let mut d = self.data.lock().unwrap();
        let other = d.hosts.iter().find(|h| h.rref != host && h.enabled && h.live).map(|h| h.rref.clone());
        let (mut stopped, mut moved) = (vec![], 0);
        for v in d.vms.iter_mut().filter(|v| v.host.as_deref() == Some(host) && matches!(v.power, PowerState::Running | PowerState::Paused)) {
            if v.storage_host.as_deref() == Some(host) || other.is_none() {
                (v.power, v.host) = (PowerState::Halted, None);
                stopped.push(v.rref.clone());
            } else {
                v.host = other.clone();
                moved += 1;
            }
        }
        if let Some(h) = d.hosts.iter_mut().find(|h| h.rref == host) {
            h.enabled = false;
        }
        recompute(&mut d);
        Ok((stopped, moved))
    }
    /// A few seconds off, then back with a new start time.
    fn restart_host_and_wait(&self, host: &str, _limit: Duration) -> Result<(), ApiError> {
        said("host.reboot", &[json!(host)]);
        let set = |live: bool| {
            let mut d = self.data.lock().unwrap();
            if let Some(h) = d.hosts.iter_mut().find(|h| h.rref == host) {
                h.live = live;
                if live {
                    h.boot_time = Some(SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0));
                    h.pending_guidance.clear();
                }
            }
        };
        let began = Instant::now();
        note("restarting: it doesn't answer for a few minutes, as expected; waiting for it to come back…");
        std::thread::sleep(Duration::from_millis(1500));
        set(false);
        note(format!("went down after {}; now waiting for it to start up again (its console shows how that goes)", took_text(began.elapsed())));
        std::thread::sleep(Duration::from_millis(6000));
        set(true);
        note(format!("back after {}", took_text(began.elapsed())));
        Ok(())
    }
    fn restart_toolstack(&self, host: &str) -> Result<(), ApiError> {
        said("host.restart_agent", &[json!(host)]);
        std::thread::sleep(Duration::from_millis(2000));
        if let Some(h) = self.data.lock().unwrap().hosts.iter_mut().find(|h| h.rref == host) {
            h.pending_guidance.retain(|g| g != "restart_toolstack");
        }
        Ok(())
    }
    fn enable_host(&self, host: &str) -> Result<(), ApiError> {
        said("host.enable", &[json!(host)]);
        if let Some(h) = self.data.lock().unwrap().hosts.iter_mut().find(|h| h.rref == host) {
            h.enabled = true;
        }
        Ok(())
    }
    fn start_vm_on(&self, vm: &str, host: &str) -> Result<(), ApiError> {
        said("VM.start_on", &[json!(vm), json!(host)]);
        std::thread::sleep(Duration::from_millis(800));
        let mut d = self.data.lock().unwrap();
        if let Some(v) = d.vms.iter_mut().find(|v| v.rref == vm) {
            (v.power, v.host) = (PowerState::Running, Some(host.to_string()));
        }
        recompute(&mut d);
        Ok(())
    }
    /// About a second; a VM has to be off.
    fn make_template(&self, source: &str, name: &str, description: &str) -> Result<String, ApiError> {
        said("VM.clone", &[json!(source), json!(name)]);
        demo_work(1100);
        let mut d = self.data.lock().unwrap();
        let from = match d.vm(source) {
            Some(v) if v.power != PowerState::Halted => return Err(ApiError { code: "VM_BAD_POWER_STATE".into(), details: vec![] }),
            Some(v) => v.clone(),
            None => d.vms.iter().find(|v| v.snapshot(source).is_some()).cloned().ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["VM".into(), source.into()] })?,
        };
        let rref = format!("tpl-{}", demo_uuid());
        let t = demo_copy(&from, &rref, name, description, None, &d.srs);
        d.user_templates.push(t);
        Ok(rref)
    }
    /// About a second on the same storage, four onto other storage; a VM has to be off.
    fn copy_vm(&self, vm: &str, name: &str, sr: Option<&str>) -> Result<String, ApiError> {
        match sr { Some(sr) => said("VM.copy", &[json!(vm), json!(name), json!(sr)]), None => said("VM.clone", &[json!(vm), json!(name)]) }
        demo_work(if sr.is_some() { 4000 } else { 1000 });
        let mut d = self.data.lock().unwrap();
        let from = match d.vm(vm) {
            Some(v) if v.power != PowerState::Halted => return Err(ApiError { code: "VM_BAD_POWER_STATE".into(), details: vec![] }),
            Some(v) => v.clone(),
            None => return Err(ApiError { code: "HANDLE_INVALID".into(), details: vec!["VM".into(), vm.into()] }),
        };
        let rref = format!("vm-copy-{}", d.vms.len());
        let mut copy = demo_copy(&from, &rref, name, &from.description, sr, &d.srs);
        copy.allowed = vec!["start".into()];
        d.vms.push(copy);
        recompute(&mut d);
        Ok(rref)
    }
    fn revert_vm(&self, snap: &str) -> Result<(), ApiError> {
        said("VM.revert", &[json!(snap)]);
        std::thread::sleep(Duration::from_millis(1800));
        let mut d = self.data.lock().unwrap();
        let Some(idx) = d.vms.iter().position(|v| v.snapshot(snap).is_some()) else {
            return Err(ApiError { code: "HANDLE_INVALID".into(), details: vec!["VM".into(), snap.into()] });
        };
        let x = d.vms[idx].snapshot(snap).cloned().unwrap_or_default();
        let (need, host) = (d.vms[idx].mem_dyn_max, d.vms[idx].host.clone());
        d.vms[idx].parent = Some(snap.to_string());
        if x.memory || x.was_running {
            let h = host.or_else(|| pick_host(&mut d, need));
            d.vms[idx].power = PowerState::Running;
            d.vms[idx].host = h;
            d.vms[idx].started = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).ok();
        } else {
            d.vms[idx].power = PowerState::Halted;
            d.vms[idx].host = None;
        }
        recompute(&mut d);
        Ok(())
    }
    fn delete_snapshot(&self, snap: &str) -> Result<Vec<String>, ApiError> {
        said("VM.destroy", &[json!(snap)]);
        std::thread::sleep(Duration::from_millis(900));
        let mut d = self.data.lock().unwrap();
        let v = d.vms.iter_mut().find(|v| v.snapshot(snap).is_some()).ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["VM".into(), snap.into()] })?;
        v.snapshots.retain(|x| x.rref != snap);
        if v.parent.as_deref() == Some(snap) {
            v.parent = None;
        }
        Ok(vec![])
    }

    fn open_console(&self, _vm_ref: &str) -> Result<ConsoleStream, ApiError> {
        let addr = debug_console_address().ok_or_else(|| ApiError::other("The demo server has no real consoles."))?;
        let (host, port) = split_host_port(addr.trim());
        Ok(ConsoleStream::Plain(tcp_connect(&host, port.unwrap_or(5900))?))
    }
    fn close(&self) {}
}

#[cfg(test)]
mod tests {

    #[test]
    fn demo_update_keeps_movable_vms_running() {
        let c = DemoConnection::new();
        let power = |c: &DemoConnection, vm: &str| c.data.lock().unwrap().vm(vm).map(|v| (v.power, v.host.clone()));
        let (stopped, moved) = c.empty_host("host-a", true).unwrap();
        assert!(stopped.is_empty() && moved == 2, "{:?} {}", stopped, moved);
        c.restart_host_and_wait("host-a", Duration::from_secs(60)).unwrap();
        c.enable_host("host-a").unwrap();
        let (stopped, _) = c.empty_host("host-b", true).unwrap();
        assert_eq!(stopped, vec!["vm-mail".to_string()]);
        assert_eq!(power(&c, "vm-web"), Some((PowerState::Running, Some("host-a".into()))));
    }


    #[test]
    fn session_ids_hidden() {
        let e = "https://192.168.1.2/updates?session_id=OpaqueRef:35b80484-8a58-93ff-2875-f33182c20aa3: Network Error: timed out";
        assert_eq!(hide_session(e), "https://192.168.1.2/updates?session_id=…: Network Error: timed out");
        assert_eq!(hide_session("/rrd_updates?session_id=OpaqueRef:ab-cd&start=5 x"), "/rrd_updates?session_id=…&start=5 x");
        assert_eq!(hide_session("nothing here"), "nothing here");
    }

    use super::*;




    #[test]
    fn a_server_that_stops_answering_is_unreachable() {
        // the coordinator answers on 127.0.0.1; nothing answers on 127.0.0.2
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let host = |r: &str, addr: &str, master: bool| HostInfo { rref: r.into(), address: addr.into(), is_master: master, live: true, ..Default::default() };
        let c = RealConnection::new(&format!("127.0.0.1:{}", port), "root", "x");
        let mut hosts = vec![host("a", "127.0.0.1", true), host("b", "127.0.0.2", false)];
        c.knock_hosts(&mut hosts);
        assert!(!hosts[1].unreachable, "one miss isn't enough");
        c.knock_hosts(&mut hosts);
        assert!(hosts[1].unreachable && !hosts[0].unreachable);
        assert!(!hosts[1].is_up() && hosts[0].is_up());
        // when even the coordinator doesn't answer here, only the pool's word counts
        let c = RealConnection::new(&format!("127.0.0.1:{}", port), "root", "x");
        let mut hosts = vec![host("a", "127.0.0.2", true), host("b", "127.0.0.2", false)];
        for _ in 0..3 {
            c.knock_hosts(&mut hosts);
        }
        assert!(hosts.iter().all(|h| !h.unreachable));
    }

    /// Against a fake server that stops answering part way (SVM_FAKE_XAPI=host:port, see the
    /// session's notes): the copy gives up LOST_AFTER after the last answer.
    #[test]
    #[ignore]
    fn lost_contact_live() {
        let addr = std::env::var("SVM_FAKE_XAPI").expect("SVM_FAKE_XAPI");
        let c = RealConnection::new(&addr, "root", "x");
        let p = Progress::new();
        watch(&p);
        let t = Instant::now();
        let e = c.copy_vm("OpaqueRef:vm", "copy", Some("OpaqueRef:sr")).unwrap_err();
        eprintln!("{} after {:.0} s; progress {:?}; {}", e.code, t.elapsed().as_secs_f32(), p.fraction(), e.message());
        assert_eq!(e.code, LOST_CONTACT);
        assert!(p.fraction().unwrap_or(0.0) > 0.0);
    }

    #[test]
    fn servers_own_networks_hidden() {
        assert!(hidden_network(&json!({ "is_guest_installer_network": "true", "is_host_internal_management_network": "true" })));
        assert!(hidden_network(&json!({ "HideFromXenCenter": "true" })));
        assert!(hidden_network(&json!({ "is_guest_installer_network": "true" })));
        assert!(!hidden_network(&json!({})));
        assert!(!hidden_network(&Value::Null));
    }

    #[test]
    fn only_long_calls_run_as_tasks() {
        // what failed making a VM from a template: a setter has no Async form
        for setter in ["VM.set_is_a_template", "VM.set_name_description", "VM.set_VCPUs_max", "VM.set_memory_limits", "VM.add_to_platform", "VM.remove_from_other_config", "VM.set_affinity"] {
            assert!(!runs_as_task(setter, 120_000), "{}", setter);
        }
        assert!(runs_as_task("VM.clone", 120_000) && runs_as_task("VM.copy", 6 * 3_600_000) && runs_as_task("PBD.plug", 180_000));
        // every power action
        for a in ACTIONS.iter() {
            assert!(runs_as_task(a.method, ACTION_TIMEOUT_MS), "{}", a.method);
        }
        // a quick call stays a call
        assert!(!runs_as_task("VM.clone", 30_000) && !runs_as_task("task.get_record", 120_000));
    }

    #[test]
    fn calls_noted_without_passwords() {
        let args = vec![json!("OpaqueRef:abc"), json!("Web 2"), json!({ "device_config": { "location": "//nas/isos", "cifspassword": "s3cret" } })];
        let text = args_text(&args);
        assert!(text.starts_with("OpaqueRef:abc  \"Web 2\"  "), "{}", text);
        assert!(text.contains("//nas/isos") && text.contains("********") && !text.contains("s3cret"), "{}", text);
        // a job's thread notes its calls, and how they went, in its details
        let p = Progress::new();
        watch(&p);
        said("VM.clone", &[json!("OpaqueRef:abc"), json!("x")]);
        answered(&Err(ApiError { code: "VM_BAD_POWER_STATE".into(), details: vec!["".into(), "running".into()] }), Duration::from_millis(5));
        let lines = p.trace.lines();
        assert_eq!(lines[0], (Said::Asked, "VM.clone  OpaqueRef:abc  \"x\"".to_string()));
        assert_eq!(lines[1], (Said::Error, "Error code: VM_BAD_POWER_STATE".to_string()));
        assert_eq!(lines[2], (Said::Error, "running".to_string()));
    }

    /// Against a fake server that, like a real one, has no Async form for setters
    /// (SVM_FAKE_XAPI=host:port): a VM made from a template of the user's.
    #[test]
    #[ignore]
    fn from_template_live() {
        let addr = std::env::var("SVM_FAKE_XAPI").expect("SVM_FAKE_XAPI");
        let c = RealConnection::new(&addr, "root", "x");
        let spec = NewVm { name: "ubuntuOS-test".into(), template: "OpaqueRef:tpl".into(), vcpus: 2, cores_per_socket: 2, memory: 2 * GB, sr: "OpaqueRef:sr".into(), from_template: true, keep_nics: true, description: "test".into(), ..Default::default() };
        let tr = Trace::default();
        let made = c.create_vm(&spec, &tr);
        eprintln!("{}", tr.text());
        assert_eq!(made.unwrap(), "OpaqueRef:newvm");
    }

    /// Every change to a VM against a fake server that, like a real one, has no Async form for
    /// setters (SVM_FAKE_XAPI=host:port).
    #[test]
    #[ignore]
    fn edit_vm_live() {
        let addr = std::env::var("SVM_FAKE_XAPI").expect("SVM_FAKE_XAPI");
        let c = RealConnection::new(&addr, "root", "x");
        let gb = 1024 * 1024 * 1024;
        for ch in [
            VmChange::Name("x".into()),
            VmChange::Description("y".into()),
            VmChange::AutoPowerOn(true),
            VmChange::BootOrder("dcn".into()),
            VmChange::Resources { vcpus: 4, cores_per_socket: 4, memory: 4 * gb },
            VmChange::AddDisk { name: "d".into(), size: 20 * gb, sr: "OpaqueRef:sr".into() },
            VmChange::GrowDisk { vdi: "OpaqueRef:vdi".into(), size: 30 * gb },
            VmChange::RemoveDisk { vbd: "OpaqueRef:vbd".into(), vdi: "OpaqueRef:vdi".into() },
            VmChange::Dvd { vbd: "OpaqueRef:cd".into(), iso: Some("OpaqueRef:iso".into()) },
            VmChange::AddNic { net: "OpaqueRef:net".into() },
            VmChange::MoveNic { vif: "OpaqueRef:vif".into(), net: "OpaqueRef:net2".into() },
            VmChange::RemoveNic { vif: "OpaqueRef:vif".into() },
        ] {
            let r = c.edit_vm("OpaqueRef:vm", &ch);
            eprintln!("{:?}: {:?}", ch, r);
            assert!(r.is_ok());
        }
    }

    #[test]
    fn task_results() {
        assert_eq!(task_value("<value>OpaqueRef:1234</value>"), json!("OpaqueRef:1234"));
        assert_eq!(task_value(""), Value::Null);
        let p = Progress::new();
        assert_eq!((p.fraction(), p.silent_for()), (None, None));
        p.set_fraction(0.25);
        p.heard(false);
        assert_eq!(p.fraction(), Some(0.25));
        assert!(p.silent_for().is_some());
        p.heard(true);
        assert!(p.silent_for().is_none());
    }

    #[test]
    fn vm_changes() {
        let c = DemoConnection::new();
        let vm = |c: &DemoConnection| c.fetch(0).unwrap().vm("vm-desk").cloned().unwrap();
        let gb = |n: i64| n * 1024 * 1024 * 1024;
        c.edit_vm("vm-desk", &VmChange::Name("Desk".into())).unwrap();
        c.edit_vm("vm-desk", &VmChange::Resources { vcpus: 4, cores_per_socket: 4, memory: gb(6) }).unwrap();
        c.edit_vm("vm-desk", &VmChange::AddDisk { name: "data".into(), size: gb(20), sr: "sr-local1".into() }).unwrap();
        c.edit_vm("vm-desk", &VmChange::AddNic { net: "net-1".into() }).unwrap();
        c.edit_vm("vm-desk", &VmChange::Dvd { vbd: "vm-desk-vbd-3".into(), iso: Some("sr-iso-vdi-2".into()) }).unwrap();
        c.edit_vm("vm-desk", &VmChange::AutoPowerOn(true)).unwrap();
        let v = vm(&c);
        assert_eq!((v.name.as_str(), v.vcpus, v.mem_dyn_max, v.auto_power_on), ("Desk", 4, gb(6), true));
        let added = v.disks.iter().find(|k| k.name == "data").unwrap().clone();
        assert_eq!((added.device.as_str(), added.size), ("1", gb(20)));
        assert_eq!(v.nics.len(), 2);
        assert!(v.disks.iter().any(|k| k.kind != "Disk" && k.vdi == "sr-iso-vdi-2"));
        // grown, moved, ejected, removed
        c.edit_vm("vm-desk", &VmChange::GrowDisk { vdi: added.vdi.clone(), size: gb(30) }).unwrap();
        let nic = vm(&c).nics[1].vif.clone();
        c.edit_vm("vm-desk", &VmChange::MoveNic { vif: nic.clone(), net: "net-0".into() }).unwrap();
        c.edit_vm("vm-desk", &VmChange::Dvd { vbd: "vm-desk-vbd-3".into(), iso: None }).unwrap();
        let v = vm(&c);
        assert_eq!(v.disks.iter().find(|k| k.vdi == added.vdi).unwrap().size, gb(30));
        assert_eq!(v.nics[1].network, "Pool-wide network 0");
        assert!(v.disks.iter().any(|k| k.kind != "Disk" && k.vdi.is_empty()));
        c.edit_vm("vm-desk", &VmChange::RemoveDisk { vbd: added.vbd.clone(), vdi: added.vdi.clone() }).unwrap();
        c.edit_vm("vm-desk", &VmChange::RemoveNic { vif: nic }).unwrap();
        let v = vm(&c);
        assert!(v.disks.iter().all(|k| k.vdi != added.vdi) && v.nics.len() == 1);
        // a running VM's CPUs and memory wait for it to be off
        assert_eq!(c.edit_vm("vm-web", &VmChange::Resources { vcpus: 2, cores_per_socket: 2, memory: gb(2) }).unwrap_err().code, "VM_BAD_POWER_STATE");
        // the boot order both ways
        assert_eq!(boot_text("dcn"), "DVD drive, Disk, Network");
        assert_eq!(boot_letters("DVD drive, Disk, Network"), "dcn");
    }

    #[test]
    fn templates_and_copies() {
        let c = DemoConnection::new();
        // a VM that's on can't be copied or made a template
        assert_eq!(c.make_template("vm-web", "x", "").unwrap_err().code, "VM_BAD_POWER_STATE");
        assert_eq!(c.copy_vm("vm-web", "x", None).unwrap_err().code, "VM_BAD_POWER_STATE");
        // from a VM that's off, and from a snapshot of a running one; the VM stays
        let a = c.make_template("vm-desk", "Desktop template", "clean").unwrap();
        let b = c.make_template("vm-web-snap-1", "Web, fresh", "").unwrap();
        let d = c.fetch(0).unwrap();
        assert!(d.vm("vm-desk").is_some() && d.vm(&a).is_none());
        assert_eq!(d.user_template(&a).map(|t| (t.name.as_str(), t.description.as_str())), Some(("Desktop template", "clean")));
        assert_eq!(d.user_template(&b).unwrap().power, PowerState::Halted);
        // copies: in an instant, or in full onto other storage
        let same = c.copy_vm("vm-desk", "Desktop 2", None).unwrap();
        let other = c.copy_vm("vm-desk", "Desktop 3", Some("sr-nfs")).unwrap();
        let d = c.fetch(0).unwrap();
        assert_eq!(d.vm(&same).unwrap().disks[0].sr, "sr-local1");
        assert_eq!(d.vm(&other).unwrap().disks[0].sr, "sr-nfs");
        // a VM from a template: its network cards kept, or one on the chosen network
        let spec = NewVm { name: "Web 2".into(), template: "tpl-web".into(), vcpus: 2, cores_per_socket: 2, memory: 4 * GB, sr: "sr-nfs".into(), from_template: true, keep_nics: true, ..Default::default() };
        let kept = c.create_vm(&spec, &Trace::default()).unwrap();
        let moved = c.create_vm(&NewVm { name: "Web 3".into(), keep_nics: false, network: Some("net-1".into()), ..spec }, &Trace::default()).unwrap();
        let d = c.fetch(0).unwrap();
        assert_eq!(d.vm(&kept).unwrap().nics[0].network, "Pool-wide network 0");
        assert_eq!(d.vm(&moved).unwrap().nics[0].network, "Storage network");
        assert_eq!(d.vm(&kept).unwrap().disks[0].size, 40 * GB);
        // deleted
        c.delete_vm("tpl-win", true).unwrap();
        assert!(c.fetch(0).unwrap().user_template("tpl-win").is_none());
    }

    #[test]
    fn snapshots_taken_reverted_and_deleted() {
        let c = DemoConnection::new();
        let vm = |c: &DemoConnection| c.fetch(0).unwrap().vm("vm-web").cloned().unwrap();
        assert_eq!(vm(&c).power, PowerState::Running);
        assert_eq!(vm(&c).snapshots.len(), 3);
        // with its memory, the newest first, and now the current state comes from it
        let snap = c.snapshot_vm("vm-web", "Before upgrade", "nginx 1.24", true).unwrap();
        let v = vm(&c);
        assert_eq!((v.snapshots.len(), v.snapshots[0].rref.as_str(), v.parent.as_deref()), (4, snap.as_str(), Some(snap.as_str())));
        assert!(v.snapshots[0].memory && v.snapshots[0].was_running && v.snapshots[0].description == "nginx 1.24");
        // a halted VM can't have its memory kept
        assert_eq!(c.snapshot_vm("vm-desk", "x", "", true).unwrap_err().code, "VM_BAD_POWER_STATE");
        // back to one taken while it was off: it stays off
        c.revert_vm("vm-web-snap-1").unwrap();
        let v = vm(&c);
        assert_eq!((v.power, v.parent.as_deref()), (PowerState::Halted, Some("vm-web-snap-1")));
        // back to the one with memory: running again
        c.revert_vm(&snap).unwrap();
        assert_eq!(vm(&c).power, PowerState::Running);
        // deleted
        assert!(c.delete_snapshot(&snap).unwrap().is_empty());
        let v = vm(&c);
        assert!(v.snapshot(&snap).is_none() && v.parent.is_none());
        c.set_vm_description("vm-web-snap-2", "Kernel 6.8").unwrap();
        assert_eq!(vm(&c).snapshot("vm-web-snap-2").unwrap().description, "Kernel 6.8");
    }

    #[test]
    fn repair_mounts_storage_again() {
        let c = DemoConnection::new();
        let set_links = |f: &dyn Fn(&mut Vec<(String, bool)>)| f(&mut c.data.lock().unwrap().srs.iter_mut().find(|x| x.rref == "sr-iso").unwrap().links);
        let trouble = || {
            let d = c.fetch(0).unwrap();
            d.sr_trouble(d.sr("sr-iso").unwrap())
        };
        assert_eq!(trouble(), None);
        // unplugged everywhere, as after the servers started while the share was off
        set_links(&|l| l.iter_mut().for_each(|x| x.1 = false));
        let t = trouble().unwrap();
        assert!(t.nowhere && !t.detached);
        assert_eq!(t.missing, vec!["host-a".to_string(), "host-b".to_string()], "the coordinator first");
        let tried = c.repair_sr("sr-iso").unwrap();
        assert_eq!(tried.iter().map(|t| (t.0.as_str(), t.1.is_ok())).collect::<Vec<_>>(), vec![("demo-host-01", true), ("demo-host-02", true)]);
        assert_eq!(trouble(), None);
        // detached: Repair has to be told where it is
        set_links(&|l| l.clear());
        assert!(trouble().unwrap().detached);
        assert_eq!(c.repair_sr("sr-iso").unwrap_err().code, "SR_HAS_NO_PBDS");
        let mut lib = IsoLibrary { location: "//nas/isos".into(), username: "joao".into(), password: "wrong".into(), smb_version: "3.0".into(), ..Default::default() };
        assert!(c.reattach_sr("sr-iso", &lib).is_err());
        assert!(trouble().unwrap().detached, "a failed attempt leaves it as it was");
        lib.password = "right".into();
        c.reattach_sr("sr-iso", &lib).unwrap();
        assert_eq!(trouble(), None);
    }

    fn tools(pv: Value, detected: bool) -> (bool, String) {
        let mut vm = VmInfo::default();
        vm_details(&mut vm, &json!({}), &json!({ "PV_drivers_version": pv, "PV_drivers_detected": detected }), &Value::Null);
        (vm.tools_installed, vm.tools)
    }

    #[test]
    fn guest_tools() {
        assert_eq!(tools(json!({ "major": "-1", "minor": "-1", "micro": "-1" }), false), (false, String::new()));
        assert_eq!(tools(json!({ "major": -1 }), false), (false, String::new()));
        assert_eq!(tools(json!({ "major": "1" }), false), (true, String::new()));
        assert_eq!(tools(json!({ "major": "9", "minor": "4", "micro": "0", "build": "12" }), true), (true, "9.4.0".into()));
        assert_eq!(tools(json!({}), false), (false, String::new()));
        assert_eq!(tools(json!({}), true), (true, String::new()));
    }

    #[test]
    fn os_from_template_when_tools_dont_say() {
        let mut vm = VmInfo::default();
        vm_details(&mut vm, &json!({ "other_config": { "base_template_name": "Windows Server 2025 (64-bit)" } }), &json!({}), &Value::Null);
        assert_eq!(vm.os, "Windows Server 2025 (64-bit)");
        let mut vm = VmInfo { os: "Microsoft Windows Server 2025 Standard".into(), ..Default::default() };
        vm_details(&mut vm, &json!({ "other_config": { "base_template_name": "Windows Server 2025 (64-bit)" } }), &json!({}), &Value::Null);
        assert_eq!(vm.os, "Microsoft Windows Server 2025 Standard");
    }

    #[test]
    fn demo_iso_descriptions() {
        let c = DemoConnection::new();
        c.set_vdi_description("sr-iso-vdi-1", "Rocky Linux 9").unwrap();
        let d = c.data.lock().unwrap().clone();
        let f = d.srs.iter().flat_map(|x| x.files.iter()).find(|f| f.vdi == "sr-iso-vdi-1").unwrap();
        assert_eq!((f.name.as_str(), f.description.as_str()), ("rocky-9.4-x86_64-minimal.iso", "Rocky Linux 9"));
        assert_eq!(c.set_vdi_description("no-such-disc", "x").unwrap_err().code, "HANDLE_INVALID");
    }

    #[test]
    fn demo_upload() {
        let dir = std::env::temp_dir().join(format!("svm-upload-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("alpine 3.20.iso");
        std::fs::write(&file, vec![0u8; 4096]).unwrap();
        let c = DemoConnection::new();
        // a whole upload: the disc shows up in the library with the file's size
        let t = Arc::new(Transfer::default());
        let file = Source::File(file);
        c.upload_iso("sr-iso", None, &file, "alpine-3.20.iso", &t).unwrap();
        assert_eq!((t.sent.load(Ordering::Relaxed), t.total.load(Ordering::Relaxed)), (4096, 4096));
        let has = |name: &str| c.data.lock().unwrap().sr("sr-iso").unwrap().files.iter().any(|f| f.name == name && f.size == 4096);
        assert!(has("alpine-3.20.iso"));
        // the same name again is refused, as by the server's ISO driver
        let again = c.upload_iso("sr-iso", None, &file, "alpine-3.20.iso", &Arc::new(Transfer::default())).unwrap_err();
        assert!(upload_text(&again).contains("already exists"));
        // a cancelled one leaves nothing
        let t = Arc::new(Transfer::default());
        t.cancel.store(true, Ordering::Relaxed);
        assert_eq!(c.upload_iso("sr-iso", None, &file, "other.iso", &t).unwrap_err().code, CANCELLED);
        assert!(!has("other.iso"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn demo_create_vm() {
        let c = DemoConnection::new();
        let d = c.data.lock().unwrap().clone();
        let tpl = d.templates.iter().find(|t| t.name == "Ubuntu Noble Numbat 24.04").unwrap().rref.clone();
        let net = d.networks[0].rref.clone();
        let spec = NewVm {
            name: "Ubuntu 24.04".into(), template: tpl, iso: "sr-iso-vdi-2".into(), vcpus: 2, cores_per_socket: 2,
            memory: 4 * GB, disk: 40 * GB, sr: "sr-nfs".into(), network: Some(net), start: true, ..Default::default()
        };
        let tr = Trace::default();
        let made = c.create_vm(&spec, &tr).unwrap();
        c.start_traced(&made, "\"Ubuntu 24.04\"", &tr).unwrap();
        assert!(tr.text().starts_with("> VM.clone"), "{}", tr.text());
        // the ISO's library checked first
        let checked = NewVm { name: "Checked".into(), check: vec!["sr-iso".into()], start: false, ..spec.clone() };
        let first = Trace::default();
        c.create_vm(&checked, &first).unwrap();
        assert!(first.text().starts_with("> SR.get_PBDs  SR=sr-iso"), "{}", first.text());
        assert!(first.text().contains("> SR.scan") && first.text().contains("> VM.clone"));
        assert!(tr.text().contains("> VM.start"));
        assert!(tr.waiting().is_none());
        let d = c.data.lock().unwrap().clone();
        let vm = d.vm(&made).unwrap();
        assert_eq!((vm.name.as_str(), vm.power, vm.vcpus, vm.mem_dyn_max), ("Ubuntu 24.04", PowerState::Running, 2, 4 * GB));
        assert_eq!(vm.os, "Ubuntu Noble Numbat 24.04");
        let drives: Vec<(&str, &str)> = vm.disks.iter().map(|k| (k.kind.as_str(), k.name.as_str())).collect();
        assert_eq!(drives, vec![("Disk", "Ubuntu 24.04 disk"), ("DVD drive", "ubuntu-24.04.1-live-server-amd64.iso")]);
        assert_eq!(vm.nics.len(), 1);
        // without a network, and left off
        let made = c.create_vm(&NewVm { name: "Quiet".into(), network: None, start: false, ..spec }, &Trace::default()).unwrap();
        let d = c.data.lock().unwrap().clone();
        let vm = d.vm(&made).unwrap();
        assert_eq!((vm.power, vm.nics.len()), (PowerState::Halted, 0));
    }

    #[test]
    fn name_answers_while_logging_in() {
        // 192.0.2.1 is a documentation address: nothing answers there, like a server that's off
        let c = Arc::new(RealConnection::new("192.0.2.1", "root", "x"));
        let worker = c.clone();
        let started = Instant::now();
        let job = std::thread::spawn(move || worker.fetch(60_000));
        std::thread::sleep(Duration::from_millis(300));
        // the window asks for the name as it draws: it mustn't wait for the login
        while started.elapsed() < Duration::from_secs(2) {
            let asked = Instant::now();
            assert_eq!(c.hostname(), "192.0.2.1");
            assert!(asked.elapsed() < Duration::from_millis(100), "hostname() waited {:?}", asked.elapsed());
            std::thread::sleep(Duration::from_millis(50));
        }
        // and the login gives up in about CONNECT_TIMEOUT_MS, not the call's 60 s
        let e = job.join().unwrap().unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(CONNECT_TIMEOUT_MS + 4_000), "took {:?}", started.elapsed());
        assert!(e.message().contains("isn't answering"), "{}", e.message());
    }

    #[test]
    fn demo_reboot_host() {
        let c = DemoConnection::new();
        // demo-host-02 restarts: Mail Relay (on its own storage) stops, it stays on
        c.reboot_host("host-b").unwrap();
        let d = c.data.lock().unwrap().clone();
        let h = d.host("host-b").unwrap();
        assert!(h.enabled && h.live);
        assert_eq!(d.vm("vm-mail").unwrap().power, PowerState::Halted);
        assert!(c.reboot_host("host-x").is_err());
    }

    #[test]
    fn demo_shutdown_host() {
        let c = DemoConnection::new();
        // demo-host-02: Mail Relay's disk is on its own storage, so it stops
        c.shutdown_host("host-b").unwrap();
        let d = c.data.lock().unwrap().clone();
        let h = d.host("host-b").unwrap();
        assert!(!h.enabled && !h.live);
        let mail = d.vm("vm-mail").unwrap();
        assert_eq!((mail.power, mail.host.as_deref()), (PowerState::Halted, None));
        assert!(d.vms.iter().all(|v| v.host.as_deref() != Some("host-b") || v.power != PowerState::Running));
    }

    #[test]
    fn demo_delete_vm() {
        let c = DemoConnection::new();
        // running: turned off, then gone
        assert_eq!(c.delete_vm("vm-web", true).unwrap(), Vec::<String>::new());
        assert!(c.data.lock().unwrap().vm("vm-web").is_none());
        assert_eq!(c.delete_vm("vm-web", true).unwrap_err().code, "HANDLE_INVALID");
    }

    #[test]
    fn template_recommendations() {
        let rec = "<restrictions><restriction field=\"memory-static-max\" max=\"1649267441664\"/><restriction field=\"supports-uefi\" value=\"yes\"/><restriction field=\"has-vendor-device\" value=\"true\"/></restrictions>";
        assert_eq!(restriction(rec, "supports-uefi").as_deref(), Some("yes"));
        assert_eq!(restriction(rec, "has-vendor-device").as_deref(), Some("true"));
        assert_eq!(restriction(rec, "supports-secure-boot"), None);
        assert_eq!(provision_size("<provision><disk device=\"0\" size=\"34359738368\" sr=\"\" bootable=\"true\" type=\"system\"/></provision>"), 34359738368);
        assert_eq!(provision_size(""), 0);
    }

    #[test]
    fn demo_delete_iso() {
        let c = DemoConnection::new();
        let names = || c.data.lock().unwrap().sr("sr-iso").unwrap().files.iter().map(|f| f.name.clone()).collect::<Vec<_>>();
        // in a running VM's drive: refused, as by the server
        assert_eq!(c.delete_iso("sr-iso-vdi-2").unwrap_err().code, "VDI_IN_USE");
        // in a shut-down VM's drive, or in none: gone
        c.delete_iso("sr-iso-vdi-1").unwrap();
        c.delete_iso("sr-iso-vdi-0").unwrap();
        assert_eq!(names()[..3], ["ubuntu-24.04.1-live-server-amd64.iso", "Win11_24H2_English_x64.iso", "windows-server-2025.iso"]);
        assert_eq!(c.delete_iso("sr-iso-vdi-0").unwrap_err().code, "HANDLE_INVALID");
    }

    #[test]
    fn names_from_links() {
        assert_eq!(url_file_name("https://releases.ubuntu.com/24.04/ubuntu-24.04.1-live-server-amd64.iso"), Some("ubuntu-24.04.1-live-server-amd64.iso".into()));
        assert_eq!(url_file_name("https://cdn.example.com/get/Win%2011%20x64.iso?token=abc#top"), Some("Win 11 x64.iso".into()));
        assert_eq!(url_file_name("https://example.com/"), None);
        assert_eq!(url_file_name("https://example.com"), None);
        assert_eq!(disposition_name("attachment; filename=\"debian 12.iso\""), Some("debian 12.iso".into()));
        assert_eq!(disposition_name("attachment; filename=a.iso; filename*=UTF-8''Instala%C3%A7%C3%A3o.iso"), Some("Instalação.iso".into()));
        assert_eq!(disposition_name("inline"), None);
        assert_eq!(percent_decode("100%"), "100%");
    }

    /// Needs the internet: cargo test --release -- --ignored real_links
    #[test]
    #[ignore]
    fn real_links() {
        let iso = probe_url("https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-virt-3.20.3-x86_64.iso").unwrap();
        assert_eq!(iso.name, "alpine-virt-3.20.3-x86_64.iso");
        assert!(iso.size > 10_000_000, "{:?}", iso);
        assert!(probe_url("https://alpinelinux.org/downloads/").unwrap_err().contains("web page"));
        assert!(probe_url("https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/no-such-file.iso").unwrap_err().contains("404"));
        // a download read through the counting reader arrives whole
        let url = "https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-virt-3.20.3-x86_64.iso.sha256";
        let size = probe_url(url).unwrap().size;
        let t = Arc::new(Transfer::default());
        let mut body = Counting { inner: Box::new(web_get(url).unwrap().into_reader()), t: t.clone() };
        let mut got = Vec::new();
        body.read_to_end(&mut got).unwrap();
        assert_eq!((got.len() as u64, t.sent.load(Ordering::Relaxed)), (size, size));
    }

    fn upload_text(e: &ApiError) -> String {
        format!("{} {}", e.code, e.details.join(" ")).to_lowercase()
    }

    #[test]
    fn xen_times() {
        assert_eq!(parse_xen_time("19700101T00:00:01Z"), Some(1));
        assert_eq!(parse_xen_time("20261114T13:22:01Z"), Some(1_794_662_521));
        assert_eq!(parse_xen_time("2026-11-14T13:22:01Z"), Some(1_794_662_521));
        assert_eq!(parse_xen_time("19700101T00:00:00Z"), None); // a VM that never started
    }
}
