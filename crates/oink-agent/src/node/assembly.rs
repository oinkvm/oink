//! Startup and reset assembly for the backend.
use super::*;
use crate::config_module::{ConfigDeps, ConfigModule};
use crate::threads::input::{InputCommand, InputSnapshots, InputThread};
use oink_foundation::realtime::Tier;
use oink_network::link::{LinkConfig, LinkLayer};

impl Node {
    #[cfg(test)]
    pub(crate) fn offer_profile() -> oink_network::negotiation::OfferProfile {
        application_profile()
    }

    pub(super) async fn start_sharing(&self) -> anyhow::Result<()> {
        self.start_sharing_with_profile(application_profile()).await
    }

    pub(super) async fn start_sharing_with_profile(
        &self,
        profile: oink_network::negotiation::OfferProfile,
    ) -> anyhow::Result<()> {
        // The caller serializes first start/reset. Check before LinkLayer::start: even
        // a bound socket would violate the IPC-only unreadable-state contract.
        crate::config_module::group_store::load(
            &self.inner.data_dir.join(crate::config_module::group_store::STATE_FILE),
            self.id(),
        )?;
        let initial_displays = self.current_displays()?;
        {
            let _gate = self.owner_gate();
            lock(&self.inner.config).seed_screen_preferences(
                initial_displays
                    .iter()
                    .map(|display| (display.key.as_str(), display.is_virtual)),
            )?;
        }
        let (links, config_port) = LinkLayer::start(LinkConfig {
            profile,
            identity: self.inner.identity.clone(),
            boot: self.inner.boot,
            bind: (self.inner.network.bind_ip, self.inner.network.port).into(),
            // Selected from the loaded two-tier measurements on native systems.
            // The network owner still enforces every class/round time budget.
            tier: Tier::Realtime,
        })
        .await?;
        if self.inner.native_surfaces {
            if let Some(trail) = oink_locator::CursorTrail::platform() {
                let _ = self.inner.cursor_trail.set(trail);
            }
            if let Some(overlay) = oink_locator::aim::AimOverlay::platform() {
                self.set_aim_surface(Some(Arc::new(overlay)));
            }
            crate::aim::set_follow_trail(self, oink_locator::CursorTrail::platform());
            self.apply_trail_appearance();
            self.watch_trail_appearance();
        }
        // Registration precedes discovery and every possible ready notification.
        let input_port = links.register(crate::streams::input::KINDS);
        let clip_port = links.register(crate::streams::clipboard::KINDS);
        let audio_port = links.register(crate::streams::audio::KINDS);
        let aim_port = links.register(crate::streams::aim::KINDS);
        let projection_port = links.register(crate::streams::projection::KINDS);
        let wallpaper_port = links.register(crate::streams::wallpaper::KINDS);
        let cache_port = links.register(&[oink_protocol::frame::StreamKind::Cache]);
        let diagnostics_port = links.register(crate::streams::diagnostics::KINDS);
        let dnd_port = links.register(crate::streams::dnd::KINDS);
        self.inner
            .links
            .set(links.clone())
            .map_err(|_| anyhow::anyhow!("network already started"))?;

        let group = ConfigModule::start_with_config(
            ConfigDeps {
                config: self.inner.config.clone(),
                owner_gate: self.inner.owner_gate.clone(),
                data_dir: self.inner.data_dir.clone(),
                links: links.clone(),
                port: config_port,
                identity: self.inner.identity.clone(),
                application: Arc::new(Application(Arc::downgrade(&self.inner))),
            },
            self.inner.network.clone(),
        )?;
        self.inner
            .group
            .set(group.clone())
            .map_err(|_| anyhow::anyhow!("configuration already started"))?;
        // Initial inputs are published before the input owner takes its first turn.
        // The module retains startup state and broadcasts later changes itself.
        self.publish_local_options();
        self.publish_local_permissions();
        let initial_group = group.clone();
        tokio::task::spawn_blocking(move || initial_group.initialize_displays(initial_displays)).await??;
        let applied = group.applied();
        let settings = self.settings();
        let directory = self.log_dir().join("Diagnostics");
        let recorder = crate::diagnostics::Recorder::start_suspended(
            directory,
            self.id(),
            Some(applied.group),
            settings.diagnostics_enabled,
            applied.paused,
            Some(crate::diagnostics::collection::Context::for_node(self)?),
        )?;
        let mut recovery_guard = crate::diagnostics::StartupRecoveryGuard(Some(recorder.clone()));
        group.observe_layout(&recorder)?;
        recorder.install_native_clock();
        oink_input::native_lifecycle::set_lifecycle(settings.diagnostics_enabled);
        let trace = crate::diagnostics::input_trace::InputTrace::for_recorder(&recorder);
        let timing: Arc<dyn oink_foundation::handoff_timing::Observer> = trace.clone();
        oink_foundation::handoff_timing::set_observer(Arc::downgrade(&timing));
        let _ = trace.config.set(group.clone());
        let _ = self.inner.input_trace.set(trace.clone());
        // A restarted Windows candidate enters health-only admission before
        // input, clipboard, drag and audio owners can accept any business work.
        #[cfg(windows)]
        if self.inner.native_services {
            // Private fake-platform fixtures must not contact the installed
            // Windows update service or join its candidate health transaction.
            crate::update::startup_gate()?;
        }
        let inject = crate::threads::inject::start_with_trace(
            self.inner.input_backend.clone(),
            Box::new(|_| {}),
            Some(trace.clone()),
        )?;
        let (main, receive) = crate::threads::main_channel();
        let _ = self.inner.main_commands.set(main.clone());
        let trace_wake = main.notifier();
        trace.set_wake(trace_wake.clone());
        oink_input::native_lifecycle::set_wake(trace_wake);
        let (input, notices) = InputThread::start(
            links.boot(),
            InputSnapshots {
                mode_switch_in_callback: self.inner.mode_switch_in_callback,
                capture_reports_key_repeat: self.inner.capture_reports_key_repeat,
                layout: group.router_layout(),
                settings: self.inner.input_settings.clone(),
                environment: self.inner.input_environment.clone(),
            },
            inject,
            main,
        )?;
        let input = Arc::new(input);
        self.inner
            .input
            .set(input.clone())
            .map_err(|_| anyhow::anyhow!("input already started"))?;
        input.send(InputCommand::Hotkeys(crate::hotkeys::bindings(
            &crate::hotkeys::configured(settings.hotkeys),
        )));
        input.send(InputCommand::LocalFeel(self.inner.input_backend.feel_profile()));
        input.send(InputCommand::EventFrequency(settings.event_frequency));
        let mut tasks = vec![crate::streams::input::InputStreams::spawn_with_port(
            links,
            group.clone(),
            input,
            notices,
            !inject_blocked(&self.inner.input_backend.permissions()),
            input_port,
        )];
        if let Some(backend) = &self.inner.clipboard_backend {
            tasks.push(crate::clip::start_link_runtime(
                self.clone(),
                clip_port,
                backend.clone(),
            )?);
        }
        let (audio, audio_task) = crate::audio::start_link_runtime(self.clone(), audio_port)?;
        self.inner
            .audio
            .set(audio)
            .map_err(|_| anyhow::anyhow!("audio already started"))?;
        tasks.push(audio_task);
        tasks.push(crate::cache::start(self.clone(), cache_port));
        tasks.push(crate::display_overlay::start(self.clone())?);
        tasks.push(crate::aim::start_link_runtime(self.clone(), aim_port)?);
        tasks.push(crate::projection::start(
            self.clone(),
            projection_port,
            self.inner.native_surfaces,
        )?);
        tasks.push(crate::wallpaper::start_link_runtime(self.clone(), wallpaper_port)?);
        let (diagnostics, diagnostics_task) =
            crate::diagnostics::collection::start_link_runtime(self.clone(), diagnostics_port, recorder)?;
        self.inner
            .diagnostics
            .set(diagnostics)
            .map_err(|_| anyhow::anyhow!("diagnostics already started"))?;
        tasks.push(diagnostics_task);
        let (dnd, dnd_task) = crate::dnd::start_link_runtime(self.clone(), dnd_port)?;
        self.inner
            .dnd
            .set(dnd)
            .map_err(|_| anyhow::anyhow!("drag task already started"))?;
        tasks.push(dnd_task);
        lock(&self.inner.tasks).extend(tasks);
        self.start_main_commands(receive)?;
        self.start_observers()?;
        recovery_guard.0 = None;
        *lock(&self.inner.group_unreadable) = None;
        self.emit_group();
        self.emit_layout();
        Ok(())
    }

    pub(crate) async fn reset_unreadable_group(&self) -> api::ApiResult<()> {
        let _startup = self.inner.startup.lock().await;
        if self.inner.group.get().is_some() || lock(&self.inner.group_unreadable).is_none() {
            return Err(api::ApiError::InvalidState {
                message: "组状态可以读取，不能重置运行中的共享".into(),
            });
        }
        crate::config_module::reset_group_state(self.data_dir(), &self.inner.identity).map_err(|error| {
            api::ApiError::Unavailable {
                message: error.to_string(),
            }
        })?;
        self.start_sharing().await.map_err(|error| api::ApiError::Unavailable {
            message: error.to_string(),
        })?;
        // The host skipped native DnD while the state was unreadable. Reset runs
        // after the native dispatcher is ready, so finish the same activation here.
        if self.inner.native_surfaces {
            let node = self.clone();
            crate::backend::on_main(move || node.start_platform_dnd());
        }
        Ok(())
    }
}

struct Application(std::sync::Weak<Inner>);
impl crate::config_module::ConfigApplication for Application {
    fn apply_state(&self, state: &crate::config_module::group_store::StoredGroupState) -> Result<(), String> {
        use oink_protocol::v1::{dto::LayoutValue, schema::WireDto};
        let inner = self.0.upgrade().ok_or("backend stopped")?;
        let layout = state
            .layout_register()
            .map_err(|error| error.to_string())?
            .value()
            .map_err(|error| error.to_string())?;
        let layout = LayoutValue::from_raw(layout).map_err(|error| error.to_string())?;
        crate::config_module::layout_wire::validate(&layout).map_err(|error| error.to_string())?;

        if let Some(group) = inner.group.get() {
            let roster = state.state.roster().map_err(|error| error.to_string())?;
            let anchor = oink_network::roster::Anchor {
                group: state.group(),
                root: roster.root_id().map_err(|error| error.to_string())?.0,
            };
            // The configuration owner has already authorized this candidate.
            // Signed self-removal is applied before the fresh group exists;
            // close old-group surfaces before exposing that intermediate roster.
            let roster = oink_network::roster::Roster::verify(roster, anchor).map_err(|error| error.to_string())?;
            if group.applied().group != state.group() || !roster.contains(inner.me) {
                crate::aim::stop(&Node { inner: inner.clone() });
            }
        }
        Ok(())
    }
}

/// Capabilities describe implemented modules. Local projection permission is
/// enforced by its stream owner so toggling it never requires reconnecting.
fn application_profile() -> oink_network::negotiation::OfferProfile {
    let mut profile = oink_network::negotiation::OfferProfile::default();
    profile.capabilities.extend(
        oink_clipboard::negotiation::CAPABILITIES
            .iter()
            .copied()
            .chain(oink_audio::negotiation::CAPABILITIES.iter().copied())
            .chain([
                "oink.input.relative/1",
                "oink.input.gestures/1",
                "oink.input.contacts/1",
                "oink.input.trail/1",
                "oink.input.edge-trigger/1",
                "oink.device-resources/1",
                "oink.device-options/1",
                "oink.aim/1",
                "oink.notify/1",
                "oink.wallpaper/1",
                "oink.layout.projection/1",
                "oink.diagnostics.collect/1",
                "oink.diagnostics.layout/1",
            ])
            .map(str::to_owned),
    );
    profile.capabilities.sort();
    profile.capabilities.dedup();
    profile.formats = oink_clipboard::negotiation::formats();
    profile.audio = oink_audio::negotiation::audio();
    profile
}
