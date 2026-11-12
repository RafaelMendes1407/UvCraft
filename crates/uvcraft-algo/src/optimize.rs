//! As-Rigid-As-Possible optimization (Liu et al. 2008, "A Local/Global
//! Approach to Mesh Parameterization").
//!
//! Starting from the current UVs (typically LSCM), it alternates:
//! - local step: per triangle, the rotation R_t closest to its current
//!   3D → UV map (closed form in 2D);
//! - global step: the UVs minimizing Σ_t Σ_edges w_ij |(u_i − u_j) − R_t (x_i − x_j)|²,
//!   a cotangent-Laplacian system factorized once (faer sparse Cholesky).
//!
//! The result is close to isometric, so it trades a little angle distortion
//! for much less area distortion. User pins (two or more at distinct UVs)
//! are hard constraints; otherwise the island keeps its UV area and center
//! (and a single pin keeps its UV). Islands run in parallel.

use std::collections::HashMap;

use faer::linalg::solvers::Solve;
use faer::sparse::{SparseColMat, Triplet};
use faer::{Mat, Side};
use glam::DVec2;
use rayon::prelude::*;
use uvcraft_core::{Cancelled, Islands, Mesh, Progress, UvLayer};

/// Which relaxation the optimize command runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptimizeMethod {
    /// As-rigid-as-possible: rare flips are possible.
    Arap,
    /// Symmetric Dirichlet with SLIM: guaranteed flip-free (see [`crate::slim`]).
    #[default]
    Slim,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OptimizeReport {
    pub optimized: usize,
    /// Islands with no UV area (unfold them first) or a failed solve.
    pub failed: Vec<usize>,
}

/// New UVs of one island: (UV vertex, position).
type IslandResult = Option<Vec<(u32, DVec2)>>;

/// One triangle: local vertex indices, 2D rest shape and cotangent weights
/// of the edges (i→i+1 gets `w[k]` for k = i).
struct Tri {
    v: [usize; 3],
    x: [DVec2; 3],
    w: [f64; 3],
}

pub fn optimize_arap(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    pinned: &[bool],
    iterations: usize,
) -> OptimizeReport {
    optimize_arap_with_progress(
        mesh,
        uv,
        islands,
        targets,
        pinned,
        iterations,
        &Progress::default(),
    )
    .expect("default progress cannot be cancelled")
}

/// ARAP with per-island progress and transactional cancellation.
pub fn optimize_arap_with_progress(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    pinned: &[bool],
    iterations: usize,
    progress: &Progress,
) -> Result<OptimizeReport, Cancelled> {
    progress.check()?;
    let snapshot: &UvLayer = uv;
    let results: Vec<(usize, IslandResult)> = targets
        .par_iter()
        .map(|&i| {
            progress.check()?;
            let result = (
                i,
                optimize_island(mesh, snapshot, islands.faces(i), pinned, iterations),
            );
            progress.step("optimize_arap", targets.len() as u64)?;
            Ok(result)
        })
        .collect::<Result<Vec<_>, Cancelled>>()?;
    progress.check()?;
    let mut report = OptimizeReport::default();
    for (i, r) in results {
        match r {
            Some(values) => {
                for (k, p) in values {
                    uv.uvs[k as usize] = p;
                }
                report.optimized += 1;
            }
            None => report.failed.push(i),
        }
    }
    Ok(report)
}

fn signed_area(t: &Tri, u: &[DVec2]) -> f64 {
    let [a, b, c] = t.v.map(|k| u[k]);
    0.5 * (b - a).perp_dot(c - a)
}

fn optimize_island(
    mesh: &Mesh,
    uv: &UvLayer,
    faces: &[u32],
    user_pins: &[bool],
    iterations: usize,
) -> IslandResult {
    // Local numbering of the island's UV vertices.
    let mut local: HashMap<u32, usize> = HashMap::new();
    let mut ids: Vec<u32> = Vec::new();
    let mut is_pinned: Vec<bool> = Vec::new();
    let mut tris: Vec<Tri> = Vec::new();
    for &f in faces {
        for t in mesh.fan_triangles(f as usize) {
            let v = t.map(|c| {
                let k = uv.corner_uv[c];
                *local.entry(k).or_insert_with(|| {
                    ids.push(k);
                    is_pinned.push(
                        user_pins
                            .get(mesh.corner_vertex(c))
                            .copied()
                            .unwrap_or(false),
                    );
                    ids.len() - 1
                })
            });
            let [p0, p1, p2] = t.map(|c| mesh.corner_position(c));
            let (e1, e2) = (p1 - p0, p2 - p0);
            let n = e1.cross(e2);
            let area = 0.5 * n.length();
            if e1.length() == 0.0 || area <= 1e-14 * e1.length_squared().max(e2.length_squared()) {
                continue; // degenerate in 3D: no rest shape
            }
            let ax = e1 / e1.length();
            let ay = n.cross(e1).normalize();
            let x = [
                DVec2::ZERO,
                DVec2::new(e1.length(), 0.0),
                DVec2::new(e2.dot(ax), e2.dot(ay)),
            ];
            // Cotangent of the angle at vertex k weights the opposite edge (k+1 → k+2).
            let cot = |k: usize| {
                let (a, b) = (x[(k + 1) % 3] - x[k], x[(k + 2) % 3] - x[k]);
                a.dot(b) / a.perp_dot(b).abs().max(1e-300)
            };
            // Clamp: negative weights (obtuse angles) would make the system indefinite.
            let w = [0, 1, 2].map(|i| cot((i + 2) % 3).clamp(1e-4, 1e4));
            tris.push(Tri { v, x, w });
        }
    }
    let n = ids.len();
    let start: Vec<DVec2> = ids.iter().map(|&k| uv.uvs[k as usize]).collect();
    let uv_area: f64 = tris.iter().map(|t| signed_area(t, &start)).sum();
    if tris.is_empty() || uv_area.abs() <= 1e-18 {
        return None;
    }
    // A mirrored layout is optimized mirrored back at the end.
    let mirror = uv_area < 0.0;
    let mut u: Vec<DVec2> = start
        .iter()
        .map(|&p| if mirror { DVec2::new(-p.x, p.y) } else { p })
        .collect();

    // Fixed vertices: user pins if they pin down the layout, else one vertex
    // (translation only; rotation and scale follow the rest shape).
    let user: Vec<usize> = (0..n).filter(|&k| is_pinned[k]).collect();
    let spread = user
        .iter()
        .map(|&k| u[k])
        .fold((DVec2::INFINITY, DVec2::NEG_INFINITY), |(lo, hi), p| {
            (lo.min(p), hi.max(p))
        });
    let hard = user.len() >= 2 && (spread.1 - spread.0).max_element() > 1e-12;
    let mut fixed = vec![false; n];
    if hard {
        user.iter().for_each(|&k| fixed[k] = true);
    } else {
        fixed[tris[0].v[0]] = true;
    }
    // Vertices only on degenerate triangles have no equation: they stay put.
    let mut used = vec![false; n];
    tris.iter().flat_map(|t| t.v).for_each(|k| used[k] = true);
    let mut free_index = vec![usize::MAX; n];
    let mut nfree = 0;
    for k in 0..n {
        if used[k] && !fixed[k] {
            free_index[k] = nfree;
            nfree += 1;
        }
    }
    if nfree == 0 {
        return None;
    }

    // Cotangent Laplacian over free vertices (lower triangle).
    let mut triplets: Vec<Triplet<usize, usize, f64>> = Vec::new();
    let mut diag = vec![0.0; nfree];
    for t in &tris {
        for i in 0..3 {
            let (a, b, w) = (t.v[i], t.v[(i + 1) % 3], t.w[i]);
            let (fa, fb) = (free_index[a], free_index[b]);
            if fa != usize::MAX {
                diag[fa] += w;
            }
            if fb != usize::MAX {
                diag[fb] += w;
            }
            if fa != usize::MAX && fb != usize::MAX {
                triplets.push(Triplet::new(fa.max(fb), fa.min(fb), -w));
            }
        }
    }
    triplets.extend(diag.iter().enumerate().map(|(i, &d)| Triplet::new(i, i, d)));
    let matrix = SparseColMat::<usize, f64>::try_new_from_triplets(nfree, nfree, &triplets).ok()?;
    let llt = matrix.sp_cholesky(Side::Lower).ok()?;

    for _ in 0..iterations {
        // Local step.
        let rots: Vec<(f64, f64)> = tris
            .iter()
            .map(|t| {
                let mut s = [[0.0f64; 2]; 2];
                for i in 0..3 {
                    let j = (i + 1) % 3;
                    let du = u[t.v[i]] - u[t.v[j]];
                    let dx = t.x[i] - t.x[j];
                    s[0][0] += t.w[i] * du.x * dx.x;
                    s[0][1] += t.w[i] * du.x * dx.y;
                    s[1][0] += t.w[i] * du.y * dx.x;
                    s[1][1] += t.w[i] * du.y * dx.y;
                }
                let theta = (s[1][0] - s[0][1]).atan2(s[0][0] + s[1][1]);
                theta.sin_cos()
            })
            .collect();
        // Global step.
        let mut b = Mat::<f64>::zeros(nfree, 2);
        for (t, &(sin, cos)) in tris.iter().zip(&rots) {
            for i in 0..3 {
                let j = (i + 1) % 3;
                let (a, c, w) = (t.v[i], t.v[j], t.w[i]);
                let dx = t.x[i] - t.x[j];
                let r = DVec2::new(cos * dx.x - sin * dx.y, sin * dx.x + cos * dx.y) * w;
                // ∂/∂u_a: +w (u_a − u_c − r); ∂/∂u_c: −w (u_a − u_c − r).
                for (k, other, sign) in [(a, c, 1.0), (c, a, -1.0)] {
                    let fk = free_index[k];
                    if fk == usize::MAX {
                        continue;
                    }
                    let mut rhs = r * sign;
                    if free_index[other] == usize::MAX {
                        rhs += u[other] * w; // known neighbor moves to the right side
                    }
                    b[(fk, 0)] += rhs.x;
                    b[(fk, 1)] += rhs.y;
                }
            }
        }
        let x = llt.solve(&b);
        for k in 0..n {
            let f = free_index[k];
            if f != usize::MAX {
                u[k] = DVec2::new(x[(f, 0)], x[(f, 1)]);
            }
        }
        if u.iter().any(|p| !p.is_finite()) {
            return None;
        }
    }

    if mirror {
        u.iter_mut().for_each(|p| p.x = -p.x);
    }
    if !hard {
        // Keep the island's UV area and center (a single pin keeps its UV).
        let area: f64 = tris.iter().map(|t| signed_area(t, &u)).sum::<f64>().abs();
        let scale = (uv_area.abs() / area.max(1e-300)).sqrt();
        let bbox = |pts: &[DVec2]| {
            pts.iter()
                .fold((DVec2::INFINITY, DVec2::NEG_INFINITY), |(lo, hi), &p| {
                    (lo.min(p), hi.max(p))
                })
        };
        let (lo, hi) = bbox(&start);
        let (nlo, nhi) = bbox(&u);
        let (center, mid) = ((lo + hi) / 2.0, (nlo + nhi) / 2.0);
        u.iter_mut().for_each(|p| *p = center + (*p - mid) * scale);
        if let Some(&k) = user.first() {
            let shift = start[k] - u[k];
            u.iter_mut().for_each(|p| *p += shift);
        }
    }
    Some(ids.into_iter().zip(u).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::analyze;
    use crate::seams::{cut_to_disk, tests::torus};
    use crate::unfold::unfold_lscm;
    use uvcraft_core::CancelToken;

    #[test]
    fn cancel_arap_discards_results() {
        let (mesh, mut uv) = crate::seams::tests::grid(4);
        let islands = Islands::compute(&mesh, &uv);
        let before = uv.clone();
        let token = CancelToken::default();
        let captured = token.clone();
        let progress = Progress::new(token, move |_| captured.cancel());
        assert_eq!(
            optimize_arap_with_progress(&mesh, &mut uv, &islands, &[0], &[], 5, &progress),
            Err(Cancelled)
        );
        assert_eq!(uv, before);
    }

    #[test]
    fn arap_cuts_area_distortion_on_a_torus() {
        let (m, mut uv) = torus(32, 16);
        cut_to_disk(&m, &mut uv, &[0]);
        let isl = Islands::compute(&m, &uv);
        assert_eq!(unfold_lscm(&m, &mut uv, &isl, &[0], &[]).unfolded, 1);
        let lscm = analyze(&m, &uv, &isl, 1024);
        let area_before = analyze(&m, &uv, &isl, 1024).islands[0].uv_area;

        let r = optimize_arap(&m, &mut uv, &isl, &[0], &[], 30);
        assert_eq!(r.optimized, 1);
        let arap = analyze(&m, &uv, &isl, 1024);
        assert!(
            arap.mean_area_distortion < lscm.mean_area_distortion * 0.7,
            "area distortion {} → {}",
            lscm.mean_area_distortion,
            arap.mean_area_distortion
        );
        assert_eq!(arap.flipped_islands, 0);
        // Texel density kept: same UV area.
        assert!((arap.islands[0].uv_area - area_before).abs() < 1e-9 * area_before.max(1.0));
    }

    #[test]
    fn flat_grid_stays_exact_and_pins_hold() {
        let (m, mut uv) = crate::seams::tests::grid(4);
        let isl = Islands::compute(&m, &uv);
        // Already isometric (up to scale): nothing should change.
        let before = uv.clone();
        optimize_arap(&m, &mut uv, &isl, &[0], &[], 5);
        for (a, b) in uv.uvs.iter().zip(&before.uvs) {
            assert!((*a - *b).length() < 1e-9, "{a} vs {b}");
        }
        // Two pins: hard constraints.
        let mut pins = vec![false; m.vertex_count()];
        pins[0] = true;
        pins[24] = true;
        uv.uvs[24] = DVec2::new(2.0, 2.0);
        let origin = uv.uvs[0];
        optimize_arap(&m, &mut uv, &isl, &[0], &pins, 5);
        assert_eq!(uv.uvs[0], origin);
        assert_eq!(uv.uvs[24], DVec2::new(2.0, 2.0));
    }
}
