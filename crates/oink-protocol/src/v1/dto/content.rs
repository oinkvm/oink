//! Clipboard and drag share stable offer, representation and file identities.
//! These are retaining wire views; native formats, local paths and OS handles
//! stay in the clipboard domain and are never serialized implicitly.
use super::*;
use crate::v1::schema::{
    Open, OpenEnvelope, WireDto, defaulted, enum_body, no_validation, open_enum, required, wire_map,
};
use crate::v1::{ErrorCode, RawMap, Result};

/// Portable single-URL contract: UTF-8 JSON with `url` and optional `title`.
/// Native-format identities and payloads are retained for same-system delivery.
pub const URL_FORMAT: &str = "oink:url/1";

wire_map!(ClipId,no_validation,{
    1=>origin:DeviceId=required;2=>seq:u64=required;
});
wire_map!(OfferId,no_validation,{
    1=>clip:ClipId=required;2=>generation:u64=required;
});
wire_map!(Holder,no_validation,{
    1=>device:DeviceId=required;2=>epoch:u64=required;
});
wire_map!(DeliveryId,no_validation,{
    1=>offer:OfferId=required;2=>receiver:DeviceId=required;3=>sequence:u64=required;
});

// UnknownSize is the known "not measured" state (0); Unknown(n) preserves a
// future size semantics and must not be interpreted as a measured zero.
open_enum!(ContentSizeKind {UnknownSize=0,Known=1});
wire_map!(KnownSize,no_validation,{1=>bytes:u64=required;});
enum_body!(ContentSizeKind {Known=>KnownSize});
pub type ContentSize = OpenEnvelope<ContentSizeKind>;

open_enum!(RepresentationDataKind {Reader=0,Inline=1,File=2,Unavailable=3,Promise=4});
wire_map!(RepresentationReader,validate_reader,{
    1=>size:ContentSize=required;2=>hash:Option<Digest>=optional;
});
wire_map!(RepresentationInline,no_validation,{1=>bytes:Bytes<65536>=required;});
wire_map!(RepresentationFile,no_validation,{1=>node:u64=required;});
wire_map!(RepresentationUnavailable,no_validation,{1=>reason:ContentUnavailable=required;});
wire_map!(RepresentationPromise,no_validation,{
    1=>node:u64=required;2=>bytes:Bytes<65536>=required;
});
enum_body!(RepresentationDataKind {
    Reader=>RepresentationReader,Inline=>RepresentationInline,File=>RepresentationFile,
    Unavailable=>RepresentationUnavailable,Promise=>RepresentationPromise,
});
pub type RepresentationData = OpenEnvelope<RepresentationDataKind>;

wire_map!(Representation,no_validation,{
    1=>id:u32=required;2=>format:FormatId=required;3=>medium:Medium=required;
    4=>aspect:u32=required;5=>index:i32=required;6=>portable_format:Option<FormatId>=optional;
    7=>data:RepresentationData=required;
});
wire_map!(ContentItem,no_validation,{
    1=>id:u32=required;2=>file:Option<u64>=optional;3=>representations:List<Representation,65535>=required;
    4=>virtual_files:bool=required;
});
open_enum!(NodeKind {File=0,Directory=1});
wire_map!(FileNode,validate_node,{
    1=>id:u64=required;2=>parent:Option<u64>=optional;3=>name:Text<4096>=required;
    4=>kind:NodeKind=required;5=>size:ContentSize=required;6=>version:u64=required;
    7=>modified_ms:Option<i64>=optional;
});
wire_map!(Inventory,validate_inventory,{
    1=>id:OfferId=required;2=>platform:Text<4096>=required;
    3=>items:List<ContentItem,65535>=required;4=>roots:List<FileNode,65535>=required;
});
wire_map!(DirectoryCursor,no_validation,{1=>version:u64=required;2=>offset:u32=required;});
wire_map!(DirectoryPage,validate_page,{
    1=>directory:u64=required;2=>version:u64=required;3=>entries:List<FileNode,1024>=required;
    4=>next:Option<DirectoryCursor>=optional;
});
open_enum!(DemandKind {Clipboard=0,Hover=1,Drop=2});
wire_map!(HolderDemand,no_validation,{1=>holder:Holder=required;});
enum_body!(DemandKind {Hover=>HolderDemand,Drop=>HolderDemand});
pub type Demand = OpenEnvelope<DemandKind>;

fn validate_reader(raw: &RawMap) -> Result<()> {
    let size = required::<ContentSize>(raw, 1)?;
    if size.variant()? == Open::Known(ContentSizeKind::Known)
        && KnownSize::from_raw(size.body()?)?.bytes()? > 256 * 1024 * 1024
    {
        return Err(ErrorCode::LimitExceeded.into());
    }
    Ok(())
}

fn validate_node(raw: &RawMap) -> Result<()> {
    let name = required::<Text<4096>>(raw, 3)?.0;
    if name.is_empty()
        || matches!(name.as_str(), "." | "..")
        || name.contains(['/', '\\', '\0'])
        || defaulted::<Option<u64>>(raw, 2, || None)? == Some(required::<u64>(raw, 1)?)
    {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}

fn validate_inventory(raw: &RawMap) -> Result<()> {
    let mut items = std::collections::BTreeSet::new();
    let mut representations = std::collections::BTreeSet::new();
    let mut roots = std::collections::BTreeSet::new();
    for item in required::<List<ContentItem, 65535>>(raw, 3)?.0 {
        if !items.insert(item.id()?) {
            return Err(ErrorCode::MalformedFrame.into());
        }
        for representation in item.representations()?.0 {
            if !representations.insert(representation.id()?) {
                return Err(ErrorCode::MalformedFrame.into());
            }
            if representations.len() > 65535 {
                return Err(ErrorCode::LimitExceeded.into());
            }
        }
    }
    for root in required::<List<FileNode, 65535>>(raw, 4)?.0 {
        if root.parent()?.is_some() || !roots.insert(root.id()?) {
            return Err(ErrorCode::MalformedFrame.into());
        }
    }
    Ok(())
}

fn validate_page(raw: &RawMap) -> Result<()> {
    let directory = required::<u64>(raw, 1)?;
    let mut ids = std::collections::BTreeSet::new();
    for node in required::<List<FileNode, 1024>>(raw, 3)?.0 {
        if node.parent()? != Some(directory) || !ids.insert(node.id()?) {
            return Err(ErrorCode::MalformedFrame.into());
        }
    }
    if defaulted::<Option<DirectoryCursor>>(raw, 4, || None)?
        .is_some_and(|cursor| cursor.version().ok() != required::<u64>(raw, 2).ok())
    {
        return Err(ErrorCode::StateConflict.into());
    }
    Ok(())
}

impl Inventory {
    /// Reliable heads share one 4 KiB inline budget; Low inventories still use
    /// the independent 64 KiB bound on each individual inline representation.
    pub fn inline_bytes(&self) -> Result<usize> {
        let mut total = 0_usize;
        for item in self.items()?.0 {
            for representation in item.representations()?.0 {
                let data = representation.data()?;
                if data.variant()? == Open::Known(RepresentationDataKind::Inline) {
                    total = total
                        .checked_add(RepresentationInline::from_raw(data.body()?)?.bytes()?.0.len())
                        .ok_or(ErrorCode::LimitExceeded)?;
                }
            }
        }
        Ok(total)
    }
}
