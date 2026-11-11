use super::super::{Handle, security, token};
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use oink_update::{
    WINDOWS_EPOCH,
    catalog::{self, Check, Expected, Kind, Outcome, Platform, VerifiedCatalog},
    feed::{self, Channel},
    fetch,
    ipc::{Reply, Request},
    trust::Roots,
};
use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};
use windows_sys::Win32::System::Threading::GetProcessId;
#[derive(Clone)]
struct Registration {
    session: Session,
    seen: Instant,
    quiesced: Option<String>,
}
fn registered_sessions(
    registrations: &BTreeMap<u32, Registration>,
    logged_on: &[u32],
    mut current: impl FnMut(&Session) -> Result<bool, Error>,
) -> Result<Vec<Session>, Error> {
    let mut active = Vec::new();
    for registration in registrations.values() {
        if logged_on.contains(&registration.session.id) && current(&registration.session)? {
            active.push(registration.session.clone());
        }
    }
    Ok(active)
}
const VERIFICATION_BUDGET: Duration = Duration::from_secs(120);
/// Automatic checks: after a successful check, and after a failed one.
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 3600);
const RETRY_INTERVAL: Duration = Duration::from_secs(2 * 3600);

pub(super) struct State {
    status: UpdateStatus,
    policy: MachinePolicy,
    queued: Option<Command>,
    local_approval: Option<oink_update::local::Package>,
    registrations: BTreeMap<u32, Registration>,
    transaction: Option<String>,
    shutdown: bool,
    prepared_at: Option<Instant>,
    verification_started: Option<Instant>,
    /// When the next automatic check is due; none until an Oink launch.
    next_check: Option<Instant>,
    /// The queued check is manual: it also offers a skipped build.
    manual_check: bool,
    /// The check the engine is running.
    running_check: Option<Check>,
    /// Bumped only when a new check replaces a queued or running one.
    check_revision: u64,
    revision: u64,
    wake: Option<Arc<super::super::wake::Event>>,
}
fn verification_current(state: &State, revision: u64) -> Result<(), Error> {
    if state.status.error == Some(Error::VerificationTimeout) {
        return Err(Error::VerificationTimeout);
    }
    if state.revision != revision || state.status.phase != Phase::Verifying {
        return Err(Error::Busy);
    }
    Ok(())
}
impl State {
    fn expire_verification(&mut self, now: Instant) {
        if self.status.phase == Phase::Verifying
            && self
                .verification_started
                .is_some_and(|start| now.saturating_duration_since(start) >= VERIFICATION_BUDGET)
        {
            self.revision = self.revision.wrapping_add(1);
            self.local_approval = None;
            self.queued = None;
            self.status.phase = Phase::Failed;
            self.status.error = Some(Error::VerificationTimeout);
            // A download that timed out in verification failed for its build.
            self.status.failed_build = self.status.candidate.as_ref().map(|c| c.build);
            tracing::warn!("package verification timed out before session drain");
        }
    }

    /// Checks are asynchronous: callers share a queued or running check and its
    /// result. Only a manual check replaces an automatic one, because it also
    /// offers a skipped build.
    fn request_check(&mut self, check: Check) {
        let current = if self.queued == Some(Command::Check) {
            Some(if self.manual_check {
                Check::Manual
            } else {
                Check::Automatic
            })
        } else {
            self.running_check
        };
        if current.is_none_or(|current| current == Check::Automatic && check == Check::Manual) {
            self.restart_check(check);
        }
        self.status.phase = Phase::Checking;
        self.status.error = None;
    }
    /// Queues a check whose result replaces any check queued or running now.
    fn restart_check(&mut self, check: Check) {
        self.check_revision = self.check_revision.wrapping_add(1);
        self.queued = Some(Command::Check);
        self.manual_check = check == Check::Manual;
        self.status.phase = Phase::Checking;
        self.status.error = None;
    }
    /// A backend registers once per launch of Oink, and every launch checks
    /// once. This service outlives the app, so a failure (issue #45: the
    /// network was down) is also retried then instead of at the next timer.
    fn launched(&mut self, now: Instant) {
        if self.policy.policy.preferences.checks_enabled {
            self.next_check = Some(now);
        }
    }
    /// When the next automatic check may start, if one is scheduled.
    fn automatic_due(&self, feed_enabled: bool, pending: bool) -> Option<Instant> {
        // A check would replace a package that is downloading or ready.
        let busy = matches!(
            self.status.phase,
            Phase::Downloading | Phase::Verifying | Phase::Ready | Phase::Quiescing | Phase::Installing
        );
        (feed_enabled
            && self.policy.policy.preferences.checks_enabled
            && self.transaction.is_none()
            && !pending
            && !busy)
            .then_some(self.next_check)
            .flatten()
    }
    /// The next command for the engine, with the kind of check it starts.
    fn next_command(&mut self, now: Instant, feed_enabled: bool) -> Option<(Command, Check)> {
        if let Some(command) = self.queued.take() {
            let check = if std::mem::take(&mut self.manual_check) {
                Check::Manual
            } else {
                Check::Automatic
            };
            return Some((command, check));
        }
        self.automatic_due(feed_enabled, false)
            .is_some_and(|at| now >= at)
            .then_some((Command::Check, Check::Automatic))
    }
    /// A check finished: the next automatic one is due a day after a success
    /// and two hours after a failure (no source verified).
    fn checked(&mut self, ok: bool, now: Instant) {
        let after = if ok { CHECK_INTERVAL } else { RETRY_INTERVAL };
        self.next_check = Some(now + after);
        self.status.next_check = self
            .policy
            .policy
            .preferences
            .checks_enabled
            .then(|| super::now() + after.as_secs() as i64);
    }
    fn apply_catalog(
        &mut self,
        catalog: VerifiedCatalog,
        envelope: Vec<u8>,
        channel: Channel,
        windows_build: u32,
        check: Check,
    ) -> Result<Option<Selected>, Error> {
        let plan = catalog.plan(&catalog::Selection {
            installed_build: self.status.current_build,
            successful_build: self.policy.successful_build,
            quarantine: &self.policy.quarantine,
            channel,
            os: os_version(windows_build),
            skipped_build: self.policy.policy.skipped_build,
            check,
        });
        let (plan, error) = match plan {
            Ok(plan) => (Some(plan), None),
            Err(Outcome::UpToDate) => (None, None),
            Err(Outcome::OsUnsupported) => (None, Some(Error::OsUnsupported)),
        };
        let mut candidate = None;
        let mut selected = None;
        let watermark = catalog.watermark().clone();
        if let Some(plan) = plan {
            feed::verify_version(&plan.release.version, &self.status.version)?;
            let release = transaction_release(&plan.release)?;
            candidate = Some(Candidate {
                local_package: false,
                build: release.build,
                version: release.version.clone(),
                size: release.package.length,
                published_at: None,
                severity: if plan.kind == Kind::Critical {
                    feed::Severity::Critical
                } else {
                    feed::Severity::Normal
                },
                notes: Some(release.notes.clone()),
                kind: plan.kind,
                notice: plan.notice,
            });
            selected = Some(Selected {
                catalog: Some(catalog),
                envelope,
                channel,
                kind: plan.kind,
                release,
                prepared: None,
            });
        }
        self.policy.watermarks.insert(catalog_watermark(channel), watermark);
        self.status.checked_at = Some(now());
        self.status.error = error;
        if self.status.failed_build != candidate.as_ref().map(|c| c.build) {
            self.status.failed_build = None;
        }
        self.status.phase = if candidate.is_some() {
            Phase::Available
        } else if error.is_some() {
            Phase::Failed
        } else {
            Phase::Idle
        };
        self.status.candidate = candidate;
        Ok(selected)
    }
    /// Only an offered candidate that is not manual-only can be downloaded.
    fn admit_download(&self) -> Result<(), Error> {
        match &self.status.candidate {
            Some(candidate) if candidate.kind != Kind::Manual => Ok(()),
            _ => Err(Error::Policy),
        }
    }
    /// A failed download keeps its candidate (and the title-bar button) and
    /// remembers the build the failure belongs to.
    fn download_failed(&mut self, build: u64, error: Error) {
        self.status.phase = Phase::Failed;
        self.status.error = Some(error);
        self.status.progress = None;
        self.status.failed_build = Some(build);
    }
    fn download_ready(&mut self, build: u64) {
        self.status.phase = Phase::Ready;
        self.status.progress = None;
        if self.status.failed_build == Some(build) {
            self.status.failed_build = None;
        }
    }
    /// Applies preferences that are already saved. Another channel's catalog
    /// decides what is offered (never an older build), so that checks again.
    fn apply_preferences(&mut self, preferences: Preferences) {
        let old = std::mem::replace(&mut self.policy.policy.preferences, preferences.clone());
        self.status.checks_enabled = preferences.checks_enabled;
        self.status.channel = preferences.channel();
        self.status.update_channel = preferences.update_channel;
        self.status.install_after_download = preferences.install_after_download;
        if preferences.channel() != old.channel() {
            self.revision = self.revision.wrapping_add(1);
            self.status.candidate = None;
            self.status.failed_build = None;
            self.restart_check(Check::Automatic);
        } else if preferences.checks_enabled && !old.checks_enabled {
            self.request_check(Check::Automatic);
        }
        if !preferences.checks_enabled {
            self.status.next_check = None;
        }
    }
    fn new() -> Result<Self, Error> {
        let receipt = installed()?;
        let policy = policy()?;
        let configured = oink_update::trust::compiled().is_ok();
        Ok(Self::with(initial_status(receipt, &policy, configured), policy))
    }
    fn with(status: UpdateStatus, policy: MachinePolicy) -> Self {
        Self {
            status,
            policy,
            queued: None,
            local_approval: None,
            registrations: BTreeMap::new(),
            transaction: None,
            shutdown: false,
            prepared_at: None,
            verification_started: None,
            next_check: None,
            manual_check: false,
            running_check: None,
            check_revision: 0,
            revision: 0,
            wake: None,
        }
    }
}
fn initial_status(receipt: msi::Receipt, policy: &MachinePolicy, configured: bool) -> UpdateStatus {
    let preferences = &policy.policy.preferences;
    UpdateStatus {
        current_build: receipt.build,
        version: receipt.version,
        checks_enabled: configured && preferences.checks_enabled,
        channel: preferences.channel(),
        update_channel: preferences.update_channel,
        install_after_download: preferences.install_after_download,
        phase: if configured { Phase::Idle } else { Phase::Disabled },
        candidate: None,
        checked_at: None,
        next_check: None,
        deferred_until: policy.policy.deferred_until,
        blockers: Vec::new(),
        progress: None,
        error: (!configured).then_some(Error::NotConfigured),
        failed_build: None,
        critical_prompted_build: policy.policy.critical_prompted_build,
        scope: Scope::Machine,
        // Every pipe client is the authenticated interactive user of its own
        // session running installed Oink. Oink has one user, who changes the
        // update preferences without elevation.
        can_change_policy: true,
    }
}
fn transaction_phase(journal: &oink_update::transaction::Journal) -> Phase {
    use oink_update::transaction::Stage;
    match journal.stage {
        Stage::Prepared => Phase::Quiescing,
        Stage::AwaitingReboot | Stage::RollbackAwaitingReboot => Phase::AwaitingReboot,
        Stage::Committed => Phase::Committed,
        Stage::RolledBack if journal.rollback_attempted => Phase::RolledBack,
        Stage::RolledBack if journal.failure.is_some() => Phase::Failed,
        Stage::RolledBack => Phase::Idle,
        Stage::RepairRequired => Phase::RepairRequired,
        _ => Phase::Installing,
    }
}
fn journal_status(name: &str) -> Result<(Phase, Option<Error>), Error> {
    let journal = oink_update::transaction::Journal::parse(&protected::read(
        &protected::maintenance().map_err(|_| Error::Storage)?.join(name),
        65536,
    )?)?;
    Ok((transaction_phase(&journal), journal.failure))
}
pub(super) fn dispatch(state: &Arc<Mutex<State>>, id: u32, process: &Handle, request: Request) -> Reply {
    let mut state = state.lock().unwrap();
    state.expire_verification(Instant::now());
    let wake = !matches!(&request, Request::Poll { .. } | Request::Query)
        || (matches!(&request, Request::Poll { .. }) && state.transaction.is_some() && !state.shutdown);
    if pending() {
        let (phase, error) =
            journal_status("pending.json").unwrap_or((Phase::RepairRequired, Some(Error::RepairRequired)));
        state.status.phase = phase;
        state.status.error = error;
    }
    let result = (|| {
        // SAFETY: process handle is pinned by authenticated pipe dispatch.
        let pid = unsafe { GetProcessId(process.0) };
        let created = protected::created(process.0)?;
        let known = state
            .registrations
            .get(&id)
            .is_some_and(|r| r.session.pid == pid && r.session.created == created);
        match request {
            Request::ApproveLocal { path, sha256 } => {
                // Kernel-authenticated process identity + elevated token are the
                // authority. Medium callers cannot manufacture a consent record.
                if !local_admitted(
                    super::pipe::elevated(process),
                    pending(),
                    state.transaction.is_some() || state.local_approval.is_some(),
                    state.status.phase,
                ) {
                    return Err(Error::Policy);
                }
                oink_update::trust::local_packages()?;
                if !oink_update::local::hex(&sha256) {
                    return Err(Error::Signature);
                }
                // Snapshot before replying: the elevated helper's successful
                // reply lets the backend dispose its user-session spool file.
                let (bytes, package) = super::local::read(std::path::Path::new(&path))?;
                if package.sha256 != sha256 {
                    return Err(Error::Signature);
                }
                package.validate(&state.status.version, state.status.current_build, "x64")?;
                let directory = protected::maintenance().map_err(|_| Error::Storage)?.join("downloads");
                protected::directory(&directory).map_err(|_| Error::Storage)?;
                protected::write(&directory.join("candidate.msi"), &bytes)?;
                state.revision = state.revision.wrapping_add(1);
                tracing::info!(build = package.build, "elevated local package accepted");
                state.local_approval = Some(package);
                state.verification_started = Some(Instant::now());
                state.queued = None;
                state.status.phase = Phase::Verifying;
                state.status.candidate = None;
                state.status.error = None;
            }
            Request::Query => {}
            Request::Startup => {
                if pending() {
                    if !super::input_allowed() {
                        return Err(Error::Busy);
                    }
                    let journal = oink_update::transaction::Journal::parse(&protected::read(
                        &protected::maintenance()
                            .map_err(|_| Error::Storage)?
                            .join("pending.json"),
                        65536,
                    )?)?;
                    return Ok(Reply::HealthCheck {
                        transaction: journal.transaction,
                        build: state.status.current_build,
                    });
                }
            }
            Request::UserQuit => {
                if !known || pending() {
                    return Err(Error::Busy);
                }
                state.policy.policy.next_quit = false;
                save_policy(&state.policy)?;
            }
            Request::Register { build } => {
                if build != state.status.current_build {
                    return Err(Error::Policy);
                }
                let sid =
                    security::sid_bytes(token(process.0).map_err(|_| Error::Policy)?.0).map_err(|_| Error::Policy)?;
                if state.transaction.is_some() && !known {
                    return Err(Error::Busy);
                }
                state.registrations.insert(
                    id,
                    Registration {
                        session: Session {
                            id,
                            pid,
                            created,
                            sid: STANDARD.encode(sid),
                            build,
                        },
                        seen: Instant::now(),
                        quiesced: None,
                    },
                );
                // The same backend registers again after a pipe failure; only a
                // new process is a launch.
                if !known && !pending() {
                    state.launched(Instant::now());
                }
                return Ok(Reply::Registered);
            }
            Request::Poll { transaction } => {
                if !known {
                    return Err(Error::Policy);
                }
                state.registrations.get_mut(&id).unwrap().seen = Instant::now();
                if let Some(tx) = &state.transaction {
                    if state.shutdown {
                        return Ok(Reply::Shutdown {
                            transaction: tx.clone(),
                        });
                    }
                    return Ok(Reply::Prepare {
                        transaction: tx.clone(),
                    });
                }
                if pending() {
                    let journal = oink_update::transaction::Journal::parse(&protected::read(
                        &protected::maintenance()
                            .map_err(|_| Error::Storage)?
                            .join("pending.json"),
                        65536,
                    )?)?;
                    if matches!(journal.stage, oink_update::transaction::Stage::Recovering) {
                        return if state.registrations[&id].quiesced.as_ref() == Some(&journal.transaction) {
                            Ok(Reply::Shutdown {
                                transaction: journal.transaction,
                            })
                        } else {
                            Ok(Reply::Prepare {
                                transaction: journal.transaction,
                            })
                        };
                    }
                    if super::input_allowed() {
                        return Ok(Reply::HealthCheck {
                            transaction: journal.transaction,
                            build: state.status.current_build,
                        });
                    }
                }
                if let Some(transaction) = transaction {
                    return Ok(Reply::Abort { transaction });
                }
            }
            Request::Quiesced { transaction } => {
                if !known || !transaction_matches(&state, &transaction) {
                    return Err(Error::Policy);
                }
                let registration = state.registrations.get_mut(&id).unwrap();
                registration.quiesced = Some(transaction);
                registration.seen = Instant::now();
                return Ok(Reply::Registered);
            }
            Request::Busy { transaction, blockers } => {
                if !known || state.transaction.as_ref() != Some(&transaction) || blockers.len() > 32 {
                    return Err(Error::Policy);
                }
                state.status.blockers = blockers;
                if state.transaction.as_ref() == Some(&transaction) {
                    abort(&mut state)?;
                    state.status.phase = Phase::Failed;
                    state.status.error = Some(Error::Busy);
                }
            }
            Request::Health {
                transaction,
                build,
                input_ready,
                routing_ready,
            } => {
                if !known || build != state.status.current_build || !input_ready || !routing_ready {
                    return Err(Error::Policy);
                }
                let directory = transaction_dir(&transaction)?;
                let journal = oink_update::transaction::Journal::parse(&protected::read(
                    &protected::maintenance()
                        .map_err(|_| Error::Storage)?
                        .join("pending.json"),
                    65536,
                )?)?;
                if journal.transaction != transaction {
                    return Err(Error::Policy);
                }
                protected::write(
                    &directory.join(format!("health-{id}.json")),
                    &serde_json::to_vec(&HealthProof {
                        session: state.registrations[&id].session.clone(),
                        observed_unix: now(),
                    })
                    .map_err(|_| Error::Metadata)?,
                )?;
                return Ok(Reply::Registered);
            }
            Request::Command { command } => {
                if !matches!(command, Command::Cancel) {
                    oink_update::trust::compiled()?;
                }
                let preparing =
                    (state.transaction.is_some() || state.status.phase == Phase::Quiescing) && !state.shutdown;
                if !command_admitted(
                    &command,
                    preparing,
                    preparing || state.transaction.is_some() || pending(),
                ) {
                    return Err(Error::Busy);
                }
                // Check, Prompted, Download and preference changes leave a
                // download or preparation in progress alone (see their arms).
                if !matches!(
                    command,
                    Command::Check | Command::Prompted { .. } | Command::SetPreferences { .. } | Command::Download
                ) {
                    state.revision = state.revision.wrapping_add(1);
                }
                match command {
                    Command::OpenLocal { .. } => return Err(Error::Policy),
                    Command::SetPreferences { preferences } => {
                        // Authenticated interactive callers only (pipe::peer);
                        // no elevation: these are the one user's preferences.
                        if preparing && preferences.channel() != state.policy.policy.preferences.channel() {
                            abort(&mut state)?;
                        }
                        let mut policy = state.policy.clone();
                        policy.policy.preferences = preferences.clone();
                        save_policy(&policy)?;
                        state.apply_preferences(preferences);
                    }
                    Command::Defer { choice } => {
                        if preparing {
                            abort(&mut state)?;
                        }
                        state.policy.policy.defer(choice, now());
                        save_policy(&state.policy)?;
                        state.status.deferred_until = state.policy.policy.deferred_until;
                    }
                    Command::Skip { expected_build } => {
                        // Critical and manual updates cannot be skipped.
                        if state.status.candidate.as_ref().map(|c| (c.build, c.kind))
                            != Some((expected_build, Kind::Normal))
                        {
                            return Err(Error::Policy);
                        }
                        if preparing {
                            abort(&mut state)?;
                        }
                        state.policy.policy.skipped_build = Some(expected_build);
                        save_policy(&state.policy)?;
                    }
                    Command::Prompted { expected_build } => {
                        if state.status.candidate.as_ref().map(|c| c.build) != Some(expected_build) {
                            return Err(Error::Policy);
                        }
                        state.policy.policy.critical_prompted_build = Some(expected_build);
                        save_policy(&state.policy)?;
                        state.status.critical_prompted_build = Some(expected_build);
                    }
                    Command::Install { expected_build } => {
                        if state.status.phase != Phase::Ready
                            || state.status.candidate.as_ref().map(|c| c.build) != Some(expected_build)
                        {
                            return Err(Error::Policy);
                        }
                        // Explicit intent closes admission immediately; every live
                        // backend must still ACK its bounded correctness fence.
                        state.status.phase = Phase::Quiescing;
                        state.status.blockers.clear();
                        state.status.error = None;
                        state.queued = Some(Command::Install { expected_build });
                    }
                    Command::Check => state.request_check(Check::Manual),
                    Command::Download => {
                        state.admit_download()?;
                        state.revision = state.revision.wrapping_add(1);
                        state.queued = Some(Command::Download);
                    }
                    Command::Cancel => {
                        abort(&mut state)?;
                        state.queued = Some(Command::Cancel);
                    }
                }
            }
        }
        Ok(Reply::Status {
            status: Box::new(state.status.clone()),
        })
    })();
    if wake && let Some(send) = &state.wake {
        send.signal();
    }
    result.unwrap_or_else(|error| Reply::Error { error })
}
fn local_admitted(elevated: bool, pending: bool, active: bool, phase: Phase) -> bool {
    elevated
        && !pending
        && !active
        && !matches!(
            phase,
            Phase::Downloading | Phase::Verifying | Phase::Quiescing | Phase::Installing
        )
}
fn abort(state: &mut State) -> Result<(), Error> {
    if state.shutdown {
        return Err(Error::Transaction);
    }
    if let Some(transaction) = state.transaction.take() {
        protected::write(&transaction_dir(&transaction)?.join("abort.json"), b"{\"schema\":1}")?;
    }
    state.prepared_at = None;
    state.status.phase = Phase::Ready;
    for registration in state.registrations.values_mut() {
        registration.quiesced = None;
    }
    Ok(())
}
fn command_admitted(command: &Command, preparing: bool, active: bool) -> bool {
    !active
        || (preparing
            && matches!(
                command,
                Command::SetPreferences { .. }
                    | Command::Defer { .. }
                    | Command::Skip { .. }
                    | Command::Prompted { .. }
                    | Command::Cancel
            ))
}
pub(super) fn serve() {
    let Some(events) = super::super::broker::events() else {
        return;
    };
    if protected::system().is_err() {
        return;
    }
    let Ok(mut initial) = State::new() else {
        return;
    };
    let wake = events.engine.clone();
    initial.wake = Some(wake.clone());
    let state = Arc::new(Mutex::new(initial));
    let engine_state = state.clone();
    let engine = std::thread::spawn(move || engine(engine_state, wake));
    let mut workers = BTreeMap::<u32, Endpoint>::new();
    let mut retries = BTreeMap::<u32, Instant>::new();
    while !super::super::broker::STOP.load(Ordering::Acquire) {
        let now = Instant::now();
        let mut unavailable = None;
        for worker in workers.values() {
            worker.changed.signal();
        }
        if let Ok(sessions) = super::pipe::sessions() {
            let logged_off: Vec<_> = workers.keys().filter(|id| !sessions.contains(id)).copied().collect();
            for id in logged_off {
                workers.remove(&id); // Drop cancels and joins the obsolete endpoint.
            }
            retries.retain(|id, _| sessions.contains(id));
            for id in sessions {
                if workers.get(&id).is_some_and(|worker| !worker.finished()) {
                    continue;
                }
                if let Some(mut worker) = workers.remove(&id)
                    && let Err(error) = worker.join()
                {
                    eprintln!("Oink update session endpoint: {error}");
                    retries.insert(id, now + Duration::from_millis(500));
                }
                if retries.get(&id).is_some_and(|at| *at > now) {
                    continue;
                }
                match Endpoint::new(id, state.clone(), events.update.clone()) {
                    Ok(worker) => {
                        workers.insert(id, worker);
                        retries.remove(&id);
                    }
                    Err(error) => {
                        eprintln!("Oink update session endpoint: {error}");
                        retries.insert(id, Instant::now() + Duration::from_millis(500));
                    }
                }
            }
        } else {
            unavailable = Some(now + Duration::from_millis(500));
        }
        // No session enumeration timer at idle. The old recovery cadence is
        // retained only after an actual endpoint/observation failure.
        let deadline = retries.values().copied().chain(unavailable).min();
        if super::super::wake::wait(&[events.stop.raw(), events.update.raw()], deadline).is_err() {
            break;
        }
    }
    drop(workers);
    let _ = engine.join();
}

struct Endpoint {
    changed: Arc<super::super::wake::Event>,
    cancel: Arc<super::super::wake::Event>,
    task: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    _completion: Option<super::super::wake::Object>,
}
impl Endpoint {
    fn new(id: u32, state: Arc<Mutex<State>>, completed: Arc<super::super::wake::Event>) -> std::io::Result<Self> {
        let changed = super::super::wake::Event::new(false)?;
        let cancel = super::super::wake::Event::new(true)?;
        let (session_changed, cancelled) = (changed.clone(), cancel.clone());
        let task = std::thread::Builder::new()
            .name(format!("oink-update-session-{id}"))
            .spawn(move || super::pipe::serve(id, state, session_changed, cancelled))?;
        let mut endpoint = Self {
            changed,
            cancel,
            task: Some(task),
            _completion: None,
        };
        // A signal from inside the thread can precede is_finished(). Wait for
        // the actual thread object instead, so endpoint recreation cannot stall.
        endpoint._completion = Some(super::super::wake::Object::thread(
            endpoint.task.as_ref().unwrap(),
            completed,
        )?);
        Ok(endpoint)
    }
    fn finished(&self) -> bool {
        self.task.as_ref().is_none_or(|task| task.is_finished())
    }
    fn join(&mut self) -> std::io::Result<()> {
        self.task.take().map_or(Ok(()), |task| {
            task.join()
                .map_err(|_| std::io::Error::other("update endpoint panicked"))?
        })
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        self.cancel.signal();
        let _ = self.join();
    }
}
struct Selected {
    /// None for an elevated local package.
    catalog: Option<VerifiedCatalog>,
    envelope: Vec<u8>,
    channel: Channel,
    kind: Kind,
    release: Release,
    prepared: Option<worker::PreparedPackage>,
}
/// The catalog of `channel` from the first source whose copy fully verifies,
/// with its exact bytes for the install worker to verify again.
fn fetch_catalog(
    trust: &feed::Trust<'_>,
    sources: &[String],
    channel: Channel,
    seen: &feed::Watermark,
    now: i64,
    get: impl FnMut(&str) -> Result<Vec<u8>, Error>,
) -> Result<(VerifiedCatalog, Vec<u8>), Error> {
    let expected = Expected {
        platform: Platform::Windows,
        architecture: ARCH,
        channel,
    };
    fetch::first_verified(
        sources,
        &catalog::path(Platform::Windows, WINDOWS_EPOCH, ARCH, channel),
        get,
        |bytes| Ok((catalog::verify(bytes, trust, &expected, seen, now)?, bytes.to_vec())),
    )
}
fn check_feed(state: &Arc<Mutex<State>>, roots: &Roots, check: Check) -> Result<Option<Selected>, Error> {
    let (channel, seen, revision) = {
        let state = state.lock().unwrap();
        let channel = state.policy.policy.preferences.channel();
        let seen = state.policy.watermarks.get(&catalog_watermark(channel)).cloned();
        (channel, seen.unwrap_or_default(), state.check_revision)
    };
    let sources = oink_update::trust::sources()?;
    let (catalog, envelope) = fetch_catalog(&roots.feed(), &sources, channel, &seen, now(), |url| {
        fetch::get(url, feed::MAX_FEED as u64, false, &mut |_, _| {})
    })?;
    let windows_build = worker::windows_build()?;
    let mut state = state.lock().unwrap();
    if state.check_revision != revision {
        return Err(Error::Busy);
    }
    let selected = state.apply_catalog(catalog, envelope, channel, windows_build, check)?;
    save_policy(&state.policy)?;
    Ok(selected)
}
fn download(selected: &mut Selected, roots: &Roots, state: &Arc<Mutex<State>>) -> Result<(), Error> {
    let catalog = selected.catalog.as_ref().ok_or(Error::Policy)?;
    catalog.ensure_fresh(now())?;
    let package = &selected.release.package;
    let mut shown = None;
    let bytes = fetch::get(&package.url, package.length, true, &mut |received, _| {
        let percent = (received.min(package.length) * 100 / package.length) as u8;
        if shown != Some(percent) {
            shown = Some(percent);
            let mut state = state.lock().unwrap();
            if state.status.phase == Phase::Downloading {
                state.status.progress = Some(percent);
            }
        }
    })?;
    feed::verify_package(&bytes, package, &roots.feed())?;
    let directory = protected::maintenance().map_err(|_| Error::Storage)?.join("downloads");
    protected::directory(&directory).map_err(|_| Error::Storage)?;
    protected::write(&directory.join("candidate.msi"), &bytes)?;
    let installed = {
        let mut current = state.lock().unwrap();
        current.status.phase = Phase::Verifying;
        current.status.progress = None;
        current.verification_started = Some(Instant::now());
        current.status.current_build
    };
    let previous = catalog
        .catalog()
        .releases
        .iter()
        .find(|r| r.build == installed)
        .map(transaction_release)
        .transpose()?;
    selected.prepared = Some(worker::prepare_package(
        selected.release.clone(),
        previous,
        &selected.envelope,
        selected.channel,
        &worker::PackageTrust::Feed(roots.clone()),
        false,
    )?);
    Ok(())
}
fn engine(state: Arc<Mutex<State>>, wake: Arc<super::super::wake::Event>) {
    let events = super::super::broker::events().unwrap();
    // Arm before reading the durable journal. External installer/worker writes
    // publish by rename; download stream writes do not wake this loop.
    let Ok(mut files) = super::maintenance_notifications() else {
        tracing::error!("update engine cannot subscribe to maintenance storage");
        return;
    };
    // Without compiled release trust the feed is off. Elevated local approvals
    // still need a live engine, package inspection, recovery and health checks.
    let roots = oink_update::trust::compiled();
    tracing::info!(feed_enabled = roots.is_ok(), "Windows local update engine running");
    let mut selected: Option<Selected> = None;
    let mut was_pending = pending();
    let mut housekeeping: Option<Cleanup> = None;
    let mut cleanup_retry = None;
    let mut worker_process: Option<super::super::wake::Process> = None;
    let mut recovery_retry = None;
    let mut cleanup_needed = true;
    let mut observed_input = None;
    while !super::super::broker::STOP.load(Ordering::Acquire) {
        state.lock().unwrap().expire_verification(Instant::now());
        let is_pending = pending();
        // Name changes in the maintenance directory wake this loop, including
        // an installer's relaunch request published by rename.
        super::relaunch::consume();
        let input = (is_pending, super::input_allowed());
        if observed_input != Some(input) {
            observed_input = Some(input);
            super::super::broker::changed();
        }
        if is_pending && worker_process.as_ref().is_some_and(|process| process.exited()) {
            worker_process = None;
            // A worker that exits with pending state is unfinished recovery, not
            // an idle poll. Preserve the former one-second recovery cadence.
            recovery_retry = Some(Instant::now() + Duration::from_secs(1));
        }
        if is_pending && recovery_retry.is_none_or(|at| Instant::now() >= at) {
            let watched = (|| {
                let process =
                    worker::resume_pending().map_err(|_| std::io::Error::other("update worker unavailable"))?;
                let identity = super::super::wake::Process::identity(&process)?;
                if worker_process.as_ref().is_none_or(|old| !old.matches(identity)) {
                    worker_process = Some(super::super::wake::Process::new(process, wake.clone())?);
                }
                Ok::<_, std::io::Error>(())
            })();
            recovery_retry = watched.is_err().then(|| Instant::now() + Duration::from_secs(1));
        } else if !is_pending {
            worker_process = None;
            recovery_retry = None;
        }
        if was_pending && !is_pending {
            let mut state = state.lock().unwrap();
            if let Ok(policy) = policy() {
                state.policy = policy;
            }
            state.transaction = None;
            state.shutdown = false;
            let (phase, error) =
                journal_status("last-result.json").unwrap_or((Phase::RepairRequired, Some(Error::RepairRequired)));
            state.status.phase = phase;
            state.status.candidate = None;
            state.status.error = error;
            // Preserve the durable install outcome for the relaunched UI;
            // an automatic check must not immediately replace it with Idle.
            state.next_check = Some(Instant::now() + CHECK_INTERVAL);
            cleanup_needed = true;
            selected = None;
        }
        was_pending = is_pending;
        // Consume consent before scheduling another feed check. Copy untrusted
        // input only after elevation, then bind the exact displayed hash in the
        // protected staging directory before inspecting any installation plan.
        let approval = {
            let mut current = state.lock().unwrap();
            current.local_approval.take().map(|package| (package, current.revision))
        };
        if let Some((package, revision)) = approval {
            let result = (|| {
                let _ = finish_housekeeping(&mut housekeeping);
                let receipt = installed()?;
                package.validate(&receipt.version, receipt.build, "x64")?;
                if package.minimum_windows_build > worker::windows_build()? {
                    return Err(Error::OsUnsupported);
                }
                let release = package.release();
                let local_trust = worker::PackageTrust::Local(oink_update::trust::local_packages()?);
                let prepared =
                    worker::prepare_package(release.clone(), None, &[], Channel::default(), &local_trust, true)?;
                let mut current = state.lock().unwrap();
                current.expire_verification(Instant::now());
                verification_current(&current, revision)?;
                selected = Some(Selected {
                    catalog: None,
                    envelope: vec![],
                    channel: Channel::default(),
                    kind: Kind::Normal,
                    release,
                    prepared: Some(prepared),
                });
                current.status.candidate = Some(package.candidate());
                current.status.phase = Phase::Ready;
                current.verification_started = None;
                current.queued = Some(Command::Install {
                    expected_build: package.build,
                });
                Ok::<_, Error>(())
            })();
            if let Err(error) = result {
                let mut state = state.lock().unwrap();
                // A timeout/cancel invalidates this preparation. A newer
                // approval must keep its own status when the old I/O returns.
                if state.revision == revision {
                    state.status.phase = Phase::Failed;
                    state.status.error = Some(error);
                }
            }
        }
        let command = {
            let mut state = state.lock().unwrap();
            if state.transaction.is_some() || pending() {
                None
            } else {
                state
                    .next_command(Instant::now(), roots.is_ok())
                    .map(|(command, check)| {
                        if command == Command::Check {
                            // Publish background admission under the same lock, so a
                            // manual check arriving before HTTP starts joins it too.
                            state.running_check = Some(check);
                            state.status.phase = Phase::Checking;
                            state.status.error = None;
                        }
                        (command, check, state.revision, state.check_revision)
                    })
            }
        };
        // A result only counts while nothing replaced its command: checks by a
        // newer check, everything else by the command revision.
        let gate = command
            .as_ref()
            .map(|(command, _, revision, check_revision)| (*command == Command::Check, *revision, *check_revision));
        let mut failed_download = None;
        let result = (|| {
            if let Some((command, check, revision, check_revision)) = command {
                match command {
                    Command::Check => {
                        // A failed check keeps an offered candidate downloadable.
                        if state.lock().unwrap().status.candidate.is_none() {
                            selected = None;
                        }
                        let checked = roots
                            .as_ref()
                            .map_err(|e| *e)
                            .and_then(|roots| check_feed(&state, roots, check));
                        {
                            let mut state = state.lock().unwrap();
                            state.running_check = None;
                            if state.check_revision == check_revision {
                                state.checked(checked.is_ok(), Instant::now());
                            }
                        }
                        selected = checked?;
                    }
                    Command::Download => {
                        let offered = state.lock().unwrap().status.candidate.as_ref().map(|c| c.build);
                        let candidate = selected
                            .as_mut()
                            .filter(|s| Some(s.release.build) == offered && s.kind != Kind::Manual)
                            .ok_or(Error::Policy)?;
                        let build = candidate.release.build;
                        if candidate.prepared.is_none() {
                            failed_download = Some(build);
                            // Only this engine starts staging. Join any background
                            // collection before touching a new artifact, without
                            // holding State or delaying UI/status requests.
                            let _ = finish_housekeeping(&mut housekeeping);
                            {
                                // A new attempt replaces the previous failure;
                                // otherwise Ready would still carry its error.
                                let mut state = state.lock().unwrap();
                                state.status.phase = Phase::Downloading;
                                state.status.error = None;
                                state.status.progress = Some(0);
                            }
                            download(candidate, roots.as_ref().map_err(|e| *e)?, &state)?;
                            let current = state.lock().unwrap();
                            verification_current(&current, revision)?;
                        }
                        failed_download = None;
                        let mut state = state.lock().unwrap();
                        state.verification_started = None;
                        state.download_ready(build);
                    }
                    Command::Install { expected_build } => {
                        let candidate = selected.as_ref().ok_or(Error::Policy)?;
                        if candidate.prepared.is_none() || candidate.release.build != expected_build {
                            return Err(Error::Policy);
                        }
                        prepare(&state, candidate, revision)?;
                    }
                    Command::Cancel => {
                        cleanup_needed = true;
                        selected = None;
                        let mut state = state.lock().unwrap();
                        state.status.phase = Phase::Idle;
                        state.status.candidate = None;
                        state.status.error = None;
                        state.status.progress = None;
                        state.status.blockers.clear();
                    }
                    _ => {}
                }
            }
            handover(&state)
        })();
        if let Err(error) = result {
            let mut state = state.lock().unwrap();
            let current = gate.is_none_or(|(check, revision, check_revision)| {
                if check {
                    state.check_revision == check_revision
                } else {
                    state.revision == revision
                }
            });
            if current {
                if let Some(build) = failed_download {
                    state.download_failed(build, error);
                } else {
                    state.status.error = Some(error);
                    state.status.phase = Phase::Failed;
                }
            }
        }
        if housekeeping.as_ref().is_some_and(|task| task.thread.is_finished()) {
            cleanup_needed = finish_housekeeping(&mut housekeeping) == Some(Err(Error::Busy));
            cleanup_retry = cleanup_needed.then(|| Instant::now() + Duration::from_secs(1));
        }
        if cleanup_needed
            && cleanup_retry.is_none_or(|at| Instant::now() >= at)
            && housekeeping.is_none()
            && !pending()
            && selected.as_ref().is_none_or(|candidate| candidate.prepared.is_none())
        {
            match Cleanup::new(wake.clone()) {
                Ok(task) => housekeeping = Some(task),
                Err(error) => {
                    tracing::warn!(%error, "cannot watch update cleanup completion");
                    cleanup_retry = Some(Instant::now() + Duration::from_secs(1));
                }
            }
        }
        let deadline = {
            let state = state.lock().unwrap();
            engine_deadline(&state, Instant::now(), roots.is_ok(), pending())
        };
        let retry = cleanup_retry.filter(|_| {
            cleanup_needed
                && housekeeping.is_none()
                && !pending()
                && selected.as_ref().is_none_or(|candidate| candidate.prepared.is_none())
        });
        let deadline = deadline.into_iter().chain(retry).chain(recovery_retry).min();
        match super::super::wake::wait(&[events.stop.raw(), wake.raw(), files.raw()], deadline) {
            Ok(super::super::wake::Woke::Signal(0)) => break,
            Ok(super::super::wake::Woke::Signal(2)) => {
                if let Err(error) = files.rearm() {
                    tracing::error!(%error, "cannot rearm maintenance notification");
                    break;
                }
            }
            Err(error) => {
                tracing::error!(%error, "update engine wait failed");
                break;
            }
            _ => {}
        }
    }
    let _ = finish_housekeeping(&mut housekeeping);
}
fn engine_deadline(state: &State, now: Instant, feed_enabled: bool, pending: bool) -> Option<Instant> {
    let check = state.automatic_due(feed_enabled, pending).map(|at| at.max(now));
    let verification = (state.status.phase == Phase::Verifying)
        .then_some(state.verification_started.map(|at| at + VERIFICATION_BUDGET))
        .flatten();
    let handover = (state.transaction.is_some() && !state.shutdown)
        .then_some(state.prepared_at.map(|at| at + Duration::from_secs(20)))
        .flatten();
    [check, verification, handover].into_iter().flatten().min()
}
struct Cleanup {
    thread: std::thread::JoinHandle<Result<(), Error>>,
    _completion: super::super::wake::Object,
}
impl Cleanup {
    fn new(event: Arc<super::super::wake::Event>) -> std::io::Result<Self> {
        let thread = std::thread::spawn(worker::cleanup_completed);
        match super::super::wake::Object::thread(&thread, event) {
            Ok(completion) => Ok(Self {
                thread,
                _completion: completion,
            }),
            Err(error) => {
                let _ = thread.join();
                Err(error)
            }
        }
    }
}
fn finish_housekeeping(task: &mut Option<Cleanup>) -> Option<Result<(), Error>> {
    let result = task.take()?.thread.join().unwrap_or(Err(Error::Storage));
    if let Err(error) = result
        && error != Error::Busy
    {
        tracing::warn!(%error, "background update cleanup failed");
    }
    Some(result)
}
fn prepare(state: &Arc<Mutex<State>>, selected: &Selected, expected_revision: u64) -> Result<(), Error> {
    if let Some(catalog) = &selected.catalog {
        catalog.ensure_fresh(now())?;
    }
    let sessions = super::pipe::sessions().map_err(|_| Error::Busy)?;
    let (registrations, revision) = {
        let state = state.lock().unwrap();
        if state.transaction.is_some() || pending() || state.revision != expected_revision {
            return Err(Error::Busy);
        }
        // Only sessions with a running registered backend participate. A logged
        // on session with no Oink process does not need a drain ACK or relaunch.
        let registrations = registered_sessions(&state.registrations, &sessions, worker::process_current)?;
        (registrations, state.revision)
    };
    let transaction = worker::stage(selected.prepared.as_ref().ok_or(Error::Policy)?, registrations)?;
    let mut state = state.lock().unwrap();
    if state.revision != revision {
        protected::write(&transaction_dir(&transaction)?.join("abort.json"), b"{\"schema\":1}")?;
        return Err(Error::Busy);
    }
    state.transaction = Some(transaction);
    state.prepared_at = Some(Instant::now());
    state.status.phase = Phase::Quiescing;
    state.status.blockers.clear();
    // Publishing a maintenance transaction changes input admission now; the
    // supervisor no longer discovers it on a fixed polling interval.
    super::super::broker::changed();
    Ok(())
}
fn handover(state: &Arc<Mutex<State>>) -> Result<(), Error> {
    let mut state = state.lock().unwrap();
    let Some(transaction) = state.transaction.clone() else {
        return Ok(());
    };
    if transaction_dir(&transaction)?.join("abort.json").exists() {
        state.transaction = None;
        state.shutdown = false;
        state.prepared_at = None;
        for registration in state.registrations.values_mut() {
            registration.quiesced = None;
        }
        return Ok(());
    }
    if state.shutdown {
        return Ok(());
    }
    if state
        .prepared_at
        .is_none_or(|at| at.elapsed() >= Duration::from_secs(20))
    {
        abort(&mut state)?;
        return Err(Error::Busy);
    }
    let directory = transaction_dir(&transaction)?;
    let sessions = super::pipe::sessions().map_err(|_| Error::Busy)?;
    let plan: Plan = feed::strict_json(&protected::read(&directory.join("plan.json"), feed::MAX_FEED as u64)?)?;
    if plan.sessions.iter().any(|s| !sessions.contains(&s.id)) {
        abort(&mut state)?;
        return Err(Error::Busy);
    }
    if !plan.sessions.iter().all(|session| {
        state.registrations.get(&session.id).is_some_and(|r| {
            r.session.pid == session.pid
                && r.session.created == session.created
                && r.quiesced.as_ref() == Some(&transaction)
                && r.seen.elapsed() <= Duration::from_secs(5)
        })
    }) {
        return Ok(());
    }
    if !directory.join("worker-ready.json").exists() {
        return Ok(());
    }
    protected::write(
        &directory.join("drained.json"),
        &serde_json::to_vec(&plan.sessions).map_err(|_| Error::Metadata)?,
    )?;
    state.shutdown = true;
    state.status.phase = Phase::Installing;
    Ok(())
}

fn transaction_matches(state: &State, tx: &str) -> bool {
    if state.transaction.as_deref() == Some(tx) {
        return true;
    }
    protected::maintenance()
        .ok()
        .and_then(|dir| protected::read(&dir.join("pending.json"), 65536).ok())
        .and_then(|bytes| oink_update::transaction::Journal::parse(&bytes).ok())
        .is_some_and(|journal| journal.transaction == tx)
}

#[cfg(test)]
mod tests {
    use super::super::test_catalog as fixture;
    use super::*;
    use serde_json::json;
    const BETA: Channel = Channel::Beta;
    fn receipt() -> msi::Receipt {
        msi::Receipt {
            schema: 1,
            product: "Oink".into(),
            source_tree_sha256: "a".repeat(64),
            version: "0.3.5".into(),
            build: 305001,
            architecture: "x64".into(),
            minimum_os: "10.0.17763".into(),
            minimum_windows_build: 17763,
            updater_included: true,
            collector_included: false,
            protocol_baseline: 1,
            upgrade_code: String::new(),
            components: vec![],
            signature: String::new(),
        }
    }
    fn preferences(checks_enabled: bool, channel: Channel, install_after_download: bool) -> Preferences {
        Preferences {
            checks_enabled,
            update_channel: Some(channel),
            install_after_download,
        }
    }
    fn check_state() -> State {
        let mut policy = MachinePolicy::default();
        policy.policy.preferences = preferences(true, BETA, true);
        State::with(initial_status(receipt(), &policy, true), policy)
    }
    fn offer(state: &mut State, payload: &serde_json::Value, check: Check) -> Option<Selected> {
        let catalog = fixture::verified(payload, BETA).unwrap();
        state
            .apply_catalog(catalog, fixture::envelope(payload), BETA, 26200, check)
            .unwrap()
    }
    #[test]
    fn the_first_source_whose_catalog_verifies_is_used() {
        let public = fixture::public();
        let trust = fixture::trust(&public);
        let sources = ["https://a.example", "https://b.example", "https://c.example"].map(String::from);
        let good = fixture::envelope(&fixture::payload());
        let mut stale = fixture::payload();
        stale["sequence"] = json!(4);
        let stale = fixture::envelope(&stale);
        let fresh = fixture::verified(&fixture::payload(), BETA).unwrap();
        let seen = fresh.watermark().clone();
        let mut asked = vec![];
        let (catalog, bytes) = fetch_catalog(&trust, &sources, BETA, &seen, fixture::NOW, |url| {
            asked.push(url.to_owned());
            match url.split('/').nth(2) {
                Some("a.example") => Err(Error::Network),
                Some("b.example") => Ok(stale.clone()),
                _ => Ok(good.clone()),
            }
        })
        .unwrap();
        assert_eq!(
            asked[0],
            "https://a.example/windows/v2/win-2026-01/x64/beta/manifest.json"
        );
        assert_eq!(asked.len(), 3);
        assert_eq!((catalog.catalog().sequence, bytes), (5, good.clone()));
        // A verification failure is reported in preference to network failures.
        let fetch = |channel, seen: &feed::Watermark, bytes: &Vec<u8>| {
            fetch_catalog(&trust, &sources, channel, seen, fixture::NOW, |url| {
                if url.starts_with("https://b.") {
                    Ok(bytes.clone())
                } else {
                    Err(Error::Network)
                }
            })
            .map(|_| ())
        };
        assert_eq!(fetch(BETA, &seen, &stale), Err(Error::Replay));
        assert_eq!(
            fetch(Channel::Stable, &feed::Watermark::default(), &good),
            Err(Error::Policy)
        );
        assert_eq!(fetch(BETA, &seen, &good), Ok(()));
        let offline = fetch_catalog(&trust, &sources, BETA, &seen, fixture::NOW, |_| Err(Error::Network));
        assert_eq!(offline.map(|_| ()), Err(Error::Network));
    }
    #[test]
    fn catalog_plans_become_candidates_with_their_kind_and_notice() {
        let mut state = check_state();
        let mut p = fixture::payload();
        p["rules"] = json!([
            {"installed":[305000,305999],"kind":"critical","target":306002,"notice":{"en":"Security fix"}},
            {"installed":[0,2147483647],"kind":"normal","target":"latest"}
        ]);
        let selected = offer(&mut state, &p, Check::Automatic).unwrap();
        assert_eq!(
            (selected.release.build, selected.kind, selected.channel),
            (306002, Kind::Critical, BETA)
        );
        let candidate = state.status.candidate.clone().unwrap();
        assert_eq!(
            (
                candidate.build,
                candidate.kind,
                candidate.severity,
                candidate.local_package
            ),
            (306002, Kind::Critical, feed::Severity::Critical, false)
        );
        assert_eq!(candidate.notice.unwrap().for_language("en"), "Security fix");
        assert_eq!(candidate.notes.unwrap().for_language("zh-Hans"), "修复");
        assert_eq!(state.status.phase, Phase::Available);
        assert!(state.status.checked_at.is_some());
        assert_eq!(state.policy.watermarks[&catalog_watermark(BETA)].sequence, 5);
        assert!(!state.policy.watermarks.contains_key("stable"));
        assert_eq!(state.admit_download(), Ok(()));
        // Manual-only updates are shown with their notes but never downloaded.
        p["rules"] = json!([{"installed":[0,2147483647],"kind":"manual","target":"latest"}]);
        assert_eq!(offer(&mut state, &p, Check::Automatic).unwrap().kind, Kind::Manual);
        let candidate = state.status.candidate.clone().unwrap();
        assert_eq!(
            (candidate.build, candidate.kind, candidate.notice),
            (307001, Kind::Manual, None)
        );
        assert_eq!(state.admit_download(), Err(Error::Policy));
        // A skipped normal update is silent for automatic checks only.
        state.policy.policy.skipped_build = Some(307001);
        assert!(offer(&mut state, &fixture::payload(), Check::Automatic).is_none());
        assert_eq!((state.status.phase, &state.status.candidate), (Phase::Idle, &None));
        assert_eq!(state.admit_download(), Err(Error::Policy));
        assert!(offer(&mut state, &fixture::payload(), Check::Manual).is_some());
        // A release this installation is routed to may need a newer Windows.
        let mut p = fixture::payload();
        for release in p["releases"].as_array_mut().unwrap() {
            release["minimumOs"] = json!("10.0.30000");
        }
        assert!(offer(&mut state, &p, Check::Manual).is_none());
        assert_eq!(
            (state.status.phase, state.status.error, &state.status.candidate),
            (Phase::Failed, Some(Error::OsUnsupported), &None)
        );
    }
    #[test]
    fn a_failed_download_stays_attached_to_its_build() {
        let mut state = check_state();
        offer(&mut state, &fixture::payload(), Check::Automatic).unwrap();
        state.download_failed(307001, Error::Network);
        assert_eq!(
            (state.status.phase, state.status.error, state.status.failed_build),
            (Phase::Failed, Some(Error::Network), Some(307001))
        );
        assert_eq!(state.status.candidate.as_ref().map(|c| c.build), Some(307001));
        assert_eq!(state.admit_download(), Ok(()));
        // The same build offered again still carries its failure.
        offer(&mut state, &fixture::payload(), Check::Automatic).unwrap();
        assert_eq!(
            (state.status.phase, state.status.failed_build),
            (Phase::Available, Some(307001))
        );
        state.download_ready(307001);
        assert_eq!((state.status.phase, state.status.failed_build), (Phase::Ready, None));
        state.download_failed(307001, Error::Signature);
        // Another build replaces the failure.
        state.policy.quarantine.insert(307001);
        offer(&mut state, &fixture::payload(), Check::Automatic).unwrap();
        assert_eq!(state.status.candidate.as_ref().map(|c| c.build), Some(306002));
        assert_eq!(state.status.failed_build, None);
        // So does a verification that never finished.
        state.status.phase = Phase::Verifying;
        state.verification_started = Some(Instant::now() - VERIFICATION_BUDGET);
        state.expire_verification(Instant::now());
        assert_eq!(state.status.failed_build, Some(306002));
    }
    #[test]
    fn automatic_checks_run_at_launch_then_daily_or_two_hours_after_a_failure() {
        let now = Instant::now();
        let mut state = check_state();
        // Nothing runs until Oink launches.
        assert_eq!(state.automatic_due(true, false), None);
        assert_eq!(state.next_command(now, true), None);
        state.launched(now);
        assert_eq!(state.automatic_due(true, false), Some(now));
        assert_eq!(state.automatic_due(false, false), None);
        assert_eq!(state.automatic_due(true, true), None);
        assert_eq!(state.next_command(now, true), Some((Command::Check, Check::Automatic)));
        state.checked(true, now);
        assert_eq!(state.next_check, Some(now + CHECK_INTERVAL));
        assert!(state.status.next_check.is_some());
        assert_eq!(
            state.next_command(now + CHECK_INTERVAL - Duration::from_secs(1), true),
            None
        );
        assert_eq!(engine_deadline(&state, now, true, false), Some(now + CHECK_INTERVAL));
        assert!(state.next_command(now + CHECK_INTERVAL, true).is_some());
        state.checked(false, now);
        assert_eq!(engine_deadline(&state, now, true, false), Some(now + RETRY_INTERVAL));
        // Another launch checks again at once.
        state.launched(now);
        assert_eq!(
            engine_deadline(&state, now + Duration::from_secs(5), true, false),
            Some(now + Duration::from_secs(5))
        );
        // A package downloading or ready to install is not replaced.
        for phase in [Phase::Downloading, Phase::Verifying, Phase::Ready, Phase::Quiescing] {
            state.status.phase = phase;
            assert_eq!(state.automatic_due(true, false), None, "{phase:?}");
            assert_eq!(state.next_command(now, true), None);
        }
        // With automatic checks off a launch checks nothing; manual checks run.
        let mut off = check_state();
        off.policy.policy.preferences.checks_enabled = false;
        off.launched(now);
        assert_eq!(off.automatic_due(true, false), None);
        off.request_check(Check::Manual);
        assert_eq!(off.next_command(now, true), Some((Command::Check, Check::Manual)));
        off.checked(true, now);
        assert_eq!(off.status.next_check, None);
        // A disabled feed still runs an explicit local install.
        off.queued = Some(Command::Install { expected_build: 44 });
        assert_eq!(
            off.next_command(now, false),
            Some((Command::Install { expected_build: 44 }, Check::Automatic))
        );
    }
    #[test]
    fn manual_checks_join_automatic_ones_or_replace_them_to_show_a_skipped_build() {
        let now = Instant::now();
        let mut state = check_state();
        state.status.error = Some(Error::Network);
        state.request_check(Check::Automatic);
        assert_eq!((state.status.phase, state.status.error), (Phase::Checking, None));
        let automatic = state.check_revision;
        state.request_check(Check::Automatic);
        assert_eq!(state.check_revision, automatic);
        state.request_check(Check::Manual);
        let manual = state.check_revision;
        assert_ne!(manual, automatic);
        for check in [Check::Automatic, Check::Manual] {
            state.request_check(check);
            assert_eq!(state.check_revision, manual);
        }
        assert_eq!(state.next_command(now, true), Some((Command::Check, Check::Manual)));
        // The engine is running the manual check: later requests share it.
        state.running_check = Some(Check::Manual);
        state.request_check(Check::Manual);
        assert_eq!((state.check_revision, &state.queued), (manual, &None));
        state.running_check = Some(Check::Automatic);
        state.request_check(Check::Automatic);
        assert_eq!((state.check_revision, &state.queued), (manual, &None));
        state.request_check(Check::Manual);
        assert_ne!(state.check_revision, manual);
        assert_eq!(state.queued, Some(Command::Check));
        // Checks never invalidate a download or preparation.
        assert_eq!(state.revision, 0);
    }
    #[test]
    fn the_user_changes_preferences_and_a_new_channel_checks_again() {
        let mut state = check_state();
        assert!(state.status.can_change_policy);
        offer(&mut state, &fixture::payload(), Check::Automatic).unwrap();
        state.download_failed(307001, Error::Network);
        // The update dialog's checkbox keeps the offer and any download.
        state.apply_preferences(preferences(true, BETA, false));
        assert!(!state.status.install_after_download);
        assert_eq!(state.status.failed_build, Some(307001));
        assert!(state.status.candidate.is_some());
        assert_eq!((state.revision, &state.queued), (0, &None));
        state.apply_preferences(preferences(true, Channel::Stable, false));
        assert_eq!(
            (state.status.channel, state.status.update_channel),
            (Channel::Stable, Some(Channel::Stable))
        );
        assert_eq!((&state.status.candidate, state.status.failed_build), (&None, None));
        assert_eq!(
            (state.status.phase, &state.queued),
            (Phase::Checking, &Some(Command::Check))
        );
        assert_ne!(state.revision, 0);
        // A running check of the old channel no longer counts.
        let revision = state.check_revision;
        state.running_check = Some(Check::Automatic);
        state.queued = None;
        state.apply_preferences(preferences(true, BETA, false));
        assert_ne!(state.check_revision, revision);
        // Back on stable, a beta installation is not downgraded.
        let mut stable = fixture::payload();
        stable["channel"] = json!("stable");
        stable["releases"].as_array_mut().unwrap().pop();
        let catalog = fixture::verified(&stable, Channel::Stable).unwrap();
        state.status.current_build = 307001;
        state.status.version = "0.3.7".into();
        let selected = state
            .apply_catalog(catalog, vec![], Channel::Stable, 26200, Check::Manual)
            .unwrap();
        assert!(selected.is_none());
        assert_eq!(state.status.phase, Phase::Idle);
        // Turning automatic checks on checks once.
        let mut off = check_state();
        off.apply_preferences(preferences(false, BETA, true));
        assert_eq!((off.status.checks_enabled, &off.queued), (false, &None));
        off.apply_preferences(preferences(true, BETA, true));
        assert_eq!((off.status.checks_enabled, &off.queued), (true, &Some(Command::Check)));
    }
    #[test]
    fn stalled_verification_fails_and_late_preparation_cannot_install() {
        let mut state = check_state();
        let start = Instant::now();
        state.status.phase = Phase::Verifying;
        state.verification_started = Some(start);
        let revision = state.revision;
        state.expire_verification(start + VERIFICATION_BUDGET - Duration::from_millis(1));
        assert_eq!(state.status.phase, Phase::Verifying);
        state.expire_verification(start + VERIFICATION_BUDGET);
        assert_eq!(state.status.phase, Phase::Failed);
        assert_eq!(state.status.error, Some(Error::VerificationTimeout));
        assert!(state.queued.is_none());
        assert_eq!(verification_current(&state, revision), Err(Error::VerificationTimeout));
    }
    #[test]
    fn cancellation_invalidates_a_prepared_local_package() {
        let mut state = check_state();
        state.status.phase = Phase::Verifying;
        let revision = state.revision;
        state.revision += 1;
        assert_eq!(verification_current(&state, revision), Err(Error::Busy));
    }
    #[test]
    fn idle_engine_has_no_poll_but_real_business_deadlines_remain() {
        let now = Instant::now();
        let mut state = check_state();
        assert_eq!(engine_deadline(&state, now, true, false), None);
        state.next_check = Some(now + CHECK_INTERVAL);
        assert_eq!(engine_deadline(&state, now, false, false), None);
        state.transaction = Some("private-test".into());
        state.prepared_at = Some(now);
        assert_eq!(
            engine_deadline(&state, now, true, true),
            Some(now + Duration::from_secs(20))
        );
        state.shutdown = true;
        assert_eq!(engine_deadline(&state, now, true, true), None);
        state.transaction = None;
        state.status.phase = Phase::Verifying;
        state.verification_started = Some(now);
        assert_eq!(
            engine_deadline(&state, now, true, false),
            Some(now + VERIFICATION_BUDGET)
        );
    }
    #[test]
    fn local_package_admission_requires_elevation_and_no_live_transaction() {
        for phase in [
            Phase::Idle,
            Phase::Ready,
            Phase::Downloading,
            Phase::Verifying,
            Phase::Quiescing,
            Phase::Installing,
        ] {
            assert!(!local_admitted(false, false, false, phase));
            assert!(!local_admitted(true, true, false, phase));
            assert!(!local_admitted(true, false, true, phase));
        }
        assert!(local_admitted(true, false, false, Phase::Idle));
        assert!(!local_admitted(true, false, false, Phase::Downloading));
        assert!(!local_admitted(true, false, false, Phase::Verifying));
        assert!(!local_admitted(true, false, false, Phase::Quiescing));
        assert!(!local_admitted(true, false, false, Phase::Installing));
    }
    #[test]
    fn sessions_without_oink_and_exited_registrations_do_not_block_install() {
        let registrations = [2, 3, 4]
            .into_iter()
            .map(|id| {
                (
                    id,
                    Registration {
                        session: Session {
                            id,
                            pid: id * 10,
                            created: 1,
                            sid: "test".into(),
                            build: 1,
                        },
                        seen: Instant::now() - Duration::from_secs(30),
                        quiesced: None,
                    },
                )
            })
            .collect();
        let mut queried = Vec::new();
        let selected = registered_sessions(&registrations, &[1, 2, 3], |session| {
            queried.push(session.id);
            Ok(session.id == 2)
        })
        .unwrap();
        assert_eq!(selected.iter().map(|s| s.id).collect::<Vec<_>>(), [2]);
        assert_eq!(queried, [2, 3]);
    }
    #[test]
    fn a_failed_attempt_keeps_a_durable_clear_result() {
        let mut journal = oink_update::transaction::Journal::new("a".repeat(32), 1, 2, "b".repeat(64)).unwrap();
        journal.stage = oink_update::transaction::Stage::RolledBack;
        journal.failure = Some(Error::Busy);
        assert_eq!(transaction_phase(&journal), Phase::Failed);
        journal.rollback_attempted = true;
        assert_eq!(transaction_phase(&journal), Phase::RolledBack);
        assert_eq!(journal.failure, Some(Error::Busy));
    }
    #[test]
    fn policy_changes_cancel_prepare_but_cannot_interrupt_handover() {
        for command in [
            Command::Cancel,
            Command::SetPreferences {
                preferences: Preferences::default(),
            },
            Command::Defer {
                choice: DeferChoice::Hour,
            },
            Command::Skip { expected_build: 2 },
            Command::Prompted { expected_build: 2 },
        ] {
            assert!(command_admitted(&command, true, true));
            assert!(!command_admitted(&command, false, true));
        }
        assert!(!command_admitted(&Command::Install { expected_build: 2 }, true, true));
        assert!(command_admitted(&Command::Check, false, false));
    }
}

#[cfg(all(test, feature = "test-feed"))]
#[test]
fn loopback_fetch_verifies_the_signed_map_and_selects_ui_language() {
    use std::io::{Read, Write};
    let body = include_bytes!("notes-loopback-test.json");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut request = [0u8; 4096];
        let received = stream.read(&mut request).unwrap();
        assert!(received > 0);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
    });
    let bytes = fetch::get(
        &format!("http://{address}/windows/v1/win-2026-01/stable/manifest.json"),
        feed::MAX_FEED as u64,
        false,
        &mut |_, _| {},
    )
    .unwrap();
    server.join().unwrap();
    let key = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=",
    )
    .unwrap();
    let key: [u8; 32] = key.try_into().unwrap();
    let manifest = feed::verify_manifest(
        &bytes,
        &feed::Trust {
            epoch: oink_update::WINDOWS_EPOCH,
            key: &key,
        },
        &feed::Watermark::default(),
        feed::timestamp("2026-10-20T00:00:00Z").unwrap(),
    )
    .unwrap();
    let quarantine = Default::default();
    let release = manifest
        .select(&feed::Selection {
            windows_build: 22000,
            architecture: "x64",
            installed_build: 32,
            successful_build: 32,
            quarantine: &quarantine,
        })
        .unwrap()
        .unwrap();
    for (language, expected) in [
        ("ja-JP", "テスト日本語"),
        ("ko-KR", "테스트 한국어"),
        ("es-MX", "Prueba"),
        ("de-DE", "TEST English"),
    ] {
        assert_eq!(release.notes.for_language(language), expected);
        eprintln!("loopback Check {language}: {expected}");
    }
}
