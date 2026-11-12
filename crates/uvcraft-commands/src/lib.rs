//! UvCraft command layer.
//!
//! Every user action is a serializable [`Command`]. The UI, the CLI, scripts,
//! macros and (later) Python all go through [`Session::execute`], so they share
//! one code path for undo/redo and the command log.

use std::collections::BTreeMap;
use std::path::PathBuf;

use glam::{DMat2, DVec2};
use serde::{Deserialize, Serialize};
use uvcraft_algo::analysis::{self, Report};
use uvcraft_algo::ao;
use uvcraft_algo::autocut;
use uvcraft_algo::border_preserve;
use uvcraft_algo::box_planar;
use uvcraft_algo::brush::{self, BrushMode};
use uvcraft_algo::density_brush;
use uvcraft_algo::density_map::DensityMap;
use uvcraft_algo::grid;
use uvcraft_algo::layout::{
    self, AlignTo, DistributeBy, OrientMode, SnapTarget, StraightenPath, UvAxis,
};
use uvcraft_algo::mosaic::{self, MosaicParams};
use uvcraft_algo::optimize::{self, OptimizeMethod};
use uvcraft_algo::overlap_guard;
use uvcraft_algo::pack::{self, PackMethod, PackOptions, PackQuality};
use uvcraft_algo::pelt;
use uvcraft_algo::physical::{self, Unit};
use uvcraft_algo::projection::{self, Projection};
use uvcraft_algo::select::{IslandMetric, IslandProblem};
use uvcraft_algo::slim;
use uvcraft_algo::stack::{self, StackMode};
use uvcraft_algo::unfold::UnfoldMethod;
use uvcraft_algo::validate::{self, MeshProblems};
use uvcraft_algo::{seams, select, unfold};
use uvcraft_core::{Cancelled, Islands, Mesh, Progress, UvLayer};
use uvcraft_io::{IoError, Source};

mod flat_pattern;
mod history;
pub mod island_groups;
mod load_uvs;
mod stack_relations;
use island_groups::{GroupBy, IslandGroups, TileDistribution};
use stack_relations::{StackRelations, sidecar_path};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pivot {
    /// Center of each island's UV bounding box.
    #[default]
    IslandCenter,
    /// UV origin (0, 0).
    Origin,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IslandMultiplier {
    pub island: usize,
    pub scale: f64,
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
fn default_iterations() -> usize {
    20
}
fn default_pin_drag_iterations() -> usize {
    8
}
fn default_pelt_branches() -> usize {
    6
}
fn default_density_brush_iterations() -> usize {
    12
}

fn default_mosaic_iterations() -> usize {
    10
}
fn default_box_smoothness() -> f64 {
    0.25
}
fn default_stretch_target() -> f64 {
    1.1
}

fn default_padding() -> f64 {
    4.0
}
fn yes() -> bool {
    true
}
fn default_snap_tolerance() -> f64 {
    4.0
}

fn default_tolerance() -> f64 {
    0.05
}

fn default_max_stack_distortion() -> f64 { 4.0 }
fn default_weld_tolerance() -> f64 {
    0.001
}
fn default_group_scale() -> f64 {
    1.0
}

/// One user action. Serialized as JSON with a `cmd` tag, e.g.
/// `{"cmd":"transform","rotate_deg":90}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    /// List glTF TEXCOORD channels and the currently active channel.
    ListUvChannels,
    /// Make an existing glTF TEXCOORD channel active for editing.
    SwitchUvChannel {
        channel: u32,
    },
    /// Create a glTF TEXCOORD channel by copying the active UV layer, then select it.
    CreateUvChannel {
        channel: u32,
    },
    /// Exchange two glTF TEXCOORD channel numbers.
    SwapUvChannels {
        a: u32,
        b: u32,
    },
    /// Load a mesh (.obj, .gltf, .glb), replacing the current document and clearing history.
    Load {
        path: PathBuf,
    },
    /// Save the current document in its original format (OBJ, glTF or GLB), topology preserved.
    Save {
        path: PathBuf,
    },
    /// Export island cut contours as a dimensioned SVG flat pattern.
    ExportFlatPattern {
        path: PathBuf,
        #[serde(default)]
        islands: Vec<usize>,
        unit: Unit,
        #[serde(default)]
        margin: f64,
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
    /// Apply one weighted UV brush stroke as one undoable edit.
    BrushEdit {
        vertices: Vec<usize>,
        weights: Vec<f64>,
        mode: BrushMode,
        delta: [f64; 2],
        strength: f64,
        iterations: usize,
    },
    /// Paint target texel density on faces and optionally relax UVs immediately.
    DensityPaint {
        faces: Vec<usize>,
        weights: Vec<f64>,
        target_density: f64,
        resolution: u32,
        strength: f64,
        #[serde(default = "default_density_brush_iterations")]
        optimize_iterations: usize,
    },
    /// Cut mesh edges into seams (UV vertices split where the cut separates faces).
    Cut {
        edges: Vec<usize>,
    },
    /// Clear coincident UV seams without moving their islands.
    ClearSeams {
        edges: Vec<usize>,
    },
    /// Weld seam edges. Separate islands are aligned onto each other first.
    Weld {
        edges: Vec<usize>,
    },
    /// Weld seam edges whose two pairs of UV endpoints are within tolerance.
    AutoWeld {
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default = "default_weld_tolerance")]
        tolerance_uv: f64,
    },
    /// Convert OBJ smoothing-group discontinuities into UV seams.
    HardEdgesToSeams,
    /// Write UV seams as OBJ smoothing-group boundaries (when representable).
    SeamsToHardEdges,
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
    /// Cut seams between six dominant-normal planar charts after label smoothing.
    BoxPlanarSeams {
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default = "default_box_smoothness")]
        smoothness: f64,
        #[serde(default = "default_mosaic_iterations")]
        iterations: usize,
    },
    /// Iteratively cut seams that reduce measured UV stretch after unfolding.
    CutReduceStretch {
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default = "default_mosaic_iterations")]
        max_cuts: usize,
        #[serde(default = "default_stretch_target")]
        target_distortion: f64,
    },
    /// Grow geodesic seam branches, open topology to disks and unfold organically.
    Pelt {
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default = "default_pelt_branches")]
        branches: usize,
        #[serde(default = "yes")]
        unfold: bool,
        #[serde(default = "default_iterations")]
        relax_iterations: usize,
    },
    /// Project islands (`"box"`, `"best"` or `{"planar": "x"|"y"|"z"}`) and fit them into 0–1.
    Project {
        #[serde(default)]
        islands: Vec<usize>,
        mode: Projection,
    },
    /// Flatten islands with LSCM, or ABF++ with `"method":"abf"` (keeps each
    /// island's area and center).
    Unfold {
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default)]
        method: UnfoldMethod,
        #[serde(default)]
        preserve_orientation: bool,
        /// Soft penalty for retaining the current border length and shape.
        #[serde(default)]
        border_weight: f64,
        /// Backtrack the solver step if it creates any new overlapping faces.
        #[serde(default)]
        prevent_overlap: bool,
    },
    /// Relax islands with ARAP (less area distortion than LSCM). Run after `unfold`.
    Optimize {
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default = "default_iterations")]
        iterations: usize,
        #[serde(default)]
        method: OptimizeMethod,
        #[serde(default)]
        preserve_orientation: bool,
        /// Soft penalty for retaining the current border length and shape.
        #[serde(default)]
        border_weight: f64,
        /// Backtrack the solver step if it creates any new overlapping faces.
        #[serde(default)]
        prevent_overlap: bool,
    },
    /// Optimize current UVs with a weighted conformal/area distortion energy.
    OptimizeAngleArea {
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default = "default_iterations")]
        iterations: usize,
        /// 0 favors angles; 1 favors area.
        area_weight: f64,
    },
    /// Improve an existing flip-free UV layout with SLIM without reinitializing it.
    Refine {
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default = "default_iterations")]
        iterations: usize,
    },
    /// Pack islands into the 0–1 tile.
    Pack {
        #[serde(default)]
        islands: Vec<usize>,
        /// Put each group/material/object category in its own UDIM tile.
        #[serde(default)]
        distribute_by: Option<TileDistribution>,
        /// Relative UV scale for selected islands, applied before layout.
        #[serde(default)]
        island_multipliers: Vec<IslandMultiplier>,
        /// Each group is packed as one rigid item; other islands pack individually.
        #[serde(default)]
        groups: Vec<Vec<usize>>,
        /// Stack matching islands first, then pack each stack as one item.
        #[serde(default)]
        stack_similar: bool,
        /// Keep stacks created by `stack` together as rigid packing items.
        #[serde(default = "yes")]
        preserve_stacks: bool,
        /// Place each item's UV origin on the texture pixel grid.
        #[serde(default)]
        pixel_aligned: bool,
        /// Search for a tighter layout while verifying exact island spacing.
        #[serde(default)]
        padding_perfect: bool,
        #[serde(default)]
        stack_mode: StackMode,
        #[serde(default = "default_tolerance")]
        stack_tolerance: f64,
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
        #[serde(default)]
        method: PackMethod,
        /// Scale islands to fill the space (false: keep size, shrink only if needed).
        #[serde(default = "yes")]
        rescale: bool,
        /// When packing some islands only, pack around the others.
        #[serde(default = "yes")]
        around_others: bool,
        /// Pack into `[umin, vmin, umax, vmax]` instead of the 0–1 tile.
        #[serde(default)]
        region: Option<[f64; 4]>,
        /// Multi-tile (UDIM): at most this many tiles; islands keep their size.
        #[serde(default)]
        tiles: Option<u32>,
        /// Multi-tile (UDIM): tiles per row (1–10, default 10).
        #[serde(default)]
        tiles_u: Option<u32>,
        /// Also try mirrored islands.
        #[serde(default)]
        mirror: bool,
        /// Try rotations in steps of this many degrees (overrides `rotate_90`).
        #[serde(default)]
        rotation_step_deg: Option<f64>,
        #[serde(default)]
        quality: PackQuality,
        /// Padding / margin in UV units (override the pixel values).
        #[serde(default)]
        padding_uv: Option<f64>,
        #[serde(default)]
        margin_uv: Option<f64>,
        /// Keep this texel density (pixels per 3D unit at `resolution`) instead of filling.
        #[serde(default)]
        texel_density: Option<f64>,
    },
    /// Scale islands to a texel density (pixels per 3D unit at `resolution`).
    SetTexelDensity {
        #[serde(default)]
        islands: Vec<usize>,
        density: f64,
        #[serde(default = "default_resolution")]
        resolution: u32,
    },
    /// Scale each island from a grayscale PGM density map sampled in UV space.
    ApplyDensityMap {
        path: PathBuf,
        #[serde(default)]
        islands: Vec<usize>,
        min_scale: f64,
        max_scale: f64,
    },
    /// Bake a grayscale ambient-occlusion PGM in the active 0–1 UV tile.
    BakeAmbientOcclusion {
        path: PathBuf,
        resolution: usize,
        rays: usize,
        max_distance: f64,
    },
    /// Keep physical 3D/UV area at 1:1 scale without normalizing UVs to a tile.
    SetRealScale {
        #[serde(default)]
        islands: Vec<usize>,
        mesh_unit: Unit,
        uv_unit: Unit,
    },
    /// Project planar islands with exact physical edge lengths; reject warped geometry.
    SetExactRealScale {
        #[serde(default)]
        islands: Vec<usize>,
        mesh_unit: Unit,
        uv_unit: Unit,
    },
    /// Measure selected mesh edges and islands in a physical unit.
    MeasurePhysical {
        #[serde(default)]
        edges: Vec<usize>,
        #[serde(default)]
        islands: Vec<usize>,
        mesh_unit: Unit,
        output_unit: Unit,
    },
    /// Pack into a physical rectangle without shrinking the current UV scale.
    PackPhysical {
        #[serde(default)]
        islands: Vec<usize>,
        width: f64,
        height: f64,
        unit: Unit,
        uv_unit: Unit,
        #[serde(default)]
        padding: f64,
        #[serde(default)]
        margin: f64,
    },
    /// Equalize texel density across islands while preserving their total UV area.
    EqualizeTexelDensity {
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default = "default_resolution")]
        resolution: u32,
    },
    /// Blender-style Average Island Scale: give selected islands one common
    /// texel density without changing their combined UV area or centers.
    AverageIslandScale {
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default = "default_resolution")]
        resolution: u32,
    },
    /// Pin (or unpin) mesh vertices: every UV copy of them stays put during
    /// `unfold`. Two or more pins in an island fix its placement and scale.
    Pin {
        vertices: Vec<usize>,
        #[serde(default = "yes")]
        pinned: bool,
    },
    /// Move one pinned mesh vertex in UV space and relax connected islands.
    DragPin {
        vertex: usize,
        uv: [f64; 2],
        #[serde(default = "default_pin_drag_iterations")]
        iterations: usize,
    },
    ClearPins,
    /// Lock whole UV islands, tracked by their mesh faces across seam edits.
    LockIslands {
        islands: Vec<usize>,
        #[serde(default = "yes")]
        locked: bool,
    },
    /// Lock mesh polygons in place during unfold and optimization.
    LockFaces {
        faces: Vec<usize>,
        #[serde(default = "yes")]
        locked: bool,
    },
    ClearLocks,
    /// Create a named island group; optionally nest existing groups beneath it.
    CreateIslandGroup {
        name: String,
        #[serde(default)]
        parent: Option<u64>,
        #[serde(default)]
        children: Vec<u64>,
        #[serde(default)]
        islands: Vec<usize>,
    },
    AddIslandsToGroup {
        group: u64,
        islands: Vec<usize>,
    },
    RemoveIslandsFromGroup {
        group: u64,
        islands: Vec<usize>,
    },
    UngroupIslands {
        group: u64,
    },
    DeleteIslandGroup {
        group: u64,
    },
    /// One collective UV transform around the group's bounding-box center.
    TransformIslandGroup {
        group: u64,
        #[serde(default)]
        translate: [f64; 2],
        #[serde(default)]
        rotate_deg: f64,
        #[serde(default = "default_group_scale")]
        scale: f64,
    },
    /// Scale a whole group to an explicitly requested aggregate density.
    SetIslandGroupTexelDensity {
        group: u64,
        density: f64,
        resolution: u32,
    },
    /// Match aggregate density of other document islands without changing
    /// the selected group's internal proportions or layout.
    AverageIslandGroupScale {
        group: u64,
        resolution: u32,
    },
    SetIslandGroup {
        group: u64,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        color: Option<[u8; 4]>,
        #[serde(default)]
        locked: Option<bool>,
        #[serde(default)]
        visible: Option<bool>,
        #[serde(default)]
        selected: Option<bool>,
    },
    MoveIslandGroup {
        group: u64,
        parent: Option<u64>,
    },
    SelectIslandGroup {
        group: u64,
    },
    /// Put islands into named groups by imported face metadata.
    OrganizeIslandGroups {
        by: GroupBy,
        #[serde(default)]
        parent: Option<u64>,
        #[serde(default)]
        islands: Vec<usize>,
    },
    /// Segment islands into near-planar / developable charts (VSA), cut the
    /// chart borders, cut each chart to a disk and (optionally) unfold.
    /// Existing seams are kept. `charts`: total count wanted (automatic from
    /// `max_error` when absent).
    Mosaic {
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default)]
        charts: Option<usize>,
        /// Mean `|n − N|²` per chart (≈ deviation² in radians); default 0.1.
        #[serde(default)]
        max_error: Option<f64>,
        /// Maximum total 3D length of new seam edges. Automatic charts may
        /// relax `max_error` to fit; an explicit chart count is kept fixed.
        #[serde(default)]
        max_seam_length: Option<f64>,
        /// Maximum measured angle/area stretch after unfold (1 = isometric).
        /// A failing bound rolls back the entire command.
        #[serde(default)]
        max_uv_distortion: Option<f64>,
        #[serde(default = "default_mosaic_iterations")]
        iterations: usize,
        #[serde(default = "yes")]
        unfold: bool,
    },
    Undo,
    Redo,
    /// Compute a quality report; optionally write it to a JSON file.
    Analyze {
        #[serde(default = "default_resolution")]
        resolution: u32,
        #[serde(default)]
        output: Option<PathBuf>,
        /// Report island pairs closer than this many pixels (0: skip).
        #[serde(default)]
        padding_px: f64,
    },
    /// Find islands with a UV quality problem. Returns sorted island indices.
    SelectIslands {
        problem: IslandProblem,
    },
    /// Query islands by texel density or distortion. Bounds are inclusive.
    SelectIslandsByMetric {
        metric: IslandMetric,
        #[serde(default)]
        min: Option<f64>,
        #[serde(default)]
        max: Option<f64>,
        #[serde(default = "default_resolution")]
        resolution: u32,
    },
    /// Select OBJ faces assigned to a smoothing group; null selects `s off` faces.
    SelectFacesBySmoothingGroup {
        group: Option<u32>,
    },
    /// Select OBJ or glTF faces by exact material name; null selects unassigned faces.
    SelectFacesByMaterial {
        material: Option<String>,
    },
    /// Select a connected face region through edges at or below this 3D dihedral angle.
    SelectFacesByAngle {
        seed_face: usize,
        max_angle_deg: f64,
    },
    /// Replace the UVs with those of another file (.obj, .gltf, .glb) that has
    /// the same topology: same faces in the same order, same corner counts.
    LoadUvs {
        path: PathBuf,
    },
    /// Mirror islands along `axis` (`"u"` flips horizontally) about each
    /// island's center, or about the line at coordinate `at`.
    Mirror {
        #[serde(default)]
        islands: Vec<usize>,
        axis: UvAxis,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<f64>,
    },
    /// Rotate islands by `"min_bbox"`, `"longest_edge"`, `"world_axis"` or
    /// `{"edge": N}`. `vertical` puts the reference along V (default: only
    /// for `world_axis`).
    Orient {
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default)]
        mode: OrientMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vertical: Option<bool>,
    },
    /// Line islands up along `axis` on their common `min`/`max`/`center`, or on `value`.
    Align {
        #[serde(default)]
        islands: Vec<usize>,
        axis: UvAxis,
        #[serde(default)]
        to: AlignTo,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<f64>,
    },
    /// Space islands evenly along `axis` by `"gaps"` or `"centers"` (first and
    /// last stay put), or with a fixed `gap`.
    Distribute {
        #[serde(default)]
        islands: Vec<usize>,
        axis: UvAxis,
        #[serde(default)]
        by: DistributeBy,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gap: Option<f64>,
    },
    /// Snap islands to the texel grid (`"pixel"`, `"pixel_center"`, `"bbox"`)
    /// or onto other islands' vertices (`"vertex"`, within `tolerance_px`).
    /// The grid step is `step` UV units, default 1 / `resolution`.
    Snap {
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default)]
        to: SnapTarget,
        #[serde(default = "default_resolution")]
        resolution: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<f64>,
        #[serde(default = "default_snap_tolerance")]
        tolerance_px: f64,
    },
    /// Write the macro (the edits currently applied since the last load,
    /// undone ones dropped) as a replayable JSON Lines script.
    SaveMacro {
        path: PathBuf,
    },
    /// Report groups of similar islands (reference first) without changing anything.
    Similar {
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default)]
        mode: StackMode,
        /// Relative tolerance on UV boundary alignment (`similar` mode).
        #[serde(default = "default_tolerance")]
        tolerance: f64,
    },
    /// Stack islands by UV boundary. Exact mode retains the legacy
    /// topology-based UV copy; similar mode preserves internal UVs.
    Stack {
        #[serde(default)]
        islands: Vec<usize>,
        #[serde(default)]
        mode: StackMode,
        #[serde(default = "default_tolerance")]
        tolerance: f64,
        /// Explicit reference island. It must belong to the selection.
        #[serde(default)]
        reference: Option<usize>,
        /// Compute the placements and diagnostics without changing UVs.
        #[serde(default)]
        preview: bool,
        /// Snap boundary UVs and relax the interior with a fixed boundary.
        #[serde(default)]
        conform: bool,
        #[serde(default = "default_max_stack_distortion")]
        max_distortion: f64,
    },
    /// Move stacked (overlapping) islands apart into a row, ready for packing.
    Unstack {
        #[serde(default)]
        islands: Vec<usize>,
    },
    /// Place targets onto `source`: copy its UVs when the topology matches
    /// (copy/paste UVs), else align their outline onto it.
    Match {
        source: usize,
        targets: Vec<usize>,
    },
    /// Cut edges, then unfold the islands they touch.
    CutUnfold {
        edges: Vec<usize>,
    },
    /// Cut the edge loop through `edge` (quad loop, or straightest path on triangles).
    CutLoop {
        edge: usize,
        #[serde(default)]
        unfold: bool,
    },
    /// Cut the shortest path through `vertices` (at least two), in order.
    CutPath {
        vertices: Vec<usize>,
        #[serde(default)]
        unfold: bool,
    },
    /// Cut the border of a face selection (edges between selected and other faces).
    CutBorder {
        faces: Vec<usize>,
        #[serde(default)]
        unfold: bool,
    },
    /// Straighten an ordered vertex or edge path within one UV island.
    StraightenPath {
        island: usize,
        #[serde(default)]
        vertices: Vec<usize>,
        #[serde(default)]
        edges: Vec<usize>,
    },
    /// Align an ordered island-border path horizontally (`u`) or vertically (`v`).
    AlignBorder {
        island: usize,
        axis: UvAxis,
        #[serde(default)]
        vertices: Vec<usize>,
        #[serde(default)]
        edges: Vec<usize>,
    },
    /// Map a complete rectangular quad-grid island to an orthogonal UV lattice.
    Rectangularize {
        island: usize,
    },
    /// Straighten a curved quad-grid island (including a one-cell-wide strip)
    /// into an orthogonal layout. Rejects islands without a valid grid.
    StraightenIsland {
        island: usize,
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
    #[error(transparent)]
    Cancelled(#[from] Cancelled),
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
    PhysicalReport(Box<PhysicalReport>),
    Selection {
        islands: Vec<usize>,
    },
    FaceSelection {
        faces: Vec<usize>,
    },
    /// Done, with a message worth showing (e.g. skipped islands).
    Info(String),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PhysicalReport {
    pub output_unit: Unit,
    pub edge_lengths: Vec<PhysicalEdge>,
    pub island_areas: Vec<PhysicalIsland>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PhysicalEdge {
    pub edge: usize,
    pub length: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PhysicalIsland {
    pub island: usize,
    pub area: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelSchemaOp {
    Create(u32),
    Swap(u32, u32),
}

/// The loaded mesh plus its editable UV layer.
#[derive(Debug, Clone)]
pub struct Document {
    pub path: PathBuf,
    pub mesh: Mesh,
    /// Geometry used for 3D measurements; glTF node transforms affect only this copy.
    measurement_mesh: Option<Mesh>,
    /// glTF mesh indices measured in local space (instanced, skinned or animated).
    measurement_local_meshes: Vec<usize>,
    uv: UvLayer,
    active_uv_channel: u32,
    uv_channels: BTreeMap<u32, UvLayer>,
    channel_schema: Vec<ChannelSchemaOp>,
    source: Source,
    smoothing_override: Option<Vec<Option<u32>>>,
    pub objects: Vec<String>,
    face_objects: Vec<u32>,
    pub missing_uv_corners: usize,
    /// UV vertices duplicated at load so that no two islands share one.
    pub split_uv_vertices: usize,
    islands: Option<Islands>,
    /// Pinned mesh vertices.
    pins: Vec<bool>,
    locked_island_faces: Vec<bool>,
    locked_faces: Vec<bool>,
    island_groups: IslandGroups,
    stack_relations: StackRelations,
    density_targets: Vec<Option<f64>>,
    /// Mesh problems, found once at load (the mesh never changes).
    pub mesh_problems: MeshProblems,
}

impl Document {
    fn is_fbx(&self) -> bool {
        matches!(
            uvcraft_io::Format::from_path(&self.path),
            Some(uvcraft_io::Format::Fbx)
        ) && matches!(self.source, Source::Obj(_))
    }

    fn effective_source(&self) -> Result<Source, CommandError> {
        let Source::Gltf(original) = &self.source else {
            return Ok(self.source.clone());
        };
        let mut source = original.clone();
        for op in &self.channel_schema {
            source = match *op {
                ChannelSchemaOp::Create(c) => source.add_empty_uv_channel(c),
                ChannelSchemaOp::Swap(a, b) => source.swap_uv_channels(a, b),
            }
            .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
        }
        Ok(Source::Gltf(source))
    }

    pub fn measurement_mesh(&self) -> &Mesh {
        self.measurement_mesh.as_ref().unwrap_or(&self.mesh)
    }

    pub fn measurement_local_meshes(&self) -> &[usize] {
        &self.measurement_local_meshes
    }

    pub fn face_objects(&self) -> &[u32] {
        &self.face_objects
    }

    /// Active UV channel and available channel indices for glTF or FBX.
    pub fn uv_channel_info(&self) -> Option<(u32, Vec<u32>)> {
        if self.is_fbx() {
            let mut channels: Vec<u32> = self.uv_channels.keys().copied().collect();
            channels.push(self.active_uv_channel);
            channels.sort_unstable();
            channels.dedup();
            return Some((self.active_uv_channel, channels));
        }
        let Source::Gltf(source) = self.effective_source().ok()? else {
            return None;
        };
        Some((self.active_uv_channel, source.uv_channels()))
    }

    pub fn uv(&self) -> &UvLayer {
        &self.uv
    }

    /// Pinned flag per mesh vertex.
    pub fn pins(&self) -> &[bool] {
        &self.pins
    }

    pub fn locked_faces(&self) -> &[bool] {
        &self.locked_faces
    }

    pub fn locked_island_faces(&self) -> &[bool] {
        &self.locked_island_faces
    }

    pub fn island_groups(&self) -> &IslandGroups {
        &self.island_groups
    }

    pub fn stack_group_count(&self) -> usize {
        self.stack_relations.group_count()
    }

    /// Island indices in intentional stacks. These pairs may overlap in UV
    /// space without being an accidental collision.
    pub fn stack_groups(&self) -> Vec<Vec<usize>> {
        let computed;
        let islands = if let Some(islands) = &self.islands {
            islands
        } else {
            computed = Islands::compute(&self.mesh, &self.uv);
            &computed
        };
        let all: Vec<usize> = (0..islands.count()).collect();
        self.stack_relations
            .packing_groups(islands, &all)
            .unwrap_or_default()
    }

    pub fn density_targets(&self) -> &[Option<f64>] {
        &self.density_targets
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

/// What undo/redo restore: everything editable (the mesh never changes).
#[derive(Debug, Clone, PartialEq)]
struct Snapshot {
    uv: UvLayer,
    pins: Vec<bool>,
    locked_island_faces: Vec<bool>,
    locked_faces: Vec<bool>,
    island_groups: IslandGroups,
    stack_relations: StackRelations,
    density_targets: Vec<Option<f64>>,
    active_uv_channel: u32,
    uv_channels: BTreeMap<u32, UvLayer>,
    channel_schema: Vec<ChannelSchemaOp>,
    smoothing_override: Option<Vec<Option<u32>>>,
}

impl Document {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            uv: self.uv.clone(),
            pins: self.pins.clone(),
            locked_island_faces: self.locked_island_faces.clone(),
            locked_faces: self.locked_faces.clone(),
            island_groups: self.island_groups.clone(),
            stack_relations: self.stack_relations.clone(),
            density_targets: self.density_targets.clone(),
            active_uv_channel: self.active_uv_channel,
            uv_channels: self.uv_channels.clone(),
            channel_schema: self.channel_schema.clone(),
            smoothing_override: self.smoothing_override.clone(),
        }
    }

    fn restore(&mut self, s: Snapshot) {
        self.set_uv(s.uv);
        self.pins = s.pins;
        self.locked_island_faces = s.locked_island_faces;
        self.locked_faces = s.locked_faces;
        self.island_groups = s.island_groups;
        self.stack_relations = s.stack_relations;
        self.density_targets = s.density_targets;
        self.active_uv_channel = s.active_uv_channel;
        self.uv_channels = s.uv_channels;
        self.channel_schema = s.channel_schema;
        self.smoothing_override = s.smoothing_override;
    }
}

/// Editing session. Undo/redo store full snapshots of the UV layer and pins.
#[derive(Debug, Default, Clone)]
pub struct Session {
    doc: Option<Document>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    log: Vec<Command>,
    /// The command behind each undo snapshot (same length as `undo`).
    history: Vec<Command>,
    /// The command behind each redo snapshot (same length as `redo`).
    undone: Vec<Command>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn document(&self) -> Option<&Document> {
        self.doc.as_ref()
    }

    /// Carry UvCraft-only group/stack intent across an external mesh refresh
    /// when every face and corner still refers to the same mesh vertex.
    pub fn retain_group_metadata_from(&mut self, previous: &Session) -> Result<(), CommandError> {
        let old = previous.doc.as_ref().ok_or(CommandError::NoDocument)?;
        let incoming = self.doc.as_mut().ok_or(CommandError::NoDocument)?;
        let a = &old.mesh;
        let b = &incoming.mesh;
        if a.vertex_count() != b.vertex_count()
            || a.face_count() != b.face_count()
            || a.corner_count() != b.corner_count()
            || (0..a.face_count())
                .any(|face| a.face_corners(face).count() != b.face_corners(face).count())
            || (0..a.corner_count()).any(|corner| {
                a.corner_face(corner) != b.corner_face(corner)
                    || a.corner_vertex(corner) != b.corner_vertex(corner)
            })
        {
            return Err(CommandError::InvalidArgument(
                "mesh topology changed; group metadata cannot be transferred".into(),
            ));
        }
        let islands = incoming.islands().clone();
        incoming.island_groups = old.island_groups.clone();
        incoming.island_groups.reconcile(&islands);
        incoming.stack_relations = old.stack_relations.clone();
        incoming.stack_relations.prune(&islands);
        Ok(())
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
        self.execute_with_progress(cmd, &Progress::default())
    }

    /// Runs one command with progress events and cooperative cancellation.
    /// A cancelled command records no log or undo entry.
    pub fn execute_with_progress(
        &mut self,
        cmd: Command,
        progress: &Progress,
    ) -> Result<Outcome, CommandError> {
        let undo_depth = self.undo.len();
        let outcome = self.apply(&cmd, progress)?;
        self.record_history(&cmd, undo_depth);
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
        let before = doc.snapshot();
        match f(doc) {
            Ok(outcome) => {
                if doc.uv.corner_uv != before.uv.corner_uv {
                    let islands = doc.islands.as_ref().map_or_else(
                        || Islands::compute(&doc.mesh, &doc.uv),
                        |cached| cached.update(&doc.mesh, &before.uv, &doc.uv),
                    );
                    islands.split_shared_uvs(&doc.mesh, &mut doc.uv);
                    doc.stack_relations.prune(&islands);
                    doc.island_groups.reconcile(&islands);
                    doc.islands = Some(islands);
                }
                if doc.uv != before.uv && doc.stack_relations.group_count() > 0 {
                    let islands = doc.islands.get_or_insert_with(|| Islands::compute(&doc.mesh, &doc.uv));
                    doc.stack_relations.retain_valid(&doc.mesh, &doc.uv, islands);
                }
                if doc.uv != before.uv
                    || doc.pins != before.pins
                    || doc.locked_island_faces != before.locked_island_faces
                    || doc.locked_faces != before.locked_faces
                    || doc.island_groups != before.island_groups
                    || doc.stack_relations != before.stack_relations
                    || doc.density_targets != before.density_targets
                    || doc.active_uv_channel != before.active_uv_channel
                    || doc.uv_channels != before.uv_channels
                    || doc.channel_schema != before.channel_schema
                    || doc.smoothing_override != before.smoothing_override
                {
                    self.undo.push(before);
                    self.redo.clear();
                }
                Ok(outcome)
            }
            Err(e) => {
                doc.restore(before);
                Err(e)
            }
        }
    }

    fn apply(&mut self, cmd: &Command, progress: &Progress) -> Result<Outcome, CommandError> {
        progress.check()?;
        match cmd {
            Command::Load { path } => {
                progress.report("load", 0, 2)?;
                let parsed = uvcraft_io::read(path).map_err(|source| CommandError::File {
                    path: path.clone(),
                    source,
                })?;
                let (measurement_mesh, measurement_local_meshes) = match &parsed.source {
                    Source::Gltf(source) => {
                        let (positions, local) = source.measurement_positions(&parsed.mesh);
                        let measured = parsed
                            .mesh
                            .with_positions(positions)
                            .expect("same vertex count");
                        (Some(measured), local)
                    }
                    Source::Obj(_) => (None, Vec::new()),
                };
                let mut uv = parsed.uv;
                let vertex_count = parsed.mesh.vertex_count();
                let face_count = parsed.mesh.face_count();
                let islands = Islands::compute(&parsed.mesh, &uv);
                let split_uv_vertices = islands.split_shared_uvs(&parsed.mesh, &mut uv);
                let mesh_problems = validate::validate(&parsed.mesh);
                let (stack_relations, island_groups) = std::fs::read(sidecar_path(path))
                    .ok()
                    .zip(std::fs::read(path).ok())
                    .and_then(|(sidecar, model)| {
                        StackRelations::project_from_sidecar(&sidecar, &model, &islands, face_count)
                    })
                    .unwrap_or_else(|| (StackRelations::default(), IslandGroups::new(face_count)));
                progress.report("load", 1, 2)?;
                self.doc = Some(Document {
                    path: path.clone(),
                    mesh: parsed.mesh,
                    measurement_mesh,
                    measurement_local_meshes,
                    uv,
                    active_uv_channel: 0,
                    uv_channels: parsed.uv_channels,
                    channel_schema: Vec::new(),
                    source: parsed.source,
                    smoothing_override: None,
                    objects: parsed.objects,
                    face_objects: parsed.face_object,
                    missing_uv_corners: parsed.missing_uv_corners,
                    split_uv_vertices,
                    islands: Some(islands),
                    pins: vec![false; vertex_count],
                    locked_island_faces: vec![false; face_count],
                    locked_faces: vec![false; face_count],
                    island_groups,
                    stack_relations,
                    density_targets: vec![None; face_count],
                    mesh_problems,
                });
                self.undo.clear();
                self.redo.clear();
                progress.finish("load", 2);
            }
            Command::Save { path } => {
                progress.report("save", 0, 2)?;
                let doc = self.doc.as_ref().ok_or(CommandError::NoDocument)?;
                let source = doc.effective_source()?;
                let prepared = if matches!(
                    uvcraft_io::Format::from_path(path),
                    Some(uvcraft_io::Format::Fbx)
                ) && (!doc.uv_channels.is_empty() || doc.active_uv_channel != 0)
                {
                    let mut layers = doc.uv_channels.clone();
                    layers.insert(doc.active_uv_channel, doc.uv.clone());
                    uvcraft_io::prepare_write_uv_channels(
                        path,
                        &source,
                        &doc.mesh,
                        &layers,
                        doc.active_uv_channel,
                    )
                } else if matches!(
                    uvcraft_io::Format::from_path(path),
                    Some(uvcraft_io::Format::Obj)
                ) {
                    match (&source, &doc.smoothing_override) {
                        (Source::Obj(obj), Some(groups)) => obj
                            .export_with_smoothing(&doc.mesh, &doc.uv, groups)
                            .map(|bytes| vec![(path.clone(), bytes)])
                            .map_err(uvcraft_io::IoError::from),
                        _ => uvcraft_io::prepare_write(path, &source, &doc.mesh, &doc.uv),
                    }
                } else if matches!(source, Source::Gltf(_))
                    && (!doc.uv_channels.is_empty() || !doc.channel_schema.is_empty())
                {
                    let mut layers = doc.uv_channels.clone();
                    layers.insert(doc.active_uv_channel, doc.uv.clone());
                    uvcraft_io::prepare_write_uv_channels(
                        path,
                        &source,
                        &doc.mesh,
                        &layers,
                        doc.active_uv_channel,
                    )
                } else if doc.active_uv_channel != 0 {
                    let mut layers = BTreeMap::new();
                    layers.insert(doc.active_uv_channel, doc.uv.clone());
                    uvcraft_io::prepare_write_uv_channels(
                        path,
                        &source,
                        &doc.mesh,
                        &layers,
                        doc.active_uv_channel,
                    )
                } else {
                    uvcraft_io::prepare_write(path, &source, &doc.mesh, &doc.uv)
                };
                let mut files = prepared.map_err(|source| CommandError::File {
                    path: path.clone(),
                    source,
                })?;
                let sidecar = sidecar_path(path);
                if (doc.stack_relations.group_count() > 0
                    || doc.island_groups.group_count() > 0
                    || sidecar.exists())
                    && let Some((_, bytes)) = files.iter().find(|(file, _)| file == path)
                {
                    let metadata = doc
                        .stack_relations
                        .project_sidecar_bytes(&doc.island_groups, bytes)
                        .map_err(|error| CommandError::InvalidArgument(error.to_string()))?;
                    files.push((sidecar, metadata));
                }
                progress.report("save", 1, 2)?;
                uvcraft_io::commit_prepared(files).map_err(|source| CommandError::File {
                    path: path.clone(),
                    source,
                })?;
                progress.finish("save", 2);
            }
            Command::ExportFlatPattern {
                path,
                islands,
                unit,
                margin,
            } => {
                if !path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
                {
                    return Err(CommandError::InvalidArgument(
                        "flat pattern output must have a .svg extension".into(),
                    ));
                }
                let doc = self.doc.as_ref().ok_or(CommandError::NoDocument)?;
                let current_islands = Islands::compute(&doc.mesh, &doc.uv);
                let svg = flat_pattern::render(
                    &doc.mesh,
                    &doc.uv,
                    &current_islands,
                    islands,
                    *unit,
                    *margin,
                )
                .map_err(CommandError::InvalidArgument)?;
                uvcraft_io::commit_prepared(vec![(path.clone(), svg.into_bytes())]).map_err(
                    |source| CommandError::File {
                        path: path.clone(),
                        source,
                    },
                )?;
            }
            Command::ListUvChannels => {
                let doc = self.doc.as_ref().ok_or(CommandError::NoDocument)?;
                if doc.is_fbx() {
                    let (active, channels) = doc.uv_channel_info().expect("FBX has active channel");
                    return Ok(Outcome::Info(format!(
                        "active UV_{active}; available: {}",
                        channels
                            .iter()
                            .map(|n| format!("UV_{n}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
                let Source::Gltf(source) = doc.effective_source()? else {
                    return Err(CommandError::InvalidArgument(
                        "UV channels require glTF".into(),
                    ));
                };
                let channels = source.uv_channels();
                return Ok(Outcome::Info(format!(
                    "active TEXCOORD_{}; available: {}",
                    doc.active_uv_channel,
                    channels
                        .iter()
                        .map(|n| format!("TEXCOORD_{n}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            Command::SwitchUvChannel { channel } => {
                return self.edit(|doc| {
                    if doc.is_fbx() {
                        if *channel == doc.active_uv_channel {
                            return Ok(Outcome::Done);
                        }
                        let next = doc.uv_channels.remove(channel).ok_or_else(|| {
                            CommandError::InvalidArgument(format!("UV_{channel} does not exist"))
                        })?;
                        doc.uv_channels
                            .insert(doc.active_uv_channel, doc.uv.clone());
                        doc.set_uv(next);
                        doc.active_uv_channel = *channel;
                        return Ok(Outcome::Done);
                    }
                    let Source::Gltf(source) = doc.effective_source()? else {
                        return Err(CommandError::InvalidArgument(
                            "UV channels require glTF".into(),
                        ));
                    };
                    if *channel == doc.active_uv_channel {
                        return Ok(Outcome::Done);
                    }
                    let next = match doc.uv_channels.get(channel) {
                        Some(layer) => layer.clone(),
                        None => source
                            .activate_uv_channel(&doc.mesh, *channel)
                            .map_err(|e| CommandError::InvalidArgument(e.to_string()))?
                            .map(|(_, uv)| uv)
                            .ok_or_else(|| {
                                CommandError::InvalidArgument(format!(
                                    "TEXCOORD_{channel} does not exist"
                                ))
                            })?,
                    };
                    doc.uv_channels
                        .insert(doc.active_uv_channel, doc.uv.clone());
                    doc.set_uv(next);
                    doc.active_uv_channel = *channel;
                    Ok(Outcome::Done)
                });
            }
            Command::CreateUvChannel { channel } => {
                return self.edit(|doc| {
                    if doc.is_fbx() {
                        if *channel == doc.active_uv_channel
                            || doc.uv_channels.contains_key(channel)
                        {
                            return Err(CommandError::InvalidArgument(format!(
                                "UV_{channel} already exists"
                            )));
                        }
                        doc.uv_channels
                            .insert(doc.active_uv_channel, doc.uv.clone());
                        let mut next = doc.uv.clone();
                        next.name = format!("UVSet{channel}");
                        doc.set_uv(next);
                        doc.active_uv_channel = *channel;
                        return Ok(Outcome::Done);
                    }
                    let Source::Gltf(source) = doc.effective_source()? else {
                        return Err(CommandError::InvalidArgument(
                            "UV channels require glTF".into(),
                        ));
                    };
                    if source.uv_channels().contains(channel) {
                        return Err(CommandError::InvalidArgument(format!(
                            "TEXCOORD_{channel} already exists"
                        )));
                    }
                    doc.uv_channels
                        .insert(doc.active_uv_channel, doc.uv.clone());
                    let mut new_layer = doc.uv.clone();
                    new_layer.name = format!("TEXCOORD_{channel}");
                    doc.set_uv(new_layer);
                    doc.active_uv_channel = *channel;
                    doc.channel_schema.push(ChannelSchemaOp::Create(*channel));
                    Ok(Outcome::Done)
                });
            }
            Command::SwapUvChannels { a, b } => {
                return self.edit(|doc| {
                    if doc.is_fbx() {
                        if a == b {
                            return Ok(Outcome::Done);
                        }
                        let mut layers = doc.uv_channels.clone();
                        layers.insert(doc.active_uv_channel, doc.uv.clone());
                        let first = layers.remove(a).ok_or_else(|| {
                            CommandError::InvalidArgument("both UV channels must exist".into())
                        })?;
                        let second = layers.remove(b).ok_or_else(|| {
                            CommandError::InvalidArgument("both UV channels must exist".into())
                        })?;
                        layers.insert(*a, second);
                        layers.insert(*b, first);
                        let active = if doc.active_uv_channel == *a {
                            *b
                        } else if doc.active_uv_channel == *b {
                            *a
                        } else {
                            doc.active_uv_channel
                        };
                        let next = layers
                            .remove(&active)
                            .expect("active FBX channel remains present");
                        doc.uv_channels = layers;
                        doc.set_uv(next);
                        doc.active_uv_channel = active;
                        return Ok(Outcome::Done);
                    }
                    let Source::Gltf(source) = doc.effective_source()? else {
                        return Err(CommandError::InvalidArgument(
                            "UV channels require glTF".into(),
                        ));
                    };
                    let available = source.uv_channels();
                    if !available.contains(a) || !available.contains(b) {
                        return Err(CommandError::InvalidArgument(
                            "both UV channels must exist".into(),
                        ));
                    }
                    if a == b {
                        return Ok(Outcome::Done);
                    }
                    let a_layer = doc.uv_channels.remove(a);
                    let b_layer = doc.uv_channels.remove(b);
                    if let Some(mut layer) = a_layer {
                        layer.name = format!("TEXCOORD_{b}");
                        doc.uv_channels.insert(*b, layer);
                    }
                    if let Some(mut layer) = b_layer {
                        layer.name = format!("TEXCOORD_{a}");
                        doc.uv_channels.insert(*a, layer);
                    }
                    if doc.active_uv_channel == *a {
                        doc.active_uv_channel = *b;
                    } else if doc.active_uv_channel == *b {
                        doc.active_uv_channel = *a;
                    }
                    doc.uv.name = format!("TEXCOORD_{}", doc.active_uv_channel);
                    doc.channel_schema.push(ChannelSchemaOp::Swap(*a, *b));
                    Ok(Outcome::Done)
                });
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
            Command::BrushEdit {
                vertices,
                weights,
                mode,
                delta,
                strength,
                iterations,
            } => {
                return self.edit(|doc| {
                    let pins = doc.effective_pins();
                    brush::edit(
                        &doc.mesh,
                        &mut doc.uv,
                        &pins,
                        vertices,
                        weights,
                        *mode,
                        DVec2::from(*delta),
                        *strength,
                        *iterations,
                    )
                    .map_err(CommandError::InvalidArgument)?;
                    Ok(Outcome::Done)
                });
            }
            Command::DensityPaint {
                faces,
                weights,
                target_density,
                resolution,
                strength,
                optimize_iterations,
            } => {
                return self.edit(|doc| {
                    if faces.len() != weights.len()
                        || faces.iter().any(|&face| face >= doc.mesh.face_count())
                        || weights
                            .iter()
                            .any(|&weight| !weight.is_finite() || !(0.0..=1.0).contains(&weight))
                    {
                        return Err(CommandError::InvalidArgument(
                            "invalid density brush face or weight".into(),
                        ));
                    }
                    let mut editable_faces = Vec::new();
                    let mut editable_weights = Vec::new();
                    for (&face, &weight) in faces.iter().zip(weights) {
                        if !doc.locked_faces[face]
                            && !doc.locked_island_faces[face]
                            && !doc.island_groups.face_locked(face)
                        {
                            editable_faces.push(face);
                            editable_weights.push(weight);
                        }
                    }
                    let changed = density_brush::paint_targets(
                        &doc.mesh,
                        &doc.uv,
                        &mut doc.density_targets,
                        &editable_faces,
                        &editable_weights,
                        *target_density,
                        *resolution,
                        *strength,
                    )
                    .map_err(CommandError::InvalidArgument)?;
                    let mut optimized = 0;
                    if *optimize_iterations > 0 && changed > 0 {
                        let mut pins = doc.effective_pins();
                        let mut active = doc.density_targets.clone();
                        for (face, target) in active.iter_mut().enumerate() {
                            if doc.locked_faces[face]
                                || doc.locked_island_faces[face]
                                || doc.island_groups.face_locked(face)
                            {
                                *target = None;
                                for corner in doc.mesh.face_corners(face) {
                                    pins[doc.mesh.corner_vertex(corner)] = true;
                                }
                            }
                        }
                        optimized = density_brush::optimize_targets(
                            &doc.mesh,
                            &mut doc.uv,
                            &active,
                            &pins,
                            *resolution,
                            *optimize_iterations,
                        )
                        .map_err(CommandError::InvalidArgument)?;
                    }
                    Ok(Outcome::Info(format!(
                        "painted {changed} face(s), optimized {optimized} step(s)"
                    )))
                });
            }
            Command::Cut { edges } => {
                return self.edit(|doc| {
                    check_edges(&doc.mesh, edges)?;
                    seams::cut_edges(&doc.mesh, &mut doc.uv, edges);
                    Ok(Outcome::Done)
                });
            }
            Command::ClearSeams { edges } => {
                return self.edit(|doc| {
                    check_edges(&doc.mesh, edges)?;
                    let count = seams::clear_aligned_seams(&doc.mesh, &mut doc.uv, edges, 1e-6)
                        .map_err(CommandError::InvalidArgument)?;
                    Ok(Outcome::Info(format!("cleared {count} seam(s)")))
                });
            }
            Command::Weld { edges } => {
                return self.edit(|doc| {
                    check_edges(&doc.mesh, edges)?;
                    seams::weld_edges(&doc.mesh, &mut doc.uv, edges);
                    Ok(Outcome::Done)
                });
            }
            Command::AutoWeld {
                islands,
                tolerance_uv,
            } => {
                if !tolerance_uv.is_finite() || *tolerance_uv < 0.0 {
                    return Err(CommandError::InvalidArgument(
                        "tolerance_uv must be finite and ≥ 0".into(),
                    ));
                }
                return self.edit(|doc| {
                    let targets = resolve_islands(doc, islands)?;
                    let welded =
                        seams::auto_weld_edges(&doc.mesh, &mut doc.uv, &targets, *tolerance_uv);
                    Ok(Outcome::Info(format!("welded {welded} edge(s)")))
                });
            }
            Command::HardEdgesToSeams => {
                return self.edit(|doc| {
                    let Source::Obj(source) = &doc.source else {
                        return Err(CommandError::InvalidArgument(
                            "smoothing groups require an OBJ source".into(),
                        ));
                    };
                    let groups = doc
                        .smoothing_override
                        .as_deref()
                        .unwrap_or(source.face_smoothing());
                    let mut edges = seams::hard_edges_from_smoothing(&doc.mesh, groups);
                    edges.extend(source.normal_discontinuity_edges(&doc.mesh));
                    edges.sort_unstable();
                    edges.dedup();
                    let added = seams::cut_edges(&doc.mesh, &mut doc.uv, &edges);
                    Ok(Outcome::Info(format!(
                        "cut {} hard edge(s), added {added} UV vertex(es)",
                        edges.len()
                    )))
                });
            }
            Command::SeamsToHardEdges => {
                return self.edit(|doc| {
                    if !matches!(doc.source, Source::Obj(_)) {
                        return Err(CommandError::InvalidArgument(
                            "smoothing groups require an OBJ source".into(),
                        ));
                    }
                    let (groups, support) =
                        seams::smoothing_groups_covering_seams(&doc.mesh, &doc.uv);
                    doc.smoothing_override = Some(groups);
                    Ok(if support.is_empty() {
                        Outcome::Done
                    } else {
                        Outcome::Info(format!(
                            "added {} supporting hard edge(s) to represent seam cycles",
                            support.len()
                        ))
                    })
                });
            }
            Command::AutoSeams {
                islands,
                angle_deg,
                ensure_disk,
            } => {
                return self.edit(|doc| {
                    progress.report("auto_seams", 0, 1)?;
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
                    progress.report("auto_seams", 1, 1)?;
                    Ok(Outcome::Done)
                });
            }
            Command::BoxPlanarSeams {
                islands,
                smoothness,
                iterations,
            } => {
                if !smoothness.is_finite() || *smoothness < 0.0 || *iterations > 1000 {
                    return Err(CommandError::InvalidArgument(
                        "smoothness must be finite and nonnegative; iterations must be at most 1000".into(),
                    ));
                }
                return self.edit(|doc| {
                    let targets = resolve_pack_islands(doc, islands)?;
                    let Document {
                        mesh, uv, islands, ..
                    } = doc;
                    let current = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    let mut selected_faces = vec![false; mesh.face_count()];
                    for target in targets {
                        for &face in current.faces(target) {
                            selected_faces[face as usize] = true;
                        }
                    }
                    let cuts: Vec<_> = box_planar::seam_edges(mesh, *smoothness, *iterations)
                        .into_iter()
                        .filter(|&edge| {
                            let (corner, _) = mesh.edge_corners(edge);
                            selected_faces[mesh.corner_face(corner)]
                        })
                        .collect();
                    let added = seams::cut_edges(mesh, uv, &cuts);
                    Ok(Outcome::Info(format!(
                        "box-planar seams: {} edges, {} UV vertices split",
                        cuts.len(),
                        added
                    )))
                });
            }
            Command::CutReduceStretch {
                islands,
                max_cuts,
                target_distortion,
            } => {
                if *max_cuts > 64 || !target_distortion.is_finite() || *target_distortion < 1.0 {
                    return Err(CommandError::InvalidArgument(
                        "max_cuts must be at most 64 and target_distortion at least 1".into(),
                    ));
                }
                return self.edit(|doc| {
                    let targets = resolve_pack_islands(doc, islands)?;
                    if targets.is_empty() {
                        return Ok(Outcome::Info("all target islands are locked".into()));
                    }
                    let current = doc.islands.as_ref().expect("resolved above");
                    let mut selected_faces = vec![false; doc.mesh.face_count()];
                    for target in targets {
                        for &face in current.faces(target) {
                            selected_faces[face as usize] = true;
                        }
                    }
                    if (0..doc.mesh.corner_count()).any(|corner| {
                        selected_faces[doc.mesh.corner_face(corner)]
                            && doc.pins[doc.mesh.corner_vertex(corner)]
                    }) {
                        return Err(CommandError::InvalidArgument(
                            "unpin target islands before automatic stretch cuts".into(),
                        ));
                    }
                    let result = autocut::cut_reduce_stretch(
                        &doc.mesh,
                        &mut doc.uv,
                        &selected_faces,
                        *max_cuts,
                        *target_distortion,
                        progress,
                    )?;
                    Ok(Outcome::Info(format!(
                        "cut {} edge(s); worst distortion {:.4} → {:.4}",
                        result.cuts, result.before, result.after
                    )))
                });
            }
            Command::Pelt {
                islands,
                branches,
                unfold,
                relax_iterations,
            } => {
                if *branches == 0 || *branches > 128 || *relax_iterations > 1000 {
                    return Err(CommandError::InvalidArgument(
                        "pelt branches must be in 1..=128 and relax_iterations at most 1000".into(),
                    ));
                }
                return self.edit(|doc| {
                    let targets = resolve_pack_islands(doc, islands)?;
                    if targets.is_empty() {
                        return Ok(Outcome::Info("all target islands are locked".into()));
                    }
                    let pins = doc.effective_pins();
                    let original = doc.islands().clone();
                    let mesh = &doc.mesh;
                    let mut selected_faces = vec![false; mesh.face_count()];
                    for &island in &targets {
                        for &face in original.faces(island) {
                            selected_faces[face as usize] = true;
                        }
                    }
                    let graph =
                        pelt::geodesic_cut_graph(mesh, &doc.uv, &original, &targets, *branches);
                    let uv = &mut doc.uv;
                    seams::cut_edges(mesh, uv, &graph);
                    progress.check()?;
                    let after_graph = Islands::compute(mesh, uv);
                    let mut current: Vec<usize> = (0..mesh.face_count())
                        .filter(|&face| selected_faces[face])
                        .map(|face| after_graph.island_of_face(face))
                        .collect();
                    current.sort_unstable();
                    current.dedup();
                    let disk = seams::cut_to_disk(mesh, uv, &current);
                    progress.check()?;
                    let final_islands = Islands::compute(mesh, uv);
                    let mut current: Vec<usize> = (0..mesh.face_count())
                        .filter(|&face| selected_faces[face])
                        .map(|face| final_islands.island_of_face(face))
                        .collect();
                    current.sort_unstable();
                    current.dedup();
                    let mut note = format!(
                        "pelt cut {} geodesic and {} disk edge(s) into {} chart(s)",
                        graph.len(),
                        disk.len(),
                        current.len()
                    );
                    if *unfold {
                        let report = unfold::unfold_with_progress(
                            mesh,
                            uv,
                            &final_islands,
                            &current,
                            &pins,
                            UnfoldMethod::Abf,
                            progress,
                        )?;
                        note.push_str(&format!("; unfolded {} chart(s)", report.unfolded));
                        if !report.failed.is_empty() || !report.skipped_closed.is_empty() {
                            note.push_str(&format!(
                                "; could not unfold {:?}, closed {:?}",
                                report.failed, report.skipped_closed
                            ));
                        }
                        if *relax_iterations > 0 && report.unfolded > 0 {
                            let relaxed = slim::optimize_slim_with_progress(
                                mesh,
                                uv,
                                &final_islands,
                                &current,
                                &pins,
                                *relax_iterations,
                                progress,
                            )?;
                            note.push_str(&format!("; relaxed {} chart(s)", relaxed.optimized));
                        }
                    }
                    Ok(Outcome::Info(note))
                });
            }
            Command::Project { islands, mode } => {
                if let Projection::CameraOrthographic(camera) = mode
                    && !camera.valid()
                {
                    return Err(CommandError::InvalidArgument(
                        "camera orthographic projection requires finite, independent screen axes and positive aspect".into(),
                    ));
                }
                return self.edit(|doc| {
                    progress.report("project", 0, 1)?;
                    let targets = resolve_pack_islands(doc, islands)?;
                    if targets.is_empty() {
                        return Ok(Outcome::Info("all target islands are locked".into()));
                    }
                    if let Projection::CameraPerspective(camera) = mode {
                        projection::project_camera_perspective(
                            &doc.mesh,
                            &mut doc.uv,
                            &targets,
                            *camera,
                        )
                        .map_err(CommandError::InvalidArgument)?;
                    } else {
                        projection::project(&doc.mesh, &mut doc.uv, &targets, *mode);
                    }
                    progress.report("project", 1, 1)?;
                    Ok(Outcome::Done)
                });
            }
            Command::Unfold {
                islands,
                method,
                preserve_orientation,
                border_weight,
                prevent_overlap,
            } => {
                if !border_weight.is_finite() || *border_weight < 0.0 {
                    return Err(CommandError::InvalidArgument(
                        "border_weight must be finite and ≥ 0".into(),
                    ));
                }
                return self.edit(|doc| {
                    let targets = resolve_unlocked_islands(doc, islands)?;
                    if targets.is_empty() {
                        return Ok(Outcome::Info("all target islands are locked".into()));
                    }
                    let effective_pins = doc.effective_pins();
                    let Document {
                        mesh,
                        uv,
                        islands,
                        ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    let before = (*preserve_orientation || *border_weight > 0.0 || *prevent_overlap).then(|| uv.clone());
                    let r = unfold::unfold_with_progress(mesh, uv, isl, &targets, &effective_pins, *method, progress)?;
                    if *preserve_orientation && let Some(before) = before.as_ref() {
                        layout::preserve_previous_orientation(mesh, before, uv, isl, &targets, &effective_pins);
                    }
                    if *border_weight > 0.0 {
                        border_preserve::preserve_border_shape(mesh, before.as_ref().unwrap(), uv, isl, &targets, &effective_pins, *border_weight)
                            .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                    }
                    if *prevent_overlap {
                        overlap_guard::prevent_new_overlaps(mesh,before.as_ref().unwrap(),uv,isl)
                            .map_err(CommandError::InvalidArgument)?;
                    }
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
                    if !r.abf_fallback.is_empty() {
                        notes.push(format!(
                            "ABF did not converge on island(s) {:?}: used LSCM",
                            r.abf_fallback
                        ));
                    }
                    Ok(if notes.is_empty() { Outcome::Done } else { Outcome::Info(notes.join("; ")) })
                });
            }
            Command::Optimize {
                islands,
                iterations,
                method,
                preserve_orientation,
                border_weight,
                prevent_overlap,
            } => {
                if !border_weight.is_finite() || *border_weight < 0.0 {
                    return Err(CommandError::InvalidArgument(
                        "border_weight must be finite and ≥ 0".into(),
                    ));
                }
                if *iterations == 0 || *iterations > 10_000 {
                    return Err(CommandError::InvalidArgument(
                        "iterations must be in 1..=10000".into(),
                    ));
                }
                return self.edit(|doc| {
                    let targets = resolve_unlocked_islands(doc, islands)?;
                    if targets.is_empty() {
                        return Ok(Outcome::Info("all target islands are locked".into()));
                    }
                    let effective_pins = doc.effective_pins();
                    let Document {
                        mesh, uv, islands, ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    let before =
                        (*preserve_orientation || *border_weight > 0.0 || *prevent_overlap)
                            .then(|| uv.clone());
                    let (failed, hint) = match method {
                        OptimizeMethod::Arap => (
                            optimize::optimize_arap_with_progress(
                                mesh,
                                uv,
                                isl,
                                &targets,
                                &effective_pins,
                                *iterations,
                                progress,
                            )?
                            .failed,
                            "unfold them first",
                        ),
                        OptimizeMethod::Slim => (
                            slim::optimize_slim_with_progress(
                                mesh,
                                uv,
                                isl,
                                &targets,
                                &effective_pins,
                                *iterations,
                                progress,
                            )?
                            .failed,
                            "they must be disks (auto_seams), and pinned ones must start unflipped",
                        ),
                    };
                    if *preserve_orientation && let Some(before) = before.as_ref() {
                        layout::preserve_previous_orientation(
                            mesh,
                            before,
                            uv,
                            isl,
                            &targets,
                            &effective_pins,
                        );
                    }
                    if *border_weight > 0.0 {
                        border_preserve::preserve_border_shape(
                            mesh,
                            before.as_ref().unwrap(),
                            uv,
                            isl,
                            &targets,
                            &effective_pins,
                            *border_weight,
                        )
                        .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                    }
                    if *prevent_overlap {
                        overlap_guard::prevent_new_overlaps(
                            mesh,
                            before.as_ref().unwrap(),
                            uv,
                            isl,
                        )
                        .map_err(CommandError::InvalidArgument)?;
                    }
                    Ok(if failed.is_empty() {
                        Outcome::Done
                    } else {
                        Outcome::Info(format!("could not optimize island(s) {failed:?}: {hint}"))
                    })
                });
            }
            Command::OptimizeAngleArea {
                islands,
                iterations,
                area_weight,
            } => {
                if *iterations == 0
                    || *iterations > 10_000
                    || !area_weight.is_finite()
                    || !(0.0..=1.0).contains(area_weight)
                {
                    return Err(CommandError::InvalidArgument(
                        "iterations must be in 1..=10000 and area_weight in 0..=1".into(),
                    ));
                }
                return self.edit(|doc| {
                    let targets = resolve_unlocked_islands(doc, islands)?;
                    if targets.is_empty() {
                        return Ok(Outcome::Info("all target islands are locked".into()));
                    }
                    let effective_pins = doc.effective_pins();
                    let Document {
                        mesh, uv, islands, ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    let failed = slim::optimize_angle_area_with_progress(
                        mesh,
                        uv,
                        isl,
                        &targets,
                        &effective_pins,
                        *iterations,
                        *area_weight,
                        progress,
                    )?
                    .failed;
                    Ok(if failed.is_empty() {
                        Outcome::Done
                    } else {
                        Outcome::Info(format!("could not optimize island(s) {failed:?}"))
                    })
                });
            }
            Command::Refine {
                islands,
                iterations,
            } => {
                if *iterations == 0 || *iterations > 10_000 {
                    return Err(CommandError::InvalidArgument(
                        "iterations must be in 1..=10000".into(),
                    ));
                }
                return self.edit(|doc| {
                    let targets = resolve_unlocked_islands(doc, islands)?;
                    if targets.is_empty() {
                        return Ok(Outcome::Info("all target islands are locked".into()));
                    }
                    let effective_pins = doc.effective_pins();
                    let Document {
                        mesh, uv, islands, ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    let report = slim::refine_existing_with_progress(
                        mesh,
                        uv,
                        isl,
                        &targets,
                        &effective_pins,
                        *iterations,
                        progress,
                    )?;
                    Ok(if report.failed.is_empty() {
                        Outcome::Done
                    } else {
                        Outcome::Info(format!(
                            "could not refine island(s) {:?}: existing UVs must be flip-free",
                            report.failed
                        ))
                    })
                });
            }
            Command::Pack {
                islands,
                distribute_by,
                island_multipliers,
                groups,
                stack_similar,
                preserve_stacks,
                pixel_aligned,
                padding_perfect,
                stack_mode,
                stack_tolerance,
                padding_px,
                margin_px,
                resolution,
                orient,
                rotate_90,
                method,
                rescale,
                around_others,
                region,
                tiles,
                tiles_u,
                mirror,
                rotation_step_deg,
                quality,
                padding_uv,
                margin_uv,
                texel_density,
            } => {
                let options = PackOptions {
                    padding_px: *padding_px,
                    margin_px: *margin_px,
                    resolution: *resolution,
                    orient: *orient,
                    rotate_90: *rotate_90,
                    method: *method,
                    rescale: *rescale,
                    around_others: *around_others,
                    region: *region,
                    tiles: *tiles,
                    tiles_u: *tiles_u,
                    mirror: *mirror,
                    rotation_step_deg: *rotation_step_deg,
                    quality: *quality,
                    padding_uv: *padding_uv,
                    margin_uv: *margin_uv,
                    texel_density: *texel_density,
                    pixel_aligned: *pixel_aligned,
                };
                options.validate().map_err(CommandError::InvalidArgument)?;
                if *padding_perfect && options.multi_tile() {
                    return Err(CommandError::InvalidArgument(
                        "padding_perfect does not support multiple tiles".into(),
                    ));
                }
                if distribute_by.is_some()
                    && (!options.multi_tile()
                        || options.region.is_some()
                        || !groups.is_empty()
                        || *stack_similar)
                {
                    return Err(CommandError::InvalidArgument(
                        "distribute_by needs UDIM tiles and cannot combine with region, packing groups or stack_similar".into(),
                    ));
                }
                if *stack_similar {
                    check_tolerance(*stack_tolerance)?;
                    if !groups.is_empty() || !island_multipliers.is_empty() {
                        return Err(CommandError::InvalidArgument(
                            "stack_similar cannot be combined with explicit packing groups or island multipliers".into(),
                        ));
                    }
                }
                return self.edit(|doc| {
                    let targets = resolve_pack_islands(doc, islands)?;
                    if targets.is_empty() {
                        return Ok(Outcome::Info("all target islands are locked".into()));
                    }
                    let topology = doc.islands().clone();
                    let mut document_groups = Vec::new();
                    for node in doc.island_groups.nodes().filter(|node| node.parent.is_none()) {
                        let members = doc.island_groups.islands_in(node.id, &topology)
                            .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                        if members.len() < 2 { continue; }
                        let selected = members.iter().filter(|member| targets.contains(member)).count();
                        if selected > 0 && selected < members.len() {
                            return Err(CommandError::InvalidArgument(format!(
                                "group {} is only partly editable or selected; select and unlock every member before packing", node.id
                            )));
                        }
                        if selected == members.len() { document_groups.push(members); }
                    }
                    if *stack_similar && !document_groups.is_empty() {
                        return Err(CommandError::InvalidArgument(
                            "stack_similar cannot be combined with document island groups".into(),
                        ));
                    }
                    if !document_groups.is_empty() && !island_multipliers.is_empty() {
                        return Err(CommandError::InvalidArgument(
                            "island multipliers cannot be used while packing document island groups".into(),
                        ));
                    }
                    let distribution = if let Some(by) = *distribute_by {
                        if targets.len() != topology.count() {
                            return Err(CommandError::InvalidArgument(
                                "distribute_by requires all islands to be editable and selected".into(),
                            ));
                        }
                        let buckets = tile_distribution_buckets(doc, &topology, &targets, by);
                        if buckets.len() > options.tiles.unwrap_or(pack::MAX_TILES) as usize {
                            return Err(CommandError::InvalidArgument(format!(
                                "{} categories need at least {} UDIM tiles", buckets.len(), buckets.len()
                            )));
                        }
                        Some(buckets)
                    } else { None };
                    let Document {
                        mesh, uv, islands, stack_relations, ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    let original_uv = uv.clone();
                    for multiplier in island_multipliers {
                        if !targets.contains(&multiplier.island) {
                            return Err(CommandError::InvalidArgument(format!(
                                "island {} multiplier is outside the editable selection",
                                multiplier.island
                            )));
                        }
                    }
                    let mut seen = vec![false; isl.count()];
                    for group in groups {
                        if group.len() < 2 {
                            return Err(CommandError::InvalidArgument("packing groups need at least two islands".into()));
                        }
                        for &i in group {
                            if i >= isl.count() || !targets.contains(&i) || seen[i] {
                                return Err(CommandError::InvalidArgument(format!("island {i} is invalid, unselected or in multiple packing groups")));
                            }
                            seen[i] = true;
                        }
                    }
                    let mut layout_options = options;
                    if !island_multipliers.is_empty() {
                        if let Some(density) = options.texel_density {
                            pack::set_texel_density(mesh, uv, isl, &targets, density, options.resolution);
                            layout_options.texel_density = None;
                            layout_options.rescale = false;
                        }
                        let factors: Vec<_> = island_multipliers.iter().map(|m| (m.island, m.scale)).collect();
                        pack::apply_island_multipliers(mesh, uv, isl, &factors)
                            .map_err(CommandError::InvalidArgument)?;
                    }
                    let auto_groups = if *stack_similar {
                        // Apply density before stacking so a later per-island
                        // density pass cannot separate matching UVs again.
                        if let Some(density) = options.texel_density {
                            pack::set_texel_density(mesh, uv, isl, &targets, density, options.resolution);
                            layout_options.texel_density = None;
                            layout_options.rescale = false;
                        }
                        stack::stack(mesh, uv, isl, &targets, *stack_mode, *stack_tolerance).groups
                    } else {
                        Vec::new()
                    };
                    let recorded_groups = if *preserve_stacks && !*stack_similar {
                        stack_relations
                            .packing_groups(isl, &targets)
                            .map_err(|message| CommandError::InvalidArgument(message.into()))?
                    } else {
                        Vec::new()
                    };
                    if !recorded_groups.is_empty() && !island_multipliers.is_empty() {
                        return Err(CommandError::InvalidArgument(
                            "packing a recorded stack with island multipliers could separate its members; disable preserve_stacks first".into(),
                        ));
                    }
                    let mut pack_groups = groups.clone();
                    for named in document_groups {
                        let overlaps: Vec<usize> = pack_groups.iter().enumerate()
                            .filter_map(|(index, existing)| existing.iter().any(|island| named.contains(island)).then_some(index))
                            .collect();
                        if overlaps.len() > 1 || overlaps.first().is_some_and(|&index| {
                            !pack_groups[index].iter().all(|island| named.contains(island))
                                && !named.iter().all(|island| pack_groups[index].contains(island))
                        }) {
                            return Err(CommandError::InvalidArgument(
                                "explicit packing group crosses a document island group".into(),
                            ));
                        }
                        if let Some(index) = overlaps.first().copied() {
                            if named.len() > pack_groups[index].len() { pack_groups[index] = named; }
                        } else { pack_groups.push(named); }
                    }
                    pack_groups.extend(auto_groups.iter().cloned());
                    for group in recorded_groups {
                        if let Some(existing) = pack_groups.iter().find(|existing| {
                            group.iter().any(|island| existing.contains(island))
                        }) {
                            if group.iter().all(|island| existing.contains(island)) {
                                continue;
                            }
                            return Err(CommandError::InvalidArgument(
                                "a recorded stack crosses a packing group boundary".into(),
                            ));
                        }
                        for &island in &group {
                            seen[island] = true;
                        }
                        pack_groups.push(group);
                    }
                    let r = if let Some(buckets) = distribution {
                        if pack_groups.iter().any(|group| {
                            !buckets
                                .iter()
                                .any(|bucket| group.iter().all(|island| bucket.contains(island)))
                        }) {
                            return Err(CommandError::InvalidArgument(
                                "a recorded stack crosses UDIM distribution categories; disable preserve_stacks or choose another distribution".into(),
                            ));
                        }
                        let columns = options.tiles_u.unwrap_or(pack::UDIM_COLUMNS) as usize;
                        let mut combined = pack::PackResult { scale: 1.0, tiles_used: buckets.len(), ..Default::default() };
                        let mut per_tile = layout_options;
                        per_tile.tiles = None;
                        per_tile.tiles_u = None;
                        per_tile.around_others = false;
                        per_tile.rescale = false;
                        for (tile, bucket) in buckets.iter().enumerate() {
                            let bucket_groups: Vec<Vec<usize>> = pack_groups
                                .iter()
                                .filter(|group| group.iter().all(|island| bucket.contains(island)))
                                .cloned()
                                .collect();
                            let packed = pack::pack_grouped_with_progress(mesh, uv, isl, bucket, &per_tile, &bucket_groups, progress)?;
                            if packed.failed {
                                combined.failed = true;
                                break;
                            }
                            combined.scale = combined.scale.min(packed.scale);
                            let offset = DVec2::new((tile % columns) as f64, (tile / columns) as f64);
                            for &island in bucket {
                                for id in isl.uv_indices(island, mesh, uv) {
                                    uv.uvs[id as usize] += offset;
                                }
                            }
                        }
                        combined
                    } else if *padding_perfect {
                        pack::pack_padding_perfect_with_progress(mesh, uv, isl, &targets, &layout_options, &pack_groups, progress)?
                    } else {
                        pack::pack_grouped_with_progress(mesh, uv, isl, &targets, &layout_options, &pack_groups, progress)?
                    };
                    let mut notes = Vec::new();
                    if r.failed {
                        *uv = original_uv;
                        return Ok(Outcome::Info("packing did not fit; UVs kept unchanged".into()));
                    }
                    if !auto_groups.is_empty() {
                        stack_relations.record(isl, &auto_groups, *stack_tolerance);
                        notes.push(format!("stacked {} similar group(s)", auto_groups.len()));
                    }
                    if r.obstacles > 0 && !r.ignored_obstacles {
                        notes.push(format!("packed around {} other island(s)", r.obstacles));
                    }
                    if r.ignored_obstacles {
                        notes.push(
                            "no room around the other islands: packed over the whole tile".into(),
                        );
                    }
                    if options.multi_tile() {
                        let cols = options.tiles_u.unwrap_or(pack::UDIM_COLUMNS) as usize;
                        let last = pack::udim(r.tiles_used.saturating_sub(1), cols);
                        notes.push(format!("used {} tile(s), 1001–{last}", r.tiles_used));
                    }
                    let keep =
                        !options.rescale || options.multi_tile() || options.texel_density.is_some();
                    if keep && r.scale < 1.0 - 1e-9 {
                        notes.push(format!("shrunk to {:.0}% to fit", r.scale * 100.0));
                    }
                    Ok(if notes.is_empty() {
                        Outcome::Done
                    } else {
                        Outcome::Info(notes.join("; "))
                    })
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
                    let targets = resolve_pack_islands(doc, islands)?;
                    let Document {
                        mesh, uv, islands, ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    pack::set_texel_density(mesh, uv, isl, &targets, *density, *resolution);
                    Ok(Outcome::Done)
                });
            }
            Command::ApplyDensityMap {
                path,
                islands,
                min_scale,
                max_scale,
            } => {
                let bytes = std::fs::read(path).map_err(|source| CommandError::Io {
                    path: path.clone(),
                    source,
                })?;
                let map = DensityMap::parse_pgm(&bytes).map_err(CommandError::InvalidArgument)?;
                return self.edit(|doc| {
                    let targets = resolve_pack_islands(doc, islands)?;
                    if targets.is_empty() {
                        return Ok(Outcome::Info("all target islands are locked".into()));
                    }
                    let Document {
                        mesh, uv, islands, ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    let factors = map
                        .multipliers(mesh, uv, isl, &targets, *min_scale, *max_scale)
                        .map_err(CommandError::InvalidArgument)?;
                    pack::apply_island_multipliers(mesh, uv, isl, &factors)
                        .map_err(CommandError::InvalidArgument)?;
                    Ok(Outcome::Done)
                });
            }
            Command::BakeAmbientOcclusion {
                path,
                resolution,
                rays,
                max_distance,
            } => {
                if !path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("pgm"))
                {
                    return Err(CommandError::InvalidArgument(
                        "AO output must have a .pgm extension".into(),
                    ));
                }
                let doc = self.doc.as_ref().ok_or(CommandError::NoDocument)?;
                let map = ao::bake_density_map_with_progress(
                    doc.measurement_mesh(),
                    &doc.uv,
                    *resolution,
                    *rays,
                    *max_distance,
                    progress,
                )
                .map_err(|error| match error {
                    ao::AoError::Invalid(message) => CommandError::InvalidArgument(message),
                    ao::AoError::Cancelled(cancelled) => CommandError::Cancelled(cancelled),
                })?;
                uvcraft_io::commit_prepared(vec![(path.clone(), map.to_pgm())]).map_err(
                    |source| CommandError::File {
                        path: path.clone(),
                        source,
                    },
                )?;
                progress.finish("bake_ao", doc.mesh.face_count() as u64);
            }
            Command::SetRealScale {
                islands,
                mesh_unit,
                uv_unit,
            }
            | Command::SetExactRealScale {
                islands,
                mesh_unit,
                uv_unit,
            } => {
                let exact = matches!(cmd, Command::SetExactRealScale { .. });
                return self.edit(|doc| {
                    let targets = resolve_pack_islands(doc, islands)?;
                    if targets.is_empty() {
                        return Ok(Outcome::Info("all target islands are locked".into()));
                    }
                    let isl = doc.islands.as_ref().expect("computed above");
                    if targets.iter().any(|&i| {
                        isl.faces(i).iter().any(|&f| {
                            doc.mesh
                                .face_corners(f as usize)
                                .any(|c| doc.pins[doc.mesh.corner_vertex(c)])
                        })
                    }) {
                        return Err(CommandError::InvalidArgument(
                            "unpin target islands before setting real scale".into(),
                        ));
                    }
                    let measured = doc.measurement_mesh.as_ref().unwrap_or(&doc.mesh);
                    (if exact {
                        physical::project_planar_1to1
                    } else {
                        physical::scale_uv_1to1
                    })(
                        measured, &mut doc.uv, isl, &targets, *mesh_unit, *uv_unit
                    )
                    .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                    Ok(Outcome::Done)
                });
            }
            Command::MeasurePhysical {
                edges,
                islands,
                mesh_unit,
                output_unit,
            } => {
                let doc = self.doc_mut()?;
                let targets = resolve_islands(doc, islands)?;
                let mesh = doc.measurement_mesh();
                let edge_lengths = edges
                    .iter()
                    .map(|&edge| {
                        physical::edge_length(mesh, edge, *mesh_unit, *output_unit)
                            .map(|length| PhysicalEdge { edge, length })
                            .map_err(|e| CommandError::InvalidArgument(e.to_string()))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let isl = doc.islands.as_ref().expect("computed above");
                let island_areas = targets
                    .iter()
                    .map(|&island| {
                        physical::island_area(mesh, &doc.uv, isl, island, *mesh_unit, *output_unit)
                            .map(|area| PhysicalIsland { island, area })
                            .map_err(|e| CommandError::InvalidArgument(e.to_string()))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok(Outcome::PhysicalReport(Box::new(PhysicalReport {
                    output_unit: *output_unit,
                    edge_lengths,
                    island_areas,
                })));
            }
            Command::PackPhysical {
                islands,
                width,
                height,
                unit,
                uv_unit,
                padding,
                margin,
            } => {
                if !width.is_finite()
                    || !height.is_finite()
                    || !padding.is_finite()
                    || !margin.is_finite()
                    || *width <= 0.0
                    || *height <= 0.0
                    || *padding < 0.0
                    || *margin < 0.0
                {
                    return Err(CommandError::InvalidArgument("physical rectangle dimensions must be positive and padding/margin nonnegative".into()));
                }
                let factor = unit.to(*uv_unit);
                let opts = PackOptions {
                    region: Some([0.0, 0.0, *width * factor, *height * factor]),
                    padding_uv: Some(*padding * factor),
                    margin_uv: Some(*margin * factor),
                    rescale: false,
                    around_others: true,
                    ..PackOptions::default()
                };
                opts.validate().map_err(CommandError::InvalidArgument)?;
                return self.edit(|doc| {
                    let targets = resolve_pack_islands(doc, islands)?;
                    if targets.is_empty() {
                        return Ok(Outcome::Info("all target islands are locked".into()));
                    }
                    let Document {
                        mesh, uv, islands, ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    let packed = pack::pack_grouped_with_progress(
                        mesh,
                        uv,
                        isl,
                        &targets,
                        &opts,
                        &[],
                        progress,
                    )?;
                    if packed.failed || packed.ignored_obstacles || packed.scale < 1.0 - 1e-9 {
                        return Err(CommandError::InvalidArgument(
                            "islands do not fit the physical rectangle at 1:1 scale".into(),
                        ));
                    }
                    Ok(Outcome::Done)
                });
            }
            Command::EqualizeTexelDensity {
                islands,
                resolution,
            }
            | Command::AverageIslandScale {
                islands,
                resolution,
            } => {
                if *resolution == 0 {
                    return Err(CommandError::InvalidArgument(
                        "resolution must be > 0".into(),
                    ));
                }
                return self.edit(|doc| {
                    let targets = resolve_pack_islands(doc, islands)?;
                    let Document {
                        mesh, uv, islands, ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    if pack::equalize_texel_density(mesh, uv, isl, &targets, *resolution).is_some()
                    {
                        Ok(Outcome::Done)
                    } else {
                        Ok(Outcome::Info(
                            "no islands with usable UV and 3D area".into(),
                        ))
                    }
                });
            }
            Command::Pin { vertices, pinned } => {
                return self.edit(|doc| {
                    let count = doc.mesh.vertex_count();
                    if let Some(&bad) = vertices.iter().find(|&&v| v >= count) {
                        return Err(CommandError::InvalidArgument(format!(
                            "vertex {bad} does not exist (the mesh has {count} vertices)"
                        )));
                    }
                    for &v in vertices {
                        doc.pins[v] = *pinned;
                    }
                    Ok(Outcome::Done)
                });
            }
            Command::DragPin {
                vertex,
                uv,
                iterations,
            } => {
                if !uv.iter().all(|v| v.is_finite()) || *iterations == 0 || *iterations > 100 {
                    return Err(CommandError::InvalidArgument(
                        "pin UV must be finite and iterations must be in 1..=100".into(),
                    ));
                }
                return self.edit(|doc| {
                    if *vertex >= doc.mesh.vertex_count()
                        || !doc.pins.get(*vertex).copied().unwrap_or(false)
                    {
                        return Err(CommandError::InvalidArgument("drag a pinned vertex".into()));
                    }
                    if (0..doc.mesh.corner_count()).any(|c| {
                        doc.mesh.corner_vertex(c) == *vertex
                            && (doc.locked_faces[doc.mesh.corner_face(c)]
                                || doc.locked_island_faces[doc.mesh.corner_face(c)]
                                || doc.island_groups.face_locked(doc.mesh.corner_face(c)))
                    }) {
                        return Err(CommandError::InvalidArgument(
                            "pinned vertex touches a locked face or island".into(),
                        ));
                    }
                    let adjacent_faces: Vec<bool> = (0..doc.mesh.face_count())
                        .map(|face| {
                            doc.mesh
                                .face_corners(face)
                                .any(|c| doc.mesh.corner_vertex(c) == *vertex)
                        })
                        .collect();
                    let affected = {
                        let islands = doc.islands();
                        (0..islands.count())
                            .filter(|&island| {
                                islands
                                    .faces(island)
                                    .iter()
                                    .any(|&face| adjacent_faces[face as usize])
                            })
                            .collect::<Vec<_>>()
                    };
                    let targets = resolve_unlocked_islands(doc, &affected)?;
                    if targets.is_empty() {
                        return Err(CommandError::InvalidArgument(
                            "pin has no unlocked UV island".into(),
                        ));
                    }
                    let pins = doc.effective_pins();
                    let islands = doc.islands.as_ref().expect("resolved above");
                    uvcraft_algo::pin_drag::step(
                        &doc.mesh,
                        &mut doc.uv,
                        islands,
                        &targets,
                        uvcraft_algo::pin_drag::PinDragParams {
                            vertex: *vertex,
                            destination: DVec2::from(*uv),
                            iterations: *iterations,
                        },
                        &pins,
                    )
                    .map_err(CommandError::InvalidArgument)?;
                    Ok(Outcome::Done)
                });
            }
            Command::ClearPins => {
                return self.edit(|doc| {
                    doc.pins.iter_mut().for_each(|p| *p = false);
                    Ok(Outcome::Done)
                });
            }
            Command::LockIslands { islands, locked } => {
                return self.edit(|doc| {
                    if islands.is_empty() {
                        return Err(CommandError::InvalidArgument(
                            "lock_islands needs at least one island".into(),
                        ));
                    }
                    let targets = resolve_islands(doc, islands)?;
                    let isl = doc.islands.as_ref().expect("computed above");
                    for i in targets {
                        for &face in isl.faces(i) {
                            doc.locked_island_faces[face as usize] = *locked;
                        }
                    }
                    Ok(Outcome::Done)
                });
            }
            Command::LockFaces { faces, locked } => {
                return self.edit(|doc| {
                    if faces.is_empty() {
                        return Err(CommandError::InvalidArgument(
                            "lock_faces needs at least one face".into(),
                        ));
                    }
                    let count = doc.mesh.face_count();
                    if let Some(&bad) = faces.iter().find(|&&f| f >= count) {
                        return Err(CommandError::InvalidArgument(format!(
                            "face {bad} does not exist"
                        )));
                    }
                    for &f in faces {
                        doc.locked_faces[f] = *locked;
                    }
                    Ok(Outcome::Done)
                });
            }
            Command::ClearLocks => {
                return self.edit(|doc| {
                    doc.locked_island_faces.fill(false);
                    doc.locked_faces.fill(false);
                    Ok(Outcome::Done)
                });
            }
            Command::CreateIslandGroup {
                name,
                parent,
                children,
                islands,
            } => {
                return self.edit(|doc| {
                    let topology = doc.islands().clone();
                    let id = doc
                        .island_groups
                        .create(name, *parent, children, islands, &topology)
                        .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                    Ok(Outcome::Info(format!("created island group {id}")))
                });
            }
            Command::AddIslandsToGroup { group, islands } => {
                return self.edit(|doc| {
                    let topology = doc.islands().clone();
                    doc.island_groups
                        .assign_islands(*group, islands, &topology)
                        .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                    Ok(Outcome::Done)
                });
            }
            Command::RemoveIslandsFromGroup { group, islands } => {
                return self.edit(|doc| {
                    let topology = doc.islands().clone();
                    doc.island_groups
                        .remove_islands(*group, islands, &topology)
                        .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                    Ok(Outcome::Done)
                });
            }
            Command::UngroupIslands { group } | Command::DeleteIslandGroup { group } => {
                return self.edit(|doc| {
                    doc.island_groups
                        .ungroup(*group)
                        .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                    Ok(Outcome::Done)
                });
            }
            Command::TransformIslandGroup {
                group,
                translate,
                rotate_deg,
                scale,
            } => {
                if !translate.iter().all(|value| value.is_finite())
                    || !rotate_deg.is_finite()
                    || !scale.is_finite()
                    || *scale <= 0.0
                {
                    return Err(CommandError::InvalidArgument("group transform requires finite translation and rotation and a positive scale".into()));
                }
                return self.edit(|doc| {
                    let topology = doc.islands().clone();
                    let members = doc
                        .island_groups
                        .islands_in(*group, &topology)
                        .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                    transform_group(
                        doc,
                        &members,
                        DMat2::from_angle(rotate_deg.to_radians()) * *scale,
                        DVec2::from(*translate),
                    )?;
                    Ok(Outcome::Done)
                });
            }
            Command::SetIslandGroupTexelDensity {
                group,
                density,
                resolution,
            } => {
                if *resolution == 0 || !density.is_finite() || *density <= 0.0 {
                    return Err(CommandError::InvalidArgument(
                        "group texel density requires a positive density and resolution".into(),
                    ));
                }
                return self.edit(|doc| {
                    let topology = doc.islands().clone();
                    let members = doc
                        .island_groups
                        .islands_in(*group, &topology)
                        .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                    ensure_group_editable(doc, &members)?;
                    let Document {
                        mesh, uv, islands, ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    match pack::set_group_texel_density(
                        mesh,
                        uv,
                        isl,
                        &members,
                        *density,
                        *resolution,
                    ) {
                        Some(_) => Ok(Outcome::Done),
                        None => Ok(Outcome::Info("group has no usable UV and 3D area".into())),
                    }
                });
            }
            Command::AverageIslandGroupScale { group, resolution } => {
                if *resolution == 0 {
                    return Err(CommandError::InvalidArgument(
                        "resolution must be > 0".into(),
                    ));
                }
                return self.edit(|doc| {
                    let topology = doc.islands().clone();
                    let members = doc
                        .island_groups
                        .islands_in(*group, &topology)
                        .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                    ensure_group_editable(doc, &members)?;
                    let others: Vec<usize> = (0..topology.count())
                        .filter(|island| !members.contains(island))
                        .collect();
                    let Document {
                        mesh, uv, islands, ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    let Some(target) =
                        pack::group_texel_density(mesh, uv, isl, &others, *resolution)
                    else {
                        return Ok(Outcome::Info(
                            "no other usable islands to average against".into(),
                        ));
                    };
                    match pack::set_group_texel_density(
                        mesh,
                        uv,
                        isl,
                        &members,
                        target,
                        *resolution,
                    ) {
                        Some(_) => Ok(Outcome::Done),
                        None => Ok(Outcome::Info("group has no usable UV and 3D area".into())),
                    }
                });
            }
            Command::SetIslandGroup {
                group,
                name,
                color,
                locked,
                visible,
                selected,
            } => {
                return self.edit(|doc| {
                    doc.island_groups
                        .set_properties(
                            *group,
                            name.as_deref(),
                            *color,
                            *locked,
                            *visible,
                            *selected,
                        )
                        .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                    Ok(Outcome::Done)
                });
            }
            Command::MoveIslandGroup { group, parent } => {
                return self.edit(|doc| {
                    doc.island_groups
                        .set_parent(*group, *parent)
                        .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                    Ok(Outcome::Done)
                });
            }
            Command::SelectIslandGroup { group } => {
                let doc = self.doc.as_mut().ok_or(CommandError::NoDocument)?;
                let topology = doc.islands().clone();
                let selected = doc
                    .island_groups
                    .islands_in(*group, &topology)
                    .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                return Ok(Outcome::Selection { islands: selected });
            }
            Command::OrganizeIslandGroups {
                by,
                parent,
                islands,
            } => {
                return self.edit(|doc| {
                    if let Some(parent) = parent
                        && doc.island_groups.get(*parent).is_none()
                    {
                        return Err(CommandError::InvalidArgument(format!(
                            "group {parent} does not exist"
                        )));
                    }
                    let face_labels: Vec<String> = match (&doc.source, by) {
                        (_, GroupBy::Object) => doc
                            .face_objects
                            .iter()
                            .map(|&id| {
                                format!(
                                    "Object: {}",
                                    doc.objects
                                        .get(id as usize)
                                        .map(String::as_str)
                                        .unwrap_or("Unnamed")
                                )
                            })
                            .collect(),
                        (Source::Obj(source), GroupBy::Material) => source
                            .face_material()
                            .iter()
                            .map(|&id| {
                                format!(
                                    "Material: {}",
                                    if id == u32::MAX {
                                        "Unassigned"
                                    } else {
                                        source
                                            .material_names()
                                            .get(id as usize)
                                            .map(String::as_str)
                                            .unwrap_or("Unassigned")
                                    }
                                )
                            })
                            .collect(),
                        (Source::Gltf(source), GroupBy::Material) => {
                            let names = source.material_names();
                            source
                                .face_material(&doc.mesh)
                                .into_iter()
                                .map(|id| {
                                    format!(
                                        "Material: {}",
                                        id.and_then(|id| names.get(id as usize))
                                            .map(String::as_str)
                                            .unwrap_or("Unassigned")
                                    )
                                })
                                .collect()
                        }
                        (Source::Obj(source), GroupBy::SmoothingGroup) => doc
                            .smoothing_override
                            .as_deref()
                            .unwrap_or(source.face_smoothing())
                            .iter()
                            .map(|id| {
                                id.map_or_else(
                                    || "Smoothing: Off".to_owned(),
                                    |id| format!("Smoothing: {id}"),
                                )
                            })
                            .collect(),
                        (Source::Gltf(_), GroupBy::SmoothingGroup) => {
                            return Err(CommandError::InvalidArgument(
                                "smoothing groups require an OBJ source".into(),
                            ));
                        }
                    };
                    let targets = resolve_islands(doc, islands)?;
                    let topology = doc.islands().clone();
                    let mut buckets: BTreeMap<String, Vec<usize>> = BTreeMap::new();
                    for island in targets {
                        let faces = topology.faces(island);
                        let first = &face_labels[faces[0] as usize];
                        let label = if faces
                            .iter()
                            .all(|&face| &face_labels[face as usize] == first)
                        {
                            first.clone()
                        } else {
                            match by {
                                GroupBy::Material => "Material: Mixed",
                                GroupBy::Object => "Object: Mixed",
                                GroupBy::SmoothingGroup => "Smoothing: Mixed",
                            }
                            .to_owned()
                        };
                        buckets.entry(label).or_default().push(island);
                    }
                    let count = buckets.len();
                    for (name, targets) in buckets {
                        let existing = doc
                            .island_groups
                            .nodes()
                            .find(|group| group.parent == *parent && group.name == name)
                            .map(|group| group.id);
                        if let Some(id) = existing {
                            doc.island_groups.assign_islands(id, &targets, &topology)
                        } else {
                            doc.island_groups
                                .create(&name, *parent, &[], &targets, &topology)
                                .map(|_| ())
                        }
                        .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                    }
                    Ok(Outcome::Info(format!(
                        "organized islands into {count} group(s)"
                    )))
                });
            }
            Command::Mosaic {
                islands,
                charts,
                max_error,
                max_seam_length,
                max_uv_distortion,
                iterations,
                unfold,
            } => {
                let params = MosaicParams {
                    charts: *charts,
                    max_error: max_error.unwrap_or(mosaic::DEFAULT_MAX_ERROR),
                    iterations: *iterations,
                };
                if charts == &Some(0)
                    || !params.max_error.is_finite()
                    || params.max_error < 0.0
                    || max_seam_length.is_some_and(|v| !v.is_finite() || v < 0.0)
                    || max_uv_distortion.is_some_and(|v| !v.is_finite() || v < 1.0)
                    || (max_uv_distortion.is_some() && !unfold)
                    || *iterations == 0
                    || *iterations > 1000
                {
                    return Err(CommandError::InvalidArgument(
                        "charts must be > 0, max_error and max_seam_length ≥ 0, max_uv_distortion ≥ 1 requires unfold, iterations in 1..=1000".into(),
                    ));
                }
                return self.edit(|doc| {
                    let targets = resolve_islands(doc, islands)?;
                    let Document {
                        mesh,
                        uv,
                        islands,
                        pins,
                        ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    let (m, budget_note) = if let Some(limit) = max_seam_length {
                        let result = mosaic::mosaic_with_seam_budget_with_progress(mesh, uv, isl, &targets, &params, *limit, progress)?
                            .map_err(|needed| CommandError::InvalidArgument(format!("max_seam_length {limit} is too small; shortest attempt needs {needed:.6}")))?;
                        (result.mosaic, format!("; added {:.6} seam length (limit {limit}); max_error {:.4}", result.added_seam_length, result.max_error_used))
                    } else {
                        (mosaic::mosaic_with_progress(mesh, uv, isl, &targets, &params, progress)?, String::new())
                    };
                    let mut note = format!("{} chart(s){budget_note}", m.charts);
                    if *unfold {
                        let after = Islands::compute(mesh, uv);
                        let r = unfold::unfold_with_progress(mesh, uv, &after, &m.islands, pins, uvcraft_algo::unfold::UnfoldMethod::Lscm, progress)?;
                        let bad = [r.failed, r.skipped_closed].concat();
                        if !bad.is_empty() {
                            if max_uv_distortion.is_some() {
                                return Err(CommandError::InvalidArgument(format!("could not unfold island(s) {bad:?} under max_uv_distortion")));
                            }
                            note += &format!("; could not unfold island(s) {bad:?}");
                        }
                        if let Some(limit) = max_uv_distortion {
                            let measured = mosaic::max_chart_uv_distortion(mesh, uv, &after, &m.islands);
                            if !measured.is_finite() || measured > *limit + 1e-9 {
                                return Err(CommandError::InvalidArgument(format!("max_uv_distortion {limit} exceeded after unfold: measured {measured:.6}; increase charts or tolerance")));
                            }
                            note += &format!("; max UV distortion {measured:.6} (limit {limit})");
                        }
                    }
                    Ok(Outcome::Info(note))
                });
            }
            Command::Undo => {
                let prev = self.undo.pop().ok_or(CommandError::NothingToUndo)?;
                let doc = self.doc_mut()?;
                let current = doc.snapshot();
                doc.restore(prev);
                self.redo.push(current);
            }
            Command::Redo => {
                let next = self.redo.pop().ok_or(CommandError::NothingToRedo)?;
                let doc = self.doc_mut()?;
                let current = doc.snapshot();
                doc.restore(next);
                self.undo.push(current);
            }
            Command::Analyze {
                resolution,
                output,
                padding_px,
            } => {
                let doc = self.doc_mut()?;
                doc.islands();
                let islands = doc.islands.as_ref().expect("computed above");
                let mut report = analysis::analyze_with_padding_and_stacks(
                    doc.measurement_mesh(),
                    &doc.uv,
                    islands,
                    *resolution,
                    *padding_px,
                    &doc.stack_groups(),
                );
                report.mesh_problems = Some(doc.mesh_problems.clone());
                report.measurement_local_meshes = doc.measurement_local_meshes.clone();
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
            Command::SelectIslands { problem } => {
                let doc = self.doc_mut()?;
                doc.islands();
                let islands = doc.islands.as_ref().expect("computed above");
                let report = analysis::analyze_with_padding_and_stacks(
                    doc.measurement_mesh(),
                    &doc.uv,
                    islands,
                    default_resolution(),
                    0.0,
                    &doc.stack_groups(),
                );
                return Ok(Outcome::Selection {
                    islands: select::islands_with_problem(&report, *problem),
                });
            }
            Command::SelectIslandsByMetric {
                metric,
                min,
                max,
                resolution,
            } => {
                if min.is_none() && max.is_none() {
                    return Err(CommandError::InvalidArgument(
                        "select_islands_by_metric needs min or max".into(),
                    ));
                }
                if min.is_some_and(|value| !value.is_finite())
                    || max.is_some_and(|value| !value.is_finite())
                    || min.zip(*max).is_some_and(|(lo, hi)| lo > hi)
                    || *resolution == 0
                {
                    return Err(CommandError::InvalidArgument(
                        "invalid metric bounds or resolution".into(),
                    ));
                }
                let doc = self.doc_mut()?;
                doc.islands();
                let islands = doc.islands.as_ref().expect("computed above");
                let report =
                    analysis::analyze(doc.measurement_mesh(), &doc.uv, islands, *resolution);
                return Ok(Outcome::Selection {
                    islands: select::islands_by_metric(&report, *metric, *min, *max),
                });
            }
            Command::SelectFacesBySmoothingGroup { group } => {
                let doc = self.doc.as_ref().ok_or(CommandError::NoDocument)?;
                let Source::Obj(source) = &doc.source else {
                    return Err(CommandError::InvalidArgument(
                        "smoothing groups require an OBJ source".into(),
                    ));
                };
                let groups = doc
                    .smoothing_override
                    .as_deref()
                    .unwrap_or(source.face_smoothing());
                return Ok(Outcome::FaceSelection {
                    faces: groups
                        .iter()
                        .enumerate()
                        .filter_map(|(face, value)| (value == group).then_some(face))
                        .collect(),
                });
            }
            Command::SelectFacesByMaterial { material } => {
                let doc = self.doc.as_ref().ok_or(CommandError::NoDocument)?;
                let (names, assignments): (Vec<String>, Vec<Option<u32>>) = match &doc.source {
                    Source::Obj(source) => (
                        source.material_names().to_vec(),
                        source
                            .face_material()
                            .iter()
                            .map(|&id| (id != u32::MAX).then_some(id))
                            .collect(),
                    ),
                    Source::Gltf(source) => {
                        (source.material_names(), source.face_material(&doc.mesh))
                    }
                };
                return Ok(Outcome::FaceSelection {
                    faces: assignments
                        .iter()
                        .enumerate()
                        .filter_map(|(face, &id)| {
                            let name = id.and_then(|id| names.get(id as usize));
                            match material {
                                Some(wanted) if name.is_some_and(|name| name == wanted) => {
                                    Some(face)
                                }
                                None if id.is_none() => Some(face),
                                _ => None,
                            }
                        })
                        .collect(),
                });
            }
            Command::SelectFacesByAngle {
                seed_face,
                max_angle_deg,
            } => {
                let doc = self.doc.as_ref().ok_or(CommandError::NoDocument)?;
                if *seed_face >= doc.mesh.face_count() {
                    return Err(CommandError::InvalidArgument(format!(
                        "face {seed_face} does not exist (the mesh has {} faces)",
                        doc.mesh.face_count()
                    )));
                }
                if !max_angle_deg.is_finite() || !(0.0..=180.0).contains(max_angle_deg) {
                    return Err(CommandError::InvalidArgument(
                        "max_angle_deg must be finite and between 0 and 180".into(),
                    ));
                }
                return Ok(Outcome::FaceSelection {
                    faces: select::faces_by_angle_from_seed(
                        doc.measurement_mesh(),
                        *seed_face,
                        *max_angle_deg,
                    ),
                });
            }
            Command::LoadUvs { path } => {
                progress.report("load_uvs", 0, 2)?;
                let parsed = uvcraft_io::read(path).map_err(|source| CommandError::File {
                    path: path.clone(),
                    source,
                })?;
                return self.edit(|doc| {
                    progress.report("load_uvs", 1, 2)?;
                    let uv =
                        load_uvs::transfer_uvs(&doc.mesh, &doc.uv.name, &parsed.mesh, &parsed.uv)
                            .map_err(|e| {
                            CommandError::InvalidArgument(format!("{}: {e}", path.display()))
                        })?;
                    doc.set_uv(uv);
                    progress.report("load_uvs", 2, 2)?;
                    Ok(match parsed.missing_uv_corners {
                        0 => Outcome::Done,
                        n => Outcome::Info(format!(
                            "{n} corners had no UVs in {} (placeholders at 0,0)",
                            path.display()
                        )),
                    })
                });
            }
            Command::Mirror { islands, axis, at } => {
                check_finite(at, "at")?;
                return self.edit(|doc| {
                    with_islands(doc, islands, |mesh, uv, isl, targets| {
                        layout::mirror(mesh, uv, isl, targets, *axis, *at);
                        Ok(Outcome::Done)
                    })
                });
            }
            Command::Orient {
                islands,
                mode,
                vertical,
            } => {
                let vertical = vertical.unwrap_or(mode.default_vertical());
                return self.edit(|doc| {
                    if let OrientMode::Edge(e) = mode {
                        check_edges(&doc.mesh, &[*e])?;
                    }
                    with_islands(doc, islands, |mesh, uv, isl, targets| {
                        let n = layout::orient(mesh, uv, isl, targets, *mode, vertical);
                        match mode {
                            OrientMode::Edge(e) if n == 0 => Err(CommandError::InvalidArgument(
                                format!("edge {e} is in none of the target islands"),
                            )),
                            _ if n < targets.len() => Ok(Outcome::Info(format!(
                                "{} island(s) had no usable direction",
                                targets.len() - n
                            ))),
                            _ => Ok(Outcome::Done),
                        }
                    })
                });
            }
            Command::Align {
                islands,
                axis,
                to,
                value,
            } => {
                check_finite(value, "value")?;
                return self.edit(|doc| {
                    with_islands(doc, islands, |mesh, uv, isl, targets| {
                        layout::align(mesh, uv, isl, targets, *axis, *to, *value);
                        Ok(Outcome::Done)
                    })
                });
            }
            Command::Distribute {
                islands,
                axis,
                by,
                gap,
            } => {
                check_finite(gap, "gap")?;
                return self.edit(|doc| {
                    with_islands(doc, islands, |mesh, uv, isl, targets| {
                        layout::distribute(mesh, uv, isl, targets, *axis, *by, *gap);
                        Ok(Outcome::Done)
                    })
                });
            }
            Command::Snap {
                islands,
                to,
                resolution,
                step,
                tolerance_px,
            } => {
                if *resolution == 0
                    || !tolerance_px.is_finite()
                    || *tolerance_px < 0.0
                    || step.is_some_and(|s| !s.is_finite() || s <= 0.0)
                {
                    return Err(CommandError::InvalidArgument(
                        "resolution and step must be > 0, tolerance_px >= 0".into(),
                    ));
                }
                let step = step.unwrap_or(1.0 / *resolution as f64);
                let tolerance = tolerance_px / *resolution as f64;
                return self.edit(|doc| {
                    with_islands(doc, islands, |mesh, uv, isl, targets| {
                        layout::snap(mesh, uv, isl, targets, *to, step, tolerance);
                        Ok(Outcome::Done)
                    })
                });
            }
            Command::SaveMacro { path } => {
                self.doc_mut()?;
                std::fs::write(path, self.macro_jsonl()).map_err(|source| CommandError::Io {
                    path: path.clone(),
                    source,
                })?;
            }
            Command::Similar {
                islands,
                mode,
                tolerance,
            } => {
                check_tolerance(*tolerance)?;
                progress.report("similar", 0, 1)?;
                let doc = self.doc_mut()?;
                let targets = resolve_islands(doc, islands)?;
                let isl = doc.islands.as_ref().expect("computed by resolve_islands");
                let groups =
                    stack::similar_groups(&doc.mesh, &doc.uv, isl, &targets, *mode, *tolerance);
                progress.report("similar", 1, 1)?;
                return Ok(Outcome::Info(format!("similar groups: {groups:?}")));
            }
            Command::Stack {
                islands,
                mode,
                tolerance,
                reference,
                preview,
                conform,
                max_distortion,
            } => {
                check_tolerance(*tolerance)?;
                if !max_distortion.is_finite() || *max_distortion < 1.0 {
                    return Err(CommandError::InvalidArgument("max_distortion must be finite and at least 1".into()));
                }
                if *preview {
                    let doc = self.doc_mut()?;
                    let mut targets = resolve_pack_islands(doc, islands)?;
                    if let Some(reference) = reference {
                        let Some(pos) = targets.iter().position(|&i| i == *reference) else {
                            return Err(CommandError::InvalidArgument("reference island is outside the selection".into()));
                        };
                        targets.swap(0, pos);
                    }
                    let isl = doc.islands.as_ref().expect("computed by resolve_pack_islands");
                    let r = if *conform {
                        let mut copy = doc.uv.clone();
                        if reference.is_some() {
                            stack::stack_conform_to_reference(&doc.mesh, &mut copy, isl,
                                &targets, *mode, *tolerance, *max_distortion)
                        } else {
                            stack::stack_conform(&doc.mesh, &mut copy, isl,
                                &targets, *mode, *tolerance, *max_distortion)
                        }
                    } else if reference.is_some() {
                        stack::preview_to_reference(&doc.mesh, &doc.uv, isl, &targets, *mode, *tolerance)
                    } else {
                        stack::preview(&doc.mesh, &doc.uv, isl, &targets, *mode, *tolerance)
                    };
                    return Ok(Outcome::Info(format!("stack preview: {:?}; matches: {:?}; failed: {:?}",
                        r.groups, r.matches, r.failed)));
                }
                return self.edit(|doc| {
                    progress.report("stack", 0, 1)?;
                    let mut targets = resolve_pack_islands(doc, islands)?;
                    if let Some(reference) = reference {
                        let Some(pos) = targets.iter().position(|&i| i == *reference) else {
                            return Err(CommandError::InvalidArgument("reference island is outside the selection".into()));
                        };
                        targets.swap(0, pos);
                    }
                    let Document {
                        mesh,
                        uv,
                        islands,
                        stack_relations,
                        ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    let r = if *conform && reference.is_some() {
                        stack::stack_conform_to_reference(mesh, uv, isl, &targets, *mode,
                            *tolerance, *max_distortion)
                    } else if *conform {
                        stack::stack_conform(mesh, uv, isl, &targets, *mode,
                            *tolerance, *max_distortion)
                    } else if reference.is_some() {
                        stack::stack_to_reference(mesh, uv, isl, &targets, *mode, *tolerance)
                    } else {
                        stack::stack(mesh, uv, isl, &targets, *mode, *tolerance)
                    };
                    stack_relations.record(isl, &r.groups, *tolerance);
                    progress.report("stack", 1, 1)?;
                    Ok(Outcome::Info(format!(
                        "stacked {} group(s) {:?}: {} exact, {} aligned; matches: {:?}; failed: {:?}",
                        r.groups.len(),
                        r.groups,
                        r.exact.len(),
                        r.aligned.len(),
                        r.matches,
                        r.failed
                    )))
                });
            }
            Command::Unstack { islands } => {
                return self.edit(|doc| {
                    let targets = resolve_pack_islands(doc, islands)?;
                    let Document {
                        mesh,
                        uv,
                        islands,
                        stack_relations,
                        ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    stack_relations.clear_for(isl, &targets);
                    let moved = stack::unstack(mesh, uv, isl, &targets);
                    Ok(Outcome::Info(format!("moved {moved} stacked island(s)")))
                });
            }
            Command::Match { source, targets } => {
                if targets.is_empty() {
                    return Err(CommandError::InvalidArgument("no target islands".into()));
                }
                return self.edit(|doc| {
                    progress.report("match", 0, 1)?;
                    let src = resolve_islands(doc, std::slice::from_ref(source))?[0];
                    let targets = resolve_islands(doc, targets)?;
                    let Document {
                        mesh, uv, islands, ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    let r = stack::match_islands(mesh, uv, isl, src, &targets);
                    progress.report("match", 1, 1)?;
                    let mut msg = format!(
                        "{} copied exactly, {} aligned",
                        r.exact.len(),
                        r.aligned.len()
                    );
                    if !r.failed.is_empty() {
                        msg += &format!("; could not match island(s) {:?}", r.failed);
                    }
                    Ok(Outcome::Info(msg))
                });
            }
            Command::CutUnfold { edges } => {
                return self.edit(|doc| {
                    check_edges(&doc.mesh, edges)?;
                    cut_and_unfold(doc, edges, true, progress)
                });
            }
            Command::CutLoop { edge, unfold } => {
                return self.edit(|doc| {
                    check_edges(&doc.mesh, &[*edge])?;
                    let edges = select::edge_loop(&doc.mesh, *edge);
                    cut_and_unfold(doc, &edges, *unfold, progress)
                });
            }
            Command::CutPath { vertices, unfold } => {
                return self.edit(|doc| {
                    let count = doc.mesh.vertex_count();
                    if vertices.len() < 2 {
                        return Err(CommandError::InvalidArgument(
                            "cut_path needs at least two vertices".into(),
                        ));
                    }
                    if let Some(&bad) = vertices.iter().find(|&&v| v >= count) {
                        return Err(CommandError::InvalidArgument(format!(
                            "vertex {bad} does not exist (the mesh has {count} vertices)"
                        )));
                    }
                    let edges = select::path_through(&doc.mesh, vertices).ok_or_else(|| {
                        CommandError::InvalidArgument("the vertices are not connected".into())
                    })?;
                    cut_and_unfold(doc, &edges, *unfold, progress)
                });
            }
            Command::CutBorder { faces, unfold } => {
                return self.edit(|doc| {
                    let count = doc.mesh.face_count();
                    if let Some(&bad) = faces.iter().find(|&&f| f >= count) {
                        return Err(CommandError::InvalidArgument(format!(
                            "face {bad} does not exist (the mesh has {count} faces)"
                        )));
                    }
                    let border = select::selection_border(&doc.mesh, &select::mask(count, faces));
                    cut_and_unfold(doc, &select::indices(&border), *unfold, progress)
                });
            }
            Command::StraightenPath {
                island,
                vertices,
                edges,
            } => {
                if vertices.is_empty() == edges.is_empty() {
                    return Err(CommandError::InvalidArgument(
                        "provide exactly one of vertices or edges".into(),
                    ));
                }
                return self.edit(|doc| {
                    let Document {
                        mesh,
                        uv,
                        islands,
                        pins,
                        ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    let path = if vertices.is_empty() {
                        StraightenPath::Edges(edges)
                    } else {
                        StraightenPath::Vertices(vertices)
                    };
                    layout::straighten_path(mesh, uv, isl, *island, path, pins)
                        .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                    Ok(Outcome::Done)
                });
            }
            Command::AlignBorder {
                island,
                axis,
                vertices,
                edges,
            } => {
                if vertices.is_empty() == edges.is_empty() {
                    return Err(CommandError::InvalidArgument(
                        "provide exactly one of vertices or edges".into(),
                    ));
                }
                return self.edit(|doc| {
                    let effective_pins = doc.effective_pins();
                    let Document {
                        mesh,
                        uv,
                        islands,
                        locked_island_faces,
                        ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    if *island >= isl.count() {
                        return Err(CommandError::InvalidIsland {
                            index: *island,
                            count: isl.count(),
                        });
                    }
                    if isl
                        .faces(*island)
                        .iter()
                        .any(|&f| locked_island_faces[f as usize])
                    {
                        return Ok(Outcome::Info("island is locked".into()));
                    }
                    let path = if vertices.is_empty() {
                        StraightenPath::Edges(edges)
                    } else {
                        StraightenPath::Vertices(vertices)
                    };
                    layout::align_border_path(mesh, uv, isl, *island, path, *axis, &effective_pins)
                        .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                    Ok(Outcome::Done)
                });
            }
            Command::Rectangularize { island } | Command::StraightenIsland { island } => {
                return self.edit(|doc| {
                    let effective_pins = doc.effective_pins();
                    let Document {
                        mesh,
                        uv,
                        islands,
                        locked_island_faces,
                        ..
                    } = doc;
                    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
                    if *island >= isl.count() {
                        return Err(CommandError::InvalidIsland {
                            index: *island,
                            count: isl.count(),
                        });
                    }
                    if isl
                        .faces(*island)
                        .iter()
                        .any(|&face| locked_island_faces[face as usize])
                    {
                        return Ok(Outcome::Info("island is locked".into()));
                    }
                    let operation = if matches!(cmd, Command::StraightenIsland { .. }) {
                        grid::straighten_island
                    } else {
                        grid::rectangularize
                    };
                    operation(mesh, uv, isl, *island, &effective_pins)
                        .map_err(|e| CommandError::InvalidArgument(e.to_string()))?;
                    Ok(Outcome::Done)
                });
            }
        }
        Ok(Outcome::Done)
    }
}

fn check_finite(value: &Option<f64>, name: &str) -> Result<(), CommandError> {
    match value {
        Some(v) if !v.is_finite() => Err(CommandError::InvalidArgument(format!(
            "{name} must be a finite number"
        ))),
        _ => Ok(()),
    }
}

/// Resolves the target islands, then runs `f` on the mesh, UVs and islands.
fn with_islands(
    doc: &mut Document,
    ids: &[usize],
    f: impl FnOnce(&Mesh, &mut UvLayer, &Islands, &[usize]) -> Result<Outcome, CommandError>,
) -> Result<Outcome, CommandError> {
    let targets = resolve_islands(doc, ids)?;
    let Document {
        mesh, uv, islands, ..
    } = doc;
    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
    f(mesh, uv, isl, &targets)
}

fn check_tolerance(tolerance: f64) -> Result<(), CommandError> {
    if tolerance.is_finite() && tolerance >= 0.0 {
        Ok(())
    } else {
        Err(CommandError::InvalidArgument(
            "tolerance must be ≥ 0".into(),
        ))
    }
}

fn check_edges(mesh: &Mesh, edges: &[usize]) -> Result<(), CommandError> {
    let count = mesh.edge_count();
    match edges.iter().find(|&&e| e >= count) {
        Some(&index) => Err(CommandError::InvalidEdge { index, count }),
        None => Ok(()),
    }
}

/// Cuts `edges`; with `unfold`, then unfolds (LSCM) the islands they touch.
fn cut_and_unfold(
    doc: &mut Document,
    edges: &[usize],
    unfold: bool,
    progress: &Progress,
) -> Result<Outcome, CommandError> {
    let total = if unfold { 2 } else { 1 };
    progress.report("cut_unfold", 0, total)?;
    let Document {
        mesh,
        uv,
        islands,
        pins,
        ..
    } = doc;
    seams::cut_edges(mesh, uv, edges);
    *islands = None;
    if !unfold {
        progress.report("cut_unfold", 1, total)?;
        return Ok(Outcome::Done);
    }
    progress.report("cut_unfold", 1, total)?;
    let isl = Islands::compute(mesh, uv);
    isl.split_shared_uvs(mesh, uv);
    let mut targets: Vec<usize> = edges
        .iter()
        .flat_map(|&e| {
            let (c0, c1) = mesh.edge_corners(e);
            [Some(c0), c1]
        })
        .flatten()
        .map(|c| isl.island_of_face(mesh.corner_face(c)))
        .collect();
    targets.sort_unstable();
    targets.dedup();
    let r = unfold::unfold_with_progress(
        mesh,
        uv,
        &isl,
        &targets,
        pins,
        uvcraft_algo::unfold::UnfoldMethod::Lscm,
        progress,
    )?;
    *islands = Some(isl);
    progress.report("cut_unfold", 2, total)?;
    let mut notes = Vec::new();
    if !r.skipped_closed.is_empty() {
        notes.push(format!(
            "skipped {} closed island(s) {:?}: the cut does not open them",
            r.skipped_closed.len(),
            r.skipped_closed
        ));
    }
    if !r.failed.is_empty() {
        notes.push(format!("could not unfold island(s) {:?}", r.failed));
    }
    Ok(if notes.is_empty() {
        Outcome::Done
    } else {
        Outcome::Info(notes.join("; "))
    })
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

fn tile_distribution_buckets(
    doc: &Document,
    topology: &Islands,
    targets: &[usize],
    by: TileDistribution,
) -> Vec<Vec<usize>> {
    let labels: Vec<String> = match by {
        TileDistribution::Group => (0..doc.mesh.face_count())
            .map(|face| {
                doc.island_groups.root_of_face(face).map_or_else(
                    || "Ungrouped".to_owned(),
                    |group| format!("Group {}: {}", group.id, group.name),
                )
            })
            .collect(),
        TileDistribution::Object => doc
            .face_objects
            .iter()
            .map(|&id| format!("Object {id}"))
            .collect(),
        TileDistribution::Material => match &doc.source {
            Source::Obj(source) => source
                .face_material()
                .iter()
                .map(|&id| format!("Material {id}"))
                .collect(),
            Source::Gltf(source) => source
                .face_material(&doc.mesh)
                .iter()
                .map(|id| {
                    id.map_or_else(
                        || "Material unassigned".to_owned(),
                        |id| format!("Material {id}"),
                    )
                })
                .collect(),
        },
    };
    let mut buckets: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for &island in targets {
        let faces = topology.faces(island);
        let first = &labels[faces[0] as usize];
        let label = if faces.iter().all(|&face| &labels[face as usize] == first) {
            first.clone()
        } else {
            "Mixed".into()
        };
        buckets.entry(label).or_default().push(island);
    }
    buckets.into_values().collect()
}

/// Omits whole-island locks, and islands whose every face is polygon-locked.
fn resolve_unlocked_islands(doc: &mut Document, ids: &[usize]) -> Result<Vec<usize>, CommandError> {
    let targets = resolve_islands(doc, ids)?;
    let isl = doc.islands.as_ref().expect("computed above");
    Ok(targets
        .into_iter()
        .filter(|&i| {
            let faces = isl.faces(i);
            !faces.iter().any(|&f| doc.locked_island_faces[f as usize])
                && !faces.iter().all(|&f| {
                    doc.locked_faces[f as usize] || doc.island_groups.face_locked(f as usize)
                })
        })
        .collect())
}

fn resolve_pack_islands(doc: &mut Document, ids: &[usize]) -> Result<Vec<usize>, CommandError> {
    let targets = resolve_islands(doc, ids)?;
    let isl = doc.islands.as_ref().expect("computed above");
    Ok(targets
        .into_iter()
        .filter(|&i| {
            !isl.faces(i).iter().any(|&f| {
                doc.locked_island_faces[f as usize]
                    || doc.locked_faces[f as usize]
                    || doc.island_groups.face_locked(f as usize)
            })
        })
        .collect())
}

fn ensure_group_editable(doc: &mut Document, members: &[usize]) -> Result<(), CommandError> {
    if members.is_empty() {
        return Err(CommandError::InvalidArgument("group has no islands".into()));
    }
    if resolve_pack_islands(doc, members)?.len() != members.len() {
        return Err(CommandError::InvalidArgument(
            "group contains a locked island or face".into(),
        ));
    }
    Ok(())
}

fn transform_group(
    doc: &mut Document,
    members: &[usize],
    m: DMat2,
    t: DVec2,
) -> Result<(), CommandError> {
    ensure_group_editable(doc, members)?;
    let Document {
        mesh, uv, islands, ..
    } = doc;
    let isl = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
    let mut ids: Vec<usize> = members
        .iter()
        .flat_map(|&island| {
            isl.uv_indices(island, mesh, uv)
                .into_iter()
                .map(|id| id as usize)
        })
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let (lo, hi) = ids.iter().fold(
        (DVec2::splat(f64::INFINITY), DVec2::splat(f64::NEG_INFINITY)),
        |(lo, hi), &id| (lo.min(uv.uvs[id]), hi.max(uv.uvs[id])),
    );
    let pivot = (lo + hi) * 0.5;
    let updated: Vec<_> = ids
        .iter()
        .map(|&id| pivot + m * (uv.uvs[id] - pivot) + t)
        .collect();
    if updated.iter().any(|p| !p.is_finite()) {
        return Err(CommandError::InvalidArgument(
            "group transform exceeds finite UV range".into(),
        ));
    }
    for (id, value) in ids.into_iter().zip(updated) {
        uv.uvs[id] = value;
    }
    Ok(())
}

impl Document {
    /// Pins every mesh vertex of locked polygons in addition to user pins.
    fn effective_pins(&self) -> Vec<bool> {
        let mut pins = self.pins.clone();
        for (face, &locked) in self.locked_faces.iter().enumerate() {
            if locked || self.island_groups.face_locked(face) {
                for c in self.mesh.face_corners(face) {
                    pins[self.mesh.corner_vertex(c)] = true;
                }
            }
        }
        pins
    }
}

fn transform(
    doc: &mut Document,
    targets: &[usize],
    m: DMat2,
    t: DVec2,
    pivot: Pivot,
) -> Result<(), CommandError> {
    let editable = resolve_pack_islands(doc, targets)?;
    if editable.is_empty() {
        return Ok(());
    }
    let Document {
        mesh, uv, islands, ..
    } = doc;
    let islands = islands.get_or_insert_with(|| Islands::compute(mesh, uv));
    // A UV vertex shared by several islands (malformed input) moves only once.
    let mut moved = vec![false; uv.uvs.len()];
    for i in editable {
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
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use uvcraft_core::CancelToken;

    #[test]
    fn camera_orthographic_project_is_serializable_undoable_and_validated() {
        let (mut session, _dir) = session();
        let before = uvs(&session);
        let mode = Projection::CameraOrthographic(uvcraft_algo::projection::CameraOrthographic {
            right: [1.0, 0.0, 0.0],
            up: [0.0, 1.0, 0.0],
            aspect: 1.0,
        });
        let cmd = Command::Project {
            islands: vec![0],
            mode,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(parse_script(&json).unwrap(), vec![cmd.clone()]);
        session.execute(cmd).unwrap();
        let after = uvs(&session);
        assert_ne!(after[..4], before[..4]);
        assert_eq!(after[4..], before[4..]);
        assert!(after.iter().all(|uv| uv.is_finite()));
        session.execute(Command::Undo).unwrap();
        assert_eq!(uvs(&session), before);

        let invalid = Command::Project {
            islands: vec![0],
            mode: Projection::CameraOrthographic(uvcraft_algo::projection::CameraOrthographic {
                right: [1.0, 0.0, 0.0],
                up: [1.0, 0.0, 0.0],
                aspect: 2.0,
            }),
        };
        assert!(matches!(
            session.execute(invalid),
            Err(CommandError::InvalidArgument(_))
        ));
        assert_eq!(uvs(&session), before);
    }

    #[test]
    fn camera_perspective_command_rejects_clipped_faces_without_history() {
        let (mut session, _dir) = session();
        let before = uvs(&session);
        let log_len = session.log().len();
        let camera = uvcraft_algo::projection::CameraPerspective {
            view_proj: [
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, -1.0, 0.0,
            ],
        };
        let cmd = Command::Project {
            islands: vec![0],
            mode: Projection::CameraPerspective(camera),
        };
        assert_eq!(
            parse_script(&serde_json::to_string(&cmd).unwrap()).unwrap(),
            vec![cmd.clone()]
        );
        assert!(matches!(
            session.execute(cmd),
            Err(CommandError::InvalidArgument(_))
        ));
        assert_eq!(uvs(&session), before);
        assert_eq!(session.log().len(), log_len);
    }

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
    fn brush_stroke_is_one_undoable_command() {
        let (mut s, _dir) = session();
        let before = uvs(&s);
        let vertices: Vec<usize> = (0..before.len()).collect();
        s.execute(Command::BrushEdit {
            weights: vec![1.0; vertices.len()],
            vertices,
            mode: BrushMode::Move,
            delta: [0.25, 0.0],
            strength: 1.0,
            iterations: 1,
        })
        .unwrap();
        let after = uvs(&s);
        assert_ne!(after, before);
        s.execute(Command::Undo).unwrap();
        assert_eq!(uvs(&s), before);
        s.execute(Command::Redo).unwrap();
        assert_eq!(uvs(&s), after);
    }

    #[test]
    fn density_paint_tracks_face_targets_optimizes_and_undoes() {
        let (mut s, _dir) = session();
        let before = uvs(&s);
        let area_before = {
            let doc = s.document().unwrap();
            let islands = Islands::compute(&doc.mesh, doc.uv());
            pack::island_areas(&doc.mesh, doc.uv(), &islands, 0).0
        };
        s.execute(Command::DensityPaint {
            faces: vec![0],
            weights: vec![1.0],
            target_density: 1500.0,
            resolution: 1024,
            strength: 1.0,
            optimize_iterations: 30,
        })
        .unwrap();
        let doc = s.document().unwrap();
        assert_eq!(doc.density_targets(), &[Some(1500.0), None]);
        let islands = Islands::compute(&doc.mesh, doc.uv());
        assert!(pack::island_areas(&doc.mesh, doc.uv(), &islands, 0).0 > area_before);
        assert_eq!(&doc.uv().uvs[4..], &before[4..]);
        s.execute(Command::Undo).unwrap();
        assert_eq!(uvs(&s), before);
        assert_eq!(s.document().unwrap().density_targets(), &[None, None]);
        let invalid = Command::DensityPaint {
            faces: vec![99],
            weights: vec![1.0],
            target_density: 1500.0,
            resolution: 1024,
            strength: 1.0,
            optimize_iterations: 10,
        };
        assert!(matches!(
            s.execute(invalid),
            Err(CommandError::InvalidArgument(_))
        ));
        assert_eq!(uvs(&s), before);
    }

    #[test]
    fn island_locks_survive_undo_and_keep_pack_unfold_optimize_stationary() {
        let (mut s, _dir) = session();
        let before = s.document().unwrap().uv().clone();
        s.execute(Command::LockIslands {
            islands: vec![0],
            locked: true,
        })
        .unwrap();
        assert!(s.document().unwrap().locked_island_faces()[0]);
        s.execute(Command::Undo).unwrap();
        assert!(!s.document().unwrap().locked_island_faces()[0]);
        s.execute(Command::Redo).unwrap();
        for json in [
            r#"{"cmd":"unfold","islands":[0]}"#,
            r#"{"cmd":"optimize","islands":[0],"iterations":2}"#,
            r#"{"cmd":"pack","islands":[0],"resolution":256}"#,
        ] {
            let cmd = parse_script(json).unwrap().remove(0);
            s.execute(cmd).unwrap();
            assert_eq!(s.document().unwrap().uv(), &before);
        }
    }

    #[test]
    fn locked_polygon_pins_its_vertices_without_changing_user_pins() {
        let (mut s, _dir) = session();
        s.execute(Command::LockFaces {
            faces: vec![0],
            locked: true,
        })
        .unwrap();
        let doc = s.document().unwrap();
        assert!(doc.locked_faces()[0]);
        assert!(doc.pins().iter().all(|p| !p));
        let effective = doc.effective_pins();
        for c in doc.mesh.face_corners(0) {
            assert!(effective[doc.mesh.corner_vertex(c)]);
        }
        let before = doc
            .mesh
            .face_corners(0)
            .map(|c| doc.uv().uv_at_corner(c))
            .collect::<Vec<_>>();
        let pack = parse_script(r#"{"cmd":"pack","resolution":256}"#)
            .unwrap()
            .remove(0);
        s.execute(pack).unwrap();
        let doc = s.document().unwrap();
        let after = doc
            .mesh
            .face_corners(0)
            .map(|c| doc.uv().uv_at_corner(c))
            .collect::<Vec<_>>();
        assert_eq!(before, after);
        assert!(matches!(
            s.execute(Command::LockFaces {
                faces: vec![usize::MAX],
                locked: true
            }),
            Err(CommandError::InvalidArgument(_))
        ));
        s.execute(Command::ClearLocks).unwrap();
        assert!(!s.document().unwrap().locked_faces()[0]);
        s.execute(Command::Undo).unwrap();
        assert!(s.document().unwrap().locked_faces()[0]);
    }

    #[test]
    fn polygon_lock_preserves_uvs_inside_a_partly_editable_island() {
        let (mut s, _dir) = session();
        let shared_edge = (0..s.document().unwrap().mesh.edge_count())
            .find(|&e| {
                let (a, b) = s.document().unwrap().mesh.edge_corners(e);
                b.is_some_and(|b| {
                    s.document().unwrap().mesh.corner_face(a)
                        != s.document().unwrap().mesh.corner_face(b)
                })
            })
            .unwrap();
        s.execute(Command::Weld {
            edges: vec![shared_edge],
        })
        .unwrap();
        s.execute(Command::LockFaces {
            faces: vec![0],
            locked: true,
        })
        .unwrap();
        let before = {
            let doc = s.document().unwrap();
            doc.mesh
                .face_corners(0)
                .map(|c| doc.uv().uv_at_corner(c))
                .collect::<Vec<_>>()
        };
        let optimize = parse_script(r#"{"cmd":"optimize","iterations":3}"#)
            .unwrap()
            .remove(0);
        s.execute(optimize).unwrap();
        let unfold = parse_script(r#"{"cmd":"unfold"}"#).unwrap().remove(0);
        s.execute(unfold).unwrap();
        let after = {
            let doc = s.document().unwrap();
            doc.mesh
                .face_corners(0)
                .map(|c| doc.uv().uv_at_corner(c))
                .collect::<Vec<_>>()
        };
        assert_eq!(before, after);
    }

    #[test]
    fn cancelled_unfold_keeps_document_history_and_uvs() {
        let (mut session, _dir) = session();
        let before = session.document().unwrap().uv().clone();
        let log_len = session.log().len();
        let token = CancelToken::default();
        let captured = token.clone();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let progress = Progress::new(token, move |event| {
            sink.lock().unwrap().push(event.clone());
            if event.completed == 1 {
                captured.cancel();
            }
        });
        let cmd = parse_script(r#"{"cmd":"unfold"}"#).unwrap().remove(0);
        assert!(matches!(
            session.execute_with_progress(cmd, &progress),
            Err(CommandError::Cancelled(_))
        ));
        assert_eq!(session.document().unwrap().uv(), &before);
        assert_eq!(session.log().len(), log_len);
        assert!(!session.can_undo());
        assert!(!events.lock().unwrap().is_empty());
    }

    #[test]
    fn cancelled_pack_after_layout_keeps_history_and_uvs() {
        let (mut session, _dir) = session();
        let before = session.document().unwrap().uv().clone();
        let token = CancelToken::default();
        let captured = token.clone();
        let progress = Progress::new(token, move |event| {
            if event.phase == "pack" && event.completed == 1 {
                captured.cancel();
            }
        });
        let cmd = parse_script(r#"{"cmd":"pack","method":"shelf","resolution":64}"#)
            .unwrap()
            .remove(0);
        assert!(matches!(
            session.execute_with_progress(cmd, &progress),
            Err(CommandError::Cancelled(_))
        ));
        assert_eq!(session.document().unwrap().uv(), &before);
        assert_eq!(session.log().len(), 1);
        assert!(!session.can_undo());
    }

    #[test]
    fn cancelled_load_save_and_projection_leave_no_partial_state() {
        let (mut session, dir) = session();
        let original = session.document().unwrap().uv().clone();
        let original_path = session.document().unwrap().path.clone();
        let make_progress = |phase: &'static str| {
            let token = CancelToken::default();
            let captured = token.clone();
            Progress::new(token, move |event| {
                if event.phase == phase && event.completed == 1 {
                    captured.cancel();
                }
            })
        };
        let load = Command::Load {
            path: original_path.clone(),
        };
        assert!(matches!(
            session.execute_with_progress(load, &make_progress("load")),
            Err(CommandError::Cancelled(_))
        ));
        assert_eq!(session.document().unwrap().path, original_path);
        assert_eq!(session.log().len(), 1);
        let output = dir.0.join("cancelled.gltf");
        assert!(matches!(
            session.execute_with_progress(
                Command::Save {
                    path: output.clone()
                },
                &make_progress("save")
            ),
            Err(CommandError::Cancelled(_))
        ));
        assert!(!output.exists());
        assert!(!dir.0.join("cancelled.bin").exists());
        assert_eq!(session.log().len(), 1);
        let project = parse_script(r#"{"cmd":"project","mode":"box"}"#)
            .unwrap()
            .remove(0);
        assert!(matches!(
            session.execute_with_progress(project, &make_progress("project")),
            Err(CommandError::Cancelled(_))
        ));
        assert_eq!(session.document().unwrap().uv(), &original);
        assert!(!session.can_undo());
        assert_eq!(session.log().len(), 1);
    }

    #[test]
    fn cancelling_cut_after_seam_step_restores_welded_uvs() {
        let (mut session, _dir) = session();
        session
            .execute(Command::Weld {
                edges: (0..7).collect(),
            })
            .unwrap();
        let edge = (0..session.document().unwrap().mesh.edge_count())
            .find(|&e| session.document().unwrap().mesh.edge_corners(e).1.is_some())
            .unwrap();
        let before = session.document().unwrap().uv().clone();
        let depth = session.undo.len();
        let log = session.log().len();
        let token = CancelToken::default();
        let captured = token.clone();
        let progress = Progress::new(token, move |event| {
            if event.phase == "cut_unfold" && event.completed == 1 {
                captured.cancel();
            }
        });
        assert!(matches!(
            session.execute_with_progress(Command::CutUnfold { edges: vec![edge] }, &progress),
            Err(CommandError::Cancelled(_))
        ));
        assert_eq!(session.document().unwrap().uv(), &before);
        assert_eq!(session.undo.len(), depth);
        assert_eq!(session.log().len(), log);
    }

    #[test]
    fn cancelling_stack_and_load_uvs_preserves_history() {
        let (mut session, _dir) = session();
        let before = session.document().unwrap().uv().clone();
        let path = session.document().unwrap().path.clone();
        let make_progress = |phase: &'static str| {
            let token = CancelToken::default();
            let captured = token.clone();
            Progress::new(token, move |event| {
                if event.phase == phase && event.completed == 1 {
                    captured.cancel();
                }
            })
        };
        let stack = parse_script(r#"{"cmd":"stack"}"#).unwrap().remove(0);
        assert!(matches!(
            session.execute_with_progress(stack, &make_progress("stack")),
            Err(CommandError::Cancelled(_))
        ));
        assert!(matches!(
            session.execute_with_progress(Command::LoadUvs { path }, &make_progress("load_uvs")),
            Err(CommandError::Cancelled(_))
        ));
        assert_eq!(session.document().unwrap().uv(), &before);
        assert_eq!(session.log().len(), 1);
        assert!(!session.can_undo());
    }

    #[test]
    fn switches_and_saves_two_existing_gltf_channels_with_undo() {
        let (mut s, dir) = session();
        let input = dir.0.join("channels.gltf");
        s.execute(Command::Save {
            path: input.clone(),
        })
        .unwrap();
        let mut root: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&input).unwrap()).unwrap();
        let old_accessor = root["meshes"][0]["primitives"][0]["attributes"]["TEXCOORD_0"]
            .as_u64()
            .unwrap() as usize;
        let old_view = root["accessors"][old_accessor]["bufferView"]
            .as_u64()
            .unwrap() as usize;
        let view = &root["bufferViews"][old_view];
        let offset = view["byteOffset"].as_u64().unwrap_or(0) as usize;
        let length = view["byteLength"].as_u64().unwrap() as usize;
        let bin_path = dir.0.join(root["buffers"][0]["uri"].as_str().unwrap());
        let mut bin = std::fs::read(&bin_path).unwrap();
        let copy = bin[offset..offset + length].to_vec();
        let new_offset = bin.len().next_multiple_of(4);
        bin.resize(new_offset, 0);
        bin.extend_from_slice(&copy);
        let new_view = root["bufferViews"].as_array().unwrap().len();
        root["bufferViews"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"buffer":0,"byteOffset":new_offset,"byteLength":length}));
        let mut accessor = root["accessors"][old_accessor].clone();
        accessor["bufferView"] = serde_json::json!(new_view);
        let new_accessor = root["accessors"].as_array().unwrap().len();
        root["accessors"].as_array_mut().unwrap().push(accessor);
        root["meshes"][0]["primitives"][0]["attributes"]["TEXCOORD_1"] =
            serde_json::json!(new_accessor);
        root["buffers"][0]["byteLength"] = serde_json::json!(bin.len());
        std::fs::write(&bin_path, bin).unwrap();
        std::fs::write(&input, serde_json::to_vec(&root).unwrap()).unwrap();

        s.execute(Command::Load { path: input }).unwrap();
        assert!(
            matches!(s.execute(Command::ListUvChannels).unwrap(), Outcome::Info(msg) if msg.contains("TEXCOORD_1"))
        );
        s.execute(Command::SwitchUvChannel { channel: 1 }).unwrap();
        s.execute(Command::Undo).unwrap();
        assert_eq!(s.document().unwrap().active_uv_channel, 0);
        s.execute(Command::Redo).unwrap();
        assert_eq!(s.document().unwrap().active_uv_channel, 1);
        let before = s.document().unwrap().uv().clone();
        let move_one = parse_script(r#"{"cmd":"transform","translate":[0.125,0]}"#)
            .unwrap()
            .remove(0);
        s.execute(move_one).unwrap();
        let changed_one = s.document().unwrap().uv().clone();
        s.execute(Command::Undo).unwrap();
        assert_eq!(s.document().unwrap().uv(), &before);
        s.execute(Command::Redo).unwrap();
        assert_eq!(s.document().unwrap().uv(), &changed_one);
        s.execute(Command::SwitchUvChannel { channel: 0 }).unwrap();
        let move_zero = parse_script(r#"{"cmd":"transform","translate":[0,0.25]}"#)
            .unwrap()
            .remove(0);
        s.execute(move_zero).unwrap();
        let changed_zero = s.document().unwrap().uv().clone();
        let output = dir.0.join("both.gltf");
        s.execute(Command::Save {
            path: output.clone(),
        })
        .unwrap();
        let parsed = uvcraft_io::read(&output).unwrap();
        assert_eq!(parsed.uv.uvs, changed_zero.uvs);
        let Source::Gltf(source) = parsed.source else {
            panic!("expected glTF");
        };
        let (_, saved_one) = source
            .activate_uv_channel(&parsed.mesh, 1)
            .unwrap()
            .unwrap();
        for c in 0..parsed.mesh.corner_count() {
            assert_eq!(saved_one.uv_at_corner(c), changed_one.uv_at_corner(c));
        }

        s.execute(Command::CreateUvChannel { channel: 2 }).unwrap();
        assert_eq!(s.document().unwrap().active_uv_channel, 2);
        assert_eq!(s.document().unwrap().uv().uvs, changed_zero.uvs);
        s.execute(Command::Undo).unwrap();
        assert_eq!(s.document().unwrap().active_uv_channel, 0);
        s.execute(Command::Redo).unwrap();
        let move_two = parse_script(r#"{"cmd":"transform","translate":[0.25,0]}"#)
            .unwrap()
            .remove(0);
        s.execute(move_two).unwrap();
        let changed_two = s.document().unwrap().uv().clone();
        s.execute(Command::SwapUvChannels { a: 0, b: 2 }).unwrap();
        assert_eq!(s.document().unwrap().active_uv_channel, 0);
        s.execute(Command::Undo).unwrap();
        assert_eq!(s.document().unwrap().active_uv_channel, 2);
        s.execute(Command::Redo).unwrap();
        let swapped = dir.0.join("swapped.gltf");
        s.execute(Command::Save {
            path: swapped.clone(),
        })
        .unwrap();
        let saved = uvcraft_io::read(&swapped).unwrap();
        for c in 0..saved.mesh.corner_count() {
            assert_eq!(saved.uv.uv_at_corner(c), changed_two.uv_at_corner(c));
        }
        let Source::Gltf(source) = saved.source else {
            panic!("expected glTF");
        };
        assert_eq!(source.uv_channels(), vec![0, 1, 2]);
        let (_, saved_two) = source.activate_uv_channel(&saved.mesh, 2).unwrap().unwrap();
        for c in 0..saved.mesh.corner_count() {
            assert_eq!(saved_two.uv_at_corner(c), changed_zero.uv_at_corner(c));
        }
    }

    #[test]
    fn fbx_channels_load_create_swap_and_save_through_session() {
        let (mut session, dir) = session();
        let (mesh, uv, source) = {
            let doc = session.document().unwrap();
            let Source::Obj(source) = &doc.source else {
                panic!("expected OBJ fixture")
            };
            (doc.mesh.clone(), doc.uv().clone(), source.clone())
        };
        let mut lightmap = uv.clone();
        lightmap.name = "Lightmap".into();
        for value in &mut lightmap.uvs {
            value.x += 0.2;
        }
        let layers = BTreeMap::from([(0, uv.clone()), (1, lightmap.clone())]);
        let input = dir.0.join("layered.fbx");
        std::fs::write(
            &input,
            uvcraft_io::fbx::export_layers(&mesh, &layers, &source).unwrap(),
        )
        .unwrap();
        session.execute(Command::Load { path: input }).unwrap();
        assert_eq!(
            session.document().unwrap().uv_channel_info(),
            Some((0, vec![0, 1]))
        );
        assert!(
            matches!(session.execute(Command::ListUvChannels).unwrap(), Outcome::Info(msg) if msg.contains("UV_1"))
        );
        session
            .execute(Command::SwitchUvChannel { channel: 1 })
            .unwrap();
        let move_one = parse_script(r#"{"cmd":"transform","translate":[0.125,0]}"#)
            .unwrap()
            .remove(0);
        session.execute(move_one).unwrap();
        let edited_one = session.document().unwrap().uv().clone();
        session
            .execute(Command::CreateUvChannel { channel: 2 })
            .unwrap();
        assert_eq!(
            session.document().unwrap().uv_channel_info(),
            Some((2, vec![0, 1, 2]))
        );
        let move_two = parse_script(r#"{"cmd":"transform","translate":[0,0.25]}"#)
            .unwrap()
            .remove(0);
        session.execute(move_two).unwrap();
        let edited_two = session.document().unwrap().uv().clone();
        session
            .execute(Command::SwapUvChannels { a: 0, b: 2 })
            .unwrap();
        assert_eq!(session.document().unwrap().active_uv_channel, 0);
        session.execute(Command::Undo).unwrap();
        assert_eq!(session.document().unwrap().active_uv_channel, 2);
        session.execute(Command::Redo).unwrap();
        let output = dir.0.join("edited.fbx");
        session
            .execute(Command::Save {
                path: output.clone(),
            })
            .unwrap();
        let saved = uvcraft_io::read(&output).unwrap();
        assert_eq!(
            saved.uv_channels.keys().copied().collect::<Vec<_>>(),
            [1, 2]
        );
        for corner in 0..saved.mesh.corner_count() {
            assert_eq!(
                saved.uv.uv_at_corner(corner),
                edited_two.uv_at_corner(corner)
            );
            assert_eq!(
                saved.uv_channels[&1].uv_at_corner(corner),
                edited_one.uv_at_corner(corner)
            );
            assert_eq!(
                saved.uv_channels[&2].uv_at_corner(corner),
                uv.uv_at_corner(corner)
            );
        }
    }

    #[test]
    fn gltf_node_transform_affects_measurements_not_export_geometry() {
        let (mut s, dir) = session();
        let gltf = dir.0.join("transformed.gltf");
        s.execute(Command::Save { path: gltf.clone() }).unwrap();
        let mut root: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&gltf).unwrap()).unwrap();
        root["nodes"][0]["scale"] = serde_json::json!([2, 2, 2]);
        std::fs::write(&gltf, serde_json::to_vec(&root).unwrap()).unwrap();

        s.execute(Command::Load { path: gltf }).unwrap();
        let doc = s.document().unwrap();
        assert_eq!(doc.mesh.positions()[1], glam::DVec3::new(1.0, 0.0, 0.0));
        assert_eq!(
            doc.measurement_mesh().positions()[1],
            glam::DVec3::new(2.0, 0.0, 0.0)
        );
        let Outcome::Report(report) = s
            .execute(Command::Analyze {
                resolution: 1024,
                output: None,
                padding_px: 0.0,
            })
            .unwrap()
        else {
            panic!("analyze should report");
        };
        assert!((report.area_3d - 8.0).abs() < 1e-8);
        assert!(report.measurement_local_meshes.is_empty());
    }

    #[test]
    fn pins_are_undoable_and_validated() {
        let (mut s, _dir) = session();
        let cmds = parse_script(r#"{"cmd":"pin","vertices":[0,2]}"#).unwrap();
        s.execute(cmds[0].clone()).unwrap();
        let pins = |s: &Session| s.document().unwrap().pins().iter().filter(|&&p| p).count();
        assert_eq!(pins(&s), 2);
        s.execute(Command::ClearPins).unwrap();
        assert_eq!(pins(&s), 0);
        s.execute(Command::Undo).unwrap();
        assert_eq!(pins(&s), 2);
        assert!(matches!(
            s.execute(Command::Pin {
                vertices: vec![9999],
                pinned: true
            }),
            Err(CommandError::InvalidArgument(_))
        ));
        assert_eq!(pins(&s), 2, "failed command leaves pins alone");
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
        // OBJ → GLB conversion: the two quads become four triangles. The seam
        // between them has coincident UVs, which glTF cannot keep apart.
        let glb = dir.0.join("x.glb");
        s.execute(Command::Save { path: glb.clone() }).unwrap();
        let converted = uvcraft_io::read(&glb).unwrap();
        assert_eq!(converted.mesh.face_count(), 4);
        assert_eq!(
            uvcraft_core::Islands::compute(&converted.mesh, &converted.uv).count(),
            1
        );

        let Outcome::Report(r) = s
            .execute(Command::Analyze {
                resolution: 1024,
                output: None,
                padding_px: 0.0,
            })
            .unwrap()
        else {
            panic!("expected a report");
        };
        assert_eq!(r.island_count, 2);
        assert_eq!(r.seam_edges, 1);
    }

    #[test]
    fn clear_seams_preserves_uvs_and_is_undoable() {
        let (mut session, _dir) = session();
        let edge = {
            let doc = session.document().unwrap();
            (0..doc.mesh.edge_count())
                .find(|&edge| doc.uv().edge_kind(&doc.mesh, edge) == uvcraft_core::EdgeKind::Seam)
                .unwrap()
        };
        let before = {
            let doc = session.document().unwrap();
            (0..doc.mesh.corner_count())
                .map(|corner| doc.uv().uv_at_corner(corner))
                .collect::<Vec<_>>()
        };
        session
            .execute(Command::ClearSeams { edges: vec![edge] })
            .unwrap();
        let doc = session.document().unwrap();
        for (corner, expected) in before.into_iter().enumerate() {
            assert_eq!(doc.uv().uv_at_corner(corner), expected);
        }
        assert_eq!(
            doc.uv().edge_kind(&doc.mesh, edge),
            uvcraft_core::EdgeKind::Interior
        );
        session.execute(Command::Undo).unwrap();
        let doc = session.document().unwrap();
        assert_eq!(
            doc.uv().edge_kind(&doc.mesh, edge),
            uvcraft_core::EdgeKind::Seam
        );
        session.execute(Command::Redo).unwrap();
        let doc = session.document().unwrap();
        assert_eq!(
            doc.uv().edge_kind(&doc.mesh, edge),
            uvcraft_core::EdgeKind::Interior
        );

        let (mut separated, _dir) = self::session();
        separated
            .execute(Command::Transform {
                islands: vec![1],
                translate: [1.0, 0.0],
                rotate_deg: 0.0,
                scale: [1.0, 1.0],
                pivot: Pivot::Origin,
            })
            .unwrap();
        let before = uvs(&separated);
        let log_len = separated.log().len();
        assert!(
            separated
                .execute(Command::ClearSeams { edges: vec![edge] })
                .is_err()
        );
        assert_eq!(uvs(&separated), before);
        assert_eq!(separated.log().len(), log_len);
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
                padding_px: 0.0,
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
    fn analyze_reports_mesh_problems() {
        let (mut s, _dir) = session();
        let Outcome::Report(r) = s
            .execute(parse_script(r#"{"cmd":"analyze"}"#).unwrap().remove(0))
            .unwrap()
        else {
            panic!()
        };
        let m = r.mesh_problems.as_ref().unwrap();
        assert!(m.is_clean(), "{m:?}");
        assert_eq!((m.components, m.boundary_edges), (1, 6));
        let json = serde_json::to_value(&*r).unwrap();
        assert_eq!(json["non_manifold_edges"], 0, "existing keys kept");
        assert_eq!(json["mesh_problems"]["components"], 1);
    }

    #[test]
    fn load_uvs_replaces_layer_and_is_undoable() {
        let (mut s, dir) = session();
        let original = s.document().unwrap().uv().clone();
        // Same topology, one welded island, different values.
        let other = dir.0.join("other.obj");
        std::fs::write(
            &other,
            "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 2 0 0\nv 2 1 0\n\
vt 0 0\nvt 0.25 0\nvt 0.25 0.5\nvt 0 0.5\nvt 0.5 0\nvt 0.5 0.5\n\
f 1/1 2/2 3/3 4/4\nf 2/2 5/5 6/6 3/3\n",
        )
        .unwrap();
        let cmd = parse_script(&format!(
            r#"{{"cmd":"load_uvs","path":{}}}"#,
            serde_json::to_string(&other).unwrap()
        ))
        .unwrap()
        .remove(0);
        s.execute(cmd).unwrap();
        let doc = s.doc.as_mut().unwrap();
        assert_eq!(doc.islands().count(), 1);
        assert_eq!(doc.uv().uv_at_corner(6), DVec2::new(0.5, 0.5));
        assert_eq!(doc.uv().uvs.len(), 6);

        s.execute(Command::Undo).unwrap();
        assert_eq!(s.document().unwrap().uv(), &original);
        assert_eq!(s.doc.as_mut().unwrap().islands().count(), 2);

        // Mismatched topology: different face size, and shuffled faces.
        let bad = dir.0.join("bad.obj");
        std::fs::write(
            &bad,
            "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 2 0 0\nf 1 2 3\nf 2 4 3\n",
        )
        .unwrap();
        let err = s.execute(Command::LoadUvs { path: bad }).unwrap_err();
        assert!(err.to_string().contains("face 0 has 3 corners"), "{err}");
        let swapped = dir.0.join("swapped.obj");
        std::fs::write(
            &swapped,
            "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 2 0 0\nv 2 1 0\n\
f 1 2 3 4\nf 2 3 4 1\n",
        )
        .unwrap();
        let err = s.execute(Command::LoadUvs { path: swapped }).unwrap_err();
        assert!(err.to_string().contains("connect"), "{err}");
        assert_eq!(s.document().unwrap().uv(), &original);
        assert!(s.can_redo(), "failed load_uvs leaves the history alone");
    }

    #[test]
    fn layout_commands_script() {
        let (mut s, _dir) = session();
        let original = uvs(&s);
        // Island 0: u ∈ [0, 0.5], island 1: u ∈ [0.5, 1]; both v ∈ [0, 1].
        let script = r#"
            {"cmd":"mirror","islands":[0],"axis":"u"}
            {"cmd":"orient","islands":[1],"mode":"min_bbox"}
            {"cmd":"align","axis":"v","to":"min","value":0.25}
            {"cmd":"distribute","axis":"u","by":"gaps","gap":0.125}
            {"cmd":"snap","islands":[1],"to":"bbox","resolution":12}
            {"cmd":"orient","islands":[0],"mode":{"edge":0},"vertical":true}
        "#;
        let cmds = parse_script(script).unwrap();
        for cmd in &cmds[..2] {
            s.execute(cmd.clone()).unwrap();
        }
        let u = uvs(&s);
        // Mirrored about u = 0.25: (0, 0) → (0.5, 0).
        assert!((u[0] - DVec2::new(0.5, 0.0)).length() < 1e-12);
        // Island 1 (0.5 × 1) now lies flat: 1 wide, 0.5 high, same center.
        let (lo, hi) = (
            u[4..].iter().fold(DVec2::INFINITY, |a, &p| a.min(p)),
            u[4..].iter().fold(DVec2::NEG_INFINITY, |a, &p| a.max(p)),
        );
        assert!((hi - lo - DVec2::new(1.0, 0.5)).length() < 1e-9);
        assert!(((lo + hi) / 2.0 - DVec2::new(0.75, 0.5)).length() < 1e-9);
        for cmd in &cmds[2..] {
            s.execute(cmd.clone()).unwrap();
        }
        let u = uvs(&s);
        assert!(u.iter().all(|p| p.y >= 0.25 - 1e-12));
        assert!(u.iter().all(|p| p.y.is_finite() && p.x.is_finite()));
        // Island 1 bbox min is on the 1/12 grid.
        let lo = u[4..].iter().fold(DVec2::INFINITY, |a, &p| a.min(p));
        assert!(((lo * 12.0).round() - lo * 12.0).length() < 1e-9, "{lo}");

        // Errors: bad edge, edge outside the targets, non-finite value, bad resolution.
        for bad in [
            r#"{"cmd":"orient","mode":{"edge":999}}"#,
            r#"{"cmd":"orient","islands":[1],"mode":{"edge":0}}"#,
            r#"{"cmd":"snap","resolution":0}"#,
            r#"{"cmd":"mirror","islands":[7],"axis":"v"}"#,
        ] {
            assert!(
                s.execute(parse_script(bad).unwrap().remove(0)).is_err(),
                "{bad}"
            );
        }
        // Everything is undoable and the log replays.
        while s.can_undo() {
            s.execute(Command::Undo).unwrap();
        }
        assert_eq!(uvs(&s), original);
        let log = s.log_jsonl();
        assert_eq!(parse_script(&log).unwrap(), s.log());
    }

    #[test]
    fn mosaic_script_cuts_a_cube_into_six_charts() {
        let (mut s, dir) = session();
        let cube = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 0 0 1\nv 1 0 1\nv 1 1 1\nv 0 1 1\n\
f 1 4 3 2\nf 5 6 7 8\nf 1 2 6 5\nf 2 3 7 6\nf 3 4 8 7\nf 4 1 5 8\n";
        let path = dir.0.join("cube.obj");
        std::fs::write(&path, cube).unwrap();
        let script = format!(
            "{{\"cmd\":\"load\",\"path\":{:?}}}\n\
             {{\"cmd\":\"mosaic\",\"charts\":6}}\n\
             {{\"cmd\":\"pack\",\"resolution\":256}}\n",
            path.to_str().unwrap()
        );
        for cmd in parse_script(&script).unwrap() {
            s.execute(cmd).unwrap();
        }
        let Outcome::Report(r) = s
            .execute(Command::Analyze {
                resolution: 1024,
                output: None,
                padding_px: 0.0,
            })
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(r.island_count, 6);
        assert_eq!(r.flipped_islands, 0);
        assert!((r.max_angle_distortion - 1.0).abs() < 1e-6);
        // Undo restores the single closed island; defaults parse.
        s.execute(Command::Undo).unwrap();
        s.execute(Command::Undo).unwrap();
        assert_eq!(s.doc.as_mut().unwrap().islands().count(), 1);
        assert_eq!(
            parse_script(r#"{"cmd":"mosaic"}"#).unwrap()[0],
            Command::Mosaic {
                islands: vec![],
                charts: None,
                max_error: None,
                max_seam_length: None,
                max_uv_distortion: None,
                iterations: 10,
                unfold: true
            }
        );
        assert!(matches!(
            s.execute(
                parse_script(r#"{"cmd":"mosaic","charts":0}"#)
                    .unwrap()
                    .remove(0)
            ),
            Err(CommandError::InvalidArgument(_))
        ));
        let Outcome::Info(note) = s
            .execute(
                parse_script(r#"{"cmd":"mosaic","unfold":false}"#)
                    .unwrap()
                    .remove(0),
            )
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(note, "6 chart(s)");
    }

    #[test]
    fn mosaic_seam_length_budget_rolls_back_on_failure() {
        let (mut s, dir) = session();
        let cube = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 0 0 1\nv 1 0 1\nv 1 1 1\nv 0 1 1\n\
f 1 4 3 2\nf 5 6 7 8\nf 1 2 6 5\nf 2 3 7 6\nf 3 4 8 7\nf 4 1 5 8\n";
        let path = dir.0.join("cube-budget.obj");
        std::fs::write(&path, cube).unwrap();
        s.execute(Command::Load { path }).unwrap();
        let before = s.doc.as_ref().unwrap().uv().clone();
        let bad = parse_script(r#"{"cmd":"mosaic","charts":6,"max_seam_length":0,"unfold":false}"#)
            .unwrap()
            .remove(0);
        assert!(matches!(
            s.execute(bad),
            Err(CommandError::InvalidArgument(_))
        ));
        assert_eq!(s.doc.as_ref().unwrap().uv(), &before);
        assert!(!s.can_undo());
        let good =
            parse_script(r#"{"cmd":"mosaic","charts":6,"max_seam_length":100,"unfold":false}"#)
                .unwrap()
                .remove(0);
        let outcome = s.execute(good).unwrap();
        assert!(
            matches!(outcome, Outcome::Info(ref note) if note.contains("added") && note.contains("limit 100"))
        );
        assert_eq!(s.doc.as_mut().unwrap().islands().count(), 6);
    }

    #[test]
    fn mosaic_uv_distortion_limit_respects_charts_and_seam_budget() {
        let (mut s, dir) = session();
        let cube = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 0 0 1\nv 1 0 1\nv 1 1 1\nv 0 1 1\n\
f 1 4 3 2\nf 5 6 7 8\nf 1 2 6 5\nf 2 3 7 6\nf 3 4 8 7\nf 4 1 5 8\n";
        let path = dir.0.join("cube-distortion.obj");
        std::fs::write(&path, cube).unwrap();
        s.execute(Command::Load { path }).unwrap();
        let before = s.doc.as_ref().unwrap().uv().clone();
        let strict = parse_script(
            r#"{"cmd":"mosaic","charts":1,"max_seam_length":100,"max_uv_distortion":1.01}"#,
        )
        .unwrap()
        .remove(0);
        assert!(
            matches!(s.execute(strict), Err(CommandError::InvalidArgument(ref msg)) if msg.contains("max_uv_distortion"))
        );
        assert_eq!(s.doc.as_ref().unwrap().uv(), &before);
        assert!(!s.can_undo());
        let good = parse_script(
            r#"{"cmd":"mosaic","charts":6,"max_seam_length":100,"max_uv_distortion":1.01}"#,
        )
        .unwrap()
        .remove(0);
        let outcome = s.execute(good).unwrap();
        assert!(matches!(outcome, Outcome::Info(ref note) if note.contains("max UV distortion")));
        assert_eq!(s.doc.as_mut().unwrap().islands().count(), 6);
        let invalid = parse_script(r#"{"cmd":"mosaic","max_uv_distortion":1.1,"unfold":false}"#)
            .unwrap()
            .remove(0);
        assert!(matches!(
            s.execute(invalid),
            Err(CommandError::InvalidArgument(_))
        ));
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
                    output: None,
                    padding_px: 0.0,
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
    #[test]
    fn analyze_reports_overlaps_and_padding() {
        let (mut s, _dir) = session();
        let report = |s: &mut Session, line: &str| {
            let mut last = None;
            for cmd in parse_script(line).unwrap() {
                if let Outcome::Report(r) = s.execute(cmd).unwrap() {
                    last = Some(r);
                }
            }
            last.unwrap()
        };
        // The two islands touch along u = 0.5.
        let r = report(
            &mut s,
            r#"{"cmd":"analyze","resolution":100,"padding_px":4}"#,
        );
        assert_eq!(r.overlapping_faces, 0);
        assert_eq!(r.padding_px, 4.0);
        assert_eq!(r.padding_violations.len(), 1);
        assert_eq!(r.padding_violations[0].islands, [0, 1]);
        assert!(r.padding_violations[0].distance_px.abs() < 1e-9);

        let script = r#"
            {"cmd":"transform","islands":[1],"translate":[0.1,0]}
            {"cmd":"analyze","resolution":100,"padding_px":4}
        "#;
        let r = report(&mut s, script);
        assert!(r.padding_violations.is_empty());

        let script = r#"
            {"cmd":"transform","islands":[1],"translate":[-0.4,0]}
            {"cmd":"analyze","resolution":100}
        "#;
        let r = report(&mut s, script);
        assert_eq!(r.overlapping_island_pairs, vec![[0, 1]]);
        assert_eq!(r.overlapping_faces, 2);
        assert!(r.padding_violations.is_empty(), "padding not checked");
        let json = serde_json::to_value(&*r).unwrap();
        assert!(json.get("islands_outside_unit_tile").is_some());
        assert_eq!(json["islands"][1]["overlaps_other_island"], true);
    }

    #[test]
    fn stack_unstack_and_match_scripts() {
        // Three identical quads (3D copies in separate objects) with different UVs.
        let obj = "v 0 0 0\nv 1 0 0\nv 1 2 0\nv 0 2 0\n\
v 0 0 5\nv 1 0 5\nv 1 2 5\nv 0 2 5\n\
v 0 0 9\nv 1 0 9\nv 1 2 9\nv 0 2 9\n\
vt 0 0\nvt 0.1 0\nvt 0.1 0.2\nvt 0 0.2\n\
vt 0.5 0.5\nvt 0.7 0.5\nvt 0.7 0.9\nvt 0.5 0.9\n\
vt 0.2 0.6\nvt 0.3 0.6\nvt 0.3 0.8\nvt 0.2 0.8\n\
o a\nf 1/1 2/2 3/3 4/4\no b\nf 5/5 6/6 7/7 8/8\no c\nf 9/9 10/10 11/11 12/12\n";
        let (mut s, dir) = session();
        let path = dir.0.join("copies.obj");
        std::fs::write(&path, obj).unwrap();
        s.execute(Command::Load { path }).unwrap();
        let corner_uvs = |s: &Session| {
            let doc = s.document().unwrap();
            (0..12)
                .map(|c| doc.uv().uv_at_corner(c))
                .collect::<Vec<_>>()
        };
        let script = r#"
            {"cmd":"similar","mode":"exact"}
            {"cmd":"stack","islands":[0,1],"mode":"exact"}
        "#;
        let cmds = parse_script(script).unwrap();
        assert_eq!(
            s.execute(cmds[0].clone()).unwrap(),
            Outcome::Info("similar groups: [[0, 1, 2]]".into())
        );
        s.execute(cmds[1].clone()).unwrap();
        assert_eq!(s.document().unwrap().stack_group_count(), 1);
        let after = corner_uvs(&s);
        assert_eq!(after[4..8], after[..4], "island 1 stacked onto island 0");
        assert_eq!(after[8], DVec2::new(0.2, 0.6), "island 2 untouched");

        let Outcome::Info(msg) = s
            .execute(parse_script(r#"{"cmd":"unstack"}"#).unwrap().remove(0))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(msg, "moved 1 stacked island(s)");
        assert_eq!(s.document().unwrap().stack_group_count(), 0);
        let unstacked = corner_uvs(&s);
        assert!((unstacked[4] - DVec2::new(0.11, 0.0)).length() < 1e-12);

        s.execute(
            parse_script(r#"{"cmd":"match","source":2,"targets":[0,1]}"#)
                .unwrap()
                .remove(0),
        )
        .unwrap();
        let matched = corner_uvs(&s);
        assert_eq!(matched[..4], matched[8..]);
        assert_eq!(matched[4..8], matched[8..]);

        // All three edits are undoable.
        for _ in 0..3 {
            s.execute(Command::Undo).unwrap();
        }
        assert_eq!(corner_uvs(&s)[4], DVec2::new(0.5, 0.5));
        assert!(matches!(
            s.execute(Command::Match {
                source: 7,
                targets: vec![0]
            }),
            Err(CommandError::InvalidIsland { index: 7, .. })
        ));
    }

    #[test]
    fn recorded_stacks_survive_undo_and_pack_as_one_item() {
        let (mut session, dir) = session();
        let stack = parse_script(r#"{"cmd":"stack","mode":"exact"}"#)
            .unwrap()
            .remove(0);
        session.execute(stack).unwrap();
        assert_eq!(session.document().unwrap().stack_group_count(), 1);
        let stacked = uvs(&session);
        assert_eq!(stacked[..4], stacked[4..8]);
        let saved = dir.0.join("stacked.obj");
        session
            .execute(Command::Save {
                path: saved.clone(),
            })
            .unwrap();
        assert!(sidecar_path(&saved).is_file());
        let mut reopened = Session::new();
        reopened
            .execute(Command::Load {
                path: saved.clone(),
            })
            .unwrap();
        assert_eq!(reopened.document().unwrap().stack_group_count(), 1);
        assert_eq!(uvs(&reopened), stacked);
        reopened
            .execute(
                parse_script(r#"{"cmd":"pack","resolution":128,"quality":"low"}"#)
                    .unwrap()
                    .remove(0),
            )
            .unwrap();
        let reopened_packed = uvs(&reopened);
        assert_eq!(reopened_packed[..4], reopened_packed[4..8]);
        let metadata_path = sidecar_path(&saved);
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&metadata_path).unwrap()).unwrap();
        legacy["version"] = 1.into();
        legacy.as_object_mut().unwrap().remove("island_groups");
        std::fs::write(&metadata_path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();
        let mut legacy_session = Session::new();
        legacy_session
            .execute(Command::Load {
                path: saved.clone(),
            })
            .unwrap();
        assert_eq!(legacy_session.document().unwrap().stack_group_count(), 1);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&saved)
            .unwrap()
            .write_all(b"# external edit\n")
            .unwrap();
        let mut stale = Session::new();
        stale
            .execute(Command::Load {
                path: saved.clone(),
            })
            .unwrap();
        assert_eq!(stale.document().unwrap().stack_group_count(), 0);

        let partial = parse_script(r#"{"cmd":"pack","islands":[0],"quality":"low"}"#)
            .unwrap()
            .remove(0);
        assert!(matches!(
            session.execute(partial),
            Err(CommandError::InvalidArgument(_))
        ));
        assert_eq!(uvs(&session), stacked);

        let pack =
            parse_script(r#"{"cmd":"pack","resolution":128,"padding_px":2,"quality":"low"}"#)
                .unwrap()
                .remove(0);
        session.execute(pack).unwrap();
        let packed = uvs(&session);
        assert_eq!(packed[..4], packed[4..8]);
        assert_eq!(session.document().unwrap().stack_group_count(), 1);
        session.execute(Command::Undo).unwrap();
        assert_eq!(uvs(&session), stacked);
        assert_eq!(session.document().unwrap().stack_group_count(), 1);
        session.execute(Command::Undo).unwrap();
        assert_eq!(session.document().unwrap().stack_group_count(), 0);
        session.execute(Command::Redo).unwrap();
        assert_eq!(session.document().unwrap().stack_group_count(), 1);

        let no_preserve = parse_script(r#"{"cmd":"pack","preserve_stacks":false,"resolution":128,"padding_px":2,"quality":"low"}"#)
            .unwrap()
            .remove(0);
        session.execute(no_preserve).unwrap();
        let separate = uvs(&session);
        assert_ne!(separate[..4], separate[4..8]);
        session
            .execute(parse_script(r#"{"cmd":"unstack"}"#).unwrap().remove(0))
            .unwrap();
        assert_eq!(session.document().unwrap().stack_group_count(), 0);
        session
            .execute(Command::Save {
                path: saved.clone(),
            })
            .unwrap();
        let mut cleared = Session::new();
        cleared.execute(Command::Load { path: saved }).unwrap();
        assert_eq!(cleared.document().unwrap().stack_group_count(), 0);
    }

    #[test]
    fn stack_preview_reference_and_individual_edit_invalidate_relation() {
        let (mut session, _dir) = session();
        let before = uvs(&session);
        let preview = parse_script(r#"{"cmd":"stack","islands":[0,1],"mode":"similar","reference":1,"preview":true}"#)
            .unwrap().remove(0);
        let outcome = session.execute(preview).unwrap();
        assert!(matches!(outcome, Outcome::Info(ref msg) if msg.contains("reference: 1")));
        assert_eq!(uvs(&session), before);
        assert_eq!(session.document().unwrap().stack_group_count(), 0);
        let stack = parse_script(r#"{"cmd":"stack","islands":[0,1],"mode":"similar","reference":1}"#)
            .unwrap().remove(0);
        session.execute(stack).unwrap();
        assert_eq!(&uvs(&session)[4..8], &before[4..8]);
        assert_eq!(session.document().unwrap().stack_group_count(), 1);
        let move_one = parse_script(r#"{"cmd":"transform","islands":[0],"translate":[0.3,0]}"#)
            .unwrap().remove(0);
        session.execute(move_one).unwrap();
        assert_eq!(session.document().unwrap().stack_group_count(), 0);
        session.execute(Command::Undo).unwrap();
        assert_eq!(session.document().unwrap().stack_group_count(), 1);
    }

    #[test]
    fn reference_image_outline_survives_stack_undo_save_and_reopen() {
        let outline = [(1.0, 5.0), (3.0, 5.0), (3.0, 3.0),
            (4.0, 1.8), (4.0, 0.5), (4.4, 0.0), (0.0, 0.0),
            (0.4, 0.5), (0.4, 1.8), (1.0, 3.0)];
        let mut first = vec![(2.0, 5.0)];
        first.extend(outline[1..].iter().chain(&outline[..1]).copied());
        let mut points = first.clone();
        points.extend(outline);
        points.push((2.0, 2.0));
        let mut obj = String::new();
        for (i, &(x, y)) in points.iter().enumerate() {
            obj.push_str(&format!("v {x} {y} {}\n", if i < first.len() { 0 } else { 2 }));
        }
        for (i, &(x, y)) in points.iter().enumerate() {
            let (u, v) = if i < first.len() { (x, y) }
                else { (-y * 0.7 + 10.0, x * 0.7 + 3.0) };
            obj.push_str(&format!("vt {u} {v}\n"));
        }
        obj.push_str("o reference\nf");
        for i in 1..=first.len() { obj.push_str(&format!(" {i}/{i}")); }
        obj.push('\n');
        obj.push_str("o target\n");
        let center = points.len();
        for i in 0..outline.len() {
            let a = first.len() + i + 1;
            let b = first.len() + (i + 1) % outline.len() + 1;
            obj.push_str(&format!("f {center}/{center} {a}/{a} {b}/{b}\n"));
        }
        let (mut session, dir) = session();
        let path = dir.0.join("reference_profile.obj");
        std::fs::write(&path, obj).unwrap();
        session.execute(Command::Load { path }).unwrap();
        let positions = session.document().unwrap().mesh.positions().to_vec();
        let before = uvs(&session);
        session.execute(parse_script(r#"{"cmd":"stack","mode":"similar","reference":0,"tolerance":0.01}"#)
            .unwrap().remove(0)).unwrap();
        assert_eq!(session.document().unwrap().stack_group_count(), 1);
        let stacked = uvs(&session);
        assert_eq!(session.document().unwrap().mesh.positions(), positions);
        let doc = session.document().unwrap();
        let islands = Islands::compute(&doc.mesh, doc.uv());
        assert!(stack::overlaid(&doc.mesh, doc.uv(), &islands, 0, 1, 1e-8));
        session.execute(Command::Undo).unwrap();
        assert_eq!(uvs(&session), before);
        session.execute(Command::Redo).unwrap();
        assert_eq!(uvs(&session), stacked);
        let saved = dir.0.join("stacked_profile.obj");
        session.execute(Command::Save { path: saved.clone() }).unwrap();
        let sidecar: serde_json::Value = serde_json::from_slice(
            &std::fs::read(sidecar_path(&saved)).unwrap()).unwrap();
        assert_eq!(sidecar["group_tolerances_ppm"][0], 10_000);
        let mut reopened = Session::new();
        reopened.execute(Command::Load { path: saved }).unwrap();
        assert_eq!(reopened.document().unwrap().stack_group_count(), 1);
        assert_eq!(reopened.document().unwrap().mesh.positions(), positions);
        assert_eq!(uvs(&reopened), stacked);
    }

    #[test]
    fn pack_into_region_and_udim_tiles() {
        let (mut s, _dir) = session();
        let corner_uvs = |s: &Session| {
            let uv = s.document().unwrap().uv();
            uv.corner_uv
                .iter()
                .map(|&k| uv.uvs[k as usize])
                .collect::<Vec<_>>()
        };
        let script = r#"
            {"cmd":"pack","region":[0.5,0.5,1,1],"margin_uv":0.01,"padding_uv":0.02,"mirror":true,"rotation_step_deg":45,"quality":"low"}
        "#;
        for cmd in parse_script(script).unwrap() {
            s.execute(cmd).unwrap();
        }
        for p in corner_uvs(&s) {
            assert!(
                p.min_element() >= 0.51 - 1e-9 && p.max_element() <= 0.99 + 1e-9,
                "{p}"
            );
        }

        // Each quad 0.7 UV wide: they do not fit in one tile, so the second
        // one overflows to the tile above (one column).
        let script = r#"
            {"cmd":"pack","tiles_u":1,"resolution":1024,"texel_density":716.8}
        "#;
        let cmd = parse_script(script).unwrap().remove(0);
        let out = s.execute(cmd).unwrap();
        assert_eq!(
            out,
            Outcome::Info("used 2 tile(s), 1001–1011".into()),
            "{out:?}"
        );
        let tiles: std::collections::BTreeSet<(i64, i64)> = corner_uvs(&s)
            .iter()
            .map(|p| (p.x.floor() as i64, p.y.floor() as i64))
            .collect();
        assert_eq!(tiles, [(0, 0), (0, 1)].into());
        assert_eq!(s.log().len(), 3, "load + 2 packs");

        for bad in [
            r#"{"cmd":"pack","tiles_u":11}"#,
            r#"{"cmd":"pack","tiles":0}"#,
            r#"{"cmd":"pack","region":[0,0,0,1]}"#,
            r#"{"cmd":"pack","rotation_step_deg":0}"#,
            r#"{"cmd":"pack","texel_density":-1}"#,
        ] {
            let cmd = parse_script(bad).unwrap().remove(0);
            assert!(
                matches!(s.execute(cmd), Err(CommandError::InvalidArgument(_))),
                "{bad}"
            );
        }
    }

    /// 3×3 quad grid, UVs = positions / 3, one island.
    fn grid_session() -> (Session, TempDir) {
        let dir = std::env::temp_dir().join(format!(
            "uvcraft-test-grid-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut obj = String::new();
        for y in 0..4 {
            for x in 0..4 {
                obj += &format!("v {x} {y} 0\nvt {} {}\n", x as f64 / 3.0, y as f64 / 3.0);
            }
        }
        for y in 0..3 {
            for x in 0..3 {
                let i = y * 4 + x + 1;
                let q = [i, i + 1, i + 5, i + 4];
                obj += &format!(
                    "f {0}/{0} {1}/{1} {2}/{2} {3}/{3}\n",
                    q[0], q[1], q[2], q[3]
                );
            }
        }
        let path = dir.join("grid.obj");
        std::fs::write(&path, obj).unwrap();
        let mut s = Session::new();
        s.execute(Command::Load { path }).unwrap();
        (s, TempDir(dir))
    }

    fn island_count(s: &mut Session) -> usize {
        s.doc.as_mut().unwrap().islands().count()
    }

    #[test]
    fn cut_by_loop_path_and_border() {
        let (mut s, _dir) = grid_session();
        assert_eq!(island_count(&mut s), 1);
        let script = r#"
            {"cmd":"cut_border","faces":[4]}
            {"cmd":"undo"}
            {"cmd":"cut_path","vertices":[1,13],"unfold":true}
        "#;
        let cmds = parse_script(script).unwrap();
        s.execute(cmds[0].clone()).unwrap();
        assert_eq!(island_count(&mut s), 2, "center face cut out");
        s.execute(cmds[1].clone()).unwrap();
        assert_eq!(island_count(&mut s), 1);
        s.execute(cmds[2].clone()).unwrap();
        assert_eq!(island_count(&mut s), 2, "column x = 1 cut through");
        let Outcome::Report(r) = s
            .execute(Command::Analyze {
                resolution: 1024,
                output: None,
                padding_px: 0.0,
            })
            .unwrap()
        else {
            panic!()
        };
        assert!((r.max_angle_distortion - 1.0).abs() < 1e-6);

        // Loop through the edge 2–6 (vertical, x = 2): the whole column.
        s.execute(Command::Undo).unwrap();
        let mesh = &s.document().unwrap().mesh;
        let e = (0..mesh.edge_count())
            .find(|&e| {
                let [a, b] = mesh.edge_vertices(e);
                [a.min(b), a.max(b)] == [2, 6]
            })
            .unwrap();
        s.execute(Command::CutLoop {
            edge: e,
            unfold: false,
        })
        .unwrap();
        assert_eq!(island_count(&mut s), 2);
        s.execute(Command::Undo).unwrap();
        let loop_edges = select::edge_loop(&s.document().unwrap().mesh, e);
        assert_eq!(loop_edges.len(), 3);
        let out = s.execute(Command::CutUnfold { edges: loop_edges }).unwrap();
        assert_eq!(out, Outcome::Done);
        assert_eq!(island_count(&mut s), 2);

        assert!(matches!(
            s.execute(Command::CutPath {
                vertices: vec![3],
                unfold: false
            }),
            Err(CommandError::InvalidArgument(_))
        ));
        assert!(matches!(
            s.execute(Command::CutBorder {
                faces: vec![99],
                unfold: false
            }),
            Err(CommandError::InvalidArgument(_))
        ));
        assert!(matches!(
            s.execute(Command::CutLoop {
                edge: 999,
                unfold: false
            }),
            Err(CommandError::InvalidEdge { .. })
        ));
        assert_eq!(parse_script(&s.log_jsonl()).unwrap(), s.log());
    }

    #[test]
    fn unfold_method_abf_from_script() {
        let (mut s, _dir) = session();
        let cmds = parse_script(
            r#"{"cmd":"unfold"}
            {"cmd":"unfold","islands":[1],"method":"abf"}"#,
        )
        .unwrap();
        assert_eq!(
            cmds[0],
            Command::Unfold {
                islands: vec![],
                method: UnfoldMethod::Lscm,
                preserve_orientation: false,
                border_weight: 0.0,
                prevent_overlap: false,
            }
        );
        for cmd in cmds {
            assert_eq!(s.execute(cmd).unwrap(), Outcome::Done);
        }
        let Outcome::Report(r) = s
            .execute(Command::Analyze {
                resolution: 1024,
                output: None,
                padding_px: 0.0,
            })
            .unwrap()
        else {
            panic!()
        };
        assert!((r.max_angle_distortion - 1.0).abs() < 1e-6);
        assert_eq!(r.flipped_triangles, 0);
        assert_eq!(parse_script(&s.log_jsonl()).unwrap(), s.log());
    }

    #[test]
    fn border_weight_preserves_shape_through_unfold_optimize_and_undo() {
        let (mut s, _dir) = session();
        s.execute(Command::Transform {
            islands: vec![0],
            translate: [0.0, 0.0],
            rotate_deg: 0.0,
            scale: [2.0, 0.5],
            pivot: Pivot::IslandCenter,
        })
        .unwrap();
        let reference = uvs(&s);
        let error = |points: &[DVec2]| -> f64 {
            [(0, 1), (1, 2), (2, 3), (3, 0)]
                .iter()
                .map(|&(a, b)| {
                    ((points[b] - points[a]) - (reference[b] - reference[a])).length_squared()
                })
                .sum()
        };

        let plain = Command::Unfold {
            islands: vec![0],
            method: UnfoldMethod::Lscm,
            preserve_orientation: false,
            border_weight: 0.0,
            prevent_overlap: false,
        };
        s.execute(plain).unwrap();
        let plain_error = error(&uvs(&s));
        assert!(plain_error > 1e-5);
        s.execute(Command::Undo).unwrap();
        assert_eq!(uvs(&s), reference);
        s.execute(Command::Unfold {
            islands: vec![0],
            method: UnfoldMethod::Lscm,
            preserve_orientation: false,
            border_weight: 20.0,
            prevent_overlap: false,
        })
        .unwrap();
        assert!(error(&uvs(&s)) < plain_error);
        s.execute(Command::Undo).unwrap();
        assert_eq!(uvs(&s), reference);

        s.execute(Command::Optimize {
            islands: vec![0],
            iterations: 20,
            method: OptimizeMethod::Arap,
            preserve_orientation: false,
            border_weight: 0.0,
            prevent_overlap: false,
        })
        .unwrap();
        let plain_error = error(&uvs(&s));
        assert!(plain_error > 1e-5);
        s.execute(Command::Undo).unwrap();
        s.execute(Command::Optimize {
            islands: vec![0],
            iterations: 20,
            method: OptimizeMethod::Arap,
            preserve_orientation: false,
            border_weight: 20.0,
            prevent_overlap: false,
        })
        .unwrap();
        assert!(error(&uvs(&s)) < plain_error);
        s.execute(Command::Undo).unwrap();
        assert_eq!(uvs(&s), reference);

        let invalid = parse_script(r#"{"cmd":"unfold","border_weight":-1}"#).unwrap();
        assert!(matches!(
            s.execute(invalid[0].clone()),
            Err(CommandError::InvalidArgument(_))
        ));
        assert_eq!(uvs(&s), reference);
    }

    #[test]
    fn island_groups_are_hierarchical_undoable_and_lock_edits() {
        let (mut s, _dir) = session();
        let commands = parse_script(r#"[
            {"cmd":"create_island_group","name":"Parent","islands":[0]},
            {"cmd":"create_island_group","name":"Child","parent":1,"islands":[1]},
            {"cmd":"set_island_group","group":1,"color":[255,0,0,255],"locked":true,"visible":false,"selected":true}
        ]"#).unwrap();
        for command in commands {
            s.execute(command).unwrap();
        }
        let groups = s.document().unwrap().island_groups();
        assert_eq!(groups.nodes().count(), 2);
        assert!(groups.face_locked(0) && groups.face_locked(1));
        assert!(!groups.face_visible(1));
        assert_eq!(groups.face_color(0), Some([255, 0, 0, 255]));
        assert_eq!(
            s.execute(Command::SelectIslandGroup { group: 1 }).unwrap(),
            Outcome::Selection {
                islands: vec![0, 1]
            }
        );

        let before = uvs(&s);
        s.execute(Command::Transform {
            islands: vec![0, 1],
            translate: [1.0, 0.0],
            rotate_deg: 0.0,
            scale: [1.0, 1.0],
            pivot: Pivot::Origin,
        })
        .unwrap();
        assert_eq!(uvs(&s), before);
        s.execute(Command::Project {
            islands: vec![0, 1],
            mode: Projection::Box,
        })
        .unwrap();
        assert_eq!(uvs(&s), before);
        s.execute(Command::SetIslandGroup {
            group: 1,
            name: Some("Renamed".into()),
            color: None,
            locked: Some(false),
            visible: Some(true),
            selected: Some(false),
        })
        .unwrap();
        assert_eq!(
            s.document().unwrap().island_groups().get(1).unwrap().name,
            "Renamed"
        );
        s.execute(Command::Undo).unwrap();
        assert!(s.document().unwrap().island_groups().get(1).unwrap().locked);
        s.execute(Command::UngroupIslands { group: 1 }).unwrap();
        let groups = s.document().unwrap().island_groups();
        assert_eq!(groups.get(2).unwrap().parent, None);
        assert_eq!(groups.face_group(0), None);
        assert!(!groups.face_locked(1));
    }

    #[test]
    fn island_group_transform_lock_undo_and_persistence() {
        let (mut s, dir) = session();
        s.execute(Command::CreateIslandGroup {
            name: "Parts".into(),
            parent: None,
            children: vec![],
            islands: vec![0, 1],
        })
        .unwrap();
        let before = uvs(&s);
        s.execute(Command::TransformIslandGroup {
            group: 1,
            translate: [0.25, -0.5],
            rotate_deg: 0.0,
            scale: 2.0,
        })
        .unwrap();
        let after = uvs(&s);
        for (old, new) in before.iter().zip(&after) {
            assert!(
                (*new
                    - (DVec2::new(0.5, 0.5)
                        + 2.0 * (*old - DVec2::new(0.5, 0.5))
                        + DVec2::new(0.25, -0.5)))
                .length()
                    < 1e-12
            );
        }
        s.execute(Command::Undo).unwrap();
        assert_eq!(uvs(&s), before);
        s.execute(Command::Redo).unwrap();
        assert_eq!(uvs(&s), after);

        s.execute(Command::SetIslandGroup {
            group: 1,
            name: None,
            color: None,
            locked: Some(true),
            visible: None,
            selected: None,
        })
        .unwrap();
        assert!(matches!(
            s.execute(Command::TransformIslandGroup {
                group: 1,
                translate: [1.0, 0.0],
                rotate_deg: 0.0,
                scale: 1.0,
            }),
            Err(CommandError::InvalidArgument(_))
        ));
        assert_eq!(uvs(&s), after);
        s.execute(Command::SetIslandGroup {
            group: 1,
            name: Some("Renamed".into()),
            color: None,
            locked: Some(false),
            visible: None,
            selected: None,
        })
        .unwrap();
        let path = dir.0.join("groups.obj");
        s.execute(Command::Save { path: path.clone() }).unwrap();
        let mut loaded = Session::new();
        loaded.execute(Command::Load { path }).unwrap();
        assert_eq!(uvs(&loaded), after);
        let topology = loaded.doc.as_mut().unwrap().islands().clone();
        let groups = loaded.document().unwrap().island_groups();
        assert_eq!(groups.get(1).unwrap().name, "Renamed");
        assert_eq!(groups.islands_in(1, &topology).unwrap(), vec![0, 1]);
    }

    #[test]
    fn group_density_uses_one_factor_and_respects_lock_atomically() {
        let (mut s, _dir) = session();
        s.execute(Command::CreateIslandGroup {
            name: "Density".into(),
            parent: None,
            children: vec![],
            islands: vec![0, 1],
        })
        .unwrap();
        let before = uvs(&s);
        let doc = s.document().unwrap();
        let islands = Islands::compute(&doc.mesh, doc.uv());
        let initial =
            pack::group_texel_density(&doc.mesh, doc.uv(), &islands, &[0, 1], 1024).unwrap();
        s.execute(Command::SetIslandGroupTexelDensity {
            group: 1,
            density: initial * 2.0,
            resolution: 1024,
        })
        .unwrap();
        let after = uvs(&s);
        let pivot = DVec2::new(0.5, 0.5);
        for (old, new) in before.iter().zip(&after) {
            assert!((*new - (pivot + (*old - pivot) * 2.0)).length() < 1e-9);
        }
        s.execute(Command::Undo).unwrap();
        assert_eq!(uvs(&s), before);
        s.execute(Command::SetIslandGroup {
            group: 1,
            name: None,
            color: None,
            locked: Some(true),
            visible: None,
            selected: None,
        })
        .unwrap();
        assert!(matches!(
            s.execute(Command::SetIslandGroupTexelDensity {
                group: 1,
                density: initial * 2.0,
                resolution: 1024,
            }),
            Err(CommandError::InvalidArgument(_))
        ));
        assert_eq!(uvs(&s), before);
    }

    #[test]
    fn group_membership_invalidates_on_split_and_undo_restores_it() {
        let (mut s, _dir) = grid_session();
        s.execute(Command::CreateIslandGroup {
            name: "Whole".into(),
            parent: None,
            children: vec![],
            islands: vec![0],
        })
        .unwrap();
        s.execute(Command::CutBorder {
            faces: vec![4],
            unfold: false,
        })
        .unwrap();
        let topology = s.doc.as_mut().unwrap().islands().clone();
        assert_eq!(topology.count(), 2);
        assert!(
            s.document()
                .unwrap()
                .island_groups()
                .islands_in(1, &topology)
                .unwrap()
                .is_empty()
        );
        s.execute(Command::Undo).unwrap();
        let topology = s.doc.as_mut().unwrap().islands().clone();
        assert_eq!(
            s.document()
                .unwrap()
                .island_groups()
                .islands_in(1, &topology)
                .unwrap(),
            vec![0]
        );
    }

    #[test]
    fn organizes_islands_by_object_material_and_smoothing_without_duplicates() {
        let (mut s, dir) = session();
        let path = dir.0.join("organized.obj");
        let obj = OBJ
            .replacen("f 1/1", "o Left\nusemtl Red\ns 3\nf 1/1", 1)
            .replacen("f 2/5", "o Right\nusemtl Blue\ns off\nf 2/5", 1);
        std::fs::write(&path, obj).unwrap();
        s.execute(Command::Load { path }).unwrap();
        for (by, names) in [
            (GroupBy::Object, ["Object: Left", "Object: Right"]),
            (GroupBy::Material, ["Material: Blue", "Material: Red"]),
            (GroupBy::SmoothingGroup, ["Smoothing: 3", "Smoothing: Off"]),
        ] {
            let command = Command::OrganizeIslandGroups {
                by,
                parent: None,
                islands: vec![],
            };
            s.execute(command.clone()).unwrap();
            let groups = s.document().unwrap().island_groups();
            let present: Vec<_> = groups.nodes().map(|group| group.name.as_str()).collect();
            assert!(names.iter().all(|name| present.contains(name)));
            assert_ne!(groups.face_group(0), groups.face_group(1));
            let count = groups.nodes().count();
            s.execute(command).unwrap();
            assert_eq!(s.document().unwrap().island_groups().nodes().count(), count);
        }
        s.execute(Command::Undo).unwrap(); // Repeated organize was a no-op.
        assert_eq!(s.document().unwrap().island_groups().nodes().count(), 4);
    }

    #[test]
    fn udim_pack_assigns_distinct_tiles_by_group_material_and_object() {
        let (mut s, dir) = session();
        let path = dir.0.join("udim-categories.obj");
        let obj = OBJ
            .replacen("f 1/1", "o Left\nusemtl Red\nf 1/1", 1)
            .replacen("f 2/5", "o Right\nusemtl Blue\nf 2/5", 1);
        std::fs::write(&path, obj).unwrap();
        s.execute(Command::Load { path }).unwrap();
        for (name, island) in [("A", 0), ("B", 1)] {
            s.execute(Command::CreateIslandGroup {
                name: name.into(),
                parent: None,
                children: vec![],
                islands: vec![island],
            })
            .unwrap();
        }
        let before = uvs(&s);
        for by in ["group", "material", "object"] {
            let text = format!(
                r#"{{"cmd":"pack","distribute_by":"{by}","tiles":2,"method":"shelf","padding_px":0,"margin_px":0,"orient":false,"rotate_90":false}}"#
            );
            let command = parse_script(&text).unwrap().remove(0);
            s.execute(command).unwrap();
            let doc = s.document().unwrap();
            let islands = Islands::compute(&doc.mesh, doc.uv());
            let tiles: Vec<i32> = (0..2)
                .map(|island| {
                    let ids = islands.uv_indices(island, &doc.mesh, doc.uv());
                    doc.uv().uvs[ids[0] as usize].x.floor() as i32
                })
                .collect();
            assert_eq!(tiles, vec![0, 1], "{by}");
            s.execute(Command::Undo).unwrap();
            assert_eq!(uvs(&s), before);
        }
        let bad = parse_script(r#"{"cmd":"pack","distribute_by":"group","tiles":1}"#)
            .unwrap()
            .remove(0);
        assert!(matches!(
            s.execute(bad),
            Err(CommandError::InvalidArgument(_))
        ));
        assert_eq!(uvs(&s), before);
    }

    #[test]
    fn unfold_can_preserve_previous_mirrored_orientation() {
        let (mut s, _dir) = session();
        s.execute(Command::Transform {
            islands: vec![0],
            translate: [0.0, 0.0],
            rotate_deg: 37.0,
            scale: [-1.0, 1.0],
            pivot: Pivot::IslandCenter,
        })
        .unwrap();
        let doc = s.doc.as_ref().unwrap();
        let corners: Vec<usize> = doc.mesh.face_corners(0).collect();
        let before: Vec<DVec2> = corners.iter().map(|&c| doc.uv().uv_at_corner(c)).collect();
        let cmd = parse_script(r#"{"cmd":"unfold","islands":[0],"preserve_orientation":true}"#)
            .unwrap()
            .remove(0);
        assert_eq!(s.execute(cmd).unwrap(), Outcome::Done);
        let doc = s.doc.as_ref().unwrap();
        let after: Vec<DVec2> = corners.iter().map(|&c| doc.uv().uv_at_corner(c)).collect();
        let area = |p: &[DVec2]| (p[1] - p[0]).perp_dot(p[2] - p[0]);
        assert!(area(&before) < 0.0 && area(&after) < 0.0);
        let center = |p: &[DVec2]| p.iter().copied().sum::<DVec2>() / p.len() as f64;
        let (a, b) = (center(&before), center(&after));
        let cross: f64 = before
            .iter()
            .zip(&after)
            .map(|(p, q)| (*q - b).perp_dot(*p - a))
            .sum();
        let dot: f64 = before
            .iter()
            .zip(&after)
            .map(|(p, q)| (*q - b).dot(*p - a))
            .sum();
        assert!(cross.abs() < dot.abs() * 1e-9);
        assert!(dot > 0.0);
    }

    #[test]
    fn select_islands_reports_quality_indices_without_editing() {
        let (mut s, _dir) = session();
        let query = parse_script(r#"{"cmd":"select_islands","problem":"overlapping"}"#)
            .unwrap()
            .remove(0);
        assert_eq!(
            s.execute(query.clone()).unwrap(),
            Outcome::Selection { islands: vec![] }
        );
        assert!(!s.can_undo());
        s.execute(Command::Transform {
            islands: vec![1],
            translate: [-0.5, 0.0],
            rotate_deg: 0.0,
            scale: [1.0, 1.0],
            pivot: Pivot::Origin,
        })
        .unwrap();
        assert_eq!(
            s.execute(query).unwrap(),
            Outcome::Selection {
                islands: vec![0, 1]
            }
        );
        s.execute(Command::Transform {
            islands: vec![1],
            translate: [2.0, 0.0],
            rotate_deg: 0.0,
            scale: [1.0, 1.0],
            pivot: Pivot::Origin,
        })
        .unwrap();
        assert_eq!(
            s.execute(Command::SelectIslands {
                problem: IslandProblem::OutsideTile
            })
            .unwrap(),
            Outcome::Selection { islands: vec![1] }
        );
    }

    #[test]
    fn packing_groups_keep_relative_island_placement() {
        let (mut s, _dir) = session();
        let center = |doc: &Document, i: usize| {
            let islands = Islands::compute(&doc.mesh, doc.uv());
            let ids = islands.uv_indices(i, &doc.mesh, doc.uv());
            ids.iter().map(|&k| doc.uv().uvs[k as usize]).sum::<DVec2>() / ids.len() as f64
        };
        let doc = s.doc.as_ref().unwrap();
        let before = center(doc, 1) - center(doc, 0);
        let cmd = parse_script(r#"{"cmd":"pack","groups":[[0,1]],"method":"shelf","orient":false,"rotate_90":false,"rescale":false,"padding_px":0,"margin_px":0}"#).unwrap().remove(0);
        s.execute(cmd).unwrap();
        let doc = s.doc.as_ref().unwrap();
        let after = center(doc, 1) - center(doc, 0);
        assert!((after - before).length() < 1e-9);
        let bad = parse_script(r#"{"cmd":"pack","groups":[[0,1],[1,0]]}"#)
            .unwrap()
            .remove(0);
        assert!(matches!(
            s.execute(bad),
            Err(CommandError::InvalidArgument(_))
        ));
    }

    #[test]
    fn document_group_packs_as_unit_and_accepts_nested_stack() {
        let (mut s, _dir) = session();
        s.execute(Command::CreateIslandGroup {
            name: "Layout".into(),
            parent: None,
            children: vec![],
            islands: vec![0, 1],
        })
        .unwrap();
        s.execute(Command::TransformIslandGroup {
            group: 1,
            translate: [2.0, 0.0],
            rotate_deg: 0.0,
            scale: 1.0,
        })
        .unwrap();
        let before = uvs(&s);
        let pack = parse_script(r#"{"cmd":"pack","method":"shelf","orient":false,"rotate_90":false,"rescale":false,"padding_px":0,"margin_px":0}"#).unwrap().remove(0);
        s.execute(pack.clone()).unwrap();
        let after = uvs(&s);
        assert_ne!(after, before);
        for i in 0..4 {
            assert!(((after[i + 4] - after[i]) - (before[i + 4] - before[i])).length() < 1e-9);
        }
        s.execute(Command::Undo).unwrap();
        assert_eq!(uvs(&s), before);
        let partial = parse_script(r#"{"cmd":"pack","islands":[0]}"#)
            .unwrap()
            .remove(0);
        let partial_result = s.execute(partial);
        assert!(
            matches!(partial_result, Err(CommandError::InvalidArgument(_))),
            "{partial_result:?}"
        );
        assert_eq!(uvs(&s), before);
        s.execute(
            parse_script(r#"{"cmd":"stack","mode":"exact"}"#)
                .unwrap()
                .remove(0),
        )
        .unwrap();
        assert_eq!(s.document().unwrap().stack_group_count(), 1);
        s.execute(pack).unwrap();
        let stacked = uvs(&s);
        assert_eq!(stacked[..4], stacked[4..8]);
    }

    #[test]
    fn contour_stack_keeps_uv_group_membership_through_pack() {
        let (mut session, _dir) = session();
        session.execute(Command::CreateIslandGroup {
            name: "Layout".into(), parent: None, children: vec![], islands: vec![0, 1],
        }).unwrap();
        session.execute(parse_script(r#"{"cmd":"stack","mode":"similar","reference":1}"#)
            .unwrap().remove(0)).unwrap();
        assert_eq!(session.document().unwrap().island_groups().group_count(), 1);
        assert_eq!(session.document().unwrap().stack_group_count(), 1);
        session.execute(parse_script(r#"{"cmd":"pack","resolution":128,"quality":"low"}"#)
            .unwrap().remove(0)).unwrap();
        let doc = session.document().unwrap();
        assert_eq!(doc.island_groups().group_count(), 1);
        assert_eq!(doc.stack_group_count(), 1);
        let islands = Islands::compute(&doc.mesh, doc.uv());
        assert!(stack::overlaid(&doc.mesh, doc.uv(), &islands, 0, 1, 1e-8));
    }

    #[test]
    fn pack_stacks_similar_islands_as_one_item() {
        let (mut s, _dir) = session();
        let cmd = parse_script(
            r#"{"cmd":"pack","stack_similar":true,"padding_px":0,"margin_px":0,"rescale":false}"#,
        )
        .unwrap()
        .remove(0);
        let outcome = s.execute(cmd).unwrap();
        assert!(
            matches!(outcome, Outcome::Info(ref note) if note.contains("stacked 1 similar group"))
        );
        let doc = s.doc.as_ref().unwrap();
        let isl = Islands::compute(&doc.mesh, doc.uv());
        let coords = |i| {
            let mut pts = isl
                .uv_indices(i, &doc.mesh, doc.uv())
                .into_iter()
                .map(|k| doc.uv().uvs[k as usize])
                .collect::<Vec<_>>();
            pts.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
            pts
        };
        assert_eq!(coords(0), coords(1));
        let bad = parse_script(r#"{"cmd":"pack","stack_similar":true,"groups":[[0,1]]}"#)
            .unwrap()
            .remove(0);
        assert!(matches!(
            s.execute(bad),
            Err(CommandError::InvalidArgument(_))
        ));
    }

    #[test]
    fn pack_pixel_aligned_option_snaps_island_origins() {
        let (mut s, _dir) = session();
        let cmd = parse_script(
            r#"{"cmd":"pack","pixel_aligned":true,"resolution":128,"padding_px":4,"margin_px":3}"#,
        )
        .unwrap()
        .remove(0);
        s.execute(cmd).unwrap();
        let doc = s.doc.as_ref().unwrap();
        let islands = Islands::compute(&doc.mesh, doc.uv());
        for i in 0..islands.count() {
            let lo = islands
                .uv_indices(i, &doc.mesh, doc.uv())
                .iter()
                .map(|&k| doc.uv().uvs[k as usize])
                .fold(DVec2::INFINITY, DVec2::min);
            assert!((lo.x * 128.0 - (lo.x * 128.0).round()).abs() < 1e-8);
            assert!((lo.y * 128.0 - (lo.y * 128.0).round()).abs() < 1e-8);
        }
    }

    #[test]
    fn auto_weld_only_joins_nearby_matching_seams() {
        let (mut s, _dir) = session();
        s.execute(Command::Transform {
            islands: vec![1],
            translate: [0.002, 0.0],
            rotate_deg: 0.0,
            scale: [1.0, 1.0],
            pivot: Pivot::Origin,
        })
        .unwrap();
        let small = parse_script(r#"{"cmd":"auto_weld","tolerance_uv":0.001}"#)
            .unwrap()
            .remove(0);
        assert_eq!(
            s.execute(small).unwrap(),
            Outcome::Info("welded 0 edge(s)".into())
        );
        assert_eq!(s.doc.as_mut().unwrap().islands().count(), 2);
        let near = parse_script(r#"{"cmd":"auto_weld","tolerance_uv":0.003}"#)
            .unwrap()
            .remove(0);
        assert_eq!(
            s.execute(near).unwrap(),
            Outcome::Info("welded 1 edge(s)".into())
        );
        assert_eq!(s.doc.as_mut().unwrap().islands().count(), 1);
        let bad = parse_script(r#"{"cmd":"auto_weld","tolerance_uv":-1}"#)
            .unwrap()
            .remove(0);
        assert!(matches!(
            s.execute(bad),
            Err(CommandError::InvalidArgument(_))
        ));
    }

    #[test]
    fn obj_hard_edges_become_uv_seams() {
        let (mut s, dir) = session();
        let obj = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 2 0 0\nv 2 1 0\n\
vt 0 0\nvt 0.5 0\nvt 0.5 1\nvt 0 1\nvt 1 0\nvt 1 1\n\
s 1\nf 1/1 2/2 3/3 4/4\ns 2\nf 2/2 5/5 6/6 3/3\n";
        let path = dir.0.join("hard.obj");
        std::fs::write(&path, obj).unwrap();
        s.execute(Command::Load { path }).unwrap();
        assert_eq!(s.doc.as_mut().unwrap().islands().count(), 1);
        let outcome = s.execute(Command::HardEdgesToSeams).unwrap();
        assert!(matches!(outcome, Outcome::Info(ref text) if text.contains("cut 1 hard edge")));
        assert_eq!(s.doc.as_mut().unwrap().islands().count(), 2);
        s.execute(Command::Undo).unwrap();
        assert_eq!(s.doc.as_mut().unwrap().islands().count(), 1);
    }

    #[test]
    fn explicit_obj_normals_become_uv_seams_even_with_one_smoothing_group() {
        let (mut session, dir) = session();
        let obj = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 0 0 1\nv 1 0 1\n\
vt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\nvt 0 2\nvt 1 2\n\
vn 0 0 1\nvn 0 -1 0\ns 1\nf 1/1/1 2/2/1 3/3/1 4/4/1\nf 2/2/2 1/1/2 5/5/2 6/6/2\n";
        let path = dir.0.join("normal_hard.obj");
        std::fs::write(&path, obj).unwrap();
        session.execute(Command::Load { path }).unwrap();
        assert_eq!(session.doc.as_mut().unwrap().islands().count(), 1);
        session.execute(Command::HardEdgesToSeams).unwrap();
        assert_eq!(session.doc.as_mut().unwrap().islands().count(), 2);
    }

    #[test]
    fn selects_obj_faces_by_effective_smoothing_group() {
        let (mut s, dir) = session();
        let obj = "v 0 0 0\nv 1 0 0\nv 0 1 0\nv 1 1 0\nv 2 0 0\n\
s 4\nf 1 2 3\ns off\nf 2 4 3\ns 4\nf 2 5 4\n";
        let path = dir.0.join("groups.obj");
        std::fs::write(&path, obj).unwrap();
        s.execute(Command::Load { path }).unwrap();
        assert_eq!(
            s.execute(Command::SelectFacesBySmoothingGroup { group: Some(4) })
                .unwrap(),
            Outcome::FaceSelection { faces: vec![0, 2] }
        );
        assert_eq!(
            s.execute(Command::SelectFacesBySmoothingGroup { group: None })
                .unwrap(),
            Outcome::FaceSelection { faces: vec![1] }
        );
        assert_eq!(
            s.execute(Command::SelectFacesBySmoothingGroup { group: Some(9) })
                .unwrap(),
            Outcome::FaceSelection { faces: vec![] }
        );
        s.doc.as_mut().unwrap().smoothing_override = Some(vec![Some(2), Some(2), None]);
        assert_eq!(
            s.execute(Command::SelectFacesBySmoothingGroup { group: Some(2) })
                .unwrap(),
            Outcome::FaceSelection { faces: vec![0, 1] }
        );
        let cmd = parse_script(r#"{"cmd":"select_faces_by_smoothing_group","group":null}"#)
            .unwrap()
            .remove(0);
        assert_eq!(
            s.execute(cmd).unwrap(),
            Outcome::FaceSelection { faces: vec![2] }
        );
    }

    #[test]
    fn selects_obj_faces_by_material_name_and_unassigned() {
        let (mut s, dir) = session();
        let obj = "v 0 0 0\nv 1 0 0\nv 0 1 0\nv 1 1 0\nv 2 0 0\n\
f 1 2 3\nusemtl red\nf 2 4 3\nusemtl blue\nf 2 5 4\nusemtl red\nf 1 3 4\n";
        let path = dir.0.join("materials.obj");
        std::fs::write(&path, obj).unwrap();
        s.execute(Command::Load { path }).unwrap();
        assert_eq!(
            s.execute(Command::SelectFacesByMaterial {
                material: Some("red".into()),
            })
            .unwrap(),
            Outcome::FaceSelection { faces: vec![1, 3] }
        );
        assert_eq!(
            s.execute(Command::SelectFacesByMaterial { material: None })
                .unwrap(),
            Outcome::FaceSelection { faces: vec![0] }
        );
        assert_eq!(
            s.execute(Command::SelectFacesByMaterial {
                material: Some("unknown".into()),
            })
            .unwrap(),
            Outcome::FaceSelection { faces: vec![] }
        );
    }

    #[test]
    fn selects_gltf_faces_by_material_name_and_unassigned() {
        let (mut s, dir) = session();
        let path = dir.0.join("materials.gltf");
        s.execute(Command::Save { path: path.clone() }).unwrap();
        let mut gltf: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        gltf["materials"] = serde_json::json!([{"name":"fabric"}]);
        gltf["meshes"][0]["primitives"][0]["material"] = serde_json::json!(0);
        std::fs::write(&path, serde_json::to_vec(&gltf).unwrap()).unwrap();
        s.execute(Command::Load { path: path.clone() }).unwrap();
        let count = s.document().unwrap().mesh.face_count();
        assert_eq!(
            s.execute(Command::SelectFacesByMaterial {
                material: Some("fabric".into()),
            })
            .unwrap(),
            Outcome::FaceSelection {
                faces: (0..count).collect(),
            }
        );
        gltf["meshes"][0]["primitives"][0]
            .as_object_mut()
            .unwrap()
            .remove("material");
        std::fs::write(&path, serde_json::to_vec(&gltf).unwrap()).unwrap();
        s.execute(Command::Load { path }).unwrap();
        assert_eq!(
            s.execute(Command::SelectFacesByMaterial { material: None })
                .unwrap(),
            Outcome::FaceSelection {
                faces: (0..count).collect(),
            }
        );
    }

    #[test]
    fn selects_connected_faces_up_to_dihedral_limit() {
        let (mut s, dir) = session();
        let obj = "v 0 0 0\nv 1 0 0\nv 0 1 0\nv 1 1 0\nv 2 0 1\n\
f 1 2 3\nf 2 4 3\nf 2 5 4\n";
        let path = dir.0.join("angles.obj");
        std::fs::write(&path, obj).unwrap();
        s.execute(Command::Load { path }).unwrap();
        let narrow = Command::SelectFacesByAngle {
            seed_face: 0,
            max_angle_deg: 30.0,
        };
        assert_eq!(
            s.execute(narrow).unwrap(),
            Outcome::FaceSelection { faces: vec![0, 1] }
        );
        assert_eq!(
            s.execute(Command::SelectFacesByAngle {
                seed_face: 0,
                max_angle_deg: 60.0,
            })
            .unwrap(),
            Outcome::FaceSelection {
                faces: vec![0, 1, 2]
            }
        );
        assert_eq!(
            s.execute(Command::SelectFacesByAngle {
                seed_face: 2,
                max_angle_deg: 30.0,
            })
            .unwrap(),
            Outcome::FaceSelection { faces: vec![2] }
        );
        assert!(matches!(
            s.execute(Command::SelectFacesByAngle {
                seed_face: 3,
                max_angle_deg: 30.0,
            }),
            Err(CommandError::InvalidArgument(_))
        ));
        assert!(matches!(
            s.execute(Command::SelectFacesByAngle {
                seed_face: 0,
                max_angle_deg: f64::NAN,
            }),
            Err(CommandError::InvalidArgument(_))
        ));
        let parsed =
            parse_script(r#"{"cmd":"select_faces_by_angle","seed_face":0,"max_angle_deg":30}"#)
                .unwrap()
                .remove(0);
        assert_eq!(
            s.execute(parsed).unwrap(),
            Outcome::FaceSelection { faces: vec![0, 1] }
        );
    }

    #[test]
    fn obj_seams_become_undoable_smoothing_groups_on_export() {
        let (mut s, dir) = session();
        let original = s.doc.as_ref().unwrap().source.clone();
        assert!(matches!(original, Source::Obj(_)));
        s.execute(Command::SeamsToHardEdges).unwrap();
        let out = dir.0.join("smoothing.obj");
        s.execute(Command::Save { path: out.clone() }).unwrap();
        let parsed = uvcraft_io::obj::read(&out).unwrap();
        assert_eq!(parsed.source.face_smoothing(), &[Some(1), Some(2)]);
        s.execute(Command::Undo).unwrap();
        s.execute(Command::Save { path: out.clone() }).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), OBJ.as_bytes());
        s.execute(Command::Redo).unwrap();
        s.execute(Command::Save { path: out.clone() }).unwrap();
        let parsed = uvcraft_io::obj::read(&out).unwrap();
        assert_eq!(parsed.source.face_smoothing(), &[Some(1), Some(2)]);
    }

    #[test]
    fn obj_smoothing_conversion_covers_cyclic_slit_with_supporting_hard_edge() {
        let (mut s, dir) = session();
        let obj = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 0.5 0.5 0\n\
vt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\nvt 0.5 0.5\nvt 1 0\n\
f 1/1 2/6 5/5\nf 2/2 3/3 5/5\nf 3/3 4/4 5/5\nf 4/4 1/1 5/5\n";
        let path = dir.0.join("slit.obj");
        std::fs::write(&path, obj).unwrap();
        s.execute(Command::Load { path }).unwrap();
        let before = s.doc.as_ref().unwrap().uv().clone();
        let seam_edges: Vec<_> = (0..s.document().unwrap().mesh.edge_count())
            .filter(|&edge| {
                before.edge_kind(&s.document().unwrap().mesh, edge) == uvcraft_core::EdgeKind::Seam
            })
            .collect();
        assert_eq!(seam_edges.len(), 1);
        let outcome = s.execute(Command::SeamsToHardEdges).unwrap();
        assert!(
            matches!(outcome, Outcome::Info(ref note) if note.contains("supporting hard edge"))
        );
        assert_eq!(s.doc.as_ref().unwrap().uv(), &before);
        let out = dir.0.join("slit-hard.obj");
        s.execute(Command::Save { path: out.clone() }).unwrap();
        let saved = uvcraft_io::obj::read(&out).unwrap();
        let mut hard = seams::hard_edges_from_smoothing(&saved.mesh, saved.source.face_smoothing());
        hard.extend(saved.source.normal_discontinuity_edges(&saved.mesh));
        assert!(seam_edges.iter().all(|edge| hard.contains(edge)));
        assert!(hard.len() >= 2);
        s.execute(Command::Undo).unwrap();
        assert!(s.doc.as_ref().unwrap().smoothing_override.is_none());
    }

    #[test]
    fn pelt_unfolds_a_closed_organic_mesh_and_undoes() {
        use std::fmt::Write as _;
        let (mut s, dir) = session();
        let path = dir.0.join("octa-pelt.obj");
        let mut obj = String::new();
        for p in [
            [0., 0., 1.],
            [0., 0., -1.],
            [1., 0., 0.],
            [0., 1., 0.],
            [-1., 0., 0.],
            [0., -1., 0.],
        ] {
            writeln!(obj, "v {} {} {}", p[0], p[1], p[2]).unwrap();
        }
        for _ in 0..6 {
            obj.push_str("vt 0 0\n");
        }
        for face in [
            [1, 3, 4],
            [1, 4, 5],
            [1, 5, 6],
            [1, 6, 3],
            [2, 4, 3],
            [2, 5, 4],
            [2, 6, 5],
            [2, 3, 6],
        ] {
            writeln!(obj, "f {0}/{0} {1}/{1} {2}/{2}", face[0], face[1], face[2]).unwrap();
        }
        std::fs::write(&path, obj).unwrap();
        s.execute(Command::Load { path }).unwrap();
        assert_eq!(s.document().unwrap().islands.as_ref().unwrap().count(), 1);
        let before = s.document().unwrap().uv().clone();
        let outcome = s
            .execute(Command::Pelt {
                islands: Vec::new(),
                branches: 4,
                unfold: true,
                relax_iterations: 5,
            })
            .unwrap();
        assert!(
            matches!(outcome, Outcome::Info(ref note) if note.contains("unfolded") && note.contains("geodesic"))
        );
        let doc = s.document().unwrap();
        assert!(doc.uv().uvs.iter().all(|p| p.is_finite()));
        assert!(doc.uv().uvs.iter().any(|p| p.length_squared() > 1e-8));
        assert!(
            (0..doc.mesh.edge_count())
                .any(|edge| doc.uv().edge_kind(&doc.mesh, edge) == uvcraft_core::EdgeKind::Seam)
        );
        s.execute(Command::Undo).unwrap();
        assert_eq!(s.document().unwrap().uv(), &before);
    }

    #[test]
    fn pelt_cuts_a_curved_open_patch_without_unfold() {
        use std::fmt::Write as _;
        let (mut s, dir) = session();
        let path = dir.0.join("cap-pelt.obj");
        let mut obj = String::new();
        for y in 0..5 {
            for x in 0..5 {
                let dx = x as f64 - 2.0;
                let dy = y as f64 - 2.0;
                writeln!(obj, "v {dx} {dy} {}", 1.0 - 0.1 * (dx * dx + dy * dy)).unwrap();
            }
        }
        for y in 0..5 {
            for x in 0..5 {
                writeln!(obj, "vt {} {}", x as f64 / 4.0, y as f64 / 4.0).unwrap();
            }
        }
        for y in 0..4 {
            for x in 0..4 {
                let a = y * 5 + x + 1;
                writeln!(
                    obj,
                    "f {0}/{0} {1}/{1} {2}/{2} {3}/{3}",
                    a,
                    a + 1,
                    a + 6,
                    a + 5
                )
                .unwrap();
            }
        }
        std::fs::write(&path, obj).unwrap();
        s.execute(Command::Load { path }).unwrap();
        let before = s.document().unwrap().uv().clone();
        s.execute(Command::Pelt {
            islands: vec![0],
            branches: 3,
            unfold: false,
            relax_iterations: 0,
        })
        .unwrap();
        let doc = s.document().unwrap();
        assert_ne!(doc.uv().corner_uv, before.corner_uv);
        assert!(doc.uv().uvs.iter().all(|p| p.is_finite()));
    }

    #[test]
    fn equalize_density_preserves_combined_uv_area() {
        let (mut s, _dir) = session();
        let transform = parse_script(r#"{"cmd":"transform","islands":[1],"scale":[0.5,0.5]}"#)
            .unwrap()
            .remove(0);
        s.execute(transform).unwrap();
        let areas = |doc: &Document| {
            let isl = Islands::compute(&doc.mesh, doc.uv());
            (0..isl.count())
                .map(|i| pack::island_areas(&doc.mesh, doc.uv(), &isl, i))
                .collect::<Vec<_>>()
        };
        let before = areas(s.doc.as_ref().unwrap());
        let cmd = parse_script(r#"{"cmd":"equalize_texel_density","resolution":1024}"#)
            .unwrap()
            .remove(0);
        assert_eq!(s.execute(cmd).unwrap(), Outcome::Done);
        let after = areas(s.doc.as_ref().unwrap());
        let total_before: f64 = before.iter().map(|a| a.0).sum();
        let total_after: f64 = after.iter().map(|a| a.0).sum();
        assert!((total_before - total_after).abs() < 1e-9);
        let density = |a: &(f64, f64)| 1024.0 * (a.0 / a.1).sqrt();
        assert!((density(&after[0]) - density(&after[1])).abs() < 1e-9);
        let normalized = s.document().unwrap().uv().clone();
        s.execute(Command::Undo).unwrap();
        let alias = parse_script(r#"{"cmd":"average_island_scale","resolution":1024}"#)
            .unwrap()
            .remove(0);
        assert_eq!(s.execute(alias).unwrap(), Outcome::Done);
        assert_eq!(s.document().unwrap().uv(), &normalized);
    }

    #[test]
    fn refine_uses_existing_uvs_and_keeps_pins() {
        let (mut s, _dir) = session();
        s.execute(Command::Pin {
            vertices: vec![0, 1],
            pinned: true,
        })
        .unwrap();
        let doc = s.doc.as_mut().unwrap();
        let corners = doc.mesh.face_corners(0);
        let uv_ids: Vec<usize> = corners.map(|c| doc.uv().corner_uv[c] as usize).collect();
        doc.uv.uvs[uv_ids[2]].y += 0.25;
        let before = uv_ids.iter().map(|&k| doc.uv().uvs[k]).collect::<Vec<_>>();
        let cmd = parse_script(r#"{"cmd":"refine","islands":[0],"iterations":30}"#)
            .unwrap()
            .remove(0);
        assert_eq!(s.execute(cmd).unwrap(), Outcome::Done);
        let after = uv_ids
            .iter()
            .map(|&k| s.doc.as_ref().unwrap().uv().uvs[k])
            .collect::<Vec<_>>();
        assert_eq!(before[0], after[0]);
        assert_eq!(before[1], after[1]);
        assert!(before[2].distance(after[2]) > 1e-6 || before[3].distance(after[3]) > 1e-6);
        let doc = s.doc.as_mut().unwrap();
        for &k in &uv_ids {
            doc.uv.uvs[k].y = 0.0;
        }
        let invalid = parse_script(r#"{"cmd":"refine","islands":[0]}"#)
            .unwrap()
            .remove(0);
        assert!(matches!(s.execute(invalid).unwrap(), Outcome::Info(_)));
        assert!(
            uv_ids
                .iter()
                .all(|&k| s.doc.as_ref().unwrap().uv().uvs[k].y == 0.0)
        );
    }

    #[test]
    fn weighted_angle_area_optimization_is_a_scriptable_edit() {
        let (mut s, _dir) = session();
        let command = parse_script(
            r#"{"cmd":"optimize_angle_area","islands":[0],"iterations":3,"area_weight":0.5}"#,
        )
        .unwrap()
        .remove(0);
        assert_eq!(s.execute(command).unwrap(), Outcome::Done);
        let invalid = parse_script(r#"{"cmd":"optimize_angle_area","area_weight":1.5}"#)
            .unwrap()
            .remove(0);
        assert!(matches!(
            s.execute(invalid),
            Err(CommandError::InvalidArgument(_))
        ));
    }

    #[test]
    fn padding_perfect_is_scriptable() {
        let (mut s, _dir) = session();
        let command = parse_script(
            r#"{"cmd":"pack","padding_perfect":true,"padding_px":2,"resolution":256}"#,
        )
        .unwrap()
        .remove(0);
        assert!(s.execute(command).is_ok());
    }

    #[test]
    fn straightens_a_vertex_path_and_rejects_ambiguous_input() {
        let (mut s, _dir) = session();
        let command = parse_script(r#"{"cmd":"straighten_path","island":0,"vertices":[0,1,2]}"#)
            .unwrap()
            .remove(0);
        assert_eq!(s.execute(command).unwrap(), Outcome::Done);
        let invalid =
            parse_script(r#"{"cmd":"straighten_path","island":0,"vertices":[0,1],"edges":[0]}"#)
                .unwrap()
                .remove(0);
        assert!(matches!(
            s.execute(invalid),
            Err(CommandError::InvalidArgument(_))
        ));
    }

    #[test]
    fn aligns_border_path_and_respects_locks() {
        let (mut s, _dir) = session();
        s.doc.as_mut().unwrap().uv.uvs[1].y = 0.2;
        let cmd = parse_script(r#"{"cmd":"align_border","island":0,"axis":"u","vertices":[0,1]}"#)
            .unwrap()
            .remove(0);
        assert_eq!(s.execute(cmd.clone()).unwrap(), Outcome::Done);
        let doc = s.document().unwrap();
        assert!((doc.uv().uvs[0].y - 0.1).abs() < 1e-12);
        assert!((doc.uv().uvs[1].y - 0.1).abs() < 1e-12);
        s.execute(Command::LockIslands {
            islands: vec![0],
            locked: true,
        })
        .unwrap();
        let before = s.document().unwrap().uv().clone();
        assert!(matches!(s.execute(cmd).unwrap(), Outcome::Info(_)));
        assert_eq!(s.document().unwrap().uv(), &before);
    }

    #[test]
    fn rectangularizes_a_quad_island_from_script() {
        let (mut s, _dir) = session();
        let command = parse_script(r#"{"cmd":"rectangularize","island":0}"#)
            .unwrap()
            .remove(0);
        assert_eq!(s.execute(command).unwrap(), Outcome::Done);
    }

    #[test]
    fn straighten_island_is_undoable_and_respects_locks() {
        let (mut session, _dir) = grid_session();
        session.doc.as_mut().unwrap().uv.uvs[5].x += 0.15;
        let before = session.document().unwrap().uv().clone();
        let command = parse_script(r#"{"cmd":"straighten_island","island":0}"#)
            .unwrap()
            .remove(0);
        assert_eq!(session.execute(command.clone()).unwrap(), Outcome::Done);
        assert_ne!(session.document().unwrap().uv(), &before);
        session.execute(Command::Undo).unwrap();
        assert_eq!(session.document().unwrap().uv(), &before);
        session
            .execute(Command::LockIslands {
                islands: vec![0],
                locked: true,
            })
            .unwrap();
        assert!(matches!(
            session.execute(command).unwrap(),
            Outcome::Info(_)
        ));
        assert_eq!(session.document().unwrap().uv(), &before);
    }

    #[test]
    fn physical_measurements_and_real_scale_are_scriptable() {
        let (mut s, _dir) = session();
        let measure = parse_script(r#"{"cmd":"measure_physical","islands":[0],"mesh_unit":"meter","output_unit":"centimeter"}"#)
            .unwrap().remove(0);
        let Outcome::PhysicalReport(report) = s.execute(measure).unwrap() else {
            panic!("physical report expected")
        };
        assert_eq!(report.island_areas.len(), 1);
        assert!((report.island_areas[0].area - 10_000.0).abs() < 1e-8);
        let scale = parse_script(
            r#"{"cmd":"set_real_scale","islands":[0],"mesh_unit":"meter","uv_unit":"millimeter"}"#,
        )
        .unwrap()
        .remove(0);
        assert_eq!(s.execute(scale).unwrap(), Outcome::Done);
        let doc = s.document().unwrap();
        let islands = Islands::compute(&doc.mesh, doc.uv());
        let report = analysis::analyze_island(&doc.mesh, doc.uv(), &islands, 0, 1);
        assert!((report.uv_area - 1_000_000.0).abs() < 1e-6);
        let before = doc.uv().clone();
        let too_small = parse_script(r#"{"cmd":"pack_physical","islands":[0],"width":100,"height":100,"unit":"millimeter","uv_unit":"millimeter"}"#)
            .unwrap().remove(0);
        assert!(matches!(
            s.execute(too_small),
            Err(CommandError::InvalidArgument(_))
        ));
        assert_eq!(s.document().unwrap().uv(), &before);
        let fits = parse_script(r#"{"cmd":"pack_physical","islands":[0],"width":2000,"height":2000,"unit":"millimeter","uv_unit":"millimeter"}"#)
            .unwrap().remove(0);
        assert_eq!(s.execute(fits).unwrap(), Outcome::Done);
        let doc = s.document().unwrap();
        let islands = Islands::compute(&doc.mesh, doc.uv());
        let packed_area = analysis::analyze_island(&doc.mesh, doc.uv(), &islands, 0, 1).uv_area;
        assert!((packed_area - 1_000_000.0).abs() < 1e-6);
    }

    #[test]
    fn exact_real_scale_is_scriptable_and_undoable() {
        let (mut session, _dir) = session();
        let before = session.document().unwrap().uv().clone();
        let cmd = parse_script(r#"{"cmd":"set_exact_real_scale","islands":[0],"mesh_unit":"meter","uv_unit":"millimeter"}"#).unwrap().remove(0);
        assert_eq!(session.execute(cmd).unwrap(), Outcome::Done);
        let doc = session.document().unwrap();
        for edge in 0..doc.mesh.edge_count() {
            let (corner, twin) = doc.mesh.edge_corners(edge);
            if doc.mesh.corner_face(corner) == 0
                || twin.is_some_and(|c| doc.mesh.corner_face(c) == 0)
            {
                let corner = if doc.mesh.corner_face(corner) == 0 {
                    corner
                } else {
                    twin.unwrap()
                };
                let length = doc
                    .uv()
                    .uv_at_corner(corner)
                    .distance(doc.uv().uv_at_corner(doc.mesh.next(corner)));
                assert!((length - doc.mesh.edge_length(edge) * 1000.0).abs() < 1e-8);
            }
        }
        session.execute(Command::Undo).unwrap();
        assert_eq!(session.document().unwrap().uv(), &before);
    }

    #[test]
    fn selects_islands_by_texel_density_and_validates_bounds() {
        let (mut s, _dir) = session();
        let cmd = parse_script(
            r#"{"cmd":"select_islands_by_metric","metric":"texel_density","min":90,"resolution":100}"#,
        )
        .unwrap()
        .remove(0);
        assert_eq!(
            s.execute(cmd.clone()).unwrap(),
            Outcome::Selection { islands: vec![] }
        );
        assert!(!s.can_undo());
        s.execute(Command::Transform {
            islands: vec![1],
            translate: [0.0; 2],
            rotate_deg: 0.0,
            scale: [2.0, 1.0],
            pivot: Pivot::IslandCenter,
        })
        .unwrap();
        assert_eq!(
            s.execute(cmd).unwrap(),
            Outcome::Selection { islands: vec![1] }
        );
        let invalid = Command::SelectIslandsByMetric {
            metric: IslandMetric::TexelDensity,
            min: Some(100.0),
            max: Some(10.0),
            resolution: 100,
        };
        assert!(matches!(
            s.execute(invalid),
            Err(CommandError::InvalidArgument(_))
        ));
    }

    #[test]
    fn box_planar_seams_split_perpendicular_charts_and_undo() {
        let dir = std::env::temp_dir().join(format!(
            "uvcraft-box-planar-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cleanup = TempDir(dir.clone());
        let path = dir.join("fold.obj");
        std::fs::write(&path, "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 0 0 1\nv 1 0 1\nvt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\nvt 0 2\nvt 1 2\nf 1/1 2/2 3/3 4/4\nf 2/2 1/1 5/5 6/6\n").unwrap();
        let mut session = Session::new();
        session.execute(Command::Load { path }).unwrap();
        assert_eq!(
            Islands::compute(
                &session.document().unwrap().mesh,
                session.document().unwrap().uv()
            )
            .count(),
            1
        );
        let command =
            parse_script(r#"{"cmd":"box_planar_seams","smoothness":0.25,"iterations":12}"#)
                .unwrap()
                .remove(0);
        assert!(matches!(
            session.execute(command).unwrap(),
            Outcome::Info(_)
        ));
        assert_eq!(
            Islands::compute(
                &session.document().unwrap().mesh,
                session.document().unwrap().uv()
            )
            .count(),
            2
        );
        session.execute(Command::Undo).unwrap();
        assert_eq!(
            Islands::compute(
                &session.document().unwrap().mesh,
                session.document().unwrap().uv()
            )
            .count(),
            1
        );
        drop(cleanup);
    }

    #[test]
    fn packing_applies_island_multipliers_and_rejects_invalid_selection() {
        let (mut session, _dir) = session();
        let before = session.document().unwrap().uv().clone();
        let invalid = parse_script(
            r#"{"cmd":"pack","islands":[0],"island_multipliers":[{"island":1,"scale":2.0}]}"#,
        )
        .unwrap()
        .remove(0);
        assert!(matches!(
            session.execute(invalid),
            Err(CommandError::InvalidArgument(_))
        ));
        assert_eq!(session.document().unwrap().uv(), &before);
        let cmd = parse_script(r#"{"cmd":"pack","island_multipliers":[{"island":0,"scale":2.0}],"region":[0,0,4,4],"padding_px":0,"margin_px":0,"rescale":false,"around_others":false}"#).unwrap().remove(0);
        session.execute(cmd).unwrap();
        let doc = session.document().unwrap();
        let islands = Islands::compute(&doc.mesh, doc.uv());
        let (first, _) = pack::island_areas(&doc.mesh, doc.uv(), &islands, 0);
        let (second, _) = pack::island_areas(&doc.mesh, doc.uv(), &islands, 1);
        assert!((first / second - 4.0).abs() < 1e-8);
    }

    #[test]
    fn grayscale_density_map_scales_islands_and_undoes() {
        let (mut session, dir) = session();
        let map_path = dir.0.join("targets.pgm");
        std::fs::write(&map_path, b"P2\n2 1\n255\n0 255\n").unwrap();
        let before = session.document().unwrap().uv().clone();
        let command = Command::ApplyDensityMap {
            path: map_path,
            islands: Vec::new(),
            min_scale: 1.0,
            max_scale: 2.0,
        };
        assert_eq!(session.execute(command).unwrap(), Outcome::Done);
        let doc = session.document().unwrap();
        let islands = Islands::compute(&doc.mesh, doc.uv());
        let (first, _) = pack::island_areas(&doc.mesh, doc.uv(), &islands, 0);
        let (second, _) = pack::island_areas(&doc.mesh, doc.uv(), &islands, 1);
        assert!((second / first - 4.0).abs() < 1e-8);
        session.execute(Command::Undo).unwrap();
        assert_eq!(session.document().unwrap().uv(), &before);
    }

    #[test]
    fn ao_bake_writes_pgm_and_cancel_does_not_publish() {
        let (mut session, dir) = session();
        let path = dir.0.join("ao.pgm");
        let command = Command::BakeAmbientOcclusion {
            path: path.clone(),
            resolution: 4,
            rays: 8,
            max_distance: 1.0,
        };
        let cancel = uvcraft_core::CancelToken::default();
        cancel.cancel();
        let progress = Progress::new(cancel, |_| {});
        assert!(matches!(
            session.execute_with_progress(command.clone(), &progress),
            Err(CommandError::Cancelled(_))
        ));
        assert!(!path.exists());
        assert_eq!(session.execute(command).unwrap(), Outcome::Done);
        let map = DensityMap::parse_pgm(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!((map.width, map.height), (4, 4));
        assert!(map.values.iter().all(|&value| (value - 1.0).abs() < 1e-12));
    }

    #[test]
    fn stretch_cut_command_reduces_distortion_and_undoes() {
        let (mut session, dir) = session();
        let path = dir.0.join("cone.obj");
        std::fs::write(
            &path,
            "v -1 -1 0\nv 1 -1 0\nv 1 1 0\nv -1 1 0\nv 0 0 2\n\
vt -1 -1\nvt 1 -1\nvt 1 1\nvt -1 1\nvt 0 0\n\
f 1/1 2/2 5/5\nf 2/2 3/3 5/5\nf 3/3 4/4 5/5\nf 4/4 1/1 5/5\n",
        )
        .unwrap();
        session.execute(Command::Load { path }).unwrap();
        let before = session.document().unwrap().uv().clone();
        let cmd =
            parse_script(r#"{"cmd":"cut_reduce_stretch","max_cuts":2,"target_distortion":1.01}"#)
                .unwrap()
                .remove(0);
        let outcome = session.execute(cmd).unwrap();
        assert!(matches!(outcome,Outcome::Info(ref text) if text.contains("cut 1 edge")));
        assert_ne!(session.document().unwrap().uv().corner_uv, before.corner_uv);
        session.execute(Command::Undo).unwrap();
        assert_eq!(session.document().unwrap().uv(), &before);
    }

    #[test]
    fn drag_pin_is_one_undoable_edit_and_honors_locks() {
        let (mut session, _dir) = grid_session();
        session
            .execute(Command::Pin {
                vertices: vec![0, 15],
                pinned: true,
            })
            .unwrap();
        let before = session.document().unwrap().uv().clone();
        let depth = session.undo.len();
        session
            .execute(Command::DragPin {
                vertex: 0,
                uv: [0.2, 0.1],
                iterations: 2,
            })
            .unwrap();
        assert_eq!(session.undo.len(), depth + 1);
        let doc = session.document().unwrap();
        let corner = (0..doc.mesh.corner_count())
            .find(|&c| doc.mesh.corner_vertex(c) == 0)
            .unwrap();
        assert!(doc.uv().uv_at_corner(corner).distance(DVec2::new(0.2, 0.1)) < 1e-9);
        session.execute(Command::Undo).unwrap();
        assert_eq!(session.document().unwrap().uv(), &before);
        session
            .execute(Command::LockFaces {
                faces: vec![0],
                locked: true,
            })
            .unwrap();
        assert!(matches!(
            session.execute(Command::DragPin {
                vertex: 0,
                uv: [0.3, 0.2],
                iterations: 2
            }),
            Err(CommandError::InvalidArgument(_))
        ));
    }

    #[test]
    fn overlap_guard_is_available_in_unfold_scripts() {
        let (mut session, _dir) = session();
        let command = parse_script(r#"{"cmd":"unfold","prevent_overlap":true}"#)
            .unwrap()
            .remove(0);
        session.execute(command).unwrap();
        let doc = session.document().unwrap();
        let islands = Islands::compute(&doc.mesh, doc.uv());
        assert!(
            uvcraft_algo::checks::find_overlaps(&doc.mesh, doc.uv(), &islands)
                .face_pairs
                .is_empty()
        );
    }
}
