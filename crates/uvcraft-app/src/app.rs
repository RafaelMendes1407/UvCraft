//! The UvCraft window: tool panel, 3D view, UV view, status bar.
//! Every edit goes through `uvcraft_commands::Session`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{
    self, Color32, Key, Modifiers, PointerButton, Pos2, Rect, Sense, Vec2 as EVec2,
};
use eframe::egui_wgpu::RenderState;
use glam::{DVec2, Vec2, Vec3};
use uvcraft_algo::projection::{Axis, Projection};
use uvcraft_algo::seams;
use uvcraft_commands::{Command, Outcome, Pivot, Session};
use uvcraft_core::EdgeKind;

use crate::camera::{OrbitCamera, UvCamera};
use crate::render::{Renderer, ViewParams};
use crate::scene::{self, ColorMode, Scene};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectMode {
    Islands,
    Edges,
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

pub struct App {
    session: Session,
    scene: Option<Scene>,
    rs: Option<RenderState>,
    renderer: Option<Renderer>,
    gpu_dirty: bool,
    cam3: OrbitCamera,
    camuv: UvCamera,
    frame_pending: bool,
    view_sizes: [Vec2; 2],

    mode: SelectMode,
    sel_faces: Vec<bool>,
    sel_edges: Vec<bool>,
    last_vertex: Option<usize>,

    color_mode: ColorMode,
    wireframe: bool,
    checker_cells: f32,

    seam_angle: f64,
    select_angle: f64,
    padding: f64,
    margin: f64,
    resolution: u32,
    density: f64,

    status: String,
    status_error: bool,
    screenshot: Option<Screenshot>,
    frames: u64,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, startup: Startup) -> Self {
        let rs = cc.wgpu_render_state.clone();
        let renderer = rs.as_ref().map(Renderer::new);
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut app = App {
            session: Session::new(),
            scene: None,
            rs,
            renderer,
            gpu_dirty: true,
            cam3: OrbitCamera::default(),
            camuv: UvCamera::default(),
            frame_pending: true,
            view_sizes: [Vec2::ONE; 2],
            mode: SelectMode::Islands,
            sel_faces: Vec::new(),
            sel_edges: Vec::new(),
            last_vertex: None,
            color_mode: ColorMode::Checker,
            wireframe: true,
            checker_cells: 16.0,
            seam_angle: 60.0,
            select_angle: 60.0,
            padding: 4.0,
            margin: 4.0,
            resolution: 2048,
            density: 512.0,
            status: "Open a mesh (File ▸ Open, Ctrl+O): .obj, .gltf or .glb".into(),
            status_error: false,
            screenshot: startup.screenshot.map(|path| Screenshot {
                path,
                requested: false,
                done: Arc::new(AtomicBool::new(false)),
            }),
            frames: 0,
        };
        if app.rs.is_none() {
            app.set_error("wgpu is not available: viewports are disabled".into());
        }
        if let Some(file) = startup.file {
            app.open(&file);
        }
        if let Some(script) = startup.script {
            app.run_script(&script);
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

    fn execute(&mut self, cmd: Command) -> bool {
        let name = cmd.name();
        let is_load = matches!(cmd, Command::Load { .. });
        match self.session.execute(cmd) {
            Ok(Outcome::Info(msg)) => self.set_status(format!("{name}: {msg}")),
            Ok(_) => self.set_status(format!("{name}: done")),
            Err(e) => {
                self.set_error(format!("{name}: {e}"));
                return false;
            }
        }
        if is_load {
            let doc = self.session.document().expect("loaded");
            self.sel_faces = vec![false; doc.mesh.face_count()];
            self.sel_edges = vec![false; doc.mesh.edge_count()];
            self.last_vertex = None;
            self.frame_pending = true;
        }
        self.refresh();
        true
    }

    fn refresh(&mut self) {
        self.scene = self
            .session
            .document()
            .map(|d| Scene::new(&d.mesh, d.uv(), self.resolution));
        self.gpu_dirty = true;
    }

    fn open(&mut self, path: &Path) {
        if self.execute(Command::Load {
            path: path.to_path_buf(),
        }) && let Some(doc) = self.session.document()
        {
            let r = &self.scene.as_ref().expect("refreshed").report;
            let mut msg = format!(
                "Loaded {} ({} faces, {} islands)",
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

    fn run_script(&mut self, path: &Path) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => return self.set_error(format!("{}: {e}", path.display())),
        };
        match uvcraft_commands::parse_script(&text) {
            Ok(cmds) => {
                for cmd in cmds {
                    if !self.execute(cmd) {
                        break;
                    }
                }
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
        if self.execute(Command::Save { path: path.clone() }) {
            self.set_status(format!("Saved {}", path.display()));
        }
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
        if let Some(p) = rfd::FileDialog::new()
            .add_filter(ext.to_uppercase(), &[ext.as_str()])
            .set_file_name(name)
            .save_file()
        {
            self.save(Some(p));
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

    fn target_edges(&self) -> Vec<usize> {
        self.sel_edges
            .iter()
            .enumerate()
            .filter(|&(_, &s)| s)
            .map(|(e, _)| e)
            .collect()
    }

    fn select_island_faces(&mut self, island: usize, on: bool) {
        if let Some(scene) = &self.scene {
            for &f in scene.islands.faces(island) {
                self.sel_faces[f as usize] = on;
            }
        }
        self.gpu_dirty = true;
    }

    fn clear_selection(&mut self) {
        self.sel_faces.iter_mut().for_each(|s| *s = false);
        self.sel_edges.iter_mut().for_each(|s| *s = false);
        self.last_vertex = None;
        self.gpu_dirty = true;
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

    // ------------------------------------------------------------ UI

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let (open, save, undo, redo, frame, cut, weld, unfold, pack, clear) = ctx.input_mut(|i| {
            let cmd = Modifiers::COMMAND;
            (
                i.consume_key(cmd, Key::O),
                i.consume_key(cmd, Key::S),
                i.consume_key(cmd, Key::Z),
                i.consume_key(cmd | Modifiers::SHIFT, Key::Z) || i.consume_key(cmd, Key::Y),
                i.consume_key(Modifiers::NONE, Key::F),
                i.consume_key(Modifiers::NONE, Key::C),
                i.consume_key(Modifiers::NONE, Key::W),
                i.consume_key(Modifiers::NONE, Key::U),
                i.consume_key(Modifiers::NONE, Key::P),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
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
        self.execute(Command::Pack {
            islands: self.target_islands(),
            padding_px: self.padding,
            margin_px: self.margin,
            resolution: self.resolution,
            orient: true,
            rotate_90: true,
        });
    }

    fn menu(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Open…  (Ctrl+O)").clicked() {
                    self.open_dialog();
                }
                let has_doc = self.session.document().is_some();
                if ui
                    .add_enabled(has_doc, egui::Button::new("Save  (Ctrl+S)"))
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
                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Edit", |ui| {
                if ui
                    .add_enabled(self.session.can_undo(), egui::Button::new("Undo  (Ctrl+Z)"))
                    .clicked()
                {
                    self.execute(Command::Undo);
                }
                if ui
                    .add_enabled(
                        self.session.can_redo(),
                        egui::Button::new("Redo  (Ctrl+Shift+Z)"),
                    )
                    .clicked()
                {
                    self.execute(Command::Redo);
                }
            });
            ui.menu_button("View", |ui| {
                if ui.button("Frame all  (F)").clicked() {
                    self.frame_views();
                }
                ui.checkbox(&mut self.wireframe, "Wireframe");
            });
        });
    }

    fn tools(&mut self, ui: &mut egui::Ui) {
        let has_doc = self.session.document().is_some();
        ui.add_enabled_ui(has_doc, |ui| {
            ui.heading("Selection");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.mode, SelectMode::Islands, "Islands");
                ui.selectable_value(&mut self.mode, SelectMode::Edges, "Edges");
            });
            ui.label(
                egui::RichText::new(match self.mode {
                    SelectMode::Islands => "Click: select · Shift: add/remove · Esc: clear",
                    SelectMode::Edges => {
                        "Click: select · Shift: add/remove · Ctrl: path from last · Esc: clear"
                    }
                })
                .small()
                .weak(),
            );
            ui.horizontal(|ui| {
                if ui.button("All islands").clicked() {
                    self.sel_faces.iter_mut().for_each(|s| *s = true);
                    self.gpu_dirty = true;
                }
                if ui.button("Clear").clicked() {
                    self.clear_selection();
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
                ui.add(
                    egui::DragValue::new(&mut self.select_angle)
                        .range(1.0..=179.0)
                        .suffix("°"),
                );
            });

            ui.separator();
            ui.heading("Seams");
            let n_edges = self.sel_edges.iter().filter(|&&s| s).count();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(n_edges > 0, egui::Button::new("Cut (C)"))
                    .clicked()
                {
                    self.execute(Command::Cut {
                        edges: self.target_edges(),
                    });
                }
                if ui
                    .add_enabled(n_edges > 0, egui::Button::new("Weld (W)"))
                    .clicked()
                {
                    self.execute(Command::Weld {
                        edges: self.target_edges(),
                    });
                }
                ui.label(format!("{n_edges} edges"));
            });
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

            ui.separator();
            ui.heading("Unwrap");
            ui.label(
                egui::RichText::new("Applies to selected islands, or all")
                    .small()
                    .weak(),
            );
            ui.horizontal_wrapped(|ui| {
                ui.label("Project:");
                for (label, mode) in [
                    ("Box", Projection::Box),
                    ("Best", Projection::Best),
                    ("X", Projection::Planar(Axis::X)),
                    ("Y", Projection::Planar(Axis::Y)),
                    ("Z", Projection::Planar(Axis::Z)),
                ] {
                    if ui.button(label).clicked() {
                        self.execute(Command::Project {
                            islands: self.target_islands(),
                            mode,
                        });
                    }
                }
            });
            if ui
                .button("Unfold (U)")
                .on_hover_text("LSCM: conformal flattening")
                .clicked()
            {
                self.execute(Command::Unfold {
                    islands: self.target_islands(),
                });
            }

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
            });
            if ui.button("Pack (P)").clicked() {
                self.pack();
            }
            ui.horizontal(|ui| {
                if ui.button("Set TD").clicked() {
                    self.execute(Command::SetTexelDensity {
                        islands: self.target_islands(),
                        density: self.density,
                        resolution: self.resolution,
                    });
                }
                ui.add(
                    egui::DragValue::new(&mut self.density)
                        .range(0.001..=1e6)
                        .suffix(" px/u"),
                );
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
                }
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
        ui.heading("Display");
        ui.horizontal(|ui| {
            let before = self.color_mode;
            ui.selectable_value(&mut self.color_mode, ColorMode::Checker, "Checker");
            ui.selectable_value(&mut self.color_mode, ColorMode::Islands, "Islands");
            ui.selectable_value(&mut self.color_mode, ColorMode::Distortion, "Distortion");
            if before != self.color_mode {
                self.gpu_dirty = true;
            }
        });
        ui.checkbox(&mut self.wireframe, "Wireframe");
        ui.add(egui::Slider::new(&mut self.checker_cells, 2.0..=64.0).text("checker"));

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

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let color = if self.status_error {
                Color32::from_rgb(255, 110, 100)
            } else {
                ui.visuals().text_color()
            };
            ui.colored_label(color, &self.status);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new("RMB/Alt+LMB orbit · MMB pan · wheel zoom · F frame")
                        .small()
                        .weak(),
                );
            });
        });
    }

    // ------------------------------------------------------------ viewports

    fn upload_scene(&mut self) {
        if !self.gpu_dirty {
            return;
        }
        let (Some(rs), Some(renderer)) = (&self.rs, &mut self.renderer) else {
            return;
        };
        let grid = scene::build_grid();
        match (self.session.document(), &self.scene) {
            (Some(doc), Some(scene)) => {
                let tris =
                    scene::build_tris(&doc.mesh, doc.uv(), scene, &self.sel_faces, self.color_mode);
                let (wire, edges) = scene::build_lines(&doc.mesh, doc.uv(), &self.sel_edges);
                renderer.set_scene(rs, &tris, &wire, &edges, &grid);
            }
            _ => renderer.set_scene(rs, &[], &[], &[], &grid),
        }
        self.gpu_dirty = false;
    }

    fn viewport(&mut self, ui: &mut egui::Ui, rect: Rect, view: usize) {
        let response = ui.allocate_rect(rect, Sense::click_and_drag());
        let size = Vec2::new(rect.width(), rect.height());
        self.view_sizes[view] = size;
        let (scroll, modifiers) = ui.input(|i| (i.smooth_scroll_delta.y, i.modifiers));
        let drag = response.drag_delta();
        let local = |p: Pos2| Vec2::new(p.x - rect.min.x, p.y - rect.min.y);

        if view == 0 {
            if response.dragged_by(PointerButton::Secondary) && !modifiers.shift
                || response.dragged_by(PointerButton::Primary) && modifiers.alt
            {
                self.cam3.orbit(drag.x, drag.y);
            }
            if response.dragged_by(PointerButton::Middle)
                || response.dragged_by(PointerButton::Secondary) && modifiers.shift
            {
                self.cam3.pan(drag.x, drag.y, size.y);
            }
            if response.hovered() && scroll != 0.0 {
                self.cam3.zoom((-scroll * 0.0015).exp());
            }
        } else {
            if response.dragged_by(PointerButton::Middle)
                || response.dragged_by(PointerButton::Secondary)
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
            && !modifiers.alt
            && let Some(p) = response.interact_pointer_pos()
        {
            self.click(view, local(p), size, modifiers);
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
                    tint: if self.color_mode == ColorMode::Checker {
                        0.0
                    } else {
                        1.0
                    },
                    checker_cells: self.checker_cells,
                    light: (-self.cam3.forward() + Vec3::Y * 0.3).to_array(),
                    wireframe: self.wireframe,
                }
            } else {
                ViewParams {
                    view_proj: self.camuv.view_proj(size),
                    uv_mode: true,
                    tint: if self.color_mode == ColorMode::Checker {
                        0.0
                    } else {
                        1.0
                    },
                    checker_cells: self.checker_cells,
                    light: [0.0, 0.0, 1.0],
                    wireframe: self.wireframe,
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
        let label = if view == 0 { "3D" } else { "UV" };
        ui.painter().text(
            rect.min + EVec2::new(8.0, 6.0),
            egui::Align2::LEFT_TOP,
            label,
            egui::FontId::proportional(14.0),
            Color32::from_white_alpha(160),
        );
    }

    fn click(&mut self, view: usize, p: Vec2, size: Vec2, m: Modifiers) {
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
                }
            }
            SelectMode::Edges => {
                // Candidate corners: the clicked face's edges in 3D; any edge in UV.
                let candidates: Vec<usize> = match (view, face) {
                    (0, Some(f)) => mesh.face_corners(f).collect(),
                    (0, None) => Vec::new(),
                    _ => (0..mesh.corner_count()).collect(),
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
                if m.ctrl
                    && let Some(from) = self.last_vertex
                {
                    let path = seams::vertex_path(mesh, from, vertex).unwrap_or_default();
                    for pe in path {
                        self.sel_edges[pe] = true;
                    }
                    self.sel_edges[e] = true;
                } else if m.shift {
                    self.sel_edges[e] = !self.sel_edges[e];
                } else {
                    self.sel_edges.iter_mut().for_each(|s| *s = false);
                    self.sel_edges[e] = true;
                }
                self.last_vertex = Some(vertex);
            }
        }
        self.gpu_dirty = true;
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.shortcuts(&ctx);

        egui::Panel::top("menu").show(ui, |ui| self.menu(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::left("tools")
            .resizable(true)
            .default_size(270.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.tools(ui));
            });
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let rect = ui.available_rect_before_wrap();
            let gap = 2.0;
            let half = (rect.width() - gap) / 2.0;
            let left = Rect::from_min_size(rect.min, EVec2::new(half, rect.height()));
            let right = Rect::from_min_size(
                rect.min + EVec2::new(half + gap, 0.0),
                EVec2::new(half, rect.height()),
            );
            if self.frame_pending && self.scene.is_some() {
                self.view_sizes = [
                    Vec2::new(left.width(), left.height()),
                    Vec2::new(right.width(), right.height()),
                ];
                self.frame_views();
                self.frame_pending = false;
            }
            self.upload_scene();
            self.viewport(ui, left, 0);
            self.viewport(ui, right, 1);
        });

        self.frames += 1;
        if let Some(shot) = &mut self.screenshot {
            if shot.done.load(Ordering::SeqCst) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else if !shot.requested && self.frames > 5 {
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
