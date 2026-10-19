//! Drag boundaries only. Reuse the ordinary Log envelope so older completed
//! segments remain readable; no payload, path, artwork, or motion stream enters it.
//! Dispatched elapsed_us is injection-queue wait; Finished elapsed_us is ordinary
//! command execution. NativeInput success is an API receipt, not a drop-target ACK.
//! MessageSent observes submission to the existing routing/ordinary transport path.
use super::{
    Event, Recorder,
    input_trace::{Evidence, InputTrace},
};
use oink_foundation::content::ClipId;
use oink_foundation::{DeviceId, Point, clipboard::ClipData, input::InjectEvent};
use oink_routing::{DndCmd, DndEvent};
use oink_routing::{Packet as Message, dnd::DndMsg};

#[derive(Clone, Copy)]
pub(crate) struct Raw {
    stage: &'static str,
    action: &'static str,
    peer: Option<DeviceId>,
    id: Option<ClipId>,
    input: Option<DeviceId>,
    epoch: Option<u64>,
    at: Option<Point>,
    success: Option<bool>,
    elapsed_us: Option<u64>,
    kinds: u8,
    items: usize,
    files: usize,
    pressed: Option<bool>,
}

impl Raw {
    fn new(stage: &'static str, action: &'static str) -> Self {
        Self {
            stage,
            action,
            peer: None,
            id: None,
            input: None,
            epoch: None,
            at: None,
            success: None,
            elapsed_us: None,
            kinds: 0,
            items: 0,
            files: 0,
            pressed: None,
        }
    }
    fn read_boundary(&self) {
        use oink_foundation::drag_trace::{Context, Trace};
        let stage = match self.stage {
            "CommandQueued" => 50,
            "CommandDispatched" => 51,
            "CommandStarted" => 52,
            "CommandFinished" => 53,
            _ => return,
        };
        let action = match self.action {
            "HoldSource" => 1,
            "ReleaseSourceButton" => 2,
            "FinishSource" => 3,
            "Begin" => 4,
            "PrepareDrop" => 5,
            "ForgetSource" => 6,
            "ReleaseData" => 7,
            "Cancel" => 8,
            _ => 0,
        };
        let trace = Trace::new(
            stage,
            Context {
                offer: self.id.map_or(0, |id| id.seq),
                object: self.epoch.unwrap_or(0),
                aux: action,
                ..Context::current()
            },
        );
        if let Some(success) = self.success {
            trace.result(if success { 0 } else { -1 });
        } else {
            trace.result(0);
        }
    }
    fn items(&mut self, items: &[ClipData]) {
        self.items = items.len();
        for item in items {
            self.kinds |= match item {
                ClipData::Text(_) => 1,
                ClipData::Html(_) => 2,
                ClipData::Rtf(_) => 4,
                ClipData::Png(_) => 8,
                ClipData::FileRoots(roots) => {
                    self.files += roots.len();
                    16
                }
                ClipData::Files(files) => {
                    self.files += files.len();
                    16
                }
                ClipData::Custom { .. } => 32,
                ClipData::Native { item, .. } => {
                    if item.file.is_some() {
                        self.files += 1;
                    }
                    64
                }
            };
        }
    }
    pub(super) fn export(self, recorder: &Recorder) -> Event {
        let anon = recorder.anonymizer();
        let mut labels = vec![
            ("stage".into(), self.stage.into()),
            ("action".into(), self.action.into()),
        ];
        if let Some(peer) = self.peer {
            labels.push(("peer".into(), anon.device(peer)));
        }
        if let Some(input) = self.input {
            labels.push(("input".into(), anon.device(input)));
        }
        if let Some(id) = self.id {
            let mut bytes = [0u8; 44];
            bytes[..4].copy_from_slice(b"drag");
            bytes[4..36].copy_from_slice(&id.origin.0);
            bytes[36..].copy_from_slice(&id.seq.to_le_bytes());
            labels.push(("drag".into(), anon.control_identity(&bytes)));
            labels.push(("origin".into(), anon.device(id.origin)));
        }
        let mut fields = vec![
            ("kind_mask".into(), self.kinds as f64),
            ("items".into(), self.items as f64),
            ("files".into(), self.files as f64),
        ];
        if let Some(epoch) = self.epoch {
            fields.push(("epoch".into(), epoch as f64));
        }
        if let Some(at) = self.at {
            fields.extend([("x".into(), at.x), ("y".into(), at.y)]);
        }
        if let Some(success) = self.success {
            fields.push(("success".into(), u8::from(success) as f64));
        }
        if let Some(pressed) = self.pressed {
            fields.push(("pressed".into(), u8::from(pressed) as f64));
        }
        if let Some(elapsed) = self.elapsed_us {
            fields.push(("elapsed_us".into(), elapsed as f64));
        }
        Event::Log {
            module: "oink_agent::diagnostics::dnd".into(),
            line: 0,
            level: 2,
            fields,
            labels,
        }
    }
}
impl Recorder {
    pub(crate) fn dnd_integrity(&self, id: ClipId, record: oink_routing::DndIntegrity) {
        let mut identity = [0u8; 44];
        identity[..4].copy_from_slice(b"drag");
        identity[4..36].copy_from_slice(&id.origin.0);
        identity[36..].copy_from_slice(&id.seq.to_le_bytes());
        self.record(Event::DndIntegrity {
            drag: self.anonymizer().control_identity(&identity),
            record,
        });
    }
    pub(crate) fn dnd_native_hold(
        &self,
        id: ClipId,
        physical: bool,
        observation: oink_foundation::dnd::NativeHoldSummary,
    ) {
        let mut identity = [0u8; 44];
        identity[..4].copy_from_slice(b"drag");
        identity[4..36].copy_from_slice(&id.origin.0);
        identity[36..].copy_from_slice(&id.seq.to_le_bytes());
        self.record(Event::DndNativeHold {
            drag: self.anonymizer().control_identity(&identity),
            physical,
            observation: Box::new(observation),
        });
    }
    /// Join ordered native-format records to the anonymized cross-device drag.
    pub(crate) fn dnd_native_operation(&self, id: ClipId, sequence: u64, hosting: bool) {
        let mut raw = Raw::new("NativeOperation", if hosting { "Host" } else { "Source" });
        raw.id = Some(id);
        let mut event = raw.export(self);
        if let Event::Log { fields, .. } = &mut event {
            fields.push(("dnd_hold_sequence".into(), sequence as f64));
        }
        self.record(event);
    }
}

impl InputTrace {
    // Probe follows the pointer and can run for every motion. Its first placement
    // is represented by the probing state boundary; never record follow commands.
    pub(crate) fn dnd_command(
        &self,
        command: &DndCmd,
        stage: &'static str,
        elapsed_us: Option<u64>,
        success: Option<bool>,
    ) {
        if !self.enabled() {
            return;
        }
        let Some(mut raw) = Self::dnd_command_data(command, stage) else {
            return;
        };
        raw.elapsed_us = elapsed_us;
        raw.success = success;
        raw.read_boundary();
        self.evidence(Evidence::Dnd(raw));
    }
    fn dnd_command_data(command: &DndCmd, stage: &'static str) -> Option<Raw> {
        let action = match command {
            DndCmd::Integrity { .. } => return None,
            DndCmd::Prepare { .. } => "Prepare",
            DndCmd::RevokePreparation { .. } => "RevokePreparation",
            DndCmd::ExportFailed { .. } => "ExportFailed",
            DndCmd::PrepareDrop { .. } => "PrepareDrop",
            DndCmd::HoldSource { .. } => "HoldSource",
            DndCmd::ResumeSource { .. } => "ResumeSource",
            DndCmd::FinishSource { .. } => "FinishSource",
            DndCmd::ReleaseSourceButton { .. } => "ReleaseSourceButton",
            DndCmd::ForgetSource { .. } => "ForgetSource",
            DndCmd::ReleaseData { .. } => "ReleaseData",
            DndCmd::Probe { .. } => return None,
            DndCmd::Unprobe => "Unprobe",
            DndCmd::SetImage(_) => "SetImage",
            DndCmd::UpdateImage { .. } => "UpdateImage",
            DndCmd::LeaveHost { .. } => "LeaveHost",
            DndCmd::Cancel { .. } => "Cancel",
            DndCmd::Begin { .. } => "Begin",
        };
        let mut raw = Raw::new(stage, action);
        match command {
            DndCmd::UpdateImage { id, epoch, .. }
            | DndCmd::HoldSource { id, epoch, .. }
            | DndCmd::LeaveHost { id, epoch, .. }
            | DndCmd::PrepareDrop { id, epoch }
            | DndCmd::Begin { id, epoch, .. } => {
                raw.id = Some(*id);
                raw.epoch = Some(*epoch);
            }
            DndCmd::ResumeSource { id, .. }
            | DndCmd::FinishSource { id, .. }
            | DndCmd::ReleaseSourceButton { id, .. }
            | DndCmd::ForgetSource { id }
            | DndCmd::ReleaseData { id } => raw.id = Some(*id),
            _ => {}
        }
        if let DndCmd::Begin { id, at, items, .. } = command {
            raw.id = Some(*id);
            raw.at = Some(*at);
            raw.items(items);
        }
        Some(raw)
    }
    pub(crate) fn dnd_start_command(&self, command: &DndCmd) -> Option<Raw> {
        if !self.enabled() {
            return None;
        }
        let raw = Self::dnd_command_data(command, "CommandStarted")?;
        raw.read_boundary();
        self.evidence(Evidence::Dnd(raw));
        Some(raw)
    }
    pub(crate) fn dnd_finish_command(&self, mut raw: Raw, elapsed_us: u64, success: Option<bool>) {
        raw.stage = "CommandFinished";
        raw.elapsed_us = Some(elapsed_us);
        raw.success = success;
        raw.read_boundary();
        self.evidence(Evidence::Dnd(raw));
    }
    pub(crate) fn dnd_event(&self, event: &DndEvent) {
        self.dnd_observed(event, "RouterEvent");
    }
    pub(crate) fn dnd_observed(&self, event: &DndEvent, stage: &'static str) {
        if !self.enabled() {
            return;
        }
        let mut raw = Raw::new(
            stage,
            match event {
                DndEvent::SourceEffectFailed { .. } => "SourceEffectFailed",
                DndEvent::HostReleaseFailed { .. } => "HostReleaseUnconfirmed",
                DndEvent::SourceHeld { .. } => "SourceHeld",
                DndEvent::SourceEnded { .. } => "SourceEnded",
                DndEvent::SourceCatcherLost { .. } => "SourceCatcherLost",
                DndEvent::SourceCancelRetired { .. } => "SourceCancelRetired",
                DndEvent::SourceButtonReleased { .. } => "SourceButtonReleased",
                DndEvent::SourceExported { .. } => "SourceExported",
                DndEvent::SourceImage { .. } => "SourceImage",
                DndEvent::Detected { .. } | DndEvent::DetectedWithImage { .. } => "Detected",
                DndEvent::HostLeft { .. } => "Released",
                DndEvent::Ended { .. } => "Ended",
            },
        );
        match event {
            DndEvent::HostReleaseFailed { id, epoch } => {
                raw.id = Some(*id);
                raw.epoch = Some(*epoch);
                raw.success = Some(false);
            }
            DndEvent::SourceHeld { id, epoch, ok } => {
                raw.id = Some(*id);
                raw.epoch = Some(*epoch);
                raw.success = Some(*ok);
            }
            DndEvent::SourceImage { id, .. }
            | DndEvent::SourceEffectFailed { id, .. }
            | DndEvent::SourceCatcherLost { id }
            | DndEvent::SourceCancelRetired { id } => {
                raw.id = Some(*id);
            }
            DndEvent::SourceButtonReleased { id, ok } => {
                raw.id = Some(*id);
                raw.success = Some(*ok);
            }
            DndEvent::SourceEnded { id, accepted } => {
                raw.id = Some(*id);
                raw.success = Some(*accepted);
            }
            DndEvent::SourceExported { id, epoch, items } => {
                raw.id = Some(*id);
                raw.epoch = Some(*epoch);
                raw.success = Some(items.is_ok());
                if let Ok(items) = items {
                    raw.items(items);
                }
            }
            DndEvent::Detected { id, items } | DndEvent::DetectedWithImage { id, items, .. } => {
                raw.id = Some(*id);
                raw.items(items);
            }
            DndEvent::HostLeft { id, epoch, ok } => {
                raw.id = Some(*id);
                raw.epoch = Some(*epoch);
                raw.success = Some(*ok);
            }
            DndEvent::Ended { id, epoch, dropped } => {
                raw.id = Some(*id);
                raw.epoch = Some(*epoch);
                raw.success = Some(*dropped);
            }
        }
        self.evidence(Evidence::Dnd(raw));
    }
    pub(crate) fn dnd_message(&self, peer: DeviceId, sent: bool, message: &Message) {
        if !matches!(message, Message::Dnd(_)) {
            return;
        }
        if !self.enabled() {
            return;
        }
        let mut raw = Raw::new(if sent { "MessageSent" } else { "MessageReceived" }, "Enter");
        raw.peer = Some(peer);
        match message {
            Message::Dnd(message) => match message {
                DndMsg::Source {
                    offer, holder, action, ..
                } => {
                    use oink_routing::dnd::SourceAction;
                    raw.id = Some(offer.clip);
                    raw.epoch = Some(holder.epoch);
                    raw.action = match action {
                        SourceAction::Handoff { .. } => "SourceHandoff",
                        SourceAction::StartDelivery => "StartDelivery",
                        SourceAction::Cancel => "SourceCancel",
                    };
                }
                DndMsg::Caps { enabled } => {
                    raw.action = "Caps";
                    raw.success = Some(*enabled);
                }
                DndMsg::Enter { id, epoch } => {
                    raw.id = Some(*id);
                    raw.epoch = Some(*epoch);
                }
                DndMsg::Notice(notice) => {
                    raw.action = "Notice";
                    raw.id = Some(notice.key.offer.clip);
                    raw.epoch = notice.state.holder().map(|h| h.epoch);
                    raw.input = Some(notice.input);
                    raw.items(&notice.items);
                }
                DndMsg::NoticeRequest { id, holder, .. } | DndMsg::Refused { id, holder, .. } => {
                    raw.action = if matches!(message, DndMsg::Refused { .. }) {
                        "Refused"
                    } else {
                        "NoticeRequest"
                    };
                    raw.id = Some(*id);
                    raw.epoch = Some(holder.epoch);
                }
                DndMsg::SourceButton { key, disposition, .. } => {
                    use oink_routing::dnd::SourceButtonDisposition::*;
                    raw.action = match disposition {
                        OriginalSourceOwns => "SourceButtonOwned",
                        ReleaseSubmitted => "SourceButtonSubmitted",
                        Released => "SourceButtonReleased",
                        ReturnedToOriginalRoute => "SourceButtonReturned",
                        Unconfirmed => "SourceButtonUnconfirmed",
                    };
                    raw.id = Some(key.offer.clip);
                }
                DndMsg::PreviewRequest { id, epoch, .. } | DndMsg::Preview { id, epoch, .. } => {
                    raw.id = Some(*id);
                    raw.epoch = Some(*epoch);
                    raw.action = match message {
                        DndMsg::PreviewRequest { .. } => "PreviewRequest",
                        _ => "Preview",
                    };
                    if let DndMsg::Preview { image, .. } = message {
                        raw.success = Some(image.is_some());
                    }
                }
            },
            _ => return,
        }
        self.evidence(Evidence::Dnd(raw));
    }
    pub(crate) fn dnd_items(&self, peer: DeviceId, id: ClipId, epoch: u64, items: Option<&[ClipData]>, accepted: bool) {
        if !self.enabled() {
            return;
        }
        let mut raw = Raw::new("RouterEvent", if items.is_some() { "ItemsReady" } else { "Refused" });
        raw.peer = Some(peer);
        raw.id = Some(id);
        raw.epoch = Some(epoch);
        raw.success = Some(accepted);
        if let Some(items) = items {
            raw.items(items);
        }
        self.evidence(Evidence::Dnd(raw));
    }
    pub(crate) fn dnd_state(&self, state: &'static str, epoch: u64, at: Option<Point>) {
        if !self.enabled() {
            return;
        }
        let value = match state {
            "idle" => 0,
            "probing" => 1,
            "ready" => 2,
            "leaving" => 3,
            "hosting" => 4,
            _ => return,
        };
        // The single routing owner only checks an atomic for ordinary motion;
        // state changes alone queue evidence or wake the ordinary consumer.
        if self.last_dnd_state.load(std::sync::atomic::Ordering::Relaxed) != value {
            self.last_dnd_state.store(value, std::sync::atomic::Ordering::Relaxed);
            let mut raw = Raw::new("StateChanged", state);
            raw.epoch = Some(epoch);
            raw.at = at;
            self.evidence(Evidence::Dnd(raw));
        }
    }
    pub(crate) fn dnd_native(&self, action: &'static str, at: Option<Point>, pressed: Option<bool>, success: bool) {
        if !self.enabled() {
            return;
        }
        let mut raw = Raw::new("NativeInput", action);
        raw.at = at;
        raw.pressed = pressed;
        raw.success = Some(success);
        self.evidence(Evidence::Dnd(raw));
    }
    pub(crate) fn dnd_injected(&self, event: &InjectEvent, success: bool) {
        match event {
            InjectEvent::Button {
                button: oink_foundation::input::MouseButton::Left,
                pressed,
            } => self.dnd_native("LeftButton", None, Some(*pressed), success),
            InjectEvent::Key { code, pressed } if *code == oink_foundation::input::KeyCode::ESCAPE => {
                self.dnd_native("Escape", None, Some(*pressed), success)
            }
            _ => {} // no motion stream or arbitrary key identity
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::super::{GroupId, Reason, SafeInput};
    use super::*;
    use oink_foundation::{
        clipboard::FileEntry,
        input::{KeyCode, MouseButton},
    };

    #[test]
    fn drag_boundaries_survive_bundle_export_without_payloads_or_probe_motion_rows() {
        let dir = tempfile::tempdir().unwrap();
        let me = DeviceId([1; 32]);
        let peer = DeviceId([2; 32]);
        let recorder = Recorder::start(dir.path().into(), me, Some(GroupId([3; 16])), true).unwrap();
        recorder.wait_recovery();
        let trace = InputTrace::for_recorder(&recorder);
        let id = ClipId {
            origin: me,
            seq: oink_routing::dnd::DND_SEQ_BIT | 42,
        };
        let items = vec![
            ClipData::Text("private drag payload".into()),
            ClipData::Files(vec![FileEntry {
                path: "private-filename.txt".into(),
                local_path: Some("/private/drag-source".into()),
                size: 19,
                is_dir: false,
                modified_ms: None,
            }]),
        ];
        let command = DndCmd::Begin {
            grant: crate::dnd::test_target(id, 1),
            epoch: 1,
            id,
            at: Point::new(17., 3.),
            items: items.clone(),
            retained_inventory: None,
        };
        trace.dnd_state("probing", 5, Some(Point::new(3., 0.)));
        for _ in 0..1000 {
            trace.dnd_state("probing", 5, Some(Point::new(9., 0.)));
            trace.dnd_command(&DndCmd::Probe { at: Point::new(9., 0.) }, "CommandQueued", None, None);
            trace.dnd_message(
                peer,
                false,
                &Message::Input(oink_routing::input::InputMsg::Motion { seq: 1, dx: 1., dy: 2. }),
            );
            trace.dnd_injected(
                &InjectEvent::MoveTo {
                    pos: Point::new(5., 6.),
                },
                true,
            );
        }
        trace.dnd_event(&DndEvent::Detected {
            id,
            items: items.clone(),
        });
        let message = Message::Dnd(DndMsg::Enter { id, epoch: 6 });
        trace.dnd_message(peer, true, &message);
        trace.dnd_message(peer, false, &message);
        trace.dnd_command(&command, "CommandQueued", None, None);
        trace.dnd_command(&command, "CommandDispatched", Some(1200), None);
        let raw = trace.dnd_start_command(&command).unwrap();
        trace.dnd_finish_command(raw, 43000, Some(true));
        trace.dnd_injected(
            &InjectEvent::Button {
                button: MouseButton::Left,
                pressed: false,
            },
            true,
        );
        trace.dnd_injected(
            &InjectEvent::Key {
                code: KeyCode::keyboard(4),
                pressed: true,
            },
            true,
        );
        trace.dnd_event(&DndEvent::Ended {
            id,
            epoch: 1,
            dropped: true,
        });
        trace.dnd_state("idle", 6, None);
        trace.drain(&recorder);
        let bundle = recorder.save(Reason::Manual).unwrap();
        let rows = super::super::tests::timeline(&bundle.path, "merged.ndjson");
        let logs: Vec<_> = rows
            .iter()
            .filter_map(|r| r["event"].get("Log"))
            .filter(|e| e["module"] == "oink_agent::diagnostics::dnd")
            .collect();
        assert_eq!(logs.len(), 11, "only boundaries, no probe/motion/key stream");
        fn labels(e: &serde_json::Value) -> std::collections::HashMap<&str, &str> {
            e["labels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| (p[0].as_str().unwrap(), p[1].as_str().unwrap()))
                .collect()
        }
        let enter: Vec<_> = logs.iter().filter(|e| labels(e)["action"] == "Enter").collect();
        assert_eq!(labels(enter[0])["drag"], labels(enter[1])["drag"]);
        let finished = logs.iter().find(|e| labels(e)["stage"] == "CommandFinished").unwrap();
        let fields: std::collections::HashMap<_, _> = finished["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p[0].as_str().unwrap(), p[1].as_f64().unwrap()))
            .collect();
        assert_eq!(fields["elapsed_us"], 43000.);
        assert_eq!(fields["kind_mask"], 17.);
        assert_eq!(fields["files"], 1.);
        let json = serde_json::to_string(&logs).unwrap();
        for forbidden in [
            "private drag payload",
            "private-filename",
            "/private/drag-source",
            &me.to_hex(),
            &peer.to_hex(),
        ] {
            assert!(!json.contains(forbidden));
        }
        // The typed Log observation is the storage envelope.
        let event = Raw::new("NativeInput", "LeftButton").export(&recorder);
        let _: Event = serde_json::from_slice(&serde_json::to_vec(&event).unwrap()).unwrap();
        assert!(!matches!(
            event,
            Event::Input {
                event: SafeInput::Motion { .. },
                ..
            }
        ));
        recorder.shutdown();
    }
    #[test]
    fn disabled_drag_diagnostics_do_not_enqueue_or_update_state() {
        let id = ClipId {
            origin: DeviceId([1; 32]),
            seq: 1,
        };
        let trace = InputTrace::new(false);
        trace.dnd_state("hosting", 1, None);
        trace.dnd_command(
            &DndCmd::Cancel {
                id,
                epoch: 1,
                physical: false,
            },
            "CommandQueued",
            None,
            None,
        );
        trace.dnd_event(&DndEvent::Ended {
            id,
            epoch: 1,
            dropped: false,
        });
        assert_eq!(trace.last_dnd_state.load(std::sync::atomic::Ordering::Relaxed), u8::MAX);
    }
}
