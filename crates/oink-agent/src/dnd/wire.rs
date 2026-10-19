//! Drag metadata projection. Retained Inventory trees remain the content authority.
use oink_clipboard::{model::Inventory, wire as content};
use oink_foundation::{
    DeviceId,
    clipboard::{ClipData, ClipPlatform},
    content::{Holder, OfferId},
};
use oink_protocol::v1::{
    self, RawMap, RawValue as V, dto,
    schema::{Open, WireDto, WireValue},
};
use oink_routing::dnd::*;
fn invalid() -> v1::Error {
    v1::ErrorCode::UnsupportedValue.into()
}
fn envelope(kind: u64, body: RawMap) -> RawMap {
    [(1, V::Uint(kind)), (2, V::Map(body))].into()
}
pub(crate) fn key(value: DragKey) -> dto::DragKey {
    dto::DragKey::from_raw(
        [
            (1, content::offer_id(value.offer).write()),
            (2, V::Uint(value.native_generation)),
        ]
        .into(),
    )
    .expect("native drag identity")
}
pub(crate) fn local_key(value: dto::DragKey) -> v1::Result<DragKey> {
    Ok(DragKey {
        offer: content::local_offer_id(value.offer()?)?,
        native_generation: value.native_generation()?,
    })
}
fn holder(value: Holder) -> dto::Holder {
    dto::Holder::from_raw([(1, content::device(value.device).write()), (2, V::Uint(value.epoch))].into())
        .expect("native holder")
}
fn local_device(value: dto::DeviceId) -> v1::Result<DeviceId> {
    value.require_supported()?;
    Ok(DeviceId(value.public_key()?.0.try_into().map_err(|_| invalid())?))
}
fn local_holder(value: dto::Holder) -> v1::Result<Holder> {
    Ok(Holder {
        device: local_device(value.device()?)?,
        epoch: value.epoch()?,
    })
}
fn state(value: DragState) -> v1::Result<dto::DragState> {
    let (kind, body) = match value {
        DragState::Announced => (0, RawMap::new()),
        DragState::Offered { holder: h, mode } | DragState::Released { holder: h, mode } => (
            if matches!(value, DragState::Offered { .. }) {
                1
            } else {
                2
            },
            [
                (1, holder(h).write()),
                (
                    2,
                    V::Uint(match mode {
                        SourceMode::Held => 0,
                        SourceMode::Exported => 1,
                    }),
                ),
            ]
            .into(),
        ),
        DragState::Revoked { scope, reason } => {
            let (scope_kind, scope_body) = match scope {
                RevokeScope::Drag => (0, RawMap::new()),
                RevokeScope::Holder(h) => (1, [(1, holder(h).write())].into()),
                RevokeScope::Preparation => (2, RawMap::new()),
            };
            (
                3,
                [
                    (1, V::Map(envelope(scope_kind, scope_body))),
                    (2, V::Uint(reason as u64)),
                ]
                .into(),
            )
        }
        DragState::Ended { accepted } => (4, [(1, V::Bool(accepted))].into()),
    };
    dto::DragState::from_raw(envelope(kind, body))
}
fn local_state(value: dto::DragState) -> v1::Result<DragState> {
    Ok(match value.variant()? {
        Open::Known(dto::DragStateKind::Announced) => DragState::Announced,
        Open::Known(kind @ (dto::DragStateKind::Offered | dto::DragStateKind::Released)) => {
            let held = dto::DragHeldState::from_raw(value.body()?)?;
            let holder = local_holder(held.holder()?)?;
            let mode = match held.mode()? {
                dto::SourceMode::Held => SourceMode::Held,
                dto::SourceMode::Exported => SourceMode::Exported,
                _ => return Err(invalid()),
            };
            if kind == dto::DragStateKind::Offered {
                DragState::Offered { holder, mode }
            } else {
                DragState::Released { holder, mode }
            }
        }
        Open::Known(dto::DragStateKind::Revoked) => {
            let revoked = dto::DragRevoked::from_raw(value.body()?)?;
            let scope = revoked.scope()?;
            let scope = match scope.variant()? {
                Open::Known(dto::RevokeScopeKind::Drag) => RevokeScope::Drag,
                Open::Known(dto::RevokeScopeKind::Preparation) => RevokeScope::Preparation,
                Open::Known(dto::RevokeScopeKind::Holder) => {
                    RevokeScope::Holder(local_holder(dto::RevokeHolder::from_raw(scope.body()?)?.holder()?)?)
                }
                _ => return Err(invalid()),
            };
            let reason = match revoked.reason()?.number() {
                0 => RevokeReason::Cancelled,
                1 => RevokeReason::CatcherLost,
                2 => RevokeReason::HoldFailed,
                3 => RevokeReason::NativeEnded,
                4 => RevokeReason::SourceUnavailable,
                5 => RevokeReason::Superseded,
                6 => RevokeReason::PeerLost,
                7 => RevokeReason::PreparationFailed,
                8 => RevokeReason::Capacity,
                9 => RevokeReason::InvalidTransition,
                _ => return Err(invalid()),
            };
            DragState::Revoked { scope, reason }
        }
        Open::Known(dto::DragStateKind::Ended) => DragState::Ended {
            accepted: dto::DragEnded::from_raw(value.body()?)?.accepted()?,
        },
        _ => return Err(invalid()),
    })
}
pub(super) fn native_inventory(id: OfferId, items: &[ClipData], retained: Option<&[u8]>) -> v1::Result<dto::Inventory> {
    if let Some(bytes) = retained {
        let inventory = dto::Inventory::from_raw(v1::cbor::decode_map(bytes, v1::limits::INVENTORY)?)?;
        if content::local_offer_id(inventory.id()?)? != id {
            return Err(invalid());
        }
        return Ok(inventory);
    }
    let inventory = oink_clipboard::content::inventory(id, items)
        .map_err(|_| invalid())?
        .unwrap_or_else(|| Inventory {
            id,
            platform: if cfg!(target_os = "macos") {
                ClipPlatform::Mac
            } else {
                ClipPlatform::Windows
            },
            items: vec![],
            roots: items
                .iter()
                .filter_map(|item| match item {
                    ClipData::FileRoots(roots) => Some(roots.iter().cloned()),
                    _ => None,
                })
                .flatten()
                .collect(),
        });
    content::reliable_inventory(&content::inventory(&inventory)?)
}
pub(crate) fn notice(value: &DragNotice) -> v1::Result<dto::DndNotice> {
    let inventory = native_inventory(value.key.offer, &value.items, value.retained_inventory.as_deref())?;
    let (kind, body) = match value.preview {
        PreviewAvailability::Unknown => (0, RawMap::new()),
        PreviewAvailability::Absent => (1, RawMap::new()),
        PreviewAvailability::Available { revision } => (2, [(1, V::Uint(revision))].into()),
    };
    dto::DndNotice::from_raw(
        [
            (1, key(value.key).write()),
            (2, V::Uint(value.revision)),
            (3, content::device(value.input).write()),
            (4, state(value.state)?.write()),
            (
                5,
                V::Map(
                    [
                        (1, V::Uint(value.metadata.revision)),
                        (
                            2,
                            V::Uint(match value.metadata.readiness {
                                MetadataReadiness::Pending => 0,
                                MetadataReadiness::Ready => 1,
                            }),
                        ),
                    ]
                    .into(),
                ),
            ),
            (6, V::Map(envelope(kind, body))),
            (7, inventory.write()),
        ]
        .into(),
    )
}
pub(crate) fn local_notice(value: &dto::DndNotice) -> v1::Result<DragNotice> {
    local_notice_view(value, true)
}
pub(crate) fn notice_header(value: &dto::DndNotice) -> v1::Result<DragNotice> {
    local_notice_view(value, false)
}
pub(crate) fn attachment_notice_header(value: &dto::AttachmentHead) -> v1::Result<DragNotice> {
    let header = value.header()?;
    if header.variant() != Ok(Open::Known(dto::AttachmentKind::Drag)) {
        return Err(invalid());
    }
    notice_header(&dto::DragAttachment::from_raw(header.body()?)?.notice()?)
}
fn local_notice_view(value: &dto::DndNotice, full: bool) -> v1::Result<DragNotice> {
    let items = if full {
        let inventory = content::local_inventory(&value.inventory()?)?;
        let platform = inventory.platform;
        let mut items = inventory
            .items
            .into_iter()
            .map(|item| ClipData::Native { platform, item })
            .collect::<Vec<_>>();
        if !inventory.roots.is_empty() {
            items.push(ClipData::FileRoots(inventory.roots));
        }
        items
    } else {
        vec![]
    };
    let preview = value.preview()?;
    let preview = match preview.variant()? {
        Open::Known(dto::PreviewAvailabilityKind::UnknownAvailability) => PreviewAvailability::Unknown,
        Open::Known(dto::PreviewAvailabilityKind::Absent) => PreviewAvailability::Absent,
        Open::Known(dto::PreviewAvailabilityKind::Available) => PreviewAvailability::Available {
            revision: dto::PreviewAvailable::from_raw(preview.body()?)?.revision()?,
        },
        _ => return Err(invalid()),
    };
    let metadata = value.metadata()?;
    Ok(DragNotice {
        key: local_key(value.key()?)?,
        revision: value.revision()?,
        input: local_device(value.input()?)?,
        state: local_state(value.state()?)?,
        metadata: MetadataVersion {
            revision: metadata.revision()?,
            readiness: match metadata.readiness()? {
                dto::MetadataReadiness::Pending => MetadataReadiness::Pending,
                dto::MetadataReadiness::Ready => MetadataReadiness::Ready,
                _ => return Err(invalid()),
            },
        },
        preview,
        items,
        retained_inventory: if full {
            Some(value.inventory()?.encode(v1::limits::INVENTORY)?)
        } else {
            None
        },
    })
}
pub(crate) fn preview(
    id: oink_foundation::content::ClipId,
    epoch: u64,
    binding: PreviewBinding,
    image: Option<&oink_foundation::dnd::DragImage>,
) -> v1::Result<dto::DndPreview> {
    if binding.key.offer.clip != id {
        return Err(invalid());
    }
    let mut raw = RawMap::from([
        (1, content::clip_id(id).write()),
        (2, V::Uint(epoch)),
        (4, key(binding.key).write()),
        (5, V::Uint(binding.metadata_revision)),
        (6, V::Uint(binding.preview_revision)),
        (7, V::Text("mime:image/png".into())),
    ]);
    if let Some(image) = image {
        if !image.valid() {
            return Err(invalid());
        }
        raw.insert(
            3,
            dto::DragImage::from_raw(
                [
                    (1, V::Bytes(image.png.to_vec())),
                    (2, image.width.write()),
                    (3, image.height.write()),
                    (4, image.hotspot_x.write()),
                    (5, image.hotspot_y.write()),
                ]
                .into(),
            )?
            .write(),
        );
    }
    dto::DndPreview::from_raw(raw)
}
pub(crate) fn local_preview(value: dto::DndPreview) -> v1::Result<DndMsg> {
    let id = content::local_clip_id(value.id()?)?;
    let key = local_key(value.key()?)?;
    if key.offer.clip != id || value.format()?.0 != "mime:image/png" {
        return Err(invalid());
    }
    let image = value
        .image()?
        .map(|image| -> v1::Result<_> {
            Ok(oink_foundation::dnd::DragImage {
                png: image.png()?.0.into(),
                width: image.width()?,
                height: image.height()?,
                hotspot_x: image.hotspot_x()?,
                hotspot_y: image.hotspot_y()?,
            })
        })
        .transpose()?;
    if image.as_ref().is_some_and(|image| !image.valid()) {
        return Err(invalid());
    }
    Ok(DndMsg::Preview {
        id,
        epoch: value.epoch()?,
        binding: PreviewBinding {
            key,
            metadata_revision: value.metadata_revision()?,
            preview_revision: value.preview_revision()?,
        },
        image,
    })
}
pub(crate) fn encode(value: &DndMsg) -> v1::Result<v1::Message> {
    use v1::Message as M;
    Ok(match value {
        DndMsg::Notice(value) => M::DndNotice(notice(value)?),
        DndMsg::NoticeRequest {
            id,
            holder: h,
            previous,
        } => M::DndNoticeRequest(dto::DndNoticeRequest::from_raw(
            [
                (1, content::clip_id(*id).write()),
                (2, holder(*h).write()),
                (3, content::device(*previous).write()),
            ]
            .into(),
        )?),
        DndMsg::Refused { id, holder: h, reason } => M::DndRefused(dto::DndRefused::from_raw(
            [
                (1, content::clip_id(*id).write()),
                (2, holder(*h).write()),
                (3, V::Uint(*reason as u64)),
            ]
            .into(),
        )?),
        DndMsg::SourceButton {
            key: k,
            revision,
            disposition,
        } => M::DndSourceButton(dto::DndSourceButton::from_raw(
            [
                (1, key(*k).write()),
                (2, V::Uint(*revision)),
                (3, V::Uint(*disposition as u64)),
            ]
            .into(),
        )?),
        DndMsg::Source {
            offer,
            holder: h,
            action,
            cancel_at,
        } => {
            let (kind, body) = match action {
                SourceAction::Handoff { next } => (0, [(1, holder(*next).write())].into()),
                SourceAction::StartDelivery => (1, RawMap::new()),
                SourceAction::Cancel => (2, RawMap::new()),
            };
            let mut raw = RawMap::from([
                (1, content::offer_id(*offer).write()),
                (2, holder(*h).write()),
                (3, V::Map(envelope(kind, body))),
            ]);
            if let Some(p) = cancel_at {
                raw.insert(
                    4,
                    dto::Point::from_raw([(1, p.x.write()), (2, p.y.write())].into())?.write(),
                );
            }
            M::DndSource(dto::DndSource::from_raw(raw)?)
        }
        DndMsg::Caps { enabled } => M::DndCaps(dto::DndCaps::from_raw([(1, V::Bool(*enabled))].into())?),
        DndMsg::Enter { id, epoch } => M::DndEnter(dto::DndEnter::from_raw(
            [(1, content::clip_id(*id).write()), (2, V::Uint(*epoch))].into(),
        )?),
        DndMsg::Preview {
            id,
            epoch,
            binding,
            image,
        } => M::DndPreview(preview(*id, *epoch, *binding, image.as_ref())?),
        DndMsg::PreviewRequest { id, epoch, binding } => {
            if binding.key.offer.clip != *id {
                return Err(invalid());
            }
            M::DndPreviewRequest(dto::DndPreviewRequest::from_raw(
                [
                    (1, content::clip_id(*id).write()),
                    (2, V::Uint(*epoch)),
                    (3, key(binding.key).write()),
                    (4, V::Uint(binding.metadata_revision)),
                    (5, V::Uint(binding.preview_revision)),
                ]
                .into(),
            )?)
        }
    })
}
pub(crate) fn decode(value: v1::Message) -> v1::Result<DndMsg> {
    use v1::Message as M;
    Ok(match value {
        M::DndNotice(value) => DndMsg::Notice(local_notice(&value)?),
        M::DndNoticeRequest(value) => DndMsg::NoticeRequest {
            id: content::local_clip_id(value.id()?)?,
            holder: local_holder(value.holder()?)?,
            previous: local_device(value.previous()?)?,
        },
        M::DndRefused(value) => DndMsg::Refused {
            id: content::local_clip_id(value.id()?)?,
            holder: local_holder(value.holder()?)?,
            reason: match value.reason()?.number() {
                0 => RevokeReason::Cancelled,
                1 => RevokeReason::CatcherLost,
                2 => RevokeReason::HoldFailed,
                3 => RevokeReason::NativeEnded,
                4 => RevokeReason::SourceUnavailable,
                5 => RevokeReason::Superseded,
                6 => RevokeReason::PeerLost,
                7 => RevokeReason::PreparationFailed,
                8 => RevokeReason::Capacity,
                9 => RevokeReason::InvalidTransition,
                _ => return Err(invalid()),
            },
        },
        M::DndSourceButton(value) => DndMsg::SourceButton {
            key: local_key(value.key()?)?,
            revision: value.revision()?,
            disposition: match value.disposition()?.number() {
                0 => SourceButtonDisposition::OriginalSourceOwns,
                1 => SourceButtonDisposition::ReleaseSubmitted,
                2 => SourceButtonDisposition::Released,
                3 => SourceButtonDisposition::ReturnedToOriginalRoute,
                4 => SourceButtonDisposition::Unconfirmed,
                _ => return Err(invalid()),
            },
        },
        M::DndSource(value) => {
            let action = value.action()?;
            let action = match action.variant()? {
                Open::Known(dto::SourceActionKind::Handoff) => SourceAction::Handoff {
                    next: local_holder(dto::SourceHandoff::from_raw(action.body()?)?.next()?)?,
                },
                Open::Known(dto::SourceActionKind::StartDelivery) => SourceAction::StartDelivery,
                Open::Known(dto::SourceActionKind::Cancel) => SourceAction::Cancel,
                _ => return Err(invalid()),
            };
            DndMsg::Source {
                offer: content::local_offer_id(value.offer()?)?,
                holder: local_holder(value.holder()?)?,
                action,
                cancel_at: value
                    .cancel_at()?
                    .map(|p| -> v1::Result<_> { Ok(oink_foundation::Point::new(p.x()?, p.y()?)) })
                    .transpose()?,
            }
        }
        M::DndCaps(value) => DndMsg::Caps {
            enabled: value.enabled()?,
        },
        M::DndEnter(value) => DndMsg::Enter {
            id: content::local_clip_id(value.id()?)?,
            epoch: value.epoch()?,
        },
        M::DndPreview(value) => return local_preview(value),
        M::DndPreviewRequest(value) => {
            let id = content::local_clip_id(value.id()?)?;
            let key = local_key(value.key()?)?;
            if key.offer.clip != id {
                return Err(invalid());
            }
            DndMsg::PreviewRequest {
                id,
                epoch: value.epoch()?,
                binding: PreviewBinding {
                    key,
                    metadata_revision: value.metadata_revision()?,
                    preview_revision: value.preview_revision()?,
                },
            }
        }
        _ => return Err(invalid()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn native_notice() -> DragNotice {
        let id = oink_foundation::content::ClipId {
            origin: DeviceId([1; 32]),
            seq: 1,
        };
        DragNotice {
            key: DragKey {
                offer: oink_foundation::content::OfferId {
                    clip: id,
                    generation: 1,
                },
                native_generation: 1,
            },
            revision: 1,
            input: id.origin,
            state: DragState::Offered {
                holder: Holder {
                    device: DeviceId([2; 32]),
                    epoch: 7,
                },
                mode: SourceMode::Held,
            },
            metadata: MetadataVersion {
                revision: 1,
                readiness: MetadataReadiness::Ready,
            },
            preview: PreviewAvailability::Absent,
            items: vec![],
            retained_inventory: None,
        }
    }
    #[test]
    fn remote_notice_retains_future_inventory_fields_and_headers_do_not_project_payload() {
        let original = notice(&native_notice()).unwrap();
        let mut inventory = original.inventory().unwrap().into_raw();
        inventory.insert(900, V::Array(vec![V::Bool(true), V::Bytes(vec![7])]));
        let mut raw = original.into_raw();
        raw.insert(7, dto::Inventory::from_raw(inventory).unwrap().write());
        let original = dto::DndNotice::from_raw(raw).unwrap();
        let decoded = local_notice(&original).unwrap();
        assert!(decoded.retained_inventory.is_some());
        assert_eq!(notice(&decoded).unwrap(), original);
        let header = notice_header(&original).unwrap();
        assert!(header.items.is_empty());
        assert!(header.retained_inventory.is_none());
        assert_eq!(header.key, decoded.key);
    }
    #[test]
    fn preview_binding_and_png_contract_survive_the_shared_codec() {
        let notice = native_notice();
        let binding = PreviewBinding {
            key: notice.key,
            metadata_revision: 4,
            preview_revision: 8,
        };
        let message = DndMsg::Preview {
            id: notice.key.offer.clip,
            epoch: 7,
            binding,
            image: None,
        };
        assert_eq!(decode(encode(&message).unwrap()).unwrap(), message);
        let mut wire = preview(notice.key.offer.clip, 7, binding, None).unwrap().into_raw();
        wire.insert(7, V::Text("future:artwork".into()));
        assert!(local_preview(dto::DndPreview::from_raw(wire).unwrap()).is_err());
    }
}
