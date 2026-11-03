//! Which desktop this Windows backend captures on, and what it may create there.
//!
//! The user backend (UIAccess, interactive user) captures on `Default`. The
//! pre-login backend (SYSTEM, started by OinkInput on `winsta0\Winlogon`) captures
//! on the logon desktop. It creates no visible window there (only message-only
//! and never-shown windows), so the cursor is parked visibly instead of covered,
//! and the foreground-capture and application-focus windows are not used. It also
//! withdraws the local source while a password field has focus (see
//! [`super::credential_focus`]).
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Profile {
    /// Name of the only desktop on which local input is a source.
    pub capture_desktop: &'static str,
    /// Withdraw the local source while the focused element is a password field.
    pub credential_gate: bool,
    /// May show windows (cursor cover, foreground capture, application focus).
    pub visible_windows: bool,
}
pub(crate) const USER: Profile = Profile {
    capture_desktop: "Default",
    credential_gate: false,
    visible_windows: true,
};
pub(crate) const PRELOGIN: Profile = Profile {
    capture_desktop: "Winlogon",
    credential_gate: true,
    visible_windows: false,
};
static PROFILE: OnceLock<Profile> = OnceLock::new();

pub(crate) fn current() -> Profile {
    *PROFILE.get().unwrap_or(&USER)
}
/// Fixed once per process, before the backend exists.
pub(crate) fn select(profile: Profile) -> Result<(), crate::InputError> {
    if PROFILE.get_or_init(|| profile) == &profile {
        Ok(())
    } else {
        Err(crate::InputError::Platform(
            "Windows input profile already selected".into(),
        ))
    }
}
