//! UvCraft file formats. [`read`] picks the format from the file extension
//! (or the GLB magic bytes); [`write`] saves in the format of the output
//! extension, converting when it differs from the source (see [`convert`]).

pub mod convert;
pub mod fbx;
pub mod gltf;
pub mod obj;

use std::collections::BTreeMap;
use std::path::Path;

use uvcraft_core::{Mesh, UvLayer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Obj,
    Gltf,
    Glb,
    Fbx,
}

impl Format {
    pub fn from_path(path: &Path) -> Option<Format> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "obj" => Some(Format::Obj),
            "gltf" => Some(Format::Gltf),
            "glb" => Some(Format::Glb),
            "fbx" => Some(Format::Fbx),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Format::Obj => "OBJ",
            Format::Gltf => "glTF",
            Format::Glb => "GLB",
            Format::Fbx => "FBX",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IoError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Obj(#[from] obj::ObjError),
    #[error(transparent)]
    Gltf(#[from] gltf::GltfError),
    #[error(transparent)]
    Fbx(#[from] fbx::FbxError),
    #[error("unknown file format (expected .obj, .gltf, .glb or .fbx)")]
    UnknownFormat,
}

/// What is needed to write a file back in its original format.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // built once per document
pub enum Source {
    Obj(obj::ObjSource),
    Gltf(gltf::GltfSource),
}

impl Source {
    pub fn format(&self) -> Format {
        match self {
            Source::Obj(_) => Format::Obj,
            Source::Gltf(g) => match g.container() {
                gltf::Container::Gltf => Format::Gltf,
                gltf::Container::Glb => Format::Glb,
            },
        }
    }
}

/// A loaded file, independent of its format.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub mesh: Mesh,
    pub uv: UvLayer,
    /// Additional FBX UV channels; channel zero is in `uv`.
    pub uv_channels: BTreeMap<u32, UvLayer>,
    pub source: Source,
    pub objects: Vec<String>,
    /// Object index for each face, in original face order.
    pub face_object: Vec<u32>,
    /// Corners that had no UVs in the file (placeholder UVs at 0,0).
    pub missing_uv_corners: usize,
}

pub fn read(path: &Path) -> Result<Loaded, IoError> {
    let format = match Format::from_path(path) {
        Some(f) => f,
        None if std::fs::read(path).is_ok_and(|b| b.starts_with(b"glTF")) => Format::Glb,
        None => return Err(IoError::UnknownFormat),
    };
    Ok(match format {
        Format::Obj => {
            let d = obj::read(path)?;
            Loaded {
                mesh: d.mesh,
                uv: d.uv,
                uv_channels: BTreeMap::new(),
                source: Source::Obj(d.source),
                objects: d.objects,
                face_object: d.face_object,
                missing_uv_corners: d.missing_uv_corners,
            }
        }
        Format::Gltf | Format::Glb => {
            let d = gltf::read(path)?;
            Loaded {
                mesh: d.mesh,
                uv: d.uv,
                uv_channels: BTreeMap::new(),
                source: Source::Gltf(d.source),
                objects: d.objects,
                face_object: d.face_object,
                missing_uv_corners: d.missing_uv_corners,
            }
        }
        Format::Fbx => {
            let mut d = fbx::parse_layers(&std::fs::read(path)?)?;
            d.layers.remove(&0);
            let parsed = d.document;
            Loaded {
                mesh: parsed.mesh,
                uv: parsed.uv,
                uv_channels: d.layers,
                source: Source::Obj(parsed.source),
                objects: parsed.objects,
                face_object: parsed.face_object,
                missing_uv_corners: parsed.missing_uv_corners,
            }
        }
    })
}

/// Saves in the format picked by the extension of `path`: back in the
/// source's format, between `.gltf` and `.glb`, or OBJ ↔ glTF via [`convert`].
pub fn write(path: &Path, source: &Source, mesh: &Mesh, uv: &UvLayer) -> Result<(), IoError> {
    commit_prepared(prepare_write(path, source, mesh, uv)?)
}

/// Renders all output files without changing the filesystem.
pub fn prepare_write(
    path: &Path,
    source: &Source,
    mesh: &Mesh,
    uv: &UvLayer,
) -> Result<Vec<(std::path::PathBuf, Vec<u8>)>, IoError> {
    let target = Format::from_path(path).ok_or(IoError::UnknownFormat)?;
    Ok(match (source, target) {
        (Source::Obj(s), Format::Fbx) => {
            vec![(path.to_path_buf(), fbx::export(mesh, uv, s)?)]
        }
        (Source::Gltf(_), Format::Fbx) => {
            return prepare_write_uv_channels(
                path,
                source,
                mesh,
                &BTreeMap::from([(0, uv.clone())]),
                0,
            );
        }
        (Source::Obj(s), Format::Obj) => vec![(path.to_path_buf(), s.export(mesh, uv)?)],
        (Source::Obj(s), _) => convert::obj_to_gltf(s, mesh, uv, path, target == Format::Glb)?,
        (Source::Gltf(s), Format::Obj) => {
            vec![(path.to_path_buf(), convert::gltf_to_obj(s, mesh, uv)?)]
        }
        (Source::Gltf(s), _) => s.export(mesh, uv, path)?,
    })
}

/// Stages every output beside its destination, then publishes it. Cancellation
/// must be checked by the caller before this commit step begins.
pub fn commit_prepared(files: Vec<(std::path::PathBuf, Vec<u8>)>) -> Result<(), IoError> {
    struct Staged(Vec<std::path::PathBuf>);
    impl Drop for Staged {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    let mut staged = Staged(Vec::new());
    for (index, (destination, bytes)) in files.iter().enumerate() {
        let mut attempt = 0u32;
        let temporary = loop {
            let candidate = destination.with_file_name(format!(
                ".{}.uvcraft-{}-{index}-{attempt}.tmp",
                destination
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("output"),
                std::process::id(),
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(mut file) => {
                    staged.0.push(candidate.clone());
                    std::io::Write::write_all(&mut file, bytes)?;
                    file.sync_all()?;
                    break candidate;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => attempt += 1,
                Err(e) => return Err(e.into()),
            }
        };
        let _ = temporary;
    }
    for ((destination, _), temporary) in files.iter().zip(&staged.0) {
        std::fs::rename(temporary, destination)?;
    }
    Ok(())
}

/// Writes several existing glTF UV channels. The active channel may split
/// vertices; secondary channels retain their original slot topology.
pub fn write_uv_channels(
    path: &Path,
    source: &Source,
    mesh: &Mesh,
    layers: &BTreeMap<u32, UvLayer>,
    active: u32,
) -> Result<(), IoError> {
    commit_prepared(prepare_write_uv_channels(
        path, source, mesh, layers, active,
    )?)
}

pub fn prepare_write_uv_channels(
    path: &Path,
    source: &Source,
    mesh: &Mesh,
    layers: &BTreeMap<u32, UvLayer>,
    active: u32,
) -> Result<Vec<(std::path::PathBuf, Vec<u8>)>, IoError> {
    match (source, Format::from_path(path)) {
        (Source::Obj(obj), Some(Format::Fbx)) => Ok(vec![(
            path.to_path_buf(),
            fbx::export_layers(mesh, layers, obj)?,
        )]),
        (Source::Gltf(g), Some(Format::Fbx)) => {
            let active_layer = layers.get(&active).ok_or_else(|| {
                IoError::Fbx(fbx::FbxError::Parse("active UV layer missing".into()))
            })?;
            let obj = obj::parse(convert::gltf_to_obj(g, mesh, active_layer)?)?;
            let mut all = layers.clone();
            for channel in g.uv_channels() {
                if let std::collections::btree_map::Entry::Vacant(entry) = all.entry(channel) {
                    if let Some((_, layer)) = g.activate_uv_channel(mesh, channel)? {
                        entry.insert(layer);
                    }
                }
            }
            let mut normalized = BTreeMap::new();
            normalized.insert(0, active_layer.clone());
            for (channel, layer) in all {
                if channel != active {
                    normalized.insert(if channel == 0 { active } else { channel }, layer);
                }
            }
            Ok(vec![(
                path.to_path_buf(),
                fbx::export_layers(&obj.mesh, &normalized, &obj.source)?,
            )])
        }
        (Source::Gltf(g), Some(Format::Glb | Format::Gltf)) => {
            Ok(g.export_uv_channels(mesh, layers, active, path)?)
        }
        _ => Err(IoError::UnknownFormat),
    }
}
