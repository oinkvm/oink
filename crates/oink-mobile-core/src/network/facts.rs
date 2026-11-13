//! C0 device facts for the mobile profile. The owner keeps one revisioned local
//! tree and accepts peer facts only for the authenticated, admitted Boot.
use super::*;
use oink_network::wire_identity as wire;

/// This device's IPv4 endpoints with the QUIC port, as the PC publishes them.
/// Route selection (protocol-v2 section 11) dials every published endpoint, so
/// a phone has to publish its own for PCs to keep standby connections to it.
pub(super) fn endpoints(local: std::net::SocketAddr) -> v2::Result<RawValue> {
    endpoint_values(&local_addresses(local), local.port())
}
/// This device's IPv4 addresses. A listener bound to one IPv4 answers only
/// there (loopback test fixtures).
fn local_addresses(local: std::net::SocketAddr) -> Vec<std::net::Ipv4Addr> {
    match local.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_unspecified() => vec![ip],
        _ => oink_network::discovery::local_ipv4()
            .into_iter()
            .map(|(ip, _)| ip)
            .collect(),
    }
}
fn endpoint_values(addresses: &[std::net::Ipv4Addr], port: u16) -> v2::Result<RawValue> {
    let mut values = Vec::new();
    for &ip in addresses {
        values.push(
            dto::EndpointFact::from_raw(
                [
                    (1, dto::AddressFamily::Ipv4.write()),
                    (2, RawValue::Bytes(ip.octets().to_vec())),
                    (3, port.write()),
                    (4, 0u32.write()),
                ]
                .into(),
            )?
            .write(),
        );
    }
    Ok(RawValue::Array(values))
}

/// The IPv4 endpoints a peer published: its route list.
pub(super) fn route_addresses(facts: &dto::DeviceFacts) -> Vec<std::net::SocketAddr> {
    let Ok(endpoints) = facts.endpoints() else {
        return Vec::new();
    };
    endpoints
        .0
        .into_iter()
        .filter(|endpoint| endpoint.family() == Ok(dto::AddressFamily::Ipv4))
        .filter_map(|endpoint| {
            let octets = <[u8; 4]>::try_from(endpoint.address().ok()?.0.as_slice()).ok()?;
            let ip = std::net::Ipv4Addr::from(octets);
            let port = endpoint.port().ok()?;
            (!ip.is_unspecified() && !ip.is_multicast() && ip != std::net::Ipv4Addr::BROADCAST)
                .then_some(std::net::SocketAddr::from((ip, port)))
        })
        .collect()
}

/// Republish changed local endpoints to every ready session, and hand the
/// same addresses to the link layer (route selection does not dial its own
/// addresses; the network thread never enumerates interfaces itself).
pub(super) fn refresh_endpoints(state: &NetworkState) {
    let local = state.links.local_addr();
    let addresses = local_addresses(local);
    state.links.set_local_addresses(addresses.clone());
    let Ok(endpoints) = endpoint_values(&addresses, local.port()) else {
        return;
    };
    let changed = edit(
        &mut state.local_facts.lock().expect("local facts mutex poisoned"),
        |raw| {
            raw.insert(10, endpoints);
            Ok(())
        },
    );
    if !matches!(changed, Ok(true)) {
        return;
    }
    let group = state
        .state
        .lock()
        .expect("group state mutex poisoned")
        .roster()
        .anchor()
        .group;
    let controls = state.controls.lock().expect("mobile control mutex poisoned");
    for (&link, session) in controls.iter().filter(|(_, session)| session.ready) {
        if send(state, session, group).is_err() {
            state
                .links
                .disconnect(link, oink_network::link::Disconnect::ControlStreamFailed);
        }
    }
}

pub(super) fn new(
    boot: Boot,
    advert: &Advert,
    profile: &OfferProfile,
    local: std::net::SocketAddr,
    form_factor: Option<dto::FormFactor>,
) -> v2::Result<dto::DeviceFacts> {
    let capabilities = capabilities(profile)?;
    let options = dto::LocalOptionsSummary::from_raw(
        [
            (1, true.write()),
            (3, 1f32.write()),
            (4, false.write()),
            (5, false.write()),
            (6, false.write()),
        ]
        .into(),
    )?;
    let observed = dto::ObservedState::from_raw(
        [
            (1, true.write()),
            (2, dto::RunIntent::Running.write()),
            (4, 0u64.write()),
        ]
        .into(),
    )?;
    let pause = dto::LocalPauseState::from_raw(
        [
            (1, false.write()),
            (2, dto::PauseReason::None.write()),
            (3, 1u64.write()),
        ]
        .into(),
    )?;
    dto::DeviceFacts::from_raw(
        [
            (1, wire::boot(boot).write()),
            (2, 1u64.write()),
            (
                3,
                RawValue::Map([(1, v2::MAJOR.write()), (2, v2::MINOR.write())].into()),
            ),
            (4, RawValue::from(advert.build.as_str())),
            (5, RawValue::Bytes(advert.group.0.to_vec())),
            (6, RawValue::from(advert.name.as_str())),
            (7, RawValue::from(advert.os.as_str())),
            (8, RawValue::from("")),
            (9, RawValue::from(advert.build.as_str())),
            (10, endpoints(local)?),
            (11, observed.write()),
            (12, RawValue::Array(Vec::new())),
            (13, capabilities.write()),
            (14, options.write()),
            (15, RawValue::Array(Vec::new())),
            (18, RawValue::Array(Vec::new())),
            (19, pause.write()),
            (25, dto::DeviceKind::Mobile.write()),
            (26, v2::MAJOR.write()),
            (27, v2::MINOR.write()),
        ]
        .into_iter()
        .chain(form_factor.map(|form| (31, form.write())))
        .collect(),
    )
}

pub(super) fn capabilities(profile: &OfferProfile) -> v2::Result<dto::FactCapabilities> {
    dto::FactCapabilities::from_raw(
        [
            (
                1,
                RawValue::Array(
                    profile
                        .capabilities
                        .iter()
                        .map(|cap| RawValue::from(cap.as_str()))
                        .collect(),
                ),
            ),
            (2, dto::CapturePermission::Indeterminate.write()),
            (3, RawValue::Array(profile.audio.iter().map(WireValue::write).collect())),
            (4, RawValue::Array(Vec::new())),
        ]
        .into(),
    )
}

fn edit(value: &mut dto::DeviceFacts, change: impl FnOnce(&mut v2::RawMap) -> v2::Result<()>) -> v2::Result<bool> {
    let mut raw = value.raw().clone();
    change(&mut raw)?;
    if raw == *value.raw() {
        return Ok(false);
    }
    raw.insert(
        2,
        value
            .fact_sequence()?
            .checked_add(1)
            .ok_or(v2::ErrorCode::StateConflict)?
            .write(),
    );
    let candidate = dto::DeviceFacts::from_raw(raw)?;
    candidate.encode(v2::limits::CONFIG_FRAME)?;
    *value = candidate;
    Ok(true)
}

pub(super) fn send(
    state: &NetworkState,
    session: &ControlSession,
    group: oink_foundation::GroupId,
) -> Result<(), MobileNetworkError> {
    let mut facts = state.local_facts.lock().expect("local facts mutex poisoned");
    edit(&mut facts, |raw| {
        raw.insert(5, RawValue::Bytes(group.0.to_vec()));
        Ok(())
    })
    .map_err(protocol_start)?;
    let bytes = v2::frame::encode(&v2::Message::ConfigFacts(facts.clone()), v2::limits::CONFIG_FRAME)
        .map_err(protocol_start)?;
    session
        .tx
        .post(bytes)
        .map_err(|error| MobileNetworkError::Stream(error.to_string()))
}

pub(super) fn receive(session: &mut ControlSession, group: &State, facts: dto::DeviceFacts) -> v2::Result<bool> {
    if !session.ready
        || wire::read_boot(&facts.boot()?)? != session.peer_boot
        || facts.group()?.0 != group.roster().anchor().group.0
        || !group.roster().contains(session.peer_boot.device_id)
    {
        return Err(v2::ErrorCode::AuthenticationFailed.into());
    }
    let next = facts.fact_sequence()?;
    if session
        .facts
        .as_ref()
        .is_some_and(|previous| previous.fact_sequence().is_ok_and(|old| old >= next))
    {
        return Ok(false);
    }
    session.facts = Some(facts);
    Ok(true)
}

impl MobileNetwork {
    pub fn set_android_input_display(
        &self,
        display: Option<crate::android_input::AndroidDisplay>,
    ) -> Result<(), MobileNetworkError> {
        if self.state.advert.os != "android" {
            return Err(MobileNetworkError::Protocol(
                "Android input requires an Android session".into(),
            ));
        }
        self.edit_local_facts(|raw| {
            let mut displays = Vec::new();
            if let Some(display) = display.filter(|display| display.valid()) {
                let bounds = RawValue::Map(
                    [
                        (1, 0.0f64.write()),
                        (2, 0.0f64.write()),
                        (3, f64::from(display.width).write()),
                        (4, f64::from(display.height).write()),
                    ]
                    .into(),
                );
                displays.push(
                    dto::DisplayFact::from_raw(
                        [
                            (1, RawValue::from(oink_foundation::display::ANDROID_INPUT_DISPLAY)),
                            (2, RawValue::from("Built-in display")),
                            (3, bounds),
                            (5, display.width.write()),
                            (6, display.height.write()),
                            (8, display.rotation.write()),
                            (9, 1.0f64.write()),
                            (10, true.write()),
                            (11, true.write()),
                            (12, false.write()),
                        ]
                        .into(),
                    )?
                    .write(),
                );
            }
            raw.insert(12, RawValue::Array(displays));
            Ok(())
        })?;
        Ok(())
    }

    /// DeviceFacts keys 4 (build) and 9 (app_version): the installed App's
    /// version as the host reports it. Without it both carry this crate's
    /// version, which is the workspace version and not the App release (PCs
    /// listed 0.4.0 in-house phones as 0.3.6). Returns whether the facts
    /// changed.
    pub fn set_app_version(&self, version: &str) -> Result<bool, MobileNetworkError> {
        self.edit_local_facts(|raw| {
            raw.insert(4, RawValue::from(version));
            raw.insert(9, RawValue::from(version));
            Ok(())
        })
    }

    /// DeviceFacts key 32: the Android privileged helper is enabled and
    /// running. Peers draw the crowned Android badge from it and PCs decide
    /// whether they may open this phone's audio endpoints. Published only
    /// while true; absence is false. Returns whether the facts changed.
    pub fn set_android_privileged(&self, running: bool) -> Result<bool, MobileNetworkError> {
        self.edit_local_facts(|raw| {
            if running {
                raw.insert(32, true.write());
            } else {
                raw.remove(&32);
            }
            Ok(())
        })
    }

    /// What this device currently publishes in DeviceFacts key 32.
    pub fn local_android_privileged(&self) -> bool {
        self.state
            .local_facts
            .lock()
            .expect("local facts mutex poisoned")
            .android_privileged()
            .unwrap_or(false)
    }

    pub fn group_id(&self) -> oink_foundation::GroupId {
        self.state
            .state
            .lock()
            .expect("group state mutex poisoned")
            .roster()
            .anchor()
            .group
    }

    pub fn peer_facts(&self, peer: DeviceId) -> Option<dto::DeviceFacts> {
        let snapshot = self.links();
        let link = snapshot
            .by_peer(peer)
            .filter(|link| link.state == oink_network::link::LinkState::Ready)?;
        let controls = self.state.controls.lock().ok()?;
        let session = controls.get(&link.link)?;
        let facts = session.facts.as_ref()?;
        (session.ready && session.peer_boot == link.peer_boot && facts.group().ok()?.0 == self.group_id().0)
            .then(|| facts.clone())
    }

    pub fn local_fact_sequence(&self) -> Result<u64, MobileNetworkError> {
        self.state
            .local_facts
            .lock()
            .expect("local facts mutex poisoned")
            .fact_sequence()
            .map_err(protocol_start)
    }

    pub fn edit_local_facts(
        &self,
        change: impl FnOnce(&mut v2::RawMap) -> v2::Result<()>,
    ) -> Result<bool, MobileNetworkError> {
        let changed = edit(
            &mut self.state.local_facts.lock().expect("local facts mutex poisoned"),
            change,
        )
        .map_err(protocol_start)?;
        if changed {
            let controls = self.state.controls.lock().expect("mobile control mutex poisoned");
            let group = self.group_id();
            for (&link, session) in controls.iter().filter(|(_, session)| session.ready) {
                if send(&self.state, session, group).is_err() {
                    self.state
                        .links
                        .disconnect(link, oink_network::link::Disconnect::ControlStreamFailed);
                }
            }
        }
        Ok(changed)
    }
}
