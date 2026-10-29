//! macOS domain enablement. The worker owns system calls; input and IPC only read/queue state.
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::api::{ApiEvent, FileProviderState as Phase, FileProviderStatus};
use crate::node::{Node, lock};
use oink_clipboard::FileProviderReason as Reason;

#[cfg(any(target_os = "macos", test))]
pub(crate) mod startup;

// Dropping the task (including abort before its next observation) retires the
// scheduler's permission to refill. The persisted setting survives a restart.
struct PlaceholderSchedule<F: Fn(bool, bool)>(F);
impl<F: Fn(bool, bool)> PlaceholderSchedule<F> {
    fn observe(&self, enabled: bool, fully_paused: bool, phase: Phase) {
        (self.0)(enabled, !fully_paused && phase == Phase::Available);
    }
}
impl<F: Fn(bool, bool)> Drop for PlaceholderSchedule<F> {
    fn drop(&mut self) {
        (self.0)(false, false);
    }
}

fn save_placeholder_setting(
    config: &mut crate::config::AgentConfig,
    enabled: bool,
    configure: impl FnOnce(bool, bool),
) -> crate::api::ApiResult<()> {
    let mut updated = config.clone();
    updated.settings.file_provider_placeholder_pool = enabled;
    updated.save().map_err(|error| crate::api::ApiError::Internal {
        message: error.to_string(),
    })?;
    *config = updated;
    // Invalidate on off now; cleanup is asynchronous. On waits for the
    // observer to confirm an available domain and that sharing is not paused.
    configure(enabled, false);
    Ok(())
}

pub(crate) fn set_placeholder_pool(node: &Node, enabled: bool) -> crate::api::ApiResult<()> {
    // The observer holds this same lock while configuring. A stale enabled
    // snapshot cannot race a completed off request and start another refill.
    save_placeholder_setting(
        &mut lock(&node.inner.config),
        enabled,
        oink_clipboard::file_provider::configure_placeholder_pool,
    )?;
    node.inner.file_provider_wake.notify_one();
    Ok(())
}

/// A disabled domain never hands a doomed file URL or promise to the target.
/// Ordinary representations in the same drag remain available.
pub(crate) fn without_files(
    mut spec: oink_clipboard::dnd::DragSpec,
) -> Result<Option<oink_clipboard::dnd::DragSpec>, oink_clipboard::ClipboardError> {
    spec.items = oink_clipboard::policy::items_without_files(spec.items);
    spec.files = None;
    if let Some(native) = spec.native.take() {
        spec.native = native
            .without_files()
            .map_err(|_| oink_clipboard::ClipboardError::Unsupported)?;
    }
    Ok((spec.native.is_some() || !spec.items.is_empty()).then_some(spec))
}

pub(crate) struct State {
    status: FileProviderStatus,
    reported: Option<FileProviderStatus>,
    reported_reason: Option<Reason>,
    required_drag: Option<oink_foundation::content::ClipId>,
    pending_open: bool,
    open_error: Option<String>,
    startup_error: Option<String>,
}

impl State {
    pub(crate) fn new(_data_dir: &Path) -> Self {
        let mut state = Self::fresh();
        state.startup_error = startup::failure();
        if state.startup_error.is_some() {
            state.observe(Phase::Error, None, false);
        }
        state
    }

    fn fresh() -> Self {
        Self {
            status: FileProviderStatus {
                state: Phase::Checking,
                ..Default::default()
            },
            reported: None,
            reported_reason: None,
            required_drag: None,
            pending_open: false,
            open_error: None,
            startup_error: None,
        }
    }

    pub(crate) fn snapshot(&self) -> FileProviderStatus {
        self.status.clone()
    }

    /// Only a toast/Settings action requests Finder. Duplicate requests coalesce.
    pub(crate) fn request_open(&mut self) {
        if self.startup_error.is_some() {
            return;
        }
        if self.status.state != Phase::Available && !self.status.opening {
            self.pending_open = true;
            self.status.opening = true;
            self.open_error = None;
            self.status.detail = None;
        }
    }

    /// Returns true once per transition into available, including a later re-enable.
    fn observe(&mut self, phase: Phase, detail: Option<String>, needed: bool) -> bool {
        let (phase, detail) = if let Some(error) = &self.startup_error {
            self.pending_open = false;
            self.status.opening = false;
            (Phase::Error, Some(error.clone()))
        } else {
            (phase, detail)
        };
        let observed_reason = reason(phase);
        let phase = if phase == Phase::Available && observed_reason.unavailable() {
            Phase::Error
        } else {
            phase
        };
        let became_available = self.status.state != Phase::Available && phase == Phase::Available;
        self.status.state = phase;
        self.status.reason = Some(observed_reason);
        self.status.needed |= needed;
        if phase == Phase::Available {
            self.open_error = None;
            self.pending_open = false;
            self.status.opening = false;
        }
        self.status.detail = detail.or_else(|| self.open_error.clone());
        became_available
    }

    /// Never bring Finder forward during a drag or while another device has focus.
    fn take_open(&mut self, safe_to_open: bool) -> bool {
        if !safe_to_open || self.status.state == Phase::Available {
            return false;
        }
        if !self.pending_open {
            return false;
        }
        self.pending_open = false;
        self.status.opening = true;
        self.open_error = None;
        self.status.detail = None;
        true
    }

    pub(crate) fn require_drag(&mut self, id: oink_foundation::content::ClipId) -> bool {
        if self.required_drag == Some(id) {
            return false;
        }
        self.required_drag = Some(id);
        true
    }

    fn opened(&mut self, result: Result<(), String>) {
        self.status.opening = false;
        self.open_error = result.err();
        self.status.detail = self.open_error.clone();
    }

    fn changed(&mut self) -> Option<FileProviderStatus> {
        if self.reported.as_ref() == Some(&self.status) {
            return None;
        }
        self.reported = Some(self.status.clone());
        Some(self.status.clone())
    }
}

fn reason(phase: Phase) -> Reason {
    match phase {
        Phase::Checking | Phase::Unsupported => Reason::Checking,
        Phase::Unregistered => Reason::ExtensionNotRegistered,
        Phase::NotEnabled => Reason::DomainDisabled,
        Phase::Disconnected => Reason::DomainDisconnected,
        Phase::Error => Reason::RegistrationFailed,
        Phase::Available => oink_clipboard::file_provider::publication_reason(),
    }
}
fn classify(domain: Option<oink_clipboard::file_provider::DomainStatus>) -> (Phase, Option<String>) {
    let phase = match domain {
        None => Phase::Unregistered,
        Some(d) if d.disconnected => Phase::Disconnected,
        Some(d) if !d.hidden => {
            tracing::warn!(
                operation = "classify_domain",
                cause = "domain_not_hidden",
                "File Provider domain rejected"
            );
            return (Phase::Error, Some("File Provider domain_not_hidden".into()));
        }
        Some(d) if !d.user_enabled => Phase::NotEnabled,
        Some(_) => Phase::Available,
    };
    let detail = if phase == Phase::Available {
        oink_clipboard::file_provider::diagnostic()
    } else {
        reason(phase).detail()
    };
    (phase, Some(detail))
}

fn classify_observation(
    status: &Result<Option<oink_clipboard::file_provider::DomainStatus>, String>,
) -> (Phase, Option<String>) {
    match status {
        Ok(domain) => classify(domain.clone()),
        Err(cause) => (Phase::Error, Some(cause.clone())),
    }
}

pub(crate) fn report(node: &Node) {
    let (change, availability_change) = {
        let mut state = lock(&node.inner.file_provider);
        let availability_change = (state.reported_reason != state.status.reason)
            .then_some(state.status.reason)
            .flatten();
        if state.reported_reason != state.status.reason {
            state.reported_reason = state.status.reason;
            if let Some(reason) = state.status.reason {
                if reason.unavailable() {
                    // Details come from fixed launch stages/domain checks or
                    // code-only NSError diagnostics, never localized userInfo.
                    tracing::warn!(%reason, detail = state.status.detail.as_deref(), "File Provider unavailable; file representations withheld");
                } else {
                    tracing::info!(%reason, "File Provider availability changed");
                }
            }
        }
        (state.changed(), availability_change)
    };
    if let Some(reason) = availability_change
        && let Some(d) = node.inner.diagnostics.get()
    {
        d.recorder.file_provider_changed(reason);
    }
    if let Some(status) = change {
        node.emit(ApiEvent::FileProviderStatus(status));
    }
}

pub(crate) fn spawn(node: &Node) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(&node.inner);
    let mut stop = node.stop_receiver();
    let wake = node.inner.file_provider_wake.clone();
    let safety = node.inner.file_provider_safety_wake.clone();
    let mut config = node.link_config().expect("config assembled").subscribe();
    let placeholders = PlaceholderSchedule(oink_clipboard::file_provider::configure_placeholder_pool);
    tokio::spawn(async move {
        if let Some(inner) = weak.upgrade() {
            let config = lock(&inner.config);
            placeholders.observe(config.settings.file_provider_placeholder_pool, false, Phase::Checking);
        }
        // Do not inspect/adopt an existing domain until the launch owner has
        // removed fresh-data leftovers or completed recovery. UI reads remain
        // Checking throughout; both success and failure wake this task.
        loop {
            let ready = oink_clipboard::file_provider::demand_changed().notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            if let Some(inner) = weak.upgrade() {
                let node = Node { inner };
                {
                    let mut state = lock(&node.inner.file_provider);
                    state.startup_error = startup::failure();
                    state.observe(Phase::Checking, None, false);
                }
                report(&node);
            }
            if !oink_clipboard::file_provider::launch_preparation_pending() {
                break;
            }
            tokio::select! {
                _ = stop.changed() => return,
                _ = &mut ready => {},
            }
        }
        if let Some(inner) = weak.upgrade() {
            lock(&inner.file_provider).startup_error = startup::failure();
        }
        let mut domains = oink_clipboard::file_provider::Observer::new();
        let mut domain_status = None;
        let first_query_timeout = tokio::time::sleep(Duration::from_secs(3));
        tokio::pin!(first_query_timeout);
        loop {
            tokio::select! {
                _ = stop.changed() => break,
                change = domains.changed() => match change {
                    Ok(status) => {
                        if domain_status.as_ref().is_some_and(|old| old != &status) {
                            oink_clipboard::file_provider::domain_changed();
                        }
                        domain_status = Some(status);
                    },
                    Err(_) => break,
                },
                _ = &mut first_query_timeout, if domain_status.is_none() => {
                    tracing::warn!(operation = "get_domains", cause = "callback_timeout", timeout_ms = 3000,
                        "File Provider observation failed");
                    domain_status = Some(Err("File Provider status query timed out".into()));
                },
                _ = oink_clipboard::file_provider::demand_changed().notified() => domains.refresh(),
                _ = wake.notified() => domains.refresh(),
                // Focus/drag/pause owners wake the safety check as their state changes.
                _ = safety.notified() => {},
                change = config.next() => if change.is_none() { break; },
            }
            let Some(inner) = weak.upgrade() else { return };
            let node = Node { inner };
            node.inner.reports.refresh_file_provider();
            let (phase, detail) = match &domain_status {
                Some(status) => classify_observation(status),
                None => (Phase::Checking, None),
            };
            let (available, phase) = {
                let mut state = lock(&node.inner.file_provider);
                let available = state.observe(phase, detail, oink_clipboard::file_provider::was_requested());
                (available, state.status.state)
            };
            if phase == Phase::Available {
                oink_clipboard::file_provider::domain_usable();
            }
            if available && let Some(clip) = node.clipboard() {
                clip.file_provider_available();
            }
            let fully_paused = node.fully_paused();
            let safe = !fully_paused
                && node.input_status().is_ok_and(|s| !s.dnd_active)
                && node.focus().is_none_or(|f| f.is_local);
            {
                let config = lock(&node.inner.config);
                placeholders.observe(config.settings.file_provider_placeholder_pool, fully_paused, phase);
            }
            let open = lock(&node.inner.file_provider).take_open(safe);
            report(&node);
            if open {
                let result = tokio::task::spawn_blocking(oink_clipboard::file_provider::open_enable)
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.map_err(|e| e.to_string()));
                if let Err(ref e) = result {
                    tracing::warn!(%e, "opening File Provider enablement in Finder failed");
                }
                lock(&node.inner.file_provider).opened(result);
                domains.refresh();
                report(&node);
            }
        }
    })
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn observation_failure_preserves_the_specific_cause_in_status() {
        let cause = "get_domains: NSCocoaErrorDomain code=4097 <- NSPOSIXErrorDomain code=13";
        let (phase, detail) = classify_observation(&Err(cause.into()));
        let mut state = State::fresh();
        state.observe(phase, detail, false);
        assert_eq!(state.snapshot().reason, Some(Reason::RegistrationFailed));
        assert_eq!(state.snapshot().detail.as_deref(), Some(cause));
    }

    #[test]
    fn launch_failure_remains_visible_despite_enabled_domain_notifications() {
        let mut state = State::fresh();
        state.startup_error = Some("Launch recovery failed; reopen Oink".into());
        assert!(!state.observe(Phase::Available, None, true));
        assert_eq!(state.snapshot().state, Phase::Error);
        assert_eq!(state.snapshot().reason, Some(Reason::RegistrationFailed));
        assert_eq!(
            state.snapshot().detail.as_deref(),
            Some("Launch recovery failed; reopen Oink")
        );
        state.request_open();
        assert!(!state.take_open(true));
        assert!(state.snapshot().detail.is_some());
        // Only the launch owner's successful native readiness recheck clears
        // this error. A domain flag notification alone cannot bypass migration.
        state.startup_error = None;
        assert!(state.observe(Phase::Available, None, true));
        assert_eq!(state.snapshot().state, Phase::Available);
        assert!(state.snapshot().detail.is_none());
    }

    #[test]
    fn deferred_launch_failure_retires_an_enable_request_made_while_checking() {
        let mut state = State::fresh();
        state.request_open();
        assert!(state.snapshot().opening);
        state.startup_error = Some("Launch preparation failed".into());
        state.observe(Phase::Available, None, true);
        assert_eq!(state.snapshot().state, Phase::Error);
        assert!(!state.snapshot().opening);
        assert!(!state.take_open(true));
    }

    #[test]
    fn placeholder_setting_persists_and_off_invalidates_without_an_observer_tick() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::AgentConfig::load_from(dir.path()).unwrap();
        let calls = std::cell::RefCell::new(Vec::new());
        for enabled in [true, false] {
            save_placeholder_setting(&mut config, enabled, |enabled, idle| {
                // Persistence precedes the backend call; off does not depend on
                // the domain observer being polled or on a cleanup OS callback.
                assert_eq!(
                    crate::config::AgentConfig::load_from(dir.path())
                        .unwrap()
                        .settings
                        .file_provider_placeholder_pool,
                    enabled
                );
                calls.borrow_mut().push((enabled, idle));
            })
            .unwrap();
        }
        assert_eq!(*calls.borrow(), [(true, false), (false, false)]);
        assert!(!config.settings.file_provider_placeholder_pool);
        // A failed save must not enable the backend or alter the live setting.
        std::fs::remove_file(config.settings_path()).unwrap();
        std::fs::create_dir(config.settings_path()).unwrap();
        assert!(save_placeholder_setting(&mut config, true, |_, _| panic!("failed save configured pool")).is_err());
        assert!(!config.settings.file_provider_placeholder_pool);
    }

    #[tokio::test]
    async fn placeholder_scheduler_requires_available_unpaused_and_retires_on_task_abort() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = calls.clone();
        let scheduler = PlaceholderSchedule(move |enabled, allowed| observed.lock().unwrap().push((enabled, allowed)));
        // The second argument is fully_paused; remote focus and an active drag
        // do not revoke refill permission. Domain prompts keep their idle gate.
        scheduler.observe(false, false, Phase::Available);
        scheduler.observe(true, true, Phase::Available);
        scheduler.observe(true, false, Phase::Checking);
        scheduler.observe(true, false, Phase::Available);
        let (ready, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _scheduler = scheduler;
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        started.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(
            *calls.lock().unwrap(),
            [
                (false, true),
                (true, false),
                (true, false),
                (true, true),
                (false, false)
            ]
        );
    }

    #[test]
    fn disabled_file_drag_is_refused_and_mixed_text_is_kept() {
        use oink_clipboard::{
            ClipData,
            dnd::{DragOperation, DragOps, DragSpec},
        };
        let make = |mixed| {
            let mut items = vec![ClipData::Files(vec![oink_clipboard::FileEntry {
                path: "file.txt".into(),
                is_dir: false,
                size: 1,
                modified_ms: None,
                local_path: None,
            }])];
            if mixed {
                items.push(ClipData::Text("text".into()));
            }
            DragSpec {
                native: None,
                operation: DragOperation::new(),
                at: oink_foundation::Point::new(0.0, 0.0),
                items,
                files: None,
                ops: DragOps::COPY,
            }
        };
        assert!(without_files(make(false)).unwrap().is_none());
        let spec = without_files(make(true)).unwrap().unwrap();
        assert_eq!(spec.items, vec![ClipData::Text("text".into())]);
        assert!(spec.files.is_none() && spec.native.is_none());
    }

    #[test]
    fn enable_notice_is_emitted_once_per_drag_session() {
        let mut state = State::fresh();
        let mut drag = oink_foundation::content::ClipId {
            origin: oink_foundation::DeviceId([1; 32]),
            seq: 3,
        };
        assert!(state.require_drag(drag));
        assert!(!state.require_drag(drag));
        state.observe(Phase::NotEnabled, None, true);
        assert!(!state.require_drag(drag));
        drag.seq += 1;
        assert!(state.require_drag(drag));
        assert!(!state.require_drag(drag));
    }

    #[test]
    fn file_demand_never_opens_finder_without_an_explicit_action() {
        for phase in [Phase::NotEnabled, Phase::Unregistered] {
            let mut state = State::fresh();
            state.observe(phase, None, true);
            assert!(!state.take_open(false));
            assert!(!state.take_open(true));
            state.request_open();
            assert!(!state.take_open(false));
            assert!(state.take_open(true));
            state.opened(Ok(()));
            assert!(!state.take_open(true));
        }
    }

    #[test]
    fn availability_changes_emit_once_and_reenable_can_resume() {
        let mut s = State::fresh();
        assert!(s.changed().is_some());
        assert!(s.changed().is_none());
        s.observe(Phase::NotEnabled, None, true);
        assert!(s.changed().is_some());
        assert!(s.observe(Phase::Available, None, true));
        assert!(s.changed().is_some());
        assert!(!s.observe(Phase::Available, None, true));
        assert!(s.changed().is_none());
        assert!(!s.take_open(true));
        s.observe(Phase::Disconnected, None, true);
        assert!(s.changed().is_some());
        assert!(s.observe(Phase::Available, None, true));
    }

    #[test]
    fn open_errors_stay_visible_and_manual_retry_clears_them() {
        let mut s = State::fresh();
        s.observe(Phase::Unregistered, None, true);
        s.request_open();
        assert!(s.take_open(true));
        s.opened(Err("Finder failed".into()));
        s.observe(Phase::NotEnabled, None, true);
        assert_eq!(s.snapshot().detail.as_deref(), Some("Finder failed"));
        assert!(!s.take_open(true));
        s.request_open();
        assert!(s.snapshot().opening);
        assert!(s.snapshot().detail.is_none());
        s.observe(Phase::Available, None, true); // user enabled while request queued
        assert!(!s.take_open(true));
        assert!(!s.snapshot().opening);
    }

    #[test]
    fn maps_actual_domain_flags_without_assuming_url_means_enabled() {
        use oink_clipboard::file_provider::DomainStatus;
        let d = |hidden, user_enabled, disconnected| {
            Some(DomainStatus {
                id: "oink-files".into(),
                hidden,
                user_enabled,
                disconnected,
            })
        };
        assert_eq!(classify(None).0, Phase::Unregistered);
        assert_eq!(classify(d(true, false, false)).0, Phase::NotEnabled);
        assert_eq!(classify(d(true, true, false)).0, Phase::Available);
        assert_eq!(classify(d(true, true, true)).0, Phase::Disconnected);
        assert_eq!(classify(d(false, true, false)).0, Phase::Error);
    }
}
