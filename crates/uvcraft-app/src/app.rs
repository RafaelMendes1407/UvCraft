//! The UvCraft window: tool panel, 3D view, UV view, status bar.
//! Every edit goes through `uvcraft_commands::Session`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Color32, Key, Modifiers, PointerButton, Pos2, Rect, Sense, Vec2 as EVec2,
};
use eframe::egui_wgpu::RenderState;
use glam::{DVec2, Vec2, Vec3};
use serde::{Deserialize, Serialize};
use uvcraft_algo::brush::BrushMode;
use uvcraft_algo::density_brush;
use uvcraft_algo::layout::UvAxis;
use uvcraft_algo::optimize::OptimizeMethod;
use uvcraft_algo::pack::PackMethod;
use uvcraft_algo::physical::Unit;
use uvcraft_algo::projection::{Axis, Projection};
use uvcraft_algo::seams;
use uvcraft_algo::select::{self, Elem, IslandMetric, IslandProblem};
use uvcraft_algo::stack::{self, StackMode};
use uvcraft_algo::unfold::UnfoldMethod;
use uvcraft_bridge::{BridgeHandle, PublishError};
use uvcraft_commands::island_groups::{GroupBy, IslandGroups, TileDistribution};
use uvcraft_commands::{Command, CommandError, Outcome, PhysicalReport, Pivot, Session};
use uvcraft_core::{
    CancelToken, EdgeKind, Islands, Mesh as CoreMesh, Progress, ProgressEvent, UvLayer,
};

use crate::blender_addon;
use crate::camera::{OrbitCamera, UvCamera};
use crate::gizmo::{self, Handle as GizmoHandle};
use crate::preferences::{
    Binding, EXTRA_ACTIONS, HOTKEY_ACTIONS, Hotkeys, MouseBinding, MouseButton, Preferences,
    WindowLayout,
};
use crate::theme;

/// Convert an unordered edge selection into one open path for the layout commands.
fn ordered_edge_path(mesh: &CoreMesh, edges: &[usize]) -> Result<Vec<usize>, String> {
    let mut adjacency: BTreeMap<usize, Vec<(usize, usize)>> = BTreeMap::new();
    let mut unique = BTreeSet::new();
    for &edge in edges {
        if edge >= mesh.edge_count() || !unique.insert(edge) {
            return Err("The edge selection contains an invalid or repeated edge".into());
        }
        let [a, b] = mesh.edge_vertices(edge);
        adjacency.entry(a).or_default().push((edge, b));
        adjacency.entry(b).or_default().push((edge, a));
    }
    if adjacency.values().any(|neighbors| neighbors.len() > 2) {
        return Err("Selected edges branch; select one unbranched path".into());
    }
    let endpoints: Vec<_> = adjacency
        .iter()
        .filter_map(|(&vertex, neighbors)| (neighbors.len() == 1).then_some(vertex))
        .collect();
    if endpoints.len() != 2 {
        return Err(if endpoints.is_empty() {
            "Selected edges form a loop; select one open path".into()
        } else {
            "Selected edges are disconnected; select one continuous path".into()
        });
    }
    let mut ordered = Vec::with_capacity(edges.len());
    let mut visited = BTreeSet::new();
    let mut vertex = endpoints[0];
    while let Some(&(edge, next)) = adjacency[&vertex]
        .iter()
        .find(|(edge, _)| !visited.contains(edge))
    {
        visited.insert(edge);
        ordered.push(edge);
        vertex = next;
    }
    if ordered.len() != edges.len() {
        return Err("Selected edges are disconnected; select one continuous path".into());
    }
    Ok(ordered)
}

fn context_seam_edges(
    mesh: &CoreMesh,
    uv: &UvLayer,
    mode: SelectMode,
    selected_edges: &[usize],
    selected_faces: &[bool],
) -> Vec<usize> {
    if mode == SelectMode::Edges {
        return selected_edges
            .iter()
            .copied()
            .filter(|&edge| edge < mesh.edge_count() && uv.edge_kind(mesh, edge) == EdgeKind::Seam)
            .collect();
    }
    if !matches!(
        mode,
        SelectMode::Faces | SelectMode::Islands | SelectMode::Objects
    ) {
        return Vec::new();
    }
    let mut edges = BTreeSet::new();
    for (face, &selected) in selected_faces.iter().enumerate() {
        if selected && face < mesh.face_count() {
            for corner in mesh.face_corners(face) {
                let edge = mesh.corner_edge(corner);
                if uv.edge_kind(mesh, edge) == EdgeKind::Seam {
                    edges.insert(edge);
                }
            }
        }
    }
    edges.into_iter().collect()
}

fn mouse_modifiers_match(binding: MouseBinding, modifiers: Modifiers) -> bool {
    modifiers.shift == binding.shift
        && modifiers.alt == binding.alt
        && modifiers.command == binding.command
}

fn mouse_drag_matches(
    response: &egui::Response,
    binding: MouseBinding,
    modifiers: Modifiers,
) -> bool {
    mouse_modifiers_match(binding, modifiers)
        && response.dragged_by(match binding.button {
            MouseButton::Left => PointerButton::Primary,
            MouseButton::Middle => PointerButton::Middle,
            MouseButton::Right => PointerButton::Secondary,
        })
}

fn edit_mouse_binding(ui: &mut egui::Ui, label: &str, binding: &mut MouseBinding) {
    ui.horizontal(|ui| {
        ui.label(label);
        egui::ComboBox::from_id_salt(("mouse", label))
            .selected_text(match binding.button {
                MouseButton::Left => "LMB",
                MouseButton::Middle => "MMB",
                MouseButton::Right => "RMB",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut binding.button, MouseButton::Left, "LMB");
                ui.selectable_value(&mut binding.button, MouseButton::Middle, "MMB");
                ui.selectable_value(&mut binding.button, MouseButton::Right, "RMB");
            });
        ui.checkbox(&mut binding.command, "Ctrl");
        ui.checkbox(&mut binding.shift, "Shift");
        ui.checkbox(&mut binding.alt, "Alt");
    });
    if binding.button == MouseButton::Left && !binding.command && !binding.shift && !binding.alt {
        binding.alt = true;
    }
}

fn edit_optional_mouse_binding(
    ui: &mut egui::Ui,
    label: &str,
    binding: &mut Option<MouseBinding>,
    fallback: MouseBinding,
) {
    let mut enabled = binding.is_some();
    ui.checkbox(&mut enabled, format!("Additional {label}"));
    if enabled {
        let current = binding.get_or_insert(fallback);
        edit_mouse_binding(ui, label, current);
    } else {
        *binding = None;
    }
}

fn unit_label(unit: Unit) -> &'static str {
    match unit {
        Unit::Millimeter => "mm",
        Unit::Centimeter => "cm",
        Unit::Meter => "m",
        Unit::Inch => "in",
    }
}

fn unit_picker(ui: &mut egui::Ui, id: &str, unit: &mut Unit) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(unit_label(*unit))
        .show_ui(ui, |ui| {
            for value in [Unit::Millimeter, Unit::Centimeter, Unit::Meter, Unit::Inch] {
                ui.selectable_value(unit, value, unit_label(value));
            }
        });
}

fn take_binding(ctx: &egui::Context, binding: &Binding) -> bool {
    let Some(key) = Key::from_name(&binding.key) else {
        return false;
    };
    ctx.input_mut(|i| {
        let hit = i.events.iter().position(|event| {
            matches!(event, egui::Event::Key { key: pressed_key, pressed: true, modifiers, .. }
                if *pressed_key == key
                    && modifiers.command == binding.command
                    && modifiers.shift == binding.shift
                    && modifiers.alt == binding.alt)
        });
        if let Some(index) = hit {
            i.events.remove(index);
            true
        } else {
            false
        }
    })
}
use crate::render::{Renderer, UvStreamLayout, ViewParams};
use crate::scene::{
    self, ColorMode, DistortionConfig, DistortionMetric, DistortionReference, Scene,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SelectMode {
    Vertices,
    Edges,
    Faces,
    Islands,
    Objects,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SelectionSet {
    name: String,
    mode: SelectMode,
    mask: Vec<bool>,
    #[serde(default)]
    mesh_signature: u64,
}

impl SelectionSet {
    fn compatible(&self, mesh: &CoreMesh) -> bool {
        self.mesh_signature == mesh_signature(mesh)
            && self.mask.len()
                == match self.mode {
                    SelectMode::Faces | SelectMode::Islands | SelectMode::Objects => {
                        mesh.face_count()
                    }
                    SelectMode::Edges => mesh.edge_count(),
                    SelectMode::Vertices => mesh.vertex_count(),
                }
    }
}

fn mesh_signature(mesh: &CoreMesh) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for value in [mesh.vertex_count(), mesh.face_count(), mesh.corner_count()]
        .into_iter()
        .chain((0..mesh.face_count()).map(|f| mesh.face_corners(f).len()))
        .chain((0..mesh.corner_count()).map(|c| mesh.corner_vertex(c)))
    {
        hash ^= value as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn selection_path_key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn brush_polygon(center: Vec2, radius: f32) -> Vec<Vec2> {
    (0..20)
        .map(|i| {
            let a = i as f32 * std::f32::consts::TAU / 20.0;
            center + Vec2::new(a.cos(), a.sin()) * radius
        })
        .collect()
}

fn brush_hits_face(points: &[Vec2], center: Vec2, radius: f32) -> bool {
    scene::point_in_polygon(center, points)
        || points.iter().any(|p| p.distance(center) <= radius)
        || points.iter().enumerate().any(|(i, &a)| {
            let b = points[(i + 1) % points.len()];
            scene::segment_distance(center.as_dvec2(), a.as_dvec2(), b.as_dvec2()) <= radius as f64
        })
}

fn closest_on_segment(point: Vec2, a: Vec2, b: Vec2) -> Vec2 {
    let ab = b - a;
    let length_sq = ab.length_squared();
    if length_sq <= 1e-12 {
        a
    } else {
        a + ab * ((point - a).dot(ab) / length_sq).clamp(0.0, 1.0)
    }
}

impl SelectMode {
    fn elem(self) -> Elem {
        match self {
            SelectMode::Vertices => Elem::Vertex,
            SelectMode::Edges => Elem::Edge,
            SelectMode::Faces => Elem::Face,
            SelectMode::Islands => Elem::Island,
            SelectMode::Objects => Elem::Face,
        }
    }
}

fn expand_object_faces(face_objects: &[u32], object_count: usize, hits: &[bool]) -> Vec<bool> {
    let mut selected = vec![false; object_count];
    for (f, &object) in face_objects.iter().enumerate() {
        if hits.get(f).copied().unwrap_or(false) {
            selected[object as usize] = true;
        }
    }
    face_objects
        .iter()
        .map(|&object| selected[object as usize])
        .collect()
}

fn angle_selection_seed(mode: SelectMode, selected: &[bool], hidden: &[bool]) -> Option<usize> {
    if !matches!(
        mode,
        SelectMode::Faces | SelectMode::Islands | SelectMode::Objects
    ) {
        return None;
    }
    selected
        .iter()
        .enumerate()
        .find_map(|(face, &on)| (on && !hidden.get(face).copied().unwrap_or(false)).then_some(face))
}

/// Select the island under a direct UV drag. Dragging an already selected
/// island keeps the whole selection; Shift extends it with a new island.
fn select_drag_island(islands: &Islands, selected: &mut [bool], face: usize, add: bool) -> bool {
    let before = selected.to_vec();
    if !selected[face] && !add {
        selected.fill(false);
    }
    for &member in islands.faces(islands.island_of_face(face)) {
        selected[member as usize] = true;
    }
    selected != before
}

/// A group's container is a single pick target, including the empty space
/// between its islands. The smaller box wins when containers overlap.
fn pick_group_container(bounds: &[(u64, Rect)], pointer: Vec2) -> Option<u64> {
    bounds
        .iter()
        .filter(|(_, bounds)| bounds.contains(Pos2::new(pointer.x, pointer.y)))
        .min_by(|a, b| {
            let area = |rect: Rect| rect.width() * rect.height();
            area(a.1).total_cmp(&area(b.1))
        })
        .map(|(group, _)| *group)
}

fn next_group_name(groups: &IslandGroups) -> String {
    for number in 1..=groups.group_count() + 1 {
        let name = format!("Group {number}");
        if !groups.nodes().any(|group| group.name == name) {
            return name;
        }
    }
    unreachable!("a group name must be available within group_count + 1 candidates")
}

fn packing_groups_for_roots(
    groups: &IslandGroups,
    islands: &Islands,
    selected: &[usize],
    locked_faces: &[bool],
    locked_island_faces: &[bool],
) -> Vec<Vec<usize>> {
    let mut by_root: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for island in 0..islands.count() {
        let Some(&first_face) = islands.faces(island).first() else {
            continue;
        };
        let Some(root) = groups.root_of_face(first_face as usize) else {
            continue;
        };
        by_root.entry(root.id).or_default().push(island);
    }
    by_root
        .into_values()
        .filter(|members| {
            members.len() >= 2
                && members.iter().all(|&island| {
                    (selected.is_empty() || selected.contains(&island))
                        && islands.faces(island).iter().all(|&face| {
                            let face = face as usize;
                            !locked_faces[face]
                                && !locked_island_faces[face]
                                && !groups.face_locked(face)
                        })
                })
        })
        .collect()
}

pub struct Startup {
    pub file: Option<PathBuf>,
    pub script: Option<PathBuf>,
    pub screenshot: Option<PathBuf>,
}

struct Screenshot {
    path: PathBuf,
    requested: bool,
    done: Arc<AtomicBool>,
}

/// A command running on a worker thread, which owns the session meanwhile.
struct Running {
    cmd: Command,
    rx: Receiver<Finished>,
    progress_rx: Receiver<ProgressEvent>,
    progress: Option<ProgressEvent>,
    cancel: CancelToken,
    started: Instant,
}

#[derive(Clone)]
struct GizmoDrag {
    handle: GizmoHandle,
    start: DVec2,
    current: DVec2,
    center: DVec2,
    islands: Vec<usize>,
}

#[derive(Clone)]
struct GroupThumb {
    island: usize,
    bounds: (DVec2, DVec2),
    edges: Vec<(DVec2, DVec2)>,
}

struct GroupThumbCache {
    group: u64,
    revision: u64,
    thumbs: Vec<GroupThumb>,
}

struct UvEditDrag {
    mode: BrushMode,
    start: Vec2,
    current: Vec2,
    last_sample: Vec2,
    weights: BTreeMap<usize, f64>,
}

struct DensityDrag {
    last_sample: Vec2,
    weights: BTreeMap<usize, f64>,
}

struct PinDrag {
    vertex: usize,
    start: DVec2,
    target: DVec2,
    pointer_offset: DVec2,
    preview: UvLayer,
    targets: Vec<usize>,
    pins: Vec<bool>,
    last_solved: DVec2,
    pending: Option<Receiver<(DVec2, UvLayer)>>,
}

impl PinDrag {
    fn release_command(self) -> Option<Command> {
        (self.target.distance(self.start) > 1e-8).then_some(Command::DragPin {
            vertex: self.vertex,
            uv: self.target.to_array(),
            iterations: 8,
        })
    }
}

struct Finished {
    session: Session,
    result: Result<Outcome, CommandError>,
    scene: Option<Scene>,
}

/// Commands that can take long enough to freeze the window.
fn is_heavy(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Load { .. }
            | Command::Save { .. }
            | Command::Project { .. }
            | Command::AutoSeams { .. }
            | Command::BoxPlanarSeams { .. }
            | Command::Pelt { .. }
            | Command::CutReduceStretch { .. }
            | Command::Unfold { .. }
            | Command::DragPin { .. }
            | Command::Optimize { .. }
            | Command::OptimizeAngleArea { .. }
            | Command::Refine { .. }
            | Command::Rectangularize { .. }
            | Command::Pack { .. }
            | Command::PackPhysical { .. }
            | Command::BakeAmbientOcclusion { .. }
            | Command::ApplyDensityMap { .. }
            | Command::CutUnfold { .. }
            | Command::CutLoop { unfold: true, .. }
            | Command::CutPath { unfold: true, .. }
            | Command::CutBorder { unfold: true, .. }
            | Command::Mosaic { .. }
            | Command::Stack { .. }
            | Command::Similar { .. }
            | Command::Match { .. }
            | Command::LoadUvs { .. }
    )
}

pub struct App {
    session: Session,
    bridge: Option<BridgeHandle>,
    bridge_revision: u64,
    bridge_session_id: Option<String>,
    bridge_publish_pending: bool,
    ctx: egui::Context,
    brand_icon: egui::TextureHandle,
    /// Commands waiting to run, in order.
    jobs: VecDeque<Command>,
    running: Option<Running>,
    scene: Option<Scene>,
    rs: Option<RenderState>,
    renderer: Option<Renderer>,
    uv_stream_layout: Option<UvStreamLayout>,
    last_preview_uv_ids: Vec<u32>,
    gpu_dirty: bool,
    cam3: OrbitCamera,
    camuv: UvCamera,
    show_3d_gizmo: bool,
    frame_pending: bool,
    view_sizes: [Vec2; 2],

    mode: SelectMode,
    mode_3d: SelectMode,
    mode_uv: SelectMode,
    active_view: usize,
    sel_faces: Vec<bool>,
    sel_edges: Vec<bool>,
    sel_verts: Vec<bool>,
    selection_sets: Vec<SelectionSet>,
    selection_sets_by_path: BTreeMap<String, Vec<SelectionSet>>,
    selection_set_name: String,
    selection_material: String,
    selection_smoothing_group: u32,
    group_name: String,
    group_rename: String,
    group_edit: Option<u64>,
    group_offset: [f64; 2],
    group_rotation: f64,
    group_scale: f64,
    group_selection: Option<u64>,
    group_workspace_open: bool,
    group_workspace_height: f32,
    group_workspace_revision: u64,
    group_thumb_cache: Option<GroupThumbCache>,
    /// Hidden faces: not drawn, not pickable.
    hidden: Vec<bool>,
    manual_hidden: Vec<bool>,
    last_vertex: Option<usize>,
    /// Last clicked edge (double-click selects its loop).
    last_edge: Option<usize>,
    /// Drag selection in progress: (view, path in view-local points).
    drag_path: Option<(usize, Vec<Vec2>)>,
    /// Drag selects with a lasso instead of a box.
    lasso: bool,
    brush: bool,
    brush_erase: bool,
    brush_radius: f32,
    brush_last: Option<(usize, Vec2)>,
    uv_edit_mode: Option<BrushMode>,
    uv_edit_strength: f64,
    uv_edit_drag: Option<UvEditDrag>,
    density_brush: bool,
    density_realtime: bool,
    density_strength: f64,
    density_drag: Option<DensityDrag>,
    pin_drag: Option<PinDrag>,
    seam_brush: bool,
    seam_brush_edges: Vec<bool>,
    seam_brush_last: Option<Vec2>,
    path_preview_edges: Vec<bool>,
    path_preview_key: Option<(usize, usize)>,
    path_preview_pointer: Option<Vec2>,
    path_preview_checked: Instant,
    gizmo_enabled: bool,
    gizmo_snap: bool,
    gizmo_drag: Option<GizmoDrag>,
    /// Direct drag may also change selection, requiring one full color upload.
    drag_selection_dirty: bool,
    /// 3D drag selection also takes hidden faces and edges.
    xray: bool,

    color_mode: ColorMode,
    distortion_metric: DistortionMetric,
    distortion_reference: DistortionReference,
    distortion_sensitivity: f64,
    distortion_opacity: f32,
    distortion_3d: bool,
    distortion_isolate: bool,
    distortion_active_island: Option<usize>,
    dark_theme: bool,
    show_help: bool,
    show_preferences: bool,
    show_advanced_tools: bool,
    prefs_path: Option<PathBuf>,
    saved_prefs: Preferences,
    hotkeys: Hotkeys,
    wireframe: bool,
    overlay_seams: bool,
    overlay_borders: bool,
    show_outliner: bool,
    tools_width: f32,
    outliner_width: f32,
    viewport_split: f32,
    layout_presets: BTreeMap<String, WindowLayout>,
    layout_name: String,
    checker_cells: f32,
    grid_visible: bool,
    grid_divisions: u32,
    texture_path: Option<PathBuf>,

    seam_angle: f64,
    select_angle: f64,
    metric: IslandMetric,
    metric_min: f64,
    metric_max: f64,
    metric_use_min: bool,
    metric_use_max: bool,
    padding: f64,
    margin: f64,
    pack_method: PackMethod,
    pack_rotate: bool,
    pack_rescale: bool,
    pack_keep_density: bool,
    pack_keep_groups: bool,
    pack_preserve_stacks: bool,
    stack_reference: Option<usize>,
    stack_conform: bool,
    stack_preview_open: bool,
    pack_udim: bool,
    pack_tiles: u32,
    pack_distribution: Option<TileDistribution>,
    pack_pixel_aligned: bool,
    pack_padding_perfect: bool,
    optimize_method: OptimizeMethod,
    border_weight: f64,
    prevent_overlap: bool,
    resolution: u32,
    density: f64,
    density_map_min: f64,
    density_map_max: f64,
    physical_mesh_unit: Unit,
    physical_uv_unit: Unit,
    physical_rect_unit: Unit,
    physical_width: f64,
    physical_height: f64,
    physical_padding: f64,
    physical_margin: f64,
    physical_report: Option<PhysicalReport>,
    new_uv_channel: u32,
    swap_uv_channel: u32,
    reference_tab: u8,
    reference_unfold_method: UnfoldMethod,
    reference_auto_seams: bool,
    reference_preserve_orientation: bool,
    reference_position: [f64; 2],
    reference_scale: [f64; 2],
    reference_rotation: f64,

    status: String,
    status_error: bool,
    screenshot: Option<Screenshot>,
    frames: u64,
}

impl App {
    fn current_layout(&self) -> WindowLayout {
        WindowLayout {
            tools_width: self.tools_width,
            outliner_width: self.outliner_width,
            viewport_split: self.viewport_split,
            show_outliner: self.show_outliner,
        }
    }

    fn apply_layout(&mut self, layout: &WindowLayout) {
        self.tools_width = layout.tools_width;
        self.outliner_width = layout.outliner_width;
        self.viewport_split = layout.viewport_split;
        self.show_outliner = layout.show_outliner;
    }

    fn preferences(&self) -> Preferences {
        Preferences {
            dark_theme: self.dark_theme,
            wireframe: self.wireframe,
            overlay_seams: self.overlay_seams,
            overlay_borders: self.overlay_borders,
            show_outliner: self.show_outliner,
            tools_width: self.tools_width,
            outliner_width: self.outliner_width,
            viewport_split: self.viewport_split,
            brush_radius: self.brush_radius,
            layout_presets: self.layout_presets.clone(),
            checker_cells: self.checker_cells,
            grid_visible: self.grid_visible,
            grid_divisions: self.grid_divisions,
            texture_path: self.texture_path.clone(),
            resolution: self.resolution,
            density: self.density,
            padding: self.padding,
            margin: self.margin,
            seam_angle: self.seam_angle,
            select_angle: self.select_angle,
            pack_rotate: self.pack_rotate,
            pack_rescale: self.pack_rescale,
            pack_keep_density: self.pack_keep_density,
            pack_keep_groups: self.pack_keep_groups,
            pack_preserve_stacks: self.pack_preserve_stacks,
            pack_pixel_aligned: self.pack_pixel_aligned,
            pack_padding_perfect: self.pack_padding_perfect,
            hotkeys: self.hotkeys.clone(),
            selection_sets: self.selection_sets_by_path.clone(),
        }
    }

    fn apply_preferences(&mut self, prefs: &Preferences) {
        self.dark_theme = prefs.dark_theme;
        self.wireframe = prefs.wireframe;
        self.overlay_seams = prefs.overlay_seams;
        self.overlay_borders = prefs.overlay_borders;
        self.show_outliner = prefs.show_outliner;
        self.tools_width = prefs.tools_width;
        self.outliner_width = prefs.outliner_width;
        self.viewport_split = prefs.viewport_split;
        self.brush_radius = prefs.brush_radius;
        self.layout_presets = prefs.layout_presets.clone();
        self.checker_cells = prefs.checker_cells;
        self.grid_visible = prefs.grid_visible;
        self.grid_divisions = prefs.grid_divisions;
        self.texture_path = prefs.texture_path.clone();
        if self.texture_path.is_none() && self.color_mode == ColorMode::Texture {
            self.color_mode = ColorMode::Checker;
        }
        self.resolution = prefs.resolution;
        self.density = prefs.density;
        self.padding = prefs.padding;
        self.margin = prefs.margin;
        self.seam_angle = prefs.seam_angle;
        self.select_angle = prefs.select_angle;
        self.pack_rotate = prefs.pack_rotate;
        self.pack_rescale = prefs.pack_rescale;
        self.pack_keep_density = prefs.pack_keep_density;
        self.pack_keep_groups = prefs.pack_keep_groups;
        self.pack_preserve_stacks = prefs.pack_preserve_stacks;
        self.pack_pixel_aligned = prefs.pack_pixel_aligned;
        self.pack_padding_perfect = prefs.pack_padding_perfect;
        self.hotkeys = prefs.hotkeys.clone();
        self.selection_sets_by_path = prefs.selection_sets.clone();
        self.selection_sets.clear();
        theme::apply(&self.ctx, prefs.dark_theme);
        self.gpu_dirty = true;
    }

    pub fn new(cc: &eframe::CreationContext<'_>, startup: Startup) -> Self {
        let rs = cc.wgpu_render_state.clone();
        let renderer = rs.as_ref().map(Renderer::new);
        let brand_png = image::load_from_memory(include_bytes!("../../../icon.png"))
            .expect("bundled UvCraft icon must be a PNG")
            .resize_exact(64, 64, image::imageops::FilterType::Lanczos3)
            .to_rgba8();
        let brand_icon = cc.egui_ctx.load_texture(
            "uvcraft-brand-icon",
            egui::ColorImage::from_rgba_unmultiplied([64, 64], brand_png.as_raw()),
            egui::TextureOptions::LINEAR,
        );
        let prefs_path = Preferences::path();
        let mut prefs = prefs_path
            .as_deref()
            .and_then(|path| Preferences::load(path).ok())
            .unwrap_or_default();
        // Move the former default three-column layout to the reference layout.
        if (prefs.tools_width - 270.0).abs() < f32::EPSILON
            && (prefs.outliner_width - 210.0).abs() < f32::EPSILON
            && (prefs.viewport_split - 0.5).abs() < f32::EPSILON
            && prefs.show_outliner
        {
            prefs.tools_width = 350.0;
            prefs.show_outliner = false;
        }
        theme::apply(&cc.egui_ctx, prefs.dark_theme);
        let mut app = App {
            session: Session::new(),
            bridge: None,
            bridge_revision: 0,
            bridge_session_id: None,
            bridge_publish_pending: false,
            ctx: cc.egui_ctx.clone(),
            brand_icon,
            jobs: VecDeque::new(),
            running: None,
            scene: None,
            rs,
            renderer,
            uv_stream_layout: None,
            last_preview_uv_ids: Vec::new(),
            gpu_dirty: true,
            cam3: OrbitCamera::default(),
            camuv: UvCamera::default(),
            show_3d_gizmo: true,
            frame_pending: true,
            view_sizes: [Vec2::ONE; 2],
            mode: SelectMode::Islands,
            mode_3d: SelectMode::Faces,
            mode_uv: SelectMode::Islands,
            active_view: 1,
            sel_faces: Vec::new(),
            sel_edges: Vec::new(),
            sel_verts: Vec::new(),
            selection_sets: Vec::new(),
            selection_sets_by_path: prefs.selection_sets.clone(),
            selection_set_name: "Set 1".into(),
            selection_material: String::new(),
            selection_smoothing_group: 1,
            group_name: "Group 1".into(),
            group_rename: String::new(),
            group_edit: None,
            group_offset: [0.0; 2],
            group_rotation: 0.0,
            group_scale: 1.0,
            group_selection: None,
            group_workspace_open: false,
            group_workspace_height: 160.0,
            group_workspace_revision: 0,
            group_thumb_cache: None,
            hidden: Vec::new(),
            manual_hidden: Vec::new(),
            last_vertex: None,
            last_edge: None,
            color_mode: if prefs.texture_path.is_some() {
                ColorMode::Texture
            } else {
                ColorMode::Checker
            },
            distortion_metric: DistortionMetric::Area,
            distortion_reference: DistortionReference::Object,
            distortion_sensitivity: 1.0,
            distortion_opacity: 0.85,
            distortion_3d: true,
            distortion_isolate: false,
            distortion_active_island: None,
            dark_theme: prefs.dark_theme,
            show_help: false,
            show_preferences: false,
            show_advanced_tools: false,
            prefs_path,
            saved_prefs: prefs.clone(),
            hotkeys: prefs.hotkeys.clone(),
            wireframe: prefs.wireframe,
            overlay_seams: prefs.overlay_seams,
            overlay_borders: prefs.overlay_borders,
            show_outliner: prefs.show_outliner,
            tools_width: prefs.tools_width,
            outliner_width: prefs.outliner_width,
            viewport_split: prefs.viewport_split,
            layout_presets: prefs.layout_presets.clone(),
            layout_name: "My layout".into(),
            checker_cells: prefs.checker_cells,
            grid_visible: prefs.grid_visible,
            grid_divisions: prefs.grid_divisions,
            texture_path: prefs.texture_path.clone(),
            seam_angle: prefs.seam_angle,
            select_angle: prefs.select_angle,
            metric: IslandMetric::MaxAngleDistortion,
            metric_min: 1.1,
            metric_max: 2.0,
            metric_use_min: true,
            metric_use_max: false,
            padding: prefs.padding,
            margin: prefs.margin,
            pack_method: PackMethod::Raster,
            pack_rotate: prefs.pack_rotate,
            pack_rescale: prefs.pack_rescale,
            pack_keep_density: prefs.pack_keep_density,
            pack_keep_groups: prefs.pack_keep_groups,
            pack_preserve_stacks: prefs.pack_preserve_stacks,
            stack_reference: None,
            stack_conform: false,
            stack_preview_open: false,
            pack_udim: false,
            pack_tiles: 10,
            pack_distribution: None,
            pack_pixel_aligned: prefs.pack_pixel_aligned,
            pack_padding_perfect: prefs.pack_padding_perfect,
            optimize_method: OptimizeMethod::Slim,
            border_weight: 0.0,
            prevent_overlap: false,
            resolution: prefs.resolution,
            density: prefs.density,
            density_map_min: 0.5,
            density_map_max: 2.0,
            physical_mesh_unit: Unit::Meter,
            physical_uv_unit: Unit::Millimeter,
            physical_rect_unit: Unit::Millimeter,
            physical_width: 1000.0,
            physical_height: 1000.0,
            physical_padding: 5.0,
            physical_margin: 10.0,
            physical_report: None,
            new_uv_channel: 1,
            swap_uv_channel: 0,
            reference_tab: 0,
            reference_unfold_method: UnfoldMethod::Abf,
            reference_auto_seams: false,
            reference_preserve_orientation: false,
            reference_position: [0.0; 2],
            reference_scale: [1.0; 2],
            reference_rotation: 0.0,
            drag_path: None,
            lasso: false,
            brush: false,
            brush_erase: false,
            brush_radius: prefs.brush_radius,
            brush_last: None,
            uv_edit_mode: None,
            uv_edit_strength: 0.5,
            uv_edit_drag: None,
            density_brush: false,
            density_realtime: true,
            density_strength: 0.5,
            density_drag: None,
            pin_drag: None,
            seam_brush: false,
            seam_brush_edges: Vec::new(),
            seam_brush_last: None,
            path_preview_edges: Vec::new(),
            path_preview_key: None,
            path_preview_pointer: None,
            path_preview_checked: Instant::now(),
            gizmo_enabled: true,
            gizmo_snap: true,
            gizmo_drag: None,
            drag_selection_dirty: false,
            xray: false,
            status: format!(
                "Open a mesh (File ▸ Open, {}): .obj, .gltf or .glb",
                prefs.hotkeys.extras[11].display()
            ),
            status_error: false,
            screenshot: startup.screenshot.map(|path| Screenshot {
                path,
                requested: false,
                done: Arc::new(AtomicBool::new(false)),
            }),
            frames: 0,
        };
        if let (Some(rs), Some(renderer), Some(path)) =
            (&app.rs, &mut app.renderer, &app.texture_path)
            && let Err(e) = renderer.set_custom_texture(rs, path)
        {
            app.color_mode = ColorMode::Checker;
            app.set_error(format!("texture {}: {e}", path.display()));
            app.texture_path = None;
        }
        if app.rs.is_none() {
            app.set_error("wgpu is not available: viewports are disabled".into());
        }
        if let Some(file) = startup.file {
            app.open(&file);
        }
        if let Some(script) = startup.script {
            app.run_script(&script);
        }
        let repaint = app.ctx.clone();
        let bind = std::env::var("UVCRAFT_BRIDGE_ADDR").unwrap_or_else(|_| "127.0.0.1:8766".into());
        match uvcraft_bridge::start_desktop(&bind, move || repaint.request_repaint()) {
            Ok(bridge) => {
                eprintln!("UvCraft desktop bridge listening on {}", bridge.address());
                eprintln!("UvCraft desktop bridge token: {}", bridge.token());
                app.bridge = Some(bridge);
            }
            Err(error) => app.set_error(format!("Blender bridge: {error}")),
        }
        app
    }

    // ------------------------------------------------------------ commands

    fn set_error(&mut self, msg: String) {
        self.status = msg;
        self.status_error = true;
    }

    fn set_status(&mut self, msg: String) {
        self.status = msg;
        self.status_error = false;
    }

    fn install_blender_addon(&mut self, addons_dir: &Path) {
        match blender_addon::install_into(addons_dir) {
            Ok(destination) => self.set_status(format!(
                "Blender add-on installed at {}. Restart Blender and enable UvCraft UV Bridge in Add-ons.",
                destination.display())),
            Err(error) => self.set_error(format!(
                "Could not install Blender add-on in {}: {error}", addons_dir.display())),
        }
    }

    /// Queues a command. Light ones run right away when nothing is pending;
    /// heavy ones run on a worker thread (see [`App::pump`]).
    fn execute(&mut self, cmd: Command) {
        let cmd = if let Some(group) = self.group_selection {
            match cmd {
                Command::AverageIslandScale { resolution, .. } => {
                    Command::AverageIslandGroupScale { group, resolution }
                }
                Command::SetTexelDensity {
                    density,
                    resolution,
                    ..
                } => Command::SetIslandGroupTexelDensity {
                    group,
                    density,
                    resolution,
                },
                Command::Transform {
                    translate,
                    rotate_deg,
                    scale,
                    ..
                } => {
                    if (scale[0] - scale[1]).abs() > 1e-9 || scale[0] <= 0.0 {
                        self.set_error("Group transforms require a positive uniform scale".into());
                        return;
                    }
                    Command::TransformIslandGroup {
                        group,
                        translate,
                        rotate_deg,
                        scale: scale[0],
                    }
                }
                Command::Mirror { .. } => {
                    self.set_error("Select an individual island to mirror it".into());
                    return;
                }
                Command::Optimize { .. }
                | Command::StraightenIsland { .. }
                | Command::StraightenPath { .. }
                | Command::AlignBorder { .. } => {
                    self.set_error("Select an individual island for this tool".into());
                    return;
                }
                other => other,
            }
        } else {
            cmd
        };
        self.jobs.push_back(cmd);
        self.pump();
    }

    fn busy(&self) -> bool {
        self.running.is_some() || !self.jobs.is_empty()
    }

    fn mode_for_view(&self, view: usize) -> SelectMode {
        if view == 0 {
            self.mode_3d
        } else {
            self.mode_uv
        }
    }

    fn set_view_mode(&mut self, view: usize, mode: SelectMode) {
        if view == 0 {
            self.mode_3d = mode;
        } else {
            self.mode_uv = mode;
        }
        self.active_view = view;
        self.mode = mode;
        self.last_edge = None;
        self.last_vertex = None;
        self.gpu_dirty = true;
    }

    fn sync_active_view_mode(&mut self) {
        if self.mode != self.mode_for_view(self.active_view) {
            if self.active_view == 0 {
                self.mode_3d = self.mode;
            } else {
                self.mode_uv = self.mode;
            }
            self.gpu_dirty = true;
        }
    }

    fn activate_view(&mut self, view: usize) {
        self.active_view = view;
        self.mode = self.mode_for_view(view);
    }

    fn try_publish_bridge(&mut self) {
        let (Some(bridge), Some(id)) = (&self.bridge, &self.bridge_session_id) else {
            self.bridge_publish_pending = false;
            return;
        };
        match bridge.publish(id, self.bridge_revision, &self.session) {
            Ok(revision) => {
                self.bridge_revision = revision;
                self.bridge_publish_pending = false;
            }
            Err(PublishError::Busy) => {
                self.bridge_publish_pending = true;
                self.ctx.request_repaint_after(Duration::from_millis(30));
            }
            Err(PublishError::Stale) => {
                self.bridge_publish_pending = false;
                self.set_error(
                    "Blender bridge: remote session changed; showing the latest version".into(),
                );
            }
        }
    }

    /// Adopt completed HTTP work on the UI thread so scene and GPU state stay local.
    fn poll_bridge(&mut self) {
        if self.busy() {
            return;
        }
        if self.bridge_publish_pending {
            self.try_publish_bridge();
            if self.bridge_publish_pending {
                return;
            }
        }
        let Some(bridge) = &self.bridge else {
            return;
        };
        let revision = bridge.revision();
        if revision == self.bridge_revision {
            return;
        }
        let snapshot = match bridge.snapshot_after(self.bridge_revision) {
            Ok(snapshot) => snapshot,
            Err(_) => {
                self.ctx.request_repaint_after(Duration::from_millis(30));
                return;
            }
        };
        self.bridge_revision = revision;
        if let Some(snapshot) = snapshot {
            let previous_group = self.group_edit;
            let previous_group_selection = self.group_selection;
            let previous_workspace_open = self.group_workspace_open;
            let same_bridge_session =
                self.bridge_session_id.as_deref() == Some(snapshot.session_id.as_str());
            let path = snapshot
                .session
                .document()
                .expect("bridge snapshot loaded")
                .path
                .clone();
            self.bridge_session_id = None;
            self.bridge_publish_pending = false;
            self.session = snapshot.session;
            let scene = self.session.document().map(|doc| {
                Scene::new_with_measurements_and_stacks(
                    &doc.mesh,
                    doc.measurement_mesh(),
                    doc.uv(),
                    self.resolution,
                    &doc.stack_groups(),
                )
            });
            self.finish(
                Command::Load { path },
                Ok(Outcome::Info("received from Blender".into())),
                scene,
                Duration::ZERO,
            );
            if same_bridge_session
                && let Some(group) = previous_group
                && self
                    .session
                    .document()
                    .is_some_and(|doc| doc.island_groups().nodes().any(|node| node.id == group))
            {
                self.group_edit = Some(group);
                self.group_selection = previous_group_selection;
                self.group_workspace_open = previous_workspace_open;
            }
            self.bridge_session_id = Some(snapshot.session_id);
            self.bridge_revision = snapshot.revision;
            self.set_status(format!("Blender object: {}", snapshot.object_id));
        } else if self.bridge_session_id.take().is_some() {
            self.set_status("Blender disconnected; document remains open".into());
        }
    }

    /// Collects a finished worker, then starts queued commands until one
    /// has to run in the background.
    fn pump(&mut self) {
        if let Some(running) = &mut self.running {
            for event in running.progress_rx.try_iter() {
                running.progress = Some(event);
            }
            match running.rx.try_recv() {
                Ok(done) => {
                    let running = self.running.take().expect("checked");
                    self.session = done.session;
                    self.finish(
                        running.cmd,
                        done.result,
                        done.scene,
                        running.started.elapsed(),
                    );
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    // Unreachable in practice: the worker catches panics.
                    self.running = None;
                    self.jobs.clear();
                    self.scene = None;
                    self.set_error("internal error: the document was lost".into());
                    return;
                }
            }
        }
        let resolution = self.resolution;
        while let Some(cmd) = self.jobs.pop_front() {
            let wants_scene = !matches!(cmd, Command::Save { .. } | Command::Analyze { .. });
            if is_heavy(&cmd) {
                let mut session = std::mem::take(&mut self.session);
                let (tx, rx) = mpsc::channel();
                let (progress_tx, progress_rx) = mpsc::channel();
                let cancel = CancelToken::default();
                let progress = Progress::new(cancel.clone(), move |event| {
                    let _ = progress_tx.send(event);
                });
                let (job, ctx) = (cmd.clone(), self.ctx.clone());
                std::thread::spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        session.execute_with_progress(job, &progress)
                    }))
                    .unwrap_or_else(|panic| {
                        let msg = panic
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_default();
                        Err(CommandError::InvalidArgument(format!(
                            "internal error: {msg}"
                        )))
                    });
                    let scene = (result.is_ok() && wants_scene)
                        .then(|| {
                            session.document().map(|d| {
                                Scene::new_with_measurements_and_stacks(
                                    &d.mesh,
                                    d.measurement_mesh(),
                                    d.uv(),
                                    resolution,
                                    &d.stack_groups(),
                                )
                            })
                        })
                        .flatten();
                    let _ = tx.send(Finished {
                        session,
                        result,
                        scene,
                    });
                    ctx.request_repaint();
                });
                self.running = Some(Running {
                    cmd,
                    rx,
                    progress_rx,
                    progress: None,
                    cancel,
                    started: Instant::now(),
                });
                return;
            }
            let result = self.session.execute(cmd.clone());
            let scene = (result.is_ok() && wants_scene)
                .then(|| {
                    self.session.document().map(|d| {
                        Scene::new_with_measurements_and_stacks(
                            &d.mesh,
                            d.measurement_mesh(),
                            d.uv(),
                            resolution,
                            &d.stack_groups(),
                        )
                    })
                })
                .flatten();
            self.finish(cmd, result, scene, Duration::ZERO);
        }
    }

    fn finish(
        &mut self,
        cmd: Command,
        result: Result<Outcome, CommandError>,
        scene: Option<Scene>,
        took: Duration,
    ) {
        let publish_to_bridge = result.is_ok() && !matches!(cmd, Command::Load { .. });
        if result.is_ok() {
            self.group_workspace_revision = self.group_workspace_revision.wrapping_add(1);
            self.group_thumb_cache = None;
        }
        if matches!(cmd, Command::Load { .. }) {
            self.group_edit = None;
            self.group_selection = None;
            self.group_workspace_open = false;
        }
        let name = cmd.name();
        let time = if took >= Duration::from_millis(200) {
            format!(" ({:.1} s)", took.as_secs_f64())
        } else {
            String::new()
        };
        let selected_islands = match &result {
            Ok(Outcome::Selection { islands }) => Some(islands.clone()),
            _ => None,
        };
        let selected_faces = match &result {
            Ok(Outcome::FaceSelection { faces }) => Some(faces.clone()),
            _ => None,
        };
        if let Ok(outcome) = &result {
            self.physical_report = match outcome {
                Outcome::PhysicalReport(report) => Some((**report).clone()),
                _ => None,
            };
        }
        match result {
            Ok(Outcome::Info(msg)) => self.set_status(format!("{name}: {msg}{time}")),
            Ok(Outcome::Selection { ref islands }) => {
                self.set_status(format!("{name}: {} islands selected{time}", islands.len()))
            }
            Ok(Outcome::FaceSelection { ref faces }) => {
                self.set_status(format!("{name}: {} faces selected{time}", faces.len()))
            }
            Ok(Outcome::PhysicalReport(report)) => self.set_status(format!(
                "{name}: {} edge lengths, {} island areas in {:?}{time}",
                report.edge_lengths.len(),
                report.island_areas.len(),
                report.output_unit,
            )),
            Ok(_) => self.set_status(format!("{name}: done{time}")),
            Err(CommandError::Cancelled(_)) => {
                self.set_status(format!("{name}: cancelled{time}"));
                self.jobs.clear();
                return;
            }
            Err(e) => {
                self.set_error(format!("{name}: {e}"));
                self.jobs.clear(); // a failing script stops here
                return;
            }
        }
        if let Some(mut scene) = scene {
            if !matches!(cmd, Command::Load { .. })
                && self.hidden.iter().any(|&h| h)
                && let Some(doc) = self.session.document()
            {
                scene.set_hidden(&doc.mesh, &self.hidden);
            }
            self.scene = Some(scene);
            self.gpu_dirty = true;
        }
        if let (Some(islands), Some(scene)) = (selected_islands, &self.scene) {
            self.sel_faces.fill(false);
            for island in islands {
                for &face in scene.islands.faces(island) {
                    if !self.is_hidden(face as usize) {
                        self.sel_faces[face as usize] = true;
                    }
                }
            }
            self.mode = SelectMode::Islands;
            self.gpu_dirty = true;
        }
        if let Some(faces) = selected_faces {
            self.sel_faces.fill(false);
            for face in faces {
                if face < self.sel_faces.len() && !self.is_hidden(face) {
                    self.sel_faces[face] = true;
                }
            }
            self.mode = SelectMode::Faces;
            self.gpu_dirty = true;
        }
        match cmd {
            Command::Load { path } => {
                self.bridge_session_id = None;
                self.bridge_publish_pending = false;
                let doc = self.session.document().expect("loaded");
                self.sel_faces = vec![false; doc.mesh.face_count()];
                self.sel_edges = vec![false; doc.mesh.edge_count()];
                self.seam_brush_edges = vec![false; doc.mesh.edge_count()];
                self.seam_brush_last = None;
                self.path_preview_edges = vec![false; doc.mesh.edge_count()];
                self.path_preview_key = None;
                self.path_preview_pointer = None;
                self.sel_verts = vec![false; doc.mesh.vertex_count()];
                let key = selection_path_key(&doc.path);
                self.selection_sets = self
                    .selection_sets_by_path
                    .get(&key)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|set| set.compatible(&doc.mesh))
                    .collect();
                self.selection_set_name = format!("Set {}", self.selection_sets.len() + 1);
                self.hidden = vec![false; doc.mesh.face_count()];
                self.manual_hidden = vec![false; doc.mesh.face_count()];
                self.group_edit = None;
                self.last_vertex = None;
                self.last_edge = None;
                self.frame_pending = true;
                if let Some(r) = self.scene.as_ref().map(|s| &s.report) {
                    let mut msg = format!(
                        "Loaded {} ({} faces, {} islands){time}",
                        path.display(),
                        r.face_count,
                        r.island_count
                    );
                    if doc.missing_uv_corners > 0 {
                        msg.push_str(" — no UVs in file: try Auto seams + Unfold + Pack");
                    }
                    self.set_status(msg);
                }
            }
            Command::Save { path } => self.set_status(format!("Saved {}{time}", path.display())),
            Command::ExportFlatPattern { path, .. } => {
                self.set_status(format!("Exported flat pattern {}{time}", path.display()))
            }
            Command::CreateIslandGroup { .. } => {
                if let Some(node) = self
                    .session
                    .document()
                    .and_then(|doc| doc.island_groups().nodes().max_by_key(|node| node.id))
                {
                    self.group_edit = Some(node.id);
                    self.group_selection = Some(node.id);
                    self.group_rename = node.name.clone();
                    self.group_workspace_open = true;
                }
            }
            Command::UngroupIslands { group } | Command::DeleteIslandGroup { group } => {
                if self.group_edit == Some(group) {
                    self.group_edit = None;
                }
                if self.group_selection == Some(group) {
                    self.group_selection = None;
                }
            }
            _ => {}
        }
        self.refresh_hidden();
        if publish_to_bridge && self.bridge_session_id.is_some() {
            self.bridge_publish_pending = true;
            self.try_publish_bridge();
        }
    }

    fn open(&mut self, path: &Path) {
        self.bridge_session_id = None;
        self.bridge_publish_pending = false;
        self.execute(Command::Load {
            path: path.to_path_buf(),
        });
    }

    fn run_script(&mut self, path: &Path) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => return self.set_error(format!("{}: {e}", path.display())),
        };
        match uvcraft_commands::parse_script(&text) {
            Ok(cmds) => {
                self.jobs.extend(cmds);
                self.pump();
            }
            Err(e) => self.set_error(format!("{}: {e}", path.display())),
        }
    }

    fn save(&mut self, path: Option<PathBuf>) {
        let Some(doc) = self.session.document() else {
            return;
        };
        let path = match path {
            Some(p) => p,
            None => doc.path.clone(),
        };
        self.execute(Command::Save { path });
    }

    fn open_dialog(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("Meshes", &["obj", "gltf", "glb"])
            .pick_file()
        {
            self.open(&p);
        }
    }

    fn save_as_dialog(&mut self) {
        let Some(doc) = self.session.document() else {
            return;
        };
        let ext = doc
            .path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("obj")
            .to_string();
        let name = doc
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // glTF can be saved as .gltf or .glb; OBJ only as OBJ.
        let mut dialog = rfd::FileDialog::new().set_file_name(name);
        dialog = match ext.to_ascii_lowercase().as_str() {
            "glb" => dialog
                .add_filter("glTF binary", &["glb"])
                .add_filter("glTF + .bin", &["gltf"]),
            "gltf" => dialog
                .add_filter("glTF + .bin", &["gltf"])
                .add_filter("glTF binary", &["glb"]),
            _ => dialog.add_filter(ext.to_uppercase(), &[ext.as_str()]),
        };
        if let Some(p) = dialog.save_file() {
            self.save(Some(p));
        }
    }

    fn export_flat_pattern_dialog(&mut self, islands: Vec<usize>) {
        let Some(doc) = self.session.document() else {
            return;
        };
        let stem = doc
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("pattern");
        let filename = format!("{stem}-flat-pattern.svg");
        if let Some(mut path) = rfd::FileDialog::new()
            .set_file_name(filename)
            .add_filter("SVG flat pattern", &["svg"])
            .save_file()
        {
            path.set_extension("svg");
            self.execute(Command::ExportFlatPattern {
                path,
                islands,
                unit: self.physical_uv_unit,
                margin: self.physical_margin * self.physical_rect_unit.to(self.physical_uv_unit),
            });
        }
    }

    /// Islands containing selected faces (empty = all).
    fn target_islands(&self) -> Vec<usize> {
        let Some(scene) = &self.scene else {
            return Vec::new();
        };
        let mut out: Vec<usize> = self
            .sel_faces
            .iter()
            .enumerate()
            .filter(|&(_, &s)| s)
            .map(|(f, _)| scene.islands.island_of_face(f))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    fn lock_targets(&self) -> (Vec<usize>, Vec<usize>) {
        let (Some(doc), Some(scene)) = (self.session.document(), &self.scene) else {
            return (Vec::new(), Vec::new());
        };
        let faces = match self.mode {
            SelectMode::Vertices => select::convert(
                &doc.mesh,
                &scene.islands,
                Elem::Vertex,
                &self.sel_verts,
                Elem::Face,
                false,
            ),
            SelectMode::Edges => select::convert(
                &doc.mesh,
                &scene.islands,
                Elem::Edge,
                &self.sel_edges,
                Elem::Face,
                false,
            ),
            SelectMode::Faces | SelectMode::Islands | SelectMode::Objects => self.sel_faces.clone(),
        };
        let faces = select::indices(&faces);
        let mut islands: Vec<usize> = faces
            .iter()
            .map(|&face| scene.islands.island_of_face(face))
            .collect();
        islands.sort_unstable();
        islands.dedup();
        (faces, islands)
    }

    /// Mesh vertices of the selection: edge ends (edges mode) or every vertex
    /// of the selected faces' islands (islands mode).
    fn selected_vertices(&self) -> Vec<usize> {
        let Some(doc) = self.session.document() else {
            return Vec::new();
        };
        let mesh = &doc.mesh;
        let mut out: Vec<usize> = match self.mode {
            SelectMode::Vertices => select::indices(&self.sel_verts),
            SelectMode::Faces | SelectMode::Objects => select::indices(&self.sel_faces)
                .into_iter()
                .flat_map(|f| mesh.face_corners(f).map(|c| mesh.corner_vertex(c)))
                .collect(),
            SelectMode::Edges => self
                .target_edges()
                .into_iter()
                .flat_map(|e| mesh.edge_vertices(e))
                .collect(),
            SelectMode::Islands => {
                let Some(scene) = &self.scene else {
                    return Vec::new();
                };
                self.target_islands()
                    .into_iter()
                    .flat_map(|i| scene.islands.faces(i).iter())
                    .flat_map(|&f| mesh.face_corners(f as usize).map(|c| mesh.corner_vertex(c)))
                    .collect()
            }
        };
        out.sort_unstable();
        out.dedup();
        out
    }

    fn pin_selection(&mut self, pinned: bool) {
        let vertices = self.selected_vertices();
        if vertices.is_empty() {
            self.set_error("select vertices, edges, faces or islands to pin first".into());
            return;
        }
        self.execute(Command::Pin { vertices, pinned });
    }

    fn target_edges(&self) -> Vec<usize> {
        self.sel_edges
            .iter()
            .enumerate()
            .filter(|&(_, &s)| s)
            .map(|(e, _)| e)
            .collect()
    }

    fn selected_uv_edge_path(&self, border_only: bool) -> Result<(usize, Vec<usize>), String> {
        if self.mode_for_view(1) != SelectMode::Edges {
            return Err("Switch to Edge mode and select an edge path".into());
        }
        let doc = self.session.document().ok_or("Load a mesh first")?;
        let scene = self.scene.as_ref().ok_or("UV islands are unavailable")?;
        let selected = self.target_edges();
        if selected.len() < 2 {
            return Err("Select at least two connected edges in Edge mode".into());
        }
        if border_only
            && selected.iter().any(|&edge| {
                edge >= doc.mesh.edge_count()
                    || !doc.uv().edge_kind(&doc.mesh, edge).is_island_boundary()
            })
        {
            return Err("Align UV needs edges along one UV island boundary".into());
        }
        let ordered = ordered_edge_path(&doc.mesh, &selected)?;
        let mut common: Option<BTreeSet<usize>> = None;
        for &edge in &ordered {
            let (corner, twin) = doc.mesh.edge_corners(edge);
            let candidates: BTreeSet<_> = [Some(corner), twin]
                .into_iter()
                .flatten()
                .map(|c| scene.islands.island_of_face(doc.mesh.corner_face(c)))
                .collect();
            common = Some(match common {
                Some(prior) => prior.intersection(&candidates).copied().collect(),
                None => candidates,
            });
        }
        let candidates = common.unwrap_or_default();
        let island = match candidates.len() {
            0 => return Err("Selected edges must belong to the same UV island".into()),
            1 => *candidates.first().unwrap(),
            _ => {
                let selected_faces: BTreeSet<_> = self
                    .sel_faces
                    .iter()
                    .enumerate()
                    .filter_map(|(face, &selected)| {
                        selected.then_some(scene.islands.island_of_face(face))
                    })
                    .collect();
                let matching: Vec<_> = candidates.intersection(&selected_faces).copied().collect();
                if matching.len() != 1 {
                    return Err("The edge path borders multiple UV islands; select one island's faces to choose a side".into());
                }
                matching[0]
            }
        };
        Ok((island, ordered))
    }

    fn select_island_faces(&mut self, island: usize, on: bool) {
        if let Some(scene) = &self.scene {
            for &f in scene.islands.faces(island) {
                self.sel_faces[f as usize] = on;
            }
        }
        self.gpu_dirty = true;
    }

    fn select_all(&mut self) {
        match self.mode {
            SelectMode::Islands | SelectMode::Faces | SelectMode::Objects => {
                self.sel_faces.iter_mut().for_each(|s| *s = true)
            }
            SelectMode::Edges => self.sel_edges.iter_mut().for_each(|s| *s = true),
            SelectMode::Vertices => self.sel_verts.iter_mut().for_each(|s| *s = true),
        }
        self.gpu_dirty = true;
    }

    /// Islands mode: flips whole islands. Edges mode: flips every edge.
    fn invert_selection(&mut self) {
        match self.mode {
            SelectMode::Islands => {
                let Some(scene) = &self.scene else { return };
                for i in 0..scene.islands.count() {
                    let faces = scene.islands.faces(i);
                    let on = !faces.iter().any(|&f| self.sel_faces[f as usize]);
                    for &f in faces {
                        self.sel_faces[f as usize] = on;
                    }
                }
            }
            SelectMode::Objects => {
                let Some(doc) = self.session.document() else {
                    return;
                };
                for object in 0..doc.objects.len() {
                    let on = !doc
                        .face_objects()
                        .iter()
                        .enumerate()
                        .any(|(f, &o)| o as usize == object && self.sel_faces[f]);
                    for (f, &o) in doc.face_objects().iter().enumerate() {
                        if o as usize == object {
                            self.sel_faces[f] = on;
                        }
                    }
                }
            }
            SelectMode::Edges => self.sel_edges.iter_mut().for_each(|s| *s = !*s),
            SelectMode::Faces => self.sel_faces.iter_mut().for_each(|s| *s = !*s),
            SelectMode::Vertices => self.sel_verts.iter_mut().for_each(|s| *s = !*s),
        }
        self.last_vertex = None;
        self.last_edge = None;
        self.gpu_dirty = true;
    }

    fn clear_selection(&mut self) {
        self.sel_faces.iter_mut().for_each(|s| *s = false);
        self.sel_edges.iter_mut().for_each(|s| *s = false);
        self.sel_verts.iter_mut().for_each(|s| *s = false);
        self.last_vertex = None;
        self.last_edge = None;
        self.gpu_dirty = true;
    }

    fn save_selection_set(&mut self) {
        let Some(doc) = self.session.document() else {
            return;
        };
        let signature = mesh_signature(&doc.mesh);
        let name = self.selection_set_name.trim().to_owned();
        if name.is_empty() {
            self.set_error("enter a name for the selection set".into());
            return;
        }
        let mask = match self.mode {
            SelectMode::Vertices => self.sel_verts.clone(),
            SelectMode::Edges => self.sel_edges.clone(),
            SelectMode::Faces | SelectMode::Islands | SelectMode::Objects => self.sel_faces.clone(),
        };
        if !mask.iter().any(|&selected| selected) {
            self.set_error("select something before saving a set".into());
            return;
        }
        let set = SelectionSet {
            name: name.clone(),
            mode: self.mode,
            mask,
            mesh_signature: signature,
        };
        if let Some(existing) = self.selection_sets.iter_mut().find(|s| s.name == name) {
            *existing = set;
        } else {
            self.selection_sets.push(set);
        }
        self.selection_set_name = format!("Set {}", self.selection_sets.len() + 1);
        self.sync_selection_sets();
        self.set_status(format!("saved selection set {name}"));
    }

    fn sync_selection_sets(&mut self) {
        if let Some(doc) = self.session.document() {
            self.selection_sets_by_path
                .insert(selection_path_key(&doc.path), self.selection_sets.clone());
        }
    }

    fn recall_selection_set(&mut self, index: usize) {
        let Some(set) = self.selection_sets.get(index).cloned() else {
            return;
        };
        let Some(doc) = self.session.document() else {
            return;
        };
        if !set.compatible(&doc.mesh) {
            self.set_error(format!(
                "selection set {} no longer matches this mesh",
                set.name
            ));
            return;
        }
        self.clear_selection();
        self.mode = set.mode;
        match set.mode {
            SelectMode::Vertices => self.sel_verts = set.mask,
            SelectMode::Edges => self.sel_edges = set.mask,
            SelectMode::Faces | SelectMode::Islands | SelectMode::Objects => {
                self.sel_faces = set.mask;
                for (selected, &hidden) in self.sel_faces.iter_mut().zip(&self.hidden) {
                    *selected &= !hidden;
                }
            }
        }
        self.gpu_dirty = true;
        self.set_status(format!("recalled selection set {}", set.name));
    }

    /// A mode's selection as a mask of its element kind.
    fn mode_mask(&self, mode: SelectMode) -> Vec<bool> {
        match mode {
            SelectMode::Vertices => self.sel_verts.clone(),
            SelectMode::Edges => self.sel_edges.clone(),
            SelectMode::Faces => self.sel_faces.clone(),
            SelectMode::Objects => self.sel_faces.clone(),
            SelectMode::Islands => match (self.session.document(), &self.scene) {
                (Some(doc), Some(scene)) => select::convert(
                    &doc.mesh,
                    &scene.islands,
                    Elem::Face,
                    &self.sel_faces,
                    Elem::Island,
                    false,
                ),
                _ => Vec::new(),
            },
        }
    }

    fn set_mode_mask(&mut self, mode: SelectMode, mask: Vec<bool>) {
        match mode {
            SelectMode::Vertices => self.sel_verts = mask,
            SelectMode::Edges => self.sel_edges = mask,
            SelectMode::Faces => self.sel_faces = mask,
            SelectMode::Islands => {
                if let (Some(doc), Some(scene)) = (self.session.document(), &self.scene) {
                    self.sel_faces = select::convert(
                        &doc.mesh,
                        &scene.islands,
                        Elem::Island,
                        &mask,
                        Elem::Face,
                        false,
                    );
                }
            }
            SelectMode::Objects => {
                if let Some(doc) = self.session.document() {
                    self.sel_faces =
                        expand_object_faces(doc.face_objects(), doc.objects.len(), &mask);
                }
            }
        }
        self.gpu_dirty = true;
    }

    fn grow_selection(&mut self, grow: bool) {
        if self.mode == SelectMode::Islands {
            return self.set_status("grow/shrink works in vertex, edge and face modes".into());
        }
        let mask = self.mode_mask(self.mode);
        let (Some(doc), Some(scene)) = (self.session.document(), &self.scene) else {
            return;
        };
        let (mesh, islands, kind) = (&doc.mesh, &scene.islands, self.mode.elem());
        let out = if grow {
            select::grow(mesh, islands, kind, &mask)
        } else {
            select::shrink(mesh, islands, kind, &mask)
        };
        self.set_mode_mask(self.mode, out);
    }

    /// Converts the current selection to another mode and switches to it.
    fn convert_selection(&mut self, to: SelectMode) {
        let from = self.mode;
        let mask = self.mode_mask(from);
        let (Some(doc), Some(scene)) = (self.session.document(), &self.scene) else {
            return;
        };
        let out = select::convert(
            &doc.mesh,
            &scene.islands,
            from.elem(),
            &mask,
            to.elem(),
            false,
        );
        self.mode = to;
        self.set_mode_mask(to, out);
        self.last_vertex = None;
    }

    /// Edges mode: adds the loop (or ring) through every selected edge.
    fn select_loops(&mut self, ring: bool) {
        if self.mode != SelectMode::Edges {
            return self.set_status("edge loops and rings work in edge mode".into());
        }
        let Some(doc) = self.session.document() else {
            return;
        };
        let mesh = &doc.mesh;
        for e in select::indices(&self.sel_edges) {
            let edges = if ring {
                select::edge_ring(mesh, e)
            } else {
                select::edge_loop(mesh, e)
            };
            for x in edges {
                self.sel_edges[x] = true;
            }
        }
        self.gpu_dirty = true;
    }

    /// Selects the open edges of the mesh, or island borders (of the
    /// selected islands if any).
    fn select_borders(&mut self, islands: bool) {
        let Some(doc) = self.session.document() else {
            return;
        };
        self.sel_edges = if islands {
            let faces = match &self.scene {
                Some(scene) if self.sel_faces.iter().any(|&s| s) => Some(select::convert(
                    &doc.mesh,
                    &scene.islands,
                    Elem::Island,
                    &self.mode_mask(SelectMode::Islands),
                    Elem::Face,
                    false,
                )),
                _ => None,
            };
            select::island_border_edges(&doc.mesh, doc.uv(), faces.as_deref())
        } else {
            select::open_edges(&doc.mesh)
        };
        self.mode = SelectMode::Edges;
        self.last_vertex = None;
        self.gpu_dirty = true;
    }

    /// Hides the selection's faces, or with `isolate` everything else.
    /// `None` unhides all.
    fn set_hidden(&mut self, isolate: Option<bool>) {
        let mask = self.mode_mask(self.mode);
        let Some(doc) = self.session.document() else {
            return;
        };
        let n = doc.mesh.face_count();
        match (isolate, &self.scene) {
            (None, _) => self.manual_hidden = vec![false; n],
            (Some(isolate), Some(scene)) => {
                let faces = select::convert(
                    &doc.mesh,
                    &scene.islands,
                    self.mode.elem(),
                    &mask,
                    Elem::Face,
                    false,
                );
                if !faces.iter().any(|&s| s) {
                    return self.set_status("nothing selected to hide or isolate".into());
                }
                for (h, &s) in self.manual_hidden.iter_mut().zip(&faces) {
                    *h |= s != isolate;
                }
            }
            (Some(_), None) => return,
        }
        self.refresh_hidden();
    }

    fn refresh_hidden(&mut self) {
        let Some(doc) = self.session.document() else {
            return;
        };
        self.hidden = (0..doc.mesh.face_count())
            .map(|face| {
                self.manual_hidden.get(face).copied().unwrap_or(false)
                    || !doc.island_groups().face_visible(face)
            })
            .collect();
        for (selected, &hidden) in self.sel_faces.iter_mut().zip(&self.hidden) {
            *selected &= !hidden;
        }
        if let Some(scene) = &mut self.scene {
            scene.set_hidden(&doc.mesh, &self.hidden);
        }
        self.gpu_dirty = true;
    }

    fn is_hidden(&self, f: usize) -> bool {
        self.hidden.get(f).copied().unwrap_or(false)
    }

    fn frame_views(&mut self) {
        let (Some(scene), Some(doc)) = (&self.scene, self.session.document()) else {
            return;
        };
        self.cam3.frame(scene.bbox.0, scene.bbox.1);
        let (lo, hi) = Scene::uv_bbox(doc.uv());
        self.camuv
            .frame(lo.as_vec2(), hi.as_vec2(), self.view_sizes[1]);
    }

    fn frame_selection(&mut self) {
        let (Some(scene), Some(doc)) = (&self.scene, self.session.document()) else {
            return;
        };
        let mesh = &doc.mesh;
        let mask = self.mode_mask(self.mode);
        let faces = select::convert(
            mesh,
            &scene.islands,
            self.mode.elem(),
            &mask,
            Elem::Face,
            false,
        );
        let vertices = self.selected_vertices();
        if vertices.is_empty() {
            self.set_status("nothing selected to frame".into());
            return;
        }
        let mut vertex_mask = vec![false; mesh.vertex_count()];
        for v in vertices {
            vertex_mask[v] = true;
        }
        let mut lo3 = Vec3::splat(f32::INFINITY);
        let mut hi3 = Vec3::splat(f32::NEG_INFINITY);
        let mut lo2 = DVec2::INFINITY;
        let mut hi2 = DVec2::NEG_INFINITY;
        let face_mode = matches!(
            self.mode,
            SelectMode::Faces | SelectMode::Islands | SelectMode::Objects
        );
        for f in 0..mesh.face_count() {
            if self.is_hidden(f) || !faces.get(f).copied().unwrap_or(false) {
                continue;
            }
            for c in mesh.face_corners(f) {
                let v = mesh.corner_vertex(c);
                if face_mode || vertex_mask[v] {
                    let p = mesh.positions()[v].as_vec3();
                    lo3 = lo3.min(p);
                    hi3 = hi3.max(p);
                    let uv = doc.uv().uv_at_corner(c);
                    lo2 = lo2.min(uv);
                    hi2 = hi2.max(uv);
                }
            }
        }
        if lo3.is_finite() && lo2.is_finite() {
            self.cam3.frame(lo3, hi3);
            self.camuv
                .frame_bounds(lo2.as_vec2(), hi2.as_vec2(), self.view_sizes[1]);
        } else {
            self.set_status("nothing visible selected to frame".into());
        }
    }

    // ------------------------------------------------------------ UI

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let hotkeys = self.hotkeys.keys.map(|key| Binding {
            key: key.to_string(),
            command: false,
            shift: false,
            alt: false,
        });
        let all = take_binding(ctx, &self.hotkeys.extras[1]);
        let invert = take_binding(ctx, &self.hotkeys.extras[2]);
        if !ctx.egui_wants_keyboard_input() {
            if all {
                self.select_all();
            }
            if invert {
                self.invert_selection();
            }
        }
        let optimize = take_binding(ctx, &hotkeys[5]);
        if optimize && !ctx.egui_wants_keyboard_input() {
            self.optimize();
        }
        let grow = take_binding(ctx, &self.hotkeys.extras[3]);
        let shrink = take_binding(ctx, &self.hotkeys.extras[4]);
        let ring = take_binding(ctx, &self.hotkeys.extras[5]);
        let lp = take_binding(ctx, &self.hotkeys.extras[6]);
        let unhide = take_binding(ctx, &self.hotkeys.extras[7]);
        let isolate = take_binding(ctx, &self.hotkeys.extras[8]);
        let hide = take_binding(ctx, &self.hotkeys.extras[9]);
        let cut_unfold = take_binding(ctx, &self.hotkeys.extras[10]);
        if !ctx.egui_wants_keyboard_input() {
            if grow {
                self.grow_selection(true);
            }
            if shrink {
                self.grow_selection(false);
            }
            if ring {
                self.select_loops(true);
            } else if lp {
                self.select_loops(false);
            }
            if unhide {
                self.set_hidden(None);
            } else if isolate {
                self.set_hidden(Some(true));
            } else if hide {
                self.set_hidden(Some(false));
            }
            if cut_unfold {
                self.execute(Command::CutUnfold {
                    edges: self.target_edges(),
                });
            }
        }
        let open = take_binding(ctx, &self.hotkeys.extras[11]);
        let save = take_binding(ctx, &self.hotkeys.extras[12]);
        let undo = take_binding(ctx, &self.hotkeys.extras[13]);
        let redo = take_binding(ctx, &self.hotkeys.extras[14]);
        let frame_sel = take_binding(ctx, &self.hotkeys.extras[0]);
        let frame = take_binding(ctx, &hotkeys[0]);
        let cut = take_binding(ctx, &hotkeys[1]);
        let weld = take_binding(ctx, &hotkeys[2]);
        let unfold = take_binding(ctx, &hotkeys[3]);
        let pack = take_binding(ctx, &hotkeys[4]);
        let clear = take_binding(ctx, &self.hotkeys.extras[15]);
        if open {
            self.open_dialog();
        }
        if save {
            self.save(None);
        }
        // Redo is checked first: Ctrl+Shift+Z also matches Ctrl+Z.
        if redo {
            self.execute(Command::Redo);
        } else if undo {
            self.execute(Command::Undo);
        }
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        if frame {
            self.frame_views();
        }
        if frame_sel {
            self.frame_selection();
        }
        if cut {
            self.execute(Command::Cut {
                edges: self.target_edges(),
            });
        }
        if weld {
            self.execute(Command::Weld {
                edges: self.target_edges(),
            });
        }
        if unfold {
            self.execute(Command::Unfold {
                islands: self.target_islands(),
                method: Default::default(),
                preserve_orientation: false,
                border_weight: self.border_weight,
                prevent_overlap: self.prevent_overlap,
            });
        }
        if pack {
            self.pack();
        }
        if clear {
            self.clear_selection();
        }
    }

    fn pack(&mut self) {
        let islands = if let Some(group) = self.group_selection {
            self.session
                .document()
                .zip(self.scene.as_ref())
                .and_then(|(doc, scene)| doc.island_groups().islands_in(group, &scene.islands).ok())
                .unwrap_or_default()
        } else {
            self.target_islands()
        };
        let groups = if self.group_selection.is_some() && islands.len() > 1 {
            vec![islands.clone()]
        } else {
            self.manual_pack_groups(&islands)
        };
        self.execute(Command::Pack {
            islands,
            distribute_by: self.pack_udim.then_some(self.pack_distribution).flatten(),
            padding_px: self.padding,
            margin_px: self.margin,
            resolution: self.resolution,
            orient: true,
            rotate_90: self.pack_rotate,
            method: self.pack_method,
            rescale: self.pack_rescale,
            around_others: true,
            region: None,
            tiles: self.pack_udim.then_some(self.pack_tiles),
            tiles_u: None,
            mirror: false,
            rotation_step_deg: None,
            quality: Default::default(),
            padding_uv: None,
            margin_uv: None,
            texel_density: self.pack_keep_density.then_some(self.density),
            groups,
            island_multipliers: Vec::new(),
            stack_similar: false,
            stack_mode: Default::default(),
            stack_tolerance: 0.05,
            preserve_stacks: self.pack_preserve_stacks || self.group_selection.is_some(),
            pixel_aligned: self.pack_pixel_aligned,
            padding_perfect: self.pack_padding_perfect && !self.pack_udim,
        });
    }

    fn manual_pack_groups(&self, selected: &[usize]) -> Vec<Vec<usize>> {
        if !self.pack_keep_groups {
            return Vec::new();
        }
        let (Some(doc), Some(scene)) = (self.session.document(), &self.scene) else {
            return Vec::new();
        };
        packing_groups_for_roots(
            doc.island_groups(),
            &scene.islands,
            selected,
            doc.locked_faces(),
            doc.locked_island_faces(),
        )
    }

    fn stack_preview_ui(&self, ui: &mut egui::Ui, selected: &[usize], reference: usize) {
        let (Some(doc), Some(scene)) = (self.session.document(), self.scene.as_ref()) else {
            return;
        };
        let mut targets = selected.to_vec();
        if let Some(pos) = targets.iter().position(|&island| island == reference) {
            targets.swap(0, pos);
        }
        let mut planned = doc.uv().clone();
        let report = if self.stack_conform {
            stack::stack_conform_to_reference(
                &doc.mesh,
                &mut planned,
                &scene.islands,
                &targets,
                StackMode::Similar,
                0.05,
                4.0,
            )
        } else {
            stack::stack_to_reference(
                &doc.mesh,
                &mut planned,
                &scene.islands,
                &targets,
                StackMode::Similar,
                0.05,
            )
        };
        ui.label(format!("Reference island: {reference}"));
        for (island, reason) in &report.failed {
            ui.colored_label(Color32::YELLOW, format!("Island {island}: {reason}"));
        }
        if report.matches.is_empty() {
            ui.colored_label(Color32::YELLOW, "No compatible contours in the selection");
            return;
        }
        for match_info in &report.matches {
            ui.label(format!(
                "Island {}: {:?}, max {:.4}%, mean {:.4}%, overlap {:.2}%, scale {:.4}{}",
                match_info.island,
                match_info.quality,
                match_info.max_boundary_error * 100.0,
                match_info.mean_boundary_error * 100.0,
                match_info.overlap_fraction * 100.0,
                match_info.scale,
                if match_info.mirrored {
                    ", mirrored"
                } else {
                    ""
                }
            ));
        }
        let mut outlines: Vec<(usize, Vec<[DVec2; 2]>)> = Vec::new();
        for island in std::iter::once(reference).chain(report.matches.iter().map(|m| m.island)) {
            let mut segments = Vec::new();
            for &face in scene.islands.faces(island) {
                for corner in doc.mesh.face_corners(face as usize) {
                    if planned.edge_kind(&doc.mesh, doc.mesh.corner_edge(corner))
                        != EdgeKind::Interior
                    {
                        segments.push([
                            planned.uv_at_corner(corner),
                            planned.uv_at_corner(doc.mesh.next(corner)),
                        ]);
                    }
                }
            }
            outlines.push((island, segments));
        }
        let points = outlines
            .iter()
            .flat_map(|(_, segments)| segments.iter().flatten());
        let (lo, hi) = points.fold((DVec2::INFINITY, DVec2::NEG_INFINITY), |(lo, hi), &p| {
            (lo.min(p), hi.max(p))
        });
        let (rect, _) = ui.allocate_exact_size(
            EVec2::new(ui.available_width().max(160.0), 190.0),
            Sense::hover(),
        );
        let width = (hi.x - lo.x).max(1e-9);
        let height = (hi.y - lo.y).max(1e-9);
        let scale =
            ((rect.width() - 20.0) as f64 / width).min((rect.height() - 20.0) as f64 / height);
        let center = (lo + hi) * 0.5;
        let map = |p: DVec2| {
            Pos2::new(
                rect.center().x + ((p.x - center.x) * scale) as f32,
                rect.center().y - ((p.y - center.y) * scale) as f32,
            )
        };
        let painter = ui.painter_at(rect);
        for (island, segments) in outlines {
            let color = if island == reference {
                Color32::YELLOW
            } else {
                Color32::LIGHT_BLUE
            };
            for [a, b] in segments {
                painter.line_segment([map(a), map(b)], egui::Stroke::new(2.0, color));
            }
        }
    }

    fn optimize(&mut self) {
        if self.group_selection.is_some() {
            self.set_error("Select an individual island to Relax it".into());
            return;
        }
        self.execute(Command::Optimize {
            islands: self.target_islands(),
            iterations: 20,
            method: self.optimize_method,
            preserve_orientation: false,
            border_weight: self.border_weight,
            prevent_overlap: self.prevent_overlap,
        });
    }

    fn project(&mut self, mode: Projection) {
        self.execute(Command::Project {
            islands: self.target_islands(),
            mode,
        });
    }

    fn menu(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.y = 3.0;
        egui::MenuBar::new().ui(ui, |ui| {
            ui.add(egui::Image::new((
                self.brand_icon.id(),
                egui::vec2(27.0, 27.0),
            )));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.label(egui::RichText::new("Uv").size(18.0).strong());
                ui.label(
                    egui::RichText::new("Craft")
                        .size(18.0)
                        .strong()
                        .color(theme::ORANGE),
                );
            });
            ui.separator();
            ui.menu_button("File", |ui| {
                if ui
                    .button(format!("Open…  ({})", self.hotkeys.extras[11].display()))
                    .clicked()
                {
                    self.open_dialog();
                }
                let has_doc = self.session.document().is_some();
                if ui
                    .add_enabled(
                        has_doc,
                        egui::Button::new(format!("Save  ({})", self.hotkeys.extras[12].display())),
                    )
                    .clicked()
                {
                    self.save(None);
                }
                if ui
                    .add_enabled(has_doc, egui::Button::new("Save As…"))
                    .clicked()
                {
                    self.save_as_dialog();
                }
                if let Some(bridge) = &self.bridge {
                    ui.separator();
                    ui.label("Blender bridge");
                    ui.label(format!("URL: http://{}", bridge.address()));
                    let token = bridge.token();
                    if ui.button("Copy bridge token").clicked() {
                        ui.ctx().copy_text(token);
                    }
                    if let Some(count) = bridge.session_count() {
                        ui.label(format!("Connected objects: {count}"));
                    }
                }
                ui.menu_button("Install Blender add-on…", |ui| {
                    let targets = blender_addon::discover_targets();
                    if targets.is_empty() {
                        ui.weak("No Blender profile found; choose its add-ons folder below.");
                    }
                    for (label, path) in targets {
                        if ui
                            .button(label)
                            .on_hover_text(path.display().to_string())
                            .clicked()
                        {
                            self.install_blender_addon(&path);
                            ui.close();
                        }
                    }
                    ui.separator();
                    if ui.button("Choose add-ons folder…").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_folder() {
                            self.install_blender_addon(&path);
                        }
                        ui.close();
                    }
                    ui.weak("Enable the add-on in Blender after installation.");
                });
                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Edit", |ui| {
                if ui
                    .add_enabled(
                        self.session.can_undo(),
                        egui::Button::new(format!("Undo  ({})", self.hotkeys.extras[13].display())),
                    )
                    .clicked()
                {
                    self.execute(Command::Undo);
                }
                if ui
                    .add_enabled(
                        self.session.can_redo(),
                        egui::Button::new(format!("Redo  ({})", self.hotkeys.extras[14].display())),
                    )
                    .clicked()
                {
                    self.execute(Command::Redo);
                }
            });
            ui.menu_button("Select", |ui| {
                for (label, mode) in [
                    ("Vertices", SelectMode::Vertices),
                    ("Edges", SelectMode::Edges),
                    ("Faces", SelectMode::Faces),
                    ("Islands", SelectMode::Islands),
                    ("Objects", SelectMode::Objects),
                ] {
                    ui.selectable_value(&mut self.mode, mode, label);
                }
                ui.separator();
                if ui.button("Select all").clicked() {
                    self.select_all();
                }
                if ui.button("Clear selection").clicked() {
                    self.clear_selection();
                }
            });
            ui.menu_button("UV", |ui| {
                for (label, mode) in [
                    ("UV vertices", SelectMode::Vertices),
                    ("UV edges", SelectMode::Edges),
                    ("UV faces", SelectMode::Faces),
                    ("UV islands", SelectMode::Islands),
                ] {
                    ui.selectable_value(&mut self.mode, mode, label);
                }
                ui.separator();
                if ui.button("Frame selected UVs").clicked() {
                    self.frame_selection();
                }
            });
            ui.menu_button("Unwrap", |ui| {
                let has_doc = self.session.document().is_some();
                if ui
                    .add_enabled(has_doc, egui::Button::new("Auto seams"))
                    .clicked()
                {
                    self.execute(Command::AutoSeams {
                        islands: self.target_islands(),
                        angle_deg: self.seam_angle,
                        ensure_disk: true,
                    });
                }
                if ui
                    .add_enabled(has_doc, egui::Button::new("Unfold"))
                    .clicked()
                {
                    self.execute(Command::Unfold {
                        islands: self.target_islands(),
                        method: Default::default(),
                        preserve_orientation: false,
                        border_weight: self.border_weight,
                        prevent_overlap: self.prevent_overlap,
                    });
                }
                if ui
                    .add_enabled(has_doc, egui::Button::new("Optimize"))
                    .clicked()
                {
                    self.optimize();
                }
            });
            ui.menu_button("Pack", |ui| {
                if ui
                    .add_enabled(
                        self.session.document().is_some(),
                        egui::Button::new("Pack islands"),
                    )
                    .clicked()
                {
                    self.pack();
                }
                ui.checkbox(&mut self.pack_rescale, "Fill available space");
                ui.checkbox(&mut self.pack_rotate, "Allow 90° rotations");
                ui.checkbox(&mut self.pack_keep_groups, "Keep island groups together");
                ui.checkbox(&mut self.pack_preserve_stacks, "Preserve stacked islands");
            });
            ui.menu_button("View", |ui| {
                if ui
                    .button(format!("Frame all  ({})", self.hotkeys.keys[0]))
                    .clicked()
                {
                    self.frame_views();
                }
                if ui
                    .button(format!(
                        "Frame selection  ({})",
                        self.hotkeys.extras[0].display()
                    ))
                    .clicked()
                {
                    self.frame_selection();
                }
                ui.checkbox(&mut self.wireframe, "Wireframe");
                ui.checkbox(&mut self.show_outliner, "Scene outliner");
                ui.separator();
                let old_theme = self.dark_theme;
                ui.radio_value(&mut self.dark_theme, true, "Dark theme");
                ui.radio_value(&mut self.dark_theme, false, "Light theme");
                if self.dark_theme != old_theme {
                    theme::apply(ui.ctx(), self.dark_theme);
                }
            });
            ui.menu_button("Tools", |ui| {
                if ui.button("Session defaults…").clicked() {
                    self.show_preferences = true;
                    ui.close();
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("Controls and workflow…").clicked() {
                    self.show_help = true;
                    ui.close();
                }
            });
            let maximized = ui.input(|input| input.viewport().maximized.unwrap_or(false));
            let drag_width = (ui.available_width() - 110.0).max(8.0);
            let drag = ui
                .add_sized(
                    [drag_width, 26.0],
                    egui::Label::new("").sense(egui::Sense::click_and_drag()),
                )
                .on_hover_cursor(egui::CursorIcon::Grab)
                .on_hover_text("Drag window; double-click to maximize");
            if drag.drag_started() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            if drag.double_clicked() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            }
            if ui
                .add_sized([28.0, 26.0], egui::Button::new("_"))
                .on_hover_text("Minimize")
                .clicked()
            {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
            if ui
                .add_sized(
                    [28.0, 26.0],
                    egui::Button::new(if maximized { "[]" } else { "[ ]" }),
                )
                .on_hover_text(if maximized { "Restore" } else { "Maximize" })
                .clicked()
            {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            }
            if ui
                .add_sized([28.0, 26.0], egui::Button::new("X"))
                .on_hover_text("Close")
                .clicked()
            {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            use theme::ToolbarIcon as Icon;
            let has_doc = self.session.document().is_some();
            if theme::icon_button(ui, Icon::Open, "Open OBJ, glTF, GLB or FBX", false).clicked() {
                self.open_dialog();
            }
            if ui
                .add_enabled_ui(has_doc, |ui| {
                    theme::icon_button(ui, Icon::Save, "Save", false)
                })
                .inner
                .clicked()
            {
                self.save(None);
            }
            if ui
                .add_enabled_ui(has_doc, |ui| {
                    theme::icon_button(ui, Icon::SaveAs, "Save As", false)
                })
                .inner
                .clicked()
            {
                self.save_as_dialog();
            }
            ui.separator();
            if ui
                .add_enabled_ui(self.session.can_undo(), |ui| {
                    theme::icon_button(ui, Icon::Undo, "Undo", false)
                })
                .inner
                .clicked()
            {
                self.execute(Command::Undo);
            }
            if ui
                .add_enabled_ui(self.session.can_redo(), |ui| {
                    theme::icon_button(ui, Icon::Redo, "Redo", false)
                })
                .inner
                .clicked()
            {
                self.execute(Command::Redo);
            }
            ui.separator();
            if theme::icon_button(
                ui,
                Icon::Select,
                "Select with rectangle",
                !self.brush && !self.lasso,
            )
            .clicked()
            {
                self.brush = false;
                self.lasso = false;
                self.density_brush = false;
            }
            if theme::icon_button(
                ui,
                Icon::Lasso,
                "Lasso selection",
                !self.brush && self.lasso,
            )
            .clicked()
            {
                self.brush = false;
                self.lasso = true;
                self.density_brush = false;
            }
            if theme::icon_button(ui, Icon::Brush, "Selection brush", self.brush).clicked() {
                self.brush = true;
                self.seam_brush = false;
                self.uv_edit_mode = None;
                self.density_brush = false;
            }
            ui.separator();
            if theme::icon_button(
                ui,
                Icon::Model,
                "Select mesh objects",
                self.mode == SelectMode::Objects,
            )
            .clicked()
            {
                self.mode = SelectMode::Objects;
            }
            if theme::icon_button(
                ui,
                Icon::Uv,
                "Select UV islands",
                self.mode == SelectMode::Islands,
            )
            .clicked()
            {
                self.mode = SelectMode::Islands;
            }
            if theme::icon_button(ui, Icon::Fit, "Frame both viewports", false).clicked() {
                self.frame_views();
            }
            ui.separator();
            if theme::icon_button(ui, Icon::Wire, "Toggle wireframe", self.wireframe).clicked() {
                self.wireframe = !self.wireframe;
            }
            if theme::icon_button(ui, Icon::Scene, "Toggle scene outliner", self.show_outliner)
                .clicked()
            {
                self.show_outliner = !self.show_outliner;
            }
            ui.separator();
            if ui
                .add_enabled(
                    has_doc,
                    egui::Button::new(egui::RichText::new("Unwrap").color(Color32::BLACK))
                        .fill(theme::ORANGE),
                )
                .on_hover_text("Unfold selected UV islands")
                .clicked()
            {
                self.execute(Command::Unfold {
                    islands: self.target_islands(),
                    method: Default::default(),
                    preserve_orientation: false,
                    border_weight: self.border_weight,
                    prevent_overlap: self.prevent_overlap,
                });
            }
            if ui
                .add_enabled(has_doc, egui::Button::new("Pack"))
                .on_hover_text("Pack selected UV islands")
                .clicked()
            {
                self.pack();
            }
            if ui
                .add_enabled(has_doc, egui::Button::new("Relax"))
                .on_hover_text("Optimize selected UV islands")
                .clicked()
            {
                self.optimize();
            }
            ui.separator();
            let (active, channels) = self
                .session
                .document()
                .and_then(|doc| doc.uv_channel_info())
                .unwrap_or((0, vec![0]));
            let mut switch_to = None;
            ui.add_enabled_ui(has_doc, |ui| {
                egui::ComboBox::from_id_salt("toolbar_uv_channel")
                    .selected_text(format!("UV{}", active + 1))
                    .width(86.0)
                    .show_ui(ui, |ui| {
                        for channel in channels {
                            if ui
                                .selectable_label(channel == active, format!("UV{}", channel + 1))
                                .clicked()
                            {
                                switch_to = Some(channel);
                            }
                        }
                    });
            });
            if let Some(channel) = switch_to.filter(|&channel| channel != active) {
                self.execute(Command::SwitchUvChannel { channel });
            }
            if theme::icon_button(ui, Icon::Settings, "Preferences", false).clicked() {
                self.show_preferences = true;
            }
        });
        ui.add_space(4.0);
    }

    fn outliner(&mut self, ui: &mut egui::Ui) {
        ui.heading("Objects");
        let Some(doc) = self.session.document() else {
            ui.label("Open a mesh to view objects");
            return;
        };
        let names = doc.objects.clone();
        let face_objects = doc.face_objects().to_vec();
        let mut select = None;
        let mut toggle = None;
        if ui.button("Show all").clicked() {
            self.set_hidden(None);
        }
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (object, name) in names.iter().enumerate() {
                let faces: Vec<usize> = face_objects
                    .iter()
                    .enumerate()
                    .filter_map(|(f, &o)| (o as usize == object).then_some(f))
                    .collect();
                let visible = faces
                    .iter()
                    .any(|&f| !self.hidden.get(f).copied().unwrap_or(false));
                let selected = !faces.is_empty()
                    && faces
                        .iter()
                        .all(|&f| self.sel_faces.get(f).copied().unwrap_or(false));
                ui.horizontal(|ui| {
                    if ui
                        .button(if visible { "👁" } else { "○" })
                        .on_hover_text("Toggle object visibility")
                        .clicked()
                    {
                        toggle = Some((object, visible));
                    }
                    if ui
                        .selectable_label(selected, format!("{}  ({} faces)", name, faces.len()))
                        .clicked()
                    {
                        select = Some(object);
                    }
                });
            }
        });
        if let Some((object, visible)) = toggle {
            for (f, &o) in face_objects.iter().enumerate() {
                if o as usize == object {
                    self.manual_hidden[f] = visible;
                    if visible {
                        self.sel_faces[f] = false;
                    }
                }
            }
            self.refresh_hidden();
        }
        if let Some(object) = select {
            self.sel_faces.fill(false);
            for (f, &o) in face_objects.iter().enumerate() {
                if o as usize == object && !self.hidden[f] {
                    self.sel_faces[f] = true;
                }
            }
            self.sel_edges.fill(false);
            self.sel_verts.fill(false);
            self.mode = SelectMode::Objects;
            self.gpu_dirty = true;
        }
    }

    fn group_panel(&mut self, ui: &mut egui::Ui) {
        if let Some(group) = self.group_edit.filter(|&group| {
            self.session
                .document()
                .is_some_and(|doc| doc.island_groups().get(group).is_some())
        }) {
            ui.horizontal(|ui| {
                ui.label("Group name");
                ui.add(egui::TextEdit::singleline(&mut self.group_rename).desired_width(110.0));
                if ui
                    .add_enabled(
                        !self.group_rename.trim().is_empty(),
                        egui::Button::new("Rename"),
                    )
                    .clicked()
                {
                    self.execute(Command::SetIslandGroup {
                        group,
                        name: Some(self.group_rename.clone()),
                        color: None,
                        locked: None,
                        visible: None,
                        selected: None,
                    });
                }
            });
        }
        ui.collapsing("Island groups", |ui| {
            let Some(doc) = self.session.document() else {
                return;
            };
            let nodes: Vec<_> = doc.island_groups().nodes().cloned().collect();
            let mut actions = Vec::new();
            let mut edit = None;
            let mut transform_request = None;
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.group_name).desired_width(120.0));
                if ui.add_enabled(self.target_islands().len() >= 2, egui::Button::new("Group selected")).clicked() {
                    actions.push(Command::CreateIslandGroup {
                        name: self.group_name.clone(),
                        parent: None,
                        children: Vec::new(),
                        islands: self.target_islands(),
                    });
                }
                if let Some(parent) = self.group_edit
                    && ui
                        .button("Child")
                        .on_hover_text("Create a subgroup of the selected group")
                        .clicked()
                    {
                        actions.push(Command::CreateIslandGroup {
                            name: self.group_name.clone(),
                            parent: Some(parent),
                            children: Vec::new(),
                            islands: self.target_islands(),
                        });
                    }
            });
            ui.horizontal(|ui| {
                ui.label("Organize by");
                for (label, by) in [
                    ("Object", GroupBy::Object),
                    ("Material", GroupBy::Material),
                    ("Smoothing", GroupBy::SmoothingGroup),
                ] {
                    if ui.button(label).clicked() {
                        actions.push(Command::OrganizeIslandGroups {
                            by,
                            parent: None,
                            islands: self.target_islands(),
                        });
                    }
                }
            });
            let mut rows = Vec::new();
            for node in &nodes {
                let mut path = vec![node.id];
                let mut parent = node.parent;
                while let Some(id) = parent {
                    path.push(id);
                    parent = nodes
                        .iter()
                        .find(|group| group.id == id)
                        .and_then(|group| group.parent);
                    if path.len() > nodes.len() {
                        break;
                    }
                }
                path.reverse();
                rows.push((path, node));
            }
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            for (path, group) in rows {
                ui.horizontal(|ui| {
                    ui.add_space(12.0 * (path.len().saturating_sub(1)) as f32);
                    if ui
                        .selectable_label(self.group_edit == Some(group.id), &group.name)
                        .clicked()
                    {
                        edit = Some((group.id, group.name.clone()));
                        actions.push(Command::SelectIslandGroup { group: group.id });
                    }
                    if ui
                        .small_button(if group.visible { "👁" } else { "○" })
                        .on_hover_text("Toggle group visibility")
                        .clicked()
                    {
                        actions.push(Command::SetIslandGroup {
                            group: group.id,
                            name: None,
                            color: None,
                            locked: None,
                            visible: Some(!group.visible),
                            selected: None,
                        });
                    }
                    if ui
                        .small_button(if group.locked { "🔒" } else { "🔓" })
                        .on_hover_text("Toggle group lock")
                        .clicked()
                    {
                        actions.push(Command::SetIslandGroup {
                            group: group.id,
                            name: None,
                            color: None,
                            locked: Some(!group.locked),
                            visible: None,
                            selected: None,
                        });
                    }
                    let mut selected = group.selected;
                    if ui
                        .checkbox(&mut selected, "")
                        .on_hover_text("Mark group selected")
                        .changed()
                    {
                        actions.push(Command::SetIslandGroup {
                            group: group.id,
                            name: None,
                            color: None,
                            locked: None,
                            visible: None,
                            selected: Some(selected),
                        });
                        if selected {
                            actions.push(Command::SelectIslandGroup { group: group.id });
                        }
                    }
                    let mut color = egui::Color32::from_rgba_unmultiplied(
                        group.color[0],
                        group.color[1],
                        group.color[2],
                        group.color[3],
                    );
                    if ui.color_edit_button_srgba(&mut color).changed() {
                        actions.push(Command::SetIslandGroup {
                            group: group.id,
                            name: None,
                            color: Some(color.to_array()),
                            locked: None,
                            visible: None,
                            selected: None,
                        });
                    }
                    if ui
                        .small_button("×")
                        .on_hover_text("Ungroup; children move to parent")
                        .clicked()
                    {
                        actions.push(Command::UngroupIslands { group: group.id });
                    }
                });
            }
            if let Some((id, name)) = edit {
                self.group_edit = Some(id);
                self.group_rename = name;
                self.gizmo_enabled = true;
            }
            if let Some(group) = self
                .group_edit
                .filter(|id| nodes.iter().any(|node| node.id == *id))
            {
                ui.horizontal(|ui| {
                    if ui
                        .button("Add selected")
                        .on_hover_text("Move the selected islands into this group")
                        .clicked()
                    {
                        actions.push(Command::AddIslandsToGroup {
                            group,
                            islands: self.target_islands(),
                        });
                    }
                });
                let group_islands = self.session.document().and_then(|doc| {
                    self.scene.as_ref().and_then(|scene| {
                        doc.island_groups().islands_in(group, &scene.islands).ok()
                    })
                }).unwrap_or_default();
                ui.label(format!("{} island(s) · click the group to select all, then use the UV gizmo", group_islands.len()));
                ui.horizontal_wrapped(|ui| {
                    ui.label("Move U/V");
                    ui.add(egui::DragValue::new(&mut self.group_offset[0]).speed(0.01));
                    ui.add(egui::DragValue::new(&mut self.group_offset[1]).speed(0.01));
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Rotate °");
                    ui.add(egui::DragValue::new(&mut self.group_rotation)
                        .speed(0.1).range(-3600.0..=3600.0).fixed_decimals(1));
                    ui.label("Scale");
                    ui.add(egui::DragValue::new(&mut self.group_scale).range(0.01..=100.0).speed(0.01));
                });
                ui.add_sized([260.0, 20.0], egui::Slider::new(
                    &mut self.group_rotation, -180.0..=180.0)
                    .text("Group rotation").suffix("°").step_by(0.1))
                    .on_hover_text("Drag left for negative rotation; type a precise angle above");
                if ui.add_enabled(!group_islands.is_empty(), egui::Button::new("Apply group transform"))
                    .on_hover_text("Move, rotate and uniformly scale the complete group around one shared center")
                    .clicked()
                {
                    transform_request = Some(group);
                }
            }
            for action in actions {
                self.execute(action);
            }
            if let Some(group) = transform_request {
                self.transform_group(group);
            }
        });
    }

    fn transform_group(&mut self, group: u64) {
        let scale = self.group_scale;
        let angle = self.group_rotation;
        let offset = DVec2::from(self.group_offset);
        if !offset.is_finite() || !scale.is_finite() || scale <= 0.0 || !angle.is_finite() {
            self.set_error("group transform needs finite coordinates and a positive scale".into());
            return;
        }
        self.execute(Command::TransformIslandGroup {
            group,
            translate: self.group_offset,
            rotate_deg: angle,
            scale,
        });
        self.group_offset = [0.0; 2];
        self.group_rotation = 0.0;
        self.group_scale = 1.0;
    }

    fn select_group_member(&mut self, group: u64, island: usize) {
        let Some(scene) = &self.scene else {
            return;
        };
        let Some(doc) = self.session.document() else {
            return;
        };
        if !doc
            .island_groups()
            .islands_in(group, &scene.islands)
            .is_ok_and(|members| members.contains(&island))
        {
            return;
        }
        let faces: Vec<usize> = scene
            .islands
            .faces(island)
            .iter()
            .map(|&face| face as usize)
            .collect();
        let mut lo = DVec2::INFINITY;
        let mut hi = DVec2::NEG_INFINITY;
        for index in scene.islands.uv_indices(island, &doc.mesh, doc.uv()) {
            let point = doc.uv().uvs[index as usize];
            lo = lo.min(point);
            hi = hi.max(point);
        }
        self.sel_faces.fill(false);
        for face in faces {
            if face < self.sel_faces.len() && !self.hidden.get(face).copied().unwrap_or(false) {
                self.sel_faces[face] = true;
            }
        }
        self.sel_edges.fill(false);
        self.sel_verts.fill(false);
        self.group_edit = Some(group);
        self.group_selection = None;
        self.set_view_mode(1, SelectMode::Islands);
        if lo.is_finite() && hi.is_finite() {
            self.camuv
                .frame_bounds(lo.as_vec2(), hi.as_vec2(), self.view_sizes[1]);
        }
        self.gpu_dirty = true;
    }

    fn rebuild_group_thumbs(&mut self, group: u64) {
        let mut thumbs = Vec::new();
        if let (Some(doc), Some(scene)) = (self.session.document(), &self.scene)
            && let Ok(members) = doc.island_groups().islands_in(group, &scene.islands)
        {
            for island in members {
                let faces = scene.islands.faces(island);
                let stride = faces.len().div_ceil(150).max(1);
                let mut bounds = (DVec2::INFINITY, DVec2::NEG_INFINITY);
                let mut edges = Vec::new();
                for (face_index, &face) in faces.iter().enumerate() {
                    for corner in doc.mesh.face_corners(face as usize) {
                        let point = doc.uv().uv_at_corner(corner);
                        bounds.0 = bounds.0.min(point);
                        bounds.1 = bounds.1.max(point);
                        if face_index % stride == 0 && edges.len() < 900 {
                            edges.push((point, doc.uv().uv_at_corner(doc.mesh.next(corner))));
                        }
                    }
                }
                if bounds.0.is_finite() && bounds.1.is_finite() {
                    thumbs.push(GroupThumb {
                        island,
                        bounds,
                        edges,
                    });
                }
            }
        }
        self.group_thumb_cache = Some(GroupThumbCache {
            group,
            revision: self.group_workspace_revision,
            thumbs,
        });
    }

    fn group_workspace(&mut self, ui: &mut egui::Ui, outer: Rect) {
        let nodes: Vec<_> = self
            .session
            .document()
            .map(|doc| {
                doc.island_groups()
                    .nodes()
                    .map(|node| (node.id, node.name.clone(), node.locked))
                    .collect()
            })
            .unwrap_or_default();
        if self
            .group_edit
            .is_some_and(|id| !nodes.iter().any(|node| node.0 == id))
        {
            self.group_edit = None;
            self.group_selection = None;
            self.group_thumb_cache = None;
        }
        ui.painter()
            .rect_filled(outer, 4.0, Color32::from_rgb(23, 25, 29));
        ui.painter().rect_stroke(
            outer,
            4.0,
            egui::Stroke::new(1.0, theme::BORDER),
            egui::StrokeKind::Inside,
        );
        let header = Rect::from_min_size(
            outer.min,
            EVec2::new(
                outer.width(),
                if self.group_workspace_open {
                    55.0
                } else {
                    31.0
                },
            ),
        );
        ui.scope_builder(
            egui::UiBuilder::new().max_rect(header.shrink2(EVec2::new(7.0, 3.0))),
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .small_button(if self.group_workspace_open { "v" } else { ">" })
                        .clicked()
                    {
                        self.group_workspace_open = !self.group_workspace_open;
                    }
                    ui.label(egui::RichText::new("Group Workspace").strong());
                    if ui
                        .add_enabled(
                            self.group_selection.is_none() && self.target_islands().len() >= 2,
                            egui::Button::new("Create Group"),
                        )
                        .on_hover_text("Create a group from selected islands")
                        .clicked()
                    {
                        self.execute(Command::CreateIslandGroup {
                            name: self.group_name.clone(),
                            parent: None,
                            children: Vec::new(),
                            islands: self.target_islands(),
                        });
                    }
                    if !nodes.is_empty() {
                        let name = self
                            .group_edit
                            .and_then(|id| nodes.iter().find(|node| node.0 == id))
                            .map_or("Choose group", |node| node.1.as_str());
                        egui::ComboBox::from_id_salt("workspace_group")
                            .selected_text(name)
                            .show_ui(ui, |ui| {
                                for (id, name, _) in &nodes {
                                    if ui
                                        .selectable_label(self.group_edit == Some(*id), name)
                                        .clicked()
                                    {
                                        self.group_edit = Some(*id);
                                        self.group_rename = name.clone();
                                        self.group_selection = Some(*id);
                                        self.group_workspace_open = true;
                                        self.set_view_mode(1, SelectMode::Islands);
                                        self.execute(Command::SelectIslandGroup { group: *id });
                                    }
                                }
                            });
                    }
                    let active = self.group_edit;
                    if let Some(group) = active {
                        let count = self
                            .session
                            .document()
                            .zip(self.scene.as_ref())
                            .and_then(|(doc, scene)| {
                                doc.island_groups().islands_in(group, &scene.islands).ok()
                            })
                            .map_or(0, |members| members.len());
                        ui.weak(format!("{count} islands"));
                        let group_mode = self.group_selection == Some(group);
                        if ui
                            .add(
                                egui::Button::new(egui::RichText::new("Select Group").color(
                                    if group_mode {
                                        Color32::BLACK
                                    } else {
                                        Color32::WHITE
                                    },
                                ))
                                .fill(if group_mode {
                                    theme::ORANGE
                                } else {
                                    theme::SURFACE
                                }),
                            )
                            .clicked()
                        {
                            self.group_rename = nodes
                                .iter()
                                .find(|node| node.0 == group)
                                .map_or(String::new(), |node| node.1.clone());
                            self.group_selection = Some(group);
                            self.set_view_mode(1, SelectMode::Islands);
                            self.execute(Command::SelectIslandGroup { group });
                        }
                        if ui
                            .add_enabled(group_mode, egui::Button::new("Individual"))
                            .clicked()
                            && let Some(first) = self
                                .session
                                .document()
                                .zip(self.scene.as_ref())
                                .and_then(|(doc, scene)| {
                                    doc.island_groups().islands_in(group, &scene.islands).ok()
                                })
                                .and_then(|members| members.first().copied())
                        {
                            self.select_group_member(group, first);
                        }
                        ui.menu_button("Manage", |ui| {
                            if ui
                                .add_enabled(
                                    !group_mode && !self.target_islands().is_empty(),
                                    egui::Button::new("Add selected islands"),
                                )
                                .clicked()
                            {
                                self.execute(Command::AddIslandsToGroup {
                                    group,
                                    islands: self.target_islands(),
                                });
                                ui.close();
                            }
                            if ui
                                .add_enabled(
                                    !group_mode && !self.target_islands().is_empty(),
                                    egui::Button::new("Remove selected islands"),
                                )
                                .clicked()
                            {
                                self.execute(Command::RemoveIslandsFromGroup {
                                    group,
                                    islands: self.target_islands(),
                                });
                                ui.close();
                            }
                            if ui.button("Ungroup").clicked() {
                                self.execute(Command::UngroupIslands { group });
                                ui.close();
                            }
                            if ui.button("Delete group").clicked() {
                                self.execute(Command::DeleteIslandGroup { group });
                                ui.close();
                            }
                        });
                    } else if nodes.is_empty() {
                        ui.weak("Select at least two islands, then Create Group here");
                    }
                });
            },
        );
        if !self.group_workspace_open {
            return;
        }
        let Some(group) = self.group_edit else {
            return;
        };
        if self.group_thumb_cache.as_ref().is_none_or(|cache| {
            cache.group != group || cache.revision != self.group_workspace_revision
        }) {
            self.rebuild_group_thumbs(group);
        }
        let thumbs = &self.group_thumb_cache.as_ref().expect("built above").thumbs;
        let selected = self.target_islands();
        let group_mode = self.group_selection == Some(group);
        let body = Rect::from_min_max(
            outer.min + EVec2::new(5.0, 58.0),
            outer.max - EVec2::new(5.0, 5.0),
        );
        let mut selected_member = None;
        let mut remove_member = None;
        ui.scope_builder(egui::UiBuilder::new().max_rect(body), |ui| {
            egui::ScrollArea::horizontal()
                .id_salt("group_workspace_members")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for thumb in thumbs {
                            let height = (body.height() - 12.0).max(70.0);
                            let (tile, response) =
                                ui.allocate_exact_size(EVec2::new(112.0, height), Sense::click());
                            let active = group_mode || selected.contains(&thumb.island);
                            let stroke = if active { theme::ORANGE } else { theme::BORDER };
                            ui.painter()
                                .rect_filled(tile, 5.0, Color32::from_rgb(29, 31, 36));
                            ui.painter().rect_stroke(
                                tile,
                                5.0,
                                egui::Stroke::new(if active { 2.0 } else { 1.0 }, stroke),
                                egui::StrokeKind::Inside,
                            );
                            let art = Rect::from_min_max(
                                tile.min + EVec2::new(8.0, 8.0),
                                tile.max - EVec2::new(8.0, 25.0),
                            );
                            let extent = (thumb.bounds.1 - thumb.bounds.0).max(DVec2::splat(1e-6));
                            let scale = (art.width() as f64 / extent.x)
                                .min(art.height() as f64 / extent.y)
                                * 0.88;
                            let mid = (thumb.bounds.0 + thumb.bounds.1) * 0.5;
                            let point = |uv: DVec2| {
                                art.center()
                                    + EVec2::new(
                                        ((uv.x - mid.x) * scale) as f32,
                                        -((uv.y - mid.y) * scale) as f32,
                                    )
                            };
                            for &(a, b) in &thumb.edges {
                                ui.painter().line_segment(
                                    [point(a), point(b)],
                                    egui::Stroke::new(
                                        1.0,
                                        if active {
                                            Color32::from_rgb(255, 185, 100)
                                        } else {
                                            Color32::from_rgb(204, 208, 215)
                                        },
                                    ),
                                );
                            }
                            ui.painter().text(
                                tile.center_bottom() - EVec2::new(0.0, 11.0),
                                egui::Align2::CENTER_CENTER,
                                format!("Island {}", thumb.island + 1),
                                egui::FontId::proportional(12.0),
                                Color32::WHITE,
                            );
                            if response.clicked() {
                                selected_member = Some(thumb.island);
                            }
                            response.context_menu(|ui| {
                                if ui.button("Remove from group").clicked() {
                                    remove_member = Some(thumb.island);
                                    ui.close();
                                }
                            });
                        }
                    });
                });
        });
        if let Some(island) = selected_member {
            self.select_group_member(group, island);
        }
        if let Some(island) = remove_member {
            self.execute(Command::RemoveIslandsFromGroup {
                group,
                islands: vec![island],
            });
        }
    }

    /// Compact work panel inspired by the project's interface reference.
    /// The full tool collection remains available in Advanced tools.
    fn reference_tools(&mut self, ui: &mut egui::Ui) {
        let has_doc = self.session.document().is_some();
        let tab_width = ((ui.available_width() - 12.0) / 3.0).max(65.0);
        ui.horizontal(|ui| {
            for (tab, label) in [(0, "Unwrap"), (1, "Pack"), (2, "Relax")] {
                let fill = if self.reference_tab == tab {
                    theme::ORANGE
                } else {
                    ui.visuals().widgets.inactive.bg_fill
                };
                let text_color = if self.reference_tab == tab {
                    Color32::BLACK
                } else {
                    Color32::WHITE
                };
                if ui
                    .add_sized(
                        [tab_width, 34.0],
                        egui::Button::new(egui::RichText::new(label).color(text_color)).fill(fill),
                    )
                    .clicked()
                {
                    self.reference_tab = tab;
                }
            }
        });
        ui.add_space(4.0);
        if !has_doc && self.running.is_none() {
            ui.label(egui::RichText::new("Open a mesh to use UV tools").weak());
        }
        if self.reference_tab == 1
            && let Some(running) = &self.running
            && matches!(&running.cmd, Command::Pack { .. })
        {
            ui.group(|ui| {
                ui.label("Packing UV islands…");
                if let Some(progress) = &running.progress {
                    let fraction = if progress.total == 0 {
                        0.0
                    } else {
                        progress.completed as f32 / progress.total as f32
                    };
                    ui.add(
                        egui::ProgressBar::new(fraction.clamp(0.0, 1.0)).text(format!(
                            "{} {}/{}",
                            progress.phase, progress.completed, progress.total
                        )),
                    );
                } else {
                    ui.spinner();
                }
                if ui
                    .add_enabled(
                        !running.cancel.is_cancelled(),
                        egui::Button::new("Stop packing"),
                    )
                    .clicked()
                {
                    running.cancel.cancel();
                    self.jobs.clear();
                }
            });
        }
        ui.add_enabled_ui(has_doc, |ui| {
            match self.reference_tab {
                1 => self.reference_pack_tools(ui),
                2 => self.reference_relax_tools(ui),
                _ => self.reference_unwrap_tools(ui),
            }
            self.reference_seam_tools(ui);
            self.reference_island_tools(ui);
            self.reference_island_detail_tools(ui);
            self.group_panel(ui);
            self.reference_transform_tools(ui);
            self.reference_visualization_tools(ui);
            self.reference_uv_sets(ui);
        });
        if self.show_outliner {
            egui::CollapsingHeader::new("Objects")
                .default_open(false)
                .show(ui, |ui| self.outliner(ui));
        }
        egui::CollapsingHeader::new("Advanced tools")
            .default_open(false)
            .show(ui, |ui| {
                ui.label("Open the full tool collection in a resizable window.");
                if ui.button("Open Advanced Tools").clicked() {
                    self.show_advanced_tools = true;
                }
            });
    }

    fn reference_unwrap_tools(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Unwrap")
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Method");
                    egui::ComboBox::from_id_salt("reference_unfold_method")
                        .selected_text(match self.reference_unfold_method {
                            UnfoldMethod::Abf => "Angle Based (ABF)",
                            UnfoldMethod::Lscm => "Conformal (LSCM)",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.reference_unfold_method,
                                UnfoldMethod::Abf,
                                "Angle Based (ABF)",
                            );
                            ui.selectable_value(
                                &mut self.reference_unfold_method,
                                UnfoldMethod::Lscm,
                                "Conformal (LSCM)",
                            );
                        });
                });
                ui.add(
                    egui::Slider::new(&mut self.seam_angle, 1.0..=179.0)
                        .text("Seam angle")
                        .suffix("°"),
                );
                ui.checkbox(&mut self.reference_auto_seams, "Hard edges as seams")
                    .on_hover_text(
                        "Cut edges above the angle limit and open closed charts before unfolding",
                    );
                ui.checkbox(
                    &mut self.reference_preserve_orientation,
                    "Preserve UV orientation",
                );
                let width = ui.available_width();
                if ui
                    .add_sized(
                        [width, 36.0],
                        egui::Button::new(egui::RichText::new("Unwrap").color(Color32::BLACK))
                            .fill(theme::ORANGE),
                    )
                    .clicked()
                {
                    if self.reference_auto_seams {
                        self.execute(Command::AutoSeams {
                            islands: self.target_islands(),
                            angle_deg: self.seam_angle,
                            ensure_disk: true,
                        });
                    }
                    self.execute(Command::Unfold {
                        islands: if self.reference_auto_seams {
                            Vec::new()
                        } else {
                            self.target_islands()
                        },
                        method: self.reference_unfold_method,
                        preserve_orientation: self.reference_preserve_orientation,
                        border_weight: self.border_weight,
                        prevent_overlap: self.prevent_overlap,
                    });
                }
            });
    }

    fn reference_pack_tools(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Pack")
            .default_open(true)
            .show(ui, |ui| {
                egui::Grid::new("reference_pack_grid")
                    .num_columns(2)
                    .show(ui, |ui| {
                        ui.label("Resolution");
                        egui::ComboBox::from_id_salt("reference_pack_resolution")
                            .selected_text(self.resolution.to_string())
                            .show_ui(ui, |ui| {
                                for value in [512, 1024, 2048, 4096, 8192] {
                                    ui.selectable_value(
                                        &mut self.resolution,
                                        value,
                                        value.to_string(),
                                    );
                                }
                            });
                        ui.end_row();
                        ui.label("Padding");
                        ui.add(
                            egui::DragValue::new(&mut self.padding)
                                .range(0.0..=64.0)
                                .suffix(" px"),
                        );
                        ui.end_row();
                    });
                ui.checkbox(&mut self.pack_rotate, "Allow 90° rotation");
                ui.checkbox(&mut self.pack_rescale, "Fill available space");
                ui.checkbox(&mut self.pack_keep_groups, "Keep island groups");
                ui.checkbox(&mut self.pack_preserve_stacks, "Preserve stacks");
                let width = ui.available_width();
                if ui
                    .add_sized(
                        [width, 36.0],
                        egui::Button::new(egui::RichText::new("Pack").color(Color32::BLACK))
                            .fill(theme::ORANGE),
                    )
                    .clicked()
                {
                    self.pack();
                }
            });
    }

    fn reference_relax_tools(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Relax")
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.optimize_method, OptimizeMethod::Slim, "SLIM");
                    ui.selectable_value(&mut self.optimize_method, OptimizeMethod::Arap, "ARAP");
                });
                ui.add(egui::Slider::new(&mut self.border_weight, 0.0..=20.0).text("Keep border"));
                ui.checkbox(&mut self.prevent_overlap, "Prevent overlap");
                let width = ui.available_width();
                if ui
                    .add_sized(
                        [width, 36.0],
                        egui::Button::new(egui::RichText::new("Relax").color(Color32::BLACK))
                            .fill(theme::ORANGE),
                    )
                    .clicked()
                {
                    self.optimize();
                }
            });
    }

    fn reference_seam_tools(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Seams")
            .default_open(true)
            .show(ui, |ui| {
                let n_edges = self.target_edges().len();
                ui.columns(2, |columns| {
                    if columns[0]
                        .add_enabled(n_edges > 0, egui::Button::new("Mark Seam"))
                        .on_hover_text("Cut selected edges without changing the 3D mesh")
                        .clicked()
                    {
                        self.execute(Command::Cut {
                            edges: self.target_edges(),
                        });
                    }
                    if columns[1]
                        .add_enabled(n_edges > 0, egui::Button::new("Clear Seams"))
                        .on_hover_text("Join coincident UV sides without moving islands")
                        .clicked()
                    {
                        self.execute(Command::ClearSeams {
                            edges: self.target_edges(),
                        });
                    }
                });
                ui.weak(format!("{n_edges} selected edges"));
            });
    }

    fn reference_island_tools(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Island")
            .default_open(true)
            .show(ui, |ui| {
                let islands = self.target_islands();
                let selected_edges = self.target_edges();
                ui.columns(3, |columns| {
                    if columns[0]
                        .add_enabled(!selected_edges.is_empty(), egui::Button::new("Separate"))
                        .on_hover_text("Separate UVs along selected edges")
                        .clicked()
                    {
                        self.execute(Command::Cut {
                            edges: selected_edges.clone(),
                        });
                    }
                    if columns[1]
                        .add_enabled(!selected_edges.is_empty(), egui::Button::new("Merge"))
                        .on_hover_text("Align islands and weld selected seam edges")
                        .clicked()
                    {
                        self.execute(Command::Weld {
                            edges: selected_edges.clone(),
                        });
                    }
                    if columns[2]
                        .add_enabled(!selected_edges.is_empty(), egui::Button::new("Stitch"))
                        .on_hover_text("Join selected seam edges where UV sides already coincide")
                        .clicked()
                    {
                        self.execute(Command::ClearSeams {
                            edges: selected_edges.clone(),
                        });
                    }
                });
                ui.columns(2, |columns| {
                    if columns[0]
                        .add_enabled(islands.len() == 1, egui::Button::new("Straighten"))
                        .on_hover_text("Straighten one selected curved quad strip")
                        .clicked()
                    {
                        self.execute(Command::StraightenIsland { island: islands[0] });
                    }
                    if columns[1]
                        .add_enabled(
                            self.mode_for_view(1) == SelectMode::Edges && selected_edges.len() >= 2,
                            egui::Button::new("Align path"),
                        )
                        .on_hover_text("Align a selected open UV border path horizontally")
                        .clicked()
                    {
                        match self.selected_uv_edge_path(true) {
                            Ok((island, edges)) => self.execute(Command::AlignBorder {
                                island,
                                axis: UvAxis::U,
                                vertices: Vec::new(),
                                edges,
                            }),
                            Err(error) => self.set_error(error),
                        }
                    }
                });
            });
    }

    fn reference_island_detail_tools(&mut self, ui: &mut egui::Ui) {
        let selected_islands = self.target_islands();
        ui.collapsing("Stack and island layout", |ui| {
            ui.label("Select islands for local tools and scale operations.");
            if selected_islands.len() >= 2 {
                let reference = self.stack_reference
                    .filter(|island| selected_islands.contains(island))
                    .unwrap_or(selected_islands[0]);
                self.stack_reference = Some(reference);
                ui.horizontal(|ui| {
                    ui.label("Stack reference");
                    egui::ComboBox::from_id_salt("stack_reference")
                        .selected_text(format!("Island {reference}"))
                        .show_ui(ui, |ui| {
                            for &island in &selected_islands {
                                ui.selectable_value(&mut self.stack_reference,
                                    Some(island), format!("Island {island}"));
                            }
                        });
                });
                ui.checkbox(&mut self.stack_conform, "Conform to reference");
                if ui.button(if self.stack_preview_open { "Hide stack preview" }
                    else { "Preview stack" }).clicked() {
                    self.stack_preview_open = !self.stack_preview_open;
                }
                if self.stack_preview_open {
                    self.stack_preview_ui(ui, &selected_islands, reference);
                }
            }
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(selected_islands.len() >= 2, egui::Button::new("Find matching"))
                    .on_hover_text("Find islands with equivalent UV contours, including different triangulations")
                    .clicked()
                {
                    self.execute(Command::Similar {
                        islands: selected_islands.clone(),
                        mode: StackMode::Similar,
                        tolerance: 0.05,
                    });
                }
                if ui.add_enabled(selected_islands.len() >= 2, egui::Button::new("Stack matching"))
                    .on_hover_text("Align matching UV boundaries onto the selected reference island")
                    .clicked()
                {
                    self.execute(Command::Stack {
                        islands: selected_islands.clone(),
                        mode: StackMode::Similar,
                        tolerance: 0.05,
                        reference: self.stack_reference,
                        preview: false,
                        conform: self.stack_conform,
                        max_distortion: 4.0,
                    });
                }
                if ui.add_enabled(!selected_islands.is_empty(), egui::Button::new("Unstack"))
                    .clicked()
                {
                    self.execute(Command::Unstack { islands: selected_islands.clone() });
                }
            });
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(!selected_islands.is_empty(), egui::Button::new("Flip U"))
                    .on_hover_text("Mirror each selected island horizontally about its center")
                    .clicked()
                {
                    self.execute(Command::Mirror { islands: selected_islands.clone(), axis: UvAxis::U, at: None });
                }
                if ui.add_enabled(!selected_islands.is_empty(), egui::Button::new("Flip V"))
                    .on_hover_text("Mirror each selected island vertically about its center")
                    .clicked()
                {
                    self.execute(Command::Mirror { islands: selected_islands.clone(), axis: UvAxis::V, at: None });
                }
                if ui.add_enabled(selected_islands.len() == 1, egui::Button::new("Straighten island"))
                    .on_hover_text("Straighten a curved rectangular quad strip; other topologies report an error")
                    .clicked()
                {
                    self.execute(Command::StraightenIsland { island: selected_islands[0] });
                }
            });
            let selected_group = self.group_selection.and_then(|group| {
                self.session.document().zip(self.scene.as_ref()).and_then(|(doc, scene)| {
                    let name = doc.island_groups().get(group)?.name.clone();
                    let members = doc.island_groups().islands_in(group, &scene.islands).ok()?;
                    Some((group, name, members.len(), scene.islands.count().saturating_sub(members.len())))
                })
            });
            let can_recalculate = if self.group_selection.is_some() {
                selected_group.as_ref().is_some_and(|group| group.2 > 0)
            } else {
                !selected_islands.is_empty()
            };
            ui.add_enabled_ui(can_recalculate, |ui| {
                ui.menu_button("Average island scale", |ui| {
                    if let Some((group, name, count, others)) = &selected_group {
                        ui.label(format!("Selected group: {name} ({count} islands)"));
                        ui.weak("Match the group's aggregate texel density to other islands, keeping member proportions.");
                        if *others == 0 {
                            ui.weak("No other islands are available as a reference.");
                        }
                        if ui.add_enabled(*others > 0,
                            egui::Button::new("Confirm group recalculation")).clicked() {
                            self.execute(Command::AverageIslandGroupScale {
                                group: *group, resolution: self.resolution,
                            });
                            ui.close();
                        }
                    } else {
                        ui.label(format!("{} selected islands", selected_islands.len()));
                        ui.weak("Equalize their texel density while keeping combined UV area.");
                        if ui.button("Recalculate selected islands").clicked() {
                            self.execute(Command::AverageIslandScale {
                                islands: selected_islands.clone(), resolution: self.resolution,
                            });
                            ui.close();
                        }
                    }
                    if ui.button("Cancel").clicked() { ui.close(); }
                });
            });
        });
    }

    fn reference_transform_tools(&mut self, ui: &mut egui::Ui) {
        if self.group_selection.is_some() {
            self.reference_scale[1] = self.reference_scale[0];
        }
        egui::CollapsingHeader::new("Transform")
            .default_open(true)
            .show(ui, |ui| {
                ui.checkbox(
                    &mut self.gizmo_enabled,
                    "Show UV move, rotate and scale handles",
                )
                .on_hover_text(
                    "Select an island or group, then drag the colored handles in the UV editor",
                );
                let selected = self.group_selection.is_some() || !self.target_islands().is_empty();
                if selected {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(if self.group_selection.is_some() {
                            "Selected group"
                        } else {
                            "Selected islands"
                        });
                        for (label, angle, scale) in [
                            ("CCW 90°", -90.0, 1.0),
                            ("CW 90°", 90.0, 1.0),
                            ("90%", 0.0, 0.9),
                            ("110%", 0.0, 1.1),
                        ] {
                            if ui.small_button(label).clicked() {
                                self.execute(Command::Transform {
                                    islands: self.target_islands(),
                                    translate: [0.0; 2],
                                    rotate_deg: angle,
                                    scale: [scale; 2],
                                    pivot: Pivot::IslandCenter,
                                });
                            }
                        }
                    });
                }
                egui::Grid::new("reference_transform_grid")
                    .num_columns(3)
                    .show(ui, |ui| {
                        ui.label("Position");
                        ui.add(
                            egui::DragValue::new(&mut self.reference_position[0])
                                .speed(0.01)
                                .prefix("X "),
                        );
                        ui.add(
                            egui::DragValue::new(&mut self.reference_position[1])
                                .speed(0.01)
                                .prefix("Y "),
                        );
                        ui.end_row();
                        ui.label("Scale");
                        let scale_x = ui.add(
                            egui::DragValue::new(&mut self.reference_scale[0])
                                .range(0.001..=1000.0)
                                .speed(0.01)
                                .prefix("X "),
                        );
                        if self.group_selection.is_some() && scale_x.changed() {
                            self.reference_scale[1] = self.reference_scale[0];
                        }
                        ui.add_enabled(
                            self.group_selection.is_none(),
                            egui::DragValue::new(&mut self.reference_scale[1])
                                .range(0.001..=1000.0)
                                .speed(0.01)
                                .prefix("Y "),
                        );
                        ui.end_row();
                    });
                ui.horizontal(|ui| {
                    ui.label("Rotate");
                    ui.add_sized(
                        [180.0, 20.0],
                        egui::Slider::new(&mut self.reference_rotation, -180.0..=180.0)
                            .step_by(0.1)
                            .show_value(false),
                    )
                    .on_hover_text(
                        "Drag left for negative rotation; drag right for positive rotation",
                    );
                    ui.add(
                        egui::DragValue::new(&mut self.reference_rotation)
                            .speed(0.1)
                            .range(-3600.0..=3600.0)
                            .fixed_decimals(1)
                            .suffix("°"),
                    )
                    .on_hover_text("Type a precise angle, including negative values");
                });
                let changed = self.reference_position != [0.0; 2]
                    || self.reference_scale != [1.0; 2]
                    || self.reference_rotation != 0.0;
                if ui
                    .add_enabled(changed, egui::Button::new("Apply transform"))
                    .on_hover_text(
                        "Apply to selected islands, or all islands when none are selected",
                    )
                    .clicked()
                {
                    self.execute(Command::Transform {
                        islands: self.target_islands(),
                        translate: self.reference_position,
                        rotate_deg: self.reference_rotation,
                        scale: self.reference_scale,
                        pivot: Pivot::IslandCenter,
                    });
                    self.reference_position = [0.0; 2];
                    self.reference_scale = [1.0; 2];
                    self.reference_rotation = 0.0;
                }
            });
    }

    fn reference_visualization_tools(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Visualization").default_open(false).show(ui, |ui| {
            ui.label("Choose the display mode from the UV editor toolbar.");
            ui.checkbox(&mut self.wireframe, "Wireframe");
            ui.horizontal(|ui| {
                if ui.checkbox(&mut self.overlay_seams, "Seams").changed() {
                    self.gpu_dirty = true;
                }
                if ui
                    .checkbox(&mut self.overlay_borders, "Island borders")
                    .changed()
                {
                    self.gpu_dirty = true;
                }
            });
            ui.add(egui::Slider::new(&mut self.checker_cells, 2.0..=64.0).text("checker"));
            ui.horizontal(|ui| {
                if ui.checkbox(&mut self.grid_visible, "UV grid").changed() {
                    self.gpu_dirty = true;
                }
                if ui
                    .add(egui::Slider::new(&mut self.grid_divisions, 2..=64).text("divisions"))
                    .changed()
                {
                    self.gpu_dirty = true;
                }
            });
            ui.horizontal(|ui| {
                if ui.button("Load texture PNG…").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("PNG image", &["png"])
                        .pick_file()
                    && let (Some(rs), Some(renderer)) = (&self.rs, &mut self.renderer)
                {
                    match renderer.set_custom_texture(rs, &path) {
                        Ok(()) => {
                            self.texture_path = Some(path);
                            self.color_mode = ColorMode::Texture;
                            self.gpu_dirty = true;
                        }
                        Err(e) => self.set_error(format!("texture: {e}")),
                    }
                }
                if ui
                    .add_enabled(
                        self.texture_path.is_some(),
                        egui::Button::new("Clear texture"),
                    )
                    .clicked()
                {
                    self.texture_path = None;
                    if self.color_mode == ColorMode::Texture {
                        self.color_mode = ColorMode::Checker;
                    }
                    self.gpu_dirty = true;
                }
            });
            if self.color_mode == ColorMode::Flipped {
                ui.label(
                    "Green: normal · Orange: mirrored island · Red: local flip · Purple: degenerate",
                );
            }
            if self.color_mode == ColorMode::Overlaps {
                ui.label(
                    "Green: clear · Red: overlaps another island · Orange: self overlap · Purple: both",
                );
            }
            if self.color_mode == ColorMode::TexelDensity {
                ui.label("Blue: below target · Green: target · Red: above target · Purple: invalid");
                ui.horizontal(|ui| {
                    ui.label("Target px/unit");
                    if ui
                        .add(egui::DragValue::new(&mut self.density).range(0.001..=f64::MAX))
                        .changed()
                    {
                        self.gpu_dirty = true;
                    }
                });
            }
            if self.color_mode == ColorMode::Distortion {
                ui.horizontal(|ui| {
                    ui.label("UV Distortion");
                    if ui.small_button("Off").clicked() {
                        self.color_mode = ColorMode::Checker;
                        self.gpu_dirty = true;
                    }
                    egui::ComboBox::from_id_salt("distortion_metric")
                        .selected_text(match self.distortion_metric {
                            DistortionMetric::Area => "Area",
                            DistortionMetric::Conformal => "Conformal",
                            DistortionMetric::Directional => "Directional",
                            DistortionMetric::Combined => "Combined",
                        }).show_ui(ui, |ui| {
                            for (metric, label) in [
                                (DistortionMetric::Area, "Area"),
                                (DistortionMetric::Conformal, "Conformal"),
                                (DistortionMetric::Directional, "Directional"),
                                (DistortionMetric::Combined, "Combined"),
                            ] {
                                if ui.selectable_value(&mut self.distortion_metric, metric, label).changed() {
                                    self.gpu_dirty = true;
                                }
                            }
                        });
                    egui::ComboBox::from_id_salt("distortion_reference")
                        .selected_text(match self.distortion_reference {
                            DistortionReference::Object => "Object",
                            DistortionReference::Selected => "Selected / group",
                            DistortionReference::Island => "Individual island",
                            DistortionReference::TexelTarget => "Texel target",
                            DistortionReference::Raw => "Raw scale",
                        }).show_ui(ui, |ui| {
                            for (reference, label) in [
                                (DistortionReference::Object, "Object"),
                                (DistortionReference::Selected, "Selected / group"),
                                (DistortionReference::Island, "Individual island"),
                                (DistortionReference::TexelTarget, "Texel target"),
                                (DistortionReference::Raw, "Raw scale"),
                            ] {
                                if ui.selectable_value(&mut self.distortion_reference, reference, label).changed() {
                                    self.gpu_dirty = true;
                                }
                            }
                        });
                });
                if ui.add(egui::Slider::new(&mut self.distortion_sensitivity, 0.25..=4.0)
                    .text("Sensitivity")).changed() { self.gpu_dirty = true; }
                ui.add(egui::Slider::new(&mut self.distortion_opacity, 0.0..=1.0)
                    .text("Heatmap opacity"));
                ui.checkbox(&mut self.distortion_3d, "Show in 3D");
                if self.distortion_reference == DistortionReference::TexelTarget {
                    ui.horizontal(|ui| {
                        ui.label("Target px/unit");
                        if ui.add(egui::DragValue::new(&mut self.density).range(0.001..=f64::MAX)).changed() {
                            self.gpu_dirty = true;
                        }
                    });
                }
                let selected = self.distortion_config().selected_islands;
                if !selected.is_empty() {
                    let active = self.distortion_active_island
                        .filter(|island| selected.contains(island)).unwrap_or(selected[0]);
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut self.distortion_isolate, "Isolate active island").changed() {
                            self.gpu_dirty = true;
                        }
                        egui::ComboBox::from_id_salt("distortion_active_island")
                            .selected_text(format!("Island {active}"))
                            .show_ui(ui, |ui| {
                                for island in selected {
                                    if ui.selectable_value(&mut self.distortion_active_island,
                                        Some(island), format!("Island {island}")).changed() {
                                        self.gpu_dirty = true;
                                    }
                                }
                            });
                    });
                }
                let limit = 2.0_f64.powf(1.0 / self.distortion_sensitivity);
                match self.distortion_metric {
                    DistortionMetric::Area => ui.label(format!(
                        "Blue: UV area ≤ {:.3}× · Green: 1× · Yellow: {:.3}× · Red: ≥ {:.3}×",
                        1.0 / limit, limit.sqrt(), limit)),
                    DistortionMetric::Directional => ui.label(format!(
                        "Blue: UV compression ≤ {:.3}× · Green: 1× · Red: expansion ≥ {:.3}×",
                        1.0 / limit, limit)),
                    DistortionMetric::Conformal => ui.label(format!(
                        "Green: σ₁/σ₂ = 1 · Yellow: {:.3} · Orange: {:.3} · Red: ≥ {:.3}",
                        limit.powf(0.33), limit.powf(0.66), limit)),
                    DistortionMetric::Combined => ui.label(format!(
                        "Green: D = 0 · Yellow: {:.3} · Orange: {:.3} · Red: ≥ {:.3}",
                        limit.ln() * 0.33, limit.ln() * 0.66, limit.ln())),
                };
                ui.label("UV area convention: blue = less UV per surface (texture enlarged); red = more UV per surface. Magenta = inverted or degenerate triangle.");
                if let Some(scene) = &self.scene {
                    ui.label(format!("Invalid UV triangles: {} · Flipped UV triangles: {}",
                        scene.report.degenerate_triangles, scene.report.flipped_triangles));
                }
            }
        });
    }

    fn reference_uv_sets(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("UV Sets")
            .default_open(false)
            .show(ui, |ui| {
                let Some((active, channels)) =
                    self.session.document().and_then(|d| d.uv_channel_info())
                else {
                    ui.weak("This mesh has one UV set");
                    return;
                };
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("reference_uv_channel")
                        .selected_text(format!("UV{active}"))
                        .show_ui(ui, |ui| {
                            for &channel in &channels {
                                if ui
                                    .selectable_label(channel == active, format!("UV{channel}"))
                                    .clicked()
                                    && channel != active
                                {
                                    self.execute(Command::SwitchUvChannel { channel });
                                }
                            }
                        });
                    ui.add(
                        egui::DragValue::new(&mut self.new_uv_channel)
                            .range(0..=15)
                            .prefix("New "),
                    );
                    if ui
                        .add_enabled(
                            !channels.contains(&self.new_uv_channel),
                            egui::Button::new("Create copy"),
                        )
                        .clicked()
                    {
                        self.execute(Command::CreateUvChannel {
                            channel: self.new_uv_channel,
                        });
                    }
                });
            });
    }

    fn tools(&mut self, ui: &mut egui::Ui) {
        let has_doc = self.session.document().is_some();
        ui.add_enabled_ui(has_doc, |ui| {
            if let Some((active, channels)) = self.session.document().and_then(|d| d.uv_channel_info()) {
                ui.heading("UV channels");
                ui.label(format!("Active: TEXCOORD_{active}"));
                let mut action = None;
                ui.horizontal_wrapped(|ui| {
                    for &channel in &channels {
                        if ui.selectable_label(channel == active, format!("{channel}")).clicked() && channel != active {
                            action = Some(Command::SwitchUvChannel { channel });
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("New");
                    ui.add(egui::DragValue::new(&mut self.new_uv_channel).range(0..=15));
                    if ui.add_enabled(!channels.contains(&self.new_uv_channel), egui::Button::new("Create copy")).clicked() {
                        action = Some(Command::CreateUvChannel { channel: self.new_uv_channel });
                    }
                });
                if self.swap_uv_channel == active {
                    self.swap_uv_channel = channels.iter().copied().find(|&c| c != active).unwrap_or(active);
                }
                ui.horizontal(|ui| {
                    ui.label("Swap with");
                    egui::ComboBox::from_id_salt("swap_uv_channel")
                        .selected_text(format!("{}", self.swap_uv_channel))
                        .show_ui(ui, |ui| {
                            for &channel in &channels {
                                if channel != active { ui.selectable_value(&mut self.swap_uv_channel, channel, format!("{channel}")); }
                            }
                        });
                    if ui.add_enabled(self.swap_uv_channel != active, egui::Button::new("Swap")).clicked() {
                        action = Some(Command::SwapUvChannels { a: active, b: self.swap_uv_channel });
                    }
                });
                if let Some(command) = action { self.execute(command); }
                ui.separator();
            }
            ui.heading("Active view selection");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.mode, SelectMode::Vertices, "Verts");
                ui.selectable_value(&mut self.mode, SelectMode::Edges, "Edges");
                ui.selectable_value(&mut self.mode, SelectMode::Faces, "Faces");
                ui.selectable_value(&mut self.mode, SelectMode::Islands, "Islands");
                ui.selectable_value(&mut self.mode, SelectMode::Objects, "Objects");
            });
            ui.horizontal(|ui| {
                if ui.selectable_label(!self.brush && !self.lasso, "Box").on_hover_text("Left-drag selects with a rectangle").clicked() {
                    self.brush = false; self.lasso = false; self.density_brush = false;
                }
                if ui.selectable_label(!self.brush && self.lasso, "Lasso").on_hover_text("Left-drag selects with a free-hand shape").clicked() {
                    self.brush = false; self.lasso = true; self.density_brush = false;
                }
                if ui.selectable_label(self.brush, "Brush").on_hover_text("Paint a circular selection in either viewport").clicked() {
                    self.brush = true;
                    self.seam_brush = false;
                    self.uv_edit_mode = None;
                    self.density_brush = false;
                }
                ui.toggle_value(&mut self.xray, "X-ray")
                    .on_hover_text("3D drag selection also takes hidden faces and edges");
            });
            if self.brush {
                ui.horizontal(|ui| {
                        ui.add(egui::Slider::new(&mut self.brush_radius, 4.0..=128.0).text("Radius pt"));
                    ui.selectable_value(&mut self.brush_erase, false, "Add");
                    ui.selectable_value(&mut self.brush_erase, true, "Remove");
                });
            }
            ui.horizontal(|ui| {
                ui.label("UV edit");
                for (label, mode) in [("Move", BrushMode::Move), ("Smooth", BrushMode::Smooth), ("Relax", BrushMode::Relax)] {
                    if ui.selectable_label(self.uv_edit_mode == Some(mode), label).clicked() {
                        self.uv_edit_mode = (self.uv_edit_mode != Some(mode)).then_some(mode);
                        self.uv_edit_drag = None;
                        if self.uv_edit_mode.is_some() { self.brush = false; self.seam_brush = false; self.density_brush = false; }
                    }
                }
            });
            if self.uv_edit_mode.is_some() {
                ui.add(egui::Slider::new(&mut self.brush_radius, 4.0..=128.0).text("Radius pt"));
                ui.add(egui::Slider::new(&mut self.uv_edit_strength, 0.01..=1.0).text("Strength"));
                ui.label("Paint UV vertices; one undo step per stroke. Pins and locked faces stay fixed.");
            }
            ui.label(
                egui::RichText::new(match self.mode {
                    SelectMode::Islands | SelectMode::Faces | SelectMode::Objects => {
                        "Click/drag: select · Shift: add · Ctrl+drag: remove"
                    }
                    SelectMode::Edges => {
                        "Click: select edge · Shift+click: edge path · Ctrl+click: vertex path · double-click: loop"
                    }
                    SelectMode::Vertices => {
                        "Click/drag: select · Shift: add · Ctrl+click: path from last"
                    }
                })
                .small()
                .weak(),
            );
            ui.horizontal(|ui| {
                if ui.button("All").on_hover_text(self.hotkeys.extras[1].display()).clicked() {
                    self.select_all();
                }
                if ui.button("Invert").on_hover_text(self.hotkeys.extras[2].display()).clicked() {
                    self.invert_selection();
                }
                if ui.button("Clear").on_hover_text("Esc").clicked() {
                    self.clear_selection();
                }
                if ui.button("Grow").on_hover_text(self.hotkeys.extras[3].display()).clicked() {
                    self.grow_selection(true);
                }
                if ui.button("Shrink").on_hover_text(self.hotkeys.extras[4].display()).clicked() {
                    self.grow_selection(false);
                }
            });
            ui.collapsing("Select by OBJ/glTF metadata", |ui| {
                ui.horizontal(|ui| {
                    ui.label("Material");
                    ui.text_edit_singleline(&mut self.selection_material);
                });
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.selection_material.trim().is_empty(), egui::Button::new("Select material")).clicked() {
                        self.execute(Command::SelectFacesByMaterial {
                            material: Some(self.selection_material.trim().to_owned()),
                        });
                    }
                    if ui.button("Unassigned").clicked() {
                        self.execute(Command::SelectFacesByMaterial { material: None });
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("OBJ smoothing group");
                    ui.add(egui::DragValue::new(&mut self.selection_smoothing_group).range(1..=u32::MAX));
                    if ui.button("Select group").clicked() {
                        self.execute(Command::SelectFacesBySmoothingGroup {
                            group: Some(self.selection_smoothing_group),
                        });
                    }
                    if ui.button("Off").clicked() {
                        self.execute(Command::SelectFacesBySmoothingGroup { group: None });
                    }
                });
            });
            ui.horizontal(|ui| {
                let seed = angle_selection_seed(self.mode, &self.sel_faces, &self.hidden);
                if ui
                    .add_enabled(seed.is_some(), egui::Button::new("Select connected ≤"))
                    .on_hover_text("Select faces connected to the first selected face within this 3D angle")
                    .clicked()
                    && let Some(seed_face) = seed
                {
                    self.execute(Command::SelectFacesByAngle {
                        seed_face,
                        max_angle_deg: self.select_angle,
                    });
                }
                ui.add(
                    egui::DragValue::new(&mut self.select_angle)
                        .range(0.0..=180.0)
                        .suffix("°"),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Convert to");
                for (label, to) in [
                    ("V", SelectMode::Vertices),
                    ("E", SelectMode::Edges),
                    ("F", SelectMode::Faces),
                    ("I", SelectMode::Islands),
                    ("O", SelectMode::Objects),
                ] {
                    if ui
                        .add_enabled(to != self.mode, egui::Button::new(label))
                        .on_hover_text("Convert the selection to this mode (touching elements)")
                        .clicked()
                    {
                        self.convert_selection(to);
                    }
                }
            });
            ui.collapsing("Saved selections", |ui| {
                ui.label("Sets are saved per mesh and restored when the same topology is reopened.");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.selection_set_name);
                    if ui.button("Save / update").clicked() {
                        self.save_selection_set();
                    }
                });
                let mut recall = None;
                let mut delete = None;
                for (index, set) in self.selection_sets.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.button(format!("{} ({:?})", set.name, set.mode)).clicked() {
                            recall = Some(index);
                        }
                        if ui.small_button("×").on_hover_text("Delete set").clicked() {
                            delete = Some(index);
                        }
                    });
                }
                if let Some(index) = recall { self.recall_selection_set(index); }
                if let Some(index) = delete {
                    self.selection_sets.remove(index);
                    self.sync_selection_sets();
                }
            });
            ui.collapsing("Locks", |ui| {
                let (faces, islands) = self.lock_targets();
                let (selected_faces_locked, selected_islands_locked, all_faces_locked, all_islands_locked, locked_island_ids, locked_face_ids) =
                    match (self.session.document(), &self.scene) {
                        (Some(doc), Some(scene)) => {
                            let face_flags = doc.locked_faces();
                            let island_flags = doc.locked_island_faces();
                            let is_locked = |island: usize| scene.islands.faces(island)
                                .iter().any(|&face| island_flags[face as usize]);
                            let locked_island_ids: Vec<usize> = (0..scene.islands.count())
                                .filter(|&island| is_locked(island)).collect();
                            let locked_face_ids: Vec<usize> = face_flags.iter().enumerate()
                                .filter_map(|(face, &locked)| locked.then_some(face)).collect();
                            (
                                faces.iter().filter(|&&face| face_flags[face]).count(),
                                islands.iter().filter(|&&island| is_locked(island)).count(),
                                locked_face_ids.len(),
                                locked_island_ids.len(),
                                locked_island_ids,
                                locked_face_ids,
                            )
                        }
                        _ => (0, 0, 0, 0, Vec::new(), Vec::new()),
                    };
                ui.label(format!(
                    "Selection: {selected_islands_locked}/{} islands, {selected_faces_locked}/{} faces locked",
                    islands.len(), faces.len(),
                ));
                ui.horizontal(|ui| {
                    if ui.add_enabled(!islands.is_empty(), egui::Button::new("Lock islands")).clicked() {
                        self.execute(Command::LockIslands { islands: islands.clone(), locked: true });
                    }
                    if ui.add_enabled(!islands.is_empty(), egui::Button::new("Unlock islands")).clicked() {
                        self.execute(Command::LockIslands { islands: islands.clone(), locked: false });
                    }
                });
                ui.horizontal(|ui| {
                    if ui.add_enabled(!faces.is_empty(), egui::Button::new("Lock faces")).clicked() {
                        self.execute(Command::LockFaces { faces: faces.clone(), locked: true });
                    }
                    if ui.add_enabled(!faces.is_empty(), egui::Button::new("Unlock faces")).clicked() {
                        self.execute(Command::LockFaces { faces: faces.clone(), locked: false });
                    }
                });
                if ui.add_enabled(all_faces_locked + all_islands_locked > 0, egui::Button::new("Clear all locks")).clicked() {
                    self.execute(Command::ClearLocks);
                }
                if all_faces_locked + all_islands_locked > 0 {
                    ui.weak(format!("Total: {all_islands_locked} islands, {all_faces_locked} faces"));
                    ui.collapsing("Locked IDs", |ui| {
                        ui.label(format!("Islands: {locked_island_ids:?}"));
                        ui.label(format!("Faces: {locked_face_ids:?}"));
                    });
                }
            });
            ui.horizontal(|ui| {
                if ui
                    .button("Loop")
                    .on_hover_text("Edge loop through the selected edges (L, double-click)")
                    .clicked()
                {
                    self.select_loops(false);
                }
                if ui
                    .button("Ring")
                    .on_hover_text(format!("Edge ring through the selected edges ({})", self.hotkeys.extras[5].display()))
                    .clicked()
                {
                    self.select_loops(true);
                }
                if ui
                    .button("Open")
                    .on_hover_text("Select the open edges of the mesh")
                    .clicked()
                {
                    self.select_borders(false);
                }
                if ui
                    .button("Borders")
                    .on_hover_text("Select island borders (of the selected islands, or all)")
                    .clicked()
                {
                    self.select_borders(true);
                }
            });
            ui.horizontal(|ui| {
                if ui.button("Hide").on_hover_text("H").clicked() {
                    self.set_hidden(Some(false));
                }
                if ui
                    .button("Isolate")
                    .on_hover_text(format!("Hide everything else ({})", self.hotkeys.extras[8].display()))
                    .clicked()
                {
                    self.set_hidden(Some(true));
                }
                let n_hidden = self.hidden.iter().filter(|&&h| h).count();
                if ui
                    .add_enabled(n_hidden > 0, egui::Button::new("Unhide"))
                    .on_hover_text(self.hotkeys.extras[7].display())
                    .clicked()
                {
                    self.set_hidden(None);
                }
                if n_hidden > 0 {
                    ui.weak(format!("{n_hidden} hidden"));
                }
            });
            ui.menu_button("Select islands with issue", |ui| {
                for (label, problem) in [
                    ("Overlapping", IslandProblem::Overlapping),
                    ("Self overlap", IslandProblem::SelfOverlapping),
                    ("Flipped", IslandProblem::Flipped),
                    ("Outside tile", IslandProblem::OutsideTile),
                    ("Crosses tile", IslandProblem::CrossesTile),
                ] {
                    if ui.button(label).clicked() {
                        self.execute(Command::SelectIslands { problem });
                        ui.close();
                    }
                }
            });
            ui.collapsing("Select islands by metric", |ui| {
                egui::ComboBox::from_id_salt("island_metric")
                    .selected_text(match self.metric {
                        IslandMetric::TexelDensity => "Texel density",
                        IslandMetric::MeanAngleDistortion => "Mean angle distortion",
                        IslandMetric::MaxAngleDistortion => "Max angle distortion",
                        IslandMetric::MeanAreaDistortion => "Mean area distortion",
                        IslandMetric::MaxAreaDistortion => "Max area distortion",
                    })
                    .show_ui(ui, |ui| {
                        for (metric, label) in [
                            (IslandMetric::TexelDensity, "Texel density"),
                            (IslandMetric::MeanAngleDistortion, "Mean angle distortion"),
                            (IslandMetric::MaxAngleDistortion, "Max angle distortion"),
                            (IslandMetric::MeanAreaDistortion, "Mean area distortion"),
                            (IslandMetric::MaxAreaDistortion, "Max area distortion"),
                        ] {
                            ui.selectable_value(&mut self.metric, metric, label);
                        }
                    });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.metric_use_min, "Min");
                    ui.add_enabled(self.metric_use_min, egui::DragValue::new(&mut self.metric_min).speed(0.01));
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.metric_use_max, "Max");
                    ui.add_enabled(self.metric_use_max, egui::DragValue::new(&mut self.metric_max).speed(0.01));
                });
                if ui.add_enabled(self.metric_use_min || self.metric_use_max, egui::Button::new("Select matching")).clicked() {
                    self.execute(Command::SelectIslandsByMetric {
                        metric: self.metric,
                        min: self.metric_use_min.then_some(self.metric_min),
                        max: self.metric_use_max.then_some(self.metric_max),
                        resolution: self.resolution,
                    });
                }
            });
            ui.horizontal(|ui| {
                if ui
                    .button("Seams")
                    .on_hover_text("Select all seam edges")
                    .clicked()
                    && let Some(doc) = self.session.document()
                {
                    for e in 0..doc.mesh.edge_count() {
                        self.sel_edges[e] = doc.uv().edge_kind(&doc.mesh, e) == EdgeKind::Seam;
                    }
                    self.mode = SelectMode::Edges;
                    self.gpu_dirty = true;
                }
                if ui
                    .button("Sharp ≥")
                    .on_hover_text("Select edges sharper than the angle")
                    .clicked()
                    && let Some(doc) = self.session.document()
                {
                    self.sel_edges.iter_mut().for_each(|s| *s = false);
                    for e in seams::sharp_edges(&doc.mesh, self.select_angle) {
                        self.sel_edges[e] = true;
                    }
                    self.mode = SelectMode::Edges;
                    self.gpu_dirty = true;
                }
                if ui
                    .button("Flat ≤")
                    .on_hover_text("Grow the selected faces across edges flatter than the angle")
                    .clicked()
                    && let Some(doc) = self.session.document()
                {
                    self.sel_faces =
                        select::faces_by_angle(&doc.mesh, &self.sel_faces, self.select_angle);
                    self.mode = SelectMode::Faces;
                    self.gpu_dirty = true;
                }
                ui.add(
                    egui::DragValue::new(&mut self.select_angle)
                        .range(1.0..=179.0)
                        .suffix("°"),
                );
            });

            ui.separator();
            ui.heading("Seams");
            ui.horizontal(|ui| {
                if ui.toggle_value(&mut self.seam_brush, "Seam brush (3D)") .changed() {
                    self.brush = false;
                    if self.seam_brush { self.uv_edit_mode = None; self.uv_edit_drag = None; self.density_brush = false; self.density_drag = None; }
                    self.seam_brush_edges.fill(false);
                    self.seam_brush_last = None;
                    self.gpu_dirty = true;
                }
                if self.seam_brush { ui.label("Drag to cut; one undo step per stroke"); }
            });
            if self.seam_brush {
                ui.add(egui::Slider::new(&mut self.brush_radius, 4.0..=128.0).text("Radius pt"));
            }
            let n_edges = self.sel_edges.iter().filter(|&&s| s).count();
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(n_edges > 0, egui::Button::new(format!("Mark Seam / Cut ({})", self.hotkeys.keys[1])))
                    .clicked()
                {
                    self.execute(Command::Cut {
                        edges: self.target_edges(),
                    });
                }
                if ui
                    .add_enabled(n_edges > 0, egui::Button::new("Clear Seam"))
                    .on_hover_text("Join coincident UV seam edges without moving islands; use Weld when the sides are apart")
                    .clicked()
                {
                    self.execute(Command::ClearSeams {
                        edges: self.target_edges(),
                    });
                }
                if ui
                    .add_enabled(n_edges > 0, egui::Button::new(format!("Weld ({})", self.hotkeys.keys[2])))
                    .on_hover_text("Align islands, then merge the selected seam edges")
                    .clicked()
                {
                    self.execute(Command::Weld {
                        edges: self.target_edges(),
                    });
                }
                if ui
                    .add_enabled(n_edges > 0, egui::Button::new("Cut + unfold"))
                    .on_hover_text(format!("Cut, then unfold the islands the cut touches ({})", self.hotkeys.extras[10].display()))
                    .clicked()
                {
                    self.execute(Command::CutUnfold {
                        edges: self.target_edges(),
                    });
                }
                ui.label(format!("{n_edges} edges"));
            });
            let n_faces = self.sel_faces.iter().filter(|&&s| s).count();
            if ui
                .add_enabled(n_faces > 0, egui::Button::new("Cut selection border"))
                .on_hover_text("Cut between the selected faces and the others")
                .clicked()
            {
                self.execute(Command::CutBorder {
                    faces: select::indices(&self.sel_faces),
                    unfold: false,
                });
            }
            ui.horizontal(|ui| {
                if ui
                    .button("Auto seams")
                    .on_hover_text(
                        "Cut sharp edges, then cut each island until it can be flattened",
                    )
                    .clicked()
                {
                    self.execute(Command::AutoSeams {
                        islands: self.target_islands(),
                        angle_deg: self.seam_angle,
                        ensure_disk: true,
                    });
                }
                ui.add(
                    egui::DragValue::new(&mut self.seam_angle)
                        .range(1.0..=179.0)
                        .suffix("°"),
                );
            });
            if ui.button("Box / Planar seams")
                .on_hover_text("Cut charts along six dominant directions and smooth the chart labels")
                .clicked() {
                self.execute(Command::BoxPlanarSeams {
                    islands: self.target_islands(),
                    smoothness: 0.25,
                    iterations: 10,
                });
            }
            if ui.button("Pelt organic shape")
                .on_hover_text("Grow geodesic seam branches, open charts into disks, then unfold and relax")
                .clicked() {
                self.execute(Command::Pelt {
                    islands: self.target_islands(),
                    branches: 6,
                    unfold: true,
                    relax_iterations: 20,
                });
            }
            if ui.button("Cut to reduce stretch")
                .on_hover_text("Iteratively add up to 10 seams where measured UV distortion is highest, stopping near 1.1")
                .clicked() {
                self.execute(Command::CutReduceStretch {
                    islands: self.target_islands(),
                    max_cuts: 10,
                    target_distortion: 1.1,
                });
            }

            ui.separator();
            ui.heading("Unwrap");
            ui.label(
                egui::RichText::new("Applies to selected islands, or all")
                    .small()
                    .weak(),
            );
            ui.horizontal(|ui| {
                ui.label("Project:");
                for (label, mode, tip) in [
                    ("Box", Projection::Box, "Each face along its dominant axis"),
                    (
                        "Best",
                        Projection::Best,
                        "Each island along its average normal",
                    ),
                ] {
                    if ui.button(label).on_hover_text(tip).clicked() {
                        self.project(mode);
                    }
                }
                if ui
                    .button("Camera ortho")
                    .on_hover_text("Orthographic projection using the current 3D view orientation")
                    .clicked()
                {
                    let size = self.view_sizes[0];
                    let camera = self
                        .cam3
                        .orthographic_projection(size.x / size.y.max(1.0));
                    self.project(Projection::CameraOrthographic(camera));
                }
                if ui
                    .button("Camera perspective")
                    .on_hover_text("Project through the current 3D perspective view; selected geometry must be inside its near/far depth range")
                    .clicked()
                {
                    let size = self.view_sizes[0];
                    let camera = self
                        .cam3
                        .perspective_projection(size.x / size.y.max(1.0));
                    self.project(Projection::CameraPerspective(camera));
                }
            });
            egui::Grid::new("projections").show(ui, |ui| {
                type Make = fn(Axis) -> Projection;
                let rows: [(&str, Make, &str); 3] = [
                    ("Planar", Projection::Planar, "Along a world axis"),
                    (
                        "Cylinder",
                        Projection::Cylindrical,
                        "Unroll each island around an axis",
                    ),
                    (
                        "Sphere",
                        Projection::Spherical,
                        "Longitude/latitude around an axis",
                    ),
                ];
                for (name, make, tip) in rows {
                    ui.label(name).on_hover_text(tip);
                    ui.horizontal(|ui| {
                        for (label, axis) in [("X", Axis::X), ("Y", Axis::Y), ("Z", Axis::Z)] {
                            if ui.button(label).on_hover_text(tip).clicked() {
                                self.project(make(axis));
                            }
                        }
                    });
                    ui.end_row();
                }
            });
            if ui
                .button(format!("Unfold ({})", self.hotkeys.keys[3]))
                .on_hover_text("LSCM: conformal flattening")
                .clicked()
            {
                self.execute(Command::Unfold {
                    islands: self.target_islands(),
                    method: Default::default(),
                    preserve_orientation: false,
                    border_weight: self.border_weight,
                    prevent_overlap: self.prevent_overlap,
                });
            }
            ui.horizontal(|ui| {
                if ui
                    .button(format!("Optimize ({})", self.hotkeys.keys[5]))
                    .on_hover_text("Relax after Unfold: much less area distortion")
                    .clicked()
                {
                    self.optimize();
                }
                ui.selectable_value(&mut self.optimize_method, OptimizeMethod::Arap, "ARAP")
                    .on_hover_text("As-rigid-as-possible (rare flips possible)");
                ui.selectable_value(&mut self.optimize_method, OptimizeMethod::Slim, "SLIM")
                    .on_hover_text("Symmetric Dirichlet: guaranteed flip-free");
            });
            ui.add(
                egui::Slider::new(&mut self.border_weight, 0.0..=20.0)
                    .text("Keep border shape")
                    .logarithmic(false),
            )
            .on_hover_text("Softly preserve border edge lengths and shape during Unfold and Optimize (0 disables)");
            ui.checkbox(&mut self.prevent_overlap, "Prevent overlap")
                .on_hover_text("Reject overlapping UV results from Unfold and Optimize");
            ui.horizontal(|ui| {
                let tip = "Edges mode: ends of selected edges. Islands mode: all their vertices.\n\
                           Unfold keeps pins in place; 2+ pins fix an island's scale.";
                if ui.button("Pin").on_hover_text(tip).clicked() {
                    self.pin_selection(true);
                }
                if ui.button("Unpin").on_hover_text(tip).clicked() {
                    self.pin_selection(false);
                }
                if ui.button("Clear pins").clicked() {
                    self.execute(Command::ClearPins);
                }
                let n = self
                    .session
                    .document()
                    .map_or(0, |d| d.pins().iter().filter(|&&p| p).count());
                ui.label(format!("{n}"));
            });
            ui.label(egui::RichText::new("Drag a red pin in the UV view to relax its island live; release for one undo step.").small().weak());

            ui.separator();
            ui.heading("Layout");
            egui::Grid::new("layout").num_columns(2).show(ui, |ui| {
                ui.label("Resolution");
                egui::ComboBox::from_id_salt("res")
                    .selected_text(self.resolution.to_string())
                    .show_ui(ui, |ui| {
                        for r in [512, 1024, 2048, 4096, 8192] {
                            ui.selectable_value(&mut self.resolution, r, r.to_string());
                        }
                    });
                ui.end_row();
                ui.label("Padding");
                ui.add(
                    egui::DragValue::new(&mut self.padding)
                        .range(0.0..=64.0)
                        .suffix(" px"),
                );
                ui.end_row();
                ui.label("Margin");
                ui.add(
                    egui::DragValue::new(&mut self.margin)
                        .range(0.0..=64.0)
                        .suffix(" px"),
                );
                ui.end_row();
                ui.label("Method");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.pack_method, PackMethod::Raster, "Raster")
                        .on_hover_text("Bitmask packing: fills holes and concavities");
                    ui.selectable_value(&mut self.pack_method, PackMethod::Shelf, "Shelf")
                        .on_hover_text("Bounding boxes on shelves: fastest");
                    ui.selectable_value(&mut self.pack_method, PackMethod::Efficient, "Efficient")
                        .on_hover_text("Try three raster placement orders and keep the best fit");
                });
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut self.pack_rotate, "Rotate 90°")
                    .on_hover_text("Raster: try 4 rotations. Shelf: lay islands flat.");
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut self.pack_rescale, "Rescale").on_hover_text(
                    "On: scale islands to fill the space.\nOff: keep their size (shrink only if needed).\n\
                     With a selection, only it is packed, around the other islands.",
                );
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut self.pack_keep_density, "Keep target TD")
                    .on_hover_text("Pack using the same texel density target shown below and in the heatmap");
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut self.pack_keep_groups, "Keep groups together")
                    .on_hover_text("Pack each complete, editable root island group as one rigid layout item");
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut self.pack_preserve_stacks, "Preserve stacks")
                    .on_hover_text("Keep registered stacked islands together; disable if a stack crosses a packing group");
                ui.end_row();
                ui.label("UDIM");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.pack_udim, "Tiles");
                    ui.add(egui::DragValue::new(&mut self.pack_tiles).range(1..=1000));
                    egui::ComboBox::from_id_salt("pack_tile_distribution")
                        .selected_text(match self.pack_distribution {
                            None => "Any",
                            Some(TileDistribution::Group) => "Group",
                            Some(TileDistribution::Material) => "Material",
                            Some(TileDistribution::Object) => "Object",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.pack_distribution, None, "Any");
                            ui.selectable_value(&mut self.pack_distribution, Some(TileDistribution::Group), "Group");
                            ui.selectable_value(&mut self.pack_distribution, Some(TileDistribution::Material), "Material");
                            ui.selectable_value(&mut self.pack_distribution, Some(TileDistribution::Object), "Object");
                        });
                });
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut self.pack_pixel_aligned, "Pixel aligned")
                    .on_hover_text("Align packed UVs to pixel centers at the chosen resolution");
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut self.pack_padding_perfect, "Padding Perfect")
                    .on_hover_text("Keep the requested pixel gap after raster placement");
                ui.end_row();
            });
            if let Some(doc) = self.session.document() {
                ui.label(format!("{} registered stack group(s)", doc.stack_group_count()));
            }
            if ui.button(format!("Pack ({})", self.hotkeys.keys[4])).clicked() {
                self.pack();
            }
            let has_edge_path = self.mode_for_view(1) == SelectMode::Edges
                && self.target_edges().len() >= 2;
            ui.collapsing("Edge path tools", |ui| {
                ui.label("In Edge mode, select one connected path of at least two edges.");
                if ui.add_enabled(has_edge_path, egui::Button::new("Straighten selected edges"))
                    .on_hover_text("Straighten the path within its UV island; endpoints and pinned vertices stay fixed")
                    .clicked()
                {
                    match self.selected_uv_edge_path(false) {
                        Ok((island, edges)) => self.execute(Command::StraightenPath {
                            island, vertices: Vec::new(), edges,
                        }),
                        Err(error) => self.set_error(error),
                    }
                }
                ui.horizontal_wrapped(|ui| {
                    for (label, axis) in [
                        ("Align UV horizontally", UvAxis::U),
                        ("Align UV vertically", UvAxis::V),
                    ] {
                        if ui.add_enabled(has_edge_path, egui::Button::new(label))
                            .on_hover_text("Align an open UV island boundary path while respecting pins")
                            .clicked()
                        {
                            match self.selected_uv_edge_path(true) {
                                Ok((island, edges)) => self.execute(Command::AlignBorder {
                                    island, axis, vertices: Vec::new(), edges,
                                }),
                                Err(error) => self.set_error(error),
                            }
                        }
                    }
                });
            });
            ui.horizontal(|ui| {
                if ui.button("Set TD").clicked() {
                    self.execute(Command::SetTexelDensity {
                        islands: self.target_islands(),
                        density: self.density,
                        resolution: self.resolution,
                    });
                }
                if ui.add(
                    egui::DragValue::new(&mut self.density)
                        .range(0.001..=1e6)
                        .suffix(" px/u"),
                ).changed() {
                    self.gpu_dirty = true;
                }
                if ui
                    .button("Get")
                    .on_hover_text("Read the first selected island's texel density")
                    .clicked()
                    && let (Some(&i), Some(scene)) = (self.target_islands().first(), &self.scene)
                {
                    let r = &scene.report.islands[i];
                    self.density = uvcraft_algo::analysis::texel_density(
                        r.uv_area,
                        r.area_3d,
                        self.resolution,
                    );
                    self.gpu_dirty = true;
                }
            });
            ui.horizontal(|ui| {
                if ui.selectable_label(self.density_brush, "Density brush").clicked() {
                    self.density_brush = !self.density_brush;
                    self.density_drag = None;
                    if self.density_brush {
                        self.brush = false;
                        self.seam_brush = false;
                        self.uv_edit_mode = None;
                        self.uv_edit_drag = None;
                    }
                }
                ui.checkbox(&mut self.density_realtime, "Live optimize")
                    .on_hover_text("Preview UV area adjustment while painting; commit as one undoable stroke on release");
            });
            if self.density_brush {
                ui.add(egui::Slider::new(&mut self.brush_radius, 4.0..=128.0).text("Radius pt"));
                ui.add(egui::Slider::new(&mut self.density_strength, 0.01..=1.0).text("Paint strength"));
                let painted = self.session.document().map_or(0, |doc| doc.density_targets().iter().flatten().count());
                ui.label(format!("{painted} faces painted · target {:.0} px/u", self.density));
            }
            ui.horizontal(|ui| {
                if ui.button("Apply density map…")
                    .on_hover_text("Read a grayscale PGM and scale selected islands by its UV-space values")
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new().add_filter("PGM", &["pgm"]).pick_file()
                {
                    self.execute(Command::ApplyDensityMap {
                        path,
                        islands: self.target_islands(),
                        min_scale: self.density_map_min,
                        max_scale: self.density_map_max,
                    });
                }
                ui.label("Min");
                ui.add(egui::DragValue::new(&mut self.density_map_min).range(0.001..=100.0));
                ui.label("Max");
                ui.add(egui::DragValue::new(&mut self.density_map_max).range(0.001..=100.0));
            });
            if ui.button("Bake AO density map…")
                .on_hover_text("Bake UV-space ambient occlusion to PGM; use Apply density map to turn it into per-island scale")
                .clicked()
                && let Some(mut path) = rfd::FileDialog::new().add_filter("PGM", &["pgm"])
                    .set_file_name("ambient-occlusion.pgm").save_file()
            {
                path.set_extension("pgm");
                self.execute(Command::BakeAmbientOcclusion {
                    path, resolution: 256, rays: 32, max_distance: 1.0,
                });
            }
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.gizmo_enabled, "UV gizmo")
                    .on_hover_text("Select islands, then drag cyan to move, orange to rotate, or green to scale in the UV view");
                ui.checkbox(&mut self.gizmo_snap, "Snap")
                    .on_hover_text("Move by texels, rotate by 15°, scale by 10% steps");
            });
            ui.horizontal(|ui| {
                for (label, deg, scale) in [
                    ("Rot +90°", 90.0, [1.0, 1.0]),
                    ("Rot −90°", -90.0, [1.0, 1.0]),
                    ("Flip U", 0.0, [-1.0, 1.0]),
                ] {
                    if ui.button(label).clicked() {
                        self.execute(Command::Transform {
                            islands: self.target_islands(),
                            translate: [0.0; 2],
                            rotate_deg: deg,
                            scale,
                            pivot: Pivot::IslandCenter,
                        });
                    }
                }
            });
        });

        ui.separator();
        ui.heading("Real Space");
        let physical_islands = self.lock_targets().0;
        let physical_edges = if self.mode == SelectMode::Edges {
            self.target_edges()
        } else {
            Vec::new()
        };
        ui.label(if physical_islands.is_empty() {
            "Islands: all (no selection)".to_owned()
        } else {
            format!("Islands: {} selected", physical_islands.len())
        });
        if !physical_edges.is_empty() {
            ui.label(format!("Edges: {} selected", physical_edges.len()));
        }
        ui.horizontal(|ui| {
            ui.label("Mesh unit");
            unit_picker(ui, "physical_mesh_unit", &mut self.physical_mesh_unit);
            ui.label("UV unit");
            unit_picker(ui, "physical_uv_unit", &mut self.physical_uv_unit);
        });
        ui.horizontal(|ui| {
            if ui.button("Measure").on_hover_text("Measure selected edges and islands; without a selection, measure all islands").clicked() {
                self.execute(Command::MeasurePhysical {
                    edges: physical_edges,
                    islands: physical_islands.clone(),
                    mesh_unit: self.physical_mesh_unit,
                    output_unit: self.physical_uv_unit,
                });
            }
            if ui.button("Set area scale").on_hover_text("Match each island’s UV area to its physical 3D area; preserves current UV shape and coordinates outside 0–1").clicked() {
                self.execute(Command::SetRealScale {
                    islands: physical_islands.clone(),
                    mesh_unit: self.physical_mesh_unit,
                    uv_unit: self.physical_uv_unit,
                });
            }
            if ui.button("Set exact 1:1 (planar)").on_hover_text("Project each planar island at its exact physical scale; non-planar islands return an error").clicked() {
                self.execute(Command::SetExactRealScale {
                    islands: physical_islands.clone(),
                    mesh_unit: self.physical_mesh_unit,
                    uv_unit: self.physical_uv_unit,
                });
            }
        });
        ui.label("Physical rectangle");
        ui.horizontal(|ui| {
            ui.label("Unit");
            unit_picker(ui, "physical_rect_unit", &mut self.physical_rect_unit);
            ui.label("W");
            ui.add(egui::DragValue::new(&mut self.physical_width).range(0.001..=1e9));
            ui.label("H");
            ui.add(egui::DragValue::new(&mut self.physical_height).range(0.001..=1e9));
        });
        ui.horizontal(|ui| {
            ui.label("Gap");
            ui.add(egui::DragValue::new(&mut self.physical_padding).range(0.0..=1e9));
            ui.label("Margin");
            ui.add(egui::DragValue::new(&mut self.physical_margin).range(0.0..=1e9));
        });
        if ui
            .button("Pack in rectangle")
            .on_hover_text("Keep island UV scale; report an error if the selection does not fit")
            .clicked()
        {
            self.execute(Command::PackPhysical {
                islands: physical_islands.clone(),
                width: self.physical_width,
                height: self.physical_height,
                unit: self.physical_rect_unit,
                uv_unit: self.physical_uv_unit,
                padding: self.physical_padding,
                margin: self.physical_margin,
            });
        }
        if ui.button("Export flat pattern SVG…")
            .on_hover_text("Export selected island contours at their current UV scale, with physical SVG dimensions")
            .clicked()
        {
            self.export_flat_pattern_dialog(physical_islands);
        }
        if let Some(report) = &self.physical_report {
            let unit = unit_label(report.output_unit);
            ui.label(format!("Measurements in {unit}"));
            egui::ScrollArea::vertical()
                .max_height(120.0)
                .show(ui, |ui| {
                    for edge in &report.edge_lengths {
                        ui.label(format!("Edge {}: {:.4} {unit}", edge.edge, edge.length));
                    }
                    for island in &report.island_areas {
                        ui.label(format!(
                            "Island {}: {:.4} {unit}²",
                            island.island, island.area
                        ));
                    }
                });
        }

        if let Some(scene) = &self.scene {
            let r = &scene.report;
            ui.separator();
            ui.heading("Stats");
            egui::Grid::new("stats")
                .num_columns(2)
                .striped(true)
                .show(ui, |ui| {
                    let mut row = |k: &str, v: String| {
                        ui.label(k);
                        ui.label(v);
                        ui.end_row();
                    };
                    row("Faces", r.face_count.to_string());
                    row("Islands", r.island_count.to_string());
                    row("Seams", format!("{} edges", r.seam_edges));
                    row("UV coverage", format!("{:.1}%", r.uv_area * 100.0));
                    row(
                        "Angle dist.",
                        format!(
                            "{:.3} (max {:.2})",
                            r.mean_angle_distortion, r.max_angle_distortion
                        ),
                    );
                    row(
                        "Area dist.",
                        format!(
                            "{:.3} (max {:.2})",
                            r.mean_area_distortion, r.max_area_distortion
                        ),
                    );
                    row(
                        "Flipped",
                        format!(
                            "{} islands, {} tris",
                            r.flipped_islands, r.flipped_triangles
                        ),
                    );
                    row("Outside 0–1", r.islands_outside_unit_tile.to_string());
                    let sel = self.target_islands();
                    if !sel.is_empty() {
                        row("Selected", format!("{} islands", sel.len()));
                        if let Some(&i) = sel.first() {
                            row(
                                "TD (first)",
                                format!("{:.1} px/u", r.islands[i].texel_density),
                            );
                        }
                    }
                });
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let document_info = self.session.document().map(|doc| {
            let file = doc
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Untitled mesh".to_owned());
            (
                file,
                doc.mesh.vertex_count(),
                doc.mesh.face_count(),
                doc.uv_channel_info(),
            )
        });
        let report_info = self
            .scene
            .as_ref()
            .map(|scene| (scene.report.island_count, scene.report.uv_area));
        let mut switch_channel = None;
        let mut create_channel = None;
        ui.horizontal(|ui| {
            if let Some((file, vertices, faces, channel_info)) = &document_info {
                ui.label(egui::RichText::new(format!("MODEL  {file}")).strong())
                    .on_hover_text(
                        self.session
                            .document()
                            .map_or_else(String::new, |doc| doc.path.display().to_string()),
                    );
                ui.separator();
                if let Some((active, channels)) = channel_info {
                    egui::ComboBox::from_id_salt("footer_uv_channel")
                        .selected_text(format!("UV{}", active + 1))
                        .width(56.0)
                        .show_ui(ui, |ui| {
                            for &channel in channels {
                                if ui
                                    .selectable_label(
                                        channel == *active,
                                        format!("UV{}", channel + 1),
                                    )
                                    .clicked()
                                    && channel != *active
                                {
                                    switch_channel = Some(channel);
                                }
                            }
                        });
                    if ui
                        .small_button("+")
                        .on_hover_text("Create UV channel")
                        .clicked()
                    {
                        create_channel = (0..=15).find(|channel| !channels.contains(channel));
                    }
                } else {
                    ui.label("UV1");
                }
                ui.separator();
                let muted = Color32::from_rgb(178, 182, 189);
                ui.colored_label(muted, format!("Verts: {vertices}"));
                ui.colored_label(muted, format!("Faces: {faces}"));
                if let Some((islands, area)) = report_info {
                    ui.colored_label(muted, format!("UV Islands: {islands}"));
                    ui.colored_label(muted, format!("Space: {:.1}%", area * 100.0));
                    ui.add(
                        egui::ProgressBar::new((area as f32).clamp(0.0, 1.0))
                            .desired_width(94.0)
                            .fill(theme::ORANGE),
                    );
                }
            } else {
                ui.weak("MODEL  No model open");
            }
            ui.separator();
            if let Some(r) = &self.running {
                ui.spinner();
                ui.label(format!(
                    "{}… {:.1} s",
                    r.cmd.name(),
                    r.started.elapsed().as_secs_f64()
                ));
                if let Some(progress) = &r.progress {
                    let fraction = if progress.total == 0 {
                        0.0
                    } else {
                        progress.completed as f32 / progress.total as f32
                    };
                    ui.add(
                        egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                            .desired_width(170.0)
                            .text(format!(
                                "{} {}/{}",
                                progress.phase, progress.completed, progress.total
                            )),
                    );
                }
                if ui
                    .add_enabled(!r.cancel.is_cancelled(), egui::Button::new("Cancel"))
                    .clicked()
                {
                    r.cancel.cancel();
                    self.jobs.clear();
                }
                if !self.jobs.is_empty() {
                    ui.weak(format!("(+{} queued)", self.jobs.len()));
                }
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            } else {
                let color = if self.status_error {
                    Color32::from_rgb(255, 110, 100)
                } else {
                    Color32::from_rgb(151, 155, 163)
                };
                ui.colored_label(color, &self.status)
                    .on_hover_text(&self.status);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Fit").on_hover_text("Frame all").clicked() {
                    self.frame_views();
                }
                if ui
                    .selectable_label(self.show_outliner, "Scene")
                    .on_hover_text("Toggle scene outliner")
                    .clicked()
                {
                    self.show_outliner = !self.show_outliner;
                }
            });
        });
        if let Some(channel) = switch_channel {
            self.execute(Command::SwitchUvChannel { channel });
        }
        if let Some(channel) = create_channel {
            self.execute(Command::CreateUvChannel { channel });
        }
    }

    // ------------------------------------------------------------ viewports

    fn distortion_config(&self) -> DistortionConfig {
        let selected_islands = if let Some(group) = self.group_selection {
            self.session
                .document()
                .zip(self.scene.as_ref())
                .and_then(|(doc, scene)| doc.island_groups().islands_in(group, &scene.islands).ok())
                .unwrap_or_default()
        } else {
            self.target_islands()
        };
        let active_island = self
            .distortion_active_island
            .filter(|island| selected_islands.contains(island))
            .or_else(|| selected_islands.first().copied());
        DistortionConfig {
            metric: self.distortion_metric,
            reference: self.distortion_reference,
            sensitivity: self.distortion_sensitivity,
            selected_islands,
            active_island,
            isolate_active: self.distortion_isolate,
            texel_target: self.density,
            resolution: self.resolution,
        }
    }

    fn upload_scene(&mut self) {
        if !self.gpu_dirty {
            return;
        }
        let distortion_config = self.distortion_config();
        let (Some(rs), Some(renderer)) = (&self.rs, &mut self.renderer) else {
            return;
        };
        let grid = if self.grid_visible {
            scene::build_grid(self.grid_divisions)
        } else {
            vec![]
        };
        match (self.session.document(), &self.scene) {
            (Some(doc), Some(scene)) => {
                if self.color_mode != ColorMode::Distortion
                    && !self.drag_selection_dirty
                    && self.pin_drag.is_none()
                    && self.density_drag.is_none()
                    && let (Some(drag), Some(layout)) = (&self.gizmo_drag, &self.uv_stream_layout)
                {
                    let transform = gizmo::transform(
                        drag.handle,
                        drag.start,
                        drag.current,
                        drag.center,
                        self.gizmo_snap,
                        self.resolution,
                    );
                    let mut changed = BTreeMap::new();
                    for &island in &drag.islands {
                        for index in scene.islands.uv_indices(island, &doc.mesh, doc.uv()) {
                            let point = gizmo::apply(doc.uv().uvs[index as usize], transform);
                            changed.insert(index, point.as_vec2().to_array());
                        }
                    }
                    let active_ids: Vec<u32> = changed.keys().copied().collect();
                    for &index in &self.last_preview_uv_ids {
                        changed
                            .entry(index)
                            .or_insert_with(|| doc.uv().uvs[index as usize].as_vec2().to_array());
                    }
                    let updates: Vec<_> = changed.into_iter().collect();
                    if renderer.set_scene_uvs_sparse(rs, layout, &updates) {
                        self.last_preview_uv_ids = active_ids;
                        self.gpu_dirty = false;
                        return;
                    }
                }
                let mut preview = doc.uv().clone();
                if let Some(drag) = &self.gizmo_drag {
                    let transform = gizmo::transform(
                        drag.handle,
                        drag.start,
                        drag.current,
                        drag.center,
                        self.gizmo_snap,
                        self.resolution,
                    );
                    for &island in &drag.islands {
                        for index in scene.islands.uv_indices(island, &doc.mesh, doc.uv()) {
                            preview.uvs[index as usize] =
                                gizmo::apply(doc.uv().uvs[index as usize], transform);
                        }
                    }
                }
                if self.density_realtime
                    && let Some(drag) = &self.density_drag
                {
                    let (faces, weights): (Vec<_>, Vec<_>) = drag
                        .weights
                        .iter()
                        .map(|(&face, &weight)| (face, weight))
                        .unzip();
                    let mut targets = doc.density_targets().to_vec();
                    if density_brush::paint_targets(
                        &doc.mesh,
                        doc.uv(),
                        &mut targets,
                        &faces,
                        &weights,
                        self.density,
                        self.resolution,
                        self.density_strength,
                    )
                    .is_ok_and(|changed| changed > 0)
                    {
                        let mut pins = doc.pins().to_vec();
                        for (face, target) in targets.iter_mut().enumerate() {
                            if doc.locked_faces()[face]
                                || doc.locked_island_faces()[face]
                                || doc.island_groups().face_locked(face)
                            {
                                *target = None;
                                for corner in doc.mesh.face_corners(face) {
                                    pins[doc.mesh.corner_vertex(corner)] = true;
                                }
                            }
                        }
                        let _ = density_brush::optimize_targets(
                            &doc.mesh,
                            &mut preview,
                            &targets,
                            &pins,
                            self.resolution,
                            12,
                        );
                    }
                }
                let uv = if let Some(drag) = &self.pin_drag {
                    &drag.preview
                } else if self.gizmo_drag.is_some()
                    || (self.density_realtime && self.density_drag.is_some())
                {
                    &preview
                } else {
                    doc.uv()
                };
                let preview_edges: Vec<bool> = self
                    .sel_edges
                    .iter()
                    .enumerate()
                    .map(|(i, &selected)| {
                        selected
                            || self.seam_brush_edges.get(i).copied().unwrap_or(false)
                            || self.path_preview_edges.get(i).copied().unwrap_or(false)
                    })
                    .collect();
                if self.color_mode != ColorMode::Distortion
                    && !self.drag_selection_dirty
                    && (self.pin_drag.is_some()
                        || self.gizmo_drag.is_some()
                        || self.density_drag.is_some())
                {
                    let mut changed = BTreeMap::new();
                    for (index, (&point, &base)) in uv.uvs.iter().zip(&doc.uv().uvs).enumerate() {
                        if point != base {
                            changed.insert(index as u32, point.as_vec2().to_array());
                        }
                    }
                    let active_ids: Vec<u32> = changed.keys().copied().collect();
                    for &index in &self.last_preview_uv_ids {
                        changed
                            .entry(index)
                            .or_insert_with(|| uv.uvs[index as usize].as_vec2().to_array());
                    }
                    let updates: Vec<_> = changed.into_iter().collect();
                    if self
                        .uv_stream_layout
                        .as_ref()
                        .is_some_and(|layout| renderer.set_scene_uvs_sparse(rs, layout, &updates))
                    {
                        self.last_preview_uv_ids = active_ids;
                        self.gpu_dirty = false;
                        return;
                    }
                    let tri_uv = scene::build_tri_uvs(uv, scene);
                    let (wire_uv, edge_uv) = scene::build_line_uvs(
                        &doc.mesh,
                        uv,
                        &preview_edges,
                        &self.hidden,
                        self.overlay_seams,
                        self.overlay_borders,
                    );
                    if renderer.set_scene_uvs(rs, &tri_uv, &wire_uv, &edge_uv) {
                        self.last_preview_uv_ids = active_ids;
                        self.gpu_dirty = false;
                        return;
                    }
                }
                let group_colors: Vec<_> = (0..doc.mesh.face_count())
                    .map(|face| doc.island_groups().face_color(face))
                    .collect();
                let tris = scene::build_tris_with_distortion(
                    &doc.mesh,
                    doc.measurement_mesh(),
                    uv,
                    scene,
                    scene::TriColorSettings {
                        selected_faces: &self.sel_faces,
                        mode: self.color_mode,
                        target_density: self.density,
                        group_colors: &group_colors,
                        distortion: &distortion_config,
                    },
                );
                let (wire, edges) = scene::build_lines(
                    &doc.mesh,
                    uv,
                    &preview_edges,
                    &self.hidden,
                    self.overlay_seams,
                    self.overlay_borders,
                );
                if self.color_mode == ColorMode::Distortion
                    && !self.drag_selection_dirty
                    && (self.gizmo_drag.is_some()
                        || self.pin_drag.is_some()
                        || self.density_drag.is_some())
                {
                    let tri_uv: Vec<_> = tris.iter().map(|vertex| vertex.uv).collect();
                    let tri_color: Vec<_> = tris.iter().map(|vertex| vertex.color).collect();
                    let wire_uv: Vec<_> = wire.iter().map(|vertex| vertex.uv).collect();
                    let edge_uv: Vec<_> = edges.iter().map(|vertex| vertex.uv).collect();
                    if renderer
                        .set_scene_uvs_and_colors(rs, &tri_uv, &tri_color, &wire_uv, &edge_uv)
                    {
                        self.last_preview_uv_ids = uv
                            .uvs
                            .iter()
                            .zip(&doc.uv().uvs)
                            .enumerate()
                            .filter_map(|(i, (point, base))| (point != base).then_some(i as u32))
                            .collect();
                        self.gpu_dirty = false;
                        return;
                    }
                }
                renderer.set_scene(rs, &tris, &wire, &edges, &grid);
                let tri_sources = scene::build_tri_uv_sources(uv, scene);
                let (wire_sources, edge_sources) = scene::build_line_uv_sources(
                    &doc.mesh,
                    uv,
                    &preview_edges,
                    &self.hidden,
                    self.overlay_seams,
                    self.overlay_borders,
                );
                self.uv_stream_layout = (tri_sources.len() == tris.len()
                    && wire_sources.len() == wire.len()
                    && edge_sources.len() == edges.len())
                .then(|| {
                    UvStreamLayout::new(&tri_sources, &wire_sources, &edge_sources, uv.uvs.len())
                });
                self.last_preview_uv_ids = if self.gizmo_drag.is_some()
                    || self.pin_drag.is_some()
                    || self.density_drag.is_some()
                {
                    uv.uvs
                        .iter()
                        .zip(&doc.uv().uvs)
                        .enumerate()
                        .filter_map(|(index, (point, base))| {
                            (point != base).then_some(index as u32)
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                self.drag_selection_dirty = false;
            }
            _ => {
                renderer.set_scene(rs, &[], &[], &[], &grid);
                self.uv_stream_layout = None;
                self.last_preview_uv_ids.clear();
            }
        }
        self.gpu_dirty = false;
    }

    fn gizmo_geometry(&self) -> Option<(DVec2, Vec<usize>)> {
        if !self.gizmo_enabled || self.running.is_some() {
            return None;
        }
        let (Some(doc), Some(scene)) = (self.session.document(), &self.scene) else {
            return None;
        };
        let islands = if let Some(group) = self.group_selection {
            if doc
                .island_groups()
                .nodes()
                .any(|node| node.id == group && node.locked)
            {
                return None;
            }
            doc.island_groups().islands_in(group, &scene.islands).ok()?
        } else {
            self.target_islands()
        };
        if islands.is_empty() {
            return None;
        }
        let mut lo = DVec2::INFINITY;
        let mut hi = DVec2::NEG_INFINITY;
        for &island in &islands {
            for index in scene.islands.uv_indices(island, &doc.mesh, doc.uv()) {
                let p = doc.uv().uvs[index as usize];
                lo = lo.min(p);
                hi = hi.max(p);
            }
        }
        let center = (lo + hi) * 0.5;
        center.is_finite().then_some((center, islands))
    }

    fn gizmo_hit(pointer: Vec2, center: Vec2) -> Option<GizmoHandle> {
        let close = |p: Vec2| pointer.distance(p) <= 14.0;
        if close(center) {
            Some(GizmoHandle::Move)
        } else if close(center + Vec2::new(0.0, -52.0)) {
            Some(GizmoHandle::Rotate)
        } else if close(center + Vec2::splat(52.0)) {
            Some(GizmoHandle::Scale)
        } else {
            None
        }
    }

    fn finish_gizmo_drag(&mut self) {
        let Some(drag) = self.gizmo_drag.take() else {
            return;
        };
        self.drag_selection_dirty = false;
        self.gpu_dirty = true;
        let transform = gizmo::transform(
            drag.handle,
            drag.start,
            drag.current,
            drag.center,
            self.gizmo_snap,
            self.resolution,
        );
        if !transform.is_noop() {
            if let Some(group) = self.group_selection {
                let offset = if drag.handle == GizmoHandle::Move {
                    transform.translate
                } else {
                    [0.0, 0.0]
                };
                self.execute(Command::TransformIslandGroup {
                    group,
                    translate: offset,
                    rotate_deg: transform.rotate_deg,
                    scale: transform.scale[0],
                });
            } else {
                self.execute(Command::Transform {
                    islands: drag.islands,
                    translate: transform.translate,
                    rotate_deg: transform.rotate_deg,
                    scale: transform.scale,
                    pivot: Pivot::Origin,
                });
            }
        }
    }

    fn sample_uv_edit(&mut self, pointer: Vec2, size: Vec2) {
        let (Some(doc), Some(drag)) = (self.session.document(), &mut self.uv_edit_drag) else {
            return;
        };
        let selected = match self.mode {
            SelectMode::Vertices => self.sel_verts.iter().any(|&on| on),
            SelectMode::Edges => self.sel_edges.iter().any(|&on| on),
            _ => self.sel_faces.iter().any(|&on| on),
        };
        let mut allowed = vec![false; doc.uv().uvs.len()];
        for face in 0..doc.mesh.face_count() {
            if self.hidden.get(face).copied().unwrap_or(false) {
                continue;
            }
            for c in doc.mesh.face_corners(face) {
                let on = match self.mode {
                    SelectMode::Vertices => self
                        .sel_verts
                        .get(doc.mesh.corner_vertex(c))
                        .copied()
                        .unwrap_or(false),
                    SelectMode::Edges => {
                        let previous = doc.mesh.prev(c);
                        self.sel_edges
                            .get(doc.mesh.corner_edge(c))
                            .copied()
                            .unwrap_or(false)
                            || self
                                .sel_edges
                                .get(doc.mesh.corner_edge(previous))
                                .copied()
                                .unwrap_or(false)
                    }
                    _ => self.sel_faces.get(face).copied().unwrap_or(false),
                };
                if !selected || on {
                    allowed[doc.uv().corner_uv[c] as usize] = true;
                }
            }
        }
        for (index, uv) in doc.uv().uvs.iter().enumerate() {
            if !allowed[index] {
                continue;
            }
            let screen = self
                .camuv
                .uv_to_screen(Vec2::new(uv.x as f32, uv.y as f32), size);
            let distance = screen.distance(pointer);
            if distance <= self.brush_radius {
                let weight = (1.0 - distance / self.brush_radius) as f64;
                drag.weights
                    .entry(index)
                    .and_modify(|w| *w = w.max(weight))
                    .or_insert(weight);
            }
        }
        drag.current = pointer;
        drag.last_sample = pointer;
    }

    fn finish_uv_edit(&mut self, size: Vec2) {
        let Some(drag) = self.uv_edit_drag.take() else {
            return;
        };
        if drag.weights.is_empty() {
            return;
        }
        let a = self.camuv.screen_to_uv(drag.start, size);
        let b = self.camuv.screen_to_uv(drag.current, size);
        let delta = [f64::from(b.x - a.x), f64::from(b.y - a.y)];
        if drag.mode == BrushMode::Move && delta[0].abs() + delta[1].abs() < 1e-10 {
            return;
        }
        let (vertices, weights): (Vec<_>, Vec<_>) = drag.weights.into_iter().unzip();
        self.execute(Command::BrushEdit {
            vertices,
            weights,
            mode: drag.mode,
            delta,
            strength: self.uv_edit_strength,
            iterations: if drag.mode == BrushMode::Relax { 10 } else { 1 },
        });
    }

    fn sample_density_drag(&mut self, pointer: Vec2, size: Vec2) {
        let (Some(doc), Some(drag)) = (self.session.document(), &mut self.density_drag) else {
            return;
        };
        let selected = self.sel_faces.iter().any(|&face| face);
        for face in 0..doc.mesh.face_count() {
            if self.hidden.get(face).copied().unwrap_or(false)
                || doc.locked_faces()[face]
                || doc.locked_island_faces()[face]
                || doc.island_groups().face_locked(face)
                || (selected && !self.sel_faces[face])
            {
                continue;
            }
            let points: Vec<Vec2> = doc
                .mesh
                .face_corners(face)
                .map(|corner| {
                    let uv = doc.uv().uv_at_corner(corner).as_vec2();
                    self.camuv.uv_to_screen(uv, size)
                })
                .collect();
            if points.len() < 3 {
                continue;
            }
            let distance = if scene::point_in_polygon(pointer, &points) {
                0.0
            } else {
                points
                    .iter()
                    .enumerate()
                    .map(|(index, &a)| {
                        let b = points[(index + 1) % points.len()];
                        scene::segment_distance(pointer.as_dvec2(), a.as_dvec2(), b.as_dvec2())
                            as f32
                    })
                    .fold(f32::INFINITY, f32::min)
            };
            if distance <= self.brush_radius {
                let weight = (1.0 - distance / self.brush_radius) as f64;
                drag.weights
                    .entry(face)
                    .and_modify(|old| *old = old.max(weight))
                    .or_insert(weight);
            }
        }
        drag.last_sample = pointer;
        self.gpu_dirty = true;
    }

    fn finish_density_drag(&mut self) {
        let Some(drag) = self.density_drag.take() else {
            return;
        };
        if drag.weights.is_empty() {
            return;
        }
        let (faces, weights): (Vec<_>, Vec<_>) = drag.weights.into_iter().unzip();
        self.execute(Command::DensityPaint {
            faces,
            weights,
            target_density: self.density,
            resolution: self.resolution,
            strength: self.density_strength,
            optimize_iterations: if self.density_realtime { 12 } else { 0 },
        });
    }

    fn draw_gizmo(&self, ui: &egui::Ui, rect: Rect, size: Vec2) {
        let center = match &self.gizmo_drag {
            Some(drag) => {
                if drag.handle == GizmoHandle::Move {
                    let t = gizmo::transform(
                        drag.handle,
                        drag.start,
                        drag.current,
                        drag.center,
                        self.gizmo_snap,
                        self.resolution,
                    );
                    drag.center + DVec2::from(t.translate)
                } else {
                    drag.center
                }
            }
            None => match self.gizmo_geometry() {
                Some((center, _)) => center,
                None => return,
            },
        };
        let local = self
            .camuv
            .uv_to_screen(Vec2::new(center.x as f32, center.y as f32), size);
        let c = rect.min + EVec2::new(local.x, local.y);
        let painter = ui.painter_at(rect);
        let move_color = Color32::from_rgb(90, 210, 245);
        let rotate_color = Color32::from_rgb(255, 180, 75);
        let scale_color = Color32::from_rgb(145, 230, 120);
        painter.circle_stroke(
            c,
            52.0,
            egui::Stroke::new(1.0, rotate_color.gamma_multiply(0.7)),
        );
        painter.line_segment(
            [c, c + EVec2::new(0.0, -52.0)],
            egui::Stroke::new(1.5, rotate_color),
        );
        painter.line_segment(
            [c, c + EVec2::splat(52.0)],
            egui::Stroke::new(1.5, scale_color),
        );
        painter.circle_filled(c, 8.0, move_color);
        painter.circle_filled(c + EVec2::new(0.0, -52.0), 7.0, rotate_color);
        painter.rect_filled(
            Rect::from_center_size(c + EVec2::splat(52.0), EVec2::splat(13.0)),
            1.0,
            scale_color,
        );
    }

    fn group_screen_bounds(&self, size: Vec2) -> Vec<(u64, Rect, Color32, String)> {
        let (Some(doc), Some(scene)) = (self.session.document(), &self.scene) else {
            return Vec::new();
        };
        let preview = self.gizmo_drag.as_ref().map(|drag| {
            gizmo::transform(
                drag.handle,
                drag.start,
                drag.current,
                drag.center,
                self.gizmo_snap,
                self.resolution,
            )
        });
        doc.island_groups()
            .nodes()
            .filter(|node| node.visible)
            .filter_map(|node| {
                let members = doc
                    .island_groups()
                    .islands_in(node.id, &scene.islands)
                    .ok()?;
                if members.is_empty() {
                    return None;
                }
                let mut lo = DVec2::INFINITY;
                let mut hi = DVec2::NEG_INFINITY;
                for island in members {
                    for index in scene.islands.uv_indices(island, &doc.mesh, doc.uv()) {
                        let mut point = doc.uv().uvs[index as usize];
                        if self.group_selection == Some(node.id)
                            && let Some(transform) = preview
                        {
                            point = gizmo::apply(point, transform);
                        }
                        lo = lo.min(point);
                        hi = hi.max(point);
                    }
                }
                if !lo.is_finite() || !hi.is_finite() {
                    return None;
                }
                let a = self.camuv.uv_to_screen(lo.as_vec2(), size);
                let b = self.camuv.uv_to_screen(hi.as_vec2(), size);
                let bounds =
                    Rect::from_two_pos(Pos2::new(a.x, a.y), Pos2::new(b.x, b.y)).expand(11.0);
                let color = if node.id > 1 && node.color == [128, 180, 240, 255] {
                    let [r, g, b] = scene::island_color(node.id as usize);
                    Color32::from_rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
                } else {
                    Color32::from_rgb(node.color[0], node.color[1], node.color[2])
                };
                Some((node.id, bounds, color, node.name.clone()))
            })
            .collect()
    }

    fn group_at_screen(&self, pointer: Vec2, size: Vec2) -> Option<u64> {
        let bounds: Vec<_> = self
            .group_screen_bounds(size)
            .into_iter()
            .map(|(group, bounds, _, _)| (group, bounds))
            .collect();
        pick_group_container(&bounds, pointer)
    }

    fn select_group_in_uv(&mut self, group: u64) {
        let members = self
            .session
            .document()
            .zip(self.scene.as_ref())
            .and_then(|(doc, scene)| doc.island_groups().islands_in(group, &scene.islands).ok());
        let Some(members) = members else {
            return;
        };
        self.set_view_mode(1, SelectMode::Islands);
        self.sel_faces.fill(false);
        if let Some(scene) = &self.scene {
            for island in members {
                for &face in scene.islands.faces(island) {
                    if !self.is_hidden(face as usize) {
                        self.sel_faces[face as usize] = true;
                    }
                }
            }
        }
        self.group_edit = Some(group);
        if let Some(name) = self
            .session
            .document()
            .and_then(|doc| doc.island_groups().get(group).map(|node| node.name.clone()))
        {
            self.group_rename = name;
        }
        self.group_selection = Some(group);
        self.gpu_dirty = true;
    }

    fn draw_group_bounds(&self, ui: &egui::Ui, rect: Rect, size: Vec2) {
        let painter = ui.painter_at(rect);
        for (group, local, color, name) in self.group_screen_bounds(size) {
            let bounds = local.translate(rect.min.to_vec2());
            let selected = self.group_selection == Some(group);
            painter.rect_filled(
                bounds,
                5.0,
                color.gamma_multiply(if selected { 0.18 } else { 0.09 }),
            );
            painter.rect_stroke(
                bounds,
                5.0,
                egui::Stroke::new(if selected { 2.5 } else { 1.5 }, color),
                egui::StrokeKind::Inside,
            );
            painter.text(
                bounds.left_top() + EVec2::new(6.0, 4.0),
                egui::Align2::LEFT_TOP,
                format!("Group · {name}"),
                egui::FontId::proportional(12.0),
                color,
            );
        }
    }

    /// UV seams need a screen-space stroke: wgpu line primitives are always
    /// one pixel wide, and the selected-edge yellow otherwise hides a cut.
    fn draw_uv_seams(&self, ui: &egui::Ui, rect: Rect, size: Vec2) {
        let (Some(doc), Some(scene)) = (self.session.document(), self.scene.as_ref()) else {
            return;
        };
        let mesh = &doc.mesh;
        let uv = self
            .pin_drag
            .as_ref()
            .map_or(doc.uv(), |drag| &drag.preview);
        let seam_edges: Vec<bool> = (0..mesh.edge_count())
            .map(|edge| uv.edge_kind(mesh, edge) == EdgeKind::Seam)
            .collect();
        let preview = self.gizmo_drag.as_ref().map(|drag| {
            (
                gizmo::transform(
                    drag.handle,
                    drag.start,
                    drag.current,
                    drag.center,
                    self.gizmo_snap,
                    self.resolution,
                ),
                drag.islands.iter().copied().collect::<BTreeSet<_>>(),
            )
        });
        let painter = ui.painter_at(rect);
        for corner in 0..mesh.corner_count() {
            let face = mesh.corner_face(corner);
            if self.hidden.get(face).copied().unwrap_or(false) {
                continue;
            }
            let edge = mesh.corner_edge(corner);
            let selected = self.sel_edges.get(edge).copied().unwrap_or(false);
            if !seam_edges[edge] || (!self.overlay_seams && !selected) {
                continue;
            }
            let point = |c| {
                let mut point = uv.uv_at_corner(c);
                if let Some((transform, islands)) = &preview
                    && islands.contains(&scene.islands.island_of_face(face))
                {
                    point = gizmo::apply(point, *transform);
                }
                point.is_finite().then(|| {
                    let local = self.camuv.uv_to_screen(point.as_vec2(), size);
                    rect.min + EVec2::new(local.x, local.y)
                })
            };
            let (Some(a), Some(b)) = (point(corner), point(mesh.next(corner))) else {
                continue;
            };
            if selected {
                painter.line_segment(
                    [a, b],
                    egui::Stroke::new(6.0, Color32::from_rgb(105, 40, 8)),
                );
                painter.line_segment(
                    [a, b],
                    egui::Stroke::new(4.2, Color32::from_rgb(255, 126, 25)),
                );
                painter.line_segment(
                    [a, b],
                    egui::Stroke::new(1.7, Color32::from_rgb(255, 238, 75)),
                );
            } else {
                painter.line_segment(
                    [a, b],
                    egui::Stroke::new(3.5, Color32::from_rgb(255, 126, 25)),
                );
            }
        }
    }

    fn start_pin_drag(&mut self, pointer: Vec2, size: Vec2) -> bool {
        let (Some(doc), Some(scene)) = (self.session.document(), &self.scene) else {
            return false;
        };
        let hit = (0..doc.mesh.corner_count())
            .filter(|&corner| {
                let vertex = doc.mesh.corner_vertex(corner);
                doc.pins().get(vertex).copied().unwrap_or(false)
                    && !self.is_hidden(doc.mesh.corner_face(corner))
            })
            .filter_map(|corner| {
                let screen = self.corner_screen(1, corner, size)?;
                (screen.distance(pointer) <= 9.0).then_some((corner, screen.distance(pointer)))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(corner, _)| corner);
        let Some(corner) = hit else { return false };
        let vertex = doc.mesh.corner_vertex(corner);
        if (0..doc.mesh.corner_count()).any(|c| {
            doc.mesh.corner_vertex(c) == vertex
                && (doc.locked_faces()[doc.mesh.corner_face(c)]
                    || doc.locked_island_faces()[doc.mesh.corner_face(c)]
                    || doc.island_groups().face_locked(doc.mesh.corner_face(c)))
        }) {
            return false;
        }
        let mut targets = (0..doc.mesh.corner_count())
            .filter(|&c| doc.mesh.corner_vertex(c) == vertex)
            .map(|c| scene.islands.island_of_face(doc.mesh.corner_face(c)))
            .collect::<Vec<_>>();
        targets.sort_unstable();
        targets.dedup();
        let mut pins = doc.pins().to_vec();
        for face in 0..doc.mesh.face_count() {
            if doc.locked_faces()[face]
                || doc.locked_island_faces()[face]
                || doc.island_groups().face_locked(face)
            {
                for c in doc.mesh.face_corners(face) {
                    pins[doc.mesh.corner_vertex(c)] = true;
                }
            }
        }
        let start = doc.uv().uv_at_corner(corner);
        let clicked = self.camuv.screen_to_uv(pointer, size);
        let clicked = DVec2::new(clicked.x as f64, clicked.y as f64);
        self.pin_drag = Some(PinDrag {
            vertex,
            start,
            target: start,
            pointer_offset: start - clicked,
            preview: doc.uv().clone(),
            targets,
            pins,
            last_solved: start,
            pending: None,
        });
        true
    }

    fn poll_pin_drag(&mut self) {
        let Some(drag) = &mut self.pin_drag else {
            return;
        };
        if let Some(rx) = &drag.pending {
            match rx.try_recv() {
                Ok((_, preview)) => {
                    drag.preview = preview;
                    drag.pending = None;
                    self.gpu_dirty = true;
                }
                Err(TryRecvError::Empty) => {
                    self.ctx.request_repaint_after(Duration::from_millis(16))
                }
                Err(TryRecvError::Disconnected) => drag.pending = None,
            }
        }
        if drag.pending.is_some() || drag.target == drag.last_solved {
            return;
        }
        let Some(doc) = self.session.document() else {
            return;
        };
        let mesh = doc.mesh.clone();
        let islands = self
            .scene
            .as_ref()
            .expect("pin drag has scene")
            .islands
            .clone();
        let mut preview = drag.preview.clone();
        let target = drag.target;
        let targets = drag.targets.clone();
        let pins = drag.pins.clone();
        let vertex = drag.vertex;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = uvcraft_algo::pin_drag::step(
                &mesh,
                &mut preview,
                &islands,
                &targets,
                uvcraft_algo::pin_drag::PinDragParams {
                    vertex,
                    destination: target,
                    iterations: 2,
                },
                &pins,
            );
            if result.is_ok() {
                let _ = tx.send((target, preview));
            }
        });
        drag.pending = Some(rx);
        drag.last_solved = target;
        self.ctx.request_repaint_after(Duration::from_millis(16));
    }

    fn finish_pin_drag(&mut self) {
        if let Some(drag) = self.pin_drag.take() {
            self.gpu_dirty = true;
            if let Some(command) = drag.release_command() {
                self.execute(command);
            }
        }
    }

    fn viewport(&mut self, ui: &mut egui::Ui, rect: Rect, view: usize) {
        self.sync_active_view_mode();
        let outer_rect = rect;
        let header_height = 43.0;
        let footer_height = 28.0;
        let rect = Rect::from_min_max(
            outer_rect.min + EVec2::new(0.0, header_height),
            outer_rect.max - EVec2::new(0.0, footer_height),
        );
        let response = ui.allocate_rect(rect, Sense::click_and_drag());
        if response.hovered()
            || response.clicked()
            || response.secondary_clicked()
            || response.dragged()
        {
            self.activate_view(view);
        }
        let size = Vec2::new(rect.width(), rect.height());
        self.view_sizes[view] = size;
        let (scroll, modifiers) = ui.input(|i| (i.smooth_scroll_delta.y, i.modifiers));
        let drag = response.drag_delta();
        let local = |p: Pos2| Vec2::new(p.x - rect.min.x, p.y - rect.min.y);
        let navigation = self.hotkeys.mouse;
        let primary_navigation = if view == 0 {
            [
                Some(navigation.orbit_3d),
                navigation.orbit_3d_alt,
                Some(navigation.pan_3d),
                navigation.pan_3d_alt,
            ]
            .into_iter()
            .flatten()
            .any(|binding| {
                binding.button == MouseButton::Left && mouse_modifiers_match(binding, modifiers)
            })
        } else {
            [Some(navigation.pan_uv), navigation.pan_uv_alt]
                .into_iter()
                .flatten()
                .any(|binding| {
                    binding.button == MouseButton::Left && mouse_modifiers_match(binding, modifiers)
                })
        };

        if view == 0 {
            let pointer = (!response.dragged())
                .then(|| response.hover_pos())
                .flatten()
                .map(local);
            self.update_edge_path_preview(pointer, size, modifiers.shift);
        }

        let pin_started = view == 1
            && !self.busy()
            && !primary_navigation
            && !self.brush
            && !self.seam_brush
            && !self.density_brush
            && self.uv_edit_mode.is_none()
            && response.drag_started_by(PointerButton::Primary)
            && response
                .interact_pointer_pos()
                .is_some_and(|p| self.start_pin_drag(local(p), size));
        let gizmo_handle = if view == 1
            && !pin_started
            && self.pin_drag.is_none()
            && !primary_navigation
            && !self.brush
            && !self.seam_brush
            && !self.density_brush
            && self.uv_edit_mode.is_none()
        {
            self.gizmo_geometry().and_then(|(center, islands)| {
                let p = response.interact_pointer_pos()?;
                let center = self
                    .camuv
                    .uv_to_screen(Vec2::new(center.x as f32, center.y as f32), size);
                Self::gizmo_hit(local(p), center).map(|handle| (handle, islands, center))
            })
        } else {
            None
        };
        let gizmo_pointer_hit = gizmo_handle.is_some();
        if response.drag_started_by(PointerButton::Primary)
            && let Some((handle, islands, _)) = gizmo_handle
            && let Some(p) = response.interact_pointer_pos()
            && let Some((center, _)) = self.gizmo_geometry()
        {
            let start = self.camuv.screen_to_uv(local(p), size);
            self.gizmo_drag = Some(GizmoDrag {
                handle,
                start: DVec2::new(start.x as f64, start.y as f64),
                current: DVec2::new(start.x as f64, start.y as f64),
                center,
                islands,
            });
            self.gpu_dirty = true;
        }
        // A press on a UV polygon moves its island directly. Use the press
        // origin rather than the pointer after egui's drag threshold, so the
        // island does not jump when the drag first becomes active.
        if view == 1
            && self.mode == SelectMode::Islands
            && !self.busy()
            && !primary_navigation
            && !self.brush
            && !self.seam_brush
            && !self.density_brush
            && self.uv_edit_mode.is_none()
            && self.pin_drag.is_none()
            && !gizmo_pointer_hit
            && self.gizmo_drag.is_none()
            && response.drag_started_by(PointerButton::Primary)
            && let Some(origin) = ui.input(|i| i.pointer.press_origin())
            && rect.contains(origin)
        {
            let start = self.camuv.screen_to_uv(local(origin), size).as_dvec2();
            let group = (!modifiers.shift)
                .then(|| self.group_at_screen(local(origin), size))
                .flatten();
            if let Some(group) = group {
                self.select_group_in_uv(group);
                let islands = self
                    .session
                    .document()
                    .zip(self.scene.as_ref())
                    .and_then(|(doc, scene)| {
                        doc.island_groups().islands_in(group, &scene.islands).ok()
                    })
                    .unwrap_or_default();
                self.gizmo_drag = Some(GizmoDrag {
                    handle: GizmoHandle::Move,
                    start,
                    current: start,
                    center: start,
                    islands,
                });
                self.gpu_dirty = true;
            } else if let (Some(doc), Some(scene)) = (self.session.document(), &self.scene)
                && let Some(face) = scene::pick_face_uv(doc.uv(), scene, start)
            {
                self.group_selection = None;
                self.drag_selection_dirty =
                    select_drag_island(&scene.islands, &mut self.sel_faces, face, modifiers.shift);
                self.gizmo_drag = Some(GizmoDrag {
                    handle: GizmoHandle::Move,
                    start,
                    current: start,
                    center: start,
                    islands: self.target_islands(),
                });
                self.gpu_dirty = true;
            }
        }
        if view == 1
            && let Some(drag) = &mut self.gizmo_drag
            && let Some(p) = response.interact_pointer_pos()
            && (response.dragged_by(PointerButton::Primary) || response.drag_stopped())
        {
            let current = self.camuv.screen_to_uv(local(p), size);
            let current = DVec2::new(current.x as f64, current.y as f64);
            if drag.current != current {
                drag.current = current;
                self.gpu_dirty = true;
                ui.ctx().request_repaint();
            }
        }
        if view == 1 && self.pin_drag.is_some() {
            if response.dragged_by(PointerButton::Primary)
                && let Some(p) = response.interact_pointer_pos()
            {
                let uv = self.camuv.screen_to_uv(local(p), size);
                if let Some(drag) = &mut self.pin_drag {
                    drag.target = DVec2::new(uv.x as f64, uv.y as f64) + drag.pointer_offset;
                }
            }
            self.poll_pin_drag();
        }
        let gizmo_active = self.gizmo_drag.is_some();
        let pin_active = self.pin_drag.is_some();
        let uv_edit_active =
            view == 1 && self.uv_edit_mode.is_some() && !primary_navigation && !pin_active;
        let density_active = view == 1 && self.density_brush && !primary_navigation && !pin_active;
        if density_active
            && response.drag_started_by(PointerButton::Primary)
            && let Some(p) = response.interact_pointer_pos()
        {
            let p = local(p);
            self.density_drag = Some(DensityDrag {
                last_sample: p,
                weights: BTreeMap::new(),
            });
            self.sample_density_drag(p, size);
        }
        if density_active
            && response.dragged_by(PointerButton::Primary)
            && let Some(p) = response.interact_pointer_pos()
            && let Some(last) = self.density_drag.as_ref().map(|d| d.last_sample)
        {
            let p = local(p);
            let step = (self.brush_radius * 0.25).max(1.0);
            let count = (p.distance(last) / step).ceil().max(1.0) as usize;
            for n in 1..=count {
                self.sample_density_drag(last.lerp(p, n as f32 / count as f32), size);
            }
            ui.ctx().request_repaint();
        }
        if uv_edit_active
            && response.drag_started_by(PointerButton::Primary)
            && let (Some(mode), Some(p)) = (self.uv_edit_mode, response.interact_pointer_pos())
        {
            let p = local(p);
            self.uv_edit_drag = Some(UvEditDrag {
                mode,
                start: p,
                current: p,
                last_sample: p,
                weights: BTreeMap::new(),
            });
            self.sample_uv_edit(p, size);
        }
        if uv_edit_active
            && response.dragged_by(PointerButton::Primary)
            && let Some(p) = response.interact_pointer_pos()
            && let Some(last) = self.uv_edit_drag.as_ref().map(|d| d.last_sample)
        {
            let p = local(p);
            let step = (self.brush_radius * 0.25).max(1.0);
            let count = (p.distance(last) / step).ceil().max(1.0) as usize;
            for n in 1..=count {
                self.sample_uv_edit(last.lerp(p, n as f32 / count as f32), size);
            }
        }

        if view == 0 {
            if mouse_drag_matches(&response, navigation.orbit_3d, modifiers)
                || navigation
                    .orbit_3d_alt
                    .is_some_and(|binding| mouse_drag_matches(&response, binding, modifiers))
            {
                self.cam3.orbit(drag.x, drag.y);
            }
            if mouse_drag_matches(&response, navigation.pan_3d, modifiers)
                || navigation
                    .pan_3d_alt
                    .is_some_and(|binding| mouse_drag_matches(&response, binding, modifiers))
            {
                self.cam3.pan(drag.x, drag.y, size.y);
            }
            if response.hovered() && scroll != 0.0 {
                self.cam3.zoom((-scroll * 0.0015).exp());
            }
        } else {
            if mouse_drag_matches(&response, navigation.pan_uv, modifiers)
                || navigation
                    .pan_uv_alt
                    .is_some_and(|binding| mouse_drag_matches(&response, binding, modifiers))
            {
                self.camuv.pan(drag.x, drag.y);
            }
            if response.hovered()
                && scroll != 0.0
                && let Some(p) = response.hover_pos()
            {
                self.camuv.zoom_at((scroll * 0.0015).exp(), local(p), size);
            }
        }
        if response.clicked()
            && !primary_navigation
            && !uv_edit_active
            && !density_active
            && !gizmo_pointer_hit
            && !gizmo_active
            && !pin_active
            && let Some(p) = response.interact_pointer_pos()
        {
            if self.seam_brush && view == 0 {
                self.sample_seam_brush(local(p), size);
                self.finish_seam_brush();
            } else if self.brush {
                self.brush_select(view, local(p), size, modifiers);
            } else {
                self.click(view, local(p), size, modifiers);
            }
            if !self.brush
                && !(self.seam_brush && view == 0)
                && response.double_clicked()
                && self.mode == SelectMode::Edges
                && let (Some(e), Some(doc)) = (self.last_edge, self.session.document())
            {
                for x in select::edge_loop(&doc.mesh, e) {
                    self.sel_edges[x] = true;
                }
            }
        }
        // Box / lasso selection: left drag unless that binding navigates.
        if response.drag_started_by(PointerButton::Primary)
            && !self.brush
            && !uv_edit_active
            && !density_active
            && !(self.seam_brush && view == 0)
            && !primary_navigation
            && !gizmo_active
            && !gizmo_pointer_hit
            && !pin_active
            && let Some(p) = response.interact_pointer_pos()
        {
            self.drag_path = Some((view, vec![local(p)]));
        }
        if let (Some((v, path)), Some(p)) = (&mut self.drag_path, response.interact_pointer_pos())
            && *v == view
            && response.dragged_by(PointerButton::Primary)
        {
            let p = local(p);
            if !self.lasso {
                path.truncate(1);
                path.push(p);
            } else if path.last().is_none_or(|q| q.distance(p) >= 3.0) {
                path.push(p);
            }
        }
        let region: Option<Vec<Vec2>> = match &self.drag_path {
            Some((v, path)) if *v == view && path.len() >= 2 => Some(if self.lasso {
                path.clone()
            } else {
                let (a, b) = (path[0], path[path.len() - 1]);
                vec![a, Vec2::new(b.x, a.y), b, Vec2::new(a.x, b.y)]
            }),
            _ => None,
        };
        if self.seam_brush
            && view == 0
            && !primary_navigation
            && (response.drag_started_by(PointerButton::Primary)
                || response.dragged_by(PointerButton::Primary))
            && let Some(p) = response.interact_pointer_pos()
        {
            let p = local(p);
            if self
                .seam_brush_last
                .is_none_or(|last| last.distance(p) >= self.brush_radius * 0.25)
            {
                self.sample_seam_brush(p, size);
                self.seam_brush_last = Some(p);
            }
        }
        if response.drag_stopped() {
            if pin_active && view == 1 {
                if let Some(p) = response.interact_pointer_pos() {
                    let uv = self.camuv.screen_to_uv(local(p), size);
                    if let Some(drag) = &mut self.pin_drag {
                        drag.target = DVec2::new(uv.x as f64, uv.y as f64) + drag.pointer_offset;
                    }
                }
                self.finish_pin_drag();
            } else if uv_edit_active {
                if let Some(p) = response.interact_pointer_pos() {
                    self.sample_uv_edit(local(p), size);
                }
                self.finish_uv_edit(size);
            } else if density_active {
                if let Some(p) = response.interact_pointer_pos() {
                    self.sample_density_drag(local(p), size);
                }
                self.finish_density_drag();
            } else if gizmo_active && view == 1 {
                self.finish_gizmo_drag();
            } else if self.seam_brush && view == 0 {
                self.finish_seam_brush();
            } else if let Some(poly) = &region
                && poly.len() >= 3
            {
                self.region_select(view, poly, size, modifiers, false);
            }
            self.drag_path = None;
            self.brush_last = None;
        }
        if self.brush
            && !primary_navigation
            && (response.drag_started_by(PointerButton::Primary)
                || response.dragged_by(PointerButton::Primary))
            && let Some(p) = response.interact_pointer_pos()
        {
            let p = local(p);
            if self.brush_last.is_none_or(|(last_view, last)| {
                last_view != view || last.distance(p) >= self.brush_radius * 0.25
            }) {
                self.brush_select(view, p, size, modifiers);
                self.brush_last = Some((view, p));
            }
        }
        if uv_edit_active
            && response.clicked()
            && let Some(p) = response.interact_pointer_pos()
        {
            let p = local(p);
            self.uv_edit_drag = self.uv_edit_mode.map(|mode| UvEditDrag {
                mode,
                start: p,
                current: p,
                last_sample: p,
                weights: BTreeMap::new(),
            });
            self.sample_uv_edit(p, size);
            self.finish_uv_edit(size);
        }
        if density_active
            && response.clicked()
            && let Some(p) = response.interact_pointer_pos()
        {
            let p = local(p);
            self.density_drag = Some(DensityDrag {
                last_sample: p,
                weights: BTreeMap::new(),
            });
            self.sample_density_drag(p, size);
            self.finish_density_drag();
        }

        // Render.
        let ppp = ui.ctx().pixels_per_point();
        let px = [
            (rect.width() * ppp).round() as u32,
            (rect.height() * ppp).round() as u32,
        ];
        if let (Some(rs), Some(renderer)) = (&self.rs, &mut self.renderer)
            && px[0] > 0
            && px[1] > 0
        {
            let params = if view == 0 {
                ViewParams {
                    view_proj: self.cam3.view_proj(size.x / size.y),
                    uv_mode: false,
                    tint: match self.color_mode {
                        ColorMode::Checker => 0.0,
                        ColorMode::Texture => -1.0,
                        ColorMode::Distortion if !self.distortion_3d => 0.0,
                        ColorMode::Distortion => self.distortion_opacity,
                        _ => 1.0,
                    },
                    checker_cells: self.checker_cells,
                    light: (-self.cam3.forward() + Vec3::Y * 0.3).to_array(),
                    wireframe: self.wireframe || self.color_mode == ColorMode::Distortion,
                    heatmap: self.color_mode == ColorMode::Distortion && self.distortion_3d,
                }
            } else {
                ViewParams {
                    view_proj: self.camuv.view_proj(size),
                    uv_mode: true,
                    tint: match self.color_mode {
                        ColorMode::Checker => 0.0,
                        ColorMode::Texture => -1.0,
                        ColorMode::Distortion => self.distortion_opacity,
                        _ => 1.0,
                    },
                    checker_cells: self.checker_cells,
                    light: [0.0, 0.0, 1.0],
                    wireframe: self.wireframe || self.color_mode == ColorMode::Distortion,
                    heatmap: self.color_mode == ColorMode::Distortion,
                }
            };
            let tex = renderer.render(rs, view, px, &params);
            ui.painter().image(
                tex,
                rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        if view == 1
            && self.color_mode == ColorMode::Distortion
            && let (Some(doc), Some(scene)) = (self.session.document(), self.scene.as_ref())
        {
            let painter = ui.painter_at(rect);
            for &(_, tri) in scene
                .tris
                .iter()
                .filter(|(face, tri)| {
                    let island = scene.islands.island_of_face(*face as usize);
                    scene.distortion_maps[tri[1] as usize].is_none_or(|map| {
                        (map.uv_area < 0.0) != scene.report.islands[island].flipped
                    })
                })
                .take(128)
            {
                let q = tri.map(|c| doc.uv().uv_at_corner(c as usize));
                let center = (q[0] + q[1] + q[2]) / 3.0;
                if !center.is_finite() {
                    continue;
                }
                let local = self.camuv.uv_to_screen(center.as_vec2(), size);
                let pos = rect.min + EVec2::new(local.x, local.y);
                if !rect.contains(pos) {
                    continue;
                }
                let degenerate = scene.distortion_maps[tri[1] as usize].is_none();
                painter.circle_filled(pos, 7.0, Color32::from_black_alpha(210));
                painter.text(
                    pos,
                    egui::Align2::CENTER_CENTER,
                    if degenerate { "!" } else { "↺" },
                    egui::FontId::proportional(12.0),
                    Color32::WHITE,
                );
            }
        }
        if view == 1 && self.grid_visible {
            let painter = ui.painter_at(rect);
            for (u, v, number) in self.camuv.visible_udim_tiles(size) {
                let top_left = self
                    .camuv
                    .uv_to_screen(Vec2::new(u as f32, v as f32 + 1.0), size);
                let bottom_right = self
                    .camuv
                    .uv_to_screen(Vec2::new(u as f32 + 1.0, v as f32), size);
                let tile = Rect::from_two_pos(
                    rect.min + EVec2::new(top_left.x, top_left.y),
                    rect.min + EVec2::new(bottom_right.x, bottom_right.y),
                );
                let color = if number == 1001 {
                    Color32::from_white_alpha(190)
                } else {
                    Color32::from_white_alpha(105)
                };
                for (a, b) in [
                    (tile.left_top(), tile.right_top()),
                    (tile.right_top(), tile.right_bottom()),
                    (tile.right_bottom(), tile.left_bottom()),
                    (tile.left_bottom(), tile.left_top()),
                ] {
                    painter.line_segment([a, b], egui::Stroke::new(1.0, color));
                }
                let visible = tile.intersect(rect);
                if visible.width() >= 48.0 && visible.height() >= 24.0 {
                    let label_rect = Rect::from_min_size(
                        visible.min + EVec2::new(3.0, 3.0),
                        EVec2::new(44.0, 18.0),
                    );
                    painter.rect_filled(label_rect, 2.0, Color32::from_black_alpha(180));
                    painter.text(
                        label_rect.min + EVec2::new(3.0, 1.0),
                        egui::Align2::LEFT_TOP,
                        number.to_string(),
                        egui::FontId::monospace(12.0),
                        Color32::WHITE,
                    );
                }
            }
        }
        if let Some(poly) = &region
            && response.dragged_by(PointerButton::Primary)
        {
            let color = Color32::from_rgb(255, 170, 60);
            let pts: Vec<Pos2> = poly
                .iter()
                .map(|p| rect.min + EVec2::new(p.x, p.y))
                .collect();
            let painter = ui.painter_at(rect);
            if !self.lasso {
                let r = Rect::from_two_pos(pts[0], pts[2]);
                painter.rect_filled(r, 0.0, color.gamma_multiply(0.08));
            }
            painter.add(egui::Shape::closed_line(pts, (1.0, color)));
        }
        if self.brush
            && response.hovered()
            && let Some(p) = response.hover_pos()
        {
            let color = if self.brush_erase || modifiers.command {
                Color32::from_rgb(255, 95, 95)
            } else {
                Color32::from_rgb(110, 220, 130)
            };
            ui.painter_at(rect)
                .circle_stroke(p, self.brush_radius, egui::Stroke::new(1.5, color));
        }
        if uv_edit_active
            && response.hovered()
            && let Some(p) = response.hover_pos()
        {
            ui.painter_at(rect).circle_stroke(
                p,
                self.brush_radius,
                egui::Stroke::new(1.5, Color32::from_rgb(90, 200, 255)),
            );
        }
        if density_active
            && response.hovered()
            && let Some(p) = response.hover_pos()
        {
            ui.painter_at(rect).circle_stroke(
                p,
                self.brush_radius,
                egui::Stroke::new(1.5, Color32::from_rgb(255, 110, 190)),
            );
        }
        if self.seam_brush
            && view == 0
            && response.hovered()
            && let Some(p) = response.hover_pos()
        {
            ui.painter_at(rect).circle_stroke(
                p,
                self.brush_radius,
                egui::Stroke::new(1.5, Color32::from_rgb(255, 150, 45)),
            );
        }
        self.draw_pins(ui, rect, view, size);
        if view == 1 {
            self.draw_group_bounds(ui, rect, size);
            self.draw_uv_seams(ui, rect, size);
            self.draw_gizmo(ui, rect, size);
        }
        response.context_menu(|ui| self.viewport_context_menu(ui, view));
        self.viewport_chrome(ui, outer_rect, view);
    }

    fn viewport_context_menu(&mut self, ui: &mut egui::Ui, view: usize) {
        let mode = self.mode_for_view(view);
        let has_doc = self.session.document().is_some();
        let selected_edges = if mode == SelectMode::Edges {
            self.target_edges()
        } else {
            Vec::new()
        };
        let selected_faces = if matches!(
            mode,
            SelectMode::Faces | SelectMode::Islands | SelectMode::Objects
        ) {
            select::indices(&self.sel_faces)
        } else {
            Vec::new()
        };
        let selected_islands = if selected_faces.is_empty() {
            Vec::new()
        } else {
            self.target_islands()
        };
        let has_selection = match mode {
            SelectMode::Vertices => self.sel_verts.iter().any(|&selected| selected),
            SelectMode::Edges => !selected_edges.is_empty(),
            _ => !selected_faces.is_empty(),
        };
        let seam_edges = if let Some(doc) = self.session.document() {
            context_seam_edges(&doc.mesh, doc.uv(), mode, &selected_edges, &self.sel_faces)
        } else {
            Vec::new()
        };

        ui.label(egui::RichText::new(if view == 0 { "3D view" } else { "UV editor" }).strong());
        ui.weak(format!("{:?} selection", mode));
        ui.separator();
        if ui
            .add_enabled(has_doc, egui::Button::new("Select all"))
            .clicked()
        {
            self.select_all();
            ui.close();
        }
        if ui
            .add_enabled(has_selection, egui::Button::new("Invert selection"))
            .clicked()
        {
            self.invert_selection();
            ui.close();
        }
        if ui
            .add_enabled(has_selection, egui::Button::new("Clear selection"))
            .clicked()
        {
            self.clear_selection();
            ui.close();
        }
        ui.separator();

        if mode == SelectMode::Edges {
            if ui
                .add_enabled(
                    !selected_edges.is_empty(),
                    egui::Button::new("Mark Seam / Cut"),
                )
                .clicked()
            {
                self.execute(Command::Cut {
                    edges: selected_edges.clone(),
                });
                ui.close();
            }
            if ui
                .add_enabled(
                    !selected_edges.is_empty(),
                    egui::Button::new("Cut and unfold"),
                )
                .clicked()
            {
                self.execute(Command::CutUnfold {
                    edges: selected_edges.clone(),
                });
                ui.close();
            }
        } else if matches!(
            mode,
            SelectMode::Faces | SelectMode::Islands | SelectMode::Objects
        ) && ui
            .add_enabled(
                !selected_faces.is_empty(),
                egui::Button::new("Cut selection border"),
            )
            .clicked()
        {
            self.execute(Command::CutBorder {
                faces: selected_faces.clone(),
                unfold: false,
            });
            ui.close();
        }
        if matches!(
            mode,
            SelectMode::Edges | SelectMode::Faces | SelectMode::Islands | SelectMode::Objects
        ) {
            if ui
                .add_enabled(
                    !seam_edges.is_empty(),
                    egui::Button::new("Stitch / Clear Seam"),
                )
                .on_hover_text("Join selected seam edges only when their UV sides already coincide")
                .clicked()
            {
                self.execute(Command::ClearSeams {
                    edges: seam_edges.clone(),
                });
                ui.close();
            }
            if ui
                .add_enabled(!seam_edges.is_empty(), egui::Button::new("Weld seam edges"))
                .on_hover_text("Move UV sides together and weld the selected seams")
                .clicked()
            {
                self.execute(Command::Weld {
                    edges: seam_edges.clone(),
                });
                ui.close();
            }
        }
        if !selected_islands.is_empty() {
            ui.separator();
            if ui.button("Unfold selected islands").clicked() {
                self.execute(Command::Unfold {
                    islands: selected_islands.clone(),
                    method: Default::default(),
                    preserve_orientation: false,
                    border_weight: self.border_weight,
                    prevent_overlap: self.prevent_overlap,
                });
                ui.close();
            }
            if ui.button("Pack selected islands").clicked() {
                self.pack();
                ui.close();
            }
            if ui
                .add_enabled(
                    selected_islands.len() == 1,
                    egui::Button::new("Straighten island"),
                )
                .clicked()
            {
                self.execute(Command::StraightenIsland {
                    island: selected_islands[0],
                });
                ui.close();
            }
            if ui
                .add_enabled(
                    selected_islands.len() >= 2,
                    egui::Button::new("Stack matching islands"),
                )
                .clicked()
            {
                self.execute(Command::Stack {
                    islands: selected_islands.clone(),
                    mode: StackMode::Similar,
                    tolerance: 0.05,
                    reference: None,
                    preview: false,
                    conform: false,
                    max_distortion: 4.0,
                });
                ui.close();
            }
        }
        if matches!(
            mode,
            SelectMode::Faces | SelectMode::Islands | SelectMode::Objects
        ) {
            ui.separator();
            ui.label(egui::RichText::new("Island groups").strong());
            if ui
                .add_enabled(
                    selected_islands.len() >= 2,
                    egui::Button::new("Create group"),
                )
                .clicked()
            {
                let name = self.session.document().map_or_else(
                    || "Group 1".to_owned(),
                    |doc| next_group_name(doc.island_groups()),
                );
                self.execute(Command::CreateIslandGroup {
                    name,
                    parent: None,
                    children: Vec::new(),
                    islands: selected_islands.clone(),
                });
                ui.close();
            }
            let groups: Vec<_> = self
                .session
                .document()
                .zip(self.scene.as_ref())
                .map(|(doc, scene)| {
                    doc.island_groups()
                        .nodes()
                        .filter_map(|group| {
                            doc.island_groups()
                                .islands_in(group.id, &scene.islands)
                                .ok()
                                .map(|members| (group.id, group.name.clone(), members))
                        })
                        .collect()
                })
                .unwrap_or_default();
            ui.add_enabled_ui(!selected_islands.is_empty() && !groups.is_empty(), |ui| {
                ui.menu_button("Add to group", |ui| {
                    for (group, name, members) in &groups {
                        if ui
                            .add_enabled(
                                selected_islands.iter().any(|i| !members.contains(i)),
                                egui::Button::new(name),
                            )
                            .clicked()
                        {
                            self.execute(Command::AddIslandsToGroup {
                                group: *group,
                                islands: selected_islands.clone(),
                            });
                            ui.close();
                        }
                    }
                });
                ui.menu_button("Remove from group", |ui| {
                    for (group, name, members) in &groups {
                        let selected_members: Vec<_> = selected_islands
                            .iter()
                            .copied()
                            .filter(|i| members.contains(i))
                            .collect();
                        if ui
                            .add_enabled(!selected_members.is_empty(), egui::Button::new(name))
                            .clicked()
                        {
                            self.execute(Command::RemoveIslandsFromGroup {
                                group: *group,
                                islands: selected_members,
                            });
                            ui.close();
                        }
                    }
                });
            });
        }
        ui.separator();
        if ui
            .add_enabled(has_selection, egui::Button::new("Hide selection"))
            .clicked()
        {
            self.set_hidden(Some(false));
            ui.close();
        }
        if ui
            .add_enabled(has_selection, egui::Button::new("Isolate selection"))
            .clicked()
        {
            self.set_hidden(Some(true));
            ui.close();
        }
        if ui
            .add_enabled(has_doc, egui::Button::new("Show all"))
            .clicked()
        {
            self.set_hidden(None);
            ui.close();
        }
        if ui
            .add_enabled(has_selection, egui::Button::new("Frame selection"))
            .clicked()
        {
            self.frame_selection();
            ui.close();
        }
        ui.separator();
        if ui
            .add_enabled(self.session.can_undo(), egui::Button::new("Undo"))
            .clicked()
        {
            self.execute(Command::Undo);
            ui.close();
        }
        if ui
            .add_enabled(self.session.can_redo(), egui::Button::new("Redo"))
            .clicked()
        {
            self.execute(Command::Redo);
            ui.close();
        }
    }

    fn view_selection_controls(&mut self, ui: &mut egui::Ui, view: usize, compact: bool) {
        let options = [
            ("Vert", SelectMode::Vertices),
            ("Edge", SelectMode::Edges),
            ("Face", SelectMode::Faces),
            ("Isl", SelectMode::Islands),
            ("Obj", SelectMode::Objects),
        ];
        let current = self.mode_for_view(view);
        if compact {
            let name = options
                .iter()
                .find(|(_, mode)| *mode == current)
                .map_or("Select", |(name, _)| *name);
            ui.menu_button(format!("Select: {name}"), |ui| {
                for (name, mode) in options {
                    if view == 1 && mode == SelectMode::Objects {
                        continue;
                    }
                    if ui.selectable_label(current == mode, name).clicked() {
                        self.set_view_mode(view, mode);
                        ui.close();
                    }
                }
            });
        } else {
            for (name, mode) in options {
                if view == 1 && mode == SelectMode::Objects {
                    continue;
                }
                let active = current == mode;
                let color = if active {
                    Color32::BLACK
                } else {
                    ui.visuals().text_color()
                };
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new(name).color(color)).fill(if active {
                            theme::ORANGE
                        } else {
                            theme::SURFACE
                        }),
                    )
                    .on_hover_text(format!(
                        "Select {name} in {}",
                        if view == 0 { "3D" } else { "UV" }
                    ))
                    .clicked()
                {
                    self.set_view_mode(view, mode);
                }
            }
        }
    }

    fn view_visibility_menu(&mut self, ui: &mut egui::Ui, view: usize) {
        ui.menu_button("Hide/Show", |ui| {
            if ui.button("Hide selected").clicked() {
                self.activate_view(view);
                self.set_hidden(Some(false));
                ui.close();
            }
            if ui.button("Isolate selected").clicked() {
                self.activate_view(view);
                self.set_hidden(Some(true));
                ui.close();
            }
            if ui.button("Show all").clicked() {
                self.activate_view(view);
                self.set_hidden(None);
                ui.close();
            }
        });
    }

    fn uv_display_menu(&mut self, ui: &mut egui::Ui) {
        let current = match self.color_mode {
            ColorMode::Checker => "Checker",
            ColorMode::Texture => "Texture",
            ColorMode::Islands => "Islands",
            ColorMode::Distortion => "UV Distortion",
            ColorMode::Flipped => "Flipped UVs",
            ColorMode::Overlaps => "Overlaps",
            ColorMode::TexelDensity => "Texel Density",
        };
        ui.menu_button(format!("Display: {current}"), |ui| {
            for (mode, label) in [
                (ColorMode::Checker, "Checker"),
                (ColorMode::Texture, "Texture"),
                (ColorMode::Islands, "Islands"),
                (ColorMode::Distortion, "UV Distortion"),
                (ColorMode::Flipped, "Flipped UVs"),
                (ColorMode::Overlaps, "Overlaps"),
                (ColorMode::TexelDensity, "Texel Density"),
            ] {
                if ui
                    .add_enabled(
                        mode != ColorMode::Texture || self.texture_path.is_some(),
                        egui::Button::selectable(mode == self.color_mode, label),
                    )
                    .clicked()
                {
                    self.color_mode = mode;
                    self.gpu_dirty = true;
                    ui.close();
                }
            }
            if self.texture_path.is_none() {
                ui.weak("Load a PNG in the Visualization panel to enable Texture");
            }
        });
    }

    fn draw_3d_orientation_gizmo(&self, ui: &egui::Ui, outer: Rect) {
        if !self.show_3d_gizmo {
            return;
        }
        let center = outer.right_top() + EVec2::new(-43.0, 78.0);
        let forward = self.cam3.forward();
        let right = forward.cross(Vec3::Y).normalize_or_zero();
        let up = right.cross(forward).normalize_or_zero();
        let painter = ui.painter();
        painter.circle_filled(center, 3.0, Color32::from_rgb(190, 193, 198));
        for (axis, label, color) in [
            (Vec3::X, "X", Color32::from_rgb(244, 91, 77)),
            (Vec3::Y, "Y", Color32::from_rgb(112, 212, 83)),
            (Vec3::Z, "Z", Color32::from_rgb(76, 157, 248)),
        ] {
            let end = center + EVec2::new(axis.dot(right), -axis.dot(up)) * 21.0;
            painter.line_segment([center, end], egui::Stroke::new(2.0, color));
            painter.circle_filled(end, 7.0, color);
            painter.text(
                end,
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(10.0),
                Color32::BLACK,
            );
        }
    }

    fn viewport_chrome(&mut self, ui: &mut egui::Ui, outer: Rect, view: usize) {
        let header = Rect::from_min_max(outer.min, outer.min + EVec2::new(outer.width(), 43.0));
        let footer = Rect::from_min_max(outer.max - EVec2::new(outer.width(), 28.0), outer.max);
        let painter = ui.painter();
        painter.rect_filled(header, 4.0, Color32::from_rgb(26, 28, 32));
        painter.rect_filled(footer, 4.0, Color32::from_rgb(23, 25, 29));
        painter.line_segment(
            [header.left_bottom(), header.right_bottom()],
            egui::Stroke::new(1.0, Color32::from_rgb(59, 62, 69)),
        );
        painter.line_segment(
            [footer.left_top(), footer.right_top()],
            egui::Stroke::new(1.0, Color32::from_rgb(59, 62, 69)),
        );

        ui.scope_builder(
            egui::UiBuilder::new().max_rect(header.shrink2(EVec2::new(10.0, 5.0))),
            |ui| {
                ui.set_clip_rect(header);
                ui.horizontal(|ui| {
                    if view == 0 {
                        ui.menu_button("Perspective", |ui| {
                            if ui.button("Frame all").clicked() {
                                self.frame_views();
                                ui.close();
                            }
                            if ui.button("Frame selection").clicked() {
                                self.frame_selection();
                                ui.close();
                            }
                        });
                        ui.separator();
                        self.view_selection_controls(ui, view, outer.width() < 540.0);
                        ui.separator();
                        self.view_visibility_menu(ui, view);
                        if outer.width() >= 690.0 {
                            ui.separator();
                            for (label, wire) in [("Solid", false), ("Wire", true)] {
                                let active = self.wireframe == wire;
                                if ui
                                    .add(
                                        egui::Button::new(egui::RichText::new(label).color(
                                            if active {
                                                Color32::BLACK
                                            } else {
                                                Color32::WHITE
                                            },
                                        ))
                                        .fill(if active {
                                            theme::ORANGE
                                        } else {
                                            theme::SURFACE
                                        }),
                                    )
                                    .clicked()
                                {
                                    self.wireframe = wire;
                                }
                            }
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Fit").on_hover_text("Frame all").clicked() {
                                self.frame_views();
                            }
                            let color = if self.show_3d_gizmo {
                                Color32::BLACK
                            } else {
                                Color32::WHITE
                            };
                            if ui
                                .add(
                                    egui::Button::new(egui::RichText::new("Gizmo").color(color))
                                        .fill(if self.show_3d_gizmo {
                                            theme::ORANGE
                                        } else {
                                            theme::SURFACE
                                        }),
                                )
                                .on_hover_text("Show 3D orientation axes")
                                .clicked()
                            {
                                self.show_3d_gizmo = !self.show_3d_gizmo;
                            }
                        });
                    } else {
                        ui.label(egui::RichText::new("UV Editor").strong());
                        ui.separator();
                        self.view_selection_controls(ui, view, outer.width() < 720.0);
                        ui.separator();
                        self.view_visibility_menu(ui, view);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button("Fit")
                                .on_hover_text("Frame all UVs")
                                .clicked()
                            {
                                self.frame_views();
                            }
                            let zoom_percent = (self.camuv.zoom / 400.0 * 100.0).round();
                            ui.menu_button(format!("{zoom_percent:.0}%"), |ui| {
                                for percent in [50.0_f32, 100.0, 200.0, 400.0] {
                                    if ui.button(format!("{percent:.0}%")).clicked() {
                                        self.camuv.zoom = 400.0 * percent / 100.0;
                                        ui.close();
                                    }
                                }
                            });
                            self.uv_display_menu(ui);
                            if ui
                                .add(
                                    egui::Button::new(egui::RichText::new("Grid").color(
                                        if self.grid_visible {
                                            Color32::BLACK
                                        } else {
                                            Color32::WHITE
                                        },
                                    ))
                                    .fill(
                                        if self.grid_visible {
                                            theme::ORANGE
                                        } else {
                                            theme::SURFACE
                                        },
                                    ),
                                )
                                .on_hover_text("Show UV grid")
                                .clicked()
                            {
                                self.grid_visible = !self.grid_visible;
                                self.gpu_dirty = true;
                            }
                        });
                    }
                });
            },
        );

        let hint = if view == 0 {
            "LMB  Select     Shift + LMB  Add     MMB  Orbit     Wheel  Zoom"
        } else {
            "LMB  Select / move islands     MMB  Pan     Wheel  Zoom"
        };
        ui.painter().text(
            footer.left_center() + EVec2::new(10.0, 0.0),
            egui::Align2::LEFT_CENTER,
            hint,
            egui::FontId::proportional(11.0),
            Color32::from_rgb(170, 174, 180),
        );
        ui.painter().rect_stroke(
            outer,
            5.0,
            egui::Stroke::new(1.0, Color32::from_rgb(63, 66, 73)),
            egui::StrokeKind::Inside,
        );
        if view == 0 {
            self.draw_3d_orientation_gizmo(ui, outer);
        }
    }

    /// Pinned vertices, then selected vertices (vertex mode), as dots.
    fn draw_pins(&self, ui: &egui::Ui, rect: Rect, view: usize, size: Vec2) {
        let Some(doc) = self.session.document() else {
            return;
        };
        self.draw_vertices(
            ui,
            rect,
            view,
            size,
            doc.pins(),
            Color32::from_rgb(255, 60, 90),
        );
        if self.mode_for_view(view) == SelectMode::Vertices {
            let yellow = Color32::from_rgb(255, 235, 25);
            self.draw_vertices(ui, rect, view, size, &self.sel_verts, yellow);
        }
    }

    /// Dots on the flagged mesh vertices (every UV copy in the UV view),
    /// skipping hidden faces.
    fn draw_vertices(
        &self,
        ui: &egui::Ui,
        rect: Rect,
        view: usize,
        size: Vec2,
        flags: &[bool],
        fill: Color32,
    ) {
        let Some(doc) = self.session.document() else {
            return;
        };
        if !flags.iter().any(|&p| p) {
            return;
        }
        let mut seen = vec![
            false;
            if view == 0 {
                flags.len()
            } else {
                doc.uv().uvs.len()
            }
        ];
        let painter = ui.painter_at(rect);
        for c in 0..doc.mesh.corner_count() {
            let v = doc.mesh.corner_vertex(c);
            if !flags.get(v).copied().unwrap_or(false) || self.is_hidden(doc.mesh.corner_face(c)) {
                continue;
            }
            let key = if view == 0 { v } else { doc.uv().corner_uv(c) };
            if std::mem::replace(&mut seen[key], true) {
                continue;
            }
            if let Some(p) = self.corner_screen(view, c, size) {
                let at = rect.min + EVec2::new(p.x, p.y);
                painter.circle(at, 3.5, fill, (1.0, Color32::BLACK));
            }
        }
    }

    /// Screen position (view-local points) of a corner in a view.
    fn corner_screen(&self, view: usize, c: usize, size: Vec2) -> Option<Vec2> {
        let doc = self.session.document()?;
        if view == 0 {
            let clip = self.cam3.view_proj(size.x / size.y)
                * doc.mesh.corner_position(c).as_vec3().extend(1.0);
            if clip.w <= 0.0 {
                return None; // behind the camera
            }
            let ndc = clip.truncate() / clip.w;
            Some(Vec2::new(
                (ndc.x + 1.0) / 2.0 * size.x,
                (1.0 - ndc.y) / 2.0 * size.y,
            ))
        } else {
            let uv = self
                .pin_drag
                .as_ref()
                .map_or(doc.uv(), |drag| &drag.preview);
            Some(self.camuv.uv_to_screen(uv.uv_at_corner(c).as_vec2(), size))
        }
    }

    /// Hover picking is limited to the visible face and a short screen-space
    /// distance from one of its edges. Large meshes skip hover preview to keep
    /// pointer motion responsive; click selection remains available.
    fn hover_edge_3d(&self, pointer: Vec2, size: Vec2) -> Option<usize> {
        let doc = self.session.document()?;
        let scene = self.scene.as_ref()?;
        if doc.mesh.face_count() > 100_000 {
            return None;
        }
        let vp = self.cam3.view_proj(size.x / size.y.max(1.0));
        let ndc = Vec2::new(
            pointer.x / size.x * 2.0 - 1.0,
            1.0 - pointer.y / size.y * 2.0,
        );
        let inverse = vp.inverse();
        let near = inverse.project_point3(ndc.extend(0.0)).as_dvec3();
        let far = inverse.project_point3(ndc.extend(1.0)).as_dvec3();
        let (face, _) = scene::pick_face_3d(&doc.mesh, scene, near, (far - near).normalize())?;
        doc.mesh
            .face_corners(face)
            .filter_map(|corner| {
                let a = self.corner_screen(0, corner, size)?.as_dvec2();
                let b = self
                    .corner_screen(0, doc.mesh.next(corner), size)?
                    .as_dvec2();
                let distance = scene::segment_distance(pointer.as_dvec2(), a, b);
                (distance <= 12.0).then_some((doc.mesh.corner_edge(corner), distance))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)))
            .map(|(edge, _)| edge)
    }

    fn update_edge_path_preview(&mut self, pointer: Option<Vec2>, size: Vec2, shift: bool) {
        let active = shift && self.mode == SelectMode::Edges && !self.busy();
        if !active || pointer.is_none() || self.last_edge.is_none() {
            if self.path_preview_edges.iter().any(|&edge| edge) {
                self.path_preview_edges.fill(false);
                self.gpu_dirty = true;
                self.ctx.request_repaint();
            }
            self.path_preview_key = None;
            self.path_preview_pointer = None;
            return;
        }
        let pointer = pointer.unwrap();
        let from = self.last_edge.unwrap();
        let moved = self
            .path_preview_pointer
            .is_none_or(|previous| previous.distance(pointer) >= 3.0);
        let source_changed = self
            .path_preview_key
            .is_some_and(|(source, _)| source != from);
        if !moved && !source_changed {
            return;
        }
        if self.path_preview_checked.elapsed() < Duration::from_millis(40) {
            return;
        }
        self.path_preview_checked = Instant::now();
        self.path_preview_pointer = Some(pointer);
        let key = self.hover_edge_3d(pointer, size).map(|to| (from, to));
        if key == self.path_preview_key {
            return;
        }
        self.path_preview_key = key;
        self.path_preview_edges.fill(false);
        if let (Some((from, to)), Some(doc)) = (key, self.session.document())
            && let Some(path) = select::edge_path(&doc.mesh, from, to)
        {
            for edge in path {
                self.path_preview_edges[edge] = true;
            }
        }
        self.gpu_dirty = true;
        self.ctx.request_repaint();
    }

    /// Faces mode: faces whose center is inside the polygon (box or lasso);
    /// islands mode: their whole islands. Edges mode: edges with both ends
    /// inside. Vertices mode: vertices inside. Shift adds, Ctrl removes,
    /// otherwise the selection is replaced. The 3D view takes only visible
    /// elements unless X-ray is on. Hidden faces are never selected.
    fn brush_select(&mut self, view: usize, center: Vec2, size: Vec2, modifiers: Modifiers) {
        let remove = if modifiers.shift {
            false
        } else {
            self.brush_erase || modifiers.command
        };
        let mut brush_modifiers = Modifiers::NONE;
        brush_modifiers.command = remove;
        brush_modifiers.shift = !remove;
        self.region_select(
            view,
            &brush_polygon(center, self.brush_radius),
            size,
            brush_modifiers,
            true,
        );
    }

    fn sample_seam_brush(&mut self, center: Vec2, size: Vec2) {
        let (Some(doc), Some(scene)) = (self.session.document(), &self.scene) else {
            return;
        };
        let mesh = &doc.mesh;
        let ids = (!self.xray).then(|| {
            scene::IdBuffer::new(
                mesh,
                scene,
                self.cam3.view_proj(size.x / size.y),
                size.x.ceil() as usize,
                size.y.ceil() as usize,
            )
        });
        let mut hit = vec![false; mesh.edge_count()];
        for c in 0..mesh.corner_count() {
            let face = mesh.corner_face(c);
            if self.is_hidden(face) {
                continue;
            }
            let edge = mesh.corner_edge(c);
            if doc.uv().edge_kind(mesh, edge) != EdgeKind::Interior {
                continue;
            }
            let (Some(a), Some(b)) = (
                self.corner_screen(0, c, size),
                self.corner_screen(0, mesh.next(c), size),
            ) else {
                continue;
            };
            let closest = closest_on_segment(center, a, b);
            if center.distance(closest) > self.brush_radius {
                continue;
            }
            if let Some(ids) = &ids {
                let (c0, c1) = mesh.edge_corners(edge);
                let faces = [Some(mesh.corner_face(c0)), c1.map(|c| mesh.corner_face(c))];
                let visible = (-2..=2).any(|dy| {
                    (-2..=2).any(|dx| {
                        ids.at(closest + Vec2::new(dx as f32, dy as f32))
                            .is_some_and(|f| faces.contains(&Some(f)))
                    })
                });
                if !visible {
                    continue;
                }
            }
            hit[edge] = true;
        }
        for (selected, &new) in self.seam_brush_edges.iter_mut().zip(&hit) {
            *selected |= new;
        }
        if hit.iter().any(|&value| value) {
            self.gpu_dirty = true;
        }
    }

    fn finish_seam_brush(&mut self) {
        self.seam_brush_last = None;
        let edges: Vec<usize> = self
            .seam_brush_edges
            .iter()
            .enumerate()
            .filter_map(|(edge, &selected)| selected.then_some(edge))
            .collect();
        self.seam_brush_edges.fill(false);
        self.gpu_dirty = true;
        if !edges.is_empty() {
            self.execute(Command::Cut { edges });
        }
    }

    fn region_select(
        &mut self,
        view: usize,
        poly: &[Vec2],
        size: Vec2,
        m: Modifiers,
        brush_mode: bool,
    ) {
        if view == 1 {
            self.group_selection = None;
        }
        let Some(doc) = self.session.document() else {
            return;
        };
        let mesh = &doc.mesh;
        let uv = doc.uv();
        let (lo, hi) = poly
            .iter()
            .fold((Vec2::INFINITY, Vec2::NEG_INFINITY), |(lo, hi), &p| {
                (lo.min(p), hi.max(p))
            });
        let inside = |p: Option<Vec2>| {
            p.is_some_and(|p| {
                p.cmpge(lo).all() && p.cmple(hi).all() && scene::point_in_polygon(p, poly)
            })
        };
        let on = !m.command;
        // 3D without X-ray: only what is visible counts.
        let ids = match (&self.scene, view == 0 && !self.xray) {
            (Some(scene), true) => Some(scene::IdBuffer::new(
                mesh,
                scene,
                self.cam3.view_proj(size.x / size.y),
                size.x.ceil() as usize,
                size.y.ceil() as usize,
            )),
            _ => None,
        };
        let replace = !m.shift && !m.command;
        match self.mode {
            SelectMode::Vertices => {
                let mut hit = vec![false; mesh.vertex_count()];
                for c in 0..mesh.corner_count() {
                    let f = mesh.corner_face(c);
                    if self.is_hidden(f) {
                        continue;
                    }
                    let Some(p) = self
                        .corner_screen(view, c, size)
                        .filter(|&p| inside(Some(p)))
                    else {
                        continue;
                    };
                    // Visible vertex: a pixel next to it shows this corner's face.
                    let visible = ids.as_ref().is_none_or(|ids| {
                        (-2..=2).any(|dy| {
                            (-2..=2)
                                .any(|dx| ids.at(p + Vec2::new(dx as f32, dy as f32)) == Some(f))
                        })
                    });
                    if visible {
                        hit[mesh.corner_vertex(c)] = true;
                    }
                }
                if replace {
                    self.sel_verts.iter_mut().for_each(|s| *s = false);
                }
                for v in select::indices(&hit) {
                    self.sel_verts[v] = on;
                }
                self.last_vertex = None;
            }
            SelectMode::Islands | SelectMode::Faces | SelectMode::Objects => {
                let Some(scene) = &self.scene else { return };
                let mut hit = vec![false; mesh.face_count()];
                if let Some(ids) = &ids {
                    // Visible pixels of the region.
                    let (x0, y0) = (lo.x.max(0.0) as usize, lo.y.max(0.0) as usize);
                    let (x1, y1) = (
                        (hi.x.ceil() as usize).min(ids.w),
                        (hi.y.ceil() as usize).min(ids.h),
                    );
                    for y in y0..y1 {
                        for x in x0..x1 {
                            let p = Vec2::new(x as f32 + 0.5, y as f32 + 0.5);
                            if let Some(f) = ids.at(p)
                                && scene::point_in_polygon(p, poly)
                            {
                                hit[f] = true;
                            }
                        }
                    }
                }
                for f in (0..mesh.face_count()).filter(|&f| ids.is_none() && !self.is_hidden(f)) {
                    let corners = mesh.face_corners(f);
                    let k = corners.len() as f32;
                    let pts: Option<Vec<Vec2>> =
                        corners.map(|c| self.corner_screen(view, c, size)).collect();
                    let center = pts.as_ref().map(|p| p.iter().sum::<Vec2>() / k);
                    let brush_hit = brush_mode
                        && pts.as_ref().is_some_and(|points| {
                            let brush_center =
                                poly.iter().copied().sum::<Vec2>() / poly.len() as f32;
                            brush_hits_face(points, brush_center, self.brush_radius)
                        });
                    if inside(center) || brush_hit {
                        hit[f] = true;
                    }
                }
                if brush_mode && view == 1 {
                    let center = poly.iter().copied().sum::<Vec2>() / poly.len() as f32;
                    if let Some(f) = scene::pick_face_uv(
                        uv,
                        scene,
                        self.camuv.screen_to_uv(center, size).as_dvec2(),
                    ) {
                        hit[f] = true;
                    }
                }
                if self.mode == SelectMode::Islands {
                    let islands = select::convert(
                        mesh,
                        &scene.islands,
                        Elem::Face,
                        &hit,
                        Elem::Island,
                        false,
                    );
                    hit = select::convert(
                        mesh,
                        &scene.islands,
                        Elem::Island,
                        &islands,
                        Elem::Face,
                        false,
                    );
                }
                if self.mode == SelectMode::Objects {
                    hit = expand_object_faces(doc.face_objects(), doc.objects.len(), &hit);
                }
                if replace {
                    self.sel_faces.iter_mut().for_each(|s| *s = false);
                }
                for f in select::indices(&hit) {
                    self.sel_faces[f] = on && !self.is_hidden(f);
                }
            }
            SelectMode::Edges => {
                let mut hit = vec![false; mesh.edge_count()];
                // Visible edge: a pixel near its midpoint shows one of its faces.
                let visible = |c: usize, a: Vec2, b: Vec2| {
                    let Some(ids) = &ids else { return true };
                    let e = mesh.corner_edge(c);
                    let (c0, c1) = mesh.edge_corners(e);
                    let faces = [Some(c0), c1].map(|c| c.map(|c| mesh.corner_face(c)));
                    let mid = (a + b) / 2.0;
                    (-2..=2).any(|dy| {
                        (-2..=2).any(|dx| {
                            ids.at(mid + Vec2::new(dx as f32, dy as f32))
                                .is_some_and(|f| faces.contains(&Some(f)))
                        })
                    })
                };
                for c in (0..mesh.corner_count()).filter(|&c| !self.is_hidden(mesh.corner_face(c)))
                {
                    let (a, b) = (
                        self.corner_screen(view, c, size),
                        self.corner_screen(view, mesh.next(c), size),
                    );
                    if (if brush_mode {
                        a.zip(b).is_some_and(|(a, b)| inside(Some((a + b) / 2.0)))
                    } else {
                        inside(a) && inside(b)
                    }) && visible(c, a.expect("inside"), b.expect("inside"))
                    {
                        hit[mesh.corner_edge(c)] = true;
                    }
                }
                if replace {
                    self.sel_edges.iter_mut().for_each(|s| *s = false);
                }
                for (e, _) in hit.iter().enumerate().filter(|(_, h)| **h) {
                    self.sel_edges[e] = on;
                }
                self.last_vertex = None;
            }
        }
        self.gpu_dirty = true;
    }

    fn click(&mut self, view: usize, p: Vec2, size: Vec2, m: Modifiers) {
        if view == 1
            && self.mode == SelectMode::Islands
            && !m.shift
            && let Some(group) = self.group_at_screen(p, size)
        {
            self.select_group_in_uv(group);
            return;
        }
        if view == 1 {
            self.group_selection = None;
        }
        let (Some(doc), Some(scene)) = (self.session.document(), &self.scene) else {
            return;
        };
        let mesh = &doc.mesh;
        let uv = doc.uv();
        // Screen positions of a corner in the clicked view.
        let vp3 = self.cam3.view_proj(size.x / size.y);
        let to_screen = |c: usize| -> DVec2 {
            if view == 0 {
                let ndc = vp3.project_point3(mesh.corner_position(c).as_vec3());
                DVec2::new(
                    ((ndc.x + 1.0) / 2.0 * size.x) as f64,
                    ((1.0 - ndc.y) / 2.0 * size.y) as f64,
                )
            } else {
                self.camuv
                    .uv_to_screen(uv.uv_at_corner(c).as_vec2(), size)
                    .as_dvec2()
            }
        };
        let face = if view == 0 {
            let ndc = Vec2::new(p.x / size.x * 2.0 - 1.0, 1.0 - p.y / size.y * 2.0);
            let inv = vp3.inverse();
            let near = inv.project_point3(ndc.extend(0.0)).as_dvec3();
            let far = inv.project_point3(ndc.extend(1.0)).as_dvec3();
            scene::pick_face_3d(mesh, scene, near, (far - near).normalize()).map(|h| h.0)
        } else {
            scene::pick_face_uv(uv, scene, self.camuv.screen_to_uv(p, size).as_dvec2())
        };
        let cursor = p.as_dvec2();

        match self.mode {
            SelectMode::Islands => {
                let island = face.map(|f| scene.islands.island_of_face(f));
                let selected = face.is_some_and(|f| self.sel_faces[f]);
                if !m.shift {
                    self.sel_faces.iter_mut().for_each(|s| *s = false);
                }
                if let Some(i) = island {
                    self.select_island_faces(i, !(m.shift && selected));
                    for (s, &h) in self.sel_faces.iter_mut().zip(&self.hidden) {
                        *s &= !h;
                    }
                }
            }
            SelectMode::Objects => {
                let object = face.map(|f| doc.face_objects()[f] as usize);
                let selected = face.is_some_and(|f| self.sel_faces[f]);
                if !m.shift {
                    self.sel_faces.fill(false);
                }
                if let Some(object) = object {
                    let on = !(m.shift && selected);
                    for (f, &id) in doc.face_objects().iter().enumerate() {
                        if id as usize == object {
                            self.sel_faces[f] = on && !self.is_hidden(f);
                        }
                    }
                }
            }
            SelectMode::Faces => {
                let selected = face.is_some_and(|f| self.sel_faces[f]);
                if !m.shift {
                    self.sel_faces.iter_mut().for_each(|s| *s = false);
                }
                if let Some(f) = face {
                    self.sel_faces[f] = !(m.shift && selected);
                }
            }
            SelectMode::Vertices => {
                // Nearest corner: of the clicked face in 3D; any visible one in UV.
                let candidates: Vec<usize> = match (view, face) {
                    (0, Some(f)) => mesh.face_corners(f).collect(),
                    (0, None) => Vec::new(),
                    _ => (0..mesh.corner_count())
                        .filter(|&c| !self.is_hidden(mesh.corner_face(c)))
                        .collect(),
                };
                let best = candidates
                    .into_iter()
                    .map(|c| (c, cursor.distance(to_screen(c))))
                    .filter(|&(_, d)| view == 0 || d < 12.0)
                    .min_by(|a, b| a.1.total_cmp(&b.1));
                let Some((c, _)) = best else {
                    if !m.shift && !m.ctrl {
                        self.clear_selection();
                    }
                    return;
                };
                let v = mesh.corner_vertex(c);
                if m.ctrl
                    && let Some(from) = self.last_vertex
                {
                    for pe in seams::vertex_path(mesh, from, v).unwrap_or_default() {
                        for x in mesh.edge_vertices(pe) {
                            self.sel_verts[x] = true;
                        }
                    }
                    self.sel_verts[v] = true;
                } else if m.shift {
                    self.sel_verts[v] = !self.sel_verts[v];
                } else {
                    self.sel_verts.iter_mut().for_each(|s| *s = false);
                    self.sel_verts[v] = true;
                }
                self.last_vertex = Some(v);
            }
            SelectMode::Edges => {
                // Candidate corners: the clicked face's edges in 3D; any visible edge in UV.
                let candidates: Vec<usize> = match (view, face) {
                    (0, Some(f)) => mesh.face_corners(f).collect(),
                    (0, None) => Vec::new(),
                    _ => (0..mesh.corner_count())
                        .filter(|&c| !self.is_hidden(mesh.corner_face(c)))
                        .collect(),
                };
                let best = candidates
                    .into_iter()
                    .map(|c| {
                        (
                            c,
                            scene::segment_distance(cursor, to_screen(c), to_screen(mesh.next(c))),
                        )
                    })
                    .filter(|&(_, d)| view == 0 || d < 12.0)
                    .min_by(|a, b| a.1.total_cmp(&b.1));
                let Some((c, _)) = best else {
                    if !m.shift && !m.ctrl {
                        self.clear_selection();
                    }
                    return;
                };
                let e = mesh.corner_edge(c);
                let n = mesh.next(c);
                let near = if cursor.distance(to_screen(c)) <= cursor.distance(to_screen(n)) {
                    c
                } else {
                    n
                };
                let vertex = mesh.corner_vertex(near);
                if m.shift
                    && let Some(from) = self.last_edge
                {
                    if let Some(path) = select::edge_path(mesh, from, e) {
                        for edge in path {
                            self.sel_edges[edge] = true;
                        }
                    } else {
                        self.set_status("No connected edge path".into());
                        return;
                    }
                } else if m.ctrl
                    && let Some(from) = self.last_vertex
                {
                    let path = seams::vertex_path(mesh, from, vertex).unwrap_or_default();
                    for pe in path {
                        self.sel_edges[pe] = true;
                    }
                    self.sel_edges[e] = true;
                } else if m.shift {
                    self.sel_edges[e] = true;
                } else {
                    self.sel_edges.iter_mut().for_each(|s| *s = false);
                    self.sel_edges[e] = true;
                }
                self.last_vertex = Some(vertex);
                self.last_edge = Some(e);
            }
        }
        self.gpu_dirty = true;
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.pump();
        self.poll_bridge();
        self.shortcuts(&ctx);
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .find(|p| !p.as_os_str().is_empty())
        });
        if let Some(path) = dropped {
            if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("jsonl") || e.eq_ignore_ascii_case("json"))
            {
                self.run_script(&path);
            } else {
                self.open(&path);
            }
        }

        egui::Panel::top("menu").show(ui, |ui| self.menu(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        let max_tools_width = (ui.available_width() - 368.0).clamp(260.0, 700.0);
        let tools_panel = egui::Panel::right("tools")
            .default_size(self.tools_width)
            .min_size(260.0)
            .max_size(max_tools_width)
            .resizable(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.reference_tools(ui));
            });
        self.tools_width = tools_panel.response.rect.width();
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let rect = ui.available_rect_before_wrap();
            let gap = 8.0;
            let available_width = (rect.width() - gap).max(1.0);
            let minimum_share = (180.0 / available_width).clamp(0.15, 0.5);
            let split = self
                .viewport_split
                .clamp(minimum_share, 1.0 - minimum_share);
            let left_width = available_width * split;
            let right_width = available_width - left_width;
            let left = Rect::from_min_size(rect.min, EVec2::new(left_width, rect.height()));
            let right = Rect::from_min_size(
                rect.min + EVec2::new(left_width + gap, 0.0),
                EVec2::new(right_width, rect.height()),
            );
            let workspace_height = if self.group_workspace_open {
                self.group_workspace_height
                    .clamp(105.0, (right.height() - 200.0).max(105.0))
            } else {
                32.0
            };
            let workspace_gap = 6.0;
            let uv_view = Rect::from_min_max(
                right.min,
                right.max - EVec2::new(0.0, workspace_height + workspace_gap),
            );
            let workspace = Rect::from_min_max(
                right.max - EVec2::new(right.width(), workspace_height),
                right.max,
            );
            let workspace_divider =
                Rect::from_min_max(uv_view.left_bottom(), workspace.right_top());
            let divider = Rect::from_min_size(
                rect.min + EVec2::new(left_width, 0.0),
                EVec2::new(gap, rect.height()),
            );
            let handle = ui.interact(
                divider,
                ui.scope_id().with("viewport_divider"),
                Sense::drag(),
            );
            if handle.dragged()
                && let Some(pointer) = ui.input(|input| input.pointer.interact_pos())
            {
                self.viewport_split = ((pointer.x - rect.left() - gap * 0.5) / available_width)
                    .clamp(minimum_share, 1.0 - minimum_share);
                ui.ctx().request_repaint();
            }
            let divider_color = if handle.hovered() || handle.dragged() {
                theme::ORANGE
            } else {
                Color32::from_rgb(62, 65, 73)
            };
            ui.painter()
                .rect_filled(divider.shrink2(EVec2::new(2.0, 0.0)), 2.0, divider_color);
            handle.on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
            // Framing needs the document, which a running worker holds: wait for it.
            if self.frame_pending && self.scene.is_some() && self.session.document().is_some() {
                self.view_sizes = [
                    Vec2::new(left.width(), (left.height() - 71.0).max(1.0)),
                    Vec2::new(uv_view.width(), (uv_view.height() - 71.0).max(1.0)),
                ];
                self.frame_views();
                self.frame_pending = false;
            }
            self.upload_scene();
            self.viewport(ui, left, 0);
            self.viewport(ui, uv_view, 1);
            if self.group_workspace_open {
                let resize = ui.interact(
                    workspace_divider,
                    ui.scope_id().with("group_workspace_resize"),
                    Sense::drag(),
                );
                if resize.dragged()
                    && let Some(pointer) = ui.input(|input| input.pointer.interact_pos())
                {
                    self.group_workspace_height = (right.bottom() - pointer.y)
                        .clamp(105.0, (right.height() - 200.0).max(105.0));
                    ui.ctx().request_repaint();
                }
                ui.painter().rect_filled(
                    workspace_divider,
                    1.0,
                    if resize.hovered() || resize.dragged() {
                        theme::ORANGE
                    } else {
                        theme::BORDER
                    },
                );
                resize.on_hover_cursor(egui::CursorIcon::ResizeVertical);
            }
            self.group_workspace(ui, workspace);
        });

        if self.show_advanced_tools {
            let mut open = self.show_advanced_tools;
            egui::Window::new("Advanced Tools")
                .open(&mut open)
                .resizable(true)
                .default_size(egui::vec2(850.0, 680.0))
                .default_pos(egui::pos2(180.0, 100.0))
                .min_width(680.0)
                .show(&ctx, |ui| {
                    ui.heading("All UV tools");
                    ui.label("Detailed controls for unwrap, layout, groups and analysis. Common actions are also in the right panel and Group Workspace.");
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_salt("advanced_tools_window_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            self.tools(ui);
                        });
                });
            self.show_advanced_tools = open;
        }

        if self.show_help {
            egui::Window::new("UvCraft help")
                .open(&mut self.show_help)
                .resizable(true)
                .default_width(430.0)
                .show(&ctx, |ui| {
                    ui.heading("Navigation");
                    ui.label(format!("3D: {} to orbit; {} to pan; wheel to zoom.", self.hotkeys.mouse.orbit_3d.display(), self.hotkeys.mouse.pan_3d.display()));
                    ui.label(format!("UV: {} to pan; wheel to zoom at the pointer.", self.hotkeys.mouse.pan_uv.display()));
                    ui.label(format!("{} frames all visible geometry; {} frames the visible selection.", self.hotkeys.keys[0], self.hotkeys.extras[0].display()));
                    ui.separator();
                    ui.heading("Selection");
                    ui.label("Choose Vertices, Edges, Faces or Islands independently in each viewport header, then click or drag a box/lasso. In UV Islands mode, drag a polygon to move its island; drag an already selected island to move the selection together.");
                    ui.label("Shift adds to selection. In Edges mode, Shift+hover previews the connected shortest path and Shift+click selects it. Ctrl+click retains the vertex path shortcut; double-click selects the edge loop.");
                    ui.label("Mark Seam cuts selected edges. Clear Seam joins only UV sides already aligned; Weld can move islands to align separated sides.");
                    ui.label("Use Hide/Show in either viewport header. Right-click a selection for cut, stitch, weld, island, visibility and history actions. The Selection tools can also grow, shrink or convert the current selection.");
                    ui.separator();
                    ui.heading("UV workflow");
                    ui.label(format!("1. Open an OBJ, glTF or GLB mesh ({} or drop a file).", self.hotkeys.extras[11].display()));
                    ui.label(format!("2. Cut selected edges ({}), or use Auto seams; then Unfold ({}).", self.hotkeys.keys[1], self.hotkeys.keys[3]));
                    ui.label(format!("3. Set texel density if needed, then Pack ({}). Save with {}.", self.hotkeys.keys[4], self.hotkeys.extras[12].display()));
                    ui.label("Use the Display modes and quality selections to inspect distortion, overlaps and flipped UVs.");
                    ui.separator();
                    ui.heading("Editing");
                    ui.label(format!("{} undoes; {} redoes. The command log and macros use the same editing history.", self.hotkeys.extras[13].display(), self.hotkeys.extras[14].display()));
                });
        }

        if self.show_preferences {
            let mut reset = false;
            let mut save_layout = false;
            let mut load_layout = None;
            let mut delete_layout = None;
            egui::Window::new("Preferences")
                .open(&mut self.show_preferences)
                .resizable(true)
                .default_width(300.0)
                .show(&ctx, |ui| {
                    ui.label("These defaults are saved automatically for the next launch.");
                    ui.checkbox(&mut self.dark_theme, "Dark theme");
                    ui.checkbox(&mut self.wireframe, "Wireframe");
                    ui.checkbox(&mut self.overlay_seams, "Show seams");
                    ui.checkbox(&mut self.overlay_borders, "Show island borders");
                    ui.checkbox(&mut self.show_outliner, "Scene outliner");
                    ui.separator();
                    ui.heading("Window layout");
                    ui.horizontal(|ui| {
                        if ui.button("Balanced").clicked() {
                            self.tools_width = 350.0;
                            self.outliner_width = 210.0;
                            self.viewport_split = 0.5;
                            self.show_outliner = false;
                        }
                        if ui.button("3D focus").clicked() {
                            self.viewport_split = 0.7;
                        }
                        if ui.button("UV focus").clicked() {
                            self.viewport_split = 0.3;
                        }
                    });
                    ui.add(
                        egui::Slider::new(&mut self.tools_width, 260.0..=700.0).text("Tools width"),
                    );
                    ui.add(
                        egui::Slider::new(&mut self.outliner_width, 140.0..=260.0)
                            .text("Outliner width"),
                    );
                    ui.add(
                        egui::Slider::new(&mut self.viewport_split, 0.15..=0.85).text("3D share"),
                    );
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.layout_name);
                        save_layout = ui.button("Save layout").clicked();
                    });
                    for name in self.layout_presets.keys() {
                        ui.horizontal(|ui| {
                            if ui.button(name).clicked() {
                                load_layout = Some(name.clone());
                            }
                            if ui
                                .small_button("×")
                                .on_hover_text("Delete layout")
                                .clicked()
                            {
                                delete_layout = Some(name.clone());
                            }
                        });
                    }
                    ui.separator();
                    ui.add(
                        egui::Slider::new(&mut self.checker_cells, 2.0..=64.0)
                            .text("Checker cells"),
                    );
                    ui.add(
                        egui::DragValue::new(&mut self.resolution)
                            .range(1..=16384)
                            .prefix("Resolution "),
                    );
                    ui.add(
                        egui::DragValue::new(&mut self.density)
                            .range(0.001..=1e6)
                            .prefix("Texel density "),
                    );
                    ui.add(
                        egui::DragValue::new(&mut self.padding)
                            .range(0.0..=1e6)
                            .prefix("Padding px "),
                    );
                    ui.add(
                        egui::DragValue::new(&mut self.margin)
                            .range(0.0..=1e6)
                            .prefix("Margin px "),
                    );
                    ui.add(
                        egui::DragValue::new(&mut self.seam_angle)
                            .range(0.0..=180.0)
                            .prefix("Seam angle ° "),
                    );
                    ui.add(
                        egui::DragValue::new(&mut self.select_angle)
                            .range(0.0..=180.0)
                            .prefix("Selection angle ° "),
                    );
                    ui.checkbox(&mut self.pack_rotate, "Rotate when packing");
                    ui.checkbox(&mut self.pack_rescale, "Rescale when packing");
                    ui.checkbox(&mut self.pack_pixel_aligned, "Pixel aligned packing");
                    ui.checkbox(&mut self.pack_padding_perfect, "Padding Perfect packing");
                    ui.separator();
                    ui.heading("Action shortcuts");
                    ui.label("Presets set keyboard shortcuts and mouse navigation.");
                    ui.horizontal(|ui| {
                        if ui.button("Maya").clicked() {
                            self.hotkeys = Hotkeys::maya();
                        }
                        if ui.button("3ds Max").clicked() {
                            self.hotkeys = Hotkeys::max();
                        }
                        if ui.button("Blender").clicked() {
                            self.hotkeys = Hotkeys::blender();
                        }
                    });
                    ui.collapsing("Mouse navigation", |ui| {
                        let mut nav = self.hotkeys.mouse;
                        let old_orbit = nav.orbit_3d;
                        edit_mouse_binding(ui, "3D orbit", &mut nav.orbit_3d);
                        edit_optional_mouse_binding(
                            ui,
                            "3D orbit",
                            &mut nav.orbit_3d_alt,
                            nav.orbit_3d,
                        );
                        if nav.orbit_3d == nav.pan_3d {
                            nav.pan_3d = old_orbit;
                        }
                        let old_pan = nav.pan_3d;
                        edit_mouse_binding(ui, "3D pan", &mut nav.pan_3d);
                        edit_optional_mouse_binding(ui, "3D pan", &mut nav.pan_3d_alt, nav.pan_3d);
                        if nav.pan_3d == nav.orbit_3d {
                            nav.orbit_3d = old_pan;
                        }
                        edit_mouse_binding(ui, "UV pan", &mut nav.pan_uv);
                        edit_optional_mouse_binding(ui, "UV pan", &mut nav.pan_uv_alt, nav.pan_uv);
                        ui.label("Mouse wheel zooms both views.");
                        if nav.valid() {
                            self.hotkeys.mouse = nav;
                        } else {
                            ui.colored_label(
                                Color32::YELLOW,
                                "Orbit and pan need different bindings.",
                            );
                        }
                    });
                    for (action, label) in HOTKEY_ACTIONS.into_iter().enumerate() {
                        let mut chosen = self.hotkeys.keys[action];
                        egui::ComboBox::from_label(label)
                            .selected_text(chosen.to_string())
                            .show_ui(ui, |ui| {
                                for key in 'A'..='Z' {
                                    if key != 'H' && key != 'L' {
                                        ui.selectable_value(&mut chosen, key, key.to_string());
                                    }
                                }
                            });
                        if chosen != self.hotkeys.keys[action] {
                            self.hotkeys.assign(action, chosen);
                        }
                    }
                    ui.collapsing("Other shortcuts", |ui| {
                        for (action, label) in EXTRA_ACTIONS.into_iter().enumerate() {
                            let mut binding = self.hotkeys.extras[action].clone();
                            ui.horizontal(|ui| {
                                ui.label(label);
                                egui::ComboBox::from_id_salt(("extra_key", action))
                                    .selected_text(&binding.key)
                                    .show_ui(ui, |ui| {
                                        for key in 'A'..='Z' {
                                            ui.selectable_value(
                                                &mut binding.key,
                                                key.to_string(),
                                                key.to_string(),
                                            );
                                        }
                                        for key in ["Period", "Comma", "Escape"] {
                                            ui.selectable_value(&mut binding.key, key.into(), key);
                                        }
                                    });
                                ui.checkbox(&mut binding.command, "Ctrl");
                                ui.checkbox(&mut binding.shift, "Shift");
                                ui.checkbox(&mut binding.alt, "Alt");
                            });
                            if binding != self.hotkeys.extras[action] {
                                self.hotkeys.assign_extra(action, binding);
                            }
                        }
                    });
                    reset = ui.button("Reset defaults").clicked();
                });
            if save_layout {
                let name = self.layout_name.trim().to_owned();
                if !name.is_empty() {
                    self.layout_presets
                        .insert(name.clone(), self.current_layout());
                    self.set_status(format!("saved layout {name}"));
                }
            }
            if let Some(name) = load_layout
                && let Some(layout) = self.layout_presets.get(&name).cloned()
            {
                self.apply_layout(&layout);
            }
            if let Some(name) = delete_layout {
                self.layout_presets.remove(&name);
            }
            if reset {
                self.apply_preferences(&Preferences::default());
            }
        }
        let current_prefs = self.preferences();
        if current_prefs != self.saved_prefs {
            if current_prefs.dark_theme != self.saved_prefs.dark_theme {
                theme::apply(&ctx, current_prefs.dark_theme);
            }
            if current_prefs.checker_cells != self.saved_prefs.checker_cells
                || current_prefs.grid_visible != self.saved_prefs.grid_visible
                || current_prefs.grid_divisions != self.saved_prefs.grid_divisions
                || current_prefs.overlay_seams != self.saved_prefs.overlay_seams
                || current_prefs.overlay_borders != self.saved_prefs.overlay_borders
            {
                self.gpu_dirty = true;
            }
            if let Some(path) = &self.prefs_path
                && let Err(e) = current_prefs.save(path)
            {
                eprintln!("preferences: {e}");
            }
            self.saved_prefs = current_prefs;
        }

        self.frames += 1;
        let busy = self.busy();
        if let Some(shot) = &mut self.screenshot {
            if shot.done.load(Ordering::SeqCst) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else if !shot.requested && self.frames > 5 && !busy {
                shot.requested = true;
                let (path, done) = (shot.path.clone(), shot.done.clone());
                let repaint = ctx.clone();
                ctx.request_screenshot(move |img| {
                    let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
                    let [w, h] = img.size;
                    if let Err(e) = image::save_buffer(
                        &path,
                        &bytes,
                        w as u32,
                        h as u32,
                        image::ExtendedColorType::Rgba8,
                    ) {
                        eprintln!("screenshot: {e}");
                    }
                    done.store(true, Ordering::SeqCst);
                    repaint.request_repaint();
                });
            }
            ctx.request_repaint();
        }
    }
}

#[cfg(test)]
mod selection_set_tests {
    use super::*;

    #[test]
    fn context_group_names_skip_existing_names() {
        let mesh = CoreMesh::new(
            vec![glam::DVec3::ZERO, glam::DVec3::X, glam::DVec3::Y],
            &[[0, 1, 2]],
        )
        .unwrap();
        let uv = UvLayer {
            name: "UV".into(),
            corner_uv: vec![0, 1, 2],
            uvs: vec![DVec2::ZERO, DVec2::X, DVec2::Y],
        };
        let islands = Islands::compute(&mesh, &uv);
        let mut groups = IslandGroups::new(1);
        assert_eq!(next_group_name(&groups), "Group 1");
        groups.create("Group 1", None, &[], &[0], &islands).unwrap();
        groups.create("Custom", None, &[], &[], &islands).unwrap();
        assert_eq!(next_group_name(&groups), "Group 2");
        groups.create("Group 2", None, &[], &[], &islands).unwrap();
        assert_eq!(next_group_name(&groups), "Group 3");
    }

    #[test]
    fn group_container_picks_empty_space_and_prefers_smallest_overlap() {
        let bounds = [
            (1, Rect::from_min_max(Pos2::ZERO, Pos2::new(100.0, 100.0))),
            (
                2,
                Rect::from_min_max(Pos2::new(30.0, 30.0), Pos2::new(70.0, 70.0)),
            ),
        ];
        assert_eq!(
            pick_group_container(&bounds, Vec2::new(10.0, 10.0)),
            Some(1)
        );
        assert_eq!(
            pick_group_container(&bounds, Vec2::new(50.0, 50.0)),
            Some(2)
        );
        assert_eq!(pick_group_container(&bounds, Vec2::new(110.0, 50.0)), None);
    }

    #[test]
    fn selected_edges_are_ordered_as_one_open_path() {
        let positions = (0..5)
            .map(|i| glam::DVec3::new(i as f64, 0.0, 0.0))
            .collect();
        let mesh = CoreMesh::new(positions, &[[0, 1, 4], [1, 2, 4], [2, 3, 4]]).unwrap();
        let edge = |a: usize, b: usize| {
            (0..mesh.edge_count())
                .find(|&e| {
                    let [x, y] = mesh.edge_vertices(e);
                    [x.min(y), x.max(y)] == [a.min(b), a.max(b)]
                })
                .unwrap()
        };
        let e01 = edge(0, 1);
        let e12 = edge(1, 2);
        let e23 = edge(2, 3);
        assert_eq!(
            ordered_edge_path(&mesh, &[e23, e01, e12]).unwrap(),
            [e01, e12, e23]
        );
        assert!(
            ordered_edge_path(&mesh, &[e01, e23])
                .unwrap_err()
                .contains("disconnected")
        );
        assert!(
            ordered_edge_path(&mesh, &[e01, e12, edge(1, 4)])
                .unwrap_err()
                .contains("branch")
        );
    }

    #[test]
    fn context_menu_uses_seams_incident_to_the_active_selection() {
        let mesh = CoreMesh::new(
            vec![
                glam::DVec3::ZERO,
                glam::DVec3::X,
                glam::DVec3::Y,
                glam::DVec3::ONE,
            ],
            &[[0, 1, 2], [2, 1, 3]],
        )
        .unwrap();
        let uv = UvLayer {
            name: "UV".into(),
            corner_uv: (0..6).collect(),
            uvs: vec![DVec2::ZERO; 6],
        };
        let seam = mesh.corner_edge(1);
        assert_eq!(
            context_seam_edges(&mesh, &uv, SelectMode::Edges, &[seam], &[]),
            [seam]
        );
        assert!(context_seam_edges(&mesh, &uv, SelectMode::Edges, &[], &[true, false]).is_empty());
        assert_eq!(
            context_seam_edges(&mesh, &uv, SelectMode::Faces, &[], &[true, false]),
            [seam]
        );
        assert!(
            context_seam_edges(&mesh, &uv, SelectMode::Vertices, &[seam], &[true, false])
                .is_empty()
        );
    }

    #[test]
    fn packing_keeps_only_complete_editable_root_groups() {
        let positions = (0..9)
            .map(|i| glam::DVec3::new(i as f64, 0.0, 0.0))
            .collect();
        let mesh = CoreMesh::new(positions, &[[0, 1, 2], [3, 4, 5], [6, 7, 8]]).unwrap();
        let uv = UvLayer {
            name: "UV".into(),
            corner_uv: (0..9).collect(),
            uvs: (0..9).map(|i| DVec2::new(i as f64, 0.0)).collect(),
        };
        let islands = Islands::compute(&mesh, &uv);
        let mut groups = IslandGroups::new(3);
        groups.create("Pair", None, &[], &[0, 1], &islands).unwrap();
        let clear = [false; 3];
        assert_eq!(
            packing_groups_for_roots(&groups, &islands, &[], &clear, &clear),
            vec![vec![0, 1]]
        );
        assert!(packing_groups_for_roots(&groups, &islands, &[0], &clear, &clear).is_empty());
        assert_eq!(
            packing_groups_for_roots(&groups, &islands, &[0, 1], &clear, &clear),
            vec![vec![0, 1]]
        );
        assert!(
            packing_groups_for_roots(&groups, &islands, &[], &[false, true, false], &clear)
                .is_empty()
        );
    }

    #[test]
    fn direct_drag_selects_one_or_moves_existing_multi_selection() {
        let positions = (0..9)
            .map(|i| glam::DVec3::new(i as f64, 0.0, 0.0))
            .collect();
        let mesh = CoreMesh::new(positions, &[[0, 1, 2], [3, 4, 5], [6, 7, 8]]).unwrap();
        let uv = UvLayer {
            name: "UV".into(),
            corner_uv: (0..9).collect(),
            uvs: (0..9).map(|i| DVec2::new(i as f64, 0.0)).collect(),
        };
        let islands = Islands::compute(&mesh, &uv);
        let mut selection = vec![false; 3];
        assert!(select_drag_island(&islands, &mut selection, 0, false));
        assert_eq!(selection, [true, false, false]);
        assert!(select_drag_island(&islands, &mut selection, 1, true));
        assert_eq!(selection, [true, true, false]);
        assert!(!select_drag_island(&islands, &mut selection, 0, false));
        assert_eq!(selection, [true, true, false]);
        assert!(select_drag_island(&islands, &mut selection, 2, false));
        assert_eq!(selection, [false, false, true]);
    }

    #[test]
    fn angle_query_uses_first_visible_selected_face_only_in_face_modes() {
        let selected = [true, false, true];
        let hidden = [true, false, false];
        assert_eq!(
            angle_selection_seed(SelectMode::Faces, &selected, &hidden),
            Some(2)
        );
        assert_eq!(
            angle_selection_seed(SelectMode::Objects, &selected, &hidden),
            Some(2)
        );
        assert_eq!(
            angle_selection_seed(SelectMode::Edges, &selected, &hidden),
            None
        );
        assert_eq!(
            angle_selection_seed(SelectMode::Faces, &selected, &[true, false, true]),
            None
        );
    }

    #[test]
    fn object_selection_expands_noncontiguous_faces_of_same_object() {
        let face_objects = [0, 1, 0, 2, 1];
        assert_eq!(
            expand_object_faces(&face_objects, 3, &[false, false, true, false, false]),
            [true, false, true, false, false]
        );
        assert_eq!(
            expand_object_faces(&face_objects, 3, &[false, true, false, true, false]),
            [false, true, false, true, true]
        );
    }

    #[test]
    fn brush_reaches_faces_whose_centroid_is_outside_the_circle() {
        let circle = brush_polygon(Vec2::ZERO, 12.0);
        assert_eq!(circle.len(), 20);
        assert!(scene::point_in_polygon(Vec2::ZERO, &circle));
        let large_face = [
            Vec2::new(-100.0, -100.0),
            Vec2::new(100.0, -100.0),
            Vec2::new(100.0, 100.0),
            Vec2::new(-100.0, 100.0),
        ];
        assert!(brush_hits_face(&large_face, Vec2::ZERO, 12.0));
        assert!(!brush_hits_face(&large_face, Vec2::new(150.0, 0.0), 12.0));
    }

    #[test]
    fn seam_brush_uses_nearest_point_on_each_edge() {
        let a = Vec2::new(0.0, 0.0);
        let b = Vec2::new(100.0, 0.0);
        assert_eq!(
            closest_on_segment(Vec2::new(50.0, 8.0), a, b),
            Vec2::new(50.0, 0.0)
        );
        assert_eq!(closest_on_segment(Vec2::new(-10.0, 0.0), a, b), a);
        assert_eq!(closest_on_segment(Vec2::new(110.0, 0.0), a, b), b);
    }

    #[test]
    fn masks_keep_their_original_element_kind() {
        let positions = vec![
            glam::DVec3::ZERO,
            glam::DVec3::X,
            glam::DVec3::Y,
            glam::DVec3::Z,
        ];
        let mesh = CoreMesh::new(positions.clone(), &[[0, 1, 2], [0, 2, 3]]).unwrap();
        let changed = CoreMesh::new(positions, &[[0, 1, 3], [1, 2, 3]]).unwrap();
        let signature = mesh_signature(&mesh);
        let faces = SelectionSet {
            name: "faces".into(),
            mode: SelectMode::Faces,
            mask: vec![true, false],
            mesh_signature: signature,
        };
        let islands = SelectionSet {
            name: "islands".into(),
            mode: SelectMode::Islands,
            mask: vec![true, false],
            mesh_signature: signature,
        };
        let edges = SelectionSet {
            name: "edges".into(),
            mode: SelectMode::Edges,
            mask: vec![true; mesh.edge_count()],
            mesh_signature: signature,
        };
        assert!(faces.compatible(&mesh));
        assert!(islands.compatible(&mesh));
        assert!(edges.compatible(&mesh));
        assert!(!edges.compatible(&changed));
    }

    #[test]
    fn selection_set_persists_under_its_mesh_path() {
        let path = std::env::temp_dir().join(format!(
            "uvcraft-selection-sets-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let set = SelectionSet {
            name: "rim".into(),
            mode: SelectMode::Edges,
            mask: vec![false, true, false],
            mesh_signature: 1234,
        };
        let mut prefs = Preferences::default();
        prefs
            .selection_sets
            .insert("/models/mesh.obj".into(), vec![set.clone()]);
        prefs.save(&path).unwrap();
        let loaded = Preferences::load(&path).unwrap();
        assert_eq!(loaded.selection_sets["/models/mesh.obj"], vec![set]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pin_release_commits_once_and_drops_stale_preview() {
        let uv = UvLayer {
            name: "UV".into(),
            corner_uv: vec![0],
            uvs: vec![DVec2::ZERO],
        };
        let make_drag = |target, pending| PinDrag {
            vertex: 3,
            start: DVec2::ZERO,
            target,
            pointer_offset: DVec2::ZERO,
            preview: uv.clone(),
            targets: vec![0],
            pins: vec![false; 4],
            last_solved: DVec2::ZERO,
            pending,
        };
        assert!(make_drag(DVec2::ZERO, None).release_command().is_none());
        let (tx, rx) = mpsc::channel();
        let command = make_drag(DVec2::new(0.2, 0.1), Some(rx)).release_command();
        assert!(
            matches!(command, Some(Command::DragPin { vertex: 3, uv, iterations: 8 })
            if uv == [0.2, 0.1])
        );
        assert!(
            tx.send((DVec2::ZERO, uv)).is_err(),
            "released drag discards stale preview"
        );
    }
}
