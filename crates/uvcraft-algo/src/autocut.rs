//! Iterative distortion-driven seam cuts followed by a fresh unfold.

use uvcraft_core::{Cancelled, EdgeKind, Islands, Mesh, Progress, UvLayer};

use crate::{analysis, mosaic, seams, unfold};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutoCutReport {
    pub cuts: usize,
    pub before: f64,
    pub after: f64,
}

fn target_charts(mesh: &Mesh, uv: &UvLayer, selected_faces: &[bool]) -> Vec<usize> {
    let islands = Islands::compute(mesh, uv);
    let mut charts: Vec<_> = (0..mesh.face_count())
        .filter(|&f| selected_faces[f])
        .map(|f| islands.island_of_face(f))
        .collect();
    charts.sort_unstable();
    charts.dedup();
    charts
}

fn metric(mesh: &Mesh, uv: &UvLayer, selected_faces: &[bool]) -> f64 {
    let islands = Islands::compute(mesh, uv);
    mosaic::max_chart_uv_distortion(mesh, uv, &islands, &target_charts(mesh, uv, selected_faces))
}

/// Cut paths from high-distortion interior vertices to the island border.
/// Each candidate is unfolded and accepted only when it reduces the worst
/// angle/area stretch without flips. Cancellation leaves the input untouched.
pub fn cut_reduce_stretch(
    mesh: &Mesh,
    uv: &mut UvLayer,
    selected_faces: &[bool],
    max_cuts: usize,
    target_distortion: f64,
    progress: &Progress,
) -> Result<AutoCutReport, Cancelled> {
    progress.check()?;
    let mut trial = uv.clone();
    let initial = metric(mesh, &trial, selected_faces);
    let mut current = initial;
    let mut cuts = 0;
    for step in 0..max_cuts {
        progress.report("cut_reduce_stretch", step as u64, max_cuts as u64)?;
        if current <= target_distortion {
            break;
        }
        let islands = Islands::compute(mesh, &trial);
        let mut candidates = Vec::new();
        for island in target_charts(mesh, &trial, selected_faces) {
            let report = analysis::analyze_island(mesh, &trial, &islands, island, 1024);
            if report.max_angle_distortion <= target_distortion
                && report.max_area_distortion <= target_distortion
            {
                continue;
            }
            let mut border = vec![false; mesh.vertex_count()];
            let mut in_chart = vec![false; mesh.face_count()];
            for &face in islands.faces(island) {
                in_chart[face as usize] = true;
            }
            for &face in islands.faces(island) {
                for corner in mesh.face_corners(face as usize) {
                    let edge = mesh.corner_edge(corner);
                    if trial.edge_kind(mesh, edge) != EdgeKind::Interior {
                        border[mesh.corner_vertex(corner)] = true;
                        border[mesh.corner_vertex(mesh.next(corner))] = true;
                    }
                }
            }
            for &face in islands.faces(island) {
                let mut face_score: f64 = 1.0;
                for tri in mesh.fan_triangles(face as usize) {
                    let p = tri.map(|c| mesh.corner_position(c));
                    let u = tri.map(|c| trial.uv_at_corner(c));
                    let score = analysis::triangle_map(p, u).map_or(f64::INFINITY, |m| {
                        if m.sigma_min <= 0.0 {
                            f64::INFINITY
                        } else {
                            m.sigma_max / m.sigma_min
                        }
                    });
                    face_score = face_score.max(score);
                }
                for corner in mesh.face_corners(face as usize) {
                    let vertex = mesh.corner_vertex(corner);
                    if border[vertex] {
                        continue;
                    }
                    let allowed = |edge: usize| {
                        if trial.edge_kind(mesh, edge) != EdgeKind::Interior {
                            return false;
                        }
                        let (a, b) = mesh.edge_corners(edge);
                        in_chart[mesh.corner_face(a)]
                            && b.is_some_and(|c| in_chart[mesh.corner_face(c)])
                    };
                    if let Some(path) =
                        seams::shortest_path(mesh, &[vertex], |v| border[v], allowed)
                        && !path.is_empty()
                    {
                        candidates.push((face_score, path));
                    }
                }
            }
        }
        candidates.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.len().cmp(&b.1.len())));
        let mut best: Option<(f64, UvLayer, usize)> = None;
        for (_, path) in candidates.into_iter().take(12) {
            progress.check()?;
            let mut candidate = trial.clone();
            let added = seams::cut_edges(mesh, &mut candidate, &path);
            if added == 0 {
                continue;
            }
            let after_islands = Islands::compute(mesh, &candidate);
            let charts = target_charts(mesh, &candidate, selected_faces);
            let unfolded = unfold::unfold_lscm(mesh, &mut candidate, &after_islands, &charts, &[]);
            if !unfolded.failed.is_empty() || !unfolded.skipped_closed.is_empty() {
                continue;
            }
            let quality = metric(mesh, &candidate, selected_faces);
            if quality.is_finite()
                && quality + 1e-5 < current
                && best.as_ref().is_none_or(|(prior, _, _)| quality < *prior)
            {
                best = Some((quality, candidate, path.len()));
            }
        }
        let Some((quality, layout, edges)) = best else {
            break;
        };
        trial = layout;
        current = quality;
        cuts += edges;
    }
    progress.report("cut_reduce_stretch", max_cuts as u64, max_cuts as u64)?;
    *uv = trial;
    Ok(AutoCutReport {
        cuts,
        before: initial,
        after: current,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::{DVec2, DVec3};

    #[test]
    fn cuts_a_cone_slit_to_reduce_distortion() {
        let mesh = Mesh::new(
            vec![
                DVec3::new(-1.0, -1.0, 0.0),
                DVec3::new(1.0, -1.0, 0.0),
                DVec3::new(1.0, 1.0, 0.0),
                DVec3::new(-1.0, 1.0, 0.0),
                DVec3::new(0.0, 0.0, 2.0),
            ],
            &[[0, 1, 4], [1, 2, 4], [2, 3, 4], [3, 0, 4]],
        )
        .unwrap();
        let mut uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 4, 1, 2, 4, 2, 3, 4, 3, 0, 4],
            uvs: vec![
                DVec2::new(-1.0, -1.0),
                DVec2::new(1.0, -1.0),
                DVec2::new(1.0, 1.0),
                DVec2::new(-1.0, 1.0),
                DVec2::ZERO,
            ],
        };
        let isl = Islands::compute(&mesh, &uv);
        unfold::unfold_lscm(&mesh, &mut uv, &isl, &[0], &[]);
        let before = uv.clone();
        let report =
            cut_reduce_stretch(&mesh, &mut uv, &[true; 4], 2, 1.01, &Progress::default()).unwrap();
        assert!(report.after < report.before, "{report:?}");
        assert!(report.cuts > 0);
        assert_ne!(uv.corner_uv, before.corner_uv);
    }
}
