//! Focus, capture preferences and trail continuation domain.
//! Membership and owner facts are supplied independently by configuration.

use oink_foundation::DeviceId;
use oink_foundation::Point;
use oink_foundation::input::FeelProfile;
use serde::{Deserialize, Serialize};

/// Routing process incarnations participating when a focus handoff was created.
/// Gossip preserves this snapshot; reconnecting must never add a new incarnation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FocusScope {
    pub members: Vec<(DeviceId, [u8; 16])>,
}

/// Group lock and its unlock tombstone. Ordered by (revision, author).
/// Binding the target to a process incarnation prevents reconnect/restart resurrection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FocusLock {
    pub revision: u64,
    pub author: DeviceId,
    /// Stable creation epoch/request token; focus gossip may fence newer claims.
    pub epoch: u64,
    pub holder: DeviceId,
    pub session: [u8; 16],
    pub scope: FocusScope,
    pub locked: bool,
}

/// Owner policy used to damp entry before focus reaches this device.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryProtection {
    pub locked: bool,
    pub game: bool,
    pub passive_displays: Vec<String>,
}

/// Input effects carried by the current Input stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ControlMsg {
    ApplicationFocus(crate::application_focus::Message),
    EntryProtection {
        state: EntryProtection,
    },
    /// Keyboard-source physical escape state, sent reliably to its current focus.
    /// Focus changes release the old target and synchronize the new one.
    LockEscape {
        held: bool,
    },
    MovePointer {
        epoch: u64,
        target: DeviceId,
    },
    Feel {
        profile: FeelProfile,
    },
    FocusEnter {
        epoch: u64,
        display: String,
        pos: Point,
        scope: Option<FocusScope>,
        pointer_source: Option<DeviceId>,
    },
    FocusState {
        epoch: u64,
        holder: DeviceId,
        scope: Option<FocusScope>,
        session: Option<[u8; 16]>,
        lock_state: Option<FocusLock>,
    },
    /// A request from any physical input member; None means toggle.
    SetFocusLock {
        epoch: u64,
        locked: Option<bool>,
        target: Option<DeviceId>,
    },
    FocusLock {
        state: FocusLock,
    },
    IdentifyScreens {
        id: u64,
    },
    GameMode {
        epoch: u64,
        generation: u64,
        mode: GameMode,
        detected: bool,
        active: bool,
        cursor_hiding_cause_available: Option<bool>,
    },
    SetGameMode {
        epoch: u64,
        mode: GameMode,
    },
    FocusResume {
        epoch: u64,
        holder: DeviceId,
        scope: FocusScope,
    },
    CursorTrail {
        active: bool,
    },
    UserActivity,
    CursorTrailHandoff {
        epoch: u64,
        sources: Vec<DeviceId>,
        state: TrailContinuation,
    },
    /// First message of every input stream (I0). Whether this device can be
    /// controlled (accessibility permission and the like). Forwarding input to a device
    /// and handing focus to it follow this message, not the device facts. It is declared
    /// only here: when it changes, the writer reopens its input stream.
    InputCapability {
        controllable: bool,
        /// Gesture-only physical contacts supported by the live injector.
        touchpad_contacts: Option<u8>,
    },
}

/// Bounded continuation. On the wire positions/direction are reference-plane offsets from
/// the exit; inside the renderer they are destination-local coordinates/direction.
/// All times are relative seconds, never machine timestamps.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrailContinuation {
    pub shown_secs: f64,
    pub size_scale: f64,
    pub native_units_per_reference: f64,
    pub emit_carry: f64,
    pub heading: Point,
    #[serde(deserialize_with = "bounded_trail_points")]
    pub points: Vec<TrailPoint>,
}

pub const MAX_TRAIL_POINTS: usize = 64;
/// Maximum 60 s local lifetime plus 2 s transit allowance; snapshots stay decimated.
pub const MAX_TRAIL_POINT_AGE_SECS: f64 = 62.;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrailPoint {
    pub age_secs: f64,
    pub pos: Point,
}

fn bounded_trail_points<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<TrailPoint>, D::Error> {
    struct Bounded;
    impl<'de> serde::de::Visitor<'de> for Bounded {
        type Value = Vec<TrailPoint>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("at most 64 trail points")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(p) = seq.next_element()? {
                if out.len() == MAX_TRAIL_POINTS {
                    return Err(serde::de::Error::custom("too many trail points"));
                }
                out.push(p);
            }
            Ok(out)
        }
    }
    d.deserialize_seq(Bounded)
}

impl TrailContinuation {
    pub fn valid(&self) -> bool {
        let finite = |p: Point| p.x.is_finite() && p.y.is_finite() && p.x.abs() < 1e7 && p.y.abs() < 1e7;
        self.size_scale.is_finite()
            && self.size_scale > 0.0
            && self.size_scale <= 100.0
            && self.native_units_per_reference.is_finite()
            && self.native_units_per_reference > 0.0
            && self.native_units_per_reference <= 10000.0 * 686.0
            && self.shown_secs.is_finite()
            && self.shown_secs >= 0.0
            && self.emit_carry.is_finite()
            && (0.0..1.0).contains(&self.emit_carry)
            && finite(self.heading)
            && self.points.len() <= MAX_TRAIL_POINTS
            && self.points.iter().all(|p| {
                finite(p.pos) && p.age_secs.is_finite() && (0.0..=MAX_TRAIL_POINT_AGE_SECS).contains(&p.age_secs)
            })
            && self.points.windows(2).all(|p| p[0].age_secs >= p[1].age_secs)
    }
    pub fn age(&mut self, secs: f64) {
        self.shown_secs += secs;
        for p in &mut self.points {
            p.age_secs += secs;
        }
    }
}

/// Manual override of the current focus device's pointer capture detection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GameMode {
    #[default]
    Auto,
    On,
    Off,
}
