//! macOS：按屏幕读当前桌面壁纸，做成缩略图（`crate::wallpaper`）。
//!
//! - 来源：`NSWorkspace.desktopImageURL(for:)` 给出的文件，加 `desktopImageOptions(for:)` 里的填充色
//!   （图片有透明部分时垫在下面）。只读文件，不截屏，不需要屏幕录制权限。
//! - 解码与缩放：ImageIO 的缩略图接口（能读 HEIC；多帧的动态 HEIC 取主图），画进屏幕宽高比的位图
//!   （铺满、居中裁剪），再编码成 JPEG。
//! - 拿不到静态图片（没有 URL、是文件夹或视频、文件不存在、解码失败）的屏幕不出现在结果里。
//! - 壁纸可能是私人照片：本模块不记录路径、内容或任何由它们算出的值。
//!
//! **线程**：`NSScreen` 只能在主线程用，所以「有哪些屏幕、各自的壁纸文件」在主队列上查询（等最多
//! [`MAIN_TIMEOUT`]），解码在调用方的线程上做。主线程没有在服务主队列时（单元测试、没有 run loop 的
//! 命令行程序）查询超时，结果为 `None`。调用方不能在持有主线程可能要等的锁时调用。

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSColor, NSColorSpace, NSScreen, NSWorkspace, NSWorkspaceActiveSpaceDidChangeNotification,
    NSWorkspaceDesktopImageFillColorKey,
};
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFMutableData, CFNumber, CFRetained, CFString, CFType, CFURL, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo,
    CGInterpolationQuality, kCGColorSpaceSRGB,
};
use objc2_foundation::{NSNotification, NSNumber, NSString};
use objc2_image_io::{
    CGImageDestination, CGImageSource, kCGImageDestinationLossyCompressionQuality,
    kCGImageSourceCreateThumbnailFromImageAlways, kCGImageSourceCreateThumbnailWithTransform,
    kCGImageSourceThumbnailMaxPixelSize,
};

use crate::ChangeHandler;
use crate::wallpaper::{DisplayWallpaper, LONG_EDGE, TARGET_BYTES, Thumbnail, cover_rect, thumbnail_size};

const MAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// 依次尝试的 JPEG 质量：第一个不超过 [`TARGET_BYTES`] 的结果就用；都超过时用最后一个。
const QUALITIES: [f64; 4] = [0.72, 0.55, 0.4, 0.28];

/// 一块屏幕的壁纸来源（主线程上查到的，纯数据）。
struct Source {
    key: String,
    /// 屏幕的宽高（point），只用它们的比例。
    width: f64,
    height: f64,
    path: PathBuf,
    /// sRGB 填充色（没有时为黑）。
    fill: [f64; 3],
}

pub(super) fn read_all() -> Option<Vec<DisplayWallpaper>> {
    let sources = on_main_wait(MAIN_TIMEOUT, sources)?;
    Some(
        sources
            .into_iter()
            .filter_map(|s| {
                let thumbnail = render(&s.path, s.fill, s.width, s.height)?;
                Some(DisplayWallpaper { key: s.key, thumbnail })
            })
            .collect(),
    )
}

fn sources(mtm: MainThreadMarker) -> Vec<Source> {
    let workspace = NSWorkspace::sharedWorkspace();
    let number_key = NSString::from_str("NSScreenNumber");
    let mut out = Vec::new();
    for screen in NSScreen::screens(mtm).to_vec() {
        let Some(id) = screen
            .deviceDescription()
            .objectForKey(&number_key)
            .and_then(|v| v.downcast::<NSNumber>().ok())
            .map(|n| n.unsignedIntValue())
        else {
            continue;
        };
        let Some(path) = workspace
            .desktopImageURLForScreen(&screen)
            .and_then(|url| url.to_file_path())
        else {
            continue;
        };
        // SAFETY: 读取 AppKit 导出的常量。
        let fill_key = unsafe { NSWorkspaceDesktopImageFillColorKey };
        let fill = workspace
            .desktopImageOptionsForScreen(&screen)
            .and_then(|options| options.objectForKey(fill_key))
            .and_then(|c| c.downcast::<NSColor>().ok())
            .and_then(|c| c.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace()))
            .map_or([0.0; 3], |c| [c.redComponent(), c.greenComponent(), c.blueComponent()]);
        let frame = screen.frame();
        out.push(Source {
            key: super::describe(id).key,
            width: frame.size.width,
            height: frame.size.height,
            path,
            fill,
        });
    }
    out
}

/// 把 `path` 的图片做成宽高比为 `width : height` 的缩略图。任何一步不成（不是文件、不是 ImageIO 认识的
/// 静态图片、解码或编码失败）都返回 `None`。
fn render(path: &Path, fill: [f64; 3], width: f64, height: f64) -> Option<Thumbnail> {
    if !path.is_file() {
        return None;
    }
    let (tw, th) = thumbnail_size(width, height)?;
    let url = CFURL::from_file_path(path)?;
    // SAFETY: 没有传 options。
    let source = unsafe { CGImageSource::with_url(&url, None) }?;
    // SAFETY: 只读查询。
    let (count, primary) = unsafe { (source.count(), source.primary_image_index()) };
    if count == 0 {
        return None;
    }
    // SAFETY: 读取 ImageIO 导出的常量。
    let keys: [&CFString; 3] = unsafe {
        [
            kCGImageSourceCreateThumbnailFromImageAlways,
            kCGImageSourceCreateThumbnailWithTransform,
            kCGImageSourceThumbnailMaxPixelSize,
        ]
    };
    // 解码成两倍于缩略图长边的图，再缩到目标尺寸：宽高比与屏幕不同、要裁掉一部分时仍然够清晰。
    let max = CFNumber::new_i64(i64::from(LONG_EDGE) * 2);
    let values: [&CFType; 3] = [CFBoolean::new(true), CFBoolean::new(true), &max];
    let options = CFDictionary::<CFString, CFType>::from_slices(&keys, &values);
    // SAFETY: options 的键值类型符合 ImageIO 文档；下标小于 count。
    let image: CFRetained<CGImage> =
        unsafe { source.thumbnail_at_index(primary.min(count - 1), Some(options.as_opaque())) }?;
    let (iw, ih) = (CGImage::width(Some(&image)), CGImage::height(Some(&image)));
    if iw == 0 || ih == 0 {
        return None;
    }

    // SAFETY: 读取 CoreGraphics 导出的常量。
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    // SAFETY: data 为空指针时位图内存由 CoreGraphics 分配和释放（此时 bytesPerRow 传 0 由它计算）。
    let ctx = unsafe {
        CGBitmapContextCreate(
            std::ptr::null_mut(),
            tw as usize,
            th as usize,
            8,
            0,
            Some(&space),
            CGImageAlphaInfo::NoneSkipLast.0,
        )
    }?;
    let (tw_f, th_f) = (f64::from(tw), f64::from(th));
    CGContext::set_rgb_fill_color(Some(&ctx), fill[0], fill[1], fill[2], 1.0);
    CGContext::fill_rect(Some(&ctx), CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(tw_f, th_f)));
    CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
    let (x, y, w, h) = cover_rect(iw as f64, ih as f64, tw_f, th_f);
    CGContext::draw_image(
        Some(&ctx),
        CGRect::new(CGPoint::new(x, y), CGSize::new(w, h)),
        Some(&image),
    );
    let scaled = CGBitmapContextCreateImage(Some(&ctx))?;

    let mut jpeg = Vec::new();
    for quality in QUALITIES {
        jpeg = encode_jpeg(&scaled, quality)?;
        if jpeg.len() <= TARGET_BYTES {
            break;
        }
    }
    Some(Thumbnail {
        width: tw as u16,
        height: th as u16,
        jpeg,
    })
}

fn encode_jpeg(image: &CGImage, quality: f64) -> Option<Vec<u8>> {
    let data = CFMutableData::new(None, 0)?;
    let jpeg_type = CFString::from_static_str("public.jpeg");
    // SAFETY: 没有传 options；data 在 dest 使用期间有效。
    let dest = unsafe { CGImageDestination::with_data(&data, &jpeg_type, 1, None) }?;
    let quality = CFNumber::new_f64(quality);
    // SAFETY: 读取 ImageIO 导出的常量。
    let keys: [&CFString; 1] = unsafe { [kCGImageDestinationLossyCompressionQuality] };
    let values: [&CFType; 1] = [&quality];
    let properties = CFDictionary::<CFString, CFType>::from_slices(&keys, &values);
    // SAFETY: properties 的键值类型符合 ImageIO 文档。
    unsafe { dest.add_image(image, Some(properties.as_opaque())) };
    // SAFETY: 已加入声明的 1 张图。
    if !unsafe { dest.finalize() } {
        return None;
    }
    Some(data.to_vec())
}

/// 切换桌面空间时调用 `on_change`（每个空间可以有不同的壁纸）。
///
/// 系统没有「壁纸变了」的公开通知；`NSWorkspace.activeSpaceDidChangeNotification` 是公开接口里唯一与壁纸
/// 相关的时机。它由 AppKit 在主线程发出，需要主线程在跑 run loop（界面进程、`run_with_main_run_loop` 的
/// 命令行程序）；没有时只是收不到通知。观察者在进程存续期间一直保留。
pub(super) fn watch_spaces(on_change: ChangeHandler) {
    let block = RcBlock::new(move |_: NonNull<NSNotification>| on_change());
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    // SAFETY: 不限定发送对象；queue 为空时 block 在发通知的线程（主线程）上执行；block 捕获的回调是
    // `Send + Sync` 的。
    let token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceActiveSpaceDidChangeNotification),
            None,
            None,
            &block,
        )
    };
    // 不注销：通知中心持有 block，token 丢弃后观察者仍然有效；这里明确表示有意保留。
    std::mem::forget(token);
}

// ───────────── 主队列 ─────────────

#[repr(C)]
struct DispatchQueue {
    _private: [u8; 0],
}

unsafe extern "C" {
    /// `dispatch_get_main_queue()` 是返回这个对象地址的宏。
    static _dispatch_main_q: DispatchQueue;
    fn dispatch_async_f(queue: *const DispatchQueue, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

type Job = Box<dyn FnOnce(MainThreadMarker) + Send + 'static>;

extern "C" fn trampoline(ctx: *mut c_void) {
    // SAFETY: ctx 是 `on_main_wait` 里 Box::into_raw 得到的 Box<Job>，只执行一次。
    let job = unsafe { Box::from_raw(ctx.cast::<Job>()) };
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // 不让 panic 穿过 C 边界（那会直接终止进程）。
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || job(mtm)));
}

/// 在主线程上执行 `f` 并等结果（最多 `timeout`；主线程没在服务主队列时超时返回 `None`）。
fn on_main_wait<R: Send + 'static>(
    timeout: Duration,
    f: impl FnOnce(MainThreadMarker) -> R + Send + 'static,
) -> Option<R> {
    if let Some(mtm) = MainThreadMarker::new() {
        return Some(f(mtm));
    }
    let (tx, rx) = mpsc::channel();
    let job: Box<Job> = Box::new(Box::new(move |mtm| {
        let _ = tx.send(f(mtm));
    }));
    // SAFETY: 主队列对象在进程内一直有效；context 由 trampoline 接管并释放。
    unsafe {
        dispatch_async_f(
            &raw const _dispatch_main_q,
            Box::into_raw(job).cast::<c_void>(),
            trampoline,
        )
    };
    rx.recv_timeout(timeout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 800×200 的测试图：左、中、右三条等宽的红、绿、蓝。写成 PNG 文件。
    fn write_bands(path: &Path) {
        let (w, h) = (800usize, 200usize);
        // SAFETY: 读取 CoreGraphics 导出的常量。
        let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).unwrap();
        // SAFETY: 位图内存由 CoreGraphics 分配。
        let ctx = unsafe {
            CGBitmapContextCreate(
                std::ptr::null_mut(),
                w,
                h,
                8,
                0,
                Some(&space),
                CGImageAlphaInfo::NoneSkipLast.0,
            )
        }
        .unwrap();
        let third = w as f64 / 3.0;
        for (i, rgb) in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]].iter().enumerate() {
            CGContext::set_rgb_fill_color(Some(&ctx), rgb[0], rgb[1], rgb[2], 1.0);
            CGContext::fill_rect(
                Some(&ctx),
                CGRect::new(CGPoint::new(third * i as f64, 0.0), CGSize::new(third, h as f64)),
            );
        }
        let image = CGBitmapContextCreateImage(Some(&ctx)).unwrap();
        let url = CFURL::from_file_path(path).unwrap();
        let png = CFString::from_static_str("public.png");
        // SAFETY: 没有传 options。
        let dest = unsafe { CGImageDestination::with_url(&url, &png, 1, None) }.unwrap();
        // SAFETY: 加入声明的 1 张图后收尾。
        unsafe {
            dest.add_image(&image, None);
            assert!(dest.finalize());
        }
    }

    /// 解码 JPEG，返回 (宽, 高, RGBX 像素)。
    fn decode(jpeg: &[u8]) -> (usize, usize, Vec<u8>) {
        let data = objc2_core_foundation::CFData::from_bytes(jpeg);
        // SAFETY: 没有传 options；下标 0 存在。
        let image = unsafe { CGImageSource::with_data(&data, None).unwrap().image_at_index(0, None) }.unwrap();
        let (w, h) = (CGImage::width(Some(&image)), CGImage::height(Some(&image)));
        let mut buf = vec![0u8; w * h * 4];
        // SAFETY: 读取 CoreGraphics 导出的常量。
        let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).unwrap();
        // SAFETY: buf 有 w × h × 4 字节，在 ctx 使用期间有效。
        let ctx = unsafe {
            CGBitmapContextCreate(
                buf.as_mut_ptr().cast(),
                w,
                h,
                8,
                w * 4,
                Some(&space),
                CGImageAlphaInfo::NoneSkipLast.0,
            )
        }
        .unwrap();
        CGContext::draw_image(
            Some(&ctx),
            CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64)),
            Some(&image),
        );
        drop(ctx);
        (w, h, buf)
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oink-wallpaper-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn thumbnail_has_screen_aspect_and_is_centre_cropped() {
        let dir = temp_dir("crop");
        let path = dir.join("bands.png");
        write_bands(&path);
        // 2:1 的屏幕、4:1 的图：高度贴合，左右各裁掉四分之一。
        let t = render(&path, [0.0; 3], 2000.0, 1000.0).expect("thumbnail");
        assert_eq!((t.width, t.height), (480, 240));
        assert!(t.jpeg.starts_with(&[0xFF, 0xD8]), "不是 JPEG");
        assert!(t.jpeg.len() <= TARGET_BYTES);
        let (w, h, px) = decode(&t.jpeg);
        assert_eq!((w, h), (480, 240));
        let at = |x: usize, y: usize| {
            let i = (y * w + x) * 4;
            [px[i], px[i + 1], px[i + 2]]
        };
        // 放大 1.2 倍后每条 320 px，可见范围是原图的 [240, 720)：红 80 px、绿 320 px、蓝 80 px。
        let dominant = |c: [u8; 3]| (0..3).max_by_key(|&i| c[i]).unwrap();
        assert_eq!(dominant(at(20, 120)), 0, "{:?}", at(20, 120));
        assert_eq!(dominant(at(60, 20)), 0);
        assert_eq!(dominant(at(240, 120)), 1, "{:?}", at(240, 120));
        assert_eq!(dominant(at(100, 200)), 1);
        assert_eq!(dominant(at(380, 30)), 1);
        assert_eq!(dominant(at(460, 120)), 2, "{:?}", at(460, 120));

        // 竖屏：宽度贴合不了，按高度铺满，只剩中间的绿。
        let t = render(&path, [0.0; 3], 1080.0, 1920.0).expect("thumbnail");
        assert_eq!((t.width, t.height), (270, 480));
        let (w, _, px) = decode(&t.jpeg);
        let i = (240 * w + 135) * 4;
        assert_eq!(dominant([px[i], px[i + 1], px[i + 2]]), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn same_input_gives_identical_bytes() {
        let dir = temp_dir("stable");
        let path = dir.join("bands.png");
        write_bands(&path);
        let a = render(&path, [0.0; 3], 1600.0, 1000.0).unwrap();
        let b = render(&path, [0.0; 3], 1600.0, 1000.0).unwrap();
        assert_eq!(a, b);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_thumbnail_when_there_is_no_static_image() {
        let dir = temp_dir("fallback");
        // 不存在。
        assert!(render(&dir.join("missing.heic"), [0.0; 3], 1600.0, 1000.0).is_none());
        // 文件夹（轮换的壁纸文件夹）。
        assert!(render(&dir, [0.0; 3], 1600.0, 1000.0).is_none());
        // 不是图片（视频、损坏的文件）。
        let junk = dir.join("clip.mov");
        std::fs::write(&junk, b"\0\0\0\x14ftypqt  not really a movie").unwrap();
        assert!(render(&junk, [0.0; 3], 1600.0, 1000.0).is_none());
        let empty = dir.join("empty.png");
        std::fs::write(&empty, b"").unwrap();
        assert!(render(&empty, [0.0; 3], 1600.0, 1000.0).is_none());
        // 屏幕尺寸无效。
        let path = dir.join("bands.png");
        write_bands(&path);
        assert!(render(&path, [0.0; 3], 0.0, 1000.0).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 单元测试的主线程不服务主队列：查询超时，结果是「这次没查到」，不阻塞、不崩溃。
    #[test]
    fn off_main_without_a_main_run_loop_gives_none() {
        let t = std::time::Instant::now();
        assert_eq!(on_main_wait(Duration::from_millis(50), |_| 1), None);
        assert!(t.elapsed() < Duration::from_secs(1));
    }
}
