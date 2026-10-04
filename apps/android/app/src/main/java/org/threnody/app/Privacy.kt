package org.threnody.app

import android.app.Activity
import android.content.Context
import android.view.WindowManager

/**
 * Screen security: no screenshots or screen recording of the app, and a
 * blank thumbnail in recent apps. On unless the user turns it off.
 */
object Privacy {
    private const val PREFS = "privacy"
    private const val SCREEN = "screen_security"

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
