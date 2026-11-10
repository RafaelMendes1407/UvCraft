//! Island layout: orientation, texel density and packing.
//!
//! Packing (baseline): islands are optionally rotated to their minimum-area
//! bounding rectangle and laid flat, then placed on shelves (sorted by height)
//! inside the 0–1 tile. One uniform scale for all islands keeps relative texel
//! density; it is the largest scale that fits, found by bisection. Padding and
//! margin are in pixels at the given resolution.

use glam::DVec2;
use serde::{Deserialize, Serialize};
use uvcraft_core::{Islands, Mesh, UvLayer};

use crate::analysis::texel_density;

fn default_padding() -> f64 {
    4.0
}
fn default_resolution() -> u32 {
    2048
}
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackOptions {
    #[serde(default = "default_padding")]
    pub padding_px: f64,
    #[serde(default = "default_padding")]
    pub margin_px: f64,
    #[serde(default = "default_resolution")]
    pub resolution: u32,
    /// Rotate each island to its minimum-area bounding rectangle.
    #[serde(default = "yes")]
    pub orient: bool,
    /// Lay each island flat (width ≥ height) by rotating 90° when needed.
    #[serde(default = "yes")]
    pub rotate_90: bool,
}

impl Default for PackOptions {
    fn default() -> Self {
        PackOptions {
            padding_px: default_padding(),
            margin_px: default_padding(),
            resolution: default_resolution(),
            orient: true,
            rotate_90: true,
        }
    }
}

/// Convex hull (Andrew's monotone chain), counter-clockwise.
fn convex_hull(mut pts: Vec<DVec2>) -> Vec<DVec2> {
    pts.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    let mut hull: Vec<DVec2> = Vec::with_capacity(2 * pts.len());
    for pass in 0..2 {
        let start = hull.len();
        let iter: Box<dyn Iterator<Item = &DVec2>> = if pass == 0 {
            Box::new(pts.iter())
        } else {
            Box::new(pts.iter().rev())
        };
        for &p in iter {
            while hull.len() >= start + 2
                && (hull[hull.len() - 1] - hull[hull.len() - 2]).perp_dot(p - hull[hull.len() - 2])
                    <= 0.0
            {
                hull.pop();
            }
            hull.push(p);
        }
        hull.pop();
    }
    hull
}

/// Rotation angle that gives the minimum-area bounding rectangle (rotating
/// calipers: the optimum has a side collinear with a hull edge).
pub fn min_area_rect_angle(points: &[DVec2]) -> f64 {
    let hull = convex_hull(points.to_vec());
    if hull.len() < 3 {
        return 0.0;
    }
    let mut best = (f64::INFINITY, 0.0);
    for i in 0..hull.len() {
        let d = hull[(i + 1) % hull.len()] - hull[i];
        if d.length_squared() == 0.0 {
            continue;
        }
        let angle = -d.y.atan2(d.x);
        let rot = DVec2::from_angle(angle);
        let (lo, hi) = bbox(hull.iter().map(|&p| rot.rotate(p)));
        let area = (hi - lo).x * (hi - lo).y;
        if area < best.0 - 1e-12 {
            best = (area, angle);
        }
    }
    best.1
}

fn bbox(points: impl Iterator<Item = DVec2>) -> (DVec2, DVec2) {
    points.fold((DVec2::INFINITY, DVec2::NEG_INFINITY), |(lo, hi), p| {
        (lo.min(p), hi.max(p))
    })
}

/// Shelf placement of sizes (already scaled) in a square of side `w`.
/// Returns the positions, or `None` if they do not fit.
fn shelf(sizes: &[(usize, DVec2)], w: f64, pad: f64) -> Option<Vec<(usize, DVec2)>> {
    let (mut x, mut y, mut row) = (0.0, 0.0, 0.0f64);
    let mut out = Vec::with_capacity(sizes.len());
    for &(i, s) in sizes {
        if s.x > w + 1e-12 {
            return None;
        }
        if x > 0.0 && x + s.x > w + 1e-12 {
            y += row + pad;
            x = 0.0;
            row = 0.0;
        }
        out.push((i, DVec2::new(x, y)));
        x += s.x + pad;
        row = row.max(s.y);
    }
    (y + row <= w + 1e-12).then_some(out)
}

/// Packs the target islands into the 0–1 tile.
pub fn pack(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    opts: &PackOptions,
) {
    let res = opts.resolution.max(1) as f64;
    let pad = opts.padding_px / res;
    let margin = opts.margin_px / res;
    let avail = 1.0 - 2.0 * margin;
    if targets.is_empty() || avail <= 0.0 {
        return;
    }

    // Normalize each island: rotate, lay flat, move to the origin.
    let mut items: Vec<(Vec<u32>, DVec2)> = Vec::with_capacity(targets.len());
    for &i in targets {
        let ids = islands.uv_indices(i, mesh, uv);
        let pts: Vec<DVec2> = ids.iter().map(|&k| uv.uvs[k as usize]).collect();
        let mut rot = DVec2::X;
        if opts.orient {
            rot = DVec2::from_angle(min_area_rect_angle(&pts));
        }
        let (lo, hi) = bbox(pts.iter().map(|&p| rot.rotate(p)));
        if opts.rotate_90 && (hi - lo).y > (hi - lo).x {
            rot = DVec2::Y.rotate(rot); // extra +90°
        }
        let (lo, hi) = bbox(pts.iter().map(|&p| rot.rotate(p)));
        for &k in &ids {
            uv.uvs[k as usize] = rot.rotate(uv.uvs[k as usize]) - lo;
        }
        items.push((ids, hi - lo));
    }

    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| {
        items[b]
            .1
            .y
            .total_cmp(&items[a].1.y)
            .then(items[b].1.x.total_cmp(&items[a].1.x))
    });
    let sized =
        |s: f64| -> Vec<(usize, DVec2)> { order.iter().map(|&i| (i, items[i].1 * s)).collect() };

    let max_dim = items
        .iter()
        .map(|(_, s)| s.max_element())
        .fold(0.0, f64::max);
    let total: f64 = items.iter().map(|(_, s)| s.x * s.y).sum();
    if max_dim <= 0.0 {
        return;
    }
    let mut hi = (avail / max_dim).min(if total > 0.0 {
        avail / total.sqrt()
    } else {
        f64::INFINITY
    });
    let mut lo = 0.0;
    if shelf(&sized(hi), avail, pad).is_some() {
        lo = hi;
    } else {
        for _ in 0..50 {
            let mid = 0.5 * (lo + hi);
            if shelf(&sized(mid), avail, pad).is_some() {
                lo = mid;
            } else {
                hi = mid;
            }
        }
    }
    // With padding alone too large, fall back to scale 0 positions (degenerate but safe).
    let placement = shelf(&sized(lo), avail, pad).unwrap_or_default();
    for (i, at) in placement {
        for &k in &items[i].0 {
            let p = &mut uv.uvs[k as usize];
            *p = DVec2::splat(margin) + at + *p * lo;
        }
    }
}

/// (unsigned UV area, 3D area) of an island.
pub fn island_areas(mesh: &Mesh, uv: &UvLayer, islands: &Islands, i: usize) -> (f64, f64) {
    let (mut a_uv, mut a_3d) = (0.0, 0.0);
    for &f in islands.faces(i) {
        for t in mesh.fan_triangles(f as usize) {
            let [a, b, c] = t.map(|c| uv.uv_at_corner(c));
            a_uv += 0.5 * (b - a).perp_dot(c - a);
            let [p, q, r] = t.map(|c| mesh.corner_position(c));
            a_3d += 0.5 * (q - p).cross(r - p).length();
        }
    }
    (a_uv.abs(), a_3d)
}

/// Scales each target island about its center to the given texel density
/// (pixels per 3D unit at `resolution`). Islands with no UV area are skipped.
pub fn set_texel_density(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    density: f64,
    resolution: u32,
) {
    for &i in targets {
        let (a_uv, a_3d) = island_areas(mesh, uv, islands, i);
        let current = texel_density(a_uv, a_3d, resolution);
        if current <= 0.0 {
            continue;
        }
        let s = density / current;
        let ids = islands.uv_indices(i, mesh, uv);
        let (lo, hi) = bbox(ids.iter().map(|&k| uv.uvs[k as usize]));
        let c = (lo + hi) / 2.0;
        for k in ids {
            let p = &mut uv.uvs[k as usize];
            *p = c + (*p - c) * s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::analyze;
    use crate::projection::{Projection, project};
    use glam::DVec3;

    #[test]
    fn min_rect_of_rotated_rectangle() {
        let rot = DVec2::from_angle(0.3);
        let pts: Vec<DVec2> = [
            DVec2::ZERO,
            DVec2::new(4.0, 0.0),
            DVec2::new(4.0, 1.0),
            DVec2::new(0.0, 1.0),
        ]
        .iter()
        .map(|&p| rot.rotate(p))
        .collect();
        let back = DVec2::from_angle(min_area_rect_angle(&pts));
        let (lo, hi) = bbox(pts.iter().map(|&p| back.rotate(p)));
        assert!(((hi - lo).x * (hi - lo).y - 4.0).abs() < 1e-9);
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
    fn packs_six_faces_without_overlap_inside_margins() {
        let (m, mut uv) = cube();
        project(&m, &mut uv, &[0], Projection::Box);
        let isl = Islands::compute(&m, &uv);
        let opts = PackOptions {
            resolution: 256,
            ..Default::default()
        };
        pack(&m, &mut uv, &isl, &(0..6).collect::<Vec<_>>(), &opts);

        let boxes: Vec<(DVec2, DVec2)> = (0..6)
            .map(|i| {
                bbox(
                    isl.uv_indices(i, &m, &uv)
                        .iter()
                        .map(|&k| uv.uvs[k as usize]),
                )
            })
            .collect();
        let margin = 4.0 / 256.0;
        let pad = 4.0 / 256.0;
        for (i, &(lo, hi)) in boxes.iter().enumerate() {
            assert!(
                lo.min_element() >= margin - 1e-9 && hi.max_element() <= 1.0 - margin + 1e-9,
                "{lo} {hi}"
            );
            for &(lo2, hi2) in &boxes[i + 1..] {
                let gap = (lo2 - hi).max(lo - hi2).max_element();
                assert!(gap >= pad - 1e-9, "islands closer than padding: {gap}");
            }
        }
        // 6 unit squares on 3 shelves of 2 (or 2 of 3): side ≈ (1 - 2m - 2p) / 3.
        let side = (boxes[0].1 - boxes[0].0).x;
        assert!(side > 0.3, "{side}");
        let r = analyze(&m, &uv, &isl, 256);
        assert!((r.max_area_distortion - 1.0).abs() < 1e-9);
        assert_eq!(r.flipped_islands, 0);
    }

    #[test]
    fn texel_density_is_set() {
        let (m, mut uv) = cube();
        project(&m, &mut uv, &[0], Projection::Box);
        let isl = Islands::compute(&m, &uv);
        set_texel_density(&m, &mut uv, &isl, &[0, 1], 100.0, 1024);
        let r = analyze(&m, &uv, &isl, 1024);
        assert!((r.islands[0].texel_density - 100.0).abs() < 1e-9);
        assert!((r.islands[1].texel_density - 100.0).abs() < 1e-9);
    }
}
