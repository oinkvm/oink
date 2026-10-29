//! Virtual files published through `OleSetClipboard` and a custom `IDataObject`.
//!
//! ## 提供的格式
//! - `CF_HDROP`: target-local Cloud Files placeholders; Windows filesystem
//!   sources use only HDROP, while Mac and Windows virtual sources retain OLE
//!   virtual files alongside it. Unavailable platforms retain virtual files.
//! - `FileGroupDescriptorW`（HGLOBAL）：清单，含目录、大小、修改时间、长文件名（相对路径 ≤ 259 个 UTF-16 单元，
//!   包含更长路径时整批返回 Unsupported，调用方可预取为 CF_HDROP；见 [`super::files::descriptors`]）；
//! - `FileContents`（按 `lindex` 取，`IStream`）：[`FileStream`]，`Read` 从 [`FileContents::open`] 读取，
//!   数据未到时阻塞等待；
//! - received 文件的 `Preferred DropEffect` = COPY | MOVE，原源始终 COPY；同时写入的其他表示形式（文件名文本等）与隐私标记（HGLOBAL）；
//! - Shell 拖拽图像 helper 的私有 IStream 上下文按 COM 引用计数保存并返回。
//! - 粘贴方通过 `SetData` 写回的格式（`Performed DropEffect`、`Paste Succeeded` 等）原样保存、可读回；
//! - `IDataObjectAsyncCapability`：资源管理器据此在后台线程复制并显示进度窗口。
//!
//! ## 线程与套间
//! - 数据对象在剪贴板线程（STA，已 `OleInitialize`）上创建并交给 `OleSetClipboard`，其他进程的调用由 COM
//!   派发到这个线程；`GetData` 都很快（清单预先算好）。
//! - `FileStream` 的 `Read` 会阻塞等网络，不能放在 STA 上（会卡住剪贴板线程、串行化所有调用）。所以每个流在
//!   一个常驻的 MTA 线程上封送（[`Mta`]），STA 拿到的是指向 MTA 的代理，再交给粘贴方——COM 封送代理时会让粘贴方
//!   直连对象所在的 MTA，之后的 `Read` 在 MTA 的 RPC 线程上执行，互不阻塞。两个 COM 对象都不是 agile 的
//!   （不聚合自由线程封送器），否则跨进程封送会退回到当前套间（STA）。
//!
//! ## 取消（`FileContents::cancel`）
//! - `IDataObjectAsyncCapability::EndOperation` 报告失败（用户在进度窗口点「取消」时为 `E_ABORT`）或效果为无；
//! - 某个流已经开始读取、还没读到结尾就被释放（同步粘贴时取消，或粘贴方中途放弃）。
//!
//! ## 进程退出（`OleFlushClipboard` 的取舍）
//! 虚拟文件的内容只能由本进程提供，进程退出后无法再粘贴。退出时（剪贴板线程收到 `WM_CLOSE`）如果剪贴板上仍是
//! 本进程的数据对象：先让它不再提供文件格式，再 `OleFlushClipboard`，把文件名文本等其他表示形式渲染成普通
//! 剪贴板数据留下；这样剪贴板不会留下一个粘贴必然失败的文件清单，也不会在退出时触发整批下载。之后剪贴板上
//! 若仍有 `FileGroupDescriptorW`（系统保留了未渲染的格式），就清空剪贴板再只写回这些表示形式。
//! backend 的 Drop 等待工作线程清理完成（上限 2 秒，同 STA 线程释放时不自等）；若超时会记录警告。
//! 进程被强行结束时来不及处理，系统会丢弃所有延迟渲染的格式。
//!
//! ## 剪贴板管理器
//! 原生调用诊断只记录阶段、长度与结果，不读取调用方身份或记录内容名称。

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read as _;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use windows::Win32::Foundation::{
    CLIPBRD_E_CANT_OPEN, DATA_S_SAMEFORMATETC, DRAGDROP_S_DROP, DV_E_DVASPECT, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED,
    E_NOTIMPL, E_POINTER, FILETIME, GlobalFree, HGLOBAL, OLE_E_ADVISENOTSUPPORTED, S_FALSE, S_OK, STG_E_ACCESSDENIED,
    STG_E_INVALIDFUNCTION, STG_E_READFAULT, STG_E_WRITEFAULT,
};
use windows::Win32::System::Com::Marshal::CoMarshalInterThreadInterfaceInStream;
use windows::Win32::System::Com::StructuredStorage::CoGetInterfaceAndReleaseStream;
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemAlloc, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink,
    IBindCtx, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, ISequentialStream_Impl, IStream,
    IStream_Impl, LOCKTYPE, STATFLAG, STATFLAG_NONAME, STATSTG, STGC, STGM_READ, STGMEDIUM, STGMEDIUM_0, STGTY_STREAM,
    STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::{GetClipboardSequenceNumber, IsClipboardFormatAvailable};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{DROPEFFECT, OleFlushClipboard, OleSetClipboard, ReleaseStgMedium};
use windows::Win32::UI::Shell::{IDataObjectAsyncCapability, IDataObjectAsyncCapability_Impl, SHCreateStdEnumFmtEtc};
use windows::core::{BOOL, HRESULT, IUnknownImpl, Interface, PWSTR, Ref, implement};

use super::files::{DescMeta, Descriptors, FD_FILESIZE, FILEDESCRIPTOR_SIZE};
use crate::{ClipboardError, FileContents};

#[cfg(all(test, debug_assertions))]
mod shell_drop_tests;

/// 写入虚拟文件所需的全部内容（在调用方线程准备好，交给剪贴板线程）。
pub(crate) struct Spec {
    pub cloud: Option<super::cloud::Offer>,
    pub descriptors: Descriptors,
    /// 其他 HGLOBAL 格式：其他表示形式、`Preferred DropEffect`、隐私标记。
    pub fixed: Vec<(u32, Vec<u8>)>,
    pub contents: Arc<dyn FileContents>,
    pub cf_descriptor: u32,
    pub cf_contents: u32,
}

thread_local! {
    /// 剪贴板线程上最近一次交给 `OleSetClipboard` 的数据对象。
    static CURRENT: RefCell<Option<Current>> = const { RefCell::new(None) };
}

struct Current {
    /// 发布时的逻辑序号；OLE 延迟渲染不是用户的新复制。
    published_count: u64,
    data: IDataObject,
    shared: Arc<Shared>,
    /// 其他表示形式（退出时写回用）。
    fixed: Vec<(u32, Vec<u8>)>,
}

/// 调用 `OleSetClipboard` 被占用时的重试间隔（毫秒）。
const RETRY_MS: [u64; 8] = [5, 10, 20, 30, 40, 50, 60, 80];

/// 在剪贴板线程（STA）上执行：放上数据对象，返回写入后的变化计数。
pub(crate) fn set_clipboard(spec: Spec) -> Result<u64, ClipboardError> {
    let fixed = spec.fixed.clone();
    let (data, shared) = build(spec, true);
    publish(data, shared, fixed)
}
pub(crate) fn set_native(
    spec: Spec,
    with_files: bool,
    native: super::content::target::Target,
) -> Result<u64, ClipboardError> {
    let (data, shared) = build_inner(spec, with_files, Some(native));
    publish(data, shared, Vec::new())
}
fn publish(data: IDataObject, shared: Arc<Shared>, fixed: Vec<(u32, Vec<u8>)>) -> Result<u64, ClipboardError> {
    shared.publication.begin();
    let mut last = Ok(());
    for delay in std::iter::once(0).chain(RETRY_MS) {
        if delay > 0 {
            std::thread::sleep(Duration::from_millis(delay));
        }
        // SAFETY: 在已 OleInitialize 的 STA 线程上调用。
        last = unsafe { OleSetClipboard(&data) };
        match &last {
            Err(e) if e.code() == CLIPBRD_E_CANT_OPEN => continue,
            _ => break,
        }
    }
    shared.publication.finish(last.is_ok());
    last.map_err(|e| ClipboardError::Platform(format!("OleSetClipboard: {e}")))?;
    // SAFETY: 无参数的系统调用。
    let published_count = u64::from(unsafe { GetClipboardSequenceNumber() });
    CURRENT.with(|c| {
        *c.borrow_mut() = Some(Current {
            published_count,
            data,
            shared,
            fixed,
        })
    });
    super::worker::publication_changed(Some(published_count));
    Ok(published_count)
}

/// 创建数据对象（在要放它的 STA 线程上调用）。`with_files = false` 时不提供文件格式（只有 `fixed` 里的
/// 表示形式，例如拖回本机的 `CF_HDROP`、文本），`contents` 不会被使用。剪贴板与跨设备拖放
/// （`DoDragDrop`，`super::dnd`）共用。
pub(crate) fn build(spec: Spec, with_files: bool) -> (IDataObject, Arc<Shared>) {
    build_inner(spec, with_files, None)
}
pub(crate) fn build_inner(
    spec: Spec,
    with_files: bool,
    native: Option<super::content::target::Target>,
) -> (IDataObject, Arc<Shared>) {
    let mut spec = spec;
    let shortcut = native.as_ref().and_then(|target| target.shortcut.clone());
    let generated = shortcut.is_some();
    let with_files = with_files || generated;
    if let Some(contents) = shortcut {
        spec.contents = contents;
    }
    let cloud = spec.cloud.map(Arc::new);
    let descriptors = OnceLock::new();
    if !generated && spec.contents.namespace().is_none() {
        let mut prepared = spec.descriptors;
        omit_unknown_sizes(&mut prepared, spec.contents.as_ref());
        let _ = descriptors.set(Ok(prepared));
    }
    let shared = Arc::new(Shared {
        publication: super::content::diagnostics::Publication::new(),
        contents: spec.contents,
        flushing: AtomicBool::new(!with_files),
        cloud_paths: cloud.is_some(),
        cloud: cloud.clone(),
        hosted_drag: AtomicBool::new(false),
        async_running: AtomicBool::new(false),
        drag_diagnostics: OnceLock::new(),
        drag_completion: Mutex::new(DragCompletion::default()),
    });
    // Local virtual providers and generated URL shortcuts need no cloud item.
    // Remote native-file publications reject a missing provider before build.
    let virtual_files = cloud.as_ref().is_none_or(|offer| offer.virtual_files);
    let obj = DataObject {
        cloud,
        virtual_files,
        native,
        shared: Arc::clone(&shared),
        descriptors,
        fixed: spec.fixed,
        cf_descriptor: spec.cf_descriptor,
        cf_contents: spec.cf_contents,
        state: Mutex::new(ObjState {
            async_mode: true,
            in_operation: false,
            stored: HashMap::new(),
            last_drag_window: 0,
        }),
    };
    (obj.into(), shared)
}

/// `OleIsCurrentClipboard`（windows crate 的封装把 `S_FALSE` 也当成功，这里需要区分）。
fn is_current(data: &IDataObject) -> bool {
    clipboard_current(data) == Some(true)
}
fn clipboard_current(data: &IDataObject) -> Option<bool> {
    let trace = oink_foundation::drag_trace::Trace::new(
        oink_foundation::drag_trace::stage::CLIPBOARD_QUERY,
        oink_foundation::drag_trace::Context::current(),
    );
    let result = trace.scope(|| {
        windows_core::link!("ole32.dll" "system" fn OleIsCurrentClipboard(pdataobj: *mut core::ffi::c_void) -> HRESULT);
        // SAFETY: data 是有效的接口指针；在剪贴板线程上调用。
        match unsafe { OleIsCurrentClipboard(data.as_raw()) } {
            S_OK => Some(true),
            S_FALSE => Some(false),
            _ => None,
        }
    });
    trace.result(result.map_or(-1, i32::from));
    result
}

/// 在 STA 上查询逻辑序号：仅精确匹配当前 OLE 对象时抑制延迟渲染产生的系统序号变化。
/// 用户替换了剪贴板（即使数据相同），立即返回真实系统序号；不依赖时间窗口。
pub(crate) fn change_count() -> u64 {
    CURRENT.with(|c| {
        if let Some(current) = c.borrow().as_ref()
            && is_current(&current.data)
        {
            return current.published_count;
        }
        // SAFETY: 无参数的系统调用。
        u64::from(unsafe { GetClipboardSequenceNumber() })
    })
}

/// 剪贴板已被别人（或普通写入）替换时，释放本线程持有的数据对象引用。
pub(crate) fn release_if_replaced() {
    CURRENT.with(|c| {
        let mut cur = c.borrow_mut();
        if let Some(c) = cur.as_ref()
            && !is_current(&c.data)
        {
            *cur = None;
            super::worker::publication_changed(None);
        }
    });
}

/// 普通写入之前调用：剪贴板马上会被清空，数据对象由 OLE 释放；本线程的引用一并释放。
pub(crate) fn forget() {
    super::worker::publication_changed(None);
    CURRENT.with(|c| c.borrow_mut().take());
}

/// 进程退出（剪贴板线程收到 `WM_CLOSE`）时调用，见模块文档。剪贴板上仍留有文件清单时调用
/// `clear_and_write` 清空剪贴板并写回其他表示形式。
pub(crate) fn shutdown(cf_descriptor: u32, clear_and_write: impl FnOnce(&[(u32, Vec<u8>)])) {
    super::worker::publication_changed(None);
    let Some(cur) = CURRENT.with(|c| c.borrow_mut().take()) else {
        return;
    };
    if !is_current(&cur.data) {
        return;
    }
    disable_files(&cur.shared);
    // SAFETY: 同上。
    if let Err(e) = unsafe { OleFlushClipboard() } {
        tracing::warn!("OleFlushClipboard 失败：{e}");
    }
    // SAFETY: 只查询格式是否存在。
    let still = unsafe { IsClipboardFormatAvailable(cf_descriptor) }.is_ok()
        // SAFETY: only checks whether our dynamic cloud-path format remains.
        || (cur.shared.cloud_paths && unsafe { IsClipboardFormatAvailable(15) }.is_ok());
    if still {
        tracing::info!("退出时剪贴板上仍有文件清单，清空后只写回其他表示形式");
        clear_and_write(&cur.fixed);
    }
}

/// 数据对象与其流共享的状态。
pub(crate) struct Shared {
    publication: super::content::diagnostics::Publication,
    contents: Arc<dyn FileContents>,
    /// 退出前的 `OleFlushClipboard`：不再提供文件格式。
    flushing: AtomicBool,
    cloud_paths: bool,
    cloud: Option<Arc<super::cloud::Offer>>,
    hosted_drag: AtomicBool,
    async_running: AtomicBool,
    drag_diagnostics: OnceLock<Arc<super::dnd::drop_trace::Session>>,
    drag_completion: Mutex<DragCompletion>,
}

#[derive(Default)]
struct DragCompletion {
    logical: Option<u32>,
    performed: Option<u32>,
    pasted: Option<u32>,
    ended: Option<(bool, bool)>,
    asynchronous: Option<(HRESULT, u32)>,
}
#[derive(Clone, Copy)]
enum MoveAction {
    DeleteReceived,
    ConvergeIdentity,
}
impl DragCompletion {
    // Microsoft source rules: delete only the unoptimized MOVE combination.
    // SetData outcomes precede EndOperation for asynchronous transfers; the
    // caller defers this decision while InOperation is true. No consumption or
    // residency check can replace the target's report (user, 2026-10-29).
    // https://learn.microsoft.com/en-us/windows/win32/shell/datascenarios
    fn move_action(&self) -> Option<MoveAction> {
        if self.ended.is_some_and(|(dropped, _)| !dropped) {
            return None;
        }
        let performed_move = self.performed == Some(2);
        if let Some((result, effect)) = self.asynchronous {
            if result.is_err() {
                return None;
            }
            if self.pasted == Some(2) {
                return Some(if performed_move {
                    MoveAction::DeleteReceived
                } else {
                    MoveAction::ConvergeIdentity
                });
            }
            if effect == 2 && performed_move {
                return Some(MoveAction::DeleteReceived);
            }
            return (effect == 2 || performed_move || self.logical == Some(2) || self.ended == Some((true, true)))
                .then_some(MoveAction::ConvergeIdentity);
        }
        if self.pasted == Some(2) {
            return Some(if performed_move {
                MoveAction::DeleteReceived
            } else {
                MoveAction::ConvergeIdentity
            });
        }
        let (dropped, moved) = self.ended?;
        if !dropped {
            return None;
        }
        if moved && performed_move {
            return Some(MoveAction::DeleteReceived);
        }
        (moved || performed_move || self.logical == Some(2)).then_some(MoveAction::ConvergeIdentity)
    }
    fn accepted(&self, hr: HRESULT, effect: DROPEFFECT, pending: bool) -> bool {
        if hr != DRAGDROP_S_DROP {
            return false;
        }
        let transferred = |effect: u32| effect & 7 != 0; // COPY | MOVE | LINK, excluding SCROLL.
        if let Some((result, _)) = self.asynchronous {
            // EndOperation's HRESULT reports extraction success. Its effect is
            // the physical performed effect, which is NONE for optimized MOVE.
            return result.is_ok();
        }
        pending || transferred(effect.0) || self.logical.is_some_and(transferred)
    }
}

pub(super) fn disable_files(shared: &Shared) {
    shared.flushing.store(true, Ordering::SeqCst);
}

impl Shared {
    /// Explorer can return NONE for a pending/optimized MOVE, then read our
    /// IDataObject after DoDragDrop. Cancelling here revoked CIDA/HDROP before
    /// that read (Windows 11). InOperation and logical completion distinguish
    /// this accepted drop from rejection; EndOperation still owns late failure.
    /// A cancelled native loop never becomes accepted, and this grants no right
    /// to delete the original remote file or infer physical MOVE cleanup.
    pub(super) fn drag_dropped(&self, hr: HRESULT, effect: DROPEFFECT) -> bool {
        self.drag_completion
            .lock()
            .unwrap()
            .accepted(hr, effect, self.async_running.load(Ordering::Acquire))
    }
    pub(super) fn observe_drag(&self, diagnostics: Arc<super::dnd::drop_trace::Session>) {
        let _ = self.drag_diagnostics.set(diagnostics);
    }
    pub(super) fn host_drag_images(&self) {
        self.hosted_drag.store(true, Ordering::Relaxed);
    }
    fn cancel(&self, why: &str) {
        tracing::info!("虚拟文件：{why}，通知取消");
        // Each hosted drag owns a provisional namespace. A reusable clipboard
        // object can already have completed copies from earlier pastes.
        if self.hosted_drag.load(Ordering::Relaxed) {
            self.flushing.store(true, Ordering::SeqCst);
            if let Some(cloud) = &self.cloud {
                cloud.cancel();
            }
        }
        self.contents.cancel();
    }
    fn move_completed(&self) {
        if let Some(cloud) = &self.cloud
            && let Err(error) = cloud.move_completed()
        {
            tracing::debug!(%error, "native MOVE intent not persisted; retain received copy");
        }
    }
    fn reconcile_move(&self) {
        if self.async_running.load(Ordering::Acquire) {
            return;
        }
        let action = self.drag_completion.lock().unwrap().move_action();
        let Some(cloud) = &self.cloud else {
            return;
        };
        let result = match action {
            Some(MoveAction::DeleteReceived) => {
                self.move_completed();
                return;
            }
            Some(MoveAction::ConvergeIdentity) => cloud.optimized_move_completed(),
            None => return,
        };
        if let Err(error) = result {
            tracing::debug!(%error, "optimized MOVE identity convergence will retry");
        }
    }
    pub(super) fn drag_ended(&self, dropped: bool, moved: bool) {
        self.drag_completion.lock().unwrap().ended = Some((dropped, moved));
        if !dropped {
            self.cancel("native drag cancelled or failed");
        } else {
            self.reconcile_move();
        }
    }
}

#[derive(Clone)]
enum StoredMedium {
    Bytes(Vec<u8>),
    Stream(IStream),
}
impl StoredMedium {
    fn tymed(&self) -> u32 {
        match self {
            Self::Bytes(_) => TYMED_HGLOBAL.0 as u32,
            Self::Stream(_) => TYMED_ISTREAM.0 as u32,
        }
    }
    fn medium(&self) -> windows::core::Result<STGMEDIUM> {
        match self {
            Self::Bytes(bytes) => medium_hglobal(bytes),
            Self::Stream(stream) => Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 {
                    pstm: ManuallyDrop::new(Some(stream.clone())),
                },
                pUnkForRelease: ManuallyDrop::new(None),
            }),
        }
    }
}
struct ObjState {
    async_mode: bool,
    in_operation: bool,
    /// Shell also writes an IStream drag context when initializing artwork.
    stored: HashMap<u16, StoredMedium>,
    last_drag_window: u32,
}

#[implement(IDataObject, IDataObjectAsyncCapability, Agile = false)]
struct DataObject {
    cloud: Option<Arc<super::cloud::Offer>>,
    virtual_files: bool,
    native: Option<super::content::target::Target>,
    shared: Arc<Shared>,
    descriptors: OnceLock<Result<Descriptors, HRESULT>>,
    fixed: Vec<(u32, Vec<u8>)>,
    cf_descriptor: u32,
    cf_contents: u32,
    state: Mutex<ObjState>,
}

fn hglobal_from(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
    // SAFETY: 分配可移动全局内存并复制数据。
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len().max(1))?;
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(h));
            return Err(E_POINTER.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        Ok(h)
    }
}

pub(super) fn hglobal_bytes(h: HGLOBAL) -> Option<Vec<u8>> {
    // SAFETY: 句柄由调用方提供，锁定期间只读取 GlobalSize 字节。
    unsafe {
        let size = GlobalSize(h);
        let p = GlobalLock(h) as *const u8;
        if p.is_null() {
            return None;
        }
        let v = std::slice::from_raw_parts(p, size).to_vec();
        let _ = GlobalUnlock(h);
        Some(v)
    }
}

fn medium_hglobal(bytes: &[u8]) -> windows::core::Result<STGMEDIUM> {
    Ok(STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 {
            hGlobal: hglobal_from(bytes)?,
        },
        pUnkForRelease: ManuallyDrop::new(None),
    })
}

/// Apply the provider's knowledge to every entry, including flat providers.
/// Zero is a known empty file only when the provider says its size is known.
fn omit_unknown_sizes(descriptors: &mut Descriptors, contents: &dyn FileContents) {
    for (index, entry) in descriptors.index.iter().enumerate() {
        if !contents.descriptor_size_known(*entry) {
            let at = 4 + index * FILEDESCRIPTOR_SIZE;
            let flags = u32::from_le_bytes(descriptors.bytes[at..at + 4].try_into().unwrap()) & !FD_FILESIZE;
            descriptors.bytes[at..at + 4].copy_from_slice(&flags.to_le_bytes());
        }
    }
}

impl DataObject {
    fn original_descriptor(&self, name: &str) -> bool {
        self.native.as_ref().is_some_and(|native| native.spec.inventory.items.iter().flat_map(|item| &item.representations).any(|rep| {
            matches!(&rep.format, crate::model::NativeFormat::WindowsRegistered(format) if format.eq_ignore_ascii_case(name))
                && matches!(rep.data, crate::model::RepresentationData::File(_))
        }))
    }
    fn wide_descriptor(&self) -> bool {
        !self.original_descriptor("FileGroupDescriptor") || self.original_descriptor("FileGroupDescriptorW")
    }
    fn ansi_descriptor(&self) -> Option<u32> {
        (self.original_descriptor("FileGroupDescriptor")
            || self
                .descriptors
                .get()
                .is_some_and(|value| value.as_ref().is_ok_and(|value| value.ansi.is_some())))
        .then(|| {
            super::content::format_id(&crate::model::NativeFormat::WindowsRegistered(
                "FileGroupDescriptor".into(),
            ))
            .map(u32::from)
        })
        .flatten()
    }
    fn path_formats(&self) -> Vec<(u32, &'static str)> {
        if self.cloud.is_none() || self.shared.flushing.load(Ordering::SeqCst) {
            return vec![];
        }
        let Some(native) = self.native.as_ref() else {
            return vec![];
        };
        let inventory = &native.spec.inventory;
        if !inventory.items.iter().any(|item| item.file.is_some())
            || inventory.items.iter().any(|item| item.virtual_files)
        {
            return vec![];
        }
        let names: &[&str] = if inventory.roots.len() == 1 {
            &["Shell IDList Array", "FileNameW", "FileName"]
        } else {
            &["Shell IDList Array"]
        };
        names
            .iter()
            .filter_map(|name| {
                super::content::format_id(&crate::model::NativeFormat::WindowsRegistered((*name).into()))
                    .map(|id| (id as u32, *name))
            })
            .collect()
    }
    fn descriptors(&self) -> windows::core::Result<&Descriptors> {
        self.descriptors
            .get_or_init(|| {
                let entries = self
                    .shared
                    .contents
                    .descriptor_entries()
                    .map_err(|_| STG_E_READFAULT)?
                    .ok_or(STG_E_READFAULT)?;
                if entries.len() > 65_535 {
                    return Err(STG_E_READFAULT);
                }
                let native = self
                    .shared
                    .contents
                    .namespace()
                    .map(|namespace| namespace.native_file_metadata())
                    .unwrap_or_default();
                let mut descriptors = if native.windows.is_empty() {
                    super::files::descriptors(&entries)
                } else {
                    let nodes = self
                        .shared
                        .contents
                        .descriptor_nodes()
                        .map_err(|_| STG_E_READFAULT)?
                        .ok_or(STG_E_READFAULT)?;
                    super::files::native_descriptors(&entries, &nodes, &native).map_err(|_| STG_E_READFAULT)?
                };
                if descriptors.skipped_long > 0 || descriptors.bytes.len() > crate::model::INVENTORY_BYTES {
                    return Err(STG_E_READFAULT);
                }
                omit_unknown_sizes(&mut descriptors, self.shared.contents.as_ref());
                Ok(descriptors)
            })
            .as_ref()
            .map_err(|hr| (*hr).into())
    }
    fn check(&self, f: &FORMATETC) -> HRESULT {
        if self.native.as_ref().is_some_and(|n| n.check(f)) {
            return S_OK;
        }
        if f.dwAspect != DVASPECT_CONTENT.0 {
            return DV_E_DVASPECT;
        }
        let cf = u32::from(f.cfFormat);
        let tymed = f.tymed;
        let descriptor = (cf == self.cf_descriptor && self.wide_descriptor()) || self.ansi_descriptor() == Some(cf);
        if (cf == self.cf_descriptor && !self.wide_descriptor())
            || (!self.virtual_files && (descriptor || cf == self.cf_contents))
        {
            return DV_E_FORMATETC;
        }
        let cloud_fmt = (cf == 15 && self.cloud.is_some()) || self.path_formats().iter().any(|(id, _)| *id == cf);
        let file_fmt = descriptor || cf == self.cf_contents || cloud_fmt;
        if file_fmt && self.shared.flushing.load(Ordering::SeqCst) {
            return DV_E_FORMATETC;
        }
        if cf == self.cf_contents {
            if tymed & TYMED_ISTREAM.0 as u32 == 0 {
                return DV_E_TYMED;
            }
            // lindex = -1 表示「整体」，在 QueryGetData 里允许（有的调用方这样探测）
            if f.lindex != -1 && !self.is_file_index(f.lindex) {
                return DV_E_LINDEX;
            }
            return S_OK;
        }
        if let Some(stored) = self.state.lock().ok().and_then(|s| s.stored.get(&f.cfFormat).cloned()) {
            return if tymed & stored.tymed() != 0 { S_OK } else { DV_E_TYMED };
        }
        let known = cloud_fmt
            || descriptor
            || self.fixed.iter().any(|(c, _)| *c == cf)
            || self
                .state
                .lock()
                .map(|s| s.stored.contains_key(&f.cfFormat))
                .unwrap_or(false);
        if !known {
            return DV_E_FORMATETC;
        }
        if tymed & TYMED_HGLOBAL.0 as u32 == 0 {
            return DV_E_TYMED;
        }
        S_OK
    }

    fn is_file_index(&self, lindex: i32) -> bool {
        let Ok(index) = usize::try_from(lindex) else {
            return false;
        };
        // QueryGetData advertises capability without forcing tree expansion.
        self.descriptors.get().is_none_or(|result| {
            result
                .as_ref()
                .is_ok_and(|d| d.meta.get(index).is_some_and(|m| !m.is_dir))
        })
    }

    fn formats(&self) -> Vec<FORMATETC> {
        let fe = |cf: u32, tymed: u32| FORMATETC {
            cfFormat: cf as u16,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed,
        };
        let mut files = Vec::new();
        if !self.shared.flushing.load(Ordering::SeqCst) {
            if self.virtual_files {
                if self.wide_descriptor() {
                    files.push(fe(self.cf_descriptor, TYMED_HGLOBAL.0 as u32));
                }
                if let Some(ansi) = self.ansi_descriptor() {
                    files.push(fe(ansi, TYMED_HGLOBAL.0 as u32));
                }
                files.push(fe(self.cf_contents, TYMED_ISTREAM.0 as u32));
            }
            if self.cloud.is_some() {
                files.push(fe(15, TYMED_HGLOBAL.0 as u32));
                files.extend(
                    self.path_formats()
                        .iter()
                        .map(|(id, _)| fe(*id, TYMED_HGLOBAL.0 as u32)),
                );
            }
        }
        let mut v = self
            .native
            .as_ref()
            .map_or_else(|| files.clone(), |native| native.formats_with_files(&files));
        for (cf, _) in &self.fixed {
            v.push(fe(*cf, TYMED_HGLOBAL.0 as u32));
        }
        if let Ok(s) = self.state.lock() {
            for (cf, medium) in &s.stored {
                if !self.fixed.iter().any(|(c, _)| *c == u32::from(*cf)) {
                    v.push(fe(u32::from(*cf), medium.tymed()));
                }
            }
        }
        v
    }
}

fn data_call(shared: &Shared, stage: u16, format: Option<&FORMATETC>, aux: u32) -> oink_foundation::drag_trace::Trace {
    let mut context = oink_foundation::drag_trace::Context {
        object: shared.publication.observe(None).object,
        format: 0,
        aspect: 0,
        tymed: 0,
        index: -1,
        aux,
        ..oink_foundation::drag_trace::Context::current()
    };
    if let Some(f) = format {
        context.format = f.cfFormat;
        context.aspect = f.dwAspect;
        context.tymed = f.tymed;
        context.index = f.lindex;
        context.aux |= u32::from(!f.ptd.is_null()) << 31;
        let formats = super::formats();
        let class = match u32::from(f.cfFormat) {
            13 => 1,
            15 => 2,
            cf if cf == formats.html => 3,
            cf if cf == formats.file_descriptor => 4,
            cf if cf == formats.file_contents => 5,
            cf if cf == formats.png => 6,
            cf if cf == formats.rtf => 7,
            _ => 0,
        };
        context.aux |= class << 16;
    }
    oink_foundation::drag_trace::Trace::new(stage, context)
}

impl IDataObject_Impl for DataObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let callback = crate::content::native_read::Callback::begin();
        // SAFETY: COM owns the optional FORMATETC for this callback. No pointed-to data is logged.
        let trace = data_call(
            &self.shared,
            oink_foundation::drag_trace::stage::GET_DATA,
            unsafe { pformatetcin.as_ref() },
            0,
        );
        let result = trace.scope(|| {
            // SAFETY: COM 保证参数指针在调用期间有效（为空时返回错误）。
            let Some(f) = (unsafe { pformatetcin.as_ref() }) else {
                return Err(E_POINTER.into());
            };
            if let Some(native) = self.native.as_ref().filter(|n| n.check(f)) {
                return native.get(f, || {
                    // GetData runs on this object's STA. Exact OLE identity avoids
                    // treating delayed rendering's sequence-number changes as a new
                    // clipboard, and does not borrow CURRENT during OLE reentrancy.
                    self.shared
                        .publication
                        .observe(clipboard_current(&self.to_interface::<IDataObject>()))
                });
            }
            let hr = self.check(f);
            if hr != S_OK {
                return Err(hr.into());
            }
            let cf = u32::from(f.cfFormat);
            let path_name = self
                .path_formats()
                .into_iter()
                .find(|(id, _)| *id == cf)
                .map(|(_, name)| name);
            if (cf == 15 || path_name.is_some())
                && let Some(cloud) = &self.cloud
            {
                tracing::info!(target: "oink_file_diagnostics", file_stage = 20_u8,
                file_object = self.shared.publication.observe(None).object, file_format = 1_u8,
                "native file format requested");
                let bytes = cloud.hdrop().map_err(|error| {
                    tracing::warn!(
                        code = error.raw_os_error(),
                        reason = crate::content::reason(&error) as u8,
                        "cloud file publication failed"
                    );
                    windows::core::Error::from(STG_E_READFAULT)
                })?;
                let bytes = if let Some(name) = path_name {
                    super::files::path_equivalent(name, &bytes)
                        .map_err(|_| windows::core::Error::from(STG_E_READFAULT))?
                } else {
                    bytes
                };
                if let Some(diagnostics) = self.shared.drag_diagnostics.get() {
                    diagnostics.bytes(cf, &bytes);
                }
                return medium_hglobal(&bytes);
            }
            if cf == self.cf_contents {
                if let Some(cloud) = &self.cloud {
                    cloud.hdrop().map_err(|_| windows::core::Error::from(STG_E_READFAULT))?;
                }
                let descriptors = self.descriptors()?;
                let Some(i) = usize::try_from(f.lindex)
                    .ok()
                    .filter(|i| descriptors.meta.get(*i).is_some_and(|m| !m.is_dir))
                else {
                    return Err(DV_E_LINDEX.into());
                };
                let DescMeta { name, size, mtime, .. } = descriptors.meta[i].clone();
                let entry = descriptors.index[i];
                tracing::info!(target: "oink_file_diagnostics", file_stage = 20_u8,
                file_object = self.shared.publication.observe(None).object, file_format = 2_u8,
                file_index = entry, size, "native file content requested");
                let lease = self
                    .shared
                    .contents
                    .namespace()
                    .map(|files| files.native_lease())
                    .transpose()
                    .map_err(|_| windows::core::Error::from(STG_E_READFAULT))?;
                let stream = FileStream {
                    _lease: lease,
                    shared: Arc::clone(&self.shared),
                    entry,
                    name,
                    size,
                    declared_size: {
                        let at = 4 + i * FILEDESCRIPTOR_SIZE;
                        let flags = u32::from_le_bytes(descriptors.bytes[at..at + 4].try_into().unwrap());
                        (flags & FD_FILESIZE != 0).then_some(size)
                    },
                    mtime,
                    st: Mutex::new(StreamState::default()),
                };
                let proxy = mta_stream(stream)?;
                return Ok(STGMEDIUM {
                    tymed: TYMED_ISTREAM.0 as u32,
                    u: STGMEDIUM_0 {
                        pstm: ManuallyDrop::new(Some(proxy)),
                    },
                    pUnkForRelease: ManuallyDrop::new(None),
                });
            }
            if cf == self.cf_descriptor {
                tracing::debug!("native file descriptors requested");
                return medium_hglobal(&self.descriptors()?.bytes);
            }
            if self.ansi_descriptor() == Some(cf) {
                return medium_hglobal(self.descriptors()?.ansi.as_ref().ok_or(STG_E_READFAULT)?);
            }
            if let Some((_, b)) = self.fixed.iter().find(|(c, _)| *c == cf) {
                if let Some(diagnostics) = self.shared.drag_diagnostics.get() {
                    diagnostics.bytes(cf, b);
                }
                return medium_hglobal(b);
            }
            let stored = self.state.lock().ok().and_then(|s| s.stored.get(&f.cfFormat).cloned());
            match stored {
                Some(medium) => medium.medium(),
                None => Err(DV_E_FORMATETC.into()),
            }
        });
        trace.result(
            result
                .as_ref()
                .err()
                .map_or(0, |error: &windows::core::Error| error.code().0),
        );
        if result.is_ok() {
            callback.served();
        }
        // SAFETY: COM keeps the optional input FORMATETC alive for this call.
        let format = unsafe { pformatetcin.as_ref() };
        if let Some(diagnostics) = self.shared.drag_diagnostics.get()
            && let Some(format) = format
        {
            diagnostics.format(2, format, result.as_ref().err().map_or(0, |error| error.code().0));
        }
        result
    }

    fn GetDataHere(&self, _pformatetc: *const FORMATETC, _pmedium: *mut STGMEDIUM) -> windows::core::Result<()> {
        // SAFETY: COM owns the optional FORMATETC for this callback. No pointed-to data is logged.
        let trace = data_call(
            &self.shared,
            oink_foundation::drag_trace::stage::GET_DATA_HERE,
            unsafe { _pformatetc.as_ref() },
            0,
        );
        let result = trace.scope(|| Err(E_NOTIMPL.into()));
        trace.result(
            result
                .as_ref()
                .err()
                .map_or(0, |error: &windows::core::Error| error.code().0),
        );
        result
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        // SAFETY: COM owns the optional FORMATETC for this callback. No pointed-to data is logged.
        let trace = data_call(
            &self.shared,
            oink_foundation::drag_trace::stage::QUERY_DATA,
            unsafe { pformatetc.as_ref() },
            0,
        );
        let result = trace.scope(|| {
            // SAFETY: 同 GetData。
            match unsafe { pformatetc.as_ref() } {
                Some(f) => self.check(f),
                None => E_POINTER,
            }
        });
        trace.result(result.0);
        // SAFETY: COM keeps the optional input FORMATETC alive for this call.
        let format = unsafe { pformatetc.as_ref() };
        if let Some(diagnostics) = self.shared.drag_diagnostics.get()
            && let Some(format) = format
        {
            diagnostics.format(1, format, result.0);
        }
        result
    }

    fn GetCanonicalFormatEtc(&self, pformatectin: *const FORMATETC, pformatetcout: *mut FORMATETC) -> HRESULT {
        // SAFETY: COM owns the optional FORMATETC for this callback. No pointed-to data is logged.
        let trace = data_call(
            &self.shared,
            oink_foundation::drag_trace::stage::OTHER_COM,
            unsafe { pformatectin.as_ref() },
            1,
        );
        let result = trace.scope(|| {
            // SAFETY: 同 GetData；输出参数由调用方提供。
            unsafe {
                if pformatetcout.is_null() {
                    return E_POINTER;
                }
                if let Some(f) = pformatectin.as_ref() {
                    *pformatetcout = *f;
                    (*pformatetcout).ptd = std::ptr::null_mut();
                }
            }
            DATA_S_SAMEFORMATETC
        });
        trace.result(result.0);
        result
    }

    fn SetData(
        &self,
        pformatetc: *const FORMATETC,
        pmedium: *const STGMEDIUM,
        frelease: BOOL,
    ) -> windows::core::Result<()> {
        // SAFETY: COM owns the optional FORMATETC for this callback. No pointed-to data is logged.
        let trace = data_call(
            &self.shared,
            oink_foundation::drag_trace::stage::OTHER_COM,
            unsafe { pformatetc.as_ref() },
            2,
        );
        let result = trace.scope(|| {
            // SAFETY: 参数指针在调用期间有效；只读取 HGLOBAL 内容；fRelease 时按约定由我们释放介质。
            unsafe {
                let (Some(f), Some(m)) = (pformatetc.as_ref(), pmedium.as_ref()) else {
                    return Err(E_POINTER.into());
                };
                let value = if m.tymed == TYMED_HGLOBAL.0 as u32 {
                    StoredMedium::Bytes(hglobal_bytes(m.u.hGlobal).ok_or(E_POINTER)?)
                } else if m.tymed == TYMED_ISTREAM.0 as u32 {
                    StoredMedium::Stream(m.u.pstm.as_ref().ok_or(E_POINTER)?.clone())
                } else {
                    tracing::warn!(format = f.cfFormat, medium = m.tymed, "unsupported SetData medium");
                    return Err(DV_E_TYMED.into());
                };
                if let Some(diagnostics) = self.shared.drag_diagnostics.get()
                    && let StoredMedium::Bytes(bytes) = &value
                {
                    diagnostics.receipt(f.cfFormat, bytes);
                }
                if let StoredMedium::Bytes(bytes) = &value
                    && let Some(effect) = bytes.get(..4)
                {
                    let effect = u32::from_le_bytes(effect.try_into().unwrap());
                    let logical = windows::Win32::System::DataExchange::RegisterClipboardFormatW(windows::core::w!(
                        "Logical Performed DropEffect"
                    ));
                    let performed = windows::Win32::System::DataExchange::RegisterClipboardFormatW(windows::core::w!(
                        "Performed DropEffect"
                    ));
                    let pasted = windows::Win32::System::DataExchange::RegisterClipboardFormatW(windows::core::w!(
                        "Paste Succeeded"
                    ));
                    if f.cfFormat as u32 == logical {
                        self.shared.drag_completion.lock().unwrap().logical = Some(effect);
                    }
                    if f.cfFormat as u32 == performed {
                        self.shared.drag_completion.lock().unwrap().performed = Some(effect);
                    }
                    if f.cfFormat as u32 == pasted {
                        self.shared.drag_completion.lock().unwrap().pasted = Some(effect);
                    }
                    if [logical, performed, pasted].contains(&(f.cfFormat as u32)) {
                        self.shared.reconcile_move();
                    }
                }
                // Each helper/process may cache a different drag-image window.
                // Keep exactly one published window for a hosted drag, including
                // thumbnail replacement and native target reentry. A zero value
                // ends a context; remember its last window until a new nonzero
                // one arrives. Clipboard objects never enable this policy.
                let next_window = if self.shared.hosted_drag.load(Ordering::Relaxed)
                    && f.cfFormat as u32
                        == windows::Win32::System::DataExchange::RegisterClipboardFormatW(windows::core::w!(
                            "DragWindow"
                        ))
                    && let StoredMedium::Bytes(bytes) = &value
                {
                    bytes
                        .get(..4)
                        .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
                        .filter(|window| *window != 0)
                } else {
                    None
                };
                let retire = if let Ok(mut s) = self.state.lock() {
                    let previous = next_window
                        .filter(|next| s.last_drag_window != 0 && s.last_drag_window != *next)
                        .map(|_| s.last_drag_window);
                    if let Some(next) = next_window {
                        s.last_drag_window = next;
                    }
                    s.stored.insert(f.cfFormat, value);
                    previous
                } else {
                    None
                };
                // Never hold the data object's lock across a window operation.
                if let Some(previous) = retire {
                    super::drag_image::retire_window(previous);
                }
                if frelease.as_bool() {
                    ReleaseStgMedium(pmedium as *mut STGMEDIUM);
                }
            }
            Ok(())
        });
        trace.result(
            result
                .as_ref()
                .err()
                .map_or(0, |error: &windows::core::Error| error.code().0),
        );
        result
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        // SAFETY: COM owns the optional FORMATETC for this callback. No pointed-to data is logged.
        let trace = data_call(
            &self.shared,
            oink_foundation::drag_trace::stage::ENUM_FORMATS,
            None,
            dwdirection,
        );
        let result = trace.scope(|| {
            if dwdirection != DATADIR_GET.0 as u32 {
                return Err(E_NOTIMPL.into());
            }
            // SAFETY: 数组在调用期间有效，系统复制一份。
            let formats = self.formats();
            for (index, format) in formats.iter().enumerate() {
                if let Some(diagnostics) = self.shared.drag_diagnostics.get() {
                    diagnostics.format(0, format, 0);
                }
                let entry = data_call(
                    &self.shared,
                    oink_foundation::drag_trace::stage::ENUM_FORMATS,
                    Some(format),
                    index as u32 | (1 << 30),
                );
                entry.result(0);
            }
            // SAFETY: the system copies the valid FORMATETC slice before returning.
            unsafe { SHCreateStdEnumFmtEtc(&formats) }
        });
        trace.result(
            result
                .as_ref()
                .err()
                .map_or(0, |error: &windows::core::Error| error.code().0),
        );
        result
    }

    fn DAdvise(&self, _f: *const FORMATETC, _advf: u32, _sink: Ref<'_, IAdviseSink>) -> windows::core::Result<u32> {
        // SAFETY: COM owns the optional FORMATETC for this callback. No pointed-to data is logged.
        let trace = data_call(
            &self.shared,
            oink_foundation::drag_trace::stage::OTHER_COM,
            unsafe { _f.as_ref() },
            3,
        );
        let result = trace.scope(|| Err(OLE_E_ADVISENOTSUPPORTED.into()));
        trace.result(
            result
                .as_ref()
                .err()
                .map_or(0, |error: &windows::core::Error| error.code().0),
        );
        result
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
        // SAFETY: COM owns the optional FORMATETC for this callback. No pointed-to data is logged.
        let trace = data_call(&self.shared, oink_foundation::drag_trace::stage::OTHER_COM, None, 4);
        let result = trace.scope(|| Err(OLE_E_ADVISENOTSUPPORTED.into()));
        trace.result(
            result
                .as_ref()
                .err()
                .map_or(0, |error: &windows::core::Error| error.code().0),
        );
        result
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        // SAFETY: COM owns the optional FORMATETC for this callback. No pointed-to data is logged.
        let trace = data_call(&self.shared, oink_foundation::drag_trace::stage::OTHER_COM, None, 5);
        let result = trace.scope(|| Err(OLE_E_ADVISENOTSUPPORTED.into()));
        trace.result(
            result
                .as_ref()
                .err()
                .map_or(0, |error: &windows::core::Error| error.code().0),
        );
        result
    }
}

impl IDataObjectAsyncCapability_Impl for DataObject_Impl {
    fn SetAsyncMode(&self, fdoopasync: BOOL) -> windows::core::Result<()> {
        if let Ok(mut s) = self.state.lock() {
            s.async_mode = fdoopasync.as_bool();
        }
        Ok(())
    }

    fn GetAsyncMode(&self) -> windows::core::Result<BOOL> {
        Ok(self.state.lock().map(|s| s.async_mode).unwrap_or(true).into())
    }

    fn StartOperation(&self, _pbcreserved: Ref<'_, IBindCtx>) -> windows::core::Result<()> {
        if let Some(diagnostics) = self.shared.drag_diagnostics.get() {
            diagnostics.async_start();
        }
        tracing::debug!("native asynchronous file operation started");
        *self.shared.drag_completion.lock().unwrap() = DragCompletion::default();
        self.shared.async_running.store(true, Ordering::Release);
        if let Ok(mut s) = self.state.lock() {
            s.in_operation = true;
        }
        Ok(())
    }

    fn InOperation(&self) -> windows::core::Result<BOOL> {
        Ok(self.state.lock().map(|s| s.in_operation).unwrap_or(false).into())
    }

    fn EndOperation(
        &self,
        hresult: HRESULT,
        _pbcreserved: Ref<'_, IBindCtx>,
        dweffects: u32,
    ) -> windows::core::Result<()> {
        if let Some(diagnostics) = self.shared.drag_diagnostics.get() {
            diagnostics.async_end(hresult.0, dweffects);
        }
        tracing::debug!(result = ?hresult, effect = dweffects, "native asynchronous file operation ended");
        self.shared.drag_completion.lock().unwrap().asynchronous = Some((hresult, dweffects));
        if let Ok(mut s) = self.state.lock() {
            s.in_operation = false;
        }
        self.shared.async_running.store(false, Ordering::Release);
        // EndOperation uses CFSTR_PERFORMEDDROPEFFECT semantics: NONE is a
        // successful optimized MOVE, not failed extraction (the HRESULT owns
        // that result). Never cancel delivered data solely for this effect.
        if hresult.is_err() {
            self.shared.cancel("粘贴方结束了异步操作但没有成功");
        } else {
            self.shared.reconcile_move();
        }

        Ok(())
    }
}

// ---------- 文件内容流 ----------

#[derive(Default)]
struct StreamState {
    reader: Option<crate::FileReader>,
    /// 读取器当前所在的偏移。
    reader_pos: u64,
    /// 流的逻辑位置（`Seek` 改变它，`Read` 时再对齐读取器）。
    pos: u64,
    /// 当前读到了结尾。
    eof: bool,
    /// 曾经完整读完过一次（之后即使 Seek 回去再释放，也不算取消）。
    completed: bool,
    logged_caller: bool,
    consumed_to: u64,
    proof_sent: bool,
}

#[implement(IStream, Agile = false)]
struct FileStream {
    shared: Arc<Shared>,
    /// 清单下标（`FileContents::open` 的参数）。
    entry: usize,
    name: String,
    size: u64,
    declared_size: Option<u64>,
    mtime: u64,
    st: Mutex<StreamState>,
    // GetData transfers an unread consumer to OLE. Its last COM reference,
    // rather than the first/last Read call, determines when cleanup may run.
    _lease: Option<crate::content::namespace::NativeLease>,
}

impl FileStream {
    fn consumed_range(&self, st: &mut StreamState, start: u64, count: usize) {
        if start <= st.consumed_to {
            st.consumed_to = st.consumed_to.max(start + count as u64);
        }
        if !st.proof_sent
            && st.eof
            && st.consumed_to == st.pos
            && let Some(cloud) = &self.shared.cloud
        {
            match cloud.target_consumed(self.entry) {
                Ok(()) => st.proof_sent = true,
                Err(error) => {
                    tracing::debug!(%error, "stream consumption proof not persisted; retain received copy")
                }
            }
        }
    }

    fn current_size(&self) -> std::io::Result<u64> {
        self.metadata_size(self.shared.contents.metadata(self.entry)?)
    }

    fn metadata_size(&self, metadata: Option<crate::FileMetadata>) -> std::io::Result<u64> {
        let size = metadata.map_or(self.size, |m| m.size);
        // Targets may trust FD_FILESIZE and read only that many bytes, or skip
        // an advertised empty file. Once published, a descriptor is immutable.
        // If selection discovers a different size, fail rather than silently
        // delivering a prefix; unknown sizes still resolve lazily through Stat.
        if self.declared_size.is_some_and(|declared| declared != size) {
            return Err(crate::content::unavailable(crate::model::Unavailable::Superseded));
        }
        Ok(size)
    }

    /// 读满 `buf`（读到结尾时可以不满），返回读到的字节数。
    fn read_into(&self, st: &mut StreamState, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let size = self.current_size()?;
        if st.pos >= size {
            st.eof = true;
            // Seek 到末尾本身不等于读完整个文件。
            st.completed |= size == 0;
            return Ok(0);
        }
        let want = (size - st.pos).min(buf.len() as u64) as usize;
        let buf = &mut buf[..want];
        if !st.logged_caller {
            st.logged_caller = true;
            tracing::debug!(entry = self.entry, "native file reader started");
        }
        // 对齐读取器与逻辑位置：往回需要重新打开，往前读掉中间的数据
        if st.reader.is_none() || st.pos < st.reader_pos {
            st.reader = Some(self.shared.contents.open(self.entry)?);
            st.reader_pos = 0;
        }
        let reader = st.reader.as_mut().expect("刚打开");
        let mut skip = st.pos - st.reader_pos;
        let mut scratch = [0u8; 64 * 1024];
        while skip > 0 {
            let want = skip.min(scratch.len() as u64) as usize;
            let n = match reader.read(&mut scratch[..want]) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            skip -= n as u64;
            st.reader_pos += n as u64;
        }
        let mut done = 0;
        while done < buf.len() {
            match reader.read(&mut buf[done..]) {
                Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        st.reader_pos += done as u64;
        st.pos = st.reader_pos;
        if st.pos >= size {
            st.eof = true;
        }
        st.completed |= st.eof;
        Ok(done)
    }
}

// Releasing one stream only releases that reader. Whole-drop transfer and other
// independent cursors survive; EndOperation failure explicitly cancels the batch.

impl ISequentialStream_Impl for FileStream_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        if pv.is_null() {
            return E_POINTER;
        }
        // SAFETY: 调用方保证 pv 指向至少 cb 字节的缓冲区。
        let buf = unsafe { std::slice::from_raw_parts_mut(pv as *mut u8, cb as usize) };
        let Ok(mut st) = self.st.lock() else {
            return STG_E_READFAULT;
        };
        let start = st.pos;
        match self.read_into(&mut st, buf) {
            Ok(n) => {
                if cb != 0 {
                    self.consumed_range(&mut st, start, n);
                }
                tracing::trace!(requested = cb, read = n, offset = st.pos, "native file read");
                if !pcbread.is_null() {
                    // SAFETY: 调用方提供的输出参数。
                    unsafe { *pcbread = n as u32 };
                }
                if n < cb as usize { S_FALSE } else { S_OK }
            }
            Err(e) => {
                tracing::warn!(target: "oink_file_diagnostics", file_stage = 21_u8,
                    file_object = self.shared.publication.observe(None).object, file_index = self.entry,
                    file_code = e.raw_os_error().unwrap_or(0) as u32,
                    file_reason = crate::content::reason(&e) as u8,
                    "native file stream read failed");
                if !pcbread.is_null() {
                    // SAFETY: 同上。
                    unsafe { *pcbread = 0 };
                }
                STG_E_READFAULT
            }
        }
    }

    fn Write(&self, _pv: *const core::ffi::c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for FileStream_Impl {
    fn Seek(&self, dlibmove: i64, dworigin: STREAM_SEEK, plibnewposition: *mut u64) -> windows::core::Result<()> {
        let mut st = self
            .st
            .lock()
            .map_err(|_| windows::core::Error::from(STG_E_INVALIDFUNCTION))?;
        let base = match dworigin {
            STREAM_SEEK_SET => 0i128,
            STREAM_SEEK_CUR => i128::from(st.pos),
            STREAM_SEEK_END => i128::from(
                self.current_size()
                    .map_err(|_| windows::core::Error::from(STG_E_READFAULT))?,
            ),
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let new = base + i128::from(dlibmove);
        let new = u64::try_from(new).map_err(|_| windows::core::Error::from(STG_E_INVALIDFUNCTION))?;
        tracing::trace!("Seek({dlibmove}, {}) {} → {new}", dworigin.0, self.name);
        st.pos = new;
        st.eof = false;
        if !plibnewposition.is_null() {
            // SAFETY: 调用方提供的输出参数。
            unsafe { *plibnewposition = new };
        }
        Ok(())
    }

    fn SetSize(&self, _libnewsize: u64) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn CopyTo(
        &self,
        pstm: Ref<'_, IStream>,
        cb: u64,
        pcbread: *mut u64,
        pcbwritten: *mut u64,
    ) -> windows::core::Result<()> {
        let dst = pstm.ok()?;
        tracing::trace!("CopyTo({cb}) {}", self.name);
        let mut left = cb;
        let (mut read, mut written) = (0u64, 0u64);
        let mut buf = vec![0u8; 256 * 1024];
        let mut result = Ok(());
        while left > 0 {
            let want = left.min(buf.len() as u64) as usize;
            let (start, n) = {
                let mut st = self
                    .st
                    .lock()
                    .map_err(|_| windows::core::Error::from(STG_E_READFAULT))?;
                let start = st.pos;
                match self.read_into(&mut st, &mut buf[..want]) {
                    Ok(n) => (start, n),
                    Err(e) => {
                        tracing::warn!(target: "oink_file_diagnostics", file_stage = 21_u8,
                            file_object = self.shared.publication.observe(None).object, file_index = self.entry,
                            file_code = e.raw_os_error().unwrap_or(0) as u32,
                            file_reason = crate::content::reason(&e) as u8,
                            "native file stream read failed");
                        result = Err(STG_E_READFAULT.into());
                        break;
                    }
                }
            };
            if n == 0 {
                if let Ok(mut st) = self.st.lock() {
                    self.consumed_range(&mut st, start, 0);
                }
                break;
            }
            read += n as u64;
            let mut w = 0u32;
            // SAFETY: 缓冲区前 n 字节有效。
            let hr = unsafe { dst.Write(buf.as_ptr().cast(), n as u32, Some(&mut w)) };
            written += u64::from(w);
            if hr.is_err() {
                result = Err(hr.into());
                break;
            }
            if w != n as u32 {
                result = Err(STG_E_WRITEFAULT.into());
                break;
            }
            if let Ok(mut st) = self.st.lock() {
                self.consumed_range(&mut st, start, n);
            }
            left -= n as u64;
            if n < want {
                break;
            }
        }
        // SAFETY: 调用方提供的输出参数（可为空）。
        unsafe {
            if !pcbread.is_null() {
                *pcbread = read;
            }
            if !pcbwritten.is_null() {
                *pcbwritten = written;
            }
        }
        result
    }

    fn Commit(&self, _grfcommitflags: &STGC) -> windows::core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn LockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: &LOCKTYPE) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn UnlockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: u32) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, grfstatflag: &STATFLAG) -> windows::core::Result<()> {
        if pstatstg.is_null() {
            return Err(E_POINTER.into());
        }
        tracing::trace!("Stat({}) {}", grfstatflag.0, self.name);
        let current = self
            .shared
            .contents
            .metadata(self.entry)
            .map_err(|_| windows::core::Error::from(STG_E_READFAULT))?;
        let size = self
            .metadata_size(current)
            .map_err(|_| windows::core::Error::from(STG_E_READFAULT))?;
        let mtime = current.map_or(self.mtime, |m| {
            m.modified_ms.map_or(0, super::files::unix_ms_to_filetime)
        });
        let ft = FILETIME {
            dwLowDateTime: mtime as u32,
            dwHighDateTime: (mtime >> 32) as u32,
        };
        let mut name = PWSTR::null();
        if *grfstatflag != STATFLAG_NONAME {
            let w: Vec<u16> = self.name.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: 按约定用 CoTaskMemAlloc 分配名字，由调用方释放。
            unsafe {
                let p = CoTaskMemAlloc(w.len() * 2) as *mut u16;
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                    name = PWSTR(p);
                }
            }
        }
        // SAFETY: 调用方提供的输出参数。
        unsafe {
            *pstatstg = STATSTG {
                pwcsName: name,
                r#type: STGTY_STREAM.0 as u32,
                cbSize: size,
                mtime: ft,
                ctime: ft,
                atime: ft,
                grfMode: STGM_READ,
                grfLocksSupported: 0,
                ..Default::default()
            };
        }
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

// ---------- MTA 线程 ----------

/// 可以跨线程传递的 COM 指针（对象本身是线程安全的，或只是封送数据流）。
struct SendPtr<T>(T);
// SAFETY: 只用于把 FileStream（内部加锁）与封送数据流（内存流，线程安全）交给 MTA 线程。
unsafe impl<T> Send for SendPtr<T> {}

type MtaJob = Box<dyn FnOnce() + Send>;

/// 常驻的 MTA 线程：流在这里封送，之后的调用由 MTA 的 RPC 线程执行。
struct Mta {
    tx: Mutex<mpsc::Sender<MtaJob>>,
}

fn mta() -> &'static Mta {
    static M: OnceLock<Mta> = OnceLock::new();
    M.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<MtaJob>();
        let _ = std::thread::Builder::new()
            .name("oink-clipboard-mta".into())
            .spawn(move || {
                // SAFETY: 本线程加入 MTA，进程存活期间不退出（保持 MTA 存在）。
                let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                if hr.is_err() {
                    tracing::warn!("MTA 线程 CoInitializeEx 失败：{hr:?}");
                }
                while let Ok(job) = rx.recv() {
                    job();
                }
            });
        Mta { tx: Mutex::new(tx) }
    })
}

/// 在 MTA 上封送流，在当前线程（STA）得到指向 MTA 的代理。
fn mta_stream(stream: FileStream) -> windows::core::Result<IStream> {
    let obj: IStream = stream.into();
    let obj = SendPtr(obj);
    let (tx, rx) = mpsc::sync_channel(1);
    let job: MtaJob = Box::new(move || {
        let obj = obj;
        // SAFETY: 在 MTA 线程上封送本进程的对象。
        let r = unsafe { CoMarshalInterThreadInterfaceInStream(&IStream::IID, &obj.0) };
        let _ = tx.send(r.map(SendPtr));
    });
    mta()
        .tx
        .lock()
        .map_err(|_| windows::core::Error::from(STG_E_READFAULT))?
        .send(job)
        .map_err(|_| windows::core::Error::from(STG_E_READFAULT))?;
    let packet = rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| windows::core::Error::from(STG_E_READFAULT))??;
    // SAFETY: 封送数据流来自 CoMarshalInterThreadInterfaceInStream，只解封一次。
    unsafe { CoGetInterfaceAndReleaseStream::<_, IStream>(&packet.0) }
}

#[cfg(all(test, debug_assertions))]
pub(super) mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn shell_completion_keeps_pending_data_and_respects_late_failure_and_native_cancel() {
        std::thread::spawn(|| {
            use windows::Win32::{
                Foundation::{DRAGDROP_S_CANCEL, E_ABORT},
                System::{
                    DataExchange::RegisterClipboardFormatW,
                    Ole::{DROPEFFECT_NONE, OleInitialize, OleUninitialize},
                },
            };
            // SAFETY: private apartment and IDataObjects, no system clipboard/desktop.
            unsafe { OleInitialize(None).unwrap() };
            struct Apartment;
            impl Drop for Apartment {
                fn drop(&mut self) {
                    // SAFETY: balances successful initialization on this thread.
                    unsafe { OleUninitialize() };
                }
            }
            let _apartment = Apartment;
            let make = || {
                let memory = Arc::new(Memory {
                    bytes: b"fixture".to_vec(),
                    opens: AtomicUsize::new(0),
                    cancels: AtomicUsize::new(0),
                    dynamic: AtomicBool::new(false),
                });
                let f = super::super::formats();
                let (data, shared) = build(
                    Spec {
                        cloud: None,
                        descriptors: super::super::files::descriptors(&[]),
                        fixed: vec![(13, vec![0, 0])],
                        contents: memory.clone(),
                        cf_descriptor: f.file_descriptor,
                        cf_contents: f.file_contents,
                    },
                    true,
                );
                shared.host_drag_images();
                (data, shared, memory)
            };
            let (data, shared, memory) = make();
            let asynchronous: IDataObjectAsyncCapability = data.cast().unwrap();
            // SAFETY: apartment-owned COM objects and ordinary scalar arguments.
            unsafe { asynchronous.StartOperation(None).unwrap() };
            let accepted = shared.drag_dropped(DRAGDROP_S_DROP, DROPEFFECT_NONE);
            assert!(accepted, "Explorer returns before extraction finishes");
            shared.drag_ended(accepted, false);
            assert_eq!(memory.cancels.load(Ordering::SeqCst), 0);
            assert!(!shared.flushing.load(Ordering::SeqCst));
            // SAFETY: same private object, explicit consumer failure after the loop.
            unsafe { asynchronous.EndOperation(E_ABORT, None, 0).unwrap() };
            assert_eq!(memory.cancels.load(Ordering::SeqCst), 1);
            assert!(shared.flushing.load(Ordering::SeqCst));
            assert!(!shared.drag_dropped(DRAGDROP_S_DROP, DROPEFFECT_NONE));

            let (data, shared, memory) = make();
            let asynchronous: IDataObjectAsyncCapability = data.cast().unwrap();
            // Completion can race ahead of the native loop return.
            // SAFETY: same apartment-owned object with matching operation lifetime.
            unsafe {
                asynchronous.StartOperation(None).unwrap();
                asynchronous.EndOperation(S_OK, None, 2).unwrap();
            }
            assert!(shared.drag_dropped(DRAGDROP_S_DROP, DROPEFFECT_NONE));
            assert_eq!(memory.cancels.load(Ordering::SeqCst), 0);

            let (data, shared, memory) = make();
            let asynchronous: IDataObjectAsyncCapability = data.cast().unwrap();
            // SAFETY: private matching lifetime; optimized MOVE reports NONE.
            unsafe {
                asynchronous.StartOperation(None).unwrap();
                asynchronous.EndOperation(S_OK, None, 0).unwrap();
            }
            let accepted = shared.drag_dropped(DRAGDROP_S_DROP, DROPEFFECT_NONE);
            assert!(accepted, "completed optimized MOVE still succeeded");
            shared.drag_ended(accepted, false);
            assert_eq!(memory.cancels.load(Ordering::SeqCst), 0);
            assert!(!shared.flushing.load(Ordering::SeqCst));

            let (data, shared, memory) = make();
            for (name, effect) in [
                (windows::core::w!("Logical Performed DropEffect"), 2u32),
                (windows::core::w!("Performed DropEffect"), 0),
            ] {
                // SAFETY: registered constant name, valid FORMATETC and owned medium.
                unsafe {
                    let format = FORMATETC {
                        cfFormat: RegisterClipboardFormatW(name) as u16,
                        dwAspect: 1,
                        lindex: -1,
                        tymed: 1,
                        ..Default::default()
                    };
                    let mut medium = medium_hglobal(&effect.to_le_bytes()).unwrap();
                    data.SetData(&format, &medium, false).unwrap();
                    ReleaseStgMedium(&mut medium);
                }
            }
            assert!(
                shared.drag_dropped(DRAGDROP_S_DROP, DROPEFFECT_NONE),
                "optimized MOVE receipt"
            );
            assert!(
                !shared.drag_dropped(DRAGDROP_S_CANCEL, DROPEFFECT_NONE),
                "a native cancellation is authoritative"
            );
            shared.drag_ended(false, false);
            assert_eq!(memory.cancels.load(Ordering::SeqCst), 1);
            assert_eq!(
                memory.opens.load(Ordering::SeqCst),
                0,
                "completion and diagnostics never fetch bodies"
            );
            let (_, rejected, _) = make();
            assert!(
                !rejected.drag_dropped(DRAGDROP_S_DROP, DROPEFFECT_NONE),
                "no target acceptance evidence"
            );
        })
        .join()
        .unwrap();
    }

    #[test]
    fn native_get_data_timeout_fails_then_retries_same_object() {
        use crate::content::native_read::{NativeReadOutcome, TEST_OUTCOMES, TestClock, tests::Harness};
        std::thread::spawn(|| {
            let mut harness = Harness::new(oink_foundation::clipboard::ClipPlatform::Windows, true);
            let clock = Arc::new(TestClock::default());
            let expiry = harness.expire_when_waiting(clock.clone());
            // SAFETY: private STA/object; never reads or changes the user's clipboard.
            unsafe {
                windows::Win32::System::Ole::OleInitialize(None).unwrap();
            }
            let native = super::super::content::target::build(harness.spec()).unwrap();
            let formats = super::super::formats();
            let (data, shared) = build_inner(
                Spec {
                    cloud: None,
                    descriptors: super::super::files::descriptors(&[]),
                    fixed: vec![],
                    contents: Arc::new(Memory {
                        bytes: vec![],
                        opens: AtomicUsize::new(0),
                        cancels: AtomicUsize::new(0),
                        dynamic: AtomicBool::new(false),
                    }),
                    cf_descriptor: formats.file_descriptor,
                    cf_contents: formats.file_contents,
                },
                false,
                Some(native),
            );
            let text = FORMATETC {
                cfFormat: 13,
                dwAspect: 1,
                tymed: 1,
                lindex: -1,
                ..Default::default()
            };
            // SAFETY: initialized COM apartment and valid owned structures/media.
            unsafe {
                assert_eq!(
                    clock.scope(|| data.GetData(&text)).err().unwrap().code(),
                    STG_E_READFAULT
                );
                expiry.join().unwrap();
                TEST_OUTCOMES
                    .with_borrow(|outcomes| assert_eq!(outcomes.last().unwrap().1, NativeReadOutcome::TimedOut));
                harness.allow_retry();
                let mut medium = data.GetData(&text).unwrap();
                assert_eq!(hglobal_bytes(medium.u.hGlobal).unwrap(), harness.bytes);
                ReleaseStgMedium(&mut medium);
                TEST_OUTCOMES.with_borrow(|outcomes| assert_eq!(outcomes.last().unwrap().1, NativeReadOutcome::Served));
                drop((data, shared));
                windows::Win32::System::Ole::OleUninitialize();
            }
        })
        .join()
        .unwrap();
    }

    #[test]
    fn diagnosed_text_data_object_keeps_lazy_reads_and_rejects_file_probes() {
        std::thread::spawn(|| {
            use crate::{
                content::{Provider, Translator},
                model::*,
            };
            struct Source {
                inventory: Inventory,
                opens: Arc<AtomicUsize>,
            }
            impl Provider for Source {
                fn inventory(&self) -> Inventory {
                    self.inventory.clone()
                }
                fn translator(&self) -> Arc<dyn Translator> {
                    unreachable!()
                }
                fn open(&self, _: RepresentationId) -> std::io::Result<crate::FileReader> {
                    self.opens.fetch_add(1, Ordering::SeqCst);
                    Ok(Box::new(std::io::Cursor::new(b"tiny text".to_vec())))
                }
            }
            // SAFETY: private STA/data object; never reads or writes the desktop clipboard.
            unsafe {
                windows::Win32::System::Ole::OleInitialize(None).unwrap();
            }
            let opens = Arc::new(AtomicUsize::new(0));
            let mut inventory = crate::content::tests::fixture().inventory();
            inventory.platform = oink_foundation::clipboard::ClipPlatform::Mac;
            inventory.roots.clear();
            inventory.items.truncate(1);
            inventory.items[0].file = None;
            inventory.items[0].representations = vec![Representation {
                id: RepresentationId(4),
                format: NativeFormat::Mac("public.utf8-plain-text".into()),
                standard: Some(StandardFormat::Text),
                medium: Medium::Bytes,
                aspect: 1,
                index: -1,
                data: RepresentationData::Reader {
                    size: ContentSize::Unknown,
                },
            }];
            let native = super::super::content::target::build(crate::NativeSpec {
                inventory: inventory.clone(),
                contents: Arc::new(Source {
                    inventory,
                    opens: opens.clone(),
                }),
                files: vec![],
                file_contents: None,
                sensitivity: crate::Sensitivity::Normal,
            })
            .unwrap();
            let memory = Arc::new(Memory {
                bytes: vec![],
                opens: AtomicUsize::new(0),
                cancels: AtomicUsize::new(0),
                dynamic: AtomicBool::new(false),
            });
            let formats = super::super::formats();
            let (data, shared) = build_inner(
                Spec {
                    cloud: None,
                    descriptors: super::super::files::descriptors(&[]),
                    fixed: vec![],
                    contents: memory,
                    cf_descriptor: formats.file_descriptor,
                    cf_contents: formats.file_contents,
                },
                false,
                Some(native),
            );
            let text = FORMATETC {
                cfFormat: 13,
                dwAspect: 1,
                tymed: 1,
                lindex: -1,
                ..Default::default()
            };
            // SAFETY: initialized COM apartment and valid owned structures/media.
            unsafe {
                data.QueryGetData(&text).ok().unwrap();
                drop(data.EnumFormatEtc(DATADIR_GET.0 as u32).unwrap());
                assert_eq!(opens.load(Ordering::SeqCst), 0, "diagnostics must not prefetch");
                let file = FORMATETC { cfFormat: 15, ..text };
                assert_eq!(data.QueryGetData(&file), DV_E_FORMATETC);
                assert_eq!(data.GetData(&file).err().unwrap().code(), DV_E_FORMATETC);
                let mut here = STGMEDIUM::default();
                assert_eq!(data.GetDataHere(&text, &mut here).unwrap_err().code(), E_NOTIMPL);
                let mut medium = data.GetData(&text).unwrap();
                assert_eq!(
                    hglobal_bytes(medium.u.hGlobal).unwrap(),
                    b"t\0i\0n\0y\0 \0t\0e\0x\0t\0\0\0"
                );
                ReleaseStgMedium(&mut medium);
                assert_eq!(
                    opens.load(Ordering::SeqCst),
                    1,
                    "GetData retains the original lazy read"
                );
                drop((data, shared));
                windows::Win32::System::Ole::OleUninitialize();
            }
        })
        .join()
        .unwrap();
    }

    struct Memory {
        bytes: Vec<u8>,
        opens: AtomicUsize,
        cancels: AtomicUsize,
        dynamic: AtomicBool,
    }
    impl FileContents for Memory {
        fn open(&self, _: usize) -> std::io::Result<crate::FileReader> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(std::io::Cursor::new(self.bytes.clone())))
        }
        fn metadata(&self, _: usize) -> std::io::Result<Option<crate::FileMetadata>> {
            Ok(self.dynamic.load(Ordering::SeqCst).then_some(crate::FileMetadata {
                exec_bits: None,
                size: self.bytes.len() as u64,
                modified_ms: Some(1234),
            }))
        }
        fn cancel(&self) {
            self.cancels.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn stream(size: u64, bytes: &[u8]) -> (FileStream, Arc<Memory>) {
        let memory = Arc::new(Memory {
            bytes: bytes.to_vec(),
            opens: AtomicUsize::new(0),
            cancels: AtomicUsize::new(0),
            dynamic: AtomicBool::new(false),
        });
        (
            FileStream {
                _lease: None,
                shared: Arc::new(Shared {
                    publication: crate::windows::content::diagnostics::Publication::new(),
                    contents: memory.clone(),
                    flushing: AtomicBool::new(false),
                    cloud_paths: false,
                    cloud: None,
                    hosted_drag: AtomicBool::new(false),
                    async_running: AtomicBool::new(false),
                    drag_diagnostics: OnceLock::new(),
                    drag_completion: Mutex::new(DragCompletion::default()),
                }),
                entry: 0,
                name: "test".into(),
                size,
                declared_size: None,
                mtime: 0,
                st: Mutex::new(StreamState::default()),
            },
            memory,
        )
    }
    pub(crate) fn read_native_contents(contents: Arc<dyn FileContents>, size: u64) -> std::io::Result<Vec<u8>> {
        let (mut file, _) = stream(size, &[]);
        Arc::get_mut(&mut file.shared).unwrap().contents = contents;
        let stream: IStream = file.into();
        let mut output = Vec::new();
        loop {
            let mut buffer = [0u8; 64 * 1024];
            let mut count = 0;
            // SAFETY: live writable buffer/count, direct local COM object.
            let result = unsafe { stream.Read(buffer.as_mut_ptr().cast(), buffer.len() as u32, Some(&mut count)) };
            result.ok().map_err(std::io::Error::other)?;
            output.extend_from_slice(&buffer[..count as usize]);
            if result == S_FALSE {
                return Ok(output);
            }
        }
    }
    struct SizedContents {
        opens: AtomicUsize,
    }
    impl FileContents for SizedContents {
        fn descriptor_size_known(&self, index: usize) -> bool {
            index != 3
        }
        fn metadata(&self, index: usize) -> std::io::Result<Option<crate::FileMetadata>> {
            Ok(Some(crate::FileMetadata {
                exec_bits: None,
                size: [7, 0, 5_000_000_000, 19, 0][index],
                modified_ms: None,
            }))
        }
        fn open(&self, _: usize) -> std::io::Result<crate::FileReader> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            Err(std::io::Error::other("metadata must not open content"))
        }
    }
    #[test]
    fn all_lindices_have_descriptor_and_stat_sizes_without_opening_content() {
        std::thread::spawn(|| {
            use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
            // SAFETY: dedicated apartment; release media and interfaces before uninitializing.
            unsafe { OleInitialize(None).unwrap() };
            let entries: Vec<_> = [
                ("first", false, 7),
                ("directory", true, 0),
                ("second", false, 5_000_000_000),
                ("unknown", false, 0),
                ("empty", false, 0),
            ]
            .into_iter()
            .map(|(path, is_dir, size)| crate::FileEntry {
                path: path.into(),
                is_dir,
                size,
                modified_ms: None,
                local_path: None,
            })
            .collect();
            let memory = Arc::new(SizedContents {
                opens: AtomicUsize::new(0),
            });
            let formats = super::super::formats();
            let (data, shared) = build(
                Spec {
                    cloud: None,
                    descriptors: super::super::files::descriptors(&entries),
                    contents: memory.clone(),
                    fixed: Vec::new(),
                    cf_descriptor: formats.file_descriptor,
                    cf_contents: formats.file_contents,
                },
                true,
            );
            let mut format = FORMATETC {
                cfFormat: formats.file_descriptor as u16,
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
                ..Default::default()
            };
            // SAFETY: valid native object, tagged media, and initialized output structures.
            unsafe {
                let mut medium = data.GetData(&format).unwrap();
                let bytes = hglobal_bytes(medium.u.hGlobal).unwrap();
                ReleaseStgMedium(&mut medium);
                for (i, entry) in entries.iter().enumerate() {
                    let offset = 4 + i * super::super::files::FILEDESCRIPTOR_SIZE;
                    let flags = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
                    assert_ne!(flags & 0x4000, 0, "progress flag at {i}");
                    assert_eq!(flags & 0x40 != 0, !entry.is_dir && i != 3, "size flag at {i}");
                }
                format.cfFormat = formats.file_contents as u16;
                format.tymed = TYMED_ISTREAM.0 as u32;
                for (index, size) in [(2, 5_000_000_000), (0, 7), (3, 19), (4, 0), (2, 5_000_000_000)] {
                    format.lindex = index;
                    let mut medium = data.GetData(&format).unwrap();
                    let stream = medium.u.pstm.as_ref().unwrap();
                    let mut stat = STATSTG::default();
                    stream.Stat(&mut stat, STATFLAG_NONAME).unwrap();
                    assert_eq!(stat.cbSize, size, "Stat for lindex {index}");
                    ReleaseStgMedium(&mut medium);
                }
                assert_eq!(memory.opens.load(Ordering::SeqCst), 0);
                drop((data, shared));
                OleUninitialize();
            }
        })
        .join()
        .unwrap();
    }

    #[test]
    fn advertised_size_change_fails_before_returning_truncated_content() {
        std::thread::spawn(|| {
            use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
            // SAFETY: isolated STA with all native media released before exit.
            unsafe { OleInitialize(None).unwrap() };
            for (advertised, bytes) in [(1, &b"grown"[..]), (99, &b"small"[..]), (0, &b"nonempty"[..])] {
                let (_, memory) = stream(advertised, bytes);
                memory.dynamic.store(true, Ordering::SeqCst);
                let formats = super::super::formats();
                let (data, shared) = build(
                    Spec {
                        cloud: None,
                        descriptors: super::super::files::descriptors(&[crate::FileEntry {
                            path: "changed".into(),
                            is_dir: false,
                            size: advertised,
                            modified_ms: None,
                            local_path: None,
                        }]),
                        contents: memory.clone(),
                        fixed: Vec::new(),
                        cf_descriptor: formats.file_descriptor,
                        cf_contents: formats.file_contents,
                    },
                    true,
                );
                let format = FORMATETC {
                    cfFormat: formats.file_contents as u16,
                    dwAspect: DVASPECT_CONTENT.0,
                    lindex: 0,
                    tymed: TYMED_ISTREAM.0 as u32,
                    ..Default::default()
                };
                // SAFETY: valid native interfaces, initialized buffers, owned medium.
                unsafe {
                    let mut medium = data.GetData(&format).unwrap();
                    let stream = medium.u.pstm.as_ref().unwrap();
                    let mut stat = STATSTG::default();
                    assert!(stream.Stat(&mut stat, STATFLAG_NONAME).is_err());
                    let mut output = [0u8; 32];
                    let mut read = 100;
                    assert_eq!(
                        stream.Read(output.as_mut_ptr().cast(), 32, Some(&mut read)),
                        STG_E_READFAULT
                    );
                    assert_eq!(read, 0);
                    ReleaseStgMedium(&mut medium);
                    drop((data, shared));
                }
                assert_eq!(memory.opens.load(Ordering::SeqCst), 0);
            }
            // SAFETY: balances initialization after all native objects have dropped.
            unsafe { OleUninitialize() };
        })
        .join()
        .unwrap();
    }

    #[test]
    fn current_metadata_replaces_manifest_size_for_stream_reads() {
        for (manifest, bytes) in [
            (1, &b"grown"[..]),
            (99, &b"small"[..]),
            (0, &b"nonempty"[..]),
            (7, &b""[..]),
        ] {
            let (s, memory) = stream(manifest, bytes);
            memory.dynamic.store(true, Ordering::SeqCst);
            assert_eq!(s.current_size().unwrap(), bytes.len() as u64);
            let mut got = [0; 32];
            let n = s.read_into(&mut s.st.lock().unwrap(), &mut got).unwrap();
            assert_eq!(&got[..n], bytes);
            assert_eq!(s.read_into(&mut s.st.lock().unwrap(), &mut got).unwrap(), 0);
            drop(s);
            assert_eq!(memory.cancels.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn full_read_then_rewind_does_not_cancel() {
        let (s, memory) = stream(4, b"abcd");
        {
            let mut st = s.st.lock().unwrap();
            assert_eq!(s.read_into(&mut st, &mut [0; 8]).unwrap(), 4);
            st.pos = 0;
            st.eof = false; // Explorer 在读取完整个流后 Seek(0)，然后释放。
        }
        drop(s);
        assert_eq!(memory.cancels.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn partial_read_then_release_only_releases_that_reader() {
        let (s, memory) = stream(4, b"abcd");
        s.read_into(&mut s.st.lock().unwrap(), &mut [0; 2]).unwrap();
        drop(s);
        assert_eq!(memory.cancels.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn truncated_source_is_an_error_not_successful_paste() {
        let (s, memory) = stream(10, b"abc");
        assert_eq!(
            s.read_into(&mut s.st.lock().unwrap(), &mut [0; 10]).unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
        drop(s);
        assert_eq!(memory.cancels.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn empty_read_and_seek_past_end_do_not_start_download() {
        let (s, memory) = stream(4, b"abcd");
        let mut st = s.st.lock().unwrap();
        assert_eq!(s.read_into(&mut st, &mut []).unwrap(), 0);
        st.pos = 100;
        assert_eq!(s.read_into(&mut st, &mut [0; 4]).unwrap(), 0);
        assert!(!st.completed);
        assert_eq!(memory.opens.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn shell_drag_image_helper_can_store_and_retrieve_its_stream_context() {
        std::thread::spawn(|| {
            use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
            // SAFETY: a fresh STA used only for this COM data-object regression.
            unsafe {
                OleInitialize(None).unwrap();
            }
            {
                let (_, memory) = stream(0, b"");
                let (data, _) = build(
                    Spec {
                        cloud: None,
                        descriptors: super::super::files::descriptors(&[]),
                        fixed: vec![],
                        contents: memory,
                        cf_descriptor: 0,
                        cf_contents: 0,
                    },
                    false,
                );
                let helper = super::super::drag_image::initialize(
                    &data,
                    &[oink_foundation::clipboard::ClipData::Text("preview".into())],
                    None,
                )
                .unwrap();
                assert!(helper.is_some());
                assert!(super::super::drag_image::capture(&data).is_some());
                // A ready source thumbnail replaces metadata/type artwork before
                // DoDragDrop, including image files. Nothing is published to the
                // system clipboard and no native drag or input is started.
                let source = oink_foundation::dnd::DragImage {
                    png: crate::dnd::preview::asymmetric_png().into(),
                    width: 3.,
                    height: 2.,
                    hotspot_x: 1.,
                    hotspot_y: 0.5,
                };
                let helper = super::super::drag_image::initialize(
                    &data,
                    &[oink_foundation::clipboard::ClipData::Text("type artwork".into())],
                    Some(&source),
                )
                .unwrap();
                assert!(helper.is_some());
                let captured = super::super::drag_image::capture(&data).unwrap();
                let pixels = super::super::image::decode_png(&captured.png).unwrap();
                assert_eq!(&pixels.data[..3], &[255, 0, 0]);
                let bottom = ((pixels.height - 1) * pixels.width * 4) as usize;
                assert_eq!(&pixels.data[bottom..bottom + 3], &[0, 0, 255]);
            }
            // SAFETY: balances successful initialization after releasing COM objects.
            unsafe {
                OleUninitialize();
            }
        })
        .join()
        .unwrap();
    }
}

#[cfg(all(test, debug_assertions))]
mod directory_tests {
    use super::*;
    use crate::content::namespace::{Files, Local, Namespace};
    use crate::model::{DirectoryCursor, DirectoryPage, FileNode, NodeId};
    use std::{io, sync::atomic::AtomicUsize};
    struct Count {
        inner: Arc<dyn Namespace>,
        lists: Arc<AtomicUsize>,
    }
    impl Namespace for Count {
        fn roots(&self) -> Vec<FileNode> {
            self.inner.roots()
        }
        fn list(&self, node: NodeId, cursor: Option<DirectoryCursor>) -> io::Result<DirectoryPage> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            self.inner.list(node, cursor)
        }
        fn select(&self, node: NodeId) -> io::Result<FileNode> {
            self.inner.select(node)
        }
        fn open(&self, node: NodeId, version: u64, offset: u64) -> io::Result<crate::FileReader> {
            self.inner.open(node, version, offset)
        }
    }
    #[test]
    fn native_format_queries_do_not_expand_and_first_descriptor_expands_exactly_once() {
        std::thread::spawn(|| {
            use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
            // SAFETY: isolated test STA with all COM objects released before exit.
            unsafe {
                OleInitialize(None).unwrap();
            }
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir(root.path().join("folder")).unwrap();
            std::fs::write(root.path().join("folder/child"), b"data").unwrap();
            let lists = Arc::new(AtomicUsize::new(0));
            let namespace = Arc::new(Count {
                inner: Local::new(vec![(NodeId(1), root.path().join("folder"))]).unwrap(),
                lists: lists.clone(),
            });
            let contents = Files::new(namespace, crate::sync::files::TargetOs::Windows).unwrap();
            let formats = super::super::formats();
            let (data, shared) = build(
                Spec {
                    cloud: None,
                    descriptors: super::super::files::descriptors(contents.roots()),
                    fixed: Vec::new(),
                    contents,
                    cf_descriptor: formats.file_descriptor,
                    cf_contents: formats.file_contents,
                },
                true,
            );
            let format = FORMATETC {
                cfFormat: formats.file_descriptor as u16,
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
                ..Default::default()
            };
            // SAFETY: initialized STA, valid FORMATETC and owned returned media.
            unsafe {
                data.QueryGetData(&format).ok().unwrap();
                let enumeration = data.EnumFormatEtc(DATADIR_GET.0 as u32).unwrap();
                drop(enumeration);
                assert_eq!(lists.load(Ordering::SeqCst), 0);
                for _ in 0..2 {
                    let mut medium = data.GetData(&format).unwrap();
                    let bytes = hglobal_bytes(medium.u.hGlobal).unwrap();
                    assert_eq!(u32::from_le_bytes(bytes[..4].try_into().unwrap()), 2);
                    ReleaseStgMedium(&mut medium);
                }
                assert_eq!(lists.load(Ordering::SeqCst), 1);
                drop((data, shared));
                OleUninitialize();
            }
        })
        .join()
        .unwrap();
    }
}
