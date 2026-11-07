import CoreGraphics
import ImageIO
import UniformTypeIdentifiers
import XCTest
@testable import Threnody

/// Points the app's directories at a fresh temporary one for each test.
class TempDirTestCase: XCTestCase {
    var dir: URL!

    override func setUpWithError() throws {
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("threnody-tests-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        Paths.supportOverride = dir
        Paths.homeOverride = dir.appendingPathComponent("threnody", isDirectory: true)
        Prefs.reset()
    }

    override func tearDownWithError() throws {
        Paths.supportOverride = nil
        Paths.homeOverride = nil
        Prefs.reset()
        try? FileManager.default.removeItem(at: dir)
    }

    /// A node in its own temporary home, without a passphrase, not listening.
    func openNode() throws -> ThrenodyNode {
        let home = dir.appendingPathComponent("node-\(UUID().uuidString)")
        return try ThrenodyNode.open(home: home.path, passphrase: nil, tunnelPort: nil)
    }

    func excludedFromBackup(_ url: URL) throws -> Bool {
        try url.resourceValues(forKeys: [.isExcludedFromBackupKey]).isExcludedFromBackup ?? false
    }
}

final class PrefsTests: TempDirTestCase {
    func testProtectiveSwitchesDefaultOn() {
        for key in Prefs.protective {
            XCTAssertTrue(Prefs.flag(key), key)
        }
        XCTAssertEqual(Prefs.protective.count, 7)
        XCTAssertNil(Prefs.number(Prefs.timer))
        Prefs.setFlag(Prefs.cover, false)
        Prefs.reset()
        XCTAssertFalse(Prefs.flag(Prefs.cover), "read back from the file")
    }

    func testSettingsFileIsOutsideBackups() throws {
        XCTAssertFalse(FileManager.default.fileExists(atPath: Paths.home.path))
        Prefs.setFlag(Prefs.onion, false)
        XCTAssertTrue(FileManager.default.fileExists(atPath: Prefs.file.path))
        XCTAssertTrue(try excludedFromBackup(Paths.home))
    }
}

final class PrivacyTests: TempDirTestCase {
    func testApplyHonoursOptOutsBeforeListening() throws {
        let n = try openNode()
        defer { n.shutdown() }
        for key in Prefs.protective { Prefs.setFlag(key, false) }
        Prefs.setNumber(Prefs.timer, 30)
        Privacy.metered = false
        Privacy.apply(to: n, persona: false)
        XCTAssertNil(n.coverTrafficMs())
        XCTAssertFalse(n.onionFirst())
        XCTAssertFalse(n.stripMetadata())
        XCTAssertFalse(n.reachInternet())
        XCTAssertFalse(n.useVolunteers())
        XCTAssertFalse(n.sendReadReceipts())
        XCTAssertEqual(n.defaultDisappearing(), 30)

        for key in Prefs.protective { Prefs.setFlag(key, true) }
        Prefs.setNumber(Prefs.timer, nil)
        Privacy.metered = true
        defer { Privacy.metered = false }
        Privacy.apply(to: n, persona: false)
        XCTAssertEqual(n.coverTrafficMs(), 10_000)
        XCTAssertTrue(n.onionFirst())
        XCTAssertTrue(n.reachInternet())
        XCTAssertNil(n.defaultDisappearing())

        Privacy.apply(to: n, persona: true)
        XCTAssertFalse(n.reachInternet(), "anonymous identities never use the DHT")
    }
}

final class ConversationTests: XCTestCase {
    private func contact(_ fp: String, account: String?, blocked: Bool = false) -> ContactInfo {
        ContactInfo(
            fingerprint: fp, name: nil, mutuallyApproved: true, verified: false, account: account,
            connected: false, accepted: true, blocked: blocked, sharedProfile: [], revealed: nil,
            revealedInvite: nil, localApproved: true
        )
    }

    func testOwnDevicesAreNotChats() {
        let contacts = [
            contact("ME", account: "MINE"),
            contact("SIBLING", account: "MINE"),
            contact("BLOCKED", account: nil, blocked: true),
            contact("ANN-1", account: "ANN"),
            contact("ANN-2", account: "ANN"),
        ]
        let all = Conversation.from(contacts, me: "ME", account: "MINE")
        XCTAssertEqual(all.map(\.key), ["BLOCKED", "ANN"])
        let chats = all.filter { !$0.blocked }
        XCTAssertEqual(chats.count, 1)
        XCTAssertEqual(chats[0].key, "ANN")
        XCTAssertEqual(Set(chats[0].devices), ["ANN-1", "ANN-2"])
    }
}

final class ChatTargetTests: TempDirTestCase {
    func testNotesRoundTrip() throws {
        let n = try openNode()
        defer { n.shutdown() }
        let notes = ChatTarget(node: n, ref: ChatRef(persona: nil, target: .notes), device: "")
        XCTAssertNil(try notes.send("milk", replyTo: nil))
        let first = try XCTUnwrap(try notes.history(limit: 10).last)
        XCTAssertEqual(first.text, "milk")
        // Notes are deleted by time and device: not in the same millisecond.
        Thread.sleep(forTimeInterval: 0.01)
        XCTAssertNil(try notes.send("and eggs", replyTo: first))
        let reply = try XCTUnwrap(try notes.history(limit: 10).last)
        XCTAssertEqual(reply.text, "and eggs")
        XCTAssertNotNil(reply.quote)
        try notes.delete([first], everyone: false)
        XCTAssertEqual(try notes.history(limit: 10).map(\.text), ["and eggs"])
        XCTAssertFalse(notes.canReact(reply))
    }
}

final class MediaTests: TempDirTestCase {
    func testSavedFilesAreExcludedFromBackupAndNamedSafely() throws {
        let location = try XCTUnwrap(Media.save(name: "../x.jpg", data: Data("jpg".utf8), persona: nil))
        XCTAssertTrue(location.hasPrefix("media/"))
        XCTAssertTrue(location.hasSuffix("-x.jpg"))
        XCTAssertFalse(location.dropFirst("media/".count).contains("/"))
        let url = try XCTUnwrap(Media.resolve(location))
        XCTAssertEqual(try Data(contentsOf: url), Data("jpg".utf8))
        XCTAssertTrue(try excludedFromBackup(Media.dir(nil)))
        let persona = try XCTUnwrap(Media.save(name: "y.png", data: Data("png".utf8), persona: "ab12"))
        XCTAssertTrue(persona.hasPrefix("media-p-ab12/"))
        Media.forget(location)
        XCTAssertNil(Media.resolve(location))
    }

    func testResolveRefusesEscapes() throws {
        let home = Paths.home
        try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
        try Data("secret".utf8).write(to: home.appendingPathComponent("identity.cbor"))
        try FileManager.default.createDirectory(at: Media.dir(nil), withIntermediateDirectories: true)
        XCTAssertNil(Media.resolve("media/../threnody/identity.cbor"))
        XCTAssertNil(Media.resolve("../etc"))
        Media.forget("media/../threnody/identity.cbor")
        XCTAssertTrue(FileManager.default.fileExists(atPath: home.appendingPathComponent("identity.cbor").path))
    }

    func testResolveAcceptsOnlyMediaDirs() throws {
        for d in ["media", "media-p-ab12"] {
            let folder = dir.appendingPathComponent(d)
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            try Data("x".utf8).write(to: folder.appendingPathComponent("x"))
        }
        XCTAssertNotNil(Media.resolve("media/x"))
        XCTAssertNotNil(Media.resolve("media-p-ab12/x"))
        for bad in ["file:///etc/hosts", "/etc/hosts", "media-p-../x", "media/a/b", "media-p-/x", "media/", "other/x"] {
            XCTAssertNil(Media.resolve(bad), bad)
        }
    }

    func testSweepKeepsReferencedAndRecentFiles() throws {
        let folder = Media.dir(nil)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let now = Date()
        func file(_ name: String, age: TimeInterval) throws -> URL {
            let url = folder.appendingPathComponent(name)
            try Data(name.utf8).write(to: url)
            try FileManager.default.setAttributes([.modificationDate: now.addingTimeInterval(-age)], ofItemAtPath: url.path)
            return url
        }
        let kept = try file("kept", age: 3600)
        let recent = try file("recent", age: 60)
        let gone = try file("gone", age: 3600)
        Media.sweep(locations: ["media/kept"], dir: folder, now: now)
        XCTAssertTrue(FileManager.default.fileExists(atPath: kept.path))
        XCTAssertTrue(FileManager.default.fileExists(atPath: recent.path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: gone.path))
    }

    func testHugeImagesAreRefusedWithoutDecoding() throws {
        let size = { (w: Int, h: Int) -> [CFString: Any] in
            [kCGImagePropertyPixelWidth: w, kCGImagePropertyPixelHeight: h]
        }
        XCTAssertFalse(Media.decodable(size(20000, 20000)))
        XCTAssertFalse(Media.decodable(size(0, 10)))
        XCTAssertFalse(Media.decodable([:]))
        XCTAssertFalse(Media.decodable(nil))
        XCTAssertTrue(Media.decodable(size(4000, 3000)))

        let url = dir.appendingPathComponent("square.png")
        try Self.encode(Self.image(64, 64), as: .png).write(to: url)
        let thumb = try XCTUnwrap(Media.thumbnail(url, maxSide: 32))
        XCTAssertLessThanOrEqual(max(thumb.width, thumb.height), 32)
    }

    func testConvertedImagesCarryNoMetadata() throws {
        let gps: [CFString: Any] = [
            kCGImagePropertyGPSLatitude: 48.8584, kCGImagePropertyGPSLatitudeRef: "N",
            kCGImagePropertyGPSLongitude: 2.2945, kCGImagePropertyGPSLongitudeRef: "E",
        ]
        let exif: [CFString: Any] = [kCGImagePropertyExifDateTimeOriginal: "2024:01:01 12:00:00"]
        let props: [CFString: Any] = [kCGImagePropertyGPSDictionary: gps, kCGImagePropertyExifDictionary: exif]
        let tiff = try Self.encode(Self.image(40, 30), as: .tiff, properties: props)
        let original = try XCTUnwrap(CGImageSourceCreateWithData(tiff as CFData, nil))
        let before = CGImageSourceCopyPropertiesAtIndex(original, 0, nil) as? [CFString: Any]
        XCTAssertNotNil(before?[kCGImagePropertyGPSDictionary], "the test image has a location")

        let (name, jpeg) = try Media.prepare(name: "p.tif", data: tiff, strip: true, max: 10_000_000)
        XCTAssertEqual(name, "p.jpg")
        let source = try XCTUnwrap(CGImageSourceCreateWithData(jpeg as CFData, nil))
        XCTAssertEqual(CGImageSourceGetType(source) as String?, UTType.jpeg.identifier)
        let after = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any]
        XCTAssertNil(after?[kCGImagePropertyGPSDictionary])
        let afterExif = after?[kCGImagePropertyExifDictionary] as? [CFString: Any]
        XCTAssertNil(afterExif?[kCGImagePropertyExifDateTimeOriginal])

        let (same, untouched) = try Media.prepare(name: "p.tif", data: tiff, strip: false, max: 10_000_000)
        XCTAssertEqual(same, "p.tif")
        XCTAssertEqual(untouched, tiff)
    }

    static func image(_ w: Int, _ h: Int) throws -> CGImage {
        let ctx = try XCTUnwrap(CGContext(
            data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
            space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        ))
        ctx.setFillColor(red: 0.8, green: 0.2, blue: 0.1, alpha: 1)
        ctx.fill(CGRect(x: 0, y: 0, width: w, height: h))
        return try XCTUnwrap(ctx.makeImage())
    }

    static func encode(_ image: CGImage, as type: UTType, properties: [CFString: Any] = [:]) throws -> Data {
        let out = NSMutableData()
        let dest = try XCTUnwrap(CGImageDestinationCreateWithData(out as CFMutableData, type.identifier as CFString, 1, nil))
        CGImageDestinationAddImage(dest, image, properties as CFDictionary)
        XCTAssertTrue(CGImageDestinationFinalize(dest))
        return out as Data
    }
}

final class ListMarkdownTests: XCTestCase {
    func testLinksAreNotRendered() {
        let a = ListMarkdown.render("[bank](https://evil.example) **b**")
        XCTAssertFalse(a.runs.contains { $0.link != nil })
        XCTAssertTrue(a.runs.contains { $0.inlinePresentationIntent?.contains(.stronglyEmphasized) == true })
        XCTAssertEqual(String(a.characters), "bank b")
    }
}

final class ChatRefTests: XCTestCase {
    func testPersonaChatsAreDistinctAndRoundTrip() {
        XCTAssertNotEqual(ChatRef(persona: nil, target: .contact("K")), ChatRef(persona: "p1", target: .contact("K")))
        let refs: [ChatRef] = [
            ChatRef(persona: nil, target: .contact("K")), ChatRef(persona: "ab", target: .contact("K")),
            ChatRef(persona: nil, target: .group("0a0b")), ChatRef(persona: "ab", target: .group("0a0b")),
            ChatRef(persona: nil, target: .notes), ChatRef(persona: nil, target: .groupInvite("0c")),
        ]
        for r in refs {
            XCTAssertEqual(ChatRef(encoded: r.encoded), r, r.encoded)
        }
        XCTAssertNil(ChatRef(encoded: "a bare key"))
        XCTAssertEqual(ChatRef(persona: nil, target: .notes).chatKey, "notes")
    }
}

final class EmojiTests: XCTestCase {
    func testOnlySingleEmojiAreAccepted() {
        for ok in ["👍", "❤️", "👍🏽", "🇫🇷", "👨‍👩‍👧"] {
            XCTAssertTrue(Emoji.isSingle(ok), ok)
        }
        for bad in ["a", "👍👍", "", "1", "#"] {
            XCTAssertFalse(Emoji.isSingle(bad), bad)
        }
    }
}

final class AlbumTests: XCTestCase {
    private func entry(_ at: UInt64, album: UInt64?) -> HistoryEntry {
        HistoryEntry(
            atMs: at, outgoing: true, device: "D", text: "", disappearing: false,
            file: album.map { FileInfo(name: "p.jpg", size: 1, location: nil, sensitive: false, album: $0, clip: nil, fetch: .notHere) },
            delivered: false, read: false, id: at, edited: false, recipients: 0, deliveredTo: 0, reactions: [],
            quote: nil, mentions: [], poll: nil, forwarded: false, list: nil, tick: .sent
        )
    }

    func testAlbumsAreGroupedAndOthersAlone() {
        let items = ChatItem.group([entry(1, album: nil), entry(2, album: 7), entry(3, album: 7), entry(4, album: 0), entry(5, album: 7)])
        XCTAssertEqual(items.map { $0.entries.map(\.atMs) }, [[1], [2, 3], [4], [5]])
    }
}
