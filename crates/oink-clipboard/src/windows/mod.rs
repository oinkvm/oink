//! Windows 实现：OpenClipboard / GetClipboardData / SetClipboardData、AddClipboardFormatListener。
//!
//! 支持的格式：
//! - 文本 `CF_UNICODETEXT`（线上统一 LF，读取时 CRLF → LF，写入时 LF → CRLF）；
//! - `HTML Format`（CF_HTML，头部与字节偏移由 [`html`] 转换，线上是片段）；
//! - `Rich Text Format`；
//! - 图片：读取优先注册格式 `PNG`，没有时从 `CF_DIBV5` / `CF_DIB` 转 PNG；写入时同时提供 `PNG`、
//!   `CF_DIBV5`（带 alpha）与 `CF_DIB`（24 位，透明部分混合白色），见 [`image`]。
//!
//! 隐私标记：
//! - 读取：有 `Clipboard Viewer Ignore` → `Transient`；有 `ExcludeClipboardContentFromMonitorProcessing`
//!   或 `CanIncludeInClipboardHistory` = 0 → `Concealed`。`CanUploadToCloudClipboard` = 0 单独出现时仍为
//!   `Normal`（它只表示不上传微软云剪贴板）。
//! - 写入：同系统自定义格式及隐私标记原始字节透传；跨系统已知标记按 `formats` 映射。
//!   不额外添加云/历史标记，所有隐私级别均同步。
//!
//! 文件（`ClipData::Files`，M4，见 [`files`]、[`vfile`]）：
//! - 读取：`CF_HDROP` → 文件夹展开成清单（在调用方线程上遍历，不占用剪贴板线程）；剪贴板上没有文本时，
//!   另加一份顶层文件名文本（一行一个）。其他表示形式照常读取。
//! - 写入：带 `local_path` 的 `ClipData::Files` 写成 `CF_HDROP`（只写顶层条目）+ `Preferred DropEffect` = 复制；
//! - 虚拟文件：[`ClipboardBackend::write_virtual_files`]，`OleSetClipboard` + 自定义 `IDataObject`。

mod clipboard_key;
pub(crate) mod cloud;
mod content;
pub(crate) mod dnd;
mod drag_image;
mod files;
pub(crate) mod html;
mod image;
#[cfg(all(test, debug_assertions))]
mod settle_tests;
mod vfile;
mod worker;

use crate::formats as opaque;
use oink_foundation::clipboard::ClipPlatform;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use oink_foundation::clipboard::{ClipData, ClipSnapshot, FileEntry, Sensitivity};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::Globalization::{CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardFormatNameW,
    GetClipboardSequenceNumber, GetOpenClipboardWindow, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
use windows::core::PCWSTR;

use crate::{ChangeHandler, ClipboardBackend, ClipboardError, FileContents};

pub(crate) fn backend() -> Result<Box<dyn ClipboardBackend>, ClipboardError> {
    Ok(Box::new(WinClipboard {
        worker: Arc::new(worker::Worker::spawn()?),
        reader: Arc::new(worker::Worker::spawn()?),
        observer: Arc::new(content::current::Observer::default()),
        cloud: std::sync::Mutex::new(None),
    }))
}

#[derive(Debug)]
struct WinClipboard {
    cloud: std::sync::Mutex<Option<cloud::Owner>>,
    worker: Arc<worker::Worker>,
    reader: Arc<worker::Worker>,
    observer: Arc<content::current::Observer>,
}

impl ClipboardBackend for WinClipboard {
    fn clipboard_key(&self) -> std::io::Result<[u8; 32]> {
        clipboard_key::current()
    }
    fn initialize_store(&self, cache: PathBuf) {
        *self.cloud.lock().unwrap() = cloud::initialize(cache);
    }
    fn change_count(&self) -> u64 {
        self.observer.logical(self.worker.change_count())
    }

    fn read(&self) -> Result<ClipSnapshot, ClipboardError> {
        let (mut snap, drops) = self.worker.run(read_on_worker)??;
        if !drops.is_empty() {
            // 遍历文件夹可能很慢：在调用方线程上做，剪贴板早已关闭
            let (entries, stats) = files::expand(&drops);
            tracing::debug!(
                "CF_HDROP：{} 个顶层条目展开为 {} 个文件、{} 个文件夹，共 {} 字节，读不到 {} 项，用时 {:?}",
                drops.len(),
                stats.files,
                stats.dirs,
                stats.bytes,
                stats.unreadable,
                stats.elapsed
            );
            if !entries.is_empty() {
                if !snap.items.iter().any(|i| matches!(i, ClipData::Text(_))) {
                    snap.items.push(ClipData::Text(files::names_text(&entries)));
                }
                snap.items.insert(0, ClipData::Files(entries));
            }
        }
        Ok(snap)
    }

    fn capture(&self, data: &std::path::Path) -> Result<crate::CapturedClipboard, ClipboardError> {
        let owner = self.worker.clone();
        let observer = self.observer.clone();
        let root = data.to_path_buf();
        self.reader.run(move |_| content::capture(owner, observer, root))?
    }

    fn write_native(&self, spec: crate::NativeSpec) -> Result<u64, ClipboardError> {
        self.write_native_guarded(spec, Arc::new(|| Ok(())))
    }
    fn write_native_guarded(
        &self,
        spec: crate::NativeSpec,
        guard: crate::content::ReaderGuard,
    ) -> Result<u64, ClipboardError> {
        self.worker
            .run(move |_| content::target::set_clipboard_guarded(spec, guard))?
    }

    fn write(&self, items: &[ClipData], sensitivity: Sensitivity) -> Result<u64, ClipboardError> {
        // 格式转换（PNG 解码、DIB 编码）在打开剪贴板之前做完，缩短占用剪贴板的时间
        let payload = prepare(items, sensitivity);
        if payload.is_empty() {
            return Err(ClipboardError::Platform("没有可写入的内容".into()));
        }
        self.worker.run(move |hwnd| write_on_worker(hwnd, &payload))?
    }

    fn set_watch_paused(&self, paused: bool) -> Result<(), ClipboardError> {
        self.worker.set_watch_paused(paused)
    }

    fn watch(&self, on_change: ChangeHandler) -> Result<(), ClipboardError> {
        self.worker.watch(on_change)
    }

    fn write_virtual_files(
        &self,
        entries: &[FileEntry],
        others: &[ClipData],
        contents: Arc<dyn FileContents>,
        sensitivity: Sensitivity,
    ) -> Result<u64, ClipboardError> {
        if !self.worker.ole_ready() {
            return Err(ClipboardError::Unsupported);
        }
        let descriptors = files::descriptors(entries);
        if descriptors.skipped_long > 0 {
            // FILEDESCRIPTORW 无法表示更长的相对路径。让调用方改用实体文件预取，
            // 不能成功发布一份缺少文件的清单。
            return Err(ClipboardError::Unsupported);
        }
        let mut names = std::collections::HashSet::new();
        for entry in entries {
            let name = files::descriptor_path(&entry.path);
            if name.is_empty() || !names.insert(name.to_lowercase()) {
                return Err(ClipboardError::Platform(
                    "虚拟文件清单包含空路径或规范化后重名的路径".into(),
                ));
            }
        }
        if descriptors.index.is_empty() {
            return Err(ClipboardError::Platform("没有可写入的文件".into()));
        }
        let f = formats();
        let mut fixed = prepare(others, sensitivity);
        fixed.push((f.preferred_drop_effect, DROPEFFECT_COPY.to_le_bytes().to_vec()));

        let spec = vfile::Spec {
            cloud: cloud::Offer::new(entries.to_vec(), contents.clone(), None),
            descriptors,
            fixed,
            contents,
            cf_descriptor: f.file_descriptor,
            cf_contents: f.file_contents,
        };
        self.worker.run(move |_| vfile::set_clipboard(spec))?
    }
}

/// `Preferred DropEffect` 的值：复制。
const DROPEFFECT_COPY: u32 = 1;

/// 剪贴板线程收到 `WM_CLOSE`（backend 被丢弃）时调用：处理仍在剪贴板上的虚拟文件（见 [`vfile`]）。
pub(crate) fn on_worker_close(hwnd: HWND) {
    vfile::shutdown(formats().file_descriptor, |fixed| {
        if let Err(e) = write_on_worker(hwnd, fixed) {
            tracing::warn!("退出时写回其他表示形式失败：{e}");
        }
    });
}

/// 注册格式的 ID。
struct Formats {
    html: u32,
    rtf: u32,
    png: u32,
    exclude_monitor: u32,
    can_include_history: u32,
    viewer_ignore: u32,
    file_descriptor: u32,
    file_contents: u32,
    preferred_drop_effect: u32,
}

fn formats() -> &'static Formats {
    static F: OnceLock<Formats> = OnceLock::new();
    F.get_or_init(|| {
        let reg = |name: &str| {
            let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: `w` 以 NUL 结尾，调用期间有效。
            unsafe { RegisterClipboardFormatW(PCWSTR(w.as_ptr())) }
        };
        Formats {
            html: reg("HTML Format"),
            rtf: reg("Rich Text Format"),
            png: reg("PNG"),
            exclude_monitor: reg("ExcludeClipboardContentFromMonitorProcessing"),
            can_include_history: reg("CanIncludeInClipboardHistory"),
            viewer_ignore: reg("Clipboard Viewer Ignore"),
            file_descriptor: reg("FileGroupDescriptorW"),
            file_contents: reg("FileContents"),
            preferred_drop_effect: reg("Preferred DropEffect"),
        }
    })
}

/// `OpenClipboard` 被其他程序占用时的重试间隔（毫秒），合计约 300 ms。
const OPEN_RETRY_MS: [u64; 10] = [5, 10, 15, 20, 25, 30, 40, 50, 50, 60];

/// 打开的剪贴板；Drop 时关闭。
struct OpenGuard;

impl OpenGuard {
    fn open(hwnd: HWND) -> Result<Self, ClipboardError> {
        for (i, delay) in std::iter::once(0).chain(OPEN_RETRY_MS).enumerate() {
            if delay > 0 {
                std::thread::sleep(Duration::from_millis(delay));
            }
            // SAFETY: hwnd 是剪贴板线程的窗口，本函数只在该线程上调用。
            if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
                if i > 0 {
                    tracing::debug!("OpenClipboard 第 {} 次重试成功", i);
                }
                return Ok(Self);
            }
        }
        // SAFETY: 只查询当前占用者，用于诊断。
        let holder = unsafe {
            GetOpenClipboardWindow().ok().map(|w| {
                let mut pid = 0u32;
                GetWindowThreadProcessId(w, Some(&mut pid));
                pid
            })
        };
        tracing::warn!("剪贴板被占用（进程 {holder:?}），放弃");
        Err(ClipboardError::Busy)
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        // SAFETY: 与 open() 成对调用。
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn available(format: u32) -> bool {
    // SAFETY: 只查询格式是否存在。
    unsafe { IsClipboardFormatAvailable(format) }.is_ok()
}

/// 读取某个格式的全部字节（剪贴板须已打开）。
fn get_bytes(format: u32) -> Option<Vec<u8>> {
    // SAFETY: 剪贴板已由调用方打开；句柄在关闭剪贴板前有效，锁定期间只读取 GlobalSize 字节。
    unsafe {
        let h = GetClipboardData(format).ok()?;
        let hg = HGLOBAL(h.0);
        let size = GlobalSize(hg);
        if size > opaque::ITEM_MAX {
            return None;
        }
        let p = GlobalLock(hg) as *const u8;
        if p.is_null() {
            return None;
        }
        let data = std::slice::from_raw_parts(p, size).to_vec();
        let _ = GlobalUnlock(hg);
        Some(data)
    }
}

/// 写入某个格式（剪贴板须已打开并清空）。
fn set_bytes(format: u32, bytes: &[u8]) -> Result<(), ClipboardError> {
    // SAFETY: 分配可移动全局内存并复制数据；SetClipboardData 成功后内存归系统所有，失败时由我们释放。
    unsafe {
        let hg = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len().max(1))
            .map_err(|e| ClipboardError::Platform(format!("GlobalAlloc: {e}")))?;
        let p = GlobalLock(hg) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(hg));
            return Err(ClipboardError::Platform("GlobalLock 失败".into()));
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(hg);
        if let Err(e) = SetClipboardData(format, Some(HANDLE(hg.0))) {
            let _ = GlobalFree(Some(hg));
            return Err(ClipboardError::Platform(format!("SetClipboardData({format}): {e}")));
        }
    }
    Ok(())
}

fn read_dword(format: u32) -> Option<u32> {
    let b = get_bytes(format)?;
    Some(u32::from_le_bytes(b.get(..4)?.try_into().ok()?))
}

/// ANSI（系统代码页）→ Unicode。
pub(crate) fn ansi_to_string(b: &[u8]) -> String {
    codepage_to_string(b, CP_ACP)
}
pub(super) fn codepage_to_string(b: &[u8], codepage: u32) -> String {
    // SAFETY: 先查询长度再转换，缓冲区大小正确。
    unsafe {
        let n = MultiByteToWideChar(codepage, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), b, None);
        let mut w = vec![0u16; n.max(0) as usize];
        let n = MultiByteToWideChar(codepage, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), b, Some(&mut w));
        String::from_utf16_lossy(&w[..n.max(0) as usize])
    }
}

pub(crate) fn decode_codepage(bytes: &[u8], codepage: u32) -> std::io::Result<String> {
    if bytes.is_empty() {
        return Ok(String::new());
    }
    let invalid = || crate::content::unavailable(crate::model::Unavailable::InvalidData);
    // SAFETY: query the required UTF-16 length before allocating the output;
    // invalid multibyte sequences fail instead of becoming replacement text.
    unsafe {
        let n = MultiByteToWideChar(
            codepage,
            windows::Win32::Globalization::MB_ERR_INVALID_CHARS,
            bytes,
            None,
        );
        if n <= 0 {
            return Err(invalid());
        }
        let mut words = vec![0; n as usize];
        let n = MultiByteToWideChar(
            codepage,
            windows::Win32::Globalization::MB_ERR_INVALID_CHARS,
            bytes,
            Some(&mut words),
        );
        if n <= 0 {
            return Err(invalid());
        }
        String::from_utf16(&words[..n as usize]).map_err(|_| invalid())
    }
}

/// 读取剪贴板：快照 + `CF_HDROP` 里的顶层路径（由调用方展开）。
fn read_on_worker(hwnd: HWND) -> Result<(ClipSnapshot, Vec<PathBuf>), ClipboardError> {
    let f = formats();
    let _open = OpenGuard::open(hwnd)?;
    let change_count = vfile::change_count();

    let sensitivity = if available(f.viewer_ignore) {
        Sensitivity::Transient
    } else if available(f.exclude_monitor)
        || (available(f.can_include_history) && read_dword(f.can_include_history) == Some(0))
    {
        Sensitivity::Concealed
    } else {
        Sensitivity::Normal
    };

    let mut items = Vec::new();
    if available(u32::from(CF_UNICODETEXT.0))
        && let Some(b) = get_bytes(u32::from(CF_UNICODETEXT.0))
    {
        let units: Vec<u16> = b
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
        items.push(ClipData::Text(html::crlf_to_lf(&String::from_utf16_lossy(
            &units[..end],
        ))));
    }
    if available(f.html)
        && let Some(h) = get_bytes(f.html).and_then(|b| html::decode_cf_html(&b))
    {
        items.push(ClipData::Html(h));
    }
    if available(f.rtf)
        && let Some(mut b) = get_bytes(f.rtf)
    {
        if let Some(end) = b.iter().position(|&c| c == 0) {
            b.truncate(end);
        }
        items.push(ClipData::Rtf(b));
    }
    if let Some(png) = read_image(f) {
        items.push(ClipData::Png(png));
    }
    let drops = if available(u32::from(CF_HDROP.0)) {
        get_bytes(u32::from(CF_HDROP.0))
            .map(|b| files::parse_hdrop(&b, ansi_to_string))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut cf = 0;
    loop {
        // SAFETY: clipboard is open on this thread; only registered HGLOBAL formats are read.
        cf = unsafe { EnumClipboardFormats(cf) };
        if cf == 0 {
            break;
        }
        if !(0xc000..=0xffff).contains(&cf) {
            continue;
        }
        let mut name = [0u16; 256];
        // SAFETY: name is a writable UTF-16 buffer, cf is an enumerated registered format.
        let n = unsafe { GetClipboardFormatNameW(cf, &mut name) };
        if n <= 0 {
            continue;
        }
        let name = String::from_utf16_lossy(&name[..n as usize]);
        if opaque::windows_custom(&name)
            && let Some(bytes) = get_bytes(cf)
        {
            items.push(opaque::custom(ClipPlatform::Windows, &name, bytes));
        }
    }
    Ok((
        ClipSnapshot {
            items,
            sensitivity,
            change_count,
        },
        drops,
    ))
}

fn read_image(f: &Formats) -> Option<Vec<u8>> {
    const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
    if available(f.png) {
        match get_bytes(f.png) {
            Some(b) if b.starts_with(PNG_MAGIC) => return Some(b),
            _ => tracing::debug!("注册格式 PNG 的内容不是 PNG，改读 DIB"),
        }
    }
    for format in [u32::from(CF_DIBV5.0), u32::from(CF_DIB.0)] {
        if !available(format) {
            continue;
        }
        let Some(dib) = get_bytes(format) else { continue };
        match image::dib_to_png(&dib) {
            Ok(png) => return Some(png),
            Err(e) => tracing::warn!("DIB（格式 {format}）转 PNG 失败：{e}"),
        }
    }
    None
}

/// 要写入的（格式, 字节）列表：各表示形式 + 隐私标记。
fn prepare(items: &[ClipData], sensitivity: Sensitivity) -> Vec<(u32, Vec<u8>)> {
    prepare_items(&opaque::for_platform(items, ClipPlatform::Windows, sensitivity))
}

/// 各表示形式的（格式, 字节）。
fn prepare_items(items: &[ClipData]) -> Vec<(u32, Vec<u8>)> {
    let f = formats();
    let mut out = Vec::new();
    for item in items {
        match item {
            ClipData::Text(s) => {
                let bytes: Vec<u8> = html::lf_to_crlf(s)
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .flat_map(u16::to_le_bytes)
                    .collect();
                out.push((u32::from(CF_UNICODETEXT.0), bytes));
            }
            ClipData::Html(h) => out.push((f.html, html::encode_cf_html(h))),
            ClipData::Rtf(r) => {
                let mut b = r.clone();
                if b.last() != Some(&0) {
                    b.push(0);
                }
                out.push((f.rtf, b));
            }
            ClipData::Png(png) => {
                out.push((f.png, png.clone()));
                match image::decode_png(png) {
                    Ok(img) => {
                        out.push((u32::from(CF_DIBV5.0), image::rgba_to_dibv5(&img)));
                        out.push((u32::from(CF_DIB.0), image::rgba_to_dib24(&img)));
                    }
                    Err(e) => tracing::warn!("PNG 解码失败，只写入 PNG 格式：{e}"),
                }
            }
            ClipData::Custom {
                platform: ClipPlatform::Windows,
                name,
                bytes,
            } if opaque::windows_custom(name) => {
                let w: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
                // SAFETY: NUL terminated format name.
                let cf = unsafe { RegisterClipboardFormatW(PCWSTR(w.as_ptr())) };
                if cf != 0 && !out.iter().any(|(f, _)| *f == cf) {
                    out.push((cf, bytes.clone()));
                }
            }
            ClipData::Custom { .. } | ClipData::Native { .. } | ClipData::FileRoots(_) => {}
            ClipData::Files(entries) => {
                let paths = files::top_local_paths(entries);
                if paths.is_empty() {
                    tracing::warn!("文件清单里没有本机路径，不写 CF_HDROP（未下载的文件用 write_virtual_files）");
                } else {
                    out.push((u32::from(CF_HDROP.0), files::build_hdrop(&paths)));
                    out.push((f.preferred_drop_effect, DROPEFFECT_COPY.to_le_bytes().to_vec()));
                }
            }
        }
    }
    out
}

fn write_on_worker(hwnd: HWND, payload: &[(u32, Vec<u8>)]) -> Result<u64, ClipboardError> {
    // 剪贴板马上被清空，OLE 会释放之前的虚拟文件数据对象
    vfile::forget();
    {
        let _open = OpenGuard::open(hwnd)?;
        // SAFETY: 剪贴板已打开；清空后所有者为我们的窗口。
        unsafe { EmptyClipboard() }.map_err(|e| ClipboardError::Platform(format!("EmptyClipboard: {e}")))?;
        for (format, bytes) in payload {
            set_bytes(*format, bytes)?;
        }
    }
    // SAFETY: 无参数的系统调用。
    Ok(u64::from(unsafe { GetClipboardSequenceNumber() }))
}

#[cfg(all(test, debug_assertions))]
mod custom_tests {
    use super::*;
    #[test]
    fn exact_windows_markers_and_custom_payloads_are_prepared() {
        let f = formats();
        let items = vec![
            opaque::custom(ClipPlatform::Windows, "CanIncludeInClipboardHistory", vec![1, 0, 0, 0]),
            opaque::custom(ClipPlatform::Windows, "Example Custom Binary", vec![0, 255, 0]),
            opaque::custom(ClipPlatform::Mac, "example.mac", vec![7]),
        ];
        let prepared = prepare(&items, Sensitivity::Concealed);
        assert_eq!(prepared.len(), 2);
        assert!(prepared.contains(&(f.can_include_history, vec![1, 0, 0, 0])));
        assert!(!prepared.iter().any(|(id, _)| *id == f.exclude_monitor));
        assert!(
            prepare(&[ClipData::Text("normal".into())], Sensitivity::Normal)
                .iter()
                .all(|(id, _)| *id == u32::from(CF_UNICODETEXT.0))
        );
    }
}
