//! UvCraft desktop app.
//!
//! Usage: `uvcraft-app [MESH] [--run SCRIPT] [--screenshot OUT.png]`
//! - `MESH`: .obj, .gltf or .glb to open at startup
//! - `--run`: command script (same format as `uvcraft run`) applied after loading
//! - `--screenshot`: save a PNG of the window after a few frames, then quit

mod app;
mod camera;
mod render;
mod scene;

use std::path::PathBuf;
use std::process::ExitCode;

use eframe::egui;

fn main() -> ExitCode {
    let mut startup = app::Startup {
        file: None,
        script: None,
        screenshot: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--run" => startup.script = args.next().map(PathBuf::from),
            "--screenshot" => startup.screenshot = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!("usage: uvcraft-app [MESH] [--run SCRIPT] [--screenshot OUT.png]");
                return ExitCode::SUCCESS;
            }
            _ if a.starts_with('-') => {
                eprintln!("error: unknown option {a}");
                return ExitCode::FAILURE;
            }
            _ => startup.file = Some(PathBuf::from(a)),
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("UvCraft")
            .with_app_id("uvcraft")
            .with_inner_size([1440.0, 860.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };
    match eframe::run_native(
        "UvCraft",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, startup)))),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
