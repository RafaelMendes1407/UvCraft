//! Global UV overlap barrier implemented as a line search on a solver step.

use std::collections::BTreeSet;

use uvcraft_core::{Islands, Mesh, UvLayer};

use crate::checks::find_overlaps;

fn pairs(mesh: &Mesh, uv: &UvLayer, islands: &Islands) -> BTreeSet<[usize; 2]> {
    find_overlaps(mesh, uv, islands)
        .face_pairs
        .into_iter()
        .map(|p| p.faces)
        .collect()
}

/// Prevent new overlapping face pairs, within or between islands. Existing
/// overlaps may disappear but cannot spread. Returns the accepted fraction of
/// the proposed UV displacement; 0 restores the input layout.
pub fn prevent_new_overlaps(
    mesh: &Mesh,
    before: &UvLayer,
    proposed: &mut UvLayer,
    islands: &Islands,
) -> Result<f64, String> {
    if before.corner_uv != proposed.corner_uv || before.uvs.len() != proposed.uvs.len() {
        return Err("overlap guard requires unchanged UV topology".into());
    }
    let baseline = pairs(mesh, before, islands);
    let acceptable = |layer: &UvLayer| pairs(mesh, layer, islands).is_subset(&baseline);
    if acceptable(proposed) {
        return Ok(1.0);
    }
    let full = proposed.uvs.clone();
    for power in 1..=12 {
        let fraction = 0.5f64.powi(power);
        for (index, p) in proposed.uvs.iter_mut().enumerate() {
            *p = before.uvs[index] + (full[index] - before.uvs[index]) * fraction;
        }
        if acceptable(proposed) {
            return Ok(fraction);
        }
    }
    proposed.uvs.clone_from(&before.uvs);
    Ok(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::{DVec2, DVec3};

    #[test]
    fn rejects_new_overlap_but_accepts_smaller_motion() {
        let mesh = Mesh::new(
            vec![
                DVec3::ZERO,
                DVec3::X,
                DVec3::Y,
                DVec3::new(2.0, 0.0, 0.0),
                DVec3::new(3.0, 0.0, 0.0),
                DVec3::new(2.0, 1.0, 0.0),
            ],
            &[[0, 1, 2], [3, 4, 5]],
        )
        .unwrap();
        let before = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 3, 4, 5],
            uvs: vec![
                DVec2::ZERO,
                DVec2::X,
                DVec2::Y,
                DVec2::new(2.0, 0.0),
                DVec2::new(3.0, 0.0),
                DVec2::new(2.0, 1.0),
            ],
        };
        let islands = Islands::compute(&mesh, &before);
        let mut proposed = before.clone();
        for p in &mut proposed.uvs[3..] {
            p.x -= 1.75;
        }
        let fraction = prevent_new_overlaps(&mesh, &before, &mut proposed, &islands).unwrap();
        assert!(fraction > 0.0 && fraction < 1.0);
        assert!(
            find_overlaps(&mesh, &proposed, &islands)
                .face_pairs
                .is_empty()
        );
    }
}
