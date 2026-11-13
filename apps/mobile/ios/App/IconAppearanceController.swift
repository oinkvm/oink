import UIKit

/// Applies the user's accent to the Home Screen icon using Apple's alternate
/// icon API. SpringBoard selects the light, dark or tinted rendition from the
/// matching AppIcon asset appearance, so light/dark follows the system.
///
/// The alternate icon sets are compiled only because project.yml lists them in
/// ASSETCATALOG_COMPILER_ALTERNATE_APP_ICON_NAMES; actool then writes the
/// CFBundleAlternateIcons entries keyed by the icon set name ("AppIcon-<accent>").
@MainActor
enum IconAppearanceController {
    static func apply(themeColor: String) {
        guard UIApplication.shared.supportsAlternateIcons,
              UIApplication.shared.applicationState == .active else { return }
        let alternateName: String?
        switch themeColor {
        case "orange", "violet", "blue", "teal": alternateName = "AppIcon-\(themeColor)"
        default: alternateName = nil // The primary AppIcon-pink.
        }
        guard UIApplication.shared.alternateIconName != alternateName else { return }
        UIApplication.shared.setAlternateIconName(alternateName) { error in
            if let error {
                NSLog("Oink could not change Home Screen icon to %@: %@", alternateName ?? "primary", error.localizedDescription)
            }
        }
    }
}
