//! Target-local native identities for source promises. The original item/type
//! order remains intact; only process-bound fulfilment controls are reconstructed.
use super::*;
use crate::content::{self, namespace::Namespace};
use crate::model::{FileNode, NativeFormat, NodeId, NodeKind, RepresentationData, Unavailable};
use objc2_app_kit::{NSPasteboardItemDataProvider, NSPasteboardType};
#[cfg(all(test, debug_assertions))]
use oink_foundation::content::OfferId;
use std::{
    fs,
    io::{self, Write},
};

#[cfg(all(test, debug_assertions))]
pub(in crate::macos) mod linked_tests;

const FILE_URL_PROMISE: &str = "com.apple.pasteboard.promised-file-url";
const FILE_PROMISE_CONTENT: &str = "com.apple.pasteboard.promised-file-content-type";

pub(in crate::macos) struct Proxy {
    files: Arc<dyn Namespace>,
    anchor: NodeId,
    names: Mutex<Option<Vec<FileNode>>>,
    // A native target can retain the promise and invoke it after drag-ended.
    // Keep the whole consumer alive, including gaps between per-file readers.
    _lease: content::namespace::NativeLease,
}
impl Proxy {
    fn new(files: Arc<dyn Namespace>, anchor: NodeId) -> io::Result<Self> {
        Ok(Self {
            _lease: files.native_lease()?,
            files,
            anchor,
            names: Mutex::new(None),
        })
    }
    fn entries(&self) -> io::Result<Vec<FileNode>> {
        let mut cached = self.names.lock().unwrap();
        if let Some(names) = cached.as_ref() {
            return Ok(names.clone());
        }
        self.files.prepare()?;
        let metadata = self.files.native_file_metadata();
        if let Some(group) = metadata.promises.iter().find(|group| group.anchor == self.anchor) {
            let roots = self.files.roots();
            metadata.validate(&roots.iter().map(|root| root.id).collect::<Vec<_>>())?;
            let names: Vec<_> = group
                .roots
                .iter()
                .map(|id| {
                    roots
                        .iter()
                        .find(|root| root.id == *id)
                        .cloned()
                        .ok_or(io::ErrorKind::InvalidData)
                })
                .collect::<Result<_, _>>()?;
            *cached = Some(names.clone());
            return Ok(names);
        }
        if let Some(root) = self.files.roots().into_iter().find(|root| root.id == self.anchor) {
            let names = vec![root];
            *cached = Some(names.clone());
            return Ok(names);
        }
        let mut names = Vec::new();
        let mut cursor = None;
        loop {
            let page = list(self.files.as_ref(), self.anchor, cursor)?;
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
    fn consumed(&self, entries: &[FileNode]) {
        // Proactive hydration and path-only reads never reach this proof. Only
        // this successful complete promise callback authorizes MOVE cleanup.
        if let Err(error) = self
            .files
            .native_consumed(&entries.iter().map(|entry| entry.id).collect::<Vec<_>>())
        {
            tracing::debug!(%error, "promise consumption proof not persisted; retain received copy");
        }
    }

    pub(super) fn receive(&self, destination: &Path) -> io::Result<Vec<String>> {
        let mut written = Vec::new();
        let entries = self.entries()?;
        for entry in &entries {
            let path = unique_path(&destination.join(&entry.name));
            copy_node(self.files.as_ref(), entry, &path, 0)?;
            written.push(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .ok_or(io::ErrorKind::InvalidData)?
                    .to_owned(),
            );
        }
        self.consumed(&entries);
        Ok(written)
    }
}
fn list(
    files: &dyn Namespace,
    node: NodeId,
    cursor: Option<crate::model::DirectoryCursor>,
) -> io::Result<crate::model::DirectoryPage> {
    if let Some(page) = files.native_list(node, cursor) {
        return page;
    }
    if files.requires_native_files() {
        return Err(io::ErrorKind::NotConnected.into());
    }
    files.list(node, cursor)
}
fn copy_node(files: &dyn Namespace, entry: &FileNode, path: &Path, depth: usize) -> io::Result<()> {
    if depth > 256 || !entry.valid() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    if let Some(target) = entry.kind.symlink_target() {
        if !oink_foundation::content::safe_symlink_target(target, depth) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        // A promise exports the received link itself, never opens its target.
        std::os::unix::fs::symlink(target, path)?;
        return Ok(());
    }
    if entry.kind == NodeKind::Directory {
        fs::create_dir(path)?;
        let result = (|| {
            let mut cursor = None;
            loop {
                let page = list(files, entry.id, cursor)?;
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
        let (mut input, selected) = if let Some(input) = files.native_open(entry.id, 0) {
            (input?, None)
        } else {
            if files.requires_native_files() {
                return Err(io::ErrorKind::NotConnected.into());
            }
            let selected = files.select(entry.id)?;
            (files.open(selected.id, selected.version, 0)?, Some(selected))
        };
        let mut output = fs::OpenOptions::new().write(true).create_new(true).open(path)?;
        let result = (|| {
            let size = io::copy(&mut input, &mut output)?;
            // An unknown-size native placeholder can report zero before its
            // coordinated read hydrates it. Verify against the received copy's
            // final metadata after EOF, not the pre-hydration stat. The source
            // namespace is never reopened, and a real mismatch still fails.
            let metadata = match selected {
                Some(selected) => crate::FileMetadata {
                    size: match selected.size {
                        crate::model::ContentSize::Known(size) => size,
                        crate::model::ContentSize::Unknown => return Err(io::ErrorKind::InvalidData.into()),
                    },
                    modified_ms: selected.modified_ms,
                    exec_bits: selected.exec_bits,
                },
                None => files.native_metadata(entry.id).ok_or(io::ErrorKind::NotConnected)??,
            };
            if metadata.size != size {
                return Err(io::ErrorKind::InvalidData.into());
            }
            crate::received::apply_exec_bits(&output, metadata.exec_bits)?;
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
    // Foreign virtual files add native promise controls beside their file URL.
    // Same-Mac mimicry retains exactly the source's existing control positions.
    extra_types: Option<Vec<String>>,
    link_drag: bool,
    _delegate: Retained<Delegate>,
}
define_class!(
    #[unsafe(super(NSFilePromiseProvider))]
    #[name = "OinkHeldPromiseProxy"]
    #[ivars = ModernIvars]
    struct Modern;
    unsafe impl NSObjectProtocol for Modern {}
    unsafe impl NSPasteboardItemDataProvider for Modern {
        #[unsafe(method(pasteboard:item:provideDataForType:))]
        fn provide(&self, board: Option<&NSPasteboard>, item: &NSPasteboardItem, ty: &NSPasteboardType) {
            if self.ivars().link_drag && ty.to_string() == FILE_URL_PROMISE {
                let result = (|| {
                    // Pasteboard Manager supplies this receiver-local context
                    // only for a real promise read with a paste destination.
                    // Ordinary URL/HTML reads must never materialize the file.
                    let destination = board
                        .and_then(|board| board.stringForType(&NSString::from_str("com.apple.pastelocation")))
                        .and_then(|value| NSURL::URLWithString(&value))
                        .and_then(|url| url.filePathURL())
                        .and_then(|url| url.path())
                        .map(|path| PathBuf::from(path.to_string()))
                        .ok_or(io::ErrorKind::InvalidData)?;
                    let names = self.ivars()._delegate.ivars().proxy.receive(&destination)?;
                    if names.len() != 1 {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                    Ok::<_, io::Error>(super::super::pasteboard::file_url_string(&destination.join(&names[0])))
                })();
                match result {
                    Ok(url) => {
                        item.setString_forType(&NSString::from_str(&url), ty);
                    }
                    Err(error) => tracing::warn!(
                        file_reason = content::reason(&error) as u8,
                        file_code = error.raw_os_error().unwrap_or(0) as u32,
                        "native linked file promise delivery failed"
                    ),
                }
            } else if let Some(value) = self.pasteboardPropertyListForType(ty) {
                set_property(item, ty, &value);
            }
        }
    }
    unsafe impl NSPasteboardWriting for Modern {
        #[unsafe(method_id(writableTypesForPasteboard:))]
        fn types(&self, pb: &NSPasteboard) -> Retained<NSArray<NSString>> {
            let extra = self.ivars().extra.writableTypesForPasteboard(pb);
            if self.ivars().extra_types.is_none() {
                extra
            } else {
                let mut types: Vec<_> = extra
                    .iter()
                    .filter(|ty| !self.ivars().link_drag || ty.to_string() != "public.file-url")
                    .collect();
                if self.ivars().link_drag {
                    // Safari/Chromium image drags use the legacy URL promise,
                    // not a real file URL. Copying modern NSFilePromiseProvider
                    // controls into a plain item does not supply that legacy
                    // fulfilment route. Replace them with the browser protocol,
                    // whose independent reader requests a destination and gets
                    // the actual received file; no modern IPC identity is needed.
                    // Keep companions lazy/readable on this same item. Adding
                    // a real file URL instead regresses Chrome link navigation.
                    // Publication/receiver checks: macOS 26.6.2. Clipboard,
                    // real files and same-Mac source ordering are unaffected.
                    types.push(NSString::from_str(FILE_URL_PROMISE));
                    types.push(NSString::from_str(FILE_PROMISE_CONTENT));
                } else {
                    // SAFETY: inherited NSPasteboardWriting signature, metadata only.
                    let base: Retained<NSArray<NSString>> =
                        unsafe { msg_send![super(self), writableTypesForPasteboard: pb] };
                    for ty in base {
                        if !types.contains(&ty) {
                            types.push(ty);
                        }
                    }
                }
                NSArray::from_retained_slice(&types)
            }
        }
        #[unsafe(method(writingOptionsForType:pasteboard:))]
        fn options(&self, ty: &NSString, pb: &NSPasteboard) -> NSPasteboardWritingOptions {
            if self.ivars().link_drag && ty.to_string() == FILE_URL_PROMISE {
                NSPasteboardWritingOptions::Promised
            } else if self.local_control(&ty.to_string()) {
                // SAFETY: same inherited NSPasteboardWriting signature.
                unsafe { msg_send![super(self), writingOptionsForType: ty, pasteboard: pb] }
            } else {
                self.ivars().extra.writingOptionsForType_pasteboard(ty, pb)
            }
        }
        #[unsafe(method_id(pasteboardPropertyListForType:))]
        fn property(&self, ty: &NSString) -> Option<Retained<AnyObject>> {
            if self.local_control(&ty.to_string()) {
                // SAFETY: use the new provider's local IPC identity, never source bytes.
                unsafe { msg_send![super(self), pasteboardPropertyListForType: ty] }
            } else {
                self.ivars().extra.pasteboardPropertyListForType(ty)
            }
        }
    }
);

fn set_property(item: &NSPasteboardItem, ty: &NSPasteboardType, value: &AnyObject) -> bool {
    // Match NSPasteboardWriting's bytes/string encoding. setPropertyList wraps
    // NSData and NSString in a binary plist: a receiver then cannot read either
    // URL/HTML bytes or the browser promise's plain content-type string.
    if let Some(data) = value.downcast_ref::<NSData>() {
        item.setData_forType(data, ty)
    } else if let Some(text) = value.downcast_ref::<NSString>() {
        item.setString_forType(text, ty)
    } else {
        // SAFETY: this is the native writer's property list for this exact type.
        unsafe { item.setPropertyList_forType(value, ty) }
    }
}

define_class!(
    #[unsafe(super(NSPasteboardItem))]
    #[name = "OinkHeldPromiseCompanionItem"]
    #[ivars = Retained<Modern>]
    struct CompanionItem;
    unsafe impl NSObjectProtocol for CompanionItem {}
);

impl Modern {
    fn companion_item(this: Retained<Self>, board: &NSPasteboard) -> io::Result<Retained<CompanionItem>> {
        let item = CompanionItem::alloc().set_ivars(this.clone());
        // SAFETY: initialize the NSPasteboardItem subclass with its retained
        // native promise provider/delegate. It must outlive receiver requests.
        let item: Retained<CompanionItem> = unsafe { msg_send![super(item), init] };
        for ty in this.writableTypesForPasteboard(board) {
            let ok = if this
                .writingOptionsForType_pasteboard(&ty, board)
                .contains(NSPasteboardWritingOptions::Promised)
            {
                item.setDataProvider_forTypes(ProtocolObject::from_ref(&*this), &NSArray::from_slice(&[&*ty]))
            } else if let Some(value) = this.pasteboardPropertyListForType(&ty) {
                set_property(&item, &ty, &value)
            } else {
                false
            };
            if !ok {
                return Err(io::ErrorKind::InvalidData.into());
            }
        }
        Ok(item)
    }
}

#[cfg(all(test, debug_assertions))]
pub(super) fn companion_promise_provider(writer: &AnyObject) -> Retained<NSFilePromiseProvider> {
    writer
        .downcast_ref::<CompanionItem>()
        .unwrap()
        .ivars()
        .clone()
        .into_super()
}
impl Modern {
    fn local_control(&self, name: &str) -> bool {
        match &self.ivars().extra_types {
            Some(extra) => !extra.iter().any(|ty| ty == name),
            None => self.ivars().controls.iter().any(|ty| ty == name),
        }
    }
}
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
                copy_node(self.ivars().proxy.files.as_ref(), &names[0], &path, 0)?;
                self.ivars().proxy.consumed(&names);
                Ok::<_, io::Error>(())
            })();
            if let Err(error) = result {
                tracing::warn!(
                    file_reason = content::reason(&error) as u8,
                    file_code = error.raw_os_error().unwrap_or(0) as u32,
                    "native promise delivery failed"
                );
                // SAFETY: standard NSError with no business-data userInfo.
                let error = unsafe { NSError::errorWithDomain_code_userInfo(NSCocoaErrorDomain, 512, None) };
                done.call((Retained::as_ptr(&error) as *mut NSError,));
            } else {
                done.call((std::ptr::null_mut(),));
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
pub(in crate::macos) fn wrap(
    spec: &crate::NativeSpec,
    writers: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>>,
) -> io::Result<(Writers, Vec<Arc<Proxy>>)> {
    if spec.inventory.platform != oink_foundation::clipboard::ClipPlatform::Mac {
        return wrap_virtual(spec, writers, false);
    }
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
        let proxy = Arc::new(Proxy::new(files, anchor)?);
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
                extra_types: None,
                link_drag: false,
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
    fn advertised_grant_preserves_app_tree_through_held_promise() {
        use crate::content::{grants::SourceGrants, namespace::Local};
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Fixture.app");
        fs::create_dir_all(root.join("Contents/MacOS")).unwrap();
        fs::create_dir_all(root.join("Contents/Resources/nested")).unwrap();
        fs::write(root.join("Contents/Resources/nested/content.txt"), b"resource").unwrap();
        let executable = root.join("Contents/MacOS/fixture");
        fs::write(&executable, b"executable").unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o4755)).unwrap();
        for (name, target) in [
            ("current", "nested"),
            ("linked.txt", "nested/content.txt"),
            ("unsafe", "../../../outside"),
        ] {
            symlink(target, root.join("Contents/Resources").join(name)).unwrap();
        }
        let source = Local::new(vec![(NodeId(1), root)]).unwrap();
        let peer = oink_foundation::DeviceId([2; 32]);
        let grants = SourceGrants::load(&temp.path().join("grants"), [(peer, [1; 32])]).unwrap();
        grants.set_peer_safe_symlinks(
            peer,
            crate::negotiation::CAPABILITIES.contains(&"oink.files.safe-symlinks/1"),
        );
        grants.set_peer_exec_bits(
            peer,
            crate::negotiation::CAPABILITIES.contains(&"oink.files.exec-bits/1"),
        );
        let token = grants.issue(peer, source.as_ref(), &[NodeId(1)]).unwrap();
        let received = grants.open(peer, token).unwrap();
        let output = temp.path().join("finder");
        fs::create_dir(&output).unwrap();
        let proxy = Proxy::new(received, NodeId(1)).unwrap();
        assert_eq!(proxy.receive(&output).unwrap(), ["Fixture.app"]);
        let copied = output.join("Fixture.app/Contents");
        for (name, target) in [("current", "nested"), ("linked.txt", "nested/content.txt")] {
            assert_eq!(
                fs::read_link(copied.join("Resources").join(name)).unwrap(),
                Path::new(target)
            );
        }
        assert!(fs::symlink_metadata(copied.join("Resources/unsafe")).is_err());
        let mode = fs::metadata(copied.join("MacOS/fixture")).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
        assert_eq!(mode & 0o7002, 0);
    }
    #[test]
    fn received_promise_checks_native_size_after_hydration() {
        use std::sync::atomic::AtomicBool;
        struct Received {
            hydrated: Arc<AtomicBool>,
            size: u64,
        }
        struct Reader(io::Cursor<Vec<u8>>, Arc<AtomicBool>);
        impl io::Read for Reader {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                let count = self.0.read(bytes)?;
                if count == 0 {
                    self.1.store(true, Ordering::Release);
                }
                Ok(count)
            }
        }
        impl Namespace for Received {
            fn roots(&self) -> Vec<FileNode> {
                vec![]
            }
            fn requires_native_files(&self) -> bool {
                true
            }
            fn list(
                &self,
                _: NodeId,
                _: Option<crate::model::DirectoryCursor>,
            ) -> io::Result<crate::model::DirectoryPage> {
                panic!("source list")
            }
            fn select(&self, _: NodeId) -> io::Result<FileNode> {
                panic!("source select")
            }
            fn open(&self, _: NodeId, _: u64, _: u64) -> io::Result<crate::FileReader> {
                panic!("source read")
            }
            fn native_open(&self, _: NodeId, _: u64) -> Option<io::Result<crate::FileReader>> {
                Some(Ok(Box::new(Reader(
                    io::Cursor::new(vec![b'B'; 2048]),
                    self.hydrated.clone(),
                ))))
            }
            fn native_metadata(&self, _: NodeId) -> Option<io::Result<crate::FileMetadata>> {
                Some(Ok(crate::FileMetadata {
                    exec_bits: self.hydrated.load(Ordering::Acquire).then_some(0o111),
                    size: if self.hydrated.load(Ordering::Acquire) {
                        self.size
                    } else {
                        0
                    },
                    modified_ms: None,
                }))
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let entry = FileNode {
            exec_bits: None,
            id: NodeId(2),
            parent: Some(NodeId(1)),
            name: "first.bin".into(),
            kind: NodeKind::File,
            size: crate::model::ContentSize::Unknown,
            version: 1,
            modified_ms: None,
        };
        let path = temp.path().join("first.bin");
        let received = Received {
            hydrated: Arc::new(AtomicBool::new(false)),
            size: 2048,
        };
        copy_node(&received, &entry, &path, 1).unwrap();
        assert_eq!(fs::read(&path).unwrap(), vec![b'B'; 2048]);
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "use the received metadata after native EOF");
        assert_eq!(mode & 0o7002, 0);
        fs::remove_file(&path).unwrap();
        let mismatched = Received {
            hydrated: Arc::new(AtomicBool::new(false)),
            size: 2049,
        };
        assert!(copy_node(&mismatched, &entry, &path, 1).is_err());
        assert!(
            !path.exists(),
            "an actual size mismatch still removes the incomplete promise"
        );
    }
    #[test]
    fn foreign_virtual_file_offers_promise_and_url_from_the_same_received_copy() {
        use crate::content::Provider;
        use crate::model::{ContentItem, Inventory, Medium, Representation, RepresentationId};
        use oink_foundation::clipboard::ClipPlatform;
        use std::sync::atomic::AtomicBool;
        struct Received {
            files: Arc<dyn Namespace>,
            reads: Arc<AtomicUsize>,
        }
        impl Namespace for Received {
            fn roots(&self) -> Vec<FileNode> {
                self.files.roots()
            }
            fn requires_native_files(&self) -> bool {
                true
            }
            fn list(
                &self,
                _: NodeId,
                _: Option<crate::model::DirectoryCursor>,
            ) -> io::Result<crate::model::DirectoryPage> {
                panic!("native promise must not list at source")
            }
            fn select(&self, _: NodeId) -> io::Result<FileNode> {
                panic!("native promise must not select at source")
            }
            fn open(&self, _: NodeId, _: u64, _: u64) -> io::Result<crate::FileReader> {
                panic!("native promise must not read at source")
            }
            fn native_metadata(&self, node: NodeId) -> Option<io::Result<crate::FileMetadata>> {
                Some(self.files.select(node).map(|entry| crate::FileMetadata {
                    exec_bits: None,
                    size: match entry.size {
                        crate::model::ContentSize::Known(size) => size,
                        _ => unreachable!(),
                    },
                    modified_ms: entry.modified_ms,
                }))
            }
            fn native_open(&self, node: NodeId, offset: u64) -> Option<io::Result<crate::FileReader>> {
                self.reads.fetch_add(1, Ordering::Relaxed);
                Some(
                    self.files
                        .select(node)
                        .and_then(|entry| self.files.open(node, entry.version, offset)),
                )
            }
        }
        struct Source {
            inventory: Inventory,
            files: Arc<dyn Namespace>,
        }
        impl Provider for Source {
            fn inventory(&self) -> Inventory {
                self.inventory.clone()
            }
            fn namespace(&self) -> Option<Arc<dyn Namespace>> {
                Some(self.files.clone())
            }
            fn translator(&self) -> Arc<dyn content::Translator> {
                Arc::new(super::super::super::content::MacTranslator)
            }
            fn open(&self, _: RepresentationId) -> io::Result<crate::FileReader> {
                panic!("file controls must not read source representation")
            }
        }
        objc2::rc::autoreleasepool(|_| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("received.txt");
            fs::write(&path, b"only received bytes").unwrap();
            let reads = Arc::new(AtomicUsize::new(0));
            let files: Arc<dyn Namespace> = Arc::new(Received {
                files: content::namespace::Local::new(vec![(NodeId(1), path.clone())]).unwrap(),
                reads: reads.clone(),
            });
            for virtual_files in [false, true] {
                let mut inventory = content::tests::fixture().inventory();
                inventory.platform = ClipPlatform::Windows;
                inventory.roots = files.roots();
                inventory.items = vec![ContentItem {
                    id: ItemId(0),
                    file: Some(NodeId(1)),
                    virtual_files,
                    representations: vec![Representation {
                        id: RepresentationId(1),
                        format: if virtual_files {
                            NativeFormat::WindowsRegistered("FileGroupDescriptorW".into())
                        } else {
                            NativeFormat::WindowsPredefined(15)
                        },
                        standard: None,
                        medium: Medium::HGlobal,
                        aspect: 1,
                        index: -1,
                        data: RepresentationData::File(NodeId(1)),
                    }],
                }];
                let spec = crate::NativeSpec {
                    inventory: inventory.clone(),
                    contents: Arc::new(Source {
                        inventory,
                        files: files.clone(),
                    }),
                    files: vec![],
                    file_contents: None,
                    sensitivity: crate::Sensitivity::Normal,
                };
                let url = super::super::super::pasteboard::file_url_string(&path);
                let urls = HashMap::from([(NodeId(1), url.clone())]);
                let board = crate::macos::test_pasteboard::new();
                super::super::super::content::write(Some(&board.name().to_string()), &spec, &urls, &|| Ok(())).unwrap();
                assert_eq!(board.pasteboardItems().unwrap().len(), 1);
                assert_eq!(
                    board
                        .stringForType(&NSString::from_str("public.file-url"))
                        .unwrap()
                        .to_string(),
                    url
                );
                assert_eq!(
                    super::super::promise::receivers(&board).is_some(),
                    virtual_files,
                    "virtual files keep an equivalent promise; real files gain no promise"
                );
                assert_eq!(reads.load(Ordering::Relaxed), 0);
                if virtual_files {
                    let native = super::super::super::content::writers(&spec, &urls).unwrap();
                    let (native, _) = wrap(&spec, native).unwrap();
                    let modern = native[0].clone().downcast::<Modern>().unwrap();
                    let target = dir.path().join("promised.txt");
                    let target_url = NSURL::fileURLWithPath(&NSString::from_str(&target.to_string_lossy()));
                    let complete = Arc::new(AtomicBool::new(false));
                    let done_complete = complete.clone();
                    let done = block2::RcBlock::new(move |error: *mut NSError| {
                        done_complete.store(error.is_null(), Ordering::Release)
                    });
                    ProtocolObject::<dyn NSFilePromiseProviderDelegate>::from_ref(&*modern.ivars()._delegate)
                        .filePromiseProvider_writePromiseToURL_completionHandler(&modern, &target_url, &done);
                    assert!(complete.load(Ordering::Acquire));
                    assert_eq!(reads.load(Ordering::Relaxed), 1);
                    assert_eq!(fs::read(target).unwrap(), fs::read(&path).unwrap());
                }
                board.clearContents();
            }
        });
    }
    #[test]
    fn promise_object_pins_native_copy_before_and_between_fulfilments() {
        struct Lease(Arc<AtomicUsize>);
        impl Drop for Lease {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        struct Leased {
            inner: Arc<dyn Namespace>,
            active: Arc<AtomicUsize>,
            consumed: Arc<AtomicUsize>,
        }
        impl Namespace for Leased {
            fn native_consumed(&self, nodes: &[NodeId]) -> io::Result<()> {
                assert_eq!(nodes, &[NodeId(1)]);
                self.consumed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn native_lease(&self) -> io::Result<content::namespace::NativeLease> {
                self.active.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(Lease(self.active.clone())))
            }
            fn roots(&self) -> Vec<FileNode> {
                self.inner.roots()
            }
            fn list(
                &self,
                node: NodeId,
                cursor: Option<crate::model::DirectoryCursor>,
            ) -> io::Result<crate::model::DirectoryPage> {
                self.inner.list(node, cursor)
            }
            fn select(&self, node: NodeId) -> io::Result<FileNode> {
                self.inner.select(node)
            }
            fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<crate::FileReader> {
                assert_eq!(self.active.load(Ordering::SeqCst), 1, "promise lost its consumer lease");
                self.inner.open(node, version, offset)
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::write(&source, b"received bytes").unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let consumed = Arc::new(AtomicUsize::new(0));
        let files = Arc::new(Leased {
            inner: content::namespace::Local::new(vec![(NodeId(1), source)]).unwrap(),
            active: active.clone(),
            consumed: consumed.clone(),
        });
        let proxy = Arc::new(Proxy::new(files, NodeId(1)).unwrap());
        assert_eq!(
            active.load(Ordering::SeqCst),
            1,
            "unread promise must pin the native copy"
        );
        let destination = temp.path().join("target");
        fs::create_dir(&destination).unwrap();
        assert_eq!(consumed.load(Ordering::SeqCst), 0);
        assert!(proxy.receive(&destination.join("missing")).is_err());
        assert_eq!(
            consumed.load(Ordering::SeqCst),
            0,
            "failed promise cannot authorize cleanup"
        );
        proxy.receive(&destination).unwrap();
        assert_eq!(consumed.load(Ordering::SeqCst), 1);
        assert_eq!(active.load(Ordering::SeqCst), 1);
        let retained = proxy.clone();
        drop(proxy);
        retained.receive(&destination).unwrap();
        assert_eq!(active.load(Ordering::SeqCst), 1);
        drop(retained);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn received_promise_uses_original_root_order_and_only_native_copy() {
        use crate::content::native_metadata::{MacPromiseGroup, NativeFileMetadata};
        use crate::model::{DirectoryCursor, DirectoryPage};
        struct Received(Arc<dyn Namespace>);
        impl Namespace for Received {
            fn native_file_metadata(&self) -> NativeFileMetadata {
                NativeFileMetadata {
                    windows: vec![],
                    promises: vec![MacPromiseGroup {
                        controls: Vec::new(),
                        anchor: NodeId(99),
                        roots: vec![NodeId(2), NodeId(1)],
                        indices: vec![0, 1],
                    }],
                }
            }
            fn requires_native_files(&self) -> bool {
                true
            }
            fn roots(&self) -> Vec<FileNode> {
                self.0.roots()
            }
            fn list(&self, _: NodeId, _: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
                panic!("a completed promise must not enumerate the source")
            }
            fn select(&self, _: NodeId) -> io::Result<FileNode> {
                panic!("a completed promise must not select at the source")
            }
            fn open(&self, _: NodeId, _: u64, _: u64) -> io::Result<crate::FileReader> {
                panic!("a completed promise must not read from the source")
            }
            fn native_list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> Option<io::Result<DirectoryPage>> {
                Some(self.0.list(node, cursor))
            }
            fn native_open(&self, node: NodeId, offset: u64) -> Option<io::Result<crate::FileReader>> {
                Some(
                    self.0
                        .select(node)
                        .and_then(|entry| self.0.open(node, entry.version, offset)),
                )
            }
            fn native_metadata(&self, node: NodeId) -> Option<io::Result<crate::FileMetadata>> {
                Some(self.0.select(node).map(|entry| crate::FileMetadata {
                    exec_bits: None,
                    size: match entry.size {
                        crate::model::ContentSize::Known(size) => size,
                        _ => panic!("known file"),
                    },
                    modified_ms: entry.modified_ms,
                }))
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("received");
        fs::create_dir(&root).unwrap();
        let folder = root.join("folder");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("nested.bin"), b"received bytes").unwrap();
        fs::create_dir(folder.join("empty")).unwrap();
        fs::write(root.join("zero.bin"), b"").unwrap();
        let local =
            content::namespace::Local::new(vec![(NodeId(1), folder), (NodeId(2), root.join("zero.bin"))]).unwrap();
        let proxy = Proxy::new(Arc::new(Received(local)), NodeId(99)).unwrap();
        let destination = temp.path().join("destination");
        fs::create_dir(&destination).unwrap();
        assert_eq!(proxy.receive(&destination).unwrap(), ["zero.bin", "folder"]);
        assert_eq!(
            fs::read(destination.join("folder/nested.bin")).unwrap(),
            b"received bytes"
        );
        assert!(destination.join("folder/empty").is_dir());
        assert_eq!(fs::metadata(destination.join("zero.bin")).unwrap().len(), 0);
        assert!(!destination.join("_promise").exists());
    }
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
                proxy: Arc::new(Proxy::new(files.clone(), anchor).unwrap()),
                queue: NSOperationQueue::new(),
            });
            // SAFETY: initialize the native delegate with valid ivars.
            let delegate: Retained<Delegate> = unsafe { msg_send![super(delegate), init] };
            let writer = NSFilePromiseProvider::initWithFileType_delegate(
                NSFilePromiseProvider::alloc(),
                &NSString::from_str("public.jpeg"),
                ProtocolObject::from_ref(&*delegate),
            );
            let source = crate::macos::test_pasteboard::new();
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
            let clipboard_target = crate::macos::test_pasteboard::new();
            super::super::super::content::write(
                Some(&clipboard_target.name().to_string()),
                &spec,
                &HashMap::new(),
                &|| Ok(()),
            )
            .unwrap();
            let identity = NSString::from_str("com.apple.pasteboard.NSFilePromiseID");
            assert_ne!(
                source.dataForType(&identity).unwrap().to_vec(),
                clipboard_target
                    .dataForType(&identity)
                    .expect("clipboard owns a local promise identity")
                    .to_vec(),
                "clipboard publication must rebuild source-process controls"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "clipboard publication must not request promise names"
            );
            let native = super::super::super::content::writers(&spec, &HashMap::new()).unwrap();
            let (native, _) = wrap(&spec, native).unwrap();
            let target = crate::macos::test_pasteboard::new();
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
            let source = crate::macos::test_pasteboard::new();
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
            let target = crate::macos::test_pasteboard::new();
            assert!(target.writeObjects(&NSArray::from_retained_slice(&native)));
            let items = target.pasteboardItems().unwrap();
            assert_eq!(items.len(), 1);
            let item = items.objectAtIndex(0);
            assert_eq!(
                item.types().iter().map(|ty| ty.to_string()).collect::<Vec<_>>(),
                types
                    .iter()
                    .map(|(ty, _)| *ty)
                    .chain(Some("public.html"))
                    .collect::<Vec<_>>()
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

pub(in crate::macos) fn needed(spec: &crate::NativeSpec) -> bool {
    spec.inventory.has_promises()
        || (spec.inventory.platform != oink_foundation::clipboard::ClipPlatform::Mac
            && spec
                .inventory
                .items
                .iter()
                .any(|item| item.virtual_files && item.file.is_some()))
}

pub(in crate::macos) fn wrap_drag(
    spec: &crate::NativeSpec,
    writers: Writers,
) -> io::Result<(Writers, Vec<Arc<Proxy>>)> {
    if spec.inventory.platform != oink_foundation::clipboard::ClipPlatform::Mac {
        wrap_virtual(spec, writers, true)
    } else {
        wrap(spec, writers)
    }
}

fn wrap_virtual(
    spec: &crate::NativeSpec,
    mut writers: Writers,
    prefer_url: bool,
) -> io::Result<(Writers, Vec<Arc<Proxy>>)> {
    // native_items projects the foreign file roots as its final N items, with
    // selection companions attached to the first root for a single IDataObject.
    let split = writers
        .len()
        .checked_sub(spec.inventory.roots.len())
        .ok_or(io::ErrorKind::InvalidData)?;
    let file_writers = writers.split_off(split);
    let mut proxies = Vec::new();
    let files = spec
        .file_contents
        .as_ref()
        .and_then(|files| files.namespace())
        .or_else(|| spec.contents.namespace())
        .ok_or(io::ErrorKind::InvalidData)?;
    let selection = spec.inventory.items.len() == 1 && spec.inventory.items[0].virtual_files;
    for (root, extra) in spec.inventory.roots.iter().zip(file_writers) {
        if !selection
            && !spec
                .inventory
                .items
                .iter()
                .any(|item| item.virtual_files && item.file == Some(root.id))
        {
            writers.push(extra);
            continue;
        }
        let proxy = Arc::new(Proxy::new(files.clone(), root.id)?);
        let board = NSPasteboard::pasteboardWithUniqueName();
        let extra_types: Vec<_> = extra
            .writableTypesForPasteboard(&board)
            .iter()
            .map(|ty| ty.to_string())
            .collect();
        let link_drag = prefer_url && extra_types.iter().any(|ty| ty == "public.url");
        let queue = NSOperationQueue::new();
        queue.setMaxConcurrentOperationCount(4);
        let delegate = Delegate::alloc().set_ivars(DelegateIvars {
            proxy: proxy.clone(),
            queue,
        });
        // SAFETY: initialize NSObject with valid retained Rust ivars.
        let delegate: Retained<Delegate> = unsafe { msg_send![super(delegate), init] };
        let this = Modern::alloc().set_ivars(ModernIvars {
            extra,
            controls: Vec::new(),
            extra_types: Some(extra_types),
            link_drag,
            _delegate: delegate.clone(),
        });
        let ty = file_type(root);
        // SAFETY: the native writer owns its weak delegate through Rust ivars.
        let writer: Retained<Modern> = unsafe {
            msg_send![super(this), initWithFileType: &*NSString::from_str(&ty), delegate: ProtocolObject::<dyn NSFilePromiseProviderDelegate>::from_ref(&*delegate)]
        };
        if link_drag {
            // AppKit's NSFilePromiseProvider publication does not service extra
            // promised URL/title/HTML flavors: their types appear, but a real
            // pasteboard read returns nil without calling the writer. A direct
            // property-list call bypasses that failure. Publish a real
            // NSPasteboardItem with its documented data-provider callbacks;
            // retain the received namespace/delegate on the item. Its browser
            // URL promise is fulfilled only when the target supplies a location.
            // Making these flavors eager would block publication on remote
            // reads. Only this foreign linked virtual-file drag uses the relay;
            // source ordering and clipboard/file-URL behavior stay intact.
            // Private-board/independent-reader verification: macOS 26.6.2;
            // browser/Finder desktop acceptance remains a manual check.
            writers.push(ProtocolObject::from_retained(
                Modern::companion_item(writer, &board)?.into_super(),
            ));
        } else {
            writers.push(ProtocolObject::from_retained(writer));
        }
        proxies.push(proxy);
    }
    Ok((writers, proxies))
}

fn file_type(root: &FileNode) -> String {
    if root.kind == NodeKind::Directory {
        return "public.folder".into();
    }
    use objc2_core_foundation::CFString;
    #[allow(deprecated)]
    use objc2_core_services::{UTTypeCreatePreferredIdentifierForTag, kUTTagClassFilenameExtension};
    Path::new(&root.name)
        .extension()
        .and_then(|extension| extension.to_str())
        .and_then(|extension| {
            // SAFETY: registry-only filename metadata, with no source body access.
            #[allow(deprecated)]
            unsafe {
                UTTypeCreatePreferredIdentifierForTag(
                    kUTTagClassFilenameExtension,
                    &CFString::from_str(extension),
                    None,
                )
            }
        })
        .map(|ty| ty.to_string())
        .unwrap_or_else(|| "public.data".into())
}
