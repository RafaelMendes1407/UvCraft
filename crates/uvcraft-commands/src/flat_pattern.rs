//! Dimensioned SVG cut contours. UV coordinates are already in the chosen
//! physical unit; callers can use `set_real_scale` before exporting.

use glam::DVec2;
use uvcraft_algo::physical::Unit;
use uvcraft_core::{Islands, Mesh, UvLayer};

pub fn render(
    mesh: &Mesh,
    uv: &UvLayer,
    islands: &Islands,
    selected: &[usize],
    unit: Unit,
    margin: f64,
) -> Result<String, String> {
    if !margin.is_finite() || margin < 0.0 {
        return Err("flat pattern margin must be finite and nonnegative".into());
    }
    let targets: Vec<usize> = if selected.is_empty() {
        (0..islands.count()).collect()
    } else {
        selected.to_vec()
    };
    if targets.is_empty() {
        return Err("flat pattern has no islands".into());
    }
    let mut included = vec![false; islands.count()];
    let mut face_island = vec![usize::MAX; mesh.face_count()];
    for island in 0..islands.count() {
        for &face in islands.faces(island) {
            face_island[face as usize] = island;
        }
    }
    for &island in &targets {
        let Some(flag) = included.get_mut(island) else {
            return Err(format!("island {island} does not exist"));
        };
        *flag = true;
    }

    let mut segments = Vec::new();
    let mut min = DVec2::INFINITY;
    let mut max = DVec2::NEG_INFINITY;
    for c in 0..mesh.corner_count() {
        let island = face_island[mesh.corner_face(c)];
        if island == usize::MAX || !included[island] {
            continue;
        }
        if mesh
            .twin(c)
            .is_some_and(|other| face_island[mesh.corner_face(other)] == island)
        {
            continue;
        }
        let a = uv.uvs[uv.corner_uv[c] as usize];
        let b = uv.uvs[uv.corner_uv[mesh.next(c)] as usize];
        if !a.is_finite() || !b.is_finite() {
            return Err("flat pattern contains non-finite UV coordinates".into());
        }
        min = min.min(a).min(b);
        max = max.max(a).max(b);
        if a.distance_squared(b) > 1e-24 {
            segments.push((island, a, b));
        }
    }
    if segments.is_empty() {
        return Err("flat pattern has no nondegenerate contours".into());
    }
    let size = max - min + DVec2::splat(margin * 2.0);
    if !size.is_finite() || size.x <= 0.0 || size.y <= 0.0 {
        return Err("flat pattern dimensions are invalid".into());
    }
    let source_unit = match unit {
        Unit::Millimeter => "mm",
        Unit::Centimeter => "cm",
        Unit::Meter => "m",
        Unit::Inch => "in",
    };
    // Millimeters are valid SVG CSS units (meters are not), and make the
    // generated file directly measurable in print and vector applications.
    let to_mm = unit.to(Unit::Millimeter);
    let size_mm = size * to_mm;
    if !size_mm.is_finite() {
        return Err("flat pattern dimensions overflow millimeters".into());
    }
    let mut out = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{:.9}mm\" height=\"{:.9}mm\" viewBox=\"0 0 {:.9} {:.9}\" data-uv-unit=\"{source_unit}\">\n<g fill=\"none\" stroke=\"black\" stroke-width=\"0.1\" vector-effect=\"non-scaling-stroke\">\n",
        size_mm.x, size_mm.y, size_mm.x, size_mm.y
    );
    for (island, a, b) in segments {
        let a = DVec2::new(a.x - min.x + margin, max.y - a.y + margin) * to_mm;
        let b = DVec2::new(b.x - min.x + margin, max.y - b.y + margin) * to_mm;
        out.push_str(&format!(
            "<path data-island=\"{island}\" d=\"M {:.9} {:.9} L {:.9} {:.9}\"/>\n",
            a.x, a.y, b.x, b.y
        ));
    }
    out.push_str("</g>\n</svg>\n");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Command, Session};
    use glam::DVec3;

    #[test]
    fn exports_a_dimensioned_quad_without_internal_edges() {
        let mesh = Mesh::new(
            vec![DVec3::ZERO, DVec3::X, DVec3::ONE, DVec3::Y],
            &[[0, 1, 2], [0, 2, 3]],
        )
        .unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 0, 2, 3],
            uvs: vec![
                DVec2::ZERO,
                DVec2::new(10.0, 0.0),
                DVec2::new(10.0, 5.0),
                DVec2::new(0.0, 5.0),
            ],
        };
        let islands = Islands::compute(&mesh, &uv);
        let svg = render(&mesh, &uv, &islands, &[], Unit::Millimeter, 1.0).unwrap();
        assert!(svg.contains("width=\"12.000000000mm\""));
        assert!(svg.contains("height=\"7.000000000mm\""));
        assert_eq!(svg.matches("<path ").count(), 4);
        let cm = render(&mesh, &uv, &islands, &[], Unit::Centimeter, 0.0).unwrap();
        assert!(cm.contains("width=\"100.000000000mm\""));
        assert!(cm.contains("data-uv-unit=\"cm\""));
        assert!(render(&mesh, &uv, &islands, &[9], Unit::Millimeter, 0.0).is_err());
    }

    #[test]
    fn export_command_writes_svg_from_loaded_mesh() {
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let input = std::env::temp_dir().join(format!("uvcraft-pattern-{suffix}.obj"));
        let output = input.with_extension("svg");
        std::fs::write(&input, "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nvt 0 0\nvt 10 0\nvt 10 5\nvt 0 5\nf 1/1 2/2 3/3 4/4\n").unwrap();
        let mut session = Session::new();
        session
            .execute(Command::Load {
                path: input.clone(),
            })
            .unwrap();
        session
            .execute(Command::ExportFlatPattern {
                path: output.clone(),
                islands: Vec::new(),
                unit: Unit::Millimeter,
                margin: 1.0,
            })
            .unwrap();
        let svg = std::fs::read_to_string(&output).unwrap();
        assert!(svg.contains("width=\"12.000000000mm\""));
        assert_eq!(svg.matches("<path ").count(), 4);
        std::fs::remove_file(input).unwrap();
        std::fs::remove_file(output).unwrap();
    }
}
