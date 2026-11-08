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
/// Compare layout register versions under the frozen global-counter rule.
/// The writer is only used by `merge_register` when equal counters carry
/// different values; it never makes one counter newer than another.
pub fn layout_version_cmp(a: &ConfigVersion, b: &ConfigVersion) -> Result<std::cmp::Ordering> {
    Ok(a.counter()?.cmp(&b.counter()?))
}
/// Merge the whole winning register, including every unknown envelope/value key.
/// The caller has already authenticated/authorised the layout writer.
pub fn merge_register(current: &Register, incoming: &Register) -> Result<Register> {
    verify_register(current)?;
    verify_register(incoming)?;
    if current.kind()? != incoming.kind()? {
        return Err(ErrorCode::StateConflict.into());
    }
    let compare = if current.kind()? == 2 {
        layout_version_cmp(&current.version()?, &incoming.version()?)?
    } else {
        version_cmp(&current.version()?, &incoming.version()?)?
    };
    match compare {
        std::cmp::Ordering::Less => Ok(incoming.clone()),
        std::cmp::Ordering::Greater => Ok(current.clone()),
        std::cmp::Ordering::Equal => {
            if current.digest()? == incoming.digest()? {
                Ok(current.clone())
            } else if current.version()?.writer()?.ordering_key()? == incoming.version()?.writer()?.ordering_key()? {
                Err(ErrorCode::StateConflict.into())
            } else if current.version()?.writer()?.ordering_key()? < incoming.version()?.writer()?.ordering_key()? {
                Ok(current.clone())
            } else {
                Ok(incoming.clone())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn device(byte: u8) -> DeviceId {
        DeviceId::from_raw([(1, RawValue::from(ED25519)), (2, RawValue::Bytes(vec![byte; 32]))].into()).unwrap()
    }

    fn register(counter: u64, writer: u8, marker: &str) -> Register {
        let mut raw: RawMap = [
            (1, 2u32.write()),
            (
                3,
                RawValue::Map([(1, counter.write()), (3, device(writer).write())].into()),
            ),
            (4, RawValue::Map([(90, RawValue::from(marker))].into())),
        ]
        .into();
        raw.insert(6, register_digest(&raw).unwrap().write());
        Register::from_raw(raw).unwrap()
    }

    #[test]
    fn same_counter_conflict_selects_smaller_device_id() {
        let current = register(7, 9, "large");
        let incoming = register(7, 1, "small");
        let selected = merge_register(&current, &incoming).unwrap();
        assert_eq!(selected.raw().get(&4), incoming.raw().get(&4));
    }

    #[test]
    fn lower_counter_is_old_even_when_its_device_id_is_smaller() {
        let current = register(7, 9, "latest");
        let incoming = register(6, 1, "old");
        let selected = merge_register(&current, &incoming).unwrap();
        assert_eq!(selected.raw().get(&4), current.raw().get(&4));
        assert_eq!(
            layout_version_cmp(&current.version().unwrap(), &incoming.version().unwrap()).unwrap(),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn non_layout_registers_keep_the_frozen_writer_order() {
        let current = register(7, 9, "large");
        let incoming = register(7, 1, "small");
        let mut current_raw = current.raw().clone();
        current_raw.insert(1, 8u32.write());
        current_raw.insert(6, register_digest(&current_raw).unwrap().write());
        let mut incoming_raw = incoming.raw().clone();
        incoming_raw.insert(1, 8u32.write());
        incoming_raw.insert(6, register_digest(&incoming_raw).unwrap().write());
        let current = Register::from_raw(current_raw).unwrap();
        let incoming = Register::from_raw(incoming_raw).unwrap();
        assert_eq!(merge_register(&current, &incoming).unwrap(), current);
    }
}
