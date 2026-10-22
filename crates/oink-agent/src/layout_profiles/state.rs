//! Project the actual layout and owner-authored visibility into the UI contract.
use super::{
    model::{self, Catalog, DeviceState},
    store,
    types::*,
};
use crate::{
    api::{DeviceKey, Placement, ScreenKey},
    node::{Node, lock},
};
use oink_foundation::{Point, Size};
use oink_protocol::v1::{
    dto,
    schema::{WireDto, WireValue},
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Mutex,
};

#[cfg(test)]
pub type SnapshotPause = (std::sync::Arc<std::sync::Barrier>, std::sync::Arc<std::sync::Barrier>);

#[derive(Debug, Default)]
pub struct State {
    #[cfg(test)]
    pub pause_snapshot_init: Mutex<Option<SnapshotPause>>,
    #[cfg(test)]
    pub catalog_publish_ready: Mutex<Option<std::sync::Arc<std::sync::Barrier>>>,
    #[cfg(test)]
    pub unavailable_observation: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    pub defer_joins: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    pub defer_catalogs: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    pub deferred_joins: std::sync::atomic::AtomicUsize,
    pub gate: Mutex<()>,
    pub command_gate: Mutex<()>,
    pub native: super::native::Runtime,
    pub data: Mutex<Data>,
    pub wake: tokio::sync::Notify,
}
#[derive(Debug, Default)]
pub struct Data {
    pub catalog: Option<Catalog>,
    pub last: Option<LayoutProfilesState>,
    pub sequence: u64,
    pub pending_layout: Option<dto::ConfigVersion>,
    pub operations: VecDeque<LayoutOperation>,
    pub operation_digests: BTreeMap<String, String>,
    pub unknown: BTreeMap<String, Vec<(DeviceKey, dto::LayoutArrangementRequest)>>,
    pub replies: VecDeque<(String, String, serde_json::Value)>,
    pub previews: BTreeMap<String, (String, PreviewReply)>,
}
#[derive(Debug)]
pub struct Observation {
    pub snapshot: ArrangementSnapshot,
    pub devices: Vec<DeviceArrangementState>,
    pub device_state: DeviceState,
}
pub fn unavailable(e: impl std::fmt::Display) -> ProfileError {
    ProfileError::new(ErrorCode::FactsUnavailable, e.to_string())
}
pub fn preference(facts: &dto::DeviceFacts, option: dto::LocalOption) -> Option<dto::DevicePreference> {
    facts
        .preferences()
        .ok()
        .flatten()?
        .0
        .into_iter()
        .find(|p| p.option() == Ok(option))
}
pub fn preference_token(facts: &dto::DeviceFacts) -> String {
    let boot = facts.boot().ok().map(|b| b.write());
    let p = preference(facts, dto::LocalOption::SyncSystemArrangement).map(|p| p.write());
    // Raw wire values aren't Serialize; canonical CBOR is the token input.
    let raw = [
        (1, boot.unwrap_or(oink_protocol::v1::RawValue::Null)),
        (2, p.unwrap_or(oink_protocol::v1::RawValue::Null)),
    ]
    .into();
    model::hash(&oink_protocol::v1::cbor::encode_map(&raw, 4096).unwrap_or_default())
}
impl Node {
    pub(super) fn observe_profiles(&self) -> Result<Observation> {
        #[cfg(test)]
        if self
            .inner
            .profiles
            .unavailable_observation
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(unavailable("Injected missing owner facts"));
        }
        let config = self.link_config().map_err(unavailable)?;
        let applied = config.applied();
        let table = config.members();
        let layout = &applied.layout;
        let mut snapshot = ArrangementSnapshot {
            screen_shared: Vec::new(),
            placements: Vec::new(),
            disabled: Vec::new(),
            native: Vec::new(),
            topology: layout.settings.clone(),
            active_profile_id: String::new(),
        };
        let mut devices = Vec::new();
        let mut online_devices = Default::default();
        let mut connected_screens = BTreeMap::new();
        for member in &table.members {
            let device = DeviceKey(member.device);
            let facts = member.facts.as_ref();
            let connected = facts
                .and_then(|f| f.displays().ok())
                .map(|d| d.0.into_iter().filter_map(|d| d.key().ok().map(|s| s.0)).collect());
            connected_screens.insert(device, connected);
            if member.online {
                std::collections::BTreeSet::insert(&mut online_devices, device);
            }
            let enabled = facts
                .and_then(|f| preference(f, dto::LocalOption::SyncSystemArrangement))
                .and_then(|p| p.value().ok())
                .and_then(|v| bool::read(&v).ok())
                .unwrap_or(false);
            let supported = member.is_self
                || config
                    .peer_agreement(member.device)
                    .is_some_and(|a| a.contains("oink.layout.arrangement/1"));
            let token = facts
                .and_then(|f| f.native_arrangement_token().ok().flatten())
                .map(|t| t.0);
            let availability = if !member.online {
                Availability::Offline
            } else if !supported {
                Availability::Unsupported
            } else if token.is_none() {
                Availability::Unavailable
            } else {
                Availability::Ready
            };
            devices.push(DeviceArrangementState {
                device,
                sync_system_arrangement: enabled,
                preference_token: facts.map(|f| preference_token(f)).unwrap_or_default(),
                native_token: token.clone(),
                availability,
                can_edit_screens: effective_system_sync(enabled, availability != Availability::Offline)
                    && availability == Availability::Ready,
            });
            let Some((origin, scale)) = layout.device_transform(member.device) else {
                continue;
            };
            let owners = applied
                .owners
                .get(&member.device)
                .ok_or_else(|| unavailable("Owner screen preferences are unavailable"))?;
            snapshot.placements.push(Placement { device, origin, scale });
            snapshot.screen_shared.push(DeviceSharing {
                device,
                shared: owners.screen_shared,
            });
            let screens = layout
                .screens
                .iter()
                .filter(|s| s.id.device == member.device)
                .collect::<Vec<_>>();
            let primary = screens
                .iter()
                .find(|s| s.primary)
                .or_else(|| screens.first())
                .expect("transform has screen");
            let primary = ScreenKey {
                device,
                display: primary.id.display.clone(),
            };
            let mut native = NativeState {
                device,
                token: token.unwrap_or_default(),
                primary,
                screens: Vec::new(),
            };
            for s in screens {
                let screen = ScreenKey {
                    device,
                    display: s.id.display.clone(),
                };
                snapshot.disabled.push(Disabled {
                    screen: screen.clone(),
                    disabled: !owners.screens.get(&s.id.display).is_none_or(|p| p.enabled),
                });
                native.screens.push(NativeScreen {
                    screen,
                    origin: Point::new(s.local.x, s.local.y),
                    size: Size::new(s.local.w, s.local.h),
                });
            }
            snapshot.native.push(native);
        }
        model::canonicalize(&mut snapshot);
        model::validate_snapshot(&snapshot)?;
        Ok(Observation {
            snapshot,
            devices,
            device_state: DeviceState {
                online_devices,
                connected_screens,
            },
        })
    }
    pub(super) fn profile_snapshot(&self) -> Result<LayoutProfilesState> {
        let mut observation = self.observe_profiles()?;
        let group = self
            .group_id()
            .ok_or_else(|| unavailable("Group is unavailable"))?
            .into();
        let mut data = lock(&self.inner.profiles.data);
        if data.catalog.as_ref().is_none_or(|c| c.group != group) {
            let catalog = select_catalog(
                store::load(&self.inner.data_dir, group)?,
                || super::join::awaiting_catalog(&self.inner.data_dir, group),
                || {
                    let c = Catalog::initial(
                        group,
                        self.id().into(),
                        &observation.snapshot,
                        observation.device_state.clone(),
                        crate::layout_sync::now_ms(),
                    )?;
                    store::save(&self.inner.data_dir, &c)?;
                    Ok(c)
                },
            )?;
            #[cfg(test)]
            if let Some((loaded, resume)) = lock(&self.inner.profiles.pause_snapshot_init).take() {
                loaded.wait();
                resume.wait();
            }
            let receipts = if catalog.group == group {
                store::load_operations(&self.inner.data_dir, group)?
            } else {
                store::Operations::default()
            };
            data.operations = receipts.operations;
            data.operation_digests = receipts.digests;
            data.replies.clear();
            data.previews.clear();
            data.unknown.clear();
            for (id, device, bytes) in receipts.unknown {
                let request =
                    dto::LayoutArrangementRequest::decode(&bytes, model::MAX_BYTES).map_err(super::wire::invalid)?;
                data.unknown.entry(id).or_default().push((device, request));
            }
            let mut actual = observation.snapshot.clone();
            actual.active_profile_id = catalog.active_profile_id.clone();
            for op in &mut data.operations {
                if op.status == OperationStatus::Running {
                    op.after = Some(actual.clone());
                    op.history_required = model::history_changed(&op.before, &actual);
                    for owner in &mut op.owners {
                        if owner.phase == OwnerPhase::Applying {
                            owner.phase = OwnerPhase::Unknown;
                        }
                    }
                    op.status = if op.owners.iter().any(|o| o.phase == OwnerPhase::Unknown) {
                        OperationStatus::OutcomeUnknown
                    } else if op.history_required {
                        OperationStatus::Partial
                    } else {
                        OperationStatus::Failed
                    };
                }
            }
            data.pending_layout = None;
            let mut recovered = catalog;
            for op in &mut data.operations {
                Self::autosave_operation(&mut recovered, op, self.id().into())?;
            }
            let receipts = store::Operations {
                operations: data.operations.clone(),
                digests: data.operation_digests.clone(),
                unknown: data
                    .unknown
                    .iter()
                    .flat_map(|(id, requests)| {
                        requests.iter().map(move |(device, request)| {
                            Ok((
                                id.clone(),
                                *device,
                                request.encode(model::MAX_BYTES).map_err(super::wire::invalid)?,
                            ))
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            };
            if recovered.group == group {
                store::save_with_operations(&self.inner.data_dir, &recovered, &receipts)?;
            }
            data.catalog = Some(recovered);
        }
        let applied_version = self.link_config().map_err(unavailable)?.applied().version.clone();
        if data.pending_layout.as_ref().is_some_and(|v| {
            oink_protocol::v1::retained::version_cmp(&applied_version, v).is_ok_and(|o| o != std::cmp::Ordering::Less)
        }) {
            data.pending_layout = None;
        }
        let c = data.catalog.as_ref().expect("catalog initialized");
        let syncing = data.pending_layout.is_some() || c.group != group;
        observation.snapshot.active_profile_id = c.active_profile_id.clone();
        let token = model::hash(&(
            c.version.token(),
            &observation.snapshot,
            &observation.devices,
            &observation.device_state,
        ));
        let operation = data.operations.back().cloned();
        let busy = operation
            .as_ref()
            .is_some_and(|op| matches!(op.status, OperationStatus::Running | OperationStatus::OutcomeUnknown));
        let mut state = LayoutProfilesState {
            epoch: model::hash(&self.inner.boot.nonce),
            sequence: data.sequence.to_string(),
            token,
            catalog_token: c.version.token(),
            readiness: if syncing { Readiness::Syncing } else { Readiness::Ready },
            profiles: c.summaries(&observation.device_state),
            active_profile_id: Some(c.active_profile_id.clone()),
            create_enabled: !busy && !syncing && c.profiles.len() < model::MAX_PROFILES,
            snapshot: Some(observation.snapshot),
            devices: observation.devices,
            pending: operation
                .as_ref()
                .filter(|op| matches!(op.status, OperationStatus::Running | OperationStatus::OutcomeUnknown))
                .map(|op| op.owners.clone())
                .unwrap_or_default(),
            operation,
        };
        if data.last.as_ref() != Some(&state) {
            data.sequence = data.sequence.saturating_add(1);
            state.sequence = data.sequence.to_string();
            data.last = Some(state.clone());
        }
        Ok(state)
    }
    pub(crate) fn refresh_layout_profiles(&self) {
        let before = lock(&self.inner.profiles.data).sequence;
        if let Ok(state) = self.profile_snapshot()
            && before != lock(&self.inner.profiles.data).sequence
        {
            self.emit(crate::api::ApiEvent::LayoutProfiles(state));
            self.inner.profiles.wake.notify_waiters();
        }
    }
    pub(super) fn persist_profile_operations(&self) -> Result<()> {
        let mut data = lock(&self.inner.profiles.data);
        while data.operations.len() > 32
            && data
                .operations
                .front()
                .is_some_and(|o| !matches!(o.status, OperationStatus::Running | OperationStatus::OutcomeUnknown))
        {
            if let Some(old) = data.operations.pop_front() {
                data.operation_digests.remove(&old.id);
                data.unknown.remove(&old.id);
            }
        }
        let Some(catalog) = &data.catalog else { return Ok(()) };
        let unknown = data
            .unknown
            .iter()
            .flat_map(|(id, requests)| {
                requests.iter().map(move |(device, request)| {
                    Ok((
                        id.clone(),
                        *device,
                        request.encode(model::MAX_BYTES).map_err(super::wire::invalid)?,
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        store::save_operations(
            &self.inner.data_dir,
            catalog.group,
            &store::Operations {
                operations: data.operations.clone(),
                digests: data.operation_digests.clone(),
                unknown,
            },
        )
    }
    pub(super) fn persist_catalog(&self, catalog: Catalog) -> Result<()> {
        #[cfg(test)]
        if let Some(ready) = lock(&self.inner.profiles.catalog_publish_ready).take() {
            ready.wait();
        }
        // Snapshot initialization also persists recovered receipts while holding
        // data. Fence the disk write with that same lock, so a paused initializer
        // cannot overwrite this catalog before its in-memory publication.
        let mut data = lock(&self.inner.profiles.data);
        store::save(&self.inner.data_dir, &catalog)?;
        if data.catalog.as_ref().is_some_and(|c| c.group != catalog.group) {
            data.operations.clear();
            data.operation_digests.clear();
            data.unknown.clear();
            data.replies.clear();
            data.previews.clear();
        }
        data.catalog = Some(catalog);
        self.inner.profiles.wake.notify_waiters();
        Ok(())
    }
}

// Keep filesystem reads injectable so a handoff changing between observations
// can be exercised without scheduler timing or a platform-specific filesystem.
fn select_catalog<T>(
    stored: Option<T>,
    awaiting: impl FnOnce() -> Result<Option<T>>,
    initial: impl FnOnce() -> Result<T>,
) -> Result<T> {
    match stored {
        Some(c) => Ok(c),
        None => match awaiting()? {
            Some(c) => Ok(c),
            None => initial(),
        },
    }
}

#[cfg(test)]
mod handoff_tests {
    use super::*;

    #[test]
    fn awaiting_catalog_is_consumed_from_one_observation() {
        let mut reads = 0;
        let catalog = select_catalog(
            None,
            || {
                reads += 1;
                // The retained handoff is available on the first read, then a
                // concurrent completion/removal makes it absent on the next read.
                Ok((reads == 1).then_some("retained source"))
            },
            || panic!("must not initialize a replacement default"),
        )
        .unwrap();
        assert_eq!(catalog, "retained source");
        assert_eq!(reads, 1);
    }
}
