//! cloud-init for a new VM: what it's told (hostname, a user with a password or SSH keys, or
//! user-data of your own), on a small disk labelled CIDATA that cloud-init's NoCloud source reads
//! on the VM's first start (as Xen Orchestra does it).

/// What the New VM dialog asks for.
#[derive(Clone, Debug, Default)]
pub struct CloudInit {
    pub hostname: String,
    pub user: String,
    pub password: String,
    /// SSH public keys, one a line.
    pub keys: String,
    /// User-data typed in by hand, used as it is.
    pub custom: Option<String>,
}

/// The disk's size: 4 MiB, the smallest LVM storage gives anyway.
pub const DRIVE_SIZE: usize = 4 << 20;

/// A YAML string (a JSON string is one).
fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

impl CloudInit {
    pub fn keys(&self) -> Vec<String> {
        self.keys.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(String::from).collect()
    }

    /// The #cloud-config: its hostname, and the user (sudo; a password, SSH keys or both).
    pub fn user_data(&self) -> String {
        if let Some(text) = &self.custom {
            return text.clone();
        }
        let mut y = String::from("#cloud-config\n");
        y.push_str(&format!("hostname: {}\nmanage_etc_hosts: true\n", q(&self.hostname)));
        if !self.user.trim().is_empty() {
            let pw = !self.password.is_empty();
            y.push_str("users:\n");
            y.push_str(&format!("  - name: {}\n    shell: /bin/bash\n    sudo: {}\n    lock_passwd: {}\n", q(self.user.trim()), q(if pw { "ALL=(ALL) ALL" } else { "ALL=(ALL) NOPASSWD:ALL" }), !pw));
            if pw {
                y.push_str(&format!("    plain_text_passwd: {}\n", q(&self.password)));
            }
            let keys = self.keys();
            if !keys.is_empty() {
                y.push_str("    ssh_authorized_keys:\n");
                for k in keys {
                    y.push_str(&format!("      - {}\n", q(&k)));
                }
            }
            if pw {
                y.push_str("ssh_pwauth: true\n");
            }
        }
        y
    }

    /// Its meta-data: an instance-id of its own (so cloud-init runs again on a copy of a VM that
    /// had it) and its hostname.
    pub fn meta_data(&self, instance: &str) -> String {
        format!("instance-id: {}\nlocal-hostname: {}\n", q(instance), q(&self.hostname))
    }

    /// What stops it being used, if anything.
    pub fn problem(&self) -> Option<&'static str> {
        if self.custom.as_deref().map_or(false, |t| t.trim().is_empty()) {
            return Some("Cloud-init: type the user-data, or pick Hostname and user.");
        }
        if self.custom.is_some() {
            return None;
        }
        if !crate::ipconf::valid_hostname(self.hostname.trim()) {
            return Some("Cloud-init: give it a hostname (letters, digits and hyphens).");
        }
        let user = self.user.trim();
        if !user.is_empty() {
            let ok = user.len() <= 32 && user.starts_with(|c: char| c.is_ascii_lowercase() || c == '_') && user.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
            if !ok {
                return Some("Cloud-init: a user name is lower case letters, digits, - and _.");
            }
            if self.password.is_empty() && self.keys().is_empty() {
                return Some("Cloud-init: give the user a password or an SSH key.");
            }
        }
        None
    }
}

/// A hostname from a VM's name: "Web Server (2)" → "web-server-2".
pub fn hostname_for(name: &str) -> String {
    let mut out = String::new();
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out: String = out.trim_end_matches('-').chars().take(63).collect();
    if out.is_empty() { "vm".into() } else { out.trim_end_matches('-').to_string() }
}

// ---------------------------------------------------------------------------------------------
// The disk: FAT12, label CIDATA, the files in its root with their long names

const SECTOR: usize = 512;
const PER_CLUSTER: usize = 4;
const RESERVED: usize = 1;
const ROOT_ENTRIES: usize = 64;
const FAT_SECTORS: usize = 6;

/// A FAT12 file system of DRIVE_SIZE bytes with `files` in its root.
pub fn config_drive(files: &[(&str, &[u8])], serial: u32) -> Vec<u8> {
    let total = DRIVE_SIZE / SECTOR;
    let mut img = vec![0u8; DRIVE_SIZE];
    // the boot sector
    let b = &mut img[..SECTOR];
    b[..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    b[3..11].copy_from_slice(b"MSWIN4.1");
    b[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    b[13] = PER_CLUSTER as u8;
    b[14..16].copy_from_slice(&(RESERVED as u16).to_le_bytes());
    b[16] = 2;
    b[17..19].copy_from_slice(&(ROOT_ENTRIES as u16).to_le_bytes());
    b[19..21].copy_from_slice(&(total as u16).to_le_bytes());
    b[21] = 0xF8;
    b[22..24].copy_from_slice(&(FAT_SECTORS as u16).to_le_bytes());
    b[24..26].copy_from_slice(&32u16.to_le_bytes());
    b[26..28].copy_from_slice(&64u16.to_le_bytes());
    b[36] = 0x80;
    b[38] = 0x29;
    b[39..43].copy_from_slice(&serial.to_le_bytes());
    b[43..54].copy_from_slice(b"CIDATA     ");
    b[54..62].copy_from_slice(b"FAT12   ");
    b[510] = 0x55;
    b[511] = 0xAA;
    // the files' clusters, one after another from cluster 2
    let fat_at = RESERVED * SECTOR;
    let root_at = (RESERVED + 2 * FAT_SECTORS) * SECTOR;
    let data_at = root_at + ROOT_ENTRIES * 32;
    let mut fat = vec![0u8; FAT_SECTORS * SECTOR];
    let mut set = |n: usize, v: u16| {
        let off = n * 3 / 2;
        if n % 2 == 0 {
            fat[off] = v as u8;
            fat[off + 1] = (fat[off + 1] & 0xF0) | ((v >> 8) as u8 & 0x0F);
        } else {
            fat[off] = (fat[off] & 0x0F) | ((v << 4) as u8);
            fat[off + 1] = (v >> 4) as u8;
        }
    };
    set(0, 0xFF8);
    set(1, 0xFFF);
    let mut root: Vec<[u8; 32]> = vec![];
    let mut label = [0u8; 32];
    label[..11].copy_from_slice(b"CIDATA     ");
    label[11] = 0x08;
    root.push(label);
    let mut next = 2usize;
    for (i, (name, data)) in files.iter().enumerate() {
        let short = short_name(name, i + 1);
        root.extend(long_entries(name, &short));
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(&short);
        e[11] = 0x20;
        // 1 Jan 2026, midnight
        let date: u16 = ((2026 - 1980) << 9) | (1 << 5) | 1;
        for at in [16, 18, 24] {
            e[at..at + 2].copy_from_slice(&date.to_le_bytes());
        }
        let clusters = data.len().div_ceil(PER_CLUSTER * SECTOR);
        if clusters > 0 {
            e[26..28].copy_from_slice(&(next as u16).to_le_bytes());
            for c in 0..clusters {
                set(next + c, if c + 1 == clusters { 0xFFF } else { (next + c + 1) as u16 });
            }
            let at = data_at + (next - 2) * PER_CLUSTER * SECTOR;
            img[at..at + data.len()].copy_from_slice(data);
            next += clusters;
        }
        e[28..32].copy_from_slice(&(data.len() as u32).to_le_bytes());
        root.push(e);
    }
    for copy in 0..2 {
        let at = fat_at + copy * FAT_SECTORS * SECTOR;
        img[at..at + fat.len()].copy_from_slice(&fat);
    }
    for (i, e) in root.iter().enumerate() {
        img[root_at + i * 32..root_at + i * 32 + 32].copy_from_slice(e);
    }
    img
}

/// An 8.3 name for a long one: "user-data" → "USER-D~1".
fn short_name(name: &str, n: usize) -> [u8; 11] {
    let mut s = [b' '; 11];
    let base: Vec<u8> = name.to_ascii_uppercase().bytes().filter(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_').collect();
    let tail = format!("~{}", n);
    let keep = 8 - tail.len();
    for (i, c) in base.iter().take(keep).chain(tail.as_bytes().iter()).enumerate() {
        s[i] = *c;
    }
    s
}

/// The long-name entries before a file's own: 13 characters each, the last part first.
fn long_entries(name: &str, short: &[u8; 11]) -> Vec<[u8; 32]> {
    let sum = short.iter().fold(0u8, |s, c| (s >> 1).wrapping_add(s << 7).wrapping_add(*c));
    let mut units: Vec<u16> = name.encode_utf16().collect();
    units.push(0);
    while units.len() % 13 != 0 {
        units.push(0xFFFF);
    }
    let parts: Vec<&[u16]> = units.chunks(13).collect();
    let mut out = vec![];
    for (i, part) in parts.iter().enumerate().rev() {
        let mut e = [0u8; 32];
        e[0] = (i + 1) as u8 | if i + 1 == parts.len() { 0x40 } else { 0 };
        e[11] = 0x0F;
        e[13] = sum;
        let at = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
        for (k, u) in part.iter().enumerate() {
            e[at[k]..at[k] + 2].copy_from_slice(&u.to_le_bytes());
        }
        out.push(e);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_data_written() {
        let c = CloudInit { hostname: "web-2".into(), user: "joao".into(), password: "p\"w".into(), keys: "ssh-ed25519 AAAA joao@laptop\n\n".into(), custom: None };
        let y = c.user_data();
        assert!(y.starts_with("#cloud-config\nhostname: \"web-2\"\n"));
        assert!(y.contains("  - name: \"joao\"\n") && y.contains("    plain_text_passwd: \"p\\\"w\"\n") && y.contains("      - \"ssh-ed25519 AAAA joao@laptop\"\n"));
        assert!(y.contains("lock_passwd: false") && y.contains("ssh_pwauth: true") && y.contains("\"ALL=(ALL) ALL\""));
        // keys only: no password to log in with, sudo without one
        let k = CloudInit { password: String::new(), ..c.clone() };
        assert!(k.user_data().contains("lock_passwd: true") && k.user_data().contains("NOPASSWD") && !k.user_data().contains("ssh_pwauth"));
        assert_eq!(c.meta_data("i-1"), "instance-id: \"i-1\"\nlocal-hostname: \"web-2\"\n");
        assert_eq!(CloudInit { custom: Some("#cloud-config\n".into()), ..c.clone() }.user_data(), "#cloud-config\n");
    }

    #[test]
    fn what_stops_it() {
        let ok = CloudInit { hostname: "web-2".into(), user: "joao".into(), password: "x".into(), ..Default::default() };
        assert_eq!(ok.problem(), None);
        assert!(CloudInit { hostname: "web 2".into(), ..ok.clone() }.problem().unwrap().contains("hostname"));
        assert!(CloudInit { user: "João".into(), ..ok.clone() }.problem().unwrap().contains("user name"));
        assert!(CloudInit { password: String::new(), ..ok.clone() }.problem().unwrap().contains("password or an SSH key"));
        assert_eq!(CloudInit { user: String::new(), password: String::new(), ..ok.clone() }.problem(), None);
        assert_eq!(hostname_for("Web Server (2)"), "web-server-2");
        assert_eq!(hostname_for("Ubuntu 24.04"), "ubuntu-24-04");
        assert_eq!(hostname_for("ÉÉ"), "vm");
    }

    #[test]
    fn drive_laid_out() {
        let img = config_drive(&[("user-data", b"#cloud-config\n"), ("meta-data", b"instance-id: x\n")], 0x1234_5678);
        assert_eq!(img.len(), DRIVE_SIZE);
        assert_eq!(&img[510..512], &[0x55, 0xAA]);
        assert_eq!(&img[43..54], b"CIDATA     ");
        assert_eq!(&short_name("user-data", 1), b"USER-D~1   ");
        assert_eq!(&short_name("network-config", 3), b"NETWOR~3   ");
        // the root: the label, then each file's long name (one entry) and its own entry
        let root = (RESERVED + 2 * FAT_SECTORS) * SECTOR;
        assert_eq!(img[root + 11], 0x08);
        assert_eq!((img[root + 32], img[root + 32 + 11]), (0x41, 0x0F));
        assert_eq!(&img[root + 64..root + 75], b"USER-D~1   ");
        // the first file's data in cluster 2, the second's in cluster 3
        let data = root + ROOT_ENTRIES * 32;
        assert!(img[data..].starts_with(b"#cloud-config\n"));
        assert!(img[data + PER_CLUSTER * SECTOR..].starts_with(b"instance-id: x\n"));
    }
}
