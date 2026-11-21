//! Disk files on this computer as a source: an .ova (an OVF package in one tar file, as VMware,
//! VirtualBox and others export), an .ovf with its disks next to it, a VMware .vmx (Workstation,
//! Fusion, an ESXi VM's folder copied off), or a lone disk image (.vmdk, .vhd, .vhdx, .qcow2,
//! .vdi, .img).

use super::disk::{self, FileAt, PartAt};
use super::*;
use std::path::Path;
use std::sync::Mutex;

/// The extensions this source opens.
pub const EXTENSIONS: &[&str] = &["ova", "ovf", "vmx", "vmdk", "vhd", "vhdx", "qcow2", "qcow", "vdi", "img", "raw"];

pub fn is_convertible(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).map_or(false, |e| EXTENSIONS.contains(&e.to_lowercase().as_str()))
}

/// The files added, as VMs.
#[derive(Default)]
pub struct Files {
    pub vms: Mutex<Vec<SourceVm>>,
}

impl Files {
    /// Reads a file and adds what it holds (a file added again replaces its VMs).
    pub fn add(&self, path: &Path) -> Result<Vec<SourceVm>, String> {
        let found = read_file(path)?;
        let mut vms = self.vms.lock().unwrap();
        let key = path.display().to_string();
        vms.retain(|v| !v.id.starts_with(&key));
        vms.extend(found.iter().cloned());
        Ok(found)
    }
}

impl Source for Files {
    fn describe(&self) -> String {
        "Disk files on this computer".into()
    }
    fn list(&self) -> Result<Vec<SourceVm>, String> {
        Ok(self.vms.lock().unwrap().clone())
    }
    fn shutdown(&self, _vm: &SourceVm, _say: &dyn Fn(String)) -> Result<(), String> {
        Ok(())
    }
    fn prepare(&self, _vm: &SourceVm, _login: Option<(&str, &str)>) -> Result<String, String> {
        Err("A file can't be prepared: run the command in the VM on its old hypervisor, before exporting it.".into())
    }
    fn open_disk(&self, _vm: &SourceVm, d: &SourceDisk) -> Result<DiskData, String> {
        open_at(&d.at).map(DiskData::Image)
    }
}

/// Opens a disk that's in a file here.
pub fn open_at(at: &DiskAt) -> Result<Box<dyn disk::Disk>, String> {
    match at {
        DiskAt::File(p) => disk::open_path(p).map_err(|e| format!("{}: {}", p.display(), e)),
        DiskAt::InTar { tar, offset, len } => {
            let f = FileAt::open(tar).map_err(|e| format!("{}: {}", tar.display(), e))?;
            disk::open(Box::new(PartAt { inner: Box::new(f), base: *offset, len: *len }), None).map_err(|e| format!("{}: {}", tar.display(), e))
        }
        _ => Err("Not a disk on this computer.".into()),
    }
}

fn file_name(p: &Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

/// What a file holds, as VMs.
pub fn read_file(path: &Path) -> Result<Vec<SourceVm>, String> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    match ext.as_str() {
        "ova" => read_ova(path),
        "ovf" => {
            let xml = std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;
            let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
            parse_ovf(&xml, &path.display().to_string(), &file_name(path), &mut |href| {
                let p = dir.join(href);
                let len = std::fs::metadata(&p).map(|m| m.len()).ok()?;
                Some((DiskAt::File(p), len))
            })
        }
        "vmx" => read_vmx(path).map(|v| vec![v]),
        _ => Ok(vec![lone_disk(path)?]),
    }
}

/// A disk image alone: a VM with that one disk, its settings left to choose (2 CPUs, 4 GB).
fn lone_disk(path: &Path) -> Result<SourceVm, String> {
    let mut d = disk::open_path(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let kind = d.kind();
    // what's written: known from the file's own map, but not for a raw image (it'd be read whole)
    let used = if kind.starts_with("raw") || kind.contains("fixed") || kind.contains("flat") { 0 } else { d.map().map(|m| m.iter().filter(|&&b| b).count() as u64 * disk::BLOCK).unwrap_or(0) };
    let name = file_name(path);
    Ok(SourceVm {
        id: path.display().to_string(),
        name: name.clone(),
        os: String::new(),
        family: family_of(&name),
        cpus: 2,
        cores: 1,
        memory: 4 << 30,
        uefi: false,
        secure_boot: false,
        vtpm: false,
        disks: vec![SourceDisk { label: format!("{} ({})", path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), kind), size: d.size(), used: used.min(d.size()), at: DiskAt::File(path.to_path_buf()) }],
        nics: vec![SourceNic { network: "Network".into(), vlan: None, mac: String::new() }],
        power: Power::Off,
        agent: false,
        blocked: None,
        note: Some("A lone disk: its CPUs, memory and boot mode aren't in the file. Check them before converting.".into()),
        node: String::new(),
    })
}

// ---------------------------------------------------------------- OVA and OVF

/// A tar's members: name, where its data starts, its length (ustar, GNU long names, pax names).
pub fn tar_members(r: &mut dyn disk::ReadAt) -> std::io::Result<Vec<(String, u64, u64)>> {
    let mut out = vec![];
    let mut at = 0u64;
    let mut long: Option<String> = None;
    let mut h = [0u8; 512];
    while at + 512 <= r.len() {
        r.read_at(at, &mut h)?;
        if h.iter().all(|&b| b == 0) {
            break;
        }
        let field = |a: usize, b: usize| String::from_utf8_lossy(&h[a..b]).trim_end_matches('\0').to_string();
        let octal = field(124, 136);
        let size = if h[124] & 0x80 != 0 {
            // base-256, for big files
            h[125..136].iter().fold(0u64, |s, &b| (s << 8) | b as u64)
        } else {
            u64::from_str_radix(octal.trim().trim_matches(char::from(0)), 8).unwrap_or(0)
        };
        let kind = h[156];
        let mut name = field(0, 100);
        // (POSIX's ustar has a prefix there; GNU's "ustar  " has times)
        if &h[257..263] == b"ustar\0" {
            let prefix = field(345, 500);
            if !prefix.is_empty() {
                name = format!("{}/{}", prefix, name);
            }
        }
        let data = at + 512;
        match kind {
            b'L' => {
                let mut b = vec![0u8; size as usize];
                r.read_at(data, &mut b)?;
                long = Some(String::from_utf8_lossy(&b).trim_end_matches('\0').to_string());
            }
            b'x' => {
                let mut b = vec![0u8; size.min(1 << 20) as usize];
                r.read_at(data, &mut b)?;
                for line in String::from_utf8_lossy(&b).lines() {
                    if let Some(p) = line.split_once(' ').map(|x| x.1).and_then(|kv| kv.strip_prefix("path=")) {
                        long = Some(p.to_string());
                    }
                }
            }
            b'0' | 0 | b'7' => out.push((long.take().unwrap_or(name), data, size)),
            _ => {
                long = None;
            }
        }
        at = data + size.div_ceil(512) * 512;
    }
    Ok(out)
}

fn read_ova(path: &Path) -> Result<Vec<SourceVm>, String> {
    let mut f = FileAt::open(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let members = tar_members(&mut f).map_err(|e| format!("{} isn't a valid .ova (a tar file): {}", path.display(), e))?;
    let ovf = match members.iter().find(|(n, _, _)| n.to_lowercase().ends_with(".ovf")) {
        Some((_, pos, size)) => {
            let mut b = vec![0u8; (*size).min(16 << 20) as usize];
            disk::ReadAt::read_at(&mut f, *pos, &mut b).map_err(|e| format!("{}: its .ovf can't be read: {}", path.display(), e))?;
            Some(String::from_utf8_lossy(&b).to_string())
        }
        None => None,
    };
    let xml = ovf.ok_or_else(|| format!("{} has no .ovf descriptor inside.", path.display()))?;
    let tar = path.to_path_buf();
    parse_ovf(&xml, &path.display().to_string(), &file_name(path), &mut |href| {
        let base = href.rsplit('/').next().unwrap_or(href);
        members.iter().find(|(n, _, _)| n == href || n.rsplit('/').next() == Some(base)).map(|(_, pos, size)| (DiskAt::InTar { tar: tar.clone(), offset: *pos, len: *size }, *size))
    })
}

fn local<'a>(n: roxmltree::Node<'a, 'a>) -> &'a str {
    n.tag_name().name()
}

fn attr<'a>(n: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    n.attributes().find(|a| a.name() == name).map(|a| a.value())
}

fn child<'a>(n: roxmltree::Node<'a, 'a>, name: &str) -> Option<roxmltree::Node<'a, 'a>> {
    n.children().find(|c| c.is_element() && local(*c) == name)
}

fn child_text(n: roxmltree::Node, name: &str) -> String {
    child(n, name).and_then(|c| c.text()).unwrap_or("").trim().to_string()
}

/// How many bytes a unit is: "byte * 2^20", "MegaBytes", "GB"…
fn unit_bytes(units: &str, default: u64) -> u64 {
    let u = units.to_lowercase().replace(' ', "");
    if let Some(p) = u.split("2^").nth(1) {
        let n: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
        return 1u64 << n.parse::<u32>().unwrap_or(0).min(60);
    }
    if u.starts_with("kilo") || u == "kb" || u == "kib" {
        1 << 10
    } else if u.starts_with("mega") || u == "mb" || u == "mib" {
        1 << 20
    } else if u.starts_with("giga") || u == "gb" || u == "gib" {
        1 << 30
    } else if u.starts_with("byte") {
        1
    } else {
        default
    }
}

/// The VMs an OVF descriptor describes. `resolve` finds a referenced file: where its bytes are,
/// and its length.
pub fn parse_ovf(xml: &str, id_base: &str, fallback_name: &str, resolve: &mut dyn FnMut(&str) -> Option<(DiskAt, u64)>) -> Result<Vec<SourceVm>, String> {
    let opts = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(xml, opts).map_err(|e| format!("Its .ovf descriptor can't be read: {}", e))?;
    let root = doc.root_element();
    // its files and disks
    let mut files: Vec<(String, String, bool)> = vec![];
    let mut disks: Vec<(String, String, u64, u64)> = vec![];
    for n in root.descendants().filter(|n| n.is_element()) {
        match local(n) {
            "File" => files.push((attr(n, "id").unwrap_or("").into(), attr(n, "href").unwrap_or("").into(), attr(n, "compression").map_or(false, |c| !c.is_empty() && c != "identity"))),
            "Disk" if attr(n, "diskId").is_some() => {
                let cap = attr(n, "capacity").and_then(|c| c.trim().parse::<u64>().ok()).unwrap_or(0);
                let unit = unit_bytes(attr(n, "capacityAllocationUnits").unwrap_or("byte"), 1);
                let used = attr(n, "populatedSize").and_then(|c| c.trim().parse::<u64>().ok()).unwrap_or(0);
                disks.push((attr(n, "diskId").unwrap_or("").into(), attr(n, "fileRef").unwrap_or("").into(), cap * unit, used));
            }
            _ => {}
        }
    }
    let systems: Vec<roxmltree::Node> = root.descendants().filter(|n| n.is_element() && local(*n) == "VirtualSystem").collect();
    if systems.is_empty() {
        return Err("Its .ovf describes no VM.".into());
    }
    let mut out = vec![];
    for (k, sys) in systems.iter().enumerate() {
        let name = {
            let n = child_text(*sys, "Name");
            if !n.is_empty() {
                n
            } else {
                attr(*sys, "id").map(str::to_string).filter(|s| !s.is_empty()).unwrap_or_else(|| fallback_name.to_string())
            }
        };
        let mut os = String::new();
        if let Some(o) = sys.descendants().find(|n| n.is_element() && local(*n) == "OperatingSystemSection") {
            let ty = attr(o, "osType").unwrap_or("");
            let desc = child_text(o, "Description");
            os = format!("{} {}", ty, desc).trim().to_string();
        }
        let (mut cpus, mut cores, mut memory) = (1i64, 1i64, 1u64 << 30);
        let (mut uefi, mut secure, mut vtpm) = (false, false, false);
        let mut vm_disks = vec![];
        let mut nics = vec![];
        let mut blocked = None;
        for n in sys.descendants().filter(|n| n.is_element()) {
            match local(n) {
                "Config" | "ExtraConfig" => {
                    let (key, value) = (attr(n, "key").unwrap_or(""), attr(n, "value").unwrap_or("").to_lowercase());
                    match key {
                        "firmware" => uefi = value == "efi",
                        "uefi.secureBoot.enabled" | "bootOptions.efiSecureBootEnabled" => secure = value == "true",
                        "vtpm.present" => vtpm = value == "true",
                        _ => {}
                    }
                }
                "Item" | "StorageItem" | "EthernetPortItem" => {
                    let ty = child_text(n, "ResourceType");
                    let qty = child_text(n, "VirtualQuantity").parse::<u64>().unwrap_or(0);
                    match ty.as_str() {
                        "3" => {
                            cpus = qty.max(1) as i64;
                            if let Some(c) = n.children().find(|c| c.is_element() && local(*c) == "CoresPerSocket").and_then(|c| c.text()).and_then(|t| t.trim().parse::<i64>().ok()) {
                                cores = c.max(1);
                            }
                        }
                        "4" => memory = qty * unit_bytes(&child_text(n, "AllocationUnits"), 1 << 20),
                        "10" => nics.push(SourceNic { network: child_text(n, "Connection"), vlan: None, mac: norm_mac(&child_text(n, "Address")) }),
                        "17" => {
                            let host = child_text(n, "HostResource");
                            let id = host.rsplit('/').next().unwrap_or("").to_string();
                            let Some((_, file_ref, size, used)) = disks.iter().find(|d| d.0 == id) else { continue };
                            let Some((_, href, compressed)) = files.iter().find(|f| f.0 == *file_ref) else { continue };
                            if *compressed {
                                blocked = Some("Its disks are compressed inside the package (gzip): unpack them first, or export it again without compression.".into());
                                continue;
                            }
                            match resolve(href) {
                                Some((at, _len)) => vm_disks.push(SourceDisk { label: href.clone(), size: *size, used: *used, at }),
                                None => blocked = Some(format!("Its disk file {} isn't there.", href)),
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        if cores <= 0 || cpus % cores != 0 {
            cores = 1;
        }
        if vm_disks.is_empty() && blocked.is_none() {
            blocked = Some("It has no disks.".into());
        }
        let label = if os.is_empty() { name.clone() } else { os.clone() };
        out.push(SourceVm {
            id: if systems.len() == 1 { id_base.to_string() } else { format!("{}#{}", id_base, k) },
            name,
            family: family_of(&label),
            os,
            cpus,
            cores,
            memory,
            uefi,
            secure_boot: secure && uefi,
            vtpm: vtpm && uefi,
            disks: vm_disks,
            nics,
            power: Power::Off,
            agent: false,
            blocked,
            note: None,
            node: String::new(),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------- VMware .vmx

/// A .vmx's settings: key = "value" lines.
pub fn parse_vmx(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.starts_with('#') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_lowercase(), v.trim().trim_matches('"').to_string()))
        })
        .collect()
}

fn read_vmx(path: &Path) -> Result<SourceVm, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    vm_from_vmx(&text, &path.display().to_string(), &file_name(path), &mut |name| {
        let p = if Path::new(name).is_absolute() { PathBuf::from(name) } else { dir.join(name) };
        let mut d = disk::open_path(&p).map_err(|e| e.to_string())?;
        let used = d.map().map(|m| m.iter().filter(|&&b| b).count() as u64 * disk::BLOCK).unwrap_or(0);
        Ok((DiskAt::File(p), d.size(), used.min(d.size())))
    })
}

/// A VM from a .vmx. `disk` opens a disk it names: where it is, its size, what's written.
pub fn vm_from_vmx(text: &str, id: &str, fallback: &str, disk: &mut dyn FnMut(&str) -> Result<(DiskAt, u64, u64), String>) -> Result<SourceVm, String> {
    let kv = parse_vmx(text);
    let get = |k: &str| kv.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    let yes = |k: &str| get(k).map_or(false, |v| v.eq_ignore_ascii_case("true"));
    let name = get("displayname").filter(|n| !n.is_empty()).unwrap_or(fallback).to_string();
    let cpus = get("numvcpus").and_then(|v| v.parse().ok()).unwrap_or(1i64).max(1);
    let mut cores = get("cpuid.corespersocket").and_then(|v| v.parse().ok()).unwrap_or(1i64).max(1);
    if cpus % cores != 0 {
        cores = 1;
    }
    let memory = get("memsize").and_then(|v| v.parse::<u64>().ok()).unwrap_or(1024) << 20;
    let uefi = get("firmware").map_or(false, |v| v.eq_ignore_ascii_case("efi"));
    let os = get("guestos").unwrap_or("").to_string();
    let mut blocked = None;
    if get("encryption.keysafe").is_some() || get("encryption.data").is_some() {
        blocked = Some("It's encrypted: decrypt it in VMware first.".into());
    }
    // its disks, by controller and place: scsi0:0, sata0:1, nvme0:0, ide0:0
    let mut found: Vec<(String, String)> = kv
        .iter()
        .filter_map(|(k, v)| {
            let dev = k.strip_suffix(".filename")?;
            let bus_ok = ["scsi", "sata", "nvme", "ide"].iter().any(|b| dev.starts_with(b)) && dev.contains(':');
            let kind = get(&format!("{}.devicetype", dev)).unwrap_or("").to_lowercase();
            let present = get(&format!("{}.present", dev)).map_or(true, |p| p.eq_ignore_ascii_case("true"));
            (bus_ok && present && !kind.contains("cdrom") && v.to_lowercase().ends_with(".vmdk")).then(|| (dev.to_string(), v.clone()))
        })
        .collect();
    let order = |d: &str| -> (u8, String) {
        let bus = if d.starts_with("scsi") { 0 } else if d.starts_with("nvme") { 1 } else if d.starts_with("sata") { 2 } else { 3 };
        (bus, d.to_string())
    };
    found.sort_by_key(|(d, _)| order(d));
    let mut disks = vec![];
    for (dev, file) in found {
        match disk(&file) {
            Ok((at, size, used)) => disks.push(SourceDisk { label: format!("{} ({})", file, dev), size, used, at }),
            Err(e) => {
                blocked.get_or_insert(e);
            }
        }
    }
    if disks.is_empty() && blocked.is_none() {
        blocked = Some("It has no disks.".into());
    }
    // its network cards
    let mut nics = vec![];
    for i in 0..10 {
        let p = format!("ethernet{}", i);
        if !yes(&format!("{}.present", p)) {
            continue;
        }
        let network = get(&format!("{}.networkname", p)).or(get(&format!("{}.connectiontype", p))).unwrap_or("Network").to_string();
        let mac = get(&format!("{}.address", p)).or(get(&format!("{}.generatedaddress", p))).unwrap_or("");
        nics.push(SourceNic { network, vlan: None, mac: norm_mac(mac) });
    }
    Ok(SourceVm {
        id: id.to_string(),
        name,
        family: family_of(&os),
        os,
        cpus,
        cores,
        memory,
        uefi,
        secure_boot: uefi && yes("uefi.secureboot.enabled"),
        vtpm: uefi && yes("vtpm.present"),
        disks,
        nics,
        power: Power::Off,
        agent: false,
        blocked,
        note: None,
        node: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OVF: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Envelope vmw:buildId="build-1" xmlns="http://schemas.dmtf.org/ovf/envelope/1" xmlns:cim="http://schemas.dmtf.org/wbem/wscim/1/common" xmlns:ovf="http://schemas.dmtf.org/ovf/envelope/1" xmlns:rasd="http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_ResourceAllocationSettingData" xmlns:vmw="http://www.vmware.com/schema/ovf" xmlns:vssd="http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_VirtualSystemSettingData" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <References>
    <File ovf:href="nextcloud-disk1.vmdk" ovf:id="file1" ovf:size="1234"/>
    <File ovf:href="nextcloud-disk2.vmdk" ovf:id="file2" ovf:size="99"/>
  </References>
  <DiskSection>
    <Info>Virtual disk information</Info>
    <Disk ovf:capacity="100" ovf:capacityAllocationUnits="byte * 2^30" ovf:diskId="vmdisk1" ovf:fileRef="file1" ovf:format="http://www.vmware.com/interfaces/specifications/vmdk.html#streamOptimized" ovf:populatedSize="36507222016"/>
    <Disk ovf:capacity="16" ovf:capacityAllocationUnits="byte * 2^30" ovf:diskId="vmdisk2" ovf:fileRef="file2"/>
  </DiskSection>
  <NetworkSection><Info>The list of logical networks</Info><Network ovf:name="VM Network"/><Network ovf:name="IoT"/></NetworkSection>
  <VirtualSystem ovf:id="nextcloud">
    <Info>A virtual machine</Info>
    <Name>nextcloud</Name>
    <OperatingSystemSection ovf:id="94" vmw:osType="ubuntu64Guest"><Info>The kind of installed guest operating system</Info><Description>Ubuntu Linux (64-bit)</Description></OperatingSystemSection>
    <VirtualHardwareSection>
      <Info>Virtual hardware requirements</Info>
      <System><vssd:VirtualSystemType>vmx-19</vssd:VirtualSystemType></System>
      <Item><rasd:AllocationUnits>hertz * 10^6</rasd:AllocationUnits><rasd:ElementName>4 virtual CPU(s)</rasd:ElementName><rasd:InstanceID>1</rasd:InstanceID><rasd:ResourceType>3</rasd:ResourceType><rasd:VirtualQuantity>4</rasd:VirtualQuantity><vmw:CoresPerSocket ovf:required="false">2</vmw:CoresPerSocket></Item>
      <Item><rasd:AllocationUnits>byte * 2^20</rasd:AllocationUnits><rasd:InstanceID>2</rasd:InstanceID><rasd:ResourceType>4</rasd:ResourceType><rasd:VirtualQuantity>4096</rasd:VirtualQuantity></Item>
      <Item><rasd:AddressOnParent>0</rasd:AddressOnParent><rasd:HostResource>ovf:/disk/vmdisk1</rasd:HostResource><rasd:InstanceID>8</rasd:InstanceID><rasd:Parent>4</rasd:Parent><rasd:ResourceType>17</rasd:ResourceType></Item>
      <Item><rasd:AddressOnParent>1</rasd:AddressOnParent><rasd:HostResource>ovf:/disk/vmdisk2</rasd:HostResource><rasd:InstanceID>9</rasd:InstanceID><rasd:Parent>4</rasd:Parent><rasd:ResourceType>17</rasd:ResourceType></Item>
      <Item><rasd:Address>00:50:56:aa:bb:cc</rasd:Address><rasd:AddressOnParent>7</rasd:AddressOnParent><rasd:Connection>VM Network</rasd:Connection><rasd:InstanceID>10</rasd:InstanceID><rasd:ResourceSubType>VmxNet3</rasd:ResourceSubType><rasd:ResourceType>10</rasd:ResourceType></Item>
      <Item><rasd:Connection>IoT</rasd:Connection><rasd:InstanceID>11</rasd:InstanceID><rasd:ResourceType>10</rasd:ResourceType></Item>
      <vmw:Config ovf:required="false" vmw:key="firmware" vmw:value="efi"/>
      <vmw:Config ovf:required="false" vmw:key="uefi.secureBoot.enabled" vmw:value="true"/>
    </VirtualHardwareSection>
  </VirtualSystem>
</Envelope>"#;

    #[test]
    fn an_ovf_from_vmware() {
        let vms = parse_ovf(OVF, "/x/nextcloud.ova", "nextcloud", &mut |href| Some((DiskAt::File(PathBuf::from(href)), 10))).unwrap();
        assert_eq!(vms.len(), 1);
        let v = &vms[0];
        assert_eq!(v.name, "nextcloud");
        assert_eq!((v.cpus, v.cores, v.memory), (4, 2, 4 << 30));
        assert!(v.uefi && v.secure_boot && !v.vtpm);
        assert_eq!(v.family, Family::Linux);
        assert_eq!(v.disks.len(), 2);
        assert_eq!(v.disks[0].size, 100 << 30);
        assert_eq!(v.disks[0].used, 36507222016);
        assert_eq!(v.disks[1].label, "nextcloud-disk2.vmdk");
        assert_eq!(v.nics.len(), 2);
        assert_eq!(v.nics[0].mac, "00:50:56:aa:bb:cc");
        assert_eq!(v.nics[1].network, "IoT");
        assert!(v.blocked.is_none());
        assert!(os_words(&v.os).starts_with("Ubuntu"));
    }

    #[test]
    fn a_vmx_from_workstation() {
        let vmx = r#".encoding = "UTF-8"
displayName = "dc01"
guestOS = "windows2019srvnext-64"
numvcpus = "4"
cpuid.coresPerSocket = "2"
memsize = "8192"
firmware = "efi"
uefi.secureBoot.enabled = "TRUE"
scsi0.present = "TRUE"
scsi0:0.present = "TRUE"
scsi0:0.fileName = "dc01.vmdk"
scsi0:1.present = "TRUE"
scsi0:1.fileName = "dc01_1.vmdk"
sata0:1.deviceType = "cdrom-image"
sata0:1.fileName = "C:\isos\win.iso"
ethernet0.present = "TRUE"
ethernet0.connectionType = "bridged"
ethernet0.generatedAddress = "00:0C:29:12:34:56"
"#;
        let v = vm_from_vmx(vmx, "dc01.vmx", "dc01", &mut |f| Ok((DiskAt::File(PathBuf::from(f)), 60 << 30, 0))).unwrap();
        assert_eq!(v.name, "dc01");
        assert_eq!((v.cpus, v.cores, v.memory), (4, 2, 8 << 30));
        assert!(v.uefi && v.secure_boot);
        assert_eq!(v.family, Family::Windows);
        assert_eq!(v.disks.len(), 2);
        assert!(v.disks[0].label.starts_with("dc01.vmdk"));
        assert_eq!(v.nics[0].mac, "00:0c:29:12:34:56");
        assert_eq!(v.nics[0].network, "bridged");
        assert!(os_words(&v.os).starts_with("Windows Server 2022"));
    }

    #[test]
    fn a_real_ova_from_qemu_disks() {
        // an .ova made here: the descriptor above, with a stream-optimized disk made by qemu-img
        if std::process::Command::new("qemu-img").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("svm-ova-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let raw = dir.join("d.raw");
        let mut data = vec![0u8; 8 << 20];
        data[5 << 20..(5 << 20) + 6].copy_from_slice(b"hello!");
        std::fs::write(&raw, &data).unwrap();
        for (n, size) in [("nextcloud-disk1.vmdk", "8M"), ("nextcloud-disk2.vmdk", "4M")] {
            let out = dir.join(n);
            let src = if n.ends_with("1.vmdk") { raw.clone() } else { let p = dir.join("e.raw"); std::fs::write(&p, vec![0u8; 4 << 20]).unwrap(); p };
            let _ = size;
            assert!(std::process::Command::new("qemu-img").args(["convert", "-q", "-f", "raw", "-O", "vmdk", "-o", "subformat=streamOptimized"]).arg(&src).arg(&out).status().unwrap().success());
        }
        std::fs::write(dir.join("nextcloud.ovf"), OVF).unwrap();
        let ova = dir.join("nextcloud.ova");
        {
            let f = std::fs::File::create(&ova).unwrap();
            let mut b = tar::Builder::new(f);
            // as VMware writes them: plain ustar members, the descriptor first
            for n in ["nextcloud.ovf", "nextcloud-disk1.vmdk", "nextcloud-disk2.vmdk"] {
                let data = std::fs::read(dir.join(n)).unwrap();
                let mut h = tar::Header::new_ustar();
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, n, &data[..]).unwrap();
            }
            b.finish().unwrap();
        }
        let vms = read_file(&ova).unwrap();
        let v = &vms[0];
        assert!(matches!(v.disks[0].at, DiskAt::InTar { .. }));
        let mut d = open_at(&v.disks[0].at).unwrap();
        assert_eq!(d.size(), 8 << 20);
        let mut buf = [0u8; 6];
        d.read(5 << 20, &mut buf).unwrap();
        assert_eq!(&buf, b"hello!");
        assert_eq!(d.map().unwrap().iter().filter(|&&b| b).count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
