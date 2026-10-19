//! In-memory clipboard backend for tests.

use oink_clipboard::{
    ChangeHandler, ClipData, ClipSnapshot, ClipboardBackend, ClipboardError, FileContents, FileEntry, Sensitivity,
};
use std::sync::{Arc, Mutex};
mod file_gate;
mod native;
pub use file_gate::FileReadGate;

#[derive(Default)]
struct Inner {
    items: Vec<ClipData>,
    sensitivity: Option<Sensitivity>,
    count: u64,
    handlers: Vec<ChangeHandler>,
    /// 写入时对内容做的改写（模拟平台转换不完全可逆，例如 HTML 被加上 meta 标签）。
    rewrite: Option<fn(ClipData) -> ClipData>,
    /// 支持虚拟文件（模拟 Windows）。
    virtual_files: bool,
    deferred_native: bool,
    unknown_representation_sizes: bool,
    file_gate: Option<Arc<file_gate::State>>,
    native: Option<oink_clipboard::NativeSpec>,
    /// 最近一次写入的虚拟文件的内容来源。
    contents: Option<Arc<dyn FileContents>>,
}

/// 内存里的剪贴板。`copy` 模拟用户复制；`write` 由同步逻辑调用。
#[derive(Default)]
pub struct FakeClipboard {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for FakeClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let i = self.lock();
        write!(f, "FakeClipboard(count={}, items={:?})", i.count, i.items)
    }
}

impl FakeClipboard {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set(&self, items: Vec<ClipData>, s: Sensitivity) -> u64 {
        let (count, handlers) = {
            let mut i = self.lock();
            i.contents = None;
            i.native = None;
            let items = match i.rewrite {
                Some(f) => items.into_iter().map(f).collect(),
                None => items,
            };
            i.items = items;
            i.sensitivity = Some(s);
            i.count += 1;
            (i.count, i.handlers.clone())
        };
        for h in handlers {
            h();
        }
        count
    }

    /// 模拟用户在本机复制。返回新的变化计数。
    pub fn copy(&self, items: Vec<ClipData>, s: Sensitivity) -> u64 {
        self.set(items, s)
    }

    /// 当前内容。
    pub fn items(&self) -> Vec<ClipData> {
        self.lock().items.clone()
    }

    pub fn sensitivity(&self) -> Option<Sensitivity> {
        self.lock().sensitivity
    }

    /// 模拟支持虚拟文件的平台（Windows）。
    pub fn set_virtual_files(&self, on: bool) {
        self.lock().virtual_files = on;
    }

    /// 最近一次写入的虚拟文件的内容来源（之后又有写入或复制时为 `None`）。
    pub fn gate_file_reads_after(&self, bytes: u64) -> (FileReadGate, std::sync::mpsc::Receiver<u64>) {
        let (gate, entered) = file_gate::new(bytes);
        self.lock().file_gate = Some(gate.state.clone());
        (gate, entered)
    }
    /// Model an OS data object which publishes native metadata before lazy reads.
    pub fn set_deferred_native(&self, deferred: bool) {
        self.lock().deferred_native = deferred;
    }
    /// Model native readers whose sizes are learned only on demand.
    pub fn set_unknown_representation_sizes(&self, unknown: bool) {
        self.lock().unknown_representation_sizes = unknown;
    }
    pub fn native_spec(&self) -> Option<oink_clipboard::NativeSpec> {
        self.lock().native.clone()
    }

    pub fn contents(&self) -> Option<Arc<dyn FileContents>> {
        self.lock().contents.clone()
    }

    /// 让之后的写入对内容做改写（测试防回环不依赖内容完全一致）。
    pub fn set_rewrite(&self, f: Option<fn(ClipData) -> ClipData>) {
        self.lock().rewrite = f;
    }
}

impl ClipboardBackend for FakeClipboard {
    fn change_count(&self) -> u64 {
        self.lock().count
    }

    fn read(&self) -> Result<ClipSnapshot, ClipboardError> {
        let i = self.lock();
        Ok(ClipSnapshot {
            items: i.items.clone(),
            sensitivity: i.sensitivity.unwrap_or(Sensitivity::Normal),
            change_count: i.count,
        })
    }

    fn capture(&self, data: &std::path::Path) -> Result<oink_clipboard::CapturedClipboard, ClipboardError> {
        native::capture(self, data)
    }
    fn write_native(&self, spec: oink_clipboard::NativeSpec) -> Result<u64, ClipboardError> {
        native::write(self, spec)
    }

    fn write_native_guarded(
        &self,
        spec: oink_clipboard::NativeSpec,
        guard: oink_clipboard::content::ReaderGuard,
    ) -> Result<u64, ClipboardError> {
        native::write_guarded(self, spec, guard)
    }

    fn write(&self, items: &[ClipData], sensitivity: Sensitivity) -> Result<u64, ClipboardError> {
        Ok(self.set(items.to_vec(), sensitivity))
    }

    fn watch(&self, on_change: ChangeHandler) -> Result<(), ClipboardError> {
        self.lock().handlers.push(on_change);
        Ok(())
    }

    fn write_virtual_files(
        &self,
        entries: &[FileEntry],
        others: &[ClipData],
        contents: Arc<dyn FileContents>,
        sensitivity: Sensitivity,
    ) -> Result<u64, ClipboardError> {
        if !self.lock().virtual_files {
            return Err(ClipboardError::Unsupported);
        }
        let mut items = vec![ClipData::Files(entries.to_vec())];
        items.extend_from_slice(others);
        let n = self.set(items, sensitivity);
        self.lock().contents = Some(contents);
        Ok(n)
    }
}
