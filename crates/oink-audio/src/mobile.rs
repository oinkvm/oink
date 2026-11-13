//! Device-local mobile audio endpoint policy.
//!
//! The controller owns only local intent and the idle deadline. Group routes,
//! network leases and native handles remain in the mobile core/platform
//! adapters. Keeping the deadline here makes the two directions independent
//! and gives both Android and iOS the same generation fence.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// The two mobile controls are independent. A phone may capture its
/// microphone while another device is the current speaker exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    SpeakerReceive,
    MicrophoneSend,
    SpeakerSend,
}

/// Device-local idle policy. The wire state reports the selected policy, but
/// the setting itself is never synchronized between group members.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleTimeout {
    OneMinute,
    #[default]
    FiveMinutes,
    TenMinutes,
    TwentyMinutes,
    ThirtyMinutes,
    SixtyMinutes,
    Unlimited,
}

impl IdleTimeout {
    pub const ALL: [Self; 7] = [
        Self::OneMinute,
        Self::FiveMinutes,
        Self::TenMinutes,
        Self::TwentyMinutes,
        Self::ThirtyMinutes,
        Self::SixtyMinutes,
        Self::Unlimited,
    ];

    pub const fn duration(self) -> Option<Duration> {
        match self {
            Self::OneMinute => Some(Duration::from_secs(60)),
            Self::FiveMinutes => Some(Duration::from_secs(5 * 60)),
            Self::TenMinutes => Some(Duration::from_secs(10 * 60)),
            Self::TwentyMinutes => Some(Duration::from_secs(20 * 60)),
            Self::ThirtyMinutes => Some(Duration::from_secs(30 * 60)),
            Self::SixtyMinutes => Some(Duration::from_secs(60 * 60)),
            Self::Unlimited => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    Manual,
    Remote,
    NoData,
    Pause,
    Fault,
    Interrupted,
    Permission,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoClosed {
    pub direction: Direction,
    pub generation: u64,
    pub reason: CloseReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointPhase {
    Off,
    Enabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointSnapshot {
    pub phase: EndpointPhase,
    pub generation: u64,
    /// Monotonic activity witness for v2 state projection. It changes only
    /// after validated current-generation media/lease activity.
    pub activity_seq: u64,
    pub timeout: IdleTimeout,
    pub close_reason: Option<CloseReason>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Endpoint {
    enabled: bool,
    generation: u64,
    timeout: IdleTimeout,
    enabled_at: Option<Instant>,
    last_activity: Option<Instant>,
    activity_seq: u64,
    close_reason: Option<CloseReason>,
}

impl Endpoint {
    fn snapshot(self) -> EndpointSnapshot {
        EndpointSnapshot {
            phase: if self.enabled {
                EndpointPhase::Enabled
            } else {
                EndpointPhase::Off
            },
            generation: self.generation,
            activity_seq: self.activity_seq,
            timeout: self.timeout,
            close_reason: self.close_reason,
        }
    }

    fn enable(&mut self, now: Instant) {
        if self.enabled {
            return;
        }
        self.generation = next_generation(self.generation);
        self.enabled = true;
        self.enabled_at = Some(now);
        self.last_activity = None;
        self.close_reason = None;
    }

    fn close(&mut self, reason: CloseReason) {
        self.enabled = false;
        self.enabled_at = None;
        self.last_activity = None;
        self.close_reason = Some(reason);
    }

    fn activity(&mut self, now: Instant) {
        if self.enabled {
            self.last_activity = Some(now);
            self.activity_seq = next_generation(self.activity_seq);
        }
    }

    fn deadline(&self) -> Option<Instant> {
        self.last_activity
            .or(self.enabled_at)?
            .checked_add(self.timeout.duration()?)
    }

    fn expired(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|deadline| now >= deadline)
    }
}

fn next_generation(old: u64) -> u64 {
    let next = old.wrapping_add(1);
    if next == 0 { 1 } else { next }
}

/// Shared policy used by the Rust mobile owner and native platform adapters.
#[derive(Clone, Copy, Debug, Default)]
pub struct Controller {
    speaker: Endpoint,
    microphone: Endpoint,
    forward: Endpoint,
}

impl Controller {
    fn endpoint_mut(&mut self, direction: Direction) -> &mut Endpoint {
        match direction {
            Direction::SpeakerReceive => &mut self.speaker,
            Direction::MicrophoneSend => &mut self.microphone,
            Direction::SpeakerSend => &mut self.forward,
        }
    }

    fn endpoint(&self, direction: Direction) -> &Endpoint {
        match direction {
            Direction::SpeakerReceive => &self.speaker,
            Direction::MicrophoneSend => &self.microphone,
            Direction::SpeakerSend => &self.forward,
        }
    }

    pub fn snapshot(&self, direction: Direction) -> EndpointSnapshot {
        self.endpoint(direction).snapshot()
    }

    pub fn set_timeout(&mut self, direction: Direction, timeout: IdleTimeout) {
        self.endpoint_mut(direction).timeout = timeout;
    }

    /// Enable starts the timeout immediately. If no valid packet/request has
    /// arrived, the deadline therefore starts at this exact instant.
    pub fn enable(&mut self, direction: Direction, now: Instant) -> EndpointSnapshot {
        let endpoint = self.endpoint_mut(direction);
        endpoint.enable(now);
        endpoint.snapshot()
    }

    pub fn disable(&mut self, direction: Direction, reason: CloseReason) -> Option<AutoClosed> {
        let endpoint = self.endpoint_mut(direction);
        if !endpoint.enabled {
            return None;
        }
        let event = AutoClosed {
            direction,
            generation: endpoint.generation,
            reason,
        };
        endpoint.close(reason);
        Some(event)
    }

    /// Record only validated activity from the current generation. Callers
    /// must invoke this for received audio packets or a valid mic demand/lease;
    /// endpoint observations and stale packets are deliberately not activity.
    pub fn record_activity(&mut self, direction: Direction, now: Instant) {
        self.endpoint_mut(direction).activity(now);
    }

    /// Record activity only when the native/core producer still belongs to the
    /// endpoint generation that is enabled now. A late callback from a retired
    /// stream must not keep the replacement stream alive or advance its state
    /// witness.
    pub fn record_activity_for_generation(&mut self, direction: Direction, generation: u64, now: Instant) -> bool {
        let endpoint = self.endpoint_mut(direction);
        if !endpoint.enabled || endpoint.generation != generation {
            return false;
        }
        endpoint.activity(now);
        true
    }

    /// Close only the currently enabled generation. A delayed command from a
    /// previous session cannot turn off a newly enabled endpoint.
    pub fn remote_close(&mut self, direction: Direction, expected_generation: u64) -> bool {
        let endpoint = self.endpoint_mut(direction);
        if !endpoint.enabled || endpoint.generation != expected_generation {
            return false;
        }
        endpoint.close(CloseReason::Remote);
        true
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        [
            self.speaker.deadline(),
            self.microphone.deadline(),
            self.forward.deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Poll both independent deadlines. The returned events are the only
    /// signal that should make the core withdraw its route and retire native handles.
    pub fn poll(&mut self, now: Instant) -> [Option<AutoClosed>; 3] {
        let mut result = [None, None, None];
        for (slot, direction) in [
            Direction::SpeakerReceive,
            Direction::MicrophoneSend,
            Direction::SpeakerSend,
        ]
        .into_iter()
        .enumerate()
        {
            let endpoint = self.endpoint_mut(direction);
            if endpoint.enabled && endpoint.expired(now) {
                let event = AutoClosed {
                    direction,
                    generation: endpoint.generation,
                    reason: CloseReason::NoData,
                };
                endpoint.close(CloseReason::NoData);
                result[slot] = Some(event);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_product_timeout_values_have_expected_durations() {
        let expected = [Some(60), Some(300), Some(600), Some(1200), Some(1800), Some(3600), None];
        for (timeout, seconds) in IdleTimeout::ALL.into_iter().zip(expected) {
            assert_eq!(timeout.duration().map(|duration| duration.as_secs()), seconds);
        }
    }

    #[test]
    fn no_first_activity_closes_from_enable_time() {
        let start = Instant::now();
        let mut controller = Controller::default();
        let enabled = controller.enable(Direction::SpeakerReceive, start);
        assert_eq!(enabled.generation, 1);
        assert!(controller.poll(start + Duration::from_secs(299))[0].is_none());
        let event = controller.poll(start + Duration::from_secs(300))[0].unwrap();
        assert_eq!(event.reason, CloseReason::NoData);
        assert_eq!(controller.snapshot(Direction::SpeakerReceive).phase, EndpointPhase::Off);
    }

    #[test]
    fn activity_rearms_only_the_same_direction() {
        let start = Instant::now();
        let mut controller = Controller::default();
        controller.enable(Direction::SpeakerReceive, start);
        controller.enable(Direction::MicrophoneSend, start);
        controller.record_activity(Direction::SpeakerReceive, start + Duration::from_secs(299));
        let events = controller.poll(start + Duration::from_secs(300));
        assert!(events[0].is_none());
        assert_eq!(events[1].unwrap().direction, Direction::MicrophoneSend);
    }

    #[test]
    fn activity_sequence_changes_only_for_valid_enabled_activity() {
        let start = Instant::now();
        let mut controller = Controller::default();
        assert_eq!(controller.snapshot(Direction::SpeakerReceive).activity_seq, 0);
        controller.record_activity(Direction::SpeakerReceive, start);
        assert_eq!(controller.snapshot(Direction::SpeakerReceive).activity_seq, 0);
        controller.enable(Direction::SpeakerReceive, start);
        controller.record_activity(Direction::SpeakerReceive, start + Duration::from_secs(1));
        let first = controller.snapshot(Direction::SpeakerReceive).activity_seq;
        controller.record_activity(Direction::SpeakerReceive, start + Duration::from_secs(2));
        assert_ne!(controller.snapshot(Direction::SpeakerReceive).activity_seq, first);
        assert_eq!(controller.snapshot(Direction::MicrophoneSend).activity_seq, 0);
    }

    #[test]
    fn stale_generation_activity_is_rejected() {
        let start = Instant::now();
        let mut controller = Controller::default();
        let first = controller.enable(Direction::MicrophoneSend, start).generation;
        controller.remote_close(Direction::MicrophoneSend, first);
        let second = controller
            .enable(Direction::MicrophoneSend, start + Duration::from_secs(1))
            .generation;
        assert!(!controller.record_activity_for_generation(
            Direction::MicrophoneSend,
            first,
            start + Duration::from_secs(2),
        ));
        assert_eq!(controller.snapshot(Direction::MicrophoneSend).activity_seq, 0);
        assert!(controller.record_activity_for_generation(
            Direction::MicrophoneSend,
            second,
            start + Duration::from_secs(3),
        ));
        assert_eq!(controller.snapshot(Direction::MicrophoneSend).activity_seq, 1);
    }

    #[test]
    fn unlimited_still_allows_explicit_close() {
        let start = Instant::now();
        let mut controller = Controller::default();
        controller.set_timeout(Direction::MicrophoneSend, IdleTimeout::Unlimited);
        controller.enable(Direction::MicrophoneSend, start);
        assert!(controller.poll(start + Duration::from_secs(86_400))[1].is_none());
        assert_eq!(
            controller
                .disable(Direction::MicrophoneSend, CloseReason::Manual)
                .unwrap()
                .reason,
            CloseReason::Manual
        );
    }

    #[test]
    fn stale_remote_close_cannot_disable_new_generation() {
        let start = Instant::now();
        let mut controller = Controller::default();
        let first = controller.enable(Direction::SpeakerReceive, start).generation;
        assert!(controller.remote_close(Direction::SpeakerReceive, first));
        let second = controller
            .enable(Direction::SpeakerReceive, start + Duration::from_secs(1))
            .generation;
        assert_ne!(first, second);
        assert!(!controller.remote_close(Direction::SpeakerReceive, first));
        assert_eq!(
            controller.snapshot(Direction::SpeakerReceive).phase,
            EndpointPhase::Enabled
        );
        assert!(controller.remote_close(Direction::SpeakerReceive, second));
    }
}
