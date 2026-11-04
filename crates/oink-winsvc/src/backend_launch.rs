//! Local session-pipe protocol for the fixed backend launch recipe and the
//! machine-level pre-login state. Frames use an Oink-specific version marker,
//! a command byte, little-endian counts, and exact UTF-16 code units. No
//! executable path or arguments are accepted. This is local service IPC, not a
//! network message.
use crate::prelogin_policy::{MAX_SNAPSHOT_BYTES, OwnerView, SnapshotFile};
use std::io;

pub(crate) const MAX_ENVIRONMENT: usize = 65536;
const MAX_DIRECTORY: usize = 32768;
const MAX_ERROR: usize = 1024;
const MAX_LAUNCH_FRAME: usize = 13 + 2 * (MAX_ENVIRONMENT + MAX_DIRECTORY);
/// Kind byte, file count, and per file an id byte plus a u32 length.
const MAX_SNAPSHOT_FRAME: usize = 6 + SnapshotFile::ALL.len() * 5 + MAX_SNAPSHOT_BYTES;
pub(crate) const MAX_FRAME: usize = if MAX_SNAPSHOT_FRAME > MAX_LAUNCH_FRAME {
    MAX_SNAPSHOT_FRAME
} else {
    MAX_LAUNCH_FRAME
};
const MAGIC: &[u8; 4] = b"OBL1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BackendLaunch {
    Start {
        environment: Vec<u16>,
        directory: Vec<u16>,
    },
    Reopen {
        environment: Vec<u16>,
        directory: Vec<u16>,
    },
    Started {
        pid: u32,
        /// Query/synchronize-only handle duplicated into the authenticated caller.
        process_handle: u64,
    },
    Failed {
        error: String,
    },
    /// No backend was created. Retry only after the pinned old network process
    /// exits; this handle grants synchronization only.
    PreloginStopping {
        process_handle: u64,
    },
    /// Read the machine-level pre-login state for the calling user.
    PreloginStatus,
    /// Make the calling session's user the machine identity. Requires an
    /// elevated caller (the administrator approval path).
    PreloginClaim,
    /// Disable the pre-login backend if the caller owns it; otherwise no effect.
    PreloginRelease,
    /// The owner's identity and routing state, written to the protected store.
    PreloginSnapshot {
        files: Vec<(SnapshotFile, Vec<u8>)>,
    },
    /// Reply to every pre-login request.
    PreloginState {
        enabled: bool,
        owner: OwnerView,
        snapshot: bool,
    },
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
            Self::Started { pid, process_handle } => {
                out.push(2);
                out.extend_from_slice(&pid.to_le_bytes());
                out.extend_from_slice(&process_handle.to_le_bytes());
            }
            Self::Failed { error } => {
                if error.len() > MAX_ERROR {
                    return Err(invalid());
                }
                out.push(3);
                out.extend_from_slice(&(error.len() as u32).to_le_bytes());
                out.extend_from_slice(error.as_bytes());
            }
            Self::PreloginStopping { process_handle } => {
                if *process_handle == 0 {
                    return Err(invalid());
                }
                out.push(9);
                out.extend_from_slice(&process_handle.to_le_bytes());
            }
            Self::PreloginStatus => out.push(4),
            Self::PreloginClaim => out.push(5),
            Self::PreloginRelease => out.push(6),
            Self::PreloginSnapshot { files } => {
                crate::prelogin_policy::validate_snapshot(files).map_err(|_| invalid())?;
                out.push(7);
                out.push(files.len() as u8);
                for (file, bytes) in files {
                    out.push(*file as u8);
                    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                    out.extend_from_slice(bytes);
                }
            }
            Self::PreloginState {
                enabled,
                owner,
                snapshot,
            } => {
                out.push(8);
                out.push(u8::from(*enabled));
                out.push(*owner as u8);
                out.push(u8::from(*snapshot));
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
            2 if bytes.len() == 17 => Ok(Self::Started {
                pid: number(5)?,
                process_handle: u64::from_le_bytes(bytes[9..17].try_into().map_err(|_| invalid())?),
            }),
            3 => {
                let len = number(5)? as usize;
                if len > MAX_ERROR || bytes.len() != 9 + len {
                    return Err(invalid());
                }
                Ok(Self::Failed {
                    error: std::str::from_utf8(&bytes[9..]).map_err(|_| invalid())?.to_owned(),
                })
            }
            4 if bytes.len() == 5 => Ok(Self::PreloginStatus),
            9 if bytes.len() == 13 => {
                let process_handle = u64::from_le_bytes(bytes[5..13].try_into().map_err(|_| invalid())?);
                if process_handle == 0 {
                    return Err(invalid());
                }
                Ok(Self::PreloginStopping { process_handle })
            }
            5 if bytes.len() == 5 => Ok(Self::PreloginClaim),
            6 if bytes.len() == 5 => Ok(Self::PreloginRelease),
            7 if bytes.len() >= 6 => {
                let count = usize::from(bytes[5]);
                let mut offset = 6;
                let mut files = Vec::with_capacity(count.min(SnapshotFile::ALL.len()));
                for _ in 0..count {
                    let file = SnapshotFile::from_id(*bytes.get(offset).ok_or_else(invalid)?).ok_or_else(invalid)?;
                    let len = number(offset + 1)? as usize;
                    offset += 5;
                    // Length is bounded by the remaining frame before any copy.
                    if len > file.max_bytes() || len > bytes.len() - offset {
                        return Err(invalid());
                    }
                    files.push((file, bytes[offset..offset + len].to_vec()));
                    offset += len;
                }
                if offset != bytes.len() {
                    return Err(invalid());
                }
                crate::prelogin_policy::validate_snapshot(&files).map_err(|_| invalid())?;
                Ok(Self::PreloginSnapshot { files })
            }
            8 if bytes.len() == 8 => {
                let flag = |b: u8| match b {
                    0 => Ok(false),
                    1 => Ok(true),
                    _ => Err(invalid()),
                };
                Ok(Self::PreloginState {
                    enabled: flag(bytes[5])?,
                    owner: OwnerView::from_id(bytes[6]).ok_or_else(invalid)?,
                    snapshot: flag(bytes[7])?,
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
            BackendLaunch::Started {
                pid: 17,
                process_handle: 0x12345678,
            },
            BackendLaunch::Failed { error: "denied".into() },
            BackendLaunch::PreloginStopping { process_handle: 123 },
            BackendLaunch::PreloginStatus,
            BackendLaunch::PreloginClaim,
            BackendLaunch::PreloginRelease,
            BackendLaunch::PreloginSnapshot {
                files: vec![
                    (SnapshotFile::DeviceKey, vec![7; 32]),
                    (SnapshotFile::GroupState, b"{}".to_vec()),
                    (SnapshotFile::Settings, Vec::new()),
                ],
            },
            BackendLaunch::PreloginState {
                enabled: true,
                owner: OwnerView::Other,
                snapshot: false,
            },
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
            unknown[4] = u8::MAX;
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

    #[test]
    fn snapshot_frames_reject_unknown_duplicate_missing_or_oversized_files() {
        let frame = |files: &[(u8, Vec<u8>)]| {
            let mut out = b"OBL1\x07".to_vec();
            out.push(files.len() as u8);
            for (id, bytes) in files {
                out.push(*id);
                out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                out.extend_from_slice(bytes);
            }
            out
        };
        let key = (SnapshotFile::DeviceKey as u8, vec![1; 32]);
        let group = (SnapshotFile::GroupState as u8, b"{}".to_vec());
        assert!(BackendLaunch::decode(&frame(&[key.clone(), group.clone()])).is_ok());
        // Unknown file id, duplicate file, missing required file, bad key length.
        assert!(BackendLaunch::decode(&frame(&[key.clone(), group.clone(), (99, vec![])])).is_err());
        assert!(BackendLaunch::decode(&frame(&[key.clone(), group.clone(), group.clone()])).is_err());
        assert!(BackendLaunch::decode(&frame(std::slice::from_ref(&key))).is_err());
        assert!(BackendLaunch::decode(&frame(&[(SnapshotFile::DeviceKey as u8, vec![1; 31]), group.clone()])).is_err());
        let oversized = (
            SnapshotFile::GroupState as u8,
            vec![0; SnapshotFile::GroupState.max_bytes() + 1],
        );
        assert!(BackendLaunch::decode(&frame(&[key.clone(), oversized])).is_err());
        // A declared length larger than the frame is rejected before any copy.
        let mut lying = frame(&[key, group]);
        let at = 6 + 1;
        lying[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(BackendLaunch::decode(&lying).is_err());
        for bad in [
            [b'O', b'B', b'L', b'1', 8, 2, 0, 0],
            [b'O', b'B', b'L', b'1', 8, 0, 3, 0],
        ] {
            assert!(BackendLaunch::decode(&bad).is_err());
        }
    }
}
