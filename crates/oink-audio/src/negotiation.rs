//! Executable codec profiles and their explicit v1 descriptors. Unknown codecs,
//! parameters or frame durations never select a guessed implementation.
use crate::dsp;
use oink_protocol::v2::{
    RawMap, RawValue,
    dto::{AudioDescriptor, AudioSupport, ControlFailure},
    schema::{WireDto, WireValue},
};
use serde::{Deserialize, Serialize};

pub const CAPABILITIES: &[&str] = &["oink.audio/2", "oink.audio.microphone/1"];
pub const SAMPLE_RATES: &[u32] = &[
    8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 64_000, 88_200, 96_000, 176_400, 192_000,
];
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Smooth,
    #[default]
    Quality,
    Lossless,
}
impl Quality {
    pub fn profile(self, sample_rate: u32) -> Profile {
        match self {
            Self::Smooth => Profile::Opus5ms,
            Self::Quality => Profile::Opus20ms,
            Self::Lossless => Profile::Pcm24 { sample_rate },
        }
    }
    pub fn number(self) -> u32 {
        match self {
            Self::Smooth => 0,
            Self::Quality => 1,
            Self::Lossless => 2,
        }
    }
    pub fn from_number(number: u32) -> Option<Self> {
        match number {
            0 => Some(Self::Smooth),
            1 => Some(Self::Quality),
            2 => Some(Self::Lossless),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Opus5ms,
    Pcm16_2p5ms,
    #[default]
    Opus20ms,
    Pcm24 {
        sample_rate: u32,
    },
}
impl Profile {
    pub const ALL: [Self; 3] = [Self::Opus5ms, Self::Opus20ms, Self::Pcm24 { sample_rate: 48_000 }];
    pub fn config(self) -> dsp::Config {
        match self {
            Self::Opus5ms => dsp::Config::low_latency_opus(),
            Self::Pcm16_2p5ms => dsp::Config::low_latency_pcm(),
            Self::Opus20ms => dsp::Config::music_opus(),
            Self::Pcm24 { sample_rate } => dsp::Config::lossless(sample_rate),
        }
    }
    pub fn codec(self) -> &'static str {
        match self {
            Self::Opus5ms | Self::Opus20ms => "opus",
            Self::Pcm16_2p5ms => "pcm-s16le",
            Self::Pcm24 { .. } => "pcm-s24le",
        }
    }
    pub fn frame_us(self) -> u32 {
        match self {
            Self::Opus5ms => 5000,
            Self::Pcm16_2p5ms => 2500,
            Self::Opus20ms => 20_000,
            Self::Pcm24 { sample_rate } => 160_000_000 / sample_rate,
        }
    }
    pub fn max_payload(self) -> u16 {
        match self {
            Self::Opus5ms => 960,
            Self::Pcm16_2p5ms => 480,
            Self::Opus20ms | Self::Pcm24 { .. } => 960,
        }
    }
    pub fn descriptor(self) -> AudioDescriptor {
        AudioDescriptor::from_raw(RawMap::from([
            (1, self.codec().into()),
            (2, self.config().sample_rate.write()),
            (3, 2_u8.write()),
            (4, self.frame_us().write()),
            (5, self.max_payload().write()),
            (6, RawValue::Map(RawMap::new())),
        ]))
        .expect("implemented audio descriptor")
    }
    pub fn from_descriptor(descriptor: &AudioDescriptor) -> Result<Self, ControlFailure> {
        if descriptor.channels() != Ok(2)
            || descriptor
                .parameters()
                .map_or(true, |parameters| !parameters.is_empty())
        {
            return Err(ControlFailure::Unsupported);
        }
        let rate = descriptor.sample_rate().map_err(|_| ControlFailure::Unsupported)?;
        if !SAMPLE_RATES.contains(&rate) {
            return Err(ControlFailure::Unsupported);
        }
        [Self::Opus5ms, Self::Opus20ms, Self::Pcm24 { sample_rate: rate }]
            .into_iter()
            .find(|profile| {
                descriptor.sample_rate() == Ok(profile.config().sample_rate)
                    && descriptor.codec().is_ok_and(|codec| codec.0 == profile.codec())
                    && descriptor.frame_us() == Ok(profile.frame_us())
                    && descriptor.max_payload() == Ok(profile.max_payload())
            })
            .ok_or(ControlFailure::Unsupported)
    }
    pub fn supported_by(self, offers: &[AudioSupport], quality: bool, datagram_limit: usize) -> bool {
        // Full-width counters, the v1 frame and network flow identifier fit in
        // this bound; a smaller path must not silently truncate encoded media.
        if self == Self::Pcm16_2p5ms
            || (self != Self::Opus5ms && !quality)
            || datagram_limit < usize::from(self.max_payload()) + 63
        {
            return false;
        }
        offers.iter().any(|offer| {
            offer.codec().is_ok_and(|codec| codec.0 == self.codec())
                && offer
                    .sample_rates()
                    .is_ok_and(|rates| rates.0.contains(&self.config().sample_rate))
                && offer.channels().is_ok_and(|channels| channels.0.contains(&2))
                && offer.frame_us().is_ok_and(|frames| frames.0.contains(&self.frame_us()))
                && offer.max_payload().is_ok_and(|maximum| maximum >= self.max_payload())
        })
    }
}
pub fn audio() -> Vec<AudioSupport> {
    [
        Profile::Opus5ms,
        Profile::Opus20ms,
        Profile::Pcm24 { sample_rate: 48_000 },
    ]
    .into_iter()
    .map(|profile| {
        AudioSupport::from_raw(RawMap::from([
            (1, profile.codec().into()),
            (
                2,
                RawValue::Array(if matches!(profile, Profile::Pcm24 { .. }) {
                    SAMPLE_RATES.iter().map(WireValue::write).collect()
                } else {
                    vec![48_000_u32.write()]
                }),
            ),
            (3, RawValue::Array(vec![2_u8.write()])),
            (
                4,
                RawValue::Array(if matches!(profile, Profile::Pcm24 { .. }) {
                    SAMPLE_RATES
                        .iter()
                        .map(|rate| (160_000_000_u32 / rate).write())
                        .collect()
                } else {
                    vec![profile.frame_us().write()]
                }),
            ),
            (5, profile.max_payload().write()),
        ]))
        .expect("implemented audio support")
    })
    .collect()
}
pub fn select(
    preferred: Profile,
    offers: &[AudioSupport],
    quality: bool,
    datagram_limit: usize,
) -> Result<Profile, ControlFailure> {
    std::iter::once(preferred)
        .chain([Profile::Opus5ms])
        .find(|profile| profile.supported_by(offers, quality, datagram_limit))
        .ok_or(ControlFailure::Unsupported)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptors_require_exact_executable_parameters_and_negotiated_pcm() {
        for profile in Profile::ALL {
            assert_eq!(Profile::from_descriptor(&profile.descriptor()), Ok(profile));
        }
        for &sample_rate in SAMPLE_RATES {
            let profile = Profile::Pcm24 { sample_rate };
            profile.config().validate().unwrap();
            assert_eq!(Profile::from_descriptor(&profile.descriptor()), Ok(profile));
            assert_eq!(select(profile, &audio(), true, 1100), Ok(profile));
            assert_eq!(select(profile, &audio(), false, 1100), Ok(Profile::Opus5ms));
        }
        assert_eq!(select(Profile::Opus20ms, &audio(), false, 1100), Ok(Profile::Opus5ms));
        assert_eq!(
            select(Profile::Pcm24 { sample_rate: 47_000 }, &audio(), true, 1100),
            Ok(Profile::Opus5ms)
        );
        let mut old = Profile::Opus5ms.descriptor();
        old.edit(|raw| {
            raw.insert(4, 10_000_u32.write());
        })
        .unwrap();
        assert_eq!(Profile::from_descriptor(&old), Err(ControlFailure::Unsupported));
        assert_eq!(
            select(Profile::Pcm24 { sample_rate: 48_000 }, &audio(), false, 1100),
            Ok(Profile::Opus5ms)
        );
        assert_eq!(
            select(Profile::Pcm24 { sample_rate: 48_000 }, &audio(), true, 1100),
            Ok(Profile::Pcm24 { sample_rate: 48_000 })
        );
        assert_eq!(
            select(Profile::Opus5ms, &audio(), false, 512),
            Err(ControlFailure::Unsupported)
        );
        let mut future = Profile::Opus5ms.descriptor();
        future
            .edit(|raw| {
                raw.insert(6, RawValue::Map([(7, true.into())].into()));
            })
            .unwrap();
        assert_eq!(Profile::from_descriptor(&future), Err(ControlFailure::Unsupported));
    }
}
