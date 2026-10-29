use super::*;
use oink_clipboard::dnd::{
    DndBackend, DndSink, DragOperation, DragOps, DragSpec, Injector, PlatformDndEvent, SourcePress, SourceStart,
};
use oink_clipboard::{ClipboardBackend, ClipboardError};
use oink_foundation::input::MouseButton;

#[derive(Clone, Default)]
struct Drag {
    received: Arc<Mutex<Vec<Arc<super::received::Received>>>>,
    export_required: Arc<std::sync::atomic::AtomicBool>,
    export_operation: Arc<Mutex<Option<DragOperation>>>,
    export_provider: Arc<Mutex<Option<Arc<dyn oink_clipboard::content::Provider>>>>,
    native: Arc<Mutex<Option<DragOperation>>>,
    probing: Arc<std::sync::atomic::AtomicBool>,
    probe_operation: Arc<Mutex<Option<DragOperation>>>,
    native_items: Arc<Mutex<Vec<ClipData>>>,
    native_clipboard: Arc<FakeClipboard>,
    image: Arc<Mutex<Option<oink_foundation::dnd::DragImage>>>,
    sink: Arc<Mutex<Option<DndSink>>>,
    inject: Arc<Mutex<Option<Injector>>>,
    offer: Arc<Mutex<Option<Vec<ClipData>>>>,
    began: Arc<Mutex<Option<DragSpec>>>,
    begin_stage: Arc<std::sync::atomic::AtomicU8>,
    begin_returned: Arc<std::sync::atomic::AtomicBool>,
    begin_error: Arc<Mutex<Option<String>>>,
    calls: Arc<Mutex<Vec<&'static str>>>,
    begin_gate: Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
    cancel_gate: Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
    press_gate: Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
    input: Arc<Mutex<Option<FakeInput>>>,
    source_start: Arc<Mutex<Option<SourceStart>>>,
    source_press: Arc<Mutex<Option<SourcePress>>>,
    dropped: Arc<std::sync::atomic::AtomicU64>,
    critical_us: Arc<Mutex<Vec<u64>>>,
    start_gate: Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
    begin_return: Arc<Mutex<Option<BoundaryGate>>>,
    resume_offer: Arc<Mutex<Option<BoundaryGate>>>,
    resume_return: Arc<Mutex<Option<BoundaryGate>>>,
    unprobe_return: Arc<Mutex<Option<BoundaryGate>>>,
    // Model native completion arriving after FinishSource has returned.
    defer_source_end: Arc<std::sync::atomic::AtomicBool>,
    pending_source_end: Arc<Mutex<Option<PlatformDndEvent>>>,
}
struct BoundaryGate {
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}
fn watchdog_read<T>(weak: &std::sync::Weak<Mutex<T>>, read: impl FnOnce(&T) -> String) -> String {
    let Some(value) = weak.upgrade() else {
        return "gone".into();
    };
    // These probes run immediately before exit. Releasing the last native
    // provider/reader reference here could itself block in teardown.
    let value = std::mem::ManuallyDrop::new(value);
    match value.try_lock() {
        Ok(value) => read(&value),
        Err(std::sync::TryLockError::WouldBlock) => "unavailable: WouldBlock".into(),
        Err(std::sync::TryLockError::Poisoned(_)) => "unavailable: Poisoned".into(),
    }
}
fn stop_at_boundary(slot: &Mutex<Option<BoundaryGate>>) {
    if let Some(gate) = slot.lock().unwrap().take() {
        gate.entered.send(()).unwrap();
        let _ = gate.release.recv();
    }
}
fn boundary(slot: &Mutex<Option<BoundaryGate>>) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (entered, reached) = std::sync::mpsc::channel();
    let (release, waiting) = std::sync::mpsc::channel();
    *slot.lock().unwrap() = Some(BoundaryGate {
        entered,
        release: waiting,
    });
    (reached, release)
}
async fn reached(receiver: std::sync::mpsc::Receiver<()>) {
    tokio::task::spawn_blocking(move || receiver.recv().unwrap())
        .await
        .unwrap();
}
impl DndBackend for Drag {
    fn source_image(&self) -> Option<oink_foundation::dnd::DragImage> {
        self.image.lock().unwrap().clone()
    }
    fn source_requires_export(&self, _: DragOperation, _: bool) -> bool {
        self.export_required.load(std::sync::atomic::Ordering::Acquire)
    }
    fn export_source(
        &self,
        operation: DragOperation,
        _: std::time::Instant,
        release: &dyn Fn() -> Result<(), String>,
    ) -> bool {
        assert_eq!(self.native.lock().unwrap().take(), Some(operation));
        *self.export_operation.lock().unwrap() = Some(operation);
        release().unwrap();
        self.calls.lock().unwrap().push("export");
        true
    }
    fn source_content(&self, _: DragOperation) -> Option<Arc<dyn oink_clipboard::content::Provider>> {
        self.export_provider
            .lock()
            .unwrap()
            .clone()
            .or_else(|| self.native_clipboard.capture(Path::new(".")).ok()?.content)
    }
    fn forget_content(&self, _: DragOperation) {
        self.export_provider.lock().unwrap().take();
        self.export_operation.lock().unwrap().take();
        self.calls.lock().unwrap().push("forget");
    }
    fn set_source_press(&self, press: SourcePress) {
        *self.source_press.lock().unwrap() = Some(press);
    }
    fn set_source_start(&self, start: SourceStart) {
        *self.source_start.lock().unwrap() = Some(start);
    }
    fn start(&self, sink: DndSink, inject: Injector) -> Result<(), ClipboardError> {
        *self.sink.lock().unwrap() = Some(sink);
        *self.inject.lock().unwrap() = Some(inject);
        if let Some(gate) = self.start_gate.lock().unwrap().take() {
            gate.recv().expect("release delayed backend start");
        }
        Ok(())
    }
    fn probe(&self, _: Point) {
        self.probing.store(true, std::sync::atomic::Ordering::Release);
        self.calls.lock().unwrap().push("probe");
        if let Some(items) = self.offer.lock().unwrap().take() {
            let operation = DragOperation::new();
            *self.probe_operation.lock().unwrap() = Some(operation);
            *self.native.lock().unwrap() = Some(operation);
            *self.native_items.lock().unwrap() = items.clone();
            self.native_clipboard.copy(items.clone(), Sensitivity::Normal);
            let items = self.native_clipboard.capture(Path::new(".")).unwrap().snapshot.items;
            self.sink.lock().unwrap().as_ref().unwrap()(PlatformDndEvent::Detected {
                operation,
                items,
                ops: DragOps::COPY,
            });
        }
    }
    fn unprobe(&self) {
        self.probing.store(false, std::sync::atomic::Ordering::Release);
        stop_at_boundary(&self.unprobe_return);
        *self.probe_operation.lock().unwrap() = None;
    }
    fn unprobe_source(&self, operation: DragOperation) {
        let current = *self.probe_operation.lock().unwrap() == Some(operation);
        if current {
            self.unprobe();
        }
    }
    fn hold_source(&self, operation: DragOperation) -> Result<oink_foundation::dnd::DragAnchor, ClipboardError> {
        assert_eq!(*self.native.lock().unwrap(), Some(operation));
        if !self.probing.load(std::sync::atomic::Ordering::Acquire) {
            self.calls.lock().unwrap().push("hold_failed");
            return Err(ClipboardError::Unsupported);
        }
        self.calls.lock().unwrap().push("hold");
        Ok(oink_foundation::dnd::DragAnchor {
            generation: operation.sequence,
            at: self.input.lock().unwrap().as_ref().unwrap().cursor_position().unwrap(),
            surface: 0,
        })
    }
    fn resume_source(&self, operation: DragOperation) -> bool {
        assert_eq!(*self.native.lock().unwrap(), Some(operation));
        self.unprobe();
        self.calls.lock().unwrap().push("resume");
        stop_at_boundary(&self.resume_offer);
        *self.offer.lock().unwrap() = Some(self.native_items.lock().unwrap().clone());
        stop_at_boundary(&self.resume_return);
        true
    }
    fn finish_source(
        &self,
        operation: DragOperation,
        accepted: bool,
        release: &dyn Fn() -> Result<(), String>,
    ) -> bool {
        if self.export_operation.lock().unwrap().as_ref() == Some(&operation) {
            self.sink.lock().unwrap().as_ref().unwrap()(PlatformDndEvent::SourceExported {
                operation,
                result: Err(oink_foundation::dnd::ExportFailure::Cancelled),
            });
            return true;
        }
        if *self.native.lock().unwrap() != Some(operation) {
            return false;
        }
        self.calls
            .lock()
            .unwrap()
            .push(if accepted { "commit" } else { "source_cancel" });
        release().unwrap();
        *self.native.lock().unwrap() = None;
        let ended = PlatformDndEvent::SourceEnded { operation, accepted };
        if self.defer_source_end.load(std::sync::atomic::Ordering::Acquire) {
            *self.pending_source_end.lock().unwrap() = Some(ended);
        } else {
            self.sink.lock().unwrap().as_ref().unwrap()(ended);
        }
        true
    }
    fn source_completion_is_async(&self) -> bool {
        self.defer_source_end.load(std::sync::atomic::Ordering::Acquire)
    }
    fn release(&self) -> bool {
        self.calls.lock().unwrap().push("release");
        true
    }
    fn begin(&self, spec: DragSpec) -> Result<(), ClipboardError> {
        self.begin_returned.store(false, std::sync::atomic::Ordering::Release);
        self.begin_stage.store(1, std::sync::atomic::Ordering::Release);
        let result = self.begin_observed(spec);
        *self.begin_error.lock().unwrap() = result.as_ref().err().map(ToString::to_string);
        self.begin_returned.store(true, std::sync::atomic::Ordering::Release);
        result
    }
    fn cancel(&self) -> bool {
        self.calls.lock().unwrap().push("cancel");
        if let Some(gate) = self.cancel_gate.lock().unwrap().take() {
            gate.recv().expect("release delayed Cancel");
        }
        self.inject_button(false);
        self.end(false);
        true
    }
}
impl Drag {
    async fn take_resumed_offer(&self, phase: &str) -> Vec<ClipData> {
        // The resume call marker precedes native offer publication. Consume the
        // actual offer while checking readiness, releasing its mutex each poll.
        // If it never arrives, the scenario watchdog dumps state without an
        // Option panic under this mutex poisoning the native callback thread.
        let mut offer = None;
        common::watchdog::until(phase, || {
            offer = self.offer.lock().unwrap().take();
            offer.is_some()
        })
        .await;
        offer.expect("watchdog wait observed the resumed native offer")
    }

    fn begin_observed(&self, spec: DragSpec) -> Result<(), ClipboardError> {
        self.calls.lock().unwrap().push("begin");
        if let Some(gate) = self.begin_gate.lock().unwrap().take() {
            gate.recv().expect("release delayed Begin");
        }
        let mut received = false;
        let mut press_result = Ok(());
        let started = Instant::now();
        let mut critical = || {
            self.begin_stage.store(3, std::sync::atomic::Ordering::Release);
            let input = self.input.lock().unwrap().clone().unwrap();
            let centre = input.cursor_position().unwrap();
            self.calls.lock().unwrap().push("placed");
            if let Some(gate) = self.press_gate.lock().unwrap().take() {
                gate.recv().expect("release source press");
            }
            let cursor = input.cursor_position().unwrap();
            press_result = self
                .source_press
                .lock()
                .unwrap()
                .as_ref()
                .expect("operation-keyed source press")(spec.operation, cursor);
            received = press_result.is_ok() && (cursor.x - centre.x).abs() <= 24. && (cursor.y - centre.y).abs() <= 24.;
        };
        self.begin_stage.store(2, std::sync::atomic::Ordering::Release);
        if let Some(start) = self.source_start.lock().unwrap().clone() {
            let dropped = start(&mut critical).map_err(ClipboardError::Platform)?;
            self.dropped.store(dropped, std::sync::atomic::Ordering::Relaxed);
        } else {
            critical();
        }
        self.begin_stage.store(4, std::sync::atomic::Ordering::Release);
        self.critical_us
            .lock()
            .unwrap()
            .push(started.elapsed().as_micros() as u64);
        press_result.map_err(ClipboardError::Platform)?;
        if !received {
            return Err(ClipboardError::Platform("source press missed 48px window".into()));
        }
        if let Some(namespace) = spec.files.as_ref().and_then(|files| files.namespace()) {
            self.begin_stage.store(5, std::sync::atomic::Ordering::Release);
            let received = super::received::Received::bind(namespace)
                .map_err(|error| ClipboardError::Platform(error.to_string()))?;
            self.received.lock().unwrap().push(received);
        }
        *self.began.lock().unwrap() = Some(spec);
        self.begin_stage.store(6, std::sync::atomic::Ordering::Release);
        stop_at_boundary(&self.begin_return);
        Ok(())
    }
    fn register_watchdog(&self, dev: &Dev) {
        let node = dev.node.dnd_watchdog_probe_for_test();
        let input = dev.input.watchdog_probe_for_test();
        let native = self.watchdog_probe();
        common::watchdog::on_timeout(format!("DND {} {:?}", dev.name, dev.node.id()), move || {
            format!("{}\n{}\n{}", node(), input(), native())
        });
    }
    fn watchdog_probe(&self) -> impl Fn() -> String + Send + Sync + 'static {
        let stage = Arc::downgrade(&self.begin_stage);
        let returned = Arc::downgrade(&self.begin_returned);
        let error = Arc::downgrade(&self.begin_error);
        let began = Arc::downgrade(&self.began);
        let calls = Arc::downgrade(&self.calls);
        let native = Arc::downgrade(&self.native);
        let exported = Arc::downgrade(&self.export_operation);
        let provider = Arc::downgrade(&self.export_provider);
        let received = Arc::downgrade(&self.received);
        let offer = Arc::downgrade(&self.offer);
        let probing = Arc::downgrade(&self.probing);
        let probe_operation = Arc::downgrade(&self.probe_operation);
        move || {
            let stage = stage
                .upgrade()
                .map(|s| match s.load(std::sync::atomic::Ordering::Acquire) {
                    0 => "idle",
                    1 => "begin entered",
                    2 => "source start wait",
                    3 => "source press",
                    4 => "press complete",
                    5 => "binding received",
                    6 => "native published",
                    _ => "unknown",
                });
            let returned = returned.upgrade().map(|v| v.load(std::sync::atomic::Ordering::Acquire));
            let probing = probing.upgrade().map(|v| v.load(std::sync::atomic::Ordering::Acquire));
            format!(
                "fixture begin_stage={stage:?} begin_returned={returned:?} begin_error={}\nbegan={} calls={} native_source={} export={} export_provider={}\nprobing={probing:?} probe_operation={} next_probe_offer={}\nreceived={}",
                watchdog_read(&error, |v| format!("{v:?}")),
                watchdog_read(&began, |v| format!(
                    "{:?}",
                    v.as_ref().map(|s| (s.operation, s.files.is_some(), s.native.is_some()))
                )),
                watchdog_read(&calls, |v| format!("{v:?}")),
                watchdog_read(&native, |v| format!("{v:?}")),
                watchdog_read(&exported, |v| format!("{v:?}")),
                watchdog_read(&provider, |v| v.is_some().to_string()),
                watchdog_read(&probe_operation, |v| format!("{v:?}")),
                watchdog_read(&offer, |v| format!("{:?}", v.as_ref().map(Vec::len))),
                watchdog_read(&received, |v| v
                    .iter()
                    .map(|r| r.watchdog_snapshot())
                    .collect::<Vec<_>>()
                    .join("\n"))
            )
        }
    }
    fn exported(&self, provider: Arc<dyn oink_clipboard::content::Provider>) {
        *self.export_provider.lock().unwrap() = Some(provider);
        let operation = self.export_operation.lock().unwrap().unwrap();
        self.sink.lock().unwrap().as_ref().unwrap()(PlatformDndEvent::SourceExported {
            operation,
            result: Ok(()),
        });
    }
    fn inject_button(&self, pressed: bool) {
        self.inject.lock().unwrap().as_ref().unwrap()(&InjectEvent::Button {
            button: MouseButton::Left,
            pressed,
        })
        .unwrap();
    }
    async fn attach(dev: &Dev) -> Self {
        let drag = Self::default();
        *drag.input.lock().unwrap() = Some(dev.input.clone());
        drag.register_watchdog(dev);
        dev.node.start_dnd_backend(Box::new(drag.clone())).unwrap();
        drag.wait_started(dev).await;
        drag
    }
    async fn wait_started(&self, dev: &Dev) {
        // start() installs the sink before it returns. Only the input owner's
        // opening proves glue publication and DndAvailable have both completed.
        common::watchdog::until("input owner published native DnD capability", || {
            dev.node
                .input_thread()
                .unwrap()
                .opening_state(true)
                .iter()
                .any(|message| {
                    matches!(
                        message,
                        oink_routing::Packet::Dnd(oink_routing::dnd::DndMsg::Caps { enabled: true })
                    )
                })
        })
        .await;
    }
    fn end(&self, dropped: bool) {
        if let Some(spec) = self.began.lock().unwrap().take() {
            self.sink.lock().unwrap().as_ref().unwrap()(PlatformDndEvent::Ended {
                operation: spec.operation,
                dropped,
            });
        }
    }
}

#[test]
fn watchdog_dumps_blocked_dnd_fixture() {
    const CHILD: &str = "OINK_DND_WATCHDOG_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let dev = start(root.path(), "watchdog").await;
            let drag = Drag::attach(&dev).await;
            // Arm only after startup: this tests failure diagnostics, not a
            // scheduling budget or a reproduction of the original hang.
            let _scenario = common::watchdog::Scenario::with_limit(
                format!("{}::watchdog fixture", module_path!()),
                Duration::from_millis(50),
                "blocked DND fixture",
            );
            drag.register_watchdog(&dev);
            let mut waiting =
                std::pin::pin!(drag.take_resumed_offer("blocked DND fixture: waiting for resumed source offer"));
            let early = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(std::future::Future::poll(waiting.as_mut(), cx).is_ready())
            })
            .await;
            assert!(!early, "a missing recross offer must wait for watchdog diagnostics");
            let _began = drag.began.lock().unwrap();
            let _calls = drag.calls.lock().unwrap();
            loop {
                std::thread::park();
            }
        });
        unreachable!();
    }
    let test = format!("{}::watchdog_dumps_blocked_dnd_fixture", module_path!());
    let test = test.split_once("::").unwrap().1;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("DND diagnostics prevented watchdog termination");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(101));
    let message = String::from_utf8(output.stderr).unwrap();
    for expected in [
        "SCENARIO WATCHDOG",
        "blocked DND fixture",
        "WATCHDOG DIAGNOSTIC: DND",
        "input_status=",
        "target_grant=",
        "native_publications=",
        "admission_leases=",
        "source_registry leases=",
        "grants state=",
        "native_input=",
        "paused:",
        "begin_stage=",
        "begin_returned=",
        "begin_error=",
        "waiting for resumed source offer",
        "next_probe_offer=None",
        "began=unavailable: WouldBlock",
        "calls=unavailable: WouldBlock",
    ] {
        assert!(message.contains(expected), "missing {expected}: {message}");
    }
    // A stalled input owner is a supported diagnostic result, including under
    // heavy load; the probe's 50 ms bound is not a scheduling assertion.
    assert!(
        message.contains("router_dnd unavailable:")
            || (message.contains("router_dnd state=") && message.contains("source_sessions=")),
        "{message}"
    );
}

#[test]
fn drag_attachment_waits_for_backend_start_to_return() {
    common::watchdog::run(
        module_path!(),
        "drag_attachment_waits_for_backend_start_to_return",
        Some(4),
        async {
            let root = tempfile::tempdir().unwrap();
            let dev = start(root.path(), "gated").await;
            let drag = Drag::default();
            drag.register_watchdog(&dev);
            let (release, gate) = std::sync::mpsc::channel();
            *drag.start_gate.lock().unwrap() = Some(gate);
            dev.node.start_dnd_backend(Box::new(drag.clone())).unwrap();
            common::watchdog::until("backend start published sink while gated", || {
                drag.sink.lock().unwrap().is_some()
            })
            .await;
            // Poll the readiness check once, after sink publication and before
            // allowing start() to return. No scheduling deadline decides this.
            let mut waiting = std::pin::pin!(drag.wait_started(&dev));
            let early = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(std::future::Future::poll(waiting.as_mut(), cx).is_ready())
            })
            .await;
            release.send(()).unwrap();
            if !early {
                waiting.await;
            }
            dev.node.shutdown().await;
            assert!(!early, "attachment returned while backend start was still gated");
        },
    );
}
fn button(input: &FakeInput, pressed: bool) {
    input.feed(&CapturedEvent {
        physical: true,
        event: InputEvent::Button {
            button: MouseButton::Left,
            pressed,
        },
        source: SourceKind::Mouse,
        time_us: 0,
        cursor: None,
    });
}
async fn dragged_items(drag: &Drag) -> Vec<ClipData> {
    let native = drag.began.lock().unwrap().as_ref().unwrap().native.clone().unwrap();
    tokio::task::spawn_blocking(move || {
        let target = FakeClipboard::new();
        target.set_virtual_files(true);
        target.write_native(native).unwrap();
        target.items()
    })
    .await
    .unwrap()
}
async fn begin_drag(a: &Dev, b: &Dev, source: &Drag, target: &Drag, items: Vec<ClipData>) {
    *source.offer.lock().unwrap() = Some(items);
    button(&a.input, true);
    cross(a, b, 1.).await;
    common::watchdog::until("native Begin on destination", || target.began.lock().unwrap().is_some()).await;
    assert!(source.calls.lock().unwrap().contains(&"hold"));
    assert!(!source.calls.lock().unwrap().contains(&"commit"));
}
async fn drop_drag(a: &Dev, b: &Dev, target: &Drag, source: &Drag) {
    b.input.take_injected();
    button(&a.input, false);
    common::watchdog::until("destination native Up", || {
        b.input.injected().iter().any(|e| {
            matches!(
                e,
                InjectEvent::Button {
                    button: MouseButton::Left,
                    pressed: false
                }
            )
        })
    })
    .await;
    target.end(true);
    common::watchdog::until("drag retired", || !b.node.input_status().unwrap().dnd_active).await;
    common::watchdog::until("source native commit", || source.native.lock().unwrap().is_none()).await;
    assert!(source.calls.lock().unwrap().contains(&"commit"));
}
#[test]
fn complete_text_drag_crosses_edge_and_drops() {
    common::watchdog::run(
        module_path!(),
        "complete_text_drag_crosses_edge_and_drops",
        Some(4),
        complete_text_drag_crosses_edge_and_drops_scenario(),
    );
}

async fn complete_text_drag_crosses_edge_and_drops_scenario() {
    let root = tempfile::tempdir().unwrap();
    let a = start(root.path(), "a").await;
    let b = start(root.path(), "b").await;
    let da = Drag::attach(&a).await;
    let db = Drag::attach(&b).await;
    common::pair(&a.node, &b.node).await;
    common::active(&[&a.node, &b.node]).await;
    row(&[&a, &b]).await;
    // The declaration advertises artwork explicitly. An Absent notice skips
    // PreviewRequest, so this fixture needs artwork to exercise the Low path.
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    png.extend_from_slice(&1_u32.to_be_bytes());
    png.extend_from_slice(&1_u32.to_be_bytes());
    png.resize(33, 0);
    *da.image.lock().unwrap() = Some(oink_foundation::dnd::DragImage {
        png: png.into(),
        width: 1.,
        height: 1.,
        hotspot_x: 0.,
        hotspot_y: 0.,
    });
    let items = vec![ClipData::Text("dragged text".repeat(10_000))];
    begin_drag(&a, &b, &da, &db, items.clone()).await;
    assert_eq!(dragged_items(&db).await, items);
    drop_drag(&a, &b, &db, &da).await;
    // Native Drop and delivered bytes are asserted above. Export after Drop
    // also verifies that diagnostics exclude the transferred clipboard payload.
    use std::io::Read;
    let bundle = api(&b.node, |n| n.export_diagnostics()).await.unwrap();
    let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
    let mut text = String::new();
    zip.by_name("merged.ndjson").unwrap().read_to_string(&mut text).unwrap();
    assert!(!text.contains("dragged text"));
    a.node.shutdown().await;
    b.node.shutdown().await;
}

async fn file_drag(relayed: bool) {
    use std::io::Read;
    let root = tempfile::tempdir().unwrap();
    let (a, b, relay) = if relayed {
        let (a, b, r) = relayed_trio(root.path()).await;
        (a, b, Some(r))
    } else {
        let a = start(root.path(), "a").await;
        let b = start(root.path(), "b").await;
        common::pair(&a.node, &b.node).await;
        common::active(&[&a.node, &b.node]).await;
        (a, b, None)
    };
    let da = Drag::attach(&a).await;
    let db = Drag::attach(&b).await;
    let mut devs = vec![&a, &b];
    if let Some(r) = &relay {
        devs.push(r);
    }
    row(&devs).await;
    let bytes = vec![0x5a; 2_000_000];
    let path = a.dir.join("drag.bin");
    std::fs::write(&path, &bytes).unwrap();
    let items = vec![ClipData::Files(vec![oink_clipboard::FileEntry {
        path: "drag.bin".into(),
        is_dir: false,
        size: bytes.len() as u64,
        modified_ms: None,
        local_path: Some(path),
    }])];
    begin_drag(&a, &b, &da, &db, items).await;
    let files = db
        .began
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .files
        .clone()
        .expect("remote lazy file reader");
    drop_drag(&a, &b, &db, &da).await;
    let actual = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::spawn_blocking(move || {
            let mut actual = Vec::new();
            files.open(0).unwrap().read_to_end(&mut actual).unwrap();
            actual
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(actual, bytes);
    if relayed {
        assert!(matches!(path_to(&a.node, &b.node), Some(LinkPath::Relayed { .. })));
    }
    a.node.shutdown().await;
    b.node.shutdown().await;
    if let Some(r) = relay {
        r.node.shutdown().await;
    }
}
#[test]
fn file_drop_delivers_after_pointer_returns_before_native_end() {
    common::watchdog::run(
        module_path!(),
        "file_drop_delivers_after_pointer_returns_before_native_end",
        Some(4),
        async {
            use std::io::Read;
            let root = tempfile::tempdir().unwrap();
            let (a, b, da, db) = drag_pair(root.path()).await;
            let bytes = vec![0x5a; 220_336];
            let path = a.dir.join("clip.mp4");
            std::fs::write(&path, &bytes).unwrap();
            begin_drag(
                &a,
                &b,
                &da,
                &db,
                vec![ClipData::Files(vec![oink_clipboard::FileEntry {
                    path: "clip.mp4".into(),
                    is_dir: false,
                    size: bytes.len() as u64,
                    modified_ms: None,
                    local_path: Some(path),
                }])],
            )
            .await;
            let files = db.began.lock().unwrap().as_ref().unwrap().files.clone().unwrap();
            b.input.take_injected();
            button(&a.input, false);
            common::watchdog::until("native UP before asynchronous acceptance", || has_up(&b)).await;
            // Escape after release cannot undo this drop, on either side of
            // the pointer's return while native acceptance is still pending.
            key(&a.input, 0x29, true);
            key(&a.input, 0x29, false);
            cross(&a, &a, -1.).await;
            key(&a.input, 0x29, true);
            key(&a.input, 0x29, false);
            common::watchdog::until("target processed focus return", || holder(&b.node) == Some(a.node.id())).await;
            assert!(!db.calls.lock().unwrap().contains(&"cancel"));
            assert!(!da.calls.lock().unwrap().contains(&"source_cancel"));
            db.end(true);
            common::watchdog::until("source committed after delayed native end", || {
                da.calls.lock().unwrap().contains(&"commit")
            })
            .await;
            let actual = tokio::task::spawn_blocking(move || {
                let mut actual = Vec::new();
                files.open(0).unwrap().read_to_end(&mut actual).unwrap();
                actual
            })
            .await
            .unwrap();
            assert_eq!(actual, bytes);
            common::watchdog::until("target drag retired", || !b.node.input_status().unwrap().dnd_active).await;
            a.node.shutdown().await;
            b.node.shutdown().await;
        },
    );
}

#[test]
fn complete_file_drag_reads_large_content_after_drop() {
    common::watchdog::run(
        module_path!(),
        "complete_file_drag_reads_large_content_after_drop",
        Some(4),
        complete_file_drag_reads_large_content_after_drop_scenario(),
    );
}

async fn complete_file_drag_reads_large_content_after_drop_scenario() {
    file_drag(false).await;
}
#[test]
fn complete_file_drag_over_relay() {
    common::watchdog::run(
        module_path!(),
        "complete_file_drag_over_relay",
        Some(4),
        complete_file_drag_over_relay_scenario(),
    );
}

async fn complete_file_drag_over_relay_scenario() {
    file_drag(true).await;
}

async fn drag_pair(root: &Path) -> (Dev, Dev, Drag, Drag) {
    let a = start(root, "a").await;
    let b = start(root, "b").await;
    let da = Drag::attach(&a).await;
    let db = Drag::attach(&b).await;
    common::pair(&a.node, &b.node).await;
    common::active(&[&a.node, &b.node]).await;
    row(&[&a, &b]).await;
    (a, b, da, db)
}

#[test]
fn fast_motion_between_start_window_and_press_cannot_lose_drag() {
    common::watchdog::run(
        module_path!(),
        "fast_motion_between_start_window_and_press_cannot_lose_drag",
        Some(4),
        fast_motion_between_start_window_and_press_cannot_lose_drag_scenario(),
    );
}

async fn fast_motion_between_start_window_and_press_cannot_lose_drag_scenario() {
    let root = tempfile::tempdir().unwrap();
    let (a, b, da, db) = drag_pair(root.path()).await;
    let (begin, begin_gate) = std::sync::mpsc::channel();
    *db.begin_gate.lock().unwrap() = Some(begin_gate);
    let (release, gate) = std::sync::mpsc::channel();
    *db.press_gate.lock().unwrap() = Some(gate);
    *da.offer.lock().unwrap() = Some(vec![ClipData::Text("fast drag".into())]);
    button(&a.input, true);
    cross(&a, &b, 1.).await;
    // cross() can emit extra horizontal deltas while focus snapshots catch up.
    // Drain them before the press gate: a distinct vertical position proves
    // the target has applied the absolute motion following all crossing input.
    let y = b.input.cursor_position().unwrap().y;
    os_move_xy(&a.input, 0., 17.);
    common::watchdog::until("crossing motion drained before source press", || {
        (b.input.cursor_position().unwrap().y - y - 17.).abs() < 0.01
    })
    .await;
    begin.send(()).unwrap();
    common::watchdog::until("source window placed", || db.calls.lock().unwrap().contains(&"placed")).await;
    let at = b.input.cursor_position().unwrap();
    b.input.take_injected();
    let input = b.node.input_thread().unwrap();
    let dropped_before = input.source_motion_discarded_for_test();
    os_move(&a.input, 120.);
    common::watchdog::until("remote motion discarded during source press", || {
        input.source_motion_discarded_for_test() > dropped_before
    })
    .await;
    let during = b.input.cursor_position().unwrap();
    release.send(()).unwrap();
    common::watchdog::until("native drag begins after source press release", || {
        db.began.lock().unwrap().is_some()
    })
    .await;
    assert_eq!(at, during, "movement must be discarded during the source press");
    assert!(db.dropped.load(std::sync::atomic::Ordering::Relaxed) > 0);
    let before_next = b.input.injected();
    assert!(
        !before_next.iter().any(|e| e.move_target().is_some()),
        "discarded motion was replayed"
    );
    os_move(&a.input, 5.);
    common::watchdog::until("first motion after press", || {
        b.input.injected().iter().any(|e| e.move_target().is_some())
    })
    .await;
    let events = b.input.injected();
    let down = events
        .iter()
        .position(|e| matches!(e, InjectEvent::Button { pressed: true, .. }))
        .unwrap();
    let next = events[down + 1..].iter().find_map(InjectEvent::move_target).unwrap();
    assert!(
        // The 120 units sent during SourceStart were discarded, including in
        // Router prediction. Only this post-press 5-unit motion may advance it.
        (next.x - (at.x + 5.)).abs() < 0.01,
        "first absolute motion landed at {next:?}"
    );
    a.node.shutdown().await;
    b.node.shutdown().await;
}
fn has_up(b: &Dev) -> bool {
    b.input.injected().iter().any(|e| {
        matches!(
            e,
            InjectEvent::Button {
                button: MouseButton::Left,
                pressed: false
            }
        )
    })
}

#[test]
fn fast_recross_waits_for_cancel_then_resumes_original_native_session() {
    common::watchdog::run(
        module_path!(),
        "fast_recross_waits_for_cancel_then_resumes_original_native_session",
        Some(4),
        fast_recross_waits_for_cancel_then_resumes_original_native_session_scenario(),
    );
}

#[test]
fn file_recross_with_source_input_preserves_current_capture() {
    common::watchdog::run(
        module_path!(),
        "file_recross_with_source_input_preserves_current_capture",
        Some(4),
        file_recross_preserves_current_capture(false, false),
    );
}

#[test]
fn file_recross_with_target_input_preserves_current_capture() {
    common::watchdog::run(
        module_path!(),
        "file_recross_with_target_input_preserves_current_capture",
        Some(4),
        file_recross_preserves_current_capture(true, false),
    );
}

#[test]
fn file_recross_after_abandoned_probe_with_source_input_releases_current_host() {
    common::watchdog::run(
        module_path!(),
        "file_recross_after_abandoned_probe_with_source_input_releases_current_host",
        Some(10),
        file_recross_preserves_current_capture(false, true),
    );
}

#[test]
fn file_recross_after_abandoned_probe_with_target_input_releases_current_host() {
    common::watchdog::run(
        module_path!(),
        "file_recross_after_abandoned_probe_with_target_input_releases_current_host",
        Some(10),
        file_recross_preserves_current_capture(true, true),
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecrossBoundary {
    Begin,
    Resume,
    Unprobe,
}
#[test]
fn recross_waits_for_resumed_offer_publication() {
    common::watchdog::run(
        module_path!(),
        "recross_waits_for_resumed_offer_publication",
        Some(2),
        async {
            let source = Drag::default();
            common::watchdog::on_timeout("delayed resume offer", source.watchdog_probe());
            let operation = DragOperation::new();
            let items = vec![ClipData::Text("resumed native offer".into())];
            *source.native.lock().unwrap() = Some(operation);
            *source.native_items.lock().unwrap() = items.clone();
            // Reproduce preemption after the call marker but before the fake
            // provider publishes its next offer. No scheduling deadline decides
            // whether the consumer can proceed.
            let (entered, release) = boundary(&source.resume_offer);
            let backend = source.clone();
            let worker = std::thread::spawn(move || backend.resume_source(operation));
            reached(entered).await;
            assert_eq!(*source.calls.lock().unwrap(), ["resume"]);
            assert!(source.offer.lock().unwrap().is_none());
            let mut waiting = std::pin::pin!(source.take_resumed_offer("resumed source published next probe offer"));
            let early = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    std::future::Future::poll(waiting.as_mut(), cx)
                })))
            })
            .await;
            // Release and join even when exercising the old panicking reader,
            // so the red regression leaves no blocked provider behind.
            release.send(()).unwrap();
            let resumed = worker.join();
            assert!(
                matches!(early, Ok(std::task::Poll::Pending)),
                "recross must wait for offer publication after recording resume"
            );
            assert!(resumed.unwrap());
            assert_eq!(waiting.await, items);
            assert!(!source.offer.is_poisoned());
        },
    );
}

#[test]
fn recross_before_native_boundary_completion_preserves_next_probe() {
    common::watchdog::run(
        module_path!(),
        "recross_before_native_boundary_completion_preserves_next_probe",
        Some(10),
        async {
            for gate in [
                RecrossBoundary::Begin,
                RecrossBoundary::Resume,
                RecrossBoundary::Unprobe,
            ] {
                file_recross_with_boundary(true, true, Some(gate)).await;
            }
        },
    );
}
async fn file_recross_preserves_current_capture(target_input: bool, abandon_probe: bool) {
    file_recross_with_boundary(target_input, abandon_probe, None).await;
}
async fn file_recross_with_boundary(target_input: bool, abandon_probe: bool, gate: Option<RecrossBoundary>) {
    let root = tempfile::tempdir().unwrap();
    let (c, a, source, target) = drag_pair(root.path()).await;
    let placements = [&c, &a]
        .iter()
        .enumerate()
        .map(|(i, d)| Placement {
            device: d.node.id().into(),
            origin: Point::new(0., if i == 0 { 250. / 686. } else { 0. }),
            scale: 400.0 / 686.0 / 1600.0,
        })
        .collect();
    api(&c.node, move |n| n.save_layout(placements)).await.unwrap();
    common::watchdog::until("target above source on both nodes", || {
        c.node.layout().0 == a.node.layout().0
    })
    .await;
    let input = if target_input { &a } else { &c };
    if target_input {
        os_move_xy(&a.input, 0., 5000.);
        common::watchdog::until("target mouse controls the original source", || {
            holder(&a.node) == Some(c.node.id()) && holder(&c.node) == Some(c.node.id())
        })
        .await;
    }
    let path = c.dir.join("recross.txt");
    std::fs::write(&path, b"recross").unwrap();
    *source.offer.lock().unwrap() = Some(vec![ClipData::Files(vec![oink_clipboard::FileEntry {
        path: "recross.txt".into(),
        is_dir: false,
        size: 7,
        modified_ms: None,
        local_path: Some(path),
    }])]);
    button(&input.input, true);
    let mut next_cross_issued = false;
    for crossing in 0..3 {
        if gate.is_some() {
            for device in [&c, &a] {
                let cards = device.node.device_cards().unwrap();
                assert_eq!(
                    cards
                        .cards
                        .iter()
                        .find(|card| card.is_self)
                        .unwrap()
                        .status
                        .as_ref()
                        .unwrap()
                        .mouse
                        .configured_mode,
                    Some(oink_agent::device_cards::SharingMode::Shared)
                );
                assert!(
                    device
                        .node
                        .link_config()
                        .unwrap()
                        .router_layout()
                        .load()
                        .topology
                        .topology()
                        .screens()
                        .iter()
                        .all(|s| s.enabled)
                );
            }
        }
        let begin_gate =
            (crossing == 0 && gate == Some(RecrossBoundary::Begin)).then(|| boundary(&target.begin_return));
        let mut return_issued = false;
        // Real routing, glue, action queue and network; the native model gives
        // each catcher re-entry a fresh operation, as AppKit/OLE do after resume.
        common::phase(&format!("recross {crossing} {gate:?}: edge push"));
        if !std::mem::take(&mut next_cross_issued) {
            os_move_xy(&input.input, 0., -5000.);
        }
        if let Some((entered, release)) = begin_gate {
            reached(entered).await;
            assert!(target.began.lock().unwrap().is_some());
            os_move_xy(&input.input, 0., 5000.);
            return_issued = true;
            release.send(()).unwrap();
        }
        if !return_issued {
            common::watchdog::until(
                &format!("recross {crossing} {gate:?}: Begin or native hold failure"),
                || target.began.lock().unwrap().is_some() || source.calls.lock().unwrap().contains(&"hold_failed"),
            )
            .await;
            assert!(
                target.began.lock().unwrap().is_some(),
                "crossing {crossing}: old capture cleanup removed the new catcher"
            );
        }
        assert!(
            source.began.lock().unwrap().is_none(),
            "source must resume its original native drag"
        );
        if crossing < 2 {
            let resume_gate =
                (crossing == 0 && gate == Some(RecrossBoundary::Resume)).then(|| boundary(&source.resume_return));
            if !return_issued {
                os_move_xy(&input.input, 0., 5000.);
            }
            let mut early_probe = None;
            if let Some((entered, release)) = resume_gate {
                reached(entered).await;
                early_probe = Some(
                    source
                        .take_resumed_offer(&format!(
                            "recross {crossing} {gate:?}: gated resume published next probe offer"
                        ))
                        .await,
                );
                os_move_xy(&input.input, 0., -5000.);
                release.send(()).unwrap();
            }
            common::watchdog::until("original native drag resumed", || {
                source.calls.lock().unwrap().iter().filter(|&&c| c == "resume").count() == crossing + 1
            })
            .await;
            assert!(source.native.lock().unwrap().is_some());
            assert!(target.began.lock().unwrap().is_none());
            if abandon_probe {
                // Model moving away before the native catcher reports a new
                // capture. The original drag and physical DOWN remain live.
                let items = if let Some(items) = early_probe {
                    items
                } else {
                    let items = source
                        .take_resumed_offer(&format!(
                            "recross {crossing} {gate:?}: resumed source published next probe offer"
                        ))
                        .await;
                    os_move_xy(&input.input, 0., -5000.);
                    items
                };
                common::watchdog::until("probe awaiting native detection", || {
                    source.probing.load(std::sync::atomic::Ordering::Acquire)
                })
                .await;
                let unprobe_gate =
                    (crossing == 0 && gate == Some(RecrossBoundary::Unprobe)).then(|| boundary(&source.unprobe_return));
                os_move_xy(&input.input, 0., 120.);
                let mut restored = false;
                if let Some((entered, release)) = unprobe_gate {
                    reached(entered).await;
                    *source.offer.lock().unwrap() = Some(items.clone());
                    restored = true;
                    os_move_xy(&input.input, 0., -5000.);
                    next_cross_issued = true;
                    release.send(()).unwrap();
                }
                if !restored {
                    common::watchdog::until("probe abandoned before detection", || {
                        !source.probing.load(std::sync::atomic::Ordering::Acquire)
                    })
                    .await;
                    assert!(source.native.lock().unwrap().is_some());
                    assert!(target.began.lock().unwrap().is_none());
                    *source.offer.lock().unwrap() = Some(items);
                }
            }
        }
    }
    drop_drag(input, &a, &target, &source).await;
    common::watchdog::until("terminal capture removes its own catcher", || {
        !source.probing.load(std::sync::atomic::Ordering::Acquire)
    })
    .await;
    assert_eq!(source.calls.lock().unwrap().iter().filter(|&&c| c == "hold").count(), 3);
    assert_eq!(
        source.calls.lock().unwrap().iter().filter(|&&c| c == "commit").count(),
        1
    );
    c.node.shutdown().await;
    a.node.shutdown().await;
}

async fn fast_recross_waits_for_cancel_then_resumes_original_native_session_scenario() {
    let root = tempfile::tempdir().unwrap();
    let (a, b, da, db) = drag_pair(root.path()).await;
    let items = vec![ClipData::Text("rapid return".into())];
    begin_drag(&a, &b, &da, &db, items.clone()).await;
    let operation = db.began.lock().unwrap().as_ref().unwrap().operation;
    let (release, gate) = std::sync::mpsc::channel();
    *db.cancel_gate.lock().unwrap() = Some(gate);
    os_move(&a.input, -5000.);
    common::watchdog::until("return native Cancel", || db.calls.lock().unwrap().contains(&"cancel")).await;
    os_move(&a.input, -200.);
    os_move(&a.input, 200.);
    let early = da.calls.lock().unwrap().contains(&"resume");
    release.send(()).unwrap();
    assert!(!early, "return Begin overtook old host Cancel");
    common::watchdog::until("original native session resumed", || {
        da.calls.lock().unwrap().contains(&"resume")
    })
    .await;
    assert!(db.began.lock().unwrap().is_none());
    assert!(
        da.began.lock().unwrap().is_none(),
        "return must never synthesize a new Begin"
    );
    assert_eq!(*da.native_items.lock().unwrap(), items);
    a.input.set_cursor(Point::new(1580., 500.));
    os_move(&a.input, 5000.);
    common::watchdog::until("native Begin after immediate second crossing", || {
        db.began.lock().unwrap().is_some()
    })
    .await;
    assert_ne!(db.began.lock().unwrap().as_ref().unwrap().operation, operation);
    assert!(da.began.lock().unwrap().is_none());
    assert_eq!(dragged_items(&db).await, items);
    eprintln!(
        "ungated source critical sections (us): a={:?}, b={:?}",
        da.critical_us.lock().unwrap(),
        db.critical_us.lock().unwrap()
    );
    drop_drag(&a, &b, &db, &da).await;
    a.node.shutdown().await;
    b.node.shutdown().await;
}

#[test]
fn physical_motion_during_source_press_is_discarded_and_next_step_is_fresh() {
    common::watchdog::run(
        module_path!(),
        "physical_motion_during_source_press_is_discarded_and_next_step_is_fresh",
        Some(4),
        physical_motion_during_source_press_is_discarded_and_next_step_is_fresh_scenario(),
    );
}

async fn physical_motion_during_source_press_is_discarded_and_next_step_is_fresh_scenario() {
    let root = tempfile::tempdir().unwrap();
    let (a, b, da, db) = drag_pair(root.path()).await;
    // I=A, C=B: crossing back to the input machine creates a remote host on A,
    // so its physical press barrier remains covered without recreating C's drag.
    cross(&a, &b, 1.).await;
    *db.offer.lock().unwrap() = Some(vec![ClipData::Text("physical return".into())]);
    button(&a.input, true);
    let (release, gate) = std::sync::mpsc::channel();
    *da.press_gate.lock().unwrap() = Some(gate);
    os_move(&a.input, -5000.);
    common::watchdog::until("return source window placed", || {
        da.calls.lock().unwrap().contains(&"placed")
    })
    .await;
    let at = a.input.cursor_position().unwrap();
    os_move(&a.input, 120.);
    let during = a.input.cursor_position().unwrap();
    release.send(()).unwrap();
    if at != during {
        a.node.shutdown().await;
        b.node.shutdown().await;
        panic!("physical motion moved {at:?} to {during:?} during source press");
    }
    common::watchdog::until("return native Begin", || da.began.lock().unwrap().is_some()).await;
    assert!(da.dropped.load(std::sync::atomic::Ordering::Relaxed) > 0);
    os_move(&a.input, 5.);
    assert!((a.input.cursor_position().unwrap().x - at.x - 5.).abs() < 0.01);
    drop_drag(&a, &a, &da, &db).await;
    a.node.shutdown().await;
    b.node.shutdown().await;
}
#[test]
fn drag_motion_flows_while_file_offer_and_native_begin_are_prepared() {
    common::watchdog::run(
        module_path!(),
        "drag_motion_flows_while_file_offer_and_native_begin_are_prepared",
        Some(4),
        drag_motion_flows_while_file_offer_and_native_begin_are_prepared_scenario(),
    );
}

async fn drag_motion_flows_while_file_offer_and_native_begin_are_prepared_scenario() {
    let root = tempfile::tempdir().unwrap();
    let (a, b, da, db) = drag_pair(root.path()).await;
    let (release, gate) = std::sync::mpsc::channel();
    *db.begin_gate.lock().unwrap() = Some(gate);
    let path = a.dir.join("drag.bin");
    std::fs::write(&path, b"drag content").unwrap();
    *da.offer.lock().unwrap() = Some(vec![ClipData::Files(vec![oink_clipboard::FileEntry {
        path: "drag.bin".into(),
        local_path: Some(path),
        size: 12,
        is_dir: false,
        modified_ms: None,
    }])]);
    button(&a.input, true);
    cross(&a, &b, 1.).await;
    common::watchdog::until("native Begin entered", || db.calls.lock().unwrap().contains(&"begin")).await;
    b.input.take_injected();
    os_move(&a.input, 5.);
    common::phase("motion while native Begin is gated");
    // Keep Begin gated until motion is actually delivered. This is an ordering
    // property, not a latency benchmark; the timeout only bounds a broken test.
    let moved = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if b.input.injected().iter().any(|event| event.move_target().is_some()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    let motion_during_begin = b.input.injected();
    // Always release the worker before asserting, including on a regression.
    release.send(()).unwrap();
    assert!(moved, "file preparation/native Begin blocked pointer motion");
    // This scenario verifies motion while Begin is gated and the normal
    // DOWN/UP ordering. Wait for the actual source press before releasing:
    // an earlier UP may correctly revoke Begin with no DOWN or UP at all,
    // covered separately by drag_up_during_slow_begin_revokes_before_source_press.
    common::watchdog::until("native Begin completed its source press", || {
        db.began.lock().unwrap().is_some()
    })
    .await;
    button(&a.input, false);
    common::watchdog::until("Up after native Begin", || has_up(&b)).await;
    os_move(&a.input, 5.);
    common::watchdog::until("motion after Up and completed source press", || {
        let events = b.input.injected();
        let up = events.iter().position(|event| {
            matches!(
                event,
                InjectEvent::Button {
                    button: MouseButton::Left,
                    pressed: false
                }
            )
        });
        up.is_some_and(|up| events[up + 1..].iter().any(|event| event.move_target().is_some()))
    })
    .await;
    let events = b.input.injected();
    let down = events
        .iter()
        .position(|event| {
            matches!(
                event,
                InjectEvent::Button {
                    button: MouseButton::Left,
                    pressed: true
                }
            )
        })
        .expect("Begin's native Down");
    let up = events
        .iter()
        .position(|event| {
            matches!(
                event,
                InjectEvent::Button {
                    button: MouseButton::Left,
                    pressed: false
                }
            )
        })
        .expect("queued Up");
    assert!(down < up, "Up overtook native Begin");
    assert_eq!(
        events[..down],
        motion_during_begin,
        "motion overtook the queued Up boundary"
    );
    assert_eq!(up, down + 1, "motion overtook the queued Up boundary");
    db.end(true);
    common::phase("drag source shutdown");
    a.node.shutdown().await;
    common::phase("drag destination shutdown");
    b.node.shutdown().await;
}

#[test]
fn drag_up_during_slow_begin_revokes_before_source_press() {
    common::watchdog::run(
        module_path!(),
        "drag_up_during_slow_begin_revokes_before_source_press",
        Some(4),
        drag_up_during_slow_begin_revokes_before_source_press_scenario(),
    );
}

async fn drag_up_during_slow_begin_revokes_before_source_press_scenario() {
    let root = tempfile::tempdir().unwrap();
    let (a, b, da, db) = drag_pair(root.path()).await;
    let (release, gate) = std::sync::mpsc::channel();
    *db.begin_gate.lock().unwrap() = Some(gate);
    *da.offer.lock().unwrap() = Some(vec![ClipData::Text("held begin".into())]);
    button(&a.input, true);
    cross(&a, &b, 1.).await;
    common::watchdog::until("native Begin entered", || db.calls.lock().unwrap().contains(&"begin")).await;
    b.input.take_injected();
    button(&a.input, false);
    common::watchdog::until("source release revokes pending source press", || {
        b.node.input_status().unwrap().dnd_host_pending.is_none()
    })
    .await;
    let early_up = has_up(&b);
    release.send(()).unwrap();
    assert!(!early_up, "no UP is owed before a target DOWN is submitted");
    common::watchdog::until("revoked Begin retired", || !b.node.input_status().unwrap().dnd_active).await;
    common::watchdog::until("original source cancelled", || da.native.lock().unwrap().is_none()).await;
    assert!(db.began.lock().unwrap().is_none());
    assert!(
        !b.input.injected().iter().any(|event| matches!(
            event,
            InjectEvent::Button {
                button: MouseButton::Left,
                ..
            }
        )),
        "revoked preparation must never synthesize DOWN or UP"
    );
    a.node.shutdown().await;
    b.node.shutdown().await;
}
#[test]
fn drag_escape_cancel_completes_before_release() {
    common::watchdog::run(
        module_path!(),
        "drag_escape_cancel_completes_before_release",
        Some(4),
        drag_escape_cancel_completes_before_release_scenario(),
    );
}

async fn drag_escape_cancel_completes_before_release_scenario() {
    let root = tempfile::tempdir().unwrap();
    let (a, b, da, db) = drag_pair(root.path()).await;
    begin_drag(&a, &b, &da, &db, vec![ClipData::Text("cancel".into())]).await;
    let (release, gate) = std::sync::mpsc::channel();
    *db.cancel_gate.lock().unwrap() = Some(gate);
    key(&a.input, 0x29, true);
    key(&a.input, 0x29, false);
    common::watchdog::until("native Cancel entered", || db.calls.lock().unwrap().contains(&"cancel")).await;
    b.input.take_injected();
    button(&a.input, false);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let early_up = has_up(&b);
    release.send(()).unwrap();
    assert!(!early_up, "Up reached a drop target while Cancel was still pending");
    common::watchdog::until("Up after native Cancel", || has_up(&b)).await;
    common::watchdog::until("cancelled drag retired", || !b.node.input_status().unwrap().dnd_active).await;
    // Router retirement may precede the fake native callback finishing: Cancel
    // injects its Up before clearing the offer. Await that completion separately.
    common::watchdog::until("native Cancel retired its offer", || db.began.lock().unwrap().is_none()).await;
    a.node.shutdown().await;
    b.node.shutdown().await;
}
#[test]
fn drag_released_without_a_drop_target_retires() {
    common::watchdog::run(
        module_path!(),
        "drag_released_without_a_drop_target_retires",
        Some(4),
        drag_released_without_a_drop_target_retires_scenario(),
    );
}

async fn drag_released_without_a_drop_target_retires_scenario() {
    let root = tempfile::tempdir().unwrap();
    let (a, b, da, db) = drag_pair(root.path()).await;
    begin_drag(&a, &b, &da, &db, vec![ClipData::Text("no target".into())]).await;
    b.input.take_injected();
    button(&a.input, false);
    common::watchdog::until("native Up with no target", || has_up(&b)).await;
    db.end(false);
    common::watchdog::until("no-target drag retired", || !b.node.input_status().unwrap().dnd_active).await;
    a.node.shutdown().await;
    b.node.shutdown().await;
}
#[test]
fn drag_crosses_two_edges_and_drops_on_third_device() {
    common::watchdog::run(
        module_path!(),
        "drag_crosses_two_edges_and_drops_on_third_device",
        Some(4),
        drag_crosses_two_edges_and_drops_on_third_device_scenario(),
    );
}

async fn drag_crosses_two_edges_and_drops_on_third_device_scenario() {
    let root = tempfile::tempdir().unwrap();
    let (a, b, da, db) = drag_pair(root.path()).await;
    let c = start(root.path(), "c").await;
    let dc = Drag::attach(&c).await;
    common::pair(&a.node, &c.node).await;
    common::active(&[&a.node, &b.node, &c.node]).await;
    row(&[&a, &b, &c]).await;
    let items = vec![ClipData::Text("two edges".repeat(10_000))];
    begin_drag(&a, &b, &da, &db, items.clone()).await;
    let end = Instant::now() + Duration::from_secs(10);
    while holder(&c.node) != Some(c.node.id()) || dc.began.lock().unwrap().is_none() {
        assert!(Instant::now() < end, "second edge failed: {}", dump(&[&a, &b, &c]));
        os_move(&a.input, 30.);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(db.calls.lock().unwrap().contains(&"cancel"));
    assert_eq!(dragged_items(&dc).await, items);
    drop_drag(&a, &c, &dc, &da).await;
    a.node.shutdown().await;
    b.node.shutdown().await;
    c.node.shutdown().await;
}

struct ExportFiles {
    inventory: oink_clipboard::model::Inventory,
    files: Arc<dyn oink_clipboard::content::namespace::Namespace>,
}
impl oink_clipboard::content::Translator for ExportFiles {
    fn standard(&self, _: &oink_clipboard::model::Representation, _: &[u8]) -> std::io::Result<Vec<ClipData>> {
        unreachable!()
    }
}
impl oink_clipboard::content::Provider for ExportFiles {
    fn inventory(&self) -> oink_clipboard::model::Inventory {
        self.inventory.clone()
    }
    fn open(&self, _: oink_clipboard::model::RepresentationId) -> std::io::Result<oink_clipboard::FileReader> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
    fn translator(&self) -> Arc<dyn oink_clipboard::content::Translator> {
        Arc::new(Self {
            inventory: self.inventory.clone(),
            files: self.files.clone(),
        })
    }
    fn namespace(&self) -> Option<Arc<dyn oink_clipboard::content::namespace::Namespace>> {
        Some(self.files.clone())
    }
}
#[test]
fn exported_promise_whole_delivery_survives_source_release_and_return_to_source() {
    common::watchdog::run(
        module_path!(),
        "exported_promise_whole_delivery_survives_source_release_and_return_to_source",
        Some(4),
        async {
            use oink_clipboard::content::namespace::{Local, Namespace};
            use oink_clipboard::model::*;
            for return_to_source in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let (a, b, da, db) = drag_pair(root.path()).await;
                da.export_required.store(true, std::sync::atomic::Ordering::Release);
                *da.offer.lock().unwrap() = Some(vec![ClipData::Text("unfulfilled promise fixture".into())]);
                button(&a.input, true);
                a.input.set_cursor(Point::new(1580., 500.));
                common::watchdog::until("promise export at boundary", || {
                    os_move(&a.input, 10.);
                    da.calls.lock().unwrap().contains(&"export")
                })
                .await;
                assert!(db.began.lock().unwrap().is_none());
                assert_eq!(holder(&a.node), Some(a.node.id()));
                let exported = a.dir.join("export-fixture");
                std::fs::create_dir(&exported).unwrap();
                std::fs::write(exported.join("one"), b"first full file").unwrap();
                std::fs::write(exported.join("two"), b"second unread file").unwrap();
                let files = Local::new(vec![(NodeId(1), exported.clone())]).unwrap();
                let inventory = Inventory {
                    id: OfferId {
                        clip: oink_foundation::content::ClipId {
                            origin: a.node.id(),
                            seq: 0,
                        },
                        generation: 0,
                    },
                    platform: oink_foundation::clipboard::ClipPlatform::Mac,
                    roots: files.roots(),
                    items: vec![ContentItem {
                        virtual_files: false,
                        id: ItemId(1),
                        file: Some(NodeId(1)),
                        representations: vec![Representation {
                            id: RepresentationId(1),
                            format: NativeFormat::Mac("public.file-url".into()),
                            medium: Medium::Bytes,
                            aspect: 1,
                            index: -1,
                            standard: None,
                            data: RepresentationData::File(NodeId(1)),
                        }],
                    }],
                };
                da.exported(Arc::new(ExportFiles { inventory, files }));
                common::watchdog::until("ready export starts remote drag", || db.began.lock().unwrap().is_some()).await;
                if return_to_source {
                    os_move(&a.input, -5000.);
                    common::watchdog::until("exported copy starts on source", || da.began.lock().unwrap().is_some())
                        .await;
                    assert!(!da.calls.lock().unwrap().contains(&"resume"));
                }
                let (target, native) = if return_to_source { (&a, &da) } else { (&b, &db) };
                let readers = native.began.lock().unwrap().as_ref().unwrap().files.clone().unwrap();
                target.input.take_injected();
                button(&a.input, false);
                common::watchdog::until("drop Up", || has_up(target)).await;
                native.end(true);
                // No receiving application reader has opened anything. The source
                // is released only after the independent whole-batch durable receipt.
                common::watchdog::until("source receives whole export receipt", || {
                    da.calls.lock().unwrap().contains(&"forget")
                })
                .await;
                std::fs::remove_dir_all(exported).unwrap();
                let received = tokio::task::spawn_blocking(move || {
                    use std::io::Read;
                    let entries = readers.descriptor_entries().unwrap().unwrap();
                    let mut result = Vec::new();
                    for (index, _) in entries.iter().enumerate().filter(|(_, entry)| !entry.is_dir) {
                        let mut bytes = Vec::new();
                        readers.open(index).unwrap().read_to_end(&mut bytes).unwrap();
                        result.push(bytes);
                    }
                    result.sort();
                    result
                })
                .await
                .unwrap();
                assert_eq!(
                    received,
                    vec![b"first full file".to_vec(), b"second unread file".to_vec()]
                );
                assert!(
                    !da.calls.lock().unwrap().contains(&"commit"),
                    "original promise was already received once"
                );
                a.node.shutdown().await;
                b.node.shutdown().await;
            }
        },
    );
}

#[path = "dnd_slow_source.rs"]
mod slow_source;

#[test]
fn sharing_off_clears_an_orphan_catcher_even_when_routing_is_idle() {
    common::watchdog::run(module_path!(), "sharing off clears orphan catcher", Some(2), async {
        let root = tempfile::tempdir().unwrap();
        let dev = start(root.path(), "idle-catcher").await;
        let drag = Drag::attach(&dev).await;
        assert!(!dev.node.input_status().unwrap().dnd_active);
        // Model the native surface surviving an earlier failed retirement. The
        // router is idle, so it has no active probe to generate an Unprobe for.
        drag.probing.store(true, std::sync::atomic::Ordering::Release);
        api(&dev.node, |node| {
            node.set_local_paused("00000000-0000-4000-8000-000000001301".into(), true)
        })
        .await
        .unwrap();
        let cleared = tokio::time::timeout(Duration::from_secs(2), async {
            while drag.probing.load(std::sync::atomic::Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        assert!(dev.node.input_status().unwrap().paused);
        dev.node.shutdown().await;
        assert!(cleared, "sharing off did not retire an orphan native catcher");
    });
}

#[test]
fn peer_pause_retires_the_held_source_catcher() {
    common::watchdog::run(module_path!(), "peer pause retires held catcher", Some(4), async {
        let root = tempfile::tempdir().unwrap();
        let (a, b, source, target) = drag_pair(root.path()).await;
        begin_drag(&a, &b, &source, &target, vec![ClipData::Text("held source".into())]).await;
        assert!(source.probing.load(std::sync::atomic::Ordering::Acquire));
        api(&b.node, |node| {
            node.set_local_paused("00000000-0000-4000-8000-000000001302".into(), true)
        })
        .await
        .unwrap();
        common::watchdog::until("paused peer cancels and retires held source", || {
            source.native.lock().unwrap().is_none() && !source.probing.load(std::sync::atomic::Ordering::Acquire)
        })
        .await;
        assert!(source.calls.lock().unwrap().contains(&"source_cancel"));
        a.node.shutdown().await;
        b.node.shutdown().await;
    });
}
