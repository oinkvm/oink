import SwiftUI

@main
struct OinkApp: App {
    init() {
        #if DEBUG
        // Design-state screenshots: `-oink-theme <accent>` and `-oink-scheme <light|dark|system>`.
        let args = ProcessInfo.processInfo.arguments
        if let index = args.firstIndex(of: "-oink-theme"), let value = args.dropFirst(index + 1).first,
           OinkPalette.Accent(rawValue: value) != nil {
            OinkPrefs.shared.set(value, forKey: OinkPrefs.accent)
        }
        // `-oink-icon <accent>`: stores the theme accent like the Settings swatch does (icon checks).
        if let index = args.firstIndex(of: "-oink-icon"), let value = args.dropFirst(index + 1).first,
           OinkPalette.Accent(rawValue: value) != nil {
            OinkPrefs.shared.set(value, forKey: OinkPrefs.accent)
        }
        if DesignScene.current != nil {
            // Every design state starts from the default preferences.
            for key in [OinkPrefs.language, OinkPrefs.homeTab, OinkPrefs.micAutoOff,
                        OinkPrefs.scheme, OinkPrefs.speakerAutoOff, OinkPrefs.echoCancel, SpeakerActions.preferenceKey, "transferProxy"] {
                UserDefaults.standard.removeObject(forKey: key)
            }
        }
        #endif
        // Unselected tab items use the mockup's secondary text colour (--c-text2); the
        // selected one follows the theme accent through .tint.
        UITabBar.appearance().unselectedItemTintColor = UIColor { traits in
            traits.userInterfaceStyle == .dark ? UIColor(red: 0xA7 / 255, green: 0xAD / 255, blue: 0xBA / 255, alpha: 1)
                : UIColor(red: 0x5F / 255, green: 0x60 / 255, blue: 0x6A / 255, alpha: 1)
        }
        OinkShortcuts.updateAppShortcutParameters()
    }
    /// Supplies the orientations the control page's 屏幕方向 allows.
    @UIApplicationDelegateAdaptor(OinkAppDelegate.self) private var appDelegate
    @Environment(\.scenePhase) private var scenePhase
    @StateObject private var core = CoreSession.shared
    @StateObject private var lifecycle = AppLifecycle()
    @AppStorage(OinkPrefs.accent, store: OinkPrefs.shared) private var accent = "pink"
    @AppStorage(OinkPrefs.scheme) private var scheme = "system"

    var body: some Scene {
        WindowGroup {
            MainView()
                .environmentObject(core)
                .oinkThemed()
                .preferredColorScheme(DesignScene.forcedScheme ?? (scheme == "light" ? .light : scheme == "dark" ? .dark : nil))
                .onChange(of: accent, initial: true) { _, value in
                    #if DEBUG
                    if DesignScene.current != nil { return }
                    #endif
                    IconAppearanceController.apply(themeColor: value)
                    // Control Center toggles and Live Activities are tinted with the accent.
                    core.audioControls?.refresh()
                }
                .onChange(of: scenePhase, initial: true) { _, phase in
                    lifecycle.update(phase, core: core)
                    // setAlternateIconName needs an active app; re-apply once active.
                    #if DEBUG
                    if DesignScene.current != nil { return }
                    #endif
                    if phase == .active { IconAppearanceController.apply(themeColor: accent) }
                }
        }
    }
}
