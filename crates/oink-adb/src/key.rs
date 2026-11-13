use crate::{Error, Result};
use base64::Engine;
use num_bigint_dig::BigUint;
use pkcs1::RsaPublicKey;
use pkcs8::{
    SecretDocument, SubjectPublicKeyInfoRef,
    der::{Decode, Encode, asn1::BitStringRef},
};
use sha2::{Digest, Sha256};
use std::sync::Arc;

struct Key {
    der: SecretDocument,
    modulus: BigUint,
    fingerprint: String,
}

#[derive(Clone)]
pub struct AdbKey(Arc<Key>);
impl std::fmt::Debug for AdbKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AdbKey([redacted])")
    }
}
impl AdbKey {
    pub fn from_pkcs8_pem(pem: &[u8]) -> Result<Self> {
        let (label, der) =
            SecretDocument::from_pem(std::str::from_utf8(pem).map_err(|_| Error::Key)?).map_err(|_| Error::Key)?;
        if label != "PRIVATE KEY" {
            return Err(Error::Key);
        }
        Self::from_der(der)
    }

    fn from_der(der: SecretDocument) -> Result<Self> {
        // TLS and certificate signatures already use ring. Validate the same
        // PKCS#8 key here and use big integers only for the public ADB format.
        let pair = ring::rsa::KeyPair::from_pkcs8(der.as_bytes()).map_err(|_| Error::Key)?;
        let public = pair.public().as_ref();
        let rsa = RsaPublicKey::from_der(public).map_err(|_| Error::Key)?;
        let modulus = BigUint::from_bytes_be(rsa.modulus.as_bytes());
        if modulus.bits() != 2048 || rsa.public_exponent.as_bytes() != [1, 0, 1] {
            return Err(Error::Key);
        }
        let spki = SubjectPublicKeyInfoRef {
            algorithm: pkcs1::ALGORITHM_ID,
            subject_public_key: BitStringRef::from_bytes(public).map_err(|_| Error::Key)?,
        }
        .to_der()
        .map_err(|_| Error::Key)?;
        let fingerprint = hex::encode(Sha256::digest(spki));
        Ok(Self(Arc::new(Key {
            der,
            modulus,
            fingerprint,
        })))
    }

    pub(crate) fn der(&self) -> &[u8] {
        self.0.der.as_bytes()
    }

    pub(crate) fn fingerprint(&self) -> &str {
        &self.0.fingerprint
    }

    #[cfg(test)]
    pub(crate) fn synthetic() -> Self {
        // Fixed, disposable RSA material for software-only loopback tests.
        let der = include_bytes!("../tests/synthetic-rsa-key.pkcs8");
        Self::from_der(SecretDocument::try_from(der.as_slice()).unwrap()).unwrap()
    }

    pub fn public_key(&self) -> String {
        // AOSP android_pubkey_encode uses little-endian RSA Montgomery data:
        // 64 words, -n^-1 mod 2^32, n, R^2 mod n, e (R=2^2048).
        let modulus = self.0.modulus.to_bytes_le();
        let word = u32::from_le_bytes(modulus[..4].try_into().expect("2048-bit modulus"));
        let mut inverse = word;
        for _ in 0..5 {
            inverse = inverse.wrapping_mul(2_u32.wrapping_sub(word.wrapping_mul(inverse)));
        }
        let rr = (BigUint::from(1_u8) << 4096_usize) % &self.0.modulus;
        let mut encoded = Vec::with_capacity(524);
        encoded.extend_from_slice(&64_u32.to_le_bytes());
        encoded.extend_from_slice(&inverse.wrapping_neg().to_le_bytes());
        encoded.extend_from_slice(&modulus);
        let mut rr = rr.to_bytes_le();
        rr.resize(256, 0);
        encoded.extend_from_slice(&rr);
        encoded.extend_from_slice(&65537_u32.to_le_bytes());
        format!(
            "{} oink@group\n",
            base64::engine::general_purpose::STANDARD.encode(encoded)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rsa_pem_and_android_montgomery_key_roundtrip() {
        let key = AdbKey::synthetic();
        let pem = key.0.der.to_pem("PRIVATE KEY", pkcs8::LineEnding::LF).unwrap();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        let imported = AdbKey::from_pkcs8_pem(pem.as_bytes()).unwrap();
        assert_eq!(key.public_key(), imported.public_key());
        assert_eq!(
            imported.fingerprint(),
            "e93e0f032e87e649019fda42928c83ea0cf47d9594595bf91281b95955782ef3"
        );
        let line = key.public_key();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(line.split_whitespace().next().unwrap())
            .unwrap();
        assert_eq!(bytes.len(), 524);
        assert_eq!(u32::from_le_bytes(bytes[..4].try_into().unwrap()), 64);
        let n0inv = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let n0 = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        assert_eq!(n0.wrapping_mul(n0inv), u32::MAX);
        assert_eq!(BigUint::from_bytes_le(&bytes[8..264]), key.0.modulus);
        assert_eq!(
            BigUint::from_bytes_le(&bytes[264..520]),
            (BigUint::from(1_u8) << 4096_usize) % &key.0.modulus
        );
        assert_eq!(u32::from_le_bytes(bytes[520..524].try_into().unwrap()), 65537);
        assert!(!format!("{key:?}").contains("BEGIN"));
    }
    #[test]
    fn malformed_private_key_is_rejected() {
        assert!(AdbKey::from_pkcs8_pem(b"not a private key").is_err());
    }
}
