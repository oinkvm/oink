//! Retaining register operations. Roster signature/permission checks and atomic
//! persistence belong to the config/network owners; these helpers grant no trust.
use super::{
    ErrorCode, RawMap, RawValue, Result,
    dto::*,
    schema::{FixedBytes, WireDto, WireValue},
};
use sha2::{Digest as _, Sha256};

pub fn sha256(bytes: &[u8]) -> Hash32 {
    FixedBytes(Sha256::digest(bytes).into())
}
pub fn register_digest(raw: &RawMap) -> Result<Hash32> {
    let mut body = raw.clone();
    body.remove(&6);
    body.entry(5).or_insert(RawValue::Bool(false));
    Ok(sha256(&RawValue::Map(body).encode(super::limits::CONFIG_STATE)?))
}
pub fn verify_register(register: &Register) -> Result<()> {
    if register_digest(register.raw())? != register.digest()? {
        return Err(ErrorCode::StateConflict.into());
    }
    Ok(())
}
pub fn version_cmp(a: &ConfigVersion, b: &ConfigVersion) -> Result<std::cmp::Ordering> {
    Ok((a.counter()?, a.writer()?.ordering_key()?).cmp(&(b.counter()?, b.writer()?.ordering_key()?)))
}
/// Merge the whole winning register, including every unknown envelope/value key.
/// The caller has already authenticated/authorised the layout writer.
pub fn merge_register(current: &Register, incoming: &Register) -> Result<Register> {
    verify_register(current)?;
    verify_register(incoming)?;
    if current.kind()? != incoming.kind()? {
        return Err(ErrorCode::StateConflict.into());
    }
    match version_cmp(&current.version()?, &incoming.version()?)? {
        std::cmp::Ordering::Less => Ok(incoming.clone()),
        std::cmp::Ordering::Greater => Ok(current.clone()),
        std::cmp::Ordering::Equal => {
            if current.digest()? == incoming.digest()? {
                Ok(current.clone())
            } else {
                Err(ErrorCode::StateConflict.into())
            }
        }
    }
}
/// Edit only declared paths in the latest tree. The expected version fences a
/// stale UI projection; counters never wrap. Owner preferences never enter this
/// multi-writer register path. The caller supplies its authenticated identity.
pub fn edit_register(
    current: &Register,
    expected: &ConfigVersion,
    writer: DeviceId,
    patch: impl FnOnce(&mut RawMap) -> Result<()>,
) -> Result<Register> {
    verify_register(current)?;
    if &current.version()? != expected {
        return Err(ErrorCode::StateConflict.into());
    }
    let counter = expected.counter()?.checked_add(1).ok_or(ErrorCode::LimitExceeded)?;
    let mut value = current.value()?;
    patch(&mut value)?;
    let mut raw = current.raw().clone();
    let mut version = expected.raw().clone();
    version.insert(1, counter.write());
    version.insert(3, writer.write());
    raw.insert(3, RawValue::Map(version));
    raw.insert(4, RawValue::Map(value));
    raw.entry(5).or_insert(false.write());
    raw.insert(6, register_digest(&raw)?.write());
    RawValue::Map(raw.clone()).encode(super::limits::CONFIG_STATE)?;
    Register::from_raw(raw)
}
/// Patch a field path without reconstructing any intervening known map.
/// Keyed arrays are edited by their owner after matching stable object identity.
pub fn patch_path(root: &mut RawMap, path: &[u32], value: Option<RawValue>) -> Result<()> {
    let Some((&key, rest)) = path.split_first() else {
        return Err(ErrorCode::MalformedFrame.into());
    };
    if key == 0 || key == u32::MAX {
        return Err(ErrorCode::MalformedFrame.into());
    }
    if rest.is_empty() {
        if let Some(value) = value {
            root.insert(key, value);
        } else {
            root.remove(&key);
        }
        return Ok(());
    }
    let Some(RawValue::Map(child)) = root.get_mut(&key) else {
        return Err(ErrorCode::StateConflict.into());
    };
    patch_path(child, rest, value)
}
/// Unversioned envelope extras use identical/disjoint union, never LWW. Return a
/// candidate only on success, so a conflict cannot partly mutate accepted state.
pub fn merge_extensions(current: &RawMap, incoming: &RawMap, known: &[u32]) -> Result<RawMap> {
    let mut merged = current.clone();
    for (&key, value) in incoming {
        if known.contains(&key) {
            continue;
        }
        if let Some(previous) = merged.get(&key) {
            if previous.encode(super::limits::CONFIG_STATE)? != value.encode(super::limits::CONFIG_STATE)? {
                return Err(ErrorCode::StateConflict.into());
            }
        } else {
            merged.insert(key, value.clone());
        }
    }
    Ok(merged)
}
pub fn record_id(record: &SignedRecord) -> Result<Hash32> {
    let mut hash = Sha256::new();
    hash.update(super::labels::ROSTER_RECORD);
    hash.update(record.encode(super::limits::CONFIG_STATE)?);
    Ok(FixedBytes(hash.finalize().into()))
}
pub fn record_signature_input(record: &SignedRecord) -> Result<Vec<u8>> {
    let mut bytes = super::labels::ROSTER_SIGNATURE.to_vec();
    bytes.extend(RawValue::Map(record.body()?).encode(super::limits::CONFIG_STATE)?);
    Ok(bytes)
}
