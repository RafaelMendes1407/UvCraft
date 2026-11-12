//! Soft preservation of an island's existing boundary during a UV solve.
//!
//! The result minimizes a screened boundary energy: each boundary UV stays
//! near the newly solved position, while every boundary edge is encouraged to
//! keep its vector from the reference layout. This preserves both edge length
//! and border shape without turning the old border into a hard constraint.

use std::collections::BTreeMap;

use glam::DVec2;
use uvcraft_core::{Islands, Mesh, UvLayer};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BorderPreserveError {
    #[error("border weight must be finite and non-negative")]
    InvalidWeight,
    #[error("reference and result must use the same UV topology")]
    TopologyMismatch,
    #[error("island index {0} is out of range")]
    InvalidIsland(usize),
}

/// Adjusts newly solved UVs to retain the reference border shape.
///
/// `reference` is the UV layer before unfold or optimize; `result` is the
/// solved layer. Both must have the same UV topology. `weight = 0` is a no-op;
/// larger weights preserve the reference boundary more strongly. Pins are
/// hard constraints indexed by mesh vertex. The caller should split UVs shared
/// by unrelated islands before calling this function.
///
/// Returns the number of target islands whose boundary coordinates changed.
pub fn preserve_border_shape(
    mesh: &Mesh,
    reference: &UvLayer,
    result: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    pinned: &[bool],
    weight: f64,
) -> Result<usize, BorderPreserveError> {
    if !weight.is_finite() || weight < 0.0 {
        return Err(BorderPreserveError::InvalidWeight);
    }
    if reference.corner_uv != result.corner_uv || reference.uvs.len() != result.uvs.len() {
        return Err(BorderPreserveError::TopologyMismatch);
    }
    if let Some(&invalid) = targets.iter().find(|&&i| i >= islands.count()) {
        return Err(BorderPreserveError::InvalidIsland(invalid));
    }
    if weight == 0.0 {
        return Ok(0);
    }

    let mut changed = 0;
    let mut targets = targets.to_vec();
    targets.sort_unstable();
    targets.dedup();
    for island in targets {
        let mut vertices: BTreeMap<u32, (usize, bool)> = BTreeMap::new();
        let mut edges = Vec::new();
        for &f in islands.faces(island) {
            for c in mesh.face_corners(f as usize) {
                if !reference
                    .edge_kind(mesh, mesh.corner_edge(c))
                    .is_island_boundary()
                {
                    continue;
                }
                let next = mesh.next(c);
                let (a, b) = (reference.corner_uv[c], reference.corner_uv[next]);
                if a == b {
                    continue;
                }
                for (id, corner) in [(a, c), (b, next)] {
                    let is_pinned = pinned
                        .get(mesh.corner_vertex(corner))
                        .copied()
                        .unwrap_or(false);
                    vertices
                        .entry(id)
                        .and_modify(|entry| entry.1 |= is_pinned)
                        .or_insert((0, is_pinned));
                }
                edges.push((a, b));
            }
        }
        if edges.is_empty() {
            continue;
        }
        // A newly imported or collapsed UV layout has no useful border
        // shape to preserve during its first unfold.
        let reference_length_squared: f64 = edges
            .iter()
            .map(|&(a, b)| (reference.uvs[b as usize] - reference.uvs[a as usize]).length_squared())
            .sum();
        if reference_length_squared <= 1e-24 {
            continue;
        }

        let ids: Vec<u32> = vertices.keys().copied().collect();
        for (i, id) in ids.iter().enumerate() {
            vertices.get_mut(id).unwrap().0 = i;
        }
        let original: Vec<DVec2> = ids.iter().map(|&id| result.uvs[id as usize]).collect();
        let mut positions = original.clone();
        let mut neighbors: Vec<Vec<(usize, DVec2)>> = vec![Vec::new(); ids.len()];
        for (a, b) in edges {
            let (ia, ib) = (vertices[&a].0, vertices[&b].0);
            let delta = reference.uvs[b as usize] - reference.uvs[a as usize];
            neighbors[ia].push((ib, -delta));
            neighbors[ib].push((ia, delta));
        }

        // Gauss-Seidel solves the strictly convex quadratic because each
        // vertex has a unit attraction to its newly solved coordinate.
        for _ in 0..256 {
            let mut max_step: f64 = 0.0;
            for (i, &id) in ids.iter().enumerate() {
                if vertices[&id].1 {
                    continue;
                }
                let mut rhs = original[i];
                for &(j, offset) in &neighbors[i] {
                    rhs += weight * (positions[j] + offset);
                }
                let next = rhs / (1.0 + weight * neighbors[i].len() as f64);
                if !next.is_finite() {
                    return Err(BorderPreserveError::InvalidWeight);
                }
                max_step = max_step.max((next - positions[i]).length());
                positions[i] = next;
            }
            if max_step <= 1e-12 {
                break;
            }
        }

        // A soft boundary penalty must not introduce a new triangle flip.
        // Shrink the proposed move until all previously non-degenerate fan
        // triangles keep their orientation.
        let triangles: Vec<[usize; 3]> = islands
            .faces(island)
            .iter()
            .flat_map(|&f| mesh.fan_triangles(f as usize))
            .map(|tri| tri.map(|c| result.corner_uv[c] as usize))
            .collect();
        let before_areas: Vec<f64> = triangles
            .iter()
            .map(|&[a, b, c]| {
                (result.uvs[b] - result.uvs[a]).perp_dot(result.uvs[c] - result.uvs[a])
            })
            .collect();
        let mut scale = 1.0;
        loop {
            for (i, &id) in ids.iter().enumerate() {
                result.uvs[id as usize] = original[i].lerp(positions[i], scale);
            }
            if triangles
                .iter()
                .zip(&before_areas)
                .all(|(&[a, b, c], &before)| {
                    let after =
                        (result.uvs[b] - result.uvs[a]).perp_dot(result.uvs[c] - result.uvs[a]);
                    before.abs() <= 1e-14 || before * after > 0.0
                })
            {
                break;
            }
            scale *= 0.5;
            if scale < 1.0 / 4096.0 {
                for (i, &id) in ids.iter().enumerate() {
                    result.uvs[id as usize] = original[i];
                }
                break;
            }
        }
        changed += usize::from(
            ids.iter()
                .enumerate()
                .any(|(i, &id)| result.uvs[id as usize] != original[i]),
        );
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    fn square() -> (Mesh, UvLayer, Islands) {
        let mesh = Mesh::new(
            vec![DVec3::ZERO, DVec3::X, DVec3::ONE, DVec3::Y],
            &[[0, 1, 2], [0, 2, 3]],
        )
        .unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 0, 2, 3],
            uvs: vec![DVec2::ZERO, DVec2::X, DVec2::ONE, DVec2::Y],
        };
        let islands = Islands::compute(&mesh, &uv);
        (mesh, uv, islands)
    }

    fn boundary_error(reference: &UvLayer, result: &UvLayer) -> f64 {
        [(0, 1), (1, 2), (2, 3), (3, 0)]
            .iter()
            .map(|&(a, b)| {
                let before = reference.uvs[b] - reference.uvs[a];
                let after = result.uvs[b] - result.uvs[a];
                (after - before).length_squared()
            })
            .sum()
    }

    #[test]
    fn soft_penalty_reduces_border_shape_error_and_keeps_pins() {
        let (mesh, reference, islands) = square();
        let mut solved = reference.clone();
        solved.uvs[0] = DVec2::new(0.2, 0.1);
        solved.uvs[1] = DVec2::new(1.8, -0.1);
        solved.uvs[2] = DVec2::new(1.5, 1.6);
        let pinned = solved.uvs[0];
        let initial = boundary_error(&reference, &solved);
        assert_eq!(
            preserve_border_shape(
                &mesh,
                &reference,
                &mut solved,
                &islands,
                &[0],
                &[true],
                20.0
            )
            .unwrap(),
            1
        );
        assert_eq!(solved.uvs[0], pinned);
        assert!(boundary_error(&reference, &solved) < initial * 0.25);
        for [a, b, c] in [[0, 1, 2], [0, 2, 3]] {
            assert!((solved.uvs[b] - solved.uvs[a]).perp_dot(solved.uvs[c] - solved.uvs[a]) > 0.0);
        }
    }

    #[test]
    fn no_op_and_invalid_input_leave_result_untouched() {
        let (mesh, reference, islands) = square();
        let mut solved = reference.clone();
        solved.uvs[2] += DVec2::new(0.2, 0.3);
        let copy = solved.clone();
        assert_eq!(
            preserve_border_shape(&mesh, &reference, &mut solved, &islands, &[0], &[], 0.0)
                .unwrap(),
            0
        );
        assert_eq!(solved, copy);
        assert_eq!(
            preserve_border_shape(&mesh, &reference, &mut solved, &islands, &[0], &[], -1.0),
            Err(BorderPreserveError::InvalidWeight)
        );
        assert_eq!(solved, copy);
        solved.corner_uv[0] = 1;
        let copy = solved.clone();
        assert_eq!(
            preserve_border_shape(&mesh, &reference, &mut solved, &islands, &[0], &[], 1.0),
            Err(BorderPreserveError::TopologyMismatch)
        );
        assert_eq!(solved, copy);
    }

    #[test]
    fn collapsed_reference_does_not_pull_first_unfold_back_to_zero() {
        let (mesh, mut reference, islands) = square();
        reference.uvs.fill(DVec2::ZERO);
        let mut solved = UvLayer {
            uvs: vec![DVec2::ZERO, DVec2::X, DVec2::ONE, DVec2::Y],
            ..reference.clone()
        };
        let copy = solved.clone();
        assert_eq!(
            preserve_border_shape(&mesh, &reference, &mut solved, &islands, &[0], &[], 10.0)
                .unwrap(),
            0
        );
        assert_eq!(solved, copy);
    }

    #[test]
    fn line_search_preserves_existing_triangle_orientation() {
        let (mesh, reference, islands) = square();
        let mut solved = reference.clone();
        // A reflected reference would reverse both faces if applied directly.
        let mut reflected = reference.clone();
        for p in &mut reflected.uvs {
            p.x = -p.x;
        }
        let before = solved.clone();
        preserve_border_shape(&mesh, &reflected, &mut solved, &islands, &[0], &[], 100.0).unwrap();
        for [a, b, c] in [[0, 1, 2], [0, 2, 3]] {
            let area = (solved.uvs[b] - solved.uvs[a]).perp_dot(solved.uvs[c] - solved.uvs[a]);
            assert!(area > 0.0);
        }
        assert!(solved.uvs.iter().zip(&before.uvs).any(|(a, b)| a != b));
    }
}
