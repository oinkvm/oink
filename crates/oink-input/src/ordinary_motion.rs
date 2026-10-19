//! macOS ordinary-motion refinement. The warp ledger always processes the
//! same stream; only a proven ordinary frame may replace its routed output.
use crate::warp_delta::WarpDeltaFix;
use oink_foundation::Point;
use serde::{Deserialize, Serialize};

pub const POINTER_TYPE: u32 = 17;
pub const ACCELERATED: u32 = 0x0001_0000;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HidNode {
    pub kind: u32,
    pub flags: u32,
    pub timestamp: u64,
    pub sender: u64,
    pub delta: (f64, f64),
    pub buttons: Option<u32>,
}

/// Stable numeric diagnostic codes. Append only; never encode a missing source
/// as a zero motion. Codes describe evidence, not inferred system preferences.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u32)]
pub enum Reason {
    Accelerated = 0,
    ApiUnavailable = 1,
    NoAttachedEvent = 2,
    NoAcceleratedChild = 3,
    ZeroRawWithoutChild = 4,
    AcceleratedRoot = 5,
    AmbiguousChildren = 6,
    ProvenanceMismatch = 7,
    Nonfinite = 8,
    TreeLimit = 9,
    NotPhysicalHid = 10,
    NotOrdinary = 11,
    ButtonsHeld = 12,
    ButtonsUnknown = 13,
}
impl Reason {
    pub const ALL: [Self; 14] = [
        Self::Accelerated,
        Self::ApiUnavailable,
        Self::NoAttachedEvent,
        Self::NoAcceleratedChild,
        Self::ZeroRawWithoutChild,
        Self::AcceleratedRoot,
        Self::AmbiguousChildren,
        Self::ProvenanceMismatch,
        Self::Nonfinite,
        Self::TreeLimit,
        Self::NotPhysicalHid,
        Self::NotOrdinary,
        Self::ButtonsHeld,
        Self::ButtonsUnknown,
    ];
}

pub fn select(root: HidNode, descendants: impl IntoIterator<Item = HidNode>) -> Result<(f64, f64), Reason> {
    if root.kind != POINTER_TYPE || root.sender == 0 || root.timestamp == 0 {
        return Err(Reason::ProvenanceMismatch);
    }
    if root.flags & ACCELERATED != 0 {
        return Err(Reason::AcceleratedRoot);
    }
    if !root.delta.0.is_finite() || !root.delta.1.is_finite() {
        return Err(Reason::Nonfinite);
    }
    let mut children = descendants
        .into_iter()
        .filter(|n| n.kind == POINTER_TYPE && n.flags & ACCELERATED != 0);
    let Some(child) = children.next() else {
        return Err(if root.delta == (0., 0.) {
            Reason::ZeroRawWithoutChild
        } else {
            Reason::NoAcceleratedChild
        });
    };
    if children.next().is_some() {
        return Err(Reason::AmbiguousChildren);
    }
    if child.sender != root.sender || child.timestamp != root.timestamp {
        return Err(Reason::ProvenanceMismatch);
    }
    match (root.buttons, child.buttons) {
        (Some(0), Some(0)) => {}
        (Some(_), Some(_)) => return Err(Reason::ButtonsHeld),
        _ => return Err(Reason::ButtonsUnknown),
    }
    if !child.delta.0.is_finite() || !child.delta.1.is_finite() {
        return Err(Reason::Nonfinite);
    }
    Ok(child.delta)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Buttons {
    Released,
    Held,
    Unknown,
}
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub physical: bool,
    pub ordinary: bool,
    pub buttons: Buttons,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decision {
    pub ledger_delta: (f64, f64),
    pub routed: (f64, f64),
    pub reason: Reason,
}

#[allow(clippy::too_many_arguments)]
pub fn motion(
    fix: &mut WarpDeltaFix,
    raw: (f64, f64),
    at: Point,
    ts: u64,
    hid: bool,
    frame: Frame,
    read: impl FnOnce() -> Result<(f64, f64), Reason>,
) -> Decision {
    let ledger_delta = fix.motion(raw, at, ts, hid);
    let result = if !frame.physical || !hid {
        Err(Reason::NotPhysicalHid)
    } else if !frame.ordinary {
        Err(Reason::NotOrdinary)
    } else {
        match frame.buttons {
            Buttons::Held => Err(Reason::ButtonsHeld),
            Buttons::Unknown => Err(Reason::ButtonsUnknown),
            Buttons::Released => read(),
        }
    };
    match result {
        Ok(routed) => Decision {
            ledger_delta,
            routed,
            reason: Reason::Accelerated,
        },
        Err(reason) => Decision {
            ledger_delta,
            routed: ledger_delta,
            reason,
        },
    }
}

/// A separate eligibility guard. Never changes the existing physical-button
/// ledger or injected-button release behavior. A gap makes evidence unknown.
#[derive(Debug, Default)]
pub struct ButtonGate {
    mask: u32,
    start: u64,
    end: u64,
    known: bool,
}
impl ButtonGate {
    /// Sample mixed HID state outside the tap after installing capture. Mixed
    /// injected state is conservative: it can only keep this on the warp-accounting path.
    /// Replay queued transitions from the start of the sampling interval, but
    /// never override queued motion until past its end.
    pub fn seed(&mut self, mask: u32, start: u64, end: u64) {
        *self = Self {
            mask,
            start,
            end,
            known: start != 0 && end >= start,
        };
    }
    pub fn invalidate(&mut self) {
        self.known = false;
    }
    pub fn transition(&mut self, button: u32, pressed: bool, ts: u64) {
        if ts == 0 {
            self.invalidate();
            return;
        }
        if ts < self.start {
            return;
        }
        let Some(bit) = 1u32.checked_shl(button) else {
            self.invalidate();
            return;
        };
        if pressed {
            self.mask |= bit;
        } else {
            self.mask &= !bit;
        }
    }
    pub fn state(&self, ts: u64) -> Buttons {
        if !self.known || ts <= self.end {
            Buttons::Unknown
        } else if self.mask != 0 {
            Buttons::Held
        } else {
            Buttons::Released
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests;
