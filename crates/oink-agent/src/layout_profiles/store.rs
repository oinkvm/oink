//! Atomic catalog and operation receipt envelope, isolated from frozen /1 storage.
use super::{
    model::{Catalog, MAX_BYTES},
    types::*,
    wire,
};
use crate::api::GroupKey;
use base64::Engine;
use oink_protocol::v1::{dto::LayoutProfileCatalogV2, schema::WireDto};
use std::{
    io::Read,
    path::{Path, PathBuf},
};
const MAX_ENVELOPE: usize = MAX_BYTES * 6;
fn path(root: &Path, group: GroupKey) -> PathBuf {
    root.join("layout-profiles-v2").join(format!(
        "{}.json",
        group.0.0.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}
fn failure(e: impl std::fmt::Display) -> ProfileError {
    ProfileError::new(ErrorCode::PersistenceFailed, format!("Profile storage: {e}"))
}
/// Local feature activation boundary, retained across group changes. The first
/// profile build lacked this marker; directory birth time preserves its boundary
/// even though catalog files are atomically replaced on every save.
pub(super) fn activation_ms(root: &Path) -> Result<u64> {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Activation {
        schema: u32,
        started_ms: u64,
    }
    let directory = root.join("layout-profiles-v2");
    let path = directory.join("activation.json");
    match std::fs::File::open(&path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(1025).read_to_end(&mut bytes).map_err(failure)?;
            if bytes.len() > 1024 {
                return Err(failure("activation marker size limit"));
            }
            let activation: Activation = serde_json::from_slice(&bytes).map_err(failure)?;
            if activation.schema != 1 {
                return Err(failure("unknown activation schema"));
            }
            return Ok(activation.started_ms);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(failure(e)),
    }
    let started_ms = match std::fs::metadata(&directory) {
        Ok(metadata) => metadata
            .created()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|age| u64::try_from(age.as_millis()).ok())
            // Unknown provenance is conservative: never waive a missing import.
            .unwrap_or(0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => crate::layout_sync::now_ms(),
        Err(e) => return Err(failure(e)),
    };
    std::fs::create_dir_all(&directory).map_err(failure)?;
    let bytes = serde_json::to_vec(&Activation { schema: 1, started_ms }).map_err(failure)?;
    oink_foundation::storage::atomic_write(&path, &bytes).map_err(failure)?;
    Ok(started_ms)
}

pub fn load(root: &Path, group: GroupKey) -> Result<Option<Catalog>> {
    let file = match std::fs::File::open(path(root, group)) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(failure(e)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_ENVELOPE as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(failure)?;
    if bytes.len() > MAX_ENVELOPE {
        return Err(failure("catalog envelope too large"));
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(failure)?;
    if value["schema"] != 1 {
        return Err(failure("unknown catalog schema"));
    }
    let encoded = value["catalog_cbor"]
        .as_str()
        .ok_or_else(|| failure("missing catalog"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(failure)?;
    let catalog = wire::read_catalog(LayoutProfileCatalogV2::decode(&bytes, MAX_BYTES).map_err(failure)?)?;
    if catalog.group != group {
        return Err(failure("catalog group mismatch"));
    }
    Ok(Some(catalog))
}
pub fn save(root: &Path, catalog: &Catalog) -> Result<()> {
    let operations = load_operations(root, catalog.group)?;
    save_with_operations(root, catalog, &operations)
}
pub fn save_with_operations(root: &Path, catalog: &Catalog, operations: &Operations) -> Result<()> {
    catalog.validate()?;
    let bytes = wire::catalog(catalog)
        .map_err(failure)?
        .encode(MAX_BYTES)
        .map_err(failure)?;
    let envelope = serde_json::json!({"schema":1,"catalog_cbor":base64::engine::general_purpose::STANDARD.encode(bytes), "operations": operations});
    let bytes = serde_json::to_vec(&envelope).map_err(failure)?;
    if bytes.len() > MAX_ENVELOPE {
        return Err(failure("catalog/operation envelope size limit"));
    }
    let path = path(root, catalog.group);
    std::fs::create_dir_all(path.parent().expect("catalog parent")).map_err(failure)?;
    oink_foundation::storage::atomic_write(&path, &bytes).map_err(failure)
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
pub struct Operations {
    pub operations: std::collections::VecDeque<LayoutOperation>,
    pub digests: std::collections::BTreeMap<String, String>,
    pub unknown: Vec<(String, crate::api::DeviceKey, Vec<u8>)>,
}
pub fn load_operations(root: &Path, group: GroupKey) -> Result<Operations> {
    let file = match std::fs::File::open(path(root, group)) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Operations::default()),
        Err(e) => return Err(failure(e)),
    };
    let mut bytes = Vec::new();
    file.take((MAX_ENVELOPE + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(failure)?;
    if bytes.len() > MAX_ENVELOPE {
        return Err(failure("catalog/operation envelope size limit"));
    }
    let envelope: serde_json::Value = serde_json::from_slice(&bytes).map_err(failure)?;
    let value: Operations = match envelope.get("operations") {
        Some(value) => serde_json::from_value(value.clone()).map_err(failure)?,
        None => Operations::default(),
    };
    if value.operations.len() > 256 || value.unknown.len() > 16 {
        return Err(failure("operation receipt count limit"));
    }
    Ok(value)
}
pub fn save_operations(root: &Path, group: GroupKey, value: &Operations) -> Result<()> {
    let catalog = load(root, group)?.ok_or_else(|| failure("missing catalog for receipt"))?;
    save_with_operations(root, &catalog, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_bootstraps_original_store_birth_and_survives_catalog_replacement() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("layout-profiles-v2");
        std::fs::create_dir(&directory).unwrap();
        let birth_ms = std::fs::metadata(&directory)
            .unwrap()
            .created()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert_eq!(activation_ms(root.path()).unwrap(), birth_ms);
        let catalog = directory.join("catalog.json");
        oink_foundation::storage::atomic_write(&catalog, b"first").unwrap();
        oink_foundation::storage::atomic_write(&catalog, b"replacement").unwrap();
        assert_eq!(activation_ms(root.path()).unwrap(), birth_ms);
        // First-build upgrade and subsequent launches use the same boundary.
        std::fs::remove_file(directory.join("activation.json")).unwrap();
        assert_eq!(activation_ms(root.path()).unwrap(), birth_ms);
        std::fs::write(directory.join("activation.json"), b"broken").unwrap();
        assert_eq!(
            activation_ms(root.path()).unwrap_err().code,
            ErrorCode::PersistenceFailed
        );
    }
}
