//! SPAKE2 binds the UI roles, exact greeting frames, full tagged identities and
//! each submitted attempt. Confirmation keys never become a group secret.
use hmac::{Hmac, Mac};
use oink_protocol::v1::{
    self, RawValue, dto,
    frame::OpaqueFrame,
    schema::{WireDto, WireValue},
};
use rand::Rng;
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PakeError {
    #[error("invalid code syntax")]
    InvalidCodeSyntax,
    #[error("invalid pairing transcript")]
    Protocol,
    #[error("incorrect pairing code")]
    WrongCode,
}
impl From<v1::Error> for PakeError {
    fn from(_: v1::Error) -> Self {
        Self::Protocol
    }
}
pub fn valid_code(code: &str) -> bool {
    code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit())
}
pub fn generate_code() -> Zeroizing<String> {
    Zeroizing::new(format!("{:06}", rand::thread_rng().gen_range(0..1_000_000u32)))
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct Keys {
    receiver: [u8; 32],
    initiator: [u8; 32],
    transcript: [u8; 32],
}
#[derive(Clone)]
struct Transcript(Sha256);
fn lp(hash: &mut Sha256, bytes: &[u8]) -> Result<(), PakeError> {
    hash.update(
        u32::try_from(bytes.len())
            .map_err(|_| PakeError::Protocol)?
            .to_le_bytes(),
    );
    hash.update(bytes);
    Ok(())
}
impl Transcript {
    fn new(context: &dto::PairContext, attempt: u8, hello: &[u8], info: &[u8]) -> Result<Self, PakeError> {
        if !(1..=3).contains(&attempt) || context.pairing_suite()?.0 != dto::PAIRING_SUITE {
            return Err(PakeError::Protocol);
        }
        for (bytes, id) in [(hello, 0xd00), (info, 0xd01)] {
            if OpaqueFrame::parse_exact(bytes, v1::limits::CONFIG_FRAME)?
                .header
                .type_id
                != id
            {
                return Err(PakeError::Protocol);
            }
        }
        context.initiator()?.device_id()?.require_supported()?;
        context.responder()?.device_id()?.require_supported()?;
        let mut hash = Sha256::new();
        hash.update(v1::labels::PAIR_TRANSCRIPT);
        lp(&mut hash, &context.encode(v1::limits::CONFIG_FRAME)?)?;
        hash.update([attempt]);
        lp(&mut hash, hello)?;
        lp(&mut hash, info)?;
        Ok(Self(hash))
    }
    fn derive(mut self, key: &[u8], a: &[u8], b: &[u8]) -> Result<Keys, PakeError> {
        lp(&mut self.0, a)?;
        lp(&mut self.0, b)?;
        let transcript: [u8; 32] = self.0.finalize().into();
        let mut bytes = Zeroizing::new([0u8; 64]);
        hkdf::Hkdf::<Sha256>::new(Some(&transcript), key)
            .expand(v1::labels::PAIR_CONFIRMATION, bytes.as_mut())
            .map_err(|_| PakeError::Protocol)?;
        let mut keys = Keys {
            receiver: [0; 32],
            initiator: [0; 32],
            transcript,
        };
        keys.receiver.copy_from_slice(&bytes[..32]);
        keys.initiator.copy_from_slice(&bytes[32..]);
        Ok(keys)
    }
}
fn mac(key: &[u8; 32], role: &[u8], transcript: &[u8; 32]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC key");
    mac.update(role);
    mac.update(transcript);
    mac
}
fn identities(context: &dto::PairContext) -> Result<(Vec<u8>, Vec<u8>), PakeError> {
    Ok((
        context.responder()?.device_id()?.encode(4096)?,
        context.initiator()?.device_id()?.encode(4096)?,
    ))
}

pub struct ResponderPake {
    spake: Spake2<Ed25519Group>,
    message: Vec<u8>,
    transcript: Transcript,
}
impl ResponderPake {
    pub fn start(
        code: &str,
        context: dto::PairContext,
        attempt: u8,
        hello: &[u8],
        info: &[u8],
    ) -> Result<(Self, dto::PakeStart), PakeError> {
        if !valid_code(code) {
            return Err(PakeError::InvalidCodeSyntax);
        }
        let transcript = Transcript::new(&context, attempt, hello, info)?;
        let (a, b) = identities(&context)?;
        let (spake, message) = Spake2::start_a(&Password::new(code.as_bytes()), &Identity::new(&a), &Identity::new(&b));
        let step = dto::PakeStart::from_raw([(1, context.write()), (2, RawValue::Bytes(message.clone()))].into())?;
        Ok((
            Self {
                spake,
                message,
                transcript,
            },
            step,
        ))
    }
    pub fn finish(self, message: &[u8], confirmation: &[u8; 32]) -> Result<dto::PakeFinish, PakeError> {
        let key = Zeroizing::new(self.spake.finish(message).map_err(|_| PakeError::Protocol)?);
        let keys = self.transcript.derive(&key, &self.message, message)?;
        mac(&keys.initiator, b"initiator\0", &keys.transcript)
            .verify_slice(confirmation)
            .map_err(|_| PakeError::WrongCode)?;
        let confirmation = mac(&keys.receiver, b"responder\0", &keys.transcript)
            .finalize()
            .into_bytes();
        Ok(dto::PakeFinish::from_raw(
            [(1, RawValue::Bytes(confirmation.to_vec()))].into(),
        )?)
    }
}
pub struct InitiatorPake {
    keys: Keys,
}
impl InitiatorPake {
    pub fn respond(
        code: &str,
        context: &dto::PairContext,
        attempt: u8,
        hello: &[u8],
        info: &[u8],
        message: &[u8],
    ) -> Result<(Self, dto::PakeReply), PakeError> {
        if !valid_code(code) {
            return Err(PakeError::InvalidCodeSyntax);
        }
        let transcript = Transcript::new(context, attempt, hello, info)?;
        let (a, b) = identities(context)?;
        let (spake, reply) =
            Spake2::<Ed25519Group>::start_b(&Password::new(code.as_bytes()), &Identity::new(&a), &Identity::new(&b));
        let key = Zeroizing::new(spake.finish(message).map_err(|_| PakeError::Protocol)?);
        let keys = transcript.derive(&key, message, &reply)?;
        let confirmation = mac(&keys.initiator, b"initiator\0", &keys.transcript)
            .finalize()
            .into_bytes();
        Ok((
            Self { keys },
            dto::PakeReply::from_raw(
                [(1, RawValue::Bytes(reply)), (2, RawValue::Bytes(confirmation.to_vec()))].into(),
            )?,
        ))
    }
    pub fn finish(self, confirmation: &[u8; 32]) -> Result<(), PakeError> {
        mac(&self.keys.receiver, b"responder\0", &self.keys.transcript)
            .verify_slice(confirmation)
            .map_err(|_| PakeError::WrongCode)
    }
}
impl std::fmt::Debug for ResponderPake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResponderPake(..)")
    }
}
impl std::fmt::Debug for InitiatorPake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InitiatorPake(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire_identity as id;
    use oink_foundation::{
        DeviceId,
        session::{Boot, PairId},
    };
    fn fixture() -> (dto::PairContext, Vec<u8>, Vec<u8>) {
        let i = Boot {
            device_id: DeviceId([1; 32]),
            nonce: [1; 16],
        };
        let r = Boot {
            device_id: DeviceId([2; 32]),
            nonce: [2; 16],
        };
        let context = dto::PairContext::from_raw(
            [
                (
                    1,
                    id::pair(PairId {
                        initiator_boot: i,
                        random: [3; 16],
                    })
                    .write(),
                ),
                (2, id::boot(i).write()),
                (3, id::boot(r).write()),
                (4, RawValue::Bytes(vec![1; 16])),
                (5, RawValue::Bytes(vec![2; 16])),
                (6, 1u8.write()),
                (7, 1u8.write()),
                (8, RawValue::Bytes(vec![2; 16])),
                (9, RawValue::Bytes(vec![3; 32])),
                (10, RawValue::from(dto::PAIRING_SUITE)),
                (11, RawValue::Bytes(vec![4; 32])),
                (12, RawValue::Bytes(vec![5; 32])),
            ]
            .into(),
        )
        .unwrap();
        // Exact captured-frame boundaries are what the PAKE transcript consumes.
        let hello = vec![0, 13, 1, 0, 0, 0, 0xa0];
        let info = vec![1, 13, 1, 0, 0, 0, 0xa0];
        (context, hello, info)
    }
    #[test]
    fn confirmations_bind_roles_code_and_attempt() {
        let (context, hello, info) = fixture();
        for attempt in 1..=3 {
            let (r, start) = ResponderPake::start("000012", context.clone(), attempt, &hello, &info).unwrap();
            let (i, reply) =
                InitiatorPake::respond("000012", &context, attempt, &hello, &info, &start.message().unwrap().0)
                    .unwrap();
            let finish = r
                .finish(&reply.message().unwrap().0, &reply.confirmation().unwrap().0)
                .unwrap();
            i.finish(&finish.confirmation().unwrap().0).unwrap();
        }
    }
    #[test]
    fn wrong_code_or_transcript_does_not_confirm() {
        let (context, hello, info) = fixture();
        for (code, attempt) in [("123456", 1), ("000012", 2)] {
            let (r, start) = ResponderPake::start("000012", context.clone(), 1, &hello, &info).unwrap();
            let (_, reply) =
                InitiatorPake::respond(code, &context, attempt, &hello, &info, &start.message().unwrap().0).unwrap();
            assert_eq!(
                r.finish(&reply.message().unwrap().0, &reply.confirmation().unwrap().0),
                Err(PakeError::WrongCode)
            );
        }
    }
    #[test]
    fn syntax_is_exact_and_generators_preserve_leading_zeroes() {
        for code in ["12345", "1234567", "１２３４５６", "123 456", "123-456", "a123456"] {
            assert!(!valid_code(code));
        }
        assert!(valid_code("000001"));
        for _ in 0..20 {
            assert!(valid_code(&generate_code()));
        }
    }
}
