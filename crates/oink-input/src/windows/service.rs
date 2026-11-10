//! Installation mode is selected before IPC; ordinary per-user builds stay local.
use crate::InputError;
pub use client::ServiceStatus;
use oink_foundation::Point;
use oink_winsvc::{client, wire::Command};
use std::{io, sync::OnceLock};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
/// Explicit diagnostic probe: always contacts the service, including from untrusted paths.
pub fn probe() -> Result<ServiceStatus, String> {
    client::probe().map_err(|e| e.to_string())
}
/// Explicit service cleanup; ordinary backend teardown uses release_if_connected instead.
pub fn release() -> Result<(), String> {
    client::release().map_err(|e| e.to_string())
}
/// Explicit service cursor query; InputBackend::cursor_position honors installation mode.
pub fn cursor_position() -> Result<Point, String> {
    client::query(Command::Position)
        .map(|s| Point::new(f64::from(s.x), f64::from(s.y)))
        .map_err(|e| e.to_string())
}
pub(super) fn installed() -> io::Result<bool> {
    static INSTALLATION: OnceLock<Result<bool, String>> = OnceLock::new();
    INSTALLATION
        .get_or_init(|| client::service_installation().map_err(|e| e.to_string()))
        .clone()
        .map_err(io::Error::other)
}
pub(crate) fn initialize() -> Result<(), InputError> {
    // Warm identity/configuration and the owner BEFORE the realtime capture
    // loop starts. Neither file ACL inspection nor MMCSS registration belongs
    // to the first remote motion's initialization critical section.
    if oink_foundation::debug_env!(os "OINK_REQUIRE_INPUT_SERVICE").is_some()
        || installed().map_err(|e| InputError::Platform(e.to_string()))?
    {
        client::initialize().map_err(|e| InputError::Platform(format!("helper owner initialization: {e}")))?;
    }
    Ok(())
}
pub(crate) fn optional(cmd: Command) -> Result<Option<ServiceStatus>, InputError> {
    request_optional(
        oink_foundation::debug_env!(os "OINK_REQUIRE_INPUT_SERVICE").is_some(),
        installed,
        || client::request(cmd),
    )
    .map_err(|e| InputError::Platform(format!("Windows input service: {e}")))
}
fn request_optional<T>(
    required: bool,
    installation: impl FnOnce() -> io::Result<bool>,
    request: impl FnOnce() -> io::Result<T>,
) -> io::Result<Option<T>> {
    if !required && !installation()? {
        return Ok(None);
    }
    match request() {
        Ok(s) => Ok(Some(s)),
        // client::request only exposes NotFound before a connection has ever succeeded.
        Err(e) if e.kind() == io::ErrorKind::NotFound && !required => Ok(None),
        Err(e) => Err(e),
    }
}

pub(crate) fn send(inputs: &[INPUT]) -> Result<bool, InputError> {
    for (index, i) in inputs.iter().enumerate() {
        let cmd = command(i)?;
        if optional(cmd)?.is_none() {
            // No bytes were sent; only this branch may safely use the ordinary backend.
            if index == 0 {
                return Ok(false);
            }
            return Err(InputError::Platform("input service disappeared within batch".into()));
        }
    }
    Ok(true)
}

/// The helper receives a bounded command, never a raw dwExtraInfo. Only our
/// builders (OINK tag) and Quicker compatibility mode (0, button events only)
/// produce these INPUTs, so a zero tag selects the helper's untagged form.
fn command(i: &INPUT) -> Result<Command, InputError> {
    // SAFETY: INPUT comes only from our fully initialized key/mouse builders; type selects union field.
    unsafe {
        if i.r#type == INPUT_KEYBOARD {
            let k = i.Anonymous.ki;
            let scan = k.dwFlags.contains(KEYEVENTF_SCANCODE);
            Ok(Command::Key {
                code: if scan { k.wScan } else { k.wVk.0 },
                scan,
                extended: k.dwFlags.contains(KEYEVENTF_EXTENDEDKEY),
                down: !k.dwFlags.contains(KEYEVENTF_KEYUP),
            })
        } else if i.r#type == INPUT_MOUSE {
            let m = i.Anonymous.mi;
            let f = m.dwFlags;
            let untagged = m.dwExtraInfo == 0;
            Ok(if f.contains(MOUSEEVENTF_MOVE) {
                if f.contains(MOUSEEVENTF_ABSOLUTE) && f.contains(MOUSEEVENTF_LEFTDOWN) {
                    Command::DragPress {
                        x: m.dx,
                        y: m.dy,
                        untagged,
                    }
                } else if f.contains(MOUSEEVENTF_ABSOLUTE) && f.contains(MOUSEEVENTF_LEFTUP) {
                    Command::DragRelease {
                        x: m.dx,
                        y: m.dy,
                        untagged,
                    }
                } else if f.contains(MOUSEEVENTF_ABSOLUTE) {
                    Command::Move { x: m.dx, y: m.dy }
                } else {
                    Command::MoveRelative { dx: m.dx, dy: m.dy }
                }
            } else if f.contains(MOUSEEVENTF_WHEEL) || f.contains(MOUSEEVENTF_HWHEEL) {
                Command::Wheel {
                    amount: m.mouseData as i32,
                    horizontal: f.contains(MOUSEEVENTF_HWHEEL),
                }
            } else {
                let (button, down) = match f {
                    MOUSEEVENTF_LEFTDOWN => (0, true),
                    MOUSEEVENTF_LEFTUP => (0, false),
                    MOUSEEVENTF_RIGHTDOWN => (1, true),
                    MOUSEEVENTF_RIGHTUP => (1, false),
                    MOUSEEVENTF_MIDDLEDOWN => (2, true),
                    MOUSEEVENTF_MIDDLEUP => (2, false),
                    MOUSEEVENTF_XDOWN => ((m.mouseData + 2) as u16, true),
                    MOUSEEVENTF_XUP => ((m.mouseData + 2) as u16, false),
                    _ => return Err(InputError::Unsupported),
                };
                Command::Button { button, down, untagged }
            })
        } else {
            Err(InputError::Unsupported)
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn helper_commands_carry_the_quicker_untagged_form_for_buttons_only() {
        use super::super::inject;
        use oink_foundation::input::MouseButton;
        for quicker in [false, true] {
            let prepare = |mut input: INPUT| {
                if quicker {
                    inject::untag_for_quicker(&mut input);
                }
                command(&input).unwrap()
            };
            for (b, id) in [
                (MouseButton::Left, 0),
                (MouseButton::Right, 1),
                (MouseButton::Middle, 2),
                (MouseButton::Back, 3),
                (MouseButton::Forward, 4),
            ] {
                for down in [true, false] {
                    assert_eq!(
                        prepare(inject::button(b, down).unwrap()),
                        Command::Button {
                            button: id,
                            down,
                            untagged: quicker
                        }
                    );
                }
            }
            assert_eq!(
                prepare(inject::drag_press(5, 6)),
                Command::DragPress {
                    x: 5,
                    y: 6,
                    untagged: quicker
                }
            );
            assert!(matches!(
                prepare(inject::drag_release_at(Point::new(1., 2.), (0, 0), (10, 10))),
                Command::DragRelease { untagged, .. } if untagged == quicker
            ));
            assert_eq!(prepare(inject::move_abs(1, 2)), Command::Move { x: 1, y: 2 });
            assert!(matches!(prepare(inject::wheel(120, false)), Command::Wheel { .. }));
        }
    }

    #[test]
    fn ordinary_installation_never_opens_installed_service() {
        let result = request_optional::<()>(false, || Ok(false), || panic!("must not contact pipe"));
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn unsafe_protected_installation_fails_before_ipc() {
        let result = request_optional::<()>(
            false,
            || Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            || panic!("unsafe protected path must fail before IPC"),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn required_diagnostic_reaches_service_and_preserves_rejection() {
        let result = request_optional::<()>(
            true,
            || panic!("required mode must attempt real authentication"),
            || Err(io::Error::from(io::ErrorKind::PermissionDenied)),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn authenticated_errors_do_not_enable_local_replay() {
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::TimedOut,
            io::ErrorKind::BrokenPipe,
        ] {
            let result = request_optional::<()>(false, || Ok(true), || Err(io::Error::from(kind)));
            assert_eq!(result.unwrap_err().kind(), kind);
        }
        assert_eq!(request_optional(false, || Ok(true), || Ok(7)).unwrap(), Some(7));
        assert!(request_optional::<()>(true, || Ok(true), || Err(io::Error::from(io::ErrorKind::NotFound))).is_err());
    }
}
