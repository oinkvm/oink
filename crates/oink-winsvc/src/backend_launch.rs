//! Local session-pipe protocol for the fixed backend launch recipe.
//! Frames use an Oink-specific version marker, a command byte, little-endian
//! counts, and exact UTF-16 code units. No executable path or arguments are accepted.
use std::io;

pub(crate) const MAX_ENVIRONMENT: usize = 65536;
const MAX_DIRECTORY: usize = 32768;
const MAX_ERROR: usize = 1024;
pub(crate) const MAX_FRAME: usize = 13 + 2 * (MAX_ENVIRONMENT + MAX_DIRECTORY);
const MAGIC: &[u8; 4] = b"OBL1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BackendLaunch {
    Start { environment: Vec<u16>, directory: Vec<u16> },
    Reopen { environment: Vec<u16>, directory: Vec<u16> },
    Started { pid: u32 },
    Failed { error: String },
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid backend launch frame")
}
impl BackendLaunch {
    pub(crate) fn encode(&self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        match self {
            Self::Start { environment, directory } | Self::Reopen { environment, directory } => {
                if environment.len() > MAX_ENVIRONMENT || directory.len() > MAX_DIRECTORY {
                    return Err(invalid());
                }
                out.push(if matches!(self, Self::Start { .. }) { 0 } else { 1 });
                out.extend_from_slice(&(environment.len() as u32).to_le_bytes());
                out.extend_from_slice(&(directory.len() as u32).to_le_bytes());
                for unit in environment.iter().chain(directory) {
                    out.extend_from_slice(&unit.to_le_bytes());
                }
            }
            Self::Started { pid } => {
                out.push(2);
                out.extend_from_slice(&pid.to_le_bytes());
            }
            Self::Failed { error } => {
                if error.len() > MAX_ERROR {
                    return Err(invalid());
                }
                out.push(3);
                out.extend_from_slice(&(error.len() as u32).to_le_bytes());
                out.extend_from_slice(error.as_bytes());
            }
        }
        Ok(out)
    }
    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_FRAME || bytes.len() < 5 || &bytes[..4] != MAGIC {
            return Err(invalid());
        }
        let number = |offset: usize| -> io::Result<u32> {
            Ok(u32::from_le_bytes(
                bytes
                    .get(offset..offset + 4)
                    .ok_or_else(invalid)?
                    .try_into()
                    .map_err(|_| invalid())?,
            ))
        };
        match bytes[4] {
            kind @ (0 | 1) => {
                let environment_len = number(5)? as usize;
                let directory_len = number(9)? as usize;
                // Bounds and the exact frame length are checked before allocating vectors.
                if environment_len > MAX_ENVIRONMENT
                    || directory_len > MAX_DIRECTORY
                    || bytes.len() != 13 + 2 * (environment_len + directory_len)
                {
                    return Err(invalid());
                }
                let split = 13 + environment_len * 2;
                let read = |data: &[u8]| {
                    data.as_chunks::<2>()
                        .0
                        .iter()
                        .copied()
                        .map(u16::from_le_bytes)
                        .collect()
                };
                let environment = read(&bytes[13..split]);
                let directory = read(&bytes[split..]);
                Ok(if kind == 0 {
                    Self::Start { environment, directory }
                } else {
                    Self::Reopen { environment, directory }
                })
            }
            2 if bytes.len() == 9 => Ok(Self::Started { pid: number(5)? }),
            3 => {
                let len = number(5)? as usize;
                if len > MAX_ERROR || bytes.len() != 9 + len {
                    return Err(invalid());
                }
                Ok(Self::Failed {
                    error: std::str::from_utf8(&bytes[9..]).map_err(|_| invalid())?.to_owned(),
                })
            }
            _ => Err(invalid()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_recipe_codec_preserves_utf16_and_rejects_nonframes() {
        let requests = [
            BackendLaunch::Start {
                environment: vec![0xd800, 0, 0],
                directory: vec![0xdfff, 0],
            },
            BackendLaunch::Reopen {
                environment: vec![1, 0, 0],
                directory: vec![2, 0],
            },
            BackendLaunch::Started { pid: 17 },
            BackendLaunch::Failed { error: "denied".into() },
        ];
        for request in requests {
            let frame = request.encode().unwrap();
            assert_eq!(BackendLaunch::decode(&frame).unwrap(), request);
            for end in 0..frame.len() {
                assert!(BackendLaunch::decode(&frame[..end]).is_err());
            }
            let mut extra = frame.clone();
            extra.push(0);
            assert!(BackendLaunch::decode(&extra).is_err());
            let mut unknown = frame;
            unknown[4] = 4;
            assert!(BackendLaunch::decode(&unknown).is_err());
        }
        assert!(BackendLaunch::decode(&[0; 20]).is_err());
        let mut huge = b"OBL1\0".to_vec();
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        huge.extend_from_slice(&0u32.to_le_bytes());
        assert!(BackendLaunch::decode(&huge).is_err());
        assert!(
            BackendLaunch::Failed {
                error: "x".repeat(MAX_ERROR + 1)
            }
            .encode()
            .is_err()
        );
        assert!(
            BackendLaunch::Start {
                environment: vec![0; MAX_ENVIRONMENT + 1],
                directory: vec![]
            }
            .encode()
            .is_err()
        );
    }
}
