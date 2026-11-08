//! Clipboard URL preparation never owns an AppKit/pasteboard thread. All native
//! types are declared once; only a file-URL read waits for this worker's result.
use super::{file_provider, files, pasteboard};
use crate::{
    ClipboardError, NativeSpec, content,
    model::{NodeId, Unavailable},
};
use std::{
    collections::HashMap,
    io,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Extension point for target-local file delivery policy. `prepare` runs on an
/// independent worker, must poll `current`, and must never dispatch synchronously
/// to AppKit. The shared whole-offer automatic cache starts immediately; other
/// payload sources can wait for `request.wait_for_demand(current)`.
pub(super) trait FileUrlSource: Send + Sync + 'static {
    fn prepare(
        &self,
        request: &FileUrlRequest,
        current: &dyn Fn() -> bool,
    ) -> Result<FileUrlPublication, ClipboardError>;
    fn published(&self, _name: Option<&str>, _count: u64) {}
    fn cancel(&self) {}
    fn cancel_on_read_timeout(&self) -> bool {
        false
    }
    fn channel(&self) -> crate::copy_diagnostics::Channel {
        crate::copy_diagnostics::Channel::FileProvider
    }
}

pub(super) struct FileUrlRequest {
    pub spec: NativeSpec,
    shared: Arc<Shared>,
}
impl FileUrlRequest {
    /// For strategies that materialize bytes: declaration is not a read demand.
    /// This worker-only wait is cancelled when a newer copy replaces this one.
    #[allow(dead_code)] // Available to demand-only sources; automatic cache intentionally skips it.
    pub fn wait_for_demand(&self, current: &dyn Fn() -> bool) -> bool {
        let mut state = self.shared.result.lock().unwrap();
        while !self.shared.demanded.load(Ordering::Acquire) && current() {
            state = self
                .shared
                .ready
                .wait_timeout(state, Duration::from_millis(10))
                .unwrap()
                .0;
        }
        current()
    }
}

pub(super) trait FileUrlLease: Send + Sync {
    /// Only unexposed URLs are retired here. Exposed URLs keep the strategy's
    /// received-file/cache lifetime, including consumers reopening a path later.
    fn cancel(&self);
}
impl FileUrlLease for file_provider::Registration {
    fn cancel(&self) {
        self.cancel();
    }
}
pub(super) struct FileUrlPublication {
    pub urls: HashMap<NodeId, String>,
    pub lease: Box<dyn FileUrlLease>,
}
struct FileProviderSource;
impl FileUrlSource for FileProviderSource {
    fn prepare(
        &self,
        request: &FileUrlRequest,
        current: &dyn Fn() -> bool,
    ) -> Result<FileUrlPublication, ClipboardError> {
        let spec = &request.spec;
        let registration = file_provider::publish_clipboard(
            spec.inventory.id.clip,
            &spec.files,
            spec.file_contents.clone().ok_or(ClipboardError::Unsupported)?,
            current,
        )?;
        let paths: Vec<_> = registration
            .entries()
            .iter()
            .filter(|entry| files::is_top_level(entry))
            .filter_map(|entry| entry.local_path.as_deref())
            .collect();
        if paths.len() != spec.inventory.roots.len() {
            registration.cancel();
            return Err(ClipboardError::Unsupported);
        }
        Ok(FileUrlPublication {
            urls: spec
                .inventory
                .roots
                .iter()
                .zip(paths)
                .map(|(node, path)| (node.id, pasteboard::file_url_string(path)))
                .collect(),
            lease: Box::new(registration),
        })
    }
}
struct Shared {
    result: Mutex<Option<Result<FileUrlPublication, ClipboardError>>>,
    ready: Condvar,
    cancelled: AtomicBool,
    demanded: AtomicBool,
    exposed: AtomicBool,
    read_failed: AtomicBool,
    current: content::ReaderGuard,
}
pub(super) struct FileUrls {
    shared: Arc<Shared>,
    source: Arc<dyn FileUrlSource>,
}
impl FileUrls {
    pub fn prepare(spec: NativeSpec, current: content::ReaderGuard) -> Result<Arc<Self>, ClipboardError> {
        // Delivery policy selection belongs here. The worker owns request/spec;
        // dropping the native owner can cancel it without waiting for a helper.
        let source: Arc<dyn FileUrlSource> = match super::small_files::SmallFiles::for_spec(&spec, current.clone())
            .map_err(|error| ClipboardError::ContentUnavailable(content::reason(&error)))?
        {
            Some(cache) => Arc::new(super::small_files::Source(cache)),
            None => Arc::new(FileProviderSource),
        };
        Self::with_source_current(spec, source, current)
            .map_err(|error| ClipboardError::ContentUnavailable(content::reason(&error)))
    }
    #[cfg(all(test, debug_assertions))]
    pub(super) fn with_source(spec: NativeSpec, source: Arc<dyn FileUrlSource>) -> io::Result<Arc<Self>> {
        Self::with_source_current(spec, source, Arc::new(|| Ok(())))
    }
    pub(super) fn with_source_current(
        spec: NativeSpec,
        source: Arc<dyn FileUrlSource>,
        current: content::ReaderGuard,
    ) -> io::Result<Arc<Self>> {
        let shared = Arc::new(Shared {
            result: Mutex::new(None),
            ready: Condvar::new(),
            cancelled: AtomicBool::new(false),
            demanded: AtomicBool::new(false),
            exposed: AtomicBool::new(false),
            read_failed: AtomicBool::new(false),
            current,
        });
        let request = FileUrlRequest {
            spec,
            shared: shared.clone(),
        };
        let worker_source = source.clone();
        let started = std::thread::Builder::new()
            .name("oink-clipboard-file-urls".into())
            .spawn(move || {
                let shared = &request.shared;
                let mut trace = crate::copy_diagnostics::Read::begin(
                    request.spec.inventory.id,
                    crate::copy_diagnostics::Stage::FilePreparation,
                    crate::model::RepresentationId(0),
                    7,
                    worker_source.channel(),
                );
                let result = worker_source.prepare(&request, &|| {
                    !shared.cancelled.load(Ordering::Acquire)
                        && !shared.read_failed.load(Ordering::Acquire)
                        && (shared.current)().is_ok()
                });
                match &result {
                    Ok(publication) => trace.success(publication.urls.values().map(|url| url.len() as u64).sum()),
                    Err(error) => trace.backend_failure(error),
                }
                let discard = {
                    let mut slot = shared.result.lock().unwrap();
                    if shared.cancelled.load(Ordering::Acquire) || shared.read_failed.load(Ordering::Acquire) {
                        result.ok()
                    } else {
                        *slot = Some(result);
                        None
                    }
                };
                shared.ready.notify_all();
                if let Some(publication) = discard {
                    publication.lease.cancel();
                }
            });
        if let Err(error) = started {
            source.cancel();
            return Err(error);
        }
        Ok(Arc::new(Self { shared, source }))
    }
    pub fn published(&self, name: Option<&str>, count: u64) {
        self.source.published(name, count);
    }
    pub fn cancel(&self) {
        self.shared.cancelled.store(true, Ordering::Release);
        let discard = {
            let mut slot = self.shared.result.lock().unwrap();
            if !self.shared.exposed.load(Ordering::Acquire) {
                slot.take().and_then(Result::ok)
            } else {
                None
            }
        };
        self.shared.ready.notify_all();
        self.source.cancel();
        if let Some(publication) = discard {
            publication.lease.cancel();
        }
    }
    pub fn read(&self, node: NodeId) -> io::Result<String> {
        self.ready_url(Some(node)).map(|url| url.expect("requested node"))
    }
    pub fn wait_ready(&self) -> io::Result<()> {
        self.ready_url(None).map(|_| ())
    }
    fn ready_url(&self, node: Option<NodeId>) -> io::Result<Option<String>> {
        // This consumes the same callback-local 5 s budget as text/image reads.
        // No catalog mutex, publication guard, agent actor or AppKit main-queue
        // task is held while waiting. The helper and network run independently.
        let budget =
            content::native_read::remote_read().ok_or_else(|| content::unavailable(Unavailable::ReadFailed))?;
        #[cfg(test)]
        {
            let shared = self.shared.clone();
            budget.wake_on_advance(move || shared.ready.notify_all());
        }
        self.shared.demanded.store(true, Ordering::Release);
        self.shared.ready.notify_all();
        let mut slot = self.shared.result.lock().unwrap();
        loop {
            if self.shared.cancelled.load(Ordering::Acquire) {
                budget.cancelled();
                return Err(content::unavailable(Unavailable::Cancelled));
            }
            if budget.remaining().is_zero() {
                budget.timed_out();
                if self.source.cancel_on_read_timeout() {
                    self.shared.read_failed.store(true, Ordering::Release);
                    *slot = Some(Err(ClipboardError::ContentUnavailable(Unavailable::NoProgress)));
                    drop(slot);
                    self.shared.ready.notify_all();
                    self.source.cancel();
                }
                return Err(content::unavailable(Unavailable::NoProgress));
            }
            match slot.as_ref() {
                Some(Ok(publication)) => {
                    let url = node
                        .map(|node| {
                            publication
                                .urls
                                .get(&node)
                                .cloned()
                                .ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))
                        })
                        .transpose()?;
                    self.shared.exposed.store(true, Ordering::Release);
                    return Ok(url);
                }
                Some(Err(error)) => {
                    return Err(content::unavailable(match error {
                        ClipboardError::ContentUnavailable(reason) => *reason,
                        ClipboardError::FileProvider(crate::FileProviderReason::Cancelled) => Unavailable::Cancelled,
                        _ => Unavailable::ReadFailed,
                    }));
                }
                None => {
                    slot = self
                        .shared
                        .ready
                        .wait_timeout(slot, budget.remaining().min(Duration::from_millis(10)))
                        .unwrap()
                        .0
                }
            }
        }
    }
}
impl Drop for FileUrls {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(all(test, debug_assertions))]
pub(super) fn run_native() {
    tests::run_native();
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::{
        content::{Provider, Translator},
        model::*,
    };
    use objc2_foundation::NSString;
    use oink_foundation::{DeviceId, clipboard::ClipPlatform};
    use std::sync::{atomic::AtomicUsize, mpsc};
    struct Source(Inventory);
    impl Provider for Source {
        fn inventory(&self) -> Inventory {
            self.0.clone()
        }
        fn open(&self, _: RepresentationId) -> io::Result<crate::FileReader> {
            Ok(Box::new(io::Cursor::new(b"new text")))
        }
        fn translator(&self) -> Arc<dyn Translator> {
            panic!("same-platform test")
        }
    }
    fn spec() -> NativeSpec {
        let root = FileNode {
            id: NodeId(1),
            parent: None,
            name: "test.txt".into(),
            kind: NodeKind::File,
            size: ContentSize::Known(8),
            modified_ms: None,
            version: 1,
            exec_bits: None,
        };
        let rep = |id, name: &str, data, standard| Representation {
            id: RepresentationId(id),
            format: NativeFormat::Mac(name.into()),
            medium: Medium::Bytes,
            aspect: 1,
            index: -1,
            standard,
            data,
        };
        let inventory = Inventory {
            id: OfferId {
                clip: ClipId {
                    origin: DeviceId([3; 32]),
                    seq: 1,
                },
                generation: 1,
            },
            platform: ClipPlatform::Mac,
            roots: vec![root],
            items: vec![ContentItem {
                id: ItemId(1),
                file: Some(NodeId(1)),
                virtual_files: false,
                representations: vec![
                    rep(1, "public.file-url", RepresentationData::File(NodeId(1)), None),
                    rep(
                        2,
                        "public.utf8-plain-text",
                        RepresentationData::Reader {
                            size: ContentSize::Unknown,
                        },
                        Some(StandardFormat::Text),
                    ),
                    rep(
                        3,
                        "public.png",
                        RepresentationData::Reader {
                            size: ContentSize::Unknown,
                        },
                        Some(StandardFormat::Png),
                    ),
                ],
            }],
        };
        inventory.validate().unwrap();
        NativeSpec {
            contents: Arc::new(Source(inventory.clone())),
            inventory,
            files: vec![],
            file_contents: None,
            sensitivity: crate::Sensitivity::Normal,
        }
    }
    struct Lease(Arc<AtomicUsize>);
    impl FileUrlLease for Lease {
        fn cancel(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Gate {
        released: AtomicBool,
        calls: AtomicUsize,
        retired: Arc<AtomicUsize>,
        demanded: mpsc::Sender<()>,
        done: mpsc::Sender<()>,
    }
    impl FileUrlSource for Gate {
        fn prepare(
            &self,
            request: &FileUrlRequest,
            current: &dyn Fn() -> bool,
        ) -> Result<FileUrlPublication, ClipboardError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !request.wait_for_demand(current) {
                let _ = self.done.send(());
                return Err(ClipboardError::ContentUnavailable(Unavailable::Cancelled));
            }
            self.demanded.send(()).unwrap();
            while !self.released.load(Ordering::Acquire) && current() {
                std::thread::sleep(Duration::from_millis(1));
            }
            let result = if current() {
                Ok(FileUrlPublication {
                    urls: [(NodeId(1), "file:///tmp/test.txt".into())].into(),
                    lease: Box::new(Lease(self.retired.clone())),
                })
            } else {
                Err(ClipboardError::ContentUnavailable(Unavailable::Cancelled))
            };
            let _ = self.done.send(());
            result
        }
    }
    fn gate() -> (Arc<Gate>, mpsc::Receiver<()>, mpsc::Receiver<()>) {
        let (demanded, demand) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        (
            Arc::new(Gate {
                released: AtomicBool::new(false),
                calls: AtomicUsize::new(0),
                retired: Arc::new(AtomicUsize::new(0)),
                demanded,
                done,
            }),
            demand,
            finished,
        )
    }
    #[test]
    fn all_types_are_declared_once_and_text_image_do_not_wait_for_files() {
        let spec = spec();
        let (source, demand, done) = gate();
        let urls = FileUrls::with_source(spec.clone(), source.clone()).unwrap();
        let board = crate::macos::test_pasteboard::new();
        let name = board.name().to_string();
        let count =
            crate::macos::content::write_lazy(Some(&name), &spec, &HashMap::new(), Some(urls.clone()), &|| Ok(()))
                .unwrap();
        let item = board.pasteboardItems().unwrap().objectAtIndex(0);
        let types = item.types().iter().map(|t| t.to_string()).collect::<Vec<_>>();
        assert_eq!(
            types,
            ["public.file-url", "public.utf8-plain-text", "public.png", "public.tiff"]
        );
        assert_eq!(
            item.stringForType(&NSString::from_str("public.utf8-plain-text"))
                .unwrap()
                .to_string(),
            "new text"
        );
        assert_eq!(
            item.dataForType(&NSString::from_str("public.png")).unwrap().to_vec(),
            b"new text"
        );
        assert!(
            demand.try_recv().is_err(),
            "enumerating/reading other types cannot demand files"
        );
        let releaser = std::thread::spawn(move || {
            demand.recv_timeout(Duration::from_secs(2)).unwrap();
            source.released.store(true, Ordering::Release);
        });
        assert_eq!(
            item.stringForType(&NSString::from_str("public.file-url"))
                .unwrap()
                .to_string(),
            "file:///tmp/test.txt"
        );
        releaser.join().unwrap();
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            board.changeCount() as u64,
            count,
            "fulfilment cannot append or republish types"
        );
        assert_eq!(item.types().iter().map(|t| t.to_string()).collect::<Vec<_>>(), types);
        board.clearContents();
    }
    fn virtual_spec(leases: Arc<AtomicUsize>) -> NativeSpec {
        struct Files {
            root: FileNode,
            leases: Arc<AtomicUsize>,
        }
        impl content::namespace::Namespace for Files {
            fn roots(&self) -> Vec<FileNode> {
                vec![self.root.clone()]
            }
            fn native_lease(&self) -> io::Result<content::namespace::NativeLease> {
                self.leases.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(()))
            }
            fn list(&self, _: NodeId, _: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
                panic!("no file listing")
            }
            fn select(&self, _: NodeId) -> io::Result<FileNode> {
                panic!("no selection")
            }
            fn open(&self, _: NodeId, _: u64, _: u64) -> io::Result<crate::FileReader> {
                panic!("no file body")
            }
        }
        let mut spec = spec();
        spec.inventory.platform = ClipPlatform::Windows;
        spec.inventory.items[0].virtual_files = true;
        for (rep, format) in spec.inventory.items[0].representations.iter_mut().zip([
            NativeFormat::WindowsPredefined(15),
            NativeFormat::WindowsPredefined(13),
            NativeFormat::WindowsRegistered("PNG".into()),
        ]) {
            rep.format = format;
            rep.medium = Medium::HGlobal;
        }
        spec.contents = Arc::new(Source(spec.inventory.clone()));
        let namespace = Arc::new(Files {
            root: spec.inventory.roots[0].clone(),
            leases: leases.clone(),
        });
        spec.file_contents =
            Some(content::namespace::Files::new(namespace, crate::sync::files::TargetOs::MacOs).unwrap());
        spec
    }
    #[test]
    fn foreign_virtual_file_promise_wrapper_does_not_wait_or_lease_during_declaration() {
        let leases = Arc::new(AtomicUsize::new(0));
        let spec = virtual_spec(leases.clone());
        let (source, demand, done) = gate();
        let urls = FileUrls::with_source(spec.clone(), source.clone()).unwrap();
        let board = crate::macos::test_pasteboard::new();
        let name = board.name().to_string();
        let count =
            crate::macos::content::write_lazy(Some(&name), &spec, &HashMap::new(), Some(urls.clone()), &|| Ok(()))
                .unwrap();
        assert_eq!(leases.load(Ordering::SeqCst), 0, "native copy is not bound yet");
        let item = board.pasteboardItems().unwrap().objectAtIndex(0);
        assert!(item.types().iter().any(|ty| ty.to_string() == "public.file-url"));
        assert_eq!(
            item.stringForType(&NSString::from_str("public.utf8-plain-text"))
                .unwrap()
                .to_string(),
            "new text"
        );
        assert!(demand.try_recv().is_err());
        let releaser = std::thread::spawn(move || {
            demand.recv_timeout(Duration::from_secs(2)).unwrap();
            source.released.store(true, Ordering::Release);
        });
        assert_eq!(
            item.stringForType(&NSString::from_str("public.file-url"))
                .unwrap()
                .to_string(),
            "file:///tmp/test.txt"
        );
        releaser.join().unwrap();
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(board.changeCount() as u64, count);
        board.clearContents();
    }

    #[test]
    fn timeout_uses_callback_budget_and_retry_reuses_preparation() {
        let (source, demand, done) = gate();
        let urls = FileUrls::with_source(spec(), source.clone()).unwrap();
        let clock = Arc::new(content::native_read::TestClock::default());
        clock.scope(|| {
            let _callback = content::native_read::Callback::begin();
            clock.expire();
            assert_eq!(
                content::reason(&urls.read(NodeId(1)).unwrap_err()),
                Unavailable::NoProgress
            );
        });
        demand.recv_timeout(Duration::from_secs(2)).unwrap();
        source.released.store(true, Ordering::Release);
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..2 {
            let _callback = content::native_read::Callback::begin();
            assert_eq!(urls.read(NodeId(1)).unwrap(), "file:///tmp/test.txt");
        }
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        urls.cancel();
        assert_eq!(
            source.retired.load(Ordering::SeqCst),
            0,
            "exposed paths retain their consumer lifecycle"
        );
    }
    #[test]
    fn replacement_cancels_waiting_read_and_obsolete_worker() {
        let (source, demand, done) = gate();
        let urls = FileUrls::with_source(spec(), source.clone()).unwrap();
        let read_urls = urls.clone();
        let reader = std::thread::spawn(move || {
            let _callback = content::native_read::Callback::begin();
            content::reason(&read_urls.read(NodeId(1)).unwrap_err())
        });
        demand.recv_timeout(Duration::from_secs(2)).unwrap();
        let board = crate::macos::test_pasteboard::new();
        let clipboard = crate::macos::MacClipboard {
            name: Some(board.name().to_string()),
            watcher: Mutex::new(None),
            registration: Arc::new(Mutex::new(Some((
                board.changeCount() as u64,
                crate::macos::FilePublication::Lazy(urls.clone()),
            )))),
            local: Arc::new(Mutex::new(None)),
        };
        crate::ClipboardBackend::write(
            &clipboard,
            &[crate::ClipData::Text("next copy".into())],
            crate::Sensitivity::Normal,
        )
        .unwrap();
        assert_eq!(reader.join().unwrap(), Unavailable::Cancelled);
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(source.retired.load(Ordering::SeqCst), 0);
        let (next, demand, done) = gate();
        next.released.store(true, Ordering::Release);
        let current = FileUrls::with_source(spec(), next).unwrap();
        let _callback = content::native_read::Callback::begin();
        assert!(current.read(NodeId(1)).is_ok());
        demand.recv_timeout(Duration::from_secs(2)).unwrap();
        done.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    #[test]
    fn ready_unexposed_publication_is_retired_once_on_replacement() {
        struct Ready(Arc<AtomicUsize>);
        impl FileUrlSource for Ready {
            fn prepare(&self, _: &FileUrlRequest, _: &dyn Fn() -> bool) -> Result<FileUrlPublication, ClipboardError> {
                Ok(FileUrlPublication {
                    urls: [(NodeId(1), "file:///tmp/test.txt".into())].into(),
                    lease: Box::new(Lease(self.0.clone())),
                })
            }
        }
        let retired = Arc::new(AtomicUsize::new(0));
        let urls = FileUrls::with_source(spec(), Arc::new(Ready(retired.clone()))).unwrap();
        let mut slot = urls.shared.result.lock().unwrap();
        while slot.is_none() {
            slot = urls.shared.ready.wait(slot).unwrap();
        }
        drop(slot);
        urls.cancel();
        urls.cancel();
        assert_eq!(retired.load(Ordering::SeqCst), 1);
    }

    /// Runs through the existing no-libtest AppKit harness. The independent
    /// child reads only a private board while the publisher pumps its run loop.
    pub(super) fn run_native() {
        use objc2::MainThreadMarker;
        use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSPasteboard};
        use objc2_foundation::{NSDate, NSRunLoop};
        use std::{
            process::{Command, Stdio},
            time::Instant,
        };
        const BOARD: &str = "OINK_TEST_LAZY_CLIPBOARD_BOARD";
        let app = NSApplication::sharedApplication(MainThreadMarker::new().expect("main-thread harness"));
        app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
        objc2::rc::autoreleasepool(|_| {
            if let Ok(name) = std::env::var(BOARD) {
                let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&name));
                let item = board.pasteboardItems().unwrap().objectAtIndex(0);
                let types = item.types().iter().map(|ty| ty.to_string()).collect::<Vec<_>>();
                assert!(types.contains(&"public.file-url".into()));
                assert!(types.contains(&"public.png".into()));
                assert_eq!(
                    item.stringForType(&NSString::from_str("public.utf8-plain-text"))
                        .unwrap()
                        .to_string(),
                    "new text"
                );
                assert_eq!(
                    item.dataForType(&NSString::from_str("public.png")).unwrap().to_vec(),
                    b"new text"
                );
                assert_eq!(
                    item.stringForType(&NSString::from_str("public.file-url"))
                        .unwrap()
                        .to_string(),
                    "file:///tmp/test.txt"
                );
                return;
            }
            for virtual_files in [false, true] {
                let spec = if virtual_files {
                    virtual_spec(Arc::new(AtomicUsize::new(0)))
                } else {
                    spec()
                };
                let (source, demand, done) = gate();
                let urls = FileUrls::with_source(spec.clone(), source.clone()).unwrap();
                let board = crate::macos::test_pasteboard::new();
                let name = board.name().to_string();
                let count =
                    crate::macos::content::write_lazy(Some(&name), &spec, &HashMap::new(), Some(urls), &|| Ok(()))
                        .unwrap();
                assert!(demand.try_recv().is_err(), "declaration must not demand file URLs");
                let release = std::thread::spawn(move || {
                    demand.recv_timeout(Duration::from_secs(10)).unwrap();
                    source.released.store(true, Ordering::Release);
                });
                let mut child = Command::new(std::env::current_exe().unwrap())
                    .arg("--lazy-clipboard")
                    .env(BOARD, name)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while child.try_wait().unwrap().is_none() {
                    if Instant::now() >= deadline {
                        child.kill().unwrap();
                        panic!("independent lazy clipboard reader timed out");
                    }
                    NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.01));
                }
                let output = child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "independent reader: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                release.join().unwrap();
                done.recv_timeout(Duration::from_secs(2)).unwrap();
                assert_eq!(board.changeCount() as u64, count);
                board.clearContents();
                eprintln!(
                    "LAZY_CLIPBOARD_NATIVE independent reader passed: all types declared once; text/image first reads; file URL waits on independent worker; count unchanged"
                );
            }
        });
    }

    #[test]
    fn declaration_guard_failure_preserves_newer_clipboard_and_cancels_preparation() {
        let (source, _demand, done) = gate();
        let spec = spec();
        let urls = FileUrls::with_source(spec.clone(), source).unwrap();
        let board = crate::macos::test_pasteboard::new();
        let name = board.name().to_string();
        let result =
            crate::macos::content::write_lazy(Some(&name), &spec, &HashMap::new(), Some(urls.clone()), &|| {
                board.clearContents();
                board.setString_forType(
                    &NSString::from_str("newer local"),
                    &NSString::from_str("public.utf8-plain-text"),
                );
                Err(content::unavailable(Unavailable::Cancelled))
            });
        assert!(matches!(result, Err(ClipboardError::Busy)));
        assert_eq!(
            board
                .stringForType(&NSString::from_str("public.utf8-plain-text"))
                .unwrap()
                .to_string(),
            "newer local"
        );
        urls.cancel();
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        board.clearContents();
    }
}
