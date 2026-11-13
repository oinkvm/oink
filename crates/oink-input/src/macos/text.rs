//! Committed remote text (`InjectEvent::Text`) as keyboard events that carry a
//! Unicode string.
//!
//! OS behaviour this relies on (probe 2026-11-09, macOS 26.6, an NSTextView
//! with WeType pinyin as the current input source, which composes a plain
//! posted `A` key into marked text): a HID-tap keyboard event on virtual key 0
//! with cleared flags and `CGEventKeyboardSetUnicodeString` is inserted
//! verbatim. The input method does not compose it, and Command, Control or
//! Option held through injected HID modifier events do not turn it into a
//! shortcut. Why this shape: setting a modifier flag on the event made the
//! insertion disappear, and a function-key code (F19) makes AppKit drop the
//! characters, so the flags are cleared explicitly and key 0 is used. Each
//! event carries at most [`crate::text::MAC_EVENT_UNITS`] code units.
use objc2_core_graphics::{CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID};

use crate::InputError;

/// Post `text` through `post` (the injector's HID tap). `source` is the
/// injector's tagged event source; without one a private source is created
/// and tagged with [`super::inject::MAGIC`] so the capture tap skips it.
pub(crate) fn post_text(
    source: Option<&CGEventSource>,
    text: &str,
    mut post: impl FnMut(&CGEvent) -> Result<(), InputError>,
) -> Result<(), InputError> {
    let private = match source {
        Some(_) => None,
        None => Some(
            CGEventSource::new(CGEventSourceStateID::Private)
                .ok_or_else(|| InputError::Platform("CGEventSourceCreate 失败".into()))?,
        ),
    };
    let source = source.or(private.as_deref());
    for chunk in crate::text::utf16_chunks(text, crate::text::MAC_EVENT_UNITS) {
        for down in [true, false] {
            let event = CGEvent::new_keyboard_event(source, 0, down)
                .ok_or_else(|| InputError::Platform("CGEventCreateKeyboardEvent 失败".into()))?;
            CGEvent::set_flags(Some(&event), CGEventFlags(0));
            CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUserData, super::inject::MAGIC);
            // SAFETY: the pointer covers `chunk.len()` initialized UTF-16 units,
            // at most MAC_EVENT_UNITS; CoreGraphics copies them.
            unsafe { CGEvent::keyboard_set_unicode_string(Some(&event), chunk.len() as _, chunk.as_ptr()) };
            post(&event)?;
        }
    }
    Ok(())
}
