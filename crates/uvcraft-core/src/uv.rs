//! UV layer: per-corner UV topology plus UV coordinates.

use glam::DVec2;

use crate::mesh::Mesh;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum UvError {
    #[error("UV layer has {found} corners, but the mesh has {expected}")]
    CornerCountMismatch { expected: usize, found: usize },
    #[error("corner {corner} references UV {index}, but the layer has {uv_count} UVs")]
    IndexOutOfRange {
        corner: usize,
        index: u32,
        uv_count: usize,
    },
}

/// How an edge looks in UV space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// Both faces share the edge's UV vertices: the edge is inside an island.
    Interior,
    /// Both faces exist but use different UV vertices: a cut.
    Seam,
    /// Only one face uses the edge (open edge of the 3D mesh).
    Border,
    /// More than two faces use the edge.
    NonManifold,
}

impl EdgeKind {
    /// True when the edge lies on an island boundary.
    pub fn is_island_boundary(self) -> bool {
        self != EdgeKind::Interior
    }
}

/// One UV channel. A seam is simply an edge whose two faces reference
/// different UV vertices, so cut and weld only change `corner_uv`.
#[derive(Debug, Clone, PartialEq)]
pub struct UvLayer {
    pub name: String,
    /// UV vertex index for each mesh corner.
    pub corner_uv: Vec<u32>,
    pub uvs: Vec<DVec2>,
}

impl UvLayer {
    pub fn validate(&self, mesh: &Mesh) -> Result<(), UvError> {
        if self.corner_uv.len() != mesh.corner_count() {
            return Err(UvError::CornerCountMismatch {
                expected: mesh.corner_count(),
                found: self.corner_uv.len(),
            });
        }
        if let Some((corner, &index)) = self
            .corner_uv
            .iter()
            .enumerate()
            .find(|&(_, &i)| i as usize >= self.uvs.len())
        {
            return Err(UvError::IndexOutOfRange {
                corner,
                index,
                uv_count: self.uvs.len(),
            });
        }
        Ok(())
    }

    /// Removes UV vertices no corner references; keeps the order of the rest.
    pub fn compact(&mut self) {
        let mut remap = vec![u32::MAX; self.uvs.len()];
        for &i in &self.corner_uv {
            remap[i as usize] = 0;
        }
        let mut next = 0u32;
        let mut uvs = Vec::with_capacity(self.uvs.len());
        for (i, r) in remap.iter_mut().enumerate() {
            if *r == 0 {
                *r = next;
                next += 1;
                uvs.push(self.uvs[i]);
            }
        }
        for i in &mut self.corner_uv {
            *i = remap[*i as usize];
        }
        self.uvs = uvs;
    }

    pub fn corner_uv(&self, c: usize) -> usize {
        self.corner_uv[c] as usize
    }

    pub fn uv_at_corner(&self, c: usize) -> DVec2 {
        self.uvs[self.corner_uv(c)]
    }

    /// Same UV mapping as `other`: every corner has the same UV coordinates
    /// and every edge the same kind. Indices may differ.
    pub fn same_mapping(&self, other: &UvLayer, mesh: &Mesh) -> bool {
        self.corner_uv.len() == other.corner_uv.len()
            && (0..self.corner_uv.len()).all(|c| self.uv_at_corner(c) == other.uv_at_corner(c))
            && (0..mesh.edge_count()).all(|e| self.edge_kind(mesh, e) == other.edge_kind(mesh, e))
    }

    pub fn edge_kind(&self, mesh: &Mesh, e: usize) -> EdgeKind {
        if mesh.is_non_manifold(e) {
            return EdgeKind::NonManifold;
        }
        let (c0, Some(c1)) = mesh.edge_corners(e) else {
            return EdgeKind::Border;
        };
        let (a0, b0) = (self.corner_uv[c0], self.corner_uv[mesh.next(c0)]);
        let (a1, b1) = (self.corner_uv[c1], self.corner_uv[mesh.next(c1)]);
        let connected = if mesh.corner_vertex(c1) == mesh.corner_vertex(c0) {
            // Same traversal direction (inconsistent winding).
            a0 == a1 && b0 == b1
        } else {
            a0 == b1 && b0 == a1
        };
        if connected {
            EdgeKind::Interior
        } else {
            EdgeKind::Seam
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    fn mesh() -> Mesh {
        let p = vec![DVec3::ZERO, DVec3::X, DVec3::Y, DVec3::ONE];
        Mesh::new(p, &[[0, 1, 2], [2, 1, 3]]).unwrap()
    }

    #[test]
    fn shared_uvs_are_interior_split_uvs_are_seams() {
        let m = mesh();
        let shared_edge = m.corner_edge(1); // 1→2
        let welded = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 2, 1, 3],
            uvs: vec![DVec2::ZERO; 4],
        };
        welded.validate(&m).unwrap();
        assert_eq!(welded.edge_kind(&m, shared_edge), EdgeKind::Interior);
        assert_eq!(welded.edge_kind(&m, m.corner_edge(0)), EdgeKind::Border);

        let cut = UvLayer {
            corner_uv: vec![0, 1, 2, 3, 4, 5],
            uvs: vec![DVec2::ZERO; 6],
            ..welded
        };
        assert_eq!(cut.edge_kind(&m, shared_edge), EdgeKind::Seam);
    }

    #[test]
    fn validate_reports_errors() {
        let m = mesh();
        let bad = UvLayer {
            name: "uv".into(),
            corner_uv: vec![0, 1, 2, 2, 1, 7],
            uvs: vec![DVec2::ZERO; 4],
        };
        assert!(matches!(
            bad.validate(&m),
            Err(UvError::IndexOutOfRange { corner: 5, .. })
        ));
    }
}
