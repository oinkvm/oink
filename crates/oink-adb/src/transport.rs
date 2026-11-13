use crate::{AdbKey, Error, Result};
use std::{collections::VecDeque, net::SocketAddr};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};
use tokio_rustls::client::TlsStream;

const CNXN: u32 = u32::from_le_bytes(*b"CNXN");
const STLS: u32 = u32::from_le_bytes(*b"STLS");
const OPEN: u32 = u32::from_le_bytes(*b"OPEN");
const OKAY: u32 = u32::from_le_bytes(*b"OKAY");
const WRTE: u32 = u32::from_le_bytes(*b"WRTE");
const CLSE: u32 = u32::from_le_bytes(*b"CLSE");
const MAX_DATA: usize = 1024 * 1024;
const MAX_OUTPUT: usize = 8 * 1024 * 1024;

#[derive(Debug)]
struct Packet {
    command: u32,
    arg0: u32,
    arg1: u32,
    data: Vec<u8>,
}
impl Packet {
    async fn read<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Self> {
        let mut header = [0; 24];
        stream.read_exact(&mut header).await?;
        let fields: Vec<u32> = header
            .as_chunks::<4>()
            .0
            .iter()
            .map(|word| u32::from_le_bytes(*word))
            .collect();
        if fields[0] ^ fields[5] != u32::MAX {
            return Err(Error::Protocol);
        }
        let length = fields[3] as usize;
        if length > MAX_DATA {
            return Err(Error::Limit);
        }
        let mut data = vec![0; length];
        stream.read_exact(&mut data).await?;
        if fields[4] != 0 && fields[4] != checksum(&data) {
            return Err(Error::Protocol);
        }
        Ok(Self {
            command: fields[0],
            arg0: fields[1],
            arg1: fields[2],
            data,
        })
    }
    async fn write<S: AsyncWrite + Unpin>(&self, stream: &mut S) -> Result<()> {
        if self.data.len() > MAX_DATA {
            return Err(Error::Limit);
        }
        let mut header = [0; 24];
        for (chunk, value) in header.as_chunks_mut::<4>().0.iter_mut().zip([
            self.command,
            self.arg0,
            self.arg1,
            self.data.len() as u32,
            checksum(&self.data),
            self.command ^ u32::MAX,
        ]) {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        stream.write_all(&header).await?;
        stream.write_all(&self.data).await?;
        stream.flush().await?;
        Ok(())
    }
}
fn checksum(data: &[u8]) -> u32 {
    data.iter().fold(0_u32, |sum, byte| sum.wrapping_add((*byte).into()))
}

pub struct Client {
    stream: TlsStream<TcpStream>,
    max_data: usize,
    next_id: u32,
    active: bool,
}
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("max_data", &self.max_data)
            .finish_non_exhaustive()
    }
}
pub struct ShellOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: u8,
}
impl std::fmt::Debug for ShellOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellOutput")
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr.len())
            .field("exit_code", &self.exit_code)
            .finish()
    }
}
impl Client {
    pub fn server_fingerprint(&self) -> Result<[u8; 32]> {
        let cert = self
            .stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|certs| certs.first())
            .ok_or(Error::Authentication)?;
        certificate_fingerprint(cert.as_ref())
    }
    pub async fn connect(address: SocketAddr, keys: &[AdbKey]) -> Result<Self> {
        if keys.is_empty() || keys.len() > 16 {
            return Err(Error::Key);
        }
        let mut socket = TcpStream::connect(address).await?;
        socket.set_nodelay(true)?;
        Packet {
            command: CNXN,
            arg0: 0x01000001,
            arg1: MAX_DATA as u32,
            data: b"host::features=shell_v2,cmd\0".to_vec(),
        }
        .write(&mut socket)
        .await?;
        let request = Packet::read(&mut socket).await?;
        if request.command != STLS {
            return Err(Error::Authentication);
        }
        Packet {
            command: STLS,
            arg0: 0x01000000,
            arg1: 0,
            data: Vec::new(),
        }
        .write(&mut socket)
        .await?;
        let mut stream = crate::tls::connect(socket, keys).await?;
        let banner = Packet::read(&mut stream).await.map_err(authentication_error)?;
        if banner.command != CNXN || banner.arg1 == 0 {
            return Err(Error::Authentication);
        }
        Ok(Self {
            stream,
            max_data: (banner.arg1 as usize).min(MAX_DATA),
            next_id: 1,
            active: false,
        })
    }
    async fn service(&mut self, name: &str) -> Result<Service<'_>> {
        if self.active {
            return Err(Error::Protocol);
        }
        if name.contains('\0') {
            return Err(Error::Protocol);
        }
        self.active = true;
        let local = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or(Error::Limit)?;
        let mut data = name.as_bytes().to_vec();
        data.push(0);
        Packet {
            command: OPEN,
            arg0: local,
            arg1: 0,
            data,
        }
        .write(&mut self.stream)
        .await?;
        let packet = Packet::read(&mut self.stream).await?;
        if packet.command != OKAY || packet.arg1 != local || packet.arg0 == 0 {
            return Err(Error::Protocol);
        }
        Ok(Service {
            client: self,
            local,
            remote: packet.arg0,
            pending: VecDeque::new(),
            closed: false,
        })
    }
    pub async fn shell(&mut self, command: &str) -> Result<ShellOutput> {
        let mut service = self.service(&format!("shell,v2,raw:{command}")).await?;
        service.write(&[4, 0, 0, 0, 0]).await?; // shell-v2 close stdin
        let mut bytes = Vec::new();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit = None;
        while let Some(chunk) = service.read().await? {
            bytes.extend_from_slice(&chunk);
            while bytes.len() >= 5 {
                let length = u32::from_le_bytes(bytes[1..5].try_into().expect("length")) as usize;
                if length > MAX_OUTPUT {
                    return Err(Error::Limit);
                }
                if bytes.len() < 5 + length {
                    break;
                }
                let payload = &bytes[5..5 + length];
                match bytes[0] {
                    1 => stdout.extend_from_slice(payload),
                    2 => stderr.extend_from_slice(payload),
                    3 if length == 1 && exit.is_none() => exit = Some(payload[0]),
                    _ => return Err(Error::Protocol),
                }
                if stdout.len() + stderr.len() > MAX_OUTPUT {
                    return Err(Error::Limit);
                }
                bytes.drain(..5 + length);
            }
        }
        if !bytes.is_empty() {
            return Err(Error::Protocol);
        }
        Ok(ShellOutput {
            stdout,
            stderr,
            exit_code: exit.ok_or(Error::Protocol)?,
        })
    }
}
fn certificate_fingerprint(der: &[u8]) -> Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    use x509_cert::der::{Decode, Encode};
    // AOSP generates a new certificate with current validity timestamps for
    // each transport, but reuses adbd's process RSA key. Pin its SPKI, not DER.
    let cert = x509_cert::Certificate::from_der(der).map_err(|_| Error::Authentication)?;
    let spki = cert
        .tbs_certificate
        .subject_public_key_info
        .to_der()
        .map_err(|_| Error::Authentication)?;
    Ok(Sha256::digest(spki).into())
}
fn authentication_error(error: Error) -> Error {
    match error {
        Error::Io(ref io)
            if io
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<rustls::Error>())
                .is_some_and(|error| matches!(error, rustls::Error::AlertReceived(_))) =>
        {
            Error::Authentication
        }
        _ => error,
    }
}
struct Service<'a> {
    client: &'a mut Client,
    local: u32,
    remote: u32,
    pending: VecDeque<Vec<u8>>,
    closed: bool,
}
impl Service<'_> {
    async fn send(&mut self, command: u32, data: Vec<u8>) -> Result<()> {
        Packet {
            command,
            arg0: self.local,
            arg1: self.remote,
            data,
        }
        .write(&mut self.client.stream)
        .await
    }
    async fn packet(&mut self) -> Result<Packet> {
        let packet = Packet::read(&mut self.client.stream).await?;
        if packet.arg1 != self.local || (packet.arg0 != self.remote && !(packet.command == CLSE && packet.arg0 == 0)) {
            return Err(Error::Protocol);
        }
        Ok(packet)
    }
    async fn write(&mut self, data: &[u8]) -> Result<()> {
        for chunk in data.chunks(self.client.max_data) {
            self.send(WRTE, chunk.to_vec()).await?;
            loop {
                let packet = self.packet().await?;
                match packet.command {
                    OKAY => break,
                    WRTE => {
                        self.send(OKAY, Vec::new()).await?;
                        self.pending.push_back(packet.data);
                        if self.pending.iter().map(Vec::len).sum::<usize>() > MAX_OUTPUT {
                            return Err(Error::Limit);
                        }
                    }
                    _ => return Err(Error::Protocol),
                }
            }
        }
        Ok(())
    }
    async fn read(&mut self) -> Result<Option<Vec<u8>>> {
        if let Some(data) = self.pending.pop_front() {
            return Ok(Some(data));
        }
        if self.closed {
            return Ok(None);
        }
        let packet = self.packet().await?;
        match packet.command {
            WRTE => {
                self.send(OKAY, Vec::new()).await?;
                Ok(Some(packet.data))
            }
            CLSE => {
                self.send(CLSE, Vec::new()).await?;
                self.closed = true;
                self.client.active = false;
                Ok(None)
            }
            _ => Err(Error::Protocol),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renewed_adbd_certificate_preserves_public_key_pin() {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["adb".into()]).unwrap();
        let first = params.clone().self_signed(&key).unwrap();
        params.not_before = rcgen::date_time_ymd(2026, 1, 1);
        let renewed = params.clone().self_signed(&key).unwrap();
        assert_ne!(first.der(), renewed.der());
        assert_eq!(
            certificate_fingerprint(first.der()).unwrap(),
            certificate_fingerprint(renewed.der()).unwrap()
        );
        let other = params.self_signed(&rcgen::KeyPair::generate().unwrap()).unwrap();
        assert_ne!(
            certificate_fingerprint(first.der()).unwrap(),
            certificate_fingerprint(other.der()).unwrap()
        );
    }
    #[tokio::test]
    async fn unauthorized_client_tls_alert_is_authentication_failure() {
        use rustls::pki_types::PrivatePkcs8KeyDer;
        use std::sync::Arc;
        let cert = rcgen::generate_simple_self_signed(vec!["adb".into()]).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.cert.der().clone()).unwrap();
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .unwrap();
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                vec![cert.cert.der().clone()],
                PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()).into(),
            )
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            assert_eq!(Packet::read(&mut socket).await.unwrap().command, CNXN);
            Packet {
                command: STLS,
                arg0: 0x01000000,
                arg1: 0,
                data: vec![],
            }
            .write(&mut socket)
            .await
            .unwrap();
            assert_eq!(Packet::read(&mut socket).await.unwrap().command, STLS);
            assert!(
                tokio_rustls::TlsAcceptor::from(Arc::new(config))
                    .accept(socket)
                    .await
                    .is_err()
            );
        });
        let key = AdbKey::synthetic();
        assert!(matches!(
            Client::connect(address, &[key]).await,
            Err(Error::Authentication)
        ));
        server.await.unwrap();
    }
    #[tokio::test]
    async fn packet_roundtrip_and_magic_rejection() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        Packet {
            command: CNXN,
            arg0: 1,
            arg1: 2,
            data: b"hello".to_vec(),
        }
        .write(&mut a)
        .await
        .unwrap();
        let packet = Packet::read(&mut b).await.unwrap();
        assert_eq!(packet.data, b"hello");
        assert_eq!(packet.arg1, 2);
        a.write_all(&[0; 24]).await.unwrap();
        assert!(matches!(Packet::read(&mut b).await, Err(Error::Protocol)));
    }
    #[tokio::test]
    async fn oversized_packet_rejected_before_allocation() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let mut header = Vec::new();
        for word in [WRTE, 1, 2, (MAX_DATA + 1) as u32, 0, WRTE ^ u32::MAX] {
            header.extend_from_slice(&word.to_le_bytes());
        }
        a.write_all(&header).await.unwrap();
        assert!(matches!(Packet::read(&mut b).await, Err(Error::Limit)));
    }
}
