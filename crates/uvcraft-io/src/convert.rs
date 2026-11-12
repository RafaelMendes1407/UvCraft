//! Conversion between OBJ and glTF.
//!
//! OBJ → glTF writes a fresh file (`.gltf` + `<stem>.bin`, or a self-contained
//! `.glb`): one glTF mesh per run of consecutive faces of the same OBJ object
//! (`o`), split into one triangle-list primitive per run of the same material
//! (`usemtl`), so face order survives. Within a primitive, one glTF vertex is
//! emitted per unique (mesh vertex, UV vertex, normal) tuple. Polygons are
//! fan-triangulated, so an n-gon becomes n − 2 faces after reloading. Corners
//! without `vn` in a primitive that has normals get their face normal. Values
//! are stored as f32 and V is flipped (`v_gltf = 1 - v`). Materials only carry
//! their name. On reload the glTF reader welds by value, so f64 positions
//! that round to the same f32 merge, and a seam whose two sides have
//! identical UVs is lost (glTF cannot express it).
//!
//! glTF → OBJ writes `v` (one per mesh vertex, in mesh order), `vt` (the UV
//! layer), `vn` (unique NORMAL values) and one `f` per face with the exact face
//! and corner order of the [`Mesh`], plus `o` per glTF mesh and `usemtl` per
//! material. A mesh used by exactly one non-skinned node whose world transform
//! keeps orientation is written in world space; any other mesh (instanced,
//! skinned, mirrored or not in a node) is written in its local space. Point and
//! line primitives are dropped, and no `.mtl` file is written.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use glam::{DMat3, DVec3};
use serde_json::{Value, json};
use uvcraft_core::{Mesh, UvLayer};

use crate::gltf::{self, GltfError, GltfSource};
use crate::obj::{ObjError, ObjSource};

const FLOAT: u32 = 5126;
const UNSIGNED_INT: u32 = 5125;
const ARRAY_BUFFER: u32 = 34962;
const ELEMENT_ARRAY_BUFFER: u32 = 34963;
const NONE: u32 = u32::MAX;

// ---------------------------------------------------------------- OBJ → glTF

/// Writes an OBJ-sourced document as `.gltf` or `.glb` (picked by `glb`).
pub fn write_obj_as_gltf(
    path: &Path,
    source: &ObjSource,
    mesh: &Mesh,
    uv: &UvLayer,
    glb: bool,
) -> Result<(), GltfError> {
    for (file, bytes) in obj_to_gltf(source, mesh, uv, path, glb)? {
        std::fs::write(file, bytes)?;
    }
    Ok(())
}

/// Collects tightly packed bufferViews/accessors into one buffer.
#[derive(Default)]
struct Bin {
    data: Vec<u8>,
    views: Vec<Value>,
    accessors: Vec<Value>,
}

impl Bin {
    fn push_f32(&mut self, values: &[f32], ty: &str, n: usize, min_max: bool) -> usize {
        let offset = self.data.len();
        for v in values {
            self.data.extend_from_slice(&v.to_le_bytes());
        }
        let mut acc = json!({
            "bufferView": self.view(offset, ARRAY_BUFFER),
            "componentType": FLOAT,
            "count": values.len() / n,
            "type": ty,
        });
        if min_max {
            let mut min = vec![f32::INFINITY; n];
            let mut max = vec![f32::NEG_INFINITY; n];
            for chunk in values.chunks_exact(n) {
                for k in 0..n {
                    min[k] = min[k].min(chunk[k]);
                    max[k] = max[k].max(chunk[k]);
                }
            }
            acc["min"] = json!(min);
            acc["max"] = json!(max);
        }
        self.accessors.push(acc);
        self.accessors.len() - 1
    }

    fn push_indices(&mut self, values: &[u32]) -> usize {
        let offset = self.data.len();
        for v in values {
            self.data.extend_from_slice(&v.to_le_bytes());
        }
        let view = self.view(offset, ELEMENT_ARRAY_BUFFER);
        self.accessors.push(json!({
            "bufferView": view,
            "componentType": UNSIGNED_INT,
            "count": values.len(),
            "type": "SCALAR",
        }));
        self.accessors.len() - 1
    }

    fn view(&mut self, offset: usize, target: u32) -> usize {
        self.views.push(json!({
            "buffer": 0,
            "byteOffset": offset,
            "byteLength": self.data.len() - offset,
            "target": target,
        }));
        self.views.len() - 1
    }
}

/// glTF files for an OBJ-sourced document: the main file at `out_path`
/// (last) plus `<stem>.bin` for a `.gltf`.
pub fn obj_to_gltf(
    source: &ObjSource,
    mesh: &Mesh,
    uv: &UvLayer,
    out_path: &Path,
    glb: bool,
) -> Result<Vec<(PathBuf, Vec<u8>)>, GltfError> {
    uv.validate(mesh)?;
    let extra = &source.extra;
    let consistent = extra.corner_normal.len() == mesh.corner_count()
        && extra.face_material.len() == mesh.face_count()
        && extra.face_object.len() == mesh.face_count();
    let object_of = |f: usize| if consistent { extra.face_object[f] } else { 0 };
    let material_of = |f: usize| {
        if consistent {
            extra.face_material[f]
        } else {
            NONE
        }
    };
    let normal_of = |c: usize| {
        let n = if consistent {
            extra.corner_normal[c]
        } else {
            NONE
        };
        extra.normals.get(n as usize).copied()
    };

    // Runs of consecutive faces: a new mesh per object change, a new primitive per material change.
    // (object, [(material, first face, end face)]).
    type Run = (u32, usize, usize);
    let mut meshes: Vec<(u32, Vec<Run>)> = Vec::new();
    for f in 0..mesh.face_count() {
        let (o, m) = (object_of(f), material_of(f));
        match meshes.last_mut() {
            Some((lo, prims)) if *lo == o => match prims.last_mut() {
                Some((lm, _, end)) if *lm == m => *end = f + 1,
                _ => prims.push((m, f, f + 1)),
            },
            _ => meshes.push((o, vec![(m, f, f + 1)])),
        }
    }

    let mut bin = Bin::default();
    let mut json_meshes = Vec::new();
    for (object, prims) in &meshes {
        let mut json_prims = Vec::new();
        for &(material, start, end) in prims {
            let has_normals =
                (start..end).any(|f| mesh.face_corners(f).any(|c| normal_of(c).is_some()));
            let mut vertex_of: HashMap<(u32, u32, [u64; 3]), u32> = HashMap::new();
            let (mut pos, mut nrm, mut tex, mut idx) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            let mut corner_index = |c: usize, f: usize| -> u32 {
                let n = if has_normals {
                    normal_of(c)
                        .unwrap_or_else(|| mesh.face_area_normal(f))
                        .normalize_or(DVec3::Z)
                } else {
                    DVec3::ZERO
                };
                let v = mesh.corner_vertex(c) as u32;
                let t = uv.corner_uv[c];
                *vertex_of
                    .entry((v, t, n.to_array().map(f64::to_bits)))
                    .or_insert_with(|| {
                        let p = mesh.position(v as usize);
                        pos.extend([p.x as f32, p.y as f32, p.z as f32]);
                        if has_normals {
                            nrm.extend([n.x as f32, n.y as f32, n.z as f32]);
                        }
                        let q = uv.uvs[t as usize];
                        tex.extend([q.x as f32, (1.0 - q.y) as f32]);
                        (pos.len() / 3 - 1) as u32
                    })
            };
            for f in start..end {
                for tri in mesh.fan_triangles(f) {
                    for c in tri {
                        idx.push(corner_index(c, f));
                    }
                }
            }
            let mut attributes = json!({ "POSITION": bin.push_f32(&pos, "VEC3", 3, true) });
            if has_normals {
                attributes["NORMAL"] = json!(bin.push_f32(&nrm, "VEC3", 3, false));
            }
            attributes["TEXCOORD_0"] = json!(bin.push_f32(&tex, "VEC2", 2, false));
            let mut p = json!({
                "attributes": attributes,
                "indices": bin.push_indices(&idx),
                "mode": 4,
            });
            if material != NONE {
                p["material"] = json!(material);
            }
            json_prims.push(p);
        }
        let name = extra
            .objects
            .get(*object as usize)
            .cloned()
            .unwrap_or_else(|| "default".into());
        json_meshes.push(json!({ "name": name, "primitives": json_prims }));
    }

    let nodes: Vec<Value> = json_meshes
        .iter()
        .enumerate()
        .map(|(i, m)| json!({ "name": m["name"], "mesh": i }))
        .collect();
    let mut root = json!({
        "asset": { "version": "2.0", "generator": "UvCraft" },
        "scene": 0,
        "scenes": [{ "nodes": (0..nodes.len()).collect::<Vec<_>>() }],
        "nodes": nodes,
        "meshes": json_meshes,
    });
    if !extra.materials.is_empty() {
        root["materials"] = json!(
            extra
                .materials
                .iter()
                .map(|m| json!({ "name": m }))
                .collect::<Vec<_>>()
        );
    }
    if !bin.data.is_empty() {
        root["buffers"] = json!([{ "byteLength": bin.data.len() }]);
        root["bufferViews"] = json!(bin.views);
        root["accessors"] = json!(bin.accessors);
    }

    if glb {
        let bin_chunk = (!bin.data.is_empty()).then_some(bin.data.as_slice());
        let bytes = gltf::build_glb(&serde_json::to_vec(&root)?, bin_chunk, &[]);
        return Ok(vec![(out_path.to_path_buf(), bytes)]);
    }
    let mut files = Vec::new();
    if !bin.data.is_empty() {
        let stem = out_path
            .file_stem()
            .map_or_else(|| "buffer".into(), |s| s.to_string_lossy().into_owned());
        let bin_name = format!("{stem}.bin");
        root["buffers"][0]["uri"] = json!(gltf::percent_encode(&bin_name));
        files.push((out_path.with_file_name(&bin_name), bin.data));
    }
    files.push((out_path.to_path_buf(), serde_json::to_vec_pretty(&root)?));
    Ok(files)
}

// ---------------------------------------------------------------- glTF → OBJ

/// Writes a glTF-sourced document as OBJ.
pub fn write_gltf_as_obj(
    path: &Path,
    source: &GltfSource,
    mesh: &Mesh,
    uv: &UvLayer,
) -> Result<(), ObjError> {
    std::fs::write(path, gltf_to_obj(source, mesh, uv)?)?;
    Ok(())
}

/// OBJ text for a glTF-sourced document.
pub fn gltf_to_obj(source: &GltfSource, mesh: &Mesh, uv: &UvLayer) -> Result<Vec<u8>, ObjError> {
    uv.validate(mesh)?;
    let data = source.conversion_data();
    let consistent = data.corner_mesh.len() == mesh.corner_count();
    let mesh_of_corner = |c: usize| {
        if consistent {
            data.corner_mesh[c]
        } else {
            NONE
        }
    };
    let transform = |m: u32| data.mesh_transform.get(m as usize).copied().flatten();

    let mut vertex_mesh = vec![NONE; mesh.vertex_count()];
    for c in 0..mesh.corner_count() {
        vertex_mesh[mesh.corner_vertex(c)] = mesh_of_corner(c);
    }

    let mut out = String::new();
    out.push_str("# Converted by UvCraft from glTF\n");
    for (v, p) in mesh.positions().iter().enumerate() {
        let p = transform(vertex_mesh[v]).map_or(*p, |m| m.transform_point3(*p));
        let _ = writeln!(out, "v {} {} {}", p.x, p.y, p.z);
    }
    for t in &uv.uvs {
        let _ = writeln!(out, "vt {} {}", t.x, t.y);
    }

    // Unique normals in corner order.
    let mut normal_of: HashMap<[u64; 3], u32> = HashMap::new();
    let mut corner_normal = vec![NONE; mesh.corner_count()];
    if consistent {
        for (c, n) in data.corner_normal.iter().enumerate() {
            let Some(mut n) = *n else { continue };
            if let Some(m) = transform(data.corner_mesh[c]) {
                let inv_t = DMat3::from_mat4(m).inverse().transpose();
                n = (inv_t * n).normalize_or(n);
            }
            let next = normal_of.len() as u32;
            corner_normal[c] = *normal_of
                .entry(n.to_array().map(f64::to_bits))
                .or_insert_with(|| {
                    let _ = writeln!(out, "vn {} {} {}", n.x, n.y, n.z);
                    next
                });
        }
    }

    let mut current_mesh = None;
    let mut current_material = None;
    for f in 0..mesh.face_count() {
        let corners = mesh.face_corners(f);
        let m = mesh_of_corner(corners.start);
        if current_mesh != Some(m) {
            current_mesh = Some(m);
            let name = data
                .mesh_names
                .get(m as usize)
                .map_or("default", String::as_str);
            let _ = writeln!(out, "o {name}");
        }
        let material = if consistent {
            data.corner_material[corners.start]
        } else {
            None
        };
        if material != current_material {
            let name = material
                .and_then(|i| data.material_names.get(i as usize))
                .map_or("default", String::as_str);
            let _ = writeln!(out, "usemtl {name}");
            current_material = material;
        }
        out.push('f');
        for c in corners {
            let v = mesh.corner_vertex(c) + 1;
            let t = uv.corner_uv[c] + 1;
            match corner_normal[c] {
                NONE => {
                    let _ = write!(out, " {v}/{t}");
                }
                n => {
                    let _ = write!(out, " {v}/{t}/{}", n + 1);
                }
            }
        }
        out.push('\n');
    }
    Ok(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Loaded, Source, read, write};
    use glam::DVec2;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> Self {
            let d =
                std::env::temp_dir().join(format!("uvcraft-convert-{}-{name}", std::process::id()));
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

    /// Two objects without shared vertices: a two-triangle quad with a UV seam
    /// (material red, then blue), and a lone triangle without normals.
    const TRIS: &str = "mtllib a.mtl\no Quad\nv 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\n\
        vt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\nvt 0.5 0.25\nvn 0 0 1\n\
        usemtl red\nf 1/1/1 2/2/1 3/3/1\nusemtl blue\nf 1/5/1 3/3/1 4/4/1\n\
        o Tri\nv 2 0 0\nv 3 0 0\nv 2 1 0\nvt 0.75 0.5\nf 5/6 6/2 7/3\n";

    const QUADS: &str = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 2 0 0\nv 2 1 0\n\
        vt 0 0\nvt 0.5 0\nvt 0.5 1\nvt 0 1\nvt 1 0\nvt 1 1\n\
        f 1/1 2/2 3/3 4/4\nf 2/2 5/5 6/6 3/3\n";

    fn load(dir: &TempDir, name: &str, text: &str) -> Loaded {
        let p = dir.0.join(name);
        std::fs::write(&p, text).unwrap();
        read(&p).unwrap()
    }

    fn assert_same_corners(a: &Loaded, b: &Loaded) {
        assert_eq!(a.mesh.face_count(), b.mesh.face_count());
        assert_eq!(a.mesh.corner_count(), b.mesh.corner_count());
        for c in 0..a.mesh.corner_count() {
            assert_eq!(
                a.mesh.corner_position(c),
                b.mesh.corner_position(c),
                "corner {c}"
            );
        }
        assert!(a.uv.same_mapping(&b.uv, &a.mesh));
        assert!(b.uv.same_mapping(&a.uv, &b.mesh));
    }

    #[test]
    fn obj_to_glb_round_trip() {
        let dir = TempDir::new("obj-glb");
        let mut doc = load(&dir, "in.obj", TRIS);
        // A changed UV must be what the glTF gets.
        doc.uv.uvs[4] = DVec2::new(0.25, 0.75);
        let out = dir.0.join("out.glb");
        write(&out, &doc.source, &doc.mesh, &doc.uv).unwrap();
        let back = read(&out).unwrap();
        assert_same_corners(&doc, &back);
        assert_eq!(back.objects, vec!["Quad", "Tri"]);

        let Source::Gltf(g) = &back.source else {
            panic!("expected glTF")
        };
        let data = g.conversion_data();
        assert_eq!(data.material_names, vec!["red", "blue"]);
        assert_eq!(
            // `usemtl` carries over the `o` line, as in OBJ.
            &data.corner_material[..7],
            &[
                Some(0),
                Some(0),
                Some(0),
                Some(1),
                Some(1),
                Some(1),
                Some(1)
            ]
        );
        assert_eq!(data.corner_normal[0], Some(DVec3::Z));
        assert_eq!(data.corner_normal[6], None);
        // One self-contained file next to the input.
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
    }

    #[test]
    fn obj_quads_to_gltf_are_fan_triangulated() {
        let dir = TempDir::new("obj-gltf");
        let doc = load(&dir, "quads.obj", QUADS);
        let out = dir.0.join("quads.gltf");
        write(&out, &doc.source, &doc.mesh, &doc.uv).unwrap();
        assert!(dir.0.join("quads.bin").exists());
        let back = read(&out).unwrap();
        assert_eq!(back.mesh.face_count(), 4);
        assert_eq!(back.missing_uv_corners, 0);
        let mut c = 0;
        for f in 0..doc.mesh.face_count() {
            for tri in doc.mesh.fan_triangles(f) {
                for k in tri {
                    assert_eq!(back.mesh.corner_position(c), doc.mesh.corner_position(k));
                    assert_eq!(back.uv.uv_at_corner(c), doc.uv.uv_at_corner(k));
                    c += 1;
                }
            }
        }
        let islands = |l: &Loaded| uvcraft_core::Islands::compute(&l.mesh, &l.uv).count();
        assert_eq!(islands(&back), islands(&doc));
    }

    /// glTF with one quad (4 vertices with normals), optionally translated by its node.
    fn quad_glb(translation: Option<[f64; 3]>) -> Vec<u8> {
        let pos: [f32; 12] = [0., 0., 0., 1., 0., 0., 1., 1., 0., 0., 1., 0.];
        let nrm: [f32; 12] = [0., 0., 1., 0., 0., 1., 0., 0., 1., 0., 0., 1.];
        let uv: [f32; 8] = [0., 1., 1., 1., 1., 0., 0., 0.];
        let idx: [u32; 6] = [0, 1, 2, 0, 2, 3];
        let mut bin = Vec::new();
        for x in pos.iter().chain(&nrm).chain(&uv) {
            bin.extend_from_slice(&x.to_le_bytes());
        }
        for i in idx {
            bin.extend_from_slice(&i.to_le_bytes());
        }
        let mut node = json!({ "mesh": 0 });
        if let Some(t) = translation {
            node["translation"] = json!(t);
        }
        let json = json!({
            "asset": { "version": "2.0" },
            "nodes": [node],
            "buffers": [{ "byteLength": bin.len() }],
            "bufferViews": [
                { "buffer": 0, "byteOffset": 0, "byteLength": 48 },
                { "buffer": 0, "byteOffset": 48, "byteLength": 48 },
                { "buffer": 0, "byteOffset": 96, "byteLength": 32 },
                { "buffer": 0, "byteOffset": 128, "byteLength": 24 }
            ],
            "accessors": [
                { "bufferView": 0, "componentType": FLOAT, "count": 4, "type": "VEC3", "min": [0,0,0], "max": [1,1,0] },
                { "bufferView": 1, "componentType": FLOAT, "count": 4, "type": "VEC3" },
                { "bufferView": 2, "componentType": FLOAT, "count": 4, "type": "VEC2" },
                { "bufferView": 3, "componentType": UNSIGNED_INT, "count": 6, "type": "SCALAR" }
            ],
            "materials": [{ "name": "skin" }],
            "meshes": [{ "name": "Card", "primitives": [{
                "attributes": { "POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2 },
                "indices": 3, "material": 0
            }] }]
        });
        gltf::build_glb(&serde_json::to_vec(&json).unwrap(), Some(&bin), &[])
    }

    #[test]
    fn glb_to_obj_round_trip() {
        let dir = TempDir::new("glb-obj");
        let p = dir.0.join("card.glb");
        std::fs::write(&p, quad_glb(None)).unwrap();
        let mut doc = read(&p).unwrap();
        doc.uv.uvs[0] = DVec2::new(0.125, 0.5);
        let out = dir.0.join("card.obj");
        write(&out, &doc.source, &doc.mesh, &doc.uv).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(
            text.contains("o Card\nusemtl skin\nf 1/1/1 2/2/1 3/3/1\nf 1/1/1 3/3/1 4/4/1\n"),
            "{text}"
        );
        assert_eq!(text.matches("\nvn ").count(), 1);
        let back = read(&out).unwrap();
        assert_same_corners(&doc, &back);
        assert_eq!(back.mesh.positions(), doc.mesh.positions());
        assert_eq!(back.uv.corner_uv, doc.uv.corner_uv);
        assert_eq!(back.uv.uvs, doc.uv.uvs);
        assert_eq!(back.objects, vec!["Card"]);
    }

    #[test]
    fn glb_to_obj_applies_simple_node_transforms() {
        let dir = TempDir::new("glb-obj-xform");
        let p = dir.0.join("moved.glb");
        std::fs::write(&p, quad_glb(Some([10.0, 0.0, -2.0]))).unwrap();
        let doc = read(&p).unwrap();
        let out = dir.0.join("moved.obj");
        write(&out, &doc.source, &doc.mesh, &doc.uv).unwrap();
        let back = read(&out).unwrap();
        for (a, b) in doc.mesh.positions().iter().zip(back.mesh.positions()) {
            assert_eq!(*a + DVec3::new(10.0, 0.0, -2.0), *b);
        }
        assert!(doc.uv.same_mapping(&back.uv, &doc.mesh));
    }
}
