//! FinalLocal metadata is committed when a one-shot receive finishes. There is
//! no receipt, sender ACK, OS-pin confirmation, or native content recheck.
use super::*;
use crate::content::{grants::FulfilledRoot, namespace::NativeFiles, native_metadata::NativeFileMetadata};
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, Weak};

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct FinalLocal {
    offer: Option<OfferId>,
    order: Vec<NodeId>,
    roots: Vec<LocalRoot>,
    #[serde(default, skip_serializing_if = "NativeFileMetadata::is_empty")]
    native: NativeFileMetadata,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalRoot {
    receipt: FulfilledRoot,
    entries: Vec<FileNode>,
}
/// Build 72 kept fully received roots linked while waiting for an OS-pin
/// receipt. Recognize only that old digest-shaped identity during recovery;
/// current arrival-based receives keep their ordinary restart failure rule.
/// User decision 2026-10-29: preserve received bytes, without another proof.
pub(crate) fn legacy_pending_roots(descriptor: &serde_json::Value) -> HashSet<NodeId> {
    descriptor["fulfilled"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|root| {
            let digest = root["digest"].as_array()?;
            (root.get("id").is_none()
                && digest.len() == 32
                && digest.iter().all(|byte| byte.as_u64().is_some_and(|byte| byte <= 255)))
            .then(|| root["node"].as_u64().map(NodeId))
            .flatten()
        })
        .collect()
}

impl FinalLocal {
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }
    pub fn contains(&self, node: NodeId) -> bool {
        self.roots
            .iter()
            .any(|root| root.entries.iter().any(|entry| entry.id == node))
    }
    pub fn forget(&mut self, root: NodeId) {
        self.order.retain(|node| *node != root);
        self.native = self.native.restricted(&self.order);
    }
    pub fn remaining_link(&self, descriptor: &Descriptor) -> io::Result<Option<Descriptor>> {
        if self.offer.is_some_and(|offer| offer != descriptor.offer) {
            return Err(unavailable(Unavailable::Permission));
        }
        let mut remaining = descriptor.clone();
        remaining
            .roots
            .retain(|root| !self.roots.iter().any(|local| local.receipt.node == root.id));
        remaining
            .fulfilled
            .retain(|root| !self.roots.iter().any(|local| local.receipt.node == root.node));
        remaining.native = remaining
            .native
            .restricted(&remaining.roots.iter().map(|root| root.id).collect::<Vec<_>>());
        let remaining = (!remaining.roots.is_empty()).then_some(remaining);
        self.validate(remaining.as_ref())?;
        Ok(remaining)
    }
    pub fn validate(&self, link: Option<&Descriptor>) -> io::Result<()> {
        let invalid = || unavailable(Unavailable::InvalidData);
        if let Some(link) = link {
            link.validate()?;
        }
        if self.roots.is_empty() {
            return if self.offer.is_none() && self.order.is_empty() && self.native.is_empty() {
                Ok(())
            } else {
                Err(invalid())
            };
        }
        let offer = self.offer.ok_or_else(invalid)?;
        if link.is_some_and(|link| link.provisional || link.offer != offer) {
            return Err(invalid());
        }
        let mut root_ids = HashSet::new();
        let mut ids = HashSet::new();
        let mut bytes = 0usize;
        for root in &self.roots {
            if !root_ids.insert(root.receipt.node) || root.entries.is_empty() {
                return Err(invalid());
            }
            let entries: std::collections::HashMap<_, _> = root.entries.iter().map(|e| (e.id, e)).collect();
            let mut names = HashSet::new();
            for entry in &root.entries {
                if !entry.valid()
                    || !ids.insert(entry.id)
                    || entry.version == 0
                    || (entry.kind == NodeKind::File && !matches!(entry.size, ContentSize::Known(_)))
                    || !names.insert((entry.parent, entry.name.as_str()))
                {
                    return Err(invalid());
                }
                bytes = bytes
                    .checked_add(entry.name.len() + entry.kind.symlink_target().map_or(0, str::len) + 512)
                    .ok_or_else(invalid)?;
                if bytes > METADATA_MAX as usize || ids.len() > crate::model::MAX_DISCOVERED_NODES {
                    return Err(invalid());
                }
                let mut current = entry;
                for depth in 0usize..256 {
                    match current.parent {
                        Some(parent) if depth < 255 => {
                            current = entries.get(&parent).ok_or_else(invalid)?;
                            if current.kind != NodeKind::Directory {
                                return Err(invalid());
                            }
                        }
                        None if current.id == root.receipt.node => {
                            if entry
                                .kind
                                .symlink_target()
                                .is_some_and(|target| !oink_foundation::content::safe_symlink_target(target, depth))
                            {
                                return Err(invalid());
                            }
                            break;
                        }
                        _ => return Err(invalid()),
                    }
                }
            }
        }
        if let Some(link) = link {
            for root in &link.roots {
                if !root_ids.insert(root.id) || ids.contains(&root.id) {
                    return Err(invalid());
                }
            }
        }
        if self.order.len() != root_ids.len() || self.order.iter().copied().collect::<HashSet<_>>() != root_ids {
            return Err(invalid());
        }
        self.native.validate(&self.order)?;
        Ok(())
    }
    /// Completion is an arrival event, not a second read of the received tree.
    pub fn commit(
        &mut self,
        link: &mut Option<Descriptor>,
        root: &FulfilledRoot,
        manifest: &[FileNode],
    ) -> io::Result<()> {
        if self.roots.iter().any(|local| local.receipt == *root) {
            return Ok(());
        }
        let source = link.as_ref().ok_or_else(|| unavailable(Unavailable::Permission))?;
        if source.provisional || !source.fulfilled.contains(root) {
            return Err(unavailable(Unavailable::Permission));
        }
        let entries = subtree(root.node, manifest)?;
        let mut next = self.clone();
        let mut remaining = source.clone();
        if next.roots.is_empty() {
            next.offer = Some(source.offer);
            next.order = source.roots.iter().map(|root| root.id).collect();
            next.native = source.native.clone();
        }
        next.roots.push(LocalRoot {
            receipt: root.clone(),
            entries,
        });
        remaining.roots.retain(|entry| entry.id != root.node);
        remaining.fulfilled.retain(|entry| entry.node != root.node);
        remaining.native = remaining
            .native
            .restricted(&remaining.roots.iter().map(|root| root.id).collect::<Vec<_>>());
        let remaining = (!remaining.roots.is_empty()).then_some(remaining);
        next.validate(remaining.as_ref())?;
        *self = next;
        *link = remaining;
        Ok(())
    }
    pub(crate) fn recover_legacy_received(
        &mut self,
        link: &mut Option<Descriptor>,
        node: NodeId,
        manifest: &[FileNode],
    ) -> io::Result<()> {
        let mut next = link.clone();
        let root = next
            .as_mut()
            .and_then(|link| link.fulfilled.iter_mut().find(|root| root.node == node))
            .ok_or(io::ErrorKind::InvalidData)?;
        // The obsolete content digest is not an identity in the new model.
        // Assign a local random identity once, in the same FinalLocal commit.
        getrandom::fill(&mut root.id).map_err(|_| io::Error::other("local identity unavailable"))?;
        let root = root.clone();
        self.commit(&mut next, &root, manifest)?;
        *link = next;
        Ok(())
    }
    /// The native provider may enumerate FinalLocal metadata offline, but its
    /// hydration callbacks must never fetch those bodies from the old sender.
    pub fn restore(&self, link: Option<Descriptor>, connector: &Connector) -> io::Result<Arc<dyn Namespace>> {
        self.validate(link.as_ref())?;
        let source = link.map(|link| connector.restore(link)).transpose()?;
        Ok(Arc::new(LocalNamespace {
            settlement: self.clone(),
            source,
            native: Mutex::new(None),
        }))
    }
}

struct LocalNamespace {
    settlement: FinalLocal,
    source: Option<Arc<dyn Namespace>>,
    native: Mutex<Option<Weak<dyn NativeFiles>>>,
}
impl LocalNamespace {
    fn entry(&self, node: NodeId) -> Option<&FileNode> {
        self.settlement
            .roots
            .iter()
            .flat_map(|root| &root.entries)
            .find(|entry| entry.id == node)
    }
    fn source(&self) -> io::Result<&Arc<dyn Namespace>> {
        self.source.as_ref().ok_or_else(|| unavailable(Unavailable::NoSuchItem))
    }
    fn native(&self) -> io::Result<Arc<dyn NativeFiles>> {
        self.native
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(io::ErrorKind::NotConnected.into())
    }
}
impl Namespace for LocalNamespace {
    fn native_lease(&self) -> io::Result<crate::content::namespace::NativeLease> {
        self.native()?.lease()
    }
    fn native_consumed(&self, nodes: &[crate::model::NodeId]) -> io::Result<()> {
        self.native()?.consumed(nodes)
    }

    fn native_file_metadata(&self) -> NativeFileMetadata {
        if self.settlement.is_empty() {
            self.source
                .as_ref()
                .map(|source| source.native_file_metadata())
                .unwrap_or_default()
        } else {
            self.settlement.native.clone()
        }
    }
    fn requires_native_files(&self) -> bool {
        true
    }
    fn bind_native(&self, native: Weak<dyn NativeFiles>) -> io::Result<()> {
        *self.native.lock().unwrap() = Some(native);
        Ok(())
    }
    fn native_list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> Option<io::Result<DirectoryPage>> {
        Some(self.list(node, cursor))
    }
    fn native_open(&self, node: NodeId, offset: u64) -> Option<io::Result<FileReader>> {
        Some(self.native().and_then(|native| native.open(node, offset)))
    }
    fn native_metadata(&self, node: NodeId) -> Option<io::Result<crate::FileMetadata>> {
        Some(self.native().and_then(|native| native.metadata(node)))
    }
    fn source_link(&self) -> Option<Descriptor> {
        self.source.as_ref()?.source_link()
    }
    fn origin(&self) -> Option<DeviceId> {
        self.settlement
            .offer
            .map(|offer| offer.clip.origin)
            .or_else(|| self.source.as_ref().and_then(|s| s.origin()))
    }
    fn roots(&self) -> Vec<FileNode> {
        let mut roots = self.source.as_ref().map_or_else(Vec::new, |source| source.roots());
        roots.extend(
            self.settlement
                .roots
                .iter()
                .flat_map(|root| root.entries.iter().filter(|entry| entry.parent.is_none()))
                .cloned(),
        );
        if !self.settlement.order.is_empty() {
            roots.sort_by_key(|root| self.settlement.order.iter().position(|id| *id == root.id));
        }
        roots
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        let Some(entry) = self.entry(node) else {
            return self.source()?.list(node, cursor);
        };
        if entry.kind != NodeKind::Directory {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        let children = self
            .settlement
            .roots
            .iter()
            .flat_map(|root| &root.entries)
            .filter(|entry| entry.parent == Some(node))
            .cloned()
            .collect::<Vec<_>>();
        crate::content::namespace::page(node, entry.version, &children, cursor)
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        match self.entry(node) {
            Some(entry) if entry.kind == NodeKind::File => Ok(entry.clone()),
            Some(_) => Err(unavailable(Unavailable::NoSuchItem)),
            None => self.source()?.select(node),
        }
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        if self.entry(node).is_some() {
            return Err(unavailable(Unavailable::Permission));
        }
        self.source()?.open(node, version, offset)
    }
}

/// Select a local manifest subtree. This never reads received bytes or the source.
pub fn subtree(root: NodeId, manifest: &[FileNode]) -> io::Result<Vec<FileNode>> {
    let mut pending = vec![(root, 0)];
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    while let Some((id, depth)) = pending.pop() {
        if depth >= 256 || !seen.insert(id) {
            return Err(unavailable(Unavailable::InvalidData));
        }
        let mut entry = manifest
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
            .ok_or_else(|| unavailable(Unavailable::InvalidData))?;
        pending.extend(
            manifest
                .iter()
                .filter(|entry| entry.parent == Some(id))
                .map(|entry| (entry.id, depth + 1)),
        );
        if id == root {
            entry.parent = None;
        }
        entries.push(entry);
    }
    Ok(entries)
}
// Uses the debug-only content fixture.
#[cfg(all(test, debug_assertions))]
mod tests;
