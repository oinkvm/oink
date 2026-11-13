//! An iPhone's speaker-exit volume: Oink's own playout gain.
//!
//! iOS has no public API that sets the system output volume
//! (`AVAudioSession.outputVolume` is read-only; `MPVolumeView` is only a
//! user-operated slider), so an iPhone exit cannot do what a PC or Android exit
//! does with forwarded volume keys and slider writes. It scales what Oink plays
//! instead (user decision 2026-11-12). The value is shown as 0–100 like every
//! exit volume and executed with the desktop source's 0–200 forwarding gain
//! curve (`oink_audio::forward_volume`) at position 2 × value: 0 is silence,
//! the default 50 is unity (the loudness before this existed) and 100 the
//! curve's maximum (+12.1 dB, peak-limited). A volume key moves 10 gain
//! positions (5 shown points), the forwarding gain's key step; volume up/down
//! clear the mute and mute toggles it, as the system volume keys of the other
//! exits do. Persisted on this phone only; the system volume stays the user's.
use oink_audio::{
    forward_volume,
    volume::{Endpoint, Fault, KeyAction, Reading},
};
use oink_foundation::storage;
use oink_protocol::v2::dto::ExitVolumeStatus;
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Unity gain: today's loudness.
pub(crate) const DEFAULT_PERCENT: u8 = 50;

pub(crate) struct PlayoutVolume {
    path: PathBuf,
    current: Mutex<Reading>,
}

impl std::fmt::Debug for PlayoutVolume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlayoutVolume")
            .field("current", &self.reading())
            .finish_non_exhaustive()
    }
}

impl PlayoutVolume {
    /// A missing or unreadable file starts at the default (no migration: the
    /// format is version 1 until release).
    pub(crate) fn open(path: PathBuf) -> Arc<Self> {
        let current = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<(u8, bool)>(&bytes).ok())
            .filter(|(percent, _)| *percent <= 100)
            .map_or(
                Reading {
                    percent: DEFAULT_PERCENT,
                    muted: false,
                },
                |(percent, muted)| Reading { percent, muted },
            );
        Arc::new(Self {
            path,
            current: Mutex::new(current),
        })
    }

    pub(crate) fn reading(&self) -> Reading {
        *self.current.lock().expect("playout volume mutex")
    }

    /// Persists before it applies: a failed save changes nothing.
    pub(crate) fn set(&self, reading: Reading) -> Result<(), Fault> {
        if reading.percent > 100 {
            return Err(failed());
        }
        let mut current = self.current.lock().expect("playout volume mutex");
        if *current == reading {
            return Ok(());
        }
        let bytes = serde_json::to_vec(&(reading.percent, reading.muted)).map_err(|_| failed())?;
        storage::atomic_write(&self.path, &bytes).map_err(|error| {
            tracing::warn!(%error, "playout volume not saved");
            failed()
        })?;
        *current = reading;
        Ok(())
    }

    pub(crate) fn key(&self, action: KeyAction) -> Result<(), Fault> {
        let mut next = self.reading();
        match action {
            KeyAction::Up | KeyAction::Down => {
                let position = forward_volume::adjust(u16::from(next.percent) * 2, action == KeyAction::Up);
                next.percent = (position / 2) as u8;
                next.muted = false;
            }
            KeyAction::Mute => next.muted = !next.muted,
        }
        self.set(next)
    }

    /// The forwarding-gain position (0..=200) the playout applies now.
    pub(crate) fn gain_percent(&self) -> u16 {
        gain_percent(self.reading())
    }
}

/// Shown value → forwarding-gain position; mute is silence.
pub(crate) fn gain_percent(reading: Reading) -> u16 {
    if reading.muted {
        0
    } else {
        u16::from(reading.percent.min(100)) * 2
    }
}

fn failed() -> Fault {
    Fault {
        status: ExitVolumeStatus::BackendFailed,
        native_code: None,
    }
}

/// The exit-volume endpoint over the playout gain. Nothing else changes the
/// value, so it never reports an outside change.
pub(crate) struct PlayoutEndpoint(pub(crate) Arc<PlayoutVolume>);

impl Endpoint for PlayoutEndpoint {
    fn changed(&self) -> bool {
        false
    }

    fn read(&mut self) -> Result<Reading, Fault> {
        Ok(self.0.reading())
    }

    fn write(&mut self, reading: Reading) -> Result<(), Fault> {
        self.0.set(reading)
    }

    fn key(&mut self, action: KeyAction) -> Result<(), Fault> {
        self.0.key(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(dir: &tempfile::TempDir) -> Arc<PlayoutVolume> {
        PlayoutVolume::open(dir.path().join("playout-volume.json"))
    }

    #[test]
    fn shown_value_maps_onto_the_forwarding_gain_curve() {
        let reading = |percent| Reading { percent, muted: false };
        // 0 is silence, 50 is unity (today's loudness), 100 the curve's maximum.
        assert_eq!(gain_percent(reading(0)), 0);
        assert_eq!(forward_volume::amplitude(gain_percent(reading(0))), 0.0);
        assert_eq!(gain_percent(reading(DEFAULT_PERCENT)), 100);
        assert_eq!(forward_volume::amplitude(gain_percent(reading(50))), 1.0);
        assert_eq!(gain_percent(reading(100)), 200);
        let max = forward_volume::decibels(f64::from(gain_percent(reading(100)))).unwrap();
        assert!((max - forward_volume::MAX_BOOST_DB).abs() < 1e-6, "{max}");
        // Monotonic between, and mute is silence at any value.
        for percent in 1..=100 {
            assert!(
                forward_volume::amplitude(gain_percent(reading(percent)))
                    > forward_volume::amplitude(gain_percent(reading(percent - 1)))
            );
            assert_eq!(gain_percent(Reading { percent, muted: true }), 0);
        }
    }

    #[test]
    fn defaults_to_unity_and_persists_what_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let first = volume(&dir);
        assert_eq!(
            first.reading(),
            Reading {
                percent: 50,
                muted: false
            }
        );
        assert_eq!(first.gain_percent(), 100);
        first
            .set(Reading {
                percent: 73,
                muted: true,
            })
            .unwrap();
        let reopened = volume(&dir);
        assert_eq!(
            reopened.reading(),
            Reading {
                percent: 73,
                muted: true
            }
        );
        assert_eq!(reopened.gain_percent(), 0);
        assert!(
            reopened
                .set(Reading {
                    percent: 101,
                    muted: false
                })
                .is_err()
        );
        assert_eq!(volume(&dir).reading().percent, 73);
    }

    #[test]
    fn an_invalid_file_starts_at_the_default() {
        let dir = tempfile::tempdir().unwrap();
        for bytes in [&b"[150,false]"[..], b"not json"] {
            fs::write(dir.path().join("playout-volume.json"), bytes).unwrap();
            assert_eq!(volume(&dir).reading().percent, DEFAULT_PERCENT);
        }
    }

    #[test]
    fn keys_step_like_the_forwarding_gain_and_toggle_mute() {
        let dir = tempfile::tempdir().unwrap();
        let v = volume(&dir);
        v.key(KeyAction::Up).unwrap();
        assert_eq!(v.reading().percent, 55, "one key = 10 gain positions");
        v.key(KeyAction::Mute).unwrap();
        assert!(v.reading().muted);
        assert_eq!(v.gain_percent(), 0);
        v.key(KeyAction::Mute).unwrap();
        assert!(!v.reading().muted);
        v.key(KeyAction::Mute).unwrap();
        v.key(KeyAction::Down).unwrap();
        assert_eq!(
            v.reading(),
            Reading {
                percent: 50,
                muted: false
            },
            "a volume key clears the mute"
        );
        for _ in 0..30 {
            v.key(KeyAction::Up).unwrap();
        }
        assert_eq!(v.reading().percent, 100);
        for _ in 0..30 {
            v.key(KeyAction::Down).unwrap();
        }
        assert_eq!(v.reading().percent, 0);
        assert_eq!(volume(&dir).reading().percent, 0, "keys persist too");
    }
}
