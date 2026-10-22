//! NSPasteboard items are inspected in their native order. Each representation
//! is read from the current system board on demand; only native Drop takes a snapshot.
use crate::content::{self, Provider, Snapshot, Translator};
use crate::model::{
    ContentItem, ContentSize, Inventory, ItemId, Medium, NativeFormat, NodeId, REPRESENTATION_BYTES, Representation,
    RepresentationData, RepresentationId, StandardFormat, Unavailable,
};
use crate::{
    CapturedClipboard, ClipData, ClipSnapshot, ClipboardError, NativeSpec,
    store::{Batch, BatchId, StoreKind},
};
use objc2::{
    AnyThread, ClassType, DefinedClass, define_class, msg_send,
    rc::{Retained, autoreleasepool},
    runtime::ProtocolObject,
};
use objc2_app_kit::{
    NSPasteboard, NSPasteboardItem, NSPasteboardItemDataProvider, NSPasteboardType, NSPasteboardWriting,
};
use objc2_foundation::{NSArray, NSData, NSObject, NSObjectProtocol, NSString, NSURL};
use oink_foundation::content::ClipId;
use oink_foundation::content::OfferId;
use oink_foundation::{DeviceId, clipboard::ClipPlatform};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

fn board(name: Option<&str>) -> Retained<NSPasteboard> {
    name.map_or_else(NSPasteboard::generalPasteboard, |name| {
        NSPasteboard::pasteboardWithName(&NSString::from_str(name))
    })
}
pub(super) fn standard(name: &str) -> Option<StandardFormat> {
    Some(match name {
        "public.utf8-plain-text"
        | "public.utf16-plain-text"
        | "public.utf16-external-plain-text"
        | "NSStringPboardType" => StandardFormat::Text,
        "public.url" => StandardFormat::Url,
        "public.html" | "NSHTMLPboardType" => StandardFormat::Html,
        "public.rtf" | "NSRTFPboardType" => StandardFormat::Rtf,
        "NSTIFFPboardType" => StandardFormat::Png,
        n if crate::formats::is_marker(ClipPlatform::Mac, n)
            || matches!(
                n,
                "de.petermaurer.TransientPasteboardType"
                    | "com.agilebits.onepassword"
                    | "com.typeit4me.clipping"
                    | "Pasteboard generator type"
            ) =>
        {
            StandardFormat::Privacy
        }
        n if bitmap_type(n) => StandardFormat::Png,
        _ => return None,
    })
}

/// Only query the OS decoder registry here. Classification must never invoke a
/// lazy drag provider or a user-installed conversion service during hover.
fn bitmap_type(name: &str) -> bool {
    use objc2_core_foundation::CFString;
    static TYPES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let types = TYPES.get_or_init(|| {
        // SAFETY: NSBitmapImageRep inherits this NSImageRep class method.
        let types: Retained<NSArray<NSString>> =
            unsafe { msg_send![objc2_app_kit::NSBitmapImageRep::class(), imageUnfilteredTypes] };
        types.iter().map(|ty| ty.to_string()).collect()
    });
    let candidate = CFString::from_str(name);
    types.iter().any(|ty| {
        // SAFETY: both arguments are retained CFStrings. This metadata API is
        // available on the supported macOS baseline and does not read payloads.
        #[allow(deprecated)]
        unsafe {
            objc2_core_services::UTTypeConformsTo(&candidate, &CFString::from_str(ty))
        }
    })
}
pub(super) fn file_path(item: &NSPasteboardItem) -> Option<PathBuf> {
    let value = item.stringForType(&NSString::from_str("public.file-url"))?;
    let url = NSURL::URLWithString(&value)?.filePathURL()?;
    Some(PathBuf::from(url.path()?.to_string()))
}
pub(super) fn promise_type(name: &str) -> bool {
    static NATIVE: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
    matches!(
        name,
        "com.apple.pasteboard.promised-file-url"
            | "com.apple.pasteboard.promised-file-content-type"
            | "NSFilesPromisePboardType"
            | "Apple files promise pasteboard type"
            | "com.apple.NSFilePromiseItemMetaData"
            | "com.apple.pasteboard.NSFilePromiseID"
    ) || NATIVE
        .get_or_init(|| {
            objc2_app_kit::NSFilePromiseReceiver::readableDraggedTypes()
                .iter()
                .map(|ty| ty.to_string())
                .collect()
        })
        .contains(name)
}
pub(super) fn inventory_on_board(pb: &NSPasteboard, id: OfferId) -> Inventory {
    inventory_on_board_with_promises(pb, id, false)
}
pub(super) fn drag_inventory(pb: &NSPasteboard, id: OfferId) -> Inventory {
    let mut inventory = inventory_on_board_with_promises(pb, id, true);
    let classes = NSArray::from_slice(&[objc2_app_kit::NSFilePromiseReceiver::class()]);
    // File types are native promise metadata; reading them does not receive it.
    // SAFETY: the requested class implements NSPasteboardReading.
    if let Some(objects) = unsafe { pb.readObjectsForClasses_options(&classes, None) } {
        let mut receivers = objects
            .into_iter()
            .filter_map(|object| object.downcast::<objc2_app_kit::NSFilePromiseReceiver>().ok());
        for item in &mut inventory.items {
            if !item
                .representations
                .iter()
                .any(|rep| matches!(rep.data, RepresentationData::Promise(..)))
            {
                continue;
            }
            let Some(receiver) = receivers.next() else {
                break;
            };
            let types = receiver.fileTypes();
            if types.len() == 1 {
                let ty = types.objectAtIndex(0).to_string();
                if ty.len() <= 4096 && !ty.contains('\0') {
                    for rep in &mut item.representations {
                        if let RepresentationData::Promise(_, bytes) = &mut rep.data
                            && matches!(&rep.format, NativeFormat::Mac(name) if name == "com.apple.NSFilePromiseItemMetaData" || name == "com.apple.pasteboard.NSFilePromiseID")
                        {
                            *bytes = ty.as_bytes().to_vec();
                        }
                    }
                }
            }
        }
    }
    inventory
}
fn inventory_on_board_with_promises(pb: &NSPasteboard, id: OfferId, proxy: bool) -> Inventory {
    let mut next = 0_u32;
    let items = pb
        .pasteboardItems()
        .map(|items| {
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let file = file_path(&item).map(|_| NodeId(index as u64 + 1));
                    let representations: Vec<Representation> = item
                        .types()
                        .iter()
                        .map(|ty| {
                            let name = ty.to_string();
                            next += 1;
                            let data = if name == "public.file-url"
                                && let Some(file) = file
                            {
                                RepresentationData::File(file)
                            } else if promise_type(&name) && proxy {
                                if name == "com.apple.pasteboard.promised-file-content-type" {
                                    // Type/extension metadata is ordinary, ordered data.
                                    RepresentationData::Reader {
                                        size: ContentSize::Unknown,
                                        hash: None,
                                    }
                                } else {
                                    let metadata = if matches!(
                                        name.as_str(),
                                        "NSFilesPromisePboardType" | "Apple files promise pasteboard type"
                                    ) {
                                        item.dataForType(&ty)
                                            .filter(|data| data.len() <= 65536)
                                            .map(|data| data.to_vec())
                                            .unwrap_or_default()
                                    } else {
                                        Vec::new()
                                    };
                                    RepresentationData::Promise(NodeId((1u64 << 32) + index as u64), metadata)
                                }
                            } else if promise_type(&name) {
                                RepresentationData::Unavailable(Unavailable::Unsupported)
                            } else {
                                RepresentationData::Reader {
                                    size: ContentSize::Unknown,
                                    hash: None,
                                }
                            };
                            Representation {
                                id: RepresentationId(next),
                                standard: standard(&name),
                                format: NativeFormat::Mac(name),
                                medium: Medium::Bytes,
                                aspect: 1,
                                index: -1,
                                data,
                            }
                        })
                        .collect();
                    ContentItem {
                        virtual_files: file.is_none()
                            && representations
                                .iter()
                                .any(|rep: &Representation| matches!(rep.data, RepresentationData::Promise(..))),
                        id: ItemId(index as u32),
                        file,
                        representations,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    Inventory {
        roots: Vec::new(),
        id,
        platform: ClipPlatform::Mac,
        items,
    }
}

/// Used from the original drag's real performDrop callback only.
/// No source URL bytes are stored as a publishable target file representation.
pub(super) fn snapshot_board(pb: &NSPasteboard, data: &Path, id: OfferId) -> io::Result<Arc<dyn Provider>> {
    let mut inventory = drag_inventory(pb, id);
    inventory.validate().map_err(content::unavailable)?;
    let batch = Batch::create(data, StoreKind::SourceSnapshot, BatchId::random()?)?;
    let native_items = pb.pasteboardItems();
    for item in &mut inventory.items {
        let native = native_items
            .as_ref()
            .and_then(|items| items.iter().nth(item.id.0 as usize));
        for rep in &mut item.representations {
            if !matches!(rep.data, RepresentationData::Reader { .. }) {
                continue;
            }
            let NativeFormat::Mac(name) = &rep.format else {
                unreachable!()
            };
            let read = oink_foundation::drag_trace::Trace::new(
                oink_foundation::drag_trace::stage::PASTEBOARD_READ,
                oink_foundation::drag_trace::Context {
                    offer: id.clip.seq,
                    representation: rep.id.0,
                    ..oink_foundation::drag_trace::Context::current()
                },
            );
            let raw = native
                .as_ref()
                .and_then(|item| item.dataForType(&NSString::from_str(name)));
            read.result(if raw.is_some() { 0 } else { -1 });
            drop(read);
            rep.data = match raw {
                Some(bytes) if bytes.len() as u64 <= REPRESENTATION_BYTES => {
                    // Bound the native length before making an owned Rust copy;
                    // a provider can return an NSMutableData subclass, so do not
                    // borrow its storage across disk work using unchecked bytes.
                    let bytes = bytes.to_vec();
                    match batch.snapshot(rep.id.0, &mut io::Cursor::new(&bytes)) {
                        Ok((size, _hash)) if rep.standard == Some(StandardFormat::Privacy) && size <= 65536 => {
                            RepresentationData::Inline(bytes.to_vec())
                        }
                        Ok((size, hash)) => RepresentationData::Reader {
                            size: ContentSize::Known(size),
                            hash: Some(hash),
                        },
                        Err(error) => RepresentationData::Unavailable(content::reason(&error)),
                    }
                }
                Some(_) => RepresentationData::Unavailable(Unavailable::LimitExceeded),
                None => RepresentationData::Unavailable(Unavailable::ReadFailed),
            };
        }
    }
    let snapshot = Snapshot::from_parts(inventory, batch, Arc::new(MacTranslator))?;
    match file_namespace(pb, false)? {
        Some(files) => Ok(content::with_files(snapshot, files)),
        None => Ok(snapshot),
    }
}

/// No native item is retained: a paste request reacquires the board and its items.
/// Registry serving and capture run on blocking executors, outside input/network.
/// changeCount is checked around the native read, including Oink's own writes.
fn read_current(name: Option<&str>, count: u64, item: usize, ty: &str) -> io::Result<crate::FileReader> {
    autoreleasepool(|_| {
        let pb = board(name);
        if pb.changeCount() as u64 != count {
            return Err(content::clipboard::changed());
        }
        let value = pb
            .pasteboardItems()
            .and_then(|items| items.iter().nth(item))
            .and_then(|item| item.dataForType(&NSString::from_str(ty)));
        if pb.changeCount() as u64 != count {
            return Err(content::clipboard::changed());
        }
        let value = value.ok_or_else(|| content::unavailable(Unavailable::ReadFailed))?;
        if value.len() as u64 > REPRESENTATION_BYTES {
            return Err(content::unavailable(Unavailable::LimitExceeded));
        }
        Ok(Box::new(io::Cursor::new(value.to_vec())) as crate::FileReader)
    })
}

pub(super) fn capture(name: Option<&str>, _data: &Path) -> Result<CapturedClipboard, ClipboardError> {
    autoreleasepool(|_| {
        let pb = board(name);
        let count = pb.changeCount() as u64;
        let id = OfferId {
            clip: ClipId {
                origin: DeviceId([0; 32]),
                seq: 0,
            },
            generation: count,
        };
        let inventory = inventory_on_board(&pb, id);
        // Do not render a payload merely to learn its size: even NSData.length
        // requires invoking the lazy owner. Unknown sizes forbid whole-offer auto pull.
        let reads: HashMap<_, _> = inventory
            .items
            .iter()
            .flat_map(|item| {
                item.representations.iter().filter_map(move |rep| {
                    let NativeFormat::Mac(ty) = &rep.format else {
                        return None;
                    };
                    Some((rep.id, (item.id.0 as usize, ty.clone())))
                })
            })
            .collect();
        let name = name.map(str::to_owned);
        let check_name = name.clone();
        let check: content::clipboard::Check = Arc::new(move || {
            autoreleasepool(|_| {
                if board(check_name.as_deref()).changeCount() as u64 == count {
                    Ok(())
                } else {
                    Err(content::clipboard::changed())
                }
            })
        });
        let names: Vec<String> = pb
            .types()
            .map(|types| types.iter().map(|n| n.to_string()).collect())
            .unwrap_or_default();
        let sensitivity = super::pasteboard::sensitivity_of(&names);
        let files =
            file_namespace(&pb, true).map_err(|error| ClipboardError::ContentUnavailable(content::reason(&error)))?;
        let native = content::clipboard::Clipboard::new(
            inventory,
            Box::new(move |id| {
                let (item, ty) = reads
                    .get(&id)
                    .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))?;
                read_current(name.as_deref(), count, *item, ty)
            }),
            check,
            Arc::new(MacTranslator),
            files,
        )
        .map_err(|error| ClipboardError::ContentUnavailable(content::reason(&error)))?;
        if pb.changeCount() as u64 != count {
            return Err(ClipboardError::Busy);
        }
        Ok(CapturedClipboard {
            snapshot: ClipSnapshot {
                items: content::items(&native.inventory()),
                sensitivity,
                change_count: count,
            },
            content: Some(native),
        })
    })
}

pub(super) fn file_namespace(
    pb: &NSPasteboard,
    clipboard: bool,
) -> io::Result<Option<Arc<dyn content::namespace::Namespace>>> {
    let roots: Vec<_> = pb
        .pasteboardItems()
        .map(|items| {
            items
                .iter()
                .enumerate()
                .filter_map(|(index, item)| file_path(&item).map(|path| (NodeId(index as u64 + 1), path)))
                .collect()
        })
        .unwrap_or_default();
    if roots.is_empty() {
        return Ok(None);
    }
    Ok(Some(if clipboard {
        content::namespace::Local::clipboard(roots)?
    } else {
        content::namespace::Local::new(roots)?
    }))
}

/// Native UTF-16 defaults to this platform's byte order; a BOM overrides it.
/// Invalid encodings must stay unavailable instead of becoming replacement text.
fn decode_text(bytes: &[u8], native_utf16: bool) -> io::Result<String> {
    let invalid = || content::unavailable(Unavailable::InvalidData);
    let (bytes, order) = match bytes {
        [255, 254, rest @ ..] => (rest, Some(true)),
        [254, 255, rest @ ..] => (rest, Some(false)),
        [239, 187, 191, rest @ ..] if !native_utf16 => (rest, None),
        bytes => (bytes, native_utf16.then_some(cfg!(target_endian = "little"))),
    };
    if let Some(le) = order {
        if !bytes.len().is_multiple_of(2) {
            return Err(invalid());
        }
        let words: Vec<_> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| {
                if le {
                    u16::from_le_bytes(*b)
                } else {
                    u16::from_be_bytes(*b)
                }
            })
            .collect();
        String::from_utf16(&words).map_err(|_| invalid())
    } else {
        String::from_utf8(bytes.to_vec()).map_err(|_| invalid())
    }
}

pub(super) struct MacTranslator;
impl Translator for MacTranslator {
    fn standard(&self, rep: &Representation, bytes: &[u8]) -> io::Result<Vec<ClipData>> {
        let NativeFormat::Mac(name) = &rep.format else {
            return Err(content::unavailable(Unavailable::Unsupported));
        };
        let value = match rep.standard {
            Some(StandardFormat::Text) => {
                let text = decode_text(bytes, name.contains("utf16"))?;
                ClipData::Text(super::pasteboard::normalize_newlines(&text))
            }
            Some(StandardFormat::Html) => ClipData::Html(decode_text(bytes, false)?),
            Some(StandardFormat::Rtf) => ClipData::Rtf(bytes.to_vec()),
            Some(StandardFormat::Png) => {
                let data = NSData::with_bytes(bytes);
                let (width, height) = super::pasteboard::bitmap_dimensions(&data)
                    .ok_or_else(|| content::unavailable(Unavailable::InvalidData))?;
                if width.checked_mul(height).is_none_or(|n| n > REPRESENTATION_BYTES / 4) {
                    return Err(content::unavailable(Unavailable::LimitExceeded));
                }
                if name == "public.png" {
                    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n")
                        || objc2_app_kit::NSBitmapImageRep::imageRepWithData(&data).is_none()
                    {
                        return Err(content::unavailable(Unavailable::InvalidData));
                    }
                    ClipData::Png(bytes.to_vec())
                } else {
                    let png = autoreleasepool(|_| super::pasteboard::bitmap_to_png(&data))
                        .ok_or_else(|| content::unavailable(Unavailable::InvalidData))?;
                    ClipData::Png(png)
                }
            }
            Some(StandardFormat::Privacy) => {
                let canonical = match name.as_str() {
                    "de.petermaurer.TransientPasteboardType" => crate::formats::MAC_MARKERS[0],
                    "com.agilebits.onepassword" => crate::formats::MAC_MARKERS[1],
                    "com.typeit4me.clipping" | "Pasteboard generator type" => crate::formats::MAC_MARKERS[2],
                    name => name,
                };
                crate::formats::custom(ClipPlatform::Mac, canonical, bytes.to_vec())
            }
            _ => return Err(content::unavailable(Unavailable::Unsupported)),
        };
        Ok(vec![value])
    }
}

struct DataIvars {
    source: Arc<dyn Provider>,
    bindings: Vec<(String, RepresentationId)>,
    foreign: bool,
}
define_class!(
    #[unsafe(super(NSObject))]
    #[name = "OinkNativePasteboardData"]
    #[ivars = DataIvars]
    struct DataProvider;
    unsafe impl NSObjectProtocol for DataProvider {}
    unsafe impl NSPasteboardItemDataProvider for DataProvider {
        #[unsafe(method(pasteboard:item:provideDataForType:))]
        fn provide(&self, _board: Option<&NSPasteboard>, item: &NSPasteboardItem, ty: &NSPasteboardType) {
            let name = ty.to_string();
            // Several source representations can project to the same native
            // type. AppKit registers that type once, but every candidate retains
            // its own read/failure boundary in the original source order.
            for (_, id) in self.ivars().bindings.iter().filter(|(n, _)| *n == name) {
                let mut decoded_link = false;
                let result = (|| {
                    let mut reader = self.ivars().source.open(*id)?;
                    let bytes = content::read_all(&mut reader)?;
                    if !self.ivars().foreign {
                        return Ok(bytes);
                    }
                    let inventory = self.ivars().source.inventory();
                    let representation =
                        content::find(&inventory, *id).ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))?;
                    if representation.standard == Some(StandardFormat::Url) {
                        let link = content::url::Link::decode(&bytes)?;
                        decoded_link = true;
                        return link.render(ClipPlatform::Mac, &name);
                    }
                    let translated = content::decode_standard(representation, &bytes)?;
                    let data = crate::formats::for_platform(&translated, ClipPlatform::Mac, crate::Sensitivity::Normal);
                    for data in data {
                        let pair = mac_bytes(data);
                        if let Some((n, bytes)) = pair
                            && n == name
                        {
                            return Ok(bytes);
                        }
                    }
                    Err(content::unavailable(Unavailable::Unsupported))
                })();
                match result {
                    Ok(bytes) => {
                        item.setData_forType(&NSData::with_bytes(&bytes), ty);
                        return;
                    }
                    Err(error) => {
                        // Fixed standard-format classes and numeric representation
                        // identity survive diagnostic redaction without logging an
                        // arbitrary private UTI or any clipboard bytes.
                        let format_class = match name.as_str() {
                            "public.utf8-plain-text"
                            | "public.utf16-plain-text"
                            | "public.utf16-external-plain-text"
                            | "NSStringPboardType" => 1_u64,
                            "public.html" | "NSHTMLPboardType" => 2,
                            "public.rtf" | "NSRTFPboardType" => 3,
                            "public.png" => 4,
                            "public.tiff" | "NSTIFFPboardType" => 5,
                            "public.jpeg" => 6,
                            "public.url" => 7,
                            _ => 0,
                        };
                        tracing::warn!(representation = u64::from(id.0), format_class, reason = ?content::reason(&error), "native clipboard representation unavailable");
                        // A valid URL with no title must not acquire a title
                        // from a different URL candidate on this same item.
                        if decoded_link {
                            return;
                        }
                    }
                }
            }
        }
    }
);
impl DataProvider {
    fn new(source: Arc<dyn Provider>, bindings: Vec<(String, RepresentationId)>, foreign: bool) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DataIvars {
            source,
            bindings,
            foreign,
        });
        // SAFETY: initialized NSObject subclass with valid Rust ivars.
        unsafe { msg_send![super(this), init] }
    }
}
fn mac_bytes(data: ClipData) -> Option<(String, Vec<u8>)> {
    Some(match data {
        ClipData::Text(text) => ("public.utf8-plain-text".into(), text.into_bytes()),
        ClipData::Html(text) => ("public.html".into(), text.into_bytes()),
        ClipData::Rtf(bytes) => ("public.rtf".into(), bytes),
        ClipData::Png(bytes) => ("public.png".into(), bytes),
        ClipData::Custom {
            platform: ClipPlatform::Mac,
            name,
            bytes,
        } => (name, bytes),
        _ => return None,
    })
}
fn foreign_names(rep: &Representation) -> Vec<String> {
    match rep.standard {
        Some(StandardFormat::Text) => vec!["public.utf8-plain-text".into()],
        Some(StandardFormat::Html) => vec!["public.html".into()],
        Some(StandardFormat::Rtf) => vec!["public.rtf".into()],
        Some(StandardFormat::Png) => vec!["public.png".into()],
        Some(StandardFormat::Privacy) => {
            let (NativeFormat::WindowsRegistered(name), RepresentationData::Inline(bytes)) = (&rep.format, &rep.data)
            else {
                return Vec::new();
            };
            crate::formats::for_platform(
                &[crate::formats::custom(ClipPlatform::Windows, name, bytes.clone())],
                ClipPlatform::Mac,
                crate::Sensitivity::Normal,
            )
            .into_iter()
            .filter_map(mac_bytes)
            .map(|(name, _)| name)
            .collect()
        }
        _ => Vec::new(),
    }
}

/// New NSPasteboardItem objects must be made for each publication. AppKit marks
/// objects from an earlier pasteboard owner stale; retaining those would not be
/// a copy snapshot. Every lazy provider here holds only Oink-owned readers.
pub(super) fn writers(
    spec: &NativeSpec,
    urls: &HashMap<NodeId, String>,
) -> Result<Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>>, ClipboardError> {
    Ok(native_items(spec, Some(urls))?
        .into_iter()
        .map(|(_, item)| ProtocolObject::from_retained(item))
        .collect())
}

/// A private placeholder marks the file's original position. The promise writer
/// replaces that key with its native promise types; an empty URL is never
/// published to the destination application.
type NativeItem = (Option<NodeId>, Retained<NSPasteboardItem>);
pub(super) fn promise_items(spec: &NativeSpec) -> Result<Vec<NativeItem>, ClipboardError> {
    native_items(spec, None)
}
fn native_items(spec: &NativeSpec, urls: Option<&HashMap<NodeId, String>>) -> Result<Vec<NativeItem>, ClipboardError> {
    let inventory = &spec.inventory;
    let wanted: HashSet<_> = inventory.wanted(ClipPlatform::Mac).map(|r| r.id).collect();
    let foreign = inventory.platform != ClipPlatform::Mac;
    let mut out = Vec::new();
    for original in &inventory.items {
        let item = NSPasteboardItem::new();
        let mut bindings = Vec::new();
        let mut file_values = Vec::new();
        let mut names = HashSet::new();
        let mut count = 0;
        for rep in original.representations.iter().filter(|r| wanted.contains(&r.id)) {
            if let RepresentationData::File(node) = rep.data {
                if foreign {
                    continue;
                }
                let url = match urls {
                    Some(urls) => urls.get(&node).ok_or(ClipboardError::Unsupported)?.as_str(),
                    None => "",
                };
                if names.insert("public.file-url".to_string()) {
                    bindings.push(("public.file-url".into(), rep.id));
                    file_values.push(url.to_owned());
                }
                continue;
            }
            let offered = if foreign {
                if rep.standard == Some(StandardFormat::Url) {
                    content::url::names(original, ClipPlatform::Mac)
                } else {
                    foreign_names(rep)
                }
            } else {
                let NativeFormat::Mac(name) = &rep.format else {
                    return Err(ClipboardError::Unsupported);
                };
                vec![name.clone()]
            };
            for name in offered {
                names.insert(name.clone());
                bindings.push((name, rep.id));
            }
        }
        if !bindings.is_empty() {
            let mut registered = HashSet::new();
            let types: Vec<_> = bindings
                .iter()
                .filter(|(name, _)| registered.insert(name))
                .map(|(name, _)| NSString::from_str(name))
                .collect();
            if foreign {
                bindings.sort_by_key(|(_, id)| content::find(inventory, *id).map_or(0, content::url::priority));
            }
            let provider = DataProvider::new(spec.contents.clone(), bindings, foreign);
            if !item.setDataProvider_forTypes(
                ProtocolObject::from_ref(&*provider),
                &NSArray::from_retained_slice(&types),
            ) {
                return Err(ClipboardError::Unsupported);
            }
            count += types.len();
        }
        // Register all types in native order before replacing file URLs with
        // local target values; otherwise every file URL jumps to the front.
        for url in file_values {
            if !item.setString_forType(&NSString::from_str(&url), &NSString::from_str("public.file-url")) {
                return Err(ClipboardError::Unsupported);
            }
        }
        if count > 0 {
            out.push((if foreign { None } else { original.file }, item));
        }
    }
    // Windows has one IDataObject containing many roots; AppKit requires a
    // separate pasteboard item per file URL. Its other formats describe that
    // same selection, not an additional item. Keep them on the first file so
    // a browser image's URL/HTML does not produce a second drag-count badge.
    // Raw same-Mac items (including genuinely separate images) stay untouched.
    if foreign {
        let mut selection = if inventory.items.len() == 1 && !inventory.roots.is_empty() {
            out.pop().map(|(_, item)| item)
        } else {
            None
        };
        for root in &inventory.roots {
            let url = match urls {
                Some(urls) => urls.get(&root.id).ok_or(ClipboardError::Unsupported)?.as_str(),
                None => "",
            };
            let item = selection.take().unwrap_or_else(NSPasteboardItem::new);
            if !item.setString_forType(&NSString::from_str(url), &NSString::from_str("public.file-url")) {
                return Err(ClipboardError::Unsupported);
            }
            out.push((Some(root.id), item));
        }
    }
    if out.is_empty() {
        return Err(ClipboardError::Unsupported);
    }
    Ok(out)
}
pub(super) fn write(
    name: Option<&str>,
    spec: &NativeSpec,
    urls: &HashMap<NodeId, String>,
    guard: &dyn Fn() -> std::io::Result<()>,
) -> Result<u64, ClipboardError> {
    autoreleasepool(|_| {
        let items = writers(spec, urls)?;
        guard().map_err(|_| ClipboardError::Busy)?;
        let pb = board(name);
        pb.clearContents();
        if !pb.writeObjects(&NSArray::from_retained_slice(&items)) {
            return Err(ClipboardError::Unsupported);
        }
        Ok(pb.changeCount() as u64)
    })
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn link_capture_and_drag_inventory_share_lazy_url_and_same_mac_bytes() {
        autoreleasepool(|_| {
            let pb = NSPasteboard::pasteboardWithUniqueName();
            let target = NSPasteboard::pasteboardWithUniqueName();
            let item = NSPasteboardItem::new();
            for (name, text) in [
                ("public.url", "https://example.invalid/"),
                ("public.url-name", "A link 猫"),
            ] {
                assert!(item.setString_forType(&NSString::from_str(text), &NSString::from_str(name)));
            }
            assert!(pb.writeObjects(&NSArray::from_retained_slice(&[ProtocolObject::from_retained(item)])));
            let dir = tempfile::tempdir().unwrap();
            let provider = capture(Some(&pb.name().to_string()), dir.path())
                .unwrap()
                .content
                .unwrap();
            let inventory = provider.inventory();
            let drag = drag_inventory(&pb, inventory.id);
            assert_eq!(drag.items, inventory.items);
            let rep = inventory
                .wanted(ClipPlatform::Windows)
                .next()
                .expect("URL-only offer projects");
            assert_eq!(rep.standard, Some(StandardFormat::Url));
            let bytes =
                content::read_all(&mut content::open_for(provider.as_ref(), rep.id, ClipPlatform::Windows).unwrap())
                    .unwrap();
            let link = content::url::Link::decode(&bytes).unwrap();
            assert_eq!(link.url, "https://example.invalid/");
            assert_eq!(link.title.as_deref(), Some("A link 猫"));
            let spec = NativeSpec {
                inventory,
                contents: provider,
                files: vec![],
                file_contents: None,
                sensitivity: crate::Sensitivity::Normal,
            };
            write(Some(&target.name().to_string()), &spec, &HashMap::new(), &|| Ok(())).unwrap();
            let item = target.pasteboardItems().unwrap().objectAtIndex(0);
            assert_eq!(
                item.types().iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                ["public.url", "public.url-name"]
            );
            assert_eq!(
                item.stringForType(&NSString::from_str("public.url-name"))
                    .unwrap()
                    .to_string(),
                "A link 猫"
            );
            assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
            pb.clearContents();
            target.clearContents();
        });
    }
    #[test]
    fn windows_file_representations_share_the_first_file_item() {
        use crate::model::{ContentSize, NodeKind};
        use content::url::tests::{Projected, source, wide_bytes};
        autoreleasepool(|_| {
            for count in [1, 2] {
                let source = source(
                    ClipPlatform::Windows,
                    &[(
                        "UniformResourceLocatorW",
                        wide_bytes("https://example.invalid/image.png"),
                    )],
                );
                let mut inventory = source.inventory();
                inventory.roots = (1..=count)
                    .map(|id| crate::model::FileNode {
                        id: NodeId(id),
                        parent: None,
                        name: format!("image-{id}.png"),
                        kind: NodeKind::File,
                        size: ContentSize::Known(42),
                        version: 1,
                        modified_ms: None,
                    })
                    .collect();
                let spec = NativeSpec {
                    inventory,
                    contents: Arc::new(Projected(source, ClipPlatform::Mac)),
                    files: vec![],
                    file_contents: None,
                    sensitivity: crate::Sensitivity::Normal,
                };
                let urls = (1..=count)
                    .map(|id| (NodeId(id), format!("file:///tmp/target/image-{id}.png")))
                    .collect();
                for urls in [None, Some(&urls)] {
                    let items = native_items(&spec, urls).unwrap();
                    assert_eq!(items.len(), count as usize, "formats must not add a drag item");
                    for (index, (node, item)) in items.iter().enumerate() {
                        assert_eq!(*node, Some(NodeId(index as u64 + 1)));
                        assert!(item.types().containsObject(&NSString::from_str("public.file-url")));
                    }
                    assert_eq!(
                        items[0]
                            .1
                            .stringForType(&NSString::from_str("public.url"))
                            .unwrap()
                            .to_string(),
                        "https://example.invalid/image.png"
                    );
                }
            }
        });
    }

    #[test]
    fn windows_links_publish_mac_url_title_and_conditional_text_for_clipboard_and_drag() {
        use content::url::tests::{Projected, source, wide_bytes};
        autoreleasepool(|_| {
            for has_title in [false, true] {
                let mut entries = vec![("UniformResourceLocatorW", wide_bytes("https://example.invalid/"))];
                if has_title {
                    entries.push(("text/x-moz-url", wide_bytes("https://example.invalid/\nA link 猫")));
                }
                let source = source(ClipPlatform::Windows, &entries);
                let spec = NativeSpec {
                    inventory: source.inventory(),
                    contents: Arc::new(Projected(source, ClipPlatform::Mac)),
                    files: vec![],
                    file_contents: None,
                    sensitivity: crate::Sensitivity::Normal,
                };
                // Both target clipboard writers and drag promise items delegate to native_items.
                for (_, item) in promise_items(&spec).unwrap() {
                    assert_eq!(
                        item.stringForType(&NSString::from_str("public.url"))
                            .unwrap()
                            .to_string(),
                        "https://example.invalid/"
                    );
                    assert_eq!(
                        item.stringForType(&NSString::from_str("public.utf8-plain-text"))
                            .unwrap()
                            .to_string(),
                        "https://example.invalid/"
                    );
                    let title = item
                        .stringForType(&NSString::from_str("public.url-name"))
                        .map(|s| s.to_string());
                    assert_eq!(title.as_deref(), has_title.then_some("A link 猫"));
                }
                let target = NSPasteboard::pasteboardWithUniqueName();
                write(Some(&target.name().to_string()), &spec, &HashMap::new(), &|| Ok(())).unwrap();
                let item = target.pasteboardItems().unwrap().objectAtIndex(0);
                assert_eq!(
                    item.dataForType(&NSString::from_str("public.url")).unwrap().to_vec(),
                    b"https://example.invalid/"
                );
                target.clearContents();
            }
        });
    }
    #[test]
    fn projected_link_never_overwrites_existing_item_text_in_either_order() {
        use content::url::tests::{Projected, source, wide_bytes};
        autoreleasepool(|_| {
            for url_first in [false, true] {
                let mut entries = vec![
                    ("UniformResourceLocatorW", wide_bytes("https://example.invalid/")),
                    ("CF_UNICODETEXT", wide_bytes("Own text")),
                ];
                if !url_first {
                    entries.reverse();
                }
                let source = source(ClipPlatform::Windows, &entries);
                let spec = NativeSpec {
                    inventory: source.inventory(),
                    contents: Arc::new(Projected(source, ClipPlatform::Mac)),
                    files: vec![],
                    file_contents: None,
                    sensitivity: crate::Sensitivity::Normal,
                };
                let items = promise_items(&spec).unwrap();
                let item = &items[0].1;
                assert_eq!(
                    item.stringForType(&NSString::from_str("public.url"))
                        .unwrap()
                        .to_string(),
                    "https://example.invalid/"
                );
                assert_eq!(
                    item.stringForType(&NSString::from_str("public.utf8-plain-text"))
                        .unwrap()
                        .to_string(),
                    "Own text"
                );
            }
        });
    }
    #[test]
    fn os_registered_bitmap_encodings_are_images_without_reading_payloads() {
        autoreleasepool(|_| {
            // The OS registry, not an Oink encoding list, determines support.
            // SAFETY: inherited NSImageRep class method, exact return type.
            let supported: Retained<NSArray<NSString>> =
                unsafe { msg_send![objc2_app_kit::NSBitmapImageRep::class(), imageUnfilteredTypes] };
            assert!(supported.iter().any(|ty| ty.to_string() == "public.jpeg"));
            for ty in supported {
                assert_eq!(
                    standard(&ty.to_string()),
                    Some(StandardFormat::Png),
                    "registered bitmap type"
                );
            }
            assert_eq!(standard("NSTIFFPboardType"), Some(StandardFormat::Png));
            assert_eq!(format!("{:?}", standard("public.url")), "Some(Url)");
            assert_eq!(standard("example.browser.private"), None);
        });
    }
    #[test]
    fn clipboard_image_preserves_plain_items_with_its_existing_promise_capability_boundary() {
        autoreleasepool(|_| {
            let source = NSPasteboard::pasteboardWithUniqueName();
            let target = NSPasteboard::pasteboardWithUniqueName();
            // A mixed browser image, deliberately with TIFF before JPEG. The
            // content-type metadata selects the original encoding; selecting
            // the first image representation instead changes the receiver's file.
            let types = [
                ("public.tiff", b"tiff representation".as_slice()),
                ("public.url", b"https://example.invalid/image.jpg"),
                ("public.url-name", b"fixture title"),
                ("public.jpeg", b"original jpeg bytes"),
                ("com.apple.pasteboard.promised-file-url", b"unfulfilled"),
                ("com.apple.pasteboard.promised-file-content-type", b"public.jpeg"),
                ("org.chromium.image-html", b"<img>"),
                ("com.apple.webarchive", b"opaque archive"),
                ("org.chromium.chromium-initiated-drag", b""),
                ("example.browser.private", &[0, 255, 1]),
            ];
            let first = NSPasteboardItem::new();
            for (name, bytes) in types {
                assert!(first.setData_forType(&NSData::with_bytes(bytes), &NSString::from_str(name)));
            }
            let second = NSPasteboardItem::new();
            second.setData_forType(&NSData::with_bytes(b"independent"), &NSString::from_str("public.png"));
            let objects = [
                ProtocolObject::from_retained(first),
                ProtocolObject::from_retained(second),
            ];
            assert!(source.writeObjects(&NSArray::from_retained_slice(&objects)));
            let dir = tempfile::tempdir().unwrap();
            let provider = capture(Some(&source.name().to_string()), dir.path())
                .unwrap()
                .content
                .unwrap();
            let inventory = provider.inventory();
            assert!(inventory.roots.is_empty());
            assert!(inventory.items.iter().all(|item| item.file.is_none()));
            let foreign: Vec<_> = inventory
                .wanted(ClipPlatform::Windows)
                .map(|rep| rep.format.clone())
                .collect();
            assert_eq!(
                foreign,
                [
                    NativeFormat::Mac("public.tiff".into()),
                    NativeFormat::Mac("public.url".into()),
                    NativeFormat::Mac("public.jpeg".into()),
                    NativeFormat::Mac("public.png".into())
                ]
            );
            let spec = NativeSpec {
                inventory,
                contents: provider,
                files: Vec::new(),
                file_contents: None,
                sensitivity: crate::Sensitivity::Normal,
            };
            write(Some(&target.name().to_string()), &spec, &HashMap::new(), &|| Ok(())).unwrap();
            let items = target.pasteboardItems().unwrap();
            assert_eq!(
                items.len(),
                2,
                "private target types: {:?}",
                items
                    .iter()
                    .map(|item| item.types().iter().map(|ty| ty.to_string()).collect::<Vec<_>>())
                    .collect::<Vec<_>>()
            );
            let target_item = items.objectAtIndex(0);
            // Clipboard capture keeps its existing capability boundary. Native
            // DnD uses drag_inventory and the separately tested promise proxy.
            let expected = [
                "public.tiff",
                "public.url",
                "public.url-name",
                "public.jpeg",
                "org.chromium.image-html",
                "com.apple.webarchive",
                "org.chromium.chromium-initiated-drag",
                "example.browser.private",
            ];
            assert_eq!(
                target_item.types().iter().map(|ty| ty.to_string()).collect::<Vec<_>>(),
                expected
            );
            for (name, bytes) in types.iter().filter(|(name, _)| expected.contains(name)) {
                assert_eq!(
                    target_item.dataForType(&NSString::from_str(name)).unwrap().to_vec(),
                    *bytes
                );
            }
            assert_eq!(
                items
                    .objectAtIndex(1)
                    .types()
                    .iter()
                    .map(|ty| ty.to_string())
                    .collect::<Vec<_>>(),
                ["public.png"]
            );
            assert!(
                target_item
                    .dataForType(&NSString::from_str("public.file-url"))
                    .is_none()
            );
            assert!(
                target_item
                    .dataForType(&NSString::from_str("com.apple.pasteboard.promised-file-url"))
                    .is_none()
            );
            source.clearContents();
            target.clearContents();
        });
    }

    fn current_fixture() -> (Retained<NSPasteboard>, tempfile::TempDir, Arc<dyn Provider>) {
        let pb = NSPasteboard::pasteboardWithUniqueName();
        pb.setData_forType(
            Some(&NSData::with_bytes(b"first")),
            &NSString::from_str("example.current"),
        );
        let dir = tempfile::tempdir().unwrap();
        let captured = capture(Some(&pb.name().to_string()), dir.path()).unwrap();
        (pb, dir, captured.content.unwrap())
    }
    #[test]
    fn clipboard_capture_never_creates_source_snapshot() {
        let (pb, dir, provider) = current_fixture();
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
        assert!(provider.local_batch().is_none());
        let mut reader = provider.open(RepresentationId(1)).unwrap();
        assert_eq!(content::read_all(&mut reader).unwrap(), b"first");
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
        pb.clearContents();
    }
    #[test]
    fn clipboard_old_offer_refuses_replacement() {
        let (pb, _dir, provider) = current_fixture();
        pb.clearContents();
        pb.setData_forType(
            Some(&NSData::with_bytes(b"second")),
            &NSString::from_str("example.current"),
        );
        let error = provider.open(RepresentationId(1)).err().expect("old offer must fail");
        assert_eq!(content::reason(&error), Unavailable::Superseded);
        pb.clearContents();
    }
    #[test]
    fn own_lazy_placeholder_invalidates_the_previous_source() {
        let (pb, _dir, provider) = current_fixture();
        let spec = NativeSpec {
            inventory: provider.inventory(),
            contents: provider.clone(),
            files: Vec::new(),
            file_contents: None,
            sensitivity: crate::Sensitivity::Normal,
        };
        write(Some(&pb.name().to_string()), &spec, &HashMap::new(), &|| Ok(())).unwrap();
        assert_eq!(
            content::reason(&provider.open(RepresentationId(1)).err().unwrap()),
            Unavailable::Superseded
        );
        pb.clearContents();
    }
    #[test]
    fn missing_copied_file_is_a_content_error_not_a_backend_failure() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("missing-file");
        let pb = NSPasteboard::pasteboardWithUniqueName();
        let item = NSPasteboardItem::new();
        let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str().unwrap()));
        item.setString_forType(&url.absoluteString().unwrap(), &NSString::from_str("public.file-url"));
        assert!(pb.writeObjects(&NSArray::from_retained_slice(&[ProtocolObject::from_retained(item)])));
        let error = capture(Some(&pb.name().to_string()), root.path()).unwrap_err();
        pb.clearContents();
        assert!(matches!(
            error,
            ClipboardError::ContentUnavailable(Unavailable::NoSuchItem)
        ));
    }

    #[test]
    fn real_file_capture_survives_a_named_pasteboard_change_without_source_copy() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("original");
        std::fs::write(&path, b"old").unwrap();
        let cache = tempfile::tempdir().unwrap();
        let pb = NSPasteboard::pasteboardWithUniqueName();
        let item = NSPasteboardItem::new();
        let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str().unwrap()));
        item.setString_forType(&url.absoluteString().unwrap(), &NSString::from_str("public.file-url"));
        assert!(pb.writeObjects(&NSArray::from_retained_slice(&[ProtocolObject::from_retained(item)])));
        let captured = capture(Some(&pb.name().to_string()), cache.path()).unwrap();
        let provider = captured.content.unwrap();
        let files = provider.namespace().unwrap();
        assert!(files.real_clipboard_files());
        assert!(
            !file_namespace(&pb, false).unwrap().unwrap().real_clipboard_files(),
            "Drop namespace must retain its original lifecycle"
        );
        let registry = content::Registry::default();
        drop(registry.register_clipboard(provider).unwrap());
        pb.clearContents();
        pb.setData_forType(
            Some(&NSData::with_bytes(b"another copy")),
            &NSString::from_str("public.utf8-plain-text"),
        );
        std::fs::write(&path, b"latest file").unwrap();
        let entry = files.select(NodeId(1)).unwrap();
        assert_eq!(entry.size, ContentSize::Known(11));
        assert_eq!(
            content::read_all(&mut files.open(entry.id, entry.version, 0).unwrap()).unwrap(),
            b"latest file"
        );
        assert!(std::fs::read_dir(cache.path()).unwrap().next().is_none());
        pb.clearContents();
    }

    #[test]
    fn clipboard_missing_lazy_data_fails_without_snapshot() {
        let pb = NSPasteboard::pasteboardWithUniqueName();
        // SAFETY: a private board with no promised owner; the declared type intentionally has no data.
        unsafe {
            pb.declareTypes_owner(
                &NSArray::from_retained_slice(&[NSString::from_str("example.unavailable")]),
                None,
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let captured = capture(Some(&pb.name().to_string()), dir.path()).unwrap();
        let provider = captured.content.unwrap();
        let error = provider
            .open(RepresentationId(1))
            .err()
            .expect("missing owner data must fail");
        assert_eq!(content::reason(&error), Unavailable::ReadFailed);
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
        pb.clearContents();
    }
    #[test]
    fn same_system_copy_retains_each_native_item_and_raw_representation_in_order() {
        autoreleasepool(|_| {
            let source = NSPasteboard::pasteboardWithUniqueName();
            let target = NSPasteboard::pasteboardWithUniqueName();
            let one = NSPasteboardItem::new();
            one.setData_forType(
                &NSData::with_bytes(&[0, 255, 0]),
                &NSString::from_str("example.private-binary"),
            );
            one.setData_forType(
                &NSData::with_bytes(&[255, 254, 65, 0]),
                &NSString::from_str("public.utf16-plain-text"),
            );
            let two = NSPasteboardItem::new();
            two.setData_forType(
                &NSData::with_bytes(b"separate item"),
                &NSString::from_str("public.utf8-plain-text"),
            );
            let writers = [ProtocolObject::from_retained(one), ProtocolObject::from_retained(two)];
            assert!(source.writeObjects(&NSArray::from_retained_slice(&writers)));
            let dir = tempfile::tempdir().unwrap();
            let captured = capture(Some(&source.name().to_string()), dir.path()).unwrap();
            let snapshot = captured.content.unwrap();
            let expected: Vec<_> = snapshot.inventory().items[0]
                .representations
                .iter()
                .map(|rep| match &rep.format {
                    NativeFormat::Mac(name) => name.clone(),
                    _ => unreachable!(),
                })
                .collect();
            // The source board remains current while the target asks for each representation.
            let spec = NativeSpec {
                inventory: snapshot.inventory(),
                contents: snapshot,
                files: Vec::new(),
                file_contents: None,
                sensitivity: crate::Sensitivity::Normal,
            };
            write(Some(&target.name().to_string()), &spec, &HashMap::new(), &|| Ok(())).unwrap();
            let items = target.pasteboardItems().unwrap();
            assert_eq!(items.len(), 2);
            let first = items.objectAtIndex(0);
            assert_eq!(
                first.types().iter().map(|name| name.to_string()).collect::<Vec<_>>(),
                expected
            );
            assert_eq!(
                first
                    .dataForType(&NSString::from_str("example.private-binary"))
                    .unwrap()
                    .to_vec(),
                [0, 255, 0]
            );
            assert_eq!(
                first
                    .dataForType(&NSString::from_str("public.utf16-plain-text"))
                    .unwrap()
                    .to_vec(),
                [255, 254, 65, 0]
            );
            assert_eq!(
                items
                    .objectAtIndex(1)
                    .dataForType(&NSString::from_str("public.utf8-plain-text"))
                    .unwrap()
                    .to_vec(),
                b"separate item"
            );
            source.clearContents();
            target.clearContents();
        });
    }
}

#[cfg(all(test, debug_assertions))]
mod audit_tests;
