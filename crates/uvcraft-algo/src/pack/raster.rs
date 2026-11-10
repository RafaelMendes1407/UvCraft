//! Raster (bitmask) packing.
//!
//! The available square is a `g × g` grid of cells. Each island is
//! conservatively rasterized (every cell its triangles touch is set), so
//! islands whose masks do not overlap never overlap in UV space. The grid
//! stores placed islands dilated by the padding (in cells); a new island's
//! undilated mask must not hit any set cell. Islands go largest first, each at
//! the position (and 90° rotation) with the lowest top edge, scanning
//! bottom-left with word-level bit tricks. The scale is the largest one where
//! every island fits, found by a coarse descent followed by bisection.

use glam::DVec2;
use rayon::prelude::*;

/// An island in its packing frame (already oriented; any offset).
pub struct Shape {
    pub pts: Vec<DVec2>,
    pub tris: Vec<[u32; 3]>,
}

/// Final UV of a point `p` of the shape: `rotate(p, rot) * scale + offset`.
#[derive(Debug, Clone, Copy)]
pub struct Placement {
    pub rot: u8,
    pub offset: DVec2,
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

/// Bitmask of one shape at one rotation and scale.
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
    /// Shape bbox minimum after rotation, in UV (unscaled).
    lo: DVec2,
    rot: u8,
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
    /// `None` when the shape does not fit in a `g × g` grid.
    fn new(shape: &Shape, rot: u8, scale: f64, cell: f64, pad: usize, g: usize) -> Option<Mask> {
        let pts: Vec<DVec2> = shape.pts.iter().map(|&p| rotate(p, rot)).collect();
        let (lo, hi) = bbox(pts.iter().copied());
        let k = scale / cell;
        let ext = (hi - lo) * k;
        let w = ((ext.x - EPS).ceil().max(1.0)) as usize;
        let h = ((ext.y - EPS).ceil().max(1.0)) as usize;
        if w > g || h > g {
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
            rot,
        })
    }
}

struct Grid {
    g: usize,
    gw: usize,
    bits: Vec<u64>,
}

impl Grid {
    fn new(g: usize) -> Grid {
        let gw = g.div_ceil(64);
        let mut bits = vec![0u64; gw * g];
        if g % 64 != 0 {
            let tail = !0u64 << (g % 64);
            for y in 0..g {
                bits[y * gw + gw - 1] = tail;
            }
        }
        Grid { g, gw, bits }
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

    /// Lowest position (by top edge) with a top edge below `bound`.
    fn find(&self, m: &Mask, bound: usize) -> Option<(usize, usize)> {
        if m.w > self.g || m.h > self.g {
            return None;
        }
        let max_x = self.g - m.w;
        for y in 0..=self.g - m.h {
            if y + m.h >= bound {
                return None;
            }
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
                    if self.fits(m, x, y) {
                        return Some((x, y));
                    }
                    free &= free - 1;
                }
            }
        }
        None
    }

    fn insert(&mut self, m: &Mask, x: usize, y: usize, pad: usize) {
        let dw = m.w + 2 * pad;
        for dy in 0..m.h + 2 * pad {
            let Some(gy) = (y + dy).checked_sub(pad).filter(|&v| v < self.g) else {
                continue;
            };
            for dx in 0..dw {
                if !m.dilated[dy * dw + dx] {
                    continue;
                }
                if let Some(gx) = (x + dx).checked_sub(pad).filter(|&v| v < self.g) {
                    self.bits[gy * self.gw + gx / 64] |= 1 << (gx % 64);
                }
            }
        }
    }
}

struct Setup<'a> {
    shapes: &'a [Shape],
    order: Vec<usize>,
    rotations: &'a [u8],
    margin: f64,
    avail: f64,
    pad: f64,
}

impl Setup<'_> {
    /// Places every shape at `scale` on a `g × g` grid, or `None` if one does
    /// not fit.
    fn attempt(&self, scale: f64, g: usize) -> Option<Vec<Placement>> {
        let cell = self.avail / g as f64;
        let pad = if self.pad > 0.0 {
            (self.pad / cell - EPS).ceil() as usize
        } else {
            0
        };
        let mut grid = Grid::new(g);
        let mut out = vec![
            Placement {
                rot: 0,
                offset: DVec2::ZERO
            };
            self.shapes.len()
        ];
        for &i in &self.order {
            let masks: Vec<Mask> = self
                .rotations
                .par_iter()
                .filter_map(|&rot| Mask::new(&self.shapes[i], rot, scale, cell, pad, g))
                .collect();
            // Each rotation searched in parallel; keep the lowest top edge
            // (ties: rotation order).
            let found: Vec<Option<(usize, usize)>> =
                masks.par_iter().map(|m| grid.find(m, usize::MAX)).collect();
            let (m, (x, y)) = masks
                .iter()
                .zip(found)
                .filter_map(|(m, f)| f.map(|p| (m, p)))
                .min_by_key(|(m, (_, y))| y + m.h)?;
            grid.insert(m, x, y, pad);
            let corner = DVec2::splat(self.margin) + DVec2::new(x as f64, y as f64) * cell;
            out[i] = Placement {
                rot: m.rot,
                offset: corner - m.lo * scale,
            };
        }
        Some(out)
    }

    /// Tries all `scales` in parallel; the largest feasible one wins.
    fn best_of(&self, scales: &[f64], g: usize) -> Option<(f64, Vec<Placement>)> {
        scales
            .par_iter()
            .filter_map(|&s| self.attempt(s, g).map(|p| (s, p)))
            .max_by(|a, b| a.0.total_cmp(&b.0))
    }
}

/// Packs `shapes` into the square `[margin, margin + avail]²` on a `g × g`
/// grid. Returns the uniform scale and one placement per shape, or `None` if
/// nothing fits even at a tiny scale.
pub fn pack(
    shapes: &[Shape],
    avail: f64,
    margin: f64,
    pad: f64,
    g: usize,
    rotations: bool,
) -> Option<(f64, Vec<Placement>)> {
    let mut area = 0.0;
    let mut max_ext = 0.0f64;
    let mut keys = Vec::with_capacity(shapes.len());
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
    }
    if max_ext <= 0.0 {
        return None;
    }
    let mut order: Vec<usize> = (0..shapes.len()).collect();
    order.sort_by(|&a, &b| keys[b].total_cmp(&keys[a]));
    let setup = Setup {
        shapes,
        order,
        rotations: if rotations { &[0, 1, 2, 3] } else { &[0] },
        margin,
        avail,
        pad,
    };

    let mut hi = avail / max_ext;
    if area > 0.0 {
        hi = hi.min(avail / area.sqrt());
    }
    // Several scales are tried at once (each attempt is sequential), first
    // on a coarse grid (cheap, more conservative masks), then on the fine one.
    let coarse = (g / 4).max(32);
    let geometric =
        |top: f64, r: f64, n: usize| (0..n).map(|k| top * r.powi(k as i32)).collect::<Vec<_>>();
    let mut top = hi;
    let found = loop {
        if let Some(f) = setup.best_of(&geometric(top, 0.92, 16), coarse) {
            break f;
        }
        top *= 0.92f64.powi(16);
        if top < hi * 1e-6 {
            return None;
        }
    };
    let step = |base: f64, span: f64, n: usize| {
        (1..=n)
            .map(|k| base * (1.0 + span * k as f64 / n as f64))
            .collect::<Vec<_>>()
    };
    let s = setup
        .best_of(&step(found.0, 1.0 / 0.92 - 1.0, 12), coarse)
        .map_or(found.0, |f| f.0);
    // Fine grid: around the coarse scale (finer masks usually allow a bit more).
    let fine: Vec<f64> = (-2..=9).map(|k| s * (1.0 + 0.01 * k as f64)).collect();
    if let Some(f) = setup.best_of(&fine, g) {
        return Some(f);
    }
    let mut s = s * 0.98;
    while s > hi * 1e-6 {
        if let Some(f) = setup.best_of(&geometric(s, 0.97, 8), g) {
            return Some(f);
        }
        s *= 0.97f64.powi(8);
    }
    None
}
