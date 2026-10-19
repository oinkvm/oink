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

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::diagnostics::{Reason, Recorder};
    use oink_foundation::DeviceId;

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
