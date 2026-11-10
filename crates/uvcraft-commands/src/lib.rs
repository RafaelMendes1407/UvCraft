//! UvCraft command layer.
//!
//! Every user action is a serializable [`Command`]. The UI, the CLI, scripts,
//! macros and (later) Python all go through [`Session::execute`], so they share
//! one code path for undo/redo and the command log.

use std::path::PathBuf;

use glam::{DMat2, DVec2};
use serde::{Deserialize, Serialize};
use uvcraft_algo::analysis::{self, Report};
use uvcraft_algo::pack::{self, PackOptions};
use uvcraft_algo::projection::{self, Projection};
use uvcraft_algo::{seams, unfold};
use uvcraft_core::{Islands, Mesh, UvLayer};
use uvcraft_io::{IoError, Source};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pivot {
    /// Center of each island's UV bounding box.
    #[default]
    IslandCenter,
    /// UV origin (0, 0).
    Origin,
}

fn unit_scale() -> [f64; 2] {
    [1.0, 1.0]
}
fn default_resolution() -> u32 {
    2048
}
fn default_angle() -> f64 {
    60.0
}
fn default_padding() -> f64 {
    4.0
}
fn yes() -> bool {
    true
}

/// One user action. Serialized as JSON with a `cmd` tag, e.g.
/// `{"cmd":"transform","rotate_deg":90}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    /// Load a mesh (.obj, .gltf, .glb), replacing the current document and clearing history.
    Load {
        path: PathBuf,
    },
    /// Save the current document in its original format (OBJ, glTF or GLB), topology preserved.
    Save {
        path: PathBuf,
    },
    /// Scale, then rotate, then translate islands about a pivot.
    Transform {
        /// Island indices; empty means all islands.
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default)]
        translate: [f64; 2],
        #[serde(default)]
        rotate_deg: f64,
        #[serde(default = "unit_scale")]
        scale: [f64; 2],
        #[serde(default)]
        pivot: Pivot,
    },
    /// Cut mesh edges into seams (UV vertices split where the cut separates faces).
    Cut {
        edges: Vec<usize>,
    },
    /// Weld seam edges. Separate islands are aligned onto each other first.
    Weld {
        edges: Vec<usize>,
    },
    /// Cut edges sharper than `angle_deg`, then (optionally) cut every island
    /// until it has a single border so it can be unfolded.
    AutoSeams {
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default = "default_angle")]
        angle_deg: f64,
        #[serde(default = "yes")]
        ensure_disk: bool,
    },
    /// Project islands (`"box"`, `"best"` or `{"planar": "x"|"y"|"z"}`) and fit them into 0–1.
    Project {
        #[serde(default)]
        islands: Vec<usize>,
        mode: Projection,
    },
    /// Flatten islands with LSCM (keeps each island's area and center).
    Unfold {
        #[serde(default)]
        islands: Vec<usize>,
    },
    /// Pack islands into the 0–1 tile.
    Pack {
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default = "default_padding")]
        padding_px: f64,
        #[serde(default = "default_padding")]
        margin_px: f64,
        #[serde(default = "default_resolution")]
        resolution: u32,
        #[serde(default = "yes")]
        orient: bool,
        #[serde(default = "yes")]
        rotate_90: bool,
    },
    /// Scale islands to a texel density (pixels per 3D unit at `resolution`).
    SetTexelDensity {
        #[serde(default)]
        islands: Vec<usize>,
        density: f64,
        #[serde(default = "default_resolution")]
        resolution: u32,
    },
    Undo,
    Redo,
    /// Compute a quality report; optionally write it to a JSON file.
    Analyze {
        #[serde(default = "default_resolution")]
        resolution: u32,
        #[serde(default)]
        output: Option<PathBuf>,
    },
}

impl Command {
    /// The command's name as written in scripts (e.g. `"auto_seams"`).
    pub fn name(&self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.get("cmd").and_then(|c| c.as_str()).map(str::to_string))
            .unwrap_or_default()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("no mesh loaded")]
    NoDocument,
    #[error("nothing to undo")]
    NothingToUndo,
    #[error("nothing to redo")]
    NothingToRedo,
    #[error("island {index} does not exist (the mesh has {count} islands)")]
    InvalidIsland { index: usize, count: usize },
    #[error("edge {index} does not exist (the mesh has {count} edges)")]
    InvalidEdge { index: usize, count: usize },
    #[error("{0}")]
    InvalidArgument(String),
    #[error("{path}: {source}")]
    File { path: PathBuf, source: IoError },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Done,
    Report(Box<Report>),
    /// Done, with a message worth showing (e.g. skipped islands).
    Info(String),
}

/// The loaded mesh plus its editable UV layer.
#[derive(Debug, Clone)]
pub struct Document {
    pub path: PathBuf,
    pub mesh: Mesh,
    uv: UvLayer,
    source: Source,
    pub objects: Vec<String>,
    pub missing_uv_corners: usize,
    /// UV vertices duplicated at load so that no two islands share one.
    pub split_uv_vertices: usize,
    islands: Option<Islands>,
}

impl Document {
    pub fn uv(&self) -> &UvLayer {
        &self.uv
    }

    pub fn islands(&mut self) -> &Islands {
        self.islands
            .get_or_insert_with(|| Islands::compute(&self.mesh, &self.uv))
    }

    fn set_uv(&mut self, uv: UvLayer) {
        if uv.corner_uv != self.uv.corner_uv {
            self.islands = None;
        }
        self.uv = uv;
    }
}

/// Editing session. Undo/redo store full UV layer snapshots (the mesh never changes).
#[derive(Debug, Default)]
pub struct Session {
    doc: Option<Document>,
    undo: Vec<UvLayer>,
    redo: Vec<UvLayer>,
    log: Vec<Command>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn document(&self) -> Option<&Document> {
        self.doc.as_ref()
    }

    /// Every successfully executed command, in order. Replaying it reproduces the session.
    pub fn log(&self) -> &[Command] {
        &self.log
    }

    pub fn log_jsonl(&self) -> String {
        self.log
            .iter()
            .map(|c| serde_json::to_string(c).expect("commands always serialize") + "\n")
            .collect()
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn execute(&mut self, cmd: Command) -> Result<Outcome, CommandError> {
        let outcome = self.apply(&cmd)?;
        self.log.push(cmd);
        Ok(outcome)
    }

    fn doc_mut(&mut self) -> Result<&mut Document, CommandError> {
        self.doc.as_mut().ok_or(CommandError::NoDocument)
    }

    /// Records an undo snapshot, then runs an edit on the UV layer.
    /// Runs an edit on the UV layer. On success, keeps the islands invariant
    /// (no UV vertex shared by two islands) and records an undo snapshot if
    /// anything changed; on error, restores the previous layer.
    fn edit(
        &mut self,
        f: impl FnOnce(&mut Document) -> Result<Outcome, CommandError>,
    ) -> Result<Outcome, CommandError> {
        let doc = self.doc.as_mut().ok_or(CommandError::NoDocument)?;
        let before = doc.uv.clone();
        match f(doc) {
            Ok(outcome) => {
                if doc.uv.corner_uv != before.corner_uv {
                    let islands = Islands::compute(&doc.mesh, &doc.uv);
                    islands.split_shared_uvs(&doc.mesh, &mut doc.uv);
                    doc.islands = Some(islands);
                }
                if doc.uv != before {
                    self.undo.push(before);
                    self.redo.clear();
                }
                Ok(outcome)
            }
            Err(e) => {
                doc.set_uv(before);
                Err(e)
            }
        }
    }

    fn apply(&mut self, cmd: &Command) -> Result<Outcome, CommandError> {
        match cmd {
            Command::Load { path } => {
                let parsed = uvcraft_io::read(path).map_err(|source| CommandError::File {
                    path: path.clone(),
                    source,
                })?;
                let mut uv = parsed.uv;
                let islands = Islands::compute(&parsed.mesh, &uv);
                let split_uv_vertices = islands.split_shared_uvs(&parsed.mesh, &mut uv);
                self.doc = Some(Document {
                    path: path.clone(),
                    mesh: parsed.mesh,
                    uv,
                    source: parsed.source,
                    objects: parsed.objects,
                    missing_uv_corners: parsed.missing_uv_corners,
                    split_uv_vertices,
                    islands: Some(islands),
                });
                self.undo.clear();
                self.redo.clear();
            }
            Command::Save { path } => {
                let doc = self.doc.as_ref().ok_or(CommandError::NoDocument)?;
                uvcraft_io::write(path, &doc.source, &doc.mesh, &doc.uv).map_err(|source| {
                    CommandError::File {
                        path: path.clone(),
                        source,
                    }
                })?;
            }
            Command::Transform {
                islands,
                translate,
                rotate_deg,
                scale,
                pivot,
            } => {
                let m = DMat2::from_angle(rotate_deg.to_radians())
                    * DMat2::from_diagonal(DVec2::from(*scale));
                let t = DVec2::from(*translate);
                return self
                    .edit(|doc| transform(doc, islands, m, t, *pivot).map(|_| Outcome::Done));
            }
            Command::Cut { edges } => {
                return self.edit(|doc| {
                    check_edges(&doc.mesh, edges)?;
                    seams::cut_edges(&doc.mesh, &mut doc.uv, edges);
                    Ok(Outcome::Done)
                });
            }
            Command::Weld { edges } => {
                return self.edit(|doc| {
                    check_edges(&doc.mesh, edges)?;
                    seams::weld_edges(&doc.mesh, &mut doc.uv, edges);
                    Ok(Outcome::Done)
                });
            }
            Command::AutoSeams {
                islands,
                angle_deg,
                ensure_disk,
            } => {
                return self.edit(|doc| {
                    let targets = resolve_islands(doc, islands)?;
                    let Document {
                        mesh, uv, islands, ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    // Remember the targets by face: island numbers change after cutting.
                    let mut in_target = vec![false; mesh.face_count()];
                    for &i in &targets {
                        for &f in isl.faces(i) {
                            in_target[f as usize] = true;
                        }
                    }
                    let sharp: Vec<usize> = seams::sharp_edges(mesh, *angle_deg)
                        .into_iter()
                        .filter(|&e| in_target[mesh.corner_face(mesh.edge_corners(e).0)])
                        .collect();
                    seams::cut_edges(mesh, uv, &sharp);
                    if *ensure_disk {
                        let after = Islands::compute(mesh, uv);
                        let mut now: Vec<usize> = (0..mesh.face_count())
                            .filter(|&f| in_target[f])
                            .map(|f| after.island_of_face(f))
                            .collect();
                        now.sort_unstable();
                        now.dedup();
                        seams::cut_to_disk(mesh, uv, &now);
                    }
                    Ok(Outcome::Done)
                });
            }
            Command::Project { islands, mode } => {
                return self.edit(|doc| {
                    let targets = resolve_islands(doc, islands)?;
                    projection::project(&doc.mesh, &mut doc.uv, &targets, *mode);
                    Ok(Outcome::Done)
                });
            }
            Command::Unfold { islands } => {
                return self.edit(|doc| {
                    let targets = resolve_islands(doc, islands)?;
                    let Document { mesh, uv, islands, .. } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    let r = unfold::unfold_lscm(mesh, uv, isl, &targets);
                    let mut notes = Vec::new();
                    if !r.skipped_closed.is_empty() {
                        notes.push(format!(
                            "skipped {} closed island(s) {:?}: cut a seam first (auto_seams does this)",
                            r.skipped_closed.len(),
                            r.skipped_closed
                        ));
                    }
                    if !r.failed.is_empty() {
                        notes.push(format!("could not unfold island(s) {:?}", r.failed));
                    }
                    Ok(if notes.is_empty() { Outcome::Done } else { Outcome::Info(notes.join("; ")) })
                });
            }
            Command::Pack {
                islands,
                padding_px,
                margin_px,
                resolution,
                orient,
                rotate_90,
            } => {
                let options = PackOptions {
                    padding_px: *padding_px,
                    margin_px: *margin_px,
                    resolution: *resolution,
                    orient: *orient,
                    rotate_90: *rotate_90,
                };
                if options.padding_px < 0.0 || options.margin_px < 0.0 || options.resolution == 0 {
                    return Err(CommandError::InvalidArgument(
                        "padding and margin must be ≥ 0, resolution > 0".into(),
                    ));
                }
                return self.edit(|doc| {
                    let targets = resolve_islands(doc, islands)?;
                    let Document {
                        mesh, uv, islands, ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    pack::pack(mesh, uv, isl, &targets, &options);
                    Ok(Outcome::Done)
                });
            }
            Command::SetTexelDensity {
                islands,
                density,
                resolution,
            } => {
                if !density.is_finite() || *density <= 0.0 || *resolution == 0 {
                    return Err(CommandError::InvalidArgument(
                        "density and resolution must be > 0".into(),
                    ));
                }
                return self.edit(|doc| {
                    let targets = resolve_islands(doc, islands)?;
                    let Document {
                        mesh, uv, islands, ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    pack::set_texel_density(mesh, uv, isl, &targets, *density, *resolution);
                    Ok(Outcome::Done)
                });
            }
            Command::Undo => {
                let prev = self.undo.pop().ok_or(CommandError::NothingToUndo)?;
                let doc = self.doc_mut()?;
                let current = doc.uv.clone();
                doc.set_uv(prev);
                self.redo.push(current);
            }
            Command::Redo => {
                let next = self.redo.pop().ok_or(CommandError::NothingToRedo)?;
                let doc = self.doc_mut()?;
                let current = doc.uv.clone();
                doc.set_uv(next);
                self.undo.push(current);
            }
            Command::Analyze { resolution, output } => {
                let doc = self.doc_mut()?;
                doc.islands();
                let islands = doc.islands.as_ref().expect("computed above");
                let report = analysis::analyze(&doc.mesh, &doc.uv, islands, *resolution);
                if let Some(path) = output {
                    let json =
                        serde_json::to_string_pretty(&report).expect("reports always serialize");
                    std::fs::write(path, json).map_err(|source| CommandError::Io {
                        path: path.clone(),
                        source,
                    })?;
                }
                return Ok(Outcome::Report(Box::new(report)));
            }
        }
        Ok(Outcome::Done)
    }
}

fn check_edges(mesh: &Mesh, edges: &[usize]) -> Result<(), CommandError> {
    let count = mesh.edge_count();
    match edges.iter().find(|&&e| e >= count) {
        Some(&index) => Err(CommandError::InvalidEdge { index, count }),
        None => Ok(()),
    }
}

/// Validates island indices; an empty list means all islands.
fn resolve_islands(doc: &mut Document, ids: &[usize]) -> Result<Vec<usize>, CommandError> {
    let count = doc.islands().count();
    if let Some(&index) = ids.iter().find(|&&i| i >= count) {
        return Err(CommandError::InvalidIsland { index, count });
    }
    let mut out = if ids.is_empty() {
        (0..count).collect()
    } else {
        ids.to_vec()
    };
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

fn transform(
    doc: &mut Document,
    targets: &[usize],
    m: DMat2,
    t: DVec2,
    pivot: Pivot,
) -> Result<(), CommandError> {
    let Document {
        mesh, uv, islands, ..
    } = doc;
    let islands = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
    let count = islands.count();
    if let Some(&index) = targets.iter().find(|&&i| i >= count) {
        return Err(CommandError::InvalidIsland { index, count });
    }
    let all: Vec<usize>;
    let targets = if targets.is_empty() {
        all = (0..count).collect();
        &all
    } else {
        targets
    };
    // A UV vertex shared by several islands (malformed input) moves only once.
    let mut moved = vec![false; uv.uvs.len()];
    for &i in targets {
        let ids = islands.uv_indices(i, mesh, uv);
        let p = match pivot {
            Pivot::Origin => DVec2::ZERO,
            Pivot::IslandCenter => {
                let (lo, hi) = ids.iter().fold(
                    (DVec2::splat(f64::INFINITY), DVec2::splat(f64::NEG_INFINITY)),
                    |(lo, hi), &k| (lo.min(uv.uvs[k as usize]), hi.max(uv.uvs[k as usize])),
                );
                (lo + hi) / 2.0
            }
        };
        for k in ids {
            let k = k as usize;
            if !std::mem::replace(&mut moved[k], true) {
                uv.uvs[k] = p + m * (uv.uvs[k] - p) + t;
            }
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("script line {line}: {message}")]
pub struct ScriptError {
    pub line: usize,
    pub message: String,
}

/// Parses a script: a JSON array of commands, or JSON Lines (one command per
/// line; blank lines and lines starting with `#` or `//` are ignored).
pub fn parse_script(text: &str) -> Result<Vec<Command>, ScriptError> {
    if text.trim_start().starts_with('[') {
        return serde_json::from_str(text).map_err(|e| ScriptError {
            line: e.line(),
            message: e.to_string(),
        });
    }
    text.lines()
        .enumerate()
        .filter(|(_, l)| {
            let l = l.trim();
            !(l.is_empty() || l.starts_with('#') || l.starts_with("//"))
        })
        .map(|(i, l)| {
            serde_json::from_str(l).map_err(|e| ScriptError {
                line: i + 1,
                message: e.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two separate quads: island 0 at u∈[0,0.5], island 1 at u∈[0.5,1].
    const OBJ: &str = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 2 0 0\nv 2 1 0\n\
vt 0 0\nvt 0.5 0\nvt 0.5 1\nvt 0 1\nvt 0.5 0\nvt 1 0\nvt 1 1\nvt 0.5 1\n\
f 1/1 2/2 3/3 4/4\nf 2/5 5/6 6/7 3/8\n";

    /// Temporary directory removed when dropped.
    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn session() -> (Session, TempDir) {
        let dir = std::env::temp_dir().join(format!(
            "uvcraft-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("in.obj");
        std::fs::write(&path, OBJ).unwrap();
        let mut s = Session::new();
        s.execute(Command::Load { path: path.clone() }).unwrap();
        (s, TempDir(dir))
    }

    fn uvs(s: &Session) -> Vec<DVec2> {
        s.document().unwrap().uv().uvs.clone()
    }

    #[test]
    fn transform_undo_redo() {
        let (mut s, _dir) = session();
        let original = uvs(&s);
        s.execute(
            parse_script(r#"{"cmd":"transform","islands":[1],"translate":[0,1]}"#)
                .unwrap()
                .remove(0),
        )
        .unwrap();
        let moved = uvs(&s);
        assert_eq!(moved[..4], original[..4]);
        assert_eq!(moved[5], DVec2::new(1.0, 1.0));

        s.execute(Command::Undo).unwrap();
        assert_eq!(uvs(&s), original);
        s.execute(Command::Redo).unwrap();
        assert_eq!(uvs(&s), moved);
        assert!(matches!(
            s.execute(Command::Redo),
            Err(CommandError::NothingToRedo)
        ));
        // Failed commands are not logged.
        assert_eq!(s.log().len(), 4);
    }

    #[test]
    fn rotate_about_island_center() {
        let (mut s, _dir) = session();
        s.execute(Command::Transform {
            islands: vec![0],
            translate: [0.0; 2],
            rotate_deg: 90.0,
            scale: [1.0; 2],
            pivot: Pivot::IslandCenter,
        })
        .unwrap();
        // Island 0 bbox [0,0.5]×[0,1], center (0.25,0.5): (0,0) → (0.75,0.25).
        let p = uvs(&s)[0];
        assert!((p - DVec2::new(0.75, 0.25)).length() < 1e-12, "{p}");
    }

    #[test]
    fn save_round_trip_and_analyze() {
        let (mut s, dir) = session();
        let out = dir.0.join("same.obj");
        s.execute(Command::Save { path: out.clone() }).unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), OBJ);
        assert!(matches!(
            s.execute(Command::Save {
                path: dir.0.join("x.glb")
            }),
            Err(CommandError::File {
                source: IoError::FormatMismatch { .. },
                ..
            })
        ));

        let Outcome::Report(r) = s
            .execute(Command::Analyze {
                resolution: 1024,
                output: None,
            })
            .unwrap()
        else {
            panic!("expected a report");
        };
        assert_eq!(r.island_count, 2);
        assert_eq!(r.seam_edges, 1);
    }

    #[test]
    fn m1_pipeline_and_rollback() {
        let (mut s, _dir) = session();
        let script = r#"
            {"cmd":"project","mode":{"planar":"z"}}
            {"cmd":"auto_seams","angle_deg":60}
            {"cmd":"unfold"}
            {"cmd":"pack","resolution":256}
            {"cmd":"set_texel_density","density":100,"resolution":1024}
        "#;
        for cmd in parse_script(script).unwrap() {
            s.execute(cmd).unwrap();
        }
        let Outcome::Report(r) = s
            .execute(Command::Analyze {
                resolution: 1024,
                output: None,
            })
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(r.island_count, 2);
        assert!((r.max_angle_distortion - 1.0).abs() < 1e-6);
        assert!(
            r.islands
                .iter()
                .all(|i| (i.texel_density - 100.0).abs() < 1e-6)
        );

        // A failing command leaves the UVs and the undo stack untouched.
        let before = uvs(&s);
        let undo_depth = s.undo.len();
        assert!(matches!(
            s.execute(Command::Cut {
                edges: vec![0, 999]
            }),
            Err(CommandError::InvalidEdge { index: 999, .. })
        ));
        assert_eq!(uvs(&s), before);
        assert_eq!(s.undo.len(), undo_depth);

        // Cut then undo restores island count.
        s.execute(Command::Weld {
            edges: (0..7).collect(),
        })
        .unwrap();
        let islands_after_weld = s.doc.as_mut().unwrap().islands().count();
        assert_eq!(islands_after_weld, 1);
        s.execute(Command::Undo).unwrap();
        assert_eq!(s.doc.as_mut().unwrap().islands().count(), 2);
        // No-op edits do not create undo entries.
        let depth = s.undo.len();
        s.execute(Command::Cut { edges: vec![] }).unwrap();
        assert_eq!(s.undo.len(), depth);
    }

    #[test]
    fn errors_and_script_parsing() {
        let (mut s, _dir) = session();
        let bad = Command::Transform {
            islands: vec![5],
            translate: [0.0; 2],
            rotate_deg: 0.0,
            scale: [1.0; 2],
            pivot: Pivot::Origin,
        };
        assert!(matches!(
            s.execute(bad),
            Err(CommandError::InvalidIsland { index: 5, count: 2 })
        ));
        assert!(!s.can_undo());

        let script = "# comment\n{\"cmd\":\"undo\"}\n\n{\"cmd\":\"analyze\"}\n";
        assert_eq!(
            parse_script(script).unwrap(),
            vec![
                Command::Undo,
                Command::Analyze {
                    resolution: 2048,
                    output: None
                }
            ]
        );
        let err = parse_script("{\"cmd\":\"undo\"}\n{\"cmd\":\"bogus\"}").unwrap_err();
        assert_eq!(err.line, 2);
        assert!(
            parse_script(r#"{"cmd":"transform","rotate":1}"#).is_err(),
            "unknown fields are rejected"
        );

        let log = s.log_jsonl();
        assert_eq!(parse_script(&log).unwrap(), s.log());
    }
}
