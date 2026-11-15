//! UvCraft desktop app.
//!
//! Usage: `uvcraft-app [MESH] [--run SCRIPT] [--screenshot OUT.png] [--clear-preferences]`
//! - `MESH`: .obj, .gltf or .glb to open at startup
//! - `--run`: command script (same format as `uvcraft run`) applied after loading
//! - `--screenshot`: save a PNG of the window after a few frames, then quit
//! - `--clear-preferences`: delete the saved preferences file, then quit

mod app;
mod blender_addon;
mod camera;
mod gizmo;
mod preferences;
mod render;
mod scene;
mod theme;

use std::path::PathBuf;
use std::process::ExitCode;

use eframe::egui;

fn window_icon() -> egui::IconData {
    let original = image::load_from_memory(include_bytes!("../../../icon.png"))
        .expect("bundled UvCraft icon must be a PNG");
    let rgba = original
        .resize_exact(64, 64, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    egui::IconData {
        rgba: rgba.into_raw(),
        width: 64,
        height: 64,
    }
}

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
            "--clear-preferences" => {
                let Some(path) = preferences::Preferences::path() else {
                    eprintln!("error: no configuration directory (HOME/XDG_CONFIG_HOME unset)");
                    return ExitCode::FAILURE;
                };
                return match preferences::Preferences::clear(&path) {
                    Ok(()) => {
                        println!("cleared {}", path.display());
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("error: {}: {e}", path.display());
                        ExitCode::FAILURE
                    }
                };
            }
            "-h" | "--help" => {
                println!(
                    "usage: uvcraft-app [MESH] [--run SCRIPT] [--screenshot OUT.png] [--clear-preferences]"
                );
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
            .with_icon(window_icon())
            .with_decorations(false)
            .with_inner_size([1600.0, 900.0])
            .with_min_inner_size([1000.0, 620.0]),
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
