//! Product command line. Calls use the same authenticated IPC as the UI.

mod cli_output;

#[cfg(windows)]
mod windows_service_test;

#[cfg(windows)]
mod update_admin;

use anyhow::Context;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use oink_agent::api::{self, Placement, Request};
use oink_agent::config::AgentConfig;
#[cfg(not(windows))]
use oink_agent::host::AgentHost;
use oink_agent::ipc::Client;
use oink_display::layout::Layout;
use oink_foundation::{DeviceId, Point, Rect};

#[derive(Parser, Debug)]
#[command(name = "oink-agent", version = env!("OINK_RELEASE_VERSION"), about = "Oink 服务进程（无界面）")]
struct Cli {
    /// 数据目录（默认按系统；也可用环境变量 OINK_HOME）。
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum DiagnosticsCmd {
    /// Save the last 30 minutes (default), a chosen range, or all retained records.
    Save {
        #[arg(long, value_name = "minutes", value_parser = clap::value_parser!(u32).range(1..))]
        last: Option<u32>,
        #[arg(long, conflicts_with = "last")]
        all: bool,
    },
    /// Show this device's local recorder and recovery status.
    Status,
    /// Enable recording on this device; no continuous network transfer.
    On,
    /// Disable recording on this device and erase its buffer; keep saved zips.
    Off,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Internal continuation; requires the installed elevated executable on Windows.
    #[cfg(windows)]
    #[command(hide = true)]
    ApproveLocalUpdate { path: String, sha256: String },
    #[cfg(target_os = "macos")]
    #[command(hide = true)]
    CompleteLocalUpdate {
        stage: PathBuf,
        destination: PathBuf,
        parent: u32,
        socket: PathBuf,
    },
    /// Internal continuation; UAC authorizes only a fixed microphone action.
    #[cfg(windows)]
    #[command(hide = true)]
    CompleteMicrophoneInstallation { action: String, token: String },
    /// Set machine update policy from this already-elevated installed CLI.
    #[cfg(windows)]
    UpdatePolicy(update_admin::Args),
    /// 按方法名和 JSON 参数调用任意后台接口。
    Call { method: String, params: Option<String> },
    /// 订阅后台事件，每行输出一条 JSON，后台退出后结束。
    Events,
    /// 退出全部应用进程后重新打开后台和界面。
    Reopen,
    /// Save a group-wide privacy-filtered diagnostic zip, list bundles or control recording.
    Diagnostics {
        #[command(subcommand)]
        cmd: DiagnosticsCmd,
    },
    /// Audio group outputs/status, or apply a settings JSON file.
    Audio {
        #[arg(long)]
        settings: Option<PathBuf>,
    },
    /// Show or set the local source pointer-motion budget (default: hz125).
    MotionFrequency {
        #[arg(value_enum)]
        frequency: Option<MotionFrequency>,
    },
    /// Show or set experimental group application focus (default: off; CLI only).
    ApplicationFocus {
        #[arg(value_enum)]
        state: Option<OnOff>,
    },
    /// Show or set native contact acceptance on this device (default: on).
    AcceptContacts {
        #[arg(value_enum)]
        state: Option<OnOff>,
    },
    /// Show or set the local renderer cursor trail lifetime in ms (100–60000; default: 270).
    TrailDuration {
        #[arg(value_parser = clap::value_parser!(u32).range(100..=60_000))]
        ms: Option<u32>,
    },
    /// Read-only Windows input service status.
    #[cfg(windows)]
    InputService {
        #[command(subcommand)]
        cmd: windows_service_test::Command,
    },
    /// 前台运行服务进程（组网、键鼠、剪贴板、布局）。Ctrl-C / SIGTERM 退出。
    Run,
    /// 配置文件。
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// 列出本机显示器（JSON）。
    Displays,
    /// 布局：`show`（正在运行的服务的布局）、`auto`（按本机显示器自动排列）、`place`（摆放设备）。
    Layout {
        #[command(subcommand)]
        cmd: LayoutCmd,
    },
    /// 总览：本机、设备组、共享状态、焦点、提醒。
    Status,
    /// Quit Oink and wait for its backend, UI windows and panels to exit.
    Stop,
    /// 本组成员与附近逐设备列表；独立设备的 group 为 null。
    Devices,
    /// Start an independent pairing object for a nearby device.
    ConnectDevice { device: String },
    /// Show all pairing sessions and pending requests.
    Pairing,
    /// 提交对方屏幕上的六位码；pair 为完整配对 ID 或当前对方的名称。
    Code {
        pair: String,
        expected_revision: u64,
        attempt: u8,
        code: String,
    },
    /// 取消当前配对；已提交成员身份时必须明确 leave。
    CancelPairing { pair: String, expected_revision: u64 },
    /// Query a literal IP address; completion arrives through the event stream.
    QueryAddress {
        ip: String,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long, default_value_t = 1)]
        generation: u64,
    },
    /// 离开当前设备组。
    Leave,
    /// 本机 IP 与端口。
    Addresses,
    /// 共享键鼠与剪贴板：on / off。
    Sharing { state: OnOff },
    /// 锁定当前焦点：on / off。
    Lock { state: OnOff },
    /// 把指针召回本机，同时解除组内焦点锁。
    ReturnPointerHome,
    /// 瞄准小游戏：start / stop / status。`start` 会在组内每台在线设备的每块
    /// 屏幕上盖上全屏叠加层，任意一台上按 Esc 结束。
    Aim {
        #[arg(default_value = "status")]
        action: String,
    },
    /// 屏幕焦点进入：off / on（保存原布局位置，默认本机）。
    Screen {
        state: OnOff,
        #[arg(long)]
        device: Option<String>,
    },
    /// 设备自己的物理键盘：off / on（锁屏解锁后仍禁用；本机组合键、on、后台重启或紧急停止恢复，默认本机）。
    Keyboard {
        state: OnOff,
        #[arg(long)]
        device: Option<String>,
    },
    /// 输入转换：不带 --settings 时查看；--settings <JSON文件> 修改源设备自己的配置。
    InputTransform {
        #[arg(long)]
        device: Option<String>,
        #[arg(long)]
        settings: Option<PathBuf>,
    },
    /// 游戏模式：on / off / auto / toggle / status（作用于当前焦点机）。
    GameMode {
        #[arg(default_value = "status")]
        mode: String,
    },
    /// 全局快捷键：不带参数时列出；`set <动作> <组合键>`（如 `Ctrl+Alt+L`，同时启用）、
    /// `on|off <动作>`。动作：lock_focus、return_pointer_home、toggle_sharing、open_window、toggle_game_mode。
    Hotkeys {
        #[command(subcommand)]
        cmd: Option<HotkeysCmd>,
    },
    /// 最近的剪贴板同步记录。
    Clipboard,
    /// 缓存磁盘占用；clear 一键清除。默认本机清除，查询默认列出在线组员。
    Cache {
        #[arg(long, global = true)]
        device: Option<String>,
        #[command(subcommand)]
        cmd: Option<CacheCmd>,
    },
    /// 文件接收进度、下载、取消与设置。
    Files {
        #[command(subcommand)]
        cmd: FilesCmd,
    },
    /// macOS 按需文件域：查询状态，或在 Finder 打开启用入口（不自动批准）。
    FileProvider {
        #[command(subcommand)]
        cmd: FileProviderCmd,
    },
    /// 最近的日志。
    Logs {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
}

#[derive(Subcommand, Debug)]
enum CacheCmd {
    Clear,
}

#[derive(Subcommand, Debug)]
enum FileProviderCmd {
    Status,
    OpenEnable,
    /// Device-local placeholder pool (on by default).
    PlaceholderPool {
        #[command(subcommand)]
        cmd: Option<PlaceholderPoolCmd>,
    },
}

#[derive(Subcommand, Debug)]
enum PlaceholderPoolCmd {
    /// Live stock, capacity, preparation and cleanup state; no per-drag timing.
    Status,
    On,
    Off,
}

#[derive(Subcommand, Debug)]
enum FilesCmd {
    Status,
    Fetch,
    Cancel,
    Settings,
}

#[derive(Subcommand, Debug)]
enum HotkeysCmd {
    /// 设置组合键并启用，键名用 `+` 连接：修饰键 Ctrl Alt Shift Meta，其余同界面（`L`、`F12`、`Backquote`）。
    Set {
        action: String,
        keys: String,
    },
    On {
        action: String,
    },
    Off {
        action: String,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Side {
    Right,
    Left,
    Above,
    Below,
}

#[derive(Subcommand, Debug)]
enum LayoutCmd {
    /// 正在运行的服务的布局（JSON）。
    Show,
    /// 按本机显示器自动排列的布局（不需要服务在运行）。
    Auto,
    /// Rebuild canvas groups from current OS display facts without changing the OS.
    Reset,
    /// 把设备 `device`（名称或 ID 前缀）整体摆到设备 `of`（默认本机）的某一边，保存并同步给组内。
    Place {
        device: String,
        side: Side,
        #[arg(long)]
        of: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// 打印设置文件路径。
    Path,
    /// 打印当前生效的设置（JSON）。
    Show,
    /// 写出默认设置文件（已存在则不覆盖）。
    Init,
}

fn main() -> ExitCode {
    oink_crash::capsule::fatal_boundary(main_inner)
}

fn main_inner() -> ExitCode {
    match execute() {
        Ok(code) => code,
        Err(error) => {
            // A closed output pipe is an ordinary command failure, never a
            // panic or fatal capsule. Returning also preserves host cleanup;
            // default SIGPIPE on Unix would bypass it and has no Windows peer.
            if !error.is::<cli_output::ClosedPipe>() {
                let _ = cli_output::stderr(format_args!("错误：{error:#}"));
            }
            ExitCode::FAILURE
        }
    }
}

fn execute() -> anyhow::Result<ExitCode> {
    oink_foundation::process::initialize_priority().context("Oink process priority initialization failed")?;
    #[cfg(windows)]
    if let Some(code) = oink_input::dispatch_raw_clip_guard() {
        return Ok(code);
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            cli_output::check(error.print())?;
            return Ok(ExitCode::from(error.exit_code() as u8));
        }
    };
    // The elevated installer must not read/write the user's Oink state or start
    // networking, logging, desktop backends or ordinary CLI configuration.
    #[cfg(windows)]
    if let Cmd::CompleteMicrophoneInstallation { action, token } = &cli.cmd {
        oink_agent::microphone_plugin::complete_windows_installation(action, token).map_err(anyhow::Error::msg)?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(home) = &cli.home {
        // SAFETY: 单线程启动阶段，尚未创建其他线程。
        unsafe { std::env::set_var("OINK_HOME", home) };
    }
    #[cfg(target_os = "macos")]
    let replacing = matches!(cli.cmd, Cmd::CompleteLocalUpdate { .. });
    #[cfg(not(target_os = "macos"))]
    let replacing = false;
    // The replacement helper survives the backend and flushes its own file on
    // return. Finder-launched backends do not retain a useful inherited stderr.
    let _update_log_guard = if replacing {
        oink_agent::logs::init_update()
    } else if matches!(cli.cmd, Cmd::Run) {
        oink_agent::logs::init_service();
        None
    } else {
        oink_agent::logs::init();
        None
    };
    run(cli)
}

fn print(v: &impl serde::Serialize) -> anyhow::Result<()> {
    cli_output::stdout(format_args!("{}", serde_json::to_string_pretty(v)?))
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum MotionFrequency {
    Hz125,
    Unlimited,
}

impl From<MotionFrequency> for api::EventFrequency {
    fn from(value: MotionFrequency) -> Self {
        match value {
            MotionFrequency::Hz125 => Self::Hz125,
            MotionFrequency::Unlimited => Self::Unlimited,
        }
    }
}

fn client() -> anyhow::Result<Client> {
    Client::connect(&oink_foundation::paths::data_dir())
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    // The host loads local preferences once. A preliminary CLI read would emit
    // the same startup issues twice into the service log.
    if matches!(cli.cmd, Cmd::Run) {
        // Input-method notifications require the platform main event loop.
        oink_input::run_with_main_run_loop(run_service)?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut cfg = AgentConfig::load()?;
    match cli.cmd {
        #[cfg(windows)]
        Cmd::CompleteMicrophoneInstallation { .. } => unreachable!("handled before CLI state initialization"),
        #[cfg(windows)]
        Cmd::ApproveLocalUpdate { path, sha256 } => {
            match oink_winsvc::update::request(oink_update::ipc::Request::ApproveLocal { path, sha256 })? {
                oink_update::ipc::Reply::Error { error } => return Err(error.into()),
                oink_update::ipc::Reply::Status { .. } => {}
                _ => anyhow::bail!("local approval rejected"),
            }
        }
        #[cfg(target_os = "macos")]
        Cmd::CompleteLocalUpdate {
            stage,
            destination,
            parent,
            socket,
        } => {
            oink_agent::update::local_mac::complete(&stage, &destination, parent, &socket)?;
        }
        #[cfg(windows)]
        Cmd::UpdatePolicy(args) => update_admin::run(args)?,
        #[cfg(windows)]
        Cmd::InputService { cmd } => windows_service_test::run(cmd)?,
        Cmd::Call { method, params } => print(&oink_agent::backend::cli::call(
            &cfg.data_dir,
            &method,
            params.as_deref(),
        )?)?,
        Cmd::Events => cli_output::with_stdout(|out| oink_agent::backend::cli::events(&cfg.data_dir, out))?,
        Cmd::Reopen => oink_agent::backend::cli::reopen(&cfg.data_dir)?,
        Cmd::Audio { settings } => {
            let c = client()?;
            if let Some(path) = settings {
                let settings = serde_json::from_slice(&std::fs::read(path)?)?;
                c.call(Request::SetAudioSettings { settings })?;
            }
            print(&c.call(Request::AudioStatus)?)?;
        }
        Cmd::MotionFrequency { frequency } => {
            let c = client()?;
            let mut settings: api::InputSettings = serde_json::from_value(c.call(Request::InputSettings)?)?;
            if let Some(frequency) = frequency {
                settings.event_frequency = frequency.into();
                c.call(Request::SetInputSettings {
                    settings: serde_json::from_value(serde_json::json!({"event_frequency":settings.event_frequency}))?,
                })?;
            }
            print(&settings.event_frequency)?;
        }
        Cmd::ApplicationFocus { state } => {
            print(&oink_agent::backend::cli::application_focus(
                &cfg.data_dir,
                state.map(|s| matches!(s, OnOff::On)),
            )?)?;
        }
        Cmd::AcceptContacts { state } => {
            let c = client()?;
            let mut settings: api::InputSettings = serde_json::from_value(c.call(Request::InputSettings)?)?;
            if let Some(state) = state {
                settings.accept_contacts = matches!(state, OnOff::On);
                c.call(Request::SetInputSettings {
                    settings: serde_json::from_value(serde_json::json!({"accept_contacts":settings.accept_contacts}))?,
                })?;
            }
            print(&settings.accept_contacts)?;
        }
        Cmd::TrailDuration { ms } => {
            let c = client()?;
            let mut settings: api::InputSettings = serde_json::from_value(c.call(Request::InputSettings)?)?;
            if let Some(ms) = ms {
                settings.cursor_trail_duration_ms = ms;
                c.call(Request::SetInputSettings {
                    settings: serde_json::from_value(
                        serde_json::json!({"cursor_trail_duration_ms":settings.cursor_trail_duration_ms}),
                    )?,
                })?;
            }
            print(&settings.cursor_trail_duration_ms)?;
        }
        Cmd::Config { cmd } => match cmd {
            ConfigCmd::Path => cli_output::stdout(format_args!("{}", cfg.settings_path().display()))?,
            ConfigCmd::Show => print(&cfg.settings)?,
            ConfigCmd::Init => {
                if cfg.settings_path().exists() {
                    cli_output::stdout(format_args!("已存在：{}", cfg.settings_path().display()))?;
                } else {
                    cfg.save()?;
                    cli_output::stdout(format_args!("已写入：{}", cfg.settings_path().display()))?;
                }
            }
        },
        Cmd::Displays => {
            let d = oink_display::platform_backend()
                .and_then(|b| b.displays())
                .map_err(|e| anyhow::anyhow!("显示器 backend：{e}"))?;
            print(&d)?;
        }
        Cmd::Run => unreachable!("run handled before reading preferences"),
        Cmd::Layout { cmd } => layout_cmd(cmd)?,
        Cmd::Diagnostics { cmd } => {
            let request = match cmd {
                DiagnosticsCmd::Save { last, all } => {
                    cli_output::stderr(format_args!(
                        "正在收集并压缩诊断包；音频和键鼠优先，每台连续 60 秒没有传输进展时结束收集，压缩耗时另计。默认保存最近 30 分钟，--last 可指定分钟数，--all 保存全部；完成后输出路径。"
                    ))?;
                    Request::ExportDiagnostics {
                        last_minutes: manual_save_minutes(last, all),
                    }
                }
                DiagnosticsCmd::Status => Request::DiagnosticsStatus,
                DiagnosticsCmd::On => Request::SetDiagnosticsEnabled { enabled: true },
                DiagnosticsCmd::Off => Request::SetDiagnosticsEnabled { enabled: false },
            };
            print(&client()?.call(request)?)?;
        }
        Cmd::Status => {
            let c = client()?;
            print(&serde_json::json!({
                "overview": c.call(Request::Overview)?,
                "status": c.call(Request::Status)?,
            }))?;
        }
        Cmd::Devices => {
            let c = client()?;
            print(&serde_json::json!({
                "group": c.call(Request::Group)?,
                "nearby": c.call(Request::Nearby)?,
            }))?;
        }
        Cmd::ConnectDevice { device } => {
            let c = client()?;
            let device = find_device(&c, &device)?;
            print(&c.call(Request::StartPairing {
                operation_id: request_uuid()?,
                target: api::pairing::PairTarget::NearbyDevice { device },
            })?)?;
        }
        Cmd::Pairing => print(&client()?.call(Request::Pairings)?)?,
        Cmd::Code {
            pair,
            expected_revision,
            attempt,
            code,
        } => {
            print(&client()?.call(Request::SubmitPairingCode {
                pair_id: pair,
                expected_revision: api_u64(expected_revision),
                attempt,
                code,
            })?)?;
        }
        Cmd::CancelPairing {
            pair,
            expected_revision,
        } => {
            print(&client()?.call(Request::CancelPairing {
                pair_id: pair,
                expected_revision: api_u64(expected_revision),
            })?)?;
        }
        Cmd::QueryAddress { ip, port, generation } => print(&client()?.call(Request::QueryPairingAddress {
            operation_id: request_uuid()?,
            ip,
            port,
            generation: api_u64(generation),
        })?)?,
        Cmd::Leave => print(&client()?.call(Request::LeaveGroup)?)?,
        Cmd::Addresses => print(&client()?.call(Request::NetworkInfo)?)?,
        Cmd::Sharing { state } => print(&client()?.call(Request::SetLocalPaused {
            request_id: request_uuid()?,
            paused: matches!(state, OnOff::Off),
        })?)?,
        Cmd::Aim { action } => {
            let request = match action.as_str() {
                "status" => Request::AimStatus,
                "start" => Request::AimStart,
                "stop" => Request::AimStop,
                other => anyhow::bail!("未知的操作：{other}（start / stop / status）"),
            };
            print(&client()?.call(request)?)?
        }
        Cmd::Screen { state, device } => {
            let c = client()?;
            let device = match device {
                Some(d) => {
                    let group: api::GroupView = serde_json::from_value(c.call(Request::Group)?)?;
                    select_device(group.members.into_iter().map(|m| m.device), &d)?
                }
                None => {
                    let o: api::Overview = serde_json::from_value(c.call(Request::Overview)?)?;
                    o.this_device.id
                }
            };
            print(&c.call(Request::SetScreenDisabled {
                device,
                disabled: matches!(state, OnOff::Off),
            })?)?;
        }
        Cmd::Keyboard { state, device } => {
            let c = client()?;
            let device = match device {
                Some(d) => {
                    let group: api::GroupView = serde_json::from_value(c.call(Request::Group)?)?;
                    select_device(group.members.into_iter().map(|m| m.device), &d)?
                }
                None => {
                    let o: api::Overview = serde_json::from_value(c.call(Request::Overview)?)?;
                    o.this_device.id
                }
            };
            print(&c.call(Request::SetKeyboardDisabled {
                device,
                disabled: matches!(state, OnOff::Off),
            })?)?;
        }
        Cmd::InputTransform { device, settings } => {
            let c = client()?;
            let group: api::GroupView = serde_json::from_value(c.call(Request::Group)?)?;
            let device = match device {
                Some(d) => select_device(group.members.iter().map(|m| m.device.clone()), &d)?,
                None => {
                    group
                        .members
                        .iter()
                        .find(|m| m.is_self)
                        .context("本机设备不存在")?
                        .device
                        .id
                }
            };
            if let Some(path) = settings {
                let settings: api::InputTransform = serde_json::from_slice(&std::fs::read(path)?)?;
                print(&c.call(Request::SetInputTransform { device, settings })?)?;
            } else {
                print(
                    &group
                        .members
                        .iter()
                        .find(|m| m.device.id == device)
                        .context("设备不存在")?
                        .input_transform,
                )?;
            }
        }
        Cmd::GameMode { mode } => {
            let request = match mode.as_str() {
                "status" => Request::GameMode,
                "toggle" => Request::ToggleGameMode,
                "on" => Request::SetGameMode {
                    mode: oink_routing::GameMode::On,
                },
                "off" => Request::SetGameMode {
                    mode: oink_routing::GameMode::Off,
                },
                "auto" => Request::SetGameMode {
                    mode: oink_routing::GameMode::Auto,
                },
                _ => anyhow::bail!("game-mode expects on/off/auto/toggle/status"),
            };
            print(&client()?.call(request)?)?;
        }
        Cmd::ReturnPointerHome => print(&client()?.call(Request::ReturnPointerHome)?)?,
        Cmd::Lock { state } => print(&client()?.call(Request::SetFocusLocked {
            locked: matches!(state, OnOff::On),
        })?)?,
        Cmd::Hotkeys { cmd } => hotkeys_cmd(cmd)?,
        Cmd::Files { cmd } => {
            let c = client()?;
            match cmd {
                FilesCmd::Status => print(&c.call(Request::ClipboardTransfer)?)?,
                FilesCmd::Fetch => print(&c.call(Request::FetchClipboardFiles)?)?,
                FilesCmd::Cancel => print(&c.call(Request::CancelClipboardFiles)?)?,
                FilesCmd::Settings => print(&c.call(Request::ClipboardSettings)?)?,
            }
        }
        Cmd::FileProvider { cmd } => {
            let c = client()?;
            let request = match cmd {
                FileProviderCmd::Status => Request::FileProviderStatus,
                FileProviderCmd::OpenEnable => Request::OpenFileProviderEnable,
                FileProviderCmd::PlaceholderPool { cmd } => {
                    if let Some(state @ (PlaceholderPoolCmd::On | PlaceholderPoolCmd::Off)) = cmd {
                        c.call(Request::SetFileProviderPlaceholderPool {
                            enabled: matches!(state, PlaceholderPoolCmd::On),
                        })?;
                    }
                    Request::FileProviderPlaceholderPool
                }
            };
            print(&c.call(request)?)?;
        }
        Cmd::Cache { device, cmd } => {
            let device = device
                .map(|v| serde_json::from_value::<api::DeviceKey>(serde_json::Value::String(v)))
                .transpose()?;
            print(&client()?.call(match cmd {
                Some(CacheCmd::Clear) => Request::ClearCache { device },
                None => Request::CacheUsage { device },
            })?)?;
        }
        Cmd::Clipboard => print(&client()?.call(Request::ClipboardActivity)?)?,
        Cmd::Logs { limit } => {
            let v = client()?.call(Request::Logs { after: None, limit })?;
            let lines: Vec<api::LogLine> = serde_json::from_value(v)?;
            for l in lines {
                cli_output::stdout(format_args!("{:>5} {:?} {} {}", l.seq, l.level, l.target, l.message))?;
            }
        }
        Cmd::Stop => {
            anyhow::ensure!(
                oink_agent::host::stop_service(&oink_foundation::paths::data_dir(), Duration::from_secs(10))?,
                "Oink processes did not exit within 10 seconds"
            );
            print(&serde_json::Value::Null)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn hotkeys_cmd(cmd: Option<HotkeysCmd>) -> anyhow::Result<()> {
    let c = client()?;
    let mut settings: api::InputSettings = serde_json::from_value(c.call(Request::InputSettings)?)?;
    let Some(cmd) = cmd else {
        return print(&settings.hotkeys);
    };
    let (action, keys, enabled) = match cmd {
        HotkeysCmd::Set { action, keys } => {
            let keys: Vec<String> = keys.split('+').map(|k| k.trim().to_string()).collect();
            oink_routing::Chord::parse(&keys).map_err(|e| anyhow::anyhow!("{}: {e}", keys.join("+")))?;
            (action, Some(keys), true)
        }
        HotkeysCmd::On { action } => (action, None, true),
        HotkeysCmd::Off { action } => (action, None, false),
    };
    let a = api::HotkeyAction::parse(&action).ok_or_else(|| anyhow::anyhow!("未知的动作：{action}"))?;
    let h = settings
        .hotkeys
        .iter_mut()
        .find(|h| h.action == a)
        .ok_or_else(|| anyhow::anyhow!("设置里没有这个动作：{action}"))?;
    if let Some(keys) = keys {
        h.keys = keys;
    }
    h.enabled = enabled;
    c.call(Request::SetInputSettings {
        settings: serde_json::from_value(serde_json::json!({"hotkeys":settings.hotkeys}))?,
    })?;
    print(&settings.hotkeys)
}

/// Run until an explicit stop, Ctrl-C, or SIGTERM.
#[cfg(not(windows))]
fn run_service() -> anyhow::Result<()> {
    let host = AgentHost::start_platform()?;
    let info = host.node().link_config()?.local_facts();
    cli_output::stderr(format_args!(
        "oink-agent 运行中：{}（{}），监听地址 {}。Ctrl-C 退出",
        info.name()?.0,
        host.node().id().short(),
        host.node().links()?.local_addr()
    ))?;
    host.block_on(async {
        #[cfg(unix)]
        let term = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut s) => {
                    s.recv().await;
                }
                Err(_) => std::future::pending().await,
            }
        };
        #[cfg(not(unix))]
        let term = std::future::pending::<()>();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term => {}
            _ = host.node().wait_for_stop() => {}
        }
    });
    let output = cli_output::stderr(format_args!("正在退出…"));
    host.shutdown();
    output
}

/// Windows CLI owns only the wait/stop request; the SYSTEM service owns backend creation.
#[cfg(windows)]
fn run_service() -> anyhow::Result<()> {
    let dir = oink_foundation::paths::data_dir();
    anyhow::ensure!(
        oink_agent::instance::is_running(&dir) != Some(true),
        "Oink 后台已在运行"
    );
    let pid = oink_winsvc::backend::start_in(&dir, &std::env::current_dir()?)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if Client::connect(&dir).and_then(|c| c.hello()).is_ok() {
            break;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "UIAccess 后台启动超时，请查看服务日志"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    cli_output::stderr(format_args!("Oink UIAccess 后台运行中（PID {pid}）。Ctrl-C 退出"))?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let stopped = async {
                while oink_agent::instance::is_running(&dir) == Some(true) {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            };
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = stopped => {} }
        });
    anyhow::ensure!(
        oink_agent::host::stop_service(&dir, Duration::from_secs(10))?,
        "后台退出超时"
    );
    Ok(())
}

fn api_u64(value: u64) -> oink_agent::device_cards::DecimalU64 {
    oink_agent::device_cards::DecimalU64(value)
}
fn request_uuid() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| anyhow::anyhow!("request identity entropy: {error}"))?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}
fn find_device(c: &Client, query: &str) -> anyhow::Result<api::DeviceKey> {
    let nearby: api::pairing::NearbySnapshot = serde_json::from_value(c.call(Request::Nearby)?)?;
    let q = query.trim().to_lowercase();
    anyhow::ensure!(!q.is_empty(), "device name or ID is required");
    let matches: std::collections::BTreeSet<_> = nearby
        .rows
        .into_iter()
        .filter(|row| row.name.to_lowercase() == q || row.device.0.to_hex().starts_with(&q))
        .map(|row| row.device)
        .collect();
    anyhow::ensure!(
        matches.len() == 1,
        "expected one nearby device, found {}",
        matches.len()
    );
    Ok(*matches.first().unwrap())
}
fn select_device(devices: impl Iterator<Item = api::DeviceSummary>, query: &str) -> anyhow::Result<api::DeviceKey> {
    let q = query.trim().to_lowercase();
    anyhow::ensure!(!q.is_empty(), "设备名称或 ID 不能为空");
    let matches: std::collections::BTreeSet<_> = devices
        .filter(|d| d.id.0.to_hex().starts_with(&q) || d.name.to_lowercase() == q)
        .map(|d| d.id)
        .collect();
    anyhow::ensure!(
        matches.len() == 1,
        "匹配到 {} 台设备；请用 devices 查看并指定唯一 ID，或 connect <IP> 直接连接",
        matches.len()
    );
    Ok(*matches.first().unwrap())
}

fn bbox(rects: impl Iterator<Item = Rect>) -> Option<Rect> {
    rects.reduce(|a, r| {
        let x0 = a.x.min(r.x);
        let y0 = a.y.min(r.y);
        Rect::new(x0, y0, a.right().max(r.right()) - x0, a.bottom().max(r.bottom()) - y0)
    })
}

fn layout_cmd(cmd: LayoutCmd) -> anyhow::Result<()> {
    match cmd {
        LayoutCmd::Auto => {
            let d = oink_display::platform_backend()
                .and_then(|b| b.displays())
                .map_err(|e| anyhow::anyhow!("显示器 backend：{e}"))?;
            print(&Layout::auto(&[(DeviceId([0; 32]), d)]))?;
        }
        LayoutCmd::Reset => print(&client()?.call(Request::ResetInitialLayout)?)?,
        LayoutCmd::Show => print(&client()?.call(Request::Layout)?)?,
        LayoutCmd::Place { device, side, of } => {
            let c = client()?;
            let layout: api::Layout = serde_json::from_value(c.call(Request::Layout)?)?;
            let me: api::Overview = serde_json::from_value(c.call(Request::Overview)?)?;
            let screens = &layout.geometry.screens;
            let pick = |q: &str| -> anyhow::Result<api::DeviceKey> {
                let q = q.to_lowercase();
                screens
                    .iter()
                    .find(|s| s.device_name.to_lowercase() == q || s.key.device.short().to_lowercase() == q)
                    .map(|s| s.key.device)
                    .or_else(|| {
                        let hex = |k: &api::DeviceKey| serde_json::to_value(k).ok()?.as_str().map(str::to_string);
                        screens
                            .iter()
                            .find(|s| hex(&s.key.device).is_some_and(|h| h.starts_with(&q)))
                            .map(|s| s.key.device)
                    })
                    .ok_or_else(|| anyhow::anyhow!("布局里没有设备「{q}」"))
            };
            let target = pick(&device)?;
            let reference = match of {
                Some(o) => pick(&o)?,
                None => me.this_device.id,
            };
            let rect_of = |d: api::DeviceKey| {
                bbox(
                    screens
                        .iter()
                        .filter(|s| s.key.device == d && !s.hidden)
                        .map(|s| s.rect()),
                )
            };
            let (t, r) = (
                rect_of(target).ok_or_else(|| anyhow::anyhow!("目标设备没有可见屏幕"))?,
                rect_of(reference).ok_or_else(|| anyhow::anyhow!("参照设备没有可见屏幕"))?,
            );
            let (dx, dy) = match side {
                Side::Right => (r.right() - t.x, r.center().y - t.center().y),
                Side::Left => (r.x - t.right(), r.center().y - t.center().y),
                Side::Below => (r.center().x - t.center().x, r.bottom() - t.y),
                Side::Above => (r.center().x - t.center().x, r.y - t.bottom()),
            };
            let screen = screens.iter().find(|s| s.key.device == target).expect("target screen");
            let scale = screen.size.w / screen.display.bounds.w;
            let placements = vec![Placement {
                device: target,
                origin: Point::new(
                    screen.origin.x - screen.display.bounds.x * scale + dx,
                    screen.origin.y - screen.display.bounds.y * scale + dy,
                ),
                scale,
            }];
            print(&c.call(Request::SaveLayout { placements })?)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_pool_cli_accepts_status_on_off_without_other_controls() {
        for (arg, expected) in [("status", 0), ("on", 1), ("off", 2)] {
            let cli = Cli::try_parse_from(["oink-agent", "file-provider", "placeholder-pool", arg]).unwrap();
            let Cmd::FileProvider {
                cmd: FileProviderCmd::PlaceholderPool { cmd: Some(command) },
            } = cli.cmd
            else {
                panic!("wrong command")
            };
            assert_eq!(
                match command {
                    PlaceholderPoolCmd::Status => 0,
                    PlaceholderPoolCmd::On => 1,
                    PlaceholderPoolCmd::Off => 2,
                },
                expected
            );
        }
        assert!(matches!(
            Cli::try_parse_from(["oink-agent", "file-provider", "placeholder-pool"])
                .unwrap()
                .cmd,
            Cmd::FileProvider {
                cmd: FileProviderCmd::PlaceholderPool { cmd: None }
            }
        ));
        assert!(Cli::try_parse_from(["oink-agent", "file-provider", "placeholder-pool", "maybe"]).is_err());
    }

    #[test]
    fn retired_mac_scroll_experiment_is_not_a_command() {
        assert!(Cli::try_parse_from(["oink-agent", "test-mac-scroll"]).is_err());
    }
    #[test]
    fn application_focus_cli_parses_query_and_on_off() {
        assert!(matches!(
            Cli::try_parse_from(["oink-agent", "application-focus"]).unwrap().cmd,
            Cmd::ApplicationFocus { state: None }
        ));
        for (arg, expected) in [("on", true), ("off", false)] {
            let Cmd::ApplicationFocus { state: Some(state) } =
                Cli::try_parse_from(["oink-agent", "application-focus", arg])
                    .unwrap()
                    .cmd
            else {
                panic!("wrong command")
            };
            assert_eq!(matches!(state, OnOff::On), expected);
        }
        assert!(Cli::try_parse_from(["oink-agent", "application-focus", "maybe"]).is_err());
    }
    #[test]
    fn accept_contacts_cli_parses_query_and_on_off() {
        assert!(matches!(
            Cli::try_parse_from(["oink-agent", "accept-contacts"]).unwrap().cmd,
            Cmd::AcceptContacts { state: None }
        ));
        for (arg, expected) in [("on", true), ("off", false)] {
            let Cmd::AcceptContacts { state: Some(state) } =
                Cli::try_parse_from(["oink-agent", "accept-contacts", arg]).unwrap().cmd
            else {
                panic!("wrong command");
            };
            assert_eq!(matches!(state, OnOff::On), expected);
        }
        assert!(Cli::try_parse_from(["oink-agent", "accept-contacts", "maybe"]).is_err());
    }
    #[test]
    fn trail_duration_cli_parses_query_range_and_rejects_invalid_values() {
        for ms in ["100", "270", "10000", "60000"] {
            let cmd = Cli::try_parse_from(["oink-agent", "trail-duration", ms]).unwrap().cmd;
            assert!(matches!(cmd, Cmd::TrailDuration { ms: Some(value) } if value == ms.parse::<u32>().unwrap()));
        }
        assert!(matches!(
            Cli::try_parse_from(["oink-agent", "trail-duration"]).unwrap().cmd,
            Cmd::TrailDuration { ms: None }
        ));
        for ms in ["0", "99", "60001", "-1", "1.5", "abc"] {
            assert!(Cli::try_parse_from(["oink-agent", "trail-duration", ms]).is_err());
        }
    }

    #[test]
    fn motion_frequency_cli_accepts_both_choices_and_read_only_query() {
        for (value, expected) in [
            ("hz125", api::EventFrequency::Hz125),
            ("unlimited", api::EventFrequency::Unlimited),
        ] {
            let cli = Cli::try_parse_from(["oink-agent", "motion-frequency", value]).unwrap();
            let Cmd::MotionFrequency {
                frequency: Some(frequency),
            } = cli.cmd
            else {
                panic!("motion frequency command")
            };
            assert_eq!(api::EventFrequency::from(frequency), expected);
        }
        assert!(matches!(
            Cli::try_parse_from(["oink-agent", "motion-frequency"]).unwrap().cmd,
            Cmd::MotionFrequency { frequency: None }
        ));
        assert!(Cli::try_parse_from(["oink-agent", "motion-frequency", "hz500"]).is_err());
    }
}

fn manual_save_minutes(last: Option<u32>, all: bool) -> Option<u32> {
    if all { None } else { Some(last.unwrap_or(30)) }
}

#[cfg(test)]
mod diagnostics_cli_tests {
    use super::*;
    #[test]
    fn save_defaults_to_thirty_and_parses_all_or_positive_minutes() {
        for (args, expected) in [
            (vec!["oink-agent", "diagnostics", "save"], Some(30)),
            (vec!["oink-agent", "diagnostics", "save", "--all"], None),
            (vec!["oink-agent", "diagnostics", "save", "--last", "1440"], Some(1440)),
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(
                matches!(cli.cmd, Cmd::Diagnostics { cmd: DiagnosticsCmd::Save { last, all } } if manual_save_minutes(last, all) == expected)
            );
        }
        assert!(Cli::try_parse_from(["oink-agent", "diagnostics", "save", "--all", "--last", "30"]).is_err());
        for value in ["0", "-1", "1.5", "4294967296"] {
            assert!(Cli::try_parse_from(["oink-agent", "diagnostics", "save", "--last", value]).is_err());
        }
    }
}

#[cfg(test)]
mod product_boundary {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_version_uses_the_compiled_release_label() {
        assert_eq!(Cli::command().get_version(), Some(env!("OINK_RELEASE_VERSION")));
        let output = Cli::try_parse_from(["oink-agent", "--version"])
            .unwrap_err()
            .to_string();
        assert_eq!(output, format!("oink-agent {}\n", env!("OINK_RELEASE_VERSION")));
    }

    #[test]
    fn cli_schema_keeps_administration_and_excludes_hooks_in_release() {
        let command = Cli::command();
        for name in [
            "call",
            "events",
            "status",
            "stop",
            "diagnostics",
            "file-provider",
            "audio",
            "aim",
        ] {
            assert!(
                command.find_subcommand(name).is_some(),
                "missing product command {name}"
            );
        }
        for name in ["synth", "clip", "debug"] {
            assert!(command.find_subcommand(name).is_none(), "{name}");
        }
        assert!(command.find_subcommand("loopback").is_none());
        assert!(command.find_subcommand("system-arrangement").is_none());
        for name in ["seconds", "name", "port"] {
            assert!(
                !command
                    .find_subcommand("run")
                    .unwrap()
                    .get_arguments()
                    .any(|arg| arg.get_id() == name),
                "{name}"
            );
        }
        assert!(!command.get_arguments().any(|arg| arg.get_long() == Some("env")));
        #[cfg(windows)]
        {
            let commands: Vec<_> = command
                .find_subcommand("input-service")
                .unwrap()
                .get_subcommands()
                .map(|c| c.get_name())
                .collect();
            assert_eq!(commands, ["status"]);
        }
    }
}
