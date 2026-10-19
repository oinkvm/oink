//! macOS：通过系统的 DNS-SD（`dns_sd.h`，由 mDNSResponder 提供）注册与浏览 `_oink._udp`。
//!
//! 为什么不用纯 Rust 的 mDNS 实现：
//! - 纯 Rust 实现要自己绑定 UDP 5353、收发组播，与系统的 mDNSResponder 抢同一个端口；应用防火墙开启
//!   时会为本进程弹「是否允许接受传入连接」，而 mDNSResponder 本身是豁免的；
//! - 本地网络隐私（macOS 15+）按 `NSBonjourServices` 声明的服务类型授权，走系统 DNS-SD 才与声明一致，
//!   首次浏览时由系统在合适的时机弹出询问；
//! - 系统实现负责睡眠代理、网卡变化、名字冲突处理，与其他 Bonjour 软件共存更好。
//!
//! 所有 DNS-SD 调用都在一个专用线程里进行：共享一条到 mDNSResponder 的连接
//! （`kDNSServiceFlagsShareConnection`），用 `poll` 等待可读后调用 `DNSServiceProcessResult`，
//! 回调也都在这个线程中执行。

use super::{Advert, SERVICE_TYPE, Sighting, Source, advert_from_txt, instance_name, txt_pairs};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_uchar, c_void};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::{fd::AsRawFd, unix::net::UnixDatagram};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

type DNSServiceRef = *mut c_void;
type DNSServiceFlags = u32;
type DNSServiceErrorType = i32;

const K_SHARE_CONNECTION: DNSServiceFlags = 0x4000;
const K_FLAGS_ADD: DNSServiceFlags = 0x2;
const K_PROTOCOL_IPV4: u32 = 0x1;

type RegisterReply = extern "C" fn(
    DNSServiceRef,
    DNSServiceFlags,
    DNSServiceErrorType,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
);
type BrowseReply = extern "C" fn(
    DNSServiceRef,
    DNSServiceFlags,
    u32,
    DNSServiceErrorType,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
);
type ResolveReply = extern "C" fn(
    DNSServiceRef,
    DNSServiceFlags,
    u32,
    DNSServiceErrorType,
    *const c_char,
    *const c_char,
    u16,
    u16,
    *const c_uchar,
    *mut c_void,
);
type AddrInfoReply = extern "C" fn(
    DNSServiceRef,
    DNSServiceFlags,
    u32,
    DNSServiceErrorType,
    *const c_char,
    *const libc::sockaddr,
    u32,
    *mut c_void,
);

// dns_sd.h 中的函数由 libSystem 提供，无需额外链接。
unsafe extern "C" {
    fn DNSServiceCreateConnection(sd: *mut DNSServiceRef) -> DNSServiceErrorType;
    fn DNSServiceRefSockFD(sd: DNSServiceRef) -> c_int;
    fn DNSServiceProcessResult(sd: DNSServiceRef) -> DNSServiceErrorType;
    fn DNSServiceRefDeallocate(sd: DNSServiceRef);
    fn DNSServiceRegister(
        sd: *mut DNSServiceRef,
        flags: DNSServiceFlags,
        if_index: u32,
        name: *const c_char,
        regtype: *const c_char,
        domain: *const c_char,
        host: *const c_char,
        port_be: u16,
        txt_len: u16,
        txt: *const c_void,
        cb: Option<RegisterReply>,
        ctx: *mut c_void,
    ) -> DNSServiceErrorType;
    fn DNSServiceBrowse(
        sd: *mut DNSServiceRef,
        flags: DNSServiceFlags,
        if_index: u32,
        regtype: *const c_char,
        domain: *const c_char,
        cb: BrowseReply,
        ctx: *mut c_void,
    ) -> DNSServiceErrorType;
    fn DNSServiceResolve(
        sd: *mut DNSServiceRef,
        flags: DNSServiceFlags,
        if_index: u32,
        name: *const c_char,
        regtype: *const c_char,
        domain: *const c_char,
        cb: ResolveReply,
        ctx: *mut c_void,
    ) -> DNSServiceErrorType;
    fn DNSServiceGetAddrInfo(
        sd: *mut DNSServiceRef,
        flags: DNSServiceFlags,
        if_index: u32,
        protocol: u32,
        hostname: *const c_char,
        cb: AddrInfoReply,
        ctx: *mut c_void,
    ) -> DNSServiceErrorType;
}

enum Cmd {
    Update(Advert),
    Refresh,
    Stop,
}

/// 运行中的系统 DNS-SD。
pub(crate) struct DnsSd {
    cmd: std_mpsc::Sender<Cmd>,
    wake: UnixDatagram,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DnsSd {
    pub fn start(advert: Advert, out: mpsc::Sender<Sighting>) -> Self {
        let (tx, rx) = std_mpsc::channel();
        let (wake, pending) = UnixDatagram::pair().expect("DNS-SD wake socket");
        wake.set_nonblocking(true).expect("nonblocking wake socket");
        pending.set_nonblocking(true).expect("nonblocking wake socket");
        let spawned = std::thread::Builder::new()
            .name("oink-dnssd".into())
            .spawn(move || run(advert, out, rx, pending));
        if let Err(e) = &spawned {
            tracing::warn!("DNS-SD 线程启动失败：{e}");
        }
        Self {
            cmd: tx,
            wake,
            thread: spawned.ok(),
        }
    }

    pub fn update(&self, a: Advert) {
        let _ = self.cmd.send(Cmd::Update(a));
        let _ = self.wake.send(&[1]);
    }

    pub fn refresh(&self) {
        let _ = self.cmd.send(Cmd::Refresh);
        let _ = self.wake.send(&[1]);
    }

    pub fn stop(self) {
        let _ = self.cmd.send(Cmd::Stop);
        let _ = self.wake.send(&[1]);
        if let Some(thread) = self.thread {
            thread.join().expect("DNS-SD thread panicked");
        }
    }
}

/// 回调上下文，只在 DNS-SD 线程内使用。
struct Ctx {
    conn: DNSServiceRef,
    me: [u8; 32],
    out: mpsc::Sender<Sighting>,
    /// 解析、查地址的子操作，到期后释放。
    subs: Vec<(DNSServiceRef, Instant)>,
    /// 解析操作 → 服务键；查地址的操作 → (服务键, 已解析出的通告内容)。
    resolving: HashMap<usize, ServiceKey>,
    pending: HashMap<usize, (ServiceKey, Advert)>,
    /// 浏览到、还没被移除的服务 → 最近一次解析结果。
    live: HashMap<ServiceKey, LiveService>,
    /// 当前浏览的代数（每次重新浏览加一）。
    generation: u64,
}

/// 服务实例名 + 网卡序号。
type ServiceKey = (CString, u32);

struct LiveService {
    sighting: Option<Sighting>,
    /// 最近一次在哪一代浏览中出现。
    generation: u64,
}

/// 附近条目的有效期是 10 秒（`nearby_ttl`），而浏览只在服务出现时报告一次：仍在的服务每隔这么久
/// 重新交给上层一次（结果来自缓存，不产生网络流量）。
const REEMIT_EVERY: Duration = Duration::from_secs(3);
/// 重新浏览后，这么久内没有再次出现的服务视为已经消失。
const REBROWSE_GRACE: Duration = Duration::from_secs(5);

fn txt_record(a: &Advert) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in txt_pairs(a) {
        let item = format!("{k}={v}");
        let b = &item.as_bytes()[..item.len().min(255)];
        out.push(b.len() as u8);
        out.extend_from_slice(b);
    }
    out
}

fn parse_txt(txt: &[u8]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut i = 0;
    while i < txt.len() {
        let n = txt[i] as usize;
        let Some(item) = txt.get(i + 1..i + 1 + n) else {
            break;
        };
        let s = String::from_utf8_lossy(item);
        if let Some((k, v)) = s.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
        i += 1 + n;
    }
    map
}

fn cstr(p: *const c_char) -> Option<&'static CStr> {
    if p.is_null() {
        None
    } else {
        // SAFETY: mDNSResponder 传入的是以 NUL 结尾的字符串，只在回调期间使用（调用方立刻复制）。
        Some(unsafe { CStr::from_ptr(p) })
    }
}

extern "C" fn on_register(
    _sd: DNSServiceRef,
    _flags: DNSServiceFlags,
    err: DNSServiceErrorType,
    name: *const c_char,
    _regtype: *const c_char,
    _domain: *const c_char,
    _ctx: *mut c_void,
) {
    crate::local_network::observe_dns_sd(err);
    if err != 0 {
        tracing::warn!("DNS-SD 注册失败：{err}");
    } else {
        tracing::debug!(
            "DNS-SD 已注册：{:?}",
            cstr(name).map(|c| c.to_string_lossy().into_owned())
        );
    }
}

extern "C" fn on_browse(
    _sd: DNSServiceRef,
    flags: DNSServiceFlags,
    if_index: u32,
    err: DNSServiceErrorType,
    name: *const c_char,
    regtype: *const c_char,
    domain: *const c_char,
    ctx: *mut c_void,
) {
    crate::local_network::observe_dns_sd(err);
    if err != 0 || ctx.is_null() {
        return;
    }
    // SAFETY: ctx 指向 run() 栈上的 Ctx，回调只在同一线程的 DNSServiceProcessResult 内发生。
    let c = unsafe { &mut *(ctx as *mut Ctx) };
    let Some(key) = cstr(name).map(|n| (n.to_owned(), if_index)) else {
        return;
    };
    if flags & K_FLAGS_ADD == 0 {
        c.live.remove(&key);
        return;
    }
    let generation = c.generation;
    c.live
        .entry(key.clone())
        .or_insert(LiveService {
            sighting: None,
            generation,
        })
        .generation = generation;
    let mut sd = c.conn;
    // SAFETY: 参数都是回调传入的有效 C 字符串；sd 是共享连接的副本（ShareConnection 要求）。
    let r = unsafe {
        DNSServiceResolve(
            &mut sd,
            K_SHARE_CONNECTION,
            if_index,
            name,
            regtype,
            domain,
            on_resolve,
            ctx,
        )
    };
    if r == 0 {
        c.resolving.insert(sd as usize, key);
        c.subs.push((sd, Instant::now() + Duration::from_secs(5)));
    } else {
        crate::local_network::observe_dns_sd(r);
    }
}

extern "C" fn on_resolve(
    rsd: DNSServiceRef,
    _flags: DNSServiceFlags,
    if_index: u32,
    err: DNSServiceErrorType,
    _fullname: *const c_char,
    host: *const c_char,
    _port_be: u16,
    txt_len: u16,
    txt: *const c_uchar,
    ctx: *mut c_void,
) {
    crate::local_network::observe_dns_sd(err);
    if err != 0 || ctx.is_null() || txt.is_null() {
        return;
    }
    // SAFETY: 同 on_browse。
    let c = unsafe { &mut *(ctx as *mut Ctx) };
    // SAFETY: txt 指向长度为 txt_len 的缓冲区，只在回调期间读取。
    let txt = unsafe { std::slice::from_raw_parts(txt, txt_len as usize) };
    let map = parse_txt(txt);
    let Some(advert) = advert_from_txt(|k| map.get(k).cloned()) else {
        return;
    };
    if advert.id.0 == c.me {
        return;
    }
    let Some(key) = c.resolving.get(&(rsd as usize)).cloned() else {
        return;
    };
    let mut sd = c.conn;
    // SAFETY: host 是回调传入的有效 C 字符串。
    let r = unsafe {
        DNSServiceGetAddrInfo(
            &mut sd,
            K_SHARE_CONNECTION,
            if_index,
            K_PROTOCOL_IPV4,
            host,
            on_addr,
            ctx,
        )
    };
    if r == 0 {
        c.pending.insert(sd as usize, (key, advert));
        c.subs.push((sd, Instant::now() + Duration::from_secs(5)));
    } else {
        crate::local_network::observe_dns_sd(r);
    }
}

extern "C" fn on_addr(
    sd: DNSServiceRef,
    flags: DNSServiceFlags,
    _if_index: u32,
    err: DNSServiceErrorType,
    _host: *const c_char,
    addr: *const libc::sockaddr,
    _ttl: u32,
    ctx: *mut c_void,
) {
    crate::local_network::observe_dns_sd(err);
    if err != 0 || ctx.is_null() || addr.is_null() || flags & K_FLAGS_ADD == 0 {
        return;
    }
    // SAFETY: 同 on_browse。
    let c = unsafe { &mut *(ctx as *mut Ctx) };
    let Some((key, advert)) = c.pending.get(&(sd as usize)).cloned() else {
        return;
    };
    // SAFETY: addr 是有效的 sockaddr；只有 AF_INET 时才按 sockaddr_in 读取。
    let ip = unsafe {
        if i32::from((*addr).sa_family) != libc::AF_INET {
            return;
        }
        let sin = &*(addr as *const libc::sockaddr_in);
        Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))
    };
    let s = Sighting {
        addrs: vec![SocketAddr::new(IpAddr::V4(ip), advert.port)],
        advert,
        source: Source::Mdns,
    };
    if let Some(l) = c.live.get_mut(&key) {
        l.sighting = Some(s.clone());
    }
    let _ = c.out.try_send(s);
}

fn register(conn: DNSServiceRef, a: &Advert) -> Option<DNSServiceRef> {
    let name = CString::new(instance_name(a).replace('\0', "")).ok()?;
    let ty = CString::new(SERVICE_TYPE).ok()?;
    let txt = txt_record(a);
    let mut sd = conn;
    // SAFETY: 所有指针在调用期间有效；sd 为共享连接的副本。
    let r = unsafe {
        DNSServiceRegister(
            &mut sd,
            K_SHARE_CONNECTION,
            0,
            name.as_ptr(),
            ty.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            a.port.to_be(),
            txt.len() as u16,
            txt.as_ptr() as *const c_void,
            Some(on_register),
            std::ptr::null_mut(),
        )
    };
    if r != 0 {
        crate::local_network::observe_dns_sd(r);
        tracing::warn!("DNSServiceRegister 失败：{r}");
        return None;
    }
    Some(sd)
}

fn browse(conn: DNSServiceRef, ctx: *mut Ctx) -> Option<DNSServiceRef> {
    let ty = CString::new(SERVICE_TYPE).ok()?;
    let mut sd = conn;
    // SAFETY: ctx 在整个线程生命周期内有效。
    let r = unsafe {
        DNSServiceBrowse(
            &mut sd,
            K_SHARE_CONNECTION,
            0,
            ty.as_ptr(),
            std::ptr::null(),
            on_browse,
            ctx as *mut c_void,
        )
    };
    if r != 0 {
        crate::local_network::observe_dns_sd(r);
        tracing::warn!("DNSServiceBrowse 失败：{r}");
        return None;
    }
    Some(sd)
}

fn dealloc(sd: DNSServiceRef) {
    // SAFETY: sd 由本模块创建且只释放一次。
    unsafe { DNSServiceRefDeallocate(sd) };
}

fn run(mut advert: Advert, out: mpsc::Sender<Sighting>, cmd: std_mpsc::Receiver<Cmd>, wake: UnixDatagram) {
    let mut conn: DNSServiceRef = std::ptr::null_mut();
    // SAFETY: 输出参数。
    let r = unsafe { DNSServiceCreateConnection(&mut conn) };
    if r != 0 {
        tracing::warn!("无法连接 mDNSResponder：{r}");
        return;
    }
    let mut ctx = Box::new(Ctx {
        conn,
        me: advert.id.0,
        out,
        subs: Vec::new(),
        resolving: HashMap::new(),
        pending: HashMap::new(),
        live: HashMap::new(),
        generation: 0,
    });
    let ctx_ptr: *mut Ctx = &mut *ctx;
    let mut reg = register(conn, &advert);
    let mut br = browse(conn, ctx_ptr);
    let mut last_browse = Instant::now();
    let mut last_reemit = Instant::now();
    // SAFETY: conn 有效。
    let fd = unsafe { DNSServiceRefSockFD(conn) };
    loop {
        // 命令
        loop {
            match cmd.try_recv() {
                Ok(Cmd::Update(a)) => {
                    if a != advert {
                        advert = a;
                        if let Some(r) = reg.take() {
                            dealloc(r);
                        }
                        reg = register(conn, &advert);
                    }
                }
                Ok(Cmd::Refresh) => last_browse = Instant::now() - Duration::from_secs(3600),
                Ok(Cmd::Stop) | Err(std_mpsc::TryRecvError::Disconnected) => {
                    for (s, _) in ctx.subs.drain(..) {
                        dealloc(s);
                    }
                    if let Some(r) = reg.take() {
                        dealloc(r);
                    }
                    if let Some(b) = br.take() {
                        dealloc(b);
                    }
                    dealloc(conn);
                    return;
                }
                Err(std_mpsc::TryRecvError::Empty) => break,
            }
        }
        // 定期重新浏览：已知服务的 IP 变了时，浏览不会再报告「新增」，重新浏览会重新解析。
        let rebrowse = if crate::local_network_access() == crate::LocalNetworkAccess::Denied {
            Duration::from_secs(2)
        } else {
            Duration::from_secs(30)
        };
        if last_browse.elapsed() >= rebrowse {
            if let Some(b) = br.take() {
                dealloc(b);
            }
            ctx.generation += 1;
            br = browse(conn, ctx_ptr);
            // Denied registrations also need a fresh operation after a Settings
            // grant; re-browsing alone cannot revive a failed registration.
            if crate::local_network_access() == crate::LocalNetworkAccess::Denied {
                if let Some(r) = reg.take() {
                    dealloc(r);
                }
                reg = register(conn, &advert);
            }
            last_browse = Instant::now();
        }
        // 仍在的服务定期重新交给上层；重新浏览后没有再出现的丢掉。
        if last_reemit.elapsed() >= REEMIT_EVERY {
            last_reemit = Instant::now();
            let cur = ctx.generation;
            let settled = last_browse.elapsed() >= REBROWSE_GRACE;
            ctx.live.retain(|_, l| l.generation == cur || !settled);
            for l in ctx.live.values() {
                if let Some(s) = &l.sighting {
                    let _ = ctx.out.try_send(s.clone());
                }
            }
        }
        // 释放到期的子操作
        let now = Instant::now();
        let mut i = 0;
        while i < ctx.subs.len() {
            if ctx.subs[i].1 <= now {
                let (s, _) = ctx.subs.swap_remove(i);
                ctx.pending.remove(&(s as usize));
                ctx.resolving.remove(&(s as usize));
                dealloc(s);
            } else {
                i += 1;
            }
        }
        let deadline = ctx
            .subs
            .iter()
            .map(|(_, at)| *at)
            .chain([last_browse + rebrowse, last_reemit + REEMIT_EVERY])
            .min()
            .unwrap();
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .as_nanos()
            .div_ceil(1_000_000)
            .min(i32::MAX as u128) as i32;
        let mut descriptors = [
            libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: both descriptors and their output array remain valid through poll.
        let n = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, timeout) };
        if descriptors[1].revents & libc::POLLIN != 0 {
            while wake.recv(&mut [0; 64]).is_ok() {}
        }
        if n > 0 && descriptors[0].revents & libc::POLLIN != 0 {
            // SAFETY: conn 有效；回调在本线程执行。
            let r = unsafe { DNSServiceProcessResult(conn) };
            if r != 0 {
                crate::local_network::observe_dns_sd(r);
                tracing::warn!("DNSServiceProcessResult 失败：{r}，停止 DNS-SD");
                return;
            }
        }
    }
}
