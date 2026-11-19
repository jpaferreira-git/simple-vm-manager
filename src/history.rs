//! A VM's or a server's records over more than the Overview's last ten minutes, as its server keeps
//! them: a point every 5 seconds for ten minutes, every minute for two hours, every hour for a
//! week and every day for a year (its RRD's archives, read through rrd_updates). And the charts
//! the Performance tab makes of them: CPU, memory, disk and network, then everything else the
//! server measures, item by item.

use crate::model::{civil_from_days, fmt_bytes, HostInfo, PoolData, PowerState, VmDisk, VmInfo};
use std::collections::BTreeMap;
use std::time::Duration;

/// How far back the Performance tab looks.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Span {
    Minutes,
    Hours,
    Week,
    Year,
}

impl Span {
    pub const ALL: [Span; 4] = [Span::Minutes, Span::Hours, Span::Week, Span::Year];

    pub fn label(self) -> &'static str {
        match self {
            Span::Minutes => "10 min",
            Span::Hours => "2 hours",
            Span::Week => "1 week",
            Span::Year => "1 year",
        }
    }

    /// What it shows, in words.
    pub fn note(self) -> &'static str {
        match self {
            Span::Minutes => "The last 10 minutes · a point every 5 seconds",
            Span::Hours => "The last 2 hours · a point a minute",
            Span::Week => "The last week · a point an hour",
            Span::Year => "The last year · a point a day",
        }
    }

    /// Seconds between its points: the server keeps a record at each.
    pub fn step(self) -> i64 {
        match self {
            Span::Minutes => 5,
            Span::Hours => 60,
            Span::Week => 3600,
            Span::Year => 86_400,
        }
    }

    /// How many points the server keeps of it.
    fn points(self) -> i64 {
        match self {
            Span::Minutes | Span::Hours => 120,
            Span::Week => 168,
            Span::Year => 366,
        }
    }

    /// Where to ask from, for the server's time `now`. A little short of all it keeps: asked from
    /// before what an archive holds, the server answers from the next, coarser one (a clock a
    /// little ahead of this computer's would do that).
    pub fn start(self, now: i64) -> i64 {
        now - (self.points() - 2) * self.step()
    }

    /// How often it's read again while it's shown: about when it has a new point.
    pub fn every(self) -> Duration {
        Duration::from_secs(match self {
            Span::Minutes => 5,
            Span::Hours => 60,
            Span::Week => 600,
            Span::Year => 3600,
        })
    }
}

/// A server's records as one rrd_updates answer has them: each one's values at `times` (seconds
/// since 1970, oldest first), NaN where there's none (before they were kept, or while a VM was
/// off).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Records {
    pub times: Vec<i64>,
    /// By name: the server's own as "host:cpu0", a VM's as "vm:<uuid>:cpu0".
    pub cols: BTreeMap<String, Vec<f64>>,
}

/// An rrd_updates answer (XML, or JSON from newer servers) as `Records`: its averages, a row per
/// time.
pub fn read_records(text: &str) -> Records {
    let (legend, mut rows) = crate::model::rrd_table_raw(text);
    rows.sort_by_key(|r| r.0);
    rows.dedup_by_key(|r| r.0);
    let mut out = Records { times: rows.iter().map(|r| r.0).collect(), ..Default::default() };
    for (i, e) in legend.iter().enumerate() {
        let mut parts = e.splitn(4, ':');
        let (Some("AVERAGE"), Some(kind), Some(uuid), Some(name)) = (parts.next(), parts.next(), parts.next(), parts.next()) else { continue };
        let key = match kind {
            "host" => format!("host:{}", name),
            "vm" => format!("vm:{}:{}", uuid, name),
            _ => continue,
        };
        out.cols.insert(key, rows.iter().map(|r| r.1.get(i).copied().unwrap_or(f64::NAN)).collect());
    }
    out
}

/// What a chart's figures are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unit {
    /// A share, 0 to 1 (shown as a percentage).
    Share,
    Bytes,
    /// Bytes a second.
    Rate,
    /// Requests a second.
    PerSec,
    /// Microseconds (shown in ms).
    Micros,
    /// A plain number (the server's load).
    Plain,
}

/// A line's colour, as the palette has it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hue {
    Accent,
    Green,
    Blue,
    Orange,
    Purple,
    Teal,
    Pink,
    Grey,
}

/// The colours given to many lines of the same kind (a server's VMs), in turn.
const HUES: [Hue; 7] = [Hue::Accent, Hue::Teal, Hue::Orange, Hue::Purple, Hue::Green, Hue::Pink, Hue::Grey];

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub name: String,
    pub hue: Hue,
    pub vals: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Chart {
    pub title: String,
    pub unit: Unit,
    pub lines: Vec<Line>,
    /// The top of its scale when that's fixed (all of a CPU, all the memory there is); else it's
    /// fitted to its peak.
    pub top: Option<f64>,
    /// What it's out of, in words ("4 CPUs", "4 GB"); empty when nothing.
    pub of: String,
    /// As wide as its group (a chart with many lines, or alone).
    pub wide: bool,
}

impl Chart {
    fn new(title: &str, unit: Unit, lines: Vec<Line>) -> Chart {
        Chart { title: title.into(), unit, lines, top: None, of: String::new(), wide: false }
    }

    /// Its scale's top: fixed, or a round figure over its peak.
    pub fn scale(&self) -> f64 {
        if let Some(t) = self.top.filter(|t| *t > 0.0) {
            return t;
        }
        let peak = self.lines.iter().filter_map(|l| peak(&l.vals)).fold(0.0f64, f64::max) * 1.15;
        match self.unit {
            // in whole KB/s or MB/s, GB
            Unit::Rate | Unit::Bytes => {
                let k = if self.unit == Unit::Bytes { 1024.0 * 1024.0 * 1024.0 } else if peak >= 1024.0 * 1024.0 { 1024.0 * 1024.0 } else { 1024.0 };
                nice(peak / k) * k
            }
            Unit::Share => 1.0,
            _ => nice(peak),
        }
    }
}

/// What a group of charts is about (for its icon).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GroupKind {
    Vms,
    Cpus,
    Load,
    Disk,
    Card,
    Storage,
}

/// One of the things measured besides the four totals (a disk, a card, each CPU…): its row, and
/// its charts under it once it's opened.
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    /// Remembered open or closed by it, for every VM or server alike ("cpus", "disk:0"…).
    pub key: String,
    pub kind: GroupKind,
    pub name: String,
    pub detail: String,
    pub charts: Vec<Chart>,
}

/// Everything the Performance tab shows: the four totals, then the rest by group.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Charts {
    pub main: Vec<Chart>,
    pub groups: Vec<Group>,
}

/// The values that are there (not gaps).
fn there(v: &[f64]) -> impl Iterator<Item = f64> + '_ {
    v.iter().copied().filter(|x| x.is_finite())
}

pub fn average(v: &[f64]) -> Option<f64> {
    let (n, s) = there(v).fold((0usize, 0.0), |(n, s), x| (n + 1, s + x));
    (n > 0).then(|| s / n as f64)
}

pub fn peak(v: &[f64]) -> Option<f64> {
    there(v).fold(None, |m: Option<f64>, x| Some(m.map_or(x, |m| m.max(x))))
}

/// The newest value there is.
pub fn last(v: &[f64]) -> Option<f64> {
    v.iter().rev().copied().find(|x| x.is_finite())
}

/// Where its values begin (the first that's there).
pub fn first_there(v: &[f64]) -> Option<usize> {
    v.iter().position(|x| x.is_finite())
}

/// Where its peak is.
pub fn index_of_peak(v: &[f64]) -> Option<usize> {
    let p = peak(v)?;
    v.iter().position(|x| *x == p)
}

/// Each point's values added up; a gap only where all of them are.
fn add(cols: &[&Vec<f64>], n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| {
            let vals: Vec<f64> = cols.iter().filter_map(|c| c.get(i).copied().filter(|v| v.is_finite())).collect();
            if vals.is_empty() {
                f64::NAN
            } else {
                vals.iter().sum()
            }
        })
        .collect()
}

/// Each point's values averaged; a gap only where all of them are.
fn mean(cols: &[&Vec<f64>], n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| {
            let vals: Vec<f64> = cols.iter().filter_map(|c| c.get(i).copied().filter(|v| v.is_finite())).collect();
            if vals.is_empty() {
                f64::NAN
            } else {
                vals.iter().sum::<f64>() / vals.len() as f64
            }
        })
        .collect()
}

/// A round figure a little over `v`: 1, 2 or 5 times a power of ten.
pub fn nice(v: f64) -> f64 {
    if !(v > 0.0) {
        return 1.0;
    }
    let e = 10f64.powf(v.log10().floor());
    let m = v / e;
    (if m <= 1.0 {
        1.0
    } else if m <= 2.0 {
        2.0
    } else if m <= 5.0 {
        5.0
    } else {
        10.0
    }) * e
}

/// "cpu12" → 12.
fn cpu_index(name: &str) -> Option<usize> {
    name.strip_prefix("cpu").filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))?.parse().ok()
}

/// The name a VM's disk goes by in its records: what its system calls it while it runs ("xvda"),
/// or, when that isn't known, the usual name for the disk in its place.
pub fn disk_dev(d: &VmDisk) -> String {
    if !d.dev.is_empty() {
        return d.dev.clone();
    }
    let n: u8 = d.device.parse().unwrap_or(0);
    format!("xvd{}", (b'a' + n.min(25)) as char)
}

/// A storage's records end in the first 8 characters of its uuid ("read_1c2b3a4d"): what they
/// are and whose.
fn sr_stat(name: &str) -> Option<(&str, &str)> {
    let (what, sr) = name.rsplit_once('_')?;
    let ok = matches!(what, "read" | "write" | "iops_read" | "iops_write" | "latency" | "read_latency" | "write_latency");
    (ok && sr.len() == 8 && sr.chars().all(|c| c.is_ascii_hexdigit())).then_some((what, sr))
}

/// "4 CPUs", "1 CPU".
fn cpus_text(n: usize) -> String {
    format!("{} CPU{}", n, if n == 1 { "" } else { "s" })
}

/// A device's charts from its records by kind ("read", "iops_write", "latency"…): read and
/// write, requests, and the wait for them, as far as the server keeps them.
fn io_charts(k: &BTreeMap<&str, &Vec<f64>>, n: usize, wait: &str) -> Vec<Chart> {
    let mut out = vec![];
    let pair = |a: &str, b: &str| -> Option<(Vec<f64>, Vec<f64>)> {
        let (x, y) = (k.get(a), k.get(b));
        (x.is_some() || y.is_some()).then(|| (x.map_or(vec![f64::NAN; n], |v| (*v).clone()), y.map_or(vec![f64::NAN; n], |v| (*v).clone())))
    };
    if let Some((r, w)) = pair("read", "write") {
        out.push(Chart::new("Read and write", Unit::Rate, vec![Line { name: "read".into(), hue: Hue::Blue, vals: r }, Line { name: "write".into(), hue: Hue::Orange, vals: w }]));
    }
    if let Some((r, w)) = pair("iops_read", "iops_write") {
        out.push(Chart::new("Requests", Unit::PerSec, vec![Line { name: "read".into(), hue: Hue::Blue, vals: r }, Line { name: "write".into(), hue: Hue::Orange, vals: w }]));
    }
    let latency = match (k.get("latency"), k.get("read_latency"), k.get("write_latency")) {
        (Some(l), _, _) => Some((*l).clone()),
        (None, Some(r), Some(w)) => Some(mean(&[*r, *w], n)),
        (None, Some(x), None) | (None, None, Some(x)) => Some((*x).clone()),
        _ => None,
    };
    if let Some(l) = latency {
        out.push(Chart::new(wait, Unit::Micros, vec![Line { name: "wait".into(), hue: Hue::Purple, vals: l }]));
    }
    out
}

/// A running VM's charts from its records (asked for it alone, or with its server's).
pub fn vm_charts(r: &Records, v: &VmInfo) -> Charts {
    let n = r.times.len();
    let pre = format!("vm:{}:", v.uuid);
    let mine: BTreeMap<&str, &Vec<f64>> = r.cols.iter().filter_map(|(k, c)| Some((k.strip_prefix(pre.as_str())?, c))).collect();
    let none = vec![f64::NAN; n];
    // its CPUs, in order, and all of them together
    let mut cpus: Vec<(usize, &Vec<f64>)> = mine.iter().filter_map(|(k, c)| Some((cpu_index(k)?, *c))).collect();
    cpus.sort_by_key(|c| c.0);
    let cpu = mean(&cpus.iter().map(|c| c.1).collect::<Vec<_>>(), n);
    let count = if cpus.is_empty() { v.vcpus.max(1) as usize } else { cpus.len() };
    // what its system uses, when its tools say how much is free inside it (KiB); else what it has
    let given = mine.get("memory").copied();
    let free = mine.get("memory_internal_free").copied().filter(|f| there(f).any(|x| x > 0.0));
    let (mem_name, mem) = match (given, free) {
        (Some(m), Some(f)) => ("In use", m.iter().zip(f).map(|(m, f)| if m.is_finite() && f.is_finite() { (m - f * 1024.0).max(0.0) } else { f64::NAN }).collect()),
        (Some(m), None) => ("Given to it", m.clone()),
        _ => ("In use", none.clone()),
    };
    let mem_top = given.and_then(|g| peak(g)).unwrap_or(0.0).max(v.mem_dyn_max as f64);
    // its disks and cards, by the names its records give them ("vbd_xvda_read", "vif_0_tx")
    let mut disks: BTreeMap<&str, BTreeMap<&str, &Vec<f64>>> = BTreeMap::new();
    let mut cards: BTreeMap<&str, BTreeMap<&str, &Vec<f64>>> = BTreeMap::new();
    for (k, c) in &mine {
        if let Some((dev, what)) = k.strip_prefix("vbd_").and_then(|x| x.split_once('_')) {
            disks.entry(dev).or_default().insert(what, c);
        } else if let Some((dev, what)) = k.strip_prefix("vif_").and_then(|x| x.split_once('_')) {
            cards.entry(dev).or_default().insert(what, c);
        }
    }
    let total = |m: &BTreeMap<&str, BTreeMap<&str, &Vec<f64>>>, what: &str| -> Vec<f64> { add(&m.values().filter_map(|k| k.get(what).copied()).collect::<Vec<_>>(), n) };
    let two = |a: &str, b: &str, x: Vec<f64>, y: Vec<f64>| vec![Line { name: a.into(), hue: Hue::Blue, vals: x }, Line { name: b.into(), hue: Hue::Orange, vals: y }];
    let main = vec![
        Chart { top: Some(1.0), of: cpus_text(count), ..Chart::new("CPU", Unit::Share, vec![Line { name: "CPU".into(), hue: Hue::Accent, vals: cpu }]) },
        Chart { top: Some(mem_top), of: fmt_bytes(mem_top as i64), ..Chart::new("Memory", Unit::Bytes, vec![Line { name: mem_name.into(), hue: Hue::Green, vals: mem }]) },
        Chart::new("Disk", Unit::Rate, two("read", "write", total(&disks, "read"), total(&disks, "write"))),
        Chart::new("Network", Unit::Rate, two("in", "out", total(&cards, "rx"), total(&cards, "tx"))),
    ];
    let mut groups = vec![];
    if cpus.len() > 1 {
        let charts = cpus.iter().map(|(i, c)| Chart { top: Some(1.0), ..Chart::new(&format!("CPU {}", i), Unit::Share, vec![Line { name: format!("CPU {}", i), hue: Hue::Accent, vals: (*c).clone() }]) }).collect();
        groups.push(Group { key: "cpus".into(), kind: GroupKind::Cpus, name: "Each CPU".into(), detail: cpus_text(cpus.len()), charts });
    }
    // its disks in their order (by where they're plugged in), then any the VM's details don't name
    let mut found: Vec<(String, &str, Option<&VmDisk>)> = disks.keys().map(|dev| {
        let d = v.disks.iter().find(|d| disk_dev(d) == *dev);
        (d.map_or(format!("~{}", dev), |d| format!("{:>4}", d.device)), *dev, d)
    }).collect();
    found.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    for (_, dev, d) in found {
        let charts = io_charts(&disks[dev], n, "Wait for the disk");
        if charts.is_empty() {
            continue;
        }
        let (name, detail, key) = match d {
            Some(d) if d.kind == "DVD drive" => ("DVD drive".to_string(), d.name.clone(), format!("disk:{}", d.device)),
            Some(d) => (format!("Disk {}", d.device), [fmt_bytes(d.size), d.sr_name.clone()].into_iter().filter(|s| !s.is_empty() && s != "0 B").collect::<Vec<_>>().join(" · "), format!("disk:{}", d.device)),
            None => (dev.to_string(), String::new(), format!("disk:{}", dev)),
        };
        groups.push(Group { key, kind: GroupKind::Disk, name, detail, charts });
    }
    let mut found: Vec<&str> = cards.keys().copied().collect();
    found.sort_by_key(|d| (d.parse::<u32>().unwrap_or(u32::MAX), d.to_string()));
    for dev in found {
        let k = &cards[dev];
        if !k.contains_key("rx") && !k.contains_key("tx") {
            continue;
        }
        let get = |w: &str| k.get(w).map_or(none.clone(), |v| (*v).clone());
        let nic = v.nics.iter().find(|x| x.device == dev);
        let detail = nic.map_or(String::new(), |x| [x.network.clone(), x.ip.clone()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · "));
        let chart = Chart { wide: true, ..Chart::new("Traffic", Unit::Rate, two("in", "out", get("rx"), get("tx"))) };
        groups.push(Group { key: format!("card:{}", dev), kind: GroupKind::Card, name: format!("Card {}", dev), detail, charts: vec![chart] });
    }
    Charts { main, groups }
}

/// A server's charts from its records (asked with its VMs'): its pool's details name its VMs,
/// cards and storage.
pub fn host_charts(r: &Records, h: &HostInfo, d: &PoolData) -> Charts {
    let n = r.times.len();
    let own: BTreeMap<&str, &Vec<f64>> = r.cols.iter().filter_map(|(k, c)| Some((k.strip_prefix("host:")?, c))).collect();
    let none = vec![f64::NAN; n];
    let get = |name: &str| own.get(name).map(|c| (*c).clone());
    let mut cpus: Vec<(usize, &Vec<f64>)> = own.iter().filter_map(|(k, c)| Some((cpu_index(k)?, *c))).collect();
    cpus.sort_by_key(|c| c.0);
    let cpu = if cpus.is_empty() { get("cpu_avg").unwrap_or_else(|| none.clone()) } else { mean(&cpus.iter().map(|c| c.1).collect::<Vec<_>>(), n) };
    let count = if cpus.is_empty() { h.cpu_count.max(1) as usize } else { cpus.len() };
    // memory in use: all of it but what's free (KiB)
    let (total, free) = (get("memory_total_kib"), get("memory_free_kib"));
    let mem = match (&total, &free) {
        (Some(t), Some(f)) => t.iter().zip(f).map(|(t, f)| if t.is_finite() && f.is_finite() { (t - f).max(0.0) * 1024.0 } else { f64::NAN }).collect(),
        _ => none.clone(),
    };
    let mem_top = total.as_ref().and_then(|t| peak(t)).map_or(0.0, |t| t * 1024.0).max(h.mem_total as f64);
    // its storage's records, by storage ("read_<sr8>"…)
    let mut srs: BTreeMap<&str, BTreeMap<&str, &Vec<f64>>> = BTreeMap::new();
    let mut pifs: BTreeMap<&str, BTreeMap<&str, &Vec<f64>>> = BTreeMap::new();
    for (k, c) in &own {
        if let Some((what, sr)) = sr_stat(k) {
            srs.entry(sr).or_default().insert(what, c);
        } else if let Some((dev, what)) = k.strip_prefix("pif_").and_then(|x| x.rsplit_once('_')) {
            if (what == "rx" || what == "tx") && dev != "lo" && dev != "aggr" {
                pifs.entry(dev).or_default().insert(what, c);
            }
        }
    }
    let total_of = |m: &BTreeMap<&str, BTreeMap<&str, &Vec<f64>>>, what: &str| -> Vec<f64> { add(&m.values().filter_map(|k| k.get(what).copied()).collect::<Vec<_>>(), n) };
    // read and written: by its storage, else its VMs' disks added up
    let (read, write) = if srs.values().any(|k| k.contains_key("read")) {
        (total_of(&srs, "read"), total_of(&srs, "write"))
    } else {
        let disk = |what: &str| -> Vec<f64> {
            let cols: Vec<&Vec<f64>> = r.cols.iter().filter(|(k, _)| k.starts_with("vm:") && k.rsplit(':').next().and_then(|x| x.strip_prefix("vbd_")).and_then(|x| x.split_once('_')).map_or(false, |(_, w)| w == what)).map(|(_, c)| c).collect();
            add(&cols, n)
        };
        (disk("read"), disk("write"))
    };
    // in and out: the server's own count of all its cards (a bond's members aren't counted twice)
    let (rx, tx) = match (get("pif_aggr_rx"), get("pif_aggr_tx")) {
        (Some(a), Some(b)) => (a, b),
        _ => (total_of(&pifs, "rx"), total_of(&pifs, "tx")),
    };
    let two = |a: &str, b: &str, x: Vec<f64>, y: Vec<f64>| vec![Line { name: a.into(), hue: Hue::Blue, vals: x }, Line { name: b.into(), hue: Hue::Orange, vals: y }];
    let main = vec![
        Chart { top: Some(1.0), of: cpus_text(count), ..Chart::new("CPU", Unit::Share, vec![Line { name: "CPU".into(), hue: Hue::Accent, vals: cpu }]) },
        Chart { top: Some(mem_top), of: fmt_bytes(mem_top as i64), ..Chart::new("Memory", Unit::Bytes, vec![Line { name: "In use".into(), hue: Hue::Green, vals: mem }]) },
        Chart::new("Storage", Unit::Rate, two("read", "write", read, write)),
        Chart::new("Network", Unit::Rate, two("in", "out", rx, tx)),
    ];
    let mut groups = vec![];
    // its VMs' CPUs, each of its own: the busiest few
    let mut vms: Vec<(f64, String, Vec<f64>)> = vec![];
    for vm in d.vms.iter() {
        let pre = format!("vm:{}:", vm.uuid);
        let cols: Vec<&Vec<f64>> = r.cols.iter().filter(|(k, _)| k.strip_prefix(pre.as_str()).map_or(false, |x| cpu_index(x).is_some())).map(|(_, c)| c).collect();
        if cols.is_empty() {
            continue;
        }
        let m = mean(&cols, n);
        if let Some(a) = average(&m) {
            vms.push((a, vm.name.clone(), m));
        }
    }
    if !vms.is_empty() {
        vms.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1.cmp(&b.1)));
        let all = vms.len();
        vms.truncate(HUES.len());
        let names: Vec<&str> = vms.iter().map(|v| v.1.as_str()).collect();
        let detail = if all > vms.len() {
            format!("The {} busiest of {}", vms.len(), all)
        } else if names.len() <= 3 {
            names.join(", ")
        } else {
            format!("{}, {} and {} more", names[0], names[1], names.len() - 2)
        };
        let lines = vms.into_iter().zip(HUES).map(|((_, name, vals), hue)| Line { name, hue, vals }).collect();
        let chart = Chart { top: Some(1.0), wide: true, ..Chart::new("Each one's share of its own CPUs", Unit::Share, lines) };
        groups.push(Group { key: "vms".into(), kind: GroupKind::Vms, name: "CPU of each VM".into(), detail, charts: vec![chart] });
    }
    if cpus.len() > 1 {
        let charts = cpus.iter().map(|(i, c)| Chart { top: Some(1.0), ..Chart::new(&format!("CPU {}", i), Unit::Share, vec![Line { name: format!("CPU {}", i), hue: Hue::Accent, vals: (*c).clone() }]) }).collect();
        groups.push(Group { key: "cpus".into(), kind: GroupKind::Cpus, name: "Each CPU".into(), detail: cpus_text(cpus.len()), charts });
    }
    if let Some(l) = get("loadavg") {
        let chart = Chart { wide: true, ..Chart::new("Load average", Unit::Plain, vec![Line { name: "load".into(), hue: Hue::Purple, vals: l }]) };
        groups.push(Group { key: "load".into(), kind: GroupKind::Load, name: "Server load".into(), detail: "Work waiting for its CPUs".into(), charts: vec![chart] });
    }
    for (dev, k) in &pifs {
        let nic = h.nics.iter().find(|x| x.device == *dev);
        let detail = nic.map_or(String::new(), |x| {
            let speed = match x.speed {
                s if s >= 1000 && s % 1000 == 0 => format!("{} Gb/s", s / 1000),
                s if s > 0 => format!("{} Mb/s", s),
                _ => String::new(),
            };
            [if x.management { "Management".to_string() } else { String::new() }, x.network.clone(), speed].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ")
        });
        let get = |w: &str| k.get(w).map_or(none.clone(), |v| (*v).clone());
        let chart = Chart { wide: true, ..Chart::new("Traffic", Unit::Rate, two("in", "out", get("rx"), get("tx"))) };
        groups.push(Group { key: format!("pif:{}", dev), kind: GroupKind::Card, name: dev.to_string(), detail, charts: vec![chart] });
    }
    let mut stores: Vec<(String, String, String, &BTreeMap<&str, &Vec<f64>>)> = srs
        .iter()
        .map(|(sr8, k)| {
            let x = d.srs.iter().find(|x| x.uuid.starts_with(sr8));
            let name = x.map_or(format!("Storage {}", sr8), |x| x.name.clone());
            let detail = x.filter(|x| x.size > 0).map_or(String::new(), |x| format!("{} of {}", fmt_bytes(x.used), fmt_bytes(x.size)));
            (name, detail, sr8.to_string(), k)
        })
        .collect();
    stores.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    for (name, detail, sr8, k) in stores {
        let charts = io_charts(k, n, "Wait for the disks");
        if !charts.is_empty() {
            groups.push(Group { key: format!("sr:{}", sr8), kind: GroupKind::Storage, name, detail, charts });
        }
    }
    Charts { main, groups }
}

/// A figure as people read it, in its unit.
pub fn fmt_value(unit: Unit, v: f64) -> String {
    let v = if v.is_finite() && v > 0.0 { v } else { 0.0 };
    match unit {
        Unit::Share => format!("{:.0}%", v * 100.0),
        Unit::Bytes => fmt_bytes(v as i64),
        Unit::Rate => match v {
            b if b < 1024.0 => format!("{:.0} B/s", b),
            b if b < 1024.0 * 1024.0 => format!("{:.0} KB/s", b / 1024.0),
            b => format!("{:.1} MB/s", b / 1024.0 / 1024.0),
        },
        Unit::PerSec => {
            if v < 10.0 {
                format!("{:.1}/s", v)
            } else {
                format!("{:.0}/s", v)
            }
        }
        Unit::Micros => match v / 1000.0 {
            ms if ms < 0.1 => format!("{:.0} µs", v),
            ms if ms < 10.0 => format!("{:.1} ms", ms),
            ms => format!("{:.0} ms", ms),
        },
        Unit::Plain => format!("{:.2}", v),
    }
}

/// A figure on a chart's scale: as `fmt_value`, without a tenth that's nothing ("2 MB/s").
pub fn fmt_axis(unit: Unit, v: f64) -> String {
    let s = match unit {
        Unit::Plain => format!("{:.1}", v),
        _ => fmt_value(unit, v),
    };
    s.replacen(".0 ", " ", 1).replacen(".0/", "/", 1).trim_end_matches(".0").to_string()
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
/// Day 0 (1 Jan 1970) was a Thursday.
const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];

/// A time `lt` (local, seconds) as "14:05".
fn clock(lt: i64) -> String {
    format!("{:02}:{:02}", lt.rem_euclid(86_400) / 3600, lt.rem_euclid(3600) / 60)
}

/// The day `t` falls on, locally: "3 Mar".
pub fn fmt_day(t: i64, offset: i64) -> String {
    let (_, m, d) = civil_from_days((t + offset).div_euclid(86_400));
    format!("{} {}", d, MONTHS[(m - 1) as usize])
}

/// A point's time, as precisely as its span needs: "14:05:35", "14:05", "Sat 14 Nov, 03:00",
/// "Sat 14 Mar 2026".
pub fn fmt_point(t: i64, span: Span, offset: i64) -> String {
    let lt = t + offset;
    let day = lt.div_euclid(86_400);
    let (y, m, d) = civil_from_days(day);
    let date = format!("{} {} {}", DAYS[day.rem_euclid(7) as usize], d, MONTHS[(m - 1) as usize]);
    match span {
        Span::Minutes => format!("{}:{:02}", clock(lt), lt.rem_euclid(60)),
        Span::Hours => clock(lt),
        Span::Week => format!("{}, {}", date, clock(lt)),
        Span::Year => format!("{} {}", date, y),
    }
}

/// Where the time axis is labelled: the first point of each new stretch (every 2 minutes, half
/// hour, day, or every other month from January), and its label: "14:06", "Thu 12", "Mar".
/// `offset` says the local time zone's offset at a time.
pub fn time_marks(times: &[i64], span: Span, offset: &dyn Fn(i64) -> i64) -> Vec<(usize, String)> {
    let stretch = |lt: i64| -> i64 {
        match span {
            Span::Minutes => lt.div_euclid(120),
            Span::Hours => lt.div_euclid(1800),
            Span::Week => lt.div_euclid(86_400),
            Span::Year => {
                let (y, m, _) = civil_from_days(lt.div_euclid(86_400));
                y * 12 + (m - 1)
            }
        }
    };
    let mut out = vec![];
    for i in 1..times.len() {
        let (a, b) = (times[i - 1] + offset(times[i - 1]), times[i] + offset(times[i]));
        let s = stretch(b);
        if s == stretch(a) {
            continue;
        }
        let label = match span {
            Span::Minutes => clock(s * 120),
            Span::Hours => clock(s * 1800),
            Span::Week => {
                let (_, _, d) = civil_from_days(s);
                format!("{} {}", DAYS[s.rem_euclid(7) as usize], d)
            }
            Span::Year => {
                if s % 2 != 0 {
                    continue; // every other month: Jan, Mar, May…
                }
                MONTHS[s.rem_euclid(12) as usize].to_string()
            }
        };
        out.push((i, label));
    }
    out
}

// ---- the demo server's ------------------------------------------------------------------------

/// A small random number maker (mulberry32): the demo's records come out the same every time.
struct Dice(u32);

impl Dice {
    fn roll(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x6D2B_79F5);
        let mut t = self.0;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        (t ^ (t >> 14)) as f64 / 4_294_967_296.0
    }
}

/// How a made-up figure goes: its floor, how much more it is by day (it peaks in the afternoon),
/// how much it wanders, a nightly backup's jump at 3 in the morning (seen by the week), and the
/// odd burst (seen by the minute).
#[derive(Clone, Copy, Default)]
struct Shape {
    base: f64,
    day: f64,
    noise: f64,
    spike: f64,
    burst: f64,
    burst_amp: f64,
    min: f64,
    max: f64,
    /// Kept since then (the VM was made then).
    since: i64,
}

/// The made-up figure `s` at each of `times`.
fn demo_line(times: &[i64], span: Span, seed: u32, s: Shape, offset: &dyn Fn(i64) -> i64) -> Vec<f64> {
    let mut dice = Dice(seed.wrapping_mul(2_654_435_761));
    let mut walk = 0.0;
    let max = if s.max > 0.0 { s.max } else { f64::INFINITY };
    times
        .iter()
        .map(|&t| {
            if t < s.since {
                dice.roll();
                return f64::NAN;
            }
            let lt = t + offset(t);
            let h = lt.rem_euclid(86_400) as f64 / 3600.0;
            let daytime = ((h - 7.0) / 16.0 * std::f64::consts::PI).sin().max(0.0);
            walk = walk * 0.8 + (dice.roll() - 0.5) * s.noise;
            let v = if span == Span::Year {
                // a day's average: less at the weekend, now and then a busier one
                let weekend = matches!(lt.div_euclid(86_400).rem_euclid(7), 2 | 3);
                let busy = if dice.roll() < 0.04 { s.day * 1.2 * dice.roll() } else { 0.0 };
                s.base + s.day * 0.5 * if weekend { 0.75 } else { 1.0 } + walk * 0.6 + s.spike * 0.04 + busy
            } else {
                let mut v = s.base + s.day * daytime + walk;
                if span == Span::Week && s.spike > 0.0 && (h - 3.0).abs() <= 0.5 {
                    v += s.spike * (0.75 + 0.25 * dice.roll());
                }
                if span != Span::Week && s.burst > 0.0 && dice.roll() < s.burst {
                    v += s.burst_amp * (0.4 + 0.6 * dice.roll());
                }
                v
            };
            v.clamp(s.min, max)
        })
        .collect()
}

/// The times the demo's records are at, up to `now`, as the server would answer.
fn demo_times(span: Span, now: i64) -> Vec<i64> {
    let end = now.div_euclid(span.step()) * span.step();
    let start = span.start(now);
    let mut out = vec![];
    let mut t = end;
    while t >= start {
        out.push(t);
        t -= span.step();
    }
    out.reverse();
    out
}

fn seed_of(name: &str) -> u32 {
    name.bytes().fold(7u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32))
}

/// A demo VM's made-up records: its CPUs, memory, disks and cards, kept since it was made (some
/// months ago).
pub fn demo_vm_records(v: &VmInfo, span: Span, now: i64, offset: &dyn Fn(i64) -> i64) -> Records {
    let times = demo_times(span, now);
    let mut out = Records { times: times.clone(), ..Default::default() };
    let seed = seed_of(&v.name);
    let since = v.created.unwrap_or(now - (120 + (seed % 400) as i64) * 86_400);
    let busy = 0.6 + (seed % 9) as f64 / 10.0;
    let line = |k: u32, s: Shape| demo_line(&times, span, seed.wrapping_add(k), Shape { since, ..s }, offset);
    let pre = format!("vm:{}:", v.uuid);
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    for c in 0..v.vcpus.max(1) as u32 {
        let s = Shape { base: 0.07 * busy, day: 0.12 * busy, noise: 0.14, spike: 0.6 + 0.08 * (c % 4) as f64, burst: 0.05, burst_amp: 0.3, max: 1.0, ..Default::default() };
        out.cols.insert(format!("{}cpu{}", pre, c), line(10 + c, s));
    }
    let mem = v.mem_dyn_max as f64;
    out.cols.insert(format!("{}memory", pre), times.iter().map(|t| if *t < since { f64::NAN } else { mem }).collect());
    if v.tools_installed {
        let used = line(3, Shape { base: 0.45 * mem, day: 0.1 * mem, noise: 0.03 * mem, spike: 0.25 * mem, max: mem, ..Default::default() });
        out.cols.insert(format!("{}memory_internal_free", pre), used.iter().map(|u| (mem - u) / 1024.0).collect());
    }
    for (i, d) in v.disks.iter().filter(|d| d.kind == "Disk").enumerate() {
        let (dev, k) = (disk_dev(d), 40 + 10 * i as u32);
        let read = line(k, Shape { base: 60.0 * KB, day: 500.0 * KB * busy, noise: 400.0 * KB, spike: 36.0 * MB, burst: 0.06, burst_amp: 1.5 * MB, ..Default::default() });
        let write = line(k + 1, Shape { base: 20.0 * KB, day: 150.0 * KB * busy, noise: 120.0 * KB, spike: 5.0 * MB, burst: 0.06, burst_amp: 500.0 * KB, ..Default::default() });
        let wait = line(k + 2, Shape { base: 700.0, day: 500.0, noise: 600.0, spike: 8000.0, burst: 0.04, burst_amp: 3000.0, min: 200.0, ..Default::default() });
        out.cols.insert(format!("{}vbd_{}_iops_read", pre, dev), read.iter().map(|x| x / (40.0 * KB) + 1.0).collect());
        out.cols.insert(format!("{}vbd_{}_iops_write", pre, dev), write.iter().map(|x| x / (16.0 * KB) + 1.0).collect());
        out.cols.insert(format!("{}vbd_{}_read", pre, dev), read);
        out.cols.insert(format!("{}vbd_{}_write", pre, dev), write);
        out.cols.insert(format!("{}vbd_{}_latency", pre, dev), wait);
    }
    for (i, nic) in v.nics.iter().enumerate() {
        let k = 70 + 10 * i as u32;
        out.cols.insert(format!("{}vif_{}_rx", pre, nic.device), line(k, Shape { base: 6.0 * KB, day: 40.0 * KB * busy, noise: 25.0 * KB, burst: 0.07, burst_amp: 150.0 * KB, ..Default::default() }));
        out.cols.insert(format!("{}vif_{}_tx", pre, nic.device), line(k + 1, Shape { base: 12.0 * KB, day: 160.0 * KB * busy, noise: 70.0 * KB, spike: 28.0 * MB, burst: 0.07, burst_amp: 400.0 * KB, ..Default::default() }));
    }
    out
}

/// A demo server's made-up records: its CPUs, memory, load, cards and storage, and the CPUs of
/// the VMs running on it.
pub fn demo_host_records(h: &HostInfo, d: &PoolData, span: Span, now: i64, offset: &dyn Fn(i64) -> i64) -> Records {
    let times = demo_times(span, now);
    let mut out = Records { times: times.clone(), ..Default::default() };
    let seed = seed_of(&h.name);
    let line = |k: u32, s: Shape| demo_line(&times, span, seed.wrapping_add(k), Shape { since: i64::MIN, ..s }, offset);
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    for c in 0..h.cpu_count.max(1) as u32 {
        let s = Shape { base: 0.04, day: 0.16 + 0.03 * (c % 4) as f64, noise: 0.16, spike: 0.35 + 0.09 * (c % 5) as f64, burst: 0.05, burst_amp: 0.4, max: 1.0, ..Default::default() };
        out.cols.insert(format!("host:cpu{}", c), line(100 + c, s));
    }
    let total = h.mem_total as f64 / 1024.0;
    let used = (h.mem_total - h.mem_free) as f64 / 1024.0;
    let used = line(2, Shape { base: used * 0.92, day: used * 0.08, noise: used * 0.04, spike: used * 0.15, max: total, ..Default::default() });
    out.cols.insert("host:memory_total_kib".into(), vec![total; times.len()]);
    out.cols.insert("host:memory_free_kib".into(), used.iter().map(|u| total - u).collect());
    out.cols.insert("host:loadavg".into(), line(3, Shape { base: 0.4, day: 1.6, noise: 0.8, spike: 5.0, burst: 0.05, burst_amp: 2.0, ..Default::default() }));
    for (i, nic) in h.nics.iter().enumerate() {
        let (k, big) = (20 + 10 * i as u32, if nic.management { 1.0 } else { 3.0 });
        out.cols.insert(format!("host:pif_{}_rx", nic.device), line(k, Shape { base: 10.0 * KB * big, day: 70.0 * KB * big, noise: 40.0 * KB * big, spike: 2.0 * MB, burst: 0.07, burst_amp: 200.0 * KB, ..Default::default() }));
        out.cols.insert(format!("host:pif_{}_tx", nic.device), line(k + 1, Shape { base: 20.0 * KB * big, day: 120.0 * KB * big, noise: 60.0 * KB * big, spike: 30.0 * MB, burst: 0.07, burst_amp: 300.0 * KB, ..Default::default() }));
    }
    let stores = d.srs.iter().filter(|x| !x.is_iso() && x.kind != "udev" && x.uuid.len() >= 8 && x.host.as_deref().map_or(true, |o| o == h.rref));
    for (i, x) in stores.enumerate() {
        let (sr8, k, big) = (&x.uuid[..8], 50 + 10 * i as u32, if x.shared { 1.0 } else { 0.4 });
        let read = line(k, Shape { base: 300.0 * KB * big, day: 1.2 * MB * big, noise: 900.0 * KB * big, spike: 60.0 * MB * big, burst: 0.06, burst_amp: 4.0 * MB, ..Default::default() });
        let write = line(k + 1, Shape { base: 200.0 * KB * big, day: 700.0 * KB * big, noise: 500.0 * KB * big, spike: 9.0 * MB * big, burst: 0.06, burst_amp: 2.0 * MB, ..Default::default() });
        out.cols.insert(format!("host:iops_read_{}", sr8), read.iter().map(|x| x / (32.0 * KB) + 2.0).collect());
        out.cols.insert(format!("host:iops_write_{}", sr8), write.iter().map(|x| x / (16.0 * KB) + 2.0).collect());
        out.cols.insert(format!("host:read_{}", sr8), read);
        out.cols.insert(format!("host:write_{}", sr8), write);
        out.cols.insert(format!("host:latency_{}", sr8), line(k + 2, Shape { base: 400.0 / big, day: 300.0, noise: 300.0, spike: 3000.0 / big, burst: 0.03, burst_amp: 2000.0, min: 100.0, ..Default::default() }));
    }
    for v in d.vms.iter().filter(|v| v.host.as_deref() == Some(h.rref.as_str()) && v.power == PowerState::Running) {
        let r = demo_vm_records(v, span, now, offset);
        let pre = format!("vm:{}:cpu", v.uuid);
        out.cols.extend(r.cols.into_iter().filter(|(k, _)| k.starts_with(&pre)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HostNic, SrInfo, VmNic};

    #[test]
    fn records_read_with_their_gaps() {
        let xml = "<xport><meta><start>0</start><step>3600</step><end>7200</end><rows>3</rows><columns>3</columns><legend>\
            <entry>AVERAGE:vm:u1:cpu0</entry><entry>AVERAGE:host:h1:loadavg</entry><entry>MIN:vm:u1:cpu0</entry></legend></meta>\
            <data><row><t>7200</t><v>0.5</v><v>1.0</v><v>0.1</v></row><row><t>3600</t><v>0.25</v><v>2.0</v><v>0.1</v></row>\
            <row><t>0</t><v>NaN</v><v>NaN</v><v>0.1</v></row></data></xport>";
        let r = read_records(xml);
        assert_eq!(r.times, vec![0, 3600, 7200]);
        assert_eq!(r.cols.len(), 2, "only the averages");
        let cpu = &r.cols["vm:u1:cpu0"];
        assert!(cpu[0].is_nan());
        assert_eq!(&cpu[1..], &[0.25, 0.5]);
        assert_eq!(r.cols["host:loadavg"][2], 1.0);
        assert_eq!(first_there(cpu), Some(1));
        assert_eq!(average(cpu), Some(0.375));
        assert_eq!(peak(cpu), Some(0.5));
        assert_eq!(last(&[1.0, f64::NAN]), Some(1.0));
    }

    #[test]
    fn vm_charts_by_disk_and_card() {
        let v = VmInfo {
            uuid: "u1".into(),
            vcpus: 2,
            mem_dyn_max: 4 << 30,
            disks: vec![VmDisk { device: "0".into(), dev: "xvda".into(), kind: "Disk".into(), size: 40 << 30, sr_name: "NFS".into(), ..Default::default() }],
            nics: vec![VmNic { device: "0".into(), network: "LAN".into(), ip: "10.0.0.5".into(), ..Default::default() }],
            ..Default::default()
        };
        let mut r = Records { times: vec![0, 5], ..Default::default() };
        for (k, a, b) in [
            ("cpu0", 0.2, 0.4),
            ("cpu1", 0.4, 0.6),
            ("memory", 4294967296.0, 4294967296.0),
            ("memory_internal_free", 2097152.0, 1048576.0),
            ("vbd_xvda_read", 100.0, 200.0),
            ("vbd_xvda_write", 10.0, 20.0),
            ("vbd_xvda_iops_read", 1.0, 2.0),
            ("vbd_xvda_latency", 500.0, 1500.0),
            ("vbd_xvda_io_throughput_read", 9.0, 9.0),
            ("vif_0_rx", 1.0, 2.0),
            ("vif_0_tx", 3.0, 4.0),
            ("vif_0_rx_errors", 0.0, 0.0),
        ] {
            r.cols.insert(format!("vm:u1:{}", k), vec![a, b]);
        }
        r.cols.insert("vm:other:cpu0".into(), vec![1.0, 1.0]);
        let c = vm_charts(&r, &v);
        let names: Vec<&str> = c.main.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(names, ["CPU", "Memory", "Disk", "Network"]);
        assert!((c.main[0].lines[0].vals[0] - 0.3).abs() < 1e-9, "its CPUs averaged");
        assert_eq!(c.main[0].of, "2 CPUs");
        assert_eq!(c.main[1].lines[0].vals, vec![2147483648.0, 3221225472.0], "all but what's free inside it");
        assert_eq!(c.main[2].lines[0].vals, vec![100.0, 200.0], "read, not the throughput in MiB/s");
        assert_eq!(c.main[3].lines[1].vals, vec![3.0, 4.0]);
        let groups: Vec<(&str, &str)> = c.groups.iter().map(|g| (g.name.as_str(), g.detail.as_str())).collect();
        assert_eq!(groups, [("Each CPU", "2 CPUs"), ("Disk 0", "40 GB · NFS"), ("Card 0", "LAN · 10.0.0.5")]);
        let disk: Vec<&str> = c.groups[1].charts.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(disk, ["Read and write", "Requests", "Wait for the disk"]);
        assert_eq!(c.groups[1].key, "disk:0");
    }

    #[test]
    fn host_charts_by_storage_and_card() {
        let h = HostInfo {
            uuid: "h1".into(),
            cpu_count: 2,
            mem_total: 8 << 30,
            nics: vec![HostNic { device: "eth0".into(), network: "LAN".into(), management: true, speed: 1000, ..Default::default() }],
            ..Default::default()
        };
        let d = PoolData {
            srs: vec![SrInfo { uuid: "1c2b3a4d-0000".into(), name: "Local storage".into(), size: 100 << 30, used: 25 << 30, ..Default::default() }],
            vms: vec![VmInfo { uuid: "u1".into(), name: "Web".into(), ..Default::default() }, VmInfo { uuid: "u2".into(), name: "Mail".into(), ..Default::default() }],
            ..Default::default()
        };
        let mut r = Records { times: vec![0, 5], ..Default::default() };
        for (k, a, b) in [
            ("host:cpu0", 0.1, 0.3),
            ("host:cpu1", 0.3, 0.5),
            ("host:memory_total_kib", 8388608.0, 8388608.0),
            ("host:memory_free_kib", 4194304.0, 4194304.0),
            ("host:loadavg", 0.5, 0.7),
            ("host:pif_eth0_rx", 10.0, 20.0),
            ("host:pif_eth0_tx", 30.0, 40.0),
            ("host:pif_lo_rx", 99.0, 99.0),
            ("host:read_1c2b3a4d", 1000.0, 2000.0),
            ("host:write_1c2b3a4d", 500.0, 500.0),
            ("host:read_latency_1c2b3a4d", 100.0, 300.0),
            ("host:write_latency_1c2b3a4d", 300.0, 500.0),
            ("vm:u1:cpu0", 0.1, 0.1),
            ("vm:u2:cpu0", 0.5, 0.7),
            ("vm:u2:cpu1", 0.3, 0.3),
        ] {
            r.cols.insert(k.into(), vec![a, b]);
        }
        let c = host_charts(&r, &h, &d);
        assert!((c.main[0].lines[0].vals[1] - 0.4).abs() < 1e-9);
        assert_eq!(c.main[1].lines[0].vals[0], 4294967296.0);
        assert_eq!(c.main[2].lines[0].vals, vec![1000.0, 2000.0], "read from its storage");
        assert_eq!(c.main[3].lines[0].vals, vec![10.0, 20.0], "its cards, not the loopback");
        let groups: Vec<(&str, &str)> = c.groups.iter().map(|g| (g.name.as_str(), g.detail.as_str())).collect();
        assert_eq!(groups, [("CPU of each VM", "Mail, Web"), ("Each CPU", "2 CPUs"), ("Server load", "Work waiting for its CPUs"), ("eth0", "Management · LAN · 1 Gb/s"), ("Local storage", "25 GB of 100 GB")]);
        let vms = &c.groups[0].charts[0].lines;
        assert_eq!(vms[0].name, "Mail", "the busiest first");
        assert!((vms[0].vals[1] - 0.5).abs() < 1e-9);
        let wait = &c.groups[4].charts.iter().find(|c| c.unit == Unit::Micros).unwrap().lines[0].vals;
        assert_eq!(wait, &vec![200.0, 400.0], "read and write waits averaged");
    }

    #[test]
    fn figures_and_marks() {
        assert_eq!(fmt_value(Unit::Share, 0.163), "16%");
        assert_eq!(fmt_value(Unit::Micros, 1500.0), "1.5 ms");
        assert_eq!(fmt_value(Unit::Micros, 40.0), "40 µs");
        assert_eq!(fmt_value(Unit::Rate, -0.0), "0 B/s");
        assert_eq!(fmt_axis(Unit::Rate, 2.0 * 1024.0 * 1024.0), "2 MB/s");
        assert_eq!(fmt_axis(Unit::Plain, 5.0), "5");
        assert_eq!(nice(42.0), 50.0);
        assert_eq!(nice(0.12), 0.2);
        let c = Chart::new("Disk", Unit::Rate, vec![Line { name: "read".into(), hue: Hue::Blue, vals: vec![f64::NAN, 3.0 * 1024.0 * 1024.0] }]);
        assert_eq!(c.scale(), 5.0 * 1024.0 * 1024.0);
        // 18 Nov 2026 (a Wednesday) 00:00 UTC, by the hour from the evening before
        let midnight = 1_794_960_000;
        let times: Vec<i64> = (0..4).map(|i| midnight - 7200 + i * 3600).collect();
        assert_eq!(time_marks(&times, Span::Week, &|_| 0), vec![(2, "Wed 18".to_string())]);
        assert_eq!(time_marks(&times, Span::Week, &|_| -3 * 3600), vec![], "still the 17th in Brazil");
        assert_eq!(fmt_point(midnight + 3 * 3600, Span::Week, 0), "Wed 18 Nov, 03:00");
        assert_eq!(fmt_point(midnight, Span::Year, 0), "Wed 18 Nov 2026");
        assert_eq!(fmt_day(midnight, 0), "18 Nov");
        // every other month, from January
        let days: Vec<i64> = (0..70).map(|i| 1_767_225_600 - 86_400 + i * 86_400).collect(); // 31 Dec 2025 on
        let marks: Vec<String> = time_marks(&days, Span::Year, &|_| 0).into_iter().map(|m| m.1).collect();
        assert_eq!(marks, ["Jan", "Mar"]);
    }

    #[test]
    fn demo_records_make_charts() {
        let now = 1_795_500_000;
        let v = VmInfo { uuid: "u1".into(), name: "Web Server".into(), created: Some(now - 200 * 86_400), vcpus: 4, mem_dyn_max: 4 << 30, tools_installed: true, disks: vec![VmDisk { device: "0".into(), kind: "Disk".into(), ..Default::default() }], nics: vec![VmNic { device: "0".into(), ..Default::default() }], ..Default::default() };
        for span in Span::ALL {
            let r = demo_vm_records(&v, span, now, &|_| 0);
            assert!(r.times.len() >= 118 && r.times.len() <= 366, "{:?}: {}", span, r.times.len());
            let c = vm_charts(&r, &v);
            assert_eq!(c.groups.len(), 3, "{:?}", span);
            assert!(peak(&c.main[0].lines[0].vals).unwrap() <= 1.0);
        }
        let year = vm_charts(&demo_vm_records(&v, Span::Year, now, &|_| 0), &v);
        assert!(first_there(&year.main[0].lines[0].vals).unwrap() > 0, "made some months ago");
    }
}
