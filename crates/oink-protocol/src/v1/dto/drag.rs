//! Source-owned drag identities and transitions; unknown states remain opaque
//! and must never be interpreted as permission to inject or deliver content.
use super::*;
use crate::v1::schema::{Open, OpenEnvelope, enum_body, no_validation, open_enum, required, wire_map};
use crate::v1::{ErrorCode, RawMap, Result};

wire_map!(DragKey,no_validation,{1=>offer:OfferId=required;2=>native_generation:u64=required;});
wire_map!(PreparationKey,no_validation,{1=>drag:DragKey=required;2=>metadata_revision:u64=required;});
open_enum!(MetadataReadiness {Pending=0,Ready=1});
wire_map!(MetadataVersion,no_validation,{1=>revision:u64=required;2=>readiness:MetadataReadiness=required;});
open_enum!(SourceMode {Held=0,Exported=1});
open_enum!(PreviewAvailabilityKind {UnknownAvailability=0,Absent=1,Available=2});
wire_map!(PreviewAvailable,no_validation,{1=>revision:u64=required;});
enum_body!(PreviewAvailabilityKind {Available=>PreviewAvailable});
pub type PreviewAvailability = OpenEnvelope<PreviewAvailabilityKind>;
open_enum!(RevokeReason {Cancelled=0,CatcherLost=1,HoldFailed=2,NativeEnded=3,SourceUnavailable=4,Superseded=5,PeerLost=6,PreparationFailed=7,Capacity=8,InvalidTransition=9});
open_enum!(RevokeScopeKind {Drag=0,Holder=1,Preparation=2});
wire_map!(RevokeHolder,no_validation,{1=>holder:Holder=required;});
enum_body!(RevokeScopeKind {Holder=>RevokeHolder});
pub type RevokeScope = OpenEnvelope<RevokeScopeKind>;
open_enum!(DragStateKind {Announced=0,Offered=1,Released=2,Revoked=3,Ended=4});
wire_map!(DragHeldState,no_validation,{1=>holder:Holder=required;2=>mode:SourceMode=required;});
wire_map!(DragRevoked,no_validation,{1=>scope:RevokeScope=required;2=>reason:RevokeReason=required;});
wire_map!(DragEnded,no_validation,{1=>accepted:bool=required;});
enum_body!(DragStateKind {Offered=>DragHeldState,Released=>DragHeldState,Revoked=>DragRevoked,Ended=>DragEnded});
pub type DragState = OpenEnvelope<DragStateKind>;
open_enum!(SourceActionKind {Handoff=0,StartDelivery=1,Cancel=2});
wire_map!(SourceHandoff,no_validation,{1=>next:Holder=required;});
enum_body!(SourceActionKind {Handoff=>SourceHandoff});
pub type SourceAction = OpenEnvelope<SourceActionKind>;
wire_map!(DragImage,validate_drag_image,{
    1=>png:Bytes<524288>=required;2=>width:f64=required;3=>height:f64=required;
    4=>hotspot_x:f64=required;5=>hotspot_y:f64=required;
});
fn validate_drag_image(raw: &RawMap) -> Result<()> {
    let width = required::<f64>(raw, 2)?;
    let height = required::<f64>(raw, 3)?;
    let x = required::<f64>(raw, 4)?;
    let y = required::<f64>(raw, 5)?;
    if !(0.0..=512.0).contains(&width)
        || width == 0.0
        || !(0.0..=512.0).contains(&height)
        || height == 0.0
        || !(0.0..=width).contains(&x)
        || !(0.0..=height).contains(&y)
    {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    // Pixel dimensions are checked by the receiving image decoder as well.
    let png = required::<Bytes<524288>>(raw, 1)?.0;
    if png.len() < 24
        || &png[..8] != b"\x89PNG\r\n\x1a\n"
        || &png[12..16] != b"IHDR"
        || u32::from_be_bytes(png[16..20].try_into().unwrap()) == 0
        || u32::from_be_bytes(png[20..24].try_into().unwrap()) == 0
        || u32::from_be_bytes(png[16..20].try_into().unwrap()) > 512
        || u32::from_be_bytes(png[20..24].try_into().unwrap()) > 512
    {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}
pub(super) fn validate_dnd_notice(raw: &RawMap) -> Result<()> {
    let key = required::<DragKey>(raw, 1)?;
    let inventory = required::<Inventory>(raw, 7)?;
    if key.offer()? != inventory.id()?
        || key.native_generation()? == 0
        || required::<u64>(raw, 2)? == 0
        || required::<MetadataVersion>(raw, 5)?.revision()? == 0
    {
        return Err(ErrorCode::StateConflict.into());
    }
    if inventory.inline_bytes()? > 4096 {
        return Err(ErrorCode::LimitExceeded.into());
    }
    if required::<DragState>(raw, 4)?.variant()? == Open::Known(DragStateKind::Announced)
        && !inventory.items()?.0.is_empty()
    {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}
