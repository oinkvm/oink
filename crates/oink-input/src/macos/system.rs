//! 系统设置、权限，以及没有现成绑定的系统函数（含运行时查找的私有函数）。

use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use objc2_app_kit::NSEvent;
use objc2_core_foundation::{
    CFBoolean, CFNumber, CFPreferencesAppSynchronize, CFPreferencesCopyAppValue, CFString, CFType,
    kCFPreferencesAnyApplication,
};
use oink_foundation::input::{FeelProfile, InputPermissions, PermissionState};

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDCheckAccess(request: u32) -> u32;
}

const K_IOHID_REQUEST_TYPE_LISTEN_EVENT: u32 = 1;
const K_IOHID_ACCESS_TYPE_GRANTED: u32 = 0;
const K_IOHID_ACCESS_TYPE_UNKNOWN: u32 = 2;

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

/// 在已加载的镜像里查找符号（私有函数），找不到返回空指针。
pub(crate) fn find_symbol(name: &CStr) -> *mut c_void {
    // SAFETY: RTLD_DEFAULT 搜索全部已加载镜像；name 以 NUL 结尾。
    unsafe { dlsym(RTLD_DEFAULT, name.as_ptr()) }
}

static SECURE_INPUT: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
static SECURE_REFRESH_PENDING: AtomicBool = AtomicBool::new(false);

/// CarbonEventsCore.h marks IsSecureEventInputEnabled "Not thread safe". Query on
/// main only; the tap must never wait for main while it may be joining the tap.
/// Background callers get the last sample and request a coalesced refresh. Before
/// main has served the first request (or the sample is stale), the value is unknown/false.
/// This is only the stuck-key cleanup/status hint: macOS itself blocks capture during secure input.
pub(crate) fn secure_input_active() -> bool {
    if super::main_queue::is_main_thread() {
        refresh_secure_input_on_main();
    } else if !SECURE_REFRESH_PENDING.swap(true, Ordering::AcqRel)
        && !super::main_queue::run_async(|| {
            refresh_secure_input_on_main();
            SECURE_REFRESH_PENDING.store(false, Ordering::Release);
        })
    {
        SECURE_REFRESH_PENDING.store(false, Ordering::Release);
    }
    recent_secure_input(*SECURE_INPUT.lock().unwrap_or_else(|e| e.into_inner()), Instant::now())
}

fn refresh_secure_input_on_main() {
    // Keep the native boundary guarded even if another caller is added later.
    if !super::main_queue::is_main_thread() {
        return;
    }
    // SAFETY: serialized on the process main thread; no arguments or borrowed objects.
    let enabled = unsafe { IsSecureEventInputEnabled() != 0 };
    crate::diagnostics::secure(enabled);
    let mut sample = SECURE_INPUT.lock().unwrap_or_else(|e| e.into_inner());
    if sample.is_none_or(|(_, before)| before != enabled) {
        tracing::info!(enabled, "secure input state changed");
    }
    *sample = Some((Instant::now(), enabled));
}

fn recent_secure_input(sample: Option<(Instant, bool)>, now: Instant) -> bool {
    // Idle taps skip this query. Do not release newly held keys using an old positive
    // sample from a password field the user left minutes ago. Normal polling is 500 ms.
    sample.is_some_and(|(at, enabled)| enabled && now.saturating_duration_since(at) < Duration::from_secs(1))
}

pub(crate) fn permissions() -> InputPermissions {
    // SAFETY: 无参数的查询函数。
    let ax = unsafe { AXIsProcessTrusted() } != 0;
    // Use the public process preflight, available since macOS 10.15. The
    // audit-token variant is absent on macOS 14.7 despite the SDK annotation;
    // on macOS 26 its check-by-token TCC path also rejects our unentitled
    // process even when this preflight is Granted. It must not override this
    // result. Neither this query nor AXIsProcessTrusted requests permission.
    // SAFETY: documented ListenEvent request type; no input is captured.
    let listen = unsafe { IOHIDCheckAccess(K_IOHID_REQUEST_TYPE_LISTEN_EVENT) };
    // These public checks are authoritative for this backend. On macOS 26.6.2,
    // the signed process observes AX=true and IOHID=Granted after a live grant,
    // while tcc_authorization_preflight(PostEvent/ListenEvent) stays Unknown.
    // That private self-query is not equivalent to the input-service preflights;
    // using it as a veto hides valid grants even after restarting/re-adding the app.
    static LAST: Mutex<Option<(bool, u32)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.as_ref() != Some(&(ax, listen)) {
        tracing::info!(
            ax_trusted = ax,
            iohid_listen = listen,
            "input permission preflights changed"
        );
        *last = Some((ax, listen));
    }
    drop(last);
    input_permissions(ax, listen, secure_input_active())
}

fn input_permissions(ax: bool, listen: u32, secure_input_active: bool) -> InputPermissions {
    InputPermissions {
        // AX's boolean preflight cannot distinguish a fresh install from a refusal.
        // IOHID can also return Denied before a request (observed on macOS 26).
        // Neither result proves that the user denied a prompt; keep it neutral.
        // These checks never request access or change the system's TCC state.
        accessibility: if ax {
            PermissionState::Granted
        } else {
            PermissionState::NotGranted
        },
        input_monitoring: match listen {
            K_IOHID_ACCESS_TYPE_GRANTED => PermissionState::Granted,
            K_IOHID_ACCESS_TYPE_UNKNOWN => PermissionState::NotDetermined,
            _ => PermissionState::NotGranted,
        },
        secure_input_active,
    }
}

/// 读全局偏好（NSGlobalDomain）。
fn global_pref(key: &str) -> Option<objc2_core_foundation::CFRetained<CFType>> {
    // SAFETY: 读取 CoreFoundation 导出的常量。
    let any = unsafe { kCFPreferencesAnyApplication };
    CFPreferencesCopyAppValue(&CFString::from_str(key), any)
}

fn pref_bool(key: &str) -> Option<bool> {
    let v = global_pref(key)?;
    if let Some(b) = v.downcast_ref::<CFBoolean>() {
        return Some(b.as_bool());
    }
    v.downcast_ref::<CFNumber>().and_then(|n| n.as_i64()).map(|n| n != 0)
}

pub(crate) struct EdgeSettings {
    pub dock: Option<crate::warp_delta::DockSide>,
    pub tile_pt: f64,
    pub corners: Vec<oink_foundation::input::ScreenCorner>,
}

/// Freeze and resistance use the same active-corner predicate and Dock preferences.
pub(crate) fn edge_settings() -> EdgeSettings {
    use crate::warp_delta::DockSide;
    use oink_foundation::input::ScreenCorner;
    let app = CFString::from_str("com.apple.dock");
    CFPreferencesAppSynchronize(&app);
    let get = |key: &str| CFPreferencesCopyAppValue(&CFString::from_str(key), &app);
    let num = |key: &str| {
        get(key).and_then(|v| {
            v.downcast_ref::<CFNumber>()
                .and_then(|n| n.as_f64())
                .or_else(|| v.downcast_ref::<CFBoolean>().map(|b| f64::from(u8::from(b.as_bool()))))
        })
    };
    let dock = (num("autohide").unwrap_or(0.0) != 0.0).then(|| {
        match get("orientation")
            .and_then(|v| v.downcast_ref::<CFString>().map(|s| s.to_string()))
            .as_deref()
        {
            Some("left") => DockSide::Left,
            Some("right") => DockSide::Right,
            _ => DockSide::Bottom,
        }
    });
    let tile = num("tilesize").unwrap_or(64.0);
    let tile_pt = if num("magnification").unwrap_or(0.0) != 0.0 {
        tile.max(num("largesize").unwrap_or(tile))
    } else {
        tile
    };
    let corners = [
        ("wvous-tl-corner", ScreenCorner::TopLeft),
        ("wvous-tr-corner", ScreenCorner::TopRight),
        ("wvous-bl-corner", ScreenCorner::BottomLeft),
        ("wvous-br-corner", ScreenCorner::BottomRight),
    ]
    .into_iter()
    .filter_map(|(key, corner)| active_corner(num(key).map(|v| v as i64)).then_some(corner))
    .collect();
    EdgeSettings { dock, tile_pt, corners }
}

pub(crate) fn feel_profile() -> FeelProfile {
    // SAFETY: 读取 CoreFoundation 导出的常量。
    CFPreferencesAppSynchronize(unsafe { kCFPreferencesAnyApplication });
    // NSEvent 的类属性反映当前生效的设置（包括用户从未修改过时的系统默认值）。
    let delay = NSEvent::keyRepeatDelay();
    let interval = NSEvent::keyRepeatInterval();
    // macOS 的「自然滚动」是全局设置，鼠标与触摸板共用；未设置时默认开启。
    let natural = pref_bool("com.apple.swipescrolldirection").unwrap_or(true);
    FeelProfile {
        key_repeat_delay_ms: secs_to_ms(delay, 500),
        key_repeat_interval_ms: secs_to_ms(interval, 33),
        wheel_lines_per_notch: None,
        natural_scroll_mouse: natural,
        natural_scroll_touchpad: natural,
    }
}

/// 系统的双击间隔（秒）。
pub(crate) fn double_click_interval() -> f64 {
    let v = NSEvent::doubleClickInterval();
    if v.is_finite() && v > 0.0 { v } else { 0.5 }
}

fn secs_to_ms(v: f64, default: u32) -> u32 {
    if v.is_finite() && v > 0.0 {
        (v * 1000.0).round() as u32
    } else {
        default
    }
}

// ---- 私有：隐藏后台程序的光标 ----

type CgsConnection = i32;
type DefaultConnectionFn = unsafe extern "C" fn() -> CgsConnection;
type SetConnectionPropertyFn =
    unsafe extern "C" fn(CgsConnection, CgsConnection, *const CFString, *const CFType) -> i32;

/// 让本进程在后台时也能隐藏光标（私有 `CGSSetConnectionProperty(..., "SetsCursorInBackground", true)`，
/// Barrier、lan-mouse 同样使用）。找不到私有函数时返回 false，隐藏光标只在本进程在前台时有效。
pub(crate) fn allow_background_cursor_hiding() -> bool {
    static DONE: OnceLock<bool> = OnceLock::new();
    *DONE.get_or_init(|| {
        let conn = find_symbol(c"_CGSDefaultConnection");
        let set = find_symbol(c"CGSSetConnectionProperty");
        if conn.is_null() || set.is_null() {
            tracing::warn!("找不到 CGSSetConnectionProperty，后台隐藏光标不可用");
            return false;
        }
        // SAFETY: 两个私有函数的签名如上（广泛使用多年）。
        unsafe {
            let conn: DefaultConnectionFn = std::mem::transmute::<*mut c_void, DefaultConnectionFn>(conn);
            let set: SetConnectionPropertyFn = std::mem::transmute::<*mut c_void, SetConnectionPropertyFn>(set);
            let cid = conn();
            let key = CFString::from_str("SetsCursorInBackground");
            let value: &CFType = CFBoolean::new(true);
            set(cid, cid, &*key, value) == 0
        }
    })
}

/// Read Dock and hot-corner preferences off the input callback. A missing/0/1 corner is inactive.
/// Side Dock placement in irregular multi-display layouts is intentionally conservative: reserve
/// that side on every display; routing removes internal seams.
pub(crate) fn edge_resistance_regions(
    displays: &[oink_foundation::display::DisplayInfo],
) -> Vec<oink_foundation::input::EdgeResistanceRegion> {
    use crate::warp_delta::DockSide;
    use oink_foundation::input::ScreenEdge;
    let settings = edge_settings();
    let dock = settings.dock.map(|side| match side {
        DockSide::Left => ScreenEdge::Left,
        DockSide::Right => ScreenEdge::Right,
        DockSide::Bottom => ScreenEdge::Bottom,
    });
    let corners = settings.corners;
    // This AppKit query reports the effective setting (preference changes require logout).
    // Avoid blocking startup on a main queue which has not begun running yet; unknown => all displays.
    let separate = if dock == Some(ScreenEdge::Bottom) && displays.len() > 1 {
        super::main_queue::run_blocking(std::time::Duration::from_millis(50), || {
            objc2_app_kit::NSScreen::screensHaveSeparateSpaces(objc2::MainThreadMarker::new().unwrap())
        })
    } else {
        None
    };
    regions_for_settings(displays, dock, &corners, separate)
}

fn active_corner(value: Option<i64>) -> bool {
    value.is_some_and(|v| v > 1)
}

fn regions_for_settings(
    displays: &[oink_foundation::display::DisplayInfo],
    dock: Option<oink_foundation::input::ScreenEdge>,
    corners: &[oink_foundation::input::ScreenCorner],
    separate_spaces: Option<bool>,
) -> Vec<oink_foundation::input::EdgeResistanceRegion> {
    use oink_foundation::input::{EdgeResistanceRegion, ScreenEdge};
    displays
        .iter()
        .map(|d| EdgeResistanceRegion {
            display: d.key.clone(),
            edges: dock
                .filter(|side| *side != ScreenEdge::Bottom || separate_spaces != Some(false) || d.is_primary)
                .into_iter()
                .collect(),
            corners: corners.to_vec(),
        })
        .collect()
}

#[cfg(all(test, debug_assertions))]
mod edge_tests {
    use super::*;
    use oink_foundation::{
        Rect,
        display::DisplayInfo,
        input::{ScreenCorner, ScreenEdge},
    };

    #[test]
    fn zero_and_one_are_inactive_other_actions_including_quick_note_are_active() {
        for value in [None, Some(-1), Some(0), Some(1)] {
            assert!(!active_corner(value));
        }
        for value in [2, 4, 14, 99] {
            assert!(active_corner(Some(value)));
        }
    }

    #[test]
    fn dock_display_selection_and_disabled_dock_leave_only_active_corners() {
        let displays = (0..2)
            .map(|i| DisplayInfo {
                key: i.to_string(),
                name: i.to_string(),
                bounds: Rect::new(i as f64 * 100.0, 0.0, 100.0, 100.0),
                pixel_width: 100,
                pixel_height: 100,
                reference_extent: None,
                rotation_deg: 0,
                is_primary: i == 0,
                is_builtin: false,
                is_virtual: false,
            })
            .collect::<Vec<_>>();
        for separate in [Some(true), None] {
            let regions = regions_for_settings(&displays, Some(ScreenEdge::Bottom), &[], separate);
            assert!(regions.iter().all(|r| r.edges == [ScreenEdge::Bottom]));
        }
        let regions = regions_for_settings(&displays, Some(ScreenEdge::Bottom), &[], Some(false));
        assert_eq!(regions[0].edges, [ScreenEdge::Bottom]);
        assert!(regions[1].edges.is_empty());
        for side in [ScreenEdge::Left, ScreenEdge::Right] {
            let regions = regions_for_settings(&displays, Some(side), &[], Some(false));
            assert!(regions.iter().all(|r| r.edges == [side]));
        }
        let regions = regions_for_settings(&displays, None, &[ScreenCorner::BottomRight], Some(true));
        assert!(
            regions
                .iter()
                .all(|r| r.edges.is_empty() && r.corners == [ScreenCorner::BottomRight])
        );
    }
}

#[cfg(all(test, debug_assertions))]
mod thread_tests {
    #[test]
    fn preflights_do_not_infer_user_refusal() {
        use super::*;
        for listen in [1, 99] {
            let p = input_permissions(false, listen, false);
            assert_eq!(p.accessibility, PermissionState::NotGranted);
            assert_eq!(p.input_monitoring, PermissionState::NotGranted);
        }
        let p = input_permissions(true, K_IOHID_ACCESS_TYPE_UNKNOWN, true);
        assert_eq!(p.accessibility, PermissionState::Granted);
        assert_eq!(p.input_monitoring, PermissionState::NotDetermined);
        assert!(p.secure_input_active);
        assert_eq!(
            input_permissions(true, K_IOHID_ACCESS_TYPE_GRANTED, false).input_monitoring,
            PermissionState::Granted
        );
    }

    #[test]
    fn public_listen_preflight_is_authoritative_and_independent_of_ax() {
        use super::*;
        // Build 5 observed AX=true and IOHID=Granted, but an unrelated
        // audit-token check returned false and incorrectly downgraded it.
        // Fake native results cover both independent TCC grants and secure input.
        for ax in [false, true] {
            for secure in [false, true] {
                for (raw, expected) in [
                    (K_IOHID_ACCESS_TYPE_GRANTED, PermissionState::Granted),
                    (1, PermissionState::NotGranted),
                    (K_IOHID_ACCESS_TYPE_UNKNOWN, PermissionState::NotDetermined),
                ] {
                    let p = input_permissions(ax, raw, secure);
                    assert_eq!(p.input_monitoring, expected);
                    assert_eq!(p.secure_input_active, secure);
                    assert_eq!(p.accessibility == PermissionState::Granted, ax);
                }
            }
        }
    }

    #[test]
    fn stale_secure_input_does_not_release_keys_after_idle() {
        let now = std::time::Instant::now();
        assert!(!super::recent_secure_input(None, now));
        assert!(!super::recent_secure_input(Some((now, false)), now));
        assert!(super::recent_secure_input(
            Some((now, true)),
            now + std::time::Duration::from_millis(500)
        ));
        assert!(!super::recent_secure_input(
            Some((now, true)),
            now + std::time::Duration::from_secs(1)
        ));
    }

    #[test]
    fn secure_input_from_worker_never_enters_carbon_or_waits_for_main() {
        assert!(!super::super::main_queue::is_main_thread());
        let start = std::time::Instant::now();
        // The test main thread does not service its queue. Calls must coalesce and return.
        for _ in 0..1000 {
            super::refresh_secure_input_on_main();
            super::secure_input_active();
        }
        assert!(super::SECURE_REFRESH_PENDING.load(std::sync::atomic::Ordering::Acquire));
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }
}
