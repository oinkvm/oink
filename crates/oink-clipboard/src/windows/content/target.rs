use super::super::vfile;
use super::diagnostics::{HResultClass, Observation, Stage, reason_code};
use super::*;

pub(super) struct Binding {
    pub format: FORMATETC,
    pub representation: RepresentationId,
    alternatives: Vec<RepresentationId>,
    pub medium: Medium,
    pub foreign: bool,
}
type RenderedFormats = std::cell::RefCell<std::collections::HashMap<(u16, u32, i32), Arc<Vec<u8>>>>;
/// Owned remote data never contains apartment-bound COM pointers. Only this
/// IDataObject (built on the native worker) constructs handles for GetData.
pub(crate) struct Target {
    pub spec: NativeSpec,
    bindings: Vec<Binding>,
    rendered: Option<RenderedFormats>,
}
impl Target {
    #[cfg(all(test, debug_assertions))]
    pub(crate) fn formats(&self) -> Vec<FORMATETC> {
        self.bindings.iter().map(|b| b.format).collect()
    }
    /// Native file controls occupy the source's positions. They are rebuilt by
    /// the file service, never moved behind every ordinary representation.
    pub(crate) fn formats_with_files(&self, files: &[FORMATETC]) -> Vec<FORMATETC> {
        let mut output = Vec::new();
        let mut included = vec![false; files.len()];
        for rep in self.spec.inventory.wanted(ClipPlatform::Windows) {
            if matches!(rep.data, RepresentationData::File(_)) {
                let original = format_id(&rep.format);
                for (index, file) in files.iter().enumerate() {
                    let matches = original.is_none_or(|cf| cf == file.cfFormat || cf == 15)
                        || matches!(&rep.format, NativeFormat::WindowsRegistered(name) if name.eq_ignore_ascii_case("FileGroupDescriptor") && index == 0);
                    if matches && !included[index] {
                        output.push(*file);
                        included[index] = true;
                    }
                }
            } else {
                output.extend(
                    self.bindings
                        .iter()
                        .filter(|binding| binding.representation == rep.id)
                        .map(|binding| binding.format),
                );
            }
        }
        for (index, file) in files.iter().enumerate() {
            if !included[index] {
                output.push(*file);
            }
        }
        output
    }
    fn bindings<'a>(&'a self, f: &'a FORMATETC) -> impl Iterator<Item = &'a Binding> {
        self.bindings.iter().filter(|b| {
            b.format.cfFormat == f.cfFormat
                && b.format.dwAspect == f.dwAspect
                && b.format.lindex == f.lindex
                && b.format.tymed & f.tymed != 0
                && f.ptd.is_null()
        })
    }
    pub(crate) fn check(&self, f: &FORMATETC) -> bool {
        self.bindings(f).next().is_some()
    }
    pub(crate) fn get(&self, f: &FORMATETC, observe: impl Fn() -> Observation) -> windows::core::Result<STGMEDIUM> {
        let mut failure = windows::core::Error::from(DV_E_FORMATETC);
        for binding in self.bindings(f) {
            for representation in std::iter::once(binding.representation).chain(binding.alternatives.iter().copied()) {
                let began = observe();
                let mut stage = Stage::Open;
                let mut reason = 0;
                let result = self
                    .read_medium(f, binding, representation, &mut stage, &mut reason)
                    .inspect_err(|error| {
                        // Sample at the failure, not merely at entry: a blocking read may
                        // outlive this object's ownership of the clipboard. Never log COM
                        // pointers, source identity, format names, paths or error text.
                        let failed = observe();
                        tracing::warn!(
                            clipboard_object = failed.object,
                            clipboard_offer = self.spec.inventory.id.clip.seq,
                            clipboard_generation = self.spec.inventory.id.generation,
                            clipboard_representation = representation.0,
                            clipboard_stage = stage as u8,
                            clipboard_reason = reason,
                            clipboard_hresult_class = HResultClass::from_code(error.code()) as u8,
                            clipboard_hresult = error.code().0 as u32,
                            clipboard_state_begin = began.state as u8,
                            clipboard_state = failed.state as u8,
                            clipboard_replacement = failed.replacement,
                            "native clipboard representation unavailable"
                        );
                    });
                match result {
                    Ok(value) => return Ok(value),
                    Err(error) => failure = error,
                }
            }
        }
        Err(failure)
    }
    fn read_medium(
        &self,
        f: &FORMATETC,
        binding: &Binding,
        representation_id: RepresentationId,
        stage: &mut Stage,
        reason: &mut u8,
    ) -> windows::core::Result<STGMEDIUM> {
        if f.cfFormat as u32 == super::super::formats().preferred_drop_effect
            && self.spec.inventory.items.iter().any(|item| item.file.is_some())
        {
            // Transfer semantics are COPY even when a source advertised a cut.
            *stage = Stage::RestoreMedium;
            return medium::restore(Medium::HGlobal, &1u32.to_le_bytes());
        }
        // Hover consumers may request text/HTML/URL on every DragOver. A
        // completed remote slot still opens a leased file and converts bytes;
        // doing that repeatedly on the OLE STA stalls feedback. Cache successful
        // HGLOBAL renderings lazily for this drag only, never before Begin.
        // No borrow survives provider/COM calls (which can reenter the STA), and
        // each caller still receives a fresh independently owned medium.
        let cache = self.rendered.as_ref().filter(|_| binding.medium == Medium::HGlobal);
        let key = (f.cfFormat, f.dwAspect, f.lindex);
        let cached = cache.and_then(|cache| cache.borrow().get(&key).cloned());
        if let Some(bytes) = cached {
            *stage = Stage::RestoreMedium;
            return medium::restore(binding.medium, &bytes);
        }
        let result = (|| {
            let context = oink_foundation::drag_trace::Context {
                offer: self.spec.inventory.id.clip.seq,
                representation: representation_id.0,
                ..oink_foundation::drag_trace::Context::current()
            };
            let opened =
                oink_foundation::drag_trace::Trace::new(oink_foundation::drag_trace::stage::NATIVE_OPEN, context);
            let result = opened.scope(|| self.spec.contents.open(representation_id));
            opened.io(&result);
            drop(opened);
            let mut reader = result?;
            *stage = Stage::Read;
            let read =
                oink_foundation::drag_trace::Trace::new(oink_foundation::drag_trace::stage::NATIVE_READ, context);
            let result = content::read_all(&mut reader);
            read.io(&result);
            drop(read);
            let bytes = result?;
            if !binding.foreign {
                return Ok(bytes);
            }
            *stage = Stage::Decode;
            let representation = content::find(&self.spec.inventory, representation_id)
                .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))?;
            if representation.standard == Some(StandardFormat::Url) {
                let format_name = match name(f.cfFormat) {
                    Some(NativeFormat::WindowsRegistered(name)) => name,
                    Some(NativeFormat::WindowsPredefined(13)) => "CF_UNICODETEXT".into(),
                    _ => return Err(content::unavailable(Unavailable::Unsupported)),
                };
                return content::url::Link::decode(&bytes)?.render(ClipPlatform::Windows, &format_name);
            }
            let standard = content::decode_standard(representation, &bytes)?;
            *stage = Stage::Convert;
            super::super::prepare(&standard, self.spec.sensitivity)
                .into_iter()
                .find(|(cf, _)| *cf == f.cfFormat as u32)
                .map(|(_, bytes)| bytes)
                .ok_or_else(|| content::unavailable(Unavailable::Unsupported))
        })();
        let bytes = result.map_err(|error| {
            *reason = reason_code(content::reason(&error));
            windows::core::Error::from(STG_E_READFAULT)
        })?;
        if binding.medium == Medium::File {
            *stage = Stage::FileMedium;
            let batch = self.spec.contents.local_batch().ok_or(STG_E_READFAULT)?;
            return file_medium(batch, representation_id);
        }
        *stage = Stage::RestoreMedium;
        let medium = medium::restore(binding.medium, &bytes)?;
        if let Some(cache) = cache {
            cache.borrow_mut().insert(key, Arc::new(bytes));
        }
        Ok(medium)
    }
}
fn foreign_ids(rep: &Representation) -> Vec<u16> {
    let formats = super::super::formats();
    match rep.standard {
        Some(StandardFormat::Text) => vec![13],
        Some(StandardFormat::Html) => vec![formats.html as u16],
        Some(StandardFormat::Rtf) => vec![formats.rtf as u16],
        Some(StandardFormat::Png) => vec![formats.png as u16, 17, 8],
        Some(StandardFormat::Privacy) => {
            let (NativeFormat::Mac(name), RepresentationData::Inline(bytes)) = (&rep.format, &rep.data) else {
                return Vec::new();
            };
            super::super::prepare(
                &[crate::formats::custom(ClipPlatform::Mac, name, bytes.clone())],
                crate::Sensitivity::Normal,
            )
            .into_iter()
            .map(|(cf, _)| cf as u16)
            .collect()
        }
        _ => Vec::new(),
    }
}
pub(crate) fn build_drag(spec: NativeSpec) -> Result<Target, ClipboardError> {
    let mut target = build(spec)?;
    target.rendered = Some(Default::default());
    Ok(target)
}
pub(crate) fn build(spec: NativeSpec) -> Result<Target, ClipboardError> {
    let foreign = spec.inventory.platform != ClipPlatform::Windows;
    let mut bindings = Vec::<Binding>::new();
    for rep in spec.inventory.wanted(ClipPlatform::Windows) {
        if matches!(rep.data, RepresentationData::File(_)) {
            continue;
        }
        let ids = if foreign {
            if rep.standard == Some(StandardFormat::Url) {
                let item = spec
                    .inventory
                    .items
                    .iter()
                    .find(|item| item.representations.iter().any(|r| r.id == rep.id))
                    .ok_or(ClipboardError::Unsupported)?;
                content::url::names(item, ClipPlatform::Windows)
                    .into_iter()
                    .filter_map(|name| {
                        if name == "CF_UNICODETEXT" {
                            Some(13)
                        } else {
                            format_id(&NativeFormat::WindowsRegistered(name))
                        }
                    })
                    .collect()
            } else {
                foreign_ids(rep)
            }
        } else {
            vec![format_id(&rep.format).ok_or(ClipboardError::Unsupported)?]
        };
        for cf in ids {
            let medium = if foreign { Medium::HGlobal } else { rep.medium };
            let aspect = if foreign { DVASPECT_CONTENT.0 } else { rep.aspect };
            let index = if foreign { -1 } else { rep.index };
            if foreign
                && let Some(existing) = bindings.iter_mut().find(|b| {
                    b.format.cfFormat == cf
                        && b.format.dwAspect == aspect
                        && b.format.lindex == index
                        && b.medium == medium
                })
            {
                // One FORMATETC can have several independently readable source
                // candidates. Keep all of them behind the single native type.
                existing.alternatives.push(rep.id);
                continue;
            }
            bindings.push(Binding {
                format: FORMATETC {
                    cfFormat: cf,
                    ptd: std::ptr::null_mut(),
                    dwAspect: aspect,
                    lindex: index,
                    tymed: medium::tymed(medium),
                },
                representation: rep.id,
                alternatives: Vec::new(),
                medium,
                foreign,
            });
        }
    }
    Ok(Target {
        spec,
        bindings,
        rendered: None,
    })
}
pub(crate) fn set_clipboard_guarded(
    spec: NativeSpec,
    guard: crate::content::ReaderGuard,
) -> Result<u64, ClipboardError> {
    struct NoFiles;
    impl crate::FileContents for NoFiles {
        fn open(&self, _: usize) -> io::Result<crate::FileReader> {
            Err(content::unavailable(Unavailable::NoSuchItem))
        }
    }
    let formats = super::super::formats();
    let with_files = !spec.files.is_empty();
    let cloud = spec.file_contents.as_ref().and_then(|contents| {
        super::super::cloud::Offer::new(spec.files.clone(), contents.clone(), Some(&spec.inventory))
    });
    let file_spec = vfile::Spec {
        cloud,
        descriptors: super::super::files::descriptors(&spec.files),
        fixed: Vec::new(),
        contents: spec.file_contents.clone().unwrap_or_else(|| Arc::new(NoFiles)),
        cf_descriptor: formats.file_descriptor,
        cf_contents: formats.file_contents,
    };
    let target = build(spec)?;
    guard().map_err(|_| ClipboardError::Busy)?;
    vfile::set_native(file_spec, with_files, target)
}

// With pUnkForRelease set, ReleaseStgMedium delegates TYMED_FILE cleanup to this
// owner. The cache deletes the data file; OLE still frees the filename even with
// pUnkForRelease set. This owner only pins the batch until native release.
// https://learn.microsoft.com/windows/win32/api/ole2/nf-ole2-releasestgmedium
#[windows::core::implement(ISequentialStream, Agile = false)]
struct FileOwner {
    _read: crate::cache::SourceLease,
    _batch: Batch,
}
impl ISequentialStream_Impl for FileOwner_Impl {
    fn Read(&self, _: *mut core::ffi::c_void, _: u32, _: *mut u32) -> windows::core::HRESULT {
        windows::Win32::Foundation::E_NOTIMPL
    }
    fn Write(&self, _: *const core::ffi::c_void, _: u32, _: *mut u32) -> windows::core::HRESULT {
        windows::Win32::Foundation::E_NOTIMPL
    }
}
fn file_medium(batch: Batch, id: RepresentationId) -> windows::core::Result<STGMEDIUM> {
    use std::{mem::ManuallyDrop, os::windows::ffi::OsStrExt};
    use windows::core::{Interface, PWSTR};
    let read = batch
        .read_lease()
        .map_err(|_| windows::core::Error::from(STG_E_READFAULT))?;
    let wide: Vec<u16> = batch
        .path()
        .join(format!("r-{}", id.0))
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: allocation includes the NUL terminator. FileOwner owns the
    // backing file pin; ReleaseStgMedium owns the filename allocation.
    unsafe {
        let name = CoTaskMemAlloc(wide.len() * 2).cast::<u16>();
        if name.is_null() {
            return Err(windows::Win32::Foundation::E_OUTOFMEMORY.into());
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), name, wide.len());
        let owner: ISequentialStream = FileOwner {
            _batch: batch,
            _read: read,
        }
        .into();
        Ok(STGMEDIUM {
            tymed: TYMED_FILE.0 as u32,
            u: STGMEDIUM_0 {
                lpszFileName: PWSTR(name),
            },
            pUnkForRelease: ManuallyDrop::new(Some(owner.cast()?)),
        })
    }
}

#[cfg(all(test, debug_assertions))]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn chrome_native(delay: std::time::Duration) -> (NativeSpec, Arc<std::sync::atomic::AtomicUsize>) {
        use crate::content::url::tests::{Projected, source};
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Counted {
            source: Projected,
            opens: Arc<AtomicUsize>,
            delay: std::time::Duration,
        }
        impl Provider for Counted {
            fn inventory(&self) -> Inventory {
                self.source.inventory()
            }
            fn translator(&self) -> Arc<dyn Translator> {
                self.source.translator()
            }
            fn open(&self, id: RepresentationId) -> io::Result<crate::FileReader> {
                self.opens.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(self.delay);
                if matches!(id, RepresentationId(2 | 4)) {
                    self.source.0.open(id)
                } else {
                    self.source.open(id)
                }
            }
        }
        let mut memory = source(
            ClipPlatform::Mac,
            &[
                ("public.utf8-plain-text", b"default".to_vec()),
                (
                    "public.html",
                    b"<a href=\"https://example.invalid/\">default</a>".to_vec(),
                ),
                ("public.url", b"https://example.invalid/".to_vec()),
                ("public.png", crate::dnd::preview::asymmetric_png()),
            ],
        );
        memory.inventory.items[0].representations[1].standard = Some(StandardFormat::Html);
        memory.inventory.items[0].representations[3].standard = Some(StandardFormat::Png);
        let inventory = memory.inventory();
        let opens = Arc::new(AtomicUsize::new(0));
        (
            NativeSpec {
                inventory,
                contents: Arc::new(Counted {
                    source: Projected(memory, ClipPlatform::Windows),
                    opens: opens.clone(),
                    delay,
                }),
                files: vec![],
                file_contents: None,
                sensitivity: crate::Sensitivity::Normal,
            },
            opens,
        )
    }
    #[test]
    fn drag_native_formats_render_lazily_once_and_return_independent_media() {
        super::super::tests::sta(|| {
            let (spec, opens) = chrome_native(std::time::Duration::ZERO);
            let target = build_drag(spec).unwrap();
            assert_eq!(
                opens.load(std::sync::atomic::Ordering::Relaxed),
                0,
                "drag start must not read content"
            );
            for format in target.formats() {
                let mut expected = None;
                for _ in 0..3 {
                    let mut value = target
                        .get(&format, || diagnostics::Publication::new().observe(Some(false)))
                        .unwrap();
                    let bytes = medium::serialize(&value, Medium::HGlobal).unwrap();
                    if let Some(expected) = &expected {
                        assert_eq!(&bytes, expected);
                    } else {
                        expected = Some(bytes);
                    }
                    // SAFETY: independent media returned by GetData are released exactly once.
                    unsafe {
                        ReleaseStgMedium(&mut value);
                    }
                }
            }
            assert_eq!(
                opens.load(std::sync::atomic::Ordering::Relaxed),
                target.formats().len(),
                "hover must reuse the rendered format instead of reopening content on every move"
            );
        });
    }
    #[test]
    fn successful_drag_fallback_does_not_retry_an_unavailable_preferred_representation_per_move() {
        super::super::tests::sta(|| {
            let (mut spec, opens) = chrome_native(std::time::Duration::ZERO);
            let mut missing = spec.inventory.items[0].representations[0].clone();
            missing.id = RepresentationId(99);
            spec.inventory.items[0].representations.insert(0, missing);
            let target = build_drag(spec).unwrap();
            let format = target.formats().into_iter().find(|f| f.cfFormat == 13).unwrap();
            for _ in 0..3 {
                let mut value = target
                    .get(&format, || diagnostics::Publication::new().observe(Some(false)))
                    .unwrap();
                // SAFETY: each successful GetData returned an independent owned medium.
                unsafe {
                    ReleaseStgMedium(&mut value);
                }
            }
            assert_eq!(
                opens.load(std::sync::atomic::Ordering::Relaxed),
                2,
                "once a native format renders successfully, hovering must not retry earlier failed candidates"
            );
        });
    }
    #[test]
    fn drag_render_cache_retries_failures_and_does_not_apply_to_clipboard_objects() {
        struct Retry {
            inner: Arc<dyn Provider>,
            first: std::sync::atomic::AtomicBool,
        }
        impl Provider for Retry {
            fn inventory(&self) -> Inventory {
                self.inner.inventory()
            }
            fn translator(&self) -> Arc<dyn Translator> {
                self.inner.translator()
            }
            fn open(&self, id: RepresentationId) -> io::Result<crate::FileReader> {
                if self.first.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                self.inner.open(id)
            }
        }
        super::super::tests::sta(|| {
            for drag in [false, true] {
                let (mut spec, opens) = chrome_native(std::time::Duration::ZERO);
                spec.contents = Arc::new(Retry {
                    inner: spec.contents,
                    first: std::sync::atomic::AtomicBool::new(true),
                });
                let target = if drag { build_drag(spec) } else { build(spec) }.unwrap();
                let format = target.formats().into_iter().find(|f| f.cfFormat == 13).unwrap();
                assert!(
                    target
                        .get(&format, || diagnostics::Publication::new().observe(Some(false)))
                        .is_err()
                );
                for _ in 0..2 {
                    let mut medium = target
                        .get(&format, || diagnostics::Publication::new().observe(Some(false)))
                        .unwrap();
                    // SAFETY: this successful GetData returned one owned medium.
                    unsafe {
                        ReleaseStgMedium(&mut medium);
                    }
                }
                assert_eq!(
                    opens.load(std::sync::atomic::Ordering::Relaxed),
                    if drag { 1 } else { 2 }
                );
            }
        });
    }
    #[test]
    fn mac_link_projection_publishes_unicode_ansi_mozilla_title_and_text_in_ole_object() {
        use content::url::tests::{Projected, source, wide_bytes};
        std::thread::spawn(|| {
            // SAFETY: private STA and IDataObjects; never touches the desktop clipboard.
            unsafe {
                windows::Win32::System::Ole::OleInitialize(None).unwrap();
            }
            for has_title in [false, true] {
                let mut entries = vec![("public.url", b"https://example.invalid/".to_vec())];
                if has_title {
                    entries.push(("public.url-name", "A link 猫".as_bytes().to_vec()));
                }
                let source = source(ClipPlatform::Mac, &entries);
                let target = build(NativeSpec {
                    inventory: source.inventory(),
                    contents: Arc::new(Projected(source, ClipPlatform::Windows)),
                    files: vec![],
                    file_contents: None,
                    sensitivity: crate::Sensitivity::Normal,
                })
                .unwrap();
                let formats = target.formats();
                assert_eq!(formats.len(), if has_title { 4 } else { 3 });
                for (format_name, expected) in [
                    ("UniformResourceLocatorW", wide_bytes("https://example.invalid/")),
                    ("UniformResourceLocator", b"https://example.invalid/\0".to_vec()),
                    ("CF_UNICODETEXT", wide_bytes("https://example.invalid/")),
                ] {
                    let cf = if format_name == "CF_UNICODETEXT" {
                        13
                    } else {
                        format_id(&NativeFormat::WindowsRegistered(format_name.into())).unwrap()
                    };
                    let format = formats.iter().find(|f| f.cfFormat == cf).unwrap();
                    let mut medium = target
                        .get(format, || super::diagnostics::Publication::new().observe(Some(false)))
                        .unwrap();
                    assert_eq!(medium::serialize(&medium, Medium::HGlobal).unwrap(), expected);
                    // SAFETY: release the single owned GetData medium.
                    unsafe {
                        ReleaseStgMedium(&mut medium);
                    }
                }
                if has_title {
                    let cf = format_id(&NativeFormat::WindowsRegistered("text/x-moz-url".into())).unwrap();
                    let format = formats.iter().find(|f| f.cfFormat == cf).unwrap();
                    let mut medium = target
                        .get(format, || super::diagnostics::Publication::new().observe(Some(false)))
                        .unwrap();
                    assert_eq!(
                        medium::serialize(&medium, Medium::HGlobal).unwrap(),
                        wide_bytes("https://example.invalid/\nA link 猫")
                    );
                    // SAFETY: release the single owned GetData medium.
                    unsafe {
                        ReleaseStgMedium(&mut medium);
                    }
                }
            }
            // SAFETY: all private apartment objects have been released.
            unsafe {
                windows::Win32::System::Ole::OleUninitialize();
            }
        })
        .join()
        .unwrap();
    }
    #[test]
    fn projected_link_keeps_existing_text_in_either_native_format_order() {
        use content::url::tests::{Projected, source, wide_bytes};
        std::thread::spawn(|| {
            // SAFETY: isolated private STA; no system clipboard publication.
            unsafe {
                windows::Win32::System::Ole::OleInitialize(None).unwrap();
            }
            for url_first in [false, true] {
                let mut entries = vec![
                    ("public.url", b"https://example.invalid/".to_vec()),
                    ("public.utf8-plain-text", b"Own text".to_vec()),
                ];
                if !url_first {
                    entries.reverse();
                }
                let source = source(ClipPlatform::Mac, &entries);
                let target = build(NativeSpec {
                    inventory: source.inventory(),
                    contents: Arc::new(Projected(source, ClipPlatform::Windows)),
                    files: vec![],
                    file_contents: None,
                    sensitivity: crate::Sensitivity::Normal,
                })
                .unwrap();
                let formats = target.formats();
                assert_eq!(formats.iter().filter(|f| f.cfFormat == 13).count(), 1);
                let mut medium = target
                    .get(formats.iter().find(|f| f.cfFormat == 13).unwrap(), || {
                        super::diagnostics::Publication::new().observe(Some(false))
                    })
                    .unwrap();
                assert_eq!(
                    medium::serialize(&medium, Medium::HGlobal).unwrap(),
                    wide_bytes("Own text")
                );
                // SAFETY: release the one owned GetData medium.
                unsafe {
                    ReleaseStgMedium(&mut medium);
                }
            }
            // SAFETY: every private COM object has been released.
            unsafe {
                windows::Win32::System::Ole::OleUninitialize();
            }
        })
        .join()
        .unwrap();
    }
    #[test]
    fn native_file_protocol_slots_and_repeated_plain_formats_keep_source_order() {
        std::thread::spawn(|| {
            // SAFETY: isolated STA, no clipboard publication or desktop input.
            unsafe {
                windows::Win32::System::Ole::OleInitialize(None).unwrap();
            }
            struct NoFiles;
            impl crate::FileContents for NoFiles {
                fn open(&self, _: usize) -> io::Result<crate::FileReader> {
                    unreachable!()
                }
            }
            let offered = [
                "Oink.Order.A",
                "FileGroupDescriptorW",
                "Oink.Order.B",
                "FileContents",
                "Oink.Order.A",
            ];
            let representations = offered
                .iter()
                .enumerate()
                .map(|(i, name)| Representation {
                    id: RepresentationId(i as u32),
                    format: NativeFormat::WindowsRegistered((*name).into()),
                    medium: if *name == "FileContents" {
                        Medium::Stream
                    } else {
                        Medium::HGlobal
                    },
                    aspect: DVASPECT_CONTENT.0,
                    index: -1,
                    standard: None,
                    data: if *name == "FileContents" || *name == "FileGroupDescriptorW" {
                        RepresentationData::File(NodeId(1))
                    } else {
                        RepresentationData::Inline(vec![i as u8])
                    },
                })
                .collect();
            let inventory = Inventory {
                id: OfferId {
                    clip: ClipId {
                        origin: DeviceId([1; 32]),
                        seq: 1,
                    },
                    generation: 1,
                },
                platform: ClipPlatform::Windows,
                items: vec![ContentItem {
                    virtual_files: false,
                    id: ItemId(0),
                    file: Some(NodeId(1)),
                    representations,
                }],
                roots: vec![],
            };
            let root = tempfile::tempdir().unwrap();
            let batch = Batch::create(root.path(), StoreKind::SourceSnapshot, BatchId::random().unwrap()).unwrap();
            let snapshot =
                Snapshot::from_parts(inventory.clone(), batch, Arc::new(super::super::WindowsTranslator)).unwrap();
            let target = build(NativeSpec {
                inventory,
                contents: snapshot,
                files: vec![],
                file_contents: None,
                sensitivity: crate::Sensitivity::Normal,
            })
            .unwrap();
            let formats = super::super::super::formats();
            let spec = vfile::Spec {
                cloud: None,
                descriptors: super::super::super::files::descriptors(&[]),
                fixed: vec![],
                contents: Arc::new(NoFiles),
                cf_descriptor: formats.file_descriptor,
                cf_contents: formats.file_contents,
            };
            let (data, _) = vfile::build_inner(spec, true, Some(target));
            // SAFETY: this STA owns the object and output buffers.
            let actual = unsafe {
                let enumerator = data.EnumFormatEtc(DATADIR_GET.0 as u32).unwrap();
                let mut entries = [FORMATETC::default(); 16];
                let mut count = 0;
                let _ = enumerator.Next(&mut entries, Some(&mut count));
                entries[..count as usize]
                    .iter()
                    .map(|entry| name(entry.cfFormat).unwrap())
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                actual,
                offered
                    .iter()
                    .map(|name| NativeFormat::WindowsRegistered((*name).into()))
                    .collect::<Vec<_>>()
            );
            drop(data);
            // SAFETY: matches successful STA initialization above.
            unsafe {
                windows::Win32::System::Ole::OleUninitialize();
            }
        })
        .join()
        .unwrap();
    }
    #[test]
    fn failed_native_reads_correlate_stage_and_current_or_replaced_object() {
        use super::super::diagnostics::ObjectState;
        use std::io::Write;
        use std::sync::Mutex;
        #[derive(Clone)]
        struct Capture(Arc<Mutex<Vec<u8>>>);
        impl Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        struct Failing {
            inventory: Inventory,
            stage: u8,
        }
        impl Provider for Failing {
            fn inventory(&self) -> Inventory {
                self.inventory.clone()
            }
            fn translator(&self) -> Arc<dyn Translator> {
                unreachable!()
            }
            fn open(&self, _: RepresentationId) -> io::Result<crate::FileReader> {
                struct FailedReader;
                impl io::Read for FailedReader {
                    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                        // read_to_end retries Interrupted. Use a terminal read
                        // failure here; Superseded is exercised at open below.
                        Err(content::unavailable(Unavailable::ReadFailed))
                    }
                }
                if self.stage == 1 {
                    Err(content::unavailable(Unavailable::Superseded))
                } else {
                    Ok(Box::new(FailedReader))
                }
            }
        }
        let inventory = Inventory {
            id: OfferId {
                clip: ClipId {
                    origin: DeviceId([7; 32]),
                    seq: 8,
                },
                generation: 9,
            },
            platform: ClipPlatform::Windows,
            items: vec![ContentItem {
                virtual_files: false,
                id: ItemId(0),
                file: None,
                representations: vec![Representation {
                    id: RepresentationId(10),
                    format: NativeFormat::WindowsPredefined(13),
                    medium: Medium::HGlobal,
                    aspect: DVASPECT_CONTENT.0,
                    index: -1,
                    standard: Some(StandardFormat::Text),
                    data: RepresentationData::Reader {
                        size: ContentSize::Unknown,
                        hash: None,
                    },
                }],
            }],
            roots: vec![],
        };
        for (stage, end_state) in [
            (1, ObjectState::Current),
            (1, ObjectState::Replaced),
            (2, ObjectState::Replaced),
        ] {
            let make_target = |inventory: Inventory| {
                build(NativeSpec {
                    inventory: inventory.clone(),
                    contents: Arc::new(Failing { inventory, stage }),
                    files: vec![],
                    file_contents: None,
                    sensitivity: crate::Sensitivity::Normal,
                })
                .unwrap()
            };
            let target = make_target(inventory.clone());
            let captured = Capture(Arc::new(Mutex::new(Vec::new())));
            let writer = captured.clone();
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish();
            // tracing-core's single-dispatch fast path computes new callsite
            // interest using the registering thread's default subscriber. A
            // parallel test without this scoped subscriber can therefore cache
            // this WARN as never enabled. Keep two registered dispatches alive
            // so interest is computed across subscribers, while capture stays
            // local to this thread. Dispatch::none() does not register itself.
            let _uncaptured = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new());
            let calls = std::cell::Cell::new(0);
            tracing::subscriber::with_default(subscriber, || {
                let error = target
                    .get(&target.formats()[0], || {
                        let before = calls.get() == 0;
                        calls.set(calls.get() + 1);
                        if !before {
                            // Force a neighbouring test's first use of the WARN
                            // callsite after this read's INFO boundaries, before
                            // this thread emits its own failure diagnostic.
                            std::thread::scope(|scope| {
                                scope
                                    .spawn(|| {
                                        let neighbour = make_target(inventory.clone());
                                        assert_eq!(
                                            neighbour
                                                .get(&neighbour.formats()[0], || Observation {
                                                    object: 0,
                                                    replacement: 0,
                                                    state: ObjectState::Current,
                                                })
                                                .err()
                                                .unwrap()
                                                .code(),
                                            STG_E_READFAULT
                                        );
                                    })
                                    .join()
                                    .unwrap();
                            });
                        }
                        Observation {
                            object: 7,
                            replacement: if before || end_state == ObjectState::Current {
                                0
                            } else {
                                11
                            },
                            state: if before { ObjectState::Current } else { end_state },
                        }
                    })
                    .err()
                    .unwrap();
                assert_eq!(error.code(), STG_E_READFAULT);
            });
            assert_eq!(calls.get(), 2);
            let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
            assert!(
                !log.contains("clipboard_object=0"),
                "captured neighbouring thread: {log}"
            );
            for field in [
                "WARN",
                "clipboard_object=7",
                "clipboard_offer=8",
                "clipboard_generation=9",
                "clipboard_representation=10",
                "clipboard_hresult_class=1",
                "clipboard_state_begin=2",
            ] {
                assert!(log.contains(field), "missing {field}: {log}");
            }
            assert!(log.contains(&format!("clipboard_reason={}", if stage == 1 { 3 } else { 4 })));
            assert!(log.contains(&format!("clipboard_stage={stage}")));
            assert!(log.contains(&format!("clipboard_state={}", end_state as u8)));
            assert!(log.contains(if end_state == ObjectState::Current {
                "clipboard_replacement=0"
            } else {
                "clipboard_replacement=11"
            }));
        }
    }

    #[test]
    fn native_snapshot_republishes_order_and_private_bytes_after_source_release() {
        std::thread::spawn(|| {
            // SAFETY: isolated COM apartment; never publishes to the desktop clipboard.
            unsafe { windows::Win32::System::Ole::OleInitialize(None).unwrap() };
            struct NoFiles;
            impl crate::FileContents for NoFiles {
                fn open(&self, _: usize) -> io::Result<crate::FileReader> {
                    unreachable!()
                }
            }
            let private = NativeFormat::WindowsRegistered("Oink.NativeSnapshot.UnitTest".into());
            let cf = format_id(&private).unwrap();
            let formats = super::super::super::formats();
            let spec = vfile::Spec {
                cloud: None,
                descriptors: super::super::super::files::descriptors(&[]),
                fixed: vec![(cf as u32, vec![0, 255, 0, 128]), (13, vec![0x41, 0, 0, 0])],
                contents: Arc::new(NoFiles),
                cf_descriptor: formats.file_descriptor,
                cf_contents: formats.file_contents,
            };
            let (source, _) = vfile::build(spec, false);
            let dir = tempfile::tempdir().unwrap();
            let id = OfferId {
                clip: ClipId {
                    origin: DeviceId([7; 32]),
                    seq: 7,
                },
                generation: 7,
            };
            let snapshot = super::super::snapshot(&source, dir.path(), id).unwrap();
            let inventory = snapshot.inventory();
            drop(source);
            let target = build(NativeSpec {
                inventory: inventory.clone(),
                contents: snapshot,
                files: Vec::new(),
                file_contents: None,
                sensitivity: crate::Sensitivity::Normal,
            })
            .unwrap();
            let republished = target.formats();
            assert_eq!(
                republished
                    .iter()
                    .map(|f| name(f.cfFormat).unwrap())
                    .collect::<Vec<_>>(),
                inventory
                    .wanted(ClipPlatform::Windows)
                    .map(|r| r.format.clone())
                    .collect::<Vec<_>>()
            );
            for (format, expected) in [(cf, vec![0, 255, 0, 128]), (13, vec![0x41, 0, 0, 0])] {
                let f = republished.iter().find(|f| f.cfFormat == format).unwrap();
                let mut value = target
                    .get(f, || super::diagnostics::Publication::new().observe(Some(false)))
                    .unwrap();
                assert_eq!(medium::serialize(&value, Medium::HGlobal).unwrap(), expected);
                // SAFETY: get returned this owned medium; release exactly once.
                unsafe { ReleaseStgMedium(&mut value) };
            }
            drop(target);
            // SAFETY: all apartment objects have been released.
            unsafe { windows::Win32::System::Ole::OleUninitialize() };
        })
        .join()
        .unwrap();
    }
    #[test]
    fn serialized_file_medium_keeps_the_managed_batch_pinned_until_native_release() {
        std::thread::spawn(|| {
            // SAFETY: isolated STA, no clipboard publication or desktop input.
            unsafe {
                windows::Win32::System::Ole::OleInitialize(None).unwrap();
            }
            let dir = tempfile::tempdir().unwrap();
            let batch = Batch::create(dir.path(), StoreKind::CompletedDelivery, BatchId([42; 16])).unwrap();
            batch.snapshot(9, &mut io::Cursor::new(b"file medium")).unwrap();
            let path = batch.path().to_owned();
            let mut medium = file_medium(batch, RepresentationId(9)).unwrap();
            crate::store::usage(dir.path(), StoreKind::CompletedDelivery, true);
            assert!(path.exists());
            assert_eq!(
                super::super::medium::serialize(&medium, Medium::File).unwrap(),
                b"file medium"
            );
            // SAFETY: one owned STGMEDIUM; its IUnknown releases the cache pin.
            unsafe {
                ReleaseStgMedium(&mut medium);
            }
            crate::store::usage(dir.path(), StoreKind::CompletedDelivery, true);
            assert!(!path.exists());
            // SAFETY: matches successful initialization on this isolated thread.
            unsafe {
                windows::Win32::System::Ole::OleUninitialize();
            }
        })
        .join()
        .unwrap();
    }
}
