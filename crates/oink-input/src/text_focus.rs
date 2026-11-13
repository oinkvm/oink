//! Text-input focus watcher (protocol §14.7): reports when keyboard focus on
//! this computer moves into or out of an editable text element, and whether it
//! is a password field, so a phone controlling it can pop its keyboard up or
//! hide it (control-page spec §9).
//!
//! The platform part observes focus changes on its own thread (macOS:
//! Accessibility `AXFocusedUIElementChanged` on the focused application;
//! Windows: `EVENT_OBJECT_FOCUS` WinEvents, classified through UI Automation).
//! Everything else is here and platform independent:
//! - [`Settle`]: focus events come in bursts (a window activating moves focus
//!   window → pane → field within milliseconds), so the focused element is
//!   read once the burst has been quiet for [`Settle::QUIET`], at the latest
//!   [`Settle::LONGEST`] after its first event;
//! - [`FocusEdges`]: turns settled observations into `(seq, editable, secure)`
//!   reports, `seq` counting each new focus into an editable element;
//! - [`mac_editable`] / [`windows_editable`]: which focused element is a text
//!   input; [`mac_secure`] / `UiaFocus::password`: which is a password field.
//!
//! Only whether an editable (or password) element has focus leaves this
//! module: never its text, name, role or application.
#![cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
use std::time::{Duration, Instant};

/// One report for the router (`Router::set_text_focus`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextFocusReport {
    /// Counts keyboard focuses into an editable text element since the watcher started.
    pub seq: u64,
    /// An editable text element has keyboard focus now.
    pub editable: bool,
    /// That element is a password field.
    pub secure: bool,
}

/// Starts the watcher thread. `report` is called on that thread with each new
/// report; the thread ends soon after `running` returns false. `None` where
/// there is no watcher (the Windows pre-login backend) or the thread could not
/// start.
#[cfg(target_os = "macos")]
pub fn spawn(report: Report, running: Running) -> Option<std::thread::JoinHandle<()>> {
    crate::macos::text_focus::spawn(report, running)
}

/// See the macOS `spawn`.
#[cfg(windows)]
pub fn spawn(report: Report, running: Running) -> Option<std::thread::JoinHandle<()>> {
    crate::windows::text_focus::spawn(report, running)
}

/// No watcher on other systems.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn spawn(report: Report, running: Running) -> Option<std::thread::JoinHandle<()>> {
    let _ = (report, running);
    None
}

/// Called on the watcher thread with each new report.
pub type Report = Box<dyn Fn(TextFocusReport) + Send>;
/// Polled by the watcher thread; it ends once this returns false.
pub type Running = Box<dyn Fn() -> bool + Send>;

/// Trailing debounce of focus events with an upper bound.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Settle {
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Settle {
    /// Quiet time after the latest event before the element is read.
    pub(crate) const QUIET: Duration = Duration::from_millis(80);
    /// A burst that never goes quiet is still read this long after its first event.
    pub(crate) const LONGEST: Duration = Duration::from_millis(300);

    /// A focus event (or anything that may have moved focus) at `now`.
    pub(crate) fn event(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    fn due(&self) -> Option<Instant> {
        Some((self.last? + Self::QUIET).min(self.first? + Self::LONGEST))
    }

    /// Whether the element should be read now; ends the burst if so.
    pub(crate) fn take_due(&mut self, now: Instant) -> bool {
        if self.due().is_some_and(|due| now >= due) {
            *self = Self::default();
            true
        } else {
            false
        }
    }

    /// How long the watcher may wait for the next event: until the read is
    /// due, at most `idle`.
    pub(crate) fn wait(&self, now: Instant, idle: Duration) -> Duration {
        self.due()
            .map_or(idle, |due| due.saturating_duration_since(now).min(idle))
    }
}

/// Settled observations → reports.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FocusEdges {
    /// The last report; `None` before the first observation.
    report: Option<TextFocusReport>,
}

impl FocusEdges {
    /// One settled observation: `changed` says the focused element is not the
    /// one of the previous observation, `editable` whether it is a text input,
    /// `secure` whether it is a password field. Returns the new report when
    /// anything changed; the first observation is always reported, so the
    /// router has a state to give phones even when no field ever gets focus.
    ///
    /// A new input focus (`seq` + 1) is a different element that is editable,
    /// or the same element becoming editable. Re-reading the same focused
    /// field (an app re-posting its focus notification) is none, so a phone
    /// keyboard the user hid is not brought back; moving to another field is.
    pub(crate) fn observe(&mut self, changed: bool, editable: bool, secure: bool) -> Option<TextFocusReport> {
        let previous = self.report;
        let mut next = previous.unwrap_or_default();
        if editable && (changed || !next.editable) {
            next.seq += 1;
        }
        next.editable = editable;
        next.secure = editable && secure;
        self.report = Some(next);
        (Some(next) != previous).then_some(next)
    }
}

/// macOS: whether the focused Accessibility element takes text input.
/// `role`/`subrole` are its `AXRole`/`AXSubrole`; `value_settable` whether its
/// `AXValue` is settable and `selected_text_range` whether it has an
/// `AXSelectedTextRange` (both read only for roles not recognised by name).
///
/// Text fields, text areas, combo boxes, search and secure (password) fields
/// count by role; other elements (web `contenteditable` regions, custom text
/// views) count when they have a settable value and a text selection.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn mac_text_role(role: Option<&str>, subrole: Option<&str>) -> bool {
    matches!(
        role,
        Some("AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField")
    ) || matches!(subrole, Some("AXSecureTextField" | "AXSearchField"))
}

/// macOS: a password field (`AXSecureTextField` subrole).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn mac_secure(subrole: Option<&str>) -> bool {
    subrole == Some("AXSecureTextField")
}

/// See [`mac_text_role`].
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn mac_editable(
    role: Option<&str>,
    subrole: Option<&str>,
    value_settable: bool,
    selected_text_range: bool,
) -> bool {
    mac_text_role(role, subrole) || (value_settable && selected_text_range)
}

/// UI Automation control type ids (UIAutomationClient.h).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const UIA_EDIT: i32 = 50004;
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const UIA_DOCUMENT: i32 = 50030;
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const UIA_COMBO_BOX: i32 = 50003;

/// What UI Automation says about the focused element (Windows).
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct UiaFocus {
    pub control_type: i32,
    pub keyboard_focusable: bool,
    pub password: bool,
    /// `ValuePattern.IsReadOnly`; `None` without a Value pattern.
    pub value_read_only: Option<bool>,
    pub text_pattern: bool,
    /// `TextEditPattern`: implemented by controls whose text an input method edits.
    pub text_edit_pattern: bool,
}

/// Windows: whether the focused UI Automation element takes text input.
///
/// - Edit: unless its value is read-only (a password field always counts).
/// - Document: only with a writable value or the TextEdit pattern; a browser
///   page or a read-only viewer is a Document with a text pattern too, and
///   clicking it must not open the phone keyboard.
/// - Combo box: an editable one (a writable value).
/// - Anything else: a writable value with a text pattern, or the TextEdit pattern.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_editable(focus: UiaFocus) -> bool {
    if !focus.keyboard_focusable {
        return false;
    }
    let writable = focus.value_read_only == Some(false);
    match focus.control_type {
        UIA_EDIT => focus.password || focus.value_read_only != Some(true),
        UIA_DOCUMENT => writable || focus.text_edit_pattern,
        UIA_COMBO_BOX => writable,
        _ => (writable && focus.text_pattern) || focus.text_edit_pattern,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_is_read_once_after_it_goes_quiet() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut settle = Settle::default();
        assert!(!settle.take_due(t0));
        assert_eq!(settle.wait(t0, Duration::from_millis(250)), Duration::from_millis(250));
        settle.event(t0);
        settle.event(ms(30));
        assert_eq!(settle.wait(ms(30), Duration::from_secs(1)), Duration::from_millis(80));
        assert!(!settle.take_due(ms(100)));
        assert!(settle.take_due(ms(110)));
        assert!(!settle.take_due(ms(500)), "one read per burst");
    }

    #[test]
    fn a_burst_that_never_goes_quiet_is_still_read() {
        let t0 = Instant::now();
        let mut settle = Settle::default();
        let mut reads = 0;
        for n in 0..40 {
            let now = t0 + Duration::from_millis(n * 20);
            settle.event(now);
            if settle.take_due(now) {
                reads += 1;
            }
        }
        // 800 ms of events 20 ms apart: read every 300 ms.
        assert_eq!(reads, 2);
    }

    fn report(seq: u64, editable: bool, secure: bool) -> Option<TextFocusReport> {
        Some(TextFocusReport { seq, editable, secure })
    }

    #[test]
    fn edges_count_new_focus_into_editable_elements_and_report_leaving() {
        let mut edges = FocusEdges::default();
        assert_eq!(
            edges.observe(true, false, false),
            report(0, false, false),
            "first observation"
        );
        assert_eq!(
            edges.observe(true, false, false),
            None,
            "another button: nothing to say"
        );
        assert_eq!(edges.observe(true, true, false), report(1, true, false));
        // The same field read again (re-posted notification): no new focus, no report.
        assert_eq!(edges.observe(false, true, false), None);
        // Another field: a new focus.
        assert_eq!(edges.observe(true, true, false), report(2, true, false));
        // Leaving for a non-text element is reported (the phone hides a popped-up keyboard).
        assert_eq!(edges.observe(true, false, false), report(2, false, false));
        // Back into a field: a new focus.
        assert_eq!(edges.observe(true, true, false), report(3, true, false));
        // The same element became editable (read-only → editable): a new focus.
        edges.observe(false, false, false);
        assert_eq!(edges.observe(false, true, false), report(4, true, false));
    }

    #[test]
    fn password_fields_are_reported_as_secure() {
        let mut edges = FocusEdges::default();
        assert_eq!(edges.observe(true, true, true), report(1, true, true));
        // Tab to the user-name field: a new focus, no longer secure.
        assert_eq!(edges.observe(true, true, false), report(2, true, false));
        // "Secure" without an editable element is not reported as secure.
        assert_eq!(edges.observe(true, false, true), report(2, false, false));
        assert!(mac_secure(Some("AXSecureTextField")));
        assert!(!mac_secure(Some("AXSearchField")));
        assert!(!mac_secure(None));
    }

    #[test]
    fn mac_roles() {
        for role in ["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"] {
            assert!(mac_editable(Some(role), None, false, false), "{role}");
        }
        assert!(mac_editable(
            Some("AXTextField"),
            Some("AXSecureTextField"),
            false,
            false
        ));
        assert!(mac_editable(Some("AXTextField"), Some("AXSearchField"), true, true));
        // A contenteditable region: a group with a settable value and a selection.
        assert!(mac_editable(Some("AXGroup"), None, true, true));
        assert!(!mac_editable(Some("AXGroup"), None, false, true));
        // A slider has a settable value but no text selection.
        assert!(!mac_editable(Some("AXSlider"), None, true, false));
        for role in ["AXButton", "AXWebArea", "AXList", "AXWindow"] {
            assert!(!mac_editable(Some(role), None, false, false), "{role}");
        }
        assert!(!mac_editable(None, None, false, false));
    }

    fn uia(control_type: i32) -> UiaFocus {
        UiaFocus {
            control_type,
            keyboard_focusable: true,
            ..Default::default()
        }
    }

    #[test]
    fn windows_control_types() {
        // Edit: Notepad's classic edit, a search box, a password box.
        assert!(windows_editable(uia(UIA_EDIT)));
        assert!(windows_editable(UiaFocus {
            value_read_only: Some(false),
            ..uia(UIA_EDIT)
        }));
        assert!(!windows_editable(UiaFocus {
            value_read_only: Some(true),
            ..uia(UIA_EDIT)
        }));
        assert!(windows_editable(UiaFocus {
            password: true,
            value_read_only: Some(true),
            ..uia(UIA_EDIT)
        }));
        // Document: a browser page (text pattern, read-only value) is not a text input.
        assert!(!windows_editable(UiaFocus {
            text_pattern: true,
            value_read_only: Some(true),
            ..uia(UIA_DOCUMENT)
        }));
        assert!(!windows_editable(UiaFocus {
            text_pattern: true,
            ..uia(UIA_DOCUMENT)
        }));
        // Word / Notepad's rich edit: writable value or TextEdit pattern.
        assert!(windows_editable(UiaFocus {
            value_read_only: Some(false),
            ..uia(UIA_DOCUMENT)
        }));
        assert!(windows_editable(UiaFocus {
            text_pattern: true,
            text_edit_pattern: true,
            ..uia(UIA_DOCUMENT)
        }));
        // Combo box: only an editable one.
        assert!(!windows_editable(uia(UIA_COMBO_BOX)));
        assert!(windows_editable(UiaFocus {
            value_read_only: Some(false),
            ..uia(UIA_COMBO_BOX)
        }));
        // Other types: a writable text control (custom editors) counts.
        const CUSTOM: i32 = 50025;
        const BUTTON: i32 = 50000;
        assert!(windows_editable(UiaFocus {
            value_read_only: Some(false),
            text_pattern: true,
            ..uia(CUSTOM)
        }));
        assert!(!windows_editable(UiaFocus {
            value_read_only: Some(false),
            ..uia(CUSTOM)
        }));
        assert!(!windows_editable(uia(BUTTON)));
        // Not keyboard focusable: never.
        assert!(!windows_editable(UiaFocus {
            keyboard_focusable: false,
            ..uia(UIA_EDIT)
        }));
    }
}
