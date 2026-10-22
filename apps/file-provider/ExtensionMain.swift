import Foundation
import Darwin

@_silgen_name("NSExtensionMain")
private func extensionMain(_ argc: Int32, _ argv: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> Int32

// Use a separate Mach-O entry symbol, selected by the release linker. On
// macOS 26.6.2 ExtensionKit reads __TEXT,__swift5_entry and calls its Swift main
// callback. Swift @main therefore recurses through NSExtensionMain back into
// itself before File Provider can answer any request. A C entry without Swift
// entry metadata leaves that callback absent, preserving the principal-class path.
// Only the exact uninstall argument takes the sandbox self-cleanup path.
@_cdecl("OinkFileProviderMain")
func providerMain(_ argc: Int32, _ argv: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> Int32 {
    if Array(CommandLine.arguments.dropFirst()) == ["clear-uninstall-data"] {
        // Run the exact sandboxed appex image under its own code identity.
        // This also works without an enabled or registered provider domain.
        do { try clearOwnProviderContainer(); exit(0) }
        catch { exit(1) }
    }
    return extensionMain(argc, argv)
}
