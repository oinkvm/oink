//! Windows text-focus watcher (`crate::text_focus`, protocol §14.7).
//!
//! Why this shape: `EVENT_OBJECT_FOCUS` WinEvents (out of context) say *that*
//! keyboard focus moved, for every application, without a COM event handler;
//! UI Automation then says *what* has focus (control type, Value / Text /
//! TextEdit patterns), which also covers browser and XAML text inputs that
//! are not Win32 edit controls. UIA calls are cross-process COM and may block,
//! so they never run on the low-level hook thread: this thread owns the
//! WinEvent hook and its message queue, waits with `MsgWaitForMultipleObjectsEx`
//! (no thread-pool waits), and reads the focused element once a burst of focus
//! events has settled (`Settle`). COM is initialised MTA on this thread, as for
//! the pre-login credential-field observer.
//!
//! Not started by the pre-login backend (no phone controls the logon screen;
//! its password field has its own observer). Elements of this process are not
//! reported.
use crate::text_focus::{FocusEdges, Report, Running, Settle, TextFocusReport, UiaFocus, windows_editable};
use std::{
    cell::Cell,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::HWND,
        System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize},
        UI::{
            Accessibility::{
                CUIAutomation, HWINEVENTHOOK, IUIAutomation, IUIAutomationElement, IUIAutomationValuePattern,
                SetWinEventHook, UIA_TextEditPatternId, UIA_TextPatternId, UIA_ValuePatternId, UnhookWinEvent,
            },
            WindowsAndMessaging::{
                DispatchMessageW, EVENT_OBJECT_FOCUS, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
                PeekMessageW, QS_ALLINPUT, TranslateMessage, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
            },
        },
    },
    core::Interface,
};

/// Longest wait between checks of `running` while nothing happens.
const IDLE: Duration = Duration::from_millis(250);

thread_local! {
    /// Set by the WinEvent callback, which runs inside this thread's message retrieval.
    static NOTIFIED: Cell<bool> = const { Cell::new(false) };
}

unsafe extern "system" fn focus_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _window: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    // Only a flag: the callback can be nested inside another call on this thread.
    NOTIFIED.set(true);
}

pub(crate) fn spawn(report: Report, running: Running) -> Option<std::thread::JoinHandle<()>> {
    if super::profile::current().credential_gate {
        return None;
    }
    std::thread::Builder::new()
        .name("oink-text-focus".into())
        .spawn(move || watch(&*report, &*running))
        .map_err(|error| tracing::warn!(%error, "text focus watcher unavailable"))
        .ok()
}

fn watch(report: &dyn Fn(TextFocusReport), running: &dyn Fn() -> bool) {
    // SAFETY: this thread's own COM apartment, balanced below.
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    // SAFETY: in-process UIA client object; released before CoUninitialize.
    let automation: windows::core::Result<IUIAutomation> =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) };
    match automation {
        Ok(automation) => observe(&automation, report, running),
        Err(error) => tracing::warn!(code = error.code().0, "text focus: UI Automation unavailable"),
    }
    if initialized {
        // SAFETY: balances the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

fn observe(automation: &IUIAutomation, report: &dyn Fn(TextFocusReport), running: &dyn Fn() -> bool) {
    // SAFETY: out-of-context hook delivered through this thread's message queue,
    // removed below on the same thread.
    let hook = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_FOCUS,
            EVENT_OBJECT_FOCUS,
            None,
            Some(focus_changed),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    if hook.is_invalid() {
        tracing::warn!("text focus: focus events unavailable");
        return;
    }
    tracing::info!("text focus watcher started");
    let mut last: Option<IUIAutomationElement> = None;
    let mut edges = FocusEdges::default();
    let mut settle = Settle::default();
    // The element focused at start is the first report.
    settle.event(Instant::now());
    while running() {
        let now = Instant::now();
        if settle.take_due(now) {
            let element = focused(automation);
            let changed = match (&element, &last) {
                (None, None) => false,
                (Some(a), Some(b)) => !same(automation, a, b),
                _ => true,
            };
            let focus = element.as_ref().map(classify);
            let editable = focus.is_some_and(windows_editable);
            let secure = focus.is_some_and(|focus| focus.password);
            if let Some(next) = edges.observe(changed, editable, secure) {
                tracing::debug!(
                    seq = next.seq,
                    editable = next.editable,
                    secure = next.secure,
                    "text focus changed"
                );
                report(next);
            }
            last = element;
        }
        let wait = settle.wait(now, IDLE).as_millis().clamp(1, IDLE.as_millis()) as u32;
        // MWMO_INPUTAVAILABLE also notices messages a previous peek already saw.
        // SAFETY: no handles; waits on this thread's own message queue.
        let _ = unsafe { MsgWaitForMultipleObjectsEx(None, wait, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
        let mut msg = MSG::default();
        // SAFETY: drains this thread's own queue; out-of-context WinEvent
        // callbacks are delivered during the peek.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if NOTIFIED.replace(false) {
            settle.event(Instant::now());
        }
    }
    // SAFETY: own hook, on the installing thread.
    unsafe {
        let _ = UnhookWinEvent(hook);
    }
    tracing::info!("text focus watcher stopped");
}

fn same(automation: &IUIAutomation, a: &IUIAutomationElement, b: &IUIAutomationElement) -> bool {
    // SAFETY: COM call on this thread's live UIA client with two live elements.
    unsafe { automation.CompareElements(a, b) }.is_ok_and(|same| same.as_bool())
}

/// The focused element, unless it belongs to this process.
fn focused(automation: &IUIAutomation) -> Option<IUIAutomationElement> {
    // SAFETY: COM calls on this thread's live UIA client.
    let element = unsafe { automation.GetFocusedElement() }.ok()?;
    // SAFETY: COM call on a live element.
    let pid = unsafe { element.CurrentProcessId() }.ok()?;
    (pid as u32 != std::process::id()).then_some(element)
}

fn classify(element: &IUIAutomationElement) -> UiaFocus {
    // SAFETY: COM calls on a live element owned by this thread; a pattern the
    // element does not support comes back as an error.
    unsafe {
        let value_read_only = element
            .GetCurrentPattern(UIA_ValuePatternId)
            .ok()
            .and_then(|pattern| pattern.cast::<IUIAutomationValuePattern>().ok())
            .and_then(|value| value.CurrentIsReadOnly().ok())
            .map(|read_only| read_only.as_bool());
        UiaFocus {
            control_type: element.CurrentControlType().map_or(0, |id| id.0),
            keyboard_focusable: element.CurrentIsKeyboardFocusable().is_ok_and(|b| b.as_bool()),
            password: element.CurrentIsPassword().is_ok_and(|b| b.as_bool()),
            value_read_only,
            text_pattern: element.GetCurrentPattern(UIA_TextPatternId).is_ok(),
            text_edit_pattern: element.GetCurrentPattern(UIA_TextEditPatternId).is_ok(),
        }
    }
}
