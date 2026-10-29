//! Event-driven recovery of unowned native drag panels.
use super::*;
use block2::RcBlock;
use objc2_app_kit::NSEventMask;
use std::ptr::NonNull;

thread_local! {
    static VISIBLE_SINCE: std::cell::Cell<[Option<Instant>; 2]> = const { std::cell::Cell::new([None; 2]) };
}

fn index(role: Role) -> usize {
    match role {
        Role::Catcher => 0,
        Role::Source => 1,
    }
}

pub(super) fn shown(role: Role, already_visible: bool) {
    VISIBLE_SINCE.with(|times| {
        let mut next = times.get();
        if !already_visible || next[index(role)].is_none() {
            next[index(role)] = Some(Instant::now());
        }
        times.set(next);
    });
}

pub(super) fn hide(panel: &NSPanel, role: Role) {
    panel.orderOut(None);
    VISIBLE_SINCE.with(|times| {
        let mut next = times.get();
        next[index(role)] = None;
        times.set(next);
    });
}

pub(super) struct Monitors(Vec<Retained<AnyObject>>);
impl Drop for Monitors {
    fn drop(&mut self) {
        for monitor in &self.0 {
            // SAFETY: Ui and its AppKit monitor tokens live on the main thread.
            unsafe { NSEvent::removeMonitor(monitor) };
        }
    }
}

pub(super) fn monitors(shared: &Arc<Shared>) -> Monitors {
    // Observe actual button-up events, not a timer. Local and global monitors
    // cover our panels and other apps respectively; neither changes or posts
    // input. Mouse moves are not observed: a leaked panel only needs checking
    // when the buttons are released (or pressed on it), and a per-move check
    // would run on the main thread that also serves cursor and drag callbacks.
    // A zero pressedMouseButtons sample is an observed state, not an elapsed
    // time guess. Normal probes and held drags are protected in recover().
    let mask = NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp | NSEventMask::OtherMouseUp;
    let weak = Arc::downgrade(shared);
    let global = RcBlock::new(move |_event: NonNull<NSEvent>| {
        if let Some(shared) = weak.upgrade() {
            buttons_released(&shared);
        }
    });
    let global = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &global);
    let weak = Arc::downgrade(shared);
    let local = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        if let Some(shared) = weak.upgrade() {
            buttons_released(&shared);
        }
        event.as_ptr()
    });
    // SAFETY: AppKit supplies the live event; this main-thread handler returns
    // that same pointer unchanged. AppKit retains the copied block.
    let local = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &local) };
    Monitors(global.into_iter().chain(local).collect())
}

fn buttons_released(shared: &Shared) {
    if NSEvent::pressedMouseButtons() == 0 {
        for role in [Role::Catcher, Role::Source] {
            observe(shared, role, Trigger::ButtonsReleased);
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Trigger {
    MouseDown = 1,
    ButtonsReleased = 2,
}

#[derive(Clone, Copy)]
struct Window {
    role: Role,
    visible: bool,
    visible_for: Duration,
    source_owned: bool,
}

pub(super) fn observe(shared: &Shared, role: Role, trigger: Trigger) -> bool {
    if MainThreadMarker::new().is_none() {
        return false;
    }
    let window = UI.with(|ui| {
        let ui = ui.borrow();
        let ui = ui.as_ref()?;
        let panel = match role {
            Role::Catcher => &ui.catcher,
            Role::Source => &ui.source,
        };
        Some(Window {
            role,
            visible: panel.isVisible(),
            visible_for: VISIBLE_SINCE.with(|times| times.get()[index(role)].map_or(Duration::ZERO, |at| at.elapsed())),
            source_owned: ui.pending.is_some() || ui.target_session.is_some(),
        })
    });
    let Some(window) = window else { return false };
    recover(shared, trigger, window, || {
        UI.with(|ui| {
            if let Some(ui) = ui.borrow_mut().as_mut() {
                match role {
                    Role::Catcher => hide(&ui.catcher, role),
                    Role::Source => {
                        hide(&ui.source, role);
                        if let Some(provider) = ui.current.take() {
                            provider.cancel();
                        }
                        ui.current_proxies.clear();
                    }
                }
            }
        });
    })
}

fn recover(shared: &Shared, trigger: Trigger, window: Window, hide: impl FnOnce()) -> bool {
    let (cleanup, recovered, operation, probing, probe_owned, held) = {
        let mut st = shared.lock();
        // User-approved safety net (2026-10-28), deliberately separate from
        // fixing lifecycle owners. A fresh DOWN cannot continue the drag for
        // which a catcher was shown. Never replay that swallowed click. With
        // all buttons up, require BOTH no pending probe and no cleanup owner:
        // a normal probe has no operation before detection/hold. Source also
        // serves as the cancel/reject panel; its pending/start/native session
        // are real ownership and must survive either observation. A held
        // source always wins: record the anomaly but never withdraw its panel.
        if !window.visible
            || (window.role == Role::Source && (window.source_owned || st.active || st.starting_operation.is_some()))
        {
            return false;
        }
        if matches!(trigger, Trigger::ButtonsReleased)
            && window.role == Role::Catcher
            && (st.probing || st.probe_operation.is_some())
        {
            return false;
        }
        let operation = match window.role {
            Role::Catcher => st.probe_operation.or(st.last_probe_operation),
            Role::Source => st.last_source_operation,
        };
        let probing = st.probing;
        let probe_owned = st.probe_operation.is_some();
        let held = st.held_source;
        let recovered = held.is_none();
        let cleanup = (recovered && window.role == Role::Catcher).then(|| {
            st.clear_probe_state();
            st.take_probe_cleanup()
        });
        (cleanup, recovered, operation, probing, probe_owned, held)
    };
    if recovered {
        // The caller is already on AppKit's main thread. Complete state,
        // receiver and panel cleanup in this turn, never queue a later hide
        // that could race a new probe or source window.
        if let Some(cleanup) = cleanup {
            cleanup.finish();
        }
        hide();
    }
    tracing::warn!(target: "oink_dnd_recovery",
        dnd_recovery_trigger = trigger as u64,
        dnd_recovery_role = index(window.role) as u64,
        dnd_recovery_visible_ms = window.visible_for.as_millis().min(u64::MAX as u128) as u64,
        dnd_recovery_operation = operation.map_or(0, |op| op.sequence),
        dnd_recovery_probing = probing,
        dnd_recovery_probe_owned = probe_owned,
        dnd_recovery_held_source = held.map_or(0, |op| op.sequence),
        dnd_recovery_recovered = recovered,
        "drag panel safety net observed a stranded window");
    recovered
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tracing_subscriber::{Layer, layer::SubscriberExt};

    #[derive(Clone, Default)]
    struct Records(Arc<Mutex<Vec<String>>>);
    impl<S: tracing::Subscriber> Layer<S> for Records {
        fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
            if event.metadata().target() == "oink_dnd_recovery" {
                assert_eq!(*event.metadata().level(), tracing::Level::WARN);
                event.record(&mut Fields(&mut self.0.lock().unwrap()));
            }
        }
    }
    struct Fields<'a>(&'a mut Vec<String>);
    impl tracing::field::Visit for Fields<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.push(format!("{}={value:?}", field.name()));
        }
    }

    fn window(role: Role) -> Window {
        Window {
            role,
            visible: true,
            visible_for: Duration::from_secs(60),
            source_owned: false,
        }
    }

    #[test]
    fn unowned_catcher_mouse_down_cleans_up_and_records_without_replaying() {
        let backend = MacDnd::new();
        let operation = DragOperation::new();
        let budget = crate::macos::thumbnail::Budget::new(Instant::now());
        {
            let mut st = backend.shared.lock();
            st.probing = true;
            st.probe_operation = Some(operation);
            st.reported_seq = Some(42);
            st.thumbnail_budget = Some(budget.clone());
        }
        backend
            .shared
            .inject
            .set(Arc::new(|_| panic!("recovery must never replay input")))
            .ok();
        let records = Records::default();
        let hidden = AtomicBool::new(false);
        tracing::subscriber::with_default(tracing_subscriber::registry().with(records.clone()), || {
            assert!(recover(
                &backend.shared,
                Trigger::MouseDown,
                window(Role::Catcher),
                || {
                    hidden.store(true, Ordering::Relaxed);
                }
            ));
        });
        assert!(hidden.load(Ordering::Relaxed));
        let st = backend.shared.lock();
        assert!(!st.probing);
        assert!(st.probe_operation.is_none());
        assert!(st.reported_seq.is_none());
        assert!(!budget.current());
        drop(st);
        let records = records.0.lock().unwrap();
        assert!(records.iter().any(|v| v == "dnd_recovery_trigger=1"));
        assert!(records.iter().any(|v| v == "dnd_recovery_visible_ms=60000"));
        assert!(records.iter().any(|v| v == "dnd_recovery_recovered=true"));
    }

    #[test]
    fn held_source_mouse_down_records_but_never_hides() {
        let backend = MacDnd::new();
        let operation = DragOperation::new();
        backend.shared.lock().held_source = Some(operation);
        let records = Records::default();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(records.clone()), || {
            for role in [Role::Catcher, Role::Source] {
                for trigger in [Trigger::MouseDown, Trigger::ButtonsReleased] {
                    assert!(!recover(&backend.shared, trigger, window(role), || panic!("held")));
                }
            }
        });
        assert_eq!(backend.shared.lock().held_source, Some(operation));
        assert!(
            records
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|v| v == "dnd_recovery_recovered=false")
        );
    }

    #[test]
    fn released_buttons_only_recover_visible_unowned_panels() {
        let backend = MacDnd::new();
        for role in [Role::Catcher, Role::Source] {
            assert!(recover(&backend.shared, Trigger::ButtonsReleased, window(role), || {}));
            let hidden = Window {
                visible: false,
                ..window(role)
            };
            assert!(!recover(&backend.shared, Trigger::ButtonsReleased, hidden, || panic!(
                "hidden"
            )));
        }
        backend.shared.lock().request_probe();
        assert!(!recover(
            &backend.shared,
            Trigger::ButtonsReleased,
            window(Role::Catcher),
            || panic!("probe")
        ));
        {
            let mut st = backend.shared.lock();
            st.probing = false;
            st.probe_operation = Some(DragOperation::new());
        }
        assert!(!recover(
            &backend.shared,
            Trigger::ButtonsReleased,
            window(Role::Catcher),
            || panic!("owned")
        ));
        let owned_source = Window {
            source_owned: true,
            ..window(Role::Source)
        };
        for trigger in [Trigger::MouseDown, Trigger::ButtonsReleased] {
            assert!(!recover(&backend.shared, trigger, owned_source, || panic!(
                "source owns panel"
            )));
        }
    }

    #[test]
    fn source_start_and_active_cancel_panel_are_never_orphaned() {
        let backend = MacDnd::new();
        for active in [true, false] {
            {
                let mut st = backend.shared.lock();
                st.active = active;
                st.starting_operation = (!active).then(DragOperation::new);
            }
            for trigger in [Trigger::MouseDown, Trigger::ButtonsReleased] {
                assert!(!recover(&backend.shared, trigger, window(Role::Source), || panic!(
                    "native source"
                )));
            }
        }
    }

    #[test]
    fn queued_unprobe_cannot_retire_a_new_probe_before_its_window_is_shown() {
        let mut st = St::default();
        assert!(st.request_probe());
        let generation = st.probe_generation;
        assert!(st.request_probe());
        assert!(!st.retire_probe_generation(generation));
        assert!(st.probing);
        assert!(st.retire_probe_generation(st.probe_generation));
        assert!(!st.probing);
    }

    #[test]
    fn repeated_window_positioning_preserves_visible_duration() {
        for role in [Role::Catcher, Role::Source] {
            shown(role, false);
            let original = VISIBLE_SINCE.with(|times| times.get()[index(role)].unwrap());
            shown(role, true);
            assert_eq!(VISIBLE_SINCE.with(|times| times.get()[index(role)]), Some(original));
        }
    }

    #[test]
    fn cleanup_removes_orphan_receivers_but_keeps_held_and_started_export_receivers() {
        let held = DragOperation::new();
        let exporting = DragOperation::new();
        let orphan = DragOperation::new();
        let mut receivers = [held, exporting, orphan]
            .into_iter()
            .map(|operation| (operation.sequence, ()))
            .collect();
        ProbeCleanup {
            unused: None,
            retained_receivers: [Some(held), Some(exporting)],
        }
        .retain_receivers(&mut receivers);
        assert_eq!(receivers.len(), 2);
        assert!(receivers.contains_key(&held.sequence));
        assert!(receivers.contains_key(&exporting.sequence));
        ProbeCleanup {
            unused: None,
            retained_receivers: [None; 2],
        }
        .retain_receivers(&mut receivers);
        assert!(receivers.is_empty());
    }
}
