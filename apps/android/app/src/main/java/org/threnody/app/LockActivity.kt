package org.threnody.app

import android.app.Activity
import android.app.KeyguardManager
import android.hardware.biometrics.BiometricManager
import android.hardware.biometrics.BiometricPrompt
import android.os.Build
import android.os.Bundle
import android.os.CancellationSignal
import android.text.InputType
import android.view.Gravity
import android.view.KeyEvent
import android.view.View
import android.view.WindowManager
import android.view.inputmethod.EditorInfo
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView
import uniffi.threnody_ffi.LockKind
import uniffi.threnody_ffi.LockOutcome
import uniffi.threnody_ffi.LockSettings
import uniffi.threnody_ffi.biometricUnlocked
import uniffi.threnody_ffi.unlockApp

/**
 * The lock screen ([AppLock]): the app's name, one PIN or passphrase
 * field, and a biometrics button when they're on. Nothing else, so it
 * shows neither who uses the app nor whether a duress PIN is set. The
 * duress PIN looks exactly like the right one here; the app then erases
 * everything and starts afresh.
 */
class LockActivity : Activity() {
    private lateinit var field: EditText
    private lateinit var unlock: View
    private lateinit var bio: View
    private lateinit var erase: View
    private lateinit var status: TextView
    private val ui = android.os.Handler(android.os.Looper.getMainLooper())
    private var busy = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Never in screenshots or recents, whatever screen security says.
        window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER_HORIZONTAL
            setBackgroundColor(color(R.color.background))
            setPadding(dp(Design.xl), dp(96), dp(Design.xl), dp(Design.xl))
        }
        root.addView(label("Threnody", Design.typeHeadline, weight = Design.weightBold).apply {
            gravity = Gravity.CENTER
        }, matchWrap)
        field = EditText(this).apply {
            gravity = Gravity.CENTER
            imeOptions = EditorInfo.IME_ACTION_DONE
            setOnEditorActionListener { _, id, e ->
                if (id == EditorInfo.IME_ACTION_DONE || e?.keyCode == KeyEvent.KEYCODE_ENTER) submit()
                true
            }
        }
        root.addView(field, matchWrap.apply { topMargin = dp(Design.xl) })
        status = label("", Design.typeBodySmall, R.color.text_secondary, maxLines = 3).apply {
            gravity = Gravity.CENTER
        }
        root.addView(status, matchWrap.apply { topMargin = dp(Design.sm) })
        unlock = primaryButton("Unlock") { submit() }
        root.addView(unlock, matchWrap.apply { topMargin = dp(Design.lg) })
        bio = secondaryButton("Use biometrics") { biometric() }
        root.addView(bio, matchWrap.apply { topMargin = dp(Design.sm) })
        erase = secondaryButton("Erase all Threnody data…") { AppLockUi.eraseAll(this) }
        root.addView(erase, matchWrap.apply { topMargin = dp(Design.sm) })
        setContentView(root)
        listOf(field, unlock, bio, erase).forEach { it.visibility = View.GONE }

        // Back leaves the app instead of revealing it. API 33+ opts into
        // predictive back (the manifest), so onBackPressed isn't called there.
        if (Build.VERSION.SDK_INT >= 33) {
            onBackInvokedDispatcher.registerOnBackInvokedCallback(
                android.window.OnBackInvokedDispatcher.PRIORITY_DEFAULT,
            ) { moveTaskToBack(true) }
        }
        load(prompt = true)
    }

    @Deprecated("API 29-32 only; 33+ uses the OnBackInvokedDispatcher")
    @Suppress("DEPRECATION")
    override fun onBackPressed() {
        moveTaskToBack(true)
    }

    override fun onResume() {
        super.onResume()
        // Unlocked elsewhere, or turned off (an interrupted erase finished).
        if (!AppLock.locked() || !AppLock.enabled(this)) {
            AppLock.onUnlocked()
            finish()
        }
    }

    /** Reads the settings if they aren't in yet, then shows what unlocks. */
    private fun load(prompt: Boolean) {
        val known = AppLock.settings
        if (known != null) return show(known, prompt)
        Threading.background {
            val s = AppLock.reload(this)
            runOnUiThread { if (s != null) show(s, prompt) else showUnreadable() }
        }
    }

    private fun showUnreadable() {
        status.text = "App lock settings can't be read."
        erase.visibility = View.VISIBLE
    }

    private fun show(s: LockSettings, prompt: Boolean) {
        val kind = s.kind
        field.visibility = if (kind == null) View.GONE else View.VISIBLE
        unlock.visibility = field.visibility
        bio.visibility = if (s.biometric) View.VISIBLE else View.GONE
        field.hint = if (kind == LockKind.PASSPHRASE) "Passphrase" else "PIN"
        field.inputType = if (kind == LockKind.PASSPHRASE) {
            InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
        } else {
            InputType.TYPE_CLASS_NUMBER or InputType.TYPE_NUMBER_VARIATION_PASSWORD
        }
        waitUntil(s.retryAtMs)
        if (kind == null) bio.visibility = View.VISIBLE
        if (prompt && (kind == null || s.biometric)) biometric()
    }

    /** Refuses entries until [retryAtMs] (wall clock), with a countdown. */
    private fun waitUntil(retryAtMs: ULong) {
        val left = retryAtMs.toLong() - System.currentTimeMillis()
        if (left <= 0) {
            field.isEnabled = true
            unlock.isEnabled = true
            return
        }
        field.isEnabled = false
        unlock.isEnabled = false
        status.text = "Try again in ${(left + 999) / 1000} s"
        ui.postDelayed({ waitUntil(retryAtMs) }, minOf(left, 1_000))
    }

    private fun submit() {
        val text = field.text.toString()
        if (busy || text.isEmpty() || !field.isEnabled) return
        busy = true
        // The same for every entry: the duress PIN looks like the right one.
        status.text = "Unlocking…"
        unlock.isEnabled = false
        val home = Threnody.home(this)
        Threading.background {
            val outcome = try {
                unlockApp(home, KeyVault.secret(this), text)
            } catch (e: Exception) {
                runOnUiThread {
                    busy = false
                    unlock.isEnabled = true
                    status.text = e.message ?: "App lock settings can't be read."
                }
                return@background
            }
            when (outcome) {
                is LockOutcome.Erased -> {
                    Threnody.wipe(this, alreadyErased = true)
                    runOnUiThread { Threnody.restartApp(this) }
                }
                is LockOutcome.Unlocked -> runOnUiThread {
                    AppLock.onUnlocked()
                    finish()
                }
                is LockOutcome.Wrong -> runOnUiThread {
                    busy = false
                    field.text.clear()
                    status.text = "Wrong PIN"
                    if (outcome.retryAtMs > 0u) waitUntil(outcome.retryAtMs) else unlock.isEnabled = true
                }
                is LockOutcome.Wait -> runOnUiThread {
                    busy = false
                    field.text.clear()
                    waitUntil(outcome.retryAtMs)
                }
            }
        }
    }

    /**
     * Biometrics. Without an app PIN (device lock) the phone's own PIN may
     * stand in for them. With an app PIN only strong biometrics count: the
     * phone's PIN must not get around the app PIN or the duress PIN.
     */
    private fun biometric() {
        val device = AppLock.settings?.kind == null
        val b = BiometricPrompt.Builder(this).setTitle("Unlock Threnody")
        if (Build.VERSION.SDK_INT >= 30) {
            b.setAllowedAuthenticators(
                if (device) BiometricManager.Authenticators.BIOMETRIC_STRONG or BiometricManager.Authenticators.DEVICE_CREDENTIAL
                else BiometricManager.Authenticators.BIOMETRIC_STRONG,
            )
        } else if (device) {
            @Suppress("DEPRECATION")
            b.setDeviceCredentialAllowed(true)
        }
        // A biometric-only prompt must have a way out, or build() throws.
        if (!device) b.setNegativeButton("Use PIN", mainExecutor) { _, _ -> field.requestFocus() }
        val prompt = try {
            b.build()
        } catch (e: Exception) {
            status.text = "Biometrics aren't available"
            return
        }
        prompt.authenticate(CancellationSignal(), mainExecutor, object : BiometricPrompt.AuthenticationCallback() {
            override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult?) {
                passed()
            }

            override fun onAuthenticationError(code: Int, message: CharSequence?) {
                // Only a screen lock that is gone opens the app: removing it
                // took the phone's PIN. Anything else (the sensor busy,
                // locked out, cancelled) keeps the lock.
                val secure = getSystemService(KeyguardManager::class.java)?.isDeviceSecure ?: true
                if (device && !secure) {
                    android.widget.Toast.makeText(
                        this@LockActivity,
                        "Your phone has no screen lock any more; set an app PIN to keep Threnody locked",
                        android.widget.Toast.LENGTH_LONG,
                    ).show()
                    passed()
                } else {
                    status.text = if (device) "Try again" else ""
                    bio.visibility = View.VISIBLE
                }
            }
        })
    }

    private fun passed() {
        val home = Threnody.home(this)
        Threading.background {
            try {
                biometricUnlocked(home, KeyVault.secret(this))
            } catch (_: Exception) {
            }
        }
        AppLock.onUnlocked()
        finish()
    }
}
