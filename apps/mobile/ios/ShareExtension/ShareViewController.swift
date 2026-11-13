import Combine
import UIKit
import SwiftUI

final class ShareViewController: UIViewController {
    private var dismissShare: (() -> Void)?

    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        // A remotely hosted share sheet can disappear without forwarding
        // SwiftUI onDisappear (observed on iOS 26.6.1). UIKit still delivers
        // this callback, including an interactive sheet dismissal.
        dismissShare?()
        dismissShare = nil
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        let host = UIHostingController(rootView: ShareView(context: extensionContext, registerDismissal: { [weak self] in
            self?.dismissShare = $0
        }).oinkThemed())
        // The extension's view is the picker page (user 2026-11-12), so from the first frame it
        // is the page colour (`OinkPalette.bg`), not the host app showing through.
        host.view.backgroundColor = UIColor { traits in
            traits.userInterfaceStyle == .dark ? UIColor(red: 0x14 / 255, green: 0x16 / 255, blue: 0x1C / 255, alpha: 1) : .white
        }
        addChild(host)
        host.view.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(host.view)
        NSLayoutConstraint.activate([host.view.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            host.view.trailingAnchor.constraint(equalTo: view.trailingAnchor), host.view.topAnchor.constraint(equalTo: view.topAnchor),
            host.view.bottomAnchor.constraint(equalTo: view.bottomAnchor)])
        host.didMove(toParent: self)
    }
}

struct ShareView: View {
    let context: NSExtensionContext?
    let registerDismissal: (@escaping () -> Void) -> Void
    @ObservedObject private var core = CoreSession.shared
    /// Device first (native-rewrite.md「分享 / 打开方式先选设备」): the picker opens with the
    /// devices online last time while the session starts; then the send card.
    @StateObject private var picker = SharePickerModel(cached: CoreSession.shared.lastOnlineTargets(),
                                                       chinese: AppText.pick("zh", "en") == "zh")
    @State private var target: SendTarget?
    @State private var summary = ""
    @State private var uiState = ShareUiState(phase: .preparing, statusText: "正在发送…", progress: nil)
    @State private var knownBytes: UInt64?
    @State private var routingReady = false
    @State private var payload: SharePayload?
    @State private var staging: ShareStagingLease?
    @State private var preparing: Task<Void, Never>?
    @State private var sendStartedAt: Date?
    @State private var operation: Task<Void, Never>?
    @State private var sending = false
    @State private var results: [DeliveryResult] = []
    @State private var dismissed = false
    private var chinese: Bool { AppText.pick("zh", "en") == "zh" }

    private var showsPicker: Bool {
        guard routingReady, let targets = core.transferTargets, target == nil else { return false }
        // Wait for the initial round before concluding a small share has only phones available.
        if targets.searching, let knownBytes, knownBytes < 300_000_000 { return false }
        return true
    }

    var body: some View {
        // The system sheet is the picker page; a picked device lays the send card over it, and
        // the card's scrim dims only this view (user 2026-11-12).
        ZStack {
            if showsPicker {
            SharePickerView(model: picker, summary: summary, onClose: { cancel() }) { id in
                Task { @MainActor in
                    if let device = await picker.pick(id, confirm: { await core.confirmOnline($0.id) }) { start(device, waitForTarget: false) }
                }
            }
            // Under the card the page only shows through.
            .allowsHitTesting(target == nil)
            .accessibilityHidden(target != nil)
            } else if target == nil { ProgressView() }
            if let target {
                ShareCardView(state: uiState) {
                    // The extension has no navigation bar: the card carries its own small
                    // 取消 while sending and 关闭 after a failure (review 2026-11-11).
                    if uiState.phase == .failed {
                        Button(AppText.pick("关闭", "Close")) { cancel() }.buttonStyle(ShareCapsuleButtonStyle())
                    } else if uiState.phase != .succeeded {
                        Button(AppText.pick("取消", "Cancel")) { cancel() }.buttonStyle(ShareCapsuleButtonStyle())
                    }
                }
                .id(target.id)
            }
        }
        .onAppear {
            registerDismissal { dismiss() }
            prepare()
        }
        // The live list of this process's own session.
        .onReceive(core.$transferTargets.compactMap { $0 }.receive(on: DispatchQueue.main)) { apply($0) }
        // Progress is published apart from the session (CoreSession.progress); read it once set.
        .onReceive(core.progress.$revision.removeDuplicates().dropFirst().receive(on: DispatchQueue.main)) { _ in
            updateTransferProgress()
        }
        // The only other device is offline: the core holds the share until it is online (user
        // 2026-11-11). This extension must stay open meanwhile (closing it ends the share).
        .onReceive(core.$sharesWaitingForTarget.receive(on: DispatchQueue.main)) { waiting in
            guard sending, let device = target, let operationID = core.activeShareOperationID else { return }
            uiState = uiState.targetWaiting(waiting.contains(operationID), sendingText: sendingText(device))
        }
        .onReceive(NotificationCenter.default.publisher(for: .NSExtensionHostDidEnterBackground)) { _ in
            operation?.cancel()
            preparing?.cancel()
            let running = operation
            let idle = payload != nil && !sending
            stopSession {
                await running?.value
                guard idle else { return }
                cleanup()
                payload = nil
                uiState = ShareUiState(phase: .failed, statusText: "分享已停止，请重新分享。", progress: nil, toPhone: target.map { !$0.isComputer } ?? false)
            }
        }
    }

    /// New targets: the picker follows them; one other device alone skips the choice (the
    /// page under the card lists just that device), and the core waits for it while it is offline.
    private func apply(_ targets: SendTargets) {
        if target == nil, targets.otherDevices == 1, let only = targets.onlyDevice {
            picker.showOnly(only)
            start(only, waitForTarget: true)
        }
        if target == nil, routingReady,
           let automatic = automaticShareTarget(targets, bytes: knownBytes) { start(automatic, waitForTarget: false) }
        picker.update(online: targets.online, roundFinished: !targets.searching)
    }

    private func dismiss() {
        guard !dismissed else { return }
        dismissed = true
        operation?.cancel()
        preparing?.cancel()
        let running = operation
        stopSession { await running?.value; cleanup() }
    }
    /// Extensions cannot use UIApplication background tasks, so dismissal and
    /// host backgrounding would otherwise let the system suspend this process
    /// mid-shutdown while it still holds Rust's cross-process owner lock.
    private func stopSession(then finish: @escaping @MainActor () async -> Void) {
        let shutdown = Task { @MainActor in
            await core.prepareSuspend()
            await finish()
        }
        ProcessInfo.processInfo.performExpiringActivity(withReason: "Oink 关闭分享会话") { expired in
            guard !expired else { return }
            // The activity lasts until this block returns; it runs on a
            // background queue, so waiting here does not block the main actor.
            let done = DispatchSemaphore(value: 0)
            Task { await shutdown.value; done.signal() }
            done.wait()
        }
    }
    /// Copies what is shared and starts the session while the picker shows.
    private func prepare() {
        let providers = (context?.inputItems as? [NSExtensionItem] ?? []).flatMap { $0.attachments ?? [] }
        summary = ShareStaging.summary(providers, chinese: chinese)
        preparing = Task { @MainActor in
            do {
                let staging = try SharedStore.staging()
                self.staging = staging
                async let connection: Void = core.prepareShare()
                knownBytes = await ShareStaging.knownBytes(providers)
                routingReady = true
                if let targets = core.transferTargets { apply(targets) }
                let payload = try await ShareStaging.collect(providers, directory: staging.directory)
                try await connection
                try Task.checkCancellation()
                self.payload = payload
                // The app owns the session: its list, once.
                if let targets = await core.handoffTransferTargets() ?? core.transferTargets { apply(targets) }
            } catch {
                await core.prepareSuspend(); cleanup()
                target = target ?? SendTarget(id: "", name: "", isComputer: true)
                uiState = ShareUiState(phase: .failed, statusText: error.localizedDescription, progress: nil)
            }
        }
    }
    /// A device was picked (or is the only one): the card, then the send once the
    /// shared items are copied.
    private func start(_ device: SendTarget, waitForTarget: Bool) {
        guard target == nil else { return }
        target = device
        uiState = ShareUiState(phase: .preparing, statusText: sendingText(device), progress: nil, toPhone: !device.isComputer)
        Task { @MainActor in
            await preparing?.value
            if payload != nil { send(to: device, waitForTarget: waitForTarget) }
        }
    }
    private func sendingText(_ device: SendTarget) -> String {
        AppText.pick("正在发送给 \(device.name)…", "Sending to \(device.name)…")
    }
    private func send(to device: SendTarget, waitForTarget: Bool) {
        guard let payload else { return }
        sending = true
        sendStartedAt = Date()
        let phone = !device.isComputer
        uiState = ShareUiState(phase: .sending, statusText: sendingText(device), progress: nil, toPhone: phone)
        operation = Task { @MainActor in
            do {
                // `share` returns only when the core reported the share as
                // Sent; every failure throws its reason, so a failed share never
                // reads as done.
                results = try await core.share(payload, target: device.id, waitForTarget: waitForTarget)
                await core.prepareSuspend()
                cleanup()
                self.payload = nil
                let seconds = ShareStatusFormatter.elapsed(seconds: Date().timeIntervalSince(sendStartedAt ?? Date()))
                let text = phone ? AppText.pick("已发送给 \(device.name)，耗时 \(seconds) 秒", "Sent to \(device.name) in \(seconds) s")
                    : AppText.pick("已完成，耗时 \(seconds) 秒", "Done in \(seconds) s")
                uiState = ShareUiState(phase: .succeeded, statusText: text, progress: 1, toPhone: phone)
                try? await Task.sleep(for: .milliseconds(800))
                if !Task.isCancelled { context?.completeRequest(returningItems: nil) }
            } catch {
                await core.prepareSuspend()
                cleanup()
                self.payload = nil
                uiState = ShareUiState(phase: .failed, statusText: error.localizedDescription, progress: uiState.progress, toPhone: phone)
            }
            sending = false
        }
    }

    private func updateTransferProgress() {
        guard sending, uiState.phase != .waiting, let device = target, let operationID = core.activeShareOperationID,
              let progress = core.transferOperations[operationID] else { return }
        let base = sendingText(device)
        let text = progress.bytesPerSecond == 0 ? base : "\(base) \(ShareStatusFormatter.rate(bytesPerSecond: Double(progress.bytesPerSecond)))"
        uiState = ShareUiState(phase: .sending, statusText: text, progress: progress.fraction, toPhone: !device.isComputer)
    }
    private func cancel() {
        operation?.cancel()
        preparing?.cancel()
        dismissed = true
        let running = operation
        stopSession {
            await running?.value
            cleanup()
            context?.cancelRequest(withError: CancellationError())
        }
    }
    private func cleanup() {
        staging?.remove()
        staging = nil
    }
}
