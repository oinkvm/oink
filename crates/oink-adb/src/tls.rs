use crate::{AdbKey, Error, Result};
use rustls::{
    ClientConfig, DigitallySignedStruct, SignatureScheme,
    client::{
        ResolvesClientCert,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    sign::CertifiedKey,
};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream};
use x509_cert::{der::Decode, name::Name};

#[derive(Debug)]
struct Keys(Vec<(String, Arc<CertifiedKey>)>);
impl ResolvesClientCert for Keys {
    fn resolve(&self, root_hint_subjects: &[&[u8]], sigschemes: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        for hint in root_hint_subjects {
            let Ok(name) = Name::from_der(hint) else { continue };
            let mut adb = false;
            let mut fingerprint = None;
            for rdn in name.0 {
                for attribute in rdn.0.iter() {
                    let oid = attribute.oid.to_string();
                    let value = std::str::from_utf8(attribute.value.value()).ok();
                    if oid == "2.5.4.10" && value == Some("AdbKey-0") {
                        adb = true;
                    }
                    if oid == "2.5.4.3" {
                        fingerprint = value.map(str::to_owned);
                    }
                }
            }
            if adb && let Some(fingerprint) = fingerprint {
                for (known, key) in &self.0 {
                    if known.eq_ignore_ascii_case(&fingerprint) && key.key.choose_scheme(sigschemes).is_some() {
                        return Some(key.clone());
                    }
                }
            }
        }
        self.0
            .iter()
            .find(|(_, key)| key.key.choose_scheme(sigschemes).is_some())
            .map(|(_, key)| key.clone())
    }
    fn has_certs(&self) -> bool {
        !self.0.is_empty()
    }
}
#[derive(Debug)]
struct AdbVerifier;
impl ServerCertVerifier for AdbVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        // adbd uses self-signed certificates, not public WebPKI identities.
        // Pairing authenticates the TLS channel with SPAKE2 + exporter. Oink
        // bootstrap additionally binds the installed APK and group identity.
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("ADB requires TLS 1.3".into()))
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
pub(crate) async fn connect(socket: TcpStream, keys: &[AdbKey]) -> Result<TlsStream<TcpStream>> {
    if keys.is_empty() {
        return Err(Error::Key);
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut certificates = Vec::with_capacity(keys.len());
    for key in keys {
        let pkcs8 = PrivatePkcs8KeyDer::from(key.der().to_vec());
        let pair =
            rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&pkcs8, &rcgen::PKCS_RSA_SHA256).map_err(|_| Error::Key)?;
        let params = rcgen::CertificateParams::new(vec!["adb".into()]).map_err(|_| Error::Key)?;
        let certificate = params.self_signed(&pair).map_err(|_| Error::Key)?;
        let certified =
            CertifiedKey::from_der(vec![certificate.der().clone()], pkcs8.into(), &provider).map_err(|_| Error::Key)?;
        certificates.push((key.fingerprint().to_owned(), Arc::new(certified)));
    }
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| Error::Key)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AdbVerifier))
        .with_client_cert_resolver(Arc::new(Keys(certificates)));
    config.enable_early_data = false;
    TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("adb").expect("static name"), socket)
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::InvalidData {
                Error::Authentication
            } else {
                Error::Io(error)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn tls13_exporter_matches_both_sides_and_is_channel_bound() {
        let key = AdbKey::synthetic();
        let pkcs8 = PrivatePkcs8KeyDer::from(key.der().to_vec());
        let pair = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&pkcs8, &rcgen::PKCS_RSA_SHA256).unwrap();
        let cert = rcgen::CertificateParams::new(vec!["adb".into()])
            .unwrap()
            .self_signed(&pair)
            .unwrap();
        let server = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], pkcs8.into())
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = tokio_rustls::TlsAcceptor::from(Arc::new(server))
                .accept(socket)
                .await
                .unwrap();
            let exporter = stream
                .get_ref()
                .1
                .export_keying_material([0; 64], b"adb-label\0", None)
                .unwrap();
            stream.write_all(&exporter).await.unwrap();
        });
        let mut stream = connect(TcpStream::connect(address).await.unwrap(), &[key])
            .await
            .unwrap();
        let exporter = stream
            .get_ref()
            .1
            .export_keying_material([0; 64], b"adb-label\0", None)
            .unwrap();
        let wrong = stream
            .get_ref()
            .1
            .export_keying_material([0; 64], b"adb-label", None)
            .unwrap();
        let mut expected = [0; 64];
        stream.read_exact(&mut expected).await.unwrap();
        assert_eq!(exporter, expected);
        assert_ne!(exporter, wrong);
        assert_eq!(
            stream.get_ref().1.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_3)
        );
        server.await.unwrap();
    }
    #[test]
    fn adb_ca_issuer_selects_matching_group_key() {
        use x509_cert::der::Encode;
        let provider = rustls::crypto::ring::default_provider();
        let mut keys = Vec::new();
        for fingerprint in ["other-key", AdbKey::synthetic().fingerprint()] {
            let key = AdbKey::synthetic();
            let private = PrivatePkcs8KeyDer::from(key.der().to_vec());
            let pair = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&private, &rcgen::PKCS_RSA_SHA256).unwrap();
            let cert = rcgen::CertificateParams::new(vec!["adb".into()])
                .unwrap()
                .self_signed(&pair)
                .unwrap();
            let certified = CertifiedKey::from_der(vec![cert.der().clone()], private.into(), &provider).unwrap();
            keys.push((fingerprint.to_owned(), Arc::new(certified)));
        }
        let issuer: Name = format!("O=AdbKey-0,CN={}", keys[1].0).parse().unwrap();
        let encoded = issuer.to_der().unwrap();
        let expected = keys[1].1.clone();
        let keys = Keys(keys);
        let selected = keys.resolve(&[&encoded], &[SignatureScheme::RSA_PSS_SHA256]).unwrap();
        assert!(Arc::ptr_eq(&expected, &selected));
    }
}
