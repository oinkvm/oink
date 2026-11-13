//! Translate only the disk representation of the old layout visibility envelope.
use oink_protocol::v2::{
    self, RawMap, RawValue, dto,
    schema::{WireDto, WireValue, required},
};

pub(super) fn project(register: dto::Register) -> v2::Result<dto::Register> {
    oink_network::legacy_merge::project_register(register)
}

pub(super) fn retain(previous: &dto::Register, candidate: &mut RawMap) -> v2::Result<()> {
    // A candidate already in the old storage shape (a retained 0.3.x merge
    // projection) carries its own digest and visibility envelope.
    if !previous.raw().contains_key(&6) || candidate.contains_key(&6) || candidate.contains_key(&7) {
        return Ok(());
    }
    let current = project(previous.clone())?;
    if current.raw() == candidate {
        *candidate = previous.raw().clone();
        return Ok(());
    }
    if previous.kind()? == 2 {
        let mut layout = required::<RawMap>(candidate, 4)?;
        let mut metadata = match previous.raw().get(&7) {
            None | Some(RawValue::Null) => RawMap::new(),
            Some(metadata) => metadata.as_map()?.clone(),
        };
        let mut hidden_devices = Vec::new();
        let mut known_devices = Vec::new();
        let mut hidden_screens = Vec::new();
        let mut known_screens = Vec::new();
        let mut devices = Vec::new();
        for device in dto::LayoutValue::from_raw(layout.clone())?.devices()?.0 {
            let mut screens = Vec::new();
            for screen in device.screens()?.0 {
                if screen.raw().contains_key(&11) {
                    let id = screen.id()?.write();
                    known_screens.push(id.clone());
                    if screen.hidden()? {
                        hidden_screens.push(id);
                    }
                }
                let mut fields = screen.into_raw();
                fields.remove(&11);
                screens.push(RawValue::Map(fields));
            }
            if device.raw().contains_key(&5) {
                let id = device.device()?.write();
                known_devices.push(id.clone());
                if device.hidden()? {
                    hidden_devices.push(id);
                }
            }
            let mut fields = device.into_raw();
            fields.remove(&5);
            fields.insert(4, RawValue::Array(screens));
            devices.push(RawValue::Map(fields));
        }
        layout.insert(1, RawValue::Array(devices));
        candidate.insert(4, RawValue::Map(layout));
        if !known_devices.is_empty() || !known_screens.is_empty() || !metadata.is_empty() {
            metadata.insert(1, RawValue::Array(hidden_devices));
            metadata.insert(2, RawValue::Array(hidden_screens));
            metadata.insert(3, RawValue::Array(known_devices));
            metadata.insert(4, RawValue::Array(known_screens));
            candidate.insert(7, RawValue::Map(metadata));
        }
    } else if let Some(metadata) = previous.raw().get(&7) {
        candidate.entry(7).or_insert_with(|| metadata.clone());
    }
    // The persisted v1 digest covers the retaining map with defaulted deleted.
    // v2 peers never receive this checksum or the old visibility envelope.
    use sha2::{Digest, Sha256};
    let mut body = candidate.clone();
    body.remove(&6);
    body.entry(5).or_insert(false.write());
    let digest = Sha256::digest(RawValue::Map(body).encode(v2::limits::CONFIG_STATE)?);
    candidate.insert(6, RawValue::Bytes(digest.to_vec()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_v1_visibility_and_keeps_the_old_storage_shape_after_edits() {
        let device = oink_network::wire_identity::device(oink_foundation::DeviceId([5; 32])).write();
        let screen = RawValue::Map([(1, device.clone()), (2, RawValue::from("built-in"))].into());
        let screens = RawValue::Array(vec![RawValue::Map(
            [
                (1, screen.clone()),
                (2, RawValue::from("Built-in")),
                (
                    3,
                    RawValue::Map(
                        [
                            (1, 0.0.write()),
                            (2, 0.0.write()),
                            (3, 640.0.write()),
                            (4, 480.0.write()),
                        ]
                        .into(),
                    ),
                ),
                (5, dto::SizeSource::Reported.write()),
                (9, true.write()),
                (10, true.write()),
            ]
            .into(),
        )]);
        let placement = RawValue::Map(
            [
                (1, device.clone()),
                (2, RawValue::Map([(1, 0.0.write()), (2, 0.0.write())].into())),
                (3, 1.0.write()),
                (4, screens),
            ]
            .into(),
        );
        let raw: RawMap = [
            (1, 2u32.write()),
            (3, RawValue::Map([(1, 1u64.write()), (3, device.clone())].into())),
            (
                4,
                RawValue::Map(
                    [
                        (1, RawValue::Array(vec![placement])),
                        (2, dto::LayoutSource::Default.write()),
                        (4, RawValue::Map(RawMap::new())),
                    ]
                    .into(),
                ),
            ),
            (6, RawValue::Bytes(vec![4; 32])),
            (
                7,
                RawValue::Map(
                    [
                        (1, RawValue::Array(vec![device])),
                        (2, RawValue::Array(vec![screen])),
                        (8, RawValue::from("preserved")),
                    ]
                    .into(),
                ),
            ),
        ]
        .into();
        let stored = dto::Register::from_raw(raw.clone()).unwrap();
        let mut projected = project(stored.clone()).unwrap().into_raw();
        assert!(!projected.contains_key(&6));
        assert!(!projected.contains_key(&7));
        let layout = dto::LayoutValue::from_raw(required(&projected, 4).unwrap()).unwrap();
        let placement = &layout.devices().unwrap().0[0];
        assert!(placement.hidden().unwrap());
        assert!(placement.screens().unwrap().0[0].hidden().unwrap());

        let mut same = projected.clone();
        retain(&stored, &mut same).unwrap();
        assert_eq!(same, raw);

        let mut layout = layout.into_raw();
        layout.insert(2, dto::LayoutSource::Manual.write());
        projected.insert(4, RawValue::Map(layout));
        let mut edited = projected.clone();
        retain(&stored, &mut edited).unwrap();
        assert_ne!(edited[&6], raw[&6]);
        assert_eq!(edited[&7].as_map().unwrap()[&8], RawValue::from("preserved"));
        assert_eq!(
            project(dto::Register::from_raw(edited).unwrap()).unwrap().raw(),
            &projected,
        );
    }

    /// M2: a retained 0.3.x projection replaces a 0.3.x state. The candidate is
    /// already in the old storage shape; its own visibility envelope is kept.
    #[test]
    fn a_candidate_already_in_the_old_shape_keeps_its_own_visibility_envelope() {
        let register = |hidden: u8| -> RawMap {
            let device = oink_network::wire_identity::device(oink_foundation::DeviceId([hidden; 32])).write();
            [
                (1, 2u32.write()),
                (
                    3,
                    RawValue::Map([(1, u64::from(hidden).write()), (3, device.clone())].into()),
                ),
                (
                    4,
                    RawValue::Map(
                        [
                            (1, RawValue::Array(Vec::new())),
                            (2, dto::LayoutSource::Default.write()),
                            (4, RawValue::Map(RawMap::new())),
                        ]
                        .into(),
                    ),
                ),
                (6, RawValue::Bytes(vec![hidden; 32])),
                (
                    7,
                    RawValue::Map([(1, RawValue::Array(vec![device])), (2, RawValue::Array(Vec::new()))].into()),
                ),
            ]
            .into()
        };
        let previous = dto::Register::from_raw(register(5)).unwrap();
        let mut candidate = register(6);
        let expected = candidate.clone();
        retain(&previous, &mut candidate).unwrap();
        assert_eq!(candidate, expected);
    }
}
