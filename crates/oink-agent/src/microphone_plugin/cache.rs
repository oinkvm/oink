//! Process-local snapshots: OS inspection never runs on an IPC reader's thread.
use super::*;
use std::sync::{Condvar, Mutex, OnceLock};

#[derive(Default)]
struct State {
    value: Inspection,
    generation: u64,
    dirty: bool,
    operation_pending: bool,
    in_flight: bool,
}
#[derive(Default)]
struct Cache {
    state: Mutex<State>,
    wake: Condvar,
}
impl Cache {
    fn snapshot(&self) -> MicrophonePluginStatus {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .value
            .status
            .clone()
    }
    fn begin(&self, action: Action) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.operation_pending || state.value.status.operation != MicrophonePluginOperation::Idle {
            return Err("A microphone operation is already pending".into());
        }
        state.generation += 1;
        state.operation_pending = true;
        state.value.status.operation = match action {
            Action::Install => MicrophonePluginOperation::Installing,
            Action::Uninstall => MicrophonePluginOperation::Uninstalling,
        };
        state.value.status.last_operation_succeeded = None;
        state.value.status.error_kind = None;
        state.value.status.detail = None;
        state.value.status.step = None;
        state.value.status.step_started_at_ms = None;
        Ok(())
    }
    fn invalidate(&self, completed: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.generation += 1;
        if completed {
            state.operation_pending = false;
        }
        state.dirty = true;
        state.value.status.refreshing = true;
        self.wake.notify_one();
    }
    fn refresh(&self, query: impl FnOnce() -> Inspection) {
        let generation = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.operation_pending || state.in_flight {
                return;
            }
            state.in_flight = true;
            state.dirty = false;
            state.value.status.refreshing = true;
            state.generation
        };
        let mut value = query();
        value.status.refreshing = false;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.in_flight = false;
        // A query started before an operation must never overwrite its result.
        if state.generation == generation && !state.operation_pending {
            state.value = value;
        }
    }
    fn run(&self) {
        loop {
            self.refresh(super::inspect_status);
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.operation_pending || !state.dirty {
                state = self.wake.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        }
    }
}
static CACHE: OnceLock<Cache> = OnceLock::new();
fn cache() -> &'static Cache {
    CACHE.get_or_init(Cache::default)
}
static STARTED: OnceLock<()> = OnceLock::new();
pub(super) fn start() {
    STARTED.get_or_init(|| {
        cache().invalidate(false);
        if let Err(error) = std::thread::Builder::new()
            .name("microphone-status".into())
            .spawn(|| cache().run())
        {
            let mut state = cache().state.lock().unwrap_or_else(|e| e.into_inner());
            state.value.status.refreshing = false;
            state.value.status.state = MicrophonePluginState::Error;
            state.value.status.error_kind = Some(MicrophonePluginErrorKind::QueryFailed);
            state.value.status.detail = Some(error.to_string());
        }
    });
}
pub(super) fn progress(step: MicrophonePluginStep) {
    let mut state = cache().state.lock().unwrap_or_else(|e| e.into_inner());
    if state.operation_pending && state.value.status.step != Some(step) {
        state.value.status.step = Some(step);
        state.value.status.step_started_at_ms = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        );
        tracing::info!(?step, "microphone installer step");
    }
}
pub(super) fn status() -> MicrophonePluginStatus {
    cache().snapshot()
}
#[cfg(windows)]
pub(super) fn virtual_output() -> Option<String> {
    cache()
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .value
        .render_id
        .clone()
}
pub(super) fn begin(action: Action) -> Result<(), String> {
    cache().begin(action)
}
pub(super) fn completed() {
    cache().invalidate(true);
}
pub(super) fn changed() {
    if STARTED.get().is_some() {
        cache().invalidate(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn installed() -> Inspection {
        MicrophonePluginStatus {
            state: MicrophonePluginState::Installed,
            managed: true,
            ..Default::default()
        }
        .into()
    }
    #[test]
    fn last_operation_wire_values_and_cache_survive_new_attempt() {
        let cache = Cache::default();
        assert_eq!(
            serde_json::to_value(cache.snapshot()).unwrap()["last_operation"],
            serde_json::Value::Null
        );
        for (action, wire) in [(Action::Install, "install"), (Action::Uninstall, "uninstall")] {
            cache.refresh(|| {
                MicrophonePluginStatus {
                    last_operation: Some(action),
                    ..Default::default()
                }
                .into()
            });
            assert_eq!(serde_json::to_value(cache.snapshot()).unwrap()["last_operation"], wire);
        }
        cache.begin(Action::Install).unwrap();
        assert_eq!(cache.snapshot().last_operation, Some(Action::Uninstall));
        cache.invalidate(true);
        cache.refresh(Inspection::default); // Fresh OS state after reboot.
        assert_eq!(cache.snapshot().last_operation, None);
        let mut legacy = serde_json::to_value(cache.snapshot()).unwrap();
        legacy.as_object_mut().unwrap().remove("last_operation");
        assert_eq!(
            serde_json::from_value::<MicrophonePluginStatus>(legacy)
                .unwrap()
                .last_operation,
            None
        );
    }
    #[test]
    fn status_and_runtime_endpoint_commit_and_clear_together() {
        let cache = Cache::default();
        let ready = || {
            let mut inspection = Inspection::default();
            super::super::windows_presence::apply(
                &mut inspection,
                true,
                true,
                oink_audio::microphone::VirtualOutput::Ready {
                    render_id: "fixture".into(),
                    recording_name: "fixture".into(),
                },
            );
            inspection
        };
        cache.refresh(ready);
        assert!(cache.snapshot().feed_loaded);
        assert_eq!(cache.state.lock().unwrap().value.render_id.as_deref(), Some("fixture"));
        cache.begin(Action::Uninstall).unwrap();
        cache.refresh(|| panic!("pending operation cannot publish partial inspection"));
        cache.invalidate(true);
        cache.refresh(|| {
            let mut inspection = Inspection::default();
            super::super::windows_presence::apply(
                &mut inspection,
                false,
                false,
                oink_audio::microphone::VirtualOutput::Missing,
            );
            inspection
        });
        assert_eq!(cache.snapshot().state, MicrophonePluginState::NotInstalled);
        assert!(!cache.snapshot().feed_loaded);
        assert_eq!(cache.state.lock().unwrap().value.render_id, None);
    }
    #[test]
    fn device_notifications_do_not_finish_an_active_operation() {
        let cache = Cache::default();
        cache.refresh(installed);
        cache.begin(Action::Install).unwrap();
        for _ in 0..100 {
            cache.invalidate(false);
        }
        cache.refresh(|| panic!("device event must not inspect an active install"));
        assert_eq!(cache.snapshot().operation, MicrophonePluginOperation::Installing);
        cache.invalidate(true);
        cache.refresh(installed);
        assert_eq!(cache.snapshot().operation, MicrophonePluginOperation::Idle);
        assert!(!cache.snapshot().refreshing);
    }
    #[test]
    fn reads_cached_value_while_slow_refresh_is_in_flight() {
        let cache = Cache::default();
        cache.refresh(installed);
        std::thread::scope(|scope| {
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let cache_ref = &cache;
            scope.spawn(move || {
                cache_ref.refresh(|| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    MicrophonePluginStatus {
                        state: MicrophonePluginState::NotInstalled,
                        ..Default::default()
                    }
                    .into()
                })
            });
            entered_rx.recv().unwrap();
            cache.refresh(|| panic!("second refresh must be coalesced"));
            let value = cache.snapshot();
            assert_eq!(value.state, MicrophonePluginState::Installed);
            assert!(value.refreshing);
            release_tx.send(()).unwrap();
        });
        assert_eq!(cache.snapshot().state, MicrophonePluginState::NotInstalled);
        assert!(!cache.snapshot().refreshing);
    }
    #[test]
    fn operation_invalidates_old_query_and_completion_refreshes() {
        let cache = Cache::default();
        cache.refresh(installed);
        cache.refresh(|| {
            cache.begin(Action::Uninstall).unwrap();
            assert!(cache.begin(Action::Install).is_err());
            installed()
        });
        assert_eq!(cache.snapshot().operation, MicrophonePluginOperation::Uninstalling);
        cache.refresh(|| panic!("must not inspect while operation is pending"));
        cache.invalidate(true);
        assert!(cache.snapshot().refreshing);
        cache.refresh(|| {
            MicrophonePluginStatus {
                state: MicrophonePluginState::NotInstalled,
                last_operation_succeeded: Some(true),
                ..Default::default()
            }
            .into()
        });
        let value = cache.snapshot();
        assert_eq!(value.operation, MicrophonePluginOperation::Idle);
        assert_eq!(value.last_operation_succeeded, Some(true));
        assert_eq!(value.state, MicrophonePluginState::NotInstalled);
    }
}
