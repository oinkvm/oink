//! Caps Lock 的普通按键注入及 Remote 本机状态保护。
//!
//! 注入普通 `FlagsChanged(kVK_CapsLock)` 不改变 HID 锁定位，也不能触发系统的 Caps 切源行为。
//! 普通锁定采用 IOKit `IOHIDSetModifierLockState`（包含 LED），字母的 AlphaShift 从这个状态读取。
//! 目标启用「使用大写锁定键切换 ABC 输入法」时，按收到的 DOWN / UP 顺序投递原生
//! `NX_SYSDEFINED / NX_KEYTYPE_CAPS_LOCK`，保留实际间隔，由系统处理短按、长按、关锁恢复。
//! macOS 14.7.8 / 26.6.2 已验证：只设置 IOKit 锁定位不能表达完整长按行为，拼音下字母可能仍是小写；
//! TIS 选源也不能替代原生手势的前台文本客户端激活。
//!
//! 焦点移交不读取或设置 Caps 锁定位及输入源。
//! 本机的 TIS 调用仅服务普通 Caps 按键的本机设置、源端守卫及本机探针；不能在后台线程调用。
//! 2026-09-29 界面程序后台调用 TIS 曾触发 HIToolbox 主线程断言并被 SIGTRAP 终止，因此本机行为
//! 设置缓存和守卫通过主队列异步执行，每个 TIS 入口拒绝后台调用。主队列未运行时按普通锁定处理。
//! Read settings through dynamically resolved `TISIsRomanSwitchEnabled` / `TISIsRomanSwitchAllowed`,
//! then global `TISRomanSwitchState`; an unknown setting uses ordinary Caps Lock.
//!
//! [`LocalGuard`] 防止源 Mac 的 HID 层在 event tap 吞键之前意外改变本机状态。进入 Remote 记录本机
//! Caps 与输入源；吞掉 Caps 后 1.5 秒内核对并恢复。回到 Local 时仅在窗口内最后核对，不无条件恢复。
//! 此保护不把本机 Caps 或输入源同步到任何设备。

use std::sync::atomic::{AtomicBool, AtomicI8, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::main_queue;

/// 诊断程序使用的原生长按时长参考值；实际阈值由系统输入法框架决定。
pub(crate) const LONG_PRESS: Duration = Duration::from_millis(500);

/// 源端吞掉 Caps Lock 之后，多长时间内持续核对并恢复本机状态（覆盖 HID 层延迟生效的锁定）。
const GUARD_WINDOW: Duration = Duration::from_millis(1500);

// ---------------------------------------------------------------------------------------------
// 纯逻辑：状态机与设置解析
// ---------------------------------------------------------------------------------------------

/// 目标机上 Caps Lock 的行为。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Behavior {
    /// 普通大写锁定：按下即切换。
    Lock,
    /// 「使用大写锁定键切换 ABC 输入法」：短按切换输入法，长按切换大写锁定。
    SwitchInputSource,
}

/// 目标端一次转换所要求的动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    None,
    SetLock(bool),
    NativeCaps(bool),
}

/// 只记住这一轮按压使用的路径，重复 DOWN 不重复注入；UP 必须与 DOWN 配对。
#[derive(Debug, Default)]
pub(crate) struct Machine {
    held: Option<Behavior>,
}

impl Machine {
    pub(crate) fn press(&mut self, behavior: Behavior, lock_on: bool) -> Action {
        if self.held.is_some() {
            return Action::None;
        }
        self.held = Some(behavior);
        match behavior {
            // OS workaround: posted NX bypasses the physical HID lock filter; with switching OFF, upper layers do not toggle.
            // 2026-10-12, macOS 14.7.8: 100/700 ms NX presses left lock/AlphaShift OFF and letters lowercase.
            // Keep IOKit for ordinary lock mode; switch mode needs native DOWN/UP at their real times for the full gesture.
            Behavior::Lock => Action::SetLock(!lock_on),
            Behavior::SwitchInputSource => Action::NativeCaps(true),
        }
    }

    pub(crate) fn release(&mut self) -> Action {
        match self.held.take() {
            Some(Behavior::SwitchInputSource) => Action::NativeCaps(false),
            Some(Behavior::Lock) | None => Action::None,
        }
    }
}

/// 偏好里的原始值（便于单元测试）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PrefValue {
    Absent,
    Bool(bool),
    Int(i64),
    Other,
}

/// 各来源读到的「用大写锁定键切换输入法」设置。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SwitchSetting {
    /// 私有 `TISIsRomanSwitchEnabled()` 的结果（找不到函数为 `None`）。
    pub enabled_fn: Option<bool>,
    /// 私有 `TISIsRomanSwitchAllowed()` 的结果（找不到函数为 `None`）。
    pub allowed_fn: Option<bool>,
    /// Global preference `TISRomanSwitchState`.
    pub pref: Option<bool>,
    /// 是否存在可以切回的非 ABC 输入法（当前输入法本身不是 ABC 也算）。
    pub has_non_roman: bool,
}

pub(crate) fn parse_pref(v: PrefValue) -> Option<bool> {
    match v {
        PrefValue::Bool(b) => Some(b),
        PrefValue::Int(n) => Some(n != 0),
        PrefValue::Absent | PrefValue::Other => None,
    }
}

/// 由设置决定行为。
pub(crate) fn decide(s: SwitchSetting) -> Behavior {
    // 没有可切换的输入法（只启用了 ABC 等拉丁布局）时，系统也按普通大写锁定处理。
    if !s.has_non_roman || s.allowed_fn == Some(false) {
        return Behavior::Lock;
    }
    // TIS reports the system default even when no preference has been written.
    // If neither source is available, keep both tap and hold as ordinary Caps Lock.
    let on = s.enabled_fn.or(s.pref).unwrap_or(false);
    if on {
        Behavior::SwitchInputSource
    } else {
        Behavior::Lock
    }
}

// ---------------------------------------------------------------------------------------------
// 系统调用
// ---------------------------------------------------------------------------------------------

/// IOKit：HID 系统的大写锁定状态。
pub(crate) mod hid {
    use std::ffi::{c_char, c_void};
    use std::sync::OnceLock;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOServiceMatching(name: *const c_char) -> *mut c_void;
        fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
        fn IOServiceOpen(service: u32, owning_task: u32, ty: u32, connect: *mut u32) -> i32;
        fn IOObjectRelease(object: u32) -> i32;
        fn IOHIDGetModifierLockState(handle: u32, selector: i32, state: *mut bool) -> i32;
        fn IOHIDSetModifierLockState(handle: u32, selector: i32, state: bool) -> i32;
    }

    unsafe extern "C" {
        static mach_task_self_: u32;
    }

    const K_IOHID_PARAM_CONNECT_TYPE: u32 = 1;
    const K_IOHID_CAPS_LOCK_STATE: i32 = 1;

    /// IOHIDSystem 的连接（进程内共用，不关闭）。打不开时为 `None`。
    pub(crate) fn connection() -> Option<u32> {
        static C: OnceLock<Option<u32>> = OnceLock::new();
        *C.get_or_init(|| {
            // SAFETY: 标准的 IOKit 调用序列；IOServiceGetMatchingService 消耗 matching 字典。
            unsafe {
                let matching = IOServiceMatching(c"IOHIDSystem".as_ptr());
                if matching.is_null() {
                    return None;
                }
                let service = IOServiceGetMatchingService(0, matching);
                if service == 0 {
                    tracing::warn!("找不到 IOHIDSystem，无法设置大写锁定");
                    return None;
                }
                let mut conn = 0u32;
                let kr = IOServiceOpen(service, mach_task_self_, K_IOHID_PARAM_CONNECT_TYPE, &mut conn);
                IOObjectRelease(service);
                if kr != 0 || conn == 0 {
                    tracing::warn!(kr, "打开 IOHIDSystem 失败，无法设置大写锁定");
                    return None;
                }
                Some(conn)
            }
        })
    }

    /// 当前的大写锁定状态。
    pub fn caps_lock() -> Option<bool> {
        let conn = connection()?;
        let mut state = false;
        // SAFETY: conn 是打开的 IOHIDSystem 连接；state 是有效的输出指针。
        let kr = unsafe { IOHIDGetModifierLockState(conn, K_IOHID_CAPS_LOCK_STATE, &mut state) };
        if kr == 0 {
            crate::diagnostics::caps(state);
        }
        (kr == 0).then_some(state)
    }

    /// 设置大写锁定（含指示灯）。
    pub fn set_caps_lock(on: bool) -> Result<(), String> {
        let conn = connection().ok_or_else(|| "IOHIDSystem 不可用".to_string())?;
        // SAFETY: conn 是打开的 IOHIDSystem 连接。
        let kr = unsafe { IOHIDSetModifierLockState(conn, K_IOHID_CAPS_LOCK_STATE, on) };
        if kr == 0 {
            crate::diagnostics::caps(on);
            Ok(())
        } else {
            Err(format!("IOHIDSetModifierLockState 失败：{kr:#x}"))
        }
    }
}

/// Text Input Sources：输入法的读取与切换。
pub(crate) mod tis {
    use std::ffi::c_void;
    use std::ptr::NonNull;

    use objc2_core_foundation::{
        CFBoolean, CFNumber, CFPreferencesCopyValue, CFPreferencesSynchronize, CFRetained, CFString, CFType,
        kCFPreferencesAnyApplication, kCFPreferencesAnyHost, kCFPreferencesCurrentUser,
    };

    use super::super::main_queue::is_main_thread;
    use super::super::system::find_symbol;
    use super::{PrefValue, SwitchSetting, parse_pref};

    /// TIS 只能在主线程调用（`main_queue` 模块说明）。其他线程上拒绝调用（返回「读不到」），记一次错误日志：
    /// 调用方应当经 `main_queue` 把工作放到主线程，走到这里说明有遗漏，但不能因此让进程崩溃。
    fn on_main(what: &str) -> bool {
        if is_main_thread() {
            return true;
        }
        static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !LOGGED.swap(true, std::sync::atomic::Ordering::AcqRel) {
            tracing::error!(what, "TIS 只能在主线程调用，已拒绝（输入法不读取、不切换）");
        }
        false
    }

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn TISCopyCurrentKeyboardInputSource() -> *mut CFType;
        fn TISGetInputSourceProperty(source: *const CFType, key: *const CFString) -> *const CFType;
        fn TISSelectInputSource(source: *const CFType) -> i32;
        fn TISCreateInputSourceList(properties: *const c_void, include_all_installed: u8) -> *mut CFType;
        static kTISPropertyInputSourceID: *const CFString;
        static kTISPropertyInputSourceType: *const CFString;
        static kTISPropertyInputSourceCategory: *const CFString;
        static kTISPropertyInputSourceIsASCIICapable: *const CFString;
        static kTISPropertyInputSourceIsSelectCapable: *const CFString;
        static kTISPropertyInputSourceIsEnabled: *const CFString;
        static kTISTypeKeyboardLayout: *const CFString;
        static kTISCategoryKeyboardInputSource: *const CFString;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFArrayGetCount(array: *const CFType) -> isize;
        fn CFArrayGetValueAtIndex(array: *const CFType, index: isize) -> *const CFType;
    }

    type BoolFn = unsafe extern "C" fn() -> u8;

    /// 一个输入源（持有引用）。
    pub struct Source(CFRetained<CFType>);

    impl Source {
        /// 接管一个 Copy/Create 规则返回的引用。
        fn from_owned(p: *mut CFType) -> Option<Self> {
            // SAFETY: 调用方保证 p 是 +1 引用（或空）。
            NonNull::new(p).map(|p| Self(unsafe { CFRetained::from_raw(p) }))
        }

        /// 借用一个 +0 引用并保留它。
        fn from_borrowed(p: *const CFType) -> Option<Self> {
            // SAFETY: p 在调用期间有效（数组元素）；retain 后独立持有。
            NonNull::new(p as *mut CFType).map(|p| Self(unsafe { CFRetained::retain(p) }))
        }

        fn prop(&self, key: *const CFString) -> Option<&CFType> {
            if !on_main("TISGetInputSourceProperty") {
                return None;
            }
            // SAFETY: self.0 是有效的 TISInputSource；返回值遵循 Get 规则，生命周期不超过 self。
            unsafe { TISGetInputSourceProperty(&*self.0, key).as_ref() }
        }

        fn string(&self, key: *const CFString) -> Option<String> {
            self.prop(key)?.downcast_ref::<CFString>().map(|s| s.to_string())
        }

        fn flag(&self, key: *const CFString) -> bool {
            self.prop(key)
                .and_then(|v| v.downcast_ref::<CFBoolean>())
                .is_some_and(|b| b.as_bool())
        }

        pub fn id(&self) -> String {
            // SAFETY: 读取 HIToolbox 导出的常量。
            self.string(unsafe { kTISPropertyInputSourceID }).unwrap_or_default()
        }

        /// 「ABC 一侧」：ASCII 键盘布局（ABC、U.S. 等）。
        pub fn is_roman(&self) -> bool {
            // SAFETY: 读取 HIToolbox 导出的常量。
            let (ty, ascii, layout) = unsafe {
                (
                    kTISPropertyInputSourceType,
                    kTISPropertyInputSourceIsASCIICapable,
                    kTISTypeKeyboardLayout,
                )
            };
            // SAFETY: 常量指向有效的 CFString。
            let layout = unsafe { &*layout }.to_string();
            self.flag(ascii) && self.string(ty).is_some_and(|t| t == layout)
        }

        /// 可以选中的键盘输入源（键盘布局或输入法的输入模式）。
        pub fn is_selectable_keyboard(&self) -> bool {
            // SAFETY: 读取 HIToolbox 导出的常量。
            let (cat, kb, sel, en) = unsafe {
                (
                    kTISPropertyInputSourceCategory,
                    kTISCategoryKeyboardInputSource,
                    kTISPropertyInputSourceIsSelectCapable,
                    kTISPropertyInputSourceIsEnabled,
                )
            };
            // SAFETY: 常量指向有效的 CFString。
            let kb = unsafe { &*kb }.to_string();
            self.string(cat).is_some_and(|c| c == kb) && self.flag(sel) && self.flag(en)
        }

        fn is_keyboard_candidate(&self) -> bool {
            // SAFETY: 读取 HIToolbox 导出的常量。
            let (cat, kb, sel, en, ascii) = unsafe {
                (
                    kTISPropertyInputSourceCategory,
                    kTISCategoryKeyboardInputSource,
                    kTISPropertyInputSourceIsSelectCapable,
                    kTISPropertyInputSourceIsEnabled,
                    kTISPropertyInputSourceIsASCIICapable,
                )
            };
            // SAFETY: 常量指向有效的 CFString。
            let kb = unsafe { &*kb }.to_string();
            self.string(cat).is_some_and(|c| c == kb) && self.flag(sel) && self.flag(en) && !self.flag(ascii)
        }

        pub fn select(&self) -> Result<(), String> {
            if !on_main("TISSelectInputSource") {
                return Err("不在主线程".into());
            }
            // SAFETY: self.0 是有效的 TISInputSource。
            let st = unsafe { TISSelectInputSource(&*self.0) };
            if st == 0 {
                Ok(())
            } else {
                Err(format!("TISSelectInputSource 失败：{st}"))
            }
        }
    }

    pub fn current() -> Option<Source> {
        if !on_main("TISCopyCurrentKeyboardInputSource") {
            return None;
        }
        // SAFETY: Copy 规则的查询函数。
        Source::from_owned(unsafe { TISCopyCurrentKeyboardInputSource() })
    }

    /// 已启用的输入源（不含未启用的已安装输入源）。
    fn enabled() -> Vec<Source> {
        if !on_main("TISCreateInputSourceList") {
            return Vec::new();
        }
        // SAFETY: Create 规则；NULL 过滤条件表示全部已启用的输入源。
        let Some(list) = Source::from_owned(unsafe { TISCreateInputSourceList(std::ptr::null(), 0) }) else {
            return Vec::new();
        };
        // SAFETY: list 是 CFArray。
        let n = unsafe { CFArrayGetCount(&*list.0) };
        (0..n)
            // SAFETY: 下标在范围内。
            .filter_map(|i| Source::from_borrowed(unsafe { CFArrayGetValueAtIndex(&*list.0, i) }))
            .collect()
    }

    pub fn by_id(id: &str) -> Option<Source> {
        enabled().into_iter().find(|s| s.id() == id)
    }

    /// 已启用、可以选中的键盘输入源：(ID, 是否在 ABC 一侧)。
    pub fn selectable_keyboard_sources() -> Vec<(String, bool)> {
        enabled()
            .into_iter()
            .filter(Source::is_selectable_keyboard)
            .map(|s| (s.id(), s.is_roman()))
            .collect()
    }

    fn private_bool(name: &std::ffi::CStr) -> Option<bool> {
        if !on_main("TIS private") {
            return None;
        }
        let f = find_symbol(name);
        if f.is_null() {
            return None;
        }
        // SAFETY: 私有函数签名 `Boolean f(void)`。
        Some(unsafe { std::mem::transmute::<*mut c_void, BoolFn>(f)() } != 0)
    }

    fn pref() -> PrefValue {
        // macOS 14.7.8 stores this in .GlobalPreferences, not com.apple.HIToolbox.
        // macOS 26.6.2 may omit it and use the default returned by TIS above.
        // Read the current user's global domain, independent of the backend bundle ID;
        // this query requires no Accessibility or Input Monitoring permission.
        // SAFETY: CoreFoundation exports these process-lifetime preference constants.
        let (app, user, host) = unsafe {
            (
                kCFPreferencesAnyApplication,
                kCFPreferencesCurrentUser,
                kCFPreferencesAnyHost,
            )
        };
        CFPreferencesSynchronize(app, user, host);
        match CFPreferencesCopyValue(&CFString::from_str("TISRomanSwitchState"), app, user, host) {
            None => PrefValue::Absent,
            Some(v) => {
                if let Some(b) = v.downcast_ref::<CFBoolean>() {
                    PrefValue::Bool(b.as_bool())
                } else if let Some(n) = v.downcast_ref::<CFNumber>().and_then(|n| n.as_i64()) {
                    PrefValue::Int(n)
                } else {
                    PrefValue::Other
                }
            }
        }
    }

    /// 读取「使用大写锁定键切换 ABC 输入法」的各来源。
    pub fn switch_setting() -> SwitchSetting {
        let has_non_roman =
            current().is_some_and(|s| !s.is_roman()) || enabled().into_iter().any(|s| s.is_keyboard_candidate());
        SwitchSetting {
            enabled_fn: private_bool(c"TISIsRomanSwitchEnabled"),
            allowed_fn: private_bool(c"TISIsRomanSwitchAllowed"),
            pref: parse_pref(pref()),
            has_non_roman,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 主线程上的输入法缓存（读取不调用 TIS）
// ---------------------------------------------------------------------------------------------

/// 主线程维护的输入法状态。其他线程只读这份缓存，不调用 TIS（`main_queue` 模块说明）。
#[derive(Clone, Debug, Default)]
struct Cache {
    /// 主线程填写过（主线程在服务主队列）。
    filled: bool,
    setting: SwitchSetting,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache {
    filled: false,
    setting: SwitchSetting {
        enabled_fn: None,
        allowed_fn: None,
        pref: None,
        has_non_roman: false,
    },
});

/// 已经排队、还没执行的刷新（避免重复排队）。
static REFRESH_QUEUED: AtomicBool = AtomicBool::new(false);
static SOURCE_SWITCHES_INPUT: AtomicBool = AtomicBool::new(false);
static SOURCE_SWITCH_OBSERVED: AtomicBool = AtomicBool::new(false);

pub(crate) fn source_switches_input() -> Option<bool> {
    SOURCE_SWITCH_OBSERVED
        .load(Ordering::Acquire)
        .then(|| SOURCE_SWITCHES_INPUT.load(Ordering::Acquire))
}

fn cache() -> Cache {
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// 只缓存本机 Caps 按键行为；焦点移交不读取或报告输入源。
fn refresh_now() {
    if main_queue::is_main_thread() {
        let setting = tis::switch_setting();
        SOURCE_SWITCHES_INPUT.store(decide(setting) == Behavior::SwitchInputSource, Ordering::Release);
        SOURCE_SWITCH_OBSERVED.store(true, Ordering::Release);
        *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Cache { filled: true, setting };
    }
}

/// 请主线程刷新缓存（已经有一个在排队时不再排）。
pub(crate) fn request_refresh() {
    if REFRESH_QUEUED.swap(true, Ordering::AcqRel) {
        return;
    }
    if !main_queue::run_async(|| {
        REFRESH_QUEUED.store(false, Ordering::Release);
        refresh_now();
    }) {
        REFRESH_QUEUED.store(false, Ordering::Release);
    }
}

/// 输入法变化的系统通知（`kTISNotifySelectedKeyboardInputSourceChanged` 等，分布式通知）。
mod notify {
    use std::ffi::c_void;

    use objc2_core_foundation::CFString;

    use super::super::main_queue;

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        static kTISNotifySelectedKeyboardInputSourceChanged: *const CFString;
        static kTISNotifyEnabledKeyboardInputSourcesChanged: *const CFString;
    }

    type Callback = extern "C" fn(*mut c_void, *mut c_void, *const c_void, *const c_void, *const c_void);

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFNotificationCenterGetDistributedCenter() -> *mut c_void;
        fn CFNotificationCenterAddObserver(
            center: *mut c_void,
            observer: *const c_void,
            callback: Callback,
            name: *const c_void,
            object: *const c_void,
            suspension_behavior: isize,
        );
    }

    /// `CFNotificationSuspensionBehaviorDeliverImmediately`。
    const DELIVER_IMMEDIATELY: isize = 4;

    /// 观察者标识（任意非空、进程内唯一的地址）。
    static OBSERVER: u8 = 0;

    extern "C" fn changed(_: *mut c_void, _: *mut c_void, _: *const c_void, _: *const c_void, _: *const c_void) {
        // 通知在注册线程（主线程）的 run loop 上投递；不是主线程时交给主线程。
        if main_queue::is_main_thread() {
            super::refresh_now();
        }
        // 输入法框架自己的进程内缓存可能在我们之后才处理这条通知：稍后再读一次。
        main_queue::run_after(std::time::Duration::from_millis(150), super::refresh_now);
    }

    /// 注册通知（在主线程上调用，回调也在主线程的 run loop 上执行）。
    pub(super) fn observe() {
        // SAFETY: 分布式通知中心是进程内单例；name 是 HIToolbox 导出的 CFString 常量；OBSERVER 是
        // 'static 地址，进程结束前不移除。
        unsafe {
            let center = CFNotificationCenterGetDistributedCenter();
            if center.is_null() {
                tracing::warn!("取不到分布式通知中心：输入法缓存只在本进程切换时更新");
                return;
            }
            for name in [
                kTISNotifySelectedKeyboardInputSourceChanged,
                kTISNotifyEnabledKeyboardInputSourcesChanged,
            ] {
                CFNotificationCenterAddObserver(
                    center,
                    &raw const OBSERVER as *const c_void,
                    changed,
                    name as *const c_void,
                    std::ptr::null(),
                    DELIVER_IMMEDIATELY,
                );
            }
        }
    }
}

/// 启动输入法缓存：请主线程读一次并订阅变化通知（只做一次，任意线程可调用，不等待）。
/// backend 创建时调用；主线程不服务主队列时采用普通锁定行为。
pub(crate) fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        main_queue::run_async(|| {
            refresh_now();
            notify::observe();
        });
    });
}

// ---------------------------------------------------------------------------------------------
// 目标端：注入
// ---------------------------------------------------------------------------------------------

/// 诊断 / 测试用：强制行为（0 = 按系统设置，1 = Lock，2 = SwitchInputSource）。
static BEHAVIOR_OVERRIDE: AtomicU8 = AtomicU8::new(0);

pub(crate) fn set_behavior_override(b: Option<Behavior>) {
    let v = match b {
        None => 0,
        Some(Behavior::Lock) => 1,
        Some(Behavior::SwitchInputSource) => 2,
    };
    BEHAVIOR_OVERRIDE.store(v, Ordering::Release);
}

/// 注入的 Caps Lock 采用的行为（任意线程，读缓存）。缓存还没有填好（主线程不服务主队列）时按普通大写
/// 锁定处理。
pub(crate) fn current_behavior() -> Behavior {
    match BEHAVIOR_OVERRIDE.load(Ordering::Acquire) {
        1 => Behavior::Lock,
        2 => Behavior::SwitchInputSource,
        _ => {
            init();
            let c = cache();
            if !c.filled {
                static LOGGED: AtomicBool = AtomicBool::new(false);
                if !LOGGED.swap(true, Ordering::AcqRel) {
                    tracing::warn!("输入法设置还没有读到（主线程没有服务主队列？）：Caps Lock 按普通大写锁定处理");
                }
                return Behavior::Lock;
            }
            decide(c.setting)
        }
    }
}

/// 目标端的 Caps Lock（注入器持有）。
#[derive(Clone, Default)]
pub(crate) struct CapsInjector {
    machine: Arc<Mutex<Machine>>,
}

impl CapsInjector {
    /// 处理一个注入的 Caps Lock 按下 / 抬起（任意线程；原生切源事件与普通键一样有序投递）。
    pub(crate) fn key(&self, pressed: bool) {
        let lock_on = hid::caps_lock().unwrap_or(false);
        let action = {
            let mut m = self.machine.lock().unwrap_or_else(|e| e.into_inner());
            if pressed {
                // 重复的按下直接忽略，不必读设置。
                if m.held.is_some() {
                    return;
                }
                let behavior = current_behavior();
                // 设置可能改过（没有通知）：为下一次按键刷新缓存。
                request_refresh();
                m.press(behavior, lock_on)
            } else {
                m.release()
            }
        };
        tracing::debug!(pressed, caps_on = lock_on, ?action, "Caps Lock target transition");
        self.perform(action);
    }

    fn perform(&self, action: Action) {
        match action {
            Action::None => {}
            Action::SetLock(on) => match hid::set_caps_lock(on) {
                Ok(()) => tracing::debug!(on, "大写锁定"),
                Err(e) => tracing::warn!(%e, "设置大写锁定失败"),
            },
            Action::NativeCaps(pressed) => {
                if super::sysevent::post_caps_transition(pressed) {
                    tracing::debug!(pressed, "Caps Lock posted native transition");
                } else {
                    tracing::warn!(pressed, "Caps Lock native event creation failed");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 源端：Remote 模式下保护本机的锁定状态与输入法
// ---------------------------------------------------------------------------------------------

const UNKNOWN: i8 = -1;

/// 截获线程使用的本机键盘状态保护（见模块文档）。锁定状态在调用线程上读写（IOKit）；输入法的记录、核对
/// 与恢复都在主线程上执行（TIS），经主队列按调用顺序排队。
pub(crate) struct LocalGuard {
    /// 进入 Remote 时本机的大写锁定（-1：没有记录）。
    saved_lock: AtomicI8,
    /// 进入 Remote 时本机的输入法（只在主线程上读写）。
    saved_source: Mutex<Option<String>>,
    /// 吞掉 Caps Lock 后核对到这个时刻（进程内单调时钟，微秒；0 = 不核对）。
    until_us: AtomicU64,
    /// 已经有一次输入法核对在主队列里排队。
    source_check_queued: AtomicBool,
}

fn mono_us() -> u64 {
    static E: OnceLock<Instant> = OnceLock::new();
    // +1：保证非 0。
    E.get_or_init(Instant::now).elapsed().as_micros() as u64 + 1
}

impl LocalGuard {
    pub(crate) const fn new() -> Self {
        Self {
            saved_lock: AtomicI8::new(UNKNOWN),
            saved_source: Mutex::new(None),
            until_us: AtomicU64::new(0),
            source_check_queued: AtomicBool::new(false),
        }
    }

    /// 从 Local 进入 Remote：记下锁定状态（很快，可以在回调里调用）。输入法由 [`Self::snapshot_source`] 记。
    pub(crate) fn enter_remote(&self) {
        let v = hid::caps_lock().map_or(UNKNOWN, i8::from);
        self.saved_lock.store(v, Ordering::Release);
        self.until_us.store(0, Ordering::Release);
    }

    /// 记下本机当前的输入法（交给主线程，不等待）。
    pub(crate) fn snapshot_source(&'static self) {
        main_queue::run_async(move || {
            let id = tis::current().map(|s| s.id());
            *self.saved_source.lock().unwrap_or_else(|e| e.into_inner()) = id;
        });
    }

    /// Remote 模式下吞掉了一个 Caps Lock 事件。
    pub(crate) fn caps_swallowed(&self) {
        self.until_us
            .store(mono_us() + GUARD_WINDOW.as_micros() as u64, Ordering::Release);
    }

    /// 是否仍在核对窗口内。
    pub(crate) fn active(&self) -> bool {
        let until = self.until_us.load(Ordering::Acquire);
        until != 0 && mono_us() <= until
    }

    /// 核对并恢复（在截获线程的循环里调用，不能在回调里）。锁定状态立即核对；输入法交给主线程核对。
    pub(crate) fn enforce(&'static self) {
        let saved = self.saved_lock.load(Ordering::Acquire);
        if saved != UNKNOWN {
            let want = saved == 1;
            if hid::caps_lock().is_some_and(|cur| cur != want) {
                tracing::info!(want, "恢复本机的大写锁定状态（Caps Lock 已发往其他电脑）");
                if let Err(e) = hid::set_caps_lock(want) {
                    tracing::warn!(%e, "恢复大写锁定失败");
                }
            }
        }
        if self.source_check_queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let queued = main_queue::run_async(move || {
            self.source_check_queued.store(false, Ordering::Release);
            self.enforce_source_on_main();
        });
        if !queued {
            self.source_check_queued.store(false, Ordering::Release);
        }
    }

    fn enforce_source_on_main(&self) {
        let saved_source = self.saved_source.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(id) = saved_source
            && let Some(cur) = tis::current()
            && cur.id() != id
            && let Some(s) = tis::by_id(&id)
        {
            tracing::info!(from = %cur.id(), to = %id, "恢复本机的输入法（Caps Lock 已发往其他电脑）");
            if let Err(e) = s.select() {
                tracing::warn!(%e, "恢复输入法失败");
            }
            refresh_now();
        }
    }

    /// 回到 Local 时仅在吞键后的核对窗口内最后恢复意外修改；各设备保留本机状态。
    pub(crate) fn leave_remote(&'static self) {
        if self.active() {
            self.enforce();
        }
        self.saved_lock.store(UNKNOWN, Ordering::Release);
        self.until_us.store(0, Ordering::Release);
        let clear = move || *self.saved_source.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if !main_queue::run_async(clear) {
            clear();
        }
    }
}

/// 对外的诊断接口（探针、实机测试程序使用）。可以从任意线程调用：需要 TIS 的调用交给主线程执行并等待
/// （最多 [`DIAG_TIMEOUT`]），主线程不服务主队列时返回「读不到」。
pub mod diag {
    use std::time::Duration;

    pub use super::Behavior;
    use super::main_queue::run_blocking;

    /// 等主线程执行的最长时间。
    pub const DIAG_TIMEOUT: Duration = Duration::from_secs(2);

    /// 当前的大写锁定状态（IOKit）。
    pub fn caps_lock() -> Option<bool> {
        super::hid::caps_lock()
    }

    /// 设置大写锁定（IOKit，含指示灯）。
    pub fn set_caps_lock(on: bool) -> Result<(), String> {
        super::hid::set_caps_lock(on)
    }

    /// 当前输入法的 ID（在主线程上直接读取 TIS）。
    pub fn input_source() -> Option<String> {
        run_blocking(DIAG_TIMEOUT, || super::tis::current().map(|s| s.id())).flatten()
    }

    /// 当前输入法是否在「ABC 一侧」（ASCII 键盘布局）。
    pub fn input_source_is_roman() -> Option<bool> {
        run_blocking(DIAG_TIMEOUT, || super::tis::current().map(|s| s.is_roman())).flatten()
    }

    /// 切换到给定 ID 的已启用输入法。
    pub fn select_input_source(id: &str) -> Result<(), String> {
        let id = id.to_string();
        run_blocking(DIAG_TIMEOUT, move || {
            let r = super::tis::by_id(&id)
                .ok_or_else(|| format!("没有已启用的输入法 {id}"))
                .and_then(|s| s.select());
            super::refresh_now();
            r
        })
        .unwrap_or_else(|| Err("主线程没有响应（没有服务主队列）".into()))
    }

    /// 按本机设置，注入的 Caps Lock 会采用的行为。
    pub fn behavior() -> Behavior {
        super::init();
        let _ = run_blocking(DIAG_TIMEOUT, super::refresh_now);
        super::current_behavior()
    }

    /// 读取设置的各来源（诊断输出）。
    pub fn setting_summary() -> String {
        run_blocking(DIAG_TIMEOUT, || format!("{:?}", super::tis::switch_setting()))
            .unwrap_or_else(|| "主线程没有响应（没有服务主队列）".into())
    }

    /// 测试用：强制行为；`None` 恢复按系统设置。
    pub fn set_behavior_override(b: Option<Behavior>) {
        super::set_behavior_override(b)
    }

    /// 原生长按时长参考值（诊断程序用；实际阈值由系统决定）。
    pub const LONG_PRESS: Duration = super::LONG_PRESS;

    /// 已启用、可以选中的键盘输入源：(ID, 是否在 ABC 一侧)。
    pub fn selectable_input_sources() -> Vec<(String, bool)> {
        run_blocking(DIAG_TIMEOUT, super::tis::selectable_keyboard_sources).unwrap_or_default()
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn lock_mode_toggles_once_per_press_without_native_gesture() {
        let mut m = Machine::default();
        assert_eq!(m.press(Behavior::Lock, false), Action::SetLock(true));
        assert_eq!(m.press(Behavior::Lock, true), Action::None);
        assert_eq!(m.release(), Action::None);
        assert_eq!(m.press(Behavior::Lock, true), Action::SetLock(false));
        assert_eq!(m.release(), Action::None);
    }

    #[test]
    fn switch_mode_preserves_native_down_up_even_when_lock_is_on() {
        let mut m = Machine::default();
        for lock_on in [false, true, false] {
            assert_eq!(m.press(Behavior::SwitchInputSource, lock_on), Action::NativeCaps(true));
            assert_eq!(m.press(Behavior::SwitchInputSource, !lock_on), Action::None);
            assert_eq!(m.release(), Action::NativeCaps(false));
            assert_eq!(m.release(), Action::None);
        }
    }

    #[test]
    fn held_native_gesture_keeps_its_release_if_setting_changes() {
        let mut m = Machine::default();
        assert_eq!(m.press(Behavior::SwitchInputSource, false), Action::NativeCaps(true));
        assert_eq!(m.press(Behavior::Lock, true), Action::None);
        assert_eq!(m.release(), Action::NativeCaps(false));
        assert_eq!(m.press(Behavior::Lock, true), Action::SetLock(false));
        assert_eq!(m.release(), Action::None);
        assert_eq!(m.press(Behavior::SwitchInputSource, false), Action::NativeCaps(true));
        assert_eq!(m.release(), Action::NativeCaps(false));
    }

    #[test]
    fn release_without_press_does_nothing() {
        assert_eq!(Machine::default().release(), Action::None);
    }

    #[test]
    fn pref_values() {
        assert_eq!(parse_pref(PrefValue::Absent), None);
        assert_eq!(parse_pref(PrefValue::Bool(false)), Some(false));
        assert_eq!(parse_pref(PrefValue::Int(1)), Some(true));
        assert_eq!(parse_pref(PrefValue::Int(0)), Some(false));
        assert_eq!(parse_pref(PrefValue::Other), None);
    }

    /// 回归（2026-09-29 线上崩溃：界面程序在 tokio 线程上移交焦点时读取输入法，HIToolbox 断言主线程失败，
    /// 进程被 SIGTRAP 终止）：非主线程上的读取、设置、源端守卫都不能调用 TIS。
    /// 单元测试的线程不是主线程、主队列也没人服务：TIS 包装拒绝调用；其余路径排队给主线程后立即返回，
    /// 本机设置读取与守卫不阻塞后台线程。历史界面程序形态曾验证此约束；焦点移交状态路径现已删除。
    #[test]
    fn background_thread_paths_never_call_tis() {
        std::thread::spawn(|| {
            assert!(!main_queue::is_main_thread());
            let t = Instant::now();
            // TIS 包装在非主线程上拒绝调用（不会走到 HIToolbox 的断言）。
            assert!(tis::current().is_none());
            assert!(tis::by_id("com.apple.keylayout.ABC").is_none());
            assert!(tis::selectable_keyboard_sources().is_empty());
            // 设置读不到时按普通大写锁定。短按现在投递真实系统事件，不能在单元测试里调用
            // perform(NativeCaps)；后台线程构造事件由 sysevent 的无投递测试覆盖。
            assert_eq!(current_behavior(), Behavior::Lock);
            // 源端守卫：输入法的记录、核对、清除都排队给主线程。
            static GUARD: LocalGuard = LocalGuard::new();
            GUARD.enter_remote();
            GUARD.snapshot_source();
            GUARD.caps_swallowed();
            GUARD.enforce();
            GUARD.leave_remote();
            assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        })
        .join()
        .expect("后台线程上的键盘状态路径不能 panic / 崩溃");
    }

    #[test]
    fn behavior_from_setting() {
        let base = SwitchSetting {
            has_non_roman: true,
            ..Default::default()
        };
        // Neither native observation nor preference: ordinary Caps for tap and hold.
        assert_eq!(decide(base), Behavior::Lock);
        // 私有函数优先于偏好。
        let s = SwitchSetting {
            enabled_fn: Some(false),
            pref: Some(true),
            ..base
        };
        assert_eq!(decide(s), Behavior::Lock);
        let s = SwitchSetting {
            enabled_fn: Some(true),
            pref: Some(false),
            ..base
        };
        assert_eq!(decide(s), Behavior::SwitchInputSource);
        // 没有私有函数时用偏好。
        let s = SwitchSetting {
            pref: Some(false),
            ..base
        };
        assert_eq!(decide(s), Behavior::Lock);
        assert_eq!(
            decide(SwitchSetting {
                pref: Some(true),
                ..base
            }),
            Behavior::SwitchInputSource
        );
        // 不允许 / 没有可切换的输入法：普通大写锁定。
        let s = SwitchSetting {
            enabled_fn: Some(true),
            allowed_fn: Some(false),
            ..base
        };
        assert_eq!(decide(s), Behavior::Lock);
        let s = SwitchSetting {
            enabled_fn: Some(true),
            has_non_roman: false,
            ..Default::default()
        };
        assert_eq!(decide(s), Behavior::Lock);
    }
}
