//! Which desktop this Windows backend captures on, and what it may create there.
//!
//! The user backend (UIAccess, interactive user) captures on `Default`. The
//! pre-login backend (SYSTEM, started by OinkInput on `winsta0\Winlogon`) captures
//! on the logon desktop. It creates no visible window there (only message-only
//! and never-shown windows), so the cursor is parked visibly instead of covered,
//! and the foreground-capture windows are not used. It also
//! withdraws the local source while a password field has focus (see
//! [`super::credential_focus`]).
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Profile {
    /// Name of the only desktop on which local input is a source.
    pub capture_desktop: &'static str,
    /// Withdraw the local source while the focused element is a password field.
    pub credential_gate: bool,
    /// May show windows (cursor cover, foreground capture).
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

impl Profile {
    /// A disconnected session's OpenInputDesktop can describe its old desktop.
    /// It must never make the SYSTEM source active on a different console.
    pub(crate) fn source_allowed(self, desktop: &str, active_console: bool) -> bool {
        desktop == self.capture_desktop && (self != PRELOGIN || active_console)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prelogin_never_captures_default_or_a_disconnected_console() {
        assert!(PRELOGIN.source_allowed("Winlogon", true));
        assert!(!PRELOGIN.source_allowed("Default", true));
        assert!(!PRELOGIN.source_allowed("Winlogon", false));
        assert!(USER.source_allowed("Default", true));
    }
}
