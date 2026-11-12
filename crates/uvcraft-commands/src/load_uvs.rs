//! `load_uvs`: take the UVs of another file with the same topology.

use std::collections::HashMap;

use uvcraft_core::{Mesh, UvLayer};

/// Builds a UV layer for `mesh` from the UVs of `other`, matched corner by
/// corner. The two meshes must have the same faces in the same order, with the
/// same corner counts, and corners must share vertices the same way (in at
/// least one direction, so that welding duplicates differently is tolerated).
///
/// The new UV vertices are keyed by (mesh vertex, other UV vertex), so each
/// one belongs to exactly one mesh vertex.
pub(crate) fn transfer_uvs(
    mesh: &Mesh,
    name: &str,
    other: &Mesh,
    other_uv: &UvLayer,
) -> Result<UvLayer, String> {
    if other.face_count() != mesh.face_count() {
        return Err(format!(
            "topology mismatch: {} faces, but the loaded mesh has {}",
            other.face_count(),
            mesh.face_count()
        ));
    }
    if let Some(f) =
        (0..mesh.face_count()).find(|&f| mesh.face_corners(f).len() != other.face_corners(f).len())
    {
        return Err(format!(
            "topology mismatch: face {f} has {} corners, but {} in the loaded mesh",
            other.face_corners(f).len(),
            mesh.face_corners(f).len()
        ));
    }
    // Corner c of both meshes is now the same corner (same CSR layout).
    if !(maps_consistently(mesh, other) || maps_consistently(other, mesh)) {
        return Err(
            "topology mismatch: faces connect their vertices differently than in the loaded mesh"
                .into(),
        );
    }

    let mut index: HashMap<(u32, u32), u32> = HashMap::new();
    let mut uvs = Vec::new();
    let corner_uv = (0..mesh.corner_count())
        .map(|c| {
            let key = (mesh.corner_vertex(c) as u32, other_uv.corner_uv[c]);
            *index.entry(key).or_insert_with(|| {
                uvs.push(other_uv.uvs[key.1 as usize]);
                (uvs.len() - 1) as u32
            })
        })
        .collect();
    Ok(UvLayer {
        name: name.to_string(),
        corner_uv,
        uvs,
    })
}

/// True if every vertex of `a` maps to a single vertex of `b` through the
/// shared corner layout.
fn maps_consistently(a: &Mesh, b: &Mesh) -> bool {
    let mut map = vec![u32::MAX; a.vertex_count()];
    (0..a.corner_count()).all(|c| {
        let (va, vb) = (a.corner_vertex(c), b.corner_vertex(c) as u32);
        let seen = std::mem::replace(&mut map[va], vb);
        seen == u32::MAX || seen == vb
    })
}
