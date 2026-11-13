import SwiftUI

/// The `-oink-ui-scene` launch argument (DEBUG screenshot fixtures only).
enum DesignScene {
    static var current: String? {
        #if DEBUG
        let args = ProcessInfo.processInfo.arguments
        guard let index = args.firstIndex(of: "-oink-ui-scene") else { return nil }
        return args.dropFirst(index + 1).first
        #else
        return nil
        #endif
    }
    static func `is`(_ id: String) -> Bool { current == id }
    static func hasPrefix(_ prefix: String) -> Bool { current?.hasPrefix(prefix) == true }

    /// `-oink-scheme light|dark`: the system appearance of the screenshot (the stored
    /// 深浅色 preference stays「跟随系统」).
    static var forcedScheme: ColorScheme? {
        #if DEBUG
        let args = ProcessInfo.processInfo.arguments
        guard let index = args.firstIndex(of: "-oink-scheme"), let value = args.dropFirst(index + 1).first else { return nil }
        return value == "dark" ? .dark : value == "light" ? .light : nil
        #else
        return nil
        #endif
    }
}
