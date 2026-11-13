//! Committed remote text as native Unicode insertions (`InjectEvent::Text`).
//!
//! Both platforms insert UTF-16 code units, not key codes, so the target's
//! keyboard layout, input method and held modifiers do not change the text.
//! macOS limits one keyboard event's Unicode payload; Windows sends one
//! `KEYEVENTF_UNICODE` press/release per code unit in paced batches.

pub use oink_foundation::text::utf16_chunks;

/// UTF-16 units that one macOS keyboard event carries. The
/// `CGEventKeyboardSetUnicodeString` reference allows at most 20; longer
/// strings are truncated by the system, so larger text is split before that.
pub const MAC_EVENT_UNITS: usize = 20;
