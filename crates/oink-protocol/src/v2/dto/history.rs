//! Shared history metadata. All network traffic is a Message variant. Conversion
//! methods are explicit: retaining DTOs remain the wire persistence boundary.
use super::*;
use crate::v2::{
    ErrorCode, RawMap, RawValue as V, Result,
    schema::{WireDto, WireValue, defaulted, no_validation, required, wire_map},
};
use oink_foundation::{clipboard::Sensitivity as S, clipboard_history as h, content as c};

wire_map!(HistoryOrder,no_validation,{
    1=>clock:u64=required;2=>origin:DeviceId=required;3=>seq:u64=required;4=>generation:u64=required;
});
wire_map!(HistoryItem,no_validation,{
    1=>id:u32=required;2=>kind:u8=required;3=>mime:Text<256>=required;4=>name:Option<Text<4096>>=optional;
    5=>bytes:Option<u64>=optional;6=>width:Option<u32>=optional;7=>height:Option<u32>=optional;
    8=>child_count:Option<u64>=optional;9=>children_bytes:Option<u64>=optional;
    10=>file_node:Option<u64>=optional;11=>file_version:Option<u64>=optional;
});
wire_map!(HistoryFormat,no_validation,{
    1=>item:u32=required;2=>representation:u32=required;3=>name:Text<4096>=required;
    4=>portable:Option<u8>=optional;5=>bytes:Option<u64>=optional;6=>file_node:Option<u64>=optional;
});
wire_map!(HistoryInline,no_validation,{
    1=>item:u32=required;2=>representation:u32=required;3=>format:u8=required;4=>bytes:Bytes<32768>=required;
});
wire_map!(HistoryPreview,no_validation,{
    1=>width:u32=required;2=>height:u32=required;3=>jpeg:Bytes<8192>=required;
});
wire_map!(HistoryDigest,no_validation,{
    1=>item:u32=required;2=>class:u8=required;3=>name:Option<Text<4096>>=optional;
    4=>size:Option<u64>=optional;5=>digest:Option<Hash32>=optional;6=>width:Option<u32>=optional;
    7=>height:Option<u32>=optional;8=>modified_ms:Option<i64>=optional;9=>directory:bool=required;
});
wire_map!(HistoryRecord,validate_record,{
    1=>id:OfferId=required;2=>clock:u64=required;3=>time_ms:i64=required;4=>origin:DeviceId=required;
    5=>proxy:Option<DeviceId>=optional;6=>sensitivity:Sensitivity=required;
    7=>items:List<HistoryItem,1024>=required;8=>formats:List<HistoryFormat,4096>=required;
    9=>digests:List<HistoryDigest,4096>=required;10=>inline:List<HistoryInline,4096>=required;
    11=>preview:Option<HistoryPreview>=optional;12=>rev:u8=required;
});
wire_map!(HistoryHolder,validate_holder,{
    1=>record_id:OfferId=required;2=>holder:DeviceId=required;3=>rev:u64=required;
    4=>state:u8=required;5=>scope:u32=required;
});
wire_map!(HistoryHolderRevision,no_validation,{1=>holder:DeviceId=required;2=>rev:u64=required;});
wire_map!(HistoryIndexEntry,validate_index_entry,{
    1=>id:OfferId=required;2=>order:HistoryOrder=required;3=>rev:u8=required;
    4=>holders:List<HistoryHolderRevision,16>=required;5=>head_only:bool=required;
});
wire_map!(HistorySummary,validate_summary,{
    1=>group:Id16=required;2=>digest:Hash32=required;3=>count:u8=required;
    4=>lower:Option<HistoryOrder>=optional;5=>subscribe:bool=required;
});
wire_map!(HistoryIndexRequest,no_validation,{1=>request_id:Id16=required;});
wire_map!(HistoryIndex,validate_index,{
    1=>request_id:Id16=required;2=>entries:List<HistoryIndexEntry,100>=required;3=>digest:Hash32=required;
});
// IDs (reconcile, up to 100) XOR cursor pagination (max 1..=20). An absent
// before cursor means the newest page. `inline=false` omits payload, not preview.
wire_map!(HistoryPull,validate_pull,{
    1=>request_id:Id16=required;2=>ids:Option<List<OfferId,100>>=optional;
    3=>before:Option<HistoryOrder>=optional;4=>max:Option<u8>=optional;5=>inline:bool=required;
});
wire_map!(HistoryRecords,no_validation,{
    1=>request_id:Id16=required;2=>records:List<HistoryRecord,100>=required;
    3=>holders:List<HistoryHolder,1600>=required;4=>more:bool=required;5=>next:Option<HistoryOrder>=optional;
    6=>digest:Hash32=required;
});
wire_map!(HistoryDelta,validate_delta,{
    1=>record:Option<HistoryRecord>=optional;2=>holder:Option<HistoryHolder>=optional;3=>digest:Hash32=required;
});
fn validate_holder(raw: &RawMap) -> Result<()> {
    if required::<u8>(raw, 4)? > 2 {
        return unsupported();
    }
    Ok(())
}
fn validate_index_entry(raw: &RawMap) -> Result<()> {
    let id = id_from(required(raw, 1)?)?;
    let key = required::<HistoryOrder>(raw, 2)?.to_domain()?;
    if key.origin != id.clip.origin
        || key.seq != id.clip.seq
        || key.generation != id.generation
        || required::<u8>(raw, 3)? > 1
    {
        return unsupported();
    }
    let mut seen = std::collections::HashSet::new();
    for h in required::<List<HistoryHolderRevision, 16>>(raw, 4)?.0 {
        if !seen.insert(device_from(h.holder()?)?) {
            return unsupported();
        }
    }
    Ok(())
}
fn validate_index(raw: &RawMap) -> Result<()> {
    let entries = required::<List<HistoryIndexEntry, 100>>(raw, 2)?
        .0
        .iter()
        .map(HistoryIndexEntry::to_domain)
        .collect::<Result<Vec<_>>>()?;
    let mut ids = std::collections::HashSet::new();
    let mut keys = std::collections::HashSet::new();
    if entries.iter().any(|e| !ids.insert(e.id) || !keys.insert(e.key))
        || h::merge_indexes(&[], &entries)
            .map_err(|_| ErrorCode::UnsupportedValue)?
            .len()
            != entries.len()
        || h::window_digest(&entries) != required::<Hash32>(raw, 3)?.0
    {
        return unsupported();
    }
    Ok(())
}
fn validate_record(raw: &RawMap) -> Result<()> {
    decode_record(raw)?
        .validate()
        .map_err(|_| ErrorCode::UnsupportedValue.into())
}
fn validate_summary(raw: &RawMap) -> Result<()> {
    let count: u8 = required(raw, 3)?;
    let lower: Option<HistoryOrder> = defaulted(raw, 4, || None)?;
    if count > 100 || (count == 0) != lower.is_none() {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok(())
}
fn validate_pull(raw: &RawMap) -> Result<()> {
    let ids: Option<List<OfferId, 100>> = defaulted(raw, 2, || None)?;
    let before: Option<HistoryOrder> = defaulted(raw, 3, || None)?;
    let max: Option<u8> = defaulted(raw, 4, || None)?;
    if match ids {
        Some(ids) => ids.0.is_empty() || before.is_some() || max.is_some(),
        None => !max.is_some_and(|n| (1..=20).contains(&n)),
    } {
        return Err(ErrorCode::MalformedFrame.into());
    }
    Ok(())
}
fn validate_delta(raw: &RawMap) -> Result<()> {
    let record: Option<HistoryRecord> = defaulted(raw, 1, || None)?;
    let holder: Option<HistoryHolder> = defaulted(raw, 2, || None)?;
    if record.is_some() == holder.is_some() {
        return Err(ErrorCode::MalformedFrame.into());
    }
    if let Some(holder) = holder {
        holder.to_domain()?;
    }
    Ok(())
}
fn unsupported<T>() -> Result<T> {
    Err(ErrorCode::UnsupportedValue.into())
}
fn device_to(d: oink_foundation::DeviceId) -> V {
    crate::v2::identity::device(d).write()
}
fn device_from(d: DeviceId) -> Result<oink_foundation::DeviceId> {
    crate::v2::identity::read_device(&d)
}
fn id_to(id: c::OfferId) -> V {
    V::Map(
        [
            (
                1,
                V::Map([(1, device_to(id.clip.origin)), (2, V::Uint(id.clip.seq))].into()),
            ),
            (2, V::Uint(id.generation)),
        ]
        .into(),
    )
}
fn id_from(id: OfferId) -> Result<c::OfferId> {
    Ok(c::OfferId {
        clip: c::ClipId {
            origin: device_from(id.clip()?.origin()?)?,
            seq: id.clip()?.seq()?,
        },
        generation: id.generation()?,
    })
}
fn map(pairs: impl IntoIterator<Item = (u32, V)>) -> V {
    V::Map(pairs.into_iter().collect())
}
fn opt<T>(v: Option<T>, f: impl FnOnce(T) -> V) -> V {
    v.map_or(V::Null, f)
}
fn num(v: u32) -> V {
    V::Uint(v.into())
}
fn dims(raw: &RawMap, w: u32, height: u32) -> Result<Option<(u32, u32)>> {
    match (
        defaulted::<Option<u32>>(raw, w, || None)?,
        defaulted::<Option<u32>>(raw, height, || None)?,
    ) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Ok(Some((w, h))),
        (None, None) => Ok(None),
        _ => unsupported(),
    }
}
fn portable(n: u8) -> Result<h::PortableFormat> {
    use h::PortableFormat::*;
    [Text, Html, Rtf, Png, Url]
        .get(n as usize)
        .copied()
        .ok_or_else(|| ErrorCode::UnsupportedValue.into())
}
impl HistoryOrder {
    pub fn from_domain(o: h::OrderKey) -> Result<Self> {
        Self::from_raw(
            [
                (1, V::Uint(o.clock)),
                (2, device_to(o.origin)),
                (3, V::Uint(o.seq)),
                (4, V::Uint(o.generation)),
            ]
            .into(),
        )
    }
    pub fn to_domain(&self) -> Result<h::OrderKey> {
        Ok(h::OrderKey {
            clock: self.clock()?,
            origin: device_from(self.origin()?)?,
            seq: self.seq()?,
            generation: self.generation()?,
        })
    }
}
impl HistoryHolder {
    pub fn from_domain(e: &h::HolderEntry) -> Result<Self> {
        Self::from_raw(
            [
                (1, id_to(e.record_id)),
                (2, device_to(e.holder)),
                (3, V::Uint(e.rev)),
                (4, num(e.state as u32)),
                (5, num(e.scope.0)),
            ]
            .into(),
        )
    }
    pub fn to_domain(&self) -> Result<h::HolderEntry> {
        Ok(h::HolderEntry {
            record_id: id_from(self.record_id()?)?,
            holder: device_from(self.holder()?)?,
            rev: self.rev()?,
            state: match self.state()? {
                0 => h::HolderState::Holding,
                1 => h::HolderState::Released,
                2 => h::HolderState::Gone,
                _ => return unsupported(),
            },
            scope: h::HolderScope(self.scope()?),
        })
    }
}
impl HistoryRecord {
    pub fn to_domain(&self) -> Result<h::ClipRecord> {
        decode_record(self.raw())
    }
    pub fn from_domain(r: &h::ClipRecord) -> Result<Self> {
        r.validate().map_err(|_| ErrorCode::UnsupportedValue)?;
        let items = r
            .items
            .iter()
            .map(|i| {
                map([
                    (1, num(i.id)),
                    (2, num(i.kind as u32)),
                    (3, V::Text(i.mime.clone())),
                    (4, opt(i.name.clone(), V::Text)),
                    (5, opt(i.bytes, V::Uint)),
                    (6, opt(i.dimensions, |d| num(d.0))),
                    (7, opt(i.dimensions, |d| num(d.1))),
                    (8, opt(i.child_count, V::Uint)),
                    (9, opt(i.children_bytes, V::Uint)),
                    (10, opt(i.file_node, V::Uint)),
                    (11, opt(i.file_version, V::Uint)),
                ])
            })
            .collect();
        let formats = r
            .formats
            .iter()
            .map(|f| {
                map([
                    (1, num(f.item)),
                    (2, num(f.representation)),
                    (3, V::Text(f.name.clone())),
                    (4, opt(f.portable, |p| num(p as u32))),
                    (5, opt(f.bytes, V::Uint)),
                    (6, opt(f.file_node, V::Uint)),
                ])
            })
            .collect();
        let digests = r
            .digests
            .iter()
            .map(|d| {
                map([
                    (1, num(d.item)),
                    (2, num(d.class.into())),
                    (3, opt(d.name.clone(), V::Text)),
                    (4, opt(d.size, V::Uint)),
                    (5, opt(d.digest, |d| V::Bytes(d.to_vec()))),
                    (6, opt(d.dimensions, |d| num(d.0))),
                    (7, opt(d.dimensions, |d| num(d.1))),
                    (8, opt(d.modified_ms, |n| n.write())),
                    (9, V::Bool(d.directory)),
                ])
            })
            .collect();
        let inline = r
            .inline
            .iter()
            .map(|p| {
                map([
                    (1, num(p.item)),
                    (2, num(p.representation)),
                    (3, num(p.format as u32)),
                    (4, V::Bytes(p.bytes.clone())),
                ])
            })
            .collect();
        let preview = opt(r.preview.as_ref(), |p| {
            map([(1, num(p.width)), (2, num(p.height)), (3, V::Bytes(p.jpeg.clone()))])
        });
        Self::from_raw(
            [
                (1, id_to(r.id)),
                (2, V::Uint(r.clock)),
                (3, r.time_ms.write()),
                (4, device_to(r.origin)),
                (5, opt(r.proxy, device_to)),
                (
                    6,
                    match r.sensitivity {
                        S::Normal => Sensitivity::Normal,
                        S::Concealed => Sensitivity::Concealed,
                        S::Transient => Sensitivity::Transient,
                        S::AutoGenerated => Sensitivity::AutoGenerated,
                    }
                    .write(),
                ),
                (7, V::Array(items)),
                (8, V::Array(formats)),
                (9, V::Array(digests)),
                (10, V::Array(inline)),
                (11, preview),
                (12, num(r.rev.into())),
            ]
            .into(),
        )
    }
}
fn decode_record(raw: &RawMap) -> Result<h::ClipRecord> {
    let items: List<HistoryItem, 1024> = required(raw, 7)?;
    let formats: List<HistoryFormat, 4096> = required(raw, 8)?;
    let digests: List<HistoryDigest, 4096> = required(raw, 9)?;
    let inline: List<HistoryInline, 4096> = required(raw, 10)?;
    let preview: Option<HistoryPreview> = defaulted(raw, 11, || None)?;
    Ok(h::ClipRecord {
        id: id_from(required(raw, 1)?)?,
        clock: required(raw, 2)?,
        time_ms: required(raw, 3)?,
        origin: device_from(required(raw, 4)?)?,
        proxy: defaulted::<Option<DeviceId>>(raw, 5, || None)?
            .map(device_from)
            .transpose()?,
        sensitivity: match required(raw, 6)? {
            Sensitivity::Normal => S::Normal,
            Sensitivity::Concealed => S::Concealed,
            Sensitivity::Transient => S::Transient,
            Sensitivity::AutoGenerated => S::AutoGenerated,
            _ => return unsupported(),
        },
        items: items
            .0
            .into_iter()
            .map(|i| {
                Ok(h::HistoryItem {
                    id: i.id()?,
                    kind: match i.kind()? {
                        0 => h::ItemKind::Text,
                        1 => h::ItemKind::Image,
                        2 => h::ItemKind::File,
                        3 => h::ItemKind::Directory,
                        4 => h::ItemKind::Other,
                        _ => return unsupported(),
                    },
                    mime: i.mime()?.0,
                    name: i.name()?.map(|n| n.0),
                    bytes: i.bytes()?,
                    dimensions: dims(i.raw(), 6, 7)?,
                    child_count: i.child_count()?,
                    children_bytes: i.children_bytes()?,
                    file_node: i.file_node()?,
                    file_version: i.file_version()?,
                })
            })
            .collect::<Result<_>>()?,
        formats: formats
            .0
            .into_iter()
            .map(|f| {
                Ok(h::HistoryFormat {
                    item: f.item()?,
                    representation: f.representation()?,
                    name: f.name()?.0,
                    portable: f.portable()?.map(portable).transpose()?,
                    bytes: f.bytes()?,
                    file_node: f.file_node()?,
                })
            })
            .collect::<Result<_>>()?,
        digests: digests
            .0
            .into_iter()
            .map(|d| {
                Ok(h::HistoryDigest {
                    item: d.item()?,
                    class: d.class()?,
                    name: d.name()?.map(|n| n.0),
                    size: d.size()?,
                    digest: d.digest()?.map(|d| d.0),
                    dimensions: dims(d.raw(), 6, 7)?,
                    modified_ms: d.modified_ms()?,
                    directory: d.directory()?,
                })
            })
            .collect::<Result<_>>()?,
        inline: inline
            .0
            .into_iter()
            .map(|p| {
                Ok(h::InlinePayload {
                    item: p.item()?,
                    representation: p.representation()?,
                    format: portable(p.format()?)?,
                    bytes: p.bytes()?.0,
                })
            })
            .collect::<Result<_>>()?,
        preview: preview
            .map(|p| -> Result<_> {
                Ok(h::InlinePreview {
                    width: p.width()?,
                    height: p.height()?,
                    jpeg: p.jpeg()?.0,
                })
            })
            .transpose()?,
        rev: required(raw, 12)?,
    })
}
impl HistoryIndexEntry {
    pub fn from_domain(e: &h::IndexEntry) -> Result<Self> {
        Self::from_raw(
            [
                (1, id_to(e.id)),
                (2, HistoryOrder::from_domain(e.key)?.write()),
                (3, num(e.rev.into())),
                (5, V::Bool(e.head_only)),
                (
                    4,
                    V::Array(
                        e.holders
                            .iter()
                            .map(|(d, r)| map([(1, device_to(*d)), (2, V::Uint(*r))]))
                            .collect(),
                    ),
                ),
            ]
            .into(),
        )
    }
    pub fn to_domain(&self) -> Result<h::IndexEntry> {
        Ok(h::IndexEntry {
            id: id_from(self.id()?)?,
            key: self.order()?.to_domain()?,
            rev: self.rev()?,
            head_only: self.head_only()?,
            holders: self
                .holders()?
                .0
                .into_iter()
                .map(|r| Ok((device_from(r.holder()?)?, r.rev()?)))
                .collect::<Result<_>>()?,
        })
    }
}
impl HistoryDelta {
    pub fn from_domain(change: &h::HistoryChange, digest: [u8; 32]) -> Result<Self> {
        let (record, holder) = match change {
            h::HistoryChange::Record(r) => (HistoryRecord::from_domain(r)?.write(), V::Null),
            h::HistoryChange::Holder(e) => (V::Null, HistoryHolder::from_domain(e)?.write()),
        };
        Self::from_raw([(1, record), (2, holder), (3, V::Bytes(digest.to_vec()))].into())
    }
    pub fn to_domain(&self) -> Result<h::HistoryChange> {
        if let Some(r) = self.record()? {
            Ok(h::HistoryChange::Record(r.to_domain()?))
        } else {
            Ok(h::HistoryChange::Holder(
                self.holder()?.ok_or(ErrorCode::MalformedFrame)?.to_domain()?,
            ))
        }
    }
}

/// Domain query used by PC serving and phone paging. IDs are for reconciliation;
/// the page form is always capped at 20 by both wire validation and the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryQuery {
    Ids(Vec<c::OfferId>),
    Page { before: Option<h::OrderKey>, max: u8 },
}
impl HistoryPull {
    pub fn from_query(request_id: [u8; 16], query: HistoryQuery, inline: bool) -> Result<Self> {
        let (ids, before, max) = match query {
            HistoryQuery::Ids(ids) => (V::Array(ids.into_iter().map(id_to).collect()), V::Null, V::Null),
            HistoryQuery::Page { before, max } => (
                V::Null,
                opt(before, |b| HistoryOrder::from_domain(b).expect("domain order").write()),
                num(max.into()),
            ),
        };
        Self::from_raw(
            [
                (1, V::Bytes(request_id.to_vec())),
                (2, ids),
                (3, before),
                (4, max),
                (5, V::Bool(inline)),
            ]
            .into(),
        )
    }
    pub fn query(&self) -> Result<HistoryQuery> {
        if let Some(ids) = self.ids()? {
            Ok(HistoryQuery::Ids(
                ids.0.into_iter().map(id_from).collect::<Result<_>>()?,
            ))
        } else {
            Ok(HistoryQuery::Page {
                before: self.before()?.map(|b| b.to_domain()).transpose()?,
                max: self.max()?.ok_or(ErrorCode::MalformedFrame)?,
            })
        }
    }
}
impl HistorySummary {
    pub fn from_window(group: oink_foundation::GroupId, window: &h::HistoryWindow, subscribe: bool) -> Result<Self> {
        Self::from_raw(
            [
                (1, V::Bytes(group.0.to_vec())),
                (2, V::Bytes(window.digest().to_vec())),
                (3, num(window.records().len() as u32)),
                (
                    4,
                    opt(window.records().last(), |r| {
                        HistoryOrder::from_domain(r.key()).expect("domain order").write()
                    }),
                ),
                (5, V::Bool(subscribe)),
            ]
            .into(),
        )
    }
}
impl HistoryIndex {
    pub fn from_window(request_id: [u8; 16], window: &h::HistoryWindow) -> Result<Self> {
        Self::from_raw(
            [
                (1, V::Bytes(request_id.to_vec())),
                (
                    2,
                    V::Array(
                        window
                            .index()
                            .iter()
                            .map(|e| HistoryIndexEntry::from_domain(e).map(|d| d.write()))
                            .collect::<Result<_>>()?,
                    ),
                ),
                (3, V::Bytes(window.digest().to_vec())),
            ]
            .into(),
        )
    }
    pub fn domain_entries(&self) -> Result<Vec<h::IndexEntry>> {
        self.entries()?.0.iter().map(HistoryIndexEntry::to_domain).collect()
    }
}
impl HistoryRecords {
    pub fn from_page(request_id: [u8; 16], page: &h::HistoryPage, digest: [u8; 32]) -> Result<Self> {
        Self::from_raw(
            [
                (1, V::Bytes(request_id.to_vec())),
                (
                    2,
                    V::Array(
                        page.records
                            .iter()
                            .map(|r| HistoryRecord::from_domain(r).map(|d| d.write()))
                            .collect::<Result<_>>()?,
                    ),
                ),
                (
                    3,
                    V::Array(
                        page.holders
                            .iter()
                            .map(|h| HistoryHolder::from_domain(h).map(|d| d.write()))
                            .collect::<Result<_>>()?,
                    ),
                ),
                (4, V::Bool(page.more)),
                (
                    5,
                    opt(page.next, |o| {
                        HistoryOrder::from_domain(o).expect("domain order").write()
                    }),
                ),
                (6, V::Bytes(digest.to_vec())),
            ]
            .into(),
        )
    }
    pub fn to_page(&self) -> Result<h::HistoryPage> {
        Ok(h::HistoryPage {
            records: self
                .records()?
                .0
                .iter()
                .map(HistoryRecord::to_domain)
                .collect::<Result<_>>()?,
            holders: self
                .holders()?
                .0
                .iter()
                .map(HistoryHolder::to_domain)
                .collect::<Result<_>>()?,
            more: self.more()?,
            next: self.next()?.map(|o| o.to_domain()).transpose()?,
        })
    }
}
