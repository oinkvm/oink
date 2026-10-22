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
    Quit,
}

impl View {
    pub fn label(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Panel => "panel",
            Self::About => "about",
            Self::Quit => "quit",
        }
    }

    pub fn normal(self) -> bool {
        self != Self::Panel
    }

    pub fn from_label(label: &str) -> Option<Self> {
        [Self::Main, Self::Panel, Self::About, Self::Quit]
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

/// Wait for committed DOM/fonts, never for a frame from an invisible WebView.
/// Generations keep a late acknowledgement from a destroyed view from revealing
/// its replacement; repeated launch/reopen requests share the pending creation.
#[derive(Debug, Default)]
pub struct WindowReadyGate {
    next_generation: u32,
    windows: std::collections::HashMap<String, WindowReadiness>,
}
#[derive(Debug)]
struct WindowReadiness {
    generation: u32,
    creating: bool,
    ready: bool,
    revealed: bool,
}
impl WindowReadiness {
    fn take_reveal(&mut self) -> bool {
        if self.creating || !self.ready || self.revealed {
            return false;
        }
        self.revealed = true;
        true
    }
}
impl WindowReadyGate {
    pub fn begin_creation(&mut self, label: &str) -> u32 {
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("window generation exhausted");
        let generation = self.next_generation;
        self.windows.insert(
            label.to_owned(),
            WindowReadiness {
                generation,
                creating: true,
                ready: false,
                revealed: false,
            },
        );
        generation
    }
    pub fn created(&mut self, label: &str) -> bool {
        let Some(window) = self.windows.get_mut(label) else {
            return false;
        };
        window.creating = false;
        window.take_reveal()
    }
    pub fn forget(&mut self, label: &str) {
        self.windows.remove(label);
    }
    pub fn ready(&mut self, label: &str, generation: u32) -> bool {
        let Some(window) = self.windows.get_mut(label) else {
            return false;
        };
        if window.generation != generation {
            return false;
        }
        window.ready = true;
        window.take_reveal()
    }
    pub fn can_show(&self, label: &str) -> bool {
        self.windows
            .get(label)
            .is_some_and(|window| window.ready && !window.creating)
    }
}

#[cfg(test)]
mod window_ready_tests {
    use super::*;
    #[test]
    fn acknowledgement_during_native_creation_waits_and_only_reveals_once() {
        let mut gate = WindowReadyGate::default();
        let generation = gate.begin_creation("main");
        assert!(!gate.ready("main", generation));
        assert!(!gate.can_show("main"));
        assert!(gate.created("main"));
        assert!(
            !gate.ready("main", generation),
            "late/duplicate ready must not steal focus"
        );
        assert!(!gate.created("main"));
        assert!(
            gate.can_show("main"),
            "an explicit later open still restores the window"
        );
    }
    #[test]
    fn startup_showwindow_and_reopen_coalesce_until_dom_ready_without_a_paint() {
        let mut gate = WindowReadyGate::default();
        let generation = gate.begin_creation("main");
        assert!(!gate.created("main"));
        for _ in 0..3 {
            assert!(!gate.can_show("main"));
        }
        assert!(gate.ready("main", generation));
        assert!(!gate.ready("main", generation));
        assert!(gate.can_show("main"));
    }
    #[test]
    fn destroyed_failed_and_recreated_views_reject_old_acknowledgements() {
        let mut gate = WindowReadyGate::default();
        let old = gate.begin_creation("main");
        gate.forget("main");
        assert!(!gate.ready("main", old));
        assert!(!gate.can_show("main"));
        let current = gate.begin_creation("main");
        assert_ne!(old, current);
        assert!(!gate.created("main"));
        assert!(!gate.ready("main", old));
        assert!(!gate.can_show("main"));
        let about = gate.begin_creation("about");
        assert!(!gate.created("about"));
        assert!(gate.ready("about", about));
        assert!(!gate.can_show("main"));
        assert!(gate.ready("main", current));
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
        for view in [View::Main, View::About, View::Quit] {
            assert!(view.normal());
            assert_eq!(View::from_label(view.label()), Some(view));
        }
        assert_eq!(View::from_label("untrusted"), None);
    }

    #[test]
    fn update_flow_has_no_window_of_its_own() {
        // The update dialog is a modal inside the main window.
        assert_eq!(View::from_label("upgrade"), None);
        assert!(serde_json::from_str::<View>("\"upgrade\"").is_err());
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
