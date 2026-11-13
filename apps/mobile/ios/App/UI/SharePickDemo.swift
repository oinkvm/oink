#if DEBUG
import SwiftUI

/// DEBUG design states of the share / open-with device picker (`-oink-ui-scene share-pick-*`,
/// mockup scenes.js SHARE_PICK_SCENES / share-ext-*), with the picker page and the card the
/// share extension uses: the card lies over the page (user 2026-11-12); the example host app
/// shows once the page is closed. `share-pick-flow` runs the flow on fixed answers (UI
/// tests): of the cached devices Office-PC and Android are confirmed online after 2 s, MacBook-Pro
/// and iPhone are not; a picked device gets the card, which ends sent after 1.5 s.
/// `share-pick-waiting`: the group's only other device is offline, so the card waits over a
/// page listing just that device.
struct SharePickDemo: View {
    let scene: String
    @StateObject private var model = SharePickerModel(cached: Self.cached, chinese: AppText.pick("zh", "en") == "zh")
    @State private var card: ShareUiState?
    @State private var closed = false

    static let office = SendTarget(id: "office", name: "Office-PC", isComputer: true, os: "windows")
    static let macbook = SendTarget(id: "macbook", name: "MacBook-Pro", isComputer: true, os: "macos")
    static let android = SendTarget(id: "android", name: "Android", isComputer: false, os: "android")
    static let iphone = SendTarget(id: "iphone", name: "iPhone", isComputer: false, os: "ios")
    static let ipad = SendTarget(id: "ipad", name: "iPad", isComputer: false, os: "ipados")
    static let cached = [office, macbook, android, iphone]
    private static let online: Set<String> = ["office", "android"]

    var body: some View {
        ZStack {
            ShareDemoHost()
            if !closed {
                SharePickerView(model: model, summary: AppText.pick("3 张照片", "3 photos"), onClose: { closed = true }) { id in
                    Task { @MainActor in
                        if let device = await model.pick(id, confirm: confirm) { start(device) }
                    }
                }
                .allowsHitTesting(card == nil)
                .accessibilityHidden(card != nil)
                if let card {
                    ShareCardView(state: card, freezeT: scene == "share-pick-flow" ? nil : 0.30) {
                        if card.phase == .failed {
                            Button(AppText.pick("关闭", "Close")) { closed = true }.buttonStyle(ShareCapsuleButtonStyle())
                        } else if card.phase != .succeeded {
                            Button(AppText.pick("取消", "Cancel")) { closed = true }.buttonStyle(ShareCapsuleButtonStyle())
                        }
                    }
                }
            }
        }
        .task { await stage() }
    }

    private func confirm(_ device: SendTarget) async -> Bool {
        try? await Task.sleep(for: .milliseconds(2000))
        return Self.online.contains(device.id)
    }

    private func start(_ device: SendTarget) {
        let to = device.name
        card = ShareUiState(phase: .sending, statusText: AppText.pick("正在发送给 \(to)…", "Sending to \(to)…"), progress: 0.3,
                            toPhone: !device.isComputer)
        Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(1500))
            guard !closed else { return }
            card = ShareUiState(phase: .succeeded, statusText: AppText.pick("已发送给 \(to)，耗时 1.52 秒", "Sent to \(to) in 1.52 s"),
                                progress: 1, toPhone: !device.isComputer)
        }
    }

    /// The fixed design states.
    private func stage() async {
        let rate = ShareStatusFormatter.rate(bytesPerSecond: 2_470_000)
        switch scene {
        case "share-pick-connecting":
            // The confirmation never ends here: the row keeps its spinner.
            _ = await model.pick("office") { _ in try? await Task.sleep(for: .seconds(3600)); return true }
        case "share-pick-offline":
            _ = await model.pick("macbook") { _ in false }
        case "share-pick-live":
            model.update(online: [Self.office, Self.ipad, Self.android], roundFinished: true)
        case "share-pick-empty":
            model.update(online: [], roundFinished: true)
        case "share-pick-sending", "share-pick-sent", "share-pick-failed":
            // The card lies over the live list (mockup share-ext-*).
            model.update(online: [Self.office, Self.ipad, Self.android], roundFinished: true)
            card = Self.card(scene, rate: rate)
        case "share-pick-waiting":
            // The first round still runs (mockup share-ext-waiting).
            model.showOnly(SendTarget(id: "macbook", name: "MacBook-Pro", isComputer: true, os: "macos", online: false))
            card = ShareUiState(phase: .waiting, statusText: ShareUiState.waitingText, progress: nil)
        default:
            break
        }
    }

    private static func card(_ scene: String, rate: String) -> ShareUiState? {
        switch scene {
        case "share-pick-sending":
            ShareUiState(phase: .sending, statusText: AppText.pick("正在发送给 Android… \(rate)", "Sending to Android… \(rate)"),
                         progress: 0.52, toPhone: true)
        case "share-pick-sent":
            ShareUiState(phase: .succeeded, statusText: AppText.pick("已发送给 Android，耗时 6.42 秒", "Sent to Android in 6.42 s"),
                         progress: 1, toPhone: true)
        case "share-pick-failed":
            ShareUiState(phase: .failed, statusText: AppText.pick("对方已断开，发送失败", "Disconnected, send failed"),
                         progress: 0.52, toPhone: true)
        default:
            nil
        }
    }
}
#endif
