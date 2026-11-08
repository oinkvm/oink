//! Local “Paste as File”: format selection is shared by Finder and Explorer.
use crate::{content::Provider, model::*};
use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    RichText,
    Text,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    Files,
    Bytes {
        representation: RepresentationId,
        extension: &'static str,
        kind: Kind,
        translate: bool,
    },
}

/// Original encoded formats rank above a bitmap conversion, irrespective of
/// native format order. Do not decode/re-encode JPEG/GIF/HEIC or lose animation.
pub fn original_extension(format: &NativeFormat) -> Option<&'static str> {
    let name = match format {
        NativeFormat::Mac(n) | NativeFormat::WindowsRegistered(n) => n.to_ascii_lowercase(),
        _ => return None,
    };
    Some(match name.as_str() {
        "public.jpeg" | "jpeg" | "jfif" | "image/jpeg" => "jpg",
        "com.compuserve.gif" | "gif" | "image/gif" => "gif",
        "org.webmproject.webp" | "public.webp" | "webp" | "image/webp" => "webp",
        "public.heic" | "heic" | "image/heic" => "heic",
        "public.heif" | "heif" | "image/heif" => "heif",
        "public.png" | "png" | "image/png" => "png",
        "public.avif" | "avif" | "image/avif" => "avif",
        "com.microsoft.bmp" | "bmp" | "image/bmp" => "bmp",
        "public.jpeg-2000" | "image/jp2" => "jp2",
        "com.apple.icns" | "image/icns" => "icns",
        "public.svg-image" | "image/svg+xml" => "svg",
        "com.microsoft.ico" | "image/x-icon" => "ico",
        _ => return None,
    })
}
pub fn select(inventory: &Inventory) -> Option<Selection> {
    if !inventory.roots.is_empty() || inventory.has_promises() {
        return Some(Selection::Files);
    }
    let reps: Vec<_> = inventory
        .items
        .iter()
        .flat_map(|i| &i.representations)
        .filter(|r| !matches!(r.data, RepresentationData::Unavailable(_) | RepresentationData::File(_)))
        .collect();
    if let Some((rep, extension)) = reps
        .iter()
        .find_map(|rep| original_extension(&rep.format).map(|ext| (*rep, ext)))
    {
        return Some(Selection::Bytes {
            representation: rep.id,
            extension,
            kind: Kind::Image,
            translate: false,
        });
    }
    for (format, extension, kind) in [
        (StandardFormat::Png, "png", Kind::Image),
        (StandardFormat::Rtf, "rtf", Kind::RichText),
        (StandardFormat::Html, "html", Kind::RichText),
        (StandardFormat::Text, "txt", Kind::Text),
    ] {
        if let Some(rep) = reps.iter().find(|r| r.standard == Some(format)) {
            return Some(Selection::Bytes {
                representation: rep.id,
                extension,
                kind,
                translate: true,
            });
        }
    }
    None
}
pub fn text_bytes(text: &str, windows: bool) -> Vec<u8> {
    let lf = text.replace("\r\n", "\n").replace('\r', "\n");
    if windows {
        lf.replace('\n', "\r\n").into_bytes()
    } else {
        lf.into_bytes()
    }
}
pub fn stem(kind: Kind, language: &str, timestamp: &str) -> String {
    let labels = match language.split('-').next().unwrap_or(language) {
        "zh" => ["粘贴的图片", "粘贴的富文本", "粘贴的文本"],
        "ja" => ["貼り付けた画像", "貼り付けたリッチテキスト", "貼り付けたテキスト"],
        "ko" => ["붙여넣은 이미지", "붙여넣은 서식 있는 텍스트", "붙여넣은 텍스트"],
        _ => ["Pasted image", "Pasted rich text", "Pasted text"],
    };
    format!(
        "{} {timestamp}",
        labels[match kind {
            Kind::Image => 0,
            Kind::RichText => 1,
            Kind::Text => 2,
        }]
    )
}
fn candidate(directory: &Path, stem: &str, extension: &str, n: u32) -> PathBuf {
    let suffix = if n == 1 { String::new() } else { format!(" ({n})") };
    directory.join(if extension.is_empty() {
        format!("{stem}{suffix}")
    } else {
        format!("{stem}{suffix}.{extension}")
    })
}
fn create(directory: &Path, stem: &str, extension: &str, is_dir: bool) -> io::Result<(PathBuf, Option<std::fs::File>)> {
    for n in 1..=u32::MAX {
        let path = candidate(directory, stem, extension, n);
        let result = if is_dir {
            std::fs::create_dir(&path).map(|_| None)
        } else {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map(Some)
        };
        match result {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("filename suffix exhausted"))
}
pub fn save(
    provider: &dyn Provider,
    directory: &Path,
    language: &str,
    timestamp: &str,
    windows: bool,
) -> io::Result<Vec<PathBuf>> {
    let inventory = provider.inventory();
    // The clipboard may become empty after a native menu was constructed.
    // Return before reading bytes, validating a vanished target, or creating it.
    let Some(selection) = select(&inventory) else {
        return Ok(vec![]);
    };
    match selection {
        Selection::Files => {
            if !directory.is_dir() {
                return Err(io::ErrorKind::NotADirectory.into());
            }
            let namespace = provider
                .namespace()
                .ok_or_else(|| io::Error::other("clipboard files unavailable"))?;
            namespace.prepare()?;
            let mut paths = Vec::new();
            for node in namespace.roots() {
                if !node.valid() {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                let name = Path::new(&node.name);
                let (stem, ext) = if node.kind == NodeKind::Directory {
                    (node.name.as_str(), "")
                } else {
                    (
                        name.file_stem().and_then(|s| s.to_str()).unwrap_or(&node.name),
                        name.extension().and_then(|s| s.to_str()).unwrap_or(""),
                    )
                };
                let (path, file) = create(directory, stem, ext, node.kind == NodeKind::Directory)?;
                if let Err(error) = copy_node(namespace.as_ref(), &node, &path, file, 0) {
                    if path.is_dir() {
                        let _ = std::fs::remove_dir_all(&path);
                    } else {
                        let _ = std::fs::remove_file(&path);
                    }
                    return Err(error);
                }
                paths.push(path);
            }
            Ok(paths)
        }
        Selection::Bytes {
            representation,
            extension,
            kind,
            translate,
        } => {
            let rep = inventory
                .items
                .iter()
                .flat_map(|i| &i.representations)
                .find(|r| r.id == representation)
                .ok_or(io::ErrorKind::InvalidData)?;
            let mut bytes = Vec::new();
            provider
                .open(representation)?
                .take(REPRESENTATION_BYTES + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > REPRESENTATION_BYTES {
                return Err(io::Error::other("clipboard representation too large"));
            }
            if bytes.is_empty() {
                return Ok(vec![]);
            }
            if translate {
                let standard = provider.translator().standard_with_source(rep, &bytes, provider)?;
                bytes = standard
                    .into_iter()
                    .find_map(|data| match (kind, data) {
                        (Kind::Image, crate::ClipData::Png(b)) => Some(b),
                        (Kind::RichText, crate::ClipData::Rtf(b)) if extension == "rtf" => Some(b),
                        (Kind::RichText, crate::ClipData::Html(s)) if extension == "html" => Some(s.into_bytes()),
                        (Kind::Text, crate::ClipData::Text(s)) => Some(text_bytes(&s, windows)),
                        _ => None,
                    })
                    .ok_or(io::ErrorKind::InvalidData)?;
            }
            // CF_UNICODETEXT can contain only its NUL terminator. Check again
            // after translation so empty text is a no-op on either platform.
            if bytes.is_empty() {
                return Ok(vec![]);
            }
            if !directory.is_dir() {
                return Err(io::ErrorKind::NotADirectory.into());
            }
            let (path, file) = create(directory, &stem(kind, language, timestamp), extension, false)?;
            if let Err(error) = file.unwrap().write_all(&bytes) {
                let _ = std::fs::remove_file(&path);
                return Err(error);
            }
            Ok(vec![path])
        }
    }
}
fn copy_node(
    namespace: &dyn crate::content::namespace::Namespace,
    node: &FileNode,
    path: &Path,
    file: Option<std::fs::File>,
    depth: usize,
) -> io::Result<()> {
    if depth > 256 || !node.valid() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    match &node.kind {
        NodeKind::Directory => {
            let mut cursor = None;
            loop {
                let page = namespace.list(node.id, cursor)?;
                page.validate(cursor).map_err(|_| io::ErrorKind::InvalidData)?;
                for child in page.entries {
                    if !child.valid() {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                    let target = path.join(&child.name);
                    let file = if child.kind == NodeKind::Directory {
                        std::fs::create_dir(&target)?;
                        None
                    } else {
                        Some(std::fs::OpenOptions::new().write(true).create_new(true).open(&target)?)
                    };
                    copy_node(namespace, &child, &target, file, depth + 1)?;
                }
                cursor = page.next;
                if cursor.is_none() {
                    break;
                }
            }
        }
        NodeKind::File => {
            let selected = namespace.select(node.id)?;
            let mut reader = namespace.open(node.id, selected.version, 0)?;
            let mut file = file.ok_or(io::ErrorKind::InvalidInput)?;
            let n = io::copy(&mut reader, &mut file)?;
            if let ContentSize::Known(size) = selected.size
                && n != size
            {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            #[cfg(unix)]
            if let Some(bits) = selected.exec_bits {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = file.metadata()?.permissions();
                permissions.set_mode(permissions.mode() | u32::from(bits & 0o111));
                file.set_permissions(permissions)?;
            }
        }
        NodeKind::Symlink(target) => {
            if !oink_foundation::content::safe_symlink_target(target, depth + 1) {
                return Err(io::ErrorKind::InvalidData.into());
            }
            drop(file);
            std::fs::remove_file(path)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, path)?;
            #[cfg(windows)]
            return Err(io::ErrorKind::Unsupported.into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::clipboard::ClipPlatform;
    fn inventory(formats: &[(&str, Option<StandardFormat>)]) -> Inventory {
        Inventory {
            id: oink_foundation::content::OfferId {
                clip: oink_foundation::content::ClipId {
                    origin: oink_foundation::DeviceId([0; 32]),
                    seq: 1,
                },
                generation: 1,
            },
            platform: ClipPlatform::Mac,
            roots: vec![],
            items: vec![ContentItem {
                id: ItemId(1),
                file: None,
                virtual_files: false,
                representations: formats
                    .iter()
                    .enumerate()
                    .map(|(n, (name, standard))| Representation {
                        id: RepresentationId(n as u32 + 1),
                        format: NativeFormat::Mac((*name).into()),
                        medium: Medium::Bytes,
                        aspect: 1,
                        index: -1,
                        standard: *standard,
                        data: RepresentationData::Reader {
                            size: ContentSize::Unknown,
                        },
                    })
                    .collect(),
            }],
        }
    }
    #[test]
    fn image_beats_rich_text_and_keeps_original_encoding() {
        let i = inventory(&[
            ("public.rtf", Some(StandardFormat::Rtf)),
            ("public.tiff", Some(StandardFormat::Png)),
            ("public.jpeg", None),
            ("public.png", Some(StandardFormat::Png)),
        ]);
        assert_eq!(
            select(&i),
            Some(Selection::Bytes {
                representation: RepresentationId(3),
                extension: "jpg",
                kind: Kind::Image,
                translate: false
            })
        );
    }
    #[test]
    fn bitmap_is_translated_to_png() {
        assert_eq!(
            select(&inventory(&[("public.tiff", Some(StandardFormat::Png))])),
            Some(Selection::Bytes {
                representation: RepresentationId(1),
                extension: "png",
                kind: Kind::Image,
                translate: true
            })
        );
    }
    #[test]
    fn rtf_beats_html_and_text() {
        assert_eq!(
            select(&inventory(&[
                ("public.utf8-plain-text", Some(StandardFormat::Text)),
                ("public.html", Some(StandardFormat::Html)),
                ("public.rtf", Some(StandardFormat::Rtf))
            ])),
            Some(Selection::Bytes {
                representation: RepresentationId(3),
                extension: "rtf",
                kind: Kind::RichText,
                translate: true
            })
        );
    }
    #[test]
    fn html_without_rtf_preserves_remote_image_urls() {
        assert_eq!(
            select(&inventory(&[("public.html", Some(StandardFormat::Html))])),
            Some(Selection::Bytes {
                representation: RepresentationId(1),
                extension: "html",
                kind: Kind::RichText,
                translate: true
            })
        );
    }
    #[test]
    fn empty_and_opaque_are_unsaveable() {
        assert_eq!(select(&inventory(&[("custom", None)])), None);
    }
    #[test]
    fn line_endings_are_idempotent_and_utf8_without_bom() {
        assert_eq!(text_bytes("汉\r\na\rb\nc", false), "汉\na\nb\nc".as_bytes());
        assert_eq!(text_bytes("汉\r\na\rb\nc", true), "汉\r\na\r\nb\r\nc".as_bytes());
    }
    #[test]
    fn filenames_follow_each_ui_language() {
        for (language, label) in [
            ("zh-CN", "粘贴的图片"),
            ("en", "Pasted image"),
            ("ja", "貼り付けた画像"),
            ("ko", "붙여넣은 이미지"),
        ] {
            assert_eq!(
                stem(Kind::Image, language, "2026-11-06 09.30.12"),
                format!("{label} 2026-11-06 09.30.12")
            );
        }
    }
    struct Fixture {
        inventory: Inventory,
        bytes: Vec<u8>,
        files: Option<std::sync::Arc<dyn crate::content::namespace::Namespace>>,
    }
    impl crate::content::Translator for Fixture {
        fn standard(&self, rep: &Representation, bytes: &[u8]) -> io::Result<Vec<crate::ClipData>> {
            crate::content::decode_standard(rep, bytes)
        }
    }
    impl Provider for Fixture {
        fn inventory(&self) -> Inventory {
            self.inventory.clone()
        }
        fn open(&self, _: RepresentationId) -> io::Result<crate::FileReader> {
            Ok(Box::new(io::Cursor::new(self.bytes.clone())))
        }
        fn translator(&self) -> std::sync::Arc<dyn crate::content::Translator> {
            std::sync::Arc::new(Fixture {
                inventory: self.inventory.clone(),
                bytes: vec![],
                files: None,
            })
        }
        fn namespace(&self) -> Option<std::sync::Arc<dyn crate::content::namespace::Namespace>> {
            self.files.clone()
        }
    }
    #[test]
    fn saves_original_bytes_and_suffixes_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let data = vec![0xff, 0xd8, 0x12, 0x13, 0xff, 0xd9];
        let f = Fixture {
            inventory: inventory(&[("public.jpeg", None)]),
            bytes: data.clone(),
            files: None,
        };
        for (n, suffix) in [(1, ""), (2, " (2)"), (3, " (3)")] {
            let paths = save(&f, dir.path(), "zh-Hans", "2026-11-06 09.30.12", false).unwrap();
            assert_eq!(
                paths[0].file_name().unwrap().to_str().unwrap(),
                format!("粘贴的图片 2026-11-06 09.30.12{suffix}.jpg")
            );
            assert_eq!(std::fs::read(&paths[0]).unwrap(), data);
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), n);
        }
    }
    #[test]
    fn saves_html_without_fetching_images_and_text_as_native_newlines() {
        let dir = tempfile::tempdir().unwrap();
        for (format, standard, data, expected, windows) in [
            (
                "public.html",
                StandardFormat::Html,
                "<img src=\"https://invalid.example/a.png\">",
                "<img src=\"https://invalid.example/a.png\">",
                false,
            ),
            (
                "public.utf8-plain-text",
                StandardFormat::Text,
                "汉\r\nx\ny",
                "汉\nx\ny",
                false,
            ),
            (
                "public.utf8-plain-text",
                StandardFormat::Text,
                "汉\r\nx\ny",
                "汉\r\nx\r\ny",
                true,
            ),
            (
                "public.rtf",
                StandardFormat::Rtf,
                "{\\rtf1 example}",
                "{\\rtf1 example}",
                false,
            ),
        ] {
            let f = Fixture {
                inventory: inventory(&[(format, Some(standard))]),
                bytes: data.as_bytes().to_vec(),
                files: None,
            };
            let path = save(&f, dir.path(), "en", "2026-11-06 09.30.12", windows)
                .unwrap()
                .remove(0);
            assert_eq!(std::fs::read(path).unwrap(), expected.as_bytes());
        }
    }
    #[test]
    fn files_beat_images_and_copy_a_folder_without_overwriting() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let root = source.path().join("folder");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), b"file").unwrap();
        let files = crate::content::namespace::Local::new(vec![(NodeId(1), root)]).unwrap();
        let mut inventory = inventory(&[("public.png", Some(StandardFormat::Png))]);
        use crate::content::namespace::Namespace;
        inventory.roots = files.roots();
        assert_eq!(select(&inventory), Some(Selection::Files));
        let f = Fixture {
            inventory,
            bytes: vec![],
            files: Some(files),
        };
        for name in ["folder", "folder (2)"] {
            let path = save(&f, dest.path(), "en", "time", false).unwrap().remove(0);
            assert_eq!(path.file_name().unwrap(), name);
            assert_eq!(std::fs::read(path.join("a.txt")).unwrap(), b"file");
        }
    }
    #[test]
    fn empty_menu_click_is_a_noop_without_reading_or_creating_files() {
        struct Empty(Inventory);
        impl Provider for Empty {
            fn inventory(&self) -> Inventory {
                self.0.clone()
            }
            fn open(&self, _: RepresentationId) -> io::Result<crate::FileReader> {
                panic!("empty click must not read data")
            }
            fn translator(&self) -> std::sync::Arc<dyn crate::content::Translator> {
                panic!("empty click must not translate")
            }
        }
        let directory = tempfile::tempdir().unwrap();
        for windows in [false, true] {
            for inventory in [inventory(&[]), inventory(&[("opaque", None)])] {
                let empty = Empty(inventory);
                assert!(
                    save(&empty, directory.path(), "en", "time", windows)
                        .unwrap()
                        .is_empty()
                );
                assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
                assert!(
                    save(&empty, &directory.path().join("gone"), "en", "time", windows)
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }
    #[test]
    fn empty_text_representation_click_does_not_create_an_empty_file() {
        let directory = tempfile::tempdir().unwrap();
        let empty = Fixture {
            inventory: inventory(&[("public.utf8-plain-text", Some(StandardFormat::Text))]),
            bytes: vec![],
            files: None,
        };
        for windows in [false, true] {
            assert!(
                save(&empty, directory.path(), "en", "time", windows)
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
            assert!(
                save(&empty, &directory.path().join("gone"), "en", "time", windows)
                    .unwrap()
                    .is_empty()
            );
        }
    }
}
