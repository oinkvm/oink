//! One retained, atomic settings store for local preferences and owner revisions.

use std::path::{Path, PathBuf};

use oink_routing::RoutingSettings;
use serde::{Deserialize, Serialize};

mod deferred;
pub mod owner;
pub mod preferences;
mod projection;
pub(crate) mod read;
pub mod records;
pub mod store;
pub use preferences::*;
pub(crate) use projection::{read_transform, transform as write_transform};

/// Local read diagnostics: paths and problem kinds only, never setting values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsIssue {
    pub path: String,
    pub kind: SettingsIssueKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsIssueKind {
    UnknownKey,
    Defaulted,
    DroppedEntry,
}

pub const SETTINGS_FILE: &str = "settings.json";

/// An effective local snapshot. Owner fields are projections of the retained
/// owner store; only local fields are serialized into the JSON `local` object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub reports: oink_telemetry::ReportConsent,
    pub diagnostics_enabled: bool,
    pub audio: oink_audio::Settings,
    #[serde(skip)]
    pub microphone: oink_audio::microphone::Settings,
    #[serde(skip)]
    pub device_name: Option<String>,
    pub routing: RoutingSettings,
    pub event_frequency: oink_display::layout::EventFrequency,
    pub onboarding_done: bool,
    /// Confirmation only; the operating system owns the actual login-item state.
    pub launch_at_login_configured: bool,
    pub hotkeys: Option<Vec<crate::api::Hotkey>>,
    pub clipboard: Option<oink_clipboard::policy::Settings>,
    pub file_provider_placeholder_pool: bool,
    pub received_files_cleanup: crate::api::ReceivedFilesCleanup,
    pub log_level: LogLevel,
    pub log_max_mb: LogMaxMb,
    pub ui_language: UiLanguage,
    pub notification_level: crate::api::NotificationSetting,
    pub theme_mode: ThemeMode,
    pub theme_accent: ThemeAccent,
    pub show_status_icon: bool,
    pub confirm_quit: bool,
    pub emergency_hold_ms: EmergencyHoldMs,
    #[serde(skip)]
    pub clipboard_enabled: bool,
    #[serde(skip)]
    pub screen_shared: bool,
    #[serde(skip)]
    pub screen_preferences: ScreenPreferences,
    #[serde(skip)]
    pub speed_mode: SpeedMode,
    #[serde(skip)]
    pub cursor_trail_duration_ms: u32,
    #[serde(skip)]
    pub input_transform: oink_foundation::input_transform::InputTransform,
    #[serde(skip)]
    pub game_mode: GameModePreference,
    #[serde(skip)]
    pub accept_contacts: bool,
    #[serde(skip)]
    pub sync_system_arrangement: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            reports: Default::default(),
            diagnostics_enabled: true,
            audio: Default::default(),
            microphone: Default::default(),
            device_name: None,
            routing: RoutingSettings::default(),
            event_frequency: Default::default(),
            onboarding_done: false,
            launch_at_login_configured: false,
            hotkeys: Some(crate::agent_api::default_hotkeys()),
            clipboard: Some(crate::clip::default_settings()),
            file_provider_placeholder_pool: true,
            received_files_cleanup: Default::default(),
            log_level: LogLevel::Info,
            log_max_mb: LogMaxMb::default(),
            ui_language: UiLanguage::Auto,
            notification_level: Default::default(),
            theme_mode: ThemeMode::System,
            theme_accent: ThemeAccent::default(),
            show_status_icon: true,
            confirm_quit: true,
            emergency_hold_ms: EmergencyHoldMs::default(),
            clipboard_enabled: true,
            screen_shared: true,
            screen_preferences: ScreenPreferences::default(),
            speed_mode: SpeedMode::Physical,
            cursor_trail_duration_ms: 270,
            input_transform: oink_foundation::input_transform::InputTransform::initial(std::env::consts::OS),
            game_mode: GameModePreference::Auto,
            accept_contacts: true,
            sync_system_arrangement: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub data_dir: PathBuf,
    pub settings: Settings,
    pub settings_issues: Vec<SettingsIssue>,
    owner: owner::OwnerStore,
    envelope: serde_json::Value,
    baseline: Settings,
    deferred: Option<deferred::PendingWrite>,
}

impl AgentConfig {
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&oink_foundation::paths::data_dir())
    }

    /// Reads only schema 1. Unknown envelopes and malformed files stay untouched.
    /// A missing file supplies defaults without writing or generating an identity.
    pub fn load_from(dir: &Path) -> anyhow::Result<Self> {
        Self::load_from_with_device_name(dir, system_device_name())
    }

    /// The network assembly can supply an initial name for a fresh isolated
    /// instance. An existing durable owner name always remains authoritative.
    pub(crate) fn load_from_with_device_name(dir: &Path, initial_name: String) -> anyhow::Result<Self> {
        use base64::Engine;
        let path = dir.join(SETTINGS_FILE);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let mut reader = read::Reader::default();
        let (mut envelope, mut settings, mut owner) = if let Some(bytes) = bytes {
            let envelope: serde_json::Value = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(
                envelope.get("schema").and_then(serde_json::Value::as_u64) == Some(1),
                "unsupported settings schema"
            );
            let local = envelope
                .get("local")
                .filter(|value| value.is_object())
                .ok_or(store::StoreError::Invalid)?;
            let settings = reader.settings(local.clone());
            let encoded = envelope
                .get("device_options_cbor")
                .and_then(serde_json::Value::as_str)
                .ok_or(store::StoreError::Invalid)?;
            let owner = owner::OwnerStore::decode(&base64::engine::general_purpose::STANDARD.decode(encoded)?)?;
            (envelope, settings, owner)
        } else {
            let settings = Settings {
                device_name: Some(initial_name),
                ..Settings::default()
            };
            let owner = owner::OwnerStore::new(projection::values(&settings))?;
            (serde_json::json!({"schema":1,"local":{}}), settings, owner)
        };
        let mut extended = owner.ensure_option(
            oink_protocol::v1::dto::LocalOption::SyncSystemArrangement,
            oink_protocol::v1::RawValue::Bool(false),
        )?;
        // Upgrade the old receiver choice without changing its behavior: an
        // old OFF becomes parent ON + mapped gestures + the existing scroll feel.
        // Keep LocalOption 19's released meaning for sources without the new capability.
        let legacy = owner
            .value(oink_protocol::v1::dto::LocalOption::AcceptContacts)
            .and_then(|value| {
                if let oink_protocol::v1::RawValue::Bool(v) = value {
                    Some(v)
                } else {
                    None
                }
            })
            .unwrap_or(true);
        for (option, value) in [
            (oink_protocol::v1::dto::LocalOption::TouchpadInjection, true),
            (oink_protocol::v1::dto::LocalOption::NativeMultifinger, legacy),
            (
                oink_protocol::v1::dto::LocalOption::MacScrollFeel,
                // The old scroll checkbox had no effect while raw contacts
                // were OFF: synthetic scrolling always preserved the Mac curve.
                !legacy || settings.routing.mac_scroll_feel,
            ),
        ] {
            extended |= owner.ensure_option(option, oink_protocol::v1::RawValue::Bool(value))?;
        }
        if let Some(local) = envelope
            .get_mut("local")
            .and_then(|v| v.get_mut("routing"))
            .and_then(serde_json::Value::as_object_mut)
        {
            for field in ["mac_scroll_feel", "touchpad_injection", "native_multifinger"] {
                extended |= local.remove(field).is_some();
            }
        }
        for (option, value) in projection::values(&Settings::default())
            .into_iter()
            .filter(|(option, _)| {
                matches!(
                    option,
                    oink_protocol::v1::dto::LocalOption::MicrophoneShareClaim
                        | oink_protocol::v1::dto::LocalOption::MicrophoneControls
                        | oink_protocol::v1::dto::LocalOption::MicrophoneReceiveMuted
                        | oink_protocol::v1::dto::LocalOption::MicrophoneInput
                )
            })
        {
            extended |= owner.ensure_option(option, value)?;
        }
        projection::apply(&owner, &mut settings)?;
        for issue in &reader.issues {
            if issue.kind != SettingsIssueKind::UnknownKey {
                tracing::warn!(path = %issue.path, kind = ?issue.kind, "local setting was not understood");
            }
        }
        let mut config = Self {
            data_dir: dir.to_owned(),
            baseline: settings.clone(),
            settings,
            settings_issues: reader.issues,
            owner,
            envelope,
            deferred: None,
        };
        if extended {
            config.save()?;
        }
        Ok(config)
    }
    pub fn settings_path(&self) -> PathBuf {
        self.data_dir.join(SETTINGS_FILE)
    }
    pub fn owner(&self) -> &owner::OwnerStore {
        &self.owner
    }

    /// Commit settings and every changed owner revision together. A failed commit
    /// restores the previous effective settings and never publishes a candidate.
    pub fn save(&mut self) -> anyhow::Result<()> {
        let result = (|| {
            let mut candidate = self.projected_candidate(&self.settings)?;
            candidate.persist()?;
            Ok::<_, anyhow::Error>(candidate)
        })();
        match result {
            Ok(candidate) => {
                *self = candidate;
                Ok(())
            }
            Err(error) => {
                self.settings = self.baseline.clone();
                Err(error)
            }
        }
    }
    /// Build the exact owner preference baseline that `save` would commit. The
    /// caller can preflight a complete Facts frame without disk or native effects.
    pub fn preview_preferences(
        &self,
        settings: &Settings,
    ) -> anyhow::Result<Vec<oink_protocol::v1::dto::DevicePreference>> {
        Ok(self.projected_candidate(settings)?.owner.preferences())
    }
    fn projected_candidate(&self, settings: &Settings) -> anyhow::Result<Self> {
        let mut candidate = self.clone();
        candidate.settings = settings.clone();
        let before = projection::values(&self.baseline);
        let after = projection::values(settings);
        for ((option, before), (_, after)) in before.into_iter().zip(after) {
            if before != after {
                let expected = candidate.owner.revision(option).ok_or(store::StoreError::Invalid)?;
                let patch = owner::typed_patch(option, &before, &after);
                candidate
                    .owner
                    .edit(option, expected, patch)
                    .map_err(|_| store::StoreError::Invalid)?;
            }
        }
        projection::apply(&candidate.owner, &mut candidate.settings)?;
        Ok(candidate)
    }
    pub fn update(&mut self, update: impl FnOnce(&mut Settings)) -> anyhow::Result<()> {
        update(&mut self.settings);
        self.save()
    }
    /// Record explicit local choices, including choices equal to today's default.
    /// Owner fields are filtered out against the local projection. Publish both
    /// the effective snapshot and the retained overrides only after atomic save.
    pub fn update_local(
        &mut self,
        patch: &serde_json::Value,
        update: impl FnOnce(&mut Settings),
    ) -> anyhow::Result<()> {
        let mut settings = self.settings.clone();
        update(&mut settings);
        let mut candidate = self.projected_candidate(&settings)?;
        let local = projection::local(&candidate.settings);
        read::explicit_json(&mut candidate.envelope["local"], patch, &local);
        candidate.persist()?;
        *self = candidate;
        Ok(())
    }
    /// Seed only newly observed display identities. The boolean is the native
    /// virtual-display observation; subsequent user choices and unplugged
    /// display preferences are never overwritten or removed by enumeration.
    pub fn seed_screen_preferences<'a>(
        &mut self,
        displays: impl IntoIterator<Item = (&'a str, bool)>,
    ) -> anyhow::Result<bool> {
        let mut preferences = self.settings.screen_preferences.screens.clone();
        for (display, is_virtual) in displays {
            if !preferences.iter().any(|screen| screen.display == display) {
                preferences.push(ScreenPreference {
                    remote_gain: 1.0,
                    display: display.to_owned(),
                    gain: 1.0,
                    enabled: !is_virtual,
                });
            }
        }
        if preferences == self.settings.screen_preferences.screens {
            return Ok(false);
        }
        self.update(|settings| settings.screen_preferences.screens = preferences)?;
        Ok(true)
    }
    pub fn edit_owner(
        &mut self,
        option: oink_protocol::v1::dto::LocalOption,
        expected: u64,
        patch: oink_protocol::v1::RawValue,
    ) -> Result<oink_protocol::v1::dto::DevicePreference, oink_protocol::v1::dto::ControlFailure> {
        use oink_protocol::v1::dto::ControlFailure;
        let mut candidate = self.clone();
        let item = candidate.owner.edit(option, expected, patch)?;
        projection::apply(&candidate.owner, &mut candidate.settings).map_err(|_| ControlFailure::InvalidValue)?;
        candidate.persist().map_err(|_| ControlFailure::PersistenceFailed)?;
        *self = candidate;
        Ok(item)
    }
    pub fn update_owner<T>(
        &mut self,
        update: impl FnOnce(&mut owner::OwnerStore) -> Result<T, store::StoreError>,
    ) -> anyhow::Result<T> {
        let mut candidate = self.clone();
        let result = update(&mut candidate.owner)?;
        projection::apply(&candidate.owner, &mut candidate.settings)?;
        candidate.persist()?;
        *self = candidate;
        Ok(result)
    }
    fn persist(&mut self) -> anyhow::Result<()> {
        let _activity = oink_foundation::maintenance::process()
            .enter(oink_foundation::maintenance::ActivityKind::Configuration)
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::WouldBlock))?;
        self.persist_admitted()
    }
    // Called only under a configuration admission lease (including the lease
    // retained by a deferred edit while update admission is closed).
    fn persist_admitted(&mut self) -> anyhow::Result<()> {
        use base64::Engine;
        let before = projection::local(&self.baseline);
        let after = projection::local(&self.settings);
        let local = self.envelope.get_mut("local").ok_or(store::StoreError::Invalid)?;
        read::patch_json(local, &before, &after);
        self.envelope["device_options_cbor"] = base64::engine::general_purpose::STANDARD
            .encode(self.owner.encode()?)
            .into();
        std::fs::create_dir_all(&self.data_dir)?;
        oink_foundation::storage::atomic_write(&self.settings_path(), &serde_json::to_vec_pretty(&self.envelope)?)?;
        self.baseline = self.settings.clone();
        self.deferred = None;
        Ok(())
    }
    pub fn device_name(&self) -> String {
        self.settings.device_name.clone().unwrap_or_else(system_device_name)
    }
}

/// The operating system's user-visible computer name.
pub fn system_device_name() -> String {
    if let Some(n) = std::env::var_os("COMPUTERNAME").and_then(|s| s.into_string().ok()) {
        return n;
    }
    #[cfg(target_os = "macos")]
    if let Some(n) = command_output("scutil", &["--get", "ComputerName"]) {
        return n;
    }
    command_output("hostname", &[]).unwrap_or_else(|| "Oink device".to_string())
}

fn command_output(cmd: &str, args: &[&str]) -> Option<String> {
    let mut command = std::process::Command::new(cmd);
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // This fallback may run in a GUI/detached process without a console.
        command.creation_flags(windows::Win32::System::Threading::CREATE_NO_WINDOW.0);
    }
    let out = command.output().ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oink_protocol::v1::{
        RawValue,
        dto::{ControlFailure, LocalOption},
        schema::WireDto,
    };
    #[test]
    fn saved_custom_keymap_survives_owner_projection_and_restart_under_a_preset() {
        use oink_protocol::v1::{dto, schema::WireValue};
        let home = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from_with_device_name(home.path(), "source".into()).unwrap();
        let mut raw = write_transform(&oink_foundation::input_transform::InputTransform::initial("macos"));
        let saved = RawValue::Map(
            [
                (
                    1,
                    RawValue::Array(vec![
                        RawValue::Map([(1, RawValue::Uint(0xff0003)), (2, RawValue::Uint(0))].into()),
                        RawValue::Map([(1, RawValue::Uint(0x700e0)), (2, RawValue::Uint(0x700e6))].into()),
                        RawValue::Map([(1, RawValue::Uint(0x70039)), (2, RawValue::Uint(0))].into()),
                    ]),
                ),
                (2, RawValue::Uint(0)),
                (3, RawValue::Bool(true)),
                (4, RawValue::Bool(true)),
            ]
            .into(),
        );
        let RawValue::Map(transform) = &mut raw else {
            unreachable!()
        };
        let RawValue::Map(target) = transform.get_mut(&2).unwrap() else {
            unreachable!()
        };
        let RawValue::Map(keyboard) = target.get_mut(&2).unwrap() else {
            unreachable!()
        };
        keyboard.insert(4, saved);
        let typed = read_transform(dto::InputTransformValue::read(&raw).unwrap()).unwrap();
        assert_eq!(
            write_transform(&typed),
            raw,
            "saved custom data must survive the typed owner projection"
        );
        config.update(|s| s.input_transform = typed).unwrap();
        let reloaded = AgentConfig::load_from_with_device_name(home.path(), "source".into()).unwrap();
        assert_eq!(write_transform(&reloaded.settings.input_transform), raw);
        assert_eq!(reloaded.owner().value(LocalOption::InputTransform), Some(raw));
        let saved = reloaded.settings.input_transform.windows.keyboard.saved_custom.clone();
        let mut switched = reloaded;
        for preset in [
            oink_foundation::input_transform::KeyPreset::WinFeel,
            oink_foundation::input_transform::KeyPreset::MacFeel,
        ] {
            switched
                .update(|s| {
                    let active = oink_foundation::input_transform::KeyboardMapping::preset(preset, "macos", "windows");
                    s.input_transform.windows.keyboard.preset = active.preset;
                    s.input_transform.windows.keyboard.table = active.table;
                })
                .unwrap();
            switched = AgentConfig::load_from_with_device_name(home.path(), "source".into()).unwrap();
            assert_eq!(switched.settings.input_transform.windows.keyboard.saved_custom, saved);
        }
        // Subsequent custom edits use keyed patches inside the retained snapshot,
        // including removing an optional hold while keeping a disabled Fn choice.
        switched
            .update(|s| {
                let saved = s.input_transform.windows.keyboard.saved_custom.as_mut().unwrap();
                saved.table[0].to = oink_foundation::input::KeyCode::RIGHT_ALT;
                saved.table[1].to = oink_foundation::input::KeyCode::NONE;
                saved.caps_hold = None;
                saved.caps_split = false;
                saved.allow_fn = false;
            })
            .unwrap();
        let restarted = AgentConfig::load_from_with_device_name(home.path(), "source".into()).unwrap();
        assert_eq!(restarted.settings.input_transform, switched.settings.input_transform);
        let saved = restarted
            .settings
            .input_transform
            .windows
            .keyboard
            .saved_custom
            .unwrap();
        assert_eq!(saved.table[0].to, oink_foundation::input::KeyCode::RIGHT_ALT);
        assert_eq!(saved.table[1].to, oink_foundation::input::KeyCode::NONE);
        assert_eq!(saved.caps_hold, None);
        assert!(!saved.caps_split && !saved.allow_fn);
    }
    #[test]
    fn released_keymap_without_saved_custom_survives_owner_store_reload() {
        let home = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from_with_device_name(home.path(), "source".into()).unwrap();
        let keyboard: oink_foundation::input_transform::KeyboardMapping = serde_json::from_str(
            r#"{"preset":"custom","table":[{"from":458976,"to":458982}],"caps_tap_hold":{"tap":458977,"hold":458809}}"#,
        )
        .unwrap();
        config
            .update(|s| s.input_transform.windows.keyboard = keyboard.clone())
            .unwrap();
        let raw = config.owner().value(LocalOption::InputTransform);
        let restarted = AgentConfig::load_from_with_device_name(home.path(), "source".into()).unwrap();
        assert_eq!(restarted.settings.input_transform.windows.keyboard, keyboard);
        assert_eq!(restarted.owner().value(LocalOption::InputTransform), raw);
        assert!(
            restarted
                .settings
                .input_transform
                .windows
                .keyboard
                .saved_custom
                .is_none()
        );
    }
    #[test]
    fn received_cleanup_is_local_never_by_default_and_survives_restart() {
        use crate::api::ReceivedFilesCleanup as C;
        let home = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(home.path()).unwrap();
        assert_eq!(config.settings.received_files_cleanup, C::Never);
        let owner = config.owner().clone();
        config
            .update_local(&serde_json::json!({"received_files_cleanup":C::Days7}), |settings| {
                settings.received_files_cleanup = C::Days7
            })
            .unwrap();
        assert_eq!(config.owner(), &owner);
        assert_eq!(
            AgentConfig::load_from(home.path())
                .unwrap()
                .settings
                .received_files_cleanup,
            C::Days7
        );
        assert_eq!(stored(home.path())["local"]["received_files_cleanup"], "days7");
    }
    fn stored(dir: &Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(dir.join(SETTINGS_FILE)).unwrap()).unwrap()
    }
    #[test]
    fn removed_projection_preference_is_an_ignored_preserved_extra() {
        for value in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let mut config = AgentConfig::load_from(dir.path()).unwrap();
            config.save().unwrap();
            let mut raw = stored(dir.path());
            raw["local"]["layout_projection_enabled"] = value.into();
            let original = serde_json::to_vec(&raw).unwrap();
            std::fs::write(config.settings_path(), &original).unwrap();
            let mut loaded = AgentConfig::load_from(dir.path()).unwrap();
            assert!(
                serde_json::to_value(&loaded.settings)
                    .unwrap()
                    .get("layout_projection_enabled")
                    .is_none()
            );
            assert!(
                loaded
                    .settings_issues
                    .iter()
                    .any(|issue| issue.path == "settings.layout_projection_enabled"
                        && issue.kind == SettingsIssueKind::UnknownKey)
            );
            assert_eq!(std::fs::read(loaded.settings_path()).unwrap(), original);
            loaded.update(|settings| settings.confirm_quit = false).unwrap();
            assert_eq!(stored(dir.path())["local"]["layout_projection_enabled"], value);
            assert!(!AgentConfig::load_from(dir.path()).unwrap().settings.confirm_quit);
        }
    }
    #[test]
    fn consent_is_an_atomic_policy_record_not_default_following_preferences() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        config
            .update(|s| {
                s.reports = s
                    .reports
                    .change(0, oink_telemetry::Choice::Granted, oink_telemetry::POLICY_VERSION)
                    .unwrap()
            })
            .unwrap();
        assert!(AgentConfig::load_from(dir.path()).unwrap().settings.reports.granted());
        assert_eq!(
            stored(dir.path())["local"]["reports"]["policy_version"],
            oink_telemetry::POLICY_VERSION
        );
    }
    #[test]
    fn tagged_local_profile_remains_a_complete_typed_value() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        config
            .update(|s| {
                if let oink_routing::damping::DampingProfile::EnergyDepth { energy, .. } =
                    &mut s.routing.damping.platform_profile
                {
                    *energy = 0.012;
                }
            })
            .unwrap();
        let restored = AgentConfig::load_from(dir.path()).unwrap();
        assert_eq!(
            restored.settings.routing.damping.platform_profile,
            config.settings.routing.damping.platform_profile
        );
        assert!(stored(dir.path())["local"]["routing"]["damping"].get("rules").is_none());
    }
    #[test]
    fn fresh_audio_defaults_and_explicit_owner_choices_survive_unrelated_edits() {
        use oink_protocol::v1::{dto::LocalOption, schema::WireValue};
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        assert!(!config.settings.audio.forwarding_enabled);
        assert_eq!(
            config.owner().value(LocalOption::ForwardingEnabled),
            Some(false.write())
        );
        assert!(!config.settings_path().exists());
        for enabled in [true, false] {
            config.update(|s| s.audio.forwarding_enabled = enabled).unwrap();
            config.update(|s| s.onboarding_done = true).unwrap();
            config = AgentConfig::load_from(dir.path()).unwrap();
            assert_eq!(config.settings.audio.forwarding_enabled, enabled);
            assert_eq!(
                config.owner().value(LocalOption::ForwardingEnabled),
                Some(enabled.write())
            );
            assert!(stored(dir.path())["local"].get("audio").is_none());
        }
    }

    #[test]
    fn touchpad_owner_options_default_on_persist_independently_and_project_legacy() {
        use oink_protocol::v1::{dto::LocalOption as O, schema::WireValue};
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        for option in [O::TouchpadInjection, O::NativeMultifinger, O::MacScrollFeel] {
            assert_eq!(config.owner().value(option), Some(true.write()));
        }
        for parent in [false, true] {
            for native in [false, true] {
                for mac in [false, true] {
                    for (option, enabled) in [
                        (O::TouchpadInjection, parent),
                        (O::NativeMultifinger, native),
                        (O::MacScrollFeel, mac),
                    ] {
                        config
                            .edit_owner(option, config.owner().revision(option).unwrap(), enabled.write())
                            .unwrap();
                    }
                    config = AgentConfig::load_from(dir.path()).unwrap();
                    let s = &config.settings;
                    assert_eq!(
                        (
                            s.routing.touchpad_injection,
                            s.routing.native_multifinger,
                            s.routing.mac_scroll_feel
                        ),
                        (parent, native, mac)
                    );
                    assert_eq!(s.accept_contacts, parent && native);
                    for field in ["mac_scroll_feel", "native_multifinger", "touchpad_injection"] {
                        assert!(stored(dir.path())["local"]["routing"].get(field).is_none());
                    }
                }
            }
        }
    }
    #[test]
    fn touchpad_upgrade_preserves_old_off_and_legacy_scroll_choice() {
        use base64::Engine;
        use oink_protocol::v1::{RawValue, dto::LocalOption as O, schema::WireValue};
        for (legacy, mac) in [(false, false), (false, true), (true, false), (true, true)] {
            let dir = tempfile::tempdir().unwrap();
            let mut config = AgentConfig::load_from(dir.path()).unwrap();
            config.save().unwrap();
            let mut raw = stored(dir.path());
            let mut owner = oink_protocol::v1::cbor::decode(&config.owner().encode().unwrap(), 4 * 1024 * 1024)
                .unwrap()
                .as_map()
                .unwrap()
                .clone();
            if let Some(RawValue::Array(items)) = owner.get_mut(&2) {
                items.retain(|item| {
                    !item
                        .as_map()
                        .ok()
                        .and_then(|m| m.get(&1))
                        .and_then(|value| value.as_u64().ok())
                        .is_some_and(|n| (26..=28).contains(&n))
                });
                for item in items {
                    if let RawValue::Map(m) = item
                        && m.get(&1) == Some(&19u32.write())
                    {
                        m.insert(3, legacy.write());
                    }
                }
            }
            raw["device_options_cbor"] = base64::engine::general_purpose::STANDARD
                .encode(RawValue::Map(owner).encode(4 * 1024 * 1024).unwrap())
                .into();
            raw["local"]["routing"]["mac_scroll_feel"] = mac.into();
            std::fs::write(config.settings_path(), serde_json::to_vec(&raw).unwrap()).unwrap();
            let reloaded = AgentConfig::load_from(dir.path()).unwrap();
            assert!(reloaded.settings.routing.touchpad_injection);
            assert_eq!(reloaded.settings.routing.native_multifinger, legacy);
            assert_eq!(reloaded.settings.routing.mac_scroll_feel, !legacy || mac);
            assert_eq!(reloaded.owner().value(O::AcceptContacts), Some(legacy.write()));
            assert!(stored(dir.path())["local"]["routing"].get("mac_scroll_feel").is_none());
        }
    }
    #[test]
    fn mac_scroll_feel_defaults_on_and_ignores_retired_test_key() {
        for legacy in [None, Some(false), Some(true)] {
            let dir = tempfile::tempdir().unwrap();
            let mut config = AgentConfig::load_from(dir.path()).unwrap();
            config.save().unwrap();
            let mut raw = stored(dir.path());
            if let Some(value) = legacy {
                raw["local"] = serde_json::json!({"routing":{"test_mac_scroll":value}});
            }
            std::fs::write(config.settings_path(), serde_json::to_vec(&raw).unwrap()).unwrap();
            let mut config = AgentConfig::load_from(dir.path()).unwrap();
            assert!(config.settings.routing.mac_scroll_feel);
            config
                .update_local(&serde_json::json!({"routing":{"mac_scroll_feel":false}}), |s| {
                    s.routing.mac_scroll_feel = false;
                })
                .unwrap();
            let reloaded = AgentConfig::load_from(dir.path()).unwrap();
            assert!(!reloaded.settings.routing.mac_scroll_feel);
        }
    }

    #[test]
    fn injected_input_forwarding_defaults_off_and_persists_only_locally() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        assert!(!config.settings.routing.forward_injected_input);
        for forward in [true, false] {
            config
                .update_local(
                    &serde_json::json!({"routing":{"forward_injected_input":forward}}),
                    |s| {
                        s.routing.forward_injected_input = forward;
                    },
                )
                .unwrap();
            let reloaded = AgentConfig::load_from(dir.path()).unwrap();
            assert_eq!(reloaded.settings.routing.forward_injected_input, forward);
            assert_eq!(
                stored(dir.path())["local"]["routing"]["forward_injected_input"],
                forward
            );
            let mut raw = stored(dir.path());
            raw.as_object_mut().unwrap().remove("local");
            assert!(!raw.to_string().contains("forward_injected_input"));
        }
    }

    #[test]
    fn old_clipboard_format_keys_do_not_disable_formats_or_other_preferences() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        config.save().unwrap();
        let mut raw = stored(dir.path());
        raw["local"]["clipboard"] = serde_json::json!({
            "text":false,"rich_text":false,"images":false,"files":false,"custom":false,
            "receive":false,"send":true,"remove_ds_store":false
        });
        std::fs::write(config.settings_path(), serde_json::to_vec(&raw).unwrap()).unwrap();
        let mut loaded = AgentConfig::load_from(dir.path()).unwrap();
        let local = loaded.settings.clipboard.unwrap();
        assert!(!local.receive);
        assert!(local.send);
        assert!(!local.remove_ds_store);
        let wire = serde_json::to_value(crate::api::ClipboardSettings::from(local)).unwrap();
        let typed_local = serde_json::to_value(local).unwrap();
        for key in ["text", "rich_text", "images", "files", "custom"] {
            assert_eq!(wire[key], true);
            assert!(typed_local.get(key).is_none());
            assert!(
                loaded
                    .settings_issues
                    .iter()
                    .any(|issue| issue.path == format!("settings.clipboard.{key}")
                        && issue.kind == SettingsIssueKind::UnknownKey)
            );
        }
        loaded.update(|s| s.confirm_quit = false).unwrap();
        let reloaded = AgentConfig::load_from(dir.path()).unwrap();
        assert_eq!(reloaded.settings.clipboard, Some(local));
        assert!(!reloaded.settings.confirm_quit);
        assert_eq!(stored(dir.path())["schema"], 1);
    }

    #[test]
    fn ds_store_filter_defaults_on_and_persists_only_locally() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        assert!(config.settings.clipboard.unwrap().remove_ds_store);
        for enabled in [false, true] {
            config
                .update_local(
                    &serde_json::json!({"clipboard":{"remove_ds_store":enabled}}),
                    |settings| {
                        settings.clipboard.as_mut().unwrap().remove_ds_store = enabled;
                    },
                )
                .unwrap();
            let reloaded = AgentConfig::load_from(dir.path()).unwrap();
            assert_eq!(reloaded.settings.clipboard.unwrap().remove_ds_store, enabled);
            assert_eq!(stored(dir.path())["local"]["clipboard"]["remove_ds_store"], enabled);
            let mut raw = stored(dir.path());
            raw.as_object_mut().unwrap().remove("local");
            assert!(!raw.to_string().contains("remove_ds_store"));
        }
    }

    #[test]
    fn unrelated_save_does_not_pin_current_local_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        config.update(|s| s.onboarding_done = true).unwrap();
        assert_eq!(stored(dir.path())["local"], serde_json::json!({"onboarding_done":true}));
    }
    #[test]
    fn login_confirmation_survives_restart_without_persisting_system_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        assert!(!config.settings.launch_at_login_configured);
        assert!(!config.settings_path().exists());
        config.update(|s| s.launch_at_login_configured = true).unwrap();
        assert!(
            AgentConfig::load_from(dir.path())
                .unwrap()
                .settings
                .launch_at_login_configured
        );
        assert_eq!(
            stored(dir.path())["local"],
            serde_json::json!({"launch_at_login_configured":true})
        );
    }
    #[test]
    fn first_nested_edit_does_not_pin_sibling_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        config.save().unwrap();
        let mut raw = stored(dir.path());
        raw["local"] = serde_json::json!({});
        std::fs::write(config.settings_path(), serde_json::to_vec(&raw).unwrap()).unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        config
            .update(|s| s.audio.keep_local_playback = !s.audio.keep_local_playback)
            .unwrap();
        assert_eq!(
            stored(dir.path())["local"],
            serde_json::json!({"audio":{"keep_local_playback":config.settings.audio.keep_local_playback}})
        );
    }
    #[test]
    fn unset_follows_new_defaults_and_explicit_default_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        config
            .update_local(&serde_json::json!({"show_status_icon":true}), |_| {})
            .unwrap();
        let changed_default = Settings {
            show_status_icon: false,
            confirm_quit: false,
            ..Default::default()
        };
        let resolved: Settings =
            read::Reader::default().object(stored(dir.path())["local"].clone(), changed_default, "settings");
        assert!(resolved.show_status_icon);
        assert!(!resolved.confirm_quit);
    }
    #[test]
    fn local_patch_retains_existing_explicit_values_and_rolls_back_failed_save() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        config
            .update_local(&serde_json::json!({"show_status_icon":false}), |s| {
                s.show_status_icon = false
            })
            .unwrap();
        config
            .update_local(&serde_json::json!({"confirm_quit":true}), |_| {})
            .unwrap();
        assert_eq!(
            stored(dir.path())["local"],
            serde_json::json!({"show_status_icon":false,"confirm_quit":true})
        );
        let old = config.envelope.clone();
        std::fs::remove_file(config.settings_path()).unwrap();
        std::fs::create_dir(config.settings_path()).unwrap();
        assert!(
            config
                .update_local(&serde_json::json!({"confirm_quit":false}), |s| s.confirm_quit = false)
                .is_err()
        );
        assert!(config.settings.confirm_quit);
        assert_eq!(config.envelope, old);
    }
    #[test]
    fn fresh_defaults_have_one_owner_source_and_native_preferences() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        assert!(!config.settings_path().exists());
        let s = &config.settings;
        assert!(s.diagnostics_enabled);
        assert!(!s.reports.granted());
        assert!(s.clipboard_enabled);
        assert!(
            s.clipboard
                .as_ref()
                .is_some_and(|s| s.receive && s.send && s.remove_ds_store)
        );
        let hotkeys = s.hotkeys.as_ref().unwrap();
        assert_eq!(hotkeys, &crate::agent_api::default_hotkeys());
        assert!(
            hotkeys
                .iter()
                .all(|h| { h.enabled == (h.action != crate::api::HotkeyAction::ToggleMicrophoneMute) })
        );
        assert!(s.show_status_icon);
        assert!(s.accept_contacts);
        assert_eq!(s.game_mode, GameModePreference::Auto);
        assert_eq!(s.screen_preferences.device_gain, 1.0);
        assert_eq!(s.routing.event_frequency, s.event_frequency);
        assert_eq!(s.routing.cursor_trail_duration_ms, s.cursor_trail_duration_ms);
        assert_eq!(s.log_max_mb.bytes(), 1_024_000_000);
        assert_eq!(s.theme_accent.light.as_str(), "#FF6B8E");
        assert_eq!(s.theme_accent.dark.as_str(), "#FF6B8E");
        config.save().unwrap();
        let raw = stored(dir.path());
        assert_eq!(raw["schema"], 1);
        assert!(raw["local"].get("theme_accent").is_none());
        assert!(raw["device_options_cbor"].is_string());
        assert!(raw["local"]["audio"].get("forwarding_enabled").is_none());
        assert!(raw["local"]["routing"].get("mouse_mode").is_none());
        assert!(raw["local"]["routing"].get("event_frequency").is_none());
        assert!(raw["local"]["routing"].get("cursor_trail_duration_ms").is_none());
        assert!(raw["local"].get("gestures_enabled").is_none());
        assert!(raw["local"].get("device_name").is_none());
        let reloaded = AgentConfig::load_from(dir.path()).unwrap();
        assert_eq!(reloaded.settings, config.settings);
        assert_eq!(reloaded.owner, config.owner);
        assert!(!dir.path().join("audio/settings.json").exists());
    }
    #[test]
    fn explicit_accent_is_preserved_when_another_preference_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        let pair = serde_json::json!({"light":"#2E7BFF","dark":"#123456"});
        config
            .update_local(&serde_json::json!({"theme_accent":pair}), |s| {
                s.theme_accent = serde_json::from_value(pair.clone()).unwrap();
            })
            .unwrap();
        let mut reloaded = AgentConfig::load_from(dir.path()).unwrap();
        assert_eq!(reloaded.settings.theme_accent.light.as_str(), "#2E7BFF");
        assert_eq!(reloaded.settings.theme_accent.dark.as_str(), "#123456");
        reloaded
            .update_local(&serde_json::json!({"confirm_quit":false}), |s| s.confirm_quit = false)
            .unwrap();
        assert_eq!(stored(dir.path())["local"]["theme_accent"], pair);
    }
    #[test]
    fn unknown_fields_and_invalid_untouched_values_survive_local_edits() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        config.save().unwrap();
        let mut raw = stored(dir.path());
        raw["future_envelope"] = serde_json::json!({"nested":true});
        raw["local"]["theme_accent"]["future"] = serde_json::json!([1, 2, 3]);
        raw["local"]["diagnostics_enabled"] = serde_json::json!("unrecognized");
        raw["local"]["hotkeys"] = serde_json::to_value(crate::agent_api::default_hotkeys()).unwrap();
        raw["local"]["hotkeys"][0]["future"] = serde_json::json!(123);
        raw["local"]["hotkeys"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"action":"future","keys":[],"enabled":false}));
        let original = serde_json::to_vec(&raw).unwrap();
        std::fs::write(config.settings_path(), &original).unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        assert!(config.settings.diagnostics_enabled);
        assert_eq!(std::fs::read(config.settings_path()).unwrap(), original);
        config
            .update(|s| {
                s.log_max_mb = LogMaxMb::try_from(2048).unwrap();
                s.hotkeys.as_mut().unwrap()[0].enabled = false;
            })
            .unwrap();
        let saved = stored(dir.path());
        assert_eq!(saved["future_envelope"], raw["future_envelope"]);
        assert_eq!(saved["local"]["theme_accent"], raw["local"]["theme_accent"]);
        assert_eq!(saved["local"]["diagnostics_enabled"], "unrecognized");
        assert_eq!(saved["local"]["hotkeys"][0]["future"], 123);
        assert_eq!(
            saved["local"]["hotkeys"].as_array().unwrap().last().unwrap()["action"],
            "future"
        );
    }
    #[test]
    fn unsupported_or_corrupt_settings_are_not_loaded_or_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SETTINGS_FILE);
        for original in ["{}", "{invalid", "{\"schema\":2,\"local\":{}}"] {
            std::fs::write(&path, original).unwrap();
            assert!(AgentConfig::load_from(dir.path()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
    }
    #[test]
    fn owner_cas_and_disk_failure_are_transactional() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        config.save().unwrap();
        let option = LocalOption::ClipboardEnabled;
        let revision = config.owner().revision(option).unwrap();
        let result = config.edit_owner(option, revision, RawValue::Bool(false)).unwrap();
        assert_eq!(result.revision().unwrap(), revision + 1);
        assert!(!config.settings.clipboard_enabled);
        assert_eq!(
            config.edit_owner(option, revision, RawValue::Bool(true)),
            Err(ControlFailure::StaleRevision)
        );
        let same = config.edit_owner(option, revision + 1, RawValue::Bool(false)).unwrap();
        assert_eq!(same.revision().unwrap(), revision + 1);
        let before = config.clone();
        std::fs::remove_file(config.settings_path()).unwrap();
        std::fs::create_dir(config.settings_path()).unwrap();
        assert_eq!(
            config.edit_owner(option, revision + 1, RawValue::Bool(true)),
            Err(ControlFailure::PersistenceFailed)
        );
        assert_eq!(config.owner, before.owner);
        assert_eq!(config.settings, before.settings);
        assert!(config.update(|s| s.log_level = LogLevel::Trace).is_err());
        assert_eq!(config.settings, before.settings);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    #[test]
    fn initial_name_only_seeds_a_missing_store() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from_with_device_name(dir.path(), "first".into()).unwrap();
        assert_eq!(config.device_name(), "first");
        config.save().unwrap();
        let reloaded = AgentConfig::load_from_with_device_name(dir.path(), "second".into()).unwrap();
        assert_eq!(reloaded.device_name(), "first");
    }
    #[test]
    fn group_claim_projection_recovers_as_one_commit_and_retries_failed_switch() {
        use oink_foundation::{DeviceId, GroupId};
        use oink_protocol::v1::{dto, schema::WireValue};
        let home = tempfile::tempdir().unwrap();
        let me = DeviceId([1; 32]);
        let source = GroupId([2; 16]);
        let target = GroupId([3; 16]);
        let mut config = AgentConfig::load_from(home.path()).unwrap();
        let claim = dto::AudioExitClaim::from_raw(
            [
                (
                    1,
                    RawValue::Map([(1, 7u64.write()), (2, store::wire_device(me).write())].into()),
                ),
                (2, true.write()),
            ]
            .into(),
        )
        .unwrap();
        config
            .update_owner(|owner| {
                owner.switch_group(source, me)?;
                owner.commit_claims(
                    source,
                    me,
                    owner::GroupClaims {
                        watermark: None,
                        seen_counter: 7,
                        records: vec![owner::ClaimRecord {
                            owner: me,
                            revision: 3,
                            claim: Some(claim.clone()),
                        }],
                    },
                )
            })
            .unwrap();
        let before = std::fs::read(config.settings_path()).unwrap();
        let saved_owner = config.owner().clone();
        std::fs::remove_file(config.settings_path()).unwrap();
        std::fs::create_dir(config.settings_path()).unwrap();
        assert!(config.update_owner(|owner| owner.switch_group(target, me)).is_err());
        assert_eq!(config.owner(), &saved_owner);
        std::fs::remove_dir(config.settings_path()).unwrap();
        std::fs::write(config.settings_path(), before).unwrap();
        let mut restarted = AgentConfig::load_from(home.path()).unwrap();
        assert_eq!(restarted.owner(), &saved_owner);
        restarted.update_owner(|owner| owner.switch_group(target, me)).unwrap();
        let mut restarted = AgentConfig::load_from(home.path()).unwrap();
        assert_eq!(restarted.owner().active_group(), Some(target));
        assert_eq!(
            restarted.owner().value(LocalOption::AudioExitClaim),
            Some(RawValue::Null)
        );
        assert_eq!(restarted.owner().claims(source), saved_owner.claims(source));
        restarted.update_owner(|owner| owner.switch_group(source, me)).unwrap();
        let recovered = AgentConfig::load_from(home.path()).unwrap();
        assert_eq!(recovered.owner().active_group(), Some(source));
        assert_eq!(
            recovered.owner().value(LocalOption::AudioExitClaim),
            Some(claim.write())
        );
        assert_eq!(recovered.owner().revision(LocalOption::AudioExitClaim), Some(5));
        assert_eq!(recovered.owner().claims(source).seen_counter, 7);
    }
    #[test]
    fn new_screen_defaults_preserve_user_choices_and_unplugged_preferences() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        assert!(
            config
                .seed_screen_preferences([("physical", false), ("virtual", true)])
                .unwrap()
        );
        assert!(config.settings.screen_preferences.screens[0].enabled);
        assert!(!config.settings.screen_preferences.screens[1].enabled);
        config
            .update(|s| s.screen_preferences.screens[1].enabled = true)
            .unwrap();
        let revision = config.owner().revision(LocalOption::ScreenPreferences).unwrap();
        assert!(!config.seed_screen_preferences([("virtual", true)]).unwrap());
        assert_eq!(config.owner().revision(LocalOption::ScreenPreferences), Some(revision));
        let reloaded = AgentConfig::load_from(dir.path()).unwrap();
        assert_eq!(reloaded.settings.screen_preferences.screens.len(), 2);
        assert!(reloaded.settings.screen_preferences.screens[1].enabled);
    }
    #[test]
    fn typed_edit_bumps_only_changed_owner_options_and_keeps_other_values() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        config
            .update(|s| {
                s.routing.remote_scroll_speed = 2.0;
                s.audio.forwarding_volume_percent = 150;
                s.game_mode = GameModePreference::On;
            })
            .unwrap();
        assert_eq!(config.owner.revision(LocalOption::RemoteScrollSpeed), Some(2));
        assert_eq!(config.owner.revision(LocalOption::ForwardingControls), Some(2));
        assert_eq!(config.owner.revision(LocalOption::GameModePreference), Some(2));
        assert_eq!(config.owner.revision(LocalOption::MouseMode), Some(1));
        assert_eq!(AgentConfig::load_from(dir.path()).unwrap().settings, config.settings);
    }
    #[test]
    fn preview_is_read_only_and_matches_the_complete_committed_owner_baseline() {
        let home = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(home.path()).unwrap();
        config.save().unwrap();
        let disk = std::fs::read(config.settings_path()).unwrap();
        let original = config.clone();
        let mut settings = config.settings.clone();
        settings.routing.remote_scroll_speed = 2.0;
        settings.game_mode = GameModePreference::On;
        settings.screen_preferences.device_gain = 1.5;
        settings.screen_preferences.screens.push(ScreenPreference {
            display: "screen".into(),
            gain: 1.0,
            enabled: true,
            remote_gain: 2.5,
        });
        let preview = config.preview_preferences(&settings).unwrap();
        assert_eq!(config.settings, original.settings);
        assert_eq!(config.owner(), original.owner());
        assert_eq!(std::fs::read(config.settings_path()).unwrap(), disk);
        config.update(|candidate| *candidate = settings).unwrap();
        assert_eq!(config.owner().preferences(), preview);
        assert_eq!(
            AgentConfig::load_from(home.path()).unwrap().owner().preferences(),
            preview
        );

        let before = config.owner().clone();
        let mut invalid = config.settings.clone();
        invalid.routing.remote_scroll_speed = 0.0;
        assert!(config.preview_preferences(&invalid).is_err());
        assert_eq!(config.owner(), &before);
    }
    #[test]
    fn per_device_hotkey_overrides_persist_with_distinct_target_identity() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(dir.path()).unwrap();
        let mut keys: Vec<_> = [1u8, 2]
            .into_iter()
            .map(|seed| crate::api::Hotkey {
                action: crate::api::HotkeyAction::MovePointerTo,
                device: Some(oink_foundation::DeviceId([seed; 32]).into()),
                keys: vec!["Ctrl".into(), "Alt".into(), seed.to_string()],
                enabled: true,
            })
            .collect();
        config
            .update_local(&serde_json::json!({"hotkeys":&keys}), |s| {
                s.hotkeys = Some(keys.clone())
            })
            .unwrap();
        keys[1].keys[2] = "8".into();
        config
            .update_local(&serde_json::json!({"hotkeys":&keys}), |s| {
                s.hotkeys = Some(keys.clone())
            })
            .unwrap();
        assert_eq!(stored(dir.path())["local"]["hotkeys"], serde_json::json!(&keys));
        let bytes = std::fs::read(config.settings_path()).unwrap();
        let mut loaded = AgentConfig::load_from(dir.path()).unwrap();
        assert_eq!(loaded.settings.hotkeys.as_ref(), Some(&keys));
        assert_eq!(
            std::fs::read(config.settings_path()).unwrap(),
            bytes,
            "loading must not rewrite config"
        );
        let projected = crate::hotkeys::effective(loaded.settings.hotkeys.clone(), &Default::default());
        assert!(
            projected
                .iter()
                .any(|h| h.action == crate::api::HotkeyAction::ToggleMicrophoneMute && !h.enabled)
        );
        loaded
            .update_local(&serde_json::json!({"confirm_quit":false}), |s| s.confirm_quit = false)
            .unwrap();
        assert_eq!(stored(dir.path())["local"]["hotkeys"], serde_json::json!(&keys));
        assert_eq!(AgentConfig::load_from(dir.path()).unwrap().settings.hotkeys, Some(keys));
    }
}
