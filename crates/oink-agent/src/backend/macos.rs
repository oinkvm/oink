//! AppKit is owned by the backend main thread. No WebView is created here.
use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use super::Launch;
use crate::api::AgentApi;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBitmapImageRep, NSEvent, NSEventMask,
    NSEventModifierFlags, NSEventType, NSImage, NSSquareStatusItemLength, NSStatusBar, NSStatusItem,
};
use objc2_foundation::{NSData, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSSize, NSString};

fn status_icon(look: crate::tray::TrayLook) -> Retained<NSImage> {
    let logical_size = NSSize::new(16.0, 16.0);
    let image = NSImage::initWithSize(NSImage::alloc(), logical_size);
    // Give AppKit independent 1x/2x/3x representations at the same logical size,
    // so moving between displays never scales a low-resolution bitmap.
    for png in crate::tray::mac_images(look) {
        let data = NSData::with_bytes(png);
        let rep = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &data).expect("embedded tray PNG");
        rep.setSize(logical_size);
        image.addRepresentation(&rep);
    }
    image.setTemplate(true);
    image.setAccessibilityDescription(Some(&NSString::from_str("Oink")));
    image
}

pub(super) fn update_status_icon(view: &crate::device_cards::DeviceCardsView) {
    let view = view.clone();
    on_main(move || {
        MENU.with(|slot| {
            if let Some(menu) = slot.borrow_mut().as_mut()
                && menu.presentation.update(&view)
                && let Some(button) = menu
                    .status_item
                    .button(MainThreadMarker::new().expect("AppKit main thread"))
            {
                button.setImage(Some(&status_icon(menu.presentation.look)));
            }
        })
    });
}

struct LaunchState {
    launch: Launch,
    smoke: bool,
    startup: RefCell<Option<(oink_network::NetworkConfig, crate::instance::InstanceLock)>>,
}

static EXIT: AtomicU8 = AtomicU8::new(0);
static SHOW_REQUESTED: AtomicBool = AtomicBool::new(false);

fn request_window() {
    let mtm = MainThreadMarker::new().expect("launch request on AppKit thread");
    let app = NSApplication::sharedApplication(mtm);
    // The bundle entry and WebView helper are separate applications. macOS 14+
    // activate() is cooperative: yield the launcher's activation before the
    // helper asks to activate, including when it is already running. A plain
    // IPC ShowWindow does not transfer that permission. The bundle-id form also
    // covers a helper that has not been started yet (public AppKit contract).
    let identifier = NSString::from_str("com.hachiwii.oink.ui");
    let running = objc2_app_kit::NSRunningApplication::runningApplicationsWithBundleIdentifier(&identifier);
    if let Some(helper) = running.firstObject() {
        app.yieldActivationToApplication(&helper);
    } else {
        app.yieldActivationToApplicationWithBundleIdentifier(&identifier);
    }
    if let Err(error) = super::show_ui() {
        // A reopen event can arrive while the startup worker is still preparing
        // IPC. Retain it until attach instead of silently discarding the click.
        SHOW_REQUESTED.store(true, Ordering::Release);
        tracing::info!(%error, "manual window request retained until backend ready");
    }
}
struct Menu {
    status_item: Retained<NSStatusItem>,
    presentation: crate::tray::TrayPresentation,
}
thread_local! { static MENU: RefCell<Option<Menu>> = const { RefCell::new(None) }; }

define_class!(
    // SAFETY: NSObject has no subclass invariants; all AppKit access is main-thread-only.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OinkBackendDelegate"]
    #[ivars = LaunchState]
    struct Delegate;
    unsafe impl NSObjectProtocol for Delegate {}
    unsafe impl NSApplicationDelegate for Delegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn launched(&self, _notification: &NSNotification) {
            // The login-item Apple event is only available inside this callback.
            let show = !self.ivars().smoke && super::shows_on_start(self.ivars().launch, oink_autostart::launched_at_login());
            tracing::info!(launch = ?self.ivars().launch, show, "macOS launch decision");
            if !self.ivars().smoke { self.menu(); }
            let smoke = self.ivars().smoke;
            let startup = self.ivars().startup.borrow_mut().take();
            let (net, lock) = startup.expect("one launch callback");
            // Escaping worker panics terminate the process through the same
            // numeric fatal boundary as main; never leave a status-only shell
            // or race main's return against the fatal capsule commit.
            std::thread::spawn(move || crate::telemetry::fatal_boundary(move || {
                let result = if smoke {
                    crate::host::AgentHost::start_launch_smoke_locked(net, lock)
                } else {
                    crate::host::AgentHost::start_platform_locked(net, lock, crate::host::IpcMode::Required)
                };
                match result {
                    Ok(host) => {
                        if !smoke { super::attach(host.node()); }
                        if smoke {
                            let home = host.node().data_dir().to_path_buf();
                            // FIFO main-queue work follows the Node's registration.
                            // Require a real initial AppKit callback as well as a
                            // completed host startup before packaging can pass.
                            crate::appearance::watch_system_theme(move |_| {
                                if let Err(error) = std::fs::write(home.join("launch-smoke-ready"), b"host and AppKit appearance ready\n") {
                                    tracing::error!(%error, "launch smoke readiness failed");
                                    EXIT.store(1, Ordering::Release);
                                    stop_loop();
                                }
                            });
                        }
                        tracing::info!("backend ready for UI");
                        let requested = SHOW_REQUESTED.swap(false, Ordering::AcqRel);
                        if show || requested { on_main(request_window); }
                        host.block_on(host.node().wait_for_stop());
                        host.shutdown();
                    }
                    Err(error) if error.downcast_ref::<crate::host::StartupTerminated>().is_some() => {}
                    Err(error) if error.downcast_ref::<crate::instance::AlreadyRunning>().is_some() => {
                        let remote = crate::remote::RemoteApi::new(oink_foundation::paths::data_dir());
                        if let Err(error) = remote.show_window() { tracing::error!(%error); EXIT.store(1, Ordering::Release); }
                    }
                    Err(error) => { tracing::error!(%error, "backend startup failed"); EXIT.store(1, Ordering::Release); }
                }
                on_main(stop_loop);
            }));
        }
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _app: &NSApplication) -> objc2_app_kit::NSApplicationTerminateReply {
            if crate::update::installation_termination() {
                crate::update::request_native_termination();
                return objc2_app_kit::NSApplicationTerminateReply::TerminateLater;
            }
            crate::update::request_quit();
            objc2_app_kit::NSApplicationTerminateReply::TerminateCancel
        }
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn reopen(&self, _app: &NSApplication, _visible: bool) -> bool {
            tracing::info!("macOS manual reopen received");
            request_window();
            false
        }
    }
    impl Delegate {
        #[unsafe(method(statusClick:))]
        fn status_click(&self, _sender: &objc2_app_kit::NSStatusBarButton) {
            let mtm = MainThreadMarker::new().expect("AppKit main thread");
            if NSApplication::sharedApplication(mtm).currentEvent().is_some_and(|event| event.clickCount() > 1) {
                std::thread::spawn(|| { let _ = super::show_ui(); });
                return;
            }
            let anchor = MENU.with(|slot| {
                let menu = slot.borrow();
                let button = menu.as_ref()?.status_item.button(mtm)?;
                let window = button.window()?;
                let screen = window.screen()?;
                let primary = objc2_app_kit::NSScreen::screens(mtm).firstObject()?;
                let top = primary.frame().origin.y + primary.frame().size.height;
                let rect = window.convertRectToScreen(button.convertRect_toView(button.bounds(), None));
                let convert = |r: objc2_foundation::NSRect| super::PanelRect {
                    x: r.origin.x,
                    y: top - r.origin.y - r.size.height,
                    width: r.size.width,
                    height: r.size.height,
                };
                Some(super::PanelAnchor { rect: convert(rect), work_area: convert(screen.visibleFrame()), scale_factor: 1.0 })
            });
            if let Some(anchor) = anchor {
                std::thread::spawn(move || {
                    if let Err(error) = super::deliver_ui(crate::api::ApiEvent::TogglePanel(anchor)) {
                        tracing::error!(%error, "could not show status panel");
                    }
                });
            }
        }


    }
);

impl Delegate {
    fn new(
        launch: Launch,
        smoke: bool,
        startup: Option<(oink_network::NetworkConfig, crate::instance::InstanceLock)>,
        mtm: MainThreadMarker,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(LaunchState {
            launch,
            smoke,
            startup: RefCell::new(startup),
        });
        // SAFETY: NSObject initializer on the just-allocated subclass.
        unsafe { msg_send![super(this), init] }
    }
    fn menu(&self) {
        let mtm = MainThreadMarker::new().expect("AppKit main thread");
        let status_item = NSStatusBar::systemStatusBar().statusItemWithLength(NSSquareStatusItemLength);
        if let Some(button) = status_item.button(mtm) {
            button.setImage(Some(&status_icon(crate::tray::TrayLook::default())));
            button.sendActionOn(NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp);
            // The status item belongs to the nonactivating backend. Only the UI
            // process presents the Web panel; clicking never activates this app.
            // SAFETY: delegate outlives the status item; selector matches sender.
            unsafe {
                button.setTarget(Some(self));
                button.setAction(Some(sel!(statusClick:)));
            }
        }
        MENU.with(|slot| {
            *slot.borrow_mut() = Some(Menu {
                status_item,
                presentation: crate::tray::TrayPresentation::default(),
            })
        });
    }
}

pub(super) fn run(launch: Launch, smoke: bool) -> u8 {
    let entered = Instant::now();
    if !smoke {
        crate::logs::init_service();
        tracing::info!(
            pid = std::process::id(),
            pre_log_ms = entered.elapsed().as_millis() as u64,
            "macOS backend process starting"
        );
    }
    if launch == Launch::Reopen {
        let dir = oink_foundation::paths::data_dir();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (Some(backend), Some(ui)) = (crate::instance::is_running(&dir), crate::instance::is_ui_running(&dir))
            else {
                eprintln!("process ownership unavailable");
                return 1;
            };
            if !backend && !ui {
                break;
            }
            if Instant::now() >= deadline {
                return 1;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let net = if smoke {
        match super::smoke::network() {
            Ok(net) => net,
            Err(error) => {
                eprintln!("{error}");
                return 1;
            }
        }
    } else {
        oink_network::NetworkConfig::default()
    };
    let lock = match super::claim_instance(&net.data_dir) {
        Ok(Some(lock)) => lock,
        Ok(None) => return 0,
        Err(error) => {
            eprintln!("后台单实例锁失败：{error}");
            return 1;
        }
    };
    if smoke {
        crate::logs::init_service();
    }
    run_application(launch, smoke, (net, lock))
}

fn run_application(
    launch: Launch,
    smoke: bool,
    startup: (oink_network::NetworkConfig, crate::instance::InstanceLock),
) -> u8 {
    let mtm = MainThreadMarker::new().expect("backend entry must run on the main thread");
    let started = Instant::now();
    tracing::info!("initializing AppKit application");
    let app = NSApplication::sharedApplication(mtm);
    tracing::info!(
        ms = started.elapsed().as_millis() as u64,
        "AppKit application initialized"
    );
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let delegate = Delegate::new(launch, smoke, Some(startup), mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
    app.setDelegate(None);
    MENU.with(|slot| *slot.borrow_mut() = None);
    EXIT.load(Ordering::Acquire)
}

pub(super) fn stop_loop() {
    let mtm = MainThreadMarker::new().expect("AppKit main thread");
    let app = NSApplication::sharedApplication(mtm);
    if crate::update::termination_waiting() {
        app.replyToApplicationShouldTerminate(true);
        return;
    }
    app.stop(None);
    // AppKit stop is observed on its next event. A local application event wakes
    // the event loop; this does not inject keyboard or mouse input into the system.
    if let Some(event) =
        NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::ApplicationDefined,
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags::empty(),
            0.0,
            0,
            None,
            0,
            0,
            0,
        )
    {
        app.postEvent_atStart(&event, true);
    }
}

#[repr(C)]
struct DispatchQueue {
    _private: [u8; 0],
}
unsafe extern "C" {
    static _dispatch_main_q: DispatchQueue;
    fn dispatch_async_f(queue: *const DispatchQueue, context: *mut c_void, work: extern "C" fn(*mut c_void));
}
type Job = Box<dyn FnOnce() + Send>;
extern "C" fn execute(context: *mut c_void) {
    // SAFETY: on_main transfers one Box<Job> to this callback, invoked exactly once.
    let job = unsafe { Box::from_raw(context.cast::<Job>()) };
    job();
}
pub(super) fn on_main(f: impl FnOnce() + Send + 'static) {
    let job: Box<Job> = Box::new(Box::new(f));
    // SAFETY: a Box<Job> is transferred exactly once to execute on the main queue.
    // AppKit objects and their destructors must stay on that queue.
    unsafe { dispatch_async_f(&raw const _dispatch_main_q, Box::into_raw(job).cast(), execute) };
}

pub(super) fn set_status_icon_visible(visible: bool) {
    on_main(move || {
        MENU.with(|slot| {
            if let Some(menu) = slot.borrow().as_ref() {
                menu.status_item.setVisible(visible);
            }
        })
    });
}

#[cfg(all(test, debug_assertions))]
mod tests {
    #[test]
    fn embedded_status_icon_loads_as_accessible_template() {
        // NSImage can be created independently of NSApplication: this test never
        // creates a status item, starts a host, or contacts the installed app.
        objc2::rc::autoreleasepool(|_| {
            use crate::tray::{TrayLook, TrayState};
            for state in [
                TrayState::Default,
                TrayState::Cursor,
                TrayState::Locked,
                TrayState::Game,
                TrayState::ScreenOff,
                TrayState::Paused,
            ] {
                for solid in [false, true] {
                    let image = super::status_icon(TrayLook { state, solid });
                    assert!(image.isValid());
                    assert!(image.isTemplate());
                    let reps = image.representations();
                    assert_eq!(reps.len(), 3);
                    for (i, rep) in reps.iter().enumerate() {
                        assert_eq!(rep.pixelsWide(), 16 * (i as isize + 1));
                        assert_eq!(rep.size(), objc2_foundation::NSSize::new(16.0, 16.0));
                    }
                    assert_eq!(image.size(), objc2_foundation::NSSize::new(16.0, 16.0));
                    assert_eq!(image.accessibilityDescription().unwrap().to_string(), "Oink");
                }
            }
        });
    }
}
