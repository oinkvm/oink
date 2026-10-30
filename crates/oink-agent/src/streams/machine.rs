//! State machine shared by outbound reliable streams of business modules.
//! One instance per module, peer and kind manages
//! only the stream this device writes; the stream the peer writes is managed by the
//! peer's machine. C0 and N0 do not use it.
//!
//! ```text
//! Closed  --allowed and wanted: ordinal+1, open, post the opening state-->  Open
//! Open    --post full / closed or rejected by the reader (not Unsupported)-->  Backoff
//! Open    --intentional Cancelled-->  Backoff (due now, failure count cleared)
//! Open    --module reopens (input capability changed): close, ordinal+1, open at once-->  Open
//! Backoff --due and still allowed: ordinal+1, open, post the opening state-->  Open
//! Open    --link lost / paused / rejected as Unsupported-->  Closed
//! Backoff --link lost / paused-->  Closed
//! ```
//!
//! The machine holds no handle and does no I/O. Its owner (the module's task) calls it
//! with what happened and carries out the returned [`Step`].
use oink_protocol::frame::{StreamCode, StreamKind, StreamTag};
use std::time::{Duration, Instant};

/// Waits before the n-th consecutive reopen; the first reopen is immediate, the last
/// value repeats. Each stream backs off alone and never escalates to its link.
pub const BACKOFF: [Duration; 6] = [
    Duration::ZERO,
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
];
/// A stream that fails sooner than this after opening counts as a consecutive failure;
/// one that lived this long clears the count.
pub const STABLE_AFTER: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Closed,
    Open,
    Backoff,
}

/// Why the outbound stream failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// `PostError::Full`.
    Full,
    /// `StreamClosed::Peer(code)`. `Unsupported` goes to `Closed` and is remembered;
    /// `Cancelled` is an intentional session retirement, with no failure backoff.
    /// `NotReady` and `Resend` reopen like any other failure (the first one at once).
    Peer(StreamCode),
}

/// What the owner must do now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// `LinkLayer::close_stream` for this tag (skip if the handle is already closed).
    Close(StreamTag),
    /// `LinkLayer::open_stream` with this tag, then post the opening state.
    Open(StreamTag),
    /// Both, in this order.
    Reopen {
        close: StreamTag,
        open: StreamTag,
    },
}

#[derive(Clone, Debug)]
pub struct StreamMachine {
    kind: StreamKind,
    state: State,
    ordinal: u32,
    failures: u8,
    opened_at: Option<Instant>,
    retry_at: Option<Instant>,
    unsupported: bool,
}

impl StreamMachine {
    /// For a new link: closed, ordinal 0 (the first stream gets 1), nothing remembered.
    pub fn new(kind: StreamKind) -> Self {
        Self {
            kind,
            state: State::Closed,
            ordinal: 0,
            failures: 0,
            opened_at: None,
            retry_at: None,
            unsupported: false,
        }
    }
    pub fn kind(&self) -> StreamKind {
        self.kind
    }
    pub fn state(&self) -> State {
        self.state
    }
    /// Tag of the stream that is open now.
    pub fn current(&self) -> Option<StreamTag> {
        (self.state == State::Open).then_some(StreamTag {
            kind: self.kind,
            ordinal: self.ordinal,
        })
    }
    /// The peer rejected this kind as unsupported and its capabilities have not changed since.
    pub fn unsupported(&self) -> bool {
        self.unsupported
    }
    /// Re-evaluate. `allowed`: the link is ready and, for a module that obeys pause, this
    /// device is not paused. `wanted`: the module has a use for the stream (always true
    /// for input; "first use happened" for the others). Call on every relevant change and
    /// when [`deadline`](Self::deadline) passes. Not allowed → `Close` and `Closed`.
    pub fn poll(&mut self, allowed: bool, wanted: bool, now: Instant) -> Step {
        if !allowed {
            let current = self.current();
            self.state = State::Closed;
            self.retry_at = None;
            self.opened_at = None;
            return current.map_or(Step::Nothing, Step::Close);
        }
        if self.state == State::Open {
            if self.opened_at.is_some_and(|at| now.duration_since(at) >= STABLE_AFTER) {
                self.failures = 0;
            }
            return Step::Nothing;
        }
        if self.unsupported || !wanted || self.retry_at.is_some_and(|at| now < at) {
            return Step::Nothing;
        }
        Step::Open(self.open(now))
    }
    /// The open stream failed.
    pub fn failed(&mut self, failure: Failure, now: Instant) -> Step {
        let Some(tag) = self.current() else {
            return Step::Nothing;
        };
        if failure == Failure::Peer(StreamCode::Unsupported) {
            self.unsupported = true;
            self.state = State::Closed;
            self.retry_at = None;
        } else if failure == Failure::Peer(StreamCode::Cancelled) {
            self.failures = 0;
            self.retry_at = Some(now);
            self.state = State::Backoff;
        } else {
            if self.opened_at.is_some_and(|at| now.duration_since(at) >= STABLE_AFTER) {
                self.failures = 0;
            }
            let delay = BACKOFF[usize::from(self.failures).min(BACKOFF.len() - 1)];
            self.failures = self.failures.saturating_add(1);
            self.retry_at = Some(now + delay);
            self.state = State::Backoff;
        }
        self.opened_at = None;
        Step::Close(tag)
    }
    /// The module replaces its own stream because something declared only in the opening
    /// state changed. Not a failure: no count, no wait. In `Backoff` it does nothing (the
    /// reopen that is due will carry the new state).
    pub fn reopen(&mut self, now: Instant) -> Step {
        let Some(close) = self.current() else {
            return Step::Nothing;
        };
        Step::Reopen {
            close,
            open: self.open(now),
        }
    }
    fn open(&mut self, now: Instant) -> StreamTag {
        self.ordinal += 1;
        self.state = State::Open;
        self.opened_at = Some(now);
        self.retry_at = None;
        StreamTag {
            kind: self.kind,
            ordinal: self.ordinal,
        }
    }
    /// The peer's capabilities changed: forget "unsupported".
    pub fn capabilities_changed(&mut self) {
        self.unsupported = false;
    }
    /// When to call `poll` again while backing off.
    pub fn deadline(&self) -> Option<Instant> {
        self.retry_at
    }
}

/// Receiving side of "a newer stream replaces the older one of the same kind": the
/// highest ordinal seen from one peer for one kind during one link.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InboundOrdinal(u32);

/// Decision for an arriving stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admit {
    /// Accept. If `replaces` is set, close that older stream (if its handle is still
    /// open) and clear the receive state of the kind before reading the new one.
    Accept { replaces: Option<u32> },
    /// Reject with `StreamCode::StaleOrdinal`.
    Stale,
}

impl InboundOrdinal {
    pub fn admit(&mut self, ordinal: u32) -> Admit {
        if ordinal <= self.0 {
            return Admit::Stale;
        }
        let replaces = (self.0 != 0).then_some(self.0);
        self.0 = ordinal;
        Admit::Accept { replaces }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    fn tag(kind: StreamKind, ordinal: u32) -> StreamTag {
        StreamTag { kind, ordinal }
    }

    #[test]
    fn every_reliable_module_follows_all_edges() {
        for kind in [
            StreamKind::Input,
            StreamKind::Clipboard,
            StreamKind::Audio,
            StreamKind::Aim,
            StreamKind::Diagnostics,
        ] {
            for failure in [
                Failure::Full,
                Failure::Peer(StreamCode::None),
                Failure::Peer(StreamCode::NotReady),
                Failure::Peer(StreamCode::Resend),
                Failure::Peer(StreamCode::Invalid),
                Failure::Peer(StreamCode::StaleOrdinal),
            ] {
                let now = Instant::now();
                let mut m = StreamMachine::new(kind);
                assert_eq!(m.poll(false, true, now), Step::Nothing);
                assert_eq!(m.poll(true, false, now), Step::Nothing);
                assert_eq!(m.poll(true, true, now), Step::Open(tag(kind, 1)));
                assert_eq!(m.poll(true, false, now), Step::Nothing, "idle retains stream");
                assert_eq!(m.failed(failure, now), Step::Close(tag(kind, 1)));
                assert_eq!(m.state(), State::Backoff);
                assert_eq!(m.poll(true, true, now), Step::Open(tag(kind, 2)));
                assert_eq!(
                    m.reopen(now),
                    Step::Reopen {
                        close: tag(kind, 2),
                        open: tag(kind, 3)
                    }
                );
                assert_eq!(m.failed(failure, now), Step::Close(tag(kind, 3)));
                assert_eq!(m.deadline(), Some(now + BACKOFF[1]));
                assert_eq!(m.reopen(now), Step::Nothing);
                assert_eq!(m.poll(false, true, now), Step::Nothing);
                assert_eq!(m.state(), State::Closed);
                assert_eq!(m.deadline(), None);
                assert_eq!(m.poll(true, true, now), Step::Open(tag(kind, 4)));
                assert_eq!(m.poll(false, true, now), Step::Close(tag(kind, 4)));
            }
        }
    }

    #[test]
    fn safety_cancellations_never_accumulate_failure_backoff() {
        let mut machine = StreamMachine::new(StreamKind::Input);
        let mut now = Instant::now();
        machine.poll(true, true, now);
        // Native pause and resume each retire the reader. Deliver those closes
        // before any C0 Facts, with no stable ten-second interval between them.
        for ordinal in 1..=16 {
            now += Duration::from_millis(1);
            assert_eq!(
                machine.failed(Failure::Peer(StreamCode::Cancelled), now),
                Step::Close(tag(StreamKind::Input, ordinal))
            );
            assert_eq!(machine.deadline(), Some(now), "a safety close is not a failed stream");
            assert_eq!(
                machine.poll(true, true, now),
                Step::Open(tag(StreamKind::Input, ordinal + 1))
            );
        }
        // Genuine queue/transport failures still back off. A later intentional
        // retirement starts a new session, rather than inheriting those failures.
        for delay in [BACKOFF[0], BACKOFF[1]] {
            machine.failed(Failure::Full, now);
            assert_eq!(machine.deadline(), Some(now + delay));
            now += delay;
            machine.poll(true, true, now);
        }
        machine.failed(Failure::Peer(StreamCode::Cancelled), now);
        assert_eq!(machine.deadline(), Some(now));
        machine.poll(true, true, now);
        machine.failed(Failure::Full, now);
        assert_eq!(machine.deadline(), Some(now));
    }

    #[test]
    fn repeated_failures_back_off_and_stable_stream_resets() {
        let mut m = StreamMachine::new(StreamKind::Input);
        let mut now = Instant::now();
        m.poll(true, true, now);
        for delay in BACKOFF.into_iter().chain([BACKOFF[5]; 3]) {
            m.failed(Failure::Full, now);
            assert_eq!(m.deadline(), Some(now + delay));
            if !delay.is_zero() {
                assert_eq!(m.poll(true, true, now + delay - Duration::from_nanos(1)), Step::Nothing);
            }
            now += delay;
            assert!(matches!(m.poll(true, true, now), Step::Open(_)));
        }
        now += STABLE_AFTER;
        m.failed(Failure::Full, now);
        assert_eq!(m.deadline(), Some(now));
    }

    #[test]
    fn unsupported_is_remembered_until_capabilities_change_or_new_link() {
        let now = Instant::now();
        let mut m = StreamMachine::new(StreamKind::Input);
        m.poll(true, true, now);
        m.failed(Failure::Peer(StreamCode::Unsupported), now);
        assert!(m.unsupported());
        assert_eq!(m.poll(true, true, now + STABLE_AFTER), Step::Nothing);
        m.poll(false, true, now);
        assert_eq!(m.poll(true, true, now), Step::Nothing);
        m.capabilities_changed();
        assert_eq!(m.poll(true, true, now), Step::Open(tag(StreamKind::Input, 2)));
        let mut fresh = StreamMachine::new(StreamKind::Input);
        assert_eq!(fresh.poll(true, true, now), Step::Open(tag(StreamKind::Input, 1)));
    }

    #[test]
    fn inbound_ordinals_never_revert_after_close() {
        let mut inbound = InboundOrdinal::default();
        assert_eq!(inbound.admit(0), Admit::Stale);
        assert_eq!(inbound.admit(1), Admit::Accept { replaces: None });
        assert_eq!(inbound.admit(3), Admit::Accept { replaces: Some(1) });
        assert_eq!(inbound.admit(1), Admit::Stale);
        assert_eq!(inbound.admit(3), Admit::Stale);
        assert_eq!(inbound.admit(2), Admit::Stale);
    }
}
