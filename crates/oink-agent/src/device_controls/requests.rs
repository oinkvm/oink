//! Explicit conversion from locally authenticated action tokens into v1 DTOs.
use crate::device_cards::{
    self, CardOptionEdit, DeviceControlFailure, GamePreference, SharingMode, TokenBinding, TokenOperation,
};
use oink_protocol::v1::{
    Message, RawMap, RawValue, dto,
    schema::{Ascii, Bytes, FixedBytes, Text, WireDto, WireValue},
};

pub fn wire_device(device: crate::api::DeviceKey) -> dto::DeviceId {
    dto::DeviceId::from_raw(
        [
            (1, Ascii::<96>("ed25519".into()).write()),
            (2, Bytes::<4096>(device.0.0.to_vec()).write()),
        ]
        .into_iter()
        .collect(),
    )
    .expect("local identity is ed25519")
}
pub fn wire_boot(owner: device_cards::OwnerVersion) -> dto::Boot {
    dto::Boot::from_raw(
        [
            (1, wire_device(owner.device).write()),
            (2, FixedBytes(owner.boot_nonce).write()),
        ]
        .into_iter()
        .collect(),
    )
    .expect("local action Boot is valid")
}
fn request_id(value: &str) -> Result<RawValue, DeviceControlFailure> {
    Ok(FixedBytes(device_cards::request_id(value)?).write())
}
fn option_value(edit: &CardOptionEdit) -> RawValue {
    match edit {
        CardOptionEdit::MouseMode(mode) | CardOptionEdit::KeyboardMode(mode) => match mode {
            SharingMode::Shared => 0u32,
            SharingMode::Local => 1u32,
            SharingMode::Disabled => 2u32,
        }
        .write(),
        CardOptionEdit::ClipboardEnabled(v)
        | CardOptionEdit::ForwardingEnabled(v)
        | CardOptionEdit::KeyboardDisabled(v)
        | CardOptionEdit::ScreenShared(v)
        | CardOptionEdit::AudioExitClaim(v) => v.write(),
        CardOptionEdit::GameModePreference(v) => match v {
            GamePreference::Auto => 0u32,
            GamePreference::On => 1u32,
            GamePreference::Off => 2u32,
        }
        .write(),
    }
}
pub(crate) fn option_request(
    binding: &TokenBinding,
    id: &str,
    option: u32,
    value: RawValue,
) -> Result<Message, DeviceControlFailure> {
    let raw: RawMap = [
        (1, request_id(id)?),
        (2, wire_boot(binding.owner).write()),
        (3, option.write()),
        (4, value),
        (5, binding.owner.revision.write()),
    ]
    .into_iter()
    .collect();
    dto::SetLocalOption::from_raw(raw)
        .map(Message::ConfigSetLocalOption)
        .map_err(|_| DeviceControlFailure::InvalidValue)
}
pub fn action_request(binding: &TokenBinding, id: &str) -> Result<Message, DeviceControlFailure> {
    match &binding.operation {
        TokenOperation::Option { edit } => option_request(binding, id, edit.number(), option_value(edit)),
        TokenOperation::Pause { paused } => dto::PauseDevice::from_raw(
            [
                (1, request_id(id)?),
                (2, wire_boot(binding.owner).write()),
                (3, paused.write()),
                (4, binding.owner.revision.write()),
            ]
            .into_iter()
            .collect(),
        )
        .map(Message::ConfigPauseDevice)
        .map_err(|_| DeviceControlFailure::InvalidValue),
        _ => Err(DeviceControlFailure::InvalidValue),
    }
}
pub fn forwarding_request(
    binding: &TokenBinding,
    id: &str,
    percent: u16,
    muted: bool,
) -> Result<Message, DeviceControlFailure> {
    if binding.operation != TokenOperation::ForwardingControls || percent > 200 {
        return Err(DeviceControlFailure::InvalidValue);
    }
    option_request(
        binding,
        id,
        15,
        RawValue::Map([(1, percent.write()), (2, muted.write())].into_iter().collect()),
    )
}
pub fn volume_request(
    binding: &TokenBinding,
    id: &str,
    percent: u8,
    muted: bool,
) -> Result<Message, DeviceControlFailure> {
    let TokenOperation::ExitVolume {
        output_id,
        output_generation,
        claim_counter,
        claim_owner,
    } = &binding.operation
    else {
        return Err(DeviceControlFailure::InvalidValue);
    };
    if percent > 100 {
        return Err(DeviceControlFailure::InvalidValue);
    }
    let claim = dto::ExitClaimStamp::from_raw(
        [(1, claim_counter.write()), (2, wire_device(*claim_owner).write())]
            .into_iter()
            .collect(),
    )
    .map_err(|_| DeviceControlFailure::InvalidValue)?;
    let version = dto::ExitVolumeVersion::from_raw(
        [
            (1, wire_boot(binding.owner).write()),
            (3, Text::<512>(output_id.clone()).write()),
            (4, output_generation.write()),
            (5, binding.owner.revision.write()),
            (6, claim.write()),
        ]
        .into_iter()
        .collect(),
    )
    .map_err(|_| DeviceControlFailure::InvalidValue)?;
    dto::SetExitVolume::from_raw(
        [
            (1, request_id(id)?),
            (2, version.write()),
            (3, percent.write()),
            (4, muted.write()),
        ]
        .into_iter()
        .collect(),
    )
    .map(Message::ConfigSetExitVolume)
    .map_err(|_| DeviceControlFailure::InvalidValue)
}
/// Includes the message id so two command families cannot collide in dedupe.
pub fn request_body(message: &Message) -> Result<Vec<u8>, DeviceControlFailure> {
    let mut bytes = message.type_id().to_be_bytes().to_vec();
    bytes.extend(
        RawValue::Map(message.raw().clone())
            .encode(super::transactions::MAX_FRAME)
            .map_err(|_| DeviceControlFailure::InvalidValue)?,
    );
    if bytes.len() + 4 > super::transactions::MAX_FRAME {
        return Err(DeviceControlFailure::InvalidValue);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "00112233-4455-6677-8899-aabbccddeeff";
    fn binding(operation: TokenOperation) -> TokenBinding {
        TokenBinding {
            owner: device_cards::OwnerVersion {
                group: oink_foundation::GroupId([1; 16]).into(),
                device: oink_foundation::DeviceId([2; 32]).into(),
                boot_nonce: [3; 16],
                revision: 9,
            },
            operation,
        }
    }
    #[test]
    fn typed_actions_keep_owner_revision_and_absolute_operation() {
        let binding = binding(TokenOperation::Option {
            edit: CardOptionEdit::KeyboardDisabled(false),
        });
        let Message::ConfigSetLocalOption(request) = action_request(&binding, ID).unwrap() else {
            panic!()
        };
        assert_eq!(request.expected_revision().unwrap(), 9);
        assert_eq!(request.option().unwrap(), dto::LocalOption::KeyboardDisabled);
        assert_eq!(request.value().unwrap(), RawValue::Bool(false));
        assert_eq!(request.target().unwrap().nonce().unwrap().0, [3; 16]);
        assert!(forwarding_request(&binding, ID, 100, false).is_err());
    }
    #[test]
    fn sliders_accept_only_their_exact_fence_and_range() {
        let binding = binding(TokenOperation::ForwardingControls);
        assert!(forwarding_request(&binding, ID, 201, false).is_err());
        let message = forwarding_request(&binding, ID, 200, true).unwrap();
        let Message::ConfigSetLocalOption(request) = &message else {
            panic!()
        };
        assert_eq!(request.option().unwrap(), dto::LocalOption::ForwardingControls);
        assert_eq!(
            request.value().unwrap(),
            RawValue::Map([(1, 200u16.write()), (2, true.write())].into_iter().collect())
        );
        assert!(action_request(&binding, ID).is_err());
        assert!(request_body(&message).unwrap().len() < 4096);
    }
}
