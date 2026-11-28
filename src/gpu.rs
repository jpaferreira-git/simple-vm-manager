// Keeps the window drawing after the computer sleeps.
//
// When the computer wakes up, the graphics driver often says the window's picture ("surface") was
// lost or is out of date, even though the window kept its size. egui-wgpu's default answer is to
// skip that frame and nothing more: the surface is only set up again when the window is resized,
// so every frame after that is skipped too and the window looks frozen. Here a lost or outdated
// surface is set up again, and the next frame is asked for.
//
// Some drivers (NVIDIA on Linux, DirectX on Windows after a driver reset) lose the whole GPU device
// while asleep. wgpu can't bring a device back, so the app starts itself again (the servers that
// were connected reconnect from the keychain, as at any start) and this copy closes.
//
// A frame that can't be drawn mustn't keep what it sent the GPU. egui-wgpu copies the frame's
// shapes and any changed pictures (a console's screen, a thumbnail) to the GPU before it asks for
// the window's picture, and hands them over together with the drawing once it has one. When it
// gets none (the screen is locked or off, the computer is going to sleep) it gives up on the frame
// and those copies stay queued: wgpu only lets them go once something is handed to the GPU. A
// window that can't draw for a whole night queued gigabytes that way, until the system ran out of
// memory, and the GPU driver refused the lot when the screen came back. So after each frame that
// couldn't be drawn, what's queued is handed over right away (with nothing to draw), and freed.

use eframe::egui;
use eframe::egui_wgpu::{SurfaceErrorAction, WgpuConfiguration};
use eframe::wgpu;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// The window's context, so a callback from the renderer can ask for the next frame.
static CTX: OnceLock<egui::Context> = OnceLock::new();
/// The GPU's queue, to hand over a frame's copies when the frame couldn't be drawn.
static QUEUE: OnceLock<Arc<wgpu::Queue>> = OnceLock::new();
/// The GPU device was lost: the app should start again.
static DEVICE_LOST: AtomicBool = AtomicBool::new(false);
/// Surface errors in a row (none for two seconds ends a row), to quiet the warnings and slow down
/// the retries while they go on (a minimized window on Windows keeps saying Outdated).
static ERRORS: AtomicU32 = AtomicU32::new(0);
/// When the last one came, in milliseconds since `START`.
static LAST_ERROR_MS: AtomicU64 = AtomicU64::new(0);
static START: OnceLock<Instant> = OnceLock::new();

/// wgpu's options for eframe: egui's defaults with a surface that comes back after sleep.
pub fn wgpu_options() -> WgpuConfiguration {
    WgpuConfiguration {
        on_surface_error: Arc::new(on_surface_error),
        ..Default::default()
    }
}

fn on_surface_error(err: wgpu::SurfaceError) -> SurfaceErrorAction {
    let now = START.get_or_init(Instant::now).elapsed().as_millis() as u64;
    if now.saturating_sub(LAST_ERROR_MS.swap(now, Ordering::Relaxed)) > 2_000 {
        ERRORS.store(0, Ordering::Relaxed);
    }
    let n = ERRORS.fetch_add(1, Ordering::Relaxed);
    if n < 5 && err != wgpu::SurfaceError::Outdated {
        eprintln!("The window's picture couldn't be drawn ({}); setting it up again", err);
    }
    flush();
    if let Some(ctx) = CTX.get() {
        // the first retries right away; a window that keeps failing (minimized) a few times a second
        let wait = if n < 10 { 16 } else { 500 };
        ctx.request_repaint_after(Duration::from_millis(wait));
    }
    match err {
        wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated | wgpu::SurfaceError::OutOfMemory => SurfaceErrorAction::RecreateSurface,
        wgpu::SurfaceError::Timeout => SurfaceErrorAction::SkipFrame,
    }
}

/// Hands the GPU what this frame copied to it, with nothing to draw: wgpu frees it once done.
fn flush() {
    if let Some(q) = QUEUE.get() {
        q.submit(std::iter::empty());
    }
}

/// Called once the window is up: remembers its context and watches the GPU device.
pub fn watch(cc: &eframe::CreationContext<'_>) {
    let _ = CTX.set(cc.egui_ctx.clone());
    let Some(rs) = cc.wgpu_render_state.as_ref() else { return }; // drawing with OpenGL
    let _ = QUEUE.set(rs.queue.clone());
    let ctx = cc.egui_ctx.clone();
    rs.device.set_device_lost_callback(move |reason, msg| {
        // dropping or replacing the device at exit calls it too: those aren't a loss
        if matches!(reason, wgpu::DeviceLostReason::Unknown | wgpu::DeviceLostReason::DeviceInvalid) {
            eprintln!("The GPU device was lost ({:?}: {}); starting again", reason, msg);
            DEVICE_LOST.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        }
    });
    // wgpu's default panics on any error; a lost device makes every call fail, so only log them
    // (the app restarts anyway).
    rs.device.on_uncaptured_error(Box::new(|e| eprintln!("wgpu error: {}", e)));
}

/// Whether the GPU device was lost (the app can't draw anymore and should start again).
pub fn device_lost() -> bool {
    DEVICE_LOST.load(Ordering::Relaxed)
}

/// Starts a new copy of the app with the same arguments (once). Whether it started.
pub fn relaunch() -> bool {
    static TRIED: AtomicBool = AtomicBool::new(false);
    if TRIED.swap(true, Ordering::Relaxed) {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else { return false };
    let args: Vec<String> = std::env::args().skip(1).collect();
    match std::process::Command::new(exe).args(args).env(crate::single::TAKING_OVER, "1").spawn() {
        Ok(_) => true,
        Err(e) => {
            eprintln!("Couldn't start the app again: {}", e);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lost_or_outdated_surface_is_set_up_again() {
        for e in [wgpu::SurfaceError::Lost, wgpu::SurfaceError::Outdated, wgpu::SurfaceError::OutOfMemory] {
            assert!(matches!(on_surface_error(e), SurfaceErrorAction::RecreateSurface));
        }
        assert!(matches!(on_surface_error(wgpu::SurfaceError::Timeout), SurfaceErrorAction::SkipFrame));
    }

    #[test]
    fn the_default_configuration_is_kept_otherwise() {
        let ours = wgpu_options();
        let theirs = WgpuConfiguration::default();
        assert_eq!(ours.present_mode, theirs.present_mode);
        assert_eq!(ours.supported_backends, theirs.supported_backends);
    }
}
