//! The rendering device owns trail appearance, including imported seam paths.
use super::*;

impl Node {
    pub(crate) fn resolved_accent(&self) -> [u8; 3] {
        let config = lock(&self.inner.config);
        config
            .settings
            .theme_accent
            .resolved(
                config.settings.theme_mode,
                self.inner.system_dark.load(Ordering::Acquire),
            )
            .rgb()
    }

    pub(crate) fn apply_trail_appearance(&self) {
        // Read while holding the settings lock through application so concurrent
        // saves/appearance callbacks cannot leave an older accent on the renderer.
        let config = lock(&self.inner.config);
        let rgb = config
            .settings
            .theme_accent
            .resolved(
                config.settings.theme_mode,
                self.inner.system_dark.load(Ordering::Acquire),
            )
            .rgb();
        if let Some(trail) = self.inner.cursor_trail.get() {
            trail.set_trail_color(rgb);
        }
        crate::aim::set_follow_trail_color(self, rgb);
    }

    #[cfg(any(target_os = "macos", windows))]
    fn system_appearance_changed(&self, dark: bool) {
        self.inner.system_dark.store(dark, Ordering::Release);
        self.apply_trail_appearance();
        self.apply_notification_preferences();
    }

    pub(super) fn watch_trail_appearance(&self) {
        #[cfg(any(target_os = "macos", windows))]
        {
            let weak = Arc::downgrade(&self.inner);
            let watch = move || {
                crate::appearance::watch_system_theme(move |dark| {
                    if let Some(inner) = weak.upgrade() {
                        Node { inner }.system_appearance_changed(dark);
                    }
                })
            };
            #[cfg(windows)]
            self.system_appearance_changed(crate::appearance::system_dark());
            watch();
        }
    }
}

#[cfg(all(test, debug_assertions, any(target_os = "macos", windows)))]
mod tests {
    use super::*;
    use crate::{
        api::AgentApi,
        config::{ThemeAccent, ThemeColor, ThemeMode},
    };
    use oink_test_support::input::FakeInput;

    #[tokio::test]
    async fn saved_preferences_and_system_changes_recolour_both_local_renderers() {
        let home = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::load_from(home.path()).unwrap();
        let colors = ThemeAccent {
            light: ThemeColor::try_from("#12ABCD".to_owned()).unwrap(),
            dark: ThemeColor::try_from("#E73681".to_owned()).unwrap(),
        };
        config
            .update(|settings| settings.theme_accent = colors.clone())
            .unwrap();
        let node = Node::prepare(NodeOptions {
            net: oink_test_support::network_config(home.path().to_path_buf(), "synthetic", 0),
            input: Arc::new(FakeInput::new()),
            displays: Vec::new(),
            display_backend: None,
            clipboard: None,
            mode_switch_in_callback: true,
            capture_reports_key_repeat: false,
            cursor_trail: false,
        })
        .unwrap();
        // Native handles remain parked: test the real settings/application path
        // without starting windows, capturing input or simulating native acceptance.
        let trail = oink_locator::CursorTrail::platform().unwrap();
        let follow = oink_locator::CursorTrail::platform().unwrap();
        node.inner.cursor_trail.set(trail.clone()).unwrap();
        crate::aim::set_follow_trail(&node, Some(follow.clone()));
        let check = |rgb| {
            assert_eq!(trail.trail_color(), rgb);
            assert_eq!(follow.trail_color(), rgb);
        };
        node.apply_trail_appearance();
        check(colors.light.rgb());
        node.system_appearance_changed(true);
        check(colors.dark.rgb());
        let mut prefs = node.ui_preferences().unwrap();
        prefs.theme_mode = ThemeMode::Light;
        node.set_ui_preferences(prefs.clone().into()).unwrap();
        check(colors.light.rgb());
        node.system_appearance_changed(false);
        check(colors.light.rgb());
        prefs.theme_mode = ThemeMode::Dark;
        node.set_ui_preferences(prefs.clone().into()).unwrap();
        check(colors.dark.rgb());
        prefs.theme_accent.dark = ThemeColor::try_from("#123456".to_owned()).unwrap();
        node.set_ui_preferences(prefs.clone().into()).unwrap();
        check([18, 52, 86]);
        prefs.theme_mode = ThemeMode::System;
        node.set_ui_preferences(prefs.into()).unwrap();
        check(colors.light.rgb());
        node.system_appearance_changed(true);
        check([18, 52, 86]);
        assert_eq!(
            AgentConfig::load_from(home.path())
                .unwrap()
                .settings
                .theme_accent
                .dark
                .rgb(),
            [18, 52, 86]
        );
    }
}
