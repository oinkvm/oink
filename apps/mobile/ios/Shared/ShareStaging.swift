import Foundation
import UniformTypeIdentifiers

/// File representations are copied before the provider callback returns. No full-image decode.
enum ShareStaging {
    /// Inspect metadata before staging. An image provider without a file URL keeps an unknown size,
    /// including cloud-only photos: downloading it must not silently change the picker decision.
    static func knownBytes(_ providers: [NSItemProvider]) async -> UInt64? {
        var total: UInt64 = 0
        var texts: [String] = []
        var htmls: [String?] = []
        var rtfs: [String?] = []
        for provider in providers {
            if provider.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier) {
                guard let value = try? await load(provider, type: UTType.fileURL.identifier), let url = value as? URL else { return nil }
                let access = url.startAccessingSecurityScopedResource(); defer { if access { url.stopAccessingSecurityScopedResource() } }
                guard let size = totalBytes(SharePayload(files: [url])) else { return nil }
                let sum = total.addingReportingOverflow(size); if sum.overflow { return nil }; total = sum.partialValue
            } else if provider.hasItemConformingToTypeIdentifier(UTType.image.identifier) { return nil }
            else if provider.hasItemConformingToTypeIdentifier(UTType.url.identifier) {
                guard let value = try? await load(provider, type: UTType.url.identifier), let url = value as? URL else { return nil }
                texts.append(url.absoluteString); htmls.append(nil); rtfs.append(nil)
            } else if provider.hasItemConformingToTypeIdentifier(UTType.text.identifier) {
                guard let parts = try? await textParts(provider) else { return nil }
                texts.append(parts.0); htmls.append(parts.1); rtfs.append(parts.2)
            } else { return nil }
        }
        let (html, rtf) = ShareText.rich(htmls: htmls, rtfs: rtfs)
        guard let textBytes = totalBytes(SharePayload(text: texts.joined(separator: "\n"), html: html, rtf: rtf)) else { return nil }
        let sum = total.addingReportingOverflow(textBytes)
        return sum.overflow ? nil : sum.partialValue
    }
    static func totalBytes(_ payload: SharePayload) -> UInt64? {
        var total = UInt64((payload.text?.utf8.count ?? 0) + (payload.html?.utf8.count ?? 0) + (payload.rtf?.utf8.count ?? 0))
        for url in payload.files {
            let files: [URL]
            guard let values = try? url.resourceValues(forKeys: [.isDirectoryKey, .fileSizeKey]) else { return nil }
            if values.isDirectory == true {
                guard let scan = FileManager.default.enumerator(at: url, includingPropertiesForKeys: [.isDirectoryKey, .fileSizeKey]) else { return nil }
                files = scan.compactMap { $0 as? URL }
            } else { files = [url] }
            for file in files {
                guard let values = try? file.resourceValues(forKeys: [.isDirectoryKey, .fileSizeKey]) else { return nil }
                if values.isDirectory == true { continue }
                guard let size = values.fileSize, size >= 0 else { return nil }
                let sum = total.addingReportingOverflow(UInt64(size)); if sum.overflow { return nil }; total = sum.partialValue
            }
        }
        return total
    }

    /// The picker's second line, from what the providers offer (before copying anything).
    static func summary(_ providers: [NSItemProvider], chinese: Bool) -> String {
        let images = providers.filter { $0.hasItemConformingToTypeIdentifier(UTType.image.identifier) }.count
        let texts = providers.filter {
            !$0.hasItemConformingToTypeIdentifier(UTType.image.identifier) && !$0.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier)
                && ($0.hasItemConformingToTypeIdentifier(UTType.url.identifier) || $0.hasItemConformingToTypeIdentifier(UTType.text.identifier))
        }.count
        return ShareSummary.text(images: images, files: providers.count - images - texts, text: texts > 0, chinese: chinese)
    }

    static func collect(_ providers: [NSItemProvider], directory: URL) async throws -> SharePayload {
        // Every file below is a copy in our staging directory: the core takes
        // it over (moves it into its store) instead of copying it again.
        var payload = SharePayload(staged: true)
        payload.sensitivity = ShareSensitivity.sourceTypes(providers.flatMap(\.registeredTypeIdentifiers))
        var texts: [String] = []
        // Per text, its rich formats (nil where it has none).
        var htmls: [String?] = []
        var rtfs: [String?] = []
        for (index, provider) in providers.enumerated() {
            try Task.checkCancellation()
            if provider.hasItemConformingToTypeIdentifier(UTType.image.identifier) {
                let type = provider.registeredTypeIdentifiers.first(where: { UTType($0)?.conforms(to: .image) == true }) ?? UTType.data.identifier
                payload.files.append(try await copy(provider, type: type, directory: directory, index: index))
            } else if provider.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier) {
                payload.files.append(try await copyFileURL(provider, directory: directory, index: index))
            } else if provider.hasItemConformingToTypeIdentifier(UTType.url.identifier) {
                let item = try await load(provider, type: UTType.url.identifier)
                guard let url = item as? URL else { throw OinkFailure(message: "无法读取分享的链接。") }
                texts.append(url.absoluteString)
                htmls.append(nil)
                rtfs.append(nil)
            } else if provider.hasItemConformingToTypeIdentifier(UTType.text.identifier) {
                let parts = try await textParts(provider)
                texts.append(parts.0); htmls.append(parts.1); rtfs.append(parts.2)
            } else {
                let type = provider.registeredTypeIdentifiers.first ?? UTType.data.identifier
                payload.files.append(try await copy(provider, type: type, directory: directory, index: index))
            }
        }
        try Task.checkCancellation()
        payload.text = texts.isEmpty ? nil : texts.joined(separator: "\n")
        (payload.html, payload.rtf) = ShareText.rich(htmls: htmls, rtfs: rtfs)
        return payload
    }

    /// Use the same portable formats for the size decision and the eventual send.
    private static func textParts(_ provider: NSItemProvider) async throws -> (String, String?, String?) {
        let types = ShareText.types(provider.registeredTypeIdentifiers)
        var html: String?
        if let type = types.html { html = try await loadText(provider, type: type) }
        var rtf: String?
        if let type = types.rtf { rtf = try await loadText(provider, type: type) }
        if rtf == nil, let rtfd = types.rtfd {
            rtf = try await loadData(provider, type: rtfd).flatMap(ShareText.rtf(fromRTFD:))
        }
        let plain: String?
        if let type = types.plain { plain = try await loadText(provider, type: type) }
        else { plain = rtf.flatMap(ShareText.plainText(fromRTF:)) ?? html.map(ShareText.plainText(fromHTML:)) }
        guard let plain else { throw OinkFailure(message: "无法读取分享的文本。") }
        return (plain, html, rtf)
    }

    /// A text representation as a string: the provider gives a String, UTF-8
    /// data, or a file URL holding it. Nil when it is none of these.
    private static func loadText(_ provider: NSItemProvider, type: String) async throws -> String? {
        let item = try await load(provider, type: type)
        if let text = item as? String { return text }
        if let attributed = item as? NSAttributedString { return attributed.string }
        guard let data = try await loadData(item) else { return nil }
        return String(data: data, encoding: .utf8) ?? String(data: data, encoding: .ascii)
    }

    private static func loadData(_ provider: NSItemProvider, type: String) async throws -> Data? {
        try await loadData(load(provider, type: type))
    }

    private static func loadData(_ item: NSSecureCoding) async throws -> Data? {
        if let data = item as? Data { return data }
        if let url = item as? URL {
            let access = url.startAccessingSecurityScopedResource()
            defer { if access { url.stopAccessingSecurityScopedResource() } }
            // An RTFD package is a directory: its file wrapper's flat form.
            if (try? url.resourceValues(forKeys: [.isDirectoryKey]))?.isDirectory == true {
                return try FileWrapper(url: url).serializedRepresentation
            }
            return try Data(contentsOf: url)
        }
        return nil
    }
    private static func load(_ provider: NSItemProvider, type: String) async throws -> NSSecureCoding {
        try await withCheckedThrowingContinuation { continuation in
            provider.loadItem(forTypeIdentifier: type) { item, error in
                if let error { continuation.resume(throwing: error) }
                else if let item { continuation.resume(returning: item) }
                else { continuation.resume(throwing: OinkFailure(message: "分享内容为空。")) }
            }
        }
    }
    private static func copyFileURL(_ provider: NSItemProvider, directory: URL, index: Int) async throws -> URL {
        try await withCheckedThrowingContinuation { continuation in
            provider.loadItem(forTypeIdentifier: UTType.fileURL.identifier) { item, error in
                do {
                    if let error { throw error }
                    guard let url = item as? URL else { throw OinkFailure(message: "无法读取分享文件地址。") }
                    let access = url.startAccessingSecurityScopedResource()
                    defer { if access { url.stopAccessingSecurityScopedResource() } }
                    let folder = directory.appendingPathComponent(String(index), isDirectory: true)
                    try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
                    let destination = folder.appendingPathComponent(url.lastPathComponent)
                    try FileManager.default.copyItem(at: url, to: destination)
                    try FileManager.default.setAttributes([.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication], ofItemAtPath: destination.path)
                    continuation.resume(returning: destination)
                } catch { continuation.resume(throwing: error) }
            }
        }
    }
    private static func copy(_ provider: NSItemProvider, type: String, directory: URL, index: Int) async throws -> URL {
        try await withCheckedThrowingContinuation { continuation in
            provider.loadFileRepresentation(forTypeIdentifier: type) { url, error in
                do {
                    if let error { throw error }
                    guard let url else { throw OinkFailure(message: "无法读取分享文件。") }
                    let folder = directory.appendingPathComponent(String(index), isDirectory: true)
                    try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
                    let destination = folder.appendingPathComponent(url.lastPathComponent)
                    try FileManager.default.copyItem(at: url, to: destination)
                    try FileManager.default.setAttributes([.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication], ofItemAtPath: destination.path)
                    continuation.resume(returning: destination)
                } catch { continuation.resume(throwing: error) }
            }
        }
    }
}
