//! Persistent hierarchy of island groups, attached to mesh faces.
//!
//! Island numbers can change when seams are cut or welded. Storing ownership
//! per face keeps groups attached to the same polygons across those edits.

use std::collections::{BTreeMap, BTreeSet};

use uvcraft_core::Islands;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    Material,
    Object,
    SmoothingGroup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TileDistribution {
    Group,
    Material,
    Object,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IslandGroup {
    pub id: u64,
    pub parent: Option<u64>,
    pub name: String,
    pub color: [u8; 4],
    pub locked: bool,
    pub visible: bool,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IslandGroups {
    groups: BTreeMap<u64, IslandGroup>,
    face_group: Vec<Option<u64>>,
    #[serde(default, with = "face_members_serde")]
    member_faces: BTreeMap<Vec<u32>, u64>,
    next_id: u64,
}

mod face_members_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(
        value: &BTreeMap<Vec<u32>, u64>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<Vec<u32>, u64>, D::Error> {
        let entries: Vec<(Vec<u32>, u64)> = Vec::deserialize(deserializer)?;
        Ok(entries.into_iter().collect())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GroupError {
    #[error("group {0} does not exist")]
    Unknown(u64),
    #[error("group name must not be empty")]
    EmptyName,
    #[error("a group cannot be nested under itself or a descendant")]
    Cycle,
    #[error("island {0} does not exist")]
    InvalidIsland(usize),
    #[error("island {0} is not a member of group {1}")]
    NotMember(usize, u64),
}

impl IslandGroups {
    pub fn new(face_count: usize) -> Self {
        Self {
            groups: BTreeMap::new(),
            face_group: vec![None; face_count],
            member_faces: BTreeMap::new(),
            next_id: 1,
        }
    }

    pub fn nodes(&self) -> impl Iterator<Item = &IslandGroup> {
        self.groups.values()
    }

    pub fn get(&self, id: u64) -> Option<&IslandGroup> {
        self.groups.get(&id)
    }

    pub fn face_group(&self, face: usize) -> Option<u64> {
        self.face_group[face]
    }

    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    fn signature(islands: &Islands, island: usize) -> Vec<u32> {
        let mut faces = islands.faces(island).to_vec();
        faces.sort_unstable();
        faces
    }

    /// A changed island is no longer assigned to an old group unless its
    /// complete face set is still present. This never guesses at split/merge
    /// ownership and never recycles a removed island's identity.
    pub fn reconcile(&mut self, islands: &Islands) {
        let available: BTreeSet<Vec<u32>> = (0..islands.count())
            .map(|island| Self::signature(islands, island))
            .collect();
        self.member_faces
            .retain(|faces, owner| available.contains(faces) && self.groups.contains_key(owner));
        self.face_group.fill(None);
        for (faces, owner) in &self.member_faces {
            for &face in faces {
                if let Some(slot) = self.face_group.get_mut(face as usize) {
                    *slot = Some(*owner);
                }
            }
        }
    }

    /// Reject corrupted hierarchy metadata while retaining valid group IDs.
    pub fn from_persisted(mut self, face_count: usize, islands: &Islands) -> Option<Self> {
        if self.groups.iter().any(|(&id, group)| {
            id != group.id
                || group
                    .parent
                    .is_some_and(|parent| !self.groups.contains_key(&parent))
        }) {
            return None;
        }
        for &id in self.groups.keys() {
            let mut seen = BTreeSet::new();
            let mut cursor = Some(id);
            while let Some(current) = cursor {
                if !seen.insert(current) {
                    return None;
                }
                cursor = self.groups.get(&current)?.parent;
            }
        }
        self.next_id = self.next_id.max(
            self.groups
                .keys()
                .next_back()
                .copied()
                .unwrap_or(0)
                .saturating_add(1),
        );
        self.face_group = vec![None; face_count];
        self.reconcile(islands);
        Some(self)
    }

    pub fn root_of_face(&self, face: usize) -> Option<&IslandGroup> {
        let mut id = self.face_group[face]?;
        while let Some(parent) = self.groups[&id].parent {
            id = parent;
        }
        self.groups.get(&id)
    }

    fn check_group(&self, id: u64) -> Result<(), GroupError> {
        self.groups
            .contains_key(&id)
            .then_some(())
            .ok_or(GroupError::Unknown(id))
    }

    fn is_ancestor(&self, ancestor: u64, mut node: u64) -> bool {
        loop {
            if ancestor == node {
                return true;
            }
            let Some(parent) = self.groups.get(&node).and_then(|group| group.parent) else {
                return false;
            };
            node = parent;
        }
    }

    /// Creates a group, optionally moving existing groups beneath it and
    /// assigning complete islands to it. All validation happens before edits.
    pub fn create(
        &mut self,
        name: &str,
        parent: Option<u64>,
        children: &[u64],
        islands_to_add: &[usize],
        islands: &Islands,
    ) -> Result<u64, GroupError> {
        if name.trim().is_empty() {
            return Err(GroupError::EmptyName);
        }
        if let Some(parent) = parent {
            self.check_group(parent)?;
        }
        for &child in children {
            self.check_group(child)?;
            if parent.is_some_and(|parent| self.is_ancestor(child, parent)) {
                return Err(GroupError::Cycle);
            }
        }
        for &island in islands_to_add {
            if island >= islands.count() {
                return Err(GroupError::InvalidIsland(island));
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        const COLORS: [[u8; 4]; 8] = [
            [128, 180, 240, 255],
            [245, 165, 95, 255],
            [155, 215, 135, 255],
            [215, 145, 210, 255],
            [105, 210, 205, 255],
            [245, 205, 100, 255],
            [175, 165, 245, 255],
            [240, 130, 145, 255],
        ];
        self.groups.insert(
            id,
            IslandGroup {
                id,
                parent,
                name: name.trim().to_owned(),
                color: COLORS[(id as usize - 1) % COLORS.len()],
                locked: false,
                visible: true,
                selected: false,
            },
        );
        for &child in children {
            self.groups.get_mut(&child).unwrap().parent = Some(id);
        }
        self.assign_islands(id, islands_to_add, islands)?;
        Ok(id)
    }

    pub fn assign_islands(
        &mut self,
        id: u64,
        targets: &[usize],
        islands: &Islands,
    ) -> Result<(), GroupError> {
        self.check_group(id)?;
        for &target in targets {
            if target >= islands.count() {
                return Err(GroupError::InvalidIsland(target));
            }
        }
        for &target in targets {
            let faces = Self::signature(islands, target);
            self.member_faces.insert(faces.clone(), id);
            for face in faces {
                self.face_group[face as usize] = Some(id);
            }
        }
        Ok(())
    }

    pub fn remove_islands(
        &mut self,
        id: u64,
        targets: &[usize],
        islands: &Islands,
    ) -> Result<(), GroupError> {
        self.check_group(id)?;
        for &target in targets {
            if target >= islands.count() {
                return Err(GroupError::InvalidIsland(target));
            }
            let faces = Self::signature(islands, target);
            let owner = self.member_faces.get(&faces).copied();
            if !owner.is_some_and(|owner| self.is_ancestor(id, owner)) {
                return Err(GroupError::NotMember(target, id));
            }
        }
        for &target in targets {
            let faces = Self::signature(islands, target);
            self.member_faces.remove(&faces);
            for face in faces {
                self.face_group[face as usize] = None;
            }
        }
        Ok(())
    }

    /// Deletes a group. Its children and direct faces move to its parent.
    pub fn ungroup(&mut self, id: u64) -> Result<(), GroupError> {
        let group = self.groups.remove(&id).ok_or(GroupError::Unknown(id))?;
        for child in self.groups.values_mut() {
            if child.parent == Some(id) {
                child.parent = group.parent;
            }
        }
        for owner in &mut self.face_group {
            if *owner == Some(id) {
                *owner = group.parent;
            }
        }
        for owner in self.member_faces.values_mut() {
            if *owner == id {
                if let Some(parent) = group.parent {
                    *owner = parent;
                }
            }
        }
        if group.parent.is_none() {
            self.member_faces.retain(|_, owner| *owner != id);
        }
        Ok(())
    }

    pub fn set_parent(&mut self, id: u64, parent: Option<u64>) -> Result<(), GroupError> {
        self.check_group(id)?;
        if let Some(parent) = parent {
            self.check_group(parent)?;
            if self.is_ancestor(id, parent) {
                return Err(GroupError::Cycle);
            }
        }
        self.groups.get_mut(&id).unwrap().parent = parent;
        Ok(())
    }

    pub fn set_properties(
        &mut self,
        id: u64,
        name: Option<&str>,
        color: Option<[u8; 4]>,
        locked: Option<bool>,
        visible: Option<bool>,
        selected: Option<bool>,
    ) -> Result<(), GroupError> {
        if name.is_some_and(|name| name.trim().is_empty()) {
            return Err(GroupError::EmptyName);
        }
        let group = self.groups.get_mut(&id).ok_or(GroupError::Unknown(id))?;
        if let Some(name) = name {
            group.name = name.trim().to_owned();
        }
        if let Some(color) = color {
            group.color = color;
        }
        if let Some(locked) = locked {
            group.locked = locked;
        }
        if let Some(visible) = visible {
            group.visible = visible;
        }
        if let Some(selected) = selected {
            group.selected = selected;
        }
        Ok(())
    }

    pub fn contains_face(&self, id: u64, face: usize) -> bool {
        self.face_group[face].is_some_and(|owner| self.is_ancestor(id, owner))
    }

    pub fn face_locked(&self, face: usize) -> bool {
        self.inherited(
            self.face_group[face],
            |group| group.locked,
            false,
            |a, b| a || b,
        )
    }

    pub fn face_visible(&self, face: usize) -> bool {
        self.inherited(
            self.face_group[face],
            |group| group.visible,
            true,
            |a, b| a && b,
        )
    }

    pub fn face_selected(&self, face: usize) -> bool {
        self.inherited(
            self.face_group[face],
            |group| group.selected,
            false,
            |a, b| a || b,
        )
    }

    pub fn face_color(&self, face: usize) -> Option<[u8; 4]> {
        self.face_group[face].map(|id| self.groups[&id].color)
    }

    pub fn islands_in(&self, id: u64, islands: &Islands) -> Result<Vec<usize>, GroupError> {
        self.check_group(id)?;
        Ok((0..islands.count())
            .filter(|&i| {
                self.member_faces
                    .get(&Self::signature(islands, i))
                    .is_some_and(|&owner| self.is_ancestor(id, owner))
            })
            .collect())
    }

    fn inherited(
        &self,
        mut id: Option<u64>,
        value: impl Fn(&IslandGroup) -> bool,
        initial: bool,
        combine: impl Fn(bool, bool) -> bool,
    ) -> bool {
        let mut result = initial;
        while let Some(current) = id {
            let group = &self.groups[&current];
            result = combine(result, value(group));
            id = group.parent;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::{DVec2, DVec3};
    use uvcraft_core::{Mesh, UvLayer};

    fn islands() -> Islands {
        let mesh = Mesh::new(
            vec![
                DVec3::ZERO,
                DVec3::X,
                DVec3::Y,
                DVec3::ONE,
                DVec3::Z,
                DVec3::X + DVec3::Z,
            ],
            &[[0, 1, 2], [3, 4, 5]],
        )
        .unwrap();
        let uv = UvLayer {
            name: "uv".into(),
            corner_uv: (0..6).collect(),
            uvs: vec![DVec2::ZERO; 6],
        };
        Islands::compute(&mesh, &uv)
    }

    #[test]
    fn new_groups_have_distinct_identifying_colors() {
        let islands = islands();
        let mut groups = IslandGroups::new(2);
        let first = groups.create("First", None, &[], &[0], &islands).unwrap();
        let second = groups.create("Second", None, &[], &[1], &islands).unwrap();
        assert_ne!(
            groups.get(first).unwrap().color,
            groups.get(second).unwrap().color
        );
        assert_ne!(groups.face_color(0), groups.face_color(1));
    }

    #[test]
    fn tree_inherits_flags_and_ungroup_adopts_children_and_faces() {
        let islands = islands();
        let mut tree = IslandGroups::new(2);
        let parent = tree.create("Parent", None, &[], &[0], &islands).unwrap();
        let child = tree
            .create("Child", Some(parent), &[], &[1], &islands)
            .unwrap();
        tree.set_properties(parent, None, None, Some(true), Some(false), Some(true))
            .unwrap();
        tree.set_properties(child, None, Some([255, 0, 0, 255]), None, None, None)
            .unwrap();
        assert!(tree.face_locked(1));
        assert!(!tree.face_visible(1));
        assert!(tree.face_selected(1));
        assert_eq!(tree.face_color(1), Some([255, 0, 0, 255]));
        assert_eq!(tree.islands_in(parent, &islands).unwrap(), vec![0, 1]);
        tree.ungroup(parent).unwrap();
        assert_eq!(tree.get(child).unwrap().parent, None);
        assert_eq!(tree.face_group(0), None);
        assert!(!tree.face_locked(1));
        assert!(tree.face_visible(1));
    }

    #[test]
    fn validation_is_atomic_and_cycles_are_rejected() {
        let islands = islands();
        let mut tree = IslandGroups::new(2);
        let a = tree.create("A", None, &[], &[0], &islands).unwrap();
        let b = tree.create("B", Some(a), &[], &[1], &islands).unwrap();
        let before = tree.clone();
        assert_eq!(tree.set_parent(a, Some(b)), Err(GroupError::Cycle));
        assert_eq!(
            tree.create("new", Some(b), &[a], &[], &islands),
            Err(GroupError::Cycle)
        );
        assert_eq!(
            tree.assign_islands(a, &[0, 99], &islands),
            Err(GroupError::InvalidIsland(99))
        );
        assert_eq!(tree, before);
    }
}
