//! Islands: connected components of faces across interior (non-seam) edges.
//! Derived data, recomputed after a UV topology change.

use crate::mesh::Mesh;
use crate::uv::{EdgeKind, UvLayer};

#[derive(Debug, Clone, PartialEq)]
pub struct Islands {
    face_island: Vec<u32>,
    faces: Vec<Vec<u32>>,
}

fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        let p = parent[x as usize];
        parent[x as usize] = parent[p as usize];
        x = p;
    }
    x
}

impl Islands {
    /// Islands are numbered in order of their lowest face index.
    pub fn compute(mesh: &Mesh, uv: &UvLayer) -> Self {
        let n = mesh.face_count();
        let mut parent: Vec<u32> = (0..n as u32).collect();
        for e in 0..mesh.edge_count() {
            if uv.edge_kind(mesh, e) != EdgeKind::Interior {
                continue;
            }
            let (c0, Some(c1)) = mesh.edge_corners(e) else {
                continue;
            };
            let a = find(&mut parent, mesh.corner_face(c0) as u32);
            let b = find(&mut parent, mesh.corner_face(c1) as u32);
            if a != b {
                let (lo, hi) = (a.min(b), a.max(b));
                parent[hi as usize] = lo;
            }
        }

        let mut root_island = vec![u32::MAX; n];
        let mut face_island = vec![0; n];
        let mut faces: Vec<Vec<u32>> = Vec::new();
        for (f, island) in face_island.iter_mut().enumerate() {
            let r = find(&mut parent, f as u32) as usize;
            if root_island[r] == u32::MAX {
                root_island[r] = faces.len() as u32;
                faces.push(Vec::new());
            }
            *island = root_island[r];
            faces[root_island[r] as usize].push(f as u32);
        }
        Islands { face_island, faces }
    }

    /// Recompute only components touched by edges whose seam state changed.
    /// Unaffected face groups are reused, then all groups are renumbered by
    /// their first face so island indices stay deterministic.
    pub fn update(&self, mesh: &Mesh, before: &UvLayer, after: &UvLayer) -> Self {
        if self.face_island.len() != mesh.face_count()
            || before.corner_uv.len() != mesh.corner_count()
            || after.corner_uv.len() != mesh.corner_count()
        {
            return Self::compute(mesh, after);
        }
        let mut dirty_islands = vec![false; self.count()];
        for edge in 0..mesh.edge_count() {
            if before.edge_kind(mesh, edge) == after.edge_kind(mesh, edge) {
                continue;
            }
            let (a, b) = mesh.edge_corners(edge);
            dirty_islands[self.island_of_face(mesh.corner_face(a))] = true;
            if let Some(b) = b {
                dirty_islands[self.island_of_face(mesh.corner_face(b))] = true;
            }
        }
        if !dirty_islands.iter().any(|&dirty| dirty) {
            return self.clone();
        }
        let mut dirty_faces = vec![false; mesh.face_count()];
        let mut components: Vec<Vec<u32>> = Vec::new();
        for (island, faces) in self.faces.iter().enumerate() {
            if dirty_islands[island] {
                for &face in faces {
                    dirty_faces[face as usize] = true;
                }
            } else {
                components.push(faces.clone());
            }
        }
        let mut visited = vec![false; mesh.face_count()];
        for start in 0..mesh.face_count() {
            if !dirty_faces[start] || visited[start] {
                continue;
            }
            let mut queue = std::collections::VecDeque::from([start]);
            visited[start] = true;
            let mut component = Vec::new();
            while let Some(face) = queue.pop_front() {
                component.push(face as u32);
                for corner in mesh.face_corners(face) {
                    if after.edge_kind(mesh, mesh.corner_edge(corner)) != EdgeKind::Interior {
                        continue;
                    }
                    let Some(other_corner) = mesh.twin(corner) else {
                        continue;
                    };
                    let other = mesh.corner_face(other_corner);
                    if dirty_faces[other] && !visited[other] {
                        visited[other] = true;
                        queue.push_back(other);
                    }
                }
            }
            component.sort_unstable();
            components.push(component);
        }
        components.sort_by_key(|faces| faces[0]);
        let mut face_island = vec![0; mesh.face_count()];
        for (island, faces) in components.iter().enumerate() {
            for &face in faces {
                face_island[face as usize] = island as u32;
            }
        }
        Self {
            face_island,
            faces: components,
        }
    }

    pub fn count(&self) -> usize {
        self.faces.len()
    }

    pub fn island_of_face(&self, f: usize) -> usize {
        self.face_island[f] as usize
    }

    pub fn faces(&self, island: usize) -> &[u32] {
        &self.faces[island]
    }

    /// Normalizes UV topology so each UV vertex belongs to one island and one
    /// mesh vertex. Files can share a UV index between islands (islands
    /// touching only at a corner) or between unrelated mesh vertices (e.g.
    /// reused `vt` lines); editing one would then drag the other. Shared
    /// indices are duplicated. Islands stay the same, so `self` remains valid.
    /// Returns the number of UV vertices added.
    pub fn split_shared_uvs(&self, mesh: &Mesh, uv: &mut UvLayer) -> usize {
        let mut owner = vec![(u32::MAX, u32::MAX); uv.uvs.len()];
        let mut copies: std::collections::HashMap<(u32, u32, u32), u32> =
            std::collections::HashMap::new();
        let before = uv.uvs.len();
        for (island, faces) in self.faces.iter().enumerate() {
            let island = island as u32;
            for &f in faces {
                for c in mesh.face_corners(f as usize) {
                    let idx = uv.corner_uv[c];
                    let key = (island, mesh.corner_vertex(c) as u32);
                    let o = &mut owner[idx as usize];
                    if o.0 == u32::MAX {
                        *o = key;
                    } else if *o != key {
                        uv.corner_uv[c] = *copies.entry((key.0, key.1, idx)).or_insert_with(|| {
                            uv.uvs.push(uv.uvs[idx as usize]);
                            uv.uvs.len() as u32 - 1
                        });
                    }
                }
            }
        }
        uv.uvs.len() - before
    }

    /// Sorted, deduplicated UV vertex indices used by an island.
    pub fn uv_indices(&self, island: usize, mesh: &Mesh, uv: &UvLayer) -> Vec<u32> {
        let mut out: Vec<u32> = self.faces[island]
            .iter()
            .flat_map(|&f| mesh.face_corners(f as usize))
            .map(|c| uv.corner_uv[c])
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::{DVec2, DVec3};

    #[test]
    fn seam_splits_islands() {
        // Strip of three triangles: 0-1-2, 2-1-3, 2-3-4.
        let p = vec![DVec3::ZERO, DVec3::X, DVec3::Y, DVec3::ONE, DVec3::Z];
        let m = Mesh::new(p, &[[0, 1, 2], [2, 1, 3], [2, 3, 4]]).unwrap();
        let welded = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 2, 1, 3, 2, 3, 4],
            uvs: vec![DVec2::ZERO; 5],
        };
        let isl = Islands::compute(&m, &welded);
        assert_eq!(isl.count(), 1);
        assert_eq!(isl.uv_indices(0, &m, &welded), vec![0, 1, 2, 3, 4]);

        // Cut between face 1 and face 2 (edge 2-3).
        let mut cut = welded.clone();
        cut.uvs.extend([DVec2::ZERO; 2]);
        cut.corner_uv[6] = 5;
        cut.corner_uv[7] = 6;
        let isl = Islands::compute(&m, &cut);
        assert_eq!(Islands::compute(&m, &welded).update(&m, &welded, &cut), isl);
        assert_eq!(isl.update(&m, &cut, &welded), Islands::compute(&m, &welded));
        assert_eq!(isl.count(), 2);
        assert_eq!(isl.faces(0), &[0, 1]);
        assert_eq!(isl.island_of_face(2), 1);
    }

    #[test]
    fn split_shared_uvs_separates_islands_touching_at_a_corner() {
        // Two triangles sharing only vertex 0, and its UV vertex 0.
        let p = vec![DVec3::ZERO, DVec3::X, DVec3::Y, DVec3::NEG_X, DVec3::NEG_Y];
        let m = Mesh::new(p, &[[0, 1, 2], [0, 3, 4]]).unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 0, 3, 4],
            uvs: vec![DVec2::ZERO, DVec2::X, DVec2::Y, DVec2::NEG_X, DVec2::NEG_Y],
        };
        let isl = Islands::compute(&m, &uv);
        assert_eq!(isl.count(), 2);
        assert_eq!(isl.split_shared_uvs(&m, &mut uv), 1);
        assert_eq!(uv.corner_uv, vec![0, 1, 2, 5, 3, 4]);
        assert_eq!(uv.uvs[5], DVec2::ZERO);
        assert_eq!(Islands::compute(&m, &uv), isl);
        assert_eq!(isl.split_shared_uvs(&m, &mut uv), 0);
    }

    #[test]
    fn split_shared_uvs_separates_mesh_vertices() {
        // One quad whose four corners all reuse UV 0 (like a file reusing one vt).
        let p = vec![DVec3::ZERO, DVec3::X, DVec3::ONE, DVec3::Y];
        let m = Mesh::new(p, &[[0, 1, 2, 3]]).unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 0, 0, 0],
            uvs: vec![DVec2::ZERO],
        };
        let isl = Islands::compute(&m, &uv);
        assert_eq!(isl.split_shared_uvs(&m, &mut uv), 3);
        assert_eq!(uv.corner_uv, vec![0, 1, 2, 3]);
    }
}
