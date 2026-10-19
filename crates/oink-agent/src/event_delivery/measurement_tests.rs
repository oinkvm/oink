//! Measurement changes do not create semantic state events.
use super::*;
use oink_audio::{AudioPhase, CaptureLevels, CapturePermission, DeviceAudioState, DeviceState, SampleLevels, Status};
use oink_foundation::DeviceId;
use std::sync::Arc;

#[test]
fn measurement_only_fields_are_not_state_events_even_for_all_views() {
    let group = test_group();
    let mut rtt = group.clone();
    *rtt.members
        .iter_mut()
        .find_map(|m| m.link.as_mut())
        .unwrap()
        .rtt_ms
        .as_mut()
        .unwrap() += 1.;
    let levels = CaptureLevels {
        window_sequence: 1,
        window_ms: 1000.,
        age_ms: 0,
        before_gain: SampleLevels {
            peak_dbfs: None,
            rms_dbfs: None,
            zero_frame_fraction: 1.,
            frames: 48_000,
            nonfinite_samples: 0,
        },
        after_gain: None,
        wasapi_buffers: None,
        wasapi_silent_buffers: None,
    };
    let audio = Status {
        capture_levels: Some(levels),
        network_rtt_ms: Some(1.),
        device_states: vec![DeviceState {
            device: DeviceId([7; 32]),
            age_ms: 0,
            state: DeviceAudioState {
                phase: AudioPhase::Forwarding,
                problem: None,
                permission: CapturePermission::Authorized,
                source_muted: false,
                forwarding_enabled: true,
                forwarding_off_reason: None,
                forwarding_muted: false,
                forwarding_volume_percent: 100,
                mute_release_unconfirmed: false,
                sent: 0,
                send_dropped: 0,
                outputs: vec![],
                receivers: vec![],
            },
        }],
        ..Default::default()
    };
    let mut cases = vec![("group.rtt_ms", ApiEvent::Group(group), ApiEvent::Group(rtt))];
    for field in [
        "audio.rtt_ms",
        "audio.report_age_ms",
        "audio.stale_age_ms",
        "audio.level_age_ms",
        "audio.level_sequence",
        "audio.sent",
    ] {
        let mut changed = audio.clone();
        match field {
            "audio.rtt_ms" => changed.network_rtt_ms = Some(2.),
            "audio.report_age_ms" => changed.device_states[0].age_ms = 100,
            "audio.stale_age_ms" => changed.device_states[0].age_ms = 4000,
            "audio.level_age_ms" => changed.capture_levels.as_mut().unwrap().age_ms = 100,
            "audio.level_sequence" => changed.capture_levels.as_mut().unwrap().window_sequence += 1,
            "audio.sent" => changed.sent += 1,
            _ => unreachable!(),
        }
        cases.push((field, ApiEvent::Audio(audio.clone()), ApiEvent::Audio(changed)));
    }
    let mut leaked = Vec::new();
    for (field, before, after) in cases {
        let mut client = Subscriber::new(1, Arc::new(|_| {}), LiveViews::ALL);
        assert!(client.accepts(&before));
        let pushes = client.accepts(&after);
        eprintln!("MEASUREMENT_AUDIT {field} state_push={pushes}");
        if pushes {
            leaked.push(field);
        }
    }
    assert!(leaked.is_empty(), "measurements leaked into state events: {leaked:?}");
}
