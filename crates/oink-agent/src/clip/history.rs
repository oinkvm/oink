//! PC history sync. Payload reads stay in Registry; this owner exchanges metadata.
use super::*;
use oink_foundation::clipboard_history as h;
use oink_protocol::v2::{
    Message, dto,
    schema::{WireDto, WireValue},
};
use std::collections::{HashMap, HashSet};

pub(super) struct History {
    pub store: Arc<oink_clipboard::history::Store>,
    seen: h::ForwardSeen,
    subscriptions: HashSet<DeviceId>,
    reconcile: Option<(DeviceId, [u8; 16])>,
    pending: HashMap<DeviceId, Vec<Message>>,
}
impl History {
    pub fn new(store: Arc<oink_clipboard::history::Store>) -> Self {
        Self {
            store,
            seen: Default::default(),
            subscriptions: Default::default(),
            reconcile: None,
            pending: Default::default(),
        }
    }
    pub fn disconnected(&mut self, peer: DeviceId) {
        self.subscriptions.remove(&peer);
        self.pending.remove(&peer);
        if self.reconcile.is_some_and(|(p, _)| p == peer) {
            self.reconcile = None;
        }
    }
    pub fn suspend(&mut self) {
        self.pending.clear();
        self.reconcile = None;
        // Foreground subscriptions are properties of the live phone link. A
        // local pause closes B0, but does not put that phone in the background.
    }
    pub fn resumed(&mut self, group: oink_foundation::GroupId) {
        for peer in self.subscriptions.clone() {
            self.queue(peer, Message::HistorySummary(self.summary(group)));
        }
    }
    pub fn reset(&mut self) {
        self.seen = Default::default();
        self.subscriptions.clear();
        self.pending.clear();
        self.reconcile = None;
    }
    fn queue(&mut self, peer: DeviceId, message: Message) {
        self.pending.entry(peer).or_default().push(message);
    }
    fn prune_pending(&mut self) {
        let window = self.store.window();
        for messages in self.pending.values_mut() {
            messages.retain_mut(|message| match message {
                Message::HistoryDelta(delta) => delta
                    .to_domain()
                    .is_ok_and(|change| window.get(change.record_id()).is_some()),
                Message::HistoryRecords(records) => {
                    let Ok(mut page) = records.to_page() else {
                        return false;
                    };
                    page.records.retain(|record| window.get(record.id).is_some());
                    page.holders.retain(|entry| window.get(entry.record_id).is_some());
                    // Pending responses must not retain sensitive inline bodies
                    // after a newer head purges them, even under backpressure.
                    let Ok(updated) = dto::HistoryRecords::from_page(
                        records.request_id().expect("validated pending response").0,
                        &page,
                        window.digest(),
                    ) else {
                        return false;
                    };
                    *records = updated;
                    true
                }
                _ => true,
            });
        }
    }
    pub fn summary(&self, group: oink_foundation::GroupId) -> dto::HistorySummary {
        dto::HistorySummary::from_window(group, &self.store.window(), false).expect("valid local window")
    }
    pub fn forward(&mut self, node: &crate::node::Node, change: h::HistoryChange, sender: Option<DeviceId>) {
        let Some(context) = Context::from_node(node) else {
            return;
        };
        self.forward_in(&context, change, sender);
    }
    fn forward_in(&mut self, context: &Context, change: h::HistoryChange, sender: Option<DeviceId>) {
        let window = self.store.window();
        self.prune_pending();
        self.seen.retain_window(&window);
        if !self.seen.observe_in_window(&window, &change) {
            return;
        }
        let mut neighbours = context.neighbours.clone();
        for n in &mut neighbours {
            n.subscribed = self.subscriptions.contains(&n.id);
        }
        let targets = h::forward_targets(context.me, false, change.author(), sender, &neighbours);
        let Ok(delta) = dto::HistoryDelta::from_domain(&change, window.digest()) else {
            return;
        };
        for peer in targets {
            self.queue(peer, Message::HistoryDelta(delta.clone()));
        }
    }
    pub fn receive(
        &mut self,
        node: &crate::node::Node,
        peer: DeviceId,
        message: Message,
    ) -> Result<Vec<oink_clipboard::model::OfferId>, oink_protocol::v2::Error> {
        let Some(context) = Context::from_node(node) else {
            return Ok(vec![]);
        };
        self.receive_in(&context, peer, message)
    }
    fn receive_in(
        &mut self,
        context: &Context,
        peer: DeviceId,
        message: Message,
    ) -> Result<Vec<oink_clipboard::model::OfferId>, oink_protocol::v2::Error> {
        let mut evicted = vec![];
        let group = context.group;
        let window = self.store.window();
        let member = |id| context.members.contains(&id);
        let phone = context.phones.contains(&peer);
        let valid_record =
            |r: &h::ClipRecord| member(r.origin) && member(r.id.clip.origin) && r.proxy.is_none_or(member);
        match message {
            Message::HistorySummary(summary) => {
                if summary.group()?.0 != group.0 {
                    return Ok(evicted);
                }
                if phone {
                    if summary.subscribe()? {
                        self.subscriptions.insert(peer);
                    } else {
                        self.subscriptions.remove(&peer);
                        self.pending.remove(&peer);
                    }
                    if summary.subscribe()? {
                        self.queue(peer, Message::HistorySummary(self.summary(group)));
                    }
                } else if summary.digest()?.0 != window.digest() && self.reconcile.is_none() {
                    let id = random_id();
                    self.reconcile = Some((peer, id));
                    self.queue(
                        peer,
                        Message::HistoryIndexRequest(dto::HistoryIndexRequest::from_raw(
                            [(1, oink_protocol::v2::RawValue::Bytes(id.to_vec()))].into(),
                        )?),
                    );
                }
            }
            Message::HistoryIndexRequest(request) => {
                if phone && !self.subscriptions.contains(&peer) {
                    return Err(oink_protocol::v2::ErrorCode::UnsupportedValue.into());
                }
                self.queue(
                    peer,
                    Message::HistoryIndex(dto::HistoryIndex::from_window(request.request_id()?.0, &window)?),
                );
            }
            Message::HistoryIndex(index) => {
                let id = index.request_id()?.0;
                if self.reconcile != Some((peer, id)) {
                    return Ok(evicted);
                }
                self.reconcile = None;
                let remote = index.domain_entries()?;
                let diff = window.reconcile(&remote);
                if !diff.pull.is_empty() {
                    let id = random_id();
                    self.queue(
                        peer,
                        Message::HistoryPull(dto::HistoryPull::from_query(
                            id,
                            dto::HistoryQuery::Ids(diff.pull),
                            true,
                        )?),
                    );
                }
                if !diff.send.is_empty() {
                    let page = page_ids(&window, &diff.send, true);
                    self.queue(
                        peer,
                        Message::HistoryRecords(dto::HistoryRecords::from_page(random_id(), &page, window.digest())?),
                    );
                }
            }
            Message::HistoryPull(pull) => {
                if phone && !self.subscriptions.contains(&peer) {
                    return Err(oink_protocol::v2::ErrorCode::UnsupportedValue.into());
                }
                let page = match pull.query()? {
                    dto::HistoryQuery::Ids(ids) => page_ids(&window, &ids, pull.inline()?),
                    dto::HistoryQuery::Page { before, max } => window.page(before, max.into(), pull.inline()?),
                };
                self.queue(
                    peer,
                    Message::HistoryRecords(dto::HistoryRecords::from_page(
                        pull.request_id()?.0,
                        &page,
                        window.digest(),
                    )?),
                );
            }
            Message::HistoryRecords(records) if !phone => {
                let mut page = records.to_page()?;
                page.holders.retain(|entry| !context.phones.contains(&entry.holder));
                if page.records.iter().any(|r| {
                    !valid_record(r) || context.phones.contains(&r.id.clip.origin) && !phone_record_complete(r)
                }) || page.holders.iter().any(|e| !member(e.holder))
                {
                    return Ok(evicted);
                }
                evicted = self
                    .store
                    .merge(page.records.clone(), page.holders.clone())
                    .map_err(invalid)?;
                // Reconciliation may repair records the neighbours have never seen.
                for r in page.records {
                    self.store.ingest_inline(&r).map_err(invalid)?;
                    self.forward_in(context, h::HistoryChange::Record(r), Some(peer));
                }
                for e in page.holders {
                    self.forward_in(context, h::HistoryChange::Holder(e), Some(peer));
                }
            }
            Message::HistoryDelta(delta) => {
                let change = delta.to_domain()?;
                if matches!(&change,h::HistoryChange::Holder(entry) if context.phones.contains(&entry.holder)) {
                    tracing::debug!("phone-authored history holder ignored: phones never hold data");
                    return Err(oink_protocol::v2::ErrorCode::UnsupportedValue.into());
                }
                if !member(change.author()) || (phone && change.validate_author(peer).is_err()) {
                    return Err(oink_protocol::v2::ErrorCode::UnsupportedValue.into());
                }
                match &change {
                    h::HistoryChange::Record(r) if valid_record(r) => {
                        if context.phones.contains(&r.id.clip.origin) && !phone_record_complete(r) {
                            tracing::debug!("phone history record is not fully inline; proxy share required");
                            return Err(oink_protocol::v2::ErrorCode::UnsupportedValue.into());
                        }
                        evicted = self.store.merge(vec![r.clone()], vec![]).map_err(invalid)?;
                        self.store.ingest_inline(r).map_err(invalid)?;
                    }
                    h::HistoryChange::Holder(e) => {
                        if self.store.window().get(e.record_id).is_none() {
                            tracing::debug!("holder for unknown or evicted history record ignored");
                        }
                        evicted = self.store.merge(vec![], vec![e.clone()]).map_err(invalid)?;
                    }
                    _ => return Ok(evicted),
                }
                self.forward_in(context, change, Some(peer));
                // Phones own a full index, not all bodies. They initiate their own
                // reconciliation; comparing a paged phone body would loop forever.
                if !phone && delta.digest()?.0 != self.store.window().digest() && self.reconcile.is_none() {
                    let id = random_id();
                    self.reconcile = Some((peer, id));
                    self.queue(
                        peer,
                        Message::HistoryIndexRequest(dto::HistoryIndexRequest::from_raw(
                            [(1, oink_protocol::v2::RawValue::Bytes(id.to_vec()))].into(),
                        )?),
                    );
                }
            }
            _ => {}
        }
        Ok(evicted)
    }
    pub fn flush(&mut self, streams: &mut crate::streams::clipboard::ClipboardStreams) {
        self.prune_pending();
        for (peer, messages) in &mut self.pending {
            while !messages.is_empty() {
                if streams.send_history(*peer, &messages[0]).is_err() {
                    break;
                }
                messages.remove(0);
            }
        }
        self.pending.retain(|_, v| !v.is_empty());
    }
}
fn invalid(_: impl std::fmt::Display) -> oink_protocol::v2::Error {
    oink_protocol::v2::ErrorCode::UnsupportedValue.into()
}
fn random_id() -> [u8; 16] {
    let mut id = [0; 16];
    getrandom::fill(&mut id).expect("history request entropy");
    id
}
fn page_ids(window: &h::HistoryWindow, ids: &[oink_foundation::content::OfferId], inline: bool) -> h::HistoryPage {
    let mut records: Vec<_> = window
        .records()
        .iter()
        .filter(|r| ids.contains(&r.id))
        .cloned()
        .collect();
    if !inline {
        for r in &mut records {
            r.inline.clear();
        }
    }
    h::HistoryPage {
        records,
        holders: window
            .holders()
            .iter()
            .filter(|e| ids.contains(&e.record_id))
            .cloned()
            .collect(),
        more: false,
        next: None,
    }
}
struct Context {
    me: DeviceId,
    group: oink_foundation::GroupId,
    members: HashSet<DeviceId>,
    phones: HashSet<DeviceId>,
    neighbours: Vec<h::HistoryNeighbour>,
}
impl Context {
    fn from_node(node: &crate::node::Node) -> Option<Self> {
        let links = node.links().ok()?;
        let config = node.link_config().ok()?;
        let members = config.members();
        let phones: HashSet<_> = members
            .members
            .iter()
            .filter(|m| m.is_mobile())
            .map(|m| m.device)
            .collect();
        // Small trusted groups use dedupe-bounded flooding. Remote adjacency is
        // not exposed by the network: no peer is assumed already connected to
        // the author. observe_in_window bounds duplicates by (id, revision).
        let neighbours = links
            .links()
            .links
            .iter()
            .filter(|e| e.state == oink_network::link::LinkState::Ready)
            .map(|e| h::HistoryNeighbour {
                id: e.peer,
                direct: matches!(e.path, oink_network::link::LinkPath::Direct),
                capable: e.permits(0x0356),
                subscribed: false,
                phone: phones.contains(&e.peer),
                connected_to_author: false,
            })
            .collect();
        Some(Self {
            me: node.id(),
            group: node.group_id()?,
            members: members.members.iter().map(|m| m.device).collect(),
            phones,
            neighbours,
        })
    }
}

pub(super) fn record(
    offer: &dto::ClipboardOffer,
    fingerprint: Option<&Fingerprint>,
    provider: Option<&dyn Provider>,
    store: &oink_clipboard::history::Store,
) -> std::io::Result<h::ClipRecord> {
    use oink_clipboard::model::*;
    let id = offer
        .id()
        .and_then(oink_clipboard::wire::local_offer_id)
        .map_err(std::io::Error::other)?;
    let inventory = offer
        .inventory()
        .and_then(|i| oink_clipboard::wire::local_inventory(&i))
        .map_err(std::io::Error::other)?;
    let origin = offer
        .real_sender()
        .and_then(|d| oink_network::wire_identity::read_device(&d))
        .map_err(std::io::Error::other)?;
    let proxy = offer
        .proxy()
        .map_err(std::io::Error::other)?
        .map(|d| oink_network::wire_identity::read_device(&d))
        .transpose()
        .map_err(std::io::Error::other)?;
    let sensitivity = match offer.sensitivity().map_err(std::io::Error::other)? {
        dto::Sensitivity::Concealed => oink_foundation::clipboard::Sensitivity::Concealed,
        dto::Sensitivity::Transient => oink_foundation::clipboard::Sensitivity::Transient,
        dto::Sensitivity::AutoGenerated => oink_foundation::clipboard::Sensitivity::AutoGenerated,
        _ => oink_foundation::clipboard::Sensitivity::Normal,
    };
    let mut r = h::ClipRecord {
        id,
        clock: offer.clock().map_err(std::io::Error::other)?,
        time_ms: offer.time_ms().map_err(std::io::Error::other)?.min(i64::MAX as u64) as i64,
        origin,
        proxy,
        sensitivity,
        items: vec![],
        formats: vec![],
        digests: fingerprint.map(h::HistoryDigest::from_fingerprint).unwrap_or_default(),
        inline: vec![],
        preview: None,
        rev: 0,
    };
    let mut inline = vec![];
    for item in &inventory.items {
        let file = item.file.and_then(|node| {
            inventory
                .roots
                .iter()
                .find(|f| f.id == node)
                .or_else(|| if node.0 == 0 { inventory.roots.first() } else { None })
        });
        let kind = if let Some(file) = file {
            if file.kind == NodeKind::Directory {
                h::ItemKind::Directory
            } else {
                h::ItemKind::File
            }
        } else if item
            .representations
            .iter()
            .any(|rep| rep.standard == Some(StandardFormat::Png))
        {
            h::ItemKind::Image
        } else if item.representations.iter().any(|rep| {
            matches!(
                rep.standard,
                Some(StandardFormat::Text | StandardFormat::Html | StandardFormat::Rtf | StandardFormat::Url)
            )
        }) {
            h::ItemKind::Text
        } else {
            h::ItemKind::Other
        };
        r.items.push(h::HistoryItem {
            id: item.id.0,
            kind,
            mime: match kind {
                h::ItemKind::Image => "image/png",
                h::ItemKind::Text => "text/plain",
                _ => "application/octet-stream",
            }
            .into(),
            name: file.map(|f| f.name.clone()),
            bytes: file.and_then(|f| match f.size {
                ContentSize::Known(n) => Some(n),
                _ => None,
            }),
            dimensions: None,
            child_count: None,
            children_bytes: None,
            file_node: file.map(|f| f.id.0),
            file_version: file.map(|f| f.version),
        });
        for rep in &item.representations {
            let portable = oink_clipboard::history::portable(rep.standard);
            r.formats.push(h::HistoryFormat {
                item: item.id.0,
                representation: rep.id.0,
                name: portable
                    .map(|p| {
                        oink_clipboard::negotiation::portable_format(oink_clipboard::history::standard(p)).to_string()
                    })
                    .unwrap_or_else(|| match &rep.format {
                        NativeFormat::Mac(name)
                        | NativeFormat::WindowsRegistered(name)
                        | NativeFormat::Unknown(name) => name.clone(),
                        NativeFormat::WindowsPredefined(id) => format!("CF_{id}"),
                    }),
                portable,
                file_node: match rep.data {
                    RepresentationData::File(node) => Some(node.0),
                    _ => None,
                },
                bytes: match rep.data.size() {
                    Some(ContentSize::Known(n)) => Some(n),
                    _ => None,
                },
            });
            if item.file.is_some() {
                continue;
            }
            if r.sensitive() {
                if let (Some(format), Some(provider)) = (portable, provider)
                    && rep
                        .data
                        .size()
                        .is_some_and(|size| matches!(size,ContentSize::Known(n) if n <= h::INLINE_MAX as u64))
                    && let Ok(reader) = content::open_portable(provider, rep.id)
                {
                    use std::io::Read;
                    let mut bytes = vec![];
                    if reader.take(h::INLINE_MAX as u64 + 1).read_to_end(&mut bytes).is_ok()
                        && bytes.len() <= h::INLINE_MAX
                    {
                        inline.push(h::InlinePayload {
                            item: item.id.0,
                            representation: rep.id.0,
                            format,
                            bytes,
                        });
                    }
                }
                continue;
            }
            if let (Some(format), Some(provider)) = (portable, provider) {
                if !store.bytes.contains(id, item.id.0, rep.id.0)
                    && let Ok(mut reader) = content::open_portable(provider, rep.id)
                {
                    let _ = store.bytes.retain(id, item.id.0, rep.id.0, &mut reader);
                }
                if let Ok(Some((mut reader, size))) = store.bytes.open(id, item.id.0, rep.id.0) {
                    if let Some(format) = r.formats.last_mut() {
                        format.bytes = Some(size);
                    }
                    if size <= h::INLINE_MAX as u64 {
                        let mut bytes = vec![];
                        if std::io::Read::read_to_end(&mut reader, &mut bytes).is_ok() {
                            inline.push(h::InlinePayload {
                                item: item.id.0,
                                representation: rep.id.0,
                                format,
                                bytes,
                            });
                        }
                    }
                    if format == h::PortableFormat::Png
                        && r.preview.is_none()
                        && let Ok(Some((mut reader, _))) = store.bytes.open(id, item.id.0, rep.id.0)
                        && let Ok(bytes) = content::read_all(&mut reader)
                        && let Ok(image) = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
                    {
                        if let Some(item) = r
                            .items
                            .iter_mut()
                            .find(|i| i.id == item.id.0 && i.kind == h::ItemKind::Image)
                        {
                            item.dimensions = Some((image.width(), image.height()));
                        }
                        r.preview =
                            h::encode_inline_preview(image.width(), image.height(), image.into_rgba8().into_raw());
                    }
                }
            }
        }
    }
    // Windows can represent an entire file selection with one clipboard item.
    // Every root still needs its own card, with its actual u64 namespace identity.
    let mut next_item = 0u32;
    for root in &inventory.roots {
        if r.items.iter().any(|item| item.file_node == Some(root.id.0)) {
            continue;
        }
        while r.items.iter().any(|item| item.id == next_item) {
            next_item = next_item.checked_add(1).ok_or(std::io::ErrorKind::InvalidData)?;
        }
        r.items.push(h::HistoryItem {
            id: next_item,
            kind: if root.kind == NodeKind::Directory {
                h::ItemKind::Directory
            } else {
                h::ItemKind::File
            },
            mime: "application/octet-stream".into(),
            name: Some(root.name.clone()),
            bytes: match root.size {
                ContentSize::Known(n) => Some(n),
                _ => None,
            },
            dimensions: None,
            child_count: None,
            children_bytes: None,
            file_node: Some(root.id.0),
            file_version: Some(root.version),
        });
    }
    r.inline = h::select_inline(inline);
    r.redact();
    r.validate().map_err(std::io::Error::other)?;
    Ok(r)
}

/// Broadcast phone text has one authored identity for both history and current
/// clipboard ordering. Proxy attachments continue through ClipboardShare.
pub(super) fn phone_offer(record: &h::ClipRecord) -> Option<dto::ClipboardOffer> {
    if record.formats.is_empty()
        || record.formats.iter().any(|f| {
            !record
                .inline
                .iter()
                .any(|p| p.item == f.item && p.representation == f.representation && Some(p.format) == f.portable)
        })
    {
        return None;
    }
    use oink_clipboard::model::*;
    if record.proxy.is_some()
        || record.origin != record.id.clip.origin
        || record.items.iter().any(|i| i.kind != h::ItemKind::Text)
    {
        return None;
    }
    let mut items = vec![];
    for item in &record.items {
        let mut representations = vec![];
        for f in record.formats.iter().filter(|f| f.item == item.id) {
            let standard = oink_clipboard::history::standard(f.portable?);
            // ClipboardOffer has its independent 4 KiB inline ceiling. The
            // phone's <=32 KiB body is already committed in the PC history
            // cache; this current-clipboard projection only names its slots.
            let data = RepresentationData::Reader {
                size: f.bytes.map_or(ContentSize::Unknown, ContentSize::Known),
            };
            representations.push(Representation {
                id: RepresentationId(f.representation),
                format: NativeFormat::Unknown(oink_clipboard::wire::portable(standard).0),
                medium: Medium::Bytes,
                aspect: 1,
                index: -1,
                standard: Some(standard),
                data,
            });
        }
        items.push(ContentItem {
            id: ItemId(item.id),
            file: None,
            virtual_files: false,
            representations,
        });
    }
    let inventory = Inventory {
        id: record.id,
        platform: oink_foundation::clipboard::ClipPlatform::Unknown,
        items,
        roots: vec![],
    };
    let raw = [
        (1, oink_clipboard::wire::offer_id(record.id).write()),
        (2, 0u64.write()),
        (3, record.clock.write()),
        (4, oink_clipboard::wire::sensitivity(record.sensitivity).write()),
        (6, oink_clipboard::wire::inventory(&inventory).ok()?.write()),
        (7, (record.time_ms.max(0) as u64).write()),
        (8, oink_clipboard::wire::device(record.origin).write()),
        (9, oink_protocol::v2::RawValue::Null),
        (10, oink_protocol::v2::RawValue::Bytes(vec![0; 16])),
        (11, oink_protocol::v2::RawValue::Bytes(vec![0; 16])),
        (12, (record.items.len() as u32).write()),
        (13, oink_protocol::v2::RawValue::Array(vec![])),
    ]
    .into();
    dto::ClipboardOffer::from_raw(raw).ok()
}

fn phone_record_complete(record: &h::ClipRecord) -> bool {
    if record.proxy.is_some()
        || record.origin != record.id.clip.origin
        || record.items.iter().any(|i| i.kind != h::ItemKind::Text)
    {
        return false;
    }
    !record.formats.is_empty()
        && record.formats.iter().all(|f| {
            f.portable.is_some()
                && record.inline.iter().any(|p| {
                    p.item == f.item
                        && p.representation == f.representation
                        && Some(p.format) == f.portable
                        && f.bytes.is_none_or(|n| n == p.bytes.len() as u64)
                })
        })
}

pub(super) struct Capture {
    pub group: oink_foundation::GroupId,
    pub store: Arc<oink_clipboard::history::Store>,
    pub registry: Arc<content::Registry>,
    pub offer: dto::ClipboardOffer,
    pub provider: Arc<dyn Provider>,
    pub fingerprint: Option<Fingerprint>,
    pub proxy: Option<PathBuf>,
}
impl Capture {
    pub fn run(self) -> std::io::Result<Vec<h::HistoryChange>> {
        let record = record(
            &self.offer,
            self.fingerprint.as_ref(),
            Some(self.provider.as_ref()),
            &self.store,
        )?;
        if self.store.group() != self.group {
            let _ = self.store.bytes.forget(record.id);
            return Ok(vec![]);
        }
        let evicted = self.store.merge_in(self.group, vec![record.clone()], vec![])?;
        for id in evicted {
            self.registry.forget_history(id);
        }
        if record.sensitive() && self.proxy.is_none() {
            self.store.ingest_inline(&record)?;
        } else {
            self.store
                .capture(self.group, self.provider.as_ref(), record.sensitivity, self.proxy)?;
        }
        let mut changes = vec![h::HistoryChange::Record(record.clone())];
        let scope = self.store.scope(record.id);
        if scope.0 != 0
            && let Some(entry) = self.store.holder(record.id, h::HolderState::Holding, scope)?
        {
            changes.push(h::HistoryChange::Holder(entry));
        }
        Ok(changes)
    }
}

#[cfg(all(test, debug_assertions))]
#[path = "history_tests.rs"]
mod tests;
