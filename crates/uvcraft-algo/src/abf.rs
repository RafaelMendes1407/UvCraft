//! Angle Based Flattening, ABF++ (Sheffer, Lévy, Mogilnitsky, Bogomyakov 2005).
//!
//! Unknowns are the planar angles α of every triangle corner. They minimize
//! Σ (α − β)² / β², where β are the 3D angles (rescaled to sum to 2π around
//! interior vertices), subject to:
//! - triangle: the three angles of a triangle sum to π;
//! - planarity: the angles around an interior vertex sum to 2π;
//! - wheel (sine law): Σ ln sin α_next − Σ ln sin α_prev = 0 around an
//!   interior vertex, so edge lengths agree going around it.
//!
//! Newton iterations on the Lagrangian use the ABF++ block elimination: the
//! energy Hessian is diagonal and every angle is in one triangle constraint,
//! so both are eliminated in closed form, leaving one sparse SPD system over
//! the planarity and wheel multipliers (faer sparse Cholesky). The UVs are then
//! rebuilt from the angles by angle-based LSCM (see [`crate::unfold`]).

use std::f64::consts::PI;

use faer::linalg::solvers::Solve;
use faer::sparse::linalg::solvers::{Llt, SymbolicLlt};
use faer::sparse::{SparseColMat, Triplet};
use faer::{Mat, Side};
use glam::{DVec2, DVec3};

/// Smallest target angle; keeps the weights 1/β² bounded on slivers.
const MIN_ANGLE: f64 = 0.5 * PI / 180.0;
/// Angles are kept inside (EPS_ANGLE, π − EPS_ANGLE) during the iterations.
const EPS_ANGLE: f64 = 1e-6;
const MAX_ITERATIONS: usize = 50;
/// Stop when the largest constraint residual (radians) is below this...
const TOLERANCE: f64 = 1e-10;
/// ...or when the largest angle step is.
const STEP_TOLERANCE: f64 = 1e-12;
/// The result is accepted if every constraint holds to this (radians).
const ACCEPT: f64 = 1e-6;

/// A CCW triangle with the given corner angles (any scale).
pub(crate) fn shape(a: [f64; 3]) -> [DVec2; 3] {
    [
        DVec2::ZERO,
        DVec2::new(a[2].sin(), 0.0),
        DVec2::new(a[0].cos(), a[0].sin()) * a[1].sin(),
    ]
}

/// The 3D corner angles of a triangle.
fn angles_3d(p: [DVec3; 3]) -> [f64; 3] {
    let angle = |a: DVec3, b: DVec3, c: DVec3| {
        let (u, v) = (b - a, c - a);
        u.cross(v).length().atan2(u.dot(v))
    };
    [
        angle(p[0], p[1], p[2]),
        angle(p[1], p[2], p[0]),
        angle(p[2], p[0], p[1]),
    ]
}

/// Optimizes the planar angles of the triangles `tris` (local vertex indices
/// into `pos3`). `interior[v]` marks vertices whose wheel is complete.
/// Returns the angles per triangle, or `None` if the solve did not converge.
pub(crate) fn abf_angles(
    tris: &[[usize; 3]],
    pos3: &[DVec3],
    interior: &[bool],
) -> Option<Vec<[f64; 3]>> {
    let nt = tris.len();
    if nt == 0 {
        return None;
    }
    // Interior vertex rows: planarity row r, wheel row nint + r.
    let mut row = vec![usize::MAX; pos3.len()];
    let mut nint = 0;
    for (v, &inside) in interior.iter().enumerate() {
        if inside {
            row[v] = nint;
            nint += 1;
        }
    }

    // Target angles β, rescaled around interior vertices, and weights.
    let mut beta: Vec<f64> = tris
        .iter()
        .flat_map(|t| angles_3d(t.map(|v| pos3[v])))
        .map(|a| if a.is_finite() { a } else { PI / 3.0 })
        .collect();
    let mut sum = vec![0.0; nint];
    for (c, &b) in beta.iter().enumerate() {
        let r = row[tris[c / 3][c % 3]];
        if r != usize::MAX {
            sum[r] += b;
        }
    }
    for (c, b) in beta.iter_mut().enumerate() {
        let r = row[tris[c / 3][c % 3]];
        if r != usize::MAX && sum[r] > 0.0 {
            *b *= 2.0 * PI / sum[r];
        }
        *b = b.clamp(MIN_ANGLE, PI - 2.0 * MIN_ANGLE);
    }
    // Hessian of the energy is diag(2/β²); store its inverse.
    let inv_h: Vec<f64> = beta.iter().map(|b| 0.5 * b * b).collect();

    let mut alpha = beta.clone();
    let mut lambda_tri = vec![0.0; nt];
    let mut lambda2 = vec![0.0; 2 * nint];

    // Constraint rows touched by a triangle: planarity of v0..v2, wheel of v0..v2.
    let rows_of = |t: usize| -> [usize; 6] {
        let r = tris[t].map(|v| row[v]);
        let w = r.map(|x| if x == usize::MAX { x } else { nint + x });
        [r[0], r[1], r[2], w[0], w[1], w[2]]
    };
    // ∂C/∂α for those 6 rows (row-major 6×3) at the current angles.
    let jacobian = |t: usize, alpha: &[f64]| -> [[f64; 3]; 6] {
        let r = rows_of(t);
        let cot = [0, 1, 2].map(|k| 1.0 / alpha[3 * t + k].tan());
        let mut j = [[0.0; 3]; 6];
        for k in 0..3 {
            if r[k] != usize::MAX {
                j[k][k] = 1.0;
                // Wheel of v_k: + ln sin α_{k+1} − ln sin α_{k+2}.
                j[3 + k][(k + 1) % 3] = cot[(k + 1) % 3];
                j[3 + k][(k + 2) % 3] = -cot[(k + 2) % 3];
            }
        }
        j
    };
    let residuals = |alpha: &[f64]| -> (Vec<f64>, Vec<f64>) {
        let tri: Vec<f64> = (0..nt)
            .map(|t| alpha[3 * t] + alpha[3 * t + 1] + alpha[3 * t + 2] - PI)
            .collect();
        let mut c2 = vec![0.0; 2 * nint];
        c2[..nint].fill(-2.0 * PI);
        for (t, verts) in tris.iter().enumerate() {
            for k in 0..3 {
                let r = row[verts[k]];
                if r == usize::MAX {
                    continue;
                }
                c2[r] += alpha[3 * t + k];
                c2[nint + r] +=
                    alpha[3 * t + (k + 1) % 3].sin().ln() - alpha[3 * t + (k + 2) % 3].sin().ln();
            }
        }
        (tri, c2)
    };
    let max_abs = |v: &[f64]| v.iter().fold(0.0f64, |m, x| m.max(x.abs()));

    let mut symbolic: Option<SymbolicLlt<usize>> = None;
    for _ in 0..MAX_ITERATIONS {
        let (c_tri, c2) = residuals(&alpha);
        if max_abs(&c_tri).max(max_abs(&c2)) < TOLERANCE {
            break;
        }
        // b1 = −∇_α L.
        let mut b1: Vec<f64> = (0..3 * nt)
            .map(|c| -(2.0 * (alpha[c] - beta[c]) / (beta[c] * beta[c]) + lambda_tri[c / 3]))
            .collect();
        let jac: Vec<[[f64; 3]; 6]> = (0..nt).map(|t| jacobian(t, &alpha)).collect();
        for t in 0..nt {
            let r = rows_of(t);
            for (i, &ri) in r.iter().enumerate() {
                if ri == usize::MAX {
                    continue;
                }
                for k in 0..3 {
                    b1[3 * t + k] -= jac[t][i][k] * lambda2[ri];
                }
            }
        }

        // Block elimination. Per triangle: Λ*_t = Σ inv_h, j_t = J2_t D_t 1.
        let mut triplets: Vec<Triplet<usize, usize, f64>> = Vec::new();
        let mut rhs: Vec<f64> = c2.clone(); // − b2_2 = + C2
        let mut lstar = vec![0.0; nt];
        let mut b1star = vec![0.0; nt];
        let mut jt = vec![[0.0; 6]; nt];
        let mut diag_sum = 0.0;
        for t in 0..nt {
            let d = [0, 1, 2].map(|k| inv_h[3 * t + k]);
            lstar[t] = d[0] + d[1] + d[2];
            // b1* = J1 Λ⁻¹ b1 − b2_1, with b2_1 = −C_tri.
            b1star[t] = (0..3).map(|k| d[k] * b1[3 * t + k]).sum::<f64>() + c_tri[t];
            let r = rows_of(t);
            let j = &jac[t];
            for i in 0..6 {
                if r[i] == usize::MAX {
                    continue;
                }
                jt[t][i] = (0..3).map(|k| j[i][k] * d[k]).sum();
                rhs[r[i]] += (0..3).map(|k| j[i][k] * d[k] * b1[3 * t + k]).sum::<f64>()
                    - jt[t][i] * b1star[t] / lstar[t];
            }
            for i in 0..6 {
                if r[i] == usize::MAX {
                    continue;
                }
                for m in 0..6 {
                    if r[m] == usize::MAX || r[m] > r[i] {
                        continue;
                    }
                    let v = (0..3).map(|k| j[i][k] * j[m][k] * d[k]).sum::<f64>()
                        - jt[t][i] * jt[t][m] / lstar[t];
                    triplets.push(Triplet::new(r[i], r[m], v));
                    if r[i] == r[m] {
                        diag_sum += v.abs();
                    }
                }
            }
        }

        let dl2: Vec<f64> = if nint == 0 {
            Vec::new()
        } else {
            let n = 2 * nint;
            let eps = 1e-12 * diag_sum.max(1e-300) / n as f64;
            triplets.extend((0..n).map(|i| Triplet::new(i, i, eps)));
            let matrix = SparseColMat::<usize, f64>::try_new_from_triplets(n, n, &triplets).ok()?;
            // The sparsity pattern is the same every iteration: analyze it once.
            if symbolic.is_none() {
                symbolic = Some(SymbolicLlt::try_new(matrix.symbolic(), Side::Lower).ok()?);
            }
            let llt =
                Llt::try_new_with_symbolic(symbolic.clone()?, matrix.as_ref(), Side::Lower).ok()?;
            let b = Mat::<f64>::from_fn(n, 1, |i, _| rhs[i]);
            let x = llt.solve(&b);
            (0..n).map(|i| x[(i, 0)]).collect()
        };
        if dl2.iter().any(|x| !x.is_finite()) {
            return None;
        }

        let mut max_step: f64 = 0.0;
        for t in 0..nt {
            let r = rows_of(t);
            let jdl: f64 = (0..6)
                .filter(|&i| r[i] != usize::MAX)
                .map(|i| jt[t][i] * dl2[r[i]])
                .sum();
            let dl1 = (b1star[t] - jdl) / lstar[t];
            lambda_tri[t] += dl1;
            let corners = 3 * t..3 * t + 3;
            let rest = inv_h[corners.clone()].iter().zip(&b1[corners.clone()]);
            for (k, (a, (&h, &g))) in alpha[corners].iter_mut().zip(rest).enumerate() {
                let j2dl: f64 = (0..6)
                    .filter(|&i| r[i] != usize::MAX)
                    .map(|i| jac[t][i][k] * dl2[r[i]])
                    .sum();
                let da = h * (g - dl1 - j2dl);
                if !da.is_finite() {
                    return None;
                }
                max_step = max_step.max(da.abs());
                *a = (*a + da).clamp(EPS_ANGLE, PI - EPS_ANGLE);
            }
        }
        for (l, d) in lambda2.iter_mut().zip(&dl2) {
            *l += d;
        }
        if max_step < STEP_TOLERANCE {
            break;
        }
    }

    let (c_tri, c2) = residuals(&alpha);
    if max_abs(&c_tri).max(max_abs(&c2)) > ACCEPT || alpha.iter().any(|a| !a.is_finite()) {
        return None;
    }
    Some(
        (0..nt)
            .map(|t| [alpha[3 * t], alpha[3 * t + 1], alpha[3 * t + 2]])
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::analyze;
    use crate::unfold::{UnfoldMethod, unfold};
    use uvcraft_core::{Islands, Mesh, UvLayer};

    fn layer(m: &Mesh) -> UvLayer {
        UvLayer {
            name: "uv".into(),
            corner_uv: (0..m.corner_count())
                .map(|c| m.corner_vertex(c) as u32)
                .collect(),
            uvs: vec![DVec2::ZERO; m.vertex_count()],
        }
    }

    /// Spherical cap (polar angle up to `max_theta`): rings around a pole, each
    /// turned by half a segment so the triangles are well shaped.
    fn cap(rings: usize, segments: usize, max_theta: f64) -> Mesh {
        let mut p = vec![DVec3::Z];
        for r in 1..=rings {
            let th = max_theta * r as f64 / rings as f64;
            for s in 0..segments {
                let ph = 2.0 * PI * s as f64 / segments as f64 + PI * r as f64 / segments as f64;
                p.push(DVec3::new(
                    th.sin() * ph.cos(),
                    th.sin() * ph.sin(),
                    th.cos(),
                ));
            }
        }
        let idx = |r: usize, s: usize| (1 + (r - 1) * segments + s % segments) as u32;
        let mut f = Vec::new();
        for s in 0..segments {
            f.push([0, idx(1, s), idx(1, s + 1)]);
        }
        for r in 1..rings {
            for s in 0..segments {
                f.push([idx(r, s), idx(r + 1, s), idx(r + 1, s + 1)]);
                f.push([idx(r, s), idx(r + 1, s + 1), idx(r, s + 1)]);
            }
        }
        Mesh::new(p, &f).unwrap()
    }

    #[test]
    fn shape_has_the_angles() {
        let a = [0.4, 1.1, PI - 1.5];
        let q = shape(a);
        let got = angles_3d(q.map(|p| p.extend(0.0)));
        for k in 0..3 {
            assert!((got[k] - a[k]).abs() < 1e-12);
        }
        assert!((q[1] - q[0]).perp_dot(q[2] - q[0]) > 0.0);
    }

    #[test]
    fn angles_satisfy_constraints() {
        let m = cap(6, 16, 1.2);
        let tris: Vec<[usize; 3]> = (0..m.face_count())
            .flat_map(|f| m.fan_triangles(f))
            .map(|t| t.map(|c| m.corner_vertex(c)))
            .collect();
        let pos = m.positions();
        let interior: Vec<bool> = (0..m.vertex_count()).map(|v| v < 1 + 5 * 16).collect();
        let a = abf_angles(&tris, pos, &interior).expect("converges");
        for t in &a {
            assert!((t.iter().sum::<f64>() - PI).abs() < 1e-8);
            assert!(t.iter().all(|&x| x > 0.0));
        }
        let mut sum = vec![0.0; m.vertex_count()];
        for (t, ang) in tris.iter().zip(&a) {
            for k in 0..3 {
                sum[t[k]] += ang[k];
            }
        }
        for v in 0..m.vertex_count() {
            if interior[v] {
                assert!((sum[v] - 2.0 * PI).abs() < 1e-8, "{}", sum[v]);
            }
        }
    }

    #[test]
    fn abf_beats_lscm_on_a_curved_cap() {
        // Hemisphere: Gaussian curvature 2π to flatten.
        let m = cap(10, 30, PI / 2.0);
        let run = |method| {
            let mut uv = layer(&m);
            let isl = Islands::compute(&m, &uv);
            let r = unfold(&m, &mut uv, &isl, &[0], &[], method);
            assert_eq!(r.unfolded, 1);
            assert!(r.abf_fallback.is_empty());
            analyze(&m, &uv, &isl, 1024)
        };
        let lscm = run(UnfoldMethod::Lscm);
        let abf = run(UnfoldMethod::Abf);
        assert_eq!(abf.flipped_triangles + abf.flipped_islands, 0);
        assert!(
            abf.mean_angle_distortion < lscm.mean_angle_distortion,
            "abf {} vs lscm {}",
            abf.mean_angle_distortion,
            lscm.mean_angle_distortion
        );
        // ABF spreads the angle error: the worst triangle improves clearly.
        assert!(
            abf.max_angle_distortion < 0.95 * lscm.max_angle_distortion,
            "abf {} vs lscm {}",
            abf.max_angle_distortion,
            lscm.max_angle_distortion
        );
        // Area and center are kept like LSCM: no previous layout → 3D area.
        assert!((abf.uv_area - abf.area_3d).abs() / abf.area_3d < 1e-6);
    }

    #[test]
    fn abf_respects_pins_and_is_exact_on_flat_meshes() {
        let (m, mut uv) = crate::seams::tests::grid(4);
        uv.uvs[0] = DVec2::new(0.5, 0.5);
        uv.uvs[4] = DVec2::new(0.5, 1.5);
        let mut pins = vec![false; m.vertex_count()];
        pins[0] = true;
        pins[4] = true;
        let isl = Islands::compute(&m, &uv);
        let r = unfold(&m, &mut uv, &isl, &[0], &pins, UnfoldMethod::Abf);
        assert_eq!((r.unfolded, r.abf_fallback.len()), (1, 0));
        assert_eq!(uv.uvs[0], DVec2::new(0.5, 0.5));
        assert_eq!(uv.uvs[4], DVec2::new(0.5, 1.5));
        assert!((uv.uvs[24] - DVec2::new(-0.5, 1.5)).length() < 1e-6);
        let a = analyze(&m, &uv, &isl, 1024);
        assert!(a.max_angle_distortion < 1.0 + 1e-6);
    }
}
