//! Explicit choices from the MSI's successful full-UI Finish page.
//! No startup default, persisted shadow preference or installer-owned Run key.

#[derive(Debug, PartialEq, Eq)]
struct Choices {
    launch: bool,
    login: bool,
}

fn parse(args: &[String]) -> Result<Option<Choices>, &'static str> {
    if args.first().map(String::as_str) != Some("--installer-finish") {
        return Ok(None);
    }
    let mut choices = Choices {
        launch: false,
        login: false,
    };
    for arg in &args[1..] {
        match arg.as_str() {
            "--launch" if !choices.launch => choices.launch = true,
            "--start-at-login" if !choices.login => choices.login = true,
            _ => return Err("invalid installer finish argument"),
        }
    }
    Ok(Some(choices))
}

fn apply(
    choices: Choices,
    mut login: impl FnMut() -> Result<(), String>,
    mut launch: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    // Each checkbox is an independent explicit user action. A system-revoked
    // login entry must not prevent the separately requested foreground launch.
    let login_result = if choices.login { login() } else { Ok(()) };
    let launch_result = if choices.launch { launch() } else { Ok(()) };
    login_result.and(launch_result)
}

/// Windows controls focus independently of process startup. A denied grant
/// must still resume the normal-user UI; an actual resume error remains fatal.
pub(crate) fn resume_user_ui<E>(
    grant: impl FnOnce() -> Result<(), E>,
    resume: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    let _ = grant();
    resume()
}

/// Called only after the UI entry's real normal-user-token guard.
#[cfg(windows)]
pub fn dispatch_args() -> Option<std::process::ExitCode> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let choices = match parse(&args) {
        Ok(None) => return None,
        Ok(Some(choices)) => choices,
        Err(error) => {
            eprintln!("{error}");
            return Some(std::process::ExitCode::FAILURE);
        }
    };
    let result = apply(
        choices,
        || {
            // Same OS-backed preference used by AgentApi::set_launch_at_login and
            // Settings. An unchecked box never disables or rewrites a prior choice.
            let status = oink_autostart::set_enabled(true)?;
            if status.state == oink_autostart::LaunchAtLoginState::Enabled {
                Ok(())
            } else {
                Err(format!("login item requires attention: {:?}", status.problem))
            }
        },
        || {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            crate::foreground_windows::launch_user_ui(&exe).map_err(|e| e.to_string())
        },
    );
    Some(match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Oink installer finish: {error}");
            std::process::ExitCode::FAILURE
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn denied_foreground_permission_still_resumes_the_ui() {
        for granted in [false, true] {
            let calls = RefCell::new(Vec::new());
            assert_eq!(
                resume_user_ui(
                    || {
                        calls.borrow_mut().push("grant");
                        if granted { Ok(()) } else { Err("focus denied") }
                    },
                    || {
                        calls.borrow_mut().push("resume");
                        Ok(())
                    }
                ),
                Ok(())
            );
            assert_eq!(*calls.borrow(), ["grant", "resume"]);
        }
        assert_eq!(
            resume_user_ui(|| Err("focus denied"), || Err("resume failed")),
            Err("resume failed")
        );
    }

    #[test]
    fn all_checkbox_combinations_only_apply_explicit_choices() {
        for launch in [false, true] {
            for login in [false, true] {
                let mut args = vec!["--installer-finish".to_owned()];
                if launch {
                    args.push("--launch".into());
                }
                if login {
                    args.push("--start-at-login".into());
                }
                let choices = parse(&args).unwrap().unwrap();
                let calls = RefCell::new(Vec::new());
                apply(
                    choices,
                    || {
                        calls.borrow_mut().push("login");
                        Ok(())
                    },
                    || {
                        calls.borrow_mut().push("launch");
                        Ok(())
                    },
                )
                .unwrap();
                let expected: Vec<_> = [(login, "login"), (launch, "launch")]
                    .into_iter()
                    .filter_map(|(on, action)| on.then_some(action))
                    .collect();
                assert_eq!(*calls.borrow(), expected);
            }
        }
    }

    #[test]
    fn ordinary_startup_never_edits_login_and_invalid_arguments_are_rejected() {
        for args in [vec![], vec!["--autostart".into()], vec!["--service".into()]] {
            assert_eq!(parse(&args), Ok(None));
        }
        for tail in [
            vec!["--unknown"],
            vec!["--launch", "--launch"],
            vec!["--start-at-login", "--start-at-login"],
        ] {
            let args = std::iter::once("--installer-finish")
                .chain(tail)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            assert!(parse(&args).is_err());
        }
    }

    #[test]
    fn login_failure_does_not_suppress_separate_launch_choice() {
        let mut launched = false;
        let result = apply(
            Choices {
                launch: true,
                login: true,
            },
            || Err("denied".into()),
            || {
                launched = true;
                Ok(())
            },
        );
        assert_eq!(result, Err("denied".into()));
        assert!(launched);
    }
}
