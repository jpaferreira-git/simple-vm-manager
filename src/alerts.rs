//! Alerts: the messages a pool keeps for people (XAPI's `message` class: a VM that crashed, a
//! license running out, a certificate expiring, a limit passed…) said in plain words; and the
//! limits a pool watches by itself (a VM's, a server's or a storage's `other_config:perfmon`, read
//! by the perfmon daemon on each server, which raises an ALARM message when one is passed), with
//! the pool's email for them.

use crate::model::fmt_bytes;

/// One of a pool's messages, as its server keeps it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct XenMessage {
    pub rref: String,
    /// What it's about, like "VM_CRASHED", "LICENSE_EXPIRES_SOON" or "ALARM".
    pub name: String,
    /// 1 and 2: data or service about to be lost; 3: degraded; 4: recovered; 5: information.
    pub priority: i64,
    /// What kind of thing it's about ("VM", "Host", "SR", "Pool"…), and which (its uuid).
    pub cls: String,
    pub obj_uuid: String,
    /// When, in seconds since 1970.
    pub time: i64,
    pub body: String,
}

/// How much an alert matters.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Severity {
    Critical,
    Warning,
    Recovered,
    Info,
}

impl Severity {
    pub fn of(priority: i64) -> Severity {
        match priority {
            1 | 2 => Severity::Critical,
            3 => Severity::Warning,
            4 => Severity::Recovered,
            _ => Severity::Info,
        }
    }

    /// It needs someone to look at it (not news of something recovered or done).
    pub fn needs_attention(self) -> bool {
        matches!(self, Severity::Critical | Severity::Warning)
    }

    pub fn label(self) -> &'static str {
        match self {
            Severity::Critical => "Critical",
            Severity::Warning => "Warning",
            Severity::Recovered => "Recovered",
            Severity::Info => "Information",
        }
    }
}

/// A message in words: what happened to `obj` (the name of what it's about), and a softer line
/// with more.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Said {
    pub title: String,
    pub detail: String,
    /// It's about a limit someone set (an ALARM): its Performance tab shows it.
    pub limit: bool,
}

/// An ALARM's figure and the limit it passed: which variable, the value, its limit, for how
/// long (seconds), from its body ("value: 0.97\nconfig:\n<variable>…").
pub fn read_alarm(body: &str) -> Option<(String, f64, Option<f64>, Option<i64>)> {
    let value: f64 = body.split("value:").nth(1)?.split_whitespace().next()?.parse().ok()?;
    let config = body.split("config:").nth(1).unwrap_or("");
    let v = read_variables(config).into_iter().next()?;
    Some((v.name, value, v.level, v.period))
}

/// "3 minutes", "1 minute", "45 seconds".
fn period_text(secs: i64) -> String {
    match secs {
        s if s >= 60 && s % 60 == 0 => {
            let m = s / 60;
            format!("{} minute{}", m, if m == 1 { "" } else { "s" })
        }
        s => format!("{} seconds", s),
    }
}

fn pct(v: f64) -> String {
    format!("{:.0}%", v * 100.0)
}

fn rate(b: f64) -> String {
    match b {
        b if b < 1024.0 * 1024.0 => format!("{:.0} KB/s", b / 1024.0),
        b => format!("{:.1} MB/s", b / 1024.0 / 1024.0).replace(".0 MB", " MB"),
    }
}

/// Days in a message name's end: "…_EXPIRING_07" → 7.
fn days_in(name: &str) -> Option<i64> {
    name.rsplit('_').next()?.parse().ok()
}

/// Says message `m`, about `obj` (its name; empty when it's gone).
pub fn describe(m: &XenMessage, obj: &str) -> Said {
    let it = if obj.is_empty() { "Something".to_string() } else { obj.to_string() };
    let its = format!("{}'s", it);
    let said = |title: String, detail: &str| Said { title, detail: detail.to_string(), limit: false };
    let name = m.name.as_str();
    if name == "ALARM" {
        let Some((var, value, level, period)) = read_alarm(&m.body) else {
            return Said { title: format!("{} passed a limit set for it", it), detail: String::new(), limit: true };
        };
        let lasting = period.map(|p| format!(" for {}", period_text(p))).unwrap_or_default();
        let limit = |text: String| format!("Your limit: {}{}", text, lasting);
        let (title, detail) = match var.as_str() {
            "cpu_usage" => (format!("{} CPU was above {}", its, level.map(pct).unwrap_or_default()), format!("{} · {}", pct(value), limit(level.map(pct).unwrap_or_default()))),
            "network_usage" => (format!("{} network was above {}", its, level.map(rate).unwrap_or_default()), format!("{} · {}", rate(value), limit(level.map(rate).unwrap_or_default()))),
            "disk_usage" => (format!("{} disks were above {}", its, level.map(rate).unwrap_or_default()), format!("{} · {}", rate(value), limit(level.map(rate).unwrap_or_default()))),
            "memory_free_kib" | "memory_internal_free" => {
                let gb = |k: f64| fmt_bytes((k * 1024.0) as i64);
                (format!("{} free memory went below {}", its, level.map(gb).unwrap_or_default()), format!("{} free · {}", gb(value), limit(level.map(gb).unwrap_or_default())))
            }
            "physical_utilisation" => (format!("{} is {} full", it, pct(value)), limit(format!("fuller than {}", level.map(pct).unwrap_or_default()))),
            "fs_usage" => (format!("{} system disk is {} full", its, pct(value)), "The disk its control domain runs from: the server can stop working when it fills up".to_string()),
            "log_fs_usage" => (format!("{} log disk is {} full", its, pct(value)), "Where the server keeps its logs".to_string()),
            "mem_usage" => (format!("{} control domain uses {} of its memory", its, pct(value)), String::new()),
            v if v.starts_with("sr_io_throughput_total") => (format!("{} storage traffic was above {} MB/s", its, level.map(|l| format!("{:.0}", l)).unwrap_or_default()), format!("{:.1} MB/s", value)),
            v => (format!("{}: {} passed its limit", it, v), format!("{}", value)),
        };
        return Said { title, detail, limit: true };
    }
    match name {
        "VM_CRASHED" => said(format!("{} crashed", it), "Its system stopped by itself; the pool restarts it if set to"),
        "VM_STARTED" => said(format!("{} started", it), ""),
        "VM_SHUTDOWN" => said(format!("{} shut down", it), ""),
        "VM_REBOOTED" => said(format!("{} restarted", it), ""),
        "VM_SUSPENDED" => said(format!("{} was suspended", it), ""),
        "VM_RESUMED" => said(format!("{} was resumed", it), ""),
        "VM_CLONED" => said(format!("{} was copied", it), ""),
        "VM_SNAPSHOTTED" => said(format!("A snapshot of {} was taken", it), ""),
        "VM_CHECKPOINTED" => said(format!("A snapshot of {} was taken with its memory", it), ""),
        "VM_SNAPSHOT_REVERTED" => said(format!("{} was put back to a snapshot", it), ""),
        "HA_HOST_FAILED" => said(format!("{} failed", it), "High availability noticed it stopped"),
        "HA_HOST_WAS_FENCED" => said(format!("{} was restarted by high availability", it), "It lost touch with the pool"),
        "HA_PROTECTED_VM_RESTART_FAILED" => said(format!("{} couldn't be started again", it), "High availability tried and failed"),
        "HA_POOL_OVERCOMMITTED" => said(format!("{} can't keep its VMs safe if a server fails", it), "Not enough room left for high availability"),
        "HA_STATEFILE_LOST" => said(format!("{} lost its high availability storage", its), ""),
        "LICENSE_EXPIRES_SOON" => said(format!("{} license runs out soon", its), ""),
        "LICENSE_EXPIRED" => said(format!("{} license has run out", its), ""),
        "GRACE_LICENSE" => said(format!("{} is on a grace license", it), "Its license server can't be reached"),
        "LICENSE_SERVER_UNREACHABLE" | "LICENSE_SERVER_UNAVAILABLE" => said("The license server can't be reached".to_string(), ""),
        "PBD_PLUG_FAILED_ON_SERVER_START" => said(format!("{} couldn't mount a storage when it started", it), ""),
        "HOST_CLOCK_SKEW_DETECTED" => said(format!("{} clock differs from the pool coordinator's", its), "Set both to the same time server (NTP)"),
        "HOST_CLOCK_WENT_BACKWARDS" => said(format!("{} clock went backwards", its), ""),
        "FAILED_LOGIN_ATTEMPTS" => said(format!("Failed logins on {}", it), ""),
        "POOL_MASTER_TRANSITION" => said(format!("{} is the pool coordinator now", it), ""),
        "MULTIPATH_PERIODIC_ALERT" => said(format!("{} lost some paths to its storage", it), ""),
        "UPDATES_FEATURE_EXPIRED" => said(format!("{} can't get updates any more", it), "Its subscription for updates ran out"),
        n if n.starts_with("HOST_SERVER_CERTIFICATE_EXPIRING") || n.starts_with("HOST_INTERNAL_CERTIFICATE_EXPIRING") || n.starts_with("POOL_CA_CERTIFICATE_EXPIRING") => {
            let what = if n.starts_with("POOL_CA") {
                "A certificate the pool trusts"
            } else if n.starts_with("HOST_INTERNAL") {
                "The one its pool's servers use among themselves: the pool renews it"
            } else {
                "The one it answers HTTPS with. A new one: xe host-refresh-server-certificate on it"
            };
            match days_in(n) {
                Some(d) => said(format!("{} certificate expires in {} days", its, d), what),
                None => said(format!("{} certificate expires soon", its), what),
            }
        }
        n if n.ends_with("CERTIFICATE_EXPIRED") => said(format!("{} certificate has expired", its), ""),
        n if n.starts_with("UPDATES_FEATURE_EXPIRING") => match days_in(n) {
            Some(d) => said(format!("{} can get updates for {} more days", it, d), "Its subscription for updates runs out"),
            None => said(format!("{} subscription for updates runs out soon", its), ""),
        },
        n => {
            // a name as words: "HOST_SYNC_DATA_FAILED" → "Host sync data failed"
            let words = n.to_lowercase().replace('_', " ");
            let mut c = words.chars();
            let words = c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default();
            if obj.is_empty() {
                said(words, "")
            } else {
                said(format!("{}: {}", it, words.to_lowercase()), "")
            }
        }
    }
}

// ---- limits ---------------------------------------------------------------------------------

/// What the Alerts dialog edits: an object's other_config:perfmon, and its pool's email for
/// alerts (where to, and through which mail server).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AlertSettings {
    pub perfmon: String,
    pub mail_to: String,
    pub mail_via: String,
}

/// One variable of an other_config:perfmon: its name, limit, and how long it must last (seconds),
/// with the rest of its XML as it was (kept as is when it's saved again).
#[derive(Clone, Debug, PartialEq)]
pub struct Variable {
    pub name: String,
    pub level: Option<f64>,
    pub period: Option<i64>,
    pub xml: String,
}

/// An attribute's value in a tag like `<name value="cpu_usage"/>`.
fn tag_value(xml: &str, tag: &str) -> Option<String> {
    let at = xml.find(&format!("<{}", tag))?;
    let rest = &xml[at..];
    let end = rest.find('>')?;
    let tag_text = &rest[..end];
    let v = tag_text.split("value=").nth(1)?;
    let q = v.chars().next()?;
    if q != '"' && q != '\'' {
        return None;
    }
    Some(v[1..].split(q).next()?.to_string())
}

/// The variables of an other_config:perfmon (`<config><variable>…</variable>…</config>`).
pub fn read_variables(xml: &str) -> Vec<Variable> {
    let mut out = vec![];
    let mut rest = xml;
    while let Some(i) = rest.find("<variable>") {
        let after = &rest[i..];
        let Some(j) = after.find("</variable>") else { break };
        let block = &after[..j + "</variable>".len()];
        if let Some(name) = tag_value(block, "name ") {
            out.push(Variable {
                name,
                level: tag_value(block, "alarm_trigger_level").and_then(|v| v.trim().parse().ok()),
                period: tag_value(block, "alarm_trigger_period").and_then(|v| v.trim().parse().ok()),
                xml: block.to_string(),
            });
        }
        rest = &after[j + "</variable>".len()..];
    }
    out
}

/// What can be watched: a VM's, a server's or a storage's limits, each in words around its two
/// figures ("CPU is above … % for … minutes").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Watched {
    Vm,
    Host,
    Sr,
}

/// A limit the dialog offers: its variable, words before and after its figure, the figure's unit
/// as typed (and how many of the variable's units make one), its usual figure, and whether it
/// fires when below rather than above.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RuleDef {
    pub var: &'static str,
    pub before: &'static str,
    pub unit: &'static str,
    pub per_unit: f64,
    pub usual: f64,
    pub low: bool,
}

const MB: f64 = 1024.0 * 1024.0;

pub fn rule_defs(w: Watched) -> Vec<RuleDef> {
    let cpu = RuleDef { var: "cpu_usage", before: "CPU is above", unit: "%", per_unit: 0.01, usual: 90.0, low: false };
    let net = RuleDef { var: "network_usage", before: "Network is above", unit: "MB/s", per_unit: MB, usual: 50.0, low: false };
    match w {
        Watched::Vm => vec![cpu, net, RuleDef { var: "disk_usage", before: "Disks are above", unit: "MB/s", per_unit: MB, usual: 50.0, low: false }],
        // (free memory in KiB)
        Watched::Host => vec![cpu, net, RuleDef { var: "memory_free_kib", before: "Free memory is below", unit: "GB", per_unit: 1024.0 * 1024.0, usual: 1.0, low: true }],
        Watched::Sr => vec![RuleDef { var: "physical_utilisation", before: "It's fuller than", unit: "%", per_unit: 0.01, usual: 90.0, low: false }],
    }
}

/// A limit as the dialog edits it: on or off, its figure and minutes as typed.
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub def: RuleDef,
    pub on: bool,
    pub figure: String,
    pub minutes: String,
}

/// A figure for typing: "90", "1.5".
fn typed(v: f64) -> String {
    let s = format!("{:.2}", v);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The limits of an other_config:perfmon, as the dialog shows them (off, at their usual figures,
/// when not set).
pub fn rules_of(w: Watched, perfmon: &str) -> Vec<Rule> {
    let vars = read_variables(perfmon);
    rule_defs(w)
        .into_iter()
        .map(|def| match vars.iter().find(|v| v.name == def.var) {
            Some(v) => Rule {
                def,
                on: true,
                figure: typed(v.level.map_or(def.usual, |l| l / def.per_unit)),
                minutes: typed(v.period.unwrap_or(60) as f64 / 60.0),
            },
            None => Rule { def, on: false, figure: typed(def.usual), minutes: "5".into() },
        })
        .collect()
}

/// Why a limit as typed can't be saved, if it can't.
pub fn rule_problem(r: &Rule) -> Option<String> {
    if !r.on {
        return None;
    }
    let figure: Option<f64> = r.figure.trim().replace(',', ".").parse().ok().filter(|f: &f64| f.is_finite() && *f > 0.0);
    let minutes: Option<f64> = r.minutes.trim().replace(',', ".").parse().ok().filter(|m: &f64| m.is_finite() && *m >= 1.0 && *m <= 1440.0);
    match (figure, minutes) {
        (None, _) => Some(format!("{}: type a figure above 0", r.def.before)),
        (Some(f), _) if r.def.unit == "%" && f > 100.0 => Some(format!("{}: type a percentage up to 100", r.def.before)),
        (_, None) => Some(format!("{}: type the minutes, 1 to 1440", r.def.before)),
        _ => None,
    }
}

/// An other_config:perfmon with `rules` (the ones on) in place of the same variables in `old`,
/// and the variables this app doesn't offer kept as they were. Empty when nothing's left.
pub fn perfmon_with(old: &str, rules: &[Rule]) -> String {
    let ours: Vec<&str> = rules.iter().map(|r| r.def.var).collect();
    let mut vars: Vec<String> = read_variables(old).into_iter().filter(|v| !ours.contains(&v.name.as_str())).map(|v| v.xml).collect();
    for r in rules.iter().filter(|r| r.on) {
        let figure: f64 = r.figure.trim().replace(',', ".").parse().unwrap_or(r.def.usual);
        let minutes: f64 = r.minutes.trim().replace(',', ".").parse().unwrap_or(5.0);
        let level = figure * r.def.per_unit;
        let mut v = format!(
            "<variable><name value=\"{}\"/><alarm_trigger_level value=\"{}\"/><alarm_trigger_period value=\"{}\"/><alarm_auto_inhibit_period value=\"3600\"/>",
            r.def.var,
            typed_level(level),
            (minutes * 60.0).round() as i64
        );
        if r.def.low {
            v.push_str("<alarm_trigger_sense value=\"low\"/>");
        }
        v.push_str("</variable>");
        vars.push(v);
    }
    if vars.is_empty() {
        String::new()
    } else {
        format!("<config>{}</config>", vars.concat())
    }
}

/// A limit as the server reads it: a fraction to 4 places, a count whole.
fn typed_level(v: f64) -> String {
    if v < 10.0 {
        let s = format!("{:.4}", v);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        format!("{:.0}", v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alarms_and_messages_in_words() {
        let body = "value: 0.973000\nconfig:\n<variable>\n\t<name value=\"cpu_usage\"/>\n\t<alarm_trigger_level value=\"0.9\"/>\n\t<alarm_trigger_period value=\"300\"/>\n</variable>\n";
        let m = XenMessage { name: "ALARM".into(), priority: 3, body: body.into(), ..Default::default() };
        let s = describe(&m, "Web Server");
        assert_eq!(s.title, "Web Server's CPU was above 90%");
        assert_eq!(s.detail, "97% · Your limit: 90% for 5 minutes");
        assert!(s.limit);
        let m = XenMessage { name: "HOST_SERVER_CERTIFICATE_EXPIRING_07".into(), priority: 2, ..Default::default() };
        assert_eq!(describe(&m, "tico").title, "tico's certificate expires in 7 days");
        let m = XenMessage { name: "HOST_SYNC_DATA_FAILED".into(), ..Default::default() };
        assert_eq!(describe(&m, "teco").title, "teco: host sync data failed");
        assert_eq!(describe(&m, "").title, "Host sync data failed");
        assert_eq!(Severity::of(2), Severity::Critical);
        assert!(!Severity::of(4).needs_attention());
        let sr = "value: 0.96\nconfig:\n<variable><name value='physical_utilisation'/><alarm_trigger_level value='0.9'/></variable>";
        let m = XenMessage { name: "ALARM".into(), body: sr.into(), ..Default::default() };
        assert_eq!(describe(&m, "Local storage").title, "Local storage is 96% full");
    }

    #[test]
    fn limits_read_and_written() {
        let old = "<config><variable><name value=\"cpu_usage\"/><alarm_trigger_level value=\"0.8\"/><alarm_trigger_period value=\"120\"/></variable><variable><name value=\"fs_usage\"/><alarm_trigger_level value=\"0.9\"/></variable></config>";
        let mut rules = rules_of(Watched::Vm, old);
        assert_eq!((rules[0].on, rules[0].figure.as_str(), rules[0].minutes.as_str()), (true, "80", "2"));
        assert_eq!((rules[1].on, rules[1].figure.as_str()), (false, "50"));
        rules[0].on = false;
        rules[1].on = true;
        rules[1].figure = "12,5".into();
        let new = perfmon_with(old, &rules);
        let vars = read_variables(&new);
        let names: Vec<&str> = vars.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["fs_usage", "network_usage"], "the others kept, CPU's taken out");
        assert_eq!(vars[1].level, Some(12.5 * 1024.0 * 1024.0));
        assert_eq!(vars[1].period, Some(300));
        let host = rules_of(Watched::Host, "");
        let mut mem = host[2].clone();
        mem.on = true;
        let xml = perfmon_with("", &[mem]);
        assert!(xml.contains("memory_free_kib") && xml.contains("value=\"1048576\"") && xml.contains("sense value=\"low\""), "{}", xml);
        assert_eq!(perfmon_with(old, &rules_of(Watched::Sr, "")), old, "nothing of ours on: the rest as it was");
        assert_eq!(perfmon_with("", &rules_of(Watched::Vm, "")), "");
        let mut bad = rules_of(Watched::Vm, "")[0].clone();
        bad.on = true;
        bad.figure = "120".into();
        assert!(rule_problem(&bad).is_some());
        bad.figure = "95".into();
        bad.minutes = "0".into();
        assert!(rule_problem(&bad).is_some());
    }
}
