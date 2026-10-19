//! QUIC 的 TLS 配置：双方都用设备 Ed25519 密钥签发的自签名证书完成 TLS 1.3 握手。
//!
//! 验证方式不依赖任何 CA：从证书中取出 Ed25519 公钥作为对方的设备 ID，并用这个公钥验证握手签名
//! （`CertificateVerify`），所以握手结束即确知对方身份。发起方若知道要连的设备 ID，会在握手中核对；
//! 接受方先接受任意设备，握手后再按组名单决定是正常连接还是只允许配对。

use crate::identity::{self, Identity};
use oink_foundation::DeviceId;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use std::sync::Arc;

pub const ALPN: &[u8] = oink_protocol::v1::QUIC_ALPN;
/// 证书里的名字（不参与验证）。
pub const SERVER_NAME: &str = "oink.invalid";

/// Parse the certificate structure before selecting its Ed25519 SPKI. A matching
/// byte sequence in a subject name or extension is never an identity.
pub fn device_id_from_cert(cert: &[u8]) -> Option<DeviceId> {
    use x509_cert::der::Decode;
    let certificate = x509_cert::Certificate::from_der(cert).ok()?;
    let spki = certificate.tbs_certificate.subject_public_key_info;
    if spki.algorithm.oid != x509_cert::der::asn1::ObjectIdentifier::new_unwrap("1.3.101.112")
        || spki.algorithm.parameters.is_some()
    {
        return None;
    }
    let key = spki.subject_public_key.as_bytes()?;
    Some(DeviceId(key.try_into().ok()?))
}

/// 本机证书与私钥。
pub struct Credentials {
    cert: CertificateDer<'static>,
    key: Vec<u8>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials")
    }
}

impl Credentials {
    /// Bulk TCP has its own ALPN and measured cipher policy; interactive QUIC is unchanged.
    pub(crate) fn bulk_server_config(&self) -> anyhow::Result<rustls::ServerConfig> {
        let mut provider = rustls::crypto::ring::default_provider();
        provider.cipher_suites = vec![rustls::crypto::ring::cipher_suite::TLS13_AES_128_GCM_SHA256];
        let mut cfg = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_client_cert_verifier(Arc::new(PeerVerifier { expected: None }))
            .with_single_cert(vec![self.cert.clone()], self.key())?;
        cfg.alpn_protocols = vec![oink_protocol::v1::LOW_ALPN.to_vec()];
        cfg.max_early_data_size = 0;
        Ok(cfg)
    }

    pub(crate) fn bulk_client_config(&self, expected: DeviceId) -> anyhow::Result<rustls::ClientConfig> {
        let mut provider = rustls::crypto::ring::default_provider();
        provider.cipher_suites = vec![rustls::crypto::ring::cipher_suite::TLS13_AES_128_GCM_SHA256];
        let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PeerVerifier {
                expected: Some(expected),
            }))
            .with_client_auth_cert(vec![self.cert.clone()], self.key())?;
        cfg.alpn_protocols = vec![oink_protocol::v1::LOW_ALPN.to_vec()];
        cfg.enable_early_data = false;
        Ok(cfg)
    }

    pub fn new(id: &Identity) -> anyhow::Result<Self> {
        let pkcs8 = id.pkcs8_der();
        let kp = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(
            &PrivatePkcs8KeyDer::from(pkcs8.clone()),
            &rcgen::PKCS_ED25519,
        )?;
        let params = rcgen::CertificateParams::new(vec![SERVER_NAME.to_string()])?;
        let cert = params.self_signed(&kp)?;
        let der = CertificateDer::from(cert.der().to_vec());
        anyhow::ensure!(
            device_id_from_cert(&der) == Some(id.id()),
            "证书中的公钥与设备 ID 不一致"
        );
        Ok(Self { cert: der, key: pkcs8 })
    }

    fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone()))
    }

    fn provider() -> Arc<rustls::crypto::CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    pub fn server_config(&self) -> anyhow::Result<rustls::ServerConfig> {
        let mut cfg = rustls::ServerConfig::builder_with_provider(Self::provider())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_client_cert_verifier(Arc::new(PeerVerifier { expected: None }))
            .with_single_cert(vec![self.cert.clone()], self.key())?;
        cfg.alpn_protocols = vec![ALPN.to_vec()];
        cfg.max_early_data_size = 0;
        Ok(cfg)
    }

    /// 发起连接用的配置；`expected` 为要连的设备 ID（`None` 表示接受任意设备）。
    pub fn client_config(&self, expected: Option<DeviceId>) -> anyhow::Result<rustls::ClientConfig> {
        let mut cfg = rustls::ClientConfig::builder_with_provider(Self::provider())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PeerVerifier { expected }))
            .with_client_auth_cert(vec![self.cert.clone()], self.key())?;
        cfg.alpn_protocols = vec![ALPN.to_vec()];
        cfg.enable_early_data = false;
        Ok(cfg)
    }
}

#[derive(Debug)]
struct PeerVerifier {
    expected: Option<DeviceId>,
}

impl PeerVerifier {
    fn check_cert(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let id = device_id_from_cert(cert)
            .ok_or(rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
        if let Some(exp) = self.expected
            && exp != id
        {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ));
        }
        Ok(())
    }

    fn check_sig(
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        if dss.scheme != SignatureScheme::ED25519 {
            return Err(rustls::Error::PeerIncompatible(
                rustls::PeerIncompatible::NoSignatureSchemesInCommon,
            ));
        }
        let id = device_id_from_cert(cert)
            .ok_or(rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
        if identity::verify_strict(&id, message, dss.signature()) {
            Ok(HandshakeSignatureValid::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::BadSignature,
            ))
        }
    }
}

impl ServerCertVerifier for PeerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check_cert(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls13RequiredForQuic,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Self::check_sig(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

impl ClientCertVerifier for PeerVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check_cert(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls13RequiredForQuic,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Self::check_sig(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certificate_identity_requires_a_complete_ed25519_spki() {
        let id = Identity::generate();
        let credentials = Credentials::new(&id).unwrap();
        let der = credentials.cert.as_ref();
        for end in 0..der.len() {
            assert_eq!(device_id_from_cert(&der[..end]), None);
        }
        let mut trailing = der.to_vec();
        trailing.push(0);
        assert_eq!(device_id_from_cert(&trailing), None);
        let mut fake = vec![0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
        fake.extend_from_slice(&id.id().0);
        assert_eq!(device_id_from_cert(&fake), None);

        // A valid certificate carrying a different algorithm is not an Ed25519 identity.
        let kp = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec![SERVER_NAME.to_string()]).unwrap();
        let other = params.self_signed(&kp).unwrap();
        assert_eq!(device_id_from_cert(other.der()), None);
        let verifier = PeerVerifier {
            expected: Some(Identity::generate().id()),
        };
        assert!(verifier.check_cert(&credentials.cert).is_err());
    }

    #[test]
    fn tls_configs_offer_only_the_oink_protocol_and_disable_early_data() {
        let credentials = Credentials::new(&Identity::generate()).unwrap();
        let server = credentials.server_config().unwrap();
        let client = credentials.client_config(None).unwrap();
        assert_eq!(server.alpn_protocols, [b"oink-wire/1".to_vec()]);
        assert_eq!(client.alpn_protocols, server.alpn_protocols);
        assert_eq!(server.max_early_data_size, 0);
        assert!(!client.enable_early_data);
        let server = credentials.bulk_server_config().unwrap();
        let client = credentials.bulk_client_config(Identity::generate().id()).unwrap();
        assert_eq!(server.alpn_protocols, [b"oink-low/1".to_vec()]);
        assert_eq!(client.alpn_protocols, server.alpn_protocols);
        assert_eq!(server.max_early_data_size, 0);
        assert!(!client.enable_early_data);
    }

    #[test]
    fn cert_carries_device_id() {
        let id = Identity::generate();
        let c = Credentials::new(&id).unwrap();
        assert_eq!(device_id_from_cert(&c.cert), Some(id.id()));
        c.server_config().unwrap();
        c.client_config(Some(id.id())).unwrap();
    }
}
