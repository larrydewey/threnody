package org.threnody.app

import android.app.Activity
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.PowerManager
import android.provider.Settings

/**
 * Staying reachable while the app is closed: the background service
 * ([ThrenodyService]) keeps the node's sessions up, but only while it
 * runs and while Android lets it use the network.
 *
 * - After a reboot or an update nothing had started it again until the
 *   app was opened, so contacts saw this phone as not connected. [Boot]
 *   starts it then.
 * - While the phone sleeps (Doze), Android cuts background apps off the
 *   network unless they are exempt from battery optimization, and a
 *   foreground service alone doesn't exempt them. [ask] offers the
 *   exemption once; the menu item opens it again.
 *
 * Neither applies with UnifiedPush wake-ups on ([Push]): then the app is
 * meant to sleep.
 */
object Reachable {
    private const val UI_PREFS = "ui"
    private const val ASKED = "battery_asked"

    /** Whether Android lets the app use the network while the phone sleeps. */
    fun unrestricted(ctx: Context): Boolean =
        ctx.getSystemService(PowerManager::class.java)?.isIgnoringBatteryOptimizations(ctx.packageName) == true

    /** Whether there is an identity to keep reachable (none before setup or after an erase). */
    fun hasIdentity(ctx: Context): Boolean = java.io.File(Threnody.home(ctx), "identity.cbor").exists()

    /** Once, after the node started: offers the exemption, if it's needed. */
    fun ask(a: Activity) {
        val prefs = a.getSharedPreferences(UI_PREFS, Context.MODE_PRIVATE)
        if (prefs.getBoolean(ASKED, false) || Push.active(a) || unrestricted(a)) return
        prefs.edit().putBoolean(ASKED, true).apply()
        if (a.isFinishing) return
        SecureBuilder(a)
            .setTitle("Stay reachable when closed?")
            .setMessage("While the phone sleeps, Android cuts Threnody off the network, so your " +
                "contacts can't reach you until you open it. Let it run in the background " +
                "without restrictions? It uses a little more battery.")
            .setPositiveButton("Allow") { _, _ -> request(a) }
            .setNegativeButton("Not now", null)
            .show()
    }

    /**
     * Asks Android for the exemption, or opens the battery optimization
     * list when the app already has it (that's where it's taken back).
     */
    fun request(a: Activity) {
        val intent = if (unrestricted(a)) {
            Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS)
        } else {
            @Suppress("BatteryLife")
            Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:${a.packageName}"))
        }
        try {
            a.startActivity(intent)
        } catch (_: Exception) {
            // No such screen on this phone: the app's own settings page has battery.
            runCatching {
                a.startActivity(Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.parse("package:${a.packageName}")))
            }
        }
    }

    /** Starts the background service after a reboot or an update of the app. */
    class Boot : BroadcastReceiver() {
        override fun onReceive(ctx: Context, intent: Intent) {
            when (intent.action) {
                Intent.ACTION_BOOT_COMPLETED, Intent.ACTION_MY_PACKAGE_REPLACED -> {}
                else -> return
            }
            if (Push.active(ctx) || !hasIdentity(ctx)) return
            try {
                ThrenodyService.start(ctx)
            } catch (e: Exception) {
                // Not allowed to start it now: it starts when the app is opened.
                Threnody.say("! background service at start-up: ${e.javaClass.simpleName}")
            }
        }
    }
}
