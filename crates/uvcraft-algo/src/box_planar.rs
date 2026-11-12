//! Six-axis face labeling with a Potts smoothness term for box-planar seams.

use uvcraft_core::Mesh;

const AXES: [[f64; 3]; 6] = [
    [1.0, 0.0, 0.0],
    [-1.0, 0.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.0, -1.0, 0.0],
    [0.0, 0.0, 1.0],
    [0.0, 0.0, -1.0],
];

/// Interior edges between dominant-normal charts. `smoothness` penalizes a
/// label break across adjacent faces; zero yields pure dominant-axis labels.
pub fn seam_edges(mesh: &Mesh, smoothness: f64, iterations: usize) -> Vec<usize> {
    let count = mesh.face_count();
    let normals: Vec<_> = (0..count)
        .map(|f| mesh.face_area_normal(f).normalize_or_zero())
        .collect();
    let mut neighbors = vec![Vec::new(); count];
    for e in 0..mesh.edge_count() {
        let (c0, Some(c1)) = mesh.edge_corners(e) else {
            continue;
        };
        let a = mesh.corner_face(c0);
        let b = mesh.corner_face(c1);
        neighbors[a].push(b);
        neighbors[b].push(a);
    }
    let data = |f: usize, label: usize| {
        let n = normals[f];
        let a = AXES[label];
        1.0 - (n.x * a[0] + n.y * a[1] + n.z * a[2])
    };
    let mut labels: Vec<_> = (0..count)
        .map(|f| {
            (0..6)
                .min_by(|&a, &b| data(f, a).total_cmp(&data(f, b)))
                .unwrap()
        })
        .collect();
    for _ in 0..iterations {
        let mut changed = false;
        for f in 0..count {
            let best = (0..6)
                .min_by(|&a, &b| {
                    let energy = |label| {
                        data(f, label)
                            + smoothness
                                * neighbors[f]
                                    .iter()
                                    .filter(|&&other| labels[other] != label)
                                    .count() as f64
                    };
                    energy(a).total_cmp(&energy(b))
                })
                .unwrap();
            if best != labels[f] {
                labels[f] = best;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    (0..mesh.edge_count())
        .filter(|&e| {
            let (c0, Some(c1)) = mesh.edge_corners(e) else {
                return false;
            };
            labels[mesh.corner_face(c0)] != labels[mesh.corner_face(c1)]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    #[test]
    fn perpendicular_planes_cut_their_shared_edge() {
        let mesh = Mesh::new(
            vec![
                DVec3::ZERO,
                DVec3::X,
                DVec3::new(1.0, 1.0, 0.0),
                DVec3::Y,
                DVec3::new(0.0, 0.0, 1.0),
                DVec3::new(1.0, 0.0, 1.0),
            ],
            &[[0, 1, 2, 3], [1, 0, 4, 5]],
        )
        .unwrap();
        let cuts = seam_edges(&mesh, 0.25, 12);
        assert_eq!(cuts.len(), 1);
        let [a, b] = mesh.edge_vertices(cuts[0]);
        assert_eq!([a.min(b), a.max(b)], [0, 1]);
    }
}
