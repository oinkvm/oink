import SwiftUI
import UIKit

/// 本机信息 dialog of the add page (mockup `SelfInfo` in an iOS-style dialog): the first two
/// addresses with「查看全部 N 个 ›」, the QUIC port and the device ID. Tapping a value copies it
/// (toast「已复制 …」). It is a custom dialog because a system alert cannot hold copyable rows.
struct InfoDialog: View {
    @EnvironmentObject private var core: CoreSession
    @Environment(\.oink) private var oink
    @Environment(\.dismiss) private var dismiss
    @State private var all: Bool
    @State private var shown = false
    @StateObject private var toasts = ToastCenter()

    init(showAll: Bool = false) { _all = State(initialValue: showAll) }

    var body: some View {
        let info = core.localIdentity
        let addresses = all ? info.addresses : Array(info.addresses.prefix(2))
        ZStack {
            oink.overlay.ignoresSafeArea()
                .onTapGesture { close() }
            VStack(spacing: 0) {
                Text("本机信息").font(.oink(17, .semibold)).foregroundStyle(oink.ink)
                    .padding(.top, 24).padding(.bottom, 8).padding(.horizontal, 22)
                Grid(alignment: .topLeading, horizontalSpacing: 16, verticalSpacing: 6) {
                    GridRow {
                        label("本机 IP")
                        VStack(alignment: .trailing, spacing: 0) {
                            ForEach(addresses, id: \.self) { value($0) }
                            if !all && info.addresses.count > 2 {
                                Button { Haptics.selection(); withAnimation(.easeOut(duration: 0.2)) { all = true } } label: {
                                    HStack(spacing: 1) {
                                        Text("查看全部 \(info.addresses.count) 个").font(.oink(12.5, .semibold))
                                        Lucide(name: "chevron-right", size: 14, color: oink.accentInk)
                                    }
                                    .foregroundStyle(oink.accentInk).frame(height: 28).padding(.horizontal, 4)
                                }
                                .buttonStyle(.plain)
                                .padding(.top, 2).padding(.trailing, -4)
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .trailing)
                    }
                    GridRow { label("本机端口"); value(info.port.map(String.init) ?? "—").frame(maxWidth: .infinity, alignment: .trailing) }
                    GridRow { label("本机 ID"); value(info.fingerprint.isEmpty ? "—" : info.fingerprint).frame(maxWidth: .infinity, alignment: .trailing) }
                }
                .padding(.horizontal, 20)
                Text("点按任一项即可复制").font(.oink(12)).foregroundStyle(oink.faint)
                    .padding(.top, 10).padding(.bottom, 14)
                Hairline()
                Button("知道了") { close() }
                    .font(.oink(16, .medium)).foregroundStyle(oink.accentInk)
                    .frame(maxWidth: .infinity, minHeight: 50)
                    .contentShape(Rectangle())
                    .buttonStyle(RowButtonStyle())
            }
            .frame(width: 300)
            .background(oink.elev, in: RoundedRectangle(cornerRadius: 24, style: .continuous))
            .overlay { RoundedRectangle(cornerRadius: 24, style: .continuous).strokeBorder(oink.line, lineWidth: 1) }
            .clipShape(RoundedRectangle(cornerRadius: 24, style: .continuous))
            .scaleEffect(shown ? 1 : 1.06)
            .opacity(shown ? 1 : 0)
        }
        .overlay(alignment: .top) {
            if let toast = toasts.toast { ToastView(toast: toast).padding(.top, 72) }
        }
        .onAppear { withAnimation(.timingCurve(0.22, 1, 0.36, 1, duration: 0.24)) { shown = true } }
    }

    private func close() { dismiss() }

    private func label(_ text: String) -> some View {
        Text(text).font(.oink(13.5)).foregroundStyle(oink.muted).padding(.top, 6)
    }

    private func value(_ text: String) -> some View {
        Button {
            UIPasteboard.general.string = text
            Haptics.selection()
            toasts.show("已复制 \(text)")
        } label: {
            HStack(spacing: 6) {
                Text(text).font(.oink(13.5).monospacedDigit()).foregroundStyle(oink.ink).lineLimit(1).truncationMode(.middle)
                Lucide(name: "copy", size: 13, color: oink.faint)
            }
            .padding(.horizontal, 6).frame(minHeight: 32)
            .contentShape(Rectangle())
        }
        .buttonStyle(RowButtonStyle())
        .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
        .padding(.trailing, -6)
        .accessibilityLabel("\(text)，点按复制")
    }
}
