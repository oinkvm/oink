//! Enumerate clipboard file URLs as [`FileEntry`] metadata,
//! 文件内容优先由 File Provider 交付；文件承诺降级用 materialize 写入接收方指定目录。
//!
//! 展开规则：
//! - 每个顶层条目（被复制的文件或文件夹）各自成根，`path` 就是它的名字；文件夹递归展开，父目录在前、
//!   同一目录内按名字排序，`path` 用 `/` 分隔。顶层名字重复（例如从搜索结果里复制了两个同名文件）时
//!   后来者改名为 `名字 (2).扩展名`。
//! - **符号链接**：顶层条目是符号链接时跟随它（用户复制的就是它指向的东西）；文件夹内部指向文件的链接
//!   当作普通文件（内容取自目标），指向文件夹的链接与断开的链接跳过（避免循环、避免把树外的内容带出去）。
//! - **包**（`.app`、`.bundle`、`.rtfd`、`.pages` 等）：在磁盘上就是文件夹，按文件夹展开，接收端重建出
//!   同名文件夹，Finder 会再把它显示成包。清单不带权限位与扩展属性，所以 `.app` 传过去后通常不能直接运行
//!   （可执行位、代码签名丢失）；文档类的包（`.rtfd`、`.pages`）不受影响。
//! - `.DS_Store` 跳过；套接字、管道、设备文件跳过；读不了的文件夹保留条目本身、跳过其内容。
//! - 上限：`MAX_ENTRIES` 个条目或 `MAX_WALK` 时间，超过时放弃整个文件清单（其他表示形式照常），
//!   避免复制了整个主目录时读剪贴板卡住。

use crate::FileContents;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::FileEntry;

/// 清单条目数上限。
pub(crate) const MAX_ENTRIES: usize = 100_000;
/// 展开的时间上限。
pub(crate) const MAX_WALK: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ExpandError {
    /// 超过条目数或时间上限。
    TooLarge { entries: usize },
}

#[derive(Default, Debug)]
pub(crate) struct Skipped {
    pub dir_links: usize,
    pub broken_links: usize,
    pub special: usize,
    pub unreadable: usize,
}

/// 把顶层路径展开成清单。读不到的顶层条目跳过。
pub(crate) fn expand(roots: &[PathBuf]) -> Result<(Vec<FileEntry>, Skipped), ExpandError> {
    expand_with(roots, MAX_ENTRIES, MAX_WALK)
}

pub(crate) fn expand_with(
    roots: &[PathBuf],
    max_entries: usize,
    max_time: Duration,
) -> Result<(Vec<FileEntry>, Skipped), ExpandError> {
    let started = Instant::now();
    let mut out = Vec::new();
    let mut skipped = Skipped::default();
    let mut names = HashSet::new();
    for root in roots {
        // 顶层跟随符号链接。
        let meta = match fs::metadata(root) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(path = %root.display(), "读不到被复制的条目：{e}");
                skipped.broken_links += 1;
                continue;
            }
        };
        let Some(raw) = root.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue; // 「/」本身
        };
        if !meta.is_dir() && !meta.is_file() {
            skipped.special += 1;
            continue;
        }
        let name = unique_name(&raw, &mut names);
        out.push(entry(name.clone(), &meta, root.clone()));
        if meta.is_dir() {
            walk(root, &name, &mut out, &mut skipped, started, max_entries, max_time)?;
        }
        if out.len() > max_entries {
            return Err(ExpandError::TooLarge { entries: out.len() });
        }
    }
    Ok((out, skipped))
}

/// 顶层名字去重：`a.txt`、`a (2).txt`、`a (3).txt`……
fn unique_name(raw: &str, seen: &mut HashSet<String>) -> String {
    if seen.insert(raw.to_owned()) {
        return raw.to_owned();
    }
    let (stem, ext) = match raw.rfind('.') {
        Some(i) if i > 0 => (&raw[..i], &raw[i..]),
        _ => (raw, ""),
    };
    (2..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|c| seen.insert(c.clone()))
        .expect("总能找到不重复的名字")
}

fn modified_ms(meta: &fs::Metadata) -> Option<i64> {
    let t = meta.modified().ok()?;
    Some(match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    })
}

fn entry(path: String, meta: &fs::Metadata, local: PathBuf) -> FileEntry {
    FileEntry {
        path,
        is_dir: meta.is_dir(),
        size: if meta.is_dir() { 0 } else { meta.len() },
        modified_ms: modified_ms(meta),
        local_path: Some(local),
    }
}

/// 深度优先、父目录在前；用显式栈，不怕很深的目录。
fn walk(
    root: &Path,
    root_rel: &str,
    out: &mut Vec<FileEntry>,
    skipped: &mut Skipped,
    started: Instant,
    max_entries: usize,
    max_time: Duration,
) -> Result<(), ExpandError> {
    // 栈里放「待处理的条目」；每个文件夹的子项按名字排序后倒序压栈，得到先序（与 `find` 相同的顺序）。
    let mut stack: Vec<(PathBuf, String)> = Vec::new();
    push_children(root, root_rel, &mut stack, skipped);
    while let Some((path, rel)) = stack.pop() {
        if out.len() > max_entries || started.elapsed() > max_time {
            return Err(ExpandError::TooLarge { entries: out.len() });
        }
        let Ok(lmeta) = fs::symlink_metadata(&path) else {
            skipped.unreadable += 1;
            continue;
        };
        if lmeta.file_type().is_symlink() {
            match fs::metadata(&path) {
                Ok(m) if m.is_file() => out.push(entry(rel, &m, path)),
                Ok(m) if m.is_dir() => skipped.dir_links += 1,
                Ok(_) => skipped.special += 1,
                Err(_) => skipped.broken_links += 1,
            }
        } else if lmeta.is_dir() {
            out.push(entry(rel.clone(), &lmeta, path.clone()));
            push_children(&path, &rel, &mut stack, skipped);
        } else if lmeta.is_file() {
            out.push(entry(rel, &lmeta, path));
        } else {
            skipped.special += 1;
        }
    }
    Ok(())
}

fn push_children(dir: &Path, rel: &str, stack: &mut Vec<(PathBuf, String)>, skipped: &mut Skipped) {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::debug!(path = %dir.display(), "读不了文件夹：{e}");
            skipped.unreadable += 1;
            return;
        }
    };
    let mut children: Vec<(String, PathBuf)> = rd
        .filter_map(Result::ok)
        .map(|d| (d.file_name().to_string_lossy().into_owned(), d.path()))
        .filter(|(n, _)| n != ".DS_Store")
        .collect();
    children.sort_by(|a, b| b.0.cmp(&a.0));
    stack.extend(children.into_iter().map(|(n, p)| (p, format!("{rel}/{n}"))));
}

/// 顶层条目（`path` 不含 `/`）。
pub(crate) fn is_top_level(e: &FileEntry) -> bool {
    !e.path.contains('/')
}

pub(crate) fn materialize(
    entries: &[FileEntry],
    dests: &[PathBuf],
    contents: &Arc<dyn FileContents>,
) -> io::Result<Vec<PathBuf>> {
    for (i, (e, dest)) in entries.iter().zip(dests).enumerate() {
        if e.is_dir {
            fs::create_dir_all(dest)?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tmp = dest.with_file_name(format!(".{name}.{}.oink-part", std::process::id()));
        let result = (|| {
            let mut reader = contents.open(i)?;
            let metadata = contents.metadata(i)?.unwrap_or(crate::FileMetadata {
                size: e.size,
                modified_ms: e.modified_ms,
            });
            let mut f = File::create(&tmp)?;
            let n = io::copy(&mut reader, &mut f)?;
            if n != metadata.size {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("{}：收到 {n} 字节，清单里是 {}", e.path, metadata.size),
                ));
            }
            if let Some(ms) = metadata.modified_ms {
                let t = if ms >= 0 {
                    UNIX_EPOCH + Duration::from_millis(ms as u64)
                } else {
                    UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs())
                };
                let _ = f.set_modified(t);
            }
            f.sync_all()?;
            fs::rename(&tmp, dest)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result?;
    }
    Ok(entries
        .iter()
        .zip(dests)
        .filter(|(e, _)| is_top_level(e))
        .map(|(_, d)| d.clone())
        .collect())
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::SystemTime;

    pub(crate) struct TempDir(pub PathBuf);
    impl TempDir {
        pub fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "oink-clip-{tag}-{}-{}",
                std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
            ));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn expands_folders_parent_first_with_rules() {
        let t = TempDir::new("expand");
        let top = t.0.join("Top");
        write(&top.join("b.txt"), "bb");
        write(&top.join("a/x.txt"), "x");
        write(&top.join("a/deep/y.txt"), "yyy");
        write(&top.join(".DS_Store"), "junk");
        fs::create_dir_all(top.join("empty")).unwrap();
        write(&t.0.join("outside.txt"), "outside");
        symlink(t.0.join("outside.txt"), top.join("link-to-file")).unwrap();
        symlink(&top, top.join("a/loop")).unwrap(); // 指向文件夹：跳过
        symlink(t.0.join("missing"), top.join("broken")).unwrap();
        let single = t.0.join("single.bin");
        write(&single, "12345");

        let (list, skipped) = expand(&[top.clone(), single.clone()]).unwrap();
        let paths: Vec<&str> = list.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "Top",
                "Top/a",
                "Top/a/deep",
                "Top/a/deep/y.txt",
                "Top/a/x.txt",
                "Top/b.txt",
                "Top/empty",
                "Top/link-to-file",
                "single.bin"
            ]
        );
        // 父目录总在子项之前。
        for (i, e) in list.iter().enumerate() {
            if let Some((parent, _)) = e.path.rsplit_once('/') {
                assert!(list[..i].iter().any(|p| p.path == parent && p.is_dir), "{}", e.path);
            }
        }
        let by = |p: &str| list.iter().find(|e| e.path == p).unwrap();
        assert!(by("Top").is_dir && by("Top/empty").is_dir);
        assert_eq!(by("Top/a/deep/y.txt").size, 3);
        assert_eq!(by("Top/link-to-file").size, 7);
        assert_eq!(
            by("Top/link-to-file").local_path.as_deref(),
            Some(&*top.join("link-to-file"))
        );
        assert_eq!(by("single.bin").local_path.as_deref(), Some(&*single));
        assert!(by("single.bin").modified_ms.unwrap() > 1_600_000_000_000);
        assert_eq!(skipped.dir_links, 1);
        assert_eq!(skipped.broken_links, 1);
    }

    #[test]
    fn top_level_symlink_is_followed_and_names_are_unique() {
        let t = TempDir::new("toplink");
        write(&t.0.join("d1/a.txt"), "1");
        write(&t.0.join("d2/a.txt"), "22");
        write(&t.0.join("real/inner.txt"), "i");
        symlink(t.0.join("real"), t.0.join("alias")).unwrap();
        let (list, _) = expand(&[t.0.join("d1/a.txt"), t.0.join("d2/a.txt"), t.0.join("alias")]).unwrap();
        let paths: Vec<&str> = list.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "a (2).txt", "alias", "alias/inner.txt"]);
        assert_eq!(list[1].size, 2);
    }

    #[test]
    fn limits_abort_the_walk() {
        let t = TempDir::new("limit");
        for i in 0..20 {
            write(&t.0.join(format!("d/f{i}")), "x");
        }
        assert!(matches!(
            expand_with(&[t.0.join("d")], 10, MAX_WALK),
            Err(ExpandError::TooLarge { .. })
        ));
        assert!(expand_with(&[t.0.join("d")], 100, MAX_WALK).is_ok());
    }

    /// 大目录：两万个文件（200 个文件夹 × 100 个文件）的展开时间。
    #[test]
    fn large_tree_is_fast_enough() {
        let t = TempDir::new("large");
        let root = t.0.join("big");
        for d in 0..200 {
            let dir = root.join(format!("dir{d:03}"));
            fs::create_dir_all(&dir).unwrap();
            for f in 0..100 {
                fs::write(dir.join(format!("f{f:03}.txt")), b"").unwrap();
            }
        }
        let t0 = Instant::now();
        let (list, _) = expand(std::slice::from_ref(&root)).unwrap();
        let took = t0.elapsed();
        assert_eq!(list.len(), 1 + 200 + 200 * 100);
        eprintln!("展开 {} 个条目用时 {took:?}", list.len());
        assert!(took < Duration::from_secs(3), "展开太慢：{took:?}");
    }
}
