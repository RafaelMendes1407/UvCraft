//! Mosaic: automatic charts from planarity and curvature.
//!
//! 1. **Variational Shape Approximation** (Cohen-Steiner, Alliez, Desbrun
//!    2004) with the L2,1 metric: each chart has a proxy normal `N`, a face
//!    costs `area · |n − N|²`. Lloyd iterations alternate a priority-queue
//!    flood from one seed face per chart with refitting the proxies. Charts
//!    are added one seed at a time at the worst face of the worst charts,
//!    until the requested count is reached and (automatic mode) every chart's
//!    mean error is at most `max_error`.
//! 2. **Developable merge** (in the spirit of D-Charts): adjacent charts are
//!    merged while the union still fits a cylinder (normals perpendicular to
//!    one axis, error `Σ area · (n·a)²`, the smallest eigenvalue of the normal
//!    covariance) within `max_error` and their shared border is smooth (no
//!    dihedral angle above [`SMOOTH_DEG`]). So the strips of a cylinder side
//!    become one chart, but cube faces stay apart. With a chart count, the
//!    cheapest merges continue until the count is reached.
//! 3. Boundary smoothing: single-face teeth are moved to the chart that
//!    surrounds them, when that chart's proxy fits them as well.
//!
//! Charts only grow across interior edges, so existing seams and borders are
//! always kept and every chart stays inside its island. Everything is
//! deterministic: heaps and ties are ordered by face and chart index.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use glam::{DMat3, DVec3};
use uvcraft_core::{Cancelled, EdgeKind, Islands, Mesh, Progress, UvLayer};

use crate::analysis;
use crate::seams;

/// Default `max_error`: mean of `|n − N|²` per chart, `≈ θ²` for a normal
/// deviation `θ` in radians (0.1 ≈ 18° RMS).
pub const DEFAULT_MAX_ERROR: f64 = 0.1;
/// Charts merged automatically never meet at an edge sharper than this.
pub const SMOOTH_DEG: f64 = 40.0;
/// Upper bound on charts created automatically (before merging).
const MAX_AUTO_CHARTS: usize = 512;
const NONE: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MosaicParams {
    /// Target chart count over all target islands (at least one per island).
    /// `None`: automatic, from `max_error`.
    pub charts: Option<usize>,
    /// Maximum mean normal error per chart (see [`DEFAULT_MAX_ERROR`]).
    pub max_error: f64,
    /// Lloyd iterations after each seed insertion round (and at the end).
    pub iterations: usize,
}

impl Default for MosaicParams {
    fn default() -> Self {
        Self {
            charts: None,
            max_error: DEFAULT_MAX_ERROR,
            iterations: 10,
        }
    }
}

/// A partition of the target faces into charts.
#[derive(Debug, Clone, PartialEq)]
pub struct Segmentation {
    /// Chart per face; `u32::MAX` for faces outside the targets.
    pub chart_of_face: Vec<u32>,
    pub chart_count: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MosaicReport {
    pub charts: usize,
    /// The charts as islands of the cut layer (`Islands::compute` numbering).
    pub islands: Vec<usize>,
    /// Every edge cut: chart borders, then the cuts that make charts disks.
    pub cut_edges: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BudgetedMosaicReport {
    pub mosaic: MosaicReport,
    /// 3D length of newly added seam edges.
    pub added_seam_length: f64,
    /// Normal-error tolerance used after relaxing the initial value if needed.
    pub max_error_used: f64,
}

#[derive(PartialEq)]
struct OrdF64(f64);
impl Eq for OrdF64 {}
impl PartialOrd for OrdF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrdF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Per-face data and adjacency across interior edges.
struct Faces {
    normal: Vec<DVec3>,
    area: Vec<f64>,
    /// (neighbor face, edge) across interior edges, target faces only.
    adj: Vec<Vec<(u32, u32)>>,
    /// Target faces, ascending.
    list: Vec<u32>,
}

impl Faces {
    fn new(mesh: &Mesh, uv: &UvLayer, list: Vec<u32>) -> Self {
        let n = mesh.face_count();
        let mut normal = vec![DVec3::ZERO; n];
        let mut area = vec![0.0; n];
        let mut adj = vec![Vec::new(); n];
        for &f in &list {
            let f = f as usize;
            let an = mesh.face_area_normal(f);
            area[f] = an.length();
            normal[f] = an.normalize_or_zero();
            for c in mesh.face_corners(f) {
                let e = mesh.corner_edge(c);
                if uv.edge_kind(mesh, e) != EdgeKind::Interior {
                    continue;
                }
                let (c0, Some(c1)) = mesh.edge_corners(e) else {
                    continue;
                };
                let other = mesh.corner_face(if c0 == c { c1 } else { c0 });
                if other != f {
                    adj[f].push((other as u32, e as u32));
                }
            }
        }
        Faces {
            normal,
            area,
            adj,
            list,
        }
    }

    fn err(&self, f: usize, proxy: DVec3) -> f64 {
        (self.normal[f] - proxy).length_squared()
    }
}

/// Flood from the seeds, cheapest (face, chart) first.
fn flood(faces: &Faces, seeds: &[u32], proxies: &[DVec3], label: &mut [u32]) {
    for &f in &faces.list {
        label[f as usize] = NONE;
    }
    let mut heap = BinaryHeap::new();
    let push = |heap: &mut BinaryHeap<_>, f: usize, r: usize, label: &[u32]| {
        for &(g, _) in &faces.adj[f] {
            if label[g as usize] == NONE {
                let e = faces.err(g as usize, proxies[r]);
                heap.push(Reverse((OrdF64(e), g, r as u32)));
            }
        }
    };
    for (r, &s) in seeds.iter().enumerate() {
        label[s as usize] = r as u32;
    }
    for (r, &s) in seeds.iter().enumerate() {
        push(&mut heap, s as usize, r, label);
    }
    while let Some(Reverse((_, f, r))) = heap.pop() {
        if label[f as usize] != NONE {
            continue;
        }
        label[f as usize] = r;
        push(&mut heap, f as usize, r as usize, label);
    }
}

/// Area-weighted average normal of each chart.
fn fit(faces: &Faces, label: &[u32], seeds: &[u32]) -> Vec<DVec3> {
    let mut sum = vec![DVec3::ZERO; seeds.len()];
    for &f in &faces.list {
        let f = f as usize;
        sum[label[f] as usize] += faces.normal[f] * faces.area[f];
    }
    sum.iter()
        .zip(seeds)
        .map(|(s, &seed)| {
            if s.length() > 1e-12 {
                s.normalize()
            } else {
                faces.normal[seed as usize]
            }
        })
        .collect()
}

/// Lloyd iterations: flood, refit, reseed at each chart's best face.
fn lloyd(
    faces: &Faces,
    seeds: &mut [u32],
    proxies: &mut Vec<DVec3>,
    label: &mut [u32],
    iterations: usize,
) {
    for _ in 0..iterations.max(1) {
        flood(faces, seeds, proxies, label);
        *proxies = fit(faces, label, seeds);
        let mut best: Vec<(f64, u32)> = vec![(f64::INFINITY, NONE); seeds.len()];
        for &f in &faces.list {
            let r = label[f as usize] as usize;
            let e = faces.err(f as usize, proxies[r]);
            if e < best[r].0 {
                best[r] = (e, f);
            }
        }
        let new: Vec<u32> = best.iter().map(|b| b.1).collect();
        if new == seeds {
            break;
        }
        seeds.copy_from_slice(&new);
    }
    flood(faces, seeds, proxies, label);
    *proxies = fit(faces, label, seeds);
}

/// Per chart: (mean error, worst face, its error).
fn chart_errors(faces: &Faces, label: &[u32], proxies: &[DVec3]) -> Vec<(f64, u32, f64)> {
    let mut acc = vec![(0.0, 0.0, NONE, -1.0); proxies.len()];
    for &f in &faces.list {
        let r = label[f as usize] as usize;
        let e = faces.err(f as usize, proxies[r]);
        let a = &mut acc[r];
        a.0 += e * faces.area[f as usize];
        a.1 += faces.area[f as usize];
        if e > a.3 {
            (a.2, a.3) = (f, e);
        }
    }
    acc.iter()
        .map(|&(e, w, f, fe)| (if w > 0.0 { e / w } else { 0.0 }, f, fe))
        .collect()
}

/// Smallest eigenvalue of a symmetric 3×3 matrix (trigonometric method).
fn min_eigenvalue(m: DMat3) -> f64 {
    let (a, b, c) = (m.x_axis.x, m.y_axis.y, m.z_axis.z);
    let (d, e, f) = (m.y_axis.x, m.z_axis.x, m.z_axis.y);
    let p1 = d * d + e * e + f * f;
    if p1 <= 1e-30 {
        return a.min(b).min(c).max(0.0);
    }
    let q = (a + b + c) / 3.0;
    let p2 = (a - q).powi(2) + (b - q).powi(2) + (c - q).powi(2) + 2.0 * p1;
    let p = (p2 / 6.0).sqrt();
    let bm = (m - DMat3::from_diagonal(DVec3::splat(q))) * (1.0 / p);
    let r = (bm.determinant() / 2.0).clamp(-1.0, 1.0);
    let phi = r.acos() / 3.0;
    (q + 2.0 * p * (phi + 2.0 * std::f64::consts::PI / 3.0).cos()).max(0.0)
}

/// Additive statistics of a chart for the cylinder fit.
#[derive(Clone, Copy)]
struct Stats {
    area: f64,
    cov: DMat3,
}

impl Stats {
    fn add(self, o: Stats) -> Stats {
        Stats {
            area: self.area + o.area,
            cov: self.cov + o.cov,
        }
    }
    /// Mean cylinder-fit error: min over axes of mean (n·a)².
    fn error(&self) -> f64 {
        if self.area > 0.0 {
            min_eigenvalue(self.cov) / self.area
        } else {
            0.0
        }
    }
}

/// Shared border between two charts.
#[derive(Clone, Copy, Default)]
struct Border {
    length: f64,
    /// Length-weighted sum of 2(1 − cos dihedral).
    bend: f64,
    max_deg: f64,
}

/// Merges adjacent charts: automatically while eligible, then (with a target
/// count) cheapest first until the count is reached. Returns the relabeling.
fn merge(
    mesh: &Mesh,
    faces: &Faces,
    label: &[u32],
    count: usize,
    target: Option<usize>,
    max_error: f64,
) -> (Vec<u32>, usize) {
    let mut stats = vec![
        Stats {
            area: 0.0,
            cov: DMat3::ZERO,
        };
        count
    ];
    for &f in &faces.list {
        let f = f as usize;
        let n = faces.normal[f];
        let s = &mut stats[label[f] as usize];
        s.area += faces.area[f];
        s.cov += DMat3::from_cols(n * n.x, n * n.y, n * n.z) * faces.area[f];
    }
    let mut borders: BTreeMap<(u32, u32), Border> = BTreeMap::new();
    for &f in &faces.list {
        for &(g, e) in &faces.adj[f as usize] {
            let (a, b) = (label[f as usize], label[g as usize]);
            if a >= b {
                continue; // each edge once, from the lower chart's face
            }
            let deg = mesh.dihedral_angle_deg(e as usize).unwrap_or(0.0);
            let len = mesh.edge_length(e as usize);
            let br = borders.entry((a, b)).or_default();
            br.length += len;
            br.bend += len * 2.0 * (1.0 - deg.to_radians().cos());
            br.max_deg = br.max_deg.max(deg);
        }
    }

    let mut parent: Vec<u32> = (0..count as u32).collect();
    let mut alive = count;
    while target.is_none_or(|t| alive > t) {
        let forced = target.is_some();
        let mut best: Option<(f64, (u32, u32))> = None;
        for (&(a, b), br) in &borders {
            let merged = stats[a as usize].add(stats[b as usize]);
            let err = merged.error();
            let eligible = err <= max_error && br.max_deg <= SMOOTH_DEG;
            if !(eligible || forced) {
                continue;
            }
            let cost = err + br.bend / br.length.max(1e-300);
            if best.is_none_or(|(c, _)| cost < c) {
                best = Some((cost, (a, b)));
            }
        }
        let Some((_, (a, b))) = best else { break };
        // Merge b into a (a < b).
        parent[b as usize] = a;
        stats[a as usize] = stats[a as usize].add(stats[b as usize]);
        alive -= 1;
        let old = std::mem::take(&mut borders);
        for ((x, y), br) in old {
            let x = if x == b { a } else { x };
            let y = if y == b { a } else { y };
            if x == y {
                continue;
            }
            let key = (x.min(y), x.max(y));
            let e = borders.entry(key).or_default();
            e.length += br.length;
            e.bend += br.bend;
            e.max_deg = e.max_deg.max(br.max_deg);
        }
    }

    // Compact the surviving charts, in order.
    let root = |mut x: u32| {
        while parent[x as usize] != x {
            x = parent[x as usize];
        }
        x
    };
    let mut remap = vec![NONE; count];
    let mut next = 0;
    let mut out = label.to_vec();
    for r in 0..count as u32 {
        if root(r) == r {
            remap[r as usize] = next;
            next += 1;
        }
    }
    for &f in &faces.list {
        out[f as usize] = remap[root(label[f as usize]) as usize];
    }
    (out, next as usize)
}

/// Moves single-face teeth to the chart around them.
fn smooth(mesh: &Mesh, faces: &Faces, label: &mut [u32], count: usize, max_error: f64) {
    for _ in 0..3 {
        // Proxies of the current charts.
        let mut sum = vec![DVec3::ZERO; count];
        for &f in &faces.list {
            sum[label[f as usize] as usize] += faces.normal[f as usize] * faces.area[f as usize];
        }
        let proxies: Vec<DVec3> = sum.iter().map(|s| s.normalize_or_zero()).collect();
        let mut changed = false;
        for &f in &faces.list {
            let f = f as usize;
            let own = label[f];
            let mut shares: BTreeMap<u32, (f64, usize)> = BTreeMap::new();
            for &(g, e) in &faces.adj[f] {
                let s = shares.entry(label[g as usize]).or_default();
                s.0 += mesh.edge_length(e as usize);
                s.1 += 1;
            }
            let (own_len, own_n) = shares.get(&own).copied().unwrap_or_default();
            if own_n != 1 {
                continue; // removing f could split or empty its chart
            }
            let Some((&other, _)) = shares
                .iter()
                .filter(|&(&r, s)| r != own && s.0 > own_len)
                .max_by(|a, b| a.1.0.total_cmp(&b.1.0).then(b.0.cmp(a.0)))
            else {
                continue;
            };
            let fits = faces.err(f, proxies[other as usize])
                <= faces.err(f, proxies[own as usize]).max(max_error);
            if fits {
                label[f] = other;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// Segments the target islands into charts. Charts never cross existing
/// seams or borders, and each island gets at least one chart.
pub fn segment(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    targets: &[usize],
    params: &MosaicParams,
) -> Segmentation {
    segment_with_progress(mesh, uv, islands, targets, params, &Progress::default())
        .expect("default progress cannot be cancelled")
}

/// Segmentation with a checkpoint after each chart refinement pass.
pub fn segment_with_progress(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    targets: &[usize],
    params: &MosaicParams,
    progress: &Progress,
) -> Result<Segmentation, Cancelled> {
    progress.check()?;
    let mut list: Vec<u32> = targets
        .iter()
        .flat_map(|&i| islands.faces(i).iter().copied())
        .collect();
    list.sort_unstable();
    list.dedup();
    let faces = Faces::new(mesh, uv, list);
    let mut label = vec![NONE; mesh.face_count()];
    if faces.list.is_empty() {
        return Ok(Segmentation {
            chart_of_face: label,
            chart_count: 0,
        });
    }

    // One initial chart per island, seeded at its face closest to the mean normal.
    let mut targets = targets.to_vec();
    targets.sort_unstable();
    targets.dedup();
    let mut seeds: Vec<u32> = Vec::new();
    let mut proxies: Vec<DVec3> = Vec::new();
    for &i in &targets {
        let fs = islands.faces(i);
        let s: DVec3 = fs
            .iter()
            .map(|&f| faces.normal[f as usize] * faces.area[f as usize])
            .sum();
        let largest = *fs
            .iter()
            .max_by(|&&a, &&b| {
                faces.area[a as usize]
                    .total_cmp(&faces.area[b as usize])
                    .then(b.cmp(&a))
            })
            .expect("islands have faces");
        let proxy = if s.length() > 1e-9 * fs.len() as f64 {
            s.normalize()
        } else {
            faces.normal[largest as usize]
        };
        let seed = *fs
            .iter()
            .min_by(|&&a, &&b| {
                faces
                    .err(a as usize, proxy)
                    .total_cmp(&faces.err(b as usize, proxy))
                    .then(a.cmp(&b))
            })
            .expect("islands have faces");
        seeds.push(seed);
        proxies.push(proxy);
    }

    let max_error = params.max_error.max(0.0);
    let target = params.charts.map(|k| k.max(seeds.len()));
    let cap = faces
        .list
        .len()
        .min(MAX_AUTO_CHARTS.max(target.unwrap_or(0)));
    lloyd(
        &faces,
        &mut seeds,
        &mut proxies,
        &mut label,
        params.iterations,
    );
    loop {
        progress.report("mosaic_charts", seeds.len() as u64, cap as u64)?;
        let errors = chart_errors(&faces, &label, &proxies);
        // Charts to split: over the threshold, worst first.
        let mut bad: Vec<usize> = (0..seeds.len())
            .filter(|&r| errors[r].0 > max_error && errors[r].2 > 0.0)
            .collect();
        let missing = target.map_or(0, |t| t.saturating_sub(seeds.len()));
        if bad.is_empty() && missing > 0 {
            // Below the count: split the worst charts anyway.
            bad = (0..seeds.len()).filter(|&r| errors[r].2 > 0.0).collect();
            if bad.is_empty() {
                bad = (0..seeds.len()).collect();
            }
        }
        bad.sort_by(|&a, &b| errors[b].0.total_cmp(&errors[a].0).then(a.cmp(&b)));
        let room = cap.saturating_sub(seeds.len());
        let quota = if missing > 0 {
            missing.min((seeds.len() / 4).max(1))
        } else {
            (seeds.len() / 4).max(1)
        };
        let mut added = 0;
        for &r in bad.iter().take(quota.min(room)) {
            // Worst face; failing that, the chart's largest-index non-seed face.
            let f = if errors[r].2 > 0.0 && errors[r].1 != seeds[r] {
                errors[r].1
            } else {
                match faces
                    .list
                    .iter()
                    .rev()
                    .find(|&&f| label[f as usize] == r as u32 && f != seeds[r])
                {
                    Some(&f) => f,
                    None => continue,
                }
            };
            seeds.push(f);
            proxies.push(faces.normal[f as usize]);
            added += 1;
        }
        if added == 0 {
            break;
        }
        lloyd(
            &faces,
            &mut seeds,
            &mut proxies,
            &mut label,
            params.iterations,
        );
    }

    let (mut label, count) = merge(mesh, &faces, &label, seeds.len(), target, max_error);
    smooth(mesh, &faces, &mut label, count, max_error);
    progress.check()?;
    Ok(Segmentation {
        chart_of_face: label,
        chart_count: count,
    })
}

/// Interior edges between two different charts.
pub fn chart_borders(mesh: &Mesh, uv: &UvLayer, seg: &Segmentation) -> Vec<usize> {
    (0..mesh.edge_count())
        .filter(|&e| {
            if uv.edge_kind(mesh, e) != EdgeKind::Interior {
                return false;
            }
            let (c0, Some(c1)) = mesh.edge_corners(e) else {
                return false;
            };
            let (a, b) = (
                seg.chart_of_face[mesh.corner_face(c0)],
                seg.chart_of_face[mesh.corner_face(c1)],
            );
            a != NONE && b != NONE && a != b
        })
        .collect()
}

/// Segments the target islands, cuts the chart borders, then cuts every
/// chart into a disk. The UVs of the new islands are left as they were
/// (unfold them next).
pub fn mosaic(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    params: &MosaicParams,
) -> MosaicReport {
    let seg = segment(mesh, uv, islands, targets, params);
    let mut cut = chart_borders(mesh, uv, &seg);
    seams::cut_edges(mesh, uv, &cut);
    let charts = chart_islands(mesh, uv, &seg.chart_of_face);
    cut.extend(seams::cut_to_disk(mesh, uv, &charts));
    MosaicReport {
        charts: charts.len(),
        islands: chart_islands(mesh, uv, &seg.chart_of_face),
        cut_edges: cut,
    }
}

/// Mosaic with transactional cancellation and start/completion events.
pub fn mosaic_with_progress(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    params: &MosaicParams,
    progress: &Progress,
) -> Result<MosaicReport, Cancelled> {
    progress.report("mosaic", 0, 1)?;
    let mut trial = uv.clone();
    let seg = segment_with_progress(mesh, &trial, islands, targets, params, progress)?;
    let mut cut = chart_borders(mesh, &trial, &seg);
    seams::cut_edges(mesh, &mut trial, &cut);
    let charts = chart_islands(mesh, &trial, &seg.chart_of_face);
    cut.extend(seams::cut_to_disk(mesh, &mut trial, &charts));
    let report = MosaicReport {
        charts: charts.len(),
        islands: chart_islands(mesh, &trial, &seg.chart_of_face),
        cut_edges: cut,
    };
    progress.report("mosaic", 1, 1)?;
    *uv = trial;
    Ok(report)
}

/// Budgeted Mosaic with transactional cancellation.
pub fn mosaic_with_seam_budget_with_progress(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    params: &MosaicParams,
    max_seam_length: f64,
    progress: &Progress,
) -> Result<Result<BudgetedMosaicReport, f64>, Cancelled> {
    progress.report("mosaic", 0, 1)?;
    let mut trial = uv.clone();
    let report =
        mosaic_with_seam_budget(mesh, &mut trial, islands, targets, params, max_seam_length);
    progress.report("mosaic", 1, 1)?;
    if report.is_ok() {
        *uv = trial;
    }
    Ok(report)
}

/// Runs Mosaic under a budget for the total 3D length of *new* seams.
/// With automatic chart count, the normal-error tolerance is relaxed until
/// a layout fits. An explicit `charts` count is never relaxed. On failure,
/// the UV layer is untouched and the shortest attempted seam length is returned.
pub fn mosaic_with_seam_budget(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    params: &MosaicParams,
    max_seam_length: f64,
) -> Result<BudgetedMosaicReport, f64> {
    let original = uv.clone();
    let was_interior: Vec<bool> = (0..mesh.edge_count())
        .map(|e| original.edge_kind(mesh, e) == EdgeKind::Interior)
        .collect();
    let mut tolerances = vec![params.max_error];
    if params.charts.is_none() {
        let mut error = params.max_error.max(0.01);
        while error < 4.0 {
            error = (error * 2.0).min(4.0);
            if error > *tolerances.last().unwrap() {
                tolerances.push(error);
            }
        }
    }
    let mut smallest = f64::INFINITY;
    for error in tolerances {
        let mut trial = original.clone();
        let trial_params = MosaicParams {
            max_error: error,
            ..*params
        };
        let report = mosaic(mesh, &mut trial, islands, targets, &trial_params);
        let length: f64 = was_interior
            .iter()
            .enumerate()
            .filter(|&(e, &old)| old && trial.edge_kind(mesh, e) == EdgeKind::Seam)
            .map(|(e, _)| mesh.edge_length(e))
            .sum();
        smallest = smallest.min(length);
        if length <= max_seam_length + 1e-9 * max_seam_length.max(1.0) {
            *uv = trial;
            return Ok(BudgetedMosaicReport {
                mosaic: report,
                added_seam_length: length,
                max_error_used: error,
            });
        }
    }
    Err(smallest)
}

/// Worst post-unfold distortion across the requested charts: the maximum of
/// angle stretch and area stretch (both 1 for an isometric map). Degenerate
/// or locally flipped UV triangles make the result infinite.
pub fn max_chart_uv_distortion(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    charts: &[usize],
) -> f64 {
    let report = analysis::analyze(mesh, uv, islands, 1024);
    charts.iter().fold(1.0, |worst, &i| {
        let Some(chart) = report.islands.get(i) else {
            return f64::INFINITY;
        };
        if chart.degenerate_triangles > 0 || chart.flipped_triangles > 0 {
            f64::INFINITY
        } else {
            worst
                .max(chart.max_angle_distortion)
                .max(chart.max_area_distortion)
        }
    })
}

/// Current islands that contain the faces marked in `chart_of_face`.
pub fn chart_islands(mesh: &Mesh, uv: &UvLayer, chart_of_face: &[u32]) -> Vec<usize> {
    let isl = Islands::compute(mesh, uv);
    let mut out: Vec<usize> = (0..mesh.face_count())
        .filter(|&f| chart_of_face[f] != NONE)
        .map(|f| isl.island_of_face(f))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec2;
    use std::f64::consts::TAU;
    use uvcraft_core::CancelToken;

    #[test]
    fn cancelled_mosaic_keeps_uvs() {
        let mesh = cube(1);
        let mut uv = welded(&mesh);
        let islands = Islands::compute(&mesh, &uv);
        let before = uv.clone();
        let token = CancelToken::default();
        let captured = token.clone();
        let progress = Progress::new(token, move |event| {
            if event.completed == 1 {
                captured.cancel();
            }
        });
        assert_eq!(
            mosaic_with_progress(
                &mesh,
                &mut uv,
                &islands,
                &[0],
                &MosaicParams::default(),
                &progress
            ),
            Err(Cancelled)
        );
        assert_eq!(uv, before);
    }

    #[test]
    fn cancelling_during_chart_refinement_rolls_back() {
        let mesh = cube(2);
        let mut uv = welded(&mesh);
        let islands = Islands::compute(&mesh, &uv);
        let before = uv.clone();
        let token = CancelToken::default();
        let captured = token.clone();
        let saw_loop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = saw_loop.clone();
        let progress = Progress::new(token, move |event| {
            if event.phase == "mosaic_charts" {
                seen.store(true, std::sync::atomic::Ordering::SeqCst);
                captured.cancel();
            }
        });
        assert_eq!(
            mosaic_with_progress(
                &mesh,
                &mut uv,
                &islands,
                &[0],
                &MosaicParams::default(),
                &progress
            ),
            Err(Cancelled)
        );
        assert!(saw_loop.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(uv, before);
    }

    fn welded(mesh: &Mesh) -> UvLayer {
        UvLayer {
            name: "uv".into(),
            corner_uv: (0..mesh.corner_count())
                .map(|c| mesh.corner_vertex(c) as u32)
                .collect(),
            uvs: vec![DVec2::ZERO; mesh.vertex_count()],
        }
    }

    /// Cube [0,1]³ with an n×n grid of quads per side, welded, outward normals.
    fn cube(n: usize) -> Mesh {
        let mut pos: Vec<DVec3> = Vec::new();
        let mut index: BTreeMap<[i64; 3], u32> = BTreeMap::new();
        let mut vid = |p: DVec3| {
            let key = (p * n as f64).round().as_i64vec3().to_array();
            *index.entry(key).or_insert_with(|| {
                pos.push(p);
                pos.len() as u32 - 1
            })
        };
        let mut faces = Vec::new();
        for axis in 0..3 {
            for side in [0.0, 1.0] {
                let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
                for i in 0..n {
                    for j in 0..n {
                        let p = |a: usize, b: usize| {
                            let mut q = DVec3::ZERO;
                            q[axis] = side;
                            q[u] = a as f64 / n as f64;
                            q[v] = b as f64 / n as f64;
                            q
                        };
                        let mut quad = [p(i, j), p(i + 1, j), p(i + 1, j + 1), p(i, j + 1)];
                        if side == 0.0 {
                            quad.reverse();
                        }
                        faces.push(quad.map(&mut vid));
                    }
                }
            }
        }
        Mesh::new(pos, &faces).unwrap()
    }

    /// Closed cylinder: `n` segments, `rings` rows on the side, triangle-fan caps.
    fn cylinder(n: usize, rings: usize) -> Mesh {
        let mut p = Vec::new();
        for r in 0..=rings {
            for i in 0..n {
                let a = TAU * i as f64 / n as f64;
                p.push(DVec3::new(a.cos(), r as f64 * 2.0 / rings as f64, a.sin()));
            }
        }
        let bottom = p.len() as u32;
        p.push(DVec3::new(0.0, 0.0, 0.0));
        let top = p.len() as u32;
        p.push(DVec3::new(0.0, 2.0, 0.0));
        let v = |r: usize, i: usize| (r * n + i % n) as u32;
        let mut faces: Vec<Vec<u32>> = Vec::new();
        for r in 0..rings {
            for i in 0..n {
                faces.push(vec![v(r, i), v(r + 1, i), v(r + 1, i + 1), v(r, i + 1)]);
            }
        }
        for i in 0..n {
            faces.push(vec![bottom, v(0, i), v(0, i + 1)]);
            faces.push(vec![top, v(rings, i + 1), v(rings, i)]);
        }
        Mesh::new(p, &faces).unwrap()
    }

    fn sphere(n: usize, k: usize) -> Mesh {
        let mut p = vec![DVec3::Y, -DVec3::Y];
        for j in 1..k {
            let t = std::f64::consts::PI * j as f64 / k as f64;
            for i in 0..n {
                let a = TAU * i as f64 / n as f64;
                p.push(DVec3::new(t.sin() * a.cos(), t.cos(), t.sin() * a.sin()));
            }
        }
        let v = |j: usize, i: usize| (2 + (j - 1) * n + i % n) as u32;
        let mut faces: Vec<Vec<u32>> = Vec::new();
        for i in 0..n {
            faces.push(vec![0, v(1, i + 1), v(1, i)]);
            faces.push(vec![1, v(k - 1, i), v(k - 1, i + 1)]);
        }
        for j in 1..k - 1 {
            for i in 0..n {
                faces.push(vec![v(j, i), v(j, i + 1), v(j + 1, i + 1), v(j + 1, i)]);
            }
        }
        Mesh::new(p, &faces).unwrap()
    }

    fn run(mesh: &Mesh, params: MosaicParams) -> (UvLayer, Segmentation, MosaicReport) {
        let mut uv = welded(mesh);
        let isl = Islands::compute(mesh, &uv);
        let targets: Vec<usize> = (0..isl.count()).collect();
        let seg = segment(mesh, &uv, &isl, &targets, &params);
        let report = mosaic(mesh, &mut uv, &isl, &targets, &params);
        (uv, seg, report)
    }

    /// All target islands unfold (so they are disks) without flips.
    fn unfolds(mesh: &Mesh, uv: &mut UvLayer) {
        let isl = Islands::compute(mesh, uv);
        let all: Vec<usize> = (0..isl.count()).collect();
        let r = crate::unfold::unfold_lscm(mesh, uv, &isl, &all, &[]);
        assert_eq!(r.unfolded, isl.count(), "{r:?}");
        let a = crate::analysis::analyze(mesh, uv, &isl, 1024);
        assert_eq!(a.flipped_islands, 0);
    }

    #[test]
    fn cube_becomes_six_planar_charts() {
        let m = cube(3);
        for charts in [None, Some(6)] {
            let (mut uv, seg, report) = run(
                &m,
                MosaicParams {
                    charts,
                    ..Default::default()
                },
            );
            assert_eq!(seg.chart_count, 6, "{charts:?}");
            assert_eq!(report.charts, 6);
            assert_eq!(Islands::compute(&m, &uv).count(), 6);
            // Each chart is one side: all its faces share a normal.
            for f in 0..m.face_count() {
                let n = m.face_area_normal(f).normalize();
                let same = (0..m.face_count())
                    .filter(|&g| seg.chart_of_face[g] == seg.chart_of_face[f])
                    .all(|g| m.face_area_normal(g).normalize().dot(n) > 0.999);
                assert!(same);
            }
            // Exactly the 12 cube edges (3 mesh edges each) are cut.
            assert_eq!(report.cut_edges.len(), 36);
            unfolds(&m, &mut uv);
        }
    }

    #[test]
    fn seam_budget_is_exact_and_transactional() {
        let mesh = cube(1);
        let mut uv = welded(&mesh);
        let original = uv.clone();
        let islands = Islands::compute(&mesh, &uv);
        let params = MosaicParams {
            charts: Some(6),
            ..Default::default()
        };
        let needed =
            mosaic_with_seam_budget(&mesh, &mut uv, &islands, &[0], &params, 0.0).unwrap_err();
        assert!(needed > 0.0);
        assert_eq!(uv, original);
        let result =
            mosaic_with_seam_budget(&mesh, &mut uv, &islands, &[0], &params, needed).unwrap();
        assert!((result.added_seam_length - needed).abs() < 1e-8);
        assert_eq!(result.mosaic.charts, 6);
        assert_eq!(Islands::compute(&mesh, &uv).count(), 6);
    }

    #[test]
    fn automatic_seam_budget_relaxes_normal_error() {
        let mesh = sphere(12, 6);
        let base = welded(&mesh);
        let islands = Islands::compute(&mesh, &base);
        let params = MosaicParams {
            max_error: 0.001,
            iterations: 5,
            ..Default::default()
        };
        let mut tight = base.clone();
        let first =
            mosaic_with_seam_budget(&mesh, &mut tight, &islands, &[0], &params, f64::INFINITY)
                .unwrap();
        let mut loose = base.clone();
        let last = mosaic_with_seam_budget(
            &mesh,
            &mut loose,
            &islands,
            &[0],
            &MosaicParams {
                max_error: 4.0,
                ..params
            },
            f64::INFINITY,
        )
        .unwrap();
        assert!(last.added_seam_length < first.added_seam_length);
        let budget = (last.added_seam_length + first.added_seam_length) / 2.0;
        let mut fitted = base;
        let result =
            mosaic_with_seam_budget(&mesh, &mut fitted, &islands, &[0], &params, budget).unwrap();
        assert!(result.max_error_used > params.max_error);
        assert!(result.added_seam_length <= budget + 1e-8);
    }

    #[test]
    fn measured_uv_distortion_distinguishes_curved_and_planar_charts() {
        let mesh = cube(1);
        let mut curved = welded(&mesh);
        let islands = Islands::compute(&mesh, &curved);
        let result = mosaic(
            &mesh,
            &mut curved,
            &islands,
            &[0],
            &MosaicParams {
                charts: Some(1),
                ..Default::default()
            },
        );
        let after = Islands::compute(&mesh, &curved);
        crate::unfold::unfold_lscm(&mesh, &mut curved, &after, &result.islands, &[]);
        let curved_distortion = max_chart_uv_distortion(&mesh, &curved, &after, &result.islands);
        assert!(curved_distortion > 1.01, "{curved_distortion}");

        let mut planar = welded(&mesh);
        let result = mosaic(
            &mesh,
            &mut planar,
            &islands,
            &[0],
            &MosaicParams {
                charts: Some(6),
                ..Default::default()
            },
        );
        let after = Islands::compute(&mesh, &planar);
        crate::unfold::unfold_lscm(&mesh, &mut planar, &after, &result.islands, &[]);
        let planar_distortion = max_chart_uv_distortion(&mesh, &planar, &after, &result.islands);
        assert!(planar_distortion < 1.000001, "{planar_distortion}");
    }

    #[test]
    fn cylinder_gives_caps_and_side() {
        let m = cylinder(24, 3);
        let (mut uv, seg, report) = run(&m, MosaicParams::default());
        assert_eq!(seg.chart_count, 3);
        assert_eq!(report.charts, 3);
        let side = 24 * 3;
        // Side quads share a chart; each cap is its own chart.
        let c = &seg.chart_of_face;
        assert!((0..side).all(|f| c[f] == c[0]));
        let (b, t) = (c[side], c[side + 1]);
        assert!(b != c[0] && t != c[0] && b != t);
        for i in 0..24 {
            assert_eq!(c[side + 2 * i], b);
            assert_eq!(c[side + 2 * i + 1], t);
        }
        // Three islands, the side tube slit open.
        assert_eq!(Islands::compute(&m, &uv).count(), 3);
        unfolds(&m, &mut uv);
    }

    #[test]
    fn chart_count_is_respected() {
        let m = sphere(16, 10);
        for k in [1, 2, 4, 7, 12, 20] {
            let (mut uv, seg, report) = run(
                &m,
                MosaicParams {
                    charts: Some(k),
                    ..Default::default()
                },
            );
            assert_eq!(seg.chart_count, k);
            assert_eq!(report.charts, k);
            unfolds(&m, &mut uv);
        }
        let c = cylinder(24, 3);
        for k in [1, 2, 3, 5] {
            assert_eq!(
                run(
                    &c,
                    MosaicParams {
                        charts: Some(k),
                        ..Default::default()
                    }
                )
                .1
                .chart_count,
                k
            );
        }
    }

    #[test]
    fn existing_seams_are_kept() {
        // Cube split by an existing seam ring around the middle: two islands.
        let m = cube(4);
        let mut uv = welded(&m);
        let ring: Vec<usize> = (0..m.edge_count())
            .filter(|&e| {
                let [a, b] = m.edge_vertices(e);
                let (pa, pb) = (m.position(a), m.position(b));
                (pa.x - 0.5).abs() < 1e-9 && (pb.x - 0.5).abs() < 1e-9
            })
            .collect();
        assert_eq!(ring.len(), 16);
        seams::cut_edges(&m, &mut uv, &ring);
        let isl = Islands::compute(&m, &uv);
        assert_eq!(isl.count(), 2);
        let params = MosaicParams::default();
        let seg = segment(&m, &uv, &isl, &[0, 1], &params);
        // Four sides are halved by the ring: 2 + 4·2 charts.
        assert_eq!(seg.chart_count, 10);
        for f in 0..m.face_count() {
            for g in 0..m.face_count() {
                if seg.chart_of_face[f] == seg.chart_of_face[g] {
                    assert_eq!(isl.island_of_face(f), isl.island_of_face(g));
                }
            }
        }
        mosaic(&m, &mut uv, &isl, &[0, 1], &params);
        assert!(ring.iter().all(|&e| uv.edge_kind(&m, e) == EdgeKind::Seam));

        // Only one island targeted: the other is untouched.
        let mut uv2 = welded(&m);
        seams::cut_edges(&m, &mut uv2, &ring);
        let before = uv2.clone();
        let r = mosaic(&m, &mut uv2, &isl, &[1], &params);
        assert_eq!(r.charts, 5);
        let other: Vec<u32> = isl.faces(0).to_vec();
        for &f in &other {
            for c in m.face_corners(f as usize) {
                let e = m.corner_edge(c);
                assert_eq!(uv2.edge_kind(&m, e), before.edge_kind(&m, e));
            }
        }
    }

    #[test]
    fn deterministic() {
        let m = sphere(20, 12);
        let p = MosaicParams {
            charts: Some(9),
            ..Default::default()
        };
        let a = run(&m, p);
        let b = run(&m, p);
        assert_eq!(a.1, b.1);
        assert_eq!(a.0, b.0);
    }

    #[test]
    fn eigenvalue() {
        let m = DMat3::from_cols(
            DVec3::new(2.0, 1.0, 0.0),
            DVec3::new(1.0, 2.0, 0.0),
            DVec3::new(0.0, 0.0, 5.0),
        );
        assert!((min_eigenvalue(m) - 1.0).abs() < 1e-12);
        assert_eq!(
            min_eigenvalue(DMat3::from_diagonal(DVec3::new(3., 0.5, 2.))),
            0.5
        );
    }
}
