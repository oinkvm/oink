import Foundation
import dnssd

/// Registers Rust's existing endpoint with system Bonjour on system-selected interfaces.
/// This does not create a listener or perform application-level multicast.
@MainActor
final class BonjourPublisher {
    var failure: ((String) -> Void)?
    private var service: DNSServiceRef?

    func publish(instance: String, type: String, port: UInt16, txt: [String: String]) throws {
        stop()
        let record = NetService.data(fromTXTRecord: txt.mapValues { Data($0.utf8) })
        guard record.count <= Int(UInt16.max) else { throw OinkFailure(message: "Bonjour TXT 过大。") }
        let error = record.withUnsafeBytes { bytes in
            DNSServiceRegister(&service, 0, UInt32(kDNSServiceInterfaceIndexAny), instance, type, "local.", nil, port.bigEndian,
                UInt16(record.count), bytes.baseAddress, { _, _, error, _, _, _, context in
                    guard error != kDNSServiceErr_NoError, let context else { return }
                    let publisher = Unmanaged<BonjourPublisher>.fromOpaque(context).takeUnretainedValue()
                    Task { @MainActor in publisher.failure?("Bonjour 发布失败（\(error)）。") }
                }, Unmanaged.passUnretained(self).toOpaque())
        }
        guard error == kDNSServiceErr_NoError, let service else {
            stop()
            throw OinkFailure(message: "Bonjour 发布失败（\(error)）。")
        }
        let queueError = DNSServiceSetDispatchQueue(service, .main)
        guard queueError == kDNSServiceErr_NoError else {
            stop()
            throw OinkFailure(message: "Bonjour 调度失败（\(queueError)）。")
        }
    }
    func stop() {
        if let service { DNSServiceRefDeallocate(service) }
        service = nil
    }
}
