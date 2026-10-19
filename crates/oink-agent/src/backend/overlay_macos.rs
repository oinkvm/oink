use super::{Frame, OverlayShown};
use objc2::rc::Retained;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSFont, NSImage, NSImageScaling, NSImageView, NSPanel, NSScreen,
    NSScreenSaverWindowLevel, NSTextAlignment, NSTextField, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSData, NSPoint, NSRect, NSSize, NSString};
use std::cell::RefCell;
use std::io::Cursor;

thread_local! { static WINDOWS: RefCell<Vec<Retained<NSPanel>>> = const { RefCell::new(Vec::new()) }; }

pub(super) fn hide() {
    super::super::on_main(close);
}
fn close() {
    super::SHOWN.store(false, std::sync::atomic::Ordering::Release);
    WINDOWS.with(|windows| {
        for window in windows.borrow_mut().drain(..) {
            window.orderOut(None);
        }
    });
}

pub(super) fn show(id: u64, frames: Vec<Frame>) {
    // Encoding does not touch AppKit and can be expensive on high-resolution displays.
    let encoded: Vec<_> = frames
        .into_iter()
        .map(|frame| {
            let mut bytes = Cursor::new(Vec::new());
            let result = frame
                .pixels
                .write_to(&mut bytes, image::ImageFormat::Png)
                .map(|()| bytes.into_inner())
                .map_err(|e| e.to_string());
            (frame, result)
        })
        .collect();
    super::super::on_main(move || {
        close();
        let mtm = MainThreadMarker::new().expect("AppKit main thread");
        let primary_height = NSScreen::screens(mtm)
            .firstObject()
            .map_or(0.0, |screen| screen.frame().size.height);
        let mut reports = Vec::new();
        for (frame, png) in encoded {
            let mut report = OverlayShown {
                display: frame.display.clone(),
                pixel_width: frame.pixels.width(),
                pixel_height: frame.pixels.height(),
                error: None,
            };
            match png.and_then(|png| make_window(mtm, &frame, &png, primary_height)) {
                Ok(window) => WINDOWS.with(|windows| windows.borrow_mut().push(window)),
                Err(error) => report.error = Some(error),
            }
            reports.push(report);
        }
        super::report(id, reports);
    });
}

fn make_window(
    mtm: MainThreadMarker,
    frame: &Frame,
    png: &[u8],
    primary_height: f64,
) -> Result<Retained<NSPanel>, String> {
    let bounds = frame.bounds;
    let rect = NSRect::new(
        NSPoint::new(bounds.x, primary_height - bounds.y - bounds.h),
        NSSize::new(bounds.w, bounds.h),
    );
    let window = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        rect,
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    // Overlay windows must never become the foreground application or acquire keyboard focus.
    let policy = oink_foundation::panel_policy::PanelPolicy::BACKEND_OVERLAY;
    window.setHidesOnDeactivate(policy.hides_on_deactivate);
    // Hide Others can hide the accessory backend during screen identification.
    // The request owns this surface's lifetime independently of app hiding.
    window.setCanHide(policy.can_hide);
    window.setIgnoresMouseEvents(true);
    window.setHasShadow(false);
    window.setLevel(NSScreenSaverWindowLevel);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    // SAFETY: WINDOWS retains the panel until it has been ordered out on this thread.
    unsafe { window.setReleasedWhenClosed(false) };
    let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(png))
        .ok_or("Could not decode identification background")?;
    let view = NSImageView::initWithFrame(
        NSImageView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(bounds.w, bounds.h)),
    );
    view.setImage(Some(&image));
    view.setImageScaling(NSImageScaling::ScaleAxesIndependently);
    let [x, y, w, h] = frame.caption.map(f64::from);
    let sx = bounds.w / f64::from(frame.pixels.width());
    let sy = bounds.h / f64::from(frame.pixels.height());
    let label = NSTextField::labelWithString(&NSString::from_str(&frame.title), mtm);
    label.setFrame(NSRect::new(
        NSPoint::new(x * sx, bounds.h - (y + h) * sy),
        NSSize::new(w * sx, h * sy),
    ));
    label.setAlignment(NSTextAlignment::Center);
    label.setFont(Some(&NSFont::boldSystemFontOfSize((h * sy / 4.0).clamp(16.0, 54.0))));
    label.setTextColor(Some(&NSColor::whiteColor()));
    view.addSubview(&label);
    window.setContentView(Some(&view));
    window.orderFrontRegardless();
    Ok(window)
}
