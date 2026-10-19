import Foundation
import Darwin
import MachO

@_silgen_name("oink_report_arm") private func armCapsule(_ path: UnsafePointer<CChar>, _ generation: UInt64,
    _ capability: UnsafePointer<UInt8>, _ version: UnsafePointer<UInt16>, _ build: UInt32, _ major: UInt8) -> Int32
@_silgen_name("oink_report_disarm") private func disarmCapsule()

// Only fixed numeric metadata in the private App Group inbox. All extraction,
// consent checks, quotas and HTTPS remain owned by the per-user background agent.
private struct ReportVersion: Codable { let major: UInt16; let minor: UInt16; let patch: UInt16; let build: UInt32 }
private struct ReportBuild: Codable { let app_version: ReportVersion; let os_version_class: [String: UInt8] }
private struct ReportGrant: Codable {
    let schema: UInt16
    let policy_version: UInt16; let generation: UInt64; let capability: [UInt8]
    let agent_pid: UInt32; let agent_launched_at: UInt64; let armed_at: UInt64
    let debug_id: [UInt8]; let build: ReportBuild
}
private struct ReportLifetime: Encodable {
    let schema: UInt16 = 1
    let pid: UInt32; let launched_at: UInt64; let armed_at: UInt64; let generation: UInt64
    let component = "file_provider"; let image = "file_provider"; let debug_id: [UInt8]; let build: ReportBuild
}
private struct ReportRegistration: Encodable { let schema: UInt16 = 1; let capability: [UInt8]; let lifetime: ReportLifetime }

private func launched(_ pid: UInt32) -> UInt64? {
    var info = proc_bsdinfo()
    let size = MemoryLayout<proc_bsdinfo>.size
    guard proc_pidinfo(Int32(pid), PROC_PIDTBSDINFO, 0, &info, Int32(size)) == size else { return nil }
    return info.pbi_start_tvsec
}
private func shippedDebugID() -> [UInt8]? {
    let header = #dsohandle.assumingMemoryBound(to: mach_header_64.self)
    guard header.pointee.magic == MH_MAGIC_64, header.pointee.ncmds < 4096 else { return nil }
    var command = UnsafeRawPointer(header).advanced(by: MemoryLayout<mach_header_64>.size)
    let end = command.advanced(by: Int(header.pointee.sizeofcmds))
    for _ in 0..<header.pointee.ncmds {
        guard command.advanced(by: MemoryLayout<load_command>.size) <= end else { return nil }
        let load = command.assumingMemoryBound(to: load_command.self).pointee
        guard load.cmdsize >= MemoryLayout<load_command>.size, command.advanced(by: Int(load.cmdsize)) <= end else { return nil }
        if load.cmd == LC_UUID && load.cmdsize >= MemoryLayout<uuid_command>.size {
            var uuid = command.assumingMemoryBound(to: uuid_command.self).pointee.uuid
            return withUnsafeBytes(of: &uuid) { Array($0) }
        }
        command = command.advanced(by: Int(load.cmdsize))
    }
    return nil
}

enum ReportCapture {
    private static let worker = DispatchQueue(label: "reports.local-capture", qos: .utility)
    private static var generation: UInt64 = 0
    private static var capability: [UInt8] = []
    // Called from ordinary extension lifecycle, never a native crash callback.
    static func refresh() {
        worker.async {
            do {
                let root = try container().appendingPathComponent("Reports", isDirectory: true)
                let grantURL = root.appendingPathComponent("grant.json")
                let attrs = try FileManager.default.attributesOfItem(atPath: grantURL.path)
                guard let size = attrs[.size] as? NSNumber, size.uint64Value <= 4096 else { disarmCapsule(); generation = 0; return }
                let grant = try JSONDecoder().decode(ReportGrant.self, from: Data(contentsOf: grantURL))
                guard grant.schema == 1, grant.policy_version == 3, grant.generation > 0, grant.capability.count == 32,
                      launched(grant.agent_pid) == grant.agent_launched_at,
                      grant.debug_id == shippedDebugID(), let launch = launched(UInt32(getpid())),
                      let major = grant.build.os_version_class["mac_o_s_major"] else { disarmCapsule(); generation = 0; return }
                if generation == grant.generation && capability == grant.capability { return }
                let path = root.appendingPathComponent("capsule.bin").path
                let version = [grant.build.app_version.major, grant.build.app_version.minor, grant.build.app_version.patch]
                let armed = path.withCString { path in grant.capability.withUnsafeBufferPointer { cap in version.withUnsafeBufferPointer { v in
                    armCapsule(path, grant.generation, cap.baseAddress!, v.baseAddress!, grant.build.app_version.build, major)
                } } }
                guard armed != 0 else { generation = 0; return }
                let at = UInt64(Date().timeIntervalSince1970)
                let lifetime = ReportLifetime(pid: UInt32(getpid()), launched_at: launch, armed_at: max(at, grant.armed_at), generation: grant.generation,
                    debug_id: grant.debug_id, build: grant.build)
                let registration = ReportRegistration(capability: grant.capability, lifetime: lifetime)
                try JSONEncoder().encode(registration).write(to: root.appendingPathComponent("lifetime.json"), options: .atomic)
                generation = grant.generation
                capability = grant.capability
            } catch { disarmCapsule(); generation = 0 }
        }
    }
}
