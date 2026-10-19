//! Fallible terminal output, separate from filesystem and IPC errors.
use std::fmt;
use std::io::{self, Write};

#[derive(Debug, thiserror::Error)]
#[error("command output pipe closed")]
pub struct ClosedPipe;

fn is_closed_pipe(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::BrokenPipe {
        return true;
    }
    // Windows may return ERROR_NO_DATA when the pipe's reader has gone away.
    #[cfg(windows)]
    if error.raw_os_error() == Some(windows::Win32::Foundation::ERROR_NO_DATA.0 as i32) {
        return true;
    }
    false
}

pub fn check(result: io::Result<()>) -> anyhow::Result<()> {
    result.map_err(|error| {
        if is_closed_pipe(&error) {
            ClosedPipe.into()
        } else {
            error.into()
        }
    })
}

pub fn stdout(args: fmt::Arguments<'_>) -> anyhow::Result<()> {
    let mut out = io::stdout().lock();
    check(writeln!(out, "{args}").and_then(|()| out.flush()))
}

pub fn stderr(args: fmt::Arguments<'_>) -> anyhow::Result<()> {
    let mut out = io::stderr().lock();
    check(writeln!(out, "{args}").and_then(|()| out.flush()))
}

pub fn with_stdout(write: impl FnOnce(&mut dyn Write) -> anyhow::Result<()>) -> anyhow::Result<()> {
    with_writer(&mut io::stdout().lock(), write)
}

fn with_writer(out: &mut dyn Write, write: impl FnOnce(&mut dyn Write) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let mut out = PipeAware { out, closed: false };
    let result = write(&mut out).and_then(|()| out.flush().map_err(Into::into));
    // Serializers can wrap write failures in their own error types. Remember
    // terminal failures at the writer, without suppressing an IPC BrokenPipe.
    if out.closed { Err(ClosedPipe.into()) } else { result }
}

struct PipeAware<'a> {
    out: &'a mut dyn Write,
    closed: bool,
}

impl Write for PipeAware<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.out
            .write(buf)
            .inspect_err(|error| self.closed |= is_closed_pipe(error))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out
            .flush()
            .inspect_err(|error| self.closed |= is_closed_pipe(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_terminal_pipe_errors_are_quiet() {
        assert!(
            check(Err(io::ErrorKind::BrokenPipe.into()))
                .unwrap_err()
                .is::<ClosedPipe>()
        );
        let error = check(Err(io::ErrorKind::PermissionDenied.into())).unwrap_err();
        assert!(!error.is::<ClosedPipe>());
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        let error = with_writer(&mut Vec::new(), |_| {
            Err(io::Error::from(io::ErrorKind::BrokenPipe).into())
        })
        .unwrap_err();
        assert!(!error.is::<ClosedPipe>(), "IPC failures must still be reported");
    }

    #[test]
    fn serialized_output_pipe_error_does_not_enter_fatal_capture() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capsule");
        let capture = oink_crash::capsule::Capture::install();
        capture
            .arm(
                &path,
                1,
                oink_telemetry::Component::Agent,
                oink_telemetry::BuildContext {
                    app_version: oink_telemetry::AppVersion {
                        major: 0,
                        minor: 1,
                        patch: 0,
                        build: 1,
                    },
                    os_version_class: oink_telemetry::OsVersionClass::Unknown,
                },
            )
            .unwrap();
        let (reader, mut writer) = io::pipe().unwrap();
        drop(reader);
        let result = oink_crash::capsule::fatal_boundary(|| {
            with_writer(&mut writer, |out| {
                serde_json::to_writer(out, &serde_json::json!({"event": "ready"}))?;
                Ok(())
            })
        });
        assert!(result.unwrap_err().is::<ClosedPipe>());
        assert!(std::fs::read(&path).unwrap().iter().all(|byte| *byte == 0));
        capture.disarm();
        assert!(!path.exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_no_data_is_a_closed_output_pipe() {
        let error = io::Error::from_raw_os_error(windows::Win32::Foundation::ERROR_NO_DATA.0 as i32);
        assert!(check(Err(error)).unwrap_err().is::<ClosedPipe>());
    }
}
