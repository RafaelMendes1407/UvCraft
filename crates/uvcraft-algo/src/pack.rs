//! Island layout: orientation, texel density and packing.
//!
//! Packing: islands are optionally rotated to their minimum-area bounding
//! rectangle, then placed inside the 0–1 tile (or a `region`) with one
//! uniform scale (which keeps relative texel density): the largest one that
//! fits (`rescale`, "fill"), or their current size / a given texel density,
//! shrunk only when needed. Padding and margin are in pixels at the given
//! resolution, or in UV units.
//! - [`PackMethod::Raster`] (default): bitmask packing that fills holes and
//!   concavities and tries 90° rotations, custom angle steps and mirroring
//!   (see [`raster`]).
//! - [`PackMethod::Shelf`]: bounding boxes on shelves; fast, lower coverage.
//!
//! Multi-tile (UDIM) packing (`tiles` / `tiles_u`): islands keep their size
//! and overflow into the next tiles 1001, 1002, … (row-major, `tiles_u`
//! columns per row); raster only.

use glam::DVec2;
use serde::{Deserialize, Serialize};
use uvcraft_core::{Cancelled, Islands, Mesh, Progress, UvLayer};

use crate::analysis::texel_density;
use crate::checks;

pub mod raster;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackMethod {
    #[default]
    Raster,
    Shelf,
    /// Try several shape orders and keep the best raster placement.
    Efficient,
}

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
    /// Shelf: lay each island flat (width ≥ height). Raster: try all four
    /// 90° rotations.
    #[serde(default = "yes")]
    pub rotate_90: bool,
    #[serde(default)]
    pub method: PackMethod,
    /// Scale islands to fill the space. If false, they keep their size and
    /// only shrink when they would not fit.
    #[serde(default = "yes")]
    pub rescale: bool,
    /// When packing some islands only, keep the others where they are and
    /// pack around them (raster method).
    #[serde(default = "yes")]
    pub around_others: bool,
    /// Pack into `[umin, vmin, umax, vmax]` instead of the 0–1 tile (the
    /// margin applies inside it). In multi-tile mode: the region of tile 1001,
    /// repeated in each tile.
    #[serde(default)]
    pub region: Option<[f64; 4]>,
    /// Multi-tile mode: use at most this many tiles.
    #[serde(default)]
    pub tiles: Option<u32>,
    /// Multi-tile mode: tiles per row (1–10, default 10). Given alone, up to
    /// [`MAX_TILES`] tiles may be used.
    #[serde(default)]
    pub tiles_u: Option<u32>,
    /// Raster: also try mirrored islands (their UVs end up flipped).
    #[serde(default)]
    pub mirror: bool,
    /// Raster: try rotations in steps of this many degrees (90 = quarter
    /// turns, 360 = none, small = nearly free rotation). Overrides `rotate_90`.
    #[serde(default)]
    pub rotation_step_deg: Option<f64>,
    #[serde(default)]
    pub quality: PackQuality,
    /// Padding in UV units (overrides `padding_px`).
    #[serde(default)]
    pub padding_uv: Option<f64>,
    /// Margin in UV units (overrides `margin_px`).
    #[serde(default)]
    pub margin_uv: Option<f64>,
    /// Keep texel density: scale each island to this density (pixels per 3D
    /// unit at `resolution`) first, then never enlarge (as `rescale: false`).
    #[serde(default)]
    pub texel_density: Option<f64>,
    /// Align each packed item's UV bounding-box minimum to the texel grid.
    #[serde(default)]
    pub pixel_aligned: bool,
}

/// Speed vs. packing quality (raster): grid resolution and number of trials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackQuality {
    /// Coarser grid, fewer scales, two 90° rotations (0° and 90°).
    Low,
    #[default]
    Normal,
    /// Finer grid, more scales.
    High,
}

impl PackQuality {
    /// Cells along the longest side of the raster grid.
    fn cells(self, resolution: u32) -> usize {
        let r = resolution as usize;
        match self {
            PackQuality::Low => (r / 4).clamp(32, 512),
            PackQuality::Normal => raster_cells(resolution),
            PackQuality::High => r.clamp(64, 2048),
        }
    }

    fn effort(self) -> raster::Effort {
        match self {
            PackQuality::Low => raster::Effort {
                coarse_trials: 8,
                coarse_ratio: 0.85,
                refine_trials: 6,
                fine_range: (-1, 4),
                fine_step: 0.02,
            },
            PackQuality::Normal => raster::Effort::NORMAL,
            PackQuality::High => raster::Effort {
                coarse_trials: 24,
                coarse_ratio: 0.95,
                refine_trials: 24,
                fine_range: (-4, 16),
                fine_step: 0.005,
            },
        }
    }

    /// Quarter turns tried with `rotate_90`.
    fn quarters(self) -> &'static [u8] {
        match self {
            PackQuality::Low => &[0, 1],
            _ => &[0, 1, 2, 3],
        }
    }
}

/// Default (and largest) number of tiles per row in multi-tile packing.
pub const UDIM_COLUMNS: u32 = 10;
/// Most tiles a multi-tile pack may use.
pub const MAX_TILES: u32 = 1000;

/// UDIM number of a tile index (row-major, `cols` tiles per row).
pub fn udim(tile: usize, cols: usize) -> u32 {
    let cols = cols.max(1);
    1001 + (tile % cols) as u32 + 10 * (tile / cols) as u32
}

impl PackOptions {
    /// Multi-tile (UDIM) packing is requested.
    pub fn multi_tile(&self) -> bool {
        self.tiles.is_some() || self.tiles_u.is_some()
    }

    /// Checks the option ranges; returns a message for the first bad one.
    pub fn validate(&self) -> Result<(), String> {
        let check = |bad: bool, msg: &str| if bad { Err(msg.to_string()) } else { Ok(()) };
        let nonneg = |v: Option<f64>| v.is_some_and(|v| !(v >= 0.0 && v.is_finite()));
        check(
            !(self.padding_px >= 0.0 && self.margin_px >= 0.0) || self.resolution == 0,
            "padding and margin must be ≥ 0, resolution > 0",
        )?;
        check(
            nonneg(self.padding_uv) || nonneg(self.margin_uv),
            "padding_uv and margin_uv must be ≥ 0",
        )?;
        if let Some([u0, v0, u1, v1]) = self.region {
            check(
                ![u0, v0, u1, v1].iter().all(|v| v.is_finite()) || u1 <= u0 || v1 <= v0,
                "region must be [umin, vmin, umax, vmax] with umin < umax and vmin < vmax",
            )?;
            check(
                self.multi_tile() && (u1 - u0 > 1.0 || v1 - v0 > 1.0),
                "with tiles, the region must fit in one tile",
            )?;
        }
        check(
            self.tiles.is_some_and(|t| t == 0 || t > MAX_TILES),
            "tiles must be 1–1000",
        )?;
        check(
            self.tiles_u.is_some_and(|t| t == 0 || t > UDIM_COLUMNS),
            "tiles_u must be 1–10",
        )?;
        check(
            self.rotation_step_deg
                .is_some_and(|s| !(s > 0.0 && s <= 360.0)),
            "rotation_step_deg must be in (0, 360]",
        )?;
        check(
            self.texel_density
                .is_some_and(|d| !(d > 0.0 && d.is_finite())),
            "texel_density must be > 0",
        )
    }

    /// Transforms the raster packer tries, non-mirrored first.
    fn xforms(&self) -> Vec<raster::Xform> {
        let mut out: Vec<raster::Xform> = match self.rotation_step_deg {
            Some(step) => {
                let n = (360.0 / step - 1e-9).ceil().max(1.0) as usize;
                (0..n)
                    .map(|k| {
                        let deg = k as f64 * step;
                        let q = (deg / 90.0 + 1e-9).floor();
                        let rest = deg - 90.0 * q;
                        raster::Xform {
                            quarter: (q as u8) % 4,
                            angle: if rest.abs() < 1e-9 {
                                0.0
                            } else {
                                rest.to_radians()
                            },
                            mirror: false,
                        }
                    })
                    .collect()
            }
            None if self.rotate_90 => self
                .quality
                .quarters()
                .iter()
                .map(|&k| raster::Xform::quarter(k))
                .collect(),
            None => vec![raster::Xform::IDENTITY],
        };
        if self.mirror {
            let mirrored: Vec<raster::Xform> = out
                .iter()
                .map(|&x| raster::Xform { mirror: true, ..x })
                .collect();
            out.extend(mirrored);
        }
        out
    }
}

/// What [`pack`] did.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PackResult {
    /// Uniform scale applied to the islands.
    pub scale: f64,
    /// Islands avoided as obstacles.
    pub obstacles: usize,
    /// There was no room around the obstacles: packed over the whole tile.
    pub ignored_obstacles: bool,
    /// Tiles used in multi-tile mode (0 otherwise).
    pub tiles_used: usize,
    /// The requested pixel-aligned layout did not fit; UVs were restored.
    pub failed: bool,
}

impl Default for PackOptions {
    fn default() -> Self {
        PackOptions {
            padding_px: default_padding(),
            margin_px: default_padding(),
            resolution: default_resolution(),
            orient: true,
            rotate_90: true,
            method: PackMethod::Raster,
            rescale: true,
            around_others: true,
            region: None,
            tiles: None,
            tiles_u: None,
            mirror: false,
            rotation_step_deg: None,
            quality: PackQuality::Normal,
            padding_uv: None,
            margin_uv: None,
            texel_density: None,
            pixel_aligned: false,
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

/// Shelf placement of sizes (already scaled) in a `w × h` rectangle.
/// Returns the positions, or `None` if they do not fit.
fn shelf(sizes: &[(usize, DVec2)], w: f64, h: f64, pad: f64) -> Option<Vec<(usize, DVec2)>> {
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
    (y + row <= h + 1e-12).then_some(out)
}

/// Packs the target islands into the 0–1 tile or `region` (around the other
/// islands if `around_others`), or across several tiles in multi-tile mode.
/// Obstacles and multi-tile mode force the raster method. Options are assumed
/// valid ([`PackOptions::validate`]).
pub fn pack(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    opts: &PackOptions,
) -> PackResult {
    pack_grouped(mesh, uv, islands, targets, opts, &[])
}

/// Packing with cooperative cancellation. Layout is committed only after the
/// final cancellation check; callbacks receive start and completion events.
pub fn pack_grouped_with_progress(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    opts: &PackOptions,
    groups: &[Vec<usize>],
    progress: &Progress,
) -> Result<PackResult, Cancelled> {
    progress.report("pack", 0, 1)?;
    let mut trial = uv.clone();
    let result = pack_grouped_impl(mesh, &mut trial, islands, targets, opts, groups, progress)?;
    progress.report("pack", 1, 1)?;
    *uv = trial;
    Ok(result)
}

pub fn pack_padding_perfect_with_progress(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    opts: &PackOptions,
    groups: &[Vec<usize>],
    progress: &Progress,
) -> Result<PackResult, Cancelled> {
    progress.report("pack_padding_perfect", 0, 1)?;
    let mut trial = uv.clone();
    let result =
        pack_padding_perfect_impl(mesh, &mut trial, islands, targets, opts, groups, progress)?;
    progress.report("pack_padding_perfect", 1, 1)?;
    *uv = trial;
    Ok(result)
}

/// Packs islands in each group as one rigid item. Members keep their relative
/// positions, rotation and scale; ungrouped target islands remain independent.
/// Group indices are validated by the command layer.
pub fn pack_grouped(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    opts: &PackOptions,
    groups: &[Vec<usize>],
) -> PackResult {
    pack_grouped_impl(
        mesh,
        uv,
        islands,
        targets,
        opts,
        groups,
        &Progress::default(),
    )
    .expect("default progress cannot be cancelled")
}

fn pack_grouped_impl(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    opts: &PackOptions,
    groups: &[Vec<usize>],
    progress: &Progress,
) -> Result<PackResult, Cancelled> {
    if opts.method == PackMethod::Efficient {
        let source = uv.clone();
        let trial_opts = PackOptions {
            method: PackMethod::Raster,
            ..*opts
        };
        let mut best: Option<(PackResult, UvLayer)> = None;
        for order_mode in 0..3 {
            progress.report("pack_efficient", order_mode, 3)?;
            let mut trial = source.clone();
            let result = pack_grouped_impl_order(
                mesh,
                &mut trial,
                islands,
                targets,
                &trial_opts,
                groups,
                progress,
                order_mode,
            )?;
            if !result.failed
                && best
                    .as_ref()
                    .is_none_or(|(prior, _)| result.scale > prior.scale + 1e-12)
            {
                best = Some((result, trial));
            }
        }
        progress.report("pack_efficient", 3, 3)?;
        if let Some((result, layout)) = best {
            *uv = layout;
            return Ok(result);
        }
        return Ok(PackResult {
            failed: true,
            ..PackResult::default()
        });
    }
    pack_grouped_impl_order(mesh, uv, islands, targets, opts, groups, progress, 0)
}

#[allow(clippy::too_many_arguments)]
fn pack_grouped_impl_order(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    opts: &PackOptions,
    groups: &[Vec<usize>],
    progress: &Progress,
    order_mode: u64,
) -> Result<PackResult, Cancelled> {
    progress.check()?;
    let original_uv = opts.pixel_aligned.then(|| uv.clone());
    let res = opts.resolution.max(1) as f64;
    let pad = opts.padding_uv.unwrap_or(opts.padding_px / res)
        + if opts.pixel_aligned { 2.0 / res } else { 0.0 };
    let margin = opts.margin_uv.unwrap_or(opts.margin_px / res);
    let [u0, v0, u1, v1] = opts.region.unwrap_or([0.0, 0.0, 1.0, 1.0]);
    let origin = DVec2::new(u0, v0) + margin;
    let size = DVec2::new(u1 - u0, v1 - v0)
        - 2.0 * margin
        - if opts.pixel_aligned {
            DVec2::splat(1.0 / res)
        } else {
            DVec2::ZERO
        };
    let mut result = PackResult::default();
    if targets.is_empty() || size.x <= 0.0 || size.y <= 0.0 {
        result.failed = opts.pixel_aligned && !targets.is_empty();
        return Ok(result);
    }
    let multi = opts.multi_tile();
    let cols = opts.tiles_u.unwrap_or(UDIM_COLUMNS).max(1) as usize;
    let max_tiles = if multi {
        opts.tiles.unwrap_or(MAX_TILES) as usize
    } else {
        1
    };
    let mut is_target = vec![false; islands.count()];
    for &i in targets {
        is_target[i] = true;
    }
    let mut obstacles: Vec<[DVec2; 3]> = Vec::new();
    if opts.around_others {
        for i in (0..islands.count()).filter(|&i| !is_target[i]) {
            result.obstacles += 1;
            for &f in islands.faces(i) {
                obstacles.extend(
                    mesh.fan_triangles(f as usize)
                        .map(|t| t.map(|c| uv.uv_at_corner(c))),
                );
            }
        }
    }
    let raster = opts.pixel_aligned
        || matches!(opts.method, PackMethod::Raster | PackMethod::Efficient)
        || !obstacles.is_empty()
        || multi;
    if let Some(density) = opts.texel_density {
        // The group is one packing item. Target density must therefore use
        // one factor and one pivot for all its members as well.
        set_texel_density_grouped(mesh, uv, islands, targets, groups, density, opts.resolution);
    }

    // Build one item per group, then one for each remaining island.
    let mut grouped = vec![false; islands.count()];
    let mut item_islands = Vec::with_capacity(targets.len());
    for group in groups {
        for &i in group {
            grouped[i] = true;
        }
        item_islands.push(group.clone());
    }
    item_islands.extend(
        targets
            .iter()
            .copied()
            .filter(|&i| !grouped[i])
            .map(|i| vec![i]),
    );

    // Normalize each item: rotate, lay flat, move to the origin.
    let mut items: Vec<(Vec<u32>, DVec2)> = Vec::with_capacity(item_islands.len());
    for (item, group) in item_islands.iter().enumerate() {
        progress.report("pack_items", item as u64, item_islands.len() as u64)?;
        let ids: Vec<u32> = group
            .iter()
            .flat_map(|&i| islands.uv_indices(i, mesh, uv))
            .collect();
        let pts: Vec<DVec2> = ids.iter().map(|&k| uv.uvs[k as usize]).collect();
        let mut rot = DVec2::X;
        if opts.orient {
            rot = DVec2::from_angle(min_area_rect_angle(&pts));
        }
        let (lo, hi) = bbox(pts.iter().map(|&p| rot.rotate(p)));
        if opts.rotate_90 && !raster && (hi - lo).y > (hi - lo).x {
            rot = DVec2::Y.rotate(rot); // extra +90°
        }
        let (lo, hi) = bbox(pts.iter().map(|&p| rot.rotate(p)));
        for &k in &ids {
            uv.uvs[k as usize] = rot.rotate(uv.uvs[k as usize]) - lo;
        }
        items.push((ids, hi - lo));
    }
    progress.report(
        "pack_items",
        item_islands.len() as u64,
        item_islands.len() as u64,
    )?;

    // Keep size (or TD) in multi-tile mode, with a texel density, or without
    // `rescale`: islands only shrink when they would not fit.
    let keep = multi || opts.texel_density.is_some() || !opts.rescale;
    let max_scale = if keep { 1.0 } else { f64::INFINITY };
    if raster {
        let params = raster::Params {
            origin,
            size,
            pad,
            cells: opts.quality.cells(opts.resolution),
            xforms: opts.xforms(),
            max_scale,
            max_tiles,
            cols,
            effort: opts.quality.effort(),
            order_mode,
        };
        if let Some(packed) = pack_raster(
            mesh,
            uv,
            islands,
            &item_islands,
            &items,
            &obstacles,
            &params,
            progress,
        )? {
            if opts.pixel_aligned {
                for (ids, _) in &items {
                    let (lo, _) = bbox(ids.iter().map(|&k| uv.uvs[k as usize]));
                    let snap = |x: f64| (x * res - 1e-9).ceil() / res - x;
                    let shift = DVec2::new(snap(lo.x), snap(lo.y));
                    for &k in ids {
                        uv.uvs[k as usize] += shift;
                    }
                }
            }
            result.scale = packed.scale;
            if multi {
                result.tiles_used = packed.tiles_used;
            }
            return Ok(result);
        }
        result.ignored_obstacles = !obstacles.is_empty();
    }
    if let Some(original) = original_uv {
        *uv = original;
        result.failed = true;
        return Ok(result);
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
        return Ok(result);
    }
    // Upper bounds: the largest island along the longest side; the total
    // area in the rectangle's area.
    let side = size.max_element();
    let area_side = if size.x == size.y {
        side
    } else {
        (size.x * size.y).sqrt()
    };
    let mut hi = (side / max_dim).min(max_scale).min(if total > 0.0 {
        area_side / total.sqrt()
    } else {
        f64::INFINITY
    });
    let mut lo = 0.0;
    if shelf(&sized(hi), size.x, size.y, pad).is_some() {
        lo = hi;
    } else {
        for iteration in 0..50 {
            progress.report("pack_shelf_search", iteration, 50)?;
            let mid = 0.5 * (lo + hi);
            if shelf(&sized(mid), size.x, size.y, pad).is_some() {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        progress.report("pack_shelf_search", 50, 50)?;
    }
    // With padding alone too large, fall back to scale 0 positions (degenerate but safe).
    let placement = shelf(&sized(lo), size.x, size.y, pad).unwrap_or_default();
    for (i, at) in placement {
        for &k in &items[i].0 {
            let p = &mut uv.uvs[k as usize];
            *p = origin + at + *p * lo;
        }
    }
    result.scale = lo;
    if multi {
        result.tiles_used = 1;
    }
    Ok(result)
}

/// Tries progressively tighter raster padding, then accepts only layouts
/// whose *geometric* island boundaries meet the requested minimum distance.
/// Returns `failed` and leaves UVs unchanged if no candidate meets it.
/// Islands intentionally stacked in the same group may overlap each other.
pub fn pack_padding_perfect(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    opts: &PackOptions,
    groups: &[Vec<usize>],
) -> PackResult {
    pack_padding_perfect_impl(
        mesh,
        uv,
        islands,
        targets,
        opts,
        groups,
        &Progress::default(),
    )
    .expect("default progress cannot be cancelled")
}

fn pack_padding_perfect_impl(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    opts: &PackOptions,
    groups: &[Vec<usize>],
    progress: &Progress,
) -> Result<PackResult, Cancelled> {
    progress.check()?;
    if targets.is_empty() {
        return Ok(PackResult::default());
    }
    let original = uv.clone();
    let requested = opts
        .padding_uv
        .unwrap_or(opts.padding_px / opts.resolution.max(1) as f64);
    let requested_px = requested * opts.resolution.max(1) as f64;
    let mut group_of = vec![usize::MAX; islands.count()];
    for (g, group) in groups.iter().enumerate() {
        for &i in group {
            group_of[i] = g;
        }
    }
    let relevant = |pair: [usize; 2]| {
        (targets.contains(&pair[0]) || targets.contains(&pair[1]))
            && (group_of[pair[0]] == usize::MAX || group_of[pair[0]] != group_of[pair[1]])
    };
    let mut best: Option<(PackResult, UvLayer)> = None;
    for (attempt, factor) in [0.0, 0.25, 0.5, 0.75, 1.0, 1.25].into_iter().enumerate() {
        progress.report("pack_padding_trials", attempt as u64, 6)?;
        let mut trial = original.clone();
        let trial_opts = PackOptions {
            method: PackMethod::Raster,
            padding_uv: Some(requested * factor),
            ..*opts
        };
        let result = pack_grouped_impl(
            mesh,
            &mut trial,
            islands,
            targets,
            &trial_opts,
            groups,
            progress,
        )?;
        if result.failed || result.scale <= 0.0 {
            continue;
        }
        let quality = checks::check(mesh, &trial, islands, requested_px, opts.resolution);
        progress.check()?;
        if quality.overlaps.island_pairs.iter().copied().any(&relevant)
            || quality
                .padding_violations
                .iter()
                .any(|v| relevant(v.islands))
        {
            continue;
        }
        let better = best.as_ref().is_none_or(|(b, _)| {
            result.scale > b.scale + 1e-12
                || ((result.scale - b.scale).abs() <= 1e-12 && result.tiles_used < b.tiles_used)
        });
        if better {
            best = Some((result, trial));
        }
    }
    progress.report("pack_padding_trials", 6, 6)?;
    if let Some((result, final_uv)) = best {
        *uv = final_uv;
        Ok(result)
    } else {
        Ok(PackResult {
            failed: true,
            ..PackResult::default()
        })
    }
}

/// Cells per side of the raster grid: about 2 px per cell, capped for speed.
fn raster_cells(resolution: u32) -> usize {
    (resolution as usize / 2).clamp(64, 1024)
}

/// Raster packing of normalized islands; `None` if it could not place them
/// (the caller then falls back to shelves).
#[allow(clippy::too_many_arguments)] // Shape data plus progress context.
fn pack_raster(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    item_islands: &[Vec<usize>],
    items: &[(Vec<u32>, DVec2)],
    obstacles: &[[DVec2; 3]],
    params: &raster::Params,
    progress: &Progress,
) -> Result<Option<raster::Packed>, Cancelled> {
    // Local index of each UV vertex within its island.
    let mut local = vec![u32::MAX; uv.uvs.len()];
    let shapes: Vec<raster::Shape> = item_islands
        .iter()
        .zip(items)
        .map(|(group, (ids, _))| {
            for (n, &k) in ids.iter().enumerate() {
                local[k as usize] = n as u32;
            }
            let tris = group
                .iter()
                .flat_map(|&i| islands.faces(i))
                .flat_map(|&f| mesh.fan_triangles(f as usize))
                .map(|t| t.map(|c| local[uv.corner_uv[c] as usize]))
                .collect();
            raster::Shape {
                pts: ids.iter().map(|&k| uv.uvs[k as usize]).collect(),
                tris,
            }
        })
        .collect();
    let Some(packed) = raster::pack_with_progress(&shapes, obstacles, params, progress)? else {
        return Ok(None);
    };
    for ((ids, _), p) in items.iter().zip(&packed.placements) {
        for &k in ids {
            let q = &mut uv.uvs[k as usize];
            *q = p.xform.apply(*q) * packed.scale + p.offset;
        }
    }
    Ok(Some(packed))
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

fn group_area_sums(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    members: &[usize],
) -> Option<(f64, f64)> {
    if members.is_empty() {
        return None;
    }
    let mut unique = members.to_vec();
    unique.sort_unstable();
    unique.dedup();
    let mut surface_uv_area = 0.0;
    let mut mesh_area = 0.0;
    for island in unique {
        if island >= islands.count() {
            return None;
        }
        let (uv_area, area_3d) = island_areas(mesh, uv, islands, island);
        if !uv_area.is_finite() || !area_3d.is_finite() {
            return None;
        }
        // Count each overlapping surface. Occupied UV area is used by the
        // packer's triangle union, whereas density is measured per surface.
        surface_uv_area += uv_area;
        mesh_area += area_3d;
    }
    (surface_uv_area > 0.0 && mesh_area > 0.0).then_some((surface_uv_area, mesh_area))
}

/// Aggregate texel density for a group in pixels per 3D unit. Intentionally
/// stacked copies each contribute their own UV and 3D surface area.
pub fn group_texel_density(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    members: &[usize],
    resolution: u32,
) -> Option<f64> {
    if resolution == 0 {
        return None;
    }
    let (uv_area, mesh_area) = group_area_sums(mesh, uv, islands, members)?;
    let density = texel_density(uv_area, mesh_area, resolution);
    (density.is_finite() && density > 0.0).then_some(density)
}

/// Scales a group as one rigid UV layout about its collective bounding-box
/// center. Returns the single factor applied; invalid or degenerate inputs
/// leave the UV layer untouched.
pub fn set_group_texel_density(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    members: &[usize],
    density: f64,
    resolution: u32,
) -> Option<f64> {
    if !density.is_finite() || density <= 0.0 {
        return None;
    }
    let current = group_texel_density(mesh, uv, islands, members, resolution)?;
    let factor = density / current;
    if !factor.is_finite() || factor <= 0.0 {
        return None;
    }
    if (factor - 1.0).abs() <= f64::EPSILON {
        return Some(factor);
    }
    let mut ids: Vec<u32> = members
        .iter()
        .flat_map(|&island| islands.uv_indices(island, mesh, uv))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return None;
    }
    let (lo, hi) = bbox(ids.iter().map(|&id| uv.uvs[id as usize]));
    let center = (lo + hi) * 0.5;
    for id in ids {
        let point = &mut uv.uvs[id as usize];
        *point = center + (*point - center) * factor;
    }
    Some(factor)
}

/// Sets a target density per packing item. Group members share one factor;
/// other target islands are adjusted individually.
pub fn set_texel_density_grouped(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    groups: &[Vec<usize>],
    density: f64,
    resolution: u32,
) {
    let mut grouped = vec![false; islands.count()];
    for group in groups {
        let _ = set_group_texel_density(mesh, uv, islands, group, density, resolution);
        for &island in group {
            if island < grouped.len() {
                grouped[island] = true;
            }
        }
    }
    for &island in targets {
        if island < grouped.len() && !grouped[island] {
            let _ = set_group_texel_density(mesh, uv, islands, &[island], density, resolution);
        }
    }
}

/// Equalizes aggregate density across selected packing units while retaining
/// the combined per-surface UV area. Each group receives one uniform factor.
pub fn equalize_texel_density_grouped(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    groups: &[Vec<usize>],
    resolution: u32,
) -> Option<f64> {
    if resolution == 0 {
        return None;
    }
    let mut selected = vec![false; islands.count()];
    for &island in targets {
        if island >= selected.len() || selected[island] {
            return None;
        }
        selected[island] = true;
    }
    let mut grouped = vec![false; islands.count()];
    let mut units = Vec::new();
    for group in groups {
        let mut unique_members = group.clone();
        unique_members.sort_unstable();
        unique_members.dedup();
        if group.is_empty()
            || group
                .iter()
                .any(|&island| island >= grouped.len() || !selected[island] || grouped[island])
            || unique_members.len() != group.len()
        {
            return None;
        }
        for &island in group {
            grouped[island] = true;
        }
        units.push(group.clone());
    }
    units.extend(
        targets
            .iter()
            .filter(|&&island| !grouped[island])
            .map(|&island| vec![island]),
    );
    let (mut surface_uv_area, mut mesh_area) = (0.0, 0.0);
    for unit in &units {
        if let Some((uv_area, area_3d)) = group_area_sums(mesh, uv, islands, unit) {
            surface_uv_area += uv_area;
            mesh_area += area_3d;
        }
    }
    if surface_uv_area <= 0.0 || mesh_area <= 0.0 {
        return None;
    }
    let density = texel_density(surface_uv_area, mesh_area, resolution);
    if !density.is_finite() || density <= 0.0 {
        return None;
    }
    for unit in units {
        let _ = set_group_texel_density(mesh, uv, islands, &unit, density, resolution);
    }
    Some(density)
}

/// Applies per-island UV scale multipliers before packing. All entries are
/// validated before mutation; each island keeps its center and orientation.
pub fn apply_island_multipliers(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    multipliers: &[(usize, f64)],
) -> Result<(), String> {
    let mut seen = vec![false; islands.count()];
    for &(island, scale) in multipliers {
        if island >= islands.count() || seen[island] {
            return Err(format!("invalid or duplicate island {island}"));
        }
        if !scale.is_finite() || scale <= 0.0 {
            return Err(format!(
                "island {island} multiplier must be finite and positive"
            ));
        }
        seen[island] = true;
    }
    for &(island, scale) in multipliers {
        let ids = islands.uv_indices(island, mesh, uv);
        let (lo, hi) = bbox(ids.iter().map(|&k| uv.uvs[k as usize]));
        let center = (lo + hi) * 0.5;
        for index in ids {
            let p = &mut uv.uvs[index as usize];
            *p = center + (*p - center) * scale;
        }
    }
    Ok(())
}

/// Gives the selected islands one common texel density while preserving their
/// combined UV area. Islands with zero UV or 3D area are left unchanged.
/// Returns the resulting density, or `None` when no island has usable area.
pub fn equalize_texel_density(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    resolution: u32,
) -> Option<f64> {
    let mut usable = Vec::new();
    let (mut uv_area, mut mesh_area) = (0.0, 0.0);
    for &i in targets {
        let (a_uv, a_3d) = island_areas(mesh, uv, islands, i);
        if a_uv > 0.0 && a_3d > 0.0 {
            uv_area += a_uv;
            mesh_area += a_3d;
            usable.push(i);
        }
    }
    if usable.is_empty() || resolution == 0 {
        return None;
    }
    let density = texel_density(uv_area, mesh_area, resolution);
    set_texel_density(mesh, uv, islands, &usable, density, resolution);
    Some(density)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::analyze;
    use crate::projection::{Projection, project};
    use glam::DVec3;
    use uvcraft_core::CancelToken;

    fn three_islands() -> (Mesh, UvLayer, Islands) {
        let mesh = Mesh::new(
            vec![
                DVec3::new(0.0, 0.0, 0.0),
                DVec3::new(1.0, 0.0, 0.0),
                DVec3::new(0.0, 1.0, 0.0),
                DVec3::new(3.0, 0.0, 0.0),
                DVec3::new(5.0, 0.0, 0.0),
                DVec3::new(3.0, 2.0, 0.0),
                DVec3::new(7.0, 0.0, 0.0),
                DVec3::new(8.0, 0.0, 0.0),
                DVec3::new(7.0, 1.0, 0.0),
            ],
            &[[0, 1, 2], [3, 4, 5], [6, 7, 8]],
        )
        .unwrap();
        let uv = UvLayer {
            name: "UVMap".into(),
            corner_uv: (0..9).collect(),
            uvs: vec![
                DVec2::new(0.0, 0.0),
                DVec2::new(0.1, 0.0),
                DVec2::new(0.0, 0.1),
                DVec2::new(0.25, 0.0),
                DVec2::new(0.75, 0.0),
                DVec2::new(0.25, 0.5),
                DVec2::new(0.8, 0.0),
                DVec2::new(0.9, 0.0),
                DVec2::new(0.8, 0.1),
            ],
        };
        let islands = Islands::compute(&mesh, &uv);
        assert_eq!(islands.count(), 3);
        (mesh, uv, islands)
    }

    #[test]
    fn group_density_scales_every_member_about_one_pivot_and_counts_stacked_surfaces() {
        let (mesh, mut uv, islands) = three_islands();
        let before = uv.uvs.clone();
        let current = group_texel_density(&mesh, &uv, &islands, &[0, 1], 1000).unwrap();
        let factor =
            set_group_texel_density(&mesh, &mut uv, &islands, &[0, 1], current * 1.5, 1000)
                .unwrap();
        assert!((factor - 1.5).abs() < 1e-12);
        let center = (DVec2::ZERO + DVec2::new(0.75, 0.5)) * 0.5;
        for (i, original) in before.iter().take(6).enumerate() {
            assert!(uv.uvs[i].distance(center + (*original - center) * 1.5) < 1e-12);
        }
        assert_eq!(uv.uvs[6..], before[6..]);
        assert!(
            (group_texel_density(&mesh, &uv, &islands, &[0, 1], 1000).unwrap() / current - 1.5)
                .abs()
                < 1e-12
        );

        let (mesh, mut stacked, islands) = three_islands();
        stacked.uvs[3..6].copy_from_slice(&before[0..3]);
        let density = group_texel_density(&mesh, &stacked, &islands, &[0, 1], 1000).unwrap();
        let expected = (0.01_f64 / 2.5).sqrt() * 1000.0;
        assert!((density - expected).abs() < 1e-9);
        set_group_texel_density(&mesh, &mut stacked, &islands, &[0, 1], density * 2.0, 1000)
            .unwrap();
        assert_eq!(&stacked.uvs[0..3], &stacked.uvs[3..6]);
    }

    #[test]
    fn target_density_packing_keeps_group_internal_distances_rigid() {
        let (mesh, mut uv, islands) = three_islands();
        let before = uv.uvs.clone();
        let opts = PackOptions {
            padding_px: 0.0,
            margin_px: 0.0,
            resolution: 1000,
            orient: false,
            rotate_90: false,
            method: PackMethod::Shelf,
            rescale: false,
            around_others: false,
            texel_density: Some(200.0),
            ..PackOptions::default()
        };
        let result = pack_grouped(&mesh, &mut uv, &islands, &[0, 1, 2], &opts, &[vec![0, 1]]);
        assert!(!result.failed);
        let ratio = (uv.uvs[1] - uv.uvs[0]).length() / (before[1] - before[0]).length();
        for (a, b) in [(3, 4), (0, 3), (2, 5)] {
            let original = before[b] - before[a];
            let packed = uv.uvs[b] - uv.uvs[a];
            assert!(
                packed.distance(original * ratio) < 1e-9,
                "group internal layout changed between {a} and {b}"
            );
        }
    }

    #[test]
    fn packing_preserves_intentional_stack_inside_group() {
        let (mesh, mut uv, islands) = three_islands();
        let first = uv.uvs[0..3].to_vec();
        uv.uvs[3..6].copy_from_slice(&first);
        let opts = PackOptions {
            padding_px: 2.0,
            margin_px: 2.0,
            resolution: 256,
            orient: false,
            rotate_90: false,
            method: PackMethod::Raster,
            rescale: false,
            ..PackOptions::default()
        };
        let result = pack_grouped(&mesh, &mut uv, &islands, &[0, 1, 2], &opts, &[vec![0, 1]]);
        assert!(!result.failed);
        assert_eq!(&uv.uvs[0..3], &uv.uvs[3..6]);
    }

    #[test]
    fn average_scale_equalizes_group_aggregate_without_changing_member_ratios() {
        let (mesh, mut uv, islands) = three_islands();
        let before = uv.uvs.clone();
        let density = equalize_texel_density_grouped(
            &mesh,
            &mut uv,
            &islands,
            &[0, 1, 2],
            &[vec![0, 1]],
            1000,
        )
        .unwrap();
        assert!(
            (group_texel_density(&mesh, &uv, &islands, &[0, 1], 1000).unwrap() - density).abs()
                < 1e-9
        );
        assert!(
            (group_texel_density(&mesh, &uv, &islands, &[2], 1000).unwrap() - density).abs() < 1e-9
        );
        let first = (uv.uvs[1] - uv.uvs[0]).length() / (before[1] - before[0]).length();
        let second = (uv.uvs[4] - uv.uvs[3]).length() / (before[4] - before[3]).length();
        let between = (uv.uvs[3] - uv.uvs[0]).length() / (before[3] - before[0]).length();
        assert!((first - second).abs() < 1e-9);
        assert!((first - between).abs() < 1e-9);

        let changed = uv.clone();
        assert!(
            equalize_texel_density_grouped(
                &mesh,
                &mut uv,
                &islands,
                &[0, 1, 2],
                &[vec![0, 1], vec![1, 2]],
                1000
            )
            .is_none()
        );
        assert_eq!(uv, changed);
    }

    #[test]
    fn efficient_strategy_keeps_best_raster_fit() {
        let (mesh, mut source) = cube();
        project(&mesh, &mut source, &[0], Projection::Box);
        let islands = Islands::compute(&mesh, &source);
        let targets: Vec<_> = (0..islands.count()).collect();
        let ordinary = PackOptions {
            method: PackMethod::Raster,
            quality: PackQuality::Low,
            resolution: 128,
            padding_px: 2.0,
            margin_px: 2.0,
            ..Default::default()
        };
        let mut normal_uv = source.clone();
        let baseline = pack(&mesh, &mut normal_uv, &islands, &targets, &ordinary);
        let mut efficient_uv = source;
        let efficient = pack(
            &mesh,
            &mut efficient_uv,
            &islands,
            &targets,
            &PackOptions {
                method: PackMethod::Efficient,
                ..ordinary
            },
        );
        assert!(!efficient.failed);
        assert!(efficient.scale + 1e-12 >= baseline.scale);
        let check = checks::check(&mesh, &efficient_uv, &islands, 2.0, 128);
        assert!(check.overlaps.island_pairs.is_empty());
        assert!(check.padding_violations.is_empty());
    }

    #[test]
    fn cancelled_pack_keeps_uvs() {
        let (mesh, mut uv) = cube();
        let islands = Islands::compute(&mesh, &uv);
        let before = uv.clone();
        let token = CancelToken::default();
        let captured = token.clone();
        let progress = Progress::new(token, move |event| {
            if event.completed == 1 {
                captured.cancel();
            }
        });
        assert_eq!(
            pack_grouped_with_progress(
                &mesh,
                &mut uv,
                &islands,
                &(0..islands.count()).collect::<Vec<_>>(),
                &PackOptions::default(),
                &[],
                &progress
            ),
            Err(Cancelled)
        );
        assert_eq!(uv, before);
    }

    #[test]
    fn cancelling_during_raster_scale_search_rolls_back() {
        let (mesh, mut uv) = cube();
        project(&mesh, &mut uv, &[0], Projection::Box);
        let islands = Islands::compute(&mesh, &uv);
        let before = uv.clone();
        let token = CancelToken::default();
        let captured = token.clone();
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = seen.clone();
        let progress = Progress::new(token, move |event| {
            if event.phase == "pack_raster_scale"
                && counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == 2
            {
                captured.cancel();
            }
        });
        let targets: Vec<usize> = (0..islands.count()).collect();
        assert_eq!(
            pack_grouped_with_progress(
                &mesh,
                &mut uv,
                &islands,
                &targets,
                &PackOptions::default(),
                &[],
                &progress
            ),
            Err(Cancelled)
        );
        assert!(seen.load(std::sync::atomic::Ordering::SeqCst) >= 2);
        assert_eq!(uv, before);
    }

    #[test]
    fn cancelling_during_shelf_search_rolls_back() {
        let (mesh, mut uv) = cube();
        project(&mesh, &mut uv, &[0], Projection::Box);
        let islands = Islands::compute(&mesh, &uv);
        let before = uv.clone();
        let token = CancelToken::default();
        let captured = token.clone();
        let progress = Progress::new(token, move |event| {
            if event.phase == "pack_shelf_search" && event.completed == 1 {
                captured.cancel();
            }
        });
        let targets: Vec<usize> = (0..islands.count()).collect();
        let opts = PackOptions {
            method: PackMethod::Shelf,
            ..PackOptions::default()
        };
        assert_eq!(
            pack_grouped_with_progress(&mesh, &mut uv, &islands, &targets, &opts, &[], &progress),
            Err(Cancelled)
        );
        assert_eq!(uv, before);
    }

    #[test]
    fn cancelling_padding_perfect_between_trials_rolls_back() {
        let (mesh, mut uv) = cube();
        project(&mesh, &mut uv, &[0], Projection::Box);
        let islands = Islands::compute(&mesh, &uv);
        let before = uv.clone();
        let token = CancelToken::default();
        let captured = token.clone();
        let progress = Progress::new(token, move |event| {
            if event.phase == "pack_padding_trials" && event.completed == 1 {
                captured.cancel();
            }
        });
        let targets: Vec<usize> = (0..islands.count()).collect();
        assert_eq!(
            pack_padding_perfect_with_progress(
                &mesh,
                &mut uv,
                &islands,
                &targets,
                &PackOptions::default(),
                &[],
                &progress
            ),
            Err(Cancelled)
        );
        assert_eq!(uv, before);
    }

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
    fn pixel_aligned_pack_keeps_padding_and_region() {
        let (m, mut uv) = cube();
        project(&m, &mut uv, &[0], Projection::Box);
        let isl = Islands::compute(&m, &uv);
        let region = [0.03, 0.05, 0.92, 0.94];
        let opts = PackOptions {
            resolution: 128,
            padding_px: 4.0,
            margin_px: 3.0,
            region: Some(region),
            pixel_aligned: true,
            ..Default::default()
        };
        let result = pack(&m, &mut uv, &isl, &(0..6).collect::<Vec<_>>(), &opts);
        assert!(!result.failed);
        for i in 0..isl.count() {
            let (lo, hi) = bbox(
                isl.uv_indices(i, &m, &uv)
                    .iter()
                    .map(|&k| uv.uvs[k as usize]),
            );
            for x in [lo.x, lo.y] {
                assert!((x * 128.0 - (x * 128.0).round()).abs() < 1e-8);
            }
            assert!(lo.x >= region[0] + 3.0 / 128.0 - 1e-9);
            assert!(lo.y >= region[1] + 3.0 / 128.0 - 1e-9);
            assert!(hi.x <= region[2] - 3.0 / 128.0 + 1e-9);
            assert!(hi.y <= region[3] - 3.0 / 128.0 + 1e-9);
        }
        let report = crate::analysis::analyze_with_padding(&m, &uv, &isl, 128, 4.0);
        assert_eq!(report.overlapping_faces, 0);
        assert!(
            report.padding_violations.is_empty(),
            "{:?}",
            report.padding_violations
        );
    }

    #[test]
    fn pixel_aligned_no_fit_restores_uvs() {
        let (m, mut uv) = cube();
        project(&m, &mut uv, &[0], Projection::Box);
        let isl = Islands::compute(&m, &uv);
        let original = uv.clone();
        let opts = PackOptions {
            resolution: 128,
            region: Some([0.0, 0.0, 0.001, 0.001]),
            pixel_aligned: true,
            ..Default::default()
        };
        let result = pack(&m, &mut uv, &isl, &[0], &opts);
        assert!(result.failed);
        assert_eq!(uv, original);
    }

    #[test]
    fn pixel_aligned_pack_respects_fixed_islands() {
        let (m, mut uv) = cube();
        project(&m, &mut uv, &[0], Projection::Box);
        let isl = Islands::compute(&m, &uv);
        let opts = PackOptions {
            resolution: 128,
            padding_px: 4.0,
            margin_px: 3.0,
            ..Default::default()
        };
        pack(&m, &mut uv, &isl, &(0..6).collect::<Vec<_>>(), &opts);
        let fixed: Vec<Vec<DVec2>> = (1..6)
            .map(|i| {
                isl.uv_indices(i, &m, &uv)
                    .iter()
                    .map(|&k| uv.uvs[k as usize])
                    .collect()
            })
            .collect();
        let pixel = PackOptions {
            pixel_aligned: true,
            ..opts
        };
        let result = pack(&m, &mut uv, &isl, &[0], &pixel);
        assert!(!result.failed);
        for (i, points) in (1..6).zip(fixed) {
            let current: Vec<DVec2> = isl
                .uv_indices(i, &m, &uv)
                .iter()
                .map(|&k| uv.uvs[k as usize])
                .collect();
            assert_eq!(current, points);
        }
        let report = crate::analysis::analyze_with_padding(&m, &uv, &isl, 128, 4.0);
        assert_eq!(report.overlapping_faces, 0);
        assert!(
            report.padding_violations.is_empty(),
            "{:?}",
            report.padding_violations
        );
    }

    #[test]
    fn padding_perfect_uses_exact_check_and_never_loses_coverage() {
        let (m, mut uv) = cube();
        project(&m, &mut uv, &[0], Projection::Box);
        let islands = Islands::compute(&m, &uv);
        let targets: Vec<usize> = (0..islands.count()).collect();
        let opts = PackOptions {
            resolution: 128,
            padding_px: 4.0,
            margin_px: 2.0,
            ..Default::default()
        };
        let mut baseline = uv.clone();
        let ordinary = pack(&m, &mut baseline, &islands, &targets, &opts);
        let perfect = pack_padding_perfect(&m, &mut uv, &islands, &targets, &opts, &[]);
        assert!(!perfect.failed);
        assert!(perfect.scale + 1e-12 >= ordinary.scale);
        let checked = checks::check(&m, &uv, &islands, 4.0, 128);
        assert!(checked.overlaps.island_pairs.is_empty());
        assert!(checked.padding_violations.is_empty());
    }

    #[test]
    fn padding_perfect_no_fit_keeps_original_uvs() {
        let (m, mut uv) = cube();
        project(&m, &mut uv, &[0], Projection::Box);
        let islands = Islands::compute(&m, &uv);
        let original = uv.clone();
        let opts = PackOptions {
            resolution: 128,
            region: Some([0.0, 0.0, 0.001, 0.001]),
            ..Default::default()
        };
        let result = pack_padding_perfect(&m, &mut uv, &islands, &[0], &opts, &[]);
        assert!(result.failed);
        assert_eq!(uv, original);
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

    /// `n` L-shaped planar islands (3 unit quads each), UVs = XY.
    fn l_shapes(n: usize) -> (Mesh, UvLayer) {
        let mut pos = Vec::new();
        let mut faces = Vec::new();
        for k in 0..n {
            let z = k as f64;
            let b = pos.len() as u32;
            // 0 1 2 / 3 4 5 / 6 7 8 grid vertices, minus the top-right quad.
            for y in 0..3 {
                for x in 0..3 {
                    pos.push(DVec3::new(x as f64, y as f64, z));
                }
            }
            for (x, y) in [(0, 0), (1, 0), (0, 1)] {
                let v = |dx: u32, dy: u32| b + (y + dy) * 3 + x + dx;
                faces.push([v(0, 0), v(1, 0), v(1, 1), v(0, 1)]);
            }
        }
        let m = Mesh::new(pos, &faces).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..m.corner_count())
                .map(|c| m.corner_vertex(c) as u32)
                .collect(),
            uvs: m.positions().iter().map(|p| p.truncate()).collect(),
        };
        (m, uv)
    }

    fn seg_dist(a: DVec2, b: DVec2, c: DVec2, d: DVec2) -> f64 {
        let cross = |o: DVec2, p: DVec2, q: DVec2| (p - o).perp_dot(q - o);
        let (d1, d2) = (cross(a, b, c), cross(a, b, d));
        let (d3, d4) = (cross(c, d, a), cross(c, d, b));
        if d1 * d2 < 0.0 && d3 * d4 < 0.0 {
            return 0.0;
        }
        let pd = |p: DVec2, s0: DVec2, s1: DVec2| {
            let e = s1 - s0;
            let t = ((p - s0).dot(e) / e.length_squared().max(1e-300)).clamp(0.0, 1.0);
            p.distance(s0 + e * t)
        };
        pd(a, c, d)
            .min(pd(b, c, d))
            .min(pd(c, a, b))
            .min(pd(d, a, b))
    }

    /// Smallest gap between two islands' triangles (0 if they overlap).
    fn gap(m: &Mesh, uv: &UvLayer, isl: &Islands, i: usize, j: usize) -> f64 {
        let tris = |k: usize| -> Vec<[DVec2; 3]> {
            isl.faces(k)
                .iter()
                .flat_map(|&f| m.fan_triangles(f as usize))
                .map(|t| t.map(|c| uv.uv_at_corner(c)))
                .collect()
        };
        let inside = |p: DVec2, t: &[DVec2; 3]| {
            let s = [0, 1, 2].map(|k| (t[(k + 1) % 3] - t[k]).perp_dot(p - t[k]));
            s.iter().all(|&v| v >= 0.0) || s.iter().all(|&v| v <= 0.0)
        };
        let (a, b) = (tris(i), tris(j));
        let mut best = f64::INFINITY;
        for s in &a {
            for t in &b {
                if inside(s[0], t) || inside(t[0], s) {
                    return 0.0;
                }
                for x in 0..3 {
                    for y in 0..3 {
                        best = best.min(seg_dist(s[x], s[(x + 1) % 3], t[y], t[(y + 1) % 3]));
                    }
                }
            }
        }
        best
    }

    #[test]
    fn raster_packs_concave_islands_tightly_and_safely() {
        let (m, uv0) = l_shapes(12);
        let isl = Islands::compute(&m, &uv0);
        assert_eq!(isl.count(), 12);
        let all: Vec<usize> = (0..12).collect();
        let coverage = |method: PackMethod| {
            let mut uv = uv0.clone();
            let opts = PackOptions {
                resolution: 512,
                method,
                ..Default::default()
            };
            pack(&m, &mut uv, &isl, &all, &opts);
            (analyze(&m, &uv, &isl, 512).uv_area, uv)
        };
        let (shelf, _) = coverage(PackMethod::Shelf);
        let (raster, uv) = coverage(PackMethod::Raster);
        assert!(raster > shelf * 1.15, "raster {raster} vs shelf {shelf}");

        let (pad, margin) = (4.0 / 512.0, 4.0 / 512.0);
        for p in uv.corner_uv.iter().map(|&k| uv.uvs[k as usize]) {
            assert!(
                p.min_element() >= margin - 1e-9 && p.max_element() <= 1.0 - margin + 1e-9,
                "{p}"
            );
        }
        for i in 0..12 {
            for j in i + 1..12 {
                let g = gap(&m, &uv, &isl, i, j);
                assert!(g >= pad - 1e-9, "islands {i} and {j} are {g} apart");
            }
        }
        let r = analyze(&m, &uv, &isl, 512);
        assert!(
            (r.max_area_distortion - 1.0).abs() < 1e-9,
            "uniform scale, rigid moves"
        );
        assert_eq!(r.flipped_islands, 0, "rotations only, no mirroring");
    }

    #[test]
    fn packs_a_selection_around_the_other_islands() {
        let (m, mut uv) = l_shapes(10);
        let isl = Islands::compute(&m, &uv);
        let all: Vec<usize> = (0..10).collect();
        let opts = PackOptions {
            resolution: 512,
            ..Default::default()
        };
        pack(&m, &mut uv, &isl, &all, &opts);
        let before = uv.clone();

        // Repack islands 0 and 1 at their current size around the other 8.
        let sub = PackOptions {
            rescale: false,
            ..opts
        };
        let r = pack(&m, &mut uv, &isl, &[0, 1], &sub);
        assert_eq!(r.obstacles, 8);
        assert!(!r.ignored_obstacles);
        assert!(r.scale <= 1.0 + 1e-12);
        // Their old spots are free again: they fit at (nearly) full size;
        // conservative rasterization may cost a cell.
        assert!(r.scale > 0.95, "{}", r.scale);
        for i in 2..10 {
            for k in isl.uv_indices(i, &m, &uv) {
                assert_eq!(
                    uv.uvs[k as usize], before.uvs[k as usize],
                    "island {i} moved"
                );
            }
        }
        let pad = 4.0 / 512.0;
        for i in 0..2 {
            for j in 0..10 {
                if i != j {
                    let g = gap(&m, &uv, &isl, i, j);
                    assert!(g >= pad - 1e-9, "islands {i} and {j} are {g} apart");
                }
            }
        }
    }

    /// Asserts that the islands `ids` are pairwise at least `pad` apart.
    fn assert_apart(m: &Mesh, uv: &UvLayer, isl: &Islands, ids: &[usize], pad: f64) {
        for (a, &i) in ids.iter().enumerate() {
            for &j in &ids[a + 1..] {
                let g = gap(m, uv, isl, i, j);
                assert!(g >= pad - 1e-9, "islands {i} and {j} are {g} apart");
            }
        }
    }

    fn island_bbox(m: &Mesh, uv: &UvLayer, isl: &Islands, i: usize) -> (DVec2, DVec2) {
        bbox(isl.uv_indices(i, m, uv).iter().map(|&k| uv.uvs[k as usize]))
    }

    /// `l_shapes` scaled in UV.
    fn small_l_shapes(n: usize, s: f64) -> (Mesh, UvLayer) {
        let (m, mut uv) = l_shapes(n);
        for p in &mut uv.uvs {
            *p *= s;
        }
        (m, uv)
    }

    #[test]
    fn packs_into_a_region() {
        for method in [PackMethod::Raster, PackMethod::Shelf] {
            let (m, mut uv) = l_shapes(8);
            let isl = Islands::compute(&m, &uv);
            let all: Vec<usize> = (0..8).collect();
            let opts = PackOptions {
                resolution: 512,
                method,
                region: Some([0.25, 0.5, 1.0, 0.75]),
                ..Default::default()
            };
            assert!(opts.validate().is_ok());
            let r = pack(&m, &mut uv, &isl, &all, &opts);
            assert!(r.scale > 0.0);
            let e = 4.0 / 512.0 - 1e-9;
            for p in uv.corner_uv.iter().map(|&k| uv.uvs[k as usize]) {
                assert!(
                    p.x >= 0.25 + e && p.x <= 1.0 - e && p.y >= 0.5 + e && p.y <= 0.75 - e,
                    "{method:?}: {p} outside the region"
                );
            }
            assert_apart(&m, &uv, &isl, &all, 4.0 / 512.0);
            // A wide region is used along its whole width.
            let hi = uv
                .corner_uv
                .iter()
                .fold(f64::NEG_INFINITY, |a, &k| a.max(uv.uvs[k as usize].x));
            assert!(hi > 0.8, "{method:?}: {hi}");
        }
        let bad = PackOptions {
            region: Some([0.5, 0.0, 0.5, 1.0]),
            ..Default::default()
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn padding_and_margin_in_uv_units() {
        let (m, mut uv) = l_shapes(6);
        let isl = Islands::compute(&m, &uv);
        let all: Vec<usize> = (0..6).collect();
        let opts = PackOptions {
            resolution: 512,
            padding_uv: Some(0.05),
            margin_uv: Some(0.1),
            ..Default::default()
        };
        pack(&m, &mut uv, &isl, &all, &opts);
        for p in uv.corner_uv.iter().map(|&k| uv.uvs[k as usize]) {
            assert!(
                p.min_element() >= 0.1 - 1e-9 && p.max_element() <= 0.9 + 1e-9,
                "{p}"
            );
        }
        assert_apart(&m, &uv, &isl, &all, 0.05);
    }

    #[test]
    fn multi_tile_overflows_row_major_at_fixed_size() {
        let (m, uv0) = small_l_shapes(5, 0.3);
        let isl = Islands::compute(&m, &uv0);
        let all: Vec<usize> = (0..5).collect();
        let opts = PackOptions {
            resolution: 512,
            tiles_u: Some(2),
            ..Default::default()
        };
        assert!(opts.validate().is_ok());
        let mut uv = uv0.clone();
        let r = pack(&m, &mut uv, &isl, &all, &opts);
        assert_eq!(r.scale, 1.0, "islands keep their size");
        assert!(r.tiles_used >= 3, "{}", r.tiles_used);
        let margin = 4.0 / 512.0 - 1e-9;
        let mut tiles = std::collections::BTreeSet::new();
        for i in 0..5 {
            let (lo, hi) = island_bbox(&m, &uv, &isl, i);
            let t = lo.floor();
            assert!(t.x >= 0.0 && t.x < 2.0 && t.y >= 0.0, "tile {t}");
            assert!(
                (lo - t).min_element() >= margin && (t + 1.0 - hi).min_element() >= margin,
                "island {i} crosses its tile: {lo} {hi}"
            );
            tiles.insert(udim((t.y as usize) * 2 + t.x as usize, 2));
        }
        // Row-major with 2 columns: tiles 1001, 1002, then 1011.
        assert!(tiles.contains(&1001) && tiles.contains(&1002) && tiles.contains(&1011));
        let r2 = analyze(&m, &uv, &isl, 512);
        assert!((r2.max_area_distortion - 1.0).abs() < 1e-9);
        assert_apart(&m, &uv, &isl, &all, 4.0 / 512.0);

        // Deterministic.
        let mut again = uv0.clone();
        pack(&m, &mut again, &isl, &all, &opts);
        assert_eq!(again.uvs, uv.uvs);

        // Capped at one tile: they shrink instead.
        let mut uv = uv0.clone();
        let one = PackOptions {
            tiles: Some(1),
            tiles_u: None,
            ..opts
        };
        let r = pack(&m, &mut uv, &isl, &all, &one);
        assert_eq!(r.tiles_used, 1);
        assert!(r.scale < 0.9, "{}", r.scale);
        for p in uv.corner_uv.iter().map(|&k| uv.uvs[k as usize]) {
            assert!(p.min_element() >= 0.0 && p.max_element() <= 1.0, "{p}");
        }
    }

    #[test]
    fn udim_numbers() {
        assert_eq!(udim(0, 10), 1001);
        assert_eq!(udim(9, 10), 1010);
        assert_eq!(udim(10, 10), 1011);
        assert_eq!(udim(2, 2), 1011);
        assert_eq!(udim(3, 2), 1012);
    }

    /// An 8×8 obstacle grid over the tile with a mirrored-L hole, plus an
    /// L-shaped island that fits the hole only when mirrored.
    fn mirror_hole() -> (Mesh, UvLayer) {
        let mut pos = Vec::new();
        let mut faces: Vec<[u32; 4]> = Vec::new();
        for y in 0..9 {
            for x in 0..9 {
                pos.push(DVec3::new(x as f64 * 0.125, y as f64 * 0.125, 0.0));
            }
        }
        let hole = [(3, 3), (4, 3), (4, 4)];
        for y in 0..8u32 {
            for x in 0..8u32 {
                if !hole.contains(&(x, y)) {
                    let v = |dx: u32, dy: u32| (y + dy) * 9 + x + dx;
                    faces.push([v(0, 0), v(1, 0), v(1, 1), v(0, 1)]);
                }
            }
        }
        let b = pos.len() as u32;
        for y in 0..3 {
            for x in 0..3 {
                pos.push(DVec3::new(2.0 + x as f64 * 0.1, y as f64 * 0.1, 1.0));
            }
        }
        for (x, y) in [(0, 0), (1, 0), (0, 1)] {
            let v = |dx: u32, dy: u32| b + (y + dy) * 3 + x + dx;
            faces.push([v(0, 0), v(1, 0), v(1, 1), v(0, 1)]);
        }
        let m = Mesh::new(pos, &faces).unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..m.corner_count())
                .map(|c| m.corner_vertex(c) as u32)
                .collect(),
            uvs: m.positions().iter().map(|p| p.truncate()).collect(),
        };
        (m, uv)
    }

    #[test]
    fn mirroring_fits_a_mirrored_hole() {
        let (m, uv0) = mirror_hole();
        let isl = Islands::compute(&m, &uv0);
        assert_eq!(isl.count(), 2);
        let target = (0..2).find(|&i| isl.faces(i).len() == 3).unwrap();
        let run = |mirror: bool| {
            let mut uv = uv0.clone();
            let opts = PackOptions {
                orient: false,
                rotation_step_deg: Some(360.0),
                mirror,
                rescale: false,
                padding_uv: Some(0.005),
                margin_uv: Some(0.0),
                ..Default::default()
            };
            let r = pack(&m, &mut uv, &isl, &[target], &opts);
            assert!(!r.ignored_obstacles);
            assert_apart(&m, &uv, &isl, &[0, 1], 0.005);
            (r.scale, analyze(&m, &uv, &isl, 2048).flipped_islands)
        };
        let (plain, flipped) = run(false);
        assert!(plain < 0.8, "{plain}");
        assert_eq!(flipped, 0);
        let (mirrored, flipped) = run(true);
        assert_eq!(mirrored, 1.0);
        assert_eq!(flipped, 1, "the island was mirrored");
    }

    #[test]
    fn rotation_steps() {
        let x = |step: Option<f64>, rotate_90: bool, mirror: bool| {
            PackOptions {
                rotation_step_deg: step,
                rotate_90,
                mirror,
                ..Default::default()
            }
            .xforms()
        };
        let quarters: Vec<raster::Xform> = (0..4).map(raster::Xform::quarter).collect();
        assert_eq!(x(None, true, false), quarters);
        assert_eq!(x(Some(90.0), false, false), quarters);
        assert_eq!(x(None, false, false), vec![raster::Xform::IDENTITY]);
        assert_eq!(x(Some(360.0), true, false), vec![raster::Xform::IDENTITY]);
        let x45 = x(Some(45.0), true, true);
        assert_eq!(x45.len(), 16);
        assert_eq!(x45[3].quarter, 1);
        assert!((x45[3].angle - 45f64.to_radians()).abs() < 1e-12);
        assert!(x45[..8].iter().all(|x| !x.mirror) && x45[8..].iter().all(|x| x.mirror));

        // Packing with 30° steps stays rigid and overlap-free.
        let (m, mut uv) = l_shapes(6);
        let isl = Islands::compute(&m, &uv);
        let all: Vec<usize> = (0..6).collect();
        let opts = PackOptions {
            resolution: 512,
            rotation_step_deg: Some(30.0),
            ..Default::default()
        };
        pack(&m, &mut uv, &isl, &all, &opts);
        let r = analyze(&m, &uv, &isl, 512);
        assert!((r.max_area_distortion - 1.0).abs() < 1e-9);
        assert_eq!(r.flipped_islands, 0);
        assert_apart(&m, &uv, &isl, &all, 4.0 / 512.0);
        for p in uv.corner_uv.iter().map(|&k| uv.uvs[k as usize]) {
            assert!(p.min_element() >= 0.0 && p.max_element() <= 1.0, "{p}");
        }
    }

    #[test]
    fn quality_levels() {
        let (m, uv0) = l_shapes(10);
        let isl = Islands::compute(&m, &uv0);
        let all: Vec<usize> = (0..10).collect();
        let run = |quality: PackQuality| {
            let mut uv = uv0.clone();
            let opts = PackOptions {
                resolution: 512,
                quality,
                ..Default::default()
            };
            let r = pack(&m, &mut uv, &isl, &all, &opts);
            assert_apart(&m, &uv, &isl, &all, 4.0 / 512.0);
            (r.scale, uv)
        };
        let (low, _) = run(PackQuality::Low);
        let (normal, uv) = run(PackQuality::Normal);
        let (high, _) = run(PackQuality::High);
        assert!(low > 0.0 && high >= normal * 0.99 && normal >= low * 0.99);
        // `normal` is the default.
        let mut d = uv0.clone();
        pack(
            &m,
            &mut d,
            &isl,
            &all,
            &PackOptions {
                resolution: 512,
                ..Default::default()
            },
        );
        assert_eq!(d.uvs, uv.uvs);
    }

    #[test]
    fn keeps_texel_density() {
        let (m, mut uv) = cube();
        project(&m, &mut uv, &[0], Projection::Box);
        let isl = Islands::compute(&m, &uv);
        let all: Vec<usize> = (0..6).collect();
        let opts = PackOptions {
            resolution: 1024,
            texel_density: Some(100.0),
            ..Default::default()
        };
        let r = pack(&m, &mut uv, &isl, &all, &opts);
        assert_eq!(r.scale, 1.0);
        let a = analyze(&m, &uv, &isl, 1024);
        assert!(
            a.islands
                .iter()
                .all(|i| (i.texel_density - 100.0).abs() < 1e-6)
        );
        assert_apart(&m, &uv, &isl, &all, 4.0 / 1024.0);
    }

    /// A 3×3 ring of quads (hole in the middle) plus a small separate quad.
    #[test]
    fn packs_inside_holes() {
        let mut pos = Vec::new();
        let mut faces: Vec<[u32; 4]> = Vec::new();
        for y in 0..4 {
            for x in 0..4 {
                pos.push(DVec3::new(x as f64, y as f64, 0.0));
            }
        }
        for y in 0..3u32 {
            for x in 0..3u32 {
                if (x, y) != (1, 1) {
                    let v = |dx: u32, dy: u32| (y + dy) * 4 + x + dx;
                    faces.push([v(0, 0), v(1, 0), v(1, 1), v(0, 1)]);
                }
            }
        }
        for p in [(5.0, 0.0), (5.3, 0.0), (5.3, 0.3), (5.0, 0.3)] {
            pos.push(DVec3::new(p.0, p.1, 1.0));
        }
        faces.push([16, 17, 18, 19]);
        let m = Mesh::new(pos, &faces).unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..m.corner_count())
                .map(|c| m.corner_vertex(c) as u32)
                .collect(),
            uvs: m.positions().iter().map(|p| p.truncate()).collect(),
        };
        let isl = Islands::compute(&m, &uv);
        let ring = (0..2).find(|&i| isl.faces(i).len() == 8).unwrap();
        let quad = 1 - ring;
        let opts = PackOptions {
            resolution: 512,
            ..Default::default()
        };
        pack(&m, &mut uv, &isl, &[0, 1], &opts);
        let (rlo, rhi) = island_bbox(&m, &uv, &isl, ring);
        let (qlo, qhi) = island_bbox(&m, &uv, &isl, quad);
        let third = (rhi - rlo) / 3.0;
        assert!(rhi.x - rlo.x > 0.9, "the ring fills the tile");
        assert!(
            (qlo - (rlo + third)).min_element() > 0.0
                && (rlo + 2.0 * third - qhi).min_element() > 0.0,
            "the quad sits in the ring's hole: {qlo} {qhi} in {rlo} {rhi}"
        );
        assert_apart(&m, &uv, &isl, &[0, 1], 4.0 / 512.0);
    }

    #[test]
    fn island_multipliers_are_atomic_and_independent() {
        let m = Mesh::new(
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
        let mut uv = UvLayer {
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
        let isl = Islands::compute(&m, &uv);
        let before = uv.clone();
        assert!(apply_island_multipliers(&m, &mut uv, &isl, &[(0, 2.0), (1, -1.0)]).is_err());
        assert_eq!(uv, before);
        apply_island_multipliers(&m, &mut uv, &isl, &[(0, 2.0)]).unwrap();
        let (a0, _) = island_areas(&m, &uv, &isl, 0);
        let (a1, _) = island_areas(&m, &uv, &isl, 1);
        assert!((a0 - 2.0).abs() < 1e-9);
        assert!((a1 - 0.5).abs() < 1e-9);
    }
}
