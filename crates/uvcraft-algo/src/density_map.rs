//! Per-island density targets sampled from a grayscale PGM map.

use glam::DVec2;
use uvcraft_core::{Islands, Mesh, UvLayer};

#[derive(Debug, Clone)]
pub struct DensityMap {
    pub width: usize,
    pub height: usize,
    pub values: Vec<f64>,
}

impl DensityMap {
    pub fn to_pgm(&self) -> Vec<u8> {
        let mut bytes = format!("P5\n{} {}\n255\n", self.width, self.height).into_bytes();
        bytes.extend(
            self.values
                .iter()
                .map(|&value| (value.clamp(0.0, 1.0) * 255.0).round() as u8),
        );
        bytes
    }

    /// Portable graymap P2 (ASCII) or P5 (8-bit binary), including comments.
    pub fn parse_pgm(bytes: &[u8]) -> Result<Self, String> {
        let mut pos = 0;
        let mut token = || -> Result<&[u8], String> {
            while pos < bytes.len() {
                if bytes[pos] == b'#' {
                    while pos < bytes.len() && bytes[pos] != b'\n' {
                        pos += 1;
                    }
                } else if bytes[pos].is_ascii_whitespace() {
                    pos += 1;
                } else {
                    break;
                }
            }
            let start = pos;
            while pos < bytes.len() && !bytes[pos].is_ascii_whitespace() && bytes[pos] != b'#' {
                pos += 1;
            }
            if start == pos {
                return Err("truncated PGM header".into());
            }
            Ok(&bytes[start..pos])
        };
        let magic = token()?.to_vec();
        if magic != b"P2" && magic != b"P5" {
            return Err("expected P2 or P5 PGM".into());
        }
        let number = |raw: &[u8]| -> Result<usize, String> {
            std::str::from_utf8(raw)
                .map_err(|_| "non-ASCII PGM header".to_owned())?
                .parse::<usize>()
                .map_err(|_| "invalid PGM number".to_owned())
        };
        let width = number(token()?)?;
        let height = number(token()?)?;
        let max = number(token()?)?;
        let count = width.checked_mul(height).ok_or("PGM dimensions overflow")?;
        if width == 0 || height == 0 || max == 0 || max > 255 || count > 100_000_000 {
            return Err("invalid PGM dimensions or maximum value".into());
        }
        let values = if magic == b"P2" {
            let mut out = Vec::with_capacity(count);
            for _ in 0..count {
                let value = number(token()?)?;
                if value > max {
                    return Err("PGM value exceeds maximum".into());
                }
                out.push(value as f64 / max as f64);
            }
            out
        } else {
            if pos >= bytes.len() || !bytes[pos].is_ascii_whitespace() {
                return Err("invalid P5 separator".into());
            }
            pos += 1;
            if bytes.len() - pos < count {
                return Err("truncated P5 pixels".into());
            }
            if bytes[pos..pos + count].iter().any(|&v| v as usize > max) {
                return Err("PGM value exceeds maximum".into());
            }
            bytes[pos..pos + count]
                .iter()
                .map(|&v| v as f64 / max as f64)
                .collect()
        };
        Ok(Self {
            width,
            height,
            values,
        })
    }

    pub fn sample(&self, uv: DVec2) -> f64 {
        let u = uv.x.rem_euclid(1.0);
        let v = uv.y.rem_euclid(1.0);
        let x = (u * self.width as f64).floor() as usize % self.width;
        let y = ((1.0 - v) * self.height as f64).floor() as usize % self.height;
        self.values[y * self.width + x]
    }

    /// Average map coverage over each island, mapped to a UV scale factor.
    pub fn multipliers(
        &self,
        mesh: &Mesh,
        uv: &UvLayer,
        islands: &Islands,
        targets: &[usize],
        min_scale: f64,
        max_scale: f64,
    ) -> Result<Vec<(usize, f64)>, String> {
        if !min_scale.is_finite()
            || !max_scale.is_finite()
            || min_scale <= 0.0
            || max_scale < min_scale
        {
            return Err("density map scale range must be finite and positive".into());
        }
        let mut out = Vec::with_capacity(targets.len());
        for &island in targets {
            if island >= islands.count() {
                return Err(format!("island {island} does not exist"));
            }
            let mut sum = 0.0;
            let mut area = 0.0;
            for &face in islands.faces(island) {
                for tri in mesh.fan_triangles(face as usize) {
                    let [a, b, c] = tri.map(|corner| uv.uv_at_corner(corner));
                    let weight = 0.5 * (b - a).perp_dot(c - a).abs();
                    if weight <= 0.0 {
                        continue;
                    }
                    // Symmetric barycentric samples reduce sensitivity to the
                    // tessellation while bounding work per triangle.
                    let samples = [
                        a * 0.6 + b * 0.2 + c * 0.2,
                        a * 0.2 + b * 0.6 + c * 0.2,
                        a * 0.2 + b * 0.2 + c * 0.6,
                        (a + b + c) / 3.0,
                    ];
                    sum += weight * samples.into_iter().map(|p| self.sample(p)).sum::<f64>() / 4.0;
                    area += weight;
                }
            }
            let average = if area > 0.0 { sum / area } else { 0.0 };
            out.push((island, min_scale + (max_scale - min_scale) * average));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ascii_and_binary_maps() {
        let ascii = DensityMap::parse_pgm(b"P2\n# target\n2 1\n255\n0 255\n").unwrap();
        let binary = DensityMap::parse_pgm(b"P5\n2 1\n255\n\0\xff").unwrap();
        assert_eq!(ascii.values, binary.values);
        assert_eq!(ascii.sample(DVec2::new(0.25, 0.5)), 0.0);
        assert_eq!(ascii.sample(DVec2::new(0.75, 0.5)), 1.0);
    }
}
