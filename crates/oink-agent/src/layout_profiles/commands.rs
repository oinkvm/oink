//! IPC commands. The gate serializes catalog edits and user operations; the
//! owner gate separately fences platform transactions against device edits.
use super::{
    model, native,
    state::{preference_token, unavailable},
    types::*,
    wire,
};
use crate::{
    api::{ApiEvent, DeviceKey},
    node::{Node, lock},
};
use oink_foundation::Point;
use oink_protocol::v1::{RawValue as V, dto, schema::WireValue};
use std::collections::BTreeSet;

fn stale() -> ProfileError {
    ProfileError::new(ErrorCode::StaleState, "The layout changed; refresh and retry")
}
fn check(expected: &str, actual: &str) -> Result<()> {
    if expected == actual { Ok(()) } else { Err(stale()) }
}
fn snapshot(state: &LayoutProfilesState) -> Result<ArrangementSnapshot> {
    state.snapshot.clone().ok_or_else(|| unavailable("Layout is not ready"))
}
#[derive(Clone)]
struct ManualLayoutDelta {
    placements: Vec<(crate::api::Placement, crate::api::Placement)>,
    size_sources: Vec<(oink_display::layout::ScreenId, oink_display::layout::SizeSource)>,
    topology: Option<oink_display::layout::LayoutSettings>,
}
impl ManualLayoutDelta {
    fn between(before: &oink_display::layout::Layout, after: &oink_display::layout::Layout) -> Self {
        let devices = after.screens.iter().map(|s| s.id.device).collect::<BTreeSet<_>>();
        let placements = devices
            .into_iter()
            .filter_map(|device| {
                let (old, old_scale) = before.device_transform(device)?;
                let (new, new_scale) = after.device_transform(device)?;
                (old != new || old_scale != new_scale).then_some((
                    crate::api::Placement {
                        device: device.into(),
                        origin: old,
                        scale: old_scale,
                    },
                    crate::api::Placement {
                        device: device.into(),
                        origin: new,
                        scale: new_scale,
                    },
                ))
            })
            .collect();
        Self {
            placements,
            size_sources: after
                .screens
                .iter()
                .filter(|s| before.screen(&s.id).is_some_and(|old| old.size_source != s.size_source))
                .map(|s| (s.id.clone(), s.size_source))
                .collect(),
            topology: (before.settings != after.settings).then(|| after.settings.clone()),
        }
    }
    fn apply(&self, layout: &mut oink_display::layout::Layout) -> bool {
        if self
            .placements
            .iter()
            .any(|(p, _)| layout.device_transform(p.device.0).is_none())
            || self.size_sources.iter().any(|(id, _)| layout.screen(id).is_none())
        {
            return false;
        }
        for (old, new) in &self.placements {
            let (mut origin, mut scale) = layout.device_transform(old.device.0).expect("checked device");
            patch_transform(&mut origin, &mut scale, old, new);
            // A manual transaction can be only a translation. Carry size-source
            // metadata separately, exactly when the original edit changed it.
            layout.place_device(old.device.0, origin, scale, false);
        }
        for (id, source) in &self.size_sources {
            layout
                .screens
                .iter_mut()
                .find(|s| &s.id == id)
                .expect("checked screen")
                .size_source = *source;
        }
        if let Some(topology) = &self.topology {
            layout.settings = topology.clone();
        }
        true
    }
}
fn patch_transform(origin: &mut Point, scale: &mut f64, old: &crate::api::Placement, new: &crate::api::Placement) {
    if old.origin.x != new.origin.x {
        origin.x = new.origin.x;
    }
    if old.origin.y != new.origin.y {
        origin.y = new.origin.y;
    }
    if old.scale != new.scale {
        *scale = new.scale;
    }
}
impl Node {
    /// Explicit manual boundary for legacy UI and projection gestures. Facts,
    /// hotplug, reconnect and automatic repair use the config actor directly.
    pub(crate) fn commit_manual_layout(
        &self,
        edit: impl FnOnce(&mut oink_display::layout::Layout) -> crate::api::ApiResult<()>,
    ) -> crate::api::ApiResult<()> {
        self.commit_manual_layout_inner(edit, false)
    }
    pub(crate) fn commit_manual_layout_exact(
        &self,
        edit: impl FnOnce(&mut oink_display::layout::Layout) -> crate::api::ApiResult<()>,
    ) -> crate::api::ApiResult<()> {
        self.commit_manual_layout_inner(edit, true)
    }
    fn commit_manual_layout_inner(
        &self,
        edit: impl FnOnce(&mut oink_display::layout::Layout) -> crate::api::ApiResult<()>,
        exact: bool,
    ) -> crate::api::ApiResult<()> {
        let group = self.link_config()?;
        let original = (*group.applied().layout).clone();
        let mut target = original.clone();
        edit(&mut target)?;
        if target == original {
            return Ok(());
        }
        let delta = ManualLayoutDelta::between(&original, &target);
        let intended_delta = delta.clone();
        self.commit_manual_change(
            move |before| {
                let mut intended = before.clone();
                for (old, new) in &intended_delta.placements {
                    let p = intended
                        .placements
                        .iter_mut()
                        .find(|p| p.device == old.device)
                        .ok_or_else(|| profile_api_error(stale()))?;
                    patch_transform(&mut p.origin, &mut p.scale, old, new);
                }
                if let Some(topology) = intended_delta.topology {
                    intended.topology = topology;
                }
                model::validate_snapshot(&intended).map_err(profile_api_error)?;
                Ok(intended)
            },
            || {
                let matched = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let flag = matched.clone();
                group.edit_layout(
                    Box::new(move |current| {
                        // Legacy placement APIs have no expected token: apply only the
                        // requested fields to the actor's latest factual geometry.
                        // Projection gestures retain their original exact-state fence.
                        if (!exact || *current == original) && delta.apply(current) {
                            flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                    }),
                    dto::LayoutSource::Manual,
                )?;
                if !matched.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(profile_api_error(stale()));
                }
                Ok(())
            },
        )
    }
    pub(crate) fn commit_manual_visibility(
        &self,
        device: DeviceKey,
        screen: Option<&str>,
        hidden: bool,
        change: impl FnOnce() -> crate::api::ApiResult<()>,
    ) -> crate::api::ApiResult<()> {
        self.commit_manual_change(
            |before| {
                let mut intended = before.clone();
                if let Some(display) = screen {
                    let bit = intended
                        .disabled
                        .iter_mut()
                        .find(|b| b.screen.device == device && b.screen.display == display)
                        .ok_or_else(|| profile_api_error(unavailable("Screen unavailable")))?;
                    bit.disabled = hidden;
                } else {
                    model::set_screen_shared(&mut intended, device, !hidden);
                }
                Ok(intended)
            },
            change,
        )
    }
    fn commit_manual_change(
        &self,
        prepare: impl FnOnce(&ArrangementSnapshot) -> crate::api::ApiResult<ArrangementSnapshot>,
        change: impl FnOnce() -> crate::api::ApiResult<()>,
    ) -> crate::api::ApiResult<()> {
        let _command = self
            .inner
            .profiles
            .command_gate
            .try_lock()
            .map_err(|_| profile_api_error(ProfileError::new(ErrorCode::Busy, "Layout operation in progress")))?;
        let _gate = lock(&self.inner.profiles.gate);
        let state = self.profile_snapshot().map_err(profile_api_error)?;
        self.current_profile_catalog().map_err(profile_api_error)?;
        if state
            .operation
            .as_ref()
            .is_some_and(|op| matches!(op.status, OperationStatus::Running | OperationStatus::OutcomeUnknown))
        {
            return Err(profile_api_error(ProfileError::new(
                ErrorCode::Busy,
                "Resolve the pending layout operation first",
            )));
        }
        let before = snapshot(&state).map_err(profile_api_error)?;
        let mut intended = prepare(&before)?;
        intended.active_profile_id = before.active_profile_id.clone();
        let mut op = LayoutOperation {
            id: model::new_id().map_err(profile_api_error)?,
            kind: OperationKind::Edit,
            status: OperationStatus::Running,
            before: before.clone(),
            after: None,
            native_restore_devices: Vec::new(),
            adjustments: Vec::new(),
            owners: Vec::new(),
            history_required: false,
            error: None,
            manual_save: Some(super::autosave::ManualSave {
                profile_id: before.active_profile_id,
                intended,
                applied: false,
            }),
        };
        self.operation_event(op.clone()).map_err(profile_api_error)?;
        let result = change();
        op.after = Some(snapshot(&self.profile_snapshot().map_err(profile_api_error)?).map_err(profile_api_error)?);
        if let Err(error) = &result {
            op.error = Some(unavailable(error));
        }
        finish(&mut op);
        self.operation_event(op).map_err(profile_api_error)?;
        result
    }
    fn current_profile_catalog(&self) -> Result<model::Catalog> {
        let catalog = lock(&self.inner.profiles.data)
            .catalog
            .clone()
            .ok_or_else(|| unavailable("Catalog unavailable"))?;
        if Some(catalog.group.0) != self.group_id() {
            return Err(ProfileError::new(
                ErrorCode::Busy,
                "Waiting for the target group catalog",
            ));
        }
        Ok(catalog)
    }
    pub(crate) fn layout_profile_command(&self, command: Command) -> serde_json::Value {
        match self.run_profile_command(command) {
            Ok(v) => v,
            Err(e) => error_reply(e),
        }
    }
    fn run_profile_command(&self, command: Command) -> Result<serde_json::Value> {
        if matches!(command, Command::Query) {
            return json(self.profile_snapshot()?);
        }
        if let Command::Operation(v) = &command {
            self.poll_profile_operations();
            let state = self.profile_snapshot()?;
            let operation = lock(&self.inner.profiles.data)
                .operations
                .iter()
                .find(|op| op.id == v.request_id)
                .cloned()
                .ok_or_else(|| ProfileError::new(ErrorCode::ProfileNotFound, "Operation is unavailable"))?;
            return json(OperationQuery { operation, state });
        }
        let _operation = self
            .inner
            .profiles
            .command_gate
            .try_lock()
            .map_err(|_| ProfileError::new(ErrorCode::Busy, "Another layout operation is in progress"))?;
        // Background catalog writes are short and must not make an otherwise
        // idle UI report Busy. Only a concurrent user operation gets Busy.
        let _gate = self
            .inner
            .profiles
            .gate
            .lock()
            .map_err(|_| ProfileError::new(ErrorCode::Busy, "Profile owner unavailable"))?;
        let digest = model::hash(&command);
        if let Some(id) = command.request_id() {
            if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
                return Err(ProfileError::new(ErrorCode::InvalidGeometry, "Invalid request ID"));
            }
            if let Some((_, old, response)) = lock(&self.inner.profiles.data)
                .replies
                .iter()
                .find(|(key, _, _)| key == id)
            {
                return if *old == digest {
                    Ok(response.clone())
                } else {
                    Err(stale())
                };
            }
        }
        let request_id = command.request_id().map(str::to_owned);
        let state = self.profile_snapshot()?;
        if let Some(id) = &request_id {
            let data = lock(&self.inner.profiles.data);
            if let Some(op) = data.operations.iter().find(|op| &op.id == id) {
                if data.operation_digests.get(id) != Some(&digest) {
                    return Err(stale());
                }
                return json(OperationReply {
                    operation_id: op.id.clone(),
                    state: state.clone(),
                });
            }
        }
        if !matches!(command, Command::Operation(_) | Command::Preview(_))
            && state
                .operation
                .as_ref()
                .is_some_and(|op| matches!(op.status, OperationStatus::Running | OperationStatus::OutcomeUnknown))
        {
            return Err(ProfileError::new(
                ErrorCode::Busy,
                "Resolve the pending layout operation first",
            ));
        }
        let current = snapshot(&state)?;
        let catalog = self.current_profile_catalog()?;
        let now = crate::layout_sync::now_ms();
        let reply = match command {
            Command::Query => unreachable!(),
            Command::Create(v) => {
                check(&v.expected_token, &state.token)?;
                self.persist_catalog(catalog.save(
                    self.id().into(),
                    &current,
                    self.observe_profiles()?.device_state,
                    Some(&v.name),
                    now,
                )?)?;
                json(self.profile_snapshot()?)?
            }
            Command::Rename(v) => {
                check(&v.expected_catalog_token, &state.catalog_token)?;
                self.persist_catalog(catalog.rename(self.id().into(), &v.profile_id, &v.name)?)?;
                json(self.profile_snapshot()?)?
            }
            Command::Delete(v) => {
                check(&v.expected_catalog_token, &state.catalog_token)?;
                self.persist_catalog(catalog.delete(
                    self.id().into(),
                    &v.profile_id,
                    &current,
                    self.observe_profiles()?.device_state,
                    now,
                )?)?;
                json(self.profile_snapshot()?)?
            }
            Command::Setting(v) => {
                let device = state
                    .devices
                    .iter()
                    .find(|d| d.device == v.device)
                    .ok_or_else(|| ProfileError::new(ErrorCode::NotMember, "Device left the group"))?;
                check(&v.expected_preference_token, &device.preference_token)?;
                if device.availability == Availability::Offline {
                    return Err(ProfileError::new(ErrorCode::OwnerOffline, "Device is offline").device(v.device));
                }
                if device.availability == Availability::Unsupported {
                    return Err(
                        ProfileError::new(ErrorCode::Unsupported, "Peer does not support system arrangement")
                            .device(v.device),
                    );
                }
                let facts = self
                    .device_facts(v.device.0)
                    .ok_or_else(|| unavailable("Owner facts unavailable"))?;
                check(&v.expected_preference_token, &preference_token(&facts))?;
                self.edit_device_option(v.device, dto::LocalOption::SyncSystemArrangement, v.enabled.write())
                    .map_err(unavailable)?;
                json(self.profile_snapshot()?)?
            }
            Command::Load(v) => {
                check(&v.expected_token, &state.token)?;
                let mut plan = model::restore(&catalog.profile(&v.profile_id)?.content, &current, &state.devices)?;
                plan.snapshot.active_profile_id = v.profile_id.clone();
                self.persist_catalog(catalog.activate(self.id().into(), &v.profile_id, true, now)?)?;
                json(self.apply_profile_plan(
                    v.request_id,
                    digest.clone(),
                    OperationKind::Load,
                    current,
                    plan,
                    false,
                )?)?
            }
            Command::Preview(v) => json(self.preview_arrangement(v, &state)?)?,
            Command::Reset(v) => {
                check(&v.expected_token, &state.token)?;
                let devices = current
                    .placements
                    .iter()
                    .map(|p| {
                        let displays = self
                            .device_facts(p.device.0)
                            .and_then(|f| f.displays().ok())
                            .map(|facts| facts.0.iter().filter_map(crate::agent_api::display_fact).collect())
                            .ok_or_else(|| unavailable("Display facts are unavailable"))?;
                        Ok((p.device.0, displays))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let initial = oink_display::layout::Layout::auto(&devices);
                let mut target = current.clone();
                for p in &mut target.placements {
                    let (origin, scale) = initial
                        .device_transform(p.device.0)
                        .ok_or_else(|| unavailable("Initial placement unavailable"))?;
                    p.origin = origin;
                    p.scale = scale;
                }
                model::validate_snapshot(&target)?;
                json(self.apply_profile_plan(
                    v.request_id,
                    digest.clone(),
                    OperationKind::Reset,
                    current,
                    model::RestorePlan {
                        snapshot: target,
                        native_devices: Vec::new(),
                        adjustments: Vec::new(),
                    },
                    true,
                )?)?
            }
            Command::Commit(v) => {
                check(&v.expected_token, &state.token)?;
                let mut target = current.clone();
                target.placements = v.placements;
                let mut patched = BTreeSet::new();
                for bit in v.disabled {
                    if !patched.insert((bit.screen.device, bit.screen.display.clone())) {
                        return Err(ProfileError::new(
                            ErrorCode::InvalidGeometry,
                            "Duplicate visibility patch",
                        ));
                    }
                    if state
                        .devices
                        .iter()
                        .any(|d| d.device == bit.screen.device && d.availability == Availability::Offline)
                    {
                        return Err(
                            ProfileError::new(ErrorCode::OwnerOffline, "Cannot edit offline visibility")
                                .device(bit.screen.device),
                        );
                    }
                    let old = target
                        .disabled
                        .iter_mut()
                        .find(|b| b.screen == bit.screen)
                        .ok_or_else(|| ProfileError::new(ErrorCode::InvalidGeometry, "Unknown visibility screen"))?;
                    *old = bit;
                }
                let mut shared = BTreeSet::new();
                for patch in v.screen_shared {
                    if !shared.insert(patch.device) || !target.placements.iter().any(|p| p.device == patch.device) {
                        return Err(ProfileError::new(
                            ErrorCode::InvalidGeometry,
                            "Invalid device visibility patch",
                        ));
                    }
                    if state
                        .devices
                        .iter()
                        .any(|d| d.device == patch.device && d.availability == Availability::Offline)
                    {
                        return Err(
                            ProfileError::new(ErrorCode::OwnerOffline, "Cannot edit offline visibility")
                                .device(patch.device),
                        );
                    }
                    model::set_screen_shared(&mut target, patch.device, patch.shared);
                }
                let mut native_devices = Vec::new();
                let mut adjustments = Vec::new();
                for token in &v.native_plan_tokens {
                    let preview = lock(&self.inner.profiles.data)
                        .previews
                        .get(token)
                        .cloned()
                        .ok_or_else(stale)?;
                    check(&preview.0, &state.token)?;
                    if native_devices.contains(&preview.1.native.device) {
                        return Err(stale());
                    }
                    let n = target
                        .native
                        .iter_mut()
                        .find(|n| n.device == preview.1.native.device)
                        .ok_or_else(stale)?;
                    *n = preview.1.native;
                    native_devices.push(n.device);
                    adjustments.extend(preview.1.adjustments);
                }
                model::validate_snapshot(&target)?;
                if target.placements.iter().map(|p| p.device).collect::<BTreeSet<_>>()
                    != current.placements.iter().map(|p| p.device).collect()
                {
                    return Err(stale());
                }
                json(self.apply_profile_plan(
                    v.request_id,
                    digest.clone(),
                    OperationKind::Edit,
                    current,
                    model::RestorePlan {
                        snapshot: target,
                        native_devices,
                        adjustments,
                    },
                    true,
                )?)?
            }
            Command::Replay(v) => {
                check(&v.expected_token, &state.token)?;
                let mut target = v.target;
                if v.original_kind != HistoryKind::Load {
                    target.active_profile_id = current.active_profile_id.clone();
                }
                if catalog.profile(&target.active_profile_id).is_err() {
                    target.active_profile_id = current.active_profile_id.clone();
                }
                let mut plan = model::replay(&v.from, &target, &current, &state.devices, &v.native_restore_devices)?;
                plan.snapshot.active_profile_id = target.active_profile_id.clone();
                self.persist_catalog(catalog.activate(
                    self.id().into(),
                    &target.active_profile_id,
                    v.original_kind == HistoryKind::Load,
                    now,
                )?)?;
                json(self.apply_profile_plan(
                    v.request_id,
                    digest.clone(),
                    OperationKind::Replay,
                    current,
                    plan,
                    v.original_kind != HistoryKind::Load,
                )?)?
            }
            Command::Operation(_) => unreachable!("operation query handled before mutation gate"),
        };
        if let Some(id) = request_id {
            let mut data = lock(&self.inner.profiles.data);
            data.replies.push_back((id, digest, reply.clone()));
            while data.replies.len() > 256 {
                data.replies.pop_front();
            }
        }
        self.refresh_layout_profiles();
        Ok(reply)
    }
    fn preview_arrangement(&self, v: PreviewRequest, state: &LayoutProfilesState) -> Result<PreviewReply> {
        check(&v.expected_token, &state.token)?;
        let device = state.devices.iter().find(|d| d.device == v.device).ok_or_else(stale)?;
        if !device.can_edit_screens {
            return Err(ProfileError::new(
                if device.availability == Availability::Offline {
                    ErrorCode::OwnerOffline
                } else if effective_system_sync(device.sync_system_arrangement, true) {
                    ErrorCode::Unsupported
                } else {
                    ErrorCode::SettingDisabled
                },
                "Native screen editing is unavailable",
            )
            .device(v.device));
        }
        let mut current = snapshot(state)?;
        let p = current
            .placements
            .iter()
            .find(|p| p.device == v.device)
            .ok_or_else(stale)?
            .clone();
        let n = current
            .native
            .iter_mut()
            .find(|n| n.device == v.device)
            .ok_or_else(stale)?;
        let desired = Point::new(
            ((v.desired_canvas_origin.x - p.origin.x) / p.scale).round(),
            ((v.desired_canvas_origin.y - p.origin.y) / p.scale).round(),
        );
        *n = model::snap_native(n, &v.moved_screen, desired)?;
        // Main-display origin must remain zero. Moving it in the canvas changes
        // every other native origin and shifts the computer transform equally.
        let anchor = n
            .screens
            .iter()
            .find(|s| s.screen == n.primary)
            .ok_or_else(stale)?
            .origin;
        for s in &mut n.screens {
            s.origin.x -= anchor.x;
            s.origin.y -= anchor.y;
        }
        let native = n.clone();
        let placement = current
            .placements
            .iter_mut()
            .find(|p| p.device == v.device)
            .expect("placement");
        placement.origin.x += anchor.x * p.scale;
        placement.origin.y += anchor.y * p.scale;
        let mut order = vec![v.device];
        order.extend(current.placements.iter().map(|p| p.device).filter(|d| *d != v.device));
        let mut adjustments = Vec::new();
        model::repair_collisions(&mut current, &order, &mut adjustments)?;
        let plan_token = model::hash(&(&state.token, &native, &current.placements));
        let reply = PreviewReply {
            sequence: v.sequence,
            native,
            placements: current.placements,
            plan_token: plan_token.clone(),
            adjustments,
        };
        let mut data = lock(&self.inner.profiles.data);
        if data.previews.len() >= 128 {
            data.previews.clear();
        }
        data.previews.insert(plan_token, (state.token.clone(), reply.clone()));
        Ok(reply)
    }
    fn operation_event(&self, mut operation: LayoutOperation) -> Result<()> {
        {
            let mut data = lock(&self.inner.profiles.data);
            let mut catalog = data.catalog.clone().expect("catalog initialized");
            Self::autosave_operation(&mut catalog, &mut operation, self.id().into())?;
            let mut operations = data.operations.clone();
            if let Some(old) = operations.iter_mut().find(|old| old.id == operation.id) {
                *old = operation.clone();
            } else {
                operations.push_back(operation.clone());
            }
            while operations.len() > 32
                && operations
                    .front()
                    .is_some_and(|o| !matches!(o.status, OperationStatus::Running | OperationStatus::OutcomeUnknown))
            {
                if let Some(old) = operations.pop_front() {
                    data.operation_digests.remove(&old.id);
                    data.unknown.remove(&old.id);
                }
            }
            let receipts = super::store::Operations {
                operations: operations.clone(),
                digests: data.operation_digests.clone(),
                unknown: data
                    .unknown
                    .iter()
                    .flat_map(|(id, requests)| {
                        requests.iter().map(move |(device, request)| {
                            use oink_protocol::v1::schema::WireDto;
                            Ok((
                                id.clone(),
                                *device,
                                request.encode(model::MAX_BYTES).map_err(wire::invalid)?,
                            ))
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            };
            // One atomic envelope publishes the autosaved catalog and completed
            // receipt together. Running receipts retain intent for crash recovery.
            super::store::save_with_operations(&self.inner.data_dir, &catalog, &receipts)?;
            data.catalog = Some(catalog);
            data.operations = operations;
        }
        self.inner.profiles.wake.notify_waiters();
        let state = self.profile_snapshot()?;
        self.emit(ApiEvent::LayoutProfileOperation(OperationEvent {
            epoch: state.epoch.clone(),
            sequence: state.sequence.clone(),
            operation,
            state: state.clone(),
        }));
        self.emit(ApiEvent::LayoutProfiles(state));
        Ok(())
    }
    pub(super) fn autosave_operation(
        catalog: &mut model::Catalog,
        op: &mut LayoutOperation,
        writer: DeviceKey,
    ) -> Result<()> {
        if matches!(op.status, OperationStatus::Running | OperationStatus::OutcomeUnknown) {
            return Ok(());
        }
        if let (Some(save), Some(after)) = (&mut op.manual_save, &op.after)
            && !save.applied
        {
            *catalog = catalog.autosave_manual(
                writer,
                &save.profile_id,
                &op.before,
                &save.intended,
                after,
                crate::layout_sync::now_ms(),
            )?;
            save.applied = true;
        }
        Ok(())
    }
    fn apply_profile_plan(
        &self,
        id: String,
        digest: String,
        kind: OperationKind,
        before: ArrangementSnapshot,
        plan: model::RestorePlan,
        manual: bool,
    ) -> Result<OperationReply> {
        lock(&self.inner.profiles.data)
            .operation_digests
            .insert(id.clone(), digest);
        let operation = LayoutOperation {
            id: id.clone(),
            kind,
            status: OperationStatus::Running,
            before: before.clone(),
            after: None,
            native_restore_devices: plan.native_devices.clone(),
            adjustments: plan.adjustments.clone(),
            owners: Vec::new(),
            history_required: false,
            manual_save: manual.then(|| super::autosave::ManualSave {
                profile_id: before.active_profile_id.clone(),
                intended: plan.snapshot.clone(),
                applied: false,
            }),
            error: None,
        };
        self.operation_event(operation.clone())?;
        let accepted = OperationReply {
            operation_id: id.clone(),
            state: self.profile_snapshot()?,
        };
        let node = self.clone();
        self.inner.rt.spawn_blocking(move || {
            let _gate = lock(&node.inner.profiles.gate);
            if let Err(error) = node.finish_profile_plan(id.clone(), kind, before, plan, operation) {
                let operation = lock(&node.inner.profiles.data)
                    .operations
                    .iter()
                    .find(|op| op.id == id)
                    .cloned();
                if let Some(mut operation) = operation {
                    operation.error = Some(error);
                    operation.after = node.profile_snapshot().ok().and_then(|s| s.snapshot);
                    finish(&mut operation);
                    let _ = node.operation_event(operation);
                }
            }
        });
        Ok(accepted)
    }
    fn finish_profile_plan(
        &self,
        id: String,
        kind: OperationKind,
        before: ArrangementSnapshot,
        plan: model::RestorePlan,
        mut operation: LayoutOperation,
    ) -> Result<OperationReply> {
        let observed = self.observe_profiles()?;
        for device in &observed.devices {
            let bits = plan
                .snapshot
                .disabled
                .iter()
                .filter(|b| b.screen.device == device.device && !before.disabled.contains(b))
                .cloned()
                .collect::<Vec<_>>();
            let native = plan
                .native_devices
                .contains(&device.device)
                .then(|| plan.snapshot.native.iter().find(|n| n.device == device.device))
                .flatten();
            let sharing = model::screen_shared(&plan.snapshot, device.device);
            let sharing_changed = sharing != model::screen_shared(&before, device.device);
            if bits.is_empty() && native.is_none() && !sharing_changed {
                continue;
            }
            if device.availability == Availability::Offline {
                operation.owners.push(OwnerProgress {
                    device: device.device,
                    phase: OwnerPhase::SkippedOffline,
                    error: None,
                });
                continue;
            }
            operation.owners.push(OwnerProgress {
                device: device.device,
                phase: OwnerPhase::Applying,
                error: None,
            });
            self.operation_event(operation.clone())?;
            // Apply the idempotent sharing preference before native work: an
            // unknown native receipt must not leave a later visibility patch
            // queued for recovery or falsely report that patch as applied.
            let visibility = if sharing_changed {
                self.edit_device_option(device.device, dto::LocalOption::ScreenShared, V::Bool(sharing))
                    .map_err(unavailable)
            } else {
                Ok(())
            };
            let result = visibility.and_then(|()| {
                if bits.is_empty() && native.is_none() {
                    Ok(())
                } else if native.is_none()
                    && matches!(
                        device.availability,
                        Availability::Unsupported | Availability::Unavailable
                    )
                {
                    // Released owners understand screen visibility option 7. Never
                    // send a new arrangement message to those peers.
                    self.edit_device_option(
                        device.device,
                        dto::LocalOption::ScreenPreferences,
                        native::visibility_patch(&bits),
                    )
                    .map_err(unavailable)
                } else {
                    self.make_arrangement_request(&id, device.device, native, &bits)
                        .and_then(|request| {
                            lock(&self.inner.profiles.data)
                                .unknown
                                .entry(id.clone())
                                .or_default()
                                .push((device.device, request.clone()));
                            self.persist_profile_operations()?;
                            let result = self.request_arrangement(device.device, request);
                            let unknown = result.as_ref().is_err_and(|e| e.code == ErrorCode::OutcomeUnknown)
                                || result.as_ref().is_ok_and(|r| r.outcome() == Ok(native::UNKNOWN));
                            if !unknown && let Some(requests) = lock(&self.inner.profiles.data).unknown.get_mut(&id) {
                                requests.retain(|(owner, _)| *owner != device.device);
                            }
                            if let (Some(target), Ok(reply)) = (native, &result)
                                && reply.outcome() == Ok(native::APPLIED)
                                && let Some(actual) =
                                    reply.native().ok().flatten().and_then(|n| wire::read_native(n).ok())
                                && !model::same_native(target, &actual)
                            {
                                operation.adjustments.push(Adjustment {
                                    code: AdjustmentCode::NativeSnapped,
                                    device: device.device,
                                    screen: None,
                                });
                            }
                            result.and_then(owner_result)
                        })
                }
            });
            let owner = operation.owners.last_mut().expect("owner progress");
            match result {
                Ok(()) => owner.phase = OwnerPhase::Applied,
                Err(e) => {
                    owner.phase = if e.code == ErrorCode::OutcomeUnknown {
                        OwnerPhase::Unknown
                    } else {
                        OwnerPhase::Failed
                    };
                    owner.error = Some(e.device(device.device));
                }
            }
        }
        let target = plan.snapshot;
        let online = observed.device_state.online_devices;
        let result = self
            .link_config()
            .map_err(unavailable)?
            .edit_layout(
                Box::new(move |layout| {
                    layout.settings = target.topology;
                    // Offline geometry is retained drawing data. Online native geometry
                    // comes exclusively from each owner's observed ConfigFacts/layout.
                    for s in &mut layout.screens {
                        if !online.contains(&DeviceKey(s.id.device))
                            && let Some(n) = target
                                .native
                                .iter()
                                .find(|n| n.device.0 == s.id.device)
                                .and_then(|n| n.screens.iter().find(|n| n.screen.display == s.id.display))
                        {
                            s.local.x = n.origin.x;
                            s.local.y = n.origin.y;
                            s.local.w = n.size.w;
                            s.local.h = n.size.h;
                        }
                    }
                    for p in target.placements {
                        layout.place_device(p.device.0, p.origin, p.scale, true);
                    }
                }),
                if kind == OperationKind::Reset {
                    dto::LayoutSource::Default
                } else {
                    dto::LayoutSource::Manual
                },
            )
            .map_err(unavailable);
        if let Err(e) = result {
            operation.error = Some(e);
        }
        operation.after = Some(snapshot(&self.profile_snapshot()?)?);
        finish(&mut operation);
        self.operation_event(operation)?;
        Ok(OperationReply {
            operation_id: id,
            state: self.profile_snapshot()?,
        })
    }
    pub(super) fn poll_profile_operations(&self) {
        if !lock(&self.inner.profiles.data)
            .operations
            .back()
            .is_some_and(|op| op.status == OperationStatus::OutcomeUnknown)
        {
            return;
        }
        let Ok(_command) = self.inner.profiles.command_gate.try_lock() else {
            return;
        };
        let Ok(_gate) = self.inner.profiles.gate.try_lock() else {
            return;
        };
        let id = lock(&self.inner.profiles.data)
            .operations
            .back()
            .filter(|op| op.status == OperationStatus::OutcomeUnknown)
            .map(|op| op.id.clone());
        if let Some(id) = id {
            let _ = self.resolve_profile_operation(&id);
        }
    }
    fn resolve_profile_operation(&self, id: &str) -> Result<()> {
        let requests = lock(&self.inner.profiles.data)
            .unknown
            .get(id)
            .cloned()
            .unwrap_or_default();
        if requests.is_empty() {
            return Ok(());
        }
        let mut op = lock(&self.inner.profiles.data)
            .operations
            .iter()
            .find(|op| op.id == id)
            .cloned()
            .ok_or_else(stale)?;
        let mut unresolved = Vec::new();
        for (device, mut request) in requests {
            request
                .edit(|raw| {
                    raw.insert(3, V::Bool(true));
                })
                .map_err(wire::invalid)?;
            let result = self.request_arrangement(device, request.clone()).and_then(owner_result);
            let owner = op.owners.iter_mut().find(|o| o.device == device).ok_or_else(stale)?;
            match result {
                Ok(()) => {
                    owner.phase = OwnerPhase::Applied;
                    owner.error = None;
                }
                Err(e) => {
                    if e.code == ErrorCode::OutcomeUnknown || e.code == ErrorCode::OwnerOffline {
                        unresolved.push((device, request));
                    } else {
                        owner.phase = OwnerPhase::Failed;
                        owner.error = Some(e);
                    }
                }
            }
        }
        lock(&self.inner.profiles.data).unknown.insert(id.into(), unresolved);
        op.after = Some(snapshot(&self.profile_snapshot()?)?);
        finish(&mut op);
        self.operation_event(op)
    }
}
fn owner_result(result: dto::LayoutArrangementResult) -> Result<()> {
    match result.outcome().map_err(wire::invalid)? {
        native::APPLIED => Ok(()),
        native::UNKNOWN => Err(ProfileError::new(
            ErrorCode::OutcomeUnknown,
            "Owner outcome is not yet known",
        )),
        _ => {
            let code = result
                .error()
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_value::<ErrorCode>(serde_json::Value::String(s.0)).ok())
                .unwrap_or(ErrorCode::OsApplyFailed);
            Err(ProfileError::new(code, "Owner could not apply the arrangement"))
        }
    }
}
fn finish(op: &mut LayoutOperation) {
    if !op.owners.iter().any(|o| o.phase == OwnerPhase::Unknown)
        && let Some(after) = &op.after
    {
        op.native_restore_devices.retain(|device| {
            let before = op.before.native.iter().find(|n| n.device == *device);
            let after = after.native.iter().find(|n| n.device == *device);
            match (before, after) {
                (Some(a), Some(b)) => !model::same_native(a, b),
                _ => false,
            }
        });
    }
    op.history_required = op
        .after
        .as_ref()
        .is_some_and(|after| model::history_changed(&op.before, after));
    op.status = if op.owners.iter().any(|o| o.phase == OwnerPhase::Unknown) {
        OperationStatus::OutcomeUnknown
    } else if op.error.is_some() || op.owners.iter().any(|o| o.phase == OwnerPhase::Failed) {
        if op.history_required {
            OperationStatus::Partial
        } else {
            OperationStatus::Failed
        }
    } else {
        OperationStatus::Applied
    };
}

fn profile_api_error(error: ProfileError) -> crate::api::ApiError {
    tracing::warn!(code = ?error.code, reason = %error.message, "manual layout commit refused");
    match error.code {
        ErrorCode::PersistenceFailed => crate::api::ApiError::PersistenceFailed,
        _ => crate::api::ApiError::InvalidState {
            message: error.to_string(),
        },
    }
}

#[cfg(test)]
mod manual_layout_delta_tests {
    use super::ManualLayoutDelta;
    use oink_display::layout::{Layout, ScreenId, ScreenPlacement, SizeSource};
    use oink_foundation::{DeviceId, Point, Rect};

    fn layout(source: SizeSource) -> Layout {
        Layout {
            screens: vec![ScreenPlacement {
                id: ScreenId::new(DeviceId([1; 32]), "screen"),
                name: "screen".into(),
                local: Rect::new(0., 0., 100., 100.),
                rect: Rect::new(0., 0., 1., 1.),
                size_source: source,
                primary: true,
                builtin: false,
            }],
            ..Layout::default()
        }
    }

    #[test]
    fn translation_preserves_size_source_and_matches_preview() {
        for source in [SizeSource::Reported, SizeSource::Estimated, SizeSource::Manual] {
            let before = layout(source);
            let device = before.screens[0].id.device;
            let preview = before.preview_move_device(device, Point::new(0., 0.25)).unwrap();
            let mut committed = before.clone();
            assert!(ManualLayoutDelta::between(&before, &preview).apply(&mut committed));
            assert_eq!(committed, preview);
        }
    }

    #[test]
    fn explicit_size_edit_preserves_manual_source_even_at_the_same_size() {
        for scale in [0.01, 0.02] {
            let before = layout(SizeSource::Reported);
            let mut edited = before.clone();
            assert!(edited.place_device(before.screens[0].id.device, Point::default(), scale, true));
            let mut committed = before.clone();
            assert!(ManualLayoutDelta::between(&before, &edited).apply(&mut committed));
            assert_eq!(committed, edited);
        }
    }
}
