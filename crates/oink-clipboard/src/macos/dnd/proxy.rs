//! Target-local native identities for source promises. The original item/type
//! order remains intact; only process-bound fulfilment controls are reconstructed.
use super::*;
use crate::content::{self, namespace::Namespace};
use crate::model::{FileNode, NativeFormat, NodeId, NodeKind, RepresentationData, Unavailable};
use objc2_app_kit::NSPasteboardItemDataProvider;
#[cfg(all(test, debug_assertions))]
use oink_foundation::content::OfferId;
use std::{
    fs,
    io::{self, Write},
};

pub(super) struct Proxy {
    files: Arc<dyn Namespace>,
    anchor: NodeId,
    names: Mutex<Option<Vec<FileNode>>>,
}
impl Proxy {
    fn entries(&self) -> io::Result<Vec<FileNode>> {
        let mut cached = self.names.lock().unwrap();
        if let Some(names) = cached.as_ref() {
            return Ok(names.clone());
        }
        let mut names = Vec::new();
        let mut cursor = None;
        loop {
            // Remote's promise container rejects Hover without receiving at the
            // source; Drop enters the existing authenticated whole-delivery path.
            let page = self.files.list(self.anchor, cursor)?;
            names.extend(page.entries);
            if names.len() > crate::model::MAX_DISCOVERED_NODES {
                return Err(content::unavailable(Unavailable::LimitExceeded));
            }
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        if names.is_empty() {
            return Err(content::unavailable(Unavailable::InvalidData));
        }
        *cached = Some(names.clone());
        Ok(names)
    }
    pub(super) fn receive(&self, destination: &Path) -> io::Result<Vec<String>> {
        let mut written = Vec::new();
        for entry in self.entries()? {
            let path = unique_path(&destination.join(&entry.name));
            copy_node(self.files.as_ref(), &entry, &path, 0)?;
            written.push(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .ok_or(io::ErrorKind::InvalidData)?
                    .to_owned(),
            );
        }
        Ok(written)
    }
}
fn copy_node(files: &dyn Namespace, entry: &FileNode, path: &Path, depth: usize) -> io::Result<()> {
    if depth > 256 || !entry.valid() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    if entry.kind == NodeKind::Directory {
        fs::create_dir(path)?;
        let result = (|| {
            let mut cursor = None;
            loop {
                let page = files.list(entry.id, cursor)?;
                for child in page.entries {
                    copy_node(files, &child, &path.join(&child.name), depth + 1)?;
                }
                cursor = page.next;
                if cursor.is_none() {
                    break;
                }
            }
            crate::store::sync_directory(path)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(path);
        }
        result
    } else {
        let selected = files.select(entry.id)?;
        let mut input = files.open(selected.id, selected.version, 0)?;
        let mut output = fs::OpenOptions::new().write(true).create_new(true).open(path)?;
        let result = (|| {
            let size = io::copy(&mut input, &mut output)?;
            if selected.size != crate::model::ContentSize::Known(size) {
                return Err(io::ErrorKind::InvalidData.into());
            }
            output.flush()?;
            output.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(path);
        }
        result
    }
}
struct ModernIvars {
    extra: Retained<ProtocolObject<dyn NSPasteboardWriting>>,
    controls: Vec<String>,
    _delegate: Retained<Delegate>,
}
define_class!(
    #[unsafe(super(NSFilePromiseProvider))]
    #[name = "OinkHeldPromiseProxy"]
    #[ivars = ModernIvars]
    struct Modern;
    unsafe impl NSObjectProtocol for Modern {}
    unsafe impl NSPasteboardWriting for Modern {
        #[unsafe(method_id(writableTypesForPasteboard:))]
        fn types(&self, pb: &NSPasteboard) -> Retained<NSArray<NSString>> {
            self.ivars().extra.writableTypesForPasteboard(pb)
        }
        #[unsafe(method(writingOptionsForType:pasteboard:))]
        fn options(&self, ty: &NSString, pb: &NSPasteboard) -> NSPasteboardWritingOptions {
            if self.ivars().controls.contains(&ty.to_string()) {
                // SAFETY: same inherited NSPasteboardWriting signature.
                unsafe { msg_send![super(self), writingOptionsForType: ty, pasteboard: pb] }
            } else {
                self.ivars().extra.writingOptionsForType_pasteboard(ty, pb)
            }
        }
        #[unsafe(method_id(pasteboardPropertyListForType:))]
        fn property(&self, ty: &NSString) -> Option<Retained<AnyObject>> {
            if self.ivars().controls.contains(&ty.to_string()) {
                // SAFETY: use the new provider's local IPC identity, never source bytes.
                unsafe { msg_send![super(self), pasteboardPropertyListForType: ty] }
            } else {
                self.ivars().extra.pasteboardPropertyListForType(ty)
            }
        }
    }
);
struct DelegateIvars {
    proxy: Arc<Proxy>,
    queue: Retained<NSOperationQueue>,
}
define_class!(
    #[unsafe(super(NSObject))]
    #[name = "OinkHeldPromiseDelegate"]
    #[ivars = DelegateIvars]
    struct Delegate;
    unsafe impl NSObjectProtocol for Delegate {}
    unsafe impl NSFilePromiseProviderDelegate for Delegate {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn name(&self, _provider: &NSFilePromiseProvider, _ty: &NSString) -> Retained<NSString> {
            match self.ivars().proxy.entries() {
                Ok(names) if names.len() == 1 => NSString::from_str(&names[0].name),
                // An empty name is an explicit native failure, not a fabricated
                // filename which another application might save permanently.
                _ => {
                    tracing::warn!("native promise name unavailable");
                    NSString::from_str("")
                }
            }
        }
        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write(&self, _provider: &NSFilePromiseProvider, url: &NSURL, done: &block2::DynBlock<dyn Fn(*mut NSError)>) {
            let result = (|| {
                let names = self.ivars().proxy.entries()?;
                if names.len() != 1 {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                let path = url
                    .path()
                    .map(|path| PathBuf::from(path.to_string()))
                    .ok_or(io::ErrorKind::InvalidData)?;
                copy_node(self.ivars().proxy.files.as_ref(), &names[0], &path, 0)
            })();
            if result.is_ok() {
                done.call((std::ptr::null_mut(),));
            } else {
                tracing::warn!("native promise delivery failed");
                // SAFETY: standard NSError with no business-data userInfo.
                let error = unsafe { NSError::errorWithDomain_code_userInfo(NSCocoaErrorDomain, 512, None) };
                done.call((Retained::as_ptr(&error) as *mut NSError,));
            }
        }
        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn queue(&self, _provider: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
            self.ivars().queue.clone()
        }
    }
);
struct LegacyIvars {
    extra: Retained<ProtocolObject<dyn NSPasteboardWriting>>,
    proxy: Arc<Proxy>,
    values: Vec<(String, Vec<u8>)>,
}
define_class!(
    #[unsafe(super(NSObject))]
    #[name = "OinkHeldLegacyPromiseData"]
    #[ivars = LegacyIvars]
    struct Legacy;
    unsafe impl NSObjectProtocol for Legacy {}
    unsafe impl NSPasteboardItemDataProvider for Legacy {
        #[unsafe(method(pasteboard:item:provideDataForType:))]
        fn provide(&self, board: Option<&NSPasteboard>, item: &NSPasteboardItem, ty: &NSString) {
            let name = ty.to_string();
            if let Some((_, bytes)) = self.ivars().values.iter().find(|(name, _)| name == &ty.to_string()) {
                if name == "NSFilesPromisePboardType" || name == "Apple files promise pasteboard type" {
                    item.setData_forType(&NSData::with_bytes(bytes), ty);
                    return;
                }
                let result = (|| {
                    let destination = board
                        .and_then(|board| board.stringForType(&NSString::from_str("com.apple.pastelocation")))
                        .and_then(|value| NSURL::URLWithString(&value))
                        .and_then(|url| url.filePathURL())
                        .and_then(|url| url.path())
                        .map(|path| PathBuf::from(path.to_string()))
                        .ok_or(io::ErrorKind::InvalidData)?;
                    let names = self.ivars().proxy.receive(&destination)?;
                    if names.len() != 1 {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                    Ok::<_, io::Error>(super::super::pasteboard::file_url_string(&destination.join(&names[0])))
                })();
                match result {
                    Ok(url) => {
                        item.setString_forType(&NSString::from_str(&url), ty);
                    }
                    Err(_) => tracing::warn!("native legacy promise delivery failed"),
                }
            } else if let Some(value) = self.ivars().extra.pasteboardPropertyListForType(ty) {
                // NSData is already the native byte representation. Passing it
                // through setPropertyList wraps it in a binary plist and changes
                // every companion's bytes (including image encodings).
                if let Some(bytes) = value.downcast_ref::<NSData>() {
                    item.setData_forType(bytes, ty);
                } else if let Some(text) = value.downcast_ref::<NSString>() {
                    item.setString_forType(text, ty);
                } else {
                    // SAFETY: forwarding an actual native property-list value.
                    unsafe {
                        item.setPropertyList_forType(&value, ty);
                    }
                }
            }
        }
    }
);

type Writers = Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>>;
pub(super) fn wrap(
    spec: &crate::NativeSpec,
    writers: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>>,
) -> io::Result<(Writers, Vec<Arc<Proxy>>)> {
    let mut output = Vec::new();
    let mut proxies = Vec::new();
    let wanted: std::collections::HashSet<_> = spec
        .inventory
        .wanted(oink_foundation::clipboard::ClipPlatform::Mac)
        .map(|rep| rep.id)
        .collect();
    let items = spec
        .inventory
        .items
        .iter()
        .filter(|item| item.representations.iter().any(|rep| wanted.contains(&rep.id)));
    for (original, extra) in items.zip(writers) {
        let Some(anchor) = original.representations.iter().find_map(|rep| match rep.data {
            RepresentationData::Promise(node, _) => Some(node),
            _ => None,
        }) else {
            output.push(extra);
            continue;
        };
        let files = spec.contents.namespace().ok_or(io::ErrorKind::InvalidData)?;
        let proxy = Arc::new(Proxy {
            files,
            anchor,
            names: Mutex::new(None),
        });
        let controls: Vec<_> = original
            .representations
            .iter()
            .filter_map(|rep| {
                if let (NativeFormat::Mac(name), RepresentationData::Promise(_, bytes)) = (&rep.format, &rep.data) {
                    Some((name.clone(), bytes.clone()))
                } else {
                    None
                }
            })
            .collect();
        if let Some((_, ty)) = controls
            .iter()
            .find(|(name, _)| name == "com.apple.NSFilePromiseItemMetaData")
        {
            let ty = std::str::from_utf8(ty)
                .ok()
                .filter(|ty| !ty.is_empty())
                .ok_or(io::ErrorKind::InvalidData)?;
            let queue = NSOperationQueue::new();
            queue.setMaxConcurrentOperationCount(4);
            let delegate = Delegate::alloc().set_ivars(DelegateIvars {
                proxy: proxy.clone(),
                queue,
            });
            // SAFETY: initialize the NSObject subclass and its valid Rust ivars.
            let delegate: Retained<Delegate> = unsafe { msg_send![super(delegate), init] };
            let this = Modern::alloc().set_ivars(ModernIvars {
                extra,
                controls: controls.iter().map(|(name, _)| name.clone()).collect(),
                _delegate: delegate.clone(),
            });
            // SAFETY: Modern's ivars retain the native delegate for its whole lifetime.
            let writer: Retained<Modern> = unsafe {
                msg_send![super(this), initWithFileType: &*NSString::from_str(ty), delegate: ProtocolObject::<dyn NSFilePromiseProviderDelegate>::from_ref(&*delegate)]
            };
            output.push(ProtocolObject::from_retained(writer));
        } else {
            let board = NSPasteboard::pasteboardWithUniqueName();
            let types = extra.writableTypesForPasteboard(&board);
            let provider = Legacy::alloc().set_ivars(LegacyIvars {
                extra,
                proxy: proxy.clone(),
                values: controls,
            });
            // SAFETY: initialize NSObject with valid retained ivars.
            let provider: Retained<Legacy> = unsafe { msg_send![super(provider), init] };
            let item = NSPasteboardItem::new();
            if !item.setDataProvider_forTypes(ProtocolObject::from_ref(&*provider), &types) {
                return Err(io::ErrorKind::InvalidData.into());
            }
            output.push(ProtocolObject::from_retained(item));
        }
        proxies.push(proxy);
    }
    Ok((output, proxies))
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::model::ItemId;
    use std::collections::HashMap;
    #[test]
    fn modern_proxy_rebuilds_identity_without_requesting_names_during_enumeration() {
        objc2::rc::autoreleasepool(|_| {
            struct Counted {
                inner: Arc<dyn Namespace>,
                calls: Arc<AtomicUsize>,
            }
            impl Namespace for Counted {
                fn roots(&self) -> Vec<FileNode> {
                    vec![]
                }
                fn list(
                    &self,
                    node: NodeId,
                    cursor: Option<crate::model::DirectoryCursor>,
                ) -> io::Result<crate::model::DirectoryPage> {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    self.inner.list(node, cursor)
                }
                fn select(&self, node: NodeId) -> io::Result<FileNode> {
                    self.inner.select(node)
                }
                fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<crate::FileReader> {
                    self.inner.open(node, version, offset)
                }
            }
            let data = tempfile::tempdir().unwrap();
            let ready = data.path().join("ready");
            fs::create_dir(&ready).unwrap();
            fs::write(ready.join("original.jpg"), b"original encoded file").unwrap();
            let anchor = NodeId(1u64 << 32);
            let calls = Arc::new(AtomicUsize::new(0));
            let files: Arc<dyn Namespace> = Arc::new(Counted {
                inner: content::namespace::Local::new(vec![(anchor, ready)]).unwrap(),
                calls: calls.clone(),
            });
            let delegate = Delegate::alloc().set_ivars(DelegateIvars {
                proxy: Arc::new(Proxy {
                    files: files.clone(),
                    anchor,
                    names: Mutex::new(None),
                }),
                queue: NSOperationQueue::new(),
            });
            // SAFETY: initialize the native delegate with valid ivars.
            let delegate: Retained<Delegate> = unsafe { msg_send![super(delegate), init] };
            let writer = NSFilePromiseProvider::initWithFileType_delegate(
                NSFilePromiseProvider::alloc(),
                &NSString::from_str("public.jpeg"),
                ProtocolObject::from_ref(&*delegate),
            );
            let source = NSPasteboard::pasteboardWithUniqueName();
            assert!(source.writeObjects(&NSArray::from_retained_slice(&[ProtocolObject::from_retained(writer)])));
            let types = source.pasteboardItems().unwrap().objectAtIndex(0).types();
            let id = OfferId {
                clip: oink_foundation::content::ClipId {
                    origin: oink_foundation::DeviceId([1; 32]),
                    seq: 1,
                },
                generation: 1,
            };
            let snapshot = super::super::super::content::snapshot_board(&source, data.path(), id).unwrap();
            assert!(snapshot.inventory().has_promises());
            // snapshot_board models the source's real Drop and may render its
            // name metadata. Target enumeration below must do no new discovery.
            calls.store(0, Ordering::SeqCst);
            let provider = content::with_files(snapshot, files);
            let spec = crate::NativeSpec {
                inventory: provider.inventory(),
                contents: provider,
                files: vec![],
                file_contents: None,
                sensitivity: crate::Sensitivity::Normal,
            };
            let native = super::super::super::content::writers(&spec, &HashMap::new()).unwrap();
            let (native, _) = wrap(&spec, native).unwrap();
            let target = NSPasteboard::pasteboardWithUniqueName();
            assert!(target.writeObjects(&NSArray::from_retained_slice(&native)));
            assert_eq!(target.pasteboardItems().unwrap().objectAtIndex(0).types(), types);
            let readers = super::super::promise::receivers(&target).unwrap();
            assert_eq!(readers.len(), 1);
            assert_eq!(readers[0].fileTypes().objectAtIndex(0).to_string(), "public.jpeg");
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "enumeration must not discover names or receive files"
            );
            let key = NSString::from_str("com.apple.pasteboard.NSFilePromiseID");
            assert_ne!(
                source.dataForType(&key).unwrap().to_vec(),
                target.dataForType(&key).unwrap().to_vec()
            );
            let modern = native[0].clone().downcast::<Modern>().unwrap();
            // SAFETY: exact delegate ABI; this fixture's delegate only uses its
            // owned namespace, with no UI/main-thread AppKit state.
            let name: Retained<NSString> = unsafe {
                msg_send![&*modern.ivars()._delegate, filePromiseProvider: &*modern, fileNameForType: &*NSString::from_str("public.jpeg")]
            };
            assert_eq!(name.to_string(), "original.jpg");
            let target_file = data.path().join("original.jpg");
            let url = NSURL::fileURLWithPath(&NSString::from_str(&target_file.to_string_lossy()));
            let result = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let completed = result.clone();
            let done =
                block2::RcBlock::new(move |error: *mut NSError| completed.store(error.is_null(), Ordering::SeqCst));
            ProtocolObject::<dyn NSFilePromiseProviderDelegate>::from_ref(&*modern.ivars()._delegate)
                .filePromiseProvider_writePromiseToURL_completionHandler(&modern, &url, &done);
            assert!(result.load(Ordering::SeqCst));
            assert_eq!(fs::read(target_file).unwrap(), b"original encoded file");
        });
    }
    #[test]
    fn mixed_legacy_proxy_preserves_order_bytes_and_fulfils_original_file_at_destination() {
        objc2::rc::autoreleasepool(|_| {
            let data = tempfile::tempdir().unwrap();
            let source = NSPasteboard::pasteboardWithUniqueName();
            let original = NSPasteboardItem::new();
            let types = [
                ("public.tiff", b"tiff".as_slice()),
                ("public.url", b"https://example.invalid/photo.jpg".as_slice()),
                ("public.url-name", b"title".as_slice()),
                ("public.jpeg", b"original jpeg bytes".as_slice()),
                (
                    "com.apple.pasteboard.promised-file-url",
                    b"process-bound control".as_slice(),
                ),
                (
                    "com.apple.pasteboard.promised-file-content-type",
                    b"public.jpeg".as_slice(),
                ),
                (
                    "com.apple.pasteboard.promised-custom-data",
                    b"opaque prefix is NOT a protocol".as_slice(),
                ),
                ("example.browser.private", b"\x00\xff".as_slice()),
            ];
            for (ty, bytes) in types {
                original.setData_forType(&NSData::with_bytes(bytes), &NSString::from_str(ty));
            }
            assert!(
                source.writeObjects(&NSArray::from_retained_slice(&[ProtocolObject::from_retained(
                    original
                )]))
            );
            let id = OfferId {
                clip: oink_foundation::content::ClipId {
                    origin: oink_foundation::DeviceId([1; 32]),
                    seq: 1,
                },
                generation: 1,
            };
            let snapshot = super::super::super::content::snapshot_board(&source, data.path(), id).unwrap();
            let inventory = snapshot.inventory();
            assert_eq!(inventory.items[0].id, ItemId(0));
            assert!(inventory.has_promises());
            assert!(inventory.roots.is_empty());
            let anchor = inventory.namespace_roots()[0].id;
            let batch = crate::store::Batch::create(
                data.path(),
                crate::store::StoreKind::PromiseExport,
                crate::store::BatchId::random().unwrap(),
            )
            .unwrap();
            let ready = batch.path().join("ready");
            fs::create_dir(&ready).unwrap();
            fs::write(ready.join("photo.jpg"), b"original jpeg bytes").unwrap();
            let local = content::namespace::Local::new(vec![(anchor, ready)]).unwrap();
            let pending = content::namespace::Pending::new(local, [anchor], batch);
            assert!(pending.finish(Ok(())));
            let provider = content::with_files(snapshot, pending);
            let spec = crate::NativeSpec {
                inventory: provider.inventory(),
                contents: provider,
                files: vec![],
                file_contents: None,
                sensitivity: crate::Sensitivity::Normal,
            };
            let native = super::super::super::content::writers(&spec, &HashMap::new()).unwrap();
            let (native, _) = wrap(&spec, native).unwrap();
            let target = NSPasteboard::pasteboardWithUniqueName();
            assert!(target.writeObjects(&NSArray::from_retained_slice(&native)));
            let items = target.pasteboardItems().unwrap();
            assert_eq!(items.len(), 1);
            let item = items.objectAtIndex(0);
            assert_eq!(
                item.types().iter().map(|ty| ty.to_string()).collect::<Vec<_>>(),
                types.iter().map(|(ty, _)| *ty).collect::<Vec<_>>()
            );
            for (ty, bytes) in types
                .iter()
                .filter(|(ty, _)| *ty != "com.apple.pasteboard.promised-file-url")
            {
                assert_eq!(item.dataForType(&NSString::from_str(ty)).unwrap().to_vec(), *bytes);
            }
            let destination = data.path().join("destination");
            fs::create_dir(&destination).unwrap();
            let location = super::super::super::pasteboard::file_url_string(&destination);
            target.setString_forType(
                &NSString::from_str(&location),
                &NSString::from_str("com.apple.pastelocation"),
            );
            let url = item
                .stringForType(&NSString::from_str("com.apple.pasteboard.promised-file-url"))
                .unwrap();
            assert_eq!(
                NSURL::URLWithString(&url).unwrap().path().unwrap().to_string(),
                destination.join("photo.jpg").to_string_lossy()
            );
            assert_eq!(fs::read(destination.join("photo.jpg")).unwrap(), b"original jpeg bytes");
            assert!(!destination.join("_promise").exists());
        });
    }
}
