import Foundation
import Security

enum SharedStore {
    static var group: String { Bundle.main.object(forInfoDictionaryKey: "OinkAppGroup") as! String }
    static var defaults: UserDefaults { UserDefaults(suiteName: group)! }
    static func root() throws -> URL {
        guard let url = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: group) else {
            throw OinkFailure(message: "共享容器不可用，请检查签名与 App Group。")
        }
        let root = url.appendingPathComponent("Library/Application Support/Oink", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true,
            attributes: [.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication])
        return root
    }
    static func stagingParent() throws -> URL {
        let url = try root().appendingPathComponent("ShareStaging", isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true,
            attributes: [.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication])
        return url
    }
    /// A staging directory owned by this process until `remove()` (L17).
    static func staging() throws -> ShareStagingLease {
        try ShareStagingLease(in: stagingParent())
    }
    /// `deviceOnly`: the value names this phone's group (see `DeviceOnlyStorage`).
    static func save<T: Encodable>(_ value: T, name: String, deviceOnly: Bool = false) throws {
        try JSONEncoder().encode(value).write(to: directory(deviceOnly).appendingPathComponent(name),
            options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
    }
    static func load<T: Decodable>(_ type: T.Type, name: String, deviceOnly: Bool = false) throws -> T {
        try JSONDecoder().decode(type, from: Data(contentsOf: directory(deviceOnly).appendingPathComponent(name)))
    }
    private static func directory(_ deviceOnly: Bool) throws -> URL {
        deviceOnly ? try DeviceOnlyStorage.prepare(in: root()) : try root()
    }
}

/// Only the main app's explicit first-run path may create identity; extensions read existing data.
enum IdentityStore {
    private static func query() throws -> [String: Any] {
        guard let group = Bundle.main.object(forInfoDictionaryKey: "OinkKeychainGroup") as? String else {
            throw OinkFailure(message: "未配置共享 Keychain。")
        }
        return [kSecClass as String: kSecClassGenericPassword,
                kSecAttrService as String: "com.hachiwii.oink.identity",
                kSecAttrAccount as String: "device", kSecAttrAccessGroup as String: group]
    }
    static func read() throws -> Data? {
        var q = try query()
        q[kSecReturnData as String] = true
        q[kSecMatchLimit as String] = kSecMatchLimitOne
        var value: CFTypeRef?
        let status = SecItemCopyMatching(q as CFDictionary, &value)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess else { throw OinkFailure(message: "身份读取失败（\(status)）；请先解锁设备。") }
        return value as? Data
    }
    static func readOrCreateForMainApp() throws -> Data {
        if let existing = try read() {
            guard existing.count == 32 else { throw OinkFailure(message: "身份数据无效，未生成替代身份。") }
            return existing
        }
        var seed = Data(count: 32)
        let status = seed.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, 32, $0.baseAddress!) }
        guard status == errSecSuccess else { throw OinkFailure(message: "无法生成设备身份。") }
        do { try create(seed) }
        catch {
            // Another legitimate first-run process may have won SecItemAdd. Never overwrite it.
            if let existing = try read(), existing.count == 32 { return existing }
            throw error
        }
        return seed
    }
    static func requireExisting() throws -> Data {
        guard let seed = try read(), seed.count == 32 else {
            throw OinkFailure(message: "请先在 Oink 内完成设备设置。")
        }
        return seed
    }
    private static func create(_ data: Data) throws {
        var q = try query()
        q[kSecValueData as String] = data
        q[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        let status = SecItemAdd(q as CFDictionary, nil)
        guard status == errSecSuccess else { throw OinkFailure(message: "身份保存失败（\(status)）。") }
    }
}
