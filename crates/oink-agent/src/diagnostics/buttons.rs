//! Anonymous button-ledger evidence sampled synchronously at a rate-limited refusal.
use super::*;
use oink_input::button_diagnostics::Sample;
use oink_routing::router::{ButtonRefusal, ButtonSourcePress};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeldButton {
    pub source: String,
    pub button: oink_foundation::input::MouseButton,
    pub since_us: Option<u64>,
    pub held_for_us: Option<u64>,
    pub captured: Option<ButtonSourcePress>,
    /// OS/tap observations belong to the holder, not the remote source's hardware.
    pub holder_sample: Sample,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Refusal {
    pub at_us: u64,
    pub epoch: u64,
    pub holder: String,
    pub holds: Vec<HeldButton>,
    pub omitted: u32,
    pub dnd_leaving: bool,
}
#[derive(Clone, Copy)]
pub(super) struct Raw {
    record: ButtonRefusal,
    samples: [Option<Sample>; 32],
}
impl Raw {
    pub(super) fn capture(
        record: ButtonRefusal,
        mut sample: impl FnMut(oink_foundation::input::MouseButton, Option<(bool, u64)>) -> Sample,
    ) -> Self {
        let mut samples = [None; 32];
        for (slot, hold) in samples.iter_mut().zip(record.holds) {
            if let Some(hold) = hold {
                let captured = (hold.source == record.holder)
                    .then_some(hold.captured)
                    .flatten()
                    .map(|press| (press.physical, press.event_time_us));
                *slot = Some(sample(hold.button, captured));
            }
        }
        Self { record, samples }
    }
    pub(super) fn export(self, recorder: &Recorder) -> Event {
        let record = self.record;
        Event::HeldButtonsRefusal(Refusal {
            at_us: record.at_us,
            epoch: record.epoch,
            holder: recorder.anonymizer().device(record.holder),
            holds: record
                .holds
                .into_iter()
                .zip(self.samples)
                .filter_map(|(hold, sample)| {
                    hold.map(|hold| HeldButton {
                        source: recorder.anonymizer().device(hold.source),
                        button: hold.button,
                        since_us: hold.since_us,
                        held_for_us: hold.since_us.map(|since| record.at_us.saturating_sub(since)),
                        captured: hold.captured,
                        holder_sample: sample.unwrap(),
                    })
                })
                .collect(),
            omitted: record.omitted,
            dnd_leaving: record.dnd_leaving,
        })
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use oink_foundation::input::MouseButton;
    use oink_input::button_diagnostics::{SystemState, TapUp};
    use oink_routing::router::ButtonHold;
    #[test]
    fn held_button_refusal_real_router_records_rate_limited_system_samples() {
        use oink_foundation::{
            Point, Rect,
            display::DisplayInfo,
            input::{CapturedEvent, InputEvent, SourceKind},
        };
        use oink_routing::{Os, Router, RouterConfig, ScreenId, SimpleLayout};
        use oink_routing::{
            Packet as Message,
            control::{ControlMsg, FocusScope},
        };
        let local = DeviceId([1; 32]);
        let peer = DeviceId([2; 32]);
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::start(dir.path().into(), local, None, true).unwrap();
        recorder.wait_recovery();
        let trace = super::super::input_trace::InputTrace::new(true);
        let bounds = Rect::new(0., 0., 100., 100.);
        let mut layout = SimpleLayout::new();
        layout.add_screen(
            ScreenId::new(local, "s"),
            bounds,
            Rect::new(0., 0., 100. / 686., 100. / 686.),
        );
        layout.add_screen(
            ScreenId::new(peer, "s"),
            bounds,
            Rect::new(120. / 686., 0., 100. / 686., 100. / 686.),
        );
        let mut router = Router::new(
            RouterConfig::new(super::super::tests::boot(local), "local", Os::current()),
            Arc::new(layout),
        );
        router.set_local_displays(vec![DisplayInfo {
            key: "s".into(),
            name: "s".into(),
            bounds,
            pixel_width: 100,
            pixel_height: 100,
            reference_extent: None,
            rotation_deg: 0,
            is_primary: true,
            is_builtin: false,
            is_virtual: false,
        }]);
        assert!(router.attach_input(
            super::super::tests::boot(peer),
            [2; 16],
            true,
            true,
            Default::default(),
            0
        ));
        assert!(router.input_message(
            peer,
            Message::Control(ControlMsg::FocusState {
                epoch: 0,
                holder: peer,
                scope: None,
                session: Some([2; 16]),
                lock_state: None,
            }),
            0
        ));
        assert!(router.input_message(
            peer,
            Message::Control(ControlMsg::FocusEnter {
                epoch: 1,
                display: "s".into(),
                pos: Point::new(99., 50.),
                scope: Some(FocusScope {
                    members: vec![(local, router.routing_session()), (peer, [2; 16])]
                }),
                pointer_source: Some(peer),
            }),
            1
        ));
        router.set_diagnostics(trace.clone());
        let button = |pressed| CapturedEvent {
            physical: true,
            source: SourceKind::Mouse,
            time_us: 42,
            cursor: Some(Point::new(99., 50.)),
            event: InputEvent::Button {
                button: MouseButton::Right,
                pressed,
            },
        };
        router.on_local_event(&button(true), 10);
        let push = |router: &mut Router, now| {
            router.on_local_event(
                &CapturedEvent {
                    physical: true,
                    source: SourceKind::Mouse,
                    time_us: now,
                    cursor: Some(Point::new(99.9, 50.)),
                    event: InputEvent::PointerDelta { dx: 10., dy: 0. },
                },
                now,
            );
        };
        for now in [100, 101, 999_999, 1_000_099, 1_000_100] {
            push(&mut router, now);
        }
        router.on_local_event(&button(false), 1_000_110);
        router.on_local_event(&button(true), 1_000_120);
        push(&mut router, 1_000_130);
        trace.drain(&recorder);
        let bundle = recorder.save(Reason::Manual).unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
        let mut text = String::new();
        std::io::Read::read_to_string(&mut zip.by_name("merged.ndjson").unwrap(), &mut text).unwrap();
        let rows: Vec<Refusal> = text
            .lines()
            .filter_map(|line| {
                let row: serde_json::Value = serde_json::from_str(line).unwrap();
                row["event"]
                    .get("HeldButtonsRefusal")
                    .map(|v| serde_json::from_value(v.clone()).unwrap())
            })
            .collect();
        assert_eq!(
            rows.len(),
            3,
            "real router -> native read-only sampler -> queue -> package"
        );
        for (row, (now, since)) in rows.iter().zip([(100, 10), (1_000_100, 10), (1_000_130, 1_000_120)]) {
            assert_eq!(row.at_us, now);
            assert_eq!(row.holds.len(), 1);
            let hold = &row.holds[0];
            assert_eq!(hold.source, recorder.anonymizer().device(local));
            assert_eq!(hold.button, MouseButton::Right);
            assert_eq!(hold.since_us, Some(since));
            assert_eq!(hold.held_for_us, Some(now - since));
            #[cfg(target_os = "macos")]
            {
                assert!(hold.holder_sample.system.hid.is_some());
                assert!(hold.holder_sample.system.combined_session.is_some());
            }
            #[cfg(windows)]
            assert!(hold.holder_sample.system.asynchronous.is_some());
        }
        recorder.shutdown();
    }
    #[test]
    fn held_button_refusal_exports_ledger_and_states_sampled_at_recording_time() {
        let dir = tempfile::tempdir().unwrap();
        let local = DeviceId([1; 32]);
        let peer = DeviceId([2; 32]);
        let recorder = Recorder::start(dir.path().into(), local, Some(GroupId([2; 16])), true).unwrap();
        recorder.wait_recovery();
        let trace = super::super::input_trace::InputTrace::new(true);
        let mut record = ButtonRefusal {
            at_us: 100,
            epoch: 7,
            holder: local,
            holds: [None; 32],
            omitted: 0,
            dnd_leaving: false,
        };
        record.holds[0] = Some(ButtonHold {
            source: local,
            button: MouseButton::Right,
            since_us: Some(10),
            captured: Some(ButtonSourcePress {
                physical: true,
                event_time_us: 42,
            }),
        });
        record.holds[1] = Some(ButtonHold {
            source: peer,
            button: MouseButton::Left,
            since_us: Some(50),
            captured: None,
        });
        for system in [
            SystemState {
                sampled_at_us: 101,
                hid: Some(false),
                combined_session: Some(true),
                asynchronous: None,
            },
            SystemState {
                sampled_at_us: 101,
                hid: None,
                combined_session: None,
                asynchronous: Some(false),
            },
        ] {
            let mut calls = Vec::new();
            let raw = Raw::capture(record, |button, press| {
                calls.push((button, press));
                Sample {
                    system,
                    hid_tap: Some(TapUp {
                        matching_up: press.map(|_| true),
                        ..Default::default()
                    }),
                    session_tap: Some(TapUp {
                        matching_up: press.map(|_| false),
                        ..Default::default()
                    }),
                }
            });
            assert_eq!(
                calls,
                vec![(MouseButton::Right, Some((true, 42))), (MouseButton::Left, None)]
            );
            let encoded = serde_json::to_vec(&raw.export(&recorder)).unwrap();
            trace.evidence(super::super::input_trace::Evidence::ButtonRefusal(raw));
            let Event::HeldButtonsRefusal(event) = serde_json::from_slice(&encoded).unwrap() else {
                panic!()
            };
            assert_eq!(event.holder, recorder.anonymizer().device(local));
            assert_eq!(event.holds.len(), 2);
            assert_eq!(event.holds[1].source, recorder.anonymizer().device(peer));
            assert_eq!(event.holds[0].held_for_us, Some(90));
            assert_eq!(event.holds[1].held_for_us, Some(50));
            assert_eq!(event.holds[0].holder_sample.system, system);
            assert_eq!(event.holds[0].holder_sample.hid_tap.unwrap().matching_up, Some(true));
            assert_eq!(
                event.holds[0].holder_sample.session_tap.unwrap().matching_up,
                Some(false)
            );
            assert_eq!(event.holds[1].holder_sample.hid_tap.unwrap().matching_up, None);
        }
        trace.drain(&recorder);
        let bundle = recorder.save(Reason::Manual).unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(bundle.path).unwrap()).unwrap();
        let mut text = String::new();
        std::io::Read::read_to_string(&mut zip.by_name("merged.ndjson").unwrap(), &mut text).unwrap();
        let rows = text
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|row| row["event"].get("HeldButtonsRefusal").is_some())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2, "one package contains both sampled records");
        for forbidden in [local.to_hex(), peer.to_hex(), "key_code".into(), "scan_code".into()] {
            assert!(!text.contains(&forbidden));
        }
        recorder.shutdown();
    }
}
