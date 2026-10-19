//! Game state belongs to the game owner. Network conversion is explicit in the agent.
//! Positions and radii use the shared reference-ratio plane.
use oink_foundation::{DeviceId, Point, session::Boot};

pub const MAX_AIM_TARGETS: usize = 8;
pub const MAX_AIM_DISPLAY_KEY: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GameInstance {
    pub authority_boot: Boot,
    pub game_counter: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GameEvent {
    pub game_instance: GameInstance,
    pub sender_boot: Boot,
    pub event_seq: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AimMsg {
    Open {
        event: GameEvent,
    },
    Joined {
        event: GameEvent,
    },
    Declined {
        event: GameEvent,
    },
    State(AimState),
    Click {
        event: GameEvent,
        display: String,
        pos: Point,
    },
    Close {
        event: GameEvent,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AimPhase {
    Countdown,
    Running,
    Finished,
}
#[derive(Clone, Debug, PartialEq)]
pub struct AimState {
    pub game: GameInstance,
    pub seq: u64,
    pub phase: AimPhase,
    pub remaining_ms: u32,
    pub score: u32,
    pub clicks: u32,
    pub radius: f32,
    pub native_input: bool,
    pub targets: Vec<AimTarget>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct AimTarget {
    pub id: u32,
    pub device: DeviceId,
    pub display: String,
    pub pos: Point,
}
fn finite(p: Point) -> bool {
    p.x.is_finite() && p.y.is_finite() && p.x.abs() < 1e7 / 686.0 && p.y.abs() < 1e7 / 686.0
}
impl AimState {
    pub fn valid(&self) -> bool {
        self.radius.is_finite()
            && (0.5 / 686.0..=200.0 / 686.0).contains(&self.radius)
            && self.remaining_ms <= 3_600_000
            && self.targets.len() <= MAX_AIM_TARGETS
            && self
                .targets
                .iter()
                .all(|t| finite(t.pos) && t.display.len() <= MAX_AIM_DISPLAY_KEY)
    }
}
impl AimMsg {
    pub fn valid(&self) -> bool {
        match self {
            Self::State(s) => s.valid(),
            Self::Click { display, pos, .. } => finite(*pos) && display.len() <= MAX_AIM_DISPLAY_KEY,
            _ => true,
        }
    }
    pub fn game(&self) -> GameInstance {
        match self {
            Self::Open { event }
            | Self::Joined { event }
            | Self::Declined { event }
            | Self::Click { event, .. }
            | Self::Close { event } => event.game_instance,
            Self::State(s) => s.game,
        }
    }
}
