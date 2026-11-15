//! Selection queries: grow/shrink, edge loops and rings, shortest paths,
//! borders, select by angle and conversion between element kinds.
//!
//! Selections are boolean masks, one entry per vertex, edge, face or island
//! (see [`Elem`]). Every function is pure and deterministic; the GUI keeps
//! the masks, and the cut commands use these queries to find their edges.

use std::collections::VecDeque;

use glam::DVec3;
use uvcraft_core::{Islands, Mesh, UvLayer};

use crate::analysis::{IslandReport, Report};
use crate::seams;
use serde::{Deserialize, Serialize};

/// UV quality condition used to select whole islands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IslandProblem {
    Overlapping,
    SelfOverlapping,
    Flipped,
    OutsideTile,
    CrossesTile,
}

/// Sorted island indices matching a condition in an analysis report.
pub fn islands_with_problem(report: &Report, problem: IslandProblem) -> Vec<usize> {
    report
        .islands
        .iter()
        .filter(|island| match problem {
            IslandProblem::Overlapping => island.self_overlapping || island.overlaps_other_island,
            IslandProblem::SelfOverlapping => island.self_overlapping,
            IslandProblem::Flipped => island.flipped || island.flipped_triangles > 0,
            IslandProblem::OutsideTile => island.outside_unit_tile,
            IslandProblem::CrossesTile => island.crosses_tile,
        })
        .map(|island| island.index)
        .collect()
}

/// Measured property used to query whole UV islands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IslandMetric {
    TexelDensity,
    MeanAngleDistortion,
    MaxAngleDistortion,
    MeanAreaDistortion,
    MaxAreaDistortion,
}

impl IslandMetric {
    fn value(self, island: &IslandReport) -> f64 {
        match self {
            Self::TexelDensity => island.texel_density,
            Self::MeanAngleDistortion => island.mean_angle_distortion,
            Self::MaxAngleDistortion => island.max_angle_distortion,
            Self::MeanAreaDistortion => island.mean_area_distortion,
            Self::MaxAreaDistortion => island.max_area_distortion,
        }
    }
}

/// Select islands whose measured value lies within the inclusive bounds.
/// The caller validates the bounds before invoking this pure query.
pub fn islands_by_metric(
    report: &Report,
    metric: IslandMetric,
    min: Option<f64>,
    max: Option<f64>,
) -> Vec<usize> {
    report
        .islands
        .iter()
        .filter(|island| {
            let value = metric.value(island);
            value.is_finite()
                && min.is_none_or(|bound| value >= bound)
                && max.is_none_or(|bound| value <= bound)
        })
        .map(|island| island.index)
        .collect()
}

/// The kind of element a selection mask refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elem {
    Vertex,
    Edge,
    Face,
    Island,
}

/// Number of elements of a kind (the length of its masks).
pub fn count(mesh: &Mesh, islands: &Islands, kind: Elem) -> usize {
    match kind {
        Elem::Vertex => mesh.vertex_count(),
        Elem::Edge => mesh.edge_count(),
        Elem::Face => mesh.face_count(),
        Elem::Island => islands.count(),
    }
}

/// Mask of length `n` with the given indices set (out-of-range ones ignored).
pub fn mask(n: usize, indices: &[usize]) -> Vec<bool> {
    let mut m = vec![false; n];
    for &i in indices {
        if i < n {
            m[i] = true;
        }
    }
    m
}

/// Sorted indices set in a mask.
pub fn indices(mask: &[bool]) -> Vec<usize> {
    mask.iter()
        .enumerate()
        .filter(|&(_, &s)| s)
        .map(|(i, _)| i)
        .collect()
}

/// Floods faces from `seed` across manifold edges whose dihedral angle is at
/// most `max_angle_deg`. Degenerate faces stop the flood. The result is sorted.
/// Callers validate `seed` and `max_angle_deg` first.
pub fn faces_by_angle_from_seed(mesh: &Mesh, seed: usize, max_angle_deg: f64) -> Vec<usize> {
    if seed >= mesh.face_count()
        || !max_angle_deg.is_finite()
        || !(0.0..=180.0).contains(&max_angle_deg)
    {
        return Vec::new();
    }
    let valid: Vec<bool> = (0..mesh.face_count())
        .map(|face| {
            let normal = mesh.face_area_normal(face);
            normal.is_finite() && normal.length_squared() > 1e-24
        })
        .collect();
    if !valid[seed] {
        return Vec::new();
    }
    let mut adjacency = vec![Vec::new(); mesh.face_count()];
    for edge in 0..mesh.edge_count() {
        if mesh.is_non_manifold(edge)
            || mesh
                .dihedral_angle_deg(edge)
                .is_none_or(|angle| angle > max_angle_deg + 1e-9)
        {
            continue;
        }
        let (a, Some(b)) = mesh.edge_corners(edge) else {
            continue;
        };
        let (a, b) = (mesh.corner_face(a), mesh.corner_face(b));
        if valid[a] && valid[b] {
            adjacency[a].push(b);
            adjacency[b].push(a);
        }
    }
    let mut selected = vec![false; mesh.face_count()];
    selected[seed] = true;
    let mut queue = VecDeque::from([seed]);
    while let Some(face) = queue.pop_front() {
        for &other in &adjacency[face] {
            if !selected[other] {
                selected[other] = true;
                queue.push_back(other);
            }
        }
    }
    indices(&selected)
}

/// Calls `f(x, y)` for every incident pair of an element `x` of kind `a`
/// and an element `y` of kind `b` (pairs may repeat).
fn incidence(mesh: &Mesh, islands: &Islands, a: Elem, b: Elem, mut f: impl FnMut(usize, usize)) {
    if matches!(
        (a, b),
        (Elem::Vertex, Elem::Edge) | (Elem::Edge, Elem::Vertex)
    ) {
        for e in 0..mesh.edge_count() {
            for v in mesh.edge_vertices(e) {
                if a == Elem::Vertex { f(v, e) } else { f(e, v) }
            }
        }
        return;
    }
    let get = |kind: Elem, c: usize| match kind {
        Elem::Vertex => mesh.corner_vertex(c),
        Elem::Edge => mesh.corner_edge(c),
        Elem::Face => mesh.corner_face(c),
        Elem::Island => islands.island_of_face(mesh.corner_face(c)),
    };
    for c in 0..mesh.corner_count() {
        f(get(a, c), get(b, c));
    }
}

/// Converts a selection between element kinds. With `contained == false`
/// an element is selected when it touches any selected element (e.g. faces
/// around selected vertices); with `contained == true` only when all
/// elements it touches are selected (e.g. faces whose every vertex is
/// selected, or vertices whose every face is selected).
pub fn convert(
    mesh: &Mesh,
    islands: &Islands,
    from: Elem,
    sel: &[bool],
    to: Elem,
    contained: bool,
) -> Vec<bool> {
    if from == to {
        return sel.to_vec();
    }
    let n = count(mesh, islands, to);
    let mut any = vec![false; n];
    let mut all = vec![true; n];
    let mut seen = vec![false; n];
    incidence(mesh, islands, from, to, |x, y| {
        seen[y] = true;
        if sel.get(x).copied().unwrap_or(false) {
            any[y] = true;
        } else {
            all[y] = false;
        }
    });
    (0..n)
        .map(|y| seen[y] && if contained { all[y] } else { any[y] })
        .collect()
}

/// The kind grow/shrink goes through: vertices for faces and edges, edges
/// for vertices.
fn pivot(kind: Elem) -> Elem {
    if kind == Elem::Vertex {
        Elem::Edge
    } else {
        Elem::Vertex
    }
}

/// Adds every element that shares a vertex with the selection (vertices:
/// every neighbor). Islands are returned unchanged.
pub fn grow(mesh: &Mesh, islands: &Islands, kind: Elem, sel: &[bool]) -> Vec<bool> {
    if kind == Elem::Island {
        return sel.to_vec();
    }
    let p = convert(mesh, islands, kind, sel, pivot(kind), false);
    let mut out = convert(mesh, islands, pivot(kind), &p, kind, false);
    for (o, &s) in out.iter_mut().zip(sel) {
        *o |= s;
    }
    out
}

/// Removes every element that shares a vertex with an unselected element
/// (vertices: that have an unselected neighbor). Islands are unchanged.
pub fn shrink(mesh: &Mesh, islands: &Islands, kind: Elem, sel: &[bool]) -> Vec<bool> {
    if kind == Elem::Island {
        return sel.to_vec();
    }
    let p = convert(mesh, islands, kind, sel, pivot(kind), true);
    let mut out = convert(mesh, islands, pivot(kind), &p, kind, true);
    for (o, &s) in out.iter_mut().zip(sel) {
        *o &= s;
    }
    out
}

/// Faces around each vertex, as CSR.
struct Adjacency {
    vertex_face_start: Vec<u32>,
    vertex_faces: Vec<u32>,
}

impl Adjacency {
    fn new(mesh: &Mesh) -> Self {
        let mut start = vec![0u32; mesh.vertex_count() + 1];
        for c in 0..mesh.corner_count() {
            start[mesh.corner_vertex(c) + 1] += 1;
        }
        for i in 0..mesh.vertex_count() {
            start[i + 1] += start[i];
        }
        let mut fill = start.clone();
        let mut faces = vec![0u32; mesh.corner_count()];
        for c in 0..mesh.corner_count() {
            let v = mesh.corner_vertex(c);
            faces[fill[v] as usize] = mesh.corner_face(c) as u32;
            fill[v] += 1;
        }
        Adjacency {
            vertex_face_start: start,
            vertex_faces: faces,
        }
    }

    fn faces(&self, v: usize) -> &[u32] {
        &self.vertex_faces
            [self.vertex_face_start[v] as usize..self.vertex_face_start[v + 1] as usize]
    }
}

fn is_open(mesh: &Mesh, e: usize) -> bool {
    !mesh.is_non_manifold(e) && mesh.edge_corners(e).1.is_none()
}

fn edge_faces(mesh: &Mesh, e: usize) -> [Option<usize>; 2] {
    let (c0, c1) = mesh.edge_corners(e);
    [Some(mesh.corner_face(c0)), c1.map(|c| mesh.corner_face(c))]
}

fn share_face(mesh: &Mesh, a: usize, b: usize) -> bool {
    let fa = edge_faces(mesh, a);
    edge_faces(mesh, b)
        .iter()
        .any(|f| f.is_some() && fa.contains(f))
}

fn direction(mesh: &Mesh, from: usize, to: usize) -> DVec3 {
    (mesh.position(to) - mesh.position(from)).normalize_or_zero()
}

/// Minimum cosine between consecutive edges for the triangle fallbacks.
const STRAIGHT_COS: f64 = 0.7;

/// The edge continuing the loop through `cur` at its endpoint `v`.
fn next_loop_edge(mesh: &Mesh, adj: &Adjacency, cur: usize, v: usize) -> Option<usize> {
    let inc = mesh.vertex_edges(v).iter().map(|&e| e as usize);
    if mesh.is_non_manifold(cur) {
        return None;
    }
    if is_open(mesh, cur) {
        // Border loops follow the border while it does not branch.
        let mut next = inc.filter(|&x| x != cur && is_open(mesh, x));
        return match (next.next(), next.next()) {
            (Some(x), None) => Some(x),
            _ => None,
        };
    }
    let faces = adj.faces(v);
    let all_quads = faces
        .iter()
        .all(|&f| mesh.face_corners(f as usize).len() == 4);
    if all_quads {
        // Regular quad vertex: the one edge sharing no face with `cur`.
        let regular = faces.len() == 4
            && mesh.vertex_edges(v).len() == 4
            && inc
                .clone()
                .all(|x| !is_open(mesh, x) && !mesh.is_non_manifold(x));
        if !regular {
            return None; // pole or border vertex: quad loops stop here
        }
        return inc.into_iter().find(|&x| !share_face(mesh, cur, x));
    }
    // Triangles: the straightest edge that does not share a face with `cur`.
    let d = direction(mesh, mesh.edge_other(cur, v), v);
    inc.filter(|&x| x != cur && !is_open(mesh, x) && !mesh.is_non_manifold(x))
        .filter(|&x| !share_face(mesh, cur, x))
        .map(|x| (x, d.dot(direction(mesh, v, mesh.edge_other(x, v)))))
        .filter(|&(_, cos)| cos >= STRAIGHT_COS)
        .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)))
        .map(|(x, _)| x)
}

/// Edge loop through `e`: on quads, straight through every regular
/// (valence 4) vertex, stopping at poles and borders; open (border) edges
/// follow the border; on triangles, the straightest continuation (within
/// ~45°). Edges are returned in order along the loop.
pub fn edge_loop(mesh: &Mesh, e: usize) -> Vec<usize> {
    let adj = Adjacency::new(mesh);
    let mut visited = vec![false; mesh.edge_count()];
    visited[e] = true;
    let mut walks: [Vec<usize>; 2] = [Vec::new(), Vec::new()];
    for (side, &start) in mesh.edge_vertices(e).iter().rev().enumerate() {
        let (mut cur, mut v) = (e, start);
        while let Some(next) = next_loop_edge(mesh, &adj, cur, v) {
            if std::mem::replace(&mut visited[next], true) {
                break;
            }
            walks[side].push(next);
            v = mesh.edge_other(next, v);
            cur = next;
        }
    }
    let [fwd, back] = walks;
    back.into_iter()
        .rev()
        .chain(std::iter::once(e))
        .chain(fwd)
        .collect()
}

/// The corner opposite corner `c` across its face, for edge rings.
fn ring_opposite(mesh: &Mesh, c: usize) -> Option<usize> {
    let face = mesh.face_corners(mesh.corner_face(c));
    let n = face.len();
    if n.is_multiple_of(2) {
        let mut o = c;
        for _ in 0..n / 2 {
            o = mesh.next(o);
        }
        return Some(o);
    }
    if n != 3 {
        return None;
    }
    // Triangle: pair it with the neighbor across its longest other edge
    // (the likely diagonal of a split quad) and take the edge opposite `c`'s
    // in that quad, if it runs roughly parallel.
    let [a, b] = [mesh.corner_vertex(c), mesh.corner_vertex(mesh.next(c))];
    let d = direction(mesh, a, b);
    let mut sides = [mesh.next(c), mesh.prev(c)];
    sides.sort_by(|&p, &q| {
        let len = |k: usize| mesh.edge_length(mesh.corner_edge(k));
        len(q).total_cmp(&len(p)).then(p.cmp(&q))
    });
    sides
        .into_iter()
        .filter_map(|k| mesh.twin(k))
        .filter(|&t| mesh.face_corners(mesh.corner_face(t)).len() == 3)
        .filter_map(|t| {
            [mesh.next(t), mesh.prev(t)].into_iter().find(|&o| {
                let [x, y] = [mesh.corner_vertex(o), mesh.corner_vertex(mesh.next(o))];
                ![a, b].contains(&x) && ![a, b].contains(&y)
            })
        })
        .find(|&o| {
            let [x, y] = [mesh.corner_vertex(o), mesh.corner_vertex(mesh.next(o))];
            d.dot(direction(mesh, x, y)).abs() >= STRAIGHT_COS
        })
}

/// Edge ring through `e`: across each quad (or even polygon) to the
/// opposite edge, on both sides, until a border, a revisit or an odd
/// polygon. Triangles are paired with a neighbor into a quad. Edges are
/// returned in order across the ring.
pub fn edge_ring(mesh: &Mesh, e: usize) -> Vec<usize> {
    let mut visited = vec![false; mesh.edge_count()];
    visited[e] = true;
    let mut walks: [Vec<usize>; 2] = [Vec::new(), Vec::new()];
    if mesh.is_non_manifold(e) {
        return vec![e];
    }
    let (c0, c1) = mesh.edge_corners(e);
    for (side, start) in [Some(c0), c1].into_iter().enumerate() {
        let mut cur = start;
        while let Some(c) = cur {
            let Some(o) = ring_opposite(mesh, c) else {
                break;
            };
            let eo = mesh.corner_edge(o);
            if std::mem::replace(&mut visited[eo], true) {
                break;
            }
            walks[side].push(eo);
            cur = if mesh.is_non_manifold(eo) {
                None
            } else {
                mesh.twin(o)
            };
        }
    }
    let [fwd, back] = walks;
    back.into_iter()
        .rev()
        .chain(std::iter::once(e))
        .chain(fwd)
        .collect()
}

/// Shortest path (3D length) through `vertices` in order: the edges of each
/// leg, concatenated. `None` when two consecutive vertices are not connected.
pub fn path_through(mesh: &Mesh, vertices: &[usize]) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    for w in vertices.windows(2) {
        out.extend(seams::vertex_path(mesh, w[0], w[1])?);
    }
    Some(out)
}

/// Shortest path joining two edges: both edges plus the shortest path
/// between their nearest endpoints.
pub fn edge_path(mesh: &Mesh, from: usize, to: usize) -> Option<Vec<usize>> {
    if from == to {
        return Some(vec![from]);
    }
    let targets = mesh.edge_vertices(to);
    let path = seams::shortest_path(
        mesh,
        &mesh.edge_vertices(from),
        |v| targets.contains(&v),
        |_| true,
    )?;
    let mut out = vec![from];
    out.extend(path.into_iter().filter(|&x| x != from && x != to));
    out.push(to);
    Some(out)
}

/// Open edges of the 3D mesh (one face).
pub fn open_edges(mesh: &Mesh) -> Vec<bool> {
    (0..mesh.edge_count()).map(|e| is_open(mesh, e)).collect()
}

/// Island boundary edges (seams, open and non-manifold edges). With a face
/// mask, only those bounding a selected face.
pub fn island_border_edges(mesh: &Mesh, uv: &UvLayer, faces: Option<&[bool]>) -> Vec<bool> {
    let mut out: Vec<bool> = (0..mesh.edge_count())
        .map(|e| uv.edge_kind(mesh, e).is_island_boundary())
        .collect();
    if let Some(sel) = faces {
        let mut touched = vec![false; mesh.edge_count()];
        for c in 0..mesh.corner_count() {
            if sel.get(mesh.corner_face(c)).copied().unwrap_or(false) {
                touched[mesh.corner_edge(c)] = true;
            }
        }
        for (o, t) in out.iter_mut().zip(touched) {
            *o &= t;
        }
    }
    out
}

/// Border of a face selection: edges between a selected and an unselected
/// face (open edges of the mesh are not included).
pub fn selection_border(mesh: &Mesh, faces: &[bool]) -> Vec<bool> {
    let mut sel = vec![false; mesh.edge_count()];
    let mut unsel = vec![false; mesh.edge_count()];
    for c in 0..mesh.corner_count() {
        let e = mesh.corner_edge(c);
        if faces.get(mesh.corner_face(c)).copied().unwrap_or(false) {
            sel[e] = true;
        } else {
            unsel[e] = true;
        }
    }
    sel.iter().zip(unsel).map(|(&a, b)| a && b).collect()
}

/// Flood fill from the seed faces across interior manifold edges whose
/// dihedral angle is at most `angle_deg` (smooth regions grow, creases stop
/// the fill). The seeds are always included.
pub fn faces_by_angle(mesh: &Mesh, seeds: &[bool], angle_deg: f64) -> Vec<bool> {
    let mut out: Vec<bool> = (0..mesh.face_count())
        .map(|f| seeds.get(f).copied().unwrap_or(false))
        .collect();
    let mut queue: VecDeque<usize> = indices(&out).into();
    while let Some(f) = queue.pop_front() {
        for c in mesh.face_corners(f) {
            let e = mesh.corner_edge(c);
            let Some(t) = mesh.twin(c) else { continue };
            let g = mesh.corner_face(t);
            if !out[g] && mesh.dihedral_angle_deg(e).is_some_and(|a| a <= angle_deg) {
                out[g] = true;
                queue.push_back(g);
            }
        }
    }
    out
}

/// Edges whose dihedral angle is at least `angle_deg` (open edges excluded).
pub fn edges_by_angle(mesh: &Mesh, angle_deg: f64) -> Vec<bool> {
    (0..mesh.edge_count())
        .map(|e| mesh.dihedral_angle_deg(e).is_some_and(|a| a >= angle_deg))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seams::tests::{column, grid};

    /// Edge between two grid vertices.
    fn edge(m: &Mesh, a: usize, b: usize) -> usize {
        m.vertex_edges(a)
            .iter()
            .map(|&e| e as usize)
            .find(|&e| m.edge_other(e, a) == b)
            .unwrap()
    }

    /// n×n grid with each quad split into two triangles along the same diagonal.
    fn tri_grid(n: usize) -> Mesh {
        let (q, _) = grid(n);
        let faces: Vec<[u32; 3]> = (0..q.face_count())
            .flat_map(|f| {
                let c: Vec<u32> = q
                    .face_corners(f)
                    .map(|c| q.corner_vertex(c) as u32)
                    .collect();
                [[c[0], c[1], c[2]], [c[0], c[2], c[3]]]
            })
            .collect();
        Mesh::new(q.positions().to_vec(), &faces).unwrap()
    }

    /// Closed quad cylinder (tube) with `n` around and `k` rows.
    fn tube(n: usize, k: usize) -> Mesh {
        let mut p = Vec::new();
        for j in 0..=k {
            for i in 0..n {
                let a = std::f64::consts::TAU * i as f64 / n as f64;
                p.push(DVec3::new(a.cos(), j as f64, a.sin()));
            }
        }
        let v = |i: usize, j: usize| (j * n + i % n) as u32;
        let faces: Vec<[u32; 4]> = (0..k)
            .flat_map(|j| (0..n).map(move |i| [v(i, j), v(i + 1, j), v(i + 1, j + 1), v(i, j + 1)]))
            .collect();
        Mesh::new(p, &faces).unwrap()
    }

    #[test]
    fn quad_loop_and_ring() {
        let (m, _) = grid(4);
        // Vertical line x = 2 crosses the whole grid: 4 edges.
        let e = edge(&m, 7, 12);
        let mut l = edge_loop(&m, e);
        assert_eq!(l.len(), 4);
        let mut col = column(&m, 4, 2);
        l.sort_unstable();
        col.sort_unstable();
        assert_eq!(l, col);
        // Its ring: the vertical edges between rows 1 and 2.
        let r = edge_ring(&m, e);
        assert_eq!(r.len(), 5);
        assert!(r.iter().all(|&x| {
            let [a, b] = m.edge_vertices(x);
            a / 5 == 1 && b / 5 == 2 || a / 5 == 2 && b / 5 == 1
        }));
        // Border edge: follows the whole open boundary (16 edges).
        assert_eq!(edge_loop(&m, edge(&m, 0, 1)).len(), 16);
    }

    #[test]
    fn closed_loop_on_tube() {
        let m = tube(8, 3);
        // A ring edge around the middle row closes on itself.
        let e = edge(&m, 8, 9);
        let l = edge_loop(&m, e);
        assert_eq!(l.len(), 8);
        let r = edge_ring(&m, e);
        assert_eq!(r.len(), 4, "rings across the rows to both borders");
        let vertical = edge_ring(&m, edge(&m, 8, 16));
        assert_eq!(vertical.len(), 8, "closed ring around the tube");
    }

    #[test]
    fn triangle_fallbacks() {
        let m = tri_grid(4);
        let e = edge(&m, 6, 7); // horizontal, row y = 1
        let l = edge_loop(&m, e);
        assert_eq!(l.len(), 4, "straight across the row: {l:?}");
        assert!(
            l.iter()
                .all(|&x| m.edge_vertices(x).iter().all(|v| v / 5 == 1))
        );
        let r = edge_ring(&m, edge(&m, 7, 12)); // vertical at x = 2
        assert_eq!(r.len(), 5, "{r:?}");
        assert!(r.iter().all(|&x| {
            let [a, b] = m.edge_vertices(x);
            (a / 5).abs_diff(b / 5) == 1 && a % 5 == b % 5
        }));
    }

    #[test]
    fn grow_shrink_convert() {
        let (m, uv) = grid(4);
        let isl = Islands::compute(&m, &uv);
        // Away from the mesh border, shrink undoes grow. Mesh borders are
        // not selection borders: a full selection does not shrink.
        {
            let (m6, uv6) = grid(6);
            let isl6 = Islands::compute(&m6, &uv6);
            let one = mask(m6.face_count(), &[14]);
            let g = grow(&m6, &isl6, Elem::Face, &one);
            assert_eq!(indices(&g).len(), 9);
            assert_eq!(shrink(&m6, &isl6, Elem::Face, &g), one);
        }
        let one = mask(m.face_count(), &[5]);
        let all = vec![true; m.face_count()];
        assert_eq!(shrink(&m, &isl, Elem::Face, &all), all);
        assert_eq!(indices(&shrink(&m, &isl, Elem::Face, &one)).len(), 0);
        let v = mask(m.vertex_count(), &[12]);
        assert_eq!(indices(&grow(&m, &isl, Elem::Vertex, &v)).len(), 5);
        let e = mask(m.edge_count(), &[edge(&m, 12, 13)]);
        assert_eq!(indices(&grow(&m, &isl, Elem::Edge, &e)).len(), 7);

        // Face → vertices → faces (contained) round-trips.
        let verts = convert(&m, &isl, Elem::Face, &one, Elem::Vertex, false);
        assert_eq!(indices(&verts), vec![6, 7, 11, 12]);
        assert_eq!(
            convert(&m, &isl, Elem::Vertex, &verts, Elem::Face, true),
            one
        );
        assert_eq!(
            indices(&convert(&m, &isl, Elem::Vertex, &verts, Elem::Face, false)).len(),
            9
        );
        let edges = convert(&m, &isl, Elem::Face, &one, Elem::Edge, false);
        assert_eq!(indices(&edges).len(), 4);
        assert_eq!(
            convert(&m, &isl, Elem::Face, &one, Elem::Island, false),
            vec![true]
        );
        assert!(convert(&m, &isl, Elem::Face, &one, Elem::Island, true) == vec![false]);
        assert_eq!(
            convert(&m, &isl, Elem::Island, &[true], Elem::Face, false),
            all
        );
    }

    #[test]
    fn paths_and_borders() {
        let (m, mut uv) = grid(4);
        assert_eq!(path_through(&m, &[0, 4, 24]).unwrap().len(), 8);
        let p = edge_path(&m, edge(&m, 0, 1), edge(&m, 3, 4)).unwrap();
        assert_eq!(p.len(), 4);
        assert_eq!(indices(&open_edges(&m)).len(), 16);

        let faces = mask(m.face_count(), &[5, 6]);
        let border = selection_border(&m, &faces);
        assert_eq!(indices(&border).len(), 6);
        seams::cut_edges(&m, &mut uv, &indices(&border));
        // Cutting the border of an interior region does not split it until it closes.
        let isl = Islands::compute(&m, &uv);
        assert_eq!(isl.count(), 2);
        let b = island_border_edges(&m, &uv, Some(&faces));
        assert_eq!(indices(&b), indices(&border));
        assert_eq!(indices(&island_border_edges(&m, &uv, None)).len(), 16 + 6);
    }

    #[test]
    fn edge_path_is_stable_and_stays_within_connected_component() {
        let (mesh, _) = grid(4);
        let a = edge(&mesh, 0, 1);
        let b = edge(&mesh, 3, 4);
        let path = edge_path(&mesh, a, b).unwrap();
        for _ in 0..8 {
            assert_eq!(edge_path(&mesh, a, b), Some(path.clone()));
        }
        assert_eq!(path.first(), Some(&a));
        assert_eq!(path.last(), Some(&b));

        let positions = vec![
            DVec3::ZERO,
            DVec3::X,
            DVec3::Y,
            DVec3::new(10.0, 0.0, 0.0),
            DVec3::new(11.0, 0.0, 0.0),
            DVec3::new(10.0, 1.0, 0.0),
        ];
        let disconnected = Mesh::new(positions, &[[0, 1, 2], [3, 4, 5]]).unwrap();
        let first = disconnected.corner_edge(0);
        let second = disconnected.corner_edge(3);
        assert_eq!(edge_path(&disconnected, first, second), None);
    }

    #[test]
    fn angle_flood() {
        let m = tube(8, 2);
        // Neighbouring faces of an octagonal tube meet at 45°.
        let seed = mask(m.face_count(), &[0]);
        assert_eq!(
            indices(&faces_by_angle(&m, &seed, 30.0)).len(),
            2,
            "rows only"
        );
        assert_eq!(indices(&faces_by_angle(&m, &seed, 50.0)).len(), 16);
        assert_eq!(indices(&edges_by_angle(&m, 40.0)).len(), 16);
    }
}
