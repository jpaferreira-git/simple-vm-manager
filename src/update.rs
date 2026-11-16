//! Updates: whether GitHub has a newer release than this copy, and putting it in this copy's place.
//! The release's Linux archive holds the program; it's written beside this one, then renamed over
//! it (Linux lets a running program's file be replaced), so a failure leaves the old one as it was.
//! The window then starts the new one and closes.

use crate::conn::{web_agent, web_get, Counting, Transfer};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub const REPO: &str = "jpaferreira-git/simple-vm-manager";
/// The program's name, in the archive and on disk.
const PROGRAM: &str = "simple-vm-manager";
/// The release file holding this platform's program; None where there isn't one (then the release
/// page is offered instead).
pub const ASSET: Option<&str> = if cfg!(all(target_os = "linux", target_arch = "x86_64")) { Some("simple-vm-manager-linux-x86_64.tar.gz") } else { None };

/// A release on GitHub: its version ("1.9.0"), its page, and this platform's file (link, size).
#[derive(Clone, Debug, Default)]
pub struct Release {
    pub version: String,
    pub page: String,
    pub download: Option<(String, u64)>,
}

/// The latest release.
pub fn latest() -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", REPO);
    let answer = web_agent()?.get(&url).set("Accept", "application/vnd.github+json").call();
    let text = match answer {
        Ok(r) => r.into_string().map_err(|e| e.to_string())?,
        Err(ureq::Error::Status(404, _)) => return Err("GitHub has no release of this app yet.".into()),
        Err(ureq::Error::Status(403 | 429, _)) => return Err("GitHub is asking to wait (too many checks from this network); it'll work later.".into()),
        Err(ureq::Error::Status(code, _)) => return Err(format!("GitHub answered with HTTP {}.", code)),
        Err(e) => {
            return Err(match e.kind() {
                ureq::ErrorKind::Dns | ureq::ErrorKind::ConnectionFailed => "Couldn't reach GitHub: is this computer online?".into(),
                _ => e.to_string(),
            })
        }
    };
    parse_release(&text)
}

/// A release from GitHub's answer (its JSON).
pub fn parse_release(json: &str) -> Result<Release, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|_| "GitHub's answer couldn't be read.".to_string())?;
    let tag = v["tag_name"].as_str().ok_or("GitHub's answer has no version in it.")?;
    let download = ASSET.and_then(|name| {
        let a = v["assets"].as_array()?.iter().find(|a| a["name"].as_str() == Some(name))?;
        Some((a["browser_download_url"].as_str()?.to_string(), a["size"].as_u64().unwrap_or(0)))
    });
    Ok(Release { version: tag.trim_start_matches(['v', 'V']).to_string(), page: v["html_url"].as_str().unwrap_or_default().to_string(), download })
}

/// Whether version `a` comes after `b`: "1.10.0" after "1.9.2", "1.9" the same as "1.9.0".
pub fn newer(a: &str, b: &str) -> bool {
    let parts = |s: &str| {
        let mut v: Vec<u64> = s.trim().trim_start_matches(['v', 'V']).split('.').map(|p| p.chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0)).collect();
        v.resize(v.len().max(3), 0);
        v
    };
    parts(a) > parts(b)
}

/// This copy's program file, when it can be replaced where it is: a folder this user may write to
/// (not /usr/bin, where the package manager put it) and not a build of the source (…/target/…).
pub fn replaceable() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    if exe.components().any(|c| c.as_os_str() == "target") {
        return None;
    }
    let probe = exe.parent()?.join(format!(".{}.write-test", PROGRAM));
    std::fs::OpenOptions::new().write(true).create_new(true).open(&probe).ok()?;
    let _ = std::fs::remove_file(&probe);
    Some(exe)
}

/// Downloads the release's archive (`size` bytes, counted in `t`; `t.cancel` stops it) and puts its
/// program in place of `target`.
pub fn install(url: &str, size: u64, target: &Path, t: &Arc<Transfer>) -> Result<(), String> {
    t.total.store(size, Ordering::Relaxed);
    let body = web_get(url)?.into_reader();
    let counted = Counting { inner: Box::new(body), t: t.clone() };
    let r = unpack(counted, target);
    if t.cancel.load(Ordering::Relaxed) {
        return Err("Cancelled.".into());
    }
    r?;
    // (the archive's last small files aren't needed: the bar shows it whole)
    t.sent.store(size.max(t.sent.load(Ordering::Relaxed)), Ordering::Relaxed);
    t.finishing.store(true, Ordering::Relaxed);
    Ok(())
}

/// Takes the program out of a .tar.gz and puts it in place of `target`: written beside it, checked
/// to be a program, made runnable, then renamed over it.
pub fn unpack(archive: impl Read, target: &Path) -> Result<(), String> {
    let dir = target.parent().ok_or("There's no folder to put it in.")?;
    let tmp = dir.join(format!(".{}.new", PROGRAM));
    let fail = |what: &str, e: &dyn std::fmt::Display| format!("{} ({}).", what, e);
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries().map_err(|e| fail("The download couldn't be read", &e))? {
        let mut e = entry.map_err(|e| fail("The download broke off", &e))?;
        let is_program = e.header().entry_type().is_file() && e.path().ok().map_or(false, |p| p.file_name().map_or(false, |n| n == PROGRAM));
        if !is_program {
            continue;
        }
        let written = (|| -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            std::io::copy(&mut e, &mut f)?;
            f.sync_all()
        })();
        if let Err(err) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(fail("The new version couldn't be written", &err));
        }
        // a program for this kind of computer, not a page or a broken file
        let mut head = [0u8; 4];
        let looks_right = std::fs::File::open(&tmp).and_then(|mut f| f.read_exact(&mut head)).is_ok() && (!cfg!(target_os = "linux") || head == *b"\x7fELF");
        if !looks_right {
            let _ = std::fs::remove_file(&tmp);
            return Err("The download isn't a program for this computer.".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
        }
        return std::fs::rename(&tmp, target).map_err(|err| {
            let _ = std::fs::remove_file(&tmp);
            fail("The new version couldn't take the old one's place", &err)
        });
    }
    Err("The download didn't have the program in it.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(newer("1.9.0", "1.8.0"));
        assert!(newer("1.10.0", "1.9.2"));
        assert!(newer("v2.0", "1.99.99"));
        assert!(!newer("1.8.0", "1.8.0"));
        assert!(!newer("1.8", "1.8.0"));
        assert!(!newer("1.7.9", "1.8.0"));
    }

    #[test]
    fn release_from_github() {
        let json = r#"{"tag_name":"v1.9.0","html_url":"https://github.com/x/y/releases/tag/v1.9.0",
            "assets":[{"name":"other.zip","size":1,"browser_download_url":"https://e/other.zip"},
                      {"name":"simple-vm-manager-linux-x86_64.tar.gz","size":9540306,"browser_download_url":"https://e/a.tar.gz"}]}"#;
        let r = parse_release(json).unwrap();
        assert_eq!(r.version, "1.9.0");
        assert!(r.page.ends_with("/v1.9.0"));
        if ASSET.is_some() {
            assert_eq!(r.download, Some(("https://e/a.tar.gz".into(), 9540306)));
        }
        assert!(parse_release("{}").is_err());
    }

    /// An archive like the release's: a folder with the program and other files.
    fn archive(program: &[u8]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut b = tar::Builder::new(gz);
        for (path, data) in [("simple-vm-manager/README.md", &b"read me"[..]), ("simple-vm-manager/simple-vm-manager", program), ("simple-vm-manager/LICENSE", b"MIT")] {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, path, data).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn program_takes_the_old_ones_place() {
        let dir = std::env::temp_dir().join(format!("svm-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("simple-vm-manager");
        std::fs::write(&target, b"old").unwrap();
        let new = b"\x7fELF and the rest of a program";
        unpack(&archive(new)[..], &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), new);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o755);
        }
        // a download that isn't a program leaves the old one
        if cfg!(target_os = "linux") {
            assert!(unpack(&archive(b"<html>not found</html>")[..], &target).is_err());
            assert_eq!(std::fs::read(&target).unwrap(), new);
        }
        assert!(!dir.join(".simple-vm-manager.new").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Against GitHub: the latest release, and its archive unpacked into a scratch file.
    /// cargo test -- --ignored real_release
    #[test]
    #[ignore]
    fn real_release() {
        let r = latest().unwrap();
        assert!(!newer("0.1.0", &r.version), "{:?}", r);
        let (url, size) = r.download.clone().expect("a Linux archive");
        let dir = std::env::temp_dir().join(format!("svm-real-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("simple-vm-manager");
        std::fs::write(&target, b"old").unwrap();
        let t = Arc::new(Transfer::default());
        install(&url, size, &target, &t).unwrap();
        assert!(t.sent.load(Ordering::Relaxed) >= size);
        assert!(std::fs::metadata(&target).unwrap().len() > 1_000_000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
