// No console window behind the app on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod animated;
mod app;
mod cli;
mod config;
mod edit;
mod encode;
mod ffmpeg;
mod player;
mod services;
mod theme;
mod util;

use eframe::egui;

fn main() -> eframe::Result<()> {
    // winit can't receive dropped files on Wayland yet. When XWayland is available, use it so
    // drag-and-drop keeps working. Set
    // CLIPCRAM_WAYLAND=1 to run natively on Wayland instead.
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_some() && std::env::var_os("CLIPCRAM_WAYLAND").is_none() {
        std::env::remove_var("WAYLAND_DISPLAY");
    }

    // `clipcram encode ...` runs without a window.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("encode") {
        std::process::exit(cli::run(&args[1..]));
    }
    if matches!(args.first().map(String::as_str), Some("--help" | "-h")) {
        println!("Usage: clipcram [VIDEO...]   open the window, with these videos queued\n\n{}", cli::USAGE);
        return Ok(());
    }

    // Videos passed on the command line (or via "Open with" in a file manager) start in the queue.
    let initial_files: Vec<std::path::PathBuf> = std::env::args_os().skip(1).map(Into::into).collect();

    let tools = ffmpeg::Tools::find();
    if let Err(missing) = &tools {
        eprintln!("Missing required programs: {}", missing.join(", "));
    }

    let icon = egui::IconData {
        rgba: include_bytes!("../icons/clipcram-128.rgba").to_vec(),
        width: 128,
        height: 128,
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ClipCram")
            .with_icon(std::sync::Arc::new(icon))
            .with_app_id("clipcram")
            .with_inner_size([1360.0, 900.0])
            .with_min_inner_size([1060.0, 680.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };

    eframe::run_native(
        "ClipCram",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, tools, initial_files)))),
    )
}
