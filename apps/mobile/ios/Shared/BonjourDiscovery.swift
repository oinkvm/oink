import Foundation
import Network
import Darwin
import dnssd

struct DiscoveredEndpoint: Codable {
    let ip: String
    let port: UInt16
    let scopeID: UInt32
}
struct DiscoveredService: Codable {
    let key: String
    let generation: UInt64
    let txt: [String: String]
    let endpoints: [DiscoveredEndpoint]
}

/// System Bonjour only: no application-level multicast sockets or interface restrictions.
@MainActor
final class BonjourDiscovery: NSObject, NetServiceDelegate {
    var upsert: ((DiscoveredService) -> Void)?
    var withdraw: ((String, UInt64) -> Void)?
    var networkChanged: ((UInt64) -> Void)?
    var failure: ((String) -> Void)?
    /// True while iOS blocks browsing because the Local Network permission
    /// is denied (the add page then shows「无法搜索附近的设备」).
    var localNetworkDenied: ((Bool) -> Void)?
    private var browser: NWBrowser?
    private var monitor: NWPathMonitor?
    private var services: [String: NetService] = [:]
    private var lastResults: [String: NWBrowser.Result] = [:]
    private var generation: UInt64 = 0
    /// The local-network shape the current browse runs on.
    private var localPath: LocalPath?

    /// What Bonjour browsing depends on: whether the path is usable and the
    /// Wi-Fi / Ethernet interfaces it has. Cellular, VPN and other attribute
    /// updates leave it unchanged, so they do not restart discovery.
    private struct LocalPath: Equatable {
        let satisfied: Bool
        let interfaces: [String]

        init(_ path: NWPath) {
            satisfied = path.status == .satisfied
            interfaces = path.availableInterfaces
                .filter { $0.type == .wifi || $0.type == .wiredEthernet }
                .map(\.name)
                .sorted()
        }
    }

    func start() {
        guard monitor == nil else { return }
        let monitor = NWPathMonitor()
        monitor.pathUpdateHandler = { [weak self, weak monitor] path in
            let local = LocalPath(path)
            Task { @MainActor in
                guard let self, let monitor, self.monitor === monitor else { return }
                guard self.localPath != local else { return }
                self.localPath = local
                self.pathChanged()
            }
        }
        self.monitor = monitor
        monitor.start(queue: .main)
    }
    private func pathChanged() {
        stopBrowser()
        generation &+= 1
        // Only discovery and redials are replaced; links stay up and end on
        // their own heartbeat if their path is really gone.
        networkChanged?(generation)
        startBrowser()
    }
    private func startBrowser() {
        let parameters = NWParameters()
        let browser = NWBrowser(for: .bonjourWithTXTRecord(type: "_oink._udp", domain: "local."), using: parameters)
        let current = generation
        browser.browseResultsChangedHandler = { [weak self] results, _ in
            Task { @MainActor in
                guard let self, self.generation == current else { return }
                self.update(results)
            }
        }
        browser.stateUpdateHandler = { [weak self] state in
            Task { @MainActor in
                guard let self, self.generation == current else { return }
                switch state {
                case .ready:
                    self.localNetworkDenied?(false)
                case .waiting(let error):
                    // A denied Local Network permission does not fail the
                    // browser: it stays in .waiting with the DNS-SD policy
                    // error until the user allows access in Settings.
                    if Self.isPolicyDenied(error) { self.localNetworkDenied?(true) }
                case .failed(let error):
                    if Self.isPolicyDenied(error) { self.localNetworkDenied?(true) }
                    else { self.failure?(error.oinkMessage) }
                default:
                    break
                }
            }
        }
        self.browser = browser
        browser.start(queue: .main)
    }
    private static func isPolicyDenied(_ error: NWError) -> Bool {
        if case .dns(let code) = error { return code == DNSServiceErrorType(kDNSServiceErr_PolicyDenied) }
        return false
    }
    private func update(_ results: Set<NWBrowser.Result>) {
        if !results.isEmpty { localNetworkDenied?(false) }
        var present = Set<String>()
        for result in results {
            guard case let .service(name, type, domain, _) = result.endpoint else { continue }
            let key = "\(name).\(type)\(domain)"
            present.insert(key)
            if lastResults[key] != result {
                services[key]?.stop()
                lastResults[key] = result
                let service = NetService(domain: domain, type: type, name: name)
                service.delegate = self
                services[key] = service
                service.resolve(withTimeout: 5)
            }
        }
        for key in Set(services.keys).subtracting(present) {
            services.removeValue(forKey: key)?.stop()
            lastResults.removeValue(forKey: key)
            withdraw?(key, generation)
        }
    }
    nonisolated func netServiceDidResolveAddress(_ sender: NetService) {
        Task { @MainActor in
            guard let key = services.first(where: { $0.value === sender })?.key else { return }
            let endpoints = (sender.addresses ?? []).compactMap(Self.endpoint)
            let txt = sender.txtRecordData().map(NetService.dictionary(fromTXTRecord:)) ?? [:]
            upsert?(DiscoveredService(key: key, generation: generation,
                txt: txt.compactMapValues { String(data: $0, encoding: .utf8) }, endpoints: endpoints))
        }
    }
    private static func endpoint(_ data: Data) -> DiscoveredEndpoint? {
        data.withUnsafeBytes { bytes in
            guard let base = bytes.baseAddress, data.count >= MemoryLayout<sockaddr>.size else { return nil }
            let address = base.assumingMemoryBound(to: sockaddr.self)
            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            guard getnameinfo(address, socklen_t(data.count), &host, socklen_t(host.count), nil, 0, NI_NUMERICHOST) == 0 else { return nil }
            switch Int32(address.pointee.sa_family) {
            case AF_INET:
                guard data.count >= MemoryLayout<sockaddr_in>.size else { return nil }
                let v4 = base.assumingMemoryBound(to: sockaddr_in.self).pointee
                return DiscoveredEndpoint(ip: String(cString: host), port: UInt16(bigEndian: v4.sin_port), scopeID: 0)
            case AF_INET6:
                guard data.count >= MemoryLayout<sockaddr_in6>.size else { return nil }
                let v6 = base.assumingMemoryBound(to: sockaddr_in6.self).pointee
                return DiscoveredEndpoint(ip: String(cString: host).components(separatedBy: "%")[0], port: UInt16(bigEndian: v6.sin6_port), scopeID: v6.sin6_scope_id)
            default: return nil
            }
        }
    }
    func stop() {
        monitor?.cancel(); monitor = nil
        localPath = nil
        stopBrowser()
        generation &+= 1
    }
    private func stopBrowser() {
        browser?.cancel(); browser = nil
        for (key, service) in services { service.stop(); withdraw?(key, generation) }
        services.removeAll()
        lastResults.removeAll()
    }
}
