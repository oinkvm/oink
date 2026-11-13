import SwiftUI
import UIKit

/// Top of the device and transfer pages: why no computer can connect.
/// - The network session did not start (review M27): the typed reason and a
///   retry; while the share extension owns the session the app reconnects by
///   itself when it ends.
/// - iOS denies Local Network access (review L17).
struct SessionBanner: View {
    @EnvironmentObject private var core: CoreSession
    @Environment(\.oink) private var oink
    @State private var retrying = false

    var body: some View {
        if let problem = core.sessionProblem {
            banner(title: problem.title, message: problem.message) {
                Button(AppText.pick("重试", "Retry")) {
                    guard !retrying else { return }
                    retrying = true
                    Task {
                        try? await core.refreshDevices()
                        retrying = false
                    }
                }
                .disabled(retrying)
            }
        } else if core.discoveryUnavailable {
            banner(title: "无法搜索附近的设备", message: "请在「设置 > Oink」中允许访问本地网络，否则无法连接电脑。") {
                Button("打开设置") {
                    if let url = URL(string: UIApplication.openSettingsURLString) { UIApplication.shared.open(url) }
                }
            }
        }
    }

    private func banner(title: String, message: String, @ViewBuilder action: () -> some View) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Lucide(name: "alert-circle", size: 18, color: oink.badInk).padding(.top, 1)
            VStack(alignment: .leading, spacing: 4) {
                Text(title).font(.oink(14, .semibold)).foregroundStyle(oink.ink)
                Text(message)
                    .font(.oink(12.5)).foregroundStyle(oink.muted)
                    .fixedSize(horizontal: false, vertical: true)
                action()
                    .font(.oink(13, .semibold))
                    .padding(.top, 4)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(14)
        .background(oink.fill, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
        .padding(.horizontal, 16)
        .padding(.vertical, 8)
    }
}
