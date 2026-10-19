//! Fixed numeric crossing observations. Producers never format, allocate or wait.
//! The process recorder owns the bounded queue; the weak observer cannot retain a node.
use crate::snapshot::Snapshot;
use std::cell::Cell;
use std::sync::{OnceLock, Weak};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Stage {
    RoutingDecision = 1,
    Enqueue,
    NetworkDequeue,
    QuicWriteStarted,
    QuicWriteBlocked,
    /// All frame bytes accepted by Quinn, not an ACK or proof of UDP transmission.
    QuicWriteAccepted,
    QuicReceive,
    InputDispatch,
    MotionPacing,
    MotionFlush,
    NativeApply,
    NativeDeferred,
    HideStarted,
    HideCompleted,
    WarpStarted,
    WarpCompleted,
    ShowStarted,
    ShowCompleted,
    CleanupQueued,
    CleanupStarted,
    CleanupCompleted,
    FirstMotionStarted,
    FirstMotionCompleted,
    GlyphRetireStarted,
    GlyphRetireCompleted,
    MotionTimer,
    OwnerPrepareQueued,
    OwnerPrepareStarted,
    OwnerReady,
    OwnerHitResolved,
    OwnerMarkerConsumed,
    HideFlushed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Token {
    pub epoch: u64,
    /// Zero for FocusEnter/native entry; otherwise the first motion sequence.
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub stage: Stage,
    pub token: Token,
    pub link: u64,
    pub stream: u32,
    pub elapsed_us: u64,
    /// Stage-specific typed outcome/flags; see the callsite and timing documentation.
    pub code: u32,
    pub sent_packets: u64,
    pub lost_packets: u64,
    pub rtt_us: u64,
    pub count: u64,
}
impl Event {
    pub fn new(stage: Stage, token: Token) -> Self {
        Self {
            stage,
            token,
            link: 0,
            stream: 0,
            elapsed_us: 0,
            code: 0,
            sent_packets: 0,
            lost_packets: 0,
            rtt_us: 0,
            count: 0,
        }
    }
}

pub trait Observer: Send + Sync {
    fn enabled(&self) -> bool;
    fn record(&self, event: Event);
}
static OBSERVER: OnceLock<Snapshot<Option<Weak<dyn Observer>>>> = OnceLock::new();
pub fn set_observer(observer: Weak<dyn Observer>) {
    OBSERVER.get_or_init(|| Snapshot::new(None)).publish(Some(observer));
}
fn observer() -> Option<std::sync::Arc<dyn Observer>> {
    OBSERVER.get()?.load().as_ref().as_ref()?.upgrade()
}
pub fn enabled() -> bool {
    observer().is_some_and(|observer| observer.enabled())
}
pub fn emit(event: Event) {
    if let Some(observer) = observer() {
        observer.record(event);
    }
}

thread_local! { static SCOPE: Cell<Option<Token>> = const { Cell::new(None) }; }
pub fn scope() -> Option<Token> {
    SCOPE.with(Cell::get)
}
pub fn with_scope<T>(token: Option<Token>, action: impl FnOnce() -> T) -> T {
    let _reset = enter_scope(token);
    action()
}
#[derive(Debug)]
pub struct Scope(Option<Token>);
impl Drop for Scope {
    fn drop(&mut self) {
        SCOPE.with(|scope| scope.set(self.0));
    }
}
pub fn enter_scope(token: Option<Token>) -> Scope {
    Scope(SCOPE.with(|scope| scope.replace(token)))
}
pub fn native(stage: Stage, code: u32) {
    if let Some(token) = scope() {
        let mut event = Event::new(stage, token);
        event.code = code;
        emit(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_native_scope_is_restored_after_unwind() {
        let outer = Token { epoch: 2, sequence: 0 };
        with_scope(Some(outer), || {
            let _ = std::panic::catch_unwind(|| with_scope(None, || panic!("scope test")));
            assert_eq!(scope(), Some(outer));
        });
        assert_eq!(scope(), None);
    }
}
