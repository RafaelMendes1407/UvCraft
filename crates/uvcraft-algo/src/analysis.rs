//! UV quality analysis: distortion, flips, texel density and statistics.
//!
//! Distortion comes from the singular values σ1 ≥ σ2 of each triangle's
//! surface→UV Jacobian. `distortion_values` accepts one shared reference scale
//! for the heatmap; the legacy report normalizes by island and keeps unsigned
//! area distortion for compatibility.
//!
//! Means are weighted by 3D area. N-gons are measured by fan triangulation.

use std::collections::BTreeSet;

use glam::{DVec2, DVec3};
use serde::Serialize;
use uvcraft_core::{EdgeKind, Islands, Mesh, UvLayer};

use crate::checks::{self, PaddingViolation};

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
    if !p.iter().all(|point| point.is_finite()) || !u.iter().all(|point| point.is_finite()) {
        return None;
    }
    let (e1, e2) = (p[1] - p[0], p[2] - p[0]);
    let n = e1.cross(e2);
    let area_3d = 0.5 * n.length();
    let longest_3d = e1
        .length_squared()
        .max(e2.length_squared())
        .max((p[2] - p[1]).length_squared());
    if !area_3d.is_finite() || !longest_3d.is_finite()
        || area_3d <= DEGENERATE_EPS * longest_3d || longest_3d == 0.0 {
        return None;
    }
    let (d1, d2) = (u[1] - u[0], u[2] - u[0]);
    let uv_area = 0.5 * d1.perp_dot(d2);
    let longest_uv = d1
        .length_squared()
        .max(d2.length_squared())
        .max((u[2] - u[1]).length_squared());
    if !uv_area.is_finite() || !longest_uv.is_finite()
        || uv_area.abs() <= DEGENERATE_EPS * longest_uv || longest_uv == 0.0 {
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
    let sigma_max = q + r;
    // Product of singular values is |det J|. This avoids cancellation in
    // q - r for strongly anisotropic but otherwise valid mappings.
    let sigma_min = (uv_area / area_3d).abs() / sigma_max;
    if !sigma_max.is_finite() || !sigma_min.is_finite()
        || sigma_max <= 0.0 || sigma_min <= 0.0 {
        return None;
    }
    Some(TriangleMap {
        area_3d,
        uv_area,
        sigma_max,
        sigma_min,
    })
}

/// Distortion of the surface→UV map against one shared linear reference scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DistortionValues {
    pub major: f64,
    pub minor: f64,
    pub area: f64,
    pub conformal: f64,
    pub combined: f64,
    /// Signed log of the largest directional deviation: positive means UV
    /// expansion, negative means UV compression.
    pub directional_log: f64,
    pub flipped: bool,
}

pub fn distortion_values(map: TriangleMap, reference_scale: f64) -> Option<DistortionValues> {
    if !reference_scale.is_finite() || reference_scale <= 0.0 { return None; }
    let major = map.sigma_max / reference_scale;
    let minor = map.sigma_min / reference_scale;
    if !major.is_finite() || !minor.is_finite() || major <= 0.0 || minor <= 0.0 {
        return None;
    }
    let log_major = major.ln();
    let log_minor = minor.ln();
    let directional_log = if log_major.abs() >= log_minor.abs() {
        log_major
    } else { log_minor };
    let area = major * minor;
    let conformal = map.sigma_max / map.sigma_min;
    if !area.is_finite() || !conformal.is_finite() || !directional_log.is_finite() {
        return None;
    }
    Some(DistortionValues {
        major, minor,
        area,
        conformal,
        combined: log_major.abs().max(log_minor.abs()),
        directional_log,
        flipped: map.uv_area < 0.0,
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
    /// Faces overlapping another face (of this or another island).
    pub overlapping_faces: usize,
    /// Some faces of the island overlap each other.
    pub self_overlapping: bool,
    /// The island overlaps another island.
    pub overlaps_other_island: bool,
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
    /// Mesh problems (non-manifold, duplicates, degenerate faces…). Filled by
    /// the `analyze` command from its cache; `analyze` itself leaves it `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mesh_problems: Option<crate::validate::MeshProblems>,
    /// glTF meshes measured in local space because they are instanced, skinned or animated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub measurement_local_meshes: Vec<usize>,
    /// Faces overlapping another face.
    pub overlapping_faces: usize,
    pub self_overlapping_islands: usize,
    /// Pairs of distinct islands that overlap, `[a, b]` with `a < b`, sorted.
    pub overlapping_island_pairs: Vec<[usize; 2]>,
    /// Overlaps explicitly recorded as intentional stacks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intentional_overlapping_island_pairs: Vec<[usize; 2]>,
    /// Required padding for `padding_violations` (0: not checked).
    pub padding_px: f64,
    /// Island pairs closer than `padding_px` (overlapping pairs at distance 0).
    pub padding_violations: Vec<PaddingViolation>,
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
        overlapping_faces: 0,
        self_overlapping: false,
        overlaps_other_island: false,
    }
}

/// [`analyze_with_padding`] without the padding check (overlaps are still checked).
pub fn analyze(mesh: &Mesh, uv: &UvLayer, islands: &Islands, resolution: u32) -> Report {
    analyze_with_padding(mesh, uv, islands, resolution, 0.0)
}

/// Full report, including overlaps and island pairs closer than `padding_px`
/// pixels at `resolution` (skipped when `padding_px` is 0).
pub fn analyze_with_padding(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    resolution: u32,
    padding_px: f64,
) -> Report {
    analyze_with_padding_and_stacks(mesh, uv, islands, resolution, padding_px, &[])
}

/// Full report with explicit stacks excluded from accidental-overlap and
/// padding diagnostics. Within-island overlap still counts as an issue.
pub fn analyze_with_padding_and_stacks(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    resolution: u32,
    padding_px: f64,
    stack_groups: &[Vec<usize>],
) -> Report {
    let mut reports: Vec<IslandReport> = (0..islands.count())
        .map(|i| analyze_island(mesh, uv, islands, i, resolution))
        .collect();
    let checks = checks::check(mesh, uv, islands, padding_px, resolution);
    let intentional: BTreeSet<[usize; 2]> = stack_groups
        .iter()
        .flat_map(|group| {
            (0..group.len()).flat_map(move |a| {
                (a + 1..group.len()).map(move |b| [group[a].min(group[b]), group[a].max(group[b])])
            })
        })
        .collect();
    let intentional_overlapping_island_pairs: Vec<[usize; 2]> = checks
        .overlaps
        .island_pairs
        .iter()
        .copied()
        .filter(|pair| intentional.contains(pair))
        .collect();
    let mut faces = BTreeSet::new();
    let mut self_overlapping_islands = BTreeSet::new();
    let mut overlapping_island_pairs = BTreeSet::new();
    for pair in &checks.overlaps.face_pairs {
        let [a, b] = pair.islands;
        if a != b && intentional.contains(&[a.min(b), a.max(b)]) {
            continue;
        }
        faces.extend(pair.faces);
        if a == b {
            self_overlapping_islands.insert(a);
        } else {
            overlapping_island_pairs.insert([a.min(b), a.max(b)]);
        }
    }
    for &f in &faces {
        reports[islands.island_of_face(f)].overlapping_faces += 1;
    }
    for &i in &self_overlapping_islands {
        reports[i].self_overlapping = true;
    }
    for &i in overlapping_island_pairs.iter().flatten() {
        reports[i].overlaps_other_island = true;
    }

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
        mesh_problems: None,
        measurement_local_meshes: Vec::new(),
        overlapping_faces: faces.len(),
        self_overlapping_islands: self_overlapping_islands.len(),
        overlapping_island_pairs: overlapping_island_pairs.into_iter().collect(),
        intentional_overlapping_island_pairs,
        padding_px: padding_px.max(0.0),
        padding_violations: checks
            .padding_violations
            .into_iter()
            .filter(|violation| !intentional.contains(&violation.islands))
            .collect(),
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
    fn area_preserving_directional_stretch_is_not_classified_as_undistorted() {
        let p = [DVec3::ZERO, DVec3::X, DVec3::Y];
        let uv = [DVec2::ZERO, DVec2::X * 2.0, DVec2::Y * 0.5];
        let m = triangle_map(p, uv).unwrap();
        let d = distortion_values(m, 1.0).unwrap();
        assert!(approx(d.area, 1.0));
        assert!(approx(d.major, 2.0) && approx(d.minor, 0.5));
        assert!(approx(d.conformal, 4.0));
        assert!(approx(d.combined, 2.0_f64.ln()));
    }

    #[test]
    fn mapping_scale_rotation_mirror_and_degeneracy_are_stable() {
        let p = [DVec3::ZERO, DVec3::X, DVec3::Y];
        for scale in [0.5, 1.0, 2.0] {
            let theta: f64 = 0.73;
            let rotate = |v: DVec2| DVec2::new(
                v.x * theta.cos() - v.y * theta.sin(),
                v.x * theta.sin() + v.y * theta.cos()) * scale;
            let m = triangle_map(p, [DVec2::ZERO, rotate(DVec2::X), rotate(DVec2::Y)]).unwrap();
            let d = distortion_values(m, 1.0).unwrap();
            assert!(approx(d.major, scale) && approx(d.minor, scale));
            assert!(approx(d.conformal, 1.0));
            assert!(approx(distortion_values(m, scale).unwrap().combined, 0.0));
        }
        let mirrored = triangle_map(p, [DVec2::ZERO, -DVec2::X, DVec2::Y]).unwrap();
        assert!(distortion_values(mirrored, 1.0).unwrap().flipped);
        assert!(triangle_map(p, [DVec2::ZERO; 3]).is_none());
        assert!(triangle_map([DVec3::ZERO; 3], [DVec2::ZERO, DVec2::X, DVec2::Y]).is_none());
        assert!(triangle_map(p, [DVec2::ZERO, DVec2::X, DVec2::new(f64::NAN, 1.0)]).is_none());
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

    #[test]
    fn overlaps_and_padding_in_report() {
        // Two quads: the second a copy of the first shifted by 4 px at 100 px.
        let p = vec![DVec3::ZERO, DVec3::X, DVec3::new(1.0, 1.0, 0.0), DVec3::Y];
        let mut pos = p.clone();
        pos.extend(p.iter().map(|q| *q + DVec3::Z));
        let mesh = Mesh::new(pos, &[[0, 1, 2, 3], [4, 5, 6, 7]]).unwrap();
        let square = |x: f64| {
            [
                DVec2::new(x, 0.0),
                DVec2::new(x + 0.2, 0.0),
                DVec2::new(x + 0.2, 0.2),
                DVec2::new(x, 0.2),
            ]
        };
        let mut uvs = square(0.0).to_vec();
        uvs.extend(square(0.24));
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..8).collect(),
            uvs,
        };
        let islands = Islands::compute(&mesh, &uv);
        let r = analyze_with_padding(&mesh, &uv, &islands, 100, 8.0);
        assert_eq!(r.overlapping_faces, 0);
        assert_eq!(r.padding_violations.len(), 1);
        assert!((r.padding_violations[0].distance_px - 4.0).abs() < 1e-6);
        assert!(
            analyze(&mesh, &uv, &islands, 100)
                .padding_violations
                .is_empty()
        );

        for k in 4..8 {
            uv.uvs[k].x -= 0.1;
        }
        let r = analyze_with_padding(&mesh, &uv, &islands, 100, 8.0);
        assert_eq!(r.overlapping_faces, 2);
        assert_eq!(r.overlapping_island_pairs, vec![[0, 1]]);
        assert_eq!(r.self_overlapping_islands, 0);
        assert!(
            r.islands
                .iter()
                .all(|i| i.overlaps_other_island && i.overlapping_faces == 1)
        );
        assert_eq!(r.padding_violations[0].distance_px, 0.0);

        let intentional =
            analyze_with_padding_and_stacks(&mesh, &uv, &islands, 100, 8.0, &[vec![0, 1]]);
        assert_eq!(intentional.overlapping_faces, 0);
        assert!(intentional.overlapping_island_pairs.is_empty());
        assert_eq!(
            intentional.intentional_overlapping_island_pairs,
            vec![[0, 1]]
        );
        assert!(intentional.padding_violations.is_empty());
        assert!(
            intentional
                .islands
                .iter()
                .all(|island| !island.overlaps_other_island)
        );
    }
}
