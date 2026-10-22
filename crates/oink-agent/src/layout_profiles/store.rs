//! Atomic catalog and operation receipt envelope, isolated from frozen /1 storage.
use super::{
    model::{Catalog, MAX_BYTES},
    types::*,
    wire,
};
use crate::api::GroupKey;
use base64::Engine;
use oink_protocol::v1::{dto::LayoutProfileCatalogV3, schema::WireDto};
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
pub fn load(root: &Path, group: GroupKey) -> Result<Option<Catalog>> {
    read(root, group)
}

fn read(root: &Path, group: GroupKey) -> Result<Option<Catalog>> {
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
    let encoded = LayoutProfileCatalogV3::decode(&bytes, MAX_BYTES).map_err(failure)?;
    let catalog = wire::read_catalog(encoded)?;
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
