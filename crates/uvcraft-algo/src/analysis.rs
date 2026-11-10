//! UV quality analysis: distortion, flips, texel density and statistics.
//!
//! Distortion comes from the singular values σ1 ≥ σ2 of each triangle's
//! 3D→UV Jacobian, after dividing out the island's global scale:
//! - angle distortion = σ1 / σ2 (1 = conformal)
//! - area distortion  = max(σ1·σ2, 1 / (σ1·σ2)) (1 = area preserving)
//!
//! Means are weighted by 3D area. N-gons are measured by fan triangulation.

use glam::{DVec2, DVec3};
use serde::Serialize;
use uvcraft_core::{EdgeKind, Islands, Mesh, UvLayer};

/// Area below this fraction of the squared longest edge counts as degenerate.
const DEGENERATE_EPS: f64 = 1e-12;

/// The 3D→UV map of one triangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TriangleMap {
    pub area_3d: f64,
    /// Signed UV area: negative means the triangle is mirrored in UV.
    pub uv_area: f64,
    pub sigma_max: f64,
    pub sigma_min: f64,
}

/// Returns `None` when the 3D or the UV triangle is degenerate.
pub fn triangle_map(p: [DVec3; 3], u: [DVec2; 3]) -> Option<TriangleMap> {
    let (e1, e2) = (p[1] - p[0], p[2] - p[0]);
    let n = e1.cross(e2);
    let area_3d = 0.5 * n.length();
    let longest_3d = e1
        .length_squared()
        .max(e2.length_squared())
        .max((p[2] - p[1]).length_squared());
    if area_3d <= DEGENERATE_EPS * longest_3d || longest_3d == 0.0 {
        return None;
    }
    let (d1, d2) = (u[1] - u[0], u[2] - u[0]);
    let uv_area = 0.5 * d1.perp_dot(d2);
    let longest_uv = d1
        .length_squared()
        .max(d2.length_squared())
        .max((u[2] - u[1]).length_squared());
    if uv_area.abs() <= DEGENERATE_EPS * longest_uv || longest_uv == 0.0 {
        return None;
    }

    // Triangle in its own 2D frame: q1 = (|e1|, 0), q2 = (q2x, q2y).
    let x = e1 / e1.length();
    let y = n.cross(e1).normalize();
    let (q1x, q2x, q2y) = (e1.length(), e2.dot(x), e2.dot(y));
    // J = [d1 d2] · [q1 q2]⁻¹
    let det = q1x * q2y;
    let j00 = d1.x * q2y / det;
    let j01 = (d2.x * q1x - d1.x * q2x) / det;
    let j10 = d1.y * q2y / det;
    let j11 = (d2.y * q1x - d1.y * q2x) / det;
    // Closed-form singular values of a 2×2 matrix.
    let e = (j00 + j11) / 2.0;
    let f = (j00 - j11) / 2.0;
    let g = (j10 + j01) / 2.0;
    let h = (j10 - j01) / 2.0;
    let q = e.hypot(h);
    let r = f.hypot(g);
    Some(TriangleMap {
        area_3d,
        uv_area,
        sigma_max: q + r,
        sigma_min: (q - r).abs(),
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IslandReport {
    pub index: usize,
    pub face_count: usize,
    pub area_3d: f64,
    /// Unsigned UV area, in units of one 0–1 tile.
    pub uv_area: f64,
    /// Pixels per 3D unit at the report resolution.
    pub texel_density: f64,
    pub bbox_min: [f64; 2],
    pub bbox_max: [f64; 2],
    /// Total signed UV area is negative: the whole island is mirrored.
    pub flipped: bool,
    /// Triangles whose orientation is opposite to the island's.
    pub flipped_triangles: usize,
    /// Triangles skipped because they are degenerate in 3D or in UV.
    pub degenerate_triangles: usize,
    pub outside_unit_tile: bool,
    /// The bounding box crosses a UDIM tile border.
    pub crosses_tile: bool,
    pub mean_angle_distortion: f64,
    pub max_angle_distortion: f64,
    pub mean_area_distortion: f64,
    pub max_area_distortion: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub resolution: u32,
    pub face_count: usize,
    pub island_count: usize,
    pub area_3d: f64,
    /// Sum of island UV areas; overlaps count twice, so it can exceed 1.
    pub uv_area: f64,
    pub flipped_islands: usize,
    pub flipped_triangles: usize,
    pub degenerate_triangles: usize,
    pub islands_outside_unit_tile: usize,
    pub islands_crossing_tile: usize,
    pub seam_edges: usize,
    pub seam_length: f64,
    pub border_edges: usize,
    pub non_manifold_edges: usize,
    pub mean_angle_distortion: f64,
    pub max_angle_distortion: f64,
    pub mean_area_distortion: f64,
    pub max_area_distortion: f64,
    pub islands: Vec<IslandReport>,
}

/// Texel density in pixels per 3D unit.
pub fn texel_density(uv_area: f64, area_3d: f64, resolution: u32) -> f64 {
    if area_3d > 0.0 {
        resolution as f64 * (uv_area.abs() / area_3d).sqrt()
    } else {
        0.0
    }
}

#[derive(Default)]
struct Weighted {
    sum: f64,
    weight: f64,
    max: f64,
}

impl Weighted {
    fn add(&mut self, value: f64, weight: f64) {
        self.sum += value * weight;
        self.weight += weight;
        self.max = self.max.max(value);
    }
    fn mean(&self) -> f64 {
        if self.weight > 0.0 {
            self.sum / self.weight
        } else {
            0.0
        }
    }
}

pub fn analyze_island(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    index: usize,
    resolution: u32,
) -> IslandReport {
    let faces = islands.faces(index);
    let mut maps = Vec::new();
    let mut degenerate = 0;
    let mut area_3d = 0.0;
    let mut signed = 0.0;
    let mut bbox_min = DVec2::splat(f64::INFINITY);
    let mut bbox_max = DVec2::splat(f64::NEG_INFINITY);
    for &f in faces {
        let f = f as usize;
        for c in mesh.face_corners(f) {
            let p = uv.uv_at_corner(c);
            bbox_min = bbox_min.min(p);
            bbox_max = bbox_max.max(p);
        }
        for tri in mesh.fan_triangles(f) {
            let p = tri.map(|c| mesh.corner_position(c));
            match triangle_map(p, tri.map(|c| uv.uv_at_corner(c))) {
                Some(m) => {
                    area_3d += m.area_3d;
                    signed += m.uv_area;
                    maps.push(m);
                }
                None => {
                    degenerate += 1;
                    area_3d += 0.5 * (p[1] - p[0]).cross(p[2] - p[0]).length();
                }
            }
        }
    }

    let flipped = signed < 0.0;
    let uv_area: f64 = maps.iter().map(|m| m.uv_area.abs()).sum();
    let flipped_triangles = maps.iter().filter(|m| (m.uv_area < 0.0) != flipped).count();
    let measured_3d: f64 = maps.iter().map(|m| m.area_3d).sum();
    let mut angle = Weighted::default();
    let mut area = Weighted::default();
    if uv_area > 0.0 && measured_3d > 0.0 {
        let scale = (uv_area / measured_3d).sqrt();
        for m in &maps {
            angle.add(m.sigma_max / m.sigma_min, m.area_3d);
            let ratio = m.sigma_max * m.sigma_min / (scale * scale);
            area.add(ratio.max(1.0 / ratio), m.area_3d);
        }
    }

    let eps = 1e-9;
    IslandReport {
        index,
        face_count: faces.len(),
        area_3d,
        uv_area,
        texel_density: texel_density(uv_area, measured_3d, resolution),
        bbox_min: bbox_min.to_array(),
        bbox_max: bbox_max.to_array(),
        flipped,
        flipped_triangles,
        degenerate_triangles: degenerate,
        outside_unit_tile: bbox_min.min_element() < -eps || bbox_max.max_element() > 1.0 + eps,
        crosses_tile: (bbox_min + eps).floor() != (bbox_max - eps).floor(),
        mean_angle_distortion: angle.mean(),
        max_angle_distortion: angle.max,
        mean_area_distortion: area.mean(),
        max_area_distortion: area.max,
    }
}

pub fn analyze(mesh: &Mesh, uv: &UvLayer, islands: &Islands, resolution: u32) -> Report {
    let reports: Vec<IslandReport> = (0..islands.count())
        .map(|i| analyze_island(mesh, uv, islands, i, resolution))
        .collect();

    let (mut seam_edges, mut seam_length, mut border_edges, mut non_manifold_edges) =
        (0, 0.0, 0, 0);
    for e in 0..mesh.edge_count() {
        match uv.edge_kind(mesh, e) {
            EdgeKind::Seam => {
                seam_edges += 1;
                seam_length += mesh.edge_length(e);
            }
            EdgeKind::Border => border_edges += 1,
            EdgeKind::NonManifold => non_manifold_edges += 1,
            EdgeKind::Interior => {}
        }
    }

    let mut angle = Weighted::default();
    let mut area = Weighted::default();
    for r in &reports {
        angle.sum += r.mean_angle_distortion * r.area_3d;
        area.sum += r.mean_area_distortion * r.area_3d;
        if r.mean_angle_distortion > 0.0 {
            angle.weight += r.area_3d;
            area.weight += r.area_3d;
        }
        angle.max = angle.max.max(r.max_angle_distortion);
        area.max = area.max.max(r.max_area_distortion);
    }

    Report {
        resolution,
        face_count: mesh.face_count(),
        island_count: reports.len(),
        area_3d: reports.iter().map(|r| r.area_3d).sum(),
        uv_area: reports.iter().map(|r| r.uv_area).sum(),
        flipped_islands: reports.iter().filter(|r| r.flipped).count(),
        flipped_triangles: reports.iter().map(|r| r.flipped_triangles).sum(),
        degenerate_triangles: reports.iter().map(|r| r.degenerate_triangles).sum(),
        islands_outside_unit_tile: reports.iter().filter(|r| r.outside_unit_tile).count(),
        islands_crossing_tile: reports.iter().filter(|r| r.crosses_tile).count(),
        seam_edges,
        seam_length,
        border_edges,
        non_manifold_edges,
        mean_angle_distortion: angle.mean(),
        max_angle_distortion: angle.max,
        mean_area_distortion: area.mean(),
        max_area_distortion: area.max,
        islands: reports,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: [DVec3; 3] = [
        DVec3::ZERO,
        DVec3::new(2.0, 0.0, 0.0),
        DVec3::new(0.0, 2.0, 0.0),
    ];

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn uniform_scale_is_conformal() {
        let u = [DVec2::ZERO, DVec2::new(0.5, 0.0), DVec2::new(0.0, 0.5)];
        let m = triangle_map(P, u).unwrap();
        assert!(approx(m.sigma_max, 0.25) && approx(m.sigma_min, 0.25));
        assert!(approx(m.uv_area, 0.125) && approx(m.area_3d, 2.0));
    }

    #[test]
    fn stretch_and_mirror() {
        // u stretched 2× relative to v, and mirrored.
        let u = [DVec2::ZERO, DVec2::new(-2.0, 0.0), DVec2::new(0.0, 1.0)];
        let m = triangle_map(P, u).unwrap();
        assert!(approx(m.sigma_max, 1.0) && approx(m.sigma_min, 0.5));
        assert!(m.uv_area < 0.0);
    }

    #[test]
    fn degenerate_uv_is_skipped() {
        assert!(triangle_map(P, [DVec2::ZERO; 3]).is_none());
        let line = [DVec2::ZERO, DVec2::X, DVec2::new(2.0, 0.0)];
        assert!(triangle_map(P, line).is_none());
    }

    fn quad(uvs: [DVec2; 4]) -> (Mesh, UvLayer) {
        let p = vec![DVec3::ZERO, DVec3::X, DVec3::new(1.0, 1.0, 0.0), DVec3::Y];
        let mesh = Mesh::new(p, &[[0, 1, 2, 3]]).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 3],
            uvs: uvs.to_vec(),
        };
        (mesh, uv)
    }

    #[test]
    fn island_report() {
        let s = 0.25;
        let (mesh, uv) = quad([
            DVec2::ZERO,
            DVec2::new(s, 0.0),
            DVec2::splat(s),
            DVec2::new(0.0, s),
        ]);
        let islands = Islands::compute(&mesh, &uv);
        let r = analyze(&mesh, &uv, &islands, 1024);
        assert_eq!(r.island_count, 1);
        assert_eq!(r.border_edges, 4);
        let i = &r.islands[0];
        assert!(approx(i.texel_density, 256.0), "{}", i.texel_density);
        assert!(approx(i.mean_angle_distortion, 1.0) && approx(i.max_area_distortion, 1.0));
        assert!(!i.flipped && !i.outside_unit_tile && !i.crosses_tile);
    }

    #[test]
    fn flipped_and_out_of_tile() {
        let (mesh, uv) = quad([
            DVec2::new(1.5, 0.0),
            DVec2::new(1.2, 0.0),
            DVec2::new(1.2, 0.3),
            DVec2::new(1.5, 0.3),
        ]);
        let islands = Islands::compute(&mesh, &uv);
        let r = analyze(&mesh, &uv, &islands, 1024);
        assert_eq!(r.flipped_islands, 1);
        assert_eq!(r.flipped_triangles, 0);
        assert!(r.islands[0].outside_unit_tile && !r.islands[0].crosses_tile);
    }
}
