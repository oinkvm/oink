//! 判断事件来自哪一类设备（「设备识别」，尽力而为）。
//!
//! 1. 私有 `CGEventCopyIOHIDEvent` 取出事件附带的 IOHIDEvent，`IOHIDEventGetSenderID` 得到发送者，
//!    它就是 IOHIDEventService 的 IORegistry entry ID；
//! 2. 用 `IORegistryEntryIDMatching` 找到这个服务，按类名 / Product / PrimaryUsage 判断类型；
//! 3. 结果按 sender ID 缓存在截获线程的线程局部表里（回调只在截获线程上执行，无需加锁）。
//!
//! 取不到 IOHIDEvent（其他软件合成的事件、部分蓝牙设备）时返回 `None`，由调用方按事件字段推断。

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::sync::OnceLock;

use objc2_core_foundation::{CFNumber, CFRetained, CFString, CFType};
use objc2_core_graphics::CGEvent;
use oink_foundation::input::SourceKind;

use super::system::find_symbol;

type CopyHidEventFn = unsafe extern "C" fn(*const CGEvent) -> *const c_void;
type GetSenderIdFn = unsafe extern "C" fn(*const c_void) -> u64;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegistryEntryIDMatching(entry_id: u64) -> *mut c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
    fn IOObjectGetClass(object: u32, class_name: *mut c_char) -> i32;
    fn IORegistryEntrySearchCFProperty(
        entry: u32,
        plane: *const c_char,
        key: *const CFString,
        allocator: *const c_void,
        options: u32,
    ) -> *const CFType;
    fn IOObjectRelease(object: u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

const K_IO_REGISTRY_ITERATE_RECURSIVELY: u32 = 1;
const K_IO_REGISTRY_ITERATE_PARENTS: u32 = 2;

/// 一个输入设备的描述（诊断输出用）。
#[derive(Clone, Debug)]
pub(crate) struct DeviceDesc {
    pub kind: SourceKind,
    pub class: String,
    pub product: Option<String>,
}

fn fns() -> Option<(CopyHidEventFn, GetSenderIdFn)> {
    static F: OnceLock<Option<(CopyHidEventFn, GetSenderIdFn)>> = OnceLock::new();
    *F.get_or_init(|| {
        let copy = find_symbol(c"CGEventCopyIOHIDEvent");
        let sender = find_symbol(c"IOHIDEventGetSenderID");
        if copy.is_null() || sender.is_null() {
            tracing::warn!("找不到 CGEventCopyIOHIDEvent / IOHIDEventGetSenderID，无法按设备区分来源");
            return None;
        }
        // SAFETY: 私有函数签名：`IOHIDEventRef CGEventCopyIOHIDEvent(CGEventRef)`、
        // `uint64_t IOHIDEventGetSenderID(IOHIDEventRef)`。
        unsafe {
            Some((
                std::mem::transmute::<*mut c_void, CopyHidEventFn>(copy),
                std::mem::transmute::<*mut c_void, GetSenderIdFn>(sender),
            ))
        }
    })
}

/// 事件的发送者 ID；取不到时为 `None`。
pub(crate) fn sender_id(event: &CGEvent) -> Option<u64> {
    let (copy, get) = fns()?;
    // SAFETY: event 有效；返回的 IOHIDEvent 遵循 Copy 规则，用完释放。
    unsafe {
        let hid = copy(event);
        if hid.is_null() {
            return None;
        }
        let id = get(hid);
        CFRelease(hid);
        (id != 0).then_some(id)
    }
}

thread_local! {
    static CACHE: RefCell<HashMap<u64, DeviceDesc>> = RefCell::new(HashMap::new());
}

/// 按 sender ID 查设备（线程局部缓存）。第一次见到某个设备时以 debug 级别记一条日志。
pub(crate) fn describe(sender: u64) -> DeviceDesc {
    if let Some(d) = CACHE.with(|c| c.borrow().get(&sender).cloned()) {
        return d;
    }
    let d = lookup(sender);
    tracing::debug!(sender = format!("{sender:#x}"), class = %d.class, product = ?d.product, kind = ?d.kind, "新的输入设备");
    CACHE.with(|c| c.borrow_mut().insert(sender, d.clone()));
    d
}

fn lookup(sender: u64) -> DeviceDesc {
    let unknown = DeviceDesc {
        kind: SourceKind::Unknown,
        class: String::new(),
        product: None,
    };
    // SAFETY: IOKit 调用；matching 字典被 IOServiceGetMatchingService 消耗；service 用完释放。
    unsafe {
        let matching = IORegistryEntryIDMatching(sender);
        if matching.is_null() {
            return unknown;
        }
        let service = IOServiceGetMatchingService(0, matching);
        if service == 0 {
            return unknown;
        }
        let mut buf = [0 as c_char; 128];
        let class = if IOObjectGetClass(service, buf.as_mut_ptr()) == 0 {
            CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        } else {
            String::new()
        };
        let search = |key: &str| -> Option<CFRetained<CFType>> {
            let k = CFString::from_str(key);
            let p = IORegistryEntrySearchCFProperty(
                service,
                c"IOService".as_ptr(),
                &*k,
                std::ptr::null(),
                K_IO_REGISTRY_ITERATE_RECURSIVELY | K_IO_REGISTRY_ITERATE_PARENTS,
            );
            std::ptr::NonNull::new(p as *mut CFType).map(|p| CFRetained::from_raw(p))
        };
        let product = search("Product").and_then(|v| v.downcast_ref::<CFString>().map(|s| s.to_string()));
        let num = |key: &str| search(key).and_then(|v| v.downcast_ref::<CFNumber>().and_then(|n| n.as_i64()));
        let usage = (num("PrimaryUsagePage"), num("PrimaryUsage"));
        IOObjectRelease(service);
        DeviceDesc {
            kind: classify(&class, product.as_deref(), usage),
            class,
            product,
        }
    }
}

/// 按类名、产品名、主用途判断设备类型。
pub(crate) fn classify(class: &str, product: Option<&str>, usage: (Option<i64>, Option<i64>)) -> SourceKind {
    let c = class.to_ascii_lowercase();
    let p = product.unwrap_or("").to_ascii_lowercase();
    if c.contains("trackpad") || c.contains("touchpad") {
        return SourceKind::Touchpad;
    }
    if c.contains("mouse") || c.contains("pointing") {
        return SourceKind::Mouse;
    }
    if c.contains("keyboard") {
        return SourceKind::Keyboard;
    }
    match usage {
        (Some(0x0D), Some(0x05)) => SourceKind::Touchpad,
        (Some(0x01), Some(0x02)) if p.contains("trackpad") || p.contains("touchpad") => SourceKind::Touchpad,
        (Some(0x01), Some(0x01 | 0x02)) => SourceKind::Mouse,
        (Some(0x01), Some(0x06 | 0x07)) => SourceKind::Keyboard,
        _ => SourceKind::Unknown,
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOServiceMatching(name: *const c_char) -> *mut c_void;
        fn IOServiceGetMatchingServices(main_port: u32, matching: *mut c_void, iter: *mut u32) -> i32;
        fn IOIteratorNext(iter: u32) -> u32;
        fn IORegistryEntryGetRegistryEntryID(entry: u32, id: *mut u64) -> i32;
    }

    /// 用本机真实的 HID 事件服务验证「registry entry ID → 服务 → 分类」这条查找路径。
    #[test]
    fn lookup_resolves_real_hid_services() {
        let mut ids = Vec::new();
        // SAFETY: 标准的 IOKit 枚举；matching 字典被消耗，迭代器与服务用完释放。
        unsafe {
            let mut iter = 0u32;
            if IOServiceGetMatchingServices(0, IOServiceMatching(c"IOHIDEventService".as_ptr()), &mut iter) != 0 {
                return;
            }
            loop {
                let s = IOIteratorNext(iter);
                if s == 0 {
                    break;
                }
                let mut id = 0u64;
                if IORegistryEntryGetRegistryEntryID(s, &mut id) == 0 {
                    ids.push(id);
                }
                IOObjectRelease(s);
            }
            IOObjectRelease(iter);
        }
        for id in ids {
            let d = lookup(id);
            assert!(!d.class.is_empty(), "registry ID {id:#x} 没有解析到服务");
            if d.kind != SourceKind::Unknown {
                eprintln!("{id:#x} {} {:?} -> {:?}", d.class, d.product, d.kind);
            }
            if d.class.contains("Trackpad") {
                assert_eq!(d.kind, SourceKind::Touchpad);
            }
        }
    }

    #[test]
    fn classify_known_drivers() {
        let t = classify(
            "AppleMultitouchTrackpadHIDEventDriver",
            Some("Apple Internal Keyboard / Trackpad"),
            (Some(1), Some(2)),
        );
        assert_eq!(t, SourceKind::Touchpad);
        assert_eq!(
            classify(
                "AppleMultitouchMouseHIDEventDriver",
                Some("Magic Mouse"),
                (Some(1), Some(2))
            ),
            SourceKind::Mouse
        );
        assert_eq!(
            classify("IOHIDEventDriver", Some("USB Optical Mouse"), (Some(1), Some(2))),
            SourceKind::Mouse
        );
        assert_eq!(
            classify("AppleHIDKeyboardEventDriverV2", None, (Some(1), Some(6))),
            SourceKind::Keyboard
        );
        assert_eq!(
            classify("IOHIDEventDriver", None, (Some(0x0D), Some(5))),
            SourceKind::Touchpad
        );
        assert_eq!(classify("Foo", None, (None, None)), SourceKind::Unknown);
    }
}
