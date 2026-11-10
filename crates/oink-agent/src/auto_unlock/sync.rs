//! Sync unlock ("同步解锁其它设备"). The initiator sends one hint per user unlock
//! edge; the target never trusts it and challenges the source on the same
//! logical session (direct or relayed) before running one local transaction.
use oink_foundation::{DeviceId, session::Boot};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::SyncSender,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// The source must answer the target's challenge within this window.
pub(crate) const SOURCE_REPLY: Duration = Duration::from_secs(2);
/// An unlock edge this soon after Oink's own Return/provider success is Oink's.
pub(crate) const OWN_UNLOCK: Duration = Duration::from_secs(5);
/// Local physical typing this recent makes a target Mac skip (user decision Q9).
#[cfg(target_os = "macos")]
pub(crate) const LOCAL_TYPING: Duration = Duration::from_secs(2);
const LAST_FILE: &str = "auto-unlock-sync-last.json";

/// Why the latest sync request was skipped. A skip is not a failure: it needs no
/// user action, is shown as secondary text and never as an alert.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncSkip {
    /// Windows: LogonUI did not serialize within the sync window (curtain up).
    LockScreen,
    DisplayWake,
    /// macOS: the loginwindow password field did not become ready.
    PasswordField,
    SourceLocked,
    LocalTyping,
    SourceDisconnected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncLast {
    pub kind: SyncSkip,
    /// Wall-clock milliseconds, for display only; never used for decisions.
    pub at_ms: u64,
}

pub(crate) fn read_last(dir: &Path) -> Option<SyncLast> {
    std::fs::read(dir.join(LAST_FILE))
        .ok()
        .and_then(|v| serde_json::from_slice(&v).ok())
}
pub(crate) fn persist_skip(dir: &Path, kind: SyncSkip) {
    let at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64);
    if let Ok(bytes) = serde_json::to_vec(&SyncLast { kind, at_ms }) {
        let _ = oink_foundation::storage::atomic_write(&dir.join(LAST_FILE), &bytes);
    }
}
pub(crate) fn clear_last(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(LAST_FILE));
}

/// User unlock edge: a confirmed lock followed by a confirmed unlock. Unknown
/// readings neither form nor break an edge, and the first definite reading
/// after start (including the first login after boot) is only a baseline.
#[derive(Default)]
pub(crate) struct UnlockEdge {
    last: Option<bool>,
}
impl UnlockEdge {
    pub(crate) fn observe(&mut self, locked: Option<bool>) -> bool {
        let Some(locked) = locked else {
            return false;
        };
        let edge = self.last == Some(true) && !locked;
        self.last = Some(locked);
        edge
    }
}

// Windows correlates through the service's provider success time instead.
#[cfg(not(windows))]
static OWN_UNLOCK_AT: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
/// Called when Oink's own local password transaction posts Return.
#[cfg(target_os = "macos")]
pub(crate) fn mark_own_unlock() {
    *OWN_UNLOCK_AT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
}
#[cfg(not(windows))]
pub(crate) fn own_unlock_recent(now: Instant) -> bool {
    recent(*OWN_UNLOCK_AT.lock().unwrap_or_else(|e| e.into_inner()), now)
}
#[cfg(any(not(windows), test))]
fn recent(at: Option<Instant>, now: Instant) -> bool {
    at.is_some_and(|at| now.saturating_duration_since(at) <= OWN_UNLOCK)
}

/// Per-process edge epochs start at a random point; a restarted source has a
/// new boot, so targets never compare epochs across boots.
pub(crate) fn next_edge_epoch() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if NEXT.load(Ordering::Acquire) == 0 {
        let _ = NEXT.compare_exchange(0, random_start(1), Ordering::AcqRel, Ordering::Acquire);
    }
    NEXT.fetch_add(1, Ordering::AcqRel)
}
fn random_start(high: u64) -> u64 {
    let mut bytes = [0; 4];
    let _ = getrandom::fill(&mut bytes);
    (high << 62) | (u64::from(u32::from_le_bytes(bytes)) << 16) | 1
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceAnswer {
    Unlocked,
    Locked,
    Disconnected,
    /// Local pause/update or a newer transaction; never recorded.
    Cancelled,
}
impl SourceAnswer {
    pub(crate) const fn diagnostic_code(self) -> u64 {
        match self {
            Self::Unlocked => 0,
            Self::Locked => 1,
            Self::Disconnected => 2,
            Self::Cancelled => 3,
        }
    }
}

struct Pending {
    source: DeviceId,
    query: u64,
    at: Instant,
    reply: SyncSender<SourceAnswer>,
}

/// Target-side gate, owned by the input thread next to EntryGate. Requests are
/// admitted per (source, boot) epoch; the challenge is bound to one source and
/// one query epoch. Query epochs live in their own high range so an UnlockReply
/// for a pointer-entry challenge (focus epoch) can never satisfy it.
pub(crate) struct SyncGate {
    seen: HashMap<DeviceId, (Boot, u64)>,
    next_query: u64,
    pending: Option<Pending>,
}
impl Default for SyncGate {
    fn default() -> Self {
        Self {
            seen: HashMap::new(),
            next_query: random_start(2),
            pending: None,
        }
    }
}
impl SyncGate {
    /// Replay fence: only an epoch newer than the last one from this boot.
    pub(crate) fn admit(&mut self, source: DeviceId, boot: Boot, epoch: u64) -> bool {
        match self.seen.get(&source) {
            Some((seen_boot, seen)) if *seen_boot == boot && epoch <= *seen => false,
            _ => {
                self.seen.insert(source, (boot, epoch));
                true
            }
        }
    }
    pub(crate) fn begin(&mut self, source: DeviceId, reply: SyncSender<SourceAnswer>, now: Instant) -> u64 {
        self.cancel();
        let query = self.next_query;
        self.next_query = self.next_query.wrapping_add(1);
        self.pending = Some(Pending {
            source,
            query,
            at: now,
            reply,
        });
        query
    }
    /// Consumes a matching reply. Returns false when the reply belongs to
    /// another challenge (for example pointer entry), leaving it to EntryGate.
    pub(crate) fn confirm(&mut self, source: DeviceId, query: u64, unlocked: bool, locked: bool, now: Instant) -> bool {
        if !self
            .pending
            .as_ref()
            .is_some_and(|p| p.source == source && p.query == query)
        {
            return false;
        }
        let pending = self.pending.take().expect("checked");
        let answer = if !locked {
            SourceAnswer::Cancelled
        } else if unlocked && now.saturating_duration_since(pending.at) <= SOURCE_REPLY {
            SourceAnswer::Unlocked
        } else {
            SourceAnswer::Locked
        };
        let _ = pending.reply.try_send(answer);
        true
    }
    /// The carrying session ended (path failure, restart, removal). A relay to
    /// direct migration keeps the session and does not reach this.
    pub(crate) fn disconnected(&mut self, source: DeviceId) {
        if self.pending.as_ref().is_some_and(|p| p.source == source) {
            let pending = self.pending.take().expect("checked");
            let _ = pending.reply.try_send(SourceAnswer::Disconnected);
        }
    }
    pub(crate) fn cancel(&mut self) {
        if let Some(pending) = self.pending.take() {
            let _ = pending.reply.try_send(SourceAnswer::Cancelled);
        }
    }
}

/// Recorder-only numeric stages (auto_unlock_event=5): 1 local unlock edge
/// (outcome 0 user, 1 own, 2 setting off, 3 not eligible), 2 sent (peer count),
/// 3 received, 4 precheck code, 5 wake, 6 source answer, 7 readiness, 8 attempt.
/// No device identity, password or input is recorded.
pub(crate) fn record_diagnostic(stage: u64, epoch: u64, outcome: u64) {
    tracing::info!(
        target: "oink_auto_unlock_diagnostics",
        auto_unlock_event = 5u64,
        auto_unlock_flow_stage = stage,
        auto_unlock_flow_epoch = epoch,
        auto_unlock_flow_outcome = outcome,
        "sync unlock stage"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: DeviceId = DeviceId([2; 32]);
    const B: DeviceId = DeviceId([3; 32]);
    fn boot(device: DeviceId, nonce: u8) -> Boot {
        Boot {
            device_id: device,
            nonce: [nonce; 16],
        }
    }

    #[test]
    fn edge_needs_confirmed_lock_then_unlock_and_ignores_unknown() {
        let mut edge = UnlockEdge::default();
        // Startup / first login: the first definite reading is a baseline.
        assert!(!edge.observe(Some(false)));
        assert!(!edge.observe(None));
        assert!(!edge.observe(Some(true)));
        assert!(!edge.observe(None));
        assert!(edge.observe(Some(false)));
        assert!(!edge.observe(Some(false)));
        let mut started_locked = UnlockEdge::default();
        assert!(!started_locked.observe(None));
        assert!(!started_locked.observe(Some(true)));
        assert!(started_locked.observe(Some(false)));
        // Unlock, lock, unlock again: one edge each time.
        assert!(!started_locked.observe(Some(true)));
        assert!(started_locked.observe(Some(false)));
    }

    #[test]
    fn own_unlock_window_is_five_seconds() {
        let at = Instant::now();
        assert!(!recent(None, at));
        assert!(recent(Some(at), at + OWN_UNLOCK));
        assert!(!recent(Some(at), at + OWN_UNLOCK + Duration::from_millis(1)));
    }

    #[test]
    fn epochs_are_fenced_per_source_boot() {
        let mut gate = SyncGate::default();
        assert!(gate.admit(A, boot(A, 1), 10));
        assert!(!gate.admit(A, boot(A, 1), 10), "duplicate");
        assert!(!gate.admit(A, boot(A, 1), 9), "older");
        assert!(gate.admit(A, boot(A, 1), 11));
        assert!(gate.admit(B, boot(B, 1), 5), "other source is independent");
        // A restarted source has a new boot; its fresh random start may be lower.
        assert!(gate.admit(A, boot(A, 2), 3));
        assert!(!gate.admit(A, boot(A, 2), 3));
    }

    #[test]
    fn challenge_is_bound_to_source_query_and_deadline() {
        let now = Instant::now();
        let mut gate = SyncGate::default();
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        let query = gate.begin(A, reply, now);
        assert!(query >> 62 == 2, "query epochs never collide with focus epochs");
        assert!(!gate.confirm(B, query, true, true, now), "other source");
        assert!(!gate.confirm(A, query + 1, true, true, now), "other query");
        assert!(answer.try_recv().is_err());
        assert!(gate.confirm(A, query, true, true, now + SOURCE_REPLY));
        assert_eq!(answer.try_recv(), Ok(SourceAnswer::Unlocked));
        assert!(!gate.confirm(A, query, true, true, now), "one shot");

        for (unlocked, locked, at, expected) in [
            (false, true, now, SourceAnswer::Locked),
            (
                true,
                true,
                now + SOURCE_REPLY + Duration::from_millis(1),
                SourceAnswer::Locked,
            ),
            (true, false, now, SourceAnswer::Cancelled),
        ] {
            let (reply, answer) = std::sync::mpsc::sync_channel(1);
            let query = gate.begin(A, reply, now);
            assert!(gate.confirm(A, query, unlocked, locked, at));
            assert_eq!(answer.try_recv(), Ok(expected));
        }
    }

    #[test]
    fn session_end_pause_and_newer_transaction_resolve_pending() {
        let now = Instant::now();
        let mut gate = SyncGate::default();
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        let query = gate.begin(A, reply, now);
        gate.disconnected(B);
        assert!(answer.try_recv().is_err(), "unrelated peer");
        gate.disconnected(A);
        assert_eq!(answer.try_recv(), Ok(SourceAnswer::Disconnected));
        assert!(!gate.confirm(A, query, true, true, now));

        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        gate.begin(A, reply, now);
        gate.cancel();
        assert_eq!(answer.try_recv(), Ok(SourceAnswer::Cancelled));

        let (reply, first) = std::sync::mpsc::sync_channel(1);
        gate.begin(A, reply, now);
        let (reply, _second) = std::sync::mpsc::sync_channel(1);
        gate.begin(B, reply, now);
        assert_eq!(first.try_recv(), Ok(SourceAnswer::Cancelled));
    }

    #[test]
    fn skip_record_keeps_only_the_latest_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_last(dir.path()), None);
        persist_skip(dir.path(), SyncSkip::LockScreen);
        persist_skip(dir.path(), SyncSkip::SourceLocked);
        assert_eq!(read_last(dir.path()).map(|l| l.kind), Some(SyncSkip::SourceLocked));
        clear_last(dir.path());
        assert_eq!(read_last(dir.path()), None);
        assert_eq!(serde_json::to_string(&SyncSkip::LockScreen).unwrap(), "\"lock_screen\"");
    }
}
