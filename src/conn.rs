//! Talking to servers: JSON-RPC 2.0 over HTTPS at /jsonrpc (as the original's XenAPI.JsonRpcClient),
//! and an in-memory demo pool (the original has simulated connections too).

use crate::model::*;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    fn close(&self);
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
        for (_, r) in self.all("PBD", t)? {
            plugged.entry(s(&r, "SR")).or_default().push(s(&r, "host"));
        }
        for (k, r) in self.all("SR", t)? {
            let shared = b(&r, "shared");
            let hosts = plugged.get(&k).cloned().unwrap_or_default();
            let host = if !shared && hosts.len() == 1 { Some(hosts[0].clone()) } else { None };
            pd.srs.push(SrInfo {
                rref: k,
                uuid: s(&r, "uuid"),
                name: s(&r, "name_label"),
                description: s(&r, "name_description"),
                kind: s(&r, "type"),
                size: n(&r, "physical_size"),
                used: n(&r, "physical_utilisation"),
                shared,
                host,
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
        let vifs = self.all("VIF", t)?;
        let guests = self.all("VM_guest_metrics", t)?;
        let sr_names: HashMap<String, String> = pd.srs.iter().map(|x| (x.rref.clone(), x.name.clone())).collect();
        let net_names: HashMap<String, String> = pd.networks.iter().map(|x| (x.rref.clone(), x.name.clone())).collect();
        let sr_hosts: HashMap<String, String> = pd.srs.iter().filter_map(|x| x.host.clone().map(|h| (x.rref.clone(), h))).collect();
        for (k, r) in self.all("VM", t)? {
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
                os: s(g.get("os_version").unwrap_or(&Value::Null), "name"),
                ip: String::new(),
                power: PowerState::parse(&s(&r, "power_state")),
                mem_static_max: n(&r, "memory_static_max"),
                mem_dyn_min: n(&r, "memory_dynamic_min"),
                mem_dyn_max: n(&r, "memory_dynamic_max"),
                vcpus: n(&r, "VCPUs_at_startup"),
                allowed: list(&r, "allowed_operations"),
                disks: vec![],
                nics: vec![],
            };
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
        };
        d.srs.push(sr("sr-local1", "Local storage", "lvm", 900 * GB, 0, Some("host-a")));
        d.srs.push(sr("sr-dvd1", "DVD drives", "udev", 4_700_000_000, 4_700_000_000, Some("host-a"))); // a disc in it
        d.srs.push(sr("sr-local2", "Local storage", "lvm", 900 * GB, 0, Some("host-b")));
        d.srs.push(sr("sr-dvd2", "DVD drives", "udev", 0, 0, Some("host-b"))); // empty: not listed
        d.srs.push(sr("sr-usb2", "Removable storage", "udev", 0, 0, Some("host-b"))); // empty: not listed
        d.srs.push(sr("sr-nfs", "Shared NFS storage", "nfs", 4096 * GB, 0, None));
        d.srs.push(sr("sr-iso", "ISO library", "iso", 250 * GB, 87 * GB, None));
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
            });
        }
        recompute(&mut d);
        DemoConnection { data: Mutex::new(d) }
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
    fn open_console(&self, _vm_ref: &str) -> Result<ConsoleStream, ApiError> {
        let addr = debug_console_address().ok_or_else(|| ApiError::other("The demo server has no real consoles."))?;
        let (host, port) = split_host_port(addr.trim());
        Ok(ConsoleStream::Plain(tcp_connect(&host, port.unwrap_or(5900))?))
    }
    fn close(&self) {}
}
