package org.threnody.app

import android.app.Activity
import android.content.Context
import android.view.WindowManager

/**
 * Privacy settings. Every one is on unless the user turns it off:
 *
 * - screen security: no screenshots or screen recording of the app, and a
 *   blank thumbnail in recent apps;
 * - cover traffic: each session sends a padded frame at a constant rate,
 *   so traffic doesn't show when messages are sent (spec §9 layer 1);
 * - onion first: contacts are reached through two-relay onion circuits
 *   when approved relays allow (spec §9 layer 2).
 */
object Privacy {
    private const val PREFS = "privacy"
    private const val SCREEN = "screen_security"
    private const val COVER = "cover_traffic"
    private const val ONION = "onion_first"

    /** Cover interval on Wi-Fi and other unmetered networks. */
    const val COVER_UNMETERED_MS = 2_000u
    /** On metered (mobile) data: ~46 MB a day per connected contact. */
    const val COVER_METERED_MS = 10_000u

    @Volatile var metered = false

    private fun prefs(ctx: Context) = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    fun coverTraffic(ctx: Context) = prefs(ctx).getBoolean(COVER, true)
    fun onionFirst(ctx: Context) = prefs(ctx).getBoolean(ONION, true)

    fun setCoverTraffic(ctx: Context, on: Boolean) {
        prefs(ctx).edit().putBoolean(COVER, on).apply()
        Threnody.applyPrivacy(ctx)
    }

    fun setOnionFirst(ctx: Context, on: Boolean) {
        prefs(ctx).edit().putBoolean(ONION, on).apply()
        Threnody.applyPrivacy(ctx)
    }

    /** The cover interval to use now, or null when the user turned it off. */
    fun coverMs(ctx: Context): UInt? = when {
        !coverTraffic(ctx) -> null
        metered -> COVER_METERED_MS
        else -> COVER_UNMETERED_MS
    }

    fun screenSecurity(ctx: Context): Boolean =
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getBoolean(SCREEN, true)

    fun setScreenSecurity(activity: Activity, on: Boolean) {
        activity.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().putBoolean(SCREEN, on).apply()
        apply(activity)
    }

    /** Call from every Activity's onCreate. */
    fun apply(activity: Activity) {
        if (screenSecurity(activity)) {
            activity.window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        } else {
            activity.window.clearFlags(WindowManager.LayoutParams.FLAG_SECURE)
        }
    }
}
