//! Stateless opaque tokens bind a command to its owner's version, not the card
//! revision. Authentication prevents a UI from changing the absolute operation;
//! every invocation still checks live membership, Boot, capability and revision.
use super::{CardAction, CardCommand, DeviceControlFailure, GamePreference, SharingMode};
use crate::api::{DeviceKey, GroupKey};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerVersion {
    pub group: GroupKey,
    pub device: DeviceKey,
    pub boot_nonce: [u8; 16],
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "option", content = "value", rename_all = "snake_case", deny_unknown_fields)]
pub enum CardOptionEdit {
    MouseMode(SharingMode),
    KeyboardMode(SharingMode),
    ClipboardEnabled(bool),
    ForwardingEnabled(bool),
    KeyboardDisabled(bool),
    ScreenShared(bool),
    GameModePreference(GamePreference),
    AudioExitClaim(bool),
}
impl CardOptionEdit {
    pub fn number(&self) -> u32 {
        match self {
            Self::MouseMode(_) => 1,
            Self::KeyboardMode(_) => 2,
            Self::ClipboardEnabled(_) => 3,
            Self::ForwardingEnabled(_) => 4,
            Self::KeyboardDisabled(_) => 5,
            Self::ScreenShared(_) => 6,
            Self::GameModePreference(_) => 18,
            Self::AudioExitClaim(_) => 20,
        }
    }
    pub fn command(&self) -> CardCommand {
        match self {
            Self::ScreenShared(_) => CardCommand::SetScreenMode,
            Self::GameModePreference(_) => CardCommand::SetGameMode,
            Self::AudioExitClaim(true) => CardCommand::MakeAudioExit,
            Self::AudioExitClaim(false) => CardCommand::ClearAudioExit,
            _ => CardCommand::SetLocalOption,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TokenOperation {
    /// Internal typed settings operation; no card token is issued for this binding.
    OwnerOption {
        option: u32,
    },
    Option {
        edit: CardOptionEdit,
    },
    Pause {
        paused: bool,
    },
    /// A source slider supplies absolute gain/mute; this is an option15 fence,
    /// not an icon's fixed desired operation.
    ForwardingControls,
    /// The selected exit, output generation and claim form one indivisible fence.
    ExitVolume {
        output_id: String,
        output_generation: u64,
        claim_counter: u64,
        claim_owner: DeviceKey,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenBinding {
    pub owner: OwnerVersion,
    pub operation: TokenOperation,
}

/// One authority lives for one projector incarnation. Reconstructing the
/// projector retires every previous token, including a delayed IPC invocation.
pub struct ActionTokens {
    key: [u8; 32],
    stream: String,
}
impl std::fmt::Debug for ActionTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionTokens")
            .field("stream", &self.stream)
            .finish_non_exhaustive()
    }
}
impl ActionTokens {
    pub fn new() -> Result<Self, getrandom::Error> {
        let mut key = [0; 32];
        let mut stream = [0; 16];
        getrandom::fill(&mut key)?;
        getrandom::fill(&mut stream)?;
        Ok(Self {
            key,
            stream: URL_SAFE_NO_PAD.encode(stream),
        })
    }
    pub fn stream(&self) -> &str {
        &self.stream
    }

    pub fn issue(&self, binding: &TokenBinding) -> String {
        // The closed typed payload has no floats or fallible map keys.
        let body = serde_json::to_vec(binding).expect("typed action token serializes");
        let signature = blake3::keyed_hash(&self.key, &body);
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(body),
            URL_SAFE_NO_PAD.encode(signature.as_bytes())
        )
    }

    pub fn action(&self, owner: OwnerVersion, edit: CardOptionEdit) -> CardAction {
        let command = edit.command();
        let token = self.issue(&TokenBinding {
            owner,
            operation: TokenOperation::Option { edit },
        });
        CardAction::Invoke { command, token }
    }

    pub fn pause(&self, owner: OwnerVersion, paused: bool) -> CardAction {
        CardAction::Invoke {
            command: CardCommand::SetDevicePaused,
            token: self.issue(&TokenBinding {
                owner,
                operation: TokenOperation::Pause { paused },
            }),
        }
    }

    pub fn read(&self, token: &str) -> Result<TokenBinding, DeviceControlFailure> {
        if token.len() > 8192 {
            return Err(DeviceControlFailure::InvalidValue);
        }
        let (body, signature) = token.split_once('.').ok_or(DeviceControlFailure::InvalidValue)?;
        let body = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| DeviceControlFailure::InvalidValue)?;
        let signature: [u8; 32] = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| DeviceControlFailure::InvalidValue)?
            .try_into()
            .map_err(|_| DeviceControlFailure::InvalidValue)?;
        // blake3::Hash equality compares the authentication tag in constant time.
        if blake3::keyed_hash(&self.key, &body) != blake3::Hash::from_bytes(signature) {
            return Err(DeviceControlFailure::StateConflict);
        }
        serde_json::from_slice(&body).map_err(|_| DeviceControlFailure::InvalidValue)
    }
}

/// UUIDs have a fixed textual form at the local boundary and map to wire bytes16.
pub fn request_id(value: &str) -> Result<[u8; 16], DeviceControlFailure> {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return Err(DeviceControlFailure::InvalidValue);
    }
    let mut output = [0; 16];
    let mut nibbles = 0;
    for (offset, &byte) in bytes.iter().enumerate() {
        if matches!(offset, 8 | 13 | 18 | 23) {
            if byte != b'-' {
                return Err(DeviceControlFailure::InvalidValue);
            }
            continue;
        }
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return Err(DeviceControlFailure::InvalidValue),
        };
        output[nibbles / 2] = output[nibbles / 2] * 16 + digit;
        nibbles += 1;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_foundation::{DeviceId, GroupId};
    fn owner() -> OwnerVersion {
        OwnerVersion {
            group: GroupId([1; 16]).into(),
            device: DeviceId([2; 32]).into(),
            boot_nonce: [3; 16],
            revision: 8,
        }
    }
    #[test]
    fn tokens_bind_boot_item_and_absolute_value_without_a_card_revision() {
        let tokens = ActionTokens::new().unwrap();
        let binding = TokenBinding {
            owner: owner(),
            operation: TokenOperation::Option {
                edit: CardOptionEdit::KeyboardDisabled(false),
            },
        };
        let token = tokens.issue(&binding);
        assert_eq!(tokens.issue(&binding), token);
        assert_eq!(tokens.read(&token).unwrap(), binding);
        for changed in [
            TokenBinding {
                owner: OwnerVersion { revision: 9, ..owner() },
                ..binding.clone()
            },
            TokenBinding {
                owner: OwnerVersion {
                    boot_nonce: [4; 16],
                    ..owner()
                },
                ..binding.clone()
            },
            TokenBinding {
                operation: TokenOperation::Option {
                    edit: CardOptionEdit::KeyboardDisabled(true),
                },
                ..binding.clone()
            },
        ] {
            assert_ne!(tokens.issue(&changed), token);
        }
        assert_eq!(
            ActionTokens::new().unwrap().read(&token),
            Err(DeviceControlFailure::StateConflict)
        );
    }
    #[test]
    fn changing_payload_without_signature_is_refused() {
        let tokens = ActionTokens::new().unwrap();
        let binding = TokenBinding {
            owner: owner(),
            operation: TokenOperation::Pause { paused: true },
        };
        let token = tokens.issue(&binding);
        let (_, signature) = token.split_once('.').unwrap();
        let changed = TokenBinding {
            operation: TokenOperation::Pause { paused: false },
            ..binding
        };
        let altered = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&changed).unwrap()),
            signature
        );
        assert_eq!(tokens.read(&altered), Err(DeviceControlFailure::StateConflict));
    }
    #[test]
    fn request_uuid_is_exact_bytes_without_number_precision_loss() {
        assert_eq!(
            request_id("00112233-4455-6677-8899-aabbccddeeff").unwrap(),
            [0, 17, 34, 51, 68, 85, 102, 119, 136, 153, 170, 187, 204, 221, 238, 255]
        );
        for invalid in [
            "",
            "00112233445566778899aabbccddeeff",
            "00112233+4455-6677-8899-aabbccddeeff",
            "00112233-4455-6677-8899-aabbccddeezz",
        ] {
            assert_eq!(request_id(invalid), Err(DeviceControlFailure::InvalidValue));
        }
    }
}
