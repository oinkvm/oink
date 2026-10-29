//! 剪贴板。
//!
//! - [`ClipboardBackend`]：读写本机系统剪贴板、监听变化。平台实现在 `src/macos`、`src/windows`。
//! - [`sync`]：与平台无关的同步逻辑（通告、拉取、防回环、隐私标记透传）。

// The AppKit harness compiles this crate with cfg(test) without libtest, whose
// #[test] bodies are omitted. Their fixture helpers are consequently unused.
#![cfg_attr(test, allow(dead_code, unused_imports))]

#[cfg(test)]
fn main() {
    #[cfg(all(target_os = "macos", debug_assertions))]
    macos::run_native_linked_promise_tests();
}

pub mod cache;
#[cfg(any(windows, test))]
mod cloud_files;
pub mod content;
pub mod model;
pub mod negotiation;
pub mod policy;
pub mod received;
pub mod timing;

pub mod provider_preparation;
use std::sync::Arc;

pub use oink_foundation::clipboard::{ClipData, ClipSnapshot, FileEntry, Sensitivity};

/// Fixed, path-free reasons shared by provider status, logs and diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum FileProviderReason {
    #[error("checking")]
    Checking,
    #[error("domain_enabled")]
    DomainEnabled,
    #[error("urls_ready")]
    UrlsReady,
    #[error("domain_disabled")]
    DomainDisabled,
    #[error("extension_not_registered")]
    ExtensionNotRegistered,
    #[error("domain_disconnected")]
    DomainDisconnected,
    #[error("registration_failed")]
    RegistrationFailed,
    #[error("bound_expired")]
    BoundExpired,
    #[error("cancelled")]
    Cancelled,
    #[error("item_missing")]
    ItemMissing,
    #[error("item_unsupported_type")]
    ItemUnsupportedType,
    #[error("item_kind_mismatch")]
    ItemKindMismatch,
    #[error("item_target_mismatch")]
    ItemTargetMismatch,
    #[error("item_identity_mismatch")]
    ItemIdentityMismatch,
    #[error("item_metadata_mismatch")]
    ItemMetadataMismatch,
    #[error("item_access_denied")]
    ItemAccessDenied,
    #[error("item_metadata_unreadable")]
    ItemMetadataUnreadable,
}
impl FileProviderReason {
    pub fn unavailable(self) -> bool {
        !matches!(
            self,
            Self::Checking | Self::DomainEnabled | Self::UrlsReady | Self::Cancelled
        )
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn item_rejected(self) -> bool {
        matches!(
            self,
            Self::ItemMissing
                | Self::ItemUnsupportedType
                | Self::ItemKindMismatch
                | Self::ItemTargetMismatch
                | Self::ItemIdentityMismatch
                | Self::ItemMetadataMismatch
                | Self::ItemAccessDenied
                | Self::ItemMetadataUnreadable
        )
    }
    pub fn needs_enable(self) -> bool {
        matches!(self, Self::DomainDisabled | Self::ExtensionNotRegistered)
    }
    pub fn detail(self) -> String {
        match self {
            Self::Checking => "正在检查文件扩展 [checking]".into(),
            Self::DomainEnabled => "文件域已启用；尚未验证本批文件 URL 发布 [domain_enabled]".into(),
            Self::UrlsReady => "最近一次文件 URL 发布成功；新批次仍需等待系统就绪 [urls_ready]".into(),
            Self::BoundExpired => "等待文件 URL 就绪超时，本次拖放已取消；请检查文件扩展后重试 [bound_expired]".into(),
            Self::Cancelled => "文件拖放准备已取消 [cancelled]".into(),
            Self::ItemMissing
            | Self::ItemUnsupportedType
            | Self::ItemKindMismatch
            | Self::ItemTargetMismatch
            | Self::ItemIdentityMismatch
            | Self::ItemMetadataMismatch
            | Self::ItemAccessDenied
            | Self::ItemMetadataUnreadable => format!("文件项准备被拒绝，本次拖放已取消 [{self}]"),
            reason => format!("文件提供服务不可用；请在设置中检查或启用 Oink 文件扩展 [{reason}]"),
        }
    }
}

/// `FileContents::open` 返回的读取器。
pub type FileReader = Box<dyn std::io::Read + Send>;

pub mod dnd;
pub mod formats;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::file_provider;
pub mod store;
pub mod sync;
#[cfg(any(windows, target_os = "macos", test))]
mod upgrade;
#[cfg(windows)]
mod windows;
pub mod wire;

#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error("本平台不支持")]
    Unsupported,
    #[error("剪贴板被其他程序占用")]
    Busy,
    /// The current item's files or metadata are unreadable; the native clipboard
    /// service is still available for the next local copy or remote publication.
    #[error("clipboard content unavailable: {0:?}")]
    ContentUnavailable(model::Unavailable),
    #[error("文件扩展不可用，可在预取限额内下载后粘贴")]
    FileProviderUnavailable,
    #[error("文件扩展：{0}")]
    FileProvider(FileProviderReason),
    #[error("系统调用失败：{0}")]
    Platform(String),
}

/// 剪贴板变化回调，可能在任意线程调用。
pub type ChangeHandler = Arc<dyn Fn() + Send + Sync>;

/// Native capture metadata and the independently owned bytes/provider it names.
pub struct CapturedClipboard {
    pub snapshot: ClipSnapshot,
    pub content: Option<Arc<dyn content::Provider>>,
}
impl std::fmt::Debug for CapturedClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapturedClipboard")
            .field("items", &self.snapshot.items.len())
            .finish()
    }
}
#[derive(Clone)]
pub struct NativeSpec {
    pub inventory: crate::model::Inventory,
    pub contents: Arc<dyn content::Provider>,
    pub files: Vec<FileEntry>,
    pub file_contents: Option<Arc<dyn FileContents>>,
    pub sensitivity: Sensitivity,
}
impl NativeSpec {
    /// Resolve promise containers on the native publication worker before
    /// creating descriptor tables, placeholders, or target pasteboard items.
    pub(crate) fn prepare_promises(&mut self) -> std::io::Result<()> {
        if !self.inventory.items.iter().any(|item| {
            item.file.is_none()
                && item
                    .representations
                    .iter()
                    .any(|rep| matches!(rep.data, crate::model::RepresentationData::Promise(..)))
        }) {
            return Ok(());
        }
        let namespace = self
            .file_contents
            .as_ref()
            .and_then(|files| files.namespace())
            .ok_or_else(|| content::unavailable(crate::model::Unavailable::InvalidData))?;
        namespace.prepare()?;
        self.inventory = content::native_metadata::project_inventory(
            self.inventory.clone(),
            namespace.roots(),
            &namespace.native_file_metadata(),
        )?;
        let files = content::namespace::Files::new(namespace, crate::sync::files::TargetOs::current())?;
        self.files = files.roots().to_vec();
        self.file_contents = Some(files);
        Ok(())
    }
    /// Keep usable non-file formats when native file publication is unavailable.
    pub fn without_files(mut self) -> std::io::Result<Option<Self>> {
        self.inventory = crate::policy::without_files(self.inventory);
        self.files.clear();
        self.file_contents = None;
        if self.inventory.items.is_empty() {
            return Ok(None);
        }
        self.contents = content::projected(self.contents, self.inventory.clone())?;
        Ok(Some(self))
    }
}
impl std::fmt::Debug for NativeSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSpec")
            .field("items", &self.inventory.items.len())
            .field("files", &self.files.len())
            .finish()
    }
}

pub trait ClipboardBackend: Send + Sync {
    /// Configure owned storage before publishing native content. Platform
    /// recovery runs on its ordinary worker, never an input callback.
    fn initialize_store(&self, _data: std::path::PathBuf) {}
    /// 系统剪贴板的变化计数（macOS `changeCount`；Windows `GetClipboardSequenceNumber`）。
    fn change_count(&self) -> u64;
    /// Metadata-only identity of the actual clipboard, scoped to this native
    /// login/boot and clipboard. Unlike change_count it does not hide delayed
    /// rendering. Called on a background worker, including while sharing pauses.
    fn clipboard_key(&self) -> std::io::Result<[u8; 32]> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
    /// 读取当前内容中本项目支持的表示形式，并判断隐私级别（例如 macOS
    /// `org.nspasteboard.ConcealedType`、Windows `ExcludeClipboardContentFromMonitorProcessing`）。
    fn read(&self) -> Result<ClipSnapshot, ClipboardError>;
    /// Native backends announce metadata and read the current system clipboard
    /// on demand, checking its change identity. They never store source payloads. Normalized
    /// in-memory/test backends can continue supplying their explicit ClipData.
    /// A native capture error is returned, never retried through a lossy reader.
    fn capture(&self, _data: &std::path::Path) -> Result<CapturedClipboard, ClipboardError> {
        Ok(CapturedClipboard {
            snapshot: self.read()?,
            content: None,
        })
    }
    fn write_native(&self, _spec: NativeSpec) -> Result<u64, ClipboardError> {
        Err(ClipboardError::Unsupported)
    }
    /// Recheck owner authority after blocking preparation, immediately before publication.
    fn write_native_guarded(&self, _spec: NativeSpec, _guard: content::ReaderGuard) -> Result<u64, ClipboardError> {
        Err(ClipboardError::Unsupported)
    }
    /// 一次性写入多种表示形式，透传原始隐私标记（无显式标记时用旧 `sensitivity`），返回写入后的变化计数
    /// （同步逻辑用它识别「这次变化是自己写的」，防止回环）。
    fn write(&self, items: &[ClipData], sensitivity: Sensitivity) -> Result<u64, ClipboardError>;
    /// 注册变化回调（macOS 没有通知，由实现轮询 `changeCount`，间隔 ≤ 250 ms）。
    fn watch(&self, on_change: ChangeHandler) -> Result<(), ClipboardError>;
    /// Suspend platform observation, preserving registrations for explicit resume.
    fn set_watch_paused(&self, _paused: bool) -> Result<(), ClipboardError> {
        Ok(())
    }

    /// 写入「虚拟文件」——只有清单，内容在系统真正读取时才通过 `contents` 按需获取
    /// (Windows: `OleSetClipboard`, `CFSTR_FILEDESCRIPTORW` and `CFSTR_FILECONTENTS`).
    /// `others` 是同时写入的其他表示形式（例如文件名文本）。返回写入后的变化计数。
    ///
    /// macOS: registers system File Provider placeholders and writes their URLs. An unavailable
    /// extension returns an error; the caller applies its bounded prefetch policy.
    fn write_virtual_files(
        &self,
        entries: &[FileEntry],
        others: &[ClipData],
        contents: Arc<dyn FileContents>,
        sensitivity: Sensitivity,
    ) -> Result<u64, ClipboardError> {
        let _ = (entries, others, contents, sensitivity);
        Err(ClipboardError::Unsupported)
    }
}

/// 源端打开文件后确认的元数据，可能不同于复制时的清单。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileMetadata {
    pub size: u64,
    pub modified_ms: Option<i64>,
    pub exec_bits: Option<u16>,
}

/// 按需提供文件内容；调用可能等待网络，只能在平台后台线程使用。
pub trait FileContents: Send + Sync {
    /// OLE calls this at its first descriptor GetData, never while advertising
    /// formats. The returned table remains immutable for this native object.
    fn descriptor_entries(&self) -> std::io::Result<Option<Vec<FileEntry>>> {
        Ok(None)
    }
    /// Stable node identities in exactly the published descriptor index order.
    /// Native catalogs persist this table without creating another file backing.
    fn descriptor_nodes(&self) -> std::io::Result<Option<Vec<model::FileNode>>> {
        Ok(None)
    }
    fn descriptor_size_known(&self, _index: usize) -> bool {
        true
    }
    fn namespace(&self) -> Option<Arc<dyn content::namespace::Namespace>> {
        None
    }
    /// 打开清单中第 `index` 个条目（必须是文件）的内容，从头读取。
    fn open(&self, index: usize) -> std::io::Result<Box<dyn std::io::Read + Send>>;
    /// 当前文件元数据；实现可以等待源端 FileInfo（仅后台线程调用）。
    /// None 表示该 provider 无动态元数据，使用清单值。
    fn metadata(&self, _index: usize) -> std::io::Result<Option<FileMetadata>> {
        Ok(None)
    }
    /// Cloud Files uses mutable placeholders, unlike an OLE descriptor table.
    /// Select current metadata/version before hydration, update the placeholder,
    /// then open that exact version. A changed version during IO fails the read.
    fn placeholder_metadata(&self, index: usize) -> std::io::Result<Option<(FileMetadata, u64)>> {
        self.metadata(index)
            .map(|metadata| metadata.map(|metadata| (metadata, 0)))
    }
    fn open_placeholder(&self, index: usize, _version: u64) -> std::io::Result<FileReader> {
        self.open(index).map(content::terminal_reader)
    }
    /// Whether background content work is still running after the last native reader closed.
    fn cache_in_use(&self) -> bool {
        false
    }
    /// 系统放弃读取（例如用户取消粘贴），可以停止后台下载。默认什么也不做。
    fn cancel(&self) {}
}

/// 创建本机平台的实现。
pub fn platform_backend() -> Result<Box<dyn ClipboardBackend>, ClipboardError> {
    #[cfg(target_os = "macos")]
    return macos::backend();
    #[cfg(windows)]
    return windows::backend();
    #[allow(unreachable_code)]
    Err(ClipboardError::Unsupported)
}

// Exercise the pure Windows bitmap codec on every development platform.
#[cfg(all(test, debug_assertions, not(windows)))]
#[path = "windows/image.rs"]
mod windows_image_tests;
