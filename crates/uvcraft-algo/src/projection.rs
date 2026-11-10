//! Initial projections. Planar projections keep the UV topology and only set
//! coordinates; box projection first cuts between faces facing different axes.

use glam::{DVec2, DVec3};
use serde::{Deserialize, Serialize};
use uvcraft_core::{Islands, Mesh, UvLayer};

use crate::seams::{cut_edges, edge_corner_pairs};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    X,
    Y,
    Z,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Projection {
    /// Along a world axis, for all target islands together.
    Planar(Axis),
    /// Each island along its own area-weighted average normal.
    Best,
    /// Each face along its dominant axis (±X, ±Y, ±Z); cuts between groups.
    Box,
}

/// Tangent basis (t, b) with t × b = n, so front faces keep their winding.
fn basis(n: DVec3) -> (DVec3, DVec3) {
    let n = n.normalize_or(DVec3::Z);
    let helper = if n.z.abs() < 0.9 { DVec3::Z } else { DVec3::Y };
    // Prefer "up" = world-ish vertical for readable results.
    let t = helper.cross(n).normalize();
    (t, n.cross(t))
}

/// Basis for one of the 6 box directions (label = axis * 2 + negative).
fn box_basis(label: usize) -> (DVec3, DVec3) {
    match label {
        0 => (DVec3::NEG_Z, DVec3::Y), // +X
        1 => (DVec3::Z, DVec3::Y),     // -X
        2 => (DVec3::X, DVec3::NEG_Z), // +Y
        3 => (DVec3::X, DVec3::Z),     // -Y
        4 => (DVec3::X, DVec3::Y),     // +Z
        _ => (DVec3::NEG_X, DVec3::Y), // -Z
    }
}

fn box_label(n: DVec3) -> usize {
    let a = n.abs();
    let axis = if a.x >= a.y && a.x >= a.z {
        0
    } else if a.y >= a.z {
        1
    } else {
        2
    };
    axis * 2 + usize::from(n[axis] < 0.0)
}

/// Projects the target islands, then fits them together into the 0–1 square
/// (uniform scale, so relative texel density is kept).
pub fn project(mesh: &Mesh, uv: &mut UvLayer, targets: &[usize], mode: Projection) {
    let islands = Islands::compute(mesh, uv);
    let faces: Vec<usize> = targets
        .iter()
        .flat_map(|&i| islands.faces(i).iter().map(|&f| f as usize))
        .collect();

    // Per-face basis.
    let mut face_basis: Vec<(usize, (DVec3, DVec3))> = Vec::with_capacity(faces.len());
    match mode {
        Projection::Planar(axis) => {
            let b = match axis {
                Axis::X => box_basis(0),
                Axis::Y => box_basis(2),
                Axis::Z => box_basis(4),
            };
            face_basis.extend(faces.iter().map(|&f| (f, b)));
        }
        Projection::Best => {
            for &i in targets {
                let n: DVec3 = islands
                    .faces(i)
                    .iter()
                    .map(|&f| mesh.face_area_normal(f as usize))
                    .sum();
                let b = basis(n);
                face_basis.extend(islands.faces(i).iter().map(|&f| (f as usize, b)));
            }
        }
        Projection::Box => {
            let mut label = vec![usize::MAX; mesh.face_count()];
            for &f in &faces {
                label[f] = box_label(mesh.face_area_normal(f));
            }
            let cuts: Vec<usize> = (0..mesh.edge_count())
                .filter(|&e| {
                    edge_corner_pairs(mesh, e).is_some_and(|[(a0, a1), _]| {
                        let (f0, f1) = (mesh.corner_face(a0), mesh.corner_face(a1));
                        label[f0] != usize::MAX && label[f1] != usize::MAX && label[f0] != label[f1]
                    })
                })
                .collect();
            cut_edges(mesh, uv, &cuts);
            face_basis.extend(faces.iter().map(|&f| (f, box_basis(label[f]))));
        }
    }

    let mut touched = Vec::new();
    for (f, (t, b)) in face_basis {
        for c in mesh.face_corners(f) {
            let p = mesh.corner_position(c);
            let i = uv.corner_uv[c];
            uv.uvs[i as usize] = DVec2::new(p.dot(t), p.dot(b));
            touched.push(i);
        }
    }
    touched.sort_unstable();
    touched.dedup();
    fit_unit_square(uv, &touched);
}

/// Uniformly scales and moves the given UV vertices into the 0–1 square.
pub fn fit_unit_square(uv: &mut UvLayer, ids: &[u32]) {
    if ids.is_empty() {
        return;
    }
    let (lo, hi) = ids
        .iter()
        .fold((DVec2::INFINITY, DVec2::NEG_INFINITY), |(lo, hi), &i| {
            (lo.min(uv.uvs[i as usize]), hi.max(uv.uvs[i as usize]))
        });
    let size = (hi - lo).max_element();
    let s = if size > 0.0 { 1.0 / size } else { 1.0 };
    for &i in ids {
        let p = &mut uv.uvs[i as usize];
        *p = (*p - lo) * s;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::analyze;
    use uvcraft_core::EdgeKind;

    fn cube() -> (Mesh, UvLayer) {
        let p = vec![
            DVec3::new(0., 0., 0.),
            DVec3::new(1., 0., 0.),
            DVec3::new(1., 1., 0.),
            DVec3::new(0., 1., 0.),
            DVec3::new(0., 0., 1.),
            DVec3::new(1., 0., 1.),
            DVec3::new(1., 1., 1.),
            DVec3::new(0., 1., 1.),
        ];
        let faces = [
            [0, 3, 2, 1],
            [4, 5, 6, 7],
            [0, 1, 5, 4],
            [1, 2, 6, 5],
            [2, 3, 7, 6],
            [3, 0, 4, 7],
        ];
        let m = Mesh::new(p, &faces).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..m.corner_count())
                .map(|c| m.corner_vertex(c) as u32)
                .collect(),
            uvs: vec![DVec2::ZERO; 8],
        };
        (m, uv)
    }

    #[test]
    fn box_projection_gives_six_undistorted_unflipped_islands() {
        let (m, mut uv) = cube();
        project(&m, &mut uv, &[0], Projection::Box);
        let isl = Islands::compute(&m, &uv);
        assert_eq!(isl.count(), 6);
        assert!((0..m.edge_count()).all(|e| uv.edge_kind(&m, e) == EdgeKind::Seam));
        let r = analyze(&m, &uv, &isl, 1024);
        assert_eq!(r.flipped_islands, 0);
        assert!((r.max_angle_distortion - 1.0).abs() < 1e-9);
        assert_eq!(r.islands_outside_unit_tile, 0);
    }

    #[test]
    fn planar_and_best_keep_topology() {
        let (m, mut uv) = cube();
        project(&m, &mut uv, &[0], Projection::Planar(Axis::Z));
        assert_eq!(Islands::compute(&m, &uv).count(), 1);
        assert_eq!(uv.uv_at_corner(0), DVec2::ZERO);
        assert_eq!(uv.uvs[6], DVec2::ONE);

        // A single quad facing +X: best-plane projection is conformal and not flipped.
        let p = vec![
            DVec3::new(0., 0., 0.),
            DVec3::new(0., 1., 0.),
            DVec3::new(0., 1., 2.),
            DVec3::new(0., 0., 2.),
        ];
        let q = Mesh::new(p, &[[0, 1, 2, 3]]).unwrap();
        let mut quv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 3],
            uvs: vec![DVec2::ZERO; 4],
        };
        project(&q, &mut quv, &[0], Projection::Best);
        let r = analyze(&q, &quv, &Islands::compute(&q, &quv), 1024);
        assert_eq!(r.flipped_islands, 0);
        assert!((r.max_angle_distortion - 1.0).abs() < 1e-9);
    }
}
