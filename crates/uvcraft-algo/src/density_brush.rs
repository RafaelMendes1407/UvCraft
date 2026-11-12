//! Paint local texel-density targets and relax UV area toward them.

use glam::DVec2;
use uvcraft_core::{Mesh, UvLayer};

fn face_uv_area(mesh: &Mesh, uv: &UvLayer, face: usize) -> f64 {
    mesh.fan_triangles(face)
        .map(|tri| {
            let [a, b, c] = tri.map(|corner| uv.uv_at_corner(corner));
            0.5 * (b - a).perp_dot(c - a)
        })
        .sum::<f64>()
        .abs()
}

/// Blend one brush stroke into per-face target densities (pixels per 3D unit).
/// Unpainted faces have `None`; their current density is used when first hit.
#[allow(clippy::too_many_arguments)]
pub fn paint_targets(
    mesh: &Mesh,
    uv: &UvLayer,
    targets: &mut [Option<f64>],
    faces: &[usize],
    weights: &[f64],
    density: f64,
    resolution: u32,
    strength: f64,
) -> Result<usize, String> {
    if targets.len() != mesh.face_count()
        || faces.len() != weights.len()
        || !density.is_finite()
        || density <= 0.0
        || resolution == 0
        || !strength.is_finite()
        || !(0.0..=1.0).contains(&strength)
        || faces.iter().any(|&face| face >= mesh.face_count())
        || weights
            .iter()
            .any(|&weight| !weight.is_finite() || !(0.0..=1.0).contains(&weight))
    {
        return Err("invalid density brush face, weight, target or resolution".into());
    }
    let mut changed = 0;
    for (&face, &weight) in faces.iter().zip(weights) {
        let alpha = weight * strength;
        if alpha == 0.0 {
            continue;
        }
        let area_3d = mesh.face_area_normal(face).length();
        let current = if area_3d > 1e-20 {
            resolution as f64 * (face_uv_area(mesh, uv, face) / area_3d).sqrt()
        } else {
            density
        };
        let before = targets[face].unwrap_or(current);
        let next = before + alpha * (density - before);
        if next.is_finite() && next > 0.0 {
            changed += usize::from(targets[face] != Some(next));
            targets[face] = Some(next);
        }
    }
    Ok(changed)
}

/// Reduce per-face texel-density error while keeping pins and face orientation.
/// This is an iterative area projection: each face requests a local scale,
/// shared UV vertices average those requests, and a line search prevents new
/// triangle flips. Returns how many iterations moved UV vertices.
pub fn optimize_targets(
    mesh: &Mesh,
    uv: &mut UvLayer,
    targets: &[Option<f64>],
    pins: &[bool],
    resolution: u32,
    iterations: usize,
) -> Result<usize, String> {
    if targets.len() != mesh.face_count()
        || pins.len() != mesh.vertex_count()
        || resolution == 0
        || iterations > 1000
        || targets
            .iter()
            .flatten()
            .any(|&d| !d.is_finite() || d <= 0.0)
    {
        return Err("invalid density optimization targets, pins, resolution or iterations".into());
    }
    let mut fixed = vec![false; uv.uvs.len()];
    for corner in 0..mesh.corner_count() {
        fixed[uv.corner_uv(corner)] |= pins[mesh.corner_vertex(corner)];
    }
    let triangles: Vec<[usize; 3]> = (0..mesh.face_count())
        .flat_map(|face| mesh.fan_triangles(face))
        .map(|tri| tri.map(|corner| uv.corner_uv(corner)))
        .collect();
    let mut moved = 0;
    for _ in 0..iterations {
        let before = uv.uvs.clone();
        let mut delta = vec![DVec2::ZERO; uv.uvs.len()];
        let mut count = vec![0u32; uv.uvs.len()];
        for (face, target) in targets.iter().enumerate() {
            let Some(target) = target else {
                continue;
            };
            let area_3d = mesh.face_area_normal(face).length();
            let area_uv = face_uv_area(mesh, uv, face);
            if area_3d <= 1e-20 || area_uv <= 1e-20 {
                continue;
            }
            let desired = area_3d * (target / resolution as f64).powi(2);
            let factor = (desired / area_uv).sqrt().clamp(0.75, 1.25);
            let corners: Vec<_> = mesh.face_corners(face).collect();
            let center = corners
                .iter()
                .map(|&corner| uv.uv_at_corner(corner))
                .sum::<DVec2>()
                / corners.len() as f64;
            for corner in corners {
                let id = uv.corner_uv(corner);
                delta[id] += (uv.uvs[id] - center) * (factor - 1.0);
                count[id] += 1;
            }
        }
        let candidate: Vec<DVec2> = (0..uv.uvs.len())
            .map(|id| {
                if fixed[id] || count[id] == 0 {
                    before[id]
                } else {
                    before[id] + 0.5 * delta[id] / count[id] as f64
                }
            })
            .collect();
        if candidate == before {
            break;
        }
        let baseline: Vec<f64> = triangles
            .iter()
            .map(|&[a, b, c]| (before[b] - before[a]).perp_dot(before[c] - before[a]))
            .collect();
        let mut step = 1.0;
        loop {
            for id in 0..uv.uvs.len() {
                uv.uvs[id] = before[id].lerp(candidate[id], step);
            }
            if triangles.iter().zip(&baseline).all(|(&[a, b, c], &old)| {
                let new = (uv.uvs[b] - uv.uvs[a]).perp_dot(uv.uvs[c] - uv.uvs[a]);
                old.abs() <= 1e-14 || old * new > 0.0
            }) {
                break;
            }
            step *= 0.5;
            if step < 1.0 / 4096.0 {
                uv.uvs.clone_from(&before);
                break;
            }
        }
        if uv.uvs == before {
            break;
        }
        moved += 1;
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    #[test]
    fn painted_face_reaches_higher_density_without_moving_pins() {
        let mesh = Mesh::new(
            vec![DVec3::ZERO, DVec3::X, DVec3::ONE, DVec3::Y],
            &[[0, 1, 2, 3]],
        )
        .unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 3],
            uvs: vec![DVec2::ZERO, DVec2::X, DVec2::ONE, DVec2::Y],
        };
        let mut targets = vec![None];
        paint_targets(&mesh, &uv, &mut targets, &[0], &[1.0], 200.0, 100, 1.0).unwrap();
        assert_eq!(targets, vec![Some(200.0)]);
        let pin = uv.uvs[0];
        optimize_targets(
            &mesh,
            &mut uv,
            &targets,
            &[true, false, false, false],
            100,
            40,
        )
        .unwrap();
        assert_eq!(uv.uvs[0], pin);
        assert!(face_uv_area(&mesh, &uv, 0) > 1.5);
    }

    #[test]
    fn invalid_paint_and_all_pins_leave_uvs_unchanged() {
        let mesh = Mesh::new(vec![DVec3::ZERO; 3], &[[0, 1, 2]]).unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2],
            uvs: vec![DVec2::ZERO, DVec2::X, DVec2::Y],
        };
        let before = uv.clone();
        let mut targets = vec![None];
        assert!(paint_targets(&mesh, &uv, &mut targets, &[99], &[1.0], 100.0, 100, 1.0).is_err());
        assert_eq!(targets, vec![None]);
        targets[0] = Some(200.0);
        optimize_targets(&mesh, &mut uv, &targets, &[true; 3], 100, 10).unwrap();
        assert_eq!(uv, before);
    }
}
