import Foundation
import ImageIO
import UniformTypeIdentifiers

/// A failure to show the user as it is.
struct AppError: LocalizedError {
    let errorDescription: String?
    init(_ text: String) { errorDescription = text }
}

/// Files of messages: received ones, and our own copies of sent ones.
///
/// They are kept privately in Application Support, out of iCloud and
/// computer backups, readable only while the phone is unlocked, and
/// deleted with their messages: at once when deleted here, and by the
/// sweep when deleted elsewhere or when they disappear. Locations are
/// relative to Application Support (the container moves between app
/// updates): `media/<name>` for the main identity, `media-p-<id>/<name>`
/// for an anonymous identity.
enum Media {
    /// How long a file nothing refers to is kept: its message may not be
    /// recorded yet.
    static let grace: TimeInterval = 10 * 60
    /// Larger images are refused rather than decoded (decompression bombs).
    static let maxPixels = 100_000_000.0

    /// "media" for the main identity, "media-p-<id>" for an anonymous one.
    static func dirName(_ persona: String?) -> String {
        persona.map { "media-p-\($0)" } ?? "media"
    }

    static func dir(_ persona: String?) -> URL {
        Paths.support.appendingPathComponent(dirName(persona), isDirectory: true)
    }

    /// Keeps `data` privately; returns its location, or nil if it couldn't.
    static func save(name: String, data: Data, persona: String?) -> String? {
        let folder = dir(persona)
        let location = "\(dirName(persona))/\(UUID().uuidString)-\(safeFileName(name: name))"
        do {
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            try Paths.excludeFromBackup(folder)
            // Written while locked (a background receipt), read only unlocked.
            try data.write(to: Paths.support.appendingPathComponent(location), options: .completeFileProtectionUnlessOpen)
            return location
        } catch {
            return nil
        }
    }

    /// Whether `location` names a file directly inside one of our media
    /// directories. A location synced from another device (an Android
    /// `file://` URI, a Linux path) never is, nor is anything with "..".
    static func isOurs(_ location: String) -> Bool {
        let parts = location.split(separator: "/", omittingEmptySubsequences: false)
        guard parts.count == 2 else { return false }
        let name = parts[1]
        guard !name.isEmpty, name != ".", name != "..", !name.contains("\\") else { return false }
        if parts[0] == "media" { return true }
        guard parts[0].hasPrefix("media-p-") else { return false }
        let id = parts[0].dropFirst("media-p-".count)
        return !id.isEmpty && id.allSatisfy(\.isHexDigit)
    }

    /// The file at `location`, if it is one of ours and still there.
    static func resolve(_ location: String?) -> URL? {
        guard let location, isOurs(location) else { return nil }
        let url = Paths.support.appendingPathComponent(location)
        return FileManager.default.fileExists(atPath: url.path) ? url : nil
    }

    /// Deletes our copy of a file (its message was deleted).
    static func forget(_ location: String?) {
        guard let url = resolve(location) else { return }
        try? FileManager.default.removeItem(at: url)
    }

    /// Deletes the files of `persona`'s (or the main identity's) media
    /// directory that no message refers to any more: deleted ones, and
    /// ones whose messages disappeared. Does nothing if some history
    /// can't be read.
    static func sweep(_ n: ThrenodyNode, persona: String?) {
        guard let kept = try? n.fileLocations() else { return }
        sweep(locations: Set(kept), dir: dir(persona), now: Date())
    }

    /// Deletes the files in `dir` that `locations` doesn't list, once
    /// they are older than `grace`.
    static func sweep(locations: Set<String>, dir: URL, now: Date) {
        let fm = FileManager.default
        guard let files = try? fm.contentsOfDirectory(at: dir, includingPropertiesForKeys: [.contentModificationDateKey])
        else { return }
        let prefix = dir.lastPathComponent
        for f in files {
            let modified = (try? f.resourceValues(forKeys: [.contentModificationDateKey]))?.contentModificationDate ?? now
            if now.timeIntervalSince(modified) > grace && !locations.contains("\(prefix)/\(f.lastPathComponent)") {
                try? fm.removeItem(at: f)
            }
        }
    }

    /// Removes a burned anonymous identity's files.
    static func remove(persona: String) {
        try? FileManager.default.removeItem(at: dir(persona))
    }

    /// Removes every identity's files (erasing everything).
    static func removeAll() {
        let fm = FileManager.default
        guard let items = try? fm.contentsOfDirectory(at: Paths.support, includingPropertiesForKeys: nil) else { return }
        for item in items where item.lastPathComponent == "media" || item.lastPathComponent.hasPrefix("media-p-") {
            try? fm.removeItem(at: item)
        }
    }

    /// Whether an image's properties show positive dimensions of at most
    /// `maxPixels`: checked before anything is decoded.
    static func decodable(_ props: [CFString: Any]?) -> Bool {
        guard let props,
              let w = props[kCGImagePropertyPixelWidth] as? Int,
              let h = props[kCGImagePropertyPixelHeight] as? Int
        else { return false }
        return w > 0 && h > 0 && Double(w) * Double(h) <= maxPixels
    }

    /// Whether the image at `url` may be decoded (and handed to QuickLook,
    /// which decodes it in full).
    static func decodable(_ url: URL) -> Bool {
        guard let source = CGImageSourceCreateWithURL(url as CFURL, [kCGImageSourceShouldCache: false] as CFDictionary)
        else { return false }
        return decodable(CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any])
    }

    /// Whether QuickLook may open the file at `url`. Whatever its name
    /// (.psd, .tga, .exr, .ico, …), anything ImageIO reads as an image is
    /// decoded whole by QuickLook, so it must be `decodable`. Call it off
    /// the main thread.
    static func previewable(_ url: URL) -> Bool {
        guard let source = CGImageSourceCreateWithURL(url as CFURL, [kCGImageSourceShouldCache: false] as CFDictionary),
              CGImageSourceGetType(source) != nil
        else { return true }
        return decodable(CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any])
    }

    private static let thumbs: NSCache<NSString, CGImage> = {
        let c = NSCache<NSString, CGImage>()
        c.totalCostLimit = 24 * 1024 * 1024
        return c
    }()

    /// The image at `url` turned upright and at most `maxSide` pixels on
    /// its longer side, or nil if it isn't one, can't be read, or is too
    /// large to decode. Call it off the main thread.
    static func thumbnail(_ url: URL, maxSide: Int) -> CGImage? {
        let key = "\(url.path)@\(maxSide)" as NSString
        if let hit = thumbs.object(forKey: key) { return hit }
        guard let source = CGImageSourceCreateWithURL(url as CFURL, [kCGImageSourceShouldCache: false] as CFDictionary),
              let cg = downsample(source, maxSide: maxSide)
        else { return nil }
        thumbs.setObject(cg, forKey: key, cost: cg.bytesPerRow * cg.height)
        return cg
    }

    /// As `thumbnail`, for bytes not saved yet (a picked photo).
    static func thumbnail(data: Data, maxSide: Int) -> CGImage? {
        guard let source = CGImageSourceCreateWithData(data as CFData, [kCGImageSourceShouldCache: false] as CFDictionary)
        else { return nil }
        return downsample(source, maxSide: maxSide)
    }

    /// The first image of `source`, upright and at most `maxSide` pixels
    /// on its longer side; nil if it isn't `decodable`.
    private static func downsample(_ source: CGImageSource, maxSide: Int) -> CGImage? {
        guard decodable(CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any]) else { return nil }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceShouldCacheImmediately: true,
            kCGImageSourceThumbnailMaxPixelSize: maxSide,
        ]
        return CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary)
    }

    /// Makes a picked file ready to send, as the other apps do: with
    /// `strip` on, an image the node can't strip (HEIC, AVIF, TIFF, …)
    /// is decoded (upright, at most 4096 pixels on its longer side) and
    /// re-encoded as JPEG with no metadata at all. Never falls back to
    /// sending the original. Other files pass through.
    static func prepare(name: String, data: Data, strip: Bool, max: UInt64) throws -> (String, Data) {
        guard strip, imageNeedsConversion(name: name, head: data.prefix(32)) else { return (name, data) }
        let unreadable = AppError("\(name): can't remove its location and camera details, so it wasn't sent")
        guard let source = CGImageSourceCreateWithData(data as CFData, [kCGImageSourceShouldCache: false] as CFDictionary),
              decodable(CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any])
        else { throw unreadable }
        let stem = (name as NSString).deletingPathExtension
        let attempts: [(Int, Double)] = [(4096, 0.92), (2048, 0.85)]
        for (side, quality) in attempts {
            let options: [CFString: Any] = [
                kCGImageSourceCreateThumbnailFromImageAlways: true,
                kCGImageSourceCreateThumbnailWithTransform: true,
                kCGImageSourceShouldCacheImmediately: true,
                kCGImageSourceThumbnailMaxPixelSize: side,
            ]
            guard let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary) else { throw unreadable }
            let out = NSMutableData()
            guard let dest = CGImageDestinationCreateWithData(out as CFMutableData, UTType.jpeg.identifier as CFString, 1, nil)
            else { throw unreadable }
            // Only the quality: no EXIF, GPS, TIFF or maker data.
            CGImageDestinationAddImage(dest, image, [kCGImageDestinationLossyCompressionQuality: quality] as CFDictionary)
            guard CGImageDestinationFinalize(dest) else { throw unreadable }
            if UInt64(out.length) <= max { return ("\(stem.isEmpty ? "photo" : stem).jpg", out as Data) }
        }
        throw AppError("\(name) is too large to send")
    }
}
