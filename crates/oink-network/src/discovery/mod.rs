//! IPv4 discovery through Oink UDP beacons and DNS-SD. Discovery results are
//! untrusted hints; manual pairing establishes TLS at the supplied service endpoint.

use oink_foundation::{DeviceId, GroupId};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch};

mod builtin;
#[cfg(target_os = "macos")]
mod dnssd;

mod advert;
pub use advert::{Advert, ProtocolOffer};
pub(crate) const SERVICE_TYPE: &str = oink_protocol::v1::DISCOVERY_SERVICE;
use oink_protocol::v1::{
    self, Message, RawValue, dto,
    schema::{WireDto, WireValue},
};

/// 发现来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Beacon,
    Mdns,
    Manual,
}

/// 发现到的一台设备。
#[derive(Clone, Debug)]
pub struct Sighting {
    pub advert: Advert,
    /// 可连接的 QUIC 地址。
    pub addrs: Vec<SocketAddr>,
    pub source: Source,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Packet {
    Beacon(Advert),
    Query([u8; 16]),
    Reply(Advert, Option<[u8; 16]>),
}
fn bounded_advert(a: &Advert) -> v1::Result<dto::Advert> {
    let mut a = a.clone();
    let mut end = a.name.len().min(200);
    while !a.name.is_char_boundary(end) {
        end -= 1;
    }
    a.name.truncate(end);
    a.to_wire()
}
pub(crate) fn encode_beacon(a: &Advert) -> v1::Result<Vec<u8>> {
    v1::frame::encode_discovery(&Message::DiscoveryBeacon(bounded_advert(a)?))
}
pub(crate) fn encode_query(nonce: &[u8; 16]) -> Vec<u8> {
    let query = dto::DiscoveryQuery::from_raw([(1, RawValue::Bytes(nonce.to_vec()))].into())
        .expect("fixed-size discovery nonce");
    v1::frame::encode_discovery(&Message::DiscoveryQuery(query)).expect("bounded discovery query")
}
pub(crate) fn encode_reply(a: &Advert, nonce: &[u8; 16]) -> v1::Result<Vec<u8>> {
    let reply =
        dto::DiscoveryReply::from_raw([(1, bounded_advert(a)?.write()), (2, RawValue::Bytes(nonce.to_vec()))].into())?;
    v1::frame::encode_discovery(&Message::DiscoveryReply(reply))
}
pub(crate) fn decode(bytes: &[u8]) -> Option<Packet> {
    match v1::frame::discovery(bytes).ok()? {
        Message::DiscoveryBeacon(a) => Some(Packet::Beacon(Advert::from_wire(&a).ok()?)),
        Message::DiscoveryQuery(query) => Some(Packet::Query(query.nonce().ok()?.0)),
        Message::DiscoveryReply(reply) => Some(Packet::Reply(
            Advert::from_wire(&reply.advert().ok()?).ok()?,
            Some(reply.nonce().ok()?.0),
        )),
        _ => None,
    }
}

/// 本机所有 IPv4 地址（不含回环），以及每个地址所在网段的广播地址。
pub fn local_ipv4() -> Vec<(Ipv4Addr, Option<Ipv4Addr>)> {
    let Ok(list) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for i in list {
        if i.is_loopback() {
            continue;
        }
        if let if_addrs::IfAddr::V4(v4) = i.addr {
            // 169.254/16 也保留：两台电脑用网线直连时就是这种地址。
            let bcast = v4.broadcast.or_else(|| {
                let ip = u32::from(v4.ip);
                let mask = u32::from(v4.netmask);
                (mask != 0).then(|| Ipv4Addr::from(ip | !mask))
            });
            out.push((v4.ip, bcast));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 发现服务的配置。
#[derive(Clone, Debug)]
pub struct DiscoveryConfig {
    pub bind_ip: IpAddr,
    pub port: u16,
    pub beacon: bool,
    pub beacon_interval: Duration,
    /// 广播到这些端口（默认只有正式发现端口与本机发现端口）。
    pub beacon_ports: Vec<u16>,
    pub mdns: MdnsMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MdnsMode {
    Off,
    /// macOS：系统 DNS-SD；其他平台：内置实现。
    System,
    /// 内置的纯 Rust 实现（`mdns-sd`）。
    Builtin,
}

type Pending = Arc<Mutex<HashMap<[u8; 16], oneshot::Sender<(Advert, SocketAddr)>>>>;

/// 运行中的发现服务。
pub struct Discovery {
    socket: Mutex<Option<Arc<UdpSocket>>>,
    pending: Pending,
    extra_targets: Arc<Mutex<Vec<SocketAddr>>>,
    refresh: mpsc::UnboundedSender<()>,
    mdns: Mutex<Option<MdnsHandle>>,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for Discovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Discovery")
    }
}

enum MdnsHandle {
    #[cfg(target_os = "macos")]
    System(dnssd::DnsSd),
    Builtin(builtin::Builtin),
}

impl MdnsHandle {
    fn update(&self, a: &Advert) {
        match self {
            #[cfg(target_os = "macos")]
            MdnsHandle::System(m) => m.update(a.clone()),
            MdnsHandle::Builtin(m) => m.update(a),
        }
    }

    fn refresh(&self) {
        match self {
            #[cfg(target_os = "macos")]
            MdnsHandle::System(m) => m.refresh(),
            MdnsHandle::Builtin(m) => m.refresh(),
        }
    }

    fn stop(self) {
        match self {
            #[cfg(target_os = "macos")]
            MdnsHandle::System(m) => m.stop(),
            MdnsHandle::Builtin(m) => m.stop(),
        }
    }
}

impl Discovery {
    pub async fn start(
        cfg: DiscoveryConfig,
        advert: watch::Receiver<Advert>,
        out: mpsc::Sender<Sighting>,
    ) -> std::io::Result<Self> {
        if !cfg.bind_ip.is_ipv4() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "IPv4 address required",
            ));
        }
        let std_sock = std::net::UdpSocket::bind(SocketAddr::new(cfg.bind_ip, cfg.port))?;
        std_sock.set_broadcast(true)?;
        std_sock.set_nonblocking(true)?;
        let socket = Arc::new(UdpSocket::from_std(std_sock)?);
        let pending: Pending = Arc::default();
        let extra_targets: Arc<Mutex<Vec<SocketAddr>>> = Arc::default();
        let (refresh_tx, refresh_rx) = mpsc::unbounded_channel();

        let mdns = match cfg.mdns {
            MdnsMode::Off => None,
            #[cfg(target_os = "macos")]
            MdnsMode::System => Some(MdnsHandle::System(dnssd::DnsSd::start(
                advert.borrow().clone(),
                out.clone(),
            ))),
            #[cfg(not(target_os = "macos"))]
            MdnsMode::System => builtin::Builtin::start(&advert.borrow(), out.clone())
                .map_err(|e| tracing::warn!("mDNS 启动失败：{e}"))
                .ok()
                .map(MdnsHandle::Builtin),
            MdnsMode::Builtin => builtin::Builtin::start(&advert.borrow(), out.clone())
                .map_err(|e| tracing::warn!("mDNS 启动失败：{e}"))
                .ok()
                .map(MdnsHandle::Builtin),
        };

        let task = tokio::spawn(run(
            cfg,
            socket.clone(),
            advert,
            out,
            pending.clone(),
            extra_targets.clone(),
            refresh_rx,
        ));
        Ok(Self {
            socket: Mutex::new(Some(socket)),
            pending,
            extra_targets,
            refresh: refresh_tx,
            mdns: Mutex::new(mdns),
            task: tokio::sync::Mutex::new(Some(task)),
        })
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.socket
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()?
            .local_addr()
            .ok()
    }

    /// 通告内容变化（组、名字、人数）。
    pub fn advert_changed(&self, a: &Advert) {
        if let Some(m) = self.mdns.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            m.update(a);
        }
        let _ = self.refresh.send(());
    }

    /// 网络变化：立即重新广播、重新浏览。
    pub fn refresh(&self) {
        if let Some(m) = self.mdns.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            m.refresh();
        }
        let _ = self.refresh.send(());
    }

    /// 额外的信标目标（单播；测试和同机多实例用）。
    pub fn add_target(&self, addr: SocketAddr) {
        if !addr.is_ipv4() {
            return;
        }
        let mut t = self.extra_targets.lock().unwrap_or_else(|e| e.into_inner());
        if !t.contains(&addr) {
            t.push(addr);
        }
        let _ = self.refresh.send(());
    }

    /// 向 `target`（发现端口）发送查询，等待回复。
    pub async fn query(&self, target: SocketAddr, timeout: Duration) -> Option<(Advert, SocketAddr)> {
        if !target.is_ipv4() {
            return None;
        }
        let nonce: [u8; 16] = rand::random();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(nonce, tx);
        let pkt = encode_query(&nonce);
        let fut = async {
            // UDP 可能丢包：每 300 ms 重发一次。
            let mut rx = rx;
            loop {
                // Borrow the socket only for each poll. A concurrent stop can
                // revoke it without waiting for this caller-owned query future.
                let send = std::future::poll_fn(|cx| {
                    let socket = self.socket.lock().unwrap_or_else(|e| e.into_inner());
                    match socket.as_ref() {
                        Some(socket) => socket.poll_send_to(cx, &pkt, target).map(Some),
                        None => std::task::Poll::Ready(None),
                    }
                });
                tokio::select! {
                    r = &mut rx => return r.ok(),
                    sent = send => { let _ = sent?; }
                }
                tokio::select! {
                    r = &mut rx => return r.ok(),
                    _ = tokio::time::sleep(Duration::from_millis(300)) => {}
                }
            }
        };
        let r = tokio::time::timeout(timeout, fut).await.ok().flatten();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&nonce);
        r
    }

    pub async fn stop(&self) {
        if let Some(task) = self.task.lock().await.take() {
            task.abort();
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                tracing::error!(%error, "discovery task failed");
            }
        }
        self.socket.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let mdns = self.mdns.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mdns) = mdns
            && let Err(error) = tokio::task::spawn_blocking(move || mdns.stop()).await
        {
            if error.is_cancelled() {
                // Only runtime teardown cancels this private cleanup worker.
                // Do not turn a process exit into panic noise or report a completed stop.
                std::future::pending::<()>().await;
            }
            panic!("mDNS shutdown task panicked: {error}");
        }
    }
}

async fn run(
    cfg: DiscoveryConfig,
    socket: Arc<UdpSocket>,
    advert: watch::Receiver<Advert>,
    out: mpsc::Sender<Sighting>,
    pending: Pending,
    extra: Arc<Mutex<Vec<SocketAddr>>>,
    mut refresh: mpsc::UnboundedReceiver<()>,
) {
    let mut tick = tokio::time::interval(cfg.beacon_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut buf = vec![0u8; 2048];
    loop {
        tokio::select! {
            _ = tick.tick() => send_beacons(&cfg, &socket, &advert, &extra).await,
            Some(()) = refresh.recv() => send_beacons(&cfg, &socket, &advert, &extra).await,
            r = socket.recv_from(&mut buf) => {
                let Ok((n, from)) = r else {
                    // Windows 上对端端口不可达会让 recv_from 报错（WSAECONNRESET），忽略即可。
                    continue;
                };
                handle_packet(&buf[..n], from, &socket, &advert, &out, &pending).await;
            }
        }
    }
}

async fn send_beacons(
    cfg: &DiscoveryConfig,
    socket: &UdpSocket,
    advert: &watch::Receiver<Advert>,
    extra: &Mutex<Vec<SocketAddr>>,
) {
    let Ok(pkt) = encode_beacon(&advert.borrow()) else {
        return;
    };
    let mut targets: Vec<SocketAddr> = extra.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if cfg.beacon {
        let bcasts: Vec<Ipv4Addr> = if cfg.bind_ip.is_unspecified() {
            local_ipv4().into_iter().filter_map(|(_, b)| b).collect()
        } else if let IpAddr::V4(ip) = cfg.bind_ip {
            local_ipv4()
                .into_iter()
                .filter(|(a, _)| *a == ip)
                .filter_map(|(_, b)| b)
                .collect()
        } else {
            Vec::new()
        };
        for b in bcasts {
            for p in &cfg.beacon_ports {
                targets.push(SocketAddr::new(IpAddr::V4(b), *p));
            }
        }
    }
    targets.sort();
    targets.dedup();
    for t in targets {
        if let Err(e) = socket.send_to(&pkt, t).await {
            tracing::trace!("信标发送到 {t} 失败：{e}");
        }
    }
}

async fn handle_packet(
    b: &[u8],
    from: SocketAddr,
    socket: &UdpSocket,
    advert: &watch::Receiver<Advert>,
    out: &mpsc::Sender<Sighting>,
    pending: &Pending,
) {
    if !from.is_ipv4() {
        return;
    }
    let Some(p) = decode(b) else {
        return;
    };
    let me = advert.borrow().id;
    match p {
        Packet::Query(nonce) => {
            let Ok(pkt) = encode_reply(&advert.borrow(), &nonce) else {
                return;
            };
            let _ = socket.send_to(&pkt, from).await;
        }
        Packet::Beacon(a) => {
            if a.id == me {
                return;
            }
            let addr = SocketAddr::new(from.ip(), a.port);
            let _ = out.try_send(Sighting {
                advert: a,
                addrs: vec![addr],
                source: Source::Beacon,
            });
        }
        Packet::Reply(a, nonce) => {
            let addr = SocketAddr::new(from.ip(), a.port);
            if let Some(n) = nonce
                && let Some(tx) = pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&n)
            {
                let _ = tx.send((a, addr));
            }
        }
    }
}

/// TXT 记录的键值（两种 mDNS 实现共用）。
pub(crate) fn txt_pairs(a: &Advert) -> Vec<(String, String)> {
    let bounded = bounded_advert(a).ok().and_then(|a| Advert::from_wire(&a).ok());
    let Some(a) = bounded else {
        return Vec::new();
    };
    vec![
        ("wire".into(), "1".into()),
        ("id".into(), a.id.to_hex()),
        ("idscheme".into(), dto::ED25519.into()),
        ("gid".into(), a.group.to_hex()),
        ("n".into(), a.count.to_string()),
        ("p".into(), a.port.to_string()),
        ("name".into(), a.name),
        (
            "majors".into(),
            a.protocols
                .iter()
                .map(|p| p.major.to_string())
                .collect::<Vec<_>>()
                .join(","),
        ),
        ("os".into(), a.os),
    ]
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// 从 TXT 键值还原通告内容。
pub(crate) fn advert_from_txt(get: impl Fn(&str) -> Option<String>) -> Option<Advert> {
    if get("wire")?.as_str() != "1" || get("idscheme").is_some_and(|s| s != dto::ED25519) {
        return None;
    }
    let majors = get("majors")?;
    if majors.len() > 47 {
        return None;
    }
    let protocols = majors
        .split(',')
        .map(|major| {
            Some(ProtocolOffer {
                major: major.parse().ok()?,
                highest_minor: 0,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    if protocols.is_empty() || protocols.len() > 8 {
        return None;
    }
    let a = Advert {
        id: DeviceId(unhex::<32>(&get("id")?)?),
        group: GroupId(unhex::<16>(&get("gid")?)?),
        count: get("n")?.parse().ok()?,
        port: get("p")?.parse().ok()?,
        name: get("name")?,
        protocols,
        identity_algorithms: vec![dto::ED25519.into()],
        os: get("os")?,
    };
    a.to_wire().ok()?;
    Some(a)
}

/// mDNS 实例名：`设备名 (短指纹)`，不超过 63 字节。
pub(crate) fn instance_name(a: &Advert) -> String {
    let suffix = format!(" ({})", a.id.short());
    let mut name = a.name.clone();
    while name.len() + suffix.len() > 63 {
        name.pop();
    }
    format!("{name}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advert() -> Advert {
        Advert::local(
            DeviceId([5; 32]),
            "synthetic".into(),
            GroupId([6; 16]),
            3,
            47470,
            "macos".into(),
        )
    }

    #[test]
    fn packet_roundtrip() {
        let a = advert();
        assert_eq!(decode(&encode_beacon(&a).unwrap()), Some(Packet::Beacon(a.clone())));
        let n = [9u8; 16];
        assert_eq!(decode(&encode_query(&n)), Some(Packet::Query(n)));
        assert_eq!(
            decode(&encode_reply(&a, &n).unwrap()),
            Some(Packet::Reply(a.clone(), Some(n)))
        );
        assert!(matches!(
            v1::frame::discovery(&encode_beacon(&a).unwrap()),
            Ok(Message::DiscoveryBeacon(_))
        ));
        let mut oversized = encode_beacon(&a).unwrap();
        oversized.resize(1201, 0);
        assert_eq!(decode(&oversized), None);
        assert_eq!(decode(b"ACRS\x01\x01short"), None);
        assert_eq!(decode(b"XXXX\x01\x02aaaaaaaaaaaaaaaa"), None);
    }

    #[test]
    fn txt_roundtrip() {
        let a = advert();
        let pairs = txt_pairs(&a);
        let back = advert_from_txt(|k| pairs.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone()));
        assert_eq!(back, Some(a.clone()));
        for missing in [None, Some(String::new())] {
            assert!(
                advert_from_txt(|key| if key == "gid" {
                    missing.clone()
                } else {
                    pairs.iter().find(|(k, _)| k == key).map(|(_, value)| value.clone())
                })
                .is_none(),
                "every discovered device must name its group"
            );
        }
        assert!(instance_name(&a).ends_with(&format!("({})", a.id.short())));
    }
    #[test]
    fn discovery_rejects_foreign_envelopes_and_invalid_hints() {
        let mut packet = encode_beacon(&advert()).unwrap();
        assert_eq!(&packet[..6], b"OINK\x01\x00");
        packet[4] = 2;
        assert!(decode(&packet).is_none());
        packet[4] = 1;
        packet.extend_from_slice(&[0]);
        assert!(decode(&packet).is_none());
        let mut bad = advert();
        bad.os = "non-ascii-\u{00e9}".into();
        assert!(encode_beacon(&bad).is_err());
        bad.os = "macos".into();
        bad.port = 0;
        assert!(encode_beacon(&bad).is_err());
        bad.port = 47470;
        bad.count = 17;
        assert!(encode_beacon(&bad).is_err());
    }

    #[test]
    fn txt_requires_oink_version_and_preserves_unknown_major_hints() {
        let mut a = advert();
        a.protocols = vec![ProtocolOffer {
            major: 9,
            highest_minor: 0,
        }];
        let pairs = txt_pairs(&a);
        assert!(!pairs.iter().any(|(k, _)| k == "gname" || k == "v"));
        let get = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(advert_from_txt(get), Some(a));
        assert!(advert_from_txt(|key| if key == "wire" { None } else { get(key) }).is_none());
        assert!(
            advert_from_txt(|key| if key == "idscheme" {
                Some("unknown".into())
            } else {
                get(key)
            })
            .is_none()
        );
        assert!(advert_from_txt(|key| if key == "idscheme" { None } else { get(key) }).is_some());
        assert!(advert_from_txt(|key| if key == "name" { Some("x".repeat(201)) } else { get(key) }).is_none());
    }

    #[tokio::test]
    async fn loopback_query_uses_observed_address_and_advertised_service_port() {
        let (advert_tx, advert_rx) = watch::channel(advert());
        let (tx, _rx) = mpsc::channel(4);
        let discovery = Discovery::start(
            DiscoveryConfig {
                bind_ip: Ipv4Addr::LOCALHOST.into(),
                port: 0,
                beacon: false,
                beacon_interval: Duration::from_secs(2),
                beacon_ports: vec![],
                mdns: MdnsMode::Off,
            },
            advert_rx,
            tx,
        )
        .await
        .unwrap();
        let endpoint = discovery.local_addr().unwrap();
        let (found, endpoint) = discovery.query(endpoint, Duration::from_secs(2)).await.unwrap();
        assert_eq!(found, advert());
        assert_eq!(
            endpoint,
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), advert_tx.borrow().port)
        );
        discovery.stop().await;
    }
}
