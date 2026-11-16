//! Saved passwords. They live in the system's keychain (Secret Service on Linux, such as GNOME
//! Keyring, KWallet or KeePassXC; Credential Manager on Windows; Keychain on macOS), never in the
//! app's own files. Every call may wait on the keychain (it can ask to be unlocked), so the app
//! makes them from background threads.

const SERVICE: &str = "simple-vm-manager";
/// Where passwords went while the app was briefly called XenCraft: moved back as they're read.
const OLD_SERVICE: &str = "xencraft";

fn entry(host: &str, user: &str) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, &format!("{}@{}", user, host))
}

/// Whether there's a keychain to keep passwords in.
pub fn available() -> bool {
    match entry("keychain-check", "simple-vm-manager").and_then(|e| e.get_password()) {
        Ok(_) | Err(keyring::Error::NoEntry) => true,
        Err(_) => false,
    }
}

pub fn load(host: &str, user: &str) -> Option<String> {
    if let Some(p) = entry(host, user).ok().and_then(|e| e.get_password().ok()) {
        return Some(p);
    }
    // kept under XenCraft, the name the app had for a while: moved back
    let old = keyring::Entry::new(OLD_SERVICE, &format!("{}@{}", user, host)).ok()?;
    let p = old.get_password().ok()?;
    if save(host, user, &p).is_ok() {
        let _ = old.delete_credential();
    }
    Some(p)
}

pub fn save(host: &str, user: &str, password: &str) -> Result<(), String> {
    entry(host, user).and_then(|e| e.set_password(password)).map_err(|e| e.to_string())
}

pub fn forget(host: &str, user: &str) {
    if let Ok(e) = entry(host, user) {
        let _ = e.delete_credential();
    }
    if let Ok(e) = keyring::Entry::new(OLD_SERVICE, &format!("{}@{}", user, host)) {
        let _ = e.delete_credential();
    }
}

#[cfg(test)]
mod tests {
    /// Writes, reads and forgets a throwaway entry in this system's real keychain:
    /// cargo test -- --ignored keychain_round_trip
    #[test]
    #[ignore]
    fn keychain_round_trip() {
        assert!(super::available(), "no keychain on this system");
        let (host, user) = ("keychain-test.invalid", "test-user");
        super::save(host, user, "s3cret pässwörd").unwrap();
        assert_eq!(super::load(host, user).as_deref(), Some("s3cret pässwörd"));
        super::forget(host, user);
        assert_eq!(super::load(host, user), None);
    }

    /// A password kept under XenCraft (the app's name for a while) is found, and moved back:
    /// cargo test -- --ignored old_name_moves_over
    #[test]
    #[ignore]
    fn old_name_moves_over() {
        let (host, user) = ("rename-test.invalid", "test-user");
        let account = format!("{}@{}", user, host);
        let old = keyring::Entry::new(super::OLD_SERVICE, &account).unwrap();
        old.set_password("from before").unwrap();
        assert_eq!(super::load(host, user).as_deref(), Some("from before"));
        assert!(matches!(old.get_password(), Err(keyring::Error::NoEntry)), "the old entry should be gone");
        assert_eq!(super::entry(host, user).unwrap().get_password().unwrap(), "from before");
        super::forget(host, user);
        assert_eq!(super::load(host, user), None);
    }
}
