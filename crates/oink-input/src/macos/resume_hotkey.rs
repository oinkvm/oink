//! Carbon hotkeys are registered on the main thread only while paused. They do
//! not install a CGEventTap or observe arbitrary keyboard events.
use crate::capture_loop::{ResumeHotkey, Wake};
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::c_void,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct HotkeyId {
    signature: u32,
    id: u32,
}
#[repr(C)]
struct EventType {
    class: u32,
    kind: u32,
}
type Ref = *mut c_void;
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetEventDispatcherTarget() -> Ref;
    fn InstallEventHandler(
        target: Ref,
        handler: unsafe extern "C" fn(Ref, Ref, Ref) -> i32,
        count: u32,
        types: *const EventType,
        data: Ref,
        result: *mut Ref,
    ) -> i32;
    fn RemoveEventHandler(handler: Ref) -> i32;
    fn RegisterEventHotKey(key: u32, modifiers: u32, id: HotkeyId, target: Ref, options: u32, result: *mut Ref) -> i32;
    fn UnregisterEventHotKey(hotkey: Ref) -> i32;
    fn GetEventParameter(
        event: Ref,
        name: u32,
        kind: u32,
        actual_kind: *mut u32,
        size: u32,
        actual_size: *mut u32,
        data: Ref,
    ) -> i32;
}
const SIGNATURE: u32 = u32::from_be_bytes(*b"Oink");
struct Callback {
    enabled: Arc<AtomicBool>,
    fired: Arc<AtomicBool>,
    wake: Wake,
}
struct Registration {
    hotkey: Ref,
    handler: Ref,
    _callback: Box<Callback>,
}
impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: both handles were installed on this main thread and are
        // removed before freeing the callback context used by Carbon.
        unsafe {
            UnregisterEventHotKey(self.hotkey);
            RemoveEventHandler(self.handler);
        }
    }
}
thread_local! { static REGISTRATIONS: RefCell<HashMap<u64, Registration>> = RefCell::default(); }
static NEXT: AtomicU64 = AtomicU64::new(1);
unsafe extern "C" fn pressed(_call: Ref, event: Ref, data: Ref) -> i32 {
    let mut id = HotkeyId::default();
    // SAFETY: Carbon supplies the event and our retained Callback pointer.
    let result = unsafe {
        GetEventParameter(
            event,
            u32::from_be_bytes(*b"----"),
            u32::from_be_bytes(*b"hkid"),
            std::ptr::null_mut(),
            std::mem::size_of::<HotkeyId>() as u32,
            std::ptr::null_mut(),
            (&mut id as *mut HotkeyId).cast(),
        )
    };
    if result != 0 || id.signature != SIGNATURE || id.id != 1 {
        return -9874;
    }
    // SAFETY: Registration retains this boxed callback until RemoveEventHandler.
    let callback = unsafe { &*data.cast::<Callback>() };
    if callback.enabled.load(Ordering::Acquire) {
        callback.fired.store(true, Ordering::Release);
        (callback.wake)();
    }
    0
}
fn register(chord: ResumeHotkey, callback: Box<Callback>) -> Result<Registration, String> {
    let key = crate::mac_keymap::hid_to_mac(chord.key).ok_or("resume hotkey has no Carbon key code")?;
    let modifiers = u32::from(chord.meta) << 8
        | u32::from(chord.shift) << 9
        | u32::from(chord.alt) << 11
        | u32::from(chord.control) << 12;
    let mut hotkey = std::ptr::null_mut();
    let mut handler = std::ptr::null_mut();
    let spec = EventType {
        class: u32::from_be_bytes(*b"keyb"),
        kind: 5,
    };
    // SAFETY: main-thread only; the boxed context lives until handler removal.
    unsafe {
        let target = GetEventDispatcherTarget();
        let status = InstallEventHandler(
            target,
            pressed,
            1,
            &spec,
            (&*callback as *const Callback).cast_mut().cast(),
            &mut handler,
        );
        if status != 0 {
            return Err(format!("InstallEventHandler: {status}"));
        }
        let status = RegisterEventHotKey(
            u32::from(key),
            modifiers,
            HotkeyId {
                signature: SIGNATURE,
                id: 1,
            },
            target,
            0,
            &mut hotkey,
        );
        if status != 0 {
            RemoveEventHandler(handler);
            return Err(format!("RegisterEventHotKey: {status}"));
        }
    }
    Ok(Registration {
        hotkey,
        handler,
        _callback: callback,
    })
}
pub(super) struct Manager {
    id: u64,
    requested: Option<ResumeHotkey>,
    enabled: Arc<AtomicBool>,
    fired: Arc<AtomicBool>,
}
impl Default for Manager {
    fn default() -> Self {
        Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            requested: None,
            enabled: Arc::new(AtomicBool::new(false)),
            fired: Arc::new(AtomicBool::new(false)),
        }
    }
}
impl Manager {
    pub fn update(&mut self, requested: Option<ResumeHotkey>, wake: Wake) {
        if self.requested == requested {
            return;
        }
        self.enabled.store(false, Ordering::Release);
        self.fired.store(false, Ordering::Release);
        self.enabled = Arc::new(AtomicBool::new(requested.is_some()));
        self.requested = requested;
        let callback = Box::new(Callback {
            enabled: self.enabled.clone(),
            fired: self.fired.clone(),
            wake,
        });
        let id = self.id;
        // Carbon marks registration as not thread safe. Main-queue ordering also
        // guarantees unregister-before-register when configuration changes.
        if !super::main_queue::run_async(move || {
            REGISTRATIONS.with(|slots| {
                let mut slots = slots.borrow_mut();
                slots.remove(&id);
                if let Some(chord) = requested
                    && callback.enabled.load(Ordering::Acquire)
                {
                    match register(chord, callback) {
                        Ok(registration) => {
                            slots.insert(id, registration);
                        }
                        Err(error) => tracing::error!(%error, "paused resume hotkey unavailable; use UI, menu or CLI"),
                    }
                }
            })
        }) {
            tracing::error!("could not schedule paused resume hotkey registration");
        }
    }
    pub fn take_fired(&self) -> bool {
        self.fired.swap(false, Ordering::AcqRel) && self.requested.is_some()
    }
}
impl Drop for Manager {
    fn drop(&mut self) {
        self.enabled.store(false, Ordering::Release);
        let id = self.id;
        super::main_queue::run_async(move || {
            REGISTRATIONS.with(|slots| {
                slots.borrow_mut().remove(&id);
            })
        });
    }
}
