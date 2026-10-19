//! Owns the Tokio runtime, sharing Node, process lock and local IPC listener.
//! The Mac application host keeps these independent of its UI helper. On
//! Windows the SYSTEM launch broker creates the user-session `oink --service`
//! backend; the normal-user tray is an IPC client. Shutdown ends subscriptions
//! before releasing native resources and process ownership.

use std::sync::Arc;
use std::time::{Duration, Instant};

use oink_network::NetworkConfig;

use crate::api::AgentApi;
use crate::instance::{self, InstanceLock};
use crate::ipc;
use crate::node::{Node, NodeOptions};

fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("oink-agent")
        .build()
}

/// A termination request during startup is a normal stop, not a startup fault.
#[derive(Debug, thiserror::Error)]
#[error("service startup interrupted by termination")]
pub struct StartupTerminated;

struct Termination {
    #[cfg(windows)]
    service_stop: Option<(tokio::sync::oneshot::Receiver<()>, oink_winsvc::backend::StopWatch)>,
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hangup: tokio::signal::unix::Signal,
}
impl Termination {
    fn listen() -> std::io::Result<Self> {
        #[cfg(unix)]
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            #[cfg(windows)]
            service_stop: None,
            #[cfg(unix)]
            term: signal(SignalKind::terminate())?,
            #[cfg(unix)]
            interrupt: signal(SignalKind::interrupt())?,
            #[cfg(unix)]
            hangup: signal(SignalKind::hangup())?,
        })
    }
    async fn wait(mut self) {
        #[cfg(unix)]
        tokio::select! {
            _ = self.term.recv() => {},
            _ = self.interrupt.recv() => {},
            _ = self.hangup.recv() => {},
        }
        #[cfg(not(unix))]
        {
            #[cfg(windows)]
            if let Some((stopped, _watch)) = &mut self.service_stop {
                tokio::select! {
                    _ = stopped => {},
                    _ = tokio::signal::ctrl_c() => {},
                }
                return;
            }
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

/// IPC 在宿主里的角色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcMode {
    /// 不开 IPC（测试）。
    Off,
    /// 测试宿主可选 IPC；应用后台始终使用 Required。
    Optional,
    /// 必须开（独立的服务进程：界面只能经 IPC 调用它）。
    Required,
}

/// 运行中的服务进程（自带运行时）。
pub struct AgentHost {
    rt: Option<tokio::runtime::Runtime>,
    node: Node,
    ipc: Option<ipc::Server>,
    _instance: Option<InstanceLock>,
    #[cfg(target_os = "macos")]
    provider_startup: Option<crate::file_provider::startup::Worker>,
}

impl std::fmt::Debug for AgentHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentHost")
            .field("node", &self.node)
            .finish_non_exhaustive()
    }
}

impl AgentHost {
    /// 本机平台 backend + 默认组网配置（`OINK_HOME`、`OINK_PORT` 等环境变量见 `NetworkConfig::from_env`），
    /// 并开启本机 IPC（命令行 `oink-agent status` 等经它调用）。同一数据目录上已有服务进程时返回
    /// [`instance::AlreadyRunning`]。
    pub fn start_platform() -> anyhow::Result<Self> {
        Self::start_platform_with(IpcMode::Required)
    }

    /// 同 [`Self::start_platform`]，指定 IPC 的角色。
    pub fn start_platform_with(ipc: IpcMode) -> anyhow::Result<Self> {
        let net = NetworkConfig::default();
        // 先取单实例锁，再创建任何 backend（第二个实例不能装上第二套键鼠钩子）。
        let lock = instance::acquire(&net.data_dir)?;
        Self::start_platform_locked(net, lock, ipc)
    }

    pub(crate) fn start_platform_locked(net: NetworkConfig, lock: InstanceLock, ipc: IpcMode) -> anyhow::Result<Self> {
        let rt = runtime()?;
        // Register termination before platform backends install cursor restoration hooks.
        // Those hooks chain the previous handler; registering afterwards would chain a
        // default re-raise and bypass graceful shutdown. No disk work runs in a handler.
        let termination = {
            let _enter = rt.enter();
            let termination = Termination::listen()?;
            #[cfg(windows)]
            let termination = {
                let mut termination = termination;
                if oink_winsvc::client::service_installation()? {
                    let (send, receive) = tokio::sync::oneshot::channel();
                    let watch = oink_winsvc::backend::watch_service_stop(move || {
                        let _ = send.send(());
                    })?;
                    termination.service_stop = Some((receive, watch));
                }
                termination
            };
            termination
        };
        // Persist the fresh/reset decision before identity creation and gate
        // provider workers. Native registration/recovery runs in the background;
        // IPC and the first window do not wait for File Provider timeouts.
        #[cfg(target_os = "macos")]
        let provider_startup = crate::file_provider::startup::prepare(&net.data_dir)?;
        let opts = NodeOptions::platform(net)?;
        tracing::info!("platform backends prepared");
        let mut host = Self::start_on_runtime(opts, ipc, rt, termination, Node::start, None)?;
        host._instance = Some(lock);
        #[cfg(target_os = "macos")]
        {
            host.provider_startup = provider_startup;
        }
        host.node.start_platform_dnd();
        Ok(host)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn start_launch_smoke_locked(net: NetworkConfig, lock: InstanceLock) -> anyhow::Result<Self> {
        let opts = crate::backend::smoke::options(net)?;
        let rt = runtime()?;
        let termination = {
            let _enter = rt.enter();
            Termination::listen()?
        };
        let mut host = Self::start_on_runtime(
            opts,
            IpcMode::Required,
            rt,
            termination,
            Node::start_launch_smoke,
            Some(0),
        )?;
        host._instance = Some(lock);
        Ok(host)
    }

    pub fn start(opts: NodeOptions, with_ipc: bool) -> anyhow::Result<Self> {
        Self::start_with(opts, if with_ipc { IpcMode::Optional } else { IpcMode::Off })
    }

    pub fn start_with(opts: NodeOptions, mode: IpcMode) -> anyhow::Result<Self> {
        Self::start_with_node(opts, mode, Node::start)
    }

    /// Use the real host lifecycle with injected fake platform backends and no
    /// native startup services. This entry point is absent from release builds.
    #[cfg(debug_assertions)]
    pub fn start_with_fake_platform(opts: NodeOptions, with_ipc: bool) -> anyhow::Result<Self> {
        Self::start_with_node(
            opts,
            if with_ipc { IpcMode::Optional } else { IpcMode::Off },
            Node::start_with_fake_platform,
        )
    }

    fn start_with_node<F>(
        opts: NodeOptions,
        mode: IpcMode,
        start: impl FnOnce(NodeOptions) -> F,
    ) -> anyhow::Result<Self>
    where
        F: std::future::Future<Output = anyhow::Result<Node>>,
    {
        let rt = runtime()?;
        let termination = {
            let _enter = rt.enter();
            Termination::listen()?
        };
        Self::start_on_runtime(opts, mode, rt, termination, start, None)
    }

    fn start_on_runtime<F>(
        opts: NodeOptions,
        mode: IpcMode,
        rt: tokio::runtime::Runtime,
        termination: Termination,
        start: impl FnOnce(NodeOptions) -> F,
        ipc_port: Option<u16>,
    ) -> anyhow::Result<Self>
    where
        F: std::future::Future<Output = anyhow::Result<Node>>,
    {
        // IPC is a startup prerequisite: fail before capture, discovery or dialling.
        // Keep the bound listener through Node startup so another process cannot take it.
        let listener = match mode {
            IpcMode::Off => None,
            IpcMode::Optional | IpcMode::Required => {
                let port = ipc_port.unwrap_or_else(|| ipc::Server::configured_port(opts.net.port));
                match rt.block_on(tokio::net::TcpListener::bind(("127.0.0.1", port))) {
                    Ok(listener) => Some(listener),
                    Err(error) if mode == IpcMode::Optional => {
                        tracing::warn!(%error, "local IPC unavailable");
                        None
                    }
                    Err(error) => return Err(anyhow::anyhow!("本机 IPC 无法监听：{error}")),
                }
            }
        };
        // Poll termination while Node is starting too. Previously a registered
        // TERM was not consumed until all startup work (including diagnostics)
        // finished. The same listener continues to serve the running host.
        let mut termination = Box::pin(termination.wait());
        let node = rt.block_on(async {
            tokio::select! {
                biased;
                () = &mut termination => Err(StartupTerminated.into()),
                result = start(opts) => result,
            }
        })?;
        let signal_node = node.clone();
        rt.spawn(async move {
            termination.await;
            signal_node.request_stop();
        });
        let ipc = match listener {
            None => None,
            Some(listener) => match rt.block_on(ipc::Server::start_listening(node.clone(), listener)) {
                Ok(s) => Some(s),
                Err(e) if mode == IpcMode::Optional => {
                    tracing::warn!(%e, "local IPC unavailable (another instance running?)");
                    None
                }
                Err(e) => {
                    // Publishing the endpoint can fail after the listener and Node
                    // started. Use the same bounded cleanup as normal process exit;
                    // directly polling native stop here could block the deadline.
                    Self {
                        rt: Some(rt),
                        node,
                        ipc: None,
                        _instance: None,
                        #[cfg(target_os = "macos")]
                        provider_startup: None,
                    }
                    .shutdown();
                    return Err(anyhow::anyhow!("本机 IPC 无法监听：{e}"));
                }
            },
        };
        Ok(Self {
            rt: Some(rt),
            node,
            ipc,
            _instance: None,
            #[cfg(target_os = "macos")]
            provider_startup: None,
        })
    }

    pub fn node(&self) -> &Node {
        &self.node
    }

    /// 界面 API。
    pub fn api(&self) -> Arc<dyn AgentApi> {
        Arc::new(self.node.clone())
    }

    /// 在宿主的运行时上执行。
    pub fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
        self.rt.as_ref().expect("runtime").block_on(f)
    }

    /// 进程退出前的有界清理（TERM / 界面退出 / 安装替换）。端口最终由 OS 回收。
    /// 同进程还要继续运行或重建宿主时，使用 [`Self::shutdown_and_wait`]。
    pub fn shutdown(mut self) {
        self.stop_all(false);
    }

    /// 停止嵌入式宿主并等待网络端口释放，供同进程立即重启使用。
    /// 必须等待网络完成屏障，因此不适用于有退出时限的进程退出路径。
    pub fn shutdown_and_wait(mut self) {
        self.stop_all(true);
    }

    fn stop_all(&mut self, wait_for_ports: bool) {
        if let Some(rt) = self.rt.take() {
            let deadline = Instant::now() + Duration::from_secs(3);
            // Cancel and reap launch tools before releasing process ownership;
            // a fresh launch must not race the previous worker's domain reset.
            #[cfg(target_os = "macos")]
            self.provider_startup.take();
            let node = self.node.clone();
            let ipc = self.ipc.take();
            rt.block_on(async {
                if let Some(server) = &ipc {
                    server.disconnect_clients().await;
                }
                let stopping = node.clone();
                // Native stop/restore calls may synchronously wait for an OS thread.
                // Poll cleanup on a worker so the host can still enforce its deadline.
                let mut cleanup = tokio::spawn(async move {
                    if wait_for_ports {
                        stopping.shutdown().await;
                    } else {
                        stopping.shutdown_for_process_exit().await;
                    }
                });
                if tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), &mut cleanup)
                    .await
                    .is_err()
                {
                    cleanup.abort();
                }
                if wait_for_ports {
                    // Embedded tests must observe socket release even when retaining a Node.
                    if let Ok(links) = node.links() {
                        links.stop().await;
                    }
                }
            });
            if let Some(server) = ipc {
                server.stop();
            }
            rt.shutdown_timeout(deadline.saturating_duration_since(Instant::now()));
        }
    }
}

impl Drop for AgentHost {
    fn drop(&mut self) {
        if std::thread::panicking() {
            #[cfg(target_os = "macos")]
            self.provider_startup.take();
            // A fatal host unwind must leave the recorder's running instance for
            // recovery. Runtime teardown cancels tasks without declaring a clean exit.
            if let Some(rt) = self.rt.take() {
                rt.shutdown_timeout(Duration::ZERO);
            }
        } else {
            self.stop_all(false);
        }
    }
}

/// 服务进程的退出码：已有服务进程在同一数据目录上运行（本进程什么也没做就退出）。
pub const EXIT_ALREADY_RUNNING: u8 = 3;

/// 独立服务进程的入口（Windows 上 `oink.exe --service`）：没有窗口，运行到经 IPC 收到 `stop`。
/// 返回进程退出码。调用前先初始化日志（`logs::init_service`）。
pub fn run_service() -> u8 {
    #[cfg(windows)]
    match oink_winsvc::backend::uiaccess() {
        Ok(true) => {}
        Ok(false) => {
            return match std::env::current_dir()
                .and_then(|cwd| oink_winsvc::backend::start_in(&oink_foundation::paths::data_dir(), &cwd))
            {
                Ok(_) => 0,
                Err(error) => {
                    tracing::error!(%error, "OinkInput UIAccess backend launch failed");
                    1
                }
            };
        }
        Err(error) => {
            tracing::error!(%error, "cannot query backend UIAccess");
            return 1;
        }
    }
    let result = crate::backend::run_main(|| -> anyhow::Result<()> {
        let lifetime = crate::backend::launch::lifetime();
        if lifetime.in_job == Some(true) {
            tracing::warn!(job_limit_flags = ?lifetime.job_limit_flags, "{}", lifetime.status);
        } else {
            tracing::info!(?lifetime, "backend process lifetime");
        }
        let started = Instant::now();
        let host = AgentHost::start_platform_with(IpcMode::Required)?;
        crate::backend::attach(host.node());
        let name = host.node().device_name(host.node().id());
        tracing::info!(
            pid = std::process::id(),
            name = %name,
            port = host.node().links().map(|l| l.local_addr().port()).unwrap_or(0),
            ms = started.elapsed().as_millis() as u64,
            "service process running (no window)"
        );
        host.block_on(host.node().wait_for_stop());
        tracing::info!("service process stopping");
        host.shutdown();
        Ok(())
    });
    match result {
        Ok(()) => 0,
        Err(e) if e.downcast_ref::<StartupTerminated>().is_some() => {
            tracing::info!("service stopped during startup");
            0
        }
        Err(e) if e.downcast_ref::<instance::AlreadyRunning>().is_some() => {
            tracing::info!("{e}; this process exits");
            EXIT_ALREADY_RUNNING
        }
        Err(e) => {
            tracing::error!("服务进程启动失败：{e:#}");
            1
        }
    }
}

/// Product stop succeeds only after both backend and UI release their ownership.
/// A received stop acknowledgement alone is never permission to replace a bundle.
pub fn stop_service(data_dir: &std::path::Path, timeout: Duration) -> anyhow::Result<bool> {
    let deadline = Instant::now() + timeout;
    if !stop_backend(data_dir, timeout)? {
        return Ok(false);
    }
    let mut ui_quit_sent = false;
    loop {
        match instance::is_ui_running(data_dir) {
            Some(false) => {
                ipc::UiControl::verify_stopped(data_dir)?;
                return Ok(true);
            }
            Some(true) if Instant::now() < deadline => {
                if !ui_quit_sent {
                    let budget = deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(100));
                    ui_quit_sent = ipc::UiControl::request_stop(data_dir, budget).is_ok();
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Some(true) => {
                tracing::error!("backend exited but UI still owns the home; Oink has not quit");
                return Ok(false);
            }
            None => anyhow::bail!("UI process ownership unavailable"),
        }
    }
}

/// Uninstall preparation keeps its own UI alive to report errors and start the
/// native finalizer. Every product quit/upgrade caller must use `stop_service`.
pub fn stop_backend(data_dir: &std::path::Path, timeout: Duration) -> anyhow::Result<bool> {
    let deadline = Instant::now() + timeout;
    let running = || -> anyhow::Result<bool> {
        let owned =
            instance::is_running(data_dir).ok_or_else(|| anyhow::anyhow!("backend process ownership unavailable"))?;
        if !owned {
            ipc::verify_no_listener(data_dir, ipc::IPC_FILE)?;
        }
        Ok(owned)
    };
    if !running()? {
        return Ok(true);
    }
    match ipc::Client::connect(data_dir).and_then(|c| c.stop_with_timeout(timeout.min(Duration::from_secs(2)))) {
        Ok(_) => tracing::info!("asked the service process to stop"),
        Err(e) => tracing::warn!("could not ask the service process to stop: {e:#}"),
    }
    while Instant::now() < deadline {
        if !running()? {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(!running()?)
}

#[cfg(all(debug_assertions, all(test, unix)))]
mod signal_tests {
    use super::*;
    use std::io::{BufRead, Write};

    #[test]
    fn termination_child() {
        let _scenario = crate::test_watchdog::Scenario::new(module_path!(), "termination_child");
        let Ok(path) = std::env::var("OINK_TEST_TERM") else {
            return;
        };
        let rt = runtime().unwrap();
        let termination = {
            let _enter = rt.enter();
            Termination::listen().unwrap()
        };
        let r = crate::diagnostics::Recorder::start(
            path.into(),
            oink_foundation::DeviceId([91; 32]),
            Some(oink_foundation::GroupId([92; 16])),
            true,
        )
        .unwrap();
        while r.status().recovering {
            std::thread::sleep(Duration::from_millis(5));
        }
        r.save(crate::diagnostics::Reason::Manual).unwrap();
        println!("termination-child-ready");
        std::io::stdout().flush().unwrap();
        rt.block_on(termination.wait());
        r.shutdown();
    }

    #[test]
    fn sigterm_is_clean_but_sigkill_recovers() {
        let _scenario = crate::test_watchdog::Scenario::new(module_path!(), "sigterm_is_clean_but_sigkill_recovers");
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            let dir = tempfile::tempdir().unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["host::signal_tests::termination_child", "--exact", "--nocapture"])
                .env("OINK_TEST_TERM", dir.path())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            loop {
                assert!(out.read_line(&mut line).unwrap() > 0);
                if line.contains("termination-child-ready") {
                    break;
                }
                line.clear();
            }
            // SAFETY: signal only this test's own live child; no platform input backend or installed app.
            assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
            let until = Instant::now() + Duration::from_secs(10);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= until {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("termination timed out");
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            assert_eq!(status.success(), signal == libc::SIGTERM);
            let r = crate::diagnostics::Recorder::start(
                dir.path().into(),
                oink_foundation::DeviceId([91; 32]),
                Some(oink_foundation::GroupId([92; 16])),
                true,
            )
            .unwrap();
            while r.status().recovering {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                r.list()
                    .unwrap()
                    .iter()
                    .filter(|b| b.reason == crate::diagnostics::Reason::RecoveredCrash)
                    .count(),
                usize::from(signal == libc::SIGKILL)
            );
            r.shutdown();
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod lifecycle_tests {
    use super::*;

    struct SlowStop {
        input: oink_test_support::input::FakeInput,
        armed: std::sync::atomic::AtomicBool,
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        completed: std::sync::mpsc::Sender<()>,
    }
    impl oink_input::InputBackend for SlowStop {
        fn start(&self, handler: oink_input::EventHandler) -> Result<(), oink_input::InputError> {
            self.input.start(handler)
        }
        fn start_loop(
            &self,
            owner: Box<dyn oink_input::capture_loop::CaptureLoop>,
        ) -> Result<(), oink_input::InputError> {
            self.input.start_loop(owner)
        }
        fn stop(&self) {
            use std::sync::atomic::Ordering;
            if self.armed.swap(false, Ordering::AcqRel) {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                self.input.stop();
                self.completed.send(()).unwrap();
                return;
            }
            self.input.stop();
        }
        fn set_mode(&self, mode: oink_input::CaptureMode) -> Result<(), oink_input::InputError> {
            self.input.set_mode(mode)
        }
        fn inject(&self, event: &oink_input::InjectEvent) -> Result<(), oink_input::InputError> {
            self.input.inject(event)
        }
        fn cursor_position(&self) -> Result<oink_foundation::Point, oink_input::InputError> {
            self.input.cursor_position()
        }
        fn feel_profile(&self) -> oink_input::FeelProfile {
            self.input.feel_profile()
        }
        fn permissions(&self) -> oink_input::InputPermissions {
            self.input.permissions()
        }
        fn capture_active(&self) -> Option<bool> {
            self.input.capture_active()
        }
        fn display_power_observation(
            &self,
        ) -> Result<Box<dyn oink_input::display_power::Observation>, oink_input::InputError> {
            self.input.display_power_observation()
        }
    }

    fn slow_stop(
        armed: bool,
    ) -> (
        Arc<SlowStop>,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
        std::sync::mpsc::Receiver<()>,
    ) {
        let (entered, entering) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (completed, completing) = std::sync::mpsc::channel();
        (
            Arc::new(SlowStop {
                input: oink_test_support::input::FakeInput::new(),
                armed: std::sync::atomic::AtomicBool::new(armed),
                entered,
                release: std::sync::Mutex::new(released),
                completed,
            }),
            entering,
            release,
            completing,
        )
    }

    fn assert_returns_before_native_stop<T: Send + 'static>(
        entering: std::sync::mpsc::Receiver<()>,
        release: std::sync::mpsc::Sender<()>,
        completing: std::sync::mpsc::Receiver<()>,
        shutdown: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (done, finished) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || assert!(done.send(shutdown()).is_ok()));
        // These are deadlock watchdogs, not a latency assertion. Native stop
        // stays blocked until the host has returned, regardless of machine load.
        let entered = entering.recv_timeout(Duration::from_secs(30));
        let result = finished.recv_timeout(Duration::from_secs(30));
        release.send(()).unwrap();
        completing.recv_timeout(Duration::from_secs(30)).unwrap();
        worker.join().unwrap();
        entered.unwrap();
        result.expect("host waited for native stop instead of its cleanup deadline")
    }

    #[cfg(unix)]
    #[test]
    fn stop_rejects_unqueryable_process_ownership() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = stop_service(dir.path(), Duration::ZERO);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn process_deadline_is_not_blocked_by_a_synchronous_native_stop() {
        let _scenario = crate::test_watchdog::Scenario::new(
            module_path!(),
            "process_deadline_is_not_blocked_by_a_synchronous_native_stop",
        );
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let (input, entering, release, completing) = slow_stop(false);
        let host = AgentHost::start(
            NodeOptions {
                net: oink_test_support::network_config(dir.path().into(), "slow-stop", 0),
                input: input.clone(),
                displays: vec![],
                display_backend: None,
                clipboard: None,
                mode_switch_in_callback: true,
                capture_reports_key_repeat: true,
                cursor_trail: false,
            },
            false,
        )
        .unwrap();
        input.armed.store(true, Ordering::Release);
        assert_returns_before_native_stop(entering, release, completing, move || host.shutdown());
    }

    #[test]
    fn required_ipc_bind_failure_precedes_node_startup() {
        let _scenario =
            crate::test_watchdog::Scenario::new(module_path!(), "required_ipc_bind_failure_precedes_node_startup");
        let occupied = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = occupied.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let input = Arc::new(oink_test_support::input::FakeInput::new());
        let result = AgentHost::start_with(
            NodeOptions {
                net: oink_test_support::network_config(dir.path().into(), "ipc-required", port - 2),
                input: input.clone(),
                displays: vec![],
                display_backend: None,
                clipboard: None,
                mode_switch_in_callback: true,
                capture_reports_key_repeat: true,
                cursor_trail: false,
            },
            IpcMode::Required,
        );
        assert!(result.unwrap_err().to_string().contains("IPC"));
        assert!(input.mode().is_none());
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "Node wrote startup state"
        );
    }

    #[test]
    fn required_ipc_publication_failure_has_bounded_cleanup() {
        let _scenario =
            crate::test_watchdog::Scenario::new(module_path!(), "required_ipc_publication_failure_has_bounded_cleanup");
        let dir = tempfile::tempdir().unwrap();
        // Binding succeeds, but the endpoint cannot replace a directory.
        std::fs::create_dir(dir.path().join(ipc::IPC_FILE)).unwrap();
        let (input, entering, release, completing) = slow_stop(true);
        let options = NodeOptions {
            net: oink_test_support::network_config(dir.path().into(), "ipc-publication", 0),
            input,
            displays: vec![],
            display_backend: None,
            clipboard: None,
            mode_switch_in_callback: true,
            capture_reports_key_repeat: true,
            cursor_trail: false,
        };
        let result = assert_returns_before_native_stop(entering, release, completing, move || {
            AgentHost::start_with(options, IpcMode::Required)
        });
        assert!(result.unwrap_err().to_string().contains("IPC"));
    }

    #[test]
    fn embedded_shutdown_releases_ports_with_a_retained_node() {
        let _scenario = crate::test_watchdog::Scenario::new(
            module_path!(),
            "embedded_shutdown_releases_ports_with_a_retained_node",
        );
        let dir = tempfile::tempdir().unwrap();
        // Discovery uses QUIC's port + 1. On Windows two concurrent ephemeral
        // allocations can be adjacent, so reserve a free pair outside the dynamic
        // range for this fixture instead of letting the other host take its beacon port.
        let (interactive, beacon) = (20000..40000)
            .find_map(|port| {
                let interactive = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, port)).ok()?;
                let beacon = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, port + 1)).ok()?;
                Some((interactive, beacon))
            })
            .expect("free adjacent loopback ports");
        let port = interactive.local_addr().unwrap().port();
        let mut config = oink_test_support::network_config(dir.path().into(), "embedded", port);
        // Bind a discovery socket on loopback without any beacon targets.
        config.beacon = true;
        drop((interactive, beacon));
        let host = AgentHost::start(
            NodeOptions {
                net: config,
                input: Arc::new(oink_test_support::input::FakeInput::new()),
                displays: vec![],
                display_backend: None,
                clipboard: None,
                mode_switch_in_callback: true,
                capture_reports_key_repeat: true,
                cursor_trail: false,
            },
            false,
        )
        .unwrap();
        let retained = host.node().clone();
        let port = retained.links().unwrap().local_addr().port();
        let discovery = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port + 1));
        assert!(std::net::UdpSocket::bind(discovery).is_err());
        host.shutdown_and_wait();
        let _interactive = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
        let _discovery = std::net::UdpSocket::bind(discovery).unwrap();
        drop(retained);
    }
}
