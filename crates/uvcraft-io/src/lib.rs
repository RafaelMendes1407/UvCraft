//! UvCraft file formats. [`read`] picks the format from the file extension
//! (or the GLB magic bytes); [`write`] saves back in the format it was read in.

pub mod gltf;
pub mod obj;

use std::path::Path;

use uvcraft_core::{Mesh, UvLayer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Obj,
    Gltf,
    Glb,
}

impl Format {
    pub fn from_path(path: &Path) -> Option<Format> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "obj" => Some(Format::Obj),
            "gltf" => Some(Format::Gltf),
            "glb" => Some(Format::Glb),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Format::Obj => "OBJ",
            Format::Gltf => "glTF",
            Format::Glb => "GLB",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IoError {
    #[error(transparent)]
    Obj(#[from] obj::ObjError),
    #[error(transparent)]
    Gltf(#[from] gltf::GltfError),
    #[error("unknown file format (expected .obj, .gltf or .glb)")]
    UnknownFormat,
    #[error("cannot save a {from} file as {to}; format conversion is not supported yet")]
    FormatMismatch {
        from: &'static str,
        to: &'static str,
    },
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
    pub source: Source,
    pub objects: Vec<String>,
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
                source: Source::Obj(d.source),
                objects: d.objects,
                missing_uv_corners: d.missing_uv_corners,
            }
        }
        Format::Gltf | Format::Glb => {
            let d = gltf::read(path)?;
            Loaded {
                mesh: d.mesh,
                uv: d.uv,
                source: Source::Gltf(d.source),
                objects: d.objects,
                missing_uv_corners: d.missing_uv_corners,
            }
        }
    })
}

/// Saves in the source's format; the extension of `path` must match it.
pub fn write(path: &Path, source: &Source, mesh: &Mesh, uv: &UvLayer) -> Result<(), IoError> {
    let target = Format::from_path(path).ok_or(IoError::UnknownFormat)?;
    if target != source.format() {
        return Err(IoError::FormatMismatch {
            from: source.format().name(),
            to: target.name(),
        });
    }
    match source {
        Source::Obj(s) => obj::write(path, s, mesh, uv)?,
        Source::Gltf(s) => gltf::write(path, s, mesh, uv)?,
    }
    Ok(())
}
