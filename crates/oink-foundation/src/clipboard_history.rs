//! Shared clipboard history. No I/O, wall-clock reads, clipboard writes or transport.
//!
//! Persist the window, local tombstones, holder revisions and the producer's Lamport
//! clock in the owning store. A new group gets a fresh window. Only shared records
//! enter this window; personal/direct transfers belong to the owner's local store.
//! Content dedupe belongs at capture, against the *current* clipboard, never across
//! history: reselecting an older clip is a new copy (clipboard_dedupe D11).
use crate::{DeviceId, clipboard::Sensitivity, clipboard_dedupe as dedupe, content::OfferId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};

pub const WINDOW_MAX: usize = 100;
pub const PAGE_MAX: usize = 20;
pub const INLINE_MAX: usize = 32 * 1024;
pub const PREVIEW_EDGE: u32 = 160;
pub const PREVIEW_MAX: usize = 8 * 1024;
pub const HISTORY_CAPABILITY: &str = "oink.clipboard.history/1";

/// Order uses the OfferId publisher, matching existing clipboard ordering.
/// Generation is a final tie breaker only for re-offers of the same ClipId.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OrderKey {
    pub clock: u64,
    pub origin: DeviceId,
    pub seq: u64,
    pub generation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ItemKind {
    Text = 0,
    Image = 1,
    File = 2,
    Directory = 3,
    Other = 4,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryItem {
    pub id: u32,
    pub kind: ItemKind,
    pub mime: String,
    pub name: Option<String>,
    pub bytes: Option<u64>,
    pub dimensions: Option<(u32, u32)>,
    pub child_count: Option<u64>,
    pub children_bytes: Option<u64>,
    /// File cards use the namespace identity, never the clipboard item ID.
    pub file_node: Option<u64>,
    pub file_version: Option<u64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum PortableFormat {
    Text = 0,
    Html = 1,
    Rtf = 2,
    Png = 3,
    Url = 4,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryFormat {
    pub item: u32,
    pub representation: u32,
    /// Portable contract or native format name. Never a local file path.
    pub name: String,
    pub portable: Option<PortableFormat>,
    pub bytes: Option<u64>,
    /// A native file representation can name a different anchor/root.
    pub file_node: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlinePayload {
    pub item: u32,
    pub representation: u32,
    pub format: PortableFormat,
    pub bytes: Vec<u8>,
}
/// Always JPEG, with alpha composited before encoding. The original's dimensions
/// live in HistoryItem, independently of this small preview's dimensions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlinePreview {
    pub width: u32,
    pub height: u32,
    pub jpeg: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryDigest {
    pub item: u32,
    /// Same numbering and comparison semantics as clipboard_dedupe::DigestClass.
    pub class: u8,
    pub name: Option<String>,
    pub size: Option<u64>,
    pub digest: Option<[u8; 32]>,
    pub dimensions: Option<(u32, u32)>,
    pub modified_ms: Option<i64>,
    pub directory: bool,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipRecord {
    pub id: OfferId,
    pub clock: u64,
    pub time_ms: i64,
    /// The actual copying device; the proxy may have allocated the OfferId.
    pub origin: DeviceId,
    pub proxy: Option<DeviceId>,
    pub sensitivity: Sensitivity,
    pub items: Vec<HistoryItem>,
    pub formats: Vec<HistoryFormat>,
    pub digests: Vec<HistoryDigest>,
    pub inline: Vec<InlinePayload>,
    pub preview: Option<InlinePreview>,
    /// 0 initially, 1 for the author's single preview supplement. No other edit.
    pub rev: u8,
}
// History can contain secrets even when the OS did not mark it concealed.
// Diagnostic formatting deliberately never prints names, digests or bytes.
impl std::fmt::Debug for ClipRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipRecord")
            .field("id", &self.id)
            .field("clock", &self.clock)
            .field("sensitivity", &self.sensitivity)
            .field("items", &self.items.len())
            .field("formats", &self.formats.len())
            .field("preview", &self.preview.is_some())
            .field("rev", &self.rev)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum HolderState {
    Holding = 0,
    Released = 1,
    Gone = 2,
}
/// Bitset of available portable representations/files. Native paths never travel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderScope(pub u32);
impl HolderScope {
    pub const TEXT: Self = Self(1);
    pub const HTML: Self = Self(2);
    pub const RTF: Self = Self(4);
    pub const PNG: Self = Self(8);
    pub const URL: Self = Self(16);
    pub const FILES: Self = Self(32);
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HolderEntry {
    pub record_id: OfferId,
    pub holder: DeviceId,
    /// Monotonic across process restarts, persisted by the holder.
    pub rev: u64,
    pub state: HolderState,
    pub scope: HolderScope,
}
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HistoryError {
    #[error("invalid or oversized history metadata")]
    InvalidRecord,
    #[error("conflicting immutable record or holder revision")]
    Conflict,
    #[error("history update author does not match authenticated author")]
    WrongAuthor,
}
pub type Result<T> = std::result::Result<T, HistoryError>;
impl ClipRecord {
    pub fn key(&self) -> OrderKey {
        OrderKey {
            clock: self.clock,
            origin: self.id.clip.origin,
            seq: self.id.clip.seq,
            generation: self.id.generation,
        }
    }
    pub fn sensitive(&self) -> bool {
        matches!(self.sensitivity, Sensitivity::Concealed | Sensitivity::Transient)
    }
    pub fn validate(&self) -> Result<()> {
        let invalid = || HistoryError::InvalidRecord;
        if self.rev > 1
            || (self.rev == 1 && self.preview.is_none())
            || self.items.len() > 1024
            || self.formats.len() > 4096
            || self.digests.len() > 4096
            || (self.sensitive() && (self.preview.is_some() || !self.digests.is_empty()))
            || self.proxy.unwrap_or(self.origin) != self.id.clip.origin
            || self
                .inline
                .iter()
                .try_fold(0usize, |n, p| n.checked_add(p.bytes.len()))
                .ok_or_else(invalid)?
                > INLINE_MAX
        {
            return Err(invalid());
        }
        let mut ids = HashSet::new();
        for item in &self.items {
            if !ids.insert(item.id)
                || item.file_node.is_some() != item.file_version.is_some()
                || (item.file_node.is_some() && !matches!(item.kind, ItemKind::File | ItemKind::Directory))
                || item.mime.len() > 256
                || item.name.as_ref().is_some_and(|n| {
                    n.len() > 4096
                        || n.contains('\0')
                        || (matches!(item.kind, ItemKind::File | ItemKind::Directory) && !valid_basename(n))
                })
                || item
                    .dimensions
                    .is_some_and(|(w, h)| w == 0 || h == 0 || item.kind != ItemKind::Image)
            {
                return Err(invalid());
            }
        }
        let mut reps = HashSet::new();
        for f in &self.formats {
            if !ids.contains(&f.item) || !reps.insert((f.item, f.representation)) || f.name.len() > 4096 {
                return Err(invalid());
            }
        }
        let mut inlines = HashSet::new();
        for p in &self.inline {
            if !inlines.insert((p.item, p.representation))
                || !self
                    .formats
                    .iter()
                    .any(|f| f.item == p.item && f.representation == p.representation && f.portable == Some(p.format))
            {
                return Err(invalid());
            }
        }
        if self.digests.iter().any(|d| {
            d.class > 6
                || !ids.contains(&d.item)
                || d.name
                    .as_ref()
                    .is_some_and(|n| n.len() > 4096 || (d.class == 5 && !valid_basename(n)))
        }) {
            return Err(invalid());
        }
        if let Some(p) = &self.preview
            && (p.width == 0
                || p.height == 0
                || p.width > PREVIEW_EDGE
                || p.height > PREVIEW_EDGE
                || p.jpeg.len() > PREVIEW_MAX
                || !p.jpeg.starts_with(&[0xff, 0xd8])
                || !p.jpeg.ends_with(&[0xff, 0xd9]))
        {
            return Err(invalid());
        }
        if let Some(p) = &self.preview {
            use image::ImageDecoder;
            // Validate the encoded dimensions as well as the metadata. An 8 KiB
            // JPEG can declare a huge raster; consumers must never interpret
            // such a payload as a bounded 160 px inline preview.
            let decoder =
                image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(&p.jpeg)).map_err(|_| invalid())?;
            if decoder.dimensions() != (p.width, p.height) {
                return Err(invalid());
            }
        }
        Ok(())
    }
    /// Use the EXISTING capture rule, including settings and currentness. Never
    /// call this during merge: two distinct published copies remain distinct.
    pub fn duplicate_of_current(
        &self,
        held: &dedupe::Identity,
        rules: dedupe::Rules,
        held_is_current: bool,
    ) -> std::result::Result<(), dedupe::NewCopy> {
        dedupe::duplicate(&self.fingerprint(), held, rules, held_is_current)
    }
    pub fn fingerprint(&self) -> dedupe::Fingerprint {
        use dedupe::DigestClass::*;
        dedupe::Fingerprint {
            entries: self
                .digests
                .iter()
                .filter_map(|d| {
                    Some(dedupe::Entry {
                        item: d.item,
                        class: *[Text, Url, Html, Rtf, Image, File, Other].get(d.class as usize)?,
                        name: d.name.clone(),
                        size: d.size,
                        digest: d.digest,
                        dimensions: d.dimensions,
                        modified_ms: d.modified_ms,
                        directory: d.directory,
                    })
                })
                .collect(),
        }
    }
    /// Concealed/transient content may be inline only while this is the head.
    /// Clear previews/digests; merge drops the whole record once superseded.
    pub fn redact(&mut self) {
        if self.sensitive() {
            self.preview = None;
            self.digests.clear();
            self.rev = 0;
        }
    }
}
// Match the existing FileNode wire rule: metadata contains names, never paths.
fn valid_basename(name: &str) -> bool {
    !name.is_empty() && !matches!(name, "." | "..") && !name.contains(['/', '\\', '\0'])
}
/// Choose whole portable representations in priority order; never truncate bytes.
/// Text wins over HTML, RTF, URL and PNG. Oversized candidates are skipped.
pub fn select_inline(mut candidates: Vec<InlinePayload>) -> Vec<InlinePayload> {
    candidates.sort_by_key(|p| match p.format {
        PortableFormat::Text => 0,
        PortableFormat::Html => 1,
        PortableFormat::Rtf => 2,
        PortableFormat::Url => 3,
        PortableFormat::Png => 4,
    });
    let mut left = INLINE_MAX;
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|p| {
            if p.bytes.len() <= left && seen.insert((p.item, p.representation)) {
                left -= p.bytes.len();
                true
            } else {
                false
            }
        })
        .collect()
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub id: OfferId,
    pub key: OrderKey,
    pub rev: u8,
    /// Concealed/Transient: retained only while newest, even without a body.
    pub head_only: bool,
    pub holders: Vec<(DeviceId, u64)>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reconcile {
    pub pull: Vec<OfferId>,
    pub send: Vec<OfferId>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryPage {
    pub records: Vec<ClipRecord>,
    pub holders: Vec<HolderEntry>,
    pub more: bool,
    pub next: Option<OrderKey>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HistoryWindow {
    records: Vec<ClipRecord>,
    holders: Vec<HolderEntry>,
    /// Local only. Hidden records remain in the shared digest and count to 100.
    tombstones: HashSet<OfferId>,
}
impl HistoryWindow {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn records(&self) -> &[ClipRecord] {
        &self.records
    }
    pub fn holders(&self) -> &[HolderEntry] {
        &self.holders
    }
    pub fn tombstones(&self) -> &HashSet<OfferId> {
        &self.tombstones
    }
    pub fn get(&self, id: OfferId) -> Option<&ClipRecord> {
        self.records.iter().find(|r| r.id == id)
    }
    pub fn hide_local(&mut self, id: OfferId) -> bool {
        self.get(id).is_some() && self.tombstones.insert(id)
    }
    /// Atomic batch merge. Return evicted IDs so owners can delete byte caches.
    /// Caller authenticates author for deltas; index responses may carry any group
    /// member's already-authored entries. Unknown-record holders are not retained.
    pub fn merge(
        &mut self,
        records: impl IntoIterator<Item = ClipRecord>,
        holders: impl IntoIterator<Item = HolderEntry>,
    ) -> Result<Vec<OfferId>> {
        let mut next = self.clone();
        for mut record in records {
            record.validate()?;
            record.inline.sort_by_key(|p| (p.item, p.representation));
            if let Some(old) = next.records.iter_mut().find(|r| r.id == record.id) {
                merge_record(old, record)?;
            } else {
                next.records.push(record);
            }
        }
        next.records.sort_by_key(|r| std::cmp::Reverse(r.key()));
        // Ambiguous author/order keys cannot give different peers different tops.
        if next.records.windows(2).any(|w| w[0].key() == w[1].key()) {
            return Err(HistoryError::Conflict);
        }
        let head = next.records.first().map(ClipRecord::key);
        let mut evicted = Vec::new();
        next.records.retain(|r| {
            let keep = !r.sensitive() || Some(r.key()) == head;
            if !keep {
                evicted.push(r.id);
            }
            keep
        });
        evicted.extend(next.records.iter().skip(WINDOW_MAX).map(|r| r.id));
        next.records.truncate(WINDOW_MAX);
        let ids: HashSet<_> = next.records.iter().map(|r| r.id).collect();
        next.holders.retain(|h| ids.contains(&h.record_id));
        next.tombstones.retain(|id| ids.contains(id));
        for entry in holders {
            if !ids.contains(&entry.record_id) {
                continue;
            }
            if let Some(old) = next
                .holders
                .iter_mut()
                .find(|h| h.record_id == entry.record_id && h.holder == entry.holder)
            {
                if old.rev == entry.rev && *old != entry {
                    return Err(HistoryError::Conflict);
                }
                if entry.rev > old.rev {
                    *old = entry;
                }
            } else {
                next.holders.push(entry);
            }
        }
        next.holders.sort_by_key(|h| {
            (
                h.record_id.clip.origin,
                h.record_id.clip.seq,
                h.record_id.generation,
                h.holder,
            )
        });
        *self = next;
        Ok(evicted)
    }
    pub fn index(&self) -> Vec<IndexEntry> {
        self.records
            .iter()
            .map(|r| {
                let mut holders: Vec<_> = self
                    .holders
                    .iter()
                    .filter(|h| h.record_id == r.id)
                    .map(|h| (h.holder, h.rev))
                    .collect();
                holders.sort_unstable();
                IndexEntry {
                    id: r.id,
                    key: r.key(),
                    rev: r.rev,
                    head_only: r.sensitive(),
                    holders,
                }
            })
            .collect()
    }
    pub fn digest(&self) -> [u8; 32] {
        window_digest(&self.index())
    }
    /// Compare the union's top 100 only. Old remote records legitimately evicted
    /// here must not cause endless pull loops. Each side runs this comparison.
    pub fn reconcile(&self, remote: &[IndexEntry]) -> Reconcile {
        reconcile_indexes(&self.index(), remote)
    }
    /// Phone pager: strict exclusive cursor, at most 20 visible shared records.
    pub fn page(&self, before: Option<OrderKey>, max: usize, inline: bool) -> HistoryPage {
        let eligible: Vec<_> = self
            .records
            .iter()
            .filter(|r| before.is_none_or(|b| r.key() < b) && !self.tombstones.contains(&r.id))
            .collect();
        let take = max.clamp(1, PAGE_MAX);
        let mut records: Vec<_> = eligible.iter().take(take).map(|r| (*r).clone()).collect();
        if !inline {
            for r in &mut records {
                r.inline.clear();
            }
        }
        let ids: HashSet<_> = records.iter().map(|r| r.id).collect();
        HistoryPage {
            more: eligible.len() > records.len(),
            next: records.last().map(ClipRecord::key),
            records,
            holders: self
                .holders
                .iter()
                .filter(|h| ids.contains(&h.record_id))
                .cloned()
                .collect(),
        }
    }
    /// Ascending shared order, then clamp wall time against the previous row.
    /// Personal/direct rows can be interleaved by these derived display times.
    pub fn display_times(&self) -> Vec<(OfferId, i64)> {
        let mut previous = i64::MIN;
        self.records
            .iter()
            .rev()
            .map(|r| {
                previous = previous.max(r.time_ms);
                (r.id, previous)
            })
            .collect()
    }
    /// Removed group members can no longer be holders. Apply the same membership
    /// view on all peers before exchanging summaries.
    /// Device roles live in Facts, outside this pure engine. Owners pass ONLY
    /// PC IDs: phones never hold shared data, including their own inline copies.
    pub fn retain_pc_holders(&mut self, pcs: &HashSet<DeviceId>) {
        self.holders.retain(|h| pcs.contains(&h.holder));
    }
    pub fn retain_members(&mut self, members: &HashSet<DeviceId>) {
        self.holders.retain(|h| members.contains(&h.holder));
    }
}
fn merge_record(old: &mut ClipRecord, new: ClipRecord) -> Result<()> {
    let mut a = old.clone();
    let mut b = new.clone();
    a.inline.clear();
    b.inline.clear();
    a.preview = None;
    b.preview = None;
    a.rev = 0;
    b.rev = 0;
    if a != b || (old.rev == new.rev && old.preview != new.preview) {
        return Err(HistoryError::Conflict);
    }
    for p in new.inline {
        if let Some(prior) = old
            .inline
            .iter()
            .find(|i| i.item == p.item && i.representation == p.representation)
        {
            if prior != &p {
                return Err(HistoryError::Conflict);
            }
        } else {
            old.inline.push(p);
        }
    }
    if let (Some(a), Some(b)) = (&old.preview, &new.preview)
        && a != b
    {
        return Err(HistoryError::Conflict);
    }
    if old.preview.is_none() {
        old.preview = new.preview;
    }
    old.inline.sort_by_key(|p| (p.item, p.representation));
    old.rev = old.rev.max(new.rev);
    old.validate()
}
/// Index-only reconciliation for phones that loaded only the first 20 bodies.
/// A partial body window must not be mistaken for the remote's complete index.
pub fn reconcile_indexes(local: &[IndexEntry], remote: &[IndexEntry]) -> Reconcile {
    let mut all: Vec<_> = local.iter().chain(remote).collect();
    all.sort_by_key(|r| std::cmp::Reverse(r.key));
    let mut seen = HashSet::new();
    all.retain(|r| seen.insert(r.id));
    let head = all.first().map(|r| r.key);
    all.retain(|r| !r.head_only || Some(r.key) == head);
    all.truncate(WINDOW_MAX);
    let needed: HashSet<_> = all.iter().map(|r| r.id).collect();
    let differs = |a: &IndexEntry, b: &IndexEntry| {
        a.rev > b.rev
            || a.holders
                .iter()
                .any(|(holder, rev)| !b.holders.iter().any(|(h, v)| h == holder && v >= rev))
    };
    Reconcile {
        pull: remote
            .iter()
            .filter(|r| needed.contains(&r.id) && local.iter().find(|l| l.id == r.id).is_none_or(|l| differs(r, l)))
            .map(|r| r.id)
            .collect(),
        send: local
            .iter()
            .filter(|l| needed.contains(&l.id) && remote.iter().find(|r| r.id == l.id).is_none_or(|r| differs(l, r)))
            .map(|r| r.id)
            .collect(),
    }
}
/// Merge complete window indexes without fetching their record bodies. Phones
/// keep this lightweight state for summaries, eviction and local tombstones,
/// then page bodies on demand. Holder values themselves arrive with records.
pub fn merge_indexes(local: &[IndexEntry], remote: &[IndexEntry]) -> Result<Vec<IndexEntry>> {
    let mut merged: Vec<IndexEntry> = Vec::new();
    for entry in local.iter().chain(remote) {
        if entry.key.origin != entry.id.clip.origin
            || entry.key.seq != entry.id.clip.seq
            || entry.key.generation != entry.id.generation
            || entry.rev > 1
            || entry.holders.len() > 16
        {
            return Err(HistoryError::InvalidRecord);
        }
        let mut holders = HashSet::new();
        if entry.holders.iter().any(|(holder, _)| !holders.insert(*holder)) {
            return Err(HistoryError::InvalidRecord);
        }
        if let Some(old) = merged.iter_mut().find(|e| e.id == entry.id) {
            if old.key != entry.key || old.head_only != entry.head_only {
                return Err(HistoryError::Conflict);
            }
            old.rev = old.rev.max(entry.rev);
            for (holder, rev) in &entry.holders {
                if let Some((_, prior)) = old.holders.iter_mut().find(|(h, _)| h == holder) {
                    *prior = (*prior).max(*rev);
                } else {
                    old.holders.push((*holder, *rev));
                }
            }
            if old.holders.len() > 16 {
                return Err(HistoryError::InvalidRecord);
            }
        } else {
            merged.push(entry.clone());
        }
    }
    merged.sort_by_key(|e| std::cmp::Reverse(e.key));
    if merged.windows(2).any(|w| w[0].key == w[1].key) {
        return Err(HistoryError::Conflict);
    }
    let head = merged.first().map(|r| r.key);
    merged.retain(|r| !r.head_only || Some(r.key) == head);
    merged.truncate(WINDOW_MAX);
    for e in &mut merged {
        e.holders.sort_unstable();
    }
    Ok(merged)
}
/// Canonical fixed-width IDs and lengths prevent concatenation ambiguities.
/// Preview rev is included so a missed one-time supplement also reconciles.
pub fn window_digest(index: &[IndexEntry]) -> [u8; 32] {
    let mut entries = index.to_vec();
    entries.sort_by_key(|r| std::cmp::Reverse(r.key));
    let head = entries.first().map(|r| r.key);
    entries.retain(|r| !r.head_only || Some(r.key) == head);
    entries.truncate(WINDOW_MAX);
    let mut h = blake3::Hasher::new();
    h.update(b"oink/clipboard/history/1");
    h.update(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        h.update(&e.id.clip.origin.0);
        h.update(&e.id.clip.seq.to_le_bytes());
        h.update(&e.id.generation.to_le_bytes());
        h.update(&[e.rev, u8::from(e.head_only)]);
        let mut holders = e.holders;
        holders.sort_unstable();
        h.update(&(holders.len() as u32).to_le_bytes());
        for (holder, rev) in holders {
            h.update(&holder.0);
            h.update(&rev.to_le_bytes());
        }
    }
    *h.finalize().as_bytes()
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryChange {
    Record(ClipRecord),
    Holder(HolderEntry),
}
impl HistoryChange {
    pub fn author(&self) -> DeviceId {
        match self {
            // A phone upload is published by the proxy that allocated the
            // OfferId. It sent the original delta; the phone's direct peers did
            // not necessarily receive it. Use that publisher in the mesh rule.
            Self::Record(r) => r.id.clip.origin,
            Self::Holder(h) => h.holder,
        }
    }
    pub fn record_id(&self) -> OfferId {
        match self {
            Self::Record(r) => r.id,
            Self::Holder(h) => h.record_id,
        }
    }
    pub fn validate_author(&self, authenticated_author: DeviceId) -> Result<()> {
        if self.author() == authenticated_author {
            Ok(())
        } else {
            Err(HistoryError::WrongAuthor)
        }
    }
}
/// Neighbour facts come from the network graph; never infer directness from IPs.
#[derive(Clone, Copy, Debug)]
pub struct HistoryNeighbour {
    pub id: DeviceId,
    pub direct: bool,
    pub capable: bool,
    pub subscribed: bool,
    pub phone: bool,
    pub connected_to_author: bool,
}
/// Delta dedupe uses record ID, author and revision (holder revisions have their
/// own namespaces). Prune with the shared window after eviction; never use a TTL.
#[derive(Clone, Debug, Default)]
pub struct ForwardSeen(HashMap<(OfferId, DeviceId, bool), u64>);
impl ForwardSeen {
    /// Call AFTER merge. Evicted/unknown records must never be re-flooded after
    /// pruning seen state, or a delayed stale delta can circulate indefinitely.
    pub fn observe_in_window(&mut self, window: &HistoryWindow, change: &HistoryChange) -> bool {
        window.get(change.record_id()).is_some() && self.observe(change)
    }
    pub fn observe(&mut self, change: &HistoryChange) -> bool {
        let (record, author, holder, rev) = match change {
            HistoryChange::Record(r) => (r.id, r.id.clip.origin, false, u64::from(r.rev)),
            HistoryChange::Holder(h) => (h.record_id, h.holder, true, h.rev),
        };
        let key = (record, author, holder);
        if self.0.get(&key).is_some_and(|v| *v >= rev) {
            return false;
        }
        self.0.insert(key, rev);
        true
    }
    pub fn retain_window(&mut self, window: &HistoryWindow) {
        self.0.retain(|(id, _, _), _| window.get(*id).is_some());
    }
}
pub fn forward_targets(
    local: DeviceId,
    local_is_phone: bool,
    author: DeviceId,
    sender: Option<DeviceId>,
    neighbours: &[HistoryNeighbour],
) -> BTreeSet<DeviceId> {
    if local_is_phone && local != author {
        return BTreeSet::new();
    }
    neighbours
        .iter()
        .filter(|n| {
            n.direct
                && n.capable
                && (!n.phone || n.subscribed)
                && n.id != author
                && Some(n.id) != sender
                && (local == author || !n.connected_to_author)
        })
        .map(|n| n.id)
        .collect()
}

/// Encode already-decoded straight-alpha RGBA8. The producer owns original-image
/// decode limits. Composite onto white so transparent pixels do not become black
/// in JPEG. Try quality first, then smaller edges; omit rather than exceed 8 KiB.
pub fn encode_inline_preview(width: u32, height: u32, rgba: Vec<u8>) -> Option<InlinePreview> {
    use image::{
        RgbImage, RgbaImage,
        codecs::jpeg::JpegEncoder,
        imageops::{FilterType, thumbnail},
    };
    if width == 0 || height == 0 {
        return None;
    }
    let mut source = RgbaImage::from_raw(width, height, rgba)?;
    // Composite before resampling: resizing straight-alpha channels first would
    // blend invisible RGB into visible edges and create dark/coloured fringes.
    for pixel in source.pixels_mut() {
        let alpha = u32::from(pixel.0[3]);
        for channel in &mut pixel.0[..3] {
            *channel = ((u32::from(*channel) * alpha + 255 * (255 - alpha) + 127) / 255) as u8;
        }
        pixel.0[3] = 255;
    }
    let scaled = image::imageops::resize(
        &source,
        ((u64::from(width) * u64::from(PREVIEW_EDGE)) / u64::from(width.max(height)))
            .max(1)
            .min(u64::from(width)) as u32,
        ((u64::from(height) * u64::from(PREVIEW_EDGE)) / u64::from(width.max(height)))
            .max(1)
            .min(u64::from(height)) as u32,
        FilterType::Triangle,
    );
    let rgb = RgbImage::from_fn(scaled.width(), scaled.height(), |x, y| {
        let p = scaled.get_pixel(x, y).0;
        image::Rgb([p[0], p[1], p[2]])
    });
    for edge in [160, 128, 96, 64, 32] {
        let candidate = thumbnail(&rgb, edge, edge);
        for quality in [65, 50, 35, 20] {
            let mut jpeg = Vec::new();
            if JpegEncoder::new_with_quality(&mut jpeg, quality)
                .encode_image(&candidate)
                .is_ok()
                && jpeg.len() <= PREVIEW_MAX
            {
                return Some(InlinePreview {
                    width: candidate.width(),
                    height: candidate.height(),
                    jpeg,
                });
            }
        }
    }
    None
}
impl HistoryDigest {
    /// Preserve every metadata field of the existing capture fingerprint. No new
    /// digest algorithm, format-subset policy or history-wide comparison rule.
    pub fn from_fingerprint(fingerprint: &dedupe::Fingerprint) -> Vec<Self> {
        fingerprint
            .entries
            .iter()
            .map(|e| Self {
                item: e.item,
                class: match e.class {
                    dedupe::DigestClass::Text => 0,
                    dedupe::DigestClass::Url => 1,
                    dedupe::DigestClass::Html => 2,
                    dedupe::DigestClass::Rtf => 3,
                    dedupe::DigestClass::Image => 4,
                    dedupe::DigestClass::File => 5,
                    dedupe::DigestClass::Other => 6,
                },
                name: e.name.clone(),
                size: e.size,
                digest: e.digest,
                dimensions: e.dimensions,
                modified_ms: e.modified_ms,
                directory: e.directory,
            })
            .collect()
    }
}
