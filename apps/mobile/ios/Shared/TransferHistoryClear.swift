import Foundation

/// Settings › 传输 › 清理传输记录 (settings-and-pairing.md §3.3, user 2026-11-11).
/// The row is grey while this phone has no transfer record; tapping it asks
/// for confirmation, and 清理 deletes the finished records through the core
/// (transfers still moving stay) and names the outcome in a toast.
@MainActor
final class TransferHistoryClear: ObservableObject {
    @Published private(set) var hasRecords = false
    @Published private(set) var clearing = false
    /// The confirm dialog is up.
    @Published var confirming = false
    private let hasHistory: @MainActor () -> Bool
    private let clear: @MainActor () throws -> Void

    init(hasHistory: @escaping @MainActor () -> Bool, clear: @escaping @MainActor () throws -> Void) {
        self.hasHistory = hasHistory
        self.clear = clear
    }

    var disabled: Bool { clearing || !hasRecords }

    func refresh() { hasRecords = hasHistory() }

    /// The row was tapped.
    func request() {
        if !disabled { confirming = true }
    }

    /// 清理 in the confirm dialog. Returns the toast text.
    func confirm() -> String {
        confirming = false
        clearing = true
        defer {
            clearing = false
            refresh()
        }
        do {
            try clear()
            return "已清理传输记录"
        } catch {
            return error.oinkMessage
        }
    }
}
