// A simpler console for virtualization servers. Runs on Windows, Linux and macOS.
#![windows_subsystem = "windows"]
#![recursion_limit = "512"] // the demo's records (conn.rs): long json! objects

mod alerts;
mod app;
mod cloudinit;
mod conn;
mod convert;
mod console;
mod details;
mod gpu;
mod helper;
mod history;
mod hosttools;
mod icons;
mod ipconf;
mod isolib;
mod model;
mod movevm;
mod newnet;
mod newsr;
mod newvm;
mod pool_update;
mod scrollback;
mod secrets;
mod theme;
mod update;
mod upload;
mod vmss;
mod xva;

use eframe::egui;

#[derive(Clone)]
struct Args {
    shot: Option<String>,
    show: String,
    theme: Option<theme::ThemeMode>,
    sidebar_open: Option<bool>,
    /// The screenshot window's width: 760 (the app's least, to check a tight layout) to 2560.
    width: f32,
    /// Its height: 480 to 1600 (740 by default), to see the bottom of a long page.
    height: f32,
}

fn main() -> eframe::Result {
    // asked by ssh for a password (VM Conversion reads Proxmox's disks over SSH): say it, and end
    if std::env::var_os(convert::proxmox::ASKPASS).is_some() {
        if let Ok(p) = std::env::var(convert::proxmox::ASKPASS_SECRET) {
            println!("{}", p);
        }
        return Ok(());
    }
    // --screenshot <file.png>: start with the demo server, draw about 120 frames, save the window, exit.
    // --show home|welcome|objects|networks|storage|isos|search|logs|vm|console|add-server|confirm|about|iso|
    // iso-advanced|iso-error|iso-disconnect|iso-describe|iso-upload|
    // iso-upload-choose|iso-upload-web|iso-upload-done|iso-delete|new-vm|new-vm-advanced|new-vm-custom|new-vm-no-start|new-vm-created|new-vm-offline|vm-delete|host-console|vm-busy|settings|update|update-progress|host-shutdown|host-restart|pool|pool-details|pool-updates|pool-updating|pool-update-stopped|move-vm|move-vm-off|move-vm-local|sr-shared|host-details|iso-library|vm-storage|activities-moving|close-asked|new-template|networks-sorted|delete-network|host-slow-restart|new-network|new-network-error|new-storage|new-storage-inside|new-storage-error|
    // sr-unmounted|sr-detached|sr-auto-repair|sr-repair-failed|host-down|snapshots|snapshots-picked|snapshots-failed|snapshots-unscheduled|snapshot-take|
    // snapshot-revert|snapshot-delete|snapshot-taken|templates|template|new-vm-template|make-template|
    // make-template-snapshot|copy-vm|copy-vm-thin|template-made|vm-off|vm-edited|vm-bios|vm-rename|host|host-off|host-waking|
    // host-maintenance|host-in-maintenance|host-exit-maintenance|vm-performance|host-performance|alerts|alerts-all|alerts-dismissed|alert-rules|
    // export|export-off|exporting|new-vm-import|importing|snapshots-page|snapshots-ticked|schedules|schedule-edit|schedule-new|
    // schedule-pick|take-snapshots|details|details-search|change-ip|new-vm-snapshot|new-vm-copying|new-vm-cloud-init|new-vm-user-data|vm-usb|vm-usb-add|backup|restore|toolstack|renew-cert|status-report|root-password|pool-actions|vm-actions|host-actions|sr-alert-rules|helper|advisor|activities|log-detail picks what the screenshot shows (default: a VM's
    // General tab; welcome is Home with no server; storage, search, logs, console and isos show that
    // tab; iso-describe an ISO's description being typed; iso-upload a demo upload under way, -done its
    // finished card; iso-upload-choose and -web the Upload dialog's two steps; sr-unmounted the ISO
    // library mounted nowhere, sr-auto-repair the Logs after the app mounted it again by itself,
    // sr-detached Repair asking where it is,
    // sr-repair-failed Repair's answer when the share doesn't, host-down a server that went down
    // without shutting down, host-off (with SVM_DEMO_OFF=host-b) a server that's off, host-waking one
    // told to turn on twelve minutes ago, host-in-maintenance (with SVM_DEMO_MAINTENANCE=host-b) one
    // in maintenance mode, snapshots the Web Server's Snapshots tab (the others with a dialog over
    // it), vm-performance and host-performance the Web Server's and demo-host-01's Performance tab
    // (a week; SVM_PERF_SPAN=minutes|hours|year for another span), alerts the Alerts page (one
    // opened; alerts-all with everything, alerts-dismissed with one dismissed as it starts),
    // alert-rules the Web Server's Alerts dialog; copy-vm-thin Copy VM onto the NFS share (thin); export the running Web Server's Export dialog,
    // export-off the Test Desktop's (off); exporting and importing one under way in Activities (with
    // SVM_DEMO_SLOW and SVM_SHOT_WAIT); new-vm-import New VM from a demo .xva; snapshots-page the
    // toolbar's Snapshots (-ticked with three VMs ticked), schedules its Schedules… list,
    // schedule-edit the Nightly schedule, schedule-new a new one, schedule-pick the Web Server's
    // Schedule… (its Snapshots tab), take-snapshots Take Snapshot for three VMs; the rest open a dialog).
    // --theme system|light|dark and --sidebar open|folded override the saved choices; --width 760…2560
    // sets the screenshot window's width (1180 by default; wider to check a wide layout), --height
    // 480…1600 its height (740 by default).
    let argv: Vec<String> = std::env::args().collect();
    let mut args = Args { shot: None, show: "vm".into(), theme: None, sidebar_open: None, width: 1180.0, height: 740.0 };
    let mut i = 1;
    while i < argv.len() {
        let next = argv.get(i + 1).cloned();
        match (argv[i].as_str(), next) {
            ("--screenshot", Some(v)) => {
                args.shot = Some(v);
                i += 1;
            }
            ("--show", Some(v)) => {
                args.show = v;
                i += 1;
            }
            ("--theme", Some(v)) => {
                args.theme = theme::ThemeMode::parse(&v);
                i += 1;
            }
            ("--sidebar", Some(v)) => {
                args.sidebar_open = Some(v != "folded" && v != "closed");
                i += 1;
            }
            ("--width", Some(v)) => {
                args.width = v.parse::<f32>().unwrap_or(1180.0).clamp(760.0, 2560.0);
                i += 1;
            }
            ("--height", Some(v)) => {
                args.height = v.parse::<f32>().unwrap_or(740.0).clamp(480.0, 1600.0);
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    let shot = args.shot.clone();

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1180.0, 740.0])
        .with_min_inner_size([760.0, 480.0])
        .with_title("Simple VM Manager")
        // Wayland app id / X11 class: matches packaging/simple-vm-manager.desktop (Hyprland, GNOME, KDE)
        .with_app_id("simple-vm-manager");
    if shot.is_some() {
        // never take focus during a check; on Windows also sit behind other windows (it still draws there).
        // A fixed size, so tiling window managers float it and every screenshot comes out the same.
        let size = [args.width, args.height];
        viewport = viewport.with_active(false).with_inner_size(size).with_min_inner_size(size).with_max_inner_size(size);
        if cfg!(windows) {
            viewport = viewport.with_window_level(egui::WindowLevel::AlwaysOnBottom);
        }
    }
    // Draw with wgpu (DirectX 12 on Windows, with its software fallback; Vulkan/OpenGL on Linux; Metal on
    // macOS); if that can't start, try OpenGL instead.
    let first = run(viewport.clone(), eframe::Renderer::Wgpu, args.clone());
    match first {
        Err(e) => {
            eprintln!("wgpu couldn't start ({}); trying OpenGL", e);
            run(viewport, eframe::Renderer::Glow, args)
        }
        ok => ok,
    }
}

fn run(viewport: egui::ViewportBuilder, renderer: eframe::Renderer, args: Args) -> eframe::Result {
    #[allow(unused_mut)]
    let mut options = eframe::NativeOptions { viewport, renderer, wgpu_options: gpu::wgpu_options(), ..Default::default() };
    #[cfg(target_os = "macos")]
    if args.shot.is_some() {
        options.event_loop_builder = Some(Box::new(|builder| {
            use winit::platform::macos::EventLoopBuilderExtMacOS;
            builder.with_activate_ignoring_other_apps(false);
        }));
    }
    let start = app::Startup { shot_path: args.shot, shot_show: args.show, theme: args.theme, sidebar_open: args.sidebar_open };
    eframe::run_native("Simple VM Manager", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, start)))))
}
