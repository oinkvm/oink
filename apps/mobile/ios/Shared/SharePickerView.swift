import SwiftUI
import UIKit

/// The share / open-with device picker page (mockup `SharePicker.jsx` `embedded`,
/// `share-ext.css` `.pk-page`): 「发送到」, what is shared, then 电脑 / 移动设备 with the
/// devices. iOS already presents the share extension in a sheet of its own, so this page
/// fills that sheet from its top: no scrim, no second half sheet and no drag handle inside
/// it, and a 32 pt close button top right instead (user 2026-11-12). The send card is laid
/// over this page (`ShareView`). Used by the share extension and by the app's DEBUG design states.
struct SharePickerView: View {
    @Environment(\.oink) private var oink
    @ObservedObject var model: SharePickerModel
    /// 「3 张照片」…
    let summary: String
    /// The close button: ends the share without sending.
    let onClose: () -> Void
    let onPick: (String) -> Void
    @State private var toast: ToastModel?

    var body: some View {
        ZStack(alignment: .top) {
            page
            if let toast {
                ToastView(toast: toast).padding(.top, 72)
                    .transition(.move(edge: .top).combined(with: .opacity))
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .background(oink.bg.ignoresSafeArea())
        .onChange(of: model.notice) { _, notice in
            guard let notice else { return }
            Haptics.error()
            withAnimation(.timingCurve(0.22, 1, 0.36, 1, duration: 0.26)) { toast = ToastModel(text: notice.text) }
            Task { @MainActor in
                try? await Task.sleep(for: .seconds(2))
                if toast?.text == notice.text { withAnimation(.easeOut(duration: 0.2)) { toast = nil } }
            }
        }
    }

    private var page: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 6) {
                Text(AppText.pick("发送到", "Send to")).font(.oink(20, .semibold)).foregroundStyle(oink.ink)
                Text(summary).font(.oink(13)).foregroundStyle(oink.muted)
            }
            // Clear of the close button.
            .padding(.trailing, 40)
            .padding(.top, 24).padding(.bottom, 14)
            if model.searching {
                HStack(spacing: 8) {
                    OinkDots()
                    Text(AppText.pick("正在查找设备…", "Looking for devices…")).font(.oink(12.5))
                }
                .foregroundStyle(oink.faint)
                .frame(minHeight: 36)
                .padding(.leading, 4).padding(.bottom, 2)
            }
            if model.isEmpty {
                empty
                Spacer(minLength: 0)
            } else {
                // The list scrolls inside the system sheet.
                ScrollView {
                    VStack(alignment: .leading, spacing: 14) {
                        section(AppText.pick("电脑", "Computers"), model.computers)
                        section(AppText.pick("移动设备", "Mobile devices"), model.phones)
                    }
                    .padding(.bottom, 40)
                }
                .scrollBounceBehavior(.basedOnSize)
            }
        }
        .padding(.horizontal, 24)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        // Top right, 18 pt in (share-ext.css `.pk-close`); it replaces the drag handle.
        .overlay(alignment: .topTrailing) {
            Button(action: onClose) {
                Lucide(name: "x", size: 17, stroke: "2_6", color: oink.text2)
                    .frame(width: 32, height: 32)
                    .background(oink.chip, in: Circle())
                    .contentShape(Circle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel(AppText.pick("关闭", "Close"))
            .accessibilityIdentifier("share-picker-close")
            .padding(.top, 18).padding(.trailing, 18)
        }
    }

    @ViewBuilder private func section(_ title: String, _ rows: [SharePickerModel.Row]) -> some View {
        if !rows.isEmpty {
            VStack(alignment: .leading, spacing: 0) {
                GroupHeading(title: title, count: rows.count).frame(minHeight: 36).padding(.bottom, 4)
                ForEach(Array(rows.enumerated()), id: \.element.id) { index, row in
                    let busy = model.connecting == row.id
                    Button { onPick(row.id) } label: { rowLabel(row, busy: busy) }
                        .buttonStyle(RowButtonStyle())
                        .disabled(busy)
                        .overlay(alignment: .top) { if index > 0 { Hairline().padding(.leading, 46) } }
                        .accessibilityLabel(busy ? AppText.pick("正在连接 \(row.device.name)", "Connecting to \(row.device.name)")
                                            : "\(row.device.name)，\(row.confirmed ? AppText.pick("在线", "Online") : AppText.pick("上次在线", "Recently online"))")
                }
            }
        }
    }

    private func rowLabel(_ row: SharePickerModel.Row, busy: Bool) -> some View {
        HStack(spacing: 14) {
            PlatformLogo(platform: DevicePlatform(os: row.device.os, name: row.device.name))
            VStack(alignment: .leading, spacing: 3) {
                Text(row.device.name).font(.oink(17, .semibold)).kerning(-0.2).foregroundStyle(oink.ink)
                    .multilineTextAlignment(.leading)
                if row.confirmed {
                    HStack(spacing: 6) {
                        Circle().fill(oink.good).frame(width: 6, height: 6)
                        Text(AppText.pick("在线", "Online"))
                    }
                    .font(.oink(13)).foregroundStyle(oink.goodInk)
                } else {
                    Text(AppText.pick("上次在线", "Recently online")).font(.oink(13)).foregroundStyle(oink.faint)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            // The system spinner while the device is confirmed (review 2026-11-11).
            if busy { ProgressView().controlSize(.regular).padding(.trailing, 6) }
        }
        .padding(.vertical, 10)
        .frame(minHeight: 68)
        .contentShape(Rectangle())
    }

    private var empty: some View {
        VStack(spacing: 0) {
            ZStack {
                Circle().fill(oink.accentSoft)
                Lucide(name: "monitor-smartphone", size: 30, color: oink.accentInk)
            }
            .frame(width: 72, height: 72)
            Text(AppText.pick("没有在线的设备", "No devices online"))
                .font(.oink(16, .semibold)).foregroundStyle(oink.ink)
                .padding(.top, 16)
            Text(AppText.pick("请在其它设备上打开 Oink", "Open Oink on another device"))
                .font(.oink(13)).foregroundStyle(oink.muted)
                .multilineTextAlignment(.center)
                .padding(.top, 6)
        }
        .frame(maxWidth: .infinity)
        .padding(.top, 36)
    }
}
