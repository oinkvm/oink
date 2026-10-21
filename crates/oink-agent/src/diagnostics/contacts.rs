//! Per-frame contact topology in the existing generic numeric log, without
//! geometry or any new release-wire event/field identifiers.
use super::Event;
use oink_input::contact_trace::Observation;

pub(crate) fn event(o: Observation) -> Event {
    let mut fields = Vec::with_capacity(26);
    fields.extend([
        ("stage".into(), f64::from(o.stage)),
        ("device".into(), f64::from(o.device)),
        ("time_us".into(), o.time_us as f64),
        ("tick".into(), f64::from(o.tick)),
        ("count".into(), f64::from(o.count)),
        ("points".into(), f64::from(o.points)),
        ("ptp_active_mask".into(), f64::from(o.active_mask)),
        ("ptp_count".into(), f64::from(o.active_mask.count_ones())),
        ("ptp_known".into(), u8::from(o.known).into()),
        ("success".into(), u8::from(o.success).into()),
    ]);
    for (index, ((id, flags), slot)) in o.ids.into_iter().zip(o.flags).zip(o.slots).enumerate() {
        fields.push((format!("id{index}"), f64::from(id)));
        fields.push((format!("flags{index}"), f64::from(flags)));
        fields.push((format!("slot{}", index + 1), f64::from(slot)));
    }
    Event::Log {
        module: "touchpad_contacts".into(),
        line: 0,
        level: 2,
        fields,
        labels: vec![],
    }
}

/// Formatting happens on the ordinary recorder thread, never on MT's callback.
pub(crate) fn mt_event(o: oink_input::mt_trace::Observation) -> Event {
    let mut fields: Vec<(String, f64)> = [
        ("schema", 2.),
        ("kind", o.kind as f64),
        ("device", o.device as f64),
        ("callback_device", o.callback_device as f64),
        ("devices", o.devices as f64),
        ("built_in", o.built_in as f64),
        ("frame_count", o.frame_count as f64),
        ("source_us", o.source_us as f64),
        ("received_us", o.received_us as f64),
        ("source_gap_us", o.source_gap_us as f64),
        ("received_gap_us", o.received_gap_us as f64),
        ("source_gap_max_us", o.source_gap_max_us as f64),
        ("received_gap_max_us", o.received_gap_max_us as f64),
        ("source_backwards", o.source_backwards as f64),
        ("samples", o.samples as f64),
        ("suppressed", o.suppressed as f64),
        ("raw_count", o.raw_count as f64),
        ("accepted", o.accepted as f64),
        ("valid", u8::from(o.valid) as f64),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v))
    .collect();
    for (i, c) in o.contacts.iter().take(usize::from(o.raw_count).min(16)).enumerate() {
        for (k, v) in [
            ("id", c.id as f64),
            ("path_index", c.id as f64),
            ("finger_identity", c.finger_identity as f64),
            ("state", c.state as f64),
            ("frame", c.frame as f64),
            ("size_bits", c.size_bits as f64),
            ("density_bits", c.density_bits as f64),
        ] {
            fields.push((format!("{k}{i}"), v));
        }
    }
    Event::Log {
        module: "mt_contacts".into(),
        line: 0,
        level: 2,
        fields,
        labels: vec![],
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::diagnostics::{Reason, Recorder};
    use oink_foundation::DeviceId;

    #[test]
    fn raw_mt_states_frames_and_sensor_bits_survive_export_without_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::start(dir.path().into(), DeviceId([1; 32]), None, true).unwrap();
        recorder.wait_recovery();
        let mut observation = oink_input::mt_trace::Observation {
            kind: 1,
            device: 3,
            callback_device: 42,
            devices: 2,
            built_in: 1,
            raw_count: 2,
            accepted: 1,
            valid: true,
            frame_count: 1234,
            source_us: 10000,
            received_us: 90000000,
            source_gap_us: 8000,
            source_gap_max_us: 24000,
            received_gap_max_us: 27000,
            ..Default::default()
        };
        observation.contacts[0] = oink_input::mt_trace::Contact {
            id: 7,
            finger_identity: 9,
            state: 4,
            frame: 1234,
            size_bits: 0.5f32.to_bits(),
            density_bits: f32::NAN.to_bits(),
        };
        observation.contacts[1].state = 7;
        recorder.record_native(None, mt_event(observation));
        let bundle = recorder.save(Reason::Manual).unwrap();
        let rows = super::super::tests::timeline(&bundle.path, "merged.ndjson");
        let log = rows
            .iter()
            .find_map(|r| {
                let log = r["event"].get("Log")?;
                (log["module"] == "mt_contacts").then_some(log)
            })
            .unwrap();
        let fields: std::collections::BTreeMap<String, f64> =
            serde_json::from_value::<Vec<(String, f64)>>(log["fields"].clone())
                .unwrap()
                .into_iter()
                .collect();
        assert_eq!(fields["id0"], 7.);
        assert_eq!(fields["schema"], 2.);
        assert_eq!(fields["path_index0"], 7.);
        assert_eq!(fields["finger_identity0"], 9.);
        assert_eq!(fields["state1"], 7.);
        assert_eq!(fields["frame_count"], 1234.);
        assert_eq!(fields["raw_count"], 2.);
        assert_eq!(fields["size_bits0"], f64::from(0.5f32.to_bits()));
        assert_eq!(fields["density_bits0"], f64::from(f32::NAN.to_bits()));
        assert_eq!(fields["source_gap_max_us"], 24000.);
        assert_eq!(fields["devices"], 2.);
        assert!(
            !fields
                .keys()
                .any(|k| k.contains("position") || k.contains("coordinate") || k.contains("serial"))
        );
        assert_eq!(fields.len(), 33); // Fixed metadata plus exactly two raw contacts.
        recorder.shutdown();
    }

    #[test]
    fn native_contact_slots_and_submitted_state_survive_recorder_export() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::start(dir.path().into(), DeviceId([1; 32]), None, true).unwrap();
        recorder.wait_recovery();
        for observation in [
            Observation {
                stage: 1,
                device: 1,
                time_us: 9000,
                tick: 9,
                count: 2,
                points: 3,
                ids: [1, 3, 4, 0, 0],
                flags: [0x4000, 0x4006, 0x4006, 0, 0],
                slots: [0, 0, u32::MAX, 17, 0],
                active_mask: 0b01100,
                known: true,
                success: true,
            },
            Observation {
                stage: 2,
                device: 1,
                time_us: 9000,
                known: true,
                success: true,
                ..Default::default()
            },
        ] {
            recorder.record_native(None, event(observation));
        }
        let bundle = recorder.save(Reason::Manual).unwrap();
        let rows = super::super::tests::timeline(&bundle.path, "merged.ndjson");
        let logs: Vec<_> = rows
            .iter()
            .filter_map(|r| {
                let log = r["event"].get("Log")?;
                (log["module"] == "touchpad_contacts").then_some(log)
            })
            .collect();
        assert_eq!(logs.len(), 2);
        let fields: std::collections::BTreeMap<_, _> =
            serde_json::from_value::<Vec<(String, f64)>>(logs[0]["fields"].clone())
                .unwrap()
                .into_iter()
                .collect();
        assert_eq!(fields["slot3"], f64::from(u32::MAX));
        assert_eq!(fields["slot4"], 17.);
        assert_eq!(fields["flags0"], f64::from(0x4000));
        assert_eq!(fields["id2"], 4.);
        assert_eq!(fields["ptp_active_mask"], 12.);
        assert_eq!(fields["ptp_count"], 2.);
        assert_eq!(fields["ptp_known"], 1.);
        assert_eq!(fields["points"], 3.);
        let closed: std::collections::BTreeMap<_, _> =
            serde_json::from_value::<Vec<(String, f64)>>(logs[1]["fields"].clone())
                .unwrap()
                .into_iter()
                .collect();
        assert_eq!(closed["stage"], 2.);
        assert_eq!(closed["ptp_active_mask"], 0.);
        assert_eq!(closed["count"], 0.);
        recorder.shutdown();
    }
}
