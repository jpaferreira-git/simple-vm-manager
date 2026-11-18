//! Updating a pool from its update channel (XenServer 8 and later): what's waiting, fetching the
//! latest from the channel, and installing them on every server, the coordinator first. A server
//! an update needs restarted is emptied first (its VMs moved to the others), restarted, and back
//! before the next. VMs that can't move (on its own storage…) are never shut down unless asked:
//! the update stops before that server instead, checked for every server before anything starts.

use crate::app::{dialog_button, dialog_buttons, dialog_window, Look};
use crate::conn::{Connection, Said, Trace};
use crate::newvm::{alert, transcript};
use crate::icons::paint_glyph;
use crate::model::*;
use crate::theme::Pal;
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Rect, Rounding, Sense, Ui};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The heights of the boxes with the update's steps and with its calls.
const STEPS_HEIGHT: f32 = 170.0;
const SHELL_HEIGHT: f32 = 180.0;

/// How long a server may take to come back from a restart.
const BACK_WITHIN: Duration = Duration::from_secs(40 * 60);

/// How a step of an update went.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum StepState {
    Doing,
    Done,
    Failed,
}

/// An update under way: its steps as they go (what, how it went, more about it), how it ended,
/// and every call made to the server with its answer (its log entry's), the steps among them.
pub struct UpdateRun {
    /// (and when each began)
    steps: Mutex<Vec<(String, StepState, String, Instant)>>,
    end: Mutex<Option<Result<String, String>>>,
    /// What's left to do by hand though it went on (VMs that didn't start again…).
    left: Mutex<Vec<String>>,
    pub trace: Arc<Trace>,
}

impl UpdateRun {
    pub fn new(trace: Arc<Trace>) -> UpdateRun {
        UpdateRun { steps: Mutex::new(vec![]), end: Mutex::new(None), left: Mutex::new(vec![]), trace }
    }
    fn leave(&self, text: impl Into<String>) {
        self.left.lock().unwrap().push(text.into());
    }
    pub fn left(&self) -> Vec<String> {
        self.left.lock().unwrap().clone()
    }
    fn begin(&self, text: impl Into<String>) -> usize {
        let text = text.into();
        self.trace.push(Said::Note, format!("# {}", text));
        let mut s = self.steps.lock().unwrap();
        s.push((text, StepState::Doing, String::new(), Instant::now()));
        s.len() - 1
    }
    fn rename(&self, i: usize, text: impl Into<String>) {
        if let Some(s) = self.steps.lock().unwrap().get_mut(i) {
            s.0 = text.into();
        }
    }
    fn end(&self, i: usize, state: StepState, more: impl Into<String>) {
        let more = more.into();
        if state == StepState::Failed {
            self.trace.push(Said::Error, more.clone());
        }
        if let Some(s) = self.steps.lock().unwrap().get_mut(i) {
            (s.1, s.2) = (state, more);
        }
    }
    pub fn steps(&self) -> Vec<(String, StepState, String, Instant)> {
        self.steps.lock().unwrap().clone()
    }
    pub fn finish(&self, result: Result<String, String>) {
        *self.end.lock().unwrap() = Some(result);
    }
    /// How it ended (None while it goes on).
    pub fn ended(&self) -> Option<Result<String, String>> {
        self.end.lock().unwrap().clone()
    }
}

/// What becomes of the running VMs that can't move off a server that restarts (their disks on its
/// own storage…): nothing (the update doesn't start), moved with their disks to another server
/// and back after, or shut down and started again after.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Stuck {
    Keep,
    Carry,
    ShutDown,
}

/// A running VM, for carrying it off with its disks: its memory, and each disk's position, size and
/// storage.
#[derive(Clone)]
pub struct PlanVm {
    pub rref: String,
    pub mem: i64,
    pub disks: Vec<(String, i64, String)>,
}

/// Storage a carried VM's disks could go to: its room, the servers that see it, and whether it
/// can send or take a running VM's disk (live storage migration).
#[derive(Clone)]
pub struct PlanSr {
    pub rref: String,
    pub free: i64,
    pub seen_by: Vec<String>,
    pub live_out: bool,
    pub live_in: bool,
}

/// What an update does: the hash of the updates to install, whether servers restart when they
/// need to, what becomes of the VMs that can't move off them, each server (the coordinator
/// first), the VMs' names; and, for carrying VMs off, the running ones, the storage, and each
/// server's free memory.
pub struct Plan {
    pub hash: String,
    pub restart: bool,
    pub stuck: Stuck,
    pub hosts: Vec<PlanHost>,
    pub vm_names: Vec<(String, String)>,
    pub vms: Vec<PlanVm>,
    pub srs: Vec<PlanSr>,
    pub room: Vec<(String, i64)>,
}

impl Plan {
    fn vm_name<'a>(&'a self, r: &'a str) -> &'a str {
        self.vm_names.iter().find(|v| v.0 == r).map_or(r, |v| v.1.as_str())
    }
    /// `text` with the references in it named (its VMs' and servers').
    fn named(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (r, name) in &self.vm_names {
            out = out.replace(r.as_str(), name);
        }
        for h in &self.hosts {
            out = out.replace(h.rref.as_str(), &h.name);
        }
        out
    }
    fn host_name<'a>(&'a self, r: &'a str) -> &'a str {
        self.hosts.iter().find(|h| h.rref == r).map_or(r, |h| h.name.as_str())
    }

    /// Where a VM stuck on server `h` can go with its disks: the other server with the most memory
    /// free (beside what's been `used` there already), and storage it sees for each disk it doesn't
    /// see already (its own first, then the roomiest): the server, and each disk's position and new
    /// storage.
    fn place(&self, h: &str, vm: &PlanVm, used: &[(String, i64)]) -> Option<(String, Vec<(String, String)>)> {
        let used_of = |r: &str| used.iter().filter(|u| u.0 == r).map(|u| u.1).sum::<i64>();
        let mut hosts: Vec<(&String, i64)> = self.room.iter().filter(|(r, _)| r != h).map(|(r, free)| (r, free - used_of(r))).filter(|(_, free)| *free >= vm.mem).collect();
        hosts.sort_by_key(|(_, free)| -free);
        'hosts: for (t, _) in hosts {
            let mut moves: Vec<(String, String)> = vec![];
            let mut taken: Vec<(String, i64)> = used.to_vec();
            for (dev, size, sr) in &vm.disks {
                let sees = |x: &PlanSr| x.seen_by.iter().any(|s| s == t);
                if self.srs.iter().any(|x| x.rref == *sr && sees(x)) {
                    continue;
                }
                // (it goes live: its storage must send it, the new one take it)
                if self.srs.iter().any(|x| x.rref == *sr && !x.live_out) {
                    return None;
                }
                let room = |x: &PlanSr| x.free - taken.iter().filter(|u| u.0 == x.rref).map(|u| u.1).sum::<i64>();
                let best = self.srs.iter().filter(|x| sees(x) && x.live_in && room(x) >= *size).max_by_key(|x| (x.seen_by.len() == 1, room(x)));
                match best {
                    Some(x) => {
                        moves.push((dev.clone(), x.rref.clone()));
                        taken.push((x.rref.clone(), *size));
                    }
                    None => continue 'hosts,
                }
            }
            return Some((t.clone(), moves));
        }
        None
    }

    /// The servers it restarts.
    fn restarts(&self) -> impl Iterator<Item = &PlanHost> {
        self.hosts.iter().filter(move |h| self.restart && ((h.updates > 0 && h.restart) || h.pending_restart))
    }
    /// VMs that can't move off a server, in words: "Mail Relay on demo-host-02 (its disks are on
    /// this server's own storage)".
    fn stuck_words(&self, host: &str, stuck: &[(String, String)]) -> String {
        stuck.iter().map(|(vm, why)| format!("{} on {} ({})", self.vm_name(vm), host, why_stuck(why))).collect::<Vec<_>>().join(", ")
    }
}

/// A VM that can't move off its server only because its disks are on that server's own storage:
/// it can go with them.
fn carryable(why: &str) -> bool {
    matches!(why.split_whitespace().next().unwrap_or_default().to_uppercase().as_str(), "VM_REQUIRES_SR" | "VM_REQUIRES_VDI")
}

/// Why a VM can't move off its server (host.get_vms_which_prevent_evacuation's error code), in words.
fn why_stuck(reason: &str) -> String {
    let code = reason.split_whitespace().next().unwrap_or_default().to_uppercase();
    match code.as_str() {
        "VM_REQUIRES_SR" | "VM_REQUIRES_VDI" => "its disks are on this server's own storage".into(),
        "HOST_NOT_ENOUGH_FREE_MEMORY" | "HOST_NOT_ENOUGH_PCPUS" => "the other servers haven't room for it".into(),
        "NO_HOSTS_AVAILABLE" => "no other server can take it".into(),
        "VM_REQUIRES_NETWORK" => "its network isn't on the other servers".into(),
        "VM_LACKS_FEATURE" | "VM_LACKS_FEATURE_SUSPEND" | "VM_MISSING_PV_DRIVERS" | "VM_OLD_PV_DRIVERS" => "it hasn't the guest tools to move".into(),
        "VM_HAS_PCI_ATTACHED" | "VM_REQUIRES_GPU" | "VM_HAS_VGPU" | "VGPU_TYPE_NOT_SUPPORTED" | "VM_HAS_SRIOV_VIF" => "it uses a device of this server".into(),
        "" => "the server didn't say why".into(),
        "NO_ROOM" => "no other server has the memory, and storage that takes running VMs' disks, for it".into(),
        c => c.to_lowercase().replace('_', " "),
    }
}

/// A server in an update: how many updates it lacks, whether they need it restarted, and whether
/// it must restart for ones installed before.
#[derive(Clone)]
pub struct PlanHost {
    pub rref: String,
    pub name: String,
    pub updates: usize,
    pub restart: bool,
    pub pending_restart: bool,
}

/// A readiness problem (an error code and its details) in words, servers named.
fn in_words(problem: &str, hosts: &[PlanHost]) -> String {
    let mut words: Vec<String> = problem.split([' ', ';', ',']).filter(|w| !w.is_empty()).map(String::from).collect();
    for w in words.iter_mut() {
        if let Some(h) = hosts.iter().find(|h| h.rref == *w) {
            *w = h.name.clone();
        }
    }
    let code = words.first().cloned().unwrap_or_default();
    let rest = words[1..].join(" ");
    match code.to_uppercase().as_str() {
        "HA_IS_ENABLED" => "High availability is on: turn it off while the pool updates".into(),
        "HOST_OFFLINE" => format!("{} is off: every server must be on", or(&rest, "A server")),
        "WLB_ENABLED" => "Workload balancing is on: turn it off while the pool updates".into(),
        _ => words.join(" "),
    }
}

/// Updates the pool as `plan` says, step by step into `run`; `restarting` is told of each server
/// about to restart (false), and back (true). In words, what was done, or why it stopped.
pub fn run(conn: &dyn Connection, plan: &Plan, r: &UpdateRun, restarting: &dyn Fn(&str, bool)) -> Result<String, String> {
    let i = r.begin("Checking the pool can be updated");
    match conn.update_readiness() {
        Ok(p) if p.is_empty() => r.end(i, StepState::Done, ""),
        Ok(p) => {
            let why = p.iter().map(|x| in_words(x, &plan.hosts)).collect::<Vec<_>>().join("; ");
            r.end(i, StepState::Failed, why.clone());
            return Err(why);
        }
        Err(e) => {
            r.end(i, StepState::Failed, e.message());
            return Err(e.message());
        }
    }
    // no VM is shut down: every server that restarts must be able to empty (those carried off with
    // their disks must have somewhere to go), checked before anything
    if plan.stuck != Stuck::ShutDown && plan.restarts().next().is_some() {
        let i = r.begin("Checking every VM can move while its server restarts");
        let mut cant: Vec<String> = vec![];
        for h in plan.restarts() {
            match conn.vms_blocking_evacuation(&h.rref) {
                Ok(stuck) if plan.stuck == Stuck::Carry => {
                    let left: Vec<(String, String)> = stuck
                        .into_iter()
                        .filter_map(|(vm, why)| match plan.vms.iter().find(|v| v.rref == vm) {
                            Some(v) if carryable(&why) && plan.place(&h.rref, v, &[]).is_some() => None,
                            _ if carryable(&why) => Some((vm, "NO_ROOM".to_string())),
                            _ => Some((vm, why)),
                        })
                        .collect();
                    if !left.is_empty() {
                        cant.push(plan.stuck_words(&h.name, &left));
                    }
                }
                Ok(stuck) if stuck.is_empty() => {}
                Ok(stuck) => cant.push(plan.stuck_words(&h.name, &stuck)),
                Err(e) => cant.push(format!("{} couldn't say ({})", h.name, e.message())),
            }
        }
        if !cant.is_empty() {
            r.end(i, StepState::Failed, format!("Can't move: {}", cant.join("; ")));
            return Err("nothing was changed, as no VM is to be shut down. Move the VMs above, or choose to shut them down for their server's restart".into());
        }
        r.end(i, StepState::Done, "");
    }
    let (mut updated, mut restarted, mut later): (usize, usize, Vec<String>) = (0, 0, vec![]);
    for h in plan.hosts.iter().filter(|h| h.updates > 0 || (h.pending_restart && plan.restart)) {
        let fail = |i: usize, e: String| -> Result<String, String> {
            let e = plan.named(&e);
            r.end(i, StepState::Failed, e.clone());
            Err(format!("{}: {}", h.name, e))
        };
        // its VMs leave first when it'll restart: those that can't, carried off with their disks
        // (when so chosen; back after), or shut down (started again after)
        let mut stopped: Vec<String> = vec![];
        let mut carried: Vec<(String, Vec<(String, String)>)> = vec![];
        let mut emptied = false;
        let carry = |carried: &mut Vec<(String, Vec<(String, String)>)>| -> Result<(), String> {
            let stuck = conn.vms_blocking_evacuation(&h.rref).unwrap_or_default();
            let mut used: Vec<(String, i64)> = vec![];
            for (vm, why) in stuck {
                let name = plan.vm_name(&vm).to_string();
                let i = r.begin(format!("{}: moving {} with its disks to another server", h.name, name));
                let Some(v) = plan.vms.iter().find(|v| v.rref == vm).filter(|_| carryable(&why)) else {
                    return fail(i, format!("stopped here, nothing shut down: {} can't move ({})", name, why_stuck(&why))).map(|_| ());
                };
                let Some((t, moves)) = plan.place(&h.rref, v, &used) else {
                    return fail(i, format!("stopped here, nothing shut down: no other server has the memory and storage for {}", name)).map(|_| ());
                };
                let size: i64 = moves.iter().filter_map(|(dev, _)| v.disks.iter().find(|d| d.0 == *dev)).map(|d| d.1).sum();
                r.rename(i, format!("{}: moving {} with its disks ({}) to {}", h.name, name, fmt_bytes(size), plan.host_name(&t)));
                let now = match conn.vm_disks(&vm) {
                    Ok((_, now)) => now,
                    Err(e) => return fail(i, e.message()).map(|_| ()),
                };
                let disks: Vec<(String, String)> = moves.iter().filter_map(|(dev, sr)| now.iter().find(|d| d.0 == *dev).map(|d| (d.1.clone(), sr.clone()))).collect();
                if let Err(e) = conn.move_vm(&MoveVm { vm: vm.clone(), host: t.clone(), disks, eject: None, running: true }) {
                    return fail(i, e.message()).map(|_| ());
                }
                r.end(i, StepState::Done, "Running all along; it comes back after the restart");
                used.push((t.clone(), v.mem));
                for (dev, sr) in &moves {
                    used.push((sr.clone(), v.disks.iter().find(|d| d.0 == *dev).map_or(0, |d| d.1)));
                }
                carried.push((vm.clone(), v.disks.iter().map(|(dev, _, sr)| (dev.clone(), sr.clone())).collect()));
            }
            Ok(())
        };
        let bring_back = |carried: &[(String, Vec<(String, String)>)]| {
            for (vm, home) in carried {
                let name = plan.vm_name(vm).to_string();
                let i = r.begin(format!("{}: bringing {} back with its disks", h.name, name));
                let back = conn.vm_disks(vm).and_then(|(running, now)| {
                    let disks = now.iter().filter_map(|(dev, vdi, sr)| home.iter().find(|d| d.0 == *dev && d.1 != *sr).map(|d| (vdi.clone(), d.1.clone()))).collect();
                    conn.move_vm(&MoveVm { vm: vm.clone(), host: h.rref.clone(), disks, eject: None, running })
                });
                match back {
                    Ok(()) => r.end(i, StepState::Done, ""),
                    Err(e) => {
                        r.end(i, StepState::Failed, plan.named(&e.message()));
                        r.leave(format!("move {} back to {} (Move, on its page)", name, h.name));
                    }
                }
            }
        };
        let empty = |stopped: &mut Vec<String>, carried: &mut Vec<(String, Vec<(String, String)>)>| -> Result<(), String> {
            if plan.stuck == Stuck::Carry {
                if let Err(e) = carry(carried) {
                    bring_back(carried);
                    return Err(e);
                }
            }
            let i = r.begin(format!("{}: emptying it for its restart", h.name));
            match conn.empty_host(&h.rref, plan.stuck == Stuck::ShutDown) {
                Ok((s, moved)) => {
                    // what happened to each of its VMs: shut down (only those that can't move) or moved
                    let mut said = vec![];
                    if !s.is_empty() {
                        let names: Vec<&str> = s.iter().map(|v| plan.vm_name(v)).collect();
                        said.push(format!("shut down, as they can't move (started again after the restart): {}", names.join(", ")));
                    }
                    said.push(match moved {
                        0 => "no other VM was running there".to_string(),
                        1 => "1 VM moved to another server, running all along".to_string(),
                        n => format!("{} VMs moved to the other servers, running all along", n),
                    });
                    let mut text = said.join("; ");
                    text[..1].make_ascii_uppercase();
                    r.end(i, StepState::Done, text);
                    *stopped = s;
                    Ok(())
                }
                // (what can't move now, in words: it may have changed since the first check)
                Err(e) if e.code == "VMS_CANT_MOVE" => {
                    let stuck = conn.vms_blocking_evacuation(&h.rref).unwrap_or_default();
                    let names = if stuck.is_empty() { e.details.iter().map(|v| plan.vm_name(v).to_string()).collect::<Vec<_>>().join(", ") } else { plan.stuck_words(&h.name, &stuck) };
                    bring_back(carried);
                    fail(i, format!("stopped here, nothing shut down: {} can't move", names)).map(|_| ())
                }
                Err(e) => {
                    bring_back(carried);
                    fail(i, e.message()).map(|_| ())
                }
            }
        };
        if plan.restart && (h.restart || (h.updates == 0 && h.pending_restart)) {
            empty(&mut stopped, &mut carried)?;
            emptied = true;
        }
        let mut pending: Vec<String> = vec![];
        if h.updates > 0 {
            let i = r.begin(format!("{}: installing {} update{}", h.name, h.updates, if h.updates == 1 { "" } else { "s" }));
            match conn.apply_updates(&h.rref, &plan.hash) {
                Ok(said) => r.end(i, StepState::Done, said.join("; ")),
                Err(e) => {
                    if emptied {
                        let _ = conn.enable_host(&h.rref);
                        start_again(conn, r, plan, h, &stopped);
                        bring_back(&carried);
                    }
                    return fail(i, e.message());
                }
            }
            updated += 1;
            // what it must do now, as it says (else as the updates said)
            pending = conn.pending_guidance(&h.rref).unwrap_or_else(|_| if h.restart { vec!["reboot_host".into()] } else { vec![] });
        } else if h.pending_restart {
            pending = vec!["reboot_host".into()];
        }
        let reboot = pending.iter().any(|g| g.starts_with("reboot_host"));
        if reboot && plan.restart {
            if !emptied {
                empty(&mut stopped, &mut carried)?;
            }
            restarting(&h.rref, false);
            let i = r.begin(format!("{}: restarting (back in a few minutes)", h.name));
            if let Err(e) = conn.restart_host_and_wait(&h.rref, BACK_WITHIN) {
                for (vm, _) in &carried {
                    r.leave(format!("move {} back to {} once it's back (Move, on its page)", plan.vm_name(vm), h.name));
                }
                return fail(i, e.message());
            }
            restarting(&h.rref, true);
            r.end(i, StepState::Done, "");
            restarted += 1;
            let i = r.begin(format!("{}: taking VMs again", h.name));
            match conn.enable_host(&h.rref) {
                Ok(()) => r.end(i, StepState::Done, ""),
                Err(e) => {
                    r.end(i, StepState::Failed, plan.named(&e.message()));
                    r.leave(format!("let {} take VMs again (XenCenter, or xe host-enable)", h.name));
                }
            }
        } else {
            if pending.iter().any(|g| g == "restart_toolstack") {
                let i = r.begin(format!("{}: restarting its toolstack", h.name));
                match conn.restart_toolstack(&h.rref) {
                    Ok(()) => r.end(i, StepState::Done, ""),
                    Err(e) => r.end(i, StepState::Failed, e.message()),
                }
            }
            if emptied {
                let _ = conn.enable_host(&h.rref);
            }
            if reboot {
                later.push(h.name.clone());
            }
        }
        start_again(conn, r, plan, h, &stopped);
        bring_back(&carried);
    }
    let mut said = match (updated, restarted) {
        (0, 0) => "Nothing to install".to_string(),
        (0, r) => format!("{} server{} restarted", r, if r == 1 { "" } else { "s" }),
        (u, 0) => format!("Installed on {} server{}", u, if u == 1 { "" } else { "s" }),
        (u, r) => format!("Installed on {} server{}, {} restarted", u, if u == 1 { "" } else { "s" }, r),
    };
    if !later.is_empty() {
        said += &format!("; {} must still restart for them to take effect", later.join(", "));
    }
    Ok(said)
}

/// Starts again the VMs shut down for a server's restart, on that server.
fn start_again(conn: &dyn Connection, r: &UpdateRun, plan: &Plan, h: &PlanHost, vms: &[String]) {
    if vms.is_empty() {
        return;
    }
    let i = r.begin(format!("{}: starting again the VM{} shut down for it", h.name, if vms.len() == 1 { "" } else { "s" }));
    let start = |vm: &str| -> Result<(), crate::conn::ApiError> {
        // (its server takes VMs by now: see enable_host)
        // (a server that's only just back may not take VMs for a moment)
        let mut tries = 0;
        loop {
            match conn.start_vm_on(vm, &h.rref) {
                Err(e) if e.code == "HOST_DISABLED" && tries < 6 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_secs(10));
                }
                r => return r,
            }
        }
    };
    let failed: Vec<(String, String)> = vms
        .iter()
        .filter_map(|vm| {
            start(vm).err().map(|e| {
                let why = match e.code.as_str() {
                    "HOST_DISABLED" | "HOST_STILL_BOOTING" => "its server isn't taking VMs yet".to_string(),
                    "HOST_NOT_ENOUGH_FREE_MEMORY" => "its server hasn't the memory free".to_string(),
                    _ => plan.named(&e.message()),
                };
                (plan.vm_name(vm).to_string(), why)
            })
        })
        .collect();
    if failed.is_empty() {
        r.end(i, StepState::Done, "");
    } else {
        let names: Vec<&str> = failed.iter().map(|f| f.0.as_str()).collect();
        r.end(i, StepState::Failed, failed.iter().map(|(n, why)| format!("{} didn't start: {}", n, why)).collect::<Vec<_>>().join("; "));
        r.leave(format!("start {} on {} (it was shut down for the restart)", names.join(", "), h.name));
    }
}

/// What the dialog shows of a pool, taken by the app each frame.
pub struct PoolView {
    /// The pools to choose from (when there's more than one), and the one shown.
    pub pools: Vec<(u32, String)>,
    pub name: String,
    pub channel: Option<String>,
    /// Its channel is an offline bundle (uploaded, not fetched), or it updates otherwise (XCP-ng).
    pub bundle: bool,
    pub xcp: bool,
    pub status: Option<(String, Standing)>,
    pub found: Option<Result<PoolUpdates, String>>,
    /// It has never fetched from its channel.
    pub never_checked: bool,
    /// Its list is being read: for how long.
    pub reading: Option<Duration>,
    /// Its VMs' names, and the running ones that can't move off their server (their disks on its
    /// own storage): their server's reference and name, and theirs.
    pub vm_names: Vec<(String, String)>,
    pub stuck: Vec<(String, String, String)>,
    /// For carrying VMs off with their disks: the running ones, the storage, each server's free
    /// memory.
    pub vms: Vec<PlanVm>,
    pub srs: Vec<PlanSr>,
    pub room: Vec<(String, i64)>,
    /// Its servers, the coordinator first: reference, name, whether it's on, whether it must
    /// restart for updates installed before.
    pub hosts: Vec<(String, String, bool, bool)>,
    /// Fetching from the channel now; and why the last fetch failed.
    pub checking: bool,
    pub check_error: Option<String>,
    pub run: Option<Arc<UpdateRun>>,
}

impl PoolView {
    /// What installing would do now (None when there's nothing to: no list, nothing lacking and
    /// no server to restart).
    /// The running VMs that can't move off a server that restarts ("Mail Relay (on demo-host-02)"),
    /// as the pool's data says (their disks on their server's own storage).
    pub fn stuck_now(&self) -> Vec<String> {
        let Some(Ok(found)) = &self.found else { return vec![] };
        let restarting = |h: &str| found.hosts.iter().any(|x| x.host == h && x.needs_restart()) || self.hosts.iter().any(|x| x.0 == h && x.3);
        self.stuck.iter().filter(|s| restarting(&s.0)).map(|s| format!("{} (on {})", s.2, s.1)).collect()
    }

    pub fn plan(&self, restart: bool, stuck: Stuck) -> Option<Plan> {
        let found = self.found.as_ref()?.as_ref().ok()?;
        let hosts: Vec<PlanHost> = self
            .hosts
            .iter()
            .map(|(r, name, _, pending)| {
                let lack = found.hosts.iter().find(|h| h.host == *r);
                PlanHost { rref: r.clone(), name: name.clone(), updates: lack.map_or(0, |h| h.updates.len()), restart: lack.map_or(false, |h| h.needs_restart()), pending_restart: *pending }
            })
            .collect();
        let something = hosts.iter().any(|h| h.updates > 0 || (h.pending_restart && restart));
        something.then(|| Plan { hash: found.hash.clone(), restart, stuck, hosts, vm_names: self.vm_names.clone(), vms: self.vms.clone(), srs: self.srs.clone(), room: self.room.clone() })
    }
}

pub enum PoolUpdateAction {
    None,
    Close,
    /// Show another pool's.
    Pick(u32),
    /// Fetch the latest from the channel.
    Check,
    /// Read its list again (after it failed).
    Reread,
    Install(Plan),
}

pub struct PoolUpdateDialog {
    pub sid: u32,
    /// Restart the servers that need it; shut down the VMs that can't move off them (else stop).
    restart: bool,
    stuck: Stuck,
    /// When the update's calls were last copied.
    copied: Option<Instant>,
}

impl PoolUpdateDialog {
    pub fn new(sid: u32) -> PoolUpdateDialog {
        PoolUpdateDialog { sid, restart: true, stuck: Stuck::Keep, copied: None }
    }

    pub fn ui(&mut self, ctx: &egui::Context, pal: &Pal, v: &PoolView) -> PoolUpdateAction {
        if v.reading.is_some() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        let mut action = PoolUpdateAction::None;
        let running = v.run.as_ref().map_or(false, |r| r.ended().is_none());
        dialog_window(ctx, &format!("Updates for {}", v.name)).id(egui::Id::new("pool-update-dialog")).show(ctx, |ui| {
            ui.set_width(600.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            // other pools
            if v.pools.len() > 1 {
                ui.horizontal_wrapped(|ui| {
                    for (sid, name) in &v.pools {
                        if ui.selectable_label(*sid == self.sid, name).clicked() && *sid != self.sid {
                            action = PoolUpdateAction::Pick(*sid);
                        }
                    }
                });
                ui.separator();
            }
            // how it stands, its channel, and Check now
            ui.horizontal(|ui| {
                if let Some((text, st)) = &v.status {
                    let (r, _) = ui.allocate_exact_size(vec2(12.0, 18.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 4.5, tone(pal, *st));
                    ui.label(egui::RichText::new(text).size(14.5).color(pal.ink));
                }
            });
            if v.xcp {
                ui.label(egui::RichText::new("XCP-ng pools update through their own tools (yum on each server, or Xen Orchestra), not from here yet.").color(pal.ink_soft));
            } else if v.channel.is_none() {
                ui.label(egui::RichText::new("This pool has no update channel. Set one up in XenCenter (Pool → Configure Updates) or with xe repository-introduce, then come back here.").color(pal.ink_soft));
            } else {
                ui.horizontal(|ui| {
                    let channel = v.channel.clone().unwrap_or_default();
                    let how = if v.bundle { format!("Update channel: {} (an offline bundle, uploaded with XenCenter)", channel) } else { format!("Update channel: {}", channel) };
                    ui.label(egui::RichText::new(how).size(12.5).color(pal.ink_faint));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if v.checking {
                            ui.label(egui::RichText::new("Fetching the latest updates…").size(12.5).color(pal.ink_soft));
                            ui.add(egui::Spinner::new().size(13.0));
                        } else if !v.bundle && dialog_button(ui, pal, "rescan", "Check now", Look::Plain, !running) {
                            action = PoolUpdateAction::Check;
                        }
                    });
                });
                if let Some(e) = &v.check_error {
                    alert(ui, pal, false, "Couldn't fetch from the channel.", e, None);
                }
            }
            ui.add_space(4.0);
            match &v.run {
                Some(run) => steps(ui, pal, run, &mut self.copied),
                None => self.lists(ui, pal, v, &mut action),
            }
            ui.add_space(6.0);
            dialog_buttons(ui, |ui| {
                let plan = v.plan(self.restart, self.stuck);
                let off: Vec<&str> = v.hosts.iter().filter(|h| !h.2).map(|h| h.1.as_str()).collect();
                if v.run.is_none() && v.channel.is_some() && !v.xcp {
                    let n = plan.as_ref().map_or(0, |p| p.hosts.iter().map(|h| h.updates).max().unwrap_or(0));
                    let what = match (&plan, n) {
                        (None, _) => "Install".to_string(),
                        (Some(_), 0) => "Restart servers".to_string(),
                        (Some(_), n) => format!("Install {} update{}", n, if n == 1 { "" } else { "s" }),
                    };
                    let blocked = self.restart && self.stuck == Stuck::Keep && !v.stuck_now().is_empty();
                    let can = plan.is_some() && off.is_empty() && !v.checking && !blocked;
                    let resp = ui.add_enabled_ui(can, |ui| dialog_button(ui, pal, "download", &what, Look::Primary, can));
                    if resp.inner {
                        if let Some(p) = plan {
                            action = PoolUpdateAction::Install(p);
                        }
                    }
                    if !off.is_empty() {
                        resp.response.on_disabled_hover_text(format!("{} {} off: every server must be on to update", off.join(", "), if off.len() == 1 { "is" } else { "are" }));
                    } else if blocked {
                        resp.response.on_disabled_hover_text("Some VMs can't move off their server: see above");
                    }
                }
                let close = if running { "Close (it carries on)" } else { "Close" };
                if dialog_button(ui, pal, "close", close, Look::Plain, true) || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    action = PoolUpdateAction::Close;
                }
            });
        });
        action
    }

    /// The updates waiting, then each server's, then whether to restart them.
    fn lists(&mut self, ui: &mut Ui, pal: &Pal, v: &PoolView, action: &mut PoolUpdateAction) {
        if v.channel.is_none() || v.xcp {
            return;
        }
        if v.never_checked && v.found.is_none() {
            ui.label(egui::RichText::new("It hasn't fetched updates from its channel yet: Check now does.").color(pal.ink_soft));
            return;
        }
        let found = match &v.found {
            None => {
                let secs = v.reading.map_or(0, |t| t.as_secs());
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(13.0));
                    let how_long = if secs >= 5 { format!("  ({}:{:02})", secs / 60, secs % 60) } else { String::new() };
                    ui.label(egui::RichText::new(format!("Reading its updates…{}", how_long)).color(pal.ink_soft));
                });
                if secs >= 5 {
                    ui.label(egui::RichText::new("The coordinator asks each server which updates it lacks before it answers: the first time after a fetch, that can take several minutes.").size(12.0).color(pal.ink_faint));
                }
                return;
            }
            Some(Err(e)) => {
                if alert(ui, pal, false, "Couldn't read its updates.", e, Some(("retry", "Read again"))) {
                    *action = PoolUpdateAction::Reread;
                }
                return;
            }
            Some(Ok(f)) => f,
        };
        let wanted = found.wanted();
        if wanted.is_empty() {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                paint_glyph(ui.painter(), r, "check", pal.green);
                ui.label(egui::RichText::new("Every server has every update the channel had when last checked.").color(pal.ink_soft));
            });
        } else {
            head(ui, pal, &format!("{} update{}", wanted.len(), if wanted.len() == 1 { "" } else { "s" }));
            egui::Frame::none().fill(pal.surface).stroke(egui::Stroke::new(1.0_f32, pal.border)).rounding(Rounding::same(8.0)).show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("updates").max_height(200.0).auto_shrink([false, true]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for (i, u) in wanted.iter().enumerate() {
                        update_row(ui, pal, u, i + 1 == wanted.len());
                    }
                });
            });
        }
        ui.add_space(4.0);
        head(ui, pal, &format!("Servers ({})", v.hosts.len()));
        egui::Frame::none().fill(pal.surface).stroke(egui::Stroke::new(1.0_f32, pal.border)).rounding(Rounding::same(8.0)).inner_margin(egui::Margin::symmetric(6.0, 4.0)).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("servers").max_height(132.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for (r, name, up, pending) in &v.hosts {
                    let lack = found.hosts.iter().find(|h| h.host == *r);
                    let n = lack.map_or(0, |h| h.updates.len());
                    let (text, col) = match (up, n, lack.map_or(false, |h| h.needs_restart()), pending) {
                        (false, _, _, _) => ("Off: can't be updated now".to_string(), pal.red),
                        (true, 0, _, true) => ("Must restart for updates installed before".to_string(), pal.orange),
                        (true, 0, _, false) => ("Up to date".to_string(), pal.green),
                        (true, n, restart, _) => (format!("{} update{}{}", n, if n == 1 { "" } else { "s" }, if restart { " · must restart after" } else { "" }), pal.ink_soft),
                    };
                    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
                    ui.painter().text(pos2(row.min.x + 6.0, row.center().y), Align2::LEFT_CENTER, name, FontId::proportional(13.5), pal.ink);
                    ui.painter().text(pos2(row.max.x - 8.0, row.center().y), Align2::RIGHT_CENTER, text, FontId::proportional(13.0), col);
                }
            });
        });
        ui.add_space(4.0);
        let needs = found.hosts.iter().any(|h| h.needs_restart()) || v.hosts.iter().any(|h| h.3);
        if needs {
            ui.checkbox(&mut self.restart, "Restart the servers that need it, one at a time, the coordinator first");
            if self.restart {
                ui.indent("stuck", |ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    ui.add(egui::Label::new(egui::RichText::new("Before a server restarts, its VMs move to the other servers and keep running. For any that can't move (disks on that server's own storage, no room on the others…):").size(12.5).color(pal.ink_soft)).wrap());
                    ui.radio_value(&mut self.stuck, Stuck::Keep, "Keep them running: don't update if one can't move");
                    ui.radio_value(&mut self.stuck, Stuck::Carry, "Move them with their disks to another server, and back after (slow: disks copied twice)");
                    ui.radio_value(&mut self.stuck, Stuck::ShutDown, "Shut only those down while their server restarts, and start them again after");
                    // the ones the pool's data says can't (their disks on their server's own storage):
                    // said only when they stop the update
                    let stuck = v.stuck_now();
                    if !stuck.is_empty() && self.stuck == Stuck::Keep {
                        let one = stuck.len() == 1;
                        ui.add_space(4.0);
                        let title = format!("{} can't move: {} on {} server's own storage.", if one { "A VM" } else { "Some VMs" }, if one { "its disks are" } else { "their disks are" }, if one { "its" } else { "their" });
                        let text = format!("{}. The update won't start till {} on shared storage, or you choose to shut {} down.", stuck.join(", "), if one { "its disks are" } else { "theirs are" }, if one { "it" } else { "them" });
                        alert(ui, pal, false, &title, &text, None);
                    }
                });
            } else {
                ui.label(egui::RichText::new("The updates are installed but some only take effect once their server restarts: do it later from its page.").size(12.0).color(pal.ink_faint));
            }
        }
    }
}

/// A list's title: small, soft, in capitals.
fn head(ui: &mut Ui, pal: &Pal, text: &str) {
    ui.label(egui::RichText::new(text.to_uppercase()).size(11.0).color(pal.ink_faint));
}

/// A dot's colour for how something stands.
pub fn tone(pal: &Pal, st: Standing) -> Color32 {
    match st {
        Standing::Good => pal.green,
        // (the network tiles' "out")
        Standing::Pending => pal.orange,
        Standing::Watch => pal.orange,
        Standing::Bad => pal.red,
        Standing::Unknown => pal.grey,
    }
}

/// An update: its kind in a tag, its summary; under it its id, when it came out and what it asks
/// of the server. Its description under the mouse; a click opens its page.
fn update_row(ui: &mut Ui, pal: &Pal, u: &UpdateInfo, last: bool) {
    let w = ui.available_width();
    let (r, resp) = ui.allocate_exact_size(vec2(w, 48.0), if u.url.is_empty() { Sense::hover() } else { Sense::click() });
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(r.shrink(2.0), Rounding::same(6.0), pal.hover);
    }
    // (security in red, as the dot that says some are waiting)
    let tag = u.kind_words();
    let col = match tag.as_str() {
        "Security" => pal.red,
        "Bug fix" => pal.blue,
        "Improvement" | "New feature" => pal.green,
        _ => pal.grey,
    };
    let tag = tag.as_str();
    let tag_r = Rect::from_min_size(pos2(r.min.x + 10.0, r.min.y + 9.0), vec2(82.0, 18.0));
    p.rect_filled(tag_r, Rounding::same(9.0), col.gamma_multiply(if pal.dark { 0.25 } else { 0.16 }));
    p.text(tag_r.center(), Align2::CENTER_CENTER, tag, FontId::proportional(11.0), col);
    let x = tag_r.max.x + 12.0;
    let room = r.max.x - 10.0 - x;
    let mut job = egui::text::LayoutJob::simple_singleline(u.summary.clone(), FontId::proportional(13.5), pal.ink);
    job.wrap = egui::text::TextWrapping { max_width: room, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    let g = p.layout_job(job);
    p.galley(pos2(x, r.min.y + 18.0 - g.size().y / 2.0), g, pal.ink);
    let asks: Vec<&str> = u
        .guidance
        .iter()
        .filter_map(|g| match g.as_str() {
            "RebootHost" => Some("restarts the server"),
            "RestartToolstack" => Some("restarts the toolstack"),
            "EvacuateHost" => Some("moves its VMs"),
            "RestartDeviceModel" | "RestartVM" => Some("VMs restart"),
            _ => None,
        })
        .collect();
    let mut about = vec![u.id.clone()];
    if let Some(t) = u.issued {
        about.push(fmt_date(t));
    }
    if u.severity.eq_ignore_ascii_case("high") {
        about.push("high severity".into());
    }
    about.extend(asks.iter().map(|a| a.to_string()));
    p.text(pos2(x, r.min.y + 35.0), Align2::LEFT_CENTER, about.join("  ·  "), FontId::proportional(12.0), pal.ink_faint);
    if !last {
        p.line_segment([pos2(r.min.x + 10.0, r.max.y - 0.5), pos2(r.max.x - 10.0, r.max.y - 0.5)], egui::Stroke::new(1.0_f32, pal.border.gamma_multiply(0.6)));
    }
    let tip = if u.description.is_empty() { u.summary.clone() } else { format!("{}\n\n{}", u.summary, u.description) };
    let resp = resp.on_hover_text(if u.url.is_empty() { tip } else { format!("{}\n\nClick to open its page.", tip) });
    if resp.clicked() {
        ui.ctx().open_url(egui::OpenUrl::new_tab(&u.url));
    }
}

/// An update's steps as they go, every call to the server under them (as in a terminal), and how
/// it ended.
fn steps(ui: &mut Ui, pal: &Pal, run: &UpdateRun, copied: &mut Option<Instant>) {
    head(ui, pal, "Updating");
    egui::Frame::none().fill(pal.surface).stroke(egui::Stroke::new(1.0_f32, pal.border)).rounding(Rounding::same(8.0)).inner_margin(egui::Margin::same(10.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        // (always this tall, scrolling to the latest: the dialog doesn't grow as the steps come)
        egui::ScrollArea::vertical().id_salt("steps").min_scrolled_height(STEPS_HEIGHT).max_height(STEPS_HEIGHT).auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            for (text, state, more, began) in run.steps() {
                // a long step says how long it's been ("restarting… 2:15")
                let secs = began.elapsed().as_secs();
                let text = if state == StepState::Doing && secs >= 5 { format!("{}   {}:{:02}", text, secs / 60, secs % 60) } else { text };
                ui.horizontal_top(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                    match state {
                        StepState::Doing => {
                            ui.put(r, egui::Spinner::new().size(13.0));
                        }
                        StepState::Done => paint_glyph(ui.painter(), r, "check", pal.green),
                        StepState::Failed => paint_glyph(ui.painter(), r, "error", pal.red),
                    }
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(egui::RichText::new(&text).color(pal.ink));
                        if !more.is_empty() {
                            ui.label(egui::RichText::new(&more).size(12.0).color(if state == StepState::Failed { pal.red } else { pal.ink_faint }));
                        }
                    });
                });
            }
        });
    });
    let ended = run.ended();
    ui.add_space(2.0);
    // (always this tall: the dialog doesn't grow as the calls come)
    transcript(ui, pal, &run.trace, copied, ended.is_none(), Some(SHELL_HEIGHT));
    match ended {
        None => {
            ui.label(egui::RichText::new("This goes on if the dialog is closed; Activities shows how it's going.").size(12.0).color(pal.ink_faint));
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
        Some(Ok(said)) => {
            let left = run.left();
            if left.is_empty() {
                ui.label(egui::RichText::new(format!("Done: {}.", said)).color(pal.green));
            } else {
                alert(ui, pal, false, &format!("{}, but some is left to do:", said), &format!("{}.", left.join("; ")), None);
            }
        }
        Some(Err(why)) => {
            alert(ui, pal, false, "Stopped:", &why, None);
        }
    }
}

/// Seconds since 1970, now.
pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|t| t.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn said_with_names() {
        let host = |r: &str, name: &str| PlanHost { rref: r.into(), name: name.into(), updates: 1, restart: true, pending_restart: false };
        let plan = Plan { hash: String::new(), restart: true, stuck: Stuck::Keep, hosts: vec![host("OpaqueRef:h1", "tico")], vm_names: vec![("OpaqueRef:v1".into(), "TrueNAS".into())], vms: vec![], srs: vec![], room: vec![] };
        assert_eq!(plan.named("The server said: HOST_DISABLED (OpaqueRef:h1) for OpaqueRef:v1"), "The server said: HOST_DISABLED (tico) for TrueNAS");
        assert_eq!(plan.stuck_words("tico", &[("OpaqueRef:v1".into(), "VM_REQUIRES_SR OpaqueRef:v1 OpaqueRef:sr".into())]), "TrueNAS on tico (its disks are on this server's own storage)");
        assert_eq!(in_words("HA_IS_ENABLED", &plan.hosts), "High availability is on: turn it off while the pool updates");
        assert_eq!(in_words("HOST_OFFLINE OpaqueRef:h1", &plan.hosts), "tico is off: every server must be on");
    }

    #[test]
    fn where_a_carried_vm_goes() {
        const GB: i64 = 1 << 30;
        let sr = |r: &str, free: i64, seen: &[&str]| PlanSr { rref: r.into(), free: free * GB, seen_by: seen.iter().map(|s| s.to_string()).collect(), live_out: true, live_in: r != "local-x" };
        let plan = Plan {
            hash: String::new(),
            restart: true,
            stuck: Stuck::Carry,
            hosts: vec![],
            vm_names: vec![],
            vms: vec![],
            srs: vec![sr("local-a", 500, &["a"]), sr("local-b", 30, &["b"]), sr("local-c", 300, &["c"]), sr("nfs", 900, &["a", "b", "c"])],
            room: vec![("a".into(), 8 * GB), ("b".into(), 64 * GB), ("c".into(), 32 * GB)],
        };
        let vm = PlanVm { rref: "v".into(), mem: 16 * GB, disks: vec![("0".into(), 100 * GB, "local-a".into()), ("1".into(), 50 * GB, "nfs".into())] };
        // the roomiest other server (b), its own storage too small for the disk: the shared one;
        // the disk already on shared storage stays
        assert_eq!(plan.place("a", &vm, &[]), Some(("b".to_string(), vec![("0".to_string(), "nfs".to_string())])));
        // b's memory taken already: c, its own storage
        assert_eq!(plan.place("a", &vm, &[("b".into(), 60 * GB)]), Some(("c".to_string(), vec![("0".to_string(), "local-c".to_string())])));
        // c's own storage can't take a running VM's disk: the shared one
        let mut no_mirror = Plan { srs: plan.srs.clone(), room: plan.room.clone(), vms: vec![], hosts: vec![], vm_names: vec![], hash: String::new(), restart: true, stuck: Stuck::Carry };
        no_mirror.srs.iter_mut().filter(|x| x.rref == "local-c").for_each(|x| x.live_in = false);
        assert_eq!(no_mirror.place("a", &vm, &[("b".into(), 60 * GB)]), Some(("c".to_string(), vec![("0".to_string(), "nfs".to_string())])));
        // nowhere with the memory
        assert_eq!(plan.place("a", &PlanVm { mem: 100 * GB, ..vm }, &[]), None);
    }
}
