package org.threnody.app

import android.app.Activity
import android.app.AlertDialog
import android.app.KeyguardManager
import android.content.Context
import android.hardware.biometrics.BiometricManager
import android.hardware.biometrics.BiometricPrompt
import android.os.Build
import android.os.CancellationSignal
import android.text.Editable
import android.text.InputType
import android.text.TextWatcher
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.Toast
import uniffi.threnody_ffi.LockKind
import uniffi.threnody_ffi.LockSettings
import uniffi.threnody_ffi.appLock
import uniffi.threnody_ffi.clearAppLock
import uniffi.threnody_ffi.lockSecretOk
import uniffi.threnody_ffi.setAppLock
import uniffi.threnody_ffi.setDuressPin
import uniffi.threnody_ffi.setLockPolicy

/**
 * App lock's settings (Privacy & Security → App lock…), the duress PIN,
 * "Erase all Threnody data", and the one-time notice that app lock is on.
 */
object AppLockUi {
    private const val UI_PREFS = "ui"
    private const val OFFERED = "lock_offered"

    private val AWAY = listOf<Pair<String, UInt>>(
        "Immediately" to 0u, "After 1 minute" to 60u, "After 5 minutes" to 300u,
        "After 15 minutes" to 900u, "After 1 hour" to 3600u,
    )
    private val IDLE = listOf<Pair<String, UInt?>>(
        "Never" to null, "After 1 minute" to 60u, "After 5 minutes" to 300u, "After 15 minutes" to 900u,
    )

    private const val DURESS_HELP = "If someone makes you unlock Threnody, enter this instead of your PIN. " +
        "Everything on this device is erased without warning, and Threnody opens as if newly installed. " +
        "Your other devices and your contacts are not told. Biometric unlock always opens your real data: " +
        "if someone could force your finger or face, turn biometrics off."

    private fun toast(a: Activity, msg: String) = a.runOnUiThread { Toast.makeText(a, msg, Toast.LENGTH_LONG).show() }

    private fun home(ctx: Context) = Threnody.home(ctx)

    private fun deviceSecure(ctx: Context) =
        ctx.getSystemService(KeyguardManager::class.java)?.isDeviceSecure == true

    private fun noun(kind: LockKind?) = if (kind == LockKind.PASSPHRASE) "passphrase" else "PIN"

    /** Runs [change] off the main thread, then rereads the settings and shows [after]. */
    private fun change(a: Activity, done: String?, change: (pepper: String) -> Unit, after: (LockSettings) -> Unit = {}) {
        Threading.background {
            try {
                val pepper = KeyVault.secret(a)
                change(pepper)
                val s = appLock(home(a), pepper)
                AppLock.changed(s)
                a.runOnUiThread {
                    done?.let { Toast.makeText(a, it, Toast.LENGTH_LONG).show() }
                    after(s)
                }
            } catch (e: Exception) {
                toast(a, e.message ?: "Couldn't change app lock")
            }
        }
    }

    /**
     * The phone's own authentication, before turning device lock off or
     * loosening it: the lock file can't check it. Runs [then] at once when
     * the phone has no screen lock.
     */
    fun confirmDevice(a: Activity, then: () -> Unit) {
        if (!deviceSecure(a)) return then()
        val b = BiometricPrompt.Builder(a).setTitle("Confirm it's you")
        if (Build.VERSION.SDK_INT >= 30) {
            b.setAllowedAuthenticators(
                BiometricManager.Authenticators.BIOMETRIC_STRONG or BiometricManager.Authenticators.DEVICE_CREDENTIAL,
            )
        } else {
            @Suppress("DEPRECATION")
            b.setDeviceCredentialAllowed(true)
        }
        try {
            b.build().authenticate(CancellationSignal(), a.mainExecutor, object : BiometricPrompt.AuthenticationCallback() {
                override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult?) = then()
            })
        } catch (e: Exception) {
            toast(a, "Couldn't confirm: ${e.message}")
        }
    }

    private fun field(a: Activity, hint: String, kind: LockKind?) = EditText(a).apply {
        this.hint = hint
        inputType = if (kind == LockKind.PASSPHRASE) {
            InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
        } else {
            InputType.TYPE_CLASS_NUMBER or InputType.TYPE_NUMBER_VARIATION_PASSWORD
        }
    }

    private fun box(a: Activity) = LinearLayout(a).apply {
        orientation = LinearLayout.VERTICAL
        setPadding(a.dp(24), a.dp(8), a.dp(24), 0)
    }

    /** Asks for the current secret of [kind]. */
    private fun askCurrent(a: Activity, kind: LockKind, title: String, done: (String) -> Unit) {
        val f = field(a, "Current ${noun(kind)}", kind)
        SecureBuilder(a)
            .setTitle(title)
            .setView(box(a).apply { addView(f) })
            .setPositiveButton("Continue") { _, _ -> done(f.text.toString()) }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Asks for a new secret of [kind], twice. */
    private fun askNew(a: Activity, kind: LockKind, title: String, message: String?, done: (String) -> Unit) {
        val first = field(a, if (kind == LockKind.PASSPHRASE) "At least 6 characters" else "4 to 16 digits", kind)
        val second = field(a, "Again", kind)
        SecureBuilder(a)
            .setTitle(title)
            .apply { if (message != null) setMessage(message) }
            .setView(box(a).apply { addView(first); addView(second) })
            .setPositiveButton("Save") { _, _ ->
                val s = first.text.toString()
                when {
                    !lockSecretOk(kind, s) -> toast(a, if (kind == LockKind.PASSPHRASE)
                        "A passphrase needs at least 6 characters" else "A PIN is 4 to 16 digits")
                    s != second.text.toString() -> toast(a, "They differ")
                    else -> done(s)
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** The settings screen: what locks the app, when, and the duress PIN. */
    fun show(a: Activity) {
        Threading.background {
            val s = try {
                if (AppLock.enabled(a)) appLock(home(a), KeyVault.secret(a)) else null
            } catch (e: Exception) {
                toast(a, e.message ?: "App lock settings can't be read")
                return@background
            }
            a.runOnUiThread { if (s == null || !s.enabled) showOff(a) else showOn(a, s) }
        }
    }

    private fun showOff(a: Activity) {
        val items = mutableListOf<Pair<String, () -> Unit>>()
        if (deviceSecure(a)) items += "Lock with the phone's screen lock" to { turnOnDevice(a) }
        items += "Lock with an app PIN…" to { turnOnSecret(a, LockKind.DIGITS) }
        items += "Lock with an app passphrase…" to { turnOnSecret(a, LockKind.PASSPHRASE) }
        SecureBuilder(a)
            .setTitle("App lock is off")
            .setItems(items.map { it.first }.toTypedArray()) { _, i -> items[i].second() }
            .setNegativeButton("Close", null)
            .show()
    }

    private fun turnOnDevice(a: Activity) = change(a, "Threnody now locks with your phone's screen lock", { p ->
        setAppLock(home(a), p, null, null, null, true, 0u, 300u, false)
    })

    private fun turnOnSecret(a: Activity, kind: LockKind) =
        askNew(a, kind, "App ${noun(kind)}", "Threnody asks for it whenever it locks.") { secret ->
            change(a, "App lock on", { p ->
                setAppLock(home(a), p, null, secret, kind, false, 0u, 300u, false)
            })
        }

    private fun away(s: LockSettings) = AWAY.firstOrNull { it.second == s.awaySecs }?.first ?: "${s.awaySecs} s"
    private fun idle(s: LockSettings) = IDLE.firstOrNull { it.second == s.idleSecs }?.first ?: "${s.idleSecs} s"

    private fun showOn(a: Activity, s: LockSettings) {
        val kind = s.kind
        val device = kind == null
        val items = mutableListOf<Pair<String, () -> Unit>>()
        items += "Lock after leaving: ${away(s)}" to { chooseAway(a, s) }
        items += "Lock when idle: ${idle(s)}" to { chooseIdle(a, s) }
        if (!device) {
            items += "Also unlock with biometrics: ${if (s.biometric) "on" else "off"}" to {
                change(a, null, { p ->
                    setLockPolicy(home(a), p, !s.biometric, s.awaySecs, s.idleSecs, s.eraseAfterFailures)
                }) { showOn(a, it) }
            }
            items += "Change ${noun(kind)}…" to { changeSecret(a, kind!!) }
            items += "Duress PIN: ${if (s.duress) "set" else "not set"}…" to { duress(a, s) }
            items += "Erase after 10 wrong ${noun(kind)}s: ${if (s.eraseAfterFailures) "on" else "off"}" to {
                eraseAfter(a, s)
            }
            if (deviceSecure(a)) items += "Use the phone's screen lock instead…" to { toDevice(a, kind!!) }
        } else {
            items += "Use an app PIN instead…" to { turnOnSecret(a, LockKind.DIGITS) }
            items += "Use an app passphrase instead…" to { turnOnSecret(a, LockKind.PASSPHRASE) }
        }
        items += "Turn app lock off…" to { turnOff(a, s) }
        val what = if (device) "your phone's screen lock" else "an app ${noun(kind)}"
        SecureBuilder(a)
            .setTitle("App lock")
            .setMessage("Threnody locks with $what. App lock keeps people at your screen out; " +
                "messages still arrive while it is locked. Notifications say only “New message”.")
            .setItems(items.map { it.first }.toTypedArray()) { _, i -> items[i].second() }
            .setNegativeButton("Close", null)
            .show()
    }

    /** In device-lock mode, loosening needs the phone's own authentication. */
    private fun loosen(a: Activity, s: LockSettings, looser: Boolean, then: () -> Unit) =
        if (s.kind == null && looser) confirmDevice(a, then) else then()

    private fun chooseAway(a: Activity, s: LockSettings) {
        SecureBuilder(a)
            .setTitle("Lock after leaving")
            .setItems(AWAY.map { it.first }.toTypedArray()) { _, i ->
                val secs = AWAY[i].second
                loosen(a, s, secs > s.awaySecs) {
                    change(a, null, { p ->
                        setLockPolicy(home(a), p, s.biometric, secs, s.idleSecs, s.eraseAfterFailures)
                    }) { showOn(a, it) }
                }
            }
            .show()
    }

    private fun chooseIdle(a: Activity, s: LockSettings) {
        SecureBuilder(a)
            .setTitle("Lock when idle in the app")
            .setItems(IDLE.map { it.first }.toTypedArray()) { _, i ->
                val secs = IDLE[i].second
                val now = s.idleSecs
                val looser = now != null && (secs == null || secs > now)
                loosen(a, s, looser) {
                    change(a, null, { p ->
                        setLockPolicy(home(a), p, s.biometric, s.awaySecs, secs, s.eraseAfterFailures)
                    }) { showOn(a, it) }
                }
            }
            .show()
    }

    private fun changeSecret(a: Activity, kind: LockKind) = askCurrent(a, kind, "Change ${noun(kind)}") { current ->
        askNew(a, kind, "New ${noun(kind)}", null) { secret ->
            val s = AppLock.settings
            change(a, "${noun(kind).replaceFirstChar { it.uppercase() }} changed", { p ->
                setAppLock(home(a), p, current, secret, kind, s?.biometric ?: false, s?.awaySecs ?: 0u,
                    s?.idleSecs, s?.eraseAfterFailures ?: false)
            })
        }
    }

    private fun toDevice(a: Activity, kind: LockKind) = askCurrent(a, kind, "Use the screen lock") { current ->
        val s = AppLock.settings
        change(a, "Threnody now locks with your phone's screen lock (the duress PIN is gone)", { p ->
            setAppLock(home(a), p, current, null, null, true, s?.awaySecs ?: 0u, s?.idleSecs, false)
        })
    }

    private fun turnOff(a: Activity, s: LockSettings) {
        val kind = s.kind
        if (kind == null) {
            confirmDevice(a) { change(a, "App lock off", { p -> clearAppLock(home(a), p, null) }) }
        } else {
            askCurrent(a, kind, "Turn app lock off") { current ->
                change(a, "App lock off", { p -> clearAppLock(home(a), p, current) })
            }
        }
    }

    private fun eraseAfter(a: Activity, s: LockSettings) {
        val on = !s.eraseAfterFailures
        val set = {
            change(a, null, { p ->
                setLockPolicy(home(a), p, s.biometric, s.awaySecs, s.idleSecs, on)
            }) { showOn(a, it) }
        }
        if (!on) return set()
        SecureBuilder(a)
            .setTitle("Erase after 10 wrong ${noun(s.kind)}s?")
            .setMessage("After 10 wrong entries in a row on the lock screen, everything on this device is " +
                "erased and Threnody opens as if newly installed. Wrong entries already slow down after 5.")
            .setPositiveButton("Turn on") { _, _ -> set() }
            .setNegativeButton("Cancel", null)
            .show()
    }

    /** Sets, changes or removes the duress PIN. */
    fun duress(a: Activity, s: LockSettings) {
        val kind = s.kind ?: return toast(a, "A duress PIN needs an app PIN or passphrase")
        askCurrent(a, kind, "Duress ${noun(kind)}") { current ->
            if (s.duress) {
                SecureBuilder(a)
                    .setTitle("Duress ${noun(kind)}")
                    .setMessage(DURESS_HELP)
                    .setPositiveButton("Change") { _, _ -> newDuress(a, kind, current) }
                    .setNeutralButton("Remove") { _, _ ->
                        change(a, "Duress ${noun(kind)} removed", { p -> setDuressPin(home(a), p, current, null) })
                    }
                    .setNegativeButton("Cancel", null)
                    .show()
            } else {
                newDuress(a, kind, current)
            }
        }
    }

    private fun newDuress(a: Activity, kind: LockKind, current: String) =
        askNew(a, kind, "Duress ${noun(kind)}", DURESS_HELP) { duress ->
            change(a, "Duress ${noun(kind)} set", { p -> setDuressPin(home(a), p, current, duress) })
        }

    /** "Erase all Threnody data…": typed confirmation, then erase and restart. */
    fun eraseAll(a: Activity) {
        val f = EditText(a).apply { hint = "Type erase" }
        val dialog = SecureBuilder(a)
            .setTitle("Erase all Threnody data?")
            .setMessage("This erases your identity, contacts, history, anonymous identities and every file " +
                "Threnody kept, including those it saved to Downloads/Threnody, then starts Threnody afresh. " +
                "Your linked devices and your contacts are not told. It can't be undone.")
            .setView(box(a).apply { addView(f) })
            .setPositiveButton("Erase") { _, _ ->
                Threading.background {
                    Threnody.wipe(a)
                    a.runOnUiThread { Threnody.restartApp(a) }
                }
            }
            .setNegativeButton("Cancel", null)
            .show()
        val erase = dialog.getButton(AlertDialog.BUTTON_POSITIVE)
        erase.isEnabled = false
        f.addTextChangedListener(object : TextWatcher {
            override fun beforeTextChanged(s: CharSequence?, start: Int, count: Int, after: Int) {}
            override fun onTextChanged(s: CharSequence?, start: Int, before: Int, count: Int) {}
            override fun afterTextChanged(s: Editable?) {
                erase.isEnabled = s.toString().trim() == "erase"
            }
        })
    }

    /**
     * Once per identity (and once for installs from before app lock), after
     * the node started: turns device lock on where the phone has a screen
     * lock and says so, with a way to turn it off; else offers an app PIN.
     * Returns whether it may show something now.
     */
    fun firstRun(a: Activity): Boolean {
        val prefs = a.getSharedPreferences(UI_PREFS, Context.MODE_PRIVATE)
        if (prefs.getBoolean(OFFERED, false)) return false
        prefs.edit().putBoolean(OFFERED, true).apply()
        if (AppLock.enabled(a)) return false
        if (deviceSecure(a)) {
            change(a, null, { p -> setAppLock(home(a), p, null, null, null, true, 0u, 300u, false) }) { s ->
                if (a.isFinishing) return@change
                SecureBuilder(a)
                    .setTitle("App lock")
                    .setMessage("Threnody now locks with your phone's screen lock. " +
                        "Change this in Privacy & Security → App lock.")
                    .setPositiveButton("OK", null)
                    .setNegativeButton("Turn off") { _, _ -> turnOff(a, s) }
                    .setCancelable(false)
                    .show()
            }
        } else {
            a.runOnUiThread {
                SecureBuilder(a)
                    .setTitle("Lock Threnody?")
                    .setMessage("Your phone has no screen lock. Set an app PIN so that people who pick up " +
                        "your phone can't read your messages.")
                    .setPositiveButton("Set an app PIN") { _, _ -> turnOnSecret(a, LockKind.DIGITS) }
                    .setNegativeButton("Not now", null)
                    .show()
            }
        }
        return true
    }
}
