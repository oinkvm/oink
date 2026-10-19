use oink_protocol::v1::{self, ErrorCode, RawValue as V, dto::*, schema::WireDto};
use oink_test_support::protocol as support;

#[test]
fn game_state_uses_reference_lengths_and_keeps_future_phase() {
    let mut raw = support::sample_named("AimState");
    raw.insert(3, V::Uint(99));
    let state = AimState::from_raw(raw.clone()).unwrap();
    assert_eq!(state.phase().unwrap(), AimPhase::Unknown(99));
    raw.insert(7, V::F32(7.0));
    assert_eq!(AimState::from_raw(raw).unwrap_err().code, ErrorCode::UnsupportedValue);
}
#[test]
fn wallpaper_payload_is_bounded_before_materialization() {
    let mut screen = support::sample_named("ScreenWallpaper");
    screen.insert(4, V::Bytes(vec![0; 262145]));
    let mut raw = support::sample_named("WallpaperSet");
    raw.insert(2, V::Array(vec![V::Map(screen)]));
    let bytes = v1::cbor::encode_map(&raw, 1024 * 1024).unwrap();
    assert_eq!(
        v1::Message::preflight(0x0800, &bytes).unwrap_err().code,
        ErrorCode::LimitExceeded
    );
}
