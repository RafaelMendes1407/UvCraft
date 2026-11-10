//! Wavefront OBJ reader/writer with faithful round-trip.
//!
//! The original bytes are kept. On export, if the UV layer is unchanged the
//! file is written back byte-for-byte. Otherwise only UV data is rewritten:
//! all `vt` lines are replaced by one block, and `f` lines get new `vt`
//! indices while keeping their `v` and `vn` tokens as written. Positions,
//! normals, face order, groups, materials and comments are untouched.

use std::collections::HashMap;
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
    let mut normal_count = 0usize;
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
                normal_count += 1;
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
                    if let Some(n) = parts.next().filter(|n| !n.is_empty()) {
                        resolve(n, normal_count, "normal", line_no)?;
                    }
                }
                let object = *current_object.get_or_insert_with(|| {
                    objects.push("default".into());
                    objects.len() as u32 - 1
                });
                face_object.push(object);
                faces.push(fv);
                face_vts.push(ft);
                LineKind::Face(faces.len() as u32 - 1)
            }
            Some("l") => LineKind::Polyline,
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
    fn reports_line_numbers() {
        let err = parse(b"v 0 0 0\nv 1 0 0\nf 1 2\n".to_vec()).unwrap_err();
        assert!(matches!(err, ObjError::Parse { line: 3, .. }), "{err}");
        let err = parse(b"v 0 0\n".to_vec()).unwrap_err();
        assert!(matches!(err, ObjError::Parse { line: 1, .. }), "{err}");
        let err = parse(b"v 0 0 0\nf 1 2 4\n".to_vec()).unwrap_err();
        assert!(matches!(err, ObjError::Parse { line: 2, .. }), "{err}");
    }
}
