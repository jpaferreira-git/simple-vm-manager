//! The demo's pretend sources: a VMware ESXi and a Proxmox server with a few VMs, so the page,
//! the Convert dialog and a conversion can be tried (and pictured) without either.

use super::*;
use std::sync::Mutex;
use std::time::Duration;

pub struct DemoSource {
    vmware: bool,
    vms: Mutex<Vec<SourceVm>>,
}

const GB: u64 = 1 << 30;

fn disk(label: &str, size: u64, used: u64) -> SourceDisk {
    SourceDisk { label: label.into(), size, used, at: DiskAt::Demo }
}

fn nic(network: &str, vlan: Option<i64>, mac: &str) -> SourceNic {
    SourceNic { network: network.into(), vlan, mac: mac.into() }
}

#[allow(clippy::too_many_arguments)]
fn vm(id: &str, name: &str, os: &str, cpus: i64, cores: i64, mem_gb: u64, uefi: bool, secure: bool, vtpm: bool, power: Power, disks: Vec<SourceDisk>, nics: Vec<SourceNic>) -> SourceVm {
    SourceVm {
        id: id.into(),
        name: name.into(),
        os: os.into(),
        family: family_of(os),
        cpus,
        cores,
        memory: mem_gb * GB,
        uefi,
        secure_boot: secure,
        vtpm,
        disks,
        nics,
        agent: power == Power::On,
        power,
        blocked: None,
        note: None,
        node: String::new(),
    }
}

impl DemoSource {
    pub fn vmware() -> DemoSource {
        let mut backup = vm("vm-15", "backup-old", "windows9Server64Guest Microsoft Windows Server 2016 (64-bit)", 2, 1, 6, false, false, false, Power::Off, vec![disk("Hard disk 1", 900 * GB, 410 * GB)], vec![nic("VM Network", None, "00:50:56:9a:00:15")]);
        backup.blocked = Some("It's encrypted (VM encryption): decrypt it in VMware first.".into());
        let vms = vec![
            vm(
                "vm-12",
                "dc01",
                "windows2019srvNext_64Guest Microsoft Windows Server 2022 (64-bit)",
                4,
                2,
                8,
                true,
                true,
                false,
                Power::On,
                vec![disk("Hard disk 1 (dc01.vmdk)", 60 * GB, 41 * GB), disk("Hard disk 2 (dc01_1.vmdk)", 100 * GB, 20 * GB)],
                vec![nic("VM Network", None, "00:50:56:9a:01:02")],
            ),
            vm("vm-13", "nextcloud", "ubuntu64Guest Ubuntu Linux (64-bit)", 2, 1, 4, false, false, false, Power::On, vec![disk("Hard disk 1 (nextcloud.vmdk)", 100 * GB, 34 * GB)], vec![nic("VM Network", None, "00:50:56:9a:01:13"), nic("IoT", Some(20), "00:50:56:9a:02:13")]),
            vm("vm-14", "win11-lab", "windows11_64Guest Microsoft Windows 11 (64-bit)", 4, 4, 8, true, true, true, Power::Off, vec![disk("Hard disk 1 (win11-lab.vmdk)", 128 * GB, 40 * GB)], vec![nic("VM Network", None, "00:50:56:9a:01:14")]),
            backup,
            vm("vm-16", "grafana", "rhel9_64Guest Rocky Linux (64-bit)", 2, 1, 2, true, false, false, Power::Off, vec![disk("Hard disk 1 (grafana.vmdk)", 40 * GB, 9 * GB)], vec![nic("VM Network", None, "00:50:56:9a:01:16")]),
        ];
        DemoSource { vmware: true, vms: Mutex::new(vms) }
    }

    pub fn proxmox() -> DemoSource {
        let vms = vec![
            vm("pve/101", "pihole", "l26 Debian GNU/Linux 12 (bookworm)", 1, 1, 1, false, false, false, Power::Off, vec![disk("scsi0 (local-lvm:vm-101-disk-0)", 16 * GB, 3 * GB)], vec![nic("vmbr0", None, "bc:24:11:00:01:01")]),
            vm("pve/102", "homebridge", "l26 Ubuntu 24.04.1 LTS", 2, 2, 2, true, false, false, Power::On, vec![disk("scsi0 (local-lvm:vm-102-disk-1)", 32 * GB, 7 * GB)], vec![nic("vmbr0", None, "bc:24:11:00:01:02")]),
            vm("pve/103", "plex", "l26 Debian GNU/Linux 12 (bookworm)", 4, 4, 8, false, false, false, Power::On, vec![disk("scsi0 (local-lvm:vm-103-disk-0)", 64 * GB, 22 * GB), disk("scsi1 (local-lvm:vm-103-disk-1)", 500 * GB, 180 * GB)], vec![nic("vmbr0", Some(30), "bc:24:11:00:01:03")]),
            vm("pve/104", "win10-games", "win10", 6, 6, 16, true, true, true, Power::Off, vec![disk("sata0 (local-lvm:vm-104-disk-0)", 256 * GB, 140 * GB)], vec![nic("vmbr0", None, "bc:24:11:00:01:04")]),
        ];
        DemoSource { vmware: false, vms: Mutex::new(vms) }
    }
}

impl Source for DemoSource {
    fn describe(&self) -> String {
        if self.vmware { "VMware ESXi 8.0.2 (demo)".into() } else { "Proxmox VE 8.2.4 (demo)".into() }
    }
    fn list(&self) -> Result<Vec<SourceVm>, String> {
        std::thread::sleep(Duration::from_millis(if cfg!(test) { 0 } else { 600 }));
        Ok(self.vms.lock().unwrap().clone())
    }
    fn shutdown(&self, vm: &SourceVm, say: &dyn Fn(String)) -> Result<(), String> {
        say(format!("Shutting it down on {}…", if self.vmware { "VMware" } else { "Proxmox" }));
        std::thread::sleep(Duration::from_millis(if cfg!(test) { 0 } else { 2500 }));
        for v in self.vms.lock().unwrap().iter_mut().filter(|v| v.id == vm.id) {
            v.power = Power::Off;
            v.agent = false;
        }
        Ok(())
    }
    fn prepare(&self, vm: &SourceVm, _login: Option<(&str, &str)>) -> Result<String, String> {
        std::thread::sleep(Duration::from_millis(if cfg!(test) { 0 } else { 2000 }));
        Ok(if vm.family == Family::Windows {
            "intelide: starts at boot\npciide: starts at boot\natapi: starts at boot\nstorahci: starts at boot\nstornvme: starts at boot\nSVM-PREPARED\n".into()
        } else {
            "card ens192 keeps its name (00:50:56:9a:01:13)\nrebuilding the boot images (update-initramfs)...\ndrivers in the boot image: xen-blkfront xen-netfront\nSVM-PREPARED\n".into()
        })
    }
    fn open_disk(&self, _vm: &SourceVm, d: &SourceDisk) -> Result<DiskData, String> {
        Ok(DiskData::Image(Box::new(DemoDisk { size: d.size, used: d.used.max(disk::BLOCK) })))
    }
    fn helper_read(&self, _vm: &SourceVm, d: &SourceDisk) -> Option<HelperRead> {
        Some(HelperRead { job: serde_json::json!({ "kind": "demo", "size": d.size }), password: String::new() })
    }
}

/// A pretend disk: written in its first `used` bytes, read at about 400 MB/s.
struct DemoDisk {
    size: u64,
    used: u64,
}

impl disk::Disk for DemoDisk {
    fn size(&self) -> u64 {
        self.size
    }
    fn map(&mut self) -> std::io::Result<Vec<bool>> {
        let n = self.size.div_ceil(disk::BLOCK) as usize;
        let used = self.used.div_ceil(disk::BLOCK) as usize;
        Ok((0..n).map(|i| i < used).collect())
    }
    fn read(&mut self, _off: u64, buf: &mut [u8]) -> std::io::Result<()> {
        buf.fill(0);
        if !cfg!(test) {
            std::thread::sleep(Duration::from_micros(buf.len() as u64 / 400));
        }
        Ok(())
    }
    fn kind(&self) -> String {
        "demo".into()
    }
}
