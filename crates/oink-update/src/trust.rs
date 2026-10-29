//! Reviewed release roots are compiled into binaries. No environment/IPC override.
use crate::{Error, WINDOWS_EPOCH};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
#[derive(Clone, Debug)]
pub struct Roots {
    pub key: [u8; 32],
    pub signer_spki_sha256: Vec<String>,
}
#[derive(Deserialize)]
struct Config {
    schema: u32,
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
    #[cfg(feature = "test-feed")]
    {
        let _: Config = crate::feed::strict_json(CONFIG)?;
        // RFC 8032 public TEST key. No production secret or config override.
        let public = STANDARD
            .decode("11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=")
            .map_err(|_| Error::NotConfigured)?;
        Ok(Roots {
            key: public.try_into().map_err(|_| Error::NotConfigured)?,
            signer_spki_sha256: vec![],
        })
    }
    #[cfg(not(feature = "test-feed"))]
    windows_configured(CONFIG)
}
#[cfg_attr(all(feature = "test-feed", not(test)), allow(dead_code))]
fn windows_configured(raw: &[u8]) -> Result<Roots, Error> {
    let windows = windows_entry(raw)?;
    if !windows.enabled {
        return Err(Error::NotConfigured);
    }
    let key = STANDARD
        .decode(windows.public_key.ok_or(Error::NotConfigured)?)
        .map_err(|_| Error::NotConfigured)?;
    validate_pins(&windows.authenticode_spki_sha256)?;
    Ok(Roots {
        key: key.try_into().map_err(|_| Error::NotConfigured)?,
        signer_spki_sha256: windows.authenticode_spki_sha256,
    })
}
fn windows_entry(raw: &[u8]) -> Result<Windows, Error> {
    let config: Config = crate::feed::strict_json(raw)?;
    if config.schema != 1 {
        return Err(Error::NotConfigured);
    }
    config
        .windows
        .into_iter()
        .find(|e| e.epoch == WINDOWS_EPOCH)
        .ok_or(Error::NotConfigured)
}
fn validate_pins(pins: &[String]) -> Result<(), Error> {
    if pins
        .iter()
        .any(|pin| pin.len() != 64 || !pin.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    {
        return Err(Error::NotConfigured);
    }
    Ok(())
}
impl Roots {
    pub fn feed(&self) -> crate::feed::Trust<'_> {
        crate::feed::Trust {
            epoch: WINDOWS_EPOCH,
            key: &self.key,
        }
    }
}

#[derive(Debug)]
pub struct LocalRoots {
    pub signer_spki_sha256: Vec<String>,
}
/// Local MSI authority is the elevated installed CLI's kernel token. Compiled
/// package pins still apply, even when the independent feed gate/key is absent.
/// This type has no manifest key and cannot be used to verify a feed.
pub fn local_packages() -> Result<LocalRoots, Error> {
    local_configured(CONFIG)
}
fn local_configured(raw: &[u8]) -> Result<LocalRoots, Error> {
    let windows = windows_entry(raw)?;
    validate_pins(&windows.authenticode_spki_sha256)?;
    Ok(LocalRoots {
        signer_spki_sha256: windows.authenticode_spki_sha256,
    })
}

/// A TEST-only candidate can withhold health proof to qualify real rollback.
/// This is compiled into that candidate; neither IPC nor runtime env enables it.
pub fn health_ready(ready: bool) -> bool {
    #[cfg(feature = "test-feed")]
    if env!("OINK_WINDOWS_UPDATE_TEST_FAIL_HEALTH") == "1" {
        return false;
    }
    ready
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_packages_keep_pins_without_enabling_or_requiring_a_feed_key() {
        let mut config: serde_json::Value = serde_json::from_slice(CONFIG).unwrap();
        config["windows"][0]["enabled"] = false.into();
        config["windows"][0]["public_key"] = serde_json::Value::Null;
        config["windows"][0]["authenticode_spki_sha256"] = serde_json::json!(["a".repeat(64)]);
        let raw = serde_json::to_vec(&config).unwrap();
        assert!(matches!(windows_configured(&raw), Err(Error::NotConfigured)));
        assert_eq!(local_configured(&raw).unwrap().signer_spki_sha256, ["a".repeat(64)]);
        config["windows"][0]["authenticode_spki_sha256"] = serde_json::json!(["invalid"]);
        assert!(local_configured(&serde_json::to_vec(&config).unwrap()).is_err());
    }
    #[test]
    fn disabled_release_gates_are_independent_of_configured_keys() {
        let mut config: serde_json::Value = serde_json::from_slice(CONFIG).unwrap();
        config["mac"]["enabled"] = false.into();
        config["mac"]["public_key"] = STANDARD.encode([1; 32]).into();
        config["windows"][0]["enabled"] = false.into();
        config["windows"][0]["public_key"] = STANDARD.encode([2; 32]).into();
        let disabled = serde_json::to_vec(&config).unwrap();
        assert!(matches!(mac_configured(&disabled), Err(Error::NotConfigured)));
        assert!(matches!(windows_configured(&disabled), Err(Error::NotConfigured)));
    }
    #[test]
    fn each_platform_requires_enablement_and_its_own_roots() {
        for mac in [false, true] {
            for windows in [false, true] {
                let mut config: serde_json::Value = serde_json::from_slice(CONFIG).unwrap();
                config["mac"]["public_key"] = serde_json::Value::Null;
                config["windows"][0]["public_key"] = serde_json::Value::Null;
                config["windows"][0]["authenticode_spki_sha256"] = serde_json::json!([]);
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
    #[test]
    fn windows_manifest_root_allows_unsigned_packages_and_validates_optional_pins() {
        let mut config: serde_json::Value = serde_json::from_slice(CONFIG).unwrap();
        config["windows"][0]["enabled"] = true.into();
        config["windows"][0]["public_key"] = STANDARD.encode([2; 32]).into();
        config["windows"][0]["authenticode_spki_sha256"] = serde_json::json!([]);
        assert!(
            windows_configured(&serde_json::to_vec(&config).unwrap())
                .unwrap()
                .signer_spki_sha256
                .is_empty()
        );
        for pin in ["", "garbage", &"A".repeat(64), &"a".repeat(63)] {
            config["windows"][0]["authenticode_spki_sha256"] = serde_json::json!([pin]);
            assert!(matches!(
                windows_configured(&serde_json::to_vec(&config).unwrap()),
                Err(Error::NotConfigured)
            ));
        }
    }
}
