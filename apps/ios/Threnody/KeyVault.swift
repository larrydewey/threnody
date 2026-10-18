import CryptoKit
import Foundation
import Security

/// Seals the node's identity under the device's keystore (spec §3.1).
///
/// The identity file is sealed with a random passphrase (Argon2id, as the
/// CLI's `--passphrase`). Where the phone has a Secure Enclave, that
/// passphrase is stored encrypted to a P-256 key that never leaves it;
/// elsewhere (the simulator) it is a Keychain item. Both are bound to
/// this device and readable after the first unlock, so a background
/// launch can open the node, and neither is in backups.
enum KeyVault {
    private static let tag = Data("org.threnody.identity".utf8)
    private static let service = "org.threnody.identity"
    private static let algorithm = SecKeyAlgorithm.eciesEncryptionCofactorVariableIVX963SHA256AESGCM
    private static let version: UInt8 = 1

    enum Failure: LocalizedError {
        case keyGone, badFile, keychain(OSStatus)
        var errorDescription: String? {
            switch self {
            case .keyGone: "The Secure Enclave key is gone; the identity can't be opened."
            case .badFile: "Unknown passphrase file."
            case .keychain(let s): "Keychain error \(s)."
            }
        }
    }

    /// The passphrase to open the node in `home` with, sealing an existing
    /// plain identity first. A new identity is created sealed by `open`.
    static func passphrase(home: URL) throws -> String {
        let pw = try secret()
        if try !identityIsSealed(home: home.path),
           FileManager.default.fileExists(atPath: home.appendingPathComponent("identity.cbor").path) {
            try changePassphrase(home: home.path, current: nil, new: pw)
        }
        return pw
    }

    /// Where the key lives, for the settings screen.
    static var describe: String {
        FileManager.default.fileExists(atPath: file.path)
            ? "sealed with a Secure Enclave key" : "sealed with a Keychain item (this device only)"
    }

    static func secret() throws -> String {
        if FileManager.default.fileExists(atPath: file.path) { return try read(file) }
        if let pw = try keychainItem() { return pw }
        let pw = Data((0..<32).map { _ in UInt8.random(in: .min ... .max) }).base64EncodedString()
        do {
            guard SecureEnclave.isAvailable else { throw Failure.keyGone }
            try seal(pw)
        } catch {
            try? FileManager.default.removeItem(at: file.appendingPathExtension("tmp"))
            try storeKeychainItem(pw)
        }
        return pw
    }

    private static var file: URL {
        URL.applicationSupportDirectory.appendingPathComponent("identity-passphrase.bin")
    }

    // MARK: Secure Enclave

    private static func enclaveKey() throws -> SecKey? {
        let query: [CFString: Any] = [
            kSecClass: kSecClassKey,
            kSecAttrApplicationTag: tag,
            kSecAttrKeyType: kSecAttrKeyTypeECSECPrimeRandom,
            kSecAttrTokenID: kSecAttrTokenIDSecureEnclave,
            kSecReturnRef: true,
        ]
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let item else { throw Failure.keychain(status) }
        return (item as! SecKey)
    }

    private static func newEnclaveKey() throws -> SecKey {
        var error: Unmanaged<CFError>?
        guard let access = SecAccessControlCreateWithFlags(
            nil, kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly, .privateKeyUsage, &error
        ) else { throw error!.takeRetainedValue() as Error }
        let attributes: [CFString: Any] = [
            kSecAttrKeyType: kSecAttrKeyTypeECSECPrimeRandom,
            kSecAttrKeySizeInBits: 256,
            kSecAttrTokenID: kSecAttrTokenIDSecureEnclave,
            kSecPrivateKeyAttrs: [
                kSecAttrIsPermanent: true,
                kSecAttrApplicationTag: tag,
                kSecAttrAccessControl: access,
            ] as [CFString: Any],
        ]
        guard let key = SecKeyCreateRandomKey(attributes as CFDictionary, &error) else {
            throw error!.takeRetainedValue() as Error
        }
        return key
    }

    /// Stores the passphrase encrypted to the enclave key, checking it reads back first.
    private static func seal(_ pw: String) throws {
        let key = try enclaveKey() ?? newEnclaveKey()
        var error: Unmanaged<CFError>?
        guard let pub = SecKeyCopyPublicKey(key),
              let sealed = SecKeyCreateEncryptedData(pub, algorithm, Data(pw.utf8) as CFData, &error) as Data?
        else { throw error.map { $0.takeRetainedValue() as Error } ?? Failure.keyGone }
        try FileManager.default.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
        let tmp = file.appendingPathExtension("tmp")
        try (Data([version]) + sealed).write(to: tmp, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
        guard try read(tmp) == pw else { throw Failure.badFile }
        try FileManager.default.moveItem(at: tmp, to: file)
        var url = file
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        try url.setResourceValues(values)
    }

    private static func read(_ url: URL) throws -> String {
        let bytes = try Data(contentsOf: url)
        guard bytes.first == version else { throw Failure.badFile }
        guard let key = try enclaveKey() else { throw Failure.keyGone }
        var error: Unmanaged<CFError>?
        guard let plain = SecKeyCreateDecryptedData(key, algorithm, bytes.dropFirst() as CFData, &error) as Data?,
              let pw = String(data: plain, encoding: .utf8)
        else { throw error.map { $0.takeRetainedValue() as Error } ?? Failure.badFile }
        return pw
    }

    // MARK: Keychain fallback

    private static func keychainItem() throws -> String? {
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: service,
            kSecReturnData: true,
        ]
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = item as? Data else { throw Failure.keychain(status) }
        return String(data: data, encoding: .utf8)
    }

    private static func storeKeychainItem(_ pw: String) throws {
        let item: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: service,
            kSecAttrAccessible: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
            kSecValueData: Data(pw.utf8),
        ]
        let status = SecItemAdd(item as CFDictionary, nil)
        guard status == errSecSuccess else { throw Failure.keychain(status) }
    }
}
