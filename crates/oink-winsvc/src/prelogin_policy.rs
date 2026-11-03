//! Machine-level pre-login rules. Pure decisions shared by the SYSTEM service and
//! its tests; the Windows store, token checks and process control live in
//! `platform::prelogin`.
//!
//! Ownership (user decision 2026-11-03): the machine has one pre-login identity.
//! It belongs to the user who most recently turned launch at login on (an
//! administrator-approved claim). Another user's later claim replaces it and
//! discards the previous snapshot. The owner turning launch at login off disables
//! the machine level and deletes the snapshot; a non-owner turning it off has no
//! machine-level effect.
use crate::backend_launch::BackendLaunch;
use std::{
    io,
    time::{Duration, Instant},
};

/// Upper bound for one snapshot frame's file contents.
pub(crate) const MAX_SNAPSHOT_BYTES: usize = 4 << 20;

/// Fixed snapshot files. Only these names are ever written; the client never
/// supplies a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum SnapshotFile {
    DeviceKey = 1,
    GroupState = 2,
    Settings = 3,
    Addresses = 4,
    DetachedScreens = 5,
}
impl SnapshotFile {
    pub const ALL: [Self; 5] = [
        Self::DeviceKey,
        Self::GroupState,
        Self::Settings,
        Self::Addresses,
        Self::DetachedScreens,
    ];
    /// The file name in the user data directory and in the snapshot.
    pub fn name(self) -> &'static str {
        match self {
            Self::DeviceKey => "device.key",
            Self::GroupState => "group-state.json",
            Self::Settings => "settings.json",
            Self::Addresses => "addresses.json",
            Self::DetachedScreens => "detached-screens.json",
        }
    }
    /// Matches the owners' own read limits (group state envelope, records).
    pub fn max_bytes(self) -> usize {
        match self {
            Self::DeviceKey => 32,
            Self::GroupState => 512 * 1024,
            Self::Settings => 512 * 1024,
            Self::Addresses | Self::DetachedScreens => 1024 * 1024,
        }
    }
    /// The identity and the group state; the other files are optional records.
    pub fn required(self) -> bool {
        matches!(self, Self::DeviceKey | Self::GroupState)
    }
    pub(crate) fn from_id(id: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|f| *f as u8 == id)
    }
}

/// Who owns the machine identity, relative to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OwnerView {
    None = 0,
    Caller = 1,
    Other = 2,
}
impl OwnerView {
    pub(crate) fn from_id(id: u8) -> Option<Self> {
        [Self::None, Self::Caller, Self::Other]
            .into_iter()
            .find(|v| *v as u8 == id)
    }
}

pub(crate) fn validate_snapshot(files: &[(SnapshotFile, Vec<u8>)]) -> io::Result<()> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid pre-login snapshot");
    let mut seen = std::collections::BTreeSet::new();
    let mut total = 0usize;
    for (file, bytes) in files {
        if !seen.insert(*file) || bytes.len() > file.max_bytes() {
            return Err(invalid());
        }
        total += bytes.len();
    }
    if total > MAX_SNAPSHOT_BYTES
        || SnapshotFile::ALL.iter().any(|f| f.required() && !seen.contains(f))
        || files
            .iter()
            .any(|(file, bytes)| *file == SnapshotFile::DeviceKey && bytes.len() != 32)
    {
        return Err(invalid());
    }
    Ok(())
}

/// The administrator-controlled machine marker.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Machine {
    pub enabled: bool,
    /// String SID of the owning user.
    pub owner: Option<String>,
}

pub(crate) trait Store {
    fn machine(&self) -> io::Result<Machine>;
    fn set_machine(&mut self, machine: &Machine) -> io::Result<()>;
    /// The owner SID recorded inside the snapshot, if a complete snapshot exists.
    fn snapshot_owner(&self) -> io::Result<Option<String>>;
    /// Replace the snapshot atomically, recording its owner.
    fn write_snapshot(&mut self, owner: &str, files: &[(SnapshotFile, Vec<u8>)]) -> io::Result<()>;
    fn delete_snapshot(&mut self) -> io::Result<()>;
}

pub(crate) struct Caller<'a> {
    pub sid: &'a str,
    pub elevated: bool,
}

fn denied(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn state(store: &dyn Store, caller: &str) -> io::Result<BackendLaunch> {
    let machine = store.machine()?;
    let owner = match machine.owner.as_deref() {
        None => OwnerView::None,
        Some(owner) if owner.eq_ignore_ascii_case(caller) => OwnerView::Caller,
        Some(_) => OwnerView::Other,
    };
    Ok(BackendLaunch::PreloginState {
        enabled: machine.enabled && machine.owner.is_some(),
        owner,
        snapshot: ready_from(&machine, store.snapshot_owner()?.as_deref()),
    })
}

fn same(a: Option<&str>, b: &str) -> bool {
    a.is_some_and(|a| a.eq_ignore_ascii_case(b))
}

fn ready_from(machine: &Machine, snapshot_owner: Option<&str>) -> bool {
    machine.enabled
        && machine
            .owner
            .as_deref()
            .is_some_and(|owner| same(snapshot_owner, owner))
}

/// Whether the service may start the pre-login backend: enabled, owned, and the
/// snapshot belongs to that owner (an old owner's snapshot never runs).
pub(crate) fn ready(store: &dyn Store) -> io::Result<bool> {
    Ok(ready_from(&store.machine()?, store.snapshot_owner()?.as_deref()))
}

/// Apply one authenticated pre-login request from the active session's user.
pub(crate) fn handle(store: &mut dyn Store, request: &BackendLaunch, caller: &Caller<'_>) -> io::Result<BackendLaunch> {
    match request {
        BackendLaunch::PreloginStatus => {}
        BackendLaunch::PreloginClaim => {
            if !caller.elevated {
                return Err(denied("administrator approval is required"));
            }
            // Never leave a previous owner's identity behind a new owner's marker.
            if !same(store.snapshot_owner()?.as_deref(), caller.sid) {
                store.delete_snapshot()?;
            }
            store.set_machine(&Machine {
                enabled: true,
                owner: Some(caller.sid.to_owned()),
            })?;
        }
        BackendLaunch::PreloginRelease => {
            if same(store.machine()?.owner.as_deref(), caller.sid) {
                // Disable first: a failed delete must not leave a runnable marker.
                store.set_machine(&Machine::default())?;
                store.delete_snapshot()?;
            }
        }
        BackendLaunch::PreloginSnapshot { files } => {
            validate_snapshot(files)?;
            let machine = store.machine()?;
            if !machine.enabled || !same(machine.owner.as_deref(), caller.sid) {
                return Err(denied("the caller does not own the pre-login identity"));
            }
            store.write_snapshot(caller.sid, files)?;
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a pre-login request",
            ));
        }
    }
    state(store, caller.sid)
}

/// One service observation of the console and machine state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Observation {
    /// Active physical console session, if any (never session 0).
    pub console: Option<u32>,
    /// The console session has a logged-on user token.
    pub user_present: bool,
    /// [`ready`]: enabled, owned and the owner's snapshot exists.
    pub ready: bool,
    /// A service-created user backend (UIAccess) is still alive in any session,
    /// for example a disconnected session after fast user switching.
    pub user_backend_alive: bool,
    /// An update transaction owns the machine.
    pub maintenance: bool,
}
impl Observation {
    pub(crate) fn wanted(&self) -> Option<u32> {
        let session = self.console?;
        (!self.user_present && self.ready && !self.user_backend_alive && !self.maintenance).then_some(session)
    }
}

/// A running pre-login backend as seen by the supervisor.
pub(crate) trait Child {
    fn session(&self) -> u32;
    fn alive(&self) -> bool;
    /// Signal stop, give it the bounded release window, then terminate.
    fn stop(self);
}

/// Minimum interval between two starts, so an immediately failing backend
/// cannot become a SYSTEM process spawn loop at the service's poll rate.
pub(crate) const RESTART_DELAY: Duration = Duration::from_secs(5);

/// Owns the single pre-login child. At most one exists, and it is stopped
/// before any user backend may start, so one DeviceId never has two processes.
pub(crate) struct Supervisor<C: Child> {
    child: Option<C>,
    last_start: Option<Instant>,
}
impl<C: Child> Default for Supervisor<C> {
    fn default() -> Self {
        Self {
            child: None,
            last_start: None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tick {
    Idle,
    Running(u32),
    Stopped,
    Started(u32),
    Exited,
}
impl<C: Child> Supervisor<C> {
    #[cfg(test)]
    pub(crate) fn running(&self) -> Option<u32> {
        self.child.as_ref().filter(|c| c.alive()).map(Child::session)
    }
    pub(crate) fn tick(
        &mut self,
        observed: &Observation,
        now: Instant,
        start: impl FnOnce(u32) -> io::Result<C>,
    ) -> io::Result<Tick> {
        let wanted = observed.wanted();
        if let Some(child) = &self.child {
            if !child.alive() {
                self.child = None;
                return Ok(Tick::Exited);
            }
            if wanted == Some(child.session()) {
                return Ok(Tick::Running(child.session()));
            }
            self.stop();
            return Ok(Tick::Stopped);
        }
        let Some(session) = wanted else {
            return Ok(Tick::Idle);
        };
        if self
            .last_start
            .is_some_and(|at| now.saturating_duration_since(at) < RESTART_DELAY)
        {
            return Ok(Tick::Idle);
        }
        self.last_start = Some(now);
        self.child = Some(start(session)?);
        Ok(Tick::Started(session))
    }
    /// Stop before a user backend may start. Returns whether a child was running.
    pub(crate) fn stop(&mut self) -> bool {
        match self.child.take() {
            Some(child) => {
                child.stop();
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

    #[derive(Default)]
    struct Memory {
        machine: Machine,
        snapshot: Option<(String, BTreeMap<SnapshotFile, Vec<u8>>)>,
    }
    impl Store for Memory {
        fn machine(&self) -> io::Result<Machine> {
            Ok(self.machine.clone())
        }
        fn set_machine(&mut self, machine: &Machine) -> io::Result<()> {
            self.machine = machine.clone();
            Ok(())
        }
        fn snapshot_owner(&self) -> io::Result<Option<String>> {
            Ok(self.snapshot.as_ref().map(|(owner, _)| owner.clone()))
        }
        fn write_snapshot(&mut self, owner: &str, files: &[(SnapshotFile, Vec<u8>)]) -> io::Result<()> {
            self.snapshot = Some((owner.into(), files.iter().cloned().collect()));
            Ok(())
        }
        fn delete_snapshot(&mut self) -> io::Result<()> {
            self.snapshot = None;
            Ok(())
        }
    }
    const A: &str = "S-1-5-21-1-1001";
    const B: &str = "S-1-5-21-1-1002";
    fn files(seed: u8) -> Vec<(SnapshotFile, Vec<u8>)> {
        vec![
            (SnapshotFile::DeviceKey, vec![seed; 32]),
            (SnapshotFile::GroupState, vec![seed]),
        ]
    }
    fn caller(sid: &str, elevated: bool) -> Caller<'_> {
        Caller { sid, elevated }
    }
    fn view(reply: BackendLaunch) -> (bool, OwnerView, bool) {
        match reply {
            BackendLaunch::PreloginState {
                enabled,
                owner,
                snapshot,
            } => (enabled, owner, snapshot),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn snapshot_files_are_fixed_plain_names_with_unique_ids() {
        let names: std::collections::BTreeSet<_> = SnapshotFile::ALL.iter().map(|f| f.name()).collect();
        assert_eq!(names.len(), SnapshotFile::ALL.len());
        for file in SnapshotFile::ALL {
            assert_eq!(SnapshotFile::from_id(file as u8), Some(file));
            assert!(!file.name().contains(['/', '\\', ':']) && !file.name().starts_with('.'));
        }
        assert_eq!(SnapshotFile::from_id(0), None);
    }

    #[test]
    fn claim_requires_elevation_and_makes_the_caller_owner() {
        let mut store = Memory::default();
        let refused = handle(&mut store, &BackendLaunch::PreloginClaim, &caller(A, false));
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(store.machine, Machine::default());
        let claimed = handle(&mut store, &BackendLaunch::PreloginClaim, &caller(A, true)).unwrap();
        assert_eq!(view(claimed), (true, OwnerView::Caller, false));
        // Enabled but not yet runnable: no snapshot.
        assert!(!ready(&store).unwrap());
        handle(
            &mut store,
            &BackendLaunch::PreloginSnapshot { files: files(1) },
            &caller(A, false),
        )
        .unwrap();
        assert!(ready(&store).unwrap());
        let other = handle(&mut store, &BackendLaunch::PreloginStatus, &caller(B, false)).unwrap();
        assert_eq!(view(other), (true, OwnerView::Other, true));
    }

    #[test]
    fn only_the_owner_may_write_snapshots() {
        let mut store = Memory::default();
        let disabled = handle(
            &mut store,
            &BackendLaunch::PreloginSnapshot { files: files(1) },
            &caller(A, false),
        );
        assert_eq!(disabled.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        handle(&mut store, &BackendLaunch::PreloginClaim, &caller(A, true)).unwrap();
        let foreign = handle(
            &mut store,
            &BackendLaunch::PreloginSnapshot { files: files(2) },
            &caller(B, true),
        );
        assert_eq!(foreign.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert!(store.snapshot.is_none());
        let bad = handle(
            &mut store,
            &BackendLaunch::PreloginSnapshot {
                files: vec![(SnapshotFile::DeviceKey, vec![1; 32])],
            },
            &caller(A, false),
        );
        assert_eq!(bad.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn another_users_claim_replaces_the_identity_and_discards_the_old_snapshot() {
        let mut store = Memory::default();
        handle(&mut store, &BackendLaunch::PreloginClaim, &caller(A, true)).unwrap();
        handle(
            &mut store,
            &BackendLaunch::PreloginSnapshot { files: files(1) },
            &caller(A, false),
        )
        .unwrap();
        let claimed = handle(&mut store, &BackendLaunch::PreloginClaim, &caller(B, true)).unwrap();
        assert_eq!(view(claimed), (true, OwnerView::Caller, false));
        assert!(store.snapshot.is_none(), "A's identity must not run as B's");
        assert!(!ready(&store).unwrap());
        // A's later routine refresh is refused; A no longer owns the identity.
        assert!(
            handle(
                &mut store,
                &BackendLaunch::PreloginSnapshot { files: files(1) },
                &caller(A, false)
            )
            .is_err()
        );
        // Reclaiming by the same owner keeps the current snapshot.
        handle(
            &mut store,
            &BackendLaunch::PreloginSnapshot { files: files(2) },
            &caller(B, false),
        )
        .unwrap();
        handle(&mut store, &BackendLaunch::PreloginClaim, &caller(B, true)).unwrap();
        assert!(ready(&store).unwrap());
    }

    #[test]
    fn owner_release_disables_and_deletes_but_a_non_owner_release_changes_nothing() {
        let mut store = Memory::default();
        handle(&mut store, &BackendLaunch::PreloginClaim, &caller(A, true)).unwrap();
        handle(
            &mut store,
            &BackendLaunch::PreloginSnapshot { files: files(1) },
            &caller(A, false),
        )
        .unwrap();
        let other = handle(&mut store, &BackendLaunch::PreloginRelease, &caller(B, false)).unwrap();
        assert_eq!(view(other), (true, OwnerView::Other, true));
        assert!(ready(&store).unwrap());
        // Releasing needs no elevation: it only reduces machine-level access.
        let released = handle(&mut store, &BackendLaunch::PreloginRelease, &caller(A, false)).unwrap();
        assert_eq!(view(released), (false, OwnerView::None, false));
        assert!(store.snapshot.is_none());
        assert_eq!(store.machine, Machine::default());
    }

    #[test]
    fn an_installer_marker_without_a_snapshot_is_not_runnable_and_sids_compare_case_insensitively() {
        let mut store = Memory {
            machine: Machine {
                enabled: true,
                owner: Some(A.to_ascii_lowercase()),
            },
            snapshot: None,
        };
        assert!(!ready(&store).unwrap());
        handle(
            &mut store,
            &BackendLaunch::PreloginSnapshot { files: files(1) },
            &caller(A, false),
        )
        .unwrap();
        assert!(ready(&store).unwrap());
        store.machine.enabled = false;
        assert!(!ready(&store).unwrap());
    }

    struct Fake {
        session: u32,
        alive: Rc<RefCell<bool>>,
        log: Rc<RefCell<Vec<String>>>,
    }
    impl Child for Fake {
        fn session(&self) -> u32 {
            self.session
        }
        fn alive(&self) -> bool {
            *self.alive.borrow()
        }
        fn stop(self) {
            self.log.borrow_mut().push(format!("stop {}", self.session));
            *self.alive.borrow_mut() = false;
        }
    }
    fn login_screen(session: u32) -> Observation {
        Observation {
            console: Some(session),
            user_present: false,
            ready: true,
            user_backend_alive: false,
            maintenance: false,
        }
    }

    #[test]
    fn supervisor_runs_only_at_a_ready_login_screen_and_stops_when_a_user_appears() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let alive = Rc::new(RefCell::new(true));
        let start = |session| {
            log.borrow_mut().push(format!("start {session}"));
            Ok(Fake {
                session,
                alive: alive.clone(),
                log: log.clone(),
            })
        };
        let mut supervisor = Supervisor::default();
        let t0 = Instant::now();
        for (observed, expected) in [
            (Observation::default(), Tick::Idle),
            (
                Observation {
                    ready: false,
                    ..login_screen(1)
                },
                Tick::Idle,
            ),
            (
                Observation {
                    maintenance: true,
                    ..login_screen(1)
                },
                Tick::Idle,
            ),
        ] {
            assert_eq!(supervisor.tick(&observed, t0, start).unwrap(), expected);
        }
        assert_eq!(supervisor.tick(&login_screen(1), t0, start).unwrap(), Tick::Started(1));
        assert_eq!(supervisor.tick(&login_screen(1), t0, start).unwrap(), Tick::Running(1));
        // The user signs in: stop before the user backend can start.
        let signed_in = Observation {
            user_present: true,
            ..login_screen(1)
        };
        assert_eq!(supervisor.tick(&signed_in, t0, start).unwrap(), Tick::Stopped);
        assert_eq!(supervisor.running(), None);
        assert_eq!(*log.borrow(), ["start 1", "stop 1"]);
    }

    #[test]
    fn fast_user_switching_with_a_live_user_backend_never_starts_a_second_instance() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let start = |session| {
            log.borrow_mut().push(format!("start {session}"));
            Ok(Fake {
                session,
                alive: Rc::new(RefCell::new(true)),
                log: log.clone(),
            })
        };
        let mut supervisor = Supervisor::default();
        let t0 = Instant::now();
        // User A's backend still lives in its disconnected session; the console
        // shows a new logon session without a user.
        let switched = Observation {
            user_backend_alive: true,
            ..login_screen(2)
        };
        assert_eq!(supervisor.tick(&switched, t0, start).unwrap(), Tick::Idle);
        assert!(log.borrow().is_empty());
        // A running pre-login backend is stopped as soon as one appears.
        assert_eq!(supervisor.tick(&login_screen(2), t0, start).unwrap(), Tick::Started(2));
        assert_eq!(supervisor.tick(&switched, t0, start).unwrap(), Tick::Stopped);
        // And it follows the console to a new session, never two at once.
        let later = t0 + RESTART_DELAY;
        assert_eq!(
            supervisor.tick(&login_screen(3), later, start).unwrap(),
            Tick::Started(3)
        );
        let next = later + RESTART_DELAY;
        assert_eq!(supervisor.tick(&login_screen(4), next, start).unwrap(), Tick::Stopped);
        assert_eq!(
            supervisor.tick(&login_screen(4), next, start).unwrap(),
            Tick::Started(4)
        );
        assert_eq!(*log.borrow(), ["start 2", "stop 2", "start 3", "stop 3", "start 4"]);
    }

    #[test]
    fn an_exited_child_is_reaped_and_restarts_are_rate_limited() {
        let alive = Rc::new(RefCell::new(true));
        let log = Rc::new(RefCell::new(Vec::new()));
        let starts = RefCell::new(0);
        let start = |session| {
            *starts.borrow_mut() += 1;
            *alive.borrow_mut() = true;
            Ok(Fake {
                session,
                alive: alive.clone(),
                log: log.clone(),
            })
        };
        let mut supervisor = Supervisor::default();
        let t0 = Instant::now();
        assert_eq!(supervisor.tick(&login_screen(1), t0, start).unwrap(), Tick::Started(1));
        *alive.borrow_mut() = false;
        assert_eq!(supervisor.tick(&login_screen(1), t0, start).unwrap(), Tick::Exited);
        assert_eq!(supervisor.tick(&login_screen(1), t0, start).unwrap(), Tick::Idle);
        assert_eq!(
            supervisor.tick(&login_screen(1), t0 + RESTART_DELAY, start).unwrap(),
            Tick::Started(1)
        );
        assert_eq!(*starts.borrow(), 2);
        // The user backend launch path stops it unconditionally first.
        assert!(supervisor.stop());
        assert!(!supervisor.stop());
    }
}
