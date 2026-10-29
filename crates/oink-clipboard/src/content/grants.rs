//! Device-bound, durable authority for offered files. Real files retain their
//! identities; one-shot files retain private immutable sender fulfilments until
//! receipt. The journal contains metadata only, never received file bytes.
use super::{
    Unavailable,
    namespace::{self, Namespace, local},
    unavailable,
};
use crate::{
    FileReader,
    model::{Demand, DirectoryCursor, DirectoryPage, FileNode, NodeId},
};
use oink_foundation::DeviceId;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use super::native_metadata::NativeFileMetadata;
mod location;
use location::Location;
mod fulfilled;
use fulfilled::Snapshot;
mod cache;
pub use cache::usage as cache_usage;
mod clipboard;
mod file_filter;
mod lifetime;
mod scope;
use clipboard::CurrentClipboard;
pub type ClipboardObservation = (oink_foundation::content::OfferId, Option<[u8; 32]>);

/// A bearer value is still useless without its authenticated device identity.
/// Do not derive Debug: protocol/diagnostic formatting must never reveal it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LinkToken(pub [u8; 32]);

/// A one-shot root and its random fulfilment identity. This identifies a native
/// receive lifecycle; it does not verify content or authorize a receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FulfilledRoot {
    pub node: NodeId,
    #[serde(default)]
    pub id: [u8; 16],
}
impl std::fmt::Debug for LinkToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LinkToken([redacted])")
    }
}

/// Only a native namespace can construct an offered source root. Arbitrary
/// paths from a receiver are never accepted by the grant API.
pub struct SourceRoot {
    entry: FileNode,
    location: Location,
}
impl std::fmt::Debug for SourceRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SourceRoot")
    }
}
impl SourceRoot {
    pub(in crate::content) fn id(&self) -> NodeId {
        self.entry.id
    }
    pub(in crate::content) fn capture_followed(
        entry: FileNode,
        path: &Path,
        expected: &local::Fingerprint,
        followed: Vec<local::batch::Followed>,
    ) -> io::Result<Self> {
        let location = Location::capture_followed(path, followed)?;
        if location.fingerprint.identity != expected.identity || location.fingerprint.created != expected.created {
            return Err(unavailable(Unavailable::Superseded));
        }
        Ok(Self { entry, location })
    }
}

const JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema: u8,
    grants: Vec<Grant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    clipboard: Option<CurrentClipboard>,
}
impl Journal {
    fn discard_one_shot(&mut self) {
        self.clipboard = None;
        for grant in &mut self.grants {
            let ephemeral: HashSet<_> = grant.snapshots.iter().map(|snapshot| snapshot.root).collect();
            grant.roots.retain(|root| !ephemeral.contains(root));
            grant.nodes.retain(|_, node| !ephemeral.contains(&node.root));
            grant.snapshots.clear();
            if !ephemeral.is_empty() {
                grant.requested.clear();
                grant.native = grant
                    .native
                    .restricted(&grant.roots.iter().copied().map(NodeId).collect::<Vec<_>>());
            }
        }
        self.grants.retain(|grant| !grant.roots.is_empty());
    }
    fn validate(&self) -> io::Result<()> {
        let invalid = || unavailable(Unavailable::InvalidData);
        if self.schema != 1 {
            return Err(invalid());
        }
        if let Some(clipboard) = &self.clipboard {
            clipboard.validate()?;
        }
        let mut tokens = HashSet::new();
        for grant in &self.grants {
            if !tokens.insert(grant.token)
                || grant.token.0 == [0; 32]
                || grant
                    .holder
                    .is_some_and(|holder| holder.device != grant.peer || grant.offered.is_none())
                || (!grant.active && grant.holder.is_none())
                || grant.roots.is_empty()
                || grant.roots.iter().collect::<HashSet<_>>().len() != grant.roots.len()
                || grant.nodes.len() > crate::model::MAX_DISCOVERED_NODES
                || grant.nodes.keys().max().is_none_or(|id| *id >= grant.next)
            {
                return Err(invalid());
            }
            for root in &grant.roots {
                if !grant
                    .nodes
                    .get(root)
                    .is_some_and(|node| node.root == *root && node.entry.parent.is_none())
                {
                    return Err(invalid());
                }
            }
            grant
                .native
                .validate(&grant.roots.iter().copied().map(NodeId).collect::<Vec<_>>())?;
            if !grant.requested.is_empty()
                && grant
                    .native
                    .resolve_roots(
                        &grant.requested.iter().copied().map(NodeId).collect::<Vec<_>>(),
                        &grant.roots.iter().copied().map(NodeId).collect::<Vec<_>>(),
                    )
                    .ok()
                    != Some(grant.roots.iter().copied().map(NodeId).collect::<Vec<_>>())
                && grant.roots.iter().any(|id| !grant.requested.contains(id))
            {
                return Err(invalid());
            }
            if grant.native.windows.iter().any(|entry| {
                !grant
                    .nodes
                    .get(&entry.node.0)
                    .is_some_and(|node| node.root == entry.root.0)
            }) {
                return Err(invalid());
            }
            let mut snapshots = HashSet::new();
            for snapshot in &grant.snapshots {
                if !grant.roots.contains(&snapshot.root) || !snapshots.insert(snapshot.root) || snapshot.id == [0; 16] {
                    return Err(invalid());
                }
                for node in grant.nodes.values().filter(|node| node.root == snapshot.root) {
                    if node.location.fingerprint.directory != (node.entry.kind == crate::model::NodeKind::Directory)
                        || (node.location.fingerprint.directory && node.view.is_none())
                        || (!node.location.fingerprint.directory
                            && node.entry.size != crate::model::ContentSize::Known(node.location.fingerprint.size))
                    {
                        return Err(invalid());
                    }
                }
            }
            for (id, node) in &grant.nodes {
                if node.entry.id.0 != *id || !node.entry.valid() || !grant.roots.contains(&node.root) {
                    return Err(invalid());
                }
                let mut ancestor = *id;
                for depth in 0..256 {
                    let current = grant.nodes.get(&ancestor).ok_or_else(invalid)?;
                    if current.root != node.root {
                        return Err(invalid());
                    }
                    match current.entry.parent {
                        Some(parent) if depth < 255 => ancestor = parent.0,
                        None if ancestor == node.root => break,
                        _ => return Err(invalid()),
                    }
                }
                if let Some(view) = &node.view
                    && (!node.location.fingerprint.directory
                        || view
                            .entries
                            .iter()
                            .any(|entry| entry.parent != Some(NodeId(*id)) || !grant.nodes.contains_key(&entry.id.0)))
                {
                    return Err(invalid());
                }
            }
        }
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    offered: Option<oink_foundation::content::OfferId>,
    #[serde(default)]
    clipboard: bool,
    token: LinkToken,
    holder: Option<oink_foundation::content::Holder>,
    active: bool,
    peer: DeviceId,
    omit_ds_store: bool,
    #[serde(default)]
    safe_symlinks: bool,
    #[serde(default)]
    exec_bits: bool,
    membership: [u8; 32],
    roots: Vec<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    requested: Vec<u64>,
    next: u64,
    nodes: BTreeMap<u64, Node>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    snapshots: Vec<Snapshot>,
    #[serde(default, skip_serializing_if = "NativeFileMetadata::is_empty")]
    native: NativeFileMetadata,
    batch: local::batch::Batch,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Node {
    root: u64,
    entry: FileNode,
    location: Location,
    view: Option<View>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct View {
    version: u64,
    entries: Vec<FileNode>,
}
struct State {
    journal: Journal,
    members: HashMap<DeviceId, [u8; 32]>,
    healthy: bool,
    clipboard_confirmed: bool,
    progress: HashMap<(LinkToken, u64, u8, u64), u64>,
    sent: HashSet<(LinkToken, NodeId)>,
}
struct Inner {
    path: PathBuf,
    state: Mutex<State>,
    // Request policy lookup must not wait for directory IO holding the journal lock.
    file_filters: Mutex<HashMap<DeviceId, bool>>,
    symlink_peers: Mutex<HashMap<DeviceId, bool>>,
    exec_peers: Mutex<HashMap<DeviceId, bool>>,
    fulfilment: Mutex<()>,
    clock_epoch_ms: u64,
    clock_started: std::time::Instant,
}
#[derive(Clone)]
pub struct SourceGrants(Arc<Inner>);
impl std::fmt::Debug for SourceGrants {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SourceGrants")
    }
}
impl SourceGrants {
    /// No tokens, paths or content hashes enter watchdog output.
    #[cfg(debug_assertions)]
    pub(crate) fn watchdog_snapshot_for_test(&self) -> String {
        let state = self.0.state.try_lock().map(|state| {
            let grants: Vec<_> = state
                .journal
                .grants
                .iter()
                .map(|grant| {
                    let snapshots: Vec<_> = grant
                        .snapshots
                        .iter()
                        .map(|s| (s.root, s.started, s.retired, s.deadline_ms))
                        .collect();
                    (
                        grant.offered,
                        grant.peer,
                        grant.holder,
                        grant.active,
                        grant.clipboard,
                        grant.roots.len(),
                        snapshots,
                    )
                })
                .collect();
            (state.healthy, state.members.len(), grants)
        });
        format!(
            "grants state={state:?} fulfilment_pending={}",
            self.0.fulfilment.try_lock().is_err()
        )
    }

    /// Startup receives the current authenticated group roster, never a cached
    /// roster from this journal. Each member carries the verified signed add-record
    /// digest, so remove/re-add or a different group cannot revive stale grants.
    /// Removal is persisted before any admission.
    pub fn load(directory: &Path, members: impl IntoIterator<Item = (DeviceId, [u8; 32])>) -> io::Result<Self> {
        fs::create_dir_all(directory)?;
        let path = directory.join("source-links-v1.json");
        let mut journal = match File::open(&path) {
            Ok(file) => {
                if file.metadata()?.len() > JOURNAL_BYTES {
                    return Err(unavailable(Unavailable::LimitExceeded));
                }
                let mut journal: Journal = match serde_json::from_reader(file) {
                    Ok(journal) => journal,
                    Err(error) if error.is_data() => {
                        // Pre-release journal layouts are not migrated. Revoke
                        // old tokens atomically before collecting their private
                        // snapshots; new source requests must remain available.
                        tracing::info!(
                            reason = "source_links_schema_reset",
                            "discarded obsolete source file authority"
                        );
                        Journal {
                            schema: 1,
                            grants: vec![],
                            clipboard: None,
                        }
                    }
                    Err(_) => return Err(unavailable(Unavailable::InvalidData)),
                };
                journal.validate()?;
                journal.grants.retain(|grant| grant.active);
                journal
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Journal {
                schema: 1,
                grants: vec![],
                clipboard: None,
            },
            Err(error) => return Err(error),
        };
        let now = lifetime::now_ms();
        // User decision 2026-10-29: only real-file authority survives a sender
        // restart. F1 caches and clipboard fulfilment state are session-owned.
        journal.discard_one_shot();
        let store = Self(Arc::new(Inner {
            path,
            clock_epoch_ms: now,
            clock_started: std::time::Instant::now(),
            fulfilment: Mutex::new(()),
            file_filters: Mutex::default(),
            symlink_peers: Mutex::default(),
            exec_peers: Mutex::default(),
            state: Mutex::new(State {
                journal,
                members: HashMap::new(),
                healthy: false,
                clipboard_confirmed: false,
                progress: HashMap::new(),
                sent: HashSet::new(),
            }),
        }));
        store.reconcile_members(members)?;
        fulfilled::collect(&store.0.path, &store.0.state.lock().unwrap().journal)?;
        Ok(store)
    }
    fn persist(&self, journal: &Journal) -> io::Result<()> {
        journal.validate()?;
        struct Bounded(Vec<u8>);
        impl io::Write for Bounded {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if bytes.len() as u64 + self.0.len() as u64 > JOURNAL_BYTES {
                    return Err(unavailable(Unavailable::LimitExceeded));
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut bytes = Bounded(Vec::new());
        let mut durable = journal.clone();
        durable.discard_one_shot();
        serde_json::to_writer(&mut bytes, &durable).map_err(|_| unavailable(Unavailable::LimitExceeded))?;
        oink_foundation::storage::atomic_write(&self.0.path, &bytes.0)
            .map_err(|error| io::Error::new(error.kind(), format!("source link journal commit: {error}")))
    }
    pub fn reconcile_members(&self, members: impl IntoIterator<Item = (DeviceId, [u8; 32])>) -> io::Result<()> {
        let mut state = self.0.state.lock().unwrap();
        let members = members.into_iter().collect::<HashMap<_, _>>();
        if state.healthy && state.members == members {
            return Ok(());
        }
        state.members = members.clone();
        self.0
            .exec_peers
            .lock()
            .unwrap()
            .retain(|peer, _| members.contains_key(peer));
        self.0
            .symlink_peers
            .lock()
            .unwrap()
            .retain(|peer, _| members.contains_key(peer));
        self.0
            .file_filters
            .lock()
            .unwrap()
            .retain(|peer, _| members.contains_key(peer));
        let snapshots = state
            .journal
            .grants
            .iter()
            .flat_map(|grant| grant.snapshots.clone())
            .collect::<Vec<_>>();
        state
            .journal
            .grants
            .retain(|grant| members.get(&grant.peer) == Some(&grant.membership));
        // Even a failed disk write revokes live readers immediately. Fail closed
        // until the roster can be persisted; reconnect cannot resurrect a token.
        state.healthy = false;
        self.persist(&state.journal)?;
        state.healthy = true;
        let tokens: HashSet<_> = state.journal.grants.iter().map(|grant| grant.token).collect();
        state.progress.retain(|(token, _, _, _), _| tokens.contains(token));
        drop(state);
        let _fulfilment = self.0.fulfilment.lock().unwrap();
        let state = self.0.state.lock().unwrap();
        fulfilled::remove_unreferenced(&self.0.path, &snapshots, &state.journal)?;
        Ok(())
    }
    /// Configure the next grant from the authenticated request peer. Existing
    /// grants retain their view across setting changes, pagination and restart.
    pub fn set_peer_file_filter(&self, peer: DeviceId, omit_ds_store: bool) {
        self.0.file_filters.lock().unwrap().insert(peer, omit_ds_store);
    }
    /// Both endpoints and the authenticated link must support Mac symlink items.
    pub fn set_peer_safe_symlinks(&self, peer: DeviceId, enabled: bool) {
        self.0
            .symlink_peers
            .lock()
            .unwrap()
            .insert(peer, enabled && cfg!(target_os = "macos"));
    }
    pub fn set_peer_exec_bits(&self, peer: DeviceId, enabled: bool) {
        self.0
            .exec_peers
            .lock()
            .unwrap()
            .insert(peer, enabled && cfg!(target_os = "macos"));
    }
    pub(super) fn project_for_peer(&self, peer: DeviceId, entry: FileNode) -> FileNode {
        file_filter::project(
            entry,
            self.0.exec_peers.lock().unwrap().get(&peer).copied().unwrap_or(false),
        )
    }
    /// Transient listing requests use the same peer policy before pagination.
    /// Durable publications instead keep the policy captured in their grant.
    pub(super) fn list_for_peer(
        &self,
        peer: DeviceId,
        source: &dyn Namespace,
        id: NodeId,
        cursor: Option<DirectoryCursor>,
    ) -> io::Result<DirectoryPage> {
        let omit = self.0.file_filters.lock().unwrap().get(&peer).copied().unwrap_or(false);
        let links = self
            .0
            .symlink_peers
            .lock()
            .unwrap()
            .get(&peer)
            .copied()
            .unwrap_or(false);
        let exec = self.0.exec_peers.lock().unwrap().get(&peer).copied().unwrap_or(false);
        if !omit && links && exec {
            return source.list(id, cursor);
        }
        let mut entries = Vec::new();
        let mut next = None;
        loop {
            let page = source.list(id, next)?;
            page.validate(next).map_err(unavailable)?;
            if page.directory != id {
                return Err(unavailable(Unavailable::InvalidData));
            }
            if entries.len() + page.entries.len() > crate::model::MAX_DISCOVERED_NODES {
                return Err(unavailable(Unavailable::LimitExceeded));
            }
            entries.extend(page.entries);
            next = page.next;
            if next.is_none() {
                return file_filter::page_for(omit, links, exec, id, page.version, &entries, cursor);
            }
        }
    }
    pub fn issue(&self, peer: DeviceId, source: &dyn Namespace, roots: &[NodeId]) -> io::Result<LinkToken> {
        self.issue_for(peer, None, source, roots, Demand::Clipboard)
    }
    pub(super) fn issue_for(
        &self,
        peer: DeviceId,
        offered: Option<oink_foundation::content::OfferId>,
        source: &dyn Namespace,
        requested: &[NodeId],
        demand: Demand,
    ) -> io::Result<LinkToken> {
        if requested.is_empty() || requested.iter().collect::<HashSet<_>>().len() != requested.len() {
            return Err(unavailable(Unavailable::InvalidData));
        }
        let holder = match demand {
            Demand::Clipboard => None,
            Demand::Hover(holder) | Demand::Drop(holder) if holder.device == peer && offered.is_some() => Some(holder),
            _ => return Err(unavailable(Unavailable::Permission)),
        };
        let active = !matches!(demand, Demand::Hover(_));
        let one_shot = source.one_shot_roots();
        // Native one-shot bodies cannot be acquired twice by racing receivers.
        // Ordinary persistent links do not wait for this potentially long work.
        let _fulfilment = (!one_shot.is_empty()).then(|| self.0.fulfilment.lock().unwrap());
        let omit_ds_store = self.0.file_filters.lock().unwrap().get(&peer).copied().unwrap_or(false);
        let safe_symlinks = self
            .0
            .symlink_peers
            .lock()
            .unwrap()
            .get(&peer)
            .copied()
            .unwrap_or(false);
        let exec_bits = self.0.exec_peers.lock().unwrap().get(&peer).copied().unwrap_or(false);
        let previous = |state: &State| {
            offered.and_then(|offered| {
                state
                    .journal
                    .grants
                    .iter()
                    .find(|grant| {
                        grant.peer == peer
                            && grant.omit_ds_store == omit_ds_store
                            && grant.safe_symlinks == safe_symlinks
                            && grant.exec_bits == exec_bits
                            && state.members.get(&peer) == Some(&grant.membership)
                            && grant.offered == Some(offered)
                            && grant.snapshots.iter().all(|snapshot| !snapshot.retired)
                            && grant.holder == holder
                            && (if grant.requested.is_empty() {
                                &grant.roots
                            } else {
                                &grant.requested
                            })
                            .iter()
                            .copied()
                            .eq(requested.iter().map(|id| id.0))
                    })
                    .map(|grant| grant.token)
            })
        };
        let membership = {
            let mut state = self.0.state.lock().unwrap();
            if !state.healthy {
                return Err(unavailable(Unavailable::Permission));
            }
            let membership = *state
                .members
                .get(&peer)
                .ok_or_else(|| unavailable(Unavailable::Permission))?;
            // A lost response can be retried even after the granted object was
            // renamed. The already-authorized scope does not need recapturing.
            if let Some(token) = previous(&state) {
                self.activate(&mut state, token, active)?;
                return Ok(token);
            }
            membership
        };
        let scope = self.resolve_scope(offered, source, requested, holder.is_none())?;
        let roots = scope.roots.as_slice();
        let one_shot = scope.one_shot;
        // A native renderer may fulfil the whole clipboard in one call. Retain
        // all its roots while this system slot owns them; receiver authority
        // remains restricted to the explicitly requested roots below.
        let capture_roots = {
            let state = self.0.state.lock().unwrap();
            if holder.is_none()
                && roots.iter().any(|id| one_shot.contains(id))
                && state.clipboard_confirmed
                && state
                    .journal
                    .clipboard
                    .as_ref()
                    .is_some_and(|current| Some(current.offer) == offered)
            {
                one_shot.clone()
            } else {
                roots.iter().copied().filter(|id| one_shot.contains(id)).collect()
            }
        };
        let reusable = {
            let state = self.0.state.lock().unwrap();
            let mut snapshots = BTreeMap::new();
            if holder.is_none()
                && state.clipboard_confirmed
                && let Some(clipboard) = state
                    .journal
                    .clipboard
                    .as_ref()
                    .filter(|current| Some(current.offer) == offered)
            {
                for root in &clipboard.roots {
                    if capture_roots.contains(&NodeId(root.snapshot.root)) {
                        snapshots.insert(root.snapshot.root, (root.snapshot.clone(), root.nodes.clone()));
                    }
                }
            }
            // A native promise has already handed its temporary bytes to this
            // offer. Host/epoch changes need fresh authority, not another native
            // fulfilment. Provisional grants belong to the live source lease;
            // abandoning one host does not end that source drag.
            if holder.is_some() {
                for grant in state.journal.grants.iter().filter(|grant| grant.offered == offered) {
                    for snapshot in &grant.snapshots {
                        if !snapshot.retired && capture_roots.contains(&NodeId(snapshot.root)) {
                            snapshots.entry(snapshot.root).or_insert_with(|| {
                                (
                                    snapshot.clone(),
                                    grant
                                        .nodes
                                        .iter()
                                        .filter(|(_, node)| node.root == snapshot.root)
                                        .map(|(id, node)| (*id, node.clone()))
                                        .collect(),
                                )
                            });
                        }
                    }
                }
            }
            snapshots
        };
        let mut captured = if roots.iter().all(|id| one_shot.contains(id)) {
            Vec::new()
        } else {
            source.source_roots()?
        };
        let mut batch = local::batch::Batch::default();
        captured.retain(|source| {
            if !roots.contains(&source.entry.id) || source.entry.kind.symlink_target().is_some() {
                return true;
            }
            match source.location.resolve() {
                Ok(path) => {
                    batch.include(&path, vec![source.entry.name.clone()], &source.location.fingerprint);
                    true
                }
                Err(error) => {
                    tracing::warn!(reason=?error.kind(), "skipping unavailable grant root");
                    false
                }
            }
        });
        captured.retain_mut(|source| {
            if !roots.contains(&source.entry.id) || source.entry.kind.symlink_target().is_none() {
                return true;
            }
            let mut classify = || -> io::Result<_> {
                let path = source.location.resolve()?;
                let item = batch
                    .capture(&path, vec![source.entry.name.clone()], &source.location.followed)?
                    .ok_or_else(|| unavailable(Unavailable::Unsupported))?;
                let mut entry = local::entry(source.entry.id, None, source.entry.name.clone(), &item.fingerprint)?;
                if let Some(target) = item.link {
                    local::set_link(&mut entry, target);
                }
                Ok((entry, Location::capture_followed(&item.path, item.followed)?))
            };
            match classify() {
                Ok((entry, location)) => {
                    source.entry = entry;
                    source.location = location;
                    true
                }
                Err(error) => {
                    tracing::warn!(reason=?error.kind(), "skipping unavailable granted link");
                    false
                }
            }
        });
        let valid_roots: Vec<_> = roots
            .iter()
            .copied()
            .filter(|id| one_shot.contains(id) || captured.iter().any(|root| root.entry.id == *id))
            .collect();
        let roots = valid_roots.as_slice();
        if roots.is_empty() {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        let offered_roots = source.roots();
        let mut builds = Vec::new();
        for id in &capture_roots {
            if reusable.contains_key(&id.0) {
                continue;
            }
            let entry = offered_roots
                .iter()
                .find(|root| root.id == *id)
                .ok_or_else(|| unavailable(Unavailable::Permission))?
                .clone();
            builds.push(fulfilled::Build::capture(&self.0.path, source, entry)?);
        }
        // Rendering can advance the native sequence; capture its final identity
        // while still holding fulfilment exclusion, before publishing the grant.
        let clipboard_key = if holder.is_none() {
            source.clipboard_key()?
        } else {
            None
        };
        let metadata_roots: Vec<_> = roots.iter().chain(&capture_roots).copied().collect();
        // Reused immutable bytes keep their original native descriptors too.
        // A live owner may change lazy metadata after its first fulfilment.
        let fresh_metadata_roots: Vec<_> = metadata_roots
            .iter()
            .copied()
            .filter(|root| !reusable.contains_key(&root.0))
            .collect();
        let mut native = source.native_file_metadata().restricted(&fresh_metadata_roots);
        let mut state = self.0.state.lock().unwrap();
        if !state.healthy || state.members.get(&peer) != Some(&membership) {
            return Err(unavailable(Unavailable::Permission));
        }
        // Retrying a lost grant reply must not exhaust a never-evicted journal.
        // Offer identities already include the source's durable generation.
        if let Some(token) = previous(&state) {
            self.activate(&mut state, token, active)?;
            return Ok(token);
        }
        let mut token = LinkToken([0; 32]);
        loop {
            getrandom::fill(&mut token.0).map_err(|_| io::Error::other("source link identity unavailable"))?;
            if !state.journal.grants.iter().any(|g| g.token == token) {
                break;
            }
        }
        let mut nodes = BTreeMap::new();
        for source in captured
            .into_iter()
            .filter(|root| roots.contains(&root.entry.id) && !one_shot.contains(&root.entry.id))
        {
            let id = source.entry.id.0;
            nodes.insert(
                id,
                Node {
                    root: id,
                    entry: source.entry,
                    location: source.location,
                    view: None,
                },
            );
        }
        for build in &builds {
            for (id, node) in &build.nodes {
                if nodes.insert(*id, node.clone()).is_some() {
                    return Err(unavailable(Unavailable::InvalidData));
                }
            }
        }
        for (_, reused) in reusable.values() {
            for (id, node) in reused {
                if nodes.insert(*id, node.clone()).is_some() {
                    return Err(unavailable(Unavailable::InvalidData));
                }
            }
        }
        let next = nodes
            .keys()
            .max()
            .unwrap()
            .checked_add(1)
            .ok_or_else(|| unavailable(Unavailable::LimitExceeded))?;
        let reused_roots: Vec<_> = reusable.keys().copied().map(NodeId).collect();
        if state.clipboard_confirmed
            && let Some(current) = state
                .journal
                .clipboard
                .as_ref()
                .filter(|current| Some(current.offer) == offered)
        {
            native.extend(&current.native.restricted(&reused_roots))?;
        }
        if let Some(offered) = offered {
            for grant in state
                .journal
                .grants
                .iter()
                .filter(|grant| grant.offered == Some(offered))
            {
                native.extend(&grant.native.restricted(&reused_roots))?;
            }
        }
        native.validate(&metadata_roots)?;
        let mut journal = state.journal.clone();
        let clipboard = holder.is_none()
            && state.clipboard_confirmed
            && journal
                .clipboard
                .as_ref()
                .is_some_and(|current| Some(current.offer) == offered);
        if clipboard {
            let snapshots = builds
                .iter()
                .map(|build| &build.snapshot)
                .chain(reusable.values().map(|(snapshot, _)| snapshot));
            let current = journal.clipboard.as_mut().unwrap();
            current.retain(snapshots, &nodes, &native)?;
            if clipboard_key.is_some() {
                current.key = clipboard_key;
            }
        }
        nodes.retain(|_, node| roots.contains(&NodeId(node.root)));
        journal.grants.push(Grant {
            batch,
            native: native.restricted(roots),
            offered,
            clipboard,
            token,
            holder,
            active,
            peer,
            omit_ds_store,
            safe_symlinks,
            exec_bits,
            membership,
            roots: roots.iter().map(|id| id.0).collect(),
            requested: if requested == roots {
                Vec::new()
            } else {
                requested.iter().map(|id| id.0).collect()
            },
            next,
            nodes,
            snapshots: builds
                .iter()
                .map(|build| build.snapshot.clone())
                .chain(reusable.values().map(|(snapshot, _)| snapshot.clone()))
                .filter(|snapshot| roots.contains(&NodeId(snapshot.root)))
                .map(|mut snapshot| {
                    snapshot.started = false;
                    snapshot.retired = false;
                    snapshot.deadline_ms = self.now_ms().saturating_add(crate::model::NO_PROGRESS_MS);
                    snapshot
                })
                .collect(),
        });
        self.persist(&journal)?;
        state.journal = journal;
        for build in &mut builds {
            if clipboard || roots.contains(&NodeId(build.snapshot.root)) {
                build.commit();
            }
        }
        let durable: Vec<_> = capture_roots
            .into_iter()
            .filter(|root| clipboard || roots.contains(root))
            .collect();
        drop(state);
        if !durable.is_empty() {
            source.release_fulfilled(&durable);
        }
        Ok(token)
    }
    fn activate(&self, state: &mut State, token: LinkToken, active: bool) -> io::Result<()> {
        if active
            && state
                .journal
                .grants
                .iter()
                .any(|grant| grant.token == token && !grant.active)
        {
            let mut journal = state.journal.clone();
            let grant = journal.grants.iter_mut().find(|grant| grant.token == token).unwrap();
            grant.active = true;
            // Hover time is not transfer time. The existing progress deadline
            // starts when this holder actually commits a Drop.
            for snapshot in &mut grant.snapshots {
                snapshot.deadline_ms = self.now_ms().saturating_add(crate::model::NO_PROGRESS_MS);
            }
            // No reader can observe activation before the durable commit.
            self.persist(&journal)?;
            state.journal = journal;
        }
        Ok(())
    }
    pub(super) fn discard_pending(&self, offer: oink_foundation::content::OfferId) -> io::Result<()> {
        let mut state = self.0.state.lock().unwrap();
        let snapshots = state
            .journal
            .grants
            .iter()
            .filter(|grant| !grant.active && grant.offered == Some(offer))
            .flat_map(|grant| grant.snapshots.clone())
            .collect::<Vec<_>>();
        let before = state.journal.grants.len();
        state
            .journal
            .grants
            .retain(|grant| grant.active || grant.offered != Some(offer));
        if state.journal.grants.len() != before {
            // Revoke in memory even if the durable removal fails. Startup also
            // discards every inactive record, so it cannot revive on restart.
            if let Err(error) = self.persist(&state.journal) {
                state.healthy = false;
                return Err(error);
            }
            drop(state);
            let _fulfilment = self.0.fulfilment.lock().unwrap();
            let state = self.0.state.lock().unwrap();
            fulfilled::remove_unreferenced(&self.0.path, &snapshots, &state.journal)?;
        }
        Ok(())
    }
    pub fn fulfilled_roots(&self, peer: DeviceId, token: LinkToken) -> io::Result<Vec<FulfilledRoot>> {
        let state = self.0.state.lock().unwrap();
        let grant = Self::admitted(&state, peer, token)?;
        Ok(grant
            .snapshots
            .iter()
            .filter(|snapshot| grant.safe_symlinks || grant.nodes[&snapshot.root].entry.kind.symlink_target().is_none())
            .map(|snapshot| FulfilledRoot {
                node: NodeId(snapshot.root),
                id: snapshot.id,
            })
            .collect())
    }
    fn admitted(state: &State, peer: DeviceId, token: LinkToken) -> io::Result<&Grant> {
        if !state.healthy || !state.members.contains_key(&peer) {
            return Err(unavailable(Unavailable::Permission));
        }
        state
            .journal
            .grants
            .iter()
            .find(|g| g.peer == peer && g.token == token && state.members.get(&peer) == Some(&g.membership))
            .ok_or_else(|| unavailable(Unavailable::Permission))
    }
    /// A fresh read context survives offer/process generations, but cancellation
    /// of this context and member removal still revoke each individual reader.
    pub fn open(&self, peer: DeviceId, token: LinkToken) -> io::Result<Arc<dyn Namespace>> {
        let state = self.0.state.lock().unwrap();
        let grant = Self::admitted(&state, peer, token)?;
        if grant.safe_symlinks
            && !self
                .0
                .symlink_peers
                .lock()
                .unwrap()
                .get(&peer)
                .copied()
                .unwrap_or(false)
        {
            return Err(unavailable(Unavailable::Unsupported));
        }
        if grant.exec_bits && !self.0.exec_peers.lock().unwrap().get(&peer).copied().unwrap_or(false) {
            return Err(unavailable(Unavailable::Unsupported));
        }
        let roots: Vec<_> = grant
            .roots
            .iter()
            .filter(|id| grant.safe_symlinks || grant.nodes[id].entry.kind.symlink_target().is_none())
            .map(|id| file_filter::project(grant.nodes[id].entry.clone(), grant.exec_bits))
            .collect();
        if roots.is_empty() {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        Ok(Arc::new(Context {
            native: grant
                .native
                .restricted(&roots.iter().map(|root| root.id).collect::<Vec<_>>()),
            store: self.clone(),
            peer,
            token,
            roots,
            cancelled: Arc::new(AtomicBool::new(false)),
        }))
    }
}

struct Context {
    native: NativeFileMetadata,
    store: SourceGrants,
    peer: DeviceId,
    token: LinkToken,
    roots: Vec<FileNode>,
    cancelled: Arc<AtomicBool>,
}
impl Context {
    fn valid(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(unavailable(Unavailable::Cancelled));
        }
        SourceGrants::admitted(&self.store.0.state.lock().unwrap(), self.peer, self.token).map(|_| ())
    }
}
fn resolve(grant: &Grant, id: NodeId) -> io::Result<(PathBuf, local::Fingerprint)> {
    if (!grant.safe_symlinks
        && grant
            .nodes
            .get(&id.0)
            .is_some_and(|n| n.entry.kind.symlink_target().is_some()))
        || (grant.omit_ds_store && file_filter::hidden(&grant.nodes, id.0))
    {
        return Err(unavailable(Unavailable::NoSuchItem));
    }
    let node = grant
        .nodes
        .get(&id.0)
        .ok_or_else(|| unavailable(Unavailable::Permission))?;
    if let Some(snapshot) = grant.snapshots.iter().find(|snapshot| snapshot.root == node.root) {
        if snapshot.retired {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        let path = node.location.immutable()?;
        let root = grant.nodes[&node.root].location.immutable()?;
        if id.0 != node.root && (path == root || !path.starts_with(&root)) {
            return Err(unavailable(Unavailable::Permission));
        }
        let mut cursor = path.as_path();
        while cursor != root {
            local::inspect(cursor)?;
            cursor = cursor.parent().ok_or_else(|| unavailable(Unavailable::Permission))?;
        }
        return Ok((path, node.location.fingerprint.clone()));
    }
    let path = node.location.resolve()?;
    let root = &grant.nodes[&node.root];
    let root_path = match node.location.followed.last() {
        Some(guard) => guard.target().to_path_buf(),
        None => root.location.resolve()?,
    };
    if path != root_path
        && (!path.starts_with(&root_path)
            || (node.location.followed.is_empty() && !root.location.fingerprint.directory))
    {
        return Err(unavailable(Unavailable::Permission));
    }
    // Check each scope component without following symlinks/reparse points.
    let mut cursor = path.as_path();
    loop {
        local::inspect(cursor)?;
        if cursor == root_path {
            break;
        }
        cursor = cursor.parent().ok_or_else(|| unavailable(Unavailable::Permission))?;
    }
    let fingerprint = local::inspect(&path)?;
    Ok((path, fingerprint))
}
impl Namespace for Context {
    fn native_file_metadata(&self) -> NativeFileMetadata {
        self.native.clone()
    }
    fn roots(&self) -> Vec<FileNode> {
        self.roots.clone()
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.store.abandon(self.peer, self.token);
    }
    fn list(&self, id: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        self.valid()?;
        self.store.fetch_progress(
            self.peer,
            self.token,
            id,
            1,
            cursor.map_or(0, |value| value.offset as u64) + 1,
        )?;
        let mut state = self.store.0.state.lock().unwrap();
        let grant = SourceGrants::admitted(&state, self.peer, self.token)?;
        let (path, before) = resolve(grant, id)?;
        if !before.directory {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        if grant
            .snapshots
            .iter()
            .any(|snapshot| snapshot.root == grant.nodes[&id.0].root)
        {
            let view = grant.nodes[&id.0]
                .view
                .as_ref()
                .ok_or_else(|| unavailable(Unavailable::InvalidData))?;
            return file_filter::page_for(
                grant.omit_ds_store,
                grant.safe_symlinks,
                grant.exec_bits,
                id,
                view.version,
                &view.entries,
                cursor,
            );
        }
        let version = before.version();
        if let Some(view) = &grant.nodes[&id.0].view
            && view.version == version
        {
            return file_filter::page_for(
                grant.omit_ds_store,
                grant.safe_symlinks,
                grant.exec_bits,
                id,
                version,
                &view.entries,
                cursor,
            );
        }
        if cursor.is_some() {
            return Err(unavailable(Unavailable::Superseded));
        }
        let mut ancestor = Some(id);
        let mut depth = 0;
        while let Some(id) = ancestor {
            depth += 1;
            if depth >= 256 {
                return Err(unavailable(Unavailable::LimitExceeded));
            }
            ancestor = grant.nodes[&id.0].entry.parent;
        }
        let known: std::collections::HashMap<_, _> = grant
            .nodes
            .iter()
            .filter(|(_, node)| node.entry.parent == Some(id))
            .map(|(id, node)| {
                (
                    (
                        node.location.fingerprint.identity[1],
                        node.location.fingerprint.created,
                        node.entry.name.clone(),
                    ),
                    *id,
                )
            })
            .collect();
        let mut remaining = JOURNAL_BYTES.saturating_sub(fs::metadata(&self.store.0.path)?.len());
        let mut journal = state.journal.clone();
        let grant = journal.grants.iter_mut().find(|g| g.token == self.token).unwrap();
        let root = grant.nodes[&id.0].root;
        for root in &grant.roots {
            let node = &grant.nodes[root];
            if node.entry.kind.symlink_target().is_none()
                && let Ok(path) = node.location.resolve()
                && let Ok(fingerprint) = local::inspect(&path)
            {
                grant.batch.include(&path, vec![node.entry.name.clone()], &fingerprint);
            }
        }
        let inherited = grant.nodes[&id.0].location.followed.clone();
        let mut logical = Vec::new();
        let mut ancestor = Some(id);
        while let Some(current) = ancestor {
            logical.push(grant.nodes[&current.0].entry.name.clone());
            ancestor = grant.nodes[&current.0].entry.parent;
        }
        logical.reverse();
        let mut entries = Vec::new();
        for (name, path) in local::batch::children(&path)? {
            if self.cancelled.load(Ordering::Acquire) {
                return Err(unavailable(Unavailable::Cancelled));
            }
            let mut child_path = logical.clone();
            child_path.push(name.clone());
            let item = match grant.batch.capture(&path, child_path, &inherited) {
                Ok(Some(item)) => item,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(reason=?error.kind(), "skipping unavailable granted item");
                    continue;
                }
            };
            let target = item.link;
            let location = Location::capture_followed(&item.path, item.followed)?;
            let existing = known
                .get(&(
                    location.fingerprint.identity[1],
                    location.fingerprint.created,
                    name.clone(),
                ))
                .copied();
            let child_id = match existing {
                Some(id) => id,
                None => {
                    let metadata_size = serde_json::to_vec(&location)
                        .map_err(|_| unavailable(Unavailable::InvalidData))?
                        .len() as u64
                        + name.len() as u64
                        + target.as_ref().map_or(0, String::len) as u64
                        + 512;
                    remaining = remaining
                        .checked_sub(metadata_size)
                        .ok_or_else(|| unavailable(Unavailable::LimitExceeded))?;
                    if grant.nodes.len() >= crate::model::MAX_DISCOVERED_NODES {
                        return Err(unavailable(Unavailable::LimitExceeded));
                    }
                    let id = grant.next;
                    grant.next = id
                        .checked_add(1)
                        .ok_or_else(|| unavailable(Unavailable::LimitExceeded))?;
                    id
                }
            };
            let mut entry = local::entry(NodeId(child_id), Some(id), name, &location.fingerprint)?;
            if let Some(target) = target {
                local::set_link(&mut entry, target);
            }
            let view = grant.nodes.get(&child_id).and_then(|n| n.view.clone());
            grant.nodes.insert(
                child_id,
                Node {
                    root,
                    entry: entry.clone(),
                    location,
                    view,
                },
            );
            entries.push(entry);
        }
        if local::inspect(&path)? != before {
            return Err(unavailable(Unavailable::Superseded));
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let page = file_filter::page_for(
            grant.omit_ds_store,
            grant.safe_symlinks,
            grant.exec_bits,
            id,
            version,
            &entries,
            None,
        )?;
        let directory = grant.nodes.get_mut(&id.0).unwrap();
        directory.location = directory.location.refresh(&path)?;
        directory.view = Some(View { version, entries });
        self.store.persist(&journal)?;
        state.journal = journal;
        Ok(page)
    }
    fn select(&self, id: NodeId) -> io::Result<FileNode> {
        self.valid()?;
        let mut state = self.store.0.state.lock().unwrap();
        let grant = SourceGrants::admitted(&state, self.peer, self.token)?;
        if grant
            .nodes
            .get(&id.0)
            .is_some_and(|node| node.entry.kind.symlink_target().is_some())
        {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        let (path, fingerprint) = resolve(grant, id)?;
        if fingerprint.directory {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        let node = &grant.nodes[&id.0];
        if grant.snapshots.iter().any(|snapshot| snapshot.root == node.root) {
            return Ok(file_filter::project(node.entry.clone(), grant.exec_bits));
        }
        let exec = grant.exec_bits;
        let entry = local::entry(id, node.entry.parent, node.entry.name.clone(), &fingerprint)?;
        if !node.location.current(&path, &fingerprint) || node.entry != entry {
            let location = node.location.refresh(&path)?;
            if location.fingerprint != fingerprint {
                return Err(unavailable(Unavailable::Superseded));
            }
            let mut journal = state.journal.clone();
            let node = journal
                .grants
                .iter_mut()
                .find(|g| g.token == self.token)
                .unwrap()
                .nodes
                .get_mut(&id.0)
                .unwrap();
            node.location = location;
            node.entry = entry.clone();
            self.store.persist(&journal)?;
            state.journal = journal;
        }
        Ok(file_filter::project(entry, exec))
    }
    fn open(&self, id: NodeId, version: u64, offset: u64) -> io::Result<FileReader> {
        if self.select(id)?.version != version {
            return Err(unavailable(Unavailable::Superseded));
        }
        self.store.fetch_progress(self.peer, self.token, id, 0, 0)?;
        let state = self.store.0.state.lock().unwrap();
        let grant = SourceGrants::admitted(&state, self.peer, self.token)?;
        if !grant.active {
            return Err(unavailable(Unavailable::Permission));
        }
        let (path, selected) = resolve(grant, id)?;
        let node = &grant.nodes[&id.0];
        let owned = grant.snapshots.iter().any(|snapshot| snapshot.root == node.root);
        if selected.directory
            || if owned {
                node.entry.version != version
            } else {
                selected.version() != version
            }
        {
            return Err(unavailable(Unavailable::Superseded));
        }
        if offset > selected.size {
            return Err(unavailable(Unavailable::InvalidData));
        }
        let mut file = File::open(&path)?;
        if local::file_fingerprint(&file)? != selected || resolve(grant, id)?.1 != selected {
            return Err(unavailable(Unavailable::Superseded));
        }
        file.seek(SeekFrom::Start(offset))?;
        Ok(super::terminal_reader(Box::new(Reader {
            file,
            selected,
            store: self.store.clone(),
            peer: self.peer,
            token: self.token,
            root: node.root,
            node: id,
            position: offset,
            scope: (grant.nodes[&id.0].root != id.0 || !node.location.followed.is_empty()).then(|| {
                (
                    grant.nodes[&id.0].location.clone(),
                    grant.nodes[&grant.nodes[&id.0].root].location.clone(),
                )
            }),
            cancelled: self.cancelled.clone(),
        })))
    }
}
struct Reader {
    file: File,
    selected: local::Fingerprint,
    store: SourceGrants,
    peer: DeviceId,
    token: LinkToken,
    root: u64,
    node: NodeId,
    position: u64,
    scope: Option<(Location, Location)>,
    cancelled: Arc<AtomicBool>,
}
impl Reader {
    fn check(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(unavailable(Unavailable::Cancelled));
        }
        let state = self.store.0.state.lock().unwrap();
        let grant = SourceGrants::admitted(&state, self.peer, self.token)?;
        if !grant.active {
            return Err(unavailable(Unavailable::Permission));
        }
        if grant
            .snapshots
            .iter()
            .any(|snapshot| snapshot.root == self.root && (snapshot.retired))
        {
            return Err(unavailable(Unavailable::NoSuchItem));
        }
        // Authority is checked live, while version belongs to the opened handle,
        // not a later grant rebind caused by an editor's atomic replacement.
        if local::file_fingerprint(&self.file)? != self.selected {
            return Err(unavailable(Unavailable::Superseded));
        }
        if let Some((location, root)) = &self.scope {
            let path = location.resolve()?;
            let selected_root = root.resolve()?;
            let boundary = location
                .followed
                .last()
                .map_or(selected_root.as_path(), |guard| guard.target());
            if !path.starts_with(boundary) || (location.followed.is_empty() && path == boundary) {
                return Err(unavailable(Unavailable::Permission));
            }
            let mut cursor = path.as_path();
            while cursor != boundary {
                local::inspect(cursor)?;
                cursor = cursor.parent().ok_or_else(|| unavailable(Unavailable::Permission))?;
            }
        }
        Ok(())
    }
}
impl Read for Reader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        let count = self.file.read(bytes)?;
        self.check()?;
        self.position = self.position.saturating_add(count as u64);
        if count != 0 {
            self.store
                .fetch_progress(self.peer, self.token, self.node, 0, self.position)?;
        }
        Ok(count)
    }
}

#[cfg(all(test, debug_assertions))]
mod tests;

#[cfg(all(test, debug_assertions))]
mod ds_store_tests;

#[cfg(all(test, debug_assertions))]
mod drag_tests;
