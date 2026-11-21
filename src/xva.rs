//! .xva files, XenServer's own for a whole VM (a tar archive: its settings in ova.xml first, then
//! its disks' blocks): what one holds, read from its ova.xml before it's imported; one written
//! for the demo; and the Export dialog that saves one.

use crate::app::{dialog_button, dialog_buttons, dialog_window, field_label, text_field, Look, LABEL_WIDTH};
use crate::model::fmt_bytes;
use crate::theme::Pal;
use eframe::egui;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// What a .xva holds, as its ova.xml says.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct XvaInfo {
    pub name: String,
    pub description: String,
    /// A template, or a snapshot (which comes in as a template).
    pub template: bool,
    /// Its system, when the file says (the guest tools' report).
    pub os: String,
    pub vcpus: i64,
    pub memory: i64,
    /// Its disks' sizes.
    pub disks: Vec<i64>,
    /// What's written on them (0 when the file doesn't say).
    pub used: i64,
    /// Its network cards' networks, by name.
    pub nets: Vec<String>,
    /// How the file is compressed: "gzip", "zstd", or not at all.
    pub compressed: Option<&'static str>,
    /// The file's size.
    pub size: u64,
}

const NOT_XVA: &str = "It isn't a VM exported from XenServer or XCP-ng (a .xva file).";

/// What the .xva at `path` holds (its ova.xml read, compressed or not), or why it can't be read.
pub fn read_header(path: &Path) -> Result<XvaInfo, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("Couldn't open it: {}", e))?;
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut magic = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match f.read(&mut magic[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) => return Err(format!("Couldn't read it: {}", e)),
        }
    }
    f.seek(SeekFrom::Start(0)).map_err(|e| format!("Couldn't read it: {}", e))?;
    let f = BufReader::new(f);
    let (compressed, xml) = if got >= 2 && magic[..2] == [0x1f, 0x8b] {
        (Some("gzip"), ova_xml(flate2::read::GzDecoder::new(f))?)
    } else if got == 4 && magic == [0x28, 0xb5, 0x2f, 0xfd] {
        let z = ruzstd::StreamingDecoder::new(f).map_err(|_| NOT_XVA.to_string())?;
        (Some("zstd"), ova_xml(z)?)
    } else {
        (None, ova_xml(f)?)
    };
    let mut info = parse_ova(&xml)?;
    (info.compressed, info.size) = (compressed, size);
    Ok(info)
}

/// The archive's first file, which must be ova.xml.
fn ova_xml(r: impl Read) -> Result<String, String> {
    let mut a = tar::Archive::new(r);
    let mut entries = a.entries().map_err(|_| NOT_XVA.to_string())?;
    let mut e = entries.next().ok_or(NOT_XVA)?.map_err(|_| NOT_XVA.to_string())?;
    let name = e.path().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    if name.to_lowercase().ends_with(".ovf") {
        return Err("It's an OVF appliance (.ova), not a .xva: this app brings in .xva files only.".into());
    }
    if name != "ova.xml" {
        return Err(NOT_XVA.into());
    }
    let mut s = String::new();
    e.by_ref().take(64 << 20).read_to_string(&mut s).map_err(|_| NOT_XVA.to_string())?;
    Ok(s)
}

/// An ova.xml's VM: its name, size, disks and network cards. The file is an XML-RPC value: a
/// struct whose "objects" are records (class, id, snapshot), linked by their ids ("Ref:3").
pub fn parse_ova(xml: &str) -> Result<XvaInfo, String> {
    let bad = || "What it says about its VM couldn't be read (its ova.xml).".to_string();
    let mut p = Xml { s: xml, i: 0 };
    p.prolog();
    let root = p.value().map_err(|_| bad())?;
    let objects = root.get("objects").and_then(Value::as_array).ok_or_else(bad)?;
    let mut by_id: HashMap<&str, (&str, &Value)> = HashMap::new();
    for o in objects {
        if let (Some(id), Some(class), Some(rec)) = (o.get("id").and_then(Value::as_str), o.get("class").and_then(Value::as_str), o.get("snapshot")) {
            by_id.insert(id, (class, rec));
        }
    }
    let vm = objects
        .iter()
        .filter(|o| o.get("class").and_then(Value::as_str) == Some("VM"))
        .filter_map(|o| o.get("snapshot"))
        .find(|r| !flag(r, "is_control_domain"))
        .ok_or_else(bad)?;
    let refs = |r: &Value, k: &str| -> Vec<String> { r.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default() };
    let obj = |id: &str, class: &str| by_id.get(id).filter(|(c, _)| *c == class).map(|(_, r)| *r);
    let mut info = XvaInfo {
        name: text(vm, "name_label"),
        description: text(vm, "name_description"),
        template: flag(vm, "is_a_template") || flag(vm, "is_a_snapshot"),
        vcpus: number(vm, "VCPUs_at_startup").max(1),
        memory: number(vm, "memory_dynamic_max").max(number(vm, "memory_static_max").min(number(vm, "memory_dynamic_max").max(1))),
        ..Default::default()
    };
    if info.memory <= 1 {
        info.memory = number(vm, "memory_static_max");
    }
    for vbd in refs(vm, "VBDs").iter().filter_map(|r| obj(r, "VBD")) {
        if text(vbd, "type") != "Disk" {
            continue;
        }
        if let Some(vdi) = obj(&text(vbd, "VDI"), "VDI") {
            info.disks.push(number(vdi, "virtual_size"));
            info.used += number(vdi, "physical_utilisation");
        }
    }
    let mut vifs: Vec<(i64, String)> = refs(vm, "VIFs")
        .iter()
        .filter_map(|r| obj(r, "VIF"))
        .map(|vif| (text(vif, "device").parse().unwrap_or(0), obj(&text(vif, "network"), "network").map(|n| text(n, "name_label")).unwrap_or_default()))
        .collect();
    vifs.sort();
    info.nets = vifs.into_iter().map(|v| v.1).collect();
    if let Some(m) = obj(&text(vm, "guest_metrics"), "VM_guest_metrics") {
        info.os = m.get("os_version").and_then(|o| o.get("name")).and_then(Value::as_str).unwrap_or("").split('|').next().unwrap_or("").trim().to_string();
    }
    Ok(info)
}

fn text(r: &Value, k: &str) -> String {
    match r.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn number(r: &Value, k: &str) -> i64 {
    match r.get(k) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<f64>().map(|f| f as i64).unwrap_or(0),
        _ => 0,
    }
}

fn flag(r: &Value, k: &str) -> bool {
    match r.get(k) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true" || s == "1",
        _ => false,
    }
}

/// Reads an XML-RPC value: <value> holding text (a string), a typed scalar, an <array> or a
/// <struct>. Enough for ova.xml, which has nothing else.
struct Xml<'a> {
    s: &'a str,
    i: usize,
}

impl Xml<'_> {
    fn ws(&mut self) {
        while let Some(c) = self.s[self.i..].chars().next().filter(|c| c.is_whitespace()) {
            self.i += c.len_utf8();
        }
    }
    /// An <?xml …?> line at the top, if there is one.
    fn prolog(&mut self) {
        self.ws();
        if self.s[self.i..].starts_with("<?") {
            if let Some(end) = self.s[self.i..].find("?>") {
                self.i += end + 2;
            }
        }
    }
    fn eat(&mut self, t: &str) -> bool {
        self.ws();
        if self.s[self.i..].starts_with(t) {
            self.i += t.len();
            true
        } else {
            false
        }
    }
    fn need(&mut self, t: &str) -> Result<(), String> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(format!("{} expected at {}", t, self.i))
        }
    }
    /// The text up to the next tag, its entities read.
    fn text(&mut self) -> String {
        let end = self.s[self.i..].find('<').map_or(self.s.len(), |k| self.i + k);
        let t = unescape(&self.s[self.i..end]);
        self.i = end;
        t
    }
    fn value(&mut self) -> Result<Value, String> {
        if self.eat("<value/>") {
            return Ok(Value::String(String::new()));
        }
        self.need("<value>")?;
        let bare = self.text();
        if self.s[self.i..].starts_with("</value>") {
            self.i += "</value>".len();
            return Ok(Value::String(bare));
        }
        let v = if self.eat("<struct/>") {
            Value::Object(Map::new())
        } else if self.eat("<struct>") {
            let mut m = Map::new();
            while self.eat("<member>") {
                self.need("<name>")?;
                let k = self.text();
                self.need("</name>")?;
                let v = self.value()?;
                self.need("</member>")?;
                m.insert(k, v);
            }
            self.need("</struct>")?;
            Value::Object(m)
        } else if self.eat("<array>") {
            let mut a = vec![];
            if !self.eat("<data/>") {
                self.need("<data>")?;
                while !self.eat("</data>") {
                    a.push(self.value()?);
                }
            }
            self.need("</array>")?;
            Value::Array(a)
        } else {
            // a typed scalar: <string>, <boolean>, <int>, <double>… (or its empty form)
            self.ws();
            let rest = &self.s[self.i..];
            if !rest.starts_with('<') {
                return Err(format!("a value expected at {}", self.i));
            }
            let close = rest.find('>').ok_or("an unclosed tag")?;
            let tag = rest[1..close].to_string();
            self.i += close + 1;
            match tag.strip_suffix('/') {
                Some(t) => scalar(t, ""),
                None => {
                    let t = self.text();
                    self.need(&format!("</{}>", tag))?;
                    scalar(&tag, &t)
                }
            }
        };
        self.need("</value>")?;
        Ok(v)
    }
}

fn scalar(tag: &str, text: &str) -> Value {
    match tag {
        "boolean" => Value::Bool(text.trim() == "1"),
        "int" | "i4" | "i8" => text.trim().parse::<i64>().map(Value::from).unwrap_or_else(|_| Value::String(text.into())),
        "double" => text.trim().parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(Value::Number).unwrap_or(Value::Null),
        _ => Value::String(text.to_string()),
    }
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..end];
        let c = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if name.starts_with("#x") => u32::from_str_radix(&name[2..], 16).ok().and_then(char::from_u32),
            _ if name.starts_with('#') => name[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A value as XML-RPC, numbers as strings (as XenServer writes its records).
fn to_xml(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            out.push_str("<value><struct>");
            for (k, v) in m {
                out.push_str("<member><name>");
                out.push_str(&escape(k));
                out.push_str("</name>");
                to_xml(v, out);
                out.push_str("</member>");
            }
            out.push_str("</struct></value>");
        }
        Value::Array(a) => {
            out.push_str("<value><array><data>");
            for v in a {
                to_xml(v, out);
            }
            out.push_str("</data></array></value>");
        }
        Value::Bool(b) => out.push_str(if *b { "<value><boolean>1</boolean></value>" } else { "<value><boolean>0</boolean></value>" }),
        Value::Number(n) => out.push_str(&format!("<value>{}</value>", n)),
        Value::String(s) => out.push_str(&format!("<value>{}</value>", escape(s))),
        Value::Null => out.push_str("<value/>"),
    }
}

/// An ova.xml saying what `info` says: a VM, its disks (as VBDs and VDIs), its cards and their
/// networks, and its system.
pub fn ova_xml_for(info: &XvaInfo) -> String {
    let mut objects = vec![];
    let id = |n: usize| format!("Ref:{}", n);
    let vbds: Vec<String> = (0..info.disks.len()).map(|i| id(10 + i)).collect();
    let vifs: Vec<String> = (0..info.nets.len()).map(|i| id(100 + i)).collect();
    objects.push(json!({ "class": "VM", "id": id(0), "snapshot": {
        "name_label": info.name, "name_description": info.description,
        "is_a_template": info.template, "is_a_snapshot": false, "is_control_domain": false,
        "VCPUs_at_startup": info.vcpus.to_string(), "VCPUs_max": info.vcpus.to_string(),
        "memory_static_max": info.memory.to_string(), "memory_dynamic_max": info.memory.to_string(),
        "memory_dynamic_min": info.memory.to_string(), "memory_static_min": info.memory.to_string(),
        "VBDs": vbds, "VIFs": vifs, "guest_metrics": id(1),
    }}));
    objects.push(json!({ "class": "VM_guest_metrics", "id": id(1), "snapshot": { "os_version": { "name": info.os } } }));
    let per_disk = if info.disks.is_empty() { 0 } else { info.used / info.disks.len() as i64 };
    for (i, size) in info.disks.iter().enumerate() {
        objects.push(json!({ "class": "VBD", "id": id(10 + i), "snapshot": { "type": "Disk", "VDI": id(1000 + i), "userdevice": i.to_string() } }));
        objects.push(json!({ "class": "VDI", "id": id(1000 + i), "snapshot": { "virtual_size": size.to_string(), "physical_utilisation": per_disk.to_string() } }));
    }
    for (i, net) in info.nets.iter().enumerate() {
        objects.push(json!({ "class": "VIF", "id": id(100 + i), "snapshot": { "device": i.to_string(), "network": id(2000 + i) } }));
        objects.push(json!({ "class": "network", "id": id(2000 + i), "snapshot": { "name_label": net } }));
    }
    let root = json!({ "version": { "hostname": "demo", "xapi_major": "25", "xapi_minor": "0", "export_vsn": "2" }, "objects": objects });
    let mut out = String::new();
    to_xml(&root, &mut out);
    out
}

/// Writes a .xva holding only `info`'s ova.xml (the demo's, which has no disks to carry). Its size.
pub fn write_xva(path: &Path, info: &XvaInfo) -> std::io::Result<u64> {
    let xml = ova_xml_for(info);
    let f = std::fs::File::create(path)?;
    let mut b = tar::Builder::new(f);
    let mut h = tar::Header::new_gnu();
    h.set_size(xml.len() as u64);
    h.set_mode(0o644);
    h.set_mtime(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
    h.set_cksum();
    b.append_data(&mut h, "ova.xml", xml.as_bytes())?;
    b.into_inner()?.sync_all()?;
    Ok(std::fs::metadata(path)?.len())
}

/// The folder new files go in: Downloads, else the home folder.
pub(crate) fn downloads() -> PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let d = home.join("Downloads");
    if d.is_dir() {
        d
    } else {
        home
    }
}

/// A file name from a VM's name: what a file system won't take replaced, ".xva" after it.
pub fn file_name(name: &str) -> String {
    let clean: String = name.chars().map(|c| if "/\\:*?\"<>|".contains(c) || c.is_control() { '-' } else { c }).collect();
    let clean = clean.trim().trim_matches('.').trim();
    format!("{}.xva", if clean.is_empty() { "VM" } else { clean })
}

/// Where a VM named `name` is saved by default: in Downloads, as its name, numbered when that's taken.
pub fn default_path(name: &str) -> PathBuf {
    let dir = downloads();
    let file = file_name(name);
    let mut p = dir.join(&file);
    let stem = file.trim_end_matches(".xva").to_string();
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{} ({}).xva", stem, n));
        n += 1;
    }
    p
}

/// A path as it's shown: under the home folder, as "~/…" (not on Windows).
pub(crate) fn tilde(p: &Path) -> String {
    let full = p.display().to_string();
    match std::env::var("HOME") {
        Ok(h) if !cfg!(windows) && !h.is_empty() && full.starts_with(&format!("{}/", h.trim_end_matches('/'))) => format!("~{}", &full[h.trim_end_matches('/').len()..]),
        _ => full,
    }
}

/// A path as typed: "~/…" read as the home folder.
pub(crate) fn expand(raw: &str) -> PathBuf {
    match raw.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest),
        None => PathBuf::from(raw),
    }
}

/// The room left on the disk holding `dir`.
#[cfg(unix)]
pub(crate) fn free_space(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid C string and a statvfs to fill
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(not(unix))]
pub(crate) fn free_space(_dir: &Path) -> Option<u64> {
    None
}

/// What's exported.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Exported {
    Vm,
    Template,
    Snapshot,
}

pub enum ExportAction {
    None,
    Close,
    /// Open the system's Save dialog.
    Browse,
    /// Save it at this path.
    Export(PathBuf),
}

/// The Export dialog: where on this computer, and whether the server compresses it.
pub struct ExportDialog {
    pub sid: u32,
    /// What's saved (a VM, a template or a snapshot), and the VM or template its job is about.
    pub obj: String,
    pub owner: String,
    pub kind: Exported,
    pub name: String,
    /// A snapshot's VM's name.
    pub of: String,
    /// It's running: saved from a snapshot taken for it.
    pub running: bool,
    pub path: String,
    pub compress: bool,
    /// Its disks' size.
    disks: i64,
    /// The room where it goes, as last looked at (by folder).
    room: Option<(PathBuf, Option<u64>)>,
}

impl ExportDialog {
    #[allow(clippy::too_many_arguments)]
    pub fn new(sid: u32, obj: String, owner: String, kind: Exported, name: String, of: String, running: bool, disks: i64) -> ExportDialog {
        let file_as = if kind == Exported::Snapshot { format!("{} – {}", of, name) } else { name.clone() };
        let path = tilde(&default_path(&file_as));
        ExportDialog { sid, obj, owner, kind, name, of, running, path, compress: true, disks, room: None }
    }

    /// The path chosen in the Save dialog.
    pub fn chose(&mut self, p: PathBuf) {
        let p = if p.extension().map_or(false, |e| e.eq_ignore_ascii_case("xva")) { p } else { p.with_file_name(format!("{}.xva", p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())) };
        self.path = tilde(&p);
    }

    /// The file it goes in (".xva" added when missing, "~" read as the home folder), or why it can't.
    fn target(&self) -> Result<PathBuf, String> {
        let raw = self.path.trim();
        if raw.is_empty() {
            return Err(String::new());
        }
        let p = expand(raw);
        let p = if p.extension().map_or(false, |e| e.eq_ignore_ascii_case("xva")) { p } else { PathBuf::from(format!("{}.xva", p.display())) };
        if p.is_dir() {
            return Err("That's a folder: add the file's name.".into());
        }
        match p.parent().filter(|d| !d.as_os_str().is_empty()) {
            Some(d) if d.is_dir() => Ok(p),
            Some(_) => Err("That folder doesn't exist.".into()),
            None => Err("Type the whole path, or use Browse.".into()),
        }
    }

    /// The folder Browse opens in, and the name it suggests.
    pub fn browse_start(&self) -> (PathBuf, String) {
        let p = expand(self.path.trim());
        let dir = p.parent().filter(|d| d.is_dir()).map(Path::to_path_buf).unwrap_or_else(downloads);
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| file_name(&self.name));
        (dir, name)
    }

    pub fn ui(&mut self, ctx: &egui::Context, pal: &Pal) -> ExportAction {
        let mut action = ExportAction::None;
        let title = match self.kind {
            Exported::Vm => "Export VM",
            Exported::Template => "Export Template",
            Exported::Snapshot => "Export Snapshot",
        };
        let target = self.target();
        if let Ok(p) = &target {
            let dir = p.parent().map(Path::to_path_buf).unwrap_or_default();
            if self.room.as_ref().map_or(true, |(d, _)| *d != dir) {
                self.room = Some((dir.clone(), free_space(&dir)));
            }
        }
        dialog_window(ctx, title).id(egui::Id::new("export-dialog")).show(ctx, |ui| {
            ui.set_width(500.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            let what = match self.kind {
                Exported::Vm => format!("Saves '{}' in one file on this computer: its settings and its disks. New VM, from a file, brings it back, in this pool or another.", self.name),
                Exported::Template => format!("Saves the template '{}' in one file on this computer, to make VMs from it in another pool too (New VM, from a file).", self.name),
                Exported::Snapshot => format!("Saves the snapshot '{}' of '{}' in one file on this computer. Brought back (New VM, from a file), it comes in as a template.", self.name, self.of),
            };
            ui.add(egui::Label::new(egui::RichText::new(what).color(pal.ink_soft)).wrap());
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                field_label(ui, "Save as:", LABEL_WIDTH);
                let w = ui.available_width() - 108.0;
                text_field(ui, pal, &mut self.path, w, false);
                if dialog_button(ui, pal, "folder", "Browse…", Look::Plain, true) {
                    action = ExportAction::Browse;
                }
            });
            let indent = LABEL_WIDTH + ui.spacing().item_spacing.x;
            ui.horizontal(|ui| {
                ui.add_space(indent);
                crate::app::tick(ui, pal, &mut self.compress, "Compress it");
                ui.label(egui::RichText::new("smaller, made on the server as it goes").size(12.0).color(pal.ink_soft));
            });
            let mut facts = vec![format!("Its disks: {} (the file holds only what's written on them)", fmt_bytes(self.disks))];
            if let Some((_, Some(free))) = &self.room {
                facts.push(format!("{} free there", fmt_bytes(*free as i64)));
            }
            ui.horizontal(|ui| {
                ui.add_space(indent);
                ui.add(egui::Label::new(egui::RichText::new(facts.join("  ·  ")).size(12.0).color(pal.ink_soft)).wrap());
            });
            match &target {
                Err(why) if !why.is_empty() => {
                    ui.add(egui::Label::new(egui::RichText::new(why.as_str()).size(12.5).color(pal.red)).wrap());
                }
                Ok(p) if p.exists() => {
                    crate::newvm::alert(ui, pal, false, "There's a file by that name.", "Exporting replaces it.", None);
                }
                _ => {}
            }
            if self.running {
                let text = format!("It keeps running: the app takes a snapshot of '{}', saves that, then deletes the snapshot. The file's disks are as after a power cut (what the VM hadn't yet written isn't in it), as a VM switched off at the plug.", self.name);
                crate::newvm::alert(ui, pal, false, "It's running.", &text, None);
            }
            ui.add_space(4.0);
            dialog_buttons(ui, |ui| {
                if dialog_button(ui, pal, "close", "Cancel", Look::Plain, true) {
                    action = ExportAction::Close;
                }
                let ready = target.is_ok();
                if dialog_button(ui, pal, "download", "Export", Look::Primary, ready) || (ready && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                    if let Ok(p) = &target {
                        action = ExportAction::Export(p.clone());
                    }
                }
            });
        });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            action = ExportAction::Close;
        }
        action
    }
}

/// The New VM dialog's summary of a file: its system, size, disks and cards, in one line.
pub fn summary(info: &XvaInfo) -> String {
    let mut parts = vec![];
    if !info.os.is_empty() {
        parts.push(info.os.clone());
    }
    parts.push(format!("{} CPU{}", info.vcpus, if info.vcpus == 1 { "" } else { "s" }));
    parts.push(format!("{} memory", fmt_bytes(info.memory)));
    parts.push(match info.disks.len() {
        0 => "no disks".to_string(),
        1 => format!("a {} disk", fmt_bytes(info.disks[0])),
        n => format!("{} disks, {}", n, fmt_bytes(info.disks.iter().sum())),
    });
    parts.push(match info.nets.len() {
        0 => "no network".to_string(),
        1 => "a network card".to_string(),
        n => format!("{} network cards", n),
    });
    parts.join("  ·  ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> XvaInfo {
        XvaInfo {
            name: "Web & <Mail>".into(),
            description: "Front end".into(),
            template: false,
            os: "Ubuntu 24.04 LTS".into(),
            vcpus: 2,
            memory: 4 << 30,
            disks: vec![40 << 30, 8 << 30],
            used: 12 << 30,
            nets: vec!["Pool-wide network 0".into(), "Storage network".into()],
            ..Default::default()
        }
    }

    #[test]
    fn ova_xml_reads_back() {
        let info = sample();
        let back = parse_ova(&ova_xml_for(&info)).unwrap();
        assert_eq!(back, info);
    }

    #[test]
    fn reads_xenserver_style_records() {
        // as XenServer writes them: bare strings, typed booleans, empty values, a prolog
        let xml = r#"<?xml version="1.0"?><value><struct><member><name>objects</name><value><array><data>
            <value><struct><member><name>class</name><value>VM</value></member><member><name>id</name><value>Ref:0</value></member>
            <member><name>snapshot</name><value><struct>
              <member><name>name_label</name><value>Home Assistant</value></member>
              <member><name>name_description</name><value/></member>
              <member><name>is_a_template</name><value><boolean>0</boolean></value></member>
              <member><name>VCPUs_at_startup</name><value>2</value></member>
              <member><name>memory_dynamic_max</name><value>4294967296</value></member>
              <member><name>memory_static_max</name><value>4294967296</value></member>
              <member><name>VBDs</name><value><array><data><value>Ref:1</value><value>Ref:2</value></data></array></value></member>
              <member><name>VIFs</name><value><array><data/></array></value></member>
              <member><name>other_config</name><value><struct/></value></member>
            </struct></value></member></struct></value>
            <value><struct><member><name>class</name><value>VBD</value></member><member><name>id</name><value>Ref:1</value></member>
              <member><name>snapshot</name><value><struct><member><name>type</name><value>Disk</value></member><member><name>VDI</name><value>Ref:3</value></member></struct></value></member></struct></value>
            <value><struct><member><name>class</name><value>VBD</value></member><member><name>id</name><value>Ref:2</value></member>
              <member><name>snapshot</name><value><struct><member><name>type</name><value>CD</value></member><member><name>VDI</name><value>OpaqueRef:NULL</value></member></struct></value></member></struct></value>
            <value><struct><member><name>class</name><value>VDI</value></member><member><name>id</name><value>Ref:3</value></member>
              <member><name>snapshot</name><value><struct><member><name>virtual_size</name><value>34359738368</value></member><member><name>physical_utilisation</name><value><string>1000</string></value></member></struct></value></member></struct></value>
            </data></array></value></member></struct></value>"#;
        let info = parse_ova(xml).unwrap();
        assert_eq!(info.name, "Home Assistant");
        assert_eq!((info.vcpus, info.memory), (2, 4 << 30));
        assert_eq!((info.disks.clone(), info.used), (vec![32 << 30], 1000));
        assert!(info.nets.is_empty() && !info.template);
    }

    #[test]
    fn written_file_reads_back_and_others_dont() {
        let dir = std::env::temp_dir().join(format!("svm-xva-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.xva");
        let size = write_xva(&p, &sample()).unwrap();
        let info = read_header(&p).unwrap();
        assert_eq!((info.name.as_str(), info.size, info.compressed), ("Web & <Mail>", size, None));
        // gzip, as old exports are
        let gz = dir.join("b.xva");
        let mut e = flate2::write::GzEncoder::new(std::fs::File::create(&gz).unwrap(), flate2::Compression::fast());
        std::io::copy(&mut std::fs::File::open(&p).unwrap(), &mut e).unwrap();
        e.finish().unwrap();
        assert_eq!(read_header(&gz).unwrap().compressed, Some("gzip"));
        let junk = dir.join("c.xva");
        std::fs::write(&junk, b"not an archive at all").unwrap();
        assert!(read_header(&junk).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_zstd() {
        // a .xva compressed as XenServer compresses an export (zstd), made with the zstd tool
        let p = std::env::temp_dir().join(format!("svm-xva-zstd-{}.xva", std::process::id()));
        std::fs::write(&p, include_bytes!("../testdata/zstd.xva")).unwrap();
        let info = read_header(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!((info.name.as_str(), info.compressed, info.vcpus, info.disks.len()), ("Home Assistant", Some("zstd"), 2, 1));
    }

    #[test]
    fn home_shown_as_tilde() {
        let Ok(home) = std::env::var("HOME") else { return };
        let p = PathBuf::from(&home).join("Downloads").join("a.xva");
        if cfg!(windows) {
            return;
        }
        assert_eq!(tilde(&p), "~/Downloads/a.xva");
        assert_eq!(expand("~/Downloads/a.xva"), p);
        assert_eq!(tilde(Path::new("/srv/x.xva")), "/srv/x.xva");
    }

    #[test]
    fn file_names() {
        assert_eq!(file_name("Web Server"), "Web Server.xva");
        assert_eq!(file_name("a/b: c?"), "a-b- c-.xva");
        assert_eq!(file_name(" .. "), "VM.xva");
    }
}
