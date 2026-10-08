package org.threnody.app

import android.app.Activity
import android.graphics.drawable.GradientDrawable
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.Gravity
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.ImageButton
import android.widget.LinearLayout
import android.widget.TextView

/**
 * The call screen: who, how the call is going, and its controls (answer
 * or decline while it rings here; mute, speaker and hang up after). Shown
 * over the lock screen for an incoming call.
 */
class CallActivity : Activity() {
    private lateinit var status: TextView
    private lateinit var who: TextView
    private lateinit var ringingRow: LinearLayout
    private lateinit var activeRow: LinearLayout
    private lateinit var mute: ImageButton
    private lateinit var speaker: ImageButton
    private var unlisten: (() -> Unit)? = null
    private val ticker = Handler(Looper.getMainLooper())
    private val tick = object : Runnable {
        override fun run() {
            show()
            ticker.postDelayed(this, 1000)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Privacy.apply(this)
        // An incoming call shows over the lock screen and wakes it.
        setShowWhenLocked(true)
        setTurnScreenOn(true)

        who = label("", Design.typeTitle * 1.4f, R.color.text, Design.weightBold).apply {
            gravity = Gravity.CENTER
        }
        status = label("", Design.typeSubtitle, R.color.muted).apply { gravity = Gravity.CENTER }

        fun round(res: Int, label: String, bg: Int, fg: Int, onClick: () -> Unit) = ImageButton(this).apply {
            setImageResource(res)
            imageTintList = android.content.res.ColorStateList.valueOf(fg)
            contentDescription = label
            tooltipText = label
            background = GradientDrawable().apply {
                shape = GradientDrawable.OVAL
                setColor(bg)
            }
            setOnClickListener { v ->
                Design.mediumHaptic(v)
                onClick()
            }
        }
        val red = getColor(android.R.color.holo_red_dark)
        val green = getColor(android.R.color.holo_green_dark)
        val white = getColor(android.R.color.white)
        val big = dp(72)
        fun row(vararg views: View) = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER
            for (v in views) addView(v, LinearLayout.LayoutParams(big, big).apply {
                marginStart = dp(Design.lg)
                marginEnd = dp(Design.lg)
            })
        }
        ringingRow = row(
            round(R.drawable.ic_call_end, "Decline", red, white) { Calls.hangUp(this) },
            round(R.drawable.ic_call, "Answer", green, white) { Calls.answer(this) },
        )
        mute = round(R.drawable.ic_mic_off, "Mute", getColor(R.color.surface_container), getColor(R.color.text)) {
            Calls.current?.let { Calls.setMuted(!it.muted) }
        }
        speaker = round(R.drawable.ic_speaker, "Speaker", getColor(R.color.surface_container), getColor(R.color.text)) {
            Calls.current?.let { Calls.setSpeaker(this, !it.speaker) }
        }
        activeRow = row(
            mute,
            round(R.drawable.ic_call_end, "Hang up", red, white) { Calls.hangUp(this) },
            speaker,
        )

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER_HORIZONTAL
            setPadding(dp(Design.xl), dp(Design.xxl * 2), dp(Design.xl), dp(Design.xxl * 2))
            addView(who, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(status, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT).apply { topMargin = dp(Design.sm) })
            addView(View(this@CallActivity), LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
            addView(ringingRow, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(activeRow, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        }
        val frame = LinearLayout(this).apply {
            setBackgroundColor(color(R.color.background))
            addView(root, LinearLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
        }
        setContentView(frame)
        fitSystemBars(frame, root, root)
        // Answer, from the ringing notification.
        if (intent.action == ANSWER) Calls.answer(this)
    }

    override fun onNewIntent(intent: android.content.Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        if (intent.action == ANSWER) Calls.answer(this)
    }

    override fun onStart() {
        super.onStart()
        unlisten = Calls.listen { runOnUiThread { show() } }
        ticker.post(tick)
        show()
    }

    override fun onStop() {
        unlisten?.invoke()
        ticker.removeCallbacks(tick)
        super.onStop()
    }

    override fun onRequestPermissionsResult(code: Int, perms: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(code, perms, results)
        Calls.permissionResult(code, results)
    }

    private fun show() {
        val c = Calls.current
        if (c == null) {
            // Over: say how, briefly, then go.
            status.text = Calls.ended ?: "Call ended"
            ringingRow.visibility = View.GONE
            activeRow.visibility = View.GONE
            ticker.removeCallbacks(tick)
            ticker.postDelayed({ finish() }, 1200)
            return
        }
        who.text = c.title
        status.text = when (c.state) {
            "calling" -> "Calling…"
            "ringing" -> "Ringing…"
            "incoming" -> "Incoming voice call"
            "connecting" -> "Connecting…"
            "interrupted" -> "Reconnecting…"
            else -> duration(SystemClock.elapsedRealtime() - c.since)
        }
        val ringing = c.state == "incoming"
        ringingRow.visibility = if (ringing) View.VISIBLE else View.GONE
        activeRow.visibility = if (ringing) View.GONE else View.VISIBLE
        mute.alpha = if (c.muted) 1f else 0.6f
        mute.contentDescription = if (c.muted) "Unmute" else "Mute"
        speaker.alpha = if (c.speaker) 1f else 0.6f
    }

    private fun duration(ms: Long): String {
        val s = ms / 1000
        return if (s >= 3600) "%d:%02d:%02d".format(s / 3600, s / 60 % 60, s % 60) else "%d:%02d".format(s / 60, s % 60)
    }

    companion object {
        const val ANSWER = "org.threnody.app.ANSWER"
    }

    // The call carries on without the screen; Back just leaves it.
    @Suppress("OVERRIDE_DEPRECATION", "DEPRECATION")
    override fun onBackPressed() {
        moveTaskToBack(false)
    }
}
