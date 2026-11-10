//! `uvcraft`: headless UvCraft. Everything goes through the command layer.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use uvcraft_algo::analysis::Report;
use uvcraft_commands::{Command, Outcome, Session, parse_script};

#[derive(Parser)]
#[command(name = "uvcraft", version, about = "UvCraft headless UV tool")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print a UV quality report for a mesh
    Analyze {
        input: PathBuf,
        /// Texture resolution used for texel density
        #[arg(long, default_value_t = 2048)]
        resolution: u32,
        /// Print the full report as JSON
        #[arg(long)]
        json: bool,
    },
    /// Run a script of commands (JSON Lines, or a JSON array)
    Run {
        script: PathBuf,
        /// Write the executed commands to this file as JSON Lines
        #[arg(long)]
        log: Option<PathBuf>,
        /// Print analyze reports as JSON
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let mut session = Session::new();
    match cli.command {
        Cmd::Analyze {
            input,
            resolution,
            json,
        } => {
            exec(&mut session, Command::Load { path: input }, json)?;
            exec(
                &mut session,
                Command::Analyze {
                    resolution,
                    output: None,
                },
                json,
            )?;
        }
        Cmd::Run { script, log, json } => {
            let text = std::fs::read_to_string(&script)
                .map_err(|e| format!("{}: {e}", script.display()))?;
            let commands = parse_script(&text).map_err(|e| format!("{}: {e}", script.display()))?;
            let result = commands.into_iter().enumerate().try_for_each(|(i, cmd)| {
                exec(&mut session, cmd, json).map_err(|e| format!("command {}: {e}", i + 1))
            });
            // Write the log even when a command failed, to show how far it got.
            if let Some(path) = log {
                std::fs::write(&path, session.log_jsonl())
                    .map_err(|e| format!("{}: {e}", path.display()))?;
            }
            result?;
        }
    }
    Ok(())
}

fn exec(session: &mut Session, cmd: Command, json: bool) -> Result<(), String> {
    match session.execute(cmd).map_err(|e| e.to_string())? {
        Outcome::Done => {}
        Outcome::Info(message) => eprintln!("note: {message}"),
        Outcome::Report(report) if json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).expect("reports always serialize")
            );
        }
        Outcome::Report(report) => {
            if let Some(doc) = session.document() {
                println!("{}", doc.path.display());
                if doc.missing_uv_corners > 0 {
                    println!(
                        "  warning: {} corners had no UVs (placeholders at 0,0)",
                        doc.missing_uv_corners
                    );
                }
            }
            print_report(&report);
        }
    }
    Ok(())
}

fn print_report(r: &Report) {
    println!(
        "  faces {}  islands {}  resolution {}",
        r.face_count, r.island_count, r.resolution
    );
    println!(
        "  UV area {:.2}% of one tile  (3D area {:.4})",
        r.uv_area * 100.0,
        r.area_3d
    );
    println!(
        "  distortion  angle mean {:.4} max {:.4}  |  area mean {:.4} max {:.4}",
        r.mean_angle_distortion,
        r.max_angle_distortion,
        r.mean_area_distortion,
        r.max_area_distortion
    );
    println!(
        "  edges  seams {} (length {:.4})  borders {}  non-manifold {}",
        r.seam_edges, r.seam_length, r.border_edges, r.non_manifold_edges
    );
    let problems = [
        (r.flipped_islands, "flipped islands"),
        (r.flipped_triangles, "flipped triangles"),
        (r.degenerate_triangles, "degenerate triangles"),
        (r.islands_outside_unit_tile, "islands outside 0-1"),
        (r.islands_crossing_tile, "islands crossing a tile border"),
    ];
    for (n, what) in problems.into_iter().filter(|&(n, _)| n > 0) {
        println!("  warning: {n} {what}");
    }
    if !r.islands.is_empty() {
        println!("  island   faces    TD px/u   angle    area");
        for i in &r.islands {
            println!(
                "  {:>6} {:>7} {:>10.2} {:>7.3} {:>7.3}{}",
                i.index,
                i.face_count,
                i.texel_density,
                i.mean_angle_distortion,
                i.mean_area_distortion,
                if i.flipped { "  flipped" } else { "" }
            );
        }
    }
}
