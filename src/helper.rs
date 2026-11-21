//! The Server Helper: an optional add-on on a pool's servers (helper/svm-helper, a XAPI plugin run
//! every minute by cron) that sends the pool's alerts to phones (Telegram, ntfy, Discord, Home
//! Assistant, any webhook), watches any storage's fullness, makes up missed snapshot schedule
//! runs, mounts the NAS again after a server started before it, and starts VMs in order when the
//! pool starts. Its settings are the pool's other_config "svm-helper", as JSON; this is that
//! JSON, and the command that installs it.

use serde_json::{json, Map, Value};

/// The helper's version this app installs (helper/svm-helper's VERSION).
pub const VERSION: &str = "1.0";

/// The pool's other_config key its settings are under.
pub const KEY: &str = "svm-helper";

/// Where its code is, to read before installing it.
pub fn source_url() -> String {
    format!("https://github.com/{}/tree/v{}/helper", crate::update::REPO, env!("CARGO_PKG_VERSION"))
}

/// The command that installs it (or brings it up to date) on a server, as root: this app's
/// version's install.sh, which checks svm-helper's SHA-256 before it puts it in place.
pub fn install_command() -> String {
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    format!("curl -fsSL https://raw.githubusercontent.com/{}/{}/helper/install.sh | sh -s {}", crate::update::REPO, tag, tag)
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

/// A server's helper, as last asked.
#[derive(Clone, Debug, PartialEq)]
pub enum Presence {
    Checking,
    Installed(String),
    Missing,
    /// It couldn't be asked (the server's off, or not answering): why.
    Unknown(String),
}

impl Presence {
    pub fn installed(&self) -> bool {
        matches!(self, Presence::Installed(_))
    }
    /// Installed, but an older version than this app's.
    pub fn outdated(&self) -> bool {
        matches!(self, Presence::Installed(v) if version_less(v, VERSION))
    }
}

fn version_less(a: &str, b: &str) -> bool {
    let parts = |v: &str| v.split('.').map(|p| p.trim().parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    parts(a) < parts(b)
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
    fn versions_and_command() {
        assert!(Presence::Installed("0.9".into()).outdated());
        assert!(!Presence::Installed(VERSION.into()).outdated());
        assert!(install_command().contains(&format!("/v{}/helper/install.sh | sh -s v{}", env!("CARGO_PKG_VERSION"), env!("CARGO_PKG_VERSION"))));
    }
}
