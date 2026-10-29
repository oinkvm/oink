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
    #[cfg(all(test, debug_assertions))]
    MUTATIONS.with(|changes| {
        if let Some(changes) = changes.borrow_mut().as_mut() {
            changes.push(root.into());
        }
    });
}

/// Invalidate after an owned mutation finishes, including partial failure.
/// Declare before file/lock guards so consumers see the closed/committed state.
pub(crate) struct Mutation<'a>(pub &'a Path);
impl Drop for Mutation<'_> {
    fn drop(&mut self) {
        changed(self.0);
    }
}

#[cfg(all(test, debug_assertions))]
thread_local! {
    static MUTATIONS: std::cell::RefCell<Option<Vec<PathBuf>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(all(test, debug_assertions))]
pub(crate) fn assert_mutation<T>(root: &Path, operation: impl FnOnce() -> T) -> T {
    MUTATIONS.with(|changes| *changes.borrow_mut() = Some(Vec::new()));
    let result = operation();
    assert!(
        MUTATIONS.with(|changes| changes.borrow_mut().take().unwrap().iter().any(|path| path == root)),
        "owner returned without publishing a cache change"
    );
    result
}

#[cfg(all(test, debug_assertions))]
pub(crate) fn assert_no_mutation<T>(root: &Path, operation: impl FnOnce() -> T) -> T {
    MUTATIONS.with(|changes| *changes.borrow_mut() = Some(Vec::new()));
    let result = operation();
    assert!(
        !MUTATIONS.with(|changes| changes.borrow_mut().take().unwrap().iter().any(|path| path == root)),
        "unchanged accounting metadata must not schedule another scan"
    );
    result
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
static SOURCE_FILES: OnceLock<std::sync::Mutex<std::collections::HashMap<PathBuf, usize>>> = OnceLock::new();
pub struct SourceLease(PathBuf, bool);
impl std::fmt::Debug for SourceLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SourceLease")
    }
}
impl SourceLease {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self::acquire(path, true)
    }
    /// Scoped protection for a freshly read current system clipboard.
    pub fn current(path: PathBuf) -> Self {
        Self::acquire(path, false)
    }
    pub(crate) fn create_owner(create: impl FnOnce() -> io::Result<PathBuf>) -> io::Result<Self> {
        let mut sources = source_guard();
        let path = create()?;
        *sources.entry(path.clone()).or_default() += 1;
        drop(sources);
        source_changed(&path);
        Ok(Self(path, true))
    }
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
    pub(crate) fn reading(path: PathBuf) -> Self {
        Self::acquire(path, true)
    }
    fn acquire(path: PathBuf, publish: bool) -> Self {
        let mut sources = source_guard();
        *sources.entry(path.clone()).or_default() += 1;
        drop(sources);
        if publish {
            source_changed(&path);
        }
        Self(path, publish)
    }
}
impl Drop for SourceLease {
    fn drop(&mut self) {
        let mut sources = SOURCE_FILES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(count) = sources.get_mut(&self.0) {
            *count -= 1;
            if *count == 0 {
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

pub(crate) fn source_guard() -> std::sync::MutexGuard<'static, std::collections::HashMap<PathBuf, usize>> {
    SOURCE_FILES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
#[cfg(all(test, debug_assertions))]
mod tests;
