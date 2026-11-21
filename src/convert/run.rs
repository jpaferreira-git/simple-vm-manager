//! A conversion, as it runs: the networks the VMs need made first (once), then each VM in turn:
//! shut down at its source, made on the pool, its disks copied into it, started. A VM whose
//! conversion fails is taken off the pool again, so nothing half made is left there.
//!
//! A disk is copied through this computer (read from the source, sent to /import_raw_vdi), or,
//! when the pool has a Helper VM that copies disks and the source lets it read the disk itself
//! (a whole raw disk: VMware's flat files, Proxmox's raw volumes), by the Helper VM: the bytes
//! then go straight from the source to the pool, not through here.

use super::*;
use crate::conn::{ApiError, Connection, Progress, Said, Transfer, CANCELLED};
use crate::model::NewNetwork;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where a source's network goes: one of the pool's, or a new one made first.
#[derive(Clone, Debug, PartialEq)]
pub enum NetTo {
    Existing(String),
    New(NewNetwork),
}

#[derive(Clone, Debug)]
pub struct NetPlan {
    /// The source's network's name, as the VMs' cards have it.
    pub source: String,
    pub to: NetTo,
}

/// A VM to convert: its name on the pool and its template.
#[derive(Clone, Debug)]
pub struct VmPlan {
    pub vm: SourceVm,
    pub name: String,
    pub template: String,
}

/// What the Convert dialog chose.
#[derive(Clone)]
pub struct Plan {
    pub source: Arc<dyn Source>,
    /// The source's name ("esxi-01.lan"), and its kind in words ("VMware").
    pub from: String,
    pub kind: String,
    /// The server the VMs start on (their home), when chosen; the server that takes their disks
    /// (whose own storage `sr` is), else the coordinator.
    pub host: Option<String>,
    pub address: Option<String>,
    pub sr: String,
    pub nets: Vec<NetPlan>,
    pub keep_macs: bool,
    pub shutdown: bool,
    pub start: bool,
    /// The pool's Helper VM copies disks it can read itself.
    pub helper: bool,
    pub vms: Vec<VmPlan>,
    /// What references are called, for the details.
    pub names: std::collections::HashMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mark {
    Todo,
    Now,
    Done,
    Failed,
}

/// How a VM's conversion goes, for the Conversions tab.
#[derive(Clone, Debug)]
pub struct JobState {
    /// Its steps, in words, and where each is.
    pub steps: Vec<(String, Mark)>,
    /// What it does now, in words ("Copying disk 1 of 2").
    pub doing: String,
    /// The disk being copied: bytes done of how many; and through the Helper VM.
    pub sent: u64,
    pub total: u64,
    pub via_helper: bool,
    pub started: Option<Instant>,
    pub ended: Option<Instant>,
    /// How it ended: the VM made (and whether it started), or why not.
    pub outcome: Option<Result<String, String>>,
    /// Things worth saying though it worked.
    pub notes: Vec<String>,
}

/// One VM's conversion.
pub struct Job {
    pub id: u64,
    pub name: String,
    pub from: String,
    pub to: String,
    pub state: Mutex<JobState>,
    pub cancel: AtomicBool,
    /// The copy under way, to stop it.
    pub transfer: Mutex<Option<Arc<Transfer>>>,
    /// Its line under Activities (the app's log), and what it reports its calls to.
    pub log: usize,
    pub progress: Arc<Progress>,
    /// Recent (when, bytes done): its speed.
    pub samples: Mutex<VecDeque<(Instant, u64)>>,
}

impl Job {
    pub fn new(id: u64, plan: &Plan, vm: &VmPlan, to: &str, log: usize, progress: Arc<Progress>) -> Job {
        let mut steps = vec![];
        if plan.shutdown && vm.vm.power == Power::On {
            steps.push((format!("Shut down on {}", plan.kind), Mark::Todo));
        }
        steps.push(("VM made".into(), Mark::Todo));
        let n = vm.vm.disks.len();
        for i in 0..n {
            steps.push((if n == 1 { "Disk".into() } else { format!("Disk {}", i + 1) }, Mark::Todo));
        }
        if plan.start {
            steps.push(("Start".into(), Mark::Todo));
        }
        Job {
            id,
            name: vm.name.clone(),
            from: plan.from.clone(),
            to: to.into(),
            state: Mutex::new(JobState { steps, doing: "Waiting its turn".into(), sent: 0, total: 0, via_helper: false, started: None, ended: None, outcome: None, notes: vec![] }),
            cancel: AtomicBool::new(false),
            transfer: Mutex::new(None),
            log,
            progress,
            samples: Mutex::new(VecDeque::new()),
        }
    }

    /// Its state now (the bytes sent read from the copy under way).
    pub fn snapshot(&self) -> JobState {
        let mut s = self.state.lock().unwrap().clone();
        if let Some(t) = self.transfer.lock().unwrap().as_ref() {
            s.sent = t.sent.load(Ordering::Relaxed);
            let total = t.total.load(Ordering::Relaxed);
            if total > 0 {
                s.total = total;
            }
        }
        s
    }

    pub fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(t) = self.transfer.lock().unwrap().as_ref() {
            t.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn set(&self, f: impl FnOnce(&mut JobState)) {
        f(&mut self.state.lock().unwrap());
    }

    fn mark(&self, i: usize, m: Mark) {
        self.set(|s| {
            if let Some(st) = s.steps.get_mut(i) {
                st.1 = m;
            }
        });
    }

    /// Bytes a second, over the last few seconds.
    pub fn rate(&self) -> Option<f64> {
        let s = self.samples.lock().unwrap();
        let (a, b) = (s.front()?, s.back()?);
        let dt = b.0.duration_since(a.0).as_secs_f64();
        (dt >= 2.0 && b.1 > a.1).then(|| (b.1 - a.1) as f64 / dt)
    }

    /// Notes how far the copy is (the window does, as it draws).
    pub fn sample(&self) {
        let st = self.snapshot();
        if st.total == 0 || st.outcome.is_some() {
            return;
        }
        let mut s = self.samples.lock().unwrap();
        let now = Instant::now();
        if s.back().map_or(true, |b| now.duration_since(b.0) >= Duration::from_millis(500)) {
            s.push_back((now, st.sent));
        }
        while s.len() > 2 && now.duration_since(s.front().unwrap().0) > Duration::from_secs(8) {
            s.pop_front();
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

fn api(e: ApiError) -> String {
    if e.code == CANCELLED {
        "Stopped.".into()
    } else {
        e.message()
    }
}

/// The date for a converted VM's description.
fn today() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    crate::model::fmt_date(now)
}

/// Runs the plan's jobs in order (`jobs` as made for its VMs); `done` is told as each ends.
pub fn run(conn: Arc<dyn Connection>, plan: Plan, jobs: Vec<Arc<Job>>, done: &dyn Fn(&Job)) {
    // the networks to make first
    let mut nets: Vec<(String, String)> = vec![];
    let mut net_error = None;
    for n in &plan.nets {
        match &n.to {
            NetTo::Existing(r) => nets.push((n.source.clone(), r.clone())),
            NetTo::New(nw) => {
                if let Some(j) = jobs.first() {
                    crate::conn::watch(&j.progress);
                    j.set(|s| s.doing = format!("Making the network '{}'", nw.name));
                }
                match conn.create_network(nw) {
                    Ok(r) => nets.push((n.source.clone(), r)),
                    Err(e) => {
                        net_error = Some(format!("The network '{}' couldn't be made: {}", nw.name, api(e)));
                        break;
                    }
                }
            }
        }
    }
    for (job, vp) in jobs.iter().zip(&plan.vms) {
        crate::conn::watch(&job.progress);
        job.set(|s| s.started = Some(Instant::now()));
        let r = match &net_error {
            Some(e) => Err(e.clone()),
            None if job.cancelled() => Err("Stopped before it began.".into()),
            None => one(&*conn, &plan, job, vp, &nets),
        };
        job.set(|s| {
            s.ended = Some(Instant::now());
            s.doing = match &r {
                Ok(_) => "Done".into(),
                Err(_) if job.cancelled() => "Stopped".into(),
                Err(_) => "Failed".into(),
            };
            for st in s.steps.iter_mut().filter(|st| st.1 == Mark::Now) {
                st.1 = if r.is_ok() { Mark::Done } else { Mark::Failed };
            }
            s.outcome = Some(r);
        });
        done(job);
    }
}

/// One VM's conversion; the VM made on the pool.
fn one(conn: &dyn Connection, plan: &Plan, job: &Job, vp: &VmPlan, nets: &[(String, String)]) -> Result<String, String> {
    let vm = &vp.vm;
    let tr = job.progress.trace.clone();
    let mut step = 0usize;
    // 1. off at its source
    match vm.power {
        Power::On if plan.shutdown => {
            job.mark(step, Mark::Now);
            job.set(|s| s.doing = format!("Shutting it down on {}", plan.kind));
            tr.push(Said::Note, format!("Shutting '{}' down on {}", vm.name, plan.from));
            plan.source.shutdown(vm, &|t| job.set(|s| s.doing = t))?;
            job.mark(step, Mark::Done);
            step += 1;
        }
        Power::On => return Err(format!("It's running on {}: a disk is copied only while its VM is off. Shut it down there, or tick “Shut them down first”.", plan.kind)),
        Power::Paused => return Err("It's suspended: resume it and shut it down at its source first.".into()),
        _ => {}
    }
    if job.cancelled() {
        return Err("Stopped.".into());
    }
    // 2. made on the pool
    job.mark(step, Mark::Now);
    job.set(|s| s.doing = "Making the VM on the pool".into());
    let target = Target {
        name: vp.name.clone(),
        description: format!("Converted from '{}' on {} ({}), {}, by Simple VM Manager", vm.name, plan.from, plan.kind, today()),
        template: vp.template.clone(),
        vcpus: vm.cpus.max(1),
        cores: if vm.cores > 0 && vm.cpus % vm.cores == 0 { vm.cores } else { 1 },
        memory: vm.memory.max(256 << 20) as i64,
        uefi: vm.uefi,
        secure_boot: vm.secure_boot,
        vtpm: vm.vtpm,
        host: plan.host.clone(),
        sr: plan.sr.clone(),
        disks: vm.disks.iter().enumerate().map(|(i, d)| (if vm.disks.len() == 1 { format!("{} disk", vp.name) } else { format!("{} disk {}", vp.name, i + 1) }, d.size.div_ceil(512) * 512)).collect(),
        nics: vm.nics.iter().filter_map(|n| nets.iter().find(|(s, _)| *s == n.network).map(|(_, r)| (r.clone(), if plan.keep_macs { n.mac.clone() } else { String::new() }))).collect(),
        names: plan.names.clone(),
    };
    let made = conn.convert_make(&target, &tr).map_err(|e| format!("The VM couldn't be made: {}", api(e)))?;
    job.mark(step, Mark::Done);
    step += 1;
    // 3. its disks; a failure takes the VM away again
    let copied = (|| -> Result<(), String> {
        for (i, (d, vdi)) in vm.disks.iter().zip(&made.vdis).enumerate() {
            if job.cancelled() {
                return Err("Stopped.".into());
            }
            job.mark(step + i, Mark::Now);
            let n = vm.disks.len();
            job.set(|s| {
                s.doing = if n == 1 { "Copying its disk".into() } else { format!("Copying disk {} of {}", i + 1, n) };
                s.sent = 0;
                s.total = d.size;
            });
            job.samples.lock().unwrap().clear();
            copy_disk(conn, plan, job, vm, d, vdi, i)?;
            job.mark(step + i, Mark::Done);
        }
        Ok(())
    })();
    if let Err(e) = copied {
        tr.push(Said::Note, "Taking the half-made VM away again:");
        match conn.delete_vm(&made.vm, true) {
            Ok(_) => job.set(|s| s.notes.push("Nothing was left on the pool.".into())),
            Err(x) => job.set(|s| s.notes.push(format!("The half-made VM '{}' couldn't be deleted ({}): delete it yourself.", vp.name, api(x)))),
        }
        return Err(e);
    }
    step += vm.disks.len();
    job.set(|s| {
        s.total = 0;
        s.sent = 0;
    });
    // 4. started
    if plan.start {
        job.mark(step, Mark::Now);
        job.set(|s| s.doing = "Starting it".into());
        let started = match &plan.host {
            Some(h) => conn.start_vm_on(&made.vm, h),
            None => conn.start_traced(&made.vm, &format!("\"{}\"", vp.name), &tr),
        };
        match started {
            Ok(()) => job.mark(step, Mark::Done),
            Err(e) => {
                job.mark(step, Mark::Failed);
                job.set(|s| s.notes.push(format!("It's made, but didn't start: {}", api(e))));
            }
        }
    }
    if vm.family == Family::Windows {
        job.set(|s| s.notes.push("Windows may ask to be activated again: to it, this is new hardware.".into()));
    }
    Ok(made.vm)
}

/// Copies one disk into its VDI: by the Helper VM when it can, else through this computer.
fn copy_disk(conn: &dyn Connection, plan: &Plan, job: &Job, vm: &SourceVm, d: &SourceDisk, vdi: &str, i: usize) -> Result<(), String> {
    let tr = job.progress.trace.clone();
    if plan.helper {
        if let Some(h) = plan.source.helper_read(vm, d) {
            match by_helper(conn, plan, job, d, vdi, h, i) {
                Ok(true) => return Ok(()),
                Ok(false) => {
                    tr.push(Said::Note, "The Helper VM didn't take the copy: copying through this computer instead.");
                    job.set(|s| s.notes.push("The Helper VM didn't take a disk's copy, so it went through this computer.".into()));
                }
                Err(e) => return Err(e),
            }
        }
    }
    job.set(|s| s.via_helper = false);
    let t = Arc::new(Transfer::default());
    if job.cancelled() {
        t.cancel.store(true, Ordering::Relaxed);
    }
    *job.transfer.lock().unwrap() = Some(t.clone());
    let data = plan.source.open_disk(vm, d)?;
    let r = match data {
        DiskData::Image(mut img) => {
            job.set(|s| s.doing = format!("{} (looking at what's written)", s.doing.trim_end_matches(" (looking at what's written)")));
            let map = img.map().map_err(|e| format!("{}: {}", d.label, e))?;
            let stream = disk::VhdStream::new(img, &map);
            let len = stream.len();
            let written = stream.carried();
            job.set(|s| {
                s.doing = s.doing.trim_end_matches(" (looking at what's written)").to_string();
                s.total = len;
            });
            tr.push(Said::Note, format!("{}: {} of {} written, sent as a VHD of {}", d.label, crate::model::fmt_bytes(written as i64), crate::model::fmt_bytes(d.size as i64), crate::model::fmt_bytes(len as i64)));
            conn.convert_write(vdi, plan.address.as_deref(), Box::new(stream), len, true, &t)
        }
        DiskData::Stream { reader, len } => {
            job.set(|s| s.total = len);
            conn.convert_write(vdi, plan.address.as_deref(), reader, len, false, &t)
        }
    };
    job.set(|s| s.sent = t.sent.load(Ordering::Relaxed));
    *job.transfer.lock().unwrap() = None;
    r.map_err(|e| if e.code == CANCELLED { "Stopped.".to_string() } else { format!("{}: {}", d.label, e.message()) })
}

/// Hands a disk's copy to the Helper VM and follows it. Ok(false): it didn't take it in time
/// (copy it here instead).
fn by_helper(conn: &dyn Connection, plan: &Plan, job: &Job, d: &SourceDisk, vdi: &str, h: HelperRead, i: usize) -> Result<bool, String> {
    let label = format!("{} disk {}", job.name, i + 1);
    let (task, secret) = conn.helper_copy_start(vdi, plan.address.as_deref(), &h.job, &h.password, &label).map_err(api)?;
    job.set(|s| s.via_helper = true);
    let asked = Instant::now();
    let r = loop {
        std::thread::sleep(Duration::from_secs(2));
        if job.cancelled() {
            break Err("Stopped.".to_string());
        }
        let p = match conn.helper_copy_poll(&task) {
            Ok(p) => p,
            Err(e) => break Err(format!("Following the Helper VM's copy: {}", api(e))),
        };
        job.set(|s| {
            s.sent = (p.progress.clamp(0.0, 1.0) * d.size as f64) as u64;
            s.total = d.size;
        });
        // (what the Helper VM says went wrong first: it cancels the task after saying it)
        if !p.error.is_empty() {
            break Err(format!("{} (the Helper VM's copy): {}", d.label, p.error));
        }
        match p.status.as_str() {
            "success" => break Ok(true),
            "failure" => break Err(format!("{} (the Helper VM's copy): it failed", d.label)),
            "cancelled" | "cancelling" => break Err("The Helper VM's copy was cancelled.".into()),
            _ => {}
        }
        if p.claimed.is_empty() && asked.elapsed() > Duration::from_secs(90) {
            break Ok(false);
        }
    };
    conn.helper_copy_end(&task, &secret, !matches!(r, Ok(true)));
    if matches!(r, Ok(false)) {
        job.set(|s| {
            s.via_helper = false;
            s.sent = 0;
        });
    }
    r
}
