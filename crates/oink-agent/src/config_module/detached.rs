//! Local unplugged geometry. Owner preferences are stored separately; an entry
//! is removed only after its screen appears in the durably applied layout.
use crate::config::store::StoreError;
use base64::Engine;
use oink_protocol::v1::{self, dto, schema::WireDto};
use std::{io::Read, path::Path};

pub const DETACHED_FILE: &str = "detached-screens.json";
pub const MAX_DETACHED: usize = 8;
const MAX_BYTES: usize = 1024 * 1024;

/// Schema1 JSON envelope: screens [{placement_cbor:<base64 ScreenPlacement>,
/// last_seen_ms:u64}]. JSON entry/envelope extras and retained CBOR are preserved.
#[derive(Clone, Debug, PartialEq)]
pub struct DetachedScreen {
    pub placement: dto::ScreenPlacement,
    pub last_seen_ms: u64,
    extra: serde_json::Map<String, serde_json::Value>,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DetachedScreens {
    pub screens: Vec<DetachedScreen>,
    extra: serde_json::Map<String, serde_json::Value>,
}
impl DetachedScreens {
    pub fn remember(&mut self, placement: dto::ScreenPlacement, last_seen_ms: u64) -> Result<(), StoreError> {
        let id = placement.id()?;
        let extra = if let Some(index) = self.screens.iter().position(|s| {
            s.placement
                .id()
                .is_ok_and(|other| super::layout_wire::same_screen(&other, &id) == Ok(true))
        }) {
            self.screens.remove(index).extra
        } else {
            serde_json::Map::new()
        };
        self.screens.push(DetachedScreen {
            placement,
            last_seen_ms,
            extra,
        });
        self.screens.sort_by_key(|s| s.last_seen_ms);
        if self.screens.len() > MAX_DETACHED {
            self.screens.drain(..self.screens.len() - MAX_DETACHED);
        }
        Ok(())
    }
    pub fn find(&self, id: &dto::ScreenId) -> Option<&dto::ScreenPlacement> {
        self.screens
            .iter()
            .find(|s| {
                s.placement
                    .id()
                    .is_ok_and(|other| super::layout_wire::same_screen(&other, id) == Ok(true))
            })
            .map(|s| &s.placement)
    }
    pub fn applied(&mut self, ids: &[dto::ScreenId]) {
        self.screens.retain(|s| {
            s.placement.id().is_ok_and(|id| {
                !ids.iter()
                    .any(|other| super::layout_wire::same_screen(&id, other) == Ok(true))
            })
        });
    }
}
pub fn load(path: &Path) -> Result<DetachedScreens, StoreError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(DetachedScreens::default()),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BYTES {
        return Err(StoreError::LimitExceeded);
    }
    let mut extra: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&bytes).map_err(|_| StoreError::Invalid)?;
    if extra.remove("schema").and_then(|v| v.as_u64()) != Some(1) {
        return Err(StoreError::UnsupportedSchema);
    }
    let entries = extra.remove("screens").ok_or(StoreError::Invalid)?;
    let entries = entries.as_array().ok_or(StoreError::Invalid)?;
    if entries.len() > MAX_DETACHED {
        return Err(StoreError::LimitExceeded);
    }
    let mut screens = Vec::new();
    let mut ids = Vec::new();
    for entry in entries {
        let mut extra = entry.as_object().cloned().ok_or(StoreError::Invalid)?;
        let last_seen_ms = extra
            .remove("last_seen_ms")
            .and_then(|v| v.as_u64())
            .ok_or(StoreError::Invalid)?;
        let encoded = extra.remove("placement_cbor").ok_or(StoreError::Invalid)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.as_str().ok_or(StoreError::Invalid)?)
            .map_err(|_| StoreError::Invalid)?;
        let placement = dto::ScreenPlacement::from_raw(v1::cbor::decode_map(&bytes, v1::limits::CONFIG_STATE)?)?;
        let id = placement.id()?;
        if ids
            .iter()
            .any(|other| super::layout_wire::same_screen(&id, other) == Ok(true))
        {
            return Err(StoreError::Invalid);
        }
        ids.push(id);
        screens.push(DetachedScreen {
            placement,
            last_seen_ms,
            extra,
        });
    }
    Ok(DetachedScreens { screens, extra })
}
pub fn save(path: &Path, state: &DetachedScreens) -> Result<(), StoreError> {
    if state.screens.len() > MAX_DETACHED {
        return Err(StoreError::LimitExceeded);
    }
    let mut extra = state.extra.clone();
    extra.insert("schema".into(), 1.into());
    let mut entries = Vec::new();
    for screen in &state.screens {
        let mut extra = screen.extra.clone();
        extra.insert("last_seen_ms".into(), screen.last_seen_ms.into());
        extra.insert(
            "placement_cbor".into(),
            base64::engine::general_purpose::STANDARD
                .encode(screen.placement.encode(v1::limits::CONFIG_STATE)?)
                .into(),
        );
        entries.push(serde_json::Value::Object(extra));
    }
    extra.insert("screens".into(), serde_json::Value::Array(entries));
    let bytes = serde_json::to_vec_pretty(&extra).map_err(|_| StoreError::Invalid)?;
    if bytes.len() > MAX_BYTES {
        return Err(StoreError::LimitExceeded);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    oink_foundation::storage::atomic_write(path, &bytes)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use oink_protocol::v1::{RawValue, schema::WireValue};
    fn screen(n: u32) -> dto::ScreenPlacement {
        let rect = RawValue::Map(
            [
                (1, 0.0f64.write()),
                (2, 0.0f64.write()),
                (3, 1.0f64.write()),
                (4, 1.0f64.write()),
            ]
            .into(),
        );
        dto::ScreenPlacement::from_raw(
            [
                (
                    1,
                    RawValue::Map(
                        [
                            (
                                1,
                                crate::config::store::wire_device(oink_foundation::DeviceId([1; 32])).write(),
                            ),
                            (2, RawValue::Text(n.to_string())),
                        ]
                        .into(),
                    ),
                ),
                (2, RawValue::Text("Display".into())),
                (3, rect.clone()),
                (4, rect),
                (5, 0u32.write()),
                (9, true.write()),
                (10, false.write()),
                (100, RawValue::Bytes(vec![3, 4])),
            ]
            .into(),
        )
        .unwrap()
    }
    #[test]
    fn bounded_memory_keeps_unknown_geometry_until_applied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DETACHED_FILE);
        let mut state = DetachedScreens::default();
        state.extra.insert("future".into(), true.into());
        for n in 0..10 {
            state.remember(screen(n), u64::from(n)).unwrap();
        }
        assert_eq!(state.screens.len(), 8);
        assert!(state.find(&screen(0).id().unwrap()).is_none());
        state.screens[0].extra.insert("future".into(), 1.into());
        save(&path, &state).unwrap();
        assert_eq!(load(&path).unwrap(), state);
        assert!(state.find(&screen(2).id().unwrap()).unwrap().raw().contains_key(&100));
        state.applied(&[screen(2).id().unwrap()]);
        assert!(state.find(&screen(2).id().unwrap()).is_none());
        assert!(state.find(&screen(3).id().unwrap()).is_some());
        std::fs::write(&path, b"{\"format\":1,\"screens\":[]}").unwrap();
        assert!(matches!(load(&path), Err(StoreError::UnsupportedSchema)));
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"format\":1,\"screens\":[]}");
    }
    #[test]
    fn matching_uses_stable_identity_and_retains_identity_extensions() {
        let mut placement = screen(1).into_raw();
        let RawValue::Map(id) = placement.get_mut(&1).unwrap() else {
            unreachable!()
        };
        id.insert(99, true.write());
        let placement = dto::ScreenPlacement::from_raw(placement).unwrap();
        let plain = screen(1).id().unwrap();
        let mut state = DetachedScreens::default();
        state.remember(placement.clone(), 10).unwrap();
        assert_eq!(state.find(&plain), Some(&placement));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DETACHED_FILE);
        save(&path, &state).unwrap();
        let mut loaded = load(&path).unwrap();
        assert_eq!(loaded.find(&plain), Some(&placement));
        loaded.applied(&[plain]);
        assert!(loaded.screens.is_empty());
    }
}
