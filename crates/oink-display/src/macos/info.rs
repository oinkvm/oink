//! 显示器的附加信息：UUID（ColorSync）与 CoreDisplay 的信息字典（私有，运行时查找）。

use std::ffi::{CStr, c_char, c_int, c_void};
use std::ptr::NonNull;
use std::sync::OnceLock;

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CFUUID};
use objc2_core_graphics::CGDirectDisplayID;

#[link(name = "ColorSync", kind = "framework")]
unsafe extern "C" {
    // 声明在 ColorSync/ColorSyncDevice.h。可能返回 NULL（objc2-color-sync 的包装在 NULL 时会 panic，所以自己声明）。
    fn CGDisplayCreateUUIDFromDisplayID(display: u32) -> *mut CFUUID;
}

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

const RTLD_LAZY: c_int = 0x1;

/// 显示器 UUID 字符串（重启、重新插拔后不变）。
pub(crate) fn display_uuid(id: CGDirectDisplayID) -> Option<String> {
    // SAFETY: 任意 display ID 都可以传入，无效时返回 NULL；返回值遵循 Create 规则（+1）。
    let raw = unsafe { CGDisplayCreateUUIDFromDisplayID(id) };
    // SAFETY: 非空指针且已 +1，交给 CFRetained 管理。
    let uuid = NonNull::new(raw).map(|p| unsafe { CFRetained::from_raw(p) })?;
    CFUUID::new_string(None, Some(&uuid)).map(|s| s.to_string())
}

/// CoreDisplay 信息字典中本项目用到的字段。
#[derive(Debug, Default, Clone)]
pub(crate) struct ExtraInfo {
    pub product_name: Option<String>,
    pub uuid: Option<String>,
    pub is_virtual: Option<bool>,
}

type InfoFn = unsafe extern "C" fn(CGDirectDisplayID) -> *mut CFDictionary;

fn info_fn() -> Option<InfoFn> {
    static F: OnceLock<Option<InfoFn>> = OnceLock::new();
    *F.get_or_init(|| {
        const PATH: &CStr = c"/System/Library/Frameworks/CoreDisplay.framework/CoreDisplay";
        const SYM: &CStr = c"CoreDisplay_DisplayCreateInfoDictionary";
        // SAFETY: 系统框架路径与符号名都是以 NUL 结尾的常量字符串。
        let h = unsafe { dlopen(PATH.as_ptr(), RTLD_LAZY) };
        if h.is_null() {
            return None;
        }
        // SAFETY: 同上；符号不存在时返回 NULL。
        let p = unsafe { dlsym(h, SYM.as_ptr()) };
        if p.is_null() {
            None
        } else {
            // SAFETY: 该私有函数的签名为 `CFDictionaryRef (CGDirectDisplayID)`（被 Lunar、displayplacer 等广泛使用）。
            Some(unsafe { std::mem::transmute::<*mut c_void, InfoFn>(p) })
        }
    })
}

pub(crate) fn display_info(id: CGDirectDisplayID) -> ExtraInfo {
    let Some(f) = info_fn() else {
        return ExtraInfo::default();
    };
    // SAFETY: 见 `info_fn`；返回值遵循 Create 规则。
    let raw = unsafe { f(id) };
    // SAFETY: 非空指针且已 +1，交给 CFRetained 管理。
    let Some(dict) = NonNull::new(raw).map(|p| unsafe { CFRetained::from_raw(p) }) else {
        return ExtraInfo::default();
    };
    // SAFETY: 信息字典的键都是 CFString，值是任意 CF 对象。
    let dict: &CFDictionary<CFString, CFType> = unsafe { dict.cast_unchecked() };
    let get = |k: &str| dict.get(&CFString::from_str(k));

    let product_name = get("DisplayProductName").and_then(|v| localized_name(&v));
    let uuid = get("kCGDisplayUUID")
        .and_then(|v| v.downcast_ref::<CFString>().map(|s| s.to_string()))
        .filter(|s| !s.is_empty());
    let flag = |k: &str| get(k).and_then(|v| as_bool(&v));
    let is_virtual = match (flag("kCGDisplayIsVirtualDevice"), flag("kCGDisplayIsAirPlay")) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(false) || b.unwrap_or(false)),
    };
    ExtraInfo {
        product_name,
        uuid,
        is_virtual,
    }
}

fn as_bool(v: &CFType) -> Option<bool> {
    if let Some(b) = v.downcast_ref::<CFBoolean>() {
        return Some(b.as_bool());
    }
    v.downcast_ref::<CFNumber>().and_then(|n| n.as_i64()).map(|n| n != 0)
}

/// `DisplayProductName` 是「语言 → 名字」的字典；优先英文。
fn localized_name(v: &CFType) -> Option<String> {
    if let Some(s) = v.downcast_ref::<CFString>() {
        return Some(s.to_string());
    }
    let d = v.downcast_ref::<CFDictionary>()?;
    // SAFETY: 同上，键值都是 CFString。
    let d: &CFDictionary<CFString, CFType> = unsafe { d.cast_unchecked() };
    for k in ["en_US", "en", "en_GB"] {
        if let Some(s) = d
            .get(&CFString::from_str(k))
            .and_then(|v| v.downcast_ref::<CFString>().map(|s| s.to_string()))
        {
            return Some(s);
        }
    }
    let (_, values) = d.to_vecs();
    values
        .into_iter()
        .find_map(|v| v.downcast_ref::<CFString>().map(|s| s.to_string()))
}
