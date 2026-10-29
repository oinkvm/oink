//! Synthetic native contracts for the fake backend, with live copy revocation.
use super::*;
use oink_clipboard::{
    content::{
        self, Provider, Translator,
        namespace::{self, Namespace},
    },
    model::*,
};
use oink_foundation::{DeviceId, clipboard::ClipPlatform};
use std::{
    collections::HashMap,
    io::{self, Read},
    path::Path,
};
fn platform() -> ClipPlatform {
    if cfg!(target_os = "macos") {
        ClipPlatform::Mac
    } else {
        ClipPlatform::Windows
    }
}
fn format(name: &str) -> NativeFormat {
    match platform() {
        ClipPlatform::Mac => NativeFormat::Mac(name.into()),
        _ => NativeFormat::WindowsRegistered(name.into()),
    }
}
fn name(format: &NativeFormat) -> String {
    match format {
        NativeFormat::Mac(n) | NativeFormat::WindowsRegistered(n) | NativeFormat::Unknown(n) => n.clone(),
        NativeFormat::WindowsPredefined(n) => format!("win-cf:{n}"),
    }
}
fn error<T>(_: T) -> ClipboardError {
    ClipboardError::Unsupported
}
struct Native {
    inventory: Inventory,
    bytes: HashMap<RepresentationId, Vec<u8>>,
    published: Option<Arc<dyn Provider>>,
    inner: Arc<Mutex<Inner>>,
    count: u64,
    files: Option<Arc<dyn Namespace>>,
}
fn check(inner: &Mutex<Inner>, count: u64) -> io::Result<()> {
    if inner.lock().unwrap().count == count {
        Ok(())
    } else {
        Err(content::unavailable(Unavailable::Superseded))
    }
}
struct Checked {
    inner: Arc<Mutex<Inner>>,
    count: u64,
    reader: oink_clipboard::FileReader,
}
impl Read for Checked {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        check(&self.inner, self.count)?;
        let n = self.reader.read(out)?;
        check(&self.inner, self.count)?;
        Ok(n)
    }
}
struct Portable;
fn decoded(rep: &Representation, bytes: Vec<u8>) -> io::Result<ClipData> {
    Ok(match rep.standard {
        Some(StandardFormat::Text) => ClipData::Text(String::from_utf8(bytes).map_err(io::Error::other)?),
        Some(StandardFormat::Html) => ClipData::Html(String::from_utf8(bytes).map_err(io::Error::other)?),
        Some(StandardFormat::Rtf) => ClipData::Rtf(bytes),
        Some(StandardFormat::Png) => ClipData::Png(bytes),
        _ => ClipData::Custom {
            platform: rep.format.platform(),
            name: name(&rep.format),
            bytes,
        },
    })
}
impl Translator for Portable {
    fn standard(&self, rep: &Representation, bytes: &[u8]) -> io::Result<Vec<ClipData>> {
        Ok(vec![decoded(rep, bytes.to_vec())?])
    }
}
impl Provider for Native {
    fn inventory(&self) -> Inventory {
        self.inventory.clone()
    }
    fn open(&self, id: RepresentationId) -> io::Result<oink_clipboard::FileReader> {
        let guard = self.reader_guard(id)?;
        guard()?;
        let reader: oink_clipboard::FileReader = if let Some(published) = &self.published {
            published.open(id)?
        } else {
            Box::new(io::Cursor::new(
                self.bytes.get(&id).ok_or(io::ErrorKind::NotFound)?.clone(),
            ))
        };
        Ok(Box::new(Checked {
            inner: self.inner.clone(),
            count: self.count,
            reader,
        }))
    }
    fn reader_guard(&self, _: RepresentationId) -> io::Result<content::ReaderGuard> {
        let inner = self.inner.clone();
        let count = self.count;
        Ok(Arc::new(move || check(&inner, count)))
    }
    fn translator(&self) -> Arc<dyn Translator> {
        Arc::new(Portable)
    }
    fn namespace(&self) -> Option<Arc<dyn Namespace>> {
        self.files.clone()
    }
}
struct CurrentFiles {
    inner: Arc<Mutex<Inner>>,
    count: u64,
    files: Arc<dyn Namespace>,
    gate: Option<Arc<super::file_gate::State>>,
}
impl Namespace for CurrentFiles {
    fn source_roots(&self) -> io::Result<Vec<content::grants::SourceRoot>> {
        check(&self.inner, self.count)?;
        let roots = self.files.source_roots()?;
        check(&self.inner, self.count)?;
        Ok(roots)
    }
    fn roots(&self) -> Vec<FileNode> {
        self.files.roots()
    }
    fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
        check(&self.inner, self.count)?;
        let page = self.files.list(node, cursor)?;
        check(&self.inner, self.count)?;
        Ok(page)
    }
    fn select(&self, node: NodeId) -> io::Result<FileNode> {
        check(&self.inner, self.count)?;
        let node = self.files.select(node)?;
        check(&self.inner, self.count)?;
        Ok(node)
    }
    fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<oink_clipboard::FileReader> {
        check(&self.inner, self.count)?;
        let reader = self.files.open(node, version, offset)?;
        let reader = if let Some(gate) = &self.gate {
            super::file_gate::wrap(reader, gate.clone(), offset)
        } else {
            reader
        };
        check(&self.inner, self.count)?;
        Ok(Box::new(Checked {
            inner: self.inner.clone(),
            count: self.count,
            reader,
        }))
    }
}
pub(super) fn capture(fake: &FakeClipboard, _data: &Path) -> Result<oink_clipboard::CapturedClipboard, ClipboardError> {
    let snapshot = fake.read()?;
    let count = snapshot.change_count;
    if let Some(spec) = fake.native_spec() {
        let files = spec.contents.namespace().map(|files| {
            Arc::new(CurrentFiles {
                inner: fake.inner.clone(),
                count,
                files,
                gate: fake.lock().file_gate.clone(),
            }) as Arc<dyn Namespace>
        });
        return Ok(oink_clipboard::CapturedClipboard {
            snapshot,
            content: Some(Arc::new(Native {
                inventory: spec.inventory,
                bytes: HashMap::new(),
                published: Some(spec.contents),
                inner: fake.inner.clone(),
                count,
                files,
            })),
        });
    }
    let data = oink_clipboard::formats::for_platform(&snapshot.items, platform(), snapshot.sensitivity);
    let mut inventory = Inventory {
        id: OfferId {
            clip: ClipId {
                origin: DeviceId([0; 32]),
                seq: count,
            },
            generation: count,
        },
        platform: platform(),
        items: vec![],
        roots: vec![],
    };
    let mut bytes = HashMap::new();
    let mut roots = vec![];
    for (index, data) in data.iter().enumerate() {
        let id = RepresentationId((index + 1) as u32);
        let (format, standard, payload) = match data {
            ClipData::Text(text) => (
                format("org.oink.test.utf8-text"),
                Some(StandardFormat::Text),
                text.as_bytes().to_vec(),
            ),
            ClipData::Html(text) => (
                format("org.oink.test.utf8-html"),
                Some(StandardFormat::Html),
                text.as_bytes().to_vec(),
            ),
            ClipData::Rtf(bytes) => (format("org.oink.test.rtf"), Some(StandardFormat::Rtf), bytes.clone()),
            ClipData::Png(bytes) => (format("org.oink.test.png"), Some(StandardFormat::Png), bytes.clone()),
            ClipData::Custom { platform, name, bytes } => (
                match platform {
                    ClipPlatform::Mac => NativeFormat::Mac(name.clone()),
                    _ => NativeFormat::WindowsRegistered(name.clone()),
                },
                oink_clipboard::formats::is_marker(*platform, name).then_some(StandardFormat::Privacy),
                bytes.clone(),
            ),
            ClipData::Files(files) => {
                for entry in files {
                    if entry.path.contains('/') && !Path::new(&entry.path).is_absolute() {
                        continue;
                    }
                    let path = entry.local_path.clone().unwrap_or_else(|| entry.path.clone().into());
                    roots.push((NodeId((roots.len() + 1) as u64), path));
                }
                continue;
            }
            ClipData::FileRoots(nodes) => {
                inventory.roots.extend(nodes.clone());
                continue;
            }
            ClipData::Native { platform: source, item } => {
                if *source != inventory.platform {
                    return Err(ClipboardError::Unsupported);
                }
                for rep in &item.representations {
                    if let RepresentationData::Inline(value) = &rep.data {
                        bytes.insert(rep.id, value.clone());
                    }
                }
                inventory.items.push(item.clone());
                continue;
            }
        };
        let size = payload.len() as u64;
        bytes.insert(id, payload);
        inventory.items.push(ContentItem {
            virtual_files: false,
            id: ItemId(id.0),
            file: None,
            representations: vec![Representation {
                id,
                format,
                medium: Medium::Bytes,
                aspect: 1,
                index: -1,
                standard,
                data: RepresentationData::Reader {
                    size: if fake.lock().unknown_representation_sizes {
                        ContentSize::Unknown
                    } else {
                        ContentSize::Known(size)
                    },
                },
            }],
        });
    }
    let files = if roots.is_empty() {
        None
    } else {
        let files = namespace::Local::new(roots).map_err(error)?;
        inventory.roots = files.roots();
        Some(Arc::new(CurrentFiles {
            inner: fake.inner.clone(),
            count,
            files,
            gate: fake.lock().file_gate.clone(),
        }) as Arc<dyn Namespace>)
    };
    inventory.validate().map_err(error)?;
    check(&fake.inner, count).map_err(error)?;
    let provider = Arc::new(Native {
        inventory: inventory.clone(),
        bytes,
        published: None,
        inner: fake.inner.clone(),
        count,
        files,
    });
    Ok(oink_clipboard::CapturedClipboard {
        snapshot: ClipSnapshot {
            items: content::items(&inventory),
            ..snapshot
        },
        content: Some(provider),
    })
}
pub(super) fn write(fake: &FakeClipboard, spec: oink_clipboard::NativeSpec) -> Result<u64, ClipboardError> {
    write_guarded(fake, spec, Arc::new(|| Ok(())))
}
pub(super) fn write_guarded(
    fake: &FakeClipboard,
    spec: oink_clipboard::NativeSpec,
    guard: content::ReaderGuard,
) -> Result<u64, ClipboardError> {
    if fake.lock().deferred_native {
        guard().map_err(|_| ClipboardError::Busy)?;
        let count = fake.set(content::items(&spec.inventory), spec.sensitivity);
        fake.lock().native = Some(spec);
        return Ok(count);
    }
    if !spec.files.is_empty() && spec.file_contents.is_some() && !fake.lock().virtual_files {
        return Err(ClipboardError::FileProviderUnavailable);
    }
    let mut items = vec![];
    for rep in spec.inventory.wanted(platform()) {
        if matches!(rep.data, RepresentationData::File(_) | RepresentationData::Promise(..)) {
            continue;
        }
        let mut bytes = vec![];
        spec.contents
            .open(rep.id)
            .map_err(error)?
            .read_to_end(&mut bytes)
            .map_err(error)?;
        items.push(decoded(rep, bytes).map_err(error)?);
    }
    if !spec.files.is_empty() {
        items.push(ClipData::Files(spec.files));
    }
    guard().map_err(|_| ClipboardError::Busy)?;
    let count = fake.set(items, spec.sensitivity);
    fake.lock().contents = spec.file_contents;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_native_sizes_require_demand_but_preserve_reader_bytes() {
        let fake = FakeClipboard::new();
        fake.set_unknown_representation_sizes(true);
        fake.copy(vec![ClipData::Text("lazy".into())], Sensitivity::Normal);
        let provider = fake.capture(Path::new("unused")).unwrap().content.unwrap();
        let inventory = provider.inventory();
        assert!(!inventory.automatic_pull(platform()));
        let mut bytes = Vec::new();
        provider
            .open(inventory.items[0].representations[0].id)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"lazy");
    }

    #[test]
    fn native_capture_is_metadata_and_open_readers_retire_with_the_copy() {
        let fake = FakeClipboard::new();
        fake.copy(vec![ClipData::Text("native fixture".into())], Sensitivity::Normal);
        let capture = fake.capture(Path::new("unused")).unwrap();
        let provider = capture.content.unwrap();
        let inventory = provider.inventory();
        assert!(matches!(
            inventory.items[0].representations[0].data,
            RepresentationData::Reader { .. }
        ));
        let mut reader = provider.open(inventory.items[0].representations[0].id).unwrap();
        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        fake.copy(vec![ClipData::Text("new copy".into())], Sensitivity::Normal);
        assert_eq!(
            content::reason(&reader.read(&mut byte).unwrap_err()),
            Unavailable::Superseded
        );
    }
    #[test]
    fn deferred_native_publication_can_be_recaptured_and_lazily_read() {
        let source = FakeClipboard::new();
        let target = FakeClipboard::new();
        source.copy(vec![ClipData::Text("lazy".into())], Sensitivity::Normal);
        let provider = source.capture(Path::new("unused")).unwrap().content.unwrap();
        target.set_deferred_native(true);
        target
            .write_native(oink_clipboard::NativeSpec {
                inventory: provider.inventory(),
                contents: provider,
                files: vec![],
                file_contents: None,
                sensitivity: Sensitivity::Normal,
            })
            .unwrap();
        assert!(target.native_spec().is_some());
        let recaptured = target.capture(Path::new("unused")).unwrap().content.unwrap();
        let mut bytes = Vec::new();
        recaptured
            .open(RepresentationId(1))
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"lazy");
        target.copy(vec![ClipData::Text("physical".into())], Sensitivity::Normal);
        assert!(recaptured.open(RepresentationId(1)).is_err());
    }
    #[test]
    fn guarded_publication_rechecks_after_payload_and_preserves_the_local_copy() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Observed {
            provider: Arc<dyn Provider>,
            opened: Arc<AtomicBool>,
        }
        impl Provider for Observed {
            fn inventory(&self) -> Inventory {
                self.provider.inventory()
            }
            fn open(&self, id: RepresentationId) -> io::Result<oink_clipboard::FileReader> {
                self.opened.store(true, Ordering::Release);
                self.provider.open(id)
            }
            fn translator(&self) -> Arc<dyn Translator> {
                self.provider.translator()
            }
        }
        let source = FakeClipboard::new();
        let target = FakeClipboard::new();
        source.copy(vec![ClipData::Text("remote".into())], Sensitivity::Normal);
        target.copy(vec![ClipData::Text("local".into())], Sensitivity::Normal);
        let provider = source.capture(Path::new("unused")).unwrap().content.unwrap();
        let opened = Arc::new(AtomicBool::new(false));
        let read = opened.clone();
        let result = target.write_native_guarded(
            oink_clipboard::NativeSpec {
                inventory: provider.inventory(),
                contents: Arc::new(Observed { provider, opened }),
                files: vec![],
                file_contents: None,
                sensitivity: Sensitivity::Normal,
            },
            Arc::new(move || {
                assert!(
                    read.load(Ordering::Acquire),
                    "authority is checked after native payload preparation"
                );
                Err(io::ErrorKind::PermissionDenied.into())
            }),
        );
        assert!(matches!(result, Err(ClipboardError::Busy)));
        assert_eq!(target.items(), vec![ClipData::Text("local".into())]);
    }
    #[test]
    fn native_write_reads_the_native_contract_and_uses_the_write_counter() {
        let source = FakeClipboard::new();
        let target = FakeClipboard::new();
        source.copy(vec![ClipData::Text("native fixture".into())], Sensitivity::Normal);
        let capture = source.capture(Path::new("unused")).unwrap();
        let provider = capture.content.unwrap();
        let count = target
            .write_native(oink_clipboard::NativeSpec {
                inventory: provider.inventory(),
                contents: provider,
                files: vec![],
                file_contents: None,
                sensitivity: Sensitivity::Normal,
            })
            .unwrap();
        assert_eq!(target.items(), source.items());
        assert_eq!(target.change_count(), count);
    }
}
