//! Backend-owned native windows. Rendering run on ordinary threads;
//! platform window creation and destruction are dispatched to the main thread.
use crate::api::{Overlay, OverlayShown};
use oink_foundation::Rect;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "macos")]
#[path = "overlay_macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "overlay_windows.rs"]
mod platform;

static SHOWN: AtomicBool = AtomicBool::new(false);

struct Frame {
    display: String,
    bounds: Rect,
    title: String,
    pixels: image::RgbImage,
    caption: [u32; 4],
}

#[cfg(any(target_os = "macos", windows))]
fn enqueue(overlay: Option<Overlay>) {
    use std::sync::{OnceLock, mpsc};
    static COMMANDS: OnceLock<mpsc::Sender<Option<Overlay>>> = OnceLock::new();
    let commands = COMMANDS.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Option<Overlay>>();
        std::thread::Builder::new()
            .name("oink-overlay-render".into())
            .spawn(move || {
                for overlay in rx {
                    match overlay {
                        Some(overlay) => match prepare(&overlay) {
                            Ok(frames) => platform::show(overlay.id, frames),
                            Err(error) => {
                                // A failed replacement must also close the previous windows.
                                platform::hide();
                                tracing::error!(%error, "could not render native overlay");
                                report(
                                    overlay.id,
                                    overlay
                                        .screens
                                        .iter()
                                        .map(|screen| OverlayShown {
                                            display: screen.display.clone(),
                                            pixel_width: 0,
                                            pixel_height: 0,
                                            error: Some(error.clone()),
                                        })
                                        .collect(),
                                );
                            }
                        },
                        None => platform::hide(),
                    }
                }
            })
            .expect("overlay rendering thread");
        tx
    });
    let _ = commands.send(overlay);
}

/// Show or replace the windows on this device's screens.
pub fn show(overlay: &Overlay) {
    #[cfg(any(target_os = "macos", windows))]
    enqueue(Some(overlay.clone()));
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = overlay;
}

pub fn hide() {
    #[cfg(any(target_os = "macos", windows))]
    enqueue(None);
}

pub fn shown() -> bool {
    SHOWN.load(Ordering::Acquire)
}

#[cfg(windows)]
pub(super) fn run_main<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    platform::run_main(work)
}

#[cfg(windows)]
pub(super) fn on_main(work: impl FnOnce() + Send + 'static) {
    platform::on_main(work);
}

fn prepare(overlay: &Overlay) -> Result<Vec<Frame>, String> {
    let node = super::node().map_err(|error| error.to_string())?;
    let displays = node
        .link_config()
        .map_err(|e| e.to_string())?
        .local_displays()
        .displays
        .clone();
    let mut frames = Vec::new();
    for screen in &overlay.screens {
        let display = displays
            .iter()
            .find(|display| display.key == screen.display)
            .ok_or_else(|| format!("找不到屏幕 {}", screen.display))?;
        let (pixels, caption) = render(display.pixel_width, display.pixel_height);
        frames.push(Frame {
            display: screen.display.clone(),
            bounds: display.bounds,
            title: format!("{} · {}", screen.device_name, screen.display_name),
            pixels,
            caption,
        });
    }
    Ok(frames)
}

fn render(width: u32, height: u32) -> (image::RgbImage, [u32; 4]) {
    (
        image::RgbImage::from_pixel(width, height, image::Rgb([30, 33, 40])),
        [0, height / 3, width, height / 3],
    )
}

fn report(id: u64, screens: Vec<OverlayShown>) {
    SHOWN.store(screens.iter().any(|screen| screen.error.is_none()), Ordering::Release);
    if let Ok(node) = super::node() {
        std::thread::spawn(move || {
            crate::display_overlay::shown(&node, id, screens);
        });
    }
}
