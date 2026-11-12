//! Unfold with Least Squares Conformal Maps (Lévy et al. 2002).
//!
//! Per triangle (local 2D coords from 3D, CCW), the Cauchy–Riemann residual
//! r = R∇u − ∇v is linear in the UVs. With e_k the edge opposite vertex k:
//!   A·|r|² = |Σ_k u_k e_k + v_k R e_k|² / (4A),   R = rotate by 90°.
//! Two vertices are pinned; the normal equations are solved with a sparse
//! Cholesky factorization (faer). Islands are independent and run in parallel.
//!
//! User pins (per mesh vertex): with two or more at distinct UVs they are hard
//! constraints at their current UVs and the island is not rescaled or moved;
//! a single pin keeps its UV and the island is placed around it.
//!
//! With [`UnfoldMethod::Abf`] the per-triangle target shapes come from ABF++
//! angles ([`crate::abf`]) instead of the 3D triangles (angle-based LSCM);
//! everything else (pins, placement) is shared.

use std::collections::{BTreeSet, HashMap};

use faer::linalg::solvers::Solve;
use faer::sparse::{SparseColMat, Triplet};
use faer::{Mat, Side};
use glam::{DVec2, DVec3};
use rayon::prelude::*;
use uvcraft_core::{Cancelled, Islands, Mesh, Progress, UvLayer};

/// Which flattening the unfold command runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnfoldMethod {
    /// Least squares conformal maps on the 3D triangles.
    #[default]
    Lscm,
    /// ABF++ angle optimization, then angle-based LSCM (falls back to LSCM).
    Abf,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UnfoldReport {
    pub unfolded: usize,
    /// Islands without a border (closed surfaces) cannot be flattened.
    pub skipped_closed: Vec<usize>,
    pub failed: Vec<usize>,
    /// ABF islands whose angle optimization did not converge (unfolded with LSCM).
    pub abf_fallback: Vec<usize>,
}

enum Outcome {
    /// New UVs, and whether ABF fell back to LSCM.
    Done(Vec<(u32, DVec2)>, bool),
    Closed,
    Failed,
}

/// Unfolds the target islands in place. Without user pins, each result keeps
/// the island's previous UV area and center (or 3D area at the origin if it
/// had none). `pinned` is per mesh vertex (empty or shorter = unpinned).
pub fn unfold_lscm(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    pinned: &[bool],
) -> UnfoldReport {
    unfold(mesh, uv, islands, targets, pinned, UnfoldMethod::Lscm)
}

/// Like [`unfold_lscm`], with a choice of method.
pub fn unfold(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    pinned: &[bool],
    method: UnfoldMethod,
) -> UnfoldReport {
    unfold_with_progress(
        mesh,
        uv,
        islands,
        targets,
        pinned,
        method,
        &Progress::default(),
    )
    .expect("default progress cannot be cancelled")
}

/// Like [`unfold`], but reports one completed island at a time and supports
/// cancellation. The UV layer is unchanged if cancellation is requested.
pub fn unfold_with_progress(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    pinned: &[bool],
    method: UnfoldMethod,
    progress: &Progress,
) -> Result<UnfoldReport, Cancelled> {
    progress.check()?;
    let snapshot: &UvLayer = uv;
    let results: Vec<(usize, Outcome)> = targets
        .par_iter()
        .map(|&i| {
            progress.check()?;
            let faces = islands.faces(i);
            let result = unfold_island(mesh, snapshot, faces, pinned, method);
            progress.step("unfold", targets.len() as u64)?;
            Ok((i, result))
        })
        .collect::<Result<Vec<_>, Cancelled>>()?;
    progress.check()?;
    let mut report = UnfoldReport::default();
    for (i, r) in results {
        match r {
            Outcome::Done(values, fell_back) => {
                for (k, p) in values {
                    uv.uvs[k as usize] = p;
                }
                report.unfolded += 1;
                if fell_back {
                    report.abf_fallback.push(i);
                }
            }
            Outcome::Closed => report.skipped_closed.push(i),
            Outcome::Failed => report.failed.push(i),
        }
    }
    Ok(report)
}

fn unfold_island(
    mesh: &Mesh,
    uv: &UvLayer,
    faces: &[u32],
    user_pins: &[bool],
    method: UnfoldMethod,
) -> Outcome {
    // Local numbering of the island's UV vertices.
    let mut local: HashMap<u32, usize> = HashMap::new();
    let mut ids: Vec<u32> = Vec::new();
    let mut pos3: Vec<DVec3> = Vec::new();
    let mut is_pinned: Vec<bool> = Vec::new();
    // Ordered, so pin choice (ties included) is deterministic.
    let mut boundary: BTreeSet<usize> = BTreeSet::new();
    for &f in faces {
        for c in mesh.face_corners(f as usize) {
            let k = uv.corner_uv[c];
            local.entry(k).or_insert_with(|| {
                ids.push(k);
                pos3.push(mesh.corner_position(c));
                is_pinned.push(
                    user_pins
                        .get(mesh.corner_vertex(c))
                        .copied()
                        .unwrap_or(false),
                );
                ids.len() - 1
            });
        }
    }
    for &f in faces {
        for c in mesh.face_corners(f as usize) {
            if uv.edge_kind(mesh, mesh.corner_edge(c)).is_island_boundary() {
                boundary.insert(local[&uv.corner_uv[c]]);
                boundary.insert(local[&uv.corner_uv[mesh.next(c)]]);
            }
        }
    }
    if boundary.is_empty() {
        return Outcome::Closed;
    }

    // Target triangle shapes (ABF), one per fan triangle in face order.
    let (shapes, fell_back) = match method {
        UnfoldMethod::Lscm => (None, false),
        UnfoldMethod::Abf => {
            let tris: Vec<[usize; 3]> = faces
                .iter()
                .flat_map(|&f| mesh.fan_triangles(f as usize))
                .map(|t| t.map(|c| local[&uv.corner_uv[c]]))
                .collect();
            let interior: Vec<bool> = (0..ids.len()).map(|k| !boundary.contains(&k)).collect();
            let shapes: Option<Vec<[DVec2; 3]>> = crate::abf::abf_angles(&tris, &pos3, &interior)
                .map(|angles| angles.iter().map(|&a| crate::abf::shape(a)).collect());
            let fell_back = shapes.is_none();
            (shapes, fell_back)
        }
    };
    let shapes: Option<&[[DVec2; 3]]> = shapes.as_deref();

    let n = ids.len();
    let user: Vec<usize> = (0..n).filter(|&k| is_pinned[k]).collect();
    let (ulo, uhi) = bbox(user.iter().map(|&k| uv.uvs[ids[k] as usize]));
    let mut pinned = vec![None; 2 * n];
    if user.len() >= 2 && (uhi - ulo).max_element() > 1e-12 {
        for &k in &user {
            let p = uv.uvs[ids[k] as usize];
            pinned[2 * k] = Some(p.x);
            pinned[2 * k + 1] = Some(p.y);
        }
        return match solve(mesh, uv, faces, &local, &pos3, &pinned, shapes) {
            Some((out, _)) => Outcome::Done(ids.iter().copied().zip(out).collect(), fell_back),
            None => Outcome::Failed,
        };
    }

    // Automatic pins: two boundary vertices far apart in 3D.
    let far = |from: usize| {
        *boundary
            .iter()
            .max_by(|&&a, &&b| {
                pos3[a]
                    .distance_squared(pos3[from])
                    .total_cmp(&pos3[b].distance_squared(pos3[from]))
            })
            .unwrap()
    };
    let start = *boundary.iter().min().unwrap();
    let p1 = far(start);
    let p0 = far(p1);
    let span = pos3[p0].distance(pos3[p1]);
    if p0 == p1 || span == 0.0 {
        return Outcome::Failed;
    }
    pinned[2 * p0] = Some(0.0);
    pinned[2 * p0 + 1] = Some(0.0);
    pinned[2 * p1] = Some(span);
    pinned[2 * p1 + 1] = Some(0.0);
    let Some((mut out, area_3d)) = solve(mesh, uv, faces, &local, &pos3, &pinned, shapes) else {
        return Outcome::Failed;
    };

    // Keep previous scale and center when the island had a real layout.
    let signed = |pts: &dyn Fn(usize) -> DVec2| -> f64 {
        faces
            .iter()
            .flat_map(|&f| mesh.fan_triangles(f as usize))
            .map(|t| {
                let [a, b, c] = t.map(|c| pts(local[&uv.corner_uv[c]]));
                0.5 * (b - a).perp_dot(c - a)
            })
            .sum()
    };
    let new_area = signed(&|k| out[k]);
    if new_area < 0.0 {
        for p in &mut out {
            p.x = -p.x;
        }
    }
    let old_area = signed(&|k| uv.uvs[ids[k] as usize]).abs();
    let (old_lo, old_hi) = bbox(ids.iter().map(|&k| uv.uvs[k as usize]));
    let (target_area, center) = if old_area > 1e-12 {
        (old_area, (old_lo + old_hi) / 2.0)
    } else {
        (area_3d, DVec2::ZERO)
    };
    let scale = (target_area / new_area.abs().max(1e-300)).sqrt();
    let (lo, hi) = bbox(out.iter().copied());
    let mid = (lo + hi) / 2.0;
    let mut placed: Vec<DVec2> = out.iter().map(|&p| center + (p - mid) * scale).collect();
    // A single user pin keeps its UV.
    if let Some(&k) = user.first() {
        let shift = uv.uvs[ids[k] as usize] - placed[k];
        placed.iter_mut().for_each(|p| *p += shift);
    }
    Outcome::Done(ids.iter().copied().zip(placed).collect(), fell_back)
}

/// Solves the LSCM normal equations with the given fixed coordinates
/// (`pinned[2k]` = u, `pinned[2k + 1]` = v of local vertex k). Returns the
/// UVs of every local vertex and the island's 3D area. `shapes` optionally
/// gives each fan triangle's target 2D shape (any scale; resized to its 3D area).
fn solve(
    mesh: &Mesh,
    uv: &UvLayer,
    faces: &[u32],
    local: &HashMap<u32, usize>,
    pos3: &[DVec3],
    pinned: &[Option<f64>],
    shapes: Option<&[[DVec2; 3]]>,
) -> Option<(Vec<DVec2>, f64)> {
    let n = pos3.len();
    let mut free_index = vec![usize::MAX; 2 * n];
    let mut nfree = 0;
    for (v, p) in pinned.iter().enumerate() {
        if p.is_none() {
            free_index[v] = nfree;
            nfree += 1;
        }
    }

    // Assemble normal equations (lower triangle only).
    let mut triplets: Vec<Triplet<usize, usize, f64>> = Vec::new();
    let mut rhs = vec![0.0; nfree];
    let mut diag_sum = 0.0;
    let mut area_3d = 0.0;
    let mut tri_index = 0;
    for &f in faces {
        for tri in mesh.fan_triangles(f as usize) {
            let shape = shapes.map(|s| s[tri_index]);
            tri_index += 1;
            let l = tri.map(|c| local[&uv.corner_uv[c]]);
            let p = l.map(|k| pos3[k]);
            let (e1, e2) = (p[1] - p[0], p[2] - p[0]);
            let nrm = e1.cross(e2);
            let area = 0.5 * nrm.length();
            if area <= 1e-14 * e1.length_squared().max(e2.length_squared()) || e1.length() == 0.0 {
                continue;
            }
            area_3d += area;
            let x = e1 / e1.length();
            let y = nrm.cross(e1).normalize();
            let q = match shape {
                Some(q) => {
                    let a = 0.5 * (q[1] - q[0]).perp_dot(q[2] - q[0]);
                    let k = (area / a).sqrt();
                    q.map(|p| p * k)
                }
                None => [
                    DVec2::ZERO,
                    DVec2::new(e1.length(), 0.0),
                    DVec2::new(e2.dot(x), e2.dot(y)),
                ],
            };
            let s = 1.0 / (2.0 * area.sqrt());
            // Edge opposite vertex k: q[k+2] - q[k+1].
            let e = [q[2] - q[1], q[0] - q[2], q[1] - q[0]];
            let rows: [[(usize, f64); 6]; 2] = [
                [
                    (2 * l[0], s * e[0].x),
                    (2 * l[0] + 1, -s * e[0].y),
                    (2 * l[1], s * e[1].x),
                    (2 * l[1] + 1, -s * e[1].y),
                    (2 * l[2], s * e[2].x),
                    (2 * l[2] + 1, -s * e[2].y),
                ],
                [
                    (2 * l[0], s * e[0].y),
                    (2 * l[0] + 1, s * e[0].x),
                    (2 * l[1], s * e[1].y),
                    (2 * l[1] + 1, s * e[1].x),
                    (2 * l[2], s * e[2].y),
                    (2 * l[2] + 1, s * e[2].x),
                ],
            ];
            for row in &rows {
                for &(a, va) in row {
                    let fa = free_index[a];
                    if fa == usize::MAX {
                        continue;
                    }
                    for &(b, vb) in row {
                        match pinned[b] {
                            Some(x) => rhs[fa] -= va * vb * x,
                            None => {
                                let fb = free_index[b];
                                if fa >= fb {
                                    triplets.push(Triplet::new(fa, fb, va * vb));
                                }
                                if fa == fb {
                                    diag_sum += va * vb;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if nfree == 0 {
        return None;
    }
    // Tiny regularization keeps vertices that only touch degenerate triangles solvable.
    let eps = 1e-9 * diag_sum.max(1e-30) / nfree as f64;
    triplets.extend((0..nfree).map(|i| Triplet::new(i, i, eps)));

    let Ok(matrix) = SparseColMat::<usize, f64>::try_new_from_triplets(nfree, nfree, &triplets)
    else {
        return None;
    };
    let Ok(llt) = matrix.sp_cholesky(Side::Lower) else {
        return None;
    };
    let b = Mat::<f64>::from_fn(nfree, 1, |i, _| rhs[i]);
    let x = llt.solve(&b);

    let out: Vec<DVec2> = (0..n)
        .map(|v| {
            let get = |k: usize| pinned[k].unwrap_or_else(|| x[(free_index[k], 0)]);
            DVec2::new(get(2 * v), get(2 * v + 1))
        })
        .collect();
    if out.iter().any(|p| !p.is_finite()) {
        return None;
    }
    Some((out, area_3d))
}

fn bbox(points: impl Iterator<Item = DVec2>) -> (DVec2, DVec2) {
    points.fold((DVec2::INFINITY, DVec2::NEG_INFINITY), |(lo, hi), p| {
        (lo.min(p), hi.max(p))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::analyze;
    use crate::seams::tests::grid;
    use uvcraft_core::CancelToken;

    #[test]
    fn cancellation_keeps_uv_layer_unchanged() {
        let (mesh, mut uv) = grid(4);
        let before = uv.clone();
        let islands = Islands::compute(&mesh, &uv);
        let token = CancelToken::default();
        let captured = token.clone();
        let progress = Progress::new(token, move |_| captured.cancel());
        let result = unfold_with_progress(
            &mesh,
            &mut uv,
            &islands,
            &[0],
            &[],
            UnfoldMethod::Lscm,
            &progress,
        );
        assert_eq!(result, Err(Cancelled));
        assert_eq!(uv, before);
    }

    #[test]
    fn flat_grid_unfolds_without_distortion() {
        let (m, mut uv) = grid(4);
        // Scramble the UVs first.
        for (i, p) in uv.uvs.iter_mut().enumerate() {
            *p = DVec2::new((i * 7 % 5) as f64, (i * 3 % 4) as f64) * 0.1;
        }
        let isl = Islands::compute(&m, &uv);
        let r = unfold_lscm(&m, &mut uv, &isl, &[0], &[]);
        assert_eq!(r.unfolded, 1);
        let a = analyze(&m, &uv, &isl, 1024);
        assert!(
            a.max_angle_distortion < 1.0 + 1e-6,
            "{}",
            a.max_angle_distortion
        );
        assert!(
            a.max_area_distortion < 1.0 + 1e-6,
            "{}",
            a.max_area_distortion
        );
        assert_eq!(a.flipped_triangles + a.flipped_islands, 0);
    }

    #[test]
    fn user_pins_are_kept_and_drive_the_layout() {
        let (m, mut uv) = grid(4);
        // Pins at vertex 0 (0,0) and 4 (4,0): a 90° turn, scale 1/4, moved.
        uv.uvs[0] = DVec2::new(0.5, 0.5);
        uv.uvs[4] = DVec2::new(0.5, 1.5);
        for p in uv.uvs.iter_mut().skip(5) {
            *p = DVec2::new(9.0, -3.0); // garbage
        }
        let mut pins = vec![false; m.vertex_count()];
        pins[0] = true;
        pins[4] = true;
        let isl = Islands::compute(&m, &uv);
        assert_eq!(unfold_lscm(&m, &mut uv, &isl, &[0], &pins).unfolded, 1);
        assert_eq!(uv.uvs[0], DVec2::new(0.5, 0.5));
        assert_eq!(uv.uvs[4], DVec2::new(0.5, 1.5));
        // Vertex 24 (4,4) maps to (-0.5, 1.5).
        assert!(
            (uv.uvs[24] - DVec2::new(-0.5, 1.5)).length() < 1e-6,
            "{}",
            uv.uvs[24]
        );

        // A single pin keeps its UV; the island keeps its own scale.
        let (m, mut uv) = grid(4);
        let mut pins = vec![false; m.vertex_count()];
        pins[12] = true;
        uv.uvs[12] = DVec2::new(3.0, 3.0);
        let isl = Islands::compute(&m, &uv);
        unfold_lscm(&m, &mut uv, &isl, &[0], &pins);
        assert_eq!(uv.uvs[12], DVec2::new(3.0, 3.0));
        let a = analyze(&m, &uv, &isl, 1024);
        assert!(a.max_angle_distortion < 1.0 + 1e-6);
    }

    #[test]
    fn curved_surface_unfolds_with_low_distortion() {
        // Half cylinder (radius 1, 180°), a developable surface: LSCM is near isometric.
        let (n_around, n_up) = (24, 8);
        let mut p = Vec::new();
        for j in 0..=n_up {
            for i in 0..=n_around {
                let t = std::f64::consts::PI * i as f64 / n_around as f64;
                p.push(DVec3::new(t.cos(), t.sin(), j as f64 * 0.25));
            }
        }
        let w = n_around + 1;
        let mut faces = Vec::new();
        for j in 0..n_up {
            for i in 0..n_around {
                let a = (j * w + i) as u32;
                faces.push(vec![a, a + w as u32, a + w as u32 + 1, a + 1]);
            }
        }
        let m = Mesh::new(p, &faces).unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..m.corner_count())
                .map(|c| m.corner_vertex(c) as u32)
                .collect(),
            uvs: vec![DVec2::ZERO; m.vertex_count()],
        };
        let isl = Islands::compute(&m, &uv);
        assert_eq!(unfold_lscm(&m, &mut uv, &isl, &[0], &[]).unfolded, 1);
        let a = analyze(&m, &uv, &isl, 1024);
        assert!(
            a.mean_angle_distortion < 1.01,
            "{}",
            a.mean_angle_distortion
        );
        assert!(a.mean_area_distortion < 1.02, "{}", a.mean_area_distortion);
        assert_eq!(a.flipped_triangles + a.flipped_islands, 0);
        // No previous layout: UV area matches 3D area.
        assert!((a.uv_area - a.area_3d).abs() / a.area_3d < 1e-6);
    }

    #[test]
    fn closed_islands_are_skipped() {
        let p = vec![DVec3::ZERO, DVec3::X, DVec3::Y, DVec3::Z];
        let m = Mesh::new(p, &[[0, 2, 1], [0, 1, 3], [1, 2, 3], [2, 0, 3]]).unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..12).map(|c| m.corner_vertex(c) as u32).collect(),
            uvs: vec![DVec2::ZERO; 4],
        };
        let isl = Islands::compute(&m, &uv);
        assert_eq!(
            unfold_lscm(&m, &mut uv, &isl, &[0], &[]).skipped_closed,
            vec![0]
        );
    }
}
