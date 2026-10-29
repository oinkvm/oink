//! Shared-mode queue timing is not device-output continuity evidence.
use std::time::{Duration, Instant};

pub(crate) struct SharedRenderObservation {
    pub playback_at: Option<Instant>,
    pub native_gap_frames: Option<u64>,
}

/// GetCurrentPadding counts frames the audio engine has not yet read. The
/// IAudioClock position describes device playback, downstream of that queue.
/// Their separately sampled values do not establish which frames were absent
/// at the device. In particular, an event can follow a healthy engine handoff
/// with zero padding while the clock-query phase varies between callbacks.
///
/// Never turn that phase variation into a PCM repair or a known-zero gap count.
/// Keep the existing padding-based presentation estimate when it is available;
/// actual native continuity stays unknown until an independent gap signal exists.
/// See IAudioClock::GetPosition and IAudioClient::GetCurrentPadding contracts.
pub(crate) fn observe_shared_render(
    clock_at: Option<Instant>,
    padding_at: Instant,
    callback_at: Instant,
    padding_frames: u32,
    sample_rate: u32,
) -> SharedRenderObservation {
    let playback_at = clock_at
        .filter(|_| padding_frames != 0 && sample_rate != 0)
        .and_then(|clock_at| {
            let padding_time = clock_at.checked_add(padding_at.checked_duration_since(clock_at)?)?;
            padding_time.checked_add(Duration::from_secs_f64(padding_frames as f64 / sample_rate as f64))
        });
    SharedRenderObservation {
        playback_at: playback_at.filter(|predicted| *predicted > callback_at),
        native_gap_frames: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_estimate_does_not_claim_native_continuity() {
        let now = Instant::now();
        for padding in [0, 1, 480, 960] {
            for clock in [None, Some(now - Duration::from_millis(1)), Some(now)] {
                let observation = observe_shared_render(clock, now, now, padding, 48_000);
                assert_eq!(observation.native_gap_frames, None);
                assert_eq!(observation.playback_at.is_some(), clock.is_some() && padding != 0);
                if let Some(at) = observation.playback_at {
                    assert_eq!(at, now + Duration::from_secs_f64(padding as f64 / 48_000.0));
                }
            }
        }
    }

    #[test]
    fn invalid_or_expired_presentation_estimates_remain_unknown() {
        let now = Instant::now();
        for (clock, callback, rate) in [
            (now + Duration::from_millis(1), now, 48_000),
            (now, now + Duration::from_millis(20), 48_000),
            (now, now, 0),
        ] {
            let observation = observe_shared_render(Some(clock), now, callback, 480, rate);
            assert_eq!(observation.playback_at, None);
            assert_eq!(observation.native_gap_frames, None);
        }
    }
}
