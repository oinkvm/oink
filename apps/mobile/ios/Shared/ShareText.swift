import UIKit
import UniformTypeIdentifiers

/// The text formats of a shared item. User 2026-11-12: the phone sends every
/// format it has: plain text, HTML and RTF go together as one text item, and
/// the computer puts them all on its clipboard. (This replaces the earlier
/// rule of the same day that sent only the plain text.)
enum ShareText {
    /// The first registered type of each format, by the provider's order.
    struct Types: Equatable {
        var plain: String?
        var html: String?
        var rtf: String?
        /// RTFD (a package, or its flat form): flattened to RTF before
        /// sending; its attachments (pictures) are left out. RTF is what the
        /// computers' clipboards take as rich text next to HTML.
        var rtfd: String?
    }

    static func types(_ registered: [String]) -> Types {
        let types = registered.compactMap { identifier in UTType(identifier).map { (identifier, $0) } }
        func first(_ matches: (UTType) -> Bool) -> String? { types.first { matches($0.1) }?.0 }
        return Types(
            plain: first { $0.conforms(to: .plainText) },
            html: first { $0.conforms(to: .html) },
            rtf: first { $0.conforms(to: .rtf) },
            rtfd: first { $0.conforms(to: .rtfd) || $0.conforms(to: .flatRTFD) })
    }

    /// The rich text of the shared texts (one entry per text, nil where it
    /// has none): HTML when every text has it (joined like the texts), so it
    /// says what the plain text says; RTF only for a single text (RTF
    /// documents do not join).
    static func rich(htmls: [String?], rtfs: [String?]) -> (html: String?, rtf: String?) {
        let html = htmls.isEmpty || htmls.contains(nil) ? nil : htmls.compactMap { $0 }.joined(separator: "<br>")
        return (html, rtfs.count == 1 ? rtfs[0] : nil)
    }

    /// RTF from RTFD data (flat RTFD, or a package's file wrapper data).
    static func rtf(fromRTFD data: Data) -> String? {
        guard let text = try? NSAttributedString(data: data, options: [.documentType: NSAttributedString.DocumentType.rtfd],
                                                 documentAttributes: nil),
              let rtf = try? text.data(from: NSRange(location: 0, length: text.length),
                                       documentAttributes: [.documentType: NSAttributedString.DocumentType.rtf])
        else { return nil }
        return String(data: rtf, encoding: .utf8) ?? String(data: rtf, encoding: .ascii)
    }

    /// The plain text of RTF, for an item that has no plain form.
    static func plainText(fromRTF rtf: String) -> String? {
        guard let data = rtf.data(using: .utf8) else { return nil }
        return (try? NSAttributedString(data: data, options: [.documentType: NSAttributedString.DocumentType.rtf],
                                        documentAttributes: nil))?.string
    }

    /// The plain text of HTML, for an item that has no plain form: tags
    /// removed (block ends become line breaks), the common character
    /// references decoded. The system HTML importer is not used: it runs
    /// WebKit and may load remote resources.
    static func plainText(fromHTML html: String) -> String {
        var text = html
        for dropped in ["script", "style", "head", "title"] {
            text = text.replacingOccurrences(of: "<\(dropped)\\b[^>]*>[\\s\\S]*?</\(dropped)\\s*>", with: "",
                                             options: [.regularExpression, .caseInsensitive])
        }
        text = text.replacingOccurrences(of: "<!--[\\s\\S]*?-->", with: "", options: .regularExpression)
        text = text.replacingOccurrences(of: "<(br|/p|/div|/li|/h[1-6]|/tr)\\b[^>]*>", with: "\n",
                                         options: [.regularExpression, .caseInsensitive])
        text = text.replacingOccurrences(of: "<[^>]*>", with: "", options: .regularExpression)
        for (entity, character) in [("&nbsp;", "\u{a0}"), ("&lt;", "<"), ("&gt;", ">"), ("&quot;", "\""), ("&#39;", "'"),
                                    ("&amp;", "&")] {
            text = text.replacingOccurrences(of: entity, with: character)
        }
        return text.trimmingCharacters(in: .whitespacesAndNewlines)
    }
}
