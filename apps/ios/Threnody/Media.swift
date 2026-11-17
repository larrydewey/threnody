import Foundation
import ImageIO
import UniformTypeIdentifiers
import os

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
///
/// Every kept file is sealed under the main identity (`sealMedia`), so a
/// copy of the app's files is useless without the identity's key, as the
/// identity itself is. Pictures decode from bytes decrypted in memory;
/// QuickLook and Share get a decrypted copy in the temporary directory
/// (`plainCopy`), deleted when QuickLook closes, on lock and at start.
/// Files kept before sealing are read as they are until `sealAll` seals
/// them at start.
enum Media {
    /// The main node, whose identity seals kept files: set when it starts.
    private static let sealerLock = OSAllocatedUnfairLock<ThrenodyNode?>(uncheckedState: nil)
    static var sealer: ThrenodyNode? {
        get { sealerLock.withLockUnchecked { $0 } }
        set { sealerLock.withLockUnchecked { $0 = newValue } }
    }

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

    /// Keeps `data` privately, sealed; returns its location, or nil if it couldn't.
    static func save(name: String, data: Data, persona: String?) -> String? {
        guard let sealer else { return nil }
        let folder = dir(persona)
        let location = "\(dirName(persona))/\(UUID().uuidString)-\(safeFileName(name: name))"
        do {
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            try Paths.excludeFromBackup(folder)
            let path = Paths.support.appendingPathComponent(location).path
            try sealer.sealMedia(data: data, dst: path)
            // Written while locked (a background receipt), read only
            // unlocked; sealed whether or not the class can be set now.
            try? FileManager.default.setAttributes([.protectionKey: FileProtectionType.completeUnlessOpen], ofItemAtPath: path)
            return location
        } catch {
            return nil
        }
    }

    /// What the kept file at `url` holds: decrypted in memory if sealed,
    /// as it is if kept before sealing. Call it off the main thread.
    static func read(_ url: URL) -> Data? {
        guard isSealedMedia(path: url.path) else { return try? Data(contentsOf: url) }
        return try? sealer?.openMedia(path: url.path)
    }

    /// Where decrypted copies wait for QuickLook or Share: the temporary
    /// directory, never backed up.
    static var plainDir: URL {
        FileManager.default.temporaryDirectory.appendingPathComponent("plain", isDirectory: true)
    }

    /// A decrypted copy of the kept file at `url`, under the name it was
    /// sent with, for QuickLook or Share; `url` itself if it isn't sealed.
    /// Delete it with `dropPlain`. Call it off the main thread.
    static func plainCopy(_ url: URL) throws -> URL {
        guard isSealedMedia(path: url.path) else { return url }
        guard let sealer else { throw AppError("Threnody isn't running") }
        let dir = plainDir.appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        // Kept as "<UUID>-<name>".
        let kept = url.lastPathComponent
        let name = kept.count > 37 ? String(kept.dropFirst(37)) : kept
        let out = dir.appendingPathComponent(name.isEmpty ? "file" : name)
        do {
            try sealer.openMediaFile(src: url.path, dst: out.path)
        } catch {
            try? FileManager.default.removeItem(at: dir)
            throw error
        }
        return out
    }

    /// Deletes a decrypted copy `plainCopy` made (QuickLook closed); never
    /// a kept file.
    static func dropPlain(_ url: URL) {
        let dir = url.deletingLastPathComponent()
        guard dir.deletingLastPathComponent().standardizedFileURL.path == plainDir.standardizedFileURL.path else { return }
        _ = try? secureDeleteFile(path: url.path)
        try? FileManager.default.removeItem(at: dir)
    }

    /// Deletes every decrypted copy (at start and on lock).
    static func wipePlain() {
        let fm = FileManager.default
        if let copies = fm.enumerator(at: plainDir, includingPropertiesForKeys: nil) {
            for case let f as URL in copies { _ = try? secureDeleteFile(path: f.path) }
        }
        try? fm.removeItem(at: plainDir)
    }

    /// Seals, in place, files every identity's folder kept before media was
    /// sealed (the plaintext is overwritten). Run before the first sweep.
    static func sealAll() {
        let fm = FileManager.default
        guard let sealer, let items = try? fm.contentsOfDirectory(at: Paths.support, includingPropertiesForKeys: nil) else { return }
        for item in items where item.lastPathComponent == "media" || item.lastPathComponent.hasPrefix("media-p-") {
            _ = try? sealer.sealMediaDir(dir: item.path)
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
    /// large to decode. A sealed one is decrypted in memory, never to a
    /// file; thumbnails are only cached in memory. Call it off the main
    /// thread.
    static func thumbnail(_ url: URL, maxSide: Int) -> CGImage? {
        let key = "\(url.path)@\(maxSide)" as NSString
        if let hit = thumbs.object(forKey: key) { return hit }
        guard let data = read(url),
              let source = CGImageSourceCreateWithData(data as CFData, [kCGImageSourceShouldCache: false] as CFDictionary),
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
