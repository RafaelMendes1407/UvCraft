//! UV quality checks: overlaps, padding violations, flipped faces and
//! out-of-tile islands.
//!
//! Overlaps are tested on fan-triangulated faces. A uniform grid gives the
//! candidate pairs; each pair is then tested exactly with the separating axis
//! theorem, with a small tolerance so that triangles merely touching along an
//! edge or at a point do not count. Triangles sharing a UV vertex are skipped:
//! they are neighbours, and a fold between them shows up as a flipped triangle.
//!
//! Padding is the distance between the boundary segments of two islands,
//! found with the same grid over segments inflated by the required gap.
//!
//! All outputs are sorted, and candidate pairs are deduplicated by
//! construction, so results do not depend on thread scheduling.

use glam::DVec2;
use rayon::prelude::*;
use serde::Serialize;
use uvcraft_core::{Islands, Mesh, UvLayer};

/// Area below this fraction of the squared longest edge counts as degenerate.
const DEGENERATE_EPS: f64 = 1e-12;
/// Overlap depth, relative to the triangles' size, below which two triangles
/// only touch.
const TOUCH_EPS: f64 = 1e-9;
/// Upper bound on the number of grid cells.
const MAX_CELLS: f64 = 4_000_000.0;

/// One fan triangle of a face, in UV space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UvTriangle {
    pub face: u32,
    pub island: u32,
    /// UV vertex indices.
    pub uv: [u32; 3],
    pub p: [DVec2; 3],
}

/// Fan triangles of every face, skipping triangles that are degenerate or
/// not finite in UV. Ordered by face, then fan order.
pub fn uv_triangles(mesh: &Mesh, uv: &UvLayer, islands: &Islands) -> Vec<UvTriangle> {
    let mut out = Vec::with_capacity(mesh.corner_count());
    for f in 0..mesh.face_count() {
        let island = islands.island_of_face(f) as u32;
        for tri in mesh.fan_triangles(f) {
            let p = tri.map(|c| uv.uv_at_corner(c));
            if p.iter().any(|q| !q.is_finite()) || is_degenerate(p) {
                continue;
            }
            out.push(UvTriangle {
                face: f as u32,
                island,
                uv: tri.map(|c| uv.corner_uv[c]),
                p,
            });
        }
    }
    out
}

fn is_degenerate(p: [DVec2; 3]) -> bool {
    let (d1, d2) = (p[1] - p[0], p[2] - p[0]);
    let longest = d1
        .length_squared()
        .max(d2.length_squared())
        .max((p[2] - p[1]).length_squared());
    longest == 0.0 || (0.5 * d1.perp_dot(d2)).abs() <= DEGENERATE_EPS * longest
}

/// True if the interiors of two triangles intersect with positive area
/// (separating axis theorem, edge normals of both triangles as axes).
pub fn triangles_overlap(a: [DVec2; 3], b: [DVec2; 3]) -> bool {
    let size = [a, b]
        .iter()
        .flat_map(|t| (0..3).map(move |i| t[i].distance_squared(t[(i + 1) % 3])))
        .fold(0.0, f64::max)
        .sqrt();
    let tol = TOUCH_EPS * size;
    for t in [a, b] {
        for i in 0..3 {
            let edge = t[(i + 1) % 3] - t[i];
            let len = edge.length();
            if len == 0.0 {
                continue;
            }
            let axis = edge.perp() / len;
            let (a0, a1) = project(a, axis);
            let (b0, b1) = project(b, axis);
            if a1.min(b1) - a0.max(b0) <= tol {
                return false;
            }
        }
    }
    true
}

fn project(t: [DVec2; 3], axis: DVec2) -> (f64, f64) {
    let d = t.map(|p| p.dot(axis));
    (d[0].min(d[1]).min(d[2]), d[0].max(d[1]).max(d[2]))
}

/// Squared distance between segments `[p0, p1]` and `[q0, q1]` (0 if they cross).
pub fn segment_distance(p0: DVec2, p1: DVec2, q0: DVec2, q1: DVec2) -> f64 {
    let (d1, d2) = (p1 - p0, q1 - q0);
    let denom = d1.perp_dot(d2);
    if denom != 0.0 {
        let s = (q0 - p0).perp_dot(d2) / denom;
        let t = (q0 - p0).perp_dot(d1) / denom;
        if (0.0..=1.0).contains(&s) && (0.0..=1.0).contains(&t) {
            return 0.0;
        }
    }
    point_segment(p0, q0, q1)
        .min(point_segment(p1, q0, q1))
        .min(point_segment(q0, p0, p1))
        .min(point_segment(q1, p0, p1))
}

fn point_segment(p: DVec2, a: DVec2, b: DVec2) -> f64 {
    let ab = b - a;
    let len2 = ab.length_squared();
    let t = if len2 > 0.0 {
        ((p - a).dot(ab) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    p.distance_squared(a + ab * t)
}

/// Uniform grid broad phase over axis-aligned boxes.
struct Grid {
    origin: DVec2,
    cell: f64,
    dims: [usize; 2],
    /// `(cell, item)` sorted by cell, then item.
    entries: Vec<(u32, u32)>,
}

impl Grid {
    fn new(boxes: &[(DVec2, DVec2)]) -> Self {
        let mut lo = DVec2::splat(f64::INFINITY);
        let mut hi = DVec2::splat(f64::NEG_INFINITY);
        let mut mean = 0.0;
        for &(a, b) in boxes {
            lo = lo.min(a);
            hi = hi.max(b);
            mean += (b - a).max_element();
        }
        let n = boxes.len().max(1) as f64;
        let extent = (hi - lo).max(DVec2::splat(f64::MIN_POSITIVE));
        // Cells about the size of an average item, capped in number.
        let floor = (extent.x * extent.y / MAX_CELLS.min(4.0 * n)).sqrt();
        let mut cell = (mean / n).max(floor);
        if !(cell.is_finite() && cell > 0.0) {
            cell = extent.max_element().max(1.0);
        }
        let dims = [
            ((extent.x / cell).ceil() as usize).clamp(1, 1 << 12),
            ((extent.y / cell).ceil() as usize).clamp(1, 1 << 12),
        ];
        let mut grid = Grid {
            origin: if boxes.is_empty() { DVec2::ZERO } else { lo },
            cell,
            dims,
            entries: Vec::new(),
        };
        let mut entries = Vec::new();
        for (i, &(a, b)) in boxes.iter().enumerate() {
            let (c0, c1) = (grid.cell_of(a), grid.cell_of(b));
            for y in c0[1]..=c1[1] {
                for x in c0[0]..=c1[0] {
                    entries.push(((y * dims[0] + x) as u32, i as u32));
                }
            }
        }
        entries.par_sort_unstable();
        grid.entries = entries;
        grid
    }

    fn cell_of(&self, p: DVec2) -> [usize; 2] {
        let q = (p - self.origin) / self.cell;
        [
            (q.x.floor().max(0.0) as usize).min(self.dims[0] - 1),
            (q.y.floor().max(0.0) as usize).min(self.dims[1] - 1),
        ]
    }

    /// Pairs `(i, j)`, `i < j`, whose boxes intersect and for which `keep`
    /// holds. Each pair is reported once, sorted.
    fn pairs(
        &self,
        boxes: &[(DVec2, DVec2)],
        keep: impl Fn(usize, usize) -> bool + Sync,
    ) -> Vec<(u32, u32)> {
        let mut starts = Vec::new();
        for (k, e) in self.entries.iter().enumerate() {
            if k == 0 || self.entries[k - 1].0 != e.0 {
                starts.push(k);
            }
        }
        starts.push(self.entries.len());
        let mut out: Vec<(u32, u32)> = starts
            .par_windows(2)
            .flat_map_iter(|w| {
                let run = &self.entries[w[0]..w[1]];
                let cell = run[0].0 as usize;
                let here = [cell % self.dims[0], cell / self.dims[0]];
                let mut found = Vec::new();
                for (k, &(_, i)) in run.iter().enumerate() {
                    let (ai, bi) = boxes[i as usize];
                    for &(_, j) in &run[k + 1..] {
                        let (aj, bj) = boxes[j as usize];
                        if ai.x > bj.x || aj.x > bi.x || ai.y > bj.y || aj.y > bi.y {
                            continue;
                        }
                        // Report the pair only in the cell holding the
                        // corner shared by both boxes, so exactly once.
                        if self.cell_of(ai.max(aj)) != here {
                            continue;
                        }
                        if keep(i as usize, j as usize) {
                            found.push((i, j));
                        }
                    }
                }
                found
            })
            .collect();
        out.par_sort_unstable();
        out
    }
}

fn triangle_box(t: &UvTriangle) -> (DVec2, DVec2) {
    (
        t.p[0].min(t.p[1]).min(t.p[2]),
        t.p[0].max(t.p[1]).max(t.p[2]),
    )
}

/// Two faces whose UV triangles overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct FacePair {
    /// `faces[0] < faces[1]`.
    pub faces: [usize; 2],
    /// Islands of `faces[0]` and `faces[1]`.
    pub islands: [usize; 2],
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Overlaps {
    /// Overlapping face pairs, sorted.
    pub face_pairs: Vec<FacePair>,
    /// Faces involved in at least one overlap, sorted.
    pub faces: Vec<usize>,
    /// Islands overlapping themselves, sorted.
    pub self_overlapping_islands: Vec<usize>,
    /// Pairs of distinct islands overlapping each other, sorted, `[a, b]` with `a < b`.
    pub island_pairs: Vec<[usize; 2]>,
}

impl Overlaps {
    /// Islands that overlap themselves or another island, sorted.
    pub fn islands(&self) -> Vec<usize> {
        let mut out = self.self_overlapping_islands.clone();
        out.extend(self.island_pairs.iter().flatten());
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Finds overlapping triangles, within and between islands.
pub fn find_overlaps(mesh: &Mesh, uv: &UvLayer, islands: &Islands) -> Overlaps {
    let tris = uv_triangles(mesh, uv, islands);
    let boxes: Vec<_> = tris.iter().map(triangle_box).collect();
    let grid = Grid::new(&boxes);
    let pairs = grid.pairs(&boxes, |i, j| {
        let (a, b) = (&tris[i], &tris[j]);
        a.face != b.face && !a.uv.iter().any(|x| b.uv.contains(x)) && triangles_overlap(a.p, b.p)
    });

    let mut face_pairs: Vec<FacePair> = pairs
        .iter()
        .map(|&(i, j)| {
            let (a, b) = (&tris[i as usize], &tris[j as usize]);
            let (a, b) = if a.face < b.face { (a, b) } else { (b, a) };
            FacePair {
                faces: [a.face as usize, b.face as usize],
                islands: [a.island as usize, b.island as usize],
            }
        })
        .collect();
    face_pairs.sort_unstable();
    face_pairs.dedup();

    let mut faces: Vec<usize> = face_pairs.iter().flat_map(|p| p.faces).collect();
    faces.sort_unstable();
    faces.dedup();
    let mut self_overlapping_islands = Vec::new();
    let mut island_pairs = Vec::new();
    for p in &face_pairs {
        let [a, b] = p.islands;
        if a == b {
            self_overlapping_islands.push(a);
        } else {
            island_pairs.push([a.min(b), a.max(b)]);
        }
    }
    self_overlapping_islands.sort_unstable();
    self_overlapping_islands.dedup();
    island_pairs.sort_unstable();
    island_pairs.dedup();
    Overlaps {
        face_pairs,
        faces,
        self_overlapping_islands,
        island_pairs,
    }
}

/// Boundary segments of every island as `(island, uv a, uv b)`: UV edges used
/// by exactly one face of the island. Sorted.
pub fn boundary_segments(mesh: &Mesh, uv: &UvLayer, islands: &Islands) -> Vec<(u32, u32, u32)> {
    let mut edges: Vec<(u32, u32, u32)> = (0..mesh.face_count())
        .flat_map(|f| {
            let island = islands.island_of_face(f) as u32;
            mesh.face_corners(f).map(move |c| {
                let (a, b) = (uv.corner_uv[c], uv.corner_uv[mesh.next(c)]);
                (island, a.min(b), a.max(b))
            })
        })
        .filter(|&(_, a, b)| a != b)
        .collect();
    edges.par_sort_unstable();
    let mut out = Vec::new();
    let mut k = 0;
    while k < edges.len() {
        let mut n = 1;
        while k + n < edges.len() && edges[k + n] == edges[k] {
            n += 1;
        }
        if n == 1 {
            out.push(edges[k]);
        }
        k += n;
    }
    out
}

/// Two islands closer than the required padding.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PaddingViolation {
    /// `[a, b]` with `a < b`.
    pub islands: [usize; 2],
    /// Smallest boundary-to-boundary distance, in pixels (0 when they overlap).
    pub distance_px: f64,
}

/// Island pairs whose boundaries come closer than `padding_px` pixels at
/// `resolution`. Islands that overlap without their boundaries getting close
/// (one inside the other) are not found here; [`check`] adds them from the
/// overlap test. Sorted by island pair.
pub fn padding_violations(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    padding_px: f64,
    resolution: u32,
) -> Vec<PaddingViolation> {
    // Written so that NaN also skips the check.
    if padding_px.is_nan() || padding_px <= 0.0 || resolution == 0 {
        return Vec::new();
    }
    let px = resolution as f64;
    // Islands exactly `padding_px` apart are fine.
    let gap = padding_px / px * (1.0 - 1e-9);
    let segs: Vec<(u32, DVec2, DVec2)> = boundary_segments(mesh, uv, islands)
        .into_iter()
        .map(|(i, a, b)| (i, uv.uvs[a as usize], uv.uvs[b as usize]))
        .filter(|(_, a, b)| a.is_finite() && b.is_finite())
        .collect();
    let half = DVec2::splat(gap / 2.0);
    let boxes: Vec<_> = segs
        .iter()
        .map(|&(_, a, b)| (a.min(b) - half, a.max(b) + half))
        .collect();
    let grid = Grid::new(&boxes);
    let gap2 = gap * gap;
    let close = grid.pairs(&boxes, |i, j| {
        let (a, b) = (&segs[i], &segs[j]);
        a.0 != b.0 && segment_distance(a.1, a.2, b.1, b.2) < gap2
    });

    let mut found: Vec<([usize; 2], f64)> = close
        .par_iter()
        .map(|&(i, j)| {
            let (a, b) = (&segs[i as usize], &segs[j as usize]);
            let pair = [a.0.min(b.0) as usize, a.0.max(b.0) as usize];
            (pair, segment_distance(a.1, a.2, b.1, b.2).sqrt())
        })
        .collect();
    found.sort_unstable_by(|x, y| x.0.cmp(&y.0).then(x.1.total_cmp(&y.1)));
    found.dedup_by_key(|x| x.0);
    found
        .into_iter()
        .map(|(islands, d)| PaddingViolation {
            islands,
            distance_px: d * px,
        })
        .collect()
}

/// Faces with a fan triangle mirrored relative to its island's overall
/// orientation, sorted. Degenerate triangles are ignored.
pub fn flipped_faces(mesh: &Mesh, uv: &UvLayer, islands: &Islands) -> Vec<usize> {
    let area = |tri: [usize; 3]| {
        let p = tri.map(|c| uv.uv_at_corner(c));
        if is_degenerate(p) {
            0.0
        } else {
            0.5 * (p[1] - p[0]).perp_dot(p[2] - p[0])
        }
    };
    let mut out: Vec<usize> = (0..islands.count())
        .into_par_iter()
        .flat_map_iter(|i| {
            let faces = islands.faces(i);
            let signed: f64 = faces
                .iter()
                .flat_map(|&f| mesh.fan_triangles(f as usize))
                .map(area)
                .sum();
            let flipped = signed < 0.0;
            faces
                .iter()
                .map(|&f| f as usize)
                .filter(move |&f| {
                    mesh.fan_triangles(f).any(|t| {
                        let a = area(t);
                        a != 0.0 && (a < 0.0) != flipped
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect();
    out.sort_unstable();
    out
}

/// Islands with any UV outside the 0–1 tile (with a 1e-9 tolerance), sorted.
pub fn out_of_tile_islands(mesh: &Mesh, uv: &UvLayer, islands: &Islands) -> Vec<usize> {
    let eps = 1e-9;
    (0..islands.count())
        .filter(|&i| {
            islands.faces(i).iter().any(|&f| {
                mesh.face_corners(f as usize).any(|c| {
                    let p = uv.uv_at_corner(c);
                    p.min_element() < -eps || p.max_element() > 1.0 + eps
                })
            })
        })
        .collect()
}

/// Overlap and padding checks together.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Checks {
    pub overlaps: Overlaps,
    /// Boundary-distance violations plus overlapping island pairs (distance 0),
    /// sorted by island pair. Empty when `padding_px` is 0.
    pub padding_violations: Vec<PaddingViolation>,
}

pub fn check(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    padding_px: f64,
    resolution: u32,
) -> Checks {
    let (overlaps, mut padding) = rayon::join(
        || find_overlaps(mesh, uv, islands),
        || padding_violations(mesh, uv, islands, padding_px, resolution),
    );
    if padding_px > 0.0 {
        for &pair in &overlaps.island_pairs {
            match padding.binary_search_by(|v| v.islands.cmp(&pair)) {
                Ok(k) => padding[k].distance_px = 0.0,
                Err(k) => padding.insert(
                    k,
                    PaddingViolation {
                        islands: pair,
                        distance_px: 0.0,
                    },
                ),
            }
        }
    }
    Checks {
        overlaps,
        padding_violations: padding,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    fn tri(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> [DVec2; 3] {
        [a, b, c].map(DVec2::from)
    }

    #[test]
    fn sat_cases() {
        let a = tri([0.0, 0.0], [1.0, 0.0], [0.0, 1.0]);
        // Identical (stacked) triangles overlap.
        assert!(triangles_overlap(a, a));
        // Mirrored copy too.
        assert!(triangles_overlap(a, [a[0], a[2], a[1]]));
        // Sharing an edge from the other side: touching only.
        assert!(!triangles_overlap(
            a,
            tri([1.0, 0.0], [1.0, 1.0], [0.0, 1.0])
        ));
        // Touching at a point.
        assert!(!triangles_overlap(
            a,
            tri([1.0, 0.0], [2.0, 0.0], [2.0, 1.0])
        ));
        // Crossing edges without vertices inside (star of David).
        let up = tri([0.0, 0.0], [2.0, 0.0], [1.0, 2.0]);
        let down = tri([0.0, 1.5], [2.0, 1.5], [1.0, -0.5]);
        assert!(triangles_overlap(up, down));
        // Contained.
        assert!(triangles_overlap(
            up,
            tri([0.9, 0.5], [1.1, 0.5], [1.0, 0.7])
        ));
        // Disjoint but bounding boxes overlap.
        assert!(!triangles_overlap(
            a,
            tri([1.0, 1.0], [0.6, 1.0], [1.0, 0.6])
        ));
    }

    #[test]
    fn segment_distances() {
        let d = |p: [f64; 4], q: [f64; 4]| {
            segment_distance(
                DVec2::new(p[0], p[1]),
                DVec2::new(p[2], p[3]),
                DVec2::new(q[0], q[1]),
                DVec2::new(q[2], q[3]),
            )
            .sqrt()
        };
        assert_eq!(d([0.0, 0.0, 1.0, 1.0], [0.0, 1.0, 1.0, 0.0]), 0.0);
        assert!((d([0.0, 0.0, 1.0, 0.0], [0.0, 0.5, 1.0, 0.5]) - 0.5).abs() < 1e-12);
        assert!((d([0.0, 0.0, 1.0, 0.0], [2.0, 0.0, 3.0, 1.0]) - 1.0).abs() < 1e-12);
        assert!((d([0.0, 0.0, 1.0, 0.0], [0.5, 0.25, 0.5, 2.0]) - 0.25).abs() < 1e-12);
    }

    /// One quad per entry of `squares` (min corner, size), each its own island.
    fn quads(squares: &[([f64; 2], f64)]) -> (Mesh, UvLayer, Islands) {
        let mut pos = Vec::new();
        let mut faces = Vec::new();
        let mut uvs = Vec::new();
        for (k, &(o, s)) in squares.iter().enumerate() {
            let base = pos.len() as u32;
            let z = k as f64;
            pos.extend([
                DVec3::new(0.0, 0.0, z),
                DVec3::new(1.0, 0.0, z),
                DVec3::new(1.0, 1.0, z),
                DVec3::new(0.0, 1.0, z),
            ]);
            faces.push([base, base + 1, base + 2, base + 3]);
            let o = DVec2::from(o);
            uvs.extend([
                o,
                o + DVec2::new(s, 0.0),
                o + DVec2::splat(s),
                o + DVec2::new(0.0, s),
            ]);
        }
        let mesh = Mesh::new(pos, &faces).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..uvs.len() as u32).collect(),
            uvs,
        };
        let islands = Islands::compute(&mesh, &uv);
        (mesh, uv, islands)
    }

    #[test]
    fn overlaps_between_islands() {
        let (mesh, uv, islands) = quads(&[
            ([0.0, 0.0], 0.2),
            ([0.1, 0.1], 0.2),   // overlaps 0
            ([0.2, 0.0], 0.1),   // touches 0 along an edge
            ([0.5, 0.5], 0.2),   // free
            ([0.55, 0.55], 0.1), // inside 3
        ]);
        let o = find_overlaps(&mesh, &uv, &islands);
        assert_eq!(o.island_pairs, vec![[0, 1], [3, 4]]);
        assert!(o.self_overlapping_islands.is_empty());
        assert_eq!(o.faces, vec![0, 1, 3, 4]);
        assert_eq!(o.islands(), vec![0, 1, 3, 4]);
        assert!(o.face_pairs.iter().all(|p| p.faces[0] < p.faces[1]));
    }

    #[test]
    fn self_overlap_within_island() {
        // A strip of 4 quads bent into an annulus spanning 400°: the last
        // quad comes back over the first, with no flipped triangle.
        let n = 4;
        let pos: Vec<DVec3> = (0..2 * (n + 1))
            .map(|i| DVec3::new((i / 2) as f64, (i % 2) as f64, 0.0))
            .collect();
        let faces: Vec<[u32; 4]> = (0..n as u32)
            .map(|k| [2 * k, 2 * k + 2, 2 * k + 3, 2 * k + 1])
            .collect();
        let mesh = Mesh::new(pos, &faces).unwrap();
        let uvs = (0..2 * (n + 1))
            .map(|i| {
                let a = ((i / 2) as f64 * 100.0).to_radians();
                let r = if i % 2 == 0 { 0.1 } else { 0.2 };
                DVec2::splat(0.5) + r * DVec2::new(a.cos(), a.sin())
            })
            .collect();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: faces.iter().flatten().copied().collect(),
            uvs,
        };
        let islands = Islands::compute(&mesh, &uv);
        assert_eq!(islands.count(), 1);
        assert!(flipped_faces(&mesh, &uv, &islands).is_empty());
        let o = find_overlaps(&mesh, &uv, &islands);
        assert_eq!(o.self_overlapping_islands, vec![0]);
        assert!(o.island_pairs.is_empty());
        assert_eq!(o.face_pairs.len(), 1);
        assert_eq!(o.face_pairs[0].faces, [0, 3]);
    }

    #[test]
    fn stacked_copies_overlap_and_no_false_positives_in_a_grid() {
        let (mesh, uv, islands) = quads(&[([0.3, 0.3], 0.2), ([0.3, 0.3], 0.2)]);
        assert_eq!(
            find_overlaps(&mesh, &uv, &islands).island_pairs,
            vec![[0, 1]]
        );

        // A 20×20 grid of touching squares: no overlaps at all.
        let squares: Vec<_> = (0..400)
            .map(|k| ([(k % 20) as f64 * 0.05, (k / 20) as f64 * 0.05], 0.05))
            .collect();
        let (mesh, uv, islands) = quads(&squares);
        assert_eq!(find_overlaps(&mesh, &uv, &islands), Overlaps::default());
    }

    #[test]
    fn padding() {
        // Gaps: 0↔1 is 2 px, 1↔2 is 8 px, 3 is inside 2 (overlap).
        let px = 1.0 / 100.0;
        let (mesh, uv, islands) = quads(&[
            ([0.0, 0.0], 0.2),
            ([0.2 + 2.0 * px, 0.0], 0.2),
            ([0.4 + 10.0 * px, 0.0], 0.3),
            ([0.55, 0.05], 0.05),
        ]);
        let v = padding_violations(&mesh, &uv, &islands, 4.0, 100);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].islands, [0, 1]);
        assert!((v[0].distance_px - 2.0).abs() < 1e-6);

        let c = check(&mesh, &uv, &islands, 4.0, 100);
        let pairs: Vec<_> = c.padding_violations.iter().map(|v| v.islands).collect();
        assert_eq!(pairs, vec![[0, 1], [2, 3]]);
        assert_eq!(c.padding_violations[1].distance_px, 0.0);

        let c = check(&mesh, &uv, &islands, 10.0, 100);
        let pairs: Vec<_> = c.padding_violations.iter().map(|v| v.islands).collect();
        assert_eq!(pairs, vec![[0, 1], [1, 2], [2, 3]]);
        // Exactly 8 px apart is fine at 8 px.
        let v = padding_violations(&mesh, &uv, &islands, 8.0, 100);
        let pairs: Vec<_> = v.iter().map(|v| v.islands).collect();
        assert_eq!(pairs, vec![[0, 1], [2, 3]]);
        assert!((v[1].distance_px - 5.0).abs() < 1e-6);
        assert!(
            check(&mesh, &uv, &islands, 0.0, 100)
                .padding_violations
                .is_empty()
        );
    }

    #[test]
    fn boundary_of_connected_island() {
        // Two quads sharing an edge: 6 boundary segments, the shared one excluded.
        let pos: Vec<DVec3> = (0..6)
            .map(|i| DVec3::new((i / 2) as f64, (i % 2) as f64, 0.0))
            .collect();
        let faces = [[0u32, 2, 3, 1], [2, 4, 5, 3]];
        let mesh = Mesh::new(pos.clone(), &faces).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: faces.iter().flatten().copied().collect(),
            uvs: pos.iter().map(|p| p.truncate() * 0.1).collect(),
        };
        let islands = Islands::compute(&mesh, &uv);
        let b = boundary_segments(&mesh, &uv, &islands);
        assert_eq!(b.len(), 6);
        assert!(!b.contains(&(0, 2, 3)));
    }

    #[test]
    fn flipped_and_out_of_tile() {
        let (mesh, mut uv, islands) =
            quads(&[([0.0, 0.0], 0.2), ([0.9, 0.5], 0.2), ([0.5, -0.5], 0.1)]);
        assert!(flipped_faces(&mesh, &uv, &islands).is_empty());
        assert_eq!(out_of_tile_islands(&mesh, &uv, &islands), vec![1, 2]);
        // Mirror island 0 entirely: no face is flipped relative to its island.
        for k in 0..4 {
            uv.uvs[k].x = -uv.uvs[k].x + 0.5;
        }
        assert!(flipped_faces(&mesh, &uv, &islands).is_empty());

        // Two-quad island with the second quad folded over: that face is flipped.
        let pos: Vec<DVec3> = (0..6)
            .map(|i| DVec3::new((i / 2) as f64, (i % 2) as f64, 0.0))
            .collect();
        let faces = [[0u32, 2, 3, 1], [2, 4, 5, 3]];
        let mesh = Mesh::new(pos.clone(), &faces).unwrap();
        let mut uvs: Vec<DVec2> = pos.iter().map(|p| p.truncate() * 0.1).collect();
        uvs[4].x = 0.05;
        uvs[5].x = 0.05;
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: faces.iter().flatten().copied().collect(),
            uvs: uvs.clone(),
        };
        let islands = Islands::compute(&mesh, &uv);
        assert_eq!(flipped_faces(&mesh, &uv, &islands), vec![1]);
    }
}
