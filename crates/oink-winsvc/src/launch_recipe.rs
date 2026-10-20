//! The small UIAccess recipe has no alternate-token or ordinary launch branch.
use std::io;

pub(crate) fn uiaccess_token<T>(
    source: impl FnOnce() -> io::Result<T>,
    duplicate: impl FnOnce(&T) -> io::Result<T>,
    enable: impl FnOnce(&T) -> io::Result<()>,
    verify: impl FnOnce(&T, &T) -> io::Result<()>,
) -> io::Result<T> {
    let source = source()?;
    let primary = duplicate(&source)?;
    enable(&primary)?;
    verify(&source, &primary)?;
    Ok(primary)
}

pub(crate) fn validate_environment(environment: &[u16]) -> io::Result<()> {
    if environment.len() > crate::backend_launch::MAX_ENVIRONMENT
        || !environment.ends_with(&[0, 0])
        || environment[..environment.len() - 1].windows(2).any(|v| v == [0, 0])
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid backend environment block",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Backend,
    Reopen,
    UpdateUi,
}
impl Target {
    pub(crate) fn argument(self) -> &'static str {
        match self {
            Self::Backend => "--service",
            Self::Reopen => "--reopen",
            Self::UpdateUi => "",
        }
    }
    pub(crate) fn uiaccess(self) -> bool {
        self == Self::Backend
    }
}

pub(crate) fn dispatch(
    message: crate::backend_launch::BackendLaunch,
    spawn: impl FnOnce(&[u16], &[u16], Target) -> io::Result<u32>,
) -> io::Result<u32> {
    use crate::backend_launch::BackendLaunch;
    let (environment, directory, target) = match message {
        BackendLaunch::Start { environment, directory } => (environment, directory, Target::Backend),
        BackendLaunch::Reopen { environment, directory } => (environment, directory, Target::Reopen),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a backend launch request",
            ));
        }
    };
    validate_environment(&environment)?;
    if directory.len() < 2
        || directory.len() > 32768
        || directory.last() != Some(&0)
        || directory[..directory.len() - 1].contains(&0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid backend working directory",
        ));
    }
    spawn(&environment, &directory, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    #[test]
    fn update_relaunch_is_fixed_ordinary_ui_entry() {
        assert_eq!(Target::UpdateUi.argument(), "");
        assert!(!Target::UpdateUi.uiaccess());
    }

    #[test]
    fn every_token_failure_stops_the_recipe_without_fallback() {
        for failure in 0..=4 {
            let step = Cell::new(0);
            let next = || {
                let n = step.get();
                step.set(n + 1);
                if n == failure {
                    Err(io::Error::other("native error"))
                } else {
                    Ok(())
                }
            };
            let result = uiaccess_token(|| next().map(|()| 1), |_| next().map(|()| 2), |_| next(), |_, _| next());
            assert_eq!(result.is_ok(), failure == 4);
            assert_eq!(step.get(), (failure + 1).min(4));
            if let Ok(token) = result {
                assert_eq!(token, 2);
            }
        }
    }

    #[test]
    fn environment_is_one_bounded_double_terminated_block() {
        assert!(
            validate_environment(
                &"OINK_HOME=C:\\data\0PATH=C:\\Windows\0\0"
                    .encode_utf16()
                    .collect::<Vec<_>>()
            )
            .is_ok()
        );
        for bad in [vec![], vec![0], vec![65, 0], vec![0, 0, 65, 0, 0], vec![65; 65537]] {
            assert!(validate_environment(&bad).is_err());
        }
    }
    #[test]
    fn launch_routes_only_fixed_backend_or_ordinary_reopen_and_preserves_context() {
        use crate::backend_launch::BackendLaunch;
        let environment: Vec<_> = "OINK_HOME=C:\\test data\0\0".encode_utf16().collect();
        let directory: Vec<_> = "C:\\working directory\0".encode_utf16().collect();
        for target in [Target::Backend, Target::Reopen] {
            let request = if target == Target::Backend {
                BackendLaunch::Start {
                    environment: environment.clone(),
                    directory: directory.clone(),
                }
            } else {
                BackendLaunch::Reopen {
                    environment: environment.clone(),
                    directory: directory.clone(),
                }
            };
            let request = BackendLaunch::decode(&request.encode().unwrap()).unwrap();
            let result = dispatch(request, |actual_env, actual_dir, actual_target| {
                assert_eq!(actual_env, environment);
                assert_eq!(actual_dir, directory);
                assert_eq!(actual_target, target);
                assert_eq!(actual_target.uiaccess(), target == Target::Backend);
                assert_eq!(
                    actual_target.argument(),
                    if target == Target::Backend {
                        "--service"
                    } else {
                        "--reopen"
                    }
                );
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "token/create failure"))
            });
            assert_eq!(result.unwrap_err().to_string(), "token/create failure");
        }
        for invalid in [
            BackendLaunch::Started { pid: 42 },
            BackendLaunch::Failed {
                error: "no service".into(),
            },
            BackendLaunch::Start {
                environment: vec![0],
                directory: directory.clone(),
            },
            BackendLaunch::Start {
                environment,
                directory: vec![65, 0, 66, 0],
            },
        ] {
            assert!(dispatch(invalid, |_, _, _| panic!("invalid request must not launch")).is_err());
        }
    }
}
