//! v2 retained register operations.
//!
//! v2 deliberately has no register digest or member content signature.  Group
//! membership/authentication is established by TLS/PAKE; this module only
//! performs structural, version and conflict checks.
use super::{
    ErrorCode, RawMap, RawValue, Result,
    dto::*,
    schema::{WireDto, WireValue},
};

pub fn verify_register(register: &Register) -> Result<()> {
    // Re-run the DTO validator so malformed state is rejected at the retained
    // boundary.  Authentication is owned by the group admission layer.
    Register::from_raw(register.raw().clone()).map(|_| ())
}

pub fn version_cmp(a: &ConfigVersion, b: &ConfigVersion) -> Result<std::cmp::Ordering> {
    Ok((a.counter()?, a.writer()?.ordering_key()?).cmp(&(b.counter()?, b.writer()?.ordering_key()?)))
}
pub fn layout_version_cmp(a: &ConfigVersion, b: &ConfigVersion) -> Result<std::cmp::Ordering> {
    Ok(a.counter()?.cmp(&b.counter()?))
}

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
            if current.value()? == incoming.value()? {
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
    RawValue::Map(raw.clone()).encode(super::limits::CONFIG_STATE)?;
    Register::from_raw(raw)
}

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
