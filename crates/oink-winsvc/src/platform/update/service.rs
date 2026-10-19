use super::super::{Handle, security, token};
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use oink_update::{
    barrier::Blocker,
    feed::{self, Selection, VerifiedManifest},
    ipc::{Reply, Request},
    trust::Roots,
};
use std::{
    io::Read,
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};
use windows_sys::Win32::System::Threading::GetProcessId;
#[derive(Clone)]
struct Registration {
    session: Session,
    seen: Instant,
    consent: Option<u64>,
    quiesced: Option<String>,
}
pub(super) struct State {
    status: UpdateStatus,
    policy: MachinePolicy,
    queued: Option<Command>,
    registrations: BTreeMap<u32, Registration>,
    transaction: Option<String>,
    automatic: bool,
    shutdown: bool,
    prepared_at: Option<Instant>,
    last_check: Option<Instant>,
    revision: u64,
}
impl State {
    fn new() -> Result<Self, Error> {
        let receipt = installed()?;
        let policy = policy()?;
        let configured = oink_update::trust::compiled().is_ok();
        let status = UpdateStatus {
            current_build: receipt.build,
            version: receipt.version,
            channel: policy.policy.preferences.channel,
            mode: policy.policy.preferences.mode,
            checks_enabled: configured && policy.policy.preferences.checks_enabled,
            phase: if configured { Phase::Idle } else { Phase::Disabled },
            candidate: None,
            checked_at: None,
            next_check: None,
            deferred_until: policy.policy.deferred_until,
            blockers: Vec::new(),
            progress: None,
            error: (!configured).then_some(Error::NotConfigured),
            scope: Scope::Machine,
            can_change_policy: false,
        };
        Ok(Self {
            status,
            policy,
            queued: None,
            registrations: BTreeMap::new(),
            transaction: None,
            automatic: true,
            shutdown: false,
            prepared_at: None,
            last_check: None,
            revision: 0,
        })
    }
}
fn transaction_phase(journal: &oink_update::transaction::Journal) -> Phase {
    use oink_update::transaction::Stage;
    match journal.stage {
        Stage::Prepared => Phase::Quiescing,
        Stage::AwaitingReboot | Stage::RollbackAwaitingReboot => Phase::AwaitingReboot,
        Stage::InstallerBusy | Stage::RollbackInstallerBusy => Phase::InstallerBusy,
        Stage::Committed => Phase::Committed,
        Stage::RolledBack if journal.rollback_attempted => Phase::RolledBack,
        Stage::RolledBack => Phase::Idle,
        Stage::RepairRequired => Phase::RepairRequired,
        _ => Phase::Installing,
    }
}
fn journal_status(name: &str) -> Result<Phase, Error> {
    let journal = oink_update::transaction::Journal::parse(&protected::read(
        &protected::maintenance().map_err(|_| Error::Storage)?.join(name),
        65536,
    )?)?;
    Ok(transaction_phase(&journal))
}
pub(super) fn dispatch(state: &Arc<Mutex<State>>, id: u32, process: &Handle, request: Request) -> Reply {
    let mut state = state.lock().unwrap();
    if pending() {
        state.status.phase = journal_status("pending.json").unwrap_or(Phase::RepairRequired);
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
                        consent: None,
                        quiesced: None,
                    },
                );
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
                        automatic: state.automatic,
                    });
                }
                if pending() {
                    let journal = oink_update::transaction::Journal::parse(&protected::read(
                        &protected::maintenance()
                            .map_err(|_| Error::Storage)?
                            .join("pending.json"),
                        65536,
                    )?)?;
                    if matches!(
                        journal.stage,
                        oink_update::transaction::Stage::Recovering
                            | oink_update::transaction::Stage::RollbackInstallerBusy
                    ) {
                        return if state.registrations[&id].quiesced.as_ref() == Some(&journal.transaction) {
                            Ok(Reply::Shutdown {
                                transaction: journal.transaction,
                            })
                        } else {
                            Ok(Reply::Prepare {
                                transaction: journal.transaction,
                                automatic: false,
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
                oink_update::trust::compiled()?;
                state.revision = state.revision.wrapping_add(1);
                let preparing = state.transaction.is_some() && !state.shutdown;
                if !command_admitted(&command, preparing, state.transaction.is_some() || pending()) {
                    return Err(Error::Busy);
                }
                match command {
                    Command::SetPreferences { preferences } => {
                        if !super::pipe::elevated(process) {
                            return Err(Error::Policy);
                        }
                        if preparing {
                            abort(&mut state)?;
                        }
                        let old = state.policy.clone();
                        state.policy.policy.preferences = preferences.clone();
                        if let Err(e) = save_policy(&state.policy) {
                            state.policy = old;
                            return Err(e);
                        }
                        state.status.channel = preferences.channel;
                        state.status.mode = preferences.mode;
                        state.status.checks_enabled = preferences.checks_enabled;
                        state.status.candidate = None;
                        state.queued = preferences.checks_enabled.then_some(Command::Check);
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
                        if state.status.candidate.as_ref().map(|c| c.build) != Some(expected_build) {
                            return Err(Error::Policy);
                        }
                        if preparing {
                            abort(&mut state)?;
                        }
                        state.policy.policy.skipped_build = Some(expected_build);
                        save_policy(&state.policy)?;
                    }
                    Command::Install { expected_build } => {
                        if state.status.candidate.as_ref().map(|c| c.build) != Some(expected_build) {
                            return Err(Error::Policy);
                        }
                        // Each registered user explicitly requests manual install or
                        // receives machine automatic policy; a different user cannot consent for them.
                        if let Some(registration) = state.registrations.get_mut(&id) {
                            registration.consent = Some(expected_build);
                        }
                        state.queued = Some(Command::Install { expected_build });
                    }
                    Command::Check => {
                        if state.last_check.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
                            return Err(Error::Busy);
                        }
                        state.queued = Some(Command::Check);
                    }
                    Command::Download => state.queued = Some(Command::Download),
                    Command::Cancel => {
                        abort(&mut state)?;
                        state.queued = Some(Command::Cancel);
                    }
                }
            }
        }
        let mut status = state.status.clone();
        status.can_change_policy = super::pipe::elevated(process);
        Ok(Reply::Status { status })
    })();
    result.unwrap_or_else(|error| Reply::Error { error })
}
fn abort(state: &mut State) -> Result<(), Error> {
    if state.shutdown {
        return Err(Error::Transaction);
    }
    if let Some(transaction) = state.transaction.take() {
        protected::write(&transaction_dir(&transaction)?.join("abort.json"), b"{\"schema\":1}")?;
    }
    state.prepared_at = None;
    state.status.phase = Phase::WaitingForIdle;
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
                Command::SetPreferences { .. } | Command::Defer { .. } | Command::Skip { .. } | Command::Cancel
            ))
}
pub(super) fn serve() {
    if protected::system().is_err() {
        return;
    }
    let Ok(initial) = State::new() else {
        return;
    };
    let state = Arc::new(Mutex::new(initial));
    let engine_state = state.clone();
    let engine = std::thread::spawn(move || engine(engine_state));
    let mut workers = BTreeMap::<u32, std::thread::JoinHandle<()>>::new();
    while !super::super::broker::STOP.load(Ordering::Acquire) {
        if let Ok(sessions) = super::pipe::sessions() {
            for id in sessions {
                if workers.get(&id).is_some_and(|w| !w.is_finished()) {
                    continue;
                }
                let state = state.clone();
                workers.insert(
                    id,
                    std::thread::spawn(move || {
                        let _ = super::pipe::serve(id, state);
                    }),
                );
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    for (_, worker) in workers {
        let _ = worker.join();
    }
    let _ = engine.join();
}
pub(super) fn http(url: &str, length: u64, package: bool) -> Result<Vec<u8>, Error> {
    let policy = if package {
        reqwest::redirect::Policy::custom(|attempt| {
            if feed::validate_redirect(attempt.url().as_str(), attempt.previous().len()).is_ok() {
                attempt.follow()
            } else {
                attempt.stop()
            }
        })
    } else {
        reqwest::redirect::Policy::none()
    };
    let client = reqwest::blocking::Client::builder()
        .redirect(policy)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .user_agent("Oink-updater/1")
        .build()
        .map_err(|_| Error::Network)?;
    let response = client.get(url).send().map_err(|_| Error::Network)?;
    if response.status() != reqwest::StatusCode::OK || response.content_length().is_some_and(|n| n > length) {
        return Err(Error::Network);
    }
    let mut bytes = Vec::new();
    response
        .take(length + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Network)?;
    if bytes.len() as u64 > length {
        return Err(Error::Limit);
    }
    Ok(bytes)
}
struct Selected {
    manifest: VerifiedManifest,
    envelope: Vec<u8>,
    release: Release,
    verified_package: bool,
}
fn check_feed(state: &Arc<Mutex<State>>, roots: &Roots) -> Result<Option<Selected>, Error> {
    let (policy, installed, revision) = {
        let mut state = state.lock().unwrap();
        state.status.phase = Phase::Checking;
        state.last_check = Some(Instant::now());
        (state.policy.clone(), state.status.current_build, state.revision)
    };
    let channel = policy.policy.preferences.channel;
    let envelope = http(
        &format!(
            "https://updates.oink.hachiwii.com/windows/v1/{}/{}/manifest.json",
            oink_update::WINDOWS_EPOCH,
            channel.name()
        ),
        feed::MAX_FEED as u64,
        false,
    )?;
    let seen = policy.watermarks.get(channel.name()).cloned().unwrap_or_default();
    let manifest = feed::verify_manifest(&envelope, &roots.feed(), channel, &seen, now())?;
    let os_build = worker::windows_build()?;
    let release = manifest
        .select(&Selection {
            channel,
            windows_build: os_build,
            architecture: "x64",
            installed_build: installed,
            successful_build: policy.successful_build,
            quarantine: &policy.quarantine,
        })?
        .cloned();
    let mut state = state.lock().unwrap();
    if state.revision != revision {
        return Err(Error::Busy);
    }
    state
        .policy
        .watermarks
        .insert(channel.name().into(), manifest.watermark().clone());
    save_policy(&state.policy)?;
    state.status.checked_at = Some(now());
    state.status.next_check = Some(now() + 24 * 3600);
    state.status.error = if release.is_none()
        && manifest.manifest().releases.iter().any(|r| {
            r.build > installed.max(policy.successful_build)
                && r.architecture == "x64"
                && r.minimum_windows_build > os_build
        }) {
        Some(Error::OsUnsupported)
    } else {
        None
    };
    state.status.candidate = release.as_ref().map(|release| Candidate {
        build: release.build,
        version: release.version.clone(),
        size: release.package.length,
        published_at: None,
        severity: release.severity,
        notes: release.notes.clone(),
    });
    state.status.phase = if release.is_some() {
        Phase::Available
    } else if state.status.error.is_some() {
        Phase::Failed
    } else {
        Phase::Idle
    };
    Ok(release.map(|release| Selected {
        manifest,
        envelope,
        release,
        verified_package: false,
    }))
}
fn download(selected: &mut Selected, roots: &Roots) -> Result<(), Error> {
    selected.manifest.ensure_fresh(now())?;
    let package = &selected.release.package;
    let bytes = http(&package.url, package.length, true)?;
    feed::verify_package(&bytes, package, &roots.feed())?;
    let directory = protected::maintenance().map_err(|_| Error::Storage)?.join("downloads");
    protected::directory(&directory).map_err(|_| Error::Storage)?;
    protected::write(&directory.join("candidate.msi"), &bytes)?;
    selected.verified_package = true;
    Ok(())
}
fn engine(state: Arc<Mutex<State>>) {
    let roots = match oink_update::trust::compiled() {
        Ok(roots) => roots,
        Err(error) => {
            let mut state = state.lock().unwrap();
            state.status.error = Some(error);
            state.status.phase = Phase::Disabled;
            state.status.checks_enabled = false;
            return;
        }
    };
    let mut selected: Option<Selected> = None;
    let mut was_pending = pending();
    while !super::super::broker::STOP.load(Ordering::Acquire) {
        let is_pending = pending();
        if is_pending {
            let _ = worker::resume_pending();
        }
        if was_pending && !is_pending {
            let mut state = state.lock().unwrap();
            if let Ok(policy) = policy() {
                state.policy = policy;
            }
            state.transaction = None;
            state.shutdown = false;
            state.status.phase = journal_status("last-result.json").unwrap_or(Phase::RepairRequired);
            state.status.candidate = None;
            state.status.error = None;
            selected = None;
        }
        was_pending = is_pending;
        let command = {
            let mut state = state.lock().unwrap();
            if state.transaction.is_some() || pending() {
                None
            } else {
                let automatic_check = state.policy.policy.preferences.checks_enabled
                    && state
                        .last_check
                        .is_none_or(|at| at.elapsed() >= Duration::from_secs(24 * 3600));
                state
                    .queued
                    .take()
                    .or_else(|| automatic_check.then_some(Command::Check))
                    .map(|command| (command, state.revision))
            }
        };
        let result = (|| {
            if let Some((command, revision)) = command {
                match command {
                    Command::Check => {
                        selected = None;
                        selected = check_feed(&state, &roots)?;
                    }
                    Command::Download | Command::Install { .. } => {
                        let candidate = selected.as_mut().ok_or(Error::Policy)?;
                        if !candidate.verified_package {
                            state.lock().unwrap().status.phase = Phase::Downloading;
                            download(candidate, &roots)?;
                        }
                        state.lock().unwrap().status.phase = Phase::Available;
                        if matches!(command, Command::Install { .. }) {
                            prepare(&state, candidate, &roots, false, revision)?;
                        }
                    }
                    Command::Cancel => {
                        selected = None;
                    }
                    _ => {}
                }
            }
            if let Some(candidate) = selected.as_mut() {
                let automatic_revision = {
                    let state = state.lock().unwrap();
                    (state.transaction.is_none()
                        && !pending()
                        && state.policy.policy.preferences.channel == candidate.manifest.manifest().channel
                        && state
                            .policy
                            .policy
                            .may_download_automatically(candidate.release.build, now()))
                    .then_some(state.revision)
                };
                if let Some(revision) = automatic_revision {
                    if !candidate.verified_package {
                        download(candidate, &roots)?;
                    }
                    prepare(&state, candidate, &roots, true, revision)?;
                }
            }
            handover(&state)
        })();
        if let Err(error) = result {
            let mut state = state.lock().unwrap();
            state.status.error = Some(error);
            state.status.phase = if error == Error::Busy {
                Phase::WaitingForIdle
            } else {
                Phase::Failed
            };
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
fn prepare(
    state: &Arc<Mutex<State>>,
    selected: &Selected,
    roots: &Roots,
    automatic: bool,
    expected_revision: u64,
) -> Result<(), Error> {
    selected.manifest.ensure_fresh(now())?;
    let sessions = super::pipe::sessions().map_err(|_| Error::Busy)?;
    let (registrations, revision) = {
        let mut state = state.lock().unwrap();
        if state.transaction.is_some() || pending() || state.revision != expected_revision {
            return Err(Error::Busy);
        }
        if sessions.iter().any(|id| {
            state
                .registrations
                .get(id)
                .is_none_or(|r| r.seen.elapsed() > Duration::from_secs(5))
        }) {
            state.status.blockers = vec![Blocker::OwnerMissing];
            return Err(Error::Busy);
        }
        if !automatic
            && sessions
                .iter()
                .any(|id| state.registrations[id].consent != Some(selected.release.build))
        {
            state.status.blockers = vec![Blocker::Consent];
            return Err(Error::Busy);
        }
        (
            sessions
                .iter()
                .map(|id| state.registrations[id].session.clone())
                .collect::<Vec<_>>(),
            state.revision,
        )
    };
    let previous_build = state.lock().unwrap().status.current_build;
    let previous = selected
        .manifest
        .manifest()
        .releases
        .iter()
        .find(|r| r.build == previous_build)
        .cloned();
    let transaction = worker::stage(
        selected.release.clone(),
        previous,
        &selected.envelope,
        registrations,
        roots,
        automatic,
    )?;
    let mut state = state.lock().unwrap();
    if state.revision != revision {
        protected::write(&transaction_dir(&transaction)?.join("abort.json"), b"{\"schema\":1}")?;
        return Err(Error::Busy);
    }
    state.transaction = Some(transaction);
    state.automatic = automatic;
    state.prepared_at = Some(Instant::now());
    state.status.phase = Phase::Quiescing;
    state.status.blockers.clear();
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
        .is_none_or(|at| at.elapsed() > Duration::from_secs(20))
    {
        abort(&mut state)?;
        return Err(Error::Busy);
    }
    let directory = transaction_dir(&transaction)?;
    let sessions = super::pipe::sessions().map_err(|_| Error::Busy)?;
    let plan: Plan = feed::strict_json(&protected::read(&directory.join("plan.json"), feed::MAX_FEED as u64)?)?;
    if sessions != plan.sessions.iter().map(|s| s.id).collect::<Vec<_>>() {
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
    use super::*;
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
        ] {
            assert!(command_admitted(&command, true, true));
            assert!(!command_admitted(&command, false, true));
        }
        assert!(!command_admitted(&Command::Install { expected_build: 2 }, true, true));
        assert!(command_admitted(&Command::Check, false, false));
    }
}
