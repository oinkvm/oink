//! Desktop window and portable payload store. The journal is atomically replaced;
//! payloads become visible only after a complete representation has been written.
use crate::{
    content::{self, Provider, namespace::Namespace},
    model::*,
    retained::RetainedClips,
};
use oink_foundation::{DeviceId, GroupId, clipboard::Sensitivity, clipboard_history as h};
use oink_protocol::v2::{dto, schema::WireDto};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
#[derive(Clone, Serialize, Deserialize)]
struct Payload {
    id: OfferId,
    inventory: Vec<u8>,
    proxy: Option<PathBuf>,
    files_gone: bool,
    refs: Vec<crate::tracked_file::TrackedFile>,
    head_only: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Journal {
    schema: u8,
    group: GroupId,
    clock: u64,
    window: h::HistoryWindow,
    payloads: Vec<Payload>,
    retired: Vec<Payload>,
}
pub struct Store {
    path: PathBuf,
    me: DeviceId,
    state: Arc<Mutex<Journal>>,
    pub bytes: Arc<RetainedClips>,
    policy: crate::policy::SharingPolicy,
}
impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardHistoryStore").finish_non_exhaustive()
    }
}
impl Store {
    pub fn load(
        data: &Path,
        cache: &Path,
        group: GroupId,
        me: DeviceId,
        policy: crate::policy::SharingPolicy,
    ) -> io::Result<Arc<Self>> {
        let path = data.join("clipboard-history.json");
        let mut state = Journal {
            schema: 1,
            group,
            clock: 0,
            window: h::HistoryWindow::new(),
            payloads: vec![],
            retired: vec![],
        };
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Journal>(&bytes) {
                Ok(saved) if saved.schema == 1 => state = saved,
                _ => tracing::warn!("unrecognized clipboard history discarded"),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let store = Arc::new(Self {
            path,
            me,
            state: Arc::new(Mutex::new(state)),
            bytes: Arc::new(RetainedClips::history(cache)),
            policy,
        });
        store.reset_group(group)?;
        {
            let mut state = store.state.lock().unwrap();
            let valid: HashSet<_> = state.window.records().iter().map(|r| r.id).collect();
            let mut keep = vec![];
            for payload in std::mem::take(&mut state.payloads) {
                if valid.contains(&payload.id) {
                    keep.push(payload);
                } else {
                    state.retired.push(payload);
                }
            }
            state.payloads = keep;
            let ids = state
                .payloads
                .iter()
                .chain(&state.retired)
                .map(|p| p.id)
                .collect::<Vec<_>>();
            store.bytes.retain_offers(&ids);
            store.save(&state)?;
        }
        // Revalidate materialized state instead of advertising vanished cache bytes.
        let owned: Vec<_> = store
            .window()
            .holders()
            .iter()
            .filter(|e| e.holder == me && e.state == h::HolderState::Holding)
            .cloned()
            .collect();
        for entry in owned {
            if store.scope(entry.record_id).0 == 0 {
                store.holder(entry.record_id, h::HolderState::Gone, h::HolderScope(0))?;
            }
        }
        Ok(store)
    }
    fn save(&self, state: &Journal) -> io::Result<()> {
        oink_foundation::storage::atomic_write(&self.path, &serde_json::to_vec(state)?)
    }
    pub fn contains(&self, id: OfferId) -> bool {
        self.state.lock().unwrap().window.get(id).is_some()
    }
    pub fn head_key(&self) -> Option<h::OrderKey> {
        self.state
            .lock()
            .unwrap()
            .window
            .records()
            .first()
            .map(h::ClipRecord::key)
    }
    pub fn window(&self) -> h::HistoryWindow {
        self.state.lock().unwrap().window.clone()
    }
    pub fn group(&self) -> GroupId {
        self.state.lock().unwrap().group
    }
    pub fn clock(&self) -> u64 {
        self.state.lock().unwrap().clock
    }
    pub fn witness(&self, clock: u64) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if clock > state.clock {
            state.clock = clock;
            self.save(&state)?;
        }
        Ok(())
    }
    pub fn reset_group(&self, group: GroupId) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.group == group {
            return Ok(());
        }
        let payloads = std::mem::take(&mut state.payloads);
        state.retired.extend(payloads);
        state.retired.retain(|payload| !self.remove_payload(payload));
        state.window = h::HistoryWindow::new();
        state.group = group;
        self.save(&state)
    }
    pub fn merge(&self, records: Vec<h::ClipRecord>, holders: Vec<h::HolderEntry>) -> io::Result<Vec<OfferId>> {
        self.merge_in(self.group(), records, holders)
    }
    pub fn merge_in(
        &self,
        group: GroupId,
        records: Vec<h::ClipRecord>,
        holders: Vec<h::HolderEntry>,
    ) -> io::Result<Vec<OfferId>> {
        let mut state = self.state.lock().unwrap();
        if state.group != group {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let mut next = state.clone();
        for r in &records {
            next.clock = next.clock.max(r.clock);
        }
        let evicted = next.window.merge(records, holders).map_err(io::Error::other)?;
        let mut kept = vec![];
        for payload in std::mem::take(&mut next.payloads) {
            if evicted.contains(&payload.id) {
                next.retired.push(payload);
            } else {
                kept.push(payload);
            }
        }
        next.payloads = kept;
        self.save(&next)?;
        next.retired.retain(|payload| !self.remove_payload(payload));
        for id in &evicted {
            let _ = self.bytes.forget(*id);
        }
        self.save(&next)?;
        *state = next;
        Ok(evicted)
    }
    pub fn cleanup(&self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.retired.is_empty() {
            return Ok(());
        }
        state.retired.retain(|payload| !self.remove_payload(payload));
        self.save(&state)
    }
    pub fn retain_pc_holders(&self, members: &HashSet<DeviceId>) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.window.retain_pc_holders(members);
        self.save(&state)
    }
    pub fn holder(
        &self,
        id: OfferId,
        status: h::HolderState,
        scope: h::HolderScope,
    ) -> io::Result<Option<h::HolderEntry>> {
        let mut state = self.state.lock().unwrap();
        if state.window.get(id).is_none() {
            return Ok(None);
        }
        let old = state
            .window
            .holders()
            .iter()
            .find(|e| e.record_id == id && e.holder == self.me);
        if old.is_some_and(|e| e.state == status && e.scope == scope) {
            return Ok(None);
        }
        let rev = old
            .map_or(Some(1), |e| e.rev.checked_add(1))
            .ok_or(io::ErrorKind::InvalidData)?;
        let entry = h::HolderEntry {
            record_id: id,
            holder: self.me,
            rev,
            state: status,
            scope,
        };
        let mut next = state.clone();
        next.window.merge([], [entry.clone()]).map_err(io::Error::other)?;
        self.save(&next)?;
        *state = next;
        Ok(Some(entry))
    }
    fn remove_payload(&self, payload: &Payload) -> bool {
        let bytes = if payload.head_only {
            self.bytes.forget_sensitive(payload.id)
        } else {
            self.bytes.forget(payload.id)
        }
        .unwrap_or(false);
        let proxy = payload.proxy.as_ref().is_none_or(|path| {
            !path.exists()
                || if payload.head_only {
                    // Only the Oink-owned upload tree is deleted; tracked user
                    // destinations are outside it. Sensitive readers are revoked.
                    std::fs::remove_dir_all(path).is_ok()
                } else {
                    crate::cache::remove_unless_active(path).unwrap_or(false)
                }
        });
        bytes && proxy
    }
    /// Cache every portable representation, even when no phone is in the group.
    /// The caller runs this on a blocking worker, before losing the native slot.
    pub fn capture(
        &self,
        group: GroupId,
        provider: &dyn Provider,
        sensitivity: Sensitivity,
        proxy: Option<PathBuf>,
    ) -> io::Result<()> {
        if matches!(sensitivity, Sensitivity::Concealed | Sensitivity::Transient) && proxy.is_none() {
            return Ok(());
        }
        let mut inventory = provider.inventory();
        if self.window().get(inventory.id).is_none() {
            return Ok(());
        }
        for item in &mut inventory.items {
            for rep in &mut item.representations {
                if portable(rep.standard).is_none() || (item.file.is_some() && !item.virtual_files) {
                    rep.data = RepresentationData::Unavailable(Unavailable::Superseded);
                    continue;
                }
                if self.bytes.contains(inventory.id, item.id.0, rep.id.0) {
                    continue;
                }
                let result = content::open_portable(provider, rep.id)
                    .and_then(|mut reader| self.bytes.retain(inventory.id, item.id.0, rep.id.0, &mut reader));
                match result {
                    Ok(size) => {
                        rep.data = RepresentationData::Reader {
                            size: ContentSize::Known(size),
                        }
                    }
                    Err(e) => rep.data = RepresentationData::Unavailable(content::reason(&e)),
                }
            }
        }
        let refs = provider
            .namespace()
            .map(|files| {
                files
                    .roots()
                    .iter()
                    .filter_map(|r| files.tracked_file(r.id).ok().flatten())
                    .collect()
            })
            .unwrap_or_default();
        self.bind_in(group, &inventory, proxy, Some(refs))
    }
    pub fn materialized(
        &self,
        group: GroupId,
        inventory: &Inventory,
        rep: RepresentationId,
        reader: &mut dyn Read,
    ) -> io::Result<()> {
        // Metadata may arrive after an automatic native download. The owner
        // replays completed in-memory bytes when that record joins the window.
        // Never create orphan disk payloads for unknown or evicted records.
        if !self.contains(inventory.id) {
            return Ok(());
        }
        let Some(item) = inventory.items.iter().find(|i| {
            i.file.is_none()
                && i.representations
                    .iter()
                    .any(|r| r.id == rep && portable(r.standard).is_some())
        }) else {
            return Ok(());
        };
        self.bytes.retain(inventory.id, item.id.0, rep.0, reader)?;
        // A downloaded bitmap is a holder for bitmap bytes, not for its
        // neighbouring file attachment. Only origins/proxies hold file roots.
        let mut portable = inventory.clone();
        portable.roots.clear();
        portable.items.retain(|item| item.file.is_none());
        self.bind_in(group, &portable, None, None)
    }
    pub fn bind(&self, inventory: &Inventory, proxy: Option<PathBuf>) -> io::Result<()> {
        self.bind_in(self.group(), inventory, proxy, None)
    }
    fn bind_in(
        &self,
        group: GroupId,
        inventory: &Inventory,
        proxy: Option<PathBuf>,
        refs: Option<Vec<crate::tracked_file::TrackedFile>>,
    ) -> io::Result<()> {
        let mut canonical = inventory.clone();
        for item in &mut canonical.items {
            for rep in &mut item.representations {
                if let Some(format) = portable(rep.standard) {
                    rep.format = NativeFormat::Unknown(crate::wire::portable(standard(format)).0);
                    rep.medium = Medium::Bytes;
                    if let Some((_, size)) = self.bytes.open(inventory.id, item.id.0, rep.id.0)? {
                        rep.data = RepresentationData::Reader {
                            size: ContentSize::Known(size),
                        };
                    } else {
                        rep.data = RepresentationData::Unavailable(Unavailable::NoSuchItem);
                    }
                } else {
                    rep.data = RepresentationData::Unavailable(Unavailable::Superseded);
                }
            }
        }
        let metadata = crate::wire::inventory(&canonical)
            .and_then(|d| d.encode(oink_protocol::v2::limits::LOW_FRAME))
            .map_err(io::Error::other)?;
        let mut state = self.state.lock().unwrap();

        if state.group != group {
            let _ = self.bytes.forget(inventory.id);
            return Err(io::ErrorKind::Interrupted.into());
        }
        if state.window.get(inventory.id).is_none() {
            let _ = self.bytes.forget(inventory.id);
            return Ok(());
        }
        let previous = state.payloads.iter().find(|p| p.id == inventory.id).cloned();
        let proxy = proxy.or_else(|| previous.as_ref().and_then(|p| p.proxy.clone()));
        let head_only = state.window.get(inventory.id).is_some_and(h::ClipRecord::sensitive);
        let refs = refs.unwrap_or_else(|| previous.as_ref().map(|p| p.refs.clone()).unwrap_or_default());
        let files_gone = previous.is_some_and(|p| p.files_gone);
        state.payloads.retain(|p| p.id != inventory.id);
        state.payloads.push(Payload {
            id: inventory.id,
            inventory: metadata,
            proxy,
            files_gone,
            refs,
            head_only,
        });
        self.save(&state)
    }
    pub fn ingest_inline(&self, record: &h::ClipRecord) -> io::Result<()> {
        if self
            .window()
            .holders()
            .iter()
            .any(|e| e.record_id == record.id && e.holder == self.me && e.state != h::HolderState::Holding)
        {
            return Ok(());
        }
        if self.window().get(record.id).is_none() || record.inline.is_empty() {
            return Ok(());
        }
        let mut items = vec![];
        for i in &record.items {
            let mut reps = vec![];
            for f in record.formats.iter().filter(|f| f.item == i.id) {
                let Some(format) = f.portable.map(standard) else {
                    continue;
                };
                let payload = record
                    .inline
                    .iter()
                    .find(|p| p.item == i.id && p.representation == f.representation);
                let data = if let Some(p) = payload {
                    self.bytes
                        .retain(record.id, i.id, f.representation, &mut io::Cursor::new(&p.bytes))?;
                    RepresentationData::Reader {
                        size: ContentSize::Known(p.bytes.len() as u64),
                    }
                } else {
                    RepresentationData::Unavailable(Unavailable::NoSuchItem)
                };
                reps.push(Representation {
                    id: RepresentationId(f.representation),
                    format: NativeFormat::Unknown(crate::wire::portable(format).0),
                    medium: Medium::Bytes,
                    aspect: 1,
                    index: -1,
                    standard: Some(format),
                    data,
                });
            }
            if !reps.is_empty() {
                items.push(ContentItem {
                    id: ItemId(i.id),
                    file: None,
                    virtual_files: false,
                    representations: reps,
                });
            }
        }
        if items.is_empty() {
            return Ok(());
        }
        if self.state.lock().unwrap().payloads.iter().any(|p| p.id == record.id) {
            return Ok(());
        }
        self.bind(
            &Inventory {
                id: record.id,
                platform: oink_foundation::clipboard::ClipPlatform::Unknown,
                items,
                roots: vec![],
            },
            None,
        )
    }
    pub fn scope(&self, id: OfferId) -> h::HolderScope {
        // Holding describes resident data, independently of sharing/pause.
        // Never inspect a policy-filtered inventory here: it is empty paused.
        let inventory = {
            let state = self.state.lock().unwrap();
            let Some(payload) = state
                .payloads
                .iter()
                .find(|p| p.id == id && state.window.get(id).is_some())
            else {
                return h::HolderScope(0);
            };
            let Ok(raw) = oink_protocol::v2::cbor::decode_map(&payload.inventory, oink_protocol::v2::limits::LOW_FRAME)
            else {
                return h::HolderScope(0);
            };
            let Ok(dto) = dto::Inventory::from_raw(raw) else {
                return h::HolderScope(0);
            };
            let Ok(inventory) = crate::wire::local_inventory(&dto) else {
                return h::HolderScope(0);
            };
            inventory
        };
        let mut scope = 0;
        for item in &inventory.items {
            for rep in &item.representations {
                if self.bytes.contains(id, item.id.0, rep.id.0)
                    && let Some(p) = portable(rep.standard)
                {
                    scope |= 1 << p as u32;
                }
            }
        }
        let state = self.state.lock().unwrap();
        let files_present = state
            .payloads
            .iter()
            .find(|p| p.id == id)
            .is_some_and(|p| !p.files_gone && p.proxy.as_ref().is_none_or(|path| path.exists()));
        if files_present && !inventory.roots.is_empty() {
            scope |= h::HolderScope::FILES.0;
        }
        h::HolderScope(scope)
    }
    pub fn provider(&self, id: OfferId, files: Option<Arc<dyn Namespace>>) -> Option<Arc<dyn Provider>> {
        self.local_provider(id, files)
            .map(|provider| self.policy.provider(provider))
    }
    /// Locally owned bytes, for a native receive guarded by the receive policy.
    /// Network serving additionally applies the PC's independent send policy.
    pub fn local_provider(&self, id: OfferId, files: Option<Arc<dyn Namespace>>) -> Option<Arc<dyn Provider>> {
        let state = self.state.lock().unwrap();
        state.window.get(id)?;
        let payload = state.payloads.iter().find(|p| p.id == id)?;
        let raw = oink_protocol::v2::cbor::decode_map(&payload.inventory, oink_protocol::v2::limits::LOW_FRAME).ok()?;
        let dto = dto::Inventory::from_raw(raw).ok()?;
        let inventory = crate::wire::local_inventory(&dto).ok()?;
        Some(Arc::new(Cached {
            inventory,
            bytes: self.bytes.clone(),
            state: self.state.clone(),
            files,
        }))
    }
    pub fn confirmed_missing(&self, id: OfferId, files: bool) -> io::Result<Option<h::HolderEntry>> {
        if files {
            let mut state = self.state.lock().unwrap();
            if let Some(p) = state.payloads.iter_mut().find(|p| p.id == id) {
                p.files_gone = true;
                self.save(&state)?;
            }
        }
        let scope = self.scope(id);
        if scope.0 == 0 {
            self.holder(id, h::HolderState::Gone, scope)
        } else if files {
            self.holder(id, h::HolderState::Holding, scope)
        } else {
            Ok(None)
        }
    }
    pub fn proxy_retained(&self, path: &Path) -> bool {
        self.state
            .lock()
            .unwrap()
            .payloads
            .iter()
            .any(|p| p.proxy.as_deref() == Some(path))
    }
    /// Metadata remains. Protect newest ten; reclaim remaining payloads largest first.
    pub fn enforce_cap(&self, cap: u64) -> io::Result<Vec<h::HolderEntry>> {
        let candidates = {
            let state = self.state.lock().unwrap();
            let mut list = vec![];
            for (rank, r) in state.window.records().iter().enumerate() {
                let proxy = state
                    .payloads
                    .iter()
                    .find(|p| p.id == r.id)
                    .and_then(|p| p.proxy.clone());
                let references = state
                    .payloads
                    .iter()
                    .find(|p| p.id == r.id)
                    .map(|p| p.refs.as_slice())
                    .unwrap_or_default();
                let external = proxy.as_ref().is_some_and(|path| {
                    !references.is_empty()
                        && references
                            .iter()
                            .all(|reference| reference.resolve().is_ok_and(|target| !target.starts_with(path)))
                });
                let size = self.bytes.bytes(r.id).saturating_add(
                    proxy
                        .as_ref()
                        .and_then(|p| {
                            if external {
                                proxy_representation_bytes(p).ok()
                            } else {
                                data_bytes(p).ok()
                            }
                        })
                        .unwrap_or(0),
                );
                list.push((r.id, rank, size, proxy, external));
            }
            list
        };
        let mut total: u64 = candidates.iter().map(|c| c.2).sum();
        let mut candidates = candidates;
        candidates.sort_by_key(|c| std::cmp::Reverse(c.2));
        let mut released = vec![];
        for (id, rank, size, proxy, external) in candidates {
            if total <= cap {
                break;
            }
            if rank < 10 || size == 0 {
                continue;
            }
            if !external
                && let Some(path) = &proxy
                && path.exists()
                && !crate::cache::remove_unless_active(path)?
            {
                continue;
            }
            if !self.bytes.forget(id)? {
                continue;
            }
            total = total.saturating_sub(size);
            let keep_files = proxy.is_none() && self.scope(id).0 & h::HolderScope::FILES.0 != 0;
            {
                let mut state = self.state.lock().unwrap();
                if external {
                    if let Some(path) = &proxy {
                        release_proxy_representations(path)?;
                    }
                } else if !keep_files {
                    state.payloads.retain(|p| p.id != id);
                }
                self.save(&state)?;
            }
            let scope = self.scope(id);
            let status = if scope.0 == 0 {
                h::HolderState::Released
            } else {
                h::HolderState::Holding
            };
            if let Some(e) = self.holder(id, status, scope)? {
                released.push(e);
            }
        }
        Ok(released)
    }
}
struct Cached {
    state: Arc<Mutex<Journal>>,
    inventory: Inventory,
    bytes: Arc<RetainedClips>,
    files: Option<Arc<dyn Namespace>>,
}
impl Provider for Cached {
    fn reader_guard(&self, _: RepresentationId) -> io::Result<content::ReaderGuard> {
        let state = self.state.clone();
        let id = self.inventory.id;
        Ok(Arc::new(move || {
            if state.lock().unwrap().window.get(id).is_some() {
                Ok(())
            } else {
                Err(content::unavailable(Unavailable::NoSuchItem))
            }
        }))
    }
    fn inventory(&self) -> Inventory {
        self.inventory.clone()
    }
    fn open(&self, id: RepresentationId) -> io::Result<crate::FileReader> {
        let item = self
            .inventory
            .items
            .iter()
            .find(|i| i.representations.iter().any(|r| r.id == id))
            .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))?;
        self.bytes
            .open(self.inventory.id, item.id.0, id.0)?
            .map(|(r, _)| Box::new(r) as crate::FileReader)
            .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))
    }
    fn translator(&self) -> Arc<dyn content::Translator> {
        Arc::new(Portable)
    }
    fn namespace(&self) -> Option<Arc<dyn Namespace>> {
        self.files.clone()
    }
}
struct Portable;
impl content::Translator for Portable {
    fn standard(&self, rep: &Representation, bytes: &[u8]) -> io::Result<Vec<oink_foundation::clipboard::ClipData>> {
        content::decode_standard(rep, bytes)
    }
}
pub fn portable(format: Option<StandardFormat>) -> Option<h::PortableFormat> {
    use StandardFormat as S;
    use h::PortableFormat as P;
    match format? {
        S::Text => Some(P::Text),
        S::Html => Some(P::Html),
        S::Rtf => Some(P::Rtf),
        S::Png => Some(P::Png),
        S::Url => Some(P::Url),
        _ => None,
    }
}
pub fn standard(format: h::PortableFormat) -> StandardFormat {
    use h::PortableFormat as P;
    match format {
        P::Text => StandardFormat::Text,
        P::Html => StandardFormat::Html,
        P::Rtf => StandardFormat::Rtf,
        P::Png => StandardFormat::Png,
        P::Url => StandardFormat::Url,
    }
}

/// Eviction ranks data length, not filesystem allocation units: two tiny records
/// can occupy equal disk blocks yet have different payload sizes.
pub(crate) fn data_bytes(path: &Path) -> io::Result<u64> {
    let meta = std::fs::symlink_metadata(path)?;
    if crate::cache::is_link(&meta) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    if !meta.is_dir() {
        return Ok(meta.len());
    }
    std::fs::read_dir(path)?.try_fold(0u64, |sum, entry| Ok(sum.saturating_add(data_bytes(&entry?.path())?)))
}

fn proxy_representation_bytes(root: &Path) -> io::Result<u64> {
    std::fs::read_dir(root)?.try_fold(0u64, |sum, entry| {
        let entry = entry?;
        Ok(sum
            + if entry.file_name().to_string_lossy().starts_with("r-") {
                data_bytes(&entry.path())?
            } else {
                0
            })
    })
}
fn release_proxy_representations(root: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with("r-") {
            crate::cache::remove_file_unless_active(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::files::{BoxFuture, BulkChannel};
    use oink_foundation::clipboard::ClipPlatform;
    fn record(seq: u64) -> h::ClipRecord {
        let id = OfferId {
            clip: ClipId {
                origin: DeviceId([1; 32]),
                seq,
            },
            generation: seq,
        };
        h::ClipRecord {
            id,
            clock: seq,
            time_ms: 0,
            origin: id.clip.origin,
            proxy: None,
            sensitivity: Sensitivity::Normal,
            items: vec![h::HistoryItem {
                id: 0,
                kind: h::ItemKind::Text,
                mime: "text/plain".into(),
                name: None,
                bytes: None,
                dimensions: None,
                child_count: None,
                children_bytes: None,
                file_node: None,
                file_version: None,
            }],
            formats: vec![h::HistoryFormat {
                item: 0,
                representation: 1,
                name: "text/plain".into(),
                portable: Some(h::PortableFormat::Text),
                bytes: None,
                file_node: None,
            }],
            digests: vec![],
            inline: vec![],
            preview: None,
            rev: 0,
        }
    }
    fn store(root: &Path) -> Arc<Store> {
        Store::load(
            root,
            &root.join("cache"),
            GroupId([1; 16]),
            DeviceId([1; 32]),
            crate::policy::SharingPolicy::new(true),
        )
        .unwrap()
    }
    fn inline(r: &mut h::ClipRecord, n: usize) {
        r.inline = vec![h::InlinePayload {
            item: 0,
            representation: 1,
            format: h::PortableFormat::Text,
            bytes: vec![b'x'; n],
        }];
    }
    #[test]
    fn window_payload_holders_and_lamport_survive_restart_and_eviction() {
        let root = tempfile::tempdir().unwrap();
        let s = store(root.path());
        let mut r = record(1);
        inline(&mut r, 12);
        s.merge(vec![r.clone()], vec![]).unwrap();
        s.ingest_inline(&r).unwrap();
        let first = s.holder(r.id, h::HolderState::Holding, s.scope(r.id)).unwrap().unwrap();
        s.witness(1000).unwrap();
        drop(s);
        let s = store(root.path());
        assert_eq!(s.clock(), 1000);
        assert_eq!(s.window().holders(), &[first]);
        let p = s.provider(r.id, None).unwrap();
        assert_eq!(
            content::read_all(&mut p.open(RepresentationId(1)).unwrap()).unwrap(),
            vec![b'x'; 12]
        );
        let second = s
            .holder(r.id, h::HolderState::Released, h::HolderScope(0))
            .unwrap()
            .unwrap();
        assert_eq!(second.rev, 2);
        s.merge((2..=101).map(record).collect(), vec![]).unwrap();
        assert!(s.provider(r.id, None).is_none());
        assert_eq!(s.bytes.bytes(r.id), 0);
        assert_eq!(s.window().records().len(), 100);
    }
    #[test]
    fn cap_protects_newest_ten_and_releases_largest_remaining_payload() {
        let root = tempfile::tempdir().unwrap();
        let s = store(root.path());
        for seq in 1..=12 {
            let mut r = record(seq);
            inline(
                &mut r,
                if seq == 1 {
                    80
                } else if seq == 2 {
                    20
                } else {
                    5
                },
            );
            s.merge(vec![r.clone()], vec![]).unwrap();
            s.ingest_inline(&r).unwrap();
            s.holder(r.id, h::HolderState::Holding, s.scope(r.id)).unwrap();
        }
        let total: u64 = (1..=12).map(|n| s.bytes.bytes(record(n).id)).sum();
        let events = s.enforce_cap(total - 70).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].record_id, record(1).id);
        assert_eq!(events[0].state, h::HolderState::Released);
        assert_eq!(s.window().records().len(), 12);
        assert!(s.provider(record(1).id, None).is_none());
        for seq in 2..=12 {
            assert!(s.provider(record(seq).id, None).is_some());
        }
    }
    #[test]
    fn active_reader_defers_cap_eviction_without_forging_a_release() {
        let root = tempfile::tempdir().unwrap();
        let s = store(root.path());
        for seq in 1..=11 {
            let mut r = record(seq);
            inline(&mut r, 10);
            s.merge(vec![r.clone()], vec![]).unwrap();
            s.ingest_inline(&r).unwrap();
            s.holder(r.id, h::HolderState::Holding, s.scope(r.id)).unwrap();
        }
        let reader = s
            .provider(record(1).id, None)
            .unwrap()
            .open(RepresentationId(1))
            .unwrap();
        assert!(s.enforce_cap(0).unwrap().is_empty());
        drop(reader);
        let released = s.enforce_cap(0).unwrap();
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].record_id, record(1).id);
    }
    struct Sink(Vec<ContentMsg>);
    impl BulkChannel for Sink {
        fn send<'a>(&'a mut self, m: &'a ContentMsg) -> BoxFuture<'a, io::Result<()>> {
            self.0.push(m.clone());
            Box::pin(async { Ok(()) })
        }
        fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<ContentMsg>>> {
            Box::pin(std::future::pending())
        }
        fn finish(&mut self) {}
    }
    #[tokio::test]
    async fn registry_serves_portable_history_after_current_registration_is_gone() {
        let root = tempfile::tempdir().unwrap();
        let s = store(root.path());
        let mut r = record(1);
        inline(&mut r, 17);
        s.merge(vec![r.clone()], vec![]).unwrap();
        s.ingest_inline(&r).unwrap();
        let registry = Arc::new(content::Registry::default());
        registry.set_history(s.clone());
        let mut channel = Sink(vec![]);
        registry
            .serve(
                DeviceId([2; 32]),
                ContentMsg::ReadRepresentation {
                    offer: r.id,
                    representation: RepresentationId(1),
                    item: ItemId(0),
                    accepted_format: crate::negotiation::portable_format(StandardFormat::Text).into(),
                    target: ClipPlatform::Mac,
                    demand: Demand::Clipboard,
                },
                &mut channel,
            )
            .await
            .unwrap();
        let bytes: Vec<_> = channel
            .0
            .into_iter()
            .filter_map(|m| match m {
                ContentMsg::RepresentationBytes { bytes, .. } => Some(bytes),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(bytes, vec![b'x'; 17]);
        s.reset_group(GroupId([2; 16])).unwrap();
        assert!(s.window().records().is_empty());
        assert!(s.provider(r.id, None).is_none());
        assert_eq!(s.bytes.bytes(r.id), 0);
    }
    #[test]
    #[cfg(any(target_os = "macos", windows))]
    fn cap_keeps_moved_phone_holder_reference_until_window_eviction() {
        use content::namespace::{Local, Namespace};
        struct FileProxy {
            inventory: Inventory,
            files: Arc<Local>,
        }
        impl Provider for FileProxy {
            fn inventory(&self) -> Inventory {
                self.inventory.clone()
            }
            fn open(&self, _: RepresentationId) -> io::Result<crate::FileReader> {
                Err(io::ErrorKind::NotFound.into())
            }
            fn translator(&self) -> Arc<dyn content::Translator> {
                Arc::new(Portable)
            }
            fn namespace(&self) -> Option<Arc<dyn Namespace>> {
                Some(self.files.clone())
            }
        }
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        let proxy = root.path().join("proxy");
        std::fs::create_dir_all(&proxy).unwrap();
        let source = proxy.join("phone.bin");
        let moved = root.path().join("moved.bin");
        std::fs::write(&source, b"phone bytes").unwrap();
        let files = Local::received_from(vec![(NodeId(1), source.clone())], DeviceId([2; 32])).unwrap();
        let mut record = record(1);
        record.items[0].kind = h::ItemKind::File;
        record.items[0].name = Some("phone.bin".into());
        record.formats.clear();
        store.merge(vec![record.clone()], vec![]).unwrap();
        let provider = FileProxy {
            inventory: Inventory {
                id: record.id,
                platform: ClipPlatform::Unknown,
                items: vec![ContentItem {
                    id: ItemId(0),
                    file: Some(NodeId(1)),
                    virtual_files: false,
                    representations: vec![],
                }],
                roots: files.roots(),
            },
            files: files.clone(),
        };
        store
            .capture(store.group(), &provider, Sensitivity::Normal, Some(proxy.clone()))
            .unwrap();
        store
            .holder(record.id, h::HolderState::Holding, h::HolderScope::FILES)
            .unwrap();
        std::fs::copy(&source, &moved).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&moved)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(std::fs::metadata(&source).unwrap().modified().unwrap()))
            .unwrap();
        files
            .proxy_file_moved(NodeId(1), &crate::tracked_file::TrackedFile::capture(&moved).unwrap())
            .unwrap();
        store
            .merge((2..=11).map(super::tests::record).collect(), vec![])
            .unwrap();
        assert!(store.enforce_cap(0).unwrap().is_empty());
        assert!(proxy.exists());
        assert_eq!(store.scope(record.id), h::HolderScope::FILES);
        assert_eq!(
            files.tracked_file(NodeId(1)).unwrap().unwrap().resolve().unwrap(),
            moved.canonicalize().unwrap()
        );
        store
            .merge((12..=111).map(super::tests::record).collect(), vec![])
            .unwrap();
        assert!(!proxy.exists());
        assert_eq!(std::fs::read(moved).unwrap(), b"phone bytes");
    }
    #[test]
    fn late_automatic_download_does_not_recreate_evicted_bytes() {
        let root = tempfile::tempdir().unwrap();
        let s = store(root.path());
        let mut first = record(1);
        inline(&mut first, 16);
        s.merge(vec![first.clone()], vec![]).unwrap();
        s.ingest_inline(&first).unwrap();
        let inventory = s.provider(first.id, None).unwrap().inventory();
        s.merge((2..=101).map(record).collect(), vec![]).unwrap();
        s.materialized(s.group(), &inventory, RepresentationId(1), &mut &b"late bytes"[..])
            .unwrap();
        assert_eq!(s.bytes.bytes(first.id), 0);
        assert!(s.provider(first.id, None).is_none());
    }

    #[test]
    fn concealed_inline_cache_lives_only_at_head_and_late_body_is_not_cached() {
        let root = tempfile::tempdir().unwrap();
        let s = store(root.path());
        let mut secret = record(1);
        secret.sensitivity = Sensitivity::Concealed;
        inline(&mut secret, 16);
        s.merge(vec![secret.clone()], vec![]).unwrap();
        s.ingest_inline(&secret).unwrap();
        assert!(s.bytes.bytes(secret.id) > 0);
        let provider = s.provider(secret.id, None).unwrap();
        let mut reader = content::open_portable(provider.as_ref(), RepresentationId(1)).unwrap();
        s.merge(vec![record(2)], vec![]).unwrap();
        assert!(s.window().get(secret.id).is_none());
        assert_eq!(s.bytes.bytes(secret.id), 0);
        assert_eq!(
            content::reason(&reader.read(&mut [0; 1]).unwrap_err()),
            Unavailable::NoSuchItem
        );
        s.merge(vec![secret.clone()], vec![]).unwrap();
        s.ingest_inline(&secret).unwrap();
        assert_eq!(s.bytes.bytes(secret.id), 0);
        assert!(s.provider(secret.id, None).is_none());
    }
}
