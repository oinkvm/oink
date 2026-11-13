use crate::{AdbKey, Error, Result, spake::Spake};
use aes_gcm::{Aes128Gcm, KeyInit, Nonce, aead::Aead};
use hkdf::Hkdf;
use sha2::Sha256;
use std::net::SocketAddr;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};
use zeroize::Zeroizing;

#[derive(Clone, Debug)]
pub struct PairingPeer {
    pub guid: String,
}

pub async fn pair(address: SocketAddr, code: &str, key: &AdbKey) -> Result<PairingPeer> {
    if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::Authentication);
    }
    let socket = TcpStream::connect(address).await?;
    socket.set_nodelay(true)?;
    let mut tls = crate::tls::connect(socket, std::slice::from_ref(key)).await?;
    // AOSP passes sizeof(kExportedKeyLabel), including its terminating NUL.
    let exporter = tls
        .get_ref()
        .1
        .export_keying_material([0; 64], b"adb-label\0", None)
        .map_err(|_| Error::Authentication)?;
    let mut password = Zeroizing::new(code.as_bytes().to_vec());
    password.extend_from_slice(&exporter);
    let spake = Spake::new(&password);
    write_packet(&mut tls, 0, &spake.message()).await?;
    let peer = read_packet(&mut tls, 0).await?;
    let material = spake.finish(peer.try_into().map_err(|_| Error::Protocol)?)?;
    let mut cipher_key = Zeroizing::new([0; 16]);
    Hkdf::<Sha256>::new(None, &*material)
        .expand(b"adb pairing_auth aes-128-gcm key", &mut *cipher_key)
        .map_err(|_| Error::Authentication)?;
    let cipher = Aes128Gcm::new_from_slice(&*cipher_key).map_err(|_| Error::Authentication)?;
    let mut info = vec![0; 8192];
    let public = key.public_key();
    if public.len() + 2 > info.len() {
        return Err(Error::Limit);
    }
    info[1..1 + public.len()].copy_from_slice(public.as_bytes());
    // Pairing exchanges one message per direction, each with sequence zero.
    let encrypted = cipher
        .encrypt(Nonce::from_slice(&[0; 12]), info.as_ref())
        .map_err(|_| Error::Authentication)?;
    write_packet(&mut tls, 1, &encrypted).await?;
    let encrypted = read_packet(&mut tls, 1).await?;
    let info = cipher
        .decrypt(Nonce::from_slice(&[0; 12]), encrypted.as_ref())
        .map_err(|_| Error::Authentication)?;
    if info.len() != 8192 || info[0] != 1 {
        return Err(Error::Protocol);
    }
    let end = info[1..].iter().position(|byte| *byte == 0).ok_or(Error::Protocol)? + 1;
    let guid = std::str::from_utf8(&info[1..end])
        .map_err(|_| Error::Protocol)?
        .to_owned();
    if guid.is_empty() {
        return Err(Error::Protocol);
    }
    tls.shutdown().await?;
    Ok(PairingPeer { guid })
}
async fn write_packet<S: AsyncWrite + Unpin>(stream: &mut S, kind: u8, data: &[u8]) -> Result<()> {
    if data.is_empty() || data.len() > 16384 {
        return Err(Error::Limit);
    }
    stream.write_all(&[1, kind]).await?;
    stream.write_all(&(data.len() as u32).to_be_bytes()).await?;
    stream.write_all(data).await?;
    stream.flush().await?;
    Ok(())
}
async fn read_packet<S: AsyncRead + Unpin>(stream: &mut S, kind: u8) -> Result<Vec<u8>> {
    let mut header = [0; 6];
    stream.read_exact(&mut header).await?;
    if header[0] != 1 || header[1] != kind {
        return Err(Error::Protocol);
    }
    let length = u32::from_be_bytes(header[2..].try_into().expect("four bytes")) as usize;
    if length == 0 || length > 16384 {
        return Err(Error::Limit);
    }
    let mut data = vec![0; length];
    stream.read_exact(&mut data).await?;
    Ok(data)
}
