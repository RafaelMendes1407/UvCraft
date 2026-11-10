//! glTF 2.0 reader/writer (`.gltf` with external or embedded buffers, and `.glb`).
//!
//! glTF stores split vertices: a vertex is duplicated wherever any attribute
//! differs (UV seams, hard normals). UvCraft rebuilds real topology:
//! - mesh vertex = unique POSITION value within one glTF mesh;
//! - UV vertex = unique (mesh vertex, UV value) pair, so hard edges that only
//!   split normals do not become seams.
//!
//! Each glTF vertex holds exactly one UV, called a *slot* here. Export writes
//! changed slot values into the existing TEXCOORD_0 bytes, in place; the rest of
//! the file is preserved. Primitives without TEXCOORD_0 get a new accessor only
//! when their UVs change.
//!
//! glTF's V axis points down; UvCraft uses V up (like OBJ), so `v = 1 - v_gltf`.
//!
//! When a cut makes one glTF vertex need two UVs, the primitive is rebuilt:
//! every attribute (and morph target) gets a new accessor with the extra
//! vertices appended, plus new triangle-list indices. Old accessors stay in
//! the file unused.
//!
//! Not supported yet: sparse accessors, Draco/meshopt compression,
//! TEXCOORD_1+, node transforms (meshes are measured in their local space),
//! converting between `.gltf` and `.glb`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use glam::{DVec2, DVec3};
use serde_json::{Value, json};
use uvcraft_core::{Mesh, MeshError, UvError, UvLayer};

const GLB_MAGIC: &[u8; 4] = b"glTF";
const CHUNK_JSON: u32 = 0x4E4F_534A;
const CHUNK_BIN: u32 = 0x004E_4942;

const BYTE: u32 = 5120;
const UNSIGNED_BYTE: u32 = 5121;
const SHORT: u32 = 5122;
const UNSIGNED_SHORT: u32 = 5123;
const UNSIGNED_INT: u32 = 5125;
const FLOAT: u32 = 5126;

#[derive(Debug, thiserror::Error)]
pub enum GltfError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid glTF: {0}")]
    Invalid(String),
    #[error("unsupported glTF feature: {0}")]
    Unsupported(String),
    #[error(transparent)]
    Mesh(#[from] MeshError),
    #[error(transparent)]
    Uv(#[from] UvError),
}

fn invalid(msg: impl Into<String>) -> GltfError {
    GltfError::Invalid(msg.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Gltf,
    Glb,
}

#[derive(Debug, Clone, PartialEq)]
enum BufferSource {
    /// The GLB binary chunk (buffer 0 of a .glb without uri).
    GlbBin,
    /// `data:` URI; `prefix` is everything up to and including the comma.
    DataUri { prefix: String },
    /// External file, `uri` as written in the JSON (percent-encoded).
    File { uri: String },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SlotTarget {
    /// Element of an existing TEXCOORD_0 accessor.
    Accessor(usize),
    /// Primitive (index into `prims`) that has no TEXCOORD_0 yet.
    Missing(usize),
}

#[derive(Debug, Clone)]
struct Slot {
    target: SlotTarget,
    element: u32,
    /// UV at load time, in UvCraft convention.
    original: DVec2,
}

#[derive(Debug, Clone)]
struct PrimInfo {
    mesh: usize,
    prim: usize,
    vertex_count: usize,
}

/// Original file content needed to write it back faithfully.
#[derive(Debug, Clone)]
pub struct GltfSource {
    container: Container,
    /// Main file bytes (used verbatim for an unchanged .gltf).
    original: Vec<u8>,
    json: Value,
    /// Raw JSON chunk of a .glb, padding included.
    json_chunk: Vec<u8>,
    buffers: Vec<Vec<u8>>,
    buffer_sources: Vec<BufferSource>,
    /// GLB chunks after JSON and BIN, kept verbatim.
    extra_chunks: Vec<(u32, Vec<u8>)>,
    /// Absolute directory of the source file (relative URIs resolve against it).
    base_dir: PathBuf,
    slots: Vec<Slot>,
    corner_slot: Vec<u32>,
    /// Primitive (index into `prims`) and glTF vertex element of each corner.
    corner_prim: Vec<u32>,
    corner_element: Vec<u32>,
    prims: Vec<PrimInfo>,
}

#[derive(Debug, Clone)]
pub struct GltfDocument {
    pub mesh: Mesh,
    pub uv: UvLayer,
    pub source: GltfSource,
    /// One object per glTF mesh (its name, or `mesh<N>`).
    pub objects: Vec<String>,
    pub face_object: Vec<u32>,
    /// Corners of primitives without TEXCOORD_0 (placeholder UVs at 0,0).
    pub missing_uv_corners: usize,
    /// Point and line primitives, which are kept but not editable.
    pub skipped_primitives: usize,
}

pub fn read(path: &Path) -> Result<GltfDocument, GltfError> {
    let bytes = std::fs::read(path)?;
    let dir = parent_dir(path).canonicalize()?;
    parse(bytes, &dir)
}

pub fn write(path: &Path, source: &GltfSource, mesh: &Mesh, uv: &UvLayer) -> Result<(), GltfError> {
    for (file, bytes) in source.export(mesh, uv, path)? {
        std::fs::write(file, bytes)?;
    }
    Ok(())
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

// ---------------------------------------------------------------- container

struct ParsedContainer {
    kind: Container,
    json_chunk: Vec<u8>,
    bin: Option<Vec<u8>>,
    extra: Vec<(u32, Vec<u8>)>,
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn parse_container(bytes: &[u8]) -> Result<ParsedContainer, GltfError> {
    if !bytes.starts_with(GLB_MAGIC) {
        return Ok(ParsedContainer {
            kind: Container::Gltf,
            json_chunk: bytes.to_vec(),
            bin: None,
            extra: Vec::new(),
        });
    }
    let version = u32_at(bytes, 4).ok_or_else(|| invalid("truncated GLB header"))?;
    if version != 2 {
        return Err(GltfError::Unsupported(format!("GLB version {version}")));
    }
    let length = u32_at(bytes, 8).ok_or_else(|| invalid("truncated GLB header"))? as usize;
    let bytes = bytes
        .get(..length)
        .ok_or_else(|| invalid("GLB shorter than its header says"))?;
    let mut chunks = Vec::new();
    let mut at = 12;
    while at < bytes.len() {
        let len = u32_at(bytes, at).ok_or_else(|| invalid("truncated GLB chunk"))? as usize;
        let ty = u32_at(bytes, at + 4).ok_or_else(|| invalid("truncated GLB chunk"))?;
        let data = bytes
            .get(at + 8..at + 8 + len)
            .ok_or_else(|| invalid("GLB chunk exceeds file"))?;
        chunks.push((ty, data.to_vec()));
        at += 8 + len;
    }
    let mut chunks = chunks.into_iter();
    let json_chunk = match chunks.next() {
        Some((CHUNK_JSON, data)) => data,
        _ => return Err(invalid("first GLB chunk must be JSON")),
    };
    let mut extra: Vec<_> = chunks.collect();
    let bin = match extra.first() {
        Some((CHUNK_BIN, _)) => Some(extra.remove(0).1),
        _ => None,
    };
    Ok(ParsedContainer {
        kind: Container::Glb,
        json_chunk,
        bin,
        extra,
    })
}

fn build_glb(json: &[u8], bin: Option<&[u8]>, extra: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut chunks: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut j = json.to_vec();
    while j.len() % 4 != 0 {
        j.push(b' ');
    }
    chunks.push((CHUNK_JSON, j));
    if let Some(b) = bin {
        let mut b = b.to_vec();
        while b.len() % 4 != 0 {
            b.push(0);
        }
        chunks.push((CHUNK_BIN, b));
    }
    chunks.extend(extra.iter().cloned());
    let total = 12 + chunks.iter().map(|(_, d)| 8 + d.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(GLB_MAGIC);
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    for (ty, data) in chunks {
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&data);
    }
    out
}

// ---------------------------------------------------------------- URIs

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Path from `from_dir` to `to`, both absolute.
fn relative_path(from_dir: &Path, to: &Path) -> PathBuf {
    let f: Vec<Component> = from_dir.components().collect();
    let t: Vec<Component> = to.components().collect();
    let common = f.iter().zip(&t).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..f.len() {
        out.push("..");
    }
    for c in &t[common..] {
        out.push(c);
    }
    out
}

fn path_to_uri(p: &Path) -> String {
    let parts: Vec<String> = p
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    percent_encode(&parts.join("/"))
}

fn is_data_uri(uri: &str) -> bool {
    uri.starts_with("data:")
}

// ---------------------------------------------------------------- accessors

#[derive(Debug, Clone, Copy)]
struct Accessor {
    buffer: usize,
    /// Absolute byte offset of element 0 within the buffer.
    offset: usize,
    stride: usize,
    count: usize,
    component: u32,
    components: usize,
    normalized: bool,
}

fn component_size(ty: u32) -> Option<usize> {
    match ty {
        BYTE | UNSIGNED_BYTE => Some(1),
        SHORT | UNSIGNED_SHORT => Some(2),
        UNSIGNED_INT | FLOAT => Some(4),
        _ => None,
    }
}

fn get_usize(v: &Value, key: &str) -> Option<usize> {
    v.get(key)?.as_u64().map(|n| n as usize)
}

fn parse_accessor(json: &Value, buffers: &[Vec<u8>], index: usize) -> Result<Accessor, GltfError> {
    let a = json["accessors"]
        .get(index)
        .ok_or_else(|| invalid(format!("accessor {index} does not exist")))?;
    if a.get("sparse").is_some() {
        return Err(GltfError::Unsupported(format!("sparse accessor {index}")));
    }
    let view_index = get_usize(a, "bufferView")
        .ok_or_else(|| GltfError::Unsupported(format!("accessor {index} without bufferView")))?;
    let view = json["bufferViews"]
        .get(view_index)
        .ok_or_else(|| invalid(format!("bufferView {view_index} does not exist")))?;
    let component = get_usize(a, "componentType").unwrap_or(0) as u32;
    let size = component_size(component)
        .ok_or_else(|| invalid(format!("accessor {index} has componentType {component}")))?;
    let components = match a["type"].as_str() {
        Some("SCALAR") => 1,
        Some("VEC2") => 2,
        Some("VEC3") => 3,
        Some("VEC4") => 4,
        other => {
            return Err(GltfError::Unsupported(format!(
                "accessor {index} type {other:?}"
            )));
        }
    };
    let buffer = get_usize(view, "buffer")
        .ok_or_else(|| invalid(format!("bufferView {view_index} has no buffer")))?;
    let view_offset = get_usize(view, "byteOffset").unwrap_or(0);
    let view_len = get_usize(view, "byteLength").unwrap_or(0);
    let stride = get_usize(view, "byteStride").unwrap_or(size * components);
    let count = get_usize(a, "count").unwrap_or(0);
    let offset = view_offset + get_usize(a, "byteOffset").unwrap_or(0);
    let data_len = buffers.get(buffer).map_or(0, Vec::len);
    let end = if count == 0 {
        offset
    } else {
        offset + (count - 1) * stride + size * components
    };
    if end > view_offset + view_len || view_offset + view_len > data_len {
        return Err(invalid(format!(
            "accessor {index} exceeds its bufferView or buffer"
        )));
    }
    Ok(Accessor {
        buffer,
        offset,
        stride,
        count,
        component,
        components,
        normalized: a["normalized"].as_bool().unwrap_or(false),
    })
}

impl Accessor {
    fn at(&self, element: usize, component: usize) -> usize {
        self.offset
            + element * self.stride
            + component * component_size(self.component).unwrap_or(4)
    }

    /// Stored value without normalization.
    fn read_raw(&self, data: &[u8], element: usize, component: usize) -> f64 {
        let i = self.at(element, component);
        match self.component {
            BYTE => data[i] as i8 as f64,
            UNSIGNED_BYTE => data[i] as f64,
            SHORT => i16::from_le_bytes([data[i], data[i + 1]]) as f64,
            UNSIGNED_SHORT => u16::from_le_bytes([data[i], data[i + 1]]) as f64,
            UNSIGNED_INT => u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as f64,
            _ => f32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as f64,
        }
    }

    fn read(&self, data: &[u8], element: usize, component: usize) -> f64 {
        let x = self.read_raw(data, element, component);
        if !self.normalized {
            return x;
        }
        match self.component {
            BYTE => (x / 127.0).max(-1.0),
            UNSIGNED_BYTE => x / 255.0,
            SHORT => (x / 32767.0).max(-1.0),
            UNSIGNED_SHORT => x / 65535.0,
            _ => x,
        }
    }

    fn write(
        &self,
        data: &mut [u8],
        element: usize,
        component: usize,
        value: f64,
    ) -> Result<(), GltfError> {
        let i = self.at(element, component);
        let unit = |max: f64| -> Result<f64, GltfError> {
            if (-1e-6..=1.0 + 1e-6).contains(&value) {
                Ok((value.clamp(0.0, 1.0) * max).round())
            } else {
                Err(GltfError::Unsupported(format!(
                    "UV {value} is outside 0-1 and cannot be stored in a normalized integer accessor"
                )))
            }
        };
        match (self.component, self.normalized) {
            (FLOAT, _) => data[i..i + 4].copy_from_slice(&(value as f32).to_le_bytes()),
            (UNSIGNED_BYTE, true) => data[i] = unit(255.0)? as u8,
            (UNSIGNED_SHORT, true) => {
                data[i..i + 2].copy_from_slice(&(unit(65535.0)? as u16).to_le_bytes())
            }
            (ty, n) => {
                return Err(GltfError::Unsupported(format!(
                    "writing UVs to componentType {ty} (normalized: {n})"
                )));
            }
        }
        Ok(())
    }
}

/// glTF (V down) → UvCraft (V up).
fn to_internal(u: f64, v: f64) -> DVec2 {
    DVec2::new(u, 1.0 - v)
}

// ---------------------------------------------------------------- parse

fn load_buffers(
    json: &Value,
    container: Container,
    bin: Option<Vec<u8>>,
    base_dir: &Path,
) -> Result<(Vec<Vec<u8>>, Vec<BufferSource>), GltfError> {
    let mut bin = bin;
    let mut data = Vec::new();
    let mut sources = Vec::new();
    for (i, b) in json["buffers"].as_array().into_iter().flatten().enumerate() {
        let len = get_usize(b, "byteLength")
            .ok_or_else(|| invalid(format!("buffer {i} has no byteLength")))?;
        let (bytes, source) = match b.get("uri").and_then(Value::as_str) {
            None if container == Container::Glb && i == 0 => (
                bin.take()
                    .ok_or_else(|| invalid("buffer 0 refers to a missing GLB binary chunk"))?,
                BufferSource::GlbBin,
            ),
            None => return Err(invalid(format!("buffer {i} has no uri"))),
            Some(uri) if is_data_uri(uri) => {
                let comma = uri
                    .find(',')
                    .ok_or_else(|| invalid(format!("buffer {i}: malformed data URI")))?;
                if !uri[..comma].ends_with(";base64") {
                    return Err(GltfError::Unsupported(format!(
                        "buffer {i}: non-base64 data URI"
                    )));
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&uri[comma + 1..])
                    .map_err(|e| invalid(format!("buffer {i}: {e}")))?;
                (
                    bytes,
                    BufferSource::DataUri {
                        prefix: uri[..=comma].to_string(),
                    },
                )
            }
            Some(uri) => {
                let path = base_dir.join(percent_decode(uri));
                let bytes = std::fs::read(&path)
                    .map_err(|e| invalid(format!("buffer {i} ({}): {e}", path.display())))?;
                (
                    bytes,
                    BufferSource::File {
                        uri: uri.to_string(),
                    },
                )
            }
        };
        if bytes.len() < len {
            return Err(invalid(format!(
                "buffer {i} is shorter than its byteLength"
            )));
        }
        data.push(bytes);
        sources.push(source);
    }
    Ok((data, sources))
}

fn triangles(mode: usize, idx: &[u32]) -> Result<Vec<[u32; 3]>, GltfError> {
    Ok(match mode {
        4 => idx.chunks_exact(3).map(|t| [t[0], t[1], t[2]]).collect(),
        5 => (0..idx.len().saturating_sub(2))
            .map(|i| {
                if i % 2 == 0 {
                    [idx[i], idx[i + 1], idx[i + 2]]
                } else {
                    [idx[i + 1], idx[i], idx[i + 2]]
                }
            })
            .collect(),
        6 => (1..idx.len().saturating_sub(1))
            .map(|i| [idx[0], idx[i], idx[i + 1]])
            .collect(),
        m => return Err(invalid(format!("primitive mode {m}"))),
    })
}

/// Parses a `.gltf` or `.glb` file; `base_dir` must be absolute.
pub fn parse(original: Vec<u8>, base_dir: &Path) -> Result<GltfDocument, GltfError> {
    let c = parse_container(&original)?;
    let json: Value = serde_json::from_slice(&c.json_chunk)?;
    if let Some(required) = json["extensionsRequired"].as_array() {
        let names: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
        for ext in [
            "KHR_draco_mesh_compression",
            "EXT_meshopt_compression",
            "KHR_meshopt_compression",
        ] {
            if names.contains(&ext) {
                return Err(GltfError::Unsupported(format!("required extension {ext}")));
            }
        }
    }
    let (buffers, buffer_sources) = load_buffers(&json, c.kind, c.bin, base_dir)?;

    let mut positions: Vec<DVec3> = Vec::new();
    let mut vertex_of: HashMap<(usize, [u64; 3]), u32> = HashMap::new();
    let mut faces: Vec<[u32; 3]> = Vec::new();
    let mut face_object = Vec::new();
    let mut objects = Vec::new();
    let mut slots: Vec<Slot> = Vec::new();
    let mut slot_of: HashMap<(usize, bool, u32), u32> = HashMap::new();
    let mut corner_slot: Vec<u32> = Vec::new();
    let mut corner_prim: Vec<u32> = Vec::new();
    let mut corner_element: Vec<u32> = Vec::new();
    let mut slot_vertex: Vec<u32> = Vec::new();
    let mut prims = Vec::new();
    let mut skipped = 0;
    let mut missing = 0;

    for (mi, m) in json["meshes"].as_array().into_iter().flatten().enumerate() {
        objects.push(
            m["name"]
                .as_str()
                .map_or_else(|| format!("mesh{mi}"), str::to_string),
        );
        for (pi, p) in m["primitives"].as_array().into_iter().flatten().enumerate() {
            let mode = get_usize(p, "mode").unwrap_or(4);
            if mode < 4 {
                skipped += 1;
                continue;
            }
            let ctx = |msg: &str| invalid(format!("mesh {mi} primitive {pi}: {msg}"));
            let pos_index =
                get_usize(&p["attributes"], "POSITION").ok_or_else(|| ctx("no POSITION"))?;
            let pos = parse_accessor(&json, &buffers, pos_index)?;
            if pos.components != 3 {
                return Err(ctx("POSITION must be VEC3"));
            }
            let tex = match get_usize(&p["attributes"], "TEXCOORD_0") {
                Some(t) => {
                    let a = parse_accessor(&json, &buffers, t)?;
                    if a.components != 2 || a.count < pos.count {
                        return Err(ctx("TEXCOORD_0 must be VEC2 with one value per vertex"));
                    }
                    Some((t, a))
                }
                None => None,
            };
            let idx: Vec<u32> = match get_usize(p, "indices") {
                Some(i) => {
                    let a = parse_accessor(&json, &buffers, i)?;
                    if a.components != 1
                        || !matches!(a.component, UNSIGNED_BYTE | UNSIGNED_SHORT | UNSIGNED_INT)
                    {
                        return Err(ctx("indices must be unsigned integer scalars"));
                    }
                    (0..a.count)
                        .map(|e| a.read_raw(&buffers[a.buffer], e, 0) as u32)
                        .collect()
                }
                None => (0..pos.count as u32).collect(),
            };
            if let Some(&bad) = idx.iter().find(|&&i| i as usize >= pos.count) {
                return Err(ctx(&format!(
                    "index {bad} out of range ({} vertices)",
                    pos.count
                )));
            }

            let prim_id = prims.len();
            prims.push(PrimInfo {
                mesh: mi,
                prim: pi,
                vertex_count: pos.count,
            });
            for tri in triangles(mode, &idx)? {
                let mut face = [0u32; 3];
                for (k, &e) in tri.iter().enumerate() {
                    let e_usize = e as usize;
                    let data = &buffers[pos.buffer];
                    let pt = DVec3::new(
                        pos.read(data, e_usize, 0),
                        pos.read(data, e_usize, 1),
                        pos.read(data, e_usize, 2),
                    );
                    let key = (mi, pt.to_array().map(f64::to_bits));
                    let v = *vertex_of.entry(key).or_insert_with(|| {
                        positions.push(pt);
                        positions.len() as u32 - 1
                    });
                    face[k] = v;

                    let (target, key) = match tex {
                        Some((t, _)) => (SlotTarget::Accessor(t), (t, true, e)),
                        None => (SlotTarget::Missing(prim_id), (prim_id, false, e)),
                    };
                    let s = *slot_of.entry(key).or_insert_with(|| {
                        let original = match tex {
                            Some((_, a)) => {
                                let d = &buffers[a.buffer];
                                to_internal(a.read(d, e_usize, 0), a.read(d, e_usize, 1))
                            }
                            None => DVec2::ZERO,
                        };
                        slots.push(Slot {
                            target,
                            element: e,
                            original,
                        });
                        slot_vertex.push(v);
                        slots.len() as u32 - 1
                    });
                    corner_slot.push(s);
                    corner_prim.push(prim_id as u32);
                    corner_element.push(e);
                    if tex.is_none() {
                        missing += 1;
                    }
                }
                faces.push(face);
                face_object.push(mi as u32);
            }
        }
    }

    let mesh = Mesh::new(positions, &faces)?;

    // Weld slots that share mesh vertex and UV value into one UV vertex.
    let mut uv_of: HashMap<(u32, [u64; 2]), u32> = HashMap::new();
    let mut uvs = Vec::new();
    let slot_uv: Vec<u32> = slots
        .iter()
        .zip(&slot_vertex)
        .map(|(s, &v)| {
            *uv_of
                .entry((v, s.original.to_array().map(f64::to_bits)))
                .or_insert_with(|| {
                    uvs.push(s.original);
                    uvs.len() as u32 - 1
                })
        })
        .collect();
    let uv = UvLayer {
        name: "TEXCOORD_0".into(),
        corner_uv: corner_slot.iter().map(|&s| slot_uv[s as usize]).collect(),
        uvs,
    };
    uv.validate(&mesh)?;

    Ok(GltfDocument {
        mesh,
        uv,
        source: GltfSource {
            container: c.kind,
            original,
            json,
            json_chunk: c.json_chunk,
            buffers,
            buffer_sources,
            extra_chunks: c.extra,
            base_dir: base_dir.to_path_buf(),
            slots,
            corner_slot,
            corner_prim,
            corner_element,
            prims,
        },
        objects,
        face_object,
        missing_uv_corners: missing,
        skipped_primitives: skipped,
    })
}

// ---------------------------------------------------------------- export

/// Collects new bufferViews/accessors appended to one buffer.
struct Appender {
    target: usize,
    /// Length of the target buffer before appending.
    base: usize,
    data: Vec<u8>,
}

impl Appender {
    /// Appends `bytes` as a new bufferView and accessor; returns the accessor index.
    fn push(
        &mut self,
        json: &mut Value,
        bytes: Vec<u8>,
        view_target: Option<u32>,
        stride: Option<usize>,
        mut accessor: Value,
    ) -> Result<usize, GltfError> {
        while (self.base + self.data.len()) % 4 != 0 {
            self.data.push(0);
        }
        let offset = self.base + self.data.len();
        let mut view =
            json!({ "buffer": self.target, "byteOffset": offset, "byteLength": bytes.len() });
        if let Some(t) = view_target {
            view["target"] = json!(t);
        }
        if let Some(s) = stride {
            view["byteStride"] = json!(s);
        }
        self.data.extend_from_slice(&bytes);
        let views = json["bufferViews"]
            .as_array_mut()
            .ok_or_else(|| invalid("no bufferViews array"))?;
        views.push(view);
        accessor["bufferView"] = json!(views.len() - 1);
        let accessors = json["accessors"]
            .as_array_mut()
            .ok_or_else(|| invalid("no accessors array"))?;
        accessors.push(accessor);
        Ok(accessors.len() - 1)
    }
}

/// Copies a vertex attribute and appends copies of the `extra` elements.
/// Returns (data, byteStride if padded, accessor JSON without bufferView).
fn copy_with_dups(
    json: &Value,
    buffers: &[Vec<u8>],
    a: usize,
    count: usize,
    extra: &[(u32, DVec2)],
) -> Result<(Vec<u8>, Option<usize>, Value), GltfError> {
    let acc = parse_accessor(json, buffers, a)?;
    if acc.count != count {
        return Err(GltfError::Unsupported(format!(
            "accessor {a} has {} elements but POSITION has {count}",
            acc.count
        )));
    }
    let size = component_size(acc.component).unwrap_or(4) * acc.components;
    // Vertex attribute elements must be 4-byte aligned.
    let stride = size.next_multiple_of(4);
    let src = &buffers[acc.buffer];
    let mut data = vec![0u8; stride * (count + extra.len())];
    let elements = (0..count).chain(extra.iter().map(|&(e, _)| e as usize));
    for (dst, e) in elements.enumerate() {
        let at = acc.at(e, 0);
        data[dst * stride..dst * stride + size].copy_from_slice(&src[at..at + size]);
    }
    let old = &json["accessors"][a];
    let mut new = json!({ "componentType": acc.component, "count": count + extra.len(), "type": old["type"] });
    if acc.normalized {
        new["normalized"] = json!(true);
    }
    for key in ["min", "max"] {
        if let Some(v) = old.get(key) {
            new[key] = v.clone();
        }
    }
    Ok((data, (stride != size).then_some(stride), new))
}

fn unique_name(stem: &str, suffix: &str, taken: &mut HashSet<String>) -> String {
    let mut name = format!("{stem}{suffix}");
    let mut n = 1;
    while taken.contains(&name) {
        name = format!("{stem}_{n}{suffix}");
        n += 1;
    }
    taken.insert(name.clone());
    name
}

impl GltfSource {
    pub fn container(&self) -> Container {
        self.container
    }

    /// Files to write for this mesh with the given UV layer: the main file at
    /// `out_path` (last) plus any external buffers next to it.
    pub fn export(
        &self,
        mesh: &Mesh,
        uv: &UvLayer,
        out_path: &Path,
    ) -> Result<Vec<(PathBuf, Vec<u8>)>, GltfError> {
        uv.validate(mesh)?;
        if uv.corner_uv.len() != self.corner_slot.len() {
            return Err(invalid("UV layer does not belong to this file"));
        }
        let ext = out_path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        let wanted = if ext.as_deref() == Some("glb") {
            Container::Glb
        } else {
            Container::Gltf
        };
        if wanted != self.container {
            return Err(GltfError::Unsupported(
                "converting between .gltf and .glb".into(),
            ));
        }

        // Each slot (glTF vertex) keeps the first UV its corners ask for.
        // Corners that need another UV get a duplicated vertex in their primitive.
        let mut value: Vec<Option<DVec2>> = vec![None; self.slots.len()];
        let mut dups: Vec<Vec<(u32, DVec2)>> = vec![Vec::new(); self.prims.len()];
        let mut dup_of: HashMap<(u32, u32, [u64; 2]), u32> = HashMap::new();
        let mut corner_dup: Vec<u32> = vec![u32::MAX; self.corner_slot.len()];
        for (c, &s) in self.corner_slot.iter().enumerate() {
            let p = uv.uv_at_corner(c);
            match value[s as usize] {
                None => value[s as usize] = Some(p),
                Some(q) if q == p => {}
                Some(_) => {
                    let prim = self.corner_prim[c];
                    let key = (prim, s, p.to_array().map(f64::to_bits));
                    corner_dup[c] = *dup_of.entry(key).or_insert_with(|| {
                        let list = &mut dups[prim as usize];
                        list.push((self.corner_element[c], p));
                        (self.prims[prim as usize].vertex_count + list.len() - 1) as u32
                    });
                }
            }
        }

        let mut json = self.json.clone();
        let mut json_changed = false;
        let mut buffers = self.buffers.clone();
        let mut sources = self.buffer_sources.clone();
        let mut changed = vec![false; buffers.len()];
        let mut touched = BTreeSet::new();
        let mut missing: HashMap<usize, Vec<(u32, DVec2)>> = HashMap::new();
        for (s, slot) in self.slots.iter().enumerate() {
            let Some(v) = value[s] else { continue };
            if v == slot.original {
                continue;
            }
            match slot.target {
                SlotTarget::Accessor(a) => {
                    let acc = parse_accessor(&json, &buffers, a)?;
                    let e = slot.element as usize;
                    acc.write(&mut buffers[acc.buffer], e, 0, v.x)?;
                    acc.write(&mut buffers[acc.buffer], e, 1, 1.0 - v.y)?;
                    changed[acc.buffer] = true;
                    touched.insert(a);
                }
                SlotTarget::Missing(p) => missing.entry(p).or_default().push((slot.element, v)),
            }
        }

        // Primitives that need new data: new TEXCOORD_0 (had none), or split vertices.
        let mut rebuild: Vec<usize> = (0..self.prims.len())
            .filter(|&p| missing.contains_key(&p) || !dups[p].is_empty())
            .collect();
        rebuild.sort_unstable();
        if !rebuild.is_empty() {
            let target = match self.container {
                Container::Glb if sources.first() == Some(&BufferSource::GlbBin) => {
                    // Drop BIN-chunk padding beyond byteLength before appending.
                    let len = get_usize(&json["buffers"][0], "byteLength").unwrap_or(0);
                    buffers[0].truncate(len);
                    0
                }
                Container::Glb => {
                    return Err(GltfError::Unsupported(
                        "adding vertex data to a .glb without a binary chunk".into(),
                    ));
                }
                Container::Gltf => {
                    buffers.push(Vec::new());
                    changed.push(false);
                    sources.push(BufferSource::File { uri: String::new() });
                    json["buffers"]
                        .as_array_mut()
                        .ok_or_else(|| invalid("no buffers array"))?
                        .push(json!({ "byteLength": 0 }));
                    buffers.len() - 1
                }
            };
            changed[target] = true;
            let mut out = Appender {
                target,
                base: buffers[target].len(),
                data: Vec::new(),
            };

            for p in rebuild {
                let info = &self.prims[p];
                let (count, extra) = (info.vertex_count, &dups[p]);
                let total = count + extra.len();
                let prim_json = json["meshes"][info.mesh]["primitives"][info.prim].clone();

                if !extra.is_empty() {
                    // Duplicate every vertex attribute (and morph target) element.
                    let mut sets: Vec<(Option<usize>, String, usize)> = Vec::new();
                    for (name, a) in prim_json["attributes"].as_object().into_iter().flatten() {
                        if name != "TEXCOORD_0" {
                            sets.push((
                                None,
                                name.clone(),
                                a.as_u64().unwrap_or(u64::MAX) as usize,
                            ));
                        }
                    }
                    for (t, morph) in prim_json["targets"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .enumerate()
                    {
                        for (name, a) in morph.as_object().into_iter().flatten() {
                            sets.push((
                                Some(t),
                                name.clone(),
                                a.as_u64().unwrap_or(u64::MAX) as usize,
                            ));
                        }
                    }
                    for (morph, name, a) in sets {
                        let (data, stride, accessor) =
                            copy_with_dups(&json, &buffers, a, count, extra)?;
                        let new = out.push(&mut json, data, Some(34962), stride, accessor)?;
                        let prim = &mut json["meshes"][info.mesh]["primitives"][info.prim];
                        match morph {
                            None => prim["attributes"][&name] = json!(new),
                            Some(t) => prim["targets"][t][&name] = json!(new),
                        }
                    }
                    // New triangle-list indices.
                    let mut data = Vec::new();
                    for (c, &dup) in corner_dup.iter().enumerate() {
                        if self.corner_prim[c] as usize == p {
                            let e = if dup != u32::MAX {
                                dup
                            } else {
                                self.corner_element[c]
                            };
                            data.extend_from_slice(&e.to_le_bytes());
                        }
                    }
                    let n = data.len() / 4;
                    let accessor =
                        json!({ "componentType": UNSIGNED_INT, "count": n, "type": "SCALAR" });
                    let idx = out.push(&mut json, data, Some(34963), None, accessor)?;
                    let prim = &mut json["meshes"][info.mesh]["primitives"][info.prim];
                    prim["indices"] = json!(idx);
                    prim["mode"] = json!(4);
                }

                // TEXCOORD_0 with one value per (possibly duplicated) vertex.
                let mut data = vec![0u8; total * 8];
                let existing = match get_usize(&prim_json["attributes"], "TEXCOORD_0") {
                    Some(a) => Some(parse_accessor(&json, &buffers, a)?),
                    None => None,
                };
                let mut put = |e: usize, u: f64, v_gltf: f64| {
                    data[e * 8..e * 8 + 4].copy_from_slice(&(u as f32).to_le_bytes());
                    data[e * 8 + 4..e * 8 + 8].copy_from_slice(&(v_gltf as f32).to_le_bytes());
                };
                for e in 0..count {
                    match existing {
                        // Buffers already hold the in-place updates.
                        Some(acc) => {
                            let src = &buffers[acc.buffer];
                            put(e, acc.read(src, e, 0), acc.read(src, e, 1));
                        }
                        None => put(e, 0.0, 1.0), // placeholder (0, 0) in UvCraft
                    }
                }
                for &(e, uvp) in missing.get(&p).into_iter().flatten() {
                    put(e as usize, uvp.x, 1.0 - uvp.y);
                }
                for (k, &(_, uvp)) in extra.iter().enumerate() {
                    put(count + k, uvp.x, 1.0 - uvp.y);
                }
                let accessor = json!({ "componentType": FLOAT, "count": total, "type": "VEC2" });
                let tex = out.push(&mut json, data, Some(34962), None, accessor)?;
                json["meshes"][info.mesh]["primitives"][info.prim]["attributes"]["TEXCOORD_0"] =
                    json!(tex);
            }
            buffers[target].extend_from_slice(&out.data);
            let len = buffers[target].len();
            json["buffers"][target]["byteLength"] = json!(len);
            json_changed = true;
        }

        // Keep accessor bounds valid where the file declares them.
        for a in touched {
            if json["accessors"][a].get("min").is_none()
                && json["accessors"][a].get("max").is_none()
            {
                continue;
            }
            let acc = parse_accessor(&json, &buffers, a)?;
            let data = &buffers[acc.buffer];
            let mut lo = [f64::INFINITY; 2];
            let mut hi = [f64::NEG_INFINITY; 2];
            for e in 0..acc.count {
                for k in 0..2 {
                    let x = acc.read_raw(data, e, k);
                    lo[k] = lo[k].min(x);
                    hi[k] = hi[k].max(x);
                }
            }
            json["accessors"][a]["min"] = json!(lo);
            json["accessors"][a]["max"] = json!(hi);
            json_changed = true;
        }

        // Buffers and images: re-encode, write next to the output, or re-point URIs.
        let out_dir = parent_dir(out_path).canonicalize()?;
        let same_dir = out_dir == self.base_dir;
        let stem = out_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("model")
            .to_string();
        let mut files = Vec::new();
        let mut taken: HashSet<String> = HashSet::new();
        if same_dir {
            for (i, s) in sources.iter().enumerate() {
                if let BufferSource::File { uri } = s
                    && !changed[i]
                    && !uri.is_empty()
                {
                    taken.insert(percent_decode(uri));
                }
            }
        }
        let written = (0..sources.len())
            .filter(|&i| changed[i] && matches!(sources[i], BufferSource::File { .. }))
            .count();
        for i in 0..sources.len() {
            match &sources[i] {
                BufferSource::GlbBin => {}
                BufferSource::DataUri { prefix } => {
                    if changed[i] {
                        let encoded = base64::engine::general_purpose::STANDARD.encode(&buffers[i]);
                        json["buffers"][i]["uri"] = json!(format!("{prefix}{encoded}"));
                        json_changed = true;
                    }
                }
                BufferSource::File { uri } => {
                    if changed[i] {
                        let suffix = if written == 1 {
                            ".bin".to_string()
                        } else {
                            format!("_{i}.bin")
                        };
                        let name = unique_name(&stem, &suffix, &mut taken);
                        files.push((out_dir.join(&name), buffers[i].clone()));
                        json["buffers"][i]["uri"] = json!(percent_encode(&name));
                        json_changed = true;
                    } else if !same_dir {
                        let target = self.base_dir.join(percent_decode(uri));
                        let target = target.canonicalize().unwrap_or(target);
                        json["buffers"][i]["uri"] =
                            json!(path_to_uri(&relative_path(&out_dir, &target)));
                        json_changed = true;
                    }
                }
            }
        }
        if !same_dir {
            for img in json["images"].as_array_mut().into_iter().flatten() {
                if let Some(uri) = img.get("uri").and_then(Value::as_str)
                    && !is_data_uri(uri)
                {
                    let target = self.base_dir.join(percent_decode(uri));
                    let target = target.canonicalize().unwrap_or(target);
                    img["uri"] = json!(path_to_uri(&relative_path(&out_dir, &target)));
                    json_changed = true;
                }
            }
        }

        let main = match self.container {
            Container::Gltf if !json_changed => self.original.clone(),
            Container::Gltf => serde_json::to_vec_pretty(&json)?,
            Container::Glb => {
                let json_bytes = if json_changed {
                    serde_json::to_vec(&json)?
                } else {
                    self.json_chunk.clone()
                };
                let bin =
                    (sources.first() == Some(&BufferSource::GlbBin)).then(|| buffers[0].as_slice());
                build_glb(&json_bytes, bin, &self.extra_chunks)
            }
        };
        files.push((out_path.to_path_buf(), main));
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Temporary directory removed when dropped.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> Self {
            let d =
                std::env::temp_dir().join(format!("uvcraft-gltf-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            TempDir(d.canonicalize().unwrap())
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Quad made of two triangles sharing glTF vertex 1. Vertex 2 is duplicated
    /// as 5 (same position), as an exporter does for a hard edge; `seam` gives
    /// the duplicate a different UV. Element 4 is unused.
    fn quad(with_uv: bool, seam: bool) -> (Value, Vec<u8>) {
        let pos: [[f32; 3]; 6] = [
            [0., 0., 0.],
            [1., 0., 0.],
            [0., 1., 0.],
            [1., 1., 0.],
            [1., 0., 0.],
            [0., 1., 0.],
        ];
        let d = if seam { 0.5 } else { 0.0 };
        let uv: [[f32; 2]; 6] = [
            [0., 1.],
            [1., 1.],
            [0., 0.],
            [1., 0.],
            [1. + d, 1.],
            [0. + d, 0.],
        ];
        let idx: [u16; 6] = [0, 1, 2, 5, 1, 3];
        let mut bin = Vec::new();
        for p in pos {
            for c in p {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
        let uv_offset = bin.len();
        for p in uv {
            for c in p {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
        let idx_offset = bin.len();
        for i in idx {
            bin.extend_from_slice(&i.to_le_bytes());
        }
        let mut attributes = json!({ "POSITION": 0 });
        if with_uv {
            attributes["TEXCOORD_0"] = json!(1);
        }
        let json = json!({
            "asset": { "version": "2.0" },
            "buffers": [{ "byteLength": bin.len() }],
            "bufferViews": [
                { "buffer": 0, "byteOffset": 0, "byteLength": uv_offset },
                { "buffer": 0, "byteOffset": uv_offset, "byteLength": idx_offset - uv_offset },
                { "buffer": 0, "byteOffset": idx_offset, "byteLength": 12 }
            ],
            "accessors": [
                { "bufferView": 0, "componentType": FLOAT, "count": 6, "type": "VEC3", "min": [0,0,0], "max": [1,1,0] },
                { "bufferView": 1, "componentType": FLOAT, "count": 6, "type": "VEC2", "min": [0,0], "max": [1.5,1] },
                { "bufferView": 2, "componentType": UNSIGNED_SHORT, "count": 6, "type": "SCALAR" }
            ],
            "meshes": [{ "name": "Quad", "primitives": [{ "attributes": attributes, "indices": 2 }] }]
        });
        (json, bin)
    }

    fn glb(with_uv: bool, seam: bool) -> Vec<u8> {
        let (json, bin) = quad(with_uv, seam);
        build_glb(&serde_json::to_vec(&json).unwrap(), Some(&bin), &[])
    }

    fn gltf_data_uri(with_uv: bool) -> Vec<u8> {
        let (mut json, bin) = quad(with_uv, false);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bin);
        json["buffers"][0]["uri"] =
            json!(format!("data:application/octet-stream;base64,{encoded}"));
        serde_json::to_vec_pretty(&json).unwrap()
    }

    fn islands(doc: &GltfDocument) -> usize {
        uvcraft_core::Islands::compute(&doc.mesh, &doc.uv).count()
    }

    #[test]
    fn welds_split_vertices_and_flips_v() {
        let dir = TempDir::new("weld");
        let doc = parse(glb(true, false), &dir.0).unwrap();
        assert_eq!(doc.mesh.vertex_count(), 4);
        assert_eq!(doc.mesh.face_count(), 2);
        assert_eq!(
            doc.uv.uvs.len(),
            4,
            "duplicates with equal UVs share one UV vertex"
        );
        assert_eq!(islands(&doc), 1);
        assert_eq!(doc.objects, vec!["Quad"]);
        // glTF (0, 1) is UvCraft (0, 0).
        assert_eq!(doc.uv.uv_at_corner(0), DVec2::ZERO);

        let seam = parse(glb(true, true), &dir.0).unwrap();
        assert_eq!(seam.mesh.vertex_count(), 4);
        assert_eq!(seam.uv.uvs.len(), 5);
        assert_eq!(islands(&seam), 2);
    }

    #[test]
    fn unchanged_glb_round_trips_byte_for_byte() {
        let dir = TempDir::new("glb-same");
        let bytes = glb(true, false);
        let doc = parse(bytes.clone(), &dir.0).unwrap();
        let files = doc
            .source
            .export(&doc.mesh, &doc.uv, &dir.0.join("out.glb"))
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, bytes);
    }

    #[test]
    fn changed_uvs_are_written_in_place() {
        let dir = TempDir::new("glb-edit");
        let mut doc = parse(glb(true, false), &dir.0).unwrap();
        for p in &mut doc.uv.uvs {
            *p = *p * 0.5 + DVec2::splat(0.25);
        }
        let out = dir.0.join("out.glb");
        let files = doc.source.export(&doc.mesh, &doc.uv, &out).unwrap();
        let again = parse(files[0].1.clone(), &dir.0).unwrap();
        assert_eq!(again.uv, doc.uv);
        assert_eq!(again.mesh.positions(), doc.mesh.positions());
        let acc = &again.source.json["accessors"][1];
        assert_eq!(acc["min"], json!([0.25, 0.25]));
        // Unused element 4 keeps its stored (1, 1).
        assert_eq!(acc["max"], json!([1.0, 1.0]));
    }

    #[test]
    fn data_uri_buffer_is_reencoded() {
        let dir = TempDir::new("data-uri");
        let bytes = gltf_data_uri(true);
        let mut doc = parse(bytes.clone(), &dir.0).unwrap();
        let out = dir.0.join("out.gltf");
        assert_eq!(
            doc.source.export(&doc.mesh, &doc.uv, &out).unwrap()[0].1,
            bytes
        );
        // glTF stores f32: use exactly representable values.
        doc.uv.uvs[0] = DVec2::new(0.125, 0.375);
        let files = doc.source.export(&doc.mesh, &doc.uv, &out).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(parse(files[0].1.clone(), &dir.0).unwrap().uv, doc.uv);
    }

    #[test]
    fn missing_texcoords_get_a_new_accessor() {
        let dir = TempDir::new("missing");
        let mut doc = parse(glb(false, false), &dir.0).unwrap();
        assert_eq!(doc.missing_uv_corners, 6);
        assert_eq!(doc.uv.uvs.len(), 4);
        for (i, p) in doc.uv.uvs.iter_mut().enumerate() {
            *p = DVec2::new(i as f64 / 4.0, 0.5);
        }
        let files = doc
            .source
            .export(&doc.mesh, &doc.uv, &dir.0.join("out.glb"))
            .unwrap();
        let again = parse(files[0].1.clone(), &dir.0).unwrap();
        assert_eq!(again.missing_uv_corners, 0);
        assert_eq!(again.uv.uvs.len(), 4);
        for c in 0..doc.mesh.corner_count() {
            assert_eq!(again.uv.uv_at_corner(c), doc.uv.uv_at_corner(c));
        }
    }

    #[test]
    fn external_buffers_move_with_the_output() {
        let src = TempDir::new("ext-src");
        let (mut json, bin) = quad(true, false);
        json["buffers"][0]["uri"] = json!("quad%20data.bin");
        json["images"] = json!([{ "uri": "tex.png" }]);
        std::fs::write(src.0.join("quad data.bin"), &bin).unwrap();
        std::fs::write(src.0.join("tex.png"), b"png").unwrap();
        let path = src.0.join("quad.gltf");
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();

        let mut doc = read(&path).unwrap();
        doc.uv.uvs[1].x = 0.75;
        let out_dir = TempDir::new("ext-out");
        let out = out_dir.0.join("result.gltf");
        write(&out, &doc.source, &doc.mesh, &doc.uv).unwrap();
        assert!(out_dir.0.join("result.bin").exists());
        let written: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(written["buffers"][0]["uri"], json!("result.bin"));
        let img = written["images"][0]["uri"].as_str().unwrap();
        assert!(img.ends_with("/tex.png") && img.starts_with("../"), "{img}");
        assert_eq!(read(&out).unwrap().uv, doc.uv);
        // The source buffer is untouched.
        assert_eq!(std::fs::read(src.0.join("quad data.bin")).unwrap(), bin);
    }

    #[test]
    fn saving_next_to_source_does_not_overwrite_kept_buffers() {
        let dir = TempDir::new("no-clobber");
        let (mut json, bin) = quad(false, false);
        json["buffers"][0]["uri"] = json!("model.bin");
        std::fs::write(dir.0.join("model.bin"), &bin).unwrap();
        let path = dir.0.join("model.gltf");
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();

        let mut doc = read(&path).unwrap();
        doc.uv.uvs[0] = DVec2::splat(0.5);
        write(&path, &doc.source, &doc.mesh, &doc.uv).unwrap();
        assert_eq!(std::fs::read(dir.0.join("model.bin")).unwrap(), bin);
        assert!(dir.0.join("model_1.bin").exists());
        assert_eq!(read(&path).unwrap().uv.uv_at_corner(0), DVec2::splat(0.5));
    }

    #[test]
    fn rejects_what_it_cannot_write() {
        let dir = TempDir::new("reject");
        let doc = parse(glb(true, false), &dir.0).unwrap();
        let out = dir.0.join("out.gltf");
        assert!(matches!(
            doc.source.export(&doc.mesh, &doc.uv, &out),
            Err(GltfError::Unsupported(_))
        ));

        let mut json = quad(true, false).0;
        json["extensionsRequired"] = json!(["KHR_draco_mesh_compression"]);
        let bytes = build_glb(
            &serde_json::to_vec(&json).unwrap(),
            Some(&quad(true, false).1),
            &[],
        );
        assert!(matches!(
            parse(bytes, &dir.0),
            Err(GltfError::Unsupported(_))
        ));
    }

    #[test]
    fn cut_through_shared_vertex_splits_it() {
        let dir = TempDir::new("split");
        for bytes in [glb(true, false), glb(false, false)] {
            let mut doc = parse(bytes, &dir.0).unwrap();
            // Give corner 1 its own UV: its glTF vertex is shared with corner 4.
            doc.uv.uvs.push(DVec2::splat(0.75));
            doc.uv.corner_uv[1] = doc.uv.uvs.len() as u32 - 1;
            let files = doc
                .source
                .export(&doc.mesh, &doc.uv, &dir.0.join("out.glb"))
                .unwrap();
            let again = parse(files[0].1.clone(), &dir.0).unwrap();
            assert_eq!(again.mesh.positions(), doc.mesh.positions());
            assert_eq!(again.mesh.face_count(), 2);
            for c in 0..doc.mesh.corner_count() {
                assert_eq!(
                    again.uv.uv_at_corner(c),
                    doc.uv.uv_at_corner(c),
                    "corner {c}"
                );
            }
            let prim = &again.source.json["meshes"][0]["primitives"][0];
            assert_eq!(prim["mode"], json!(4));
            let pos = &again.source.json["accessors"]
                [prim["attributes"]["POSITION"].as_u64().unwrap() as usize];
            assert_eq!(pos["count"], json!(7), "one vertex duplicated");
            assert!(pos.get("min").is_some() && pos.get("max").is_some());
        }
    }

    #[test]
    fn strips_fans_and_points() {
        assert_eq!(
            triangles(5, &[0, 1, 2, 3]).unwrap(),
            vec![[0, 1, 2], [2, 1, 3]]
        );
        assert_eq!(
            triangles(6, &[0, 1, 2, 3]).unwrap(),
            vec![[0, 1, 2], [0, 2, 3]]
        );
        let (mut json, bin) = quad(true, false);
        json["meshes"][0]["primitives"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "attributes": { "POSITION": 0 }, "mode": 0 }));
        let dir = TempDir::new("points");
        let doc = parse(
            build_glb(&serde_json::to_vec(&json).unwrap(), Some(&bin), &[]),
            &dir.0,
        )
        .unwrap();
        assert_eq!(doc.skipped_primitives, 1);
        assert_eq!(doc.mesh.face_count(), 2);
    }

    #[test]
    fn normalized_texcoords() {
        let acc = Accessor {
            buffer: 0,
            offset: 0,
            stride: 4,
            count: 1,
            component: UNSIGNED_SHORT,
            components: 2,
            normalized: true,
        };
        let mut data = vec![0u8; 4];
        acc.write(&mut data, 0, 0, 0.5).unwrap();
        assert!((acc.read(&data, 0, 0) - 0.5).abs() < 1e-4);
        assert!(acc.write(&mut data, 0, 1, 1.5).is_err());
    }
}
