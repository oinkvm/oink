//! Single-phase native owner transaction and exact-boot request deduplication.
use super::{
    model,
    state::{preference, unavailable},
    types::*,
    wire,
};
use crate::{
    api::{DeviceKey, ScreenKey},
    node::{Node, lock},
};
use oink_display::arrangement::{NativeConfiguration, NativeOrigin};
use oink_foundation::{Point, Size};
use oink_protocol::v1::{
    Message, RawValue as V, dto as d,
    schema::{WireDto, WireValue},
};
use std::{
    collections::BTreeMap,
    sync::{Mutex, mpsc},
    time::Duration,
};

pub const APPLIED: u32 = 0;
pub const FAILED: u32 = 1;
pub const UNKNOWN: u32 = 2;
const LIMIT: usize = 1024;
type PendingResponses = BTreeMap<(DeviceKey, [u8; 16]), mpsc::Sender<d::LayoutArrangementResult>>;
#[derive(Debug, Default)]
pub struct Runtime {
    pub(crate) observation_gate: Mutex<()>,
    records: Mutex<Option<Records>>,
    pending: Mutex<PendingResponses>,
}
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Record {
    digest: [u8; 32],
    result: Option<Vec<u8>>,
    created_ms: u64,
}
const RECEIPT_BYTES: usize = 8 * 1024 * 1024;
type Records = BTreeMap<(DeviceKey, [u8; 16]), Record>;
fn receipt_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join("layout-profiles").join("owner-receipts.json")
}
fn load_records(root: &std::path::Path) -> Result<Records> {
    use std::io::Read;
    let file = match std::fs::File::open(receipt_path(root)) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(fail(ErrorCode::PersistenceFailed, e.to_string())),
    };
    let mut bytes = Vec::new();
    file.take(RECEIPT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| fail(ErrorCode::PersistenceFailed, e.to_string()))?;
    if bytes.len() > RECEIPT_BYTES {
        return Err(fail(ErrorCode::SizeLimit, "Owner receipt limit"));
    }
    let rows: Vec<((DeviceKey, [u8; 16]), Record)> =
        serde_json::from_slice(&bytes).map_err(|e| fail(ErrorCode::PersistenceFailed, e.to_string()))?;
    if rows.len() > LIMIT {
        return Err(fail(ErrorCode::SizeLimit, "Owner receipt count limit"));
    }
    Ok(rows.into_iter().collect())
}
fn save_records(root: &std::path::Path, records: &Records) -> Result<()> {
    let bytes = serde_json::to_vec(&records.iter().collect::<Vec<_>>())
        .map_err(|e| fail(ErrorCode::PersistenceFailed, e.to_string()))?;
    if bytes.len() > RECEIPT_BYTES {
        return Err(fail(ErrorCode::SizeLimit, "Owner receipt limit"));
    }
    let path = receipt_path(root);
    std::fs::create_dir_all(path.parent().expect("receipt parent"))
        .and_then(|_| oink_foundation::storage::atomic_write(&path, &bytes))
        .map_err(|e| fail(ErrorCode::PersistenceFailed, e.to_string()))
}
fn fail(code: ErrorCode, message: impl Into<String>) -> ProfileError {
    ProfileError::new(code, message)
}
fn native_state(device: DeviceKey, observed: &NativeConfiguration) -> Result<NativeState> {
    let primary = observed
        .displays
        .iter()
        .find(|s| s.primary)
        .ok_or_else(|| fail(ErrorCode::FactsUnavailable, "Main screen unavailable"))?;
    Ok(NativeState {
        device,
        token: model::hash(observed),
        primary: ScreenKey {
            device,
            display: primary.key.clone(),
        },
        screens: observed
            .displays
            .iter()
            .filter(|s| s.mirror_of.is_none())
            .map(|s| NativeScreen {
                screen: ScreenKey {
                    device,
                    display: s.key.clone(),
                },
                origin: Point::new(s.bounds.x, s.bounds.y),
                size: Size::new(s.bounds.w, s.bounds.h),
            })
            .collect(),
    })
}
pub fn visibility_patch(bits: &[Disabled]) -> V {
    V::Map(
        [(
            1,
            V::Map(
                [(
                    1,
                    V::Array(
                        bits.iter()
                            .map(|b| {
                                V::Map([(1, V::Text(b.screen.display.clone())), (3, (!b.disabled).write())].into())
                            })
                            .collect(),
                    ),
                )]
                .into(),
            ),
        )]
        .into(),
    )
}
fn response(
    request: &d::LayoutArrangementRequest,
    outcome: u32,
    native: Option<&NativeState>,
    error: Option<ErrorCode>,
    preferences: Vec<d::DevicePreference>,
) -> d::LayoutArrangementResult {
    let mut raw = [
        (1, request.request_id().expect("validated request").write()),
        (2, request.target().expect("validated request").write()),
        (3, outcome.write()),
        (6, V::Array(preferences.iter().map(WireValue::write).collect())),
    ]
    .into_iter()
    .collect::<oink_protocol::v1::RawMap>();
    if let Some(native) = native.and_then(|n| wire::native(n).ok()) {
        raw.insert(4, native.write());
    }
    if let Some(error) = error {
        raw.insert(
            5,
            V::Text(
                serde_json::to_value(error)
                    .expect("error enum")
                    .as_str()
                    .expect("error string")
                    .into(),
            ),
        );
    }
    d::LayoutArrangementResult::from_raw(raw).expect("bounded result")
}
impl Node {
    /// The owner reacts to global layout visibility after the register has
    /// been accepted. The OS write is deliberately downstream of the shared
    /// register; it never edits owner visibility preferences.
    pub(crate) fn apply_global_layout_visibility(&self, layout: &oink_display::layout::Layout) -> Result<()> {
        let settings = self.settings();
        if !settings.sync_system_arrangement {
            return Ok(());
        }
        let backend = self
            .inner
            .display_backend
            .as_ref()
            .ok_or_else(|| fail(ErrorCode::Unsupported, "Native arrangement unavailable"))?;
        let current = backend.native_configuration().map_err(unavailable)?;
        let disabled = if cfg!(windows) && settings.disable_hidden_displays {
            current
                .sources()
                .map(|source| {
                    let hidden = layout
                        .screens
                        .iter()
                        .find(|screen| screen.id.device == self.id() && screen.id.display == source.key)
                        .is_some_and(|screen| screen.screen_hidden || screen.device_hidden);
                    (source.key.clone(), hidden)
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let applied = backend.apply_native_layout(&current, &[], &disabled);
        // Query active geometry even after a partial OS failure. The layout
        // catalog deliberately retains disabled targets and is not cursor space.
        self.refresh_active_input_displays().map_err(unavailable)?;
        applied.map_err(unavailable)?;
        Ok(())
    }

    /// Record the observation without changing the caller's error/retry policy.
    pub(crate) fn observe_native_arrangement(&self, call_site: &'static str) -> Result<NativeState> {
        self.refresh_native_arrangement().inspect_err(|error| {
            tracing::warn!(target: "oink_arrangement", call_site, code = ?error.code, %error,
                "native arrangement observation failed");
        })
    }
    /// Read-only enumeration. Called from the blocking display worker, never the
    /// ConfigModule actor (publishing Facts synchronously calls that actor).
    pub(crate) fn refresh_native_arrangement(&self) -> Result<NativeState> {
        let _observation = lock(&self.inner.profiles.native.observation_gate);
        #[cfg(windows)]
        let _ = self.follow_windows_reconnections()?;
        let backend = self
            .inner
            .display_backend
            .as_ref()
            .ok_or_else(|| fail(ErrorCode::Unsupported, "Display backend unavailable"))?;
        let native = backend.native_configuration().map_err(|error| match error {
            oink_display::DisplayError::Unsupported => fail(ErrorCode::Unsupported, "Native arrangement unavailable"),
            error => unavailable(error),
        })?;
        let state = native_state(self.id().into(), &native).inspect_err(|error| {
            tracing::warn!(target: "oink_arrangement", operation = "native_state",
                display_paths = native.displays.len(), %error, "native arrangement read failed");
        })?;
        self.link_config()
            .map_err(unavailable)?
            .publish_native_arrangement(state.token.clone())
            .map_err(unavailable)?;
        Ok(state)
    }
    pub(crate) fn receive_arrangement(&self, peer: DeviceKey, message: Message) {
        match message {
            Message::LayoutArrangementRequest(request) => {
                let node = self.clone();
                self.inner.rt.spawn_blocking(move || {
                    let result = node.owner_arrangement(peer, request);
                    if let Ok(config) = node.link_config() {
                        let _ = config.send_device_control(peer.0, Message::LayoutArrangementResult(result));
                    }
                });
            }
            Message::LayoutArrangementResult(result) => {
                if let Ok(id) = result.request_id()
                    && let Some(tx) = lock(&self.inner.profiles.native.pending).get(&(peer, id.0))
                {
                    let _ = tx.send(result);
                }
            }
            _ => {}
        }
    }
    pub(super) fn owner_arrangement(
        &self,
        peer: DeviceKey,
        request: d::LayoutArrangementRequest,
    ) -> d::LayoutArrangementResult {
        let id = request.request_id().expect("validated request").0;
        let status = request.status_only() == Ok(true);
        if !self.group_members().contains(&peer.0)
            || (!status
                && request
                    .target()
                    .and_then(|b| oink_network::wire_identity::read_boot(&b))
                    != Ok(self.boot()))
        {
            return response(&request, FAILED, None, Some(ErrorCode::NotMember), Vec::new());
        }
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(request.encode(model::MAX_BYTES).expect("bounded request")).into();
        {
            let mut cache = lock(&self.inner.profiles.native.records);
            if cache.is_none() {
                match load_records(&self.inner.data_dir) {
                    Ok(records) => *cache = Some(records),
                    Err(_) => return response(&request, FAILED, None, Some(ErrorCode::PersistenceFailed), Vec::new()),
                }
            }
            let records = cache.as_mut().expect("loaded receipts");
            if let Some(record) = records.get(&(peer, id)) {
                if !status && record.digest != digest {
                    return response(&request, FAILED, None, Some(ErrorCode::StaleState), Vec::new());
                }
                if let Some(reply) = &record.result {
                    return d::LayoutArrangementResult::decode(reply, model::MAX_BYTES).unwrap_or_else(|_| {
                        response(&request, UNKNOWN, None, Some(ErrorCode::OutcomeUnknown), Vec::new())
                    });
                }
                drop(cache);
                let observed = self.refresh_native_arrangement().ok();
                return response(
                    &request,
                    UNKNOWN,
                    observed.as_ref(),
                    Some(ErrorCode::OutcomeUnknown),
                    Vec::new(),
                );
            }
            if status {
                drop(cache);
                let observed = self.refresh_native_arrangement().ok();
                return response(
                    &request,
                    UNKNOWN,
                    observed.as_ref(),
                    Some(ErrorCode::OutcomeUnknown),
                    Vec::new(),
                );
            }
            let now = crate::layout_sync::now_ms();
            // Keep unresolved receipts indefinitely. Completed receipts have a
            // seven-day retry window; capacity exhaustion rejects new writes.
            records.retain(|_, r| r.result.is_none() || now.saturating_sub(r.created_ms) < 7 * 24 * 60 * 60 * 1000);
            if records.len() >= LIMIT {
                return response(&request, FAILED, None, Some(ErrorCode::Busy), Vec::new());
            }
            records.insert(
                (peer, id),
                Record {
                    digest,
                    result: None,
                    created_ms: now,
                },
            );
            // Durable acceptance precedes every possible OS side effect.
            if save_records(&self.inner.data_dir, records).is_err() {
                records.remove(&(peer, id));
                return response(&request, FAILED, None, Some(ErrorCode::PersistenceFailed), Vec::new());
            }
        }
        let result = self.apply_owner_arrangement(peer, &request);
        let observed = self.refresh_native_arrangement().ok();
        let prefs = lock(&self.inner.config).owner().preferences();
        let mut reply = response(
            &request,
            if result.is_ok() { APPLIED } else { FAILED },
            observed.as_ref(),
            result.err().map(|e| e.code),
            prefs,
        );
        if let Ok(config) = self.link_config() {
            reply = match config.fence_arrangement_result(reply) {
                Ok(reply) => reply,
                Err(_) => {
                    // Keep the durable acceptance unresolved. Replaying the OS
                    // write or acknowledging an unpublished fence is unsafe.
                    return response(
                        &request,
                        UNKNOWN,
                        observed.as_ref(),
                        Some(ErrorCode::OutcomeUnknown),
                        Vec::new(),
                    );
                }
            };
        }
        {
            let mut cache = lock(&self.inner.profiles.native.records);
            let records = cache.as_mut().expect("loaded receipts");
            records.get_mut(&(peer, id)).expect("reserved request").result =
                Some(reply.encode(model::MAX_BYTES).expect("bounded reply"));
            if save_records(&self.inner.data_dir, records).is_err() {
                records.get_mut(&(peer, id)).expect("reserved request").result = None;
                return response(
                    &request,
                    UNKNOWN,
                    observed.as_ref(),
                    Some(ErrorCode::OutcomeUnknown),
                    Vec::new(),
                );
            }
        }
        self.refresh_layout_profiles();
        reply
    }
    fn apply_owner_arrangement(&self, peer: DeviceKey, request: &d::LayoutArrangementRequest) -> Result<()> {
        // Same lock order as the platform observer: enumeration first, then
        // owner preferences. A pre-transaction callback must not publish stale
        // geometry after the transaction's final observation.
        let _observation = lock(&self.inner.profiles.native.observation_gate);
        // Close the event/command race: an outside reactivation is an unhide
        // even when an explicit Oink command arrives before the display worker.
        #[cfg(windows)]
        let reconnected = self.follow_windows_reconnections()?;
        let _gate = self.owner_gate();
        let config = self.link_config().map_err(unavailable)?;
        if !config.applied().members.contains(&peer.0)
            || request
                .target()
                .and_then(|b| oink_network::wire_identity::read_boot(&b))
                != Ok(self.boot())
        {
            return Err(fail(ErrorCode::NotMember, "Owner authority changed"));
        }
        let origins = request
            .origins()
            .map_err(wire::invalid)?
            .0
            .into_iter()
            .map(|o| {
                Ok(NativeOrigin {
                    key: o.display()?.0,
                    origin: Point::new(o.origin()?.x()?, o.origin()?.y()?),
                })
            })
            .collect::<oink_protocol::v1::Result<Vec<_>>>()
            .map_err(wire::invalid)?;
        let bits = request
            .disabled()
            .map_err(wire::invalid)?
            .0
            .into_iter()
            .map(wire::read_disabled)
            .collect::<oink_protocol::v1::Result<Vec<_>>>()
            .map_err(wire::invalid)?;
        #[cfg(windows)]
        if bits
            .iter()
            .any(|bit| bit.disabled && reconnected.iter().any(|display| display == &bit.screen.display))
        {
            return Err(fail(ErrorCode::StaleState, "Display was re-enabled by Windows"));
        }
        let backend = self
            .inner
            .display_backend
            .as_ref()
            .ok_or_else(|| fail(ErrorCode::Unsupported, "Native arrangement unavailable"))?;
        let current = backend.native_configuration().map_err(unavailable)?;
        if bits
            .iter()
            .any(|b| b.screen.device.0 != self.id() || !current.displays.iter().any(|s| s.key == b.screen.display))
        {
            return Err(fail(
                ErrorCode::InvalidGeometry,
                "Visibility references an unknown screen",
            ));
        }
        {
            let stored = lock(&self.inner.config);
            let owner = stored.owner();
            if request.preference_revision().map_err(wire::invalid)?
                != owner.revision(d::LocalOption::SyncSystemArrangement)
            {
                return Err(fail(ErrorCode::StaleState, "Owner preference changed"));
            }
            if !origins.is_empty() {
                if !stored.settings.sync_system_arrangement {
                    return Err(fail(ErrorCode::SettingDisabled, "System arrangement sync is off"));
                }
                if request.native_token().map_err(wire::invalid)?.map(|t| t.0) != Some(model::hash(&current)) {
                    return Err(fail(ErrorCode::NativeChanged, "Native display configuration changed"));
                }
                current
                    .with_origins(&origins)
                    .map_err(|e| fail(ErrorCode::Disconnected, e.to_string()))?;
            }
        }
        let settings = self.settings();
        let write_visibility =
            cfg!(windows) && settings.sync_system_arrangement && settings.disable_hidden_displays && !bits.is_empty();
        if !origins.is_empty() || write_visibility {
            // The backend rechecks the complete snapshot immediately before one
            // session-scoped OS transaction, then re-enumerates the result.
            #[cfg(windows)]
            let result = {
                let settings = self.settings();
                let layout = config.applied().layout.clone();
                let visibility = current
                    .sources()
                    .filter(|_| settings.disable_hidden_displays)
                    .map(|s| {
                        let hidden = bits
                            .iter()
                            .find(|b| b.screen.display == s.key)
                            .map(|b| b.disabled)
                            .unwrap_or_else(|| {
                                layout
                                    .screens
                                    .iter()
                                    .find(|p| p.id.device == self.id() && p.id.display == s.key)
                                    .is_some_and(|p| p.screen_hidden)
                            });
                        (s.key.clone(), hidden)
                    })
                    .collect::<Vec<_>>();
                backend.apply_native_layout(&current, &origins, &visibility)
            };
            #[cfg(not(windows))]
            let result = backend.apply_native_origins(&current, &origins);
            // Even a failed transaction can have changed the desktop. Publish
            // only re-enumerated actual geometry, never the requested origins.
            self.refresh_active_input_displays().map_err(unavailable)?;
            let observed = backend.displays().map_err(unavailable)?;
            config.initialize_displays(observed).map_err(unavailable)?;
            result.map_err(|e| fail(ErrorCode::OsApplyFailed, e.to_string()))?;
        }
        Ok(())
    }
    /// Called only while observation_gate is held. The OS is authoritative for
    /// reactivation, even if Windows (rather than a human) restored the target.
    #[cfg(windows)]
    pub(crate) fn follow_windows_reconnections(&self) -> Result<Vec<String>> {
        let Some(backend) = &self.inner.display_backend else {
            return Ok(Vec::new());
        };
        let keys = backend
            .reconnected_displays()
            .map_err(|e| unavailable(format!("{e:?}")))?;
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let device = self.id();
        let reconnected = keys.clone();
        // A Windows re-enable is an external native observation. Clear Oink's
        // bookkeeping before publishing the corresponding global unhide so
        // the accepted layout change does not fight the OS with a redundant
        // disable/enable transaction.
        backend
            .acknowledge_reconnected(&keys)
            .map_err(|e| unavailable(format!("{e:?}")))?;
        let layout = self.link_config().map_err(unavailable)?;
        layout
            .edit_layout(
                Box::new(move |current| {
                    for screen in &mut current.screens {
                        if screen.id.device == device && reconnected.iter().any(|key| key == &screen.id.display) {
                            screen.screen_hidden = false;
                            screen.screen_visibility_known = true;
                        }
                    }
                }),
                d::LayoutSource::Manual,
            )
            .map_err(unavailable)?;
        Ok(keys)
    }
    /// Normal exit releases only Oink-owned disabled paths.
    /// Existing positions and hidden owner preferences remain authoritative.
    #[cfg(windows)]
    pub(crate) fn release_windows_displays(&self) -> Result<()> {
        let _observation = lock(&self.inner.profiles.native.observation_gate);
        let _ = self.follow_windows_reconnections()?;
        let _gate = self.owner_gate();
        let Some(backend) = &self.inner.display_backend else {
            return Ok(());
        };
        backend.release_disabled_displays().map_err(unavailable)?;
        self.refresh_active_input_displays().map_err(unavailable)?;
        Ok(())
    }
    /// A fresh startup applies this owner's current layout once. Neither display
    /// callbacks nor preference enablement call this writer. Shutdown releases targets
    /// without undoing arrangement positions.
    #[cfg(windows)]
    pub(crate) fn apply_windows_startup_layout(&self) -> Result<()> {
        let _observation = lock(&self.inner.profiles.native.observation_gate);
        let _ = self.follow_windows_reconnections()?;
        let _gate = self.owner_gate();
        let settings = self.settings();
        let Some(backend) = &self.inner.display_backend else {
            return Ok(());
        };
        if !settings.sync_system_arrangement || !settings.disable_hidden_displays {
            backend.release_disabled_displays().map_err(unavailable)?;
        }
        if !settings.sync_system_arrangement {
            return Ok(());
        }
        let config = self.link_config().map_err(unavailable)?;
        let applied = config.applied();
        let current = backend.native_configuration().map_err(unavailable)?;
        let saved = |key: &str| {
            applied
                .layout
                .screens
                .iter()
                .find(|s| s.id.device == self.id() && s.id.display == key)
        };
        let anchor = current
            .sources()
            .find(|d| d.primary)
            .and_then(|d| saved(&d.key))
            .map(|s| Point::new(s.local.x, s.local.y))
            .unwrap_or_default();
        let origins = current
            .sources()
            .map(|d| NativeOrigin {
                key: d.key.clone(),
                origin: saved(&d.key)
                    .map(|s| Point::new(s.local.x - anchor.x, s.local.y - anchor.y))
                    .unwrap_or(Point::new(d.bounds.x, d.bounds.y)),
            })
            .collect::<Vec<_>>();
        let bits = current
            .sources()
            .filter(|_| settings.disable_hidden_displays)
            .filter_map(|d| {
                applied
                    .layout
                    .screens
                    .iter()
                    .find(|p| p.id.device == self.id() && p.id.display == d.key)
                    .map(|p| (d.key.clone(), p.screen_hidden || p.device_hidden))
            })
            .collect::<Vec<_>>();
        backend
            .apply_native_layout(&current, &origins, &bits)
            .map_err(|e| fail(ErrorCode::OsApplyFailed, e.to_string()))?;
        Ok(())
    }
    pub(super) fn request_arrangement(
        &self,
        device: DeviceKey,
        request: d::LayoutArrangementRequest,
    ) -> Result<d::LayoutArrangementResult> {
        if device.0 == self.id() {
            return Ok(self.owner_arrangement(self.id().into(), request));
        }
        let config = self.link_config().map_err(unavailable)?;
        let target = request.target().map_err(wire::invalid)?;
        let status_only = request.status_only().map_err(wire::invalid)?;
        let id = request.request_id().map_err(wire::invalid)?.0;
        let (tx, rx) = mpsc::channel();
        lock(&self.inner.profiles.native.pending).insert((device, id), tx);
        let sent = config.send_device_control(device.0, Message::LayoutArrangementRequest(request));
        let result = if sent.is_err() {
            Err(fail(ErrorCode::OwnerOffline, "Owner link is unavailable"))
        } else {
            rx.recv_timeout(Duration::from_secs(15))
                .map_err(|_| fail(ErrorCode::OutcomeUnknown, "Owner outcome is not yet known"))
                .and_then(|result| {
                    if result.target().map_err(wire::invalid)? != target {
                        return Err(fail(ErrorCode::OutcomeUnknown, "Owner boot changed"));
                    }
                    // The owner queues the fenced Facts before this C0 result.
                    // Publish history only after its observed versions arrive;
                    // never project requested native origins.
                    let deadline = std::time::Instant::now() + Duration::from_secs(3);
                    if !status_only {
                        loop {
                            let layout_ready = result.layout_version().map_err(wire::invalid)?.is_none_or(|v| {
                                oink_protocol::v1::retained::layout_version_cmp(&config.applied().version, &v)
                                    .is_ok_and(|o| o != std::cmp::Ordering::Less)
                            });
                            let facts_ready = result.fact_sequence().map_err(wire::invalid)?.is_none_or(|sequence| {
                                self.device_facts(device.0).is_some_and(|f| {
                                    f.boot() == Ok(target.clone()) && f.fact_sequence().is_ok_and(|s| s >= sequence)
                                })
                            });
                            if layout_ready && facts_ready {
                                break;
                            }
                            if std::time::Instant::now() >= deadline {
                                return Err(fail(ErrorCode::OutcomeUnknown, "Waiting for observed owner state"));
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                    Ok(result)
                })
        };
        lock(&self.inner.profiles.native.pending).remove(&(device, id));
        result
    }
    pub(super) fn make_arrangement_request(
        &self,
        id: &str,
        device: DeviceKey,
        native: Option<&NativeState>,
        bits: &[Disabled],
    ) -> Result<d::LayoutArrangementRequest> {
        let config = self.link_config().map_err(unavailable)?;
        let boot = if device.0 == self.id() {
            self.boot()
        } else {
            config
                .peer_boot(device.0)
                .ok_or_else(|| fail(ErrorCode::OwnerOffline, "Owner is offline"))?
        };
        let facts = self
            .device_facts(device.0)
            .ok_or_else(|| unavailable("Owner facts unavailable"))?;
        let preference = preference(&facts, d::LocalOption::SyncSystemArrangement)
            .ok_or_else(|| fail(ErrorCode::Unsupported, "Owner has no arrangement preference"))?;
        let mut raw = [
            (1, V::Bytes(request_id(id, device).to_vec())),
            (2, oink_network::wire_identity::boot(boot).write()),
            (5, preference.revision().map_err(wire::invalid)?.write()),
            (8, V::Array(bits.iter().map(wire::disabled).collect())),
        ]
        .into_iter()
        .collect::<oink_protocol::v1::RawMap>();
        if let Some(p) = super::state::preference(&facts, d::LocalOption::ScreenPreferences) {
            raw.insert(6, p.revision().map_err(wire::invalid)?.write());
        }
        if let Some(native) = native {
            raw.insert(
                4,
                V::Text(
                    facts
                        .native_arrangement_token()
                        .map_err(wire::invalid)?
                        .ok_or_else(|| unavailable("Native token unavailable"))?
                        .0,
                ),
            );
            raw.insert(
                7,
                V::Array(
                    native
                        .screens
                        .iter()
                        .map(|s| V::Map([(1, V::Text(s.screen.display.clone())), (2, wire::point(s.origin))].into()))
                        .collect(),
                ),
            );
        }
        d::LayoutArrangementRequest::from_raw(raw).map_err(wire::invalid)
    }
}
pub fn request_id(id: &str, device: DeviceKey) -> [u8; 16] {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{id}:{}", device.0.to_hex()).as_bytes());
    digest[..16].try_into().expect("hash prefix")
}
