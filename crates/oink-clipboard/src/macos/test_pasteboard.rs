//! Private native pasteboards for parallel tests; never accesses the general board.
use objc2::rc::Retained;
use objc2_app_kit::NSPasteboard;
use objc2_foundation::{NSString, NSUUID};
use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
};

pub(super) fn new() -> Retained<NSPasteboard> {
    // On macOS 14.7.8, concurrent pasteboardWithUniqueName calls returned the
    // same CFPasteboardUnique name, mixing fixtures' items and native owners.
    // A named board with our own atomic suffix isolates concurrent fixtures
    // without serializing the operations being tested. The UUID scopes the
    // counter to this test process; the general/drag pasteboards are untouched.
    static NAMESPACE: OnceLock<String> = OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let namespace = NAMESPACE.get_or_init(|| NSUUID::UUID().UUIDString().to_string());
    let name = format!("oink-test-{namespace}-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    NSPasteboard::pasteboardWithName(&NSString::from_str(&name))
}

#[test]
fn concurrent_private_boards_keep_distinct_names_and_contents() {
    use objc2::{rc::autoreleasepool, runtime::ProtocolObject};
    use objc2_app_kit::NSPasteboardItem;
    use objc2_foundation::{NSArray, NSString};
    use std::{collections::HashSet, sync::Barrier};

    const WORKERS: usize = 16;
    const BOARDS: usize = 8;
    let ready = Barrier::new(WORKERS);
    let names = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..WORKERS)
            .map(|worker| {
                let ready = &ready;
                scope.spawn(move || {
                    autoreleasepool(|_| {
                        ready.wait();
                        let boards: Vec<_> = (0..BOARDS)
                            .map(|index| {
                                let board = new();
                                let name = board.name().to_string();
                                let expected = format!("private fixture {worker}/{index}");
                                let item = NSPasteboardItem::new();
                                let ty = NSString::from_str("public.utf8-plain-text");
                                item.setString_forType(&NSString::from_str(&expected), &ty);
                                let written = board.writeObjects(&NSArray::from_retained_slice(&[
                                    ProtocolObject::from_retained(item),
                                ]));
                                (board, name, expected, written)
                            })
                            .collect();
                        // Keep every board alive until all concurrent writers finish.
                        ready.wait();
                        let observed: Vec<_> = boards
                            .into_iter()
                            .map(|(board, name, expected, written)| {
                                let actual = board
                                    .stringForType(&NSString::from_str("public.utf8-plain-text"))
                                    .map(|value| value.to_string());
                                // SAFETY: releaseGlobally is a public NSPasteboard
                                // selector; this fixture owns only private boards.
                                unsafe {
                                    let _: () = objc2::msg_send![&*board, releaseGlobally];
                                }
                                (name, expected, written, actual)
                            })
                            .collect();
                        let mut names = Vec::new();
                        for (name, expected, written, actual) in observed {
                            assert!(written, "private fixture write failed: {name}");
                            assert_eq!(actual.as_deref(), Some(expected.as_str()), "board={name}");
                            names.push(name);
                        }
                        names
                    })
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(names.len(), WORKERS * BOARDS);
    assert_eq!(names.iter().collect::<HashSet<_>>().len(), names.len());
}
