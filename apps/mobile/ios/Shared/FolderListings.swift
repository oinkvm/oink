import Foundation

struct IOSDirectoryKey: Hashable {
    var folderMessageID: Int64
    var directory: UInt64
}

struct IOSDirectoryRequest {
    var key: IOSDirectoryKey
    var append: Bool
}

/// Why a folder listing ended without a page: the folder view's text, and
/// whether 重试 can help (false when the content is gone for good).
struct TransferDirectoryFailure: Equatable {
    var text: String
    var retryable: Bool
}

/// What the folder views show, by (folder message, node as requested; 0 is
/// the root): the pages loaded so far and the listings that ended without
/// one. CoreSession feeds it its requests and the core's page and failure
/// events; a page is loading while it has neither.
struct FolderListings {
    var pages: [IOSDirectoryKey: TransferDirectoryPage] = [:]
    var failures: [IOSDirectoryKey: TransferDirectoryFailure] = [:]
    private(set) var requests: [String: IOSDirectoryRequest] = [:]

    /// Listing `operation` of `key` went out (`append`: its next page); the
    /// page loads again until its page or failure arrives.
    mutating func requested(_ operation: String, key: IOSDirectoryKey, append: Bool) {
        requests[operation] = IOSDirectoryRequest(key: key, append: append)
        failures[key] = nil
    }

    /// The listing could not even be asked for (no session, folder gone):
    /// no page or failure event follows, so it fails here.
    mutating func notRequested(_ operation: String, failure: TransferDirectoryFailure) {
        guard let request = requests.removeValue(forKey: operation) else { return }
        failures[request.key] = failure
    }

    /// One page of `directory` (as requested) of folder `folderID`.
    mutating func page(_ operation: String, folderID: Int64, directory: UInt64, entries: [TransferDirectoryEntry],
                       nextOffset: UInt32?, nextVersion: UInt64?) {
        let request = requests.removeValue(forKey: operation)
        let key = IOSDirectoryKey(folderMessageID: folderID, directory: directory)
        var page = request?.append == true ? (pages[key] ?? TransferDirectoryPage(
            folderMessageID: folderID, directory: directory, entries: [], nextOffset: nil, nextVersion: nil))
            : TransferDirectoryPage(folderMessageID: folderID, directory: directory, entries: [], nextOffset: nil, nextVersion: nil)
        page.entries.append(contentsOf: entries)
        page.nextOffset = nextOffset
        page.nextVersion = nextVersion
        pages[key] = page
        failures[key] = nil
    }

    /// The listing of `directory` (as requested) of folder `folderID` ended
    /// without a page. A failed first page drops what an earlier listing
    /// showed (it may be gone); a failed next page keeps the entries shown.
    mutating func failed(_ operation: String, folderID: Int64, directory: UInt64, failure: TransferDirectoryFailure) {
        let request = requests.removeValue(forKey: operation)
        let key = IOSDirectoryKey(folderMessageID: folderID, directory: directory)
        if request?.append != true { pages[key] = nil }
        failures[key] = failure
    }

    /// The session ended: its requests get no answer.
    mutating func endSession() {
        requests.removeAll()
    }
}
