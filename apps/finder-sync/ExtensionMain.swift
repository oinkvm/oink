import Foundation
@_silgen_name("NSExtensionMain")
private func extensionMain(_ argc: Int32, _ argv: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> Int32
// Match the File Provider's C entry: Swift entry metadata causes recursive
// NSExtensionMain calls in ExtensionKit on macOS 26.6.2.
@_cdecl("OinkFinderSyncMain")
func finderMain(_ argc: Int32, _ argv: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> Int32 {
    extensionMain(argc, argv)
}
