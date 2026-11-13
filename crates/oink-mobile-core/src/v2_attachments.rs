use oink_protocol::v2::{
    self, ErrorCode, Message, RawMap, RawValue, dto, limits,
    schema::{Open, WireDto, WireValue},
};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

const PENDING_BYTES: usize = 32 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(10);

/// Pair the reliable offer prefix with the Low descriptor remainder. The two
/// transports may deliver in either order.
#[derive(Default)]
pub(crate) struct ClipboardAttachments {
    pending: HashMap<String, Pending>,
}

struct Pending {
    id: u64,
    identity: RawMap,
    remaining: u32,
    created: Instant,
    bytes: usize,
    complete: bool,
    prefix: Option<dto::ClipboardOffer>,
    tail: Option<dto::InventoryRemainder>,
}

impl ClipboardAttachments {
    pub(crate) fn remove(&mut self, peer: &str) {
        self.pending.remove(peer);
    }

    pub(crate) fn push(&mut self, peer: &str, message: Message) -> v2::Result<Option<dto::ClipboardOffer>> {
        self.pending.retain(|_, pending| pending.created.elapsed() < DEADLINE);
        let bytes = v2::cbor::map_len(message.raw())?;
        let (id, identity, remaining, prefix, tail) = match message {
            Message::AttachmentHead(head) => {
                let header = head.header()?;
                if header.variant()? != Open::Known(dto::AttachmentKind::Offer) {
                    return Err(ErrorCode::UnsupportedValue.into());
                }
                let offer = dto::OfferAttachment::from_raw(header.body()?)?.offer()?;
                (head.id()?, offer.id()?.into_raw(), head.remaining()?, Some(offer), None)
            }
            Message::AttachmentItems(items) => {
                let tail = items.items()?;
                if items.family()? != dto::AttachmentKind::Offer
                    || tail.descriptor_count()? != items.remaining()? as usize
                {
                    return Err(ErrorCode::StateConflict.into());
                }
                (items.id()?, items.identity()?, items.remaining()?, None, Some(tail))
            }
            _ => return Err(ErrorCode::UnsupportedValue.into()),
        };
        if self.pending.get(peer).is_some_and(|pending| pending.id > id) {
            return Ok(None);
        }
        if self.pending.get(peer).is_some_and(|pending| pending.id < id) {
            self.remove(peer);
        }
        let total = self.pending.values().map(|pending| pending.bytes).sum::<usize>();
        if bytes > limits::INVENTORY || total.saturating_add(bytes) > PENDING_BYTES {
            return Err(ErrorCode::LimitExceeded.into());
        }
        let pending = self.pending.entry(peer.to_owned()).or_insert_with(|| Pending {
            id,
            identity: identity.clone(),
            remaining,
            created: Instant::now(),
            bytes: 0,
            complete: false,
            prefix: None,
            tail: None,
        });
        if pending.identity != identity || pending.remaining != remaining {
            self.remove(peer);
            return Err(ErrorCode::StateConflict.into());
        }
        if pending.complete
            || (prefix.is_some() && pending.prefix.is_some())
            || (tail.is_some() && pending.tail.is_some())
        {
            return Ok(None);
        }
        pending.bytes += bytes;
        if prefix.is_some() {
            pending.prefix = prefix;
        }
        if tail.is_some() {
            pending.tail = tail;
        }
        if pending.prefix.is_none() || remaining != 0 && pending.tail.is_none() {
            return Ok(None);
        }
        pending.complete = true;
        pending.bytes = 0;
        let prefix = pending.prefix.take().expect("present prefix");
        let Some(tail) = pending.tail.take() else {
            return Ok(Some(prefix));
        };
        let mut inventory = prefix.inventory()?.into_raw();
        let Some(RawValue::Array(items)) = inventory.get_mut(&3) else {
            return Err(ErrorCode::MalformedFrame.into());
        };
        items.extend(tail.items()?.0.iter().map(WireValue::write));
        let Some(RawValue::Array(roots)) = inventory.get_mut(&4) else {
            return Err(ErrorCode::MalformedFrame.into());
        };
        roots.extend(tail.roots()?.0.iter().map(WireValue::write));
        let inventory = dto::Inventory::from_raw(inventory)?;
        inventory.encode(limits::INVENTORY)?;
        let mut offer = prefix.into_raw();
        offer.insert(6, inventory.write());
        dto::ClipboardOffer::from_raw(offer).map(Some)
    }
}
