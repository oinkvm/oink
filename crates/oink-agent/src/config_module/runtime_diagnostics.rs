//! Immutable diagnostic observations of durable, applied and installed state.
//! This view grants no configuration authority and never blocks native effects.
use super::*;
use crate::diagnostics::{
    Anonymizer,
    layout::{self as diagnostic, ChangeKind, Inputs},
};

impl Owner {
    pub(crate) fn observe_layout(&mut self, sink: diagnostic::Sink) -> ApiResult<()> {
        if !self.layout_sink.as_ref().is_some_and(|old| old.same(&sink)) {
            self.diagnostic_emitted = None;
        }
        self.layout_sink = Some(sink);
        self.refresh_diagnostics()
    }
    pub(crate) fn flush_layout_diagnostics(&mut self) -> ApiResult<()> {
        self.refresh_diagnostics()
    }
    pub(crate) fn refresh_diagnostics(&mut self) -> ApiResult<()> {
        let result = self.refresh_diagnostic_inputs();
        if result.is_err()
            && let Some(sink) = &self.layout_sink
        {
            sink.dropped();
        }
        result
    }
    fn refresh_diagnostic_inputs(&mut self) -> ApiResult<()> {
        let group = self.state.roster().anchor().group;
        let anon = Anonymizer::new(group);
        let applied = self.shared.applied.load();
        let previous = self.shared.diagnostic_layout.load();
        let previous = previous.as_ref().as_ref();
        let register = self.state.layout().map_err(internal)?;
        let value = dto::LayoutValue::from_raw(register.value().map_err(internal)?).map_err(internal)?;
        let saved = Arc::new(layout_wire::from_wire(&value).map_err(internal)?);
        let saved_members = self.state.roster().members().map(|m| m.device).collect::<Vec<_>>();
        let installed = self.router_compilation.clone().unwrap_or_else(|| {
            let router = self.shared.router.load();
            diagnostic::RouterCompilation {
                group: router.group,
                version: router.version.clone(),
                sequence: 0,
                stage: diagnostic::CompilationStage::Prepared,
                topology: router.topology.clone(),
                source_layout: router.layout.clone(),
                input_online: None,
            }
        });
        let devices: BTreeSet<_> = saved_members
            .iter()
            .chain(applied.members.iter())
            .copied()
            .chain(installed.source_devices())
            .collect();
        let mut observations = Vec::new();
        let mut owner_preferences = Vec::new();
        let mut owners = (*applied.owners).clone();
        for device in devices {
            let facts = if device == self.identity.id() {
                Some(Arc::new(self.local.value().clone()))
            } else {
                self.facts.get(&device).cloned().or_else(|| {
                    previous
                        .and_then(|p| p.devices.iter().find(|d| d.device == device))
                        .and_then(|d| d.facts.clone())
                })
            };
            if let Some(preferences) = facts
                .as_ref()
                .map(|f| f.preferences())
                .transpose()
                .map_err(internal)?
                .flatten()
            {
                if let Ok(projected) = layout_wire::owner_preferences(&preferences.0) {
                    owners.insert(device, projected);
                }
                owner_preferences.push((device, preferences.0));
            }
            observations.push(diagnostic::DeviceInputs {
                device,
                saved_member: saved_members.contains(&device),
                applied_member: applied.members.contains(&device),
                online: device == self.identity.id()
                    || self
                        .sessions
                        .values()
                        .any(|s| s.ready && s.peer_boot.device_id == device),
                facts,
            });
        }
        let pause = self
            .local
            .value()
            .pause()
            .map_err(internal)?
            .ok_or_else(|| internal("local pause unavailable"))?;
        let mut inputs = Inputs {
            group,
            saved,
            version: register.version().map_err(internal)?,
            roster_digest: self.state.roster().digest().map_err(internal)?,
            saved_members,
            applied_roster_digest: applied.roster_digest,
            applied_version: applied.version.clone(),
            source: value.source().map_err(internal)?,
            paused: pause.paused().map_err(internal)?,
            owners: Arc::new(owners),
            owner_preferences,
            local_pause: pause,
            devices: observations,
            compiled: installed,
            sequence: previous.map_or(1, |p| p.sequence),
        };
        let projected = inputs.project(&anon).map_err(internal)?;
        let changed = match previous {
            Some(before) => {
                before.group != group
                    || !projected
                        .changes_from(&before.project(&anon).map_err(internal)?)
                        .is_empty()
            }
            None => true,
        };
        if changed && let Some(before) = previous {
            inputs.sequence = before.sequence.saturating_add(1);
        }
        self.shared.diagnostic_layout.publish(Some(inputs.clone()));
        self.shared.diagnostic.publish(serde_json::json!({"schema":1,"member_count":inputs.saved_members.len(),"ready_links":self.sessions.values().filter(|s|s.ready).count(),"layout_register_counter":inputs.version.counter().map_err(internal)?}));
        let Some(sink) = &self.layout_sink else { return Ok(()) };
        if !sink.active() {
            // Rekey closes the recording gate while installing its new alias
            // domain. Retain the old view through that transient handoff.
            if sink.explicitly_disabled() {
                self.diagnostic_emitted = None;
            }
            return Ok(());
        }
        // A group handoff is emitted only after the recorder adopts its new
        // alias domain. Keep raw immutable inputs until that ordered handoff.
        if !sink.accepts(group) {
            return Ok(());
        }
        let after = inputs.project(&anon).map_err(internal)?;
        let before = self
            .diagnostic_emitted
            .as_ref()
            .map(|i| i.project(&anon))
            .transpose()
            .map_err(internal)?;
        let mut kinds = before
            .as_ref()
            .map_or_else(|| vec![ChangeKind::Initialized], |before| after.changes_from(before));
        if self.diagnostic_emitted.as_ref().is_some_and(|i| i.group != group) {
            kinds.insert(0, ChangeKind::GroupChanged);
        }
        if kinds.is_empty() {
            return Ok(());
        }
        sink.record(group, diagnostic::LayoutChange { kinds, before, after });
        self.diagnostic_emitted = Some(inputs);
        Ok(())
    }
}
