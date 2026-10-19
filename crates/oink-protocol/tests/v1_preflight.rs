use oink_protocol::v1::{ErrorCode, Message};
#[test]
fn known_list_and_blob_limits_are_checked_without_materializing_a_tree() {
    // A declared 65,535-key HeldSync list exceeds 256 immediately. Its absent
    // elements would otherwise cause a truncation error; the limit wins first.
    assert_eq!(
        Message::preflight(0x205, &[0xa1, 1, 0x99, 0xff, 0xff])
            .unwrap_err()
            .code,
        ErrorCode::LimitExceeded
    );
    // The same guard applies recursively inside typed facts before RawValue Vecs.
    assert_eq!(
        Message::preflight(0xd08, &[0xa1, 12, 0x99, 1, 1]).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
    let mut audio = vec![0xa1, 6, 0x59, 3, 0xc1];
    audio.extend(vec![0; 961]);
    assert_eq!(
        Message::preflight(0x604, &audio).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
}
