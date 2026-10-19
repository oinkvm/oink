//! Device-list invalidation only; no timers, enumeration or endpoint IDs here.
//! macOS retains its existing HAL cache/listeners. Other platforms can always
//! refresh explicitly, including if native notification registration fails.
#[cfg(windows)]
#[path = "windows_devices.rs"]
mod windows;
#[cfg(not(target_os = "macos"))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(target_os = "macos"))]
static GENERATION: AtomicU64 = AtomicU64::new(1);

#[cfg(windows)]
pub(crate) fn note_windows_notification(default: bool) {
    windows_history::note(default);
}
/// Diagnostic-only source context at a volume press; never affects routing.
pub fn recent_windows_notifications() -> Option<oink_foundation::input::RecentAudioNotifications> {
    #[cfg(windows)]
    {
        Some(windows_history::recent())
    }
    #[cfg(not(windows))]
    {
        None
    }
}
#[cfg(any(windows, test))]
mod windows_history {
    use std::sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    };
    use std::time::Instant;
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    static DEFAULT: AtomicU64 = AtomicU64::new(0);
    static ARRIVAL_REMOVAL: AtomicU64 = AtomicU64::new(0);
    pub(super) fn initialize() {
        ORIGIN.get_or_init(Instant::now);
    }
    fn now() -> u64 {
        ORIGIN.get().map_or(0, |origin| origin.elapsed().as_millis() as u64 + 1)
    }
    pub(super) fn note(default: bool) {
        // Initialized before native registration. No allocation, lock, query or
        // log in MMDevice callbacks; timestamps describe notifications, not a
        // claimed causal relation between hardware and a later key event.
        let stamp = now();
        if stamp != 0 {
            (if default { &DEFAULT } else { &ARRIVAL_REMOVAL }).store(stamp, Ordering::Release);
        }
    }
    fn age(at: u64, previous: u64) -> Option<u32> {
        (previous != 0)
            .then(|| at.saturating_sub(previous))
            .filter(|age| *age <= 5000)
            .map(|age| age as u32)
    }
    pub(super) fn recent() -> oink_foundation::input::RecentAudioNotifications {
        let at = now();
        oink_foundation::input::RecentAudioNotifications {
            default_endpoint_age_ms: age(at, DEFAULT.load(Ordering::Acquire)),
            arrival_removal_age_ms: age(at, ARRIVAL_REMOVAL.load(Ordering::Acquire)),
        }
    }
    #[test]
    fn ages_expire_without_a_timer_or_treating_startup_as_a_notification() {
        assert_eq!(age(10001, 0), None);
        assert_eq!(age(10001, 5001), Some(5000));
        assert_eq!(age(10001, 5000), None);
        initialize();
        note(true);
        note(false);
        let snapshot = recent();
        assert!(snapshot.default_endpoint_age_ms.is_some());
        assert!(snapshot.arrival_removal_age_ms.is_some());
    }
}

pub fn generation() -> u64 {
    #[cfg(target_os = "macos")]
    {
        crate::macos_devices::generation()
    }
    #[cfg(not(target_os = "macos"))]
    {
        #[cfg(windows)]
        windows::start();
        GENERATION.load(Ordering::Acquire)
    }
}

/// Settings/list request, or a native callback. No allocation, lock or query.
pub fn invalidate() {
    #[cfg(target_os = "macos")]
    crate::macos_devices::invalidate();
    #[cfg(not(target_os = "macos"))]
    {
        GENERATION.fetch_add(1, Ordering::Release);
        notify();
    }
}

/// Changes only on a CoreAudio service-restart notification, not hotplug.
pub fn service_generation() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        Some(crate::macos_devices::service_generation())
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

// Notification fan-out uses immutable snapshots: native callbacks neither lock
// nor enumerate devices. Consumers retain their callback for their own lifetime.
type Callback = std::sync::Arc<dyn Fn() + Send + Sync>;
type Listeners = oink_foundation::snapshot::Snapshot<Vec<std::sync::Weak<dyn Fn() + Send + Sync>>>;
static LISTENERS: std::sync::OnceLock<Listeners> = std::sync::OnceLock::new();
pub fn subscribe(wake: Callback) -> Callback {
    static REGISTER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = REGISTER.lock().unwrap_or_else(|e| e.into_inner());
    let listeners = LISTENERS.get_or_init(Default::default);
    let mut next = listeners.load().as_ref().clone();
    next.retain(|w| w.strong_count() != 0);
    next.push(std::sync::Arc::downgrade(&wake));
    listeners.publish(next);
    #[cfg(target_os = "macos")]
    crate::macos_devices::set_wake(notify);
    wake
}
pub(crate) extern "C" fn notify() {
    if let Some(listeners) = LISTENERS.get() {
        for listener in listeners.load().iter() {
            if let Some(wake) = listener.upgrade() {
                wake();
            }
        }
    }
}
