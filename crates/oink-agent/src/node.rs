//! Backend assembly: each module owns its streams; input owns the router.
mod access;
mod appearance;
mod assembly;
mod card_projection;
mod main_commands;
mod notifications;
mod observers;
#[cfg(all(test, debug_assertions))]
mod startup_store_tests;
use crate::api::{self, ApiEvent, EventSink, SubscriptionId};
use crate::clip::ClipHandle;
use crate::config::AgentConfig;
use crate::layout_sync::{Source, Version};
use crate::threads::input::Environment;
use oink_clipboard::ClipboardBackend;
use oink_display::{DisplayBackend, layout::Layout};
use oink_foundation::{DeviceId, display::DisplayInfo, snapshot::Snapshot};
use oink_input::{InputBackend, InputPermissions, PermissionState};
use oink_network::NetworkConfig;
use oink_routing::StatusEvent;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
/// 启动一个服务进程需要的部件。
pub struct NodeOptions {
    pub net: NetworkConfig,
    pub input: Arc<dyn InputBackend>,
    pub displays: Vec<DisplayInfo>,
    pub display_backend: Option<Arc<dyn DisplayBackend>>,
    pub clipboard: Option<Arc<dyn ClipboardBackend>>,
    /// Apply callback-originated modes before capture returns when no native
    /// mode call is already in flight; other modes retain injection queue order.
    pub mode_switch_in_callback: bool,
    pub capture_reports_key_repeat: bool,
    /// 在本机屏幕上绘制光标定位拖尾（平台实现才有，测试用 `false`）。
    pub cursor_trail: bool,
}

impl std::fmt::Debug for NodeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeOptions")
            .field("net", &self.net)
            .finish_non_exhaustive()
    }
}

impl NodeOptions {
    /// 本机平台的输入、显示器、剪贴板 backend（`OINK_PASTEBOARD` 可让 macOS 改用具名剪贴板）。
    pub fn platform(net: NetworkConfig) -> anyhow::Result<Self> {
        #[cfg(windows)]
        anyhow::ensure!(
            oink_winsvc::backend::uiaccess()?,
            "Windows 后台需要 OinkInput 系统服务提供 UIAccess；请通过已安装的 Oink 打开"
        );
        let input = oink_input::platform_backend().map_err(|e| anyhow::anyhow!("输入 backend：{e}"))?;
        let display = oink_display::platform_backend().map_err(|e| anyhow::anyhow!("显示器 backend：{e}"))?;
        let displays = display.displays().map_err(|e| anyhow::anyhow!("枚举显示器：{e}"))?;
        let clipboard = match oink_clipboard::platform_backend() {
            Ok(c) => Some(Arc::from(c)),
            Err(e) => {
                tracing::warn!(%e, "剪贴板 backend 不可用");
                None
            }
        };
        Ok(Self {
            net,
            input: Arc::from(input),
            displays,
            display_backend: Some(Arc::from(display)),
            clipboard,
            // macOS、Windows backend 都把系统的自动重复作为再一次按下上报（重复只在源机产生），
            // 都支持在截获回调里切换模式。
            mode_switch_in_callback: true,
            capture_reports_key_repeat: true,
            cursor_trail: true,
        })
    }
}
pub(crate) struct Inner {
    pub(crate) reports: crate::telemetry::Owner,

    pub(crate) update: Mutex<crate::update::Controller>,
    pub(crate) links: OnceLock<oink_network::link::LinkLayer>,
    pub(crate) group: OnceLock<crate::config_module::ConfigModule>,
    pub(crate) input: OnceLock<Arc<crate::threads::input::InputThread>>,
    pub(crate) input_backend: Arc<dyn InputBackend>,
    identity: Arc<oink_network::identity::Identity>,
    pub(crate) boot: oink_foundation::session::Boot,
    network: NetworkConfig,
    startup: tokio::sync::Mutex<()>,
    pub(crate) group_unreadable: Mutex<Option<String>>,
    clipboard_backend: Option<Arc<dyn ClipboardBackend>>,
    pub(crate) display_backend: Option<Arc<dyn DisplayBackend>>,
    initial_displays: Vec<DisplayInfo>,
    pub(crate) input_settings: Arc<Snapshot<oink_routing::RoutingSettings>>,
    input_environment: Arc<Snapshot<Environment>>,
    cursor_trail: OnceLock<oink_locator::CursorTrail>,
    notifications: Mutex<notifications::Notifications>,
    native_surfaces: bool,
    system_dark: AtomicBool,
    pub(crate) native_services: bool,
    mode_switch_in_callback: bool,
    capture_reports_key_repeat: bool,
    ordinary_threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    main_commands: OnceLock<crate::threads::MainSender>,
    pub(crate) audio: OnceLock<crate::audio::LinkHandle>,
    pub(crate) dnd: OnceLock<crate::dnd::LinkHandle>,
    pub(crate) input_trace: OnceLock<Arc<crate::diagnostics::input_trace::InputTrace>>,
    pub(crate) diagnostics: OnceLock<crate::diagnostics::collection::Collector>,
    pub clip: std::sync::OnceLock<ClipHandle>,
    pub(crate) cache: crate::cache::State,
    #[cfg(target_os = "macos")]
    pub(crate) file_provider: Mutex<crate::file_provider::State>,
    #[cfg(target_os = "macos")]
    pub(crate) file_provider_wake: Arc<tokio::sync::Notify>,
    #[cfg(target_os = "macos")]
    pub(crate) file_provider_safety_wake: Arc<tokio::sync::Notify>,
    pub me: DeviceId,
    pub data_dir: PathBuf,
    pub config: Arc<Mutex<AgentConfig>>,
    /// Serializes durable owner edits through native apply and Facts publication.
    pub(crate) owner_gate: Arc<Mutex<()>>,
    pub(crate) cards: Mutex<crate::device_cards::Projector>,
    pub(crate) controls: Mutex<crate::device_controls::runtime::State>,
    pub(crate) profiles: crate::layout_profiles::state::State,
    input_observation: Mutex<Option<access::InputObservation>>,

    sinks: Mutex<Vec<crate::event_delivery::Subscriber>>,
    next_sink: AtomicU64,
    last_group: Mutex<Option<api::GroupView>>,
    last_overview: Mutex<Option<api::Overview>>,
    last_audio: Mutex<Option<oink_audio::Status>>,
    last_pause: Mutex<Option<api::PauseStatus>>,
    last_diagnostics: Mutex<Option<api::DiagnosticRecorderStatus>>,
    last_launch: Mutex<Option<api::LaunchAtLoginStatus>>,
    last_permissions: Mutex<Option<Vec<api::PermissionItem>>>,
    permission_refresh: AtomicBool,
    #[cfg(windows)]
    pub(crate) windows_access: Snapshot<crate::windows_permissions::Observation>,
    last_layout: Mutex<Option<api::Layout>>,
    last_input: Mutex<Option<api::InputSettings>>,
    pub(crate) current_alerts: Mutex<Vec<api::Alert>>,
    pub(crate) quality: Mutex<crate::connection_quality::History>,
    pub(crate) audio_quality: Mutex<crate::audio_quality::History>,
    pub started: Instant,
    #[cfg(all(test, debug_assertions))]
    pub platform_ready: AtomicBool,
    #[cfg(test)]
    platform_hold: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    #[cfg(test)]
    platform_clock_held: AtomicBool,
    pub rt: tokio::runtime::Handle,
    pub overlay: Mutex<Option<api::Overlay>>,
    pub overlay_wake: Arc<tokio::sync::Notify>,
    /// 瞄准小游戏。
    pub(crate) aim: crate::aim::Aim,
    pub(crate) projection: crate::projection::State,
    /// 桌面壁纸缩略图。
    pub(crate) wallpaper: crate::wallpaper::State,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    log_listener: Mutex<Option<u64>>,
    stop: tokio::sync::watch::Sender<bool>,
}

#[derive(Clone)]
pub struct Node {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("me", &self.inner.me)
            .finish_non_exhaustive()
    }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn sharing_not_started() -> api::ApiError {
    api::ApiError::Unavailable {
        message: "共享尚未启动".into(),
    }
}

fn replace_changed<T: Clone + PartialEq>(last: &Mutex<Option<T>>, value: &T) -> bool {
    let mut last = lock(last);
    if last.as_ref() == Some(value) {
        return false;
    }
    *last = Some(value.clone());
    true
}

/// 这项权限是否已授予（或本系统不需要）。
pub(crate) fn granted(s: PermissionState) -> bool {
    matches!(s, PermissionState::Granted | PermissionState::NotApplicable)
}

/// 「辅助功能」与「输入监控」是否都已授予（或本系统不需要）。
pub(crate) fn capture_permitted(p: &InputPermissions) -> bool {
    granted(p.accessibility) && granted(p.input_monitoring)
}

/// Both input rights are required: losing local capture also withdraws remote control.
pub(crate) fn inject_blocked(p: &InputPermissions) -> bool {
    !capture_permitted(p)
}
impl Node {
    pub(crate) fn owner_gate(&self) -> MutexGuard<'_, ()> {
        lock(&self.inner.owner_gate)
    }

    pub async fn start(opts: NodeOptions) -> anyhow::Result<Self> {
        Self::start_with_services(opts, !cfg!(test)).await
    }

    #[cfg(target_os = "macos")]
    pub(crate) async fn start_launch_smoke(opts: NodeOptions) -> anyhow::Result<Self> {
        // Keep native trail/appearance startup, but never request native audio
        // recovery, File Provider registration, or desktop capture in packaging.
        Self::start_with_services(opts, false).await
    }

    /// Run the real application owners against injected fake platform backends.
    /// Native audio recovery, power monitoring and File Provider registration are
    /// disabled explicitly; this development entry point is absent in releases.
    #[cfg(debug_assertions)]
    pub async fn start_with_fake_platform(mut opts: NodeOptions) -> anyhow::Result<Self> {
        opts.cursor_trail = false;
        Self::start_with_services(opts, false).await
    }

    pub(crate) fn native_services_enabled(&self) -> bool {
        self.inner.native_services
    }

    async fn start_with_services(opts: NodeOptions, native_services: bool) -> anyhow::Result<Self> {
        let node = Self::prepare_with_services(opts, native_services)?;
        if lock(&node.inner.group_unreadable).is_none() {
            node.start_sharing().await?;
        }
        Ok(node)
    }

    // Assemble snapshots before starting background owners. Tests can drive the
    // same publication path with explicit source clocks and fake platform I/O.
    #[cfg(test)]
    pub(crate) fn prepare(opts: NodeOptions) -> anyhow::Result<Self> {
        Self::prepare_with_services(opts, false)
    }

    fn prepare_with_services(mut opts: NodeOptions, native_services: bool) -> anyhow::Result<Self> {
        let data_dir = opts.net.data_dir.clone();
        let mut config = AgentConfig::load_from_with_device_name(&data_dir, opts.net.device_name.clone())?;
        if let Some(name) = &config.settings.device_name {
            opts.net.device_name = name.clone();
        }
        let identity = Arc::new(oink_network::identity::Identity::load_or_create(&data_dir)?);
        tracing::info!("backend identity and settings ready");
        let me = identity.id();
        let boot = oink_foundation::session::Boot {
            device_id: me,
            nonce: {
                let mut nonce = [0; 16];
                getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!(e.to_string()))?;
                nonce
            },
        };
        // This new backend Boot releases both previous input disables before
        // capture or any local owner facts can be published.
        config.update_owner(|owner| owner.clear_input_disables_on_start())?;
        let unreadable =
            crate::config_module::group_store::load(&data_dir.join(crate::config_module::group_store::STATE_FILE), me)
                .err()
                .map(|error| error.to_string());
        let settings = config.settings.clone();
        if native_services {
            oink_clipboard::received::configure(settings.received_files_cleanup);
        }
        let native = opts.cursor_trail;
        let node = Self {
            inner: Arc::new(Inner {
                reports: crate::telemetry::Owner::new(&data_dir, config.settings.reports),
                links: OnceLock::new(),
                group: OnceLock::new(),
                input: OnceLock::new(),
                audio: OnceLock::new(),
                dnd: OnceLock::new(),
                input_trace: OnceLock::new(),
                diagnostics: OnceLock::new(),
                clip: OnceLock::new(),
                cache: crate::cache::State::new(native),
                input_backend: opts.input,
                identity,
                boot,
                network: opts.net,
                startup: tokio::sync::Mutex::new(()),
                group_unreadable: Mutex::new(unreadable),
                clipboard_backend: opts.clipboard,
                display_backend: opts.display_backend.clone(),
                initial_displays: opts.displays,
                input_settings: Arc::new(Snapshot::new(settings.routing)),
                input_environment: Arc::new(Snapshot::default()),
                cursor_trail: OnceLock::new(),
                notifications: Mutex::default(),
                system_dark: AtomicBool::new(false),
                native_surfaces: native,
                native_services,
                mode_switch_in_callback: opts.mode_switch_in_callback,
                capture_reports_key_repeat: opts.capture_reports_key_repeat,
                ordinary_threads: Mutex::new(Vec::new()),
                main_commands: OnceLock::new(),
                #[cfg(target_os = "macos")]
                file_provider: Mutex::new(crate::file_provider::State::new(&data_dir)),
                #[cfg(target_os = "macos")]
                file_provider_wake: Arc::new(tokio::sync::Notify::new()),
                #[cfg(target_os = "macos")]
                file_provider_safety_wake: Arc::new(tokio::sync::Notify::new()),
                me,
                update: Mutex::new(crate::update::Controller::load(&data_dir)),
                data_dir,
                config: Arc::new(Mutex::new(config)),
                owner_gate: Arc::new(Mutex::new(())),
                cards: Mutex::new(
                    crate::device_cards::Projector::new().map_err(|error| anyhow::anyhow!(error.to_string()))?,
                ),
                controls: Mutex::default(),
                profiles: Default::default(),
                input_observation: Mutex::default(),
                sinks: Mutex::new(Vec::new()),
                next_sink: AtomicU64::new(0),
                last_group: Mutex::new(None),
                last_overview: Mutex::new(None),
                last_audio: Mutex::new(None),
                last_pause: Mutex::new(None),
                last_diagnostics: Mutex::new(None),
                last_launch: Mutex::new(None),
                last_permissions: Mutex::new(None),
                permission_refresh: AtomicBool::new(false),
                #[cfg(windows)]
                windows_access: Snapshot::new(crate::windows_permissions::Observation::UNKNOWN),
                last_layout: Mutex::new(None),
                last_input: Mutex::new(None),
                current_alerts: Mutex::new(Vec::new()),
                quality: Mutex::default(),
                audio_quality: Mutex::default(),
                started: Instant::now(),
                #[cfg(all(test, debug_assertions))]
                platform_ready: AtomicBool::new(false),
                #[cfg(test)]
                platform_hold: Mutex::new(None),
                #[cfg(test)]
                platform_clock_held: AtomicBool::new(false),
                rt: tokio::runtime::Handle::current(),
                overlay: Mutex::new(None),
                overlay_wake: Arc::default(),
                aim: crate::aim::Aim::default(),
                projection: crate::projection::State::default(),
                wallpaper: crate::wallpaper::State::new(opts.display_backend),
                tasks: Mutex::new(Vec::new()),
                log_listener: Mutex::new(None),
                stop: tokio::sync::watch::channel(false).0,
            }),
        };
        Ok(node)
    }
    pub fn id(&self) -> DeviceId {
        self.inner.me
    }

    /// 接上瞄准小游戏的全屏叠加层（平台实现在启动时已接上；测试接假的）。`None`：本机不参加小游戏。
    pub fn set_aim_surface(&self, surface: Option<Arc<dyn oink_locator::aim::AimSurface>>) {
        crate::aim::set_surface(self, surface);
    }

    /// 瞄准小游戏的规则参数（下一局起生效）。测试用它把一局缩短；平时是默认值。
    pub fn set_aim_config(&self, config: oink_locator::aim::game::Config) {
        crate::aim::set_config(self, config);
    }

    /// Control Aim rule time in integration fixtures; transport timers remain real.
    /// Install once before starting a game, then advance the clock monotonically.
    #[cfg(debug_assertions)]
    pub fn set_aim_clock_for_test(&self, clock: Arc<std::sync::atomic::AtomicU64>) {
        self.inner
            .aim
            .test_clock
            .set(clock)
            .expect("Aim test clock already installed");
    }

    pub fn start_platform_dnd(&self) {
        let _runtime = self.inner.rt.enter();
        if let Some(handle) = self.inner.dnd.get() {
            match oink_clipboard::dnd::platform_dnd() {
                Ok(backend) => {
                    if let Err(error) = handle.start_backend(self.clone(), backend, self.inner.input_backend.clone()) {
                        tracing::warn!(%error, "drag backend failed");
                    }
                }
                Err(error) => tracing::info!(%error, "drag and drop unavailable"),
            }
        }
    }
    /// Attach a drag backend after its event loop is ready; also used by headless scenarios.
    pub fn start_dnd_backend(&self, backend: Box<dyn oink_clipboard::dnd::DndBackend>) -> anyhow::Result<()> {
        let _runtime = self.inner.rt.enter();
        let handle = self
            .inner
            .dnd
            .get()
            .ok_or_else(|| anyhow::anyhow!("DnD runtime unavailable"))?;
        handle.start_backend(self.clone(), backend, self.inner.input_backend.clone())
    }
    pub fn clipboard(&self) -> Option<&ClipHandle> {
        self.inner.clip.get()
    }
    pub fn data_dir(&self) -> &std::path::Path {
        &self.inner.data_dir
    }

    pub fn log_dir(&self) -> PathBuf {
        let paths = oink_foundation::paths::StoragePaths::current();
        if self.data_dir() == paths.data {
            paths.logs
        } else {
            oink_foundation::paths::StoragePaths::isolated(self.data_dir().into()).logs
        }
    }

    /// Cache roots follow platform defaults while explicit test instances stay isolated.
    pub fn cache_dir(&self) -> PathBuf {
        let paths = oink_foundation::paths::StoragePaths::current();
        if paths.data == self.inner.data_dir {
            paths.cache
        } else {
            oink_foundation::paths::StoragePaths::isolated(self.inner.data_dir.clone()).cache
        }
    }

    /// The new process components are absent while sharing has not started. Business
    /// tasks obtain these handles at their startup, never by reading the old facade.
    pub fn links(&self) -> api::ApiResult<oink_network::link::LinkLayer> {
        self.inner.links.get().cloned().ok_or_else(sharing_not_started)
    }

    pub fn link_config(&self) -> api::ApiResult<crate::config_module::ConfigModule> {
        self.inner.group.get().cloned().ok_or(api::ApiError::NetworkStopped)
    }

    pub fn input_thread(&self) -> api::ApiResult<Arc<crate::threads::input::InputThread>> {
        self.inner.input.get().cloned().ok_or_else(sharing_not_started)
    }

    pub fn input_status(&self) -> api::ApiResult<Arc<crate::threads::input::InputStatus>> {
        Ok(self.input_thread()?.status())
    }

    /// Ordinary observers query the injected platform backend without borrowing the router.
    pub fn capture_active(&self) -> Option<bool> {
        self.inner.input_backend.capture_active()
    }

    pub(crate) fn input_backend(&self) -> &Arc<dyn InputBackend> {
        &self.inner.input_backend
    }

    pub fn stop_receiver(&self) -> tokio::sync::watch::Receiver<bool> {
        self.inner.stop.subscribe()
    }

    pub fn now(&self) -> oink_routing::Micros {
        self.inner.started.elapsed().as_micros() as oink_routing::Micros
    }

    /// 请求宿主优雅退出（IPC stop 使用，先应答再通知宿主）。
    pub fn request_stop(&self) {
        self.inner.stop.send_replace(true);
        if let Some(main) = self.inner.main_commands.get() {
            let _ = main.send(crate::threads::MainCommand::Wake);
        }
    }

    pub async fn wait_for_stop(&self) {
        let mut rx = self.stop_receiver();
        let _ = rx.wait_for(|stop| *stop).await;
    }

    pub async fn shutdown(&self) {
        self.shutdown_for_process_exit().await;
    }
    pub(crate) async fn shutdown_for_process_exit(&self) {
        self.inner.reports.shutdown();
        self.request_stop();
        self.shutdown_notifications();
        crate::display_overlay::hide(self);
        crate::aim::stop(self);
        // The ordinary command owner cancels native DnD before closing its queue.
        // Keep capture/injection and the application event loop alive until then.
        let threads = std::mem::take(&mut *lock(&self.inner.ordinary_threads));
        let _ = tokio::task::spawn_blocking(move || {
            for thread in threads {
                // Stop wakes an observer even when its sampling clock is held.
                thread.thread().unpark();
                let _ = thread.join();
            }
        })
        .await;
        if let Some(input) = self.inner.input.get().cloned() {
            let _ = tokio::task::spawn_blocking(move || input.stop()).await;
        }
        self.drain_input_trace();
        if let Some(audio) = self.inner.audio.get() {
            audio.stop();
        }
        if let Some(clip) = self.clipboard() {
            clip.shutdown().await;
        }
        if let Some(group) = self.inner.group.get() {
            group.stop().await;
        }
        if let Some(links) = self.inner.links.get() {
            links.stop().await;
        }
        let tasks = std::mem::take(&mut *lock(&self.inner.tasks));
        for task in tasks {
            let _ = task.await;
        }
        // All owner tasks have stopped, so no slider edit can race the final
        // flush. This covers ordinary quit and backend restart as well as update.
        if let Err(error) = lock(&self.inner.config).flush_deferred() {
            tracing::error!(%error, "pending settings could not be flushed before process exit");
        }
        if let Some(d) = self.inner.diagnostics.get() {
            let recorder = d.recorder.clone();
            let _ = tokio::task::spawn_blocking(move || recorder.shutdown()).await;
        }
        if let Some(id) = lock(&self.inner.log_listener).take() {
            crate::logs::unlisten(id);
        }
    }
    pub fn subscribe(&self, sink: EventSink) -> SubscriptionId {
        self.subscribe_views(sink, api::LiveViews::ALL)
    }

    pub(crate) fn subscribe_views(&self, sink: EventSink, views: api::LiveViews) -> SubscriptionId {
        let id = self.inner.next_sink.fetch_add(1, Ordering::AcqRel) + 1;
        lock(&self.inner.sinks).push(crate::event_delivery::Subscriber::new(id, sink.clone(), views));
        let mut preferences = api::UiPreferences::from(&self.settings());
        loop {
            sink(&ApiEvent::UiPreferences(preferences.clone()));
            let current = api::UiPreferences::from(&self.settings());
            if current == preferences {
                break;
            }
            preferences = current;
        }
        sink(&ApiEvent::Pause(self.pause_status()));
        if let Some(audio) = self.inner.audio.get() {
            let event = ApiEvent::Audio(audio.status());
            // Prime the subscriber's comparator with its initial snapshot too.
            // A later quality-only update must not act like a second first value.
            let accepts = lock(&self.inner.sinks)
                .iter_mut()
                .find(|s| s.id == id)
                .is_some_and(|s| s.accepts(&event));
            if accepts {
                sink(&event);
            }
        }
        let mut game = self.game_status();
        loop {
            sink(&ApiEvent::GameMode(game.clone().into()));
            // A change notification can reach this sink between the read and
            // snapshot delivery. Finish initialization with the current value
            // so an older snapshot cannot leave a late subscriber stale.
            let current = self.game_status();
            if current == game {
                break;
            }
            game = current;
        }
        if let Some(d) = self.inner.diagnostics.get() {
            let mut changes = d.recorder.status_changes();
            loop {
                changes.borrow_and_update();
                let event = ApiEvent::DiagnosticsStatus(d.recorder.status());
                let accepts = lock(&self.inner.sinks)
                    .iter_mut()
                    .find(|s| s.id == id)
                    .is_some_and(|s| s.accepts(&event));
                if accepts {
                    sink(&event);
                }
                if !changes.has_changed().unwrap_or(false) {
                    break;
                }
            }
        }
        // Initialize view comparisons at subscription, rather than letting a
        // future quality/recovery report appear to be a first state change.
        self.emit_layout();
        self.emit_group();
        // Global state deduplication may already have published this group to
        // a previous subscriber. Prime this subscriber independently too.
        if let Ok(group) = crate::agent_api::group_view(self) {
            self.deliver_initial(id, ApiEvent::Group(group));
        }
        // A late subscriber needs a permission snapshot even if the global
        // comparator already published this value before it subscribed.
        self.deliver_initial(id, ApiEvent::Permissions(crate::agent_api::permission_items(self)));
        if let Ok(group) = self.link_config() {
            self.deliver_initial(id, ApiEvent::NearbySnapshot((*group.nearby()).clone()));
            if let Ok(snapshot) = group.blocked_devices() {
                self.deliver_initial(id, ApiEvent::BlockedDevices { snapshot });
            }
        }
        if let Ok(snapshot) = self.device_cards_snapshot() {
            self.deliver_initial(id, ApiEvent::DeviceCards(snapshot));
        }
        self.deliver_initial(id, ApiEvent::ReportStatusChanged(self.inner.reports.status()));
        SubscriptionId(id)
    }

    fn deliver_initial(&self, id: u64, event: ApiEvent) {
        let sink = lock(&self.inner.sinks)
            .iter_mut()
            .find(|subscriber| subscriber.id == id)
            .and_then(|subscriber| subscriber.accepts(&event).then(|| subscriber.sink.clone()));
        if let Some(sink) = sink {
            sink(&event);
        }
    }

    pub fn unsubscribe(&self, id: SubscriptionId) {
        lock(&self.inner.sinks).retain(|s| s.id != id.0);
    }

    pub(crate) fn has_stats_subscribers(&self) -> bool {
        lock(&self.inner.sinks)
            .iter()
            .any(|s| s.views.stats || s.views.group_quality)
    }

    pub(crate) fn set_live_views(&self, id: SubscriptionId, views: api::LiveViews) {
        let mut sinks = lock(&self.inner.sinks);
        if let Some(s) = sinks.iter_mut().find(|s| s.id == id.0) {
            s.views = views;
        }
        drop(sinks);
        // Opening a view receives a current snapshot even if another subscriber
        // has already seen the same value. It never waits for a periodic change.
        let mut events = Vec::new();
        if views.stats || views.group_quality {
            events.push(ApiEvent::Stats(crate::agent_api::peer_statuses(self)));
        }
        if views.audio_quality
            && let Some(audio) = self.inner.audio.get()
        {
            events.push(ApiEvent::AudioStats((&audio.status()).into()));
        }
        if views.diagnostics
            && let Some(d) = self.inner.diagnostics.get()
        {
            events.push(ApiEvent::DiagnosticsStats((&d.recorder.status()).into()));
        }
        for event in events {
            let sink = lock(&self.inner.sinks)
                .iter_mut()
                .find(|s| s.id == id.0)
                .and_then(|s| s.accepts(&event).then(|| s.sink.clone()));
            if let Some(sink) = sink {
                sink(&event);
            }
        }
    }

    /// 推给所有订阅者（不在持有内部锁时调用）。
    pub fn emit(&self, ev: ApiEvent) {
        // A summary push means its content changed, regardless of which caller
        // requested the refresh. Queries/initial view loading read live state.
        let changed = match &ev {
            ApiEvent::Group(g) => replace_changed(&self.inner.last_group, &crate::event_delivery::group_state(g)),
            ApiEvent::Overview(o) => replace_changed(&self.inner.last_overview, o),
            ApiEvent::Audio(a) => replace_changed(&self.inner.last_audio, &crate::event_delivery::audio_state(a)),
            ApiEvent::Pause(p) => replace_changed(&self.inner.last_pause, p),
            ApiEvent::LaunchAtLogin(s) => replace_changed(&self.inner.last_launch, s),
            ApiEvent::DiagnosticsStatus(s) => replace_changed(
                &self.inner.last_diagnostics,
                &crate::event_delivery::diagnostic_state(s),
            ),
            ApiEvent::Permissions(p) => replace_changed(&self.inner.last_permissions, p),
            ApiEvent::Layout(l) => replace_changed(&self.inner.last_layout, l),
            ApiEvent::InputSettings(i) => replace_changed(&self.inner.last_input, i),
            _ => true,
        };
        if !changed {
            return;
        }
        if let Some(d) = self.inner.diagnostics.get() {
            crate::diagnostics::api_event(&d.recorder, &ev);
        }
        let sinks: Vec<EventSink> = lock(&self.inner.sinks)
            .iter_mut()
            .filter_map(|s| s.accepts(&ev).then(|| s.sink.clone()))
            .collect();
        for s in sinks {
            s(&ev);
        }
    }

    pub(crate) fn emit_group(&self) {
        self.refresh_device_cards();
        if lock(&self.inner.sinks).is_empty() {
            return;
        }
        if let Ok(g) = crate::agent_api::group_view(self) {
            self.emit(ApiEvent::Group(g));
        }
        if let Ok(o) = crate::agent_api::overview(self) {
            self.emit(ApiEvent::Overview(o));
        }
        #[cfg(windows)]
        // Include changes in authenticated inbound TCP evidence immediately.
        // Installer firewall/service observations also refresh independently
        // through the ordinary platform observer.
        self.emit(ApiEvent::Permissions(crate::agent_api::permission_items(self)));
    }

    pub(crate) fn update_settings(&self, f: impl FnOnce(&mut crate::config::Settings)) -> anyhow::Result<()> {
        let mut c = lock(&self.inner.config);
        let previous_log_max_mb = c.settings.log_max_mb;
        c.update(f)?;
        if c.settings.log_max_mb != previous_log_max_mb
            && let Err(error) = crate::logs::set_max_mb(u64::from(c.settings.log_max_mb.get()))
        {
            tracing::warn!(kind = ?error.kind(), "ordinary log cleanup failed");
        }
        Ok(())
    }

    pub(crate) fn settings(&self) -> crate::config::Settings {
        lock(&self.inner.config).settings.clone()
    }

    pub fn device_name(&self, device: DeviceId) -> String {
        if device == self.id() {
            return self
                .settings()
                .device_name
                .unwrap_or_else(|| self.inner.network.device_name.clone());
        }
        self.inner
            .group
            .get()
            .and_then(|group| {
                group
                    .members()
                    .members
                    .iter()
                    .find(|m| m.device == device)
                    .map(|m| m.name.clone())
            })
            .unwrap_or_else(|| device.short())
    }
    pub fn focus(&self) -> Option<api::Focus> {
        let status = self.input_status().ok()?;
        let focus = status.focus_info.as_ref()?;
        self.focus_view(focus)
    }
    pub(crate) fn focus_view(&self, focus: &oink_routing::FocusInfo) -> Option<api::Focus> {
        let display = focus.pointer.as_ref().map(|p| p.screen.display.clone());
        let display_name = self.inner.group.get().and_then(|group| {
            group
                .local_displays()
                .displays
                .iter()
                .find(|d| Some(&d.key) == display.as_ref())
                .map(|d| d.name.clone())
        });
        Some(api::Focus {
            device: focus.holder.into(),
            device_name: self.device_name(focus.holder),
            display,
            display_name,
            is_local: focus.holder == self.id(),
        })
    }
    pub(crate) fn layout_owners(
        &self,
    ) -> std::sync::Arc<std::collections::BTreeMap<DeviceId, oink_display::layout::OwnerLayoutPreferences>> {
        self.inner
            .group
            .get()
            .map(|group| group.applied().owners.clone())
            .unwrap_or_default()
    }
    pub fn layout(&self) -> (Layout, Version, bool) {
        let applied = self.inner.group.get().map(|g| g.applied());
        let version = Version {
            revision: applied
                .as_ref()
                .map_or(0, |a| a.version.counter().expect("validated config version")),
            source: applied.as_ref().map_or(Source::Default, |a| {
                Source::try_from(a.layout_source).expect("validated applied layout source")
            }),
        };
        (
            applied.map_or_else(Layout::default, |a| (*a.layout).clone()),
            version,
            false,
        )
    }
    pub fn online_set(&self) -> std::collections::BTreeSet<DeviceId> {
        self.input_status()
            .map(|s| {
                s.peers
                    .iter()
                    .filter(|p| p.attached && p.online)
                    .map(|p| p.peer)
                    .chain(std::iter::once(self.id()))
                    .collect()
            })
            .unwrap_or_default()
    }
    pub(crate) async fn edit_layout(&self, source: Source, edit: impl FnOnce(&mut Layout)) -> Result<(), String> {
        if source == Source::Manual {
            return self
                .commit_manual_layout(|layout| {
                    edit(layout);
                    Ok(())
                })
                .map_err(|e| e.to_string());
        }
        let group = self.link_config().map_err(|e| e.to_string())?;
        let mut layout = (*group.applied().layout).clone();
        edit(&mut layout);
        group
            .edit_layout(Box::new(move |current| *current = layout), source.into())
            .map_err(|e| e.to_string())
    }
    pub(crate) async fn try_edit_layout(
        &self,
        edit: impl FnOnce(&mut Layout) -> api::ApiResult<()>,
    ) -> api::ApiResult<()> {
        self.try_edit_layout_with_source(Source::Manual, edit).await
    }
    pub(crate) async fn try_edit_layout_with_source(
        &self,
        source: Source,
        edit: impl FnOnce(&mut Layout) -> api::ApiResult<()>,
    ) -> api::ApiResult<()> {
        if source == Source::Manual {
            return self.commit_manual_layout(edit);
        }
        let group = self.link_config()?;
        let mut layout = (*group.applied().layout).clone();
        edit(&mut layout)?;
        group.edit_layout(Box::new(move |current| *current = layout), source.into())
    }
    pub(crate) fn emit_layout(&self) {
        self.refresh_layout_profiles();
        if lock(&self.inner.sinks).is_empty() {
            return;
        }
        if let Ok(layout) = crate::agent_api::layout_api(self) {
            self.emit(ApiEvent::Layout(layout));
        }
        self.emit(ApiEvent::InputSettings(crate::agent_api::input_settings(self)));
    }
}

impl Node {
    /// Re-check the system clipboard at explicit clear, including a local copy
    /// not yet consumed by the clipboard owner.
    pub(crate) fn cache_clipboard_paths(&self) -> Result<Vec<PathBuf>, ()> {
        let Some(backend) = self.inner.clipboard_backend.as_ref() else {
            return Ok(Vec::new());
        };
        let snapshot = backend.read().map_err(|_| ())?;
        Ok(snapshot
            .items
            .into_iter()
            .filter_map(|item| match item {
                oink_clipboard::ClipData::Files(entries) => Some(entries),
                _ => None,
            })
            .flatten()
            .filter_map(|e| e.local_path)
            .collect())
    }
}
