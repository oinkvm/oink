use super::*;
use oink_update::ipc::{Reply, Request};
pub(super) fn status() -> ApiResult<UpdateStatus> {
    match oink_winsvc::update::request(Request::Query).map_err(api_error)? {
        Reply::Status { status } => Ok(*status),
        Reply::Error { error } => Err(api_error(error)),
        _ => Err(api_error(Error::Transaction)),
    }
}
pub(super) fn command(node: &Node, command: Command) -> ApiResult<UpdateStatus> {
    match oink_winsvc::update::request(Request::Command { command }).map_err(api_error)? {
        Reply::Status { status } => {
            let status = *status;
            let mut state = lock(&node.inner.update);
            state.status = status.clone();
            if let Some(wake) = &state.wake {
                let _ = wake.try_send(());
            }
            Ok(status)
        }
        Reply::Error { error } => Err(api_error(error)),
        _ => Err(api_error(Error::Transaction)),
    }
}
pub(super) fn attach(node: &Node) {
    let (wake, receive) = std::sync::mpsc::sync_channel(1);
    lock(&node.inner.update).wake = Some(wake);
    let startup = STARTUP.lock().unwrap().take();
    let mut initial = startup.map(|transaction| {
        (
            transaction,
            Prepared {
                node: node.clone(),
                gate: maintenance::process().clone(),
                generation: BOOT_GENERATION,
                expires: Instant::now() + Duration::from_secs(600),
                consumed: false,
            },
        )
    });
    let weak = std::sync::Arc::downgrade(&node.inner);
    std::thread::spawn(move || {
        let mut registered = false;
        let mut health_mode = initial.is_some();
        let mut health_reported = None;
        let mut prepared = initial.take();
        let mut delay = Duration::from_millis(500);
        loop {
            let _ = receive.recv_timeout(delay);
            let Some(inner) = weak.upgrade() else {
                break;
            };
            let node = Node { inner };
            delay = polling_delay(health_mode, prepared.is_some(), lock(&node.inner.update).status.phase);
            if !registered {
                if status().is_err() {
                    continue;
                }
                registered = matches!(
                    oink_winsvc::update::request(Request::Register {
                        build: env!("OINK_RELEASE_BUILD").parse().unwrap_or(0)
                    }),
                    Ok(Reply::Registered)
                );
                if !registered {
                    continue;
                }
            }
            if !health_mode
                && let Some((transaction, token)) = &prepared
                && let Err(blockers) = token.validate()
            {
                let _ = oink_winsvc::update::request(Request::Busy {
                    transaction: transaction.clone(),
                    blockers,
                });
                prepared = None;
            }
            let request = Request::Poll {
                transaction: prepared.as_ref().map(|(tx, _)| tx.clone()),
            };
            match oink_winsvc::update::request(request) {
                Ok(Reply::Prepare { transaction }) => {
                    if prepared.as_ref().is_none_or(|(tx, _)| *tx != transaction) {
                        prepared = None;
                        match node.prepare_for_update() {
                            Ok(token) => prepared = Some((transaction.clone(), token)),
                            Err(blockers) => {
                                let _ = oink_winsvc::update::request(Request::Busy { transaction, blockers });
                                continue;
                            }
                        }
                    }
                    let _ = oink_winsvc::update::request(Request::Quiesced { transaction });
                }
                Ok(Reply::Shutdown { transaction }) => {
                    if let Some((tx, token)) = prepared.take()
                        && tx == transaction
                    {
                        if health_mode && token.validate().is_err() {
                            prepared = Some((tx, token));
                            continue;
                        }
                        match token.shutdown_with_ui(true) {
                            Ok(()) => break,
                            Err(blockers) => {
                                let _ = oink_winsvc::update::request(Request::Busy { transaction, blockers });
                            }
                        }
                    }
                }
                Ok(Reply::Abort { .. }) => {
                    prepared = None;
                }
                Ok(Reply::HealthCheck { transaction, build }) => {
                    if env!("OINK_RELEASE_BUILD").parse::<u64>().ok() != Some(build) {
                        continue;
                    }
                    if prepared.is_none() {
                        match node.prepare_for_update() {
                            Ok(mut token) => {
                                token.hold_for_health();
                                prepared = Some((transaction.clone(), token));
                                health_mode = true;
                            }
                            Err(_) => continue,
                        }
                    }
                    if health_report_due(health_reported.as_ref(), &transaction, std::time::Instant::now())
                        && let Ok(status) = node.input_status()
                    {
                        let ready = oink_update::trust::health_ready(status.helper_priority.is_some())
                            && status.focus_info.is_some();
                        let reply = oink_winsvc::update::request(Request::Health {
                            transaction: transaction.clone(),
                            build,
                            input_ready: oink_update::trust::health_ready(status.helper_priority.is_some()),
                            routing_ready: status.focus_info.is_some(),
                        });
                        if ready && matches!(reply, Ok(Reply::Registered)) {
                            health_reported = Some((transaction, std::time::Instant::now()));
                        }
                    }
                }
                Ok(Reply::Status { status }) => {
                    let status = *status;
                    if health_mode {
                        prepared = None;
                        health_mode = false;
                        health_reported = None;
                    }
                    let changed = lock(&node.inner.update).status != status;
                    if changed {
                        let mut current = lock(&node.inner.update);
                        if current.local_mode {
                            continue;
                        }
                        current.status = status.clone();
                        drop(current);
                        tracing::info!(phase=?status.phase,error=?status.error,"Windows update service status changed");
                        node.emit(ApiEvent::UpdateStatusChanged(status));
                    }
                }
                Ok(Reply::Error { .. }) | Err(_) => {
                    registered = false;
                    // A service restart or transient pipe failure cannot reopen
                    // business admission while candidate health is uncommitted.
                    if !health_mode {
                        prepared = None;
                    }
                }
                _ => {}
            }
        }
    });
}

pub(super) fn user_quit() -> bool {
    // A development build with no configured service can still quit normally.
    match oink_winsvc::update::request(Request::UserQuit) {
        Ok(Reply::Error { .. }) => false,
        Ok(_) | Err(Error::NotConfigured) => true,
        Err(_) => false,
    }
}

const BOOT_GENERATION: u64 = u64::MAX;
static STARTUP: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
pub(crate) fn startup_gate() -> Result<(), Error> {
    match oink_winsvc::update::request(Request::Startup) {
        Ok(Reply::HealthCheck { transaction, build }) => {
            if env!("OINK_RELEASE_BUILD").parse::<u64>().ok() != Some(build) {
                return Err(Error::Policy);
            }
            maintenance::process()
                .close_for_update(BOOT_GENERATION)
                .map_err(|_| Error::Busy)?;
            *STARTUP.lock().unwrap() = Some(transaction);
            Ok(())
        }
        Ok(Reply::Error { error }) => Err(error),
        Ok(Reply::Status { .. }) => Ok(()),
        Err(Error::NotConfigured) if env!("OINK_RELEASE_BUILD") == "0" => Ok(()),
        _ => Err(Error::Transaction),
    }
}

// Released workers require every session proof to be at most three seconds
// old. Refresh once a second until commit, so one slow session cannot expire
// another session's successful proof. The old 50 ms preparation poll generated
// a record-write/wake feedback loop; this is a bounded health heartbeat only.
fn health_report_due(last: Option<&(String, std::time::Instant)>, transaction: &str, now: std::time::Instant) -> bool {
    last.is_none_or(|(tx, at)| tx != transaction || now.saturating_duration_since(*at) >= Duration::from_secs(1))
}
// Health proofs are acknowledged state, not a 50 ms preparation fence. Keeping
// the preparation cadence after a successful proof caused a record-write/wake
// feedback loop in the new service during an old worker's commit (Win10 22H2).
fn polling_delay(health_mode: bool, prepared: bool, phase: Phase) -> Duration {
    if !health_mode && (prepared || phase == Phase::Quiescing) {
        Duration::from_millis(50)
    } else {
        Duration::from_millis(500)
    }
}
#[cfg(test)]
mod health_poll_tests {
    use super::*;
    #[test]
    fn a_delayed_second_session_does_not_expire_the_first_sessions_proof() {
        let now = std::time::Instant::now();
        let proof = ("transaction".into(), now);
        assert!(health_report_due(None, "transaction", now));
        assert!(!health_report_due(
            Some(&proof),
            "transaction",
            now + Duration::from_millis(999)
        ));
        assert!(health_report_due(
            Some(&proof),
            "transaction",
            now + Duration::from_secs(1)
        ));
        assert!(health_report_due(
            Some(&proof),
            "transaction",
            now + Duration::from_secs(10)
        ));
        assert!(health_report_due(Some(&proof), "another", now));
    }
    #[test]
    fn candidate_health_does_not_reenter_the_fast_preparation_poll() {
        for phase in [Phase::Installing, Phase::Quiescing, Phase::Committed] {
            assert_eq!(polling_delay(true, true, phase), Duration::from_millis(500));
        }
        assert_eq!(polling_delay(false, true, Phase::Quiescing), Duration::from_millis(50));
        assert_eq!(polling_delay(false, false, Phase::Idle), Duration::from_millis(500));
    }
}
