//! CPU ambient-occlusion bake into a UV density map using a triangle BVH.

use glam::{DVec2, DVec3};
use uvcraft_core::{Cancelled, Mesh, Progress, UvLayer};

use crate::density_map::DensityMap;

#[derive(Debug, thiserror::Error)]
pub enum AoError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Cancelled(#[from] Cancelled),
}

#[derive(Clone, Copy)]
struct Tri {
    p: [DVec3; 3],
    face: usize,
    min: DVec3,
    max: DVec3,
    center: DVec3,
}

impl Tri {
    fn new(p: [DVec3; 3], face: usize) -> Self {
        let min = p[0].min(p[1]).min(p[2]);
        let max = p[0].max(p[1]).max(p[2]);
        Self {
            p,
            face,
            min,
            max,
            center: (min + max) * 0.5,
        }
    }
}

#[derive(Clone, Copy)]
struct Node {
    min: DVec3,
    max: DVec3,
    left: Option<usize>,
    right: Option<usize>,
    span: (usize, usize),
}

struct Bvh {
    triangles: Vec<Tri>,
    order: Vec<usize>,
    nodes: Vec<Node>,
}

impl Bvh {
    fn new(triangles: Vec<Tri>) -> Self {
        let order = (0..triangles.len()).collect();
        let mut tree = Self {
            triangles,
            order,
            nodes: Vec::new(),
        };
        if !tree.order.is_empty() {
            tree.build(0, tree.order.len());
        }
        tree
    }

    fn build(&mut self, start: usize, end: usize) -> usize {
        let mut min = DVec3::INFINITY;
        let mut max = DVec3::NEG_INFINITY;
        for &index in &self.order[start..end] {
            min = min.min(self.triangles[index].min);
            max = max.max(self.triangles[index].max);
        }
        let node = self.nodes.len();
        self.nodes.push(Node {
            min,
            max,
            left: None,
            right: None,
            span: (start, end),
        });
        if end - start > 8 {
            let span = max - min;
            let axis = if span.x >= span.y && span.x >= span.z {
                0
            } else if span.y >= span.z {
                1
            } else {
                2
            };
            self.order[start..end].sort_by(|&a, &b| {
                self.triangles[a].center[axis].total_cmp(&self.triangles[b].center[axis])
            });
            let mid = (start + end) / 2;
            let left = self.build(start, mid);
            let right = self.build(mid, end);
            self.nodes[node].left = Some(left);
            self.nodes[node].right = Some(right);
        }
        node
    }

    fn occluded(&self, origin: DVec3, dir: DVec3, max_distance: f64, exclude_face: usize) -> bool {
        if self.nodes.is_empty() {
            return false;
        }
        let mut stack = vec![0];
        while let Some(index) = stack.pop() {
            let node = self.nodes[index];
            if !ray_box(origin, dir, node.min, node.max, max_distance) {
                continue;
            }
            if let (Some(left), Some(right)) = (node.left, node.right) {
                stack.push(left);
                stack.push(right);
            } else {
                for &triangle in &self.order[node.span.0..node.span.1] {
                    let tri = self.triangles[triangle];
                    if tri.face != exclude_face && ray_triangle(origin, dir, tri.p, max_distance) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

fn ray_box(origin: DVec3, dir: DVec3, min: DVec3, max: DVec3, far: f64) -> bool {
    let mut near: f64 = 0.0;
    let mut end = far;
    for axis in 0..3 {
        if dir[axis].abs() < 1e-15 {
            if origin[axis] < min[axis] || origin[axis] > max[axis] {
                return false;
            }
        } else {
            let a = (min[axis] - origin[axis]) / dir[axis];
            let b = (max[axis] - origin[axis]) / dir[axis];
            near = near.max(a.min(b));
            end = end.min(a.max(b));
            if end < near {
                return false;
            }
        }
    }
    true
}

fn ray_triangle(origin: DVec3, dir: DVec3, tri: [DVec3; 3], far: f64) -> bool {
    let e1 = tri[1] - tri[0];
    let e2 = tri[2] - tri[0];
    let h = dir.cross(e2);
    let det = e1.dot(h);
    if det.abs() < 1e-12 {
        return false;
    }
    let inv = 1.0 / det;
    let s = origin - tri[0];
    let u = inv * s.dot(h);
    if !(-1e-10..=1.0 + 1e-10).contains(&u) {
        return false;
    }
    let q = s.cross(e1);
    let v = inv * dir.dot(q);
    if v < -1e-10 || u + v > 1.0 + 1e-10 {
        return false;
    }
    let t = inv * e2.dot(q);
    t > 1e-9 && t < far
}

fn barycentric(p: DVec2, tri: [DVec2; 3]) -> Option<[f64; 3]> {
    let a = tri[1] - tri[0];
    let b = tri[2] - tri[0];
    let det = a.perp_dot(b);
    if det.abs() < 1e-15 {
        return None;
    }
    let q = p - tri[0];
    let u = q.perp_dot(b) / det;
    let v = a.perp_dot(q) / det;
    if u < -1e-10 || v < -1e-10 || u + v > 1.0 + 1e-10 {
        return None;
    }
    Some([1.0 - u - v, u, v])
}

/// Bake hemisphere visibility into the active 0–1 UV tile. Uncovered pixels
/// are white; overlapping charts average their samples. Ray queries use a BVH.
pub fn bake_density_map(
    mesh: &Mesh,
    uv: &UvLayer,
    resolution: usize,
    rays: usize,
    max_distance: f64,
) -> Result<DensityMap, AoError> {
    bake_density_map_with_progress(
        mesh,
        uv,
        resolution,
        rays,
        max_distance,
        &Progress::default(),
    )
}

pub fn bake_density_map_with_progress(
    mesh: &Mesh,
    uv: &UvLayer,
    resolution: usize,
    rays: usize,
    max_distance: f64,
    progress: &Progress,
) -> Result<DensityMap, AoError> {
    if !(1..=1024).contains(&resolution)
        || !(1..=128).contains(&rays)
        || !max_distance.is_finite()
        || max_distance <= 0.0
    {
        return Err(AoError::Invalid(
            "AO requires resolution 1–1024, rays 1–128 and positive finite distance".into(),
        ));
    }
    let triangles: Vec<_> = (0..mesh.face_count())
        .flat_map(|face| {
            mesh.fan_triangles(face)
                .map(move |corners| Tri::new(corners.map(|c| mesh.corner_position(c)), face))
        })
        .collect();
    let bvh = Bvh::new(triangles);
    let mut sum = vec![0.0; resolution * resolution];
    let mut count = vec![0u32; resolution * resolution];
    let epsilon = max_distance * 1e-6;
    for face in 0..mesh.face_count() {
        progress.report("bake_ao", face as u64, mesh.face_count() as u64)?;
        for corners in mesh.fan_triangles(face) {
            let positions = corners.map(|c| mesh.corner_position(c));
            let coords = corners.map(|c| uv.uv_at_corner(c));
            if coords.iter().any(|p| !p.is_finite()) {
                return Err(AoError::Invalid("non-finite UV coordinates".into()));
            }
            let normal = (positions[1] - positions[0])
                .cross(positions[2] - positions[0])
                .normalize_or_zero();
            if normal.length_squared() == 0.0 {
                continue;
            }
            let tangent = if normal.x.abs() < 0.9 {
                normal.cross(DVec3::X).normalize()
            } else {
                normal.cross(DVec3::Y).normalize()
            };
            let bitangent = normal.cross(tangent);
            let lo = coords[0]
                .min(coords[1])
                .min(coords[2])
                .clamp(DVec2::ZERO, DVec2::ONE);
            let hi = coords[0]
                .max(coords[1])
                .max(coords[2])
                .clamp(DVec2::ZERO, DVec2::ONE);
            let x0 = (lo.x * resolution as f64).floor() as usize;
            let x1 = ((hi.x * resolution as f64).ceil() as usize).min(resolution);
            let y0 = (lo.y * resolution as f64).floor() as usize;
            let y1 = ((hi.y * resolution as f64).ceil() as usize).min(resolution);
            for y in y0..y1 {
                progress.check()?;
                for x in x0..x1 {
                    let uv_point = DVec2::new(
                        (x as f64 + 0.5) / resolution as f64,
                        1.0 - (y as f64 + 0.5) / resolution as f64,
                    );
                    let Some(w) = barycentric(uv_point, coords) else {
                        continue;
                    };
                    let point = positions[0] * w[0]
                        + positions[1] * w[1]
                        + positions[2] * w[2]
                        + normal * epsilon;
                    let mut blocked = 0;
                    for ray in 0..rays {
                        let z = (ray as f64 + 0.5) / rays as f64;
                        let phi = std::f64::consts::TAU
                            * ((ray as f64 * 0.618_033_988_749_894_9).fract());
                        let horizontal = (1.0 - z * z).sqrt();
                        let direction = normal * z
                            + tangent * (horizontal * phi.cos())
                            + bitangent * (horizontal * phi.sin());
                        if bvh.occluded(point, direction, max_distance, face) {
                            blocked += 1;
                        }
                    }
                    let index = y * resolution + x;
                    sum[index] += 1.0 - blocked as f64 / rays as f64;
                    count[index] += 1;
                }
            }
        }
    }
    let values = sum
        .into_iter()
        .zip(count)
        .map(
            |(sum, count)| {
                if count == 0 { 1.0 } else { sum / count as f64 }
            },
        )
        .collect();
    progress.report(
        "bake_ao",
        mesh.face_count() as u64,
        mesh.face_count() as u64,
    )?;
    Ok(DensityMap {
        width: resolution,
        height: resolution,
        values,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_plane_is_white_and_parallel_occluder_darkens_it() {
        let lower = [DVec3::ZERO, DVec3::X, DVec3::ONE, DVec3::Y];
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 3],
            uvs: vec![DVec2::ZERO, DVec2::X, DVec2::ONE, DVec2::Y],
        };
        let plane = Mesh::new(lower.to_vec(), &[[0, 1, 2, 3]]).unwrap();
        let white = bake_density_map(&plane, &uv, 4, 16, 1.0).unwrap();
        assert!(white.values.iter().all(|&v| (v - 1.0).abs() < 1e-12));
        let mut points = lower.to_vec();
        points.extend(lower.map(|p| p + DVec3::new(0.0, 0.0, 0.1)));
        let enclosed = Mesh::new(points, &[[0, 1, 2, 3], [4, 5, 6, 7]]).unwrap();
        uv.corner_uv.extend([0, 1, 2, 3]);
        let shadow = bake_density_map(&enclosed, &uv, 4, 16, 1.0).unwrap();
        assert!(shadow.values.iter().any(|&v| v < 0.9));
    }
}
