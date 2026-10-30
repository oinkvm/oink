//! Durable native namespace; does not retain transfer-cache leases.
use super::*;
use crate::received::{self, Basis, Item, Reader, Usage};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
};
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Node {
    pub path: PathBuf,
    pub life: Item,
    pub directory: bool,
    pub deleted: bool,
    /// Source Gone only disables source reads. Native deletion needs separate proof.
    #[serde(default)]
    pub withdrawn: bool,
    #[serde(default)]
    pub local_final: bool,
    #[serde(default)]
    pub renamed_locally: bool,
    pub handed_off: bool,
    pub length: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Source {
    pub descriptor: Option<crate::content::linked::Descriptor>,
    #[serde(default)]
    pub settlement: crate::content::linked::completed::FinalLocal,
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub failed: HashSet<crate::model::NodeId>,
    pub nodes: Vec<crate::model::FileNode>,
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub pending: HashSet<crate::model::NodeId>,
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub listed: HashSet<crate::model::NodeId>,
    pub base: PathBuf,
}
impl Source {
    fn refetchable(&self, index: usize) -> bool {
        let Some(descriptor) = self.descriptor.as_ref().filter(|link| !link.provisional) else {
            return false;
        };
        let Some(mut node) = self.nodes.get(index) else {
            return false;
        };
        for _ in 0..256 {
            if let Some(parent) = node.parent {
                let Some(ancestor) = self.nodes.iter().find(|entry| entry.id == parent) else {
                    return false;
                };
                node = ancestor;
            } else {
                return descriptor.roots.iter().any(|root| root.id == node.id)
                    && !descriptor.fulfilled.iter().any(|root| root.node == node.id)
                    && !self.failed.contains(&node.id);
            }
        }
        false
    }
    fn validate(&self) -> io::Result<()> {
        if !self.listed.is_subset(&self.pending)
            || self.pending.iter().any(|id| {
                !self
                    .nodes
                    .iter()
                    .any(|node| node.id == *id && node.kind == crate::model::NodeKind::Directory)
            })
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        self.settlement.validate(self.descriptor.as_ref())?;
        if self.descriptor.is_none() && self.settlement.is_empty() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if self.nodes.is_empty()
            || self.nodes.len() > crate::model::MAX_DISCOVERED_NODES
            || self.base.as_os_str().is_empty()
            || self.base.is_absolute()
            || self
                .base
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(())
    }
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Journal {
    pub deliveries: BTreeMap<u64, Vec<Node>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub moves: BTreeMap<u64, super::moved::State>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub consumed: BTreeMap<u64, HashSet<usize>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<u64, Source>,
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub cancelled: HashSet<u64>,
    #[serde(skip)]
    pub opens: HashMap<(u64, usize), i64>,
    /// Admission barrier held across the native unlink, without holding a mutex
    /// through an OS call which can synchronously dispatch provider callbacks.
    #[serde(skip)]
    pub cleanup_inflight: HashSet<(u64, usize)>,
    /// Confirmed unlink/NotFound, distinct from pre-unlink durable tombstones.
    /// Completed old paths leave the retry set without removing their journal.
    #[serde(skip)]
    pub cleanup_complete: HashSet<(u64, usize)>,
}
impl Journal {
    fn refetchable(&self, id: u64, index: usize) -> bool {
        !self.cancelled.contains(&id)
            && self
                .deliveries
                .get(&id)
                .and_then(|nodes| nodes.get(index))
                .is_some_and(|node| !node.local_final && !node.withdrawn && !node.deleted)
            && self.sources.get(&id).is_some_and(|source| source.refetchable(index))
    }
    pub(super) fn has_open_received_item(&self, id: u64) -> bool {
        // Windows 11 can deliver an open without a matching cloud close when
        // MOVE converts a child to an ordinary file outside the sync root.
        // Once that old item is gone, its balance cannot protect the empty old
        // parent. Keep balances for every remaining item (including directories);
        // deletion still requires an exclusive, identity-checked native handle.
        self.opens.iter().any(|((owner, index), count)| {
            *owner == id
                && *count != 0
                && self
                    .deliveries
                    .get(&id)
                    .and_then(|nodes| nodes.get(*index))
                    .is_none_or(|node| !node.deleted)
        })
    }
    pub fn load(root: &Path) -> io::Result<Self> {
        if let Ok(meta) = std::fs::symlink_metadata(root.with_extension("received"))
            && (!meta.is_file() || crate::cache::is_link(&meta) || meta.len() > 64 * 1024 * 1024)
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let value: serde_json::Value = match std::fs::read(root.with_extension("received")) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let legacy: HashMap<u64, _> = value["sources"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(id, source)| {
                Some((
                    id.parse().ok()?,
                    crate::content::linked::completed::legacy_pending_roots(&source["descriptor"]),
                ))
            })
            .collect();
        let mut state: Self = serde_json::from_value(value)?;
        for (id, source) in &state.sources {
            source.validate()?;
            if state
                .deliveries
                .get(id)
                .is_none_or(|nodes| nodes.len() != source.nodes.len())
            {
                return Err(io::ErrorKind::InvalidData.into());
            }
        }
        for (id, source) in &mut state.sources {
            if !state.cancelled.contains(id)
                && !state.moves.contains_key(id)
                && source
                    .descriptor
                    .as_ref()
                    .is_some_and(|descriptor| !descriptor.provisional)
                && let Some(legacy) = legacy.get(id)
            {
                for root in legacy {
                    if source.failed.contains(root) {
                        continue;
                    }
                    let entries = crate::content::linked::completed::subtree(*root, &source.nodes)?;
                    let received = entries.iter().all(|entry| {
                        let index = source.nodes.iter().position(|node| node.id == entry.id).unwrap();
                        let node = &state.deliveries[id][index];
                        !node.deleted
                            && !node.withdrawn
                            && if node.directory {
                                !source.pending.contains(&entry.id)
                            } else {
                                node.handed_off && entry.size == crate::model::ContentSize::Known(node.length)
                            }
                    });
                    if received {
                        // Build 72 already persisted byte handoff and directory
                        // completion. Convert before restart withdrawal; no CF
                        // residency/pin proof, body read or sender query.
                        source
                            .settlement
                            .recover_legacy_received(&mut source.descriptor, *root, &source.nodes)?;
                    }
                }
            }
            let roots = source.descriptor.as_ref().map_or_else(Vec::new, |descriptor| {
                descriptor.fulfilled.iter().map(|root| root.node).collect()
            });
            for root in roots {
                source.failed.insert(root);
                let entries = crate::content::linked::completed::subtree(root, &source.nodes)?;
                for (index, entry) in source.nodes.iter().enumerate() {
                    if entries.iter().any(|candidate| candidate.id == entry.id) {
                        state.deliveries.get_mut(id).ok_or(io::ErrorKind::InvalidData)?[index].withdrawn = true;
                    }
                }
            }
        }
        for nodes in state.deliveries.values_mut() {
            for node in nodes {
                if node.path.is_absolute()
                    || node
                        .path
                        .components()
                        .any(|c| !matches!(c, std::path::Component::Normal(_)))
                {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                node.life.resume_at(received::now());
            }
        }
        Ok(state)
    }
    fn merge_discovered(&mut self, id: u64, found: BTreeMap<usize, Node>) {
        let Some(last) = found.keys().last().copied() else {
            return;
        };
        let existing = self.deliveries.entry(id).or_default();
        let previous_len = existing.len();
        let first = found.values().next().unwrap();
        existing.resize_with(existing.len().max(last + 1), || Node {
            path: first.path.components().take(2).collect(),
            life: Item::new(received::now()),
            directory: true,
            deleted: true,
            withdrawn: false,
            local_final: false,
            renamed_locally: false,
            handed_off: false,
            length: 0,
        });
        for (index, node) in found {
            if index >= previous_len {
                existing[index] = node;
                continue;
            }
            let old = &mut existing[index];
            if old.deleted {
                continue;
            } // no late discovery can revive a tombstone
            old.path = node.path;
            old.directory = node.directory;
            old.length = node.length;
            old.life.materialized |= node.life.materialized;
            old.handed_off |= node.handed_off;
            old.life.resume_at(received::now());
        }
    }
    pub fn save(&self, root: &Path) -> io::Result<()> {
        for source in self.sources.values() {
            source.validate()?;
        }
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        let path = root.with_extension("received");
        // received_usage runs maintenance. Rewriting/invalidating an unchanged
        // journal would make every accounting query schedule another query.
        if std::fs::read(&path).is_ok_and(|previous| previous == bytes) {
            // A previous post-rename flush may have failed; identical contents
            // still need the durability barrier before reporting a commit.
            return std::fs::OpenOptions::new().write(true).open(&path)?.sync_all();
        }
        let result = oink_foundation::storage::atomic_write(&path, &bytes);
        // A post-rename durability error still committed new accounting state.
        // A rejected write did not; notifying it would loop failed usage scans.
        if result.is_ok() || std::fs::read(&path).is_ok_and(|current| current == bytes) {
            crate::cache::changed(root);
        }
        result
    }
}
impl Service {
    pub(super) fn cancel_received(&self, id: u64) {
        {
            let mut journal = self.received.lock().unwrap();
            journal.cancelled.insert(id);
            if let Err(error) = journal.save(&self.root) {
                tracing::warn!(%error,"cannot persist received cancellation");
            }
        }
        {
            let mut deliveries = self.deliveries.lock().unwrap();
            if let Some(delivery) = deliveries.get(&id) {
                for file in delivery.files() {
                    file.deleted.store(true, Ordering::Release);
                }
            }
            // Keep the reader counter discoverable until existing consumers
            // drain. Admission is already closed by the journal marker.
            if deliveries
                .get(&id)
                .is_none_or(|delivery| delivery.readers.load(Ordering::Acquire) == 0)
            {
                deliveries.remove(&id);
            }
        }
        self.received_sweep();
    }
    fn cleanup_cancelled(&self) {
        let (cancelled, deliveries, complete) = {
            let journal = self.received.lock().unwrap();
            (
                journal.cancelled.clone(),
                journal.deliveries.clone(),
                journal.cleanup_complete.clone(),
            )
        };
        for id in cancelled {
            if self
                .deliveries
                .lock()
                .unwrap()
                .get(&id)
                .is_some_and(|delivery| delivery.readers.load(Ordering::Acquire) != 0)
            {
                continue;
            }
            let Some(nodes) = deliveries.get(&id) else {
                continue;
            };
            // Only our exact native identities are removed. Exclusive handles
            // defer busy files until the next maintenance pass; no tree unlink.
            for (index, node) in nodes
                .iter()
                .enumerate()
                .filter(|(index, node)| !node.directory && !complete.contains(&(id, *index)))
            {
                let path = self.root.join(&node.path);
                let removed = match std::fs::symlink_metadata(&path) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => true,
                    Ok(_) => cleanup_finished(&exclusive_delete_handle(&self.api, &path, |handle| {
                        !node.local_final
                            && !node.renamed_locally
                            && self
                                .handle_identity(handle, 0)
                                .is_ok_and(|(owner, slot, _)| owner == id && slot == index)
                            && self.clean_cancelled_file(handle, node.length).unwrap_or(false)
                    })),
                    _ => false,
                };
                if removed {
                    self.cleanup_done(id, index, &path);
                }
            }
            let mut dirs: Vec<_> = nodes
                .iter()
                .enumerate()
                .filter(|(index, node)| node.directory && !complete.contains(&(id, *index)))
                .collect();
            dirs.sort_by_key(|(_, n)| std::cmp::Reverse(n.path.components().count()));
            for (index, node) in dirs {
                let path = self.root.join(&node.path);
                let result = exclusive_delete_handle(&self.api, &path, |handle| {
                    self.handle_identity(handle, 0)
                        .is_ok_and(|(owner, slot, _)| owner == id && slot == index)
                });
                if cleanup_finished(&result) {
                    self.cleanup_done(id, index, &path);
                }
            }
        }
    }
    fn clean_cancelled_file(&self, handle: HANDLE, expected_length: u64) -> io::Result<bool> {
        let mut buffer = [0u64; 1024];
        let mut written = 0;
        // SAFETY: aligned output and the protected exclusive deletion handle.
        hr(unsafe {
            (self.api.info)(
                handle,
                CF_PLACEHOLDER_INFO_STANDARD,
                buffer.as_mut_ptr().cast(),
                std::mem::size_of_val(&buffer) as u32,
                &mut written,
            )
        })?;
        // SAFETY: successful native query initialized standard info.
        let info = unsafe { &*buffer.as_ptr().cast::<CF_PLACEHOLDER_STANDARD_INFO>() };
        let mut length = 0i64;
        // SAFETY: same protected file and fixed-size output.
        if unsafe { GetFileSizeEx(handle, &mut length) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.InSyncState == CF_IN_SYNC_STATE_IN_SYNC
            && info.ModifiedDataSize == 0
            && length >= 0
            && length as u64 == expected_length)
    }
    pub(super) fn native_complete(&self, id: u64, index: usize, length: u64) {
        let _mutation = crate::cache::Mutation(&self.root);
        let mut journal = self.received.lock().unwrap();
        if let Some(node) = journal.deliveries.get_mut(&id).and_then(|v| v.get_mut(index)) {
            node.handed_off = true;
            node.length = length;
        }
        if journal.save(&self.root).is_err() {
            for node in journal.deliveries.values_mut().flatten() {
                node.life.resume_at(received::now());
            }
        }
        let refetchable = journal.refetchable(id, index);
        let path = journal
            .deliveries
            .get(&id)
            .and_then(|nodes| nodes.get(index))
            .map(|node| self.root.join(&node.path));
        drop(journal);
        // Best-effort policy publication only. Receive completion is independent
        // of Cloud Filter residency/pin state and never opens a second reader.
        let delivery = self.deliveries.lock().unwrap().get(&id).cloned();
        if let Some(delivery) = delivery
            && let Err(error) = delivery.complete_received()
        {
            tracing::warn!(%error, "cannot persist one-shot completion");
        }
        if let Some(path) = path {
            self.request_policy(&path, self.api.received_pin_state(refetchable));
        }
    }
    fn request_policy(&self, path: &Path, policy: i32) {
        let result = (|| -> io::Result<()> {
            let file = std::fs::OpenOptions::new()
                .access_mode(0x40000)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
                .open(path)?;
            // SAFETY: synchronous policy request, owned handle with WRITE_DAC.
            hr(unsafe { (self.api.pin)(file.as_raw_handle(), policy, CF_SET_PIN_FLAG_NONE, std::ptr::null_mut()) })
        })();
        if let Err(error) = result {
            tracing::debug!(%error, "received keep-local request failed");
        }
    }

    pub(super) fn received_sweep(&self) {
        self.cleanup_cancelled();
        self.cleanup_moved();
        let snapshot = self.received.lock().unwrap().deliveries.clone();
        for (id, nodes) in snapshot {
            for (index, node) in nodes.iter().enumerate() {
                if node.deleted || node.directory {
                    continue;
                }
                let path = self.root.join(&node.path);
                let identity = self.disk_identity(&path);
                let owned_incomplete = identity
                    .as_ref()
                    .is_ok_and(|(owner, slot, full)| *owner == id && *slot == index && !full);
                let missing = std::fs::symlink_metadata(&path).is_err_and(|e| e.kind() == io::ErrorKind::NotFound);
                let owned = identity
                    .as_ref()
                    .is_ok_and(|(owner, slot, _)| *owner == id && *slot == index);
                let refetchable = self.received.lock().unwrap().refetchable(id, index);
                let residency = owned.then(|| self.received_residency(&path, node.length)).transpose();
                let complete = matches!(residency, Ok(Some(true)));
                let live = self.deliveries.lock().unwrap().get(&id).cloned();
                let mut journal = self.received.lock().unwrap();
                let busy = journal.opens.get(&(id, index)).copied().unwrap_or(0) != 0
                    || live.as_ref().is_some_and(|d| d.readers.load(Ordering::Acquire) > 0);
                let linked = journal.sources.contains_key(&id);
                let awaiting_publication = journal.sources.get(&id).is_some_and(|source| {
                    source
                        .nodes
                        .get(index)
                        .and_then(|node| node.parent)
                        .is_some_and(|parent| source.pending.contains(&parent))
                });
                if missing && awaiting_publication {
                    continue;
                }

                let Some(current) = journal.deliveries.get_mut(&id).and_then(|v| v.get_mut(index)) else {
                    continue;
                };
                if current.path != node.path {
                    continue;
                }
                if let Ok(Some(full)) = residency {
                    // Linked items can return to dataless. For source-less
                    // items this remains evidence of a previous complete copy;
                    // a later partial/uncertain observation cannot make it stale.
                    current.life.materialized = full || !refetchable && current.life.materialized;
                }

                // Recovery only removes provably never-complete placeholders. A
                // successful handoff or prior materialization is protected on errors.
                let stale =
                    !linked && !current.life.materialized && !current.handed_off && live.is_none() && owned_incomplete;
                if missing {
                    current.deleted = true;
                    continue;
                }
                if !owned {
                    continue;
                }
                let reason = current
                    .life
                    .cleanup(received::now(), received::setting(), Basis::LastUsed, busy);
                if !busy && complete && reason.is_some() && refetchable {
                    drop(journal);
                    if self.api.can_evict_received()
                        && let Err(error) = self.dehydrate_received(id, index, node, live.as_ref())
                    {
                        tracing::debug!(%error, "received dehydration deferred");
                    }
                    // Legacy platforms keep linked content at expiry. Never
                    // fall through to source-less deletion or attempt eviction.
                    continue;
                }
                if !busy && (stale || complete && reason.is_some()) {
                    // OS sharing denies deletion while an incompatible data handle
                    // exists; a failed deletion is retried, never counted as freed.
                    drop(journal);
                    let allocated = std::fs::symlink_metadata(&path)
                        .and_then(|m| crate::cache::allocated(&path, &m))
                        .unwrap_or(0);
                    let mut reserved = false;
                    let result = exclusive_delete_handle(&self.api, &path, |handle| {
                        if !self
                            .handle_identity(handle, 0)
                            .is_ok_and(|(owner, slot, _)| owner == id && slot == index)
                        {
                            return false;
                        }
                        // A missing live owner only permits stale-placeholder
                        // retirement when the native data still proves clean.
                        // Explicit age policy can separately retire local edits.
                        let clean_unfinished =
                            !stale || self.clean_cancelled_file(handle, node.length).unwrap_or(false);
                        let mut journal = self.received.lock().unwrap();
                        let Some(current) = journal.deliveries.get(&id).and_then(|v| v.get(index)) else {
                            return false;
                        };
                        let busy = journal.opens.get(&(id, index)).copied().unwrap_or(0) != 0
                            || live
                                .as_ref()
                                .is_some_and(|delivery| delivery.readers.load(Ordering::Acquire) != 0);
                        let eligible = current.path == node.path
                            && !current.deleted
                            && !busy
                            && (stale
                                && clean_unfinished
                                && !current.local_final
                                && !current.renamed_locally
                                && !current.life.materialized
                                && !current.handed_off
                                || current
                                    .life
                                    .cleanup(received::now(), received::setting(), Basis::LastUsed, false)
                                    .is_some());
                        reserved = eligible && journal.cleanup_inflight.insert((id, index));
                        reserved
                    });
                    let mut journal = self.received.lock().unwrap();
                    if reserved {
                        journal.cleanup_inflight.remove(&(id, index));
                    }
                    let Some(current) = journal.deliveries.get_mut(&id).and_then(|v| v.get_mut(index)) else {
                        continue;
                    };
                    match result {
                        Ok(()) => {
                            current.deleted = true;
                            tracing::info!(
                                cleanup_reason = if stale {
                                    "stale"
                                } else {
                                    match reason.unwrap() {
                                        received::Reason::Age7 => "age_7d",
                                        received::Reason::Age30 => "age_30d",
                                    }
                                },
                                count = 1_u64,
                                removed_bytes = allocated,
                                "received file removed"
                            );
                        }
                        Err(e) if e.kind() == io::ErrorKind::NotFound => current.deleted = true,
                        Err(_) => {
                            tracing::debug!(
                                cleanup_reason = if stale { "stale" } else { "policy" },
                                outcome = "deferred",
                                count = 1_u64,
                                "received cleanup deferred"
                            );
                        }
                    }
                }
            }
        }
        self.prune_empty_received();
        let journal = self.received.lock().unwrap();
        if journal.save(&self.root).is_err() {
            tracing::warn!(reason = "journal_write", "received-file state not saved");
        }
    }
    fn received_residency(&self, path: &Path, length: u64) -> io::Result<bool> {
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        self.handle_identity(file.as_raw_handle(), length)
            .map(|(_, _, full)| full)
    }
    // Restore applies the catalog's chosen policy once. Reading native dirty
    // state detects independent edits; it never reads pin state or checks that
    // the policy request was accepted. No pin confirmation/retry worker.
    fn restore_received_policy(&self, path: &Path, node: &Node, refetchable: bool) {
        let policy = if refetchable && self.api.can_evict_received() {
            let clean = std::fs::OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(path)
                .and_then(|file| self.clean_cancelled_file(file.as_raw_handle(), node.length));
            if clean.unwrap_or(false) {
                CF_PIN_STATE_UNSPECIFIED
            } else {
                CF_PIN_STATE_PINNED
            }
        } else {
            CF_PIN_STATE_PINNED
        };
        self.request_policy(path, policy);
    }
    fn dehydrate_received(&self, id: u64, index: usize, node: &Node, live: Option<&Arc<Delivery>>) -> io::Result<()> {
        if !self.api.can_evict_received() {
            return Err(io::ErrorKind::Unsupported.into());
        }
        let protected = protection::Protected::open(
            self.api.clone(),
            &self.root.join(&node.path),
            CF_OPEN_FILE_FLAG_EXCLUSIVE | CF_OPEN_FILE_FLAG_WRITE_ACCESS,
        )?;
        if !self
            .handle_identity(protected.handle, node.length)
            .is_ok_and(|(owner, slot, full)| owner == id && slot == index && full)
            || !self.clean_cancelled_file(protected.handle, node.length)?
        {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        {
            let mut journal = self.received.lock().unwrap();
            let eligible = journal.refetchable(id, index)
                && journal.opens.get(&(id, index)).copied().unwrap_or(0) == 0
                && live.is_none_or(|delivery| delivery.readers.load(Ordering::Acquire) == 0)
                && journal
                    .deliveries
                    .get(&id)
                    .and_then(|nodes| nodes.get(index))
                    .is_some_and(|current| {
                        current.path == node.path
                            && current
                                .life
                                .cleanup(received::now(), received::setting(), Basis::LastUsed, false)
                                .is_some()
                    });
            if !eligible || !journal.cleanup_inflight.insert((id, index)) {
                return Err(io::ErrorKind::WouldBlock.into());
            }
        }
        // Exclusive RH oplock and the reader admission barrier cover the same
        // clean identity throughout. Never force dehydration of open/dirty files.
        // Policy was published at receive/restore; do not read it back, unpin a
        // newly user-pinned file, or retry a failed policy request here.
        // SAFETY: synchronous dehydration on our referenced exclusive handle.
        let result =
            hr(unsafe { (self.api.dehydrate)(protected.handle, 0, -1, CF_DEHYDRATE_FLAG_NONE, std::ptr::null_mut()) });
        let mut journal = self.received.lock().unwrap();
        journal.cleanup_inflight.remove(&(id, index));
        if result.is_ok() {
            if let Some(current) = journal.deliveries.get_mut(&id).and_then(|nodes| nodes.get_mut(index)) {
                current.life.materialized = false;
            }
            crate::cache::changed(&self.root);
        }
        result
    }
    pub(super) fn received_observe(&self, id: u64, index: usize, reader: Reader, open: bool, deleted: bool) {
        let mut journal = self.received.lock().unwrap();
        // A close which deletes the placeholder still balances its open. The
        // separate DELETE_COMPLETION callback does not represent a close.
        let balance = journal.opens.entry((id, index)).or_default();
        *balance += if open { 1 } else { -1 };
        if let Some(nodes) = journal.deliveries.get_mut(&id)
            && let Some(node) = nodes.get_mut(index)
        {
            node.life.observe(reader, received::now());
            if deleted {
                let path = node.path.clone();
                for node in &mut *nodes {
                    if node.path.starts_with(&path) {
                        node.deleted = true;
                    }
                }
            }
        }
        // No disk, signature verification or native calls on cfapi threads.
        // Reconnect advances last-used conservatively for every recovered item.
        tracing::debug!(
            reader = match reader {
                Reader::System => 0_u8,
                Reader::App => 2,
                Reader::Unknown => 3,
            },
            count = 1_u64,
            "received reader observed"
        );
        crate::cache::changed(&self.root);
    }
}
pub(crate) fn received_usage() -> io::Result<Usage> {
    let service = CURRENT.lock().unwrap().upgrade().ok_or(io::ErrorKind::NotConnected)?;
    service.received_sweep();
    let deliveries = service.received.lock().unwrap().deliveries.clone();
    let mut usage = Usage {
        time_basis: Basis::LastUsed,
        cleanup: received::setting(),
        ..Default::default()
    };
    for (id, nodes) in &deliveries {
        for (index, node) in nodes.iter().enumerate().filter(|(_, n)| !n.deleted && !n.directory) {
            let path = service.root.join(&node.path);
            let identity = service.disk_identity(&path);
            if !identity
                .as_ref()
                .is_ok_and(|(owner, slot, _)| owner == id && *slot == index)
            {
                usage.incomplete = true;
                continue;
            }
            if !node.life.materialized || !identity.as_ref().is_ok_and(|(_, _, full)| *full) {
                usage.incomplete |= identity.is_ok_and(|(_, _, full)| full);
                continue;
            }
            match std::fs::symlink_metadata(&path).and_then(|m| crate::cache::allocated(&path, &m)) {
                Ok(size) => usage.materialized_bytes += size,
                Err(_) => usage.incomplete = true,
            }
        }
    }
    tracing::debug!(
        materialized_bytes = usage.materialized_bytes,
        incomplete = usage.incomplete,
        "received usage"
    );
    Ok(usage)
}
pub(crate) fn open_received() -> io::Result<()> {
    let service = CURRENT.lock().unwrap().upgrade().ok_or(io::ErrorKind::NotConnected)?;
    std::process::Command::new("explorer.exe")
        .arg(&service.root)
        .spawn()
        .map(|_| ())
}
/// Fixed protected OS locations only; unknown helpers are retained, never
/// mistaken for Explorer. Preverification happens off native callback threads.
pub(super) fn reader_classes() -> HashMap<String, Reader> {
    let mut classes = HashMap::new();
    let windows = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let mut paths = Vec::new();
    if let Some(programs) = std::env::var_os("ProgramFiles") {
        for exe in ["MsMpEng.exe", "MpDefenderCoreService.exe"] {
            paths.push((
                PathBuf::from(&programs).join("Windows Defender").join(exe),
                Reader::System,
            ));
        }
    }
    if let Some(data) = std::env::var_os("ProgramData")
        && let Ok(versions) = std::fs::read_dir(PathBuf::from(data).join(r"Microsoft\Windows Defender\Platform"))
    {
        for entry in versions.flatten() {
            if entry.metadata().is_ok_and(|m| m.is_dir() && !crate::cache::is_link(&m)) {
                for exe in ["MsMpEng.exe", "MpDefenderCoreService.exe"] {
                    paths.push((entry.path().join(exe), Reader::System));
                }
            }
        }
    }
    for name in ["SearchIndexer.exe", "SearchProtocolHost.exe", "SearchFilterHost.exe"] {
        paths.push((windows.join("System32").join(name), Reader::System));
    }
    for path in paths {
        if trusted(&path.0) {
            if let Some(native) = nt_path(&path.0) {
                classes.insert(native.to_lowercase(), path.1);
            }
            classes.insert(path.0.to_string_lossy().to_lowercase(), path.1);
        }
    }
    classes
}
fn nt_path(path: &Path) -> Option<String> {
    let value = path.to_str()?;
    let drive = value.get(..2)?;
    let mut target = vec![0u16; 32768];
    // SAFETY: QueryDosDevice writes into the supplied live UTF-16 buffer.
    let length = unsafe { QueryDosDeviceW(wide(drive).as_ptr(), target.as_mut_ptr(), target.len() as u32) };
    if length == 0 {
        return None;
    }
    let end = target.iter().position(|c| *c == 0)?;
    Some(format!("{}{}", String::from_utf16_lossy(&target[..end]), &value[2..]))
}
fn trusted(path: &Path) -> bool {
    use windows_sys::Win32::Security::WinTrust::*;
    if std::fs::symlink_metadata(path).map_or(true, |m| crate::cache::is_link(&m)) {
        return false;
    }
    let name = wide(path);
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: name.as_ptr(),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: initialized trust structures and path live throughout verification;
    // UI/network retrieval are disabled and the trust state is always closed.
    unsafe {
        let result = WinVerifyTrust(
            std::ptr::null_mut(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        );
        let microsoft = if result == 0 {
            let provider = WTHelperProvDataFromStateData(data.hWVTStateData);
            if provider.is_null() {
                false
            } else {
                let signer = WTHelperGetProvSignerFromChain(provider, 0, 0, 0);
                if signer.is_null() || (*signer).csCertChain == 0 || (*signer).pasCertChain.is_null() {
                    false
                } else {
                    let certificate = (*(*signer).pasCertChain).pCert;
                    let mut name = [0u16; 256];
                    let count = windows_sys::Win32::Security::Cryptography::CertGetNameStringW(
                        certificate,
                        windows_sys::Win32::Security::Cryptography::CERT_NAME_SIMPLE_DISPLAY_TYPE,
                        0,
                        std::ptr::null(),
                        name.as_mut_ptr(),
                        name.len() as u32,
                    );
                    count > 0
                        && count as usize <= name.len()
                        && matches!(
                            String::from_utf16_lossy(&name[..count.saturating_sub(1) as usize]).as_str(),
                            "Microsoft Windows" | "Microsoft Corporation" | "Microsoft Windows Publisher"
                        )
                }
            }
        } else {
            false
        };
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        WinVerifyTrust(
            std::ptr::null_mut(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        );
        microsoft
    }
}

/// Windows native names can differ from the configured root only by case.
/// Preserve the actual tail; textual strip_prefix would deny legitimate edits.
pub(super) fn relative_path(root: &Path, path: &Path) -> Option<PathBuf> {
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
    let mut tail = path.components();
    for component in root.components() {
        let other = tail.next()?;
        let a = wide(component.as_os_str());
        let b = wide(other.as_os_str());
        // SAFETY: counted UTF-16 components remain live during ordinal comparison.
        if unsafe { CompareStringOrdinal(a.as_ptr(), (a.len() - 1) as i32, b.as_ptr(), (b.len() - 1) as i32, 1) }
            != CSTR_EQUAL
        {
            return None;
        }
    }
    Some(tail.collect())
}
/// Native callback paths are volume-rooted, not DOS absolute paths.
unsafe fn callback_path(info: &CF_CALLBACK_INFO, pointer: *const u16) -> Option<PathBuf> {
    unsafe fn string(pointer: *const u16) -> Option<String> {
        if pointer.is_null() {
            return None;
        }
        let mut n = 0;
        // SAFETY: the callback owns a terminated UTF-16 string for this call.
        unsafe {
            while n < 32768 && *pointer.add(n) != 0 {
                n += 1;
            }
            if n == 32768 {
                None
            } else {
                Some(String::from_utf16_lossy(std::slice::from_raw_parts(pointer, n)))
            }
        }
    }
    // SAFETY: cfapi supplies these two live strings.
    unsafe {
        let p = string(pointer)?;
        if p.starts_with(r"\Device\") {
            let native = nt_path(Path::new(&format!("{}\\", string(info.VolumeDosName)?)))?;
            return p
                .strip_prefix(native.trim_end_matches('\\'))
                .map(|tail| PathBuf::from(format!("{}{tail}", string(info.VolumeDosName).unwrap_or_default())));
        }
        Some(PathBuf::from(format!("{}{}", string(info.VolumeDosName)?, p)))
    }
}
fn rename_allowed(root: &Path, old: &Path, new: &Path, flags: CF_CALLBACK_RENAME_FLAGS) -> bool {
    let Some(old) = relative_path(root, old) else {
        return false;
    };
    // Source/device and batch directories are provider bookkeeping, not received
    // items. Never let a shell move that bookkeeping or reparent across batches.
    if old.components().count() <= 2 || !old.components().all(|c| matches!(c, std::path::Component::Normal(_))) {
        return false;
    }
    match relative_path(root, new) {
        Some(new) => {
            new.components().count() > 2
                && old.components().take(2).eq(new.components().take(2))
                && new.components().all(|c| matches!(c, std::path::Component::Normal(_)))
        }
        // This is the destination application's move of its received copy.
        // CfAPI still owns hydration/move execution. A successful completion
        // outside the root never makes the destination an Oink-managed path:
        // the journal retains only the old identity for absent-item retirement.
        None => {
            new.is_absolute()
                && flags & CF_CALLBACK_RENAME_FLAG_SOURCE_IN_SCOPE != 0
                && flags & CF_CALLBACK_RENAME_FLAG_TARGET_IN_SCOPE == 0
        }
    }
}
pub(super) unsafe extern "system" fn renaming(info: *const CF_CALLBACK_INFO, params: *const CF_CALLBACK_PARAMETERS) {
    // SAFETY: cfapi owns callback/context/path pointers for this call.
    unsafe {
        let i = &*info;
        let Some(service) = (&*i.CallbackContext.cast::<Weak<Service>>()).upgrade() else {
            return;
        };
        let old = callback_path(i, i.NormalizedPath);
        let new = callback_path(i, (*params).Anonymous.Rename.TargetPath);
        let flags = (*params).Anonymous.Rename.Flags;
        let allowed = old
            .as_ref()
            .zip(new.as_ref())
            .is_some_and(|(old, new)| rename_allowed(&service.root, old, new, flags));
        let connection = i.ConnectionKey;
        let transfer = i.TransferKey;
        let request = i.RequestKey;
        tracing::info!(target: "oink_file_diagnostics", file_stage = 60_u8,
            file_request = request, file_flags = flags, allowed,
            "cloud file rename requested");
        if allowed && flags & CF_CALLBACK_RENAME_FLAG_TARGET_IN_SCOPE == 0 {
            let deliveries: Vec<_> = service.deliveries.lock().unwrap().values().cloned().collect();
            let active = deliveries.into_iter().find_map(|delivery| {
                if !delivery.downloading.load(Ordering::Acquire) {
                    return None;
                }
                let index = delivery.native_paths.lock().unwrap().iter().position(|path| {
                    old.as_deref()
                        .is_some_and(|old| relative_path(path, old).is_some_and(|tail| tail.as_os_str().is_empty()))
                })?;
                Some((delivery, index))
            });
            if let Some((delivery, index)) = active {
                // The rename's live transfer key also admits directory population.
                // Publish it before joining the download: CfCreatePlaceholders
                // itself waits behind this still-unacknowledged rename.
                let population = delivery
                    .file(index)
                    .entry
                    .is_dir
                    .then(|| population::Pending::register(&delivery, index, transfer, request, None));
                let provider = service.clone();
                // MOVE must not convert a directory's children into ordinary
                // files while the active whole-drop download is still flushing
                // and recording them. Join that same download on a separate
                // thread; blocking a fetch worker would starve its own hydration.
                // Ordinary moves without an active download keep platform behavior.
                if std::thread::Builder::new()
                    .name("oink-cloud-move".into())
                    .spawn(move || {
                        let current = provider.clone();
                        let id = delivery.id;
                        let check: crate::content::ReaderGuard = Arc::new(move || {
                            if current.stopping.load(Ordering::Acquire)
                                || current.received.lock().unwrap().cancelled.contains(&id)
                            {
                                Err(unavailable())
                            } else {
                                Ok(())
                            }
                        });
                        let result = crate::content::namespace::NativeFiles::download(&*delivery, &check, &|_, _| {});
                        drop(population);
                        acknowledge_rename(
                            &provider.api,
                            connection,
                            transfer,
                            request,
                            if result.is_ok() {
                                STATUS_SUCCESS
                            } else {
                                STATUS_CLOUD_FILE_UNSUCCESSFUL
                            },
                        );
                    })
                    .is_ok()
                {
                    return;
                }
                acknowledge_rename(
                    &service.api,
                    connection,
                    transfer,
                    request,
                    STATUS_CLOUD_FILE_UNSUCCESSFUL,
                );
                return;
            }
        }
        acknowledge_rename(
            &service.api,
            connection,
            transfer,
            request,
            if allowed {
                STATUS_SUCCESS
            } else {
                STATUS_CLOUD_FILE_ACCESS_DENIED
            },
        );
    }
}
fn acknowledge_rename(api: &Api, connection: CF_CONNECTION_KEY, transfer: i64, request: i64, status: NTSTATUS) {
    let mut op = operation(connection, transfer, request);
    op.Type = CF_OPERATION_TYPE_ACK_RENAME;
    let mut p = CF_OPERATION_PARAMETERS {
        ParamSize: (offset_of!(CF_OPERATION_PARAMETERS, Anonymous) + size_of::<CF_OPERATION_PARAMETERS_0_6>()) as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            AckRename: CF_OPERATION_PARAMETERS_0_6 {
                Flags: CF_OPERATION_ACK_RENAME_FLAG_NONE,
                CompletionStatus: status,
            },
        },
    };
    // SAFETY: all scalar keys and acknowledgement buffers live through execute.
    let result = unsafe { (api.execute)(&op, &mut p) };
    tracing::info!(target: "oink_file_diagnostics", file_stage = 61_u8,
        file_request = request, file_status = status as u32, file_code = result as u32,
        "cloud file rename acknowledged");
}
pub(super) unsafe extern "system" fn renamed(info: *const CF_CALLBACK_INFO, params: *const CF_CALLBACK_PARAMETERS) {
    // SAFETY: both native paths are valid for this completion.
    unsafe {
        let i = &*info;
        let Some(service) = (&*i.CallbackContext.cast::<Weak<Service>>()).upgrade() else {
            return;
        };
        let Some((old, new)) =
            callback_path(i, (*params).Anonymous.RenameCompletion.SourcePath).zip(callback_path(i, i.NormalizedPath))
        else {
            return;
        };
        let Some(old) = relative_path(&service.root, &old) else {
            return;
        };
        let Some(new) = relative_path(&service.root, &new) else {
            return;
        };
        let mut journal = service.received.lock().unwrap();
        for node in journal.deliveries.values_mut().flatten() {
            if let Ok(tail) = node.path.strip_prefix(&old) {
                node.path = new.join(tail);
                node.renamed_locally = true;
                node.life.observe(Reader::Unknown, received::now());
            }
        }
        for source in journal.sources.values_mut() {
            if let Ok(tail) = source.base.strip_prefix(&old) {
                source.base = new.join(tail);
            }
        }
        drop(journal);
        for delivery in service.deliveries.lock().unwrap().values() {
            for path in delivery.native_paths.lock().unwrap().iter_mut() {
                if let Ok(tail) = path.strip_prefix(service.root.join(&old)) {
                    *path = service.root.join(&new).join(tail);
                }
            }
        }
        crate::cache::changed(&service.root);
    }
}

pub(super) fn cleanup_finished(result: &io::Result<()>) -> bool {
    result.is_ok()
        || result
            .as_ref()
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

pub(super) fn exclusive_delete_handle(
    api: &Arc<Api>,
    path: &Path,
    eligible: impl FnOnce(HANDLE) -> bool,
) -> io::Result<()> {
    // An ordinary share-none handle does not exclude a writable mapping left
    // after its creator closed. RH admission plus a protected reference does.
    let file = protection::Protected::open(
        api.clone(),
        path,
        CF_OPEN_FILE_FLAG_EXCLUSIVE | CF_OPEN_FILE_FLAG_DELETE_ACCESS | CF_OPEN_FILE_FLAG_WRITE_ACCESS,
    )?;
    if !eligible(file.handle) {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    file.delete()?;
    crate::cache::changed(path);
    Ok(())
}

impl Service {
    pub(super) fn create_container(&self, path: &Path, id: u64) -> io::Result<()> {
        let _mutation = crate::cache::Mutation(&self.root);
        let name = wide(path.file_name().ok_or(io::ErrorKind::InvalidInput)?);
        let mut identity = [0u8; 16];
        identity[..8].copy_from_slice(&id.to_le_bytes());
        identity[8..].copy_from_slice(&u64::MAX.to_le_bytes());
        let mut info = CF_PLACEHOLDER_CREATE_INFO {
            RelativeFileName: name.as_ptr(),
            FsMetadata: CF_FS_METADATA {
                BasicInfo: FILE_BASIC_INFO {
                    FileAttributes: FILE_ATTRIBUTE_DIRECTORY,
                    ..Default::default()
                },
                FileSize: 0,
            },
            FileIdentity: identity.as_ptr().cast(),
            FileIdentityLength: 16,
            Flags: CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC | CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION,
            ..Default::default()
        };
        let mut count = 0;
        // SAFETY: filename, identity and the single metadata entry live through create.
        hr(unsafe {
            (self.api.create)(
                wide(path.parent().ok_or(io::ErrorKind::InvalidInput)?).as_ptr(),
                &mut info,
                1,
                CF_CREATE_FLAG_STOP_ON_ERROR,
                &mut count,
            )
        })?;
        hr(info.Result)?;
        if count != 1 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(())
    }
    pub(super) fn owned_container(&self, path: &Path, id: u64) -> io::Result<bool> {
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let mut buffer = [0u64; 1024];
        let mut returned = 0;
        // SAFETY: an aligned output buffer covers the standard-info header and identity.
        if unsafe {
            (self.api.info)(
                file.as_raw_handle(),
                CF_PLACEHOLDER_INFO_STANDARD,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
                &mut returned,
            )
        } < 0
        {
            return Ok(false);
        }
        // SAFETY: the successful standard-info call initialized these fields.
        let info = unsafe { &*buffer.as_ptr().cast::<CF_PLACEHOLDER_STANDARD_INFO>() };
        if info.FileIdentityLength != 16 {
            return Ok(false);
        }
        // SAFETY: info reports a 16-byte identity inside our larger buffer.
        let identity = unsafe { std::slice::from_raw_parts(info.FileIdentity.as_ptr(), 16) };
        Ok(identity[..8] == id.to_le_bytes() && identity[8..] == u64::MAX.to_le_bytes())
    }
}
// Enabling OS auto-dehydration must not make a one-shot/local edit evictable
// when its best-effort PINNED publication failed. The native in-sync/open-file
// checks still apply after this admission decision; no pin proof gates F₁ EOF.
pub(super) unsafe extern "system" fn dehydrating(info: *const CF_CALLBACK_INFO, _: *const CF_CALLBACK_PARAMETERS) {
    // SAFETY: cfapi owns the context and bounded identity during this callback.
    unsafe {
        let info = &*info;
        let Some(service) = (&*info.CallbackContext.cast::<Weak<Service>>()).upgrade() else {
            return;
        };
        let allowed = if info.FileIdentityLength == 16 && !info.FileIdentity.is_null() {
            let bytes = std::slice::from_raw_parts(info.FileIdentity.cast::<u8>(), 16);
            let id = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            let index = u64::from_le_bytes(bytes[8..].try_into().unwrap()) as usize;
            let live = service.deliveries.lock().unwrap().get(&id).cloned();
            let journal = service.received.lock().unwrap();
            let refetchable = journal.refetchable(id, index);
            let balance = journal.opens.get(&(id, index)).copied().unwrap_or(0);
            let readers = live.map_or(0, |delivery| delivery.readers.load(Ordering::Acquire));
            tracing::debug!(refetchable, balance, readers, "received OS dehydration admission");
            // Windows 11's external CfDehydratePlaceholder caller opens its
            // own protected handle first (private probe 2026-10-29: balance=1,
            // readers=0). Counting that handle here rejects every OS Free up
            // space request. CfDehydrate's required exclusive oplock excludes
            // actual file users; our native leases additionally cover streams
            // which have not opened a handle yet. Age cleanup still checks the
            // open balance before obtaining its own exclusive handle.
            service.api.can_evict_received() && refetchable && readers == 0
        } else {
            false
        };
        let mut op = operation(info.ConnectionKey, info.TransferKey, info.RequestKey);
        op.Type = CF_OPERATION_TYPE_ACK_DEHYDRATE;
        let mut params = CF_OPERATION_PARAMETERS {
            ParamSize: (offset_of!(CF_OPERATION_PARAMETERS, Anonymous) + size_of::<CF_OPERATION_PARAMETERS_0_5>())
                as u32,
            Anonymous: CF_OPERATION_PARAMETERS_0 {
                AckDehydrate: CF_OPERATION_PARAMETERS_0_5 {
                    Flags: CF_OPERATION_ACK_DEHYDRATE_FLAG_NONE,
                    CompletionStatus: if allowed {
                        STATUS_SUCCESS
                    } else {
                        STATUS_CLOUD_FILE_ACCESS_DENIED
                    },
                    FileIdentity: std::ptr::null(),
                    FileIdentityLength: 0,
                },
            },
        };
        let _ = (service.api.execute)(&op, &mut params);
    }
}
pub(super) unsafe fn deleted_path(info: *const CF_CALLBACK_INFO) {
    // SAFETY: all pointers refer to this cfapi notification.
    unsafe {
        let info = &*info;
        let Some(service) = (&*info.CallbackContext.cast::<Weak<Service>>()).upgrade() else {
            return;
        };
        let Some(path) = callback_path(info, info.NormalizedPath) else {
            return;
        };
        let Some(relative) = relative_path(&service.root, &path) else {
            return;
        };
        let mut changed = Vec::new();
        {
            let mut journal = service.received.lock().unwrap();
            for (id, nodes) in &mut journal.deliveries {
                for (index, node) in nodes.iter_mut().enumerate() {
                    if node.path.starts_with(&relative) {
                        node.deleted = true;
                        changed.push((*id, index));
                    }
                }
            }
        }
        let deliveries = service.deliveries.lock().unwrap();
        for (id, index) in changed {
            if let Some(delivery) = deliveries.get(&id)
                && let Some(file) = delivery.get_file(index)
            {
                file.deleted.store(true, Ordering::Release);
            }
        }
        tracing::info!(
            cleanup_reason = "user_delete",
            count = 1_u64,
            "received deletion observed"
        );
        crate::cache::changed(&service.root);
    }
}

impl Service {
    pub(super) fn native_identity_matches(&self, path: &Path, id: u64, index: usize) -> bool {
        self.disk_identity(path)
            .is_ok_and(|(owner, slot, _)| owner == id && slot == index)
    }
    pub(super) fn disk_identity(&self, path: &Path) -> io::Result<(u64, usize, bool)> {
        let meta = std::fs::symlink_metadata(path)?;
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        self.handle_identity(file.as_raw_handle(), meta.len())
    }
    pub(super) fn handle_identity(&self, handle: HANDLE, length: u64) -> io::Result<(u64, usize, bool)> {
        self.handle_identity_traced(handle, length, HydrationTrace::default())
    }
    pub(super) fn handle_identity_traced(
        &self,
        handle: HANDLE,
        length: u64,
        trace: HydrationTrace,
    ) -> io::Result<(u64, usize, bool)> {
        let mut buffer = [0u64; 1024];
        let mut returned = 0;
        // SAFETY: aligned output covers the header and our fixed 16-byte identity.
        trace.hr(54, unsafe {
            (self.api.info)(
                handle,
                CF_PLACEHOLDER_INFO_STANDARD,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
                &mut returned,
            )
        })?;
        // SAFETY: info was initialized by the successful call above.
        let info = unsafe { &*buffer.as_ptr().cast::<CF_PLACEHOLDER_STANDARD_INFO>() };
        if info.FileIdentityLength != 16 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        // SAFETY: the identity is inside the larger native output allocation.
        let bytes = unsafe { std::slice::from_raw_parts(info.FileIdentity.as_ptr(), 16) };
        Ok((
            u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            u64::from_le_bytes(bytes[8..].try_into().unwrap()) as usize,
            info.OnDiskDataSize >= length as i64,
        ))
    }
    /// Runs once before publishing CURRENT. Only an owned batch identity permits
    /// recovery; arbitrary user files and reparse targets are never traversed.
    pub(super) fn recover_received(&self) -> io::Result<()> {
        let _mutation = crate::cache::Mutation(&self.root);
        {
            let mut journal = self.received.lock().unwrap();
            let provisional: Vec<_> = journal
                .sources
                .iter()
                .filter(|(_, source)| source.descriptor.as_ref().is_some_and(|link| link.provisional))
                .map(|(id, _)| *id)
                .collect();
            journal.cancelled.extend(provisional);
            journal.save(&self.root)?;
        }
        for source in std::fs::read_dir(&self.root)?.flatten() {
            if !source.file_type()?.is_dir() && !crate::cache::is_link(&source.metadata()?) {
                continue;
            }
            let source_path = source.path();
            if !self
                .disk_identity(&source_path)
                .is_ok_and(|(_, index, _)| index == usize::MAX)
            {
                continue;
            }
            for batch in std::fs::read_dir(&source_path)?.flatten() {
                let batch_path = batch.path();
                let Ok((id, index, _)) = self.disk_identity(&batch_path) else {
                    continue;
                };
                if index != usize::MAX {
                    continue;
                }
                let mut found = BTreeMap::new();
                let mut queue = vec![batch_path.clone()];
                while let Some(parent) = queue.pop() {
                    for child in std::fs::read_dir(parent)?.flatten() {
                        let path = child.path();
                        let meta = std::fs::symlink_metadata(&path)?;
                        let Ok((owner, index, full)) = self.disk_identity(&path) else {
                            continue;
                        };
                        if owner != id || index >= crate::model::MAX_DISCOVERED_NODES {
                            continue;
                        }
                        let directory = meta.is_dir();
                        // Enumerating a partial native directory here would pull
                        // the remote source during startup. Its durable catalog
                        // already records every previously discovered child.
                        let pending = self.received.lock().unwrap().sources.get(&id).is_some_and(|source| {
                            source
                                .nodes
                                .get(index)
                                .is_some_and(|node| source.pending.contains(&node.id))
                        });
                        if directory && !pending {
                            queue.push(path.clone());
                        }
                        let mut life = Item::new(received::now());
                        life.resume_at(received::now());
                        life.materialized = full && !directory;
                        found.insert(
                            index,
                            Node {
                                path: path.strip_prefix(&self.root).map_err(io::Error::other)?.into(),
                                life,
                                directory,
                                deleted: false,
                                withdrawn: false,
                                local_final: false,
                                renamed_locally: false,
                                handed_off: full && !directory,
                                length: meta.len(),
                            },
                        );
                    }
                }
                if found.is_empty() {
                    // No native children: safe empty-only orphan rollback.
                    if !self.received.lock().unwrap().deliveries.contains_key(&id) {
                        let _ = std::fs::remove_dir(&batch_path);
                    }
                } else {
                    self.received.lock().unwrap().merge_discovered(id, found);
                }
            }
            // Includes old failed preparations with no journaled delivery.
            // rmdir cannot remove any materialized/user payload.
            let _ = std::fs::remove_dir(&source_path);
        }
        self.restore_linked_deliveries()?;
        self.received.lock().unwrap().save(&self.root)?;
        self.received_sweep();
        Ok(())
    }

    fn restore_linked_deliveries(&self) -> io::Result<()> {
        let saved: Vec<_> = {
            let journal = self.received.lock().unwrap();
            journal
                .sources
                .iter()
                .filter(|(id, _)| !journal.cancelled.contains(id))
                .filter_map(|(id, source)| {
                    journal
                        .deliveries
                        .get(id)
                        .map(|nodes| (*id, source.clone(), nodes.clone()))
                })
                .collect()
        };
        for (id, source, nodes) in saved {
            let cancelled = self.received.lock().unwrap().cancelled.contains(&id);
            if nodes.iter().all(|node| node.deleted) {
                continue;
            }
            source.validate()?;
            let path = self.root.join(&source.base);
            if !self.owned_container(&path, id).unwrap_or(false)
                && !nodes.iter().enumerate().any(|(index, node)| {
                    !node.deleted && self.native_identity_matches(&self.root.join(&node.path), id, index)
                })
            {
                continue;
            }
            let mut names = crate::content::namespace::Names::new(crate::sync::files::TargetOs::Windows);
            let entries = nodes
                .iter()
                .enumerate()
                .map(|(index, node)| {
                    let original = source.nodes.get(index).ok_or(io::ErrorKind::InvalidData)?;
                    let mut entry = names.entry(original)?;
                    if entry.is_dir != node.directory {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                    entry.size = node.length;
                    Ok(entry)
                })
                .collect::<io::Result<Vec<_>>>()?;
            let namespace = source
                .settlement
                .restore(source.descriptor.clone(), crate::content::linked::native_connector())?;
            let contents = crate::content::namespace::Files::restore_table(
                namespace,
                crate::sync::files::TargetOs::Windows,
                source.nodes.clone(),
                entries.clone(),
            )?;
            let native_paths: Vec<_> = nodes.iter().map(|node| self.root.join(&node.path)).collect();
            let roots = contents
                .descriptor_nodes()?
                .unwrap()
                .iter()
                .enumerate()
                .filter(|(_, node)| node.parent.is_none())
                .map(|(index, _)| native_paths[index].clone())
                .collect();
            let delivery = Arc::new(Delivery {
                service: self.context.as_ref().clone(),
                population: Mutex::new(()),
                population_requests: Mutex::new(HashMap::new()),
                download: OnceLock::new(),
                downloading: AtomicBool::new(false),
                id,
                path,
                native_paths: Mutex::new(native_paths),
                roots,
                files: Mutex::new(
                    entries
                        .into_iter()
                        .zip(&nodes)
                        .map(|(entry, node)| {
                            let file = DeliveredFile::new(entry, node.deleted || cancelled);
                            file.withdrawn.store(node.withdrawn, Ordering::Release);
                            file.local_final.store(node.local_final, Ordering::Release);
                            Arc::new(file)
                        })
                        .collect(),
                ),
                contents: Mutex::new(contents),
                created: Instant::now(),
                readers: Arc::new(AtomicUsize::new(0)),
            });
            self.deliveries.lock().unwrap().insert(id, delivery.clone());
            native::bind(&delivery)?;
            for (index, node) in nodes
                .iter()
                .enumerate()
                .filter(|(_, node)| !node.deleted && !node.directory)
            {
                let refetchable = !node.local_final && !node.withdrawn && source.refetchable(index);
                if refetchable || node.local_final || node.handed_off {
                    self.restore_received_policy(&self.root.join(&node.path), node, refetchable);
                }
            }
        }
        Ok(())
    }
    fn prune_empty_received(&self) {
        let (deliveries, complete) = {
            let journal = self.received.lock().unwrap();
            (journal.deliveries.clone(), journal.cleanup_complete.clone())
        };
        for (id, nodes) in &deliveries {
            // Empty directory deliveries are received items too; preserve them
            // by default and apply the explicit age policy without recursion.
            let cancelled = {
                let journal = self.received.lock().unwrap();
                if journal.cleanup_complete.contains(&(*id, usize::MAX)) {
                    continue;
                }
                journal.cancelled.contains(id)
            };
            if !cancelled
                && nodes.iter().all(|n| n.directory)
                && nodes.iter().any(|n| {
                    !n.deleted && received::setting().days() == 0
                        || !n.deleted
                            && received::now().saturating_sub(n.life.last_used)
                                < received::setting().days() * received::DAY
                })
            {
                continue;
            }
            if nodes.iter().any(|n| !n.directory && !n.deleted) {
                continue;
            }
            let live = self.deliveries.lock().unwrap().get(id).cloned();
            {
                let mut journal = self.received.lock().unwrap();
                if live
                    .as_ref()
                    .is_some_and(|delivery| delivery.readers.load(Ordering::Acquire) != 0)
                    || journal.has_open_received_item(*id)
                    || journal.cleanup_inflight.iter().any(|(owner, _)| owner == id)
                {
                    continue;
                }
                journal.cleanup_inflight.insert((*id, usize::MAX));
            }
            // A batch may mix an empty selection with a nonempty selection.
            // Finishing/moving the latter's last file does not consume the former.
            // Preserve each originally empty directory under its own age policy;
            // only former file ancestors are synthetic empty-parent cleanup.
            // Paths are journal identities, not filesystem enumeration/following.
            let file_paths: std::collections::BTreeSet<_> = nodes
                .iter()
                .filter(|node| !node.directory)
                .map(|node| node.path.clone())
                .collect();
            let now = received::now();
            let days = received::setting().days();
            let mut dirs: Vec<_> = nodes
                .iter()
                .enumerate()
                .filter(|(index, node)| {
                    node.directory
                        && !complete.contains(&(*id, *index))
                        && (cancelled
                            || days != 0
                                && now.saturating_sub(node.life.last_used) >= days * received::DAY
                                && !self.received.lock().unwrap().refetchable(*id, *index)
                            || file_paths
                                .range(node.path.clone()..)
                                .next()
                                .is_some_and(|path| path.starts_with(&node.path)))
                })
                .map(|(index, node)| (self.root.join(&node.path), index))
                .collect();
            if let Some(node) = nodes.first() {
                let batch: PathBuf = node.path.components().take(2).collect();
                dirs.push((self.root.join(batch), usize::MAX));
            }
            dirs.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
            dirs.dedup();
            for (path, index) in dirs {
                let result = exclusive_delete_handle(&self.api, &path, |handle| {
                    self.handle_identity(handle, 0)
                        .is_ok_and(|(owner, slot, _)| owner == *id && slot == index)
                });
                if cleanup_finished(&result) {
                    self.cleanup_done(*id, index, &path);
                }
            }
            self.received
                .lock()
                .unwrap()
                .cleanup_inflight
                .remove(&(*id, usize::MAX));
            // The plain source grouping directory has no native identity. Leave
            // an empty group rather than unlink a possible replacement by path.
        }
    }
    pub(super) fn cleanup_done(&self, id: u64, index: usize, path: &Path) {
        let mut journal = self.received.lock().unwrap();
        if let Some(node) = journal.deliveries.get_mut(&id).and_then(|nodes| nodes.get_mut(index)) {
            if self.root.join(&node.path) != path {
                return;
            }
            node.deleted = true;
        }
        journal.cleanup_complete.insert((id, index));
    }
}

/// CF sync roots are reparse points too. Reject redirection tags, not Cloud tags.
pub(super) fn valid_root(path: &Path) -> io::Result<bool> {
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: the handle and fixed-size native output remain live.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // IO_REPARSE_TAG_CLOUD and its versioned variants share all but bits 12..15.
    Ok(info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
        && (info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 || info.ReparseTag & !0xf000 == 0x9000001a))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn received_moves_can_leave_root_but_cannot_move_provider_bookkeeping() {
        let root = Path::new(r"C:\Oink\CloudFiles");
        let old = root.join(r"source\batch\file.txt");
        let outside = Path::new(r"C:\destination\file.txt");
        let source = CF_CALLBACK_RENAME_FLAG_SOURCE_IN_SCOPE;
        let both = source | CF_CALLBACK_RENAME_FLAG_TARGET_IN_SCOPE;
        assert!(rename_allowed(root, &old, outside, source));
        assert!(rename_allowed(
            root,
            &old,
            &root.join(r"source\batch\renamed.txt"),
            both
        ));
        for path in [root.to_path_buf(), root.join("source"), root.join(r"source\batch")] {
            assert!(!rename_allowed(root, &path, outside, source));
        }
        assert!(!rename_allowed(
            root,
            &old,
            &root.join(r"source\other-batch\file.txt"),
            both
        ));
        assert!(!rename_allowed(
            root,
            &old,
            &root.join(r"source\batch\..\file.txt"),
            both
        ));
        assert!(!rename_allowed(root, &old, outside, both));
        assert!(!rename_allowed(root, &old, outside, CF_CALLBACK_RENAME_FLAG_NONE));
        assert!(!rename_allowed(root, outside, &old, both));
        assert!(!rename_allowed(root, &old, Path::new("relative"), source));
    }
    #[test]
    fn callback_paths_preserve_leaf_case_and_reject_neighboring_roots() {
        let root = Path::new(r"C:\fixture\received");
        assert_eq!(
            relative_path(root, Path::new(r"c:\FIXTURE\Received\source\batch\Leaf")),
            Some(PathBuf::from(r"source\batch\Leaf"))
        );
        assert!(relative_path(root, Path::new(r"C:\fixture\received-other\file")).is_none());
        let volume = wide("C:");
        let normalized = wide(r"\fixture\received\source\batch\Leaf");
        let info = CF_CALLBACK_INFO {
            VolumeDosName: volume.as_ptr(),
            ..Default::default()
        };
        // SAFETY: both counted/terminated native strings live through the helper.
        let actual = unsafe { callback_path(&info, normalized.as_ptr()) };
        assert_eq!(actual, Some(PathBuf::from(r"C:\fixture\received\source\batch\Leaf")));
    }
    // Uses the debug-only cache mutation helpers.
    #[cfg(debug_assertions)]
    #[test]
    fn journal_restart_discards_open_balances_and_advances_last_used() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("CloudFiles");
        let mut life = Item::new(1);
        life.materialized = true;
        life.observe(Reader::App, 2);
        let node = Node {
            path: PathBuf::from(r"device\batch\file"),
            life,
            directory: false,
            deleted: false,
            withdrawn: false,
            local_final: false,
            renamed_locally: false,
            handed_off: true,
            length: 4,
        };
        let mut journal = Journal::default();
        journal.deliveries.insert(7, vec![node]);
        journal.opens.insert((7, 0), 1);
        crate::cache::assert_mutation(&root, || journal.save(&root)).unwrap();
        crate::cache::assert_no_mutation(&root, || journal.save(&root)).unwrap();
        let recovered = Journal::load(&root).unwrap();
        assert!(recovered.opens.is_empty());
        let item = &recovered.deliveries[&7][0].life;
        assert!(item.materialized);
        assert!(item.last_used >= 2);
        journal.deliveries.get_mut(&7).unwrap()[0].path = PathBuf::from(r"..\foreign");
        journal.save(&root).unwrap();
        assert_eq!(Journal::load(&root).err().unwrap().kind(), io::ErrorKind::InvalidData);
    }
    #[test]
    fn explorer_has_no_special_cleanup_identity() {
        let classes = reader_classes();
        let explorer = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("explorer.exe");
        assert_eq!(classes.get(&explorer.to_string_lossy().to_lowercase()), None);
        let temp = tempfile::tempdir().unwrap();
        let imposter = temp.path().join("explorer.exe");
        std::fs::write(&imposter, b"not a signed executable").unwrap();
        assert!(!trusted(&imposter));
    }
}
