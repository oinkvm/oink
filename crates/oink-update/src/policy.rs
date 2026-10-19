use crate::{Error, barrier::Blocker, feed::Severity};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    #[default]
    Stable,
    Beta,
}
impl Channel {
    pub fn name(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Notify,
    Automatic,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferences {
    pub checks_enabled: bool,
    pub mode: Mode,
    pub channel: Channel,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            checks_enabled: true,
            mode: Mode::Notify,
            channel: if crate::RELEASE_VERSION.contains("-beta") {
                Channel::Beta
            } else {
                Channel::Stable
            },
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferChoice {
    Hour,
    Tomorrow,
    NextQuit,
    Indefinitely,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(deserialize_with = "policy_schema")]
    schema: u32,
    pub preferences: Preferences,
    pub deferred_until: Option<i64>,
    pub next_quit: bool,
    pub indefinitely: bool,
    pub skipped_build: Option<u64>,
}
fn policy_schema<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    let schema = u32::deserialize(deserializer)?;
    if schema != 1 {
        return Err(serde::de::Error::custom("unsupported update policy schema"));
    }
    Ok(schema)
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            schema: 1,
            preferences: Preferences::default(),
            deferred_until: None,
            next_quit: false,
            indefinitely: false,
            skipped_build: None,
        }
    }
}
impl Policy {
    pub fn defer(&mut self, choice: DeferChoice, now: i64) {
        self.deferred_until = match choice {
            DeferChoice::Hour => now.checked_add(3600),
            DeferChoice::Tomorrow => now.checked_add(86400),
            _ => None,
        };
        self.next_quit = choice == DeferChoice::NextQuit;
        self.indefinitely = choice == DeferChoice::Indefinitely;
    }
    pub fn may_download_automatically(&self, build: u64, now: i64) -> bool {
        self.preferences.checks_enabled
            && self.preferences.mode == Mode::Automatic
            && !self.indefinitely
            && !self.next_quit
            && self.deferred_until.is_none_or(|until| now >= until)
            && self.skipped_build != Some(build)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Disabled,
    Idle,
    Checking,
    Available,
    Downloading,
    Verifying,
    WaitingForIdle,
    Quiescing,
    Installing,
    AwaitingReboot,
    InstallerBusy,
    Committed,
    RolledBack,
    RepairRequired,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    User,
    Machine,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub build: u64,
    pub version: String,
    pub size: u64,
    pub published_at: Option<i64>,
    pub severity: Severity,
    pub notes: crate::feed::ReleaseNotes,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateStatus {
    pub current_build: u64,
    pub version: String,
    pub channel: Channel,
    pub mode: Mode,
    pub checks_enabled: bool,
    pub phase: Phase,
    pub candidate: Option<Candidate>,
    pub checked_at: Option<i64>,
    pub next_check: Option<i64>,
    pub deferred_until: Option<i64>,
    pub blockers: Vec<Blocker>,
    pub progress: Option<u8>,
    pub error: Option<Error>,
    pub scope: Scope,
    pub can_change_policy: bool,
}
/// Local IPC only. A build is a stale-UI guard, never authority to select a URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Check,
    Download,
    Install { expected_build: u64 },
    Defer { choice: DeferChoice },
    Skip { expected_build: u64 },
    SetPreferences { preferences: Preferences },
    Cancel,
}

/// Sparkle's ready continuation can install on ordinary quit. Skip cancels that
/// particular installer without persistently skipping its build; Dismiss does not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReadyState {
    #[default]
    None,
    Held,
    Cancelling,
    Authorized,
}
impl ReadyState {
    pub fn ready(&mut self) {
        *self = Self::Held;
    }
    pub fn cancel(&mut self) -> bool {
        if *self == Self::Held {
            *self = Self::Cancelling;
            true
        } else {
            false
        }
    }
    pub fn dismissed(&mut self) {
        *self = Self::None;
    }
    pub fn authorize(&mut self) -> Result<(), Error> {
        if *self != Self::Held {
            return Err(Error::Transaction);
        }
        *self = Self::Authorized;
        Ok(())
    }
    pub fn ordinary_quit_allowed(self) -> bool {
        matches!(self, Self::None)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_and_off_never_auto_download() {
        let mut p = Policy::default();
        assert!(!p.may_download_automatically(2, 0));
        p.preferences.mode = Mode::Automatic;
        assert!(p.may_download_automatically(2, 0));
        p.preferences.checks_enabled = false;
        assert!(!p.may_download_automatically(2, 0));
    }
    #[test]
    fn deferrals_and_skip_survive_serialization() {
        let mut p = Policy::default();
        p.preferences.mode = Mode::Automatic;
        p.defer(DeferChoice::Hour, 5);
        p.skipped_build = Some(2);
        let p: Policy = serde_json::from_slice(&serde_json::to_vec(&p).unwrap()).unwrap();
        assert!(!p.may_download_automatically(3, 3604));
        assert!(p.may_download_automatically(3, 3605));
        assert!(!p.may_download_automatically(2, 3605));
    }
    #[test]
    fn mode_change_requires_cancel_ack_before_quit() {
        let mut s = ReadyState::None;
        s.ready();
        assert!(!s.ordinary_quit_allowed());
        assert!(s.cancel());
        assert!(!s.ordinary_quit_allowed());
        assert!(s.authorize().is_err());
        s.dismissed();
        assert!(s.ordinary_quit_allowed());
    }
}

#[cfg(test)]
mod storage_tests {
    use super::*;
    #[test]
    fn only_current_policy_envelope_is_accepted() {
        let policy = Policy::default();
        let mut value = serde_json::to_value(&policy).unwrap();
        assert_eq!(value["schema"], 1);
        assert_eq!(serde_json::from_value::<Policy>(value.clone()).unwrap(), policy);
        value["schema"] = 2.into();
        assert!(serde_json::from_value::<Policy>(value.clone()).is_err());
        value.as_object_mut().unwrap().remove("schema");
        assert!(serde_json::from_value::<Policy>(value).is_err());
    }
}
