//! Geodesic cut graph for pelt-style unfolding of organic meshes.
//!
//! A connected network grows from an open boundary (or one vertex on a
//! closed surface). Each new branch follows a shortest mesh-edge path to the
//! vertex farthest from that network in geodesic distance. The result is a
//! deterministic cut tree; callers can add topology cuts before unfolding.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeSet, BinaryHeap};

use uvcraft_core::{EdgeKind, Islands, Mesh, UvLayer};

#[derive(Clone, Copy, PartialEq)]
struct Visit {
    distance: f64,
    vertex: usize,
}

impl Eq for Visit {}
impl PartialOrd for Visit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Visit {
    fn cmp(&self, other: &Self) -> Ordering {
        self.distance
            .total_cmp(&other.distance)
            .then(self.vertex.cmp(&other.vertex))
    }
}

/// Returns interior mesh edges of a geodesic branch network for target islands.
/// `branches` is the maximum number of farthest-point paths per island.
pub fn geodesic_cut_graph(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    targets: &[usize],
    branches: usize,
) -> Vec<usize> {
    let mut result = BTreeSet::new();
    for &island in targets {
        let mut in_face = vec![false; mesh.face_count()];
        let mut vertices = BTreeSet::new();
        let mut network = vec![false; mesh.vertex_count()];
        for &face in islands.faces(island) {
            in_face[face as usize] = true;
            for corner in mesh.face_corners(face as usize) {
                vertices.insert(mesh.corner_vertex(corner));
                if uv
                    .edge_kind(mesh, mesh.corner_edge(corner))
                    .is_island_boundary()
                {
                    network[mesh.corner_vertex(corner)] = true;
                    network[mesh.corner_vertex(mesh.next(corner))] = true;
                }
            }
        }
        if !network.iter().any(|&member| member) {
            if let Some(&first) = vertices.first() {
                network[first] = true;
            }
        }
        let allowed: Vec<bool> = (0..mesh.edge_count())
            .map(|edge| {
                if uv.edge_kind(mesh, edge) != EdgeKind::Interior {
                    return false;
                }
                let (a, Some(b)) = mesh.edge_corners(edge) else {
                    return false;
                };
                in_face[mesh.corner_face(a)] && in_face[mesh.corner_face(b)]
            })
            .collect();
        for _ in 0..branches {
            let mut distance = vec![f64::INFINITY; mesh.vertex_count()];
            let mut previous = vec![usize::MAX; mesh.vertex_count()];
            let mut queue = BinaryHeap::new();
            for &vertex in &vertices {
                if network[vertex] {
                    distance[vertex] = 0.0;
                    queue.push(Reverse(Visit {
                        distance: 0.0,
                        vertex,
                    }));
                }
            }
            while let Some(Reverse(Visit {
                distance: d,
                vertex,
            })) = queue.pop()
            {
                if d > distance[vertex] {
                    continue;
                }
                for &edge in mesh.vertex_edges(vertex) {
                    let edge = edge as usize;
                    if !allowed[edge] {
                        continue;
                    }
                    let other = mesh.edge_other(edge, vertex);
                    let next = d + mesh.edge_length(edge);
                    if next < distance[other] {
                        distance[other] = next;
                        previous[other] = edge;
                        queue.push(Reverse(Visit {
                            distance: next,
                            vertex: other,
                        }));
                    }
                }
            }
            let farthest = vertices
                .iter()
                .copied()
                .filter(|&vertex| distance[vertex].is_finite())
                .max_by(|&a, &b| distance[a].total_cmp(&distance[b]).then(b.cmp(&a)));
            let Some(mut vertex) = farthest.filter(|&vertex| distance[vertex] > 1e-12) else {
                break;
            };
            while !network[vertex] {
                let edge = previous[vertex];
                if edge == usize::MAX {
                    break;
                }
                result.insert(edge);
                network[vertex] = true;
                vertex = mesh.edge_other(edge, vertex);
            }
        }
    }
    result.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::{DVec2, DVec3};
    use uvcraft_core::UvLayer;

    #[test]
    fn closed_octahedron_gets_connected_geodesic_cut_branches() {
        let positions = vec![
            DVec3::Z,
            DVec3::NEG_Z,
            DVec3::X,
            DVec3::Y,
            DVec3::NEG_X,
            DVec3::NEG_Y,
        ];
        let mesh = Mesh::new(
            positions,
            &[
                [0, 2, 3],
                [0, 3, 4],
                [0, 4, 5],
                [0, 5, 2],
                [1, 3, 2],
                [1, 4, 3],
                [1, 5, 4],
                [1, 2, 5],
            ],
        )
        .unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..mesh.corner_count())
                .map(|c| mesh.corner_vertex(c) as u32)
                .collect(),
            uvs: vec![DVec2::ZERO; mesh.vertex_count()],
        };
        let islands = Islands::compute(&mesh, &uv);
        assert_eq!(islands.count(), 1);
        let cut = geodesic_cut_graph(&mesh, &uv, &islands, &[0], 4);
        assert!(cut.len() >= 3);
        assert!(
            cut.iter()
                .all(|&edge| uv.edge_kind(&mesh, edge) == EdgeKind::Interior)
        );
        assert_eq!(cut, geodesic_cut_graph(&mesh, &uv, &islands, &[0], 4));
    }

    #[test]
    fn curved_open_patch_branches_from_boundary_to_interior() {
        let mut positions = Vec::new();
        for y in 0..5 {
            for x in 0..5 {
                let dx = x as f64 - 2.0;
                let dy = y as f64 - 2.0;
                positions.push(DVec3::new(dx, dy, 1.0 - 0.1 * (dx * dx + dy * dy)));
            }
        }
        let mut faces = Vec::new();
        for y in 0..4 {
            for x in 0..4 {
                let a = y * 5 + x;
                faces.push([a as u32, (a + 1) as u32, (a + 6) as u32, (a + 5) as u32]);
            }
        }
        let mesh = Mesh::new(positions, &faces).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..mesh.corner_count())
                .map(|c| mesh.corner_vertex(c) as u32)
                .collect(),
            uvs: vec![DVec2::ZERO; mesh.vertex_count()],
        };
        let islands = Islands::compute(&mesh, &uv);
        let cut = geodesic_cut_graph(&mesh, &uv, &islands, &[0], 3);
        assert!(!cut.is_empty());
        assert!(
            cut.iter()
                .all(|&edge| uv.edge_kind(&mesh, edge) == EdgeKind::Interior)
        );
    }
}
