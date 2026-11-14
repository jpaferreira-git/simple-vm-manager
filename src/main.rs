// A simpler console for virtualization servers. Runs on Windows, Linux and macOS.
#![windows_subsystem = "windows"]

mod app;
mod conn;
mod console;
mod icons;
mod model;
mod theme;

use eframe::egui;

#[derive(Clone)]
struct Args {
    shot: Option<String>,
    show: String,
    theme: Option<theme::ThemeMode>,
    sidebar_open: Option<bool>,
}

fn main() -> eframe::Result {
    // --screenshot <file.png>: start with the demo server, draw about 120 frames, save the window, exit.
    // --show home|welcome|objects|networks|storage|search|logs|vm|console|add-server|confirm|about picks
    // what the screenshot shows (default: a VM's General tab; welcome is Home with no server; storage,
    // search, logs and console show that tab; the last three open that dialog over it).
    // --theme system|light|dark and --sidebar open|folded override the saved choices.
    let argv: Vec<String> = std::env::args().collect();
    let mut args = Args { shot: None, show: "vm".into(), theme: None, sidebar_open: None };
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
        viewport = viewport.with_active(false).with_min_inner_size([1180.0, 740.0]).with_max_inner_size([1180.0, 740.0]);
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
    let mut options = eframe::NativeOptions { viewport, renderer, ..Default::default() };
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
