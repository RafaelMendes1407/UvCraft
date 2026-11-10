//! CPU-side data derived from the document: islands, report, render
//! vertices and picking.

use glam::{DVec2, DVec3, Vec3};
use uvcraft_algo::analysis::{self, Report, triangle_map};
use uvcraft_core::{EdgeKind, Islands, Mesh, UvLayer};

use crate::render::{LineVertex, TriVertex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Checker,
    Islands,
    Distortion,
}

pub struct Scene {
    pub islands: Islands,
    pub report: Report,
    /// Fan triangles: (face, corners).
    pub tris: Vec<(u32, [u32; 3])>,
    pub bbox: (Vec3, Vec3),
}

impl Scene {
    pub fn new(mesh: &Mesh, uv: &UvLayer, resolution: u32) -> Self {
        let islands = Islands::compute(mesh, uv);
        let report = analysis::analyze(mesh, uv, &islands, resolution);
        let tris = (0..mesh.face_count())
            .flat_map(|f| {
                mesh.fan_triangles(f)
                    .map(move |t| (f as u32, t.map(|c| c as u32)))
            })
            .collect();
        let bbox = mesh.positions().iter().fold(
            (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY)),
            |(lo, hi), p| (lo.min(p.as_vec3()), hi.max(p.as_vec3())),
        );
        Scene {
            islands,
            report,
            tris,
            bbox,
        }
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

fn distortion_color(d: f64) -> [f32; 3] {
    // 1 (conformal) = blue, 1.5 = white, ≥ 2 = red.
    let t = ((d - 1.0) / 1.0).clamp(0.0, 1.0) as f32;
    let (blue, white, red) = ([0.2, 0.45, 0.95], [0.95, 0.95, 0.95], [0.95, 0.2, 0.15]);
    let lerp = |a: [f32; 3], b: [f32; 3], t: f32| [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t);
    if t < 0.5 {
        lerp(blue, white, t * 2.0)
    } else {
        lerp(white, red, t * 2.0 - 1.0)
    }
}

pub fn build_tris(
    mesh: &Mesh,
    uv: &UvLayer,
    scene: &Scene,
    selected_faces: &[bool],
    mode: ColorMode,
) -> Vec<TriVertex> {
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
            ColorMode::Checker | ColorMode::Islands => {
                island_color(scene.islands.island_of_face(f))
            }
            ColorMode::Distortion => triangle_map(p, q).map_or([0.6, 0.0, 0.6], |m| {
                distortion_color(m.sigma_max / m.sigma_min)
            }),
        };
        let a = if selected_faces.get(f).copied().unwrap_or(false) {
            1.0
        } else {
            0.5
        };
        for k in 0..3 {
            out.push(TriVertex {
                pos: p[k].as_vec3().to_array(),
                uv: q[k].as_vec2().to_array(),
                normal: n,
                color: [rgb[0], rgb[1], rgb[2], a],
            });
        }
    }
    out
}

/// (interior wireframe, highlighted edges: seams, borders, selection).
pub fn build_lines(
    mesh: &Mesh,
    uv: &UvLayer,
    selected_edges: &[bool],
) -> (Vec<LineVertex>, Vec<LineVertex>) {
    let kinds: Vec<EdgeKind> = (0..mesh.edge_count())
        .map(|e| uv.edge_kind(mesh, e))
        .collect();
    let mut wire = Vec::new();
    let mut edges = Vec::new();
    for c in 0..mesh.corner_count() {
        let e = mesh.corner_edge(c);
        let n = mesh.next(c);
        let selected = selected_edges.get(e).copied().unwrap_or(false);
        let color = if selected {
            [1.0, 0.92, 0.1, 1.0]
        } else {
            match kinds[e] {
                EdgeKind::Interior => [0.0, 0.0, 0.0, 0.35],
                EdgeKind::Seam => [1.0, 0.42, 0.08, 1.0],
                EdgeKind::Border => [0.25, 0.8, 1.0, 1.0],
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
                uv: uv.uv_at_corner(k).as_vec2().to_array(),
                color,
            });
        }
    }
    (wire, edges)
}

/// UV grid: tile borders around 0–1 and a faint 1/10 grid inside it.
pub fn build_grid() -> Vec<LineVertex> {
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
    for i in 1..10 {
        let t = i as f32 / 10.0;
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
    fn picking() {
        let p = vec![DVec3::ZERO, DVec3::X, DVec3::new(1.0, 1.0, 0.0), DVec3::Y];
        let mesh = Mesh::new(p, &[[0, 1, 2, 3]]).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 3],
            uvs: vec![DVec2::ZERO, DVec2::X, DVec2::ONE, DVec2::Y],
        };
        let scene = Scene::new(&mesh, &uv, 1024);
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
            build_tris(&mesh, &uv, &scene, &[true], ColorMode::Islands).len(),
            6
        );
        let (wire, edges) = build_lines(&mesh, &uv, &[]);
        assert_eq!((wire.len(), edges.len()), (0, 8), "all 4 edges are borders");
    }
}
