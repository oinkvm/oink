//! Native system appearance shared by the UI icons and backend trail renderer.
//! Watching never requires a WebView or a normal application window.

#[cfg(windows)]
pub use platform::taskbar_dark;
#[cfg(any(target_os = "macos", windows))]
pub use platform::{system_dark, watch_system_theme};

#[cfg(target_os = "macos")]
mod platform {
    use std::{cell::RefCell, ffi::c_void};

    use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained};
    use objc2_app_kit::NSApplication;
    use objc2_foundation::{
        NSArray, NSDictionary, NSKeyValueObservingOptions, NSObject, NSObjectNSKeyValueObserverRegistration,
        NSObjectProtocol, NSString, ns_string,
    };

    /// Must be read on the AppKit main thread, like application icons.
    pub fn system_dark(mtm: MainThreadMarker) -> bool {
        let appearance = NSApplication::sharedApplication(mtm).effectiveAppearance();
        let names = NSArray::from_slice(&[
            ns_string!("NSAppearanceNameAqua"),
            ns_string!("NSAppearanceNameDarkAqua"),
        ]);
        appearance
            .bestMatchFromAppearancesWithNames(&names)
            .is_some_and(|name| *name == *ns_string!("NSAppearanceNameDarkAqua"))
    }

    type Callback = Box<dyn Fn(bool) + Send + Sync>;
    define_class!(
        // SAFETY: NSObject has no subclass invariants. AppKit owns all callbacks.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "OinkSystemAppearanceObserver"]
        #[ivars = Callback]
        struct Observer;
        unsafe impl NSObjectProtocol for Observer {}
        impl Observer {
            #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
            fn changed(&self, _key: Option<&NSString>, _object: Option<&NSObject>,
                _change: Option<&NSDictionary>, _context: *mut c_void) {
                (self.ivars())(system_dark(self.mtm()));
            }
        }
    );
    // NSApplication and these registrations live for the process lifetime.
    // Retain observers even when every product window has been closed.
    thread_local! { static OBSERVERS: RefCell<Vec<Retained<Observer>>> = const { RefCell::new(Vec::new()) }; }

    /// Register on AppKit's main thread, delivering the current value there.
    /// Worker callers return immediately; registration waits for the main loop.
    pub fn watch_system_theme(changed: impl Fn(bool) + Send + Sync + 'static) {
        // run_with_main_run_loop runs its closure on a worker, even when called
        // from main. Dispatch registration itself, not a run-loop wrapper. This
        // also defers early callers until AppKit can service its main queue.
        // Verified with backend launches on macOS 14.7 and 26.
        let Some(mtm) = MainThreadMarker::new() else {
            crate::backend::on_main(move || watch_system_theme(changed));
            return;
        };
        let observer = Observer::alloc(mtm).set_ivars(Box::new(changed) as Callback);
        // SAFETY: initialize this newly allocated NSObject subclass.
        let observer: Retained<Observer> = unsafe { msg_send![super(observer), init] };
        // Window ThemeChanged events do not exist during backend-only operation.
        // Observe AppKit's effectiveAppearance directly, with Initial to avoid a
        // read/register race. No system settings or app appearance are changed.
        // SAFETY: selector signature above matches KVO; app and observer outlive
        // this process-lifetime registration, and context is unused.
        unsafe {
            NSApplication::sharedApplication(mtm).addObserver_forKeyPath_options_context(
                &observer,
                ns_string!("effectiveAppearance"),
                NSKeyValueObservingOptions::Initial,
                std::ptr::null_mut(),
            );
        }
        OBSERVERS.with(|observers| observers.borrow_mut().push(observer));
    }

    #[cfg(all(test, debug_assertions))]
    mod tests {
        #[test]
        fn worker_registration_defers_without_a_running_main_loop() {
            // libtest's worker is the same threading contract as backend host
            // startup. Before the fix this call panicked at MainThreadMarker.
            std::thread::spawn(|| {
                assert!(objc2::MainThreadMarker::new().is_none());
                super::watch_system_theme(|_| {
                    assert!(objc2::MainThreadMarker::new().is_some());
                });
            })
            .join()
            .unwrap();
        }
    }
}

#[cfg(windows)]
mod platform {
    use windows::{
        Win32::System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_NOTIFY, KEY_QUERY_VALUE, REG_NOTIFY_CHANGE_LAST_SET, RRF_RT_REG_DWORD,
            RegCloseKey, RegGetValueW, RegNotifyChangeKeyValue, RegOpenKeyExW,
        },
        core::{PCWSTR, w},
    };

    const PERSONALIZE: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");

    pub fn system_dark() -> bool {
        dark_value(w!("AppsUseLightTheme"))
    }

    /// Windows permits separate application and taskbar color modes.
    pub fn taskbar_dark() -> bool {
        dark_value(w!("SystemUsesLightTheme"))
    }

    fn dark_value(value: PCWSTR) -> bool {
        let mut light = 1u32;
        let mut size = std::mem::size_of_val(&light) as u32;
        // SAFETY: read-only HKCU value with a DWORD-sized output buffer.
        let result = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PERSONALIZE,
                value,
                RRF_RT_REG_DWORD,
                None,
                Some((&mut light as *mut u32).cast()),
                Some(&mut size),
            )
        };
        result.is_ok() && light == 0
    }

    /// Immediately delivers the current value; later calls run on the watcher.
    pub fn watch_system_theme(changed: impl Fn(bool) + Send + Sync + 'static) {
        std::thread::spawn(move || {
            let mut key = HKEY::default();
            // SAFETY: read/notify-only access to the user's appearance key.
            let result = unsafe {
                RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    PERSONALIZE,
                    None,
                    KEY_NOTIFY | KEY_QUERY_VALUE,
                    &mut key,
                )
            };
            changed(system_dark());
            if result.is_err() {
                tracing::warn!(?result, "system appearance notifications unavailable");
                return;
            }
            loop {
                // A tray-only launch has no window to emit ThemeChanged. Block
                // on native notifications without polling or an invisible WebView.
                // SAFETY: live read-only key; synchronous wait needs no event.
                if unsafe { RegNotifyChangeKeyValue(key, false, REG_NOTIFY_CHANGE_LAST_SET, None, false) }.is_err() {
                    break;
                }
                changed(system_dark());
            }
            // SAFETY: key was opened here and is no longer borrowed.
            let _ = unsafe { RegCloseKey(key) };
        });
    }
}
