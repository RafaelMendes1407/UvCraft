//! Immutable polygon mesh with half-edge style connectivity.
//!
//! The 3D mesh never changes after import: every UvCraft operation edits only
//! the UV layer. This is what makes topology-preserving export, cheap undo and
//! per-island parallelism possible.

use std::ops::Range;

use glam::DVec3;

/// Sentinel for "no element" in compact `u32` storage.
pub const NONE: u32 = u32::MAX;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum MeshError {
    #[error("face {face} has {count} corners; at least 3 are required")]
    FaceTooSmall { face: usize, count: usize },
    #[error("face {face} references vertex {vertex}, but the mesh has {vertex_count} vertices")]
    VertexOutOfRange {
        face: usize,
        vertex: u32,
        vertex_count: usize,
    },
    #[error("mesh too large: {0} elements exceed the u32 index range")]
    TooLarge(usize),
}

/// Polygon mesh stored as CSR arrays.
///
/// Face `f` owns the corners `face_start[f]..face_start[f + 1]`. A corner is
/// also a half-edge: corner `c` runs from `corner_vertex(c)` to the vertex of
/// `next(c)`. Face order and per-face corner order are exactly the input order.
#[derive(Debug, Clone)]
pub struct Mesh {
    positions: Vec<DVec3>,
    face_start: Vec<u32>,
    corner_vertex: Vec<u32>,
    corner_face: Vec<u32>,
    /// Opposite corner across the edge, or `NONE` on borders and non-manifold edges.
    twin: Vec<u32>,
    corner_edge: Vec<u32>,
    /// First corner of each edge.
    edge_corner: Vec<u32>,
    edge_non_manifold: Vec<bool>,
    inconsistent_orientation_edges: usize,
    /// CSR vertex → incident edges.
    vertex_edge_start: Vec<u32>,
    vertex_edges: Vec<u32>,
}

fn to_u32(n: usize) -> Result<u32, MeshError> {
    u32::try_from(n)
        .ok()
        .filter(|&v| v != NONE)
        .ok_or(MeshError::TooLarge(n))
}

impl Mesh {
    /// Builds a mesh from positions and polygon faces (vertex indices, any winding).
    pub fn new<F: AsRef<[u32]>>(positions: Vec<DVec3>, faces: &[F]) -> Result<Self, MeshError> {
        to_u32(positions.len())?;
        let mut face_start = Vec::with_capacity(faces.len() + 1);
        let mut corner_vertex = Vec::new();
        let mut corner_face = Vec::new();
        face_start.push(0);
        for (f, face) in faces.iter().enumerate() {
            let face = face.as_ref();
            if face.len() < 3 {
                return Err(MeshError::FaceTooSmall {
                    face: f,
                    count: face.len(),
                });
            }
            let f32_ = to_u32(f)?;
            for &v in face {
                if v as usize >= positions.len() {
                    return Err(MeshError::VertexOutOfRange {
                        face: f,
                        vertex: v,
                        vertex_count: positions.len(),
                    });
                }
                corner_vertex.push(v);
                corner_face.push(f32_);
            }
            face_start.push(to_u32(corner_vertex.len())?);
        }

        let mut mesh = Mesh {
            positions,
            face_start,
            corner_vertex,
            corner_face,
            twin: Vec::new(),
            corner_edge: Vec::new(),
            edge_corner: Vec::new(),
            edge_non_manifold: Vec::new(),
            inconsistent_orientation_edges: 0,
            vertex_edge_start: Vec::new(),
            vertex_edges: Vec::new(),
        };
        mesh.build_edges();
        mesh.build_vertex_edges();
        Ok(mesh)
    }

    /// Groups corners by undirected edge (sort-based: no per-edge allocation).
    fn build_edges(&mut self) {
        let n = self.corner_vertex.len();
        let mut keys: Vec<(u32, u32, u32)> = (0..n)
            .map(|c| {
                let a = self.corner_vertex[c];
                let b = self.corner_vertex[self.next(c)];
                (a.min(b), a.max(b), c as u32)
            })
            .collect();
        keys.sort_unstable();

        self.twin = vec![NONE; n];
        self.corner_edge = vec![NONE; n];
        let mut i = 0;
        while i < keys.len() {
            let mut j = i + 1;
            while j < keys.len() && keys[j].0 == keys[i].0 && keys[j].1 == keys[i].1 {
                j += 1;
            }
            let edge = self.edge_corner.len() as u32;
            let group = &keys[i..j];
            for &(_, _, c) in group {
                self.corner_edge[c as usize] = edge;
            }
            self.edge_corner.push(group[0].2);
            self.edge_non_manifold.push(group.len() > 2);
            if let [(_, _, c0), (_, _, c1)] = *group {
                self.twin[c0 as usize] = c1;
                self.twin[c1 as usize] = c0;
                if self.corner_vertex[c0 as usize] == self.corner_vertex[c1 as usize] {
                    self.inconsistent_orientation_edges += 1;
                }
            }
            i = j;
        }
    }

    fn build_vertex_edges(&mut self) {
        let mut count = vec![0u32; self.positions.len() + 1];
        for e in 0..self.edge_count() {
            let [a, b] = self.edge_vertices(e);
            count[a + 1] += 1;
            if b != a {
                count[b + 1] += 1;
            }
        }
        for i in 1..count.len() {
            count[i] += count[i - 1];
        }
        let mut fill = count.clone();
        self.vertex_edges = vec![0; *count.last().unwrap_or(&0) as usize];
        for e in 0..self.edge_count() {
            let [a, b] = self.edge_vertices(e);
            for v in if a == b { vec![a] } else { vec![a, b] } {
                self.vertex_edges[fill[v] as usize] = e as u32;
                fill[v] += 1;
            }
        }
        self.vertex_edge_start = count;
    }

    /// Edges incident to vertex `v`.
    pub fn vertex_edges(&self, v: usize) -> &[u32] {
        &self.vertex_edges
            [self.vertex_edge_start[v] as usize..self.vertex_edge_start[v + 1] as usize]
    }

    /// The endpoint of edge `e` that is not `v`.
    pub fn edge_other(&self, e: usize, v: usize) -> usize {
        let [a, b] = self.edge_vertices(e);
        if a == v { b } else { a }
    }

    /// Area-weighted face normal (Newell's method): length = face area.
    pub fn face_area_normal(&self, f: usize) -> DVec3 {
        let mut n = DVec3::ZERO;
        for c in self.face_corners(f) {
            let p = self.corner_position(c);
            let q = self.corner_position(self.next(c));
            n += DVec3::new(
                (p.y - q.y) * (p.z + q.z),
                (p.z - q.z) * (p.x + q.x),
                (p.x - q.x) * (p.y + q.y),
            );
        }
        n * 0.5
    }

    /// Two-sided dihedral angle at an interior manifold edge, in degrees
    /// (0 = flat). `None` on borders and non-manifold edges.
    pub fn dihedral_angle_deg(&self, e: usize) -> Option<f64> {
        let (c0, c1) = self.edge_corners(e);
        let n0 = self
            .face_area_normal(self.corner_face(c0))
            .normalize_or_zero();
        let n1 = self
            .face_area_normal(self.corner_face(c1?))
            .normalize_or_zero();
        // Faces with inconsistent winding have one normal reversed.
        let n1 = if self.corner_vertex(c1?) == self.corner_vertex(c0) {
            -n1
        } else {
            n1
        };
        Some(n0.dot(n1).clamp(-1.0, 1.0).acos().to_degrees())
    }

    pub fn vertex_count(&self) -> usize {
        self.positions.len()
    }
    pub fn face_count(&self) -> usize {
        self.face_start.len() - 1
    }
    pub fn corner_count(&self) -> usize {
        self.corner_vertex.len()
    }
    pub fn edge_count(&self) -> usize {
        self.edge_corner.len()
    }

    pub fn positions(&self) -> &[DVec3] {
        &self.positions
    }
    pub fn position(&self, v: usize) -> DVec3 {
        self.positions[v]
    }

    pub fn face_corners(&self, f: usize) -> Range<usize> {
        self.face_start[f] as usize..self.face_start[f + 1] as usize
    }

    /// Fan triangulation of face `f` as corner triples (used for metrics).
    pub fn fan_triangles(&self, f: usize) -> impl Iterator<Item = [usize; 3]> {
        let r = self.face_corners(f);
        let c0 = r.start;
        (r.start + 1..r.end - 1).map(move |c| [c0, c, c + 1])
    }

    pub fn corner_vertex(&self, c: usize) -> usize {
        self.corner_vertex[c] as usize
    }
    pub fn corner_face(&self, c: usize) -> usize {
        self.corner_face[c] as usize
    }
    pub fn corner_position(&self, c: usize) -> DVec3 {
        self.positions[self.corner_vertex(c)]
    }

    /// Next corner in the same face (the half-edge's end vertex).
    pub fn next(&self, c: usize) -> usize {
        let f = self.corner_face(c);
        if c + 1 == self.face_start[f + 1] as usize {
            self.face_start[f] as usize
        } else {
            c + 1
        }
    }

    pub fn prev(&self, c: usize) -> usize {
        let f = self.corner_face(c);
        if c == self.face_start[f] as usize {
            self.face_start[f + 1] as usize - 1
        } else {
            c - 1
        }
    }

    /// Opposite corner across this corner's edge; `None` on borders and non-manifold edges.
    pub fn twin(&self, c: usize) -> Option<usize> {
        let t = self.twin[c];
        (t != NONE).then_some(t as usize)
    }

    pub fn corner_edge(&self, c: usize) -> usize {
        self.corner_edge[c] as usize
    }

    /// One corner of edge `e` and its twin, if the edge is manifold and interior.
    pub fn edge_corners(&self, e: usize) -> (usize, Option<usize>) {
        let c = self.edge_corner[e] as usize;
        (c, self.twin(c))
    }

    pub fn edge_vertices(&self, e: usize) -> [usize; 2] {
        let c = self.edge_corner[e] as usize;
        [self.corner_vertex(c), self.corner_vertex(self.next(c))]
    }

    pub fn edge_length(&self, e: usize) -> f64 {
        let [a, b] = self.edge_vertices(e);
        self.positions[a].distance(self.positions[b])
    }

    pub fn is_non_manifold(&self, e: usize) -> bool {
        self.edge_non_manifold[e]
    }

    pub fn non_manifold_edge_count(&self) -> usize {
        self.edge_non_manifold.iter().filter(|&&b| b).count()
    }

    /// Manifold edges whose two faces traverse it in the same direction.
    pub fn inconsistent_orientation_edge_count(&self) -> usize {
        self.inconsistent_orientation_edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad_pair() -> Mesh {
        // Two quads sharing edge 1-4.
        let p = (0..6)
            .map(|i| DVec3::new((i % 3) as f64, (i / 3) as f64, 0.0))
            .collect();
        Mesh::new(p, &[vec![0, 1, 4, 3], vec![1, 2, 5, 4]]).unwrap()
    }

    #[test]
    fn counts_and_twins() {
        let m = quad_pair();
        assert_eq!(m.face_count(), 2);
        assert_eq!(m.corner_count(), 8);
        assert_eq!(m.edge_count(), 7);
        // Corner 1 (1→4) and corner 7 (4→1) are twins.
        assert_eq!(m.twin(1), Some(7));
        assert_eq!(m.twin(7), Some(1));
        assert_eq!(m.twin(0), None);
        assert_eq!(m.next(3), 0);
        assert_eq!(m.prev(4), 7);
        assert_eq!(
            m.fan_triangles(0).collect::<Vec<_>>(),
            vec![[0, 1, 2], [0, 2, 3]]
        );
        assert_eq!(m.non_manifold_edge_count(), 0);
        assert_eq!(m.inconsistent_orientation_edge_count(), 0);
    }

    #[test]
    fn adjacency_normals_and_dihedral() {
        let m = quad_pair();
        // Vertex 1 touches edges 0-1, 1-2, 1-4.
        let mut around: Vec<usize> = m
            .vertex_edges(1)
            .iter()
            .map(|&e| m.edge_other(e as usize, 1))
            .collect();
        around.sort();
        assert_eq!(around, vec![0, 2, 4]);
        assert_eq!(m.face_area_normal(0), DVec3::new(0.0, 0.0, 1.0));
        let shared = m.corner_edge(1);
        assert!(m.dihedral_angle_deg(shared).unwrap().abs() < 1e-9);
        assert_eq!(m.dihedral_angle_deg(m.corner_edge(0)), None);

        // Folded: second quad rotated 90° around the shared edge.
        let p = vec![
            DVec3::ZERO,
            DVec3::X,
            DVec3::new(1.0, 0.0, -1.0),
            DVec3::Y,
            DVec3::new(1.0, 1.0, 0.0),
            DVec3::new(1.0, 1.0, -1.0),
        ];
        let folded = Mesh::new(p, &[vec![0, 1, 4, 3], vec![1, 2, 5, 4]]).unwrap();
        assert!((folded.dihedral_angle_deg(folded.corner_edge(1)).unwrap() - 90.0).abs() < 1e-9);
    }

    #[test]
    fn non_manifold_edge_has_no_twins() {
        let p = vec![DVec3::ZERO, DVec3::X, DVec3::Y, DVec3::Z, DVec3::NEG_Y];
        let m = Mesh::new(p, &[[0, 1, 2], [1, 0, 3], [0, 1, 4]]).unwrap();
        assert_eq!(m.non_manifold_edge_count(), 1);
        assert_eq!(m.twin(0), None);
    }

    #[test]
    fn rejects_bad_faces() {
        let p = vec![DVec3::ZERO, DVec3::X, DVec3::Y];
        assert!(matches!(
            Mesh::new(p.clone(), &[[0, 1]]),
            Err(MeshError::FaceTooSmall { face: 0, count: 2 })
        ));
        assert!(matches!(
            Mesh::new(p, &[[0, 1, 9]]),
            Err(MeshError::VertexOutOfRange { vertex: 9, .. })
        ));
    }
}
