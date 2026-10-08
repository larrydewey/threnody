package org.threnody.app

import android.app.Application
import android.content.Context
import android.os.Build
import androidx.appcompat.app.AppCompatDelegate
import com.google.android.material.color.DynamicColors

/**
 * Application class for Threnody.
 * Handles dynamic color initialization and global setup.
 */
class ThrenodyApplication : Application() {

    override fun onCreate() {
        super.onCreate()

        // Initialize dynamic colors (Android 12+)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            DynamicColors.applyToActivitiesIfAvailable(this)
        }

        // Warm up threading
        Threading.warmUp(this)

        // Set default theme based on system setting
        AppCompatDelegate.setDefaultNightMode(AppCompatDelegate.MODE_NIGHT_FOLLOW_SYSTEM)
    }

    override fun attachBaseContext(base: Context) {
        super.attachBaseContext(base)
        // Ensure dynamic colors are applied to the base context as well
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            DynamicColors.applyToActivitiesIfAvailable(this)
        }
    }

    companion object {
        @JvmStatic
        fun applyDynamicColors(activity: android.app.Activity) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                DynamicColors.applyToActivityIfAvailable(activity)
            }
        }
    }
}