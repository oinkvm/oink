//! Pre-login availability (Windows): the user backend's machine-level launch at
//! login and its identity snapshot export, and the SYSTEM pre-login entry.
//!
//! - Launch at login has two layers ([`oink_autostart::set_enabled`]). The machine
//!   layer here talks to `OinkInput` over this session's launch pipe; claiming
//!   the identity runs the installed CLI through UAC.
//! - While this user owns the machine identity, the backend exports `device.key`,
//!   the group state and the routing records to the service after group changes,
//!   settings edits and at normal exit. The service rejects non-owners.
//! - `oink.exe --prelogin` runs [`run`]: entry checks, a fresh working copy of
//!   the snapshot, machine-level roots, then the [`crate::node::Profile::Prelogin`]
//!   Node without IPC, tray or UI.
use crate::node::Node;
use oink_autostart::{MachineLayer, NoMachineLayer};

/// The machine layer for this backend: the service on a protected Windows
/// install, nothing elsewhere (macOS, development builds, test fixtures).
pub(crate) fn machine_layer(node: &Node) -> Box<dyn MachineLayer> {
    #[cfg(windows)]
    if node.native_services_enabled() && oink_winsvc::client::service_installation().unwrap_or(false) {
        return Box::new(windows::Service);
    }
    let _ = node;
    Box::new(NoMachineLayer)
}

impl Node {
    /// Group, layout or settings changed: refresh the exported snapshot (debounced).
    pub(crate) fn prelogin_changed(&self) {
        if let Some(wake) = self.inner.prelogin_export.get() {
            wake.notify_one();
        }
    }

    pub(crate) fn start_prelogin_export(&self) -> Option<tokio::task::JoinHandle<()>> {
        #[cfg(windows)]
        if self.inner.profile == crate::node::Profile::User
            && self.native_services_enabled()
            && oink_winsvc::client::service_installation().unwrap_or(false)
        {
            return Some(windows::start_export(self));
        }
        None
    }

    /// Final export at normal exit, bounded so quitting never waits on the service.
    pub(crate) async fn prelogin_export_on_exit(&self) {
        #[cfg(windows)]
        if self.inner.prelogin_export.get().is_some() {
            let directory = self.data_dir().to_owned();
            let push = tokio::task::spawn_blocking(move || windows::export(&directory));
            if tokio::time::timeout(std::time::Duration::from_secs(2), push)
                .await
                .is_err()
            {
                tracing::warn!("pre-login snapshot export at exit timed out");
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use oink_autostart::{PreloginProblem, PreloginState};
    use oink_winsvc::{OwnerView, SnapshotFile, prelogin};
    use std::{path::Path, sync::Arc, time::Duration};

    pub(super) struct Service;
    fn state(result: std::io::Result<prelogin::State>) -> Result<PreloginState, PreloginProblem> {
        match result {
            Ok(state) if !state.enabled => Ok(PreloginState::Off),
            Ok(state) => Ok(match state.owner {
                OwnerView::Caller => PreloginState::Enabled,
                OwnerView::Other => PreloginState::OtherUser,
                OwnerView::None => PreloginState::Off,
            }),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                tracing::warn!(%error, "pre-login request rejected");
                Err(PreloginProblem::Rejected)
            }
            Err(error) => {
                tracing::warn!(%error, "pre-login service unavailable");
                Err(PreloginProblem::ServiceUnavailable)
            }
        }
    }
    impl MachineLayer for Service {
        fn status(&self) -> Result<PreloginState, PreloginProblem> {
            state(prelogin::status())
        }
        fn claim(&self) -> Result<PreloginState, PreloginProblem> {
            match prelogin::elevate_claim() {
                prelogin::Elevation::Approved => {
                    tracing::info!("pre-login identity claimed for this user");
                    state(prelogin::status())
                }
                prelogin::Elevation::Declined => Err(PreloginProblem::ElevationDeclined),
                prelogin::Elevation::Failed => Err(PreloginProblem::ServiceUnavailable),
            }
        }
        fn release(&self) -> Result<PreloginState, PreloginProblem> {
            state(prelogin::release())
        }
    }

    /// Read the owner's files from the user data directory. Optional records may
    /// be absent; the identity and group state may not.
    pub(super) fn collect(directory: &Path) -> std::io::Result<Vec<(SnapshotFile, Vec<u8>)>> {
        use std::io::Read;
        let mut files = Vec::new();
        for file in SnapshotFile::ALL {
            match std::fs::File::open(directory.join(file.name())) {
                Ok(handle) => {
                    let mut bytes = Vec::new();
                    handle.take(file.max_bytes() as u64 + 1).read_to_end(&mut bytes)?;
                    if bytes.len() > file.max_bytes() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("{} exceeds the pre-login snapshot limit", file.name()),
                        ));
                    }
                    files.push((file, bytes));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && !file.required() => {}
                Err(error) => return Err(error),
            }
        }
        Ok(files)
    }

    /// Push only while this user owns an enabled machine identity: the private
    /// key never crosses the pipe otherwise.
    pub(super) fn export(directory: &Path) {
        match prelogin::status() {
            Ok(state) if state.enabled && state.owner == OwnerView::Caller => {}
            Ok(_) => return,
            Err(error) => {
                tracing::debug!(%error, "pre-login status unavailable; snapshot not exported");
                return;
            }
        }
        let result = collect(directory).and_then(prelogin::push_snapshot);
        match result {
            Ok(state) => tracing::info!(snapshot = state.snapshot, "pre-login snapshot exported"),
            Err(error) => tracing::warn!(%error, "pre-login snapshot export failed"),
        }
    }

    pub(super) fn start_export(node: &Node) -> tokio::task::JoinHandle<()> {
        let wake = Arc::new(tokio::sync::Notify::new());
        let _ = node.inner.prelogin_export.set(wake.clone());
        let directory = node.data_dir().to_owned();
        let mut stop = node.stop_receiver();
        // An installer-enabled marker has no snapshot yet: export once at start.
        wake.notify_one();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop.changed() => break,
                    () = wake.notified() => {}
                }
                // Coalesce a burst of edits (pairing, merge, layout drags).
                tokio::select! {
                    _ = stop.changed() => break,
                    () = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
                let directory = directory.clone();
                let _ = tokio::task::spawn_blocking(move || export(&directory)).await;
            }
        })
    }
}

/// `oink.exe --prelogin`: returns the process exit code.
#[cfg(windows)]
pub fn run() -> u8 {
    match run_checked() {
        Ok(()) => 0,
        Err(error) => {
            tracing::error!("pre-login backend failed: {error:#}");
            eprintln!("Oink pre-login backend: {error:#}");
            1
        }
    }
}

#[cfg(windows)]
fn run_checked() -> anyhow::Result<()> {
    use anyhow::Context as _;
    // Before any storage access: never the SYSTEM profile's LOCALAPPDATA.
    let entry = oink_winsvc::prelogin::verify_entry().context("pre-login entry check")?;
    let roots = oink_winsvc::prelogin::roots()?;
    oink_foundation::paths::StoragePaths::set_process_roots(oink_foundation::paths::StoragePaths {
        data: roots.data.clone(),
        cache: roots.cache.clone(),
        logs: roots.logs.clone(),
    })
    .map_err(|e| anyhow::anyhow!(e))?;
    oink_winsvc::prelogin::prepare_work().context("prepare pre-login working copy")?;
    crate::logs::init_prelogin();
    tracing::info!(session = entry.session, "pre-login backend starting");
    // The broker selects the active console helper independently of this network session.
    oink_winsvc::client::use_prelogin_route();
    let input = oink_input::prelogin_backend().map_err(|e| anyhow::anyhow!("input backend: {e}"))?;
    let display = oink_display::platform_backend().map_err(|e| anyhow::anyhow!("display backend: {e}"))?;
    let displays = display.displays().map_err(|e| anyhow::anyhow!("displays: {e}"))?;
    let net = oink_network::NetworkConfig::default();
    let options = crate::node::NodeOptions {
        net,
        input: std::sync::Arc::from(input),
        displays,
        display_backend: Some(std::sync::Arc::from(display)),
        clipboard: None,
        mode_switch_in_callback: true,
        capture_reports_key_repeat: true,
        // No visible window on the logon desktop.
        cursor_trail: false,
    };
    let lock = crate::instance::acquire(&options.net.data_dir)?;
    let host = crate::host::AgentHost::start_prelogin(options, lock)?;
    let node = host.node().clone();
    tracing::info!(
        name = %node.device_name(node.id()),
        port = node.links().map(|l| l.local_addr().port()).unwrap_or(0),
        "pre-login backend running"
    );
    let waiter = std::thread::Builder::new()
        .name("oink-prelogin-stop".into())
        .spawn(move || {
            entry.wait_stop();
            node.request_stop();
        })?;
    host.block_on(host.node().wait_for_stop());
    tracing::info!("pre-login backend stopping");
    // Drain routing/injection and leave the group before confirming the final
    // native release ACK. Release failure is diagnostic; the service waits for
    // this pinned process to exit before creating the user network identity.
    host.shutdown_and_wait();
    drop(waiter);
    if let Some(reason) = oink_winsvc::client::prelogin_shutdown_failure() {
        tracing::warn!(?reason, "pre-login final release unconfirmed; shutdown complete");
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn snapshot_names_match_the_owners_files() {
        use oink_winsvc::SnapshotFile;
        assert_eq!(
            SnapshotFile::GroupState.name(),
            crate::config_module::group_store::STATE_FILE
        );
        assert_eq!(SnapshotFile::Settings.name(), crate::config::SETTINGS_FILE);
        assert_eq!(
            SnapshotFile::Addresses.name(),
            crate::config_module::local_records::ADDRESSES_FILE
        );
        assert_eq!(
            SnapshotFile::DetachedScreens.name(),
            crate::config_module::detached::DETACHED_FILE
        );
    }

    #[test]
    fn collect_requires_identity_and_group_state_but_not_records() {
        let directory = tempfile::tempdir().unwrap();
        assert!(super::windows::collect(directory.path()).is_err());
        std::fs::write(directory.path().join("device.key"), [3; 32]).unwrap();
        std::fs::write(directory.path().join("group-state.json"), b"{}").unwrap();
        let files = super::windows::collect(directory.path()).unwrap();
        assert_eq!(files.len(), 2);
        std::fs::write(directory.path().join("addresses.json"), b"[]").unwrap();
        assert_eq!(super::windows::collect(directory.path()).unwrap().len(), 3);
    }
}
