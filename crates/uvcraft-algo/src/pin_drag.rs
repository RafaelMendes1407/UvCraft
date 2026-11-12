//! Warm-started, orientation-safe ARAP steps for moving a pinned UV vertex.

use std::collections::BTreeSet;

use glam::DVec2;
use uvcraft_core::{Islands, Mesh, UvLayer};

use crate::optimize;

#[derive(Debug, Clone, Copy)]
pub struct PinDragParams {
    pub vertex: usize,
    pub destination: DVec2,
    pub iterations: usize,
}

/// Moves every UV copy of `vertex` in the target islands, then relaxes from
/// the supplied UV layout. Reusing the previous result warms the next step.
pub fn step(
    mesh: &Mesh,
    uv: &mut UvLayer,
    islands: &Islands,
    targets: &[usize],
    params: PinDragParams,
    pins: &[bool],
) -> Result<usize, String> {
    let PinDragParams {
        vertex,
        destination,
        iterations,
    } = params;
    if vertex >= mesh.vertex_count() || !pins.get(vertex).copied().unwrap_or(false) {
        return Err("drag a pinned mesh vertex".into());
    }
    if !destination.is_finite() || iterations == 0 {
        return Err("destination must be finite and iterations must be positive".into());
    }
    if targets.is_empty() || targets.iter().any(|&i| i >= islands.count()) {
        return Err("target islands are empty or invalid".into());
    }
    let mut copies = BTreeSet::new();
    let mut triangles = Vec::new();
    let mut active = BTreeSet::new();
    let mut other_pins = BTreeSet::new();
    for &island in targets {
        for &face in islands.faces(island) {
            for corner in mesh.face_corners(face as usize) {
                let id = uv.corner_uv(corner);
                active.insert(id);
                if mesh.corner_vertex(corner) == vertex {
                    copies.insert(id);
                } else if pins
                    .get(mesh.corner_vertex(corner))
                    .copied()
                    .unwrap_or(false)
                {
                    other_pins.insert(id);
                }
            }
            triangles.extend(
                mesh.fan_triangles(face as usize)
                    .map(|tri| tri.map(|c| uv.corner_uv(c))),
            );
        }
    }
    if copies.is_empty() {
        return Err("pinned vertex is outside target islands".into());
    }
    let signed = |layer: &UvLayer, [a, b, c]: [usize; 3]| {
        (layer.uvs[b] - layer.uvs[a]).perp_dot(layer.uvs[c] - layer.uvs[a])
    };
    let baseline: Vec<f64> = triangles.iter().map(|&tri| signed(uv, tri)).collect();
    let mut current = uv.clone();
    let mut fraction = 1.0_f64;
    let mut progress = 0.0_f64;
    let mut accepted = 0;
    let mut attempts = 0;
    let guided: Vec<DVec2> = (0..uv.uvs.len())
        .map(|id| {
            let point = uv.uvs[id];
            if !active.contains(&id) || other_pins.contains(&id) {
                return DVec2::ZERO;
            }
            let &closest = copies
                .iter()
                .min_by(|&&a, &&b| {
                    point
                        .distance_squared(uv.uvs[a])
                        .total_cmp(&point.distance_squared(uv.uvs[b]))
                })
                .expect("dragged vertex has a UV copy");
            let moving_distance = point.distance(uv.uvs[closest]);
            let fixed_distance = other_pins
                .iter()
                .map(|&pin| point.distance(uv.uvs[pin]))
                .fold(f64::INFINITY, f64::min);
            let weight = if fixed_distance.is_infinite() {
                1.0
            } else {
                fixed_distance / (moving_distance + fixed_distance).max(1e-12)
            };
            (destination - uv.uvs[closest]) * weight
        })
        .collect();
    // Solve toward the cursor from the last displayed layout. Large cursor
    // jumps are split only when the direct solve folds a triangle.
    while progress < 1.0 && accepted < 16 && attempts < 32 {
        attempts += 1;
        let next = (progress + fraction).min(1.0);
        let mut candidate = current.clone();
        for &index in &active {
            candidate.uvs[index] = uv.uvs[index] + guided[index] * next;
        }
        for &index in &copies {
            candidate.uvs[index] = uv.uvs[index].lerp(destination, next);
        }
        let pin_only = candidate.clone();
        let report =
            optimize::optimize_arap(mesh, &mut candidate, islands, targets, pins, iterations);
        let safe = |layer: &UvLayer| {
            layer.uvs.iter().all(|p| p.is_finite())
                && triangles.iter().zip(&baseline).all(|(&tri, &old)| {
                    let new = signed(layer, tri);
                    old.abs() <= 1e-14 || old * new > 0.02 * old * old
                })
        };
        let valid = if report.optimized == targets.len() && safe(&candidate) {
            true
        } else if safe(&pin_only)
            && report.optimized == targets.len()
            && candidate.uvs.iter().all(|p| p.is_finite())
        {
            // ARAP may overshoot the valid region. Blend only free vertices
            // back toward the pin-only pose; the cursor constraint stays exact.
            let optimized = candidate;
            let mut alpha = 0.5;
            candidate = pin_only.clone();
            while alpha >= 1.0 / 4096.0 {
                for index in 0..candidate.uvs.len() {
                    if !copies.contains(&index) {
                        candidate.uvs[index] =
                            pin_only.uvs[index].lerp(optimized.uvs[index], alpha);
                    }
                }
                if safe(&candidate) {
                    break;
                }
                alpha *= 0.5;
            }
            true
        } else if safe(&pin_only) {
            candidate = pin_only;
            true
        } else {
            false
        };
        if valid {
            current = candidate;
            progress = next;
            accepted += 1;
            fraction = 1.0 - progress;
        } else {
            fraction *= 0.5;
            if fraction < 1.0 / 4096.0 {
                return Err("pin drag would fold the target island".into());
            }
        }
    }
    if progress < 1.0 {
        return Err("pin drag did not converge".into());
    }
    *uv = current;
    Ok(targets.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seams::tests::grid;
    use std::time::{Duration, Instant};

    #[test]
    fn dragged_pin_stays_fixed_and_second_step_uses_previous_layout() {
        let (mesh, mut uv) = grid(3);
        let islands = Islands::compute(&mesh, &uv);
        let vertex = 0;
        let mut pins = vec![false; mesh.vertex_count()];
        pins[vertex] = true;
        pins[mesh.vertex_count() - 1] = true;
        let first = DVec2::new(0.2, 0.1);
        let second = DVec2::new(0.3, 0.2);
        step(
            &mesh,
            &mut uv,
            &islands,
            &[0],
            PinDragParams {
                vertex,
                destination: first,
                iterations: 1,
            },
            &pins,
        )
        .unwrap();
        let first_uv = uv.clone();
        step(
            &mesh,
            &mut uv,
            &islands,
            &[0],
            PinDragParams {
                vertex,
                destination: second,
                iterations: 1,
            },
            &pins,
        )
        .unwrap();
        let corner = (0..mesh.corner_count())
            .find(|&c| mesh.corner_vertex(c) == vertex)
            .unwrap();
        assert!(uv.uv_at_corner(corner).distance(second) < 1e-9);
        assert_ne!(uv.uvs, first_uv.uvs);
    }

    #[test]
    fn interactive_steps_keep_orientation_and_run_within_a_frame_budget() {
        let (mesh, mut uv) = grid(16);
        let islands = Islands::compute(&mesh, &uv);
        let mut pins = vec![false; mesh.vertex_count()];
        pins[0] = true;
        pins[mesh.vertex_count() - 1] = true;
        let before: Vec<f64> = (0..mesh.face_count())
            .flat_map(|face| mesh.fan_triangles(face))
            .map(|tri| {
                let [a, b, c] = tri.map(|corner| uv.uv_at_corner(corner));
                (b - a).perp_dot(c - a)
            })
            .collect();
        let start = Instant::now();
        for destination in [DVec2::new(0.04, 0.02), DVec2::new(0.08, 0.05)] {
            step(
                &mesh,
                &mut uv,
                &islands,
                &[0],
                PinDragParams {
                    vertex: 0,
                    destination,
                    iterations: 2,
                },
                &pins,
            )
            .unwrap();
            assert_eq!(uv.uvs[0], destination);
            assert_eq!(uv.uvs[mesh.vertex_count() - 1], DVec2::ONE);
        }
        let after = (0..mesh.face_count())
            .flat_map(|face| mesh.fan_triangles(face))
            .map(|tri| {
                let [a, b, c] = tri.map(|corner| uv.uv_at_corner(corner));
                (b - a).perp_dot(c - a)
            });
        assert!(
            before
                .iter()
                .copied()
                .zip(after)
                .all(|(old, new)| old * new > 0.0)
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "interactive steps took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn invalid_move_keeps_the_input_layer() {
        let (mesh, mut uv) = grid(3);
        let before = uv.clone();
        let islands = Islands::compute(&mesh, &uv);
        let mut pins = vec![false; mesh.vertex_count()];
        pins[0] = true;
        assert!(
            step(
                &mesh,
                &mut uv,
                &islands,
                &[0],
                PinDragParams {
                    vertex: 0,
                    destination: DVec2::new(f64::NAN, 0.0),
                    iterations: 2,
                },
                &pins
            )
            .is_err()
        );
        assert_eq!(uv, before);
    }
}
