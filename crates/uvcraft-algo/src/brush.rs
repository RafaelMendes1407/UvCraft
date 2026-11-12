//! Local UV brush edits. A stroke supplies its accumulated vertex weights.

use std::collections::BTreeSet;

use glam::DVec2;
use serde::{Deserialize, Serialize};
use uvcraft_core::{Mesh, UvLayer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrushMode {
    Move,
    Smooth,
    Relax,
}

/// Applies one stroke atomically. Pinned mesh vertices remain fixed in every mode.
#[allow(clippy::too_many_arguments)] // Mesh, UV and pin state plus the serialized stroke fields.
pub fn edit(
    mesh: &Mesh,
    uv: &mut UvLayer,
    pins: &[bool],
    vertices: &[usize],
    weights: &[f64],
    mode: BrushMode,
    delta: DVec2,
    strength: f64,
    iterations: usize,
) -> Result<(), String> {
    if vertices.len() != weights.len() || pins.len() != mesh.vertex_count() {
        return Err("brush vertex, weight or pin count mismatch".into());
    }
    if !delta.is_finite()
        || !strength.is_finite()
        || !(0.0..=1.0).contains(&strength)
        || !(1..=100).contains(&iterations)
    {
        return Err("invalid brush delta, strength or iteration count".into());
    }
    let mut influence = vec![0.0_f64; uv.uvs.len()];
    for (&vertex, &weight) in vertices.iter().zip(weights) {
        if vertex >= uv.uvs.len() || !weight.is_finite() || !(0.0..=1.0).contains(&weight) {
            return Err("invalid brush vertex or weight".into());
        }
        influence[vertex] = influence[vertex].max(weight * strength);
    }
    let mut pinned_uv = vec![false; uv.uvs.len()];
    let mut neighbors = vec![BTreeSet::new(); uv.uvs.len()];
    for face in 0..mesh.face_count() {
        for corner in mesh.face_corners(face) {
            let next = mesh.next(corner);
            let a = uv.corner_uv[corner] as usize;
            let b = uv.corner_uv[next] as usize;
            pinned_uv[a] |= pins[mesh.corner_vertex(corner)];
            neighbors[a].insert(b);
            neighbors[b].insert(a);
        }
    }
    for (i, pinned) in pinned_uv.iter().enumerate() {
        if *pinned {
            influence[i] = 0.0;
        }
    }
    let rounds = if mode == BrushMode::Relax {
        iterations
    } else {
        1
    };
    for _ in 0..rounds {
        let old = uv.uvs.clone();
        for &i in vertices {
            let alpha = influence[i];
            if alpha == 0.0 {
                continue;
            }
            uv.uvs[i] = match mode {
                BrushMode::Move => old[i] + delta * alpha,
                BrushMode::Smooth | BrushMode::Relax => {
                    if neighbors[i].is_empty() {
                        old[i]
                    } else {
                        let avg = neighbors[i].iter().map(|&n| old[n]).sum::<DVec2>()
                            / neighbors[i].len() as f64;
                        old[i].lerp(avg, alpha)
                    }
                }
            };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    fn quad() -> (Mesh, UvLayer) {
        let mesh = Mesh::new(vec![DVec3::ZERO; 4], &[[0, 1, 2, 3]]).unwrap();
        let uv = UvLayer {
            name: "UV".into(),
            corner_uv: vec![0, 1, 2, 3],
            uvs: vec![
                DVec2::ZERO,
                DVec2::new(3.0, 0.0),
                DVec2::new(1.0, 1.0),
                DVec2::new(0.0, 1.0),
            ],
        };
        (mesh, uv)
    }

    #[test]
    fn move_is_weighted_and_respects_pins() {
        let (mesh, mut uv) = quad();
        edit(
            &mesh,
            &mut uv,
            &[false, true, false, false],
            &[0, 1],
            &[0.5, 1.0],
            BrushMode::Move,
            DVec2::new(2.0, 0.0),
            1.0,
            1,
        )
        .unwrap();
        assert_eq!(uv.uvs[0], DVec2::new(1.0, 0.0));
        assert_eq!(uv.uvs[1], DVec2::new(3.0, 0.0));
        assert_eq!(uv.uvs[2], DVec2::new(1.0, 1.0));
    }

    #[test]
    fn smooth_and_relax_only_change_weighted_vertices() {
        let (mesh, mut uv) = quad();
        edit(
            &mesh,
            &mut uv,
            &[false; 4],
            &[1],
            &[1.0],
            BrushMode::Smooth,
            DVec2::ZERO,
            1.0,
            1,
        )
        .unwrap();
        assert_eq!(uv.uvs[1], DVec2::new(0.5, 0.5));
        assert_eq!(uv.uvs[0], DVec2::ZERO);
        let (_, mut uv) = quad();
        edit(
            &mesh,
            &mut uv,
            &[false; 4],
            &[1],
            &[0.5],
            BrushMode::Relax,
            DVec2::ZERO,
            1.0,
            10,
        )
        .unwrap();
        assert!(uv.uvs[1].distance(DVec2::new(0.5, 0.5)) < 0.01);
        assert_eq!(uv.uvs[2], DVec2::new(1.0, 1.0));
    }

    #[test]
    fn invalid_stroke_keeps_uvs_unchanged() {
        let (mesh, mut uv) = quad();
        let before = uv.clone();
        assert!(
            edit(
                &mesh,
                &mut uv,
                &[false; 4],
                &[0, 99],
                &[1.0, 1.0],
                BrushMode::Move,
                DVec2::X,
                1.0,
                1
            )
            .is_err()
        );
        assert_eq!(uv, before);
    }
}
