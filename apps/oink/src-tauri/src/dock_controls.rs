//! Add the optional Dock-menu delegate method without replacing Tao's delegate.
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{MainThreadMarker, sel};
use objc2_app_kit::{NSApplication, NSMenu};
use std::cell::RefCell;
thread_local! { static MENU: RefCell<Option<oink_agent::native_controls::macos::Menu>> = const {RefCell::new(None)}; }
unsafe extern "C-unwind" fn dock_menu(_this: *mut AnyObject, _sel: Sel, _app: *mut NSApplication) -> *const NSMenu {
    MENU.with(|slot| slot.borrow().as_ref().map_or(std::ptr::null(), |m| &*m.menu))
}
pub fn update(app: &tauri::AppHandle, model: &oink_agent::native_controls::Model) {
    let mtm = MainThreadMarker::new().expect("Dock menu on main thread");
    let application = NSApplication::sharedApplication(mtm);
    let Some(delegate) = application.delegate() else {
        return;
    };
    let selector = sel!(applicationDockMenu:);
    let object: &AnyObject = (*delegate).as_ref();
    let class = object.class();
    if class.instance_method(selector).is_none() {
        // SAFETY: add only this absent optional delegate method. The callback
        // follows applicationDockMenu:'s object-return/object-argument ABI and
        // returns a menu retained in main-thread storage for AppKit tracking.
        let added = unsafe {
            let implementation: Imp = std::mem::transmute(
                dock_menu as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut NSApplication) -> *const NSMenu,
            );
            objc2::ffi::class_addMethod(
                class as *const AnyClass as *mut AnyClass,
                selector,
                implementation,
                c"@@:@".as_ptr(),
            )
        };
        if !added.as_bool() {
            tracing::warn!("could not install Dock controls");
            return;
        }
    }
    let handle = app.clone();
    MENU.with(|slot| {
        *slot.borrow_mut() = Some(oink_agent::native_controls::macos::Menu::new(
            model,
            move |id| crate::native_controls::invoke(&handle, id),
            mtm,
        ))
    });
}
