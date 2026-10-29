//! An install request owns the main window until failure or process replacement.

/// Decode inside the command boundary so rejected updater arguments have a
/// logged, typed receipt instead of Tauri's unstructured argument-error string.
pub(crate) fn decode(request: serde_json::Value) -> Result<oink_agent::api::Request, oink_agent::api::ApiError> {
    let updating = request.get("method").and_then(|value| value.as_str()) == Some("update");
    serde_json::from_value(request).map_err(|error| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
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
