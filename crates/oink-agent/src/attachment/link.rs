//! Bounded descriptor attachments. One source identity and one assembly deadline
//! cover both the reliable prefix and the Low remainder.
use oink_network::link::StreamId;
use oink_protocol::v1::{
    self, Message, RawMap, RawValue as V, dto, limits,
    schema::{Open, WireDto, WireValue},
};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub(crate) enum Header {
    Offer(dto::ClipboardOffer),
    Drag(dto::DndNotice),
}
impl Header {
    fn from_message(message: Message) -> v1::Result<Self> {
        match message {
            Message::ClipboardOffer(o) => Ok(Self::Offer(o)),
            Message::DndNotice(n) => Ok(Self::Drag(n)),
            _ => Err(v1::ErrorCode::UnsupportedValue.into()),
        }
    }
    fn message(&self) -> Message {
        match self {
            Self::Offer(o) => Message::ClipboardOffer(o.clone()),
            Self::Drag(n) => Message::DndNotice(n.clone()),
        }
    }
    fn family(&self) -> dto::AttachmentKind {
        match self {
            Self::Offer(_) => dto::AttachmentKind::Offer,
            Self::Drag(_) => dto::AttachmentKind::Drag,
        }
    }
    fn identity(&self) -> v1::Result<RawMap> {
        Ok(match self {
            Self::Offer(o) => o.id()?.into_raw(),
            Self::Drag(n) => n.key()?.into_raw(),
        })
    }
    fn inventory(&self) -> v1::Result<dto::Inventory> {
        match self {
            Self::Offer(o) => o.inventory(),
            Self::Drag(n) => n.inventory(),
        }
    }
    fn with_inventory(&self, inventory: dto::Inventory) -> v1::Result<Self> {
        Ok(match self {
            Self::Offer(o) => {
                let mut raw = o.raw().clone();
                raw.insert(6, inventory.write());
                Self::Offer(dto::ClipboardOffer::from_raw(raw)?)
            }
            Self::Drag(n) => {
                let mut raw = n.raw().clone();
                raw.insert(7, inventory.write());
                Self::Drag(dto::DndNotice::from_raw(raw)?)
            }
        })
    }
    fn refresh_digest(&mut self) -> v1::Result<()> {
        if let Self::Offer(offer) = self {
            let digest = blake3::hash(&offer.inventory()?.encode(limits::INVENTORY)?);
            let mut raw = offer.raw().clone();
            raw.insert(5, oink_clipboard::wire::digest(digest.into()).write());
            *offer = dto::ClipboardOffer::from_raw(raw)?;
        }
        Ok(())
    }
    fn wire(&self) -> v1::Result<dto::AttachmentHeader> {
        let kind = match self {
            Self::Offer(_) => 0,
            Self::Drag(_) => 1,
        };
        let message = match self {
            Self::Offer(o) => o.write(),
            Self::Drag(n) => n.write(),
        };
        dto::AttachmentHeader::from_raw([(1, V::Uint(kind)), (2, V::Map([(1, message)].into()))].into())
    }
    fn from_wire(header: dto::AttachmentHeader) -> v1::Result<Self> {
        match header.variant()? {
            Open::Known(dto::AttachmentKind::Offer) => {
                Ok(Self::Offer(dto::OfferAttachment::from_raw(header.body()?)?.offer()?))
            }
            Open::Known(dto::AttachmentKind::Drag) => {
                Ok(Self::Drag(dto::DragAttachment::from_raw(header.body()?)?.notice()?))
            }
            _ => Err(v1::ErrorCode::UnsupportedValue.into()),
        }
    }
}
fn inventory_parts(inventory: &dto::Inventory, count: usize) -> v1::Result<(dto::Inventory, dto::InventoryRemainder)> {
    #[cfg(test)]
    SPLIT_WORK.with_borrow_mut(|work| work.partitions += 1);
    let items = inventory.items()?.0;
    let roots = inventory.roots()?.0;
    let mut raw = inventory.raw().clone();
    raw.insert(3, V::Array(items.iter().take(count).map(WireValue::write).collect()));
    raw.insert(
        4,
        V::Array(
            roots
                .iter()
                .take(count.saturating_sub(items.len()))
                .map(WireValue::write)
                .collect(),
        ),
    );
    let tail = dto::InventoryRemainder::from_raw(
        [
            (
                1,
                V::Array(
                    items
                        .iter()
                        .skip(count.min(items.len()))
                        .map(WireValue::write)
                        .collect(),
                ),
            ),
            (
                2,
                V::Array(
                    roots
                        .iter()
                        .skip(count.saturating_sub(items.len()))
                        .map(WireValue::write)
                        .collect(),
                ),
            ),
        ]
        .into(),
    )?;
    Ok((dto::Inventory::from_raw(raw)?, tail))
}
fn head(id: u64, header: &Header, remaining: usize) -> v1::Result<Message> {
    #[cfg(test)]
    SPLIT_WORK.with_borrow_mut(|work| work.heads += 1);
    Ok(Message::AttachmentHead(dto::AttachmentHead::from_raw(
        [
            (1, V::Uint(id)),
            (2, header.wire()?.write()),
            (3, V::Uint(remaining as u64)),
        ]
        .into(),
    )?))
}
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
struct SplitWork {
    heads: usize,
    partitions: usize,
    descriptor_sizes: usize,
}
#[cfg(test)]
thread_local! {
    static SPLIT_WORK: std::cell::RefCell<SplitWork> = Default::default();
}

fn prefix_count(id: u64, header: &Header, inventory: &dto::Inventory) -> v1::Result<usize> {
    let (Some(V::Array(items)), Some(V::Array(roots))) = (inventory.raw().get(&3), inventory.raw().get(&4)) else {
        return Err(v1::ErrorCode::MalformedFrame.into());
    };
    let count = items.len() + roots.len();
    if count > 65535 {
        return Err(v1::ErrorCode::LimitExceeded.into());
    }
    let mut empty = inventory.raw().clone();
    empty.insert(3, V::Array(vec![]));
    empty.insert(4, V::Array(vec![]));
    let empty = head(id, &header.with_inventory(dto::Inventory::from_raw(empty)?)?, 0)?;
    // Keep all envelope/unknown fields. Only descriptor bytes, the two array
    // lengths and the remaining count vary; CBOR array and uint headers have
    // identical widths. No candidate prefix needs a DTO rebuild or validation.
    let base = v1::frame::FRAME_HEADER + v1::cbor::map_len(empty.raw())? - 3;
    let mut bytes = 0;
    let mut best = 0;
    for (index, descriptor) in items.iter().chain(roots).enumerate() {
        #[cfg(test)]
        SPLIT_WORK.with_borrow_mut(|work| work.descriptor_sizes += 1);
        bytes += descriptor.encoded_len()?;
        let kept = index + 1;
        let size = base
            + bytes
            + V::Uint(kept.min(items.len()) as u64).encoded_len()?
            + V::Uint(kept.saturating_sub(items.len()) as u64).encoded_len()?
            + V::Uint((count - kept) as u64).encoded_len()?;
        if size <= limits::RELIABLE_FRAME {
            best = kept;
        }
    }
    Ok(best)
}
pub(crate) fn split(id: u64, message: Message) -> v1::Result<(Message, Option<Message>)> {
    let mut header = Header::from_message(message)?;
    let original = header.inventory()?;
    let mut inventory = oink_clipboard::wire::reliable_inventory(&original)?;
    if inventory != original {
        header = header.with_inventory(inventory.clone())?;
        header.refresh_digest()?;
    }
    let message = header.message();
    if v1::frame::encode(&message, limits::RELIABLE_FRAME).is_ok() {
        return Ok((message, None));
    }
    inventory = oink_clipboard::wire::descriptor_inventory(&inventory)?;
    header = header.with_inventory(inventory.clone())?;
    header.refresh_digest()?;
    let low = prefix_count(id, &header, &inventory)?;
    let (prefix, tail) = inventory_parts(&inventory, low)?;
    let count = low + tail.items()?.0.len() + tail.roots()?.0.len();
    let first = head(id, &header.with_inventory(prefix)?, count - low)?;
    v1::frame::encode(&first, limits::RELIABLE_FRAME)?;
    let second = Message::AttachmentItems(dto::AttachmentItems::from_raw(
        [
            (1, V::Uint(id)),
            (2, tail.write()),
            (3, header.family().write()),
            (4, V::Map(header.identity()?)),
            (5, V::Uint((count - low) as u64)),
        ]
        .into(),
    )?);
    v1::frame::encode(&second, limits::LOW_FRAME)?;
    Ok((first, Some(second)))
}
fn result(
    id: u64,
    family: dto::AttachmentKind,
    identity: RawMap,
    remaining: u32,
    outcome: dto::AttachmentOutcome,
) -> Message {
    Message::AttachmentResult(
        dto::AttachmentResult::from_raw(
            [
                (1, V::Uint(id)),
                (2, family.write()),
                (3, outcome.write()),
                (4, V::Map(identity)),
                (5, V::Uint(remaining.into())),
            ]
            .into(),
        )
        .expect("validated attachment identity"),
    )
}
#[derive(Debug)]
struct Entry {
    id: u64,
    identity: RawMap,
    remaining: u32,
    expires: Instant,
    head: Option<Header>,
    tail: Option<dto::InventoryRemainder>,
    bytes: usize,
    ended: bool,
}
#[derive(Debug, Default)]
pub(crate) struct Assemblies {
    entries: HashMap<(StreamId, u8), Entry>,
}
impl Assemblies {
    pub(crate) fn clear(&mut self, stream: StreamId) {
        self.entries.retain(|(s, _), _| *s != stream);
    }
    /// A Low tail has no reliable-stream ordinal. Bind it only after its head
    /// identifies that stream; already superseded/ended tails may be discarded.
    pub(crate) fn can_receive_tail(&self, stream: StreamId, items: &dto::AttachmentItems) -> bool {
        let family = match items.family() {
            Ok(dto::AttachmentKind::Offer) => 0,
            Ok(dto::AttachmentKind::Drag) => 1,
            _ => return false,
        };
        let Ok(id) = items.id() else { return false };
        self.entries
            .get(&(stream, family))
            .is_some_and(|entry| entry.id > id || entry.id == id && (entry.head.is_some() || entry.ended))
    }
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.entries.values().filter(|e| !e.ended).map(|e| e.expires).min()
    }
    pub(crate) fn expire(&mut self, now: Instant) -> Vec<(StreamId, Message)> {
        let mut replies = vec![];
        for ((stream, family), entry) in &mut self.entries {
            if !entry.ended && now >= entry.expires {
                entry.ended = true;
                entry.head = None;
                entry.tail = None;
                entry.bytes = 0;
                replies.push((
                    *stream,
                    result(
                        entry.id,
                        if *family == 0 {
                            dto::AttachmentKind::Offer
                        } else {
                            dto::AttachmentKind::Drag
                        },
                        entry.identity.clone(),
                        entry.remaining,
                        dto::AttachmentOutcome::Expired,
                    ),
                ));
            }
        }
        replies
    }
    pub(crate) fn receive(
        &mut self,
        stream: StreamId,
        message: Message,
        now: Instant,
    ) -> v1::Result<(Option<Message>, Vec<Message>)> {
        let (id, family, identity, remaining, header, tail) = match &message {
            Message::AttachmentHead(head) => {
                let header = Header::from_wire(head.header()?)?;
                (
                    head.id()?,
                    header.family(),
                    header.identity()?,
                    head.remaining()?,
                    Some(header),
                    None,
                )
            }
            Message::AttachmentItems(items) => (
                items.id()?,
                items.family()?,
                items.identity()?,
                items.remaining()?,
                None,
                Some(items.items()?),
            ),
            _ => return Err(v1::ErrorCode::UnsupportedValue.into()),
        };
        let family_id = match family {
            dto::AttachmentKind::Offer => 0,
            dto::AttachmentKind::Drag => 1,
            _ => return Err(v1::ErrorCode::UnsupportedValue.into()),
        };
        let key = (stream, family_id);
        let mut replies = vec![];
        if let Some(old) = self.entries.get(&key) {
            if id < old.id {
                return Ok((
                    None,
                    vec![result(
                        id,
                        family,
                        identity,
                        remaining,
                        dto::AttachmentOutcome::Superseded,
                    )],
                ));
            }
            if id == old.id {
                if old.ended {
                    return Ok((None, vec![]));
                }
                if old.identity != identity || old.remaining != remaining {
                    return Err(v1::ErrorCode::StateConflict.into());
                }
                if now >= old.expires {
                    let old = self.entries.get_mut(&key).unwrap();
                    old.ended = true;
                    old.head = None;
                    old.tail = None;
                    old.bytes = 0;
                    return Ok((
                        None,
                        vec![result(id, family, identity, remaining, dto::AttachmentOutcome::Expired)],
                    ));
                }
                if header.is_some() && old.head.is_some() || tail.is_some() && old.tail.is_some() {
                    return Ok((None, vec![]));
                }
            }
        }
        let limit = if header.is_some() {
            limits::RELIABLE_FRAME
        } else {
            limits::LOW_FRAME
        };
        let size = v1::frame::encode(&message, limit)?.len();
        let prefix = header
            .as_ref()
            .map(|h| h.inventory().and_then(|i| Ok(i.items()?.0.len() + i.roots()?.0.len())))
            .transpose()?
            .unwrap_or(0);
        if prefix.checked_add(remaining as usize).is_none_or(|n| n > 65535) {
            return Err(v1::ErrorCode::LimitExceeded.into());
        }
        if let Some(tail) = &tail
            && tail.descriptor_count()? != remaining as usize
        {
            return Err(v1::ErrorCode::StateConflict.into());
        }
        let old_bytes = self.entries.get(&key).filter(|e| e.id != id).map_or(0, |e| e.bytes);
        if self.entries.len() >= 64 && !self.entries.contains_key(&key)
            || self.entries.values().map(|e| e.bytes).sum::<usize>() - old_bytes + size > 32 * 1024 * 1024
        {
            return Ok((
                None,
                vec![result(id, family, identity, remaining, dto::AttachmentOutcome::Busy)],
            ));
        }
        if self.entries.get(&key).is_some_and(|e| e.id != id) {
            let old = self.entries.remove(&key).unwrap();
            if !old.ended {
                replies.push(result(
                    old.id,
                    family,
                    old.identity,
                    old.remaining,
                    dto::AttachmentOutcome::Superseded,
                ));
            }
        }
        let entry = self.entries.entry(key).or_insert_with(|| Entry {
            id,
            identity,
            remaining,
            expires: now + Duration::from_secs(10),
            head: None,
            tail: None,
            bytes: 0,
            ended: false,
        });
        entry.bytes += size;
        if let Some(header) = header {
            entry.head = Some(header);
        }
        if let Some(tail) = tail {
            entry.tail = Some(tail);
        }
        if entry.bytes > limits::INVENTORY + limits::RELIABLE_FRAME {
            return Err(v1::ErrorCode::LimitExceeded.into());
        }
        if entry.head.is_none() || entry.remaining > 0 && entry.tail.is_none() {
            return Ok((None, replies));
        }
        let header = entry.head.take().unwrap();
        let mut inventory = header.inventory()?.into_raw();
        if let Some(tail) = entry.tail.take() {
            if tail.descriptor_count()? != entry.remaining as usize {
                return Err(v1::ErrorCode::StateConflict.into());
            }
            let V::Array(items) = inventory.get_mut(&3).unwrap() else {
                unreachable!()
            };
            items.extend(tail.items()?.0.iter().map(WireValue::write));
            let V::Array(roots) = inventory.get_mut(&4).unwrap() else {
                unreachable!()
            };
            roots.extend(tail.roots()?.0.iter().map(WireValue::write));
        }
        let inventory = dto::Inventory::from_raw(inventory)?;
        inventory.encode(limits::INVENTORY)?;
        let complete = header.with_inventory(inventory)?.message();
        entry.ended = true;
        entry.bytes = 0;
        Ok((Some(complete), replies))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::{
        DeviceId,
        clipboard::{ClipPlatform, Sensitivity},
        content::*,
    };
    use oink_network::link::{LinkId, StreamDir};
    use oink_protocol::frame::{StreamKind, StreamTag};
    fn stream(ordinal: u32) -> StreamId {
        StreamId {
            link: LinkId(1),
            dir: StreamDir::In,
            tag: StreamTag {
                kind: StreamKind::Clipboard,
                ordinal,
            },
        }
    }
    fn offer() -> Message {
        offer_with_items(100)
    }
    fn offer_with_items(count: u32) -> Message {
        offer_with_descriptors(count, 0)
    }
    fn offer_with_descriptors(count: u32, roots: u32) -> Message {
        let inventory = oink_clipboard::model::Inventory {
            id: OfferId {
                clip: ClipId {
                    origin: DeviceId([1; 32]),
                    seq: 1,
                },
                generation: 1,
            },
            platform: ClipPlatform::Mac,
            roots: (0..roots)
                .map(|n| FileNode {
                    id: NodeId(u64::from(n)),
                    parent: None,
                    name: format!("root-{n}"),
                    kind: NodeKind::File,
                    size: ContentSize::Unknown,
                    version: 1,
                    modified_ms: None,
                })
                .collect(),
            items: (0..count)
                .map(|n| ContentItem {
                    virtual_files: false,
                    id: ItemId(n),
                    file: None,
                    representations: vec![Representation {
                        id: RepresentationId(n),
                        format: NativeFormat::Mac(format!("oink.fixture.{}", "x".repeat(450))),
                        medium: Medium::Bytes,
                        aspect: 1,
                        index: -1,
                        standard: None,
                        data: RepresentationData::Reader {
                            size: ContentSize::Unknown,
                            hash: None,
                        },
                    }],
                })
                .collect(),
        };
        let mut raw = oink_clipboard::wire::inventory(&inventory).unwrap().into_raw();
        raw.insert(900, V::Map([(1, V::Text("future".into()))].into()));
        let offer = oink_clipboard::sync::Ordering::new(DeviceId([1; 32]), 0)
            .capture(dto::Inventory::from_raw(raw).unwrap(), 1, Sensitivity::Normal, 0)
            .unwrap()
            .unwrap();
        Message::ClipboardOffer(offer)
    }
    fn outcome(replies: &[Message]) -> dto::AttachmentOutcome {
        let [Message::AttachmentResult(result)] = replies else {
            panic!()
        };
        result.outcome().unwrap()
    }
    #[test]
    fn split_work_is_linear_and_prefix_is_maximal() {
        for (items, roots) in [(100, 0), (600, 0), (1200, 0), (0, 1200), (1, 1200), (24, 1200)] {
            let count = items + roots;
            let original = offer_with_descriptors(items, roots);
            SPLIT_WORK.with_borrow_mut(|work| *work = SplitWork::default());
            let (first, second) = split(256, original.clone()).unwrap();
            let work = SPLIT_WORK.with_borrow(|work| *work);
            assert_eq!(work.descriptor_sizes, count as usize);
            assert_eq!(work.partitions, 1, "partition only the final prefix/tail");
            assert_eq!(work.heads, 2, "only empty sizing envelope and final head");
            let Message::AttachmentHead(ref first_head) = first else {
                panic!()
            };
            let kept = count as usize - first_head.remaining().unwrap() as usize;
            let header = Header::from_message(original.clone()).unwrap();
            let inventory = header.inventory().unwrap();
            let (larger, _) = inventory_parts(&inventory, kept + 1).unwrap();
            let larger = head(256, &header.with_inventory(larger).unwrap(), count as usize - kept - 1).unwrap();
            assert!(v1::frame::encode(&first, limits::RELIABLE_FRAME).is_ok());
            assert!(v1::frame::encode(&larger, limits::RELIABLE_FRAME).is_err());
            let mut cache = Assemblies::default();
            let now = Instant::now();
            assert!(cache.receive(stream(1), first, now).unwrap().0.is_none());
            assert_eq!(
                cache.receive(stream(1), second.unwrap(), now).unwrap().0,
                Some(original)
            );
        }
    }
    #[test]
    fn either_order_preserves_the_entire_typed_tree_and_watermark() {
        let original = offer();
        let (head, tail) = split(1, original.clone()).unwrap();
        let tail = tail.unwrap();
        assert!(v1::frame::encode(&head, limits::RELIABLE_FRAME).is_ok());
        for reverse in [false, true] {
            let mut cache = Assemblies::default();
            let now = Instant::now();
            let (a, b) = if reverse {
                (tail.clone(), head.clone())
            } else {
                (head.clone(), tail.clone())
            };
            assert!(cache.receive(stream(1), a, now).unwrap().0.is_none());
            assert_eq!(cache.receive(stream(1), b, now).unwrap().0, Some(original.clone()));
            assert!(cache.receive(stream(1), head.clone(), now).unwrap().0.is_none());
        }
    }
    #[test]
    fn expiry_never_renews_and_only_replies_for_the_current_scope() {
        let (head, tail) = split(1, offer()).unwrap();
        let mut cache = Assemblies::default();
        let now = Instant::now();
        cache.receive(stream(1), head.clone(), now).unwrap();
        cache.receive(stream(2), head.clone(), now).unwrap();
        cache.receive(stream(1), head, now + Duration::from_secs(9)).unwrap();
        let (_, replies) = cache
            .receive(stream(1), tail.unwrap(), now + Duration::from_secs(10))
            .unwrap();
        assert_eq!(outcome(&replies), dto::AttachmentOutcome::Expired);
        let expired = cache.expire(now + Duration::from_secs(10));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].0, stream(2));
    }
    #[test]
    fn identities_and_descriptor_counts_cannot_be_spliced() {
        let (head, tail) = split(1, offer()).unwrap();
        let Message::AttachmentItems(tail) = tail.unwrap() else {
            panic!()
        };
        let mut raw = tail.raw().clone();
        raw.insert(5, V::Uint(0));
        assert!(dto::AttachmentItems::from_raw(raw).is_err());
        let mut raw = tail.into_raw();
        raw.insert(
            4,
            oink_clipboard::wire::offer_id(OfferId {
                clip: ClipId {
                    origin: DeviceId([1; 32]),
                    seq: 99,
                },
                generation: 99,
            })
            .write(),
        );
        let tail = Message::AttachmentItems(dto::AttachmentItems::from_raw(raw).unwrap());
        let mut cache = Assemblies::default();
        let now = Instant::now();
        cache.receive(stream(1), head, now).unwrap();
        assert!(cache.receive(stream(1), tail, now).is_err());
    }
}
