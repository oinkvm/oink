//! [`AgentApi`] 的真实实现（界面与命令行调用服务进程）。类型与 JSON 约定见 [`crate::api`]。
//!
//! 命令可能阻塞（等网络）：异步的组网调用放到服务进程的运行时上执行，调用线程用通道等待结果，
//! 所以可以从任意线程调用（界面在后台线程、命令行在主线程、本机 IPC 在阻塞线程池里）。

use std::future::Future;

use crate::config::SpeedMode;
use crate::threads::input::InputCommand;
use oink_display::layout as dl;
use oink_foundation::input::PermissionState;
use oink_foundation::{DeviceId, Point, Size};
use oink_protocol::v1::{
    dto::{self, DeviceFacts},
    schema::WireValue,
};
use oink_routing::ButtonHandoff;

use crate::api::*;
use crate::layout_sync::Source;
use crate::node::{Node, lock};

fn now_ms() -> i64 {
    crate::layout_sync::now_ms() as i64
}

fn unavailable(msg: impl Into<String>) -> ApiError {
    ApiError::Unavailable { message: msg.into() }
}

fn not_found(msg: impl Into<String>) -> ApiError {
    ApiError::NotFound { message: msg.into() }
}

fn invalid(msg: impl Into<String>) -> ApiError {
    ApiError::InvalidArgument { message: msg.into() }
}

fn command_err(message: String) -> ApiError {
    ApiError::InvalidState { message }
}

/// 在服务进程的运行时上执行异步操作并等待结果（任意线程可调用）。
fn block<F>(node: &Node, f: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    node.inner.rt.spawn(async move {
        let _ = tx.send(f.await);
    });
    rx.recv().expect("runtime dropped the task")
}

fn apply_layout(node: &Node, source: Source, edit: impl FnOnce(&mut dl::Layout) + Send + 'static) -> ApiResult<()> {
    let task = node.clone();
    block(node, async move { task.edit_layout(source, edit).await }).map_err(command_err)
}
fn try_apply_layout(
    node: &Node,
    edit: impl FnOnce(&mut dl::Layout) -> ApiResult<()> + Send + 'static,
) -> ApiResult<()> {
    let task = node.clone();
    block(node, async move { task.try_edit_layout(edit).await })
}

pub(crate) fn parse_os(s: &str) -> Os {
    match s {
        "macos" => Os::Macos,
        "windows" => Os::Windows,
        "linux" => Os::Linux,
        _ => Os::Other,
    }
}

pub(crate) fn local_os_version() -> String {
    // A status refresh runs on the same consumer as input messages. The OS
    // version is constant during this process; spawning sw_vers for every
    // MembersChanged/Overview blocks that consumer during ordinary Low traffic.
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(detect_local_os_version).clone()
}

#[cfg(all(test, debug_assertions))]
static OS_VERSION_PROBES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn detect_local_os_version() -> String {
    #[cfg(all(test, debug_assertions))]
    OS_VERSION_PROBES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|v| format!("macOS {}", v.trim()))
            .unwrap_or_else(|| "macOS".into())
    }
    #[cfg(windows)]
    {
        "Windows".into()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        std::env::consts::OS.into()
    }
}

#[cfg(all(test, debug_assertions))]
#[test]
fn status_readers_probe_the_os_version_only_once() {
    let readers: Vec<_> = (0..8).map(|_| std::thread::spawn(local_os_version)).collect();
    let versions: Vec<_> = readers.into_iter().map(|thread| thread.join().unwrap()).collect();
    assert!(versions.iter().all(|version| version == &versions[0]));
    assert!(!versions[0].is_empty());
    assert_eq!(OS_VERSION_PROBES.load(std::sync::atomic::Ordering::Relaxed), 1);
}

fn summary(node: &Node, id: DeviceId, name: String, os: &str) -> DeviceSummary {
    let me = id == node.id();
    DeviceSummary {
        id: id.into(),
        fingerprint: id.short(),
        name,
        os: parse_os(os),
        os_version: if me { local_os_version() } else { String::new() },
        model: None,
        app_version: if me {
            env!("OINK_RELEASE_VERSION").to_string()
        } else {
            String::new()
        },
    }
}

fn sharing_state(node: &Node, peer: DeviceId) -> SharingState {
    let ready = node.links().is_ok_and(|links| links.links().ready(peer));
    let attached = node
        .input_status()
        .is_ok_and(|input| input.peers.iter().any(|p| p.peer == peer && p.attached));
    SharingState::derive(ready, node.fully_paused(), attached)
}
fn peer_state(node: &Node, peer: DeviceId) -> PeerState {
    if peer == node.id() {
        return PeerState::Online;
    }
    if let Ok(links) = node.links()
        && let Some(link) = links.links().by_peer(peer)
    {
        return if link.state == oink_network::link::LinkState::Ready {
            PeerState::Online
        } else {
            PeerState::Connecting
        };
    }
    if node.link_config().is_ok_and(|group| {
        group
            .members()
            .members
            .iter()
            .any(|m| m.device == peer && m.address_stale)
    }) {
        return PeerState::AddressStale {
            last_address: node
                .device_facts(peer)
                .and_then(|f| f.endpoints().ok()?.0.first().and_then(endpoint_address))
                .unwrap_or_default(),
        };
    }
    PeerState::Offline { last_seen_ms: None }
}
fn fact_summary(node: &Node, facts: &DeviceFacts) -> Option<DeviceSummary> {
    let mut out = summary(
        node,
        oink_network::wire_identity::read_device(&facts.boot().ok()?.device_id().ok()?).ok()?,
        facts.name().ok()?.0,
        &facts.os().ok()?.0,
    );
    out.os_version = facts.os_version().ok()?.0;
    out.app_version = facts.app_version().ok()?.0;
    Some(out)
}
fn device_summary(node: &Node, id: DeviceId) -> DeviceSummary {
    if let Some(f) = node.device_facts(id).and_then(|facts| fact_summary(node, &facts)) {
        return f;
    }
    let member = node
        .link_config()
        .ok()
        .and_then(|g| g.members().members.iter().find(|m| m.device == id).cloned());
    summary(
        node,
        id,
        node.device_name(id),
        member.as_ref().map(|m| m.os.as_str()).unwrap_or(""),
    )
}
fn link_info(node: &Node, peer: DeviceId) -> Option<LinkInfo> {
    let links = node.links().ok()?.links();
    let link = links.by_peer(peer)?;
    Some(LinkInfo {
        kind: match link.path {
            oink_network::link::LinkPath::Direct => LinkKind::Direct,
            oink_network::link::LinkPath::Relayed { via } => LinkKind::Relay {
                via: via.into(),
                via_name: node.device_name(via),
            },
        },
        remote_address: link.remote_address.map(|address| address.to_string()),
        rtt_ms: link.rtt.map(|r| r.as_secs_f64() * 1000.),
        loss_pct: if link.sent_packets == 0 {
            0.
        } else {
            link.lost_packets as f64 / link.sent_packets as f64 * 100.
        },
        bulk: Some(if link.low_available { "available" } else { "unavailable" }.into()),
        bulk_unavailable: link.low_failure.as_ref().map(|failure| BulkUnavailableInfo {
            from: failure.from.into(),
            to: failure.to.into(),
            reason: format!("{:?}", failure.reason),
            port: None,
            message: failure.to_string(),
        }),
    })
}
pub(crate) fn group_view(node: &Node) -> ApiResult<GroupView> {
    let group = node.link_config()?;
    let state = group.applied();
    let input = node.input_status().ok();
    let mut members = group.members().members.clone();
    members.sort_by_key(|m| (!m.is_self, m.name.clone()));
    Ok(GroupView {
        id: Some(state.group.into()),
        members: members
            .iter()
            .map(|m| {
                let facts = if m.is_self {
                    Some(group.local_facts())
                } else {
                    m.facts.clone().filter(|facts| {
                        group.peer_boot(m.device).is_some_and(|boot| {
                            facts.boot().ok().is_some_and(|fact| {
                                fact.device_id()
                                    .ok()
                                    .and_then(|id| oink_network::wire_identity::read_device(&id).ok())
                                    == Some(boot.device_id)
                                    && fact.nonce().ok().is_some_and(|nonce| nonce.0 == boot.nonce)
                            })
                        })
                    })
                };
                let options = facts.as_ref().and_then(|facts| facts.local_options().ok());
                let resources = facts.as_ref().and_then(|facts| facts.resources().ok().flatten());
                let mode = |mode: Option<dto::SharingMode>| match mode {
                    Some(dto::SharingMode::Shared) => Some(oink_foundation::input::InputMode::Shared),
                    Some(dto::SharingMode::Local) => Some(oink_foundation::input::InputMode::Local),
                    _ => None,
                };
                Member {
                    device: device_summary(node, m.device),
                    is_self: m.is_self,
                    state: peer_state(node, m.device),
                    reachable: m.is_self || m.online,
                    facts_fresh: facts.is_some(),
                    input_transform: m.is_self.then(|| {
                        InputTransform::from(node.settings().input_transform).with_local_exclusions(m.device, &m.os)
                    }),
                    screen_disabled: state.owners.get(&m.device).map(|owner| !owner.screen_shared),
                    screen_local_input: options.as_ref().and_then(|options| options.screen_local_input().ok()),
                    mouse_mode: mode(
                        resources
                            .as_ref()
                            .and_then(|resources| resources.mouse_mode().ok().flatten()),
                    ),
                    keyboard_mode: mode(
                        resources
                            .as_ref()
                            .and_then(|resources| resources.keyboard_mode().ok().flatten()),
                    ),
                    keyboard_disabled: options.as_ref().and_then(|options| options.keyboard_disabled().ok()),
                    sharing: sharing_state(node, m.device),
                    link: if m.is_self { None } else { link_info(node, m.device) },
                    added_by: m.added_by.map(|d| node.device_name(d)),
                    added_at_ms: m.added_at_ms as i64,
                    display_count: facts
                        .as_ref()
                        .and_then(|facts| facts.displays().ok())
                        .map_or(0, |displays| displays.0.len() as u32),
                    feel: if m.is_self {
                        Some(node.input_backend().feel_profile())
                    } else {
                        input
                            .as_ref()
                            .and_then(|i| i.peers.iter().find(|p| p.peer == m.device).and_then(|p| p.feel.clone()))
                    },
                }
            })
            .collect(),
    })
}

fn endpoint_address(endpoint: &dto::EndpointFact) -> Option<String> {
    let bytes = endpoint.address().ok()?.0;
    let ip = match endpoint.family().ok()? {
        dto::AddressFamily::Ipv4 => std::net::IpAddr::V4(std::net::Ipv4Addr::from(<[u8; 4]>::try_from(bytes).ok()?)),
        dto::AddressFamily::Ipv6 => std::net::IpAddr::V6(std::net::Ipv6Addr::from(<[u8; 16]>::try_from(bytes).ok()?)),
        _ => return None,
    };
    Some(std::net::SocketAddr::new(ip, endpoint.port().ok()?).to_string())
}

fn local_permission_platform() -> (Os, i64) {
    #[cfg(target_os = "macos")]
    let major = objc2_foundation::NSProcessInfo::processInfo()
        .operatingSystemVersion()
        .majorVersion as i64;
    #[cfg(not(target_os = "macos"))]
    let major = 0;
    (parse_os(std::env::consts::OS), major)
}

fn platform_permission_kinds(os: Os, macos_major: i64) -> &'static [PermissionKind] {
    use PermissionKind::*;
    match os {
        // Local Network privacy and its Settings pane were introduced in macOS 15.
        // Earlier releases must not ask users to grant a permission that does not exist.
        Os::Macos if macos_major >= 15 => &[Accessibility, InputMonitoring, LocalNetwork],
        Os::Macos => &[Accessibility, InputMonitoring],
        Os::Windows => &[Firewall, SystemService],
        Os::Linux | Os::Other => &[],
    }
}

fn permission_settings_target(kind: PermissionKind, os: Os, macos_major: i64) -> Option<&'static str> {
    if !platform_permission_kinds(os, macos_major).contains(&kind)
        && !(os == Os::Macos && kind == PermissionKind::AudioCapture)
    {
        return None;
    }
    match os {
        Os::Macos => kind.macos_settings_url(),
        Os::Windows => kind.windows_settings_uri(),
        Os::Linux | Os::Other => None,
    }
}

// A current-access Boolean does not report whether an authorization alert was
// presented. In particular AX is asynchronous and IOHID's false can accompany
// its Settings alert. Never open Settings alongside those possible dialogs.
#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy)]
enum PermissionRequestOutcome {
    Granted,
    MayPrompt,
    Unavailable,
}

#[cfg(any(target_os = "macos", test))]
fn permission_request_action(
    kind: PermissionKind,
    before: PermissionState,
    request: impl FnOnce() -> PermissionRequestOutcome,
    open_settings: impl FnOnce() -> ApiResult<()>,
) -> ApiResult<()> {
    match request() {
        PermissionRequestOutcome::Granted => Ok(()),
        PermissionRequestOutcome::Unavailable => open_settings(),
        PermissionRequestOutcome::MayPrompt
            if before == PermissionState::Denied
                && matches!(kind, PermissionKind::LocalNetwork | PermissionKind::AudioCapture) =>
        {
            // A prior, observed decision cannot be re-prompted by these APIs.
            // A denial resulting from THIS request must not take this branch.
            open_settings()
        }
        PermissionRequestOutcome::MayPrompt => Ok(()),
    }
}

pub(crate) fn permission_items(node: &Node) -> Vec<PermissionItem> {
    let (os, major) = local_permission_platform();
    let p = node.input_backend().permissions();
    #[cfg(windows)]
    if os == Os::Windows {
        let observed = if node.inner.native_services {
            *node.inner.windows_access.load()
        } else {
            crate::windows_permissions::Observation::UNKNOWN
        };
        return observed.items(
            node.links()
                .is_ok_and(|links| links.links().ready_count() > 0 && links.tcp_inbound_verified()),
        );
    }
    platform_permission_kinds(os, major)
        .iter()
        .map(|&kind| PermissionItem {
            kind,
            state: match kind {
                PermissionKind::Accessibility => p.accessibility,
                // Accessibility can permit an event tap too. Its successful
                // attachment must not manufacture a separate IM grant.
                PermissionKind::InputMonitoring => p.input_monitoring,
                #[cfg(target_os = "macos")]
                PermissionKind::LocalNetwork if node.inner.native_services => mac_permissions::local_network_state(),
                _ => PermissionState::NotDetermined,
            },
            required: kind != PermissionKind::SystemService,
        })
        .collect()
}

// One actionable issue can require several grants. Keep the individual typed
// observations in one stable alert instead of emitting identical notices per grant.
fn missing_permission_alert(permissions: Vec<PermissionItem>, capture: Option<bool>, now: i64) -> Option<Alert> {
    use crate::device_cards::{UserFacingCode as Code, UserFacingProblem};
    let permissions: Vec<_> = permissions
        .into_iter()
        .filter(|p| {
            p.required
                && if matches!(p.kind, PermissionKind::Accessibility | PermissionKind::InputMonitoring) {
                    !crate::node::granted(p.state)
                } else {
                    p.state == PermissionState::Denied
                }
        })
        .collect();
    if permissions.is_empty() {
        return None;
    }
    let level = if capture == Some(true) && permissions.iter().all(|p| p.kind == PermissionKind::InputMonitoring) {
        AlertLevel::Warning
    } else {
        AlertLevel::Error
    };
    let code = if permissions.iter().all(|p| p.state == PermissionState::Denied) {
        Code::PermissionDenied
    } else {
        Code::PermissionRequired
    };
    Some(Alert {
        id: "permissions-missing".into(),
        level,
        kind: AlertKind::PermissionMissing { permissions },
        problem: Some(UserFacingProblem {
            code,
            unknown_code: None,
            native_code: None,
            retry_after_ms: None,
        }),
        at_ms: now,
    })
}

fn alerts(node: &Node) -> Vec<Alert> {
    use crate::device_cards::{UserFacingCode as Code, UserFacingProblem};
    let problem = |code| {
        Some(UserFacingProblem {
            code,
            unknown_code: None,
            native_code: None,
            retry_after_ms: None,
        })
    };
    let mut out = Vec::new();
    let now = now_ms();
    let capture = node.input_backend().capture_active();
    if lock(&node.inner.group_unreadable).is_some() {
        out.push(Alert {
            id: "group-state-unreadable".into(),
            level: AlertLevel::Error,
            kind: AlertKind::GroupStateUnreadable,
            problem: problem(Code::StateConflict),
            at_ms: now,
        });
    }
    let input = node.input_backend().permissions();
    let mut permissions = vec![
        PermissionItem {
            kind: PermissionKind::Accessibility,
            state: input.accessibility,
            required: true,
        },
        PermissionItem {
            kind: PermissionKind::InputMonitoring,
            state: input.input_monitoring,
            required: true,
        },
    ];
    permissions.extend(
        permission_items(node)
            .into_iter()
            .filter(|p| !matches!(p.kind, PermissionKind::Accessibility | PermissionKind::InputMonitoring)),
    );
    if let Some(alert) = missing_permission_alert(permissions, capture, now) {
        out.push(alert);
    }
    if capture == Some(false) && !node.fully_paused() && crate::node::capture_permitted(&input) {
        out.push(Alert {
            id: "capture-unavailable".into(),
            level: AlertLevel::Error,
            kind: AlertKind::CaptureUnavailable,
            problem: problem(Code::CaptureUnavailable),
            at_ms: now,
        });
    }
    let pause = node.pause_status();
    if pause.paused
        && pause.emergency
        && let Some(device) = pause.author
    {
        out.push(Alert {
            id: "emergency-stop".into(),
            level: AlertLevel::Warning,
            kind: AlertKind::EmergencyStop { device },
            problem: None,
            at_ms: now,
        });
    }
    if input.secure_input_active {
        out.push(Alert {
            id: "secure-input".into(),
            level: AlertLevel::Warning,
            kind: AlertKind::SecureInput { app: None },
            problem: problem(Code::SecureInput),
            at_ms: now,
        });
    }
    let missing: Vec<_> = node
        .layout()
        .0
        .effective(&node.layout_owners())
        .screens()
        .iter()
        .filter(|screen| {
            !node.device_facts(screen.id.device).is_some_and(|f| {
                f.displays().is_ok_and(|displays| {
                    displays
                        .0
                        .iter()
                        .any(|d| d.key().is_ok_and(|key| key.0 == screen.id.display))
                })
            })
        })
        .map(|s| s.id.clone())
        .collect();
    for screen in missing {
        out.push(Alert {
            id: format!("display-info:{}:{}", screen.device.to_hex(), screen.display),
            level: if node.links().is_ok_and(|l| l.links().ready(screen.device)) {
                AlertLevel::Warning
            } else {
                AlertLevel::Info
            },
            kind: AlertKind::DisplayInfoUnavailable {
                screen: (&screen).into(),
            },
            problem: None,
            at_ms: now,
        });
    }
    let mut current = lock(&node.inner.current_alerts);
    for alert in &mut out {
        if let Some(previous) = current.iter().find(|previous| previous.id == alert.id) {
            let mut candidate = alert.clone();
            candidate.at_ms = previous.at_ms;
            if &candidate == previous {
                alert.at_ms = previous.at_ms;
            }
        }
    }
    *current = out.clone();
    out
}

pub(crate) fn overview(node: &Node) -> ApiResult<Overview> {
    let members = node.group_members();
    let settings = node.settings();
    let perms = permission_items(node);
    // 截获没在运行、键鼠权限不全（含「未询问」）、或必需的权限被拒绝：部分不可用。
    let degraded = !node.input_backend().capture_active().unwrap_or(false)
        || !crate::node::capture_permitted(&node.input_backend().permissions())
        || perms
            .iter()
            .any(|p| p.required && matches!(p.state, PermissionState::Denied));
    Ok(Overview {
        this_device: device_summary(node, node.id()),
        group: node.group_id().map(|id| GroupSummary {
            id: id.into(),
            member_count: members.len() as u32,
            online_count: members
                .iter()
                .filter(|id| **id == node.id() || node.links().is_ok_and(|l| l.links().ready(**id)))
                .count() as u32,
        }),
        service: if node.fully_paused() {
            ServiceState::Paused
        } else if degraded {
            ServiceState::Degraded
        } else {
            ServiceState::Running
        },
        sharing_enabled: !node.fully_paused(),
        pause: node.pause_status(),
        focus_lock: node
            .input_status()
            .ok()
            .and_then(|s| s.locked_device)
            .map(|d| device_summary(node, d)),
        focus: node.focus(),
        onboarding_done: settings.onboarding_done,
        alerts: alerts(node),
    })
}

pub(crate) fn display_fact(f: &dto::DisplayFact) -> Option<oink_foundation::display::DisplayInfo> {
    let bounds = f.bounds().ok()?;
    let extent = f.reference_extent().ok()?;
    Some(oink_foundation::display::DisplayInfo {
        key: f.key().ok()?.0,
        name: f.name().ok()?.0,
        bounds: oink_foundation::Rect::new(bounds.x().ok()?, bounds.y().ok()?, bounds.w().ok()?, bounds.h().ok()?),
        pixel_width: f.pixel_width().ok()?,
        pixel_height: f.pixel_height().ok()?,
        reference_extent: match extent {
            Some(extent) => Some(Size::new(extent.w().ok()?, extent.h().ok()?)),
            None => None,
        },
        rotation_deg: f.rotation_deg().ok()?,
        is_primary: f.is_primary().ok()?,
        is_builtin: f.is_builtin().ok()?,
        is_virtual: f.is_virtual().ok()?,
    })
}

pub(crate) fn geometry(node: &Node, layout: &dl::Layout) -> LayoutGeometry {
    let os: std::collections::HashMap<DeviceId, String> = node
        .known_facts()
        .into_iter()
        .filter_map(|f| {
            Some((
                oink_network::wire_identity::read_device(&f.boot().ok()?.device_id().ok()?).ok()?,
                f.os().ok()?.0,
            ))
        })
        .collect();
    layout_geometry(
        layout,
        &node.layout_owners(),
        |d| node.online_set().contains(d),
        |id| {
            let display = node.device_facts(id.device).and_then(|f| {
                f.displays()
                    .ok()?
                    .0
                    .iter()
                    .find(|d| d.key().is_ok_and(|key| key.0 == id.display))
                    .and_then(display_fact)
            });
            Some(ScreenMeta {
                device_name: node.device_name(id.device),
                os: parse_os(os.get(&id.device).map(String::as_str).unwrap_or("")),
                display,
            })
        },
    )
}

pub(crate) fn layout_api(node: &Node) -> ApiResult<Layout> {
    let (layout, v, _) = node.layout();
    Ok(Layout {
        revision: v.revision,
        source: match v.source {
            Source::Default => LayoutSource::Default,
            Source::Manual => LayoutSource::Manual,
        },
        geometry: geometry(node, &layout),
    })
}

fn screen_preference_patch(
    display: String,
    key: u32,
    value: oink_protocol::v1::RawValue,
) -> oink_protocol::v1::RawValue {
    use oink_protocol::v1::RawValue;
    RawValue::Map(
        [(
            1,
            RawValue::Map(
                [(
                    1,
                    RawValue::Array(vec![RawValue::Map([(1, RawValue::Text(display)), (key, value)].into())]),
                )]
                .into(),
            ),
        )]
        .into(),
    )
}

fn screen_id(k: &ScreenKey) -> dl::ScreenId {
    dl::ScreenId::new(k.device.0, k.display.clone())
}

pub(crate) fn default_hotkeys() -> Vec<Hotkey> {
    let k = |a, keys: &[&str]| Hotkey {
        action: a,
        keys: keys.iter().map(|s| s.to_string()).collect(),
        // Default bindings are enabled and consume matching key combinations.
        enabled: true,
    };
    vec![
        k(HotkeyAction::LockFocus, &["Ctrl", "Alt", "L"]),
        k(HotkeyAction::ReturnPointerHome, &["Ctrl", "Alt", "H"]),
        k(HotkeyAction::ToggleSharing, &["Ctrl", "Alt", "P"]),
        k(HotkeyAction::PauseAll, &["Ctrl", "Alt", "Shift", "P"]),
        k(HotkeyAction::OpenWindow, &["Ctrl", "Alt", "A"]),
        k(HotkeyAction::ToggleGameMode, &["Ctrl", "Alt", "G"]),
    ]
}

pub(crate) fn input_settings(node: &Node) -> InputSettings {
    let s = node.settings();
    let r = &s.routing;
    let layout_settings = node.layout().0.settings;
    let speed_mode = s.speed_mode;
    let contacts_injection_available = node.contacts_injection_available();
    InputSettings {
        mac_caps_switches_input_source: node.input_backend().caps_lock_switches_input_source(),
        key_support: KeySupport::default(),
        event_frequency: s.event_frequency,
        accept_contacts: s.accept_contacts && contacts_injection_available,
        contacts_injection_available,
        mac_scroll_feel: s.routing.mac_scroll_feel,
        forward_injected_input: s.routing.forward_injected_input,
        emergency_hold_ms: s.emergency_hold_ms.get(),
        device_gain: s.screen_preferences.device_gain,
        scroll: ScrollSettings {
            speed: r.remote_scroll_speed,
        },
        pointer: PointerSettings {
            reference: match &r.fixed_reference_display {
                Some(d) => PointerReference::Fixed { display: d.clone() },
                None => PointerReference::LastScreen,
            },
            scaling: match speed_mode {
                SpeedMode::Physical => PointerScaling::Physical,
                SpeedMode::ScreenProportional => PointerScaling::Proportional,
            },
            drag_across_devices: r.button_handoff == ButtonHandoff::Allow,
        },
        hotkeys: crate::hotkeys::configured(s.hotkeys.clone()),
        gestures_enabled: r.forward_gestures,
        return_pointer_on_display_sleep: r.return_pointer_on_display_sleep,
        wake_displays_on_input: r.wake_displays_on_input,
        cursor_trail: r.cursor_trail,
        cursor_trail_duration_ms: s.cursor_trail_duration_ms,
        gap_travel: layout_settings.gap_travel,
    }
}

fn check_point(p: Point) -> ApiResult<()> {
    if p.x.is_finite() && p.y.is_finite() {
        Ok(())
    } else {
        Err(invalid("位置必须为有限数值"))
    }
}
fn check_size(s: Size) -> ApiResult<()> {
    if s.w.is_finite() && s.h.is_finite() && s.w > 0.0 && s.h > 0.0 {
        Ok(())
    } else {
        Err(invalid("尺寸必须为正的有限数值"))
    }
}

impl AgentApi for Node {
    fn layout_profile_command(&self, command: crate::layout_profiles::types::Command) -> ApiResult<serde_json::Value> {
        Ok(Node::layout_profile_command(self, command))
    }
    fn device_cards(&self) -> ApiResult<crate::device_cards::DeviceCardsView> {
        self.device_cards_snapshot()
    }
    fn invoke_device_card_action(
        &self,
        request_id: String,
        token: String,
    ) -> ApiResult<crate::device_cards::DeviceControlReply> {
        Ok(self.card_control(request_id, token, crate::device_controls::requests::action_request))
    }
    fn set_forwarding_controls(
        &self,
        request_id: String,
        version_token: String,
        volume_percent: u16,
        muted: bool,
    ) -> ApiResult<crate::device_cards::DeviceControlReply> {
        Ok(self.card_control(request_id, version_token, |binding, id| {
            crate::device_controls::requests::forwarding_request(binding, id, volume_percent, muted)
        }))
    }
    fn set_exit_volume(
        &self,
        request_id: String,
        version_token: String,
        volume_percent: u8,
        muted: bool,
    ) -> ApiResult<crate::device_cards::DeviceControlReply> {
        Ok(self.card_control(request_id, version_token, |binding, id| {
            crate::device_controls::requests::volume_request(binding, id, volume_percent, muted)
        }))
    }
    fn set_local_paused(&self, request_id: String, paused: bool) -> ApiResult<crate::device_cards::DeviceControlReply> {
        Ok(self.local_pause_control(request_id, paused))
    }
    fn set_device_paused(
        &self,
        request_id: String,
        token: String,
        paused: bool,
    ) -> ApiResult<crate::device_cards::DeviceControlReply> {
        Ok(self.card_control(request_id, token, |binding, id| {
            if binding.operation != (crate::device_cards::TokenOperation::Pause { paused }) {
                return Err(crate::device_cards::DeviceControlFailure::InvalidValue);
            }
            crate::device_controls::requests::action_request(binding, id)
        }))
    }
    fn pause_all(&self, request_id: String) -> ApiResult<crate::device_cards::PauseAllAccepted> {
        self.pause_all_control(request_id)
    }
    fn member_addresses(&self, device: DeviceKey) -> ApiResult<Vec<String>> {
        self.link_config()?.member_addresses(device.0)
    }
    fn set_member_address(&self, device: DeviceKey, address: String) -> ApiResult<()> {
        self.link_config()?.set_member_address(device.0, address)
    }
    fn nearby(&self) -> ApiResult<pairing::NearbySnapshot> {
        Ok((*self.link_config()?.nearby()).clone())
    }
    fn set_onboarding_connect(&self, active: bool) -> ApiResult<()> {
        self.link_config()?.set_onboarding_connect(active)
    }
    fn refresh_nearby(&self, operation_id: String) -> ApiResult<pairing::PairReply<pairing::Accepted>> {
        self.link_config()?.refresh_nearby(operation_id)
    }
    fn query_pairing_address(
        &self,
        operation_id: String,
        generation: crate::device_cards::DecimalU64,
        ip: String,
        port: Option<u16>,
    ) -> ApiResult<pairing::PairReply<pairing::Accepted>> {
        self.link_config()?.query_pairing_address(
            crate::api::request_context::current(),
            operation_id,
            generation.0,
            ip,
            port,
        )
    }
    fn release_pairing_query(&self, query_id: String, generation: crate::device_cards::DecimalU64) -> ApiResult<()> {
        self.link_config()?
            .release_pairing_query(crate::api::request_context::current(), query_id, generation.0)
    }
    fn start_pairing(
        &self,
        operation_id: String,
        target: pairing::PairTarget,
    ) -> ApiResult<pairing::PairReply<pairing::Started>> {
        self.link_config()?
            .start_pairing(crate::api::request_context::current(), operation_id, target)
    }
    fn pairings(&self) -> ApiResult<pairing::PairingSnapshot> {
        Ok((*self.link_config()?.pairings()).clone())
    }
    fn group_migration(&self) -> ApiResult<Option<pairing::GroupMigrationStatus>> {
        Ok((*self.link_config()?.group_migration()).clone())
    }
    fn accept_pairing(
        &self,
        pair_id: String,
        expected_revision: crate::device_cards::DecimalU64,
    ) -> ApiResult<pairing::PairReply<pairing::Accepted>> {
        self.link_config()?.accept_pairing(pair_id, expected_revision.0)
    }
    fn decline_pairing(
        &self,
        pair_id: String,
        expected_revision: crate::device_cards::DecimalU64,
    ) -> ApiResult<pairing::PairReply<pairing::Accepted>> {
        self.link_config()?.decline_pairing(pair_id, expected_revision.0)
    }
    fn cancel_pairing(
        &self,
        pair_id: String,
        expected_revision: crate::device_cards::DecimalU64,
    ) -> ApiResult<pairing::PairReply<pairing::CancelResult>> {
        self.link_config()?.cancel_pairing(pair_id, expected_revision.0)
    }
    fn pairing_code(&self, pair_id: String) -> ApiResult<pairing::CodeView> {
        self.link_config()?.pairing_code(pair_id)
    }
    fn submit_pairing_code(
        &self,
        pair_id: String,
        expected_revision: crate::device_cards::DecimalU64,
        attempt: u8,
        code: String,
    ) -> ApiResult<pairing::PairReply<pairing::Accepted>> {
        self.link_config()?
            .submit_pairing_code(pair_id, expected_revision.0, attempt, code)
    }
    fn undo_pairing(
        &self,
        operation_id: String,
        handle: pairing::UndoHandle,
    ) -> ApiResult<pairing::PairReply<pairing::Accepted>> {
        self.link_config()?.undo_pairing(operation_id, handle)
    }
    fn subscribe_pairing_events(
        &self,
        after_event_id: crate::device_cards::DecimalU64,
    ) -> ApiResult<Vec<pairing::PairingEvent>> {
        self.link_config()?.subscribe_pairing_events(after_event_id.0)
    }
    fn ack_pairing_event(&self, event_id: crate::device_cards::DecimalU64) -> ApiResult<()> {
        self.link_config()?.ack_pairing_event(event_id.0)
    }
    fn blocked_devices(&self) -> ApiResult<crate::device_cards::BlockedDevicesSnapshot> {
        self.link_config()?.blocked_devices()
    }
    fn block(&self, operation_id: String, device: DeviceKey) -> ApiResult<crate::device_cards::BlockReply> {
        self.link_config()?.block(operation_id, device.0)
    }
    fn unblock(&self, operation_id: String, device: DeviceKey) -> ApiResult<crate::device_cards::BlockReply> {
        self.link_config()?.unblock(operation_id, device.0)
    }
    fn notify(&self, notification: Notification) -> ApiResult<Option<NotificationReceipt>> {
        self.raise_notification(notification, None, 0)
    }
    fn ui_preferences(&self) -> ApiResult<UiPreferences> {
        Ok(UiPreferences::from(&self.settings()))
    }
    fn set_ui_preferences(&self, patch: PreferencePatch<UiPreferences>) -> ApiResult<UiPreferences> {
        let _gate = self.owner_gate();
        let applied = {
            let mut config = lock(&self.inner.config);
            let settings = patch.apply(&UiPreferences::from(&config.settings))?;
            config
                .update_local(&patch.value(), |current| settings.apply(current))
                .map_err(|error| ApiError::Internal {
                    message: format!("saving local preferences: {error}"),
                })?;
            UiPreferences::from(&config.settings)
        };
        self.apply_trail_appearance();
        self.apply_notification_preferences();
        // Native hosts and log filters consume only the durable applied value.
        crate::logs::set_max_mb(applied.log_max_mb.get().into()).map_err(|error| ApiError::Internal {
            message: format!("applying ordinary log limit: {error}"),
        })?;
        crate::logs::set_level(applied.log_level.as_str()).map_err(|error| ApiError::Internal {
            message: format!("applying log level: {error}"),
        })?;
        #[cfg(target_os = "macos")]
        crate::backend::set_status_icon_visible(applied.show_status_icon);
        self.emit(ApiEvent::UiPreferences(applied.clone()));
        Ok(applied)
    }

    fn submit_crash_capsule(
        &self,
        generation: u64,
        crash: oink_telemetry::SafeCrash,
        collected_at: u64,
    ) -> ApiResult<()> {
        self.inner
            .reports
            .submit_crash(generation, crash, collected_at)
            .map_err(|_| ApiError::Internal {
                message: "Crash capsule rejected".into(),
            })
    }
    fn report_status(&self) -> ApiResult<crate::telemetry::ReportStatus> {
        Ok(self.inner.reports.status())
    }
    fn set_report_consent(
        &self,
        expected_generation: u64,
        choice: oink_telemetry::Choice,
        policy_version: u16,
    ) -> ApiResult<()> {
        let result = self
            .inner
            .reports
            .set(expected_generation, choice, policy_version, |consent| {
                let mut config = self.inner.config.lock().map_err(|_| oink_telemetry::Error::Io)?;
                let previous = config.settings.reports;
                config.settings.reports = consent;
                if config.save().is_err() {
                    config.settings.reports = previous;
                    return Err(oink_telemetry::Error::Io);
                }
                Ok(())
            });
        // A failed save can still close reporting in memory and advance the
        // denial fence. Publish that actual off state so native capture revokes
        // even when the command must return a persistence failure.
        self.emit(ApiEvent::ReportStatusChanged(self.inner.reports.status()));
        result.map_err(|_| ApiError::Internal {
            message: "Report consent could not be stored; refresh report status".into(),
        })
    }

    fn reset_group_state(&self) -> ApiResult<()> {
        let node = self.clone();
        block(self, async move { node.reset_unreadable_group().await })
    }

    fn audio_status(&self) -> ApiResult<oink_audio::Status> {
        Ok(self
            .inner
            .audio
            .get()
            .ok_or_else(|| unavailable("音频尚未启动"))?
            .status())
    }
    fn refresh_audio_devices(&self) -> ApiResult<()> {
        self.inner
            .audio
            .get()
            .ok_or_else(|| unavailable("音频尚未启动"))?
            .refresh_devices()
            .map_err(unavailable)
    }
    fn set_audio_settings(&self, settings: PreferencePatch<oink_audio::Settings>) -> ApiResult<()> {
        let audio = self.inner.audio.get().ok_or_else(|| unavailable("音频尚未启动"))?;
        audio.configure(settings.clone()).map_err(|error| {
            tracing::warn!(%error, "audio settings request rejected");
            unavailable(error)
        })?;
        Ok(())
    }

    fn overview(&self) -> ApiResult<Overview> {
        overview(self)
    }

    fn game_mode(&self) -> ApiResult<GameModeStatus> {
        Ok(self.game_status().into())
    }
    fn set_screen_disabled(&self, device: DeviceKey, disabled: bool) -> ApiResult<()> {
        self.commit_manual_visibility(device, None, disabled, || {
            self.edit_device_option(device, dto::LocalOption::ScreenShared, (!disabled).write())
        })
    }
    fn set_input_transform(&self, device: DeviceKey, settings: InputTransform) -> ApiResult<()> {
        self.edit_device_option_with(device, dto::LocalOption::InputTransform, |current| {
            let current = dto::InputTransformValue::read(&current).map_err(|error| ApiError::InvalidState {
                message: error.to_string(),
            })?;
            let current = crate::config::read_transform(current).map_err(|error| ApiError::InvalidState {
                message: error.to_string(),
            })?;
            // Diff known projections so omitted future fields stay owner-owned.
            Ok(crate::config::owner::typed_patch(
                dto::LocalOption::InputTransform,
                &crate::config::write_transform(&current),
                &crate::config::write_transform(&settings.into()),
            ))
        })
    }
    fn set_keyboard_disabled(&self, device: DeviceKey, disabled: bool) -> ApiResult<()> {
        self.edit_device_option(device, dto::LocalOption::KeyboardDisabled, disabled.write())
    }
    fn set_game_mode(&self, mode: oink_routing::GameMode) -> ApiResult<()> {
        // Focus hotkeys use the runtime holder command. Device card actions use
        // the separate durable GameModePreference owner option.
        self.input_thread()?.send(InputCommand::GameMode(mode));
        Ok(())
    }
    fn lock_focus_to(&self, device: DeviceKey) -> ApiResult<()> {
        let (reply, result) = std::sync::mpsc::sync_channel(1);
        self.input_thread()?.send(InputCommand::LockFocusTo {
            target: device.0,
            reply,
        });
        result
            .recv_timeout(std::time::Duration::from_secs(10))
            .map_err(|_| unavailable("input owner did not acknowledge focus admission"))?
            .map_err(|reason| match reason {
                oink_routing::FocusLockError::Paused => command_err("local device is paused".into()),
                oink_routing::FocusLockError::Unavailable => unavailable("focus target is unavailable"),
            })
    }
    fn set_focus_locked(&self, locked: bool) -> ApiResult<()> {
        self.input_thread()?.send(InputCommand::FocusLock(locked));
        // The observer publishes the accepted group state after the holder replies.
        Ok(())
    }

    fn return_pointer_home(&self) -> ApiResult<()> {
        self.input_thread()?.send(InputCommand::ReturnPointerHome);
        Ok(())
    }

    fn group(&self) -> ApiResult<GroupView> {
        group_view(self)
    }

    fn rename_device(&self, device: DeviceKey, name: String) -> ApiResult<()> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(invalid("名称不能为空"));
        }
        self.edit_device_option(
            device,
            dto::LocalOption::DeviceName,
            oink_protocol::v1::RawValue::Text(name),
        )
    }

    fn remove_member(&self, device: DeviceKey) -> ApiResult<()> {
        let node = self.clone();
        block(self, async move { node.group_remove(device.0).await }).map_err(command_err)
    }
    fn leave_group(&self) -> ApiResult<()> {
        let node = self.clone();
        block(self, async move { node.group_leave().await }).map_err(command_err)
    }

    fn network_info(&self) -> ApiResult<NetworkInfo> {
        Ok(NetworkInfo {
            addresses: self
                .link_config()?
                .local_addresses()
                .iter()
                .map(|ip| LocalAddress {
                    ip: ip.to_string(),
                    interface: String::new(),
                    kind: InterfaceKind::Other,
                })
                .collect(),
            port: self.links()?.local_addr().port(),
        })
    }

    fn reset_initial_layout(&self) -> ApiResult<Layout> {
        let node = self.clone();
        block(self, async move { Node::reset_initial_layout(&node).await })
    }
    fn layout(&self) -> ApiResult<Layout> {
        layout_api(self)
    }

    fn save_layout(&self, placements: Vec<Placement>) -> ApiResult<Layout> {
        let (current, _, _) = Node::layout(self);
        let mut seen = std::collections::BTreeSet::new();
        for p in &placements {
            check_point(p.origin)?;
            if !p.scale.is_finite() || p.scale <= 0.0 || !seen.insert(p.device.0) {
                return Err(invalid("Invalid or duplicate computer placement"));
            }
            if current.device_transform(p.device.0).is_none() {
                return Err(not_found("Computer has no screens"));
            }
        }
        apply_layout(self, Source::Manual, move |layout| {
            for p in placements {
                layout.place_device(p.device.0, p.origin, p.scale, true);
            }
            // Adopt the submitted placements exactly. The canvas editor owns
            // collision checks and auto-compaction for its manual edits, and it
            // ignores hidden or disabled screens, which this layout cannot see;
            // undo/redo replays exact history values through this call too.
        })?;
        layout_api(self)
    }

    fn set_screen_gain(&self, screen: ScreenKey, gain: f64) -> ApiResult<()> {
        if !(0.1..=5.0).contains(&gain) {
            return Err(invalid("Gain must be within 0.1–5"));
        }
        let id = screen_id(&screen);
        if Node::layout(self).0.screen(&id).is_none() {
            return Err(not_found("没有这块屏幕"));
        }
        self.edit_device_option(
            screen.device,
            dto::LocalOption::ScreenPreferences,
            screen_preference_patch(screen.display, 2, gain.write()),
        )
    }

    fn set_screen_remote_gain(&self, screen: ScreenKey, gain: f64) -> ApiResult<()> {
        if !(0.1..=5.0).contains(&gain) {
            return Err(invalid("Gain must be within 0.1–5"));
        }
        let id = screen_id(&screen);
        if Node::layout(self).0.screen(&id).is_none() {
            return Err(not_found("没有这块屏幕"));
        }
        self.edit_device_option(
            screen.device,
            dto::LocalOption::ScreenPreferences,
            screen_preference_patch(screen.display, 4, gain.write()),
        )
    }

    fn set_screen_hidden(&self, screen: ScreenKey, hidden: bool) -> ApiResult<()> {
        let id = screen_id(&screen);
        if Node::layout(self).0.screen(&id).is_none() {
            return Err(not_found("没有这块屏幕"));
        }
        self.commit_manual_visibility(screen.device, Some(&screen.display), hidden, || {
            self.edit_device_option(
                screen.device,
                dto::LocalOption::ScreenPreferences,
                screen_preference_patch(screen.display.clone(), 3, (!hidden).write()),
            )
        })
    }

    fn layout_projection(&self) -> ApiResult<ProjectionStatus> {
        Ok(crate::projection::status(self))
    }
    fn set_layout_projection(&self, enabled: bool) -> ApiResult<ProjectionStatus> {
        Ok(crate::projection::set_mode(self, enabled))
    }
    fn preview_layout_projection(
        &self,
        gesture: u64,
        placements: Vec<Placement>,
        affected: Vec<ScreenKey>,
        ended: bool,
        arrangement: Option<Box<crate::layout_profiles::types::ArrangementSnapshot>>,
    ) -> ApiResult<()> {
        crate::projection::preview_ui_arrangement(self, gesture, placements, affected, ended, arrangement)
    }

    fn preview_move_screen(&self, screen: ScreenKey, origin: Point) -> ApiResult<Option<LayoutGeometry>> {
        check_point(origin)?;
        let l = Node::layout(self).0;
        let id = screen_id(&screen);
        if l.screen(&id).is_none() {
            return Err(not_found("没有这块屏幕"));
        }
        Ok(l.preview_move(&id, origin).map(|l| geometry(self, &l)))
    }

    fn preview_move_device(&self, device: DeviceKey, delta: Point) -> ApiResult<Option<LayoutGeometry>> {
        check_point(delta)?;
        Ok(Node::layout(self)
            .0
            .preview_move_device(device.0, delta)
            .map(|l| geometry(self, &l)))
    }

    fn move_screen(&self, screen: ScreenKey, origin: Point) -> ApiResult<Layout> {
        check_point(origin)?;
        try_apply_layout(self, move |l| {
            if l.move_screen(&screen_id(&screen), origin) {
                Ok(())
            } else {
                Err(not_found("没有这块屏幕"))
            }
        })?;
        layout_api(self)
    }

    fn move_device(&self, device: DeviceKey, delta: Point) -> ApiResult<Layout> {
        check_point(delta)?;
        try_apply_layout(self, move |l| {
            if l.move_device(device.0, delta) {
                Ok(())
            } else {
                Err(not_found("设备没有屏幕"))
            }
        })?;
        layout_api(self)
    }

    fn set_screen_size(&self, screen: ScreenKey, size: Size) -> ApiResult<Layout> {
        check_size(size)?;
        try_apply_layout(self, move |l| {
            if l.resize_screen(&screen_id(&screen), size) {
                Ok(())
            } else {
                Err(not_found("没有这块屏幕"))
            }
        })?;
        layout_api(self)
    }

    fn wallpapers(&self, refresh: bool) -> ApiResult<Vec<crate::api::ScreenWallpaper>> {
        if refresh {
            crate::wallpaper::refresh(self);
        }
        Ok(crate::wallpaper::list(self))
    }
    fn identify_screens(&self) -> ApiResult<()> {
        crate::display_overlay::identify(self);
        Ok(())
    }
    fn aim_status(&self) -> ApiResult<AimStatus> {
        Ok(crate::aim::status(self))
    }
    fn aim_start(&self) -> ApiResult<AimStatus> {
        crate::aim::start(self)
    }
    fn aim_stop(&self) -> ApiResult<()> {
        crate::aim::stop(self);
        Ok(())
    }

    fn input_settings(&self) -> ApiResult<InputSettings> {
        Ok(input_settings(self))
    }

    fn set_input_settings(&self, patch: PreferencePatch<InputSettings>) -> ApiResult<()> {
        let _gate = self.owner_gate();
        let s = patch.apply(&input_settings(self))?;
        let edits_contacts = patch.value().get("accept_contacts").is_some();
        if edits_contacts && s.accept_contacts && !self.contacts_injection_available() {
            return Err(invalid("Native contact injection is unavailable"));
        }
        if !(0.1..=10.0).contains(&s.scroll.speed) {
            return Err(invalid("Scroll speed must be within 0.1–10"));
        }
        if !(0.1..=5.0).contains(&s.device_gain) {
            return Err(invalid("Device gain must be within 0.1–5"));
        }
        if !oink_display::layout::valid_cursor_trail_duration_ms(s.cursor_trail_duration_ms) {
            return Err(invalid("Cursor trail duration must be within 100–60000 ms"));
        }
        let emergency_hold = crate::config::EmergencyHoldMs::try_from(s.emergency_hold_ms).map_err(invalid)?;
        crate::hotkeys::validate(&s.hotkeys).map_err(invalid)?;
        // InputSettings is flat; explicit local overrides use the durable routing path.
        // Preserve an explicit choice equal to the default without pinning siblings.
        let mut local_patch = patch.value();
        let mut routing_patch = serde_json::Map::new();
        for name in ["mac_scroll_feel", "forward_injected_input"] {
            if let Some(value) = local_patch.as_object_mut().and_then(|p| p.remove(name)) {
                routing_patch.insert(name.into(), value);
            }
        }
        if !routing_patch.is_empty() {
            local_patch["routing"] = routing_patch.into();
        }
        self.edit_local_input_settings(&local_patch, |candidate| {
            let routing = &mut candidate.routing;
            routing.mac_scroll_feel = s.mac_scroll_feel;
            routing.forward_injected_input = s.forward_injected_input;
            routing.remote_scroll_speed = s.scroll.speed;
            routing.fixed_reference_display = match s.pointer.reference {
                PointerReference::Fixed { display } => Some(display),
                PointerReference::LastScreen => None,
            };
            routing.button_handoff = if s.pointer.drag_across_devices {
                ButtonHandoff::Allow
            } else {
                ButtonHandoff::Block
            };
            routing.forward_gestures = s.gestures_enabled;
            routing.return_pointer_on_display_sleep = s.return_pointer_on_display_sleep;
            routing.wake_displays_on_input = s.wake_displays_on_input;
            routing.cursor_trail = s.cursor_trail;
            candidate.cursor_trail_duration_ms = s.cursor_trail_duration_ms;
            candidate.event_frequency = s.event_frequency;
            candidate.emergency_hold_ms = emergency_hold;
            if edits_contacts {
                candidate.accept_contacts = s.accept_contacts;
            }
            candidate.screen_preferences.device_gain = s.device_gain;
            candidate.speed_mode = match s.pointer.scaling {
                PointerScaling::Physical => SpeedMode::Physical,
                PointerScaling::Proportional => SpeedMode::ScreenProportional,
            };
            candidate.hotkeys = Some(s.hotkeys);
        })?;
        drop(_gate);
        if Node::layout(self).0.settings.gap_travel != s.gap_travel {
            self.link_config()?
                .edit_settings(Box::new(move |settings| settings.gap_travel = s.gap_travel))?;
        }
        Ok(())
    }

    fn cache_usage(&self, device: Option<DeviceKey>) -> ApiResult<Vec<CacheDevice>> {
        let node = self.clone();
        block(self, async move {
            crate::cache::query(node, device, false).await.map(|r| r.0)
        })
    }
    fn clear_cache(&self, device: Option<DeviceKey>) -> ApiResult<CacheDeviceClear> {
        let node = self.clone();
        block(self, async move {
            let (usage, result) = crate::cache::query(node, device, true).await?;
            Ok(CacheDeviceClear {
                device: usage[0].device,
                result: result.expect("clear result"),
            })
        })
    }
    fn received_files_usage(&self) -> ApiResult<ReceivedFilesUsage> {
        let observed = self.native_services_enabled().then(oink_clipboard::received::usage);
        let mut usage = observed.and_then(Result::ok).unwrap_or_else(|| ReceivedFilesUsage {
            incomplete: self.native_services_enabled(),
            time_basis: if cfg!(windows) {
                oink_clipboard::received::Basis::LastUsed
            } else {
                oink_clipboard::received::Basis::Received
            },
            ..Default::default()
        });
        usage.cleanup = self.settings().received_files_cleanup;
        Ok(usage)
    }
    fn set_received_files_cleanup(&self, cleanup: ReceivedFilesCleanup) -> ApiResult<()> {
        let _gate = self.owner_gate();
        lock(&self.inner.config)
            .update_local(&serde_json::json!({"received_files_cleanup":cleanup}), |settings| {
                settings.received_files_cleanup = cleanup
            })
            .map_err(|e| unavailable(e.to_string()))?;
        if self.native_services_enabled() {
            oink_clipboard::received::configure(cleanup);
        }
        self.emit(ApiEvent::ReceivedFilesUsage(self.received_files_usage()?));
        Ok(())
    }
    fn open_received_files(&self) -> ApiResult<()> {
        oink_clipboard::received::open().map_err(|e| unavailable(e.to_string()))
    }
    fn clipboard_settings(&self) -> ApiResult<ClipboardSettings> {
        Ok(match self.clipboard() {
            Some(c) => c.settings(),
            None => self.settings().clipboard.unwrap_or_else(crate::clip::default_settings),
        })
    }

    fn set_clipboard_settings(&self, patch: PreferencePatch<ClipboardSettings>) -> ApiResult<()> {
        let _gate = self.owner_gate();
        let s = patch.apply(&self.clipboard_settings()?)?;
        lock(&self.inner.config)
            .update_local(&serde_json::json!({"clipboard":patch.value()}), |settings| {
                settings.clipboard = Some(s)
            })
            .map_err(|error| unavailable(error.to_string()))?;
        if let Some(clipboard) = self.clipboard() {
            clipboard.set_settings(s);
        }
        self.emit(ApiEvent::ClipboardSettings(s));
        Ok(())
    }

    fn clipboard_transfer(&self) -> ApiResult<Option<FileTransferStatus>> {
        Ok(self.clipboard().and_then(|c| c.transfer()))
    }
    fn fetch_clipboard_files(&self) -> ApiResult<()> {
        self.clipboard()
            .ok_or_else(|| ApiError::Unavailable {
                message: "剪贴板服务不可用".into(),
            })?
            .fetch_files();
        Ok(())
    }
    fn cancel_clipboard_files(&self) -> ApiResult<()> {
        self.clipboard()
            .ok_or_else(|| ApiError::Unavailable {
                message: "剪贴板服务不可用".into(),
            })?
            .cancel_files();
        Ok(())
    }

    fn clipboard_activity(&self) -> ApiResult<Vec<ClipActivity>> {
        Ok(self.clipboard().map(|c| c.activity()).unwrap_or_default())
    }

    fn file_provider_status(&self) -> ApiResult<FileProviderStatus> {
        #[cfg(target_os = "macos")]
        {
            Ok(lock(&self.inner.file_provider).snapshot())
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(FileProviderStatus::default())
        }
    }

    fn open_file_provider_enable(&self) -> ApiResult<()> {
        #[cfg(target_os = "macos")]
        {
            lock(&self.inner.file_provider).request_open();
            self.inner.file_provider_wake.notify_one();
            crate::file_provider::report(self);
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(ApiError::Unsupported {
                message: "当前系统不使用 macOS File Provider".into(),
            })
        }
    }

    fn file_provider_placeholder_pool(&self) -> ApiResult<FileProviderPlaceholderStatus> {
        let enabled = self.settings().file_provider_placeholder_pool;
        #[cfg(target_os = "macos")]
        let stock = Some(oink_clipboard::file_provider::placeholder_pool_status().into());
        #[cfg(not(target_os = "macos"))]
        let stock = None;
        Ok(FileProviderPlaceholderStatus {
            enabled,
            supported: cfg!(target_os = "macos"),
            stock,
        })
    }

    fn set_file_provider_placeholder_pool(&self, enabled: bool) -> ApiResult<()> {
        #[cfg(target_os = "macos")]
        {
            crate::file_provider::set_placeholder_pool(self, enabled)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = enabled;
            Err(ApiError::Unsupported {
                message: "当前系统不使用 macOS File Provider".into(),
            })
        }
    }

    fn update_status(&self) -> ApiResult<UpdateStatus> {
        Ok(self.query_update_status())
    }
    fn update_command(&self, command: UpdateCommand) -> ApiResult<UpdateStatus> {
        self.execute_update_command(command)
    }
    fn audio_quality(&self) -> ApiResult<AudioQuality> {
        Ok(lock(&self.inner.audio_quality)
            .view((oink_audio::diagnostics::monotonic_us(std::time::Instant::now()).max(0) / 1000) as u64))
    }
    fn connection_quality(&self) -> ApiResult<ConnectionQuality> {
        Ok(
            crate::node::lock(&self.inner.quality)
                .view(crate::connection_quality::now_ms_at(std::time::Instant::now())),
        )
    }
    fn status(&self) -> ApiResult<StatusView> {
        status_view(self)
    }

    fn logs(&self, after: Option<u64>, limit: u32) -> ApiResult<Vec<LogLine>> {
        Ok(crate::logs::recent(after, limit.clamp(1, 2000) as usize))
    }

    fn export_diagnostics_range(&self, last_minutes: Option<u32>) -> ApiResult<DiagnosticsBundle> {
        if last_minutes == Some(0) {
            return Err(ApiError::Internal {
                message: "minutes must be positive".into(),
            });
        }
        let d = self.inner.diagnostics.get().ok_or_else(|| ApiError::Unavailable {
            message: "diagnostics unavailable".into(),
        })?;
        let b = d
            .save_group_range(last_minutes)
            .map_err(|e| ApiError::Internal { message: e.to_string() })?;
        Ok(DiagnosticsBundle {
            path: b.path,
            bytes: b.bytes,
            cutoff_complete: b.cutoff_complete,
            included_device_count: b.included_device_count,
            missing_device_count: b.missing_device_count,
        })
    }
    fn diagnostics_folder(&self) -> ApiResult<String> {
        let d = self.inner.diagnostics.get().ok_or_else(|| ApiError::Unavailable {
            message: "diagnostics unavailable".into(),
        })?;
        d.recorder
            .folder()
            .map_err(|e| ApiError::Internal { message: e.to_string() })
    }
    fn diagnostics_status(&self) -> ApiResult<DiagnosticRecorderStatus> {
        let d = self.inner.diagnostics.get().ok_or_else(|| ApiError::Unavailable {
            message: "diagnostics unavailable".into(),
        })?;
        Ok(d.recorder.status())
    }

    fn set_diagnostics_enabled(&self, enabled: bool) -> ApiResult<()> {
        let d = self.inner.diagnostics.get().ok_or_else(|| ApiError::Unavailable {
            message: "diagnostics unavailable".into(),
        })?;
        let _owner = self.owner_gate();
        {
            let mut config = lock(&self.inner.config);
            config
                .update(|settings| settings.diagnostics_enabled = enabled)
                .map_err(|e| ApiError::Internal { message: e.to_string() })?;
        }
        let applied = d.recorder.set_enabled(enabled);
        // A filesystem failure can leave the durable preference enabled while
        // the recorder is safely off. Native observations follow actual state.
        oink_input::native_lifecycle::set_lifecycle(d.recorder.enabled());
        applied.map_err(|e| ApiError::Internal { message: e.to_string() })?;
        self.drain_input_trace();
        if enabled && let Ok(input) = self.input_thread() {
            input.send(crate::threads::input::InputCommand::DiagnosticsSnapshot);
        }
        d.recorder.refresh_environment();
        Ok(())
    }

    fn permissions(&self) -> ApiResult<Vec<PermissionItem>> {
        self.refresh_permissions();
        let items = permission_items(self);
        self.emit(ApiEvent::Permissions(items.clone()));
        Ok(items)
    }

    fn request_permission(&self, kind: PermissionKind) -> ApiResult<()> {
        let (os, major) = local_permission_platform();
        if permission_settings_target(kind, os, major).is_none() {
            return Err(ApiError::Unsupported {
                message: "This permission does not apply on this system".into(),
            });
        }
        #[cfg(target_os = "macos")]
        {
            // Run inside oink-backend, the TCC-responsible host, only from Grant.
            // Opening a privacy pane alone never registers a missing TCC entry.
            if !self.inner.native_services {
                return Err(ApiError::Unsupported {
                    message: "Native permission requests are unavailable".into(),
                });
            }
            let before = if kind == PermissionKind::AudioCapture {
                match oink_audio::capture_permission_preflight() {
                    oink_audio::CapturePreflight::Authorized => PermissionState::Granted,
                    oink_audio::CapturePreflight::Denied => PermissionState::Denied,
                    _ => PermissionState::NotDetermined,
                }
            } else {
                permission_items(self)
                    .into_iter()
                    .find(|p| p.kind == kind)
                    .map_or(PermissionState::NotDetermined, |p| p.state)
            };
            permission_request_action(
                kind,
                before,
                || {
                    use PermissionRequestOutcome::*;
                    let granted = match kind {
                        PermissionKind::Accessibility => mac_permissions::request_accessibility(),
                        PermissionKind::InputMonitoring => mac_permissions::request_input_monitoring(),
                        PermissionKind::AudioCapture => {
                            if let Err(error) = oink_audio::request_capture_permission() {
                                tracing::warn!(%error, "system audio authorization request unavailable");
                                return Unavailable;
                            }
                            matches!(
                                oink_audio::capture_permission_preflight(),
                                oink_audio::CapturePreflight::Authorized
                            )
                        }
                        PermissionKind::LocalNetwork => {
                            mac_permissions::request_local_network();
                            mac_permissions::local_network_state() == PermissionState::Granted
                        }
                        _ => return Unavailable,
                    };
                    self.refresh_permissions();
                    self.emit(ApiEvent::Permissions(permission_items(self)));
                    if granted { Granted } else { MayPrompt }
                },
                || self.open_permission_settings(kind),
            )
        }
        #[cfg(not(target_os = "macos"))]
        self.open_permission_settings(kind)
    }

    fn open_permission_settings(&self, kind: PermissionKind) -> ApiResult<()> {
        let (os, major) = local_permission_platform();
        let Some(url) = permission_settings_target(kind, os, major) else {
            return Err(ApiError::Unsupported {
                message: "本系统没有对应的设置页面".into(),
            });
        };
        #[cfg(windows)]
        let r = {
            use std::os::windows::process::CommandExt;
            // `start` delegates to the user's registered settings handler; the
            // command interpreter itself must never create a visible console.
            std::process::Command::new("cmd")
                .args(["/C", "start", "", url])
                .creation_flags(windows::Win32::System::Threading::CREATE_NO_WINDOW.0)
                .spawn()
        };
        #[cfg(not(windows))]
        let r = std::process::Command::new("open").arg(url).spawn();
        r.map(|_| ()).map_err(|e| ApiError::Internal { message: e.to_string() })
    }

    fn complete_onboarding(&self) -> ApiResult<()> {
        self.update_settings(|s| s.onboarding_done = true)
            .map_err(|error| unavailable(error.to_string()))?;
        self.emit(ApiEvent::Overview(overview(self)?));
        Ok(())
    }

    // Login registration is queried from the OS instead of persisted settings.
    // 平台模块登记相应应用入口；界面只显示后台返回/推送的实际系统状态。
    fn launch_at_login_status(&self) -> ApiResult<LaunchAtLoginStatus> {
        let mut status = oink_autostart::status();
        status.configured = self.settings().launch_at_login_configured;
        Ok(status)
    }

    fn set_launch_at_login(&self, enabled: bool) -> ApiResult<LaunchAtLoginStatus> {
        let _gate = self.owner_gate();
        match oink_autostart::set_enabled(enabled) {
            Ok(mut status) => {
                // Leaving onboarding confirms the displayed choice, including an
                // untouched default. Store only that confirmation, not OS state.
                self.update_settings(|s| s.launch_at_login_configured = true)
                    .map_err(|error| unavailable(error.to_string()))?;
                status.configured = true;
                tracing::info!(enabled, state = ?status.state, "launch at login changed by user");
                self.emit(ApiEvent::LaunchAtLogin(status.clone()));
                Ok(status)
            }
            Err(message) => {
                tracing::warn!(enabled, %message, "launch at login change failed");
                Err(ApiError::InvalidState { message })
            }
        }
    }

    fn open_launch_at_login_settings(&self) -> ApiResult<()> {
        oink_autostart::open_system_settings().map_err(|message| ApiError::InvalidState { message })
    }

    fn reopen_oink(&self) -> ApiResult<()> {
        crate::backend::reopen()
    }
    fn show_window(&self) -> ApiResult<()> {
        self.emit(ApiEvent::ShowWindow);
        Ok(())
    }

    fn subscribe(&self, sink: EventSink) -> SubscriptionId {
        Node::subscribe(self, sink)
    }

    fn unsubscribe(&self, id: SubscriptionId) {
        Node::unsubscribe(self, id)
    }
}

pub(crate) fn peer_statuses(node: &Node) -> Vec<PeerStatus> {
    let me = node.id();
    let quality = lock(&node.inner.quality).view(crate::connection_quality::now_ms_at(std::time::Instant::now()));
    let input = node.input_status().ok();
    let contacts_enabled = true;
    node.group_members()
        .into_iter()
        .filter(|id| *id != me)
        .map(|id| {
            let device = device_summary(node, id);
            let capacity = input
                .as_ref()
                .and_then(|s| s.peers.iter().find(|p| p.peer == id && p.attached))
                .and_then(|p| p.touchpad_contacts);
            PeerStatus {
                device: id.into(),
                name: device.name,
                os: device.os,
                state: peer_state(node, id),
                reachable: node.links().is_ok_and(|l| l.links().ready(id)),
                facts_fresh: true,
                touchpad_contacts: capacity,
                contacts_supported: capacity == Some(5),
                contacts_path: PeerContactsPath::derive(contacts_enabled, capacity),
                sharing: sharing_state(node, id),
                link: link_info(node, id),
                rtt_history_ms: quality
                    .peers
                    .iter()
                    .find(|p| p.device.0 == id)
                    .map(|p| p.samples.iter().filter_map(|s| s.rtt_ms).collect())
                    .unwrap_or_default(),
            }
        })
        .collect()
}

pub(crate) fn status_view(node: &Node) -> ApiResult<StatusView> {
    let peers = peer_statuses(node);
    let o = overview(node)?;
    let (settings_issues, accept_contacts) = {
        let config = lock(&node.inner.config);
        (config.settings_issues.clone(), config.settings.accept_contacts)
    };
    Ok(StatusView {
        network: oink_network::udp_send_status(),
        settings_issues,
        backend_lifetime: crate::backend::launch::lifetime().status.into(),
        backend_uiaccess: crate::backend::launch::lifetime().uiaccess,
        contact_injection: node.input_backend().contact_injection_status(),
        accept_contacts: accept_contacts && node.contacts_injection_available(),
        service: o.service,
        version: env!("OINK_RELEASE_VERSION").to_string(),
        uptime_s: node.inner.started.elapsed().as_secs(),
        focus: o.focus,
        peers,
        alerts: o.alerts,
    })
}

/// macOS 键鼠权限的请求（没有现成绑定的系统函数）。
#[cfg(target_os = "macos")]
mod mac_permissions {
    use std::ffi::c_void;

    use objc2_core_foundation::{CFBoolean, CFDictionary, CFString};

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        static kAXTrustedCheckOptionPrompt: &'static CFString;
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOHIDRequestAccess(request: u32) -> bool;
    }
    unsafe extern "C" {
        fn oink_request_local_network();
    }
    pub(super) fn request_input_monitoring() -> bool {
        // SAFETY: kIOHIDRequestTypeListenEvent from IOHIDLib.h.
        unsafe { IOHIDRequestAccess(1) }
    }
    pub(super) fn request_local_network() {
        // SAFETY: bridge owns a serialized browser for the backend lifetime.
        unsafe { oink_request_local_network() }
    }
    pub(super) fn local_network_state() -> oink_foundation::input::PermissionState {
        use oink_foundation::input::PermissionState;
        match oink_network::local_network_access() {
            oink_network::LocalNetworkAccess::Granted => PermissionState::Granted,
            oink_network::LocalNetworkAccess::Denied => PermissionState::Denied,
            oink_network::LocalNetworkAccess::NotDetermined => PermissionState::NotDetermined,
        }
    }

    // Both the Grant browser and ordinary DNS-SD use the same live observation.
    #[unsafe(no_mangle)]
    extern "C" fn oink_observe_local_network(state: i32) {
        use oink_network::LocalNetworkAccess;
        oink_network::observe_local_network_access(match state {
            1 => LocalNetworkAccess::Granted,
            2 => LocalNetworkAccess::Denied,
            _ => return,
        });
    }

    /// 请求「辅助功能」：未授权时系统弹出提示并把 Oink 加入列表（开关由用户打开）。返回当前是否已授权。
    pub(super) fn request_accessibility() -> bool {
        // SAFETY: 读取 ApplicationServices 导出的常量。
        let key: &CFString = unsafe { kAXTrustedCheckOptionPrompt };
        let options = CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[CFBoolean::new(true)]);
        let ptr: *const CFDictionary<CFString, CFBoolean> = &*options;
        // SAFETY: options 是有效的 CFDictionary，调用期间保持存活。
        unsafe { AXIsProcessTrustedWithOptions(ptr.cast()) != 0 }
    }
}

#[cfg(all(test, debug_assertions))]
mod link_tests;

#[cfg(all(test, debug_assertions))]
mod permission_tests {
    use super::*;
    use PermissionKind::*;

    #[test]
    fn requests_do_not_open_settings_alongside_possible_dialogs() {
        use PermissionRequestOutcome::*;
        use std::cell::RefCell;
        for kind in [Accessibility, InputMonitoring, LocalNetwork, AudioCapture] {
            for before in [
                PermissionState::NotDetermined,
                PermissionState::Denied,
                PermissionState::Granted,
            ] {
                for outcome in [Granted, MayPrompt, Unavailable] {
                    let calls = RefCell::new(vec![]);
                    permission_request_action(
                        kind,
                        before,
                        || {
                            calls.borrow_mut().push("request");
                            outcome
                        },
                        || {
                            calls.borrow_mut().push("settings");
                            Ok(())
                        },
                    )
                    .unwrap();
                    let fallback = matches!(outcome, Unavailable)
                        || (matches!(outcome, MayPrompt)
                            && before == PermissionState::Denied
                            && matches!(kind, LocalNetwork | AudioCapture));
                    assert_eq!(
                        *calls.borrow(),
                        if fallback {
                            vec!["request", "settings"]
                        } else {
                            vec!["request"]
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn platform_permission_sets_match_existing_os_permissions() {
        assert_eq!(platform_permission_kinds(Os::Windows, 0), &[Firewall, SystemService]);
        assert_eq!(
            platform_permission_kinds(Os::Macos, 14),
            &[Accessibility, InputMonitoring]
        );
        for major in [15, 26] {
            assert_eq!(
                platform_permission_kinds(Os::Macos, major),
                &[Accessibility, InputMonitoring, LocalNetwork]
            );
        }
        for os in [Os::Linux, Os::Other] {
            assert!(platform_permission_kinds(os, 0).is_empty());
        }
    }

    #[test]
    fn settings_targets_exist_on_the_reported_platform_and_version() {
        for kind in [Accessibility, InputMonitoring, LocalNetwork, AudioCapture] {
            assert_eq!(permission_settings_target(kind, Os::Windows, 0), None);
        }
        for kind in [Firewall, SystemService] {
            assert_eq!(permission_settings_target(kind, Os::Macos, 26), None);
            assert_eq!(
                permission_settings_target(kind, Os::Windows, 0),
                kind.windows_settings_uri()
            );
        }
        assert_eq!(permission_settings_target(LocalNetwork, Os::Macos, 14), None);
        assert_eq!(
            permission_settings_target(LocalNetwork, Os::Macos, 15),
            LocalNetwork.macos_settings_url()
        );
        assert_eq!(
            permission_settings_target(AudioCapture, Os::Macos, 14),
            AudioCapture.macos_settings_url()
        );
        assert_eq!(permission_settings_target(Firewall, Os::Linux, 0), None);
    }

    #[test]
    fn permissions_api_preserves_the_platform_set_and_rejects_nonexistent_grants() {
        crate::test_watchdog::run(module_path!(), "permissions_api_platform_set", Some(2), async {
            let dir = tempfile::tempdir().unwrap();
            let node = crate::snapshot_tests::start_with_clip(
                &oink_test_support::network_config(dir.path().into(), "permissions", 0),
                &crate::snapshot_tests::Displays::new("permissions"),
                None,
            )
            .await;
            let (os, major) = local_permission_platform();
            let Reply::Permissions(items) = dispatch(&node, Request::Permissions).unwrap() else {
                panic!("expected typed permissions reply");
            };
            assert_eq!(
                items.iter().map(|item| item.kind).collect::<Vec<_>>(),
                platform_permission_kinds(os, major)
            );
            let json = serde_json::to_value(&items).unwrap();
            println!("permission platform={os:?}, macos_major={major}, items={json}");
            let decoded: Vec<PermissionItem> = serde_json::from_value(json).unwrap();
            assert_eq!(items, decoded);
            for kind in [
                Accessibility,
                InputMonitoring,
                LocalNetwork,
                AudioCapture,
                Firewall,
                SystemService,
            ] {
                if permission_settings_target(kind, os, major).is_none() {
                    for request in [
                        Request::RequestPermission { kind },
                        Request::OpenPermissionSettings { kind },
                    ] {
                        assert!(matches!(dispatch(&node, request), Err(ApiError::Unsupported { .. })));
                    }
                }
            }
            node.shutdown().await;
        });
    }
}
