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
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 443; // XenAPI.HTTP.DEFAULT_HTTPS_PORT
pub const MAX_LOGIN_ATTEMPTS: u32 = 3; // XenConnection.DEFAULT_MAX_SESSION_LOGIN_ATTEMPTS
pub const ACTION_TIMEOUT_MS: u64 = 86_400_000; // XenAPI.Session.STANDARD_TIMEOUT
pub const DEMO_ACTION_MS: u64 = 1500; // estimated

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
            _ => {
                if self.details.is_empty() {
                    format!("The server said: {}", self.code)
                } else {
                    format!("The server said: {} ({})", self.code, self.details.join(", "))
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
    /// Deletes an ISO file: its disc, and the file on the share. Shut-down VMs that have it in a
    /// DVD drive get the drive emptied first, as destroying a disc also destroys the drives it's in.
    fn delete_iso(&self, vdi_ref: &str) -> Result<(), ApiError>;
    /// Changes a disc's description (an ISO file's, in an ISO library).
    fn set_vdi_description(&self, vdi_ref: &str, text: &str) -> Result<(), ApiError>;
    /// Copies a file into an ISO library as `name` (see `iso_file_name`): a new disc in the library,
    /// then the file's bytes sent to it, from this computer or as they arrive from the web.
    /// `address` is the server to send them to for a library that only one server sees; a shared
    /// one takes them on the pool coordinator. A failed or cancelled upload leaves nothing behind.
    fn upload_iso(&self, sr_ref: &str, address: Option<&str>, src: &Source, name: &str, t: &Arc<Transfer>) -> Result<(), ApiError>;
    fn close(&self);
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
fn web_agent() -> Result<ureq::Agent, String> {
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
fn web_get(url: &str) -> Result<ureq::Response, String> {
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
struct Counting {
    inner: Box<dyn Read + Send>,
    t: Arc<Transfer>,
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
    id: u64,
}

pub struct RealConnection {
    port: u16,
    user: String,
    password: String,
    state: Mutex<RealState>,
    tls: Arc<native_tls::TlsConnector>,
}

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
            state: Mutex::new(RealState { host, session: None, id: 0 }),
            tls: Arc::new(tls),
        }
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

    fn login(&self, st: &mut RealState, timeout_ms: u64) -> Result<(), ApiError> {
        let mut last = ApiError::other("Couldn't log in.");
        for _ in 0..MAX_LOGIN_ATTEMPTS {
            st.id += 1;
            let params = vec![json!(self.user), json!(self.password), json!("2.0"), json!("SimpleVMManager")];
            match self.rpc(&st.host.clone(), st.id, "session.login_with_password", params, timeout_ms) {
                Ok(v) => {
                    st.session = v.as_str().map(|s| s.to_string());
                    return Ok(());
                }
                Err(e) if e.code == "HOST_IS_SLAVE" && !e.details.is_empty() => {
                    st.host = e.details[0].clone();
                    last = e;
                }
                Err(e) if e.code == "CONNECTION" => last = e,
                Err(e) => return Err(e), // wrong password etc.: no point retrying
            }
        }
        Err(last)
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

    fn call(&self, method: &str, extra: Vec<Value>, timeout_ms: u64) -> Result<Value, ApiError> {
        let (host, session, id) = {
            let mut st = self.state.lock().unwrap();
            if st.session.is_none() {
                self.login(&mut st, timeout_ms.min(60_000))?;
            }
            st.id += 1;
            (st.host.clone(), st.session.clone().unwrap_or_default(), st.id)
        };
        let mut params = vec![json!(session)];
        params.extend(extra.iter().cloned());
        match self.rpc(&host, id, method, params, timeout_ms) {
            Err(e) if e.code == "SESSION_INVALID" => {
                let (host, session, id) = {
                    let mut st = self.state.lock().unwrap();
                    st.session = None;
                    self.login(&mut st, timeout_ms.min(60_000))?;
                    st.id += 1;
                    (st.host.clone(), st.session.clone().unwrap_or_default(), st.id)
                };
                let mut params = vec![json!(session)];
                params.extend(extra);
                self.rpc(&host, id, method, params, timeout_ms)
            }
            other => other,
        }
    }

    fn all(&self, cls: &str, timeout_ms: u64) -> Result<Map<String, Value>, ApiError> {
        Ok(self.call(&format!("{}.get_all_records", cls), vec![], timeout_ms)?.as_object().cloned().unwrap_or_default())
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
        let order: Vec<&str> = s(boot, "order").chars().filter_map(|c| match c {
            'c' => Some("Disk"),
            'd' => Some("DVD drive"),
            'n' => Some("Network"),
            _ => None,
        }).collect();
        vm.boot_order = order.join(", ");
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
    vm.snapshots = list(r, "snapshots").len();
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
        for (_, r) in self.all("pool", t)? {
            pd.name = s(&r, "name_label");
            master = s(&r, "master");
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
                cpu_count: n(&cpu, "count"),
                mem_total: n(&m, "memory_total"),
                mem_free: n(&m, "memory_free"),
                enabled: b(&r, "enabled"),
            });
        }
        // Each storage is plugged into the servers that can use it (a PBD per server): local storage
        // into one, shared storage into all of them.
        let mut plugged: HashMap<String, Vec<String>> = HashMap::new();
        let mut configs: HashMap<String, Value> = HashMap::new();
        for (_, r) in self.all("PBD", t)? {
            plugged.entry(s(&r, "SR")).or_default().push(s(&r, "host"));
            configs.entry(s(&r, "SR")).or_insert_with(|| r.get("device_config").cloned().unwrap_or(Value::Null));
        }
        for (k, r) in self.all("SR", t)? {
            let shared = b(&r, "shared");
            let hosts = plugged.get(&k).cloned().unwrap_or_default();
            let host = if !shared && hosts.len() == 1 { Some(hosts[0].clone()) } else { None };
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
            });
        }
        for (k, r) in self.all("network", t)? {
            pd.networks.push(NetInfo {
                rref: k,
                uuid: s(&r, "uuid"),
                name: s(&r, "name_label"),
                description: s(&r, "name_description"),
                bridge: s(&r, "bridge"),
                mtu: n(&r, "MTU"),
            });
        }
        let vbds = self.all("VBD", t)?;
        let vdis = self.all("VDI", t)?;
        let vm_records = self.all("VM", t)?;
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
        let net_names: HashMap<String, String> = pd.networks.iter().map(|x| (x.rref.clone(), x.name.clone())).collect();
        let sr_hosts: HashMap<String, String> = pd.srs.iter().filter_map(|x| x.host.clone().map(|h| (x.rref.clone(), h))).collect();
        for (k, r) in vm_records {
            if b(&r, "is_a_template") || b(&r, "is_control_domain") || b(&r, "is_a_snapshot") {
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
            for vbd_ref in list(&r, "VBDs") {
                let Some(vbd) = vbds.get(&vbd_ref) else { continue };
                let cd = s(vbd, "type") == "CD";
                let mut d = VmDisk { device: s(vbd, "userdevice"), kind: if cd { "DVD drive".into() } else { "Disk".into() }, ..Default::default() };
                if let Some(vdi) = vdis.get(&s(vbd, "VDI")) {
                    if vm.storage_host.is_none() {
                        vm.storage_host = sr_hosts.get(&s(vdi, "SR")).cloned();
                    }
                    d.name = s(vdi, "name_label");
                    d.size = n(vdi, "virtual_size");
                    d.sr_name = sr_names.get(&s(vdi, "SR")).cloned().unwrap_or_default();
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
                vm.nics.push(VmNic { mac: s(vif, "MAC"), network: net_names.get(&s(vif, "network")).cloned().unwrap_or_default(), ip, device: dev });
            }
            vm.nics.sort_by(|a, b| a.device.cmp(&b.device));
            pd.vms.push(vm);
        }
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

    fn set_vdi_description(&self, vdi_ref: &str, text: &str) -> Result<(), ApiError> {
        self.call("VDI.set_name_description", vec![json!(vdi_ref), json!(text)], 30_000).map(|_| ())
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
            return Err(ApiError::other("This VM has no graphical console."));
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
}

impl DemoConnection {
    pub fn new() -> DemoConnection {
        let mut d = PoolData { name: "Demo Pool".into(), ..Default::default() };
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
        d.networks.push(NetInfo { rref: "net-0".into(), uuid: demo_uuid(), name: "Pool-wide network 0".into(), description: String::new(), bridge: "xenbr0".into(), mtu: 1500 });
        d.networks.push(NetInfo { rref: "net-1".into(), uuid: demo_uuid(), name: "Storage network".into(), description: String::new(), bridge: "xenbr1".into(), mtu: 9000 });

        let vms: [(&str, &str, &str, PowerState, Option<&str>, &str, &str, i64, i64, &str, i64); 6] = [
            ("vm-web", "Web Server", "Public website front end", PowerState::Running, Some("host-a"), "Ubuntu 24.04 LTS", "10.20.1.21", 4, 4, "sr-nfs", 40),
            ("vm-db", "Database", "Main database server", PowerState::Running, Some("host-a"), "Rocky Linux 9", "10.20.1.22", 8, 16, "sr-nfs", 200),
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
                    VmDisk { name: format!("{} disk 0", name), device: "0".into(), sr_name, kind: "Disk".into(), size: disk * GB },
                    VmDisk { name: "(empty)".into(), device: "3".into(), sr_name: String::new(), kind: "DVD drive".into(), size: 0 },
                ],
                nics: vec![VmNic { device: "0".into(), mac: format!("aa:16:3e:{:02x}:10:{:02x}", i + 1, 0x20 + i), network: "Pool-wide network 0".into(), ip: ip.to_string() }],
                ..Default::default()
            });
            demo_details(d.vms.last_mut().unwrap());
        }
        recompute(&mut d);
        DemoConnection { data: Mutex::new(d) }
    }
}

/// The discs in the demo's ISO library `sr`, two of them described.
fn demo_isos(sr: &str) -> Vec<IsoFile> {
    [
        ("debian-12.7.0-amd64-netinst.iso", 661, "Network install, for small servers"),
        ("rocky-9.4-x86_64-minimal.iso", 1_800, ""),
        ("ubuntu-24.04.1-live-server-amd64.iso", 2_700, "Ubuntu Server LTS, for the web servers"),
        ("Win11_24H2_English_x64.iso", 5_800, ""),
        ("windows-server-2025.iso", 5_200, ""),
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
    let (tags, folder, days, snaps, auto, ha): (&[&str], &str, i64, usize, bool, &str) = match v.rref.as_str() {
        "vm-web" => (&["web", "production"], "/Production", 12, 2, true, "Restart"),
        "vm-db" => (&["database", "production"], "/Production", 31, 5, true, "Restart"),
        "vm-mail" => (&["mail"], "/Production", 4, 0, true, "Best effort"),
        "vm-build" => (&["ci"], "/Development", 0, 1, false, ""),
        "vm-desk" => (&["testing"], "/Development", 0, 3, false, ""),
        _ => (&["backup"], "", 0, 0, false, ""),
    };
    v.tags = tags.iter().map(|t| t.to_string()).collect();
    v.folder = folder.into();
    v.snapshots = snaps;
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
            PowerState::Halted => vec!["start"],
            PowerState::Running => vec!["clean_shutdown", "clean_reboot", "suspend", "hard_shutdown", "hard_reboot"],
            PowerState::Suspended => vec!["resume", "hard_shutdown"],
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
    /// Pretends to mount the share: an address with "unreachable" in it doesn't answer, and the
    /// password "wrong" is refused, so the error page can be tried out.
    fn create_iso_library(&self, lib: &IsoLibrary) -> Result<(), ApiError> {
        std::thread::sleep(Duration::from_millis(900));
        let fail = |text: &str| Err(ApiError { code: "SR_BACKEND_FAILURE_222".into(), details: vec![String::new(), text.into(), String::new()] });
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
        let mut d = self.data.lock().unwrap();
        let rref = format!("sr-iso-{}", d.srs.len());
        let files = demo_isos(&rref);
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
        });
        Ok(())
    }
    fn scan_sr(&self, _sr_ref: &str) -> Result<(), ApiError> {
        std::thread::sleep(Duration::from_millis(600));
        Ok(())
    }
    fn forget_sr(&self, sr_ref: &str) -> Result<(), ApiError> {
        std::thread::sleep(Duration::from_millis(600));
        let mut d = self.data.lock().unwrap();
        if d.vms.iter().any(|v| v.power == PowerState::Running && v.disks.iter().any(|k| k.kind != "Disk" && d.sr(sr_ref).map_or(false, |x| x.name == k.sr_name))) {
            return Err(ApiError { code: "VDI_IN_USE".into(), details: vec![] });
        }
        d.srs.retain(|x| x.rref != sr_ref);
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
    fn set_vdi_description(&self, vdi_ref: &str, text: &str) -> Result<(), ApiError> {
        std::thread::sleep(Duration::from_millis(400));
        let mut d = self.data.lock().unwrap();
        let f = d.srs.iter_mut().flat_map(|x| x.files.iter_mut()).find(|f| f.vdi == vdi_ref);
        let f = f.ok_or_else(|| ApiError { code: "HANDLE_INVALID".into(), details: vec!["VDI".into(), vdi_ref.into()] })?;
        f.description = text.to_string();
        Ok(())
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
    use super::*;

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
    fn demo_delete_iso() {
        let c = DemoConnection::new();
        let names = || c.data.lock().unwrap().sr("sr-iso").unwrap().files.iter().map(|f| f.name.clone()).collect::<Vec<_>>();
        // in a running VM's drive: refused, as by the server
        assert_eq!(c.delete_iso("sr-iso-vdi-2").unwrap_err().code, "VDI_IN_USE");
        // in a shut-down VM's drive, or in none: gone
        c.delete_iso("sr-iso-vdi-1").unwrap();
        c.delete_iso("sr-iso-vdi-0").unwrap();
        assert_eq!(names(), vec!["ubuntu-24.04.1-live-server-amd64.iso", "Win11_24H2_English_x64.iso", "windows-server-2025.iso"]);
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
