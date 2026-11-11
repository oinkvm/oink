//! An install request owns the main window until failure or process replacement.

/// Decode inside the command boundary so rejected updater arguments have a
/// logged, typed receipt instead of Tauri's unstructured argument-error string.
pub(crate) fn decode(request: serde_json::Value) -> Result<oink_agent::api::Request, oink_agent::api::ApiError> {
    let sensitive = request.get("method").and_then(|value| value.as_str()) == Some("save_auto_unlock");
    let updating = request.get("method").and_then(|value| value.as_str()) == Some("update");
    serde_json::from_value(request).map_err(|error| {
        if sensitive {
            return oink_agent::api::ApiError::InvalidArgument {
                message: "invalid password request".into(),
            };
        }
        tracing::warn!(step = "request_decode", %error, "API request rejected");
        if updating {
            oink_agent::api::ApiError::UpdateFailed {
                update_error: oink_agent::api::UpdateError::Metadata,
            }
        } else {
            oink_agent::api::ApiError::InvalidArgument {
                message: error.to_string(),
            }
        }
    })
}
pub(crate) fn dispatch<T, E>(
    install: bool,
    hide: impl FnOnce() -> Result<(), E>,
    request: impl FnOnce() -> Result<T, E>,
    failed: impl FnOnce(),
) -> Result<T, E> {
    if install && let Err(error) = hide() {
        failed();
        return Err(error);
    }
    let result = request();
    if install && result.is_err() {
        failed();
    }
    result
}
#[derive(Default)]
pub(crate) struct Owner {
    pending: std::sync::atomic::AtomicBool,
    failed: std::sync::atomic::AtomicBool,
}
impl Owner {
    pub fn begin(&self) {
        self.failed.store(false, std::sync::atomic::Ordering::Release);
        self.pending.store(true, std::sync::atomic::Ordering::Release);
    }
    pub fn failure(&self) -> bool {
        let pending = self.pending.swap(false, std::sync::atomic::Ordering::AcqRel);
        if pending {
            self.failed.store(true, std::sync::atomic::Ordering::Release);
        }
        pending
    }
    pub fn take_failure(&self) -> bool {
        self.failed.swap(false, std::sync::atomic::Ordering::AcqRel)
    }
}

/// What this shell process does by itself with update status: install a feed
/// download once it is ready when the user asked for that, and show the window
/// for a critical update found by the launch check. Background checks later in
/// the session leave a hidden window hidden; the next launch or window open
/// shows the dialog.
#[derive(Default)]
pub(crate) struct Follow {
    installed: std::sync::Mutex<Option<u64>>,
    launch: std::sync::Mutex<Launch>,
}
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Launch {
    #[default]
    AwaitingCheck,
    Checking,
    Done,
}
impl Follow {
    /// The build to install now; each build at most once per process.
    pub fn install(&self, status: &oink_agent::api::UpdateStatus) -> Option<u64> {
        let candidate = status.candidate.as_ref()?;
        if status.phase != oink_agent::update::Phase::Ready || candidate.local_package || !status.install_after_download
        {
            return None;
        }
        let mut installed = self.installed.lock().unwrap_or_else(|e| e.into_inner());
        (*installed != Some(candidate.build)).then(|| {
            *installed = Some(candidate.build);
            candidate.build
        })
    }
    pub fn prompt(&self, status: &oink_agent::api::UpdateStatus) -> bool {
        let mut launch = self.launch.lock().unwrap_or_else(|e| e.into_inner());
        if *launch == Launch::Done {
            return false;
        }
        let unprompted = status.candidate.as_ref().is_some_and(|candidate| {
            candidate.kind == oink_agent::api::UpdateKind::Critical
                && !candidate.local_package
                && status.critical_prompted_build != Some(candidate.build)
        });
        *launch = if unprompted {
            Launch::Done
        } else if status.phase == oink_agent::update::Phase::Checking {
            Launch::Checking
        } else if *launch == Launch::Checking {
            Launch::Done
        } else {
            *launch
        };
        unprompted
    }
}

#[cfg(test)]
mod follow_tests {
    use super::*;
    use oink_agent::{api::UpdateKind, update::Phase};
    fn status(phase: Phase, kind: UpdateKind, build: u64) -> oink_agent::api::UpdateStatus {
        serde_json::from_value(serde_json::json!({
            "current_build": 305001, "version": "0.3.5", "checks_enabled": true, "channel": "stable",
            "update_channel": null, "install_after_download": true, "phase": phase,
            "candidate": {"build": build, "version": "0.3.6", "size": 1, "published_at": null,
                "severity": "normal", "notes": null, "kind": kind, "notice": null},
            "checked_at": null, "next_check": null, "deferred_until": null, "blockers": [], "progress": null,
            "error": null, "failed_build": null, "critical_prompted_build": null, "scope": "user", "can_change_policy": true
        }))
        .unwrap()
    }
    #[test]
    fn a_ready_feed_download_installs_once_when_asked() {
        let follow = Follow::default();
        assert_eq!(follow.install(&status(Phase::Available, UpdateKind::Normal, 2)), None);
        assert_eq!(follow.install(&status(Phase::Ready, UpdateKind::Normal, 2)), Some(2));
        assert_eq!(follow.install(&status(Phase::Ready, UpdateKind::Normal, 2)), None);
        assert_eq!(follow.install(&status(Phase::Ready, UpdateKind::Normal, 3)), Some(3));
        let mut off = status(Phase::Ready, UpdateKind::Normal, 4);
        off.install_after_download = false;
        assert_eq!(follow.install(&off), None);
        let mut local = status(Phase::Ready, UpdateKind::Normal, 5);
        local.candidate.as_mut().unwrap().local_package = true;
        assert_eq!(follow.install(&local), None);
    }
    #[test]
    fn only_the_launch_check_shows_the_window_for_an_unprompted_critical_update() {
        let follow = Follow::default();
        assert!(!follow.prompt(&status(Phase::Idle, UpdateKind::Normal, 2)));
        assert!(!follow.prompt(&status(Phase::Checking, UpdateKind::Normal, 2)));
        assert!(follow.prompt(&status(Phase::Available, UpdateKind::Critical, 2)));
        assert!(!follow.prompt(&status(Phase::Available, UpdateKind::Critical, 3)));
        // The launch check found nothing: a later background check does not show the window.
        let follow = Follow::default();
        assert!(!follow.prompt(&status(Phase::Checking, UpdateKind::Normal, 2)));
        assert!(!follow.prompt(&status(Phase::Available, UpdateKind::Normal, 2)));
        assert!(!follow.prompt(&status(Phase::Available, UpdateKind::Critical, 3)));
        // Already prompted for this build.
        let follow = Follow::default();
        let mut prompted = status(Phase::Available, UpdateKind::Critical, 2);
        prompted.critical_prompted_build = Some(2);
        assert!(!follow.prompt(&prompted));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    #[test]
    fn invalid_password_requests_never_echo_the_submitted_value() {
        let error = decode(
            serde_json::json!({"method":"save_auto_unlock","params":{"password":{"synthetic-only-secret":true}}}),
        )
        .unwrap_err();
        assert_eq!(error.message(), "invalid password request");
        assert!(!format!("{error:?}").contains("synthetic-only-secret"));
    }
    #[test]
    fn install_arguments_decode_before_any_window_or_backend_action() {
        use oink_agent::api::{ApiError, Request, UpdateCommand, UpdateError};
        let request =
            serde_json::json!({"method":"update","params":{"command":{"type":"install","expected_build":65}}});
        assert_eq!(
            decode(request).unwrap(),
            Request::Update {
                command: UpdateCommand::Install { expected_build: 65 }
            }
        );
        for command in [
            serde_json::json!({"type":"install"}),
            serde_json::json!({"type":"install","expected_build":"65"}),
            serde_json::json!({"type":"install","expected_build":65,"extra":true}),
        ] {
            assert_eq!(
                decode(serde_json::json!({"method":"update","params":{"command":command}})),
                Err(ApiError::UpdateFailed {
                    update_error: UpdateError::Metadata
                })
            );
        }
    }
    #[test]
    fn install_closes_main_before_the_backend_and_reopens_on_rejection() {
        let calls = RefCell::new(Vec::new());
        let result = dispatch(
            true,
            || {
                calls.borrow_mut().push("hide");
                Ok(())
            },
            || {
                calls.borrow_mut().push("request");
                Err::<(), _>("rejected")
            },
            || calls.borrow_mut().push("reopen failed update"),
        );
        assert_eq!(result, Err("rejected"));
        assert_eq!(*calls.borrow(), ["hide", "request", "reopen failed update"]);
    }
    #[test]
    fn successful_install_keeps_the_old_main_closed_until_relaunch() {
        let calls = RefCell::new(Vec::new());
        dispatch(
            true,
            || {
                calls.borrow_mut().push("hide");
                Ok(())
            },
            || {
                calls.borrow_mut().push("request");
                Ok::<(), ()>(())
            },
            || calls.borrow_mut().push("reopen failed update"),
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["hide", "request"]);
    }
    #[test]
    fn background_checks_do_not_change_windows() {
        dispatch(false, || panic!("no hide"), || Ok::<(), ()>(()), || panic!("no reopen")).unwrap();
    }
    #[test]
    fn only_an_explicit_install_reopens_a_failed_dialog_once() {
        let owner = Owner::default();
        assert!(!owner.failure());
        assert!(!owner.take_failure());
        owner.begin();
        assert!(owner.failure());
        assert!(!owner.failure());
        assert!(owner.take_failure());
        assert!(!owner.take_failure());
    }
}
