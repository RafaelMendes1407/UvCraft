//! `uvcraft`: headless UvCraft. Everything goes through the command layer.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use uvcraft_algo::analysis::Report;
use uvcraft_algo::validate::MeshProblems;
use uvcraft_commands::{Command, Outcome, Session, parse_script};

mod rpc;

#[derive(Parser)]
#[command(name = "uvcraft", version, about = "UvCraft headless UV tool")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the versioned local HTTP bridge for Blender and other DCCs.
    Bridge {
        /// Loopback address and port; non-loopback addresses are rejected.
        #[arg(long, default_value = "127.0.0.1:8766")]
        bind: String,
    },
    /// Serve JSON-RPC 2.0 requests over a line-delimited TCP connection.
    Serve {
        /// Address to listen on (defaults to loopback only).
        #[arg(long, default_value = "127.0.0.1:8765")]
        bind: String,
    },
    /// Print a UV quality report for a mesh
    Analyze {
        input: PathBuf,
        /// Texture resolution used for texel density
        #[arg(long, default_value_t = 2048)]
        resolution: u32,
        /// Report island pairs closer than this many pixels (0: skip)
        #[arg(long, default_value_t = 0.0)]
        padding: f64,
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
        /// Write the macro (edits still applied at the end, undone ones dropped) to this file
        #[arg(long = "macro")]
        macro_path: Option<PathBuf>,
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
        Cmd::Bridge { bind } => return uvcraft_bridge::serve(&bind),
        Cmd::Serve { bind } => return rpc::serve(&bind),
        Cmd::Analyze {
            input,
            resolution,
            padding,
            json,
        } => {
            exec(&mut session, Command::Load { path: input }, json)?;
            exec(
                &mut session,
                Command::Analyze {
                    resolution,
                    output: None,
                    padding_px: padding,
                },
                json,
            )?;
        }
        Cmd::Run {
            script,
            log,
            macro_path,
            json,
        } => {
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
            if let Some(path) = macro_path {
                std::fs::write(&path, session.macro_jsonl())
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
        Outcome::Selection { islands } if json => {
            println!("{}", serde_json::json!({"islands": islands}));
        }
        Outcome::Selection { islands } => {
            println!("islands: {islands:?}");
        }
        Outcome::FaceSelection { faces } if json => {
            println!("{}", serde_json::json!({"faces": faces}));
        }
        Outcome::FaceSelection { faces } => {
            println!("faces: {faces:?}");
        }
        Outcome::Report(report) if json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).expect("reports always serialize")
            );
        }
        Outcome::PhysicalReport(report) if json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).expect("physical reports serialize")
            );
        }
        Outcome::PhysicalReport(report) => {
            for item in &report.edge_lengths {
                println!(
                    "edge {}: {:.6} {:?}",
                    item.edge, item.length, report.output_unit
                );
            }
            for item in &report.island_areas {
                println!(
                    "island {}: {:.6} {:?}²",
                    item.island, item.area, report.output_unit
                );
            }
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
    if !r.measurement_local_meshes.is_empty() {
        println!(
            "  note: glTF meshes {:?} measured in local space (instanced, skinned, animated or without a unique node)",
            r.measurement_local_meshes
        );
    }
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
        (r.overlapping_faces, "overlapping faces"),
        (r.self_overlapping_islands, "self-overlapping islands"),
        (r.overlapping_island_pairs.len(), "overlapping island pairs"),
    ];
    for (n, what) in problems.into_iter().filter(|&(n, _)| n > 0) {
        println!("  warning: {n} {what}");
    }
    if let Some(m) = &r.mesh_problems {
        print_mesh_problems(m);
    }
    if !r.padding_violations.is_empty() {
        println!(
            "  warning: {} island pairs closer than {} px",
            r.padding_violations.len(),
            r.padding_px
        );
        for v in &r.padding_violations {
            println!(
                "    islands {} and {}: {:.2} px",
                v.islands[0], v.islands[1], v.distance_px
            );
        }
    }
    if !r.islands.is_empty() {
        println!("  island   faces    TD px/u   angle    area");
        for i in &r.islands {
            println!(
                "  {:>6} {:>7} {:>10.2} {:>7.3} {:>7.3}{}{}",
                i.index,
                i.face_count,
                i.texel_density,
                i.mean_angle_distortion,
                i.mean_area_distortion,
                if i.flipped { "  flipped" } else { "" },
                if i.overlapping_faces > 0 {
                    "  overlaps"
                } else {
                    ""
                },
            );
        }
    }
}

fn print_mesh_problems(m: &MeshProblems) {
    println!(
        "  mesh  vertices {}  edges {}  components {}  boundary edges {}",
        m.vertex_count, m.edge_count, m.components, m.boundary_edges
    );
    let list = |ids: &[usize]| {
        let shown: Vec<String> = ids.iter().take(10).map(|i| i.to_string()).collect();
        let more = if ids.len() > 10 { ", ..." } else { "" };
        format!("[{}{more}]", shown.join(", "))
    };
    let duplicates: Vec<usize> = m.duplicate_vertices.iter().map(|g| g[0]).collect();
    let problems: [(&[usize], &str); 7] = [
        (&m.non_manifold_edges, "non-manifold edges"),
        (&m.non_manifold_vertices, "non-manifold vertices"),
        (&duplicates, "groups of duplicate vertices (first of each)"),
        (&m.degenerate_faces, "degenerate faces"),
        (&m.repeated_vertex_faces, "faces with a repeated vertex"),
        (
            &m.inconsistent_winding_edges,
            "edges with inconsistent winding",
        ),
        (&m.isolated_vertices, "isolated vertices"),
    ];
    for (ids, what) in problems.into_iter().filter(|(ids, _)| !ids.is_empty()) {
        println!("  mesh warning: {} {what} {}", ids.len(), list(ids));
    }
}
