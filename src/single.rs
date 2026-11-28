// One copy of the app at a time.
//
// Two copies left open both read every server's records, consoles and charts, and use twice the
// memory. So the first copy holds a lock on a small file in the app's settings folder while it
// runs (the system lets go of it when the app ends, even if it crashed or was killed). A copy
// started while it's held asks the one that's open to show its window, and ends.
//
// The app starts a copy of itself on purpose in two cases, and that one waits for the old copy to
// close instead: after installing an update, and when the GPU was lost during sleep (see gpu.rs).
// It's told so by TAKING_OVER.

use eframe::egui;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

/// Set (to anything) for a copy the app started to take its own place.
pub const TAKING_OVER: &str = "SVM_TAKING_OVER";
/// How long such a copy waits for the old one to close before starting anyway.
const TAKE_OVER_WAIT: Duration = Duration::from_secs(30);

/// The lock, held for as long as the app runs.
static HELD: OnceLock<File> = OnceLock::new();
/// Another copy asked this one to show its window.
static SHOW_ASKED: AtomicBool = AtomicBool::new(false);

const LOCK_FILE: &str = "running.lock";
const SHOW_FILE: &str = "show-window";

/// What `start` found.
#[derive(Debug, PartialEq)]
pub enum Start {
    /// This is the only copy (or the lock couldn't be used at all: the app starts as before).
    Alone,
    /// Another copy is open, and was asked to show itself: this one should end.
    AlreadyOpen,
}

/// Takes the lock as the app starts.
pub fn start(dir: Option<PathBuf>) -> Start {
    let Some(dir) = dir else { return Start::Alone };
    let taking_over = std::env::var_os(TAKING_OVER).is_some();
    std::env::remove_var(TAKING_OVER); // not passed on to what this copy starts
    match take(&dir, if taking_over { TAKE_OVER_WAIT } else { Duration::ZERO }) {
        Some(Some(f)) => {
            let _ = HELD.set(f);
            Start::Alone
        }
        Some(None) if taking_over => Start::Alone, // the old copy didn't close: start anyway
        Some(None) => {
            ask_to_show(&dir);
            Start::AlreadyOpen
        }
        None => Start::Alone,
    }
}

/// The lock in `dir`, waiting up to `wait` for it: Some(None) when another copy holds it, None when
/// it can't be used (no folder, a file system without locks).
fn take(dir: &Path, wait: Duration) -> Option<Option<File>> {
    std::fs::create_dir_all(dir).ok()?;
    let f = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(LOCK_FILE)).ok()?;
    let until = Instant::now() + wait;
    loop {
        match f.try_lock() {
            Ok(()) => return Some(Some(f)),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < until => std::thread::sleep(Duration::from_millis(250)),
            Err(std::fs::TryLockError::WouldBlock) => return Some(None),
            Err(std::fs::TryLockError::Error(e)) => {
                eprintln!("Couldn't check for another open copy of the app ({}); starting anyway", e);
                return None;
            }
        }
    }
}

/// Tells the open copy to show its window: it watches this file.
fn ask_to_show(dir: &Path) {
    if let Ok(mut f) = File::create(dir.join(SHOW_FILE)) {
        let _ = writeln!(f, "{:?}", SystemTime::now());
    }
    eprintln!("Simple VM Manager is already open; showing its window instead of opening another");
}

/// Once the window is up: watches for another copy asking to show it.
pub fn watch(ctx: &egui::Context, dir: Option<PathBuf>) {
    let Some(dir) = dir.filter(|_| HELD.get().is_some()) else { return };
    let ctx = ctx.clone();
    let file = dir.join(SHOW_FILE);
    let seen = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut last = seen(&file);
    let _ = std::thread::Builder::new().name("single-copy".into()).spawn(move || loop {
        std::thread::sleep(Duration::from_millis(500));
        let now = seen(&file);
        if now.is_some() && now != last {
            SHOW_ASKED.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        }
        last = now;
    });
}

/// Whether another copy asked to show the window since the last look.
pub fn show_asked() -> bool {
    SHOW_ASKED.swap(false, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_copy_finds_the_first_and_asks_it_to_show() {
        let dir = std::env::temp_dir().join(format!("svm-single-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = take(&dir, Duration::ZERO).expect("lock usable").expect("first copy gets it");
        // another copy (another handle on the file, as another process has) doesn't get it
        assert!(matches!(take(&dir, Duration::ZERO), Some(None)));
        // and gets it once the first closes
        drop(first);
        assert!(matches!(take(&dir, Duration::ZERO), Some(Some(_))));
        ask_to_show(&dir);
        assert!(dir.join(SHOW_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_copy_taking_over_waits_for_the_old_one() {
        let dir = std::env::temp_dir().join(format!("svm-single-wait-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let old = take(&dir, Duration::ZERO).unwrap().unwrap();
        let closes = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            drop(old);
        });
        let started = Instant::now();
        assert!(matches!(take(&dir, Duration::from_secs(5)), Some(Some(_))));
        assert!(started.elapsed() >= Duration::from_millis(500));
        closes.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
