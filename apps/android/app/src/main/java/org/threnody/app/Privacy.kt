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
 *   when approved relays allow (spec §9 layer 2);
 * - photo metadata: location, camera and times are removed from images
 *   before they are sent.
 *
 * One connectivity setting, also on unless turned off: reaching contacts
 * across the internet (Appendix N). Its cost is that strangers in the
 * public DHT see this device's IP address, though not who it talks to.
 * Anonymous identities never use it.
 *
 * GIF search is off until the user agrees to it, as GIPHY sees what is
 * searched for and this device's IP address ([Giphy]).
 */
object Privacy {
    private const val PREFS = "privacy"
    private const val SCREEN = "screen_security"
    private const val COVER = "cover_traffic"
    private const val ONION = "onion_first"
    private const val STRIP = "strip_metadata"
    private const val REACH = "reach_internet"
    private const val VOLUNTEERS = "use_volunteers"
    private const val READ_RECEIPTS = "send_read_receipts"
    private const val TYPING = "send_typing"
    private const val GIPHY = "giphy_search"

    /** Cover interval on Wi-Fi and other unmetered networks. */
    const val COVER_UNMETERED_MS = 2_000u
    /** On metered (mobile) data: ~46 MB a day per connected contact. */
    const val COVER_METERED_MS = 10_000u

    @Volatile var metered = false

    private const val TIMER = "default_timer"
    /** Disappearing-timer choices, in seconds (null = off). */
    val TIMERS = listOf<Pair<String, UInt?>>(
        "Off" to null, "30 seconds" to 30u, "5 minutes" to 300u, "1 hour" to 3600u,
        "1 day" to 86_400u, "1 week" to 604_800u, "4 weeks" to 2_419_200u,
    )

    /** Timer for chats without their own: a week unless changed (0 = off). */
    fun defaultTimer(ctx: Context): UInt? =
        prefs(ctx).getLong(TIMER, 0L).takeIf { it > 0 }?.toUInt()

    fun setDefaultTimer(ctx: Context, secs: UInt?) {
        prefs(ctx).edit().putLong(TIMER, secs?.toLong() ?: 0L).apply()
        Threnody.applyPrivacy(ctx)
    }

    private fun prefs(ctx: Context) = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    fun coverTraffic(ctx: Context) = prefs(ctx).getBoolean(COVER, true)
    fun onionFirst(ctx: Context) = prefs(ctx).getBoolean(ONION, true)
    fun stripMetadata(ctx: Context) = prefs(ctx).getBoolean(STRIP, true)
    fun reachInternet(ctx: Context) = prefs(ctx).getBoolean(REACH, true)
    /** Route through volunteer relays from subscribed directories (Appendix P). */
    fun useVolunteers(ctx: Context) = prefs(ctx).getBoolean(VOLUNTEERS, true)

    fun sendReadReceipts(ctx: Context) = prefs(ctx).getBoolean(READ_RECEIPTS, true)

    fun sendTyping(ctx: Context) = prefs(ctx).getBoolean(TYPING, true)

    /** Whether GIF search may contact GIPHY: only once the user agrees. */
    fun giphy(ctx: Context) = prefs(ctx).getBoolean(GIPHY, false)

    fun setGiphy(ctx: Context, on: Boolean) = prefs(ctx).edit().putBoolean(GIPHY, on).apply()

    fun setSendTyping(ctx: Context, on: Boolean) = prefs(ctx).edit().putBoolean(TYPING, on).apply()

    fun setSendReadReceipts(ctx: Context, on: Boolean) {
        prefs(ctx).edit().putBoolean(READ_RECEIPTS, on).apply()
        Threnody.applyPrivacy(ctx)
    }

    fun setUseVolunteers(ctx: Context, on: Boolean) {
        prefs(ctx).edit().putBoolean(VOLUNTEERS, on).apply()
        Threnody.applyPrivacy(ctx)
    }

    fun setReachInternet(ctx: Context, on: Boolean) {
        prefs(ctx).edit().putBoolean(REACH, on).apply()
        Threnody.applyPrivacy(ctx)
    }

    fun setStripMetadata(ctx: Context, on: Boolean) {
        prefs(ctx).edit().putBoolean(STRIP, on).apply()
        Threnody.applyPrivacy(ctx)
    }

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
    /**
     * A dialog's window is separate from its activity's, so it needs its
     * own FLAG_SECURE: without it, screenshots and the recents screen
     * show dialogs and trays while the screen behind them stays blank.
     */
    fun secure(dialog: android.app.Dialog) {
        if (screenSecurity(dialog.context)) dialog.window?.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
    }

    fun apply(activity: Activity) {
        if (screenSecurity(activity)) {
            activity.window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        } else {
            activity.window.clearFlags(WindowManager.LayoutParams.FLAG_SECURE)
        }
    }
}
