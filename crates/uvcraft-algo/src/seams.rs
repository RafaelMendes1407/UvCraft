//! Seam editing: cut, weld, automatic seams and shortest paths.
//!
//! A seam is an edge whose two faces use different UV vertices, so cutting
//! means splitting UV vertices and welding means merging them.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

use glam::DVec2;
use uvcraft_core::{EdgeKind, Islands, Mesh, UvLayer};

fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        let p = parent[x as usize];
        parent[x as usize] = parent[p as usize];
        x = p;
    }
    x
}

fn union(parent: &mut [u32], a: u32, b: u32) {
    let (a, b) = (find(parent, a), find(parent, b));
    if a != b {
        parent[a.max(b) as usize] = a.min(b);
    }
}

/// For an edge with two faces: corner pairs that sit on the same mesh vertex
/// on either side, `[(a0, a1), (b0, b1)]`.
pub fn edge_corner_pairs(mesh: &Mesh, e: usize) -> Option<[(usize, usize); 2]> {
    let (c0, c1) = mesh.edge_corners(e);
    let c1 = c1?;
    let (a0, b0) = (c0, mesh.next(c0));
    Some(if mesh.corner_vertex(c1) == mesh.corner_vertex(c0) {
        [(a0, c1), (b0, mesh.next(c1))]
    } else {
        [(a0, mesh.next(c1)), (b0, c1)]
    })
}

/// Cuts edges: UV vertices are split wherever the cut separates the faces
/// around them. A cut edge whose endpoints stay connected around the vertex
/// (an isolated interior edge) has no effect until the cut reaches a border.
/// Returns the number of UV vertices added.
pub fn cut_edges(mesh: &Mesh, uv: &mut UvLayer, edges: &[usize]) -> usize {
    let mut cut = vec![false; mesh.edge_count()];
    let mut affected: HashSet<u32> = HashSet::new();
    for &e in edges {
        if let Some(pairs) = edge_corner_pairs(mesh, e)
            && uv.edge_kind(mesh, e) == EdgeKind::Interior
        {
            cut[e] = true;
            for (x, _) in pairs {
                affected.insert(uv.corner_uv[x]);
            }
        }
    }
    if affected.is_empty() {
        return 0;
    }

    // Corners stay together when connected through a non-cut interior edge.
    let mut parent: Vec<u32> = (0..mesh.corner_count() as u32).collect();
    for (e, &is_cut) in cut.iter().enumerate() {
        if is_cut || uv.edge_kind(mesh, e) != EdgeKind::Interior {
            continue;
        }
        if let Some(pairs) = edge_corner_pairs(mesh, e) {
            for (x, y) in pairs {
                union(&mut parent, x as u32, y as u32);
            }
        }
    }

    let before = uv.uvs.len();
    let mut claimed: HashMap<u32, u32> = HashMap::new(); // uv index → group
    let mut group_index: HashMap<u32, u32> = HashMap::new(); // group → uv index
    for c in 0..mesh.corner_count() {
        let idx = uv.corner_uv[c];
        if !affected.contains(&idx) {
            continue;
        }
        let g = find(&mut parent, c as u32);
        if let Some(&i) = group_index.get(&g) {
            uv.corner_uv[c] = i;
            continue;
        }
        let i = match claimed.get(&idx) {
            None => {
                claimed.insert(idx, g);
                idx
            }
            Some(_) => {
                uv.uvs.push(uv.uvs[idx as usize]);
                uv.uvs.len() as u32 - 1
            }
        };
        group_index.insert(g, i);
        uv.corner_uv[c] = i;
    }
    uv.uvs.len() - before
}

/// Signed UV area of an island.
fn island_signed_area(mesh: &Mesh, uv: &UvLayer, islands: &Islands, i: usize) -> f64 {
    islands
        .faces(i)
        .iter()
        .flat_map(|&f| mesh.fan_triangles(f as usize))
        .map(|t| {
            let [a, b, c] = t.map(|c| uv.uv_at_corner(c));
            0.5 * (b - a).perp_dot(c - a)
        })
        .sum()
}

/// Welds seam edges. When an edge joins two islands, the smaller one is first
/// moved rigidly (mirrored if needed) onto the other so the shared edges line
/// up; then the UV vertices are merged at their average position.
/// Returns the number of edges welded.
pub fn weld_edges(mesh: &Mesh, uv: &mut UvLayer, edges: &[usize]) -> usize {
    let mut remaining: Vec<usize> = edges
        .iter()
        .copied()
        .filter(|&e| uv.edge_kind(mesh, e) == EdgeKind::Seam)
        .collect();
    remaining.sort_unstable();
    remaining.dedup();
    let welded = remaining.len();

    while let Some(&first) = remaining.first() {
        let islands = Islands::compute(mesh, uv);
        let side = |e: usize| {
            let (c0, c1) = mesh.edge_corners(e);
            let i0 = islands.island_of_face(mesh.corner_face(c0));
            let i1 = islands.island_of_face(mesh.corner_face(c1.expect("seams have two faces")));
            (i0.min(i1), i0.max(i1))
        };
        let key = side(first);
        let (group, rest): (Vec<usize>, Vec<usize>) =
            remaining.iter().partition(|&&e| side(e) == key);
        remaining = rest;

        let pairs: Vec<(usize, usize)> = group
            .iter()
            .flat_map(|&e| edge_corner_pairs(mesh, e).expect("seams have two faces"))
            .collect();

        if key.0 != key.1 {
            // Move the island with fewer faces onto the other.
            let (fixed, moving) = if islands.faces(key.0).len() >= islands.faces(key.1).len() {
                key
            } else {
                (key.1, key.0)
            };
            let moving_ids = islands.uv_indices(moving, mesh, uv);
            // Point pairs (moving, fixed).
            let pts: Vec<(DVec2, DVec2)> = pairs
                .iter()
                .map(|&(x, y)| {
                    let (x, y) = if islands.island_of_face(mesh.corner_face(x)) == moving {
                        (x, y)
                    } else {
                        (y, x)
                    };
                    (uv.uv_at_corner(x), uv.uv_at_corner(y))
                })
                .collect();
            let mirror = (island_signed_area(mesh, uv, &islands, fixed) < 0.0)
                != (island_signed_area(mesh, uv, &islands, moving) < 0.0);
            let flip = |p: DVec2| if mirror { DVec2::new(-p.x, p.y) } else { p };
            let n = pts.len() as f64;
            let cm = pts.iter().map(|p| flip(p.0)).sum::<DVec2>() / n;
            let cf = pts.iter().map(|p| p.1).sum::<DVec2>() / n;
            let (mut cross, mut dot) = (0.0, 0.0);
            for &(m, f) in &pts {
                let (m, f) = (flip(m) - cm, f - cf);
                cross += m.perp_dot(f);
                dot += m.dot(f);
            }
            let rot = DVec2::from_angle(cross.atan2(dot));
            for k in moving_ids {
                let p = &mut uv.uvs[k as usize];
                *p = cf + rot.rotate(flip(*p) - cm);
            }
        }

        // Merge UV vertices, placing each merged vertex at the average.
        let mut parent: Vec<u32> = (0..uv.uvs.len() as u32).collect();
        for &(x, y) in &pairs {
            union(&mut parent, uv.corner_uv[x], uv.corner_uv[y]);
        }
        let mut sum: HashMap<u32, (DVec2, f64)> = HashMap::new();
        let mut members: HashSet<u32> = HashSet::new();
        for &(x, y) in &pairs {
            members.insert(uv.corner_uv[x]);
            members.insert(uv.corner_uv[y]);
        }
        for &m in &members {
            let r = find(&mut parent, m);
            let s = sum.entry(r).or_insert((DVec2::ZERO, 0.0));
            s.0 += uv.uvs[m as usize];
            s.1 += 1.0;
        }
        for (&r, &(p, n)) in &sum {
            uv.uvs[r as usize] = p / n;
        }
        for i in uv.corner_uv.iter_mut() {
            *i = find(&mut parent, *i);
        }
    }
    uv.compact();
    welded
}

/// Interior edges whose dihedral angle exceeds `angle_deg`.
pub fn sharp_edges(mesh: &Mesh, angle_deg: f64) -> Vec<usize> {
    (0..mesh.edge_count())
        .filter(|&e| mesh.dihedral_angle_deg(e).is_some_and(|a| a > angle_deg))
        .collect()
}

/// Dijkstra over mesh edges (3D length) from any of `sources` to the first
/// vertex where `is_target` holds, using only edges where `allowed` holds.
/// Returns the edges of the path.
pub fn shortest_path(
    mesh: &Mesh,
    sources: &[usize],
    is_target: impl Fn(usize) -> bool,
    allowed: impl Fn(usize) -> bool,
) -> Option<Vec<usize>> {
    let mut dist = vec![f64::INFINITY; mesh.vertex_count()];
    let mut via = vec![u32::MAX; mesh.vertex_count()];
    let mut heap = BinaryHeap::new();
    for &s in sources {
        dist[s] = 0.0;
        heap.push((Reverse(OrdF64(0.0)), s));
    }
    while let Some((Reverse(OrdF64(d)), v)) = heap.pop() {
        if d > dist[v] {
            continue;
        }
        if is_target(v) {
            let mut path = Vec::new();
            let mut cur = v;
            while via[cur] != u32::MAX {
                let e = via[cur] as usize;
                path.push(e);
                cur = mesh.edge_other(e, cur);
            }
            path.reverse();
            return Some(path);
        }
        for &e in mesh.vertex_edges(v) {
            let e = e as usize;
            if !allowed(e) {
                continue;
            }
            let w = mesh.edge_other(e, v);
            let nd = d + mesh.edge_length(e);
            if nd < dist[w] {
                dist[w] = nd;
                via[w] = e as u32;
                heap.push((Reverse(OrdF64(nd)), w));
            }
        }
    }
    None
}

/// Shortest path between two vertices along any mesh edges.
pub fn vertex_path(mesh: &Mesh, from: usize, to: usize) -> Option<Vec<usize>> {
    if from == to {
        return Some(Vec::new());
    }
    shortest_path(mesh, &[from], |v| v == to, |_| true)
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

/// Boundary loops of an island, as groups of mesh vertices.
fn boundary_loops(mesh: &Mesh, uv: &UvLayer, faces: &[u32]) -> Vec<Vec<usize>> {
    let mut verts: Vec<usize> = Vec::new();
    let mut edges: Vec<[usize; 2]> = Vec::new();
    for &f in faces {
        for c in mesh.face_corners(f as usize) {
            let e = mesh.corner_edge(c);
            if uv.edge_kind(mesh, e).is_island_boundary() {
                let (a, b) = (mesh.corner_vertex(c), mesh.corner_vertex(mesh.next(c)));
                verts.extend([a, b]);
                edges.push([a, b]);
            }
        }
    }
    verts.sort_unstable();
    verts.dedup();
    let local: HashMap<usize, u32> = verts
        .iter()
        .enumerate()
        .map(|(i, &v)| (v, i as u32))
        .collect();
    let mut parent: Vec<u32> = (0..verts.len() as u32).collect();
    for [a, b] in edges {
        union(&mut parent, local[&a], local[&b]);
    }
    let mut groups: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, &v) in verts.iter().enumerate() {
        let r = find(&mut parent, i as u32);
        groups.entry(r).or_default().push(v);
    }
    let mut loops: Vec<Vec<usize>> = groups.into_values().collect();
    loops.sort_by_key(|l| l[0]);
    loops
}

/// Cuts islands until each has exactly one boundary loop, so it can be
/// flattened: closed islands get a slit between two far-apart vertices, and
/// islands with several loops (tubes) get cuts joining the loops along
/// shortest paths. Handles (genus > 0) are not cut yet.
/// Returns all edges cut.
pub fn cut_to_disk(mesh: &Mesh, uv: &mut UvLayer, targets: &[usize]) -> Vec<usize> {
    let mut all_cuts = Vec::new();
    // Islands are renumbered after each cut; track targets by a face.
    let islands = Islands::compute(mesh, uv);
    let seeds: Vec<usize> = targets
        .iter()
        .map(|&i| islands.faces(i)[0] as usize)
        .collect();
    for seed in seeds {
        for _ in 0..64 {
            let islands = Islands::compute(mesh, uv);
            let island = islands.island_of_face(seed);
            let faces = islands.faces(island).to_vec();
            let in_island: HashSet<usize> = faces.iter().map(|&f| f as usize).collect();
            let interior = |e: usize| {
                uv.edge_kind(mesh, e) == EdgeKind::Interior
                    && in_island.contains(&mesh.corner_face(mesh.edge_corners(e).0))
            };
            let loops = boundary_loops(mesh, uv, &faces);
            let path = match loops.len() {
                1 => break,
                0 => {
                    let start = mesh.corner_vertex(mesh.face_corners(faces[0] as usize).start);
                    let far = farthest(mesh, start, interior);
                    let far2 = farthest(mesh, far, interior);
                    shortest_path(mesh, &[far], |v| v == far2, interior)
                }
                _ => {
                    let others: HashSet<usize> = loops[1..].iter().flatten().copied().collect();
                    shortest_path(mesh, &loops[0], |v| others.contains(&v), interior)
                }
            };
            match path {
                Some(p) if !p.is_empty() => {
                    cut_edges(mesh, uv, &p);
                    all_cuts.extend(p);
                }
                _ => break,
            }
        }
    }
    all_cuts
}

fn farthest(mesh: &Mesh, from: usize, allowed: impl Fn(usize) -> bool) -> usize {
    let mut dist = vec![f64::INFINITY; mesh.vertex_count()];
    let mut heap = BinaryHeap::new();
    dist[from] = 0.0;
    heap.push((Reverse(OrdF64(0.0)), from));
    let mut best = (0.0, from);
    while let Some((Reverse(OrdF64(d)), v)) = heap.pop() {
        if d > dist[v] {
            continue;
        }
        if d > best.0 {
            best = (d, v);
        }
        for &e in mesh.vertex_edges(v) {
            let e = e as usize;
            if !allowed(e) {
                continue;
            }
            let w = mesh.edge_other(e, v);
            if d + mesh.edge_length(e) < dist[w] {
                dist[w] = d + mesh.edge_length(e);
                heap.push((Reverse(OrdF64(dist[w])), w));
            }
        }
    }
    best.1
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use glam::DVec3;

    /// n×n grid of quads in the XY plane with UVs = positions / n, fully welded.
    pub fn grid(n: usize) -> (Mesh, UvLayer) {
        let mut p = Vec::new();
        for y in 0..=n {
            for x in 0..=n {
                p.push(DVec3::new(x as f64, y as f64, 0.0));
            }
        }
        let w = n + 1;
        let mut faces = Vec::new();
        for y in 0..n {
            for x in 0..n {
                let i = (y * w + x) as u32;
                faces.push(vec![i, i + 1, i + 1 + w as u32, i + w as u32]);
            }
        }
        let uvs = p.iter().map(|q: &DVec3| q.truncate() / n as f64).collect();
        let mesh = Mesh::new(p, &faces).unwrap();
        let corner_uv = (0..mesh.corner_count())
            .map(|c| mesh.corner_vertex(c) as u32)
            .collect();
        (
            mesh,
            UvLayer {
                name: "uv".into(),
                corner_uv,
                uvs,
            },
        )
    }

    /// Edges of the grid's vertical line x = col.
    pub fn column(mesh: &Mesh, n: usize, col: usize) -> Vec<usize> {
        let w = n + 1;
        (0..n)
            .map(|y| {
                let a = y * w + col;
                let b = a + w;
                mesh.vertex_edges(a)
                    .iter()
                    .map(|&e| e as usize)
                    .find(|&e| mesh.edge_other(e, a) == b)
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn cut_across_splits_island_and_weld_restores_it() {
        let (m, mut uv) = grid(3);
        let cut = column(&m, 3, 1);
        assert_eq!(cut_edges(&m, &mut uv, &cut), 4);
        assert_eq!(Islands::compute(&m, &uv).count(), 2);
        for &e in &cut {
            assert_eq!(uv.edge_kind(&m, e), EdgeKind::Seam);
        }

        // Move the small island away, then weld: it snaps back.
        let isl = Islands::compute(&m, &uv);
        let small = (0..2).min_by_key(|&i| isl.faces(i).len()).unwrap();
        for k in isl.uv_indices(small, &m, &uv) {
            let p = uv.uvs[k as usize];
            uv.uvs[k as usize] = DVec2::new(-p.y, p.x) + DVec2::new(5.0, 0.0);
        }
        let before = grid(3).1;
        assert_eq!(weld_edges(&m, &mut uv, &cut), 3);
        assert_eq!(Islands::compute(&m, &uv).count(), 1);
        assert_eq!(uv.uvs.len(), before.uvs.len());
        for c in 0..m.corner_count() {
            assert!((uv.uv_at_corner(c) - before.uv_at_corner(c)).length() < 1e-9);
        }
    }

    #[test]
    fn interior_slit_splits_only_inner_vertices() {
        let (m, mut uv) = grid(4);
        // Two edges in the middle of column 2 (rows 1 and 2): a slit.
        let slit = &column(&m, 4, 2)[1..3];
        assert_eq!(
            cut_edges(&m, &mut uv, slit),
            1,
            "only the middle vertex splits"
        );
        assert_eq!(Islands::compute(&m, &uv).count(), 1);
        assert!(slit.iter().all(|&e| uv.edge_kind(&m, e) == EdgeKind::Seam));
        // A single isolated interior edge cannot open.
        let (m, mut uv) = grid(4);
        assert_eq!(cut_edges(&m, &mut uv, &column(&m, 4, 2)[1..2]), 0);
    }

    #[test]
    fn closed_and_tube_islands_are_cut_to_disks() {
        // Cube: closed.
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
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..m.corner_count())
                .map(|c| m.corner_vertex(c) as u32)
                .collect(),
            uvs: vec![DVec2::ZERO; 8],
        };
        let cuts = cut_to_disk(&m, &mut uv, &[0]);
        assert!(!cuts.is_empty());
        let isl = Islands::compute(&m, &uv);
        assert_eq!(isl.count(), 1);
        assert_eq!(boundary_loops(&m, &uv, isl.faces(0)).len(), 1);

        // Sharp edges of a cube: all 12.
        assert_eq!(sharp_edges(&m, 60.0).len(), 12);

        // Tube: the cube without top and bottom → two loops.
        let tube = Mesh::new(m.positions().to_vec(), &faces[2..]).unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..tube.corner_count())
                .map(|c| tube.corner_vertex(c) as u32)
                .collect(),
            uvs: vec![DVec2::ZERO; 8],
        };
        assert_eq!(
            boundary_loops(&tube, &uv, Islands::compute(&tube, &uv).faces(0)).len(),
            2
        );
        assert_eq!(cut_to_disk(&tube, &mut uv, &[0]).len(), 1);
        let isl = Islands::compute(&tube, &uv);
        assert_eq!(boundary_loops(&tube, &uv, isl.faces(0)).len(), 1);
    }

    #[test]
    fn paths() {
        let (m, _) = grid(3);
        // Corner to corner on a 3×3 grid: 6 edges.
        assert_eq!(vertex_path(&m, 0, 15).unwrap().len(), 6);
        assert_eq!(vertex_path(&m, 5, 5).unwrap().len(), 0);
    }
}
