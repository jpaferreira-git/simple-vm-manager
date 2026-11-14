//! Saved passwords. They live in the system's keychain (Secret Service on Linux, such as GNOME
//! Keyring, KWallet or KeePassXC; Credential Manager on Windows; Keychain on macOS), never in the
//! app's own files. Every call may wait on the keychain (it can ask to be unlocked), so the app
//! makes them from background threads.

const SERVICE: &str = "simple-vm-manager";

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
    entry(host, user).ok()?.get_password().ok()
}

pub fn save(host: &str, user: &str, password: &str) -> Result<(), String> {
    entry(host, user).and_then(|e| e.set_password(password)).map_err(|e| e.to_string())
}

pub fn forget(host: &str, user: &str) {
    if let Ok(e) = entry(host, user) {
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
}
