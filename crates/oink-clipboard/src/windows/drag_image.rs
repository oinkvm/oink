//! Content previews for the Shell drag-image helper. Runs on the drag STA.
use super::image;
use oink_foundation::clipboard::ClipData;
use windows::Win32::{
    Foundation::{COLORREF, POINT, RECT, SIZE},
    Graphics::Gdi::*,
    Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL},
    System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IDataObject},
    UI::{
        Shell::{
            CLSID_DragDropHelper, IDragSourceHelper, SHDRAGIMAGE, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON,
            SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW,
        },
        WindowsAndMessaging::{DI_NORMAL, DestroyIcon, DrawIconEx},
    },
};
use windows::core::{PCWSTR, w};

struct Bitmap {
    handle: HBITMAP,
    bits: *mut u8,
    width: i32,
    height: i32,
}
impl Drop for Bitmap {
    fn drop(&mut self) {
        // SAFETY: owned, deselected bitmap (or null after transfer to Shell).
        unsafe {
            let _ = DeleteObject(self.handle.into());
        }
    }
}
impl Bitmap {
    fn new(width: i32, height: i32) -> Result<Self, String> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        // SAFETY: bounded positive dimensions, output pointer owned by returned bitmap.
        let handle =
            unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) }.map_err(|e| e.to_string())?;
        let mut bitmap = Self {
            handle,
            bits: bits.cast(),
            width,
            height,
        };
        bitmap.pixels().fill(0);
        Ok(bitmap)
    }
    fn pixels(&mut self) -> &mut [u8] {
        // SAFETY: 32-bit top-down DIB owns width*height*4 bytes until Drop.
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.width * self.height * 4) as usize) }
    }
    fn draw(&self, draw: impl FnOnce(HDC)) -> Result<(), String> {
        // SAFETY: temporary memory DC; restore selected object before destroying it.
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return Err("CreateCompatibleDC failed".into());
            }
            let old = SelectObject(dc, self.handle.into());
            draw(dc);
            let _ = GdiFlush();
            SelectObject(dc, old);
            let _ = DeleteDC(dc);
        }
        Ok(())
    }
}

fn size(width: u32, height: u32) -> (i32, i32) {
    let scale = (160. / f64::from(width.max(1)))
        .min(80. / f64::from(height.max(1)))
        .min(1.);
    (
        (f64::from(width) * scale).round().max(1.) as i32,
        (f64::from(height) * scale).round().max(1.) as i32,
    )
}
fn text_preview(text: &str) -> String {
    text.chars()
        .take(80)
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect()
}
fn preview(items: &[ClipData], scale: f64) -> Result<Option<Bitmap>, String> {
    let has_content = items
        .iter()
        .any(|item| matches!(item, ClipData::Text(_) | ClipData::Png(_)));
    let has_files = items
        .iter()
        .any(|item| matches!(item, ClipData::FileRoots(roots) if !roots.is_empty()));
    let metadata = (has_files || !has_content)
        .then(|| crate::dnd::preview::metadata(items))
        .flatten();
    if let Some(entries) = items
        .iter()
        .find_map(|i| if let ClipData::Files(v) = i { Some(v) } else { None })
        .or(metadata.as_ref())
    {
        let tops: Vec<_> = entries.iter().filter(|e| !e.path.contains('/')).take(9).collect();
        if tops.is_empty() {
            return Ok(None);
        }
        let icon_side = (48. * scale).round() as i32;
        let offset = (4. * scale).round() as i32;
        let side = icon_side + (tops.len() as i32 - 1) * offset;
        let mut bitmap = Bitmap::new(side, side)?;
        bitmap.draw(|dc| {
            for (n, entry) in tops.iter().enumerate().rev() {
                let name: Vec<u16> = entry.path.encode_utf16().chain(Some(0)).collect();
                let mut info = SHFILEINFOW::default();
                // SAFETY: USEFILEATTRIBUTES requests a type icon without touching the remote file.
                unsafe {
                    if SHGetFileInfoW(
                        PCWSTR(name.as_ptr()),
                        if entry.is_dir {
                            FILE_ATTRIBUTE_DIRECTORY
                        } else {
                            FILE_ATTRIBUTE_NORMAL
                        },
                        Some(&mut info),
                        size_of::<SHFILEINFOW>() as u32,
                        SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
                    ) != 0
                    {
                        let _ = DrawIconEx(
                            dc,
                            n as i32 * offset,
                            n as i32 * offset,
                            info.hIcon,
                            icon_side,
                            icon_side,
                            0,
                            None,
                            DI_NORMAL,
                        );
                        let _ = DestroyIcon(info.hIcon);
                    }
                }
            }
        })?;
        // Shell icon compositing produces premultiplied BGRA; helper requires straight alpha.
        for px in bitmap.pixels().as_chunks_mut::<4>().0 {
            if px[3] > 0 {
                for c in 0..3 {
                    px[c] = (u32::from(px[c]) * 255 / u32::from(px[3])).min(255) as u8;
                }
            }
        }
        return Ok(Some(bitmap));
    }
    if let Some(png) = items
        .iter()
        .find_map(|i| if let ClipData::Png(v) = i { Some(v) } else { None })
    {
        let rgba = image::decode_png(png).map_err(|e| e.to_string())?;
        let (w, h) = size(rgba.width, rgba.height);
        let (w, h) = (
            (f64::from(w) * scale).round() as i32,
            (f64::from(h) * scale).round() as i32,
        );
        let mut bitmap = Bitmap::new(w, h)?;
        for y in 0..h as usize {
            for x in 0..w as usize {
                let src = ((y * rgba.height as usize / h as usize) * rgba.width as usize
                    + x * rgba.width as usize / w as usize)
                    * 4;
                let dst = (y * w as usize + x) * 4;
                let p = &rgba.data[src..src + 4];
                bitmap.pixels()[dst..dst + 4].copy_from_slice(&[p[2], p[1], p[0], p[3]]);
            }
        }
        return Ok(Some(bitmap));
    }
    if let Some(text) = items
        .iter()
        .find_map(|i| if let ClipData::Text(v) = i { Some(v) } else { None })
    {
        let mut bitmap = Bitmap::new((160. * scale).round() as i32, (32. * scale).round() as i32)?;
        bitmap.pixels().fill(255);
        let mut text: Vec<u16> = text_preview(text).encode_utf16().collect();
        bitmap.draw(|dc| {
            // SAFETY: bounded text in the selected DIB; font is deselected before deletion.
            unsafe {
                let font = CreateFontW(
                    (-16. * scale).round() as i32,
                    0,
                    0,
                    0,
                    400,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    NONANTIALIASED_QUALITY,
                    0,
                    w!("Segoe UI"),
                );
                let old = SelectObject(dc, font.into());
                SetTextColor(dc, COLORREF(0));
                SetBkMode(dc, TRANSPARENT);
                DrawTextW(
                    dc,
                    &mut text,
                    &mut RECT {
                        left: (4. * scale).round() as i32,
                        top: (4. * scale).round() as i32,
                        right: (156. * scale).round() as i32,
                        bottom: (28. * scale).round() as i32,
                    },
                    DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
                );
                SelectObject(dc, old);
                let _ = DeleteObject(font.into());
            }
        })?;
        for px in bitmap.pixels().as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
        return Ok(Some(bitmap));
    }
    Ok(None)
}

/// A foreign helper caches its own window even after DragImageBits changes.
/// Hide is insufficient because its next DragOver shows it again. Retire only
/// the previous Shell image window when this drag publishes a replacement;
/// the helper itself remains alive and accepts DragOver/Leave/reentry normally.
/// Verified across separate processes on Windows 11. No input or target-window
/// messages are sent: only the published SysDragImage receives WM_CLOSE.
pub(super) fn retire_window(raw: u32) {
    use windows::Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        UI::WindowsAndMessaging::{GetClassNameW, PostMessageW, WM_CLOSE},
    };
    let window = HWND(raw as usize as _);
    // SAFETY: read a bounded class name and asynchronously close only a Shell
    // image window identified by this IDataObject's documented DragWindow DWORD.
    unsafe {
        let mut class = [0u16; 32];
        let count = GetClassNameW(window, &mut class).max(0) as usize;
        if "SysDragImage".encode_utf16().eq(class[..count].iter().copied()) {
            let _ = PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

pub(super) fn drag_window(data: &IDataObject) -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::System::{
        Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL},
        DataExchange::RegisterClipboardFormatW,
        Memory::{GlobalLock, GlobalSize, GlobalUnlock},
        Ole::ReleaseStgMedium,
    };
    // SAFETY: read only the documented DWORD HWND from a bounded local medium,
    // then unlock/release it. No content provider is involved in this private format.
    unsafe {
        let mut medium = data
            .GetData(&FORMATETC {
                cfFormat: RegisterClipboardFormatW(w!("DragWindow")) as u16,
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
                ..Default::default()
            })
            .ok()?;
        let result = (|| {
            if medium.tymed != TYMED_HGLOBAL.0 as u32 || GlobalSize(medium.u.hGlobal) < 4 {
                return None;
            }
            let bytes = GlobalLock(medium.u.hGlobal);
            if bytes.is_null() {
                return None;
            }
            let raw = std::ptr::read_unaligned(bytes.cast::<u32>());
            let _ = GlobalUnlock(medium.u.hGlobal);
            (raw != 0).then_some(windows::Win32::Foundation::HWND(raw as usize as _))
        })();
        ReleaseStgMedium(&mut medium);
        result
    }
}

pub(super) fn initialize(
    data: &IDataObject,
    items: &[ClipData],
    source: Option<&oink_foundation::dnd::DragImage>,
) -> Result<Option<IDragSourceHelper>, String> {
    initialize_prepared(data, items, source, None, None)
}
pub(super) fn initialize_prepared(
    data: &IDataObject,
    items: &[ClipData],
    source: Option<&oink_foundation::dnd::DragImage>,
    pixels: Option<&image::Rgba>,
    existing: Option<IDragSourceHelper>,
) -> Result<Option<IDragSourceHelper>, String> {
    // The cursor's target window determines the monitor, not the foreground
    // application (which may remain on another monitor during a remote drag).
    // SAFETY: read-only window/DPI queries on the per-monitor-aware drag STA.
    let scale = unsafe {
        let mut point = POINT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point);
        let window = windows::Win32::UI::WindowsAndMessaging::WindowFromPoint(point);
        windows::Win32::UI::HiDpi::GetDpiForWindow(window).max(96) as f64 / 96.
    };
    let mut bitmap = if let Some(source) = source.filter(|s| s.valid()) {
        let decoded;
        let rgba = if let Some(pixels) = pixels {
            pixels
        } else {
            decoded = image::decode_png(&source.png).map_err(|e| e.to_string())?;
            &decoded
        };
        let [logical_width, logical_height, _, _] = crate::dnd::preview::image_geometry(source);
        let width = (logical_width * scale).round().max(1.) as i32;
        let height = (logical_height * scale).round().max(1.) as i32;
        let mut bitmap = Bitmap::new(width, height)?;
        for y in 0..height as usize {
            for x in 0..width as usize {
                let src = ((y * rgba.height as usize / height as usize) * rgba.width as usize
                    + x * rgba.width as usize / width as usize)
                    * 4;
                let dst = (y * width as usize + x) * 4;
                let p = &rgba.data[src..src + 4];
                bitmap.pixels()[dst..dst + 4].copy_from_slice(&[p[2], p[1], p[0], p[3]]);
            }
        }
        bitmap
    } else {
        let Some(bitmap) = preview(items, scale)? else {
            return Ok(None);
        };
        bitmap
    };
    // SAFETY: caller is the OleInitialize'd drag STA; data remains alive throughout DoDragDrop.
    unsafe {
        let helper: IDragSourceHelper = match existing {
            Some(helper) => helper,
            None => CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER).map_err(|e| e.to_string())?,
        };
        let image = SHDRAGIMAGE {
            sizeDragImage: SIZE {
                cx: bitmap.width,
                cy: bitmap.height,
            },
            ptOffset: source.map_or(POINT { x: 0, y: 0 }, |s| POINT {
                x: (s.hotspot_x / s.width * f64::from(bitmap.width)).round() as i32,
                y: (s.hotspot_y / s.height * f64::from(bitmap.height)).round() as i32,
            }),
            hbmpDragImage: bitmap.handle,
            crColorKey: COLORREF(u32::MAX),
        };
        helper.InitializeFromBitmap(&image, data).map_err(|e| e.to_string())?;
        // Successful InitializeFromBitmap takes bitmap ownership.
        bitmap.handle = HBITMAP::default();
        Ok(Some(helper))
    }
}

/// Input cap: an 8K screen fits, but bogus layouts cannot allocate arbitrarily.
/// At most 128 MiB of BGRA, independently capped at 8192 pixels per side.
const MAX_SOURCE_SIDE: i32 = 8192;
const MAX_SOURCE_PIXELS: usize = 32 * 1024 * 1024;
const MAX_SOURCE_BYTES: usize = 24 + MAX_SOURCE_PIXELS * 4;

fn source_size(width: u32, height: u32, limit: u32) -> (u32, u32) {
    let scale = (f64::from(limit) / f64::from(width.max(height))).min(1.);
    (
        (f64::from(width) * scale).round().max(1.) as u32,
        (f64::from(height) * scale).round().max(1.) as u32,
    )
}

/// Encode straight-alpha PNG only after filtering the premultiplied channels.
/// Keep the old integer conversion for images that do not need resizing.
fn source_png(premultiplied: &::image::RgbaImage) -> Option<Vec<u8>> {
    let mut rgba = premultiplied.as_raw().clone();
    for p in rgba.as_chunks_mut::<4>().0 {
        for c in 0..3 {
            p[c] = if p[3] == 0 {
                0
            } else {
                (u32::from(p[c]) * 255 / u32::from(p[3])).min(255) as u8
            };
        }
    }
    image::encode_png(&image::Rgba {
        pixels_per_meter: None,
        width: premultiplied.width(),
        height: premultiplied.height(),
        data: rgba,
    })
    .ok()
}

/// Explorer serializes a 24-byte SHDRAGIMAGE header even in its x64 process.
/// This is private Shell data: only the observed, absolutely bounded layout is accepted.
/// Called on the drag STA, never on a hook or while holding shared drag state.
fn parse_source(bytes: &[u8], dpi: u32) -> Option<oink_foundation::dnd::DragImage> {
    use ::image::imageops::{FilterType, resize};
    use oink_foundation::dnd::DragImage;
    let n = |offset| Some(i32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?));
    let (w, h, x, y) = (n(0)?, n(4)?, n(8)?, n(12)?);
    // Only the observed alpha format; do not guess color-key semantics.
    if !(1..=MAX_SOURCE_SIDE).contains(&w)
        || !(1..=MAX_SOURCE_SIDE).contains(&h)
        || !(0..=w).contains(&x)
        || !(0..=h).contains(&y)
        || n(20)? != -1
    {
        return None;
    }
    let pixels = (w as usize).checked_mul(h as usize)?;
    if pixels > MAX_SOURCE_PIXELS || bytes.len() != 24_usize.checked_add(pixels.checked_mul(4)?)? {
        return None;
    }
    if !bytes[24..].as_chunks::<4>().0.iter().any(|p| p[3] != 0) {
        return None;
    }
    // DragImageBits uses the Shell's bottom-up, DWORD-aligned 32-bit DIB
    // storage, even when InitializeFromBitmap received a top-down bitmap.
    // Its alpha is already premultiplied (including the Shell's opacity).
    let stride = (w as usize).checked_mul(4)?;
    let mut rgba: Vec<_> = bytes[24..].chunks_exact(stride).rev().flatten().copied().collect();
    for p in rgba.as_chunks_mut::<4>().0 {
        p.swap(0, 2); // BGRA -> RGBA; filtering still happens premultiplied.
        if p[3] == 0 {
            p.fill(0);
        }
    }
    let original = ::image::RgbaImage::from_raw(w as u32, h as u32, rgba)?;
    let (width, height) = source_size(w as u32, h as u32, DragImage::MAX_PIXELS);
    // Triangle widens its sampling support for downscaling and avoids ringing.
    // Filtering before unpremultiplication prevents dark/colored alpha fringes.
    let preview = if original.dimensions() == (width, height) {
        original
    } else {
        let resized = resize(&original, width, height, FilterType::Triangle);
        drop(original);
        resized
    };
    let png = source_png(&preview)?;
    // One dimensional downscale only. Over-budget PNGs use the target fallback.
    if png.len() > DragImage::MAX_BYTES {
        return None;
    }
    let dpi_scale = f64::from(dpi.max(96)) / 96.;
    // The PNG pixel budget controls sampling resolution, not its displayed
    // point size. Preserve the original source DIP geometry and hotspot;
    // apply only the separate shared 512-point geometry bound if necessary.
    let geometry_scale = (512. / (f64::from(w.max(h)) / dpi_scale)).min(1.) / dpi_scale;
    let image = DragImage {
        png: png.into(),
        width: f64::from(w) * geometry_scale,
        height: f64::from(h) * geometry_scale,
        hotspot_x: f64::from(x) * geometry_scale,
        hotspot_y: f64::from(y) * geometry_scale,
    };
    if !image.valid() {
        return None;
    }
    tracing::debug!(
        dnd_preview_stage = 1_u64,
        dnd_preview_source_w = w,
        dnd_preview_source_h = h,
        dnd_preview_png_w = preview.width(),
        dnd_preview_png_h = preview.height(),
        dnd_preview_width = image.width,
        dnd_preview_height = image.height,
        dnd_preview_hotspot_x = image.hotspot_x,
        dnd_preview_hotspot_y = image.hotspot_y,
        dnd_preview_scale = dpi_scale,
        "source drag artwork geometry"
    );
    Some(image)
}

pub(super) fn capture(data: &IDataObject) -> Option<oink_foundation::dnd::DragImage> {
    use windows::Win32::System::{
        Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL},
        DataExchange::RegisterClipboardFormatW,
        Memory::{GlobalLock, GlobalSize, GlobalUnlock},
        Ole::ReleaseStgMedium,
    };
    use windows::Win32::UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::GetForegroundWindow};
    // SAFETY: the COM medium is released on every path; read only a bounded, locked HGLOBAL.
    unsafe {
        let cf = RegisterClipboardFormatW(w!("DragImageBits"));
        let f = FORMATETC {
            cfFormat: cf as u16,
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
            ..Default::default()
        };
        let mut medium = data.GetData(&f).ok()?;
        let result = (|| {
            if medium.tymed != TYMED_HGLOBAL.0 as u32 {
                return None;
            }
            let h = medium.u.hGlobal;
            let len = GlobalSize(h);
            if !(24..=MAX_SOURCE_BYTES).contains(&len) {
                return None;
            }
            let ptr = GlobalLock(h);
            if ptr.is_null() {
                return None;
            }
            // Parse while locked: avoid another full-size copy of the bounded input.
            let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), len);
            let result = parse_source(bytes, GetDpiForWindow(GetForegroundWindow()));
            let _ = GlobalUnlock(h);
            result
        })();
        ReleaseStgMedium(&mut medium);
        result
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    fn assert_native_artwork(items: &[ClipData]) {
        let mut bitmap = preview(items, 1.).unwrap().expect("native offer needs visible artwork");
        assert!(
            bitmap.pixels().as_chunks::<4>().0.iter().any(|pixel| pixel[3] != 0),
            "native icon has no alpha: nonzero bytes={}, dimensions={}x{}",
            bitmap.pixels().iter().filter(|byte| **byte != 0).count(),
            bitmap.width,
            bitmap.height
        );
    }

    #[test]
    fn native_file_and_nonfile_offers_draw_artwork_without_source_bitmap_or_payload() {
        // The Shell image cache is process-global. Other native tests own COM
        // apartments, so exercise the serial drag STA in a private process.
        const CHILD: &str = "OINK_TEST_NATIVE_ARTWORK";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "windows::drag_image::tests::native_file_and_nonfile_offers_draw_artwork_without_source_bitmap_or_payload", "--nocapture"])
                .env(CHILD, "1")
                .status().unwrap();
            assert!(status.success(), "private Shell artwork test failed");
            return;
        }
        use oink_foundation::{clipboard::ClipPlatform, content::*};
        use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
        // SHGetFileInfo requires COM initialization, just like the production
        // drag STA. The unit-test harness does not initialize it for us.
        // Exercise both cases on that same STA. Simultaneous independent Shell
        // apartments yielded an empty icon in the test harness, whereas the
        // product owns one serial drag STA (windows-native-concurrent.log).
        // SAFETY: this test thread owns its apartment until the guard drops.
        unsafe { OleInitialize(None).unwrap() };
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                // SAFETY: balances this thread's successful initialization.
                unsafe { OleUninitialize() };
            }
        }
        let _apartment = Apartment;
        let items = [ClipData::Native {
            platform: ClipPlatform::Mac,
            item: ContentItem {
                virtual_files: false,
                id: ItemId(0),
                file: None,
                representations: vec![Representation {
                    id: RepresentationId(0),
                    format: NativeFormat::Mac("public.utf8-plain-text".into()),
                    medium: Medium::Bytes,
                    aspect: 1,
                    index: -1,
                    standard: Some(StandardFormat::Text),
                    data: RepresentationData::Reader {
                        size: ContentSize::Unknown,
                    },
                }],
            },
        }];
        assert_native_artwork(&items);
        // Mirror of Mac -> Windows: decoded native hover text must reach the
        // old text renderer rather than being preempted by metadata's .txt icon.
        let text = ClipData::Text("Native words".into());
        let mut native_text = preview(&[items[0].clone(), text.clone()], 1.).unwrap().unwrap();
        let mut legacy_text = preview(&[text], 1.).unwrap().unwrap();
        assert_eq!((native_text.width, native_text.height), (160, 32));
        assert_eq!(native_text.pixels(), legacy_text.pixels());
        let png = ClipData::Png(crate::dnd::preview::asymmetric_png());
        let mut native_image = preview(&[items[0].clone(), png.clone()], 2.).unwrap().unwrap();
        let mut legacy_image = preview(&[png], 2.).unwrap().unwrap();
        assert_eq!((native_image.width, native_image.height), (6, 4));
        assert_eq!(native_image.pixels(), legacy_image.pixels());
        let items = [ClipData::FileRoots(vec![FileNode {
            exec_bits: None,
            id: NodeId(1),
            parent: None,
            name: "example.txt".into(),
            kind: NodeKind::File,
            size: ContentSize::Unknown,
            version: 1,
            modified_ms: None,
        }])];
        assert_native_artwork(&items);
    }

    use super::*;

    #[test]
    fn shell_serialized_dib_fixture_round_trips_asymmetric_rows() {
        use std::sync::Arc;
        use windows::Win32::System::{
            Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL},
            DataExchange::RegisterClipboardFormatW,
            Memory::{GlobalLock, GlobalSize, GlobalUnlock},
            Ole::{OleInitialize, OleUninitialize, ReleaseStgMedium},
        };
        struct NoFiles;
        impl crate::FileContents for NoFiles {
            fn open(&self, _: usize) -> std::io::Result<crate::FileReader> {
                panic!("no file reads")
            }
        }
        // This is an in-memory Shell helper fixture, not a desktop drag:
        // no clipboard publication, DoDragDrop, cursor, or injected events.
        // SAFETY: initialize OLE on this test thread; the guard below balances
        // every successful initialization on the same thread.
        unsafe { OleInitialize(None).unwrap() };
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                // SAFETY: balances this thread's successful OleInitialize.
                unsafe { OleUninitialize() };
            }
        }
        let _apartment = Apartment;
        let (data, _lease) = super::super::vfile::build(
            super::super::vfile::Spec {
                cloud: None,
                descriptors: super::super::files::descriptors(&[]),
                fixed: vec![],
                contents: Arc::new(NoFiles),
                cf_descriptor: 0,
                cf_contents: 0,
            },
            false,
        );
        let mut bitmap = Bitmap::new(3, 2).unwrap();
        // Straight-alpha top-down BGRA supplied to InitializeFromBitmap.
        bitmap.pixels().copy_from_slice(&[
            0, 0, 255, 128, 0, 255, 0, 255, 255, 255, 255, 255, 255, 0, 0, 255, 0, 255, 255, 255, 0, 0, 0, 0,
        ]);
        // SAFETY: OLE is initialized and the bitmap and data object are live;
        // bitmap ownership transfers on successful initialization. GetData
        // requests HGLOBAL; its locked bytes are copied within GlobalSize
        // before unlocking and releasing the medium.
        unsafe {
            let helper: IDragSourceHelper =
                CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER).unwrap();
            helper
                .InitializeFromBitmap(
                    &SHDRAGIMAGE {
                        sizeDragImage: SIZE { cx: 3, cy: 2 },
                        ptOffset: POINT { x: 1, y: 0 },
                        hbmpDragImage: bitmap.handle,
                        crColorKey: COLORREF(u32::MAX),
                    },
                    &data,
                )
                .unwrap();
            bitmap.handle = HBITMAP::default();
            let cf = RegisterClipboardFormatW(w!("DragImageBits"));
            let mut medium = data
                .GetData(&FORMATETC {
                    cfFormat: cf as u16,
                    dwAspect: DVASPECT_CONTENT.0,
                    lindex: -1,
                    tymed: TYMED_HGLOBAL.0 as u32,
                    ..Default::default()
                })
                .unwrap();
            let h = medium.u.hGlobal;
            let ptr = GlobalLock(h);
            assert!(!ptr.is_null());
            let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), GlobalSize(h)).to_vec();
            let _ = GlobalUnlock(h);
            ReleaseStgMedium(&mut medium);
            eprintln!("Shell fixture 3x2 BGRA rows: {:?}", &bytes[24..]);
            let source = parse_source(&bytes, 96).unwrap();
            eprintln!("SHELL_FIXTURE {}", serde_json::to_string(&source).unwrap());
            let rgba = image::decode_png(&source.png).unwrap();
            // The helper applies its own translucency before serialization.
            let opaque = bytes[27];
            let half = bytes[39];
            assert!(opaque > 0 && half > 0 && (2_u16 * u16::from(half)).abs_diff(u16::from(opaque)) <= 1);
            assert_eq!(
                rgba.data,
                [
                    255, 0, 0, half, 0, 255, 0, opaque, 255, 255, 255, opaque, 0, 0, 255, opaque, 255, 255, 0, opaque,
                    0, 0, 0, 0,
                ]
            );
        }
    }

    #[test]
    fn bottom_up_source_is_upright_with_straight_alpha() {
        // A real 32-bit DIB row has DWORD stride: three pixels = 12 bytes,
        // with the bottom row first. Red/green above blue/yellow is asymmetric
        // in both axes; half-alpha red also catches BGRA/alpha mistakes.
        let dib = [
            255, 0, 0, 255, 0, 255, 255, 255, 0, 0, 0, 0, 0, 0, 128, 128, 0, 255, 0, 255, 255, 255, 255, 255,
        ];
        let source = parse_source(&source_bytes(3, 2, 1, 0, &dib), 96).unwrap();
        let rgba = image::decode_png(&source.png).unwrap();
        assert_eq!(
            rgba.data,
            [
                255, 0, 0, 128, 0, 255, 0, 255, 255, 255, 255, 255, 0, 0, 255, 255, 255, 255, 0, 255, 0, 0, 0, 0,
            ]
        );
        assert_eq!(
            (source.width, source.height, source.hotspot_x, source.hotspot_y),
            (3., 2., 1., 0.)
        );
    }

    #[test]
    fn source_pixel_downscale_preserves_original_point_size_and_hotspot() {
        let source = parse_source(&source_bytes(768, 96, 96, 24, &vec![255; 768 * 96 * 4]), 192).unwrap();
        let rgba = image::decode_png(&source.png).unwrap();
        assert_eq!((rgba.width, rgba.height), (512, 64));
        // PNG compression/downsampling changes resolution, not point geometry.
        // A 2x Retina target must draw this 384x48 pt frame as 768x96 px.
        assert_eq!((source.width, source.height), (384., 48.));
        assert_eq!((source.hotspot_x, source.hotspot_y), (48., 12.));
    }

    fn source_bytes(width: i32, height: i32, x: i32, y: i32, bgra: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for n in [width, height, x, y, 0, -1] {
            bytes.extend_from_slice(&n.to_le_bytes());
        }
        bytes.extend_from_slice(bgra);
        bytes
    }

    #[test]
    fn oversized_source_preserves_aspect_hotspot_and_premultiplied_alpha() {
        let mut bgra = Vec::new();
        for _ in 0..512 {
            for x in 0..1024 {
                bgra.extend_from_slice(if x < 512 { &[0, 0, 128, 128] } else { &[255, 0, 0, 0] });
            }
        }
        let source = parse_source(&source_bytes(1024, 512, 384, 128, &bgra), 192).unwrap();
        assert!(source.valid());
        assert_eq!((source.width, source.height), (512., 256.));
        assert_eq!((source.hotspot_x, source.hotspot_y), (192., 64.));
        let rgba = image::decode_png(&source.png).unwrap();
        assert_eq!((rgba.width, rgba.height), (512, 256));
        let pixels = rgba.data.as_chunks::<4>().0;
        assert_eq!(pixels[0], [255, 0, 0, 128]);
        assert_eq!(pixels[511], [0, 0, 0, 0]);
        assert!(pixels.iter().any(|p| p[3] > 0 && p[3] < 128));
        for p in pixels.iter().filter(|p| p[3] > 0) {
            assert_eq!(&p[..3], &[255, 0, 0]); // No dark fringe or hidden blue bleed.
        }
    }

    fn noisy_bgra(width: u32, height: u32) -> Vec<u8> {
        let mut seed = 0x1234_5678_u32;
        let mut bgra = Vec::new();
        for _ in 0..width * height {
            for _ in 0..3 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                bgra.push(seed as u8);
            }
            bgra.push(255);
        }
        bgra
    }

    #[test]
    fn png_over_byte_budget_is_dropped_without_further_shrinking() {
        use oink_foundation::dnd::DragImage;
        let bgra = noisy_bgra(512, 512);
        let raw = ::image::RgbaImage::from_raw(512, 512, bgra.clone()).unwrap();
        assert!(source_png(&raw).unwrap().len() > DragImage::MAX_BYTES);
        assert!(parse_source(&source_bytes(512, 512, 256, 512, &bgra), 96).is_none());

        // Duplicate each pixel in both axes: a single 2:1 resize still has
        // enough entropy to exceed 512 KiB. It must be dropped, not shrunk again.
        let mut doubled = Vec::new();
        for row in bgra.as_chunks::<{ 512 * 4 }>().0 {
            let row: Vec<u8> = row
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| p.iter().chain(p))
                .copied()
                .collect();
            doubled.extend_from_slice(&row);
            doubled.extend_from_slice(&row);
        }
        let raw = ::image::RgbaImage::from_raw(1024, 1024, doubled.clone()).unwrap();
        let resized = ::image::imageops::resize(&raw, 512, 512, ::image::imageops::FilterType::Triangle);
        assert!(source_png(&resized).unwrap().len() > DragImage::MAX_BYTES);
        assert!(parse_source(&source_bytes(1024, 1024, 512, 1024, &doubled), 96).is_none());
    }

    #[test]
    fn source_over_old_byte_limit_is_kept_unchanged() {
        use oink_foundation::dnd::DragImage;
        let bgra = noisy_bgra(192, 192);
        let source = parse_source(&source_bytes(192, 192, 96, 178, &bgra), 192).unwrap();
        assert!(source.png.len() > 64 * 1024);
        assert!(source.png.len() <= DragImage::MAX_BYTES);
        assert_eq!((source.width, source.height), (96., 96.));
        assert_eq!((source.hotspot_x, source.hotspot_y), (48., 89.));
        let rgba = image::decode_png(&source.png).unwrap();
        assert_eq!((rgba.width, rgba.height), (192, 192));
        let expected: Vec<u8> = bgra
            .as_chunks::<{ 192 * 4 }>()
            .0
            .iter()
            .rev()
            .flatten()
            .copied()
            .collect::<Vec<_>>()
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[2], p[1], p[0], p[3]])
            .collect();
        assert_eq!(rgba.data, expected);
        assert_eq!(*source.png, image::encode_png(&rgba).unwrap());
    }

    #[test]
    fn bogus_source_layouts_still_rejected() {
        let good = source_bytes(1, 1, 1, 1, &[0, 0, 128, 128]);
        for (offset, value) in [(0, 0), (4, -1), (0, MAX_SOURCE_SIDE + 1), (8, -1), (12, 2), (20, 0)] {
            let mut bytes = good.clone();
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(parse_source(&bytes, 96).is_none());
        }
        assert!(parse_source(&good[..23], 96).is_none());
        assert!(parse_source(&good[..27], 96).is_none());
        let mut wrong_header = good[..24].to_vec();
        wrong_header.extend_from_slice(&[0; 8]); // Unvalidated x64 ABI header.
        wrong_header.extend_from_slice(&good[24..]);
        assert!(parse_source(&wrong_header, 96).is_none());
        assert!(parse_source(&source_bytes(8192, 8192, 0, 0, &[]), 96).is_none());
        assert!(parse_source(&source_bytes(1, 1, 0, 0, &[0; 4]), 96).is_none());
    }
    #[test]
    fn previews_preserve_aspect_and_bound_text() {
        assert_eq!(size(400, 200), (160, 80));
        assert_eq!(size(100, 1000), (8, 80));
        assert_eq!(size(20, 10), (20, 10));
        assert_eq!(text_preview("中\n🙂\t文"), "中 🙂 文");
        assert_eq!(text_preview(&"🙂".repeat(100)).chars().count(), 80);
    }
    #[test]
    fn text_bitmap_is_bounded_at_target_dpi_and_uses_straight_alpha() {
        for scale in [1., 1.5, 2.] {
            let mut bitmap = preview(&[ClipData::Text("Frame tracking".repeat(100))], scale)
                .unwrap()
                .unwrap();
            assert_eq!(
                (bitmap.width, bitmap.height),
                ((160. * scale) as i32, (32. * scale) as i32)
            );
            assert_eq!(bitmap.pixels().len(), (160. * 32. * scale * scale * 4.) as usize);
            assert!(bitmap.pixels().as_chunks::<4>().0.iter().all(|p| p[3] == 255));
            assert!(bitmap.pixels().as_chunks::<4>().0.iter().any(|p| p[..3] != [255; 3]));
        }
    }
    #[test]
    fn image_preview_is_top_down_straight_alpha_bgra() {
        let rgba = image::Rgba {
            pixels_per_meter: None,
            width: 1,
            height: 2,
            data: vec![255, 0, 0, 128, 0, 0, 255, 255],
        };
        let mut bitmap = preview(&[ClipData::Png(image::encode_png(&rgba).unwrap())], 1.)
            .unwrap()
            .unwrap();
        assert_eq!(bitmap.pixels(), &[0, 0, 255, 128, 255, 0, 0, 255]);
    }
    #[test]
    fn source_layout_is_bounded_and_unpremultiplied() {
        let mut bytes = Vec::new();
        for n in [1_i32, 2, 0, 1, 0, -1] {
            bytes.extend_from_slice(&n.to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0, 128, 128, 255, 0, 0, 255]);
        let source = parse_source(&bytes, 192).unwrap();
        assert_eq!((source.width, source.height, source.hotspot_y), (0.5, 1., 0.5));
        let rgba = image::decode_png(&source.png).unwrap();
        assert_eq!(rgba.data, [0, 0, 255, 255, 255, 0, 0, 128]);
        assert_eq!(*source.png, image::encode_png(&rgba).unwrap()); // In-limit output is byte-for-byte unchanged.
        bytes.push(0);
        assert!(parse_source(&bytes, 96).is_none());
        bytes.pop();
        bytes[0..4].copy_from_slice(&i32::MAX.to_le_bytes());
        assert!(parse_source(&bytes, 96).is_none());
    }
}
