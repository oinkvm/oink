use super::*;
use oink_foundation::input::{CapturedEvent, InjectEvent, InputEvent, KeyCode};
use std::collections::HashMap;

/// Deliberately neither Serialize nor Debug. The only owner is the recorder's
/// in-memory one-slot pool, never its row queue, store, snapshot or wire stream.
#[derive(Default)]
pub(super) struct Presses {
    generation: u64,
    held: HashMap<(Origin, Option<DeviceId>, KeyCode), u64>,
}
impl Recorder {
    pub(super) fn held_press_ids(&self, codes: &[Option<KeyCode>], sent: bool, source: DeviceId) -> Vec<u64> {
        let Some(presses) = self.inner.presses.pop() else {
            return Vec::new();
        };
        let ids = if presses.generation == self.inner.press_generation.load(Ordering::Relaxed) {
            codes
                .iter()
                .flatten()
                .filter_map(|code| {
                    let key = if sent {
                        (Origin::Physical, None, *code)
                    } else {
                        (Origin::Remote, Some(source), *code)
                    };
                    presses.held.get(&key).copied()
                })
                .collect()
        } else {
            Vec::new()
        };
        let _ = self.inner.presses.push(presses);
        ids
    }

    pub(super) fn annotate_key(
        &self,
        event: &mut SafeInput,
        code: Option<KeyCode>,
        stream: Origin,
        mut peer: Option<DeviceId>,
    ) -> Option<DeviceId> {
        let (
            SafeInput::Key {
                pressed,
                repeat,
                diagnostic,
                ..
            },
            Some(code),
        ) = (event, code)
        else {
            return peer;
        };
        if !self.enabled() {
            return peer;
        }
        // Exclusive ownership without a lock or a wait. Contention is diagnostic
        // loss, never input backpressure. Invalidate pairings after such a gap.
        let Some(mut presses) = self.inner.presses.pop() else {
            self.inner.press_generation.fetch_add(1, Ordering::Relaxed);
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            return peer;
        };
        let generation = self.inner.press_generation.load(Ordering::Relaxed);
        if presses.generation != generation {
            presses.held.clear();
            presses.generation = generation;
        }
        // Timer/pause cleanup has no triggering input message. Attribute an UP
        // only if exactly one injection ledger holds it; never guess between peers.
        if stream == Origin::Injection && peer.is_none() && !*pressed {
            let mut candidates = presses.held.keys().filter(|(s, _, k)| *s == stream && *k == code);
            let first = candidates.next().copied();
            if candidates.next().is_some() {
                // This UP ends an unknown one of these presses. Forget every
                // candidate so a later DOWN cannot reuse a pre-UP identity.
                presses.held.retain(|(s, _, k), _| *s != stream || *k != code);
            } else if let Some((_, candidate, _)) = first {
                peer = candidate;
            }
        }
        let key = (stream, peer, code);
        let old = presses.held.get(&key).copied();
        diagnostic.pressed_while_held |= *pressed && *repeat == Some(false) && old.is_some();
        diagnostic.press_id = if !*pressed {
            presses.held.remove(&key)
        } else if old.is_some() {
            old
        } else if *repeat == Some(true) {
            None // an orphan repeat does not prove we observed the press
        } else if presses.held.len() < 8192 {
            let id = self.inner.press_sequence.fetch_add(1, Ordering::Relaxed) + 1;
            presses.held.insert(key, id);
            Some(id)
        } else {
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            None
        };
        let _ = self.inner.presses.push(presses);
        peer
    }
    #[cfg(all(test, debug_assertions))]
    pub(crate) fn captured_metadata(
        &self,
        ev: &CapturedEvent,
        modifiers: Option<u16>,
        metadata: oink_input::diagnostics::CaptureMetadata,
    ) {
        self.captured_outcome(ev, modifiers, metadata, None, None);
    }
    pub(super) fn captured_outcome(
        &self,
        ev: &CapturedEvent,
        modifiers: Option<u16>,
        metadata: oink_input::diagnostics::CaptureMetadata,
        outcome: Option<oink_foundation::input::Disposition>,
        path: Option<oink_routing::router::ContactsPath>,
    ) {
        if !self.enabled() {
            return;
        }
        let mut row = Event::captured(ev);
        if let Event::Input {
            event,
            modifiers: out,
            disposition,
            ..
        } = &mut row
        {
            *out = modifiers;
            *disposition = outcome.map(|d| match d {
                oink_foundation::input::Disposition::Pass => super::privacy::CaptureDisposition::Pass,
                oink_foundation::input::Disposition::Swallow => super::privacy::CaptureDisposition::Swallow,
            });
            if let InputEvent::Key { code, .. } = ev.event {
                if let SafeInput::Key { diagnostic, repeat, .. } = event {
                    diagnostic.raw = metadata.raw;
                    diagnostic.backend_generated = metadata.backend_generated;
                    diagnostic.pressed_while_held = metadata.pressed_while_held;
                    *repeat = metadata.repeat;
                }
                // Physical and backend-generated releases belong to one capture ledger.
                let _ = self.annotate_key(event, Some(code), Origin::Physical, None);
            }
        }
        self.record(row);
        if let InputEvent::Gesture(gesture) = ev.event {
            self.captured_gesture_path(gesture, outcome, path);
        }
    }
    pub(crate) fn injected(&self, ev: &InjectEvent, peer: Option<DeviceId>, success: bool) {
        if !self.enabled() {
            return;
        }
        let mut event = SafeInput::injection(ev);
        let code = match ev {
            InjectEvent::Key { code, .. } | InjectEvent::Volume { code, .. } => Some(*code),
            _ => None,
        };
        let peer = self.annotate_key(&mut event, code, Origin::Injection, peer);
        self.record(Event::Input {
            origin: Origin::Injection,
            peer: peer.map(|p| self.anonymizer().device(p)),
            original_us: None,
            cursor: ev.move_target(),
            event,
            success: Some(success),
            modifiers: None,
            disposition: None,
        });
    }
    pub(super) fn keyboard_snapshot(&self) {
        for (kind, value) in [StateKind::CapsLock, StateKind::SecureInput]
            .into_iter()
            .zip(oink_input::diagnostics::keyboard_state())
        {
            if let Some(active) = value {
                self.record(Event::State {
                    kind,
                    active,
                    count: 0,
                    value: 0.0,
                });
            }
        }
    }
}

// Scope only the synchronous message dispatch. Carry it alongside queued local
// actions; no protocol or routing event is changed. Forwarded origin is authoritative.
#[cfg(all(test, debug_assertions))]
thread_local! { static SOURCE: std::cell::Cell<Option<DeviceId>> = const { std::cell::Cell::new(None) }; }
#[cfg(all(test, debug_assertions))]
pub(crate) fn source() -> Option<DeviceId> {
    SOURCE.get()
}
pub(crate) fn message_source(from: DeviceId, msg: &oink_routing::Packet) -> Option<DeviceId> {
    let mut from = from;
    if let oink_routing::Packet::Input(input) | oink_routing::Packet::TimedInput { input, .. } = msg {
        let mut input = input;
        let mut depth = 0;
        while let oink_routing::input::InputMsg::Forwarded { origin, msg, .. } = input {
            if depth >= 8 {
                return None;
            }
            depth += 1;
            from = origin.device_id;
            input = msg;
        }
        Some(from)
    } else {
        None
    }
}
#[cfg(all(test, debug_assertions))]
pub(crate) fn with_source<T>(from: Option<DeviceId>, f: impl FnOnce() -> T) -> T {
    struct Reset(Option<DeviceId>);
    impl Drop for Reset {
        fn drop(&mut self) {
            SOURCE.set(self.0);
        }
    }
    let _reset = Reset(SOURCE.replace(from));
    f()
}
