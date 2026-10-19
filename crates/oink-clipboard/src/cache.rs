//! Accounting for Oink-owned storage; never follows links or deletes delivered user files.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheCategory {
    ClipboardFiles,
    DragFiles,
    ProviderCatalog,
    ProviderMaterialized,
    ProviderTemporary,
    DeliveredFiles,
    SourceSnapshots,
    PromiseExports,
    CompletedDeliveries,
    Other,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheUsage {
    pub categories: Vec<CacheSize>,
    pub total_bytes: u64,
    /// Some storage could not be inspected; totals are a lower bound.
    pub incomplete: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheSize {
    pub category: CacheCategory,
    pub bytes: u64,
    pub kept_bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheClear {
    pub removed_bytes: u64,
    pub kept_bytes: u64,
    pub usage: CacheUsage,
}

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::OnceLock,
};
use tokio::sync::broadcast;

static CHANGES: OnceLock<broadcast::Sender<PathBuf>> = OnceLock::new();
pub fn subscribe() -> broadcast::Receiver<PathBuf> {
    CHANGES.get_or_init(|| broadcast::channel(128).0).subscribe()
}
pub(crate) fn changed(root: &Path) {
    let _ = CHANGES.get_or_init(|| broadcast::channel(128).0).send(root.into());
}

pub fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}
/// Allocated bytes, including sidecars; clones may share blocks. Never reads contents.
pub fn allocated(path: &Path, meta: &fs::Metadata) -> io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = path;
        Ok(meta.blocks() * 512)
    }
    #[cfg(windows)]
    {
        use std::os::windows::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
            io::AsRawHandle,
        };
        use windows::{
            Win32::{
                Foundation::{GetLastError, HANDLE, SetLastError, WIN32_ERROR},
                Storage::FileSystem::{
                    FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_SPARSE_FILE, FILE_FLAG_OPEN_REPARSE_POINT,
                    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
                    FileStandardInfo, GetCompressedFileSizeW, GetFileInformationByHandleEx,
                },
            },
            core::PCWSTR,
        };
        // Ordinary files need allocated clusters; compressed/sparse files need
        // their physical size rather than nominal uncompressed allocation.
        if meta.file_attributes() & (FILE_ATTRIBUTE_COMPRESSED.0 | FILE_ATTRIBUTE_SPARSE_FILE.0) == 0 {
            let file = fs::OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES.0)
                .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
                .open(path)?;
            let mut info = FILE_STANDARD_INFO::default();
            // SAFETY: file owns the live handle; info is writable for the exact
            // FILE_STANDARD_INFO size requested and remains alive for this call.
            unsafe {
                GetFileInformationByHandleEx(
                    HANDLE(file.as_raw_handle()),
                    FileStandardInfo,
                    (&mut info as *mut FILE_STANDARD_INFO).cast(),
                    std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
                )
            }
            .map_err(io::Error::other)?;
            return Ok(info.AllocationSize.max(0) as u64);
        }
        let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut high = 0;
        // SAFETY: name is NUL-terminated and high is a live writable output.
        // The thread-local error is read immediately after the size query.
        unsafe {
            SetLastError(WIN32_ERROR(0));
            let low = GetCompressedFileSizeW(PCWSTR(name.as_ptr()), Some(&mut high));
            if low == u32::MAX && GetLastError().0 != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok((u64::from(high) << 32) | u64::from(low))
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(meta.len())
    }
}
pub fn tree_bytes(path: &Path) -> io::Result<u64> {
    let meta = fs::symlink_metadata(path)?;
    if is_link(&meta) {
        return Err(io::Error::other("linked cache entry"));
    }
    if !meta.is_dir() {
        return allocated(path, &meta);
    }
    fs::read_dir(path)?.try_fold(0u64, |n, e| Ok(n.saturating_add(tree_bytes(&e?.path())?)))
}
// Shared source files can themselves reside in an Oink provider/cache root.
// Source publications and each live serving task own these leases.
static SOURCE_FILES: OnceLock<std::sync::Mutex<std::collections::HashMap<PathBuf, SourcePins>>> = OnceLock::new();
// Both purposes use this same lifetime mutex. Expiry may ignore publication
// owners, but never readers; manual cache clearing continues to respect both.
#[derive(Default)]
pub(crate) struct SourcePins {
    owners: usize,
    pub(crate) readers: usize,
}
pub struct SourceLease(PathBuf, bool, bool);
impl std::fmt::Debug for SourceLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SourceLease")
    }
}
impl SourceLease {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self::acquire(path, true, false)
    }
    /// Scoped protection for a freshly read current system clipboard.
    pub fn current(path: PathBuf) -> Self {
        Self::acquire(path, false, false)
    }
    pub(crate) fn create_directory(create: impl FnOnce() -> io::Result<PathBuf>) -> io::Result<Self> {
        Self::create_pinned(create, true)
    }
    pub(crate) fn create_owner(create: impl FnOnce() -> io::Result<PathBuf>) -> io::Result<Self> {
        Self::create_pinned(create, false)
    }
    fn create_pinned(create: impl FnOnce() -> io::Result<PathBuf>, reader: bool) -> io::Result<Self> {
        let mut sources = source_guard();
        let path = create()?;
        let pins = sources.entry(path.clone()).or_default();
        if reader {
            pins.readers += 1;
        } else {
            pins.owners += 1;
        }
        drop(sources);
        source_changed(&path);
        Ok(Self(path, true, reader))
    }
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
    pub(crate) fn reading(path: PathBuf) -> Self {
        Self::acquire(path, true, true)
    }
    fn acquire(path: PathBuf, publish: bool, reader: bool) -> Self {
        let mut sources = source_guard();
        let pins = sources.entry(path.clone()).or_default();
        if reader {
            pins.readers += 1;
        } else {
            pins.owners += 1;
        }
        drop(sources);
        if publish {
            source_changed(&path);
        }
        Self(path, publish, reader)
    }
}
impl Drop for SourceLease {
    fn drop(&mut self) {
        let mut sources = SOURCE_FILES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(count) = sources.get_mut(&self.0) {
            if self.2 {
                count.readers -= 1;
            } else {
                count.owners -= 1;
            }
            if count.readers == 0 && count.owners == 0 {
                sources.remove(&self.0);
            }
        }
        drop(sources);
        if self.1 {
            source_changed(&self.0);
        }
    }
}
#[cfg(target_os = "macos")]
pub(crate) fn source_paths() -> Vec<PathBuf> {
    SOURCE_FILES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect()
}

fn source_changed(path: &Path) {
    #[cfg(target_os = "macos")]
    if let Some(root) = crate::file_provider::cache_root() {
        changed(&root);
    }
    if let Some(parent) = path.parent() {
        changed(parent);
    }
    if let Some(root) = path.ancestors().find(|p| {
        p.file_name()
            .is_some_and(|n| crate::store::StoreKind::ALL.iter().any(|k| n == k.directory()))
    }) {
        changed(root);
    }
}

pub(crate) fn source_guard() -> std::sync::MutexGuard<'static, std::collections::HashMap<PathBuf, SourcePins>> {
    SOURCE_FILES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
#[cfg(all(test, debug_assertions))]
mod tests;
