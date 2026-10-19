//! 文件名在目标系统上的处理：非法字符、Windows 保留名、`.` / `..`、过长的名字、大小写不敏感时的重名。
//!
//! 清单里的路径来自另一台设备（`/` 分隔的相对路径），写到本机磁盘或交给系统之前必须处理：
//! - 任何系统：去掉空的段、`.`；`..` 与控制字符替换掉（不能借此写到接收目录之外）；
//! - Windows：`< > : " | ? * \` 换成外形相近的全角字符；去掉结尾的空格和点；`CON`、`NUL`、`COM1` 等保留名加 `_`；
//! - 单个名字不超过 255（Windows 按 UTF-16 单元、macOS 按 UTF-8 字节），截短时保留扩展名；
//! - 两个条目处理后同名（两个系统默认都不区分大小写）时，后一个加 ` (2)`、` (3)`……，其子条目随之改变。

use crate::FileEntry;
use std::collections::{HashMap, HashSet};

/// 文件将要写入的系统。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetOs {
    Windows,
    MacOs,
    /// 其他 Unix：只做通用处理。
    Other,
}

impl TargetOs {
    /// 本机。
    pub const fn current() -> Self {
        if cfg!(windows) {
            TargetOs::Windows
        } else if cfg!(target_os = "macos") {
            TargetOs::MacOs
        } else {
            TargetOs::Other
        }
    }
}

const WIN_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "COM¹",
    "COM²", "COM³", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²",
    "LPT³", "CONIN$", "CONOUT$",
];

const MAX_NAME: usize = 255;

fn name_len(s: &str, os: TargetOs) -> usize {
    match os {
        TargetOs::Windows => s.encode_utf16().count(),
        _ => s.len(),
    }
}

/// 截短到 `max`（按目标系统的计量），在字符边界上截断。
fn truncate_to(s: &str, max: usize, os: TargetOs) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let mut t = out.clone();
        t.push(c);
        if name_len(&t, os) > max {
            break;
        }
        out = t;
    }
    out
}

/// 拆成主名与扩展名（扩展名含点；以点开头的名字如 `.gitignore` 没有扩展名）。
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= 32 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

fn limit_len(name: String, os: TargetOs) -> String {
    if name_len(&name, os) <= MAX_NAME {
        return name;
    }
    let (stem, ext) = split_ext(&name);
    let room = MAX_NAME.saturating_sub(name_len(ext, os));
    let mut s = truncate_to(stem, room, os);
    s.push_str(ext);
    s
}

/// 处理一个路径段（文件或文件夹名）。结果非空、不含路径分隔符，也不是 `.` / `..`。
pub fn sanitize_component(name: &str, os: TargetOs) -> String {
    // 接收目录的生命周期标记不能与用户文件同名，避免覆盖用户内容。
    if name.eq_ignore_ascii_case(".oink-managed") || name.eq_ignore_ascii_case(".oink-protected") {
        return format!("{name} (file)");
    }
    let mut s: String = name
        .chars()
        .map(|c| match c {
            '\0'..='\x1f' | '\x7f' => '_',
            '/' => '／',
            '\\' if os == TargetOs::Windows => '＼',
            '<' if os == TargetOs::Windows => '＜',
            '>' if os == TargetOs::Windows => '＞',
            ':' if os == TargetOs::Windows => '：',
            '"' if os == TargetOs::Windows => '＂',
            '|' if os == TargetOs::Windows => '｜',
            '?' if os == TargetOs::Windows => '？',
            '*' if os == TargetOs::Windows => '＊',
            c => c,
        })
        .collect();
    if !s.is_empty() && s.chars().all(|c| c == '.') {
        // `.`、`..`：不能指向当前或上级目录。
        s = s.replace('.', "_");
    }
    if os == TargetOs::Windows {
        // Win32 会悄悄去掉结尾的空格和点，结果可能与别的文件同名，或者根本打不开。
        let trimmed = s.trim_end_matches([' ', '.']).len();
        s.truncate(trimmed);
        let (stem, _) = s.split_once('.').unwrap_or((&s, ""));
        let stem_trim = stem.trim_end_matches(' ');
        if WIN_RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem_trim)) {
            s.insert(stem.len(), '_');
        }
    }
    if s.is_empty() {
        s.push('_');
    }
    limit_len(s, os)
}

/// 把一个相对路径（`/` 分隔）的各段依次处理，去掉空段与 `.`。结果可能为空（例如路径只有 `/`）。
pub fn sanitize_path(path: &str, os: TargetOs) -> Vec<String> {
    path.split(['/', '\\'].as_slice())
        .filter(|p| !p.is_empty() && *p != ".")
        .map(|p| sanitize_component(p, os))
        .collect()
}

pub(crate) fn with_suffix(name: &str, n: usize, os: TargetOs) -> String {
    let (stem, ext) = split_ext(name);
    let suffix = format!(" ({n}){ext}");
    let room = MAX_NAME.saturating_sub(name_len(&suffix, os));
    format!("{}{suffix}", truncate_to(stem, room, os))
}

/// 处理整份清单：各段按目标系统处理，重名（不区分大小写）的加序号，子条目跟随父条目的新名字。
/// 返回的清单与输入一一对应（顺序、条数不变）。
pub fn sanitize_entries(entries: &[FileEntry], os: TargetOs) -> Vec<FileEntry> {
    // 原路径 → 处理后的路径（文件夹用于子条目）
    let mut mapped: HashMap<String, String> = HashMap::new();
    let mut used: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let raw: Vec<&str> = e.path.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
        let (parent_raw, name_raw) = match raw.split_last() {
            Some((last, parent)) => (parent.join("/"), *last),
            None => (String::new(), ""),
        };
        let parent = if parent_raw.is_empty() {
            String::new()
        } else if let Some(p) = mapped.get(&parent_raw) {
            p.clone()
        } else {
            // 父目录没有单独列出：逐段处理。
            sanitize_path(&parent_raw, os).join("/")
        };
        let base = sanitize_component(name_raw, os);
        let join = |name: &str| {
            if parent.is_empty() {
                name.to_string()
            } else {
                format!("{parent}/{name}")
            }
        };
        let mut path = join(&base);
        let mut n = 2;
        while !used.insert(path.to_lowercase()) {
            path = join(&with_suffix(&base, n, os));
            n += 1;
        }
        mapped.insert(raw.join("/"), path.clone());
        out.push(FileEntry {
            path,
            is_dir: e.is_dir,
            size: e.size,
            modified_ms: e.modified_ms,
            local_path: None,
        });
    }
    out
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    fn entry(path: &str, is_dir: bool) -> FileEntry {
        FileEntry {
            path: path.into(),
            is_dir,
            size: 0,
            modified_ms: None,
            local_path: None,
        }
    }

    #[test]
    fn windows_rules() {
        let w = TargetOs::Windows;
        assert_eq!(sanitize_component("a:b?c*.txt", w), "a：b？c＊.txt");
        assert_eq!(sanitize_component("CON", w), "CON_");
        assert_eq!(sanitize_component("con.txt", w), "con_.txt");
        assert_eq!(sanitize_component("Com1.tar.gz", w), "Com1_.tar.gz");
        assert_eq!(sanitize_component("CONSOLE.txt", w), "CONSOLE.txt");
        assert_eq!(sanitize_component("name. . ", w), "name");
        assert_eq!(sanitize_component("...", w), "___");
        assert_eq!(sanitize_component(" . ", w), "_");
        assert_eq!(sanitize_component("a\\b", w), "a＼b");
        assert_eq!(sanitize_component("tab\there", w), "tab_here");
        // macOS 上 `:` 与 `\` 合法（`:` 在访达里显示为 `/`），保留名也不是问题
        let m = TargetOs::MacOs;
        assert_eq!(sanitize_component("a:b\\c", m), "a:b\\c");
        assert_eq!(sanitize_component("CON", m), "CON");
    }

    #[test]
    fn traversal_is_neutralized() {
        for os in [TargetOs::Windows, TargetOs::MacOs, TargetOs::Other] {
            assert_eq!(sanitize_path("../../etc/passwd", os), vec!["__", "__", "etc", "passwd"]);
            assert_eq!(sanitize_path("/abs/./x", os), vec!["abs", "x"]);
            let e = sanitize_entries(&[entry("..", true), entry("../x", false)], os);
            assert_eq!(e[0].path, "__");
            assert_eq!(e[1].path, "__/x");
        }
    }

    #[test]
    fn long_names_keep_extension() {
        let long = format!("{}.txt", "长".repeat(300));
        let w = sanitize_component(&long, TargetOs::Windows);
        assert_eq!(w.encode_utf16().count(), 255);
        assert!(w.ends_with("长.txt"));
        let m = sanitize_component(&long, TargetOs::MacOs);
        assert!(m.len() <= 255, "{}", m.len());
        assert!(m.ends_with(".txt"));
    }

    #[test]
    fn collisions_get_suffix_and_children_follow() {
        let es = [
            entry("Dir", true),
            entry("Dir/a.txt", false),
            entry("dir", true),
            entry("dir/a.txt", false),
            entry("dir/A.TXT", false),
            entry("x?", false),
            entry("x？", false),
        ];
        let w = sanitize_entries(&es, TargetOs::Windows);
        let paths: Vec<&str> = w.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "Dir",
                "Dir/a.txt",
                "dir (2)",
                "dir (2)/a.txt",
                "dir (2)/A (2).TXT",
                "x？",
                "x？ (2)"
            ]
        );
    }
}
