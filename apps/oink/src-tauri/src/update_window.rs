//! An install request owns the main window until failure or process replacement.
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
