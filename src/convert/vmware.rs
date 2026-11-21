//! VMware ESXi or vCenter as a source, through the vSphere API (SOAP at /sdk) as VMware's own
//! tools use it: its VMs and their hardware (RetrievePropertiesEx over a container view), a clean
//! shutdown (ShutdownGuest), a program run in a VM through VMware Tools (the guest operations
//! manager), and each disk's flat file read from its datastore over HTTPS (/folder).

use super::*;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Vmware {
    base: String,
    user: String,
    password: String,
    agent: ureq::Agent,
    state: Mutex<State>,
}

#[derive(Default, Clone)]
struct State {
    cookie: Option<String>,
    version: String,
    pc: String,
    views: String,
    sessions: String,
    root: String,
    guest_ops: String,
    about: String,
    vcenter: bool,
    /// Each VM's datacenter and ESXi host (its name), by its moref, from the last listing.
    vm_places: Vec<(String, String, String)>,
}

/// XML's five escapes.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn first<'a>(n: roxmltree::Node<'a, 'a>, name: &str) -> Option<roxmltree::Node<'a, 'a>> {
    n.descendants().find(|c| c.is_element() && c.tag_name().name() == name)
}

fn kids<'a>(n: roxmltree::Node<'a, 'a>, name: &'a str) -> impl Iterator<Item = roxmltree::Node<'a, 'a>> + 'a {
    n.children().filter(move |c| c.is_element() && c.tag_name().name() == name)
}

fn text_of(n: roxmltree::Node, name: &str) -> String {
    n.children().find(|c| c.is_element() && c.tag_name().name() == name).and_then(|c| c.text()).unwrap_or("").trim().to_string()
}

fn xsi_type(n: roxmltree::Node) -> String {
    n.attributes().find(|a| a.name() == "type").map(|a| a.value().rsplit(':').next().unwrap_or("").to_string()).unwrap_or_default()
}

/// One object of a property listing: its moref, and each property's value (as a node's XML).
pub struct Obj {
    pub moref: String,
    pub props: Vec<(String, String)>,
}

impl Obj {
    fn get(&self, name: &str) -> Option<&str> {
        self.props.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
    /// A property's plain value (the text inside its <val>).
    pub fn text(&self, name: &str) -> String {
        self.get(name).and_then(|x| roxmltree::Document::parse(x).ok().and_then(|d| val_root(&d).and_then(|v| v.text()).map(|t| t.trim().to_string()))).unwrap_or_default()
    }
}

/// A property's <val>, in the document made of it.
fn val_root<'a>(d: &'a roxmltree::Document<'a>) -> Option<roxmltree::Node<'a, 'a>> {
    d.root_element().children().find(|c| c.is_element())
}

/// The objects in a RetrievePropertiesEx answer, and its token for more.
pub fn parse_objects(xml: &str) -> Result<(Vec<Obj>, Option<String>), String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| format!("VMware's answer can't be read: {}", e))?;
    let mut out = vec![];
    let Some(ret) = first(doc.root_element(), "returnval") else { return Ok((out, None)) };
    let token = Some(text_of(ret, "token")).filter(|t| !t.is_empty());
    for o in kids(ret, "objects") {
        let moref = text_of(o, "obj");
        let mut props = vec![];
        for p in kids(o, "propSet") {
            let name = text_of(p, "name");
            if let Some(v) = kids(p, "val").next() {
                // (inside an element that names the namespaces it uses, to be read on its own)
                let r = v.range();
                props.push((name, format!("<w xmlns=\"urn:vim25\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\">{}</w>", &xml[r])));
            }
        }
        out.push(Obj { moref, props });
    }
    Ok((out, token))
}

impl Vmware {
    pub fn new(address: &str, user: &str, password: &str) -> Vmware {
        Vmware { base: base_url(address, 443), user: user.into(), password: password.into(), agent: agent(120), state: Mutex::new(State::default()) }
    }

    fn st(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    /// A SOAP call; its answer's XML. A session that ran out is logged in again, once.
    fn call(&self, body: &str) -> Result<String, String> {
        match self.call_once(body) {
            Err(e) if e.contains("NotAuthenticated") || e.contains("session is not authenticated") => {
                self.log_in()?;
                self.call_once(body)
            }
            r => r,
        }
    }

    fn call_once(&self, body: &str) -> Result<String, String> {
        let st = self.st();
        let version = if st.version.is_empty() { "7.0".to_string() } else { st.version.clone() };
        let env = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><soapenv:Envelope xmlns:soapenv=\"http://schemas.xmlsoap.org/soap/envelope/\" xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"><soapenv:Body>{}</soapenv:Body></soapenv:Envelope>",
            body
        );
        let mut req = self.agent.post(&format!("{}/sdk", self.base)).set("Content-Type", "text/xml; charset=utf-8").set("SOAPAction", &format!("urn:vim25/{}", version));
        if let Some(c) = &st.cookie {
            req = req.set("Cookie", c);
        }
        let r = req.send_string(&env).map_err(|e| http_error(e, "VMware"))?;
        if let Some(c) = r.header("Set-Cookie").and_then(|c| c.split(';').next()).filter(|c| c.starts_with("vmware_soap_session")) {
            self.state.lock().unwrap().cookie = Some(c.to_string());
        }
        r.into_string().map_err(|e| format!("VMware's answer broke off: {}", e))
    }

    /// Connects and logs in: the API's version, its service content, a session.
    pub fn log_in(&self) -> Result<(), String> {
        // the newest API version it speaks
        if self.st().version.is_empty() {
            let v = self
                .agent
                .get(&format!("{}/sdk/vimServiceVersions.xml", self.base))
                .call()
                .ok()
                .and_then(|r| r.into_string().ok())
                .and_then(|x| x.split("<version>").nth(1).and_then(|v| v.split('<').next()).map(|v| v.trim().to_string()))
                .unwrap_or_else(|| "7.0".into());
            self.state.lock().unwrap().version = v;
        }
        let xml = self.call_once("<RetrieveServiceContent xmlns=\"urn:vim25\"><_this type=\"ServiceInstance\">ServiceInstance</_this></RetrieveServiceContent>")?;
        let doc = roxmltree::Document::parse(&xml).map_err(|e| format!("VMware's answer can't be read: {}", e))?;
        let ret = first(doc.root_element(), "returnval").ok_or("That isn't a VMware ESXi or vCenter (its API didn't answer).")?;
        let about = first(ret, "about");
        {
            let mut st = self.state.lock().unwrap();
            st.pc = text_of(ret, "propertyCollector");
            st.views = text_of(ret, "viewManager");
            st.sessions = text_of(ret, "sessionManager");
            st.root = text_of(ret, "rootFolder");
            st.guest_ops = text_of(ret, "guestOperationsManager");
            st.about = about.map(|a| text_of(a, "fullName")).unwrap_or_default();
            st.vcenter = about.map(|a| text_of(a, "apiType")).as_deref() == Some("VirtualCenter");
        }
        let st = self.st();
        self.call_once(&format!(
            "<Login xmlns=\"urn:vim25\"><_this type=\"SessionManager\">{}</_this><userName>{}</userName><password>{}</password></Login>",
            esc(&st.sessions),
            esc(&self.user),
            esc(&self.password)
        ))
        .map_err(|e| if e.contains("InvalidLogin") || e.contains("incorrect user name or password") { "VMware: the user name or password isn't right.".to_string() } else { e })?;
        Ok(())
    }

    fn ensure(&self) -> Result<(), String> {
        if self.st().cookie.is_none() {
            self.log_in()?;
        }
        Ok(())
    }

    /// Every object of `kind` in `container` (a folder or datacenter), with these properties.
    pub fn objects(&self, container: (&str, &str), kind: &str, paths: &[&str]) -> Result<Vec<Obj>, String> {
        let st = self.st();
        let view = self.call(&format!(
            "<CreateContainerView xmlns=\"urn:vim25\"><_this type=\"ViewManager\">{}</_this><container type=\"{}\">{}</container><type>{}</type><recursive>true</recursive></CreateContainerView>",
            esc(&st.views),
            container.0,
            esc(container.1),
            kind
        ))?;
        let view = roxmltree::Document::parse(&view).ok().and_then(|d| first(d.root_element(), "returnval").and_then(|n| n.text()).map(|t| t.trim().to_string())).ok_or("VMware didn't make a view of its VMs.")?;
        let path_set: String = paths.iter().map(|p| format!("<pathSet>{}</pathSet>", p)).collect();
        let mut xml = self.call(&format!(
            "<RetrievePropertiesEx xmlns=\"urn:vim25\"><_this type=\"PropertyCollector\">{}</_this><specSet><propSet><type>{}</type>{}</propSet><objectSet><obj type=\"ContainerView\">{}</obj><skip>true</skip><selectSet xsi:type=\"TraversalSpec\"><name>view</name><type>ContainerView</type><path>view</path><skip>false</skip></selectSet></objectSet></specSet><options><maxObjects>200</maxObjects></options></RetrievePropertiesEx>",
            esc(&st.pc),
            kind,
            path_set,
            esc(&view)
        ));
        let mut out = vec![];
        loop {
            let (objs, token) = parse_objects(&xml?)?;
            out.extend(objs);
            let Some(t) = token else { break };
            xml = self.call(&format!("<ContinueRetrievePropertiesEx xmlns=\"urn:vim25\"><_this type=\"PropertyCollector\">{}</_this><token>{}</token></ContinueRetrievePropertiesEx>", esc(&st.pc), esc(&t)));
        }
        let _ = self.call(&format!("<DestroyView xmlns=\"urn:vim25\"><_this type=\"ContainerView\">{}</_this></DestroyView>", esc(&view)));
        Ok(out)
    }

    /// One object's properties.
    fn props(&self, kind: &str, moref: &str, paths: &[&str]) -> Result<Obj, String> {
        let st = self.st();
        let path_set: String = paths.iter().map(|p| format!("<pathSet>{}</pathSet>", p)).collect();
        let xml = self.call(&format!(
            "<RetrievePropertiesEx xmlns=\"urn:vim25\"><_this type=\"PropertyCollector\">{}</_this><specSet><propSet><type>{}</type>{}</propSet><objectSet><obj type=\"{}\">{}</obj><skip>false</skip></objectSet></specSet><options/></RetrievePropertiesEx>",
            esc(&st.pc),
            kind,
            path_set,
            kind,
            esc(moref)
        ))?;
        parse_objects(&xml)?.0.into_iter().next().ok_or_else(|| "VMware no longer has that VM.".to_string())
    }

    fn power(&self, vm: &str) -> Result<String, String> {
        Ok(self.props("VirtualMachine", vm, &["runtime.powerState"])?.text("runtime.powerState"))
    }

    fn auth(user: &str, password: &str) -> String {
        format!("<auth xsi:type=\"NamePasswordAuthentication\"><interactiveSession>false</interactiveSession><username>{}</username><password>{}</password></auth>", esc(user), esc(password))
    }

    /// The datastore file URL of a "[datastore] folder/file" path, in a datacenter.
    fn file_url(&self, ds_path: &str, dc: &str) -> Option<String> {
        let (ds, path) = ds_path.strip_prefix('[')?.split_once("] ")?;
        let enc = |s: &str| s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) { (b as char).to_string() } else { format!("%{:02X}", b) }).collect::<String>();
        Some(format!("{}/folder/{}?dcPath={}&dsName={}", self.base, enc(path), enc(dc), enc(ds)))
    }

    fn basic(&self) -> String {
        format!("Basic {}", base64(format!("{}:{}", self.user, self.password).as_bytes()))
    }
}

/// What VM properties are read.
const VM_PROPS: &[&str] = &[
    "name",
    "config.template",
    "config.guestId",
    "config.guestFullName",
    "guest.guestFullName",
    "config.hardware.numCPU",
    "config.hardware.numCoresPerSocket",
    "config.hardware.memoryMB",
    "config.hardware.device",
    "config.firmware",
    "config.bootOptions.efiSecureBootEnabled",
    "config.bootOptions.bootOrder",
    "config.keyId",
    "runtime.powerState",
    "runtime.host",
    "guest.toolsRunningStatus",
    "layoutEx.disk",
    "layoutEx.file",
];

/// A VM from its properties. `dc`: its datacenter; `host`: its ESXi's name; `vlans`: each
/// port group's VLAN (standard and distributed); `dvpg`: each distributed port group's key and
/// name.
pub fn vm_from(o: &Obj, dc: &str, host: &str, vlans: &[(String, i64)], dvpg: &[(String, String)], url: &dyn Fn(&str) -> Option<String>) -> Option<SourceVm> {
    if o.text("config.template") == "true" {
        return None;
    }
    let name = o.text("name");
    let os = format!("{} {}", o.text("config.guestId"), {
        let g = o.text("guest.guestFullName");
        if g.is_empty() { o.text("config.guestFullName") } else { g }
    })
    .trim()
    .to_string();
    let cpus = o.text("config.hardware.numCPU").parse().unwrap_or(1i64).max(1);
    let mut cores = o.text("config.hardware.numCoresPerSocket").parse().unwrap_or(1i64).max(1);
    if cpus % cores != 0 {
        cores = 1;
    }
    let memory = o.text("config.hardware.memoryMB").parse::<u64>().unwrap_or(1024) << 20;
    let uefi = o.text("config.firmware") == "efi";
    let power = match o.text("runtime.powerState").as_str() {
        "poweredOn" => Power::On,
        "poweredOff" => Power::Off,
        "suspended" => Power::Paused,
        _ => Power::Unknown,
    };
    // its files: each disk's chain of files (more than one link: it has snapshots), and sizes
    let mut files: Vec<(String, String, String, u64)> = vec![]; // key, name, type, size
    if let Some(x) = o.get("layoutEx.file") {
        if let Ok(d) = roxmltree::Document::parse(x) {
            for f in val_root(&d).into_iter().flat_map(|v| v.children()).filter(|c| c.is_element()) {
                files.push((text_of(f, "key"), text_of(f, "name"), text_of(f, "type"), text_of(f, "size").parse().unwrap_or(0)));
            }
        }
    }
    let mut chains: Vec<(String, Vec<Vec<String>>)> = vec![]; // disk key, chain of file keys
    if let Some(x) = o.get("layoutEx.disk") {
        if let Ok(d) = roxmltree::Document::parse(x) {
            for dl in val_root(&d).into_iter().flat_map(|v| v.children()).filter(|c| c.is_element()) {
                let key = text_of(dl, "key");
                let chain = kids(dl, "chain").map(|c| kids(c, "fileKey").map(|k| k.text().unwrap_or("").trim().to_string()).collect()).collect();
                chains.push((key, chain));
            }
        }
    }
    let boot_disk = o.get("config.bootOptions.bootOrder").and_then(|x| roxmltree::Document::parse(x).ok().and_then(|d| d.root_element().descendants().find(|n| n.is_element() && n.tag_name().name() == "deviceKey").and_then(|n| n.text()).map(|t| t.trim().to_string())));
    let mut blocked = None;
    if !o.text("config.keyId").is_empty() || o.get("config.keyId").map_or(false, |x| x.contains("keyId")) {
        blocked = Some("It's encrypted (VM encryption): decrypt it in VMware first.".to_string());
    }
    // its devices: controllers (bus kind and number), disks, cards, a vTPM
    let mut controllers: Vec<(String, u8, i64)> = vec![]; // key, bus order, bus number
    let mut disks: Vec<(u8, i64, i64, String, SourceDisk)> = vec![];
    let mut nics = vec![];
    let mut vtpm = false;
    if let Some(x) = o.get("config.hardware.device") {
        if let Ok(d) = roxmltree::Document::parse(x) {
            let devs: Vec<roxmltree::Node> = val_root(&d).into_iter().flat_map(|v| v.children()).filter(|c| c.is_element()).collect();
            for dev in &devs {
                let t = xsi_type(*dev);
                let bus = if t.contains("SCSI") || t.contains("LsiLogic") || t.contains("BusLogic") || t.contains("ParaVirtualSCSI") {
                    Some(0)
                } else if t.contains("NVME") || t.contains("Nvme") {
                    Some(1)
                } else if t.contains("AHCI") || t.contains("Sata") {
                    Some(2)
                } else if t.contains("IDEController") {
                    Some(3)
                } else {
                    None
                };
                if let Some(b) = bus {
                    controllers.push((text_of(*dev, "key"), b, text_of(*dev, "busNumber").parse().unwrap_or(0)));
                }
            }
            for dev in &devs {
                let t = xsi_type(*dev);
                let key = text_of(*dev, "key");
                let label = first(*dev, "deviceInfo").map(|i| text_of(i, "label")).unwrap_or_default();
                if t == "VirtualDisk" {
                    let size = text_of(*dev, "capacityInBytes").parse::<u64>().ok().filter(|&b| b > 0).unwrap_or_else(|| text_of(*dev, "capacityInKB").parse::<u64>().unwrap_or(0) * 1024);
                    let backing = kids(*dev, "backing").next();
                    let bt = backing.map(xsi_type).unwrap_or_default();
                    let file = backing.map(|b| text_of(b, "fileName")).unwrap_or_default();
                    if bt.contains("RawDiskMapping") {
                        blocked.get_or_insert_with(|| format!("{} is a raw device mapping (RDM), not a file: VMware can't hand it out as one.", label));
                        continue;
                    }
                    if backing.map_or(false, |b| kids(b, "parent").next().is_some()) || chains.iter().any(|(k, c)| *k == key && c.len() > 1) {
                        blocked.get_or_insert_with(|| "It has snapshots: delete them first in VMware (Snapshots, Delete All), which merges them into its disks.".to_string());
                    }
                    // its data file: the chain's file of type diskExtent (thin, thick, NFS)
                    let chain_keys: Vec<String> = chains.iter().find(|(k, _)| *k == key).and_then(|(_, c)| c.last().cloned()).unwrap_or_default();
                    let extent = files.iter().find(|f| chain_keys.contains(&f.0) && f.2 == "diskExtent");
                    let (at, used) = match extent {
                        Some((_, name, _, sz)) => match url(name) {
                            Some(u) => (DiskAt::Http { url: u }, *sz),
                            None => {
                                blocked.get_or_insert_with(|| format!("{}'s file can't be reached ({}).", label, name));
                                continue;
                            }
                        },
                        None => {
                            blocked.get_or_insert_with(|| format!("{} isn't a plain file on its datastore (vSAN or vVols?): export the VM from VMware as an OVF instead, and convert the files.", label));
                            continue;
                        }
                    };
                    let ckey = text_of(*dev, "controllerKey");
                    let (bus, busno) = controllers.iter().find(|c| c.0 == ckey).map(|c| (c.1, c.2)).unwrap_or((9, 0));
                    let unit = text_of(*dev, "unitNumber").parse().unwrap_or(0);
                    let first_one = Some(&key) == boot_disk.as_ref();
                    let shown = if file.is_empty() { label.clone() } else { format!("{} ({})", label, file.rsplit('/').next().unwrap_or(&file)) };
                    disks.push((if first_one { 0 } else { bus + 1 }, busno, unit, key, SourceDisk { label: shown, size, used: used.min(size), at }));
                } else if t == "VirtualTPM" {
                    vtpm = true;
                } else if t.starts_with("Virtual") && first(*dev, "macAddress").is_some() {
                    let mac = norm_mac(&text_of(*dev, "macAddress"));
                    let backing = kids(*dev, "backing").next();
                    let network = match backing {
                        Some(b) if xsi_type(b).contains("DistributedVirtualPort") => {
                            let pg = first(b, "portgroupKey").and_then(|n| n.text()).unwrap_or("").trim().to_string();
                            dvpg.iter().find(|(k, _)| *k == pg).map(|(_, n)| n.clone()).unwrap_or(pg)
                        }
                        Some(b) => text_of(b, "deviceName"),
                        None => String::new(),
                    };
                    let vlan = vlans.iter().find(|(n, _)| *n == network).map(|(_, v)| *v).filter(|v| (1..=4094).contains(v));
                    nics.push(SourceNic { network, vlan, mac });
                }
            }
        }
    }
    disks.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
    let disks: Vec<SourceDisk> = disks.into_iter().map(|d| d.4).collect();
    if disks.is_empty() && blocked.is_none() {
        blocked = Some("It has no disks.".into());
    }
    let agent = o.text("guest.toolsRunningStatus") == "guestToolsRunning";
    let note = (power == Power::Paused).then(|| "It's suspended: resume it and shut it down, or it'll be turned off where it is.".to_string());
    Some(SourceVm {
        id: o.moref.clone(),
        family: family_of(&os),
        name,
        os,
        cpus,
        cores,
        memory,
        uefi,
        secure_boot: uefi && o.text("config.bootOptions.efiSecureBootEnabled") == "true",
        vtpm: uefi && vtpm,
        disks,
        nics,
        power,
        agent,
        blocked,
        note,
        node: if host.is_empty() { dc.to_string() } else { host.to_string() },
    })
}

impl Source for Vmware {
    fn describe(&self) -> String {
        let st = self.st();
        if st.about.is_empty() { "VMware".into() } else { st.about }
    }

    fn list(&self) -> Result<Vec<SourceVm>, String> {
        self.ensure()?;
        let st = self.st();
        let root = ("Folder", st.root.as_str());
        let dcs = self.objects(root, "Datacenter", &["name"])?;
        let hosts = self.objects(root, "HostSystem", &["name", "config.network.portgroup"]).unwrap_or_default();
        let mut vlans: Vec<(String, i64)> = vec![];
        for h in &hosts {
            if let Some(x) = h.get("config.network.portgroup") {
                if let Ok(d) = roxmltree::Document::parse(x) {
                    for pg in d.root_element().descendants().filter(|n| n.is_element() && n.tag_name().name() == "spec") {
                        vlans.push((text_of(pg, "name"), text_of(pg, "vlanId").parse().unwrap_or(0)));
                    }
                }
            }
        }
        let mut dvpg = vec![];
        if st.vcenter {
            for pg in self.objects(root, "DistributedVirtualPortgroup", &["key", "name", "config.defaultPortConfig"]).unwrap_or_default() {
                let name = pg.text("name");
                if let Some(v) = pg.get("config.defaultPortConfig").and_then(|x| x.split("<vlanId>").nth(1)).and_then(|v| v.split('<').next()).and_then(|v| v.trim().parse::<i64>().ok()) {
                    vlans.push((name.clone(), v));
                }
                dvpg.push((pg.text("key"), name));
            }
        }
        let host_name = |moref: &str| hosts.iter().find(|h| h.moref == moref).map(|h| h.text("name")).unwrap_or_default();
        let mut out = vec![];
        let mut places = vec![];
        for dc in &dcs {
            let dc_name = dc.text("name");
            for o in self.objects(("Datacenter", &dc.moref), "VirtualMachine", VM_PROPS)? {
                let host = host_name(&o.text("runtime.host"));
                let url = |p: &str| self.file_url(p, &dc_name);
                if let Some(v) = vm_from(&o, &dc_name, &host, &vlans, &dvpg, &url) {
                    places.push((v.id.clone(), dc_name.clone(), host.clone()));
                    out.push(v);
                }
            }
        }
        self.state.lock().unwrap().vm_places = places;
        out.sort_by_key(|v| v.name.to_lowercase());
        Ok(out)
    }

    fn shutdown(&self, vm: &SourceVm, say: &dyn Fn(String)) -> Result<(), String> {
        self.ensure()?;
        let p = self.power(&vm.id)?;
        if p == "poweredOff" {
            return Ok(());
        }
        let tools = self.props("VirtualMachine", &vm.id, &["guest.toolsRunningStatus"])?.text("guest.toolsRunningStatus");
        if tools != "guestToolsRunning" {
            return Err("VMware Tools isn't running in it, so it can't be shut down cleanly: shut it down in VMware, then Try Again.".into());
        }
        say("Shutting it down on VMware…".into());
        self.call(&format!("<ShutdownGuest xmlns=\"urn:vim25\"><_this type=\"VirtualMachine\">{}</_this></ShutdownGuest>", esc(&vm.id)))?;
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(600) {
            std::thread::sleep(Duration::from_secs(4));
            if self.power(&vm.id)? == "poweredOff" {
                return Ok(());
            }
        }
        Err("It didn't shut down within 10 minutes: shut it down in VMware, then Try Again.".into())
    }

    fn prepare(&self, vm: &SourceVm, login: Option<(&str, &str)>) -> Result<String, String> {
        self.ensure()?;
        let (user, password) = login.ok_or("VMware runs it in the VM with an account of the VM's own: give one.")?;
        let st = self.st();
        if st.guest_ops.is_empty() {
            return Err("This VMware can't run programs in its VMs (no guest operations).".into());
        }
        let ops = self.props("GuestOperationsManager", &st.guest_ops, &["processManager", "fileManager"])?;
        let (pm, fm) = (ops.text("processManager"), ops.text("fileManager"));
        let windows = vm.family == Family::Windows;
        let (program, args) = if windows { prep::windows_program() } else { prep::linux_program() };
        let auth = Self::auth(user, password);
        let pid = self.call(&format!(
            "<StartProgramInGuest xmlns=\"urn:vim25\"><_this type=\"GuestProcessManager\">{}</_this><vm type=\"VirtualMachine\">{}</vm>{}<spec xsi:type=\"GuestProgramSpec\"><programPath>{}</programPath><arguments>{}</arguments></spec></StartProgramInGuest>",
            esc(&pm),
            esc(&vm.id),
            auth,
            esc(program),
            esc(&args)
        ))
        .map_err(|e| if e.contains("InvalidGuestLogin") || e.contains("Failed to authenticate") { "The VM's user name or password isn't right.".to_string() } else { e })?;
        let pid = roxmltree::Document::parse(&pid).ok().and_then(|d| first(d.root_element(), "returnval").and_then(|n| n.text()).map(|t| t.trim().to_string())).ok_or("VMware didn't start it in the VM.")?;
        // wait till it ends (rebuilding a Linux boot image can take a few minutes)
        let started = Instant::now();
        let mut exit = None;
        while started.elapsed() < Duration::from_secs(1200) {
            std::thread::sleep(Duration::from_secs(3));
            let x = self.call(&format!("<ListProcessesInGuest xmlns=\"urn:vim25\"><_this type=\"GuestProcessManager\">{}</_this><vm type=\"VirtualMachine\">{}</vm>{}<pids>{}</pids></ListProcessesInGuest>", esc(&pm), esc(&vm.id), auth, esc(&pid)))?;
            if x.contains("<endTime>") {
                exit = Some(x.split("<exitCode>").nth(1).and_then(|v| v.split('<').next()).unwrap_or("").trim().to_string());
                break;
            }
        }
        let Some(code) = exit else { return Err("It's still running in the VM after 20 minutes.".into()) };
        // what it printed
        let log = if windows { prep::WINDOWS_LOG } else { prep::LINUX_LOG };
        let printed = self
            .call(&format!("<InitiateFileTransferFromGuest xmlns=\"urn:vim25\"><_this type=\"GuestFileManager\">{}</_this><vm type=\"VirtualMachine\">{}</vm>{}<guestFilePath>{}</guestFilePath></InitiateFileTransferFromGuest>", esc(&fm), esc(&vm.id), auth, esc(log)))
            .ok()
            .and_then(|x| x.split("<url>").nth(1).and_then(|u| u.split('<').next()).map(|u| u.replace("&amp;", "&")))
            .and_then(|u| {
                // its host is "*": the ESXi the VM runs on (or the one connected to)
                let host = st.vm_places.iter().find(|p| p.0 == vm.id).map(|p| p.2.clone()).filter(|h| !h.is_empty() && st.vcenter);
                let here = self.base.trim_start_matches("https://").to_string();
                let u = u.replace("https://*", &format!("https://{}", host.unwrap_or(here)));
                self.agent.get(&u).call().ok()?.into_string().ok()
            })
            .unwrap_or_default();
        if printed.trim().is_empty() {
            return if code == "0" { Ok(prep::DONE.to_string()) } else { Err(format!("It ended with code {} in the VM.", code)) };
        }
        Ok(printed)
    }

    fn open_disk(&self, _vm: &SourceVm, d: &SourceDisk) -> Result<DiskData, String> {
        let DiskAt::Http { url } = &d.at else { return Err("Not a VMware disk.".into()) };
        self.ensure()?;
        let mut req = self.agent.get(url).set("Authorization", &self.basic());
        if let Some(c) = self.st().cookie {
            req = req.set("Cookie", &c);
        }
        let r = req.call().map_err(|e| http_error(e, "Reading the disk from VMware"))?;
        let len = r.header("Content-Length").and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(d.size);
        if len != d.size {
            return Err(format!("VMware sends {} bytes for a disk of {}: is it still a flat file?", len, d.size));
        }
        Ok(DiskData::Stream { reader: Box::new(r.into_reader()), len })
    }

    fn helper_read(&self, _vm: &SourceVm, d: &SourceDisk) -> Option<HelperRead> {
        let DiskAt::Http { url } = &d.at else { return None };
        Some(HelperRead { job: serde_json::json!({ "kind": "http", "url": url, "user": self.user, "size": d.size }), password: self.password.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A RetrievePropertiesEx answer as ESXi 8 writes it (shortened).
    const ANSWER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<soapenv:Envelope xmlns:soapenc="http://schemas.xmlsoap.org/soap/encoding/" xmlns:soapenv="http://schemas.xmlsoap.org/soap/envelope/" xmlns:xsd="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
<soapenv:Body><RetrievePropertiesExResponse xmlns="urn:vim25"><returnval>
<objects><obj type="VirtualMachine">12</obj>
<propSet><name>config.bootOptions.bootOrder</name><val xsi:type="ArrayOfVirtualMachineBootOptionsBootableDevice"><VirtualMachineBootOptionsBootableDevice xsi:type="VirtualMachineBootOptionsBootableDiskDevice"><deviceKey>2000</deviceKey></VirtualMachineBootOptionsBootableDevice></val></propSet>
<propSet><name>config.firmware</name><val xsi:type="xsd:string">efi</val></propSet>
<propSet><name>config.bootOptions.efiSecureBootEnabled</name><val xsi:type="xsd:boolean">true</val></propSet>
<propSet><name>config.guestFullName</name><val xsi:type="xsd:string">Microsoft Windows Server 2022 (64-bit)</val></propSet>
<propSet><name>config.guestId</name><val xsi:type="xsd:string">windows2019srvNext_64Guest</val></propSet>
<propSet><name>config.hardware.device</name><val xsi:type="ArrayOfVirtualDevice">
<VirtualDevice xsi:type="VirtualLsiLogicSASController"><key>1000</key><deviceInfo><label>SCSI controller 0</label></deviceInfo><busNumber>0</busNumber></VirtualDevice>
<VirtualDevice xsi:type="VirtualDisk"><key>2001</key><deviceInfo><label>Hard disk 2</label></deviceInfo><backing xsi:type="VirtualDiskFlatVer2BackingInfo"><fileName>[datastore1] dc01/dc01_1.vmdk</fileName><datastore type="Datastore">ds1</datastore><diskMode>persistent</diskMode><thinProvisioned>true</thinProvisioned></backing><controllerKey>1000</controllerKey><unitNumber>1</unitNumber><capacityInKB>104857600</capacityInKB><capacityInBytes>107374182400</capacityInBytes></VirtualDevice>
<VirtualDevice xsi:type="VirtualDisk"><key>2000</key><deviceInfo><label>Hard disk 1</label></deviceInfo><backing xsi:type="VirtualDiskFlatVer2BackingInfo"><fileName>[datastore1] dc01/dc01.vmdk</fileName><thinProvisioned>true</thinProvisioned></backing><controllerKey>1000</controllerKey><unitNumber>0</unitNumber><capacityInKB>62914560</capacityInKB><capacityInBytes>64424509440</capacityInBytes></VirtualDevice>
<VirtualDevice xsi:type="VirtualVmxnet3"><key>4000</key><deviceInfo><label>Network adapter 1</label></deviceInfo><backing xsi:type="VirtualEthernetCardNetworkBackingInfo"><deviceName>IoT</deviceName></backing><macAddress>00:50:56:9a:01:02</macAddress></VirtualDevice>
<VirtualDevice xsi:type="VirtualTPM"><key>11000</key><deviceInfo><label>Virtual TPM</label></deviceInfo></VirtualDevice>
</val></propSet>
<propSet><name>config.hardware.memoryMB</name><val xsi:type="xsd:int">8192</val></propSet>
<propSet><name>config.hardware.numCPU</name><val xsi:type="xsd:int">4</val></propSet>
<propSet><name>config.hardware.numCoresPerSocket</name><val xsi:type="xsd:int">2</val></propSet>
<propSet><name>config.template</name><val xsi:type="xsd:boolean">false</val></propSet>
<propSet><name>guest.toolsRunningStatus</name><val xsi:type="xsd:string">guestToolsRunning</val></propSet>
<propSet><name>layoutEx.disk</name><val xsi:type="ArrayOfVirtualMachineFileLayoutExDiskLayout">
<VirtualMachineFileLayoutExDiskLayout><key>2000</key><chain><fileKey>4</fileKey><fileKey>5</fileKey></chain></VirtualMachineFileLayoutExDiskLayout>
<VirtualMachineFileLayoutExDiskLayout><key>2001</key><chain><fileKey>6</fileKey><fileKey>7</fileKey></chain></VirtualMachineFileLayoutExDiskLayout>
</val></propSet>
<propSet><name>layoutEx.file</name><val xsi:type="ArrayOfVirtualMachineFileLayoutExFileInfo">
<VirtualMachineFileLayoutExFileInfo><key>4</key><name>[datastore1] dc01/dc01.vmdk</name><type>diskDescriptor</type><size>600</size></VirtualMachineFileLayoutExFileInfo>
<VirtualMachineFileLayoutExFileInfo><key>5</key><name>[datastore1] dc01/dc01-flat.vmdk</name><type>diskExtent</type><size>40802189312</size></VirtualMachineFileLayoutExFileInfo>
<VirtualMachineFileLayoutExFileInfo><key>6</key><name>[datastore1] dc01/dc01_1.vmdk</name><type>diskDescriptor</type><size>600</size></VirtualMachineFileLayoutExFileInfo>
<VirtualMachineFileLayoutExFileInfo><key>7</key><name>[datastore1] dc01/dc01_1-flat.vmdk</name><type>diskExtent</type><size>21474836480</size></VirtualMachineFileLayoutExFileInfo>
</val></propSet>
<propSet><name>name</name><val xsi:type="xsd:string">dc01</val></propSet>
<propSet><name>runtime.host</name><val type="HostSystem" xsi:type="ManagedObjectReference">ha-host</val></propSet>
<propSet><name>runtime.powerState</name><val xsi:type="VirtualMachinePowerState">poweredOn</val></propSet>
</objects>
<token>abc</token>
</returnval></RetrievePropertiesExResponse></soapenv:Body></soapenv:Envelope>"#;

    #[test]
    fn a_vm_from_esxi() {
        let (objs, token) = parse_objects(ANSWER).unwrap();
        assert_eq!(token.as_deref(), Some("abc"));
        assert_eq!(objs.len(), 1);
        let vmw = Vmware::new("esxi-01.lan", "root", "x");
        let url = |p: &str| vmw.file_url(p, "ha-datacenter");
        let v = vm_from(&objs[0], "ha-datacenter", "esxi-01.lan", &[("IoT".into(), 20)], &[], &url).unwrap();
        assert_eq!(v.id, "12");
        assert_eq!(v.name, "dc01");
        assert_eq!((v.cpus, v.cores, v.memory), (4, 2, 8 << 30));
        assert!(v.uefi && v.secure_boot && v.vtpm);
        assert_eq!(v.power, Power::On);
        assert!(v.agent);
        assert!(v.blocked.is_none(), "{:?}", v.blocked);
        // the boot disk first, whatever its order in the list
        assert_eq!(v.disks.len(), 2);
        assert!(v.disks[0].label.starts_with("Hard disk 1"));
        assert_eq!(v.disks[0].size, 64424509440);
        assert_eq!(v.disks[0].used, 40802189312);
        assert_eq!(v.disks[0].at, DiskAt::Http { url: "https://esxi-01.lan/folder/dc01/dc01-flat.vmdk?dcPath=ha-datacenter&dsName=datastore1".into() });
        assert_eq!(v.nics, vec![SourceNic { network: "IoT".into(), vlan: Some(20), mac: "00:50:56:9a:01:02".into() }]);
        assert!(os_words(&v.os).starts_with("Windows Server 2022"));
    }

    #[test]
    fn snapshots_block_it() {
        // a disk whose chain has two links: a snapshot's delta on top
        let x = ANSWER.replace("<chain><fileKey>4</fileKey><fileKey>5</fileKey></chain>", "<chain><fileKey>4</fileKey><fileKey>5</fileKey></chain><chain><fileKey>8</fileKey><fileKey>9</fileKey></chain>");
        let (objs, _) = parse_objects(&x).unwrap();
        let url = |_: &str| Some("u".to_string());
        let v = vm_from(&objs[0], "dc", "h", &[], &[], &url).unwrap();
        assert!(v.blocked.unwrap().contains("snapshots"));
    }

    #[test]
    fn escapes() {
        assert_eq!(esc("a<b>&\"'"), "a&lt;b&gt;&amp;&quot;&apos;");
    }
}
