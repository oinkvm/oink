//! Ordinary-priority platform observations. The capture owner publishes held state and
//! consumes immutable samples; a stalled observer cannot delay a native callback.
use super::*;
use oink_foundation::snapshot::Snapshot;

#[derive(Default, PartialEq)]
struct Held {
    keys: Vec<KeyCode>,
    buttons: Vec<MouseButton>,
}
struct Sample {
    held: Arc<Held>,
    at: Instant,
    keys: Vec<KeyCode>,
    buttons: Vec<MouseButton>,
    secure: bool,
    cursor: Point,
}
struct HealthPort(CFRetained<CFMachPort>);
// SAFETY: this retained port is used only for CGEventTapIsEnabled/Enable on the
// observer. Snapshots retain an old port across withdrawal; enabling an invalidated
// port cannot enable its replacement. CoreFoundation retains/releases and native identity are
// thread safe; no callback context or run-loop ownership is accessed here.
unsafe impl Send for HealthPort {}
// SAFETY: the same retained, thread-safe CoreFoundation identity is read by one observer.
unsafe impl Sync for HealthPort {}

pub(super) struct State {
    held: Snapshot<Held>,
    sample: Snapshot<Option<Sample>>,
    worker: OnceLock<std::thread::Thread>,
    pub(super) reset_caps: AtomicBool,
    port: Snapshot<Option<HealthPort>>,
}
impl State {
    pub(super) fn set_port(&self, port: Option<CFRetained<CFMachPort>>) {
        self.port.publish(port.map(HealthPort));
    }
    pub(super) fn new() -> Self {
        Self {
            held: Snapshot::default(),
            sample: Snapshot::default(),
            worker: OnceLock::new(),
            reset_caps: AtomicBool::new(false),
            port: Snapshot::default(),
        }
    }
    pub(super) fn spawn(s: &'static Shared) -> std::io::Result<JoinHandle<()>> {
        std::thread::Builder::new()
            .name("oink-input-observer".into())
            .spawn(move || {
                let state = s.observations.as_ref().unwrap();
                let _ = state.worker.set(std::thread::current());
                let mut session = super::super::session::snapshot();
                let mut power = crate::display_power::Monitor::new().ok();
                let mut visibility = cursor::Reconcile::default();
                let mut freeze = FreezeWatch::default();
                let mut diagnostics_at = Instant::now();
                let mut health_at = Instant::now();
                let mut observation_at = Instant::now();
                while !s.stop.load(Ordering::Acquire) {
                    let pending = s.pending.fetch_and(PENDING_RECONCILE, Ordering::AcqRel);
                    let remote = s.remote.load(Ordering::Acquire);
                    if pending & PENDING_APPLY_MODE != 0 || (!remote && s.owner_posts_snapshot().count != 0) {
                        apply_mode(s);
                    }
                    if pending & PENDING_OWNER_MOVE != 0 {
                        apply_owner_move(s);
                    }
                    if pending & PENDING_CAPS_LEAVE != 0 && !s.keyboard_remote.load(Ordering::Acquire) {
                        s.caps_guard.leave_remote();
                    }
                    if pending & PENDING_CAPS_ENTER != 0 && s.keyboard_remote.load(Ordering::Acquire) {
                        s.caps_guard.snapshot_source();
                    }
                    if s.keyboard_remote.load(Ordering::Acquire)
                        && (pending & PENDING_CAPS_GUARD != 0 || s.caps_guard.active())
                    {
                        s.caps_guard.enforce();
                    }
                    let held = state.held.load();
                    let now = Instant::now();
                    if now >= observation_at {
                        observation_at = now + Duration::from_millis(50);
                        if !held.keys.is_empty() || !held.buttons.is_empty() {
                            let locked = super::super::session::locked() == Some(true);
                            let keys = released(held.keys.iter().copied(), key_physically_down);
                            let buttons = if locked {
                                held.buttons.clone()
                            } else {
                                released(held.buttons.iter().copied(), button_physically_down)
                            };
                            let secure = locked || (!held.keys.is_empty() && system::secure_input_active());
                            let cursor = if buttons.is_empty() {
                                Point::default()
                            } else {
                                let p = current_location();
                                Point::new(p.x, p.y)
                            };
                            state.sample.publish(Some(Sample {
                                held: held.clone(),
                                at: now,
                                keys,
                                buttons,
                                secure,
                                cursor,
                            }));
                            wake(s);
                        }
                    }
                    if now >= diagnostics_at {
                        diagnostics_at = now + Duration::from_millis(250);
                        let current = super::super::session::snapshot();
                        let transition = current != session || power.as_mut().and_then(|m| m.poll()).is_some();
                        session = current;
                        {
                            let _guard = s.cursor_lock.lock().unwrap_or_else(|e| e.into_inner());
                            visibility.check(transition, remote);
                        }
                        if remote {
                            check_frozen(s, &mut freeze);
                        }
                    }
                    if now >= health_at {
                        health_at = now + Duration::from_secs(5);
                        super::super::physical_motion::report();
                        let port = state.port.load();
                        if let Some(port) = &*port {
                            if s.capture_permitted() && !CGEvent::tap_is_enabled(&port.0) {
                                s.ordinary_buttons
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .invalidate();
                                CGEvent::tap_enable(&port.0, true);
                                s.pending.fetch_or(PENDING_RECONCILE, Ordering::AcqRel);
                                wake(s);
                            }
                            super::super::keyboard_diagnostics::report(
                                remote,
                                held.keys.len(),
                                true,
                                CGEvent::tap_is_enabled(&port.0),
                            );
                        }
                    }
                    // Held-key recovery has work only while a key/button is held.
                    // A new held snapshot already unparks this observer.
                    let mut next = diagnostics_at.min(health_at);
                    if !held.keys.is_empty() || !held.buttons.is_empty() {
                        next = next.min(observation_at);
                    }
                    std::thread::park_timeout(next.saturating_duration_since(Instant::now()));
                }
            })
    }
    pub(super) fn wake(&self) {
        if let Some(thread) = self.worker.get() {
            thread.unpark();
        }
    }
}
#[derive(Default)]
pub(super) struct Reader {
    previous: Option<Arc<Option<Sample>>>,
    keys: LostUps,
    buttons: HashMap<MouseButton, Instant>,
}
impl Reader {
    fn sample_keys(&mut self, current: &Arc<Held>, sample: &Sample) -> Option<Vec<KeyCode>> {
        if !Arc::ptr_eq(&sample.held, current) {
            return None;
        }
        Some(if sample.secure {
            sample.held.keys.clone()
        } else {
            self.keys.confirm(&sample.keys, sample.at)
        })
    }
    pub(super) fn poll(&mut self, s: &Shared) {
        let state = s.observations.as_ref().unwrap();
        if state.reset_caps.swap(false, Ordering::AcqRel) || {
            let reconcile = s.pending.fetch_and(!PENDING_RECONCILE, Ordering::AcqRel) & PENDING_RECONCILE != 0;
            if reconcile {
                // The health observer may re-enable capture while this owner
                // sleeps. A gap is unknown, not proof of a missing up.
                let available = state
                    .port
                    .load()
                    .as_ref()
                    .as_ref()
                    .is_some_and(|port| CGEvent::tap_is_enabled(&port.0));
                super::button_evidence::boundary(crate::button_diagnostics::Layer::SessionTap, available);
            }
            reconcile
        } {
            reset_caps_gesture(s, "capture or focus boundary");
        }
        let held = Held {
            keys: PRESSED.with(|keys| keys.borrow().iter().copied().collect()),
            buttons: BUTTONS.with(|buttons| buttons.borrow().iter().copied().collect()),
        };
        if *state.held.load() != held {
            state.held.publish(held);
            state.wake();
            self.keys.clear();
            self.buttons.clear();
        }
        let sample = state.sample.load();
        if self
            .previous
            .as_ref()
            .is_some_and(|previous| Arc::ptr_eq(previous, &sample))
        {
            return;
        }
        self.previous = Some(sample.clone());
        let Some(sample) = sample.as_ref() else {
            return;
        };
        let Some(keys) = self.sample_keys(&state.held.load(), sample) else {
            return;
        };
        for code in keys {
            if PRESSED.with(|p| p.borrow_mut().remove(&code)) {
                if let Some(key) = disp_key(code) {
                    KEY_DISPOSITION.with(|d| d.borrow_mut().remove(&key));
                }
                release_key(s, code);
            }
        }
        self.buttons.retain(|button, _| sample.buttons.contains(button));
        for &button in &sample.buttons {
            let first = *self.buttons.entry(button).or_insert(sample.at);
            if sample.at.duration_since(first) >= LOST_UP_CONFIRM && BUTTONS.with(|p| p.borrow_mut().remove(&button)) {
                self.buttons.remove(&button);
                s.physical_buttons.record(button_number(button), false);
                call_handler(
                    s,
                    &CapturedEvent {
                        physical: false,
                        event: InputEvent::Button { button, pressed: false },
                        source: SourceKind::Unknown,
                        time_us: now_us(),
                        cursor: Some(sample.cursor),
                    },
                );
            }
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn delayed_hardware_sample_cannot_release_a_new_press() {
        let key = KeyCode::keyboard(4);
        let old = Arc::new(Held {
            keys: vec![key],
            buttons: vec![],
        });
        let new = Arc::new(Held {
            keys: vec![key],
            buttons: vec![],
        });
        let sample = Sample {
            held: old,
            at: Instant::now(),
            keys: vec![key],
            buttons: vec![],
            secure: true,
            cursor: Point::default(),
        };
        assert!(Reader::default().sample_keys(&new, &sample).is_none());
    }
    #[test]
    fn missing_up_requires_distinct_hardware_observations_and_secure_input_releases_promptly() {
        let key = KeyCode::keyboard(4);
        let held = Arc::new(Held {
            keys: vec![key],
            buttons: vec![],
        });
        let mut sample = Sample {
            held: held.clone(),
            at: Instant::now(),
            keys: vec![key],
            buttons: vec![],
            secure: false,
            cursor: Point::default(),
        };
        let mut reader = Reader::default();
        assert!(reader.sample_keys(&held, &sample).unwrap().is_empty());
        sample.at += Duration::from_millis(50);
        assert_eq!(reader.sample_keys(&held, &sample).unwrap(), vec![key]);
        sample.secure = true;
        sample.keys.clear();
        assert_eq!(reader.sample_keys(&held, &sample).unwrap(), vec![key]);
    }
}
