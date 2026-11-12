//! Island layout operations: mirror, auto-orient, align, distribute and snap.
//!
//! All of them move whole islands rigidly (except per-vertex snapping), so
//! they only touch UV coordinates, never `corner_uv`. Every UV vertex belongs
//! to exactly one island (see `Islands::split_shared_uvs`), so moving the UV
//! vertices of an island never drags another one.

use std::collections::{BTreeMap, BTreeSet};
use std::f64::consts::{FRAC_PI_2, PI};

use glam::{DMat2, DVec2, DVec3};
use serde::{Deserialize, Serialize};
use uvcraft_core::{Islands, Mesh, UvLayer};

use crate::pack::min_area_rect_angle;

/// A UV axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UvAxis {
    U,
    V,
}

impl UvAxis {
    fn index(self) -> usize {
        match self {
            UvAxis::U => 0,
            UvAxis::V => 1,
        }
    }
}

/// What to orient an island by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrientMode {
    /// Minimum-area bounding box (convex hull + rotating calipers), long side
    /// horizontal (or vertical).
    #[default]
    MinBbox,
    /// The island's longest 3D edge becomes horizontal (or vertical).
    LongestEdge,
    /// The world up axis (+Y; +Z, then +X, when +Y is nearly perpendicular to
    /// the island) points along +V (or +U).
    WorldAxis,
    /// This mesh edge becomes horizontal (or vertical) in every target island
    /// that contains it.
    Edge(usize),
}

impl OrientMode {
    /// Whether the reference direction goes along V by default.
    pub fn default_vertical(self) -> bool {
        matches!(self, OrientMode::WorldAxis)
    }
}

/// Where [`align`] moves the islands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlignTo {
    #[default]
    Min,
    Max,
    Center,
}

/// How [`distribute`] spaces the islands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DistributeBy {
    /// Equal distance between island centers.
    Centers,
    /// Equal empty space between consecutive islands.
    #[default]
    Gaps,
}

/// What [`snap`] snaps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapTarget {
    /// Every UV vertex to the nearest grid point (texel corner).
    #[default]
    Pixel,
    /// Every UV vertex to the nearest texel center.
    PixelCenter,
    /// Each island moves rigidly so its bounding-box min corner is on the grid.
    Bbox,
    /// Every UV vertex to the nearest UV vertex of a non-target island, if
    /// within the tolerance.
    Vertex,
}

/// UV bounding box `(min, max)` of an island.
pub fn island_bounds(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    island: usize,
) -> (DVec2, DVec2) {
    bounds(uv, &islands.uv_indices(island, mesh, uv))
}

fn bounds(uv: &UvLayer, ids: &[u32]) -> (DVec2, DVec2) {
    ids.iter()
        .fold((DVec2::INFINITY, DVec2::NEG_INFINITY), |(lo, hi), &k| {
            let p = uv.uvs[k as usize];
            (lo.min(p), hi.max(p))
        })
}

fn translate(uv: &mut UvLayer, ids: &[u32], t: DVec2) {
    for &k in ids {
        uv.uvs[k as usize] += t;
    }
}

fn rotate(uv: &mut UvLayer, ids: &[u32], pivot: DVec2, angle: f64) {
    let r = DMat2::from_angle(angle);
    for &k in ids {
        let p = &mut uv.uvs[k as usize];
        *p = pivot + r * (*p - pivot);
    }
}

/// Mirrors islands along `axis` (`U` flips horizontally). The mirror line is
/// at coordinate `at`, or through each island's own bounding-box center.
/// Mirroring reverses the UV winding of the island's faces.
pub fn mirror(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    axis: UvAxis,
    at: Option<f64>,
) {
    let a = axis.index();
    for &i in targets {
        let ids = islands.uv_indices(i, mesh, uv);
        let c = at.unwrap_or_else(|| {
            let (lo, hi) = bounds(uv, &ids);
            (lo[a] + hi[a]) / 2.0
        });
        for k in ids {
            let p = &mut uv.uvs[k as usize];
            p[a] = 2.0 * c - p[a];
        }
    }
}

/// Wraps an angle into (−period/2, period/2].
fn wrap(angle: f64, period: f64) -> f64 {
    let a = angle.rem_euclid(period);
    if a > period / 2.0 { a - period } else { a }
}

/// Image in UV of the 3D direction `d`, summed over the island's triangles
/// (each triangle's 3D → UV Jacobian applied to `d` projected on its plane,
/// weighted by its 3D area).
fn projected_direction(mesh: &Mesh, uv: &UvLayer, faces: &[u32], d: DVec3) -> DVec2 {
    let mut sum = DVec2::ZERO;
    for &f in faces {
        for [c0, c1, c2] in mesh.fan_triangles(f as usize) {
            let p0 = mesh.corner_position(c0);
            let (e1, e2) = (mesh.corner_position(c1) - p0, mesh.corner_position(c2) - p0);
            let (g11, g12, g22) = (e1.dot(e1), e1.dot(e2), e2.dot(e2));
            let det = g11 * g22 - g12 * g12;
            if det <= 1e-24 * g11 * g22 || det == 0.0 {
                continue;
            }
            let (r1, r2) = (d.dot(e1), d.dot(e2));
            let a = (g22 * r1 - g12 * r2) / det;
            let b = (g11 * r2 - g12 * r1) / det;
            let u0 = uv.uv_at_corner(c0);
            let image = a * (uv.uv_at_corner(c1) - u0) + b * (uv.uv_at_corner(c2) - u0);
            sum += image * (0.5 * det.sqrt());
        }
    }
    sum
}

/// UV direction of a mesh edge inside the island, if the island contains it.
fn edge_direction(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    island: usize,
    e: usize,
) -> Option<DVec2> {
    let (c, twin) = mesh.edge_corners(e);
    [Some(c), twin]
        .into_iter()
        .flatten()
        .find(|&c| islands.island_of_face(mesh.corner_face(c)) == island)
        .map(|c| uv.uv_at_corner(mesh.next(c)) - uv.uv_at_corner(c))
}

/// Rotation (radians, counter-clockwise) that orients the island, or `None`
/// if the reference direction is missing or degenerate.
fn orient_angle(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    island: usize,
    ids: &[u32],
    mode: OrientMode,
    vertical: bool,
) -> Option<f64> {
    let target = if vertical { FRAC_PI_2 } else { 0.0 };
    // Rotation bringing a line (either direction) of direction `d` onto the target axis.
    let line = |d: DVec2| (d.length_squared() > 0.0).then(|| wrap(target - d.y.atan2(d.x), PI));
    match mode {
        OrientMode::MinBbox => {
            let pts: Vec<DVec2> = ids.iter().map(|&k| uv.uvs[k as usize]).collect();
            let mut angle = wrap(min_area_rect_angle(&pts), FRAC_PI_2);
            let r = DMat2::from_angle(angle);
            let (lo, hi) =
                pts.iter()
                    .fold((DVec2::INFINITY, DVec2::NEG_INFINITY), |(lo, hi), &p| {
                        let q = r * p;
                        (lo.min(q), hi.max(q))
                    });
            let size = hi - lo;
            let long_is_vertical = size.y > size.x * (1.0 + 1e-9);
            let long_is_horizontal = size.x > size.y * (1.0 + 1e-9);
            if (vertical && long_is_horizontal) || (!vertical && long_is_vertical) {
                angle += if angle > 0.0 { -FRAC_PI_2 } else { FRAC_PI_2 };
            }
            Some(angle)
        }
        OrientMode::LongestEdge => {
            let mut best: Option<(f64, usize)> = None;
            for &f in islands.faces(island) {
                for c in mesh.face_corners(f as usize) {
                    let e = mesh.corner_edge(c);
                    let len = mesh.edge_length(e);
                    // Strictly longer, ties to the lowest edge index: deterministic.
                    if best.is_none_or(|(l, b)| len > l || (len == l && e < b)) {
                        best = Some((len, e));
                    }
                }
            }
            let (_, e) = best?;
            line(edge_direction(mesh, uv, islands, island, e)?)
        }
        OrientMode::Edge(e) => line(edge_direction(mesh, uv, islands, island, e)?),
        OrientMode::WorldAxis => {
            let faces = islands.faces(island);
            let [gx, gy, gz] =
                [DVec3::X, DVec3::Y, DVec3::Z].map(|d| projected_direction(mesh, uv, faces, d));
            let longest = gx.length().max(gy.length()).max(gz.length());
            if longest == 0.0 {
                return None;
            }
            let g = if gy.length() >= 0.5 * longest {
                gy
            } else if gz.length() >= gx.length() {
                gz
            } else {
                gx
            };
            Some(wrap(target - g.y.atan2(g.x), 2.0 * PI))
        }
    }
}

/// Rotates each target island about its bounding-box center according to
/// `mode`. `vertical` puts the reference direction along V instead of U.
/// Returns the number of islands that were oriented (an island without the
/// reference edge, or with a degenerate one, is left alone).
pub fn orient(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    mode: OrientMode,
    vertical: bool,
) -> usize {
    let mut oriented = 0;
    for &i in targets {
        let ids = islands.uv_indices(i, mesh, uv);
        let Some(angle) = orient_angle(mesh, uv, islands, i, &ids, mode, vertical) else {
            continue;
        };
        oriented += 1;
        if angle != 0.0 {
            let (lo, hi) = bounds(uv, &ids);
            rotate(uv, &ids, (lo + hi) / 2.0, angle);
        }
    }
    oriented
}

/// Moves islands along `axis` so their min, max or center lines up, either
/// on `value` or on the targets' common bounding box.
pub fn align(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    axis: UvAxis,
    to: AlignTo,
    value: Option<f64>,
) {
    let a = axis.index();
    let items: Vec<(Vec<u32>, (DVec2, DVec2))> = targets
        .iter()
        .map(|&i| {
            let ids = islands.uv_indices(i, mesh, uv);
            let b = bounds(uv, &ids);
            (ids, b)
        })
        .collect();
    let key = |(lo, hi): (DVec2, DVec2)| match to {
        AlignTo::Min => lo[a],
        AlignTo::Max => hi[a],
        AlignTo::Center => (lo[a] + hi[a]) / 2.0,
    };
    let goal = value.unwrap_or_else(|| {
        let (lo, hi) = items.iter().fold(
            (DVec2::INFINITY, DVec2::NEG_INFINITY),
            |(lo, hi), (_, (l, h))| (lo.min(*l), hi.max(*h)),
        );
        key((lo, hi))
    });
    for (ids, b) in &items {
        let mut t = DVec2::ZERO;
        t[a] = goal - key(*b);
        translate(uv, ids, t);
    }
}

/// Spreads islands evenly along `axis`, in the order of their centers. The
/// first and last islands stay put, unless a fixed `gap` is given: then the
/// islands are laid out from the first one with that empty space between them.
pub fn distribute(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    axis: UvAxis,
    by: DistributeBy,
    gap: Option<f64>,
) {
    let a = axis.index();
    let mut items: Vec<(f64, f64, usize, Vec<u32>)> = targets
        .iter()
        .map(|&i| {
            let ids = islands.uv_indices(i, mesh, uv);
            let (lo, hi) = bounds(uv, &ids);
            (lo[a], hi[a], i, ids)
        })
        .collect();
    let n = items.len();
    if n < 2 {
        return;
    }
    items.sort_by(|x, y| (x.0 + x.1).total_cmp(&(y.0 + y.1)).then(x.2.cmp(&y.2)));
    let (first, last) = (&items[0], &items[n - 1]);
    let mut moves = Vec::with_capacity(n);
    match (by, gap) {
        (_, Some(gap)) => {
            let mut cursor = first.0;
            for it in &items {
                moves.push(cursor - it.0);
                cursor += it.1 - it.0 + gap;
            }
        }
        (DistributeBy::Centers, None) => {
            let c0 = (first.0 + first.1) / 2.0;
            let step = ((last.0 + last.1) / 2.0 - c0) / (n - 1) as f64;
            for (j, it) in items.iter().enumerate() {
                moves.push(c0 + step * j as f64 - (it.0 + it.1) / 2.0);
            }
        }
        (DistributeBy::Gaps, None) => {
            let total: f64 = items.iter().map(|it| it.1 - it.0).sum();
            let gap = (last.1 - first.0 - total) / (n - 1) as f64;
            let mut cursor = first.0;
            for it in &items {
                moves.push(cursor - it.0);
                cursor += it.1 - it.0 + gap;
            }
        }
    }
    for (it, m) in items.iter().zip(moves) {
        let mut t = DVec2::ZERO;
        t[a] = m;
        translate(uv, &it.3, t);
    }
}

/// Snaps islands. `step` is the grid spacing in UV units (1 / resolution for
/// texels); `tolerance` is the maximum distance for [`SnapTarget::Vertex`].
/// Returns the number of UV vertices moved.
pub fn snap(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    to: SnapTarget,
    step: f64,
    tolerance: f64,
) -> usize {
    // Ties go up, so texel centers and corners are found the same way on both sides of 0.
    let grid = |p: DVec2, offset: f64| ((p / step - offset + 0.5).floor() + offset) * step;
    let mut moved = 0;
    let mut set = |uv: &mut UvLayer, k: u32, p: DVec2| {
        if uv.uvs[k as usize] != p {
            uv.uvs[k as usize] = p;
            moved += 1;
        }
    };
    match to {
        SnapTarget::Pixel | SnapTarget::PixelCenter => {
            let offset = if to == SnapTarget::Pixel { 0.0 } else { 0.5 };
            for &i in targets {
                for k in islands.uv_indices(i, mesh, uv) {
                    let p = grid(uv.uvs[k as usize], offset);
                    set(uv, k, p);
                }
            }
        }
        SnapTarget::Bbox => {
            for &i in targets {
                let ids = islands.uv_indices(i, mesh, uv);
                let (lo, _) = bounds(uv, &ids);
                let t = grid(lo, 0.0) - lo;
                for k in ids {
                    let p = uv.uvs[k as usize] + t;
                    set(uv, k, p);
                }
            }
        }
        SnapTarget::Vertex => {
            if tolerance <= 0.0 {
                return 0;
            }
            let mut is_target = vec![false; islands.count()];
            for &i in targets {
                is_target[i] = true;
            }
            // Bucket the other islands' UV vertices by cell of size `tolerance`.
            let cell = |p: DVec2| {
                (
                    (p.x / tolerance).floor() as i64,
                    (p.y / tolerance).floor() as i64,
                )
            };
            let mut buckets: BTreeMap<(i64, i64), Vec<u32>> = BTreeMap::new();
            for i in (0..islands.count()).filter(|&i| !is_target[i]) {
                for k in islands.uv_indices(i, mesh, uv) {
                    buckets.entry(cell(uv.uvs[k as usize])).or_default().push(k);
                }
            }
            for &i in targets {
                for k in islands.uv_indices(i, mesh, uv) {
                    let p = uv.uvs[k as usize];
                    let (cx, cy) = cell(p);
                    let mut best: Option<(f64, u32)> = None;
                    for dx in -1..=1 {
                        for dy in -1..=1 {
                            for &q in buckets.get(&(cx + dx, cy + dy)).into_iter().flatten() {
                                let d = uv.uvs[q as usize].distance_squared(p);
                                if d <= tolerance * tolerance
                                    && best.is_none_or(|(bd, bq)| d < bd || (d == bd && q < bq))
                                {
                                    best = Some((d, q));
                                }
                            }
                        }
                    }
                    if let Some((_, q)) = best {
                        let p = uv.uvs[q as usize];
                        set(uv, k, p);
                    }
                }
            }
        }
    }
    moved
}

/// Rotate (and, if needed, mirror) each result island so its layout follows
/// the orientation it had before an unfold or optimization. Scale is kept.
/// Two or more pins are left untouched because their UV positions are hard
/// constraints. An island with one pin rotates around that pin.
pub fn preserve_previous_orientation(
    mesh: &Mesh,
    before: &UvLayer,
    after: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    pinned: &[bool],
) {
    for &i in targets {
        let ids = islands.uv_indices(i, mesh, after);
        if ids.len() < 2 {
            continue;
        }
        let mut pin_ids = BTreeSet::new();
        let (mut old_area, mut new_area) = (0.0, 0.0);
        for &f in islands.faces(i) {
            for c in mesh.face_corners(f as usize) {
                if pinned.get(mesh.corner_vertex(c)).copied().unwrap_or(false) {
                    pin_ids.insert(after.corner_uv[c]);
                }
            }
            for t in mesh.fan_triangles(f as usize) {
                let [a, b, c] = t.map(|c| before.uv_at_corner(c));
                old_area += 0.5 * (b - a).perp_dot(c - a);
                let [a, b, c] = t.map(|c| after.uv_at_corner(c));
                new_area += 0.5 * (b - a).perp_dot(c - a);
            }
        }
        if pin_ids.len() >= 2 || old_area.abs() <= 1e-12 || new_area.abs() <= 1e-12 {
            continue;
        }
        let mirror = old_area.signum() != new_area.signum();
        let flipped = |p: DVec2| if mirror { DVec2::new(-p.x, p.y) } else { p };
        let n = ids.len() as f64;
        let old_center = ids.iter().map(|&k| before.uvs[k as usize]).sum::<DVec2>() / n;
        let new_center = ids
            .iter()
            .map(|&k| flipped(after.uvs[k as usize]))
            .sum::<DVec2>()
            / n;
        let (mut dot, mut cross) = (0.0, 0.0);
        for &k in &ids {
            let a = flipped(after.uvs[k as usize]) - new_center;
            let b = before.uvs[k as usize] - old_center;
            dot += a.dot(b);
            cross += a.perp_dot(b);
        }
        let rotation = DVec2::from_angle(cross.atan2(dot));
        let shift = if let Some(&k) = pin_ids.iter().next() {
            before.uvs[k as usize] - rotation.rotate(flipped(after.uvs[k as usize]) - new_center)
        } else {
            old_center
        };
        for k in ids {
            after.uvs[k as usize] =
                rotation.rotate(flipped(after.uvs[k as usize]) - new_center) + shift;
        }
    }
}

/// Ordered open path to straighten. Edge IDs must be in traversal order.
pub enum StraightenPath<'a> {
    Vertices(&'a [usize]),
    Edges(&'a [usize]),
}

#[derive(Debug, PartialEq, Eq)]
pub enum StraightenError {
    InvalidIsland,
    TooShort,
    InvalidPath,
    Disconnected,
    WrongIsland,
    NotBoundary,
    AmbiguousUv,
    Degenerate,
    PinnedConflict,
}

impl std::fmt::Display for StraightenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidIsland => "island index is out of range",
            Self::TooShort => "path needs at least two vertices",
            Self::InvalidPath => "path has an invalid or repeated vertex or edge",
            Self::Disconnected => "path edges are disconnected or branched",
            Self::WrongIsland => "path is not wholly inside one UV island",
            Self::NotBoundary => "path is not on the UV island boundary",
            Self::AmbiguousUv => "a mesh vertex has multiple UV copies in this island",
            Self::Degenerate => "UV endpoints coincide or path has zero 3D length",
            Self::PinnedConflict => "a pinned vertex is off the endpoint line or out of order",
        })
    }
}

impl std::error::Error for StraightenError {}

fn path_vertices(mesh: &Mesh, path: StraightenPath<'_>) -> Result<Vec<usize>, StraightenError> {
    match path {
        StraightenPath::Vertices(vertices) => Ok(vertices.to_vec()),
        StraightenPath::Edges(edges) => {
            let Some(&first) = edges.first() else {
                return Err(StraightenError::TooShort);
            };
            if first >= mesh.edge_count() {
                return Err(StraightenError::InvalidPath);
            }
            let [a, b] = mesh.edge_vertices(first);
            if edges.len() == 1 {
                return Ok(vec![a, b]);
            }
            let second = *edges.get(1).unwrap();
            if second >= mesh.edge_count() || second == first {
                return Err(StraightenError::InvalidPath);
            }
            let [c, d] = mesh.edge_vertices(second);
            let (start, shared) = if a == c || a == d {
                (b, a)
            } else if b == c || b == d {
                (a, b)
            } else {
                return Err(StraightenError::Disconnected);
            };
            let mut vertices = vec![start, shared];
            let mut used = BTreeSet::from([first]);
            for &e in &edges[1..] {
                if e >= mesh.edge_count() || !used.insert(e) {
                    return Err(StraightenError::InvalidPath);
                }
                let [x, y] = mesh.edge_vertices(e);
                let last = *vertices.last().unwrap();
                let next = if x == last {
                    y
                } else if y == last {
                    x
                } else {
                    return Err(StraightenError::Disconnected);
                };
                vertices.push(next);
            }
            Ok(vertices)
        }
    }
}

/// Makes an open mesh path straight in one UV island. Endpoints and pins stay
/// fixed. Unpinned vertices are spaced by cumulative 3D edge length between
/// consecutive fixed anchors. The function validates everything before edits.
pub fn straighten_path(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    island: usize,
    path: StraightenPath<'_>,
    pinned: &[bool],
) -> Result<usize, StraightenError> {
    if island >= islands.count() {
        return Err(StraightenError::InvalidIsland);
    }
    let vertices = path_vertices(mesh, path)?;
    if vertices.len() < 2 {
        return Err(StraightenError::TooShort);
    }
    let mut seen = BTreeSet::new();
    if vertices
        .iter()
        .any(|&v| v >= mesh.vertex_count() || !seen.insert(v))
    {
        return Err(StraightenError::InvalidPath);
    }
    let mut uv_by_vertex = vec![None; mesh.vertex_count()];
    let mut ambiguous = vec![false; mesh.vertex_count()];
    for &f in islands.faces(island) {
        for c in mesh.face_corners(f as usize) {
            let v = mesh.corner_vertex(c);
            let k = uv.corner_uv[c];
            if uv_by_vertex[v].is_some_and(|old| old != k) {
                ambiguous[v] = true;
            }
            uv_by_vertex[v] = Some(k);
        }
    }
    let ids: Vec<u32> = vertices
        .iter()
        .map(|&v| {
            if ambiguous[v] {
                Err(StraightenError::AmbiguousUv)
            } else {
                uv_by_vertex[v].ok_or(StraightenError::WrongIsland)
            }
        })
        .collect::<Result<_, _>>()?;
    let mut cumulative = vec![0.0; vertices.len()];
    for j in 1..vertices.len() {
        let (a, b) = (vertices[j - 1], vertices[j]);
        let edges: Vec<usize> = mesh
            .vertex_edges(a)
            .iter()
            .map(|&e| e as usize)
            .filter(|&e| mesh.edge_other(e, a) == b)
            .filter(|&e| {
                let (c0, c1) = mesh.edge_corners(e);
                islands.island_of_face(mesh.corner_face(c0)) == island
                    || c1.is_some_and(|c| islands.island_of_face(mesh.corner_face(c)) == island)
            })
            .collect();
        if edges.len() != 1 {
            return Err(StraightenError::Disconnected);
        }
        cumulative[j] = cumulative[j - 1] + mesh.edge_length(edges[0]);
    }
    if cumulative.last().copied().unwrap_or(0.0) <= 0.0 {
        return Err(StraightenError::Degenerate);
    }
    let (a, b) = (
        uv.uvs[ids[0] as usize],
        uv.uvs[*ids.last().unwrap() as usize],
    );
    let axis = b - a;
    let len2 = axis.length_squared();
    if !len2.is_finite() || len2 <= 1e-24 {
        return Err(StraightenError::Degenerate);
    }
    let mut anchors = vec![0];
    let mut last_t = 0.0;
    for j in 1..vertices.len() - 1 {
        if !pinned.get(vertices[j]).copied().unwrap_or(false) {
            continue;
        }
        let p = uv.uvs[ids[j] as usize];
        let t = (p - a).dot(axis) / len2;
        let off = (p - a).perp_dot(axis).abs() / len2.sqrt();
        if !t.is_finite() || off > 1e-9 * len2.sqrt().max(1.0) || t <= last_t || t >= 1.0 {
            return Err(StraightenError::PinnedConflict);
        }
        anchors.push(j);
        last_t = t;
    }
    anchors.push(vertices.len() - 1);
    let mut target: Vec<DVec2> = ids.iter().map(|&k| uv.uvs[k as usize]).collect();
    for pair in anchors.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        let span = cumulative[end] - cumulative[start];
        if span <= 0.0 {
            return Err(StraightenError::Degenerate);
        }
        let (p, q) = (target[start], target[end]);
        for j in start + 1..end {
            let t = (cumulative[j] - cumulative[start]) / span;
            target[j] = p.lerp(q, t);
        }
    }
    let mut moved = 0;
    for (&k, p) in ids.iter().zip(target) {
        if uv.uvs[k as usize] != p {
            uv.uvs[k as usize] = p;
            moved += 1;
        }
    }
    Ok(moved)
}

/// Projects an ordered island-border path onto a horizontal (`U`) or vertical
/// (`V`) line. The coordinates along the line are preserved. Pins determine
/// the line's position; without pins, the mean perpendicular coordinate does.
/// All path and pin constraints are checked before modifying the UV layer.
pub fn align_border_path(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    island: usize,
    path: StraightenPath<'_>,
    axis: UvAxis,
    pinned: &[bool],
) -> Result<usize, StraightenError> {
    if island >= islands.count() {
        return Err(StraightenError::InvalidIsland);
    }
    let vertices = path_vertices(mesh, path)?;
    if vertices.len() < 2 {
        return Err(StraightenError::TooShort);
    }
    let mut seen = BTreeSet::new();
    if vertices
        .iter()
        .any(|&v| v >= mesh.vertex_count() || !seen.insert(v))
    {
        return Err(StraightenError::InvalidPath);
    }
    let mut uv_by_vertex = vec![None; mesh.vertex_count()];
    let mut ambiguous = vec![false; mesh.vertex_count()];
    for &f in islands.faces(island) {
        for c in mesh.face_corners(f as usize) {
            let v = mesh.corner_vertex(c);
            let id = uv.corner_uv[c];
            if uv_by_vertex[v].is_some_and(|old| old != id) {
                ambiguous[v] = true;
            }
            uv_by_vertex[v] = Some(id);
        }
    }
    let ids: Vec<u32> = vertices
        .iter()
        .map(|&v| {
            if ambiguous[v] {
                Err(StraightenError::AmbiguousUv)
            } else {
                uv_by_vertex[v].ok_or(StraightenError::WrongIsland)
            }
        })
        .collect::<Result<_, _>>()?;
    for pair in vertices.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let edges: Vec<_> = mesh
            .vertex_edges(a)
            .iter()
            .map(|&e| e as usize)
            .filter(|&e| mesh.edge_other(e, a) == b)
            .filter(|&e| uv.edge_kind(mesh, e).is_island_boundary())
            .filter(|&e| {
                let (c0, c1) = mesh.edge_corners(e);
                islands.island_of_face(mesh.corner_face(c0)) == island
                    || c1.is_some_and(|c| islands.island_of_face(mesh.corner_face(c)) == island)
            })
            .collect();
        if edges.len() != 1 {
            return Err(StraightenError::NotBoundary);
        }
    }
    let along = axis.index();
    let across = 1 - along;
    let positions: Vec<DVec2> = ids.iter().map(|&id| uv.uvs[id as usize]).collect();
    if positions.iter().any(|p| !p.is_finite()) {
        return Err(StraightenError::Degenerate);
    }
    let lo = positions
        .iter()
        .map(|p| p[along])
        .fold(f64::INFINITY, f64::min);
    let hi = positions
        .iter()
        .map(|p| p[along])
        .fold(f64::NEG_INFINITY, f64::max);
    if hi - lo <= 1e-12 {
        return Err(StraightenError::Degenerate);
    }
    let fixed: Vec<f64> = vertices
        .iter()
        .zip(&positions)
        .filter_map(|(&v, p)| pinned.get(v).copied().unwrap_or(false).then_some(p[across]))
        .collect();
    let line = fixed.first().copied().unwrap_or_else(|| {
        positions.iter().map(|p| p[across]).sum::<f64>() / positions.len() as f64
    });
    let tolerance = 1e-9 * (hi - lo).max(1.0);
    if fixed.iter().any(|&value| (value - line).abs() > tolerance) {
        return Err(StraightenError::PinnedConflict);
    }
    let mut moved = 0;
    for (&v, &id) in vertices.iter().zip(&ids) {
        if pinned.get(v).copied().unwrap_or(false) {
            continue;
        }
        let p = &mut uv.uvs[id as usize];
        if p[across] != line {
            p[across] = line;
            moved += 1;
        }
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn border_alignment_is_axis_constrained_and_atomic_with_pins() {
        let (mesh, mut uv, islands) = two_quads(1.0);
        uv.uvs[0].y = 0.2;
        uv.uvs[1].y = -0.1;
        let original = uv.clone();
        let mut pins = vec![false; mesh.vertex_count()];
        pins[0] = true;
        pins[1] = true;
        assert_eq!(
            align_border_path(
                &mesh,
                &mut uv,
                &islands,
                0,
                StraightenPath::Vertices(&[0, 1]),
                UvAxis::U,
                &pins
            ),
            Err(StraightenError::PinnedConflict)
        );
        assert_eq!(uv, original);
        pins[1] = false;
        assert_eq!(
            align_border_path(
                &mesh,
                &mut uv,
                &islands,
                0,
                StraightenPath::Vertices(&[0, 1]),
                UvAxis::U,
                &pins
            ),
            Ok(1)
        );
        assert_eq!(uv.uvs[0].y, 0.2);
        assert_eq!(uv.uvs[1].y, 0.2);
        assert_eq!(uv.uvs[1].x, 1.0);
        let before = uv.clone();
        assert_eq!(
            align_border_path(
                &mesh,
                &mut uv,
                &islands,
                0,
                StraightenPath::Vertices(&[0, 2]),
                UvAxis::U,
                &[]
            ),
            Err(StraightenError::NotBoundary)
        );
        assert_eq!(uv, before);
    }

    #[test]
    fn straightens_vertex_and_edge_paths_by_3d_length() {
        let (mesh, mut uv, islands) = two_quads(1.0);
        let moved = straighten_path(
            &mesh,
            &mut uv,
            &islands,
            0,
            StraightenPath::Vertices(&[0, 1, 2]),
            &[],
        )
        .unwrap();
        assert_eq!(moved, 1);
        assert!(uv.uvs[1].distance(DVec2::splat(0.5)) < 1e-12);
        let (mesh, mut uv, islands) = two_quads(1.0);
        let edge = |a: usize, b: usize| {
            mesh.vertex_edges(a)
                .iter()
                .map(|&e| e as usize)
                .find(|&e| mesh.edge_other(e, a) == b)
                .unwrap()
        };
        let moved = straighten_path(
            &mesh,
            &mut uv,
            &islands,
            0,
            StraightenPath::Edges(&[edge(0, 1), edge(1, 2)]),
            &[],
        )
        .unwrap();
        assert_eq!(moved, 1);
        assert!(uv.uvs[1].distance(DVec2::splat(0.5)) < 1e-12);
    }

    #[test]
    fn straighten_path_respects_pins_and_rejects_bad_paths_without_editing() {
        let (mesh, mut uv, islands) = two_quads(1.0);
        let original = uv.clone();
        let mut pins = vec![false; mesh.vertex_count()];
        pins[1] = true;
        assert_eq!(
            straighten_path(
                &mesh,
                &mut uv,
                &islands,
                0,
                StraightenPath::Vertices(&[0, 1, 2]),
                &pins
            ),
            Err(StraightenError::PinnedConflict)
        );
        assert_eq!(uv, original);
        assert_eq!(
            straighten_path(
                &mesh,
                &mut uv,
                &islands,
                0,
                StraightenPath::Vertices(&[0, 4]),
                &[]
            ),
            Err(StraightenError::WrongIsland)
        );
        assert_eq!(
            straighten_path(
                &mesh,
                &mut uv,
                &islands,
                0,
                StraightenPath::Vertices(&[0, 1, 2, 3, 0]),
                &[]
            ),
            Err(StraightenError::InvalidPath)
        );
        let first = mesh.vertex_edges(0)[0] as usize;
        let other = mesh.vertex_edges(4)[0] as usize;
        assert_eq!(
            straighten_path(
                &mesh,
                &mut uv,
                &islands,
                0,
                StraightenPath::Edges(&[first, other]),
                &[]
            ),
            Err(StraightenError::Disconnected)
        );
        assert_eq!(uv, original);
        uv.uvs[1] = DVec2::new(0.0, 0.25); // pin lies on endpoint line 0→3
        let moved = straighten_path(
            &mesh,
            &mut uv,
            &islands,
            0,
            StraightenPath::Vertices(&[0, 1, 2, 3]),
            &pins,
        )
        .unwrap();
        assert_eq!(moved, 1);
        assert_eq!(uv.uvs[1], DVec2::new(0.0, 0.25));
        assert!(uv.uvs[2].distance(DVec2::new(0.0, 0.625)) < 1e-12);
    }

    #[test]
    fn previous_orientation_restores_rotation_and_mirror() {
        let (mesh, before, islands) = two_quads(1.0);
        let mut after = before.clone();
        let ids = islands.uv_indices(0, &mesh, &after);
        for &k in &ids {
            let p = after.uvs[k as usize];
            after.uvs[k as usize] =
                DVec2::from_angle(0.7).rotate(DVec2::new(-p.x, p.y)) + DVec2::new(5.0, 3.0);
        }
        preserve_previous_orientation(&mesh, &before, &mut after, &islands, &[0], &[]);
        for k in ids {
            assert!(after.uvs[k as usize].distance(before.uvs[k as usize]) < 1e-9);
        }
    }

    /// Two unit quads side by side in 3D (x ∈ [0,1] and [2,3], in the XY
    /// plane), each its own island, with UVs = 3D (x, y) scaled by `s`.
    fn two_quads(s: f64) -> (Mesh, UvLayer, Islands) {
        let p = vec![
            DVec3::new(0., 0., 0.),
            DVec3::new(1., 0., 0.),
            DVec3::new(1., 1., 0.),
            DVec3::new(0., 1., 0.),
            DVec3::new(2., 0., 0.),
            DVec3::new(3., 0., 0.),
            DVec3::new(3., 1., 0.),
            DVec3::new(2., 1., 0.),
        ];
        let m = Mesh::new(p.clone(), &[[0, 1, 2, 3], [4, 5, 6, 7]]).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..8).collect(),
            uvs: p.iter().map(|q| DVec2::new(q.x, q.y) * s).collect(),
        };
        let isl = Islands::compute(&m, &uv);
        (m, uv, isl)
    }

    /// One 3D rectangle 4 × 1 (long along X), with UVs rotated by `angle`.
    fn rect(angle: f64) -> (Mesh, UvLayer, Islands) {
        let p = vec![
            DVec3::new(0., 0., 0.),
            DVec3::new(4., 0., 0.),
            DVec3::new(4., 1., 0.),
            DVec3::new(0., 1., 0.),
        ];
        let m = Mesh::new(p.clone(), &[[0, 1, 2, 3]]).unwrap();
        let r = DMat2::from_angle(angle);
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..4).collect(),
            uvs: p.iter().map(|q| r * DVec2::new(q.x, q.y) * 0.1).collect(),
        };
        let isl = Islands::compute(&m, &uv);
        (m, uv, isl)
    }

    fn size(m: &Mesh, uv: &UvLayer, isl: &Islands, i: usize) -> DVec2 {
        let (lo, hi) = island_bounds(m, uv, isl, i);
        hi - lo
    }

    fn close(a: DVec2, b: DVec2) -> bool {
        (a - b).length() < 1e-9
    }

    #[test]
    fn mirror_about_center_and_line() {
        let (m, mut uv, isl) = two_quads(0.25);
        let before = uv.uvs.clone();
        mirror(&m, &mut uv, &isl, &[0], UvAxis::U, None);
        // Island 0 spans u ∈ [0, 0.25]: u → 0.25 − u, v unchanged.
        assert!(close(uv.uvs[0], DVec2::new(0.25, 0.0)));
        assert!(close(uv.uvs[2], DVec2::new(0.0, 0.25)));
        assert_eq!(uv.uvs[4..], before[4..], "other island untouched");
        mirror(&m, &mut uv, &isl, &[1], UvAxis::V, Some(0.5));
        assert!(close(uv.uvs[4], DVec2::new(0.5, 1.0)));
        assert!(close(uv.uvs[6], DVec2::new(0.75, 0.75)));
    }

    #[test]
    fn orient_min_bbox_horizontal_and_vertical() {
        for angle in [0.3, -1.0, 2.0, FRAC_PI_2] {
            let (m, mut uv, isl) = rect(angle);
            let (lo, hi) = island_bounds(&m, &uv, &isl, 0);
            assert_eq!(
                orient(&m, &mut uv, &isl, &[0], OrientMode::MinBbox, false),
                1
            );
            assert!(
                close(size(&m, &uv, &isl, 0), DVec2::new(0.4, 0.1)),
                "{angle}"
            );
            let (lo2, hi2) = island_bounds(&m, &uv, &isl, 0);
            assert!(close(lo + hi, lo2 + hi2), "keeps the center");
            orient(&m, &mut uv, &isl, &[0], OrientMode::MinBbox, true);
            assert!(close(size(&m, &uv, &isl, 0), DVec2::new(0.1, 0.4)));
        }
    }

    #[test]
    fn orient_by_edges() {
        let (m, mut uv, isl) = rect(0.7);
        orient(&m, &mut uv, &isl, &[0], OrientMode::LongestEdge, false);
        assert!(close(size(&m, &uv, &isl, 0), DVec2::new(0.4, 0.1)));
        // Edge 0→3 (the short side, along 3D Y) made horizontal.
        let e = (0..m.edge_count())
            .find(|&e| {
                let [a, b] = m.edge_vertices(e);
                a.min(b) == 0 && a.max(b) == 3
            })
            .unwrap();
        orient(&m, &mut uv, &isl, &[0], OrientMode::Edge(e), false);
        assert!(close(size(&m, &uv, &isl, 0), DVec2::new(0.1, 0.4)));
        // An island without the edge is left alone.
        let (m2, mut uv2, isl2) = two_quads(0.1);
        assert_eq!(
            orient(&m2, &mut uv2, &isl2, &[1], OrientMode::Edge(0), false),
            0
        );
    }

    #[test]
    fn orient_world_axis_points_up() {
        // UVs rotated by 2.5 rad (more than 90°): world +Y must end up along +V.
        let (m, mut uv, isl) = rect(2.5);
        orient(&m, &mut uv, &isl, &[0], OrientMode::WorldAxis, true);
        // Vertex 3 is at 3D y = 1, vertex 0 at y = 0: same u, higher v.
        let d = uv.uvs[3] - uv.uvs[0];
        assert!(close(d, DVec2::new(0.0, 0.1)), "{d}");
        orient(&m, &mut uv, &isl, &[0], OrientMode::WorldAxis, false);
        let d = uv.uvs[3] - uv.uvs[0];
        assert!(close(d, DVec2::new(0.1, 0.0)), "{d}");
    }

    #[test]
    fn align_and_distribute() {
        let (m, mut uv, isl) = two_quads(0.1);
        // Island 0: u ∈ [0, 0.1], island 1: u ∈ [0.2, 0.3]; move 1 up first.
        for k in 4..8 {
            uv.uvs[k].y += 0.5;
        }
        align(&m, &mut uv, &isl, &[0, 1], UvAxis::V, AlignTo::Min, None);
        assert!((island_bounds(&m, &uv, &isl, 1).0.y).abs() < 1e-12);
        align(
            &m,
            &mut uv,
            &isl,
            &[0, 1],
            UvAxis::U,
            AlignTo::Center,
            Some(0.5),
        );
        for i in 0..2 {
            let (lo, hi) = island_bounds(&m, &uv, &isl, i);
            assert!(((lo.x + hi.x) / 2.0 - 0.5).abs() < 1e-12);
        }
        align(
            &m,
            &mut uv,
            &isl,
            &[0, 1],
            UvAxis::U,
            AlignTo::Max,
            Some(1.0),
        );
        assert!((island_bounds(&m, &uv, &isl, 0).1.x - 1.0).abs() < 1e-12);

        // Fixed gap: laid out left to right from the first island.
        let (m, mut uv, isl) = two_quads(0.1);
        distribute(
            &m,
            &mut uv,
            &isl,
            &[0, 1],
            UvAxis::U,
            DistributeBy::Gaps,
            Some(0.05),
        );
        assert!((island_bounds(&m, &uv, &isl, 1).0.x - 0.15).abs() < 1e-12);
    }

    #[test]
    fn distribute_three_islands() {
        // Three quads of widths 0.1, 0.3, 0.1 at u = 0, 0.15, 0.9.
        let p: Vec<DVec3> = (0..12)
            .map(|i| DVec3::new(i as f64, (i % 2) as f64, 0.0))
            .collect();
        let m = Mesh::new(p, &[[0, 1, 2, 3], [4, 5, 6, 7], [8, 9, 10, 11]]).unwrap();
        let quad = |x: f64, w: f64| {
            [
                DVec2::new(x, 0.),
                DVec2::new(x + w, 0.),
                DVec2::new(x + w, 0.1),
                DVec2::new(x, 0.1),
            ]
        };
        let uvs: Vec<DVec2> = [quad(0.0, 0.1), quad(0.15, 0.3), quad(0.9, 0.1)].concat();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..12).collect(),
            uvs,
        };
        let isl = Islands::compute(&m, &uv);

        let mut g = uv.clone();
        distribute(
            &m,
            &mut g,
            &isl,
            &[0, 1, 2],
            UvAxis::U,
            DistributeBy::Gaps,
            None,
        );
        // Span [0, 1], widths 0.5 total: gaps of 0.25.
        assert!((island_bounds(&m, &g, &isl, 1).0.x - 0.35).abs() < 1e-12);
        assert!((island_bounds(&m, &g, &isl, 2).0.x - 0.9).abs() < 1e-12);

        let mut c = uv.clone();
        distribute(
            &m,
            &mut c,
            &isl,
            &[0, 1, 2],
            UvAxis::U,
            DistributeBy::Centers,
            None,
        );
        // Centers 0.05 and 0.95 stay: the middle one goes to 0.5.
        let (lo, hi) = island_bounds(&m, &c, &isl, 1);
        assert!(((lo.x + hi.x) / 2.0 - 0.5).abs() < 1e-12);
    }

    #[test]
    fn snap_modes() {
        let (m, uv0, isl) = two_quads(0.1);
        let step = 1.0 / 8.0;
        // Pixel: 0.1 → 0.125, 0.2 → 0.25, 0.3 → 0.25.
        let mut uv = uv0.clone();
        assert!(snap(&m, &mut uv, &isl, &[0, 1], SnapTarget::Pixel, step, 0.0) > 0);
        assert!(uv.uvs.iter().all(|p| close(*p, (*p / step).round() * step)));
        assert!(close(uv.uvs[2], DVec2::new(0.125, 0.125)));

        let mut uv = uv0.clone();
        snap(&m, &mut uv, &isl, &[0], SnapTarget::PixelCenter, step, 0.0);
        assert!(close(uv.uvs[0], DVec2::new(0.0625, 0.0625)));

        // Bbox: island 1 (u ∈ [0.2, 0.3]) moves rigidly to u ∈ [0.25, 0.35].
        let mut uv = uv0.clone();
        snap(&m, &mut uv, &isl, &[1], SnapTarget::Bbox, step, 0.0);
        assert!(close(uv.uvs[4], DVec2::new(0.25, 0.0)));
        assert!(close(uv.uvs[6], DVec2::new(0.35, 0.1)));
        assert_eq!(uv.uvs[..4], uv0.uvs[..4]);

        // Vertex: island 1's left side (u = 0.2) snaps onto island 0's right side
        // (u = 0.1) with a large enough tolerance only.
        let mut uv = uv0.clone();
        assert_eq!(
            snap(&m, &mut uv, &isl, &[1], SnapTarget::Vertex, step, 0.05),
            0
        );
        assert_eq!(
            snap(&m, &mut uv, &isl, &[1], SnapTarget::Vertex, step, 0.15),
            2
        );
        assert!(close(uv.uvs[4], uv0.uvs[1]));
        assert!(close(uv.uvs[7], uv0.uvs[2]));
        assert_eq!(uv.uvs[5], uv0.uvs[5], "too far: stays");
    }
}
