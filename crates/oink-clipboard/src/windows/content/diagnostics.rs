//! Numeric correlation only; no native format names, caller identity or content.
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicU64, Ordering},
};
use windows::{Win32::Foundation::*, core::HRESULT};

static NEXT_OBJECT: AtomicU64 = AtomicU64::new(1);
thread_local! {
    static LATEST_PUBLICATION: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ObjectState {
    Unpublished = 0,
    Publishing = 1,
    Current = 2,
    Replaced = 3,
    Unknown = 4,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Observation {
    pub object: u64,
    // Most recent successful publication on this STA, not an OS sequence number.
    // Zero means no known newer Oink object (e.g. replacement by another app).
    pub replacement: u64,
    pub state: ObjectState,
}

pub(crate) struct Publication {
    object: u64,
    phase: AtomicU8,
    latest: Arc<AtomicU64>,
}
impl Publication {
    pub fn new() -> Self {
        Self {
            object: NEXT_OBJECT.fetch_add(1, Ordering::Relaxed),
            phase: AtomicU8::new(0),
            latest: LATEST_PUBLICATION.with(Arc::clone),
        }
    }
    pub fn begin(&self) {
        self.phase.store(1, Ordering::Release);
    }
    pub fn finish(&self, success: bool) {
        if success {
            self.latest.store(self.object, Ordering::Release);
        }
        self.phase.store(if success { 2 } else { 0 }, Ordering::Release);
    }
    pub(crate) fn observe(&self, is_current: Option<bool>) -> Observation {
        let phase = self.phase.load(Ordering::Acquire);
        let state = match is_current {
            Some(true) => ObjectState::Current,
            Some(false) => match phase {
                1 => ObjectState::Publishing,
                2 => ObjectState::Replaced,
                _ => ObjectState::Unpublished,
            },
            None => ObjectState::Unknown,
        };
        let latest = self.latest.load(Ordering::Acquire);
        Observation {
            object: self.object,
            replacement: if state == ObjectState::Replaced && latest != 0 && latest != self.object {
                latest
            } else {
                0
            },
            state,
        }
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub(super) enum Stage {
    Open = 1,
    Read = 2,
    Decode = 3,
    Convert = 4,
    FileMedium = 5,
    RestoreMedium = 6,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum HResultClass {
    ReadFault = 1,
    Format = 2,
    OutOfMemory = 3,
    AccessDenied = 4,
    Other = 5,
}
impl HResultClass {
    pub fn from_code(code: HRESULT) -> Self {
        match code {
            STG_E_READFAULT => Self::ReadFault,
            DV_E_FORMATETC | DV_E_TYMED | DV_E_DVASPECT | DV_E_LINDEX => Self::Format,
            E_OUTOFMEMORY => Self::OutOfMemory,
            E_ACCESSDENIED | STG_E_ACCESSDENIED => Self::AccessDenied,
            _ => Self::Other,
        }
    }
}
// Explicit codes, independent of Rust enum layout. Zero is not an IO failure.
pub(super) fn reason_code(reason: crate::model::Unavailable) -> u8 {
    use crate::model::Unavailable::*;
    match reason {
        Unsupported => 1,
        LimitExceeded => 2,
        Superseded => 3,
        ReadFailed => 4,
        Cancelled => 5,
        NoSuchItem => 6,
        Permission => 7,
        NoProgress => 8,
        StorageFull => 9,
        InvalidData => 10,
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn publication_observation_distinguishes_current_replaced_and_inflight() {
        let first = Publication::new();
        assert_eq!(first.observe(Some(false)).state, ObjectState::Unpublished);
        first.begin();
        assert_eq!(first.observe(Some(false)).state, ObjectState::Publishing);
        // OLE can reenter GetData before OleSetClipboard returns.
        assert_eq!(first.observe(Some(true)).state, ObjectState::Current);
        first.finish(true);
        assert_eq!(first.observe(None).state, ObjectState::Unknown);
        assert_eq!(first.observe(Some(true)).state, ObjectState::Current);
        assert_eq!(first.observe(Some(false)).state, ObjectState::Replaced);
        assert_eq!(first.observe(Some(false)).replacement, 0); // external replacement
        let second = Publication::new();
        second.begin();
        second.finish(false);
        assert_eq!(first.observe(Some(false)).replacement, 0); // failed publication is not a replacement
        second.begin();
        second.finish(true);
        assert_eq!(first.observe(Some(false)).replacement, second.object);
        assert_eq!(second.observe(Some(true)).replacement, 0);
    }
    #[test]
    fn native_result_classes_are_typed_without_error_text() {
        assert_eq!(HResultClass::from_code(STG_E_READFAULT), HResultClass::ReadFault);
        assert_eq!(HResultClass::from_code(DV_E_TYMED), HResultClass::Format);
        assert_eq!(HResultClass::from_code(E_OUTOFMEMORY), HResultClass::OutOfMemory);
        assert_eq!(HResultClass::from_code(E_ACCESSDENIED), HResultClass::AccessDenied);
        assert_eq!(HResultClass::from_code(E_FAIL), HResultClass::Other);
    }
}
