//! A browser-style .url output is a bounded generated representation, not a
//! source file or a second received-file backing. Shell serializes the shortcut
//! so Unicode URLs use the platform's native on-disk format.
use super::*;
use std::{io::Read, sync::OnceLock};
use windows::{
    Win32::UI::Shell::{CLSID_InternetShortcut, IUniformResourceLocatorW},
    core::Interface,
};

struct Rendered {
    name: String,
    bytes: Arc<[u8]>,
}
pub(super) struct Shortcut {
    provider: Arc<dyn Provider>,
    inventory: Inventory,
    candidates: Vec<RepresentationId>,
    rendered: OnceLock<Rendered>,
}
impl Shortcut {
    pub fn for_spec(spec: &NativeSpec) -> Option<Arc<dyn crate::FileContents>> {
        if !spec.files.is_empty()
            || !spec.inventory.namespace_roots().is_empty()
            || spec.inventory.items.iter().any(|item| item.file.is_some())
        {
            return None;
        }
        let candidates: Vec<_> = spec
            .inventory
            .wanted(ClipPlatform::Windows)
            .filter(|r| r.standard == Some(StandardFormat::Url))
            .map(|r| r.id)
            .collect();
        (!candidates.is_empty()).then(|| {
            Arc::new(Self {
                provider: spec.contents.clone(),
                inventory: spec.inventory.clone(),
                candidates,
                rendered: OnceLock::new(),
            }) as Arc<dyn crate::FileContents>
        })
    }
    fn rendered(&self) -> io::Result<&Rendered> {
        if let Some(rendered) = self.rendered.get() {
            return Ok(rendered);
        }
        // The descriptor is first rendered on IDataObject's STA, but vfile's
        // IStream metadata/open calls run on MTA RPC threads. On Windows 11,
        // creating IUniformResourceLocatorW there fails with E_NOINTERFACE.
        // Freeze the generated name/bytes/size together at descriptor demand;
        // re-rendering per stream read both requires the wrong apartment and
        // risks changing the advertised file. This creates no source namespace
        // and leaves native URL representations independent. Do not hold a lock
        // across provider/COM calls, which can reenter the STA. Only a successful
        // render is retained; concurrent callers all use the published result.
        let (name, bytes) = self.render()?;
        let _ = self.rendered.set(Rendered {
            name,
            bytes: bytes.into(),
        });
        Ok(self.rendered.get().expect("successful render published"))
    }
    fn render(&self) -> io::Result<(String, Vec<u8>)> {
        let mut error = content::unavailable(Unavailable::Unsupported);
        for id in &self.candidates {
            let result = (|| {
                let rep =
                    content::find(&self.inventory, *id).ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))?;
                let mut reader = self.provider.open(*id)?;
                let bytes = content::url::read(&mut reader)?;
                let link = if self.inventory.platform == ClipPlatform::Windows {
                    content::url::capture(self.provider.as_ref(), &self.inventory, rep, &bytes, false)?
                } else {
                    content::url::Link::decode(&bytes)?
                };
                link.render(ClipPlatform::Mac, "public.url")?; // validate before native serialization
                let title = link.title.as_deref().unwrap_or("Link");
                let mut name = String::new();
                let mut length = 0;
                for ch in title.chars() {
                    length += ch.len_utf16();
                    if length > 240 {
                        break;
                    }
                    name.push(if ch == '/' { '_' } else { ch });
                }
                let name = format!("{}.url", super::super::files::sanitize_component(&name));
                Ok((name, serialize(&link.url)?))
            })();
            match result {
                Ok(value) => return Ok(value),
                Err(e) => error = e,
            }
        }
        Err(error)
    }
}
impl crate::FileContents for Shortcut {
    fn descriptor_entries(&self) -> io::Result<Option<Vec<crate::FileEntry>>> {
        let rendered = self.rendered()?;
        Ok(Some(vec![crate::FileEntry {
            path: rendered.name.clone(),
            size: rendered.bytes.len() as u64,
            is_dir: false,
            modified_ms: None,
            local_path: None,
        }]))
    }
    fn open(&self, index: usize) -> io::Result<crate::FileReader> {
        if index != 0 {
            return Err(content::unavailable(Unavailable::NoSuchItem));
        }
        Ok(Box::new(io::Cursor::new(self.rendered()?.bytes.clone())))
    }
    fn metadata(&self, index: usize) -> io::Result<Option<crate::FileMetadata>> {
        if index != 0 {
            return Err(content::unavailable(Unavailable::NoSuchItem));
        }
        Ok(Some(crate::FileMetadata {
            exec_bits: None,
            size: self.rendered()?.bytes.len() as u64,
            modified_ms: None,
        }))
    }
}
fn serialize(url: &str) -> io::Result<Vec<u8>> {
    struct Com(bool);
    impl Drop for Com {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: balance only this function's successful CoInitializeEx.
                unsafe { CoUninitialize() };
            }
        }
    }
    let failed = |_| content::unavailable(Unavailable::ReadFailed);
    // SAFETY: Shell serialization requires an STA. Balance only successful
    // initialization; no COM pointer leaves this calling thread.
    let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if initialized.is_err() {
        return Err(failed(()));
    }
    let _com = Com(initialized.is_ok());
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("link.url");
    use std::os::windows::ffi::OsStrExt;
    let path_w: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let url_w: Vec<_> = url.encode_utf16().chain(Some(0)).collect();
    // SAFETY: both strings are NUL terminated and the private native object is
    // used only here. Save writes our temporary file; no URL is ever invoked.
    unsafe {
        let link: IUniformResourceLocatorW =
            CoCreateInstance(&CLSID_InternetShortcut, None, CLSCTX_INPROC_SERVER).map_err(|_| failed(()))?;
        link.SetURL(PCWSTR(url_w.as_ptr()), 0).map_err(|_| failed(()))?;
        link.cast::<IPersistFile>()
            .map_err(|_| failed(()))?
            .Save(PCWSTR(path_w.as_ptr()), true)
            .map_err(|_| failed(()))?;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(content::url::MAX_WIRE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > content::url::MAX_WIRE_BYTES {
        return Err(content::unavailable(Unavailable::LimitExceeded));
    }
    Ok(bytes)
}

// Uses the debug-only URL/content fixtures and STA helper.
#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::content::url::tests::{Projected, source};
    #[test]
    fn shortcut_stream_and_sink_survive_drop_grant_transition() {
        after_drop(false);
    }
    #[test]
    fn shortcut_stream_and_sink_survive_drop_grant_transition_via_sink() {
        after_drop(true);
    }
    fn after_drop(via_sink: bool) {
        super::super::tests::sta(move || {
            use crate::content::{Registry, Remote, TransferPhase, namespace::Namespace};
            use oink_foundation::content::Holder;
            use std::sync::atomic::{AtomicUsize, Ordering};
            use windows::Win32::UI::Shell::IDataObjectAsyncCapability;
            struct Observed {
                remote: Remote,
                opens: AtomicUsize,
            }
            impl Provider for Observed {
                fn inventory(&self) -> Inventory {
                    self.remote.inventory()
                }
                fn translator(&self) -> Arc<dyn Translator> {
                    self.remote.translator()
                }
                fn open(&self, id: RepresentationId) -> io::Result<crate::FileReader> {
                    self.opens.fetch_add(1, Ordering::SeqCst);
                    Provider::open(&self.remote, id)
                }
            }
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let _entered = runtime.enter();
            let source = Arc::new(source(
                ClipPlatform::Mac,
                &[
                    ("public.url", b"https://example.invalid/path?a=1&b=2".to_vec()),
                    ("public.url-name", "A title 猫".as_bytes().to_vec()),
                ],
            ));
            let registry = Arc::new(Registry::default());
            let lease = registry.register(source.clone()).unwrap();
            let (link, _requests) = crate::content::tests::native_test_link(registry);
            let directory = tempfile::tempdir().unwrap();
            let holder = Holder {
                device: DeviceId([2; 32]),
                epoch: 7,
            };
            let remote = Remote::new(
                directory.path(),
                BatchId([17; 16]),
                source.inventory(),
                ClipPlatform::Windows,
                link,
                Demand::Hover(holder),
            )
            .unwrap();
            let provider = Arc::new(Observed {
                remote: remote.clone(),
                opens: AtomicUsize::new(0),
            });
            let native = super::super::target::build_drag(NativeSpec {
                inventory: source.inventory(),
                contents: provider.clone(),
                files: vec![],
                file_contents: None,
                sensitivity: crate::Sensitivity::Normal,
            })
            .unwrap();
            let shortcut = native.shortcut.clone().unwrap();
            let f = super::super::super::formats();
            let (data, shared) = super::super::super::vfile::build_inner(
                super::super::super::vfile::Spec {
                    cloud: None,
                    descriptors: super::super::super::files::descriptors(&[]),
                    fixed: vec![],
                    contents: shortcut,
                    cf_descriptor: f.file_descriptor,
                    cf_contents: f.file_contents,
                },
                false,
                Some(native),
            );
            shared.host_drag_images();
            // SAFETY: this STA owns the private data object and every medium;
            // no clipboard publication or desktop operation occurs.
            unsafe {
                let descriptor_format = FORMATETC {
                    cfFormat: f.file_descriptor as u16,
                    dwAspect: DVASPECT_CONTENT.0,
                    tymed: TYMED_HGLOBAL.0 as u32,
                    lindex: -1,
                    ..Default::default()
                };
                assert!(data.QueryGetData(&descriptor_format).is_ok());
                assert_eq!(provider.opens.load(Ordering::SeqCst), 0, "format probes stay lazy");
                let mut descriptor = data.GetData(&descriptor_format).unwrap();
                let descriptor_bytes = super::super::medium::serialize(&descriptor, Medium::HGlobal).unwrap();
                ReleaseStgMedium(&mut descriptor);
                let expected_size = u32::from_le_bytes(descriptor_bytes[72..76].try_into().unwrap()) as usize;
                assert!(expected_size > 0);
                let contents_format = FORMATETC {
                    cfFormat: f.file_contents as u16,
                    tymed: TYMED_ISTREAM.0 as u32,
                    lindex: 0,
                    ..descriptor_format
                };
                let mut medium = data.GetData(&contents_format).unwrap();
                let stream = medium.u.pstm.as_ref().unwrap();
                let opens = provider.opens.load(Ordering::SeqCst);
                let async_data: IDataObjectAsyncCapability = data.cast().unwrap();
                async_data.StartOperation(None).unwrap();
                assert!(shared.drag_dropped(
                    windows::Win32::Foundation::DRAGDROP_S_DROP,
                    windows::Win32::System::Ole::DROPEFFECT_COPY
                ));
                shared.drag_ended(true, false);
                remote.set_demand(Demand::Drop(holder));
                assert!(remote.start_delivery());
                runtime.block_on(async {
                    let mut progress = remote.progress();
                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        progress.wait_for(|p| p.phase == TransferPhase::Stored && p.source_acknowledged),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                });
                lease.cancel();
                if via_sink {
                    // Independent indexed FileContents stream, as used by the
                    // native sink when capturing the dropped virtual file.
                    let sink = super::super::virtual_files::Virtual::metadata(&data).unwrap().unwrap();
                    sink.snapshot(&data, directory.path()).unwrap();
                    let file = sink.roots().remove(0);
                    let bytes = content::read_all(&mut sink.open(file.id, file.version, 0).unwrap()).unwrap();
                    assert_eq!(bytes.len(), expected_size);
                } else {
                    let mut bytes = vec![0; expected_size + 1];
                    let mut read = 0;
                    let hr = stream.Read(bytes.as_mut_ptr().cast(), bytes.len() as u32, Some(&mut read));
                    assert!(hr.is_ok(), "FileContents read after accepted drop failed: {hr:?}");
                    assert_eq!(read as usize, expected_size);
                    bytes.truncate(read as usize);
                    let mut stat = STATSTG::default();
                    stream.Stat(&mut stat, STATFLAG_NONAME).unwrap();
                    assert_eq!(stat.cbSize, expected_size as u64);
                    stream.Seek(0, STREAM_SEEK_SET, None).unwrap();
                    let mut repeated = vec![0; expected_size];
                    assert!(
                        stream
                            .Read(repeated.as_mut_ptr().cast(), repeated.len() as u32, Some(&mut read))
                            .is_ok()
                    );
                    assert_eq!(repeated, bytes);
                }
                ReleaseStgMedium(&mut medium);
                assert_eq!(
                    provider.opens.load(Ordering::SeqCst),
                    opens,
                    "published generated bytes must not reopen the source URL on stream RPC threads"
                );
                async_data
                    .EndOperation(windows::Win32::Foundation::S_OK, None, 1)
                    .unwrap();
            }
        });
    }
    #[test]
    fn browser_shortcut_roundtrips_unicode_url_through_native_shell_without_opening_it() {
        super::super::tests::sta(|| {
            let url = "https://example.invalid/猫?q=é&x=1";
            let source = source(
                ClipPlatform::Mac,
                &[
                    ("public.url", url.as_bytes().to_vec()),
                    ("public.url-name", b"CON/../Title:*".to_vec()),
                ],
            );
            let native = super::super::target::build(NativeSpec {
                inventory: source.inventory(),
                contents: Arc::new(Projected(source, ClipPlatform::Windows)),
                files: vec![],
                file_contents: None,
                sensitivity: crate::Sensitivity::Normal,
            })
            .unwrap();
            let shortcut = native.shortcut.clone().unwrap();
            let f = super::super::super::formats();
            let (data, _) = super::super::super::vfile::build_inner(
                super::super::super::vfile::Spec {
                    cloud: None,
                    descriptors: super::super::super::files::descriptors(&[]),
                    fixed: vec![],
                    contents: shortcut.clone(),
                    cf_descriptor: f.file_descriptor,
                    cf_contents: f.file_contents,
                },
                false,
                Some(native),
            );
            let format = |cf, medium| FORMATETC {
                cfFormat: cf,
                dwAspect: DVASPECT_CONTENT.0,
                tymed: medium,
                lindex: -1,
                ..Default::default()
            };
            // SAFETY: private IDataObject in this STA; no system clipboard access.
            unsafe {
                assert!(
                    data.QueryGetData(&format(f.file_descriptor as u16, TYMED_HGLOBAL.0 as u32))
                        .is_ok()
                );
                assert!(
                    data.QueryGetData(&format(f.file_contents as u16, TYMED_ISTREAM.0 as u32))
                        .is_ok()
                );
                assert!(data.QueryGetData(&format(15, TYMED_HGLOBAL.0 as u32)).is_err());
                let mut medium = data
                    .GetData(&format(f.file_descriptor as u16, TYMED_HGLOBAL.0 as u32))
                    .unwrap();
                let bytes = super::super::medium::serialize(&medium, Medium::HGlobal).unwrap();
                ReleaseStgMedium(&mut medium);
                assert_eq!(u32::from_le_bytes(bytes[..4].try_into().unwrap()), 1);
            }
            // Explorer and the sink both consume FileContents through the MTA
            // IStream proxy, rather than calling FileContents::open on this STA.
            // SAFETY: private data object and owned media in the initialized STA.
            unsafe {
                let mut contents_format = format(f.file_contents as u16, TYMED_ISTREAM.0 as u32);
                contents_format.lindex = 0;
                let mut medium = data.GetData(&contents_format).unwrap();
                let stream = medium.u.pstm.as_ref().unwrap();
                let mut bytes = [0u8; 4096];
                let mut read = 0;
                let hr = stream.Read(bytes.as_mut_ptr().cast(), bytes.len() as u32, Some(&mut read));
                assert!(hr.is_ok(), "native FileContents IStream failed: {hr:?}");
                assert!(read > 0);
                ReleaseStgMedium(&mut medium);
            }
            let entries = shortcut.descriptor_entries().unwrap().unwrap();
            assert!(entries[0].path.ends_with(".url"));
            assert!(!entries[0].path.contains(['/', '\\', ':', '*']));
            let mut bytes = Vec::new();
            shortcut.open(0).unwrap().read_to_end(&mut bytes).unwrap();
            assert_eq!(entries[0].size, bytes.len() as u64);
            assert!(shortcut.open(1).is_err());
            let temp = tempfile::tempdir().unwrap();
            let file = temp.path().join("test.url");
            std::fs::write(&file, bytes).unwrap();
            use std::os::windows::ffi::OsStrExt;
            let wide: Vec<_> = file.as_os_str().encode_wide().chain(Some(0)).collect();
            // SAFETY: Shell only loads our temporary file and returns its URL;
            // it is never invoked. GetURL transfers one CoTaskMem allocation.
            unsafe {
                let link: IUniformResourceLocatorW =
                    CoCreateInstance(&CLSID_InternetShortcut, None, CLSCTX_INPROC_SERVER).unwrap();
                link.cast::<IPersistFile>()
                    .unwrap()
                    .Load(PCWSTR(wide.as_ptr()), STGM_READ)
                    .unwrap();
                let result = link.GetURL().unwrap();
                let actual = result.to_string().unwrap();
                CoTaskMemFree(Some(result.0.cast()));
                assert_eq!(actual, url);
            }
        });
    }
}
