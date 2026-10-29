//! Native file representations remain usable after a received root becomes
//! FinalLocal. These fields describe formats; they never authorize source IO.
use crate::model::{NodeId, Unavailable};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, io};
mod projection;
pub(crate) use projection::inventory as project_inventory;

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeFileMetadata {
    pub windows: Vec<WindowsFileDescriptor>,
    pub promises: Vec<MacPromiseGroup>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowsFileDescriptor {
    pub root: NodeId,
    pub node: NodeId,
    /// Original FileContents lindex, including explicitly listed directories.
    pub index: u32,
    /// FILEDESCRIPTORW header through nFileSizeLow; its name comes from the
    /// validated Unicode namespace.
    pub wide: Option<Vec<u8>>,
    /// FILEDESCRIPTORA includes the original ANSI name: it can be a lossy alias
    /// which cannot be reconstructed from the Unicode name on another system.
    /// Native publishers validate/sanitize it before offering it to applications.
    pub ansi: Option<Vec<u8>>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MacPromiseGroup {
    /// The inventory's process-independent promise container identifier.
    pub anchor: NodeId,
    /// Actual received roots, in the original promise item's name order.
    pub roots: Vec<NodeId>,
    pub indices: Vec<u32>,
    /// Process-independent promise controls captured with the immutable bytes.
    /// Native IPC identities were already replaced by type metadata at capture.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controls: Vec<MacPromiseControl>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MacPromiseControl {
    pub name: String,
    pub bytes: Vec<u8>,
}
#[cfg(target_os = "macos")]
impl MacPromiseGroup {
    pub(crate) fn controls(item: &crate::model::ContentItem) -> Vec<MacPromiseControl> {
        item.representations
            .iter()
            .filter_map(|rep| match (&rep.format, &rep.data) {
                (crate::model::NativeFormat::Mac(name), crate::model::RepresentationData::Promise(_, bytes)) => {
                    Some(MacPromiseControl {
                        name: name.clone(),
                        bytes: bytes.clone(),
                    })
                }
                _ => None,
            })
            .collect()
    }
}
impl NativeFileMetadata {
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty() && self.promises.is_empty()
    }
    pub fn validate(&self, roots: &[NodeId]) -> io::Result<()> {
        let invalid = || super::unavailable(Unavailable::InvalidData);
        let roots: HashSet<_> = roots.iter().copied().collect();
        if self.windows.len() > 65_535 || self.promises.len() > 65_535 {
            return Err(invalid());
        }
        let mut nodes = HashSet::new();
        let mut indices = HashSet::new();
        for entry in &self.windows {
            if !roots.contains(&entry.root)
                || !nodes.insert(entry.node)
                || !indices.insert(entry.index)
                || (entry.wide.is_none() && entry.ansi.is_none())
                || entry.wide.as_ref().is_some_and(|bytes| bytes.len() != 72)
                || entry.ansi.as_ref().is_some_and(|bytes| bytes.len() != 332)
            {
                return Err(invalid());
            }
        }
        let mut anchors = HashSet::new();
        let mut promised = HashSet::new();
        for group in &self.promises {
            let mut formats = HashSet::new();
            if group.controls.len() > 64
                || group.controls.iter().any(|control| {
                    control.name.is_empty()
                        || control.name.len() > 4096
                        || control.name.contains('\0')
                        || control.bytes.len() > 65536
                        || !formats.insert(&control.name)
                })
            {
                return Err(invalid());
            }
            if roots.contains(&group.anchor)
                || !anchors.insert(group.anchor)
                || group.roots.is_empty()
                || group.roots.len() > 65_535
                || group.indices.len() != group.roots.len()
                || group.indices.windows(2).any(|pair| pair[0] >= pair[1])
                || group
                    .roots
                    .iter()
                    .any(|id| !roots.contains(id) || !promised.insert(*id))
            {
                return Err(invalid());
            }
        }
        Ok(())
    }
    /// Resolve exactly the advertised capability scope. Promise containers can
    /// expand to several actual roots, but overlapping requests cannot silently
    /// duplicate or widen authority. Order is the source's native name order.
    pub fn resolve_roots(&self, requested: &[NodeId], available: &[NodeId]) -> io::Result<Vec<NodeId>> {
        let invalid = || super::unavailable(Unavailable::InvalidData);
        self.validate(available)?;
        if requested.is_empty() || requested.len() > u16::MAX as usize {
            return Err(invalid());
        }
        let mut seen = HashSet::new();
        let mut resolved = Vec::new();
        for requested in requested {
            let roots = if let Some(group) = self.promises.iter().find(|group| group.anchor == *requested) {
                group.roots.as_slice()
            } else if available.contains(requested) {
                std::slice::from_ref(requested)
            } else {
                return Err(invalid());
            };
            for root in roots {
                if !seen.insert(*root) || resolved.len() == u16::MAX as usize {
                    return Err(invalid());
                }
                resolved.push(*root);
            }
        }
        Ok(resolved)
    }
    pub fn restricted(&self, roots: &[NodeId]) -> Self {
        let roots: HashSet<_> = roots.iter().copied().collect();
        Self {
            windows: self
                .windows
                .iter()
                .filter(|entry| roots.contains(&entry.root))
                .cloned()
                .collect(),
            promises: self
                .promises
                .iter()
                .filter_map(|group| {
                    let (selected, indices): (Vec<_>, Vec<_>) = group
                        .roots
                        .iter()
                        .zip(&group.indices)
                        .filter(|(root, _)| roots.contains(root))
                        .map(|(root, index)| (*root, *index))
                        .unzip();
                    (!selected.is_empty()).then_some(MacPromiseGroup {
                        controls: group.controls.clone(),
                        anchor: group.anchor,
                        roots: selected,
                        indices,
                    })
                })
                .collect(),
        }
    }
    /// Join disjoint root subsets while restoring a mixed linked/local catalog.
    pub fn extend(&mut self, other: &Self) -> io::Result<()> {
        for entry in &other.windows {
            if let Some(old) = self
                .windows
                .iter()
                .find(|old| old.node == entry.node || old.index == entry.index)
            {
                if old != entry {
                    return Err(super::unavailable(Unavailable::InvalidData));
                }
            } else {
                self.windows.push(entry.clone());
            }
        }
        self.windows.sort_by_key(|entry| entry.index);
        for group in &other.promises {
            if let Some(old) = self.promises.iter_mut().find(|old| old.anchor == group.anchor) {
                if old.controls != group.controls {
                    return Err(super::unavailable(Unavailable::InvalidData));
                }
                let mut entries: Vec<_> = old.roots.iter().copied().zip(old.indices.iter().copied()).collect();
                for (&root, &index) in group.roots.iter().zip(&group.indices) {
                    if let Some(previous) = entries.iter().find(|(id, rank)| *id == root || *rank == index) {
                        if *previous != (root, index) {
                            return Err(super::unavailable(Unavailable::InvalidData));
                        }
                    } else {
                        entries.push((root, index));
                    }
                }
                entries.sort_by_key(|(_, index)| *index);
                (old.roots, old.indices) = entries.into_iter().unzip();
            } else {
                self.promises.push(group.clone());
            }
        }
        Ok(())
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn partial_root_receipts_preserve_original_promise_order() {
        let metadata = NativeFileMetadata {
            windows: vec![],
            promises: vec![MacPromiseGroup {
                controls: vec![MacPromiseControl {
                    name: "com.apple.NSFilePromiseItemMetaData".into(),
                    bytes: b"public.data".to_vec(),
                }],
                anchor: NodeId(9),
                roots: vec![NodeId(3), NodeId(1), NodeId(2)],
                indices: vec![0, 1, 2],
            }],
        };
        let mut merged = metadata.restricted(&[NodeId(2)]);
        merged.extend(&metadata.restricted(&[NodeId(1)])).unwrap();
        merged.extend(&metadata.restricted(&[NodeId(3)])).unwrap();
        assert_eq!(merged, metadata);
        merged.validate(&[NodeId(1), NodeId(2), NodeId(3)]).unwrap();
        let mut conflicting = metadata.restricted(&[NodeId(1)]);
        conflicting.promises[0].indices[0] = 0;
        assert!(merged.extend(&conflicting).is_err());
        let mut changed = metadata.restricted(&[NodeId(1)]);
        changed.promises[0].controls[0].bytes = b"public.jpeg".to_vec();
        assert!(
            merged.extend(&changed).is_err(),
            "different native controls cannot describe the same immutable root"
        );
    }
}
