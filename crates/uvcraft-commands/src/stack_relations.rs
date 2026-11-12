//! Explicit stacking intent, keyed by each island's face set rather than its
//! transient island index. UV edits retain only geometrically valid relations;
//! seam edits prune them when an island's topology changes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::island_groups::IslandGroups;
use serde::{Deserialize, Serialize};
use uvcraft_algo::stack;
use uvcraft_core::{Islands, Mesh, UvLayer};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StackRelations {
    groups: Vec<StoredStack>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredStack {
    members: Vec<Vec<u32>>,
    tolerance_ppm: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct Sidecar {
    version: u32,
    model_hash: u64,
    groups: Vec<Vec<Vec<u32>>>,
    #[serde(default)]
    group_tolerances_ppm: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    island_groups: Option<IslandGroups>,
}

/// The sidecar is separate from OBJ/glTF/FBX so these formats remain valid.
pub fn sidecar_path(model: &Path) -> PathBuf {
    let mut name = model.as_os_str().to_os_string();
    name.push(".uvcraft.json");
    PathBuf::from(name)
}

fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |value, &byte| {
        (value ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

impl StackRelations {
    fn member(islands: &Islands, island: usize) -> Vec<u32> {
        let mut faces = islands.faces(island).to_vec();
        faces.sort_unstable();
        faces
    }

    pub fn record(&mut self, islands: &Islands, groups: &[Vec<usize>], tolerance: f64) {
        let ppm = (tolerance.max(1e-5) * 1e6).ceil().min(u32::MAX as f64) as u32;
        let new_groups: Vec<StoredStack> = groups
            .iter()
            .filter(|group| group.len() > 1)
            .map(|group| StoredStack {
                members: group
                    .iter()
                    .map(|&island| Self::member(islands, island))
                    .collect(),
                tolerance_ppm: ppm,
            })
            .collect();
        let affected: BTreeSet<Vec<u32>> = new_groups
            .iter()
            .flat_map(|group| group.members.iter().cloned())
            .collect();
        self.groups
            .retain(|group| !group.members.iter().any(|member| affected.contains(member)));
        self.groups.extend(new_groups);
    }

    pub fn clear_for(&mut self, islands: &Islands, targets: &[usize]) {
        let selected: BTreeSet<Vec<u32>> = targets
            .iter()
            .map(|&island| Self::member(islands, island))
            .collect();
        self.groups
            .retain(|group| !group.members.iter().any(|member| selected.contains(member)));
    }

    pub fn prune(&mut self, islands: &Islands) {
        let available: BTreeSet<Vec<u32>> = (0..islands.count())
            .map(|island| Self::member(islands, island))
            .collect();
        let mut seen = BTreeSet::new();
        self.groups.retain(|group| {
            if group.members.len() < 2 {
                return false;
            }
            let members: BTreeSet<Vec<u32>> = group.members.iter().cloned().collect();
            if members.len() != group.members.len()
                || !members
                    .iter()
                    .all(|member| available.contains(member) && !seen.contains(member))
            {
                return false;
            }
            seen.extend(members);
            true
        });
    }

    /// Remove recorded stacks whose current UV regions no longer overlap.
    pub fn retain_valid(&mut self, mesh: &Mesh, uv: &UvLayer, islands: &Islands) {
        let by_faces: BTreeMap<Vec<u32>, usize> = (0..islands.count())
            .map(|island| (Self::member(islands, island), island))
            .collect();
        self.groups.retain(|group| {
            let Some(indices) = group
                .members
                .iter()
                .map(|member| by_faces.get(member).copied())
                .collect::<Option<Vec<_>>>()
            else {
                return false;
            };
            indices[1..].iter().all(|&target| {
                stack::overlaid(
                    mesh,
                    uv,
                    islands,
                    indices[0],
                    target,
                    group.tolerance_ppm as f64 / 1e6,
                )
            })
        });
    }

    /// Returns only groups whose members are all editable targets. Refuse a
    /// partial stack selection so packing cannot silently split a stack.
    pub fn packing_groups(
        &self,
        islands: &Islands,
        targets: &[usize],
    ) -> Result<Vec<Vec<usize>>, &'static str> {
        let by_faces: BTreeMap<Vec<u32>, usize> = (0..islands.count())
            .map(|island| (Self::member(islands, island), island))
            .collect();
        let selected: BTreeSet<usize> = targets.iter().copied().collect();
        let mut result = Vec::new();
        for group in &self.groups {
            let Some(indices) = group
                .members
                .iter()
                .map(|member| by_faces.get(member).copied())
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let count = indices
                .iter()
                .filter(|island| selected.contains(island))
                .count();
            if count == 0 {
                continue;
            }
            if count != indices.len() {
                return Err("select every island in a stack, or set preserve_stacks to false");
            }
            result.push(indices);
        }
        Ok(result)
    }

    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    pub fn project_sidecar_bytes(
        &self,
        island_groups: &IslandGroups,
        model_bytes: &[u8],
    ) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec_pretty(&Sidecar {
            version: 3,
            model_hash: hash(model_bytes),
            groups: self
                .groups
                .iter()
                .map(|group| group.members.clone())
                .collect(),
            group_tolerances_ppm: self
                .groups
                .iter()
                .map(|group| group.tolerance_ppm)
                .collect(),
            island_groups: Some(island_groups.clone()),
        })
    }

    pub fn project_from_sidecar(
        sidecar_bytes: &[u8],
        model_bytes: &[u8],
        islands: &Islands,
        face_count: usize,
    ) -> Option<(Self, IslandGroups)> {
        let value: Sidecar = serde_json::from_slice(sidecar_bytes).ok()?;
        if !matches!(value.version, 1..=3) || value.model_hash != hash(model_bytes) {
            return None;
        }
        let mut stacks = Self {
            groups: value
                .groups
                .into_iter()
                .enumerate()
                .map(|(i, members)| StoredStack {
                    members,
                    tolerance_ppm: value.group_tolerances_ppm.get(i).copied().unwrap_or(50_000),
                })
                .collect(),
        };
        stacks.prune(islands);
        let groups = value
            .island_groups
            .and_then(|groups| groups.from_persisted(face_count, islands))
            .unwrap_or_else(|| IslandGroups::new(face_count));
        Some((stacks, groups))
    }
}
