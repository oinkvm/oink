//! Native cross-device drag source and target interfaces.
//!
//! - **接收窗口**（[`DndBackend::probe`]）：指针按着左键贴在通往其他设备的边上时，在光标下放一个很小的透明
//!   窗口，注册为拖放目标。只有真的系统拖拽经过时它才会收到拖入；读出拖拽内容（文件清单、文本、PNG）后经
//!   [`DndSink`] 报告 [`PlatformDndEvent::Detected`]。
//! - **交出**（[`DndBackend::hold_source`]）：保留原生源会话，在同一接收窗口隐藏图像与光标，返回已确认的
//!   捕获锚点。边界不发 UP；回源经 [`DndBackend::resume_source`] 恢复同一会话。
//! - **最终提交**（[`DndBackend::finish_source`]）：真实远端放下或取消后才结束原生源，只接受 COPY。
//!   `release` 是此步骤使用的底层原生接收操作，也供独立平台例程使用。
//! - **发起**（[`DndBackend::begin`]）：在光标下放一个透明小窗，注入左键按下，在按下的处理里发起系统拖拽；
//!   远端文件用按需读取的 [`FileContents`]（macOS File Provider、Windows 虚拟文件）。拖拽结束时报告
//!   [`PlatformDndEvent::Ended`]。
//! - **取消**（[`DndBackend::cancel`]）：把本机由 [`begin`](DndBackend::begin) 发起的拖拽无害地结束。
//!
//! Native lifecycle calls run on an ordinary action worker; each backend dispatches
//! only its window/session work to AppKit or the OLE STA. Optional artwork never
//! waits on the input/capture/injection owners or acquires their completion fence.
//! The short positioned-DOWN guard excludes motion only during button submission.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use oink_foundation::Point;
use oink_foundation::clipboard::ClipData;
use oink_foundation::input::{InjectEvent, MouseButton};

use crate::{ClipboardError, FileContents};

/// 拖拽允许的操作（位）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DragOps(pub u8);

impl DragOps {
    pub const COPY: Self = Self(1);
    pub const MOVE: Self = Self(2);
    pub const LINK: Self = Self(4);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// 平台报告的事件。
#[derive(Clone, Debug, PartialEq)]
pub enum PlatformDndEvent {
    /// 接收窗口上有系统拖拽经过。`items`：拖拽内容（`ClipData::Files` 带 `local_path`）；`ops`：源允许的操作。
    Detected {
        operation: DragOperation,
        items: Vec<ClipData>,
        ops: DragOps,
    },
    /// A source thumbnail completed on an ordinary worker.
    SourceImage {
        operation: DragOperation,
        image: oink_foundation::dnd::DragImage,
    },
    /// [`DndBackend::begin`] 发起的拖拽结束了（`dropped`：放到了某个目标上）。
    Ended { operation: DragOperation, dropped: bool },
    /// Destination callback for the original source session, independent of any
    /// Oink-hosted target drag on this machine.
    SourceEnded { operation: DragOperation, accepted: bool },
    /// The destination stopped observing the original drag. This does not
    /// prove that the third-party native source ended or received its UP.
    SourceCatcherLost { operation: DragOperation },
    NativeHoldSummary {
        operation: DragOperation,
        summary: Box<oink_foundation::dnd::NativeHoldSummary>,
    },
    /// Pure macOS promises have already ended their original native session.
    /// Success only authorizes a pending crossing to use the owned export.
    SourceExported {
        operation: DragOperation,
        result: Result<(), oink_foundation::dnd::ExportFailure>,
    },
    /// Explicit cancel and original release were submitted before the catcher
    /// exit was retired. Does not claim a native ended callback or animation.
    SourceCancelRetired { operation: DragOperation },
}

/// Identity captured by the platform when a drag begins and returned unchanged
/// when it ends. Local-only native tools use no session; shared drags carry the
/// full network-granted identity. The operation distinguishes repeated drags in
/// one session, including a drag handed away and later handed back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DragOperation {
    pub sequence: u64,
}

impl Default for DragOperation {
    fn default() -> Self {
        Self::new()
    }
}

impl DragOperation {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let mut sequence = NEXT.load(Ordering::Relaxed);
        loop {
            let next = sequence
                .checked_add(1)
                .expect("native drag operation counter exhausted");
            match NEXT.compare_exchange_weak(sequence, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(current) => sequence = current,
            }
        }
        Self { sequence }
    }
}

/// Existing preparation boundaries, copied before diagnostic formatting.
#[derive(Clone, Copy, Debug)]
pub enum PreparationStage {
    SourceRead,
    Normalization,
    NativePreparation,
    Metadata,
    PendingWindow,
    SourcePress,
    NativeAcknowledgement,
    WritersPreview,
    NativeBegin,
    Preview,
    Namespace,
    Registration,
}
static PREPARATION_OBSERVER: std::sync::OnceLock<fn(PreparationStage, std::time::Instant, std::time::Instant)> =
    std::sync::OnceLock::new();
pub fn set_preparation_observer(observer: fn(PreparationStage, std::time::Instant, std::time::Instant)) {
    let _ = PREPARATION_OBSERVER.set(observer);
}
pub fn preparation_timing(stage: PreparationStage, start: std::time::Instant, end: std::time::Instant) {
    if let Some(observer) = PREPARATION_OBSERVER.get() {
        observer(stage, start, end);
    }
}

static NATIVE_TIMING_ENABLED: std::sync::OnceLock<fn() -> bool> = std::sync::OnceLock::new();
pub fn set_native_timing_enabled(enabled: fn() -> bool) {
    let _ = NATIVE_TIMING_ENABLED.set(enabled);
}
#[cfg(windows)]
pub(crate) fn native_timing_enabled() -> bool {
    NATIVE_TIMING_ENABLED.get().is_some_and(|enabled| enabled())
}

/// Copy-only timing handoff. Registration is ordinary-thread work; native
/// completion invokes only the supplied allocation-free numeric recorder.
#[derive(Clone, Copy, Debug)]
pub struct NativeBeginTiming {
    pub generation: u64,
    pub queued_at: std::time::Instant,
    pub record: fn(u64, std::time::Instant, std::time::Instant),
}
impl NativeBeginTiming {
    pub fn complete(self, at: Option<std::time::Instant>) {
        if let Some(at) = at {
            (self.record)(self.generation, self.queued_at, at);
        }
    }
}

/// One bounded native operation slot. An old callback cannot consume a replacement.
#[derive(Default, Debug)]
pub struct TargetStartObservation(Option<(DragOperation, NativeBeginTiming)>);
impl TargetStartObservation {
    pub fn arm(&mut self, operation: DragOperation, timing: NativeBeginTiming) {
        self.0 = Some((operation, timing));
    }
    pub fn take(&mut self, operation: DragOperation) -> Option<NativeBeginTiming> {
        if self.0.as_ref().is_some_and(|(current, _)| *current == operation) {
            self.0.take().map(|(_, timing)| timing)
        } else {
            None
        }
    }
}
/// Native source boundaries on one host; no content or cross-host clock.
#[derive(Clone, Copy, Debug)]
pub struct SourceTiming {
    pub operation: DragOperation,
    pub probed_at: Option<std::time::Instant>,
    pub recognized_at: std::time::Instant,
    pub native_read_at: std::time::Instant,
    pub inventory_at: std::time::Instant,
}
static SOURCE_OBSERVER: std::sync::OnceLock<fn(SourceTiming)> = std::sync::OnceLock::new();
pub fn set_source_observer(observer: fn(SourceTiming)) {
    let _ = SOURCE_OBSERVER.set(observer);
}
pub(crate) fn source_timing(timing: SourceTiming) {
    if let Some(observer) = SOURCE_OBSERVER.get() {
        observer(timing);
    }
}

/// 平台事件的接收者（可能在任意线程调用，必须很快返回）。
pub type DndSink = Arc<dyn Fn(PlatformDndEvent) + Send + Sync>;

/// 注入一个事件到本机（`InputBackend::inject`）。
pub type Injector = Arc<dyn Fn(&InjectEvent) -> Result<(), String> + Send + Sync>;

/// Press at the exact source-window hit-test position, retaining it across submission.
pub type SourcePress = Arc<dyn Fn(DragOperation, Point) -> Result<(), String> + Send + Sync>;

/// Execute only window placement, the positioned Down and its native acknowledgement.
/// The ordinary caller runs the work; the injection owner pauses motion for this
/// portion of its already-running Begin. Newly arriving motion is discarded and
/// counted; other queue boundaries retain their order. Returns the discard count.
/// Metadata preparation stays outside it.
pub type SourceStart = Arc<dyn Fn(&mut dyn FnMut()) -> Result<u64, String> + Send + Sync>;

pub type SourceImageNudge = Arc<
    dyn Fn(DragOperation, MouseButton, &dyn Fn(oink_foundation::dnd::ImageNudgePost)) -> Result<(), String>
        + Send
        + Sync,
>;

/// Native Mac source transitions share the input owner's serialized ledger.
pub type SourceReturn = Arc<dyn Fn(DragOperation, Point, &dyn Fn(Point)) -> Result<(), String> + Send + Sync>;

/// Callbacks owned by the input backend for one native source's transitions.
pub struct SourceHoldInput {
    pub freeze: Arc<dyn Fn(DragOperation) -> Result<oink_foundation::dnd::DragAnchor, String> + Send + Sync>,
    pub relocate: Arc<dyn Fn(DragOperation, Point) -> Result<(), String> + Send + Sync>,
    pub return_to: SourceReturn,
}
impl std::fmt::Debug for SourceHoldInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceHoldInput").finish_non_exhaustive()
    }
}

/// Numeric once-per-native-Begin summary, including early failures. No native
/// window handle, application identity or per-motion record is exported.
#[derive(Default)]
pub(crate) struct SourceStartTrace {
    pub offset: Option<Point>,
    pub received: bool,
    pub dropped: u64,
    pub critical_us: u64,
}
impl Drop for SourceStartTrace {
    fn drop(&mut self) {
        let offset = self.offset.unwrap_or_default();
        tracing::debug!(
            dnd_start_offset_x = offset.x,
            dnd_start_offset_y = offset.y,
            dnd_start_offset_known = self.offset.is_some(),
            dnd_start_received = self.received,
            dropped_motions = self.dropped,
            critical_us = self.critical_us,
            "drag start window press receipt"
        );
    }
}

/// 一次要发起的拖拽。
pub struct DragSpec {
    pub operation: DragOperation,
    /// Handoff position hint. Startup samples the actual cursor after preparation.
    pub at: Point,
    /// 内容。`ClipData::Files` 的清单已按本机规范化文件名；`files` 为 `None` 时每项都带 `local_path`
    /// （本机的文件，拖回内容源）。
    pub items: Vec<ClipData>,
    /// 远端文件的内容（`FileContents::open(i)` 的 `i` 是清单下标）。
    pub files: Option<Arc<dyn FileContents>>,
    /// Ordered native representations with deferred readers for the actual drop.
    pub native: Option<crate::NativeSpec>,
    /// 允许的操作（远端内容总是复制；本机文件为源拖拽当初允许的操作）。
    pub ops: DragOps,
}

impl std::fmt::Debug for DragSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DragSpec")
            .field("operation", &self.operation)
            .field("at", &self.at)
            .field("representations", &self.items.len())
            .field("remote_files", &self.files.is_some())
            .field("ops", &self.ops)
            .finish()
    }
}

/// 本机平台的拖放能力。
pub trait DndBackend: Send + Sync {
    /// 准备好（创建窗口等）。平台做不了拖放（例如 macOS 上主线程没有运行 `NSApplication`）时返回
    /// `Err(Unsupported)`，服务进程随之通告对端本机不能拖放。
    fn start(&self, sink: DndSink, inject: Injector) -> Result<(), ClipboardError>;
    /// Optional positioned source press supplied by the input backend.
    fn set_source_press(&self, _press: SourcePress) {}
    fn set_source_start(&self, _start: SourceStart) {}
    fn set_source_image_nudge(&self, _nudge: SourceImageNudge) {}
    fn set_source_hold_input(&self, _input: SourceHoldInput) {}
    /// Mac commits its input anchor before it hides source artwork.
    fn source_hold_installs_anchor(&self) -> bool {
        false
    }
    /// Arm deferred artwork refreshes after the input anchor has been installed.
    fn source_hold_ready(&self, _operation: DragOperation) {}
    /// Revoke queued artwork events before any source-button release.
    fn retire_source_image_nudges(&self, _operation: DragOperation) {}
    fn set_content_store(&self, _root: std::path::PathBuf) {}
    fn source_content(&self, _operation: DragOperation) -> Option<Arc<dyn crate::content::Provider>> {
        None
    }

    fn forget_content(&self, _operation: DragOperation) {}
    /// 在 `at`（本机全局坐标）放出或挪动接收窗口。
    fn probe(&self, at: Point);
    /// 撤掉接收窗口。
    fn unprobe(&self);
    /// Retire only this source capture. A resumed native drag can already have
    /// re-entered with a new operation when cleanup of the old capture arrives.
    /// Backends supporting source holds must check identity on their native UI
    /// thread together with hiding the catcher; stale cleanup has no effect.
    fn unprobe_source(&self, _operation: DragOperation) {}
    fn hold_source(&self, _operation: DragOperation) -> Result<oink_foundation::dnd::DragAnchor, ClipboardError> {
        Err(ClipboardError::Unsupported)
    }
    fn source_requires_export(&self, _operation: DragOperation, _windows_target: bool) -> bool {
        false
    }
    fn export_source(
        &self,
        _operation: DragOperation,
        _crossing: std::time::Instant,
        _release: &dyn Fn() -> Result<(), String>,
    ) -> bool {
        false
    }
    /// Resume the original source session. Never inject Down or call begin.
    fn resume_source(&self, _operation: DragOperation) -> bool {
        false
    }
    /// Restore the logical routing point while the native source is still hidden.
    fn resume_source_at(&self, operation: DragOperation, _at: Option<Point>) -> bool {
        self.resume_source(operation)
    }
    /// Attach the router's exact numeric inputs to this native hold's summary.
    fn observe_source_cancel_geometry(
        &self,
        _operation: DragOperation,
        _geometry: oink_foundation::dnd::CancelGeometry,
    ) {
    }
    /// Called by the input backend at actual submission, before waiting for its
    /// receipt. A posted coordinate is not an AppKit acknowledgement.
    fn observe_source_cancel_posted(&self, _operation: DragOperation, _at: Point) {}
    /// Move the original native session, then await its position callback within
    /// the platform main-queue budget. `move_source` submits one tagged dragged
    /// event through the input backend; never creates a new drag or a DOWN.
    fn prepare_source_cancel(
        &self,
        _operation: DragOperation,
        _at: Point,
        _move_source: &dyn Fn(Point) -> Result<(), String>,
    ) -> bool {
        false
    }
    /// Keep cancel-only relocation and final release ordered on the action queue.
    fn finish_source_at(
        &self,
        operation: DragOperation,
        accept: bool,
        cancel_at: Option<Point>,
        move_source: &dyn Fn(Point) -> Result<(), String>,
        release: &dyn Fn() -> Result<(), String>,
    ) -> bool {
        if !accept && let Some(at) = cancel_at {
            let acknowledged = self.prepare_source_cancel(operation, at, move_source);
            tracing::info!(
                dnd_cancel_session = acknowledged,
                dnd_cancel_x = at.x,
                dnd_cancel_y = at.y,
                "held source cancel position acknowledgement"
            );
        }
        self.finish_source(operation, accept, release)
    }
    /// Final COPY/cancel. The supplied release preserves a still-physical local
    /// hold; native callbacks remain the authority for completion.
    fn finish_source(
        &self,
        _operation: DragOperation,
        _accept: bool,
        _release: &dyn Fn() -> Result<(), String>,
    ) -> bool {
        false
    }
    fn source_completion_is_async(&self) -> bool {
        false
    }
    /// 在接收窗口上注入左键抬起，等源拖拽放到接收窗口上（最多约 1 秒），撤掉窗口。返回是否确认放下了。
    fn release(&self) -> bool;
    /// 结束物理左键发起的源拖拽。macOS 在接收窗口收尾期间调用专用抬起；其他平台保持 release 行为。
    fn release_physical(&self, _release: &dyn Fn() -> Result<(), String>) -> bool {
        self.release()
    }

    /// Artwork captured from the current native source, if the platform exposes real pixels.
    fn source_image(&self) -> Option<oink_foundation::dnd::DragImage> {
        None
    }
    /// Optional artwork for the next begin call, on the same ordered action queue.
    fn set_target_image(&self, _image: Option<oink_foundation::dnd::DragImage>) {}
    /// Apply optional artwork only to the matching live native operation.
    /// Both platforms start with defaults and asynchronously replace the image.
    fn update_target_image(&self, _operation: DragOperation, _image: oink_foundation::dnd::DragImage) {}
    /// Prepare non-file hover artwork off the native UI thread and outside the
    /// window/Down critical section. True means preparation could have waited;
    /// the caller must recheck the holder/left-button authority before Begin.
    fn prepare_preview(&self, _spec: &DragSpec) -> bool {
        false
    }

    /// Detection only: a target release is about to be submitted. Must not wait.
    fn observe_target_drop(&self, _operation: DragOperation) {}

    /// 发起拖拽（放小窗、注入左键按下、开始系统拖拽）。失败时平台自己注入抬起（如果注入过按下）。
    fn begin(&self, spec: DragSpec) -> Result<(), ClipboardError>;
    /// Observe AppKit willBegin or Windows OLE loop entry without waiting.
    /// Successful begin can return before that callback. Cancellation/end closes
    /// the observation with None if no native start was observed.
    fn observe_native_begin(
        &self,
        _operation: DragOperation,
        _wait_for_native: bool,
        callback: Box<dyn FnOnce(Option<std::time::Instant>) + Send>,
    ) {
        callback(None);
    }

    /// Arm before Begin. The backend completes at AppKit API return / OLE call entry.
    fn target_start_timing(&self, _operation: DragOperation, _timing: Option<NativeBeginTiming>) {}

    fn observe_native_timing(&self, _operation: DragOperation, _wait: bool, _timing: NativeBeginTiming) {}

    /// Only a native macOS backend opts into system File Provider registration.
    #[cfg(target_os = "macos")]
    fn uses_file_provider(&self) -> bool {
        false
    }
    /// macOS: begin with explicit system File Provider URLs and their lifetime registration.
    #[cfg(target_os = "macos")]
    fn begin_file_provider(
        &self,
        spec: DragSpec,
        registration: crate::file_provider::Registration,
    ) -> Result<(), ClipboardError> {
        let _ = spec;
        registration.cancel();
        Err(ClipboardError::Unsupported)
    }
    /// 无害地结束 [`begin`](Self::begin) 发起、还在进行的拖拽（不放到任何程序里）。返回是否确认结束了。
    fn cancel(&self) -> bool;
    /// Cancel a hosted drag without clearing the still-held local physical button at HID.
    fn cancel_physical(&self, _release: &dyn Fn() -> Result<(), String>) -> bool {
        self.cancel()
    }
}

/// 本机平台的拖放实现。
pub fn platform_dnd() -> Result<Box<dyn DndBackend>, ClipboardError> {
    #[cfg(target_os = "macos")]
    return Ok(Box::new(crate::macos::dnd::MacDnd::new()));
    #[cfg(windows)]
    return crate::windows::dnd::backend();
    #[allow(unreachable_code)]
    Err(ClipboardError::Unsupported)
}

#[cfg(any(target_os = "macos", windows, all(test, debug_assertions)))]
pub(crate) mod preview;

#[cfg(all(test, debug_assertions))]
mod cancel_order_tests {
    use super::*;
    use std::sync::Mutex;

    struct Backend {
        events: Arc<Mutex<Vec<&'static str>>>,
        acknowledged: bool,
    }
    impl DndBackend for Backend {
        fn start(&self, _: DndSink, _: Injector) -> Result<(), ClipboardError> {
            Ok(())
        }
        fn probe(&self, _: Point) {}
        fn unprobe(&self) {}
        fn begin(&self, _: DragSpec) -> Result<(), ClipboardError> {
            unreachable!()
        }
        fn cancel(&self) -> bool {
            unreachable!()
        }
        fn release(&self) -> bool {
            unreachable!()
        }
        fn prepare_source_cancel(
            &self,
            _: DragOperation,
            at: Point,
            move_source: &dyn Fn(Point) -> Result<(), String>,
        ) -> bool {
            assert_eq!(at, Point::new(1., 558.5));
            move_source(at).unwrap();
            self.events.lock().unwrap().push("session_ack_or_budget");
            self.acknowledged
        }
        fn finish_source(&self, _: DragOperation, accept: bool, release: &dyn Fn() -> Result<(), String>) -> bool {
            self.events.lock().unwrap().push(if accept { "drop" } else { "escape" });
            release().is_ok()
        }
    }
    #[test]
    fn dragged_then_session_ack_then_cancel_never_on_drop() {
        for (accept, at, acknowledged, expected) in [
            (
                false,
                Some(Point::new(1., 558.5)),
                true,
                vec!["dragged", "session_ack_or_budget", "escape", "release"],
            ),
            (
                false,
                Some(Point::new(1., 558.5)),
                false,
                vec!["dragged", "session_ack_or_budget", "escape", "release"],
            ),
            (true, Some(Point::new(1., 558.5)), true, vec!["drop", "release"]),
            (false, None, true, vec!["escape", "release"]),
        ] {
            let events = Arc::new(Mutex::new(Vec::new()));
            let backend = Backend {
                events: events.clone(),
                acknowledged,
            };
            assert!(backend.finish_source_at(
                DragOperation::new(),
                accept,
                at,
                &|_| {
                    events.lock().unwrap().push("dragged");
                    Ok(())
                },
                &|| {
                    events.lock().unwrap().push("release");
                    Ok(())
                }
            ));
            assert_eq!(*events.lock().unwrap(), expected);
        }
    }
}

#[cfg(test)]
mod target_start_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};
    static RECORDED: AtomicU64 = AtomicU64::new(0);
    fn record(_: u64, start: Instant, end: Instant) {
        RECORDED.fetch_add(end.duration_since(start).as_micros() as u64, Ordering::Relaxed);
    }
    #[test]
    fn target_start_is_once_only_keyed_and_retired_on_replacement() {
        let at = Instant::now();
        let first = DragOperation::new();
        let next = DragOperation::new();
        let timing = NativeBeginTiming {
            generation: 7,
            queued_at: at,
            record,
        };
        let mut slot = TargetStartObservation::default();
        slot.arm(first, timing);
        assert!(slot.take(next).is_none());
        slot.arm(next, timing);
        assert!(slot.take(first).is_none());
        slot.take(next).unwrap().complete(Some(at + Duration::from_micros(42)));
        assert!(slot.take(next).is_none());
        assert_eq!(RECORDED.load(Ordering::Relaxed), 42);
    }
}
