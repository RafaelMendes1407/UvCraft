//! Initial projections. Planar projections keep the UV topology and only set
//! coordinates; box projection first cuts between faces facing different axes;
//! cylindrical and spherical projections cut each island where it wraps around
//! the axis.

use glam::{DMat4, DVec2, DVec3, DVec4};
use serde::{Deserialize, Serialize};
use uvcraft_core::{Islands, Mesh, UvLayer};

use crate::seams::{cut_edges, edge_corner_pairs};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    X,
    Y,
    Z,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Projection {
    /// Along a world axis, for all target islands together.
    Planar(Axis),
    /// Each island along its own area-weighted average normal.
    Best,
    /// Each face along its dominant axis (±X, ±Y, ±Z); cuts between groups.
    Box,
    /// Each island unrolled around a world axis through its bbox center.
    Cylindrical(Axis),
    /// Each island mapped by longitude/latitude around a world axis through
    /// its bbox center.
    Spherical(Axis),
    /// Orthographic projection along a captured camera's screen axes.
    CameraOrthographic(CameraOrthographic),
    /// Perspective projection through a captured world-to-clip matrix.
    CameraPerspective(CameraPerspective),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CameraPerspective {
    /// Column-major world-to-clip matrix using Z in [0, 1].
    pub view_proj: [f64; 16],
}

impl CameraPerspective {
    pub fn valid(self) -> bool {
        self.view_proj.iter().all(|value| value.is_finite())
    }
}

/// Projects selected islands as seen by a perspective camera. Depth clipping
/// is checked before changing UVs, so a near-plane crossing is never turned
/// into non-finite UV coordinates or a partial edit.
pub fn project_camera_perspective(
    mesh: &Mesh,
    uv: &mut UvLayer,
    targets: &[usize],
    camera: CameraPerspective,
) -> Result<(), String> {
    if !camera.valid() {
        return Err("camera matrix must be finite".into());
    }
    let matrix = DMat4::from_cols_array(&camera.view_proj);
    let islands = Islands::compute(mesh, uv);
    let mut projected = Vec::new();
    for &island in targets {
        for &face in islands.faces(island) {
            for corner in mesh.face_corners(face as usize) {
                let p = mesh.corner_position(corner);
                let clip = matrix * DVec4::new(p.x, p.y, p.z, 1.0);
                if !clip.is_finite() || clip.w <= 1e-9 || clip.z < -1e-6 || clip.z > clip.w + 1e-6 {
                    return Err("selected geometry crosses the camera near/far plane or lies behind the camera".into());
                }
                let mapped = DVec2::new(clip.x / clip.w, clip.y / clip.w);
                if !mapped.is_finite() {
                    return Err("camera projection produced non-finite coordinates".into());
                }
                projected.push((uv.corner_uv[corner], mapped));
            }
        }
    }
    let mut touched = Vec::with_capacity(projected.len());
    for (index, value) in projected {
        uv.uvs[index as usize] = value;
        touched.push(index);
    }
    touched.sort_unstable();
    touched.dedup();
    fit_unit_square(uv, &touched);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CameraOrthographic {
    pub right: [f64; 3],
    pub up: [f64; 3],
    /// Width divided by height of the camera viewport.
    pub aspect: f64,
}

impl CameraOrthographic {
    pub fn valid(self) -> bool {
        let right = DVec3::from_array(self.right);
        let up = DVec3::from_array(self.up);
        self.aspect.is_finite()
            && self.aspect > 0.0
            && right.is_finite()
            && up.is_finite()
            && right.length_squared() > 1e-24
            && up.length_squared() > 1e-24
            && right.cross(up).length_squared() > 1e-24
    }
}

/// Tangent basis (t, b) with t × b = n, so front faces keep their winding.
fn basis(n: DVec3) -> (DVec3, DVec3) {
    let n = n.normalize_or(DVec3::Z);
    let helper = if n.z.abs() < 0.9 { DVec3::Z } else { DVec3::Y };
    // Prefer "up" = world-ish vertical for readable results.
    let t = helper.cross(n).normalize();
    (t, n.cross(t))
}

/// Basis for one of the 6 box directions (label = axis * 2 + negative).
fn box_basis(label: usize) -> (DVec3, DVec3) {
    match label {
        0 => (DVec3::NEG_Z, DVec3::Y), // +X
        1 => (DVec3::Z, DVec3::Y),     // -X
        2 => (DVec3::X, DVec3::NEG_Z), // +Y
        3 => (DVec3::X, DVec3::Z),     // -Y
        4 => (DVec3::X, DVec3::Y),     // +Z
        _ => (DVec3::NEG_X, DVec3::Y), // -Z
    }
}

fn box_label(n: DVec3) -> usize {
    let a = n.abs();
    let axis = if a.x >= a.y && a.x >= a.z {
        0
    } else if a.y >= a.z {
        1
    } else {
        2
    };
    axis * 2 + usize::from(n[axis] < 0.0)
}

/// Projects the target islands, then fits them together into the 0–1 square
/// (uniform scale, so relative texel density is kept).
pub fn project(mesh: &Mesh, uv: &mut UvLayer, targets: &[usize], mode: Projection) {
    let islands = Islands::compute(mesh, uv);
    let faces: Vec<usize> = targets
        .iter()
        .flat_map(|&i| islands.faces(i).iter().map(|&f| f as usize))
        .collect();

    if let Projection::Cylindrical(axis) | Projection::Spherical(axis) = mode {
        let spherical = matches!(mode, Projection::Spherical(_));
        let touched = wrap_project(mesh, uv, &islands, targets, axis, spherical);
        fit_unit_square(uv, &touched);
        return;
    }
    if let Projection::CameraOrthographic(camera) = mode {
        if !camera.valid() {
            return;
        }
        let right = DVec3::from_array(camera.right).normalize();
        let up = DVec3::from_array(camera.up).normalize();
        let mut touched = Vec::new();
        for f in faces {
            for c in mesh.face_corners(f) {
                let p = mesh.corner_position(c);
                let i = uv.corner_uv[c];
                uv.uvs[i as usize] = DVec2::new(p.dot(right) / camera.aspect, p.dot(up));
                touched.push(i);
            }
        }
        touched.sort_unstable();
        touched.dedup();
        fit_unit_square(uv, &touched);
        return;
    }
    if let Projection::CameraPerspective(camera) = mode {
        let _ = project_camera_perspective(mesh, uv, targets, camera);
        return;
    }

    // Per-face basis.
    let mut face_basis: Vec<(usize, (DVec3, DVec3))> = Vec::with_capacity(faces.len());
    match mode {
        Projection::Planar(axis) => {
            let b = match axis {
                Axis::X => box_basis(0),
                Axis::Y => box_basis(2),
                Axis::Z => box_basis(4),
            };
            face_basis.extend(faces.iter().map(|&f| (f, b)));
        }
        Projection::Best => {
            for &i in targets {
                let n: DVec3 = islands
                    .faces(i)
                    .iter()
                    .map(|&f| mesh.face_area_normal(f as usize))
                    .sum();
                let b = basis(n);
                face_basis.extend(islands.faces(i).iter().map(|&f| (f as usize, b)));
            }
        }
        Projection::Cylindrical(_) | Projection::Spherical(_) => unreachable!(),
        Projection::CameraOrthographic(_) => unreachable!(),
        Projection::CameraPerspective(_) => unreachable!(),
        Projection::Box => {
            let mut label = vec![usize::MAX; mesh.face_count()];
            for &f in &faces {
                label[f] = box_label(mesh.face_area_normal(f));
            }
            let cuts: Vec<usize> = (0..mesh.edge_count())
                .filter(|&e| {
                    edge_corner_pairs(mesh, e).is_some_and(|[(a0, a1), _]| {
                        let (f0, f1) = (mesh.corner_face(a0), mesh.corner_face(a1));
                        label[f0] != usize::MAX && label[f1] != usize::MAX && label[f0] != label[f1]
                    })
                })
                .collect();
            cut_edges(mesh, uv, &cuts);
            face_basis.extend(faces.iter().map(|&f| (f, box_basis(label[f]))));
        }
    }

    let mut touched = Vec::new();
    for (f, (t, b)) in face_basis {
        for c in mesh.face_corners(f) {
            let p = mesh.corner_position(c);
            let i = uv.corner_uv[c];
            uv.uvs[i as usize] = DVec2::new(p.dot(t), p.dot(b));
            touched.push(i);
        }
    }
    touched.sort_unstable();
    touched.dedup();
    fit_unit_square(uv, &touched);
}

/// Right-handed frame (e1, e2, axis): e1 × e2 = axis.
fn axis_frame(axis: Axis) -> (DVec3, DVec3, DVec3) {
    match axis {
        Axis::X => (DVec3::Y, DVec3::Z, DVec3::X),
        Axis::Y => (DVec3::Z, DVec3::X, DVec3::Y),
        Axis::Z => (DVec3::X, DVec3::Y, DVec3::Z),
    }
}

/// Cylindrical / spherical projection. Per island: the angle around the axis
/// is measured from the middle of the largest angular gap of its vertices (so
/// partial cylinders are not cut); each face unwraps its corners next to its
/// own centroid angle, and edges whose sides disagree by a full turn are cut.
/// u = angle × mean radius, v = height (cylinder) or latitude × mean radius
/// (sphere), so outward-facing faces are not flipped. Corners on the axis
/// (poles, cap centers) have no angle: each takes its face's angle and its own
/// UV vertex, as in a latitude/longitude map. Returns the UV vertices written.
fn wrap_project(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    axis: Axis,
    spherical: bool,
) -> Vec<u32> {
    use std::f64::consts::{PI, TAU};
    let (e1, e2, a) = axis_frame(axis);
    // Per corner: (angle, second coordinate before scaling, radius).
    let mut theta = vec![f64::NAN; mesh.corner_count()];
    let mut second = vec![0.0; mesh.corner_count()];
    let mut scale_of = vec![1.0; mesh.corner_count()];
    let mut on_axis = vec![false; mesh.corner_count()];
    for &i in targets {
        let faces = islands.faces(i);
        let corners = || faces.iter().flat_map(|&f| mesh.face_corners(f as usize));
        let (lo, hi) = corners().fold((DVec3::INFINITY, DVec3::NEG_INFINITY), |(lo, hi), c| {
            let p = mesh.corner_position(c);
            (lo.min(p), hi.max(p))
        });
        let center = (lo + hi) / 2.0;
        let raw = |p: DVec3| {
            let d = p - center;
            d.dot(e2).atan2(d.dot(e1))
        };
        // Largest gap between sorted vertex angles (the wrap-around gap included).
        let mut angles: Vec<f64> = corners().map(|c| raw(mesh.corner_position(c))).collect();
        angles.sort_by(f64::total_cmp);
        angles.dedup();
        let mut offset = 0.0;
        if let (Some(&first), Some(&last)) = (angles.first(), angles.last()) {
            let mut best = (first + TAU - last, last + (first + TAU - last) / 2.0);
            for w in angles.windows(2) {
                if w[1] - w[0] > best.0 {
                    best = (w[1] - w[0], (w[0] + w[1]) / 2.0);
                }
            }
            offset = best.1;
        }
        let rel = |p: DVec3| (raw(p) - offset).rem_euclid(TAU);
        let n = corners().count().max(1) as f64;
        let radius = corners()
            .map(|c| {
                let d = mesh.corner_position(c) - center;
                if spherical {
                    d.length()
                } else {
                    (d - a * d.dot(a)).length()
                }
            })
            .sum::<f64>()
            / n;
        let radius = if radius > 0.0 { radius } else { 1.0 };
        for &f in faces {
            let f = f as usize;
            let k = mesh.face_corners(f).len() as f64;
            let centroid: DVec3 = mesh
                .face_corners(f)
                .map(|c| mesh.corner_position(c))
                .sum::<DVec3>()
                / k;
            let tc = rel(centroid);
            for c in mesh.face_corners(f) {
                let p = mesh.corner_position(c);
                let d = p - center;
                if (d - a * d.dot(a)).length() <= 1e-9 * radius {
                    on_axis[c] = true;
                }
                let mut t = if on_axis[c] { tc } else { rel(p) };
                if t - tc > PI {
                    t -= TAU;
                } else if tc - t > PI {
                    t += TAU;
                }
                theta[c] = t;
                scale_of[c] = radius;
                second[c] = if spherical {
                    (d.dot(a) / d.length().max(1e-300)).clamp(-1.0, 1.0).asin() * radius
                } else {
                    d.dot(a)
                };
            }
        }
    }

    let cuts: Vec<usize> = (0..mesh.edge_count())
        .filter(|&e| {
            edge_corner_pairs(mesh, e).is_some_and(|pairs| {
                pairs.iter().any(|&(x, y)| {
                    !theta[x].is_nan() && !theta[y].is_nan() && (theta[x] - theta[y]).abs() > PI
                })
            })
        })
        .collect();
    cut_edges(mesh, uv, &cuts);
    for (c, _) in on_axis.iter().enumerate().filter(|(_, p)| **p) {
        uv.corner_uv[c] = uv.uvs.len() as u32;
        uv.uvs.push(DVec2::ZERO);
    }
    uv.compact();

    let mut touched = Vec::new();
    for c in 0..mesh.corner_count() {
        if !theta[c].is_nan() {
            let i = uv.corner_uv[c];
            uv.uvs[i as usize] = DVec2::new(theta[c] * scale_of[c], second[c]);
            touched.push(i);
        }
    }
    touched.sort_unstable();
    touched.dedup();
    touched
}

/// Uniformly scales and moves the given UV vertices into the 0–1 square.
pub fn fit_unit_square(uv: &mut UvLayer, ids: &[u32]) {
    if ids.is_empty() {
        return;
    }
    let (lo, hi) = ids
        .iter()
        .fold((DVec2::INFINITY, DVec2::NEG_INFINITY), |(lo, hi), &i| {
            (lo.min(uv.uvs[i as usize]), hi.max(uv.uvs[i as usize]))
        });
    let size = (hi - lo).max_element();
    let s = if size > 0.0 { 1.0 / size } else { 1.0 };
    for &i in ids {
        let p = &mut uv.uvs[i as usize];
        *p = (*p - lo) * s;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::analyze;
    use uvcraft_core::EdgeKind;

    #[test]
    fn orthographic_camera_projection_uses_screen_axes_and_aspect() {
        let mesh = Mesh::new(
            vec![
                DVec3::new(0.0, 0.0, 0.0),
                DVec3::new(2.0, 0.0, 3.0),
                DVec3::new(2.0, 1.0, 3.0),
                DVec3::new(0.0, 1.0, 0.0),
            ],
            &[[0, 1, 2, 3]],
        )
        .unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 3],
            uvs: vec![DVec2::ZERO; 4],
        };
        let camera = CameraOrthographic {
            right: [1.0, 0.0, 0.0],
            up: [0.0, 1.0, 0.0],
            aspect: 2.0,
        };
        project(&mesh, &mut uv, &[0], Projection::CameraOrthographic(camera));
        assert_eq!(uv.uvs, [DVec2::ZERO, DVec2::X, DVec2::ONE, DVec2::Y]);
        assert!(
            !CameraOrthographic {
                aspect: 0.0,
                ..camera
            }
            .valid()
        );
        assert!(
            !CameraOrthographic {
                up: [2.0, 0.0, 0.0],
                ..camera
            }
            .valid()
        );
    }

    #[test]
    fn perspective_camera_projects_depth_and_rejects_near_plane_without_edit() {
        let mesh = Mesh::new(
            vec![
                DVec3::new(0.0, 0.0, 2.0),
                DVec3::new(2.0, 0.0, 2.0),
                DVec3::new(2.0, 2.0, 4.0),
                DVec3::new(0.0, 2.0, 4.0),
            ],
            &[[0, 1, 2, 3]],
        )
        .unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 3],
            uvs: vec![DVec2::ZERO; 4],
        };
        let camera = CameraPerspective {
            view_proj: [
                1.0, 0.0, 0.0, 0.0, // x
                0.0, 1.0, 0.0, 0.0, // y
                0.0, 0.0, 1.0, 1.0, // z, w
                0.0, 0.0, -1.0, 0.0,
            ],
        };
        project_camera_perspective(&mesh, &mut uv, &[0], camera).unwrap();
        assert_eq!(
            uv.uvs,
            [
                DVec2::ZERO,
                DVec2::X,
                DVec2::splat(0.5),
                DVec2::new(0.0, 0.5)
            ]
        );
        let before = uv.clone();
        let behind = Mesh::new(
            vec![
                DVec3::new(0.0, 0.0, 0.5),
                DVec3::new(1.0, 0.0, 2.0),
                DVec3::new(0.0, 1.0, 2.0),
            ],
            &[[0, 1, 2]],
        )
        .unwrap();
        assert!(project_camera_perspective(&behind, &mut uv, &[0], camera).is_err());
        assert_eq!(uv, before);
    }

    fn cube() -> (Mesh, UvLayer) {
        let p = vec![
            DVec3::new(0., 0., 0.),
            DVec3::new(1., 0., 0.),
            DVec3::new(1., 1., 0.),
            DVec3::new(0., 1., 0.),
            DVec3::new(0., 0., 1.),
            DVec3::new(1., 0., 1.),
            DVec3::new(1., 1., 1.),
            DVec3::new(0., 1., 1.),
        ];
        let faces = [
            [0, 3, 2, 1],
            [4, 5, 6, 7],
            [0, 1, 5, 4],
            [1, 2, 6, 5],
            [2, 3, 7, 6],
            [3, 0, 4, 7],
        ];
        let m = Mesh::new(p, &faces).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..m.corner_count())
                .map(|c| m.corner_vertex(c) as u32)
                .collect(),
            uvs: vec![DVec2::ZERO; 8],
        };
        (m, uv)
    }

    #[test]
    fn box_projection_gives_six_undistorted_unflipped_islands() {
        let (m, mut uv) = cube();
        project(&m, &mut uv, &[0], Projection::Box);
        let isl = Islands::compute(&m, &uv);
        assert_eq!(isl.count(), 6);
        assert!((0..m.edge_count()).all(|e| uv.edge_kind(&m, e) == EdgeKind::Seam));
        let r = analyze(&m, &uv, &isl, 1024);
        assert_eq!(r.flipped_islands, 0);
        assert!((r.max_angle_distortion - 1.0).abs() < 1e-9);
        assert_eq!(r.islands_outside_unit_tile, 0);
    }

    #[test]
    fn planar_and_best_keep_topology() {
        let (m, mut uv) = cube();
        project(&m, &mut uv, &[0], Projection::Planar(Axis::Z));
        assert_eq!(Islands::compute(&m, &uv).count(), 1);
        assert_eq!(uv.uv_at_corner(0), DVec2::ZERO);
        assert_eq!(uv.uvs[6], DVec2::ONE);

        // A single quad facing +X: best-plane projection is conformal and not flipped.
        let p = vec![
            DVec3::new(0., 0., 0.),
            DVec3::new(0., 1., 0.),
            DVec3::new(0., 1., 2.),
            DVec3::new(0., 0., 2.),
        ];
        let q = Mesh::new(p, &[[0, 1, 2, 3]]).unwrap();
        let mut quv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 3],
            uvs: vec![DVec2::ZERO; 4],
        };
        project(&q, &mut quv, &[0], Projection::Best);
        let r = analyze(&q, &quv, &Islands::compute(&q, &quv), 1024);
        assert_eq!(r.flipped_islands, 0);
        assert!((r.max_angle_distortion - 1.0).abs() < 1e-9);
    }

    /// Open tube around Y: `seg` segments around, `rows` quads tall, radius 1.
    fn tube(seg: usize, rows: usize, axis_offset: f64) -> (Mesh, UvLayer) {
        let mut p = Vec::new();
        for r in 0..=rows {
            for k in 0..seg {
                let t = std::f64::consts::TAU * k as f64 / seg as f64;
                p.push(DVec3::new(t.sin() + axis_offset, r as f64 * 0.3, t.cos()));
            }
        }
        let mut faces = Vec::new();
        for r in 0..rows {
            for k in 0..seg {
                let v = |r: usize, k: usize| (r * seg + k % seg) as u32;
                // Outward-facing: counter-clockwise seen from outside.
                faces.push([v(r, k), v(r, k + 1), v(r + 1, k + 1), v(r + 1, k)]);
            }
        }
        let m = Mesh::new(p, &faces).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..m.corner_count())
                .map(|c| m.corner_vertex(c) as u32)
                .collect(),
            uvs: vec![DVec2::ZERO; m.vertex_count()],
        };
        (m, uv)
    }

    #[test]
    fn cylindrical_unrolls_a_tube_with_one_seam() {
        let (m, mut uv) = tube(32, 4, 5.0);
        project(&m, &mut uv, &[0], Projection::Cylindrical(Axis::Y));
        let isl = Islands::compute(&m, &uv);
        assert_eq!(isl.count(), 1);
        let seams = (0..m.edge_count())
            .filter(|&e| uv.edge_kind(&m, e) == EdgeKind::Seam)
            .count();
        assert_eq!(seams, 4, "one vertical line of 4 edges");
        let r = analyze(&m, &uv, &isl, 1024);
        assert_eq!(r.flipped_islands, 0);
        assert!(r.max_angle_distortion < 1.01, "{}", r.max_angle_distortion);
    }

    #[test]
    fn cylindrical_does_not_cut_a_partial_tube() {
        let (m, uv0) = tube(32, 2, 0.0);
        // Keep the half of the tube around the ±π line (z < 0), as its own mesh.
        let faces: Vec<[u32; 4]> = (0..m.face_count())
            .filter(|&f| m.face_corners(f).all(|c| m.corner_position(c).z <= 1e-9))
            .map(|f| {
                let v: Vec<u32> = m
                    .face_corners(f)
                    .map(|c| m.corner_vertex(c) as u32)
                    .collect();
                [v[0], v[1], v[2], v[3]]
            })
            .collect();
        let half = Mesh::new(m.positions().to_vec(), &faces).unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..half.corner_count())
                .map(|c| half.corner_vertex(c) as u32)
                .collect(),
            uvs: uv0.uvs.clone(),
        };
        project(&half, &mut uv, &[0], Projection::Cylindrical(Axis::Y));
        let seams = (0..half.edge_count())
            .filter(|&e| uv.edge_kind(&half, e) == EdgeKind::Seam)
            .count();
        assert_eq!(seams, 0);
    }

    #[test]
    fn spherical_maps_a_uv_sphere_without_flips() {
        let (rings, seg) = (12, 24);
        let mut p = vec![DVec3::new(0.0, -1.0, 0.0)];
        for r in 1..rings {
            let phi = std::f64::consts::PI * r as f64 / rings as f64 - std::f64::consts::FRAC_PI_2;
            for k in 0..seg {
                let t = std::f64::consts::TAU * k as f64 / seg as f64;
                p.push(DVec3::new(
                    phi.cos() * t.sin(),
                    phi.sin(),
                    phi.cos() * t.cos(),
                ));
            }
        }
        p.push(DVec3::new(0.0, 1.0, 0.0));
        let top = (p.len() - 1) as u32;
        let v = |r: usize, k: usize| (1 + (r - 1) * seg + k % seg) as u32;
        let mut tris: Vec<[u32; 3]> = Vec::new();
        for k in 0..seg {
            tris.push([0, v(1, k + 1), v(1, k)]);
            tris.push([top, v(rings - 1, k), v(rings - 1, k + 1)]);
        }
        let mut quads: Vec<[u32; 4]> = Vec::new();
        for r in 1..rings - 1 {
            for k in 0..seg {
                quads.push([v(r, k), v(r, k + 1), v(r + 1, k + 1), v(r + 1, k)]);
            }
        }
        let faces: Vec<Vec<u32>> = tris
            .iter()
            .map(|t| t.to_vec())
            .chain(quads.iter().map(|q| q.to_vec()))
            .collect();
        let m = Mesh::new(p, &faces).unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..m.corner_count())
                .map(|c| m.corner_vertex(c) as u32)
                .collect(),
            uvs: vec![DVec2::ZERO; m.vertex_count()],
        };
        project(&m, &mut uv, &[0], Projection::Spherical(Axis::Y));
        let isl = Islands::compute(&m, &uv);
        assert_eq!(isl.count(), 1);
        let r = analyze(&m, &uv, &isl, 1024);
        assert_eq!(r.flipped_triangles, 0);
        // Latitude/longitude stretches u by 1/cos(latitude): conformal at the
        // equator, growing towards the poles (≈ 1.46 on average here).
        assert!(r.mean_angle_distortion < 1.5, "{}", r.mean_angle_distortion);
        assert_eq!(r.degenerate_triangles, 0);
    }
}
