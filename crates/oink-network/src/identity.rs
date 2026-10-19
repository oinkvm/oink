//! Ed25519 device identity. The private 32-byte seed is stored owner-only.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use oink_foundation::DeviceId;
use std::path::Path;

/// 本机设备身份。
pub struct Identity {
    key: SigningKey,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Identity({})", self.id())
    }
}

impl Identity {
    /// 随机生成（测试用；正式使用 [`Identity::load_or_create`]）。
    pub fn generate() -> Self {
        Self {
            key: SigningKey::generate(&mut rand::rngs::OsRng),
        }
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(&seed),
        }
    }

    /// 从 `dir/device.key` 读取；不存在时生成并保存（权限 0600）。
    pub fn load_or_create(dir: &Path) -> std::io::Result<Self> {
        let path = dir.join("device.key");
        match std::fs::read(&path) {
            Ok(bytes) => {
                let seed: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{} 长度不是 32 字节", path.display()),
                    )
                })?;
                Ok(Self::from_seed(seed))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(dir)?;
                let id = Self::generate();
                write_private(&path, &id.key.to_bytes())?;
                Ok(id)
            }
            Err(e) => Err(e),
        }
    }

    pub fn id(&self) -> DeviceId {
        DeviceId(self.key.verifying_key().to_bytes())
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.key.sign(msg).to_bytes()
    }

    /// PKCS#8 DER 编码的私钥（TLS 证书用）。
    pub(crate) fn pkcs8_der(&self) -> Vec<u8> {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        self.key
            .to_pkcs8_der()
            .map(|d| d.as_bytes().to_vec())
            .unwrap_or_default()
    }
}

/// 用设备 ID（公钥）验证签名。
pub fn verify(id: &DeviceId, msg: &[u8], sig: &[u8; 64]) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(&id.0) else {
        return false;
    };
    vk.verify(msg, &Signature::from_bytes(sig)).is_ok()
}

/// 严格验证（TLS 握手签名用）。
pub(crate) fn verify_strict(id: &DeviceId, msg: &[u8], sig: &[u8]) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(&id.0) else {
        return false;
    };
    let Ok(sig) = Signature::from_slice(sig) else {
        return false;
    };
    vk.verify_strict(msg, &sig).is_ok()
}

/// 写入只有本用户可读写的文件（先写临时文件再改名）。
pub(crate) fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    oink_foundation::storage::atomic_write(path, data)
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn invalid_key_is_preserved_and_never_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.key");
        std::fs::write(&path, b"invalid seed").unwrap();
        assert_eq!(
            Identity::load_or_create(dir.path()).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"invalid seed");
    }

    #[test]
    fn persist_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(dir.path()).unwrap();
        let b = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(a.id(), b.id());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("device.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let sig = a.sign(b"hello");
        assert!(verify(&a.id(), b"hello", &sig));
        assert!(!verify(&a.id(), b"hellO", &sig));
    }
}
