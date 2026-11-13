#if DEBUG
import CoreImage
import Foundation
import UIKit

/// `-oink-ui-scene transfer-perf` (DEBUG only): a conversation of real size for the
/// scroll measurement (UITests/TransferScrollUITests). About 420 records: short
/// texts (some with links), long texts, image batches, file batches, single files,
/// folders and large photos / screenshots, downloaded (the original) or not (the PC's
/// thumbnail). Pictures are real JPEG / PNG files on disk with their own paths, so the
/// list decodes them the way it decodes received ones. Deterministic: the same records
/// on every run.
enum TransferPerfFixture {
    static let recordCount = 420

    /// Newest first, like `TransferModel.messages`.
    static func messages() -> [TransferMessage] {
        let files = PerfFiles()
        var rng = SplitMix(seed: 0x0A1E_0111)
        var list: [TransferMessage] = []
        let start = Date().addingTimeInterval(-3 * 86_400)
        var n = 0

        func sender() -> (id: String, mine: Bool, proxy: String?) {
            switch rng.below(10) {
            case 0..<4: ("self", true, nil)
            case 4..<7: ("fixture-windowsPc", false, nil)
            case 7..<9: ("fixture-macMini", false, nil)
            default: ("fixture-android", false, "fixture-windowsPc")
            }
        }
        func record(_ kind: TransferMessageKind, _ who: (id: String, mine: Bool, proxy: String?), text: String? = nil,
                    name: String? = nil, size: Int64? = nil, stored: Bool = true, path: String? = nil,
                    operation: String? = nil, items: Int? = nil) -> TransferMessage {
            n += 1
            let attachment = kind != .text
            var message = TransferMessage(
                id: String(100_000 + n), createdAt: start.addingTimeInterval(Double(n) * 397), senderDeviceID: who.proxy ?? who.id,
                senderName: who.id, offerID: nil, sourceBoot: nil, operationID: operation, itemID: nil, representationID: nil,
                originSenderID: who.id, proxyDeviceID: who.proxy, proxyName: who.proxy, kind: kind, text: text,
                contentID: attachment ? "perf-content-\(n)" : nil, localPath: stored ? path : nil,
                attachmentName: name, byteSize: size, uti: nil,
                state: attachment ? (stored || who.mine ? .stored : .placeholder) : nil,
                availability: "Ready", isMine: who.mine, delivery: who.mine ? .sent : nil)
            message.itemCount = items
            return message
        }
        func picture(_ who: (id: String, mine: Bool, proxy: String?), stored: Bool, operation: String? = nil) -> TransferMessage {
            let kind = rng.below(10)
            let portrait = kind >= 6 && kind < 8
            let (path, name, size): (String, String, Int64) = kind < 6
                ? (files.photo(n + 1, portrait: false), "IMG_\(4000 + n).JPG", 3_800_000)
                : kind < 8 ? (files.photo(n + 1, portrait: true), "IMG_\(4000 + n).JPG", 3_400_000)
                : (files.screenshot(n + 1), "屏幕截图 \(n).png", 1_900_000)
            var message = record(.image, who, name: name, size: size, stored: stored || who.mine, path: path, operation: operation)
            // Received and not opened: the PC's thumbnail (images download only when opened).
            if message.state != .stored {
                message.imagePreview = .thumbnail
                message.thumbnailPath = files.thumbnail(n, portrait: portrait)
                message.imageAspect = portrait ? 3.0 / 4 : kind < 6 ? 4.0 / 3 : 1.6
            } else {
                message.imagePreview = .original
            }
            return message
        }
        func file(_ who: (id: String, mine: Bool, proxy: String?), stored: Bool, operation: String? = nil) -> TransferMessage {
            let names = ["Q4 设计评审.pdf", "会议纪要.docx", "排期表.xlsx", "图标资源（全部尺寸）.zip", "release-notes.md",
                         "crash-2026-11-09.log", "合同扫描件.pdf", "build-111.ipa", "Oink-0.3.5.dmg", "数据导出.csv"]
            return record(.file, who, name: names[rng.below(names.count)], size: Int64(20_000 + rng.below(40_000_000)),
                          stored: stored, path: nil, operation: operation)
        }

        while list.count < recordCount {
            let who = sender()
            switch rng.below(100) {
            case 0..<34:
                list.append(record(.text, who, text: shortText(&rng)))
            case 34..<44:
                list.append(record(.text, who, text: longText(&rng)))
            case 44..<54:
                list.append(picture(who, stored: true))
            case 54..<59:
                list.append(picture(who, stored: false))
            case 59..<70:
                let operation = "perf-op-\(n)"
                let stored = rng.below(3) > 0
                for _ in 0..<(3 + rng.below(7)) { list.append(picture(who, stored: stored && rng.below(5) > 0, operation: operation)) }
            case 70..<75:
                let operation = "perf-op-\(n)"
                for _ in 0..<(2 + rng.below(5)) {
                    switch rng.below(4) {
                    case 0: list.append(picture(who, stored: true, operation: operation))
                    case 1: list.append(record(.folder, who, name: "现场照片 \(n)", operation: operation, items: 4 + rng.below(40)))
                    default: list.append(file(who, stored: rng.below(2) == 0, operation: operation))
                    }
                }
            case 75..<93:
                list.append(file(who, stored: rng.below(3) == 0))
            default:
                list.append(record(.folder, who, name: ["项目素材", "现场照片", "导出 2026-11", "截图"][rng.below(4)] + " \(n)",
                                   items: 3 + rng.below(80)))
            }
        }
        return list.reversed()
    }

    private static let phrases = [
        "收到，我在手机上确认一下。", "这版界面已经整理好了，发给你看看。", "晚上把会议纪要发我一下", "好的", "👌",
        "刚才那个文件打不开，能再发一次吗？", "地址：上海市徐汇区漕溪北路 88 号 12 楼", "验证码 482913，五分钟内有效",
        "Meeting moved to 3:30, same room.", "git checkout mobile-v040 && cargo test -p oink-mobile-core",
        "参考 https://developer.apple.com/documentation/swiftui/lazyvstack 里的说明", "明天上午十点前给我就行",
        "已经在 Office-PC 上打开了，没问题。", "这个链接 https://example.com/share/8f3a2c 需要登录",
    ]

    private static func shortText(_ rng: inout SplitMix) -> String {
        (0...rng.below(3)).map { _ in phrases[rng.below(phrases.count)] }.joined(separator: rng.below(2) == 0 ? "\n" : " ")
    }

    /// 12–70 paragraphs: notes, logs and pasted prose, the kind of text copied on a PC.
    private static func longText(_ rng: inout SplitMix) -> String {
        let lines = [
            "0.4.0 移动端联调记录：传输页长文本默认只显示前 10 行，第 8–10 行渐隐，点「查看更多」每次多显示 20 行。",
            "2026-11-09T21:14:03.512Z INFO oink_mobile_core::transfer: offer accepted op=7f3a items=6 bytes=18600000",
            "多文件聚合：一次发送或接收的一批文件显示为一叠，左右划动切换最上面那张，点「展开」恢复成普通消息。",
            "The quick brown fox jumps over the lazy dog while the build queue waits for the windowsPc slot to free up.",
            "详见 https://github.com/oinkvm/oink/issues/412 和设计稿里的 transfer-stack 一节。",
            "- [ ] 横屏和大折叠内屏的截图还没补",
            "    let entries = TransferLayout.entries(ordered, expanded: expanded)",
            "滚出一屏之后回来会恢复折叠，恢复时列表不跳动；深色模式也检查过了。",
        ]
        return (0..<(12 + rng.below(59))).map { _ in lines[rng.below(lines.count)] }.joined(separator: "\n")
    }

    /// Deterministic generator (SplitMix64).
    struct SplitMix {
        var state: UInt64
        init(seed: UInt64) { state = seed }
        mutating func next() -> UInt64 {
            state &+= 0x9E37_79B9_7F4A_7C15
            var z = state
            z = (z ^ (z >> 30)) &* 0xBF58_476D_1CE4_E5B9
            z = (z ^ (z >> 27)) &* 0x94D0_49BB_1331_11EB
            return z ^ (z >> 31)
        }
        mutating func below(_ bound: Int) -> Int { Int(next() % UInt64(bound)) }
    }

    /// Picture files in Caches/oink-perf-fixture: one generated original per shape
    /// (12 MP landscape / portrait photo with photo-like noise, a 2880 × 1800 screenshot),
    /// cloned to a path per record so every record is a separate file to decode.
    private struct PerfFiles {
        let dir: URL

        init() {
            dir = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0].appendingPathComponent("oink-perf-fixture")
            try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        }

        func photo(_ n: Int, portrait: Bool) -> String {
            clone(base: portrait ? "base-portrait.jpg" : "base-landscape.jpg", to: "photo-\(n).jpg") {
                Self.photoData(size: portrait ? CGSize(width: 3024, height: 4032) : CGSize(width: 4032, height: 3024))
            }
        }

        /// A PC thumbnail (long side 640 px), one file per record.
        func thumbnail(_ n: Int, portrait: Bool) -> String {
            clone(base: portrait ? "thumb-portrait.jpg" : "thumb-landscape.jpg", to: "thumb-\(n).jpg") {
                Self.photoData(size: portrait ? CGSize(width: 480, height: 640) : CGSize(width: 640, height: 480))
            }
        }

        func screenshot(_ n: Int) -> String {
            clone(base: "base-screen.png", to: "screen-\(n).png") { Self.screenshotData() }
        }

        private func clone(base: String, to name: String, make: () -> Data?) -> String {
            let source = dir.appendingPathComponent(base), target = dir.appendingPathComponent(name)
            if !FileManager.default.fileExists(atPath: source.path), let data = make() { try? data.write(to: source) }
            if !FileManager.default.fileExists(atPath: target.path) { try? FileManager.default.copyItem(at: source, to: target) }
            return target.path
        }

        private static func photoData(size: CGSize) -> Data? {
            let rect = CGRect(origin: .zero, size: size)
            guard let noise = CIFilter(name: "CIRandomGenerator")?.outputImage?.cropped(to: rect) else { return nil }
            let grain = noise.applyingGaussianBlur(sigma: 1.2).cropped(to: rect)
                .applyingFilter("CIColorMatrix", parameters: ["inputAVector": CIVector(x: 0, y: 0, z: 0, w: 0.35)])
            let sky = CIFilter(name: "CILinearGradient", parameters: [
                "inputPoint0": CIVector(x: 0, y: 0), "inputPoint1": CIVector(x: size.width, y: size.height),
                "inputColor0": CIColor(red: 0.55, green: 0.68, blue: 0.62), "inputColor1": CIColor(red: 0.95, green: 0.78, blue: 0.70),
            ])?.outputImage?.cropped(to: rect)
            guard let sky, let cg = CIContext().createCGImage(grain.composited(over: sky), from: rect) else { return nil }
            return UIImage(cgImage: cg).jpegData(compressionQuality: 0.88)
        }

        private static func screenshotData() -> Data? {
            let size = CGSize(width: 2880, height: 1800)
            let format = UIGraphicsImageRendererFormat()
            format.scale = 1
            return UIGraphicsImageRenderer(size: size, format: format).image { context in
                UIColor(white: 0.96, alpha: 1).setFill(); context.fill(CGRect(origin: .zero, size: size))
                UIColor(red: 0.18, green: 0.2, blue: 0.25, alpha: 1).setFill(); context.fill(CGRect(x: 0, y: 0, width: 520, height: 1800))
                let attributes: [NSAttributedString.Key: Any] = [.font: UIFont.systemFont(ofSize: 30), .foregroundColor: UIColor.darkGray]
                for row in 0..<40 {
                    ("\(row + 1)  let entries = TransferLayout.entries(ordered, expanded: expanded) // 传输页滚动" as NSString)
                        .draw(at: CGPoint(x: 580, y: 40 + row * 43), withAttributes: attributes)
                }
            }.pngData()
        }
    }
}
#endif
