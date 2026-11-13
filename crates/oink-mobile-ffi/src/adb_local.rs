//! Direct-Boot-safe localhost ADB functions. They never open a MobileSession or CE store.
use crate::{MobileError, UserFacingCode};
use oink_adb::AdbKey;
use std::{net::SocketAddr, time::Duration};
use zeroize::Zeroizing;

fn failure() -> MobileError {
    // No peer data, pairing code, private material or shell output in diagnostics.
    MobileError::Command {
        code: UserFacingCode::BackendFailed,
        detail: "local ADB operation failed".into(),
    }
}
fn adb_failure(error: oink_adb::Error) -> MobileError {
    let (code, detail) = match error {
        oink_adb::Error::Authentication => (UserFacingCode::PermissionDenied, "ADB pairing authorization required"),
        oink_adb::Error::Key => (UserFacingCode::InvalidValue, "ADB credential is invalid"),
        oink_adb::Error::Io(_) => (UserFacingCode::LinkLost, "local ADB connection failed"),
        _ => (UserFacingCode::BackendFailed, "local ADB startup failed"),
    };
    MobileError::Command {
        code,
        detail: detail.into(),
    }
}
fn address(host: &str, port: u16) -> Result<SocketAddr, MobileError> {
    if port == 0 {
        return Err(failure());
    }
    let value = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    value.parse().map_err(|_| failure())
}
fn local_address(host: &str, port: u16) -> Result<SocketAddr, MobileError> {
    let address = address(host, port)?;
    if address.ip().is_loopback() {
        return Ok(address);
    }
    // NSD supplies a port hint. Check the numeric interface independently
    // before sending credentials to it if the preferred loopback fails.
    if if_addrs::get_if_addrs().is_ok_and(|interfaces| {
        interfaces.iter().any(|interface| {
            interface.ip() == address.ip()
                && match address {
                    SocketAddr::V4(_) => true,
                    SocketAddr::V6(address) => {
                        if address.scope_id() == 0 {
                            !address.ip().is_unicast_link_local()
                        } else {
                            interface.index == Some(address.scope_id())
                        }
                    }
                }
        })
    }) {
        Ok(address)
    } else {
        Err(MobileError::Command {
            code: UserFacingCode::InvalidAddress,
            detail: "ADB endpoint is not a local interface".into(),
        })
    }
}

fn key(value: &[u8]) -> Result<AdbKey, MobileError> {
    if value.len() > 8 * 1024 {
        return Err(adb_failure(oink_adb::Error::Key));
    }
    AdbKey::from_pkcs8_pem(value).map_err(adb_failure)
}

fn keys(values: &[Vec<u8>]) -> Result<Vec<AdbKey>, MobileError> {
    if !(1..=16).contains(&values.len()) {
        return Err(adb_failure(oink_adb::Error::Key));
    }
    values.iter().map(|value| key(value)).collect()
}
#[uniffi::export]
pub fn adb_public_key(private_pem: Vec<u8>) -> Result<Vec<u8>, MobileError> {
    let private_pem = Zeroizing::new(private_pem);
    key(&private_pem).map(|key| key.public_key().into_bytes())
}
#[uniffi::export(async_runtime = "tokio")]
pub async fn adb_pair_local(host: String, port: u16, code: String, private_pem: Vec<u8>) -> Result<(), MobileError> {
    let code = Zeroizing::new(code);
    let private_pem = Zeroizing::new(private_pem);
    if code.len() != 6 || !code.bytes().all(|value| value.is_ascii_digit()) {
        return Err(adb_failure(oink_adb::Error::Authentication));
    }
    let local = local_address(&host, port)?;
    let key = key(&private_pem)?;
    let loopback = SocketAddr::from(([127, 0, 0, 1], port));
    tokio::time::timeout(Duration::from_secs(10), async {
        match oink_adb::pair(loopback, &code, &key).await {
            Ok(_) => Ok(()),
            Err(oink_adb::Error::Io(_)) if local != loopback => oink_adb::pair(local, &code, &key)
                .await
                .map(|_| ())
                .map_err(adb_failure),
            Err(error) => Err(adb_failure(error)),
        }
    })
    .await
    .map_err(|_| failure())?
}
#[uniffi::export(async_runtime = "tokio")]
pub async fn adb_start_local(
    host: String,
    port: u16,
    private_pems: Vec<Vec<u8>>,
    apk_path: String,
) -> Result<(), MobileError> {
    let private_pems = Zeroizing::new(private_pems);
    let local = local_address(&host, port)?;
    let keys = keys(&private_pems)?;
    let loopback = SocketAddr::from(([127, 0, 0, 1], port));
    tokio::time::timeout(Duration::from_secs(10), async {
        match oink_adb::start_helper(loopback, &keys, &apk_path).await {
            Ok(()) => Ok(()),
            Err(oink_adb::Error::Io(_)) if local != loopback => oink_adb::start_helper(local, &keys, &apk_path)
                .await
                .map_err(adb_failure),
            Err(error) => Err(adb_failure(error)),
        }
    })
    .await
    .map_err(|_| failure())?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numeric_addresses_preserve_ipv6_scope_and_reject_dns() {
        assert_eq!(address("127.0.0.1", 42).unwrap().port(), 42);
        let SocketAddr::V6(value) = address("fe80::1%7", 42).unwrap() else {
            panic!("IPv6")
        };
        assert_eq!(value.scope_id(), 7);
        assert!(address("example.invalid", 42).is_err());
        assert!(address("127.0.0.1", 0).is_err());
    }

    #[test]
    fn local_exports_reject_invalid_credentials_and_wildcard_addresses() {
        assert!(local_address("::1", 42).is_ok());
        assert!(local_address("0.0.0.0", 42).is_err());
        assert!(local_address("::", 42).is_err());
        assert!(matches!(
            adb_public_key(b"malformed private key".to_vec()),
            Err(MobileError::Command {
                code: UserFacingCode::InvalidValue,
                ..
            })
        ));
        assert!(keys(&[(); 17].map(|_| Vec::new())).is_err());
    }
}
