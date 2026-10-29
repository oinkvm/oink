//! Once per native drag boundary, never on motion or data reads. Class codes
//! describe types only; they are NOT a representation-selection policy.
use super::*;

// One byte per representation, ';' terminates each item (including empty ones).
// Keep this alphabet and schema aligned with diagnostics/trace.rs and ACCEPTANCE.
const LIMIT: usize = 131_070; // 65,535 items + 65,535 representations.
fn class(name: &str) -> char {
    match name {
        "public.utf8-plain-text"
        | "public.utf16-plain-text"
        | "public.utf16-external-plain-text"
        | "NSStringPboardType" => 'B',
        "public.html" | "NSHTMLPboardType" => 'C',
        "public.rtf" | "NSRTFPboardType" => 'D',
        "public.png" => 'E',
        "public.tiff" | "NSTIFFPboardType" => 'F',
        "public.jpeg" => 'G',
        "public.url" | "Apple URL pasteboard type" => 'H',
        "public.url-name" => 'I',
        "public.file-url" | "NSFilenamesPboardType" => 'J',
        "com.apple.pasteboard.promised-file-content-type" => 'K',
        "com.apple.pasteboard.promised-file-url" => 'L',
        "com.apple.webarchive" | "Apple Web Archive pasteboard type" => 'N',
        "com.compuserve.gif" | "org.webmproject.webp" | "public.heic" | "public.avif" => 'Q',
        _ if super::super::content::promise_type(name) => 'M',
        _ if crate::formats::is_marker(oink_foundation::clipboard::ClipPlatform::Mac, name) => 'P',
        _ if name.starts_with("org.chromium.") || name.starts_with("com.apple.WebKit.") => 'O',
        _ => 'A',
    }
}

fn summarize(board: &NSPasteboard) -> (String, usize, bool) {
    let Some(items) = board.pasteboardItems() else {
        return (String::new(), 0, false);
    };
    let mut classes = String::new();
    for item in &items {
        for ty in item.types() {
            if classes.len() == LIMIT {
                return (classes, items.len(), false);
            }
            classes.push(class(&ty.to_string()));
        }
        if classes.len() == LIMIT {
            return (classes, items.len(), false);
        }
        classes.push(';');
    }
    (classes, items.len(), true)
}

pub(super) fn record(board: &NSPasteboard, operation: DragOperation, presented: bool) {
    let (classes, count, complete) = summarize(board);
    tracing::debug!(
        target: "oink_clipboard::macos::dnd",
        dnd_hold_sequence = operation.sequence,
        dnd_representation_schema = 1_u64,
        dnd_representation_stage = u64::from(presented),
        dnd_representation_items = count as u64,
        dnd_representation_complete = complete,
        dnd_representation_classes = classes.as_str(),
        "native drag ordered representation classes"
    );
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    #[test]
    fn native_classes_preserve_items_order_and_multiplicity_without_reading_data() {
        let board = crate::macos::test_pasteboard::new();
        let first = NSPasteboardItem::new();
        for name in [
            "public.tiff",
            "public.url",
            "public.url-name",
            "public.jpeg",
            "com.apple.pasteboard.promised-file-url",
            "com.apple.pasteboard.promised-file-content-type",
            "public.html",
            "com.apple.webarchive",
            "org.chromium.image-html",
            "private.secret-name",
        ] {
            first.setData_forType(&NSData::with_bytes(&[]), &NSString::from_str(name));
        }
        let second = NSPasteboardItem::new();
        second.setData_forType(&NSData::with_bytes(&[]), &NSString::from_str("private.other-secret"));
        second.setData_forType(&NSData::with_bytes(&[]), &NSString::from_str("public.tiff"));
        assert!(board.writeObjects(&NSArray::from_retained_slice(&[
            ProtocolObject::from_retained(first),
            ProtocolObject::from_retained(second),
        ])));
        assert_eq!(summarize(&board), ("FHIGLKCNOA;AF;".into(), 2, true));
        board.clearContents();
    }
}
