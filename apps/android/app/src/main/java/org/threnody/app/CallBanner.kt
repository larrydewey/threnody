package org.threnody.app

import android.app.Activity
import android.content.Intent
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.Gravity
import android.view.View
import android.widget.TextView

/**
 * A bar under a screen's top bar while a call runs and its screen isn't
 * showing: who, how long, and a tap to go back to it. Without it, the way
 * back to an audio call was the notification shade.
 *
 * Call [start] from `onStart` and [stop] from `onStop`.
 */
class CallBanner(private val activity: Activity) {
    val view: TextView = TextView(activity).apply {
        gravity = Gravity.CENTER
        textSize = Design.typeBodySmall
        setTextColor(activity.getColor(android.R.color.white))
        setBackgroundColor(activity.getColor(android.R.color.holo_green_dark))
        setPadding(activity.dp(Design.lg), activity.dp(Design.sm), activity.dp(Design.lg), activity.dp(Design.sm))
        visibility = View.GONE
        isClickable = true
        isFocusable = true
        setOnClickListener { v ->
            Design.lightHaptic(v)
            activity.startActivity(Intent(activity, CallActivity::class.java))
        }
    }
    private var unlisten: (() -> Unit)? = null
    private val handler = Handler(Looper.getMainLooper())
    private val tick = object : Runnable {
        override fun run() {
            refresh()
            handler.postDelayed(this, 1000)
        }
    }

    fun start() {
        unlisten = Calls.listen { activity.runOnUiThread { refresh() } }
        handler.post(tick)
    }

    fun stop() {
        unlisten?.invoke()
        unlisten = null
        handler.removeCallbacks(tick)
    }

    private fun refresh() {
        val c = Calls.current
        // Ringing has its own full screen and notification.
        if (c == null || c.state == "incoming") {
            view.visibility = View.GONE
            return
        }
        // Past app lock, say there's a call, not who it's with.
        val who = if (AppLock.locked()) "your call" else c.title
        val how = when (c.state) {
            "connected" -> {
                val s = (SystemClock.elapsedRealtime() - c.since) / 1000
                "%d:%02d".format(s / 60, s % 60)
            }
            "interrupted" -> "reconnecting"
            "ringing" -> "ringing"
            else -> "calling"
        }
        view.text = "Return to $who · $how" + if (c.muted) " · muted" else ""
        view.contentDescription = "Return to the call with $who"
        view.visibility = View.VISIBLE
    }
}
