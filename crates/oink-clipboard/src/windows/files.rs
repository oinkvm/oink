//! 文件（M4）：`CF_HDROP` 的解析与生成、文件夹展开成清单、虚拟文件的 `FILEGROUPDESCRIPTORW`。
//!
//! - 读取：`CF_HDROP`（`DROPFILES` + 以 NUL 分隔的路径）→ 顶层路径 → [`expand`] 展开成清单（父目录在前，
//!   `path` 为 `/` 分隔的相对路径，带大小、修改时间与 `local_path`）。长路径（超过 `MAX_PATH`）由 Rust 标准库
//!   在访问时自动加 `\\?\` 前缀，清单里的 `local_path` 保持普通形式。
//! - 写入实体文件：只写顶层条目（文件夹由粘贴方递归复制），另写 `Preferred DropEffect` = 复制。
//! - 虚拟文件：[`descriptors`] 把清单转成 `FILEGROUPDESCRIPTORW`，并清理不安全的路径（`..`、盘符、Windows 不允许的字符）。

mod native;
pub(crate) use native::native_descriptors;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use oink_foundation::clipboard::FileEntry;

/// `DROPFILES` 结构的大小（`pFiles` u32 + `POINT` + `fNC` + `fWide`）。
const DROPFILES_SIZE: usize = 20;

/// 解析 `CF_HDROP` 的内存块。`fWide` = 0（ANSI）时交给 `ansi` 解码。
pub(crate) fn parse_hdrop(bytes: &[u8], ansi: impl Fn(&[u8]) -> String) -> Vec<PathBuf> {
    let Some(head) = bytes.get(..DROPFILES_SIZE) else {
        return Vec::new();
    };
    let offset = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
    let wide = u32::from_le_bytes([head[16], head[17], head[18], head[19]]) != 0;
    let Some(list) = bytes.get(offset..) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if wide {
        let units: Vec<u16> = list.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        for s in units.split(|&c| c == 0) {
            if s.is_empty() {
                break;
            }
            out.push(PathBuf::from(String::from_utf16_lossy(s)));
        }
    } else {
        for s in list.split(|&c| c == 0) {
            if s.is_empty() {
                break;
            }
            out.push(PathBuf::from(ansi(s)));
        }
    }
    out
}

/// 生成 `CF_HDROP` 的内存块（宽字符）。
pub(crate) fn build_hdrop(paths: &[PathBuf]) -> Vec<u8> {
    let mut out = Vec::with_capacity(DROPFILES_SIZE + paths.len() * 64);
    out.extend_from_slice(&(DROPFILES_SIZE as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 12]); // pt、fNC
    out.extend_from_slice(&1u32.to_le_bytes()); // fWide
    for p in paths {
        let s = strip_verbatim(&p.to_string_lossy());
        for u in s.encode_utf16().chain(std::iter::once(0)) {
            out.extend_from_slice(&u.to_le_bytes());
        }
    }
    out.extend_from_slice(&[0, 0]);
    out
}

#[windows::core::implement(windows::Win32::UI::Shell::IFileSystemBindData)]
struct ShellFindData(std::sync::Mutex<windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW>);
impl windows::Win32::UI::Shell::IFileSystemBindData_Impl for ShellFindData_Impl {
    fn SetFindData(
        &self,
        data: *const windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW,
    ) -> windows::core::Result<()> {
        // SAFETY: a non-null COM argument covers one complete fixed-size record.
        let data = unsafe { data.as_ref() }.ok_or(windows::Win32::Foundation::E_POINTER)?;
        *self.0.lock().unwrap() = *data;
        Ok(())
    }
    fn GetFindData(
        &self,
        data: *mut windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW,
    ) -> windows::core::Result<()> {
        if data.is_null() {
            return Err(windows::Win32::Foundation::E_POINTER.into());
        }
        // SAFETY: checked writable COM output, no ownership transferred.
        unsafe {
            data.write(*self.0.lock().unwrap());
        }
        Ok(())
    }
}

/// Only Cloud Files directories get an ordinary-directory Shell view. A junction,
/// symlink or non-cloud reparse point must never acquire that representation.
fn shell_directory_metadata(
    mut data: windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW,
) -> Option<windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW> {
    use windows_sys::Win32::Storage::{CloudFilters::*, FileSystem::*};
    // SAFETY: pure attribute/tag classification; no filesystem access or hydration.
    let state = unsafe { CfGetPlaceholderStateFromAttributeTag(data.dwFileAttributes, data.dwReserved0) };
    if data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || state == CF_PLACEHOLDER_STATE_INVALID
        || state & CF_PLACEHOLDER_STATE_PLACEHOLDER == 0
    {
        return None;
    }
    data.dwFileAttributes &= !FILE_ATTRIBUTE_REPARSE_POINT;
    Some(data)
}

/// Shell path equivalents identify the actual containing folder and its direct
/// children. Shell consumers use IShellFolder child APIs, whose PIDLs must have
/// one SHITEMID; a desktop parent plus absolute, multilevel children is not that
/// representation. Received roots all live in one receiver-local batch folder.
pub(crate) fn shell_id_list(paths: &[PathBuf]) -> std::io::Result<Vec<u8>> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::{
            Storage::FileSystem::{FindClose, FindFirstFileW, WIN32_FIND_DATAW},
            System::Com::{CoTaskMemFree, CreateBindCtx},
            UI::Shell::{
                IFileSystemBindData, ILCreateFromPathW, ILFindLastID, ILFree, ILGetSize, IShellItem,
                SHCreateItemFromParsingName, SHGetIDListFromObject, STR_FILE_SYS_BIND_DATA,
            },
        },
        core::PCWSTR,
    };
    let invalid = || crate::content::unavailable(crate::model::Unavailable::InvalidData);
    if paths.is_empty() || paths.len() > 65_535 {
        return Err(invalid());
    }
    let parent = paths[0].parent().ok_or_else(invalid)?;
    if !parent.is_absolute() || paths.iter().any(|path| path.parent() != Some(parent)) {
        return Err(invalid());
    }
    let mut lists = Vec::with_capacity(paths.len() + 1);
    for (index, path) in std::iter::once(parent)
        .chain(paths.iter().map(PathBuf::as_path))
        .enumerate()
    {
        if !path.is_absolute() {
            return Err(invalid());
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: the path is NUL terminated. Shell owns the returned allocation
        // until the matching deallocator below; only its declared size is copied.
        let bytes = unsafe {
            let mut found = WIN32_FIND_DATAW::default();
            let metadata = if let Ok(find) = FindFirstFileW(PCWSTR(wide.as_ptr()), &mut found) {
                FindClose(find).map_err(|_| invalid())?;
                shell_directory_metadata(found)
            } else {
                None
            };
            let bound = metadata.is_some();
            let pidl = if let Some(metadata) = metadata {
                // Windows 11 Shell Drop silently omits an empty CF directory when
                // ILCreateFromPath caches its reparse representation. Population,
                // pinning and COPY do not repair it. Bind the receiver-owned folder
                // as an ordinary directory in this PIDL only; disk attributes, CF
                // identity, lazy discovery and source-read authority stay unchanged.
                // IFileOperation accepts the old PIDL; the native Drop regression
                // must cover this distinct consumer. Caller owns the native STA.
                let context = CreateBindCtx(0).map_err(|_| invalid())?;
                let data: IFileSystemBindData = ShellFindData(std::sync::Mutex::new(metadata)).into();
                context
                    .RegisterObjectParam(STR_FILE_SYS_BIND_DATA, &data)
                    .map_err(|_| invalid())?;
                let item: IShellItem =
                    SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), &context).map_err(|_| invalid())?;
                SHGetIDListFromObject(&item).map_err(|_| invalid())?
            } else {
                ILCreateFromPathW(PCWSTR(wide.as_ptr()))
            };
            if pidl.is_null() {
                return Err(invalid());
            }
            let item = if index == 0 { pidl } else { ILFindLastID(pidl) };
            let size = ILGetSize(Some(item));
            let bytes = if size >= 2 && size as usize <= crate::model::INVENTORY_BYTES {
                Some(std::slice::from_raw_parts(item.cast::<u8>(), size as usize).to_vec())
            } else {
                None
            };
            if bound {
                CoTaskMemFree(Some(pidl.cast()));
            } else {
                ILFree(Some(pidl));
            }
            bytes.ok_or_else(invalid)?
        };
        lists.push(bytes);
    }
    let mut offset = 4 + 4 * lists.len();
    let mut bytes = (paths.len() as u32).to_le_bytes().to_vec();
    for list in &lists {
        bytes.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += list.len();
        if offset > crate::model::INVENTORY_BYTES {
            return Err(invalid());
        }
    }
    for list in lists {
        bytes.extend(list);
    }
    Ok(bytes)
}
/// Rebuild path formats from our own HDROP, never from a sender's path/PIDL.
pub(crate) fn path_equivalent(name: &str, hdrop: &[u8]) -> std::io::Result<Vec<u8>> {
    let paths = parse_hdrop(hdrop, |_| unreachable!("our HDROP is UTF-16"));
    if name == "Shell IDList Array" {
        return shell_id_list(&paths);
    }
    if paths.len() != 1 {
        return Err(crate::content::unavailable(crate::model::Unavailable::Unsupported));
    }
    let path = strip_verbatim(
        paths[0]
            .to_str()
            .ok_or_else(|| crate::content::unavailable(crate::model::Unavailable::InvalidData))?,
    );
    if name == "FileNameW" {
        Ok(path.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect())
    } else if name == "FileName" {
        crate::content::url::ansi(&path)
    } else {
        Err(crate::content::unavailable(crate::model::Unavailable::Unsupported))
    }
}

/// 去掉 `\\?\` 前缀（`\\?\UNC\server\share` → `\\server\share`），粘贴方与剪贴板历史更认普通形式；
/// 超过 `MAX_PATH` 的路径仍能用（资源管理器与标准库都会自己处理）。
fn strip_verbatim(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        s.to_owned()
    }
}

fn modified_ms(meta: &std::fs::Metadata) -> Option<i64> {
    let t = meta.modified().ok()?;
    Some(match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).ok()?,
        Err(e) => -i64::try_from(e.duration().as_millis()).ok()?,
    })
}

/// 顶层条目的名字（`C:\` 这样的根目录没有文件名，用盘符）。
fn top_name(p: &Path) -> String {
    match p.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => p
            .to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .trim_end_matches(':')
            .trim_start_matches(r"\\?\")
            .replace(['\\', '/', ':'], "_"),
    }
}

/// 展开的统计（诊断与性能测试用）。
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ExpandStats {
    pub files: usize,
    pub dirs: usize,
    pub bytes: u64,
    pub unreadable: usize,
    pub elapsed: Duration,
}

/// 把顶层路径展开成清单：先序遍历（父目录在前），同一目录内按名字排序；不跟随目录的符号链接 / 联接点
/// （记为空目录，避免循环）；读不到的子目录记为空目录并计数。
pub(crate) fn expand(tops: &[PathBuf]) -> (Vec<FileEntry>, ExpandStats) {
    let started = Instant::now();
    let mut out = Vec::new();
    let mut stats = ExpandStats::default();
    let mut used: Vec<String> = Vec::new();
    for top in tops {
        let meta = match std::fs::metadata(top) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("读取 {} 失败：{e}", top.display());
                stats.unreadable += 1;
                continue;
            }
        };
        // 不同目录下的同名顶层条目（例如搜索结果）：加序号区分
        let mut name = top_name(top);
        if used.contains(&name) {
            let base = name.clone();
            let mut i = 2;
            while used.contains(&name) {
                name = format!("{base} ({i})");
                i += 1;
            }
        }
        used.push(name.clone());
        push_entry(&mut out, &mut stats, name.clone(), top, &meta);
        if meta.is_dir() {
            walk(top, &name, &mut out, &mut stats);
        }
    }
    stats.elapsed = started.elapsed();
    (out, stats)
}

fn push_entry(out: &mut Vec<FileEntry>, stats: &mut ExpandStats, path: String, local: &Path, meta: &std::fs::Metadata) {
    let is_dir = meta.is_dir();
    if is_dir {
        stats.dirs += 1;
    } else {
        stats.files += 1;
        stats.bytes += meta.len();
    }
    out.push(FileEntry {
        path,
        is_dir,
        size: if is_dir { 0 } else { meta.len() },
        modified_ms: modified_ms(meta),
        local_path: Some(local.to_path_buf()),
    });
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<FileEntry>, stats: &mut ExpandStats) {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::warn!("列出 {} 失败：{e}", dir.display());
            stats.unreadable += 1;
            return;
        }
    };
    let mut children: Vec<(String, PathBuf, std::fs::Metadata, bool)> = Vec::new();
    for ent in rd {
        let Ok(ent) = ent else {
            stats.unreadable += 1;
            continue;
        };
        // Windows 上 DirEntry::metadata 直接用枚举目录时得到的数据，不再打开文件；不跟随链接
        let Ok(lmeta) = ent.metadata() else {
            stats.unreadable += 1;
            continue;
        };
        let path = ent.path();
        let (meta, descend) = if lmeta.file_type().is_symlink() {
            // 文件链接跟随到目标；目录链接 / 联接点不进入
            match std::fs::metadata(&path) {
                Ok(m) => (m, false),
                Err(_) => {
                    stats.unreadable += 1;
                    continue;
                }
            }
        } else {
            let d = lmeta.is_dir();
            (lmeta, d)
        };
        children.push((ent.file_name().to_string_lossy().into_owned(), path, meta, descend));
    }
    children.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, path, meta, descend) in children {
        let rel_child = format!("{rel}/{name}");
        push_entry(out, stats, rel_child.clone(), &path, &meta);
        if descend {
            walk(&path, &rel_child, out, stats);
        }
    }
}

/// 顶层条目的名字，一行一个（复制文件时同时提供的文本表示）。
pub(crate) fn names_text(entries: &[FileEntry]) -> String {
    entries
        .iter()
        .filter(|e| !e.path.contains('/'))
        .map(|e| e.path.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 顶层条目的本机路径（写 `CF_HDROP` 用）；没有 `local_path` 的条目跳过。
pub(crate) fn top_local_paths(entries: &[FileEntry]) -> Vec<PathBuf> {
    entries
        .iter()
        .filter(|e| !e.path.contains('/'))
        .filter_map(|e| {
            if e.local_path.is_none() {
                tracing::warn!("文件 {} 没有本机路径，不能写成 CF_HDROP，跳过", e.path);
            }
            e.local_path.clone()
        })
        .collect()
}

// ---------- 虚拟文件：FILEGROUPDESCRIPTORW ----------

const FD_ATTRIBUTES: u32 = 0x0000_0004;
const FD_WRITESTIME: u32 = 0x0000_0020;
pub(crate) const FD_FILESIZE: u32 = 0x0000_0040;
const FD_PROGRESSUI: u32 = 0x0000_4000;
const FD_UNICODE: u32 = 0x8000_0000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
/// `FILEDESCRIPTORW` 的大小与 `cFileName` 的容量（含结尾 NUL）。
pub(crate) const FILEDESCRIPTOR_SIZE: usize = 592;
const NAME_CAP: usize = 260;

/// Windows 文件名里不允许的字符与保留名，换成安全的形式。
pub(super) fn sanitize_component(c: &str) -> String {
    let mut s: String = c
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    if s.is_empty() || s == "." || s == ".." {
        return "_".to_owned();
    }
    // 结尾的点和空格会被系统去掉，导致重名或找不到：换成下划线
    let keep = s.trim_end_matches(['.', ' ']).len();
    if keep < s.len() {
        let tail = s[keep..].chars().count();
        s.truncate(keep);
        s.extend(std::iter::repeat_n('_', tail));
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        s.insert(0, '_');
    }
    s
}

/// 清单里的相对路径 → 描述符里的相对路径（`\` 分隔）。去掉空段与 `.`，`..` 与盘符换成普通名字，
/// 保证粘贴结果不会跑到目标目录之外。
pub(crate) fn descriptor_path(path: &str) -> String {
    path.split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != ".")
        .map(sanitize_component)
        .collect::<Vec<_>>()
        .join("\\")
}

/// 一个描述符的摘要。
#[derive(Clone, Debug)]
pub(crate) struct DescMeta {
    /// 最后一段名字（`IStream::Stat` 的名字）。
    pub name: String,
    pub size: u64,
    /// FILETIME；没有修改时间时为 0。
    pub mtime: u64,
    pub is_dir: bool,
}

/// 生成的描述符。
pub(crate) struct Descriptors {
    /// `FILEGROUPDESCRIPTORW` 的字节。
    pub bytes: Vec<u8>,
    pub ansi: Option<Vec<u8>>,
    /// 描述符下标 → 清单下标（`FileContents::open` 的参数）。
    pub index: Vec<usize>,
    /// 描述符下标 → 名字、大小等，供 `IStream::Stat`。
    pub meta: Vec<DescMeta>,
    /// 因为路径太长（超过 259 个 UTF-16 单元）而跳过的条目数。
    pub skipped_long: usize,
}

/// Unix 毫秒 → FILETIME（1601 起的 100 ns）。
pub(crate) fn unix_ms_to_filetime(ms: i64) -> u64 {
    let t = (i128::from(ms) + 11_644_473_600_000) * 10_000;
    t.clamp(0, i128::from(u64::MAX)) as u64
}

pub(crate) fn descriptors(entries: &[FileEntry]) -> Descriptors {
    let mut bytes = vec![0u8; 4];
    let mut index = Vec::new();
    let mut meta = Vec::new();
    let mut skipped_long = 0;
    for (i, e) in entries.iter().enumerate() {
        let name = descriptor_path(&e.path);
        let units: Vec<u16> = name.encode_utf16().collect();
        if units.len() >= NAME_CAP {
            tracing::warn!(reason = "descriptor_path_limit", "virtual file descriptor unavailable");
            skipped_long += 1;
            continue;
        }
        let mut d = [0u8; FILEDESCRIPTOR_SIZE];
        let mut flags = FD_ATTRIBUTES | FD_PROGRESSUI | FD_UNICODE;
        // FILEDESCRIPTORW sizes are meaningful only with FD_FILESIZE. Shell
        // builds the whole-copy estimate before opening every FileContents
        // stream; a later IStream::Stat cannot repair a missing batch size.
        // The published inventory is the contract: unknown lengths are cleared
        // by vfile, and size/version changes fail with Unavailable::Superseded
        // during transfer rather than returning a prefix or another file version.
        // Do not select content here: Explorer also asks during hover, while the
        // original source must stay held until actual drop demand.
        // https://learn.microsoft.com/windows/win32/api/shlobj_core/ns-shlobj_core-filedescriptorw
        if !e.is_dir {
            flags |= FD_FILESIZE;
        }
        let ft = e.modified_ms.map(unix_ms_to_filetime);
        if e.is_dir && ft.is_some() {
            flags |= FD_WRITESTIME;
        }
        d[0..4].copy_from_slice(&flags.to_le_bytes());
        // clsid(16) sizel(8) pointl(8) 为 0
        let attrs = if e.is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        d[36..40].copy_from_slice(&attrs.to_le_bytes());
        // ftCreationTime 40..48、ftLastAccessTime 48..56、ftLastWriteTime 56..64
        if let Some(ft) = ft {
            d[56..60].copy_from_slice(&(ft as u32).to_le_bytes());
            d[60..64].copy_from_slice(&((ft >> 32) as u32).to_le_bytes());
        }
        let size = if e.is_dir { 0 } else { e.size };
        d[64..68].copy_from_slice(&((size >> 32) as u32).to_le_bytes());
        d[68..72].copy_from_slice(&(size as u32).to_le_bytes());
        for (k, u) in units.iter().enumerate() {
            d[72 + k * 2..74 + k * 2].copy_from_slice(&u.to_le_bytes());
        }
        bytes.extend_from_slice(&d);
        index.push(i);
        let display = name.rsplit('\\').next().unwrap_or(&name).to_owned();
        meta.push(DescMeta {
            name: display,
            size,
            mtime: ft.unwrap_or(0),
            is_dir: e.is_dir,
        });
    }
    let n = index.len() as u32;
    bytes[0..4].copy_from_slice(&n.to_le_bytes());
    Descriptors {
        ansi: None,
        bytes,
        index,
        meta,
        skipped_long,
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    fn entry(path: &str, is_dir: bool, size: u64) -> FileEntry {
        FileEntry {
            path: path.into(),
            is_dir,
            size,
            modified_ms: Some(1_700_000_000_000),
            local_path: None,
        }
    }

    #[test]
    fn hdrop_roundtrip() {
        let paths = vec![
            PathBuf::from(r"C:\tmp\a.txt"),
            PathBuf::from(r"C:\tmp\中文 目录"),
            PathBuf::from(r"\\?\C:\very\long"),
        ];
        let b = build_hdrop(&paths);
        let back = parse_hdrop(&b, |_| unreachable!());
        assert_eq!(back[0], paths[0]);
        assert_eq!(back[1], paths[1]);
        assert_eq!(back[2], PathBuf::from(r"C:\very\long"));
        assert_eq!(back.len(), 3);
    }

    #[test]
    fn hdrop_ansi_and_garbage() {
        let mut b = vec![0u8; 20];
        b[0] = 20;
        b.extend_from_slice(b"C:\\x.txt\0D:\\y\0\0");
        let back = parse_hdrop(&b, |s| String::from_utf8_lossy(s).into_owned());
        assert_eq!(back, vec![PathBuf::from(r"C:\x.txt"), PathBuf::from(r"D:\y")]);
        assert!(parse_hdrop(&[1, 2, 3], |_| String::new()).is_empty());
        let mut bad = vec![0u8; 20];
        bad[0] = 200;
        assert!(parse_hdrop(&bad, |_| String::new()).is_empty());
    }

    #[test]
    fn sanitizes_paths() {
        assert_eq!(descriptor_path("a/b/c.txt"), r"a\b\c.txt");
        assert_eq!(descriptor_path("../../etc/passwd"), r"_\_\etc\passwd");
        assert_eq!(descriptor_path("/abs//x"), "abs\\x");
        assert_eq!(descriptor_path("C:/x"), r"C_\x");
        assert_eq!(descriptor_path("a:b?.txt"), "a_b_.txt");
        assert_eq!(descriptor_path("dir./con.txt"), r"dir_\_con.txt");
        assert_eq!(descriptor_path("COM1"), "_COM1");
        assert_eq!(descriptor_path("COMX"), "COMX");
        assert_eq!(descriptor_path("a.. /b"), r"a___\b");
        assert_eq!(descriptor_path("中文/名字.txt"), r"中文\名字.txt");
    }

    #[test]
    fn builds_descriptors() {
        let long = "x".repeat(300);
        let entries = vec![
            entry("dir", true, 0),
            entry("dir/中文.txt", false, 5_000_000_000),
            entry(&long, false, 1),
            entry("b.bin", false, 7),
        ];
        let d = descriptors(&entries);
        assert_eq!(d.index, vec![0, 1, 3]);
        assert_eq!(d.skipped_long, 1);
        assert_eq!(d.bytes.len(), 4 + 3 * FILEDESCRIPTOR_SIZE);
        assert_eq!(u32::from_le_bytes(d.bytes[0..4].try_into().unwrap()), 3);
        let fd = |i: usize| &d.bytes[4 + i * FILEDESCRIPTOR_SIZE..4 + (i + 1) * FILEDESCRIPTOR_SIZE];
        let u32_at = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        // 目录：属性为目录，没有 FD_FILESIZE
        assert_eq!(u32_at(fd(0), 36), FILE_ATTRIBUTE_DIRECTORY);
        assert_eq!(u32_at(fd(0), 0) & FD_FILESIZE, 0);
        // 大文件：高低 32 位
        let f1 = fd(1);
        assert_eq!(u32_at(f1, 0) & FD_FILESIZE, FD_FILESIZE);
        let size = (u64::from(u32_at(f1, 64)) << 32) | u64::from(u32_at(f1, 68));
        assert_eq!(size, 5_000_000_000);
        let name: Vec<u16> = f1[72..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .take_while(|&c| c != 0)
            .collect();
        assert_eq!(String::from_utf16_lossy(&name), r"dir\中文.txt");
        assert_eq!(u32_at(f1, 0) & FD_WRITESTIME, 0);
        let ft = (u64::from(u32_at(f1, 60)) << 32) | u64::from(u32_at(f1, 56));
        assert_eq!(ft, unix_ms_to_filetime(1_700_000_000_000));
        assert_eq!(d.meta[1].name, "中文.txt");
        assert!(d.meta[0].is_dir && !d.meta[1].is_dir);
    }

    #[test]
    fn every_file_descriptor_advertises_its_size_and_progress() {
        let entries = vec![
            entry("first", false, 7),
            entry("directory", true, 0),
            entry("second", false, 5_000_000_000),
            entry("empty", false, 0),
        ];
        let d = descriptors(&entries);
        for (i, entry) in entries.iter().enumerate() {
            let fd = &d.bytes[4 + i * FILEDESCRIPTOR_SIZE..4 + (i + 1) * FILEDESCRIPTOR_SIZE];
            let field = |offset| u32::from_le_bytes(fd[offset..offset + 4].try_into().unwrap());
            assert_ne!(field(0) & FD_PROGRESSUI, 0, "descriptor {i}");
            assert_eq!(field(0) & FD_FILESIZE != 0, !entry.is_dir, "descriptor {i}");
            assert_eq!((u64::from(field(64)) << 32) | u64::from(field(68)), entry.size);
        }
    }

    #[test]
    fn filetime_epoch() {
        assert_eq!(unix_ms_to_filetime(0), 116_444_736_000_000_000);
    }

    #[test]
    fn expands_tree_parents_first() {
        let root = std::env::temp_dir().join(format!("oink-expand-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("top/sub/深")).unwrap();
        std::fs::create_dir_all(root.join("top/empty")).unwrap();
        std::fs::write(root.join("top/a.txt"), b"hello").unwrap();
        std::fs::write(root.join("top/sub/深/b.txt"), b"hi").unwrap();
        std::fs::write(root.join("single.bin"), [0u8; 3]).unwrap();
        let (entries, stats) = expand(&[root.join("top"), root.join("single.bin")]);
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "top",
                "top/a.txt",
                "top/empty",
                "top/sub",
                "top/sub/深",
                "top/sub/深/b.txt",
                "single.bin"
            ]
        );
        assert_eq!(stats.files, 3);
        assert_eq!(stats.dirs, 4);
        assert_eq!(stats.bytes, 10);
        let a = &entries[1];
        assert_eq!((a.is_dir, a.size), (false, 5));
        assert_eq!(a.local_path.as_deref(), Some(root.join("top/a.txt").as_path()));
        assert!(a.modified_ms.is_some());
        assert_eq!(names_text(&entries), "top\nsingle.bin");
        assert_eq!(
            top_local_paths(&entries),
            vec![root.join("top"), root.join("single.bin")]
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;
    #[test]
    fn shell_directory_view_only_masks_cloud_reparse_metadata() {
        use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
        let mut data = WIN32_FIND_DATAW {
            dwFileAttributes: 0x410,
            dwReserved0: 0x9000001a,
            ..Default::default()
        };
        let projected = shell_directory_metadata(data).unwrap();
        assert_eq!(projected.dwFileAttributes, 0x10);
        assert_eq!(data.dwFileAttributes, 0x410, "only the copied Shell view changes");
        data.dwReserved0 = 0xa000000c; // symlink
        assert!(shell_directory_metadata(data).is_none());
        data.dwReserved0 = 0xa0000003; // junction
        assert!(shell_directory_metadata(data).is_none());
        data.dwReserved0 = 0x9000001a;
        data.dwFileAttributes = 0x480; // regular cloud file
        assert!(shell_directory_metadata(data).is_none());
        data.dwFileAttributes = 0x10;
        data.dwReserved0 = 0;
        assert!(shell_directory_metadata(data).is_none());
    }

    #[test]
    fn shell_can_bind_the_received_selection_as_direct_children() {
        std::thread::spawn(|| {
            use windows::Win32::{
                System::{
                    Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL},
                    Ole::{OleInitialize, OleUninitialize},
                    SystemServices::{SFGAO_CANCOPY, SFGAO_CANMOVE, SFGAO_FILESYSTEM},
                },
                UI::Shell::{IShellFolder, SHBindToObject},
            };
            // SAFETY: this private test thread has no previous COM apartment.
            unsafe {
                OleInitialize(None).unwrap();
            }
            struct Apartment;
            impl Drop for Apartment {
                fn drop(&mut self) {
                    // SAFETY: balances this thread's successful OleInitialize.
                    unsafe {
                        OleUninitialize();
                    }
                }
            }
            let _apartment = Apartment;
            let root = tempfile::tempdir().unwrap();
            let paths = [root.path().join("one.txt"), root.path().join("two.txt")];
            for path in &paths {
                std::fs::write(path, b"fixture").unwrap();
            }
            let bytes = shell_id_list(&paths).unwrap();
            let pidl = |index: usize| {
                let at = 4 + 4 * index;
                let offset = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
                bytes[offset..].as_ptr().cast()
            };
            // SAFETY: CIDA offsets own complete native parent/child PIDLs for
            // the duration of these Shell calls. No desktop or shared root is used.
            unsafe {
                let folder: IShellFolder = SHBindToObject(None, pidl(0), None).unwrap();
                let wanted = (SFGAO_CANCOPY | SFGAO_CANMOVE | SFGAO_FILESYSTEM).0;
                let mut attributes = wanted;
                folder.GetAttributesOf(&[pidl(1), pidl(2)], &mut attributes).unwrap();
                assert_eq!(
                    attributes & wanted,
                    wanted,
                    "Shell must recognize the files as copy/move sources"
                );
                let data: IDataObject = folder
                    .GetUIObjectOf(Default::default(), &[pidl(1), pidl(2)], None)
                    .unwrap();
                let format = FORMATETC {
                    cfFormat: 15,
                    dwAspect: DVASPECT_CONTENT.0,
                    lindex: -1,
                    tymed: TYMED_HGLOBAL.0 as u32,
                    ..Default::default()
                };
                data.QueryGetData(&format).ok().unwrap();
            }
        })
        .join()
        .unwrap();
    }
    #[test]
    fn shell_paths_share_local_identity_and_legacy_names_require_one_item() {
        let temp = tempfile::tempdir().unwrap();
        let paths = [temp.path().join("first.txt"), temp.path().join("second.txt")];
        for path in &paths {
            std::fs::write(path, b"fixture").unwrap();
        }
        let bytes = shell_id_list(&paths).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[..4].try_into().unwrap()), 2);
        let parent_offset = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        for (index, expected) in paths.iter().enumerate() {
            let at = 8 + 4 * index;
            let offset = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            let child_size = u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
            assert!(child_size > 0);
            assert_eq!(
                &bytes[offset + child_size..offset + child_size + 2],
                &[0, 0],
                "Shell folder APIs require one direct child, not an absolute PIDL"
            );
            let mut wide = [0u16; 260];
            // SAFETY: offsets refer to complete native PIDLs copied above, and
            // the writable output buffer has the API's required MAX_PATH size.
            let resolved = unsafe {
                use windows::Win32::UI::Shell::{ILCombine, ILFree, SHGetPathFromIDListW};
                let absolute = ILCombine(
                    Some(bytes.as_ptr().add(parent_offset).cast()),
                    Some(bytes.as_ptr().add(offset).cast()),
                );
                assert!(!absolute.is_null());
                let result = SHGetPathFromIDListW(absolute, &mut wide);
                ILFree(Some(absolute));
                result
            };
            assert!(resolved.as_bool());
            let end = wide.iter().position(|v| *v == 0).unwrap();
            let resolved = PathBuf::from(String::from_utf16(&wide[..end]).unwrap());
            // Shell can expand an 8.3 spelling inherited from TEMP. Compare the
            // existing files' canonical paths so identity, not spelling, must match.
            assert_eq!(resolved.canonicalize().unwrap(), expected.canonicalize().unwrap());
        }
        let one = build_hdrop(&paths[..1]);
        let name = path_equivalent("FileNameW", &one).unwrap();
        let wide: Vec<_> = name
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .take_while(|v| *v != 0)
            .collect();
        assert_eq!(PathBuf::from(String::from_utf16(&wide).unwrap()), paths[0]);
        assert!(path_equivalent("FileNameW", &build_hdrop(&paths)).is_err());
        assert!(shell_id_list(&[PathBuf::from("relative")]).is_err());
        assert!(shell_id_list(&[paths[0].clone(), temp.path().join("nested/file.txt")]).is_err());
    }
}
