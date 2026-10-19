//! Window lifetime and placement shared by the native constructors and tests.
use oink_agent::backend::{PanelAnchor, PanelRect};
use serde::{Deserialize, Serialize};

/// A login/background start stays windowless even if backend startup fails.
pub(crate) fn shows_on_start(login: bool, background: bool) -> bool {
    !login && !background
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum View {
    Main,
    Panel,
    About,
    Upgrade,
    Quit,
}

impl View {
    pub fn label(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Panel => "panel",
            Self::About => "about",
            Self::Upgrade => "upgrade",
            Self::Quit => "quit",
        }
    }

    pub fn normal(self) -> bool {
        self != Self::Panel
    }

    pub fn from_label(label: &str) -> Option<Self> {
        [Self::Main, Self::Panel, Self::About, Self::Upgrade, Self::Quit]
            .into_iter()
            .find(|view| view.label() == label)
    }
}

/// Only an explicit local user action requests activation. Backend events do
/// not prove that the sender owns Windows foreground permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activation {
    User,
    Background,
}

/// Provenance is local shell input, not a backend event with a suggestive name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenTrigger {
    Launch,
    SecondInstance,
    Tray,
    Hotkey,
    WindowAction,
    Backend,
}
impl OpenTrigger {
    pub fn activation(self) -> Activation {
        match self {
            Self::Launch | Self::SecondInstance | Self::Tray | Self::Hotkey | Self::WindowAction => Activation::User,
            Self::Backend => Activation::Background,
        }
    }
}

pub use oink_agent::backend::foreground::handoff;

/// A normal window may be requested again while its first WebView is loading.
/// Only the frontend's ready acknowledgement releases that initial visibility.
#[derive(Debug, Default)]
pub struct FirstPaintGate {
    painting: std::collections::HashSet<String>,
    creating: std::collections::HashSet<String>,
}
impl FirstPaintGate {
    /// Record the wait before entering a native message pump. An early frontend
    /// acknowledgement cannot expose a window before native preparation finishes.
    pub fn begin_creation(&mut self, label: &str) {
        self.hold(label);
        self.creating.insert(label.to_owned());
    }
    pub fn created(&mut self, label: &str) {
        self.creating.remove(label);
    }
    pub fn forget(&mut self, label: &str) {
        self.painting.remove(label);
        self.creating.remove(label);
    }
    pub fn hold(&mut self, label: &str) {
        self.painting.insert(label.to_owned());
    }
    pub fn ready(&mut self, label: &str) {
        self.painting.remove(label);
    }
    pub fn can_show(&self, label: &str) -> bool {
        !self.painting.contains(label) && !self.creating.contains(label)
    }
}

#[cfg(test)]
mod first_paint_tests {
    use super::*;
    #[test]
    fn early_paint_ack_waits_for_native_creation_and_failed_creation_can_retry() {
        let mut gate = FirstPaintGate::default();
        gate.begin_creation("about");
        gate.ready("about");
        assert!(!gate.can_show("about"));
        gate.created("about");
        assert!(gate.can_show("about"));
        gate.begin_creation("about");
        gate.forget("about");
        gate.begin_creation("about");
        gate.created("about");
        assert!(!gate.can_show("about"));
        gate.ready("about");
        assert!(gate.can_show("about"));
    }

    #[test]
    fn repeated_open_waits_for_first_paint_and_recreated_windows_wait_again() {
        let mut gate = FirstPaintGate::default();
        gate.hold("main");
        assert!(!gate.can_show("main"));
        assert!(!gate.can_show("main"), "a second open cannot reveal an incomplete view");
        gate.ready("main");
        assert!(gate.can_show("main"));
        gate.hold("about");
        assert!(!gate.can_show("about"));
        assert!(gate.can_show("main"));
        gate.ready("main"); // destruction forgets the old window
        gate.hold("main");
        assert!(!gate.can_show("main"));
    }
}

pub const PANEL_WIDTH: f64 = 360.0;
pub const PANEL_HEIGHT: f64 = 520.0;

/// Bounds in the anchor's coordinate space; clamp to the clicked monitor's
/// usable frame, including negative origins and bottom/side Windows taskbars.
pub fn panel_bounds(anchor: PanelAnchor, width: f64, height: f64) -> Option<PanelRect> {
    let a = anchor.rect;
    let w = anchor.work_area;
    let scale = anchor.scale_factor;
    if ![
        a.x, a.y, a.width, a.height, w.x, w.y, w.width, w.height, scale, width, height,
    ]
    .into_iter()
    .all(f64::is_finite)
        || scale <= 0.0
        || width <= 0.0
        || height <= 0.0
        || w.width <= 0.0
        || w.height <= 0.0
    {
        return None;
    }
    let width = (width * scale).min(w.width);
    let height = (height * scale).min(w.height);
    let below = a.y + a.height;
    let y = if below + height <= w.y + w.height {
        below
    } else {
        a.y - height
    };
    Some(PanelRect {
        x: (a.x + (a.width - width) / 2.0).clamp(w.x, w.x + w.width - width),
        y: y.clamp(w.y, w.y + w.height - height),
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn login_and_background_starts_never_request_a_window() {
        for background in [false, true] {
            assert!(!super::shows_on_start(true, background));
        }
        assert!(!super::shows_on_start(false, true));
        assert!(super::shows_on_start(false, false));
    }
    use super::*;

    #[test]
    fn panel_never_requests_regular_activation() {
        assert!(!View::Panel.normal());
        for view in [View::Main, View::About, View::Upgrade, View::Quit] {
            assert!(view.normal());
            assert_eq!(View::from_label(view.label()), Some(view));
        }
        assert_eq!(View::from_label("untrusted"), None);
    }

    #[test]
    fn placement_uses_clicked_monitor_scale_and_work_area() {
        let mut anchor = PanelAnchor {
            rect: PanelRect {
                x: -30.0,
                y: 0.0,
                width: 24.0,
                height: 24.0,
            },
            work_area: PanelRect {
                x: -1920.0,
                y: 24.0,
                width: 1920.0,
                height: 1056.0,
            },
            scale_factor: 1.0,
        };
        let rect = panel_bounds(anchor, PANEL_WIDTH, PANEL_HEIGHT).unwrap();
        assert_eq!((rect.x, rect.y, rect.width), (-360.0, 24.0, 360.0));
        anchor.rect.y = 1080.0;
        anchor.scale_factor = 1.5;
        let rect = panel_bounds(anchor, PANEL_WIDTH, PANEL_HEIGHT).unwrap();
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (-540.0, 300.0, 540.0, 780.0));
        anchor.scale_factor = f64::NAN;
        assert!(panel_bounds(anchor, 360.0, 520.0).is_none());
    }
}

#[cfg(test)]
mod activation_tests {
    use super::*;
    #[test]
    fn only_local_user_triggers_request_activation() {
        for trigger in [
            OpenTrigger::Launch,
            OpenTrigger::SecondInstance,
            OpenTrigger::Tray,
            OpenTrigger::Hotkey,
            OpenTrigger::WindowAction,
        ] {
            assert_eq!(trigger.activation(), Activation::User);
        }
        assert_eq!(OpenTrigger::Backend.activation(), Activation::Background);
    }
    #[test]
    fn handoff_grants_before_signalling_and_never_signals_after_failure() {
        let calls = std::cell::RefCell::new(Vec::new());
        handoff(
            || {
                calls.borrow_mut().push("grant");
                Ok::<_, ()>(())
            },
            || {
                calls.borrow_mut().push("signal");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["grant", "signal"]);
        assert!(handoff(|| Err(()), || panic!("no ungranted notification")).is_err());
    }
}
