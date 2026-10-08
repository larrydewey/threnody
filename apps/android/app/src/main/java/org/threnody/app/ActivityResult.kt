package org.threnody.app

import android.app.Activity
import android.content.Intent
import android.os.Bundle
import java.util.HashMap
import java.util.concurrent.atomic.AtomicInteger

/**
 * Platform-only Activity Result API compatibility layer.
 * Replaces deprecated onActivityResult with a callback-based approach
 * without requiring AndroidX Activity/Fragment dependencies.
 *
 * Usage in Activity:
 *   val launcher = ActivityResultLauncher(this)
 *   launcher.launch(intent) { resultCode, data -> handle(resultCode, data) }
 *
 * In the Activity, override onActivityResult once:
 *   override fun onActivityResult(requestCode, resultCode, data) {
 *       if (!ActivityResultRegistry.dispatch(requestCode, resultCode, data)) {
 *           super.onActivityResult(requestCode, resultCode, data)
 *       }
 *   }
 */
class ActivityResultLauncher(private val activity: Activity) {
    internal val callbacks = HashMap<Int, (Int, Intent?) -> Unit>()
    private val requestCodeGen = AtomicInteger(0x1000)

    /** Starts an activity for result with a callback. */
    fun launch(intent: Intent, callback: (resultCode: Int, data: Intent?) -> Unit) {
        val requestCode = requestCodeGen.incrementAndGet()
        callbacks[requestCode] = callback
        activity.startActivityForResult(intent, requestCode)
    }
}

/** Registry that routes results to the right launcher. */
object ActivityResultRegistry {
    private val launchers = java.util.concurrent.ConcurrentHashMap<Activity, ActivityResultLauncher>()

    fun get(activity: Activity): ActivityResultLauncher =
        launchers.computeIfAbsent(activity) { ActivityResultLauncher(it) }

    /** Call from Activity.onActivityResult. Returns true if handled. */
    fun dispatch(activity: Activity, requestCode: Int, resultCode: Int, data: Intent?): Boolean {
        val launcher = launchers[activity] ?: return false
        val callback = launcher.callbacks.remove(requestCode) ?: return false
        callback(resultCode, data)
        return true
    }
}