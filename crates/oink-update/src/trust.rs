//! Reviewed release roots are compiled into binaries. No environment/IPC override.
use crate::{Error, WINDOWS_EPOCH};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
#[derive(Debug)]
pub struct Roots {
    pub key: [u8; 32],
    pub repository: String,
    pub signer_spki_sha256: Vec<String>,
}
#[derive(Deserialize)]
struct Config {
    schema: u32,
    repository: String,
    windows: Vec<Windows>,
    mac: Mac,
}
#[derive(Deserialize)]
struct Windows {
    enabled: bool,
    epoch: String,
    public_key: Option<String>,
    authenticode_spki_sha256: Vec<String>,
}
#[derive(Deserialize)]
struct Mac {
    enabled: bool,
    public_key: Option<String>,
}
const CONFIG: &[u8] = include_bytes!("../../../packaging/update-trust.json");

/// Release qualification is independent of whether Sparkle was linked or a key installed.
pub fn mac_enabled() -> bool {
    mac_configured(CONFIG).is_ok()
}
fn mac_configured(raw: &[u8]) -> Result<(), Error> {
    let config: Config = crate::feed::strict_json(raw)?;
    if config.schema != 1 || !config.mac.enabled {
        return Err(Error::NotConfigured);
    }
    let key = STANDARD
        .decode(config.mac.public_key.ok_or(Error::NotConfigured)?)
        .map_err(|_| Error::NotConfigured)?;
    if key.len() != 32 {
        return Err(Error::NotConfigured);
    }
    Ok(())
}
pub fn compiled() -> Result<Roots, Error> {
    windows_configured(CONFIG)
}
fn windows_configured(raw: &[u8]) -> Result<Roots, Error> {
    let config: Config = crate::feed::strict_json(raw)?;
    if config.schema != 1 {
        return Err(Error::NotConfigured);
    }
    let windows = config
        .windows
        .into_iter()
        .find(|e| e.epoch == WINDOWS_EPOCH)
        .ok_or(Error::NotConfigured)?;
    if !windows.enabled {
        return Err(Error::NotConfigured);
    }
    let key = STANDARD
        .decode(windows.public_key.ok_or(Error::NotConfigured)?)
        .map_err(|_| Error::NotConfigured)?;
    if windows.authenticode_spki_sha256.is_empty()
        || windows
            .authenticode_spki_sha256
            .iter()
            .any(|pin| pin.len() != 64 || !pin.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    {
        return Err(Error::NotConfigured);
    }
    Ok(Roots {
        key: key.try_into().map_err(|_| Error::NotConfigured)?,
        repository: config.repository,
        signer_spki_sha256: windows.authenticode_spki_sha256,
    })
}
impl Roots {
    pub fn feed(&self) -> crate::feed::Trust<'_> {
        crate::feed::Trust {
            epoch: WINDOWS_EPOCH,
            key: &self.key,
            repository: &self.repository,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn beta_is_manual_only_on_both_platforms() {
        assert!(!mac_enabled());
        assert!(matches!(compiled(), Err(Error::NotConfigured)));
    }
    #[test]
    fn each_platform_requires_enablement_and_its_own_roots() {
        for mac in [false, true] {
            for windows in [false, true] {
                let mut config: serde_json::Value = serde_json::from_slice(CONFIG).unwrap();
                config["mac"]["enabled"] = mac.into();
                config["windows"][0]["enabled"] = windows.into();
                let empty = serde_json::to_vec(&config).unwrap();
                assert!(mac_configured(&empty).is_err());
                assert!(windows_configured(&empty).is_err());
                config["mac"]["public_key"] = STANDARD.encode([1; 32]).into();
                config["windows"][0]["public_key"] = STANDARD.encode([2; 32]).into();
                config["windows"][0]["authenticode_spki_sha256"] = serde_json::json!(["a".repeat(64)]);
                let roots = serde_json::to_vec(&config).unwrap();
                assert_eq!(mac_configured(&roots).is_ok(), mac);
                assert_eq!(windows_configured(&roots).is_ok(), windows);
            }
        }
    }
}
