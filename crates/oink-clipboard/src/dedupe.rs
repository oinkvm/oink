//! Clipboard content fingerprints for duplicate detection between clipboard
//! synchronizers. The comparison rules live in
//! `oink_foundation::clipboard_dedupe`; this module derives fingerprints from
//! native inventories and captured providers.
//!
//! First choice is the Oink clip marker: every clipboard Oink writes from a
//! received offer carries one private format ([`MARKER_MAC`], [`MARKER_WINDOWS`])
//! whose bytes are only that clip's identity. A synchronizer that copies the
//! clipboard wholesale brings the marker back; the change is then recognised
//! by identity, reading nothing but the marker. The marker is internal: it is
//! removed from every announced inventory and is never a compared format.
//! Content comparison (sending side, behind the "Clipboard deduplication"
//! setting) is the fallback when there is no recognisable marker.
//!
//! Capture reads only what the comparison needs and what is local: one
//! plain-text representation per item and one bitmap per item. Content another
//! synchronizer promised (Universal Clipboard: fetched from the other Apple
//! device on demand) is never read: it compares as unknown. HTML, RTF, URLs,
//! private formats and files are compared by metadata only. Content Oink wrote
//! from an offer has no fingerprint at all (protocol v1 offers carry no
//! digests, and reading it would fetch from the source device).
use crate::ClipData;
use crate::content::Provider;
use crate::model::{
    ContentSize, Inventory, NativeFormat, NodeKind, Representation, RepresentationData, StandardFormat,
};
use oink_foundation::clipboard::ClipPlatform;
use oink_foundation::clipboard_dedupe::{self as rules, DigestClass, Entry, Fingerprint};
use std::io::Read;

/// macOS Universal Clipboard marks content it brought from another Apple
/// device with this type (observed on the general pasteboard; also the marker
/// clipboard managers such as Maccy key on). Its payloads are promises that
/// the system fetches from the other device on demand.
pub const UNIVERSAL_CLIPBOARD: &str = "com.apple.is-remote-clipboard";
/// nspasteboard.org source-application annotation: metadata, not content.
const SOURCE_ANNOTATION: &str = "org.nspasteboard.source";
/// Oink clip marker (encoding in `oink_foundation::clipboard_marker`).
pub use oink_foundation::clipboard_marker::{MARKER_APPLE as MARKER_MAC, MARKER_BYTES, MARKER_WINDOWS};

pub fn is_marker(format: &NativeFormat) -> bool {
    match format {
        NativeFormat::Mac(name) => name == MARKER_MAC,
        NativeFormat::WindowsRegistered(name) => name.eq_ignore_ascii_case(MARKER_WINDOWS),
        _ => false,
    }
}

/// Marker bytes of a written clip; None for unannounced (capture) identities.
pub fn marker(id: crate::model::OfferId) -> Option<Vec<u8>> {
    oink_foundation::clipboard_marker::encode(id)
}

pub fn parse_marker(bytes: &[u8]) -> Option<crate::model::OfferId> {
    oink_foundation::clipboard_marker::parse(bytes)
}

/// Read only the marker representation (a few dozen bytes; for promised
/// content this is the only type transferred).
pub fn read_marker(provider: &dyn Provider) -> Option<crate::model::OfferId> {
    let inventory = provider.inventory();
    let rep = inventory
        .items
        .iter()
        .flat_map(|item| &item.representations)
        .find(|rep| is_marker(&rep.format))?;
    parse_marker(&read_limited(provider, rep, 4096)?)
}

/// The provider without marker representations (and without items left with
/// nothing else): markers never leave this device.
pub fn without_marker(provider: std::sync::Arc<dyn Provider>) -> std::io::Result<std::sync::Arc<dyn Provider>> {
    let mut inventory = provider.inventory();
    if !inventory
        .items
        .iter()
        .flat_map(|item| &item.representations)
        .any(|rep| is_marker(&rep.format))
    {
        return Ok(provider);
    }
    for item in &mut inventory.items {
        item.representations.retain(|rep| !is_marker(&rep.format));
    }
    inventory
        .items
        .retain(|item| !item.representations.is_empty() || item.file.is_some());
    crate::content::projected(provider, inventory)
}

/// The current clipboard was written by another synchronizer.
pub fn external(inventory: &Inventory) -> bool {
    inventory
        .items
        .iter()
        .flat_map(|item| &item.representations)
        .any(|rep| matches!(&rep.format, NativeFormat::Mac(name) if name == UNIVERSAL_CLIPBOARD))
}

fn class(rep: &Representation) -> Option<DigestClass> {
    if matches!(
        rep.data,
        RepresentationData::File(_) | RepresentationData::Unavailable(_)
    ) {
        return None;
    }
    Some(match rep.standard {
        Some(StandardFormat::Text) => DigestClass::Text,
        Some(StandardFormat::Html) => DigestClass::Html,
        Some(StandardFormat::Rtf) => DigestClass::Rtf,
        Some(StandardFormat::Png) => DigestClass::Image,
        Some(StandardFormat::Url) => DigestClass::Url,
        Some(StandardFormat::Privacy) => return None,
        None => {
            let metadata = is_marker(&rep.format)
                || match &rep.format {
                    NativeFormat::Mac(name) => {
                        name == UNIVERSAL_CLIPBOARD
                            || name == SOURCE_ANNOTATION
                            || crate::formats::is_marker(ClipPlatform::Mac, name)
                    }
                    NativeFormat::WindowsRegistered(name) => crate::formats::is_marker(ClipPlatform::Windows, name),
                    _ => false,
                };
            if metadata {
                return None;
            }
            DigestClass::Other
        }
    })
}

fn format_name(format: &NativeFormat) -> String {
    match format {
        NativeFormat::Mac(name) => format!("mac:{name}"),
        NativeFormat::WindowsRegistered(name) => format!("win:{}", name.to_ascii_lowercase()),
        NativeFormat::WindowsPredefined(cf) => format!("win-cf:{cf}"),
        NativeFormat::Unknown(name) => format!("unknown:{name}"),
    }
}

fn native_size(rep: &Representation) -> Option<u64> {
    match &rep.data {
        RepresentationData::Reader {
            size: ContentSize::Known(n),
        } => Some(*n),
        RepresentationData::Inline(bytes) => Some(bytes.len() as u64),
        _ => None,
    }
}

/// Metadata-only fingerprint: one entry per (item, class), opaque formats per
/// native name, and one entry per file root. Reps of file items describe the
/// file itself (icons, names); the root metadata stands for them.
pub fn from_inventory(inventory: &Inventory) -> Fingerprint {
    let mut entries: Vec<Entry> = Vec::new();
    for item in inventory.items.iter().filter(|item| item.file.is_none()) {
        for rep in &item.representations {
            let Some(class) = class(rep) else { continue };
            let name = (class == DigestClass::Other).then(|| format_name(&rep.format));
            if entries
                .iter()
                .any(|e| e.item == item.id.0 && e.class == class && e.name == name)
            {
                continue;
            }
            let mut entry = Entry::new(item.id.0, class);
            entry.name = name;
            // Text is compared by normalized digest; its native size is a
            // different encoding of the same text and would only mislead.
            if class != DigestClass::Text {
                entry.size = native_size(rep);
            }
            entries.push(entry);
        }
    }
    for root in &inventory.roots {
        let mut entry = Entry::new(0, DigestClass::File);
        entry.name = Some(root.name.clone());
        entry.directory = root.kind == NodeKind::Directory;
        entry.size = match (&root.kind, root.size) {
            (NodeKind::Directory, _) => None,
            (_, ContentSize::Known(n)) => Some(n),
            _ => None,
        };
        entry.modified_ms = root.modified_ms;
        entries.push(entry);
    }
    Fingerprint { entries }
}

fn preferred<'a>(reps: impl Iterator<Item = &'a Representation>, names: &[&str]) -> Option<&'a Representation> {
    let reps: Vec<_> = reps.collect();
    reps.iter()
        .find(|rep| match &rep.format {
            NativeFormat::Mac(name) | NativeFormat::WindowsRegistered(name) => {
                names.iter().any(|n| n.eq_ignore_ascii_case(name))
            }
            NativeFormat::WindowsPredefined(13) => names.contains(&"CF_UNICODETEXT"),
            _ => false,
        })
        .or_else(|| reps.first())
        .copied()
}

fn read_limited(provider: &dyn Provider, rep: &Representation, limit: usize) -> Option<Vec<u8>> {
    let mut reader = provider.open(rep.id).ok()?;
    let mut bytes = Vec::new();
    (&mut reader).take(limit as u64 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= limit).then_some(bytes)
}

fn standard(provider: &dyn Provider, rep: &Representation, bytes: &[u8]) -> Vec<ClipData> {
    provider
        .translator()
        .standard_with_source(rep, bytes, provider)
        .unwrap_or_default()
}

/// Pixel digest (None above the pixel threshold) and dimensions.
pub type PngDigest = (Option<[u8; 32]>, (u32, u32));

/// Pixel digest and dimensions of a PNG. Dimensions alone above the pixel
/// threshold (or for interlaced images, which do not decode row by row).
pub fn png_digest(png: &[u8]) -> Option<PngDigest> {
    let mut decoder = png::Decoder::new_with_limits(
        std::io::Cursor::new(png),
        png::Limits {
            bytes: rules::IMAGE_DIGEST_BYTES,
        },
    );
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let (width, height, interlaced) = {
        let info = reader.info();
        (info.width, info.height, info.interlaced)
    };
    if interlaced || u64::from(width) * u64::from(height) > rules::IMAGE_DIGEST_PIXELS {
        return Some((None, (width, height)));
    }
    let (color, depth) = reader.output_color_type();
    if depth != png::BitDepth::Eight {
        return Some((None, (width, height)));
    }
    let mut digest = rules::PixelDigest::new(width, height);
    let mut rgba = Vec::with_capacity(width as usize * 4);
    while let Some(row) = reader.next_row().ok()? {
        let data = row.data();
        rgba.clear();
        match color {
            png::ColorType::Rgba => rgba.extend_from_slice(data),
            png::ColorType::Rgb => data
                .as_chunks::<3>()
                .0
                .iter()
                .for_each(|p| rgba.extend_from_slice(&[p[0], p[1], p[2], 255])),
            png::ColorType::Grayscale => data.iter().for_each(|g| rgba.extend_from_slice(&[*g, *g, *g, 255])),
            png::ColorType::GrayscaleAlpha => data
                .as_chunks::<2>()
                .0
                .iter()
                .for_each(|p| rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]])),
            png::ColorType::Indexed => return Some((None, (width, height))),
        }
        digest.rgba_row(&rgba);
    }
    Some((Some(digest.finish()), (width, height)))
}

/// Fingerprint of a captured provider. Reads one text and one bitmap
/// representation per item; `external` (promised) content is not read at all.
/// Read failures leave the entry without a digest, which is never a subset.
pub fn fingerprint(provider: &dyn Provider, external: bool) -> Fingerprint {
    let inventory = provider.inventory();
    let mut fingerprint = from_inventory(&inventory);
    if external {
        return fingerprint;
    }
    for entry in &mut fingerprint.entries {
        let Some(item) = inventory
            .items
            .iter()
            .find(|item| item.id.0 == entry.item && item.file.is_none())
        else {
            continue;
        };
        let reps = || {
            item.representations
                .iter()
                .filter(|rep| class(rep) == Some(entry.class))
        };
        match entry.class {
            DigestClass::Text => {
                let Some(rep) = preferred(reps(), &["public.utf8-plain-text", "CF_UNICODETEXT"]) else {
                    continue;
                };
                let Some(bytes) = read_limited(provider, rep, rules::TEXT_DIGEST_BYTES) else {
                    continue;
                };
                if let Some(ClipData::Text(text)) = standard(provider, rep, &bytes).into_iter().next() {
                    let (digest, size) = rules::text_digest(&text);
                    entry.digest = Some(digest);
                    entry.size = Some(size);
                }
            }
            DigestClass::Image => {
                let Some(rep) = preferred(reps(), &["public.png", "PNG"]) else {
                    continue;
                };
                let Some(bytes) = read_limited(provider, rep, rules::IMAGE_DIGEST_BYTES) else {
                    continue;
                };
                let png = match standard(provider, rep, &bytes).into_iter().next() {
                    Some(ClipData::Png(png)) => png,
                    _ => continue,
                };
                if let Some((digest, dimensions)) = png_digest(&png) {
                    entry.digest = digest;
                    entry.dimensions = Some(dimensions);
                }
            }
            _ => {}
        }
    }
    fingerprint
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::content::{Translator, tests::fixture};
    use crate::model::*;
    use oink_foundation::clipboard_dedupe::{Identity, Source, echo};
    use std::{collections::HashMap, io, sync::Arc};

    /// Native bytes per representation; text is UTF-8, images are PNG or BMP.
    struct Board {
        inventory: Inventory,
        bytes: HashMap<RepresentationId, Vec<u8>>,
        /// Every representation opened, in order.
        reads: std::sync::Mutex<Vec<RepresentationId>>,
    }
    struct Decode;
    impl Translator for Decode {
        fn standard(&self, rep: &Representation, bytes: &[u8]) -> io::Result<Vec<ClipData>> {
            Ok(match rep.standard {
                Some(StandardFormat::Text) => vec![ClipData::Text(String::from_utf8(bytes.to_vec()).unwrap())],
                Some(StandardFormat::Png) if bytes.starts_with(b"BM") => {
                    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Bmp)
                        .map_err(io::Error::other)?
                        .to_rgba8();
                    vec![ClipData::Png(encode(image.width(), image.height(), image.as_raw()))]
                }
                Some(StandardFormat::Png) => vec![ClipData::Png(bytes.to_vec())],
                _ => vec![],
            })
        }
    }
    impl Provider for Board {
        fn inventory(&self) -> Inventory {
            self.inventory.clone()
        }
        fn open(&self, id: RepresentationId) -> io::Result<crate::FileReader> {
            self.reads.lock().unwrap().push(id);
            Ok(Box::new(io::Cursor::new(self.bytes[&id].clone())))
        }
        fn translator(&self) -> Arc<dyn Translator> {
            Arc::new(Decode)
        }
    }
    fn encode(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().unwrap().write_image_data(rgba).unwrap();
        out
    }
    fn board(reps: Vec<(&str, Option<StandardFormat>, Vec<u8>)>) -> Board {
        let mut inventory = fixture().inventory();
        inventory.platform = ClipPlatform::Mac;
        inventory.roots.clear();
        let mut bytes = HashMap::new();
        inventory.items = vec![ContentItem {
            id: ItemId(0),
            file: None,
            virtual_files: false,
            representations: reps
                .into_iter()
                .enumerate()
                .map(|(index, (name, standard, data))| {
                    let id = RepresentationId(index as u32 + 1);
                    bytes.insert(id, data);
                    Representation {
                        id,
                        format: NativeFormat::Mac(name.into()),
                        medium: Medium::Bytes,
                        aspect: 1,
                        index: -1,
                        standard,
                        data: RepresentationData::Reader {
                            size: ContentSize::Unknown,
                        },
                    }
                })
                .collect(),
        }];
        Board {
            inventory,
            bytes,
            reads: Default::default(),
        }
    }
    fn rgba() -> Vec<u8> {
        (0..4 * 3 * 2).map(|n| (n * 11) as u8).collect()
    }

    #[test]
    fn universal_clipboard_marker_means_external_and_is_not_content() {
        let local = board(vec![(
            "public.utf8-plain-text",
            Some(StandardFormat::Text),
            b"hi".to_vec(),
        )]);
        let remote = board(vec![
            ("public.utf8-plain-text", Some(StandardFormat::Text), b"hi\r\n".to_vec()),
            (UNIVERSAL_CLIPBOARD, None, vec![]),
            (SOURCE_ANNOTATION, None, b"com.apple.UIKit".to_vec()),
        ]);
        assert!(!external(&local.inventory));
        assert!(external(&remote.inventory));
        let current = Identity {
            fingerprint: fingerprint(&local, false),
            source: Source::Device(oink_foundation::DeviceId([1; 32])),
        };
        let new = fingerprint(&remote, true);
        assert_eq!(new.entries.len(), 1, "markers are metadata, not formats");
        // Promised data is never read: unknown, so never a duplicate.
        assert_eq!(new.entries[0].digest, None);
        assert_eq!(
            echo(&new, &current),
            Err(oink_foundation::clipboard_dedupe::Fresh::Unknown(DigestClass::Text))
        );
    }

    #[test]
    fn oink_marker_round_trips_and_never_leaves_the_device() {
        let id = OfferId {
            clip: oink_foundation::content::ClipId {
                origin: oink_foundation::DeviceId([7; 32]),
                seq: 42,
            },
            generation: 43,
        };
        let bytes = marker(id).unwrap();
        assert_eq!(bytes.len(), MARKER_BYTES);
        assert_eq!(parse_marker(&bytes), Some(id));
        let mut padded = bytes.clone();
        padded.extend_from_slice(&[0; 12]);
        assert_eq!(parse_marker(&padded), Some(id), "HGLOBAL padding");
        assert_eq!(parse_marker(&bytes[..MARKER_BYTES - 1]), None);
        let mut wrong = bytes.clone();
        wrong[0] = b'X';
        assert_eq!(parse_marker(&wrong), None);
        assert_eq!(
            marker(OfferId {
                clip: oink_foundation::content::ClipId {
                    origin: oink_foundation::DeviceId([0; 32]),
                    seq: 1
                },
                generation: 1
            }),
            None
        );

        let provider = Arc::new(board(vec![
            ("public.utf8-plain-text", Some(StandardFormat::Text), b"t".to_vec()),
            (MARKER_MAC, None, bytes.clone()),
        ]));
        assert_eq!(read_marker(provider.as_ref()), Some(id));
        assert_eq!(
            from_inventory(&provider.inventory).entries.len(),
            1,
            "the marker is not a format"
        );
        let stripped = without_marker(provider).unwrap();
        assert!(
            stripped
                .inventory()
                .items
                .iter()
                .flat_map(|item| &item.representations)
                .all(|rep| !is_marker(&rep.format))
        );
        assert_eq!(read_marker(stripped.as_ref()), None);
        assert!(is_marker(&NativeFormat::WindowsRegistered("oink clip".into())));
    }

    /// Universal Clipboard brings another Mac's Oink write back to this Mac
    /// next to promised content (oinkvm/oink#35). Only the marker is read and
    /// it names the clip; the promised content is never opened.
    #[test]
    fn universal_clipboard_write_back_is_recognised_by_marker_reading_only_the_marker() {
        let id = OfferId {
            clip: oink_foundation::content::ClipId {
                origin: oink_foundation::DeviceId([3; 32]),
                seq: 9,
            },
            generation: 9,
        };
        let provider = Arc::new(board(vec![
            ("public.png", Some(StandardFormat::Png), encode(3, 2, &rgba())),
            (UNIVERSAL_CLIPBOARD, None, Vec::new()),
            (MARKER_MAC, None, marker(id).unwrap()),
        ]));
        assert!(external(&provider.inventory));
        assert_eq!(read_marker(provider.as_ref()), Some(id));
        assert_eq!(
            *provider.reads.lock().unwrap(),
            [RepresentationId(3)],
            "only the marker"
        );
        let new = fingerprint(provider.as_ref(), true);
        assert_eq!(new.entries.len(), 1);
        assert_eq!((new.entries[0].digest, new.entries[0].dimensions), (None, None));
        assert_eq!(provider.reads.lock().unwrap().len(), 1, "promised content is not read");
    }

    /// A previous local copy compared with a synchronizer's rewrite of it.
    #[test]
    fn captured_local_content_compares_by_format_subset() {
        let copied = board(vec![
            ("public.utf8-plain-text", Some(StandardFormat::Text), b"a\nb\n".to_vec()),
            ("public.html", Some(StandardFormat::Html), b"<b>a</b>".to_vec()),
            ("com.example.private", None, b"app".to_vec()),
        ]);
        let previous = Identity {
            fingerprint: fingerprint(&copied, false),
            source: Source::Device(oink_foundation::DeviceId([1; 32])),
        };
        assert_eq!(
            copied.reads.lock().unwrap().len(),
            1,
            "one text read; HTML/private by metadata"
        );
        // Plain text only, CRLF, plus a synchronizer's own tag: a subset.
        let rewritten = board(vec![
            ("public.utf8-plain-text", Some(StandardFormat::Text), b"a\r\nb".to_vec()),
            ("com.vendor.sync-tag", None, b"x".to_vec()),
        ]);
        assert_eq!(echo(&fingerprint(&rewritten, false), &previous), Ok(()));
        let prefix = board(vec![(
            "public.utf8-plain-text",
            Some(StandardFormat::Text),
            b"a".to_vec(),
        )]);
        assert_eq!(
            echo(&fingerprint(&prefix, false), &previous),
            Err(oink_foundation::clipboard_dedupe::Fresh::Differs(DigestClass::Text))
        );
        let richer = board(vec![
            ("public.utf8-plain-text", Some(StandardFormat::Text), b"a\nb".to_vec()),
            ("public.rtf", Some(StandardFormat::Rtf), b"{\\rtf1 a}".to_vec()),
        ]);
        assert_eq!(
            echo(&fingerprint(&richer, false), &previous),
            Err(oink_foundation::clipboard_dedupe::Fresh::Missing(DigestClass::Rtf))
        );
    }

    #[test]
    fn reencoded_bitmaps_have_the_same_pixel_digest() {
        let png = board(vec![("public.png", Some(StandardFormat::Png), encode(3, 2, &rgba()))]);
        let mut bmp = Vec::new();
        image::RgbaImage::from_raw(3, 2, rgba())
            .unwrap()
            .write_to(&mut io::Cursor::new(&mut bmp), image::ImageFormat::Bmp)
            .unwrap();
        let tiffish = board(vec![("public.tiff", Some(StandardFormat::Png), bmp)]);
        let a = fingerprint(&png, false);
        let b = fingerprint(&tiffish, false);
        assert_eq!(a.entries[0].dimensions, Some((3, 2)));
        assert!(a.entries[0].digest.is_some());
        assert_eq!(a.entries[0].digest, b.entries[0].digest);
        // A promised (external) image is never read.
        assert_eq!(fingerprint(&png, true).entries[0].digest, None);
        let mut other = rgba();
        other[0] ^= 1;
        let changed = board(vec![("public.png", Some(StandardFormat::Png), encode(3, 2, &other))]);
        assert_ne!(fingerprint(&changed, false).entries[0].digest, a.entries[0].digest);
    }

    /// Real AppKit capture on a private named pasteboard: the Universal
    /// Clipboard marker type is recognised, and its text is not read.
    #[cfg(target_os = "macos")]
    #[test]
    fn native_pasteboard_universal_clipboard_marker_is_recognised() {
        use objc2::{rc::autoreleasepool, runtime::ProtocolObject};
        use objc2_app_kit::{NSPasteboard, NSPasteboardItem};
        use objc2_foundation::{NSArray, NSData, NSString};
        let name = format!("com.hachiwii.oink.test.dedupe.{}", std::process::id());
        let write = |marker: bool, text: &str| {
            autoreleasepool(|_| {
                let pb = NSPasteboard::pasteboardWithName(&NSString::from_str(&name));
                pb.clearContents();
                let item = NSPasteboardItem::new();
                item.setString_forType(&NSString::from_str(text), &NSString::from_str("public.utf8-plain-text"));
                if marker {
                    item.setData_forType(&NSData::new(), &NSString::from_str(UNIVERSAL_CLIPBOARD));
                }
                assert!(pb.writeObjects(&NSArray::from_retained_slice(&[ProtocolObject::from_retained(item)])));
            })
        };
        let backend = crate::named_pasteboard_backend(&name).unwrap();
        let data = tempfile::tempdir().unwrap();
        write(false, "copied on this Mac\n");
        let local = backend.capture(data.path()).unwrap().content.unwrap();
        assert!(!external(&local.inventory()));
        let local = fingerprint(local.as_ref(), false);
        write(true, "copied on this Mac\r\n");
        let remote = backend.capture(data.path()).unwrap().content.unwrap();
        assert!(external(&remote.inventory()));
        let remote = fingerprint(remote.as_ref(), true);
        assert!(remote.entries.iter().all(|e| e.class == DigestClass::Text));
        let current = Identity {
            fingerprint: local,
            source: Source::Device(oink_foundation::DeviceId([1; 32])),
        };
        assert!(
            remote.entries.iter().all(|e| e.digest.is_none()),
            "promised data is not read"
        );
        assert!(echo(&remote, &current).is_err());
        autoreleasepool(|_| {
            NSPasteboard::pasteboardWithName(&NSString::from_str(&name)).clearContents();
        });
    }
}
