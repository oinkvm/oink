//! Every enumerated IDataObject representation retains its format, medium,
//! aspect, index and source order. Clipboard bytes are read on demand on a separate STA.
use crate::content::{self, Provider, Snapshot, Translator};
use crate::model::*;
use crate::{
    CapturedClipboard, ClipData, ClipSnapshot, ClipboardError, NativeSpec,
    store::{Batch, BatchId, StoreKind},
};
use oink_foundation::content::ClipId;
use oink_foundation::{DeviceId, clipboard::ClipPlatform};
use std::{io, path::Path, sync::Arc};
use windows::{
    Win32::{
        Foundation::{DV_E_FORMATETC, DV_E_TYMED, STG_E_READFAULT},
        System::{
            Com::*,
            DataExchange::{GetClipboardFormatNameW, METAFILEPICT, RegisterClipboardFormatW},
            Ole::{OleGetClipboard, ReleaseStgMedium},
        },
    },
    core::PCWSTR,
};
pub(crate) mod current;
pub(super) mod diagnostics;
mod medium;
mod shortcut;
pub(super) mod target;
pub(super) mod virtual_files;

pub(super) fn name(cf: u16) -> Option<NativeFormat> {
    if (1..=17).contains(&cf) {
        return Some(NativeFormat::WindowsPredefined(cf));
    }
    if cf < 0xc000 {
        return None;
    }
    let mut text = [0u16; 4096];
    // SAFETY: writable name buffer; registered ID came from FORMATETC.
    let n = unsafe { GetClipboardFormatNameW(cf as u32, &mut text) };
    (n > 0).then(|| NativeFormat::WindowsRegistered(String::from_utf16_lossy(&text[..n as usize])))
}
pub(super) fn format_id(format: &NativeFormat) -> Option<u16> {
    match format {
        NativeFormat::WindowsPredefined(cf) => Some(*cf),
        NativeFormat::WindowsRegistered(name) => {
            let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            // SAFETY: bounded, validated, NUL terminated registered name.
            let id = unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) };
            (id != 0).then_some(id as u16)
        }
        _ => None,
    }
}
fn png_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("PNG")
        || name.split(';').next().is_some_and(|mime| {
            mime.trim().eq_ignore_ascii_case("image/png") || mime.trim().eq_ignore_ascii_case("image/x-png")
        })
}
fn standard(format: &NativeFormat) -> Option<StandardFormat> {
    if matches!(format, NativeFormat::WindowsRegistered(name) if ["UniformResourceLocatorW", "UniformResourceLocator", "text/x-moz-url"].iter().any(|n| name.eq_ignore_ascii_case(n)))
    {
        return Some(StandardFormat::Url);
    }
    match format {
        NativeFormat::WindowsPredefined(1 | 7 | 13) => Some(StandardFormat::Text),
        NativeFormat::WindowsPredefined(2 | 6 | 8 | 17) => Some(StandardFormat::Png),
        NativeFormat::WindowsRegistered(name) => match name.as_str() {
            n if n.eq_ignore_ascii_case("HTML Format") => Some(StandardFormat::Html),
            n if n.eq_ignore_ascii_case("Rich Text Format") => Some(StandardFormat::Rtf),
            n if png_name(n) => Some(StandardFormat::Png),
            n if crate::formats::is_marker(ClipPlatform::Windows, n) => Some(StandardFormat::Privacy),
            _ => None,
        },
        _ => None,
    }
}
fn is_file(format: &NativeFormat) -> bool {
    matches!(format, NativeFormat::WindowsPredefined(15))
        || matches!(format, NativeFormat::WindowsRegistered(n) if ["FileGroupDescriptorW", "FileGroupDescriptor", "FileContents", "Shell IDList Array", "FileNameW", "FileName"].iter().any(|name|n.eq_ignore_ascii_case(name)))
}

pub(super) fn inventory(data: &IDataObject, id: OfferId) -> io::Result<Inventory> {
    // SAFETY: current apartment owns data. Next allocates ptd; we free each one.
    // Device-specific render targets are explicitly unavailable, not replaced by
    // a guessed display/printer: those device contexts cannot cross machines.
    unsafe {
        let formats = data
            .EnumFormatEtc(DATADIR_GET.0 as u32)
            .map_err(|_| content::unavailable(Unavailable::ReadFailed))?;
        let mut representations = Vec::new();
        let mut enumerated = 0;
        loop {
            let mut entries = [FORMATETC::default()];
            let mut n = 0;
            formats
                .Next(&mut entries, Some(&mut n))
                .ok()
                .map_err(|_| content::unavailable(Unavailable::ReadFailed))?;
            if n == 0 {
                break;
            }
            let entry = entries[0];
            let device = !entry.ptd.is_null();
            if device {
                CoTaskMemFree(Some(entry.ptd.cast()));
            }
            enumerated += 1;
            if enumerated > MAX_REPRESENTATIONS {
                return Err(content::unavailable(Unavailable::LimitExceeded));
            }
            let Some(format) = name(entry.cfFormat) else {
                continue;
            };
            for bit in [
                TYMED_HGLOBAL,
                TYMED_FILE,
                TYMED_ISTREAM,
                TYMED_ISTORAGE,
                TYMED_GDI,
                TYMED_MFPICT,
                TYMED_ENHMF,
            ] {
                if entry.tymed & bit.0 as u32 == 0 {
                    continue;
                }
                let Some(medium) = medium::kind(bit.0 as u32, entry.cfFormat) else {
                    continue;
                };
                if representations.len() >= MAX_REPRESENTATIONS {
                    return Err(content::unavailable(Unavailable::LimitExceeded));
                }
                let data = if device {
                    RepresentationData::Unavailable(Unavailable::Unsupported)
                } else if is_file(&format) {
                    RepresentationData::File(NodeId(1))
                } else {
                    RepresentationData::Reader {
                        size: ContentSize::Unknown,
                    }
                };
                representations.push(Representation {
                    id: RepresentationId(representations.len() as u32 + 1),
                    standard: standard(&format),
                    format: format.clone(),
                    medium,
                    aspect: entry.dwAspect,
                    index: entry.lindex,
                    data,
                });
            }
        }
        let file = representations
            .iter()
            .any(|r| matches!(r.data, RepresentationData::File(_)))
            .then_some(NodeId(1));
        let virtual_files = file.is_some()
            && !representations
                .iter()
                .any(|rep| rep.format == NativeFormat::WindowsPredefined(15));
        let inventory = Inventory {
            roots: Vec::new(),
            id,
            platform: ClipPlatform::Windows,
            items: vec![ContentItem {
                virtual_files,
                id: ItemId(0),
                file,
                representations,
            }],
        };
        inventory.validate().map_err(content::unavailable)?;
        Ok(inventory)
    }
}
pub(super) fn read_representation(data: &IDataObject, rep: &Representation) -> io::Result<Vec<u8>> {
    let cf = format_id(&rep.format).ok_or_else(|| content::unavailable(Unavailable::InvalidData))?;
    let fe = FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: rep.aspect,
        lindex: rep.index,
        tymed: medium::tymed(rep.medium),
    };
    // SAFETY: valid FORMATETC; medium is consumed in this apartment and always
    // released after serialization, including failures and budget rejection.
    unsafe {
        data.GetData(&fe)
            .map_err(|_| content::unavailable(Unavailable::ReadFailed))
            .and_then(|mut value| {
                let result = if value.tymed == fe.tymed {
                    medium::serialize(&value, rep.medium)
                } else {
                    Err(content::unavailable(Unavailable::InvalidData))
                };
                ReleaseStgMedium(&mut value);
                result
            })
    }
}
pub(super) fn snapshot(data: &IDataObject, root: &Path, id: OfferId) -> io::Result<Arc<dyn Provider>> {
    let mut inventory = inventory(data, id)?;
    let batch = Batch::create(root, StoreKind::SourceSnapshot, BatchId::random()?)?;
    for rep in inventory.items.iter_mut().flat_map(|item| &mut item.representations) {
        if !matches!(rep.data, RepresentationData::Reader { .. }) {
            continue;
        }
        let result = read_representation(data, rep);
        rep.data = match result.and_then(|bytes| {
            batch
                .snapshot(rep.id.0, &mut io::Cursor::new(&bytes))
                .map(|size| (bytes, size))
        }) {
            Ok((bytes, size)) if rep.standard == Some(StandardFormat::Privacy) && size <= 65536 => {
                RepresentationData::Inline(bytes)
            }
            Ok((_, size)) => RepresentationData::Reader {
                size: ContentSize::Known(size),
            },
            Err(error) => RepresentationData::Unavailable(content::reason(&error)),
        };
    }
    Ok(Snapshot::from_parts(inventory, batch, Arc::new(WindowsTranslator))?)
}
pub(super) fn capture(
    owner: Arc<super::worker::Worker>,
    observer: Arc<current::Observer>,
    root: std::path::PathBuf,
) -> Result<CapturedClipboard, ClipboardError> {
    let count = current::sequence();
    // SAFETY: called on the clipboard worker's initialized STA.
    let data = unsafe { OleGetClipboard() }.map_err(|_| ClipboardError::Busy)?;
    let id = OfferId {
        clip: ClipId {
            origin: DeviceId([0; 32]),
            seq: 0,
        },
        generation: count,
    };
    let mut inventory =
        inventory(&data, id).map_err(|error| ClipboardError::ContentUnavailable(content::reason(&error)))?;
    // Privacy control values are small metadata (e.g. CanIncludeInClipboardHistory's
    // BOOL). Retain only the existing bounded inline exception, never a batch.
    for rep in inventory.items.iter_mut().flat_map(|item| &mut item.representations) {
        if rep.standard == Some(StandardFormat::Privacy)
            && matches!(rep.data, RepresentationData::Reader { .. })
            && let Ok(bytes) = read_representation(&data, rep)
            && bytes.len() <= 65536
        {
            rep.data = RepresentationData::Inline(bytes);
        }
    }
    let sensitivity = inventory.items.iter().flat_map(|item| &item.representations).fold(
        crate::Sensitivity::Normal,
        |level, rep| {
            let NativeFormat::WindowsRegistered(name) = &rep.format else {
                return level;
            };
            if name == "Clipboard Viewer Ignore" {
                crate::Sensitivity::Transient
            } else if level != crate::Sensitivity::Transient
                && (name == "ExcludeClipboardContentFromMonitorProcessing"
                    || name == "CanIncludeInClipboardHistory"
                        && matches!(&rep.data,RepresentationData::Inline(bytes) if bytes.get(..4) == Some(&[0,0,0,0])))
            {
                crate::Sensitivity::Concealed
            } else {
                level
            }
        },
    );
    let worker = Arc::new(super::worker::Worker::spawn_reader()?);
    let identity = current::Identity::new(count);
    let files = match real_files(&data).map_err(|error| ClipboardError::ContentUnavailable(content::reason(&error)))? {
        Some(files) => Some(files),
        None => {
            for item in &mut inventory.items {
                item.virtual_files = item.file.is_some();
            }
            virtual_files::Virtual::metadata(&data)
                .map_err(|error| ClipboardError::ContentUnavailable(content::reason(&error)))?
                .map(|files| files.current(worker.clone(), owner.clone(), identity.clone(), inventory.clone(), root))
        }
    };
    let check_identity = identity.clone();
    let read_identity = identity.clone();
    let source = inventory.clone();
    let native = content::clipboard::Clipboard::new(
        inventory,
        Box::new(move |id| {
            let identity = read_identity.clone();
            let inventory = source.clone();
            let owner = owner.clone();
            worker
                .run(move |_| {
                    let result = identity.read(&inventory, |data| {
                        let rep = content::find(&inventory, id)
                            .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))?;
                        read_representation(data, rep)
                            .map(|bytes| Box::new(io::Cursor::new(bytes)) as crate::FileReader)
                    });
                    owner.notify();
                    result
                })
                .map_err(|_| content::unavailable(Unavailable::ReadFailed))?
        }),
        Arc::new(move || check_identity.check()),
        Arc::new(WindowsTranslator),
        files,
    )
    .map_err(|error| ClipboardError::ContentUnavailable(content::reason(&error)))?;
    let items = content::items(&native.inventory());
    if current::sequence() != count {
        return Err(ClipboardError::Busy);
    }
    observer.set(&identity);
    Ok(CapturedClipboard {
        snapshot: ClipSnapshot {
            items,
            sensitivity,
            change_count: count,
        },
        content: Some(native),
    })
}
pub(super) fn real_files(data: &IDataObject) -> io::Result<Option<Arc<dyn content::namespace::Namespace>>> {
    let cf = FORMATETC {
        cfFormat: 15,
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
        ..Default::default()
    };
    // SAFETY: CF_HDROP is read only to identify files. Source paths never become
    // a publishable raw representation on a different computer.
    let roots = unsafe {
        data.GetData(&cf).ok().and_then(|mut medium| {
            let bytes = if medium.tymed == TYMED_HGLOBAL.0 as u32 {
                medium::global(medium.u.hGlobal).ok()
            } else {
                None
            };
            ReleaseStgMedium(&mut medium);
            bytes
        })
    }
    .map(|b| super::files::parse_hdrop(&b, super::ansi_to_string))
    .unwrap_or_default();
    if roots.is_empty() {
        return Ok(None);
    }
    Ok(Some(content::namespace::Local::clipboard(
        roots
            .into_iter()
            .enumerate()
            .map(|(index, path)| (NodeId(index as u64 + 1), path))
            .collect(),
    )?))
}
pub(super) struct WindowsTranslator;
impl Translator for WindowsTranslator {
    fn standard_with_source(
        &self,
        rep: &Representation,
        bytes: &[u8],
        source: &dyn Provider,
    ) -> io::Result<Vec<ClipData>> {
        if matches!(rep.format, NativeFormat::WindowsPredefined(1 | 7)) {
            use windows::Win32::Globalization::*;
            let inventory = source.inventory();
            if let Some(locale) = inventory
                .items
                .iter()
                .find(|item| item.representations.iter().any(|r| r.id == rep.id))
                .and_then(|item| {
                    item.representations
                        .iter()
                        .find(|r| r.format == NativeFormat::WindowsPredefined(16))
                })
            {
                let mut reader = content::open_for(source, locale.id, ClipPlatform::Windows)?;
                let mut value = [0; 4];
                io::Read::read_exact(&mut reader, &mut value)?;
                let field = if rep.format == NativeFormat::WindowsPredefined(7) {
                    LOCALE_IDEFAULTCODEPAGE
                } else {
                    LOCALE_IDEFAULTANSICODEPAGE
                };
                let mut page = [0; 16];
                // SAFETY: bounded writable UTF-16 buffer; locale is native metadata.
                let n = unsafe { GetLocaleInfoW(u32::from_le_bytes(value), field, Some(&mut page)) };
                let codepage: u32 = String::from_utf16_lossy(&page[..n.max(1) as usize - 1])
                    .parse()
                    .map_err(|_| content::unavailable(Unavailable::InvalidData))?;
                let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
                return Ok(vec![ClipData::Text(super::html::crlf_to_lf(&super::decode_codepage(
                    &bytes[..end],
                    codepage,
                )?))]);
            }
        }
        self.standard(rep, bytes)
    }
    fn standard(&self, rep: &Representation, bytes: &[u8]) -> io::Result<Vec<ClipData>> {
        let data = match rep.standard {
            Some(StandardFormat::Text) => {
                let text = if rep.format == NativeFormat::WindowsPredefined(13) {
                    let words: Vec<_> = bytes
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|b| u16::from_le_bytes([b[0], b[1]]))
                        .take_while(|n| *n != 0)
                        .collect();
                    if !bytes.len().is_multiple_of(2) && words.len() * 2 == bytes.len() - 1 {
                        return Err(content::unavailable(Unavailable::InvalidData));
                    }
                    String::from_utf16(&words).map_err(|_| content::unavailable(Unavailable::InvalidData))?
                } else {
                    let codepage = if rep.format == NativeFormat::WindowsPredefined(7) {
                        windows::Win32::Globalization::CP_OEMCP
                    } else {
                        windows::Win32::Globalization::CP_ACP
                    };
                    super::decode_codepage(
                        &bytes[..bytes.iter().position(|n| *n == 0).unwrap_or(bytes.len())],
                        codepage,
                    )?
                };
                ClipData::Text(super::html::crlf_to_lf(&text))
            }
            Some(StandardFormat::Html) => ClipData::Html(
                super::html::decode_cf_html(bytes).ok_or_else(|| content::unavailable(Unavailable::InvalidData))?,
            ),
            Some(StandardFormat::Rtf) => {
                // RTF's \bin control permits embedded NUL bytes. Only native
                // allocation terminators after the document may be removed.
                let end = bytes.iter().rposition(|b| *b != 0).map_or(0, |n| n + 1);
                ClipData::Rtf(bytes[..end].to_vec())
            }
            Some(StandardFormat::Png) => {
                if matches!(&rep.format,NativeFormat::WindowsRegistered(n) if png_name(n)) {
                    super::image::decode_png(bytes).map_err(|e| content::unavailable(e.reason()))?;
                    ClipData::Png(bytes.to_vec())
                } else if rep.format == NativeFormat::WindowsPredefined(6) {
                    ClipData::Png(super::image::tiff_to_png(bytes).map_err(|e| content::unavailable(e.reason()))?)
                } else if rep.medium == Medium::Bitmap {
                    ClipData::Png(super::image::bitmap_to_png(bytes).map_err(|e| content::unavailable(e.reason()))?)
                } else {
                    ClipData::Png(super::image::dib_to_png(bytes).map_err(|e| content::unavailable(e.reason()))?)
                }
            }
            Some(StandardFormat::Privacy) => {
                let NativeFormat::WindowsRegistered(name) = &rep.format else {
                    return Err(content::unavailable(Unavailable::InvalidData));
                };
                crate::formats::custom(ClipPlatform::Windows, name, bytes.to_vec())
            }
            _ => return Err(content::unavailable(Unavailable::Unsupported)),
        };
        Ok(vec![data])
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use windows::Win32::{
        Graphics::Gdi::*,
        System::{
            Com::StructuredStorage::*,
            Ole::{OleInitialize, OleUninitialize},
        },
    };
    #[test]
    fn real_drop_snapshot_keeps_surviving_hdrop_anchor_after_skipping_link() {
        sta(|| {
            use crate::content::{
                Deferred,
                namespace::{Local, Namespace},
            };
            struct NoBytes;
            impl crate::FileContents for NoBytes {
                fn open(&self, _: usize) -> io::Result<crate::FileReader> {
                    panic!("only CF_HDROP metadata is captured")
                }
            }
            for internal in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let good = temp.path().join("ordinary.bin");
                let link = temp.path().join("link");
                std::fs::write(&good, b"ordinary surviving contents\n").unwrap();
                std::os::windows::fs::symlink_file(
                    if internal {
                        good.clone()
                    } else {
                        temp.path().join("absent")
                    },
                    &link,
                )
                .unwrap();
                let paths = vec![link, good];
                let formats = super::super::formats();
                let (object, _) = super::super::vfile::build(
                    super::super::vfile::Spec {
                        cloud: None,
                        descriptors: super::super::files::descriptors(&[]),
                        fixed: vec![(15, super::super::files::build_hdrop(&paths))],
                        contents: Arc::new(NoBytes),
                        cf_descriptor: formats.file_descriptor,
                        cf_contents: formats.file_contents,
                    },
                    false,
                );
                let id = crate::content::url::tests::source(ClipPlatform::Windows, &[])
                    .inventory
                    .id;
                let source = Deferred::new(inventory(&object, id).unwrap(), Arc::new(WindowsTranslator)).unwrap();
                let files = Local::new(
                    paths
                        .into_iter()
                        .enumerate()
                        .map(|(index, path)| (NodeId(index as u64 + 1), path))
                        .collect(),
                )
                .unwrap();
                assert!(source.attach_files(files.clone()));
                assert_eq!(source.inventory().items[0].file, Some(NodeId(2)));
                let drop = snapshot(&object, temp.path(), id).unwrap();
                assert!(drop.namespace().is_none(), "real Drop snapshots non-file formats only");
                assert_eq!(
                    drop.inventory().items[0].file,
                    Some(NodeId(1)),
                    "raw COM anchor is unchanged"
                );
                assert!(
                    source.complete(drop),
                    "real Drop must reconcile the captured selection before comparison"
                );
                let node = files.select(NodeId(2)).unwrap();
                assert_eq!(
                    content::read_all(&mut files.open(node.id, node.version, 0).unwrap()).unwrap(),
                    b"ordinary surviving contents\n"
                );
            }
        });
    }

    #[test]
    fn url_formats_have_portable_link_semantics() {
        for name in ["UniformResourceLocatorW", "UniformResourceLocator", "text/x-moz-url"] {
            assert_eq!(
                format!("{:?}", standard(&NativeFormat::WindowsRegistered(name.into()))),
                "Some(Url)"
            );
        }
    }
    #[test]
    fn link_data_object_capture_projects_w_and_matching_mozilla_title_without_native_changes() {
        sta(|| {
            struct NoFiles;
            impl crate::FileContents for NoFiles {
                fn open(&self, _: usize) -> io::Result<crate::FileReader> {
                    unreachable!()
                }
            }
            use crate::content::url::tests::{source, wide_bytes};
            let entries = [
                ("UniformResourceLocator", b"https://different.invalid/\0".to_vec()),
                ("text/x-moz-url", wide_bytes("https://example.invalid/\nTitle 猫")),
                ("UniformResourceLocatorW", wide_bytes("https://example.invalid/")),
            ];
            let fixed = entries
                .iter()
                .map(|(name, bytes)| {
                    (
                        u32::from(format_id(&NativeFormat::WindowsRegistered((*name).into())).unwrap()),
                        bytes.clone(),
                    )
                })
                .collect();
            let formats = super::super::formats();
            let (object, _) = super::super::vfile::build(
                super::super::vfile::Spec {
                    cloud: None,
                    descriptors: super::super::files::descriptors(&[]),
                    fixed,
                    contents: Arc::new(NoFiles),
                    cf_descriptor: formats.file_descriptor,
                    cf_contents: formats.file_contents,
                },
                false,
            );
            let fixture = source(ClipPlatform::Windows, &[]);
            let captured = inventory(&object, fixture.inventory.id).unwrap();
            assert_eq!(captured.wanted(ClipPlatform::Windows).count(), 3);
            assert_eq!(captured.wanted(ClipPlatform::Mac).count(), 3);
            let dir = tempfile::tempdir().unwrap();
            let snapshot = snapshot(&object, dir.path(), captured.id).unwrap();
            let rep = captured.wanted(ClipPlatform::Mac).find(|rep| matches!(&rep.format, NativeFormat::WindowsRegistered(name) if name == "UniformResourceLocatorW")).unwrap();
            let bytes =
                content::read_all(&mut content::open_for(snapshot.as_ref(), rep.id, ClipPlatform::Mac).unwrap())
                    .unwrap();
            let link = content::url::Link::decode(&bytes).unwrap();
            assert_eq!(link.url, "https://example.invalid/");
            assert_eq!(link.title.as_deref(), Some("Title 猫"));
            for rep in captured.wanted(ClipPlatform::Windows) {
                assert_eq!(
                    content::read_all(
                        &mut content::open_for(snapshot.as_ref(), rep.id, ClipPlatform::Windows).unwrap()
                    )
                    .unwrap(),
                    read_representation(&object, rep).unwrap()
                );
            }
        });
    }
    pub(super) fn sta(test: impl FnOnce() + Send + 'static) {
        std::thread::spawn(move || {
            // SAFETY: isolated test STA; no clipboard publication, windows, or
            // injected input. All COM resources drop before uninitialization.
            unsafe {
                OleInitialize(None).unwrap();
            }
            test();
            // SAFETY: test has released its COM objects on this same STA.
            unsafe {
                OleUninitialize();
            }
        })
        .join()
        .unwrap();
    }
    #[test]
    fn bitmap_first_image_offer_serves_portable_png_and_preserves_native_bytes() {
        sta(|| {
            // CF_BITMAP can precede the synthesized DIB formats. The Mac binds
            // the first image to public.png, so that representation must work.
            let bitmap = [
                2i32.to_le_bytes().as_slice(),
                2i32.to_le_bytes().as_slice(),
                &[0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0],
            ]
            .concat();
            let mut native = medium::restore(Medium::Bitmap, &bitmap).unwrap();
            let bytes = medium::serialize(&native, Medium::Bitmap).unwrap();
            // SAFETY: this test owns the apartment-local medium.
            unsafe { ReleaseStgMedium(&mut native) };
            let id = crate::content::tests::fixture().inventory().id;
            let mut representations = vec![Representation {
                id: RepresentationId(2),
                format: NativeFormat::WindowsPredefined(2),
                standard: Some(StandardFormat::Png),
                medium: Medium::Bitmap,
                aspect: DVASPECT_CONTENT.0,
                index: -1,
                data: RepresentationData::Inline(bytes.clone()),
            }];
            for (id, cf) in [(3, 8), (4, 17)] {
                representations.push(Representation {
                    id: RepresentationId(id),
                    format: NativeFormat::WindowsPredefined(cf),
                    medium: Medium::HGlobal,
                    data: RepresentationData::Reader {
                        size: ContentSize::Unknown,
                    },
                    ..representations[0].clone()
                });
            }
            let inventory = Inventory {
                id,
                platform: ClipPlatform::Windows,
                roots: vec![],
                items: vec![ContentItem {
                    id: ItemId(0),
                    file: None,
                    virtual_files: false,
                    representations,
                }],
            };
            assert!(!inventory.automatic_pull(ClipPlatform::Mac));
            let selected = inventory.wanted(ClipPlatform::Mac).next().unwrap().id;
            assert_eq!(selected, RepresentationId(2));
            let source = content::clipboard::Clipboard::new(
                inventory,
                Box::new(|_| panic!("inline bitmap requires no system clipboard read")),
                Arc::new(|| Ok(())),
                Arc::new(WindowsTranslator),
                None,
            )
            .unwrap();
            let read =
                |target| content::read_all(&mut content::open_for(source.as_ref(), selected, target).unwrap()).unwrap();
            assert_eq!(read(ClipPlatform::Windows), bytes);
            let decoded = super::super::image::decode_png(&read(ClipPlatform::Mac)).unwrap();
            assert_eq!((decoded.width, decoded.height), (2, 2));
            assert_eq!(
                decoded.data,
                [0, 0, 255, 255, 255, 255, 255, 255, 255, 0, 0, 255, 0, 255, 0, 255]
            );
        });
    }
    #[test]
    fn native_memory_stream_bitmap_and_palette_reconstruct_without_translation() {
        sta(|| {
            let bitmap = [
                2i32.to_le_bytes().as_slice(),
                2i32.to_le_bytes().as_slice(),
                &[0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0],
            ]
            .concat();
            for (kind, bytes) in [
                (Medium::HGlobal, vec![0, 255, 0, 17]),
                (Medium::Stream, vec![0, 255, 0, 17]),
                (Medium::Bitmap, bitmap),
                (Medium::Palette, vec![0, 3, 2, 0, 11, 22, 33, 0, 44, 55, 66, 0]),
            ] {
                let mut original = medium::restore(kind, &bytes).unwrap();
                let first = medium::serialize(&original, kind).unwrap();
                // GlobalAlloc may include extra zero-initialized allocator bytes;
                // the second native roundtrip must retain the exact offered size.
                assert_eq!(&first[..bytes.len()], bytes);
                let mut reconstructed = medium::restore(kind, &first).unwrap();
                assert_eq!(medium::serialize(&reconstructed, kind).unwrap(), first);
                // SAFETY: tests own both returned media, each released once.
                unsafe {
                    ReleaseStgMedium(&mut original);
                    ReleaseStgMedium(&mut reconstructed);
                }
            }
        });
    }
    #[test]
    fn structured_storage_preserves_streams_and_class_identity() {
        sta(|| unsafe {
            // SAFETY: apartment-local compound storage in owned memory; no
            // system clipboard interaction and all STGMEDIUM ownership is local.
            let lock = CreateILockBytesOnHGlobal(None, true).unwrap();
            let storage =
                StgCreateDocfileOnILockBytes(&lock, STGM_CREATE | STGM_READWRITE | STGM_SHARE_EXCLUSIVE, 0).unwrap();
            let class = windows::core::GUID::from_u128(0x12345678123456781234567812345678);
            storage.SetClass(&class).unwrap();
            let stream = storage
                .CreateStream(
                    windows::core::w!("payload"),
                    STGM_CREATE | STGM_READWRITE | STGM_SHARE_EXCLUSIVE,
                    0,
                    0,
                )
                .unwrap();
            let bytes = [0u8, 255, 0, 9];
            let mut n = 0;
            stream.Write(bytes.as_ptr().cast(), 4, Some(&mut n)).ok().unwrap();
            assert_eq!(n, 4);
            drop(stream);
            storage.Commit(0).unwrap();
            let mut original = STGMEDIUM {
                tymed: TYMED_ISTORAGE.0 as u32,
                u: STGMEDIUM_0 {
                    pstg: std::mem::ManuallyDrop::new(Some(storage)),
                },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            };
            let encoded = medium::serialize(&original, Medium::Storage).unwrap();
            let mut restored = medium::restore(Medium::Storage, &encoded).unwrap();
            let storage = restored.u.pstg.as_ref().unwrap();
            let mut stat = STATSTG::default();
            storage.Stat(&mut stat, STATFLAG_NONAME.0 as u32).unwrap();
            assert_eq!(stat.clsid, class);
            let stream = storage
                .OpenStream(windows::core::w!("payload"), None, STGM_READ | STGM_SHARE_EXCLUSIVE, 0)
                .unwrap();
            let mut read = [0u8; 4];
            stream.Read(read.as_mut_ptr().cast(), 4, Some(&mut n)).ok().unwrap();
            assert_eq!(read, bytes);
            drop(stream);
            ReleaseStgMedium(&mut original);
            ReleaseStgMedium(&mut restored);
        });
    }
    #[test]
    fn metafile_media_preserve_their_serialized_drawing_records() {
        sta(|| unsafe {
            // SAFETY: memory recording DCs only; no screen drawing/input.
            let dc = CreateMetaFileW(None);
            assert!(!dc.is_invalid());
            let _ = LineTo(dc, 10, 20);
            let mf = CloseMetaFile(dc);
            assert!(!mf.is_invalid());
            let n = GetMetaFileBitsEx(mf, 0, None);
            let mut bytes = vec![0u8; n as usize + 12];
            bytes[..4].copy_from_slice(&1i32.to_le_bytes());
            bytes[4..8].copy_from_slice(&10i32.to_le_bytes());
            bytes[8..12].copy_from_slice(&20i32.to_le_bytes());
            assert_eq!(GetMetaFileBitsEx(mf, n, Some(bytes[12..].as_mut_ptr().cast())), n);
            let _ = DeleteMetaFile(mf);
            let dc = CreateEnhMetaFileW(None, None, None, None);
            assert!(!dc.is_invalid());
            let _ = LineTo(dc, 10, 20);
            let emf = CloseEnhMetaFile(dc);
            let n = GetEnhMetaFileBits(emf, None);
            let mut enhanced = vec![0u8; n as usize];
            assert_eq!(GetEnhMetaFileBits(emf, Some(&mut enhanced)), n);
            let _ = DeleteEnhMetaFile(Some(emf));
            for (kind, bytes) in [(Medium::Metafile, bytes), (Medium::EnhancedMetafile, enhanced)] {
                let mut restored = medium::restore(kind, &bytes).unwrap();
                assert_eq!(medium::serialize(&restored, kind).unwrap(), bytes);
                ReleaseStgMedium(&mut restored);
            }
        });
    }
}

#[cfg(all(test, debug_assertions))]
mod audit_tests;
