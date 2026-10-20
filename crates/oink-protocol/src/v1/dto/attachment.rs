//! Attachment heads carry the operation identity and an ordered prefix. The
//! remainder contains descriptors only and is bound to the same source Boot.
use super::*;
use crate::v1::schema::{OpenEnvelope, WireDto, defaulted, enum_body, no_validation, required, wire_map};
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
        Ok(super::content::array_field(self.raw(), 1)?.len() + super::content::array_field(self.raw(), 2)?.len())
    }
}
fn validate_remainder(raw: &RawMap) -> Result<()> {
    let items = super::content::array_field(raw, 1)?;
    let roots = super::content::array_field(raw, 2)?;
    if items.len() + roots.len() > 65535 {
        return Err(ErrorCode::LimitExceeded.into());
    }
    for item in items {
        for representation in super::content::array_field(item.as_map()?, 3)? {
            // Promise bytes describe target-local promise types; they do not
            // contain file data or source-process handles. Inline content and
            // opaque future kinds have no metadata-only guarantee.
            let data = super::content::map_field(representation.as_map()?, 7)?;
            if !matches!(
                required::<RepresentationDataKind>(data, 1)?,
                RepresentationDataKind::Reader
                    | RepresentationDataKind::File
                    | RepresentationDataKind::Unavailable
                    | RepresentationDataKind::Promise
            ) {
                return Err(ErrorCode::UnsupportedValue.into());
            }
        }
    }
    for root in roots {
        if defaulted::<Option<u64>>(root.as_map()?, 2, || None)?.is_some() {
            return Err(ErrorCode::MalformedFrame.into());
        }
    }
    Ok(())
}
pub(super) fn validate_attachment_items(raw: &RawMap) -> Result<()> {
    validate_identity(raw, 3)?;
    let remaining = required::<u32>(raw, 5)?;
    let remainder = super::content::map_field(raw, 2)?;
    if super::content::array_field(remainder, 1)?.len() + super::content::array_field(remainder, 2)?.len()
        != remaining as usize
    {
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
