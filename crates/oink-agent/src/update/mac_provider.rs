//! A temporary provider disconnect belongs to an approved replacement attempt.
//! Abort reconnects only a domain we disconnected; new-host launch recovery
//! reconnects after successful replacement. Never enable an inactive domain.
use oink_update::Error;
use std::{path::Path, path::PathBuf, time::Duration, time::Instant};

pub(super) const COMMAND_BUDGET: Duration = Duration::from_secs(8);
pub(super) const PREPARATION_BUDGET: Duration = COMMAND_BUDGET.saturating_mul(2);

#[derive(Debug)]
pub(super) struct Suspension {
    manager: PathBuf,
    prepared: bool,
}
impl Suspension {
    pub(super) fn prepare(manager: PathBuf, deadline: Instant, current: &dyn Fn() -> bool) -> Result<Self, Error> {
        let mut result = Self {
            manager,
            prepared: false,
        };
        #[derive(serde::Deserialize)]
        struct Domain {
            id: String,
            hidden: bool,
            #[serde(rename = "userEnabled")]
            enabled: bool,
            disconnected: bool,
        }
        let domains: Vec<Domain> = serde_json::from_str(&run(&result.manager, "inspect", deadline, current)?)
            .map_err(|_| Error::Transaction)?;
        result.prepared = domains
            .iter()
            .any(|d| d.id == "oink-files" && d.hidden && d.enabled && !d.disconnected);
        if !result.prepared {
            return Ok(result);
        }
        // Record rollback intent before asking macOS: a timeout/failed reply
        // does not prove the asynchronous disconnect never took effect. An
        // initially inactive domain must never be reconnected by this guard.
        let status = run(&result.manager, "prepare-update", deadline, current)?;
        result.prepared = match status.as_str() {
            "prepared" => true,
            "inactive" => false,
            _ => return Err(Error::Transaction),
        };
        Ok(result)
    }
    /// The next host owns the persisted reconnect after a committed shutdown.
    pub(super) fn commit(&mut self) {
        self.prepared = false;
    }
    fn restore(&mut self) -> Result<(), Error> {
        if self.prepared {
            let result = run(&self.manager, "finish-update", Instant::now() + COMMAND_BUDGET, &|| {
                true
            })?;
            if !matches!(result.as_str(), "connected" | "inactive") {
                return Err(Error::Transaction);
            }
            self.prepared = false;
        }
        Ok(())
    }
}
impl Drop for Suspension {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            // Next normal launch also runs the manager's persisted recovery.
            // A failed reconnect is visible; never report a successful rollback.
            tracing::error!(%error, "could not reconnect File Provider after aborted update");
        }
    }
}
fn run(manager: &Path, action: &str, deadline: Instant, current: &dyn Fn() -> bool) -> Result<String, Error> {
    let (status, output) = crate::file_provider::startup::run(manager, &[action.as_ref()], deadline, current)
        .map_err(|_| Error::Transaction)?;
    if !status.success() {
        return Err(Error::Transaction);
    }
    Ok(output.trim().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn manager(directory: &Path, response: &str) -> PathBuf {
        let path = directory.join("manager");
        // Fake manager only: no system provider, app, permissions or IPC.
        let disconnected = response == "inactive";
        std::fs::write(&path, format!("#!/bin/sh\necho \"$1\" >> \"$0.calls\"\ncase \"$1\" in\ninspect) echo '[{{\"id\":\"oink-files\",\"hidden\":true,\"userEnabled\":true,\"disconnected\":{disconnected}}}]';;\nprepare-update) if [ '{response}' = failed ]; then exit 1; fi; echo '{response}';;\nfinish-update) echo connected;;\nesac\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    #[test]
    fn committed_shutdown_leaves_reconnect_to_the_new_host() {
        let directory = tempfile::tempdir().unwrap();
        let path = manager(directory.path(), "prepared");
        let mut guard = Suspension::prepare(path.clone(), Instant::now() + Duration::from_secs(16), &|| true).unwrap();
        guard.commit();
        drop(guard);
        assert_eq!(
            std::fs::read_to_string(path.with_extension("calls")).unwrap(),
            "inspect\nprepare-update\n"
        );
    }
    #[test]
    fn inspect_and_disconnect_share_the_preparation_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let path = manager(directory.path(), "prepared");
        let script = std::fs::read_to_string(&path).unwrap().replace(
            "case \"$1\" in",
            "if [ \"$1\" != finish-update ]; then sleep 0.06; fi\ncase \"$1\" in",
        );
        std::fs::write(&path, script).unwrap();
        let result = Suspension::prepare(path, Instant::now() + Duration::from_millis(90), &|| true);
        assert!(
            matches!(result, Err(Error::Transaction)),
            "commands cannot each reset the shared deadline"
        );
    }
    #[test]
    fn aborted_replacement_reconnects_only_a_domain_it_suspended() {
        for (response, calls) in [
            ("prepared", "inspect\nprepare-update\nfinish-update\n"),
            ("inactive", "inspect\n"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = manager(directory.path(), response);
            drop(Suspension::prepare(path.clone(), Instant::now() + Duration::from_secs(16), &|| true).unwrap());
            assert_eq!(std::fs::read_to_string(path.with_extension("calls")).unwrap(), calls);
        }
    }
    #[test]
    fn failed_prepare_never_authorizes_replacement_and_restores_a_possible_disconnect() {
        let directory = tempfile::tempdir().unwrap();
        let path = manager(directory.path(), "unexpected");
        assert!(matches!(
            Suspension::prepare(path.clone(), Instant::now() + Duration::from_secs(16), &|| true),
            Err(Error::Transaction)
        ));
        assert_eq!(
            std::fs::read_to_string(path.with_extension("calls")).unwrap(),
            "inspect\nprepare-update\nfinish-update\n"
        );
        assert!(matches!(
            Suspension::prepare(path, Instant::now() + Duration::from_secs(16), &|| false),
            Err(Error::Transaction)
        ));
        let path = manager(directory.path(), "failed");
        assert!(matches!(
            Suspension::prepare(path, Instant::now() + Duration::from_secs(16), &|| true),
            Err(Error::Transaction)
        ));
    }
}
