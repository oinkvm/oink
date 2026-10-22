//! Bounded ordinary-thread admission. State updates share one reserved queue marker;
//! replacing a pending value never queues another marker. Only command consumption
//! resolves that marker: capture callbacks and stream reads do no extra work.
use super::{InputCommand, drag::Queued};
use arc_swap::ArcSwapOption;
use crossbeam_queue::ArrayQueue;
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

const ACTION_CAPACITY: usize = 1024;
const LOCAL_STATE_CAPACITY: usize = 22;
const STATE_CAPACITY: usize = LOCAL_STATE_CAPACITY + 2 * oink_protocol::v1::limits::MEMBERS;

type StateKey = (u8, Option<oink_foundation::DeviceId>);

fn state_key(command: &InputCommand) -> Option<StateKey> {
    use InputCommand::*;
    let kind = match command {
        ProjectionGesture(_) => 21,
        Paused(_) => 0,
        Overlay(_) => 1,
        Controllable(_) => 2,
        FocusLock(_) => 3,
        GameMode(_) => 4,
        KeyboardDisabled(_) => 18,
        PeerAdmission(admission) => return Some((17, Some(admission.boot.device_id))),
        RetirePeer(boot) => return Some((17, Some(boot.device_id))),
        SessionLocked(_) => 19,
        TrailPinned(_) => 5,
        LocalFeel(_) => 6,
        LocalDisplays(_) | LocalDisplayFailure => 7,
        EdgeResistance(_) => 9,
        PointerSample { .. } => 10,
        VolumeExit(..) => 11,
        Hotkeys(_) => 12,
        EventFrequency(_) => 13,
        DndAvailable(_) => 14,
        // Shutdown must remain deliverable even when action admission is exhausted.
        Stop => 15,
        PeerFacts { boot, .. } => return Some((16, Some(boot.device_id))),
        ReceiveEmergency { .. }
        | TriggerEmergency { .. }
        | ApplyOwnerPreferences { .. }
        | ApplyOwnerPause { .. }
        | ReleaseOwnedFocusLock { .. }
        | PrepareUpdate { .. }
        | CommitUpdate { .. }
        | AbortUpdate { .. }
        | LockFocusTo { .. }
        | ReturnPointerHome
        | IdentifyScreens(_)
        | DiagnosticsSnapshot
        | Outbound { .. }
        | OutboundGone { .. }
        | Inbound { .. }
        | InboundGone { .. }
        | DndEvent(_)
        | DndItems { .. }
        | DndRefused { .. }
        | PeerMessage { .. }
        | DndDeliveryReceipt { .. }
        | DndContentDemand { .. }
        | SendMessage { .. } => return None,
    };
    Some((kind, None))
}

pub(super) struct StateCommand(InputCommand);
impl Clone for StateCommand {
    fn clone(&self) -> Self {
        use InputCommand::*;
        // Stream handles and drag actions are never put in the replacement batch.
        let command = match &self.0 {
            ProjectionGesture(v) => ProjectionGesture(*v),
            Paused(v) => Paused(*v),
            Overlay(v) => Overlay(*v),
            Controllable(v) => Controllable(*v),
            FocusLock(v) => FocusLock(*v),
            GameMode(v) => GameMode(*v),
            KeyboardDisabled(v) => KeyboardDisabled(*v),
            PeerAdmission(v) => PeerAdmission(v.clone()),
            RetirePeer(v) => RetirePeer(*v),
            SessionLocked(v) => SessionLocked(*v),
            TrailPinned(v) => TrailPinned(*v),
            LocalFeel(v) => LocalFeel(v.clone()),
            LocalDisplays(v) => LocalDisplays(v.clone()),
            LocalDisplayFailure => LocalDisplayFailure,
            EdgeResistance(v) => EdgeResistance(v.clone()),
            PointerSample {
                scope,
                application,
                sample,
                hidden,
            } => PointerSample {
                scope: *scope,
                application: *application,
                sample: *sample,
                hidden: *hidden,
            },
            VolumeExit(v, keep) => VolumeExit(v.clone(), keep.clone()),
            Hotkeys(v) => Hotkeys(v.clone()),
            EventFrequency(v) => EventFrequency(*v),
            DndAvailable(v) => DndAvailable(*v),
            Stop => Stop,
            PeerFacts {
                boot,
                os,
                displays,
                transform,
            } => PeerFacts {
                boot: *boot,
                os: *os,
                displays: displays.clone(),
                transform: transform.clone(),
            },
            _ => unreachable!("only state commands enter the replacement batch"),
        };
        Self(command)
    }
}
impl StateCommand {
    pub(super) fn queued(self) -> Queued {
        Queued {
            command: self.0,
            declaration: Ok(None),
        }
    }
}

#[allow(clippy::large_enum_variant)] // Keep existing action payloads inline in the fixed-capacity queue.
pub(super) enum Batch {
    Action(Queued),
    State,
}

pub(super) struct Commands {
    queue: ArrayQueue<Batch>,
    actions: AtomicUsize,
    state: ArcSwapOption<Vec<StateCommand>>,
    rejected: AtomicU64,
}
impl Commands {
    pub(super) fn new() -> Self {
        Self {
            queue: ArrayQueue::new(ACTION_CAPACITY + 1),
            actions: AtomicUsize::new(0),
            state: ArcSwapOption::empty(),
            rejected: AtomicU64::new(0),
        }
    }

    pub(super) fn push(&self, command: InputCommand) {
        if let Some(key) = state_key(&command) {
            let command = StateCommand(command);
            let mut old = self.state.load_full();
            loop {
                let mut next = old.as_deref().cloned().unwrap_or_default();
                next.retain(|c| state_key(&c.0) != Some(key));
                // Bound distinct pending peer keys too, including during member churn.
                // Local settings and shutdown retain their reserved space.
                if key.1.is_some()
                    && next.iter().filter(|c| state_key(&c.0).unwrap().1.is_some()).count()
                        >= STATE_CAPACITY - LOCAL_STATE_CAPACITY
                {
                    self.rejected.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                next.push(command.clone());
                let previous = self.state.compare_and_swap(&old, Some(Arc::new(next)));
                let same = match (&old, &*previous) {
                    (None, None) => true,
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                    _ => false,
                };
                if same {
                    if old.is_none() {
                        // Action admission cannot consume this reserved position.
                        assert!(self.queue.push(Batch::State).is_ok());
                    }
                    return;
                }
                old = arc_swap::Guard::into_inner(previous);
            }
        }
        if self
            .actions
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < ACTION_CAPACITY).then_some(n + 1)
            })
            .is_err()
        {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            return;
        }
        assert!(self.queue.push(Batch::Action(Queued::prepare(command))).is_ok());
    }
    pub(super) fn pop(&self) -> Option<Batch> {
        let batch = self.queue.pop()?;
        if matches!(batch, Batch::Action(_)) {
            self.actions.fetch_sub(1, Ordering::Relaxed);
        }
        Some(batch)
    }
    pub(super) fn take_state(&self) -> Vec<StateCommand> {
        self.state.swap(None).map(Arc::unwrap_or_clone).unwrap_or_default()
    }
    #[cfg(all(test, debug_assertions))]
    pub(super) fn queue_len(&self) -> usize {
        self.queue.len()
    }
    pub(super) fn take_rejected(&self) -> u64 {
        self.rejected.swap(0, Ordering::Relaxed)
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    fn boot(device_id: oink_foundation::DeviceId) -> oink_foundation::session::Boot {
        oink_foundation::session::Boot {
            device_id,
            nonce: [1; 16],
        }
    }

    #[test]
    fn concurrent_senders_share_bounded_action_admission_and_one_state_marker() {
        let commands = Arc::new(Commands::new());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let commands = commands.clone();
                scope.spawn(move || {
                    for _ in 0..2000 {
                        commands.push(InputCommand::ReturnPointerHome);
                        commands.push(InputCommand::FocusLock(true));
                    }
                });
            }
        });
        commands.push(InputCommand::FocusLock(false));
        assert_eq!(commands.queue_len(), ACTION_CAPACITY + 1);
        assert_eq!(commands.take_rejected(), 16_000 - ACTION_CAPACITY as u64);
        let mut actions = 0;
        let mut states = 0;
        while let Some(batch) = commands.pop() {
            match batch {
                Batch::Action(_) => actions += 1,
                Batch::State => {
                    let latest = commands.take_state();
                    assert_eq!(latest.len(), 1);
                    assert!(matches!(latest[0].0, InputCommand::FocusLock(false)));
                    states += 1;
                }
            }
        }
        assert_eq!(actions, ACTION_CAPACITY);
        assert_eq!(states, 1);
    }

    #[test]
    fn peer_churn_is_bounded_without_using_local_state_or_shutdown_slots() {
        let commands = Commands::new();
        for id in 0..oink_protocol::v1::limits::MEMBERS as u8 {
            let peer = oink_foundation::DeviceId([id; 32]);
            commands.push(InputCommand::PeerFacts {
                boot: boot(peer),
                displays: vec![],
                transform: None,
                os: oink_routing::Os::Windows,
            });
            commands.push(InputCommand::RetirePeer(boot(peer)));
        }
        commands.push(InputCommand::PeerFacts {
            boot: boot(oink_foundation::DeviceId([200; 32])),
            displays: vec![],
            transform: None,
            os: oink_routing::Os::Windows,
        });
        commands.push(InputCommand::PeerFacts {
            boot: boot(oink_foundation::DeviceId([0; 32])),
            displays: vec![],
            transform: None,
            os: oink_routing::Os::MacOS,
        });
        commands.push(InputCommand::Paused(true));
        commands.push(InputCommand::Stop);
        assert_eq!(commands.take_rejected(), 1);
        assert_eq!(commands.queue_len(), 1);
        assert!(matches!(commands.pop(), Some(Batch::State)));
        let states = commands.take_state();
        assert_eq!(states.len(), STATE_CAPACITY - LOCAL_STATE_CAPACITY + 2);
        assert!(matches!(states.last().unwrap().0, InputCommand::Stop));
        assert!(states.iter().any(|c| matches!(
            c.0,
            InputCommand::PeerFacts {
                os: oink_routing::Os::MacOS,
                ..
            }
        )));
        assert!(commands.pop().is_none());
    }
}
