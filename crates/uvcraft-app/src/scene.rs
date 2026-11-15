//! CPU-side data derived from the document: islands, report, render
//! vertices and picking.

use glam::{DVec2, DVec3, Vec3};
use uvcraft_algo::analysis::{
    self, DistortionValues, Report, TriangleMap, distortion_values, triangle_map,
};
use uvcraft_core::{EdgeKind, Islands, Mesh, UvLayer};

use crate::render::{LineVertex, TriVertex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Checker,
    Texture,
    Islands,
    Distortion,
    Flipped,
    Overlaps,
    TexelDensity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistortionMetric {
    Area,
    Conformal,
    Directional,
    Combined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistortionReference {
    Object,
    Selected,
    Island,
    TexelTarget,
    Raw,
}

#[derive(Debug, Clone)]
pub struct DistortionConfig {
    pub metric: DistortionMetric,
    pub reference: DistortionReference,
    pub sensitivity: f64,
    pub selected_islands: Vec<usize>,
    pub active_island: Option<usize>,
    pub isolate_active: bool,
    pub texel_target: f64,
    pub resolution: u32,
}

impl Default for DistortionConfig {
    fn default() -> Self {
        Self {
            metric: DistortionMetric::Area,
            reference: DistortionReference::Object,
            sensitivity: 1.0,
            selected_islands: Vec::new(),
            active_island: None,
            isolate_active: false,
            texel_target: 512.0,
            resolution: 1024,
        }
    }
}

pub struct Scene {
    pub islands: Islands,
    pub report: Report,
    pub intentional_stack_islands: Vec<bool>,
    /// Fan triangles: (face, corners).
    pub tris: Vec<(u32, [u32; 3])>,
    /// Base Jacobians, indexed by the middle corner of each fan triangle.
    pub distortion_maps: Vec<Option<TriangleMap>>,
    pub base_uvs: Vec<DVec2>,
    pub distortion_sums: Vec<(f64, f64)>,
    pub bbox: (Vec3, Vec3),
}

impl Scene {
    #[cfg(test)]
    pub fn new(mesh: &Mesh, uv: &UvLayer, resolution: u32) -> Self {
        Self::new_with_measurements(mesh, mesh, uv, resolution)
    }

    #[cfg(test)]
    pub fn new_with_measurements(
        mesh: &Mesh,
        measurement_mesh: &Mesh,
        uv: &UvLayer,
        resolution: u32,
    ) -> Self {
        Self::new_with_measurements_and_stacks(mesh, measurement_mesh, uv, resolution, &[])
    }

    pub fn new_with_measurements_and_stacks(
        mesh: &Mesh,
        measurement_mesh: &Mesh,
        uv: &UvLayer,
        resolution: u32,
        stack_groups: &[Vec<usize>],
    ) -> Self {
        let islands = Islands::compute(mesh, uv);
        let report = analysis::analyze_with_padding_and_stacks(
            measurement_mesh,
            uv,
            &islands,
            resolution,
            0.0,
            stack_groups,
        );
        let mut intentional_stack_islands = vec![false; islands.count()];
        for pair in &report.intentional_overlapping_island_pairs {
            intentional_stack_islands[pair[0]] = true;
            intentional_stack_islands[pair[1]] = true;
        }
        let tris = (0..mesh.face_count())
            .flat_map(|f| {
                mesh.fan_triangles(f)
                    .map(move |t| (f as u32, t.map(|c| c as u32)))
            })
            .collect();
        let mut distortion_maps = vec![None; mesh.corner_count()];
        let mut distortion_sums = vec![(0.0, 0.0); islands.count()];
        for face in 0..mesh.face_count() {
            for tri in mesh.fan_triangles(face) {
                let map = triangle_map(
                    tri.map(|c| measurement_mesh.corner_position(c)),
                    tri.map(|c| uv.uv_at_corner(c)),
                );
                if let Some(map) = map {
                    let sum = &mut distortion_sums[islands.island_of_face(face)];
                    sum.0 += map.area_3d;
                    sum.1 += map.uv_area.abs();
                }
                distortion_maps[tri[1]] = map;
            }
        }
        let bbox = mesh.positions().iter().fold(
            (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY)),
            |(lo, hi), p| (lo.min(p.as_vec3()), hi.max(p.as_vec3())),
        );
        Scene {
            islands,
            report,
            intentional_stack_islands,
            tris,
            distortion_maps,
            base_uvs: uv.uvs.clone(),
            distortion_sums,
            bbox,
        }
    }

    /// Rebuilds the drawn and pickable triangles without the hidden faces.
    pub fn set_hidden(&mut self, mesh: &Mesh, hidden: &[bool]) {
        self.tris = (0..mesh.face_count())
            .filter(|&f| !hidden.get(f).copied().unwrap_or(false))
            .flat_map(|f| {
                mesh.fan_triangles(f)
                    .map(move |t| (f as u32, t.map(|c| c as u32)))
            })
            .collect();
    }

    pub fn uv_bbox(uv: &UvLayer) -> (DVec2, DVec2) {
        uv.uvs
            .iter()
            .fold((DVec2::INFINITY, DVec2::NEG_INFINITY), |(lo, hi), &p| {
                (lo.min(p), hi.max(p))
            })
    }
}

/// Pleasant, well-separated color for island `i`.
pub fn island_color(i: usize) -> [f32; 3] {
    let h = (i as f32 * 0.618_034) % 1.0;
    let (s, v) = (0.5, 0.92);
    let k = |n: f32| {
        let k = (n + h * 6.0) % 6.0;
        v - v * s * k.min(4.0 - k).clamp(0.0, 1.0)
    };
    [k(5.0), k(3.0), k(1.0)]
}

fn render_uv(point: DVec2) -> [f32; 2] {
    let value = point.as_vec2();
    if value.is_finite() {
        value.to_array()
    } else {
        [0.0; 2]
    }
}

fn blend(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t)
}

fn distortion_color(
    values: Option<DistortionValues>,
    metric: DistortionMetric,
    sensitivity: f64,
) -> [f32; 3] {
    let Some(v) = values else {
        return [0.7, 0.25, 0.85];
    };
    let green = [0.2, 0.8, 0.35];
    let blue = [0.18, 0.42, 0.95];
    let yellow = [0.95, 0.82, 0.18];
    let orange = [0.98, 0.5, 0.12];
    let red = [0.95, 0.16, 0.12];
    let sensitivity = if sensitivity.is_finite() {
        sensitivity.clamp(0.01, 100.0)
    } else {
        1.0
    };
    let signed = match metric {
        DistortionMetric::Area => Some(v.area.log2()),
        DistortionMetric::Directional => Some(v.directional_log / std::f64::consts::LN_2),
        _ => None,
    };
    if let Some(value) = signed {
        let t = (value * sensitivity).clamp(-1.0, 1.0) as f32;
        if t < 0.0 {
            return blend(green, blue, -t);
        }
        return if t < 0.5 {
            blend(green, yellow, t * 2.0)
        } else {
            blend(yellow, red, t * 2.0 - 1.0)
        };
    }
    let magnitude = match metric {
        DistortionMetric::Conformal => v.conformal.log2(),
        DistortionMetric::Combined => v.combined / std::f64::consts::LN_2,
        _ => 0.0,
    };
    let t = (magnitude * sensitivity).clamp(0.0, 1.0) as f32;
    if t < 0.33 {
        blend(green, yellow, t / 0.33)
    } else if t < 0.66 {
        blend(yellow, orange, (t - 0.33) / 0.33)
    } else {
        blend(orange, red, (t - 0.66) / 0.34)
    }
}

fn density_color(density: f64, target: f64) -> [f32; 3] {
    if !density.is_finite() || density <= 0.0 || !target.is_finite() || target <= 0.0 {
        return [0.7, 0.25, 0.85];
    }
    let delta = (density / target).log2().clamp(-1.0, 1.0) as f32;
    let green = [0.2, 0.8, 0.35];
    let end = if delta < 0.0 {
        [0.2, 0.45, 0.95]
    } else {
        [0.95, 0.2, 0.15]
    };
    [0, 1, 2].map(|i| green[i] + (end[i] - green[i]) * delta.abs())
}

#[cfg(test)]
pub fn build_tris(
    mesh: &Mesh,
    uv: &UvLayer,
    scene: &Scene,
    selected_faces: &[bool],
    mode: ColorMode,
    target_density: f64,
) -> Vec<TriVertex> {
    build_tris_with_colors(mesh, uv, scene, selected_faces, mode, target_density, &[])
}

#[cfg(test)]
pub fn build_tris_with_colors(
    mesh: &Mesh,
    uv: &UvLayer,
    scene: &Scene,
    selected_faces: &[bool],
    mode: ColorMode,
    target_density: f64,
    group_colors: &[Option<[u8; 4]>],
) -> Vec<TriVertex> {
    build_tris_with_distortion(
        mesh,
        mesh,
        uv,
        scene,
        TriColorSettings {
            selected_faces,
            mode,
            target_density,
            group_colors,
            distortion: &DistortionConfig::default(),
        },
    )
}

type TriangleMapCache<'a> = (std::borrow::Cow<'a, [Option<TriangleMap>]>, Vec<(f64, f64)>);

fn distortion_maps_for_uv<'a>(
    measurement_mesh: &Mesh,
    uv: &UvLayer,
    scene: &'a Scene,
) -> TriangleMapCache<'a> {
    let dirty: Vec<bool> = uv
        .uvs
        .iter()
        .enumerate()
        .map(|(i, &point)| scene.base_uvs.get(i).is_none_or(|&base| base != point))
        .collect();
    if !dirty.iter().any(|&changed| changed) && uv.uvs.len() == scene.base_uvs.len() {
        return (
            std::borrow::Cow::Borrowed(&scene.distortion_maps),
            scene.distortion_sums.clone(),
        );
    }
    let mut maps = scene.distortion_maps.clone();
    let mut sums = scene.distortion_sums.clone();
    for face in 0..measurement_mesh.face_count() {
        for tri in measurement_mesh.fan_triangles(face) {
            if !tri
                .iter()
                .any(|&c| dirty.get(uv.corner_uv(c)).copied().unwrap_or(true))
            {
                continue;
            }
            let slot = tri[1];
            let sum = &mut sums[scene.islands.island_of_face(face)];
            if let Some(old) = maps[slot] {
                sum.0 -= old.area_3d;
                sum.1 -= old.uv_area.abs();
            }
            let updated = triangle_map(
                tri.map(|c| measurement_mesh.corner_position(c)),
                tri.map(|c| uv.uv_at_corner(c)),
            );
            if let Some(new) = updated {
                sum.0 += new.area_3d;
                sum.1 += new.uv_area.abs();
            }
            maps[slot] = updated;
        }
    }
    (std::borrow::Cow::Owned(maps), sums)
}

fn distortion_reference_scale(sums: &[(f64, f64)], config: &DistortionConfig) -> f64 {
    match config.reference {
        DistortionReference::Raw => 1.0,
        DistortionReference::TexelTarget => {
            let scale = config.texel_target / config.resolution.max(1) as f64;
            if scale.is_finite() && scale > 0.0 {
                scale
            } else {
                1.0
            }
        }
        domain => {
            let ids: Vec<usize> = match domain {
                DistortionReference::Object => (0..sums.len()).collect(),
                DistortionReference::Selected if !config.selected_islands.is_empty() => {
                    config.selected_islands.clone()
                }
                DistortionReference::Island => config
                    .active_island
                    .or_else(|| config.selected_islands.first().copied())
                    .map_or_else(|| (0..sums.len()).collect(), |i| vec![i]),
                _ => (0..sums.len()).collect(),
            };
            let (surface, texture) = ids
                .iter()
                .filter_map(|&i| sums.get(i))
                .fold((0.0, 0.0), |(a, b), &(x, y)| (a + x, b + y));
            if surface > 0.0 && texture > 0.0 {
                (texture / surface).sqrt()
            } else {
                1.0
            }
        }
    }
}

pub struct TriColorSettings<'a> {
    pub selected_faces: &'a [bool],
    pub mode: ColorMode,
    pub target_density: f64,
    pub group_colors: &'a [Option<[u8; 4]>],
    pub distortion: &'a DistortionConfig,
}

pub fn build_tris_with_distortion(
    mesh: &Mesh,
    measurement_mesh: &Mesh,
    uv: &UvLayer,
    scene: &Scene,
    settings: TriColorSettings<'_>,
) -> Vec<TriVertex> {
    let TriColorSettings {
        selected_faces,
        mode,
        target_density,
        group_colors,
        distortion,
    } = settings;
    let (maps, sums) = if mode == ColorMode::Distortion {
        distortion_maps_for_uv(measurement_mesh, uv, scene)
    } else {
        (
            std::borrow::Cow::Borrowed(scene.distortion_maps.as_slice()),
            Vec::new(),
        )
    };
    let reference_scale = if mode == ColorMode::Distortion {
        distortion_reference_scale(&sums, distortion)
    } else {
        1.0
    };
    let mut out = Vec::with_capacity(scene.tris.len() * 3);
    for &(f, t) in &scene.tris {
        let f = f as usize;
        let n = mesh
            .face_area_normal(f)
            .normalize_or_zero()
            .as_vec3()
            .to_array();
        let p = t.map(|c| mesh.corner_position(c as usize));
        let q = t.map(|c| uv.uv_at_corner(c as usize));
        let rgb = match mode {
            ColorMode::Checker | ColorMode::Texture | ColorMode::Islands => group_colors
                .get(f)
                .copied()
                .flatten()
                .map(|rgba| {
                    [
                        rgba[0] as f32 / 255.0,
                        rgba[1] as f32 / 255.0,
                        rgba[2] as f32 / 255.0,
                    ]
                })
                .unwrap_or_else(|| island_color(scene.islands.island_of_face(f))),
            ColorMode::Distortion => {
                let map = maps[t[1] as usize];
                let island = scene.islands.island_of_face(f);
                if map.is_some_and(|m| (m.uv_area < 0.0) != scene.report.islands[island].flipped) {
                    [0.85, 0.2, 0.75]
                } else {
                    distortion_color(
                        map.and_then(|m| distortion_values(m, reference_scale)),
                        distortion.metric,
                        distortion.sensitivity,
                    )
                }
            }
            ColorMode::Flipped => {
                let island = scene.islands.island_of_face(f);
                let mirrored = scene.report.islands[island].flipped;
                match triangle_map(p, q) {
                    None => [0.7, 0.25, 0.85], // degenerate UV or 3D triangle
                    Some(m) if (m.uv_area < 0.0) != mirrored => [1.0, 0.16, 0.22],
                    Some(_) if mirrored => [0.95, 0.58, 0.14],
                    Some(_) => [0.2, 0.7, 0.42],
                }
            }
            ColorMode::Overlaps => {
                let island = scene.islands.island_of_face(f);
                let r = &scene.report.islands[island];
                if r.self_overlapping && r.overlaps_other_island {
                    [0.9, 0.2, 0.75]
                } else if r.self_overlapping {
                    [0.95, 0.45, 0.15]
                } else if r.overlaps_other_island {
                    [0.95, 0.2, 0.15]
                } else if scene.intentional_stack_islands[island] {
                    [0.25, 0.55, 0.95]
                } else {
                    [0.2, 0.7, 0.42]
                }
            }
            ColorMode::TexelDensity => density_color(
                scene.report.islands[scene.islands.island_of_face(f)].texel_density,
                target_density,
            ),
        };
        let a = if mode == ColorMode::Distortion
            && distortion.isolate_active
            && distortion
                .active_island
                .is_some_and(|active| scene.islands.island_of_face(f) != active)
        {
            -1.0
        } else if selected_faces.get(f).copied().unwrap_or(false) {
            1.0
        } else {
            0.5
        };
        for k in 0..3 {
            out.push(TriVertex {
                pos: p[k].as_vec3().to_array(),
                uv: render_uv(q[k]),
                normal: n,
                color: [rgb[0], rgb[1], rgb[2], a],
            });
        }
    }
    out
}

/// UV-only triangle stream in exactly the same order as `build_tris_with_colors`.
pub fn build_tri_uvs(uv: &UvLayer, scene: &Scene) -> Vec<[f32; 2]> {
    let mut out = Vec::with_capacity(scene.tris.len() * 3);
    for &(_, corners) in &scene.tris {
        out.extend(corners.map(|corner| render_uv(uv.uv_at_corner(corner as usize))));
    }
    out
}

/// UV source index for each vertex in `build_tri_uvs` and `build_tris_with_colors`.
pub fn build_tri_uv_sources(uv: &UvLayer, scene: &Scene) -> Vec<u32> {
    let mut out = Vec::with_capacity(scene.tris.len() * 3);
    for &(_, corners) in &scene.tris {
        out.extend(corners.map(|corner| uv.corner_uv(corner as usize) as u32));
    }
    out
}

/// UV source indices in the same order as the two line streams.
pub fn build_line_uv_sources(
    mesh: &Mesh,
    uv: &UvLayer,
    selected_edges: &[bool],
    hidden_faces: &[bool],
    show_seams: bool,
    show_borders: bool,
) -> (Vec<u32>, Vec<u32>) {
    let kinds: Vec<EdgeKind> = (0..mesh.edge_count())
        .map(|edge| uv.edge_kind(mesh, edge))
        .collect();
    let mut wire = Vec::new();
    let mut edges = Vec::new();
    for corner in 0..mesh.corner_count() {
        if hidden_faces
            .get(mesh.corner_face(corner))
            .copied()
            .unwrap_or(false)
        {
            continue;
        }
        let edge = mesh.corner_edge(corner);
        let selected = selected_edges.get(edge).copied().unwrap_or(false);
        if !selected
            && ((kinds[edge] == EdgeKind::Seam && !show_seams)
                || (kinds[edge] == EdgeKind::Border && !show_borders))
        {
            continue;
        }
        let stream = if selected || kinds[edge] != EdgeKind::Interior {
            &mut edges
        } else {
            &mut wire
        };
        stream.push(uv.corner_uv(corner) as u32);
        stream.push(uv.corner_uv(mesh.next(corner)) as u32);
    }
    (wire, edges)
}

/// UV-only line stream matching `build_lines`, for interactive UV movement.
pub fn build_line_uvs(
    mesh: &Mesh,
    uv: &UvLayer,
    selected_edges: &[bool],
    hidden_faces: &[bool],
    show_seams: bool,
    show_borders: bool,
) -> (Vec<[f32; 2]>, Vec<[f32; 2]>) {
    let kinds: Vec<EdgeKind> = (0..mesh.edge_count())
        .map(|e| uv.edge_kind(mesh, e))
        .collect();
    let mut wire = Vec::new();
    let mut edges = Vec::new();
    for corner in 0..mesh.corner_count() {
        if hidden_faces
            .get(mesh.corner_face(corner))
            .copied()
            .unwrap_or(false)
        {
            continue;
        }
        let edge = mesh.corner_edge(corner);
        let selected = selected_edges.get(edge).copied().unwrap_or(false);
        if !selected
            && ((kinds[edge] == EdgeKind::Seam && !show_seams)
                || (kinds[edge] == EdgeKind::Border && !show_borders))
        {
            continue;
        }
        let target = if selected || kinds[edge] != EdgeKind::Interior {
            &mut edges
        } else {
            &mut wire
        };
        for c in [corner, mesh.next(corner)] {
            target.push(render_uv(uv.uv_at_corner(c)));
        }
    }
    (wire, edges)
}

/// (interior wireframe, highlighted edges: seams, borders, selection).
pub fn build_lines(
    mesh: &Mesh,
    uv: &UvLayer,
    selected_edges: &[bool],
    hidden_faces: &[bool],
    show_seams: bool,
    show_borders: bool,
) -> (Vec<LineVertex>, Vec<LineVertex>) {
    let kinds: Vec<EdgeKind> = (0..mesh.edge_count())
        .map(|e| uv.edge_kind(mesh, e))
        .collect();
    let mut wire = Vec::new();
    let mut edges = Vec::new();
    for c in 0..mesh.corner_count() {
        if hidden_faces
            .get(mesh.corner_face(c))
            .copied()
            .unwrap_or(false)
        {
            continue;
        }
        let e = mesh.corner_edge(c);
        let n = mesh.next(c);
        let selected = selected_edges.get(e).copied().unwrap_or(false);
        if !selected
            && ((kinds[e] == EdgeKind::Seam && !show_seams)
                || (kinds[e] == EdgeKind::Border && !show_borders))
        {
            continue;
        }
        let color = if selected {
            [1.0, 0.92, 0.1, 1.0]
        } else {
            match kinds[e] {
                EdgeKind::Interior => [0.9, 0.91, 0.94, 0.5],
                EdgeKind::Seam => [1.0, 0.42, 0.08, 1.0],
                EdgeKind::Border => [1.0, 0.55, 0.16, 1.0],
                EdgeKind::NonManifold => [1.0, 0.1, 0.9, 1.0],
            }
        };
        let target = if selected || kinds[e] != EdgeKind::Interior {
            &mut edges
        } else {
            &mut wire
        };
        for k in [c, n] {
            target.push(LineVertex {
                pos: mesh.corner_position(k).as_vec3().to_array(),
                uv: render_uv(uv.uv_at_corner(k)),
                color,
            });
        }
    }
    (wire, edges)
}

/// UV grid: tile borders around 0–1 and a faint 1/10 grid inside it.
pub fn build_grid(divisions: u32) -> Vec<LineVertex> {
    let mut out = Vec::new();
    let mut line = |a: [f32; 2], b: [f32; 2], c: [f32; 4]| {
        for uv in [a, b] {
            out.push(LineVertex {
                pos: [0.0; 3],
                uv,
                color: c,
            });
        }
    };
    for i in 1..divisions {
        let t = i as f32 / divisions as f32;
        let c = [1.0, 1.0, 1.0, 0.07];
        line([t, 0.0], [t, 1.0], c);
        line([0.0, t], [1.0, t], c);
    }
    for i in -2..=3 {
        let t = i as f32;
        let c = if (0..=1).contains(&i) {
            [1.0, 1.0, 1.0, 0.55]
        } else {
            [1.0, 1.0, 1.0, 0.15]
        };
        line([t, -2.0], [t, 3.0], c);
        line([-2.0, t], [3.0, t], c);
    }
    out
}

/// Nearest face hit by a ray (Möller–Trumbore); returns (face, distance).
pub fn pick_face_3d(mesh: &Mesh, scene: &Scene, origin: DVec3, dir: DVec3) -> Option<(usize, f64)> {
    let mut best: Option<(usize, f64)> = None;
    for &(f, t) in &scene.tris {
        let [a, b, c] = t.map(|c| mesh.corner_position(c as usize));
        let (e1, e2) = (b - a, c - a);
        let p = dir.cross(e2);
        let det = e1.dot(p);
        if det.abs() < 1e-18 {
            continue;
        }
        let inv = 1.0 / det;
        let s = origin - a;
        let u = s.dot(p) * inv;
        if !(0.0..=1.0).contains(&u) {
            continue;
        }
        let q = s.cross(e1);
        let v = dir.dot(q) * inv;
        if v < 0.0 || u + v > 1.0 {
            continue;
        }
        let dist = e2.dot(q) * inv;
        if dist > 0.0 && best.is_none_or(|(_, d)| dist < d) {
            best = Some((f as usize, dist));
        }
    }
    best
}

/// Topmost face (last drawn) containing a UV point.
pub fn pick_face_uv(uv: &UvLayer, scene: &Scene, p: DVec2) -> Option<usize> {
    scene.tris.iter().rev().find_map(|&(f, t)| {
        let [a, b, c] = t.map(|c| uv.uv_at_corner(c as usize));
        let d = (b - a).perp_dot(c - a);
        if d == 0.0 {
            return None;
        }
        let w1 = (p - a).perp_dot(c - a) / d;
        let w2 = (b - a).perp_dot(p - a) / d;
        (w1 >= 0.0 && w2 >= 0.0 && w1 + w2 <= 1.0).then_some(f as usize)
    })
}

/// Nearest face per pixel of a `w × h` view (`u32::MAX` = background),
/// rasterized on the CPU with a depth test. Pixel (x, y) covers view-local
/// points [x, x+1) × [y, y+1); depth is NDC z (smaller = nearer).
pub struct IdBuffer {
    pub w: usize,
    pub h: usize,
    pub face: Vec<u32>,
}

impl IdBuffer {
    pub fn new(mesh: &Mesh, scene: &Scene, view_proj: glam::Mat4, w: usize, h: usize) -> Self {
        let mut face = vec![u32::MAX; w * h];
        let mut depth = vec![f32::INFINITY; w * h];
        let (wf, hf) = (w as f32, h as f32);
        for &(f, t) in &scene.tris {
            let mut p = [glam::Vec3::ZERO; 3];
            let mut behind = false;
            for k in 0..3 {
                let clip = view_proj * mesh.corner_position(t[k] as usize).as_vec3().extend(1.0);
                if clip.w <= 1e-6 {
                    behind = true;
                    break;
                }
                let ndc = clip.truncate() / clip.w;
                p[k] = glam::Vec3::new((ndc.x + 1.0) / 2.0 * wf, (1.0 - ndc.y) / 2.0 * hf, ndc.z);
            }
            if behind {
                continue;
            }
            let area =
                (p[1].x - p[0].x) * (p[2].y - p[0].y) - (p[1].y - p[0].y) * (p[2].x - p[0].x);
            if area.abs() < 1e-12 {
                continue;
            }
            let x0 = p
                .iter()
                .map(|q| q.x)
                .fold(f32::INFINITY, f32::min)
                .floor()
                .max(0.0) as usize;
            let x1 = p
                .iter()
                .map(|q| q.x)
                .fold(f32::NEG_INFINITY, f32::max)
                .ceil()
                .min(wf) as usize;
            let y0 = p
                .iter()
                .map(|q| q.y)
                .fold(f32::INFINITY, f32::min)
                .floor()
                .max(0.0) as usize;
            let y1 = p
                .iter()
                .map(|q| q.y)
                .fold(f32::NEG_INFINITY, f32::max)
                .ceil()
                .min(hf) as usize;
            for y in y0..y1 {
                for x in x0..x1 {
                    // Sample at the pixel center; barycentrics from edge functions.
                    let (sx, sy) = (x as f32 + 0.5, y as f32 + 0.5);
                    let e = |a: glam::Vec3, b: glam::Vec3| {
                        (b.x - a.x) * (sy - a.y) - (b.y - a.y) * (sx - a.x)
                    };
                    let (b0, b1, b2) = (
                        e(p[1], p[2]) / area,
                        e(p[2], p[0]) / area,
                        e(p[0], p[1]) / area,
                    );
                    if b0 < 0.0 || b1 < 0.0 || b2 < 0.0 {
                        continue;
                    }
                    let z = b0 * p[0].z + b1 * p[1].z + b2 * p[2].z;
                    let i = y * w + x;
                    if (0.0..depth[i]).contains(&z) {
                        depth[i] = z;
                        face[i] = f;
                    }
                }
            }
        }
        IdBuffer { w, h, face }
    }

    /// Face at a view-local point, if any.
    pub fn at(&self, p: glam::Vec2) -> Option<usize> {
        if p.x < 0.0 || p.y < 0.0 {
            return None;
        }
        let (x, y) = (p.x as usize, p.y as usize);
        (x < self.w && y < self.h)
            .then(|| self.face[y * self.w + x])
            .filter(|&f| f != u32::MAX)
            .map(|f| f as usize)
    }
}

/// Even-odd point-in-polygon test (the polygon is implicitly closed).
pub fn point_in_polygon(p: glam::Vec2, poly: &[glam::Vec2]) -> bool {
    let mut inside = false;
    let mut j = poly.len().wrapping_sub(1);
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[j]);
        if (a.y > p.y) != (b.y > p.y) && p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Distance from `p` to segment `a`–`b`.
pub fn segment_distance(p: DVec2, a: DVec2, b: DVec2) -> f64 {
    let ab = b - a;
    let t = if ab.length_squared() > 0.0 {
        ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0)
    } else {
        0.0
    };
    p.distance(a + ab * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distortion_mode_distinguishes_uniform_expansion_and_compression() {
        let positions = vec![
            DVec3::ZERO,
            DVec3::X,
            DVec3::Y,
            DVec3::new(3.0, 0.0, 0.0),
            DVec3::new(4.0, 0.0, 0.0),
            DVec3::new(3.0, 1.0, 0.0),
        ];
        let faces = [[0u32, 1, 2], [3, 4, 5]];
        let mesh = Mesh::new(positions, &faces).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..6).collect(),
            uvs: vec![
                DVec2::ZERO,
                DVec2::X * 2.0,
                DVec2::Y * 2.0,
                DVec2::new(3.0, 0.0),
                DVec2::new(3.5, 0.0),
                DVec2::new(3.0, 0.5),
            ],
        };
        let scene = Scene::new(&mesh, &uv, 1024);
        let colors = build_tris(&mesh, &uv, &scene, &[], ColorMode::Distortion, 512.0);
        assert_ne!(
            colors[0].color[..3],
            colors[3].color[..3],
            "shared-reference heatmap must distinguish UV expansion from compression"
        );
    }

    #[test]
    fn distortion_modes_detect_area_preserving_anisotropy() {
        let positions = vec![
            DVec3::ZERO,
            DVec3::X,
            DVec3::Y,
            DVec3::new(3.0, 0.0, 0.0),
            DVec3::new(4.0, 0.0, 0.0),
            DVec3::new(3.0, 1.0, 0.0),
        ];
        let faces = [[0u32, 1, 2], [3, 4, 5]];
        let mesh = Mesh::new(positions, &faces).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..6).collect(),
            uvs: vec![
                DVec2::ZERO,
                DVec2::X * 2.0,
                DVec2::Y * 0.5,
                DVec2::new(3.0, 0.0),
                DVec2::new(4.0, 0.0),
                DVec2::new(3.0, 1.0),
            ],
        };
        let scene = Scene::new(&mesh, &uv, 1024);
        let mut config = DistortionConfig::default();
        let colors = |config: &DistortionConfig| {
            build_tris_with_distortion(
                &mesh,
                &mesh,
                &uv,
                &scene,
                TriColorSettings {
                    selected_faces: &[],
                    mode: ColorMode::Distortion,
                    target_density: 512.0,
                    group_colors: &[],
                    distortion: config,
                },
            )
        };
        let area = colors(&config);
        assert_eq!(area[0].color[..3], area[3].color[..3]);
        for metric in [
            DistortionMetric::Conformal,
            DistortionMetric::Directional,
            DistortionMetric::Combined,
        ] {
            config.metric = metric;
            let result = colors(&config);
            assert_ne!(result[0].color[..3], result[3].color[..3], "{metric:?}");
        }
    }

    #[test]
    fn individual_reference_normalizes_uniform_scale_but_object_reference_compares_islands() {
        let positions = vec![
            DVec3::ZERO,
            DVec3::X,
            DVec3::Y,
            DVec3::new(3.0, 0.0, 0.0),
            DVec3::new(4.0, 0.0, 0.0),
            DVec3::new(3.0, 1.0, 0.0),
        ];
        let faces = [[0u32, 1, 2], [3, 4, 5]];
        let mesh = Mesh::new(positions, &faces).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..6).collect(),
            uvs: vec![
                DVec2::ZERO,
                DVec2::X * 2.0,
                DVec2::Y * 2.0,
                DVec2::new(3.0, 0.0),
                DVec2::new(3.5, 0.0),
                DVec2::new(3.0, 0.5),
            ],
        };
        let scene = Scene::new(&mesh, &uv, 1024);
        let mut config = DistortionConfig::default();
        let colors = |config: &DistortionConfig| {
            build_tris_with_distortion(
                &mesh,
                &mesh,
                &uv,
                &scene,
                TriColorSettings {
                    selected_faces: &[],
                    mode: ColorMode::Distortion,
                    target_density: 512.0,
                    group_colors: &[],
                    distortion: config,
                },
            )
        };
        assert_ne!(colors(&config)[0].color[..3], colors(&config)[3].color[..3]);
        config.reference = DistortionReference::Island;
        config.active_island = Some(0);
        assert_eq!(colors(&config)[0].color[..3], [0.2, 0.8, 0.35]);
        config.reference = DistortionReference::Selected;
        config.selected_islands = vec![0];
        assert_eq!(colors(&config)[0].color[..3], [0.2, 0.8, 0.35]);
        assert_ne!(colors(&config)[3].color[..3], [0.2, 0.8, 0.35]);
        config.selected_islands = vec![0, 1];
        assert_eq!(
            colors(&config)[0].color[..3],
            build_tris(&mesh, &uv, &scene, &[], ColorMode::Distortion, 512.0)[0].color[..3]
        );
        config.reference = DistortionReference::TexelTarget;
        config.texel_target = 2048.0;
        config.resolution = 1024;
        assert_eq!(colors(&config)[0].color[..3], [0.2, 0.8, 0.35]);
        config.reference = DistortionReference::Raw;
        assert_ne!(colors(&config)[0].color[..3], [0.2, 0.8, 0.35]);
    }

    #[test]
    fn interactive_uv_change_recomputes_only_affected_jacobian() {
        let mesh = Mesh::new(vec![DVec3::ZERO, DVec3::X, DVec3::Y], &[[0u32, 1, 2]]).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2],
            uvs: vec![DVec2::ZERO, DVec2::X, DVec2::Y],
        };
        let scene = Scene::new(&mesh, &uv, 1024);
        let config = DistortionConfig {
            reference: DistortionReference::Raw,
            ..Default::default()
        };
        let draw = |layer: &UvLayer| {
            build_tris_with_distortion(
                &mesh,
                &mesh,
                layer,
                &scene,
                TriColorSettings {
                    selected_faces: &[],
                    mode: ColorMode::Distortion,
                    target_density: 512.0,
                    group_colors: &[],
                    distortion: &config,
                },
            )
        };
        assert_eq!(draw(&uv)[0].color[..3], [0.2, 0.8, 0.35]);
        let mut preview = uv.clone();
        preview.uvs[1] *= 2.0;
        assert_ne!(draw(&preview)[0].color[..3], draw(&uv)[0].color[..3]);
        assert_eq!(scene.distortion_maps[1].unwrap().sigma_max, 1.0);
        assert_eq!(draw(&uv)[0].color[..3], [0.2, 0.8, 0.35]);
    }

    #[test]
    fn measurement_mesh_drives_the_same_triangle_colors_in_both_views() {
        let faces = [[0u32, 1, 2]];
        let mesh = Mesh::new(vec![DVec3::ZERO, DVec3::X, DVec3::Y], &faces).unwrap();
        let measured =
            Mesh::new(vec![DVec3::ZERO, DVec3::X * 2.0, DVec3::Y * 2.0], &faces).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2],
            uvs: vec![DVec2::ZERO, DVec2::X, DVec2::Y],
        };
        let scene = Scene::new_with_measurements(&mesh, &measured, &uv, 1024);
        let config = DistortionConfig {
            reference: DistortionReference::Raw,
            ..Default::default()
        };
        let colors = build_tris_with_distortion(
            &mesh,
            &measured,
            &uv,
            &scene,
            TriColorSettings {
                selected_faces: &[],
                mode: ColorMode::Distortion,
                target_density: 512.0,
                group_colors: &[],
                distortion: &config,
            },
        );
        assert_eq!(colors.len(), 3);
        for (actual, expected) in colors[0].color[..3].iter().zip([0.18, 0.42, 0.95]) {
            assert!((actual - expected).abs() < 1e-6);
        }
        assert!(colors.iter().all(|vertex| vertex.color == colors[0].color));
    }

    #[test]
    fn intentional_overlap_keeps_each_face_measurement_and_invalids_are_visible() {
        let faces = [[0u32, 1, 2], [3, 4, 5]];
        let mesh = Mesh::new(
            vec![
                DVec3::ZERO,
                DVec3::X,
                DVec3::Y,
                DVec3::new(0.0, 0.0, 2.0),
                DVec3::new(2.0, 0.0, 2.0),
                DVec3::new(0.0, 2.0, 2.0),
            ],
            &faces,
        )
        .unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..6).collect(),
            uvs: vec![
                DVec2::ZERO,
                DVec2::X,
                DVec2::Y,
                DVec2::ZERO,
                DVec2::X,
                DVec2::Y,
            ],
        };
        let config = DistortionConfig {
            reference: DistortionReference::Raw,
            ..Default::default()
        };
        let draw = |uv: &UvLayer| {
            let scene =
                Scene::new_with_measurements_and_stacks(&mesh, &mesh, uv, 1024, &[vec![0, 1]]);
            build_tris_with_distortion(
                &mesh,
                &mesh,
                uv,
                &scene,
                TriColorSettings {
                    selected_faces: &[],
                    mode: ColorMode::Distortion,
                    target_density: 512.0,
                    group_colors: &[],
                    distortion: &config,
                },
            )
        };
        let colors = draw(&uv);
        assert_ne!(colors[0].color[..3], colors[3].color[..3]);
        uv.uvs[4] = DVec2::ZERO;
        assert_eq!(draw(&uv)[3].color[..3], [0.7, 0.25, 0.85]);
        uv.uvs[4] = -DVec2::X;
        let mirrored =
            Scene::new_with_measurements_and_stacks(&mesh, &mesh, &uv, 1024, &[vec![0, 1]]);
        assert_eq!(mirrored.report.flipped_islands, 1);
        assert_ne!(draw(&uv)[3].color[..3], [0.7, 0.25, 0.85]);
        uv.uvs[4] = DVec2::new(f64::NAN, 0.0);
        let invalid = draw(&uv);
        assert_eq!(invalid[3].color[..3], [0.7, 0.25, 0.85]);
        assert!(
            invalid.iter().all(
                |v| v.uv.iter().all(|x| x.is_finite()) && v.color.iter().all(|x| x.is_finite())
            )
        );
    }

    #[test]
    fn heatmap_refreshes_after_relax_and_straighten() {
        let positions: Vec<DVec3> = (0..3)
            .flat_map(|y| (0..3).map(move |x| DVec3::new(x as f64, y as f64, 0.0)))
            .collect();
        let faces = [[0u32, 1, 4, 3], [1, 2, 5, 4], [3, 4, 7, 6], [4, 5, 8, 7]];
        let mesh = Mesh::new(positions.clone(), &faces).unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: faces.iter().flatten().copied().collect(),
            uvs: positions.iter().map(|p| p.truncate()).collect(),
        };
        uv.uvs[4] += DVec2::new(0.35, -0.2);
        let islands = Islands::compute(&mesh, &uv);
        let config = DistortionConfig {
            metric: DistortionMetric::Combined,
            reference: DistortionReference::Raw,
            ..Default::default()
        };
        let draw = |uv: &UvLayer| {
            let scene = Scene::new(&mesh, uv, 1024);
            build_tris_with_distortion(
                &mesh,
                &mesh,
                uv,
                &scene,
                TriColorSettings {
                    selected_faces: &[],
                    mode: ColorMode::Distortion,
                    target_density: 512.0,
                    group_colors: &[],
                    distortion: &config,
                },
            )
            .iter()
            .map(|v| v.color)
            .collect::<Vec<_>>()
        };
        let before = draw(&uv);
        let mut pins = vec![true; 9];
        pins[4] = false;
        let result = uvcraft_algo::slim::optimize_slim(&mesh, &mut uv, &islands, &[0], &pins, 20);
        assert_eq!(result.optimized, 1);
        assert_ne!(draw(&uv), before, "Relax must refresh distortion colors");

        let angles = [
            0.0_f64,
            std::f64::consts::FRAC_PI_6,
            std::f64::consts::FRAC_PI_2,
        ];
        let positions: Vec<DVec3> = angles
            .iter()
            .flat_map(|&angle| {
                [1.0, 2.0]
                    .map(move |radius| DVec3::new(radius * angle.cos(), radius * angle.sin(), 0.0))
            })
            .collect();
        let mesh = Mesh::new(positions.clone(), &[[0u32, 2, 3, 1], [2, 4, 5, 3]]).unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..mesh.corner_count())
                .map(|c| mesh.corner_vertex(c) as u32)
                .collect(),
            uvs: positions.iter().map(|p| p.truncate() * 0.25).collect(),
        };
        let islands = Islands::compute(&mesh, &uv);
        let straight_config = DistortionConfig {
            metric: DistortionMetric::Conformal,
            sensitivity: 0.5,
            ..Default::default()
        };
        let before_scene = Scene::new(&mesh, &uv, 1024);
        let before = build_tris_with_distortion(
            &mesh,
            &mesh,
            &uv,
            &before_scene,
            TriColorSettings {
                selected_faces: &[],
                mode: ColorMode::Distortion,
                target_density: 512.0,
                group_colors: &[],
                distortion: &straight_config,
            },
        );
        uvcraft_algo::grid::straighten_island(&mesh, &mut uv, &islands, 0, &[]).unwrap();
        let after_scene = Scene::new(&mesh, &uv, 1024);
        let after = build_tris_with_distortion(
            &mesh,
            &mesh,
            &uv,
            &after_scene,
            TriColorSettings {
                selected_faces: &[],
                mode: ColorMode::Distortion,
                target_density: 512.0,
                group_colors: &[],
                distortion: &straight_config,
            },
        );
        assert_ne!(
            before.iter().map(|v| v.color).collect::<Vec<_>>(),
            after.iter().map(|v| v.color).collect::<Vec<_>>(),
            "Straighten must refresh distortion colors"
        );
    }

    #[test]
    fn picking() {
        let p = vec![DVec3::ZERO, DVec3::X, DVec3::new(1.0, 1.0, 0.0), DVec3::Y];
        let mesh = Mesh::new(p, &[[0, 1, 2, 3]]).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 3],
            uvs: vec![DVec2::ZERO, DVec2::X, DVec2::ONE, DVec2::Y],
        };
        let scene = Scene::new(&mesh, &uv, 1024);
        let grouped = build_tris_with_colors(
            &mesh,
            &uv,
            &scene,
            &[],
            ColorMode::Islands,
            512.0,
            &[Some([255, 0, 0, 255])],
        );
        assert_eq!(
            build_tri_uvs(&uv, &scene),
            grouped.iter().map(|v| v.uv).collect::<Vec<_>>()
        );
        assert_eq!(
            build_tri_uv_sources(&uv, &scene)
                .iter()
                .map(|&index| uv.uvs[index as usize].as_vec2().to_array())
                .collect::<Vec<_>>(),
            build_tri_uvs(&uv, &scene),
        );
        assert_eq!(&grouped[0].color[..3], &[1.0, 0.0, 0.0]);
        let hit = pick_face_3d(&mesh, &scene, DVec3::new(0.7, 0.2, 5.0), DVec3::NEG_Z);
        assert_eq!(hit.map(|h| h.0), Some(0));
        assert!((hit.unwrap().1 - 5.0).abs() < 1e-12);
        assert_eq!(
            pick_face_3d(&mesh, &scene, DVec3::new(2.0, 0.2, 5.0), DVec3::NEG_Z),
            None
        );
        assert_eq!(pick_face_uv(&uv, &scene, DVec2::new(0.3, 0.9)), Some(0));
        assert_eq!(pick_face_uv(&uv, &scene, DVec2::new(1.3, 0.9)), None);
        assert!(
            (segment_distance(DVec2::new(0.5, 1.0), DVec2::ZERO, DVec2::X) - 1.0).abs() < 1e-12
        );
        assert_eq!(
            build_tris(&mesh, &uv, &scene, &[true], ColorMode::Islands, 512.0).len(),
            6
        );
        // Looking down -Z at the quad: every covered pixel sees face 0.
        let vp = glam::camera::rh::proj::directx::orthographic(-0.5, 1.5, -0.5, 1.5, -10.0, 10.0);
        let ids = IdBuffer::new(&mesh, &scene, vp, 20, 20);
        assert_eq!(ids.at(glam::Vec2::new(10.0, 10.0)), Some(0));
        assert_eq!(ids.at(glam::Vec2::new(1.0, 1.0)), None);
        let (wire, edges) = build_lines(&mesh, &uv, &[], &[], true, true);
        assert_eq!((wire.len(), edges.len()), (0, 8), "all 4 edges are borders");
        let (wire_uv, edge_uv) = build_line_uvs(&mesh, &uv, &[], &[], true, true);
        assert_eq!(wire_uv, wire.iter().map(|v| v.uv).collect::<Vec<_>>());
        assert_eq!(edge_uv, edges.iter().map(|v| v.uv).collect::<Vec<_>>());
        let (wire_sources, edge_sources) = build_line_uv_sources(&mesh, &uv, &[], &[], true, true);
        assert_eq!(wire_sources.len(), wire_uv.len());
        assert_eq!(
            edge_sources
                .iter()
                .map(|&index| uv.uvs[index as usize].as_vec2().to_array())
                .collect::<Vec<_>>(),
            edge_uv,
        );
        let (wire, edges) = build_lines(&mesh, &uv, &[], &[true], true, true);
        assert_eq!(wire.len() + edges.len(), 0, "hidden face: no lines");
        let (wire, edges) =
            build_lines(&mesh, &uv, &[true, false, false, false], &[], false, false);
        assert_eq!(wire.len(), 0);
        assert_eq!(edges.len(), 2, "selected border remains visible");
        let mut hidden = Scene::new(&mesh, &uv, 1024);
        hidden.set_hidden(&mesh, &[true]);
        assert!(hidden.tris.is_empty());
        assert_eq!(pick_face_uv(&uv, &hidden, DVec2::new(0.3, 0.9)), None);
    }

    #[test]
    fn flipped_mode_distinguishes_mirrors_from_local_folds() {
        let pos: Vec<DVec3> = (0..6)
            .map(|i| DVec3::new((i / 2) as f64, (i % 2) as f64, 0.0))
            .collect();
        let faces = [[0u32, 2, 3, 1], [2, 4, 5, 3]];
        let mesh = Mesh::new(pos.clone(), &faces).unwrap();
        let mut uvs: Vec<DVec2> = pos.iter().map(|p| p.truncate() * 0.1).collect();
        uvs[4].x = 0.05;
        uvs[5].x = 0.05;
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: faces.iter().flatten().copied().collect(),
            uvs,
        };
        let scene = Scene::new(&mesh, &uv, 1024);
        let tris = build_tris(&mesh, &uv, &scene, &[], ColorMode::Flipped, 512.0);
        assert_eq!(tris[0].color[..3], [0.2, 0.7, 0.42]);
        assert_eq!(tris[6].color[..3], [1.0, 0.16, 0.22]);
        let distortion = build_tris(&mesh, &uv, &scene, &[], ColorMode::Distortion, 512.0);
        assert_eq!(distortion[6].color[..3], [0.85, 0.2, 0.75]);

        let mut mirrored = uv;
        for p in &mut mirrored.uvs {
            p.x = -p.x;
        }
        let scene = Scene::new(&mesh, &mirrored, 1024);
        let tris = build_tris(&mesh, &mirrored, &scene, &[], ColorMode::Flipped, 512.0);
        assert_eq!(tris[0].color[..3], [0.95, 0.58, 0.14]);
        assert_eq!(tris[6].color[..3], [1.0, 0.16, 0.22]);
    }

    #[test]
    fn density_heatmap_tracks_target_and_invalid_values() {
        let near =
            |a: [f32; 3], b: [f32; 3]| a.into_iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6);
        assert!(near(density_color(512.0, 512.0), [0.2, 0.8, 0.35]));
        assert!(near(density_color(256.0, 512.0), [0.2, 0.45, 0.95]));
        assert!(near(density_color(1024.0, 512.0), [0.95, 0.2, 0.15]));
        assert_eq!(density_color(0.0, 512.0), [0.7, 0.25, 0.85]);
    }

    #[test]
    fn grid_divisions_change_line_count() {
        let low = build_grid(2);
        let high = build_grid(10);
        // Two line segments per internal division, plus fixed tile borders.
        assert_eq!(high.len() - low.len(), (10 - 2) * 4);
        assert!(low.iter().all(|v| v.uv.iter().all(|x| x.is_finite())));
    }
}
