//! Local loopback API used by the UI through [`crate::remote::RemoteApi`] and
//! by command-line clients through [`Client`].
//!
//! - 端口：`OINK_IPC_PORT`，默认 QUIC 端口 + 2（47472）。端口与一次性令牌写在数据目录的 `ipc.json`
//!   （Unix 上权限 0600；Windows 上在用户自己的 `%APPDATA%` 下），只有能读这个文件的本机用户才能调用。
//! - 格式：一行一个 JSON。请求 `{"token": "...", ...}`，再带下面其中一项：
//!   - `"call": Request`（见 [`crate::api::Request`]）→ `{"ok": Reply}` 或 `{"err": ApiError}`；
//!   - `"hello": true` → [`ServiceInfo`]（版本、进程号）；
//!   - `"stop": true` → 应答后服务进程优雅退出；
//!   - `"subscribe": true` → 先应答 [`ServiceInfo`]，然后这条连接只推送事件：`{"event": ApiEvent}`；服务进程
//!     即将退出时推送 `{"closing": "stopping"}`；界面处理不过来（积压超过 [`EVENT_BACKLOG`] 条）时推送
//!     `{"closing": "lagged"}` 后断开，界面重新订阅并重新拉取全部状态。
//! - 同一连接上可以依次发多个请求（界面复用连接）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::api::{self, ApiError, ApiEvent, Request};
use crate::node::Node;

mod ui;
pub use ui::UiControl;

pub const IPC_FILE: &str = "ipc.json";

/// A missing owner cannot mean successful stop while its endpoint is live.
/// Do not send commands to an unowned endpoint: its recorded port may have been
/// reused. Only an absent file or a refused loopback connection proves it idle;
/// unreadable endpoints and other socket errors leave shutdown unverified.
pub(crate) fn verify_no_listener(data_dir: &Path, file: &str) -> anyhow::Result<()> {
    let text = match std::fs::read(data_dir.join(file)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => anyhow::bail!("cannot verify Oink has stopped: cannot read {file}: {error}"),
    };
    let endpoint: Endpoint = serde_json::from_slice(&text)
        .map_err(|error| anyhow::anyhow!("cannot verify Oink has stopped: invalid {file}: {error}"))?;
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], endpoint.port));
    // Windows 11 reports a refused loopback connect after about two seconds
    // (measured with a closed private listener). A short UI polling budget
    // misclassifies stale endpoints as unknown; retain a bounded 3-second check.
    match std::net::TcpStream::connect_timeout(&address, Duration::from_secs(3)) {
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => Ok(()),
        Err(error) => anyhow::bail!("cannot verify Oink has stopped: cannot query {file}: {error}"),
        Ok(_) => anyhow::bail!(
            "Oink process ownership unavailable: {file} is still reachable; cannot verify Oink has stopped"
        ),
    }
}

/// 一条事件订阅连接最多积压多少条事件（界面卡住时不无限占用内存）。
pub const EVENT_BACKLOG: usize = 4096;

/// `closing` 的取值：服务进程正在按请求退出。
pub const CLOSING_STOPPING: &str = "stopping";
/// `closing` 的取值：这条订阅积压太多，服务进程断开它（服务仍在运行）。
pub const CLOSING_LAGGED: &str = "lagged";
pub const CLOSING_BUILD_MISMATCH: &str = "build_mismatch";

/// Local IPC is private to one bundle build, including its embedded frontend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildIdentity {
    pub version: String,
    pub build: String,
    pub source: String,
}

impl BuildIdentity {
    pub fn current() -> Self {
        Self {
            version: env!("OINK_RELEASE_VERSION").into(),
            build: env!("OINK_RELEASE_BUILD").into(),
            source: env!("OINK_DIAGNOSTIC_SOURCE_TREE_SHA256").into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Endpoint {
    pub port: u16,
    pub token: String,
}

impl Endpoint {
    /// 读取数据目录里的 `ipc.json`。
    pub(crate) fn read(data_dir: &Path) -> anyhow::Result<Self> {
        Self::read_file(data_dir, IPC_FILE)
    }

    fn read_file(data_dir: &Path, name: &str) -> anyhow::Result<Self> {
        let path = data_dir.join(name);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("服务进程没有在运行（读不到 {}：{e}）", path.display()))?;
        Ok(serde_json::from_str(&text)?)
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Envelope {
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<[u8; 16]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<Request>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub stop: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hello: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub subscribe: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub ui_ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_views: Option<api::LiveViews>,
}

/// `hello` / `subscribe` 的应答：正在运行的服务进程。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceInfo {
    /// 服务进程的版本（`OINK_RELEASE_VERSION`）。
    pub version: String,
    pub pid: u32,
    pub build: BuildIdentity,
}

impl ServiceInfo {
    pub fn current() -> Self {
        Self {
            version: env!("OINK_RELEASE_VERSION").to_owned(),
            pid: std::process::id(),
            build: BuildIdentity::current(),
        }
    }
}

/// 运行中的 IPC 服务。
#[derive(Debug)]
pub struct Server {
    task: tokio::task::JoinHandle<()>,
    file: PathBuf,
    port: u16,
    closing: watch::Sender<bool>,
    conns: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Server {
    pub(crate) fn configured_port(network_port: u16) -> u16 {
        oink_foundation::debug_env!("OINK_IPC_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| {
                if network_port == 0 {
                    0
                } else {
                    network_port.wrapping_add(2)
                }
            })
    }

    /// 端口：`OINK_IPC_PORT`，默认 QUIC 端口 + 2。
    pub async fn start(node: Node) -> anyhow::Result<Self> {
        let port = Self::configured_port(node.links()?.local_addr().port());
        Self::start_on(node, port).await
    }

    /// 在指定端口监听（0 = 系统分配，测试用）；实际端口写进 `ipc.json`。
    pub async fn start_on(node: Node, port: u16) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        Self::start_listening(node, listener).await
    }

    pub(crate) async fn start_listening(node: Node, listener: TcpListener) -> anyhow::Result<Self> {
        let port = listener.local_addr()?.port();
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|e| anyhow::anyhow!("IPC token entropy: {e}"))?;
        let token: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
        let file = node.data_dir().join(IPC_FILE);
        write_private(
            &file,
            &serde_json::to_string(&Endpoint {
                port,
                token: token.clone(),
            })?,
        )?;
        tracing::info!(port, "local IPC listening");
        let (closing, _) = watch::channel(false);
        let conns: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> = Arc::default();
        let task = {
            let closing = closing.clone();
            let conns = conns.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        continue;
                    };
                    let node = node.clone();
                    let token = token.clone();
                    let closing = closing.subscribe();
                    let handle = tokio::spawn(async move {
                        if let Err(e) = serve(stream, node, token, closing).await {
                            tracing::debug!(%e, "IPC connection ended");
                        }
                    });
                    let mut conns = conns.lock().unwrap_or_else(|e| e.into_inner());
                    conns.retain(|h| !h.is_finished());
                    conns.push(handle);
                }
            })
        };
        Ok(Self {
            task,
            file,
            port,
            closing,
            conns,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// 优雅关闭：告诉事件订阅者服务即将退出（`{"closing": "stopping"}`），等各连接结束（最多 1 秒），
    /// 然后停止监听、删除 `ipc.json`。
    pub async fn close(self) {
        self.disconnect_clients().await;
        let _ = std::fs::remove_file(&self.file);
    }

    /// Announce/close subscriptions first; keep the endpoint file until host cleanup ends.
    pub(crate) async fn disconnect_clients(&self) {
        self.task.abort();
        self.closing.send_replace(true);
        let conns: Vec<_> = std::mem::take(&mut *self.conns.lock().unwrap_or_else(|e| e.into_inner()));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        for mut h in conns {
            if tokio::time::timeout_at(deadline, &mut h).await.is_err() {
                h.abort();
            }
        }
    }

    /// 立即停止（不等订阅者）。
    pub fn stop(self) {
        self.task.abort();
        self.closing.send_replace(true);
        for h in self.conns.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            h.abort();
        }
        let _ = std::fs::remove_file(&self.file);
    }
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

async fn write_line(w: &mut tokio::net::tcp::OwnedWriteHalf, v: &serde_json::Value) -> anyhow::Result<()> {
    let mut s = serde_json::to_string(v)?;
    s.push('\n');
    w.write_all(s.as_bytes()).await?;
    Ok(())
}

async fn serve(stream: TcpStream, node: Node, token: String, closing: watch::Receiver<bool>) -> anyhow::Result<()> {
    let connection_client = api::request_context::new_client();
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        let mut stop = false;
        let reply = match serde_json::from_str::<Envelope>(&line) {
            Err(e) => serde_json::json!({ "err": ApiError::InvalidArgument { message: e.to_string() } }),
            Ok(env) if env.token != token => {
                serde_json::json!({ "err": ApiError::InvalidArgument { message: "令牌不对".into() } })
            }
            Ok(env) if env.stop => {
                stop = true;
                serde_json::json!({ "ok": null })
            }
            Ok(env) if env.build.as_ref() != Some(&BuildIdentity::current()) => {
                let backend = BuildIdentity::current();
                tracing::warn!(client = ?env.build, ?backend, "local IPC build mismatch; refusing client; exit and reopen Oink from the current bundle");
                write_line(
                    &mut w,
                    &serde_json::json!({
                        "closing": CLOSING_BUILD_MISMATCH,
                        "backend": backend,
                        "err": ApiError::Unavailable { message: "local IPC build mismatch".into() }
                    }),
                )
                .await?;
                return Ok(());
            }
            Ok(env) if env.ui_ready => {
                crate::backend::ui_ready(&node);
                serde_json::json!({ "ok": null })
            }
            Ok(env) if env.hello => serde_json::json!({ "ok": ServiceInfo::current() }),
            Ok(env) if env.subscribe => {
                return stream_events(lines, w, node, closing, env.live_views.unwrap_or(api::LiveViews::ALL)).await;
            }
            Ok(env) => {
                let node = node.clone();
                tokio::task::spawn_blocking(move || match env.call {
                    Some(req) => {
                        match api::request_context::with_client(env.client_id.unwrap_or(connection_client), || {
                            api::dispatch(&node, req)
                        }) {
                            Ok(r) => serde_json::json!({ "ok": r }),
                            Err(e) => serde_json::json!({ "err": e }),
                        }
                    }
                    None => serde_json::json!({ "err": ApiError::InvalidArgument { message: "缺少 call".into() } }),
                })
                .await?
            }
        };
        write_line(&mut w, &reply).await?;
        if stop {
            tracing::info!("stop requested over local IPC");
            node.request_stop();
            return Ok(());
        }
    }
    Ok(())
}

/// 事件订阅：把服务的事件逐行推给这条连接，直到对方断开、服务退出或积压过多。
async fn stream_events(
    mut lines: tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
    mut w: tokio::net::tcp::OwnedWriteHalf,
    node: Node,
    mut closing: watch::Receiver<bool>,
    views: api::LiveViews,
) -> anyhow::Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ApiEvent>(EVENT_BACKLOG);
    let lagged = Arc::new(tokio::sync::Notify::new());
    let sink = {
        let lagged = lagged.clone();
        Arc::new(move |ev: &ApiEvent| {
            if let Err(tokio::sync::mpsc::error::TrySendError::Full(_)) = tx.try_send(ev.clone()) {
                lagged.notify_one();
            }
        })
    };
    // 先订阅再应答：界面收到应答后才重新拉取状态，之后的变化一定会作为事件到达。
    let id = node.subscribe_views(sink, views);
    tracing::info!("UI subscribed to service events over local IPC");
    let result = async {
        write_line(&mut w, &serde_json::json!({ "ok": ServiceInfo::current() })).await?;
        loop {
            tokio::select! {
                ev = rx.recv() => {
                    let Some(ev) = ev else { return Ok(()) };
                    write_line(&mut w, &serde_json::json!({ "event": ev })).await?;
                }
                () = lagged.notified() => {
                    tracing::warn!(backlog = EVENT_BACKLOG, "IPC event subscriber is not keeping up; disconnecting it");
                    write_line(&mut w, &serde_json::json!({ "closing": CLOSING_LAGGED })).await?;
                    return Ok(());
                }
                requested = async { closing.wait_for(|c| *c).await.map(|_| ()) } => {
                    // Losing the server task/sender is a broken connection, not
                    // an explicit shutdown request. Only a true value announces stop.
                    if requested.is_ok() {
                        write_line(&mut w, &serde_json::json!({ "closing": CLOSING_STOPPING })).await?;
                    }
                    return Ok(());
                }
                () = node.wait_for_stop() => {
                    write_line(&mut w, &serde_json::json!({ "closing": CLOSING_STOPPING })).await?;
                    return Ok(());
                }
                // 订阅连接接收本客户端的可见视图需求；EOF 表示对方断开。
                line = lines.next_line() => {
                    match line? {
                        None => return Ok(()),
                        Some(line) => {
                            // This connection was authenticated at subscription.
                            if let Ok(views) = serde_json::from_str::<api::LiveViews>(&line) {
                                node.set_live_views(id, views);
                            }
                        }
                    }
                }
            }
        }
    }
    .await;
    node.unsubscribe(id);
    tracing::info!("UI event subscription ended");
    let _ = w.shutdown().await;
    result
}

/// Same completed-result contract for CLI and RemoteApi/UI saves. The server
/// single-flights saves; a transport deadline cannot imply that it cancelled one.
pub(crate) const DIAGNOSTICS_SAVE_TIMEOUT: Option<Duration> = None;
pub(crate) fn diagnostics_read_error(error: std::io::Error) -> String {
    if matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ) {
        "诊断保存等待超时，结果尚未确认；服务端可能仍在保存，请用 diagnostics list 或界面列表查询，重复保存会合并正在执行的请求。".into()
    } else {
        format!("诊断保存连接中断，结果尚未确认：{error}；请用 diagnostics list 或界面列表查询。")
    }
}

/// 命令行一侧：调用正在运行的服务进程。
#[derive(Debug)]
pub struct Client {
    endpoint: Endpoint,
    client_id: [u8; 16],
}

impl Client {
    /// 读取数据目录里的 `ipc.json`。
    pub fn connect(data_dir: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            endpoint: Endpoint::read(data_dir)?,
            client_id: api::request_context::new_client(),
        })
    }

    fn roundtrip(&self, env: &Envelope) -> anyhow::Result<serde_json::Value> {
        self.roundtrip_with_timeout(env, Some(Duration::from_secs(30)))
    }

    // Keep socket setup separate so tests can inspect the kernel's configured deadlines
    // without comparing elapsed wall time (which also includes scheduler delay).
    fn bounded_stream(&self, timeout: Duration) -> anyhow::Result<std::net::TcpStream> {
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], self.endpoint.port));
        let s = std::net::TcpStream::connect_timeout(&address, timeout)
            .map_err(|e| anyhow::anyhow!("连不上服务进程（端口 {}）：{e}", self.endpoint.port))?;
        s.set_read_timeout(Some(timeout))?;
        s.set_write_timeout(Some(timeout))?;
        Ok(s)
    }

    fn roundtrip_with_timeout(&self, env: &Envelope, timeout: Option<Duration>) -> anyhow::Result<serde_json::Value> {
        use std::io::{BufRead, Write};
        let mut s = self.bounded_stream(timeout.unwrap_or(Duration::from_secs(30)))?;
        s.set_read_timeout(timeout)?;
        let mut line = serde_json::to_string(env)?;
        line.push('\n');
        s.write_all(line.as_bytes())?;
        let mut r = std::io::BufReader::new(s);
        let mut out = String::new();
        r.read_line(&mut out).map_err(|error| {
            if matches!(env.call, Some(Request::ExportDiagnostics { .. })) {
                anyhow::anyhow!(diagnostics_read_error(error))
            } else {
                error.into()
            }
        })?;
        let v: serde_json::Value = serde_json::from_str(&out)?;
        if let Some(e) = v.get("err") {
            let e: ApiError = serde_json::from_value(e.clone())?;
            return Err(anyhow::anyhow!("{e}"));
        }
        Ok(v.get("ok").cloned().unwrap_or(serde_json::Value::Null))
    }

    fn envelope(&self) -> Envelope {
        Envelope {
            token: self.endpoint.token.clone(),
            build: Some(BuildIdentity::current()),
            client_id: Some(self.client_id),
            ..Envelope::default()
        }
    }

    /// 调用一个 API（`Request`），返回应答的 JSON。
    pub fn call(&self, req: Request) -> anyhow::Result<serde_json::Value> {
        let timeout = if matches!(req, Request::ExportDiagnostics { .. }) {
            DIAGNOSTICS_SAVE_TIMEOUT
        } else {
            Some(Duration::from_secs(30))
        };
        self.roundtrip_with_timeout(
            &Envelope {
                call: Some(req),
                ..self.envelope()
            },
            timeout,
        )
    }

    /// 请求服务进程优雅停止；应答表示请求已收到。
    pub fn stop(&self) -> anyhow::Result<serde_json::Value> {
        self.stop_with_timeout(Duration::from_secs(30))
    }

    pub(crate) fn stop_with_timeout(&self, timeout: Duration) -> anyhow::Result<serde_json::Value> {
        self.roundtrip_with_timeout(
            &Envelope {
                stop: true,
                ..self.envelope()
            },
            Some(timeout),
        )
    }

    /// Announce that the native UI has installed its event subscription.
    pub fn ui_ready(&self) -> anyhow::Result<()> {
        self.roundtrip(&Envelope {
            ui_ready: true,
            ..self.envelope()
        })?;
        Ok(())
    }

    /// 正在运行的服务进程的版本与进程号。
    pub fn hello(&self) -> anyhow::Result<ServiceInfo> {
        self.hello_with_timeout(Duration::from_secs(30))
    }

    /// Authenticated handshake with bounded connect, read, and write timeouts.
    pub fn hello_with_timeout(&self, timeout: Duration) -> anyhow::Result<ServiceInfo> {
        Ok(serde_json::from_value(self.roundtrip_with_timeout(
            &Envelope {
                hello: true,
                ..self.envelope()
            },
            Some(timeout),
        )?)?)
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn absent_stale_live_and_unreadable_endpoints_have_distinct_stop_results() {
        let dir = tempfile::tempdir().unwrap();
        assert!(verify_no_listener(dir.path(), IPC_FILE).is_ok());
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let endpoint = Endpoint {
            port: listener.local_addr().unwrap().port(),
            token: "private-test-token".into(),
        };
        write_private(&dir.path().join(IPC_FILE), &serde_json::to_string(&endpoint).unwrap()).unwrap();
        assert!(verify_no_listener(dir.path(), IPC_FILE).is_err());
        drop(listener);
        let stale = verify_no_listener(dir.path(), IPC_FILE);
        assert!(stale.is_ok(), "stale endpoint: {stale:?}");
        std::fs::write(dir.path().join(IPC_FILE), b"invalid").unwrap();
        assert!(verify_no_listener(dir.path(), IPC_FILE).is_err());
        std::fs::remove_file(dir.path().join(IPC_FILE)).unwrap();
        std::fs::create_dir(dir.path().join(IPC_FILE)).unwrap();
        assert!(verify_no_listener(dir.path(), IPC_FILE).is_err());
    }

    #[test]
    fn live_views_follow_the_authenticated_subscription_and_disconnect() {
        crate::test_watchdog::run(
            module_path!(),
            "live_views_follow_the_authenticated_subscription_and_disconnect",
            Some(2),
            live_views_follow_the_authenticated_subscription_and_disconnect_scenario(),
        );
    }

    async fn live_views_follow_the_authenticated_subscription_and_disconnect_scenario() {
        use crate::api::{AgentApi, LiveViews};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let node = Node::start(crate::node::NodeOptions {
            net: oink_test_support::network_config(dir.path().into(), "live-views", 0),
            input: Arc::new(oink_test_support::input::FakeInput::new()),
            displays: vec![],
            display_backend: None,
            clipboard: None,
            mode_switch_in_callback: true,
            capture_reports_key_repeat: true,
            cursor_trail: false,
        })
        .await
        .unwrap();
        let server = Server::start_on(node.clone(), 0).await.unwrap();
        let remote = crate::remote::RemoteApi::new(dir.path());
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        remote.subscribe(Arc::new(move |event| {
            if matches!(event, ApiEvent::Stats(_)) {
                seen.fetch_add(1, Ordering::Relaxed);
            }
        }));
        let follower = remote.clone();
        let (ready, connected) = tokio::sync::oneshot::channel();
        let follow = std::thread::spawn(move || {
            follower.follow_events(|_| {
                let _ = ready.send(());
            })
        });
        connected.await.unwrap();
        assert!(!node.has_stats_subscribers());
        node.emit(ApiEvent::Stats(vec![]));
        remote
            .set_live_views(LiveViews {
                stats: true,
                ..Default::default()
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while count.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(node.has_stats_subscribers());
        remote.set_live_views(LiveViews::default()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while node.has_stats_subscribers() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        crate::test_watchdog::phase("cancel and join idle IPC subscription");
        remote.stop_following();
        assert_eq!(follow.join().unwrap(), crate::remote::EventsEnd::Cancelled);
        // Force cancellation after authentication but before the first event
        // read, without a sleep that masks the shutdown/read boundary.
        let immediate = crate::remote::RemoteApi::new(dir.path());
        let reader = immediate.clone();
        let early = std::thread::spawn(move || reader.follow_events(|_| reader.stop_following()));
        crate::test_watchdog::phase("cancel before first IPC event read");
        assert_eq!(early.join().unwrap(), crate::remote::EventsEnd::Cancelled);
        server.stop();
        node.shutdown().await;
    }

    #[test]
    fn diagnostic_save_socket_timeout_explains_unconfirmed_completion() {
        let listener = (47470..=47489)
            .find_map(|port| std::net::TcpListener::bind(("127.0.0.1", port)).ok())
            .unwrap();
        let client = Client {
            client_id: [1; 16],
            endpoint: Endpoint {
                port: listener.local_addr().unwrap().port(),
                token: "save-timeout".into(),
            },
        };
        let (release, wait) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (_connection, _) = listener.accept().unwrap();
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let error = client
            .roundtrip_with_timeout(
                &Envelope {
                    call: Some(Request::ExportDiagnostics { last_minutes: None }),
                    ..client.envelope()
                },
                Some(Duration::from_millis(10)),
            )
            .unwrap_err();
        release.send(()).unwrap();
        server.join().unwrap();
        let message = error.to_string();
        assert!(message.contains("结果尚未确认"), "{message}");
        assert!(message.contains("diagnostics list"), "{message}");
        assert!(!message.contains("os error"), "{message}");
    }

    #[test]
    fn bounded_stream_has_read_and_write_deadlines() {
        let listener = (47470..=47489)
            .find_map(|port| std::net::TcpListener::bind(("127.0.0.1", port)).ok())
            .expect("no free Oink test TCP port");
        let client = Client {
            client_id: [1; 16],
            endpoint: Endpoint {
                port: listener.local_addr().unwrap().port(),
                token: "deadline-test".into(),
            },
        };
        // Inspect configured socket options, not how quickly this process gets scheduled.
        // This also fails immediately if a deadline is removed, rather than hanging a
        // test waiting for an unanswered hello. The CLI fixture separately verifies EOF.
        let timeout = Duration::from_millis(500);
        let stream = client.bounded_stream(timeout).unwrap();
        assert_eq!(stream.read_timeout().unwrap(), Some(timeout));
        assert_eq!(stream.write_timeout().unwrap(), Some(timeout));
    }
}

#[cfg(test)]
mod product_boundary {
    use super::Envelope;

    #[test]
    fn development_envelope_keys_are_rejected() {
        for hook in [
            serde_json::json!({"debug": true}),
            serde_json::json!({"synth": {"kind": "idle_activity"}}),
        ] {
            for operation in ["hello", "stop", "subscribe"] {
                let mut request = hook.clone();
                request["token"] = "test-token".into();
                request[operation] = true.into();
                let decoded = serde_json::from_value::<Envelope>(request);
                assert!(decoded.is_err(), "{operation}: {hook}");
            }
        }
        // Unknown keys fail before dispatch, even beside a valid product operation.
        assert!(
            serde_json::from_value::<Envelope>(serde_json::json!({
                "token": "test-token", "hello": true, "unknown_hook": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<Envelope>(serde_json::json!({
                "token": "test-token", "hello": true
            }))
            .is_ok()
        );
    }
}
