//! Native power observations are independent of audio samples and input routing.
#[cfg(target_os = "macos")]
#[path = "power/macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "power/windows.rs"]
mod platform;
#[cfg(any(target_os = "macos", windows))]
pub use platform::{Hold, observe};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub display: bool,
    pub system: bool,
    pub activity: bool,
}
impl State {
    pub fn union(self, other: Self) -> Self {
        Self {
            display: self.display || other.display,
            system: self.system || other.system,
            activity: self.activity || other.activity,
        }
    }
}
/// The owning PID is resolved locally; process names never leave this boundary.
pub fn oink_image(path: &str) -> bool {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    matches!(
        file.to_ascii_lowercase().as_str(),
        "oink" | "oink.exe" | "oink-agent" | "oink-agent.exe" | "oink-service" | "oink-service.exe"
    )
}
#[derive(Debug)]
pub struct Assertion<'a> {
    pub kind: &'a str,
    pub enabled: bool,
    pub pid: u32,
    pub on_behalf: Option<u32>,
}
/// Resolve attribution before combining rows, including daemon-owned audio requests.
pub fn mac_assertion(row: Assertion<'_>, own_pid: u32, image: impl Fn(u32) -> Option<String>) -> State {
    if !row.enabled || row.pid == own_pid {
        return State::default();
    }
    let Some(owner) = image(row.pid) else {
        return State::default();
    };
    if oink_image(&owner) {
        return State::default();
    }
    let basename = owner.rsplit('/').next().unwrap_or(&owner);
    if basename == "WindowServer" && row.kind == "UserIsActive" {
        return State::default();
    }
    let attributed = row.on_behalf;
    if basename == "coreaudiod" && attributed.is_none() {
        return State::default();
    }
    if let Some(pid) = attributed
        && (pid == own_pid || image(pid).is_none_or(|p| oink_image(&p)))
    {
        return State::default();
    }
    match row.kind {
        "PreventUserIdleDisplaySleep" | "NoDisplaySleepAssertion" => State {
            display: true,
            ..State::default()
        },
        "PreventUserIdleSystemSleep" | "PreventSystemSleep" | "NoIdleSleepAssertion" => State {
            system: true,
            ..State::default()
        },
        "UserIsActive" => State {
            activity: true,
            ..State::default()
        },
        _ => State::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(kind: &str, pid: u32, behalf: Option<u32>) -> State {
        mac_assertion(
            Assertion {
                kind,
                enabled: true,
                pid,
                on_behalf: behalf,
            },
            1,
            |pid| {
                Some(
                    match pid {
                        1 | 2 => "/app/Oink",
                        3 => "/system/WindowServer",
                        4 => "/system/coreaudiod",
                        _ => "/app/Player",
                    }
                    .into(),
                )
            },
        )
    }
    #[test]
    fn native_semantics_and_feedback() {
        assert_eq!(
            sample("PreventUserIdleSystemSleep", 5, None),
            State {
                system: true,
                ..State::default()
            }
        );
        assert_eq!(
            sample("PreventUserIdleDisplaySleep", 5, None),
            State {
                display: true,
                ..State::default()
            }
        );
        assert!(sample("UserIsActive", 5, None).activity);
        for (kind, pid) in [
            ("UserIsActive", 1),
            ("UserIsActive", 2),
            ("UserIsActive", 3),
            ("PreventDiskIdle", 5),
        ] {
            assert_eq!(sample(kind, pid, None), State::default());
        }
        assert_eq!(sample("PreventUserIdleSystemSleep", 4, Some(1)), State::default());
        assert_eq!(sample("PreventUserIdleSystemSleep", 4, None), State::default());
        assert!(sample("PreventUserIdleSystemSleep", 4, Some(5)).system);
        assert_eq!(sample("PreventUserIdleSystemSleep", 4, Some(2)), State::default());
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn observe() -> Result<State, String> {
    Err("power observation unavailable".into())
}
#[cfg(not(any(target_os = "macos", windows)))]
#[derive(Debug, Default)]
pub struct Hold;
#[cfg(not(any(target_os = "macos", windows)))]
impl Hold {
    pub fn apply(&mut self, _: State) -> Result<(), String> {
        Err("power application unavailable".into())
    }
}
