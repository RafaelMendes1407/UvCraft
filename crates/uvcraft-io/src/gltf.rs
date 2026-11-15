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
//! Additional TEXCOORD channels can be selected through `activate_uv_channel`.
//! Draco primitives and meshopt buffer views are decoded before mesh assembly;
//! edited exports write ordinary glTF buffers. Node transforms are exposed
//! for measurements through `measurement_positions`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use glam::{DMat4, DQuat, DVec2, DVec3};
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
    /// Element of an existing active TEXCOORD accessor.
    Accessor(usize),
    /// Primitive (index into `prims`) that has no active TEXCOORD accessor yet.
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
    expanded_sparse: bool,
    vertex_mesh: Vec<usize>,
    active_channel: u32,
    prechanged_buffers: BTreeSet<usize>,
    prechanged_accessors: BTreeSet<usize>,
    prechanged_json: bool,
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

pub(crate) fn build_glb(json: &[u8], bin: Option<&[u8]>, extra: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut chunks: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut j = json.to_vec();
    while !j.len().is_multiple_of(4) {
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

/// Resolves an external buffer URI against the asset's directory. Only
/// regular files inside that directory are accepted: absolute paths, URI
/// schemes (`file:`, `http:`, ...), `..` segments, symlinks leading outside
/// it and device files or FIFOs (`/dev/zero` would never finish reading) are
/// rejected, so an untrusted asset cannot make the loader read, and a later
/// `.glb` save embed, an arbitrary file.
fn external_buffer_path(base_dir: &Path, uri: &str) -> Result<PathBuf, String> {
    let decoded = percent_decode(uri);
    let has_scheme = decoded.split_once(':').is_some_and(|(scheme, _)| {
        scheme.len() > 1
            && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    });
    let relative = Path::new(&decoded);
    if has_scheme
        || decoded.contains('\0')
        || relative.is_absolute()
        || relative.has_root()
        || relative
            .components()
            .any(|c| matches!(c, Component::Prefix(_) | Component::ParentDir))
    {
        return Err(format!(
            "unsupported external URI {uri:?} (must be a path inside the asset's folder)"
        ));
    }
    let path = base_dir.join(relative);
    let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let real = path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let root = base_dir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", base_dir.display()))?;
    if !real.starts_with(&root) {
        return Err(format!(
            "{} leads outside the asset's folder",
            path.display()
        ));
    }
    Ok(path)
}

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

pub(crate) fn percent_encode(s: &str) -> String {
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

/// Materialize sparse accessors in buffer zero. The original file remains
/// available for byte-identical export when no UVs are edited.
fn expand_sparse(json: &mut Value, buffers: &mut [Vec<u8>]) -> Result<bool, GltfError> {
    let Some(accessors) = json["accessors"].as_array() else {
        return Ok(false);
    };
    let sparse_ids: Vec<usize> = accessors
        .iter()
        .enumerate()
        .filter_map(|(i, a)| a.get("sparse").map(|_| i))
        .collect();
    if sparse_ids.is_empty() {
        return Ok(false);
    }
    if buffers.is_empty() {
        return Err(invalid("sparse accessors require a buffer"));
    }
    for i in sparse_ids {
        let a = json["accessors"][i].clone();
        let size = component_size(get_usize(&a, "componentType").unwrap_or(0) as u32)
            .ok_or_else(|| invalid(format!("accessor {i} has invalid component type")))?;
        let components = match a["type"].as_str() {
            Some("SCALAR") => 1,
            Some("VEC2") => 2,
            Some("VEC3") => 3,
            Some("VEC4") => 4,
            _ => return Err(GltfError::Unsupported(format!("sparse accessor {i} type"))),
        };
        let count =
            get_usize(&a, "count").ok_or_else(|| invalid(format!("accessor {i} has no count")))?;
        let elem_size = size * components;
        let mut dense = vec![
            0u8;
            count
                .checked_mul(elem_size)
                .ok_or_else(|| invalid("accessor too large"))?
        ];
        if let Some(view_id) = get_usize(&a, "bufferView") {
            let view = json["bufferViews"]
                .get(view_id)
                .ok_or_else(|| invalid("missing base bufferView"))?;
            let b = get_usize(view, "buffer")
                .ok_or_else(|| invalid("base bufferView has no buffer"))?;
            let data = buffers
                .get(b)
                .ok_or_else(|| invalid("base buffer missing"))?;
            let offset = get_usize(view, "byteOffset").unwrap_or(0)
                + get_usize(&a, "byteOffset").unwrap_or(0);
            let stride = get_usize(view, "byteStride").unwrap_or(elem_size);
            let view_end = get_usize(view, "byteOffset").unwrap_or(0)
                + get_usize(view, "byteLength").unwrap_or(0);
            for e in 0..count {
                let start = offset
                    .checked_add(
                        e.checked_mul(stride)
                            .ok_or_else(|| invalid("base accessor too large"))?,
                    )
                    .ok_or_else(|| invalid("base accessor too large"))?;
                let src = data
                    .get(start..start + elem_size)
                    .filter(|_| start + elem_size <= view_end)
                    .ok_or_else(|| invalid("base accessor exceeds bufferView"))?;
                dense[e * elem_size..(e + 1) * elem_size].copy_from_slice(src);
            }
        }
        let sparse = &a["sparse"];
        let n =
            get_usize(sparse, "count").ok_or_else(|| invalid("sparse accessor has no count"))?;
        if n > count {
            return Err(invalid("sparse count exceeds accessor count"));
        }
        let indices = &sparse["indices"];
        let values = &sparse["values"];
        let iv = json["bufferViews"]
            .get(
                get_usize(indices, "bufferView")
                    .ok_or_else(|| invalid("sparse indices have no bufferView"))?,
            )
            .ok_or_else(|| invalid("missing sparse index bufferView"))?;
        let vv = json["bufferViews"]
            .get(
                get_usize(values, "bufferView")
                    .ok_or_else(|| invalid("sparse values have no bufferView"))?,
            )
            .ok_or_else(|| invalid("missing sparse value bufferView"))?;
        let ib = buffers
            .get(
                get_usize(iv, "buffer")
                    .ok_or_else(|| invalid("sparse index bufferView has no buffer"))?,
            )
            .ok_or_else(|| invalid("sparse index buffer missing"))?;
        let vb = buffers
            .get(
                get_usize(vv, "buffer")
                    .ok_or_else(|| invalid("sparse value bufferView has no buffer"))?,
            )
            .ok_or_else(|| invalid("sparse value buffer missing"))?;
        let index_ty = get_usize(indices, "componentType").unwrap_or(0) as u32;
        let index_size = match index_ty {
            UNSIGNED_BYTE => 1,
            UNSIGNED_SHORT => 2,
            UNSIGNED_INT => 4,
            _ => return Err(invalid("sparse index component type")),
        };
        let io = get_usize(iv, "byteOffset").unwrap_or(0)
            + get_usize(indices, "byteOffset").unwrap_or(0);
        let vo =
            get_usize(vv, "byteOffset").unwrap_or(0) + get_usize(values, "byteOffset").unwrap_or(0);
        let ie =
            get_usize(iv, "byteOffset").unwrap_or(0) + get_usize(iv, "byteLength").unwrap_or(0);
        let ve =
            get_usize(vv, "byteOffset").unwrap_or(0) + get_usize(vv, "byteLength").unwrap_or(0);
        let mut previous = None;
        for j in 0..n {
            let at = io + j * index_size;
            let bytes = ib
                .get(at..at + index_size)
                .filter(|_| at + index_size <= ie)
                .ok_or_else(|| invalid("sparse indices exceed bufferView"))?;
            let index = match index_ty {
                UNSIGNED_BYTE => bytes[0] as usize,
                UNSIGNED_SHORT => u16::from_le_bytes(bytes.try_into().unwrap()) as usize,
                _ => u32::from_le_bytes(bytes.try_into().unwrap()) as usize,
            };
            if index >= count || previous.is_some_and(|p| index <= p) {
                return Err(invalid("sparse indices must be increasing and in range"));
            }
            previous = Some(index);
            let at = vo + j * elem_size;
            let src = vb
                .get(at..at + elem_size)
                .filter(|_| at + elem_size <= ve)
                .ok_or_else(|| invalid("sparse values exceed bufferView"))?;
            dense[index * elem_size..(index + 1) * elem_size].copy_from_slice(src);
        }
        let target = &mut buffers[0];
        target.resize(target.len().next_multiple_of(4), 0);
        let offset = target.len();
        target.extend_from_slice(&dense);
        let views = json["bufferViews"]
            .as_array_mut()
            .ok_or_else(|| invalid("no bufferViews array"))?;
        views.push(json!({"buffer":0,"byteOffset":offset,"byteLength":dense.len()}));
        let view_id = views.len() - 1;
        let acc = &mut json["accessors"][i];
        acc["bufferView"] = json!(view_id);
        acc["byteOffset"] = json!(0);
        acc.as_object_mut().unwrap().remove("sparse");
    }
    json["buffers"][0]["byteLength"] = json!(buffers[0].len());
    Ok(true)
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
            None if json["extensionsRequired"].as_array().is_some_and(|names| {
                names.iter().any(|n| {
                    matches!(
                        n.as_str(),
                        Some("EXT_meshopt_compression" | "KHR_meshopt_compression")
                    )
                })
            }) =>
            {
                (
                    vec![0; len],
                    BufferSource::DataUri {
                        prefix: "data:application/octet-stream;base64,".into(),
                    },
                )
            }
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
                let path = external_buffer_path(base_dir, uri)
                    .map_err(|e| invalid(format!("buffer {i}: {e}")))?;
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

/// Materialize EXT/KHR meshopt views into their declared uncompressed buffer
/// ranges before accessor and sparse parsing. The original bytes remain on
/// `GltfSource` only for source attribution; edited exports use decoded data.
fn decode_meshopt_color(bytes: &mut [u8], stride: usize) -> Result<(), GltfError> {
    if !matches!(stride, 4 | 8) || !bytes.len().is_multiple_of(stride) {
        return Err(invalid("meshopt COLOR requires 4 or 8 byte elements"));
    }
    let max = if stride == 4 { 255_i64 } else { 65535_i64 };
    for element in bytes.chunks_exact_mut(stride) {
        let (y, co, cg, alpha) = if stride == 4 {
            (
                i64::from(element[0]),
                i64::from(element[1] as i8),
                i64::from(element[2] as i8),
                u32::from(element[3]),
            )
        } else {
            (
                i64::from(u16::from_le_bytes([element[0], element[1]])),
                i64::from(i16::from_le_bytes([element[2], element[3]])),
                i64::from(i16::from_le_bytes([element[4], element[5]])),
                u32::from(u16::from_le_bytes([element[6], element[7]])),
            )
        };
        if alpha < 2 {
            return Err(invalid("meshopt COLOR has invalid alpha scale"));
        }
        let alpha_scale = (1_i64 << (32 - alpha.leading_zeros())) - 1;
        let a = i64::from(alpha) & (alpha_scale >> 1);
        let a = (a << 1) | (a & 1);
        let channels = [y + co - cg, y + cg, y - co - cg, a];
        for (i, value) in channels.into_iter().enumerate() {
            if !(0..=alpha_scale).contains(&value) {
                return Err(invalid("meshopt COLOR component out of range"));
            }
            let expanded = (value * max + alpha_scale / 2) / alpha_scale;
            if stride == 4 {
                element[i] = expanded as u8;
            } else {
                element[2 * i..2 * i + 2].copy_from_slice(&(expanded as u16).to_le_bytes());
            }
        }
    }
    Ok(())
}

fn decode_meshopt_views(
    json: &mut Value,
    buffers: &mut [Vec<u8>],
) -> Result<BTreeSet<usize>, GltfError> {
    let mut changed = BTreeSet::new();
    let Some(views) = json["bufferViews"].as_array_mut() else {
        return Ok(changed);
    };
    for (index, view) in views.iter_mut().enumerate() {
        let ext_name = ["EXT_meshopt_compression", "KHR_meshopt_compression"]
            .into_iter()
            .find(|name| view["extensions"].get(name).is_some());
        let Some(ext_name) = ext_name else { continue };
        let ext = &view["extensions"][ext_name];
        let source =
            get_usize(ext, "buffer").ok_or_else(|| invalid("meshopt source buffer missing"))?;
        let source_offset = get_usize(ext, "byteOffset").unwrap_or(0);
        let source_len =
            get_usize(ext, "byteLength").ok_or_else(|| invalid("meshopt byteLength missing"))?;
        let count = get_usize(ext, "count").ok_or_else(|| invalid("meshopt count missing"))?;
        let stride =
            get_usize(ext, "byteStride").ok_or_else(|| invalid("meshopt byteStride missing"))?;
        let decoded_len = count
            .checked_mul(stride)
            .ok_or_else(|| invalid("meshopt decoded length overflow"))?;
        if get_usize(view, "byteLength") != Some(decoded_len) {
            return Err(invalid(format!(
                "meshopt view {index} decoded length mismatch"
            )));
        }
        let source_end = source_offset
            .checked_add(source_len)
            .ok_or_else(|| invalid("meshopt source overflow"))?;
        let encoded = buffers
            .get(source)
            .and_then(|b| b.get(source_offset..source_end))
            .ok_or_else(|| invalid(format!("meshopt view {index} compressed data out of range")))?
            .to_vec();
        let mode = ext["mode"]
            .as_str()
            .ok_or_else(|| invalid("meshopt mode missing"))?;
        let filter = ext["filter"].as_str().unwrap_or("NONE");
        let mode = awsm_renderer_codec_meshopt::Mode::from_gltf(mode)
            .ok_or_else(|| invalid(format!("meshopt view {index} invalid mode")))?;
        let color_filter = filter == "COLOR" && ext_name == "KHR_meshopt_compression";
        let filter = if color_filter {
            awsm_renderer_codec_meshopt::Filter::None
        } else {
            awsm_renderer_codec_meshopt::Filter::from_gltf(filter).ok_or_else(|| {
                invalid(format!("meshopt view {index} unsupported filter {filter}"))
            })?
        };
        let mut decoded =
            awsm_renderer_codec_meshopt::decode_buffer_view(&encoded, count, stride, mode, filter)
                .map_err(|e| invalid(format!("meshopt view {index} decode failed: {e}")))?;
        if color_filter {
            decode_meshopt_color(&mut decoded, stride)?;
        }
        let target =
            get_usize(view, "buffer").ok_or_else(|| invalid("meshopt target buffer missing"))?;
        let target_offset = get_usize(view, "byteOffset").unwrap_or(0);
        let target_end = target_offset
            .checked_add(decoded_len)
            .ok_or_else(|| invalid("meshopt target overflow"))?;
        let destination = buffers
            .get_mut(target)
            .and_then(|b| b.get_mut(target_offset..target_end))
            .ok_or_else(|| invalid(format!("meshopt view {index} target out of range")))?;
        destination.copy_from_slice(&decoded);
        view["extensions"].as_object_mut().unwrap().remove(ext_name);
        changed.insert(target);
    }
    if !changed.is_empty() {
        for list in ["extensionsRequired", "extensionsUsed"] {
            if let Some(names) = json[list].as_array_mut() {
                names.retain(|name| {
                    !matches!(
                        name.as_str(),
                        Some("EXT_meshopt_compression" | "KHR_meshopt_compression")
                    )
                });
            }
        }
    }
    Ok(changed)
}

fn draco_component_type(
    ty: draco_oxide_core::attribute::ComponentDataType,
) -> Result<u32, GltfError> {
    use draco_oxide_core::attribute::ComponentDataType as C;
    Ok(match ty {
        C::I8 => BYTE,
        C::U8 => UNSIGNED_BYTE,
        C::I16 => SHORT,
        C::U16 => UNSIGNED_SHORT,
        C::U32 => UNSIGNED_INT,
        C::F32 => FLOAT,
        _ => {
            return Err(GltfError::Unsupported(format!(
                "Draco component type {ty:?}"
            )));
        }
    })
}

/// Decode Draco primitives into ordinary accessors while retaining their
/// decoded attribute and triangle order. Compressed source data stays unused.
fn decode_draco_primitives(
    json: &mut Value,
    buffers: &mut Vec<Vec<u8>>,
    sources: &mut Vec<BufferSource>,
) -> Result<BTreeSet<usize>, GltfError> {
    let mut tasks = Vec::new();
    for (mi, mesh) in json["meshes"].as_array().into_iter().flatten().enumerate() {
        for (pi, primitive) in mesh["primitives"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let Some(ext) = primitive["extensions"].get("KHR_draco_mesh_compression") else {
                continue;
            };
            let view =
                get_usize(ext, "bufferView").ok_or_else(|| invalid("Draco bufferView missing"))?;
            let mut attrs = Vec::new();
            for (semantic, draco_id) in ext["attributes"].as_object().into_iter().flatten() {
                let draco_id = draco_id
                    .as_u64()
                    .ok_or_else(|| invalid("Draco attribute ID invalid"))?
                    as usize;
                let accessor = get_usize(&primitive["attributes"], semantic)
                    .ok_or_else(|| invalid("Draco attribute accessor missing"))?;
                attrs.push((semantic.clone(), draco_id, accessor));
            }
            tasks.push((mi, pi, view, attrs, get_usize(primitive, "indices")));
        }
    }
    let mut changed = BTreeSet::new();
    for (mi, pi, view_id, attrs, index_accessor) in tasks {
        let view = json["bufferViews"]
            .get(view_id)
            .ok_or_else(|| invalid("Draco bufferView out of range"))?;
        let buffer_id = get_usize(view, "buffer").ok_or_else(|| invalid("Draco buffer missing"))?;
        let offset = get_usize(view, "byteOffset").unwrap_or(0);
        let length =
            get_usize(view, "byteLength").ok_or_else(|| invalid("Draco byteLength missing"))?;
        let end = offset
            .checked_add(length)
            .ok_or_else(|| invalid("Draco byte range overflow"))?;
        let compressed = buffers
            .get(buffer_id)
            .and_then(|b| b.get(offset..end))
            .ok_or_else(|| invalid("Draco compressed bytes out of range"))?;
        let decoded = draco_oxide_decoder::decode_mesh(compressed)
            .map_err(|e| invalid(format!("Draco decode failed: {e}")))?;
        let mut bytes = Vec::new();
        let new_buffer = buffers.len();
        for (semantic, draco_id, accessor_id) in attrs {
            let attr = decoded
                .attributes
                .iter()
                .find(|a| a.get_id().as_usize() == draco_id)
                .ok_or_else(|| invalid(format!("Draco {semantic} attribute {draco_id} missing")))?;
            let components = attr.get_num_components();
            let component = draco_component_type(attr.get_component_type())?;
            let ty = match components {
                1 => "SCALAR",
                2 => "VEC2",
                3 => "VEC3",
                4 => "VEC4",
                _ => {
                    return Err(invalid(format!(
                        "Draco {semantic} component count {components}"
                    )));
                }
            };
            let stride = components
                .checked_mul(attr.get_component_type().size())
                .ok_or_else(|| invalid("Draco attribute stride overflow"))?;
            let raw = attr.get_data_as_bytes();
            let point_map = attr.point_map_as_slice();
            while bytes.len() % 4 != 0 {
                bytes.push(0);
            }
            let start = bytes.len();
            for point in 0..attr.len() {
                let value = point_map.map_or(point, |map| usize::from(map[point]));
                let value_start = value
                    .checked_mul(stride)
                    .ok_or_else(|| invalid("Draco attribute offset overflow"))?;
                let value_end = value_start
                    .checked_add(stride)
                    .ok_or_else(|| invalid("Draco attribute offset overflow"))?;
                bytes.extend_from_slice(
                    raw.get(value_start..value_end)
                        .ok_or_else(|| invalid("Draco attribute data out of range"))?,
                );
            }
            let view_id = json["bufferViews"].as_array().map_or(0, Vec::len);
            json["bufferViews"].as_array_mut().ok_or_else(|| invalid("bufferViews missing"))?
                .push(json!({"buffer": new_buffer, "byteOffset": start, "byteLength": bytes.len() - start}));
            let accessor = json["accessors"]
                .get_mut(accessor_id)
                .ok_or_else(|| invalid("Draco accessor out of range"))?;
            accessor["bufferView"] = json!(view_id);
            accessor["byteOffset"] = json!(0);
            accessor["componentType"] = json!(component);
            accessor["count"] = json!(attr.len());
            accessor["type"] = json!(ty);
            accessor.as_object_mut().unwrap().remove("sparse");
            accessor.as_object_mut().unwrap().remove("normalized");
        }
        let accessor_id = if let Some(id) = index_accessor {
            id
        } else {
            let accessors = json["accessors"]
                .as_array_mut()
                .ok_or_else(|| invalid("accessors missing"))?;
            let id = accessors.len();
            accessors.push(json!({}));
            json["meshes"][mi]["primitives"][pi]["indices"] = json!(id);
            id
        };
        {
            while bytes.len() % 4 != 0 {
                bytes.push(0);
            }
            let start = bytes.len();
            for face in &decoded.faces {
                for &point in face {
                    let index = u32::try_from(usize::from(point))
                        .map_err(|_| invalid("Draco point index exceeds u32"))?;
                    bytes.extend_from_slice(&index.to_le_bytes());
                }
            }
            let view_id = json["bufferViews"].as_array().map_or(0, Vec::len);
            json["bufferViews"].as_array_mut().ok_or_else(|| invalid("bufferViews missing"))?
                .push(json!({"buffer": new_buffer, "byteOffset": start, "byteLength": bytes.len() - start}));
            let accessor = json["accessors"]
                .get_mut(accessor_id)
                .ok_or_else(|| invalid("Draco index accessor out of range"))?;
            accessor["bufferView"] = json!(view_id);
            accessor["byteOffset"] = json!(0);
            accessor["componentType"] = json!(UNSIGNED_INT);
            accessor["count"] = json!(decoded.faces.len() * 3);
            accessor["type"] = json!("SCALAR");
            accessor.as_object_mut().unwrap().remove("sparse");
            accessor.as_object_mut().unwrap().remove("normalized");
        }
        json["meshes"][mi]["primitives"][pi]["extensions"]
            .as_object_mut()
            .unwrap()
            .remove("KHR_draco_mesh_compression");
        json["buffers"]
            .as_array_mut()
            .ok_or_else(|| invalid("buffers missing"))?
            .push(
                json!({"byteLength": bytes.len(), "uri": "data:application/octet-stream;base64,"}),
            );
        buffers.push(bytes);
        sources.push(BufferSource::DataUri {
            prefix: "data:application/octet-stream;base64,".into(),
        });
        changed.insert(new_buffer);
    }
    if !changed.is_empty() {
        for list in ["extensionsRequired", "extensionsUsed"] {
            if let Some(names) = json[list].as_array_mut() {
                names.retain(|name| name.as_str() != Some("KHR_draco_mesh_compression"));
            }
        }
    }
    Ok(changed)
}

fn triangles(mode: usize, idx: &[u32]) -> Result<Vec<[u32; 3]>, GltfError> {
    Ok(match mode {
        4 => idx
            .as_chunks::<3>()
            .0
            .iter()
            .map(|t| [t[0], t[1], t[2]])
            .collect(),
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
    let mut json: Value = serde_json::from_slice(&c.json_chunk)?;
    if !json.is_object() {
        return Err(invalid("the glTF JSON root must be an object"));
    }
    let (mut buffers, mut buffer_sources) = load_buffers(&json, c.kind, c.bin, base_dir)?;
    let decoded_meshopt = decode_meshopt_views(&mut json, &mut buffers)?;
    let decoded_draco = decode_draco_primitives(&mut json, &mut buffers, &mut buffer_sources)?;
    let expanded_sparse = expand_sparse(&mut json, &mut buffers)?;

    let mut positions: Vec<DVec3> = Vec::new();
    let mut vertex_mesh = Vec::new();
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
                        vertex_mesh.push(mi);
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
            expanded_sparse,
            vertex_mesh,
            active_channel: 0,
            prechanged_buffers: decoded_meshopt.union(&decoded_draco).copied().collect(),
            prechanged_accessors: BTreeSet::new(),
            prechanged_json: !decoded_meshopt.is_empty() || !decoded_draco.is_empty(),
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
        while !(self.base + self.data.len()).is_multiple_of(4) {
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
    /// Adds a TEXCOORD channel initialized to UV (0,0) at every vertex.
    /// Callers may then export an edited layer through `export_uv_channels`.
    pub fn add_empty_uv_channel(&self, channel: u32) -> Result<Self, GltfError> {
        if self.uv_channels().contains(&channel) {
            return Err(invalid(format!("TEXCOORD_{channel} already exists")));
        }
        if self.buffers.is_empty() {
            return Err(invalid("glTF has no buffer for UV channel"));
        }
        let mut source = self.clone();
        let original_len =
            get_usize(&source.json["buffers"][0], "byteLength").unwrap_or(source.buffers[0].len());
        source.buffers[0].truncate(original_len);
        let mut out = Appender {
            target: 0,
            base: source.buffers[0].len(),
            data: Vec::new(),
        };
        let name = format!("TEXCOORD_{channel}");
        for prim in &self.prims {
            let mut data = Vec::with_capacity(prim.vertex_count * 8);
            for _ in 0..prim.vertex_count {
                data.extend_from_slice(&0.0f32.to_le_bytes());
                data.extend_from_slice(&1.0f32.to_le_bytes());
            }
            let accessor = json!({"componentType":FLOAT,"count":prim.vertex_count,"type":"VEC2"});
            let index = out.push(&mut source.json, data, Some(34962), None, accessor)?;
            source.json["meshes"][prim.mesh]["primitives"][prim.prim]["attributes"][&name] =
                json!(index);
        }
        source.buffers[0].extend_from_slice(&out.data);
        source.json["buffers"][0]["byteLength"] = json!(source.buffers[0].len());
        source.prechanged_buffers.insert(0);
        source.prechanged_json = true;
        Ok(source)
    }

    /// Exchanges two TEXCOORD attribute names without changing their bytes.
    pub fn swap_uv_channels(&self, a: u32, b: u32) -> Result<Self, GltfError> {
        let available = self.uv_channels();
        if !available.contains(&a) || !available.contains(&b) {
            return Err(invalid("both UV channels must exist"));
        }
        if a == b {
            return Ok(self.clone());
        }
        let mut source = self.clone();
        let an = format!("TEXCOORD_{a}");
        let bn = format!("TEXCOORD_{b}");
        for prim in &self.prims {
            let attrs = source.json["meshes"][prim.mesh]["primitives"][prim.prim]["attributes"]
                .as_object_mut()
                .ok_or_else(|| invalid("primitive has no attributes"))?;
            let av = attrs.remove(&an);
            let bv = attrs.remove(&bn);
            if let Some(v) = bv {
                attrs.insert(an.clone(), v);
            }
            if let Some(v) = av {
                attrs.insert(bn.clone(), v);
            }
        }
        source.prechanged_json = true;
        Ok(source)
    }

    /// Exports edits to several existing channels. Independent split layouts
    /// are composed through in-memory GLB passes; no intermediate files are
    /// written, and all vertex attributes are copied onto duplicated slots.
    pub fn export_uv_channels(
        &self,
        mesh: &Mesh,
        layers: &BTreeMap<u32, UvLayer>,
        active: u32,
        out_path: &Path,
    ) -> Result<Vec<(PathBuf, Vec<u8>)>, GltfError> {
        if !layers.contains_key(&active) {
            return Err(invalid("active UV channel missing"));
        }
        let mut split_channels = Vec::new();
        for (&channel, uv) in layers {
            uv.validate(mesh)?;
            let Some((selected, _)) = self.activate_uv_channel(mesh, channel)? else {
                return Err(GltfError::Unsupported(format!(
                    "TEXCOORD_{channel} does not exist"
                )));
            };
            if uv.corner_uv.len() != selected.corner_slot.len() {
                return Err(invalid("UV layer does not belong to this file"));
            }
            let mut values: Vec<Option<DVec2>> = vec![None; selected.slots.len()];
            let needs_split = selected.corner_slot.iter().enumerate().any(|(c, &s)| {
                let p = uv.uv_at_corner(c);
                let entry = &mut values[s as usize];
                if let Some(q) = entry {
                    *q != p
                } else {
                    *entry = Some(p);
                    false
                }
            });
            if needs_split {
                split_channels.push(channel);
            }
        }
        if split_channels.len() > 1 {
            // The existing single-channel rebuild duplicates every attribute,
            // including other TEXCOORDs. Reparse an in-memory GLB after each
            // split so the next pass sees the new slot layout. No intermediate
            // file is written, and face/corner order must stay identical.
            let first = split_channels[0];
            let (selected, _) = self
                .activate_uv_channel(mesh, first)?
                .expect("checked above");
            let temporary = self.base_dir.join("uvcraft-channel-pass.glb");
            let files = selected.export(mesh, &layers[&first], &temporary)?;
            let bytes = files
                .into_iter()
                .find(|(p, _)| *p == temporary)
                .ok_or_else(|| invalid("in-memory GLB pass produced no main file"))?
                .1;
            let next = parse(bytes, &self.base_dir)?;
            if next.mesh.corner_count() != mesh.corner_count()
                || next.mesh.positions() != mesh.positions()
                || (0..mesh.corner_count())
                    .any(|c| next.mesh.corner_vertex(c) != mesh.corner_vertex(c))
            {
                return Err(invalid("in-memory UV pass changed mesh topology"));
            }
            return next
                .source
                .export_uv_channels(&next.mesh, layers, active, out_path);
        }
        let export_channel = split_channels.first().copied().unwrap_or(active);
        let mut source = self.clone();
        for (&channel, uv) in layers {
            if channel == export_channel {
                continue;
            }
            uv.validate(mesh)?;
            let Some((selected, _)) = source.activate_uv_channel(mesh, channel)? else {
                return Err(GltfError::Unsupported(format!(
                    "TEXCOORD_{channel} does not exist"
                )));
            };
            if uv.corner_uv.len() != selected.corner_slot.len() {
                return Err(invalid("UV layer does not belong to this file"));
            }
            let mut values: Vec<Option<DVec2>> = vec![None; selected.slots.len()];
            for (c, &s) in selected.corner_slot.iter().enumerate() {
                let p = uv.uv_at_corner(c);
                let entry = &mut values[s as usize];
                if let Some(q) = entry {
                    if *q != p {
                        return Err(GltfError::Unsupported(format!(
                            "TEXCOORD_{channel} requires a shared vertex split"
                        )));
                    }
                } else {
                    *entry = Some(p);
                }
            }
            for (s, slot) in selected.slots.iter().enumerate() {
                let Some(p) = values[s] else { continue };
                if p == slot.original {
                    continue;
                }
                let SlotTarget::Accessor(a) = slot.target else {
                    return Err(GltfError::Unsupported(format!(
                        "TEXCOORD_{channel} is absent in a primitive"
                    )));
                };
                let acc = parse_accessor(&source.json, &source.buffers, a)?;
                acc.write(
                    &mut source.buffers[acc.buffer],
                    slot.element as usize,
                    0,
                    p.x,
                )?;
                acc.write(
                    &mut source.buffers[acc.buffer],
                    slot.element as usize,
                    1,
                    1.0 - p.y,
                )?;
                source.prechanged_buffers.insert(acc.buffer);
                source.prechanged_accessors.insert(a);
            }
        }
        let uv = &layers[&export_channel];
        let Some((selected, _)) = source.activate_uv_channel(mesh, export_channel)? else {
            return Err(GltfError::Unsupported(format!(
                "TEXCOORD_{export_channel} does not exist"
            )));
        };
        selected.export(mesh, uv, out_path)
    }

    pub fn container(&self) -> Container {
        self.container
    }

    /// Available TEXCOORD channels in triangle primitives.
    pub fn uv_channels(&self) -> Vec<u32> {
        let mut channels = BTreeSet::new();
        for prim in &self.prims {
            let attrs = &self.json["meshes"][prim.mesh]["primitives"][prim.prim]["attributes"];
            for name in attrs.as_object().into_iter().flat_map(|m| m.keys()) {
                if let Some(n) = name
                    .strip_prefix("TEXCOORD_")
                    .and_then(|s| s.parse::<u32>().ok())
                {
                    channels.insert(n);
                }
            }
        }
        channels.into_iter().collect()
    }

    /// Selects a UV channel for editing and export. The original source and
    /// TEXCOORD_0 remain intact; the returned source writes only this channel.
    /// `None` means no triangle primitive contains the requested channel.
    pub fn activate_uv_channel(
        &self,
        mesh: &Mesh,
        channel: u32,
    ) -> Result<Option<(Self, UvLayer)>, GltfError> {
        let name = format!("TEXCOORD_{channel}");
        let mut accessors = Vec::with_capacity(self.prims.len());
        let mut present = false;
        for prim in &self.prims {
            let attr = &self.json["meshes"][prim.mesh]["primitives"][prim.prim]["attributes"];
            let acc = match get_usize(attr, &name) {
                Some(a) => {
                    let parsed = parse_accessor(&self.json, &self.buffers, a)?;
                    if parsed.components != 2 || parsed.count < prim.vertex_count {
                        return Err(invalid(format!(
                            "{name} must be VEC2 with one value per vertex"
                        )));
                    }
                    present = true;
                    Some((a, parsed))
                }
                None => None,
            };
            accessors.push(acc);
        }
        if !present {
            return Ok(None);
        }
        let mut source = self.clone();
        source.active_channel = channel;
        source.slots.clear();
        source.corner_slot.clear();
        let mut slot_of: HashMap<(usize, bool, u32), u32> = HashMap::new();
        let mut uv_of: HashMap<(usize, [u64; 2]), u32> = HashMap::new();
        let mut uvs = Vec::new();
        let mut corner_uv = Vec::with_capacity(self.corner_slot.len());
        for c in 0..self.corner_slot.len() {
            let p = self.corner_prim[c] as usize;
            let e = self.corner_element[c];
            let (target, key, original) = match accessors[p] {
                Some((a, acc)) => {
                    let data = &self.buffers[acc.buffer];
                    (
                        SlotTarget::Accessor(a),
                        (a, true, e),
                        to_internal(acc.read(data, e as usize, 0), acc.read(data, e as usize, 1)),
                    )
                }
                None => (SlotTarget::Missing(p), (p, false, e), DVec2::ZERO),
            };
            let s = *slot_of.entry(key).or_insert_with(|| {
                source.slots.push(Slot {
                    target,
                    element: e,
                    original,
                });
                source.slots.len() as u32 - 1
            });
            source.corner_slot.push(s);
            let vertex = mesh.corner_vertex(c);
            let uv_id = *uv_of
                .entry((vertex, original.to_array().map(f64::to_bits)))
                .or_insert_with(|| {
                    uvs.push(original);
                    uvs.len() as u32 - 1
                });
            corner_uv.push(uv_id);
        }
        let uv = UvLayer {
            name,
            corner_uv,
            uvs,
        };
        uv.validate(mesh)?;
        Ok(Some((source, uv)))
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
        if self.expanded_sparse
            && self.prechanged_buffers.is_empty()
            && !self.prechanged_json
            && wanted == self.container
            && (wanted == Container::Glb || parent_dir(out_path).canonicalize()? == self.base_dir)
            && self
                .corner_slot
                .iter()
                .enumerate()
                .all(|(c, &s)| uv.uv_at_corner(c) == self.slots[s as usize].original)
        {
            return Ok(vec![(out_path.to_path_buf(), self.original.clone())]);
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
        let channel_name = format!("TEXCOORD_{}", self.active_channel);
        let mut json_changed = self.expanded_sparse || self.prechanged_json;
        let mut buffers = self.buffers.clone();
        let mut sources = self.buffer_sources.clone();
        let mut changed = vec![false; buffers.len()];
        if self.expanded_sparse {
            changed[0] = true;
        }
        for &b in &self.prechanged_buffers {
            changed[b] = true;
        }
        let mut touched = self.prechanged_accessors.clone();
        let mut missing: HashMap<usize, Vec<(u32, DVec2)>> = HashMap::new();
        // A normalized integer accessor cannot represent UVs outside its
        // quantized range. Rebuild every primitive using it with FLOAT UVs.
        let mut promote = BTreeSet::new();
        for (s, slot) in self.slots.iter().enumerate() {
            let Some(v) = value[s] else { continue };
            if v == slot.original {
                continue;
            }
            if let SlotTarget::Accessor(a) = slot.target {
                let acc = parse_accessor(&json, &buffers, a)?;
                let within_unit = (0.0..=1.0).contains(&v.x) && (0.0..=1.0).contains(&(1.0 - v.y));
                if acc.component != FLOAT
                    && (!matches!(acc.component, UNSIGNED_BYTE | UNSIGNED_SHORT)
                        || !acc.normalized
                        || !within_unit)
                {
                    promote.insert(a);
                }
            }
        }
        for (c, &slot_id) in self.corner_slot.iter().enumerate() {
            let slot = &self.slots[slot_id as usize];
            if let SlotTarget::Accessor(a) = slot.target
                && promote.contains(&a)
            {
                let uv = value[slot_id as usize].unwrap_or(slot.original);
                missing
                    .entry(self.corner_prim[c] as usize)
                    .or_default()
                    .push((slot.element, uv));
            }
        }
        for (s, slot) in self.slots.iter().enumerate() {
            let Some(v) = value[s] else { continue };
            if v == slot.original {
                continue;
            }
            match slot.target {
                SlotTarget::Accessor(a) => {
                    if promote.contains(&a) {
                        continue;
                    }
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
                        if name != &channel_name {
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
                let existing = match get_usize(&prim_json["attributes"], &channel_name) {
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
                json["meshes"][info.mesh]["primitives"][info.prim]["attributes"][&channel_name] =
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

        // Container conversion.
        if wanted != self.container {
            json_changed = true; // never copy the other container's bytes
        }
        if wanted == Container::Glb && self.container == Container::Gltf && !buffers.is_empty() {
            // Every buffer goes into the BIN chunk, each 4-byte aligned.
            let mut merged = Vec::new();
            let mut offsets = Vec::with_capacity(buffers.len());
            for (i, b) in buffers.iter().enumerate() {
                offsets.push(merged.len());
                let len = get_usize(&json["buffers"][i], "byteLength").unwrap_or(b.len());
                merged.extend_from_slice(&b[..len.min(b.len())]);
                merged.resize(merged.len().next_multiple_of(4), 0);
            }
            for view in json["bufferViews"].as_array_mut().into_iter().flatten() {
                let b = get_usize(view, "buffer").unwrap_or(0);
                let off = get_usize(view, "byteOffset").unwrap_or(0);
                view["buffer"] = json!(0);
                view["byteOffset"] = json!(off + offsets.get(b).copied().unwrap_or(0));
            }
            json["buffers"] = json!([{ "byteLength": merged.len() }]);
            buffers = vec![merged];
            sources = vec![BufferSource::GlbBin];
            changed = vec![true];
            json_changed = true;
        } else if wanted == Container::Gltf
            && self.container == Container::Glb
            && sources.first() == Some(&BufferSource::GlbBin)
        {
            // The BIN chunk becomes `<stem>.bin` (written below).
            let len = get_usize(&json["buffers"][0], "byteLength").unwrap_or(buffers[0].len());
            buffers[0].truncate(len);
            sources[0] = BufferSource::File { uri: String::new() };
            changed[0] = true;
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

        let main = match wanted {
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
                // Unknown chunks only survive a .glb → .glb save.
                let extra: &[_] = if self.container == Container::Glb {
                    &self.extra_chunks
                } else {
                    &[]
                };
                build_glb(&json_bytes, bin, extra)
            }
        };
        files.push((out_path.to_path_buf(), main));
        Ok(files)
    }
}

// ---------------------------------------------------------------- conversion support

/// Data the OBJ converter needs, per corner and per glTF mesh.
pub(crate) struct ConversionData {
    /// glTF mesh index of each corner.
    pub corner_mesh: Vec<u32>,
    /// glTF material index of each corner's primitive, if any.
    pub corner_material: Vec<Option<u32>>,
    /// NORMAL of each corner's glTF vertex, if the primitive has normals.
    pub corner_normal: Vec<Option<DVec3>>,
    /// Name of each glTF mesh (`mesh<N>` when unnamed).
    pub mesh_names: Vec<String>,
    /// Name of each glTF material (`material<N>` when unnamed).
    pub material_names: Vec<String>,
    /// World transform of each glTF mesh when it is simple (see [`GltfSource::conversion_data`]).
    pub mesh_transform: Vec<Option<DMat4>>,
}

fn node_local(node: &Value) -> DMat4 {
    let f = |key: &str, n: usize| -> Option<Vec<f64>> {
        let a = node.get(key)?.as_array()?;
        let v: Vec<f64> = a.iter().filter_map(Value::as_f64).collect();
        (v.len() == n).then_some(v)
    };
    if let Some(m) = f("matrix", 16) {
        return DMat4::from_cols_slice(&m);
    }
    let t = f("translation", 3).map_or(DVec3::ZERO, |v| DVec3::new(v[0], v[1], v[2]));
    let r = f("rotation", 4).map_or(DQuat::IDENTITY, |v| {
        DQuat::from_xyzw(v[0], v[1], v[2], v[3]).normalize()
    });
    let s = f("scale", 3).map_or(DVec3::ONE, |v| DVec3::new(v[0], v[1], v[2]));
    DMat4::from_scale_rotation_translation(s, r, t)
}

impl GltfSource {
    /// Vertex positions for physical 3D measurements. A mesh with exactly one
    /// static, unskinned node uses its world transform. Meshes with multiple
    /// instances, animation or skinning remain in local space and their mesh
    /// indices are returned in `local_space_meshes`.
    pub fn measurement_positions(&self, mesh: &Mesh) -> (Vec<DVec3>, Vec<usize>) {
        let nodes = self.json["nodes"].as_array().map_or(&[][..], Vec::as_slice);
        let meshes = self.json["meshes"]
            .as_array()
            .map_or(&[][..], Vec::as_slice);
        let mut parents: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
        let mut users: Vec<Vec<usize>> = vec![Vec::new(); meshes.len()];
        for (i, node) in nodes.iter().enumerate() {
            if let Some(m) = get_usize(node, "mesh").filter(|&m| m < meshes.len()) {
                users[m].push(i);
            }
            for child in node["children"].as_array().into_iter().flatten() {
                if let Some(c) = child
                    .as_u64()
                    .map(|c| c as usize)
                    .filter(|&c| c < nodes.len())
                {
                    parents[c].push(i);
                }
            }
        }
        let mut animated = vec![false; nodes.len()];
        for animation in self.json["animations"].as_array().into_iter().flatten() {
            for channel in animation["channels"].as_array().into_iter().flatten() {
                if let Some(n) = get_usize(&channel["target"], "node").filter(|&n| n < nodes.len())
                {
                    animated[n] = true;
                }
            }
        }
        let world = |start: usize| -> Option<DMat4> {
            let mut m = DMat4::IDENTITY;
            let mut current = start;
            let mut seen = vec![false; nodes.len()];
            loop {
                if seen[current] || animated[current] || nodes[current].get("skin").is_some() {
                    return None;
                }
                seen[current] = true;
                m = node_local(&nodes[current]) * m;
                match parents[current].as_slice() {
                    [] => break,
                    [p] => current = *p,
                    _ => return None,
                }
            }
            (m.is_finite() && m.determinant().abs() > 1e-15).then_some(m)
        };
        let transforms: Vec<Option<DMat4>> = users
            .iter()
            .enumerate()
            .map(|(i, u)| {
                if meshes[i].get("weights").is_some() {
                    return None;
                }
                match u.as_slice() {
                    [n] => world(*n),
                    _ => None,
                }
            })
            .collect();
        let local_space_meshes = transforms
            .iter()
            .enumerate()
            .filter_map(|(i, m)| m.is_none().then_some(i))
            .collect();
        let positions = mesh
            .positions()
            .iter()
            .enumerate()
            .map(|(v, &p)| {
                self.vertex_mesh
                    .get(v)
                    .and_then(|&m| transforms.get(m))
                    .and_then(|m| *m)
                    .map_or(p, |m| m.transform_point3(p))
            })
            .collect();
        (positions, local_space_meshes)
    }
}

impl GltfSource {
    /// glTF material names in index order; unnamed materials use `material<N>`.
    pub fn material_names(&self) -> Vec<String> {
        self.json["materials"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(i, material)| {
                material["name"]
                    .as_str()
                    .map_or_else(|| format!("material{i}"), str::to_string)
            })
            .collect()
    }

    /// Material index of each editable face; `None` means no assignment.
    pub fn face_material(&self, mesh: &Mesh) -> Vec<Option<u32>> {
        let meshes = self.json["meshes"]
            .as_array()
            .map_or(&[][..], Vec::as_slice);
        (0..mesh.face_count())
            .map(|face| {
                let corner = mesh.face_corners(face).next()?;
                let prim = &self.prims[*self.corner_prim.get(corner)? as usize];
                get_usize(&meshes[prim.mesh]["primitives"][prim.prim], "material")
                    .map(|index| index as u32)
            })
            .collect()
    }

    /// Normals, materials, names and transforms for converting to OBJ.
    ///
    /// A mesh gets a world transform only when exactly one node uses it, that
    /// node has no skin, its node chain is acyclic and the transform keeps
    /// orientation (positive determinant). Otherwise it stays in local space.
    pub(crate) fn conversion_data(&self) -> ConversionData {
        let json = &self.json;
        let meshes = json["meshes"].as_array().map_or(&[][..], Vec::as_slice);
        let nodes = json["nodes"].as_array().map_or(&[][..], Vec::as_slice);
        let mut parent = vec![None; nodes.len()];
        for (i, n) in nodes.iter().enumerate() {
            for c in n["children"].as_array().into_iter().flatten() {
                if let Some(c) = c.as_u64().map(|c| c as usize).filter(|&c| c < nodes.len()) {
                    parent[c] = Some(i);
                }
            }
        }
        let mut users: Vec<Vec<usize>> = vec![Vec::new(); meshes.len()];
        for (i, n) in nodes.iter().enumerate() {
            if let Some(m) = get_usize(n, "mesh").filter(|&m| m < meshes.len()) {
                users[m].push(i);
            }
        }
        let world = |node: usize| -> Option<DMat4> {
            if nodes[node].get("skin").is_some() {
                return None;
            }
            let mut m = DMat4::IDENTITY;
            let mut cur = Some(node);
            let mut steps = 0;
            while let Some(n) = cur {
                steps += 1;
                if steps > nodes.len() {
                    return None; // cycle
                }
                m = node_local(&nodes[n]) * m;
                cur = parent[n];
            }
            (m.determinant() > 0.0 && m.is_finite()).then_some(m)
        };
        let mesh_transform = users
            .iter()
            .map(|u| match u.as_slice() {
                [n] => world(*n),
                _ => None,
            })
            .collect();

        let prim_material: Vec<Option<u32>> = self
            .prims
            .iter()
            .map(|p| get_usize(&meshes[p.mesh]["primitives"][p.prim], "material").map(|m| m as u32))
            .collect();
        let prim_normals: Vec<Option<Accessor>> = self
            .prims
            .iter()
            .map(|p| {
                let a = get_usize(
                    &meshes[p.mesh]["primitives"][p.prim]["attributes"],
                    "NORMAL",
                )?;
                parse_accessor(json, &self.buffers, a)
                    .ok()
                    .filter(|a| a.components == 3 && a.count >= p.vertex_count)
            })
            .collect();
        let corner_normal = self
            .corner_prim
            .iter()
            .zip(&self.corner_element)
            .map(|(&p, &e)| {
                let a = prim_normals[p as usize]?;
                let d = &self.buffers[a.buffer];
                let e = e as usize;
                Some(DVec3::new(
                    a.read(d, e, 0),
                    a.read(d, e, 1),
                    a.read(d, e, 2),
                ))
            })
            .collect();
        let name = |v: &Value, prefix: &str, i: usize| {
            v["name"]
                .as_str()
                .map_or_else(|| format!("{prefix}{i}"), str::to_string)
        };
        ConversionData {
            corner_mesh: self
                .corner_prim
                .iter()
                .map(|&p| self.prims[p as usize].mesh as u32)
                .collect(),
            corner_material: self
                .corner_prim
                .iter()
                .map(|&p| prim_material[p as usize])
                .collect(),
            corner_normal,
            mesh_names: meshes
                .iter()
                .enumerate()
                .map(|(i, m)| name(m, "mesh", i))
                .collect(),
            material_names: json["materials"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(i, m)| name(m, "material", i))
                .collect(),
            mesh_transform,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_object_json_root_is_an_error_not_a_panic() {
        for text in ["1", "[]", "null", "\"asset\""] {
            assert!(
                parse(text.as_bytes().to_vec(), Path::new(".")).is_err(),
                "{text}"
            );
        }
    }

    #[test]
    fn external_buffers_must_be_relative_regular_files() {
        let dir = std::env::temp_dir().join(format!("uvcraft-gltf-uri-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/data file.bin"), [0u8; 4]).unwrap();
        assert_eq!(
            external_buffer_path(&dir, "sub/data%20file.bin").unwrap(),
            dir.join("sub/data file.bin")
        );
        let absolute = dir.join("sub/data file.bin").to_string_lossy().into_owned();
        for uri in [
            absolute.as_str(),
            "/etc/hostname",
            "file:///etc/hostname",
            "https://example.com/a.bin",
            "sub",
            "missing.bin",
            "a%00b.bin",
            "../sibling.bin",
            "sub/../sub/data%20file.bin",
            "sub/%2E%2E/%2E%2E/x.bin",
        ] {
            assert!(external_buffer_path(&dir, uri).is_err(), "{uri}");
        }
        #[cfg(unix)]
        {
            assert!(external_buffer_path(&dir, "../../../../../../../../dev/zero").is_err());
            let outside =
                std::env::temp_dir().join(format!("uvcraft-gltf-out-{}.bin", std::process::id()));
            std::fs::write(&outside, [0u8; 4]).unwrap();
            std::os::unix::fs::symlink(&outside, dir.join("link.bin")).unwrap();
            assert!(external_buffer_path(&dir, "link.bin").is_err());
            let _ = std::fs::remove_file(outside);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

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

    fn meshopt_glb() -> Vec<u8> {
        let (mut json, bin) = quad(true, false);
        let positions: Vec<[u8; 12]> = bin[..72].as_chunks::<12>().0.to_vec();
        let texcoords: Vec<[u8; 8]> = bin[72..120].as_chunks::<8>().0.to_vec();
        let indices: [u32; 6] = [0, 1, 2, 5, 1, 3];
        let packed = [
            (
                awsm_renderer_codec_meshopt::meshopt::encoding::encode_vertex_buffer(&positions)
                    .unwrap(),
                12,
                6,
                "ATTRIBUTES",
            ),
            (
                awsm_renderer_codec_meshopt::meshopt::encoding::encode_vertex_buffer(&texcoords)
                    .unwrap(),
                8,
                6,
                "ATTRIBUTES",
            ),
            (
                awsm_renderer_codec_meshopt::meshopt::encoding::encode_index_buffer(&indices, 6)
                    .unwrap(),
                2,
                6,
                "TRIANGLES",
            ),
        ];
        assert_eq!(
            awsm_renderer_codec_meshopt::decode_buffer_view(
                &packed[0].0,
                6,
                12,
                awsm_renderer_codec_meshopt::Mode::Attributes,
                awsm_renderer_codec_meshopt::Filter::None
            )
            .unwrap(),
            bin[..72]
        );
        assert_eq!(
            awsm_renderer_codec_meshopt::decode_buffer_view(
                &packed[1].0,
                6,
                8,
                awsm_renderer_codec_meshopt::Mode::Attributes,
                awsm_renderer_codec_meshopt::Filter::None
            )
            .unwrap(),
            bin[72..120]
        );
        let mut compressed = Vec::new();
        for (view, (bytes, stride, count, mode)) in json["bufferViews"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .zip(packed)
        {
            let offset = compressed.len();
            compressed.extend(bytes);
            view["buffer"] = json!(1);
            view["extensions"] = json!({"EXT_meshopt_compression": {
                "buffer": 0, "byteOffset": offset, "byteLength": compressed.len() - offset,
                "byteStride": stride, "count": count, "mode": mode
            }});
        }
        json["buffers"] = json!([{"byteLength": compressed.len()}, {"byteLength": bin.len()}]);
        json["extensionsRequired"] = json!(["EXT_meshopt_compression"]);
        json["extensionsUsed"] = json!(["EXT_meshopt_compression"]);
        build_glb(&serde_json::to_vec(&json).unwrap(), Some(&compressed), &[])
    }

    #[test]
    fn decodes_khr_meshopt_color_filter_at_eight_and_sixteen_bits() {
        let mut rgba8 = [100, 20, (-10_i8) as u8, 0xbf];
        decode_meshopt_color(&mut rgba8, 4).unwrap();
        assert_eq!(rgba8, [130, 90, 90, 127]);

        let mut rgba16 = Vec::new();
        rgba16.extend_from_slice(&20000_u16.to_le_bytes());
        rgba16.extend_from_slice(&1000_i16.to_le_bytes());
        rgba16.extend_from_slice(&(-2000_i16).to_le_bytes());
        rgba16.extend_from_slice(&0x9234_u16.to_le_bytes());
        decode_meshopt_color(&mut rgba16, 8).unwrap();
        let decoded: Vec<_> = rgba16
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| u16::from_le_bytes(*bytes))
            .collect();
        assert_eq!(decoded, [23000, 18000, 21000, 9320]);

        let encoded = awsm_renderer_codec_meshopt::meshopt::encoding::encode_vertex_buffer(&[[
            100_u8,
            20,
            (-10_i8) as u8,
            0xbf,
        ]])
        .unwrap();
        let mut json = json!({
            "bufferViews": [{
                "buffer": 1, "byteLength": 4,
                "extensions": {"KHR_meshopt_compression": {
                    "buffer": 0, "byteLength": encoded.len(), "byteStride": 4,
                    "count": 1, "mode": "ATTRIBUTES", "filter": "COLOR"
                }}
            }],
            "extensionsUsed": ["KHR_meshopt_compression"],
            "extensionsRequired": ["KHR_meshopt_compression"]
        });
        let mut buffers = vec![encoded, vec![0; 4]];
        assert_eq!(
            decode_meshopt_views(&mut json, &mut buffers).unwrap(),
            BTreeSet::from([1])
        );
        assert_eq!(buffers[1], [130, 90, 90, 127]);
        assert!(json["extensionsRequired"].as_array().unwrap().is_empty());
    }

    #[test]
    fn decodes_required_meshopt_views_and_preserves_uv_edit() {
        let dir = TempDir::new("meshopt");
        let baseline = parse(glb(true, false), &dir.0).unwrap();
        let mut compressed = parse(meshopt_glb(), &dir.0).unwrap();
        assert_eq!(compressed.mesh.positions(), baseline.mesh.positions());
        assert_eq!(compressed.mesh.face_count(), baseline.mesh.face_count());
        for face in 0..baseline.mesh.face_count() {
            let corners = |doc: &GltfDocument| {
                let mut values: Vec<_> = doc
                    .mesh
                    .face_corners(face)
                    .map(|corner| {
                        (
                            doc.mesh
                                .corner_position(corner)
                                .to_array()
                                .map(f64::to_bits),
                            doc.uv.uv_at_corner(corner).to_array().map(f64::to_bits),
                        )
                    })
                    .collect();
                values.sort_unstable();
                values
            };
            assert_eq!(corners(&compressed), corners(&baseline));
        }
        compressed.uv.uvs[0].x = 0.25;
        let saved = compressed
            .source
            .export(&compressed.mesh, &compressed.uv, &dir.0.join("out.glb"))
            .unwrap();
        let again = parse(saved[0].1.clone(), &dir.0).unwrap();
        assert_eq!(again.uv.uvs[0].x, 0.25);
        assert_eq!(again.mesh.positions(), baseline.mesh.positions());
    }

    #[test]
    fn imports_official_draco_boom_box_and_reexports_edited_uvs() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/draco");
        let bytes = std::fs::read(fixture.join("BoomBox.gltf")).unwrap();
        let mut no_index: Value = serde_json::from_slice(&bytes).unwrap();
        no_index["meshes"][0]["primitives"][0]
            .as_object_mut()
            .unwrap()
            .remove("indices");
        let without_index = parse(serde_json::to_vec(&no_index).unwrap(), &fixture).unwrap();
        assert_eq!(without_index.mesh.face_count(), 6036);
        let mut doc = parse(bytes, &fixture).unwrap();
        assert_eq!(doc.mesh.face_count(), 6036);
        assert_eq!(doc.missing_uv_corners, 0);
        let before = doc.mesh.positions().to_vec();
        let original = doc.uv.uvs[0];
        doc.uv.uvs[0].x += 0.125;
        let dir = TempDir::new("draco-export");
        let files = doc
            .source
            .export(&doc.mesh, &doc.uv, &dir.0.join("edited.glb"))
            .unwrap();
        let again = parse(files[0].1.clone(), &dir.0).unwrap();
        assert_eq!(again.mesh.face_count(), 6036);
        assert_eq!(again.mesh.positions(), before);
        assert!((again.uv.uvs[0].x - (original.x + 0.125)).abs() < 1e-6);
    }

    #[test]
    fn imports_official_required_meshopt_cube_and_reexports_edited_uvs() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/meshopt");
        let mut doc = read(&fixture.join("MeshoptCubeTest.gltf")).unwrap();
        assert!(doc.mesh.face_count() > 0);
        assert!(doc.missing_uv_corners > 0);
        // Compare against the uncompressed fallback from the optional variant
        // of the same Khronos asset, so decoder mistakes cannot self-validate.
        let mut fallback: Value = serde_json::from_slice(
            &std::fs::read(fixture.join("MeshoptCubeTestOptional.gltf")).unwrap(),
        )
        .unwrap();
        for view in fallback["bufferViews"].as_array_mut().unwrap() {
            if let Some(extensions) = view["extensions"].as_object_mut() {
                extensions.remove("KHR_meshopt_compression");
            }
        }
        fallback["extensionsUsed"]
            .as_array_mut()
            .unwrap()
            .retain(|name| name.as_str() != Some("KHR_meshopt_compression"));
        let plain = parse(serde_json::to_vec(&fallback).unwrap(), &fixture).unwrap();
        assert_eq!(doc.mesh.positions(), plain.mesh.positions());
        assert_eq!(doc.mesh.corner_count(), plain.mesh.corner_count());
        let faces = |mesh: &Mesh| {
            let mut triangles: Vec<_> = (0..mesh.face_count())
                .map(|face| {
                    let mut vertices: Vec<_> = mesh
                        .face_corners(face)
                        .map(|corner| mesh.corner_position(corner).to_array().map(f64::to_bits))
                        .collect();
                    vertices.sort_unstable();
                    vertices
                })
                .collect();
            triangles.sort_unstable();
            triangles
        };
        assert_eq!(faces(&doc.mesh), faces(&plain.mesh));
        let positions = doc.mesh.positions().to_vec();
        let original = doc.uv.uvs[0];
        doc.uv.uvs[0].x += 0.125;
        let dir = TempDir::new("meshopt-cube-export");
        let files = doc
            .source
            .export(&doc.mesh, &doc.uv, &dir.0.join("edited.glb"))
            .unwrap();
        let again = parse(files[0].1.clone(), &dir.0).unwrap();
        assert_eq!(again.mesh.positions(), positions);
        assert_eq!(again.mesh.face_count(), doc.mesh.face_count());
        assert!((again.uv.uvs[0].x - (original.x + 0.125)).abs() < 1e-6);
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
    fn converts_between_gltf_and_glb() {
        // .gltf with two external buffers (positions | UVs + indices) and an image.
        let src = TempDir::new("conv-src");
        let (mut json, bin) = quad(true, false);
        let split = 6 * 12;
        json["buffers"] = json!([
            { "uri": "a.bin", "byteLength": split },
            { "uri": "b.bin", "byteLength": bin.len() - split }
        ]);
        for v in 1..3 {
            let off = json["bufferViews"][v]["byteOffset"].as_u64().unwrap() as usize;
            json["bufferViews"][v]["buffer"] = json!(1);
            json["bufferViews"][v]["byteOffset"] = json!(off - split);
        }
        json["images"] = json!([{ "uri": "tex.png" }]);
        std::fs::write(src.0.join("a.bin"), &bin[..split]).unwrap();
        std::fs::write(src.0.join("b.bin"), &bin[split..]).unwrap();
        std::fs::write(src.0.join("tex.png"), b"png").unwrap();
        let path = src.0.join("quad.gltf");
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        let mut doc = read(&path).unwrap();
        doc.uv.uvs[1].x = 0.75;

        let out = TempDir::new("conv-out");
        let glb_path = out.0.join("quad.glb");
        write(&glb_path, &doc.source, &doc.mesh, &doc.uv).unwrap();
        let as_glb = read(&glb_path).unwrap();
        assert_eq!(as_glb.source.container(), Container::Glb);
        assert_eq!(as_glb.uv, doc.uv);
        assert_eq!(as_glb.mesh.positions(), doc.mesh.positions());
        assert_eq!(as_glb.source.json["buffers"].as_array().unwrap().len(), 1);
        assert!(
            as_glb.source.json["images"][0]["uri"]
                .as_str()
                .unwrap()
                .ends_with("tex.png")
        );
        assert_eq!(
            std::fs::read_dir(&out.0).unwrap().count(),
            1,
            "one self-contained file"
        );

        let back = TempDir::new("conv-back");
        let gltf_path = back.0.join("again.gltf");
        write(&gltf_path, &as_glb.source, &as_glb.mesh, &as_glb.uv).unwrap();
        assert!(back.0.join("again.bin").exists());
        let as_gltf = read(&gltf_path).unwrap();
        assert_eq!(as_gltf.source.container(), Container::Gltf);
        assert_eq!(as_gltf.uv, doc.uv);
        assert_eq!(as_gltf.mesh.positions(), doc.mesh.positions());
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
    fn rejects_invalid_draco_view() {
        let dir = TempDir::new("reject");
        let mut json = quad(true, false).0;
        json["extensionsRequired"] = json!(["KHR_draco_mesh_compression"]);
        json["meshes"][0]["primitives"][0]["extensions"] = json!({
            "KHR_draco_mesh_compression": {"bufferView": 999, "attributes": {"POSITION": 0}}
        });
        let bytes = build_glb(
            &serde_json::to_vec(&json).unwrap(),
            Some(&quad(true, false).1),
            &[],
        );
        assert!(matches!(parse(bytes, &dir.0), Err(GltfError::Invalid(_))));
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

    #[test]
    fn sparse_uv_accessor_round_trips_and_can_be_edited() {
        let dir = TempDir::new("sparse-uv");
        let (mut json, mut bin) = quad(true, false);
        let idx_offset = bin.len();
        bin.push(1); // glTF vertex 1
        bin.resize(bin.len().next_multiple_of(4), 0);
        let value_offset = bin.len();
        bin.extend_from_slice(&0.25f32.to_le_bytes());
        bin.extend_from_slice(&0.75f32.to_le_bytes());
        json["bufferViews"].as_array_mut().unwrap().extend([
            json!({"buffer":0,"byteOffset":idx_offset,"byteLength":1}),
            json!({"buffer":0,"byteOffset":value_offset,"byteLength":8}),
        ]);
        json["accessors"][1]["sparse"] = json!({
            "count":1,
            "indices":{"bufferView":3,"componentType":UNSIGNED_BYTE},
            "values":{"bufferView":4}
        });
        json["buffers"][0]["byteLength"] = json!(bin.len());
        let bytes = build_glb(&serde_json::to_vec(&json).unwrap(), Some(&bin), &[]);
        let mut doc = parse(bytes.clone(), &dir.0).unwrap();
        assert_eq!(doc.uv.uv_at_corner(1), DVec2::new(0.25, 0.25));
        let out = dir.0.join("sparse.glb");
        assert_eq!(
            doc.source.export(&doc.mesh, &doc.uv, &out).unwrap()[0].1,
            bytes
        );
        let corner_uv = doc.uv.corner_uv[1] as usize;
        doc.uv.uvs[corner_uv] = DVec2::new(0.5, 0.5);
        let files = doc.source.export(&doc.mesh, &doc.uv, &out).unwrap();
        let again = parse(files[0].1.clone(), &dir.0).unwrap();
        assert_eq!(again.uv.uv_at_corner(1), DVec2::new(0.5, 0.5));
    }

    #[test]
    fn measurement_positions_use_only_unique_static_nodes() {
        let dir = TempDir::new("measurement-nodes");
        let (mut json, bin) = quad(true, false);
        json["nodes"] = json!([{ "mesh":0, "translation":[2,3,4], "scale":[2,2,2] }]);
        let bytes = build_glb(&serde_json::to_vec(&json).unwrap(), Some(&bin), &[]);
        let doc = parse(bytes, &dir.0).unwrap();
        let (positions, fallback) = doc.source.measurement_positions(&doc.mesh);
        assert!(fallback.is_empty());
        assert_eq!(positions[0], DVec3::new(2.0, 3.0, 4.0));
        assert_eq!(positions[1], DVec3::new(4.0, 3.0, 4.0));
        assert_eq!(doc.mesh.positions()[1], DVec3::new(1.0, 0.0, 0.0));

        json["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"mesh":0}));
        let doc = parse(
            build_glb(&serde_json::to_vec(&json).unwrap(), Some(&bin), &[]),
            &dir.0,
        )
        .unwrap();
        let (positions, fallback) = doc.source.measurement_positions(&doc.mesh);
        assert_eq!(fallback, vec![0]);
        assert_eq!(positions, doc.mesh.positions());

        json["nodes"].as_array_mut().unwrap().pop();
        json["nodes"][0]["skin"] = json!(0);
        let doc = parse(
            build_glb(&serde_json::to_vec(&json).unwrap(), Some(&bin), &[]),
            &dir.0,
        )
        .unwrap();
        assert_eq!(doc.source.measurement_positions(&doc.mesh).1, vec![0]);

        json["nodes"][0].as_object_mut().unwrap().remove("skin");
        json["animations"] = json!([{"channels":[{"target":{"node":0,"path":"translation"}}]}]);
        let doc = parse(
            build_glb(&serde_json::to_vec(&json).unwrap(), Some(&bin), &[]),
            &dir.0,
        )
        .unwrap();
        assert_eq!(doc.source.measurement_positions(&doc.mesh).1, vec![0]);
    }

    #[test]
    fn measurement_positions_compose_parent_transform_and_reject_animated_ancestor() {
        let dir = TempDir::new("measurement-hierarchy");
        let (mut json, bin) = quad(true, false);
        json["nodes"] = json!([
            {"children":[1], "translation":[5,0,0]},
            {"mesh":0, "translation":[1,0,0], "scale":[-2,2,2]}
        ]);
        let load = |json: &Value| {
            parse(
                build_glb(&serde_json::to_vec(json).unwrap(), Some(&bin), &[]),
                &dir.0,
            )
            .unwrap()
        };
        let doc = load(&json);
        let (positions, fallback) = doc.source.measurement_positions(&doc.mesh);
        assert!(fallback.is_empty());
        assert_eq!(positions[0], DVec3::new(6.0, 0.0, 0.0));
        assert_eq!(positions[1], DVec3::new(4.0, 0.0, 0.0));

        json["animations"] = json!([{"channels":[{"target":{"node":0,"path":"translation"}}]}]);
        let doc = load(&json);
        let (positions, fallback) = doc.source.measurement_positions(&doc.mesh);
        assert_eq!(fallback, vec![0]);
        assert_eq!(positions, doc.mesh.positions());
    }

    #[test]
    fn second_uv_channel_can_be_selected_and_saved_independently() {
        let dir = TempDir::new("uv-channel-1");
        let (mut json, mut bin) = quad(true, false);
        let offset = bin.len();
        for _ in 0..6 {
            bin.extend_from_slice(&0.25f32.to_le_bytes());
            bin.extend_from_slice(&0.75f32.to_le_bytes());
        }
        json["bufferViews"]
            .as_array_mut()
            .unwrap()
            .push(json!({"buffer":0,"byteOffset":offset,"byteLength":48}));
        json["accessors"]
            .as_array_mut()
            .unwrap()
            .push(json!({"bufferView":3,"componentType":FLOAT,"count":6,"type":"VEC2"}));
        json["meshes"][0]["primitives"][0]["attributes"]["TEXCOORD_1"] = json!(3);
        json["buffers"][0]["byteLength"] = json!(bin.len());
        let original = build_glb(&serde_json::to_vec(&json).unwrap(), Some(&bin), &[]);
        let doc = parse(original, &dir.0).unwrap();
        assert_eq!(doc.source.uv_channels(), vec![0, 1]);
        assert!(
            doc.source
                .activate_uv_channel(&doc.mesh, 2)
                .unwrap()
                .is_none()
        );
        let (source, mut uv) = doc
            .source
            .activate_uv_channel(&doc.mesh, 1)
            .unwrap()
            .unwrap();
        assert_eq!(uv.name, "TEXCOORD_1");
        assert_eq!(uv.uv_at_corner(0), DVec2::new(0.25, 0.25));
        uv.uvs[0] = DVec2::new(1.25, -0.5);
        let files = source
            .export(&doc.mesh, &uv, &dir.0.join("out.glb"))
            .unwrap();
        let again = parse(files[0].1.clone(), &dir.0).unwrap();
        assert_eq!(again.uv, doc.uv, "TEXCOORD_0 stays unchanged");
        let (_, uv1) = again
            .source
            .activate_uv_channel(&again.mesh, 1)
            .unwrap()
            .unwrap();
        assert_eq!(uv1.uv_at_corner(0), DVec2::new(1.25, -0.5));

        // Splitting a vertex in channel 1 duplicates channel 0 values too.
        let (_, mut split_uv) = doc
            .source
            .activate_uv_channel(&doc.mesh, 1)
            .unwrap()
            .unwrap();
        split_uv.uvs.push(DVec2::new(0.75, 0.25));
        split_uv.corner_uv[1] = split_uv.uvs.len() as u32 - 1;
        let files = source
            .export(&doc.mesh, &split_uv, &dir.0.join("split.glb"))
            .unwrap();
        let split = parse(files[0].1.clone(), &dir.0).unwrap();
        for c in 0..doc.mesh.corner_count() {
            assert_eq!(split.uv.uv_at_corner(c), doc.uv.uv_at_corner(c));
        }
        let (_, split_channel) = split
            .source
            .activate_uv_channel(&split.mesh, 1)
            .unwrap()
            .unwrap();
        assert_eq!(split_channel.uv_at_corner(1), DVec2::new(0.75, 0.25));

        let mut channel_zero = doc.uv.clone();
        let z = channel_zero.corner_uv[0] as usize;
        channel_zero.uvs[z] = DVec2::new(0.125, 0.125);
        let mut layers = BTreeMap::new();
        layers.insert(0, channel_zero.clone());
        layers.insert(1, split_uv.clone());
        let files = doc
            .source
            .export_uv_channels(&doc.mesh, &layers, 0, &dir.0.join("secondary-split.glb"))
            .unwrap();
        let saved = parse(files[0].1.clone(), &dir.0).unwrap();
        for c in 0..doc.mesh.corner_count() {
            assert_eq!(saved.uv.uv_at_corner(c), channel_zero.uv_at_corner(c));
        }
        let (_, saved_one) = saved
            .source
            .activate_uv_channel(&saved.mesh, 1)
            .unwrap()
            .unwrap();
        for c in 0..doc.mesh.corner_count() {
            assert_eq!(saved_one.uv_at_corner(c), split_uv.uv_at_corner(c));
        }
    }

    #[test]
    fn distinct_seams_in_two_channels_preserve_gltf_data() {
        let dir = TempDir::new("two-channel-seams");
        let (mut json, mut bin) = quad(true, false);
        let offset = bin.len();
        for _ in 0..6 {
            bin.extend_from_slice(&0.25f32.to_le_bytes());
            bin.extend_from_slice(&0.75f32.to_le_bytes());
        }
        json["bufferViews"]
            .as_array_mut()
            .unwrap()
            .push(json!({"buffer":0,"byteOffset":offset,"byteLength":48}));
        json["accessors"]
            .as_array_mut()
            .unwrap()
            .push(json!({"bufferView":3,"componentType":FLOAT,"count":6,"type":"VEC2"}));
        let attrs = &mut json["meshes"][0]["primitives"][0]["attributes"];
        attrs["TEXCOORD_1"] = json!(3);
        attrs["NORMAL"] = json!(0);
        json["meshes"][0]["primitives"][0]["material"] = json!(0);
        json["meshes"][0]["primitives"][0]["targets"] = json!([{"POSITION":0}]);
        json["materials"] = json!([{"name":"keep me","doubleSided":true}]);
        json["images"] = json!([{"uri":"data:image/png;base64,AA=="}]);
        json["buffers"][0]["byteLength"] = json!(bin.len());
        let extra = vec![(0x1234_5678, b"keep".to_vec())];
        let doc = parse(
            build_glb(&serde_json::to_vec(&json).unwrap(), Some(&bin), &extra),
            &dir.0,
        )
        .unwrap();
        let mut zero = doc.uv.clone();
        zero.uvs.push(DVec2::new(0.625, 0.625));
        zero.corner_uv[4] = zero.uvs.len() as u32 - 1;
        let (_, mut one) = doc
            .source
            .activate_uv_channel(&doc.mesh, 1)
            .unwrap()
            .unwrap();
        one.uvs.push(DVec2::new(0.75, 0.25));
        one.corner_uv[1] = one.uvs.len() as u32 - 1;
        let layers = BTreeMap::from([(0, zero.clone()), (1, one.clone())]);
        let files = doc
            .source
            .export_uv_channels(&doc.mesh, &layers, 0, &dir.0.join("out.glb"))
            .unwrap();
        let saved = parse(files[0].1.clone(), &dir.0).unwrap();
        assert_eq!(saved.mesh.positions(), doc.mesh.positions());
        assert_eq!(saved.mesh.corner_count(), doc.mesh.corner_count());
        assert_eq!(saved.source.json["materials"], json["materials"]);
        assert_eq!(saved.source.json["images"], json["images"]);
        assert_eq!(saved.source.extra_chunks, extra);
        let prim = &saved.source.json["meshes"][0]["primitives"][0];
        assert_eq!(prim["material"], json!(0));
        let pos = prim["attributes"]["POSITION"].as_u64().unwrap() as usize;
        let norm = prim["attributes"]["NORMAL"].as_u64().unwrap() as usize;
        let morph = prim["targets"][0]["POSITION"].as_u64().unwrap() as usize;
        assert_eq!(
            saved.source.json["accessors"][pos]["count"],
            saved.source.json["accessors"][norm]["count"]
        );
        assert_eq!(
            saved.source.json["accessors"][pos]["count"],
            saved.source.json["accessors"][morph]["count"]
        );
        for c in 0..doc.mesh.corner_count() {
            assert_eq!(saved.uv.uv_at_corner(c), zero.uv_at_corner(c));
        }
        let (_, saved_one) = saved
            .source
            .activate_uv_channel(&saved.mesh, 1)
            .unwrap()
            .unwrap();
        for c in 0..doc.mesh.corner_count() {
            assert_eq!(saved_one.uv_at_corner(c), one.uv_at_corner(c));
        }
        let gltf_files = doc
            .source
            .export_uv_channels(&doc.mesh, &layers, 0, &dir.0.join("out.gltf"))
            .unwrap();
        for (path, bytes) in gltf_files {
            std::fs::write(path, bytes).unwrap();
        }
        let gltf_saved = read(&dir.0.join("out.gltf")).unwrap();
        assert_eq!(gltf_saved.source.json["materials"], json["materials"]);
        assert_eq!(gltf_saved.source.json["images"], json["images"]);
        for c in 0..doc.mesh.corner_count() {
            assert_eq!(gltf_saved.uv.uv_at_corner(c), zero.uv_at_corner(c));
        }
        let (_, gltf_one) = gltf_saved
            .source
            .activate_uv_channel(&gltf_saved.mesh, 1)
            .unwrap()
            .unwrap();
        for c in 0..doc.mesh.corner_count() {
            assert_eq!(gltf_one.uv_at_corner(c), one.uv_at_corner(c));
        }
    }

    #[test]
    fn normalized_uv_outside_unit_is_promoted_to_float() {
        let dir = TempDir::new("normalized-outside");
        let (mut json, mut bin) = quad(true, false);
        let offset = bin.len().next_multiple_of(4);
        bin.resize(offset, 0);
        for uv in [
            [0u16, 65535],
            [65535, 65535],
            [0, 0],
            [65535, 0],
            [65535, 65535],
            [0, 0],
        ] {
            for c in uv {
                bin.extend_from_slice(&c.to_le_bytes());
            }
        }
        json["bufferViews"]
            .as_array_mut()
            .unwrap()
            .push(json!({"buffer":0,"byteOffset":offset,"byteLength":24}));
        json["accessors"][1]["bufferView"] = json!(3);
        json["accessors"][1]["componentType"] = json!(UNSIGNED_SHORT);
        json["accessors"][1]["normalized"] = json!(true);
        json["buffers"][0]["byteLength"] = json!(bin.len());
        let mut doc = parse(
            build_glb(&serde_json::to_vec(&json).unwrap(), Some(&bin), &[]),
            &dir.0,
        )
        .unwrap();
        let u = doc.uv.corner_uv[0] as usize;
        doc.uv.uvs[u] = DVec2::new(-0.5, 2.0);
        let files = doc
            .source
            .export(&doc.mesh, &doc.uv, &dir.0.join("out.glb"))
            .unwrap();
        let again = parse(files[0].1.clone(), &dir.0).unwrap();
        assert_eq!(again.uv.uv_at_corner(0), DVec2::new(-0.5, 2.0));
        let tex = again.source.json["meshes"][0]["primitives"][0]["attributes"]["TEXCOORD_0"]
            .as_u64()
            .unwrap() as usize;
        assert_eq!(
            again.source.json["accessors"][tex]["componentType"],
            json!(FLOAT)
        );
    }
}
