//! Rectangularize a UV island whose quads form a complete topological grid.

use std::collections::{BTreeMap, BTreeSet};

use glam::DVec2;
use uvcraft_core::{Islands, Mesh, UvLayer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridReport {
    pub columns: usize,
    pub rows: usize,
    pub moved: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridError {
    InvalidIsland,
    NonQuad,
    NotRectangularGrid,
    Degenerate,
    PinnedConflict,
}

impl std::fmt::Display for GridError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidIsland => "island index is out of range",
            Self::NonQuad => "island contains a non-quad face",
            Self::NotRectangularGrid => "quads do not form a complete rectangular grid",
            Self::Degenerate => "grid has zero UV and 3D edge length",
            Self::PinnedConflict => "pins cannot all lie on one rectangular grid",
        })
    }
}
impl std::error::Error for GridError {}

fn put(coords: &mut BTreeMap<u32, (i32, i32)>, id: u32, p: (i32, i32)) -> Result<bool, GridError> {
    match coords.get(&id) {
        Some(&old) if old == p => Ok(false),
        Some(_) => Err(GridError::NotRectangularGrid),
        None => {
            coords.insert(id, p);
            Ok(true)
        }
    }
}

/// Replaces a complete quad-grid island with an orthogonal UV lattice.
/// Horizontal and vertical step lengths follow the mean existing UV edges
/// (or 3D edges when UVs are collapsed). Pins are fitted by a similarity
/// transform when possible; conflicting pins reject the edit atomically.
pub fn rectangularize(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    island: usize,
    pinned: &[bool],
) -> Result<GridReport, GridError> {
    rectangularize_impl(mesh, uv, islands, island, pinned, false)
}

/// Straightens a curved quad-grid island while retaining relative 3D edge
/// lengths along each row and column. A strip is a one-cell-wide grid.
/// Invalid topology or conflicting pins leave all UVs untouched.
pub fn straighten_island(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    island: usize,
    pinned: &[bool],
) -> Result<GridReport, GridError> {
    rectangularize_impl(mesh, uv, islands, island, pinned, true)
}

fn rectangularize_impl(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    island: usize,
    pinned: &[bool],
    preserve_lengths: bool,
) -> Result<GridReport, GridError> {
    if island >= islands.count() {
        return Err(GridError::InvalidIsland);
    }
    let mut faces = Vec::new();
    let mut adjacent: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
    let mut edge_uses: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    let mut mesh_vertex: BTreeMap<u32, usize> = BTreeMap::new();
    for &f in islands.faces(island) {
        let corners: Vec<usize> = mesh.face_corners(f as usize).collect();
        if corners.len() != 4 {
            return Err(GridError::NonQuad);
        }
        let face: [u32; 4] = [0, 1, 2, 3].map(|j| uv.corner_uv[corners[j]]);
        if face.iter().collect::<BTreeSet<_>>().len() != 4 {
            return Err(GridError::NotRectangularGrid);
        }
        for (j, &c) in corners.iter().enumerate() {
            let id = face[j];
            if mesh_vertex
                .insert(id, mesh.corner_vertex(c))
                .is_some_and(|old| old != mesh.corner_vertex(c))
            {
                return Err(GridError::NotRectangularGrid);
            }
            let next = face[(j + 1) % 4];
            adjacent.entry(id).or_default().insert(next);
            adjacent.entry(next).or_default().insert(id);
            let key = (id.min(next), id.max(next));
            let count = edge_uses.entry(key).or_default();
            *count += 1;
            if *count > 2 {
                return Err(GridError::NotRectangularGrid);
            }
        }
        faces.push(face);
    }
    let corners: Vec<u32> = adjacent
        .iter()
        .filter_map(|(&id, ns)| (ns.len() == 2).then_some(id))
        .collect();
    if corners.len() != 4 || adjacent.values().any(|ns| !(2..=4).contains(&ns.len())) {
        return Err(GridError::NotRectangularGrid);
    }
    let first = corners[0];
    let neighbors: Vec<u32> = adjacent[&first].iter().copied().collect();
    let (x_neighbor, y_neighbor) = (neighbors[0], neighbors[1]);
    let initial = faces
        .iter()
        .find(|face| {
            face.contains(&first) && face.contains(&x_neighbor) && face.contains(&y_neighbor)
        })
        .ok_or(GridError::NotRectangularGrid)?;
    let j = initial.iter().position(|&id| id == first).unwrap();
    let sign = if initial[(j + 1) % 4] == x_neighbor {
        1
    } else {
        -1
    };
    let mut coords: BTreeMap<u32, (i32, i32)> =
        BTreeMap::from([(first, (0, 0)), (x_neighbor, (1, 0)), (y_neighbor, (0, 1))]);
    loop {
        let mut changed = false;
        for face in &faces {
            for j in 0..4 {
                let (Some(&a), Some(&b)) = (coords.get(&face[j]), coords.get(&face[(j + 1) % 4]))
                else {
                    continue;
                };
                let e = (b.0 - a.0, b.1 - a.1);
                if e.0.abs() + e.1.abs() != 1 {
                    return Err(GridError::NotRectangularGrid);
                }
                let d = (-sign * e.1, sign * e.0);
                changed |= put(&mut coords, face[(j + 2) % 4], (b.0 + d.0, b.1 + d.1))?;
                changed |= put(&mut coords, face[(j + 3) % 4], (a.0 + d.0, a.1 + d.1))?;
            }
        }
        if !changed {
            break;
        }
    }
    if coords.len() != adjacent.len() {
        return Err(GridError::NotRectangularGrid);
    }
    if coords.keys().any(|&id| !uv.uvs[id as usize].is_finite()) {
        return Err(GridError::Degenerate);
    }
    let (min_x, max_x) = coords.values().fold((i32::MAX, i32::MIN), |(lo, hi), p| {
        (lo.min(p.0), hi.max(p.0))
    });
    let (min_y, max_y) = coords.values().fold((i32::MAX, i32::MIN), |(lo, hi), p| {
        (lo.min(p.1), hi.max(p.1))
    });
    let (columns, rows) = ((max_x - min_x) as usize, (max_y - min_y) as usize);
    if columns == 0
        || rows == 0
        || faces.len() != columns * rows
        || coords.len() != (columns + 1) * (rows + 1)
    {
        return Err(GridError::NotRectangularGrid);
    }
    let mut coordinate_vertex = BTreeMap::new();
    for (&id, &(x, y)) in &coords {
        if coordinate_vertex.insert((x, y), id).is_some() {
            return Err(GridError::NotRectangularGrid);
        }
    }
    let mut occupied_cells = BTreeSet::new();
    for face in &faces {
        let p = face.map(|id| coords[&id]);
        let e = (p[1].0 - p[0].0, p[1].1 - p[0].1);
        let d = (p[3].0 - p[0].0, p[3].1 - p[0].1);
        if e.0.abs() + e.1.abs() != 1
            || d.0.abs() + d.1.abs() != 1
            || e.0 * d.0 + e.1 * d.1 != 0
            || e.0 * d.1 - e.1 * d.0 != sign
            || p[2] != (p[1].0 + d.0, p[1].1 + d.1)
        {
            return Err(GridError::NotRectangularGrid);
        }
        let cell = (
            p.iter().map(|v| v.0).min().unwrap(),
            p.iter().map(|v| v.1).min().unwrap(),
        );
        if !occupied_cells.insert(cell) {
            return Err(GridError::NotRectangularGrid);
        }
    }
    let mut uv_length = [0.0; 2];
    let mut mesh_length = [0.0; 2];
    let mut counts = [0usize; 2];
    let mut segment_length = [vec![0.0; columns], vec![0.0; rows]];
    let mut segment_count = [vec![0usize; columns], vec![0usize; rows]];
    for &(a, b) in edge_uses.keys() {
        let (p, q) = (coords[&a], coords[&b]);
        let axis = if p.0 != q.0 { 0 } else { 1 };
        uv_length[axis] += uv.uvs[a as usize].distance(uv.uvs[b as usize]);
        let length = mesh.positions()[mesh_vertex[&a]].distance(mesh.positions()[mesh_vertex[&b]]);
        mesh_length[axis] += length;
        let segment = (if axis == 0 {
            p.0.min(q.0) - min_x
        } else {
            p.1.min(q.1) - min_y
        }) as usize;
        segment_length[axis][segment] += length;
        segment_count[axis][segment] += 1;
        counts[axis] += 1;
    }
    let mut step = [0.0; 2];
    for axis in 0..2 {
        step[axis] = if uv_length[axis] > 1e-12 {
            uv_length[axis]
        } else {
            mesh_length[axis]
        } / counts[axis] as f64;
        if !step[axis].is_finite() || step[axis] <= 0.0 {
            return Err(GridError::Degenerate);
        }
    }
    let mut offsets = [vec![0.0; columns + 1], vec![0.0; rows + 1]];
    for axis in 0..2 {
        let n = offsets[axis].len() - 1;
        let total: f64 = segment_length[axis].iter().sum();
        for segment in 0..n {
            let length = if preserve_lengths {
                if total <= 0.0 || segment_count[axis][segment] == 0 {
                    return Err(GridError::Degenerate);
                }
                // All grid rows/columns have the same count of parallel edges.
                // Normalize the 3D lengths to the original UV extent.
                step[axis] * n as f64 * segment_length[axis][segment] / total
            } else {
                step[axis]
            };
            if !length.is_finite() || length <= 0.0 {
                return Err(GridError::Degenerate);
            }
            offsets[axis][segment + 1] = offsets[axis][segment] + length;
        }
    }
    let id_at = |x, y| coordinate_vertex[&(x + min_x, y + min_y)];
    let p00 = uv.uvs[id_at(0, 0) as usize];
    let bx = uv.uvs[id_at(columns as i32, 0) as usize] - p00;
    let by = uv.uvs[id_at(0, rows as i32) as usize] - p00;
    let ux = if bx.length_squared() > 1e-24 {
        bx.normalize()
    } else {
        DVec2::X
    };
    let uy = ux.perp() * if bx.perp_dot(by) < 0.0 { -1.0 } else { 1.0 };
    let n = coords.len() as f64;
    let old_center = coords.keys().map(|&id| uv.uvs[id as usize]).sum::<DVec2>() / n;
    let mut target: BTreeMap<u32, DVec2> = coords
        .iter()
        .map(|(&id, &(x, y))| {
            (
                id,
                ux * offsets[0][(x - min_x) as usize] + uy * offsets[1][(y - min_y) as usize],
            )
        })
        .collect();
    let new_center = target.values().copied().sum::<DVec2>() / n;
    for point in target.values_mut() {
        *point += old_center - new_center;
    }
    let pins: Vec<u32> = coords
        .keys()
        .copied()
        .filter(|id| pinned.get(mesh_vertex[id]).copied().unwrap_or(false))
        .collect();
    match pins.as_slice() {
        [] => {}
        [id] => {
            let shift = uv.uvs[*id as usize] - target[id];
            for point in target.values_mut() {
                *point += shift;
            }
        }
        [a, b, ..] => {
            let (ta, tb) = (target[a], target[b]);
            let (oa, ob) = (uv.uvs[*a as usize], uv.uvs[*b as usize]);
            let (d0, d1) = (tb - ta, ob - oa);
            if d0.length_squared() <= 1e-24 || d1.length_squared() <= 1e-24 {
                return Err(GridError::PinnedConflict);
            }
            let scale = d1.length() / d0.length();
            let rotation = DVec2::from_angle(d0.perp_dot(d1).atan2(d0.dot(d1)));
            for point in target.values_mut() {
                *point = oa + rotation.rotate(*point - ta) * scale;
            }
        }
    }
    let tolerance = 1e-8 * step[0].max(step[1]).max(1.0);
    for &id in &pins {
        if target[&id].distance(uv.uvs[id as usize]) > tolerance {
            return Err(GridError::PinnedConflict);
        }
        target.insert(id, uv.uvs[id as usize]);
    }
    let mut moved = 0;
    for (id, point) in target {
        if uv.uvs[id as usize] != point {
            uv.uvs[id as usize] = point;
            moved += 1;
        }
    }
    Ok(GridReport {
        columns,
        rows,
        moved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    fn quad_grid(missing_last: bool) -> (Mesh, UvLayer, Islands) {
        let positions: Vec<DVec3> = (0..3)
            .flat_map(|y| (0..3).map(move |x| DVec3::new(x as f64, y as f64, 0.0)))
            .collect();
        let mut faces = vec![[0, 1, 4, 3], [1, 2, 5, 4], [3, 4, 7, 6], [4, 5, 8, 7]];
        if missing_last {
            faces.pop();
        }
        let mesh = Mesh::new(positions.clone(), &faces).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..mesh.corner_count())
                .map(|c| mesh.corner_vertex(c) as u32)
                .collect(),
            uvs: positions.iter().map(|p| DVec2::new(p.x, p.y)).collect(),
        };
        let islands = Islands::compute(&mesh, &uv);
        (mesh, uv, islands)
    }

    #[test]
    fn distorted_quad_grid_becomes_orthogonal_lattice() {
        let (mesh, mut uv, islands) = quad_grid(false);
        uv.uvs[4] += DVec2::new(0.3, -0.2);
        uv.uvs[1] += DVec2::new(0.1, 0.15);
        let result = rectangularize(&mesh, &mut uv, &islands, 0, &[]).unwrap();
        assert_eq!((result.columns, result.rows), (2, 2));
        assert!(result.moved > 0);
        let dx = uv.uvs[1] - uv.uvs[0];
        let dy = uv.uvs[3] - uv.uvs[0];
        assert!(dx.dot(dy).abs() < 1e-9);
        for y in 0..3 {
            for x in 0..3 {
                let p = uv.uvs[y * 3 + x];
                assert!(p.distance(uv.uvs[0] + dx * x as f64 + dy * y as f64) < 1e-9);
            }
        }
    }

    #[test]
    fn curved_strip_straightens_with_relative_segment_lengths() {
        let angles = [
            0.0_f64,
            std::f64::consts::FRAC_PI_6,
            std::f64::consts::FRAC_PI_2,
        ];
        let positions: Vec<DVec3> = angles
            .iter()
            .flat_map(|&angle| {
                [1.0, 2.0]
                    .map(move |radius| DVec3::new(radius * angle.cos(), radius * angle.sin(), 0.0))
            })
            .collect();
        let mesh = Mesh::new(positions.clone(), &[[0, 2, 3, 1], [2, 4, 5, 3]]).unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..mesh.corner_count())
                .map(|corner| mesh.corner_vertex(corner) as u32)
                .collect(),
            uvs: positions
                .iter()
                .map(|p| DVec2::new(p.x, p.y) * 0.25)
                .collect(),
        };
        let islands = Islands::compute(&mesh, &uv);
        assert_eq!(islands.count(), 1);
        let before = uv.clone();
        let report = straighten_island(&mesh, &mut uv, &islands, 0, &[]).unwrap();
        assert_eq!(report.columns * report.rows, 2);
        assert!(report.columns == 1 || report.rows == 1);
        assert!(report.moved > 0);
        let first = uv.uvs[0].distance(uv.uvs[2]);
        let second = uv.uvs[2].distance(uv.uvs[4]);
        let expected = positions[0].distance(positions[2]) / positions[2].distance(positions[4]);
        assert!((first / second - expected).abs() < 1e-9);
        assert!(
            (uv.uvs[2] - uv.uvs[0])
                .perp_dot(uv.uvs[4] - uv.uvs[2])
                .abs()
                < 1e-9
        );
        assert_eq!(uv.corner_uv, before.corner_uv);
    }

    #[test]
    fn pins_are_preserved_or_rejected_atomically() {
        let (mesh, mut uv, islands) = quad_grid(false);
        uv.uvs[4] += DVec2::new(0.4, 0.2);
        let mut pins = vec![false; mesh.vertex_count()];
        pins[0] = true;
        pins[8] = true;
        let original = uv.clone();
        let result = rectangularize(&mesh, &mut uv, &islands, 0, &pins).unwrap();
        assert!(result.moved > 0);
        assert_eq!(uv.uvs[0], original.uvs[0]);
        assert_eq!(uv.uvs[8], original.uvs[8]);

        let (mesh, mut uv, islands) = quad_grid(false);
        uv.uvs[2] += DVec2::new(0.3, 0.1);
        pins[2] = true;
        let before = uv.clone();
        assert_eq!(
            rectangularize(&mesh, &mut uv, &islands, 0, &pins),
            Err(GridError::PinnedConflict)
        );
        assert_eq!(uv, before);
    }

    #[test]
    fn incomplete_quad_patch_is_rejected_without_editing() {
        let (mesh, mut uv, islands) = quad_grid(true);
        let before = uv.clone();
        assert_eq!(
            rectangularize(&mesh, &mut uv, &islands, 0, &[]),
            Err(GridError::NotRectangularGrid)
        );
        assert_eq!(uv, before);
        assert_eq!(
            straighten_island(&mesh, &mut uv, &islands, 0, &[]),
            Err(GridError::NotRectangularGrid)
        );
        assert_eq!(uv, before);
    }

    #[test]
    fn single_skew_quad_becomes_rectangle() {
        let mesh = Mesh::new(
            vec![DVec3::ZERO, DVec3::X, DVec3::X + DVec3::Y, DVec3::Y],
            &[[0, 1, 2, 3]],
        )
        .unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 3],
            uvs: vec![
                DVec2::ZERO,
                DVec2::new(1.0, 0.1),
                DVec2::new(1.1, 1.1),
                DVec2::new(0.0, 1.0),
            ],
        };
        let islands = Islands::compute(&mesh, &uv);
        let report = rectangularize(&mesh, &mut uv, &islands, 0, &[]).unwrap();
        assert_eq!((report.columns, report.rows), (1, 1));
        let horizontal = uv.uvs[1] - uv.uvs[0];
        let vertical = uv.uvs[3] - uv.uvs[0];
        assert!(horizontal.dot(vertical).abs() < 1e-9);
        assert!((uv.uvs[2] - uv.uvs[1] - vertical).length() < 1e-9);
    }

    #[test]
    fn nonfinite_uv_is_rejected_without_editing() {
        let (mesh, mut uv, islands) = quad_grid(false);
        uv.uvs[4].x = f64::NAN;
        let before = uv.clone();
        assert_eq!(
            rectangularize(&mesh, &mut uv, &islands, 0, &[]),
            Err(GridError::Degenerate)
        );
        assert_eq!(uv.corner_uv, before.corner_uv);
        for (a, b) in uv.uvs.iter().zip(before.uvs.iter()) {
            assert_eq!(a.x.to_bits(), b.x.to_bits());
            assert_eq!(a.y.to_bits(), b.y.to_bits());
        }
    }
}
