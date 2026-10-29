import Foundation
import FileProvider
import AppKit
import Darwin

private var nativeWatchdog: DispatchSourceTimer?

@main struct Manager {
    @MainActor static func main() {
        let command = CommandLine.arguments.dropFirst().first ?? "status"
        if command == "finish-uninstall" {
            // Run the final native alert on the process's original main thread.
            // dispatchMain may service the main queue on a worker thread.
            do { try finishOinkUninstall(); exit(0) }
            catch { exit(1) }
        }
        let native = ["received-read", "received-stat", "received-link", "received-download", "received-move"].contains(command)
        let activity = NativeActivity()
        if native {
            let timer = DispatchSource.makeTimerSource(queue: .global())
            timer.schedule(deadline: .now() + 1, repeating: 1)
            timer.setEventHandler { if activity.expired() { exit(2) } }
            timer.resume()
            // The process exits after this one command; keep its watchdog alive.
            nativeWatchdog = timer
        }
        // Bound system callbacks even when the extension is disabled or authorization is pending.
        // Whole-domain deletion may take longer than an item operation, especially
        // with multiple domains. A timeout remains a failure, never a removal ACK.
        if !native { DispatchQueue.global().asyncAfter(deadline: .now() + (["remove-domain", "reset-store", "recover-launch", "container"].contains(command) ? 60 : command == "stop-uninstall-workers" ? 20 : 12)) {
            fputs(command == "remove-domain" ? "File Provider domain removal timed out\n" :
                  "File Provider operation timed out; retry opening the Oink location in Finder\n", stderr)
            exit(2)
        } }
        Task {
            do {
                if command == "stop-uninstall-workers" {
                    guard CommandLine.arguments.count == 3, let uiPID = pid_t(CommandLine.arguments[2]), uiPID > 1 else {
                        throw CocoaError(.fileReadInvalidFileName)
                    }
                    // Detached initialization managers can outlive backend exit.
                    // Drain them before removal so none can re-add the domain.
                    // The extension is stopped only after domain removal, when
                    // fileproviderd no longer has a reason to relaunch it.
                    try stopOinkProcesses(app: installedUninstallApp(), excluding: [uiPID], includeExtension: false)
                    print("stopped"); exit(0)
                }
                if command == "unregister-login-item" {
                    try await unregisterOinkLoginItem()
                    print("unregistered"); exit(0)
                }
                let id = NSFileProviderDomainIdentifier(providerDomain)
                let registered = try await NSFileProviderManager.domains()
                var domains = registered.filter { $0.identifier == id }
                if let action = ProviderRemovalAction(rawValue: command) {
                    let uninstall = Array(CommandLine.arguments.dropFirst(2)) == ["--uninstall"]
                    guard CommandLine.arguments.count == 2 || uninstall else { throw CocoaError(.fileReadInvalidFileName) }
                    // Only explicit uninstall may erase the extension's own home.
                    let app = try uninstall ? installedUninstallApp() : nil
                    func removeDomains() async throws -> String {
                        try await action.perform(domains: action.domains(registered, uninstall: uninstall), remove: { domain, mode in
                            try await NSFileProviderManager.remove(domain, mode: mode)
                        }, remaining: { action.domains(try await NSFileProviderManager.domains(), uninstall: uninstall) })
                    }
                    let result: String
                    if let app {
                        result = try await finishUninstallProvider(removeDomains: removeDomains, stopExtension: {
                            // Once domains are gone fileproviderd cannot relaunch
                            // writers while the appex clears its own container.
                            try stopOinkProcesses(app: app.appendingPathComponent("Contents/PlugIns"))
                        }, clearExtension: { try clearUninstallExtension(app: app) })
                    } else { result = try await removeDomains() }
                    print(result)
                    exit(0)
                }
                if command == "inspect" {
                    let values: [[String: Any]] = domains.map { ["id": $0.identifier.rawValue, "hidden": $0.isHidden, "userEnabled": $0.userEnabled, "disconnected": $0.isDisconnected] }
                    print(String(data: try JSONSerialization.data(withJSONObject: values, options: [.sortedKeys]), encoding: .utf8)!)
                    exit(0)
                }
                if command == "check-launch" {
                    // Read-only recovery of an uncertain Recover result. A
                    // connected domain cannot bypass unfinished local migration.
                    try ProviderUpgrade(group: appGroupContainer()).requireReady()
                    guard let domain = domains.first, let manager = NSFileProviderManager(for: domain) else {
                        throw NSFileProviderError(.providerNotFound)
                    }
                    try await verifyProviderLaunch(hidden: domain.isHidden, enabled: domain.userEnabled,
                        disconnected: domain.isDisconnected,
                        resolveRoot: { try await manager.getUserVisibleURL(for: .rootContainer) })
                    print("connected"); exit(0)
                }
                if command == "recover-launch" || command == "reset-store" {
                    guard CommandLine.arguments.count == (command == "reset-store" ? 3 : 2) else { throw CocoaError(.fileReadInvalidFileName) }
                    let upgrade = ProviderUpgrade(group: try appGroupContainer())
                    try upgrade.prepareLocal(resetToken: command == "reset-store" ? CommandLine.arguments[2] : nil)
                    guard let domain = domains.first(where: { $0.identifier == id }) else {
                        try await finishProviderUpgrade(upgrade, domain: nil)
                        print("absent"); exit(0)
                    }
                    if !domain.userEnabled { print("inactive"); exit(0) }
                    guard let manager = NSFileProviderManager(for: domain) else { throw CocoaError(.fileReadUnknown) }
                    try await recoverProviderLaunch(hidden: domain.isHidden, enabled: domain.userEnabled,
                        disconnected: domain.isDisconnected, disconnect: { options in
                            try await manager.disconnect(reason: "Updating Oink", options: options)
                        }, reconnect: { try await manager.reconnect() },
                        finishUpgrade: { _ = try await finishProviderUpgrade(upgrade, domain: domain, manager: manager) },
                        resolveRoot: { try await manager.getUserVisibleURL(for: .rootContainer) })
                    print("connected"); exit(0)
                }
                if let action = ProviderUpdateAction(rawValue: command) {
                    guard let domain = domains.first(where: { $0.identifier == id }),
                          domain.isHidden, domain.userEnabled else {
                        print("inactive"); exit(0)
                    }
                    if action == .prepare && domain.isDisconnected {
                        // The installer must not finish/reconnect a domain it
                        // did not suspend, including an existing user disconnect.
                        print("inactive"); exit(0)
                    }
                    if action == .finish && !domain.isDisconnected {
                        print("connected"); exit(0)
                    }
                    guard let manager = NSFileProviderManager(for: domain) else { throw CocoaError(.fileReadUnknown) }
                    try await action.perform(disconnect: { options in
                        try await manager.disconnect(reason: "Updating Oink", options: options)
                    }, reconnect: {
                        try await manager.reconnect()
                    })
                    print(action == .prepare ? "prepared" : "connected")
                    exit(0)
                }
                // No command that exposes or writes receiver data bypasses the
                // durable cut-over. Inactive domains keep their user state and
                // cannot publish until a later enabled launch completes reset.
                if command != "inspect" && command != "open-enable" {
                    try await finishProviderUpgrade(ProviderUpgrade(group: try appGroupContainer()), domain: domains.first)
                }
                if command == "container" { print(try container().path); exit(0) }
                if !domains.contains(where: { $0.identifier == id }) && (command == "publish" || command == "initialize" || command == "open-enable") {
                    let domain = NSFileProviderDomain(identifier: id, displayName: providerDisplayName)
                    domain.isHidden = true
                    try await NSFileProviderManager.add(domain)
                    domains = try await NSFileProviderManager.domains()
                }
                if command == "open-received" || command == "received-usage" || command == "received-maintain" {
                    guard let domain=domains.first(where:{$0.identifier==id}),let manager=NSFileProviderManager(for:domain) else {throw CocoaError(.fileNoSuchFile)}
                    if command == "open-received" {let url=try await manager.getUserVisibleURL(for:.rootContainer);guard NSWorkspace.shared.open(url) else {throw CocoaError(.fileReadUnknown)};print("opened");exit(0)}
                    let days=CommandLine.arguments.count>2 ? UInt64(CommandLine.arguments[2]) ?? 0 : 0
                    let value=try await receivedUsage(manager:manager,root:container(),days:days,clear:command == "received-maintain")
                    print(String(data:try JSONSerialization.data(withJSONObject:value,options:[.sortedKeys]),encoding:.utf8)!);exit(0)
                }
                if command == "cache" || command == "cache-clear" {
                    let clear = command == "cache-clear"
                    let protection = try JSONDecoder().decode(CacheProtection.self, from: FileHandle.standardInput.readDataToEndOfFile())
                    var bytes: UInt64 = 0, kept: UInt64 = 0, removed: UInt64 = 0
                    var temporary: UInt64 = 0, temporaryKept: UInt64 = 0
                    var incomplete = false
                    var evicted = Set<String>()
                    var protectedBatches = Set(protection.items.compactMap { $0.split(separator: "-").first.map(String.init) })
                    var busy = false
                    var items: [NSFileProviderItem] = []
                    if let domain = domains.first(where: { $0.identifier == id }), domain.isHidden,
                       let cacheManager = NSFileProviderManager(for: domain) {
                        let downloads = cacheManager.globalProgress(for: .downloading)
                        busy = downloads.totalUnitCount < 0 || downloads.completedUnitCount < downloads.totalUnitCount
                        let observer = MaterializedObserver(cacheManager.enumeratorForMaterializedItems())
                        do { items = try await observer.read() } catch { incomplete = true }
                        observer.enumerator.invalidate()
                    }
                    let cacheManager = domains.first(where: { $0.identifier == id && $0.isHidden }).flatMap { NSFileProviderManager(for: $0) }
                    for item in items {
                        guard let manager = cacheManager else { incomplete = true; continue }
                        do {
                            let url = try await manager.getUserVisibleURL(for: item.itemIdentifier)
                            var metadata = stat()
                            guard lstat(url.path, &metadata) == 0 else { throw CocoaError(.fileReadUnknown) }
                            // Received OS copies have an independent user lifecycle.
                            if try receivedProtected(String(item.itemIdentifier.rawValue.split(separator:"-").first ?? ""),in:container()) {
                                if metadata.st_flags & UInt32(SF_DATALESS)==0,item.contentType?.conforms(to:.folder) != true,UInt64(max(0,metadata.st_size))==(item.documentSize ?? nil)?.uint64Value {evicted.insert(item.itemIdentifier.rawValue)}
                                continue
                            }
                            let protected = protection.items.contains(item.itemIdentifier.rawValue) || sourceProtects(url,paths:protection.paths)
                            if protected, let key=item.itemIdentifier.rawValue.split(separator:"-").first {protectedBatches.insert(String(key))}
                            guard item.contentType != .folder else {continue}
                            let size = UInt64(max(0, metadata.st_blocks)) * 512
                            if clear && !busy && !protected {
                                do { try await manager.evictItem(identifier: item.itemIdentifier); cacheStorageChanged(); removed += size; evicted.insert(item.itemIdentifier.rawValue); continue }
                                catch {
                                    if let key=item.itemIdentifier.rawValue.split(separator:"-").first {protectedBatches.insert(String(key))}
                                }
                            }
                            bytes += size
                            if clear || protected { kept += size }
                        } catch {
                            incomplete = true
                            if let key=item.itemIdentifier.rawValue.split(separator:"-").first {protectedBatches.insert(String(key))}
                        }
                    }
                    let temp = try inspectTemporary(in: contentTemporaryDirectory(), clear: clear && !busy, evicted: evicted)
                    temporary = temp.bytes; temporaryKept = temp.kept; removed += temp.removed
                    incomplete = incomplete || temp.incomplete
                    if clear && busy {kept=bytes;temporaryKept=temporary}
                    let result: [String: Any] = ["bytes": bytes, "kept": kept, "removed": removed, "temporary": temporary, "temporaryKept": temporaryKept, "incomplete": incomplete, "busy": busy, "protected_batches": Array(protectedBatches)]
                    print(String(data: try JSONSerialization.data(withJSONObject: result, options: [.sortedKeys]), encoding: .utf8)!)
                    exit(0)
                }
                guard let domain = domains.first(where: { $0.identifier == id }) else {
                    fputs("Oink file extension is not registered\n", stderr); exit(4)
                }
                // Opening the root is the user-facing enablement flow even for a hidden,
                // disabled domain. This does not change userEnabled or the hidden flag.
                if command == "open-enable" {
                    guard domain.isHidden, let manager = NSFileProviderManager(for: domain) else {
                        throw NSError(domain: "OinkFileProvider", code: 2, userInfo: [NSLocalizedDescriptionKey: "Oink hidden file domain is unavailable"])
                    }
                    let url = try await manager.getUserVisibleURL(for: .rootContainer)
                    let opened = await MainActor.run { NSWorkspace.shared.open(url) }
                    guard opened else { throw CocoaError(.fileReadUnknown) }
                    print("opened")
                    exit(0)
                }
                if domain.isDisconnected { exit(5) }
                guard domain.userEnabled, domain.isHidden,
                      let manager = NSFileProviderManager(for: domain) else {
                    fputs("Oink file extension is not enabled\n", stderr); exit(3)
                }
                if native {
                    try await nativeReceived(command, arguments: Array(CommandLine.arguments.dropFirst(2)),
                                             manager: manager, activity: activity)
                    exit(0)
                }
                if command == "status" || command == "initialize" {
                    print("domain enabled; item URL readiness is checked on publication"); exit(0)
                }
                if command == "signal" {
                    // Replicated extensions signal only the working set. The SDK
                    // explicitly says that signals for other containers are ignored.
                    try await manager.signalEnumerator(for: .workingSet)
                    exit(0)
                }
                guard ["publish", "prepare-spare", "prepare-placeholder-parent", "prepare-placeholders", "publish-placeholders", "retire-placeholders"].contains(command), CommandLine.arguments.count == 3 else { throw CocoaError(.fileReadInvalidFileName) }
                let batch = CommandLine.arguments[2]
                guard batch.allSatisfy({ $0.isHexDigit || $0 == "-" }) else { throw CocoaError(.fileReadInvalidFileName) }
                if command == "retire-placeholders" {
                    let ledgerURL = try container().appendingPathComponent(batch + ".placeholder")
                    let ledger = try JSONDecoder().decode(PlaceholderLedger.self, from: Data(contentsOf: ledgerURL))
                    // The catalog is already removed under the Rust storage lock.
                    // Only observe the OS namespace; never delete CloudStorage files.
                    try await retirePlaceholder(batch: batch, ledger: ledger, manager: manager)
                    print("retired")
                    exit(0)
                }
                let catalog = try JSONDecoder().decode(Catalog.self, from: Data(contentsOf: try container().appendingPathComponent(batch + ".json")))
                try await manager.signalEnumerator(for: .workingSet)
                if command == "prepare-spare" || command == "prepare-placeholder-parent" {
                    guard catalog.records.count == 1, let parent = catalog.records.first,
                          parent.id == batch + "-root", parent.directory, parent.children == 0 else {
                        throw CocoaError(.fileReadCorruptFile)
                    }
                    let item = NSFileProviderItemIdentifier(parent.id)
                    // SDK NSFileProviderManager.h: downloading a dataless directory
                    // enumerates/materializes one level, leaving children dataless.
                    // The request callback only acknowledges scheduling. Follow it
                    // with a coordinated directory read (same header's URL contract)
                    // so readiness waits for materialization, not just request ACK.
                    let preparedURL = try await prepareSpareDirectory(resolve: {
                        try await manager.getUserVisibleURL(for: item)
                    }, download: {
                        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
                            manager.requestDownloadForItem(withIdentifier: item, requestedRange: nil) { error in
                                if let error { continuation.resume(throwing: error) }
                                else { continuation.resume() }
                            }
                        }
                    }, materialized: { url in
                        let scoped = url.startAccessingSecurityScopedResource()
                        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
                        var coordinationError: NSError?
                        var readResult: Result<Bool, Error> = .failure(CocoaError(.fileReadUnknown))
                        NSFileCoordinator().coordinate(readingItemAt: url, options: [], error: &coordinationError) { readable in
                            readResult = Result {
                                // Reading this synthetic empty directory forces its
                                // one-level namespace to materialize; no user content.
                                let children = try FileManager.default.contentsOfDirectory(at: readable, includingPropertiesForKeys: [])
                                var metadata = stat()
                                return children.isEmpty && lstat(readable.path, &metadata) == 0 &&
                                    metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFDIR) &&
                                    metadata.st_flags & UInt32(SF_DATALESS) == 0
                            }
                        }
                        if let coordinationError { throw coordinationError }
                        return try readResult.get()
                    })
                    if command == "prepare-placeholder-parent" {
                        print(String(data: try JSONEncoder().encode(preparedURL.path), encoding: .utf8)!)
                    } else { print("ready") }
                    exit(0)
                }
                let requested = catalog.records.filter { $0.parent == batch + "-root" }
                guard let parent = catalog.records.first(where: { $0.id == batch + "-root" }) else {
                    throw CocoaError(.fileReadCorruptFile)
                }
                let urls = try await resolvePublishedItemURLs(parent: parent, children: requested, resolve: { record in
                    try await manager.getUserVisibleURL(for: NSFileProviderItemIdentifier(record.id))
                }, rejected: { rejection in
                    // Other in-flight SDK callbacks need not finish before a
                    // definitive rejection. This helper owns one publication.
                    fputs("File Provider item rejected cause=\(rejection.rawValue)\n", stderr)
                    exit(rejection.rawValue)
                })
                if command == "publish-placeholders" {
                    let ledger = try JSONDecoder().decode(PlaceholderLedger.self, from: Data(contentsOf: try container().appendingPathComponent(batch + ".placeholder")))
                    // Unused slots and each consumed slot's old generic name must
                    // disappear as well. A final user name may equal a generic
                    // path; that path is then a validated final item, not surplus.
                    let final = Set(urls)
                    for old in ledger.urls where !final.contains(old) {
                        try await waitForItemRemoval(URL(fileURLWithPath: old))
                    }
                }
                print(String(data: try JSONEncoder().encode(urls), encoding: .utf8)!)
                exit(0)
            } catch {
                fputs(providerFailureDiagnostic(error) + "\n", stderr)
                if ["publish", "prepare-spare", "prepare-placeholder-parent", "prepare-placeholders", "publish-placeholders", "retire-placeholders"].contains(command),
                   let rejection = publicationRejection(error) {
                    fputs("File Provider item rejected cause=\(rejection.rawValue)\n", stderr)
                    exit(rejection.rawValue)
                }
                // NSError descriptions may include the file path or an endpoint.
                // The host can retry a just-moved provider registration without
                // retrying unrelated API failures or parsing localized messages.
                exit((error as NSError).domain == NSFileProviderErrorDomain &&
                     (error as NSError).code == -2001 ? 6 : 1)
            }
        }
        dispatchMain()
    }
}
