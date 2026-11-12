//! Flip-free optimization: SLIM (Rabinovich et al. 2017, "Scalable Locally
//! Injective Mappings") on the symmetric Dirichlet energy
//! Σ_t A_t (‖J_t‖² + ‖J_t⁻¹‖²), which is infinite for flipped triangles.
//!
//! - Start: the current UVs if no triangle is flipped or degenerate, else a
//!   Tutte embedding (boundary on a circle by arc length, interior vertices
//!   at the average of their neighbors), which is flip-free for disks.
//! - Each iteration solves the reweighted proxy Σ A ‖W (J − R)‖² (W and R
//!   from the SVD of J) as one sparse system for u and v, then moves towards
//!   it with a step capped below the first triangle inversion and halved
//!   until the energy decreases. No triangle ever flips.
//!
//! Like ARAP: two or more user pins at distinct UVs are hard constraints
//! (they need a flip-free start); otherwise the island keeps its UV area and
//! center, and a single pin keeps its UV.

use std::collections::HashMap;

use faer::linalg::solvers::Solve;
use faer::sparse::{SparseColMat, Triplet};
use faer::{Mat, Side};
use glam::{DVec2, DVec3};
use rayon::prelude::*;
use uvcraft_core::{Cancelled, Islands, Mesh, Progress, UvLayer};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SlimReport {
    pub optimized: usize,
    /// Islands that are not disks (cut them first), or pinned islands that
    /// start with flipped triangles.
    pub failed: Vec<usize>,
}

/// A triangle with a usable 3D rest shape.
struct Tri {
    v: [usize; 3],
    area: f64,
    /// Gradients of the barycentric coordinates in the rest frame.
    g: [DVec2; 3],
}

type IslandResult = Option<Vec<(u32, DVec2)>>;

#[derive(Clone, Copy)]
enum Objective {
    SymmetricDirichlet,
    /// 0 = conformal (angle), 1 = symmetric area distortion.
    AngleArea(f64),
}

#[derive(Clone, Copy)]
struct SolveMode {
    strict_start: bool,
    objective: Objective,
}

pub fn optimize_slim(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    pinned: &[bool],
    iterations: usize,
) -> SlimReport {
    optimize_slim_with_progress(
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

pub fn optimize_slim_with_progress(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    pinned: &[bool],
    iterations: usize,
    progress: &Progress,
) -> Result<SlimReport, Cancelled> {
    optimize_impl_with_progress(
        mesh,
        uv,
        islands,
        targets,
        pinned,
        iterations,
        SolveMode {
            strict_start: false,
            objective: Objective::SymmetricDirichlet,
        },
        progress,
    )
}

/// Refines existing UVs without replacing an invalid start with a Tutte
/// embedding. Islands with flipped or collapsed UV triangles are skipped.
pub fn refine_existing(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    pinned: &[bool],
    iterations: usize,
) -> SlimReport {
    refine_existing_with_progress(
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

pub fn refine_existing_with_progress(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    pinned: &[bool],
    iterations: usize,
    progress: &Progress,
) -> Result<SlimReport, Cancelled> {
    optimize_impl_with_progress(
        mesh,
        uv,
        islands,
        targets,
        pinned,
        iterations,
        SolveMode {
            strict_start: true,
            objective: Objective::SymmetricDirichlet,
        },
        progress,
    )
}

/// Minimizes a weighted mix of conformal (angle) distortion and symmetric
/// area distortion. `area_weight` is in 0..=1: 0 favors equal singular
/// values, 1 favors unit area ratio. The solve is flip-free and honors pins.
pub fn optimize_angle_area(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    pinned: &[bool],
    iterations: usize,
    area_weight: f64,
) -> SlimReport {
    optimize_angle_area_with_progress(
        mesh,
        uv,
        islands,
        targets,
        pinned,
        iterations,
        area_weight,
        &Progress::default(),
    )
    .expect("default progress cannot be cancelled")
}

#[allow(clippy::too_many_arguments)] // Existing optimize arguments plus progress context.
pub fn optimize_angle_area_with_progress(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    pinned: &[bool],
    iterations: usize,
    area_weight: f64,
    progress: &Progress,
) -> Result<SlimReport, Cancelled> {
    progress.check()?;
    if !area_weight.is_finite() || !(0.0..=1.0).contains(&area_weight) {
        return Ok(SlimReport {
            failed: targets.to_vec(),
            ..SlimReport::default()
        });
    }
    optimize_impl_with_progress(
        mesh,
        uv,
        islands,
        targets,
        pinned,
        iterations,
        SolveMode {
            strict_start: false,
            objective: Objective::AngleArea(area_weight),
        },
        progress,
    )
}

#[allow(clippy::too_many_arguments)] // Shared solver options and progress context.
fn optimize_impl_with_progress(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    pinned: &[bool],
    iterations: usize,
    mode: SolveMode,
    progress: &Progress,
) -> Result<SlimReport, Cancelled> {
    progress.check()?;
    let snapshot: &UvLayer = uv;
    let results: Vec<(usize, IslandResult)> = targets
        .par_iter()
        .map(|&i| {
            progress.check()?;
            let result = (
                i,
                slim_island(
                    mesh,
                    snapshot,
                    islands.faces(i),
                    pinned,
                    iterations,
                    mode.strict_start,
                    mode.objective,
                ),
            );
            progress.step("optimize_slim", targets.len() as u64)?;
            Ok(result)
        })
        .collect::<Result<Vec<_>, Cancelled>>()?;
    progress.check()?;
    let mut report = SlimReport::default();
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

/// Jacobian of the 3D → UV map of a triangle (rows: u, v).
fn jacobian(t: &Tri, u: &[DVec2]) -> [[f64; 2]; 2] {
    let mut j = [[0.0; 2]; 2];
    for k in 0..3 {
        let p = u[t.v[k]];
        j[0][0] += p.x * t.g[k].x;
        j[0][1] += p.x * t.g[k].y;
        j[1][0] += p.y * t.g[k].x;
        j[1][1] += p.y * t.g[k].y;
    }
    j
}

fn det(j: &[[f64; 2]; 2]) -> f64 {
    j[0][0] * j[1][1] - j[0][1] * j[1][0]
}

/// Symmetric Dirichlet energy; infinite if any triangle is flipped.
fn energy(tris: &[Tri], u: &[DVec2]) -> f64 {
    tris.iter()
        .map(|t| {
            let j = jacobian(t, u);
            let d = det(&j);
            if d <= 0.0 {
                return f64::INFINITY;
            }
            let f = j[0][0].powi(2) + j[0][1].powi(2) + j[1][0].powi(2) + j[1][1].powi(2);
            t.area * (f + f / (d * d))
        })
        .sum()
}

fn objective_energy(tris: &[Tri], u: &[DVec2], objective: Objective) -> f64 {
    match objective {
        Objective::SymmetricDirichlet => energy(tris, u),
        Objective::AngleArea(w) => tris
            .iter()
            .map(|t| {
                let j = jacobian(t, u);
                let d = det(&j);
                if d <= 0.0 {
                    return f64::INFINITY;
                }
                let f = j.iter().flatten().map(|x| x * x).sum::<f64>();
                t.area * ((1.0 - w) * f / d + w * (d + 1.0 / d))
            })
            .sum(),
    }
}

/// Diagonally preconditioned steepest descent for the angle/area objective.
fn mixed_direction(tris: &[Tri], u: &[DVec2], fixed: &[bool], w: f64) -> Option<Vec<DVec2>> {
    let mut grad = vec![DVec2::ZERO; u.len()];
    let mut diag = vec![0.0; u.len()];
    for t in tris {
        let j = jacobian(t, u);
        let d = det(&j);
        if d <= 0.0 {
            return None;
        }
        let f = j.iter().flatten().map(|x| x * x).sum::<f64>();
        let co = [[j[1][1], -j[1][0]], [-j[0][1], j[0][0]]];
        let mut dj = [[0.0; 2]; 2];
        for r in 0..2 {
            for c in 0..2 {
                dj[r][c] = (1.0 - w) * (2.0 * j[r][c] / d - f * co[r][c] / (d * d))
                    + w * (1.0 - 1.0 / (d * d)) * co[r][c];
            }
        }
        for k in 0..3 {
            let g = t.g[k];
            grad[t.v[k]] += t.area
                * DVec2::new(
                    dj[0][0] * g.x + dj[0][1] * g.y,
                    dj[1][0] * g.x + dj[1][1] * g.y,
                );
            diag[t.v[k]] += t.area * g.length_squared();
        }
    }
    for k in 0..u.len() {
        if fixed[k] {
            grad[k] = DVec2::ZERO;
        } else {
            grad[k] = -grad[k] / diag[k].max(1e-12);
        }
    }
    grad.iter().all(|p| p.is_finite()).then_some(grad)
}

fn signed_area(v: [usize; 3], u: &[DVec2]) -> f64 {
    let [a, b, c] = v.map(|k| u[k]);
    0.5 * (b - a).perp_dot(c - a)
}

/// Largest step along `d` before some triangle's signed area reaches 0.
fn max_step(tris: &[Tri], u: &[DVec2], d: &[DVec2]) -> f64 {
    let mut best = f64::INFINITY;
    for t in tris {
        let [a, b, c] = t.v;
        let (e1, e2) = (u[b] - u[a], u[c] - u[a]);
        let (f1, f2) = (d[b] - d[a], d[c] - d[a]);
        // area(α) ∝ qa α² + qb α + qc, with qc > 0.
        let qa = f1.perp_dot(f2);
        let qb = e1.perp_dot(f2) + f1.perp_dot(e2);
        let qc = e1.perp_dot(e2);
        let root = if qa.abs() < 1e-300 {
            if qb < 0.0 { -qc / qb } else { f64::INFINITY }
        } else {
            let disc = qb * qb - 4.0 * qa * qc;
            if disc < 0.0 {
                f64::INFINITY
            } else {
                let s = disc.sqrt();
                [(-qb - s) / (2.0 * qa), (-qb + s) / (2.0 * qa)]
                    .into_iter()
                    .filter(|&r| r > 0.0)
                    .fold(f64::INFINITY, f64::min)
            }
        };
        best = best.min(root);
    }
    best
}

/// The island's single boundary cycle (local indices, in half-edge order:
/// the island is on its left), or `None` if it is not a disk-like boundary.
fn boundary_cycle(all: &[[usize; 3]]) -> Option<Vec<usize>> {
    let mut half: HashMap<(usize, usize), u32> = HashMap::new();
    for t in all {
        for i in 0..3 {
            *half.entry((t[i], t[(i + 1) % 3])).or_default() += 1;
        }
    }
    let mut next: HashMap<usize, usize> = HashMap::new();
    for &(a, b) in half.keys() {
        if !half.contains_key(&(b, a)) && next.insert(a, b).is_some() {
            return None; // pinched boundary vertex
        }
    }
    let start = *next.keys().min()?;
    let mut cycle = vec![start];
    let mut cur = next[&start];
    while cur != start {
        cycle.push(cur);
        cur = *next.get(&cur)?;
        if cycle.len() > next.len() {
            return None;
        }
    }
    (cycle.len() == next.len()).then_some(cycle)
}

/// Tutte embedding: boundary on the unit circle (by 3D arc length, counter-
/// clockwise), interior at the average of its neighbors.
fn tutte(all: &[[usize; 3]], pos3: &[DVec3]) -> Option<Vec<DVec2>> {
    let n = pos3.len();
    let cycle = boundary_cycle(all)?;
    let mut u = vec![DVec2::ZERO; n];
    let mut on_boundary = vec![false; n];
    let lengths: Vec<f64> = (0..cycle.len())
        .map(|i| {
            pos3[cycle[i]]
                .distance(pos3[cycle[(i + 1) % cycle.len()]])
                .max(1e-12)
        })
        .collect();
    let total: f64 = lengths.iter().sum();
    let mut acc = 0.0;
    for (i, &v) in cycle.iter().enumerate() {
        u[v] = DVec2::from_angle(std::f64::consts::TAU * acc / total);
        on_boundary[v] = true;
        acc += lengths[i];
    }
    let mut index = vec![usize::MAX; n];
    let mut m = 0;
    for v in 0..n {
        if !on_boundary[v] {
            index[v] = m;
            m += 1;
        }
    }
    if m == 0 {
        return Some(u);
    }
    let mut triplets = Vec::new();
    let mut rhs = Mat::<f64>::zeros(m, 2);
    for t in all {
        for i in 0..3 {
            let (a, b) = (t[i], t[(i + 1) % 3]);
            // Symmetric unit weight on edge a–b (counted once per triangle side).
            for (p, q) in [(a, b), (b, a)] {
                let ip = index[p];
                if ip == usize::MAX {
                    continue;
                }
                triplets.push(Triplet::new(ip, ip, 1.0));
                if index[q] == usize::MAX {
                    rhs[(ip, 0)] += u[q].x;
                    rhs[(ip, 1)] += u[q].y;
                } else if ip > index[q] {
                    triplets.push(Triplet::new(ip, index[q], -1.0));
                }
            }
        }
    }
    let matrix = SparseColMat::<usize, f64>::try_new_from_triplets(m, m, &triplets).ok()?;
    let x = matrix.sp_cholesky(Side::Lower).ok()?.solve(&rhs);
    for v in 0..n {
        if index[v] != usize::MAX {
            u[v] = DVec2::new(x[(index[v], 0)], x[(index[v], 1)]);
        }
    }
    u.iter().all(|p| p.is_finite()).then_some(u)
}

/// One SLIM step target: minimizer of Σ A ‖W (J − R)‖² with `fixed`
/// vertices held. Returns the new positions of every vertex.
fn proxy_solve(
    tris: &[Tri],
    u: &[DVec2],
    free_index: &[usize],
    nfree: usize,
) -> Option<Vec<DVec2>> {
    let mut triplets: Vec<Triplet<usize, usize, f64>> = Vec::new();
    let mut rhs = vec![0.0; 2 * nfree];
    for t in tris {
        let j = jacobian(t, u);
        // Closed-form 2×2 SVD: J = Rot(φ) diag(σ1, σ2) Rot(θ).
        let e = (j[0][0] + j[1][1]) / 2.0;
        let f = (j[0][0] - j[1][1]) / 2.0;
        let g = (j[1][0] + j[0][1]) / 2.0;
        let h = (j[1][0] - j[0][1]) / 2.0;
        let (q, r) = ((e * e + h * h).sqrt(), (f * f + g * g).sqrt());
        let (s1, s2) = (q + r, q - r);
        let (a1, a2) = (g.atan2(f), h.atan2(e));
        let phi = (a2 + a1) / 2.0;
        // Closest rotation and SLIM weights for the symmetric Dirichlet energy.
        let (rs, rc) = a2.sin_cos();
        let rot = [[rc, -rs], [rs, rc]];
        let weight = |s: f64| -> f64 {
            let s = s.max(1e-8);
            if (s - 1.0).abs() < 1e-8 {
                2.0
            } else {
                ((s - s.powi(-3)) / (s - 1.0)).max(1e-8).sqrt()
            }
        };
        let (w1, w2) = (weight(s1), weight(s2));
        let (ps, pc) = phi.sin_cos();
        // W = U diag(w1, w2) Uᵀ with U = Rot(φ).
        let w = [
            [pc * pc * w1 + ps * ps * w2, pc * ps * (w1 - w2)],
            [pc * ps * (w1 - w2), ps * ps * w1 + pc * pc * w2],
        ];
        let wr = [
            [
                w[0][0] * rot[0][0] + w[0][1] * rot[1][0],
                w[0][0] * rot[0][1] + w[0][1] * rot[1][1],
            ],
            [
                w[1][0] * rot[0][0] + w[1][1] * rot[1][0],
                w[1][0] * rot[0][1] + w[1][1] * rot[1][1],
            ],
        ];
        let sa = t.area.sqrt();
        // Residual (a, b): Σ_{k, r} sa W[a][r] g_k[b] u_k[r] − sa (W R)[a][b].
        #[allow(clippy::needless_range_loop)] // index form mirrors the formula
        for a in 0..2 {
            for b in 0..2 {
                let mut row: [(usize, f64); 6] = [(0, 0.0); 6];
                for k in 0..3 {
                    for r in 0..2 {
                        row[k * 2 + r] = (2 * t.v[k] + r, sa * w[a][r] * t.g[k][b]);
                    }
                }
                let target = sa * wr[a][b];
                for &(va, ca) in &row {
                    let fa = free_index[va];
                    if fa == usize::MAX {
                        continue;
                    }
                    rhs[fa] += ca * target;
                    for &(vb, cb) in &row {
                        let fb = free_index[vb];
                        if fb == usize::MAX {
                            let known = if vb % 2 == 0 {
                                u[vb / 2].x
                            } else {
                                u[vb / 2].y
                            };
                            rhs[fa] -= ca * cb * known;
                        } else if fa >= fb {
                            triplets.push(Triplet::new(fa, fb, ca * cb));
                        }
                    }
                }
            }
        }
    }
    let matrix =
        SparseColMat::<usize, f64>::try_new_from_triplets(2 * nfree, 2 * nfree, &triplets).ok()?;
    let llt = matrix.sp_cholesky(Side::Lower).ok()?;
    let x = llt.solve(&Mat::<f64>::from_fn(2 * nfree, 1, |i, _| rhs[i]));
    let out: Vec<DVec2> = (0..u.len())
        .map(|k| {
            let get = |s: usize, old: f64| {
                let f = free_index[s];
                if f == usize::MAX { old } else { x[(f, 0)] }
            };
            DVec2::new(get(2 * k, u[k].x), get(2 * k + 1, u[k].y))
        })
        .collect();
    out.iter().all(|p| p.is_finite()).then_some(out)
}

fn slim_island(
    mesh: &Mesh,
    uv: &UvLayer,
    faces: &[u32],
    user_pins: &[bool],
    iterations: usize,
    strict_start: bool,
    objective: Objective,
) -> IslandResult {
    let mut local: HashMap<u32, usize> = HashMap::new();
    let mut ids: Vec<u32> = Vec::new();
    let mut pos3: Vec<DVec3> = Vec::new();
    let mut is_pinned: Vec<bool> = Vec::new();
    let mut all: Vec<[usize; 3]> = Vec::new();
    let mut tris: Vec<Tri> = Vec::new();
    for &f in faces {
        for t in mesh.fan_triangles(f as usize) {
            let v = t.map(|c| {
                let k = uv.corner_uv[c];
                *local.entry(k).or_insert_with(|| {
                    ids.push(k);
                    pos3.push(mesh.corner_position(c));
                    is_pinned.push(
                        user_pins
                            .get(mesh.corner_vertex(c))
                            .copied()
                            .unwrap_or(false),
                    );
                    ids.len() - 1
                })
            });
            all.push(v);
            let [p0, p1, p2] = t.map(|c| mesh.corner_position(c));
            let (e1, e2) = (p1 - p0, p2 - p0);
            let n = e1.cross(e2);
            let area = 0.5 * n.length();
            if e1.length() == 0.0 || area <= 1e-12 * e1.length_squared().max(e2.length_squared()) {
                continue;
            }
            let ax = e1 / e1.length();
            let ay = n.cross(e1).normalize();
            let x = [
                DVec2::ZERO,
                DVec2::new(e1.length(), 0.0),
                DVec2::new(e2.dot(ax), e2.dot(ay)),
            ];
            let g = [0, 1, 2].map(|k| {
                let e = x[(k + 2) % 3] - x[(k + 1) % 3];
                DVec2::new(-e.y, e.x) / (2.0 * area)
            });
            tris.push(Tri { v, area, g });
        }
    }
    let n = ids.len();
    if tris.is_empty() {
        return None;
    }
    let area_3d: f64 = tris.iter().map(|t| t.area).sum();
    let start: Vec<DVec2> = ids.iter().map(|&k| uv.uvs[k as usize]).collect();
    let uv_area: f64 = all.iter().map(|&v| signed_area(v, &start)).sum();
    let mirror = uv_area < 0.0;
    let mut u: Vec<DVec2> = start
        .iter()
        .map(|&p| if mirror { DVec2::new(-p.x, p.y) } else { p })
        .collect();

    let user: Vec<usize> = (0..n).filter(|&k| is_pinned[k]).collect();
    let (lo, hi) = user
        .iter()
        .fold((DVec2::INFINITY, DVec2::NEG_INFINITY), |(lo, hi), &k| {
            (lo.min(u[k]), hi.max(u[k]))
        });
    let hard = user.len() >= 2 && (hi - lo).max_element() > 1e-12;
    let flip_free = tris.iter().all(|t| signed_area(t.v, &u) > 0.0);
    if !flip_free {
        if hard || strict_start {
            return None;
        }
        u = tutte(&all, &pos3)?;
    }
    if !hard {
        // Start at 3D scale: the energy's optimum is isometric.
        let a: f64 = tris.iter().map(|t| signed_area(t.v, &u)).sum();
        let s = (area_3d / a.max(1e-300)).sqrt();
        u.iter_mut().for_each(|p| *p *= s);
    }

    let mut fixed = vec![false; n];
    if hard {
        user.iter().for_each(|&k| fixed[k] = true);
    } else {
        fixed[tris[0].v[0]] = true;
    }
    let mut used = vec![false; n];
    tris.iter().flat_map(|t| t.v).for_each(|k| used[k] = true);
    let mut free_index = vec![usize::MAX; 2 * n];
    let mut nfree = 0;
    for k in 0..n {
        if used[k] && !fixed[k] {
            free_index[2 * k] = 2 * nfree;
            free_index[2 * k + 1] = 2 * nfree + 1;
            nfree += 1;
        }
    }
    if nfree == 0 {
        return None;
    }

    let mut e0 = objective_energy(&tris, &u, objective);
    if !e0.is_finite() {
        return None;
    }
    for _ in 0..iterations {
        let d: Vec<DVec2> = match objective {
            Objective::SymmetricDirichlet => {
                let Some(target) = proxy_solve(&tris, &u, &free_index, nfree) else {
                    break;
                };
                target.iter().zip(&u).map(|(t, p)| *t - *p).collect()
            }
            Objective::AngleArea(w) => {
                let Some(direction) = mixed_direction(&tris, &u, &fixed, w) else {
                    break;
                };
                direction
            }
        };
        let mut alpha = (0.9 * max_step(&tris, &u, &d)).min(1.0);
        let mut accepted = None;
        while alpha > 1e-12 {
            let trial: Vec<DVec2> = u.iter().zip(&d).map(|(p, d)| *p + *d * alpha).collect();
            let e = objective_energy(&tris, &trial, objective);
            if e < e0 {
                accepted = Some((trial, e));
                break;
            }
            alpha *= 0.5;
        }
        let Some((next, e)) = accepted else { break };
        let converged = e0 - e < 1e-8 * e0;
        u = next;
        e0 = e;
        if converged {
            break;
        }
    }

    if mirror {
        u.iter_mut().for_each(|p| p.x = -p.x);
    }
    if !hard {
        let area: f64 = all.iter().map(|&v| signed_area(v, &u)).sum::<f64>().abs();
        let target_area = if uv_area.abs() > 1e-18 {
            uv_area.abs()
        } else {
            area_3d
        };
        let scale = (target_area / area.max(1e-300)).sqrt();
        let bbox = |pts: &[DVec2]| {
            pts.iter()
                .fold((DVec2::INFINITY, DVec2::NEG_INFINITY), |(lo, hi), &p| {
                    (lo.min(p), hi.max(p))
                })
        };
        let (lo, hi) = bbox(&start);
        let center = if uv_area.abs() > 1e-18 {
            (lo + hi) / 2.0
        } else {
            DVec2::ZERO
        };
        let (nlo, nhi) = bbox(&u);
        let mid = (nlo + nhi) / 2.0;
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
    fn cancel_slim_discards_results() {
        let (mesh, mut uv) = crate::seams::tests::grid(4);
        let islands = Islands::compute(&mesh, &uv);
        let before = uv.clone();
        let token = CancelToken::default();
        let captured = token.clone();
        let progress = Progress::new(token, move |_| captured.cancel());
        assert_eq!(
            optimize_slim_with_progress(&mesh, &mut uv, &islands, &[0], &[], 5, &progress),
            Err(Cancelled)
        );
        assert_eq!(uv, before);
    }

    #[test]
    fn angle_area_objective_extremes_descend_and_hold_pins() {
        let t = Tri {
            v: [0, 1, 2],
            area: 0.5,
            g: [DVec2::new(-1.0, -1.0), DVec2::X, DVec2::Y],
        };
        let u = [DVec2::ZERO, DVec2::new(2.0, 0.0), DVec2::Y];
        let conformal = [DVec2::ZERO, DVec2::new(2.0, 0.0), DVec2::new(0.0, 2.0)];
        let area_preserving = [DVec2::ZERO, DVec2::new(2.0, 0.0), DVec2::new(0.0, 0.5)];
        assert!(
            objective_energy(
                std::slice::from_ref(&t),
                &conformal,
                Objective::AngleArea(0.0)
            ) < objective_energy(
                std::slice::from_ref(&t),
                &area_preserving,
                Objective::AngleArea(0.0)
            )
        );
        assert!(
            objective_energy(
                std::slice::from_ref(&t),
                &area_preserving,
                Objective::AngleArea(1.0)
            ) < objective_energy(
                std::slice::from_ref(&t),
                &conformal,
                Objective::AngleArea(1.0)
            )
        );
        for w in [0.0, 1.0] {
            let direction =
                mixed_direction(std::slice::from_ref(&t), &u, &[true, false, false], w).unwrap();
            assert_eq!(direction[0], DVec2::ZERO);
            let next = u
                .iter()
                .zip(&direction)
                .map(|(p, d)| *p + *d * 0.01)
                .collect::<Vec<_>>();
            assert!(
                objective_energy(std::slice::from_ref(&t), &next, Objective::AngleArea(w))
                    < objective_energy(std::slice::from_ref(&t), &u, Objective::AngleArea(w))
            );
        }

        let positions = vec![DVec3::ZERO, DVec3::X, DVec3::new(1.0, 1.0, 0.0), DVec3::Y];
        let mesh = Mesh::new(positions, &[[0, 1, 2, 3]]).unwrap();
        let baseline = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 3],
            uvs: vec![DVec2::ZERO, DVec2::X, DVec2::new(1.3, 1.0), DVec2::Y],
        };
        let islands = Islands::compute(&mesh, &baseline);
        for w in [0.0, 0.5, 1.0] {
            let mut uv = baseline.clone();
            let report = optimize_angle_area(
                &mesh,
                &mut uv,
                &islands,
                &[0],
                &[true, true, false, false],
                30,
                w,
            );
            assert_eq!(report.optimized, 1, "{report:?}");
            assert_eq!(uv.uvs[0], baseline.uvs[0]);
            assert_eq!(uv.uvs[1], baseline.uvs[1]);
            assert_eq!(analyze(&mesh, &uv, &islands, 1024).flipped_triangles, 0);
        }
    }

    #[test]
    fn slim_from_scrambled_uvs_is_flip_free_and_low_distortion() {
        let (m, mut uv) = torus(32, 16);
        cut_to_disk(&m, &mut uv, &[0]);
        let isl = Islands::compute(&m, &uv);
        // Garbage start: everything flipped/collapsed → Tutte start.
        for (i, p) in uv.uvs.iter_mut().enumerate() {
            *p = DVec2::new((i * 7919 % 101) as f64, (i * 104_729 % 97) as f64) * 0.01;
        }
        let r = optimize_slim(&m, &mut uv, &isl, &[0], &[], 50);
        assert_eq!(r.optimized, 1, "{r:?}");
        let a = analyze(&m, &uv, &isl, 1024);
        assert_eq!(a.flipped_triangles, 0);
        assert!(a.mean_area_distortion < 1.5, "{}", a.mean_area_distortion);
        assert!(a.mean_angle_distortion < 1.3, "{}", a.mean_angle_distortion);
    }

    #[test]
    fn slim_improves_lscm_and_keeps_area() {
        let (m, mut uv) = torus(32, 16);
        cut_to_disk(&m, &mut uv, &[0]);
        let isl = Islands::compute(&m, &uv);
        unfold_lscm(&m, &mut uv, &isl, &[0], &[]);
        let before = analyze(&m, &uv, &isl, 1024);
        optimize_slim(&m, &mut uv, &isl, &[0], &[], 30);
        let after = analyze(&m, &uv, &isl, 1024);
        assert_eq!(after.flipped_triangles, 0);
        assert!(after.mean_area_distortion < before.mean_area_distortion * 0.7);
        assert!((after.uv_area - before.uv_area).abs() < 1e-9 * before.uv_area);
    }

    #[test]
    fn non_disks_are_refused_when_a_start_is_needed() {
        let (m, mut uv) = torus(12, 8); // closed: no boundary
        let isl = Islands::compute(&m, &uv);
        let r = optimize_slim(&m, &mut uv, &isl, &[0], &[], 5);
        assert_eq!(r.failed, vec![0]);
    }
}
