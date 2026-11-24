//! The Helper VM (helper-vm/ in this repository): a small Linux VM in the pool that looks after it
//! through the pool's own API, also while this app is closed. It sends the pool's alerts to phones
//! (Telegram, ntfy, Discord, Home Assistant, any webhook), watches any storage's fullness, makes up
//! missed snapshot schedule runs, mounts the NAS again after a server started before it, starts
//! VMs in order when the pool starts, and checks the pool against good practice for the Advisor
//! page. Nothing of it is on the servers.
//!
//! It and this app talk through the pool's other_config:
//! - "svm-helper": its settings, set in a pool's Helper tab (JSON; `Settings`);
//! - "svm-helper-vm": its heartbeat, every five minutes (`VmStatus`);
//! - "svm-helper-advice": the Advisor's checklist (`Advice`);
//! - "svm-helper-request": what this app asks of it, {"advice": when, "test": when};
//! - "svm-helper-notes": what the owner said about the Advisor's checks (`Notes`): those dismissed,
//!   and what they explained to Claude.

use serde_json::{json, Map, Value};

/// The pool's other_config key its settings are under.
pub const KEY: &str = "svm-helper";
/// Its heartbeat's.
pub const VM_KEY: &str = "svm-helper-vm";
/// The Advisor's checklist's.
pub const ADVICE_KEY: &str = "svm-helper-advice";
/// What this app asks of it.
pub const REQUEST_KEY: &str = "svm-helper-request";
/// What the owner said about the Advisor's checks.
pub const NOTES_KEY: &str = "svm-helper-notes";

/// The Helper VM's version this app's own version comes with (helper-vm/svm-helper.py's VERSION, at
/// this app's tag): an older one is offered an update to it.
pub const LATEST: &str = "2.4.0";

/// Whether version `a` ("2.0.3") is older than `b`.
pub fn older(a: &str, b: &str) -> bool {
    let parts = |v: &str| v.split('.').map(|p| p.trim().parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    parts(a) < parts(b)
}

/// Where to read about it, and how it's set up.
pub fn about_url() -> String {
    format!("https://github.com/{}/tree/main/helper-vm#readme", crate::update::REPO)
}

/// The commands that set it up, in a Linux VM's terminal (Debian, Ubuntu, Fedora, Rocky, Arch).
pub fn setup_commands() -> [String; 2] {
    [format!("wget -O setup.sh https://raw.githubusercontent.com/{}/main/helper-vm/setup.sh", crate::update::REPO), "sudo bash setup.sh".to_string()]
}

/// Where alerts can go: on or off, its address (or Telegram's bot token), and a second field
/// (Telegram's chat, ntfy's access token).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Channel {
    pub on: bool,
    pub url: String,
    pub extra: String,
}

/// The helper's settings.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub telegram: Channel,
    pub ntfy: Channel,
    pub discord: Channel,
    pub homeassistant: Channel,
    pub webhook: Channel,
    /// Alerts this bad or worse are sent: 2 critical and major, 3 warnings too, 5 everything.
    pub min_priority: i64,
    /// Quiet from, till (HH:MM): only critical alerts then.
    pub quiet: (bool, String, String),
    /// Storage fuller than this (%) raises an alert (a limit set on one storage wins).
    pub storage: (bool, i64),
    /// A snapshot schedule's missed run is made up.
    pub schedules: bool,
    /// Shared storage is mounted again on a server that started before the NAS.
    pub remount: bool,
    /// VMs started in order once the pool starts: on, and each (its uuid, seconds to wait after).
    pub order_on: bool,
    pub order: Vec<(String, i64)>,
    /// What else the settings hold (kept as it is).
    rest: Map<String, Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            telegram: Channel::default(),
            ntfy: Channel::default(),
            discord: Channel::default(),
            homeassistant: Channel::default(),
            webhook: Channel::default(),
            min_priority: 3,
            quiet: (false, "23:00".into(), "07:00".into()),
            storage: (true, 90),
            schedules: true,
            remount: true,
            order_on: false,
            order: vec![],
            rest: Map::new(),
        }
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string()
}

fn b(v: &Value, k: &str, default: bool) -> bool {
    v.get(k).and_then(|x| x.as_bool()).unwrap_or(default)
}

fn n(v: &Value, k: &str, default: i64) -> i64 {
    v.get(k).and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|t| t.trim().parse().ok()))).unwrap_or(default)
}

impl Settings {
    /// From the pool's other_config value ("" or unreadable: the defaults).
    pub fn parse(text: &str) -> Settings {
        let mut out = Settings::default();
        let Ok(Value::Object(all)) = serde_json::from_str::<Value>(text) else { return out };
        let v = Value::Object(all.clone());
        let ch = v.get("channels").cloned().unwrap_or(Value::Null);
        let channel = |name: &str, url: &str, extra: &str| {
            let c = ch.get(name).cloned().unwrap_or(Value::Null);
            Channel { on: b(&c, "on", false), url: s(&c, url), extra: if extra.is_empty() { String::new() } else { s(&c, extra) } }
        };
        out.telegram = channel("telegram", "token", "chat");
        out.ntfy = channel("ntfy", "url", "token");
        out.discord = channel("discord", "url", "");
        out.homeassistant = channel("homeassistant", "url", "");
        out.webhook = channel("webhook", "url", "");
        out.min_priority = n(&v, "min_priority", 3).clamp(1, 5);
        if let Some(q) = v.get("quiet") {
            out.quiet = (b(q, "on", false), or(s(q, "from"), "23:00"), or(s(q, "to"), "07:00"));
        }
        let watch = v.get("watch").cloned().unwrap_or(Value::Null);
        if let Some(st) = watch.get("storage") {
            out.storage = (b(st, "on", true), n(st, "level", 90).clamp(1, 100));
        }
        out.schedules = b(&watch, "schedules", true);
        out.remount = b(&v, "remount", true);
        if let Some(so) = v.get("start_order") {
            out.order_on = b(so, "on", false);
            out.order = so.get("vms").and_then(|x| x.as_array()).map(|a| a.iter().map(|i| (s(i, "uuid"), n(i, "wait", 30))).filter(|i| !i.0.is_empty()).collect()).unwrap_or_default();
        }
        out.rest = all.into_iter().filter(|(k, _)| !["channels", "min_priority", "quiet", "watch", "remount", "start_order"].contains(&k.as_str())).collect();
        out
    }

    /// As the pool keeps it.
    pub fn to_json(&self) -> String {
        let mut m = self.rest.clone();
        let c = |x: &Channel, url: &str, extra: &str| {
            let mut o = json!({ "on": x.on, url: x.url.trim() });
            if !extra.is_empty() {
                o[extra] = json!(x.extra.trim());
            }
            o
        };
        m.insert(
            "channels".into(),
            json!({
                "telegram": c(&self.telegram, "token", "chat"),
                "ntfy": c(&self.ntfy, "url", "token"),
                "discord": c(&self.discord, "url", ""),
                "homeassistant": c(&self.homeassistant, "url", ""),
                "webhook": c(&self.webhook, "url", ""),
            }),
        );
        m.insert("min_priority".into(), json!(self.min_priority));
        m.insert("quiet".into(), json!({ "on": self.quiet.0, "from": self.quiet.1.trim(), "to": self.quiet.2.trim() }));
        m.insert("watch".into(), json!({ "storage": { "on": self.storage.0, "level": self.storage.1 }, "schedules": self.schedules }));
        m.insert("remount".into(), json!(self.remount));
        m.insert("start_order".into(), json!({ "on": self.order_on, "vms": self.order.iter().map(|(u, w)| json!({ "uuid": u, "wait": w })).collect::<Vec<_>>() }));
        Value::Object(m).to_string()
    }

    /// What's wrong with them as typed, if anything.
    pub fn problem(&self) -> Option<String> {
        let url = |c: &Channel, name: &str| -> Option<String> {
            (c.on && !(c.url.trim().starts_with("https://") || c.url.trim().starts_with("http://"))).then(|| format!("{}: type its address (https://…)", name))
        };
        if self.telegram.on && (self.telegram.url.trim().is_empty() || self.telegram.extra.trim().is_empty()) {
            return Some("Telegram: type the bot's token and the chat's id".into());
        }
        if let Some(p) = url(&self.ntfy, "ntfy").or_else(|| url(&self.discord, "Discord")).or_else(|| url(&self.homeassistant, "Home Assistant")).or_else(|| url(&self.webhook, "Webhook")) {
            return Some(p);
        }
        let hhmm = |t: &str| {
            let mut p = t.trim().split(':');
            matches!((p.next().and_then(|h| h.parse::<u32>().ok()), p.next().and_then(|m| m.parse::<u32>().ok()), p.next()), (Some(h), Some(m), None) if h < 24 && m < 60)
        };
        if self.quiet.0 && !(hhmm(&self.quiet.1) && hhmm(&self.quiet.2)) {
            return Some("Quiet hours: type them as 23:00 and 07:00".into());
        }
        None
    }

    /// Whether any channel is on.
    pub fn sends(&self) -> bool {
        [&self.telegram, &self.ntfy, &self.discord, &self.homeassistant, &self.webhook].iter().any(|c| c.on)
    }
}

fn or(t: String, default: &str) -> String {
    if t.trim().is_empty() {
        default.to_string()
    } else {
        t
    }
}

fn i(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64))).unwrap_or(0)
}

/// The Helper VM, from its heartbeat.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VmStatus {
    pub version: String,
    /// Its machine's name, and its address.
    pub machine: String,
    pub address: String,
    /// When it last looked at the pool (seconds since 1970), and since when it runs.
    pub last_look: i64,
    pub since: i64,
    /// Claude works for it (alerts explained, the review and the Advisor in words).
    pub claude: bool,
    /// The weekly review: on, the last one.
    pub review: (bool, i64),
    /// The Advisor: on, the last check, the request it answered, how many to fix and to look at.
    pub advice: (bool, i64, i64, i64, i64),
    /// How many services it watches.
    pub watching: i64,
    /// It answers this app's requests (Check now, Send a test): 2.2 and later.
    pub requests: bool,
    /// It updates itself when asked (2.3 and later, set up by its setup.sh).
    pub updates: bool,
    /// Its last update, as asked: (the version asked, when, fine, why not).
    pub update: Option<(String, i64, bool, String)>,
    /// The last test alert sent as asked: the request it answered, and what each channel said.
    pub test: Option<(i64, Vec<(String, String)>)>,
}

impl VmStatus {
    pub fn parse(text: &str) -> Option<VmStatus> {
        let j: Value = serde_json::from_str(text).ok()?;
        let rv = j.get("review").cloned().unwrap_or(Value::Null);
        let ad = j.get("advice").cloned().unwrap_or(Value::Null);
        let test = j.get("test").map(|t| (i(t, "request"), t.get("results").and_then(|x| x.as_object()).map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("?").to_string())).collect()).unwrap_or_default()));
        Some(VmStatus {
            version: s(&j, "version"),
            machine: s(&j, "machine"),
            address: s(&j, "address"),
            last_look: i(&j, "last_look"),
            since: i(&j, "since"),
            claude: b(&j, "claude", false),
            review: (b(&rv, "on", false), i(&rv, "last")),
            advice: (b(&ad, "on", false), i(&ad, "last"), i(&ad, "request"), i(&ad, "bad"), i(&ad, "warn")),
            watching: i(&j, "watching"),
            requests: i(&j, "requests") >= 1,
            updates: i(&j, "updates") >= 1,
            update: j.get("update").filter(|u| u.is_object()).map(|u| (s(u, "ref"), i(u, "at"), b(u, "ok", false), s(u, "error"))),
            test,
        })
    }

    /// Heard from lately (it looks every minute, and says so every five).
    pub fn fresh(&self, now: i64) -> bool {
        now - self.last_look < 15 * 60
    }
}

/// How a check went.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Bad,
    Warn,
    Ok,
}

/// One of the Advisor's checks.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub id: String,
    pub topic: String,
    pub status: Status,
    pub title: String,
    /// Why it matters and what to do (none when it's fine).
    pub note: String,
    /// What it's about: servers, VMs, storage.
    pub objects: Vec<String>,
    /// A finding of Claude's own, not one of the helper's checks.
    pub by_claude: bool,
    /// What this app can do about it, when it can.
    pub fix: Option<Fix>,
}

/// What can be done about a check from this app (see advisor_page.rs): its action, the objects'
/// uuids, and their names as the check gives them.
#[derive(Clone, Debug, PartialEq)]
pub struct Fix {
    pub action: String,
    pub uuids: Vec<String>,
    pub names: Vec<String>,
}

/// The Advisor's checklist, as the Helper VM left it in the pool.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Advice {
    /// When it was made (seconds since 1970), and the request it answered (0: its daily one).
    pub at: i64,
    pub request: i64,
    /// Worded by Claude (else the helper's own plain words).
    pub by_claude: bool,
    pub checks: Vec<Check>,
}

/// The Advisor's topics, in order: (key, name).
pub const TOPICS: [(&str, &str); 5] = [("backups", "Backups"), ("storage", "Storage"), ("servers", "Servers"), ("vms", "VMs"), ("security", "Security")];

impl Advice {
    pub fn parse(text: &str) -> Option<Advice> {
        let j: Value = serde_json::from_str(text).ok()?;
        let checks = j
            .get("checks")?
            .as_array()?
            .iter()
            .filter_map(|c| {
                let status = match c.get("status")?.as_str()? {
                    "ok" => Status::Ok,
                    "warn" => Status::Warn,
                    "bad" => Status::Bad,
                    _ => return None,
                };
                let title = s(c, "title");
                (!title.is_empty()).then(|| Check {
                    id: s(c, "id"),
                    topic: s(c, "topic"),
                    status,
                    title,
                    note: s(c, "note"),
                    objects: c.get("objects").and_then(|o| o.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
                    by_claude: s(c, "by") == "claude",
                    fix: c.get("fix").filter(|f| f.is_object()).map(|f| {
                        let list = |k: &str| f.get(k).and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
                        Fix { action: s(f, "action"), uuids: list("uuids"), names: list("names") }
                    }),
                })
            })
            .collect();
        Some(Advice { at: i(&j, "at"), request: i(&j, "request"), by_claude: s(&j, "by") == "claude", checks })
    }

    /// How many to fix, and to look at.
    pub fn counts(&self) -> (usize, usize) {
        (self.checks.iter().filter(|c| c.status == Status::Bad).count(), self.checks.iter().filter(|c| c.status == Status::Warn).count())
    }

    /// Without the checks the owner dismissed: those shown, and those dismissed.
    pub fn split(&self, notes: &Notes) -> (Advice, Vec<Check>) {
        let (gone, kept): (Vec<Check>, Vec<Check>) = self.checks.iter().cloned().partition(|c| c.status != Status::Ok && notes.dismissed.contains_key(&c.key()));
        (Advice { checks: kept, ..self.clone() }, gone)
    }
}

impl Check {
    /// What knows it from one check to the next: the helper's own checks keep their id; Claude's
    /// findings are numbered anew each time, so theirs is their title (helper-vm's check_key).
    pub fn key(&self) -> String {
        if self.by_claude {
            format!("claude:{}", self.title.trim().to_lowercase())
        } else {
            self.id.clone()
        }
    }
}

/// What the owner said about the Advisor's checks, by check key (`Check::key`), kept in the pool
/// (NOTES_KEY) for the Helper VM: the checks they dismissed (its title then, and when), which the
/// Advisor no longer shows and Claude doesn't bring up again; and what they explained to Claude
/// about one (its title, their words, when), which Claude goes by from its next look.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Notes {
    pub dismissed: std::collections::BTreeMap<String, (String, i64)>,
    pub said: std::collections::BTreeMap<String, (String, String, i64)>,
}

/// A change to the owner's notes on a check.
#[derive(Clone, Debug, PartialEq)]
pub enum NoteChange {
    Dismiss,
    /// Shown again.
    Restore,
    /// What they explain to Claude about it.
    Explain(String),
    /// What they explained, taken back.
    Forget,
}

impl Notes {
    pub fn parse(text: &str) -> Notes {
        let Ok(j) = serde_json::from_str::<Value>(text) else { return Notes::default() };
        let map = |k: &str| j.get(k).and_then(|m| m.as_object()).cloned().unwrap_or_default();
        Notes {
            dismissed: map("dismissed").into_iter().map(|(k, v)| (k, (s(&v, "title"), i(&v, "at")))).collect(),
            said: map("said").into_iter().map(|(k, v)| (k, (s(&v, "title"), s(&v, "text"), i(&v, "at")))).filter(|(_, v)| !v.1.trim().is_empty()).collect(),
        }
    }

    pub fn to_json(&self) -> String {
        let dismissed: Map<String, Value> = self.dismissed.iter().map(|(k, (title, at))| (k.clone(), json!({"title": title, "at": at}))).collect();
        let said: Map<String, Value> = self.said.iter().map(|(k, (title, text, at))| (k.clone(), json!({"title": title, "text": text, "at": at}))).collect();
        json!({"version": 1, "dismissed": dismissed, "said": said}).to_string()
    }

    /// With `change` made to check `key` (its title `title`) at `now`.
    pub fn with(mut self, key: &str, title: &str, change: &NoteChange, now: i64) -> Notes {
        match change {
            NoteChange::Dismiss => {
                self.dismissed.insert(key.into(), (title.into(), now));
            }
            NoteChange::Restore => {
                self.dismissed.remove(key);
            }
            NoteChange::Explain(text) if !text.trim().is_empty() => {
                // (as long as a note may be: a few sentences)
                let text: String = text.trim().chars().take(600).collect();
                self.said.insert(key.into(), (title.into(), text, now));
            }
            NoteChange::Explain(_) | NoteChange::Forget => {
                self.said.remove(key);
            }
        }
        self
    }
}

/// The request to leave in the pool: what was there, with `what` asked at `now` (an update with
/// the version on GitHub to fetch it from, `update_ref`).
pub fn request(before: &str, what: &str, now: i64, update_ref: Option<&str>) -> String {
    let mut m = match serde_json::from_str::<Value>(before) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    m.insert(what.to_string(), json!(now));
    if let Some(r) = update_ref.filter(|_| what == "update") {
        m.insert("update_ref".into(), json!(r));
    }
    Value::Object(m).to_string()
}

/// The Helper VM version a copy of svm-helper.py is ("2.4.0").
pub fn version_in(script: &str) -> Option<&str> {
    script.lines().find_map(|l| l.strip_prefix("VERSION = \"")?.strip_suffix('"'))
}

/// The version of this app on GitHub for the Helper VM to update from: one that's there and comes
/// with this app's Helper VM (LATEST). That's this copy's own version once it's released; a copy
/// built before its release takes the latest release, if that one has the same Helper VM.
pub fn update_ref() -> Result<String, String> {
    let has_latest = |r: &str| -> Result<bool, String> {
        let url = format!("https://raw.githubusercontent.com/{}/{}/helper-vm/svm-helper.py", crate::update::REPO, r);
        match crate::conn::web_agent()?.get(&url).call() {
            Ok(a) => Ok(version_in(&a.into_string().map_err(|e| e.to_string())?) == Some(LATEST)),
            Err(ureq::Error::Status(404, _)) => Ok(false),
            Err(ureq::Error::Status(code, _)) => Err(format!("GitHub answered with HTTP {}.", code)),
            Err(e) => Err(match e.kind() {
                ureq::ErrorKind::Dns | ureq::ErrorKind::ConnectionFailed => "Couldn't reach GitHub: is this computer online?".into(),
                _ => e.to_string(),
            }),
        }
    };
    let own = format!("v{}", env!("CARGO_PKG_VERSION"));
    if has_latest(&own)? {
        return Ok(own);
    }
    let latest = format!("v{}", crate::update::latest()?.version);
    if latest != own && has_latest(&latest)? {
        return Ok(latest);
    }
    Err(format!("its version {} isn't on GitHub yet: it comes out with the next release of Simple VM Manager.", LATEST))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_defaults() {
        let d = Settings::parse("");
        assert_eq!((d.min_priority, d.storage, d.schedules, d.remount, d.order_on), (3, (true, 90), true, true, false));
        let mut x = Settings::default();
        x.telegram = Channel { on: true, url: " 123:abc ".into(), extra: "-100200".into() };
        x.ntfy = Channel { on: true, url: "https://ntfy.sh/home".into(), extra: String::new() };
        x.order_on = true;
        x.order = vec![("u-1".into(), 30), ("u-2".into(), 0)];
        x.quiet = (true, "22:30".into(), "06:00".into());
        let text = x.to_json();
        let back = Settings::parse(&text);
        assert_eq!(back.telegram.url, "123:abc");
        assert_eq!((back.ntfy.on, back.order.clone(), back.quiet.clone()), (true, x.order.clone(), x.quiet.clone()));
        // what the helper reads, by its keys
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["channels"]["telegram"]["chat"], "-100200");
        assert_eq!(v["start_order"]["vms"][1]["wait"], 0);
        assert_eq!(v["watch"]["storage"]["level"], 90);
        // keys it doesn't know stay
        let kept = Settings::parse(r#"{"future": {"a": 1}, "remount": false}"#);
        assert!(!kept.remount);
        assert!(kept.to_json().contains("\"future\""));
    }

    #[test]
    fn settings_problems() {
        let mut x = Settings::default();
        assert_eq!(x.problem(), None);
        x.discord = Channel { on: true, url: "discord.com/api/webhooks/1".into(), extra: String::new() };
        assert!(x.problem().unwrap().starts_with("Discord"));
        x.discord.url = "https://discord.com/api/webhooks/1".into();
        x.quiet = (true, "25:00".into(), "07:00".into());
        assert!(x.problem().unwrap().starts_with("Quiet"));
        x.quiet.1 = "23:00".into();
        assert_eq!(x.problem(), None);
        assert!(x.sends());
    }

    #[test]
    fn the_helper_vm_and_its_advice() {
        let v = VmStatus::parse(r#"{"version": "2.2.0", "machine": "svm-helper", "address": "192.168.1.13", "last_look": 1000, "claude": true, "advice": {"on": true, "last": 950, "request": 940, "bad": 1, "warn": 2}, "requests": 1, "test": {"request": 7, "results": {"ntfy": "ok"}}}"#).unwrap();
        assert_eq!((v.version.as_str(), v.claude, v.advice, v.requests), ("2.2.0", true, (true, 950, 940, 1, 2), true));
        assert_eq!(v.test, Some((7, vec![("ntfy".into(), "ok".into())])));
        assert!(v.fresh(1000 + 60) && !v.fresh(1000 + 3600));
        // an older one: no requests answered
        assert!(!VmStatus::parse(r#"{"version": "2.0.3", "last_look": 5, "copy": 1}"#).unwrap().requests);
        let a = Advice::parse(r#"{"at": 950, "request": 940, "by": "claude", "checks": [{"id": "clocks", "topic": "servers", "status": "bad", "title": "Relógio errado", "note": "Ligue o NTP.", "objects": ["godofredo"]}, {"id": "x", "topic": "vms", "status": "odd", "title": "skipped"}, {"id": "claude_1", "topic": "vms", "status": "warn", "title": "dc01", "by": "claude"}]}"#).unwrap();
        assert_eq!((a.at, a.request, a.by_claude, a.checks.len(), a.counts()), (950, 940, true, 2, (1, 1)));
        assert_eq!((a.checks[0].status, a.checks[0].objects.clone(), a.checks[1].by_claude), (Status::Bad, vec!["godofredo".to_string()], true));
        assert_eq!(request(r#"{"test": 5}"#, "advice", 9, None), r#"{"advice":9,"test":5}"#);
        assert_eq!(request("", "test", 3, Some("v1.31.5")), r#"{"test":3}"#);
        assert_eq!(request("", "update", 4, Some("v1.31.5")), r#"{"update":4,"update_ref":"v1.31.5"}"#);
        // the version a copy of the helper is, as update_ref reads it from GitHub
        assert_eq!(version_in("import os\n\nVERSION = \"2.4.0\"\nREPO = \"x\"\n"), Some("2.4.0"));
        assert_eq!(version_in("404: Not Found"), None);
        // a check the app can fix, and an update as the VM tells it
        let a = Advice::parse(r#"{"at": 1, "checks": [{"id": "autostart", "topic": "vms", "status": "warn", "title": "x", "fix": {"action": "autostart", "uuids": ["u-1"], "names": ["Plex"]}}]}"#).unwrap();
        assert_eq!(a.checks[0].fix, Some(Fix { action: "autostart".into(), uuids: vec!["u-1".into()], names: vec!["Plex".into()] }));
        let v = VmStatus::parse(r#"{"version": "2.3.0", "updates": 1, "update": {"ref": "v1.30.0", "at": 5, "ok": false, "error": "no network"}}"#).unwrap();
        assert_eq!((v.updates, v.update), (true, Some(("v1.30.0".into(), 5, false, "no network".into()))));
    }

    #[test]
    fn notes_dismiss_and_explain() {
        let c = |id: &str, by_claude: bool, title: &str, status: Status| Check { id: id.into(), topic: "vms".into(), status, title: title.into(), note: String::new(), objects: vec![], by_claude, fix: None };
        let (ha, plex, fine) = (c("ha", false, "No HA", Status::Warn), c("claude_3", true, " Plex has too much memory ", Status::Warn), c("ntp", false, "Clocks agree", Status::Ok));
        assert_eq!((ha.key(), plex.key()), ("ha".to_string(), "claude:plex has too much memory".to_string()));
        let n = Notes::default().with(&ha.key(), &ha.title, &NoteChange::Dismiss, 5).with(&plex.key(), &plex.title, &NoteChange::Explain("  It transcodes 4K at night. ".into()), 6).with(&fine.key(), &fine.title, &NoteChange::Dismiss, 7);
        // through the pool and back
        let n = Notes::parse(&n.to_json());
        assert_eq!(n.dismissed.get("ha"), Some(&("No HA".to_string(), 5)));
        assert_eq!(n.said.get("claude:plex has too much memory").map(|s| s.1.as_str()), Some("It transcodes 4K at night."));
        // the dismissed ones apart (a fine one can't be: there's nothing to dismiss)
        let a = Advice { at: 1, request: 0, by_claude: true, checks: vec![ha.clone(), plex.clone(), fine.clone()] };
        let (shown, gone) = a.split(&n);
        assert_eq!((shown.checks.len(), gone.len(), gone[0].id.as_str(), shown.counts()), (2, 1, "ha", (0, 1)));
        // brought back; what was explained, taken back (or emptied)
        let n = n.with("ha", "", &NoteChange::Restore, 8).with("claude:plex has too much memory", "", &NoteChange::Explain(" ".into()), 9);
        assert!(!n.dismissed.contains_key("ha") && n.said.is_empty());
        assert_eq!(Notes::parse("not json"), Notes::default());
    }

    #[test]
    fn latest_is_the_helper_this_version_comes_with() {
        let helper = include_str!("../helper-vm/svm-helper.py");
        assert!(helper.contains(&format!("\nVERSION = \"{}\"\n", LATEST)), "helper.rs's LATEST isn't helper-vm/svm-helper.py's VERSION");
        assert_eq!(version_in(helper), Some(LATEST));
        assert!(older("2.0.3", LATEST) && older("2.2.10", "2.3") && !older(LATEST, LATEST) && !older("2.10.0", "2.9.9"));
    }
}
