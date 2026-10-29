//! Stable public-key identities and their canonical text and UI fingerprints.

use serde::{Deserialize, Serialize};
use std::fmt;

/// 设备 ID：设备的 Ed25519 公钥。与 IP 地址无关，地址变化不影响身份。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeviceId(pub [u8; 32]);

/// 设备组 ID：组创建时随机生成。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GroupId(pub [u8; 16]);

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

impl DeviceId {
    /// Human-readable fingerprint: the first 60 public-key bits in Crockford Base32.
    /// This is only for visual comparison; authorization compares the full key.
    pub fn short(&self) -> String {
        let bits = u64::from_be_bytes(self.0[..8].try_into().expect("eight bytes")) >> 4;
        let mut text = String::with_capacity(14);
        for i in 0..12 {
            if i != 0 && i % 4 == 0 {
                text.push('-');
            }
            let index = ((bits >> (55 - 5 * i)) & 0x1f) as usize;
            text.push(CROCKFORD[index] as char);
        }
        text
    }

    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }
}

impl GroupId {
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut text = vec![0; bytes.len() * 2];
    encode_hex(bytes, &mut text);
    String::from_utf8(text).expect("hex is ASCII")
}

fn encode_hex(bytes: &[u8], text: &mut [u8]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let (pairs, _) = text.as_chunks_mut::<2>();
    for (byte, pair) in bytes.iter().zip(pairs) {
        *pair = [DIGITS[(byte >> 4) as usize], DIGITS[(byte & 15) as usize]];
    }
}

fn hex_text<'a>(bytes: &[u8], text: &'a mut [u8]) -> &'a str {
    encode_hex(bytes, text);
    std::str::from_utf8(text).expect("hex is ASCII")
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({})", hex_text(&self.0, &mut [0; 64]))
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(hex_text(&self.0, &mut [0; 64]))
    }
}

impl fmt::Debug for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GroupId({})", hex_text(&self.0[..4], &mut [0; 8]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_preserves_all_bytes_and_debug_formats() {
        let bytes: Vec<_> = (0..=255).collect();
        let expected: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(hex(&bytes), expected);
        assert_eq!(hex(&[]), "");
        let group = GroupId([0xab; 16]);
        assert_eq!(group.to_hex(), "ab".repeat(16));
        assert_eq!(format!("{group:?}"), "GroupId(abababab)");
    }

    #[test]
    #[ignore = "isolated resource benchmark; run with --ignored --nocapture"]
    fn resource_identity_format_benchmark() {
        let id = std::hint::black_box(DeviceId([0xab; 32]));
        let started = std::time::Instant::now();
        for _ in 0..100_000 {
            std::hint::black_box(id.to_hex());
            std::hint::black_box(format!("{id:?}"));
        }
        println!("identity_pairs=100000 elapsed_us={}", started.elapsed().as_micros());
    }

    #[test]
    fn fingerprint_uses_exactly_the_first_sixty_bits() {
        let mut bytes = [0; 32];
        bytes[..8].copy_from_slice(&0x0044_3214_c742_54bfu64.to_be_bytes());
        let id = DeviceId(bytes);
        assert_eq!(id.short(), "0123-4567-89AB");
        bytes[7] ^= 0x0f;
        bytes[31] = 255;
        assert_eq!(DeviceId(bytes).short(), id.short());
        bytes[7] ^= 0x10;
        assert_ne!(DeviceId(bytes).short(), id.short());
        assert_eq!(id.to_string(), id.to_hex());
        assert_eq!(id.to_string().len(), 64);
        assert_eq!(format!("{id:?}"), format!("DeviceId({})", id.to_hex()));
    }

    #[test]
    fn short_fingerprint_format() {
        let id = DeviceId([0xff; 32]);
        assert_eq!(id.short(), "ZZZZ-ZZZZ-ZZZZ");
        let id = DeviceId([0; 32]);
        assert_eq!(id.short(), "0000-0000-0000");
    }
}
