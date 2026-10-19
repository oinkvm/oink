//! Ordinary-thread preparation; the input owner patches only the state envelope.
use super::InputCommand;
use oink_foundation::content::{ClipId, Holder, OfferId};
use oink_foundation::{DeviceId, clipboard::ClipData};
use oink_protocol::v1::{
    self, Message, RawValue, dto,
    schema::{WireDto, WireValue},
};
use oink_routing::dnd::*;
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) struct Queued {
    pub command: InputCommand,
    pub declaration: Result<Option<Declaration>, &'static str>,
}
pub(super) struct Declaration {
    pub id: ClipId,
    full: Option<Prepared>,
    announced: Option<Prepared>,
}
#[derive(Clone)]
struct Prepared {
    head: Message,
    tail: Option<Message>,
}
fn next_attachment() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .expect("drag attachment identity exhausted")
}
impl Declaration {
    pub fn for_notice(&self, notice: &DragNotice) -> Option<(Message, Option<Message>)> {
        if notice.key.offer.clip != self.id || notice.items.is_empty() {
            return None;
        }
        let mut prepared = if notice.state == DragState::Announced {
            &self.announced
        } else {
            &self.full
        }
        .clone()?;
        let attachment = next_attachment();
        let patch = |prefix: dto::DndNotice| -> v1::Result<dto::DndNotice> {
            let mut envelope = crate::dnd::wire::notice(&notice.clone_without_items())?.into_raw();
            let key = dto::DragKey::read(envelope.get(&1).ok_or(v1::ErrorCode::MalformedFrame)?)?;
            let mut inventory = prefix.inventory()?.into_raw();
            inventory.insert(1, key.offer()?.write());
            envelope.insert(7, RawValue::Map(inventory));
            dto::DndNotice::from_raw(envelope)
        };
        match &mut prepared.head {
            Message::DndNotice(value) => *value = patch(value.clone()).ok()?,
            Message::AttachmentHead(value) => {
                let mut raw = value.raw().clone();
                let header = value.header().ok()?;
                let drag = dto::DragAttachment::from_raw(header.body().ok()?).ok()?;
                let notice = patch(drag.notice().ok()?).ok()?;
                let header = dto::AttachmentHeader::from_raw(
                    [
                        (1, dto::AttachmentKind::Drag.write()),
                        (2, RawValue::Map([(1, notice.write())].into())),
                    ]
                    .into(),
                )
                .ok()?;
                raw.insert(1, attachment.write());
                raw.insert(2, header.write());
                *value = dto::AttachmentHead::from_raw(raw).ok()?;
                if let Some(Message::AttachmentItems(tail)) = &mut prepared.tail {
                    let mut raw = tail.raw().clone();
                    raw.insert(1, attachment.write());
                    raw.insert(4, notice.key().ok()?.write());
                    *tail = dto::AttachmentItems::from_raw(raw).ok()?;
                }
            }
            _ => return None,
        }
        Some((prepared.head, prepared.tail))
    }
}
impl Queued {
    /// Hashing and metadata splitting run before queue admission. Only the source
    /// prepares declarations: a previous host never forwards cached offer items.
    pub fn prepare(command: InputCommand) -> Self {
        use oink_routing::DndEvent;
        let contents = match &command {
            InputCommand::DndEvent(
                DndEvent::Detected { id, items }
                | DndEvent::DetectedWithImage { id, items, .. }
                | DndEvent::SourceExported {
                    id, items: Ok(items), ..
                },
            ) => Some((*id, items)),
            _ => None,
        };
        let declaration = contents.map_or(Ok(None), |(id, items)| {
            let encode = |state, items| -> Result<Option<Prepared>, &'static str> {
                let notice = DragNotice {
                    key: DragKey {
                        offer: OfferId {
                            clip: id,
                            generation: u64::MAX,
                        },
                        native_generation: u64::MAX,
                    },
                    revision: u64::MAX,
                    input: DeviceId([255; 32]),
                    state,
                    metadata: MetadataVersion {
                        revision: u64::MAX,
                        readiness: MetadataReadiness::Ready,
                    },
                    preview: PreviewAvailability::Available { revision: u64::MAX },
                    items,
                    retained_inventory: None,
                };
                let notice = crate::dnd::wire::notice(&notice).map_err(|_| "invalid drag declaration")?;
                let (head, tail) = crate::attachment::split(u64::MAX, Message::DndNotice(notice))
                    .map_err(|_| "drag declaration exceeds wire limits")?;
                Ok(Some(Prepared { head, tail }))
            };
            let full = encode(
                DragState::Offered {
                    holder: Holder {
                        device: DeviceId([255; 32]),
                        epoch: u64::MAX,
                    },
                    mode: SourceMode::Held,
                },
                items.clone(),
            )?;
            let roots = items
                .iter()
                .filter(|i| matches!(i, ClipData::FileRoots(_)))
                .cloned()
                .collect();
            let announced = encode(DragState::Announced, roots)?;
            Ok(Some(Declaration { id, full, announced }))
        });
        Self { command, declaration }
    }
}
