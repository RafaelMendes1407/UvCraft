//! Wavefront OBJ reader/writer with faithful round-trip.
//!
//! The original bytes are kept. On export, if the UV layer is unchanged the
//! file is written back byte-for-byte. Otherwise only UV data is rewritten:
//! all `vt` lines are replaced by one block, and `f` lines get new `vt`
//! indices while keeping their `v` and `vn` tokens as written. Positions,
//! normals, face order, groups, materials and comments are untouched.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write as _;
use std::ops::Range;
use std::path::Path;

use glam::{DVec2, DVec3};
use uvcraft_core::{Mesh, MeshError, UvError, UvLayer};

#[derive(Debug, thiserror::Error)]
pub enum ObjError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("line {line}: {message}")]
    Parse { line: usize, message: String },
    #[error(transparent)]
    Mesh(#[from] MeshError),
    #[error(transparent)]
    Uv(#[from] UvError),
    #[error("smoothing group count does not match face count")]
    SmoothingCount,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum LineKind {
    Other,
    Vt,
    Face(u32),
    /// Polyline (`l`); may reference `vt`, so it is rewritten too.
    Polyline,
}

/// Original file content needed to write it back faithfully.
#[derive(Debug, Clone)]
pub struct ObjSource {
    original: Vec<u8>,
    lines: Vec<(Range<usize>, LineKind)>,
    original_uv: UvLayer,
    newline: &'static str,
    /// Data used only when converting to another format.
    pub(crate) extra: ObjExtra,
    face_smoothing: Vec<Option<u32>>,
}

/// Per-face and per-corner OBJ data that the [`Mesh`] does not hold.
#[derive(Debug, Clone, Default)]
pub(crate) struct ObjExtra {
    /// `vn` values in file order.
    pub normals: Vec<DVec3>,
    /// Normal index per corner, or `u32::MAX` when the corner has none.
    pub corner_normal: Vec<u32>,
    /// `usemtl` names in first-use order.
    pub materials: Vec<String>,
    /// Material index per face, or `u32::MAX` before any `usemtl`.
    pub face_material: Vec<u32>,
    pub objects: Vec<String>,
    pub face_object: Vec<u32>,
}

/// A parsed OBJ file. The whole file is one mesh; `o` names are kept per face.
#[derive(Debug, Clone)]
pub struct ObjDocument {
    pub mesh: Mesh,
    pub uv: UvLayer,
    pub source: ObjSource,
    /// Object names from `o` lines ("default" for faces before any `o`).
    pub objects: Vec<String>,
    pub face_object: Vec<u32>,
    /// Corners that had no `vt` in the file and got a placeholder UV at (0, 0).
    pub missing_uv_corners: usize,
}

pub fn read(path: &Path) -> Result<ObjDocument, ObjError> {
    parse(std::fs::read(path)?)
}

pub fn write(path: &Path, source: &ObjSource, mesh: &Mesh, uv: &UvLayer) -> Result<(), ObjError> {
    let bytes = source.export(mesh, uv)?;
    let mut file = std::fs::File::create(path)?;
    file.write_all(&bytes)?;
    Ok(())
}

/// Writes an OBJ with an explicit, per-face smoothing assignment. Original
/// lines (including comments and materials) remain intact; a new `s` line
/// immediately before each face overrides earlier smoothing declarations.
pub fn write_with_smoothing(
    path: &Path,
    source: &ObjSource,
    mesh: &Mesh,
    uv: &UvLayer,
    groups: &[Option<u32>],
) -> Result<(), ObjError> {
    std::fs::write(path, source.export_with_smoothing(mesh, uv, groups)?)?;
    Ok(())
}

/// Splits into lines, each range including its terminator.
fn line_ranges(bytes: &[u8]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            out.push(start..i + 1);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push(start..bytes.len());
    }
    out
}

/// Line content without terminator, and the terminator itself.
fn split_terminator(raw: &[u8]) -> (&[u8], &[u8]) {
    let mut end = raw.len();
    if end > 0 && raw[end - 1] == b'\n' {
        end -= 1;
        if end > 0 && raw[end - 1] == b'\r' {
            end -= 1;
        }
    }
    raw.split_at(end)
}

/// Text before `#`, and the comment (including `#`) if any.
fn split_comment(text: &str) -> (&str, Option<&str>) {
    match text.find('#') {
        Some(i) => (&text[..i], Some(&text[i..])),
        None => (text, None),
    }
}

fn resolve(token: &str, count: usize, what: &str, line: usize) -> Result<u32, ObjError> {
    let err = |message: String| ObjError::Parse { line, message };
    let i: i64 = token
        .parse()
        .map_err(|_| err(format!("invalid {what} index '{token}'")))?;
    let idx = match i {
        0 => return Err(err(format!("{what} index 0 is invalid (OBJ is 1-based)"))),
        i if i > 0 => i - 1,
        i => count as i64 + i,
    };
    if idx < 0 || idx as usize >= count {
        return Err(err(format!(
            "{what} index {i} out of range ({count} defined)"
        )));
    }
    Ok(idx as u32)
}

fn floats<'a>(tokens: impl Iterator<Item = &'a str>, line: usize) -> Result<Vec<f64>, ObjError> {
    tokens
        .map(|t| {
            t.parse::<f64>().map_err(|_| ObjError::Parse {
                line,
                message: format!("invalid number '{t}'"),
            })
        })
        .collect()
}

pub fn parse(original: Vec<u8>) -> Result<ObjDocument, ObjError> {
    let ranges = line_ranges(&original);
    let newline = match ranges.first() {
        Some(r) if original[r.clone()].ends_with(b"\r\n") => "\r\n",
        _ => "\n",
    };

    let mut positions: Vec<DVec3> = Vec::new();
    let mut vts: Vec<DVec2> = Vec::new();
    let mut normals: Vec<DVec3> = Vec::new();
    let mut corner_normal: Vec<u32> = Vec::new();
    let mut materials: Vec<String> = Vec::new();
    let mut current_material = u32::MAX;
    let mut face_material: Vec<u32> = Vec::new();
    let mut face_smoothing_labels: Vec<Option<String>> = Vec::new();
    let mut current_smoothing: Option<String> = None;
    let mut faces: Vec<Vec<u32>> = Vec::new();
    let mut face_vts: Vec<Vec<Option<u32>>> = Vec::new();
    let mut objects: Vec<String> = Vec::new();
    let mut current_object: Option<u32> = None;
    let mut face_object: Vec<u32> = Vec::new();
    let mut lines = Vec::with_capacity(ranges.len());

    for (line_idx, range) in ranges.into_iter().enumerate() {
        let line_no = line_idx + 1;
        let (content, _) = split_terminator(&original[range.clone()]);
        let text = String::from_utf8_lossy(content);
        let (data, _) = split_comment(&text);
        let mut tokens = data.split_whitespace();
        let kind = match tokens.next() {
            Some("v") => {
                let f = floats(tokens.take(3), line_no)?;
                if f.len() < 3 {
                    return Err(ObjError::Parse {
                        line: line_no,
                        message: "vertex needs 3 coordinates".into(),
                    });
                }
                positions.push(DVec3::new(f[0], f[1], f[2]));
                LineKind::Other
            }
            Some("vt") => {
                let f = floats(tokens.take(2), line_no)?;
                let Some(&u) = f.first() else {
                    return Err(ObjError::Parse {
                        line: line_no,
                        message: "texture coordinate needs at least 1 value".into(),
                    });
                };
                vts.push(DVec2::new(u, f.get(1).copied().unwrap_or(0.0)));
                LineKind::Vt
            }
            Some("vn") => {
                let f = floats(tokens.take(3), line_no)?;
                let mut n = DVec3::ZERO;
                for (k, x) in f.into_iter().enumerate() {
                    n[k] = x;
                }
                normals.push(n);
                LineKind::Other
            }
            Some("f") => {
                let mut fv = Vec::new();
                let mut ft = Vec::new();
                for token in tokens {
                    let mut parts = token.split('/');
                    let v = parts.next().unwrap_or_default();
                    fv.push(resolve(v, positions.len(), "vertex", line_no)?);
                    ft.push(match parts.next() {
                        Some(t) if !t.is_empty() => {
                            Some(resolve(t, vts.len(), "texture", line_no)?)
                        }
                        _ => None,
                    });
                    corner_normal.push(match parts.next().filter(|n| !n.is_empty()) {
                        Some(n) => resolve(n, normals.len(), "normal", line_no)?,
                        None => u32::MAX,
                    });
                }
                let object = *current_object.get_or_insert_with(|| {
                    objects.push("default".into());
                    objects.len() as u32 - 1
                });
                face_object.push(object);
                face_material.push(current_material);
                face_smoothing_labels.push(current_smoothing.clone());
                faces.push(fv);
                face_vts.push(ft);
                LineKind::Face(faces.len() as u32 - 1)
            }
            Some("l") => LineKind::Polyline,
            Some("s") => {
                let label = tokens.next().unwrap_or("off");
                current_smoothing = if label.eq_ignore_ascii_case("off") || label == "0" {
                    None
                } else if label.eq_ignore_ascii_case("on") {
                    Some("1".into())
                } else {
                    Some(label.to_string())
                };
                LineKind::Other
            }
            Some("usemtl") => {
                let name = data.trim_start()[6..].trim();
                current_material = match materials.iter().position(|m| m == name) {
                    Some(i) => i as u32,
                    None => {
                        materials.push(name.to_string());
                        materials.len() as u32 - 1
                    }
                };
                LineKind::Other
            }
            Some("o") => {
                let name = data.trim_start()[1..].trim();
                objects.push(if name.is_empty() {
                    "unnamed".into()
                } else {
                    name.into()
                });
                current_object = Some(objects.len() as u32 - 1);
                LineKind::Other
            }
            _ => LineKind::Other,
        };
        lines.push((range, kind));
    }

    // OBJ permits named smoothing groups as well as numeric labels. Give
    // names stable IDs that do not collide with any numeric group in the file.
    let mut used: BTreeSet<u32> = face_smoothing_labels
        .iter()
        .flatten()
        .filter_map(|s| s.parse::<u32>().ok())
        .collect();
    let mut names: HashMap<String, u32> = HashMap::new();
    let mut next = 1u32;
    let face_smoothing: Vec<Option<u32>> = face_smoothing_labels
        .into_iter()
        .map(|label| {
            label.map(|s| {
                if let Ok(id) = s.parse::<u32>() {
                    return id;
                }
                *names.entry(s).or_insert_with(|| {
                    while used.contains(&next) {
                        next += 1;
                    }
                    let id = next;
                    used.insert(id);
                    next += 1;
                    id
                })
            })
        })
        .collect();

    let mesh = Mesh::new(positions, &faces).map_err(|e| match e {
        MeshError::FaceTooSmall { face, count } => ObjError::Parse {
            line: face_line(&lines, face),
            message: format!("face has {count} vertices; at least 3 are required"),
        },
        other => other.into(),
    })?;

    // Corners without vt share one placeholder UV per mesh vertex.
    let mut uvs = vts;
    let mut placeholder: HashMap<u32, u32> = HashMap::new();
    let mut missing = 0;
    let mut corner_uv = Vec::with_capacity(mesh.corner_count());
    for (fv, ft) in faces.iter().zip(&face_vts) {
        for (&v, &t) in fv.iter().zip(ft) {
            corner_uv.push(match t {
                Some(t) => t,
                None => {
                    missing += 1;
                    *placeholder.entry(v).or_insert_with(|| {
                        uvs.push(DVec2::ZERO);
                        uvs.len() as u32 - 1
                    })
                }
            });
        }
    }
    let uv = UvLayer {
        name: "map1".into(),
        corner_uv,
        uvs,
    };
    uv.validate(&mesh)?;

    Ok(ObjDocument {
        mesh,
        source: ObjSource {
            original,
            lines,
            original_uv: uv.clone(),
            newline,
            face_smoothing,
            extra: ObjExtra {
                normals,
                corner_normal,
                materials,
                face_material,
                objects: objects.clone(),
                face_object: face_object.clone(),
            },
        },
        uv,
        objects,
        face_object,
        missing_uv_corners: missing,
    })
}

fn face_line(lines: &[(Range<usize>, LineKind)], face: usize) -> usize {
    lines
        .iter()
        .position(|(_, k)| *k == LineKind::Face(face as u32))
        .map_or(0, |i| i + 1)
}

impl ObjSource {
    pub fn material_names(&self) -> &[String] {
        &self.extra.materials
    }

    /// Material index of each face, or `u32::MAX` before any `usemtl`.
    pub fn face_material(&self) -> &[u32] {
        &self.extra.face_material
    }

    pub fn face_smoothing(&self) -> &[Option<u32>] {
        &self.face_smoothing
    }

    /// Edges whose explicit OBJ corner normals disagree across a shared edge.
    /// Missing normals on both sides are left to smoothing-group handling.
    pub fn normal_discontinuity_edges(&self, mesh: &Mesh) -> Vec<usize> {
        let mut edges = Vec::new();
        for edge in 0..mesh.edge_count() {
            let (a, Some(b)) = mesh.edge_corners(edge) else {
                continue;
            };
            let matching = if mesh.corner_vertex(a) == mesh.corner_vertex(b) {
                [(a, b), (mesh.next(a), mesh.next(b))]
            } else {
                [(a, mesh.next(b)), (mesh.next(a), b)]
            };
            if matching.iter().any(|&(left, right)| {
                let a = self.extra.corner_normal[left];
                let b = self.extra.corner_normal[right];
                match (a == u32::MAX, b == u32::MAX) {
                    (true, true) => false,
                    (true, false) | (false, true) => true,
                    (false, false) => {
                        self.extra.normals[a as usize]
                            .distance_squared(self.extra.normals[b as usize])
                            > 1e-12
                    }
                }
            }) {
                edges.push(edge);
            }
        }
        edges
    }

    pub fn export_with_smoothing(
        &self,
        mesh: &Mesh,
        uv: &UvLayer,
        groups: &[Option<u32>],
    ) -> Result<Vec<u8>, ObjError> {
        if groups.len() != mesh.face_count() {
            return Err(ObjError::SmoothingCount);
        }
        let base = self.export(mesh, uv)?;
        // Explicit OBJ normals supersede `s` labels. Recompute every corner's
        // normal under the new groups, sharing one averaged normal per mesh
        // vertex and smoothing group. Faces marked `s off` stay independent.
        let key = |face: usize, corner: usize| {
            (
                mesh.corner_vertex(corner),
                groups[face].unwrap_or(u32::MAX),
                if groups[face].is_none() { face } else { 0 },
            )
        };
        let mut sums: BTreeMap<(usize, u32, usize), DVec3> = BTreeMap::new();
        for face in 0..mesh.face_count() {
            let normal = mesh.face_area_normal(face);
            for corner in mesh.face_corners(face) {
                *sums.entry(key(face, corner)).or_default() += normal;
            }
        }
        let mut normal_id = BTreeMap::new();
        let mut normal_lines = Vec::new();
        for (key, sum) in sums {
            let normal = if sum.length_squared() > 0.0 {
                sum.normalize()
            } else {
                DVec3::Z
            };
            normal_id.insert(key, normal_id.len() as u32 + 1);
            normal_lines.extend_from_slice(
                format!("vn {} {} {}{}", normal.x, normal.y, normal.z, self.newline).as_bytes(),
            );
        }
        let corner_normal: Vec<u32> = (0..mesh.corner_count())
            .map(|corner| normal_id[&key(mesh.corner_face(corner), corner)])
            .collect();
        let mut out = Vec::with_capacity(base.len() + groups.len() * 8);
        let mut face = 0;
        let mut normals_written = false;
        for range in line_ranges(&base) {
            let raw = &base[range];
            let (content, _) = split_terminator(raw);
            let text = String::from_utf8_lossy(content);
            let (data, _) = split_comment(&text);
            let directive = data.split_whitespace().next();
            if directive == Some("vn") {
                continue;
            }
            if directive == Some("f") {
                if !normals_written {
                    out.extend_from_slice(&normal_lines);
                    normals_written = true;
                }
                let value = groups[face].map_or_else(|| "off".to_string(), |n| n.to_string());
                out.extend_from_slice(format!("s {value}{}", self.newline).as_bytes());
                let corners = mesh.face_corners(face);
                rewrite_line(raw, &mut out, |index, parts| {
                    let vertex = parts.first().copied().unwrap_or_default();
                    let normal = corner_normal[corners.start + index];
                    match parts.get(1).filter(|uv| !uv.is_empty()) {
                        Some(uv) => format!("{vertex}/{uv}/{normal}"),
                        None => format!("{vertex}//{normal}"),
                    }
                });
                face += 1;
            } else {
                out.extend_from_slice(raw);
            }
        }
        Ok(out)
    }

    /// The bytes to write for this mesh with the given UV layer.
    pub fn export(&self, mesh: &Mesh, uv: &UvLayer) -> Result<Vec<u8>, ObjError> {
        uv.validate(mesh)?;
        // Index-only differences (e.g. split shared UVs) keep the original bytes.
        if uv.same_mapping(&self.original_uv, mesh) {
            return Ok(self.original.clone());
        }

        let mut out = Vec::with_capacity(self.original.len());
        let mut vt_block_written = false;
        for (range, kind) in &self.lines {
            let raw = &self.original[range.clone()];
            if matches!(kind, LineKind::Vt | LineKind::Face(_)) && !vt_block_written {
                // Before the first vt or f line, so every reference follows its definition.
                for p in &uv.uvs {
                    out.extend_from_slice(format!("vt {} {}{}", p.x, p.y, self.newline).as_bytes());
                }
                vt_block_written = true;
            }
            match *kind {
                LineKind::Other => out.extend_from_slice(raw),
                LineKind::Vt => {}
                LineKind::Face(f) => {
                    let corners = mesh.face_corners(f as usize);
                    rewrite_line(raw, &mut out, |k, parts| {
                        let v = parts.first().copied().unwrap_or_default();
                        let t = uv.corner_uv[corners.start + k] + 1;
                        match parts.get(2).filter(|n| !n.is_empty()) {
                            Some(n) => format!("{v}/{t}/{n}"),
                            None => format!("{v}/{t}"),
                        }
                    });
                }
                // Old vt indices are meaningless after the rewrite: keep vertices only.
                LineKind::Polyline => rewrite_line(raw, &mut out, |_, parts| {
                    parts.first().copied().unwrap_or_default().to_string()
                }),
            }
        }
        if !vt_block_written {
            for p in &uv.uvs {
                out.extend_from_slice(format!("vt {} {}{}", p.x, p.y, self.newline).as_bytes());
            }
        }
        Ok(out)
    }
}

/// Rewrites each element token of an `f`/`l` line, keeping keyword, comment and terminator.
fn rewrite_line(raw: &[u8], out: &mut Vec<u8>, mut token: impl FnMut(usize, &[&str]) -> String) {
    let (content, terminator) = split_terminator(raw);
    let text = String::from_utf8_lossy(content);
    let (data, comment) = split_comment(&text);
    let mut tokens = data.split_whitespace();
    let mut line = tokens.next().unwrap_or_default().to_string();
    for (k, t) in tokens.enumerate() {
        let parts: Vec<&str> = t.split('/').collect();
        line.push(' ');
        line.push_str(&token(k, &parts));
    }
    if let Some(c) = comment {
        line.push(' ');
        line.push_str(c);
    }
    out.extend_from_slice(line.as_bytes());
    out.extend_from_slice(terminator);
}

#[cfg(test)]
mod tests {
    use super::*;

    const CUBE_FACE: &str = "# two quads\r\nmtllib a.mtl\r\no Plane\r\nv 0 0 0\r\nv 1 0 0\r\nv 1 1 0\r\nv 0 1 0\r\nv 2 0 0\r\nv 2 1 0\r\nvt 0 0\r\nvt 0.5 0\r\nvt 0.5 1\r\nvt 0 1\r\nvt 1 0\r\nvt 1 1\r\nvn 0 0 1\r\nusemtl red\r\ns off\r\nf 1/1/1 2/2/1 3/3/1 4/4/1 # left\r\nf -5/-5/-1 5/5/1 6/6/1 3/3/1\r\nl 1/1 2/2";

    #[test]
    fn unchanged_uvs_round_trip_byte_for_byte() {
        let doc = parse(CUBE_FACE.as_bytes().to_vec()).unwrap();
        assert_eq!(doc.mesh.face_count(), 2);
        assert_eq!(doc.uv.uvs.len(), 6);
        assert_eq!(doc.objects, vec!["Plane"]);
        // Relative index -5 resolves to vertex 2 (index 1) and vt 2 (index 1).
        assert_eq!(doc.mesh.corner_vertex(4), 1);
        assert_eq!(doc.uv.corner_uv[4], 1);
        let out = doc.source.export(&doc.mesh, &doc.uv).unwrap();
        assert_eq!(out, CUBE_FACE.as_bytes());
    }

    #[test]
    fn smoothing_override_keeps_original_lines_and_reloads_groups() {
        let doc = parse(CUBE_FACE.as_bytes().to_vec()).unwrap();
        assert_eq!(doc.source.face_smoothing(), &[None, None]);
        let out = doc
            .source
            .export_with_smoothing(&doc.mesh, &doc.uv, &[Some(1), Some(2)])
            .unwrap();
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.contains("s off\r\nvn "));
        assert!(text.contains("s 1\r\nf 1/1/"));
        assert!(text.contains("s 2\r\nf -5/-5/"));
        assert!(text.contains("# left"));
        let again = parse(out).unwrap();
        assert_eq!(again.source.face_smoothing(), &[Some(1), Some(2)]);
        assert_eq!(again.mesh.positions(), doc.mesh.positions());
        assert_eq!(again.uv, doc.uv);
    }

    #[test]
    fn smoothing_export_rewrites_stale_explicit_normals() {
        let obj = "v 0 0 0\nv 1 0 0\nv 0 1 0\nv 0 0 1\n\
vt 0 0\nvt 1 0\nvt 0 1\nvt 1 1\n\
vn 0 0 1\ns 1\nf 1/1/1 2/2/1 3/3/1\nf 2/2/1 1/1/1 4/4/1\n";
        let doc = parse(obj.as_bytes().to_vec()).unwrap();
        assert!(doc.source.normal_discontinuity_edges(&doc.mesh).is_empty());
        let bytes = doc
            .source
            .export_with_smoothing(&doc.mesh, &doc.uv, &[Some(1), Some(2)])
            .unwrap();
        let saved = parse(bytes).unwrap();
        assert_eq!(saved.mesh.positions(), doc.mesh.positions());
        assert_eq!(saved.uv, doc.uv);
        assert_eq!(saved.source.face_smoothing(), &[Some(1), Some(2)]);
        assert_eq!(
            saved.source.normal_discontinuity_edges(&saved.mesh).len(),
            1
        );
    }

    #[test]
    fn smoothing_parser_accepts_on_and_named_groups() {
        let src =
            "v 0 0 0\nv 1 0 0\nv 0 1 0\nv 1 1 0\ns on\nf 1 2 3\ns smooth\nf 2 4 3\ns 1\nf 1 2 4\n";
        let doc = parse(src.as_bytes().to_vec()).unwrap();
        let groups = doc.source.face_smoothing();
        assert_eq!(groups[0], Some(1));
        assert_eq!(groups[2], Some(1));
        assert_ne!(groups[1], Some(1));
        assert_eq!(
            doc.source.export(&doc.mesh, &doc.uv).unwrap(),
            src.as_bytes()
        );
    }

    #[test]
    fn changed_uvs_rewrite_only_uv_data() {
        let mut doc = parse(CUBE_FACE.as_bytes().to_vec()).unwrap();
        doc.uv.uvs[1].x = 0.25;
        let out = String::from_utf8(doc.source.export(&doc.mesh, &doc.uv).unwrap()).unwrap();
        let expected = "# two quads\r\nmtllib a.mtl\r\no Plane\r\nv 0 0 0\r\nv 1 0 0\r\nv 1 1 0\r\nv 0 1 0\r\nv 2 0 0\r\nv 2 1 0\r\nvt 0 0\r\nvt 0.25 0\r\nvt 0.5 1\r\nvt 0 1\r\nvt 1 0\r\nvt 1 1\r\nvn 0 0 1\r\nusemtl red\r\ns off\r\nf 1/1/1 2/2/1 3/3/1 4/4/1 # left\r\nf -5/2/-1 5/5/1 6/6/1 3/3/1\r\nl 1 2";
        assert_eq!(out, expected);
        let again = parse(out.into_bytes()).unwrap();
        assert_eq!(again.uv, doc.uv);
        assert_eq!(again.mesh.positions(), doc.mesh.positions());
    }

    #[test]
    fn missing_uvs_get_placeholders_and_are_written_before_faces() {
        let src = "v 0 0 0\nv 1 0 0\nv 0 1 0\nv 1 1 0\nf 1 2 3\nf 3//1 2 4\nvn 0 0 1\n";
        // vn after its use is an error: this file is invalid on purpose.
        assert!(parse(src.as_bytes().to_vec()).is_err());

        let src = "v 0 0 0\nv 1 0 0\nv 0 1 0\nv 1 1 0\nvn 0 0 1\nf 1 2 3\nf 3//1 2 4\n";
        let mut doc = parse(src.as_bytes().to_vec()).unwrap();
        assert_eq!(doc.missing_uv_corners, 6);
        assert_eq!(doc.uv.uvs.len(), 4);
        doc.uv.uvs[3] = DVec2::ONE;
        let out = String::from_utf8(doc.source.export(&doc.mesh, &doc.uv).unwrap()).unwrap();
        assert_eq!(
            out,
            "v 0 0 0\nv 1 0 0\nv 0 1 0\nv 1 1 0\nvn 0 0 1\nvt 0 0\nvt 0 0\nvt 0 0\nvt 1 1\nf 1/1 2/2 3/3\nf 3/3/1 2/2 4/4\n"
        );
    }

    #[test]
    fn explicit_corner_normals_identify_hard_edges() {
        let src = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 0 0 1\nv 1 0 1\n\
vn 0 0 1\nvn 0 -1 0\ns 1\nf 1//1 2//1 3//1 4//1\nf 2//2 1//2 5//2 6//2\n";
        let doc = parse(src.as_bytes().to_vec()).unwrap();
        let edges = doc.source.normal_discontinuity_edges(&doc.mesh);
        assert_eq!(edges.len(), 1);
        let [a, b] = doc.mesh.edge_vertices(edges[0]);
        assert_eq!([a.min(b), a.max(b)], [0, 1]);
        let same_vectors = src.replace("vn 0 -1 0", "vn 0 0 1");
        let smooth = parse(same_vectors.into_bytes()).unwrap();
        assert!(
            smooth
                .source
                .normal_discontinuity_edges(&smooth.mesh)
                .is_empty()
        );
    }

    #[test]
    fn reports_line_numbers() {
        let err = parse(b"v 0 0 0\nv 1 0 0\nf 1 2\n".to_vec()).unwrap_err();
        assert!(matches!(err, ObjError::Parse { line: 3, .. }), "{err}");
        let err = parse(b"v 0 0\n".to_vec()).unwrap_err();
        assert!(matches!(err, ObjError::Parse { line: 1, .. }), "{err}");
        let err = parse(b"v 0 0 0\nf 1 2 4\n".to_vec()).unwrap_err();
        assert!(matches!(err, ObjError::Parse { line: 2, .. }), "{err}");
    }
}
