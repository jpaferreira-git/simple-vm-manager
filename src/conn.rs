//! Talking to servers: JSON-RPC 2.0 over HTTPS at /jsonrpc (as the original's XenAPI.JsonRpcClient),
//! and an in-memory demo pool (the original has simulated connections too).

use crate::alerts::{AlertSettings, XenMessage};
use crate::history::{demo_host_records, demo_vm_records, read_records, Records, Span};
use crate::model::*;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
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
    /// The server task's progress, per mille; u64::MAX while the server hasn't said.
    permille: AtomicU64,
    /// Since when the server hasn't answered, while it doesn't.
    silent_since: Mutex<Option<Instant>>,
    /// What the job copies, when that's known (disks moved or copied).
    bytes: Mutex<Option<Bytes>>,
    /// The bytes done as it went (when, how many), the last half minute of it: its speed.
    done_at: Mutex<std::collections::VecDeque<(Instant, i64)>>,
}

/// A job that copies `total` bytes, its server task now on `part` of them after `before` (one
/// task for it all, or one disk after another).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Bytes {
    total: i64,
    before: i64,
    part: i64,
}

impl Progress {
    pub fn new() -> Arc<Progress> {
        Arc::new(Progress {
            trace: Arc::new(Trace::default()),
            permille: AtomicU64::new(u64::MAX),
            silent_since: Mutex::new(None),
            bytes: Mutex::new(None),
            done_at: Mutex::new(Default::default()),
        })
    }
    /// How far it is (0 to 1), once the server says: of all it copies, when that's known.
    pub fn fraction(&self) -> Option<f32> {
        let p = self.permille.load(Ordering::Relaxed);
        match *self.bytes.lock().unwrap() {
            Some(b) if b.total > 0 => Some((self.done_of(b) as f64 / b.total as f64).clamp(0.0, 1.0) as f32),
            _ => (p != u64::MAX).then(|| p.min(1000) as f32 / 1000.0),
        }
    }
    pub fn set_fraction(&self, f: f64) {
        self.permille.store((f.clamp(0.0, 1.0) * 1000.0) as u64, Ordering::Relaxed);
        if let Some(b) = *self.bytes.lock().unwrap() {
            let now = Instant::now();
            let mut at = self.done_at.lock().unwrap();
            at.push_back((now, self.done_of(b)));
            while at.front().map_or(false, |(t, _)| now.duration_since(*t) > Duration::from_secs(30)) {
                at.pop_front();
            }
        }
    }
    /// From now on the server's task copies `part` bytes, `before` of `total` being done.
    pub fn copying(&self, before: i64, part: i64, total: i64) {
        *self.bytes.lock().unwrap() = Some(Bytes { total, before, part });
        self.permille.store(0, Ordering::Relaxed);
        self.done_at.lock().unwrap().push_back((Instant::now(), before));
    }
    fn done_of(&self, b: Bytes) -> i64 {
        let p = self.permille.load(Ordering::Relaxed);
        let f = if p == u64::MAX { 0.0 } else { p.min(1000) as f64 / 1000.0 };
        (b.before + (b.part as f64 * f) as i64).min(b.total)
    }
    /// How much it has copied, and of how much (as the server's progress says, so near enough).
    pub fn copied(&self) -> Option<(i64, i64)> {
        self.bytes.lock().unwrap().filter(|b| b.total > 0).map(|b| (self.done_of(b), b.total))
    }
    /// How fast it copies, in bytes a second, over the last half minute (once there's enough of
    /// it to say).
    pub fn rate(&self) -> Option<f64> {
        let at = self.done_at.lock().unwrap();
        let (first, last) = (at.front()?, at.back()?);
        let secs = last.0.duration_since(first.0).as_secs_f64();
        (secs >= 4.0 && last.1 >= first.1).then(|| (last.1 - first.1) as f64 / secs)
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

/// The watched job copies `part` bytes now, `before` of `total` done (see `Progress::copying`).
fn copying(before: i64, part: i64, total: i64) {
    if let Some(w) = watched() {
        w.copying(before, part, total);
    }
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
            Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), if k.contains("password") || k.contains("session") { json!("********") } else { hide(v) })).collect()),
            Value::Array(a) => Value::Array(a.iter().map(hide).collect()),
            // (a session in a link: migrate_receive's answer)
            Value::String(t) if t.contains("session_id=") => json!(hide_session(t)),
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
/// A server's other_config keys for maintenance mode, as XenCenter writes them: the mark, and the
/// VMs (uuids, with commas) it moved off, shut down, or suspended.
const MAINT: &str = "MAINTENANCE_MODE";
const MAINT_MOVED: &str = "MAINTENANCE_MODE_EVACUATED_VMS_MIGRATED";
const MAINT_STOPPED: &str = "MAINTENANCE_MODE_EVACUATED_VMS_HALTED";
const MAINT_SUSPENDED: &str = "MAINTENANCE_MODE_EVACUATED_VMS_SUSPENDED";

/// Sends a server's Wake-on-LAN signal from this computer: its card's magic packet, to its
/// network's broadcast address (when known) and to this computer's own network, on ports 9 and 7.
/// It only reaches a server on the same network as this computer. Noted in the job's details.
pub fn send_wake(target: &WakeTarget) -> Result<(), ApiError> {
    let mac = parse_mac(&target.mac).ok_or_else(|| ApiError::other("its network card's address isn't known"))?;
    let packet = magic_packet(mac);
    let failed = |e: std::io::Error| ApiError::other(format!("couldn't send a Wake-on-LAN signal from this computer: {}", e));
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").map_err(failed)?;
    sock.set_broadcast(true).map_err(failed)?;
    let mut to: Vec<&str> = vec![];
    if !target.broadcast.is_empty() {
        to.push(&target.broadcast);
    }
    to.push("255.255.255.255");
    let mut last = None;
    let mut sent = vec![];
    for addr in &to {
        for port in [9u16, 7] {
            match sock.send_to(&packet, (*addr, port)) {
                Ok(_) if !sent.contains(addr) => sent.push(*addr),
                Ok(_) => {}
                Err(e) => last = Some(e),
            }
        }
    }
    if sent.is_empty() {
        return Err(failed(last.unwrap_or_else(|| std::io::Error::other("nowhere to send it"))));
    }
    note(format!("sent a Wake-on-LAN signal for {} from this computer, to {}", target.mac, sent.join(" and ")));
    Ok(())
}
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
    /// Creates shared storage for VMs' disks from an NFS folder (mounted on every server of the
    /// pool right away, so a wrong address fails here as for an ISO library); its reference.
    fn create_nfs_storage(&self, st: &NfsStorage) -> Result<String, ApiError>;
    /// The folders an NFS server shares, as the pool's coordinator sees them (SR.probe).
    fn nfs_exports(&self, server: &str) -> Result<Vec<String>, ApiError>;
    /// Makes a network (on a VLAN of a card of every server, or private); its reference. A VLAN
    /// that can't be made takes the new network away again.
    fn create_network(&self, n: &NewNetwork) -> Result<String, ApiError>;
    /// Deletes a network: its VLAN first (from every server's card), then the network. One with
    /// cards on it (VMs', templates', snapshots') is refused by the server.
    fn delete_network(&self, net: &str) -> Result<(), ApiError>;
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
    /// Makes a snapshot schedule (`x.rref` empty) or changes one, and puts exactly the VMs `x.vms`
    /// on it (taking off those no longer listed). Its reference.
    fn save_schedule(&self, x: &ScheduleInfo) -> Result<String, ApiError>;
    /// Runs a snapshot schedule now, as at its times (VMSS.snapshot_now: the server's vmss
    /// plugin takes its VMs' snapshots and deletes those past what it keeps). The plugin's answer:
    /// "True", or what went wrong.
    fn run_schedule(&self, vmss: &str) -> Result<String, ApiError>;
    /// Deletes a snapshot schedule, its VMs taken off it first (the snapshots it took stay).
    fn delete_schedule(&self, vmss: &str) -> Result<(), ApiError>;
    /// Puts VMs on a snapshot schedule, or (None) on none.
    fn set_vm_schedule(&self, vms: &[String], vmss: Option<&str>) -> Result<(), ApiError>;
    /// Turns a snapshot schedule on or off.
    fn set_schedule_enabled(&self, vmss: &str, on: bool) -> Result<(), ApiError>;
    /// The last ten minutes of a running VM's load, from the server it runs on (its address).
    fn vm_perf(&self, vm_uuid: &str, host_address: &str) -> Result<Perf, ApiError>;
    /// The last ten minutes of a server's load and its running VMs', from the server itself (its
    /// uuid and address).
    fn host_perf(&self, host_uuid: &str, host_address: &str) -> Result<HostPerf, ApiError>;
    /// A running VM's records over `span` (`vm_uuid`), or a server's own and its running VMs'
    /// (None), as the server at `host_address` keeps them (the one the VM runs on).
    fn history(&self, host_address: &str, vm_uuid: Option<&str>, span: Span) -> Result<Records, ApiError>;
    /// The pool's messages for people (its alerts): all of them, or those since a time (seconds
    /// since 1970).
    fn messages(&self, since: Option<i64>) -> Result<Vec<XenMessage>, ApiError>;
    /// Removes messages from the pool (dismissed: XenCenter no longer shows them either). One
    /// already gone counts as done.
    fn dismiss_messages(&self, refs: &[String]) -> Result<(), ApiError>;
    /// A VM's, a server's or a storage's limits (its other_config's perfmon; `cls` "VM", "host"
    /// or "SR"), and the pool's email for alerts.
    fn alert_settings(&self, cls: &str, obj: &str) -> Result<AlertSettings, ApiError>;
    /// Sets those (the pool's email too when `mail` is given: empty to stop it), then has every
    /// server read its limits again.
    fn set_alert_settings(&self, cls: &str, obj: &str, perfmon: &str, mail: Option<(String, String)>) -> Result<(), ApiError>;
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
    /// Has the pool turn on a server that's off (host.power_on: its coordinator sends it a
    /// Wake-on-LAN signal), set to Wake-on-LAN first when it has no way set. Done once the pool
    /// has sent it; the server takes minutes to answer.
    fn power_on_host(&self, host_ref: &str) -> Result<(), ApiError>;
    /// Puts a server in maintenance mode: no new VMs sent to it (even after a restart, where the
    /// server allows), its VMs moved to the pool's other servers and those that can't move shut
    /// down (with `stop`; without, it fails first when there are any), then marked as XenCenter
    /// marks it, with the VMs moved off and shut down, to bring back later. It takes VMs again if
    /// a step fails. The VMs it shut down, and how many it moved.
    fn enter_maintenance(&self, host_ref: &str, stop: bool) -> Result<(Vec<String>, usize), ApiError>;
    /// Takes a server out of maintenance mode: it takes VMs again, its mark gone; with `back`, the
    /// VMs maintenance moved off it come back (live) and those it shut down start on it again.
    /// Those that couldn't, each with why.
    fn exit_maintenance(&self, host_ref: &str, back: bool) -> Result<Vec<String>, ApiError>;
    /// Starts a VM on a server.
    fn start_vm_on(&self, vm_ref: &str, host_ref: &str) -> Result<(), ApiError>;
    /// Moves a VM (see `MoveVm`). Running, it goes live: to the server (VM.pool_migrate), with its
    /// disks when they change storage (VM.migrate_send), or only its disks (VDI.pool_migrate). Off,
    /// its disks are copied to their new storage and swapped in, and its home set to the server.
    fn move_vm(&self, m: &MoveVm) -> Result<(), ApiError>;
    /// Makes a storage the pool's default: for new VMs' disks, suspended VMs' memory and crash
    /// dumps (pool.set_default_SR, set_suspend_image_SR, set_crash_dump_SR, as XenCenter's Set as
    /// Default).
    fn make_default_sr(&self, sr_ref: &str) -> Result<(), ApiError>;
    /// Whether a VM runs, and its disks: each one's position, VDI and storage.
    fn vm_disks(&self, vm_ref: &str) -> Result<(bool, Vec<(String, String, String)>), ApiError>;
    /// Deletes a snapshot and its own copies of the disks (and its saved memory). The ones that
    /// couldn't be deleted (the snapshot is gone by then): each one's name and why.
    fn delete_snapshot(&self, snapshot_ref: &str) -> Result<Vec<String>, ApiError>;
    /// Every field the server keeps on a VM, a server or the pool (`kind`: "vm", "host" or
    /// "pool"; `rref` isn't needed for the pool), then those of its metrics: (what, the record).
    fn records(&self, kind: &str, rref: &str) -> Result<Vec<(String, Value)>, ApiError>;
    /// Sets a server's network card's address (PIF.reconfigure_ip): `mode` "DHCP", "Static" or
    /// "None"; for Static its IP, netmask, gateway and DNS servers (comma-separated). A management
    /// card whose address moves takes the connection with it: this one follows to the new address.
    fn set_ip(&self, pif: &str, mode: &str, ip: &str, netmask: &str, gateway: &str, dns: &str) -> Result<(), ApiError>;
    /// A server's own name for itself (its hostname), changed at once (host.set_hostname_live).
    fn set_hostname(&self, host: &str, name: &str) -> Result<(), ApiError>;
    /// Copies a file into an ISO library as `name` (see `iso_file_name`): a new disc in the library,
    /// then the file's bytes sent to it, from this computer or as they arrive from the web.
    /// `address` is the server to send them to for a library that only one server sees; a shared
    /// one takes them on the pool coordinator. A failed or cancelled upload leaves nothing behind.
    fn upload_iso(&self, sr_ref: &str, address: Option<&str>, src: &Source, name: &str, t: &Arc<Transfer>) -> Result<(), ApiError>;
    /// Saves a VM, a template or a snapshot (`obj`) in a .xva file on this computer, compressed on
    /// the server (zstd) with `compress`. A running VM is saved from a snapshot taken for it
    /// (`via_snapshot`), deleted afterwards. The bytes saved. One that fails or is stopped leaves
    /// no file behind (it's written beside, then renamed).
    fn export_vm(&self, obj: &str, to: &Path, compress: bool, via_snapshot: bool, t: &Arc<Transfer>) -> Result<u64, ApiError>;
    /// Brings a .xva file into the pool (see `ImportVm`), then names it, sets its home server and
    /// moves its network cards as asked. The VM (or template) made.
    fn import_vm(&self, imp: &ImportVm, t: &Arc<Transfer>) -> Result<String, ApiError>;
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
    /// How far the server's own task is, per mille plus one (0 while it hasn't said): an export,
    /// whose size isn't known till it ends.
    server_part: AtomicU64,
}

impl Transfer {
    /// The server says its task is this far (0 to 1).
    pub fn set_part(&self, f: f64) {
        self.server_part.store((f.clamp(0.0, 1.0) * 1000.0) as u64 + 1, Ordering::Relaxed);
    }
    /// How far it is: of the bytes to send when that's known, else as the server's task says.
    pub fn fraction(&self) -> Option<f32> {
        let (sent, total) = (self.sent.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed));
        if total > 0 {
            return Some((sent as f64 / total as f64).min(1.0) as f32);
        }
        let p = self.server_part.load(Ordering::Relaxed);
        (p > 0).then(|| (p - 1) as f32 / 1000.0)
    }
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
    /// The address to reach a server over HTTPS (`address`, else the pool's coordinator), and
    /// the session to send along.
    fn http_base(&self, address: Option<&str>) -> (String, String) {
        let (coordinator, session) = {
            let st = self.state.lock().unwrap();
            (st.host.clone(), st.session.clone().unwrap_or_default())
        };
        let host = address.filter(|a| !a.is_empty()).map(str::to_string).unwrap_or(coordinator);
        let host = if host.contains(':') && !host.starts_with('[') { format!("[{}]", host) } else { host };
        let port = if self.port == DEFAULT_PORT { String::new() } else { format!(":{}", self.port) };
        (format!("https://{}{}", host, port), session)
    }

    /// One VM, template or snapshot into a .xva: an HTTP GET of /export (the coordinator sends it
    /// on to the server that has its disks), written beside `to` and renamed once whole, the
    /// server's task looked at meanwhile for how far it is.
    fn export_one(&self, obj: &str, to: &Path, compress: bool, t: &Arc<Transfer>) -> Result<u64, ApiError> {
        let uuid = self.call("VM.get_uuid", vec![json!(obj)], 30_000)?.as_str().unwrap_or_default().to_string();
        let label = to.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let task = self.call("task.create", vec![json!("Export"), json!(label)], 30_000)?.as_str().unwrap_or_default().to_string();
        let (base, session) = self.http_base(None);
        let how = if compress { "zstd" } else { "false" };
        let url = format!("{}/export?session_id={}&uuid={}&task_id={}&use_compression={}", base, session, uuid, task, how);
        if let Some(w) = watched() {
            w.trace.push(Said::Asked, format!("GET /export  uuid={}  use_compression={}", uuid, how));
            w.trace.push(Said::Note, format!("Saving it as {}", to.display()));
        }
        let part = to.with_file_name(format!("{}.part", label));
        let finish = |r: Result<u64, ApiError>| -> Result<u64, ApiError> {
            if r.is_err() {
                let _ = std::fs::remove_file(&part);
            }
            r
        };
        let agent = ureq::AgentBuilder::new()
            .tls_connector(self.tls.clone())
            .timeout_connect(Duration::from_secs(30))
            .timeout_read(Duration::from_secs(600))
            .user_agent(concat!("SimpleVMManager/", env!("CARGO_PKG_VERSION")))
            .build();
        let resp = match agent.get(&url).call() {
            Ok(r) => r,
            Err(e) => {
                let _ = self.call("task.cancel", vec![json!(task)], 30_000);
                let verdict = self.task_result(&task, Duration::from_secs(20));
                let _ = self.call("task.destroy", vec![json!(task)], 30_000);
                return Err(match (verdict, e) {
                    (Err(e), _) if e.code != CANCELLED => e,
                    (_, ureq::Error::Status(code, r)) => ApiError::other(format!("The server answered the export with HTTP {} {}.", code, r.status_text())),
                    (_, e) => ApiError::other(e.to_string()),
                });
            }
        };
        let mut out = match std::fs::File::create(&part) {
            Ok(f) => f,
            Err(e) => {
                let _ = self.call("task.cancel", vec![json!(task)], 30_000);
                let _ = self.call("task.destroy", vec![json!(task)], 30_000);
                return Err(ApiError::other(format!("Couldn't write {}: {}", part.display(), e)));
            }
        };
        // the server's task says how far it is (the file's size isn't known till it ends)
        let done = AtomicBool::new(false);
        let written = std::thread::scope(|sc| {
            sc.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    for _ in 0..20 {
                        if done.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    if let Ok(p) = self.call_raw("task.get_progress", vec![json!(task)], 20_000) {
                        if let Some(f) = p.as_f64().or_else(|| p.as_str().and_then(|x| x.parse().ok())) {
                            t.set_part(f);
                        }
                    }
                }
            });
            let r = (|| -> Result<u64, String> {
                let mut src = resp.into_reader();
                let mut buf = vec![0u8; 1 << 20];
                let mut total = 0u64;
                loop {
                    if t.cancel.load(Ordering::Relaxed) {
                        return Err(String::new());
                    }
                    let n = src.read(&mut buf).map_err(|e| format!("The download broke off: {}", e))?;
                    if n == 0 {
                        break;
                    }
                    out.write_all(&buf[..n]).map_err(|e| format!("Couldn't write {}: {}", part.display(), e))?;
                    total += n as u64;
                    t.sent.fetch_add(n as u64, Ordering::Relaxed);
                }
                out.sync_all().map_err(|e| format!("Couldn't write {}: {}", part.display(), e))?;
                Ok(total)
            })();
            done.store(true, Ordering::Relaxed);
            r
        });
        t.finishing.store(true, Ordering::Relaxed);
        drop(out);
        let cancelled = t.cancel.load(Ordering::Relaxed);
        if cancelled || written.is_err() {
            let _ = self.call("task.cancel", vec![json!(task)], 30_000);
        }
        let verdict = self.task_result(&task, Duration::from_secs(if written.is_ok() && !cancelled { 300 } else { 30 }));
        let _ = self.call("task.destroy", vec![json!(task)], 30_000);
        if cancelled {
            return finish(Err(ApiError { code: CANCELLED.into(), details: vec![] }));
        }
        let bytes = match (verdict, written) {
            (Err(e), _) if e.code != CANCELLED => return finish(Err(e)),
            (_, Err(why)) => return finish(Err(ApiError::other(why))),
            (Ok(true), Ok(n)) => n,
            (_, Ok(_)) => return finish(Err(ApiError::other("The server didn't say the export was whole."))),
        };
        if let Err(e) = std::fs::rename(&part, to) {
            return finish(Err(ApiError::other(format!("Couldn't name it {}: {}", to.display(), e))));
        }
        if let Some(w) = watched() {
            w.trace.push(Said::Answer, format!("{} saved", fmt_bytes(bytes as i64)));
        }
        Ok(bytes)
    }

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

    /// cloud-init's disk for a new VM: a small FAT disk labelled CIDATA with its user-data and
    /// meta-data, on `sr`, written through /import_raw_vdi and attached as its next disk. The
    /// disk goes again if it can't be attached.
    fn add_config_drive(&self, vm: &str, sr: &str, ci: &crate::cloudinit::CloudInit, names: &HashMap<String, String>, tr: &Trace) -> Result<(), ApiError> {
        let t = 120_000;
        let call = |shown: String, method: &str, args: Vec<Value>| {
            tr.push(Said::Asked, shown);
            let started = Instant::now();
            let r = self.call(method, args, t);
            tr.outcome(&r, started.elapsed());
            r
        };
        let me = named(names, vm);
        tr.push(Said::Note, "cloud-init: its disk, read on the first start");
        let uuid = call(format!("VM.get_uuid  vm={}", me), "VM.get_uuid", vec![json!(vm)])?.as_str().unwrap_or_default().to_string();
        let ud = ci.user_data();
        let md = ci.meta_data(&format!("svm-{}", uuid));
        let image = crate::cloudinit::config_drive(&[("user-data", ud.as_bytes()), ("meta-data", md.as_bytes())], u32::from_str_radix(&uuid.replace('-', "")[..8.min(uuid.len())], 16).unwrap_or(0xC1DA_7A00));
        let record = json!({
            "name_label": "cloud-init config", "name_description": "What cloud-init sets up on the VM's first start (it holds its user's password, if one was given): delete it once the VM is set up.",
            "SR": sr, "virtual_size": crate::cloudinit::DRIVE_SIZE, "type": "user", "sharable": false, "read_only": false,
            "other_config": {}, "xenstore_data": {}, "sm_config": { "vmhint": vm }, "tags": [],
        });
        let vdi = call(format!("VDI.create  name=\"cloud-init config\"  SR={}  size=4 MiB", named(names, sr)), "VDI.create", vec![record])?.as_str().unwrap_or_default().to_string();
        let done = (|| -> Result<(), ApiError> {
            // on a server's own storage, written at that server
            let address = if call(format!("SR.get_shared  sr={}", named(names, sr)), "SR.get_shared", vec![json!(sr)])?.as_bool().unwrap_or(true) {
                None
            } else {
                let pbd = call(format!("SR.get_PBDs  sr={}", named(names, sr)), "SR.get_PBDs", vec![json!(sr)])?.as_array().and_then(|a| a.first().cloned());
                match pbd {
                    Some(p) => {
                        let host = call("PBD.get_host".into(), "PBD.get_host", vec![p])?;
                        call(format!("host.get_address  host={}", named(names, host.as_str().unwrap_or_default())), "host.get_address", vec![host])?.as_str().map(String::from)
                    }
                    None => None,
                }
            };
            tr.push(Said::Asked, format!("PUT /import_raw_vdi  vdi=cloud-init config  ({} KB: user-data, meta-data)", crate::cloudinit::DRIVE_SIZE / 1024));
            let started = Instant::now();
            let r = self.write_vdi(&vdi, address.as_deref(), &image);
            tr.outcome(&r.as_ref().map(|_| Value::Null).map_err(|e| e.clone()), started.elapsed());
            r?;
            let device = call(format!("VM.get_allowed_VBD_devices  vm={}", me), "VM.get_allowed_VBD_devices", vec![json!(vm)])?
                .as_array()
                .and_then(|a| a.first().cloned())
                .and_then(|d| d.as_str().map(String::from))
                .ok_or_else(|| ApiError::other("The VM has no room for cloud-init's disk."))?;
            let drive = json!({
                "VM": vm, "VDI": vdi, "userdevice": device, "bootable": false, "mode": "RW", "type": "Disk", "unpluggable": true,
                "empty": false, "other_config": { "owner": "" }, "qos_algorithm_type": "", "qos_algorithm_params": {}, "device": "", "currently_attached": false,
            });
            call(format!("VBD.create  VM={}  VDI=cloud-init config  userdevice={}", me, device), "VBD.create", vec![drive])?;
            Ok(())
        })();
        if done.is_err() {
            let _ = self.call("VDI.destroy", vec![json!(vdi)], t);
        }
        done
    }

    /// Writes `bytes` over a new disk (VDI) through /import_raw_vdi (at `address`, else the
    /// coordinator), then waits for the task's verdict: the PUT is answered before it's written.
    fn write_vdi(&self, vdi: &str, address: Option<&str>, bytes: &[u8]) -> Result<(), ApiError> {
        let task = self.call("task.create", vec![json!("Write a disk"), json!("")], 30_000)?.as_str().unwrap_or_default().to_string();
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
            .timeout(Duration::from_secs(300))
            .redirects(0)
            .user_agent(concat!("SimpleVMManager/", env!("CARGO_PKG_VERSION")))
            .build();
        let sent = agent.put(&url).set("Content-Length", &bytes.len().to_string()).send_bytes(bytes);
        let put_error = match sent {
            Ok(_) => None,
            Err(ureq::Error::Status(code, _)) => Some(ApiError::other(format!("The server answered the disk's writing with HTTP {}.", code))),
            Err(e) => Some(ApiError::other(e.to_string())),
        };
        if put_error.is_some() {
            let _ = self.call("task.cancel", vec![json!(task)], 30_000);
        }
        let verdict = self.task_result(&task, Duration::from_secs(if put_error.is_some() { 30 } else { 300 }));
        let _ = self.call("task.destroy", vec![json!(task)], 30_000);
        match verdict {
            Ok(true) => Ok(()),
            Err(e) if e.code != CANCELLED => Err(e),
            _ => Err(put_error.unwrap_or_else(|| ApiError::other("The server didn't finish writing the disk in time."))),
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
            if let Some(ci) = &v.cloud_init {
                self.add_config_drive(&vm, v.copy_to.as_deref().unwrap_or(&v.sr), ci, &names, tr)?;
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
    /// the rest moved, then it goes off or restarts; enabled again if a step fails (unless it
    /// wasn't taking VMs before). One in maintenance mode stays so once it's back.
    fn host_off(&self, host: &str, reboot: bool) -> Result<(), ApiError> {
        let maintenance = self.call_raw("host.get_other_config", vec![json!(host)], 30_000).map_or(false, |o| s(&o, MAINT).eq_ignore_ascii_case("true"));
        let was_enabled = self.call_raw("host.get_enabled", vec![json!(host)], 30_000).ok().and_then(|v| v.as_bool()).unwrap_or(true);
        self.emptied(host, true, maintenance)?;
        let done = self.call(if reboot { "host.reboot" } else { "host.shutdown" }, vec![json!(host)], ACTION_TIMEOUT_MS).map(|_| ());
        if done.is_err() && was_enabled {
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

    /// No new VMs sent to a server; with `keep`, not even after it restarts (host.disable's
    /// auto_enable, on servers that have it: older ones take VMs again once they've restarted).
    fn disable(&self, host: &str, keep: bool) -> Result<(), ApiError> {
        let t = 60_000;
        if keep {
            match self.call("host.disable", vec![json!(host), json!(false)], t) {
                Err(e) if e.code == "MESSAGE_PARAMETER_COUNT_MISMATCH" => note("an older server: it takes VMs again if it's restarted"),
                r => return r.map(|_| ()),
            }
        }
        self.call("host.disable", vec![json!(host)], t).map(|_| ())
    }

    /// No new VMs sent to a server (with `keep`, even after a restart), the VMs that can't move
    /// elsewhere stopped (with `stop`; without, it fails first when there are any), the rest
    /// moved: the VMs it stopped, and those it moved. Enabled again if a step fails (when it was
    /// taking VMs before).
    fn emptied(&self, host: &str, stop: bool, keep: bool) -> Result<(Vec<String>, Vec<String>), ApiError> {
        let t = 60_000;
        let was_enabled = self.call_raw("host.get_enabled", vec![json!(host)], 30_000).ok().and_then(|v| v.as_bool()).unwrap_or(true);
        if !stop {
            let stuck = self.blocking(host)?;
            if !stuck.is_empty() {
                return Err(ApiError { code: "VMS_CANT_MOVE".into(), details: stuck.into_iter().map(|s| s.0).collect() });
            }
        }
        self.disable(host, keep)?;
        let done = (|| -> Result<(Vec<String>, Vec<String>), ApiError> {
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
            let left: Vec<String> = self
                .call("host.get_resident_VMs", vec![json!(host)], t)?
                .as_array()
                .map(|a| a.iter().filter(|vm| !self.call("VM.get_is_control_domain", vec![(*vm).clone()], t).ok().and_then(|x| x.as_bool()).unwrap_or(true)).map(val_str).collect())
                .unwrap_or_default();
            self.call("host.evacuate", vec![json!(host)], ACTION_TIMEOUT_MS)?;
            Ok((stopped, left))
        })();
        if done.is_err() && was_enabled {
            let _ = self.call("host.enable", vec![json!(host)], t);
        }
        done
    }

    /// What points to storage `sr` (gone): the VM's (or a snapshot's) storage for saved memory
    /// (which, so it can be cleared), the pool's default storage, its or a server's storage for
    /// suspended VMs or crash dumps. In words.
    fn whose_storage(&self, vm: &str, sr: &str) -> Option<(String, Option<String>)> {
        let t = 30_000;
        let suspend_of = |v: &str| self.call_raw("VM.get_suspend_SR", vec![json!(v)], t).ok().map(|x| val_str(&x));
        if suspend_of(vm).as_deref() == Some(sr) {
            return Some(("Its storage for saved memory".into(), Some(vm.to_string())));
        }
        for snap in self.call_raw("VM.get_snapshots", vec![json!(vm)], t).ok().and_then(|x| x.as_array().cloned()).unwrap_or_default() {
            let snap = val_str(&snap);
            if suspend_of(&snap).as_deref() == Some(sr) {
                return Some(("A snapshot's storage for saved memory".into(), Some(snap)));
            }
        }
        for (_, p) in self.all("pool", t).unwrap_or_default() {
            for (field, what) in [("default_SR", "The pool's default storage"), ("suspend_image_SR", "The pool's storage for suspended VMs"), ("crash_dump_SR", "The pool's storage for crash dumps")] {
                if s(&p, field) == sr {
                    return Some((what.into(), None));
                }
            }
        }
        for (_, h) in self.all("host", t).unwrap_or_default() {
            for (field, what) in [("suspend_image_sr", "storage for suspended VMs"), ("crash_dump_sr", "storage for crash dumps")] {
                if s(&h, field) == sr {
                    return Some((format!("{}'s {}", s(&h, "name_label"), what), None));
                }
            }
        }
        None
    }

    /// What each disk takes on its storage (what a copy of it carries), else its size; 0 when
    /// the server doesn't say. Not noted in the job's details.
    fn disks_bytes<'a>(&self, vdis: impl Iterator<Item = &'a str>) -> Vec<i64> {
        let number = |v: Value| match v {
            Value::Number(x) => x.as_i64().unwrap_or(0),
            Value::String(t) => t.trim().parse().unwrap_or(0),
            _ => 0,
        };
        vdis.map(|vdi| {
            let used = self.call_raw("VDI.get_physical_utilisation", vec![json!(vdi)], 30_000).map(number).unwrap_or(0);
            if used > 0 {
                used
            } else {
                self.call_raw("VDI.get_virtual_size", vec![json!(vdi)], 30_000).map(number).unwrap_or(0)
            }
        })
        .collect()
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
        // (how far an earlier task got isn't this one's)
        if let Some(w) = &watch {
            w.permille.store(u64::MAX, Ordering::Relaxed);
        }
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
        let get = self.rrd_getter(address, 15)?;
        let get = |start: i64| get(&format!("start={}&cf=AVERAGE&interval=5&host={}", start, host));
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

    /// A way to ask the server at `address` (the coordinator when empty) for its records: an
    /// rrd_updates query (without the session) to its answer, waited for up to `secs`.
    fn rrd_getter(&self, address: &str, secs: u64) -> Result<impl Fn(&str) -> Result<String, ApiError>, ApiError> {
        let (coordinator, session) = self.session(20_000)?;
        let at = if address.is_empty() { coordinator } else { address.to_string() };
        let at = if at.contains(':') && !at.starts_with('[') { format!("[{}]", at) } else { at };
        let port = if self.port == DEFAULT_PORT { String::new() } else { format!(":{}", self.port) };
        let agent = ureq::AgentBuilder::new().tls_connector(self.tls.clone()).timeout_connect(Duration::from_secs(5)).timeout(Duration::from_secs(secs)).build();
        Ok(move |query: &str| -> Result<String, ApiError> {
            let url = format!("https://{}{}/rrd_updates?session_id={}&{}", at, port, session, query);
            agent.get(&url).call().map_err(web_error)?.into_string().map_err(|e| ApiError::other(e.to_string()))
        })
    }

    fn all(&self, cls: &str, timeout_ms: u64) -> Result<Map<String, Value>, ApiError> {
        Ok(self.call(&format!("{}.get_all_records", cls), vec![], timeout_ms)?.as_object().cloned().unwrap_or_default())
    }
}

/// The calls that can take long and that the server can run as a task (an Async form): only
/// these go as tasks. A field's setter (set_*, add_to_*, remove_from_*) has no Async form: the
/// server answers "No async mode for this operation".
const TASKS: [&str; 39] = [
    "VM.clone", "VM.copy", "VM.provision", "VM.snapshot", "VM.checkpoint", "VM.revert", "VM.start", "VM.start_on", "VM.resume", "VM.resume_on",
    "VM.suspend", "VM.clean_shutdown", "VM.hard_shutdown", "VM.clean_reboot", "VM.hard_reboot", "VM.destroy", "VDI.create", "VDI.destroy",
    "VDI.resize", "VBD.eject", "VBD.insert", "VBD.plug", "VIF.plug", "VIF.unplug", "VIF.move", "PBD.plug", "PBD.unplug", "SR.create", "SR.scan",
    "SR.forget", "host.evacuate", "host.shutdown", "pool.sync_updates", "host.apply_updates", "VM.pool_migrate", "VM.migrate_send",
    "VDI.pool_migrate", "VDI.copy", "host.power_on",
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
/// The references in a task's result ("<value><array><data><value>OpaqueRef:…"), in order.
fn task_refs(result: &str) -> Vec<String> {
    result
        .match_indices("OpaqueRef:")
        .map(|(i, _)| result[i..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == ':' || *c == '-').collect::<String>())
        .filter(|r| r != "OpaqueRef:NULL")
        .collect()
}

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

/// A snapshot schedule (VMSS record `r`, known as `rref`).
fn schedule_of(rref: &str, r: &Value) -> ScheduleInfo {
    let when = r.get("schedule").cloned().unwrap_or(Value::Null);
    let num = |k: &str| s(&when, k).trim().parse::<u32>().unwrap_or(0);
    let kind = s(r, "type");
    ScheduleInfo {
        rref: rref.to_string(),
        uuid: s(r, "uuid"),
        name: s(r, "name_label"),
        description: s(r, "name_description"),
        enabled: b(r, "enabled"),
        memory: kind == "checkpoint",
        kind,
        keep: n(r, "retained_snapshots").clamp(1, 10),
        every: Every::parse(&s(r, "frequency")),
        hour: num("hour").min(23),
        min: num("min").min(59),
        days: s(&when, "days").split(',').map(|d| d.trim().to_string()).filter(|d| !d.is_empty()).collect(),
        last_run: parse_xen_time(&s(r, "last_run_time")).filter(|t| *t > 0),
        vms: list(r, "VMs").into_iter().filter(|v| v != "OpaqueRef:NULL").collect(),
    }
}

/// What's written on a disk on thin storage: its own file and the ones it's built on (a
/// snapshot's or a template's: sm_config's vhd-parent), which go with it when it's copied.
fn written(vdi: &Value, by_uuid: &HashMap<String, &Value>) -> i64 {
    let mut sum = n(vdi, "physical_utilisation");
    let mut at = vdi;
    for _ in 0..64 {
        let parent = s(at.get("sm_config").unwrap_or(&Value::Null), "vhd-parent");
        let Some(p) = by_uuid.get(&parent) else { break };
        sum += n(p, "physical_utilisation");
        at = p;
    }
    sum
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
/// Where a template this app makes keeps when it was made (seconds since 1970): XenServer keeps no
/// date of its own for that.
const CREATED_KEY: &str = "simple_vm_manager_created";

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
    // (the server's "never" is the start of 1970)
    vm.created = s(other, CREATED_KEY).trim().parse::<i64>().ok().or_else(|| parse_xen_time(&s(metrics, "install_time"))).filter(|t| *t > 86_400);
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
        let mut no_vmss = false;
        for (_, r) in self.all("pool", t)? {
            pd.name = s(&r, "name_label");
            // a license without snapshot schedules says so here
            no_vmss = s(r.get("restrictions").unwrap_or(&Value::Null), "restrict_vmss") == "true";
            master = s(&r, "master");
            pd.default_sr = Some(s(&r, "default_SR")).filter(|x| !x.is_empty() && x != "OpaqueRef:NULL");
            repos = list(&r, "repositories");
            pd.suspend_sr = Some(s(&r, "suspend_image_SR")).filter(|x| !x.is_empty() && x != "OpaqueRef:NULL");
            pd.crash_dump_sr = Some(s(&r, "crash_dump_SR")).filter(|x| !x.is_empty() && x != "OpaqueRef:NULL");
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
            let other = r.get("other_config").cloned().unwrap_or(Value::Null);
            let uuids = |k: &str| s(&other, k).split(',').map(str::trim).filter(|u| !u.is_empty()).map(String::from).collect::<Vec<_>>();
            pd.hosts.push(HostInfo {
                is_master: k == master,
                rref: k,
                uuid: s(&r, "uuid"),
                name: s(&r, "name_label"),
                hostname: s(&r, "hostname"),
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
                suspend_sr: Some(s(&r, "suspend_image_sr")).filter(|x| !x.is_empty() && x != "OpaqueRef:NULL"),
                crash_dump_sr: Some(s(&r, "crash_dump_sr")).filter(|x| !x.is_empty() && x != "OpaqueRef:NULL"),
                updates_applied: s(&r, "latest_synced_updates_applied"),
                pending_guidance: list(&r, "pending_guidances").into_iter().chain(list(&r, "pending_guidances_recommended")).collect(),
                last_update: parse_xen_time(&s(&r, "last_software_update")),
                nics: vec![], // filled in from the PIFs below
                power_on_mode: s(&r, "power_on_mode"),
                maintenance: s(&other, MAINT).eq_ignore_ascii_case("true"),
                maint_moved: uuids(MAINT_MOVED),
                maint_stopped: uuids(MAINT_STOPPED),
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
        // what each kind of storage can do (its SM's features): moving a running VM's disks
        let mut features: HashMap<String, Vec<String>> = HashMap::new();
        for (_, r) in self.all("SM", t).unwrap_or_default() {
            let mut f: Vec<String> = r.get("features").and_then(|v| v.as_object()).map(|o| o.keys().cloned().collect()).unwrap_or_default();
            f.extend(list(&r, "capabilities"));
            features.insert(s(&r, "type"), f);
        }
        for (k, r) in self.all("SR", t)? {
            let shared = b(&r, "shared");
            let can = |what: &[&str]| features.get(&s(&r, "type")).map_or(true, |f| f.iter().any(|x| what.contains(&x.as_str())));
            let links = plugged.get(&k).cloned().unwrap_or_default();
            let host = if !shared && links.len() == 1 { Some(links[0].0.clone()) } else { None };
            let config = configs.get(&k).cloned().unwrap_or(Value::Null);
            let share = match s(&config, "type").as_str() {
                "cifs" => format!("Windows share (SMB {})", or_default(&s(&config, "vers"), "3.0")),
                "nfs_iso" => format!("NFS (v{})", or_default(&s(&config, "nfsversion"), "3")),
                _ if s(&r, "type") == "nfs" => format!("NFS (v{})", or_default(&s(&config, "nfsversion"), "3")),
                _ => String::new(),
            };
            // NFS storage for disks names its server and folder apart, iSCSI its target and IQN
            let location = match (s(&r, "type").as_str(), s(&config, "server")) {
                ("nfs", server) if !server.is_empty() => format!("{}:{}", server, s(&config, "serverpath")),
                ("lvmoiscsi", _) => [s(&config, "target"), s(&config, "targetIQN")].into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join("  ·  "),
                _ => s(&config, "location"),
            };
            pd.srs.push(SrInfo {
                location,
                // a server's own disk (a DVD drive's "location" is its device, not this)
                device: if s(&r, "type") == "udev" { String::new() } else { s(&config, "device") },
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
                live_out: can(&["VDI_MIRROR"]),
                live_in: can(&["VDI_MIRROR", "VDI_MIRROR_IN"]),
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
                device: String::new(), // from the PIFs below
                vlan: -1,
            });
        }
        // each server's network cards: the physical ones and bonds, not a bond's members or VLANs
        // (if these can't be read, the rest still shows); and each network's card and VLAN
        let pif_metrics = self.all("PIF_metrics", t).unwrap_or_default();
        let mut cards: HashMap<String, (String, i64)> = HashMap::new();
        for (pif, r) in self.all("PIF", t).unwrap_or_default() {
            let null = |k: &str| matches!(s(&r, k).as_str(), "" | "OpaqueRef:NULL");
            let vlan = r.get("VLAN").map_or(-1, |_| n(&r, "VLAN"));
            if null("bond_slave_of") {
                cards.entry(s(&r, "network")).or_insert((s(&r, "device"), vlan));
            }
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
                mac: s(&r, "MAC"),
                netmask: s(&r, "netmask"),
                pif,
                mode: s(&r, "ip_configuration_mode"),
                gateway: s(&r, "gateway"),
                dns: s(&r, "DNS"),
            };
            if let Some(h) = pd.hosts.iter_mut().find(|h| h.rref == s(&r, "host")) {
                h.nics.push(nic);
            }
        }
        for h in pd.hosts.iter_mut() {
            h.nics.sort_by(|a, b| a.device.cmp(&b.device));
        }
        for net in pd.networks.iter_mut() {
            if let Some((device, vlan)) = cards.get(&net.rref) {
                (net.device, net.vlan) = (device.clone(), *vlan);
            }
        }
        let vbds = self.all("VBD", t)?;
        let vdis = self.all("VDI", t)?;
        let vdi_by_uuid: HashMap<String, &Value> = vdis.values().map(|v| (s(v, "uuid"), v)).collect();
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
            let time = parse_xen_time(&s(r, "snapshot_time"));
            snapshots.entry(s(r, "snapshot_of")).or_default().push(SnapshotInfo {
                rref: k.clone(),
                // (a schedule's "20261120-1843" as its date and time here)
                name: snapshot_name(&s(r, "name_label"), b(r, "is_vmss_snapshot"), time),
                description: s(r, "name_description"),
                time,
                // a checkpoint's record is suspended: its memory is saved with it
                memory: s(r, "power_state") == "Suspended",
                was_running: s(&info, "power-state-at-snapshot") == "Running",
                scheduled: b(r, "is_vmss_snapshot"),
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
            vm.schedule = Some(s(&r, "snapshot_schedule")).filter(|x| !x.is_empty() && x != "OpaqueRef:NULL");
            let parent = s(&r, "parent");
            vm.parent = vm.snapshots.iter().any(|x| x.rref == parent).then_some(parent);
            for vbd_ref in list(&r, "VBDs") {
                let Some(vbd) = vbds.get(&vbd_ref) else { continue };
                let cd = s(vbd, "type") == "CD";
                let mut d = VmDisk { device: s(vbd, "userdevice"), dev: s(vbd, "device"), kind: if cd { "DVD drive".into() } else { "Disk".into() }, vbd: vbd_ref.clone(), ..Default::default() };
                if let Some(vdi) = vdis.get(&s(vbd, "VDI")) {
                    d.vdi = s(vbd, "VDI");
                    if vm.storage_host.is_none() {
                        vm.storage_host = sr_hosts.get(&s(vdi, "SR")).cloned();
                    }
                    d.name = s(vdi, "name_label");
                    d.size = n(vdi, "virtual_size");
                    d.sr = s(vdi, "SR");
                    d.sr_name = sr_names.get(&d.sr).cloned().unwrap_or_default();
                    if pd.srs.iter().any(|x| x.rref == d.sr && x.is_thin()) {
                        d.used = written(vdi, &vdi_by_uuid);
                    }
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
        // its snapshot schedules (XenServer 7.2 and later; a license may leave them out)
        match self.all("VMSS", t) {
            Ok(all) => {
                pd.schedules = all.iter().map(|(k, r)| schedule_of(k, r)).collect();
                pd.schedules.sort_by_key(|x| x.name.to_lowercase());
            }
            Err(e) if e.code == "MESSAGE_METHOD_UNKNOWN" || e.code == "UNKNOWN_CLASS" || e.code == "MESSAGE_DEPRECATED" => {
                pd.schedules_off = Some("This pool's version has no snapshot schedules (XenServer 7.2 and later have them).".into());
            }
            Err(_) => {}
        }
        if no_vmss {
            pd.schedules_off = Some("The pool's license doesn't include snapshot schedules.".into());
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

    /// SR.create on the pool coordinator, shared: the pool gives every server a connection to it
    /// and plugs them all in.
    fn create_nfs_storage(&self, st: &NfsStorage) -> Result<String, ApiError> {
        let t = 30_000;
        let coordinator = self.all("pool", t)?.values().map(|p| s(p, "master")).next().unwrap_or_default();
        let config: Map<String, Value> = st.device_config().into_iter().map(|(k, v)| (k, json!(v))).collect();
        let args = vec![json!(coordinator), Value::Object(config), json!(0), json!(st.name), json!(st.description), json!("nfs"), json!("user"), json!(true), json!({})];
        let sr = self.call("SR.create", args, 300_000)?;
        Ok(sr.as_str().unwrap_or_default().to_string())
    }

    fn create_network(&self, nw: &NewNetwork) -> Result<String, ApiError> {
        let t = 60_000;
        let record = json!({ "name_label": nw.name, "name_description": nw.description, "MTU": nw.mtu, "other_config": {}, "tags": [] });
        let net = val_str(&self.call("network.create", vec![record], t)?);
        if let Some((device, tag)) = &nw.vlan {
            // the coordinator's card of that name: the pool makes the VLAN on every server's
            let coordinator = self.all("pool", t)?.values().map(|p| s(p, "master")).next().unwrap_or_default();
            let pif = self.all("PIF", t)?.into_iter().find(|(_, p)| {
                s(p, "host") == coordinator && s(p, "device") == *device && n(p, "VLAN") < 0 && matches!(s(p, "bond_slave_of").as_str(), "" | "OpaqueRef:NULL")
            });
            let made = match pif {
                Some((pif, _)) => self.call("pool.create_VLAN_from_PIF", vec![json!(pif), json!(net), json!(tag)], 300_000).map(|_| ()),
                None => Err(ApiError::other(format!("the pool's coordinator has no card called {}", device))),
            };
            if let Err(e) = made {
                note("the VLAN couldn't be made: the new network is taken away again");
                let _ = self.call("network.destroy", vec![json!(net)], t);
                return Err(e);
            }
        }
        Ok(net)
    }

    fn delete_network(&self, net: &str) -> Result<(), ApiError> {
        let t = 60_000;
        for pif in self.call("network.get_PIFs", vec![json!(net)], t)?.as_array().cloned().unwrap_or_default() {
            let r = self.call("PIF.get_record", vec![pif.clone()], t)?;
            if n(&r, "VLAN") < 0 {
                return Err(ApiError { code: "NETWORK_CONTAINS_PIF".into(), details: vec![s(&r, "device")] });
            }
            // a VLAN: taken off its card (the server unplugs it first)
            let vlan = s(&r, "VLAN_master_of");
            if !matches!(vlan.as_str(), "" | "OpaqueRef:NULL") {
                self.call("VLAN.destroy", vec![json!(vlan)], 120_000)?;
            } else {
                self.call("PIF.destroy", vec![pif], 120_000)?;
            }
        }
        self.call("network.destroy", vec![json!(net)], t).map(|_| ())
    }

    /// SR.probe with only the server: the NFS driver answers with the list of its exports (as an
    /// error, "the folder is missing", that carries the list), or with it as the result.
    fn nfs_exports(&self, server: &str) -> Result<Vec<String>, ApiError> {
        let t = 30_000;
        let coordinator = self.all("pool", t)?.values().map(|p| s(p, "master")).next().unwrap_or_default();
        let args = vec![json!(coordinator), json!({ "server": server }), json!("nfs"), json!({})];
        match self.call_raw("SR.probe", args, 90_000) {
            Ok(v) => Ok(nfs_exports(v.as_str().unwrap_or_default())),
            Err(e) => {
                let found = nfs_exports(&e.details.join("\n"));
                if found.is_empty() {
                    Err(e)
                } else {
                    Ok(found)
                }
            }
        }
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
            // when it was made, which XenServer doesn't keep (a clone has its source's install date)
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let _ = self.call("VM.remove_from_other_config", vec![json!(tpl), json!(CREATED_KEY)], t);
            self.call("VM.add_to_other_config", vec![json!(tpl), json!(CREATED_KEY), json!(now.to_string())], t)?;
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
            Some(sr) => {
                // its disks' bytes go to the other storage
                let (_, disks) = self.vm_disks(vm)?;
                let total = self.disks_bytes(disks.iter().map(|d| d.1.as_str())).iter().sum();
                copying(0, total, total);
                self.call("VM.copy", vec![json!(vm), json!(name), json!(sr)], 6 * 3_600_000)?
            }
        };
        Ok(made.as_str().unwrap_or_default().to_string())
    }

    fn save_schedule(&self, x: &ScheduleInfo) -> Result<String, ApiError> {
        let t = 30_000;
        let schedule: serde_json::Map<String, Value> = x.schedule_map().into_iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
        let make = || -> Result<String, ApiError> {
            let rec = json!({
                "name_label": x.name, "name_description": x.description, "enabled": x.enabled, "type": x.kind_to_save(),
                "retained_snapshots": x.keep, "frequency": x.every.api(), "schedule": schedule,
            });
            Ok(val_str(&self.call("VMSS.create", vec![rec], t)?))
        };
        let refs = |v: Value| -> Vec<String> { v.as_array().map(|a| a.iter().map(val_str).filter(|r| r != "OpaqueRef:NULL").collect()).unwrap_or_default() };
        // (its reference, and the VMs on it now)
        let (vmss, on) = if x.rref.is_empty() {
            (make()?, vec![])
        } else {
            let on = refs(self.call("VMSS.get_VMs", vec![json!(x.rref)], t)?);
            if val_str(&self.call("VMSS.get_frequency", vec![json!(x.rref)], t)?) != x.every.api() {
                // another frequency takes a schedule of its own: a new one made, its VMs moved
                // onto it, the old one deleted
                note("Another frequency: a new schedule takes its place, with its VMs");
                let made = make()?;
                for vm in &on {
                    self.call("VM.set_snapshot_schedule", vec![json!(vm), json!(made)], t)?;
                }
                self.call("VMSS.destroy", vec![json!(x.rref)], t)?;
                (made, on)
            } else {
                let sets = [
                    ("VMSS.set_name_label", json!(x.name)),
                    ("VMSS.set_name_description", json!(x.description)),
                    ("VMSS.set_type", json!(x.kind_to_save())),
                    ("VMSS.set_retained_snapshots", json!(x.keep)),
                    ("VMSS.set_schedule", Value::Object(schedule.clone())),
                    ("VMSS.set_enabled", json!(x.enabled)),
                ];
                for (method, value) in sets {
                    self.call(method, vec![json!(x.rref), value], t)?;
                }
                (x.rref.clone(), on)
            }
        };
        // its VMs: those listed on it, the others off
        for vm in x.vms.iter().filter(|v| !on.contains(v)) {
            self.call("VM.set_snapshot_schedule", vec![json!(vm), json!(vmss)], t)?;
        }
        for vm in on.iter().filter(|v| !x.vms.contains(v)) {
            self.call("VM.set_snapshot_schedule", vec![json!(vm), json!("OpaqueRef:NULL")], t)?;
        }
        Ok(vmss)
    }

    fn delete_schedule(&self, vmss: &str) -> Result<(), ApiError> {
        let t = 30_000;
        let on = self.call("VMSS.get_VMs", vec![json!(vmss)], t)?;
        for vm in on.as_array().into_iter().flatten().map(val_str).filter(|r| r != "OpaqueRef:NULL") {
            self.call("VM.set_snapshot_schedule", vec![json!(vm), json!("OpaqueRef:NULL")], t)?;
        }
        self.call("VMSS.destroy", vec![json!(vmss)], t).map(|_| ())
    }

    fn set_vm_schedule(&self, vms: &[String], vmss: Option<&str>) -> Result<(), ApiError> {
        for vm in vms {
            self.call("VM.set_snapshot_schedule", vec![json!(vm), json!(vmss.unwrap_or("OpaqueRef:NULL"))], 30_000)?;
        }
        Ok(())
    }

    fn set_schedule_enabled(&self, vmss: &str, on: bool) -> Result<(), ApiError> {
        self.call("VMSS.set_enabled", vec![json!(vmss), json!(on)], 30_000).map(|_| ())
    }

    fn run_schedule(&self, vmss: &str) -> Result<String, ApiError> {
        // (as long as its VMs' snapshots take, one after another)
        self.call("VMSS.snapshot_now", vec![json!(vmss)], 4 * 3_600_000).map(|v| val_str(&v))
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

    fn messages(&self, since: Option<i64>) -> Result<Vec<XenMessage>, ApiError> {
        let v = match since {
            None => self.call("message.get_all_records", vec![], 60_000)?,
            Some(at) => self.call("message.get_since", vec![json!(xen_time_text(at))], 30_000)?,
        };
        let map = v.as_object().cloned().unwrap_or_default();
        Ok(map
            .into_iter()
            .map(|(k, r)| XenMessage { rref: k, name: s(&r, "name"), priority: n(&r, "priority"), cls: s(&r, "cls"), obj_uuid: s(&r, "obj_uuid"), time: parse_xen_time(&s(&r, "timestamp")).unwrap_or(0), body: s(&r, "body") })
            .collect())
    }

    fn dismiss_messages(&self, refs: &[String]) -> Result<(), ApiError> {
        // all at once where the server can (newer ones), else one by one
        if refs.len() > 1 {
            match self.call("message.destroy_many", vec![json!(refs)], 60_000) {
                Ok(_) => return Ok(()),
                Err(e) if e.code == "MESSAGE_METHOD_UNKNOWN" || e.code == "HANDLE_INVALID" => {}
                Err(e) => return Err(e),
            }
        }
        for r in refs {
            match self.call("message.destroy", vec![json!(r)], 20_000) {
                Ok(_) => {}
                Err(e) if e.code == "HANDLE_INVALID" => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn alert_settings(&self, cls: &str, obj: &str) -> Result<AlertSettings, ApiError> {
        let t = 20_000;
        let own = self.call(&format!("{}.get_other_config", cls), vec![json!(obj)], t)?;
        let pool = self.call("pool.get_all", vec![], t)?.as_array().and_then(|a| a.first().cloned()).unwrap_or(Value::Null);
        let pool = self.call("pool.get_other_config", vec![pool], t)?;
        Ok(AlertSettings { perfmon: s(&own, "perfmon"), mail_to: s(&pool, "mail-destination"), mail_via: s(&pool, "ssmtp-mailhub") })
    }

    fn set_alert_settings(&self, cls: &str, obj: &str, perfmon: &str, mail: Option<(String, String)>) -> Result<(), ApiError> {
        let t = 20_000;
        // (taking out a key that isn't there is fine)
        self.call(&format!("{}.remove_from_other_config", cls), vec![json!(obj), json!("perfmon")], t)?;
        if !perfmon.is_empty() {
            self.call(&format!("{}.add_to_other_config", cls), vec![json!(obj), json!("perfmon"), json!(perfmon)], t)?;
        }
        if let Some((to, via)) = mail {
            let pool = self.call("pool.get_all", vec![], t)?.as_array().and_then(|a| a.first().cloned()).unwrap_or(Value::Null);
            for (k, v) in [("mail-destination", to), ("ssmtp-mailhub", via)] {
                self.call("pool.remove_from_other_config", vec![pool.clone(), json!(k)], t)?;
                if !v.trim().is_empty() {
                    self.call("pool.add_to_other_config", vec![pool.clone(), json!(k), json!(v.trim())], t)?;
                }
            }
        }
        // each server's perfmon reads its limits again now (else within minutes); one that
        // doesn't answer reads them when it's back
        let hosts = self.call("host.get_all", vec![], t)?;
        for h in hosts.as_array().cloned().unwrap_or_default() {
            let _ = self.call("host.call_plugin", vec![h, json!("perfmon"), json!("refresh"), json!({})], t);
        }
        Ok(())
    }

    fn history(&self, address: &str, vm_uuid: Option<&str>, span: Span) -> Result<Records, ApiError> {
        // a year of a busy server's records is a few megabytes
        let get = self.rrd_getter(address, 60)?;
        let whose = match vm_uuid {
            Some(u) => format!("vm_uuid={}", u),
            None => "host=true".to_string(),
        };
        let ask = |start: i64| -> Result<(String, Records), ApiError> {
            let text = get(&format!("start={}&cf=AVERAGE&interval={}&{}", start, span.step(), whose))?;
            let r = read_records(&text);
            Ok((text, r))
        };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let (mut text, mut r) = ask(span.start(now))?;
        // asked by this computer's clock: when the server's differs, again by its own (the span
        // it keeps ends at its own time)
        if let Some(end) = rrd_end(&text).filter(|e| (*e - now).abs() > span.step().max(10)) {
            (text, r) = ask(span.start(end))?;
        }
        if r.times.is_empty() || r.cols.is_empty() {
            let head: String = text.trim().chars().take(160).collect();
            return Err(ApiError::other(format!("the server sent no records for it ({} bytes): {}", text.len(), head)));
        }
        Ok(r)
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
        self.emptied(host, stop, false).map(|(stopped, moved)| (stopped, moved.len()))
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

    fn power_on_host(&self, host: &str) -> Result<(), ApiError> {
        let t = 60_000;
        if val_str(&self.call("host.get_power_on_mode", vec![json!(host)], t)?).is_empty() {
            note("no way to turn it on was set: set to Wake-on-LAN");
            self.call("host.set_power_on_mode", vec![json!(host), json!("wake-on-lan"), json!({})], t)?;
        }
        self.call("host.power_on", vec![json!(host)], 15 * 60_000).map(|_| ())
    }

    fn enter_maintenance(&self, host: &str, stop: bool) -> Result<(Vec<String>, usize), ApiError> {
        let t = 60_000;
        let (stopped, moved) = self.emptied(host, stop, true)?;
        // marked as XenCenter marks it, with the VMs to bring back (by uuid)
        let uuids = |refs: &[String]| refs.iter().filter_map(|r| self.call("VM.get_uuid", vec![json!(r)], t).ok().map(|u| val_str(&u))).collect::<Vec<_>>().join(",");
        let marks = [(MAINT, "true".to_string()), (MAINT_MOVED, uuids(&moved)), (MAINT_STOPPED, uuids(&stopped))];
        for (k, v) in &marks {
            let _ = self.call_raw("host.remove_from_other_config", vec![json!(host), json!(k)], t);
            self.call("host.add_to_other_config", vec![json!(host), json!(k), json!(v)], t)?;
        }
        Ok((stopped, moved.len()))
    }

    fn exit_maintenance(&self, host: &str, back: bool) -> Result<Vec<String>, ApiError> {
        let t = 60_000;
        let other = self.call("host.get_other_config", vec![json!(host)], t)?;
        let uuids = |k: &str| s(&other, k).split(',').map(str::trim).filter(|u| !u.is_empty()).map(String::from).collect::<Vec<_>>();
        let (moved, stopped) = (uuids(MAINT_MOVED), uuids(MAINT_STOPPED));
        self.call("host.enable", vec![json!(host)], t)?;
        for k in [MAINT, MAINT_MOVED, MAINT_STOPPED, MAINT_SUSPENDED] {
            if !s(&other, k).is_empty() {
                let _ = self.call("host.remove_from_other_config", vec![json!(host), json!(k)], t);
            }
        }
        let mut failed = vec![];
        if !back {
            return Ok(failed);
        }
        for (uuid, start) in moved.iter().map(|u| (u, false)).chain(stopped.iter().map(|u| (u, true))) {
            // (one deleted since is let go)
            let Ok(vm) = self.call_raw("VM.get_by_uuid", vec![json!(uuid)], t).map(|v| val_str(&v)) else { continue };
            let name = self.call_raw("VM.get_name_label", vec![json!(vm)], t).map(|v| val_str(&v)).unwrap_or_else(|_| uuid.clone());
            let power = self.call("VM.get_power_state", vec![json!(vm)], t).map(|v| val_str(&v)).unwrap_or_default();
            let done = if start {
                // shut down by maintenance: started here again, if it's still off
                if power != "Halted" {
                    continue;
                }
                self.call("VM.start_on", vec![json!(vm), json!(host), json!(false), json!(false)], ACTION_TIMEOUT_MS)
            } else {
                // moved off: back here, if it still runs elsewhere
                let on = self.call("VM.get_resident_on", vec![json!(vm)], t).map(|v| val_str(&v)).unwrap_or_default();
                if power != "Running" || on == host {
                    continue;
                }
                self.call("VM.pool_migrate", vec![json!(vm), json!(host), json!({"live": "true"})], ACTION_TIMEOUT_MS)
            };
            if let Err(e) = done {
                failed.push(format!("{}: {}", name, e.message()));
            }
        }
        Ok(failed)
    }

    fn start_vm_on(&self, vm: &str, host: &str) -> Result<(), ApiError> {
        self.call("VM.start_on", vec![json!(vm), json!(host), json!(false), json!(false)], ACTION_TIMEOUT_MS).map(|_| ())
    }

    fn make_default_sr(&self, sr: &str) -> Result<(), ApiError> {
        let pool = self.pool_ref()?;
        for m in ["pool.set_default_SR", "pool.set_suspend_image_SR", "pool.set_crash_dump_SR"] {
            self.call(m, vec![pool.clone(), json!(sr)], 60_000)?;
        }
        Ok(())
    }

    fn vm_disks(&self, vm: &str) -> Result<(bool, Vec<(String, String, String)>), ApiError> {
        let t = 60_000;
        let running = self.call("VM.get_power_state", vec![json!(vm)], t)?.as_str() == Some("Running");
        let mut out = vec![];
        for vbd in self.call("VM.get_VBDs", vec![json!(vm)], t)?.as_array().cloned().unwrap_or_default() {
            let r = self.call("VBD.get_record", vec![vbd], t)?;
            if s(&r, "type") == "Disk" && !b(&r, "empty") {
                let vdi = s(&r, "VDI");
                let sr = val_str(&self.call("VDI.get_SR", vec![json!(vdi)], t)?);
                out.push((s(&r, "userdevice"), vdi, sr));
            }
        }
        Ok((running, out))
    }

    fn move_vm(&self, m: &MoveVm) -> Result<(), ApiError> {
        let (t, long) = (60_000, ACTION_TIMEOUT_MS);
        let vm = json!(m.vm);
        if let Some(vbd) = &m.eject {
            note("its DVD drive emptied first: its disc's library isn't where it goes");
            self.call("VBD.eject", vec![json!(vbd)], 120_000)?;
        }
        if m.running {
            let now = val_str(&self.call("VM.get_resident_on", vec![vm.clone()], t)?);
            if m.disks.is_empty() {
                if now != m.host {
                    self.call("VM.pool_migrate", vec![vm, json!(m.host), json!({ "live": "true" })], long)?;
                }
            } else if now == m.host {
                // only its disks, as it runs, one after another
                let sizes = self.disks_bytes(m.disks.iter().map(|d| d.0.as_str()));
                let total = sizes.iter().sum();
                let mut before = 0;
                for ((vdi, sr), size) in m.disks.iter().zip(&sizes) {
                    copying(before, *size, total);
                    self.call("VDI.pool_migrate", vec![json!(vdi), json!(sr), json!({})], long)?;
                    before += size;
                }
            } else {
                // to the server with its disks: the server's management network carries them
                let net = self
                    .all("PIF", t)?
                    .values()
                    .find(|p| s(p, "host") == m.host && b(p, "management"))
                    .map(|p| s(p, "network"))
                    .ok_or_else(|| ApiError::other("the server has no management network to receive it on"))?;
                let dest = self.call("host.migrate_receive", vec![json!(m.host), json!(net), json!({})], t)?;
                // every disk in the map (xapi wants them all): those staying, to where they are
                let (_, now) = self.vm_disks(&m.vm)?;
                let vdi_map: Map<String, Value> = now.iter().map(|(_, vdi, sr)| (vdi.clone(), json!(m.disks.iter().find(|d| d.0 == *vdi).map_or(sr.as_str(), |d| d.1.as_str())))).collect();
                let total: i64 = self.disks_bytes(m.disks.iter().map(|d| d.0.as_str())).iter().sum();
                let send = || -> Result<(), ApiError> {
                    copying(0, total, total);
                    let args = vec![vm.clone(), dest.clone(), json!(true), Value::Object(vdi_map.clone()), json!({}), json!({ "force": "false" }), json!({})];
                    match self.call("VM.migrate_send", args, long) {
                        // (servers older than vGPUs: without its map)
                        Err(e) if e.code == "MESSAGE_PARAMETER_COUNT_MISMATCH" => {
                            self.call("VM.migrate_send", vec![vm.clone(), dest.clone(), json!(true), Value::Object(vdi_map.clone()), json!({}), json!({ "force": "false" })], long).map(|_| ())
                        }
                        r => r.map(|_| ()),
                    }
                };
                match send() {
                    // a setting pointing to storage that's gone: the VM's own (or a snapshot's)
                    // storage for saved memory is cleared, as XenServer itself does after a move,
                    // and it's tried again; another (the pool's, a server's) is said
                    Err(e) if e.code == "HANDLE_INVALID" && e.details.first().map(String::as_str) == Some("SR") => {
                        let gone = e.details.get(1).cloned().unwrap_or_default();
                        match self.whose_storage(&m.vm, &gone) {
                            Some((what, Some(holder))) => {
                                note(format!("{} pointed to storage that no longer exists: cleared (as XenServer does after a move); trying again", what));
                                self.call("VM.set_suspend_SR", vec![json!(holder), json!("OpaqueRef:NULL")], t)?;
                                send()?;
                            }
                            Some((what, None)) => return Err(ApiError { code: "STORAGE_GONE".into(), details: vec![what, gone] }),
                            None => return Err(e),
                        }
                    }
                    r => r?,
                }
            }
            return Ok(());
        }
        // off: each disk copied to its new storage and swapped in for the old one
        let vbds = self.call("VM.get_VBDs", vec![vm.clone()], t)?.as_array().cloned().unwrap_or_default();
        let sizes = self.disks_bytes(m.disks.iter().map(|d| d.0.as_str()));
        let total = sizes.iter().sum();
        let mut before = 0;
        for ((vdi, sr), size) in m.disks.iter().zip(&sizes) {
            copying(before, *size, total);
            before += size;
            let mut found = None;
            for vbd in &vbds {
                let r = self.call("VBD.get_record", vec![vbd.clone()], t)?;
                if s(&r, "VDI") == *vdi {
                    found = Some((vbd.clone(), r));
                    break;
                }
            }
            let (vbd, r) = found.ok_or_else(|| ApiError::other("one of its disks isn't attached to it any more"))?;
            let copy = val_str(&self.call("VDI.copy", vec![json!(vdi), json!(sr)], long)?);
            let drive = json!({
                "VM": m.vm, "VDI": copy, "userdevice": s(&r, "userdevice"), "bootable": b(&r, "bootable"), "mode": s(&r, "mode"),
                "type": s(&r, "type"), "unpluggable": b(&r, "unpluggable"), "empty": false, "other_config": r.get("other_config").cloned().unwrap_or(json!({})),
                "qos_algorithm_type": s(&r, "qos_algorithm_type"), "qos_algorithm_params": r.get("qos_algorithm_params").cloned().unwrap_or(json!({})),
                "device": "", "currently_attached": false,
            });
            // the old drive goes first (its position is the new one's); put back if the new fails
            self.call("VBD.destroy", vec![vbd], t)?;
            if let Err(e) = self.call("VBD.create", vec![drive], t) {
                let _ = self.call("VDI.destroy", vec![json!(copy)], long);
                let back = json!({
                    "VM": m.vm, "VDI": vdi, "userdevice": s(&r, "userdevice"), "bootable": b(&r, "bootable"), "mode": s(&r, "mode"),
                    "type": s(&r, "type"), "unpluggable": b(&r, "unpluggable"), "empty": false, "other_config": r.get("other_config").cloned().unwrap_or(json!({})),
                    "qos_algorithm_type": "", "qos_algorithm_params": {}, "device": "", "currently_attached": false,
                });
                let _ = self.call("VBD.create", vec![back], t);
                return Err(e);
            }
            if let Err(e) = self.call("VDI.destroy", vec![json!(vdi)], long) {
                note(format!("the old copy of a disk couldn't be deleted ({}): delete it from its storage", e.message()));
            }
        }
        self.call("VM.set_affinity", vec![vm, json!(m.host)], t).map(|_| ())
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

    fn set_ip(&self, pif: &str, mode: &str, ip: &str, netmask: &str, gateway: &str, dns: &str) -> Result<(), ApiError> {
        let t = 30_000;
        let old = self.call("PIF.get_IP", vec![json!(pif)], t)?.as_str().unwrap_or_default().to_string();
        let management = self.call("PIF.get_management", vec![json!(pif)], t)?.as_bool().unwrap_or(false);
        let moves = management && mode == "Static" && !ip.is_empty() && ip != old;
        if moves {
            // where to log in next: the new address first (the pool's other servers after it)
            let mut st = self.state.lock().unwrap();
            st.members.retain(|m| !m.eq_ignore_ascii_case(ip));
            st.members.insert(0, ip.to_string());
        }
        let r = self.call("PIF.reconfigure_ip", vec![json!(pif), json!(mode), json!(ip), json!(netmask), json!(gateway), json!(dns)], 90_000);
        if moves {
            // the connection was to the old address: the next call logs in at the new one
            let mut st = self.state.lock().unwrap();
            if st.host.eq_ignore_ascii_case(&old) {
                st.host = ip.to_string();
            }
            st.session = None;
        }
        match r {
            // its answer went down with the old address
            Err(e) if moves && e.code == "CONNECTION" => Ok(()),
            r => r.map(|_| ()),
        }
    }

    fn set_hostname(&self, host: &str, name: &str) -> Result<(), ApiError> {
        self.call("host.set_hostname_live", vec![json!(host), json!(name)], 60_000).map(|_| ())
    }

    fn records(&self, kind: &str, rref: &str) -> Result<Vec<(String, Value)>, ApiError> {
        let t = 30_000;
        let valid = |r: &str| !r.is_empty() && r != "OpaqueRef:NULL";
        // the item's own record, then its metrics' (which a VM that's off may not have)
        let (main, title, metrics): (&str, &str, &[(&str, &str, &str)]) = match kind {
            "vm" => ("VM", "VM", &[("metrics", "VM_metrics", "VM metrics"), ("guest_metrics", "VM_guest_metrics", "Guest metrics")]),
            "host" => ("host", "Server", &[("metrics", "host_metrics", "Server metrics")]),
            _ => return Ok(self.all("pool", t)?.into_iter().map(|(_, r)| ("Pool".to_string(), r)).collect()),
        };
        let r = self.call(&format!("{}.get_record", main), vec![json!(rref)], t)?;
        let mut out = vec![];
        for (field, cls, what) in metrics {
            let m = s(&r, field);
            if valid(&m) {
                if let Ok(x) = self.call(&format!("{}.get_record", cls), vec![json!(m)], t) {
                    out.push((what.to_string(), x));
                }
            }
        }
        out.insert(0, (title.to_string(), r));
        Ok(out)
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

    fn export_vm(&self, obj: &str, to: &Path, compress: bool, via_snapshot: bool, t: &Arc<Transfer>) -> Result<u64, ApiError> {
        if !via_snapshot {
            return self.export_one(obj, to, compress, t);
        }
        let name = self.call("VM.get_name_label", vec![json!(obj)], 30_000)?.as_str().unwrap_or_default().to_string();
        let snap = self.call("VM.snapshot", vec![json!(obj), json!(format!("{} (for an export)", name))], 1_800_000)?.as_str().unwrap_or_default().to_string();
        let result = self.export_one(&snap, to, compress, t);
        note("The snapshot was only for the export: deleting it.");
        match self.delete_snapshot(&snap) {
            Ok(left) if !left.is_empty() => note(format!("Not deleted: {}", left.join("; "))),
            Err(e) => note(format!("The snapshot couldn't be deleted ({}): delete it on the Snapshots tab.", e.message())),
            Ok(_) => {}
        }
        result
    }

    fn import_vm(&self, imp: &ImportVm, t: &Arc<Transfer>) -> Result<String, ApiError> {
        let file = &imp.file;
        let size = std::fs::metadata(file).map_err(|e| ApiError::other(format!("Couldn't read {}: {}", file.display(), e)))?.len();
        let f = std::fs::File::open(file).map_err(|e| ApiError::other(format!("Couldn't open {}: {}", file.display(), e)))?;
        t.total.store(size, Ordering::Relaxed);
        let label = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let task = self.call("task.create", vec![json!("Import"), json!(label)], 30_000)?.as_str().unwrap_or_default().to_string();
        let (base, session) = self.http_base(imp.address.as_deref());
        let url = format!("{}/import?session_id={}&task_id={}&sr_id={}&restore=false&force=false", base, session, task, imp.sr);
        if let Some(w) = watched() {
            w.trace.push(Said::Asked, format!("PUT /import  sr={}  file=\"{}\"  ({})", named(&imp.names, &imp.sr), label, fmt_bytes(size as i64)));
        }
        let agent = ureq::AgentBuilder::new()
            .tls_connector(self.tls.clone())
            .timeout_connect(Duration::from_secs(30))
            .timeout_write(Duration::from_secs(600))
            .timeout_read(Duration::from_secs(3600))
            .redirects(0)
            .user_agent(concat!("SimpleVMManager/", env!("CARGO_PKG_VERSION")))
            .build();
        let body = Counting { inner: Box::new(std::io::BufReader::with_capacity(1 << 20, f)), t: t.clone() };
        let sent = agent.put(&url).set("Content-Length", &size.to_string()).send(body);
        t.finishing.store(true, Ordering::Relaxed);
        let cancelled = t.cancel.load(Ordering::Relaxed);
        let put_error = match sent {
            Ok(_) => None,
            Err(ureq::Error::Status(code, _)) => Some(ApiError::other(format!("The server answered the import with HTTP {}.", code))),
            Err(e) => Some(ApiError::other(e.to_string())),
        };
        if cancelled || put_error.is_some() {
            let _ = self.call("task.cancel", vec![json!(task)], 30_000);
        }
        // the server writes the disks as they come; what's left is short
        let wait = if cancelled || put_error.is_some() { 60 } else { 1800 };
        let verdict = self.task_result(&task, Duration::from_secs(wait));
        let made = self.call_raw("task.get_result", vec![json!(task)], 30_000).ok().map(|v| v.as_str().unwrap_or_default().to_string()).unwrap_or_default();
        let _ = self.call("task.destroy", vec![json!(task)], 30_000);
        if cancelled {
            return Err(ApiError { code: CANCELLED.into(), details: vec![] });
        }
        match verdict {
            Ok(true) => {}
            Err(e) if e.code != CANCELLED => return Err(e),
            _ => return Err(put_error.unwrap_or_else(|| ApiError::other("The server didn't finish the import in time."))),
        }
        // the task's result: the VMs made, as XML ("<value><array>…OpaqueRef:…")
        let vms = task_refs(&made);
        let vm = vms.first().cloned().ok_or_else(|| ApiError::other("The server imported it but didn't say what it made."))?;
        if let Some(w) = watched() {
            w.trace.push(Said::Answer, format!("{}  (imported)", vm));
        }
        let t30 = 30_000;
        self.call("VM.set_name_label", vec![json!(vm), json!(imp.name)], t30)?;
        if let Some(h) = &imp.host {
            self.call("VM.set_affinity", vec![json!(vm), json!(h)], t30)?;
        }
        if let Some(net) = &imp.network {
            let vifs: Vec<String> = self.call("VM.get_VIFs", vec![json!(vm)], t30)?.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            for vif in vifs {
                if self.call("VIF.get_network", vec![json!(vif)], t30)?.as_str() != Some(net.as_str()) {
                    self.call("VIF.move", vec![json!(vif), json!(net)], 120_000)?;
                }
            }
        }
        Ok(vm)
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
    /// Its messages for people, the limits set on its VMs, servers and storage (by reference),
    /// and its email for alerts.
    alerts: Mutex<(Vec<XenMessage>, HashMap<String, String>, (String, String))>,
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
            hostname: name.into(),
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
            suspend_sr: None,
            crash_dump_sr: None,
            updates_applied: String::new(),
            pending_guidance: vec![],
            last_update: None,
            // till 31 July next year
            license_expiry: parse_xen_time(&format!("{}0731T00:00:00Z", 1971 + SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) / 31_556_952)),
            nics: vec![
                HostNic {
                    device: "eth0".into(), network: "Pool-wide network 0".into(), ip: addr.into(), management: true, carrier: true, speed: 1000,
                    mac: format!("aa:16:3e:00:{}:01", &r[r.len() - 1..]), netmask: "255.255.0.0".into(), pif: format!("{}-pif-eth0", r),
                    mode: "Static".into(), gateway: "10.20.0.1".into(), dns: "10.20.0.1, 1.1.1.1".into(),
                },
                HostNic {
                    device: "eth1".into(), network: "Storage network".into(), ip: addr.replace("10.20.", "10.30."), management: false, carrier: true, speed: 10000,
                    mac: format!("aa:16:3e:00:{}:02", &r[r.len() - 1..]), netmask: "255.255.0.0".into(), pif: format!("{}-pif-eth1", r),
                    mode: "Static".into(), gateway: String::new(), dns: String::new(),
                },
                HostNic {
                    device: "eth2".into(), network: "Pool-wide network 2".into(), ip: String::new(), management: false, carrier: false, speed: 0,
                    mac: format!("aa:16:3e:00:{}:03", &r[r.len() - 1..]), netmask: String::new(), pif: format!("{}-pif-eth2", r),
                    mode: "None".into(), gateway: String::new(), dns: String::new(),
                },
            ],
            power_on_mode: if master { String::new() } else { "wake-on-lan".into() },
            maintenance: false,
            maint_moved: vec![],
            maint_stopped: vec![],
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
            live_out: true,
            live_in: !std::env::var("SVM_DEMO_NO_MIRROR").map_or(false, |v| v.split(',').any(|x| x.trim() == r)),
            ..Default::default()
        };
        d.srs.push(SrInfo { device: "/dev/disk/by-id/ata-Samsung_SSD_870_EVO_1TB_S6PTNX0T123456A-part3".into(), ..sr("sr-local1", "Local storage", "lvm", 900 * GB, 0, Some("host-a")) });
        d.srs.push(sr("sr-dvd1", "DVD drives", "udev", 4_700_000_000, 4_700_000_000, Some("host-a"))); // a disc in it
        // (demo-host-02's installed with thin provisioning: EXT, not LVM)
        d.srs.push(SrInfo { device: "/dev/sda3".into(), ..sr("sr-local2", "Local storage", "ext", 900 * GB, 0, Some("host-b")) });
        d.srs.push(sr("sr-dvd2", "DVD drives", "udev", 0, 0, Some("host-b"))); // empty: not listed
        d.srs.push(sr("sr-usb2", "Removable storage", "udev", 0, 0, Some("host-b"))); // empty: not listed
        d.srs.push(SrInfo { location: "nas:/mnt/tank/vms".into(), share: "NFS (v3)".into(), ..sr("sr-nfs", "Shared NFS storage", "nfs", 4096 * GB, 0, None) });
        d.srs.push(SrInfo { location: "//nas/isos".into(), share: "Windows share (SMB 3.0)".into(), files: demo_isos("sr-iso"), ..sr("sr-iso", "ISO library", "iso", 250 * GB, 87 * GB, None) });
        // the shared storage is the pool's default (where new disks, suspended VMs' memory and
        // crash dumps go); SVM_DEMO_GONE_DEFAULT: it (and the rest) is storage that's gone
        (d.default_sr, d.suspend_sr, d.crash_dump_sr) = (Some("sr-nfs".into()), Some("sr-nfs".into()), Some("sr-nfs".into()));
        if std::env::var("SVM_DEMO_GONE_DEFAULT").is_ok() {
            (d.default_sr, d.suspend_sr, d.crash_dump_sr) = (Some("sr-deleted".into()), Some("sr-deleted".into()), Some("sr-deleted".into()));
        }
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
        d.networks.push(NetInfo { rref: "net-0".into(), uuid: demo_uuid(), name: "Pool-wide network 0".into(), description: String::new(), bridge: "xenbr0".into(), mtu: 1500, device: "eth0".into(), vlan: -1 });
        d.networks.push(NetInfo { rref: "net-1".into(), uuid: demo_uuid(), name: "Storage network".into(), description: String::new(), bridge: "xenbr1".into(), mtu: 9000, device: "eth1".into(), vlan: -1 });
        d.networks.push(NetInfo { rref: "net-2".into(), uuid: demo_uuid(), name: "Office VLAN".into(), description: "192.168.10.0/24".into(), bridge: "xapi0".into(), mtu: 1500, device: "eth0".into(), vlan: 10 });

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
            let thin = d.sr(srr).is_some_and(|x| x.is_thin());
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
                    // on thin storage about a third of it written; on LVM not known
                    VmDisk { name: format!("{} disk 0", name), device: "0".into(), vbd: format!("{}-vbd-0", r), vdi: format!("{}-vdi-0", r), sr: srr.to_string(), sr_name, kind: "Disk".into(), size: disk * GB, used: if thin { disk * GB * 7 / 20 } else { 0 }, ..Default::default() },
                    VmDisk { name: "(empty)".into(), device: "3".into(), vbd: format!("{}-vbd-3", r), kind: "DVD drive".into(), ..Default::default() },
                ],
                nics: vec![VmNic { device: "0".into(), vif: format!("{}-vif-0", r), net: "net-0".into(), mac: format!("aa:16:3e:{:02x}:10:{:02x}", i + 1, 0x20 + i), network: "Pool-wide network 0".into(), ip: ip.to_string() }],
                ..Default::default()
            });
            demo_details(d.vms.last_mut().unwrap());
        }
        d.schedules = demo_schedules();
        for v in d.vms.iter_mut() {
            v.schedule = d.schedules.iter().find(|x| x.vms.contains(&v.rref)).map(|x| x.rref.clone());
        }
        link_schedules(&mut d);
        // the servers named in SVM_DEMO_OFF were shut down (their VMs moved to the other server
        // first, or shut down when their disks are on it), and those in SVM_DEMO_MAINTENANCE are in
        // maintenance mode (their VMs moved off, to come back later)
        for (var, maint) in [("SVM_DEMO_OFF", false), ("SVM_DEMO_MAINTENANCE", true)] {
            let Ok(names) = std::env::var(var) else { continue };
            for i in 0..d.hosts.len() {
                let r = d.hosts[i].rref.clone();
                if !names.split(',').any(|x| x.trim() == r) {
                    continue;
                }
                let other = d.hosts.iter().find(|h| h.rref != r).map(|h| h.rref.clone());
                let (mut moved, mut stopped) = (vec![], vec![]);
                for v in d.vms.iter_mut().filter(|v| v.host.as_deref() == Some(r.as_str())) {
                    if v.storage_host.as_deref() == Some(r.as_str()) || other.is_none() {
                        (v.power, v.host) = (PowerState::Halted, None);
                        stopped.push(v.uuid.clone());
                    } else {
                        v.host = other.clone();
                        moved.push(v.uuid.clone());
                    }
                }
                let h = &mut d.hosts[i];
                h.enabled = false;
                if maint {
                    (h.maintenance, h.maint_moved, h.maint_stopped) = (true, moved, stopped);
                } else {
                    h.live = false;
                }
            }
        }
        recompute(&mut d);
        // two templates made from VMs like these, ready to copy
        // (each made a number of days ago)
        let made: [(&str, &str, &str, &str, i64, i64, i64, i64); 2] = [
            ("tpl-web", "Ubuntu 24.04 web server", "nginx and PHP 8.3, generalized", "Ubuntu 24.04 LTS", 2, 4, 40, 12),
            ("tpl-win", "Windows Server 2022 base", "Sysprepped, with the October updates", "Windows Server 2022", 4, 8, 80, 41),
        ];
        let today = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        for (r, name, description, os, cpus, mem, disk, days) in made {
            let mut t = VmInfo {
                created: Some(today - days * 86_400 - 5 * 3600),
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
            t.disks.push(VmDisk { name: format!("{} disk 0", name), device: "0".into(), vbd: format!("{}-vbd-0", r), vdi: format!("{}-vdi-0", r), sr: "sr-nfs".into(), sr_name: "Shared NFS storage".into(), kind: "Disk".into(), size: disk * GB, ..Default::default() });
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
        let alerts = demo_alerts(&d);
        DemoConnection { data: Mutex::new(d), updates: Mutex::new(found), alerts: Mutex::new(alerts) }
    }

    /// New VM from one of the user's templates: the calls a real server gets, with made-up answers.
    fn demo_from_template(&self, v: &NewVm, tr: &Trace) -> Result<String, ApiError> {
        // a template of the user's, or a VM's snapshot (kept as a template is)
        let tpl = {
            let d = self.data.lock().unwrap();
            d.user_template(&v.template).cloned().or_else(|| {
                d.vms.iter().find(|x| x.snapshot(&v.template).is_some()).map(|x| VmInfo { power: PowerState::Halted, host: None, snapshots: vec![], parent: None, schedule: None, ..x.clone() })
            })
        }
        .ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["VM".into(), v.template.clone()] })?;
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
        if v.cloud_init.is_some() {
            tr.push(Said::Note, "cloud-init: its disk, read on the first start");
            for asked in ["VDI.create  name=\"cloud-init config\"  size=4 MiB", "PUT /import_raw_vdi  vdi=cloud-init config  (4096 KB: user-data, meta-data)", "VBD.create  VDI=cloud-init config  userdevice=1"] {
                tr.push(Said::Asked, asked);
                std::thread::sleep(Duration::from_millis(150));
                tr.push(Said::Answer, "OK  (0.1 s)");
            }
        }
        let mut d = self.data.lock().unwrap();
        let mut vm = demo_copy(&tpl, &rref, &v.name, &v.description, v.copy_to.as_deref(), &d.srs);
        (vm.vcpus, vm.mem_static_max, vm.mem_dyn_min, vm.mem_dyn_max, vm.affinity) = (v.vcpus, v.memory, v.memory, v.memory, v.host.clone());
        if v.cloud_init.is_some() {
            let sr = v.copy_to.clone().unwrap_or_else(|| v.sr.clone());
            let sr_name = d.srs.iter().find(|x| x.rref == sr).map(|x| x.name.clone()).unwrap_or_default();
            let n = vm.disks.len();
            vm.disks.push(VmDisk { name: "cloud-init config".into(), device: n.to_string(), vbd: format!("{}-vbd-ci", rref), vdi: format!("{}-vdi-ci", rref), sr, sr_name, kind: "Disk".into(), size: crate::cloudinit::DRIVE_SIZE as i64, ..Default::default() });
        }
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
/// The demo pool's messages (a limit passed last night, a VM that crashed, a certificate
/// running out, a snapshot taken…) and the limits set that raised them.
fn demo_alerts(d: &PoolData) -> (Vec<XenMessage>, HashMap<String, String>, (String, String)) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0);
    let uuid_of = |r: &str| -> String {
        d.vms.iter().find(|v| v.rref == r).map(|v| v.uuid.clone()).or_else(|| d.hosts.iter().find(|h| h.rref == r).map(|h| h.uuid.clone())).or_else(|| d.srs.iter().find(|x| x.rref == r).map(|x| x.uuid.clone())).or_else(|| d.schedules.iter().find(|x| x.rref == r).map(|x| x.uuid.clone())).unwrap_or_default()
    };
    let cpu = "<variable>\n\t<name value=\"cpu_usage\"/>\n\t<alarm_trigger_level value=\"0.9\"/>\n\t<alarm_trigger_period value=\"300\"/>\n\t<alarm_auto_inhibit_period value=\"3600\"/>\n</variable>";
    let full = "<variable>\n\t<name value=\"physical_utilisation\"/>\n\t<alarm_trigger_level value=\"0.9\"/>\n\t<alarm_trigger_period value=\"300\"/>\n</variable>";
    let msg = |i: usize, name: &str, priority: i64, cls: &str, obj: &str, ago: i64, body: String| XenMessage {
        rref: format!("OpaqueRef:demo-message-{}", i),
        name: name.into(),
        priority,
        cls: cls.into(),
        obj_uuid: uuid_of(obj),
        time: now - ago,
        body,
    };
    let list = vec![
        msg(1, "ALARM", 3, "SR", "sr-local2", 40 * 60, format!("value: 0.962000\nconfig:\n{}", full)),
        msg(2, "ALARM", 3, "VM", "vm-web", 9 * 3600, format!("value: 0.973000\nconfig:\n{}", cpu)),
        msg(3, "VM_CRASHED", 2, "VM", "vm-mail", 26 * 3600, "The VM crashed and was restarted (actions_after_crash: restart)".into()),
        msg(4, "HOST_SERVER_CERTIFICATE_EXPIRING_30", 3, "Host", "host-a", 2 * 86_400 + 3 * 3600, "The TLS server certificate is expiring soon. Refresh it before it expires.".into()),
        msg(5, "VM_SNAPSHOTTED", 5, "VM", "vm-web", 2 * 86_400 + 7 * 3600, "Snapshot 'Before nginx 1.26' was taken".into()),
        msg(6, "POOL_MASTER_TRANSITION", 4, "Host", "host-a", 5 * 86_400, "This host is now the pool coordinator".into()),
        // the snapshot schedules' (as the server's vmss plugin writes them): last night's went
        // well; the database's last hour's didn't (its storage full), after it last ran well
        msg(7, "VMSS_SNAPSHOT_SUCCEEDED", 5, "VMSS", "vmss-nightly", 6 * 3600 + 19 * 60, "VMSS_SNAPSHOT_SUCCEEDED".into()),
        msg(8, "VMSS_SNAPSHOT_FAILED", 1, "VMSS", "vmss-db", 5 * 60, "Snapshot failed on 1 out of 1 VMs with the following errors: \n\nDetails:\nVM: Database UUID: 6f2c0d4e-demo Error:['SR_BACKEND_FAILURE_44', '', 'There is insufficient space']".into()),
    ];
    let mut limits = HashMap::new();
    limits.insert("vm-web".to_string(), format!("<config>{}</config>", cpu));
    limits.insert("sr-local2".to_string(), format!("<config>{}</config>", full));
    (list, limits, (String::new(), String::new()))
}

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

/// `ms` of work, its progress said every tenth of a second (five times as long with
/// SVM_DEMO_SLOW, for a check's screenshot of it under way).
/// The demo's snapshot schedules: the Web Server and the Mail Relay every night (a week of them
/// kept), the database every hour.
fn demo_schedules() -> Vec<ScheduleInfo> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0);
    let schedule = |rref: &str, name: &str, description: &str, every: Every, hour: u32, min: u32, keep: i64, vms: &[&str], ago: i64| ScheduleInfo {
        rref: rref.into(),
        uuid: format!("{}-uuid", rref),
        name: name.into(),
        description: description.into(),
        enabled: true,
        kind: "snapshot".into(),
        keep,
        every,
        hour,
        min,
        last_run: Some(now - ago),
        vms: vms.iter().map(|v| v.to_string()).collect(),
        ..Default::default()
    };
    vec![
        schedule("vmss-db", "Database hourly", "The shop's orders since the last one", Every::Hourly, 0, 30, 10, &["vm-db"], 20 * 60),
        schedule("vmss-nightly", "Nightly", "A week of nights kept", Every::Daily, 3, 0, 7, &["vm-web", "vm-mail"], 6 * 3600 + 20 * 60),
    ]
}

/// Each VM's schedule and each schedule's VMs agreeing (the schedules' lists as the VMs say),
/// the schedules by name.
fn link_schedules(d: &mut PoolData) {
    for x in d.schedules.iter_mut() {
        x.vms = d.vms.iter().filter(|v| v.schedule.as_deref() == Some(x.rref.as_str())).map(|v| v.rref.clone()).collect();
    }
    d.schedules.sort_by_key(|x| x.name.to_lowercase());
}

fn demo_work(ms: u64) {
    let ms = if std::env::var("SVM_DEMO_SLOW").is_ok() { ms * 5 } else { ms };
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
    v.created = SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs() as i64);
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
            scheduled: false,
        })
        .collect();
    // what its snapshot schedule took (see demo_schedules): the Web Server's and the Mail Relay's
    // every night, the database's every hour; named as the server's plugin names them
    let ago: &[i64] = match v.rref.as_str() {
        "vm-web" | "vm-mail" => &[6, 30],
        "vm-db" => &[0, 1, 2],
        _ => &[],
    };
    for (i, hours) in ago.iter().enumerate() {
        let t = now - hours * 3600 - 20 * 60;
        let name = snapshot_name(&plugin_snapshot_name(t), true, Some(t));
        v.snapshots.push(SnapshotInfo { rref: format!("{}-auto-{}", v.rref, i), name, time: Some(t), was_running: true, scheduled: true, ..Default::default() });
    }
    v.snapshots.sort_by_key(|x| std::cmp::Reverse(x.time));
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

/// A demo VM's, server's or pool's records as a server would send them: every field XenServer
/// 8.4 has, made from what the demo knows.
fn demo_records(d: &PoolData, kind: &str, rref: &str) -> Option<Vec<(String, Value)>> {
    const NULL: &str = "OpaqueRef:NULL";
    let xt = |t: Option<i64>| {
        let t = t.unwrap_or(0);
        let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
        // the civil date from days since 1970 (Howard Hinnant's algorithm)
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
        format!("{:04}{:02}{:02}T{:02}:{:02}:{:02}Z", year, month, day, secs / 3600, secs % 3600 / 60, secs % 60)
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0);
    let master = d.hosts.iter().find(|h| h.is_master);
    let opt = |r: &Option<String>| r.clone().unwrap_or_else(|| NULL.into());
    match kind {
        "vm" => {
            let v = d.vm(rref)?;
            let running = matches!(v.power, PowerState::Running | PowerState::Paused);
            let uefi = v.firmware.to_lowercase().contains("uefi");
            let order = if v.boot_order.to_lowercase().starts_with("dvd") { "dc" } else { "cd" };
            let vm = json!({
                "uuid": v.uuid, "name_label": v.name, "name_description": v.description, "power_state": match v.power { PowerState::Running => "Running", PowerState::Paused => "Paused", PowerState::Suspended => "Suspended", _ => "Halted" },
                "allowed_operations": v.allowed, "current_operations": {}, "user_version": "1",
                "is_a_template": false, "is_default_template": false, "is_a_snapshot": false, "is_control_domain": false,
                "suspend_VDI": NULL, "resident_on": opt(&v.host), "scheduled_to_be_resident_on": NULL, "affinity": opt(&v.affinity),
                "memory_overhead": "11534336", "memory_target": v.mem_dyn_max.to_string(), "memory_static_max": v.mem_static_max.to_string(),
                "memory_dynamic_max": v.mem_dyn_max.to_string(), "memory_dynamic_min": v.mem_dyn_min.to_string(), "memory_static_min": v.mem_static_min.to_string(),
                "VCPUs_params": {}, "VCPUs_max": v.vcpus_max.max(v.vcpus).to_string(), "VCPUs_at_startup": v.vcpus.to_string(),
                "actions_after_softreboot": "soft_reboot", "actions_after_shutdown": "destroy", "actions_after_reboot": "restart", "actions_after_crash": "restart",
                "consoles": if running { vec![format!("{}-console", v.rref)] } else { vec![] }, "VIFs": v.nics.iter().map(|n| n.vif.clone()).collect::<Vec<_>>(),
                "VBDs": v.disks.iter().map(|k| k.vbd.clone()).collect::<Vec<_>>(), "VUSBs": [], "crash_dumps": [], "VTPMs": [], "VGPUs": [], "attached_PCIs": [],
                "PV_bootloader": "", "PV_kernel": "", "PV_ramdisk": "", "PV_args": "", "PV_bootloader_args": "", "PV_legacy_args": "",
                "HVM_boot_policy": "BIOS order", "HVM_boot_params": { "order": order, "firmware": if uefi { "uefi" } else { "bios" } }, "HVM_shadow_multiplier": 1.0,
                "platform": { "timeoffset": "0", "device-model": "qemu-upstream-compat", "secureboot": if v.secure_boot.to_lowercase().starts_with("on") { "true" } else { "false" },
                              "viridian": "false", "acpi": "1", "apic": "true", "pae": "true", "nx": "true", "hpet": "true", "vga": "std", "videoram": "8", "cores-per-socket": v.vcpus.max(1).to_string() },
                "PCI_bus": "", "other_config": { "base_template_name": "Other install media", "folder": v.folder, "mac_seed": format!("{}-mac", v.uuid), "auto_poweron": v.auto_power_on.to_string(), "import_task": NULL },
                "domid": if running { "3" } else { "-1" }, "domarch": if running { "x64" } else { "" },
                "last_boot_CPU_flags": if running { json!({ "vendor": "GenuineIntel", "features": "1fcbfbff-f7fa3223-2c100800-00000121-0000000f-219c07ab-00000000-00000000-00001000-ac000400-00000000-00000000-00000000-00000000-00000000" }) } else { json!({}) },
                "metrics": format!("{}-metrics", v.rref), "guest_metrics": if v.tools_installed { format!("{}-guest", v.rref) } else { NULL.into() },
                "last_booted_record": "", "recommendations": "<restrictions><restriction field=\"memory-static-max\" max=\"1649267441664\"/><restriction field=\"vcpus-max\" max=\"32\"/></restrictions>",
                "xenstore_data": { "vm-data": "", "vm-data/mmio-hole-size": "268435456" },
                "ha_always_run": !v.ha.is_empty(), "ha_restart_priority": if v.ha.is_empty() { "" } else { "restart" },
                "snapshot_of": NULL, "snapshots": v.snapshots.iter().map(|x| x.rref.clone()).collect::<Vec<_>>(), "snapshot_time": xt(None), "transportable_snapshot_id": "",
                "blobs": {}, "tags": v.tags, "blocked_operations": {}, "snapshot_info": {}, "snapshot_metadata": "",
                "parent": opt(&v.parent), "children": [], "bios_strings": { "bios-vendor": "Xen", "bios-version": "", "system-manufacturer": "Xen", "system-product-name": "HVM domU", "system-version": "", "system-serial-number": "", "baseboard-manufacturer": "", "hp-rombios": "", "oem-1": "Xen", "oem-2": "MS_VM_CERT/SHA1/bdbeb6e0a816d43fa6d3fe8aaef04c2bad9d3e3d" },
                "protection_policy": NULL, "is_snapshot_from_vmpp": false, "snapshot_schedule": opt(&v.schedule), "is_vmss_snapshot": false,
                "appliance": NULL, "start_delay": "0", "shutdown_delay": "0", "order": "0", "suspend_SR": NULL, "version": "0",
                "generation_id": "0:0", "hardware_platform_version": "2", "has_vendor_device": false, "requires_reboot": false, "reference_label": "",
                "domain_type": if v.virt_mode.to_lowercase().contains("pv") && !v.virt_mode.to_lowercase().contains("hvm") { "pv" } else { "hvm" },
                "NVRAM": if uefi { json!({ "EFI-variables": "(1.4 KiB)" }) } else { json!({}) }, "pending_guidances": [], "pending_guidances_recommended": [], "pending_guidances_full": [],
            });
            let mut out = vec![("VM".to_string(), vm)];
            if running || v.created.is_some() {
                out.push((
                    "VM metrics".into(),
                    json!({
                        "uuid": format!("{}-metrics", v.uuid), "memory_actual": v.mem_actual.max(if running { v.mem_dyn_max } else { 0 }).to_string(),
                        "VCPUs_number": if running { v.vcpus } else { 0 }.to_string(), "VCPUs_utilisation": {}, "VCPUs_CPU": {}, "VCPUs_params": {}, "VCPUs_flags": {},
                        "state": [], "start_time": xt(v.started), "install_time": xt(v.created), "last_updated": xt(None), "other_config": {},
                        "hvm": true, "nested_virt": false, "nomigrate": false, "current_domain_type": if running { "hvm" } else { "unspecified" },
                    }),
                ));
            }
            if v.tools_installed {
                let mut nets = serde_json::Map::new();
                for (i, ip) in v.ips.iter().enumerate() {
                    nets.insert(format!("{}/ip", i), json!(ip));
                    nets.insert(format!("{}/ipv4/0", i), json!(ip));
                }
                let (major, minor) = v.tools.split_once('.').unwrap_or((v.tools.as_str(), "0"));
                out.push((
                    "Guest metrics".into(),
                    json!({
                        "uuid": format!("{}-guest", v.uuid), "os_version": { "name": v.os, "uname": v.kernel, "distro": v.os.split_whitespace().next().unwrap_or("").to_lowercase() },
                        "netbios_name": {}, "PV_drivers_version": { "major": major, "minor": minor, "micro": "0", "build": "0" }, "PV_drivers_up_to_date": true,
                        "memory": {}, "disks": {}, "networks": nets, "other": { "feature-balloon": "1", "has-vendor-device": "0", "platform-feature-multiprocessor-suspend": "1" },
                        "last_updated": xt(Some(now - 40)), "other_config": {}, "live": running, "can_use_hotplug_vbd": "yes", "can_use_hotplug_vif": "yes", "PV_drivers_detected": true,
                    }),
                ));
            }
            Some(out)
        }
        "host" => {
            let h = d.host(rref)?;
            let resident: Vec<String> = d.vms.iter().filter(|v| v.host.as_deref() == Some(h.rref.as_str())).map(|v| v.rref.clone()).collect();
            let host = json!({
                "uuid": h.uuid, "name_label": h.name, "name_description": h.description, "memory_overhead": "4362076160",
                "allowed_operations": ["vm_migrate", "provision", "vm_resume", "evacuate", "vm_start"], "current_operations": {},
                "API_version_major": "2", "API_version_minor": "21", "API_version_vendor": "XenSource", "API_version_vendor_implementation": {},
                "enabled": h.enabled, "software_version": { "product_version": h.version, "product_brand": or(&h.brand, "XenServer"), "platform_name": "XCP", "platform_version": "3.4.0",
                    "build_number": "24.38.0", "hostname": h.name, "date": "2026-10-08", "xapi": "24.38", "xapi_build": "24.38.0", "xen": "4.17.5-6", "linux": "4.19.0+1", "xencenter_min": "2.21", "xencenter_max": "2.21", "network_backend": "openvswitch", "db_schema": "5.788" },
                "other_config": { "iscsi_iqn": format!("iqn.2026-11.com.example:{}", &h.uuid[..8.min(h.uuid.len())]), "agent_start_time": format!("{}.", h.boot_time.unwrap_or(0) + 60), "boot_time": format!("{}.", h.boot_time.unwrap_or(0)), "MAINTENANCE_MODE": if h.maintenance { "true" } else { "false" } },
                "capabilities": ["xen-3.0-x86_64", "hvm-3.0-x86_32", "hvm-3.0-x86_32p", "hvm-3.0-x86_64"], "cpu_configuration": {}, "sched_policy": "credit",
                "supported_bootloaders": ["pygrub", "eliloader"], "resident_VMs": resident, "logging": {}, "PIFs": h.nics.iter().map(|n| format!("{}-pif-{}", h.rref, n.device)).collect::<Vec<_>>(),
                "suspend_image_sr": opt(&h.suspend_sr), "crash_dump_sr": opt(&h.crash_dump_sr), "crashdumps": [], "patches": [], "updates": [], "PBDs": [], "host_CPUs": [],
                "cpu_info": { "cpu_count": h.cpu_count.to_string(), "socket_count": "1", "threads_per_core": "2", "vendor": "GenuineIntel", "speed": "3600.000", "modelname": h.cpu_model, "family": "6", "model": "158", "stepping": "13",
                              "flags": "fpu de tsc msr pae mce cx8 apic sep mca cmov pat clflush acpi mmx fxsr sse sse2 ht syscall nx rdtscp lm constant_tsc rep_good nopl nonstop_tsc cpuid pni pclmulqdq monitor est ssse3 fma cx16 sse4_1 sse4_2 movbe popcnt aes xsave avx f16c rdrand hypervisor lahf_lm abm 3dnowprefetch fsgsbase bmi1 avx2 bmi2 erms" },
                "hostname": h.name, "address": h.address, "metrics": format!("{}-metrics", h.rref), "license_params": { "restrict_vmss": "false", "restrict_pool_size": "false", "sku_type": "free", "expiry": h.license_expiry.map(|t| xt(Some(t))).unwrap_or_default() },
                "ha_statefiles": [], "ha_network_peers": [], "blobs": {}, "tags": [], "external_auth_type": "", "external_auth_service_name": "", "external_auth_configuration": {},
                "edition": or(&h.edition, "free"), "license_server": { "address": "localhost", "port": "27000" }, "bios_strings": { "bios-vendor": "American Megatrends Inc.", "system-manufacturer": "Demo Computers", "system-product-name": "Demo Server 1U" },
                "power_on_mode": h.power_on_mode, "power_on_config": {}, "local_cache_sr": NULL, "chipset_info": { "iommu": "true" }, "PCIs": [], "PGPUs": [], "PUSBs": [],
                "ssl_legacy": false, "guest_VCPUs_params": {}, "display": "enabled", "virtual_hardware_platform_versions": ["0", "1", "2"], "control_domain": h.control_domain,
                "updates_requiring_reboot": [], "features": [], "iscsi_iqn": format!("iqn.2026-11.com.example:{}", &h.uuid[..8.min(h.uuid.len())]), "multipathing": false, "uefi_certificates": "",
                "certificates": [], "editions": [or(&h.edition, "free")], "pending_guidances": h.pending_guidance, "tls_verification_enabled": true, "last_software_update": xt(h.last_update),
                "https_only": false, "latest_synced_updates_applied": or(&h.updates_applied, "unknown"), "numa_affinity_policy": "default_policy", "ssh_enabled": true, "ssh_enabled_timeout": "0", "console_idle_timeout": "0",
            });
            let metrics = json!({ "uuid": format!("{}-metrics", h.uuid), "memory_total": h.mem_total.to_string(), "memory_free": h.mem_free.to_string(), "live": h.live, "last_updated": xt(Some(now - 5)), "other_config": {} });
            Some(vec![("Server".into(), host), ("Server metrics".into(), metrics)])
        }
        _ => {
            let pool = json!({
                "uuid": "8c1a4e2f-5b3d-4e6a-9f0c-2d7b1e8a6c4f", "name_label": d.name, "name_description": "", "master": master.map(|h| h.rref.clone()).unwrap_or_else(|| NULL.into()),
                "default_SR": opt(&d.default_sr), "suspend_image_SR": opt(&d.suspend_sr), "crash_dump_SR": opt(&d.crash_dump_sr),
                "other_config": { "auto_poweron": "true", "memory-ratio-hvm": "0.25", "memory-ratio-pv": "0.25" }, "ha_enabled": false, "ha_configuration": {}, "ha_statefiles": [],
                "ha_host_failures_to_tolerate": "0", "ha_plan_exists_for": "0", "ha_allow_overcommit": false, "ha_overcommitted": false, "blobs": {}, "tags": [], "gui_config": {},
                "health_check_config": {}, "wlb_url": "", "wlb_username": "", "wlb_enabled": false, "wlb_verify_cert": false, "redo_log_enabled": false, "redo_log_vdi": NULL,
                "vswitch_controller": "", "restrictions": { "restrict_vmss": if d.schedules_off.is_some() { "true" } else { "false" }, "restrict_pool_size": "false", "restrict_ha": "false", "restrict_rbac": "false" },
                "metadata_VDIs": [], "ha_cluster_stack": "xhad", "allowed_operations": ["ha_enable", "cluster_create", "designate_new_master"], "current_operations": {}, "guest_agent_config": {},
                "cpu_info": { "cpu_count": d.hosts.iter().map(|h| h.cpu_count).sum::<i64>().to_string(), "socket_count": d.hosts.len().to_string(), "vendor": "GenuineIntel" },
                "policy_no_vendor_device": false, "live_patching_disabled": false, "igmp_snooping_enabled": false, "uefi_certificates": "", "is_psr_pending": false, "tls_verification_enabled": true,
                "repositories": [], "client_certificate_auth_enabled": false, "client_certificate_auth_name": "", "repository_proxy_url": "", "repository_proxy_username": "", "repository_proxy_password": NULL,
                "migration_compression": false, "coordinator_bias": true, "telemetry_uuid": NULL, "telemetry_frequency": "weekly", "telemetry_next_collection": xt(Some(now + 5 * 86_400)),
                "last_update_sync": xt(d.last_update_sync), "update_sync_frequency": "weekly", "update_sync_day": "0", "update_sync_enabled": false, "recommendations": {}, "license_server": {}, "ha_reboot_vm_on_internal_shutdown": true,
            });
            Some(vec![("Pool".into(), pool)])
        }
    }
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
    /// Mounts it in about two seconds; an address with "unreachable" in it doesn't answer, and a
    /// folder with "denied" in it doesn't let the servers in.
    fn create_nfs_storage(&self, st: &NfsStorage) -> Result<String, ApiError> {
        said("SR.create", &[json!("host-a"), json!(st.device_config().into_iter().collect::<HashMap<_, _>>()), json!(0), json!(st.name), json!(st.description), json!("nfs"), json!("user"), json!(true), json!({})]);
        std::thread::sleep(Duration::from_millis(2000));
        let (server, path) = NfsStorage::parts(&st.location);
        let fail = |code: &str, text: String| {
            let e = ApiError { code: code.into(), details: vec![String::new(), text, String::new()] };
            answered(&Err(e.clone()), Duration::from_millis(2000));
            Err(e)
        };
        if server.contains("unreachable") {
            return fail("SR_BACKEND_FAILURE_108", "Unable to detect an NFS service on this target.".into());
        }
        if path.contains("denied") {
            return fail("SR_BACKEND_FAILURE_73", format!("NFS mount error [opterr=mount failed with return code 32] mount.nfs: access denied by server while mounting {}:{}", server, path));
        }
        let mut d = self.data.lock().unwrap();
        let rref = format!("sr-nfs-{}", d.srs.len());
        let links = d.hosts.iter().map(|h| (h.rref.clone(), true)).collect();
        d.srs.push(SrInfo {
            rref: rref.clone(),
            uuid: demo_uuid(),
            name: st.name.clone(),
            description: st.description.clone(),
            kind: "nfs".into(),
            size: 8192 * GB,
            used: 0,
            shared: true,
            host: None,
            location: format!("{}:{}", server, path),
            device: String::new(),
            share: format!("NFS (v{})", st.nfs_version),
            files: vec![],
            links,
            live_out: true,
            live_in: true,
        });
        answered(&Ok(json!(rref)), Duration::from_millis(2000));
        Ok(rref)
    }
    /// About a second; VLAN 666 is taken.
    fn create_network(&self, nw: &NewNetwork) -> Result<String, ApiError> {
        said("network.create", &[json!({ "name_label": nw.name, "MTU": nw.mtu })]);
        std::thread::sleep(Duration::from_millis(600));
        if let Some((device, tag)) = &nw.vlan {
            said("pool.create_VLAN_from_PIF", &[json!(format!("{}-pif-host-a", device)), json!("new"), json!(tag)]);
            std::thread::sleep(Duration::from_millis(600));
            if *tag == 666 {
                let e = ApiError { code: "PIF_VLAN_EXISTS".into(), details: vec![format!("{}-pif-host-a", device)] };
                answered(&Err(e.clone()), Duration::from_millis(600));
                return Err(e);
            }
        }
        let mut d = self.data.lock().unwrap();
        let rref = format!("net-{}", d.networks.len());
        let bridge = format!("xapi{}", d.networks.iter().filter(|n| n.bridge.starts_with("xapi")).count());
        let (device, vlan) = nw.vlan.clone().unwrap_or((String::new(), -1));
        d.networks.push(NetInfo { rref: rref.clone(), uuid: demo_uuid(), name: nw.name.clone(), description: nw.description.clone(), bridge, mtu: nw.mtu, device, vlan });
        Ok(rref)
    }
    /// About a second; one with a card on it is refused.
    fn delete_network(&self, net: &str) -> Result<(), ApiError> {
        said("network.destroy", &[json!(net)]);
        std::thread::sleep(Duration::from_millis(800));
        let mut d = self.data.lock().unwrap();
        if d.vms.iter().chain(&d.user_templates).any(|v| v.nics.iter().any(|n| n.net == net)) {
            return Err(ApiError { code: "NETWORK_CONTAINS_VIF".into(), details: vec![] });
        }
        d.networks.retain(|n| n.rref != net);
        Ok(())
    }
    fn nfs_exports(&self, server: &str) -> Result<Vec<String>, ApiError> {
        std::thread::sleep(Duration::from_millis(1200));
        if server.contains("unreachable") {
            return Err(ApiError { code: "SR_BACKEND_FAILURE_108".into(), details: vec![String::new(), "Unable to detect an NFS service on this target.".into(), String::new()] });
        }
        let xml = "<?xml version=\"1.0\" ?>\n<nfs-exports>\n\t<Export>\n\t\t<Target>10.20.0.5</Target>\n\t\t<Path>/mnt/tank/vms</Path>\n\t\t<Accesslist>10.20.0.0/24</Accesslist>\n\t</Export>\n\t<Export>\n\t\t<Target>10.20.0.5</Target>\n\t\t<Path>/mnt/tank/isos</Path>\n\t\t<Accesslist>*</Accesslist>\n\t</Export>\n\t<Export>\n\t\t<Target>10.20.0.5</Target>\n\t\t<Path>/mnt/tank/backups</Path>\n\t\t<Accesslist>10.20.0.0/24</Accesslist>\n\t</Export>\n</nfs-exports>";
        Ok(nfs_exports(xml))
    }
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
            device: String::new(),
            share: if lib.nfs { format!("NFS (v{})", lib.nfs_version) } else { format!("Windows share (SMB {})", lib.smb_version) },
            files,
            links,
            live_out: false,
            live_in: false,
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
    /// Pretends to save it in about four seconds (a running VM's snapshot first), the server's
    /// progress said as it goes; the file written is a real .xva of the VM's settings, without
    /// its disks, so the demo can import it again.
    fn export_vm(&self, obj: &str, to: &Path, compress: bool, via_snapshot: bool, t: &Arc<Transfer>) -> Result<u64, ApiError> {
        let info = {
            let d = self.data.lock().unwrap();
            let v = d.vm(obj).or_else(|| d.user_template(obj)).or_else(|| d.vms.iter().find(|v| v.snapshot(obj).is_some()));
            let v = v.ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["VM".into(), obj.into()] })?;
            if !via_snapshot && d.vm(obj).map_or(false, |v| v.power == PowerState::Running) {
                return Err(ApiError { code: "VM_BAD_POWER_STATE".into(), details: vec![obj.into(), "halted".into(), "running".into()] });
            }
            let snap = v.snapshot(obj).map(|x| x.name.clone());
            crate::xva::XvaInfo {
                name: snap.map(|n| format!("{} – {}", v.name, n)).unwrap_or_else(|| v.name.clone()),
                description: v.description.clone(),
                template: d.user_template(obj).is_some() || v.snapshot(obj).is_some(),
                os: v.os.clone(),
                vcpus: v.vcpus,
                memory: v.mem_dyn_max,
                disks: v.disks.iter().filter(|k| k.kind == "Disk").map(|k| k.size).collect(),
                used: v.disks.iter().filter(|k| k.kind == "Disk").map(|k| k.size / 3).sum(),
                nets: v.nics.iter().map(|n| n.network.clone()).collect(),
                ..Default::default()
            }
        };
        if via_snapshot {
            said("VM.snapshot", &[json!(obj), json!(format!("{} (for an export)", info.name))]);
            demo_work(900);
        }
        said("GET /export", &[json!(obj), json!(if compress { "zstd" } else { "false" })]);
        let bytes = if compress { info.used / 2 } else { info.used } as u64;
        let slow = if cfg!(test) { 0 } else if std::env::var("SVM_DEMO_SLOW").is_ok() { 40 } else { 4 };
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(slow) {
            if t.cancel.load(Ordering::Relaxed) {
                return Err(ApiError { code: CANCELLED.into(), details: vec![] });
            }
            let part = start.elapsed().as_secs_f64() / slow as f64;
            t.set_part(part);
            t.sent.store((bytes as f64 * part) as u64, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(50));
        }
        if t.cancel.load(Ordering::Relaxed) {
            return Err(ApiError { code: CANCELLED.into(), details: vec![] });
        }
        crate::xva::write_xva(to, &info).map_err(|e| ApiError::other(format!("Couldn't write {}: {}", to.display(), e)))?;
        t.sent.store(bytes, Ordering::Relaxed);
        t.set_part(1.0);
        if via_snapshot {
            note("The snapshot was only for the export: deleting it.");
            said("VM.destroy", &[json!("the snapshot")]);
        }
        Ok(bytes)
    }
    /// Reads what the file holds, pretends to send it in about four seconds, then adds it to the
    /// demo pool (a template when it holds one) as asked.
    fn import_vm(&self, imp: &ImportVm, t: &Arc<Transfer>) -> Result<String, ApiError> {
        let info = crate::xva::read_header(&imp.file).map_err(ApiError::other)?;
        t.total.store(info.size, Ordering::Relaxed);
        said("PUT /import", &[json!(imp.sr), json!(imp.file.display().to_string())]);
        let slow = if cfg!(test) { 0 } else if std::env::var("SVM_DEMO_SLOW").is_ok() { 40 } else { 4 };
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(slow) {
            if t.cancel.load(Ordering::Relaxed) {
                return Err(ApiError { code: CANCELLED.into(), details: vec![] });
            }
            t.sent.store((info.size as f64 * start.elapsed().as_secs_f64() / slow as f64) as u64, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(50));
        }
        if t.cancel.load(Ordering::Relaxed) {
            return Err(ApiError { code: CANCELLED.into(), details: vec![] });
        }
        t.sent.store(info.size, Ordering::Relaxed);
        t.finishing.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(500));
        let mut d = self.data.lock().unwrap();
        let base = d.vms.first().cloned().unwrap_or_default();
        let rref = format!("vm-import-{}", d.vms.len() + d.user_templates.len());
        let mut v = demo_copy(&base, &rref, &imp.name, &info.description, None, &d.srs);
        let x = d.sr(&imp.sr).cloned().unwrap_or_default();
        (v.os, v.vcpus, v.vcpus_max) = (info.os.clone(), info.vcpus.max(1), info.vcpus.max(1));
        (v.mem_static_max, v.mem_dyn_min, v.mem_dyn_max, v.mem_static_min) = (info.memory, info.memory, info.memory, info.memory.min(GB));
        let cds: Vec<VmDisk> = v.disks.iter().filter(|k| k.kind != "Disk").cloned().collect();
        v.disks = info.disks.iter().enumerate().map(|(i, &size)| VmDisk { name: format!("{} {}", imp.name, i), device: i.to_string(), vdi: format!("{}-vdi-{}", rref, i), vbd: format!("{}-vbd-{}", rref, i), sr: x.rref.clone(), sr_name: x.name.clone(), kind: "Disk".into(), size, ..Default::default() }).collect();
        v.disks.extend(cds);
        v.storage_host = x.host.clone();
        v.affinity = imp.host.clone();
        let nets: Vec<(String, String)> = d.networks.iter().map(|n| (n.rref.clone(), n.name.clone())).collect();
        v.nics = info
            .nets
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let (net, network) = match &imp.network {
                    Some(r) => nets.iter().find(|n| n.0 == *r).cloned().unwrap_or_default(),
                    None => nets.iter().find(|n| n.1 == *name).or(nets.first()).cloned().unwrap_or_default(),
                };
                VmNic { device: i.to_string(), vif: format!("{}-vif-{}", rref, i), net, network, mac: format!("aa:16:3e:9{}:{:02x}:{:02x}", i, d.vms.len(), i), ip: String::new() }
            })
            .collect();
        if imp.template {
            v.allowed.clear();
            d.user_templates.push(v);
        } else {
            v.allowed = vec!["start".into()];
            d.vms.push(v);
        }
        recompute(&mut d);
        Ok(rref)
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
        vm.disks.push(VmDisk { device: "0".into(), name: format!("{} disk", v.name), vbd: format!("{}-vbd-0", rref), vdi: format!("{}-vdi-0", rref), sr: v.sr.clone(), sr_name, kind: "Disk".into(), size: v.disk, ..Default::default() });
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
        v.snapshots.insert(0, SnapshotInfo { rref: rref.clone(), name: name.into(), description: description.into(), time, memory, was_running, scheduled: false });
        v.parent = Some(rref.clone());
        Ok(rref)
    }
    /// Half a second, as a real server would take it.
    fn save_schedule(&self, x: &ScheduleInfo) -> Result<String, ApiError> {
        said(if x.rref.is_empty() { "VMSS.create" } else { "VMSS.set_schedule" }, &[json!(x.name), json!(x.when())]);
        demo_work(500);
        let mut d = self.data.lock().unwrap();
        let rref = if x.rref.is_empty() { format!("vmss-{}", demo_uuid()) } else { x.rref.clone() };
        let mut y = ScheduleInfo { rref: rref.clone(), kind: x.kind_to_save(), ..x.clone() };
        match d.schedules.iter_mut().find(|s| s.rref == rref) {
            Some(old) => {
                y.last_run = old.last_run;
                *old = y;
            }
            None => d.schedules.push(y),
        }
        for v in d.vms.iter_mut() {
            if x.vms.contains(&v.rref) {
                v.schedule = Some(rref.clone());
            } else if v.schedule.as_deref() == Some(rref.as_str()) {
                v.schedule = None;
            }
        }
        link_schedules(&mut d);
        Ok(rref)
    }
    fn delete_schedule(&self, vmss: &str) -> Result<(), ApiError> {
        said("VMSS.destroy", &[json!(vmss)]);
        demo_work(400);
        let mut d = self.data.lock().unwrap();
        d.schedules.retain(|x| x.rref != vmss);
        for v in d.vms.iter_mut().filter(|v| v.schedule.as_deref() == Some(vmss)) {
            v.schedule = None;
        }
        Ok(())
    }
    fn set_vm_schedule(&self, vms: &[String], vmss: Option<&str>) -> Result<(), ApiError> {
        demo_work(300);
        let mut d = self.data.lock().unwrap();
        for vm in vms {
            said("VM.set_snapshot_schedule", &[json!(vm), json!(vmss.unwrap_or("OpaqueRef:NULL"))]);
            if let Some(v) = d.vms.iter_mut().find(|v| v.rref == *vm) {
                v.schedule = vmss.map(String::from);
            }
        }
        link_schedules(&mut d);
        Ok(())
    }
    /// A snapshot of each of its VMs, a second each, named as the server's plugin names them;
    /// past what it keeps, its oldest go.
    fn run_schedule(&self, vmss: &str) -> Result<String, ApiError> {
        said("VMSS.snapshot_now", &[json!(vmss)]);
        let Some(x) = self.data.lock().unwrap().schedules.iter().find(|x| x.rref == vmss).cloned() else { return Err(ApiError::other("No such schedule")) };
        for vm in &x.vms {
            demo_work(1000);
            let mut d = self.data.lock().unwrap();
            let Some(v) = d.vms.iter_mut().find(|v| v.rref == *vm) else { continue };
            let time = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).ok();
            // (as the plugin names them, shown as the app shows those)
            let name = snapshot_name(&plugin_snapshot_name(time.unwrap_or(0)), true, time);
            let rref = format!("{}-snap-{}", v.rref, demo_uuid());
            let was_running = v.power == PowerState::Running;
            v.snapshots.insert(0, SnapshotInfo { rref, name, description: String::new(), time, memory: x.memory, was_running, scheduled: true });
            while v.snapshots.iter().filter(|s| s.scheduled).count() > x.keep as usize {
                if let Some(i) = v.snapshots.iter().rposition(|s| s.scheduled) {
                    v.snapshots.remove(i);
                }
            }
        }
        let mut d = self.data.lock().unwrap();
        if let Some(y) = d.schedules.iter_mut().find(|y| y.rref == vmss) {
            y.last_run = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).ok();
        }
        Ok("True".into())
    }
    fn set_schedule_enabled(&self, vmss: &str, on: bool) -> Result<(), ApiError> {
        said("VMSS.set_enabled", &[json!(vmss), json!(on)]);
        demo_work(200);
        let mut d = self.data.lock().unwrap();
        if let Some(x) = d.schedules.iter_mut().find(|x| x.rref == vmss) {
            x.enabled = on;
        }
        Ok(())
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
                v.disks.push(VmDisk { name: name.clone(), device: device.clone(), vbd: format!("{}-vbd-{}", vm, device), vdi: format!("{}-vdi-{}", vm, device), sr: sr.clone(), sr_name, kind: "Disk".into(), size: *size, ..Default::default() });
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
    /// Made-up records over `span`: a VM's daily round with a backup at night, kept since it
    /// was made; a server's likewise, with its VMs' CPUs.
    fn history(&self, host_address: &str, vm_uuid: Option<&str>, span: Span) -> Result<Records, ApiError> {
        std::thread::sleep(Duration::from_millis(200));
        let d = self.data.lock().unwrap();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0);
        match vm_uuid {
            Some(u) => {
                let v = d.vms.iter().find(|v| v.uuid == u).ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["VM".into(), u.into()] })?;
                Ok(demo_vm_records(v, span, now, &local_offset))
            }
            None => {
                // (the demo has one server per address; its first one when none is given)
                let h = d.hosts.iter().find(|h| h.address == host_address).or(d.hosts.first()).ok_or_else(|| ApiError::other("no server"))?;
                Ok(demo_host_records(h, &d, span, now, &local_offset))
            }
        }
    }
    fn messages(&self, since: Option<i64>) -> Result<Vec<XenMessage>, ApiError> {
        std::thread::sleep(Duration::from_millis(120));
        let a = self.alerts.lock().unwrap();
        Ok(a.0.iter().filter(|m| since.map_or(true, |t| m.time > t)).cloned().collect())
    }
    fn dismiss_messages(&self, refs: &[String]) -> Result<(), ApiError> {
        std::thread::sleep(Duration::from_millis(300));
        self.alerts.lock().unwrap().0.retain(|m| !refs.contains(&m.rref));
        Ok(())
    }
    fn alert_settings(&self, _cls: &str, obj: &str) -> Result<AlertSettings, ApiError> {
        std::thread::sleep(Duration::from_millis(150));
        let a = self.alerts.lock().unwrap();
        Ok(AlertSettings { perfmon: a.1.get(obj).cloned().unwrap_or_default(), mail_to: a.2 .0.clone(), mail_via: a.2 .1.clone() })
    }
    fn set_alert_settings(&self, _cls: &str, obj: &str, perfmon: &str, mail: Option<(String, String)>) -> Result<(), ApiError> {
        std::thread::sleep(Duration::from_millis(500));
        let mut a = self.alerts.lock().unwrap();
        if perfmon.is_empty() {
            a.1.remove(obj);
        } else {
            a.1.insert(obj.to_string(), perfmon.to_string());
        }
        if let Some(m) = mail {
            a.2 = m;
        }
        Ok(())
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
    /// The signal sent; the server answers a few seconds later.
    fn power_on_host(&self, host: &str) -> Result<(), ApiError> {
        if self.data.lock().unwrap().host(host).map_or(false, |h| h.power_on_mode.is_empty()) {
            note("no way to turn it on was set: set to Wake-on-LAN");
            said("host.set_power_on_mode", &[json!(host), json!("wake-on-lan"), json!({})]);
        }
        said("host.power_on", &[json!(host)]);
        std::thread::sleep(Duration::from_millis(4000));
        let mut d = self.data.lock().unwrap();
        if let Some(h) = d.hosts.iter_mut().find(|h| h.rref == host) {
            (h.live, h.enabled, h.power_on_mode) = (true, true, "wake-on-lan".into());
            h.boot_time = Some(SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0));
        }
        recompute(&mut d);
        Ok(())
    }
    fn enter_maintenance(&self, host: &str, stop: bool) -> Result<(Vec<String>, usize), ApiError> {
        let before: Vec<String> = self.data.lock().unwrap().vms.iter().filter(|v| v.host.as_deref() == Some(host) && matches!(v.power, PowerState::Running | PowerState::Paused)).map(|v| v.rref.clone()).collect();
        let (stopped, moved) = self.empty_host(host, stop)?;
        said("host.add_to_other_config", &[json!(host), json!(MAINT), json!("true")]);
        let mut d = self.data.lock().unwrap();
        let uuid = |d: &PoolData, r: &String| d.vm(r).map(|v| v.uuid.clone());
        let moved_ids: Vec<String> = before.iter().filter(|r| !stopped.contains(r)).filter_map(|r| uuid(&d, r)).collect();
        let stopped_ids: Vec<String> = stopped.iter().filter_map(|r| uuid(&d, r)).collect();
        if let Some(h) = d.hosts.iter_mut().find(|h| h.rref == host) {
            (h.maintenance, h.maint_moved, h.maint_stopped) = (true, moved_ids, stopped_ids);
        }
        Ok((stopped, moved))
    }
    fn exit_maintenance(&self, host: &str, back: bool) -> Result<Vec<String>, ApiError> {
        said("host.enable", &[json!(host)]);
        std::thread::sleep(Duration::from_millis(800));
        let mut d = self.data.lock().unwrap();
        let Some(h) = d.hosts.iter_mut().find(|h| h.rref == host) else { return Ok(vec![]) };
        h.enabled = true;
        h.maintenance = false;
        let (moved, stopped) = (std::mem::take(&mut h.maint_moved), std::mem::take(&mut h.maint_stopped));
        if back {
            for v in d.vms.iter_mut() {
                if moved.contains(&v.uuid) && v.power == PowerState::Running {
                    said("VM.pool_migrate", &[json!(v.rref), json!(host), json!({"live": "true"})]);
                    v.host = Some(host.to_string());
                } else if stopped.contains(&v.uuid) && v.power == PowerState::Halted {
                    said("VM.start_on", &[json!(v.rref), json!(host), json!(false), json!(false)]);
                    (v.power, v.host) = (PowerState::Running, Some(host.to_string()));
                }
            }
        }
        recompute(&mut d);
        Ok(vec![])
    }
    fn make_default_sr(&self, sr: &str) -> Result<(), ApiError> {
        said("pool.set_default_SR", &[json!("OpaqueRef:demo-pool"), json!(sr)]);
        std::thread::sleep(Duration::from_millis(400));
        let mut d = self.data.lock().unwrap();
        (d.default_sr, d.suspend_sr, d.crash_dump_sr) = (Some(sr.into()), Some(sr.into()), Some(sr.into()));
        Ok(())
    }
    fn vm_disks(&self, vm: &str) -> Result<(bool, Vec<(String, String, String)>), ApiError> {
        let d = self.data.lock().unwrap();
        let v = d.vm(vm).ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["VM".into(), vm.into()] })?;
        Ok((v.power == PowerState::Running, v.disks.iter().filter(|k| k.kind == "Disk").map(|k| (k.device.clone(), k.vdi.clone(), k.sr.clone())).collect()))
    }
    /// A couple of seconds, more for disks; then it's there.
    fn move_vm(&self, m: &MoveVm) -> Result<(), ApiError> {
        said(if m.running { if m.disks.is_empty() { "VM.pool_migrate" } else { "VM.migrate_send" } } else { "VDI.copy" }, &[json!(m.vm), json!(m.host)]);
        // the disks that go elsewhere, all at once
        let total = self.data.lock().unwrap().vm(&m.vm).map_or(0, |v| v.disks.iter().filter(|k| m.disks.iter().any(|d| d.0 == k.vdi)).map(|k| k.size).sum());
        if total > 0 {
            copying(0, total, total);
        }
        demo_work(1500 + 1500 * m.disks.len() as u64);
        let mut d = self.data.lock().unwrap();
        let srs: Vec<(String, String, Option<String>, bool)> = d.srs.iter().map(|x| (x.rref.clone(), x.name.clone(), x.host.clone(), x.is_thin())).collect();
        let v = d.vms.iter_mut().find(|v| v.rref == m.vm).ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["VM".into(), m.vm.clone()] })?;
        for k in v.disks.iter_mut() {
            if let Some((_, sr)) = m.disks.iter().find(|(vdi, _)| *vdi == k.vdi) {
                if let Some((r, name, _, thin)) = srs.iter().find(|x| x.0 == *sr) {
                    (k.sr, k.sr_name) = (r.clone(), name.clone());
                    // what's written is known on thin storage (a third or so, from LVM); not on LVM
                    k.used = match (*thin, k.used) {
                        (false, _) => 0,
                        (true, 0) => k.size * 7 / 20,
                        (true, u) => u,
                    };
                }
            }
        }
        if m.eject.is_some() {
            if let Some(k) = v.disks.iter_mut().find(|k| k.kind == "DVD drive") {
                (k.vdi, k.sr, k.sr_name, k.name) = (String::new(), String::new(), String::new(), "(empty)".into());
            }
        }
        v.storage_host = v.disks.iter().filter(|k| k.kind == "Disk").find_map(|k| srs.iter().find(|x| x.0 == k.sr).and_then(|x| x.2.clone()));
        if m.running {
            v.host = Some(m.host.clone());
        } else {
            v.affinity = Some(m.host.clone());
        }
        recompute(&mut d);
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
        if sr.is_some() {
            // its disks go to the other storage
            let total = self.data.lock().unwrap().vm(vm).map_or(0, |v| v.disks.iter().filter(|k| k.kind == "Disk").map(|k| k.size).sum());
            copying(0, total, total);
        }
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
    fn set_ip(&self, pif: &str, mode: &str, ip: &str, netmask: &str, gateway: &str, dns: &str) -> Result<(), ApiError> {
        said("PIF.reconfigure_ip", &[json!(pif), json!(mode), json!(ip), json!(netmask), json!(gateway), json!(dns)]);
        std::thread::sleep(Duration::from_millis(1200));
        let mut d = self.data.lock().unwrap();
        let h = d.hosts.iter_mut().find(|h| h.nics.iter().any(|n| n.pif == pif)).ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["PIF".into(), pif.into()] })?;
        let n = h.nics.iter_mut().find(|n| n.pif == pif).unwrap();
        n.mode = mode.into();
        if mode == "Static" {
            (n.ip, n.netmask, n.gateway, n.dns) = (ip.into(), netmask.into(), gateway.into(), dns.into());
        } else if mode == "None" {
            (n.ip, n.netmask, n.gateway, n.dns) = Default::default();
        }
        if n.management && !n.ip.is_empty() {
            h.address = n.ip.clone();
        }
        Ok(())
    }
    fn set_hostname(&self, host: &str, name: &str) -> Result<(), ApiError> {
        said("host.set_hostname_live", &[json!(host), json!(name)]);
        std::thread::sleep(Duration::from_millis(400));
        let mut d = self.data.lock().unwrap();
        let h = d.hosts.iter_mut().find(|h| h.rref == host).ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["host".into(), host.into()] })?;
        h.hostname = name.into();
        Ok(())
    }
    fn records(&self, kind: &str, rref: &str) -> Result<Vec<(String, Value)>, ApiError> {
        said(&format!("{}.get_record", kind), &[json!(rref)]);
        std::thread::sleep(Duration::from_millis(120));
        let d = self.data.lock().unwrap();
        demo_records(&d, kind, rref).ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec![kind.into(), rref.into()] })
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
    fn schedule_snapshot_names_read_as_dates() {
        // the vmss plugin's "20261120-1843" (UTC) as the app names snapshots, in local time
        let t = parse_xen_time("20261120T18:43:07Z").unwrap();
        assert_eq!(plugin_snapshot_name(t), "20261120-1843");
        assert_eq!(snapshot_name("20261120-1843", true, Some(t)), fmt_date_time(t, local_offset(t)));
        // a name the user gave it, or one not a schedule's, as it is
        assert_eq!(snapshot_name("Before update", true, Some(t)), "Before update");
        assert_eq!(snapshot_name("20261120-1843", false, Some(t)), "20261120-1843");
    }

    #[test]
    fn demo_makes_nfs_storage() {
        let c = DemoConnection::new();
        assert_eq!(c.nfs_exports("nas").unwrap(), vec!["/mnt/tank/backups".to_string(), "/mnt/tank/isos".into(), "/mnt/tank/vms".into()]);
        assert!(c.nfs_exports("unreachable").is_err());
        let st = NfsStorage { name: "VMs".into(), location: "nas:/mnt/tank/vms".into(), nfs_version: "4.1".into(), ..Default::default() };
        let sr = c.create_nfs_storage(&st).unwrap();
        let d = c.data.lock().unwrap();
        let x = d.sr(&sr).unwrap();
        assert!(x.shared && x.links.len() == d.hosts.len() && x.links.iter().all(|l| l.1), "mounted on every server");
        assert_eq!((x.kind.as_str(), x.location.as_str(), x.share.as_str()), ("nfs", "nas:/mnt/tank/vms", "NFS (v4.1)"));
        drop(d);
        let e = c.create_nfs_storage(&NfsStorage { location: "nas:/mnt/denied".into(), ..st }).unwrap_err();
        assert_eq!(crate::isolib::mount_trouble(&e), Some(crate::isolib::MountTrouble::Refused));
    }

    #[test]
    fn demo_moves_vms() {
        let c = DemoConnection::new();
        let get = |c: &DemoConnection, r: &str| c.data.lock().unwrap().vm(r).cloned().unwrap();
        // running, live to the other server, its disk going along
        let mail = get(&c, "vm-mail");
        let vdi = mail.disks.iter().find(|k| k.kind == "Disk").unwrap().vdi.clone();
        c.move_vm(&MoveVm { vm: "vm-mail".into(), host: "host-a".into(), disks: vec![(vdi, "sr-local1".into())], eject: None, running: true }).unwrap();
        let mail = get(&c, "vm-mail");
        assert_eq!((mail.host.as_deref(), mail.storage_host.as_deref(), mail.power), (Some("host-a"), Some("host-a"), PowerState::Running));
        // off: its home is the server now
        c.move_vm(&MoveVm { vm: "vm-backup".into(), host: "host-b".into(), disks: vec![], eject: None, running: false }).unwrap();
        assert_eq!(get(&c, "vm-backup").home(), Some("host-b"));
    }


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
    fn sessions_never_noted() {
        let dest = json!({"SM": "http://192.168.1.2/services/SM?session_id=OpaqueRef:7e54-aacb", "master": "http://192.168.1.2", "session_id": "OpaqueRef:7e54-aacb", "host": "OpaqueRef:6b2d"});
        let text = args_text(&[json!("OpaqueRef:21ac"), dest]);
        assert!(!text.contains("7e54"), "{}", text);
        assert!(text.contains("OpaqueRef:6b2d") && text.contains("192.168.1.2"));
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
        let moved = c.create_vm(&NewVm { name: "Web 3".into(), keep_nics: false, network: Some("net-1".into()), ..spec.clone() }, &Trace::default()).unwrap();
        let d = c.fetch(0).unwrap();
        assert_eq!(d.vm(&kept).unwrap().nics[0].network, "Pool-wide network 0");
        assert_eq!(d.vm(&moved).unwrap().nics[0].network, "Storage network");
        assert_eq!(d.vm(&kept).unwrap().disks[0].size, 40 * GB);
        // a VM from a VM's snapshot: a VM of its own, off, with no snapshots
        let snap = d.vm("vm-web").unwrap().snapshots.iter().find(|x| !x.memory).unwrap().rref.clone();
        let from_snap = c.create_vm(&NewVm { name: "Web 4".into(), template: snap, ..spec.clone() }, &Trace::default()).unwrap();
        let d = c.fetch(0).unwrap();
        let v = d.vm(&from_snap).unwrap();
        assert!(v.power == PowerState::Halted && v.snapshots.is_empty() && v.name == "Web 4" && v.disks[0].size == 40 * GB);
        // with cloud-init: its small disk attached too
        let ci = crate::cloudinit::CloudInit { hostname: "web-5".into(), user: "joao".into(), password: "x".into(), ..Default::default() };
        let with_ci = c.create_vm(&NewVm { name: "Web 5".into(), cloud_init: Some(ci), ..spec.clone() }, &Trace::default()).unwrap();
        let d = c.fetch(0).unwrap();
        let disks = &d.vm(&with_ci).unwrap().disks;
        assert!(disks.iter().any(|k| k.name == "cloud-init config" && k.size == 4 << 20));
        // deleted
        c.delete_vm("tpl-win", true).unwrap();
        assert!(c.fetch(0).unwrap().user_template("tpl-win").is_none());
    }

    #[test]
    fn snapshot_schedules() {
        let c = DemoConnection::new();
        let d = c.fetch(0).unwrap();
        let nightly = d.schedules.iter().find(|x| x.rref == "vmss-nightly").unwrap().clone();
        assert_eq!((nightly.summary().as_str(), nightly.vms.len()), ("Daily at 03:00  ·  keeps 7", 2));
        assert_eq!(d.vm("vm-web").unwrap().schedule.as_deref(), Some("vmss-nightly"));
        // a new one: its VMs moved onto it (the database off its hourly one)
        let x = ScheduleInfo { name: "Weekend".into(), enabled: true, keep: 4, every: Every::Weekly, hour: 22, days: vec!["Saturday".into()], vms: vec!["vm-db".into(), "vm-desk".into()], ..Default::default() };
        let made = c.save_schedule(&x).unwrap();
        let d = c.fetch(0).unwrap();
        let y = d.schedules.iter().find(|x| x.rref == made).unwrap();
        assert_eq!((y.when().as_str(), y.vms.len()), ("Sat at 22:00", 2));
        assert!(d.schedules.iter().find(|x| x.rref == "vmss-db").unwrap().vms.is_empty());
        // changed: one VM taken off; paused; then deleted, its VM on none
        // run now: a snapshot of each of its VMs, marked as the schedule's, and when it ran
        let before = c.fetch(0).unwrap().vm("vm-web").unwrap().snapshots.len();
        assert_eq!(c.run_schedule("vmss-nightly").unwrap(), "True");
        let d = c.fetch(0).unwrap();
        let web = d.vm("vm-web").unwrap();
        assert!(web.snapshots[0].scheduled && web.snapshots[0].name.contains(", "));
        assert!(web.snapshots.len() <= before + 1 && web.snapshots.iter().filter(|s| s.scheduled).count() <= 7);
        assert!(d.schedules.iter().find(|x| x.rref == "vmss-nightly").unwrap().last_run.unwrap() > SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64 - 60);
        c.save_schedule(&ScheduleInfo { rref: made.clone(), vms: vec!["vm-db".into()], ..x.clone() }).unwrap();
        assert_eq!(c.fetch(0).unwrap().vm("vm-desk").unwrap().schedule, None);
        c.set_schedule_enabled(&made, false).unwrap();
        assert!(!c.fetch(0).unwrap().schedules.iter().find(|x| x.rref == made).unwrap().enabled);
        c.delete_schedule(&made).unwrap();
        let d = c.fetch(0).unwrap();
        assert!(d.schedules.iter().all(|x| x.rref != made) && d.vm("vm-db").unwrap().schedule.is_none());
        // VMs put on one, and on none
        c.set_vm_schedule(&["vm-db".into(), "vm-desk".into()], Some("vmss-nightly")).unwrap();
        assert_eq!(c.fetch(0).unwrap().schedules.iter().find(|x| x.rref == "vmss-nightly").unwrap().vms.len(), 4);
        c.set_vm_schedule(&["vm-web".into()], None).unwrap();
        assert_eq!(c.fetch(0).unwrap().vm("vm-web").unwrap().schedule, None);
    }

    #[test]
    fn schedules_read() {
        let r = json!({
            "name_label": "Nightly", "name_description": "", "enabled": true, "type": "checkpoint", "retained_snapshots": "7",
            "frequency": "weekly", "schedule": { "hour": "3", "min": "30", "days": "Monday,Friday" },
            "last_run_time": "20261118T03:30:02Z", "VMs": ["OpaqueRef:a", "OpaqueRef:b"],
        });
        let x = schedule_of("OpaqueRef:s", &r);
        assert_eq!((x.every, x.hour, x.min, x.keep, x.memory), (Every::Weekly, 3, 30, 7, true));
        assert_eq!((x.days.len(), x.vms.len(), x.when().as_str()), (2, 2, "Mon, Fri at 03:30"));
        assert!(x.last_run.is_some());
        // never run yet: the epoch, said as nothing
        let never = schedule_of("x", &json!({ "last_run_time": "19700101T00:00:00Z", "frequency": "hourly", "schedule": { "min": "15" } }));
        assert_eq!((never.last_run, never.when().as_str(), never.keep), (None, "Every hour at :15", 1));
    }

    #[test]
    fn addresses_changed() {
        let c = DemoConnection::new();
        c.set_ip("host-a-pif-eth0", "Static", "10.20.0.21", "255.255.0.0", "10.20.0.1", "1.1.1.1").unwrap();
        c.set_hostname("host-a", "xen-01").unwrap();
        let d = c.fetch(0).unwrap();
        let h = d.host("host-a").unwrap();
        assert_eq!((h.address.as_str(), h.hostname.as_str()), ("10.20.0.21", "xen-01"));
        assert_eq!((h.nics[0].ip.as_str(), h.nics[0].dns.as_str()), ("10.20.0.21", "1.1.1.1"));
        // a card's address taken away
        c.set_ip("host-a-pif-eth1", "None", "", "", "", "").unwrap();
        let d = c.fetch(0).unwrap();
        let n = &d.host("host-a").unwrap().nics[1];
        assert!(n.ip.is_empty() && n.mode == "None");
    }

    #[test]
    fn snapshots_taken_reverted_and_deleted() {
        let c = DemoConnection::new();
        let vm = |c: &DemoConnection| c.fetch(0).unwrap().vm("vm-web").cloned().unwrap();
        assert_eq!(vm(&c).power, PowerState::Running);
        // three taken by hand, two by its nightly schedule
        assert_eq!(vm(&c).snapshots.len(), 5);
        assert_eq!(vm(&c).snapshots.iter().filter(|x| x.scheduled).count(), 2);
        // with its memory, the newest first, and now the current state comes from it
        let snap = c.snapshot_vm("vm-web", "Before upgrade", "nginx 1.24", true).unwrap();
        let v = vm(&c);
        assert_eq!((v.snapshots.len(), v.snapshots[0].rref.as_str(), v.parent.as_deref()), (6, snap.as_str(), Some(snap.as_str())));
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
    fn demo_export_then_import() {
        let dir = std::env::temp_dir().join(format!("svm-xva-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = DemoConnection::new();
        // a running VM only from a snapshot
        let to = dir.join("web.xva");
        assert_eq!(c.export_vm("vm-web", &to, true, false, &Arc::new(Transfer::default())).unwrap_err().code, "VM_BAD_POWER_STATE");
        let t = Arc::new(Transfer::default());
        let bytes = c.export_vm("vm-web", &to, true, true, &t).unwrap();
        assert!(bytes > 0 && t.fraction().map_or(false, |f| f > 0.9));
        // what it holds, read back, then brought in under another name, its cards moved
        let info = crate::xva::read_header(&to).unwrap();
        let web = c.data.lock().unwrap().vm("vm-web").cloned().unwrap();
        assert_eq!((info.name.as_str(), info.vcpus, info.template), ("Web Server", web.vcpus, false));
        let imp = ImportVm { file: to.clone(), name: "Web Server 2".into(), sr: "sr-nfs".into(), network: Some("net-1".into()), ..Default::default() };
        let t = Arc::new(Transfer::default());
        let vm = c.import_vm(&imp, &t).unwrap();
        let d = c.data.lock().unwrap().clone();
        let v = d.vm(&vm).unwrap();
        assert_eq!((v.name.as_str(), v.power), ("Web Server 2", PowerState::Halted));
        assert!(v.disks.iter().filter(|k| k.kind == "Disk").all(|k| k.sr == "sr-nfs"));
        assert!(!v.nics.is_empty() && v.nics.iter().all(|n| n.net == "net-1"));
        // a stopped one leaves nothing
        let t = Arc::new(Transfer::default());
        t.cancel.store(true, Ordering::Relaxed);
        let imp = ImportVm { name: "Never".into(), ..imp };
        assert_eq!(c.import_vm(&imp, &t).unwrap_err().code, CANCELLED);
        assert!(!c.data.lock().unwrap().vms.iter().any(|v| v.name == "Never"));
        // a template comes in as one
        let tpl = dir.join("tpl.xva");
        c.export_vm("tpl-web", &tpl, false, false, &Arc::new(Transfer::default())).unwrap();
        let imp = ImportVm { file: tpl.clone(), name: "Web template 2".into(), sr: "sr-nfs".into(), template: true, ..Default::default() };
        let r = c.import_vm(&imp, &Arc::new(Transfer::default())).unwrap();
        assert!(c.data.lock().unwrap().user_template(&r).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn written_counts_what_a_disk_is_built_on() {
        // a VM's disk on its snapshot's file, on a template's: all three go with it
        let base = json!({"uuid": "u-base", "physical_utilisation": "9000", "sm_config": {}});
        let snap = json!({"uuid": "u-snap", "physical_utilisation": 3000, "sm_config": {"vhd-parent": "u-base"}});
        let leaf = json!({"uuid": "u-leaf", "physical_utilisation": 500, "sm_config": {"vhd-parent": "u-snap"}});
        let by_uuid: HashMap<String, &Value> = [&base, &snap, &leaf].into_iter().map(|v| (s(v, "uuid"), v)).collect();
        assert_eq!(written(&leaf, &by_uuid), 12500);
        assert_eq!(written(&base, &by_uuid), 9000);
        // a parent that isn't there (another pool's, or gone): only what's known
        let lone = json!({"uuid": "u-lone", "physical_utilisation": 700, "sm_config": {"vhd-parent": "u-gone"}});
        assert_eq!(written(&lone, &by_uuid), 700);
    }

    #[test]
    fn task_refs_read() {
        assert_eq!(task_refs("<value><array><data><value>OpaqueRef:1a-2b</value><value>OpaqueRef:NULL</value></data></array></value>"), vec!["OpaqueRef:1a-2b".to_string()]);
        assert!(task_refs("").is_empty());
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
