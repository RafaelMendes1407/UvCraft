//! Mesh problem detection. Reports problems as sorted index lists so they can
//! be selected and isolated; it never changes or reorders the mesh.
//!
//! `Mesh::new` already accepts every problem found here: an edge used by more
//! than two faces simply gets no twins (it acts like a cut), and inconsistent
//! winding is tolerated by the twin search.

use std::collections::HashMap;

use glam::DVec3;
use serde::Serialize;
use uvcraft_core::Mesh;

/// Face area below this fraction of the squared longest edge counts as zero.
const DEGENERATE_EPS: f64 = 1e-12;
/// Default duplicate-vertex tolerance, relative to the bounding-box diagonal.
pub const DEFAULT_RELATIVE_TOLERANCE: f64 = 1e-6;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MeshProblems {
    pub vertex_count: usize,
    pub edge_count: usize,
    /// Connected components of faces (faces sharing an edge).
    pub components: usize,
    /// Edges used by exactly one face.
    pub boundary_edges: usize,
    /// Edges used by more than two faces.
    pub non_manifold_edges: Vec<usize>,
    /// Vertices whose faces form more than one fan (bow ties, and the ends of
    /// non-manifold edges).
    pub non_manifold_vertices: Vec<usize>,
    /// Absolute distance under which two vertices count as duplicates.
    pub duplicate_tolerance: f64,
    /// Groups of coincident vertices, each sorted, ordered by first index.
    pub duplicate_vertices: Vec<Vec<usize>>,
    /// Faces with (near) zero area.
    pub degenerate_faces: Vec<usize>,
    /// Faces that use the same vertex more than once.
    pub repeated_vertex_faces: Vec<usize>,
    /// Two-face edges whose faces traverse it in the same direction.
    pub inconsistent_winding_edges: Vec<usize>,
    /// Vertices used by no face.
    pub isolated_vertices: Vec<usize>,
}

impl MeshProblems {
    /// Total number of problems (excluding counts such as borders and components).
    pub fn problem_count(&self) -> usize {
        self.non_manifold_edges.len()
            + self.non_manifold_vertices.len()
            + self.duplicate_vertices.len()
            + self.degenerate_faces.len()
            + self.repeated_vertex_faces.len()
            + self.inconsistent_winding_edges.len()
            + self.isolated_vertices.len()
    }

    pub fn is_clean(&self) -> bool {
        self.problem_count() == 0
    }
}

struct Dsu(Vec<u32>);

impl Dsu {
    fn new(n: usize) -> Self {
        Dsu((0..n as u32).collect())
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.0[x] as usize != x {
            let p = self.0[x] as usize;
            self.0[x] = self.0[p];
            x = p;
        }
        x
    }
    /// Keeps the smaller root, so results do not depend on union order.
    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.0[a.max(b)] = a.min(b) as u32;
        }
    }
}

/// Default absolute tolerance for `mesh`: a millionth of its bounding-box diagonal.
pub fn default_tolerance(mesh: &Mesh) -> f64 {
    let (lo, hi) = mesh.positions().iter().fold(
        (DVec3::splat(f64::INFINITY), DVec3::splat(f64::NEG_INFINITY)),
        |(lo, hi), &p| (lo.min(p), hi.max(p)),
    );
    if mesh.vertex_count() == 0 {
        0.0
    } else {
        lo.distance(hi) * DEFAULT_RELATIVE_TOLERANCE
    }
}

/// Validates with the default duplicate tolerance.
pub fn validate(mesh: &Mesh) -> MeshProblems {
    validate_with_tolerance(mesh, default_tolerance(mesh))
}

/// Validates; `tolerance` is the absolute duplicate-vertex distance (0 = exact).
pub fn validate_with_tolerance(mesh: &Mesh, tolerance: f64) -> MeshProblems {
    let tolerance = if tolerance.is_finite() {
        tolerance.max(0.0)
    } else {
        0.0
    };
    let (nc, ne) = (mesh.corner_count(), mesh.edge_count());

    // Faces per edge, and face components across edges.
    let mut edge_faces = vec![0usize; ne];
    let mut faces = Dsu::new(mesh.face_count());
    let mut used = vec![false; mesh.vertex_count()];
    for c in 0..nc {
        let e = mesh.corner_edge(c);
        edge_faces[e] += 1;
        let (first, _) = mesh.edge_corners(e);
        faces.union(mesh.corner_face(c), mesh.corner_face(first));
        used[mesh.corner_vertex(c)] = true;
    }
    let components = (0..mesh.face_count())
        .filter(|&f| faces.find(f) == f)
        .count();

    let mut inconsistent_winding_edges = Vec::new();
    for e in 0..ne {
        if let (c0, Some(c1)) = mesh.edge_corners(e)
            && mesh.corner_vertex(c0) == mesh.corner_vertex(c1)
        {
            inconsistent_winding_edges.push(e);
        }
    }

    let mut degenerate_faces = Vec::new();
    let mut repeated_vertex_faces = Vec::new();
    for f in 0..mesh.face_count() {
        let mut vs: Vec<usize> = mesh
            .face_corners(f)
            .map(|c| mesh.corner_vertex(c))
            .collect();
        let longest = mesh
            .face_corners(f)
            .map(|c| {
                mesh.corner_position(c)
                    .distance_squared(mesh.corner_position(mesh.next(c)))
            })
            .fold(0.0, f64::max);
        let area = mesh.face_area_normal(f).length();
        if longest == 0.0 || area <= DEGENERATE_EPS * longest {
            degenerate_faces.push(f);
        }
        vs.sort_unstable();
        if vs.windows(2).any(|w| w[0] == w[1]) {
            repeated_vertex_faces.push(f);
        }
    }

    MeshProblems {
        vertex_count: mesh.vertex_count(),
        edge_count: ne,
        components,
        boundary_edges: edge_faces.iter().filter(|&&n| n == 1).count(),
        non_manifold_edges: (0..ne).filter(|&e| edge_faces[e] > 2).collect(),
        non_manifold_vertices: non_manifold_vertices(mesh),
        duplicate_tolerance: tolerance,
        duplicate_vertices: duplicate_vertices(mesh.positions(), tolerance),
        degenerate_faces,
        repeated_vertex_faces,
        inconsistent_winding_edges,
        isolated_vertices: (0..mesh.vertex_count()).filter(|&v| !used[v]).collect(),
    }
}

/// Vertices whose corners form more than one fan. Corners around a vertex are
/// joined across two-face edges; non-manifold edges do not join them.
fn non_manifold_vertices(mesh: &Mesh) -> Vec<usize> {
    let nc = mesh.corner_count();
    let mut fans = Dsu::new(nc);
    // The corner of `t`'s face that sits on vertex `v` (t's edge touches v).
    let at = |t: usize, v: usize| {
        if mesh.corner_vertex(t) == v {
            t
        } else {
            mesh.next(t)
        }
    };
    for c in 0..nc {
        let (v, n) = (mesh.corner_vertex(c), mesh.next(c));
        let w = mesh.corner_vertex(n);
        let Some(t) = mesh.twin(c) else { continue };
        if v == w || t < c {
            continue;
        }
        fans.union(c, at(t, v));
        fans.union(n, at(t, w));
    }
    let mut roots = vec![u32::MAX; mesh.vertex_count()];
    let mut bad = vec![false; mesh.vertex_count()];
    for c in 0..nc {
        let v = mesh.corner_vertex(c);
        let r = fans.find(c) as u32;
        if roots[v] == u32::MAX {
            roots[v] = r;
        } else if roots[v] != r {
            bad[v] = true;
        }
    }
    (0..bad.len()).filter(|&v| bad[v]).collect()
}

/// Groups of vertices within `tolerance` of each other (transitively), via a
/// uniform grid. Output is sorted and independent of hash order.
fn duplicate_vertices(positions: &[DVec3], tolerance: f64) -> Vec<Vec<usize>> {
    let cell_size = if tolerance > 0.0 { tolerance } else { 1.0 };
    let cell = |p: DVec3| -> [i64; 3] {
        if tolerance > 0.0 {
            (p / cell_size).floor().to_array().map(|x| x as i64)
        } else {
            // Exact matching: key by the bit pattern (0.0 and -0.0 unified).
            p.to_array().map(|x| (x + 0.0).to_bits() as i64)
        }
    };
    let mut grid: HashMap<[i64; 3], Vec<u32>> = HashMap::new();
    let mut dsu = Dsu::new(positions.len());
    let mut any = false;
    for (v, &p) in positions.iter().enumerate() {
        if !p.is_finite() {
            continue;
        }
        let k = cell(p);
        let range = if tolerance > 0.0 { -1..=1 } else { 0..=0 };
        for dx in range.clone() {
            for dy in range.clone() {
                for dz in range.clone() {
                    let key = [k[0] + dx, k[1] + dy, k[2] + dz];
                    if let Some(list) = grid.get(&key) {
                        for &u in list {
                            let q = positions[u as usize];
                            if (tolerance > 0.0 && p.distance(q) <= tolerance) || p == q {
                                dsu.union(u as usize, v);
                                any = true;
                            }
                        }
                    }
                }
            }
        }
        grid.entry(k).or_default().push(v as u32);
    }
    if !any {
        return Vec::new();
    }
    let mut groups: Vec<Vec<usize>> = vec![Vec::new(); positions.len()];
    for v in 0..positions.len() {
        let r = dsu.find(v);
        groups[r].push(v);
    }
    // Roots are each group's smallest index, so this is ordered by first index.
    groups.into_iter().filter(|g| g.len() > 1).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid_quads() -> Mesh {
        // 2×1 quads, consistent winding.
        let p = (0..6)
            .map(|i| DVec3::new((i % 3) as f64, (i / 3) as f64, 0.0))
            .collect();
        Mesh::new(p, &[vec![0, 1, 4, 3], vec![1, 2, 5, 4]]).unwrap()
    }

    #[test]
    fn clean_mesh_has_no_problems() {
        let r = validate(&grid_quads());
        assert!(r.is_clean(), "{r:?}");
        assert_eq!(r.components, 1);
        assert_eq!(r.boundary_edges, 6);
        assert_eq!(r.edge_count, 7);
    }

    #[test]
    fn closed_cube_is_clean() {
        let p = (0..8)
            .map(|i| DVec3::new((i & 1) as f64, ((i >> 1) & 1) as f64, (i >> 2) as f64))
            .collect();
        let f = [
            [0, 2, 3, 1],
            [4, 5, 7, 6],
            [0, 1, 5, 4],
            [2, 6, 7, 3],
            [0, 4, 6, 2],
            [1, 3, 7, 5],
        ];
        let r = validate(&Mesh::new(p, &f).unwrap());
        assert!(r.is_clean(), "{r:?}");
        assert_eq!(r.boundary_edges, 0);
        assert_eq!(r.components, 1);
    }

    #[test]
    fn non_manifold_edge_and_vertices() {
        let p = vec![DVec3::ZERO, DVec3::X, DVec3::Y, DVec3::Z, DVec3::NEG_Y];
        let m = Mesh::new(p, &[[0, 1, 2], [1, 0, 3], [0, 1, 4]]).unwrap();
        let r = validate(&m);
        assert_eq!(r.non_manifold_edges.len(), 1);
        assert_eq!(m.edge_vertices(r.non_manifold_edges[0]), [0, 1]);
        assert_eq!(r.non_manifold_vertices, vec![0, 1]);
        assert_eq!(r.components, 1);
    }

    #[test]
    fn bow_tie_vertex() {
        // Two triangles touching only at vertex 0.
        let p = vec![DVec3::ZERO, DVec3::X, DVec3::Y, DVec3::NEG_X, DVec3::NEG_Y];
        let r = validate(&Mesh::new(p, &[[0, 1, 2], [0, 3, 4]]).unwrap());
        assert_eq!(r.non_manifold_vertices, vec![0]);
        assert!(r.non_manifold_edges.is_empty());
        assert_eq!(r.components, 2);
    }

    #[test]
    fn duplicates_degenerates_isolated_and_winding() {
        let p = vec![
            DVec3::ZERO,
            DVec3::X,
            DVec3::Y,
            DVec3::new(1.0, 1e-9, 0.0), // duplicate of 1 (within tolerance)
            DVec3::new(9.0, 9.0, 9.0),  // isolated
            DVec3::new(2.0, 0.0, 0.0),  // collinear with 0, 1
            DVec3::new(1.0, 1.0, 0.0),
        ];
        let faces = vec![
            vec![0u32, 1, 2],
            vec![1, 6, 2],
            vec![1, 0, 5], // zero area
            vec![2, 6, 2], // repeated vertex
        ];
        let m = Mesh::new(p, &faces).unwrap();
        let r = validate(&m);
        assert_eq!(r.duplicate_vertices, vec![vec![1, 3]]);
        assert!(r.duplicate_tolerance > 0.0);
        assert_eq!(r.isolated_vertices, vec![3, 4]);
        assert_eq!(r.degenerate_faces, vec![2, 3]);
        assert_eq!(r.repeated_vertex_faces, vec![3]);
        assert!(r.inconsistent_winding_edges.is_empty());

        // Exact tolerance: 1 and 3 are no longer duplicates.
        assert!(
            validate_with_tolerance(&m, 0.0)
                .duplicate_vertices
                .is_empty()
        );
    }

    #[test]
    fn flipped_neighbor_is_inconsistent() {
        let p = (0..6)
            .map(|i| DVec3::new((i % 3) as f64, (i / 3) as f64, 0.0))
            .collect();
        let m = Mesh::new(p, &[vec![0, 1, 4, 3], vec![4, 5, 2, 1]]).unwrap();
        let r = validate(&m);
        assert_eq!(r.inconsistent_winding_edges.len(), 1);
        assert_eq!(m.edge_vertices(r.inconsistent_winding_edges[0]), [1, 4]);
        // A flipped neighbor still forms one fan around the shared vertices.
        assert!(r.non_manifold_vertices.is_empty());
    }

    #[test]
    fn exact_duplicates_and_deterministic_groups() {
        let mut p = Vec::new();
        for i in 0..50 {
            p.push(DVec3::new((i % 5) as f64, 0.0, 0.0));
        }
        let groups = duplicate_vertices(&p, 0.0);
        assert_eq!(groups.len(), 5);
        assert_eq!(groups[0], (0..50).step_by(5).collect::<Vec<_>>());
        assert_eq!(groups, duplicate_vertices(&p, 1e-3));
    }
}
