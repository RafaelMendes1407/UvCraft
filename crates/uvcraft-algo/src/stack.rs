//! Similar islands: find them, stack them, unstack them, and copy UVs from
//! one island onto a topologically identical one.
//!
//! Two islands match *exactly* when there is a bijection between their faces
//! and UV vertices that preserves the face/corner structure (a combinatorial
//! isomorphism of the UV patches). It is found by a breadth-first search over
//! the half-edge structure from a seed face pair, trying every rotation and
//! mirror of the seed. The seed is chosen among faces with the rarest local
//! invariant, so only a handful of candidates are tried. Among all valid
//! correspondences (symmetric islands have several), the one with the best 3D
//! fit wins. With a correspondence, UVs are copied exactly.
//!
//! In similar mode, UV boundary loops are compared independently of interior
//! triangulation. Collinear border splits are ignored for fitting, while the
//! original UV indices remain available for application. The alignment tests
//! cyclic starts, both traversal directions and optional reflection. Every
//! boundary loop, including holes, is checked after fitting.
//!
//! Everything is deterministic: candidates are tried in index order and ties
//! are broken by order.

use std::collections::{BTreeMap, VecDeque};

use glam::{DMat2, DVec2, DVec3};
use uvcraft_core::{EdgeKind, Islands, Mesh, NONE, UvLayer};

/// How strict island matching is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackMode {
    /// Same topology and the same 3D shape (up to rigid motion and mirroring).
    Exact,
    /// Similar UV regions under rotation, reflection, translation and scale.
    #[default]
    Similar,
}

/// Relative 3D edge-length error below which a topological match counts as
/// the same shape in [`StackMode::Exact`] (absorbs text-file rounding).
pub const EXACT_GEOMETRY_TOLERANCE: f64 = 1e-4;

/// Boundary samples used when simplified vertex counts differ.
const SAMPLES: usize = 64;
/// Maximum candidate start points when aligning outlines.
const MAX_STARTS: usize = 256;
/// Seed candidates tried per island pair before giving up.
const MAX_SEED_CANDIDATES: usize = 1024;
/// Valid correspondences collected per island pair (symmetry group size).
const MAX_CORRESPONDENCES: usize = 64;

/// Island connectivity in local indices.
#[derive(Debug, Clone)]
struct Topo {
    /// Local corners of local face `f`: `face_start[f]..face_start[f + 1]`.
    face_start: Vec<u32>,
    /// Global corner per local corner.
    corners: Vec<u32>,
    corner_face: Vec<u32>,
    /// Local UV vertex per local corner.
    corner_uv: Vec<u32>,
    /// Local corner across an interior edge, or `NONE` on the island boundary.
    adj: Vec<u32>,
    /// Global UV vertex per local UV vertex.
    uv_global: Vec<u32>,
    face_key: Vec<u64>,
    /// Hash of the sorted face keys and the element counts.
    signature: u64,
}

fn mix(h: u64, x: u64) -> u64 {
    // FNV-1a over the 8 bytes, then a final avalanche: deterministic across runs.
    let mut h = h;
    for b in x.to_le_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^ (h >> 29)
}

const SEED: u64 = 0xcbf2_9ce4_8422_2325;

impl Topo {
    fn face_count(&self) -> usize {
        self.face_start.len() - 1
    }

    fn degree(&self, f: usize) -> usize {
        (self.face_start[f + 1] - self.face_start[f]) as usize
    }

    fn next(&self, c: usize) -> usize {
        let f = self.corner_face[c] as usize;
        if c + 1 == self.face_start[f + 1] as usize {
            self.face_start[f] as usize
        } else {
            c + 1
        }
    }

    fn position(&self, mesh: &Mesh, c: usize) -> DVec3 {
        mesh.corner_position(self.corners[c] as usize)
    }

    fn build(mesh: &Mesh, uv: &UvLayer, faces: &[u32], scratch: &mut Scratch) -> Topo {
        let mut face_start = Vec::with_capacity(faces.len() + 1);
        let mut corners = Vec::new();
        let mut corner_face = Vec::new();
        face_start.push(0u32);
        for (lf, &f) in faces.iter().enumerate() {
            scratch.face_local[f as usize] = face_start[lf];
            for c in mesh.face_corners(f as usize) {
                corners.push(c as u32);
                corner_face.push(lf as u32);
            }
            face_start.push(corners.len() as u32);
        }

        let mut uv_global = Vec::new();
        let mut corner_uv = Vec::with_capacity(corners.len());
        for &c in &corners {
            let g = uv.corner_uv[c as usize];
            let slot = &mut scratch.uv_local[g as usize];
            if slot.0 != scratch.stamp {
                *slot = (scratch.stamp, uv_global.len() as u32);
                uv_global.push(g);
            }
            corner_uv.push(slot.1);
        }
        scratch.stamp += 1;

        let mut adj = vec![NONE; corners.len()];
        for (lc, &c) in corners.iter().enumerate() {
            let c = c as usize;
            if uv.edge_kind(mesh, mesh.corner_edge(c)) == EdgeKind::Interior
                && let Some(t) = mesh.twin(c)
            {
                let f = mesh.corner_face(t);
                adj[lc] = scratch.face_local[f] + (t - mesh.face_corners(f).start) as u32;
            }
        }

        let mut topo = Topo {
            face_start,
            corners,
            corner_face,
            corner_uv,
            adj,
            uv_global,
            face_key: Vec::new(),
            signature: 0,
        };
        topo.compute_keys();
        topo
    }

    /// Isomorphism-invariant key per face: degree, boundary edges, hop
    /// distance to the boundary, and the multiset of its UV vertices'
    /// (valence, on-boundary) pairs.
    fn compute_keys(&mut self) {
        let nf = self.face_count();
        let nuv = self.uv_global.len();
        let mut valence = vec![0u64; nuv];
        let mut on_border = vec![false; nuv];
        for c in 0..self.corners.len() {
            valence[self.corner_uv[c] as usize] += 1;
            if self.adj[c] == NONE {
                on_border[self.corner_uv[c] as usize] = true;
                on_border[self.corner_uv[self.next(c)] as usize] = true;
            }
        }
        let mut dist = vec![u64::MAX; nf];
        let mut queue = VecDeque::new();
        for (f, d) in dist.iter_mut().enumerate() {
            let r = self.face_start[f] as usize..self.face_start[f + 1] as usize;
            if r.into_iter().any(|c| self.adj[c] == NONE) {
                *d = 0;
                queue.push_back(f);
            }
        }
        while let Some(f) = queue.pop_front() {
            for c in self.face_start[f] as usize..self.face_start[f + 1] as usize {
                if self.adj[c] != NONE {
                    let g = self.corner_face[self.adj[c] as usize] as usize;
                    if dist[g] == u64::MAX {
                        dist[g] = dist[f] + 1;
                        queue.push_back(g);
                    }
                }
            }
        }
        self.face_key = (0..nf)
            .map(|f| {
                let r = self.face_start[f] as usize..self.face_start[f + 1] as usize;
                let mut verts: Vec<u64> = r
                    .clone()
                    .map(|c| {
                        let u = self.corner_uv[c] as usize;
                        (valence[u] << 1) | on_border[u] as u64
                    })
                    .collect();
                verts.sort_unstable();
                let borders = r.filter(|&c| self.adj[c] == NONE).count() as u64;
                let mut h = mix(mix(mix(SEED, self.degree(f) as u64), borders), dist[f]);
                for v in verts {
                    h = mix(h, v);
                }
                h
            })
            .collect();
        let mut keys = self.face_key.clone();
        keys.sort_unstable();
        let mut h = mix(
            mix(mix(SEED, nf as u64), nuv as u64),
            self.corners.len() as u64,
        );
        for k in keys {
            h = mix(h, k);
        }
        self.signature = h;
    }

    /// Closed UV boundary loops, with the outer loop first by absolute area.
    /// Interior edges never enter this representation.
    fn boundary_loop_indices(&self, uv: &UvLayer) -> Vec<Vec<u32>> {
        let border: Vec<usize> = (0..self.corners.len())
            .filter(|&c| self.adj[c] == NONE)
            .collect();
        let mut by_start: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
        let mut by_end: BTreeMap<u32, usize> = BTreeMap::new();
        for &c in &border {
            by_start.entry(self.corner_uv[c]).or_default().push(c);
            *by_end.entry(self.corner_uv[self.next(c)]).or_default() += 1;
        }
        if by_start
            .iter()
            .any(|(vertex, edges)| edges.len() != 1 || by_end.get(vertex) != Some(&1))
        {
            return Vec::new();
        }
        let mut visited = vec![false; self.corners.len()];
        let mut loops = Vec::new();
        for &start in &border {
            if visited[start] {
                continue;
            }
            let mut pts = Vec::new();
            let mut c = start;
            let mut closed = false;
            loop {
                visited[c] = true;
                pts.push(self.uv_global[self.corner_uv[c] as usize]);
                let end = self.corner_uv[self.next(c)];
                if end == self.corner_uv[start] {
                    closed = true;
                    break;
                }
                match by_start
                    .get(&end)
                    .and_then(|v| v.iter().copied().find(|&n| !visited[n]))
                {
                    Some(n) => c = n,
                    None => break,
                }
            }
            if closed
                && pts.len() >= 3
                && polygon_area(
                    &pts.iter()
                        .map(|&id| uv.uvs[id as usize])
                        .collect::<Vec<_>>(),
                )
                .abs()
                    > 1e-20
            {
                loops.push(pts);
            }
        }
        loops.sort_by(|a, b| {
            let area = |ring: &Vec<u32>| {
                polygon_area(
                    &ring
                        .iter()
                        .map(|&id| uv.uvs[id as usize])
                        .collect::<Vec<_>>(),
                )
                .abs()
            };
            area(b).total_cmp(&area(a))
        });
        loops
    }

    fn boundary_loops(&self, uv: &UvLayer) -> Vec<Vec<DVec2>> {
        self.boundary_loop_indices(uv)
            .into_iter()
            .map(|ring| ring.into_iter().map(|id| uv.uvs[id as usize]).collect())
            .collect()
    }
}

fn polygon_area(pts: &[DVec2]) -> f64 {
    (0..pts.len())
        .map(|i| pts[i].perp_dot(pts[(i + 1) % pts.len()]))
        .sum::<f64>()
        * 0.5
}

struct Scratch {
    face_local: Vec<u32>,
    uv_local: Vec<(u32, u32)>,
    stamp: u32,
}

impl Scratch {
    fn new(mesh: &Mesh, uv: &UvLayer) -> Self {
        Scratch {
            face_local: vec![NONE; mesh.face_count()],
            uv_local: vec![(NONE, 0); uv.uvs.len()],
            stamp: 0,
        }
    }
}

/// A combinatorial correspondence from island `a` to island `b`.
#[derive(Debug, Clone)]
struct Correspondence {
    /// Local corner of `b` per local corner of `a`.
    corner_map: Vec<u32>,
    mirrored: bool,
}

/// Image position of corner `i` of a face of degree `n` under offset `k`.
fn image(i: usize, k: usize, mirrored: bool, n: usize) -> usize {
    if mirrored {
        (k + n - i % n) % n
    } else {
        (k + i) % n
    }
}

/// Grows a correspondence from seed `(sf → tf, k, mirrored)` by BFS.
fn grow(
    a: &Topo,
    b: &Topo,
    sf: usize,
    tf: usize,
    k: usize,
    mirrored: bool,
) -> Option<Correspondence> {
    let nf = a.face_count();
    let mut face_map = vec![(NONE, 0u32, false); nf];
    let mut face_rev = vec![NONE; b.face_count()];
    let mut uv_map = vec![NONE; a.uv_global.len()];
    let mut uv_rev = vec![NONE; b.uv_global.len()];
    let mut queue = VecDeque::new();
    face_map[sf] = (tf as u32, k as u32, mirrored);
    face_rev[tf] = sf as u32;
    queue.push_back(sf);
    let mut mapped = 1;
    while let Some(f) = queue.pop_front() {
        let (g, k, m) = face_map[f];
        let (g, k) = (g as usize, k as usize);
        let n = a.degree(f);
        let (fa, fb) = (a.face_start[f] as usize, b.face_start[g] as usize);
        for i in 0..n {
            let ua = a.corner_uv[fa + i] as usize;
            let ub = b.corner_uv[fb + image(i, k, m, n)];
            if uv_map[ua] == NONE && uv_rev[ub as usize] == NONE {
                uv_map[ua] = ub;
                uv_rev[ub as usize] = ua as u32;
            } else if uv_map[ua] != ub {
                return None;
            }
        }
        for i in 0..n {
            let ta = a.adj[fa + i];
            // Half-edge of `g` carrying the image of edge (i, i+1).
            let hb = fb
                + if m {
                    image(i + 1, k, m, n)
                } else {
                    image(i, k, m, n)
                };
            let tb = b.adj[hb];
            if ta == NONE || tb == NONE {
                if ta != tb {
                    return None;
                }
                continue;
            }
            let (ta, tb) = (ta as usize, tb as usize);
            let (f2, g2) = (a.corner_face[ta] as usize, b.corner_face[tb] as usize);
            let n2 = a.degree(f2);
            if b.degree(g2) != n2 {
                return None;
            }
            let pa = ta - a.face_start[f2] as usize;
            let qb = tb - b.face_start[g2] as usize;
            let want = uv_map[a.corner_uv[ta] as usize];
            let (k2, m2) = if b.corner_uv[tb] == want {
                ((qb + n2 - pa) % n2, false)
            } else if b.corner_uv[b.next(tb)] == want {
                ((qb + 1 + pa) % n2, true)
            } else {
                return None;
            };
            let entry = (g2 as u32, k2 as u32, m2);
            if face_map[f2].0 != NONE {
                if face_map[f2] != entry {
                    return None;
                }
            } else {
                if face_rev[g2] != NONE {
                    return None;
                }
                face_map[f2] = entry;
                face_rev[g2] = f2 as u32;
                mapped += 1;
                queue.push_back(f2);
            }
        }
    }
    if mapped != nf {
        return None;
    }
    let mut corner_map = vec![NONE; a.corners.len()];
    for (f, &(g, k, m)) in face_map.iter().enumerate() {
        let n = a.degree(f);
        for i in 0..n {
            corner_map[a.face_start[f] as usize + i] =
                b.face_start[g as usize] + image(i, k as usize, m, n) as u32;
        }
    }
    Some(Correspondence {
        corner_map,
        mirrored,
    })
}

/// All correspondences from `a` to `b` (up to a cap), in deterministic order.
fn correspondences(a: &Topo, b: &Topo) -> Vec<Correspondence> {
    if a.signature != b.signature
        || a.face_count() != b.face_count()
        || a.corners.len() != b.corners.len()
        || a.uv_global.len() != b.uv_global.len()
    {
        return Vec::new();
    }
    let mut by_key: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (f, &k) in b.face_key.iter().enumerate() {
        by_key.entry(k).or_default().push(f);
    }
    let Some(seed) =
        (0..a.face_count()).min_by_key(|&f| (by_key.get(&a.face_key[f]).map_or(0, Vec::len), f))
    else {
        return Vec::new();
    };
    let Some(candidates) = by_key.get(&a.face_key[seed]) else {
        return Vec::new();
    };
    let n = a.degree(seed);
    let mut out = Vec::new();
    let mut tried = 0;
    for &tf in candidates {
        for mirrored in [false, true] {
            for k in 0..n {
                tried += 1;
                if tried > MAX_SEED_CANDIDATES || out.len() >= MAX_CORRESPONDENCES {
                    return out;
                }
                if let Some(c) = grow(a, b, seed, tf, k, mirrored) {
                    out.push(c);
                }
            }
        }
    }
    out
}

/// (relative 3D edge-length error, relative residual after translation-only alignment).
fn geometry_error(mesh: &Mesh, a: &Topo, b: &Topo, corr: &Correspondence) -> (f64, f64) {
    let (mut num, mut den) = (0.0, 0.0);
    let n = a.corners.len();
    let mut ca = DVec3::ZERO;
    let mut cb = DVec3::ZERO;
    for c in 0..n {
        let x = corr.corner_map[c] as usize;
        let y = corr.corner_map[a.next(c)] as usize;
        let la = a.position(mesh, c).distance(a.position(mesh, a.next(c)));
        let lb = b.position(mesh, x).distance(b.position(mesh, y));
        num += (la - lb) * (la - lb);
        den += la * la;
        ca += a.position(mesh, c);
        cb += b.position(mesh, x);
    }
    ca /= n as f64;
    cb /= n as f64;
    let (mut tnum, mut tden) = (0.0, 0.0);
    for c in 0..n {
        let pa = a.position(mesh, c) - ca;
        let pb = b.position(mesh, corr.corner_map[c] as usize) - cb;
        tnum += (pa - pb).length_squared();
        tden += pa.length_squared();
    }
    let rel = |x: f64, d: f64| if d > 0.0 { (x / d).sqrt() } else { 0.0 };
    (rel(num, den), rel(tnum, tden))
}

/// The best correspondence and its relative 3D edge-length error: lowest
/// error, then not mirrored, then closest to a pure translation, then first.
fn best_correspondence(mesh: &Mesh, a: &Topo, b: &Topo) -> Option<(Correspondence, f64)> {
    const EPS: f64 = 1e-9;
    let scored: Vec<_> = correspondences(a, b)
        .into_iter()
        .map(|c| {
            let e = geometry_error(mesh, a, b, &c);
            (c, e)
        })
        .collect();
    let min_err = scored
        .iter()
        .map(|(_, e)| e.0)
        .fold(f64::INFINITY, f64::min);
    let mut close: Vec<_> = scored
        .into_iter()
        .filter(|(_, e)| e.0 <= min_err + EPS)
        .collect();
    // A mirrored correspondence copies UVs with reversed orientation (flipped
    // faces), so it only wins when no unmirrored one fits as well.
    if close.iter().any(|(c, _)| !c.mirrored) {
        close.retain(|(c, _)| !c.mirrored);
    }
    let min_tr = close.iter().map(|(_, e)| e.1).fold(f64::INFINITY, f64::min);
    let pick = close.iter().position(|(_, e)| e.1 <= min_tr + EPS)?;
    let (c, e) = close.swap_remove(pick);
    Some((c, e.0))
}

/// `n` points evenly spaced by arc length along a closed polyline.
fn resample(pts: &[DVec2], n: usize) -> Vec<DVec2> {
    let m = pts.len();
    let seg: Vec<f64> = (0..m).map(|i| pts[i].distance(pts[(i + 1) % m])).collect();
    let total: f64 = seg.iter().sum();
    let mut out = Vec::with_capacity(n);
    let (mut i, mut acc) = (0, 0.0);
    for j in 0..n {
        let t = total * j as f64 / n as f64;
        while i + 1 < m && acc + seg[i] < t {
            acc += seg[i];
            i += 1;
        }
        let f = if seg[i] > 0.0 {
            ((t - acc) / seg[i]).clamp(0.0, 1.0)
        } else {
            0.0
        };
        out.push(pts[i].lerp(pts[(i + 1) % m], f));
    }
    out
}

/// Similarity transform `x ↦ m·(x − from) + to`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Similarity {
    m: DMat2,
    from: DVec2,
    to: DVec2,
    mirrored: bool,
}

/// Remove only redundant points on straight segments. Original UV indices
/// remain in `Topo`, so this is solely a matching representation.
fn simplify_boundary(pts: &[DVec2]) -> Vec<DVec2> {
    let mut out = pts.to_vec();
    let scale = out
        .iter()
        .fold(DVec2::splat(f64::INFINITY), |a, &p| a.min(p));
    let hi = out
        .iter()
        .fold(DVec2::splat(f64::NEG_INFINITY), |a, &p| a.max(p));
    let eps = (hi - scale).length() * 1e-10;
    let mut i = 0;
    while out.len() > 3 && i < out.len() {
        let n = out.len();
        let (a, b, c) = (out[(i + n - 1) % n], out[i], out[(i + 1) % n]);
        let edge = c - a;
        if edge.length() > eps
            && edge.perp_dot(b - a).abs() / edge.length() <= eps
            && (b - a).dot(b - c) <= eps * eps
        {
            out.remove(i);
        } else {
            i += 1;
        }
    }
    out
}

fn fit_points(q: &[DVec2], pts: &[DVec2], mirrored: bool) -> Option<(Similarity, f64)> {
    let n = q.len();
    let cq = q.iter().copied().sum::<DVec2>() / n as f64;
    let cp = pts.iter().copied().sum::<DVec2>() / n as f64;
    let qq: f64 = q.iter().map(|x| (*x - cq).length_squared()).sum();
    let (mut a, mut b, mut pp) = (0.0, 0.0, 0.0);
    let mirror = if mirrored {
        DMat2::from_cols(-DVec2::X, DVec2::Y)
    } else {
        DMat2::IDENTITY
    };
    for (&x, &y) in pts.iter().zip(q) {
        let (x, y) = (mirror * (x - cp), y - cq);
        a += x.dot(y);
        b += x.perp_dot(y);
        pp += x.length_squared();
    }
    if qq <= 1e-30 || pp <= 1e-30 {
        return None;
    }
    let s = (a * a + b * b).sqrt() / pp;
    let sim = Similarity {
        m: DMat2::from_angle(b.atan2(a)) * mirror * s,
        from: cp,
        to: cq,
        mirrored,
    };
    let err = pts
        .iter()
        .zip(q)
        .map(|(&x, &y)| (sim.m * (x - cp) + cq - y).length_squared())
        .sum::<f64>();
    Some((sim, (err / qq).sqrt()))
}

/// Try every cyclic start and both traversal directions. Equal simplified
/// polygons use their corners directly; otherwise compare arc-length samples.
fn fit_boundaries(src: &[DVec2], tgt: &[DVec2]) -> Option<(Similarity, f64)> {
    let src = simplify_boundary(src);
    let tgt = simplify_boundary(tgt);
    let direct = src.len() == tgt.len();
    let q = if direct { src } else { resample(&src, SAMPLES) };
    let base = if direct { tgt } else { resample(&tgt, SAMPLES) };
    let mut best: Option<(Similarity, f64)> = None;
    for reversed in [false, true] {
        for start in 0..base.len().min(MAX_STARTS) {
            let pts: Vec<_> = (0..base.len())
                .map(|i| {
                    base[if reversed {
                        (start + base.len() - i) % base.len()
                    } else {
                        (start + i) % base.len()
                    }]
                })
                .collect();
            for mirrored in [false, true] {
                if let Some((sim, rel)) = fit_points(&q, &pts, mirrored)
                    && best.as_ref().is_none_or(|(_, r)| rel < *r - 1e-12)
                {
                    best = Some((sim, rel));
                }
            }
        }
    }
    best
}

/// How a target island is placed onto its reference.
#[derive(Debug, Clone)]
enum Placement {
    /// Copy UVs through a topological correspondence: `(source uv, target uv)`.
    Exact(Vec<(u32, u32)>),
    /// Move the target's own UVs with a similarity transform.
    Aligned(Similarity, f64),
}

/// Matching policy.
#[derive(Debug, Clone, Copy)]
enum Policy {
    Mode(StackMode, f64),
    /// Always place if at all possible (`match` command).
    Force,
}

fn exact_placement(a: &Topo, b: &Topo, corr: &Correspondence) -> Placement {
    let mut pairs: Vec<(u32, u32)> = (0..a.corners.len())
        .map(|c| {
            (
                a.uv_global[a.corner_uv[c] as usize],
                b.uv_global[b.corner_uv[corr.corner_map[c] as usize] as usize],
            )
        })
        .collect();
    pairs.sort_unstable();
    pairs.dedup();
    Placement::Exact(pairs)
}

fn point_segment_distance(p: DVec2, a: DVec2, b: DVec2) -> f64 {
    let d = b - a;
    let t = if d.length_squared() > 0.0 {
        ((p - a).dot(d) / d.length_squared()).clamp(0.0, 1.0)
    } else {
        0.0
    };
    p.distance(a + d * t)
}

fn closest_on_segment(p: DVec2, a: DVec2, b: DVec2) -> DVec2 {
    let d = b - a;
    let t = if d.length_squared() > 0.0 {
        ((p - a).dot(d) / d.length_squared()).clamp(0.0, 1.0)
    } else {
        0.0
    };
    a + d * t
}

fn face_uv_area(mesh: &Mesh, uv: &UvLayer, face: usize) -> f64 {
    let corners = mesh.face_corners(face);
    let points: Vec<_> = corners.map(|c| uv.uv_at_corner(c)).collect();
    polygon_area(&points)
}

/// Fixed-boundary uniform harmonic relaxation. Work on a copy and commit only
/// after boundary, orientation and area checks have all passed.
fn conform(
    mesh: &Mesh,
    uv: &mut UvLayer,
    reference: &Topo,
    target: &Topo,
    sim: Similarity,
    tolerance: f64,
    max_distortion: f64,
) -> Result<(), &'static str> {
    let src_loops = reference.boundary_loops(uv);
    let mut candidate = uv.clone();
    apply(&mut candidate, target, &Placement::Aligned(sim, 0.0));
    let initial_areas: Vec<f64> = (0..target.face_count())
        .map(|f| {
            let face = mesh.corner_face(target.corners[target.face_start[f] as usize] as usize);
            face_uv_area(mesh, &candidate, face)
        })
        .collect();
    let mut boundary = vec![false; target.uv_global.len()];
    for c in 0..target.corners.len() {
        if target.adj[c] == NONE {
            boundary[target.corner_uv[c] as usize] = true;
            boundary[target.corner_uv[target.next(c)] as usize] = true;
        }
    }
    let mut neighbors = vec![std::collections::BTreeSet::<usize>::new(); target.uv_global.len()];
    for c in 0..target.corners.len() {
        let (a, b) = (
            target.corner_uv[c] as usize,
            target.corner_uv[target.next(c)] as usize,
        );
        neighbors[a].insert(b);
        neighbors[b].insert(a);
    }
    let source_corners: Vec<DVec2> = src_loops
        .iter()
        .flat_map(|ring| simplify_boundary(ring))
        .collect();
    let size = src_loops[0]
        .iter()
        .fold(DVec2::NEG_INFINITY, |a, &p| a.max(p))
        .distance(src_loops[0].iter().fold(DVec2::INFINITY, |a, &p| a.min(p)));
    let aligned = candidate.uvs.clone();
    for ring in target.boundary_loop_indices(uv) {
        for (i, &global) in ring.iter().enumerate() {
            let n = ring.len();
            let p = aligned[global as usize];
            let a = aligned[ring[(i + n - 1) % n] as usize];
            let b = aligned[ring[(i + 1) % n] as usize];
            let turn =
                (p - a).perp_dot(b - p).abs() / ((p - a).length() * (b - p).length()).max(1e-30);
            let source_corner = source_corners
                .iter()
                .copied()
                .min_by(|a, b| p.distance_squared(*a).total_cmp(&p.distance_squared(*b)));
            let projection = src_loops
                .iter()
                .flat_map(|ring| {
                    (0..ring.len())
                        .map(move |j| closest_on_segment(p, ring[j], ring[(j + 1) % ring.len()]))
                })
                .min_by(|a, b| p.distance_squared(*a).total_cmp(&p.distance_squared(*b)));
            let Some(best) = projection else {
                return Err("reference has no usable UV boundary");
            };
            candidate.uvs[global as usize] = if turn > 1e-6
                && source_corner.is_some_and(|corner| corner.distance(p) <= tolerance * size)
            {
                source_corner.unwrap()
            } else {
                best
            };
        }
    }
    for _ in 0..500 {
        let mut max_delta: f64 = 0.0;
        for (local, &global) in target.uv_global.iter().enumerate() {
            if boundary[local] || neighbors[local].is_empty() {
                continue;
            }
            let avg = neighbors[local]
                .iter()
                .map(|&n| candidate.uvs[target.uv_global[n] as usize])
                .sum::<DVec2>()
                / neighbors[local].len() as f64;
            max_delta = max_delta.max(avg.distance(candidate.uvs[global as usize]));
            candidate.uvs[global as usize] = avg;
        }
        if max_delta < 1e-12 {
            break;
        }
    }
    let target_loops = target.boundary_loops(&candidate);
    let identity = Similarity {
        m: DMat2::IDENTITY,
        from: DVec2::ZERO,
        to: DVec2::ZERO,
        mirrored: false,
    };
    if boundary_error(&src_loops, &target_loops, identity) > tolerance.min(1e-6) {
        return Err("boundary cannot coincide with the reference using this topology");
    }
    for (f, &before) in initial_areas.iter().enumerate() {
        let face = mesh.corner_face(target.corners[target.face_start[f] as usize] as usize);
        let after = face_uv_area(mesh, &candidate, face);
        if before.abs() <= 1e-15 || after.abs() <= 1e-15 {
            return Err("degenerate UV face after conforming");
        }
        if before.signum() != after.signum() {
            return Err("inverted UV face after conforming");
        }
        let ratio = (after / before).abs();
        if ratio > max_distortion || ratio < 1.0 / max_distortion {
            return Err("UV face distortion exceeds the limit");
        }
    }
    for &global in &target.uv_global {
        uv.uvs[global as usize] = candidate.uvs[global as usize];
    }
    Ok(())
}

fn loop_distance(a: &[DVec2], b: &[DVec2]) -> f64 {
    a.iter()
        .map(|&p| {
            (0..b.len())
                .map(|i| point_segment_distance(p, b[i], b[(i + 1) % b.len()]))
                .fold(f64::INFINITY, f64::min)
        })
        .fold(0.0, f64::max)
}

fn boundary_error(src: &[Vec<DVec2>], tgt: &[Vec<DVec2>], sim: Similarity) -> f64 {
    if src.len() != tgt.len() || src.is_empty() {
        return f64::INFINITY;
    }
    let size = src[0]
        .iter()
        .fold(DVec2::splat(f64::NEG_INFINITY), |a, &p| a.max(p))
        .distance(
            src[0]
                .iter()
                .fold(DVec2::splat(f64::INFINITY), |a, &p| a.min(p)),
        );
    if size <= 0.0 {
        return f64::INFINITY;
    }
    let transformed: Vec<Vec<DVec2>> = tgt
        .iter()
        .map(|ring| {
            ring.iter()
                .map(|&p| sim.m * (p - sim.from) + sim.to)
                .collect()
        })
        .collect();
    // Pair the outer loops, then seek the best unused hole for each source
    // hole. Check both directions so a missing notch cannot be hidden.
    let mut used = vec![false; transformed.len()];
    let mut worst: f64 = 0.0;
    for (i, ring) in src.iter().enumerate() {
        let candidates = if i == 0 { 0..1 } else { 1..transformed.len() };
        let Some((j, error)) = candidates
            .filter(|&j| !used[j])
            .map(|j| {
                let other = &transformed[j];
                (
                    j,
                    loop_distance(ring, other).max(loop_distance(other, ring)),
                )
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
        else {
            return f64::INFINITY;
        };
        used[j] = true;
        worst = worst.max(error);
    }
    worst / size
}

fn mean_boundary_error(src: &[Vec<DVec2>], tgt: &[Vec<DVec2>], sim: Similarity) -> f64 {
    let transformed: Vec<Vec<DVec2>> = tgt
        .iter()
        .map(|ring| {
            ring.iter()
                .map(|&p| sim.m * (p - sim.from) + sim.to)
                .collect()
        })
        .collect();
    let distance = |p: DVec2, loops: &[Vec<DVec2>]| {
        loops
            .iter()
            .flat_map(|ring| {
                (0..ring.len())
                    .map(move |i| point_segment_distance(p, ring[i], ring[(i + 1) % ring.len()]))
            })
            .fold(f64::INFINITY, f64::min)
    };
    let mut sum = 0.0;
    let mut count = 0;
    for ring in src {
        for &p in ring {
            sum += distance(p, &transformed);
            count += 1;
        }
    }
    for ring in &transformed {
        for &p in ring {
            sum += distance(p, src);
            count += 1;
        }
    }
    let lo = src[0].iter().fold(DVec2::INFINITY, |a, &p| a.min(p));
    let hi = src[0].iter().fold(DVec2::NEG_INFINITY, |a, &p| a.max(p));
    if count == 0 || hi.distance(lo) <= 0.0 {
        f64::INFINITY
    } else {
        sum / count as f64 / hi.distance(lo)
    }
}

fn point_in_ring(p: DVec2, ring: &[DVec2]) -> bool {
    let mut inside = false;
    for i in 0..ring.len() {
        let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
        if (a.y > p.y) != (b.y > p.y) && p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x {
            inside = !inside;
        }
    }
    inside
}

fn point_in_region(p: DVec2, loops: &[Vec<DVec2>]) -> bool {
    loops.first().is_some_and(|outer| point_in_ring(p, outer))
        && loops[1..].iter().all(|hole| !point_in_ring(p, hole))
}

/// Deterministic area overlap estimate used only for diagnostics. Boundary
/// distances remain the acceptance criterion, including thin details.
fn overlap_fraction(src: &[Vec<DVec2>], tgt: &[Vec<DVec2>], sim: Similarity) -> f64 {
    let transformed: Vec<Vec<DVec2>> = tgt
        .iter()
        .map(|ring| {
            ring.iter()
                .map(|&p| sim.m * (p - sim.from) + sim.to)
                .collect()
        })
        .collect();
    let mut lo = DVec2::INFINITY;
    let mut hi = DVec2::NEG_INFINITY;
    for &p in src.iter().chain(&transformed).flat_map(|ring| ring.iter()) {
        lo = lo.min(p);
        hi = hi.max(p);
    }
    let mut inter = 0usize;
    let mut union = 0usize;
    for y in 0..80 {
        for x in 0..80 {
            let p = lo + (hi - lo) * DVec2::new((x as f64 + 0.5) / 80.0, (y as f64 + 0.5) / 80.0);
            let a = point_in_region(p, src);
            let b = point_in_region(p, &transformed);
            inter += usize::from(a && b);
            union += usize::from(a || b);
        }
    }
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// How to place island `b` onto reference `a`, or `None` if they do not match.
fn plan(mesh: &Mesh, uv: &UvLayer, a: &Topo, b: &Topo, policy: Policy) -> Option<Placement> {
    let geometry_tol = match policy {
        Policy::Mode(StackMode::Exact, _) => EXACT_GEOMETRY_TOLERANCE,
        Policy::Mode(StackMode::Similar, tol) => tol.max(EXACT_GEOMETRY_TOLERANCE),
        Policy::Force => f64::INFINITY,
    };
    if matches!(policy, Policy::Mode(StackMode::Exact, _) | Policy::Force)
        && let Some((corr, err)) = best_correspondence(mesh, a, b)
        && err <= geometry_tol
    {
        return Some(exact_placement(a, b, &corr));
    }
    let shape_tol = match policy {
        Policy::Mode(StackMode::Exact, _) => return None,
        Policy::Mode(StackMode::Similar, tol) => tol,
        Policy::Force => f64::INFINITY,
    };
    let (src, tgt) = (a.boundary_loops(uv), b.boundary_loops(uv));
    if src.len() != tgt.len() {
        return None;
    }
    let (sim, residual) = fit_boundaries(src.first()?, tgt.first()?)?;
    let max_error = boundary_error(&src, &tgt, sim);
    (residual <= shape_tol && max_error <= shape_tol).then_some(Placement::Aligned(sim, max_error))
}

fn failure_reason(uv: &UvLayer, a: &Topo, b: &Topo, mode: StackMode, tolerance: f64) -> String {
    if mode == StackMode::Exact {
        return "topology or 3D edge geometry differs in exact mode".into();
    }
    let (src, tgt) = (a.boundary_loops(uv), b.boundary_loops(uv));
    if src.is_empty() || tgt.is_empty() {
        return "missing or degenerate UV boundary".into();
    }
    if src.len() != tgt.len() {
        return format!(
            "UV boundary loop count differs ({} versus {})",
            src.len(),
            tgt.len()
        );
    }
    let Some((sim, fit)) = fit_boundaries(&src[0], &tgt[0]) else {
        return "boundary alignment is degenerate".into();
    };
    let error = boundary_error(&src, &tgt, sim);
    if fit > tolerance {
        format!(
            "contours incompatible: fit error {:.4}% exceeds {:.4}%",
            fit * 100.0,
            tolerance * 100.0
        )
    } else {
        format!(
            "boundary difference {:.4}% exceeds {:.4}%",
            error * 100.0,
            tolerance * 100.0
        )
    }
}

fn apply(uv: &mut UvLayer, b: &Topo, placement: &Placement) {
    match placement {
        Placement::Exact(pairs) => {
            for &(s, t) in pairs {
                uv.uvs[t as usize] = uv.uvs[s as usize];
            }
        }
        Placement::Aligned(sim, _) => {
            for &g in &b.uv_global {
                let p = &mut uv.uvs[g as usize];
                *p = sim.m * (*p - sim.from) + sim.to;
            }
        }
    }
}

fn build_topos(mesh: &Mesh, uv: &UvLayer, islands: &Islands, ids: &[usize]) -> Vec<Topo> {
    let mut scratch = Scratch::new(mesh, uv);
    ids.iter()
        .map(|&i| Topo::build(mesh, uv, islands.faces(i), &mut scratch))
        .collect()
}

/// A group of matching islands: the reference first, then the others with
/// how each is placed onto it.
struct Group {
    members: Vec<(usize, Option<Placement>)>,
}

fn group(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    targets: &[usize],
    mode: StackMode,
    tolerance: f64,
    reference_only: bool,
) -> (Vec<Topo>, Vec<Group>) {
    let topos = build_topos(mesh, uv, islands, targets);
    let mut assigned = vec![false; targets.len()];
    let mut groups = Vec::new();
    for r in 0..targets.len() {
        if reference_only && r > 0 {
            break;
        }
        if assigned[r] {
            continue;
        }
        assigned[r] = true;
        let mut members = vec![(r, None)];
        for c in r + 1..targets.len() {
            if assigned[c] {
                continue;
            }
            if let Some(p) = plan(
                mesh,
                uv,
                &topos[r],
                &topos[c],
                Policy::Mode(mode, tolerance),
            ) {
                assigned[c] = true;
                members.push((c, Some(p)));
            }
        }
        groups.push(Group { members });
    }
    (topos, groups)
}

/// Groups of similar islands among `targets` (sorted island indices), each
/// with its reference (lowest index) first. Singletons are left out.
pub fn similar_groups(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    targets: &[usize],
    mode: StackMode,
    tolerance: f64,
) -> Vec<Vec<usize>> {
    let (_, groups) = group(mesh, uv, islands, targets, mode, tolerance, false);
    groups
        .into_iter()
        .filter(|g| g.members.len() > 1)
        .map(|g| g.members.iter().map(|&(i, _)| targets[i]).collect())
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StackReport {
    /// Groups stacked (reference first); singletons are left out.
    pub groups: Vec<Vec<usize>>,
    /// Islands whose UVs were copied through a topological correspondence.
    pub exact: Vec<usize>,
    /// Islands aligned onto their reference by a shape fit.
    pub aligned: Vec<usize>,
    /// Diagnostics for each placed island, relative to its reference size.
    pub matches: Vec<StackMatch>,
    /// Islands rejected during optional conforming, with the reason.
    pub failed: Vec<(usize, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryQuality {
    Exact,
    Approximate,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StackMatch {
    pub reference: usize,
    pub island: usize,
    pub quality: BoundaryQuality,
    pub max_boundary_error: f64,
    pub mean_boundary_error: f64,
    pub overlap_fraction: f64,
    pub scale: f64,
    pub mirrored: bool,
}

/// Stacks each group of similar islands among `targets` onto its reference
/// (the lowest island index of the group).
#[derive(Clone, Copy)]
struct StackOptions {
    preview: bool,
    conform_boundary: bool,
    max_distortion: f64,
    reference_only: bool,
}

fn stack_impl(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    mode: StackMode,
    tolerance: f64,
    options: StackOptions,
) -> StackReport {
    let (topos, groups) = group(
        mesh,
        uv,
        islands,
        targets,
        mode,
        tolerance,
        options.reference_only,
    );
    let mut report = StackReport::default();
    for g in groups.into_iter().filter(|g| g.members.len() > 1) {
        let reference_index = g.members[0].0;
        let reference = targets[reference_index];
        let mut succeeded = vec![reference];
        for (i, p) in g.members {
            let Some(p) = p else { continue };
            let (max_boundary_error, mean_error, overlap, scale, mirrored) = match &p {
                Placement::Exact(_) => (0.0, 0.0, 1.0, 1.0, false),
                Placement::Aligned(sim, error) => {
                    let src = topos[reference_index].boundary_loops(uv);
                    let tgt = topos[i].boundary_loops(uv);
                    (
                        *error,
                        mean_boundary_error(&src, &tgt, *sim),
                        overlap_fraction(&src, &tgt, *sim),
                        sim.m.x_axis.length(),
                        sim.mirrored,
                    )
                }
            };
            if options.conform_boundary
                && !options.preview
                && let Placement::Aligned(sim, _) = &p
                && let Err(reason) = conform(
                    mesh,
                    uv,
                    &topos[reference_index],
                    &topos[i],
                    *sim,
                    tolerance,
                    options.max_distortion,
                )
            {
                report.failed.push((targets[i], reason.into()));
                continue;
            }
            match p {
                Placement::Exact(_) => report.exact.push(targets[i]),
                Placement::Aligned(_, _) => report.aligned.push(targets[i]),
            }
            report.matches.push(StackMatch {
                reference,
                island: targets[i],
                quality: if max_boundary_error <= 1e-6 {
                    BoundaryQuality::Exact
                } else {
                    BoundaryQuality::Approximate
                },
                max_boundary_error,
                mean_boundary_error: mean_error,
                overlap_fraction: overlap,
                scale,
                mirrored,
            });
            if !options.preview && !options.conform_boundary {
                apply(uv, &topos[i], &p);
            }
            if !options.preview && options.conform_boundary && matches!(p, Placement::Exact(_)) {
                apply(uv, &topos[i], &p);
            }
            succeeded.push(targets[i]);
        }
        if succeeded.len() > 1 {
            report.groups.push(succeeded);
        }
    }
    if options.reference_only && !targets.is_empty() {
        for i in 1..targets.len() {
            if !report.matches.iter().any(|m| m.island == targets[i])
                && !report
                    .failed
                    .iter()
                    .any(|(island, _)| *island == targets[i])
            {
                report.failed.push((
                    targets[i],
                    failure_reason(uv, &topos[0], &topos[i], mode, tolerance),
                ));
            }
        }
    }
    report.exact.sort_unstable();
    report.aligned.sort_unstable();
    report
}

pub fn stack(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    mode: StackMode,
    tolerance: f64,
) -> StackReport {
    stack_impl(
        mesh,
        uv,
        islands,
        targets,
        mode,
        tolerance,
        StackOptions {
            preview: false,
            conform_boundary: false,
            max_distortion: 4.0,
            reference_only: false,
        },
    )
}

/// Stack only islands compatible with the first target onto that reference.
pub fn stack_to_reference(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    mode: StackMode,
    tolerance: f64,
) -> StackReport {
    stack_impl(
        mesh,
        uv,
        islands,
        targets,
        mode,
        tolerance,
        StackOptions {
            preview: false,
            conform_boundary: false,
            max_distortion: 4.0,
            reference_only: true,
        },
    )
}

/// Conform accepted islands with a fixed UV border and reject invalid faces.
pub fn stack_conform(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    mode: StackMode,
    tolerance: f64,
    max_distortion: f64,
) -> StackReport {
    stack_impl(
        mesh,
        uv,
        islands,
        targets,
        mode,
        tolerance,
        StackOptions {
            preview: false,
            conform_boundary: true,
            max_distortion,
            reference_only: false,
        },
    )
}

pub fn stack_conform_to_reference(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    mode: StackMode,
    tolerance: f64,
    max_distortion: f64,
) -> StackReport {
    stack_impl(
        mesh,
        uv,
        islands,
        targets,
        mode,
        tolerance,
        StackOptions {
            preview: false,
            conform_boundary: true,
            max_distortion,
            reference_only: true,
        },
    )
}

/// Read-only placement plan for UI previews and command diagnostics.
pub fn preview(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    targets: &[usize],
    mode: StackMode,
    tolerance: f64,
) -> StackReport {
    let mut copy = uv.clone();
    stack_impl(
        mesh,
        &mut copy,
        islands,
        targets,
        mode,
        tolerance,
        StackOptions {
            preview: true,
            conform_boundary: false,
            max_distortion: 4.0,
            reference_only: false,
        },
    )
}

pub fn preview_to_reference(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    targets: &[usize],
    mode: StackMode,
    tolerance: f64,
) -> StackReport {
    let mut copy = uv.clone();
    stack_impl(
        mesh,
        &mut copy,
        islands,
        targets,
        mode,
        tolerance,
        StackOptions {
            preview: true,
            conform_boundary: false,
            max_distortion: 4.0,
            reference_only: true,
        },
    )
}

/// Check an existing stack in its current UV positions. This uses the
/// identity transform, so a later individual move invalidates the relation.
pub fn overlaid(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    reference: usize,
    target: usize,
    tolerance: f64,
) -> bool {
    let topos = build_topos(mesh, uv, islands, &[reference, target]);
    let src = topos[0].boundary_loops(uv);
    let tgt = topos[1].boundary_loops(uv);
    let identity = Similarity {
        m: DMat2::IDENTITY,
        from: DVec2::ZERO,
        to: DVec2::ZERO,
        mirrored: false,
    };
    boundary_error(&src, &tgt, identity) <= tolerance
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MatchReport {
    /// Targets that received the source's UVs exactly (same topology).
    pub exact: Vec<usize>,
    /// Targets whose own UVs were aligned onto the source's outline.
    pub aligned: Vec<usize>,
    /// Targets that could not be matched (closed or degenerate UVs).
    pub failed: Vec<usize>,
}

/// Places each target onto `source`: copies the source's UVs when the
/// topology matches (copy/paste UVs), else aligns the target's outline onto
/// the source's.
pub fn match_islands(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    source: usize,
    targets: &[usize],
) -> MatchReport {
    let mut report = MatchReport::default();
    let mut scratch = Scratch::new(mesh, uv);
    let a = Topo::build(mesh, uv, islands.faces(source), &mut scratch);
    let mut placements = Vec::new();
    for &t in targets.iter().filter(|&&t| t != source) {
        let b = Topo::build(mesh, uv, islands.faces(t), &mut scratch);
        match plan(mesh, uv, &a, &b, Policy::Force) {
            Some(p) => {
                match p {
                    Placement::Exact(_) => report.exact.push(t),
                    Placement::Aligned(_, _) => report.aligned.push(t),
                }
                placements.push((b, p));
            }
            None => report.failed.push(t),
        }
    }
    for (b, p) in &placements {
        apply(uv, b, p);
    }
    report
}

/// Separates stacked islands among `targets`: islands whose UV bounding
/// boxes overlap by at least half (intersection over union) form a stack;
/// all but the first of each stack are moved into a row to its right.
/// Returns the number of islands moved.
pub fn unstack(mesh: &Mesh, uv: &mut UvLayer, islands: &Islands, targets: &[usize]) -> usize {
    let boxes: Vec<(DVec2, DVec2)> = targets
        .iter()
        .map(|&i| {
            islands.uv_indices(i, mesh, uv).iter().fold(
                (DVec2::splat(f64::INFINITY), DVec2::splat(f64::NEG_INFINITY)),
                |(lo, hi), &k| (lo.min(uv.uvs[k as usize]), hi.max(uv.uvs[k as usize])),
            )
        })
        .collect();
    let area = |lo: DVec2, hi: DVec2| {
        let d = (hi - lo).max(DVec2::ZERO);
        d.x * d.y
    };
    let iou = |a: (DVec2, DVec2), b: (DVec2, DVec2)| {
        let inter = area(a.0.max(b.0), a.1.min(b.1));
        let union = area(a.0, a.1) + area(b.0, b.1) - inter;
        if union > 0.0 {
            inter / union
        } else {
            // Degenerate boxes: stacked when identical.
            f64::from(a == b)
        }
    };
    // Union-find over overlapping pairs; roots are the lowest index.
    let mut parent: Vec<usize> = (0..targets.len()).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    for i in 0..targets.len() {
        for j in i + 1..targets.len() {
            if iou(boxes[i], boxes[j]) >= 0.5 {
                let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                parent[a.max(b)] = a.min(b);
            }
        }
    }
    let mut stacks: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..targets.len() {
        let r = find(&mut parent, i);
        stacks.entry(r).or_default().push(i);
    }
    let mut moved = 0;
    for members in stacks.values().filter(|m| m.len() > 1) {
        let (lo0, hi0) = boxes[members[0]];
        let gap = 0.1
            * members
                .iter()
                .map(|&i| (boxes[i].1 - boxes[i].0).x)
                .fold(0.0, f64::max);
        let mut x = hi0.x + gap;
        for &i in &members[1..] {
            let (lo, hi) = boxes[i];
            let t = DVec2::new(x - lo.x, lo0.y - lo.y);
            for k in islands.uv_indices(targets[i], mesh, uv) {
                uv.uvs[k as usize] += t;
            }
            x += (hi - lo).x + gap;
            moved += 1;
        }
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outline_pair(
        transform: impl Fn(DVec2) -> DVec2,
        rotate_start: usize,
        reverse: bool,
    ) -> (Mesh, UvLayer, Islands) {
        let shape = [
            DVec2::new(0.0, 0.0),
            DVec2::new(2.0, 0.0),
            DVec2::new(2.4, 0.8),
            DVec2::new(1.3, 1.4),
            DVec2::new(0.2, 0.9),
        ];
        let n = shape.len();
        let positions: Vec<DVec3> = shape
            .iter()
            .map(|p| p.extend(0.0))
            .chain(shape.iter().map(|p| p.extend(2.0)))
            .collect();
        let second: Vec<u32> = (0..n)
            .map(|i| {
                let j = if reverse {
                    (rotate_start + n - i) % n
                } else {
                    (rotate_start + i) % n
                };
                (n + j) as u32
            })
            .collect();
        let faces = vec![(0..n as u32).collect::<Vec<_>>(), second];
        let mesh = Mesh::new(positions, &faces).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: faces.iter().flatten().copied().collect(),
            uvs: shape
                .iter()
                .copied()
                .chain(shape.iter().copied().map(transform))
                .collect(),
        };
        let islands = Islands::compute(&mesh, &uv);
        (mesh, uv, islands)
    }

    #[test]
    fn outline_alignment_handles_rotation_reflection_scale_and_start_order() {
        for (mirror, reverse) in [(false, false), (true, false), (true, true)] {
            let (mesh, mut uv, islands) = outline_pair(
                |p| {
                    let p = if mirror { DVec2::new(-p.x, p.y) } else { p };
                    DMat2::from_angle(0.71) * p * 0.63 + DVec2::new(5.0, -3.0)
                },
                3,
                reverse,
            );
            let before = uv.clone();
            let preview = preview(&mesh, &uv, &islands, &[0, 1], StackMode::Similar, 1e-5);
            assert_eq!(uv, before);
            assert_eq!(preview.aligned, vec![1]);
            let report = stack(&mesh, &mut uv, &islands, &[0, 1], StackMode::Similar, 1e-5);
            assert_eq!(report.aligned, vec![1]);
            assert!(overlaid(&mesh, &uv, &islands, 0, 1, 1e-8));
            assert_eq!(report.matches[0].mirrored, mirror);
            assert!((report.matches[0].scale - 1.0 / 0.63).abs() < 1e-9);
        }
    }

    #[test]
    fn similarly_sized_but_different_outline_is_rejected() {
        let (mesh, mut uv, islands) = outline_pair(
            |p| {
                let mut q = p + DVec2::new(5.0, 0.0);
                if p == DVec2::new(2.4, 0.8) {
                    q.y += 0.5;
                }
                q
            },
            0,
            false,
        );
        let before = uv.clone();
        let report = stack(&mesh, &mut uv, &islands, &[0, 1], StackMode::Similar, 0.01);
        assert!(report.groups.is_empty());
        assert_eq!(uv, before);
    }

    #[test]
    fn holes_must_match_even_when_outer_boundaries_match() {
        let outer = [
            DVec2::new(0.0, 0.0),
            DVec2::new(4.0, 0.0),
            DVec2::new(4.0, 4.0),
            DVec2::new(0.0, 4.0),
        ];
        let inner = [
            DVec2::new(1.0, 1.0),
            DVec2::new(2.0, 1.0),
            DVec2::new(2.0, 2.0),
            DVec2::new(1.0, 2.0),
        ];
        let mut positions = Vec::new();
        let mut uvs = Vec::new();
        let mut faces = Vec::<Vec<u32>>::new();
        for copy in 0..2u32 {
            let offset = copy * 8;
            for p in outer.into_iter().chain(inner) {
                positions.push(p.extend(copy as f64 * 3.0));
                uvs.push(p + DVec2::new(copy as f64 * 10.0, 0.0));
            }
            for i in 0..4u32 {
                faces.push(vec![
                    offset + i,
                    offset + (i + 1) % 4,
                    offset + 4 + (i + 1) % 4,
                    offset + 4 + i,
                ]);
            }
        }
        let mesh = Mesh::new(positions, &faces).unwrap();
        let corner_uv = faces.iter().flatten().copied().collect();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv,
            uvs,
        };
        let islands = Islands::compute(&mesh, &uv);
        assert_eq!(islands.count(), 2);
        let topos = build_topos(&mesh, &uv, &islands, &[0, 1]);
        assert_eq!(topos[0].boundary_loops(&uv).len(), 2);
        assert_eq!(topos[1].boundary_loops(&uv).len(), 2);
        let matched = stack(&mesh, &mut uv, &islands, &[0, 1], StackMode::Similar, 1e-5);
        assert_eq!(matched.aligned, vec![1]);
        for p in &mut uv.uvs[12..16] {
            p.x += 0.4;
        }
        let before = uv.clone();
        let rejected = stack(&mesh, &mut uv, &islands, &[0, 1], StackMode::Similar, 0.01);
        assert!(rejected.groups.is_empty());
        assert_eq!(uv, before);
    }

    #[test]
    fn reference_silhouette_stacks_despite_different_triangulation_and_border_split() {
        // Traced from docs/images/ajuste_empilhamento.png: raised rectangular neck,
        // sloping shoulders, straight body and a wider tapered foot.
        let outline = [
            (1.0, 5.0),
            (3.0, 5.0),
            (3.0, 3.0),
            (4.0, 1.8),
            (4.0, 0.5),
            (4.4, 0.0),
            (0.0, 0.0),
            (0.4, 0.5),
            (0.4, 1.8),
            (1.0, 3.0),
        ];
        let mut points = vec![DVec2::new(2.0, 5.0)]; // extra collinear border point
        points.extend(
            outline[1..]
                .iter()
                .chain(&outline[..1])
                .map(|&(x, y)| DVec2::new(x, y)),
        );
        let mut positions: Vec<DVec3> = points.iter().map(|p| p.extend(0.0)).collect();
        let n = points.len() as u32;
        let first_center = n;
        let extra_internal = n + 1;
        positions.push(DVec3::new(2.0, 2.0, 0.0));
        positions.push(DVec3::new(2.5, 4.2, 0.0));
        let offset = positions.len() as u32;
        positions.extend(outline.iter().map(|&(x, y)| DVec3::new(x, y, 2.0)));
        positions.push(DVec3::new(2.0, 2.0, 2.6));
        // Both islands are triangulated, with different fan centers and an
        // extra interior split in the first island.
        let mut faces = Vec::new();
        faces.extend([
            vec![first_center, 0, extra_internal],
            vec![0, 1, extra_internal],
            vec![1, first_center, extra_internal],
        ]);
        for i in 1..n {
            faces.push(vec![first_center, i, (i + 1) % n]);
        }
        for i in 0..outline.len() as u32 {
            faces.push(vec![
                offset + outline.len() as u32,
                offset + i,
                offset + (i + 1) % outline.len() as u32,
            ]);
        }
        let mesh = Mesh::new(positions, &faces).unwrap();
        let mut uvs = points;
        uvs.push(DVec2::new(2.0, 2.0));
        uvs.push(DVec2::new(2.5, 4.2));
        uvs.extend(
            outline
                .iter()
                .map(|&(x, y)| DVec2::new(-y, x) * 0.7 + DVec2::new(10.0, 3.0)),
        );
        uvs.push(DVec2::new(-2.0, 2.0) * 0.7 + DVec2::new(10.0, 3.0));
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: faces.iter().flatten().copied().collect(),
            uvs,
        };
        let islands = Islands::compute(&mesh, &uv);
        assert_eq!(islands.count(), 2);
        let before_mesh = mesh.positions().to_vec();
        let before_internal = uv.uvs.last().copied().unwrap();
        let before_target_border = uv.uvs[offset as usize];
        let mut conform_uv = uv.clone();
        let before_boundary = build_topos(&mesh, &uv, &islands, &[0, 1]);
        assert_eq!(before_boundary[0].boundary_loops(&uv)[0].len(), 11);
        assert_eq!(before_boundary[1].boundary_loops(&uv)[0].len(), 10);
        let report = stack(&mesh, &mut uv, &islands, &[0, 1], StackMode::Similar, 0.01);
        assert_eq!(report.aligned, vec![1]);
        assert_eq!(mesh.positions(), before_mesh);
        let after = build_topos(&mesh, &uv, &islands, &[0, 1]);
        let rings = after
            .iter()
            .map(|t| t.boundary_loops(&uv))
            .collect::<Vec<_>>();
        assert!(loop_distance(&rings[0][0], &rings[1][0]) < 1e-9);
        assert!(loop_distance(&rings[1][0], &rings[0][0]) < 1e-9);
        assert_ne!(uv.uvs.last().copied().unwrap(), before_internal);
        let after_internal = uv.uvs.last().copied().unwrap();
        let after_target_border = uv.uvs[offset as usize];
        let expected = before_internal.distance(before_target_border) * report.matches[0].scale;
        assert!((after_internal.distance(after_target_border) - expected).abs() < 1e-9);
        let conform_report = stack_conform(
            &mesh,
            &mut conform_uv,
            &islands,
            &[0, 1],
            StackMode::Similar,
            0.01,
            4.0,
        );
        assert!(
            conform_report.failed.is_empty(),
            "{:?}",
            conform_report.failed
        );
        assert_eq!(conform_report.aligned, vec![1]);
        let mut approximate_uv = uv.clone();
        approximate_uv.uvs[offset as usize + 4].x += 0.01;
        let approx = stack_conform(
            &mesh,
            &mut approximate_uv,
            &islands,
            &[0, 1],
            StackMode::Similar,
            0.05,
            4.0,
        );
        assert!(approx.failed.is_empty(), "{:?}", approx.failed);
        assert!(overlaid(&mesh, &approximate_uv, &islands, 0, 1, 1e-6));
        let mut flipped_uv = conform_uv.clone();
        *flipped_uv.uvs.last_mut().unwrap() = DVec2::new(30.0, 30.0);
        let unchanged = flipped_uv.clone();
        let rejected = stack_conform(
            &mesh,
            &mut flipped_uv,
            &islands,
            &[0, 1],
            StackMode::Similar,
            0.01,
            4.0,
        );
        assert_eq!(rejected.failed.len(), 1);
        assert_eq!(flipped_uv, unchanged);
    }

    /// A unit cube, one island per face (24 UV vertices), each face's UVs a
    /// unit square at a different offset and rotation.
    fn cube() -> (Mesh, UvLayer) {
        let p: Vec<DVec3> = (0..8)
            .map(|i| DVec3::new((i & 1) as f64, (i >> 1 & 1) as f64, (i >> 2) as f64))
            .collect();
        let faces = [
            [0, 2, 3, 1],
            [4, 5, 7, 6],
            [0, 1, 5, 4],
            [2, 6, 7, 3],
            [0, 4, 6, 2],
            [1, 3, 7, 5],
        ];
        let mesh = Mesh::new(p, &faces).unwrap();
        let mut uvs = Vec::new();
        let square = [DVec2::ZERO, DVec2::X, DVec2::ONE, DVec2::Y];
        for f in 0..6 {
            for c in 0..4 {
                uvs.push(square[(c + f) % 4] * 0.1 + DVec2::new(0.15 * f as f64, 0.0));
            }
        }
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..24).collect(),
            uvs,
        };
        (mesh, uv)
    }

    /// `copies` translated copies of a triangulated asymmetric strip, each an island.
    fn strips(copies: usize) -> (Mesh, UvLayer) {
        let base = [
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(1.0, 0.0, 0.0),
            DVec3::new(2.5, 0.0, 0.2),
            DVec3::new(0.0, 1.0, 0.0),
            DVec3::new(1.2, 1.1, 0.0),
            DVec3::new(2.0, 1.3, 0.5),
        ];
        let tris = [[0, 1, 4], [0, 4, 3], [1, 2, 5], [1, 5, 4]];
        let mut p = Vec::new();
        let mut faces = Vec::new();
        let mut uvs = Vec::new();
        for k in 0..copies {
            let o = p.len() as u32;
            for &q in &base {
                p.push(q + DVec3::new(0.0, 0.0, 5.0 * k as f64));
                // Each copy has its own (distorted, shifted) UVs.
                uvs.push(DVec2::new(q.x, q.y) * (1.0 + 0.3 * k as f64) + DVec2::splat(k as f64));
            }
            for t in tris {
                faces.push(t.map(|v| v + o));
            }
        }
        let corner_uv = faces.iter().flatten().copied().collect();
        let mesh = Mesh::new(p, &faces).unwrap();
        (
            mesh,
            UvLayer {
                name: "uv".into(),
                corner_uv,
                uvs,
            },
        )
    }

    fn island_uvs(mesh: &Mesh, uv: &UvLayer, isl: &Islands, i: usize) -> Vec<(usize, DVec2)> {
        isl.faces(i)
            .iter()
            .flat_map(|&f| mesh.face_corners(f as usize))
            .map(|c| (mesh.corner_vertex(c), uv.uv_at_corner(c)))
            .collect()
    }

    #[test]
    fn cube_faces_stack_exactly() {
        let (mesh, mut uv) = cube();
        let isl = Islands::compute(&mesh, &uv);
        assert_eq!(isl.count(), 6);
        let all: Vec<usize> = (0..6).collect();
        assert_eq!(
            similar_groups(&mesh, &uv, &isl, &all, StackMode::Exact, 0.05),
            vec![all.clone()]
        );
        let r = stack(&mesh, &mut uv, &isl, &all, StackMode::Exact, 0.05);
        assert_eq!(r.exact, vec![1, 2, 3, 4, 5]);
        // Square faces also match mirrored; that must not win and flip them.
        assert!(crate::checks::flipped_faces(&mesh, &uv, &isl).is_empty());
        // Every face now covers exactly the reference square.
        let reference = isl.uv_indices(0, &mesh, &uv);
        let mut ref_pts: Vec<[u64; 2]> = reference
            .iter()
            .map(|&k| uv.uvs[k as usize].to_array().map(f64::to_bits))
            .collect();
        ref_pts.sort();
        for i in 1..6 {
            let mut pts: Vec<[u64; 2]> = isl
                .uv_indices(i, &mesh, &uv)
                .iter()
                .map(|&k| uv.uvs[k as usize].to_array().map(f64::to_bits))
                .collect();
            pts.sort();
            assert_eq!(pts, ref_pts, "island {i}");
        }
        // Unstack spreads them back into a row without overlap.
        let moved = unstack(&mesh, &mut uv, &isl, &all);
        assert_eq!(moved, 5);
        let xs: Vec<f64> = (0..6)
            .map(|i| {
                isl.uv_indices(i, &mesh, &uv)
                    .iter()
                    .map(|&k| uv.uvs[k as usize].x)
                    .fold(f64::INFINITY, f64::min)
            })
            .collect();
        for w in xs.windows(2) {
            assert!(w[1] >= w[0] + 0.1 - 1e-12, "{xs:?}");
        }
        assert_eq!(unstack(&mesh, &mut uv, &isl, &all), 0);
    }

    #[test]
    fn duplicated_meshes_get_identical_uvs_per_vertex() {
        let (mesh, mut uv) = strips(3);
        let isl = Islands::compute(&mesh, &uv);
        assert_eq!(isl.count(), 3);
        let before0 = island_uvs(&mesh, &uv, &isl, 0);
        let r = stack(&mesh, &mut uv, &isl, &[0, 1, 2], StackMode::Exact, 0.05);
        assert_eq!(r.groups, vec![vec![0, 1, 2]]);
        assert_eq!(r.exact, vec![1, 2]);
        assert_eq!(island_uvs(&mesh, &uv, &isl, 0), before0);
        // Corresponding vertices (same offset within each copy) share UVs.
        for i in 1..3 {
            let other: Vec<(usize, DVec2)> = island_uvs(&mesh, &uv, &isl, i)
                .into_iter()
                .map(|(v, p)| (v - 6 * i, p))
                .collect();
            assert_eq!(other, before0);
        }
    }

    #[test]
    fn mirrored_copy_matches_through_a_mirrored_correspondence() {
        // Copy 1 is copy 0 mirrored in x with reversed winding.
        let (mesh0, uv0) = strips(1);
        let mut p: Vec<DVec3> = mesh0.positions().to_vec();
        p.extend(
            mesh0
                .positions()
                .iter()
                .map(|q| DVec3::new(-q.x, q.y, q.z + 3.0)),
        );
        let mut faces: Vec<[u32; 3]> = (0..4)
            .map(|f| {
                let r = mesh0.face_corners(f);
                [r.start, r.start + 1, r.start + 2].map(|c| mesh0.corner_vertex(c) as u32)
            })
            .collect();
        faces.extend(
            faces
                .clone()
                .into_iter()
                .map(|t| [t[0] + 6, t[2] + 6, t[1] + 6]),
        );
        let mesh = Mesh::new(p, &faces).unwrap();
        let mut uvs = uv0.uvs.clone();
        uvs.extend(uv0.uvs.iter().map(|q| *q + DVec2::splat(3.0)));
        let corner_uv = faces.iter().flatten().copied().collect();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv,
            uvs,
        };
        let isl = Islands::compute(&mesh, &uv);
        let r = match_islands(&mesh, &mut uv, &isl, 0, &[1]);
        assert_eq!(r.exact, vec![1]);
        for v in 0..6 {
            assert_eq!(uv.uvs[v + 6], uv.uvs[v]);
        }
    }

    #[test]
    fn similar_shapes_are_aligned_and_different_ones_are_not() {
        // Island 0: a 2×1 rectangle of two quads. Island 1: the same outline
        // as one quad (different topology), rotated 90° and scaled ×0.5 in UV.
        // Island 2: a square of the same 3D area.
        let p = vec![
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(1.0, 0.0, 0.0),
            DVec3::new(2.0, 0.0, 0.0),
            DVec3::new(0.0, 1.0, 0.0),
            DVec3::new(1.0, 1.0, 0.0),
            DVec3::new(2.0, 1.0, 0.0),
            DVec3::new(0.0, 0.0, 3.0),
            DVec3::new(2.0, 0.0, 3.0),
            DVec3::new(2.0, 1.0, 3.0),
            DVec3::new(0.0, 1.0, 3.0),
            DVec3::new(0.0, 0.0, 6.0),
            DVec3::new(2f64.sqrt(), 0.0, 6.0),
            DVec3::new(2f64.sqrt(), 2f64.sqrt(), 6.0),
            DVec3::new(0.0, 2f64.sqrt(), 6.0),
        ];
        let faces = vec![
            vec![0, 1, 4, 3],
            vec![1, 2, 5, 4],
            vec![6, 7, 8, 9],
            vec![10, 11, 12, 13],
        ];
        let mesh = Mesh::new(p.clone(), &faces).unwrap();
        let mut uvs: Vec<DVec2> = p[..6].iter().map(|q| DVec2::new(q.x, q.y)).collect();
        // Rotated 90° and halved: (x, y) ↦ (−y, x)/2 + (5, 5).
        uvs.extend(
            p[6..10]
                .iter()
                .map(|q| DVec2::new(-q.y, q.x) * 0.5 + DVec2::splat(5.0)),
        );
        uvs.extend(
            p[10..]
                .iter()
                .map(|q| DVec2::new(q.x, q.y) + DVec2::splat(9.0)),
        );
        let corner_uv = faces.iter().flatten().copied().collect();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv,
            uvs,
        };
        let isl = Islands::compute(&mesh, &uv);
        assert_eq!(isl.count(), 3);
        let all = [0, 1, 2];
        assert!(similar_groups(&mesh, &uv, &isl, &all, StackMode::Exact, 0.05).is_empty());
        assert_eq!(
            similar_groups(&mesh, &uv, &isl, &all, StackMode::Similar, 0.05),
            vec![vec![0, 1]]
        );
        let r = stack(&mesh, &mut uv, &isl, &all, StackMode::Similar, 0.05);
        assert_eq!(r.aligned, vec![1]);
        let mut corners: Vec<[i64; 2]> = isl
            .uv_indices(1, &mesh, &uv)
            .iter()
            .map(|&k| (uv.uvs[k as usize] * 1e6).round().as_i64vec2().to_array())
            .collect();
        corners.sort();
        assert_eq!(
            corners,
            vec![
                [0, 0],
                [0, 1_000_000],
                [2_000_000, 0],
                [2_000_000, 1_000_000]
            ]
        );
    }

    #[test]
    fn grid_islands_with_symmetry_pick_the_translation() {
        // Two identical 3×3 quad grids translated in 3D; target UVs scrambled.
        let mut p = Vec::new();
        let mut faces = Vec::new();
        for copy in 0..2u32 {
            let o = p.len() as u32;
            for j in 0..4 {
                for i in 0..4 {
                    p.push(DVec3::new(i as f64, j as f64, 10.0 * copy as f64));
                }
            }
            for j in 0..3 {
                for i in 0..3 {
                    let v = o + j * 4 + i;
                    faces.push([v, v + 1, v + 5, v + 4]);
                }
            }
        }
        let mesh = Mesh::new(p.clone(), &faces).unwrap();
        let uvs: Vec<DVec2> = p
            .iter()
            .map(|q| DVec2::new(q.x * 0.1 + q.z, q.y * 0.1 - q.z * q.x))
            .collect();
        let corner_uv = faces.iter().flatten().copied().collect();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv,
            uvs,
        };
        let isl = Islands::compute(&mesh, &uv);
        let r = match_islands(&mesh, &mut uv, &isl, 0, &[1]);
        assert_eq!(r.exact, vec![1]);
        for v in 0..16 {
            assert_eq!(uv.uvs[v + 16], uv.uvs[v], "vertex {v}");
        }
    }

    #[test]
    fn matching_an_island_onto_itself_is_a_no_op() {
        let (mesh, mut uv) = strips(1);
        let isl = Islands::compute(&mesh, &uv);
        let r = match_islands(&mesh, &mut uv, &isl, 0, &[0]);
        assert_eq!(r, MatchReport::default());
    }
}
