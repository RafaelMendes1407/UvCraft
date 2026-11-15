//! A conservative FBX 7.x mesh importer. It converts ASCII and binary geometry
//! records to the existing OBJ-backed document so edited UVs can be exported
//! as OBJ or glTF.
//!
//! Static Model hierarchy, pivots, geometric transforms, the three FBX scale
//! inheritance modes, and shared Geometry instances are baked into positions.
//! Animation, constraints, visibility, and deformation are not evaluated.
//! Secondary UV layers are available through [`parse_layers`] and
//! [`export_layers`] and are retained by the application when saving FBX.
//! Conversion to single-channel formats exports the active layer. ASCII export
//! writes one Geometry/Model
//! pair per consecutive object run; vertices shared across runs are duplicated,
//! while face order and materials remain intact.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read;

use flate2::read::ZlibDecoder;
use glam::{DMat4, DVec2, DVec3, DVec4};

use crate::obj::{self, ObjDocument};
use uvcraft_core::{Mesh, UvError, UvLayer};

#[derive(Debug, thiserror::Error)]
pub enum FbxError {
    #[error("unsupported binary FBX version {0}")]
    BinaryVersion(u32),
    #[error("FBX text is not UTF-8")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("invalid FBX ASCII: {0}")]
    Parse(String),
    #[error(transparent)]
    Obj(#[from] obj::ObjError),
    #[error(transparent)]
    Uv(#[from] UvError),
}

fn export_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '"' | '\n' | '\r' | '\\' => '_',
            other => other,
        })
        .collect()
}

/// Export one mesh as FBX 7.4 ASCII. Per-corner UVs and per-face materials
/// are written in face order, with a Model for each consecutive object run.
pub fn export(mesh: &Mesh, uv: &UvLayer, source: &obj::ObjSource) -> Result<Vec<u8>, FbxError> {
    export_with_layers(mesh, uv, &BTreeMap::new(), source)
}

/// Export all supplied UV layers. Channel zero is the active layer; additional
/// channel IDs become FBX LayerElementUV IDs in each Geometry record.
pub fn export_layers(
    mesh: &Mesh,
    layers: &BTreeMap<u32, UvLayer>,
    source: &obj::ObjSource,
) -> Result<Vec<u8>, FbxError> {
    let primary = layers
        .get(&0)
        .ok_or_else(|| FbxError::Parse("FBX export needs UV layer 0".into()))?;
    export_with_layers(mesh, primary, layers, source)
}

fn export_with_layers(
    mesh: &Mesh,
    uv: &UvLayer,
    layers: &BTreeMap<u32, UvLayer>,
    source: &obj::ObjSource,
) -> Result<Vec<u8>, FbxError> {
    uv.validate(mesh)?;
    for layer in layers.values() {
        layer.validate(mesh)?;
    }
    let extra = &source.extra;
    let object_of = |face: usize| {
        if extra.face_object.len() == mesh.face_count() {
            extra.face_object[face]
        } else {
            0
        }
    };
    let mut runs = vec![(0usize, mesh.face_count(), 0u32)];
    if mesh.face_count() > 0 {
        runs.clear();
        let mut start = 0;
        for face in 1..=mesh.face_count() {
            if face == mesh.face_count() || object_of(face) != object_of(start) {
                runs.push((start, face, object_of(start)));
                start = face;
            }
        }
    }
    let materials_valid = extra.face_material.len() == mesh.face_count();
    let mut material_names = extra.materials.clone();
    let default_material = if materials_valid
        && extra
            .face_material
            .iter()
            .any(|&id| id == u32::MAX || id as usize >= material_names.len())
    {
        let index = material_names.len();
        material_names.push("Default".into());
        index
    } else {
        0
    };
    let smoothing = source.face_smoothing();
    let mut smooth_normals: BTreeMap<(usize, u32), DVec3> = BTreeMap::new();
    if smoothing.len() == mesh.face_count() {
        for (face, &group) in smoothing.iter().enumerate() {
            let Some(group) = group else {
                continue;
            };
            let normal = mesh.face_area_normal(face);
            for corner in mesh.face_corners(face) {
                *smooth_normals
                    .entry((mesh.corner_vertex(corner), group))
                    .or_insert(DVec3::ZERO) += normal;
            }
        }
    }
    let mut out = String::from(
        "; FBX 7.4.0 project file\nFBXHeaderExtension: { FBXHeaderVersion: 1003 FBXVersion: 7400 }\n\
         GlobalSettings: { Properties70: {\n\
         P: \"UpAxis\", \"int\", \"Integer\", \"\", 1\n\
         P: \"UpAxisSign\", \"int\", \"Integer\", \"\", 1\n\
         P: \"FrontAxis\", \"int\", \"Integer\", \"\", 2\n\
         P: \"FrontAxisSign\", \"int\", \"Integer\", \"\", 1\n\
         P: \"CoordAxis\", \"int\", \"Integer\", \"\", 0\n\
         P: \"CoordAxisSign\", \"int\", \"Integer\", \"\", 1\n\
         P: \"UnitScaleFactor\", \"double\", \"Number\", \"\", 100\n\
         } }\n",
    );
    let _ = writeln!(
        out,
        "Definitions: {{\n    Version: 100\n    Count: {}",
        runs.len() * 2 + material_names.len()
    );
    let _ = writeln!(
        out,
        "    ObjectType: \"Geometry\" {{ Count: {} }}",
        runs.len()
    );
    let _ = writeln!(out, "    ObjectType: \"Model\" {{ Count: {} }}", runs.len());
    let _ = writeln!(
        out,
        "    ObjectType: \"Material\" {{ Count: {} }}",
        material_names.len()
    );
    out.push_str("}\nObjects: {\n");
    for (run, &(start, end, object)) in runs.iter().enumerate() {
        let object_name = export_name(
            extra
                .objects
                .get(object as usize)
                .map_or("Mesh", String::as_str),
        );
        let geometry_id = run * 2 + 1;
        let model_id = run * 2 + 2;
        let mut local_vertices = Vec::new();
        let mut remap = BTreeMap::new();
        if runs.len() == 1 {
            for v in 0..mesh.vertex_count() {
                remap.insert(v, v);
                local_vertices.push(v);
            }
        } else {
            for f in start..end {
                for c in mesh.face_corners(f) {
                    let v = mesh.corner_vertex(c);
                    if let std::collections::btree_map::Entry::Vacant(entry) = remap.entry(v) {
                        entry.insert(local_vertices.len());
                        local_vertices.push(v);
                    }
                }
            }
        }
        let _ = writeln!(
            out,
            "    Geometry: {geometry_id}, \"Geometry::{object_name}\", \"Mesh\" {{"
        );
        let _ = write!(
            out,
            "        Vertices: *{} {{ a: ",
            local_vertices.len() * 3
        );
        for (i, &v) in local_vertices.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let p = mesh.position(v);
            let _ = write!(out, "{},{},{}", p.x, p.y, p.z);
        }
        out.push_str(" }\n");
        let corners: usize = (start..end).map(|f| mesh.face_corners(f).len()).sum();
        let _ = write!(out, "        PolygonVertexIndex: *{corners} {{ a: ");
        let mut first = true;
        for f in start..end {
            let range = mesh.face_corners(f);
            for c in range.clone() {
                if !first {
                    out.push(',');
                }
                first = false;
                let v = remap[&mesh.corner_vertex(c)] as i64;
                let encoded = if c == range.end - 1 { -v - 1 } else { v };
                let _ = write!(out, "{encoded}");
            }
        }
        out.push_str(" }\n");
        out.push_str("        LayerElementNormal: 0 {\n            MappingInformationType: \"ByPolygonVertex\"\n            ReferenceInformationType: \"Direct\"\n");
        let _ = write!(out, "            Normals: *{} {{ a: ", corners * 3);
        let mut first = true;
        for face in start..end {
            let face_normal = mesh.face_area_normal(face).normalize_or_zero();
            let group = smoothing.get(face).copied().flatten();
            for corner in mesh.face_corners(face) {
                if !first {
                    out.push(',');
                }
                first = false;
                let explicit = extra.corner_normal.get(corner).copied().unwrap_or(u32::MAX);
                let normal = if explicit != u32::MAX {
                    extra
                        .normals
                        .get(explicit as usize)
                        .copied()
                        .unwrap_or(face_normal)
                } else if let Some(group) = group {
                    smooth_normals
                        .get(&(mesh.corner_vertex(corner), group))
                        .copied()
                        .unwrap_or(face_normal)
                        .normalize_or_zero()
                } else {
                    face_normal
                };
                let _ = write!(out, "{},{},{}", normal.x, normal.y, normal.z);
            }
        }
        out.push_str(" }\n        }\n");
        for (channel, layer) in std::iter::once((0, uv)).chain(
            layers
                .iter()
                .filter(|(id, _)| **id != 0)
                .map(|(id, layer)| (*id, layer)),
        ) {
            let _ = writeln!(out, "        LayerElementUV: {channel} {{");
            let _ = writeln!(out, "            Name: \"{}\"", export_name(&layer.name));
            out.push_str("            MappingInformationType: \"ByPolygonVertex\"\n            ReferenceInformationType: \"IndexToDirect\"\n");
            let _ = write!(out, "            UV: *{} {{ a: ", layer.uvs.len() * 2);
            for (i, p) in layer.uvs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{},{}", p.x, p.y);
            }
            out.push_str(" }\n");
            let _ = write!(out, "            UVIndex: *{corners} {{ a: ");
            let mut first = true;
            for f in start..end {
                for c in mesh.face_corners(f) {
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    let _ = write!(out, "{}", layer.corner_uv[c]);
                }
            }
            out.push_str(" }\n        }\n");
        }
        if !material_names.is_empty() {
            out.push_str("        LayerElementMaterial: 0 {\n            MappingInformationType: \"ByPolygon\"\n            ReferenceInformationType: \"IndexToDirect\"\n");
            let _ = write!(out, "            Materials: *{} {{ a: ", end - start);
            for f in start..end {
                if f > start {
                    out.push(',');
                }
                let slot = if materials_valid {
                    let id = extra.face_material[f] as usize;
                    if id < extra.materials.len() {
                        id
                    } else {
                        default_material
                    }
                } else {
                    0
                };
                let _ = write!(out, "{slot}");
            }
            out.push_str(" }\n        }\n");
        }
        out.push_str("        LayerElementSmoothing: 0 {\n            MappingInformationType: \"ByPolygon\"\n            ReferenceInformationType: \"Direct\"\n");
        let _ = write!(out, "            Smoothing: *{} {{ a: ", end - start);
        for face in start..end {
            if face > start {
                out.push(',');
            }
            let _ = write!(
                out,
                "{}",
                smoothing.get(face).copied().flatten().unwrap_or(0)
            );
        }
        out.push_str(" }\n        }\n");
        out.push_str("        Layer: 0 {\n            Version: 100\n            LayerElement: { Type: \"LayerElementNormal\" TypedIndex: 0 }\n            LayerElement: { Type: \"LayerElementUV\" TypedIndex: 0 }\n            LayerElement: { Type: \"LayerElementSmoothing\" TypedIndex: 0 }\n");
        if !material_names.is_empty() {
            out.push_str(
                "            LayerElement: { Type: \"LayerElementMaterial\" TypedIndex: 0 }\n",
            );
        }
        out.push_str("        }\n");
        for &channel in layers.keys().filter(|&&channel| channel != 0) {
            let _ = writeln!(
                out,
                "        Layer: {channel} {{\n            Version: 100\n            LayerElement: {{ Type: \"LayerElementUV\" TypedIndex: {channel} }}\n        }}"
            );
        }
        out.push_str("    }\n");
        let _ = writeln!(
            out,
            "    Model: {model_id}, \"Model::{object_name}\", \"Mesh\" {{}}"
        );
    }
    let material_base = runs.len() * 2 + 1;
    for (i, name) in material_names.iter().enumerate() {
        let _ = writeln!(
            out,
            "    Material: {}, \"Material::{}\", \"\" {{}}",
            material_base + i,
            export_name(name)
        );
    }
    out.push_str("}\nConnections: {\n");
    for run in 0..runs.len() {
        let geometry_id = run * 2 + 1;
        let model_id = run * 2 + 2;
        let _ = writeln!(out, "    C: \"OO\",{geometry_id},{model_id}");
        let _ = writeln!(out, "    C: \"OO\",{model_id},0");
        for i in 0..material_names.len() {
            let _ = writeln!(out, "    C: \"OO\",{},{model_id}", material_base + i);
        }
    }
    out.push_str("}\n");
    Ok(out.into_bytes())
}

fn blocks<'a>(text: &'a str, label: &str) -> Result<Vec<(&'a str, &'a str)>, FbxError> {
    let mut found = Vec::new();
    for (start, line) in text.match_indices(label) {
        let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
        if !text[line_start..start].trim().is_empty() {
            continue;
        }
        let header_end = start + line.len();
        let Some(open) = text[header_end..].find('{').map(|n| header_end + n) else {
            return Err(FbxError::Parse(format!("{label} has no body")));
        };
        let close = matching_brace(text, open)?;
        found.push((&text[header_end..open], &text[open + 1..close]));
    }
    Ok(found)
}

fn matching_brace(text: &str, open: usize) -> Result<usize, FbxError> {
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for (offset, &byte) in text.as_bytes()[open..].iter().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else if byte == b'"' {
            quoted = true;
        } else if byte == b'{' {
            depth += 1;
        } else if byte == b'}' {
            depth -= 1;
            if depth == 0 {
                return Ok(open + offset);
            }
        }
    }
    Err(FbxError::Parse("unclosed FBX block".into()))
}

fn array<T: std::str::FromStr>(body: &str, key: &str) -> Result<Option<Vec<T>>, FbxError> {
    let label = format!("{key}:");
    let Some((start, _)) = body
        .match_indices(&label)
        .find(|(i, _)| *i == 0 || matches!(body.as_bytes()[i - 1], b'\n' | b'\r' | b' ' | b'\t'))
    else {
        return Ok(None);
    };
    let after = &body[start + label.len()..];
    let Some(open_rel) = after.find('{') else {
        return Err(FbxError::Parse(format!("{key} array has no body")));
    };
    let open = start + label.len() + open_rel;
    let close = matching_brace(body, open)?;
    let content = &body[open + 1..close];
    let Some((_, values)) = content.split_once("a:") else {
        return Err(FbxError::Parse(format!("{key} array has no a: values")));
    };
    values
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<T>()
                .map_err(|_| FbxError::Parse(format!("invalid {key} value {s}")))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn string_field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let (_, rest) = body.split_once(&format!("{key}:"))?;
    rest.split('"').nth(1)
}

fn record_name(header: &str) -> String {
    header
        .split('"')
        .nth(1)
        .unwrap_or("mesh")
        .split("::")
        .last()
        .unwrap_or("mesh")
        .to_owned()
}

fn record_id(header: &str) -> Option<i64> {
    header.split(',').next()?.trim().parse().ok()
}

fn active_uv_index(geometry_body: &str) -> Option<u32> {
    for (header, body) in blocks(geometry_body, "Layer:").ok()? {
        if record_id(header) != Some(0) {
            continue;
        }
        for (_, element) in blocks(body, "LayerElement:").ok()? {
            if string_field(element, "Type") != Some("LayerElementUV") {
                continue;
            }
            let (_, rest) = element.split_once("TypedIndex:")?;
            return rest
                .trim_start()
                .split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok();
        }
    }
    None
}

fn safe_name(name: &str) -> String {
    name.replace(['\n', '\r'], " ")
}

fn property_values<'a>(body: &'a str, name: &str) -> Option<Vec<&'a str>> {
    body.lines().find_map(|line| {
        let (_, values) = line.split_once("P:")?;
        let values = values.trim();
        let fields: Vec<&str> = values.split(',').map(str::trim).collect();
        if fields.first().copied() == Some(format!("\"{name}\"").as_str()) {
            Some(fields.into_iter().skip(4).collect())
        } else {
            None
        }
    })
}

fn property_vec(body: &str, name: &str, default: DVec3) -> DVec3 {
    let Some(values) = property_values(body, name) else {
        return default;
    };
    if values.len() < 3 {
        return default;
    }
    let parse = |s: &str| s.trim_end_matches('}').trim().parse::<f64>();
    let Ok(x) = parse(values[0]) else {
        return default;
    };
    let Ok(y) = parse(values[1]) else {
        return default;
    };
    let Ok(z) = parse(values[2]) else {
        return default;
    };
    DVec3::new(x, y, z)
}

fn property_number(body: &str, name: &str) -> Option<f64> {
    property_values(body, name)?
        .first()?
        .trim_end_matches('}')
        .trim()
        .parse()
        .ok()
}

#[derive(Clone, Copy)]
struct SceneCoordinates {
    axes: [(usize, f64); 3],
    scale: f64,
}

impl SceneCoordinates {
    fn from_text(text: &str) -> Result<Self, FbxError> {
        let Some((_, body)) = blocks(text, "GlobalSettings:")?.into_iter().next() else {
            return Ok(Self {
                axes: [(0, 1.0), (1, 1.0), (2, 1.0)],
                scale: 1.0,
            });
        };
        let axis = |name: &str, default: usize| {
            property_number(body, name)
                .and_then(|v| (0.0..=2.0).contains(&v).then_some(v as usize))
                .unwrap_or(default)
        };
        let sign = |name: &str| {
            if property_number(body, name) == Some(-1.0) {
                -1.0
            } else {
                1.0
            }
        };
        let axes = [
            (axis("CoordAxis", 0), sign("CoordAxisSign")),
            (axis("UpAxis", 1), sign("UpAxisSign")),
            (axis("FrontAxis", 2), sign("FrontAxisSign")),
        ];
        if axes[0].0 == axes[1].0 || axes[0].0 == axes[2].0 || axes[1].0 == axes[2].0 {
            return Err(FbxError::Parse("invalid scene axis permutation".into()));
        }
        let scale = property_number(body, "UnitScaleFactor").map_or(1.0, |cm| cm / 100.0);
        if !scale.is_finite() || scale <= 0.0 {
            return Err(FbxError::Parse("invalid UnitScaleFactor".into()));
        }
        Ok(Self { axes, scale })
    }

    fn convert(self, p: DVec3) -> DVec3 {
        let a = p.to_array();
        DVec3::new(
            a[self.axes[0].0] * self.axes[0].1 * self.scale,
            a[self.axes[1].0] * self.axes[1].1 * self.scale,
            a[self.axes[2].0] * self.axes[2].1 * self.scale,
        )
    }
}

fn rotation(angles: DVec3, order: u32) -> DMat4 {
    let [x, y, z] = angles.to_array().map(f64::to_radians);
    let (rx, ry, rz) = (
        DMat4::from_rotation_x(x),
        DMat4::from_rotation_y(y),
        DMat4::from_rotation_z(z),
    );
    match order {
        1 => rx * rz * ry,
        2 => ry * rz * rx,
        3 => ry * rx * rz,
        4 => rz * rx * ry,
        5 => rz * ry * rx,
        _ => rx * ry * rz,
    }
}

#[derive(Clone, Copy)]
struct ModelMatrices {
    local: DMat4,
    local_rotation: DMat4,
    local_scale: DMat4,
    geometric: DMat4,
    inherit: u32,
}

fn model_matrices(body: &str) -> ModelMatrices {
    let zero = DVec3::ZERO;
    let one = DVec3::ONE;
    let order = property_values(body, "RotationOrder")
        .and_then(|v| v.first().and_then(|s| s.parse::<u32>().ok()))
        .unwrap_or(0);
    let t = |name, default| DMat4::from_translation(property_vec(body, name, default));
    let r = |name| rotation(property_vec(body, name, zero), order);
    let scale = |name| DMat4::from_scale(property_vec(body, name, one));
    let rp = property_vec(body, "RotationPivot", zero);
    let sp = property_vec(body, "ScalingPivot", zero);
    let local_rotation = r("PreRotation") * r("Lcl Rotation") * r("PostRotation");
    let local_scale = scale("Lcl Scaling");
    // Autodesk FBX SDK node transform order, with static property values.
    let local = t("Lcl Translation", zero)
        * t("RotationOffset", zero)
        * DMat4::from_translation(rp)
        * local_rotation
        * DMat4::from_translation(-rp)
        * t("ScalingOffset", zero)
        * DMat4::from_translation(sp)
        * local_scale
        * DMat4::from_translation(-sp);
    let geometric =
        t("GeometricTranslation", zero) * r("GeometricRotation") * scale("GeometricScaling");
    ModelMatrices {
        local,
        local_rotation,
        local_scale,
        geometric,
        inherit: property_number(body, "InheritType").unwrap_or(0.0) as u32,
    }
}

fn world_matrix(
    id: i64,
    matrices: &BTreeMap<i64, ModelMatrices>,
    parents: &BTreeMap<i64, i64>,
    visiting: &mut Vec<i64>,
) -> Result<(DMat4, DMat4), FbxError> {
    if visiting.contains(&id) {
        return Err(FbxError::Parse("cyclic Model hierarchy".into()));
    }
    visiting.push(id);
    let current = matrices.get(&id).copied().unwrap_or(ModelMatrices {
        local: DMat4::IDENTITY,
        local_rotation: DMat4::IDENTITY,
        local_scale: DMat4::IDENTITY,
        geometric: DMat4::IDENTITY,
        inherit: 0,
    });
    let parent = parents.get(&id).copied();
    let world = if let Some(parent) = parent {
        let (above, parent_rotation) = world_matrix(parent, matrices, parents, visiting)?;
        let mut parent_linear = above;
        parent_linear.w_axis = DVec4::W;
        let parent_shear_scale = parent_rotation.inverse() * parent_linear;
        let global_translation =
            above.transform_point3(current.local.transform_point3(DVec3::ZERO));
        let global_rs = match current.inherit {
            // Parent scale follows child rotation (RrSs, the FBX default).
            0 => {
                parent_rotation * current.local_rotation * parent_shear_scale * current.local_scale
            }
            // Parent scale precedes child rotation (RSrs).
            1 => {
                parent_rotation * parent_shear_scale * current.local_rotation * current.local_scale
            }
            // Parent's local scale does not affect child's own scale (Rrs).
            2 => {
                let parent_scale = matrices
                    .get(&parent)
                    .map_or(DMat4::IDENTITY, |m| m.local_scale);
                if parent_scale.determinant().abs() < 1e-15 {
                    return Err(FbxError::Parse(
                        "Rrs parent has singular local scale".into(),
                    ));
                }
                parent_rotation
                    * current.local_rotation
                    * parent_shear_scale
                    * parent_scale.inverse()
                    * current.local_scale
            }
            _ => return Err(FbxError::Parse("unsupported InheritType".into())),
        };
        let world = DMat4::from_translation(global_translation) * global_rs;
        (world, parent_rotation * current.local_rotation)
    } else {
        (current.local, current.local_rotation)
    };
    visiting.pop();
    Ok(world)
}

#[derive(Debug)]
enum BinProp {
    Scalar(String),
    Array(Vec<String>),
}

#[derive(Debug)]
struct BinNode {
    name: String,
    props: Vec<BinProp>,
    children: Vec<BinNode>,
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], FbxError> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| FbxError::Parse("binary offset overflow".into()))?;
        let bytes = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| FbxError::Parse("truncated binary FBX".into()))?;
        self.at = end;
        Ok(bytes)
    }
    fn u8(&mut self) -> Result<u8, FbxError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, FbxError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, FbxError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn count(&mut self, wide: bool) -> Result<usize, FbxError> {
        usize::try_from(if wide {
            self.u64()?
        } else {
            self.u32()? as u64
        })
        .map_err(|_| FbxError::Parse("binary count too large".into()))
    }
}

fn bin_property(c: &mut Cursor<'_>) -> Result<BinProp, FbxError> {
    let code = c.u8()?;
    let scalar = match code {
        b'Y' => Some(i16::from_le_bytes(c.take(2)?.try_into().unwrap()).to_string()),
        b'C' => Some(u8::from(c.u8()? != 0).to_string()),
        b'I' => Some(i32::from_le_bytes(c.take(4)?.try_into().unwrap()).to_string()),
        b'L' => Some(i64::from_le_bytes(c.take(8)?.try_into().unwrap()).to_string()),
        b'F' => Some(f32::from_le_bytes(c.take(4)?.try_into().unwrap()).to_string()),
        b'D' => Some(f64::from_le_bytes(c.take(8)?.try_into().unwrap()).to_string()),
        b'S' | b'R' => {
            let len = c.u32()? as usize;
            let bytes = c.take(len)?;
            Some(if code == b'S' {
                format!(
                    "\"{}\"",
                    String::from_utf8_lossy(bytes).replace('"', "\\\"")
                )
            } else {
                "\"\"".into()
            })
        }
        _ => None,
    };
    if let Some(s) = scalar {
        return Ok(BinProp::Scalar(s));
    }
    let width = match code {
        b'd' | b'l' => 8,
        b'f' | b'i' => 4,
        b'b' | b'c' => 1,
        _ => {
            return Err(FbxError::Parse(format!(
                "unsupported binary property {code:#x}"
            )));
        }
    };
    let count = c.u32()? as usize;
    let encoding = c.u32()?;
    let encoded_len = c.u32()? as usize;
    let encoded = c.take(encoded_len)?;
    let expected_len = count
        .checked_mul(width)
        .ok_or_else(|| FbxError::Parse("binary array length overflow".into()))?;
    let decoded;
    let bytes: &[u8] = match encoding {
        0 => encoded,
        1 => {
            let mut decoder = ZlibDecoder::new(encoded).take(expected_len as u64 + 1);
            decoded = {
                let mut out = Vec::new();
                decoder
                    .read_to_end(&mut out)
                    .map_err(|e| FbxError::Parse(format!("invalid compressed FBX array: {e}")))?;
                out
            };
            &decoded
        }
        _ => return Err(FbxError::Parse("unsupported binary array encoding".into())),
    };
    if bytes.len() != expected_len {
        return Err(FbxError::Parse("binary array length mismatch".into()));
    }
    let values = bytes
        .chunks_exact(width)
        .map(|part| match code {
            b'd' => f64::from_le_bytes(part.try_into().unwrap()).to_string(),
            b'f' => f32::from_le_bytes(part.try_into().unwrap()).to_string(),
            b'l' => i64::from_le_bytes(part.try_into().unwrap()).to_string(),
            b'i' => i32::from_le_bytes(part.try_into().unwrap()).to_string(),
            _ => part[0].to_string(),
        })
        .collect();
    Ok(BinProp::Array(values))
}

fn bin_node(
    c: &mut Cursor<'_>,
    wide: bool,
    limit: usize,
    depth: usize,
) -> Result<Option<BinNode>, FbxError> {
    if depth > 128 {
        return Err(FbxError::Parse("binary FBX nesting too deep".into()));
    }
    let end = c.count(wide)?;
    let props = c.count(wide)?;
    let prop_len = c.count(wide)?;
    let name_len = c.u8()? as usize;
    if end == 0 && props == 0 && prop_len == 0 && name_len == 0 {
        return Ok(None);
    }
    if end > limit || end <= c.at || props > 1_000_000 {
        return Err(FbxError::Parse("invalid binary node offset/count".into()));
    }
    let name = String::from_utf8_lossy(c.take(name_len)?).into_owned();
    let prop_end =
        c.at.checked_add(prop_len)
            .ok_or_else(|| FbxError::Parse("binary property overflow".into()))?;
    if prop_end > end {
        return Err(FbxError::Parse("binary properties exceed node".into()));
    }
    let mut values = Vec::with_capacity(props);
    for _ in 0..props {
        values.push(bin_property(c)?);
    }
    if c.at != prop_end {
        return Err(FbxError::Parse(
            "binary property byte count mismatch".into(),
        ));
    }
    let mut children = Vec::new();
    while c.at < end {
        let before = c.at;
        if let Some(node) = bin_node(c, wide, end, depth + 1)? {
            children.push(node);
        }
        if c.at <= before {
            return Err(FbxError::Parse("binary parser did not advance".into()));
        }
    }
    if c.at != end {
        return Err(FbxError::Parse("binary node end mismatch".into()));
    }
    Ok(Some(BinNode {
        name,
        props: values,
        children,
    }))
}

fn emit_node(out: &mut String, node: &BinNode, indent: usize) {
    let pad = " ".repeat(indent);
    out.push_str(&pad);
    out.push_str(&node.name);
    out.push(':');
    let mut first = true;
    for prop in &node.props {
        if first {
            out.push(' ');
            first = false;
        } else {
            out.push_str(", ");
        }
        match prop {
            BinProp::Scalar(s) => out.push_str(s),
            BinProp::Array(values) => out.push_str(&format!("*{}", values.len())),
        }
    }
    if node.children.is_empty()
        && node.props.iter().all(|p| !matches!(p, BinProp::Array(_)))
        && node.name != "Material"
        && node.name != "Model"
    {
        out.push('\n');
        return;
    }
    out.push_str(" {\n");
    for prop in &node.props {
        if let BinProp::Array(values) = prop {
            out.push_str(&pad);
            out.push_str("    a: ");
            out.push_str(&values.join(","));
            out.push('\n');
        }
    }
    for child in &node.children {
        emit_node(out, child, indent + 4);
    }
    out.push_str(&pad);
    out.push_str("}\n");
}

fn binary_to_ascii(bytes: &[u8]) -> Result<String, FbxError> {
    const MAGIC: &[u8] = b"Kaydara FBX Binary  \0\x1a\0";
    if !bytes.starts_with(MAGIC) {
        return Err(FbxError::Parse("invalid binary FBX magic".into()));
    }
    let mut c = Cursor {
        bytes,
        at: MAGIC.len(),
    };
    let version = c.u32()?;
    if !(7400..=7700).contains(&version) {
        return Err(FbxError::BinaryVersion(version));
    }
    let wide = version >= 7500;
    let mut out = format!("; FBX {version} binary converted internally\n");
    while let Some(node) = bin_node(&mut c, wide, bytes.len(), 0)? {
        emit_node(&mut out, &node, 0);
    }
    Ok(out)
}

/// Read an FBX 7.x ASCII or binary file. Static Model transforms and geometry
/// instances are baked into positions; animation and constraints are ignored.
pub fn parse(bytes: &[u8]) -> Result<ObjDocument, FbxError> {
    let binary_text;
    let text = if bytes.starts_with(b"Kaydara FBX Binary") {
        binary_text = binary_to_ascii(bytes)?;
        binary_text.as_str()
    } else {
        std::str::from_utf8(bytes)?
    };
    let geometries = blocks(text, "Geometry:")?;
    if geometries.is_empty() {
        return Err(FbxError::Parse("no Geometry mesh records".into()));
    }
    let coordinates = SceneCoordinates::from_text(text)?;
    let materials = blocks(text, "Material:")?;
    let material_names: Vec<String> = materials
        .iter()
        .map(|(header, _)| safe_name(&record_name(header)))
        .collect();
    let material_by_id: BTreeMap<i64, String> = materials
        .iter()
        .filter_map(|(header, _)| Some((record_id(header)?, safe_name(&record_name(header)))))
        .collect();
    let models = blocks(text, "Model:")?;
    let model_names: BTreeMap<i64, String> = models
        .iter()
        .filter_map(|(header, _)| Some((record_id(header)?, safe_name(&record_name(header)))))
        .collect();
    let model_transforms: BTreeMap<i64, ModelMatrices> = models
        .iter()
        .filter_map(|(header, body)| Some((record_id(header)?, model_matrices(body))))
        .collect();
    let geometry_ids: Vec<i64> = geometries
        .iter()
        .filter_map(|(header, _)| record_id(header))
        .collect();
    let mut geometry_models: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    let mut model_parents = BTreeMap::new();
    let mut model_materials: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for (_, body) in blocks(text, "Connections:")? {
        for line in body.lines() {
            let Some((_, values)) = line.trim().split_once("C:") else {
                continue;
            };
            let mut fields = values.split(',').map(str::trim);
            if fields.next() != Some("\"OO\"") {
                continue;
            }
            let (Some(child), Some(parent)) = (
                fields.next().and_then(|s| s.parse::<i64>().ok()),
                fields.next().and_then(|s| s.parse::<i64>().ok()),
            ) else {
                continue;
            };
            if let Some(material) = material_by_id.get(&child) {
                model_materials
                    .entry(parent)
                    .or_default()
                    .push(material.clone());
            } else if model_names.contains_key(&parent) && geometry_ids.contains(&child) {
                geometry_models.entry(child).or_default().push(parent);
            } else if model_names.contains_key(&parent) && model_names.contains_key(&child) {
                model_parents.insert(child, parent);
            }
        }
    }
    let mut obj = String::from("# Imported from FBX by UvCraft\n");
    let mut vertex_offset = 0usize;
    let mut vt_count = 0usize;
    let mut mesh_count = 0usize;
    for (header, body) in geometries {
        if !header.contains("\"Mesh\"") {
            continue;
        }
        let vertices: Vec<f64> = array(body, "Vertices")?
            .ok_or_else(|| FbxError::Parse("mesh has no Vertices".into()))?;
        let indices: Vec<i64> = array(body, "PolygonVertexIndex")?
            .ok_or_else(|| FbxError::Parse("mesh has no PolygonVertexIndex".into()))?;
        if !vertices.len().is_multiple_of(3) || vertices.is_empty() {
            return Err(FbxError::Parse("Vertices needs XYZ triples".into()));
        }
        if vertices.iter().any(|v| !v.is_finite()) {
            return Err(FbxError::Parse("non-finite vertex".into()));
        }
        let count = vertices.len() / 3;
        let uv_layers = blocks(body, "LayerElementUV:")?;
        let active = active_uv_index(body);
        let uv_layer = active
            .and_then(|index| {
                uv_layers
                    .iter()
                    .find(|(header, _)| record_id(header) == Some(index as i64))
            })
            .or_else(|| uv_layers.first())
            .map(|(_, b)| *b);
        let uv_values = uv_layer
            .map(|b| array::<f64>(b, "UV"))
            .transpose()?
            .flatten()
            .unwrap_or_default();
        let uv_indices = uv_layer
            .map(|b| array::<usize>(b, "UVIndex"))
            .transpose()?
            .flatten()
            .unwrap_or_default();
        if uv_values.len() % 2 != 0 {
            return Err(FbxError::Parse("UV needs pairs".into()));
        }
        let uv_mapping = uv_layer
            .and_then(|b| string_field(b, "MappingInformationType"))
            .unwrap_or("ByPolygonVertex");
        let uv_reference = uv_layer
            .and_then(|b| string_field(b, "ReferenceInformationType"))
            .unwrap_or("Direct");
        let material_layer = blocks(body, "LayerElementMaterial:")?
            .into_iter()
            .next()
            .map(|(_, b)| b);
        let material_values = material_layer
            .map(|b| array::<usize>(b, "Materials"))
            .transpose()?
            .flatten()
            .unwrap_or_default();
        let material_mapping = material_layer
            .and_then(|b| string_field(b, "MappingInformationType"))
            .unwrap_or("AllSame");
        let connected = record_id(header)
            .and_then(|id| geometry_models.get(&id))
            .cloned()
            .unwrap_or_default();
        let instances: Vec<Option<i64>> = if connected.is_empty() {
            vec![None]
        } else {
            connected.into_iter().map(Some).collect()
        };
        for model_id in instances {
            let transform = if let Some(id) = model_id {
                let (world, _) =
                    world_matrix(id, &model_transforms, &model_parents, &mut Vec::new())?;
                let geometric = model_transforms
                    .get(&id)
                    .map_or(DMat4::IDENTITY, |m| m.geometric);
                world * geometric
            } else {
                DMat4::IDENTITY
            };
            for point in vertices.as_chunks::<3>().0 {
                let p = coordinates
                    .convert(transform.transform_point3(DVec3::new(point[0], point[1], point[2])));
                obj += &format!("v {} {} {}\n", p.x, p.y, p.z);
            }
            let object_name = model_id
                .and_then(|id| model_names.get(&id))
                .cloned()
                .unwrap_or_else(|| safe_name(&record_name(header)));
            let mesh_materials = model_id
                .and_then(|id| model_materials.get(&id))
                .filter(|names| !names.is_empty())
                .unwrap_or(&material_names);
            obj += &format!("o {object_name}\n");
            mesh_count += 1;
            let mut polygon = Vec::new();
            let mut face = 0usize;
            let mut current_material: Option<usize> = None;
            let mut vt_map: BTreeMap<(usize, usize), usize> = BTreeMap::new();
            for (corner, &encoded) in indices.iter().enumerate() {
                let end = encoded < 0;
                let point = if end {
                    encoded.checked_neg().and_then(|n| n.checked_sub(1))
                } else {
                    Some(encoded)
                }
                .ok_or_else(|| FbxError::Parse("invalid polygon vertex index".into()))?
                    as usize;
                if point >= count {
                    return Err(FbxError::Parse(format!(
                        "polygon vertex {point} out of range"
                    )));
                }
                let global = vertex_offset + point;
                let uv_slot = match uv_mapping {
                    "ByPolygonVertex" => corner,
                    "ByVertice" | "ByVertex" => point,
                    "ByPolygon" => face,
                    "AllSame" => 0,
                    other => {
                        return Err(FbxError::Parse(format!("unsupported UV mapping {other}")));
                    }
                };
                let vt = if uv_layer.is_some() {
                    let direct = match uv_reference {
                        "IndexToDirect" => *uv_indices
                            .get(uv_slot)
                            .ok_or_else(|| FbxError::Parse("UVIndex too short".into()))?,
                        "Direct" => uv_slot,
                        other => {
                            return Err(FbxError::Parse(format!(
                                "unsupported UV reference {other}"
                            )));
                        }
                    };
                    let xy = uv_values
                        .get(direct * 2..direct * 2 + 2)
                        .ok_or_else(|| FbxError::Parse("UV index out of range".into()))?;
                    let key = (global, direct);
                    Some(*vt_map.entry(key).or_insert_with(|| {
                        vt_count += 1;
                        obj += &format!("vt {} {}\n", xy[0], xy[1]);
                        vt_count
                    }))
                } else {
                    None
                };
                polygon.push(if let Some(vt) = vt {
                    format!("{}/{}", global + 1, vt)
                } else {
                    (global + 1).to_string()
                });
                if end {
                    if polygon.len() < 3 {
                        return Err(FbxError::Parse("polygon has fewer than 3 corners".into()));
                    }
                    let material_slot = match material_mapping {
                        "ByPolygon" => face,
                        "AllSame" => 0,
                        _ => face,
                    };
                    let next_material = material_values.get(material_slot).copied();
                    if next_material != current_material {
                        if let Some(name) = next_material.and_then(|i| mesh_materials.get(i)) {
                            obj += &format!("usemtl {}\n", name);
                        }
                        current_material = next_material;
                    }
                    obj += &format!("f {}\n", polygon.join(" "));
                    polygon.clear();
                    face += 1;
                }
            }
            if !polygon.is_empty() {
                return Err(FbxError::Parse("unterminated polygon".into()));
            }
            vertex_offset += count;
        }
    }
    if mesh_count == 0 {
        return Err(FbxError::Parse("no mesh Geometry records".into()));
    }
    obj::parse(obj.into_bytes()).map_err(Into::into)
}

/// FBX mesh plus every UV layer, indexed by LayerElementUV ID. Layer zero is
/// the active OBJ-backed layer.
pub struct FbxLayersDocument {
    pub document: ObjDocument,
    pub layers: BTreeMap<u32, UvLayer>,
}

/// Parse all UV layers for static mesh import. Each layer has one UV index per
/// mesh corner, including expanded Geometry instances and object runs.
pub fn parse_layers(bytes: &[u8]) -> Result<FbxLayersDocument, FbxError> {
    let document = parse(bytes)?;
    let binary_text;
    let text = if bytes.starts_with(b"Kaydara FBX Binary") {
        binary_text = binary_to_ascii(bytes)?;
        binary_text.as_str()
    } else {
        std::str::from_utf8(bytes)?
    };
    let geometries = blocks(text, "Geometry:")?;
    let active_layer = geometries
        .iter()
        .find_map(|(_, body)| active_uv_index(body));
    let model_ids: Vec<i64> = blocks(text, "Model:")?
        .iter()
        .filter_map(|(header, _)| record_id(header))
        .collect();
    let geometry_ids: Vec<i64> = geometries
        .iter()
        .filter_map(|(header, _)| record_id(header))
        .collect();
    let mut instance_count: BTreeMap<i64, usize> = BTreeMap::new();
    for (_, body) in blocks(text, "Connections:")? {
        for line in body.lines() {
            let Some((_, values)) = line.split_once("C:") else {
                continue;
            };
            let mut fields = values.split(',').map(str::trim);
            if fields.next() != Some("\"OO\"") {
                continue;
            }
            let (Some(child), Some(parent)) = (
                fields.next().and_then(|s| s.parse::<i64>().ok()),
                fields.next().and_then(|s| s.parse::<i64>().ok()),
            ) else {
                continue;
            };
            if geometry_ids.contains(&child) && model_ids.contains(&parent) {
                *instance_count.entry(child).or_default() += 1;
            }
        }
    }
    struct LayerData {
        name: String,
        uvs: Vec<DVec2>,
        corner_uv: Vec<u32>,
        remap: BTreeMap<(usize, usize), u32>,
    }
    struct LayerDesc {
        id: u32,
        name: String,
        values: Vec<f64>,
        indices: Vec<usize>,
        mapping: String,
        reference: String,
    }
    let mut built: BTreeMap<u32, LayerData> = BTreeMap::new();
    let mut global_vertex = 0usize;
    let mut global_corner = 0usize;
    for (header, body) in geometries {
        if !header.contains("\"Mesh\"") {
            continue;
        }
        let vertices: Vec<f64> = array(body, "Vertices")?
            .ok_or_else(|| FbxError::Parse("mesh has no Vertices".into()))?;
        let indices: Vec<i64> = array(body, "PolygonVertexIndex")?
            .ok_or_else(|| FbxError::Parse("mesh has no PolygonVertexIndex".into()))?;
        let vertex_count = vertices.len() / 3;
        let mut descs = Vec::new();
        for (layer_header, layer_body) in blocks(body, "LayerElementUV:")? {
            let Some(id) = record_id(layer_header).and_then(|i| u32::try_from(i).ok()) else {
                continue;
            };
            let values: Vec<f64> = array(layer_body, "UV")?.unwrap_or_default();
            if !values.len().is_multiple_of(2) {
                return Err(FbxError::Parse("UV needs pairs".into()));
            }
            descs.push(LayerDesc {
                id,
                name: string_field(layer_body, "Name")
                    .map_or_else(|| format!("UVSet{id}"), str::to_owned),
                values,
                indices: array(layer_body, "UVIndex")?.unwrap_or_default(),
                mapping: string_field(layer_body, "MappingInformationType")
                    .unwrap_or("ByPolygonVertex")
                    .into(),
                reference: string_field(layer_body, "ReferenceInformationType")
                    .unwrap_or("Direct")
                    .into(),
            });
        }
        let count = record_id(header)
            .and_then(|id| instance_count.get(&id).copied())
            .unwrap_or(0)
            .max(1);
        for _ in 0..count {
            for (&id, layer) in &mut built {
                if !descs.iter().any(|desc| desc.id == id) {
                    layer
                        .corner_uv
                        .extend(std::iter::repeat_n(0, indices.len()));
                }
            }
            for desc in &descs {
                let layer = built.entry(desc.id).or_insert_with(|| LayerData {
                    name: desc.name.clone(),
                    uvs: vec![DVec2::ZERO],
                    corner_uv: vec![0; global_corner],
                    remap: BTreeMap::new(),
                });
                let mut face = 0usize;
                for (corner, &encoded) in indices.iter().enumerate() {
                    let point = if encoded < 0 { -encoded - 1 } else { encoded } as usize;
                    if point >= vertex_count {
                        return Err(FbxError::Parse("polygon vertex out of range".into()));
                    }
                    let slot = match desc.mapping.as_str() {
                        "ByPolygonVertex" => corner,
                        "ByVertice" | "ByVertex" => point,
                        "ByPolygon" => face,
                        "AllSame" => 0,
                        other => {
                            return Err(FbxError::Parse(format!("unsupported UV mapping {other}")));
                        }
                    };
                    let direct = match desc.reference.as_str() {
                        "IndexToDirect" => *desc
                            .indices
                            .get(slot)
                            .ok_or_else(|| FbxError::Parse("UVIndex too short".into()))?,
                        "Direct" => slot,
                        other => {
                            return Err(FbxError::Parse(format!(
                                "unsupported UV reference {other}"
                            )));
                        }
                    };
                    let xy = desc
                        .values
                        .get(direct * 2..direct * 2 + 2)
                        .ok_or_else(|| FbxError::Parse("UV index out of range".into()))?;
                    let key = (global_vertex + point, direct);
                    let uv_index = *layer.remap.entry(key).or_insert_with(|| {
                        let next = layer.uvs.len() as u32;
                        layer.uvs.push(DVec2::new(xy[0], xy[1]));
                        next
                    });
                    layer.corner_uv.push(uv_index);
                    if encoded < 0 {
                        face += 1;
                    }
                }
            }
            global_corner += indices.len();
            global_vertex += vertex_count;
        }
    }
    if global_corner != document.mesh.corner_count() {
        return Err(FbxError::Parse(
            "UV layer corner count differs from mesh".into(),
        ));
    }
    let mut layers: BTreeMap<u32, UvLayer> = built
        .into_iter()
        .map(|(id, data)| {
            (
                id,
                UvLayer {
                    name: data.name,
                    uvs: data.uvs,
                    corner_uv: data.corner_uv,
                },
            )
        })
        .collect();
    if let Some(active) = active_layer.filter(|&index| index != 0) {
        let previous_zero = layers.remove(&0);
        layers.remove(&active);
        if let Some(previous_zero) = previous_zero {
            layers.insert(active, previous_zero);
        }
    }
    layers.insert(0, document.uv.clone());
    for layer in layers.values() {
        layer.validate(&document.mesh)?;
    }
    Ok(FbxLayersDocument { document, layers })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;

    const QUAD: &str = r#"; FBX 7.4.0 project file
Geometry: 1, "Geometry::Quad", "Mesh" {
    Vertices: *12 { a: 0,0,0,1,0,0,1,1,0,0,1,0 }
    PolygonVertexIndex: *4 { a: 0,1,2,-4 }
    LayerElementUV: 0 {
        MappingInformationType: "ByPolygonVertex"
        ReferenceInformationType: "IndexToDirect"
        UV: *8 { a: 0,0,1,0,1,1,0,1 }
        UVIndex: *4 { a: 0,1,2,3 }
    }
    LayerElementMaterial: 0 { MappingInformationType: "AllSame"
        Materials: *1 { a: 0 }
    }
}
Material: 2, "Material::Red", "" {}
"#;

    #[test]
    fn imports_ascii_quad_uvs_material_and_order() {
        let doc = parse(QUAD.as_bytes()).unwrap();
        assert_eq!(doc.mesh.vertex_count(), 4);
        assert_eq!(doc.mesh.face_count(), 1);
        assert_eq!(doc.objects, ["Quad"]);
        assert_eq!(doc.face_object, [0]);
        assert_eq!(doc.uv.uv_at_corner(2).to_array(), [1.0, 1.0]);
        let out = String::from_utf8(doc.source.export(&doc.mesh, &doc.uv).unwrap()).unwrap();
        assert!(out.contains("usemtl Red"));
        assert!(out.contains("f 1/1 2/2 3/3 4/4"));
    }

    #[test]
    fn imports_model_hierarchy_and_geometric_transform() {
        let scene = format!(
            "{QUAD}\nModel: 10, \"Model::Parent\", \"Null\" {{\n\
             Properties70: {{ P: \"Lcl Translation\", \"Lcl Translation\", \"\", \"A\", 10,0,0\n }}\n\
             }}\nModel: 11, \"Model::Child\", \"Mesh\" {{\n\
             Properties70: {{\n\
             P: \"Lcl Translation\", \"Lcl Translation\", \"\", \"A\", 0,2,0\n\
             P: \"Lcl Rotation\", \"Lcl Rotation\", \"\", \"A\", 0,0,90\n\
             P: \"GeometricTranslation\", \"Vector3D\", \"\", \"A\", 0,0,3\n\
             }}\n}}\nConnections: {{\n\
             C: \"OO\",1,11\nC: \"OO\",11,10\nC: \"OO\",2,11\n}}\n"
        );
        let doc = parse(scene.as_bytes()).unwrap();
        assert_eq!(doc.objects, ["Child"]);
        assert_eq!(doc.mesh.face_count(), 1);
        let p0 = doc.mesh.position(0);
        let p1 = doc.mesh.position(1);
        assert!((p0 - DVec3::new(10.0, 2.0, 3.0)).length() < 1e-9);
        assert!((p1 - DVec3::new(10.0, 3.0, 3.0)).length() < 1e-9);
    }

    #[test]
    fn respects_static_fbx_inheritance_modes() {
        for (mode, expected, expected_diagonal) in [
            (0, DVec3::new(0.0, 2.0, 0.0), DVec3::new(-1.0, 2.0, 0.0)),
            (1, DVec3::new(0.0, 1.0, 0.0), DVec3::new(-2.0, 1.0, 0.0)),
            (2, DVec3::new(0.0, 1.0, 0.0), DVec3::new(-1.0, 1.0, 0.0)),
        ] {
            let scene = format!(
                "{QUAD}\nModel: 10, \"Model::Parent\", \"Null\" {{\n\
                 Properties70: {{\nP: \"Lcl Scaling\", \"Lcl Scaling\", \"\", \"A\", 2,1,1\n}}\n\
                 }}\nModel: 11, \"Model::Child\", \"Mesh\" {{\n\
                 Properties70: {{\nP: \"Lcl Rotation\", \"Lcl Rotation\", \"\", \"A\", 0,0,90\n\
                 P: \"InheritType\", \"enum\", \"\", \"\", {mode}\n}}\n\
                 }}\nConnections: {{\nC: \"OO\",1,11\nC: \"OO\",11,10\n}}\n"
            );
            let doc = parse(scene.as_bytes()).unwrap();
            let actual = doc.mesh.position(1);
            assert!(
                (actual - expected).length() < 1e-9,
                "mode {mode}: {actual:?}"
            );
            let diagonal = doc.mesh.position(2);
            assert!(
                (diagonal - expected_diagonal).length() < 1e-9,
                "mode {mode}: {diagonal:?}"
            );
        }
    }

    #[test]
    fn imports_shared_geometry_as_two_ordered_instances() {
        let scene = format!(
            "{QUAD}\nModel: 10, \"Model::First\", \"Mesh\" {{}}\n\
             Model: 11, \"Model::Second\", \"Mesh\" {{\n\
             Properties70: {{ P: \"Lcl Translation\", \"Lcl Translation\", \"\", \"A\", 5,0,0 }}\n\
             }}\nConnections: {{\nC: \"OO\",1,10\nC: \"OO\",1,11\n}}\n"
        );
        let doc = parse(scene.as_bytes()).unwrap();
        assert_eq!(doc.objects, ["First", "Second"]);
        assert_eq!(doc.face_object, [0, 1]);
        assert_eq!(doc.mesh.vertex_count(), 8);
        assert_eq!(doc.mesh.face_count(), 2);
        assert_eq!(doc.mesh.corner_vertex(4), 4);
        assert_eq!(doc.mesh.position(4), DVec3::new(5.0, 0.0, 0.0));
        assert_eq!(doc.uv.uv_at_corner(4), doc.uv.uv_at_corner(0));
    }

    #[test]
    fn converts_explicit_scene_axes_and_centimeter_scale_to_y_up_meters() {
        let settings = r#"GlobalSettings: {
    Properties70: {
        P: "UpAxis", "int", "Integer", "", 2
        P: "UpAxisSign", "int", "Integer", "", 1
        P: "FrontAxis", "int", "Integer", "", 1
        P: "FrontAxisSign", "int", "Integer", "", -1
        P: "CoordAxis", "int", "Integer", "", 0
        P: "CoordAxisSign", "int", "Integer", "", 1
        P: "UnitScaleFactor", "double", "Number", "", 2.54
    }
}
"#;
        let doc = parse(format!("{settings}{QUAD}").as_bytes()).unwrap();
        let p = doc.mesh.position(2);
        assert!((p - DVec3::new(0.0254, 0.0, -0.0254)).length() < 1e-12);
    }

    #[test]
    fn exports_multiple_object_runs_in_face_order_with_materials() {
        let obj = obj::parse(
            b"v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\n\
vt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\n\
o A\nusemtl Red\nf 1/1 2/2 3/3\n\
o B\nusemtl Blue\nf 1/1 3/3 4/4\n\
o A\nusemtl Red\nf 1/1 2/2 4/4\n"
                .to_vec(),
        )
        .unwrap();
        let bytes = export(&obj.mesh, &obj.uv, &obj.source).unwrap();
        let ascii = std::str::from_utf8(&bytes).unwrap();
        assert!(ascii.contains("Definitions: {"));
        assert!(ascii.contains("LayerElementNormal: 0"));
        assert!(ascii.contains("LayerElementSmoothing: 0"));
        assert!(ascii.contains("Layer: 0 {"));
        assert!(ascii.contains("C: \"OO\",2,0"));
        let loaded = parse(&bytes).unwrap();
        assert_eq!(loaded.objects, ["A", "B", "A"]);
        assert_eq!(loaded.face_object, [0, 1, 2]);
        assert_eq!(loaded.mesh.face_count(), 3);
        for c in 0..obj.mesh.corner_count() {
            assert_eq!(loaded.uv.uv_at_corner(c), obj.uv.uv_at_corner(c));
            assert_eq!(loaded.mesh.corner_position(c), obj.mesh.corner_position(c));
        }
        let back =
            String::from_utf8(loaded.source.export(&loaded.mesh, &loaded.uv).unwrap()).unwrap();
        let used: Vec<&str> = back
            .lines()
            .filter(|line| line.starts_with("usemtl "))
            .collect();
        assert_eq!(used, ["usemtl Red", "usemtl Blue", "usemtl Red"]);
    }

    #[test]
    fn round_trips_secondary_uv_layer_through_isolated_fbx_api() {
        let obj = obj::parse(
            b"v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\n\
vt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\n\
o Card\nf 1/1 2/2 3/3\nf 1/1 3/3 4/4\n"
                .to_vec(),
        )
        .unwrap();
        let lightmap = UvLayer {
            name: "Lightmap".into(),
            uvs: vec![
                DVec2::new(0.1, 0.2),
                DVec2::new(0.7, 0.2),
                DVec2::new(0.7, 0.8),
                DVec2::new(0.1, 0.8),
                DVec2::new(0.9, 0.9),
            ],
            corner_uv: vec![0, 1, 2, 4, 2, 3],
        };
        let layers = BTreeMap::from([(0, obj.uv.clone()), (1, lightmap.clone())]);
        let bytes = export_layers(&obj.mesh, &layers, &obj.source).unwrap();
        let parsed = parse_layers(&bytes).unwrap();
        assert_eq!(parsed.layers.keys().copied().collect::<Vec<_>>(), [0, 1]);
        assert_eq!(parsed.layers[&1].name, "Lightmap");
        assert!(parsed.layers[&0].same_mapping(&obj.uv, &parsed.document.mesh));
        assert!(parsed.layers[&1].same_mapping(&lightmap, &parsed.document.mesh));
        assert_ne!(
            parsed.layers[&1].uv_at_corner(0),
            parsed.layers[&1].uv_at_corner(3)
        );
        let mut alternate = String::from_utf8(bytes).unwrap();
        alternate =
            alternate.replacen("Type: \"LayerElementUV\" TypedIndex: 0", "__ACTIVE_UV__", 1);
        alternate = alternate.replacen(
            "Type: \"LayerElementUV\" TypedIndex: 1",
            "Type: \"LayerElementUV\" TypedIndex: 0",
            1,
        );
        alternate = alternate.replace("__ACTIVE_UV__", "Type: \"LayerElementUV\" TypedIndex: 1");
        let switched = parse_layers(alternate.as_bytes()).unwrap();
        assert!(
            switched
                .document
                .uv
                .same_mapping(&lightmap, &switched.document.mesh)
        );
        assert!(switched.layers[&1].same_mapping(&obj.uv, &switched.document.mesh));
    }
    fn i64_prop(value: i64) -> Vec<u8> {
        let mut p = vec![b'L'];
        p.extend(value.to_le_bytes());
        p
    }
    fn f64_prop(value: f64) -> Vec<u8> {
        let mut p = vec![b'D'];
        p.extend(value.to_le_bytes());
        p
    }
    fn str_prop(value: &str) -> Vec<u8> {
        let mut p = vec![b'S'];
        p.extend((value.len() as u32).to_le_bytes());
        p.extend(value.as_bytes());
        p
    }
    fn f64_array(values: &[f64]) -> Vec<u8> {
        let mut p = vec![b'd'];
        p.extend((values.len() as u32).to_le_bytes());
        p.extend(0u32.to_le_bytes());
        p.extend(((values.len() * 8) as u32).to_le_bytes());
        for &v in values {
            p.extend(v.to_le_bytes());
        }
        p
    }
    fn f64_array_compressed(values: &[f64]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        for &value in values {
            encoder.write_all(&value.to_le_bytes()).unwrap();
        }
        let bytes = encoder.finish().unwrap();
        let mut p = vec![b'd'];
        p.extend((values.len() as u32).to_le_bytes());
        p.extend(1u32.to_le_bytes());
        p.extend((bytes.len() as u32).to_le_bytes());
        p.extend(bytes);
        p
    }
    fn i32_array(values: &[i32]) -> Vec<u8> {
        let mut p = vec![b'i'];
        p.extend((values.len() as u32).to_le_bytes());
        p.extend(0u32.to_le_bytes());
        p.extend(((values.len() * 4) as u32).to_le_bytes());
        for &v in values {
            p.extend(v.to_le_bytes());
        }
        p
    }
    struct TestNode {
        name: &'static str,
        props: Vec<Vec<u8>>,
        children: Vec<TestNode>,
    }
    fn node(name: &'static str, props: Vec<Vec<u8>>, children: Vec<TestNode>) -> TestNode {
        TestNode {
            name,
            props,
            children,
        }
    }
    fn write_node(node: &TestNode, offset: usize) -> Vec<u8> {
        let mut props: Vec<u8> = Vec::new();
        for p in &node.props {
            props.extend(p);
        }
        let start_children = offset + 13 + node.name.len() + props.len();
        let mut children = Vec::new();
        for child in &node.children {
            children.extend(write_node(child, start_children + children.len()));
        }
        if !node.children.is_empty() {
            children.extend([0u8; 13]);
        }
        let end = start_children + children.len();
        let mut bytes = Vec::new();
        bytes.extend((end as u32).to_le_bytes());
        bytes.extend((node.props.len() as u32).to_le_bytes());
        bytes.extend((props.len() as u32).to_le_bytes());
        bytes.push(node.name.len() as u8);
        bytes.extend(node.name.as_bytes());
        bytes.extend(props);
        bytes.extend(children);
        bytes
    }
    fn binary_quad() -> Vec<u8> {
        let geometry = node(
            "Geometry",
            vec![i64_prop(1), str_prop("Geometry::Quad"), str_prop("Mesh")],
            vec![
                node(
                    "Vertices",
                    vec![f64_array_compressed(&[
                        0., 0., 0., 1., 0., 0., 1., 1., 0., 0., 1., 0.,
                    ])],
                    vec![],
                ),
                node(
                    "PolygonVertexIndex",
                    vec![i32_array(&[0, 1, 2, -4])],
                    vec![],
                ),
                node(
                    "LayerElementUV",
                    vec![i64_prop(0)],
                    vec![
                        node(
                            "MappingInformationType",
                            vec![str_prop("ByPolygonVertex")],
                            vec![],
                        ),
                        node(
                            "ReferenceInformationType",
                            vec![str_prop("IndexToDirect")],
                            vec![],
                        ),
                        node(
                            "UV",
                            vec![f64_array(&[0., 0., 1., 0., 1., 1., 0., 1.])],
                            vec![],
                        ),
                        node("UVIndex", vec![i32_array(&[0, 1, 2, 3])], vec![]),
                    ],
                ),
                node(
                    "LayerElementUV",
                    vec![i64_prop(1)],
                    vec![
                        node("Name", vec![str_prop("Lightmap")], vec![]),
                        node(
                            "MappingInformationType",
                            vec![str_prop("ByPolygonVertex")],
                            vec![],
                        ),
                        node("ReferenceInformationType", vec![str_prop("Direct")], vec![]),
                        node(
                            "UV",
                            vec![f64_array(&[0.1, 0.1, 0.9, 0.1, 0.9, 0.9, 0.1, 0.9])],
                            vec![],
                        ),
                    ],
                ),
                node(
                    "LayerElementMaterial",
                    vec![i64_prop(0)],
                    vec![
                        node("MappingInformationType", vec![str_prop("AllSame")], vec![]),
                        node("Materials", vec![i32_array(&[0])], vec![]),
                    ],
                ),
            ],
        );
        let material = node(
            "Material",
            vec![i64_prop(2), str_prop("Material::Blue"), str_prop("")],
            vec![],
        );
        let selected_material = node(
            "Material",
            vec![i64_prop(3), str_prop("Material::Red"), str_prop("")],
            vec![],
        );
        let model = node(
            "Model",
            vec![i64_prop(4), str_prop("Model::QuadObject"), str_prop("Mesh")],
            vec![node(
                "Properties70",
                vec![],
                vec![node(
                    "P",
                    vec![
                        str_prop("Lcl Translation"),
                        str_prop("Lcl Translation"),
                        str_prop(""),
                        str_prop("A"),
                        f64_prop(5.0),
                        f64_prop(0.0),
                        f64_prop(0.0),
                    ],
                    vec![],
                )],
            )],
        );
        let connections = node(
            "Connections",
            vec![],
            vec![
                node("C", vec![str_prop("OO"), i64_prop(1), i64_prop(4)], vec![]),
                node("C", vec![str_prop("OO"), i64_prop(3), i64_prop(4)], vec![]),
            ],
        );
        let mut bytes = b"Kaydara FBX Binary  \0\x1a\0".to_vec();
        bytes.extend(7400u32.to_le_bytes());
        bytes.extend(write_node(&geometry, bytes.len()));
        bytes.extend(write_node(&material, bytes.len()));
        bytes.extend(write_node(&selected_material, bytes.len()));
        bytes.extend(write_node(&model, bytes.len()));
        bytes.extend(write_node(&connections, bytes.len()));
        bytes.extend([0u8; 13]);
        bytes
    }
    #[test]
    fn imports_binary_quad_uvs_and_material() {
        let doc = parse(&binary_quad()).unwrap();
        assert_eq!(doc.mesh.face_count(), 1);
        assert_eq!(doc.objects, ["QuadObject"]);
        assert_eq!(doc.mesh.position(0), DVec3::new(5.0, 0.0, 0.0));
        assert_eq!(doc.uv.uv_at_corner(2).to_array(), [1.0, 1.0]);
        let out = String::from_utf8(doc.source.export(&doc.mesh, &doc.uv).unwrap()).unwrap();
        assert!(out.contains("usemtl Red"));
        let layered = parse_layers(&binary_quad()).unwrap();
        assert_eq!(layered.layers[&1].name, "Lightmap");
        assert_eq!(layered.layers[&1].uv_at_corner(2), DVec2::new(0.9, 0.9));
    }

    #[test]
    fn reads_binary_fbx_and_prepares_obj_export() {
        let path = std::env::temp_dir().join(format!(
            "uvcraft-fbx-import-{}-{}.fbx",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, binary_quad()).unwrap();
        let loaded = crate::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let output = path.with_extension("obj");
        let files =
            crate::prepare_write(&output, &loaded.source, &loaded.mesh, &loaded.uv).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, output);
        let obj = std::str::from_utf8(&files[0].1).unwrap();
        assert!(obj.contains("o QuadObject"));
        assert!(obj.contains("usemtl Red"));
    }

    #[test]
    fn exports_ascii_fbx_with_face_order_uvs_and_materials() {
        let obj = obj::parse(
            b"v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\n\
vt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\nvt 0.5 0.5\n\
o Card\nusemtl Red\nf 1/1 2/2 3/3\nusemtl Blue\nf 1/5 3/3 4/4\n"
                .to_vec(),
        )
        .unwrap();
        let bytes = export(&obj.mesh, &obj.uv, &obj.source).unwrap();
        let loaded = parse(&bytes).unwrap();
        assert_eq!(loaded.objects, ["Card"]);
        assert_eq!(loaded.mesh.vertex_count(), 4);
        assert_eq!(loaded.mesh.face_count(), 2);
        for c in 0..obj.mesh.corner_count() {
            assert_eq!(loaded.mesh.corner_vertex(c), obj.mesh.corner_vertex(c));
            assert_eq!(loaded.uv.uv_at_corner(c), obj.uv.uv_at_corner(c));
        }
        let exported_obj =
            String::from_utf8(loaded.source.export(&loaded.mesh, &loaded.uv).unwrap()).unwrap();
        assert!(exported_obj.contains("usemtl Red"));
        assert!(exported_obj.contains("usemtl Blue"));

        let target = std::env::temp_dir().join("uvcraft-roundtrip.fbx");
        let files = crate::prepare_write(
            &target,
            &crate::Source::Obj(obj.source.clone()),
            &obj.mesh,
            &obj.uv,
        )
        .unwrap();
        assert_eq!(files[0].0, target);
        assert_eq!(parse(&files[0].1).unwrap().mesh.face_count(), 2);

        let glb_path = std::env::temp_dir().join("uvcraft-roundtrip.glb");
        let glb =
            crate::convert::obj_to_gltf(&obj.source, &obj.mesh, &obj.uv, &glb_path, true).unwrap();
        let gltf = crate::gltf::parse(glb[0].1.clone(), &std::env::temp_dir()).unwrap();
        let files = crate::prepare_write(
            &target,
            &crate::Source::Gltf(gltf.source),
            &gltf.mesh,
            &gltf.uv,
        )
        .unwrap();
        assert_eq!(parse(&files[0].1).unwrap().mesh.face_count(), 2);
    }
}
