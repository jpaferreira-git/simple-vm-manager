//! What's run in a VM before it leaves its old hypervisor, so it starts on XenServer.
//!
//! - **Windows** starts from a disk that XenServer shows it as an IDE (or NVMe) drive until its
//!   XenServer VM Tools are in. A Windows installed on VMware's SCSI or Proxmox's VirtIO has
//!   those drivers set to load only when asked, and stops at a blue screen
//!   (INACCESSIBLE_BOOT_DEVICE, 0x7B). The command sets them to load at boot. Nothing else
//!   changes.
//! - **Linux** needs Xen's disk driver (xen-blkfront) in its boot image (initramfs): the disks
//!   are /dev/xvd* there, and a boot image made just for VMware's or KVM's hardware has no
//!   driver for them. Its network cards also get new names on Xen (ens192 → enX0), and the
//!   network settings written for the old names would no longer fit. So the script pins each
//!   card's name to its MAC (kept on XenServer), adds the drivers and rebuilds the boot image,
//!   and warns about disks named /dev/sd* in /etc/fstab.

/// What both print at the end when all went well.
pub const DONE: &str = "SVM-PREPARED";

/// The drivers turned on at boot (those this Windows has).
const WINDOWS_DRIVERS: &str = "intelide pciide atapi storahci stornvme";

/// What's run in Windows: each driver set to start at boot (those this Windows has), then a check
/// that it took (it doesn't without an administrator's rights). No double quotes inside, so it
/// can go inside `cmd /c "…"` typed in PowerShell too.
pub fn windows_inner() -> String {
    format!(
        "for %s in ({}) do @(reg query HKLM\\SYSTEM\\CurrentControlSet\\Services\\%s >nul 2>&1 && reg add HKLM\\SYSTEM\\CurrentControlSet\\Services\\%s /v Start /t REG_DWORD /d 0 /f >nul && echo %s: starts at boot) & (reg query HKLM\\SYSTEM\\CurrentControlSet\\Services\\pciide /v Start | findstr /c:0x0 >nul && echo {} || echo The drivers did not change: run it as an administrator)",
        WINDOWS_DRIVERS, DONE
    )
}

/// The Windows command: for an administrator's Command Prompt or PowerShell (`cmd /c "…"` works
/// in both).
pub fn windows_command() -> String {
    format!("cmd /c \"{}\"", windows_inner())
}

/// Where Windows writes what it printed, when run through the guest tools.
pub const WINDOWS_LOG: &str = "C:\\Windows\\Temp\\svm-prepare.log";
pub const LINUX_LOG: &str = "/tmp/svm-prepare.log";

/// The same, as the program and its arguments the guest tools run (what it prints goes to
/// `WINDOWS_LOG`).
pub fn windows_program() -> (&'static str, String) {
    ("C:\\Windows\\System32\\cmd.exe", format!("/c \"({}) > {} 2>&1\"", windows_inner(), WINDOWS_LOG))
}

/// The Linux script as the program and arguments the guest tools run (what it prints goes to
/// `LINUX_LOG`).
pub fn linux_program() -> (&'static str, String) {
    ("/bin/sh", format!("-c \"echo {} | base64 -d | sh > {} 2>&1\"", super::base64(LINUX_SCRIPT.as_bytes()), LINUX_LOG))
}

/// The Linux script (POSIX sh, run as root).
pub const LINUX_SCRIPT: &str = r#"#!/bin/sh
# Simple VM Manager: makes this Linux ready to start on XenServer.
[ "$(id -u)" = 0 ] || { echo "Run it as root (with sudo)."; exit 1; }
warn() { echo "WARNING: $*"; }

# 1. Each network card keeps its name on XenServer (its MAC stays the same).
if [ -d /run/systemd/system ]; then
  mkdir -p /etc/systemd/network
  for d in /sys/class/net/*; do
    n=${d##*/}
    [ -e "$d/device" ] || continue
    case "$n" in eth[0-9]*) continue ;; esac
    mac=$(cat "$d/address" 2>/dev/null)
    [ -n "$mac" ] || continue
    printf '[Match]\nMACAddress=%s\n\n[Link]\nName=%s\n' "$mac" "$n" > "/etc/systemd/network/10-svm-$n.link"
    echo "card $n keeps its name ($mac)"
  done
fi

# 2. Xen's disk and network drivers in the boot image (those built as modules).
mods=""
for m in xen-blkfront xen-netfront ata_piix ata_generic; do
  f=$(modinfo -F filename "$m" 2>/dev/null | head -n1)
  case "$f" in *.ko*) mods="$mods $m" ;; esac
done
if command -v dracut >/dev/null 2>&1 && [ -d /etc/dracut.conf.d ]; then
  [ -n "$mods" ] && echo "add_drivers+=\"$mods \"" > /etc/dracut.conf.d/90-svm-xen.conf
  echo "rebuilding the boot images (dracut)..."
  dracut -f --regenerate-all >/dev/null 2>&1 || dracut -f || warn "dracut failed"
elif command -v update-initramfs >/dev/null 2>&1; then
  for m in $mods; do grep -qx "$m" /etc/initramfs-tools/modules 2>/dev/null || echo "$m" >> /etc/initramfs-tools/modules; done
  echo "rebuilding the boot images (update-initramfs)..."
  update-initramfs -u -k all >/dev/null 2>&1 || warn "update-initramfs failed"
elif command -v mkinitcpio >/dev/null 2>&1; then
  mkdir -p /etc/mkinitcpio.conf.d
  [ -n "$mods" ] && echo "MODULES+=($mods)" > /etc/mkinitcpio.conf.d/90-svm-xen.conf
  echo "rebuilding the boot images (mkinitcpio)..."
  mkinitcpio -P >/dev/null 2>&1 || warn "mkinitcpio failed"
else
  warn "no dracut, update-initramfs or mkinitcpio here: add xen-blkfront to its boot image by hand"
fi
echo "drivers in the boot image:${mods:- (built into the kernel)}"

# 3. Disks named by device change name on XenServer (/dev/sda becomes /dev/xvda).
if grep -Eq '^[[:space:]]*/dev/(sd|vd|hd|nvme)' /etc/fstab 2>/dev/null; then
  warn "/etc/fstab names disks by device (/dev/sd...): change those lines to UUID=... (see blkid) or it may not start"
fi
if grep -Eq 'root=/dev/(sd|vd|hd|nvme)' /proc/cmdline 2>/dev/null; then
  warn "the kernel's root= names its disk by device: use root=UUID=... in the boot loader"
fi
echo SVM-PREPARED
"#;

/// The Linux one-liner to paste (the script inside it, so nothing is fetched).
pub fn linux_command() -> String {
    format!("echo {} | base64 -d | sudo sh", super::base64(LINUX_SCRIPT.as_bytes()))
}

/// The lines worth showing from what it printed: its warnings, and whether it finished.
pub fn outcome(printed: &str) -> Result<Vec<String>, String> {
    let warnings: Vec<String> = printed.lines().filter_map(|l| l.trim().strip_prefix("WARNING:").map(|w| w.trim().to_string())).collect();
    if printed.contains(DONE) {
        Ok(warnings)
    } else {
        let last = printed.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        Err(if last.is_empty() { "It didn't finish.".into() } else { format!("It didn't finish: {}", last) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_linux_script_runs_in_sh() {
        // its syntax, as sh reads it
        let ok = std::process::Command::new("sh").args(["-n", "-c", LINUX_SCRIPT]).status().map(|s| s.success()).unwrap_or(true);
        assert!(ok);
        // and the one-liner carries it whole
        let cmd = linux_command();
        let b64 = cmd.strip_prefix("echo ").unwrap().split(' ').next().unwrap();
        assert_eq!(super::super::base64_decode(b64), LINUX_SCRIPT.as_bytes());
    }

    #[test]
    fn the_windows_command_is_one_line() {
        let c = windows_command();
        assert!(!c.contains('\n'));
        assert!(c.starts_with("cmd /c \"for %s in (intelide"));
        assert!(c.contains("echo SVM-PREPARED ||"));
        // no quotes inside it (PowerShell would end the string there)
        assert_eq!(c.matches('"').count(), 2);
        let (prog, args) = windows_program();
        assert!(prog.ends_with("cmd.exe"));
        assert!(args.starts_with("/c \"(for %s"));
        assert!(args.ends_with("svm-prepare.log 2>&1\""));
    }

    #[test]
    fn what_it_printed() {
        assert_eq!(outcome("x\nWARNING: fstab\nSVM-PREPARED\n").unwrap(), vec!["fstab".to_string()]);
        assert!(outcome("Run it as root (with sudo).\n").unwrap_err().contains("root"));
    }
}
