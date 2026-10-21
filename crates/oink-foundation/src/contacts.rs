//! Gesture-only touchpad geometry in integer reference quanta, origin at the top left.
//! Fixed arrays bound both decoding and real-time copies; unused entries are zeroed.
use serde::{Deserialize, Serialize};

/// One integer unit is this fraction of the fixed reference diagonal.
pub const REFERENCE_QUANTUM: f64 = 1.0 / 68_600.0;

pub const MAX_CONTACTS: usize = 5;
pub const MAX_CHANGES: usize = 10;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Surface {
    pub width: u32,
    pub height: u32,
}
impl Surface {
    pub fn valid(self) -> bool {
        (1_000..=100_000).contains(&self.width) && (1_000..=100_000).contains(&self.height)
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    pub id: u32,
    pub x: i32,
    pub y: i32,
}
impl Contact {
    pub fn valid(self, surface: Surface) -> bool {
        self.id != 0 && self.x >= 0 && self.y >= 0 && self.x <= surface.width as i32 && self.y <= surface.height as i32
    }
}
/// Local capture snapshot; only eligible touching contacts appear here.
/// macOS requires state 3/4 and a non-rejected finger identity (1..=5).
/// `len = 255` reports capture corruption/overflow, never a partial frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    pub device: u32,
    pub time_us: u64,
    pub surface: Surface,
    pub len: u8,
    pub contacts: [Contact; MAX_CONTACTS],
}
impl Sample {
    pub fn points(&self) -> &[Contact] {
        &self.contacts[..usize::from(self.len).min(MAX_CONTACTS)]
    }
    pub fn valid(&self) -> bool {
        self.device != 0
            && self.surface.valid()
            && usize::from(self.len) <= MAX_CONTACTS
            && self
                .points()
                .iter()
                .enumerate()
                .all(|(i, p)| p.valid(self.surface) && !self.points()[..i].iter().any(|q| q.id == p.id))
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    #[default]
    Begin,
    Move,
    End,
    Cancel,
}
impl Phase {
    pub fn ends(self) -> bool {
        matches!(self, Self::End | Self::Cancel)
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub contact: Contact,
    pub phase: Phase,
}
/// All phases travel on the reliable ordered input stream. These are changes,
/// not independently recoverable snapshots: omitted IDs remain active.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    pub device: u32,
    pub time_us: u64,
    pub phase: Phase,
    pub surface: Surface,
    pub len: u8,
    pub changes: [Change; MAX_CHANGES],
}
impl Frame {
    pub fn changes(&self) -> &[Change] {
        &self.changes[..usize::from(self.len).min(MAX_CHANGES)]
    }
    pub fn cancel(device: u32) -> Self {
        Self {
            device,
            phase: Phase::Cancel,
            ..Self::default()
        }
    }
    pub fn between(previous: Option<&Sample>, next: &Sample) -> Self {
        let mut out = Self {
            device: next.device,
            time_us: next.time_us,
            surface: next.surface,
            phase: if previous.is_none() {
                Phase::Begin
            } else if next.len == 0 {
                Phase::End
            } else {
                Phase::Move
            },
            ..Self::default()
        };
        if let Some(previous) = previous {
            for &p in previous.points() {
                if !next.points().iter().any(|q| q.id == p.id) {
                    out.push(p, Phase::End);
                }
            }
        }
        for &p in next.points() {
            let exists = previous.is_some_and(|s| s.points().iter().any(|q| q.id == p.id));
            out.push(p, if exists { Phase::Move } else { Phase::Begin });
        }
        out
    }
    fn push(&mut self, contact: Contact, phase: Phase) {
        self.changes[usize::from(self.len)] = Change { contact, phase };
        self.len += 1;
    }
}
