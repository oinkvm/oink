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
const LOCAL_STATE_CAPACITY: usize = 24;
const STATE_CAPACITY: usize = LOCAL_STATE_CAPACITY + 2 * oink_protocol::v2::limits::MEMBERS;

type StateKey = (u8, Option<oink_foundation::DeviceId>);

fn state_key(command: &InputCommand) -> Option<StateKey> {
    use InputCommand::*;
    let kind = match command {
        DndPrepared(_) | RecordMouseShortcut { .. } => return None,
        #[cfg(debug_assertions)]
        DndWatchdogSnapshotForTest(_) => return None,
        Paused(_) => 0,
        Controllable(_) => 2,
        InjectionAvailable(_) => 23,
        FocusLock(_) => 3,
        GameMode(_) => 4,
        KeyboardDisabled(_) => 18,
        PeerAdmission(admission) => return Some((17, Some(admission.boot.device_id))),
        RetirePeer(boot) => return Some((17, Some(boot.device_id))),
        SessionLocked(_) => 19,
        PointerBase(_) => 24,
        TextFocus(_) => 25,
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
        | MovePointerTo(..)
        | ReturnPointerHome
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
        | LocalUnlock { .. }
        | SyncUnlockBroadcast { .. }
        | SyncUnlockQuery { .. }
        | UnlockReplyReady { .. }
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
            Paused(v) => Paused(*v),
            Controllable(v) => Controllable(*v),
            InjectionAvailable(v) => InjectionAvailable(*v),
            FocusLock(v) => FocusLock(*v),
            GameMode(v) => GameMode(*v),
            KeyboardDisabled(v) => KeyboardDisabled(*v),
            PeerAdmission(v) => PeerAdmission(v.clone()),
            RetirePeer(v) => RetirePeer(*v),
            SessionLocked(v) => SessionLocked(*v),
            PointerBase(v) => PointerBase(v.clone()),
            TextFocus(v) => TextFocus(*v),
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
                mobile,
                displays,
                transform,
            } => PeerFacts {
                boot: *boot,
                os: *os,
                mobile: *mobile,
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
    pub(super) fn take_rejected(&self) -> u64 {
        self.rejected.swap(0, Ordering::Relaxed)
    }
}
