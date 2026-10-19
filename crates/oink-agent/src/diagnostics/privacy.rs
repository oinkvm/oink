//! The privacy boundary. No raw key or free-form text is an event payload.
use oink_foundation::input::{
    CapturedEvent, Gesture, InjectEvent, InputEvent, KeyCode, MouseButton, ScrollPhase, ScrollUnit,
};
use oink_foundation::{DeviceId, GroupId, Point};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct Anonymizer {
    key: [u8; 32],
}
impl Anonymizer {
    pub fn new(group: GroupId) -> Self {
        Self {
            key: blake3::derive_key(oink_protocol::v1::labels::DIAGNOSTICS_GROUP, &group.0),
        }
    }
    /// Independent devices use a local namespace, never a synthetic group ID.
    pub fn for_membership(me: DeviceId, group: Option<GroupId>) -> Self {
        match group {
            Some(group) => Self::new(group),
            None => Self {
                key: blake3::derive_key(oink_protocol::v1::labels::DIAGNOSTICS_INDEPENDENT, &me.0),
            },
        }
    }
    pub fn device(&self, id: DeviceId) -> String {
        self.token(b"device", &id.0)
    }
    pub fn control_identity(&self, bytes: &[u8]) -> String {
        self.token(b"control", bytes)
    }
    pub fn router_instance(&self, id: &[u8; 16]) -> String {
        self.token(b"router_instance", id)
    }
    pub fn screen(&self, device: DeviceId, id: &str) -> String {
        let mut bytes = device.0.to_vec();
        bytes.extend_from_slice(id.as_bytes());
        self.token(b"screen", &bytes)
    }
    fn token(&self, domain: &[u8], bytes: &[u8]) -> String {
        let mut h = blake3::Hasher::new_keyed(&self.key);
        h.update(domain);
        h.update(bytes);
        h.finalize().to_hex()[..24].to_owned()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyClass {
    Ordinary,
    Function,
    LeftControl,
    LeftShift,
    LeftAlt,
    LeftMeta,
    RightControl,
    RightShift,
    RightAlt,
    RightMeta,
    Fn,
}
impl KeyClass {
    pub fn from_code(code: KeyCode) -> Self {
        if code.page() == KeyCode::PAGE_KEYBOARD {
            match code.usage() {
                0xe0 => Self::LeftControl,
                0xe1 => Self::LeftShift,
                0xe2 => Self::LeftAlt,
                0xe3 => Self::LeftMeta,
                0xe4 => Self::RightControl,
                0xe5 => Self::RightShift,
                0xe6 => Self::RightAlt,
                0xe7 => Self::RightMeta,
                0x3a..=0x45 | 0x68..=0x73 => Self::Function,
                _ => Self::Ordinary,
            }
        } else if code.page() == 0xff && code.usage() == 3 {
            Self::Fn
        } else if code.page() == KeyCode::PAGE_CONSUMER {
            Self::Function
        } else {
            Self::Ordinary
        }
    }
}
/// Only press sequence and platform-neutral observations may cross the privacy boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyDiagnostic {
    pub raw: oink_input::diagnostics::RawKeyEvent,
    pub backend_generated: Option<bool>,
    pub press_id: Option<u64>,
    pub pressed_while_held: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SafeInput {
    /// Redacted sentinel: Recorder discards it, including explicitly constructed Event values.
    ContactsOmitted,
    Motion {
        dx: f64,
        dy: f64,
        raw: bool,
    },
    Position {
        at: Point,
        delta: Option<Point>,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    Scroll {
        dx: f64,
        dy: f64,
        unit: ScrollUnit,
        phase: ScrollPhase,
    },
    Key {
        class: KeyClass,
        pressed: bool,
        repeat: Option<bool>,
        diagnostic: KeyDiagnostic,
    },
    Gesture(Gesture),
    ScrollDistance {
        dx: f64,
        dy: f64,
        phase: ScrollPhase,
    },
    RemoteMotion {
        seq: u64,
        dx: f64,
        dy: f64,
    },
    RelativeTotals {
        epoch: u64,
        generation: u64,
        seq: u64,
        x: f64,
        y: f64,
    },
}
impl SafeInput {
    pub fn capture(ev: &InputEvent) -> Self {
        match *ev {
            InputEvent::TouchpadSample(_) => Self::ContactsOmitted,
            InputEvent::PointerDelta { dx, dy } => Self::Motion { dx, dy, raw: false },
            InputEvent::RawPointerDelta { dx, dy } => Self::Motion { dx, dy, raw: true },
            InputEvent::Button { button, pressed } => Self::Button { button, pressed },
            InputEvent::Scroll { dx, dy, unit, phase } => Self::Scroll { dx, dy, unit, phase },
            InputEvent::Key { code, pressed } => Self::Key {
                class: KeyClass::from_code(code),
                pressed,
                repeat: None,
                diagnostic: KeyDiagnostic::default(),
            },
            InputEvent::Gesture(g) => Self::Gesture(g),
        }
    }
    pub fn injection(ev: &InjectEvent) -> Self {
        match *ev {
            InjectEvent::TouchpadContacts(_) => Self::ContactsOmitted,
            InjectEvent::MoveTo { pos } => Self::Position { at: pos, delta: None },
            InjectEvent::MoveToWithDelta { pos, dx, dy } => Self::Position {
                at: pos,
                delta: Some(Point { x: dx, y: dy }),
            },
            InjectEvent::MoveRelative { dx, dy } => Self::Motion { dx, dy, raw: true },
            InjectEvent::Button { button, pressed } => Self::Button { button, pressed },
            InjectEvent::Scroll { dx, dy, unit, phase } => Self::Scroll { dx, dy, unit, phase },
            InjectEvent::TouchpadScroll {
                dx,
                dy,
                pixels_per_reference,
                wheel,
            } => Self::Scroll {
                // Record the native target displacement, not the dormant wheel fallback.
                dx: dx * pixels_per_reference,
                dy: dy * pixels_per_reference,
                unit: ScrollUnit::Pixel,
                phase: wheel.phase,
            },
            InjectEvent::Key { code, pressed } => Self::Key {
                class: KeyClass::from_code(code),
                pressed,
                repeat: None,
                diagnostic: KeyDiagnostic::default(),
            },
            InjectEvent::Volume {
                code, pressed, repeat, ..
            } => Self::Key {
                class: KeyClass::from_code(code),
                pressed,
                repeat: Some(repeat),
                diagnostic: KeyDiagnostic::default(),
            },
            InjectEvent::Gesture(g) => Self::Gesture(g),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Origin {
    Physical,
    Synthetic,
    Remote,
    Injection,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum CaptureDisposition {
    Pass,
    Swallow,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    Input {
        origin: Origin,
        peer: Option<String>,
        original_us: Option<u64>,
        cursor: Option<Point>,
        event: SafeInput,
        success: Option<bool>,
        modifiers: Option<u16>,
        disposition: Option<CaptureDisposition>,
    },
    /// Values are limited to audited numeric/bool tracing fields; never message/Debug/string values.
    Log {
        module: String,
        line: u32,
        level: u8,
        fields: Vec<(String, f64)>,
        labels: Vec<(String, String)>,
    },
    Clipboard {
        kind_mask: u8,
        bytes: u64,
        outcome: u8,
    },
    Files {
        files: u64,
        files_done: u64,
        bytes: u64,
        bytes_done: u64,
        phase: u8,
    },
    Game {
        holder: String,
        epoch: u64,
        active: bool,
        detected: bool,
        mode: oink_routing::GameMode,
    },
    Focus {
        holder: String,
        epoch: u64,
        at: Option<Point>,
    },
    State {
        kind: StateKind,
        active: bool,
        count: u64,
        value: f64,
    },
    Link {
        peer: String,
        online: bool,
        rtt_us: Option<u64>,
        sent_packets: Option<u64>,
        lost_packets: Option<u64>,
    },
    KeyboardAnomaly {
        nonmodifier_flags: bool,
    },
    NativeInputLifecycle {
        at_us: u64,
        sequence: u64,
        kind: oink_input::native_lifecycle::Kind,
        data: oink_input::native_lifecycle::Data,
        holder: Option<String>,
        epoch: Option<u64>,
    },
    Handoff(super::handoff::HandoffEvent),
    Topology(Box<super::layout::LayoutChange>),
    Crossing(super::handoff::CrossingEvent),
    Trail(super::handoff::TrailEvent),
    Displays(super::handoff::DisplaysEvent),
    RouterState(super::handoff::RouterStateEvent),
    /// Source snapshot, not OS state. IDs refer only to already observed presses;
    /// no ordinary key identity is serialized. Unchanged nonempty rows are sampled.
    HeldSync {
        peer: String,
        source: String,
        sent: bool,
        keys: usize,
        buttons: usize,
        modifiers: u8,
        press_ids: Vec<u64>,
        observations: u64,
    },
    Gesture(super::gesture::GestureEvent),
    VolumeRouting(super::volume::VolumeRoutingEvent),
    VolumeRefused(super::volume::VolumeRefused),
    PointerTransition {
        sequence: u64,
        holder: Option<String>,
        epoch: Option<u64>,
        accounting: oink_input::pointer_transition::Observation,
    },
    // Event payloads contain only typed observations and pseudonymous aliases.
    FileProviderAvailability(super::ProviderAvailability),
    FileProviderDrag {
        path: super::ProviderDragPath,
        reason: oink_clipboard::FileProviderReason,
    },
    FileProviderPreparation {
        drag: String,
        epoch: u64,
        path: oink_clipboard::provider_preparation::PreparationPath,
        reason: oink_clipboard::FileProviderReason,
        timing: oink_clipboard::provider_preparation::PreparationTiming,
        items: u64,
        bytes: u64,
    },
    /// Source-aggregated input timings and jitter. Payload fields append in place;
    /// the unreleased format has one current encoding, no compatibility decoder.
    InputTiming(oink_input::timing::Sample),
    /// One factual destination summary per native hold.
    DndNativeHold {
        drag: String,
        physical: bool,
        observation: Box<oink_foundation::dnd::NativeHoldSummary>,
    },
    DndIntegrity {
        drag: String,
        record: oink_routing::DndIntegrity,
    },
    ButtonTransition {
        sequence: u64,
        holder: Option<String>,
        epoch: Option<u64>,
        observation: oink_input::button_diagnostics::Transition,
    },
    HeldButtonsRefusal(super::buttons::Refusal),
    SourceRouting(oink_routing::router::SourceDecision),
    ProtocolViolation(super::protocol_violations::Issue),
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum StateKind {
    CaptureMode,
    Injection,
    Layout,
    Permission,
    DisplayPower,
    Clipboard,
    Files,
    DragDrop,
    GameMode,
    Emergency,
    QueueLoss,
    HeldSync,
    SessionLock,
    RecorderTick,
    CapsLock,
    SecureInput,
}
impl Event {
    pub fn captured(ev: &CapturedEvent) -> Self {
        Self::Input {
            origin: if ev.physical {
                Origin::Physical
            } else {
                Origin::Synthetic
            },
            peer: None,
            original_us: Some(ev.time_us),
            cursor: ev.cursor,
            event: SafeInput::capture(&ev.event),
            success: None,
            modifiers: None,
            disposition: None,
        }
    }
}

impl std::fmt::Debug for Anonymizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Anonymizer([redacted])")
    }
}

#[cfg(all(test, debug_assertions))]
mod lifecycle_tests {
    use super::*;
    #[test]
    fn park_input_failure_and_contact_kinds_roundtrip() {
        use oink_input::native_lifecycle::{Data, Kind};
        for kind in [
            Kind::ParkInputSenderUnavailable,
            Kind::ParkInputSendRejected,
            Kind::ParkInputHookNotObserved,
            Kind::ParkInputTargetMismatch,
            Kind::ParkInputHideLost,
            Kind::ParkInputHookChainRejected,
            Kind::ParkInputReadbackMismatch,
            Kind::ParkInputCompletionUnavailable,
            Kind::ContactCapture,
            Kind::ContactInjection,
        ] {
            let event = Event::NativeInputLifecycle {
                at_us: 1234,
                sequence: 8,
                kind,
                data: Data {
                    extra: 7,
                    flags: 5,
                    point: Some([1920, 1080]),
                    success: Some(false),
                    ..Data::default()
                },
                holder: None,
                epoch: None,
            };
            let encoded = serde_json::to_vec(&event).unwrap();
            let decoded: Event = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                serde_json::to_value(&event).unwrap()
            );
            assert!(matches!(decoded, Event::NativeInputLifecycle { kind: k, data, .. }
                if k == kind && data.extra == 7 && data.flags == 5 && data.success == Some(false)));
        }
    }

    #[test]
    fn native_hold_summary_roundtrips_numeric_observations() {
        let event = Event::DndNativeHold {
            drag: "anonymized".into(),
            physical: false,
            observation: Box::new(oink_foundation::dnd::NativeHoldSummary {
                operation: 42,
                native_sequence: 81,
                end: oink_foundation::dnd::NativeHoldEnd::DraggingExited,
                updated: 7,
                returned_none: 2,
                first_none_us: Some(45),
                image_nudges_posted: 1,
                image_nudges: vec![oink_foundation::dnd::ImageNudgeObservation {
                    generation: 3,
                    reason: oink_foundation::dnd::ImageNudgeReason::InitialHide,
                    posted_us: 60,
                    post: oink_foundation::dnd::ImageNudgePost {
                        at: Point::new(0., 50.),
                        button: oink_foundation::input::MouseButton::Left,
                        flags: 256,
                    },
                    image_update_us: Some(80),
                }],
                cancel: Some(oink_foundation::dnd::CancelObservation {
                    geometry: oink_foundation::dnd::CancelGeometry {
                        down: Some(Point::new(50., 50.)),
                        target_plane: Some(Point::new(90., -10.)),
                        projected: Some(Point::new(75., -50.)),
                        edge: Some(Point::new(62.5, 0.)),
                        fallback: None,
                    },
                    posted: Some(Point::new(62.5, 0.)),
                    delivery_ack: Some(true),
                    callback_count: 2,
                    last_callback: Some(Point::new(62.5, 0.)),
                    ack_position: Some(Point::new(62.5, 0.)),
                    ack_latency_us: Some(1234),
                    wait_us: Some(1250),
                }),
                ..Default::default()
            }),
        };
        let bytes = serde_json::to_vec(&event).unwrap();
        let restored: Event = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            serde_json::to_value(&restored).unwrap(),
            serde_json::to_value(&event).unwrap()
        );
        assert!(
            matches!(restored, Event::DndNativeHold { observation, physical: false, .. }
            if observation.updated == 7 && observation.returned_none == 2 && observation.first_none_us == Some(45)
                && observation.image_nudges_posted == 1 && observation.image_nudges[0].image_update_us == Some(80))
        );
    }
    #[test]
    fn timing_roundtrips_only_numeric_observations() {
        let timing = Event::InputTiming(oink_input::timing::Sample {
            inject_queue_wait_max_us: 1_751_366,
            oldest_pending_us: 1_200_000,
            jitter: oink_input::jitter::Sample {
                reliable: oink_input::jitter::PathSample {
                    ingress: oink_input::jitter::Distribution {
                        count: 7,
                        max_us: 8000,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        });
        let bytes = serde_json::to_vec(&timing).unwrap();
        let restored: Event = serde_json::from_slice(&bytes).unwrap();
        assert!(
            matches!(restored, Event::InputTiming(v) if v.inject_queue_wait_max_us == 1_751_366 && v.jitter.reliable.ingress.max_us == 8000)
        );
    }
}
