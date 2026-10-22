//! Serial owner of discovery, C0 admission, independent pairing, signed roster,
//! retained shared layout and current-boot device Facts.
pub mod detached;
mod facts;
pub mod group_store;
mod handshake;
pub mod layout_wire;
pub mod local_records;
mod runtime;
use crate::{
    api::{ApiResult, pairing as api},
    device_cards::{BlockReply, BlockedDevicesSnapshot},
};
use oink_display::{
    DisplayInfo,
    layout::{Layout, OwnerLayoutPreferences},
};
use oink_foundation::{DeviceId, GroupId, session::Boot, snapshot::Snapshot};
use oink_network::link::{ConfigPort, LinkLayer};
use oink_protocol::v1::{self, dto};
use oink_routing::TopologyLayout;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
pub const MAX_MEMBERS: usize = oink_network::roster::MAX_MEMBERS;
pub const REDIAL_INTERVAL: Duration = Duration::from_secs(1);
/// The config owner alone schedules retries of an unreachable paired member.
pub const RECONNECT_BACKOFF: [Duration; 5] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(5),
];
/// Authentication/selection rejection is not evidence of a transient outage.
pub const HANDSHAKE_BACKOFF: [Duration; 4] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(60),
    Duration::from_secs(300),
];
pub const STATE_BROADCAST: Duration = Duration::from_secs(60);
pub const FACTS_INTERVAL: Duration = Duration::from_secs(5);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Item {
    Members,
    Layout,
    Pause,
}
#[derive(Clone, Debug, PartialEq)]
pub struct MemberInfo {
    pub device: DeviceId,
    pub is_self: bool,
    pub name: String,
    pub os: String,
    pub online: bool,
    pub facts: Option<Arc<dto::DeviceFacts>>,
    pub added_by: Option<DeviceId>,
    pub added_at_ms: u64,
    /// Most recent local live-link observation, refreshed on the offline transition.
    pub last_connected_ms: Option<u64>,
    pub address_stale: bool,
    pub protocol_incompatible: bool,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MemberTable {
    pub members: Vec<MemberInfo>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedState {
    pub group: GroupId,
    pub roster_digest: [u8; 32],
    pub members: Vec<DeviceId>,
    pub layout: Arc<Layout>,
    /// Drawing preferences include retained offline owners; routing uses RouterLayout.
    pub owners: Arc<BTreeMap<DeviceId, OwnerLayoutPreferences>>,
    pub layout_source: dto::LayoutSource,
    pub version: dto::ConfigVersion,
    pub paused: bool,
    pub emergency: bool,
}
#[derive(Clone, Debug, PartialEq)]
pub struct LocalDisplays {
    pub displays: Vec<DisplayInfo>,
    pub layout: Arc<Layout>,
    /// Drawing preferences include retained offline owners; routing uses RouterLayout.
    pub owners: Arc<BTreeMap<DeviceId, OwnerLayoutPreferences>>,
}
#[derive(Clone)]
pub struct RouterLayout {
    pub group: GroupId,
    pub version: dto::ConfigVersion,
    pub layout: Arc<Layout>,
    pub owners: Arc<BTreeMap<DeviceId, OwnerLayoutPreferences>>,
    pub devices: BTreeSet<DeviceId>,
    pub topology: TopologyLayout,
}
impl std::fmt::Debug for RouterLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouterLayout")
            .field("devices", &self.devices)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigChange {
    State {
        items: Vec<Item>,
    },
    Members,
    Facts {
        device: DeviceId,
        capabilities_changed: bool,
    },
    LocalDisplays,
    PairingEvent(Box<api::PairingEvent>),
    BlockedDevices,
    DeviceControl {
        peer: DeviceId,
        message: v1::Message,
    },
    ProtocolError {
        link: oink_network::link::LinkId,
        peer: DeviceId,
        error: dto::ProtocolError,
    },
}
#[derive(Debug)]
pub struct ConfigChanges(tokio::sync::mpsc::UnboundedReceiver<ConfigChange>);
impl ConfigChanges {
    pub async fn next(&mut self) -> Option<ConfigChange> {
        self.0.recv().await
    }
}
pub trait ConfigApplication: Send + Sync {
    fn prepare_profile_join(&self, _intent: &dto::MergeIntent) -> Result<(), String> {
        Ok(())
    }

    fn apply_state(&self, state: &group_store::StoredGroupState) -> Result<(), String>;
}
pub struct ConfigDeps {
    pub config: Arc<std::sync::Mutex<crate::config::AgentConfig>>,
    pub owner_gate: Arc<std::sync::Mutex<()>>,
    pub data_dir: PathBuf,
    pub links: LinkLayer,
    pub port: ConfigPort,
    pub identity: Arc<oink_network::identity::Identity>,
    pub application: Arc<dyn ConfigApplication>,
}
impl std::fmt::Debug for ConfigDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigDeps")
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
}
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("group state unreadable: {0}")]
    Unreadable(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
pub fn reset_group_state(
    data_dir: &std::path::Path,
    identity: &oink_network::identity::Identity,
) -> std::io::Result<()> {
    let path = data_dir.join(group_store::STATE_FILE);
    if path.exists() {
        std::fs::rename(
            &path,
            data_dir.join(format!("group-state-unreadable-{}.json", runtime::now_ms())),
        )?;
    }
    let defaults = oink_network::NetworkConfig::default();
    let state =
        oink_network::group_state::State::create(identity, &defaults.device_name, &defaults.os, runtime::now_ms())
            .map_err(std::io::Error::other)?;
    let stored =
        group_store::StoredGroupState::new(identity.id(), state.wire().clone()).map_err(std::io::Error::other)?;
    group_store::save(&path, &stored).map_err(std::io::Error::other)
}
#[derive(Clone, Debug)]
pub struct ConfigModule {
    shared: Arc<runtime::Shared>,
    commands: tokio::sync::mpsc::UnboundedSender<runtime::Command>,
    links: LinkLayer,
}
impl ConfigModule {
    pub fn start(deps: ConfigDeps) -> Result<Self, StartError> {
        Self::start_with_config(deps, oink_network::NetworkConfig::default())
    }
    pub fn start_with_config(deps: ConfigDeps, config: oink_network::NetworkConfig) -> Result<Self, StartError> {
        runtime::Owner::start(deps, config)
    }
    pub async fn stop(&self) {
        let mut stopped = self.shared.stopped.subscribe();
        let _ = self.commands.send(runtime::Command::Stop);
        while !*stopped.borrow_and_update() {
            if stopped.changed().await.is_err() {
                break;
            }
        }
    }
    pub fn members(&self) -> Arc<MemberTable> {
        self.shared.members.load()
    }
    pub fn local_facts(&self) -> Arc<dto::DeviceFacts> {
        self.shared.local_facts.load()
    }
    pub fn applied(&self) -> Arc<AppliedState> {
        self.shared.applied.load()
    }
    pub fn local_displays(&self) -> Arc<LocalDisplays> {
        self.shared.displays.load()
    }
    pub fn router_layout(&self) -> Arc<Snapshot<RouterLayout>> {
        self.shared.router.clone()
    }
    pub fn nearby(&self) -> Arc<api::NearbySnapshot> {
        self.shared.nearby.load()
    }
    pub fn pairings(&self) -> Arc<api::PairingSnapshot> {
        self.shared.pairings.load()
    }
    pub fn group_migration(&self) -> Arc<Option<api::GroupMigrationStatus>> {
        self.shared.migration.load()
    }
    pub fn local_addresses(&self) -> Arc<Vec<std::net::IpAddr>> {
        self.shared.addresses.load()
    }
    pub fn subscribe(&self) -> ConfigChanges {
        self.shared.subscribe()
    }
    pub fn peer_boot(&self, device: DeviceId) -> Option<Boot> {
        self.links
            .links()
            .by_peer(device)
            .filter(|l| l.state == oink_network::link::LinkState::Ready)
            .map(|l| l.peer_boot)
    }
    pub fn peer_agreement(&self, device: DeviceId) -> Option<Arc<v1::admission::Agreement>> {
        self.links
            .links()
            .by_peer(device)
            .filter(|l| l.state == oink_network::link::LinkState::Ready)
            .map(|l| Arc::new(l.agreement.clone()))
    }
    pub fn peer_formats(&self, device: DeviceId) -> Option<Arc<dto::FormatSupport>> {
        self.links
            .links()
            .by_peer(device)
            .filter(|l| l.state == oink_network::link::LinkState::Ready)
            .and_then(|l| l.peer_formats.clone())
            .map(Arc::new)
    }
    pub fn peer_audio(&self, device: DeviceId) -> Option<Vec<dto::AudioSupport>> {
        self.links
            .links()
            .by_peer(device)
            .filter(|l| l.state == oink_network::link::LinkState::Ready)
            .and_then(|l| l.peer_audio.clone())
    }
    pub fn send_device_control(&self, peer: DeviceId, message: v1::Message) -> ApiResult<()> {
        self.call(move |owner| owner.device_control(peer, message))
    }
    pub fn send_protocol_error(&self, link: oink_network::link::LinkId, error: dto::ProtocolError) -> ApiResult<()> {
        self.call(move |owner| owner.protocol_error(link, error))
    }
    pub fn publish_local_owner(
        &self,
        preferences: Vec<dto::DevicePreference>,
        pause: dto::LocalPauseState,
    ) -> ApiResult<u64> {
        self.call(move |owner| owner.publish_owner(preferences, pause))
    }
    pub fn publish_local_clipboard(
        &self,
        mode: Option<dto::SharingMode>,
        fault: Option<dto::ResourceFault>,
    ) -> ApiResult<u64> {
        self.call(move |owner| {
            owner.local.clipboard(mode, fault).map_err(runtime::internal)?;
            owner.publish_facts()
        })
    }
    pub fn publish_local_audio(
        &self,
        permission: dto::CapturePermission,
        outputs: Vec<dto::AudioOutputFact>,
        speaker: dto::SpeakerObservation,
        fault: Option<dto::ResourceFault>,
        exit_volume: Option<dto::ExitVolumeState>,
    ) -> ApiResult<u64> {
        self.call(move |owner| owner.publish_audio(permission, outputs, speaker, fault, exit_volume))
    }
    pub fn publish_local_input(
        &self,
        mouse: dto::SharingMode,
        keyboard: dto::SharingMode,
        faults: dto::ResourceFaults,
        options: dto::LocalOptionsSummary,
        game: Option<dto::GameObservation>,
        accessibility_granted: Option<bool>,
    ) -> ApiResult<u64> {
        self.call(move |owner| owner.publish_input(mouse, keyboard, faults, options, game, accessibility_granted))
    }
    pub(crate) fn publish_native_arrangement(&self, token: String) -> ApiResult<u64> {
        self.call(move |owner| owner.publish_native_arrangement(token))
    }
    pub(crate) fn fence_arrangement_result(
        &self,
        mut result: dto::LayoutArrangementResult,
    ) -> ApiResult<dto::LayoutArrangementResult> {
        use oink_protocol::v1::schema::WireValue;
        let shared = self.shared.clone();
        self.call(move |owner| {
            // Local projection (including a new RTT) can advance Facts without
            // broadcasting. Fence the sequence actually queued for peers, in
            // the same actor turn, before a completed owner receipt is sent.
            let sequence = owner.publish_facts()?;
            let version = shared.applied.load().version.clone();
            result
                .edit(|raw| {
                    raw.insert(7, version.write());
                    raw.insert(8, sequence.write());
                })
                .map_err(runtime::internal)?;
            Ok(result)
        })
    }
    pub fn publish_local_claim(&self, claim: dto::AudioExitClaim) -> ApiResult<u64> {
        self.call(move |owner| owner.publish_claim(claim))
    }
    pub(crate) fn initialize_displays(&self, displays: Vec<DisplayInfo>) -> ApiResult<()> {
        self.call(move |owner| owner.displays_changed(displays))
    }
    pub fn local_displays_changed(&self, displays: Vec<DisplayInfo>) {
        self.send(move |owner| {
            let _ = owner.displays_changed(displays);
        });
    }
    pub fn edit_layout(&self, edit: Box<dyn FnOnce(&mut Layout) + Send>, source: dto::LayoutSource) -> ApiResult<()> {
        self.call(move |owner| owner.edit_layout(edit, source))
    }
    pub fn edit_settings(
        &self,
        edit: Box<dyn FnOnce(&mut oink_display::layout::LayoutSettings) + Send>,
    ) -> ApiResult<()> {
        self.edit_layout(
            Box::new(move |layout| edit(&mut layout.settings)),
            dto::LayoutSource::Manual,
        )
    }

    pub fn remove_member(&self, device: DeviceId) -> ApiResult<()> {
        self.call(move |owner| owner.remove_member(device, None))
    }
    pub fn leave_group(&self) -> ApiResult<()> {
        self.call(|owner| owner.leave_group())
    }
    pub fn sleep(&self) {
        self.send(|owner| owner.sleep());
    }
    pub fn wake(&self) {
        self.send(|owner| owner.wake());
    }
    pub fn member_addresses(&self, device: DeviceId) -> ApiResult<Vec<String>> {
        self.call(move |owner| owner.member_addresses(device))
    }
    pub fn set_member_address(&self, device: DeviceId, address: String) -> ApiResult<()> {
        self.call(move |owner| owner.set_member_address(device, address))
    }
    pub fn refresh_nearby(&self, operation: String) -> ApiResult<api::PairReply<api::Accepted>> {
        self.call(move |owner| owner.refresh_nearby(operation))
    }
    pub fn set_onboarding_connect(&self, active: bool) -> ApiResult<()> {
        self.call(move |owner| {
            owner.set_onboarding_connect(active);
            Ok(())
        })
    }
    pub fn query_pairing_address(
        &self,
        client: [u8; 16],
        operation: String,
        generation: u64,
        ip: String,
        port: Option<u16>,
    ) -> ApiResult<api::PairReply<api::Accepted>> {
        self.call(move |owner| owner.query_address(client, operation, generation, ip, port))
    }
    #[cfg(test)]
    pub(crate) fn hold_periodic_facts(&self) {
        self.call(|owner| {
            owner.hold_facts = true;
            Ok(())
        })
        .unwrap();
    }
    pub fn release_pairing_query(&self, client: [u8; 16], id: String, generation: u64) -> ApiResult<()> {
        self.call(move |owner| {
            owner.release_query(client, &id, generation);
            Ok(())
        })
    }
    pub fn start_pairing(
        &self,
        client: [u8; 16],
        operation: String,
        target: api::PairTarget,
    ) -> ApiResult<api::PairReply<api::Started>> {
        self.call(move |owner| owner.start_pairing(client, operation, target))
    }
    pub fn accept_pairing(&self, id: String, revision: u64) -> ApiResult<api::PairReply<api::Accepted>> {
        self.call(move |owner| owner.accept_pairing(id, revision))
    }
    pub fn decline_pairing(&self, id: String, revision: u64) -> ApiResult<api::PairReply<api::Accepted>> {
        self.call(move |owner| owner.decline_pairing(id, revision))
    }
    pub fn cancel_pairing(&self, id: String, revision: u64) -> ApiResult<api::PairReply<api::CancelResult>> {
        self.call(move |owner| owner.cancel_pairing(id, revision))
    }
    pub fn pairing_code(&self, id: String) -> ApiResult<api::CodeView> {
        self.call(move |owner| owner.pairing_code(id))
    }
    pub fn submit_pairing_code(
        &self,
        id: String,
        revision: u64,
        attempt: u8,
        code: String,
    ) -> ApiResult<api::PairReply<api::Accepted>> {
        self.call(move |owner| owner.submit_pairing(id, revision, attempt, code))
    }
    pub fn undo_pairing(&self, operation: String, undo: api::UndoHandle) -> ApiResult<api::PairReply<api::Accepted>> {
        self.call(move |owner| owner.undo_pairing(operation, undo))
    }
    pub fn subscribe_pairing_events(&self, after: u64) -> ApiResult<Vec<api::PairingEvent>> {
        self.call(move |owner| owner.pairing_events(after))
    }
    pub fn ack_pairing_event(&self, id: u64) -> ApiResult<()> {
        self.call(move |owner| owner.ack_pairing_event(id))
    }
    pub fn blocked_devices(&self) -> ApiResult<BlockedDevicesSnapshot> {
        self.call(|owner| owner.blocked_devices())
    }
    pub fn block(&self, operation: String, device: DeviceId) -> ApiResult<BlockReply> {
        self.call(move |owner| owner.block_edit(operation, device, true))
    }
    pub fn unblock(&self, operation: String, device: DeviceId) -> ApiResult<BlockReply> {
        self.call(move |owner| owner.block_edit(operation, device, false))
    }
    pub fn diagnostic_excerpt(&self) -> serde_json::Value {
        self.shared.diagnostic.load().as_ref().clone()
    }
    pub(crate) fn diagnostic_layout(
        &self,
        anon: &crate::diagnostics::Anonymizer,
    ) -> Option<crate::diagnostics::layout::CompleteLayout> {
        self.shared
            .diagnostic_layout
            .load()
            .as_ref()
            .as_ref()
            .and_then(|inputs| inputs.project(anon).ok())
    }
    pub(crate) fn observe_layout(&self, recorder: &crate::diagnostics::Recorder) -> ApiResult<()> {
        let sink = crate::diagnostics::layout::Sink::new(recorder);
        self.call(move |owner| owner.observe_layout(sink))
    }
    pub(crate) fn router_compiled(&self, compiled: crate::diagnostics::layout::RouterCompilation) {
        self.send(move |owner| owner.router_compiled(compiled));
    }
    pub(crate) fn flush_layout_diagnostics(&self) {
        let _ = self.call(|owner| owner.flush_layout_diagnostics());
    }
}

#[cfg(all(test, debug_assertions))]
pub(crate) use runtime::tests as runtime_tests;
