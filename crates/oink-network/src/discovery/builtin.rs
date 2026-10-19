//! 内置的纯 Rust mDNS（`mdns-sd`）：Windows 等非 macOS 平台使用；macOS 上可用 `OINK_MDNS=builtin`
//! 切换过来做对比。

use super::{Advert, SERVICE_TYPE, Sighting, Source, advert_from_txt, instance_name, txt_pairs};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// 仍在的服务重新交给上层的间隔（附近条目 10 秒过期）。
const REEMIT_EVERY: Duration = Duration::from_secs(3);

pub(crate) struct Builtin {
    daemon: ServiceDaemon,
    fullname: Mutex<Option<String>>,
    ty: String,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn service_info(a: &Advert, ty: &str) -> Option<ServiceInfo> {
    let props: HashMap<String, String> = txt_pairs(a).into_iter().collect();
    let host = format!("oink-{}.local.", a.id.short().to_lowercase());
    ServiceInfo::new(ty, &instance_name(a), &host, "", a.port, props)
        .map(ServiceInfo::enable_addr_auto)
        .map_err(|e| tracing::warn!("mDNS 服务信息无效：{e}"))
        .ok()
}

#[test]
fn mdns_publishes_all_interface_addresses_automatically() {
    let advert = Advert::local(
        oink_foundation::DeviceId([1; 32]),
        "synthetic".into(),
        oink_foundation::GroupId([2; 16]),
        1,
        47480,
        "windows".into(),
    );
    assert!(service_info(&advert, "_oink._udp.local.").unwrap().is_addr_auto());
}

impl Builtin {
    pub fn start(a: &Advert, out: mpsc::Sender<Sighting>) -> Result<Self, mdns_sd::Error> {
        let daemon = ServiceDaemon::new()?;
        daemon.disable_interface(mdns_sd::IfKind::IPv6)?;
        let ty = format!("{SERVICE_TYPE}.local.");
        let me = a.id;
        let mut this = Self {
            daemon,
            fullname: Mutex::new(None),
            ty: ty.clone(),
            thread: None,
        };
        this.update(a);
        let rx = this.daemon.browse(&ty)?;
        this.thread = std::thread::Builder::new()
            .name("oink-mdns".into())
            .spawn(move || {
                // 浏览到、还没被移除的服务 → 最近一次解析结果；定期重新交给上层（附近条目 10 秒过期，
                // 而解析结果只在服务出现或变化时报告一次）。
                let mut live: HashMap<String, Sighting> = HashMap::new();
                let mut last_reemit = Instant::now();
                loop {
                    if last_reemit.elapsed() >= REEMIT_EVERY {
                        last_reemit = Instant::now();
                        for s in live.values() {
                            if out.try_send(s.clone()).is_err() {
                                return;
                            }
                        }
                    }
                    let ev = match rx.recv_timeout(REEMIT_EVERY) {
                        Ok(ev) => ev,
                        Err(mdns_sd::RecvTimeoutError::Timeout) => continue,
                        Err(mdns_sd::RecvTimeoutError::Disconnected) => break,
                    };
                    let info = match ev {
                        ServiceEvent::ServiceResolved(info) => info,
                        ServiceEvent::ServiceRemoved(_, fullname) => {
                            live.remove(&fullname);
                            continue;
                        }
                        _ => continue,
                    };
                    let get = |k: &str| info.get_property_val_str(k).map(str::to_string);
                    let Some(advert) = advert_from_txt(get) else {
                        continue;
                    };
                    if advert.id == me {
                        continue;
                    }
                    let addrs: Vec<SocketAddr> = info
                        .get_addresses_v4()
                        .into_iter()
                        .map(|ip| SocketAddr::new(ip.into(), info.get_port()))
                        .collect();
                    if addrs.is_empty() {
                        continue;
                    }
                    let s = Sighting {
                        advert,
                        addrs,
                        source: Source::Mdns,
                    };
                    live.insert(info.get_fullname().to_string(), s.clone());
                    if out.try_send(s).is_err() {
                        break;
                    }
                }
            })
            .ok();
        Ok(this)
    }

    pub fn update(&self, a: &Advert) {
        let mut cur = self.fullname.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = cur.take() {
            let _ = self.daemon.unregister(&old);
        }
        if let Some(info) = service_info(a, &self.ty) {
            let full = info.get_fullname().to_string();
            match self.daemon.register(info) {
                Ok(()) => *cur = Some(full),
                Err(e) => tracing::warn!("mDNS 注册失败：{e}"),
            }
        }
    }

    pub fn refresh(&self) {
        // mdns-sd 自己监视网卡变化并周期性地重新查询，这里不需要额外操作。
    }

    pub fn stop(self) {
        loop {
            match self.daemon.shutdown() {
                Ok(done) => {
                    done.recv().expect("mDNS daemon lost shutdown acknowledgement");
                    break;
                }
                // This is command-queue backpressure, not a port-bind retry. All
                // producers have stopped; allow the daemon to consume queued commands.
                Err(mdns_sd::Error::Again) => std::thread::yield_now(),
                Err(mdns_sd::Error::DaemonShutdown) => break,
                Err(error) => panic!("mDNS shutdown request failed: {error}"),
            }
        }
        if let Some(thread) = self.thread {
            thread.join().expect("mDNS browse thread panicked");
        }
    }
}
