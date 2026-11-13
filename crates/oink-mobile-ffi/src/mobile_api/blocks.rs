//! FFI projection of the durable local pairing block list.

use super::*;
use oink_mobile_core::BlockError;

impl MobileSession {
    pub(super) fn blocked_view(&self) -> BlockedDevicesSnapshot {
        let (revision, entries) = self.network.blocked_devices();
        BlockedDevicesSnapshot {
            stream: self.stream.clone(),
            revision,
            entries: entries
                .into_iter()
                .map(|entry| BlockedDevice {
                    device: entry.device.to_hex(),
                    fingerprint: entry.device.short(),
                    name: entry.name,
                    os: entry.os,
                    blocked_at_ms: entry.blocked_at_ms,
                })
                .collect(),
        }
    }

    /// Display hint for a new block entry: a pairing request, nearby row or
    /// group member, in that order. The entry itself never grants anything.
    fn block_hint(&self, device: DeviceId) -> (Option<String>, Option<String>) {
        if let Some(advert) = self.network.authenticated_advert(device) {
            return (Some(advert.name), Some(advert.os));
        }
        let hex = device.to_hex();
        if let Some(row) = self.nearby_view().rows.into_iter().find(|row| row.device == hex) {
            return (Some(row.name), Some(row.os));
        }
        self.network
            .members()
            .into_iter()
            .find(|member| member.device == device)
            .map_or((None, None), |member| (member.name, member.os))
    }

    pub(super) fn edit_block(&self, request: String, device: &str, install: bool) -> Result<BlockReply, MobileError> {
        let Some(peer) = parse_device_id(device) else {
            return Err(MobileError::Command {
                code: UserFacingCode::InvalidValue,
                detail: "invalid device ID".into(),
            });
        };
        let result = if install {
            let (name, os) = self.block_hint(peer);
            self.network.block(peer, name, os)
        } else {
            self.network.unblock(peer)
        };
        let changed = match result {
            Ok(changed) => changed,
            Err(error) => {
                return Ok(BlockReply::Error(match error {
                    BlockError::SelfDevice => BlockListError { code: 0, limit: None },
                    BlockError::Persistence(_) => BlockListError { code: 1, limit: None },
                    BlockError::LimitExceeded => BlockListError {
                        code: 2,
                        limit: Some(oink_mobile_core::MAX_BLOCKS as u32),
                    },
                }));
            }
        };
        let snapshot = self.blocked_view();
        if changed {
            self.refresh_presence();
            self.emit_event(MobileEvent::BlockedDevices {
                snapshot: snapshot.clone(),
            });
        }
        Ok(BlockReply::Edited(BlockEdited {
            operation_id: request,
            changed,
            snapshot,
        }))
    }
}

fn parse_device_id(value: &str) -> Option<DeviceId> {
    if value.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(value.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(DeviceId(bytes))
}
