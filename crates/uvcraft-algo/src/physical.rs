//! Physical measurements and 1:1 UV scale for flat-pattern workflows.

use glam::DVec2;
use serde::{Deserialize, Serialize};
use uvcraft_core::{Islands, Mesh, UvLayer};

use crate::analysis::analyze_island;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Millimeter,
    Centimeter,
    Meter,
    Inch,
}

impl Unit {
    pub fn meters(self) -> f64 {
        match self {
            Self::Millimeter => 0.001,
            Self::Centimeter => 0.01,
            Self::Meter => 1.0,
            Self::Inch => 0.0254,
        }
    }

    /// Number of `target` units in one unit of `self`.
    pub fn to(self, target: Unit) -> f64 {
        self.meters() / target.meters()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PhysicalError {
    #[error("edge index is out of range")]
    InvalidEdge,
    #[error("island index is out of range")]
    InvalidIsland,
    #[error("island has degenerate or non-finite 3D/UV area")]
    DegenerateIsland,
    #[error("island is not planar enough for exact 1:1 projection")]
    NonPlanarIsland,
    #[error("one UV vertex represents different 3D positions")]
    SharedUvPosition,
}

pub fn edge_length(
    mesh: &Mesh,
    edge: usize,
    mesh_unit: Unit,
    output_unit: Unit,
) -> Result<f64, PhysicalError> {
    if edge >= mesh.edge_count() {
        return Err(PhysicalError::InvalidEdge);
    }
    Ok(mesh.edge_length(edge) * mesh_unit.to(output_unit))
}

pub fn island_area(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    island: usize,
    mesh_unit: Unit,
    output_unit: Unit,
) -> Result<f64, PhysicalError> {
    if island >= islands.count() {
        return Err(PhysicalError::InvalidIsland);
    }
    let area = analyze_island(mesh, uv, islands, island, 1).area_3d;
    Ok(area * mesh_unit.to(output_unit).powi(2))
}

/// Uniformly scales each island's current UVs so one UV unit equals one
/// `uv_unit` of physical distance. Coordinates are not normalized to 0–1.
/// The edit is atomic: every target is validated before any UV changes.
pub fn scale_uv_1to1(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    mesh_unit: Unit,
    uv_unit: Unit,
) -> Result<(), PhysicalError> {
    let mut edits = Vec::with_capacity(targets.len());
    for &island in targets {
        if island >= islands.count() {
            return Err(PhysicalError::InvalidIsland);
        }
        let report = analyze_island(mesh, uv, islands, island, 1);
        if report.degenerate_triangles != 0
            || !report.area_3d.is_finite()
            || !report.uv_area.is_finite()
            || report.area_3d <= 0.0
            || report.uv_area <= 0.0
        {
            return Err(PhysicalError::DegenerateIsland);
        }
        let indices = islands.uv_indices(island, mesh, uv);
        let center = (DVec2::from(report.bbox_min) + DVec2::from(report.bbox_max)) * 0.5;
        let scale = mesh_unit.to(uv_unit) * (report.area_3d / report.uv_area).sqrt();
        if !scale.is_finite() {
            return Err(PhysicalError::DegenerateIsland);
        }
        edits.push((indices, center, scale));
    }
    for (indices, center, scale) in edits {
        for index in indices {
            let p = &mut uv.uvs[index as usize];
            *p = center + (*p - center) * scale;
        }
    }
    Ok(())
}

/// Projects planar islands directly from their 3D geometry. Every edge length
/// is preserved in physical units; nonplanar islands are rejected atomically.
/// UV centers are retained, so coordinates are never normalized to 0–1.
pub fn project_planar_1to1(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    mesh_unit: Unit,
    uv_unit: Unit,
) -> Result<(), PhysicalError> {
    let mut edits = Vec::new();
    for &island in targets {
        if island >= islands.count() {
            return Err(PhysicalError::InvalidIsland);
        }
        let faces = islands.faces(island);
        let corners: Vec<_> = faces
            .iter()
            .flat_map(|&f| mesh.face_corners(f as usize))
            .collect();
        let Some(&first) = corners.first() else {
            return Err(PhysicalError::DegenerateIsland);
        };
        let origin = mesh.corner_position(first);
        let mut basis = None;
        for &face in faces {
            for [a, b, c] in mesh.fan_triangles(face as usize) {
                let (p, q, r) = (
                    mesh.corner_position(a),
                    mesh.corner_position(b),
                    mesh.corner_position(c),
                );
                let cross = (q - p).cross(r - p);
                if cross.length_squared() > 1e-24 {
                    let u = (q - p).normalize();
                    let n = cross.normalize();
                    basis = Some((u, n.cross(u), n));
                    break;
                }
            }
            if basis.is_some() {
                break;
            }
        }
        let Some((u_axis, v_axis, normal)) = basis else {
            return Err(PhysicalError::DegenerateIsland);
        };
        let mut extent: f64 = 0.0;
        for &corner in &corners {
            extent = extent.max(mesh.corner_position(corner).distance(origin));
        }
        let tolerance = extent.max(1.0) * 1e-8;
        let mut projected = std::collections::BTreeMap::new();
        let mut center_before = DVec2::ZERO;
        let mut count = 0.0;
        let scale = mesh_unit.to(uv_unit);
        for &corner in &corners {
            let p = mesh.corner_position(corner);
            let delta = p - origin;
            if delta.dot(normal).abs() > tolerance {
                return Err(PhysicalError::NonPlanarIsland);
            }
            let uv_index = uv.corner_uv[corner] as usize;
            let exact = DVec2::new(delta.dot(u_axis), delta.dot(v_axis)) * scale;
            match projected.insert(uv_index, exact) {
                Some(previous) if previous.distance(exact) > tolerance * scale => {
                    return Err(PhysicalError::SharedUvPosition);
                }
                _ => {}
            }
        }
        let center_after = projected.values().copied().sum::<DVec2>() / projected.len() as f64;
        for &index in projected.keys() {
            center_before += uv.uvs[index];
            count += 1.0;
        }
        center_before /= count;
        let offset = center_before - center_after;
        edits.extend(projected.into_iter().map(|(index, p)| (index, p + offset)));
    }
    for (index, position) in edits {
        uv.uvs[index] = position;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    #[test]
    fn units_and_real_scale() {
        let mesh = Mesh::new(
            vec![
                DVec3::ZERO,
                DVec3::new(2.0, 0.0, 0.0),
                DVec3::new(2.0, 1.0, 0.0),
                DVec3::new(0.0, 1.0, 0.0),
            ],
            &[[0, 1, 2, 3]],
        )
        .unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 3],
            uvs: vec![DVec2::ZERO, DVec2::X, DVec2::ONE, DVec2::Y],
        };
        let islands = Islands::compute(&mesh, &uv);
        assert!(
            (island_area(&mesh, &uv, &islands, 0, Unit::Meter, Unit::Centimeter).unwrap()
                - 20_000.0)
                .abs()
                < 1e-9
        );
        let edge = (0..mesh.edge_count())
            .find(|&e| {
                let [a, b] = mesh.edge_vertices(e);
                (a == 0 && b == 1) || (a == 1 && b == 0)
            })
            .unwrap();
        assert!(
            (edge_length(&mesh, edge, Unit::Meter, Unit::Centimeter).unwrap() - 200.0).abs() < 1e-9
        );
        scale_uv_1to1(
            &mesh,
            &mut uv,
            &islands,
            &[0],
            Unit::Meter,
            Unit::Millimeter,
        )
        .unwrap();
        let bounds = uv
            .uvs
            .iter()
            .fold((DVec2::INFINITY, DVec2::NEG_INFINITY), |(lo, hi), &p| {
                (lo.min(p), hi.max(p))
            });
        let uv_area = (bounds.1.x - bounds.0.x) * (bounds.1.y - bounds.0.y);
        assert!((uv_area - 2_000_000.0).abs() < 1e-9);
        assert_eq!(Unit::Inch.to(Unit::Millimeter), 25.4);
    }

    #[test]
    fn degenerate_uv_rejects_without_mutation() {
        let mesh = Mesh::new(vec![DVec3::ZERO, DVec3::X, DVec3::Y], &[[0, 1, 2]]).unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2],
            uvs: vec![DVec2::ZERO; 3],
        };
        let islands = Islands::compute(&mesh, &uv);
        let before = uv.clone();
        assert_eq!(
            scale_uv_1to1(&mesh, &mut uv, &islands, &[0], Unit::Meter, Unit::Meter),
            Err(PhysicalError::DegenerateIsland)
        );
        assert_eq!(uv, before);
    }

    #[test]
    fn exact_planar_projection_preserves_each_edge_length() {
        let mesh = Mesh::new(
            vec![
                DVec3::ZERO,
                DVec3::new(2.0, 0.0, 0.0),
                DVec3::new(2.0, 1.0, 0.0),
                DVec3::new(0.0, 1.0, 0.0),
            ],
            &[[0, 1, 2, 3]],
        )
        .unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 3],
            uvs: vec![DVec2::ZERO, DVec2::X, DVec2::ONE, DVec2::Y],
        };
        let islands = Islands::compute(&mesh, &uv);
        project_planar_1to1(
            &mesh,
            &mut uv,
            &islands,
            &[0],
            Unit::Meter,
            Unit::Millimeter,
        )
        .unwrap();
        for e in 0..mesh.edge_count() {
            let (c, _) = mesh.edge_corners(e);
            let length_uv = uv.uv_at_corner(c).distance(uv.uv_at_corner(mesh.next(c)));
            assert!((length_uv - mesh.edge_length(e) * 1000.0).abs() < 1e-8);
        }
    }

    #[test]
    fn exact_projection_rejects_nonplanar_without_mutation() {
        let mesh = Mesh::new(
            vec![DVec3::ZERO, DVec3::X, DVec3::Y, DVec3::new(1.0, 1.0, 0.5)],
            &[[0, 1, 2], [1, 3, 2]],
        )
        .unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 1, 3, 2],
            uvs: vec![DVec2::ZERO, DVec2::X, DVec2::Y, DVec2::ONE],
        };
        let islands = Islands::compute(&mesh, &uv);
        let before = uv.clone();
        assert_eq!(
            project_planar_1to1(&mesh, &mut uv, &islands, &[0], Unit::Meter, Unit::Meter),
            Err(PhysicalError::NonPlanarIsland)
        );
        assert_eq!(uv, before);
    }
}
