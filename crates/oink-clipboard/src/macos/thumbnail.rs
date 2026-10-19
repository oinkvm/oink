//! Optional source artwork; never fulfills file promises.
use objc2_app_kit::NSPasteboard;
use objc2_core_foundation::{CFBoolean, CFData, CFDictionary, CFMutableData, CFNumber, CFRetained, CFString, CFType};
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSString, NSURL};
use objc2_image_io::{
    CGImageDestination, CGImageSource, kCGImageSourceCreateThumbnailFromImageAlways,
    kCGImageSourceCreateThumbnailWithTransform, kCGImageSourceThumbnailMaxPixelSize,
};
use oink_foundation::dnd::DragImage;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

/// Immutable, fully decoded CoreGraphics pixels passed once to AppKit.
pub(super) struct TargetImage(CFRetained<CGImage>);
// SAFETY: CGImage is immutable; ImageIO decoding is completed on the worker.
// Only a retained immutable image crosses threads, never an NSImage/bitmap rep.
unsafe impl Send for TargetImage {}
impl TargetImage {
    pub(super) fn appkit(&self, size: objc2_foundation::NSSize) -> objc2::rc::Retained<objc2_app_kit::NSImage> {
        use objc2::AnyThread;
        objc2_app_kit::NSImage::initWithCGImage_size(objc2_app_kit::NSImage::alloc(), &self.0, size)
    }
}
pub(super) fn decode_target(bytes: &[u8]) -> Option<TargetImage> {
    let data = CFData::from_bytes(bytes);
    // SAFETY: immutable bounded PNG bytes, typed ImageIO caching option.
    let source = unsafe { CGImageSource::with_data(&data, None) }?;
    // SAFETY: ImageIO's public constant has static lifetime and a CFBoolean value.
    let keys = unsafe { [objc2_image_io::kCGImageSourceShouldCacheImmediately] };
    let values: [&CFType; 1] = [CFBoolean::new(true)];
    let options = CFDictionary::from_slices(&keys, &values);
    // SAFETY: frame zero and documented options; corrupt PNG returns None.
    unsafe { source.image_at_index(0, Some(options.as_opaque())) }.map(TargetImage)
}

pub(super) const MAX_BYTES: usize = 32 * 1024;
const MAX_INPUT: usize = 32 * 1024 * 1024;
const BUDGET: Duration = Duration::from_millis(200);

#[derive(Clone)]
pub(super) struct Budget {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}
impl Budget {
    pub(super) fn new(start: Instant) -> Self {
        Self {
            deadline: start + BUDGET,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }
    pub(super) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    fn current_at(&self, now: Instant) -> bool {
        now < self.deadline && !self.cancelled.load(Ordering::Acquire)
    }
    pub(super) fn current(&self) -> bool {
        self.current_at(Instant::now())
    }
}

pub(super) fn qualifies(items: usize, ty: &str) -> bool {
    // SAFETY: retained CFStrings describe UTI metadata; this calls no provider.
    #[allow(deprecated)]
    unsafe {
        items == 1
            && objc2_core_services::UTTypeConformsTo(
                &CFString::from_str(ty),
                &CFString::from_static_str("public.image"),
            )
    }
}

fn file_type(path: &std::path::Path) -> Option<String> {
    // NSURL's metadata query returns the file's UTI (including HEIC), rather
    // than an extension allowlist. It runs only on the artwork worker.
    let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str()?));
    let mut value: *mut objc2::runtime::AnyObject = std::ptr::null_mut();
    // SAFETY: live URL and out-parameter; Foundation returns a borrowed NSString.
    #[allow(deprecated)]
    let ok: bool = unsafe {
        objc2::msg_send![&url, getResourceValue: &mut value,
        forKey: objc2_foundation::NSURLTypeIdentifierKey, error: std::ptr::null_mut::<*mut objc2_foundation::NSError>()]
    };
    if !ok || value.is_null() {
        return None;
    }
    // SAFETY: NSURLTypeIdentifierKey's value is an NSString.
    Some(unsafe { &*value.cast::<NSString>() }.to_string())
}

/// Called on the artwork worker with a named pasteboard, never with a retained
/// NSDraggingInfo. Reading an explicit image flavor may ask a lazy data provider
/// for bytes; no promise receiver, promised URL or promised-content flavor is read.
pub(super) fn generate(name: &str, change: isize, budget: &Budget) -> Option<DragImage> {
    if !budget.current() {
        return None;
    }
    let board = NSPasteboard::pasteboardWithName(&NSString::from_str(name));
    if board.changeCount() != change {
        return None;
    }
    let items = board.pasteboardItems()?;
    if items.len() != 1 {
        return None;
    }
    let item = items.objectAtIndex(0);
    // A real file URL takes precedence over companion representations. Never
    // resolve promised-file-url: doing so would fulfill a promise for artwork.
    let result = if let Some(path) = super::content::file_path(&item) {
        if !qualifies(1, &file_type(&path)?) || !budget.current() {
            return None;
        }
        let metadata = std::fs::metadata(&path).ok()?;
        if !metadata.is_file() || metadata.len() > MAX_INPUT as u64 {
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        render_bytes(&bytes, budget)
    } else {
        let ty = item.types().iter().find(|ty| qualifies(1, &ty.to_string()))?;
        if !budget.current() {
            return None;
        }
        let data = item.dataForType(&ty)?;
        if data.len() > MAX_INPUT {
            return None;
        }
        render_bytes(&data.to_vec(), budget)
    };
    (budget.current() && board.changeCount() == change)
        .then_some(result)
        .flatten()
}

fn render_bytes(bytes: &[u8], budget: &Budget) -> Option<DragImage> {
    if !budget.current() || bytes.len() > MAX_INPUT {
        return None;
    }
    let data = CFData::from_bytes(bytes);
    // SAFETY: owned immutable image bytes and default options.
    let source = unsafe { CGImageSource::with_data(&data, None) }?;
    // A noisy 256px PNG can exceed the small wire allowance. Retry smaller,
    // within the same deadline, preserving alpha rather than switching to JPEG.
    for edge in [256, 128, 64] {
        if !budget.current() {
            return None;
        }
        // SAFETY: ImageIO option constants and values have documented CF types.
        let keys: [&CFString; 3] = unsafe {
            [
                kCGImageSourceCreateThumbnailFromImageAlways,
                kCGImageSourceCreateThumbnailWithTransform,
                kCGImageSourceThumbnailMaxPixelSize,
            ]
        };
        let max = CFNumber::new_i64(edge);
        let values: [&CFType; 3] = [CFBoolean::new(true), CFBoolean::new(true), &max];
        let options = CFDictionary::from_slices(&keys, &values);
        // ImageIO has no public cancellation for this synchronous call. Logical
        // cancellation revokes its result at 200ms; it never blocks a handoff.
        // The caller allows only one decoder worker so overdue calls cannot pile up.
        // SAFETY: source owns immutable bytes and options contain typed ImageIO keys.
        let image: CFRetained<CGImage> = unsafe { source.thumbnail_at_index(0, Some(options.as_opaque())) }?;
        if !budget.current() {
            return None;
        }
        let data = CFMutableData::new(None, 0)?;
        // SAFETY: output owns one image; the mutable CFData lives through finalize.
        let dest = unsafe { CGImageDestination::with_data(&data, &CFString::from_static_str("public.png"), 1, None) }?;
        // SAFETY: destination was created for exactly this one retained image.
        unsafe {
            dest.add_image(&image, None);
        }
        // SAFETY: the declared image has been added and CFData remains alive.
        if !unsafe { dest.finalize() } || !budget.current() {
            return None;
        }
        let png = data.to_vec();
        if png.len() <= MAX_BYTES {
            let width = CGImage::width(Some(&image)) as f64;
            let height = CGImage::height(Some(&image)) as f64;
            let result = DragImage {
                png: png.into(),
                width,
                height,
                hotspot_x: width / 2.,
                hotspot_y: height / 2.,
            };
            return result.valid().then_some(result);
        }
    }
    None
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn qualification_requires_exactly_one_item_and_image_uti() {
        for ty in [
            "public.png",
            "public.tiff",
            "public.jpeg",
            "public.heic",
            "com.compuserve.gif",
        ] {
            assert!(qualifies(1, ty));
            assert!(!qualifies(2, ty));
            assert!(!qualifies(0, ty));
        }
        for ty in [
            "public.text",
            "public.file-url",
            "public.pdf",
            "com.apple.pasteboard.promised-file-url",
        ] {
            assert!(!qualifies(1, ty));
        }
    }
    #[test]
    fn timeout_and_cancellation_discard_results_instead_of_waiting() {
        let start = Instant::now();
        let budget = Budget::new(start);
        assert!(budget.current_at(start + Duration::from_millis(199)));
        assert!(!budget.current_at(start + Duration::from_millis(200)));
        budget.cancel();
        assert!(!budget.current_at(start));
    }
    #[test]
    fn imageio_encodes_small_png_and_rejects_invalid_data() {
        let budget = Budget::new(Instant::now());
        let image = render_bytes(&crate::dnd::preview::asymmetric_png(), &budget).unwrap();
        assert!(image.valid());
        assert_eq!((image.width, image.height), (3., 2.));
        assert!(image.png.len() <= MAX_BYTES);
        assert!(render_bytes(b"not an image", &budget).is_none());
        budget.cancel();
        assert!(render_bytes(&crate::dnd::preview::asymmetric_png(), &budget).is_none());
    }
}

#[cfg(all(test, debug_assertions))]
mod native_tests {
    use super::*;
    use objc2::{rc::Retained, runtime::ProtocolObject};
    use objc2_app_kit::{NSPasteboardItem, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSData};
    fn board(items: Vec<Retained<NSPasteboardItem>>) -> Retained<NSPasteboard> {
        let board = NSPasteboard::pasteboardWithUniqueName();
        let objects: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> =
            items.into_iter().map(ProtocolObject::from_retained).collect();
        assert!(board.writeObjects(&NSArray::from_retained_slice(&objects)));
        board
    }
    fn image_item() -> Retained<NSPasteboardItem> {
        let item = NSPasteboardItem::new();
        assert!(item.setData_forType(
            &NSData::with_bytes(&crate::dnd::preview::asymmetric_png()),
            &NSString::from_str("public.png")
        ));
        item
    }
    #[test]
    fn named_board_single_image_only_and_no_promise_fulfillment() {
        objc2::rc::autoreleasepool(|_| {
            let single = board(vec![image_item()]);
            let name = single.name().to_string();
            assert!(generate(&name, single.changeCount(), &Budget::new(Instant::now())).is_some());
            assert!(generate(&name, single.changeCount() + 1, &Budget::new(Instant::now())).is_none());
            let expired = Budget::new(Instant::now() - Duration::from_millis(200));
            assert!(generate(&name, single.changeCount(), &expired).is_none());
            let multi = board(vec![image_item(), image_item()]);
            assert!(
                generate(
                    &multi.name().to_string(),
                    multi.changeCount(),
                    &Budget::new(Instant::now())
                )
                .is_none()
            );
            let text = NSPasteboardItem::new();
            text.setString_forType(
                &NSString::from_str("other item"),
                &NSString::from_str("public.utf8-plain-text"),
            );
            let mixed = board(vec![image_item(), text]);
            assert!(
                generate(
                    &mixed.name().to_string(),
                    mixed.changeCount(),
                    &Budget::new(Instant::now())
                )
                .is_none()
            );
            // An image-typed promise without explicit image bytes is metadata,
            // never a thumbnail input. No receiver or promise callback is installed.
            let promise = NSPasteboardItem::new();
            promise.setString_forType(
                &NSString::from_str("public.png"),
                &NSString::from_str("com.apple.pasteboard.promised-file-content-type"),
            );
            let promises = board(vec![promise]);
            assert!(
                generate(
                    &promises.name().to_string(),
                    promises.changeCount(),
                    &Budget::new(Instant::now())
                )
                .is_none()
            );
            for board in [single, multi, mixed, promises] {
                board.clearContents();
            }
        });
    }

    /// Synthetic pixels only. This is also the bounded local timing check: one
    /// generation per format, not a benchmark loop or any user-file inspection.
    #[test]
    fn generated_image_files_have_bounded_thumbnails_and_report_timing() {
        use objc2_core_foundation::{CFURL, CGPoint, CGRect, CGSize};
        use objc2_core_graphics::{
            CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGImageAlphaInfo,
        };
        objc2::rc::autoreleasepool(|_| {
            let dir = tempfile::tempdir().unwrap();
            let space = CGColorSpace::new_device_rgb().unwrap();
            // SAFETY: CoreGraphics owns the allocated, bounded RGBA context.
            let context = unsafe {
                CGBitmapContextCreate(
                    std::ptr::null_mut(),
                    2048,
                    1536,
                    8,
                    0,
                    Some(&space),
                    CGImageAlphaInfo::PremultipliedLast.0,
                )
            }
            .unwrap();
            for (x, rgb) in [(0., [1., 0., 0.]), (1024., [0., 0., 1.])] {
                CGContext::set_rgb_fill_color(Some(&context), rgb[0], rgb[1], rgb[2], 1.);
                CGContext::fill_rect(
                    Some(&context),
                    CGRect::new(CGPoint::new(x, 0.), CGSize::new(1024., 1536.)),
                );
            }
            let pixels = CGBitmapContextCreateImage(Some(&context)).unwrap();
            for (uti, ext) in [
                ("public.png", "png"),
                ("public.jpeg", "jpg"),
                ("public.tiff", "tiff"),
                ("public.heic", "heic"),
            ] {
                let path = dir.path().join(format!("synthetic.{ext}"));
                let url = CFURL::from_file_path(&path).unwrap();
                // SAFETY: one synthetic image written to a temporary file.
                let destination =
                    unsafe { CGImageDestination::with_url(&url, &CFString::from_str(uti), 1, None) }.unwrap();
                // SAFETY: one image matches the destination's declared count.
                unsafe {
                    destination.add_image(&pixels, None);
                    assert!(destination.finalize());
                }
                assert!(qualifies(1, &file_type(&path).unwrap()));
                let item = NSPasteboardItem::new();
                let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str().unwrap()));
                item.setString_forType(&url.absoluteString().unwrap(), &NSString::from_str("public.file-url"));
                let board = board(vec![item]);
                let start = Instant::now();
                let result = generate(&board.name().to_string(), board.changeCount(), &Budget::new(start)).unwrap();
                let elapsed = start.elapsed();
                println!(
                    "thumbnail_sample format={ext} input=2048x1536 output={}x{} bytes={} elapsed_us={}",
                    result.width,
                    result.height,
                    result.png.len(),
                    elapsed.as_micros()
                );
                assert!(elapsed < BUDGET);
                assert_eq!((result.width, result.height), (256., 192.));
                assert!(result.png.len() <= MAX_BYTES && result.valid());
                board.clearContents();
            }
        });
    }
}
