//! Executable codec profiles and their explicit v1 descriptors. Unknown codecs,
//! parameters or frame durations never select a guessed implementation.
use crate::dsp;
use oink_protocol::v1::{
    RawMap, RawValue,
    dto::{AudioDescriptor, AudioSupport, ControlFailure},
    schema::{WireDto, WireValue},
};
use serde::{Deserialize, Serialize};

pub const CAPABILITIES: &[&str] = &[
    "oink.audio/1",
    "oink.audio.pcm/1",
    "oink.audio.state/1",
    "oink.audio.exit-volume/1",
    "oink.audio.local-output/1",
];
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    #[default]
    Opus5ms,
    Pcm16_2p5ms,
}
impl Profile {
    pub const ALL: [Self; 2] = [Self::Opus5ms, Self::Pcm16_2p5ms];
    pub fn config(self) -> dsp::Config {
        match self {
            Self::Opus5ms => dsp::Config::low_latency_opus(),
            Self::Pcm16_2p5ms => dsp::Config::low_latency_pcm(),
        }
    }
    pub fn codec(self) -> &'static str {
        match self {
            Self::Opus5ms => "opus",
            Self::Pcm16_2p5ms => "pcm-s16le",
        }
    }
    pub fn frame_us(self) -> u32 {
        match self {
            Self::Opus5ms => 5000,
            Self::Pcm16_2p5ms => 2500,
        }
    }
    pub fn max_payload(self) -> u16 {
        match self {
            Self::Opus5ms => 960,
            Self::Pcm16_2p5ms => 480,
        }
    }
    pub fn descriptor(self) -> AudioDescriptor {
        AudioDescriptor::from_raw(RawMap::from([
            (1, self.codec().into()),
            (2, 48_000_u32.write()),
            (3, 2_u8.write()),
            (4, self.frame_us().write()),
            (5, self.max_payload().write()),
            (6, RawValue::Map(RawMap::new())),
        ]))
        .expect("implemented audio descriptor")
    }
    pub fn from_descriptor(descriptor: &AudioDescriptor) -> Result<Self, ControlFailure> {
        if descriptor.sample_rate() != Ok(48_000)
            || descriptor.channels() != Ok(2)
            || descriptor
                .parameters()
                .map_or(true, |parameters| !parameters.is_empty())
        {
            return Err(ControlFailure::Unsupported);
        }
        Self::ALL
            .into_iter()
            .find(|profile| {
                descriptor.codec().is_ok_and(|codec| codec.0 == profile.codec())
                    && descriptor.frame_us() == Ok(profile.frame_us())
                    && descriptor.max_payload() == Ok(profile.max_payload())
            })
            .ok_or(ControlFailure::Unsupported)
    }
    pub fn supported_by(self, offers: &[AudioSupport], pcm: bool, datagram_limit: usize) -> bool {
        // Full-width counters, the v1 frame and network flow identifier fit in
        // this bound; a smaller path must not silently truncate encoded media.
        if (self == Self::Pcm16_2p5ms && !pcm) || datagram_limit < usize::from(self.max_payload()) + 63 {
            return false;
        }
        offers.iter().any(|offer| {
            offer.codec().is_ok_and(|codec| codec.0 == self.codec())
                && offer.sample_rates().is_ok_and(|rates| rates.0.contains(&48_000))
                && offer.channels().is_ok_and(|channels| channels.0.contains(&2))
                && offer.frame_us().is_ok_and(|frames| frames.0.contains(&self.frame_us()))
                && offer.max_payload().is_ok_and(|maximum| maximum >= self.max_payload())
        })
    }
}
pub fn audio() -> Vec<AudioSupport> {
    Profile::ALL
        .into_iter()
        .map(|profile| {
            AudioSupport::from_raw(RawMap::from([
                (1, profile.codec().into()),
                (2, RawValue::Array(vec![48_000_u32.write()])),
                (3, RawValue::Array(vec![2_u8.write()])),
                (4, RawValue::Array(vec![profile.frame_us().write()])),
                (5, profile.max_payload().write()),
            ]))
            .expect("implemented audio support")
        })
        .collect()
}
pub fn select(
    preferred: Profile,
    offers: &[AudioSupport],
    pcm: bool,
    datagram_limit: usize,
) -> Result<Profile, ControlFailure> {
    std::iter::once(preferred)
        .chain(Profile::ALL)
        .find(|profile| profile.supported_by(offers, pcm, datagram_limit))
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
        let mut old = Profile::Opus5ms.descriptor();
        old.edit(|raw| {
            raw.insert(4, 10_000_u32.write());
        })
        .unwrap();
        assert_eq!(Profile::from_descriptor(&old), Err(ControlFailure::Unsupported));
        assert_eq!(
            select(Profile::Pcm16_2p5ms, &audio(), false, 1100),
            Ok(Profile::Opus5ms)
        );
        assert_eq!(
            select(Profile::Pcm16_2p5ms, &audio(), true, 1100),
            Ok(Profile::Pcm16_2p5ms)
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
