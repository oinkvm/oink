//! Local-only credential storage. No secret is passed through ordinary routed
//! input, diagnostics, configuration, or a command-line helper.
//!
//! The local transaction is admitted only after the display is awake, the
//! console session is locked, and the visible loginwindow surface is stable.
//! Secure Input is intentionally not a readiness requirement; it was observed
//! to pulse or remain false while the password surface accepted input.

use std::{
    ffi::{CStr, c_void},
    ptr,
    sync::Mutex,
    time::{Duration, Instant},
};
use zeroize::{Zeroize, Zeroizing};
type Cf = *const c_void;
type Result<T> = std::result::Result<T, &'static str>;
const KEYCHAIN: &str = "macOS专用钥匙串操作失败";
pub(crate) const PASSWORD_INCORRECT: &str = "密码不正确";
const PASSWORD_VERIFY: &str = "无法验证登录密码";
pub(crate) const LOCK_CHANGED: &str = "锁屏状态或登录会话已变化，自动解锁已停止";
pub(crate) const DISPLAY_ASLEEP: &str = "无法唤醒 macOS 锁屏显示器";
pub(crate) const NOT_READY: &str = "macOS 锁屏密码框未在规定时间内就绪";
pub(crate) const LOCAL_TYPING: &str = "本机正在输入，已跳过同步解锁";
const READINESS_TIMEOUT: Duration = Duration::from_secs(5);
const READINESS_STABLE: Duration = Duration::from_millis(600);
const READINESS_SAMPLE_INTERVAL: Duration = Duration::from_millis(20);
const READINESS_MAX_GAP: Duration = Duration::from_millis(100);
static STORE: Mutex<()> = Mutex::new(());

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    static kCFAllocatorNull: Cf;
    static kCFBooleanTrue: Cf;
    fn CFRelease(v: Cf);
    fn CFGetTypeID(v: Cf) -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(v: Cf) -> u8;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFDataGetTypeID() -> usize;
    fn CFArrayGetCount(a: Cf) -> isize;
    fn CFArrayGetValueAtIndex(a: Cf, index: isize) -> Cf;
    fn CFStringCreateWithBytes(a: Cf, b: *const u8, n: isize, encoding: u32, external: u8) -> Cf;
    fn CFStringGetTypeID() -> usize;
    fn CFStringGetCString(s: Cf, buffer: *mut i8, buffer_size: isize, encoding: u32) -> u8;
    fn CFDictionaryCreate(a: Cf, k: *const Cf, v: *const Cf, n: isize, kc: Cf, vc: Cf) -> Cf;
    fn CFDictionaryGetValue(d: Cf, k: Cf) -> Cf;
    static kCFTypeArrayCallBacks: c_void;
    fn CFArrayCreate(a: Cf, values: *const Cf, count: isize, callbacks: Cf) -> Cf;
    fn CFNumberGetValue(n: Cf, kind: isize, out: *mut c_void) -> u8;
    fn CFDataCreateWithBytesNoCopy(a: Cf, b: *const u8, n: isize, deallocator: Cf) -> Cf;
    fn CFDataGetLength(v: Cf) -> isize;
    fn CFDataGetBytePtr(v: Cf) -> *const u8;
}
#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecClass: Cf;
    static kSecClassGenericPassword: Cf;
    static kSecAttrService: Cf;
    static kSecAttrAccount: Cf;
    static kSecAttrAccess: Cf;
    static kSecUseKeychain: Cf;
    static kSecMatchSearchList: Cf;
    static kSecUseAuthenticationUI: Cf;
    static kSecUseAuthenticationUIFail: Cf;
    static kSecReturnData: Cf;
    static kSecValueData: Cf;
    fn SecItemCopyMatching(q: Cf, result: *mut Cf) -> i32;
    fn SecItemAdd(q: Cf, result: *mut Cf) -> i32;
    fn SecItemDelete(q: Cf) -> i32;
    fn SecKeychainCopyDefault(keychain: *mut Cf) -> i32;
    fn SecKeychainSetUserInteractionAllowed(allowed: u8) -> i32;
    fn SecKeychainGetStatus(keychain: Cf, status: *mut u32) -> i32;
    fn SecTrustedApplicationCreateFromPath(path: *const i8, app: *mut Cf) -> i32;
    fn SecAccessCreate(label: Cf, trusted: Cf, access: *mut Cf) -> i32;
    fn SecCodeCopySelf(flags: u32, code: *mut Cf) -> i32;
    fn SecRequirementCreateWithString(text: Cf, flags: u32, requirement: *mut Cf) -> i32;
    fn SecCodeCheckValidity(code: Cf, flags: u32, requirement: Cf) -> i32;
}
#[link(name = "OpenDirectory", kind = "framework")]
unsafe extern "C" {
    fn ODSessionCreate(allocator: Cf, options: Cf, error: *mut Cf) -> Cf;
    fn ODNodeCreateWithNodeType(allocator: Cf, session: Cf, node_type: u32, error: *mut Cf) -> Cf;
    fn ODNodeCopyRecord(node: Cf, record_type: Cf, record_name: Cf, attributes: Cf, error: *mut Cf) -> Cf;
    fn ODRecordVerifyPassword(record: Cf, password: Cf, error: *mut Cf) -> bool;
    static kODRecordTypeUsers: Cf;
}
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegistryGetRootEntry(port: u32) -> u32;
    fn IORegistryEntryCreateCFProperty(entry: u32, key: Cf, a: Cf, options: u32) -> Cf;
    fn IOObjectRelease(object: u32) -> i32;
}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGSessionCopyCurrentDictionary() -> Cf;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayIsAsleep(display: u32) -> u8;
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> Cf;
    fn CGPreflightPostEventAccess() -> bool;
    fn CGEventCreateKeyboardEvent(source: Cf, key: u16, down: bool) -> Cf;
    fn CGEventKeyboardSetUnicodeString(event: Cf, n: usize, text: *const u16);
    fn CGEventSetFlags(event: Cf, flags: u64);
    fn CGEventSetIntegerValueField(event: Cf, field: u32, value: i64);
    fn CGEventPost(tap: u32, event: Cf);
    fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
}
struct Owned(Cf);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: exactly one Create/Copy reference is owned here.
            unsafe { CFRelease(self.0) }
        }
    }
}
fn owned(raw: Cf) -> Result<Owned> {
    if raw.is_null() { Err(KEYCHAIN) } else { Ok(Owned(raw)) }
}
fn string(s: &str) -> Result<Owned> {
    // SAFETY: valid UTF-8 bytes, copied synchronously.
    owned(unsafe { CFStringCreateWithBytes(ptr::null(), s.as_ptr(), s.len() as isize, 0x08000100, 0) })
}
fn dict(pairs: &[(Cf, Cf)]) -> Result<Owned> {
    let keys: Vec<_> = pairs.iter().map(|p| p.0).collect();
    let values: Vec<_> = pairs.iter().map(|p| p.1).collect();
    // SAFETY: all entries are live CF objects, retained by the type callbacks.
    owned(unsafe {
        CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            keys.len() as isize,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    })
}
fn code(status: i32) -> Result<()> {
    match status {
        0 => Ok(()),
        -34018 => Err("macOS专用钥匙串签名权限缺失或无效"),
        -25308 => Err("macOS专用钥匙串当前不可无交互访问"),
        -25293 => Err("macOS专用钥匙串访问被拒绝"),
        _ => Err(KEYCHAIN),
    }
}
fn array(values: &[Cf]) -> Result<Owned> {
    // SAFETY: callers pass live CF objects; standard callbacks retain them.
    owned(unsafe {
        CFArrayCreate(
            ptr::null(),
            values.as_ptr(),
            values.len() as isize,
            &raw const kCFTypeArrayCallBacks,
        )
    })
}
fn require_backend() -> Result<()> {
    // The manager historically shared the backend's signing identifier; the
    // packaging rule now gives it a distinct identity. A trusted-application
    // ACL compares designated requirements, not just executable paths.
    let requirement = string(
        r#"anchor apple generic and identifier "com.hachiwii.oink" and certificate leaf[subject.OU] = "893V37X2CW""#,
    )?;
    let mut me = ptr::null();
    let mut req = ptr::null();
    // SAFETY: initialized out-pointers, owned Copy/Create results released even
    // on failure. This verifies code identity, never requests credentials.
    let (self_status, req_status) = unsafe {
        (
            SecCodeCopySelf(0, &mut me),
            SecRequirementCreateWithString(requirement.0, 0, &mut req),
        )
    };
    let me = Owned(me);
    let req = Owned(req);
    if self_status != 0 || req_status != 0 || me.0.is_null() || req.0.is_null() {
        return Err("仅受信任签名的Oink后台可访问自动解锁凭据");
    }
    // SAFETY: validated owned code and requirement references.
    if unsafe { SecCodeCheckValidity(me.0, 0, req.0) } != 0 {
        return Err("仅受信任签名的Oink后台可访问自动解锁凭据");
    }
    Ok(())
}
fn keychain() -> Result<Owned> {
    require_backend()?;
    // Classic Keychain does not uniformly honor SecItem's UI-fail option.
    // The backend has no interactive Keychain business; disable its process-wide
    // Security UI permanently, never temporarily restore it during a lock race.
    // Other Oink components have separate processes and are unaffected.
    // SAFETY: supported process-local Security setting, Boolean false.
    code(unsafe { SecKeychainSetUserInteractionAllowed(0) })?;
    let mut raw = ptr::null();
    // SAFETY: initialized out-pointer; only the user's default local keychain.
    let status = unsafe { SecKeychainCopyDefault(&mut raw) };
    let keychain = Owned(raw);
    code(status)?;
    if keychain.0.is_null() {
        return Err(KEYCHAIN);
    }
    Ok(keychain)
}
fn require_unlocked(keychain: &Owned) -> Result<()> {
    let mut flags = 0;
    // SAFETY: owned keychain and writable flags. Never unlock or change policy.
    code(unsafe { SecKeychainGetStatus(keychain.0, &mut flags) })?;
    if flags & 1 == 0 {
        return Err("本机钥匙串已锁定，自动解锁凭据暂不可用");
    }
    Ok(())
}
fn backend_access() -> Result<Owned> {
    let mut raw = ptr::null();
    // SAFETY: null path means this signed backend, NOT an unrestricted ACL.
    let status = unsafe { SecTrustedApplicationCreateFromPath(ptr::null(), &mut raw) };
    let app = Owned(raw);
    code(status)?;
    if app.0.is_null() {
        return Err(KEYCHAIN);
    }
    let trusted = array(&[app.0])?;
    let label = string("Oink local auto-unlock")?;
    let mut raw = ptr::null();
    // SAFETY: exactly one trusted application; no empty/NULL trusted list.
    let status = unsafe { SecAccessCreate(label.0, trusted.0, &mut raw) };
    let access = Owned(raw);
    code(status)?;
    if access.0.is_null() {
        return Err(KEYCHAIN);
    }
    Ok(access)
}
fn query(extra: &[(Cf, Cf)], adding: bool, keychain: &Owned) -> Result<Owned> {
    let service = string("com.hachiwii.oink.auto-unlock")?;
    let account = string("local-console-password")?;
    let search = array(&[keychain.0])?;
    // Classic Keychain is the sole store, not a fallback. On macOS 26.6.2 the
    // DP entitlement requires a profile (unprofiled keychain-access-groups
    // killed the test host; app-group-only access returned -34018). The local
    // keychain's own lock policy remains authoritative; never prompt or unlock.
    // SAFETY: process-lifetime Security constants and live retained objects.
    let mut pairs = unsafe {
        vec![
            (kSecClass, kSecClassGenericPassword),
            (kSecAttrService, service.0),
            (kSecAttrAccount, account.0),
            if adding {
                (kSecUseKeychain, keychain.0)
            } else {
                (kSecMatchSearchList, search.0)
            },
            (kSecUseAuthenticationUI, kSecUseAuthenticationUIFail),
        ]
    };
    pairs.extend_from_slice(extra);
    dict(&pairs)
}
/// Checks existence only, including while the Keychain is locked. Never fetches
/// the password or prompts for access.
pub fn status() -> Result<bool> {
    let _guard = STORE.lock().map_err(|_| KEYCHAIN)?;
    let keychain = keychain()?;
    let q = query(&[], false, &keychain)?;
    // SAFETY: live query; null output requests no credential material.
    match unsafe { SecItemCopyMatching(q.0, ptr::null_mut()) } {
        0 => Ok(true),
        -25300 => Ok(false),
        n => code(n).map(|()| false),
    }
}
/// Saves exactly the supplied bytes. Does not authenticate or inject. The caller
/// owns and must zeroize its IPC/UI buffers; this function never logs its input.
pub fn save(password: &str) -> Result<()> {
    let _guard = STORE.lock().map_err(|_| KEYCHAIN)?;
    verify_password(password)?;
    let keychain = keychain()?;
    require_unlocked(&keychain)?;
    let q = query(&[], false, &keychain)?;
    // SAFETY: borrowed bytes remain alive through the synchronous calls and the
    // CFData/dictionaries are dropped before return; kCFAllocatorNull never frees them.
    let data = owned(unsafe {
        CFDataCreateWithBytesNoCopy(
            ptr::null(),
            password.as_ptr(),
            password.len() as isize,
            kCFAllocatorNull,
        )
    })?;
    let access = backend_access()?;
    // Never update an item's bytes under a pre-existing, possibly broader ACL.
    // Updating kSecAttrAccess stalled despite kSecUseAuthenticationUIFail in
    // the macOS 26.6.2 fixture; its UI behavior is unverified. Replace the exact item
    // instead. If adding fails after deletion, status reports absent; do not
    // retain stale credentials or retry into an item created by another process.
    // SAFETY: live query scoped to our service/account and default keychain.
    let deleted = unsafe { SecItemDelete(q.0) };
    if deleted != -25300 {
        code(deleted)?;
    }
    // SAFETY: live data, exclusive backend ACL and process-lifetime constants.
    let attributes = unsafe { [(kSecValueData, data.0), (kSecAttrAccess, access.0)] };
    let add = query(&attributes, true, &keychain)?;
    // SAFETY: no result data requested; Security copies/encrypts synchronously.
    code(unsafe { SecItemAdd(add.0, ptr::null_mut()) })
}
fn verify_password(password: &str) -> Result<()> {
    // SAFETY: getpwuid returns either a null pointer or a process-owned passwd
    // record; both pointers are checked before CStr borrows the username.
    let username = unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() || (*entry).pw_name.is_null() {
            return Err(PASSWORD_VERIFY);
        }
        CStr::from_ptr((*entry).pw_name)
            .to_str()
            .map_err(|_| PASSWORD_VERIFY)
            .and_then(string)
    }?;
    let password = string(password)?;
    let mut session_error = ptr::null();
    // SAFETY: null allocator/options request OpenDirectory defaults and the
    // local error slot is valid for the synchronous call; Owned releases the
    // returned session reference exactly once.
    let session = Owned(unsafe { ODSessionCreate(ptr::null(), ptr::null(), &mut session_error) });
    let _session_error = Owned(session_error);
    if session.0.is_null() {
        return Err(PASSWORD_VERIFY);
    }
    let mut node_error = ptr::null();
    // SAFETY: session.0 is a live OpenDirectory session owned above, the
    // default allocator is requested, and node_error is a valid output slot.
    let node = Owned(unsafe { ODNodeCreateWithNodeType(ptr::null(), session.0, 0x2201, &mut node_error) });
    let _node_error = Owned(node_error);
    if node.0.is_null() {
        return Err(PASSWORD_VERIFY);
    }
    let mut record_error = ptr::null();
    // SAFETY: node.0 and username.0 remain live for this synchronous call;
    // the null attributes argument requests the complete user record.
    let record =
        Owned(unsafe { ODNodeCopyRecord(node.0, kODRecordTypeUsers, username.0, ptr::null(), &mut record_error) });
    let _record_error = Owned(record_error);
    if record.0.is_null() {
        return Err(PASSWORD_VERIFY);
    }
    let mut verify_error = ptr::null();
    // SAFETY: record.0 and password.0 are live owned CF objects and
    // verify_error is a valid output slot for the synchronous verification.
    let valid = unsafe { ODRecordVerifyPassword(record.0, password.0, &mut verify_error) };
    let _verify_error = Owned(verify_error);
    if valid { Ok(()) } else { Err(PASSWORD_INCORRECT) }
}
/// Deletion is idempotent and permitted while the Keychain is locked. No
/// separate enabled bit can outlive the credential.
pub fn disable() -> Result<()> {
    let _guard = STORE.lock().map_err(|_| KEYCHAIN)?;
    let keychain = keychain()?;
    let q = query(&[], false, &keychain)?;
    // SAFETY: deletes only the exact backend-owned local item.
    match unsafe { SecItemDelete(q.0) } {
        0 | -25300 => Ok(()),
        n => code(n),
    }
}
/// No Keychain read, wake, character or Return is permitted until the native
/// readiness and mid-unlock tests have succeeded. No environment bypass exists.
pub fn attempt(
    backend: std::sync::Arc<dyn oink_input::InputBackend>,
    start: impl FnOnce(super::typing::Job) -> Result<()>,
) -> Result<()> {
    password_field_ready(backend.as_ref())?;
    let text = read_secret()?;
    if text.is_empty() {
        return Err("未保存有效的登录密码");
    }
    type_password(backend, text, false, start)
}
/// Sync unlock target transaction. The display was woken at `woke` (in
/// parallel with the source challenge); readiness keeps the same five-second
/// window measured from that wake.
pub fn sync_attempt(
    backend: std::sync::Arc<dyn oink_input::InputBackend>,
    woke: Instant,
    start: impl FnOnce(super::typing::Job) -> Result<()>,
) -> Result<()> {
    if let Err(reason) = wait_ready(woke + READINESS_TIMEOUT) {
        // SAFETY: read-only CoreGraphics query of the main display.
        return Err(if unsafe { CGDisplayIsAsleep(CGMainDisplayID()) != 0 } {
            DISPLAY_ASLEEP
        } else {
            reason
        });
    }
    let text = read_secret()?;
    if text.is_empty() {
        return Err("未保存有效的登录密码");
    }
    // User decision Q5 also applies to macOS, but loginwindow's verdict after
    // Return is not observable to Oink. An unattended sync therefore verifies
    // the stored password with Open Directory before typing anything; a wrong
    // one latches password_incorrect and stops further sync attempts.
    verify_password(std::str::from_utf8(&text).map_err(|_| "macOS已存凭据格式无效")?)?;
    type_password(backend, text, true, start)
}
/// The backend's own user session is locked, on console and logged in.
pub(crate) fn console_locked() -> bool {
    locked_session().is_ok()
}
/// Pre-start rejection and lock changes are not failures: the user unlocked or
/// the transaction lost its admission before typing anything.
pub(crate) fn stopped_by_state_change(reason: &str) -> bool {
    matches!(
        reason,
        LOCK_CHANGED
            | "本机已解锁或锁定状态未知，自动输入已停止"
            | "输入开始前焦点或连接状态改变，已取消自动解锁"
            | "输入开始前状态改变，自动解锁已取消"
    )
}
/// Local keyboard input within the last two seconds (user decision Q9). The
/// HID system state covers the lock screen, where the password field's Secure
/// Input hides key events from Oink's own event tap.
fn local_typing() -> bool {
    const HID_SYSTEM_STATE: i32 = 1;
    const KEY_DOWN: u32 = 10;
    // SAFETY: read-only query of the HID event source state; no permission prompt.
    let seconds = unsafe { CGEventSourceSecondsSinceLastEventType(HID_SYSTEM_STATE, KEY_DOWN) };
    seconds.is_finite() && seconds >= 0.0 && seconds < super::sync::LOCAL_TYPING.as_secs_f64()
}
fn type_password(
    backend: std::sync::Arc<dyn oink_input::InputBackend>,
    text: Zeroizing<Vec<u8>>,
    sync: bool,
    start: impl FnOnce(super::typing::Job) -> Result<()>,
) -> Result<()> {
    let session = locked_session()?;
    let created = std::time::Instant::now();
    let (reply, result) = std::sync::mpsc::sync_channel(1);
    let job = super::typing::Job::new(
        Box::new(move |admission| {
            // Checked on the injection owner immediately before the first
            // character, together with the per-character lock guard.
            if sync && local_typing() {
                return Err(LOCAL_TYPING);
            }
            let mut begun = false;
            guarded_text_at(
                text,
                created,
                || admission.load(std::sync::atomic::Ordering::Acquire) && backend.input_safety().active(),
                || locked_session().ok().filter(|now| *now == session).map(|_| true),
                |c| {
                    post_character(session, c, created + super::typing::MAX_TYPING, &mut begun, &|| {
                        admission.load(std::sync::atomic::Ordering::Acquire) && backend.input_safety().active()
                    })
                },
            )
        }),
        reply,
    );
    start(job)?;
    result
        .recv_timeout(super::typing::MAX_TYPING + std::time::Duration::from_millis(500))
        .map_err(|_| "自动输入未能及时完成，已取消")?
}
fn password_field_ready(backend: &dyn oink_input::InputBackend) -> Result<()> {
    password_field_ready_for(backend, READINESS_TIMEOUT)
}

fn password_field_ready_for(backend: &dyn oink_input::InputBackend, timeout: Duration) -> Result<()> {
    // UserActivity is the only wake mechanism used here. Secure Input is
    // intentionally not part of readiness: on the measured macOS versions it
    // can pulse or remain false while the loginwindow surface is usable.
    backend.declare_user_activity().map_err(|_| DISPLAY_ASLEEP)?;
    wait_ready(Instant::now() + timeout)
}
fn wait_ready(deadline: Instant) -> Result<()> {
    let mut readiness = ReadinessState::default();
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(NOT_READY);
        }
        if observe_readiness(&mut readiness, now, false, readiness_sample()) {
            return Ok(());
        }
        std::thread::sleep(READINESS_SAMPLE_INTERVAL);
    }
}

// Kept private and unreachable from attempt while the native safety gate is
// closed. These helpers are for the eventual measured local injection path.
#[allow(dead_code)]
fn read_secret() -> Result<Zeroizing<Vec<u8>>> {
    let _guard = STORE.lock().map_err(|_| KEYCHAIN)?;
    let keychain = keychain()?;
    require_unlocked(&keychain)?;
    // SAFETY: process-lifetime return-data constants, no secret in the query.
    let q = query(&[unsafe { (kSecReturnData, kCFBooleanTrue) }], false, &keychain)?;
    let mut raw = ptr::null();
    // SAFETY: initialized out-pointer, owned Copy result even on failure.
    let result = unsafe { SecItemCopyMatching(q.0, &mut raw) };
    let data = Owned(raw);
    code(result)?;
    // SAFETY: check type and length before reading the Security-owned bytes;
    // release the immutable OS buffer promptly, zeroize our mutable copy on drop.
    unsafe {
        if data.0.is_null() || CFGetTypeID(data.0) != CFDataGetTypeID() {
            return Err(KEYCHAIN);
        }
        let n = CFDataGetLength(data.0);
        if !(0..=65536).contains(&n) {
            return Err(KEYCHAIN);
        }
        if n == 0 {
            return Ok(Zeroizing::new(Vec::new()));
        }
        Ok(Zeroizing::new(
            std::slice::from_raw_parts(CFDataGetBytePtr(data.0), n as usize).to_vec(),
        ))
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Session {
    uid: i64,
    audit: i64,
}
fn get(d: Cf, name: &str) -> Result<Cf> {
    let key = string(name)?;
    // SAFETY: caller passes a checked dictionary kept alive for the access.
    Ok(unsafe { CFDictionaryGetValue(d, key.0) })
}
fn flag(v: Cf) -> bool {
    // SAFETY: null/type checked before Boolean access. Missing is never locked.
    unsafe { !v.is_null() && CFGetTypeID(v) == CFBooleanGetTypeID() && CFBooleanGetValue(v) != 0 }
}
fn number(v: Cf) -> Result<i64> {
    let mut out = 0;
    // SAFETY: checked CFNumber, signed 64-bit destination.
    if unsafe {
        v.is_null()
            || CFGetTypeID(v) != CFNumberGetTypeID()
            || CFNumberGetValue(v, 4, (&mut out as *mut i64).cast()) == 0
    } {
        Err(LOCK_CHANGED)
    } else {
        Ok(out)
    }
}
fn locked_session() -> Result<Session> {
    let key = string("IOConsoleLocked")?;
    // SAFETY: read-only root entry/property; each owned reference released once.
    let io = unsafe {
        let root = IORegistryGetRootEntry(0);
        if root == 0 {
            return Err(LOCK_CHANGED);
        }
        let io = Owned(IORegistryEntryCreateCFProperty(root, key.0, ptr::null(), 0));
        IOObjectRelease(root);
        io
    };
    if !flag(io.0) {
        return Err(LOCK_CHANGED);
    }
    // CG lock alone includes the password grace period (14.7.8: IO lagged CG
    // by 5.422 s). IO is primary; CG must also confirm, not replace it. These
    // samples are NOT an atomic delivery guarantee; that is why attempt is shut.
    // SAFETY: read-only Copy API; Owned releases the returned snapshot.
    let cg = Owned(unsafe { CGSessionCopyCurrentDictionary() });
    // SAFETY: non-null owned CF value; check its type before dictionary access.
    if cg.0.is_null() || unsafe { CFGetTypeID(cg.0) != CFDictionaryGetTypeID() } {
        return Err(LOCK_CHANGED);
    }
    for key in [
        "CGSSessionScreenIsLocked",
        "kCGSSessionOnConsoleKey",
        "kCGSessionLoginDoneKey",
    ] {
        if !flag(get(cg.0, key)?) {
            return Err(LOCK_CHANGED);
        }
    }
    Ok(Session {
        uid: number(get(cg.0, "kCGSSessionUserIDKey")?)?,
        audit: number(get(cg.0, "kCGSSessionAuditIDKey")?)?,
    })
}

#[derive(Clone, PartialEq, Eq)]
struct WindowShape {
    number: i64,
    pid: i64,
    layer: i64,
    bounds: [i64; 4],
    alpha: u64,
    onscreen: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct WindowFingerprint(Vec<WindowShape>);

#[derive(Clone)]
struct ReadinessSample {
    session: Session,
    fingerprint: WindowFingerprint,
}

#[derive(Default)]
struct ReadinessState {
    candidate: Option<(Session, WindowFingerprint, Instant)>,
    last_observation: Option<Instant>,
}

/// `secure_input` is retained as an observation input for callers and tests,
/// but deliberately does not participate in the decision. A `Some(sample)`
/// means the display was awake and the locked session was readable; an absent
/// sample resets the candidate.
fn observe_readiness(
    state: &mut ReadinessState,
    at: Instant,
    _secure_input: bool,
    sample: Option<ReadinessSample>,
) -> bool {
    if state
        .last_observation
        .is_some_and(|last| at.saturating_duration_since(last) > READINESS_MAX_GAP)
    {
        state.candidate = None;
    }
    state.last_observation = Some(at);
    let Some(sample) = sample else {
        state.candidate = None;
        return false;
    };
    match state.candidate.as_ref() {
        Some((session, fingerprint, since)) if *session == sample.session && *fingerprint == sample.fingerprint => {
            at.saturating_duration_since(*since) >= READINESS_STABLE
        }
        _ => {
            state.candidate = Some((sample.session, sample.fingerprint, at));
            false
        }
    }
}

fn readiness_sample() -> Option<ReadinessSample> {
    // The window query is only attempted after the display wake check. This
    // keeps a sleeping display from arming a stale loginwindow fingerprint.
    // SAFETY: both calls are read-only CoreGraphics queries and the display ID
    // returned by CGMainDisplayID is valid for the process's main display.
    if unsafe { CGDisplayIsAsleep(CGMainDisplayID()) != 0 } {
        return None;
    }
    let session = locked_session().ok()?;
    let fingerprint = loginwindow_fingerprint()?;
    Some(ReadinessSample { session, fingerprint })
}

fn cf_string_is(value: Cf, expected: &str) -> bool {
    if value.is_null() {
        return false;
    }
    // SAFETY: value was checked non-null and is a borrowed CF object owned by
    // the window dictionary for the duration of this synchronous query.
    if unsafe { CFGetTypeID(value) != CFStringGetTypeID() } {
        return false;
    }
    let mut buffer = [0i8; 64];
    // SAFETY: fixed writable buffer and UTF-8 encoding. The result is used only
    // for the non-secret process owner name supplied by CoreGraphics.
    unsafe {
        CFStringGetCString(value, buffer.as_mut_ptr(), buffer.len() as isize, 0x0800_0100) != 0
            && CStr::from_ptr(buffer.as_ptr()).to_bytes() == expected.as_bytes()
    }
}

fn real_number(value: Cf) -> Option<f64> {
    if value.is_null() {
        return None;
    }
    // SAFETY: value was checked non-null and remains owned by its containing
    // CoreFoundation object while this type query runs.
    if unsafe { CFGetTypeID(value) != CFNumberGetTypeID() } {
        return None;
    }
    let mut out = 0.0f64;
    // kCFNumberFloat64Type = 6.
    // SAFETY: out is a valid writable f64 buffer and value has the required
    // CFNumber type; the call is synchronous and does not retain the pointer.
    if unsafe { CFNumberGetValue(value, 6, (&mut out as *mut f64).cast()) == 0 } {
        None
    } else if out.is_finite() {
        Some(out)
    } else {
        None
    }
}

fn window_number(d: Cf, name: &str) -> Option<f64> {
    real_number(get(d, name).ok()?)
}

fn loginwindow_fingerprint() -> Option<WindowFingerprint> {
    // kCGWindowListOptionOnScreenOnly; no titles or image data are requested.
    // SAFETY: CoreGraphics returns an owned CFArray reference or null; Owned
    // releases the returned reference exactly once.
    let windows = Owned(unsafe { CGWindowListCopyWindowInfo(1, 0) });
    if windows.0.is_null() {
        return None;
    }
    // SAFETY: windows.0 was checked non-null and remains live under Owned.
    if unsafe { CFGetTypeID(windows.0) != CFArrayGetTypeID() } {
        return None;
    }
    // SAFETY: windows.0 is a live CFArray returned by CoreGraphics.
    let count = unsafe { CFArrayGetCount(windows.0) };
    if count < 0 {
        return None;
    }
    let mut shapes = Vec::new();
    for index in 0..count {
        // SAFETY: index is within the count returned for this live CFArray;
        // the returned element is borrowed for the duration of this loop.
        let window = unsafe { CFArrayGetValueAtIndex(windows.0, index) };
        if window.is_null() {
            continue;
        }
        // SAFETY: window was checked non-null and is borrowed from windows.
        if unsafe { CFGetTypeID(window) != CFDictionaryGetTypeID() } {
            continue;
        }
        let owner = get(window, "kCGWindowOwnerName").ok()?;
        if !cf_string_is(owner, "loginwindow") {
            continue;
        }
        let number = window_number(window, "kCGWindowNumber")?.round() as i64;
        let pid = window_number(window, "kCGWindowOwnerPID")?.round() as i64;
        let layer = window_number(window, "kCGWindowLayer")?.round() as i64;
        let alpha = window_number(window, "kCGWindowAlpha")?;
        let onscreen = flag(get(window, "kCGWindowIsOnscreen").ok()?);
        if layer < 2000 || alpha < 0.99 || !onscreen {
            continue;
        }
        let bounds = get(window, "kCGWindowBounds").ok()?;
        if bounds.is_null() {
            continue;
        }
        // SAFETY: bounds was checked non-null and is borrowed from window.
        if unsafe { CFGetTypeID(bounds) != CFDictionaryGetTypeID() } {
            continue;
        }
        let bounds = [
            window_number(bounds, "X")?.round() as i64,
            window_number(bounds, "Y")?.round() as i64,
            window_number(bounds, "Width")?.round() as i64,
            window_number(bounds, "Height")?.round() as i64,
        ];
        shapes.push(WindowShape {
            number,
            pid,
            layer,
            bounds,
            alpha: alpha.to_bits(),
            onscreen,
        });
    }
    if shapes.is_empty() {
        return None;
    }
    shapes.sort_by_key(|shape| shape.number);
    Some(WindowFingerprint(shapes))
}
/// Own the remaining text so any guard failure wipes it on every exit. No batch
/// posting and no retries. Synthetic tests verify control flow only, not macOS.
#[allow(dead_code)]
fn guarded_text(
    text: Zeroizing<Vec<u8>>,
    eligible: impl FnMut() -> bool,
    locked: impl FnMut() -> Option<bool>,
    post: impl FnMut(Option<char>) -> Result<()>,
) -> Result<()> {
    guarded_text_at(text, std::time::Instant::now(), eligible, locked, post)
}
fn guarded_text_at(
    mut text: Zeroizing<Vec<u8>>,
    at: std::time::Instant,
    mut eligible: impl FnMut() -> bool,
    mut locked: impl FnMut() -> Option<bool>,
    mut post: impl FnMut(Option<char>) -> Result<()>,
) -> Result<()> {
    let mut transaction = super::typing::Typing::new(at);
    let mut offset = 0;
    while offset < text.len() {
        let mut c = std::str::from_utf8(&text[offset..])
            .map_err(|_| "macOS已存凭据格式无效")?
            .chars()
            .next()
            .ok_or(KEYCHAIN)?;
        let end = offset + c.len_utf8();
        let still_locked = locked();
        let admitted = eligible();
        let result = transaction
            .before_character(std::time::Instant::now(), admitted, still_locked)
            .and_then(|()| post(Some(c)));
        c.zeroize();
        text[offset..end].zeroize();
        offset = end;
        result?;
    }
    let still_locked = locked();
    let admitted = eligible();
    transaction.before_character(std::time::Instant::now(), admitted, still_locked)?;
    post(None)
}
/// Future local sink: guard immediately before EACH event, including Return and
/// key-up. No ordinary InputBackend diagnostics/serialization see secret text.
#[allow(dead_code)]
fn post_character(
    session: Session,
    character: Option<char>,
    deadline: std::time::Instant,
    begun: &mut bool,
    eligible: &dyn Fn() -> bool,
) -> Result<()> {
    // SAFETY: read-only permission preflight; never requests permission.
    if !unsafe { CGPreflightPostEventAccess() } {
        return Err("macOS输入注入权限不可用");
    }
    let mut units = Zeroizing::new([0u16; 2]);
    let len = character.map(|c| c.encode_utf16(&mut *units).len()).unwrap_or(0);
    for down in [true, false] {
        // SAFETY: null source is supported; scalar keycode/down, owned result.
        let event =
            owned(unsafe { CGEventCreateKeyboardEvent(ptr::null(), if character.is_some() { 0 } else { 36 }, down) })?;
        // SAFETY: owned event; Unicode copies at most two live UTF-16 units.
        unsafe {
            CGEventSetFlags(event.0, 0);
            // Same OINK self-injection tag as oink-input. Its tap passes the
            // event without routing it back onto the network.
            CGEventSetIntegerValueField(event.0, 42, 0x4f49_4e4b);
            if len != 0 {
                CGEventKeyboardSetUnicodeString(event.0, len, units.as_ptr());
            }
        }
        if down {
            let still_locked = locked_session().is_ok_and(|now| now == session);
            let in_time = std::time::Instant::now() < deadline;
            // Last admission read immediately before every OS post, after
            // CF/IO work. Pointer/focus changes do not revoke this local
            // transaction; pause/update/disconnect do. A key-up is cleanup for
            // a posted key-down and must still be emitted if the lock state
            // changes between the two.
            let admitted = eligible();
            let allowed = still_locked && in_time && admitted;
            if !allowed {
                // Wipe our CGEvent's Unicode payload before release.
                // SAFETY: event.0 is the live event owned by this scope; zero
                // length permits the null text pointer and clears its payload.
                unsafe { CGEventKeyboardSetUnicodeString(event.0, 0, ptr::null()) };
                return Err(if !still_locked {
                    LOCK_CHANGED
                } else if !in_time {
                    "自动输入超过两秒，已停止并释放暂存按键"
                } else if *begun {
                    "自动输入期间状态改变，已停止自动解锁"
                } else {
                    "输入开始前焦点或连接状态改变，已取消自动解锁"
                });
            }
            // SAFETY: owned keyboard event, valid HID tap constant.
            unsafe { CGEventPost(0, event.0) };
            *begun = true;
            if character.is_none() {
                // Return submitted Oink's password: an unlock edge in the next
                // five seconds is Oink's own and must not fan out again.
                super::sync::mark_own_unlock();
            }
        } else {
            // This is deliberately unconditional after a successful down. It
            // prevents a stale modifier/key state when Touch ID or another
            // local action unlocks the session during the character.
            // SAFETY: event.0 is the live keyboard event owned by this scope;
            // CGEventPost consumes it synchronously before it is released.
            unsafe { CGEventPost(0, event.0) };
        }
        // Wipe our CGEvent's Unicode payload before release. A posted OS copy
        // cannot be withdrawn/wiped; its containment remains unverified.
        // SAFETY: owned event and zero length permit a null Unicode pointer.
        unsafe { CGEventKeyboardSetUnicodeString(event.0, 0, ptr::null()) };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_input_stays_closed_when_display_wake_is_unavailable() {
        let input = oink_test_support::input::FakeInput::new();
        assert_eq!(
            password_field_ready_for(&input, Duration::ZERO),
            Err("macOS 锁屏密码框未在规定时间内就绪")
        );
    }
    #[test]
    fn local_text_ignores_pointer_departure_but_stops_on_lock_change() {
        use std::cell::Cell;
        let posted = Cell::new(0);
        let result = guarded_text(
            Zeroizing::new(b"fake".to_vec()),
            || true,
            || if posted.get() < 2 { Some(true) } else { None },
            |_| {
                posted.set(posted.get() + 1);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(posted.get(), 2);
        let posted = Cell::new(0);
        let result = guarded_text(
            Zeroizing::new(b"fake".to_vec()),
            || posted.get() == 0,
            || Some(true),
            |_| {
                posted.set(posted.get() + 1);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(posted.get(), 1);
        let result = guarded_text(
            Zeroizing::new(b"fake".to_vec()),
            || false,
            || Some(true),
            |_| panic!("no first character when entry revoked"),
        );
        assert!(result.is_err());
    }
    use super::*;
    use std::time::{Duration, Instant};
    #[test]
    fn aborts_remaining_characters_and_return_on_guard_failure() {
        let mut calls = 0;
        let result = guarded_text(
            Zeroizing::new(b"fake-only".to_vec()),
            || true,
            || Some(true),
            |_| {
                calls += 1;
                if calls == 3 { Err(LOCK_CHANGED) } else { Ok(()) }
            },
        );
        assert_eq!(result, Err(LOCK_CHANGED));
        assert_eq!(calls, 3);
    }
    #[test]
    fn return_has_its_own_guard_and_is_never_retried() {
        let mut chars = 0;
        let mut returns = 0;
        let result = guarded_text(
            Zeroizing::new("f🦀".as_bytes().to_vec()),
            || true,
            || Some(true),
            |c| {
                if c.is_some() {
                    chars += 1;
                    Ok(())
                } else {
                    returns += 1;
                    Err(LOCK_CHANGED)
                }
            },
        );
        assert_eq!(result, Err(LOCK_CHANGED));
        assert_eq!((chars, returns), (2, 1));
    }

    #[test]
    fn readiness_needs_lit_locked_stable_loginwindow_but_not_secure_input() {
        let session = Session {
            uid: 501,
            audit: 100022,
        };
        let fingerprint = WindowFingerprint(vec![WindowShape {
            number: 7,
            pid: 397,
            layer: 2004,
            bounds: [0, 0, 1728, 1117],
            alpha: 1.0f64.to_bits(),
            onscreen: true,
        }]);
        let sample = ReadinessSample { session, fingerprint };
        let at = Instant::now();
        let mut state = ReadinessState::default();
        assert!(!observe_readiness(&mut state, at, false, Some(sample.clone())));
        for milliseconds in (20..=580).step_by(20) {
            assert!(!observe_readiness(
                &mut state,
                at + Duration::from_millis(milliseconds),
                true,
                Some(sample.clone())
            ));
        }
        assert!(!observe_readiness(
            &mut state,
            at + Duration::from_millis(599),
            true,
            Some(sample.clone())
        ));
        // Secure Input is intentionally absent from this decision table.
        assert!(observe_readiness(
            &mut state,
            at + Duration::from_millis(600),
            true,
            Some(sample)
        ));
    }

    #[test]
    fn readiness_resets_after_unknown_or_changed_window_state() {
        let session = Session {
            uid: 501,
            audit: 100022,
        };
        let first = ReadinessSample {
            session,
            fingerprint: WindowFingerprint(vec![WindowShape {
                number: 7,
                pid: 397,
                layer: 2004,
                bounds: [0, 0, 1728, 1117],
                alpha: 1.0f64.to_bits(),
                onscreen: true,
            }]),
        };
        let changed = ReadinessSample {
            fingerprint: WindowFingerprint(vec![WindowShape {
                number: 8,
                ..first.fingerprint.0[0]
            }]),
            ..first.clone()
        };
        let at = Instant::now();
        let mut state = ReadinessState::default();
        assert!(!observe_readiness(&mut state, at, true, Some(first.clone())));
        assert!(!observe_readiness(
            &mut state,
            at + Duration::from_millis(590),
            false,
            None
        ));
        assert!(!observe_readiness(
            &mut state,
            at + Duration::from_millis(600),
            true,
            Some(changed.clone())
        ));
        for milliseconds in (620..=1180).step_by(20) {
            assert!(!observe_readiness(
                &mut state,
                at + Duration::from_millis(milliseconds),
                true,
                Some(changed.clone())
            ));
        }
        assert!(!observe_readiness(
            &mut state,
            at + Duration::from_millis(1199),
            true,
            Some(changed.clone())
        ));
        assert!(observe_readiness(
            &mut state,
            at + Duration::from_millis(1200),
            true,
            Some(changed)
        ));
    }
}
