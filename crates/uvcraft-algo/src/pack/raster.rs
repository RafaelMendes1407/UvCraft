//! Raster (bitmask) packing.
//!
//! The available rectangle is a grid of square cells. Each island is
//! conservatively rasterized (every cell its triangles touch is set), so
//! islands whose masks do not overlap never overlap in UV space. The grid
//! stores placed islands dilated by the padding (in cells); a new island's
//! undilated mask must not hit any set cell. Empty cells inside an island
//! (holes, concavities) stay free, so smaller islands can go there. Islands go
//! largest first, each at the position (and transform: rotation, optional
//! mirroring) with the lowest top edge, scanning bottom-left with word-level
//! bit tricks. The scale is the largest one where every island fits, found by
//! a coarse descent followed by a finer search.
//!
//! Several tiles (UDIM) can be used: an island that does not fit in the open
//! tiles goes to the next one (first fit), row-major with a column limit.
//! On native platforms, hard placement searches use wgpu compute when a
//! physical adapter is available. CPU remains the fallback; set
//! `UVCRAFT_GPU_PACK=0` to force it. Browser WebGPU needs an async packing API.

use std::collections::BTreeMap;

use glam::DVec2;
use rayon::prelude::*;
use uvcraft_core::{Cancelled, Progress};

mod gpu;

/// An island in its packing frame (already oriented; any offset).
pub struct Shape {
    pub pts: Vec<DVec2>,
    pub tris: Vec<[u32; 3]>,
}

/// A rigid (or mirrored) transform tried for a shape:
/// optional mirroring in U, then a rotation by `angle` radians, then `quarter`
/// quarter turns counter-clockwise (exact).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Xform {
    pub quarter: u8,
    pub angle: f64,
    pub mirror: bool,
}

impl Xform {
    pub const IDENTITY: Xform = Xform {
        quarter: 0,
        angle: 0.0,
        mirror: false,
    };

    /// `k` quarter turns.
    pub fn quarter(k: u8) -> Xform {
        Xform {
            quarter: k,
            ..Xform::IDENTITY
        }
    }

    pub fn apply(&self, p: DVec2) -> DVec2 {
        let p = if self.mirror {
            DVec2::new(-p.x, p.y)
        } else {
            p
        };
        let p = if self.angle != 0.0 {
            DVec2::from_angle(self.angle).rotate(p)
        } else {
            p
        };
        rotate(p, self.quarter)
    }
}

/// Final UV of a point `p` of the shape: `xform.apply(p) * scale + offset`.
#[derive(Debug, Clone, Copy)]
pub struct Placement {
    pub xform: Xform,
    pub offset: DVec2,
    /// Tile index (row-major, see [`Params::cols`]).
    pub tile: usize,
}

/// Rotation by `k` quarter turns counter-clockwise.
pub fn rotate(p: DVec2, k: u8) -> DVec2 {
    match k % 4 {
        0 => p,
        1 => DVec2::new(-p.y, p.x),
        2 => -p,
        _ => DVec2::new(p.y, -p.x),
    }
}

fn bbox(points: impl Iterator<Item = DVec2>) -> (DVec2, DVec2) {
    points.fold((DVec2::INFINITY, DVec2::NEG_INFINITY), |(lo, hi), p| {
        (lo.min(p), hi.max(p))
    })
}

const EPS: f64 = 1e-9;

/// Separating-axis test of a triangle against the unit cell at (cx, cy).
fn tri_touches_cell(t: &[DVec2; 3], cx: usize, cy: usize) -> bool {
    let center = DVec2::new(cx as f64 + 0.5, cy as f64 + 0.5);
    for i in 0..3 {
        let e = t[(i + 1) % 3] - t[i];
        let n = DVec2::new(-e.y, e.x);
        if n == DVec2::ZERO {
            continue;
        }
        let d = t.map(|p| p.dot(n));
        let (lo, hi) = (d[0].min(d[1]).min(d[2]), d[0].max(d[1]).max(d[2]));
        let r = (0.5 + EPS) * (n.x.abs() + n.y.abs());
        let c = center.dot(n);
        if hi < c - r || lo > c + r {
            return false;
        }
    }
    true
}

/// Bitmask of one shape at one transform and scale.
struct Mask {
    w: usize,
    h: usize,
    /// Words per row of `bits`.
    mw: usize,
    bits: Vec<u64>,
    /// First set column of row 0 (the row is never empty).
    fx: usize,
    /// Mask dilated by `pad` cells, `(w + 2 pad) × (h + 2 pad)`.
    dilated: Vec<bool>,
    /// Shape bbox minimum after the transform, in UV (unscaled).
    lo: DVec2,
    xform: Xform,
}

fn dilate(cells: &[bool], w: usize, h: usize, pad: usize) -> Vec<bool> {
    let (dw, dh) = (w + 2 * pad, h + 2 * pad);
    // Horizontal pass into a (dw × h) buffer, then vertical into (dw × dh).
    let mut horiz = vec![false; dw * h];
    for y in 0..h {
        for x in 0..w {
            if cells[y * w + x] {
                horiz[y * dw + x..=y * dw + x + 2 * pad].fill(true);
            }
        }
    }
    let mut out = vec![false; dw * dh];
    for y in 0..h {
        for x in 0..dw {
            if horiz[y * dw + x] {
                for yy in y..=y + 2 * pad {
                    out[yy * dw + x] = true;
                }
            }
        }
    }
    out
}

impl Mask {
    /// `None` when the shape does not fit in a `gx × gy` grid.
    fn new(
        shape: &Shape,
        xform: Xform,
        scale: f64,
        cell: f64,
        pad: usize,
        (gx, gy): (usize, usize),
    ) -> Option<Mask> {
        let pts: Vec<DVec2> = shape.pts.iter().map(|&p| xform.apply(p)).collect();
        let (lo, hi) = bbox(pts.iter().copied());
        let k = scale / cell;
        let ext = (hi - lo) * k;
        let w = ((ext.x - EPS).ceil().max(1.0)) as usize;
        let h = ((ext.y - EPS).ceil().max(1.0)) as usize;
        if w > gx || h > gy {
            return None;
        }
        let q: Vec<DVec2> = pts.iter().map(|&p| (p - lo) * k).collect();
        let mut cells = vec![false; w * h];
        let clamp = |v: f64, n: usize| (v.floor().max(0.0) as usize).min(n - 1);
        for t in &shape.tris {
            let t = t.map(|i| q[i as usize]);
            let (tlo, thi) = bbox(t.iter().copied());
            let (x0, x1) = (clamp(tlo.x - EPS, w), clamp(thi.x + EPS, w));
            let (y0, y1) = (clamp(tlo.y - EPS, h), clamp(thi.y + EPS, h));
            for cy in y0..=y1 {
                for cx in x0..=x1 {
                    if !cells[cy * w + cx] && tri_touches_cell(&t, cx, cy) {
                        cells[cy * w + cx] = true;
                    }
                }
            }
        }
        if shape.tris.is_empty() {
            for p in &q {
                cells[clamp(p.y, h) * w + clamp(p.x, w)] = true;
            }
        }
        // Trim empty rows/columns at the low side so row 0 is never empty
        // (keeps `fx` meaningful); rasterization can leave them only through
        // floating-point slack, so the high side is left alone.
        let first_row = (0..h).find(|&y| cells[y * w..(y + 1) * w].iter().any(|&c| c))?;
        let (cells, h) = if first_row > 0 {
            (cells[first_row * w..].to_vec(), h - first_row)
        } else {
            (cells, h)
        };
        let lo = lo + DVec2::new(0.0, first_row as f64 / k);

        let mw = w.div_ceil(64);
        let mut bits = vec![0u64; mw * h];
        for y in 0..h {
            for x in 0..w {
                if cells[y * w + x] {
                    bits[y * mw + x / 64] |= 1 << (x % 64);
                }
            }
        }
        let fx = (0..w).find(|&x| cells[x]).unwrap_or(0);
        Some(Mask {
            w,
            h,
            mw,
            bits,
            fx,
            dilated: dilate(&cells, w, h, pad),
            lo,
            xform,
        })
    }
}

#[derive(Clone)]
struct Grid {
    gx: usize,
    gy: usize,
    gw: usize,
    bits: Vec<u64>,
}

impl Grid {
    fn new((gx, gy): (usize, usize)) -> Grid {
        let gw = gx.div_ceil(64);
        let mut bits = vec![0u64; gw * gy];
        if gx % 64 != 0 {
            let tail = !0u64 << (gx % 64);
            for y in 0..gy {
                bits[y * gw + gw - 1] = tail;
            }
        }
        Grid { gx, gy, gw, bits }
    }

    fn fits(&self, m: &Mask, x: usize, y: usize) -> bool {
        let (base, sh) = (x / 64, x % 64);
        for r in 0..m.h {
            let row = &self.bits[(y + r) * self.gw..(y + r + 1) * self.gw];
            for i in 0..m.mw {
                let v = m.bits[r * m.mw + i];
                if v == 0 {
                    continue;
                }
                if row[base + i] & (v << sh) != 0 {
                    return false;
                }
                if sh > 0 && base + i + 1 < self.gw && row[base + i + 1] & (v >> (64 - sh)) != 0 {
                    return false;
                }
            }
        }
        true
    }

    /// Lowest position (by top edge).
    fn find(&self, m: &Mask) -> Option<(usize, usize)> {
        if gpu::eligible(self, m) {
            match self.find_cpu_budget(m, 2048) {
                Ok(result) => return result,
                Err(()) => return gpu::find(self, m).unwrap_or_else(|| self.find_cpu(m)),
            }
        }
        self.find_cpu(m)
    }

    fn find_cpu(&self, m: &Mask) -> Option<(usize, usize)> {
        self.find_cpu_budget(m, usize::MAX)
            .expect("unbounded search")
    }

    /// Scan cheap candidates first; return `Err` when enough expensive fit
    /// checks have failed that a parallel GPU search is worthwhile.
    fn find_cpu_budget(&self, m: &Mask, budget: usize) -> Result<Option<(usize, usize)>, ()> {
        if m.w > self.gx || m.h > self.gy {
            return Ok(None);
        }
        let mut checks = 0;
        let max_x = self.gx - m.w;
        for y in 0..=self.gy - m.h {
            // Candidates: cells of row y that are free under the mask's first cell.
            let (a, b) = (m.fx, m.fx + max_x);
            let row = &self.bits[y * self.gw..(y + 1) * self.gw];
            for (wi, &word) in row.iter().enumerate().take(b / 64 + 1).skip(a / 64) {
                let mut free = !word;
                if wi == a / 64 {
                    free &= !0u64 << (a % 64);
                }
                if wi == b / 64 && b % 64 != 63 {
                    free &= (1u64 << (b % 64 + 1)) - 1;
                }
                while free != 0 {
                    let x = wi * 64 + free.trailing_zeros() as usize - m.fx;
                    if checks >= budget {
                        return Err(());
                    }
                    checks += 1;
                    if self.fits(m, x, y) {
                        return Ok(Some((x, y)));
                    }
                    free &= free - 1;
                }
            }
        }
        Ok(None)
    }

    fn insert(&mut self, m: &Mask, x: usize, y: usize, pad: usize) {
        let dw = m.w + 2 * pad;
        for dy in 0..m.h + 2 * pad {
            let Some(gy) = (y + dy).checked_sub(pad).filter(|&v| v < self.gy) else {
                continue;
            };
            for dx in 0..dw {
                if !m.dilated[dy * dw + dx] {
                    continue;
                }
                if let Some(gx) = (x + dx).checked_sub(pad).filter(|&v| v < self.gx) {
                    self.bits[gy * self.gw + gx / 64] |= 1 << (gx % 64);
                }
            }
        }
    }
}

/// How hard the scale search works (see [`pack`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Effort {
    /// Scales tried per coarse round, as a geometric series of this ratio.
    pub coarse_trials: usize,
    pub coarse_ratio: f64,
    /// Scales tried between the coarse result and the next coarse step.
    pub refine_trials: usize,
    /// Fine-grid scales: `s * (1 + fine_step * k)` for `k` in `fine_range`.
    pub fine_range: (i32, i32),
    pub fine_step: f64,
}

impl Effort {
    pub const NORMAL: Effort = Effort {
        coarse_trials: 16,
        coarse_ratio: 0.92,
        refine_trials: 12,
        fine_range: (-2, 9),
        fine_step: 0.01,
    };
}

/// Where and how to pack.
#[derive(Debug, Clone)]
pub struct Params {
    /// Lower-left corner of the usable area of tile 0 (margin included).
    pub origin: DVec2,
    /// Usable size of one tile's area.
    pub size: DVec2,
    /// Padding between islands, in UV units.
    pub pad: f64,
    /// Grid cells along the longest side of `size`.
    pub cells: usize,
    /// Transforms tried for each island, in order of preference.
    pub xforms: Vec<Xform>,
    pub max_scale: f64,
    /// Maximum number of tiles (1: single tile).
    pub max_tiles: usize,
    /// Tiles per row: tile `t` is offset by `(t % cols, t / cols)` UV units.
    pub cols: usize,
    pub effort: Effort,
    /// 0: area, 1: longest dimension, 2: perimeter.
    pub order_mode: u64,
}

impl Params {
    pub fn tile_offset(&self, t: usize) -> DVec2 {
        let cols = self.cols.max(1);
        DVec2::new((t % cols) as f64, (t / cols) as f64)
    }
}

/// Result of [`pack`].
#[derive(Debug, Clone)]
pub struct Packed {
    pub scale: f64,
    pub placements: Vec<Placement>,
    pub tiles_used: usize,
}

struct Setup<'a> {
    shapes: &'a [Shape],
    order: Vec<usize>,
    params: &'a Params,
    /// Grids with the obstacles already in, per (grid size, tile).
    bases: BTreeMap<(usize, usize), Grid>,
}

fn pad_cells(pad: f64, cell: f64) -> usize {
    if pad > 0.0 {
        (pad / cell - EPS).ceil() as usize
    } else {
        0
    }
}

/// Grid dimensions for `g` cells along the longest side, and the cell size.
fn dims(size: DVec2, g: usize) -> ((usize, usize), f64) {
    let cell = size.max_element() / g as f64;
    let n = |s: f64| {
        if s == size.max_element() {
            g
        } else {
            ((s / cell + EPS).floor() as usize).clamp(1, g)
        }
    };
    ((n(size.x), n(size.y)), cell)
}

/// A grid with fixed triangles (absolute UVs) rasterized and dilated by the
/// padding, like placed islands. `origin` is the grid's lower-left corner.
fn obstacle_grid(
    (gx, gy): (usize, usize),
    cell: f64,
    origin: DVec2,
    pad: f64,
    obstacles: &[[DVec2; 3]],
) -> Grid {
    let mut cells = vec![false; gx * gy];
    let (fx, fy) = (gx as f64, gy as f64);
    for t in obstacles {
        let q = t.map(|p| (p - origin) / cell);
        let (lo, hi) = bbox(q.iter().copied());
        if hi.x < 0.0 || hi.y < 0.0 || lo.x >= fx || lo.y >= fy {
            continue;
        }
        let clamp = |v: f64, n: usize| (v.floor().max(0.0) as usize).min(n - 1);
        for cy in clamp(lo.y - EPS, gy)..=clamp(hi.y + EPS, gy) {
            for cx in clamp(lo.x - EPS, gx)..=clamp(hi.x + EPS, gx) {
                if !cells[cy * gx + cx] && tri_touches_cell(&q, cx, cy) {
                    cells[cy * gx + cx] = true;
                }
            }
        }
    }
    let pad = pad_cells(pad, cell);
    let dilated = dilate(&cells, gx, gy, pad);
    let dw = gx + 2 * pad;
    let mut grid = Grid::new((gx, gy));
    for y in 0..gy {
        for x in 0..gx {
            if dilated[(y + pad) * dw + x + pad] {
                grid.bits[y * grid.gw + x / 64] |= 1 << (x % 64);
            }
        }
    }
    grid
}

/// Tiles whose area the obstacles may touch (a superset), always with tile 0.
fn obstacle_tiles(p: &Params, obstacles: &[[DVec2; 3]]) -> Vec<usize> {
    let mut tiles = std::collections::BTreeSet::from([0]);
    let cols = p.cols.max(1);
    let rows = p.max_tiles.div_ceil(cols);
    for t in obstacles {
        let (lo, hi) = bbox(t.iter().map(|&q| q - p.origin));
        if hi.x < 0.0 || hi.y < 0.0 {
            continue;
        }
        // Tile (c, r) spans [c, c + size.x] × [r, r + size.y] around `origin`.
        let c0 = (lo.x - p.size.x).floor().max(0.0) as usize;
        let r0 = (lo.y - p.size.y).floor().max(0.0) as usize;
        let c1 = (hi.x.floor() as usize).min(cols - 1);
        let r1 = (hi.y.floor() as usize).min(rows.saturating_sub(1));
        for r in r0..=r1 {
            for c in c0..=c1 {
                let i = r * cols + c;
                if i < p.max_tiles {
                    tiles.insert(i);
                }
            }
        }
    }
    tiles.into_iter().collect()
}

impl Setup<'_> {
    /// Places every shape at `scale` on grids of `g` cells along the longest
    /// side, or `None` if one does not fit in the allowed tiles.
    fn attempt(&self, scale: f64, g: usize) -> Option<(Vec<Placement>, usize)> {
        let p = self.params;
        let (dim, cell) = dims(p.size, g);
        let pad = pad_cells(p.pad, cell);
        let base = |t: usize| {
            self.bases
                .get(&(g, t))
                .map_or_else(|| (Grid::new(dim), false), |b| (b.clone(), true))
        };
        let mut grids: Vec<Grid> = Vec::new();
        let mut out = vec![
            Placement {
                xform: Xform::IDENTITY,
                offset: DVec2::ZERO,
                tile: 0,
            };
            self.shapes.len()
        ];
        for &i in &self.order {
            let masks: Vec<Mask> = p
                .xforms
                .par_iter()
                .filter_map(|&xf| Mask::new(&self.shapes[i], xf, scale, cell, pad, dim))
                .collect();
            if masks.is_empty() {
                return None;
            }
            let mut t = 0;
            let (t, m, (x, y)) = loop {
                let mut fresh = false;
                if t == grids.len() {
                    if t >= p.max_tiles {
                        return None;
                    }
                    let (grid, has_obstacles) = base(t);
                    grids.push(grid);
                    fresh = !has_obstacles;
                }
                let grid = &grids[t];
                // Each transform searched in parallel; keep the lowest top edge
                // (ties: transform order).
                let found: Vec<Option<(usize, usize)>> =
                    masks.par_iter().map(|m| grid.find(m)).collect();
                if let Some((m, xy)) = masks
                    .iter()
                    .zip(found)
                    .filter_map(|(m, f)| f.map(|p| (m, p)))
                    .min_by_key(|(m, (_, y))| y + m.h)
                {
                    break (t, m, xy);
                }
                if fresh {
                    // Does not fit even in an empty tile.
                    return None;
                }
                t += 1;
            };
            grids[t].insert(m, x, y, pad);
            let corner = p.origin + p.tile_offset(t) + DVec2::new(x as f64, y as f64) * cell;
            out[i] = Placement {
                xform: m.xform,
                offset: corner - m.lo * scale,
                tile: t,
            };
        }
        Some((out, grids.len()))
    }

    /// Tries all `scales` (capped at `max_scale`) in parallel; the largest
    /// feasible one wins.
    fn best_of(&self, scales: &[f64], g: usize) -> Option<Packed> {
        let mut scales: Vec<f64> = scales
            .iter()
            .map(|&s| s.min(self.params.max_scale))
            .collect();
        scales.dedup();
        scales
            .par_iter()
            .filter_map(|&s| {
                self.attempt(s, g).map(|(placements, tiles_used)| Packed {
                    scale: s,
                    placements,
                    tiles_used,
                })
            })
            .max_by(|a, b| a.scale.total_cmp(&b.scale))
    }
}

impl Setup<'_> {
    async fn attempt_async(
        &self,
        scale: f64,
        g: usize,
        finder: Option<&gpu::Finder>,
    ) -> Option<(Vec<Placement>, usize)> {
        let p = self.params;
        let (dim, cell) = dims(p.size, g);
        let pad = pad_cells(p.pad, cell);
        let mut grids = Vec::<Grid>::new();
        let mut out = vec![
            Placement {
                xform: Xform::IDENTITY,
                offset: DVec2::ZERO,
                tile: 0,
            };
            self.shapes.len()
        ];
        for &i in &self.order {
            let masks: Vec<Mask> = p
                .xforms
                .par_iter()
                .filter_map(|&xf| Mask::new(&self.shapes[i], xf, scale, cell, pad, dim))
                .collect();
            if masks.is_empty() {
                return None;
            }
            let mut t = 0;
            let (t, m, (x, y)) = loop {
                let mut fresh = false;
                if t == grids.len() {
                    if t >= p.max_tiles {
                        return None;
                    }
                    let (grid, has_obstacles) = self
                        .bases
                        .get(&(g, t))
                        .map_or_else(|| (Grid::new(dim), false), |b| (b.clone(), true));
                    grids.push(grid);
                    fresh = !has_obstacles;
                }
                let grid = &grids[t];
                let mut found = Vec::with_capacity(masks.len());
                for mask in &masks {
                    let xy =
                        if let Some(finder) = finder.filter(|_| gpu::eligible_async(grid, mask)) {
                            match grid.find_cpu_budget(mask, 2048) {
                                Ok(xy) => xy,
                                Err(()) => finder
                                    .find_async(grid, mask)
                                    .await
                                    .unwrap_or_else(|_| grid.find_cpu(mask)),
                            }
                        } else {
                            grid.find_cpu(mask)
                        };
                    found.push(xy);
                }
                if let Some((m, xy)) = masks
                    .iter()
                    .zip(found)
                    .filter_map(|(m, xy)| xy.map(|xy| (m, xy)))
                    .min_by_key(|(m, (_, y))| y + m.h)
                {
                    break (t, m, xy);
                }
                if fresh {
                    return None;
                }
                t += 1;
            };
            grids[t].insert(m, x, y, pad);
            let corner = p.origin + p.tile_offset(t) + DVec2::new(x as f64, y as f64) * cell;
            out[i] = Placement {
                xform: m.xform,
                offset: corner - m.lo * scale,
                tile: t,
            };
        }
        Some((out, grids.len()))
    }

    async fn best_of_async(
        &self,
        scales: &[f64],
        g: usize,
        finder: Option<&gpu::Finder>,
    ) -> Option<Packed> {
        let mut scales: Vec<f64> = scales
            .iter()
            .map(|&s| s.min(self.params.max_scale))
            .collect();
        scales.dedup();
        let mut best: Option<Packed> = None;
        for s in scales {
            if let Some((placements, tiles_used)) = self.attempt_async(s, g, finder).await {
                if best.as_ref().is_none_or(|b| s > b.scale) {
                    best = Some(Packed {
                        scale: s,
                        placements,
                        tiles_used,
                    });
                }
            }
        }
        best
    }
}

/// Packs `shapes` into the rectangle `origin + [0, size]` (repeated per tile),
/// around fixed `obstacles` (triangles in absolute UVs), with a uniform scale
/// of at most `max_scale`. Returns the scale and one placement per shape, or
/// `None` if nothing fits even at a tiny scale.
pub fn pack(shapes: &[Shape], obstacles: &[[DVec2; 3]], p: &Params) -> Option<Packed> {
    pack_with_progress(shapes, obstacles, p, &Progress::default())
        .expect("default progress cannot be cancelled")
}

/// Raster scale search with checkpoints between each batch of trial scales.
/// `total = 0` denotes an indeterminate number of search batches.
pub fn pack_with_progress(
    shapes: &[Shape],
    obstacles: &[[DVec2; 3]],
    p: &Params,
    progress: &Progress,
) -> Result<Option<Packed>, Cancelled> {
    progress.check()?;
    let mut area = 0.0;
    let mut max_ext = 0.0f64;
    let mut keys = Vec::with_capacity(shapes.len());
    let mut extents = Vec::with_capacity(shapes.len());
    for s in shapes {
        let (lo, hi) = bbox(s.pts.iter().copied());
        let ext = hi - lo;
        max_ext = max_ext.max(ext.max_element());
        area += s
            .tris
            .iter()
            .map(|t| {
                let [a, b, c] = t.map(|i| s.pts[i as usize]);
                0.5 * (b - a).perp_dot(c - a).abs()
            })
            .sum::<f64>();
        keys.push(ext.x * ext.y);
        extents.push(ext);
    }
    if max_ext <= 0.0 || p.xforms.is_empty() || p.max_tiles == 0 {
        return Ok(None);
    }
    let mut order: Vec<usize> = (0..shapes.len()).collect();
    let key = |index: usize| match p.order_mode {
        1 => extents[index].max_element(),
        2 => extents[index].x + extents[index].y,
        _ => keys[index],
    };
    order.sort_by(|&a, &b| key(b).total_cmp(&key(a)).then(a.cmp(&b)));
    let g = p.cells.max(1);
    let coarse = (g / 4).max(32);
    let mut bases = BTreeMap::new();
    if !obstacles.is_empty() {
        let jobs: Vec<(usize, usize)> = obstacle_tiles(p, obstacles)
            .into_iter()
            .flat_map(|t| [(coarse, t), (g, t)])
            .collect();
        bases = jobs
            .par_iter()
            .map(|&(n, t)| {
                let (dim, cell) = dims(p.size, n);
                let origin = p.origin + p.tile_offset(t);
                ((n, t), obstacle_grid(dim, cell, origin, p.pad, obstacles))
            })
            .collect();
    }
    let setup = Setup {
        shapes,
        order,
        params: p,
        bases,
    };

    // Upper bounds: the largest island fits along the longest side, and the
    // total area fits in the allowed tiles.
    let side = p.size.max_element();
    let mut hi = (side / max_ext).min(p.max_scale);
    if area > 0.0 {
        let tile_area = if p.size.x == p.size.y {
            side
        } else {
            (p.size.x * p.size.y).sqrt()
        };
        let room = if p.max_tiles > 1 {
            tile_area * (p.max_tiles as f64).sqrt()
        } else {
            tile_area
        };
        hi = hi.min(room / area.sqrt());
    }
    let e = p.effort;
    // Several scales are tried at once (each attempt is sequential), first
    // on a coarse grid (cheap, more conservative masks), then on the fine one.
    let geometric =
        |top: f64, r: f64, n: usize| (0..n).map(|k| top * r.powi(k as i32)).collect::<Vec<_>>();
    let mut top = hi;
    let mut batches = 0;
    let found = loop {
        progress.report("pack_raster_scale", batches, 0)?;
        if let Some(f) = setup.best_of(&geometric(top, e.coarse_ratio, e.coarse_trials), coarse) {
            break f;
        }
        batches += 1;
        top *= e.coarse_ratio.powi(e.coarse_trials as i32);
        if top < hi * 1e-6 {
            return Ok(None);
        }
    };
    progress.report("pack_raster_scale", batches + 1, 0)?;
    let step = |base: f64, span: f64, n: usize| {
        (1..=n)
            .map(|k| base * (1.0 + span * k as f64 / n as f64))
            .collect::<Vec<_>>()
    };
    let s = setup
        .best_of(
            &step(found.scale, 1.0 / e.coarse_ratio - 1.0, e.refine_trials),
            coarse,
        )
        .map_or(found.scale, |f| f.scale);
    // Fine grid: around the coarse scale (finer masks usually allow a bit more).
    let fine: Vec<f64> = (e.fine_range.0..=e.fine_range.1)
        .map(|k| s * (1.0 + e.fine_step * k as f64))
        .collect();
    if let Some(f) = setup.best_of(&fine, g) {
        progress.check()?;
        return Ok(Some(f));
    }
    let mut s = s * 0.98;
    while s > hi * 1e-6 {
        batches += 1;
        progress.report("pack_raster_scale", batches + 1, 0)?;
        if let Some(f) = setup.best_of(&geometric(s, 0.97, 8), g) {
            progress.check()?;
            return Ok(Some(f));
        }
        s *= 0.97f64.powi(8);
    }
    progress.check()?;
    Ok(None)
}

/// Async raster packer for browser WebGPU. Expensive placement searches run
/// compute shaders when WebGPU is available; adapter or mapping failures fall
/// back to the same CPU search. Await this from the browser event loop.
pub async fn pack_with_progress_async(
    shapes: &[Shape],
    obstacles: &[[DVec2; 3]],
    p: &Params,
    progress: &Progress,
) -> Result<Option<Packed>, Cancelled> {
    progress.check()?;
    let mut area = 0.0;
    let mut max_ext = 0.0f64;
    let mut keys = Vec::with_capacity(shapes.len());
    let mut extents = Vec::with_capacity(shapes.len());
    for s in shapes {
        let (lo, hi) = bbox(s.pts.iter().copied());
        let ext = hi - lo;
        max_ext = max_ext.max(ext.max_element());
        area += s
            .tris
            .iter()
            .map(|t| {
                let [a, b, c] = t.map(|i| s.pts[i as usize]);
                0.5 * (b - a).perp_dot(c - a).abs()
            })
            .sum::<f64>();
        keys.push(ext.x * ext.y);
        extents.push(ext);
    }
    if max_ext <= 0.0 || p.xforms.is_empty() || p.max_tiles == 0 {
        return Ok(None);
    }
    let mut order: Vec<usize> = (0..shapes.len()).collect();
    let key = |index: usize| match p.order_mode {
        1 => extents[index].max_element(),
        2 => extents[index].x + extents[index].y,
        _ => keys[index],
    };
    order.sort_by(|&a, &b| key(b).total_cmp(&key(a)).then(a.cmp(&b)));
    let g = p.cells.max(1);
    let coarse = (g / 4).max(32);
    let mut bases = BTreeMap::new();
    if !obstacles.is_empty() {
        let jobs: Vec<(usize, usize)> = obstacle_tiles(p, obstacles)
            .into_iter()
            .flat_map(|t| [(coarse, t), (g, t)])
            .collect();
        bases = jobs
            .par_iter()
            .map(|&(n, t)| {
                let (dim, cell) = dims(p.size, n);
                let origin = p.origin + p.tile_offset(t);
                ((n, t), obstacle_grid(dim, cell, origin, p.pad, obstacles))
            })
            .collect();
    }
    let setup = Setup {
        shapes,
        order,
        params: p,
        bases,
    };
    let side = p.size.max_element();
    let mut hi = (side / max_ext).min(p.max_scale);
    if area > 0.0 {
        let tile_area = if p.size.x == p.size.y {
            side
        } else {
            (p.size.x * p.size.y).sqrt()
        };
        let room = if p.max_tiles > 1 {
            tile_area * (p.max_tiles as f64).sqrt()
        } else {
            tile_area
        };
        hi = hi.min(room / area.sqrt());
    }
    let finder = gpu::Finder::new_async(false).await.ok();
    let e = p.effort;
    let geometric =
        |top: f64, r: f64, n: usize| (0..n).map(|k| top * r.powi(k as i32)).collect::<Vec<_>>();
    let mut top = hi;
    let mut batches = 0;
    let found = loop {
        progress.report("pack_raster_scale", batches, 0)?;
        if let Some(f) = setup
            .best_of_async(
                &geometric(top, e.coarse_ratio, e.coarse_trials),
                coarse,
                finder.as_ref(),
            )
            .await
        {
            break f;
        }
        batches += 1;
        top *= e.coarse_ratio.powi(e.coarse_trials as i32);
        if top < hi * 1e-6 {
            return Ok(None);
        }
    };
    progress.report("pack_raster_scale", batches + 1, 0)?;
    let step = |base: f64, span: f64, n: usize| {
        (1..=n)
            .map(|k| base * (1.0 + span * k as f64 / n as f64))
            .collect::<Vec<_>>()
    };
    let s = setup
        .best_of_async(
            &step(found.scale, 1.0 / e.coarse_ratio - 1.0, e.refine_trials),
            coarse,
            finder.as_ref(),
        )
        .await
        .map_or(found.scale, |f| f.scale);
    let fine: Vec<f64> = (e.fine_range.0..=e.fine_range.1)
        .map(|k| s * (1.0 + e.fine_step * k as f64))
        .collect();
    if let Some(f) = setup.best_of_async(&fine, g, finder.as_ref()).await {
        progress.check()?;
        return Ok(Some(f));
    }
    let mut s = s * 0.98;
    while s > hi * 1e-6 {
        batches += 1;
        progress.report("pack_raster_scale", batches + 1, 0)?;
        if let Some(f) = setup
            .best_of_async(&geometric(s, 0.97, 8), g, finder.as_ref())
            .await
        {
            progress.check()?;
            return Ok(Some(f));
        }
        s *= 0.97f64.powi(8);
    }
    progress.check()?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xform_mirror_and_angle() {
        let p = DVec2::new(1.0, 2.0);
        assert_eq!(Xform::IDENTITY.apply(p), p);
        assert_eq!(Xform::quarter(1).apply(p), DVec2::new(-2.0, 1.0));
        let m = Xform {
            mirror: true,
            ..Xform::IDENTITY
        };
        assert_eq!(m.apply(p), DVec2::new(-1.0, 2.0));
        let a = Xform {
            angle: std::f64::consts::FRAC_PI_2,
            ..Xform::IDENTITY
        };
        assert!((a.apply(p) - DVec2::new(-2.0, 1.0)).length() < 1e-12);
    }

    #[test]
    fn rectangular_grid_dims() {
        let ((gx, gy), cell) = dims(DVec2::new(2.0, 1.0), 100);
        assert_eq!((gx, gy), (100, 50));
        assert!((cell - 0.02).abs() < 1e-15);
        assert_eq!(dims(DVec2::splat(0.5), 64).0, (64, 64));
    }

    #[test]
    fn async_pack_matches_sync_pack() {
        let shapes = [Shape {
            pts: vec![DVec2::ZERO, DVec2::X * 0.3, DVec2::Y * 0.3],
            tris: vec![[0, 1, 2]],
        }];
        let p = Params {
            origin: DVec2::ZERO,
            size: DVec2::ONE,
            pad: 0.01,
            cells: 64,
            xforms: vec![Xform::IDENTITY],
            max_scale: 1.0,
            max_tiles: 1,
            cols: 1,
            effort: Effort {
                coarse_trials: 2,
                coarse_ratio: 0.9,
                refine_trials: 2,
                fine_range: (-1, 1),
                fine_step: 0.01,
            },
            order_mode: 0,
        };
        let sync = pack(&shapes, &[], &p).unwrap();
        let asynchronous = pollster::block_on(pack_with_progress_async(
            &shapes,
            &[],
            &p,
            &Progress::default(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(asynchronous.scale, sync.scale);
        assert_eq!(asynchronous.tiles_used, sync.tiles_used);
        assert_eq!(asynchronous.placements[0].offset, sync.placements[0].offset);
    }
}
