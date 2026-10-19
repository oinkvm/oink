//! Attachment heads carry the operation identity and an ordered prefix. The
//! remainder contains descriptors only and is bound to the same source Boot.
use super::*;
use crate::v1::schema::{Open, OpenEnvelope, WireDto, enum_body, no_validation, required, wire_map};
use crate::v1::{ErrorCode, RawMap, Result};

wire_map!(OfferAttachment,no_validation,{1=>offer:ClipboardOffer=required;});
wire_map!(DragAttachment,no_validation,{1=>notice:DndNotice=required;});
enum_body!(AttachmentKind {Offer=>OfferAttachment,Drag=>DragAttachment});
pub type AttachmentHeader = OpenEnvelope<AttachmentKind>;
wire_map!(InventoryRemainder,validate_remainder,{
    1=>items:List<ContentItem,65535>=required;2=>roots:List<FileNode,65535>=required;
});
impl InventoryRemainder {
    pub fn descriptor_count(&self) -> Result<usize> {
        Ok(self.items()?.0.len() + self.roots()?.0.len())
    }
}
fn validate_remainder(raw: &RawMap) -> Result<()> {
    let items = required::<List<ContentItem, 65535>>(raw, 1)?.0;
    let roots = required::<List<FileNode, 65535>>(raw, 2)?.0;
    if items.len() + roots.len() > 65535 {
        return Err(ErrorCode::LimitExceeded.into());
    }
    for item in items {
        for representation in item.representations()?.0 {
            // Promise bytes describe target-local promise types; they do not
            // contain file data or source-process handles. Inline content and
            // opaque future kinds have no metadata-only guarantee.
            let data = representation.data()?;
            if !matches!(
                data.variant()?,
                Open::Known(
                    RepresentationDataKind::Reader
                        | RepresentationDataKind::File
                        | RepresentationDataKind::Unavailable
                        | RepresentationDataKind::Promise
                )
            ) {
                return Err(ErrorCode::UnsupportedValue.into());
            }
        }
    }
    for root in roots {
        if root.parent()?.is_some() {
            return Err(ErrorCode::MalformedFrame.into());
        }
    }
    Ok(())
}
pub(super) fn validate_attachment_items(raw: &RawMap) -> Result<()> {
    validate_identity(raw, 3)?;
    let remaining = required::<u32>(raw, 5)?;
    if required::<InventoryRemainder>(raw, 2)?.descriptor_count()? != remaining as usize {
        return Err(ErrorCode::StateConflict.into());
    }
    Ok(())
}
pub(super) fn validate_attachment_result(raw: &RawMap) -> Result<()> {
    validate_identity(raw, 2)
}
fn validate_identity(raw: &RawMap, family_key: u32) -> Result<()> {
    let identity = required::<RawMap>(raw, 4)?;
    match required::<AttachmentKind>(raw, family_key)? {
        AttachmentKind::Offer => {
            OfferId::from_raw(identity)?;
        }
        AttachmentKind::Drag => {
            DragKey::from_raw(identity)?;
        }
        AttachmentKind::Unknown(_) => {}
    }
    if required::<u32>(raw, 5)? > 65535 {
        return Err(ErrorCode::LimitExceeded.into());
    }
    Ok(())
}
