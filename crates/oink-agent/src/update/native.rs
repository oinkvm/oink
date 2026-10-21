use super::*;
use std::ffi::{CStr, c_char};
unsafe extern "C" {
    fn oink_update_start(
        callback: extern "C" fn(u32, u64, u64, u32, *const c_char, *const c_char),
        timing: extern "C" fn(u32, u64, u64, u64, u64),
        beta: bool,
        checks: bool,
    ) -> bool;
    fn oink_update_check();
    fn oink_update_download();
    fn oink_update_cancel();
    fn oink_update_install() -> bool;
    fn oink_update_quit_allowed() -> bool;
    fn oink_update_preferences(checks: bool, beta: bool);
    fn oink_update_checks_enabled() -> bool;
    fn oink_update_current_build() -> u64;
    fn oink_update_timing(boundary: u32);
}
pub fn start(p: Preferences) {
    if !oink_update::trust::mac_enabled() {
        return;
    }
    // SAFETY: backend main-thread dispatch; callback has static lifetime.
    let configured = unsafe { oink_update_start(event, timing, p.channel == Channel::Beta, p.checks_enabled) };
    if configured && let Ok(node) = crate::backend::node() {
        let mut state = lock(&node.inner.update);
        state.status.error = None;
        state.status.phase = Phase::Idle;
        // SAFETY: updater created above; access remains on main thread.
        unsafe {
            state.status.current_build = oink_update_current_build();
            state.status.checks_enabled = oink_update_checks_enabled();
        }
        state.policy.preferences.checks_enabled = state.status.checks_enabled;
        // Startup is asynchronous relative to the UI's bootstrap query. Publish
        // enablement too, so a window opened early does not stay manual-only.
        let status = state.status.clone();
        drop(state);
        node.emit(ApiEvent::UpdateStatusChanged(status));
    }
}
pub fn check() {
    // SAFETY: all native commands run through backend::on_main.
    unsafe {
        oink_update_check();
    }
}
pub fn download() {
    // SAFETY: main-thread driver owns continuation.
    unsafe {
        oink_update_download();
    }
}
pub fn cancel() {
    // SAFETY: main-thread driver owns cancellation.
    unsafe {
        oink_update_cancel();
    }
}
pub fn install() -> bool {
    // SAFETY: caller validated the backend drain lease.
    unsafe { oink_update_install() }
}
pub fn install_clicked() {
    // SAFETY: only the native synchronized logger is accessed, not the updater.
    unsafe { oink_update_timing(6) }
}
pub fn quiesce_done() {
    // SAFETY: only the native synchronized logger is accessed, not the updater.
    unsafe { oink_update_timing(7) }
}
extern "C" fn timing(boundary: u32, monotonic_us: u64, elapsed_us: u64, previous_elapsed_us: u64, bytes: u64) {
    let phase = match boundary {
        1 => "download_started",
        2 => "download_finished",
        3 => "extraction_started",
        4 => "extraction_input_complete",
        5 => "ready_to_install",
        6 => "install_clicked",
        7 => "quiesce_done",
        8 => "sparkle_reply",
        9 => "sparkle_will_extract",
        10 => "sparkle_did_extract",
        11 => "sparkle_will_install",
        12 => "sparkle_will_relaunch",
        _ => return,
    };
    // A resumed cached installer has no observable download start in this
    // process. Preserve that absence rather than inventing a download duration.
    let download_elapsed_us = (elapsed_us != u64::MAX).then_some(elapsed_us);
    tracing::info!(target: "oink_update_timing", phase, monotonic_us, ?download_elapsed_us, previous_elapsed_us, bytes, "Mac update boundary");
}
pub fn quit_allowed() -> bool {
    // SAFETY: called from AppKit main thread.
    unsafe { oink_update_quit_allowed() }
}
pub fn preferences(p: Preferences) {
    // SAFETY: called from AppKit main thread after policy persistence.
    unsafe {
        oink_update_preferences(p.checks_enabled, p.channel == Channel::Beta);
    }
}
extern "C" fn event(kind: u32, build: u64, size: u64, progress: u32, version: *const c_char, notes: *const c_char) {
    let Ok(node) = crate::backend::node() else {
        return;
    };
    let mut state = lock(&node.inner.update);
    state.status.progress = Some(progress.min(100) as u8);
    match kind {
        0 => {
            state.status.phase = Phase::Idle;
            state.status.checked_at = Some(now());
            state.status.error = None;
        }
        1 => {
            state.status.phase = Phase::Checking;
            state.status.error = None;
        }
        2 => {
            // SAFETY: native bridge passes valid, borrowed UTF-8 strings for this call.
            let (version, notes) = unsafe {
                (
                    CStr::from_ptr(version).to_string_lossy().into_owned(),
                    oink_update::feed::ReleaseNotes::from_json(CStr::from_ptr(notes).to_bytes()),
                )
            };
            let Ok(notes) = notes else {
                state.status.candidate = None;
                state.status.error = Some(Error::Metadata);
                state.status.phase = Phase::Failed;
                state.ready = false;
                crate::backend::on_main(cancel);
                let status = state.status.clone();
                drop(state);
                node.emit(ApiEvent::UpdateStatusChanged(status));
                return;
            };
            state.status.candidate = Some(Candidate {
                build,
                version,
                size,
                published_at: None,
                severity: oink_update::feed::Severity::Normal,
                notes,
            });
            state.status.phase = Phase::Available;
            state.status.error = None;
            state.status.checked_at = Some(now());
        }
        3 => state.status.phase = Phase::Downloading,
        4 => state.status.phase = Phase::Verifying,
        5 => {
            state.ready = true;
            state.status.phase = Phase::Ready;
            state.status.blockers.clear();
        }
        6 => state.status.phase = Phase::Installing,
        7 => state.status.phase = Phase::Committed,
        8 => {
            installation_failed();
            state.installing = false;
            state.ready = false;
            state.status.phase = Phase::Failed;
            state.status.error = Some(match progress {
                1 => Error::Network,
                4 => Error::OsUnsupported,
                _ => Error::Signature,
            });
        }
        9 => {
            // Sparkle dismisses on both abort and completion, with or without
            // showUpdaterError. Always release our single approval token.
            installation_failed();
            state.dismissed();
        }
        _ => return,
    }
    let status = state.status.clone();
    drop(state);
    node.emit(ApiEvent::UpdateStatusChanged(status));
}
