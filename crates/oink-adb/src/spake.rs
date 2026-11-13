//! BoringSSL's pre-RFC SPAKE2 profile used by Android wireless pairing.
//! Protocol constants/transcript follow crypto/curve25519/spake25519.c at
//! 0f4454c07545e6f4465ad88e83fcbaa95a08d28f (Google, ISC). AOSP names include NUL.
use crate::{Error, Result};
use curve25519_dalek::{
    constants::ED25519_BASEPOINT_POINT,
    edwards::{CompressedEdwardsY, EdwardsPoint},
    scalar::Scalar,
    traits::IsIdentity,
};
use rand::RngCore;
use sha2::{Digest, Sha512};
use zeroize::Zeroizing;

const M: &str = "5ada7e4bf6ddd9adb6626d32131c6b5c51a1e347a3478f53cfcf441b88eed12e";
const N: &str = "10e3df0ae37d8e7a99b5fe74b44672103dbddcbd06af680d71329a11693bc778";
const CLIENT: &[u8] = b"adb pair client\0";
const SERVER: &[u8] = b"adb pair server\0";

pub(crate) struct Spake {
    client: bool,
    private: Zeroizing<Scalar>,
    password: Zeroizing<Scalar>,
    hash: Zeroizing<[u8; 64]>,
    message: [u8; 32],
}
impl Spake {
    pub fn new(password: &[u8]) -> Self {
        let mut random = Zeroizing::new([0; 64]);
        rand::rngs::OsRng.fill_bytes(&mut *random);
        Self::with_entropy(password, true, &random)
    }
    fn with_entropy(password: &[u8], client: bool, random: &[u8; 64]) -> Self {
        let private = Zeroizing::new(Scalar::from_bytes_mod_order_wide(random));
        let hash = Zeroizing::new(Sha512::digest(password).into());
        let password = Zeroizing::new(Scalar::from_bytes_mod_order_wide(&hash));
        let message = (ED25519_BASEPOINT_POINT * (*private * Scalar::from(8_u8))
            + mask(if client { M } else { N }, &password))
        .compress()
        .to_bytes();
        Self {
            client,
            private,
            password,
            hash,
            message,
        }
    }
    pub fn message(&self) -> [u8; 32] {
        self.message
    }
    pub fn finish(self, peer: [u8; 32]) -> Result<Zeroizing<[u8; 64]>> {
        self.finish_named(peer, CLIENT, SERVER)
    }
    fn finish_named(self, peer: [u8; 32], client_name: &[u8], server_name: &[u8]) -> Result<Zeroizing<[u8; 64]>> {
        let point = CompressedEdwardsY(peer).decompress().ok_or(Error::Authentication)?;
        let unmasked = point - mask(if self.client { N } else { M }, &self.password);
        let shared = unmasked.mul_by_cofactor() * *self.private;
        if shared.is_identity() {
            return Err(Error::Authentication);
        }
        let shared = shared.compress().to_bytes();
        let (client, server) = if self.client {
            (self.message, peer)
        } else {
            (peer, self.message)
        };
        let mut hash = Sha512::new();
        for value in [client_name, server_name, &client, &server, &shared, &*self.hash] {
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value);
        }
        Ok(Zeroizing::new(hash.finalize().into()))
    }
}
fn point(encoded: &str) -> EdwardsPoint {
    CompressedEdwardsY(
        hex::decode(encoded)
            .expect("published constant")
            .try_into()
            .expect("32 bytes"),
    )
    .decompress()
    .expect("published point")
}
fn mask(encoded: &str, password: &Scalar) -> EdwardsPoint {
    // BoringSSL adds a multiple of group order until the password scalar is
    // divisible by eight. Scalar reduction would lose that torsion adjustment.
    // Project the public mask point with [8][8^-1] instead, which is equivalent
    // and keeps dalek's constant-time scalar multiplication over its subgroup.
    point(encoded).mul_by_cofactor() * (password * Scalar::from(8_u8).invert())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn published_boringssl_mask_points_and_generation_seeds() {
        use sha2::Sha256;
        for (seed, expected) in [
            (b"edwards25519 point generation seed (M)".as_slice(), M),
            (b"edwards25519 point generation seed (N)".as_slice(), N),
        ] {
            let mut bytes: [u8; 32] = Sha256::digest(seed).into();
            while CompressedEdwardsY(bytes).decompress().is_none() {
                bytes = Sha256::digest(bytes).into();
            }
            assert_eq!(hex::encode(bytes), expected);
        }
    }
    #[test]
    fn peers_agree_and_password_mismatch_fails_key_confirmation() {
        let client = Spake::with_entropy(b"123456exporter", true, &[1; 64]);
        let server = Spake::with_entropy(b"123456exporter", false, &[2; 64]);
        let a = client.message();
        let b = server.message();
        assert_eq!(*client.finish(b).unwrap(), *server.finish(a).unwrap());
        let client = Spake::with_entropy(b"123456exporter", true, &[1; 64]);
        let server = Spake::with_entropy(b"654321exporter", false, &[2; 64]);
        let a = client.message();
        let b = server.message();
        assert_ne!(*client.finish(b).unwrap(), *server.finish(a).unwrap());
    }
    #[test]
    fn independent_fixed_entropy_vector() {
        // Public libadb-android fork b6c301dab4b83ae5892a7e68fdb96b6554e046a8
        // uses space-terminated test identities. Production AOSP uses NUL.
        let password=hex::decode("353932373831e63dd959651c211600f3b6561d0b9d90af09d0a4a453ee2059a480cc7c5a94d4d48933f9fff5fe43317d52fa7bff8f8bc4f3488b8007330fec7c7edc91c20e5d").unwrap();
        let alice=hex::decode("47f6c458e5f062db8427d2d9bb20c954a76d6943959756a18d11d45e1ad190f980a86d185a93ca1d3025c5febe3aac4045b34a39b1f511385ca97fc4332137f3").unwrap().try_into().unwrap();
        let bob=hex::decode("a6bf9f9bf7819e0ded8c2dd82a1aa38acb2f8a6403429cff33d64ea9c40439d5fd7029811a5f5a8f7c89c8b44ac0b421f6b24ca2ba18d2069995831730cd8c5a").unwrap().try_into().unwrap();
        let alice = Spake::with_entropy(&password, true, &alice);
        let bob = Spake::with_entropy(&password, false, &bob);
        assert_eq!(
            hex::encode(alice.message()),
            "135d85fa69022bbc7445653e19047e5b6981aa5b9d309b0de6d2e704dfe1568c"
        );
        assert_eq!(
            hex::encode(bob.message()),
            "d5bd4ead287e42f0a073adcb8dc46acc0630c4925fe1d43350e7441a8b29e03a"
        );
        let result = alice
            .finish_named(bob.message(), b"adb pair client ", b"adb pair server ")
            .unwrap();
        assert_eq!(
            hex::encode(*result),
            "c6aa9dd57cb181e3e855ec36d2dd8d5a8c7b2d60fc7ebca9469188f8e67f718139c13dc9e66bd13a092f65df97059d3182e39c914ab797d2b8b215bb682583b1"
        );
    }
    #[test]
    fn tls_exporter_is_part_of_password() {
        let client = Spake::with_entropy(b"123456tls-one", true, &[1; 64]);
        let server = Spake::with_entropy(b"123456tls-two", false, &[2; 64]);
        let a = client.message();
        let b = server.message();
        assert_ne!(*client.finish(b).unwrap(), *server.finish(a).unwrap());
    }
}
