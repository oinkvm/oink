//! Non-file hover content for previews, and metadata-only file type icons.
use oink_foundation::clipboard::{ClipData, ClipPlatform, FileEntry};
use oink_foundation::content::{NodeKind, RepresentationData, StandardFormat};
use std::io::Read;

/// Native targets display source pixels at at most 128 logical pixels. PNG
/// resolution and Windows monitor DPI are independent of this presentation size.
pub(crate) fn image_geometry(image: &oink_foundation::dnd::DragImage) -> [f64; 4] {
    let ratio = (128. / image.width.max(image.height)).min(1.);
    [
        image.width * ratio,
        image.height * ratio,
        image.hotspot_x * ratio,
        image.hotspot_y * ratio,
    ]
}
#[cfg(target_os = "macos")]
pub(crate) fn fitted_image(image: &oink_foundation::dnd::DragImage) -> oink_foundation::dnd::DragImage {
    let [width, height, hotspot_x, hotspot_y] = image_geometry(image);
    oink_foundation::dnd::DragImage {
        png: image.png.clone(),
        width,
        height,
        hotspot_x,
        hotspot_y,
    }
}

/// Read only the first non-file item's visual representations, before the
/// native window/Down critical section. Hover never authorizes native Drop.
/// These bytes are artwork inputs, not extra formats published by the writer.
pub(crate) fn content(
    native: &crate::NativeSpec,
    target: ClipPlatform,
    translator: &dyn crate::content::Translator,
) -> Vec<ClipData> {
    if !native.inventory.roots.is_empty() || !native.files.is_empty() {
        return Vec::new();
    }
    let Some(item) = native.inventory.items.iter().find(|item| item.file.is_none()) else {
        return Vec::new();
    };
    // Source image thumbnails travel separately. Never make native Begin wait
    // on a full image representation or a slow image provider for hover artwork.
    if item
        .representations
        .iter()
        .any(|r| r.standard == Some(StandardFormat::Png) && !matches!(r.data, RepresentationData::Inline(_)))
    {
        return Vec::new();
    }
    // Optional artwork has a short shared budget, independent of the
    // delivery watchdog. A slow provider falls back to its type icon.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let mut out = Vec::new();
    for format in [StandardFormat::Png, StandardFormat::Text, StandardFormat::Rtf] {
        let limit = if format == StandardFormat::Png {
            8 * 1024 * 1024
        } else {
            256 * 1024
        };
        for rep in item.representations.iter().filter(|rep| rep.standard == Some(format)) {
            if rep
                .data
                .size()
                .is_some_and(|size| matches!(size, oink_foundation::content::ContentSize::Known(size) if size > limit))
                || matches!(
                    rep.data,
                    RepresentationData::File(_) | RepresentationData::Unavailable(_)
                )
            {
                continue;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return out;
            }
            let reader = if native.inventory.platform == target
                && let RepresentationData::Inline(bytes) = &rep.data
            {
                Ok(Box::new(std::io::Cursor::new(bytes.clone())) as crate::FileReader)
            } else {
                native.contents.open_preview(rep.id, remaining)
            };
            let Ok(reader) = reader else { continue };
            let mut bytes = Vec::new();
            if reader.take(limit + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > limit {
                continue;
            }
            let decoded = if native.inventory.platform == target {
                translator.standard(rep, &bytes)
            } else {
                crate::content::decode_standard(rep, &bytes)
            };
            let Ok(decoded) = decoded else { continue };
            let before = out.len();
            out.extend(decoded.into_iter().filter(|data| {
                matches!(
                    (format, data),
                    (StandardFormat::Png, ClipData::Png(_))
                        | (StandardFormat::Text, ClipData::Text(_))
                        | (StandardFormat::Rtf, ClipData::Rtf(_))
                )
            }));
            if out.len() != before {
                break;
            }
        }
        if format == StandardFormat::Png && !out.is_empty() {
            break;
        }
    }
    out
}

/// Type icons are the fallback when a source image or hover preview is absent.
/// File roots always win over their accompanying text/image representations.
pub(crate) fn metadata(items: &[ClipData]) -> Option<Vec<FileEntry>> {
    if let Some(roots) = items.iter().find_map(|item| match item {
        ClipData::FileRoots(roots) if !roots.is_empty() => Some(roots),
        _ => None,
    }) {
        return Some(
            roots
                .iter()
                .map(|root| entry(root.name.clone(), root.kind == NodeKind::Directory))
                .collect(),
        );
    }
    let native: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            ClipData::Native { item, .. } => Some(item),
            _ => None,
        })
        .collect();
    (!native.is_empty()).then(|| {
        native
            .iter()
            .map(|item| {
                let ext = if item
                    .representations
                    .iter()
                    .any(|r| r.standard == Some(StandardFormat::Png))
                {
                    "png"
                } else if item.representations.iter().any(|r| {
                    matches!(
                        r.standard,
                        Some(StandardFormat::Text | StandardFormat::Html | StandardFormat::Rtf)
                    )
                }) {
                    "txt"
                } else {
                    "data"
                };
                entry(format!("drag.{ext}"), false)
            })
            .collect()
    })
}
fn entry(path: String, is_dir: bool) -> FileEntry {
    FileEntry {
        path,
        is_dir,
        size: 0,
        modified_ms: None,
        local_path: None,
    }
}

/// Synthetic shared 3x2 PNG: red/green/white above blue/yellow/transparent.
#[cfg(all(test, debug_assertions))]
pub(crate) fn asymmetric_png() -> Vec<u8> {
    vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 3, 0, 0, 0, 2, 8, 6, 0, 0, 0, 157, 116,
        102, 26, 0, 0, 0, 26, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 208, 192, 240, 159, 225, 63, 8, 48, 48, 128,
        105, 6, 16, 131, 1, 0, 174, 118, 12, 117, 8, 218, 200, 75, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use oink_foundation::{clipboard::ClipPlatform, content::*};
    #[test]
    fn thumbnail_presentation_is_128_logical_pixels_with_proportional_hotspot() {
        let image = oink_foundation::dnd::DragImage {
            png: asymmetric_png().into(),
            width: 256.,
            height: 192.,
            hotspot_x: 128.,
            hotspot_y: 96.,
        };
        let geometry = image_geometry(&image);
        assert_eq!(geometry, [128., 96., 64., 48.]);
        // The same logical size is used by AppKit. The Shell bitmap uses physical pixels once.
        for dpi in [96., 144., 192.] {
            assert_eq!(
                (geometry[0] * dpi / 96., geometry[1] * dpi / 96.),
                (128. * dpi / 96., 96. * dpi / 96.)
            );
        }
    }
    #[test]
    fn held_metadata_provides_icons_without_reading_payload_or_using_companions() {
        let native = ClipData::Native {
            platform: ClipPlatform::Mac,
            item: ContentItem {
                virtual_files: false,
                id: ItemId(0),
                file: Some(NodeId(1)),
                representations: vec![Representation {
                    id: RepresentationId(0),
                    format: NativeFormat::Mac("public.png".into()),
                    medium: Medium::Bytes,
                    aspect: 1,
                    index: -1,
                    standard: Some(StandardFormat::Png),
                    data: RepresentationData::Reader {
                        size: ContentSize::Unknown,
                    },
                }],
            },
        };
        assert_eq!(metadata(std::slice::from_ref(&native)).unwrap()[0].path, "drag.png");
        let roots = ClipData::FileRoots(vec![FileNode {
            exec_bits: None,
            id: NodeId(1),
            parent: None,
            name: "example.txt".into(),
            kind: NodeKind::File,
            size: ContentSize::Unknown,
            version: 1,
            modified_ms: None,
        }]);
        let preview = metadata(&[native, roots]).unwrap();
        assert_eq!(preview.len(), 1);
        assert_eq!(preview[0].path, "example.txt");
        assert!(preview[0].local_path.is_none());
        assert!(metadata(&[ClipData::Text("legacy".into())]).is_none());
    }
}
