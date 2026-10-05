package org.threnody.app

import android.content.Context
import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyInfo
import android.security.keystore.KeyProperties
import android.security.keystore.StrongBoxUnavailableException
import android.util.Base64
import java.io.File
import java.security.KeyStore
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.SecretKeyFactory
import javax.crypto.spec.GCMParameterSpec
import uniffi.threnody_ffi.changePassphrase
import uniffi.threnody_ffi.identityIsSealed

/**
 * Seals the node's identity under the Android Keystore (spec §3.1).
 *
 * The identity file is sealed with a random passphrase (Argon2id, as the
 * CLI's `--passphrase`). That passphrase is stored encrypted with an AES
 * key that lives in the Keystore, in StrongBox where the phone has it,
 * else in the TEE. A copy of the app's files is useless off the device.
 * The key needs no user authentication, so the background service can
 * open the node after a restart.
 */
object KeyVault {
    private const val ALIAS = "threnody-identity"
    private const val FILE = "identity-passphrase.bin"
    private const val VERSION: Byte = 1

    /**
     * The passphrase to open the node in [home] with, sealing an existing
     * plain identity first. A new identity is created sealed by `open`.
     */
    fun passphrase(ctx: Context, home: String): String {
        val pw = secret(ctx)
        // A plain identity (from before this, or interrupted mid-way): seal it.
        if (!identityIsSealed(home) && File(home, "identity.cbor").exists()) {
            changePassphrase(home, null, pw)
            Threnody.say("* identity key sealed with the Android Keystore")
        }
        return pw
    }

    /** The Keystore-protected passphrase that seals identities on this device. */
    fun secret(ctx: Context): String {
        val file = File(ctx.noBackupFilesDir, FILE)
        return if (file.exists()) read(file) else create(file)
    }

    /** Where the Keystore key lives, for the diagnostics screen. */
    fun describe(): String = try {
        val key = key() ?: return "no Keystore key"
        val info = SecretKeyFactory.getInstance(key.algorithm, "AndroidKeyStore")
            .getKeySpec(key, KeyInfo::class.java) as KeyInfo
        val level = if (Build.VERSION.SDK_INT >= 31) {
            when (info.securityLevel) {
                KeyProperties.SECURITY_LEVEL_STRONGBOX -> "StrongBox"
                KeyProperties.SECURITY_LEVEL_TRUSTED_ENVIRONMENT -> "TEE"
                KeyProperties.SECURITY_LEVEL_SOFTWARE -> "software"
                else -> "unknown"
            }
        } else {
            @Suppress("DEPRECATION")
            if (info.isInsideSecureHardware) "secure hardware" else "software"
        }
        "sealed with an Android Keystore key ($level)"
    } catch (e: Exception) {
        "Keystore: ${e.message}"
    }

    private fun keyStore() = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }

    private fun key(): SecretKey? = keyStore().getKey(ALIAS, null) as SecretKey?

    private fun newKey(strongBox: Boolean): SecretKey {
        val spec = KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            .setIsStrongBoxBacked(strongBox)
            .build()
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
            .apply { init(spec) }
            .generateKey()
    }

    /** Makes a passphrase and stores it encrypted, checking it reads back first. */
    private fun create(file: File): String {
        val key = key() ?: try {
            newKey(strongBox = true)
        } catch (_: StrongBoxUnavailableException) {
            newKey(strongBox = false)
        }
        val pw = Base64.encodeToString(ByteArray(32).also { SecureRandom().nextBytes(it) }, Base64.NO_WRAP)
        val cipher = Cipher.getInstance("AES/GCM/NoPadding").apply { init(Cipher.ENCRYPT_MODE, key) }
        val sealed = cipher.doFinal(pw.toByteArray())
        val bytes = byteArrayOf(VERSION, cipher.iv.size.toByte()) + cipher.iv + sealed
        // Write atomically, and only rely on it once it decrypts.
        val tmp = File(file.parentFile, "$FILE.tmp")
        tmp.writeBytes(bytes)
        check(read(tmp) == pw) { "Keystore round trip failed" }
        if (!tmp.renameTo(file)) error("could not save the sealed passphrase")
        return pw
    }

    private fun read(file: File): String {
        val b = file.readBytes()
        require(b.size > 2 && b[0] == VERSION) { "unknown passphrase file" }
        val ivLen = b[1].toInt()
        val iv = b.copyOfRange(2, 2 + ivLen)
        val key = key() ?: error("the Keystore key is gone; the identity can't be opened")
        val cipher = Cipher.getInstance("AES/GCM/NoPadding").apply {
            init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(128, iv))
        }
        return String(cipher.doFinal(b, 2 + ivLen, b.size - 2 - ivLen))
    }
}
