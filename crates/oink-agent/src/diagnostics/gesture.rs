use super::*;
use oink_foundation::input::{Disposition, Gesture};
use oink_input::gesture_trace::{Decision, Observation, Outcome, Stage, Summary};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GestureEvent {
    pub stage: Stage,
    pub source: Option<String>,
    pub summary: Summary,
    /// Source routing reason, captured with the gesture (never reconstructed at drain time).
    #[serde(default)]
    pub contacts_path: Option<oink_routing::router::ContactsPath>,
    pub disposition: Option<super::privacy::CaptureDisposition>,
}
impl Recorder {
    /// Ordinary input consumer only; the capture owner already queued this gesture.
    #[cfg(all(test, debug_assertions))]
    pub(super) fn captured_gesture(&self, gesture: Gesture, disposition: Option<Disposition>) {
        self.captured_gesture_path(gesture, disposition, None);
    }
    pub(super) fn captured_gesture_path(
        &self,
        gesture: Gesture,
        disposition: Option<Disposition>,
        path: Option<oink_routing::router::ContactsPath>,
    ) {
        if !self.enabled() {
            return;
        }
        let Some(mut tracker) = self.inner.gestures.pop() else {
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            self.inner.press_generation.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let stamp = CAPTURE_STAMP.get().unwrap_or_else(|| self.inner.clock.now());
        let summary = tracker.observe(
            gesture,
            stamp.mono_us,
            self.inner.press_generation.load(Ordering::Relaxed),
            Some(self.inner.me),
            Outcome {
                decision: Decision::Observed,
                threshold: None,
                notches: 0,
            },
        );
        let _ = self.inner.gestures.push(tracker);
        if let Some(summary) = summary {
            self.record(Event::Gesture(GestureEvent {
                stage: Stage::Capture,
                source: Some(self.anonymizer().device(self.inner.me)),
                summary,
                contacts_path: path,
                disposition: disposition.map(|d| match d {
                    Disposition::Pass => super::privacy::CaptureDisposition::Pass,
                    Disposition::Swallow => super::privacy::CaptureDisposition::Swallow,
                }),
            }));
        }
    }
    pub(crate) fn native_gesture(&self, observation: Observation) -> Event {
        Event::Gesture(GestureEvent {
            stage: observation.stage,
            source: observation.source.map(|id| self.anonymizer().device(id)),
            summary: observation.summary,
            contacts_path: None,
            disposition: None,
        })
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use oink_foundation::input::{CapturedEvent, GesturePhase, InputEvent, SourceKind, SwipeAxis};
    #[test]
    fn gesture_source_and_native_receiver_rows_are_clock_stamped_and_private_in_zip() {
        let dir = tempfile::tempdir().unwrap();
        let me = DeviceId([1; 32]);
        let recorder = Recorder::start(dir.path().into(), me, Some(GroupId([2; 16])), true).unwrap();
        recorder.wait_recovery();
        let trace = input_trace::InputTrace::for_recorder(&recorder);
        let mut native = oink_input::gesture_trace::Tracker::default();
        for (phase, progress, at) in [
            (GesturePhase::Began, 0., 10),
            (GesturePhase::Changed, -0.9, 110),
            (GesturePhase::Ended, -0.4, 210),
        ] {
            let gesture = Gesture::DockSwipe {
                axis: SwipeAxis::Vertical,
                phase,
                progress,
                velocity: 2.75,
            };
            // Capture queued stamp survives an arbitrarily later ordinary drain.
            let stamp = clock::Stamp {
                mono_us: at,
                utc_us: utc_us(),
                clock_id: 0,
            };
            trace.finish_capture(
                &CapturedEvent {
                    event: InputEvent::Gesture(gesture),
                    physical: true,
                    source: SourceKind::Touchpad,
                    time_us: 999,
                    cursor: None,
                },
                Some((stamp, oink_input::diagnostics::CaptureMetadata::default())),
                Disposition::Swallow,
            );
            if let Some(summary) = native.observe(
                gesture,
                at + 500,
                1,
                Some(me),
                Outcome {
                    decision: Decision::BelowThreshold,
                    threshold: Some(0.5),
                    notches: 0,
                },
            ) {
                let event = recorder.native_gesture(Observation {
                    stage: Stage::WindowsMapper,
                    source: Some(me),
                    summary,
                });
                recorder.record_native(Some((at + 500, stamp.utc_us, 0)), event);
            }
        }
        trace.drain(&recorder);
        let bundle = recorder.save(Reason::Manual).unwrap();
        let rows = super::super::tests::timeline(&bundle.path, "merged.ndjson");
        let gestures: Vec<_> = rows.iter().filter(|r| r["event"].get("Gesture").is_some()).collect();
        assert_eq!(gestures.len(), 4, "no summary rows for Changed");
        let ends: Vec<_> = gestures
            .iter()
            .filter(|r| r["event"]["Gesture"]["summary"]["phase"] == "Ended")
            .collect();
        assert_eq!(ends.len(), 2);
        let a = &ends[0]["event"]["Gesture"];
        let b = &ends[1]["event"]["Gesture"];
        assert_eq!(a["summary"]["identity"], b["summary"]["identity"]);
        assert_eq!(a["source"], recorder.anonymizer().device(me));
        assert_eq!(a["source"], b["source"]);
        assert_eq!(a["summary"]["peak_abs_progress"], 0.9);
        assert_eq!(a["summary"]["duration_us"], 200);
        assert_eq!(a["summary"]["changed_events"], 1);
        assert_eq!(ends[0]["mono_us"], 210);
        assert_eq!(ends[1]["mono_us"], 710);
        let json = serde_json::to_string(&gestures).unwrap();
        for private in [
            me.to_hex(),
            "key_code".into(),
            "window_title".into(),
            "clipboard".into(),
        ] {
            assert!(!json.contains(&private));
        }
        recorder.shutdown();
    }
    #[test]
    fn capture_record_distinguishes_contacts_disabled_from_unsupported() {
        use oink_routing::router::ContactsPath;
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::start(dir.path().into(), DeviceId([1; 32]), None, true).unwrap();
        recorder.wait_recovery();
        let trace = input_trace::InputTrace::for_recorder(&recorder);
        for path in [ContactsPath::CaptureUnavailable, ContactsPath::AwaitLift] {
            let ev = CapturedEvent {
                event: InputEvent::Gesture(Gesture::Magnify {
                    phase: GesturePhase::Began,
                    amount: 0.2,
                }),
                source: SourceKind::Touchpad,
                physical: true,
                cursor: None,
                time_us: 0,
            };
            trace.finish_capture_path(&ev, trace.begin_capture(), Disposition::Swallow, Some(path));
        }
        trace.drain(&recorder);
        let bundle = recorder.save(Reason::Manual).unwrap();
        let rows = super::super::tests::timeline(&bundle.path, "merged.ndjson");
        let paths: Vec<_> = rows
            .iter()
            .filter_map(|r| r["event"].get("Gesture"))
            .map(|g| g["contacts_path"].clone())
            .collect();
        assert_eq!(
            paths,
            vec![serde_json::json!("CaptureUnavailable"), serde_json::json!("AwaitLift")]
        );
        recorder.shutdown();
    }
    #[test]
    fn disabled_recording_does_not_touch_gesture_state_and_reenable_does_not_join_old_gesture() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::start(dir.path().into(), DeviceId([1; 32]), None, false).unwrap();
        recorder.wait_recovery();
        let began = Gesture::Magnify {
            phase: GesturePhase::Began,
            amount: 0.2,
        };
        recorder.captured_gesture(began, None);
        assert!(recorder.inner.queue.load().is_empty());
        recorder.set_enabled(true).unwrap();
        recorder.captured_gesture(
            Gesture::Magnify {
                phase: GesturePhase::Ended,
                amount: 0.3,
            },
            None,
        );
        let bundle = recorder.save(Reason::Manual).unwrap();
        let rows = super::super::tests::timeline(&bundle.path, "merged.ndjson");
        let row = rows.iter().find_map(|r| r["event"].get("Gesture")).unwrap();
        assert_eq!(row["summary"]["missing_begin"], true);
        assert_eq!(row["summary"]["total_amount"], 0.3);
        recorder.shutdown();
    }
}
