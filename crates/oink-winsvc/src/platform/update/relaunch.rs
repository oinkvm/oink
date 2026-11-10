//! Reopen Oink after a standalone MSI (issue #36).
//!
//! A user who double-clicks an MSI (or runs msiexec at any UI level) on a
//! device controlled through Oink loses that control when the installer asks
//! Oink to quit. The installer's PrepareUserSessions records each session/user
//! whose Oink was running; its commit-only RelaunchUserSessions action
//! publishes `relaunch.json` (packaging/windows/Relaunch.h) in this protected
//! directory once the whole transaction committed. The update engine is
//! already woken by that rename and consumes the request exactly once, opening
//! the ordinary UI entry with the session's own logon token, like the updater's
//! post-update reopen. In-app updates never publish it: their worker owns the
//! relaunch and the installer skips while its pending journal exists.
use super::{super::security, protected};
use oink_update::{Error, feed::strict_json};
use serde::Deserialize;

const FILE: &str = "relaunch.json";

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Request {
    schema: u32,
    sessions: Vec<Target>,
}
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Target {
    id: u32,
    sid: String,
}

fn parse(bytes: &[u8]) -> Result<Request, Error> {
    let request: Request = strict_json(bytes)?;
    if request.schema != 1 {
        return Err(Error::Metadata);
    }
    Ok(request)
}

/// Sessions to open: those whose current logon user is the recorded user. A
/// logged-off session, a different user on a reused session id, or another
/// account's runas process in the session is not reopened.
fn selected(request: &Request, mut logon_user: impl FnMut(u32) -> Option<String>) -> Vec<u32> {
    let mut sessions = Vec::new();
    for target in &request.sessions {
        if !sessions.contains(&target.id) && logon_user(target.id).as_deref() == Some(target.sid.as_str()) {
            sessions.push(target.id);
        }
    }
    sessions
}

pub(super) fn consume() {
    let Ok(path) = protected::maintenance().map(|directory| directory.join(FILE)) else {
        return;
    };
    if !path.exists() {
        return;
    }
    let request = protected::read(&path, 65536).and_then(|bytes| parse(&bytes));
    // One shot whatever happens next: a request must never reopen Oink again
    // after a later restart of this service or of Windows.
    if let Err(error) = std::fs::remove_file(&path) {
        tracing::error!(%error, "cannot remove the installer relaunch request");
        return;
    }
    let request = match request {
        Ok(request) => request,
        Err(error) => {
            tracing::warn!(%error, "ignoring an invalid installer relaunch request");
            return;
        }
    };
    let opened = selected(&request, |id| {
        security::user_sid(id).and_then(|sid| security::sid_string(&sid)).ok()
    });
    tracing::info!(
        requested = request.sessions.len(),
        ?opened,
        "reopening Oink after a standalone install"
    );
    for id in opened {
        // The SID is re-read from WTS here; spawn_update_ui verifies it again
        // against the token it launches with.
        let launched = security::user_sid(id).and_then(|sid| super::super::backend::spawn_update_ui(id, &sid));
        match launched {
            Ok(pid) => tracing::info!(session = id, pid, "opened Oink after a standalone install"),
            Err(error) => tracing::warn!(session = id, %error, "could not reopen Oink after a standalone install"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "S-1-5-21-1-2-3-1001";
    const OTHER: &str = "S-1-5-21-1-2-3-1002";

    #[test]
    fn installer_request_format_is_strict() {
        // Byte-for-byte what relaunch::json() in packaging/windows/Relaunch.h writes.
        let request =
            parse(br#"{"schema":1,"sessions":[{"id":1,"sid":"S-1-5-21-1-2-3-1001"},{"id":2,"sid":"S-1-5-18"}]}"#)
                .unwrap();
        assert_eq!(
            request.sessions,
            [
                Target {
                    id: 1,
                    sid: USER.into()
                },
                Target {
                    id: 2,
                    sid: "S-1-5-18".into()
                }
            ]
        );
        assert!(parse(br#"{"schema":1,"sessions":[]}"#).unwrap().sessions.is_empty());
        for bad in [
            &br#"{"schema":2,"sessions":[]}"#[..],
            br#"{"schema":1}"#,
            br#"{"schema":1,"sessions":[],"launch":true}"#,
            br#"{"schema":1,"sessions":[{"id":1,"sid":"S-1-5-18","args":"--x"}]}"#,
            br#"{"schema":1,"sessions":[{"id":-1,"sid":"S-1-5-18"}]}"#,
            br#"{"schema":1,"schema":1,"sessions":[]}"#,
            b"",
        ] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn only_the_recorded_logon_user_of_a_live_session_is_reopened_once() {
        let request = Request {
            schema: 1,
            sessions: vec![
                Target {
                    id: 1,
                    sid: USER.into(),
                },
                Target {
                    id: 1,
                    sid: OTHER.into(),
                }, // runas process in the same session
                Target {
                    id: 2,
                    sid: USER.into(),
                }, // logged off since
                Target {
                    id: 3,
                    sid: USER.into(),
                }, // session id now used by another user
                Target {
                    id: 4,
                    sid: OTHER.into(),
                },
            ],
        };
        let mut queried = Vec::new();
        let opened = selected(&request, |id| {
            queried.push(id);
            match id {
                1 => Some(USER.into()),
                3 => Some(OTHER.into()),
                4 => Some(OTHER.into()),
                _ => None,
            }
        });
        assert_eq!(opened, [1, 4]);
        // The runas entry of an already selected session needs no second query.
        assert_eq!(queried, [1, 2, 3, 4]);
    }
}
