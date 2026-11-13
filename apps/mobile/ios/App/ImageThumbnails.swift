import ImageIO
import UIKit

/// Downsampled images for the transfer list (review L17). ImageIO decodes straight
/// to the requested size and the result is cached per file version.
///
/// The list never decodes on the main thread: even downsampled, a received 12 MP
/// photo takes tens of milliseconds to decode, and SwiftUI builds rows (and the
/// rows it prefetches) inside the scroll's frame, so decoding there dropped frames
/// whenever pictures came into view (v040 transfer scroll fix). Rows take their
/// layout from `pixelSize` (file header only), show `cached` when it is there and
/// otherwise `load` on a background queue.
enum ImageThumbnails {
    /// Long side of the list thumbnails in pixels: the largest list picture (a
    /// 260 pt bubble filled by a portrait photo, about 350 pt long) at 3x.
    static let listMaxPixel = 1200

    private static let cache: NSCache<NSString, UIImage> = {
        let cache = NSCache<NSString, UIImage>()
        // Bitmaps are 4 bytes a pixel (a 1200 × 900 thumbnail is 4.3 MB).
        cache.totalCostLimit = 160 << 20
        return cache
    }()
    private static let sizes = NSCache<NSString, NSValue>()
    private static let cacheLock = NSLock()
    private static var generation: UInt64 = 0

    /// A superseded password must leave decoded memory as well as the disk store.
    /// The generation also prevents an already running decode from putting it back.
    static func purge() {
        cacheLock.lock()
        defer { cacheLock.unlock() }
        generation &+= 1
        cache.removeAllObjects()
        sizes.removeAllObjects()
    }
    private static let decoder: OperationQueue = {
        let queue = OperationQueue()
        queue.name = "oink.list-thumbnails"
        queue.qualityOfService = .userInitiated
        queue.maxConcurrentOperationCount = 2
        return queue
    }()
    /// Decodes running, with the callers waiting for each.
    @MainActor private static var running: [NSString: [CheckedContinuation<UIImage?, Never>]] = [:]

    /// Path, file version and thumbnail size. Rows ask on every build, so this is a plain stat():
    /// FileManager.attributesOfItem builds a whole attribute dictionary and showed up in
    /// the scroll profile.
    private static func key(_ path: String, _ maxPixel: Int) -> NSString {
        var info = stat()
        let modified = stat(path, &info) == 0 ? "\(info.st_mtimespec.tv_sec).\(info.st_mtimespec.tv_nsec)" : "0"
        return "\(path)#\(modified)#\(maxPixel)" as NSString
    }

    /// The image at `path` scaled so its longer side is at most `maxPixel`, or nil
    /// when the file is not a readable image. Decodes on the calling thread.
    static func image(path: String, maxPixel: Int) -> UIImage? {
        let key = key(path, maxPixel)
        if let hit = cache.object(forKey: key) { return hit }
        return decode(path: path, maxPixel: maxPixel, key: key)
    }

    /// The thumbnail if it is already decoded; never decodes.
    static func cached(path: String, maxPixel: Int) -> UIImage? {
        cache.object(forKey: key(path, maxPixel))
    }

    /// The thumbnail, decoded off the main thread; callers asking for the same file
    /// while it decodes share that decode.
    @MainActor static func load(path: String, maxPixel: Int) async -> UIImage? {
        let key = key(path, maxPixel)
        if let hit = cache.object(forKey: key) { return hit }
        return await withCheckedContinuation { continuation in
            start(path: path, maxPixel: maxPixel, key: key, waiter: continuation)
        }
    }

    /// Starts the decode of a picture about to be shown (SwiftUI builds rows ahead
    /// of the scroll), so it is usually ready when the row appears.
    @MainActor static func prefetch(path: String, maxPixel: Int) {
        let key = key(path, maxPixel)
        guard cache.object(forKey: key) == nil, running[key] == nil else { return }
        start(path: path, maxPixel: maxPixel, key: key, waiter: nil)
    }

    @MainActor private static func start(path: String, maxPixel: Int, key: NSString,
                                         waiter: CheckedContinuation<UIImage?, Never>?) {
        if running[key] != nil {
            if let waiter { running[key]?.append(waiter) }
            return
        }
        running[key] = waiter.map { [$0] } ?? []
        decoder.addOperation {
            let image = decode(path: path, maxPixel: maxPixel, key: key)
            DispatchQueue.main.async {
                MainActor.assumeIsolated {
                    for waiter in running.removeValue(forKey: key) ?? [] { waiter.resume(returning: image) }
                }
            }
        }
    }

    private static func decode(path: String, maxPixel: Int, key: NSString) -> UIImage? {
        cacheLock.lock()
        let started = generation
        cacheLock.unlock()
        guard let source = CGImageSourceCreateWithURL(URL(fileURLWithPath: path) as CFURL,
                                                      [kCGImageSourceShouldCache: false] as CFDictionary),
              let image = CGImageSourceCreateThumbnailAtIndex(source, 0, [
                  kCGImageSourceCreateThumbnailFromImageAlways: true,
                  kCGImageSourceCreateThumbnailWithTransform: true,
                  kCGImageSourceShouldCacheImmediately: true,
                  kCGImageSourceThumbnailMaxPixelSize: maxPixel,
              ] as CFDictionary) else { return nil }
        let result = UIImage(cgImage: image)
        cacheLock.lock()
        defer { cacheLock.unlock() }
        guard started == generation, FileManager.default.fileExists(atPath: path) else { return nil }
        cache.setObject(result, forKey: key, cost: image.bytesPerRow * image.height)
        return result
    }

    /// Width and height of the picture as shown (EXIF orientation applied), read
    /// from the file header without decoding; nil when it is not a readable image.
    static func pixelSize(path: String) -> CGSize? {
        let key = key(path, 0)
        if let hit = sizes.object(forKey: key) { return hit.cgSizeValue }
        guard let source = CGImageSourceCreateWithURL(URL(fileURLWithPath: path) as CFURL,
                                                      [kCGImageSourceShouldCache: false] as CFDictionary),
              let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
              let width = properties[kCGImagePropertyPixelWidth] as? Double,
              let height = properties[kCGImagePropertyPixelHeight] as? Double, width > 0, height > 0 else { return nil }
        // Orientations 5–8 turn the picture a quarter.
        let turned = ((properties[kCGImagePropertyOrientation] as? Int) ?? 1) >= 5
        let size = turned ? CGSize(width: height, height: width) : CGSize(width: width, height: height)
        sizes.setObject(NSValue(cgSize: size), forKey: key)
        return size
    }
}
