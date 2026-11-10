//! Capture-worker side of `clipboard_dedupe`: what is read, and what leaves.
use super::*;
use oink_clipboard::model::{
    ContentItem, ContentSize, ItemId, Medium, NativeFormat, OfferId, Representation, RepresentationData,
    RepresentationId, StandardFormat,
};
use oink_foundation::clipboard::{ClipPlatform, ClipSnapshot, Sensitivity};
use oink_foundation::content::ClipId as FoundationClipId;
use std::io;

/// A Mac clipboard of named byte representations that records every read.
struct Board {
    inventory: Inventory,
    bytes: Vec<Vec<u8>>,
    reads: Mutex<Vec<String>>,
}
struct Utf8;
impl content::Translator for Utf8 {
    fn standard(&self, rep: &Representation, bytes: &[u8]) -> io::Result<Vec<oink_clipboard::ClipData>> {
        Ok(match rep.standard {
            Some(StandardFormat::Text) => vec![oink_clipboard::ClipData::Text(
                String::from_utf8(bytes.to_vec()).map_err(io::Error::other)?,
            )],
            _ => vec![],
        })
    }
}
impl Provider for Board {
    fn inventory(&self) -> Inventory {
        self.inventory.clone()
    }
    fn open(&self, id: RepresentationId) -> io::Result<oink_clipboard::FileReader> {
        let rep = self
            .inventory
            .items
            .iter()
            .flat_map(|item| &item.representations)
            .find(|rep| rep.id == id)
            .unwrap();
        let NativeFormat::Mac(name) = &rep.format else {
            unreachable!()
        };
        self.reads.lock().unwrap().push(name.clone());
        Ok(Box::new(io::Cursor::new(self.bytes[id.0 as usize - 1].clone())))
    }
    fn translator(&self) -> Arc<dyn content::Translator> {
        Arc::new(Utf8)
    }
}
fn board(reps: &[(&str, Option<StandardFormat>, &[u8])]) -> Arc<Board> {
    let representations = reps
        .iter()
        .enumerate()
        .map(|(index, (name, standard, _))| Representation {
            id: RepresentationId(index as u32 + 1),
            format: NativeFormat::Mac((*name).into()),
            medium: Medium::Bytes,
            aspect: 1,
            index: -1,
            standard: *standard,
            data: RepresentationData::Reader {
                size: ContentSize::Unknown,
            },
        })
        .collect();
    Arc::new(Board {
        inventory: Inventory {
            id: OfferId {
                clip: FoundationClipId {
                    origin: DeviceId([0; 32]),
                    seq: 1,
                },
                generation: 1,
            },
            platform: ClipPlatform::Mac,
            items: vec![ContentItem {
                id: ItemId(0),
                file: None,
                virtual_files: false,
                representations,
            }],
            roots: vec![],
        },
        bytes: reps.iter().map(|(_, _, bytes)| bytes.to_vec()).collect(),
        reads: Mutex::default(),
    })
}
fn captured(board: Arc<Board>) -> Result<oink_clipboard::CapturedClipboard, oink_clipboard::ClipboardError> {
    Ok(oink_clipboard::CapturedClipboard {
        snapshot: ClipSnapshot {
            items: vec![],
            sensitivity: Sensitivity::Normal,
            change_count: 1,
        },
        content: Some(board),
    })
}
fn clip(origin: u8) -> OfferId {
    OfferId {
        clip: FoundationClipId {
            origin: DeviceId([origin; 32]),
            seq: 7,
        },
        generation: 7,
    }
}
const TEXT: &str = "public.utf8-plain-text";
fn formats(captured: &oink_clipboard::CapturedClipboard) -> Vec<NativeFormat> {
    captured
        .content
        .as_ref()
        .unwrap()
        .inventory()
        .items
        .iter()
        .flat_map(|item| &item.representations)
        .map(|rep| rep.format.clone())
        .collect()
}

#[test]
fn known_marker_reads_only_the_marker_and_never_leaves_the_device() {
    let marker = oink_clipboard::dedupe::marker(clip(2)).unwrap();
    let source = board(&[
        (TEXT, Some(StandardFormat::Text), b"rewritten"),
        (oink_clipboard::dedupe::MARKER_MAC, None, &marker),
    ]);
    let mut result = captured(source.clone());
    let identity = captured_identity(&mut result, &[clip(2).clip], true).unwrap();
    assert_eq!(identity.marker, Some(clip(2).clip));
    assert!(identity.fingerprint.is_none(), "a known clip is not compared");
    assert_eq!(*source.reads.lock().unwrap(), [oink_clipboard::dedupe::MARKER_MAC]);
    assert_eq!(
        formats(result.as_ref().unwrap()),
        [NativeFormat::Mac(TEXT.into())],
        "the marker is not in the announced inventory"
    );
}

#[test]
fn unknown_marker_is_no_marker_and_content_is_fingerprinted() {
    let marker = oink_clipboard::dedupe::marker(clip(3)).unwrap();
    let source = board(&[
        (TEXT, Some(StandardFormat::Text), b"fresh\r\n"),
        (oink_clipboard::dedupe::MARKER_MAC, None, &marker),
    ]);
    let mut result = captured(source.clone());
    let identity = captured_identity(&mut result, &[clip(2).clip], true).unwrap();
    assert_eq!(identity.marker, None);
    let fingerprint = identity.fingerprint.unwrap();
    assert_eq!(
        fingerprint.entries[0].digest,
        Some(oink_foundation::clipboard_dedupe::text_digest("fresh").0)
    );
    assert_eq!(formats(result.as_ref().unwrap()), [NativeFormat::Mac(TEXT.into())]);
}

#[test]
fn deduplication_off_reads_no_content() {
    let source = board(&[(TEXT, Some(StandardFormat::Text), b"not read")]);
    let mut result = captured(source.clone());
    let identity = captured_identity(&mut result, &[], false).unwrap();
    assert!(identity.fingerprint.is_none());
    assert!(source.reads.lock().unwrap().is_empty());
}

#[test]
fn universal_clipboard_content_is_never_read() {
    let source = board(&[
        (TEXT, Some(StandardFormat::Text), b"promised by another Mac"),
        (oink_clipboard::dedupe::UNIVERSAL_CLIPBOARD, None, b""),
    ]);
    let mut result = captured(source.clone());
    let identity = captured_identity(&mut result, &[], true).unwrap();
    assert!(identity.external);
    let fingerprint = identity.fingerprint.unwrap();
    assert!(fingerprint.entries.iter().all(|entry| entry.digest.is_none()));
    assert!(source.reads.lock().unwrap().is_empty(), "promised data is not read");
}
