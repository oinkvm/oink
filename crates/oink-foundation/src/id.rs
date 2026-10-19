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
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({})", self.to_hex())
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GroupId({})", &self.to_hex()[..8])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
