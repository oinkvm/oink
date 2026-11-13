//! Device-owned microphone gain. All listeners hear the same processed stream.
use oink_foundation::{session::Boot, storage};
use oink_network::wire_identity::boot;
use oink_protocol::v2::{
    self, dto,
    schema::{WireDto, WireValue},
};
use std::{collections::VecDeque, fs, io, path::PathBuf};

pub(crate) struct MicrophoneControls {
    path: PathBuf,
    option: dto::LocalOption,
    state: dto::LocalOptionState,
    writable: bool,
    completed: VecDeque<(Boot, dto::SetLocalOption, dto::LocalEditResult)>,
}

impl MicrophoneControls {
    pub(crate) fn open(path: PathBuf) -> Self {
        Self::open_option(path, dto::LocalOption::MicrophoneControls)
    }

    pub(crate) fn open_option(path: PathBuf, option: dto::LocalOption) -> Self {
        let stored = fs::read(&path)
            .map_err(|error| error.kind())
            .and_then(|bytes| dto::LocalOptionState::decode(&bytes, 1024).map_err(|_| io::ErrorKind::InvalidData));
        let writable = match &stored {
            Ok(state) => {
                state.option() == Ok(option)
                    && state
                        .value()
                        .and_then(|value| dto::ForwardingControlsValue::read(&value))
                        .is_ok()
            }
            Err(error) => *error == io::ErrorKind::NotFound,
        };
        let state = stored.ok().filter(|_| writable).unwrap_or_else(|| {
            dto::LocalOptionState::from_raw(
                [
                    (1, option.write()),
                    (2, v2::RawValue::Map([(1, 100u16.write()), (2, false.write())].into())),
                    (3, 1u64.write()),
                ]
                .into(),
            )
            .expect("microphone gain defaults")
        });
        Self {
            path,
            option,
            state,
            writable,
            completed: VecDeque::new(),
        }
    }

    pub(crate) fn gain(&self) -> (u16, bool) {
        let value = self
            .state
            .value()
            .and_then(|value| dto::ForwardingControlsValue::read(&value))
            .expect("validated microphone controls");
        (
            value.volume_percent().expect("microphone gain"),
            value.muted().expect("microphone mute"),
        )
    }

    pub(crate) fn snapshot(&self) -> crate::MobileMicrophoneControls {
        let (volume_percent, muted) = self.gain();
        crate::MobileMicrophoneControls {
            volume_percent,
            muted,
            revision: self.state.revision().expect("validated microphone revision"),
        }
    }

    /// Owner-local edit (this phone's own UI). Same persistence and revision
    /// advance as a remote C0 edit; returns whether the value changed.
    pub(crate) fn set_local(&mut self, volume_percent: u16, muted: bool) -> Result<bool, dto::ControlFailure> {
        let revision = self.state.revision().map_err(|_| dto::ControlFailure::InvalidValue)?;
        let value = dto::ForwardingControlsValue::from_raw([(1, volume_percent.write()), (2, muted.write())].into())
            .map_err(|_| dto::ControlFailure::InvalidValue)?;
        self.commit(revision, value)
    }

    pub(crate) fn preference(&self) -> v2::Result<dto::DevicePreference> {
        dto::DevicePreference::from_raw(
            [
                (1, self.option.write()),
                (2, self.state.revision()?.write()),
                (3, self.state.value()?),
            ]
            .into(),
        )
    }

    pub(crate) fn handle(
        &mut self,
        local: Boot,
        peer: Boot,
        request: dto::SetLocalOption,
        supported: bool,
    ) -> v2::Result<dto::LocalEditResult> {
        use dto::{ControlFailure as Failure, ControlOutcome as Outcome};
        let id = request.request_id()?;
        let option = request.option()?;
        if let Some((_, original, reply)) = self
            .completed
            .iter()
            .find(|(boot, prior, _)| *boot == peer && prior.request_id().as_ref() == Ok(&id))
        {
            if original == &request {
                return Ok(reply.clone());
            }
            return self.reply(local, &request, Outcome::Refused, Failure::StateConflict);
        }
        let applied = if request.target()? != boot(local) {
            Err(Failure::StaleBoot)
        } else if option != self.option {
            Err(Failure::UnsupportedOption)
        } else if !supported {
            Err(Failure::Unsupported)
        } else {
            self.apply(&request)
        };
        let (outcome, reason) = match applied {
            Ok(true) => (Outcome::Applied, Failure::None),
            Ok(false) => (Outcome::NoChange, Failure::None),
            Err(reason @ Failure::PersistenceFailed) => (Outcome::Failed, reason),
            Err(reason) => (Outcome::Refused, reason),
        };
        let reply = self.reply(local, &request, outcome, reason)?;
        if self.completed.len() == 64 {
            self.completed.pop_front();
        }
        self.completed.push_back((peer, request, reply.clone()));
        Ok(reply)
    }

    fn apply(&mut self, request: &dto::SetLocalOption) -> Result<bool, dto::ControlFailure> {
        use dto::ControlFailure as Failure;
        let revision = self.state.revision().map_err(|_| Failure::InvalidValue)?;
        if request.expected_revision() != Ok(revision) {
            return Err(Failure::StaleRevision);
        }
        let value = dto::ForwardingControlsValue::read(&request.value().map_err(|_| Failure::InvalidValue)?)
            .map_err(|_| Failure::InvalidValue)?;
        self.commit(revision, value)
    }

    fn commit(&mut self, revision: u64, value: dto::ForwardingControlsValue) -> Result<bool, dto::ControlFailure> {
        use dto::ControlFailure as Failure;
        if self.state.value() == Ok(value.write()) {
            return Ok(false);
        }
        if !self.writable {
            return Err(Failure::PersistenceFailed);
        }
        let next = dto::LocalOptionState::from_raw(
            [
                (1, self.option.write()),
                (2, value.write()),
                (3, revision.checked_add(1).ok_or(Failure::StateConflict)?.write()),
            ]
            .into(),
        )
        .map_err(|_| Failure::InvalidValue)?;
        storage::atomic_write(&self.path, &next.encode(1024).map_err(|_| Failure::InvalidValue)?)
            .map_err(|_| Failure::PersistenceFailed)?;
        self.state = next;
        Ok(true)
    }

    fn reply(
        &self,
        local: Boot,
        request: &dto::SetLocalOption,
        outcome: dto::ControlOutcome,
        reason: dto::ControlFailure,
    ) -> v2::Result<dto::LocalEditResult> {
        let current = (request.option()? == self.option).then(|| self.state.clone());
        dto::LocalEditResult::from_raw(
            [
                (1, request.request_id()?.write()),
                (2, boot(local).write()),
                (3, outcome.write()),
                (4, reason.write()),
                (5, current.write()),
                (7, (reason == dto::ControlFailure::None).write()),
            ]
            .into(),
        )
    }
}

/// Desktop CAS completes only after the owner's facts publication.
pub(crate) fn publish_result(result: &mut dto::LocalEditResult, fact_sequence: Option<u64>) -> v2::Result<()> {
    if matches!(
        result.outcome()?,
        dto::ControlOutcome::Applied | dto::ControlOutcome::NoChange
    ) {
        result.edit(|raw| {
            if let Some(sequence) = fact_sequence {
                raw.insert(6, sequence.write());
            } else {
                raw.insert(3, dto::ControlOutcome::Failed.write());
                raw.insert(4, dto::ControlFailure::ApplyFailed.write());
            }
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::DeviceId;
    use oink_protocol::v2::schema::FixedBytes;

    #[test]
    fn local_gain_edit_persists_and_advances_the_revision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("microphone-controls-v2.cbor");
        let mut controls = MicrophoneControls::open(path.clone());
        assert_eq!(controls.snapshot().revision, 1);
        assert!(controls.set_local(150, true).unwrap());
        assert!(!controls.set_local(150, true).unwrap());
        assert_eq!(controls.set_local(201, false), Err(dto::ControlFailure::InvalidValue));
        let reopened = MicrophoneControls::open(path).snapshot();
        assert_eq!(
            reopened,
            crate::MobileMicrophoneControls {
                volume_percent: 150,
                muted: true,
                revision: 2,
            }
        );
    }

    #[test]
    fn sender_gain_success_requires_published_fact_sequence() {
        let local = Boot {
            device_id: DeviceId([1; 32]),
            nonce: [3; 16],
        };
        let peer = Boot {
            device_id: DeviceId([2; 32]),
            nonce: [4; 16],
        };
        let mut controls = MicrophoneControls::open(PathBuf::new());
        let request = dto::SetLocalOption::from_raw(
            [
                (1, FixedBytes([5; 16]).write()),
                (2, boot(local).write()),
                (3, controls.option.write()),
                (4, controls.state.value().unwrap()),
                (5, 1u64.write()),
            ]
            .into(),
        )
        .unwrap();
        let result = controls.handle(local, peer, request, true).unwrap();
        let mut published = result.clone();
        publish_result(&mut published, Some(2)).unwrap();
        assert_eq!(published.outcome().unwrap(), dto::ControlOutcome::NoChange);
        assert_eq!(published.fact_sequence().unwrap(), Some(2));
        assert!(published.persisted().unwrap());

        let mut failed = result;
        publish_result(&mut failed, None).unwrap();
        assert_eq!(failed.outcome().unwrap(), dto::ControlOutcome::Failed);
        assert_eq!(failed.reason().unwrap(), dto::ControlFailure::ApplyFailed);
    }
    #[test]
    fn forwarding_gain_uses_the_same_revision_fence_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forward.cbor");
        let local = Boot {
            device_id: DeviceId([1; 32]),
            nonce: [1; 16],
        };
        let peer = Boot {
            device_id: DeviceId([2; 32]),
            nonce: [2; 16],
        };
        let mut controls = MicrophoneControls::open_option(path.clone(), dto::LocalOption::ForwardingControls);
        let request = dto::SetLocalOption::from_raw(
            [
                (1, FixedBytes([3; 16]).write()),
                (2, boot(local).write()),
                (3, dto::LocalOption::ForwardingControls.write()),
                (4, v2::RawValue::Map([(1, 200u16.write()), (2, true.write())].into())),
                (5, 1u64.write()),
            ]
            .into(),
        )
        .unwrap();
        let result = controls.handle(local, peer, request.clone(), true).unwrap();
        assert_eq!(result.outcome().unwrap(), dto::ControlOutcome::Applied);
        assert_eq!(controls.gain(), (200, true));
        assert_eq!(
            controls.handle(local, peer, request, true).unwrap(),
            result,
            "duplicate is idempotent"
        );
        let reopened = MicrophoneControls::open_option(path, dto::LocalOption::ForwardingControls);
        assert_eq!(reopened.gain(), (200, true));
        assert_eq!(
            reopened.preference().unwrap().option().unwrap(),
            dto::LocalOption::ForwardingControls
        );
        assert_eq!(reopened.snapshot().revision, 2);
    }
}
