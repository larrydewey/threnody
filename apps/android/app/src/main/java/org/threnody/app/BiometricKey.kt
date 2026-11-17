package org.threnody.app

import android.app.Activity
import android.content.Context
import android.hardware.biometrics.BiometricManager
import android.hardware.biometrics.BiometricPrompt
import android.os.Build
import android.os.CancellationSignal
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyPermanentlyInvalidatedException
import android.security.keystore.KeyProperties
import java.io.File
import java.security.KeyStore
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Binds biometric unlock of an app PIN or passphrase to the fingerprints
 * (or faces) enrolled when it was turned on.
 *
 * A bare BiometricPrompt success says only that *some* enrolled biometric
 * matched, and anyone who knows the phone's PIN can enrol their own. So
 * turning biometrics on makes a Keystore AES key that needs a strong
 * biometric for every use and is destroyed when biometrics are added, and
 * encrypts a random token with it. The lock screen only opens when the
 * prompt's cipher then decrypts that token. Device-lock mode (no app PIN)
 * doesn't use this: the phone's own PIN may open it anyway.
 */
object BiometricKey {
    private const val ALIAS = "threnody-biometric-unlock"
    private const val FILE = "biometric-token.bin"
    private const val VERSION: Byte = 1

    /** The key is gone or was invalidated by a change of enrolled biometrics. */
    class Gone : Exception("biometric key gone")

    private fun file(ctx: Context) = File(ctx.noBackupFilesDir, FILE)

    private fun keyStore() = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }

    private fun key(): SecretKey? = keyStore().getKey(ALIAS, null) as SecretKey?

    /** Whether the phone can do a strong biometric now. */
    fun available(ctx: Context): Boolean {
        val m = ctx.getSystemService(BiometricManager::class.java) ?: return false
        return if (Build.VERSION.SDK_INT >= 30) {
            m.canAuthenticate(BiometricManager.Authenticators.BIOMETRIC_STRONG) == BiometricManager.BIOMETRIC_SUCCESS
        } else {
            @Suppress("DEPRECATION")
            m.canAuthenticate() == BiometricManager.BIOMETRIC_SUCCESS
        }
    }

    /** Deletes the key and its token. Never throws. */
    @Synchronized
    fun forget(ctx: Context) {
        try {
            file(ctx).delete()
        } catch (_: Exception) {
        }
        try {
            keyStore().deleteEntry(ALIAS)
        } catch (_: Exception) {
        }
    }

    private fun newKey(): SecretKey {
        val b = KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            .setUserAuthenticationRequired(true)
            .setInvalidatedByBiometricEnrollment(true)
        if (Build.VERSION.SDK_INT >= 30) {
            b.setUserAuthenticationParameters(0, KeyProperties.AUTH_BIOMETRIC_STRONG)
        } else {
            @Suppress("DEPRECATION")
            b.setUserAuthenticationValidityDurationSeconds(-1)
        }
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
            .apply { init(b.build()) }
            .generateKey()
    }

    /** A strong-biometric-only prompt; [cancel] is the negative button. */
    private fun prompt(a: Activity, title: String, cancel: String, onCancel: () -> Unit): BiometricPrompt {
        val b = BiometricPrompt.Builder(a).setTitle(title)
        if (Build.VERSION.SDK_INT >= 30) {
            b.setAllowedAuthenticators(BiometricManager.Authenticators.BIOMETRIC_STRONG)
        }
        // A CryptoObject also limits API 29 to strong biometrics.
        b.setNegativeButton(cancel, a.mainExecutor) { _, _ -> onCancel() }
        return b.build()
    }

    /**
     * Turning biometrics on: makes a fresh key, asks for a biometric to
     * encrypt the token with it, and runs [done] with whether it worked.
     * [done] runs on the main thread.
     */
    fun enroll(a: Activity, done: (ok: Boolean, error: String?) -> Unit) {
        if (!available(a)) return done(false, "Set up a fingerprint (or another strong biometric) on your phone first")
        val cipher = try {
            forget(a)
            Cipher.getInstance("AES/GCM/NoPadding").apply { init(Cipher.ENCRYPT_MODE, newKey()) }
        } catch (e: Exception) {
            forget(a)
            return done(false, "Biometrics aren't available: ${e.message}")
        }
        val p = try {
            prompt(a, "Turn on biometric unlock", "Cancel") { forget(a); done(false, null) }
        } catch (e: Exception) {
            forget(a)
            return done(false, "Biometrics aren't available: ${e.message}")
        }
        p.authenticate(BiometricPrompt.CryptoObject(cipher), CancellationSignal(), a.mainExecutor,
            object : BiometricPrompt.AuthenticationCallback() {
                override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult?) {
                    try {
                        val c = result?.cryptoObject?.cipher ?: error("no cipher")
                        val token = ByteArray(32).also { SecureRandom().nextBytes(it) }
                        val sealed = c.doFinal(token)
                        val bytes = byteArrayOf(VERSION, c.iv.size.toByte()) + c.iv + sealed
                        val tmp = File(a.noBackupFilesDir, "$FILE.tmp")
                        tmp.writeBytes(bytes)
                        if (!tmp.renameTo(file(a))) error("could not save the biometric token")
                        done(true, null)
                    } catch (e: Exception) {
                        forget(a)
                        done(false, "Couldn't turn biometrics on: ${e.message}")
                    }
                }

                override fun onAuthenticationError(code: Int, message: CharSequence?) {
                    // The negative button reports through its own listener.
                    if (code == BiometricPrompt.BIOMETRIC_ERROR_CANCELED ||
                        code == BiometricPrompt.BIOMETRIC_ERROR_USER_CANCELED) {
                        forget(a)
                        done(false, null)
                        return
                    }
                    forget(a)
                    done(false, message?.toString() ?: "Couldn't turn biometrics on")
                }
            })
    }

    /**
     * The lock screen: shows a strong-biometric prompt bound to the key and
     * calls [ok] only once its cipher decrypted the token. Throws [Gone]
     * (before showing anything) when the key or token is missing or the
     * enrolled biometrics changed; the caller turns biometrics off.
     */
    fun unlock(
        a: Activity,
        ok: () -> Unit,
        failed: (String) -> Unit,
        onError: (code: Int, message: CharSequence?) -> Unit,
        usePin: () -> Unit,
    ) {
        val bytes = try {
            file(a).readBytes()
        } catch (_: Exception) {
            throw Gone()
        }
        if (bytes.size <= 2 || bytes[0] != VERSION) throw Gone()
        val ivLen = bytes[1].toInt()
        if (ivLen <= 0 || bytes.size <= 2 + ivLen) throw Gone()
        val iv = bytes.copyOfRange(2, 2 + ivLen)
        val cipher = try {
            val key = key() ?: throw Gone()
            Cipher.getInstance("AES/GCM/NoPadding").apply { init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(128, iv)) }
        } catch (_: KeyPermanentlyInvalidatedException) {
            throw Gone()
        }
        val p = prompt(a, "Unlock Threnody", "Use PIN", usePin)
        p.authenticate(BiometricPrompt.CryptoObject(cipher), CancellationSignal(), a.mainExecutor,
            object : BiometricPrompt.AuthenticationCallback() {
                override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult?) {
                    val good = try {
                        val c = result?.cryptoObject?.cipher ?: error("no cipher")
                        c.doFinal(bytes, 2 + ivLen, bytes.size - 2 - ivLen).size == 32
                    } catch (_: Exception) {
                        false
                    }
                    if (good) ok() else failed("Biometric unlock failed; enter your PIN")
                }

                override fun onAuthenticationError(code: Int, message: CharSequence?) = onError(code, message)
            })
    }
}
