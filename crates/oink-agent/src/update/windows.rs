use super::*;
use oink_update::ipc::{Reply, Request};
pub(super) fn status() -> ApiResult<UpdateStatus> {
    match oink_winsvc::update::request(Request::Query).map_err(api_error)? {
        Reply::Status { status } => Ok(status),
        Reply::Error { error } => Err(api_error(error)),
        _ => Err(api_error(Error::Transaction)),
    }
}
pub(super) fn command(command: Command) -> ApiResult<UpdateStatus> {
    match oink_winsvc::update::request(Request::Command { command }).map_err(api_error)? {
        Reply::Status { status } => Ok(status),
        Reply::Error { error } => Err(api_error(error)),
        _ => Err(api_error(Error::Transaction)),
    }
}
pub(super) fn attach(node: &Node) {
    let startup = STARTUP.lock().unwrap().take();
    let mut initial = startup.map(|transaction| {
        (
            transaction,
            Prepared {
                node: node.clone(),
                generation: BOOT_GENERATION,
                input_generation: maintenance::process().input_generation(),
                expires: Instant::now() + Duration::from_secs(600),
                consumed: false,
            },
        )
    });
    let weak = std::sync::Arc::downgrade(&node.inner);
    std::thread::spawn(move || {
        let mut registered = false;
        let mut health_mode = initial.is_some();
        let mut prepared = initial.take();
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let Some(inner) = weak.upgrade() else {
                break;
            };
            let node = Node { inner };
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
                Ok(Reply::Prepare { transaction, automatic }) => {
                    if prepared.as_ref().is_none_or(|(tx, _)| *tx != transaction) {
                        prepared = None;
                        match node.prepare_for_update(automatic) {
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
                    if let Some((tx, mut token)) = prepared.take()
                        && tx == transaction
                    {
                        if health_mode && token.refresh_health_input().is_err() {
                            prepared = Some((tx, token));
                            continue;
                        }
                        match token.shutdown() {
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
                        match node.prepare_for_update(false) {
                            Ok(mut token) => {
                                token.hold_for_health();
                                prepared = Some((transaction.clone(), token));
                                health_mode = true;
                            }
                            Err(_) => continue,
                        }
                    }
                    if let Ok(status) = node.input_status() {
                        let _ = oink_winsvc::update::request(Request::Health {
                            transaction,
                            build,
                            input_ready: status.helper_priority.is_some(),
                            routing_ready: status.focus_info.is_some(),
                        });
                    }
                }
                Ok(Reply::Status { status }) => {
                    if health_mode {
                        prepared = None;
                        health_mode = false;
                    }
                    let changed = lock(&node.inner.update).status != status;
                    if changed {
                        lock(&node.inner.update).status = status.clone();
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
            if !maintenance::process()
                .close(BOOT_GENERATION)
                .map_err(|_| Error::Busy)?
                .is_empty()
            {
                return Err(Error::Busy);
            }
            *STARTUP.lock().unwrap() = Some(transaction);
            Ok(())
        }
        Ok(Reply::Error { error }) => Err(error),
        Ok(Reply::Status { .. }) => Ok(()),
        Err(Error::NotConfigured) if env!("OINK_RELEASE_BUILD") == "0" => Ok(()),
        _ => Err(Error::Transaction),
    }
}
